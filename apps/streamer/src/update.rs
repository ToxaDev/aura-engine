//! The streamer's own updates, asked for by the web remote's Update button.
//!
//! The streamer ships with the app: each release carries
//! `aura-streamer-v<version><suffix>.zip` for every system and its `.sig`,
//! signed with the app's key over the same message (`aura_core::update_sig`).
//! The zip holds the binary (`aura-streamer` or `aura-streamer.exe`; the
//! remote's files are inside it) and texts nobody installs from here.
//!
//! * `check` — the newest release carrying this system's zip, and whether it is
//!   newer than this copy. Kept a while: a page opening does not ask GitHub.
//! * `install` — in the background: download, check the signature, put the new
//!   binary in this one's place (the old one moved aside), start it again.
//! * `status_json` — what the page polls while that goes on.
//!
//! Only a release build (`AURA_RELEASE_BUILD=1`) asks GitHub. Any other build is
//! off unless `AURA_UPDATE_TEST_FEED` and `AURA_UPDATE_TEST_PUBKEY` are set, the
//! same test hook as the app's (ignored in a release build).
//!
//! The binary is replaced only where this copy may write. A copy in a folder it
//! cannot write to (one installed for all users) is told to update from the
//! release's page.
//!
//! Coming back: under a supervisor (a systemd unit, a launchd agent: they set
//! `AURA_SUPERVISED=1`, systemd also `INVOCATION_ID`) the streamer just quits and
//! is started again. Otherwise it starts the new binary itself with
//! `--after-update <pid>` and its own arguments, then quits; the new one waits
//! for the old one to let go of the port.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use aura_core::update_sig::{build_update_message, decode_pubkey_hex, semver_gt, verify_ed25519, PROD_PUBKEY_HEX};
use serde_json::{json, Value};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The releases, newest first: the app's and the streamer's are the same ones.
const GITHUB_RELEASES_LIST_URL: &str = "https://api.github.com/repos/ToxaDev/aura-engine/releases?per_page=30";

const ASSET_PREFIX: &str = "aura-streamer-v";
#[cfg(windows)]
const ASSET_SUFFIX: &str = "-windows-x64.zip";
#[cfg(target_os = "linux")]
const ASSET_SUFFIX: &str = "-linux-x86_64.zip";
#[cfg(target_os = "macos")]
const ASSET_SUFFIX: &str = "-macos-universal.zip";
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const ASSET_SUFFIX: &str = "-unsupported.zip";

/// The binary's name in the zip.
#[cfg(windows)]
const BIN: &str = "aura-streamer.exe";
#[cfg(not(windows))]
const BIN: &str = "aura-streamer";

/// The largest zip taken, and the largest binary taken out of it.
const MAX_ZIP_BYTES: u64 = 200 << 20;
const MAX_BIN_BYTES: u64 = 150 << 20;
/// A `.sig` is one line of base64.
const MAX_SIG_BYTES: u64 = 4 << 10;

/// How long a check is kept, and a failed one.
const KEEP_CHECK: Duration = Duration::from_secs(6 * 3600);
const KEEP_FAILED_CHECK: Duration = Duration::from_secs(10 * 60);

/// How long a download waits for the response headers, and for the next bytes.
const RESPONSE_WAIT: Duration = Duration::from_secs(30);
const DOWNLOAD_IDLE: Duration = Duration::from_secs(60);

/// What the page is given a moment to see before the streamer goes.
const RESTART_PAUSE: Duration = Duration::from_millis(1500);

/// How long a stop may take before the restart.
const STOP_WAIT: Duration = Duration::from_secs(3);

// ── the build's feed and key ───────────────────────────────────────────────

fn is_release_build() -> bool {
    option_env!("AURA_RELEASE_BUILD") == Some("1")
}

/// The releases list to read; `None` when updates are off in this build.
fn feed_url() -> Option<String> {
    if is_release_build() {
        return Some(GITHUB_RELEASES_LIST_URL.to_string());
    }
    // The app's test hook names `…/releases/latest`; the list is next to it.
    let feed = std::env::var("AURA_UPDATE_TEST_FEED").ok().filter(|f| !f.is_empty())?;
    Some(match feed.strip_suffix("/latest") {
        Some(base) => format!("{base}?per_page=30"),
        None => feed,
    })
}

fn active_pubkey() -> Option<[u8; 32]> {
    if is_release_build() {
        return decode_pubkey_hex(PROD_PUBKEY_HEX);
    }
    decode_pubkey_hex(&std::env::var("AURA_UPDATE_TEST_PUBKEY").ok()?)
}

fn user_agent() -> String {
    format!("AuraStreamer/{CURRENT}")
}

// ── state ──────────────────────────────────────────────────────────────────

/// A release that has this system's zip and its signature.
#[derive(Clone, Debug, PartialEq)]
struct Offer {
    version: String,
    tag: String,
    notes: String,
    url: String,
    asset_name: String,
    asset_url: String,
    sig_url: String,
}

#[derive(Clone)]
struct Checked {
    at: Instant,
    /// The newest release with this system's zip, newer or not.
    offer: Option<Offer>,
    release_notes: Vec<(String, String)>,
    error: Option<String>,
}

impl Checked {
    fn fresh(&self) -> bool {
        let keep = if self.error.is_some() { KEEP_FAILED_CHECK } else { KEEP_CHECK };
        self.at.elapsed() < keep
    }

    fn available(&self) -> bool {
        self.offer.as_ref().is_some_and(|o| semver_gt(&o.tag, CURRENT))
    }
}

static CHECKED: Mutex<Option<Checked>> = Mutex::new(None);

struct Progress {
    phase: &'static str,
    done: u64,
    total: u64,
    error: Option<String>,
}

static PROGRESS: Mutex<Progress> = Mutex::new(Progress { phase: "idle", done: 0, total: 0, error: None });

/// Held from the moment an install is asked for until it fails (a success ends
/// with the process).
static INSTALLING: AtomicBool = AtomicBool::new(false);

fn set_progress(phase: &'static str, done: u64, total: u64, error: Option<String>) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = Progress { phase, done, total, error };
    }
}

/// `{phase, done, total, error}` for `/aura/update/status`.
pub fn status_json() -> Vec<u8> {
    let v = PROGRESS
        .lock()
        .map(|p| json!({ "phase": p.phase, "done": p.done, "total": p.total, "error": p.error }))
        .unwrap_or_else(|_| json!({ "phase": "idle", "done": 0, "total": 0, "error": null }));
    v.to_string().into_bytes()
}

// ── the check ──────────────────────────────────────────────────────────────

/// How this copy takes an update: in its own place, or from the page.
fn install_kind() -> (&'static str, Option<&'static str>) {
    if exe_dir_writable() {
        ("self", None)
    } else {
        ("page", Some("This copy of Aura Streamer updates from the download page."))
    }
}

/// Whether a file can be made beside the running binary.
fn exe_dir_writable() -> bool {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else {
        return false;
    };
    let probe = dir.join(format!(".aura-streamer-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn off_value() -> Value {
    json!({
        "enabled": false, "available": false, "product": "streamer", "install": false,
        "install_kind": "page", "install_note": null, "current": CURRENT, "version": "", "tag": "",
        "notes": "", "release_notes": [], "url": "", "error": null,
    })
}

fn to_value(c: &Checked) -> Value {
    let available = c.available();
    let (kind, note) = install_kind();
    let o = c.offer.as_ref().filter(|_| available);
    json!({
        "enabled": true,
        "product": "streamer",
        "available": available,
        "install": available && kind == "self",
        "install_kind": kind,
        "install_note": note,
        "current": CURRENT,
        "version": o.map_or("", |o| &o.version),
        "tag": o.map_or("", |o| &o.tag),
        "notes": o.map_or("", |o| &o.notes),
        "release_notes": c.release_notes.iter().map(|(v, n)| json!({ "version": v, "notes": n })).collect::<Vec<_>>(),
        "url": o.map_or("", |o| &o.url),
        "error": c.error,
    })
}

/// The answer to `update_check`: never an error, the failure is in `error`.
/// Waits for GitHub: on a thread that may wait.
pub fn check() -> Value {
    run_blocking(check_async(false)).unwrap_or_else(off_value)
}

/// The check, from the cache while it is fresh (unless `force`).
async fn check_async(force: bool) -> Value {
    let Some(feed) = feed_url() else { return off_value() };
    if !force {
        if let Some(c) = CHECKED.lock().ok().and_then(|c| c.clone()).filter(Checked::fresh) {
            return to_value(&c);
        }
    }
    let checked = match fetch(&feed).await {
        Ok((offer, release_notes)) => Checked { at: Instant::now(), offer, release_notes, error: None },
        Err(e) => {
            aura_core::aelog!("[UPDATE] check failed (quiet): {e}");
            Checked { at: Instant::now(), offer: None, release_notes: vec![], error: Some(e) }
        }
    };
    let v = to_value(&checked);
    if let Ok(mut c) = CHECKED.lock() {
        *c = Some(checked);
    }
    v
}

/// The newest release with this system's zip, and the notes of every release
/// between this copy and it.
async fn fetch(feed: &str) -> Result<(Option<Offer>, Vec<(String, String)>), String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(15))
        .https_only(is_release_build())
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client.get(feed).send().await.map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("feed returned HTTP {}", resp.status()));
    }
    let releases: Vec<Value> = resp.json().await.map_err(|e| format!("JSON parse error: {e}"))?;
    let offer = newest_offer(&releases);
    if let Some(o) = &offer {
        if is_release_build() && !(o.asset_url.starts_with("https://") && o.sig_url.starts_with("https://")) {
            return Err("asset URL is not HTTPS".to_string());
        }
    }
    let notes = offer.as_ref().map(|o| notes_between(&releases, CURRENT, &o.tag)).unwrap_or_default();
    Ok((offer, notes))
}

fn published(r: &Value) -> bool {
    !r["draft"].as_bool().unwrap_or(false) && !r["prerelease"].as_bool().unwrap_or(false)
}

fn semver_tag(tag: &str) -> Option<&str> {
    let v = tag.strip_prefix('v')?;
    let ok = v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some(v)
}

/// This system's zip and its signature on a release, by their exact names.
fn offer_of(r: &Value) -> Option<Offer> {
    let tag = r["tag_name"].as_str()?;
    let version = semver_tag(tag)?;
    let name = format!("{ASSET_PREFIX}{version}{ASSET_SUFFIX}");
    let sig = format!("{name}.sig");
    let assets = r["assets"].as_array()?;
    let url_of = |n: &str| {
        assets.iter().find(|a| a["name"].as_str() == Some(n)).and_then(|a| a["browser_download_url"].as_str())
    };
    Some(Offer {
        version: version.to_string(),
        tag: tag.to_string(),
        notes: r["body"].as_str().unwrap_or("").to_string(),
        url: r["html_url"].as_str().unwrap_or("").to_string(),
        asset_url: url_of(&name)?.to_string(),
        sig_url: url_of(&sig)?.to_string(),
        asset_name: name,
    })
}

/// The newest published release that carries this system's zip.
fn newest_offer(releases: &[Value]) -> Option<Offer> {
    releases.iter().filter(|r| published(r)).filter_map(offer_of).fold(None, |best: Option<Offer>, o| match best {
        Some(b) if !semver_gt(&o.tag, &b.tag) => Some(b),
        _ => Some(o),
    })
}

/// `(version, notes)` of every published release newer than `current` and at
/// most `latest`, newest first.
fn notes_between(releases: &[Value], current: &str, latest: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = releases
        .iter()
        .filter(|r| published(r))
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let v = semver_tag(tag)?;
            (semver_gt(tag, current) && !semver_gt(tag, latest))
                .then(|| (v.to_string(), r["body"].as_str().unwrap_or("").to_string()))
        })
        .collect();
    out.sort_by(|a, b| {
        let (a, b) = (format!("v{}", a.0), format!("v{}", b.0));
        if semver_gt(&a, &b) {
            std::cmp::Ordering::Less
        } else if semver_gt(&b, &a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    out
}

// ── the daily check ────────────────────────────────────────────────────────

/// A minute after the start, then once a day: an update found is told to every
/// open page (`update:available`).
pub async fn keep_checking(emit: impl Fn(Value) + Send + 'static) {
    if feed_url().is_none() {
        return;
    }
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let v = check_async(true).await;
        if v["available"].as_bool() == Some(true) {
            aura_core::aelog!("[UPDATE] {} is out (this is {CURRENT})", v["version"].as_str().unwrap_or("?"));
            emit(v);
        }
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    }
}

/// The engine's filter download checks its list with this key, follows this
/// build's release flag and starts no download while an install runs.
pub fn lend_to_filter_download() {
    aura_core::filters_dl::set_hooks(aura_core::filters_dl::Hooks {
        pubkey: active_pubkey,
        release_build: is_release_build,
        unless_installing: |refusal, start| {
            if INSTALLING.load(Ordering::Acquire) { Err(refusal.to_string()) } else { start() }
        },
    });
}

// ── the install ────────────────────────────────────────────────────────────

/// The answer to `update_install`: starts the work and returns at once; the
/// page follows it on `/aura/update/status`.
pub fn install() -> Result<Value, String> {
    let Some(pubkey) = feed_url().and(active_pubkey()) else {
        return Err("Updates are off in this build of Aura Streamer.".to_string());
    };
    if aura_core::filters_dl::in_flight() {
        return Err("Filters are downloading — update after it finishes.".to_string());
    }
    if !exe_dir_writable() {
        return Err("This copy of Aura Streamer updates from the download page.".to_string());
    }
    let handle = tokio::runtime::Handle::try_current().map_err(|_| "no runtime to install on".to_string())?;
    if INSTALLING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        // Already going: the page follows that one.
        return Ok(Value::Null);
    }
    set_progress("checking", 0, 0, None);
    handle.spawn(async move {
        if let Err(e) = install_async(pubkey).await {
            aura_core::aelog!("[UPDATE] install failed: {e}");
            set_progress("error", 0, 0, Some(e));
            INSTALLING.store(false, Ordering::Release);
        }
    });
    Ok(Value::Null)
}

async fn install_async(pubkey: [u8; 32]) -> Result<(), String> {
    // The newest word on it: the offer may have moved since the page asked.
    check_async(true).await;
    let c = CHECKED.lock().ok().and_then(|c| c.clone()).ok_or("the check found nothing")?;
    if let Some(e) = &c.error {
        return Err(format!("the check failed: {e}"));
    }
    let offer = c.offer.clone().filter(|_| c.available()).ok_or("There is no newer version to install.")?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let dir = aura_core::app_dir::root()
        .ok_or("cannot locate the data folder")?
        .join("streamer")
        .join("update")
        .join(&offer.tag);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    aura_core::aelog!("[UPDATE] installing {} over {CURRENT}", offer.tag);

    // Download and check.
    set_progress("downloading", 0, 0, None);
    let zip_path = dir.join(&offer.asset_name);
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .https_only(is_release_build())
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    download(&client, &offer.asset_url, &zip_path, MAX_ZIP_BYTES, true).await?;
    let sig_path = dir.join(format!("{}.sig", offer.asset_name));
    download(&client, &offer.sig_url, &sig_path, MAX_SIG_BYTES, false).await?;
    set_progress("verifying", 0, 0, None);
    let sig = std::fs::read_to_string(&sig_path).map_err(|e| format!("cannot read the signature: {e}"))?;
    let sha = aura_core::checksum::sha256_file(&zip_path)?;
    verify_ed25519(&pubkey, build_update_message(&offer.tag, &offer.asset_name, &sha).as_bytes(), sig.trim())
        .map_err(|e| format!("signature check failed, nothing was changed: {e}"))?;
    aura_core::aelog!("[UPDATE] signature OK");

    // In this one's place.
    set_progress("installing", 0, 0, None);
    let new = sibling(&exe, ".new");
    let _ = std::fs::remove_file(&new);
    let placed = extract_binary(&zip_path, &new).and_then(|_| swap(&new, &exe, stamp_now()));
    let old = match placed {
        Ok(old) => old,
        Err(e) => {
            let _ = std::fs::remove_file(&new);
            return Err(e);
        }
    };

    // Quiet, then back.
    set_progress("restarting", 0, 0, None);
    let _ = tokio::task::spawn_blocking(|| aura_core::player::controller::get().stop_and_wait(STOP_WAIT)).await;
    if !supervised() {
        // Started on the way out (`restart`), once this one's sockets are
        // closed: on Windows a child takes a copy of them, and the port with it.
        let args = relaunch_args(std::env::args_os().skip(1).collect(), std::process::id());
        if let Ok(mut r) = RESTART.lock() {
            *r = Some(Restart { exe: exe.clone(), old, args });
        }
    }
    aura_core::aelog!("[UPDATE] {} is in place, restarting", offer.tag);
    tokio::time::sleep(RESTART_PAUSE).await;
    request_quit();
    Ok(())
}

/// `url` to `dest`, at most `cap` bytes; the progress counted when `counted`.
async fn download(client: &reqwest::Client, url: &str, dest: &Path, cap: u64, counted: bool) -> Result<(), String> {
    use futures_util::StreamExt;
    let resp = tokio::time::timeout(RESPONSE_WAIT, client.get(url).send())
        .await
        .map_err(|_| format!("no answer for {} s", RESPONSE_WAIT.as_secs()))?
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    if total > cap {
        return Err(format!("the download is too big ({total} bytes)"));
    }
    let mut file = std::fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut done = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::time::timeout(DOWNLOAD_IDLE, stream.next())
            .await
            .map_err(|_| format!("the server sent nothing for {} s", DOWNLOAD_IDLE.as_secs()))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("download error: {e}"))?;
        done += chunk.len() as u64;
        if done > cap {
            return Err("the download is too big".to_string());
        }
        file.write_all(&chunk).map_err(|e| format!("write error: {e}"))?;
        if counted {
            set_progress("downloading", done, total, None);
        }
    }
    file.sync_all().map_err(|e| format!("write error: {e}"))
}

/// Whether a zip entry is the binary: `BIN` at the top or in one top folder,
/// with no way out of it.
fn is_binary_entry(name: &str) -> bool {
    if name.contains('\\') || name.contains(':') || name.starts_with('/') {
        return false;
    }
    let parts: Vec<&str> = name.split('/').collect();
    let folder_ok = |f: &str| !f.is_empty() && f != "." && f != "..";
    match parts.as_slice() {
        [file] => *file == BIN,
        [folder, file] => folder_ok(folder) && *file == BIN,
        _ => false,
    }
}

/// The binary out of the zip into `dest` (nothing else is read), executable.
fn extract_binary(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("cannot open the zip: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip: {e}"))?;
    let mut index = None;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        if is_binary_entry(entry.name()) {
            if index.is_some() {
                return Err(format!("{BIN} is in the zip twice"));
            }
            index = Some(i);
        }
    }
    let i = index.ok_or_else(|| format!("{BIN} is not in the zip"))?;
    let mut entry = zip.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
    let mut out = std::fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut written = 0u64;
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = entry.read(&mut buf).map_err(|e| format!("cannot unpack {BIN}: {e}"))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > MAX_BIN_BYTES {
            return Err(format!("{BIN} in the zip is too big"));
        }
        out.write_all(&buf[..n]).map_err(|e| format!("write error: {e}"))?;
    }
    if written == 0 {
        return Err(format!("{BIN} in the zip is empty"));
    }
    out.sync_all().map_err(|e| format!("write error: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make {} executable: {e}", dest.display()))?;
    }
    Ok(())
}

/// `path` with `tail` added to its name.
fn sibling(path: &Path, tail: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(tail);
    PathBuf::from(s)
}

/// `new` in `exe`'s place; the running one moved aside as `<exe>.<stamp>.old`
/// (a running binary can be renamed, not overwritten, on Windows). Returns
/// where the old one went. On a failure `exe` is as it was.
fn swap(new: &Path, exe: &Path, stamp: u64) -> Result<PathBuf, String> {
    let old = sibling(exe, &format!(".{stamp}.old"));
    std::fs::rename(exe, &old).map_err(|e| format!("cannot move {} aside: {e}", exe.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("cannot put the new version in place: {e}"));
    }
    Ok(old)
}

fn stamp_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Started by systemd or launchd, which start it again once it quits.
fn supervised() -> bool {
    std::env::var_os("AURA_SUPERVISED").is_some_and(|v| v == "1") || std::env::var_os("INVOCATION_ID").is_some()
}

/// This run's arguments for the new one: an earlier `--after-update <pid>` out,
/// this one's in front.
fn relaunch_args(args: Vec<OsString>, pid: u32) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec!["--after-update".into(), pid.to_string().into()];
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--after-update" {
            it.next();
        } else {
            out.push(a);
        }
    }
    out
}

/// What starts the new version once this one has let go of everything.
struct Restart {
    exe: PathBuf,
    /// Where the running binary went: put back if the new one does not start.
    old: PathBuf,
    args: Vec<OsString>,
}

static RESTART: Mutex<Option<Restart>> = Mutex::new(None);

/// The last thing before the process ends, its sockets closed: the new version
/// started, when an update asked for it. When it cannot be, the old binary goes
/// back and is started instead.
pub fn restart() {
    let Some(r) = RESTART.lock().ok().and_then(|mut r| r.take()) else { return };
    let Err(e) = relaunch(&r.exe, &r.args) else { return };
    aura_core::aelog!("[UPDATE] {e}; the old version goes back");
    let failed = sibling(&r.exe, ".failed");
    let back = std::fs::rename(&r.exe, &failed).and_then(|_| std::fs::rename(&r.old, &r.exe));
    let _ = std::fs::remove_file(&failed);
    match back {
        Ok(()) => {
            if let Err(e) = relaunch(&r.exe, &r.args) {
                aura_core::aelog!("[UPDATE] {e}");
            }
        }
        Err(e) => aura_core::aelog!("[UPDATE] the old version could not go back: {e}"),
    }
}

fn relaunch(exe: &Path, args: &[OsString]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        // Its own console: this one closes with the old process.
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("cannot start the new version: {e}"))
}

// ── quitting and starting again ────────────────────────────────────────────

fn quit_notify() -> &'static tokio::sync::Notify {
    static QUIT: OnceLock<tokio::sync::Notify> = OnceLock::new();
    QUIT.get_or_init(tokio::sync::Notify::new)
}

fn request_quit() {
    // A permit is kept when nobody waits yet.
    quit_notify().notify_one();
}

/// Resolves when an installed update wants this process gone.
pub async fn quit_requested() {
    quit_notify().notified().await;
}

/// The pid after `--after-update`: this process was started by the one it
/// replaces.
pub fn after_update() -> Option<u32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at = args.iter().position(|a| a == "--after-update")?;
    args.get(at + 1)?.parse().ok()
}

/// At the start: what an update left behind — the old binaries beside this one
/// (once their process has gone) and the downloads.
pub fn clean_up() {
    let removed = remove_old_binaries();
    if let Some(dir) = aura_core::app_dir::root().map(|d| d.join("streamer").join("update")) {
        let _ = std::fs::remove_dir_all(dir);
    }
    if removed.1 > 0 {
        // Windows lets go of a binary a moment after its process ends.
        std::thread::spawn(|| {
            for wait in [5, 30] {
                std::thread::sleep(Duration::from_secs(wait));
                if remove_old_binaries().1 == 0 {
                    break;
                }
            }
        });
    }
}

/// `(removed, left)` of `<exe>.<stamp>.old` beside the running binary.
fn remove_old_binaries() -> (usize, usize) {
    let Ok(exe) = std::env::current_exe() else { return (0, 0) };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return (0, 0) };
    old_binaries_in(dir, &name.to_string_lossy())
}

fn old_binaries_in(dir: &Path, exe_name: &str) -> (usize, usize) {
    let (mut removed, mut left) = (0, 0);
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(mid) = name.strip_prefix(exe_name).and_then(|r| r.strip_prefix('.')).and_then(|r| r.strip_suffix(".old"))
        else {
            continue;
        };
        if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        } else {
            left += 1;
        }
    }
    (removed, left)
}

// ── helpers ────────────────────────────────────────────────────────────────

/// `fut` from a thread that may wait: on the server's runtime when there is one
/// (the commands run on its blocking pool), else on a small one of its own.
fn run_blocking<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => Some(h.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread().enable_all().build().ok().map(|rt| rt.block_on(fut)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, with_zip: bool, draft: bool) -> Value {
        let v = tag.trim_start_matches('v');
        let name = format!("{ASSET_PREFIX}{v}{ASSET_SUFFIX}");
        let mut assets = vec![json!({ "name": format!("aura-engine-v{v}-windows-x64.zip"), "browser_download_url": "https://x/app.zip" })];
        if with_zip {
            assets.push(json!({ "name": name, "browser_download_url": format!("https://x/{name}") }));
            assets.push(json!({ "name": format!("{name}.sig"), "browser_download_url": format!("https://x/{name}.sig") }));
        }
        json!({ "tag_name": tag, "draft": draft, "prerelease": false, "body": format!("notes {v}"),
                "html_url": format!("https://x/r/{tag}"), "assets": assets })
    }

    #[test]
    fn the_newest_release_with_this_systems_zip_is_offered() {
        let list = vec![
            release("v2.1.0", false, false),
            release("v2.0.5", true, true),
            release("v2.0.1", true, false),
            release("v2.0.0", true, false),
            release("instruments-pack-3", true, false),
        ];
        let o = newest_offer(&list).unwrap();
        assert_eq!(o.tag, "v2.0.1", "no zip in 2.1.0, 2.0.5 a draft");
        assert_eq!(o.asset_name, format!("aura-streamer-v2.0.1{ASSET_SUFFIX}"));
        assert!(o.sig_url.ends_with(".sig"));
        assert!(newest_offer(&[release("v2.0.0", false, false)]).is_none());
    }

    #[test]
    fn a_zip_without_its_signature_is_not_offered() {
        let mut r = release("v2.0.0", true, false);
        r["assets"].as_array_mut().unwrap().retain(|a| !a["name"].as_str().unwrap().ends_with(".sig"));
        assert!(offer_of(&r).is_none());
    }

    #[test]
    fn an_older_or_the_same_version_is_never_available() {
        let at = |tag: &str| Checked {
            at: Instant::now(),
            offer: offer_of(&release(tag, true, false)),
            release_notes: vec![],
            error: None,
        };
        assert!(!at("v0.0.1").available());
        assert!(!at(&format!("v{CURRENT}")).available());
        assert!(at("v999.0.0").available());
        let v = to_value(&at("v0.0.1"));
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["install"], json!(false));
        assert_eq!(v["product"], json!("streamer"));
        assert_eq!(v["version"], json!(""), "an older release is not shown");
    }

    #[test]
    fn notes_of_every_release_between_newest_first() {
        let list = vec![release("v2.0.0", true, false), release("v2.0.2", true, false), release("v2.0.1", false, false), release("v2.0.3", false, true)];
        let n = notes_between(&list, "1.9.9", "v2.0.2");
        let versions: Vec<&str> = n.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(versions, ["2.0.2", "2.0.1", "2.0.0"]);
    }

    #[test]
    fn off_in_a_test_build_without_the_hook() {
        if std::env::var_os("AURA_UPDATE_TEST_FEED").is_some() {
            return;
        }
        let v = check();
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(v["product"], json!("streamer"));
        assert!(install().unwrap_err().contains("off"));
    }

    #[test]
    fn only_the_binary_is_taken_from_the_zip() {
        for ok in [BIN.to_string(), format!("aura-streamer/{BIN}")] {
            assert!(is_binary_entry(&ok), "{ok}");
        }
        for bad in [
            format!("a/b/{BIN}"),
            format!("../{BIN}"),
            format!("/{BIN}"),
            format!("C:{BIN}"),
            format!("a\\{BIN}"),
            "aura-streamer/README.txt".to_string(),
            format!("{BIN}x"),
        ] {
            assert!(!is_binary_entry(&bad), "{bad}");
        }
    }

    fn zip_with(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let p = dir.join("u.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        for (name, bytes) in entries {
            z.start_file(*name, zip::write::FileOptions::default()).unwrap();
            z.write_all(bytes).unwrap();
        }
        z.finish().unwrap();
        p
    }

    #[test]
    fn the_binary_is_unpacked_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        let bin_entry = format!("aura-streamer/{BIN}");
        let z = zip_with(t.path(), &[("aura-streamer/README.txt", b"r"), (&bin_entry, b"new binary"), ("../evil", b"x")]);
        let dest = t.path().join("out.new");
        extract_binary(&z, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        assert!(!t.path().join("README.txt").exists() && !t.path().parent().unwrap().join("evil").exists());

        let twice = zip_with(t.path(), &[(BIN, b"a"), (&bin_entry, b"b")]);
        assert!(extract_binary(&twice, &dest).unwrap_err().contains("twice"));
        let none = zip_with(t.path(), &[("aura-streamer/README.txt", b"r")]);
        assert!(extract_binary(&none, &dest).unwrap_err().contains("not in the zip"));
    }

    #[test]
    fn the_new_binary_takes_the_old_ones_place_and_the_old_one_is_cleared_later() {
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join(BIN);
        std::fs::write(&exe, b"old").unwrap();
        let new = sibling(&exe, ".new");
        std::fs::write(&new, b"new").unwrap();
        let old = swap(&new, &exe, 42).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        assert!(!new.exists());
        std::fs::write(t.path().join(format!("{BIN}.x.old")), b"not ours").unwrap();
        assert_eq!(old_binaries_in(t.path(), BIN), (1, 0));
        assert!(!old.exists() && exe.exists() && t.path().join(format!("{BIN}.x.old")).exists());

        // No new one: the running one stays where it is.
        assert!(swap(&new, &exe, 43).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
    }

    #[test]
    fn a_signed_zip_passes_and_a_changed_one_does_not() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7; 32]);
        let t = tempfile::tempdir().unwrap();
        let z = zip_with(t.path(), &[(BIN, b"bin")]);
        let name = format!("{ASSET_PREFIX}2.0.0{ASSET_SUFFIX}");
        let sha = aura_core::checksum::sha256_file(&z).unwrap();
        let sig = STANDARD.encode(key.sign(build_update_message("v2.0.0", &name, &sha).as_bytes()).to_bytes());
        let pubkey = key.verifying_key().to_bytes();
        assert!(verify_ed25519(&pubkey, build_update_message("v2.0.0", &name, &sha).as_bytes(), &sig).is_ok());
        assert!(verify_ed25519(&pubkey, build_update_message("v2.0.1", &name, &sha).as_bytes(), &sig).is_err());
    }

    #[test]
    fn the_new_one_gets_this_ones_arguments() {
        let args = relaunch_args(vec!["--after-update".into(), "5".into(), "--x".into()], 9);
        assert_eq!(args, ["--after-update", "9", "--x"].map(OsString::from).to_vec());
    }
}
