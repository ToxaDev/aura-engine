//! Persistent calibration of the CPU convolver's block cost per (taps, rate).
//!
//! Entries are stored in `%LOCALAPPDATA%/AuraEngine/player-calibration.json`.
//! `AURA_PLAYER_CALIB_FILE` names another file (empty = keep the store in
//! memory); a test build never touches the real one.
//! The machine identity (CPU brand + the number of CPUs this process may run
//! on) and the app version (major.minor) are checked on load; if they differ,
//! all entries are discarded so stale measurements from a different machine,
//! a narrower affinity mask or a breaking update do not affect routing
//! decisions.
//!
//! Key format: `"{taps_label}:{out_rate}"` for single-stream chains,
//! `"{taps_label}:{out_rate}:HP"` for Hybrid-Phase / alpha-HP pairs.
//!
//! Three writers: the render thread (a live 2-second window, `update`, no
//! I/O), the pre-trial (`insert_trial`) and the mid-track deficit escalation
//! (`record_deficit`). The file is written by whoever takes a [`FlushJob`],
//! outside the store's lock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::player::convolver::BLOCK;

/// Entries older than this are discarded on load.
const EXPIRY_SECS: u64 = 30 * 24 * 3600;

/// EMA weight for live updates of a confirmed entry.
const EMA_ALPHA_STABLE: f64 = 0.1;

/// Lifetime of an entry written by the mid-track deficit escalation (K3). A
/// key routed away from the CPU is not measured there again, so a transient
/// load must not pin it for a month.
pub const K3_TTL_SECS: u64 = 12 * 3600;

/// Whole chain / convolver alone. The pre-trial times the convolver blocks
/// only; the live windows time the whole chain. To be set by the bench (B4).
/// Until B4 runs, 1.25 is the estimate from live chains against the
/// convolver's steady blocks (30M 1.15-1.23, 10M 1.21-1.37). The old 1.10
/// sat on a trial block that paid its own page faults (22-46 % over steady);
/// the trial now maps its pages before timing, so the factor carries the
/// whole gap.
pub const TRIAL_CHAIN_FACTOR: f64 = 1.25;

/// Unforced flushes are at most this frequent.
const FLUSH_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// A live window that comes more than this after the previous one is not
/// used. After idle the first window holds a single step, and a fresh chain's
/// first step runs a whole subsonic-guard block while being credited one step.
const LIVE_GAP: Duration = Duration::from_secs(5);

/// A live window more than this factor away from the entry is discarded.
const LIVE_OUTLIER_RATIO: f64 = 8.0;

/// Sequence of the flush snapshots of every store in the process.
static FLUSH_SEQ: AtomicU64 = AtomicU64::new(0);

/// The newest snapshot written to each file. Writes go one at a time, and a
/// snapshot older than the one already written is skipped.
static WRITTEN: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());

/// One calibration entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibEntry {
    /// Wall time of one full OLA block (all L branches, stereo), milliseconds.
    pub block_cost_ms: f64,
    /// Unix timestamp (seconds) when this entry was last written.
    pub timestamp_secs: u64,
    /// True when set from a pre-trial measurement taken under contention, not
    /// yet confirmed by a live 2-second RTF window from the render thread.
    /// Never written to disk.
    pub provisional: bool,
    /// Lifetime in seconds; `None` = `EXPIRY_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl CalibEntry {
    fn fresh_at(&self, now_secs: u64) -> bool {
        now_secs.saturating_sub(self.timestamp_secs) < self.ttl_secs.unwrap_or(EXPIRY_SECS)
    }
}

/// On-disk JSON format.
#[derive(Serialize, Deserialize)]
struct StoreFile {
    machine: String,
    version: String,
    entries: HashMap<String, CalibEntry>,
}

/// Calibration store: loaded once at startup, updated by the render thread
/// (CPU chains only, K2), queried at chain-build time by the controller.
///
/// Wrap in `Arc<Mutex<_>>` for shared access.
pub struct CalibrationStore {
    machine: String,
    version: String,
    entries: HashMap<String, CalibEntry>,
    /// True when entries have been modified since the last flush.
    dirty: bool,
    /// The file; `None` keeps the store in memory.
    path: Option<PathBuf>,
    /// When the last [`FlushJob`] was taken.
    last_flush: Option<Instant>,
    /// Key and time of the last live window, used or not (the live filter).
    last_live: Option<(String, Instant)>,
    /// Live windows closed before this are dropped (see [`hold_live`]).
    ///
    /// [`hold_live`]: CalibrationStore::hold_live
    live_hold: Option<Instant>,
}

/// A serialised snapshot of the store, taken under its lock and written
/// outside it.
pub struct FlushJob {
    path: PathBuf,
    json: String,
    /// Order of the snapshot among all taken in the process.
    seq: u64,
    /// Entries in the snapshot.
    pub entries: usize,
}

impl FlushJob {
    /// Write `<file>.tmp`, then rename it over the file, so a reader never
    /// sees half of one. Returns the bytes written; 0 when a newer snapshot
    /// of the store is already in the file (this one is skipped).
    ///
    /// Jobs are taken under the store's lock but written after it is
    /// released, from several threads: the writes go one at a time, so two
    /// never share the temporary, and an older snapshot never replaces a
    /// newer one.
    pub fn write(self) -> std::io::Result<usize> {
        let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        let last = written.iter().position(|(p, _)| *p == self.path);
        if last.map_or(false, |i| written[i].1 > self.seq) {
            return Ok(0);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, self.json.as_bytes())?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        match last {
            Some(i) => written[i].1 = self.seq,
            None => written.push((self.path.clone(), self.seq)),
        }
        Ok(self.json.len())
    }
}

impl CalibrationStore {
    /// Load from the calibration file, discarding entries whose
    /// machine/version/expiry do not match.  Never fails — returns an empty
    /// store on any error.
    pub fn load() -> Self {
        Self::load_from(calib_path())
    }

    /// Load from `path` (`None` = an empty store kept in memory). Later
    /// flushes go to the same path.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let machine = machine_id();
        let version = app_version();
        let mut store = CalibrationStore {
            machine: machine.clone(),
            version: version.clone(),
            entries: HashMap::new(),
            dirty: false,
            path,
            last_flush: None,
            last_live: None,
            live_hold: None,
        };
        let Some(path) = store.path.as_ref() else { return store; };
        let Ok(text) = std::fs::read_to_string(path) else { return store; };
        let Ok(f): Result<StoreFile, _> = serde_json::from_str(&text) else { return store; };
        if f.machine != machine || f.version != version { return store; }
        let now = unix_secs();
        store.entries = f.entries.into_iter()
            .filter(|(_, e)| e.fresh_at(now))
            .collect();
        store
    }

    /// Write to disk now when dirty (tests, shutdown).  Silent on error; a
    /// failed write leaves the store dirty.
    #[allow(dead_code)] // for a shutdown hook; the store also writes on change
    pub fn flush(&mut self) {
        if let Some(job) = self.take_flush(true) {
            if job.write().is_err() {
                self.mark_dirty();
            }
        }
    }

    /// Snapshot for a write, or `None` when there is nothing to write, no
    /// file, or (unless `force`) the last snapshot is younger than
    /// `FLUSH_MIN_INTERVAL`. Provisional entries are left out. Cheap: the
    /// caller writes the job after releasing the lock.
    pub fn take_flush(&mut self, force: bool) -> Option<FlushJob> {
        self.take_flush_at(force, Instant::now())
    }

    fn take_flush_at(&mut self, force: bool, now: Instant) -> Option<FlushJob> {
        if !self.dirty {
            return None;
        }
        let path = self.path.clone()?;
        if !force && self.last_flush.map_or(false, |t| now.saturating_duration_since(t) < FLUSH_MIN_INTERVAL) {
            return None;
        }
        let now_s = unix_secs();
        let entries: HashMap<String, CalibEntry> = self.entries.iter()
            .filter(|(_, e)| !e.provisional && e.fresh_at(now_s))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        let n = entries.len();
        let f = StoreFile {
            machine: self.machine.clone(),
            version: self.version.clone(),
            entries,
        };
        let json = serde_json::to_string_pretty(&f).ok()?;
        self.dirty = false;
        self.last_flush = Some(now);
        let seq = FLUSH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        Some(FlushJob { path, json, seq, entries: n })
    }

    /// A taken [`FlushJob`] failed to write: write again at the next flush.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// The entry for `key` while it is fresh (younger than its lifetime).
    pub fn entry(&self, key: &str) -> Option<&CalibEntry> {
        let now = unix_secs();
        self.entries.get(key).filter(|e| e.fresh_at(now))
    }

    /// Return the raw `block_cost_ms` for `key`, or `None` if missing or
    /// expired.
    pub fn block_cost_ms(&self, key: &str) -> Option<f64> {
        self.entry(key).map(|e| e.block_cost_ms)
    }

    /// Predicted CPU RTF for `key` at the source sample rate `src_rate`.
    ///
    /// Formula (K2):
    ///     RTF = (BLOCK / src_rate) / (block_cost_ms / 1000)
    pub fn rtf_for(&self, key: &str, src_rate: u32) -> Option<f64> {
        let ms = self.block_cost_ms(key)?;
        Some(rtf_from_cost(ms, src_rate))
    }

    /// Update an entry with a live `block_cost_ms` measurement from the render
    /// thread (K2).  Must NOT be called for GPU chains.  No I/O.
    ///
    /// Only a clean window is used: one that follows a window of the same key
    /// within `LIVE_GAP`. The first window ever, the first after a key change
    /// and the first after a pause are dropped (they may hold one step, or
    /// two chains).
    ///
    /// - Missing (or expired) entry → inserted directly.
    /// - More than `LIVE_OUTLIER_RATIO` away from the entry → dropped.
    /// - Provisional entry → replaced, and confirmed.
    /// - Stable entry      → EMA, alpha = EMA_ALPHA_STABLE.
    /// - An entry with a lifetime of its own (K3) keeps it and its
    ///   timestamp: the windows after a fire come from the chain that fell
    ///   behind, still rendering until the swap, and measure the same
    ///   deficit. It expires `K3_TTL_SECS` after the fire.
    pub fn update(&mut self, key: &str, block_cost_ms: f64) {
        self.update_at(key, block_cost_ms, Instant::now());
    }

    /// Drop the live windows that close before `until`, and the first one
    /// after it (it may hold some of that time). A pre-trial or a K3 build
    /// beside the playing chain slows it down; that is not the chain's cost.
    pub fn hold_live(&mut self, until: Instant) {
        self.live_hold = Some(until);
        self.last_live = None;
    }

    fn update_at(&mut self, key: &str, block_cost_ms: f64, now: Instant) {
        if !(block_cost_ms.is_finite() && block_cost_ms > 0.0) {
            return;
        }
        if self.live_hold.map_or(false, |t| now < t) {
            self.last_live = None;
            return;
        }
        let same_key = matches!(&self.last_live, Some((k, _)) if k == key);
        let clean = same_key
            && self.last_live.as_ref()
                .map_or(false, |(_, t)| now.saturating_duration_since(*t) < LIVE_GAP);
        if same_key {
            if let Some((_, t)) = self.last_live.as_mut() {
                *t = now;
            }
        } else {
            self.last_live = Some((key.to_string(), now));
        }
        if !clean {
            return;
        }

        let now_s = unix_secs();
        match self.entries.get_mut(key).filter(|e| e.fresh_at(now_s)) {
            Some(e) => {
                let ratio = block_cost_ms / e.block_cost_ms;
                if !(ratio >= 1.0 / LIVE_OUTLIER_RATIO && ratio <= LIVE_OUTLIER_RATIO) {
                    return;
                }
                if e.provisional {
                    e.block_cost_ms = block_cost_ms;
                    e.provisional = false;
                } else {
                    e.block_cost_ms = EMA_ALPHA_STABLE * block_cost_ms
                        + (1.0 - EMA_ALPHA_STABLE) * e.block_cost_ms;
                }
                if e.ttl_secs.is_none() {
                    e.timestamp_secs = now_s;
                }
            }
            None => {
                self.entries.insert(key.to_string(), CalibEntry {
                    block_cost_ms,
                    timestamp_secs: now_s,
                    provisional: false,
                    ttl_secs: None,
                });
            }
        }
        self.dirty = true;
    }

    /// Insert a provisional entry.  Used before the first live render window
    /// when only a pre-trial measurement is available.
    #[cfg(test)]
    pub fn insert_provisional(&mut self, key: String, block_cost_ms: f64) {
        self.insert_trial(key, block_cost_ms, true);
    }

    /// Insert a pre-trial measurement (K2). `provisional` when it was taken
    /// under contention: the first clean live window then replaces it.
    pub fn insert_trial(&mut self, key: String, block_cost_ms: f64, provisional: bool) {
        self.entries.insert(key, CalibEntry {
            block_cost_ms,
            timestamp_secs: unix_secs(),
            provisional,
            ttl_secs: None,
        });
        self.dirty = true;
    }

    /// Record the cost the mid-track deficit escalation (K3) observed. It
    /// replaces the entry, confirmed, for `K3_TTL_SECS`, so every later build
    /// of this key routes away from the rung that fell behind.
    pub fn record_deficit(&mut self, key: String, block_cost_ms: f64) {
        self.entries.insert(key, CalibEntry {
            block_cost_ms,
            timestamp_secs: unix_secs(),
            provisional: false,
            ttl_secs: Some(K3_TTL_SECS),
        });
        self.dirty = true;
    }
}

// ── Public helpers ───────────────────────────────────────────────────────────

/// Compute RTF from a block cost and a source sample rate.
///
/// RTF = (BLOCK / src_rate) / (block_cost_ms / 1000)
pub fn rtf_from_cost(block_cost_ms: f64, src_rate: u32) -> f64 {
    let block_dur_s = BLOCK as f64 / src_rate as f64;
    block_dur_s * 1000.0 / block_cost_ms
}

/// Block cost of a chain that renders `rtf` × real time at `out_rate` with
/// `l` branches: the render thread's conversion of a live window, and the
/// inverse of [`rtf_from_cost`] at `out_rate / l`.
pub fn cost_from_rtf(rtf: f64, l: usize, out_rate: u32) -> f64 {
    let block_dur_s = BLOCK as f64 * l as f64 / out_rate as f64;
    block_dur_s / rtf * 1000.0
}

/// The calibration key of a chain, in the format chain.rs gives `calib_key`:
/// `"{taps_label}:{out_rate}"`, with `":HP"` for a Hybrid-Phase / alpha-HP
/// pair. `None` for a tap count without a ladder label.
pub fn key(taps: usize, out_rate: u32, pair: bool) -> Option<String> {
    let label = crate::audio::converter::dsp::filter::taps_label(taps)?;
    Some(format!("{}:{}{}", label, out_rate, if pair { ":HP" } else { "" }))
}

/// CPUs this process may run on: the popcount of its affinity mask, which a
/// user or a launcher can narrow. Above 64 CPUs only the current processor
/// group is seen. Falls back to `available_parallelism`.
pub fn cpu_width() -> usize {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::GetCurrentProcess;
        use winapi::um::winbase::GetProcessAffinityMask;
        let mut process: usize = 0;
        let mut system: usize = 0;
        if GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) != 0 && process != 0 {
            return process.count_ones() as usize;
        }
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Shared handle passed to the render thread for live calibration updates.
pub type CalibHandle = Arc<Mutex<CalibrationStore>>;

/// Create and return a new shared calibration store, loaded from disk.
pub fn new_handle() -> CalibHandle {
    Arc::new(Mutex::new(CalibrationStore::load()))
}

// ── Private helpers ──────────────────────────────────────────────────────────

fn calib_path() -> Option<PathBuf> {
    resolve_calib_path(std::env::var_os("AURA_PLAYER_CALIB_FILE"))
}

/// `AURA_PLAYER_CALIB_FILE` wins (empty = no file); otherwise a test build has
/// no file and the app keeps it under `%LOCALAPPDATA%\AuraEngine`.
fn resolve_calib_path(over: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(p) = over {
        return if p.is_empty() { None } else { Some(PathBuf::from(p)) };
    }
    if cfg!(test) {
        return None;
    }
    crate::app_dir::root().map(|d| d.join("player-calibration.json"))
}

/// `"{brand}@{cpu_width}"`: unmasked it is the logical core count as before;
/// a narrowed affinity mask is a different machine.
pub fn machine_id() -> String {
    use sysinfo::{CpuExt, SystemExt};
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu();
    let brand = sys.cpus().first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{}@{}", brand, cpu_width())
}

fn app_version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    let mut parts = v.splitn(3, '.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    format!("{}.{}", major, minor)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_store() -> CalibrationStore {
        CalibrationStore {
            machine: "test-cpu@8".into(),
            version: "1.3".into(),
            entries: HashMap::new(),
            dirty: false,
            path: None,
            last_flush: None,
            last_live: None,
            live_hold: None,
        }
    }

    /// K2 formula: 30M filter, 44.1 kHz source, 75 ms block cost → RTF ≈ 9.9.
    #[test]
    fn rtf_formula_k2() {
        let rtf = rtf_from_cost(75.0, 44100);
        // RTF = (32768 / 44100) / 0.075 = 0.74285 / 0.075 ≈ 9.904
        assert!((rtf - 9.904).abs() < 0.01, "RTF={rtf:.4}");
    }

    /// Round-trip through JSON: entry survives serialize / deserialize.
    #[test]
    fn calib_roundtrip() {
        let dir = std::env::temp_dir()
            .join(format!("aura_calib_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("player-calibration.json");

        let mut store = blank_store();
        store.insert_provisional("1M:352800".into(), 12.5);

        let f = StoreFile {
            machine: store.machine.clone(),
            version: store.version.clone(),
            entries: store.entries.clone(),
        };
        std::fs::write(&path, serde_json::to_string_pretty(&f).unwrap()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let f2: StoreFile = serde_json::from_str(&text).unwrap();
        let e = f2.entries.get("1M:352800").expect("entry missing");
        assert!((e.block_cost_ms - 12.5).abs() < 0.001, "cost={}", e.block_cost_ms);
        assert!(e.provisional, "should still be provisional");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Provisional → the first window is dropped (first ever) → the first clean
    /// window replaces it → stable EMA (alpha 0.1).
    #[test]
    fn ema_update() {
        let mut store = blank_store();
        store.insert_provisional("1M:352800".into(), 50.0);

        // First window ever: dropped.
        store.update("1M:352800", 20.0);
        let e = &store.entries["1M:352800"];
        assert!(e.provisional, "the first window must not confirm");
        assert!((e.block_cost_ms - 50.0).abs() < f64::EPSILON);

        // First clean window: replaces the provisional value.
        store.update("1M:352800", 20.0);
        let e = &store.entries["1M:352800"];
        assert!(!e.provisional, "must be confirmed after the first clean window");
        assert!((e.block_cost_ms - 20.0).abs() < 1e-9, "replace={:.4}", e.block_cost_ms);

        // Stable: alpha = 0.1 → new = 0.1×10 + 0.9×20 = 1 + 18 = 19
        store.update("1M:352800", 10.0);
        let e = &store.entries["1M:352800"];
        assert!((e.block_cost_ms - 19.0).abs() < 1e-9, "EMA stable={:.4}", e.block_cost_ms);
    }

    /// Missing key → the first window is dropped, the second inserted with no EMA.
    #[test]
    fn update_missing_key() {
        let mut store = blank_store();
        store.update("5k:352800", 5.0);
        assert!(store.entries.get("5k:352800").is_none(), "first window ever must be dropped");
        store.update("5k:352800", 5.0);
        let e = &store.entries["5k:352800"];
        assert!((e.block_cost_ms - 5.0).abs() < f64::EPSILON);
        assert!(!e.provisional);
    }

    /// rtf_for returns None for an expired entry.
    #[test]
    fn expired_entry_returns_none() {
        let mut store = blank_store();
        store.entries.insert("1M:352800".into(), CalibEntry {
            block_cost_ms: 50.0,
            timestamp_secs: 0,   // far in the past
            provisional: false,
            ttl_secs: None,
        });
        assert!(store.rtf_for("1M:352800", 44100).is_none(), "expired entry must be None");
    }

    /// The first window ever is dropped; so is the first after a key change;
    /// the next clean window replaces a provisional entry.
    #[test]
    fn first_window_dropped_then_replaces() {
        let mut store = blank_store();
        store.insert_trial("30M:352800".into(), 80.0, true);
        store.insert_trial("10M:352800".into(), 30.0, true);

        store.update("30M:352800", 100.0);            // first ever
        assert!(store.entries["30M:352800"].provisional);
        assert!((store.entries["30M:352800"].block_cost_ms - 80.0).abs() < f64::EPSILON);

        store.update("10M:352800", 40.0);             // key change
        assert!(store.entries["10M:352800"].provisional);
        assert!((store.entries["10M:352800"].block_cost_ms - 30.0).abs() < f64::EPSILON);

        store.update("10M:352800", 40.0);             // clean
        let e = &store.entries["10M:352800"];
        assert!(!e.provisional);
        assert!((e.block_cost_ms - 40.0).abs() < f64::EPSILON);

        store.update("30M:352800", 100.0);            // key change again
        assert!(store.entries["30M:352800"].provisional, "a window after a key change must be dropped");
        store.update("30M:352800", 100.0);
        assert!(!store.entries["30M:352800"].provisional);
        assert!((store.entries["30M:352800"].block_cost_ms - 100.0).abs() < f64::EPSILON);
    }

    /// A window more than LIVE_GAP after the previous one is dropped; the one
    /// after it is clean again.
    #[test]
    fn gap_drops_window() {
        let mut store = blank_store();
        let k = "1M:352800";
        let t0 = Instant::now();
        store.update_at(k, 10.0, t0);                              // first ever
        store.update_at(k, 10.0, t0 + Duration::from_secs(2));     // inserted
        assert!((store.entries[k].block_cost_ms - 10.0).abs() < f64::EPSILON);
        store.update_at(k, 20.0, t0 + Duration::from_secs(8));     // 6 s gap: dropped
        assert!((store.entries[k].block_cost_ms - 10.0).abs() < f64::EPSILON, "a window after a gap must be dropped");
        store.update_at(k, 20.0, t0 + Duration::from_secs(10));    // clean: EMA
        assert!((store.entries[k].block_cost_ms - 11.0).abs() < 1e-9, "EMA={}", store.entries[k].block_cost_ms);
    }

    /// A window ×8 away from the entry is dropped; a sane one is blended.
    #[test]
    fn outlier_rejected() {
        let mut store = blank_store();
        let k = "30M:352800";
        store.insert_trial(k.into(), 100.0, false);
        store.update(k, 100.0);                   // first ever: dropped
        store.update(k, 1000.0);                  // ×10: dropped
        assert!((store.entries[k].block_cost_ms - 100.0).abs() < f64::EPSILON, "outlier applied");
        store.update(k, 150.0);                   // 0.1×150 + 0.9×100
        assert!((store.entries[k].block_cost_ms - 105.0).abs() < 1e-9, "EMA={}", store.entries[k].block_cost_ms);
    }

    /// Forced → a file, written through a temporary that does not stay behind,
    /// without the provisional entries; unforced within FLUSH_MIN_INTERVAL → nothing.
    #[test]
    fn flush_throttle_and_atomic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("player-calibration.json");
        let mut store = CalibrationStore::load_from(Some(path.clone()));
        assert!(store.take_flush(true).is_none(), "clean store must not flush");

        store.insert_trial("30M:352800".into(), 80.0, false);
        store.insert_trial("10M:352800".into(), 30.0, true);
        let job = store.take_flush(true).expect("forced flush");
        assert_eq!(job.entries, 1);
        let bytes = job.write().expect("write");
        assert!(bytes > 0);
        assert!(path.exists(), "file not written");
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists(), "temporary left behind");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("30M:352800"));
        assert!(!text.contains("10M:352800"), "provisional entry written");

        store.insert_trial("5M:352800".into(), 50.0, false);
        let t = store.last_flush.unwrap();
        assert!(store.take_flush_at(false, t + Duration::from_secs(9)).is_none(), "throttle ignored");
        assert!(store.dirty, "a throttled flush must stay dirty");
        let job = store.take_flush_at(false, t + Duration::from_secs(11)).expect("unforced flush after the interval");
        assert_eq!(job.entries, 2);
        job.write().unwrap();

        let back = CalibrationStore::load_from(Some(path.clone()));
        assert!(back.entry("30M:352800").is_some());
        assert!(back.entry("5M:352800").is_some());
        assert!(back.entry("10M:352800").is_none());

        // No file → never a job.
        let mut mem = blank_store();
        mem.insert_trial("1M:352800".into(), 5.0, false);
        assert!(mem.take_flush(true).is_none());
    }

    /// A K3 entry lives `ttl_secs`; the default lifetime applies without one.
    #[test]
    fn ttl_expires_deficit_entry() {
        let mut store = blank_store();
        store.record_deficit("30M:384000:HP".into(), 90.0);
        let e = store.entry("30M:384000:HP").expect("fresh deficit entry");
        assert_eq!(e.ttl_secs, Some(K3_TTL_SECS));
        assert!(!e.provisional);

        store.entries.insert("10M:384000".into(), CalibEntry {
            block_cost_ms: 30.0,
            timestamp_secs: unix_secs() - 2,
            provisional: false,
            ttl_secs: Some(1),
        });
        assert!(store.entry("10M:384000").is_none(), "ttl 1 s, age 2 s must be expired");
        assert!(store.rtf_for("10M:384000", 48_000).is_none());

        store.entries.insert("5M:384000".into(), CalibEntry {
            block_cost_ms: 20.0,
            timestamp_secs: unix_secs() - 2,
            provisional: false,
            ttl_secs: None,
        });
        assert!(store.entry("5M:384000").is_some());
    }

    /// The chain that fell behind keeps rendering until the swap: its clean
    /// windows blend into the K3 entry but keep its lifetime and timestamp.
    #[test]
    fn deficit_entry_keeps_ttl_through_live_windows() {
        let mut store = blank_store();
        let k = "30M:352800:HP";
        store.record_deficit(k.into(), 90.0);
        let fired_at = unix_secs() - 100;
        store.entries.get_mut(k).unwrap().timestamp_secs = fired_at;
        let t0 = Instant::now();
        store.update_at(k, 100.0, t0);                                  // first ever: dropped
        store.update_at(k, 100.0, t0 + Duration::from_secs(2));         // clean: EMA
        store.update_at(k, 100.0, t0 + Duration::from_secs(4));         // clean: EMA
        let e = store.entry(k).expect("K3 entry");
        assert_eq!(e.ttl_secs, Some(K3_TTL_SECS), "a live window cleared the K3 lifetime");
        assert_eq!(e.timestamp_secs, fired_at, "a live window moved the K3 timestamp");
        assert!(!e.provisional);
        // 0.1×100 + 0.9×90 = 91, then 0.1×100 + 0.9×91 = 91.9
        assert!((e.block_cost_ms - 91.9).abs() < 1e-9, "EMA={}", e.block_cost_ms);

        // An ordinary entry is refreshed.
        let k2 = "10M:352800";
        store.insert_trial(k2.into(), 30.0, false);
        store.entries.get_mut(k2).unwrap().timestamp_secs = fired_at;
        store.update_at(k2, 30.0, t0 + Duration::from_secs(6));         // key change: dropped
        store.update_at(k2, 30.0, t0 + Duration::from_secs(8));
        assert!(store.entries[k2].timestamp_secs > fired_at);
    }

    /// Windows that close during a hold are dropped, and so is the first
    /// after it; the next one is used.
    #[test]
    fn hold_live_drops_windows() {
        let mut store = blank_store();
        let k = "1M:352800";
        let t0 = Instant::now();
        store.update_at(k, 10.0, t0);                                   // first ever
        store.update_at(k, 10.0, t0 + Duration::from_secs(2));          // inserted
        store.hold_live(t0 + Duration::from_secs(5));
        store.update_at(k, 40.0, t0 + Duration::from_secs(4));          // held
        assert!((store.entries[k].block_cost_ms - 10.0).abs() < f64::EPSILON, "a held window was used");
        store.update_at(k, 40.0, t0 + Duration::from_secs(6));          // first after the hold
        assert!((store.entries[k].block_cost_ms - 10.0).abs() < f64::EPSILON, "the window after the hold was used");
        store.update_at(k, 20.0, t0 + Duration::from_secs(8));          // clean: EMA
        assert!((store.entries[k].block_cost_ms - 11.0).abs() < 1e-9, "EMA={}", store.entries[k].block_cost_ms);

        // Released at once (a hold that ends now): the next window still is
        // the first after it.
        store.hold_live(t0 + Duration::from_secs(9));
        store.update_at(k, 20.0, t0 + Duration::from_secs(10));
        assert!((store.entries[k].block_cost_ms - 11.0).abs() < 1e-9);
        store.update_at(k, 20.0, t0 + Duration::from_secs(12));
        assert!((store.entries[k].block_cost_ms - 11.9).abs() < 1e-9, "EMA={}", store.entries[k].block_cost_ms);
    }

    /// Two snapshots written in the wrong order: the older one is skipped
    /// and the file keeps the newer.
    #[test]
    fn older_flush_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("player-calibration.json");
        let mut store = CalibrationStore::load_from(Some(path.clone()));
        store.insert_trial("30M:352800".into(), 80.0, false);
        let older = store.take_flush(true).expect("first snapshot");
        store.insert_trial("10M:352800".into(), 30.0, false);
        let newer = store.take_flush(true).expect("second snapshot");
        assert!(newer.write().unwrap() > 0);
        assert_eq!(older.write().unwrap(), 0, "an older snapshot replaced a newer one");
        let back = CalibrationStore::load_from(Some(path));
        assert!(back.entry("30M:352800").is_some());
        assert!(back.entry("10M:352800").is_some());
    }

    /// `ttl_secs` survives a write and a load; a file written before the field
    /// existed still loads.
    #[test]
    fn load_keeps_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("player-calibration.json");
        let mut store = CalibrationStore::load_from(Some(path.clone()));
        store.record_deficit("30M:352800:HP".into(), 90.0);
        store.insert_trial("1M:352800".into(), 5.0, false);
        store.take_flush(true).unwrap().write().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("ttl_secs").count(), 1, "ttl_secs only where set:\n{text}");

        let back = CalibrationStore::load_from(Some(path.clone()));
        assert_eq!(back.entry("30M:352800:HP").unwrap().ttl_secs, Some(K3_TTL_SECS));
        assert_eq!(back.entry("1M:352800").unwrap().ttl_secs, None);

        let old = serde_json::json!({
            "machine": machine_id(),
            "version": app_version(),
            "entries": {
                "1M:352800": { "block_cost_ms": 12.5, "timestamp_secs": unix_secs(), "provisional": false }
            }
        });
        std::fs::write(&path, old.to_string()).unwrap();
        let back = CalibrationStore::load_from(Some(path));
        let e = back.entry("1M:352800").expect("old file must load");
        assert!((e.block_cost_ms - 12.5).abs() < f64::EPSILON);
        assert_eq!(e.ttl_secs, None);
    }

    /// `key()` builds the string chain.rs builds for `calib_key`.
    #[test]
    fn key_matches_chain_format() {
        use crate::audio::converter::dsp::filter::{taps_label, TAP_LADDER};
        assert_eq!(key(30_000_000, 352_800, true).as_deref(), Some("30M:352800:HP"));
        assert_eq!(key(1_000_000, 384_000, false).as_deref(), Some("1M:384000"));
        for taps in TAP_LADDER {
            for pair in [false, true] {
                let hp_suffix = if pair { ":HP" } else { "" };
                let chain = format!("{}:{}{}", taps_label(taps).unwrap_or("?"), 352_800u32, hp_suffix);
                assert_eq!(key(taps, 352_800, pair), Some(chain));
            }
        }
        assert_eq!(key(1_000, 352_800, false), None);
    }

    /// `cost_from_rtf` and `rtf_from_cost` are inverses at `out_rate / L`.
    #[test]
    fn cost_rtf_inverse() {
        for (l, out_rate) in [(8usize, 352_800u32), (2, 384_000), (4, 192_000)] {
            for rtf in [0.64, 1.0, 1.5, 9.9] {
                let ms = cost_from_rtf(rtf, l, out_rate);
                let back = rtf_from_cost(ms, out_rate / l as u32);
                assert!((back - rtf).abs() < 1e-9, "L {l} rate {out_rate}: {rtf} → {ms} ms → {back}");
            }
        }
    }

    /// A test build never resolves the real file; the override wins.
    #[test]
    fn test_build_has_no_path() {
        assert_eq!(resolve_calib_path(None), None);
        assert_eq!(resolve_calib_path(Some("".into())), None);
        assert_eq!(resolve_calib_path(Some("x.json".into())), Some(PathBuf::from("x.json")));
        if std::env::var_os("AURA_PLAYER_CALIB_FILE").is_none() {
            assert!(CalibrationStore::load().path.is_none());
        }
    }

    #[test]
    fn cpu_width_in_range() {
        let w = cpu_width();
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert!(w >= 1 && w <= n.max(64), "cpu_width {w}, available {n}");
    }

    #[test]
    fn machine_id_format() {
        let id = machine_id();
        let (brand, width) = id.rsplit_once('@').expect("brand@width");
        assert!(!brand.is_empty(), "{id}");
        assert_eq!(width.parse::<usize>().ok(), Some(cpu_width()), "{id}");
    }
}
