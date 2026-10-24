//! Signed in-app updates from GitHub Releases.
//!
//! Enabled in release builds only (`AURA_RELEASE_BUILD=1` at compile time).
//! In any other build, `update_check` returns `{ enabled: false }` and
//! `update_install` is a no-op — unless `AURA_UPDATE_TEST_FEED` is set, in
//! which case the updater runs against that URL with a test public key from
//! `AURA_UPDATE_TEST_PUBKEY`. Both test vars are ignored when
//! `AURA_RELEASE_BUILD=1` is set.
//!
//! ## Signature scheme
//!
//! Each release ships a `.sig` asset alongside the app zip. The signature is
//! Ed25519 over exactly:
//!
//! ```text
//! aura-engine-update-v1\n
//! <tag, e.g. v1.5.0>\n
//! <asset file name>\n
//! <lowercase hex SHA-256 of the zip>\n
//! ```
//!
//! The `.sig` file is base64 of the 64-byte signature (one line). The app
//! rebuilds this message from the tag it is installing, the asset name it
//! downloaded and the SHA-256 it computed itself, then verifies before
//! extracting anything.
//!
//! An older tag than the running version is refused even if it verifies
//! (anti-rollback).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lazy_static::lazy_static;
use serde::Serialize;

// ── constants ─────────────────────────────────────────────────────────────

/// The production public key (Ed25519, 32 bytes as lowercase hex).
/// Never used when `AURA_RELEASE_BUILD` is not set.
const PROD_PUBKEY_HEX: &str =
    "cd2a803ad18aadae9e100644d385549a3a46fbfc19df00a8fd0621b424b391a6";

const GITHUB_API_URL: &str =
    "https://api.github.com/repos/ToxaDev/aura-engine/releases/latest";

const GITHUB_RELEASES_LIST_URL: &str =
    "https://api.github.com/repos/ToxaDev/aura-engine/releases?per_page=30";

/// The running version's own release (its interface packs), `<tag>` appended.
const GITHUB_RELEASE_BY_TAG_URL: &str =
    "https://api.github.com/repos/ToxaDev/aura-engine/releases/tags/";

const APP_ZIP_SUFFIX: &str = "-windows-x64.zip";
const APP_SIG_SUFFIX: &str = "-windows-x64.zip.sig";

/// Maximum allowed size for a single zip entry (50 MB).
const MAX_ENTRY_BYTES: u64 = 50 * 1024 * 1024;

/// Files the updater will extract from the zip. Everything else is skipped,
/// wherever it lies in the archive.
const ALLOWED_FILES: &[&str] = &["aura-engine.exe", "README.txt", "LICENSE.txt", "THIRD-PARTY-NOTICES.txt"];

/// The ASIO driver's folder in the zip (`<folder>/asio/<file>`) and the files
/// taken from it. None of them shares a name with `ALLOWED_FILES`: an
/// updater before 1.6.0 takes those by name at any depth and stops the
/// update on one that is nested, so the driver's own texts have names of
/// their own.
const ASIO_ZIP_DIR: &str = "asio";
pub const ASIO_FILES: &[&str] = &[
    crate::player::input::driver::DLL64,
    crate::player::input::driver::DLL32,
    "LICENSE-GPL-3.0.txt",
    "README-ASIO.txt",
];

// ── feature flags ─────────────────────────────────────────────────────────

/// True when this binary was compiled with `AURA_RELEASE_BUILD=1`.
///
/// `option_env!` expands to a compile-time `Option<&str>`, but comparing
/// `str` inside a `const` isn't stable on Rust 1.76, so this is a
/// `fn` (called only at start-up and in tests).
pub(crate) fn is_release_build() -> bool {
    option_env!("AURA_RELEASE_BUILD") == Some("1")
}

fn feed_url() -> Option<String> {
    if is_release_build() {
        return Some(GITHUB_API_URL.to_string());
    }
    // Non-release: test hook only
    std::env::var("AURA_UPDATE_TEST_FEED").ok()
}

/// The URL for listing all releases (used to collect notes for skipped versions).
/// In a release build this is the production list endpoint. In a test build the
/// URL is derived from `AURA_UPDATE_TEST_FEED` by replacing the `/latest` suffix
/// with `?per_page=30`, so a local mock server only needs one extra route.
fn releases_list_url(feed: &str) -> String {
    if is_release_build() {
        return GITHUB_RELEASES_LIST_URL.to_string();
    }
    if let Some(base) = feed.strip_suffix("/latest") {
        format!("{}?per_page=30", base)
    } else {
        // Already a list URL or non-standard test feed; use as-is.
        feed.to_string()
    }
}

/// The URL of the release with `tag`. In a test build it is derived from
/// `AURA_UPDATE_TEST_FEED` by replacing the `/latest` suffix with
/// `/tags/<tag>`, the route GitHub has for it.
fn release_by_tag_url(feed: &str, tag: &str) -> String {
    if is_release_build() {
        return format!("{GITHUB_RELEASE_BY_TAG_URL}{tag}");
    }
    let base = feed.strip_suffix("/latest").unwrap_or(feed);
    format!("{base}/tags/{tag}")
}

pub(crate) fn active_pubkey() -> Option<[u8; 32]> {
    if is_release_build() {
        return Some(decode_pubkey_hex(PROD_PUBKEY_HEX).expect("embedded pubkey invalid"));
    }
    // Non-release: AURA_UPDATE_TEST_PUBKEY required
    let hex = std::env::var("AURA_UPDATE_TEST_PUBKEY").ok()?;
    decode_pubkey_hex(&hex)
}

// ── global state ──────────────────────────────────────────────────────────

/// One entry in the per-release notes list sent to the frontend.
/// The JS parses each `notes` body into Added / Fixed sections.
#[derive(Clone, Serialize)]
pub struct ReleaseNoteEntry {
    pub version: String,
    pub notes: String,
}

/// The update check result, computed once per session and reused.
#[derive(Clone)]
struct CheckCache {
    available: bool,
    current: String,
    version: String,
    tag: String,
    notes: String,
    /// Every published release strictly newer than the running version and
    /// at most as new as `tag`, newest first.  Always contains at least the
    /// latest release's notes when `available` is true.
    release_notes: Vec<ReleaseNoteEntry>,
    release_url: String,
    asset_name: String,
    asset_url: String,
    sig_url: String,
    error: Option<String>,
    /// An interface update for the running version, looked for only when no
    /// newer app release is offered.
    ui: Option<UiOffer>,
}

/// An interface pack on the running version's release, newer than the one
/// this run serves.
#[derive(Clone, Debug, PartialEq)]
struct UiOffer {
    patch: u32,
    asset_name: String,
    asset_url: String,
    sig_url: String,
    notes_url: Option<String>,
    notes: String,
}

/// What the frontend and protocol route read.
#[derive(Clone, Serialize)]
pub struct UpdateProgress {
    pub phase: String,  // idle checking downloading verifying installing restarting error
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

impl UpdateProgress {
    fn idle() -> Self {
        UpdateProgress {
            phase: "idle".to_string(),
            done: 0,
            total: 0,
            error: None,
        }
    }
}

lazy_static! {
    static ref CHECK_CACHE: Mutex<Option<CheckCache>> = Mutex::new(None);
    static ref PROGRESS: Mutex<UpdateProgress> = Mutex::new(UpdateProgress::idle());
}

fn set_progress(phase: &str, done: u64, total: u64, error: Option<String>) {
    if let Ok(mut p) = PROGRESS.lock() {
        p.phase = phase.to_string();
        p.done = done;
        p.total = total;
        p.error = error;
    }
}

/// The install slot, held from the moment `update_install` claims it until
/// the work is handed to the async worker.
///
/// The check "is an install already running?" and the move to "checking"
/// happen under ONE lock, so two calls that arrive together cannot both pass
/// (upd-2). Dropped without `hand_off()` — any early `return Err` after the
/// claim — it puts back the progress it found, so a refused install never
/// leaves the dialog stuck on "checking".
struct InstallClaim<'a> {
    slot: &'a Mutex<UpdateProgress>,
    prev: Option<UpdateProgress>,
}

impl<'a> InstallClaim<'a> {
    /// `conversion_running` is asked under the same lock as the phase:
    /// `start_unless_installing` starts a conversion under that lock too, so
    /// an install and a conversion can never both get going.
    fn try_claim(
        slot: &'a Mutex<UpdateProgress>,
        conversion_running: impl FnOnce() -> bool,
    ) -> Result<Self, String> {
        let mut p = slot.lock().map_err(|_| "update state is unavailable".to_string())?;
        if install_in_flight(&p.phase) {
            return Err("an update is already in progress".to_string());
        }
        // Under this lock and then the download's own, in that order, as the
        // download claims its slot (`filters_dl`): never both at once.
        if crate::filters_dl::in_flight() {
            return Err("filters are downloading — please wait for them to finish before updating".to_string());
        }
        if conversion_running() {
            return Err(
                "a conversion is in progress — please wait for it to finish before updating"
                    .to_string(),
            );
        }
        let prev = std::mem::replace(
            &mut *p,
            UpdateProgress {
                phase: "checking".to_string(),
                done: 0,
                total: 0,
                error: None,
            },
        );
        Ok(InstallClaim { slot, prev: Some(prev) })
    }

    /// The worker owns the phase from here on; nothing is restored on drop.
    fn hand_off(mut self) {
        self.prev = None;
    }
}

impl Drop for InstallClaim<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            if let Ok(mut p) = self.slot.lock() {
                *p = prev;
            }
        }
    }
}

/// Any phase but these two means an install is under way.
fn install_in_flight(phase: &str) -> bool {
    phase != "idle" && phase != "error"
}

/// What the converter says when a conversion is asked for during an install.
/// Short on purpose: the status line shows 58 characters, "Error: " included.
pub const INSTALLING_REFUSAL: &str = "Installing an update — convert after the restart.";

/// Run `start` (the start of a conversion) only while no install is under
/// way, and under the progress lock, so an install cannot claim the slot
/// between this check and the conversion marking itself as running.
pub fn start_unless_installing<T>(start: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    start_unless_installing_in(&PROGRESS, INSTALLING_REFUSAL, start)
}

/// [`start_unless_installing`] for other work that must not run into an
/// install (a filter download), refused with `refusal`.
pub fn unless_installing<T>(refusal: &str, start: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    start_unless_installing_in(&PROGRESS, refusal, start)
}

fn start_unless_installing_in<T>(
    slot: &Mutex<UpdateProgress>,
    refusal: &str,
    start: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    // A poisoned lock must not stop conversions for good.
    let p = slot.lock().unwrap_or_else(|e| e.into_inner());
    if install_in_flight(&p.phase) {
        return Err(refusal.to_string());
    }
    start()
}

/// Serialised progress for the `/update/status` protocol route.
pub fn status_json() -> Vec<u8> {
    PROGRESS
        .lock()
        .map(|p| serde_json::to_vec(&*p).unwrap_or_default())
        .unwrap_or_default()
}

// ── startup: --after-update handling ─────────────────────────────────────

/// Called at the very start of `main`, **before** `single_instance::acquire`.
///
/// If `--after-update <pid>` is present: waits up to 15 s for the old
/// process to exit, then tries to delete `aura-engine.exe.old` (best
/// effort), then returns. The rest of start-up continues normally.
pub fn handle_after_update() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pos) = args.iter().position(|a| a == "--after-update") else {
        return;
    };
    let pid: u32 = match args.get(pos + 1).and_then(|s| s.parse().ok()) {
        Some(p) => p,
        None => {
            eprintln!("[UPDATER] --after-update: missing or invalid pid");
            return;
        }
    };

    crate::aelog!("[UPDATER] after-update: waiting for old process {} to exit", pid);
    wait_for_pid(pid, 15_000);

    // Best-effort delete of the old exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let old = dir.join("aura-engine.exe.old");
            // An interface update restarts the same exe: there is no .old.
            let tries = if old.exists() { 6u32 } else { 0 };
            for attempt in 0..tries {
                match std::fs::remove_file(&old) {
                    Ok(()) => {
                        crate::aelog!("[UPDATER] deleted aura-engine.exe.old");
                        break;
                    }
                    Err(_) if attempt < 5 => {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    Err(e) => {
                        crate::aelog!(
                            "[UPDATER] could not delete aura-engine.exe.old: {}",
                            e
                        );
                    }
                }
            }
        }
    }

    crate::aelog!("[UPDATER] updated to {}", env!("CARGO_PKG_VERSION"));
}

#[cfg(windows)]
fn wait_for_pid(pid: u32, timeout_ms: u32) {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winnt::SYNCHRONIZE;

    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return;
        }
        WaitForSingleObject(h, timeout_ms);
        CloseHandle(h);
    }
}

#[cfg(not(windows))]
fn wait_for_pid(_pid: u32, _timeout_ms: u32) {}

// ── update_check command ──────────────────────────────────────────────────

#[tauri::command]
pub async fn update_check() -> serde_json::Value {
    // Return cached result if we already ran this session.
    {
        let lock = CHECK_CACHE.lock().unwrap();
        if let Some(ref c) = *lock {
            return cache_to_value(c);
        }
    }

    let Some(feed) = feed_url() else {
        return serde_json::json!({ "enabled": false, "available": false,
            "current": env!("CARGO_PKG_VERSION"), "version": "", "tag": "",
            "notes": "", "url": "", "error": null });
    };
    let Some(pubkey) = active_pubkey() else {
        return serde_json::json!({ "enabled": false, "available": false,
            "current": env!("CARGO_PKG_VERSION"), "version": "", "tag": "",
            "notes": "", "url": "", "error": "no public key configured" });
    };
    let _ = pubkey; // key is valid; used in install, not here

    let current = env!("CARGO_PKG_VERSION").to_string();
    let ua = format!("AuraEngine/{}", current);

    let client = match build_api_client(&ua) {
        Ok(c) => c,
        Err(e) => {
            return error_result(&current, &format!("http client error: {}", e));
        }
    };

    match fetch_release(&client, &feed).await {
        Ok(mut cache) => {
            // Augment with notes from every skipped release (best effort).
            // If the extra call fails we fall back to a single-entry list so
            // the frontend always has something to render.
            if cache.available {
                let rn = fetch_release_notes_list(
                    &client,
                    &feed,
                    &current,
                    &cache.tag,
                )
                .await;
                cache.release_notes = rn.unwrap_or_else(|| {
                    vec![ReleaseNoteEntry {
                        version: cache.version.clone(),
                        notes: cache.notes.clone(),
                    }]
                });
            } else {
                // No newer app: an interface update for this one, perhaps.
                // An app update always wins, so this is looked for only here.
                match fetch_ui_offer(&client, &feed, &current, crate::ui_pack::active_patch()).await {
                    Ok(ui) => cache.ui = ui,
                    Err(e) => crate::aelog!("[UPDATER] interface update check failed (quiet): {}", e),
                }
            }
            let v = cache_to_value(&cache);
            *CHECK_CACHE.lock().unwrap() = Some(cache);
            v
        }
        Err(e) => {
            crate::aelog!("[UPDATER] check failed (quiet): {}", e);
            error_result(&current, &e)
        }
    }
}

fn cache_to_value(c: &CheckCache) -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "available": c.available,
        "current": c.current,
        "version": c.version,
        "tag": c.tag,
        "notes": c.notes,
        "release_notes": &c.release_notes,
        "url": c.release_url,
        "error": c.error,
        "ui_current": crate::ui_pack::active_patch(),
        "ui_pack": c.ui.as_ref().map(|u| serde_json::json!({
            "patch": u.patch,
            "notes": u.notes,
            "label": crate::ui_pack::label(&c.current, u.patch),
        })),
    })
}

fn error_result(current: &str, msg: &str) -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "available": false,
        "current": current,
        "version": "",
        "tag": "",
        "notes": "",
        "url": "",
        "error": msg,
    })
}

/// Parse the GitHub releases/latest JSON and decide if an update is available.
async fn fetch_release(
    client: &reqwest::Client,
    feed: &str,
) -> Result<CheckCache, String> {
    let resp = client
        .get(feed)
        .send()
        .await
        .map_err(|e| format!("network error: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("feed returned HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {}", e))?;

    // The latest release is not the app's: the instruments pack's release is
    // a pre-release, never "latest" — but were it ever published as a normal
    // one by mistake, no update would show until the next app release. The
    // app's own newest release from the list, then.
    let body = if is_app_release(&body) {
        body
    } else {
        match fetch_newest_app_release(client, feed).await {
            Some(r) => {
                crate::aelog!(
                    "[UPDATER] the latest release ({}) is not the app's — using {} from the list",
                    body["tag_name"].as_str().unwrap_or("?"),
                    r["tag_name"].as_str().unwrap_or("?")
                );
                r
            }
            None => body,
        }
    };

    let tag = body["tag_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let version = tag.trim_start_matches('v').to_string();
    let release_url = body["html_url"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let notes = body["body"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let current = env!("CARGO_PKG_VERSION");

    if tag.is_empty() {
        return Err("feed has no tag_name".to_string());
    }

    // Locate the app zip and its .sig in the assets list, by their names.
    let assets = body["assets"].as_array().cloned().unwrap_or_default();
    let app_zip = app_asset(&assets, &tag, APP_ZIP_SUFFIX);
    let app_sig = app_asset(&assets, &tag, APP_SIG_SUFFIX);

    // Both must be present; a release without a .sig is not offered.
    let (Some(zip_asset), Some(sig_asset)) = (app_zip, app_sig) else {
        return Ok(CheckCache {
            available: false,
            current: current.to_string(),
            version,
            tag,
            notes,
            release_notes: vec![],
            release_url,
            asset_name: String::new(),
            asset_url: String::new(),
            sig_url: String::new(),
            error: None,
            ui: None,
        });
    };

    let asset_name = zip_asset["name"].as_str().unwrap_or("").to_string();
    let asset_url = zip_asset["browser_download_url"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let sig_url = sig_asset["browser_download_url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Production downloads must be HTTPS; the test hook may use HTTP.
    if is_release_build()
        && (!asset_url.starts_with("https://") || !sig_url.starts_with("https://"))
    {
        return Err("asset URL is not HTTPS".to_string());
    }

    let available = semver_gt(&tag, current);

    Ok(CheckCache {
        available,
        current: current.to_string(),
        version,
        tag,
        notes,
        release_notes: vec![], // populated by update_check after this returns
        release_url,
        asset_name,
        asset_url,
        sig_url,
        error: None,
        ui: None,
    })
}

/// The release's own app asset: exactly `aura-engine-v<version><suffix>`
/// for its tag (the name CI gives the app zip and the signing script signs).
/// A bundle (`aura-engine-v<version>-bundle-<tier>-windows-x64.zip`) is
/// never it, wherever the release lists it.
fn app_asset<'a>(assets: &'a [serde_json::Value], tag: &str, suffix: &str) -> Option<&'a serde_json::Value> {
    let name = format!("aura-engine-v{}{}", tag.trim_start_matches('v'), suffix);
    assets.iter().find(|a| a["name"].as_str() == Some(name.as_str()))
}

/// A release of the app itself: a `v<major>.<minor>.<patch>` tag with the app
/// zip and its signature among the assets. The instruments pack's release
/// (tag `instruments-pack-<n>`, one zip) never is.
fn is_app_release(r: &serde_json::Value) -> bool {
    let tag = r["tag_name"].as_str().unwrap_or("");
    let version = tag.strip_prefix('v').unwrap_or("");
    let semver = version.split('.').count() == 3
        && version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let assets = r["assets"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    semver && app_asset(assets, tag, APP_ZIP_SUFFIX).is_some() && app_asset(assets, tag, APP_SIG_SUFFIX).is_some()
}

/// The newest published (not draft, not pre-release) app release in a list.
fn newest_app_release(releases: &[serde_json::Value]) -> Option<&serde_json::Value> {
    releases
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false) && !r["prerelease"].as_bool().unwrap_or(false))
        .filter(|r| is_app_release(r))
        .fold(None, |best: Option<&serde_json::Value>, r| match best {
            Some(b) if !semver_gt(r["tag_name"].as_str().unwrap_or(""), b["tag_name"].as_str().unwrap_or("")) => Some(b),
            _ => Some(r),
        })
}

/// The app's newest release from the releases list (when "latest" is not
/// the app's); `None` on any network or parse error.
async fn fetch_newest_app_release(client: &reqwest::Client, feed: &str) -> Option<serde_json::Value> {
    let resp = client.get(releases_list_url(feed)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let releases: Vec<serde_json::Value> = resp.json().await.ok()?;
    newest_app_release(&releases).cloned()
}

/// Filter a slice of GitHub release JSON objects to those that are published
/// (not draft, not pre-release), strictly newer than `current`, and at most
/// as new as `latest_tag`.  Returns entries sorted newest first.
///
/// Extracted as a pure function so unit tests can drive it with fixture JSON.
fn filter_release_notes(
    releases: &[serde_json::Value],
    current: &str,
    latest_tag: &str,
) -> Vec<ReleaseNoteEntry> {
    let mut entries: Vec<ReleaseNoteEntry> = releases
        .iter()
        .filter(|r| {
            !r["draft"].as_bool().unwrap_or(false)
                && !r["prerelease"].as_bool().unwrap_or(false)
        })
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let version = tag.trim_start_matches('v').to_string();
            let notes = r["body"].as_str().unwrap_or("").to_string();
            // Strictly newer than current AND at most as new as the offered tag.
            if semver_gt(tag, current) && !semver_gt(tag, latest_tag) {
                Some(ReleaseNoteEntry { version, notes })
            } else {
                None
            }
        })
        .collect();

    // Sort newest first (GitHub returns newest first already, but filter_map
    // does not guarantee order if the API ever changes).
    entries.sort_by(|a, b| {
        let av = format!("v{}", a.version);
        let bv = format!("v{}", b.version);
        if semver_gt(&av, &bv) {
            std::cmp::Ordering::Less
        } else if semver_gt(&bv, &av) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });

    entries
}

/// Fetch the full release list and extract per-release notes for every
/// version newer than `current` and at most as new as `latest_tag`.
/// Returns `None` on any network or parse error; the caller falls back to a
/// single-entry list built from the already-fetched latest release.
async fn fetch_release_notes_list(
    client: &reqwest::Client,
    feed: &str,
    current: &str,
    latest_tag: &str,
) -> Option<Vec<ReleaseNoteEntry>> {
    let list_url = releases_list_url(feed);
    let resp = client.get(&list_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let releases: Vec<serde_json::Value> = resp.json().await.ok()?;
    let entries = filter_release_notes(&releases, current, latest_tag);
    if entries.is_empty() { None } else { Some(entries) }
}

// ── interface update (a pack for the running version) ──────────────────────

/// The size the notes of an interface update may have; longer ones are cut.
const UI_NOTES_MAX_BYTES: usize = 8 * 1024;

/// The highest interface pack for `version` on its release, above
/// `installed`: `aura-engine-ui-v<version>-p<N>.zip` with its `.sig`, and
/// its notes (`…-p<N>.md`) when the release has them. Only exact names
/// count: an app zip, a bundle, the ASIO driver or a pack of another
/// version never is one.
fn pick_ui_offer(release: &serde_json::Value, version: &str, installed: u32) -> Option<UiOffer> {
    let assets = release["assets"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_string)
    };
    let (_, patch, name) = assets
        .iter()
        .filter_map(|a| a["name"].as_str())
        .filter_map(|n| crate::ui_pack::parse_asset_name(n).map(|(v, p)| (v, p, n)))
        .filter(|(v, p, n)| v == version && *p > installed && url_of(&format!("{n}.sig")).is_some())
        .max_by_key(|(_, p, _)| *p)?;
    Some(UiOffer {
        patch,
        asset_name: name.to_string(),
        asset_url: url_of(name)?,
        sig_url: url_of(&format!("{name}.sig"))?,
        notes_url: url_of(&format!("{}.md", name.trim_end_matches(".zip"))),
        notes: String::new(),
    })
}

/// The interface update for the running version, if its release has one
/// newer than `installed`; `Ok(None)` when it has none (or no release).
async fn fetch_ui_offer(
    client: &reqwest::Client,
    feed: &str,
    version: &str,
    installed: u32,
) -> Result<Option<UiOffer>, String> {
    let url = release_by_tag_url(feed, &format!("v{version}"));
    let resp = client.get(&url).send().await.map_err(|e| format!("network error: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("release returned HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("JSON parse error: {e}"))?;
    let Some(mut offer) = pick_ui_offer(&body, version, installed) else {
        return Ok(None);
    };
    if is_release_build()
        && [Some(&offer.asset_url), Some(&offer.sig_url), offer.notes_url.as_ref()]
            .into_iter()
            .flatten()
            .any(|u| !u.starts_with("https://"))
    {
        return Err("asset URL is not HTTPS".to_string());
    }
    if let Some(notes_url) = &offer.notes_url {
        // Display only (the dialog shows it as plain text); best effort.
        if let Ok(bytes) = fetch_capped(client, notes_url, UI_NOTES_MAX_BYTES as u64 * 4, false).await {
            offer.notes = cut_notes(&String::from_utf8_lossy(&bytes));
        }
    }
    crate::aelog!("[UPDATER] interface update {} for {} is available", offer.patch, version);
    Ok(Some(offer))
}

/// The notes cut to `UI_NOTES_MAX_BYTES` on a character boundary.
fn cut_notes(s: &str) -> String {
    let s = s.trim();
    if s.len() <= UI_NOTES_MAX_BYTES {
        return s.to_string();
    }
    let mut end = UI_NOTES_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// A whole response body, refused above `cap` bytes; `progress` reports the
/// download on the update status.
async fn fetch_capped(client: &reqwest::Client, url: &str, cap: u64, progress: bool) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let resp = within(RESPONSE_WAIT, client.get(url).send())
        .await
        .map_err(|e| format!("download failed: {e}"))?
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    if total > cap {
        return Err(format!("download is {total} bytes, more than {cap}"));
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = within(DOWNLOAD_IDLE, stream.next()).await.map_err(|e| format!("download error: {e}"))? {
        let chunk = chunk.map_err(|e| format!("download error: {e}"))?;
        if (body.len() + chunk.len()) as u64 > cap {
            return Err(format!("download is more than {cap} bytes"));
        }
        body.extend_from_slice(&chunk);
        if progress {
            set_progress("downloading", body.len() as u64, total, None);
        }
    }
    Ok(body)
}

/// Downloads the offered pack and its signature, checks them as a start
/// would and places them in `ui_dir`. Nothing is written unless the pack
/// verifies.
async fn download_ui_pack(
    offer: &UiOffer,
    version: &str,
    pubkey: &[u8; 32],
    ui_dir: &Path,
    ua: &str,
) -> Result<(), String> {
    set_progress("downloading", 0, 0, None);
    crate::aelog!("[UPDATER] downloading {}", offer.asset_url);
    let api_client = build_api_client(ua).map_err(|e| format!("http client: {e}"))?;
    let dl_client = build_download_client(ua).map_err(|e| format!("http client: {e}"))?;
    let bytes = fetch_capped(&dl_client, &offer.asset_url, crate::ui_pack::MAX_PACK_BYTES, true).await?;
    let sig = fetch_capped(&api_client, &offer.sig_url, 4096, false).await.map_err(|e| format!("sig {e}"))?;
    let sig = String::from_utf8(sig).map_err(|_| "the signature is not text".to_string())?;

    set_progress("verifying", 0, 0, None);
    let pack = crate::ui_pack::verify_pack(&bytes, &sig, &offer.asset_name, version, pubkey)
        .map_err(|e| format!("signature check failed — nothing was changed: {e}"))?;
    crate::aelog!("[UPDATER] interface update {} verified ({} files)", pack.patch, pack.files.len());

    set_progress("installing", 0, 0, None);
    crate::ui_pack::write_pack(ui_dir, &offer.asset_name, &bytes, &sig)
}

/// Installs the interface update `update_check` found, then restarts into it.
#[tauri::command]
pub async fn ui_pack_install(app: tauri::AppHandle) -> Result<(), String> {
    let offer = {
        let lock = CHECK_CACHE.lock().unwrap();
        match &*lock {
            Some(c) if c.available => return Err("an app update is available; install it instead".to_string()),
            Some(c) => c.ui.clone().ok_or_else(|| "no interface update is available".to_string())?,
            None => return Err("run update_check first".to_string()),
        }
    };
    // Anti-rollback: never the one this run serves, never an older one.
    let installed = crate::ui_pack::active_patch();
    if offer.patch <= installed {
        return Err(format!("interface update {} is not newer than {}", offer.patch, installed));
    }

    let claim = InstallClaim::try_claim(&PROGRESS, crate::audio::converter::is_running)?;

    let exe = std::env::current_exe().map_err(|e| format!("cannot determine the exe: {e}"))?;
    let exe_dir = exe.parent().ok_or_else(|| "cannot determine exe directory".to_string())?;
    let ui_dir = exe_dir.join(crate::ui_pack::UI_DIR);
    let probe = exe_dir.join(".aura-update-probe");
    match std::fs::OpenOptions::new().write(true).create(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(_) => {
            return Err(format!(
                "the folder {} is not writable — you may need to move \
                 Aura Engine out of Program Files.",
                exe_dir.display()
            ));
        }
    }
    let Some(pubkey) = active_pubkey() else {
        return Err("no public key configured".to_string());
    };

    // Stop playback: the app restarts into the new interface.
    crate::player::controller::get().stop();

    let ua = format!("AuraEngine/{}", env!("CARGO_PKG_VERSION"));
    tokio::spawn(async move {
        claim.hand_off();
        if let Err(e) = download_ui_pack(&offer, env!("CARGO_PKG_VERSION"), &pubkey, &ui_dir, &ua).await {
            set_progress("error", 0, 0, Some(e.clone()));
            crate::aelog!("[UPDATER] interface update failed: {}", e);
            return;
        }
        crate::aelog!("[UPDATER] interface update {} placed; relaunching", offer.patch);
        if let Err(e) = relaunch(&exe, std::process::id()) {
            // The pack is in place: the next start serves it.
            set_progress("error", 0, 0, Some(format!("{e} — the interface update applies at the next start")));
            return;
        }
        set_progress("restarting", 0, 0, None);
        std::thread::sleep(std::time::Duration::from_millis(400));
        app.exit(0);
    });
    Ok(())
}

// ── update_install command ────────────────────────────────────────────────

#[tauri::command]
pub async fn update_install(app: tauri::AppHandle) -> Result<(), String> {
    // Read cached check result.
    let cache = {
        let lock = CHECK_CACHE.lock().unwrap();
        match &*lock {
            Some(c) if c.available => c.clone(),
            Some(_) => return Err("no update is available".to_string()),
            None => return Err("run update_check first".to_string()),
        }
    };

    // Anti-rollback: verify again that the tag is newer (the cache may have
    // been built with a test feed; a second call could replay an old one).
    if !semver_gt(&cache.tag, env!("CARGO_PKG_VERSION")) {
        return Err(format!(
            "{} is not newer than {}",
            cache.tag,
            env!("CARGO_PKG_VERSION")
        ));
    }

    // Refuse if an install is already in flight or a conversion is running,
    // and claim the slot in the same step, so neither a second install nor a
    // conversion can slip in before the hand-off below. Every early return
    // from here on releases the claim.
    let claim = InstallClaim::try_claim(&PROGRESS, crate::audio::converter::is_running)?;

    // Is the exe directory writable?
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .ok_or_else(|| "cannot determine exe directory".to_string())?;

    let probe = exe_dir.join(".aura-update-probe");
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(_) => {
            return Err(format!(
                "the folder {} is not writable — you may need to move \
                 Aura Engine out of Program Files. Release page: {}",
                exe_dir.display(),
                cache.release_url
            ));
        }
    }

    // Checked before playback stops, so a refusal leaves the music playing.
    let Some(pubkey) = active_pubkey() else {
        return Err("no public key configured".to_string());
    };

    // Stop playback.
    crate::player::controller::get().stop();

    // Hand off to the async worker and return immediately; the UI polls
    // /update/status every 250 ms. The phase is already "checking" (claim);
    // the worker takes the claim over as its first step, so a task that
    // never runs still puts the phase back.
    let ua = format!("AuraEngine/{}", env!("CARGO_PKG_VERSION"));
    tokio::spawn(async move {
        claim.hand_off();
        let result = do_install(cache, exe_dir, pubkey, &ua).await;
        if let Err(e) = result {
            set_progress("error", 0, 0, Some(e.clone()));
            crate::aelog!("[UPDATER] install failed: {}", e);
            return;
        }
        // do_install only returns Ok after spawning the new process and
        // calling app.exit(0), so we set "restarting" just before that.
        set_progress("restarting", 0, 0, None);
        std::thread::sleep(std::time::Duration::from_millis(400));
        app.exit(0);
    });

    Ok(())
}

async fn do_install(
    cache: CheckCache,
    exe_dir: PathBuf,
    pubkey: [u8; 32],
    ua: &str,
) -> Result<(), String> {
    // ── download directory ──────────────────────────────────────────────
    let dl_dir = crate::app_dir::root()
        .or_else(|| local_app_data_dir().map(|d| d.join("AuraEngine")))
        .ok_or("cannot locate %LOCALAPPDATA%")?
        .join("updates")
        .join(&cache.tag);
    std::fs::create_dir_all(&dl_dir)
        .map_err(|e| format!("cannot create download dir: {}", e))?;

    let zip_path = dl_dir.join(&cache.asset_name);

    // ── download zip with progress ─────────────────────────────────────
    set_progress("downloading", 0, 0, None);
    crate::aelog!("[UPDATER] downloading {}", cache.asset_url);

    let api_client = build_api_client(ua).map_err(|e| format!("http client: {}", e))?;
    let dl_client = build_download_client(ua).map_err(|e| format!("http client: {}", e))?;

    download_streaming(&dl_client, &cache.asset_url, &zip_path).await?;

    // ── download .sig ──────────────────────────────────────────────────
    crate::aelog!("[UPDATER] downloading signature");
    let sig_bytes_resp = api_client
        .get(&cache.sig_url)
        .send()
        .await
        .map_err(|e| format!("sig download failed: {}", e))?;
    if !sig_bytes_resp.status().is_success() {
        return Err(format!("sig download failed: HTTP {}", sig_bytes_resp.status()));
    }
    let sig_text = sig_bytes_resp
        .text()
        .await
        .map_err(|e| format!("sig read error: {}", e))?;

    // ── SHA-256 and signature verify ───────────────────────────────────
    set_progress("verifying", 0, 0, None);
    crate::aelog!("[UPDATER] verifying signature");

    let sha256_hex = sha256_file(&zip_path)?;
    let message = build_update_message(&cache.tag, &cache.asset_name, &sha256_hex);
    verify_ed25519(&pubkey, message.as_bytes(), sig_text.trim())
        .map_err(|e| format!("signature check failed — nothing was changed: {}", e))?;

    crate::aelog!("[UPDATER] signature OK");

    // ── extract ────────────────────────────────────────────────────────
    set_progress("installing", 0, 0, None);
    crate::aelog!("[UPDATER] extracting");

    let new_path = exe_dir.join("aura-engine.exe.new");
    let old_path = exe_dir.join("aura-engine.exe.old");
    let live_path = exe_dir.join("aura-engine.exe");

    // The exe lands at new_path directly; no copy needed, which removes the
    // window where a replacement could be swapped in after verification.
    extract_new_exe(&zip_path, &dl_dir, &new_path)?;

    // ── atomic swap ────────────────────────────────────────────────────
    // Delete any stale .old from a previous interrupted update.
    let _ = std::fs::remove_file(&old_path);

    // Rename live → .old (Windows allows renaming a running image).
    std::fs::rename(&live_path, &old_path)
        .map_err(|e| format!("cannot rename running exe to .old: {}", e))?;

    // Rename .new → live.
    if let Err(e) = std::fs::rename(&new_path, &live_path) {
        // Roll back: restore the running exe from .old.
        match std::fs::rename(&old_path, &live_path) {
            Ok(()) => return Err(format!("swap failed (rolled back): {}", e)),
            Err(rb) => return Err(format!(
                "swap failed and rollback also failed — \
                 old exe is at {old} — rename it to {live} manually. \
                 Swap error: {e}. Rollback error: {rb}.",
                old = old_path.display(),
                live = live_path.display(),
            )),
        }
    }

    match place_asio_files(&dl_dir.join(ASIO_ZIP_DIR), &exe_dir.join(ASIO_ZIP_DIR)) {
        (0, 0) => {}
        (n, 0) => crate::aelog!("[UPDATER] placed the ASIO driver's {} files beside the exe", n),
        (n, f) => crate::aelog!("[UPDATER] the ASIO driver: {} placed, {} failed; the new build fetches what is not its own", n, f),
    }

    crate::aelog!("[UPDATER] swap complete; relaunching as {}", cache.tag);

    // ── relaunch ───────────────────────────────────────────────────────
    // If the new process fails to start, roll back the swap immediately so
    // the user is not left without a running application.
    if let Err(e) = relaunch(&live_path, std::process::id()) {
        // Attempt rollback: live → .new, then .old → live.
        let r1 = std::fs::rename(&live_path, &new_path);
        let r2 = std::fs::rename(&old_path, &live_path);
        let rb_msg = match (&r1, &r2) {
            (Ok(()), Ok(())) => "rolled back".to_string(),
            (Err(e1), _) => format!(
                "rollback step 1 failed ({e1}) — original exe should be at {old}",
                old = old_path.display(),
            ),
            (Ok(()), Err(e2)) => format!(
                "rollback step 2 failed ({e2}) — original exe may be at {old}",
                old = old_path.display(),
            ),
        };
        return Err(format!("relaunch failed ({rb_msg}): {e}"));
    }

    Ok(())
}

/// Download a URL to a local path, updating the progress counter.
async fn download_streaming(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    use futures_util::StreamExt;

    let resp = within(RESPONSE_WAIT, client.get(url).send())
        .await
        .map_err(|e| format!("download failed: {}", e))?
        .map_err(|e| format!("download failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }

    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {}", dest.display(), e))?;

    let mut done: u64 = 0;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = within(DOWNLOAD_IDLE, stream.next())
        .await
        .map_err(|e| format!("download error: {}", e))?
    {
        let chunk = chunk.map_err(|e| format!("download error: {}", e))?;
        file.write_all(&chunk)
            .map_err(|e| format!("write error: {}", e))?;
        done += chunk.len() as u64;
        set_progress("downloading", done, total, None);
    }

    Ok(())
}

/// Extract `aura-engine.exe` (and optionally README.txt, LICENSE.txt,
/// THIRD-PARTY-NOTICES.txt) from the zip. The exe is written directly to `exe_dest` — no intermediate copy,
/// so there is no window for an attacker to swap the file between extraction
/// and the install move. Other allowed files land in `dl_dir`.
fn extract_new_exe(zip_path: &Path, dl_dir: &Path, exe_dest: &Path) -> Result<(), String> {
    use zip::ZipArchive;

    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("cannot open zip: {}", e))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("zip open failed: {}", e))?;

    let mut exe_found = false;
    // What an earlier, interrupted update left of the driver is not this one's.
    let asio_dl = dl_dir.join(ASIO_ZIP_DIR);
    let _ = std::fs::remove_dir_all(&asio_dl);

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {} error: {}", i, e))?;

        let raw_name = entry.name().to_string();

        // Skip directory entries.
        if raw_name.ends_with('/') || raw_name.ends_with('\\') {
            continue;
        }

        // Only the explicitly allowed files; anything else is skipped
        // unread, at any depth (the bare app's zip carries
        // fir-optimizer/output/PUT-FILTERS-HERE.txt).
        let filename = raw_name
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();

        // The driver's files go to dl_dir\asio, for place_asio_files. A
        // failure with them never stops the update: the exe is what it is
        // for, and the new build fetches its driver itself.
        if let Some(name) = asio_zip_entry(&raw_name) {
            let dest = asio_dl.join(name);
            let r = std::fs::create_dir_all(&asio_dl)
                .map_err(|e| format!("cannot create {}: {}", asio_dl.display(), e))
                .and_then(|_| extract_entry(&mut entry, &raw_name, &dest));
            if let Err(e) = r {
                let _ = std::fs::remove_file(&dest);
                crate::aelog!("[UPDATER] the ASIO driver's {} was not extracted: {}", name, e);
            }
            continue;
        }

        if !ALLOWED_FILES.contains(&filename.as_str()) {
            continue;
        }

        // An allowed file must be exactly <top-folder>/<filename> with no
        // traversal.
        validate_zip_entry_name(&raw_name)?;

        // The exe goes to exe_dest directly; all other files go into dl_dir.
        let dest: PathBuf = if filename == "aura-engine.exe" {
            exe_dest.to_owned()
        } else {
            dl_dir.join(&filename)
        };
        extract_entry(&mut entry, &raw_name, &dest)?;

        if filename == "aura-engine.exe" {
            exe_found = true;
        }
    }

    if exe_found { Ok(()) } else { Err("aura-engine.exe not found in the zip".to_string()) }
}

/// Writes one zip entry to `dest`, counting the bytes actually decompressed
/// (not the header-declared size, which is attacker-controlled).
fn extract_entry(entry: &mut impl Read, raw_name: &str, dest: &Path) -> Result<(), String> {
    let mut out = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {}", dest.display(), e))?;
    let mut bytes_written: u64 = 0;
    let mut buf = [0u8; 16384];
    loop {
        let n = entry.read(&mut buf)
            .map_err(|e| format!("extract {} error: {}", raw_name, e))?;
        if n == 0 {
            return Ok(());
        }
        bytes_written += n as u64;
        if bytes_written > MAX_ENTRY_BYTES {
            return Err(format!(
                "entry {} exceeded the {} byte limit during extraction",
                raw_name, MAX_ENTRY_BYTES
            ));
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("write error extracting {}: {}", raw_name, e))?;
    }
}

/// Puts `tmp` in `dest`'s place. When `dest` cannot be replaced (a player
/// has it loaded: Windows refuses that, allows a rename) it is moved aside
/// as `<dest>.<stamp>.old` first; `tmp` is removed when neither works.
fn replace_or_aside(tmp: &Path, dest: &Path, stamp: u64) -> Result<(), String> {
    if std::fs::rename(tmp, dest).is_ok() {
        return Ok(());
    }
    let mut old = dest.as_os_str().to_owned();
    old.push(format!(".{stamp}.old"));
    let r = std::fs::rename(dest, &old)
        .and_then(|_| std::fs::rename(tmp, dest))
        .map_err(|e| format!("cannot replace {}: {}", dest.display(), e));
    if r.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    r
}

/// Deletes what earlier replacements moved aside in `dir`, those nobody
/// holds any longer.
fn remove_old_files(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().ends_with(".old") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn stamp_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The driver's file an entry is, when it is exactly
/// `<folder>/asio/<one of ASIO_FILES>`; anything else under `asio/` is not.
fn asio_zip_entry(name: &str) -> Option<&'static str> {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() != 3 || parts[0].is_empty() || parts[0] == ".." || parts[0].contains(['\\', ':']) || parts[1] != ASIO_ZIP_DIR {
        return None;
    }
    ASIO_FILES.iter().copied().find(|f| *f == parts[2])
}

/// Puts the driver's files the zip brought (`from`, dl_dir\asio) into
/// `asio\` beside the new exe, each written beside its place first and then
/// renamed over it (a file held open moved aside first). Nothing runs these
/// files — the app only copies them to where the driver is installed, after
/// checking they are its own — so a failure here leaves the update
/// standing: the new build fetches its driver itself when it is installed.
/// Every file is tried, whatever happened to the one before: (placed,
/// failed), each failure logged.
fn place_asio_files(from: &Path, to: &Path) -> (usize, usize) {
    let (mut placed, mut failed) = (0, 0);
    let stamp = stamp_now();
    remove_old_files(to);
    for name in ASIO_FILES {
        let src = from.join(name);
        if !src.is_file() {
            continue;
        }
        let dest = to.join(name);
        let tmp = to.join(format!("{name}.new"));
        let r = std::fs::create_dir_all(to)
            .map_err(|e| format!("cannot create {}: {}", to.display(), e))
            .and_then(|_| std::fs::copy(&src, &tmp).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("cannot write {}: {}", tmp.display(), e)
            }))
            .and_then(|_| replace_or_aside(&tmp, &dest, stamp));
        match r {
            Ok(()) => placed += 1,
            Err(e) => {
                failed += 1;
                crate::aelog!("[UPDATER] the ASIO driver's {} was not placed: {}", name, e);
            }
        }
    }
    (placed, failed)
}

// ── the ASIO driver of this build, fetched ───────────────────────────────

/// Where a release's driver asset is: its name is made from the version
/// alone (release.yml gives it), so no feed is asked.
const ASIO_ASSET_BASE: &str = "https://github.com/ToxaDev/aura-engine/releases/download";

/// The driver asset is two DLLs and two texts, well under a megabyte.
const ASIO_ASSET_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// The driver asset of release `version` (without the v).
pub fn asio_asset_url(version: &str) -> String {
    format!("{ASIO_ASSET_BASE}/v{version}/aura-player-asio-v{version}-windows.zip")
}

/// Why the driver could not be fetched.
#[derive(Debug, PartialEq, Eq)]
pub enum AsioFetchError {
    /// Not downloaded (no connection, HTTP error, too big).
    Network(String),
    /// Downloaded, but not this build's driver: a DLL missing, or its
    /// SHA-256 not the one this build was made with. Nothing was written.
    Mismatch(String),
    /// Right, but no folder took the files.
    Write(String),
}

/// The asset's entries that are the driver's files: exactly
/// `<folder>/<one of ASIO_FILES>`, with no traversal.
fn asio_asset_entry(name: &str) -> Option<&'static str> {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[0] == ".." || parts[0].contains(['\\', ':']) {
        return None;
    }
    ASIO_FILES.iter().copied().find(|f| *f == parts[1])
}

/// Downloads the driver asset at `url` and writes its files into the first
/// of `dirs` that takes them (each written beside its place, then renamed
/// over it) — only when both DLLs are in it and their SHA-256s are `want`
/// (64-bit, 32-bit): the build carries its own driver's hashes, so a file
/// that is not exactly that driver is never written. The folder written to.
pub async fn fetch_asio_driver(url: &str, want: (&str, &str), dirs: &[PathBuf]) -> Result<PathBuf, AsioFetchError> {
    use sha2::{Digest, Sha256};
    use AsioFetchError::*;

    let ua = format!("AuraEngine/{}", env!("CARGO_PKG_VERSION"));
    let client = build_download_client(&ua).map_err(|e| Network(format!("http client: {e}")))?;
    crate::aelog!("[INPUT] downloading the driver: {}", url);
    let resp = within(RESPONSE_WAIT, client.get(url).send())
        .await
        .map_err(|e| Network(format!("download failed: {e}")))?
        .map_err(|e| Network(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Network(format!("download failed: HTTP {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n > ASIO_ASSET_MAX_BYTES) {
        return Err(Network("the driver asset is too big".into()));
    }
    // Counted as it arrives, whatever the header said.
    let mut resp = resp;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = within(DOWNLOAD_IDLE, resp.chunk())
        .await
        .map_err(|e| Network(format!("download error: {e}")))?
        .map_err(|e| Network(format!("download error: {e}")))?
    {
        if (body.len() + chunk.len()) as u64 > ASIO_ASSET_MAX_BYTES {
            return Err(Network("the driver asset is too big".into()));
        }
        body.extend_from_slice(&chunk);
    }

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).map_err(|e| Mismatch(format!("not a zip: {e}")))?;
    let mut files: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| Mismatch(format!("zip entry {i}: {e}")))?;
        let Some(name) = asio_asset_entry(entry.name()) else { continue };
        let mut data = Vec::new();
        (&mut entry).take(ASIO_ASSET_MAX_BYTES + 1).read_to_end(&mut data).map_err(|e| Mismatch(format!("{name}: {e}")))?;
        if data.len() as u64 > ASIO_ASSET_MAX_BYTES {
            return Err(Mismatch(format!("{name} is too big")));
        }
        files.retain(|(n, _)| *n != name);
        files.push((name, data));
    }
    let hex = |d: &[u8]| Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect::<String>();
    for (dll, sha) in [(ASIO_FILES[0], want.0), (ASIO_FILES[1], want.1)] {
        match files.iter().find(|(n, _)| *n == dll) {
            None => return Err(Mismatch(format!("{dll} is not in the asset"))),
            Some((_, d)) if !hex(d).eq_ignore_ascii_case(sha) => return Err(Mismatch(format!("{dll} is not this build's driver"))),
            Some(_) => {}
        }
    }

    let mut last = String::from("no folder to write to");
    for dir in dirs {
        let put = || -> Result<(), String> {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
            remove_old_files(dir);
            let stamp = stamp_now();
            for (name, data) in &files {
                let dest = dir.join(name);
                let tmp = dir.join(format!("{name}.new"));
                std::fs::write(&tmp, data).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("cannot write {}: {}", tmp.display(), e)
                })?;
                replace_or_aside(&tmp, &dest, stamp)?;
            }
            Ok(())
        };
        match put() {
            Ok(()) => {
                crate::aelog!("[INPUT] the driver is in {}", dir.display());
                return Ok(dir.clone());
            }
            Err(e) => last = e,
        }
    }
    Err(Write(last))
}

/// Validate that a zip entry name has no traversal, absolute paths, or
/// unexpected depth. Format must be `<folder>/<filename>` with exactly one
/// slash and both parts non-empty.
fn validate_zip_entry_name(name: &str) -> Result<(), String> {
    // Absolute Windows paths
    if name.len() >= 3 && name.as_bytes()[1] == b':' {
        return Err(format!("absolute path in zip: {}", name));
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("absolute path in zip: {}", name));
    }
    // Traversal
    if name.contains("..") {
        return Err(format!("path traversal in zip: {}", name));
    }
    // Must have exactly one slash component for <folder>/<file>
    let parts: Vec<&str> = name.splitn(3, '/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("unexpected zip entry structure: {}", name));
    }
    // If there are more slashes the file is nested too deep.
    if parts.len() > 2 {
        return Err(format!("nested path in zip (only top-level files allowed): {}", name));
    }
    Ok(())
}

// ── relaunch ──────────────────────────────────────────────────────────────

#[cfg(windows)]
fn relaunch(new_exe: &Path, old_pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    std::process::Command::new(new_exe)
        .arg("--after-update")
        .arg(old_pid.to_string())
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch new exe: {}", e))
}

#[cfg(not(windows))]
fn relaunch(new_exe: &Path, old_pid: u32) -> Result<(), String> {
    std::process::Command::new(new_exe)
        .arg("--after-update")
        .arg(old_pid.to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch new exe: {}", e))
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Short-deadline client for the GitHub API check and the small `.sig` fetch.
fn build_api_client(user_agent: &str) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(10))
        // HTTPS-only in production; the test hook talks to a local HTTP server.
        .https_only(is_release_build())
        .build()
}

/// Client used for the zip download. No overall deadline: a client timeout
/// in reqwest 0.11 is the whole request, body included, so a slow line lost a
/// big download at the deadline however fast the bytes were still coming.
/// The connect timeout catches unreachable hosts; [`within`] around the
/// response and around every chunk catches a connection that went silent.
fn build_download_client(user_agent: &str) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(std::time::Duration::from_secs(15))
        // HTTPS-only in production; the test hook talks to a local HTTP server.
        .https_only(is_release_build())
        .build()
}

/// How long a download waits for the response headers.
pub(crate) const RESPONSE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a download may go without a single byte before it is given up.
pub(crate) const DOWNLOAD_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// `fut`, or an error once `limit` passes without it finishing: the idle
/// deadline the download client does not have (reqwest 0.11 has no read
/// timeout).
pub(crate) async fn within<T>(
    limit: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| format!("the server sent nothing for {} s", limit.as_secs()))
}

/// Build the exact message that is signed/verified.
pub fn build_update_message(tag: &str, asset_name: &str, sha256_hex: &str) -> String {
    format!(
        "aura-engine-update-v1\n{}\n{}\n{}\n",
        tag, asset_name, sha256_hex
    )
}

/// Hex-decode a 64-char public key string into 32 bytes.
pub fn decode_pubkey_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Compute the lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("cannot open {} for hashing: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read error while hashing: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(result.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Verify an Ed25519 signature. `sig_b64` is trimmed base64.
pub fn verify_ed25519(
    pubkey_bytes: &[u8; 32],
    message: &[u8],
    sig_b64: &str,
) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use ed25519_dalek::{Signature, VerifyingKey, Verifier};

    let sig_raw = STANDARD
        .decode(sig_b64)
        .map_err(|e| format!("base64 decode: {}", e))?;
    if sig_raw.len() != 64 {
        return Err(format!(
            "signature is {} bytes, expected 64",
            sig_raw.len()
        ));
    }
    let sig_arr: [u8; 64] = sig_raw.try_into().unwrap();

    let vk = VerifyingKey::from_bytes(pubkey_bytes)
        .map_err(|e| format!("invalid public key: {}", e))?;
    let sig = Signature::from_bytes(&sig_arr);

    vk.verify(message, &sig)
        .map_err(|_| "signature mismatch".to_string())
}

/// Semver comparison: returns true when `a` is strictly greater than `b`.
/// Both may be prefixed with `v`. Compares as [major, minor, patch].
pub fn semver_gt(a: &str, b: &str) -> bool {
    fn parse(s: &str) -> Option<[u64; 3]> {
        let s = s.trim_start_matches('v');
        let mut it = s.splitn(4, '.');
        let major: u64 = it.next()?.parse().ok()?;
        let minor: u64 = it.next()?.parse().ok()?;
        // Patch may have a pre-release suffix; take only the leading digits.
        let patch_raw = it.next()?;
        let patch: u64 = patch_raw
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;
        Some([major, minor, patch])
    }
    match (parse(a), parse(b)) {
        (Some(av), Some(bv)) => av > bv,
        _ => false,
    }
}

/// `%LOCALAPPDATA%` on Windows; equivalent on other platforms.
fn local_app_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        dirs::data_local_dir()
    }
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── filter_release_notes ─────────────────────────────────────────────

    fn make_release(tag: &str, draft: bool, prerelease: bool, body: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "body": body,
        })
    }

    #[test]
    fn filter_skips_older_than_current() {
        let releases = vec![
            make_release("v1.3.4", false, false, "old"),
            make_release("v1.3.3", false, false, "older"),
        ];
        let entries = filter_release_notes(&releases, "1.3.4", "v1.3.4");
        assert!(entries.is_empty(), "nothing newer than current");
    }

    #[test]
    fn filter_skips_drafts_and_prereleases() {
        let releases = vec![
            make_release("v1.4.0", true, false, "draft"),
            make_release("v1.4.1", false, true, "beta"),
            make_release("v1.4.2", false, false, "real"),
        ];
        let entries = filter_release_notes(&releases, "1.3.4", "v1.4.2");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, "1.4.2");
    }

    #[test]
    fn filter_skips_newer_than_latest() {
        // latest is 1.4.0 but 1.5.0 is also in the list (shouldn't happen in
        // practice but the filter must be robust)
        let releases = vec![
            make_release("v1.5.0", false, false, "future"),
            make_release("v1.4.0", false, false, "latest"),
            make_release("v1.3.5", false, false, "skipped"),
        ];
        let entries = filter_release_notes(&releases, "1.3.4", "v1.4.0");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version, "1.4.0");
        assert_eq!(entries[1].version, "1.3.5");
    }

    #[test]
    fn filter_three_skipped_sorted_newest_first() {
        let releases = vec![
            // GitHub returns newest first but we test with mixed order
            make_release("v1.3.5", false, false, "patch"),
            make_release("v1.3.7", false, false, "latest-ish"),
            make_release("v1.3.6", false, false, "middle"),
        ];
        let entries = filter_release_notes(&releases, "1.3.4", "v1.3.7");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].version, "1.3.7");
        assert_eq!(entries[1].version, "1.3.6");
        assert_eq!(entries[2].version, "1.3.5");
    }

    #[test]
    fn filter_empty_list_returns_empty() {
        let entries = filter_release_notes(&[], "1.3.4", "v1.4.0");
        assert!(entries.is_empty());
    }

    // ── the instruments pack's release never hides the app's ────────────

    fn release_with(tag: &str, prerelease: bool, assets: &[String]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": prerelease,
            "body": "",
            "html_url": format!("https://example.invalid/{tag}"),
            "assets": assets.iter().map(|n| serde_json::json!({
                "name": n,
                "browser_download_url": format!("http://127.0.0.1/{tag}/{n}"),
            })).collect::<Vec<_>>(),
        })
    }

    fn app_release(tag: &str) -> serde_json::Value {
        let v = tag.trim_start_matches('v');
        release_with(tag, false, &[format!("aura-engine-v{v}{APP_ZIP_SUFFIX}"), format!("aura-engine-v{v}{APP_SIG_SUFFIX}")])
    }

    fn pack_release(n: u32, prerelease: bool) -> serde_json::Value {
        release_with(&format!("instruments-pack-{n}"), prerelease, &[format!("aura-instruments-pack-{n}.zip")])
    }

    #[test]
    fn a_pack_release_is_not_an_app_release() {
        assert!(is_app_release(&app_release("v1.5.0")));
        assert!(!is_app_release(&pack_release(2, true)));
        assert!(!is_app_release(&pack_release(2, false)));
        let no_sig = release_with("v1.5.0", false, &[format!("aura-engine-v1.5.0{APP_ZIP_SUFFIX}")]);
        assert!(!is_app_release(&no_sig), "no signature, no update");
        let short = release_with("v1.5", false, &[format!("aura-engine-v1.5{APP_ZIP_SUFFIX}"), format!("aura-engine-v1.5{APP_SIG_SUFFIX}")]);
        assert!(!is_app_release(&short));
    }

    #[test]
    fn the_newest_app_release_passes_packs_drafts_and_prereleases() {
        let mut draft = app_release("v1.6.0");
        draft["draft"] = true.into();
        let mut beta = app_release("v1.5.1");
        beta["prerelease"] = true.into();
        let list = vec![pack_release(3, false), draft, beta, app_release("v1.4.2"), app_release("v1.5.0"), pack_release(2, true)];
        assert_eq!(newest_app_release(&list).and_then(|r| r["tag_name"].as_str()), Some("v1.5.0"));
        assert!(newest_app_release(&[pack_release(2, false)]).is_none());
    }

    /// A local feed: `/…/releases/latest` and `/…/releases?per_page=30`;
    /// returns the feed URL and the paths asked.
    fn serve_feed(latest: serde_json::Value, list: serde_json::Value) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                let _ = r.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let body = if path.ends_with("/releases/latest") {
                    latest.to_string()
                } else if path.ends_with("/releases?per_page=30") {
                    list.to_string()
                } else {
                    let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        (format!("http://{addr}/repos/x/releases/latest"), asked)
    }

    /// A pack release published as a normal one by mistake becomes "latest":
    /// the app still finds its own newest release, and offers it.
    #[tokio::test]
    async fn a_pack_release_marked_latest_does_not_hide_app_updates() {
        let list = serde_json::json!([pack_release(2, false), app_release("v99.0.0"), app_release("v1.0.0")]);
        let (feed, asked) = serve_feed(pack_release(2, false), list);
        let client = build_api_client("test").unwrap();
        let c = fetch_release(&client, &feed).await.unwrap();
        assert_eq!(c.tag, "v99.0.0");
        assert!(c.available);
        assert_eq!(c.asset_name, format!("aura-engine-v99.0.0{APP_ZIP_SUFFIX}"));
        assert!(c.sig_url.ends_with(APP_SIG_SUFFIX));
        assert_eq!(*asked.lock().unwrap(), ["/repos/x/releases/latest", "/repos/x/releases?per_page=30"]);
    }

    /// The usual case: "latest" is the app's, the list is not asked.
    #[tokio::test]
    async fn an_app_release_as_latest_is_taken_as_it_is() {
        let (feed, asked) = serve_feed(app_release("v99.0.0"), serde_json::json!([]));
        let client = build_api_client("test").unwrap();
        let c = fetch_release(&client, &feed).await.unwrap();
        assert_eq!(c.tag, "v99.0.0");
        assert!(c.available);
        assert_eq!(*asked.lock().unwrap(), ["/repos/x/releases/latest"]);
    }

    #[test]
    fn releases_list_url_strips_latest() {
        let url = releases_list_url("http://127.0.0.1:9999/releases/latest");
        assert_eq!(url, "http://127.0.0.1:9999/releases?per_page=30");
    }

    #[test]
    fn releases_list_url_nonstandard_passthrough() {
        let url = releases_list_url("http://127.0.0.1:9999/custom-feed");
        assert_eq!(url, "http://127.0.0.1:9999/custom-feed");
    }

    // ── semver_gt ────────────────────────────────────────────────────────

    #[test]
    fn semver_gt_basic() {
        assert!(semver_gt("v1.5.0", "v1.3.4"));
        assert!(semver_gt("v2.0.0", "v1.99.99"));
        assert!(semver_gt("v1.3.5", "v1.3.4"));
        assert!(!semver_gt("v1.3.4", "v1.3.4"));
        assert!(!semver_gt("v1.3.3", "v1.3.4"));
        assert!(!semver_gt("v0.9.0", "v1.0.0"));
    }

    #[test]
    fn semver_gt_no_v_prefix() {
        assert!(semver_gt("1.5.0", "1.3.4"));
        assert!(!semver_gt("1.3.4", "1.5.0"));
    }

    #[test]
    fn semver_gt_v99() {
        assert!(semver_gt("v9.9.9", env!("CARGO_PKG_VERSION")));
    }

    // ── download deadlines ───────────────────────────────────────────────

    /// A silent server is given up after the idle limit; one that answers in
    /// time is not cut, however long the download as a whole takes.
    #[tokio::test]
    async fn within_gives_up_only_on_silence() {
        let silent = within(std::time::Duration::from_millis(20), std::future::pending::<()>()).await;
        assert!(silent.unwrap_err().contains("sent nothing"));
        assert_eq!(within(std::time::Duration::from_millis(20), async { 7 }).await, Ok(7));
    }

    // ── build_update_message ─────────────────────────────────────────────

    #[test]
    fn message_format() {
        let msg = build_update_message(
            "v1.5.0",
            "aura-engine-v1.5.0-windows-x64.zip",
            "abc123",
        );
        assert_eq!(
            msg,
            "aura-engine-update-v1\nv1.5.0\naura-engine-v1.5.0-windows-x64.zip\nabc123\n"
        );
        assert!(msg.ends_with('\n'), "message must end with newline");
    }

    #[test]
    fn message_utf8_bytes() {
        let msg = build_update_message("v1.5.0", "aura-engine-v1.5.0-windows-x64.zip", "dead");
        // Must be valid UTF-8 with no carriage returns.
        assert!(msg.is_ascii());
        assert!(!msg.contains('\r'));
    }

    // ── decode_pubkey_hex ────────────────────────────────────────────────

    #[test]
    fn decode_pubkey_valid() {
        let k = decode_pubkey_hex(
            "cd2a803ad18aadae9e100644d385549a3a46fbfc19df00a8fd0621b424b391a6",
        );
        assert!(k.is_some());
        assert_eq!(k.unwrap()[0], 0xcd);
        assert_eq!(k.unwrap()[1], 0x2a);
    }

    #[test]
    fn decode_pubkey_bad_len() {
        assert!(decode_pubkey_hex("cd2a").is_none());
        assert!(decode_pubkey_hex("").is_none());
    }

    #[test]
    fn decode_pubkey_bad_char() {
        let hex63 = "cd2a803ad18aadae9e100644d385549a3a46fbfc19df00a8fd0621b424b391a";
        assert!(decode_pubkey_hex(&format!("{}g", hex63)).is_none());
    }

    // ── signature round-trip ─────────────────────────────────────────────

    #[test]
    fn signature_verify_round_trip() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::{SigningKey, Signer};
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();
        let pubkey_bytes: [u8; 32] = verifying_key.to_bytes();

        let message = build_update_message(
            "v9.9.9",
            "aura-engine-v9.9.9-windows-x64.zip",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );

        let sig: ed25519_dalek::Signature = signing_key.sign(message.as_bytes());
        let sig_b64 = STANDARD.encode(sig.to_bytes());

        assert!(verify_ed25519(&pubkey_bytes, message.as_bytes(), &sig_b64).is_ok());
    }

    #[test]
    fn signature_wrong_key_rejected() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::{SigningKey, Signer};
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let key1 = SigningKey::generate(&mut csprng);
        let key2 = SigningKey::generate(&mut csprng);

        let message = b"aura-engine-update-v1\nv1.5.0\nfoo.zip\nhex\n";
        let sig: ed25519_dalek::Signature = key1.sign(message);
        let sig_b64 = STANDARD.encode(sig.to_bytes());

        // Verify against key2's public key — must fail
        let pk2: [u8; 32] = key2.verifying_key().to_bytes();
        assert!(verify_ed25519(&pk2, message, &sig_b64).is_err());
    }

    #[test]
    fn signature_tampered_message_rejected() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::{SigningKey, Signer};
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let key = SigningKey::generate(&mut csprng);
        let pk: [u8; 32] = key.verifying_key().to_bytes();

        let message = b"aura-engine-update-v1\nv1.5.0\nfoo.zip\nhex\n";
        let sig: ed25519_dalek::Signature = key.sign(message);
        let sig_b64 = STANDARD.encode(sig.to_bytes());

        // One byte changed in the message
        let mut tampered = message.to_vec();
        tampered[0] ^= 0xff;
        assert!(verify_ed25519(&pk, &tampered, &sig_b64).is_err());
    }

    // ── zip entry validation ─────────────────────────────────────────────

    #[test]
    fn zip_entry_valid() {
        assert!(validate_zip_entry_name("aura-engine-v9.9.9/aura-engine.exe").is_ok());
        assert!(validate_zip_entry_name("aura-engine-v1.5.0-windows-x64/README.txt").is_ok());
    }

    #[test]
    fn zip_entry_traversal_rejected() {
        assert!(validate_zip_entry_name("../evil.exe").is_err());
        assert!(validate_zip_entry_name("folder/../../../etc/passwd").is_err());
    }

    #[test]
    fn zip_entry_absolute_rejected() {
        assert!(validate_zip_entry_name("/etc/passwd").is_err());
        assert!(validate_zip_entry_name("C:/Windows/System32/evil.dll").is_err());
    }

    #[test]
    fn zip_entry_nested_rejected() {
        assert!(validate_zip_entry_name("folder/sub/file.exe").is_err());
    }

    #[test]
    fn zip_entry_empty_rejected() {
        assert!(validate_zip_entry_name("").is_err());
        assert!(validate_zip_entry_name("/").is_err());
    }

    // ── swap + rollback ──────────────────────────────────────────────────

    #[test]
    fn swap_and_rollback() {
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("aura-engine.exe");
        let new  = dir.path().join("aura-engine.exe.new");
        let old  = dir.path().join("aura-engine.exe.old");

        // Write sentinel content
        std::fs::write(&live, b"OLD").unwrap();
        std::fs::write(&new,  b"NEW").unwrap();

        // Perform the swap (same logic as do_install)
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&live, &old).unwrap();
        std::fs::rename(&new, &live).unwrap();

        assert_eq!(std::fs::read(&live).unwrap(), b"NEW");
        assert_eq!(std::fs::read(&old).unwrap(),  b"OLD");
        assert!(!new.exists());
    }

    #[test]
    fn swap_rollback_on_second_rename_failure() {
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("aura-engine.exe");
        let old  = dir.path().join("aura-engine.exe.old");

        std::fs::write(&live, b"LIVE").unwrap();

        // Simulate first rename succeeding, second failing (by not creating .new)
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&live, &old).unwrap();
        // .new doesn't exist, so the second rename will fail
        let new_missing = dir.path().join("aura-engine.exe.new");
        let result = std::fs::rename(&new_missing, &live);
        if result.is_err() {
            // Roll back
            let _ = std::fs::rename(&old, &live);
        }

        assert!(live.exists(), "rollback should restore the live exe");
        assert_eq!(std::fs::read(&live).unwrap(), b"LIVE");
    }

    // ── rollback error surfaces the correct path (upd-1) ────────────────

    #[test]
    fn swap_rollback_failure_names_the_path() {
        // Simulate: live→.old succeeds, .new→live fails, .old→live also fails
        // (because .old has been removed). The error message must name old_path.
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("aura-engine.exe");
        let new  = dir.path().join("aura-engine.exe.new");
        let old  = dir.path().join("aura-engine.exe.old");

        std::fs::write(&live, b"ORIG").unwrap();
        // Intentionally do NOT create .new so the swap rename fails.

        let _ = std::fs::remove_file(&old);
        std::fs::rename(&live, &old).unwrap();

        let swap_err = std::fs::rename(&new, &live).unwrap_err();

        // Now also make .old→live rollback fail by removing .old.
        std::fs::remove_file(&old).unwrap();
        let rb_err = std::fs::rename(&old, &live).unwrap_err();

        // Build the message that do_install would build.
        let msg = format!(
            "swap failed and rollback also failed — \
             old exe is at {old_p} — rename it to {live_p} manually. \
             Swap error: {swap_e}. Rollback error: {rb_e}.",
            old_p  = old.display(),
            live_p = live.display(),
            swap_e = swap_err,
            rb_e   = rb_err,
        );
        assert!(msg.contains("rollback also failed"), "msg: {msg}");
        assert!(msg.contains("aura-engine.exe.old"), "msg: {msg}");
    }

    // ── concurrent-install guard (upd-2) ─────────────────────────────────

    // The claim is tested on a local slot, never on the global PROGRESS, so
    // these tests cannot race each other or anything else in the process.
    // (update_install itself needs a Tauri AppHandle; the claim is the part
    // that decides whether a second call gets through.)

    fn slot_in(phase: &str, error: Option<&str>) -> Mutex<UpdateProgress> {
        Mutex::new(UpdateProgress {
            phase: phase.to_string(),
            done: 7,
            total: 9,
            error: error.map(str::to_string),
        })
    }

    #[test]
    fn install_guard_blocks_while_in_progress() {
        for phase in ["checking", "downloading", "verifying", "installing", "restarting"] {
            let slot = slot_in(phase, None);
            let r = InstallClaim::try_claim(&slot, || false);
            assert!(r.is_err(), "phase {phase} should block a second install");
            drop(r);
            assert_eq!(slot.lock().unwrap().phase, phase, "a refusal must not touch the phase");
        }
        for phase in ["idle", "error"] {
            let slot = slot_in(phase, None);
            let claim = InstallClaim::try_claim(&slot, || false).expect("should claim");
            assert_eq!(slot.lock().unwrap().phase, "checking");
            claim.hand_off();
            assert_eq!(slot.lock().unwrap().phase, "checking", "handed off: the worker owns it");
        }
    }

    #[test]
    fn install_claim_released_on_early_return() {
        // A claim dropped without hand_off (an early `return Err` in
        // update_install) puts back exactly what it found, error text included.
        let slot = slot_in("error", Some("sig download failed: HTTP 404"));
        {
            let _claim = InstallClaim::try_claim(&slot, || false).expect("should claim");
            assert_eq!(slot.lock().unwrap().phase, "checking");
        }
        let p = slot.lock().unwrap();
        assert_eq!(p.phase, "error");
        assert_eq!(p.error.as_deref(), Some("sig download failed: HTTP 404"));
        assert_eq!((p.done, p.total), (7, 9));
        drop(p);
        // ...and the slot can be claimed again.
        assert!(InstallClaim::try_claim(&slot, || false).is_ok());
    }

    /// Two threads released together try to claim `slot` through `claim`;
    /// neither result is counted until both have tried. Returns how many of
    /// `rounds` ended with BOTH threads through. Keep `claim` panic-free: a
    /// thread that panics before `tried.wait()` leaves the other one waiting.
    fn double_claims(rounds: usize, claim: fn(&Mutex<UpdateProgress>) -> bool) -> usize {
        use std::sync::{Arc, Barrier};
        let mut doubles = 0;
        for _ in 0..rounds {
            let slot = Arc::new(slot_in("idle", None));
            let start = Arc::new(Barrier::new(2));
            let tried = Arc::new(Barrier::new(2));
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    let (slot, start, tried) = (slot.clone(), start.clone(), tried.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        let ok = claim(&slot);
                        tried.wait(); // a winner has left the slot in "checking"; count after both tried
                        ok
                    })
                })
                .collect();
            let passed = workers.into_iter().map(|w| w.join().unwrap()).filter(|ok| *ok).count();
            if passed == 2 {
                doubles += 1;
            }
        }
        doubles
    }

    #[test]
    fn install_claim_race_two_threads() {
        // The real claim: across many simultaneous pairs, never two winners.
        fn real(slot: &Mutex<UpdateProgress>) -> bool {
            match InstallClaim::try_claim(slot, || false) {
                Ok(c) => {
                    c.hand_off(); // keep "checking" so the other thread sees it
                    true
                }
                Err(_) => false,
            }
        }
        assert_eq!(double_claims(2000, real), 0, "two installs got through together");

        // The harness itself must be able to see the bug it guards against:
        // the pre-fix shape (read the phase, release the lock, set it later)
        // lets both threads through. The sleep yields the CPU, so the two
        // checks overlap even on one core; 50 rounds leave a wide margin.
        fn check_then_set(slot: &Mutex<UpdateProgress>) -> bool {
            let phase = slot.lock().unwrap().phase.clone();
            if phase != "idle" && phase != "error" {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5)); // the work between
            slot.lock().unwrap().phase = "checking".to_string();
            true
        }
        assert!(double_claims(50, check_then_set) > 0, "the race harness cannot see a race");
    }

    // ── install versus conversion ─────────────────────────────────────────

    #[test]
    fn a_running_conversion_refuses_the_install_and_leaves_the_phase() {
        let slot = slot_in("idle", None);
        let r = InstallClaim::try_claim(&slot, || true);
        assert!(r.as_ref().err().is_some_and(|e| e.contains("conversion is in progress")));
        drop(r);
        assert_eq!(slot.lock().unwrap().phase, "idle");
    }

    #[test]
    fn a_conversion_waits_for_no_install() {
        for phase in ["checking", "downloading", "verifying", "installing", "restarting"] {
            let slot = slot_in(phase, None);
            let mut started = false;
            let r = start_unless_installing_in(&slot, INSTALLING_REFUSAL, || { started = true; Ok(()) });
            assert_eq!(r, Err(INSTALLING_REFUSAL.to_string()), "phase {phase}");
            assert!(!started, "phase {phase}: the conversion must not start");
        }
        for phase in ["idle", "error"] {
            let slot = slot_in(phase, None);
            assert_eq!(start_unless_installing_in(&slot, INSTALLING_REFUSAL, || Ok(7)), Ok(7), "phase {phase}");
            // The conversion's own refusal comes back as it was.
            let r: Result<(), String> = start_unless_installing_in(&slot, INSTALLING_REFUSAL, || Err("busy".into()));
            assert_eq!(r, Err("busy".to_string()));
        }
    }

    /// An install and a conversion asked for at the same moment: exactly one
    /// of them gets going, never both (the conversion marks itself running
    /// under the same lock the install checks it under).
    #[test]
    fn install_and_conversion_never_both_start() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};
        for _ in 0..2000 {
            let slot = Arc::new(slot_in("idle", None));
            let converting = Arc::new(AtomicBool::new(false));
            let go = Arc::new(Barrier::new(2));
            let install = {
                let (slot, converting, go) = (slot.clone(), converting.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    match InstallClaim::try_claim(&slot, || converting.load(Ordering::SeqCst)) {
                        Ok(c) => {
                            c.hand_off();
                            true
                        }
                        Err(_) => false,
                    }
                })
            };
            let convert = {
                let (slot, converting, go) = (slot.clone(), converting.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    start_unless_installing_in(&slot, INSTALLING_REFUSAL, || {
                        converting.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .is_ok()
                })
            };
            let (installed, converted) = (install.join().unwrap(), convert.join().unwrap());
            assert!(!(installed && converted), "an install and a conversion both started");
            assert!(installed || converted, "one of them must start");
        }
    }

    // ── --after-update arg parsing ────────────────────────────────────────

    #[test]
    fn after_update_arg_parse() {
        // Simulate what handle_after_update does with args.
        let args = vec![
            "aura-engine.exe".to_string(),
            "--after-update".to_string(),
            "12345".to_string(),
        ];
        let pos = args.iter().position(|a| a == "--after-update");
        assert!(pos.is_some());
        let pid: Option<u32> = args.get(pos.unwrap() + 1).and_then(|s| s.parse().ok());
        assert_eq!(pid, Some(12345));
    }

    #[test]
    fn after_update_arg_missing_pid() {
        let args = vec![
            "aura-engine.exe".to_string(),
            "--after-update".to_string(),
            // no pid follows
        ];
        let pos = args.iter().position(|a| a == "--after-update").unwrap();
        let pid: Option<u32> = args.get(pos + 1).and_then(|s| s.parse().ok());
        assert!(pid.is_none());
    }

    #[test]
    fn after_update_arg_absent() {
        let args = vec!["aura-engine.exe".to_string()];
        let pos = args.iter().position(|a| a == "--after-update");
        assert!(pos.is_none());
    }

    // ── what the extraction takes from a release zip ─────────────────────

    /// A zip at `path` with these (name, contents) entries; a name ending in
    /// '/' is a directory entry.
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use zip::write::FileOptions;
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(name.trim_end_matches('/'), FileOptions::default()).unwrap();
            } else {
                w.start_file(*name, FileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
    }

    /// The bare app's zip as CI makes it: the app's folder with the exe and
    /// its texts, and the filters' folder nested in it with its note. The
    /// note is skipped (not allowed, at any depth); every allowed file comes
    /// out.
    #[test]
    fn a_nested_entry_that_is_not_allowed_is_skipped() {
        let dir = TempDir::new().unwrap();
        let zip = dir.path().join("a.zip");
        let top = "aura-engine-v9.9.9-windows-x64";
        write_zip(&zip, &[
            (&format!("{top}/"), b""),
            (&format!("{top}/fir-optimizer/"), b""),
            (&format!("{top}/fir-optimizer/output/"), b""),
            (&format!("{top}/fir-optimizer/output/PUT-FILTERS-HERE.txt"), b"place filters here"),
            (&format!("{top}/aura-engine.exe"), b"MZ new exe"),
            (&format!("{top}/README.txt"), b"readme"),
            (&format!("{top}/LICENSE.txt"), b"license"),
            (&format!("{top}/THIRD-PARTY-NOTICES.txt"), b"notices"),
        ]);
        let dl = dir.path().join("dl");
        std::fs::create_dir_all(&dl).unwrap();
        let exe = dir.path().join("aura-engine.exe.new");
        extract_new_exe(&zip, &dl, &exe).expect("the update goes on");
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ new exe");
        assert_eq!(std::fs::read(dl.join("README.txt")).unwrap(), b"readme");
        assert_eq!(std::fs::read(dl.join("LICENSE.txt")).unwrap(), b"license");
        assert_eq!(std::fs::read(dl.join("THIRD-PARTY-NOTICES.txt")).unwrap(), b"notices");
        assert!(!dl.join("PUT-FILTERS-HERE.txt").exists());
        assert!(!dl.join("fir-optimizer").exists());
    }

    /// The app zip from 1.6.0 on, with the driver in its `asio\` folder: the
    /// driver's own files come out into dl_dir\asio and go beside the exe;
    /// anything else in `asio\`, deeper, or with a traversal is skipped. An
    /// updater before 1.6.0 (only ALLOWED_FILES, by name) skips the whole
    /// folder and goes on: no driver file is named like one of its files.
    #[test]
    fn the_drivers_files_come_out_of_the_asio_folder_and_go_beside_the_exe() {
        let dir = TempDir::new().unwrap();
        let zip = dir.path().join("a.zip");
        let top = "aura-engine-v9.9.9-windows-x64";
        let mut entries: Vec<(String, Vec<u8>)> = vec![
            (format!("{top}/aura-engine.exe"), b"MZ new exe".to_vec()),
            (format!("{top}/LICENSE.txt"), b"license".to_vec()),
            (format!("{top}/asio/"), vec![]),
            (format!("{top}/asio/evil.dll"), b"no".to_vec()),
            (format!("{top}/asio/sub/{}", ASIO_FILES[0]), b"no".to_vec()),
            (format!("../asio/{}", ASIO_FILES[1]), b"no".to_vec()),
        ];
        for f in ASIO_FILES {
            entries.push((format!("{top}/asio/{f}"), format!("driver {f}").into_bytes()));
        }
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
        write_zip(&zip, &refs);
        for f in ASIO_FILES {
            assert!(!ALLOWED_FILES.contains(f), "{f} would stop an older updater");
        }

        let dl = dir.path().join("dl");
        std::fs::create_dir_all(&dl).unwrap();
        let exe = dir.path().join("aura-engine.exe.new");
        extract_new_exe(&zip, &dl, &exe).expect("the update goes on");
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ new exe");
        let got: Vec<String> = std::fs::read_dir(dl.join("asio")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(got.len(), ASIO_FILES.len(), "{got:?}");

        let beside = dir.path().join("app").join("asio");
        std::fs::create_dir_all(&beside).unwrap();
        std::fs::write(beside.join(ASIO_FILES[0]), b"the last release's").unwrap();
        assert_eq!(place_asio_files(&dl.join("asio"), &beside), (ASIO_FILES.len(), 0));
        for f in ASIO_FILES {
            assert_eq!(std::fs::read(beside.join(f)).unwrap(), format!("driver {f}").into_bytes());
            assert!(!beside.join(format!("{f}.new")).exists());
        }
        assert_eq!(place_asio_files(&dir.path().join("none"), &beside), (0, 0), "a zip without the driver places nothing");
    }

    /// A local server answering every request with `status` and `body`.
    fn serve_bytes(status: u16, body: Vec<u8>) -> String {
        use std::io::{BufRead, BufReader};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                // status 0: a 200 that does not say its length (the body ends with the connection)
                let _ = if status == 0 {
                    write!(s, "HTTP/1.1 200 X\r\nConnection: close\r\n\r\n")
                } else {
                    write!(s, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                };
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}/v9.9.9/aura-player-asio-v9.9.9-windows.zip")
    }

    fn sha_hex(d: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The driver asset as release.yml makes it.
    fn driver_asset(dir: &Path, dll64: &[u8], dll32: &[u8]) -> Vec<u8> {
        let zip = dir.join("asset.zip");
        let top = "aura-player-asio-v9.9.9-windows";
        write_zip(&zip, &[
            (&format!("{top}/"), b""),
            (&format!("{top}/{}", ASIO_FILES[0]), dll64),
            (&format!("{top}/{}", ASIO_FILES[1]), dll32),
            (&format!("{top}/LICENSE-GPL-3.0.txt"), b"gpl"),
            (&format!("{top}/README-ASIO.txt"), b"readme"),
            (&format!("{top}/sub/{}", ASIO_FILES[0]), b"deeper, not taken"),
            (&format!("{top}/evil.exe"), b"not taken"),
        ]);
        std::fs::read(zip).unwrap()
    }

    /// This build's driver, fetched: written into the first folder that
    /// takes it (an unwritable one is passed over), replacing what was
    /// there; nothing is written when a DLL is not the build's own or
    /// missing, nor when the download fails.
    #[tokio::test]
    async fn the_driver_is_fetched_only_as_this_builds_own() {
        let dir = TempDir::new().unwrap();
        let (d64, d32) = (b"MZ sixty-four".as_slice(), b"MZ thirty-two".as_slice());
        let (h64, h32) = (sha_hex(d64), sha_hex(d32));
        let url = serve_bytes(200, driver_asset(dir.path(), d64, d32));

        let blocked = dir.path().join("a-file");
        std::fs::write(&blocked, b"x").unwrap();
        let dest = dir.path().join("data").join("asio");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join(ASIO_FILES[0]), b"the last release's").unwrap();
        let got = fetch_asio_driver(&url, (&h64, &h32), &[blocked.join("asio"), dest.clone()]).await;
        assert_eq!(got, Ok(dest.clone()));
        assert_eq!(std::fs::read(dest.join(ASIO_FILES[0])).unwrap(), d64);
        assert_eq!(std::fs::read(dest.join(ASIO_FILES[1])).unwrap(), d32);
        assert_eq!(std::fs::read(dest.join("LICENSE-GPL-3.0.txt")).unwrap(), b"gpl");
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), ASIO_FILES.len(), "only the driver's files, no .new left");

        let other = dir.path().join("other");
        let wrong = fetch_asio_driver(&url, (&h64, &sha_hex(b"another build")), &[other.clone()]).await;
        assert!(matches!(wrong, Err(AsioFetchError::Mismatch(_))), "{wrong:?}");
        assert!(!other.exists(), "nothing written");

        let half = serve_bytes(200, {
            let z = dir.path().join("half.zip");
            write_zip(&z, &[(&format!("x/{}", ASIO_FILES[0]), d64)]);
            std::fs::read(z).unwrap()
        });
        assert!(matches!(fetch_asio_driver(&half, (&h64, &h32), &[other.clone()]).await, Err(AsioFetchError::Mismatch(_))));
        let gone = serve_bytes(404, vec![]);
        assert!(matches!(fetch_asio_driver(&gone, (&h64, &h32), &[other.clone()]).await, Err(AsioFetchError::Network(_))));
        assert!(!other.exists());
        let nowhere = fetch_asio_driver(&url, (&h64, &h32), &[blocked.join("asio")]).await;
        assert!(matches!(nowhere, Err(AsioFetchError::Write(_))), "{nowhere:?}");
    }

    /// No connection at all (nothing listens there): a network error, and
    /// nothing written.
    #[tokio::test]
    async fn an_unreachable_host_is_a_network_error() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("asio");
        let r = fetch_asio_driver("http://127.0.0.1:1/aura-player-asio-v1.6.0-windows.zip", ("a", "b"), &[out.clone()]).await;
        assert!(matches!(&r, Err(AsioFetchError::Network(_))), "{r:?}");
        assert!(!out.exists());
    }

    /// The published 1.6.0 driver asset, fetched from GitHub as a 1.5.x
    /// user's Install fetches it, and checked with 1.6.0's own hashes (its
    /// CI run's AURA_ASIO_SHA256_*): written into a temp folder only,
    /// nothing installed or registered. Another hash takes nothing. Needs
    /// the network:  cargo test real_published -- --ignored
    #[tokio::test]
    #[ignore]
    async fn the_real_published_1_6_0_driver_is_fetched_and_checked() {
        const H64: &str = "3b0b870d0a4fa16f2d94e2aa553ff444ab96210ad73ec348fae3185dfb474d10";
        const H32: &str = "599560b68154be88565ff2424fc23769b898b0a4b6f19bcd06a272352a822732";
        let url = asio_asset_url("1.6.0");
        assert_eq!(url, "https://github.com/ToxaDev/aura-engine/releases/download/v1.6.0/aura-player-asio-v1.6.0-windows.zip");
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("asio");
        let got = fetch_asio_driver(&url, (H64, H32), &[out.clone()]).await;
        assert_eq!(got, Ok(out.clone()));
        assert_eq!(sha256_file(&out.join(ASIO_FILES[0])).unwrap(), H64);
        assert_eq!(sha256_file(&out.join(ASIO_FILES[1])).unwrap(), H32);
        let mut names: Vec<String> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        let mut want: Vec<String> = ASIO_FILES.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(names, want, "the driver's four files, no .new left");

        let other = dir.path().join("other");
        let wrong = fetch_asio_driver(&url, (H64, &H32.replace('5', "6")), &[other.clone()]).await;
        assert!(matches!(&wrong, Err(AsioFetchError::Mismatch(m)) if m.contains(ASIO_FILES[1])), "{wrong:?}");
        assert!(!other.exists(), "nothing written");
    }

    /// An asset bigger than the limit is not taken, whether its header says
    /// so or it only arrives so; nothing is written.
    #[tokio::test]
    async fn an_asset_over_the_limit_is_refused_as_it_arrives() {
        let dir = TempDir::new().unwrap();
        let big = vec![0u8; ASIO_ASSET_MAX_BYTES as usize + 1];
        let out = dir.path().join("asio");
        for url in [serve_bytes(200, big.clone()), serve_bytes(0, big)] {
            let r = fetch_asio_driver(&url, ("a", "b"), &[out.clone()]).await;
            assert!(matches!(&r, Err(AsioFetchError::Network(m)) if m.contains("too big")), "{r:?}");
        }
        assert!(!out.exists());
    }

    /// The driver's files never stop an update: when dl_dir\asio cannot be
    /// made the exe and its texts still come out. What an earlier update left
    /// in dl_dir\asio is cleared first.
    #[test]
    fn the_drivers_files_failing_never_stop_the_exes_update() {
        let dir = TempDir::new().unwrap();
        let zip = dir.path().join("a.zip");
        let top = "aura-engine-v9.9.9-windows-x64";
        write_zip(&zip, &[
            (&format!("{top}/aura-engine.exe"), b"MZ new exe"),
            (&format!("{top}/asio/{}", ASIO_FILES[0]), b"dll"),
            (&format!("{top}/README.txt"), b"readme"),
        ]);
        let dl = dir.path().join("dl");
        std::fs::create_dir_all(dl.join("asio")).unwrap();
        std::fs::write(dl.join("asio").join("stale.dll"), b"an earlier update's").unwrap();
        let exe = dir.path().join("aura-engine.exe.new");
        extract_new_exe(&zip, &dl, &exe).unwrap();
        assert!(!dl.join("asio").join("stale.dll").exists(), "cleared first");
        assert_eq!(std::fs::read(dl.join("asio").join(ASIO_FILES[0])).unwrap(), b"dll");

        // dl_dir\asio cannot be made: a file has its name.
        let dl2 = dir.path().join("dl2");
        std::fs::create_dir_all(&dl2).unwrap();
        std::fs::write(dl2.join("asio"), b"in the way").unwrap();
        let exe2 = dir.path().join("second.exe.new");
        extract_new_exe(&zip, &dl2, &exe2).expect("the exe's update goes on");
        assert_eq!(std::fs::read(&exe2).unwrap(), b"MZ new exe");
        assert_eq!(std::fs::read(dl2.join("README.txt")).unwrap(), b"readme");
    }

    /// Placing beside the exe: a DLL a player holds (mapped as an image) is
    /// moved aside and replaced; a file that cannot be placed does not stop
    /// the others; what was moved aside goes the next time once let go of.
    #[cfg(windows)]
    #[test]
    fn placing_moves_a_held_dll_aside_and_tries_every_file() {
        use winapi::um::libloaderapi::{FreeLibrary, LoadLibraryExW, LOAD_LIBRARY_AS_IMAGE_RESOURCE};

        let dir = TempDir::new().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        for f in ASIO_FILES {
            std::fs::write(from.join(f), format!("new {f}")).unwrap();
        }
        let held = to.join(ASIO_FILES[0]);
        let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("version.dll");
        std::fs::copy(&sys, &held).unwrap();
        let wide: Vec<u16> = held.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        // SAFETY: a null-terminated path; mapped as an image resource only, no code runs.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_AS_IMAGE_RESOURCE) };
        assert!(!module.is_null());
        // The second file cannot be written: a folder has its .new name.
        std::fs::create_dir_all(to.join(format!("{}.new", ASIO_FILES[1]))).unwrap();

        assert_eq!(place_asio_files(&from, &to), (ASIO_FILES.len() - 1, 1));
        assert_eq!(std::fs::read(&held).unwrap(), format!("new {}", ASIO_FILES[0]).into_bytes());
        assert_eq!(std::fs::read(to.join(ASIO_FILES[2])).unwrap(), format!("new {}", ASIO_FILES[2]).into_bytes(), "the files after the failed one placed");
        let olds = || std::fs::read_dir(&to).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".old")).count();
        assert_eq!(olds(), 1, "the held one moved aside");

        // SAFETY: the module loaded above.
        unsafe { FreeLibrary(module) };
        std::fs::remove_dir(to.join(format!("{}.new", ASIO_FILES[1]))).unwrap();
        assert_eq!(place_asio_files(&from, &to), (ASIO_FILES.len(), 0));
        assert_eq!(olds(), 0, "let go of, it goes");
    }

    #[test]
    fn only_the_drivers_own_names_directly_in_the_assets_folder_are_taken() {
        assert_eq!(asio_asset_entry(&format!("aura-player-asio-v1.6.0-windows/{}", ASIO_FILES[0])), Some(ASIO_FILES[0]));
        for bad in [
            format!("../{}", ASIO_FILES[0]),
            format!("/{}", ASIO_FILES[0]),
            format!("C:/{}", ASIO_FILES[0]),
            format!("a\\b/{}", ASIO_FILES[0]),
            ASIO_FILES[0].to_string(),
            format!("a/b/{}", ASIO_FILES[0]),
            "a/aura-engine.exe".to_string(),
        ] {
            assert_eq!(asio_asset_entry(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_driver_asset_is_named_from_the_version_as_release_yml_names_it() {
        assert_eq!(
            asio_asset_url("1.6.0"),
            "https://github.com/ToxaDev/aura-engine/releases/download/v1.6.0/aura-player-asio-v1.6.0-windows.zip"
        );
    }

    #[test]
    fn only_the_drivers_own_names_directly_in_asio_are_its_files() {
        let top = "aura-engine-v9.9.9-windows-x64";
        assert_eq!(asio_zip_entry(&format!("{top}/asio/{}", ASIO_FILES[1])), Some(ASIO_FILES[1]));
        for bad in [
            format!("{top}/asio/aura-engine.exe"),
            format!("{top}/asio/{}/x", ASIO_FILES[0]),
            format!("{top}/ASIO2/{}", ASIO_FILES[0]),
            format!("asio/{}", ASIO_FILES[0]),
            format!("/asio/{}", ASIO_FILES[0]),
            format!("../asio/{}", ASIO_FILES[0]),
            format!("C:/asio/{}", ASIO_FILES[0]),
            format!("{top}/sub/asio/{}", ASIO_FILES[0]),
        ] {
            assert_eq!(asio_zip_entry(&bad), None, "{bad}");
        }
    }

    /// An allowed file anywhere but `<folder>/<file>` stops the update, as
    /// before: nested, with a traversal, or absolute.
    #[test]
    fn an_allowed_file_out_of_place_stops_the_update() {
        for bad in [
            "aura-engine-v9.9.9-windows-x64/sub/aura-engine.exe",
            "aura-engine-v9.9.9-windows-x64/../aura-engine.exe",
            "C:/Windows/aura-engine.exe",
            "/aura-engine.exe",
            "aura-engine.exe",
            "a/b/README.txt",
            "a/b/THIRD-PARTY-NOTICES.txt",
        ] {
            let dir = TempDir::new().unwrap();
            let zip = dir.path().join("a.zip");
            write_zip(&zip, &[(bad, b"x"), ("aura-engine-v9.9.9-windows-x64/aura-engine.exe", b"MZ")]);
            let exe = dir.path().join("aura-engine.exe.new");
            assert!(extract_new_exe(&zip, dir.path(), &exe).is_err(), "{bad} was taken");
        }
    }

    /// The app zip and its signature are the release's own, by name: a
    /// bundle listed ahead of them (and a signature of one, should a bundle
    /// ever be signed) is never taken; a release of bundles alone is not an
    /// app release.
    #[test]
    fn the_app_zip_is_taken_by_its_name_not_a_bundle() {
        let tag = "v1.5.0";
        let assets = [
            "aura-engine-v1.5.0-bundle-1M-windows-x64.zip",
            "aura-engine-v1.5.0-bundle-1M-windows-x64.zip.sig",
            "aura-engine-v1.5.0-bundle-30M-windows-x64.zip",
            "aura-engine-v1.4.9-windows-x64.zip",
            "aura-engine-v1.5.0-windows-x64.zip.sha256",
            "aura-engine-v1.5.0-windows-x64.zip",
            "aura-engine-v1.5.0-windows-x64.zip.sig",
        ]
        .map(String::from);
        let r = release_with(tag, false, &assets);
        let list = r["assets"].as_array().unwrap();
        assert_eq!(app_asset(list, tag, APP_ZIP_SUFFIX).unwrap()["name"], "aura-engine-v1.5.0-windows-x64.zip");
        assert_eq!(app_asset(list, tag, APP_SIG_SUFFIX).unwrap()["name"], "aura-engine-v1.5.0-windows-x64.zip.sig");
        assert!(is_app_release(&r));
        let bundles_only = release_with(tag, false, &assets[..3]);
        assert!(!is_app_release(&bundles_only));
        let b = bundles_only["assets"].as_array().unwrap();
        assert!(app_asset(b, tag, APP_ZIP_SUFFIX).is_none());
        assert!(app_asset(b, tag, APP_SIG_SUFFIX).is_none());
    }

    /// Through the check: bundles first in the release's assets, the offer
    /// is the app zip and its signature.
    #[tokio::test]
    async fn the_offer_is_the_app_zip_with_bundles_listed_first() {
        let assets = [
            "aura-engine-v99.0.0-bundle-1M-windows-x64.zip",
            "aura-engine-v99.0.0-bundle-30M-windows-x64.zip",
            "aura-engine-v99.0.0-windows-x64.zip",
            "aura-engine-v99.0.0-windows-x64.zip.sig",
        ]
        .map(String::from);
        let (feed, _) = serve_feed(release_with("v99.0.0", false, &assets), serde_json::json!([]));
        let client = build_api_client("test").unwrap();
        let c = fetch_release(&client, &feed).await.unwrap();
        assert!(c.available);
        assert_eq!(c.asset_name, "aura-engine-v99.0.0-windows-x64.zip");
        assert!(c.asset_url.ends_with("/aura-engine-v99.0.0-windows-x64.zip"));
        assert!(c.sig_url.ends_with("/aura-engine-v99.0.0-windows-x64.zip.sig"));
    }

    // ── interface update ──────────────────────────────────────────────────

    /// Every asset name a release of `v` has had or may have, the interface
    /// packs and their notes among them.
    fn every_asset_name(v: &str) -> Vec<String> {
        vec![
            format!("aura-engine-v{v}{APP_ZIP_SUFFIX}"),
            format!("aura-engine-v{v}{APP_SIG_SUFFIX}"),
            format!("aura-engine-v{v}-windows-x64.zip.sha256"),
            format!("aura-engine-v{v}-bundle-1M-windows-x64.zip"),
            format!("aura-engine-v{v}-bundle-30M-windows-x64.zip"),
            format!("aura-player-asio-v{v}-windows.zip"),
            format!("aura-engine-ui-v{v}-p1.zip"),
            format!("aura-engine-ui-v{v}-p1.zip.sig"),
            format!("aura-engine-ui-v{v}-p1.md"),
            format!("aura-engine-ui-v{v}-p12.zip"),
            format!("aura-engine-ui-v{v}-p12.zip.sig"),
        ]
    }

    /// The updaters already out there never take an interface pack for the
    /// app: 1.5.0 took the first `aura-engine-*-windows-x64.zip(.sig)`, 1.5.1
    /// to 1.6.1 the exact `aura-engine-v<version>-windows-x64.zip(.sig)` (the
    /// same `app_asset` as here), and the ASIO driver is fetched by its exact
    /// URL. With the packs listed first, each still finds the app's own.
    #[test]
    fn old_updaters_never_take_an_interface_pack() {
        let v = "1.6.2";
        let mut names = every_asset_name(v);
        names.rotate_left(6); // the packs first
        assert!(names[0].starts_with("aura-engine-ui-"));
        let release = release_with("v1.6.2", false, &names);
        let assets = release["assets"].as_array().unwrap();
        let ui = |n: &str| n.starts_with("aura-engine-ui-");
        // 1.5.0 (bcbe5ac): starts_with("aura-engine-") && ends_with(suffix), the first one
        for suffix in [APP_ZIP_SUFFIX, APP_SIG_SUFFIX] {
            let took = assets
                .iter()
                .filter_map(|a| a["name"].as_str())
                .find(|n| n.starts_with("aura-engine-") && n.ends_with(suffix))
                .unwrap();
            assert!(!ui(took), "1.5.0 would take {took}");
            assert_eq!(took, format!("aura-engine-v{v}{suffix}"));
        }
        // 1.5.1, 1.6.0, 1.6.1: app_asset unchanged since 1.5.1
        for suffix in [APP_ZIP_SUFFIX, APP_SIG_SUFFIX] {
            assert_eq!(app_asset(assets, "v1.6.2", suffix).unwrap()["name"], format!("aura-engine-v{v}{suffix}"));
        }
        assert!(is_app_release(&release));
        // without the app's own assets, the packs make no app release
        let only_ui: Vec<String> = every_asset_name(v).into_iter().filter(|n| ui(n)).collect();
        let r = release_with("v1.6.2", false, &only_ui);
        assert!(!is_app_release(&r));
        assert!(app_asset(r["assets"].as_array().unwrap(), "v1.6.2", APP_ZIP_SUFFIX).is_none());
        // the ASIO driver: by its exact name
        assert!(!only_ui.iter().any(|n| asio_asset_url(v).ends_with(&format!("/{n}"))));
        // and the interface packs are never an app zip or the driver's
        for n in &only_ui {
            assert!(!n.ends_with(APP_ZIP_SUFFIX) && !n.ends_with(APP_SIG_SUFFIX), "{n}");
        }
    }

    #[test]
    fn the_offer_is_the_highest_pack_above_the_installed_one() {
        let v = "1.6.2";
        let mut names = every_asset_name(v);
        // a newer one without its .sig, another version's, a .sig alone
        names.extend([
            format!("aura-engine-ui-v{v}-p20.zip"),
            "aura-engine-ui-v1.6.3-p30.zip".to_string(),
            "aura-engine-ui-v1.6.3-p30.zip.sig".to_string(),
            format!("aura-engine-ui-v{v}-p40.zip.sig"),
        ]);
        let r = release_with("v1.6.2", false, &names);
        let o = pick_ui_offer(&r, v, 0).unwrap();
        assert_eq!(o.patch, 12);
        assert_eq!(o.asset_name, "aura-engine-ui-v1.6.2-p12.zip");
        assert!(o.asset_url.ends_with("/aura-engine-ui-v1.6.2-p12.zip"));
        assert!(o.sig_url.ends_with("/aura-engine-ui-v1.6.2-p12.zip.sig"));
        assert_eq!(o.notes_url, None);
        assert_eq!(pick_ui_offer(&r, v, 11).unwrap().patch, 12);
        // equal or lower: nothing (anti-rollback)
        assert!(pick_ui_offer(&r, v, 12).is_none());
        assert!(pick_ui_offer(&r, v, 13).is_none());
        // p1 has notes
        let only_p1 = release_with("v1.6.2", false, &every_asset_name(v)[..9]);
        let o = pick_ui_offer(&only_p1, v, 0).unwrap();
        assert_eq!(o.patch, 1);
        assert!(o.notes_url.unwrap().ends_with("/aura-engine-ui-v1.6.2-p1.md"));
        // another version's release
        assert!(pick_ui_offer(&r, "1.6.1", 0).is_none());
        assert!(pick_ui_offer(&app_release("v1.6.2"), v, 0).is_none());
    }

    #[test]
    fn the_release_of_a_tag_is_asked_at_its_own_route() {
        assert_eq!(
            release_by_tag_url("http://127.0.0.1:9/repos/x/releases/latest", "v1.6.2"),
            "http://127.0.0.1:9/repos/x/releases/tags/v1.6.2"
        );
    }

    #[test]
    fn long_notes_are_cut_on_a_character() {
        assert_eq!(cut_notes("  a  "), "a");
        let long = "\u{44f}".repeat(UI_NOTES_MAX_BYTES);
        let cut = cut_notes(&long);
        assert!(cut.len() <= UI_NOTES_MAX_BYTES + "…".len());
        assert!(cut.ends_with('…'));
    }

    /// A local server with fixed answers per path; `routes` gets the base
    /// URL so the release can point at the server itself.
    fn serve_routes(routes: impl FnOnce(&str) -> std::collections::HashMap<String, Vec<u8>>) -> String {
        use std::io::{BufRead, BufReader};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let routes = routes(&base);
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                let _ = r.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                match routes.get(&path) {
                    Some(body) => {
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = s.write_all(body);
                    }
                    None => {
                        let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        base
    }

    /// The whole way on a local feed with a test key: no newer app → the
    /// running version's release offers UI 2 (UI 1 serves now) → download,
    /// verify, placed in the ui folder → the next start serves its index.html.
    #[tokio::test]
    async fn an_interface_update_is_found_installed_and_served_at_the_next_start() {
        use crate::ui_pack::tests::{key, signed_pack};
        // the running version: its app release is not newer, so its packs are asked
        let v = env!("CARGO_PKG_VERSION");
        let k = key();
        let pk = k.verifying_key().to_bytes();
        let (name, bytes, sig) = signed_pack(&k, v, 2, b"<p>interface 2</p>");
        let base = serve_routes(|base| {
            let asset = |n: &str| serde_json::json!({ "name": n, "browser_download_url": format!("{base}/dl/{n}") });
            let latest = serde_json::json!({
                "tag_name": format!("v{v}"), "body": "", "html_url": "",
                "assets": [asset(&format!("aura-engine-v{v}{APP_ZIP_SUFFIX}")), asset(&format!("aura-engine-v{v}{APP_SIG_SUFFIX}"))],
            });
            let mut this = latest.clone();
            this["assets"].as_array_mut().unwrap().extend([
                asset(&format!("aura-engine-ui-v{v}-p1.zip")),
                asset(&format!("aura-engine-ui-v{v}-p1.zip.sig")),
                asset(&name),
                asset(&format!("{name}.sig")),
                asset(&format!("aura-engine-ui-v{v}-p2.md")),
            ]);
            [
                ("/repos/x/releases/latest".to_string(), latest.to_string().into_bytes()),
                (format!("/repos/x/releases/tags/v{v}"), this.to_string().into_bytes()),
                (format!("/dl/{name}"), bytes.clone()),
                (format!("/dl/{name}.sig"), sig.clone().into_bytes()),
                (format!("/dl/aura-engine-ui-v{v}-p2.md"), "- Fixed: <b>the</b> scale.\n".as_bytes().to_vec()),
            ]
            .into_iter()
            .collect()
        });
        let feed = format!("{base}/repos/x/releases/latest");
        let client = build_api_client("test").unwrap();

        // The app release is this version: no app update, so the packs are asked.
        let c = fetch_release(&client, &feed).await.unwrap();
        assert!(!c.available);
        let offer = fetch_ui_offer(&client, &feed, v, 1).await.unwrap().unwrap();
        assert_eq!(offer.patch, 2);
        assert_eq!(offer.notes, "- Fixed: <b>the</b> scale.");
        assert!(fetch_ui_offer(&client, &feed, v, 2).await.unwrap().is_none());
        // a version without a release: nothing, quietly
        assert!(fetch_ui_offer(&client, &feed, "0.0.1", 0).await.unwrap().is_none());

        let d = tempfile::tempdir().unwrap();
        let ui = d.path().join(crate::ui_pack::UI_DIR);
        // a wrong key places nothing
        let wrong = key().verifying_key().to_bytes();
        assert!(download_ui_pack(&offer, v, &wrong, &ui, "test").await.is_err());
        assert!(!ui.exists());
        // another version refuses it too
        assert!(download_ui_pack(&offer, "0.0.1", &pk, &ui, "test").await.is_err());

        download_ui_pack(&offer, v, &pk, &ui, "test").await.unwrap();
        let p = crate::ui_pack::load_from_dir(&ui, v, Some(&pk)).unwrap();
        assert_eq!(p.patch, 2);
        assert_eq!(p.files["/index.html"], b"<p>interface 2</p>");
    }
}
