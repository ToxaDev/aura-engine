//! The instruments pack: what scenes with instruments (Spec 2) need that the
//! app does not ship — the networks that take a song apart (HTDemucs's six
//! sources, DrumSep's kit pieces, Basic Pitch's notes; ONNX) and the ONNX
//! Runtime they run on (its DirectML build + DirectML.dll). Downloaded once,
//! on the user's word, from a pre-release of the project's GitHub; from then
//! on an app update that pins a newer pack fetches it by itself, in the
//! background (Anton 28.09: whoever once took the instruments gets their
//! updates with the app's).
//!
//! Trust: the zip's size and SHA-256 are pinned here (`PIN`); a file that is
//! not exactly the published one is never unpacked. Its `pack.json` lists
//! every file with its size and SHA-256, checked while unpacking.
//!
//! In the root (`%LOCALAPPDATA%\AuraEngine\spatial\`; a run with a WebView2
//! profile of its own — our test tool's windows — keeps it in that profile;
//! `AURA_SPATIAL_DIR` names another folder):
//!
//! - `pack-<n>\` — a complete pack; its `.installed` is written last;
//! - `pack-<n>.zip.part` — a download under way, kept to go on from;
//! - `pack-<n>.tmp\` — being unpacked, then renamed to `pack-<n>` in one step;
//! - `instruments.json` — `{"on": true}` once the user took the pack: the
//!   app keeps it up to date from then on (read at start, before any window);
//! - `remove.pending` — packs to delete at the next start (in use when the
//!   user removed them).
//!
//! The runtime DLLs load once a run, from the pack in use, and Windows does
//! not delete a loaded DLL: so a new pack goes in beside the old one, and the
//! old one goes at the next start, before anything is loaded.
//!
//! While the pinned pack is missing, an older complete one keeps serving if
//! it was made for the same major of the scene spec. So a model whose format
//! changes gets a new file name (`drumsep2.onnx`): an older pack then simply
//! lacks it, and whoever asks for it (`Pack::file`) does without — it never
//! reads a file it does not know.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A pack an app works with; it changes only with the app.
pub struct Pin {
    pub version: u32,
    /// The major of the scene spec its networks serve.
    pub spec_major: u32,
    /// The published zip, byte for byte.
    pub zip_size: u64,
    pub zip_sha256: &'static str,
    /// Its files unpacked: the room an install needs besides the zip.
    pub unpacked: u64,
    pub url: &'static str,
}

/// The pack this app pins: `aura-instruments-pack-2.zip`, made by
/// `make_pack2.py` (HTDemucs, ONNX Runtime 1.24.4 DirectML, Basic Pitch,
/// DrumSep, their licences). Published as a PRE-release: the app's updater
/// reads `/releases/latest`, which a pre-release never is.
pub const PIN: Pin = Pin {
    version: 2,
    spec_major: 2,
    zip_size: 282_375_113,
    zip_sha256: "56e1680f30d8ad64ccd82d0139956059a3a29bbf89a108285a89aced7c0fe8ce",
    unpacked: 373_868_553,
    url: "https://github.com/ToxaDev/aura-engine/releases/download/instruments-pack-2/aura-instruments-pack-2.zip",
};

pub const MODEL: &str = "htdemucs_6s.onnx";
pub const RUNTIME: &str = "onnxruntime.dll";
pub const DIRECTML: &str = "DirectML.dll";
/// DrumSep: the kit pieces (read by the kit's analysis).
#[allow(dead_code)]
pub const DRUMS: &str = "drumsep.onnx";
/// Basic Pitch: the notes (read by the notes' analysis).
#[allow(dead_code)]
pub const PITCH: &str = "basic_pitch.onnx";
/// A pack without these is no use at all.
const REQUIRED: &[&str] = &[RUNTIME, DIRECTML, MODEL];
const MANIFEST: &str = "pack.json";
/// Written last, when every file is in place.
const DONE: &str = ".installed";
const FLAG: &str = "instruments.json";
const PENDING: &str = "remove.pending";

fn is_release_build() -> bool {
    option_env!("AURA_RELEASE_BUILD") == Some("1")
}

/// The published zip; a test build may be pointed at a local server
/// (`AURA_SPATIAL_PACK_URL`) — the pinned hash holds either way.
fn url() -> String {
    if !is_release_build() {
        if let Ok(u) = std::env::var("AURA_SPATIAL_PACK_URL") {
            return u;
        }
    }
    PIN.url.to_string()
}

/// The zip's file name, as published.
fn zip_name(pin: &Pin) -> &'static str {
    pin.url.rsplit('/').next().unwrap_or(pin.url)
}

/// The folder packs are kept in.
pub fn root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("AURA_SPATIAL_DIR") {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER") {
        return Some(PathBuf::from(d).join("spatial"));
    }
    Some(crate::app_dir::root()?.join("spatial"))
}

// ── the manifest ──

#[derive(serde::Deserialize)]
struct Manifest {
    pack_version: u32,
    #[serde(default)]
    spec_version: String,
    #[serde(default)]
    files: Vec<Entry>,
}

#[derive(serde::Deserialize)]
struct Entry {
    name: String,
    size: u64,
    sha256: String,
}

/// "2.1" → 2; nothing → 0 (pack 1 said nothing: no app uses it now).
fn spec_major(v: &str) -> u32 {
    v.split('.').next().and_then(|m| m.trim().parse().ok()).unwrap_or(0)
}

/// A name that stays inside the pack's folder and is not one of its marks.
fn plain_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\\', ':']) && n != MANIFEST && n != DONE
}

impl Manifest {
    fn parse(b: &[u8]) -> Result<Manifest, String> {
        serde_json::from_slice(b).map_err(|e| format!("{MANIFEST}: {e}"))
    }

    /// What this app needs of a pack's manifest before anything is unpacked.
    fn check(&self, pin: &Pin) -> Result<(), String> {
        if self.pack_version != pin.version {
            return Err(format!("{MANIFEST} is for pack {}, not {}", self.pack_version, pin.version));
        }
        if spec_major(&self.spec_version) != pin.spec_major {
            return Err(format!("{MANIFEST} is for spec {:?}, not {}.x", self.spec_version, pin.spec_major));
        }
        for (i, e) in self.files.iter().enumerate() {
            if !plain_name(&e.name) {
                return Err(format!("{MANIFEST} names a file {:?}", e.name));
            }
            if self.files[..i].iter().any(|o| o.name.eq_ignore_ascii_case(&e.name)) {
                return Err(format!("{MANIFEST} lists {} twice", e.name));
            }
            if e.sha256.len() != 64 || !e.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("{MANIFEST} has no sha256 for {}", e.name));
            }
        }
        match REQUIRED.iter().find(|r| !self.files.iter().any(|e| e.name == **r)) {
            Some(r) => Err(format!("{MANIFEST} does not list {r}")),
            None => Ok(()),
        }
    }
}

// ── the packs on disk ──

/// A complete pack.
#[derive(Clone, Debug, PartialEq)]
pub struct Pack {
    pub dir: PathBuf,
    pub version: u32,
    pub spec_major: u32,
    files: Vec<String>,
    /// Its files, in bytes.
    pub bytes: u64,
}

impl Pack {
    /// One of the pack's files, if this pack has it (the kit's and the
    /// notes' analysis ask for theirs).
    #[allow(dead_code)]
    pub fn file(&self, name: &str) -> Option<PathBuf> {
        self.files.iter().any(|f| f == name).then(|| self.dir.join(name))
    }
}

/// `dir` as pack `version`, if it is complete: marked done, its manifest
/// read, every file it lists there at its size (sizes only — this is asked
/// often; the hashes were checked when it went in).
fn read_pack(dir: &Path, version: u32) -> Option<Pack> {
    let done = std::fs::read_to_string(dir.join(DONE)).ok()?;
    if done.split_whitespace().next()? != version.to_string() {
        return None;
    }
    let m = Manifest::parse(&std::fs::read(dir.join(MANIFEST)).ok()?).ok()?;
    if m.pack_version != version || !REQUIRED.iter().all(|r| m.files.iter().any(|e| e.name == *r)) {
        return None;
    }
    let mut bytes = 0;
    for e in &m.files {
        if !plain_name(&e.name) {
            return None;
        }
        let md = std::fs::metadata(dir.join(&e.name)).ok()?;
        if !md.is_file() || md.len() != e.size {
            return None;
        }
        bytes += e.size;
    }
    Some(Pack {
        dir: dir.to_path_buf(),
        version,
        spec_major: spec_major(&m.spec_version),
        files: m.files.into_iter().map(|e| e.name).collect(),
        bytes,
    })
}

fn pack_dir(root: &Path, version: u32) -> PathBuf {
    root.join(format!("pack-{version}"))
}

fn part_path(root: &Path, version: u32) -> PathBuf {
    root.join(format!("pack-{version}.zip.part"))
}

/// The pinned pack, if it is complete.
fn current_in(root: &Path, pin: &Pin) -> Option<Pack> {
    read_pack(&pack_dir(root, pin.version), pin.version)
}

/// What sits in the root, newest first: (name, version, what follows the
/// number — "", "tmp", "zip.part" —, is a folder).
fn entries(root: &Path) -> Vec<(String, u32, String, bool)> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut v: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rest = name.strip_prefix("pack-")?;
            let (num, kind) = rest.split_once('.').unwrap_or((rest, ""));
            let n: u32 = num.parse().ok()?;
            let kind = kind.to_string();
            let dir = e.file_type().ok()?.is_dir();
            Some((name, n, kind, dir))
        })
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// The pack to use: the pinned one when it is complete; else the newest
/// complete other one made for the same spec major (it serves while the
/// pinned one is fetched, or after a failed update); else none — scenes
/// with instruments then show the rough layer.
fn active_in(root: &Path, pin: &Pin) -> Option<Pack> {
    current_in(root, pin).or_else(|| {
        entries(root)
            .into_iter()
            .filter(|(_, n, kind, dir)| *dir && kind.is_empty() && *n != pin.version)
            .find_map(|(name, n, ..)| read_pack(&root.join(name), n).filter(|p| p.spec_major == pin.spec_major))
    })
}

static ACTIVE: Mutex<Option<(Instant, Option<Pack>)>> = Mutex::new(None);

/// The pack in use now. Looked at twice a second at most: the separation
/// worker and the scenes' data route ask all the time.
pub fn active() -> Option<Pack> {
    let mut g = ACTIVE.lock().unwrap();
    if let Some((at, p)) = g.as_ref() {
        if at.elapsed() < Duration::from_millis(500) {
            return p.clone();
        }
    }
    let p = root().and_then(|r| active_in(&r, &PIN));
    *g = Some((Instant::now(), p.clone()));
    p
}

/// After an install or a removal: look again at once.
fn forget() {
    *ACTIVE.lock().unwrap() = None;
}

/// The folder of the pack in use.
pub fn installed() -> Option<PathBuf> {
    active().map(|p| p.dir)
}

/// Where the pinned pack was looked for and what was not there (for the
/// log, when it is not found).
pub fn why_not() -> String {
    let Some(r) = root() else { return "no LOCALAPPDATA".into() };
    let d = pack_dir(&r, PIN.version);
    if !d.is_dir() {
        return format!("{} — not there", d.display());
    }
    let mut missing = Vec::new();
    match std::fs::read_to_string(d.join(DONE)) {
        Ok(s) if s.split_whitespace().next() == Some(PIN.version.to_string().as_str()) => {}
        Ok(_) => missing.push(format!("{DONE} (another pack's)")),
        Err(e) => missing.push(format!("{DONE} ({e})")),
    }
    match std::fs::read(d.join(MANIFEST)).map_err(|e| e.to_string()).and_then(|b| Manifest::parse(&b)) {
        Ok(m) => {
            for e in &m.files {
                match std::fs::metadata(d.join(&e.name)) {
                    Ok(md) if md.is_file() && md.len() == e.size => {}
                    Ok(md) => missing.push(format!("{} ({} bytes, not {})", e.name, md.len(), e.size)),
                    Err(err) => missing.push(format!("{} ({err})", e.name)),
                }
            }
        }
        Err(e) => missing.push(format!("{MANIFEST} ({e})")),
    }
    let missing = if missing.is_empty() { "nothing".to_string() } else { missing.join(", ") };
    format!("{} — missing: {missing}", d.display())
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn tree_bytes(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| tree_bytes(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// What the packs and downloads take on the disk.
fn disk_bytes_in(root: &Path) -> u64 {
    entries(root).iter().map(|(name, ..)| tree_bytes(&root.join(name))).sum()
}

/// Delete `p` — a file, or a folder and what is in it — as far as it goes;
/// what is in use stays. A folder's `.installed` goes first, so a pack
/// deleted halfway is never taken for a complete one. True when all went.
fn delete(p: &Path) -> bool {
    if p.is_dir() {
        let _ = std::fs::remove_file(p.join(DONE));
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let q = e.path();
                let _ = if q.is_dir() { std::fs::remove_dir_all(&q) } else { std::fs::remove_file(&q) };
            }
        }
        std::fs::remove_dir(p).is_ok()
    } else {
        std::fs::remove_file(p).is_ok() || !p.exists()
    }
}

// ── the user took the pack: it is kept up to date ──

fn wanted_in(root: &Path) -> bool {
    std::fs::read(root.join(FLAG))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["on"].as_bool())
        .unwrap_or(false)
}

fn set_wanted_in(root: &Path, on: bool) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot make {}: {e}", root.display()))?;
    // written beside, then renamed over: never half a file
    let tmp = root.join(format!("{FLAG}.tmp"));
    std::fs::write(&tmp, serde_json::json!({ "on": on }).to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, root.join(FLAG)).map_err(|e| format!("{FLAG}: {e}"))?;
    if on {
        let _ = std::fs::remove_file(root.join(PENDING));
    }
    Ok(())
}

// ── clean-up and removal ──

/// At start, before anything is loaded: what earlier runs left — unpacking
/// that never finished, downloads of other versions, packs the user removed
/// while they were in use, and (once the pinned pack is complete) the packs
/// before it. Returns what went, for the log.
fn cleanup_in(root: &Path, pin: &Pin) -> Vec<String> {
    let pending = root.join(PENDING).is_file() && !wanted_in(root);
    let whole = current_in(root, pin).is_some();
    let mut gone = Vec::new();
    let mut left = false;
    for (name, n, kind, dir) in entries(root) {
        let go = match (kind.as_str(), dir) {
            ("tmp", true) => true,
            ("zip.part", false) => pending || whole || n != pin.version,
            ("", true) => pending || (whole && n != pin.version),
            _ => false,
        };
        if !go {
            continue;
        }
        if delete(&root.join(&name)) {
            gone.push(name);
        } else {
            left = true;
            crate::aelog!("[PACK] cannot remove {} yet", root.join(&name).display());
        }
    }
    if pending && !left {
        let _ = std::fs::remove_file(root.join(PENDING));
    }
    gone
}

/// Every pack and download deleted, and no more updates. What this run
/// uses (the runtime DLLs) goes at the next start. Returns the bytes freed
/// and whether some wait for the next start.
fn remove_in(root: &Path) -> (u64, bool) {
    let _ = set_wanted_in(root, false);
    let mut freed = 0;
    let mut later = false;
    for (name, ..) in entries(root) {
        let p = root.join(name);
        let before = tree_bytes(&p);
        if !delete(&p) {
            later = true;
        }
        freed += before.saturating_sub(tree_bytes(&p));
    }
    if later {
        let _ = std::fs::write(root.join(PENDING), "");
    }
    (freed, later)
}

// ── fetching ──

/// How a download behaves (the tests make it quick).
#[derive(Clone, Copy)]
struct Net {
    /// No data this long: the connection is taken for dead.
    stall: Duration,
    /// The pause before a broken-off download goes on.
    resume_wait: Duration,
    /// Tries in a row that bring nothing before it gives up.
    tries: u32,
}

const NET: Net = Net { stall: Duration::from_secs(30), resume_wait: Duration::from_secs(2), tries: 5 };

/// What an install says on its way: (state, bytes so far, of).
type Report<'a> = &'a (dyn Fn(&'static str, u64, u64) + Send + Sync);

enum Fail {
    /// Nothing answered: no network, or no server there.
    NoAnswer(String),
    /// It broke off, or the server had a moment: go on from where it is.
    Again(String),
    /// Going on would not help.
    Stop(String),
    Cancelled,
}

/// "bytes 100-199/200" → (100, Some(200)); "bytes 100-199/*" → (100, None).
fn content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let (range, total) = v.trim().strip_prefix("bytes")?.trim().split_once('/')?;
    let start = range.split_once('-')?.0.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((start, total))
}

/// The pinned zip into `part`, going on from what `part` holds already (an
/// earlier try, a Cancel, an earlier run). Returns when `part` has every
/// byte; the hash is checked after.
async fn fetch(url: &str, part: &Path, pin: &Pin, net: Net, cancel: &AtomicBool, report: Report<'_>) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("aura-engine/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .https_only(is_release_build())
        .build()
        .map_err(|e| e.to_string())?;
    let total = pin.zip_size;
    let mut idle = 0;
    let mut answered = false;
    loop {
        let mut before = file_len(part);
        if before > total {
            let _ = std::fs::remove_file(part);
            before = 0;
        }
        if before == total {
            report("downloading", total, total);
            return Ok(());
        }
        let r = fetch_once(&client, url, part, total, net, cancel, report).await;
        let after = file_len(part);
        if after > before {
            idle = 0;
        } else {
            idle += 1;
        }
        let why = match r {
            Ok(()) => return Ok(()),
            Err(Fail::Cancelled) => return Err("cancelled".into()),
            Err(Fail::Stop(e)) => return Err(e),
            Err(Fail::NoAnswer(e)) if !answered => return Err(e),
            Err(Fail::NoAnswer(e) | Fail::Again(e)) => e,
        };
        answered = true;
        if idle >= net.tries {
            return Err(why);
        }
        crate::aelog!("[PACK] {why} — going on from {} of {total} bytes", after.min(total));
        let until = Instant::now() + net.resume_wait;
        while Instant::now() < until {
            if cancel.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One request: from what `part` has to the end (or as far as it gets).
async fn fetch_once(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    total: u64,
    net: Net,
    cancel: &AtomicBool,
    report: Report<'_>,
) -> Result<(), Fail> {
    use futures_util::StreamExt;
    if cancel.load(Ordering::Acquire) {
        return Err(Fail::Cancelled);
    }
    let have = file_len(part);
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = match tokio::time::timeout(net.stall, req.send()).await {
        Err(_) => return Err(Fail::NoAnswer(format!("download failed: no answer in {} s", net.stall.as_secs()))),
        Ok(Err(e)) => return Err(Fail::NoAnswer(format!("download failed: {e}"))),
        Ok(Ok(r)) => r,
    };
    let status = resp.status();
    let (mut file, mut got) = match status.as_u16() {
        206 => {
            let cr = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            match content_range(&cr) {
                Some((start, t)) if start == have && t.map_or(true, |t| t == total) => {}
                _ => {
                    // not the part asked for: start over
                    let _ = std::fs::remove_file(part);
                    return Err(Fail::Again(format!("the server sent another range ({cr})")));
                }
            }
            let f = std::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .map_err(|e| Fail::Stop(format!("cannot open {}: {e}", part.display())))?;
            (f, have)
        }
        200 => {
            if let Some(len) = resp.content_length() {
                if len != total {
                    return Err(Fail::Stop(format!("the server offers {len} bytes — not the pack")));
                }
            }
            let f = std::fs::File::create(part).map_err(|e| Fail::Stop(format!("cannot create {}: {e}", part.display())))?;
            (f, 0)
        }
        416 => {
            let _ = std::fs::remove_file(part);
            return Err(Fail::Again("the server refused the range".into()));
        }
        s if s >= 500 => return Err(Fail::Again(format!("download failed: HTTP {status}"))),
        _ => return Err(Fail::Stop(format!("download failed: HTTP {status}"))),
    };
    report("downloading", got, total);
    let mut stream = resp.bytes_stream();
    let mut told = Instant::now();
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Fail::Cancelled);
        }
        let next = match tokio::time::timeout(net.stall, stream.next()).await {
            Ok(n) => n,
            Err(_) => return Err(Fail::Again(format!("the download stalled (no data for {} s)", net.stall.as_secs()))),
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| Fail::Again(format!("download error: {e}")))?;
        if got + chunk.len() as u64 > total {
            drop(file);
            let _ = std::fs::remove_file(part);
            return Err(Fail::Stop("the download is larger than the pack".into()));
        }
        file.write_all(&chunk).map_err(|e| Fail::Stop(format!("cannot write {}: {e}", part.display())))?;
        got += chunk.len() as u64;
        if told.elapsed() >= Duration::from_millis(100) {
            told = Instant::now();
            report("downloading", got, total);
        }
    }
    report("downloading", got, total);
    if got < total {
        return Err(Fail::Again("the connection closed early".into()));
    }
    Ok(())
}

/// A test build (not the published one, AURA_RELEASE_BUILD) with the pack's
/// zip beside its exe takes it from there: the same window, progress and
/// checks, no GitHub (Anton 27.09: the pack is published with 1.5.0; until
/// then his builds get it this way — he never picks a file).
fn dev_pack() -> Option<PathBuf> {
    if is_release_build() {
        return None;
    }
    let p = std::env::current_exe().ok()?.parent()?.join(zip_name(&PIN));
    p.is_file().then_some(p)
}

/// The dev pack copied as a download would be: in pieces, with progress,
/// cancellable.
fn copy_in(src: &Path, dest: &Path, cancel: &AtomicBool, report: Report<'_>) -> Result<(), String> {
    let total = file_len(src);
    let mut from = std::fs::File::open(src).map_err(|e| format!("cannot open {}: {e}", src.display()))?;
    let mut to = std::fs::File::create(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0u64;
    let mut told = Instant::now();
    report("downloading", 0, total);
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        let n = from.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if told.elapsed() >= Duration::from_millis(100) {
            told = Instant::now();
            report("downloading", got, total);
        }
    }
    report("downloading", got, total);
    Ok(())
}

/// The room an install needs, checked before it starts: what is left to
/// download, the pack unpacked, and a margin.
fn room(root: &Path, pin: &Pin, have: u64) -> Result<(), String> {
    let need = pin.zip_size.saturating_sub(have) + pin.unpacked + (64 << 20);
    match crate::audio::disk_space::free_bytes(root) {
        Some(free) if free < need => {
            let vol = crate::audio::disk_space::volume_root(root)
                .map(|v| v.trim_end_matches('\\').to_uppercase())
                .unwrap_or_else(|| root.display().to_string());
            Err(format!("Not enough free space on {vol} — the instruments pack needs {} MB.", need.div_ceil(1_000_000)))
        }
        _ => Ok(()),
    }
}

// ── checking and putting in place ──

/// The zip is exactly the pinned one.
fn verify(zip: &Path, pin: &Pin) -> Result<(), String> {
    let size = file_len(zip);
    let hash = crate::updater::sha256_file(zip)?;
    if size != pin.zip_size || !hash.eq_ignore_ascii_case(pin.zip_sha256) {
        return Err(format!("the file is not the published pack ({size} bytes, sha256 {hash})"));
    }
    Ok(())
}

/// A verified zip unpacked beside the packs there are, every file checked
/// against the manifest, then put in place in one step.
fn put_in_place(root: &Path, zip: &Path, pin: &Pin) -> Result<Pack, String> {
    let tmp = root.join(format!("pack-{}.tmp", pin.version));
    let dest = pack_dir(root, pin.version);
    if let Err(e) = unpack(zip, &tmp, pin) {
        delete(&tmp);
        return Err(e);
    }
    if dest.exists() && !delete(&dest) {
        delete(&tmp);
        return Err(format!("cannot replace {} (in use?)", dest.display()));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("cannot put the pack in place: {e}"))?;
    read_pack(&dest, pin.version).ok_or_else(|| format!("the pack in {} does not read back", dest.display()))
}

/// The files `pack.json` lists into `dir` (made anew), each checked, then
/// the manifest and the mark that the pack is whole.
fn unpack(zip: &Path, dir: &Path, pin: &Pin) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if dir.exists() && !delete(dir) {
        return Err(format!("cannot clear {}", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let f = std::fs::File::open(zip).map_err(|e| format!("cannot open the zip: {e}"))?;
    let mut a = zip::ZipArchive::new(f).map_err(|e| format!("not a zip: {e}"))?;
    let raw = {
        let e = a.by_name(MANIFEST).map_err(|_| format!("the pack has no {MANIFEST}"))?;
        let mut v = Vec::new();
        e.take(1 << 20).read_to_end(&mut v).map_err(|e| format!("{MANIFEST}: {e}"))?;
        v
    };
    let m = Manifest::parse(&raw)?;
    m.check(pin)?;
    let mut buf = vec![0u8; 1 << 20];
    for e in &m.files {
        let mut src = a.by_name(&e.name).map_err(|_| format!("the pack has no {}", e.name))?;
        let path = dir.join(&e.name);
        let mut out = std::fs::File::create(&path).map_err(|err| format!("cannot create {}: {err}", path.display()))?;
        let mut h = Sha256::new();
        let mut n = 0u64;
        loop {
            let k = src.read(&mut buf).map_err(|err| format!("unzip {}: {err}", e.name))?;
            if k == 0 {
                break;
            }
            n += k as u64;
            if n > e.size {
                return Err(format!("{} in the pack is larger than {MANIFEST} says", e.name));
            }
            h.update(&buf[..k]);
            out.write_all(&buf[..k]).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        }
        out.sync_all().map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        let hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if n != e.size || !hash.eq_ignore_ascii_case(&e.sha256) {
            return Err(format!("{} in the pack is not what {MANIFEST} says ({n} bytes, sha256 {hash})", e.name));
        }
    }
    std::fs::write(dir.join(MANIFEST), &raw).map_err(|e| format!("{MANIFEST}: {e}"))?;
    std::fs::write(dir.join(DONE), format!("{} {}\n", pin.version, pin.zip_sha256)).map_err(|e| format!("{DONE}: {e}"))?;
    Ok(())
}

/// The calling thread below normal priority while this lives: a background
/// update's hashing and unpacking never compete with the music.
struct LowPriority;

impl LowPriority {
    fn now() -> LowPriority {
        #[cfg(windows)]
        unsafe {
            use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
            SetThreadPriority(GetCurrentThread(), winapi::um::winbase::THREAD_PRIORITY_BELOW_NORMAL as i32);
        }
        LowPriority
    }
}

impl Drop for LowPriority {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
            SetThreadPriority(GetCurrentThread(), winapi::um::winbase::THREAD_PRIORITY_NORMAL as i32);
        }
    }
}

// ── the install the app runs (one at a time) ──

#[derive(Clone)]
struct Job {
    /// idle | downloading | verifying | unpacking | done | error | cancelled
    state: &'static str,
    got: u64,
    total: u64,
    /// What was there before this download (it went on from it).
    from: u64,
    error: Option<String>,
    /// Started by the app itself (an update), not by the user.
    background: bool,
}

const IDLE: Job = Job { state: "idle", got: 0, total: 0, from: 0, error: None, background: false };
static JOB: Mutex<Job> = Mutex::new(IDLE);
static BUSY: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

fn status_value(full: bool) -> serde_json::Value {
    let j = JOB.lock().unwrap().clone();
    let root = root();
    let act = active();
    let mut v = serde_json::json!({
        "installed": act.is_some(),
        "current": act.as_ref().is_some_and(|p| p.version == PIN.version),
        "installedVersion": act.as_ref().map(|p| p.version),
        "version": PIN.version,
        "spec": PIN.spec_major,
        "bytes": PIN.zip_size,
        "unpacked": PIN.unpacked,
        "wanted": root.as_deref().is_some_and(wanted_in),
        "state": j.state,
        "got": j.got,
        "total": j.total,
        "from": j.from,
        "error": j.error,
        "background": j.background,
        "busy": BUSY.load(Ordering::Acquire),
    });
    if full {
        v["diskBytes"] = root.as_deref().map_or(0, disk_bytes_in).into();
    }
    v
}

fn tell(app: &tauri::AppHandle) {
    use tauri::Manager;
    let _ = app.emit_all("spatial:progress", status_value(false));
}

fn set_job(app: &tauri::AppHandle, f: impl FnOnce(&mut Job)) {
    f(&mut JOB.lock().unwrap());
    tell(app);
}

/// Is a pack there, which, is the user keeping it, and what is an install doing.
#[tauri::command]
pub fn spatial_status() -> serde_json::Value {
    status_value(true)
}

/// Stop the install that is running (what came so far is kept to go on from).
#[tauri::command]
pub fn spatial_cancel() {
    CANCEL.store(true, Ordering::Release);
}

/// The user takes the pack: download, check, unpack. Resolves when it is in
/// (from then on it is kept up to date), or with the reason it is not;
/// `spatial:progress` events on the way.
#[tauri::command]
pub async fn spatial_install(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    run_install(&app, None, false).await
}

/// The same from a zip at hand (our checks, offline): the same pinned hash.
#[tauri::command]
pub async fn spatial_install_file(app: tauri::AppHandle, path: String) -> Result<serde_json::Value, String> {
    run_install(&app, Some(PathBuf::from(path)), false).await
}

/// Keep the pack up to date with the app (on), or not (off: an update under
/// way stops, nothing more is downloaded).
#[tauri::command]
pub fn spatial_set_wanted(on: bool) -> Result<serde_json::Value, String> {
    let root = root().ok_or("no place for the pack")?;
    set_wanted_in(&root, on)?;
    if !on && BUSY.load(Ordering::Acquire) && JOB.lock().unwrap().background {
        CANCEL.store(true, Ordering::Release);
    }
    crate::aelog!("[PACK] instruments {}", if on { "on: the pack is kept up to date" } else { "off: no pack updates" });
    Ok(status_value(true))
}

/// The user removes the pack: every pack and download deleted, no more
/// updates. Resolves with the bytes freed and whether some wait for the
/// next start (in use now).
#[tauri::command]
pub async fn spatial_remove(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let root = root().ok_or("no place for the pack")?;
    set_wanted_in(&root, false)?;
    if BUSY.load(Ordering::Acquire) {
        CANCEL.store(true, Ordering::Release);
        let t0 = Instant::now();
        while BUSY.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let (freed, later) = tokio::task::spawn_blocking(move || remove_in(&root)).await.map_err(|e| e.to_string())?;
    forget();
    crate::aelog!(
        "[PACK] removed by the user: {:.1} MB freed{}",
        freed as f64 / 1e6,
        if later { "; the rest is in use and goes at the next start" } else { "" }
    );
    set_job(&app, |j| *j = IDLE);
    Ok(serde_json::json!({ "freed": freed, "later": later }))
}

async fn run_install(app: &tauri::AppHandle, file: Option<PathBuf>, background: bool) -> Result<serde_json::Value, String> {
    let root = root().ok_or("no place for the pack")?;
    if current_in(&root, &PIN).is_some() {
        if !background {
            let _ = set_wanted_in(&root, true);
        }
        set_job(app, |j| *j = Job { state: "done", got: PIN.zip_size, total: PIN.zip_size, ..IDLE });
        return Ok(status_value(true));
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        if background {
            return Err("an install is running already".into());
        }
        // The app is fetching it by itself: the user's window follows that one.
        set_job(app, |j| j.background = false);
        while BUSY.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        forget();
        if current_in(&root, &PIN).is_some() {
            let _ = set_wanted_in(&root, true);
            return Ok(status_value(true));
        }
        let j = JOB.lock().unwrap().clone();
        return Err(if j.state == "cancelled" { "cancelled".into() } else { j.error.unwrap_or_else(|| "the download stopped".into()) });
    }
    CANCEL.store(false, Ordering::Release);
    set_job(app, |j| *j = Job { state: "downloading", total: PIN.zip_size, background, ..IDLE });
    let r = install(app, &root, file, background).await;
    forget();
    let out = match r {
        Ok(()) => {
            let _ = set_wanted_in(&root, true);
            crate::aelog!("[PACK] pack {} installed in {}", PIN.version, pack_dir(&root, PIN.version).display());
            set_job(app, |j| {
                j.state = "done";
                j.got = PIN.zip_size;
                j.total = PIN.zip_size;
                j.error = None;
            });
            Ok(status_value(true))
        }
        Err(e) => {
            let cancelled = e == "cancelled";
            crate::aelog!("[PACK] pack {} not installed: {e}", PIN.version);
            set_job(app, |j| {
                j.state = if cancelled { "cancelled" } else { "error" };
                j.error = (!cancelled).then(|| e.clone());
            });
            Err(e)
        }
    };
    BUSY.store(false, Ordering::Release);
    tell(app);
    out
}

async fn install(app: &tauri::AppHandle, root: &Path, file: Option<PathBuf>, background: bool) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot make {}: {e}", root.display()))?;
    let rep: Arc<dyn Fn(&'static str, u64, u64) + Send + Sync> = {
        let app = app.clone();
        Arc::new(move |state, got, total| {
            set_job(&app, |j| {
                j.state = state;
                j.got = got;
                j.total = total;
            })
        })
    };
    let (zip, fetched) = match file {
        Some(f) => {
            crate::aelog!("[PACK] no pack {} ({}) — installing it from {}", PIN.version, why_not(), f.display());
            (f, false)
        }
        None => {
            let part = part_path(root, PIN.version);
            let have = file_len(&part).min(PIN.zip_size);
            room(root, &PIN, have)?;
            set_job(app, |j| j.from = have);
            match dev_pack() {
                Some(src) => {
                    crate::aelog!("[PACK] a test build: pack {} comes from {} (not GitHub)", PIN.version, src.display());
                    let (dst, r) = (part.clone(), rep.clone());
                    tokio::task::spawn_blocking(move || copy_in(&src, &dst, &CANCEL, &*r))
                        .await
                        .map_err(|e| e.to_string())??;
                }
                None => {
                    crate::aelog!(
                        "[PACK] {} pack {} ({}){}",
                        if background { "updating to" } else { "downloading" },
                        PIN.version,
                        why_not(),
                        if have > 0 { format!(", going on from {have} bytes") } else { String::new() }
                    );
                    fetch(&url(), &part, &PIN, NET, &CANCEL, &*rep).await?;
                }
            }
            (part, true)
        }
    };
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let _low = background.then(LowPriority::now);
        rep("verifying", PIN.zip_size, PIN.zip_size);
        if let Err(e) = verify(&zip, &PIN) {
            if fetched {
                // the next try starts clean
                let _ = std::fs::remove_file(&zip);
            }
            return Err(e);
        }
        rep("unpacking", PIN.zip_size, PIN.zip_size);
        put_in_place(&root, &zip, &PIN)?;
        if fetched {
            let _ = std::fs::remove_file(&zip);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── at start ──

/// A while after start, so that the start and the first music are not
/// slowed; a test build may say when (`AURA_PACK_UPDATE_DELAY_S`).
fn update_delay() -> Duration {
    if !is_release_build() {
        if let Some(s) = std::env::var("AURA_PACK_UPDATE_DELAY_S").ok().and_then(|v| v.parse().ok()) {
            return Duration::from_secs(s);
        }
    }
    Duration::from_secs(20)
}

/// A background update's tries in one run: at once, after 2, after 10 more
/// minutes; then the next start.
const RETRIES: [u64; 3] = [0, 120, 600];

/// At start (`setup`): what earlier runs left is cleared; then, for a user
/// who took the pack, the one this app pins is fetched in the background if
/// it is not there — which is what an app update brings.
pub fn start(app: tauri::AppHandle) {
    let Some(dir) = root() else { return };
    let gone = cleanup_in(&dir, &PIN);
    if !gone.is_empty() {
        crate::aelog!("[PACK] cleared at start: {}", gone.join(", "));
    }
    let wanted = wanted_in(&dir);
    let current = current_in(&dir, &PIN).is_some();
    let serving = active_in(&dir, &PIN).filter(|p| p.version != PIN.version);
    crate::aelog!(
        "[PACK] instruments {}; pack {} {}{}",
        if wanted { "on" } else { "off" },
        PIN.version,
        if current { "installed" } else { "not installed" },
        serving.map(|p| format!(" (pack {} serves meanwhile)", p.version)).unwrap_or_default()
    );
    if !wanted || current {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(update_delay()).await;
        for (i, wait) in RETRIES.iter().enumerate() {
            if *wait > 0 {
                tokio::time::sleep(Duration::from_secs(*wait)).await;
            }
            let Some(root) = root() else { return };
            if !wanted_in(&root) || current_in(&root, &PIN).is_some() {
                return;
            }
            if BUSY.load(Ordering::Acquire) {
                continue; // the user's own install is on
            }
            match run_install(&app, None, true).await {
                Ok(_) => return,
                Err(e) if e == "cancelled" => return,
                Err(e) => crate::aelog!("[PACK] the update to pack {} failed (try {} of {}): {e}", PIN.version, i + 1, RETRIES.len()),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

    fn sha(b: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// Small stand-ins for a pack's files; `v` makes each pack's differ.
    fn files_of(v: u32) -> Vec<(String, Vec<u8>)> {
        vec![
            (RUNTIME.into(), format!("runtime {v} ").repeat(300).into_bytes()),
            (DIRECTML.into(), b"directml ".repeat(500)),
            (MODEL.into(), (0..40_000u32).flat_map(|i| (i.wrapping_mul(2_654_435_761) ^ v).to_le_bytes()).collect()),
            (DRUMS.into(), (0..20_000u32).flat_map(|i| i.wrapping_mul(40_503).to_le_bytes()).collect()),
            ("LICENSES.txt".into(), b"licences".to_vec()),
        ]
    }

    /// A pack zip as `make_pack2.py` makes one, and the pin that fits it.
    struct Built {
        zip: Vec<u8>,
        pin: Pin,
    }

    fn build(v: u32, spec: &str, files: &[(String, Vec<u8>)], tweak: impl FnOnce(&mut serde_json::Value)) -> Built {
        let mut m = serde_json::json!({
            "pack_version": v,
            "spec_version": spec,
            "files": files.iter().map(|(n, b)| serde_json::json!({ "name": n, "size": b.len(), "sha256": sha(b) })).collect::<Vec<_>>(),
        });
        tweak(&mut m);
        let mut cur = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut cur);
            let o = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.start_file(MANIFEST, o).unwrap();
            z.write_all(m.to_string().as_bytes()).unwrap();
            for (n, b) in files {
                z.start_file(n.as_str(), o).unwrap();
                z.write_all(b).unwrap();
            }
            z.finish().unwrap();
        }
        let zip = cur.into_inner();
        let pin = Pin {
            version: v,
            spec_major: spec_major(spec),
            zip_size: zip.len() as u64,
            zip_sha256: leak(sha(&zip)),
            unpacked: files.iter().map(|(_, b)| b.len() as u64).sum(),
            url: leak(format!("http://127.0.0.1/instruments-pack-{v}/aura-instruments-pack-{v}.zip")),
        };
        Built { zip, pin }
    }

    /// Straight from the zip's bytes, as an install does after the download.
    fn put(root: &Path, b: &Built) -> Result<Pack, String> {
        let z = root.join(format!("in-{}.zip", b.pin.version));
        std::fs::write(&z, &b.zip).unwrap();
        let r = verify(&z, &b.pin).and_then(|_| put_in_place(root, &z, &b.pin));
        let _ = std::fs::remove_file(&z);
        r
    }

    fn quiet(_: &'static str, _: u64, _: u64) {}

    fn net() -> Net {
        Net { stall: Duration::from_secs(10), resume_wait: Duration::from_millis(20), tries: 3 }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// Ranges answered as asked.
        Ranges,
        /// The first answer breaks off after this many bytes.
        DropFirstAt(usize),
        /// Every answer is the whole file (a server that ignores Range).
        NoRanges,
        /// `/pack.zip` sends to `/files/pack.zip` (as GitHub sends to its storage).
        Redirect,
    }

    /// A local server for one file; `seen` = "path range" of each request.
    struct Server {
        url: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    fn serve(body: Vec<u8>, mode: Mode) -> Server {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut dropped = false;
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let Some((path, range)) = request(&s) else { continue };
                log.lock().unwrap().push(format!("{path} {}", range.map_or("-".to_string(), |r| r.to_string())));
                if mode == Mode::Redirect && path == "/pack.zip" {
                    let _ = write!(s, "HTTP/1.1 302 Found\r\nLocation: /files/pack.zip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                if path != "/pack.zip" && path != "/files/pack.zip" {
                    let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let ranged = range.is_some() && mode != Mode::NoRanges;
                let from = if ranged { range.unwrap() as usize } else { 0 };
                if from >= body.len() && ranged {
                    let _ = write!(
                        s,
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    continue;
                }
                let rest = &body[from..];
                if ranged {
                    let _ = write!(
                        s,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        from,
                        body.len() - 1,
                        body.len(),
                        rest.len()
                    );
                } else {
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", rest.len());
                }
                let send = match mode {
                    Mode::DropFirstAt(n) if !dropped => {
                        dropped = true;
                        &rest[..n.min(rest.len())]
                    }
                    _ => rest,
                };
                let _ = s.write_all(send);
                let _ = s.flush();
            }
        });
        Server { url: format!("http://{addr}/pack.zip"), seen }
    }

    fn request(s: &TcpStream) -> Option<(String, Option<u64>)> {
        let mut r = BufReader::new(s.try_clone().ok()?);
        let mut line = String::new();
        r.read_line(&mut line).ok()?;
        let path = line.split_whitespace().nth(1)?.to_string();
        let mut range = None;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).ok()? == 0 {
                break;
            }
            let h = h.trim_end().to_ascii_lowercase();
            if h.is_empty() {
                break;
            }
            if let Some(v) = h.strip_prefix("range:") {
                range = v.trim().strip_prefix("bytes=").and_then(|x| x.trim_end_matches('-').parse().ok());
            }
        }
        Some((path, range))
    }

    fn seen(s: &Server) -> Vec<String> {
        s.seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_pack_downloads_checks_and_goes_in() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let srv = serve(b.zip.clone(), Mode::Ranges);
        let part = part_path(t.path(), 2);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        verify(&part, &b.pin).unwrap();
        let p = put_in_place(t.path(), &part, &b.pin).unwrap();
        assert_eq!(p.version, 2);
        assert_eq!(p.spec_major, 2);
        assert_eq!(active_in(t.path(), &b.pin), Some(p.clone()));
        assert_eq!(p.file(DRUMS), Some(t.path().join("pack-2").join(DRUMS)));
        assert_eq!(p.file(PITCH), None, "a file this pack does not have");
        assert_eq!(std::fs::read(p.file(MODEL).unwrap()).unwrap(), files_of(2)[2].1);
        assert!(!t.path().join("pack-2.tmp").exists());
        assert_eq!(seen(&srv), ["/pack.zip -"]);
    }

    #[tokio::test]
    async fn a_download_that_breaks_off_goes_on_where_it_stopped() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let cut = b.zip.len() * 2 / 5;
        let srv = serve(b.zip.clone(), Mode::DropFirstAt(cut));
        let part = part_path(t.path(), 2);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b.zip);
        let s = seen(&srv);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0], "/pack.zip -");
        let at: u64 = s[1].strip_prefix("/pack.zip ").unwrap().parse().unwrap();
        assert!(at > 0 && at <= cut as u64, "went on from {at}, broke off at {cut}");
    }

    #[tokio::test]
    async fn a_part_left_by_an_earlier_run_is_continued() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let third = b.zip.len() / 3;
        let part = part_path(t.path(), 2);
        std::fs::write(&part, &b.zip[..third]).unwrap();
        let srv = serve(b.zip.clone(), Mode::Ranges);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b.zip);
        assert_eq!(seen(&srv), [format!("/pack.zip {third}")]);
    }

    #[tokio::test]
    async fn a_server_without_ranges_starts_over() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let half = b.zip.len() / 2;
        let part = part_path(t.path(), 2);
        std::fs::write(&part, vec![7u8; half]).unwrap(); // not even the right bytes
        let srv = serve(b.zip.clone(), Mode::NoRanges);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b.zip);
        assert_eq!(seen(&srv), [format!("/pack.zip {half}")]);
    }

    #[tokio::test]
    async fn a_redirect_keeps_the_range() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let half = b.zip.len() / 2;
        let part = part_path(t.path(), 2);
        std::fs::write(&part, &b.zip[..half]).unwrap();
        let srv = serve(b.zip.clone(), Mode::Redirect);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b.zip);
        assert_eq!(seen(&srv), [format!("/pack.zip {half}"), format!("/files/pack.zip {half}")]);
    }

    #[tokio::test]
    async fn a_whole_part_is_not_fetched_again() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let part = part_path(t.path(), 2);
        std::fs::write(&part, &b.zip).unwrap();
        let srv = serve(b.zip.clone(), Mode::Ranges);
        fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap();
        assert!(seen(&srv).is_empty());
    }

    #[tokio::test]
    async fn no_server_fails_at_once() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t0 = Instant::now();
        let e = fetch(&format!("http://127.0.0.1:{port}/pack.zip"), &part_path(t.path(), 2), &b.pin, net(), &AtomicBool::new(false), &quiet)
            .await
            .unwrap_err();
        assert!(e.starts_with("download failed"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    }

    #[tokio::test]
    async fn a_missing_file_on_the_server_says_so() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let srv = serve(b.zip.clone(), Mode::Ranges);
        let url = srv.url.replace("/pack.zip", "/other.zip");
        let e = fetch(&url, &part_path(t.path(), 2), &b.pin, net(), &AtomicBool::new(false), &quiet).await.unwrap_err();
        assert!(e.contains("HTTP 404"), "{e}");
        assert_eq!(seen(&srv).len(), 1, "a 404 is not asked again");
    }

    #[tokio::test]
    async fn cancel_keeps_what_came() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let half = b.zip.len() / 2;
        let part = part_path(t.path(), 2);
        std::fs::write(&part, &b.zip[..half]).unwrap();
        let srv = serve(b.zip.clone(), Mode::NoRanges);
        let e = fetch(&srv.url, &part, &b.pin, net(), &AtomicBool::new(true), &quiet).await.unwrap_err();
        assert_eq!(e, "cancelled");
        assert_eq!(file_len(&part), half as u64);
    }

    #[test]
    fn a_file_that_is_not_the_pack_is_never_unpacked() {
        let t = tempfile::tempdir().unwrap();
        let mut b = build(2, "2.0", &files_of(2), |_| {});
        b.pin.zip_sha256 = leak(sha(b"another zip"));
        let e = put(t.path(), &b).unwrap_err();
        assert!(e.starts_with("the file is not the published pack"), "{e}");
        assert!(entries(t.path()).is_empty());
    }

    #[test]
    fn a_bad_manifest_puts_nothing_in_place() {
        let f = files_of(2);
        let mut spec3 = build(2, "3.0", &f, |_| {});
        spec3.pin.spec_major = 2;
        let cases = vec![
            ("another pack's manifest", build(2, "2.0", &f, |m| m["pack_version"] = 3.into())),
            ("another spec", spec3),
            ("a path out of the folder", build(2, "2.0", &f, |m| m["files"][3]["name"] = "../drumsep.onnx".into())),
            ("a wrong hash", build(2, "2.0", &f, |m| m["files"][2]["sha256"] = sha(b"x").into())),
            ("a wrong size", build(2, "2.0", &f, |m| m["files"][2]["size"] = 5.into())),
            ("no runtime", build(2, "2.0", &f[1..], |_| {})),
            (
                "a file the zip does not have",
                build(2, "2.0", &f, |m| {
                    m["files"].as_array_mut().unwrap().push(serde_json::json!({ "name": PITCH, "size": 3, "sha256": sha(b"abc") }))
                }),
            ),
        ];
        for (what, b) in cases {
            let t = tempfile::tempdir().unwrap();
            let e = put(t.path(), &b).unwrap_err();
            assert!(entries(t.path()).is_empty(), "{what} ({e}): {:?}", entries(t.path()));
            assert!(active_in(t.path(), &b.pin).is_none(), "{what}");
        }
    }

    #[test]
    fn the_old_pack_serves_until_the_new_one_is_in() {
        let t = tempfile::tempdir().unwrap();
        let old = build(1, "2.0", &files_of(1), |_| {});
        put(t.path(), &old).unwrap();
        let new = build(2, "2.1", &files_of(2), |_| {});
        assert_eq!(active_in(t.path(), &new.pin).map(|p| p.version), Some(1));
        put(t.path(), &new).unwrap();
        assert_eq!(active_in(t.path(), &new.pin).map(|p| p.version), Some(2));
        assert!(pack_dir(t.path(), 1).is_dir(), "the old one stays until the next start");
        assert_eq!(cleanup_in(t.path(), &new.pin), ["pack-1"]);
        assert_eq!(active_in(t.path(), &new.pin).map(|p| p.version), Some(2));
    }

    #[test]
    fn a_pack_made_for_another_spec_never_serves() {
        for spec in ["", "1.4", "3.0"] {
            let t = tempfile::tempdir().unwrap();
            let old = build(1, spec, &files_of(1), |_| {});
            put(t.path(), &old).unwrap();
            let new = build(2, "2.0", &files_of(2), |_| {});
            assert_eq!(active_in(t.path(), &new.pin), None, "spec {spec:?}");
        }
    }

    #[test]
    fn a_damaged_pack_is_not_complete() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let p = put(t.path(), &b).unwrap();
        std::fs::write(p.file(DRUMS).unwrap(), b"short").unwrap();
        assert_eq!(active_in(t.path(), &b.pin), None);
    }

    #[test]
    fn cleanup_keeps_what_still_serves() {
        let t = tempfile::tempdir().unwrap();
        let old = build(1, "2.0", &files_of(1), |_| {});
        put(t.path(), &old).unwrap();
        let new = build(2, "2.0", &files_of(2), |_| {});
        std::fs::write(part_path(t.path(), 1), b"old part").unwrap();
        std::fs::write(part_path(t.path(), 2), b"new part").unwrap();
        std::fs::create_dir_all(t.path().join("pack-2.tmp")).unwrap();
        std::fs::write(t.path().join("pack-2.tmp").join(MODEL), b"half").unwrap();
        let mut gone = cleanup_in(t.path(), &new.pin);
        gone.sort();
        assert_eq!(gone, ["pack-1.zip.part", "pack-2.tmp"]);
        assert!(pack_dir(t.path(), 1).is_dir(), "pack 1 serves until pack 2 is in");
        assert!(part_path(t.path(), 2).is_file(), "pack 2's download goes on");
    }

    #[test]
    fn removal_frees_everything_and_stops_updates() {
        let t = tempfile::tempdir().unwrap();
        let old = build(1, "2.0", &files_of(1), |_| {});
        let new = build(2, "2.0", &files_of(2), |_| {});
        put(t.path(), &old).unwrap();
        put(t.path(), &new).unwrap();
        std::fs::write(part_path(t.path(), 3), vec![1u8; 1000]).unwrap();
        set_wanted_in(t.path(), true).unwrap();
        let bytes = disk_bytes_in(t.path());
        assert!(bytes > 1000);
        let (freed, later) = remove_in(t.path());
        assert!(!later);
        assert_eq!(freed, bytes);
        assert!(entries(t.path()).is_empty());
        assert!(!wanted_in(t.path()));
        assert!(!t.path().join(PENDING).exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_pack_in_use_goes_at_the_next_start() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        let p = put(t.path(), &b).unwrap();
        set_wanted_in(t.path(), true).unwrap();
        // as a loaded DLL: open, and no one may delete it
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(p.file(RUNTIME).unwrap()).unwrap();
        let (_, later) = remove_in(t.path());
        assert!(later);
        assert!(t.path().join(PENDING).is_file());
        assert_eq!(active_in(t.path(), &b.pin), None, "a pack deleted halfway never serves");
        assert_eq!(cleanup_in(t.path(), &b.pin), Vec::<String>::new(), "still in use");
        drop(held);
        assert_eq!(cleanup_in(t.path(), &b.pin), ["pack-2"]);
        assert!(!t.path().join(PENDING).exists());
        assert!(entries(t.path()).is_empty());
    }

    #[test]
    fn taking_the_pack_again_cancels_a_pending_removal() {
        let t = tempfile::tempdir().unwrap();
        let b = build(2, "2.0", &files_of(2), |_| {});
        put(t.path(), &b).unwrap();
        std::fs::write(t.path().join(PENDING), "").unwrap();
        set_wanted_in(t.path(), true).unwrap();
        assert!(cleanup_in(t.path(), &b.pin).is_empty());
        assert!(current_in(t.path(), &b.pin).is_some());
    }

    #[test]
    fn the_instruments_flag_is_kept() {
        let t = tempfile::tempdir().unwrap();
        assert!(!wanted_in(t.path()));
        set_wanted_in(t.path(), true).unwrap();
        assert!(wanted_in(t.path()));
        set_wanted_in(t.path(), false).unwrap();
        assert!(!wanted_in(t.path()));
        std::fs::write(t.path().join(FLAG), "{ not json").unwrap();
        assert!(!wanted_in(t.path()));
        assert!(!t.path().join(format!("{FLAG}.tmp")).exists());
    }

    #[test]
    fn content_ranges_and_spec_versions_read() {
        assert_eq!(content_range("bytes 100-199/200"), Some((100, Some(200))));
        assert_eq!(content_range(" bytes 0-0/* "), Some((0, None)));
        assert_eq!(content_range("items 1-2/3"), None);
        assert_eq!(content_range(""), None);
        assert_eq!(spec_major("2.0"), 2);
        assert_eq!(spec_major("2"), 2);
        assert_eq!(spec_major("10.3"), 10);
        assert_eq!(spec_major(""), 0);
        assert_eq!(spec_major("x"), 0);
    }

    #[test]
    fn the_pin_names_a_pack_release_not_an_app_release() {
        let tail = format!("/releases/download/instruments-pack-{0}/aura-instruments-pack-{0}.zip", PIN.version);
        assert!(PIN.url.starts_with("https://github.com/ToxaDev/aura-engine/"), "{}", PIN.url);
        assert!(PIN.url.ends_with(&tail), "{}", PIN.url);
        assert_eq!(zip_name(&PIN), format!("aura-instruments-pack-{}.zip", PIN.version));
        assert_eq!(PIN.zip_sha256.len(), 64);
        assert!(PIN.zip_sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(PIN.unpacked > PIN.zip_size);
    }

    /// The pinned numbers are the real zip's: `AURA_PACK_ZIP=<path to
    /// aura-instruments-pack-2.zip>` checks it and unpacks it (skipped
    /// without it — the zip is not in git).
    #[test]
    fn the_pinned_zip_is_the_built_one() {
        let Some(zip) = std::env::var_os("AURA_PACK_ZIP") else { return };
        let zip = PathBuf::from(zip);
        let t = tempfile::tempdir().unwrap();
        verify(&zip, &PIN).unwrap();
        let p = put_in_place(t.path(), &zip, &PIN).unwrap();
        assert_eq!((p.version, p.spec_major), (PIN.version, PIN.spec_major));
        for f in [MODEL, RUNTIME, DIRECTML, DRUMS, PITCH, "LICENSES.txt"] {
            assert!(p.file(f).is_some_and(|f| f.is_file()), "{f}");
        }
        assert_eq!(p.bytes + file_len(&p.dir.join(MANIFEST)), PIN.unpacked);
    }
}
