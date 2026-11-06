//! Downloading the filter files a setting is missing, file by file, from the
//! public `filters` release, into the folder next to the app that the lookup
//! searches first after `AURA_FILTER_DIR`.
//!
//! Trust comes from the signed list (`manifest`), made with the update key;
//! a file counts only at its listed size and SHA-256 (`fetch`). One download
//! runs at a time and never together with an update install; conversions go
//! on, since a file appears only once whole (an atomic rename). Progress is
//! read from the `/filters/status` route, like the updater's.

pub mod fetch;
pub mod manifest;
pub mod plan;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lazy_static::lazy_static;
use serde::Serialize;

use crate::audio::converter::dsp::filter as flt;
use manifest::{Entry, Manifest};

const RELEASE_URL: &str = "https://github.com/ToxaDev/aura-engine/releases/download/filters/";

/// How long a fetched list is used before it is fetched again.
const MANIFEST_TTL: Duration = Duration::from_secs(600);

/// The release, or in a build that is not a release `AURA_FILTERS_URL` (a
/// test server), ending in `/`.
fn base_url() -> String {
    if !crate::updater::is_release_build() {
        if let Ok(u) = std::env::var("AURA_FILTERS_URL") {
            return if u.ends_with('/') { u } else { format!("{u}/") };
        }
    }
    RELEASE_URL.to_string()
}

fn source() -> Result<fetch::HttpSource, String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("AuraEngine/{}", env!("CARGO_PKG_VERSION")))
        // No overall deadline: a 1 GB file on a slow line takes long.
        // Silence is caught per chunk (`fetch::Timing`).
        .connect_timeout(Duration::from_secs(15))
        .https_only(crate::updater::is_release_build())
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    Ok(fetch::HttpSource { client, base_url: base_url() })
}

/// What the dialog reads.
#[derive(Clone, Serialize)]
pub struct Progress {
    /// idle checking downloading done cancelled error
    pub phase: String,
    /// 1-based number of the file under way, of `files`.
    pub file: u32,
    pub files: u32,
    pub name: String,
    /// Bytes in hand of `total`, over all the files, parts from before included.
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

impl Progress {
    fn idle() -> Self {
        Progress { phase: "idle".into(), file: 0, files: 0, name: String::new(), done: 0, total: 0, error: None }
    }
}

lazy_static! {
    static ref PROGRESS: Mutex<Progress> = Mutex::new(Progress::idle());
    static ref MANIFEST: Mutex<Option<(Instant, Arc<Manifest>)>> = Mutex::new(None);
}
static CANCEL: AtomicBool = AtomicBool::new(false);

fn lock(slot: &Mutex<Progress>) -> std::sync::MutexGuard<'_, Progress> {
    // A poisoned lock must not stop the status route for good.
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

fn progress() -> std::sync::MutexGuard<'static, Progress> {
    lock(&PROGRESS)
}

fn busy(phase: &str) -> bool {
    matches!(phase, "checking" | "downloading")
}

/// Whether a download is under way.
pub fn in_flight() -> bool {
    busy(&progress().phase)
}

/// Serialised progress for the `/filters/status` route.
pub fn status_json() -> Vec<u8> {
    serde_json::to_vec(&*progress()).unwrap_or_default()
}

/// The signed list, from the cache while it is fresh.
async fn manifest(source: &dyn fetch::Source, fresh: bool) -> Result<Arc<Manifest>, String> {
    manifest_with(source, fresh, crate::updater::active_pubkey()).await
}

async fn manifest_with(
    source: &dyn fetch::Source,
    fresh: bool,
    pubkey: Option<[u8; 32]>,
) -> Result<Arc<Manifest>, String> {
    if !fresh {
        if let Some((at, m)) = MANIFEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if at.elapsed() < MANIFEST_TTL {
                return Ok(m.clone());
            }
        }
    }
    let pubkey = pubkey
        .ok_or_else(|| "Filter downloads are not available in this build (it has no update key).".to_string())?;
    let idle = crate::updater::RESPONSE_WAIT;
    let reach = |e: String| format!("Cannot get the filter list: {e}");
    let bytes = fetch::fetch_small(source, manifest::MANIFEST_NAME, manifest::MANIFEST_MAX_BYTES, idle)
        .await
        .map_err(reach)?;
    let sig = fetch::fetch_small(source, manifest::SIG_NAME, manifest::SIG_MAX_BYTES, idle).await.map_err(reach)?;
    let m = Arc::new(manifest::verify(&bytes, &sig, &pubkey)?);
    *MANIFEST.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), m.clone()));
    Ok(m)
}

fn dest_dir() -> Result<PathBuf, String> {
    flt::portable_filter_dir().ok_or_else(|| "The app's own folder is unknown.".to_string())
}

/// The nearest folder of `path` that exists, `path` itself included.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| p.is_dir())
}

/// Whether files can be made in `dir` (or, while it does not exist yet, in
/// the folder it would be made in). Leaves nothing behind.
fn writable(dir: &Path) -> bool {
    let Some(at) = existing_ancestor(dir) else { return false };
    let probe = at.join(format!(".aura-write-probe-{}", std::process::id()));
    let ok = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

fn not_writable(dir: &Path) -> String {
    format!(
        "Cannot write to {} — move the app to a folder you can write to (not Program Files), \
         or put the filter files there by hand.",
        dir.display()
    )
}

fn phase_word(minimum: bool) -> &'static str {
    if minimum { "minimum" } else { "linear" }
}

/// Plan the download for a setting: `taps` at `fs` in `phases` (`"linear"`,
/// `"minimum"`; Hybrid-Phase asks for both) for the source `families`
/// (`"44k"`, `"48k"`; empty = both).
#[tauri::command]
pub async fn filters_plan(
    taps: usize,
    fs: u32,
    phases: Vec<String>,
    families: Vec<String>,
    hires: Option<bool>,
) -> Result<serde_json::Value, String> {
    flt::check_taps(taps)?;
    if ![2, 4, 8, 16, 32].contains(&fs) {
        return Err(format!("Unsupported FS ×{fs}"));
    }
    let mut minimum = Vec::new();
    for p in &phases {
        match p.as_str() {
            "linear" => minimum.push(false),
            "minimum" => minimum.push(true),
            other => return Err(format!("Unknown phase '{other}'")),
        }
    }
    if minimum.is_empty() {
        minimum.push(false);
    }
    let mut bases = Vec::new();
    for f in &families {
        match f.as_str() {
            "44k" => bases.push(44_100),
            "48k" => bases.push(48_000),
            other => return Err(format!("Unknown family '{other}'")),
        }
    }
    if bases.is_empty() {
        bases = vec![44_100, 48_000];
    }
    let src = source()?;
    let m = manifest(&src, false).await?;
    let dest = dest_dir()?;
    // One plan per group, so the dialog can tick them apart: the families'
    // own cells and, asked for, the hi-res sources' lower-factor cells. The
    // groups never share a cell (different bases, different factors).
    let dirs = flt::search_dir_list();
    let mut groups: Vec<(&str, Vec<plan::Cell>)> = bases.iter()
        .map(|&b| (if b == 44_100 { "44k" } else { "48k" }, plan::cells(taps, fs, &minimum, &[b])))
        .collect();
    if hires.unwrap_or(false) {
        groups.push(("hires", plan::hires_cells(taps, fs, &minimum, &bases)));
    }
    let mut p = plan::Plan::default();
    let mut group_of = Vec::new();
    for (g, cells) in &groups {
        let q = plan::plan(&m, cells, &dirs, &dest);
        group_of.extend(q.files.iter().map(|_| *g));
        p.files.extend(q.files);
        p.not_offered.extend(q.not_offered);
        p.shadowed.extend(q.shadowed.into_iter().filter(|s| !p.shadowed.contains(s)).collect::<Vec<_>>());
    }
    let to_fetch = p.to_fetch();
    let need = plan::space_needed(to_fetch);
    let free = existing_ancestor(&dest).and_then(crate::audio::disk_space::free_bytes);
    Ok(serde_json::json!({
        "files": p.files.iter().zip(&group_of)
            .map(|(n, g)| serde_json::json!({"name": n.name, "size": n.size, "part": n.part_bytes, "group": g}))
            .collect::<Vec<_>>(),
        "total": p.files.iter().map(|n| n.size).sum::<u64>(),
        "to_fetch": to_fetch,
        "need": need,
        "free": free,
        "enough": free.map(|f| p.files.is_empty() || f >= need),
        "dest": dest.display().to_string(),
        "writable": writable(&dest),
        "not_offered": p.not_offered.iter()
            .map(|c| serde_json::json!({"taps": c.taps, "rate": c.rate, "phase": phase_word(c.minimum)}))
            .collect::<Vec<_>>(),
        "shadowed": p.shadowed.iter().map(|s| s.display().to_string()).collect::<Vec<_>>(),
        "created": m.created,
    }))
}

/// Takes the slot under the updater's lock, then this one's (the order the
/// updater's claim takes them in), so a download and an install never both
/// start.
fn claim() -> Result<(), String> {
    claim_in(&PROGRESS, &CANCEL)
}

fn claim_in(slot: &Mutex<Progress>, cancel: &AtomicBool) -> Result<(), String> {
    crate::updater::unless_installing("An update is being installed — download filters after the restart.", || {
        let mut p = lock(slot);
        if busy(&p.phase) {
            return Err("A filter download is already running.".to_string());
        }
        *p = Progress { phase: "checking".into(), ..Progress::idle() };
        cancel.store(false, Ordering::Relaxed);
        Ok(())
    })
}

/// Puts the phase to `error` if the worker ends without a word (a panic), so
/// a dead download does not hold back updates for good.
struct Finish(bool);

impl Drop for Finish {
    fn drop(&mut self) {
        if !self.0 {
            let mut p = progress();
            p.phase = "error".into();
            p.error = Some("The download stopped unexpectedly.".into());
        }
    }
}

/// Download exactly `names` (from a plan; each must be in the signed list)
/// in the background; progress at `/filters/status`.
#[tauri::command]
pub fn filters_download(names: Vec<String>) -> Result<(), String> {
    if names.is_empty() {
        return Err("Nothing to download.".to_string());
    }
    claim()?;
    tauri::async_runtime::spawn(async move {
        let mut finish = Finish(false);
        let end = match (source(), dest_dir()) {
            (Ok(src), Ok(dest)) => run(&src, &dest, &names, &CANCEL, fetch::Timing::REAL, &PROGRESS).await,
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        let mut p = progress();
        match end {
            Ok(true) => p.phase = "done".into(),
            Ok(false) => p.phase = "cancelled".into(),
            Err(e) => {
                crate::aelog!("[FILTERS] download failed: {}", e);
                p.phase = "error".into();
                p.error = Some(e);
            }
        }
        finish.0 = true;
    });
    Ok(())
}

/// Stop the download after the chunk in hand; what arrived stays in the
/// `.part` files for the next try.
#[tauri::command]
pub fn filters_cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// The worker: `Ok(true)` once every file is in place, `Ok(false)` when
/// cancelled.
async fn run(
    src: &dyn fetch::Source,
    dest: &Path,
    names: &[String],
    cancel: &AtomicBool,
    timing: fetch::Timing,
    slot: &Mutex<Progress>,
) -> Result<bool, String> {
    let m = manifest(src, false).await?;
    let mut entries: Vec<Entry> = Vec::new();
    for n in names {
        let e = m.get(n).ok_or_else(|| format!("{n} is not in the filter list."))?;
        if !entries.iter().any(|x| x.name == e.name) {
            entries.push(e.clone());
        }
    }
    if !writable(dest) {
        return Err(not_writable(dest));
    }
    std::fs::create_dir_all(dest).map_err(|_| not_writable(dest))?;

    // A file already in place at its size and hash is not fetched again.
    let mut todo = Vec::new();
    for e in entries {
        let path = dest.join(&e.name);
        if std::fs::metadata(&path).is_ok_and(|md| md.len() == e.size) {
            let sha = tokio::task::spawn_blocking(move || crate::updater::sha256_file(&path)).await;
            if matches!(sha, Ok(Ok(ref h)) if *h == e.sha256) {
                continue;
            }
        }
        todo.push(e);
    }
    let part = |e: &Entry| {
        std::fs::metadata(fetch::part_path(dest, &e.name)).map(|md| md.len()).ok().filter(|n| *n <= e.size).unwrap_or(0)
    };
    let total: u64 = todo.iter().map(|e| e.size).sum();
    let to_fetch: u64 = todo.iter().map(|e| e.size - part(e)).sum();
    if let Some(free) = crate::audio::disk_space::free_bytes(dest) {
        let need = plan::space_needed(to_fetch);
        if free < need {
            return Err(format!(
                "Not enough free space in {}: the filters need {:.1} GB, {:.1} GB is free.",
                dest.display(),
                need as f64 / 1e9,
                free as f64 / 1e9
            ));
        }
    }
    {
        let mut p = lock(slot);
        p.phase = "downloading".into();
        p.files = todo.len() as u32;
        p.total = total;
        p.done = todo.iter().map(part).sum();
    }
    let mut before = 0u64;
    for (i, e) in todo.iter().enumerate() {
        {
            let mut p = lock(slot);
            p.file = i as u32 + 1;
            p.name = e.name.clone();
        }
        crate::aelog!("[FILTERS] {} ({} bytes)", e.name, e.size);
        let mut tell = |have: u64| lock(slot).done = before + have;
        match fetch::fetch_file(src, dest, e, cancel, timing, &mut tell).await {
            Ok(()) => before += e.size,
            Err(fetch::FetchError::Cancelled) => return Ok(false),
            Err(fetch::FetchError::Failed(why)) => return Err(why),
        }
    }
    Ok(true)
}

/// The filter files in the download folder, for the storage view.
#[tauri::command]
pub async fn filters_storage() -> Result<serde_json::Value, String> {
    let dest = dest_dir()?;
    // The list marks what is verified; without it (offline) the files are
    // still shown.
    let m = match source() {
        Ok(src) => manifest(&src, false).await.ok(),
        Err(_) => None,
    };
    let items = storage::list(&dest, m.as_deref());
    Ok(serde_json::json!({
        "dest": dest.display().to_string(),
        "items": items,
        "total": items.iter().map(|i| i.size).sum::<u64>(),
        "verified_list": m.is_some(),
    }))
}

/// Show the download folder in the file manager (made if it is not there yet).
#[tauri::command]
pub fn filters_dir_open() -> Result<String, String> {
    let d = dest_dir()?;
    std::fs::create_dir_all(&d).map_err(|_| not_writable(&d))?;
    let opener = if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(&d).spawn().map_err(|e| e.to_string())?;
    Ok(d.display().to_string())
}

/// Move exactly `names` from the download folder to the Recycle Bin. Refused
/// while a download or a conversion runs; held under the download's lock so
/// none starts meanwhile.
#[tauri::command]
pub async fn filters_trash(names: Vec<String>) -> Result<Vec<storage::Removed>, String> {
    let dest = dest_dir()?;
    tokio::task::spawn_blocking(move || {
        let p = progress();
        if busy(&p.phase) {
            return Err("Filters are downloading — remove files after it finishes.".to_string());
        }
        if crate::audio::converter::is_running() {
            return Err("A conversion is running — remove files after it finishes.".to_string());
        }
        // A thread of its own: the Recycle Bin is a COM call that sets up its
        // apartment, which a pooled thread may already have set up otherwise.
        let out = std::thread::scope(|sc| sc.spawn(|| storage::remove(&dest, &names, &storage::to_recycle_bin)).join())
            .map_err(|_| "Removing files failed.".to_string())?;
        drop(p);
        for r in &out {
            crate::aelog!("[FILTERS] to the Recycle Bin: {} {}", r.name, r.error.as_deref().unwrap_or("ok"));
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("Removing files failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::fetch::tests::{data, sha_hex, Fake, FAST};
    use super::manifest::tests::{entry_json, manifest_json, sign, test_key};
    use super::*;

    const A: &str = "fir_5k_88200_linear_phase.npy";
    const B: &str = "fir_5k_96000_linear_phase.npy";

    /// A fake release with two files and its signed list, the cache primed
    /// as `manifest()` would leave it.
    fn release() -> (Fake, Vec<u8>, Vec<u8>) {
        let (a, b) = (data(50), data(70));
        let m = manifest_json(vec![
            entry_json(A, 50, &sha_hex(&a), "5k", 88_200, "linear"),
            entry_json(B, 70, &sha_hex(&b), "5k", 96_000, "linear"),
        ]);
        let sig = sign(&test_key(), &m);
        let fake = Fake::new(&[
            (A, a.clone()),
            (B, b.clone()),
            (manifest::MANIFEST_NAME, m.clone()),
            (manifest::SIG_NAME, sig.clone()),
        ]);
        (fake, a, b)
    }

    fn prime(fake: &Fake) {
        let m = fake.files.get(manifest::MANIFEST_NAME).unwrap();
        let sig = fake.files.get(manifest::SIG_NAME).unwrap();
        let parsed = manifest::verify(m, sig, &test_key().verifying_key().to_bytes()).unwrap();
        *MANIFEST.lock().unwrap() = Some((Instant::now(), Arc::new(parsed)));
    }

    /// The global slot and cache are shared: the tests that use them run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn a_download_fetches_what_is_missing_and_counts_every_byte() {
        let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("fir-optimizer").join("output");
        let (fake, a, b) = release();
        prime(&fake);
        let slot = Mutex::new(Progress::idle());
        let names = vec![A.to_string(), B.to_string(), A.to_string()];
        assert_eq!(run(&fake, &dest, &names, &AtomicBool::new(false), FAST, &slot).await, Ok(true));
        assert_eq!(std::fs::read(dest.join(A)).unwrap(), a);
        assert_eq!(std::fs::read(dest.join(B)).unwrap(), b);
        let p = lock(&slot).clone();
        assert_eq!((p.files, p.file, p.done, p.total), (2, 2, 120, 120));

        // Both in place: nothing is asked again.
        let asked = fake.asked.lock().unwrap().len();
        assert_eq!(run(&fake, &dest, &names, &AtomicBool::new(false), FAST, &slot).await, Ok(true));
        assert_eq!(fake.asked.lock().unwrap().len(), asked);
        // A damaged copy at the right size is fetched again.
        std::fs::write(dest.join(B), vec![0u8; 70]).unwrap();
        assert_eq!(run(&fake, &dest, &names, &AtomicBool::new(false), FAST, &slot).await, Ok(true));
        assert_eq!(std::fs::read(dest.join(B)).unwrap(), b);
    }

    #[tokio::test]
    async fn names_outside_the_list_and_a_cancel_are_handled() {
        let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        let (fake, ..) = release();
        prime(&fake);
        let slot = Mutex::new(Progress::idle());
        let r = run(&fake, t.path(), &["fir_evil.npy".to_string()], &AtomicBool::new(false), FAST, &slot).await;
        assert!(r.unwrap_err().contains("not in the filter list"));
        let r = run(&fake, t.path(), &[A.to_string()], &AtomicBool::new(true), FAST, &slot).await;
        assert_eq!(r, Ok(false));
        assert!(!t.path().join(A).exists());
    }

    #[tokio::test]
    async fn the_list_is_fetched_and_checked_once_then_cached() {
        let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (fake, ..) = release();
        *MANIFEST.lock().unwrap() = None;
        let key = Some(test_key().verifying_key().to_bytes());
        assert!(manifest_with(&fake, true, None).await.unwrap_err().contains("no update key"));
        let wrong = Some(ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes());
        assert!(manifest_with(&fake, true, wrong).await.unwrap_err().contains("signature"));
        let m = manifest_with(&fake, true, key).await.unwrap();
        assert_eq!(m.files.len(), 2);
        let asked = fake.asked.lock().unwrap().len();
        manifest_with(&fake, false, key).await.unwrap();
        assert_eq!(fake.asked.lock().unwrap().len(), asked);
    }

    #[test]
    fn one_download_at_a_time() {
        // A slot of its own: the global one is read by the updater's tests.
        let slot = Mutex::new(Progress::idle());
        let cancel = AtomicBool::new(true);
        assert!(claim_in(&slot, &cancel).is_ok());
        assert!(busy(&lock(&slot).phase) && !cancel.load(Ordering::Relaxed));
        assert!(claim_in(&slot, &cancel).unwrap_err().contains("already running"));
        lock(&slot).phase = "error".into();
        assert!(claim_in(&slot, &cancel).is_ok());
    }

    #[test]
    fn writability_is_probed_without_leaving_anything() {
        let t = tempfile::tempdir().unwrap();
        let deep = t.path().join("a").join("b");
        assert!(writable(&deep));
        assert!(!t.path().join("a").exists());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    }
}
