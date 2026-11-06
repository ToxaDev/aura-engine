//! What a session was doing when it stopped being heard from.
//!
//! The heavy threads (the player's control thread and preparations, the
//! converter, the card, the output) leave a marker: what stage they are in,
//! how far along, when that last changed. Atomics only, so a marker on a hot
//! path costs a few loads and a thread stuck anywhere still has it readable.
//!
//! A thread of its own looks at them every 2 s and pushes the session log to
//! the disk (a machine that freezes keeps what was written up to then). A
//! marker that has not changed for 30 s while its stage is busy gets one
//! `[WATCHDOG] stall` line with every marker and the free memory.
//!
//! Its lines go through its own handle of the session file, past `aelog!`:
//! that macro waits for the log's lock, and a hang inside the log is one of
//! the hangs this is here to record. So nothing here takes `lock_out`, and
//! nothing here holds a lock across a call that may log.
//!
//! The `running` file in the app's data folder says a session is under way.
//! A clean exit takes it away; found at the next start, it names the session
//! that ended without one (the report offered then is about that session).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long a busy marker may stand still before the stall line.
const STALL_MS: u64 = 30_000;
/// How often the log goes to the disk and the markers are looked at.
const TICK: Duration = Duration::from_secs(2);
/// How often the memory line is written while something runs.
const MEM_EVERY: Duration = Duration::from_secs(10);

/// Milliseconds since the program started (never 0 once read, so "not yet"
/// has a value of its own).
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// How long this run has been up.
pub fn uptime_s() -> u64 {
    now_ms() / 1000
}

/// One heavy area's slot.
pub struct Marker {
    name: &'static str,
    /// How many are inside (`enter`); 0 is idle.
    depth: AtomicU32,
    /// The last stage entered. A pointer to a `&'static str`, so it is read
    /// and written whole.
    stage: AtomicPtr<&'static str>,
    progress: AtomicU64,
    changed_ms: AtomicU64,
    /// The stall line of this standstill has been written.
    told: AtomicBool,
}

impl Marker {
    const fn new(name: &'static str) -> Marker {
        Marker {
            name,
            depth: AtomicU32::new(0),
            stage: AtomicPtr::new(std::ptr::null_mut()),
            progress: AtomicU64::new(0),
            changed_ms: AtomicU64::new(0),
            told: AtomicBool::new(false),
        }
    }

    fn changed(&self) {
        self.changed_ms.store(now_ms(), Ordering::Relaxed);
        self.told.store(false, Ordering::Relaxed);
    }

    fn set_stage(&self, stage: &'static &'static str) {
        self.stage.store(stage as *const &'static str as *mut &'static str, Ordering::Relaxed);
    }

    /// In `stage` until the returned guard goes (`MARK.enter(&"hp lists")`).
    /// Several threads may be inside at once: the marker is busy until the
    /// last one leaves, and names the stage entered last.
    pub fn enter(&'static self, stage: &'static &'static str) -> Busy {
        self.set_stage(stage);
        self.progress.store(0, Ordering::Relaxed);
        self.changed();
        self.depth.fetch_add(1, Ordering::Relaxed);
        Busy(self)
    }

    /// How far along; only a value that differs counts as movement.
    pub fn progress(&self, value: u64) {
        if self.progress.swap(value, Ordering::Relaxed) != value {
            self.changed();
        }
    }

    /// One more step of something that has no number of its own.
    pub fn tick(&self) {
        self.progress.fetch_add(1, Ordering::Relaxed);
        self.changed();
    }

    /// For a loop that is its own marker: busy in `stage` at `value`, or idle.
    pub fn hold(&self, busy: Option<(&'static &'static str, u64)>) {
        match busy {
            Some((stage, value)) => {
                if self.depth.swap(1, Ordering::Relaxed) == 0 {
                    self.set_stage(stage);
                    self.changed();
                }
                self.progress(value);
            }
            None => self.depth.store(0, Ordering::Relaxed),
        }
    }

    /// Its stage, or None while idle.
    fn stage_now(&self) -> Option<&'static str> {
        if self.depth.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let p = self.stage.load(Ordering::Relaxed);
        // SAFETY: only `set_stage` stores here, a pointer from a `&'static`
        // reference; null until the first one.
        Some(if p.is_null() { "?" } else { unsafe { *p } })
    }

    /// How long a busy marker has stood still, in ms.
    fn still_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.changed_ms.load(Ordering::Relaxed))
    }
}

/// Leaves the stage when dropped.
pub struct Busy(&'static Marker);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.depth.fetch_sub(1, Ordering::Relaxed);
        self.0.changed();
    }
}

/// The player's control thread: every listener's request runs on it.
pub static CONTROL: Marker = Marker::new("control");
/// Background preparations of a track: its chain, its Hybrid-Phase lists.
pub static PREP: Marker = Marker::new("prep");
/// The converter's batch.
pub static CONVERTER: Marker = Marker::new("converter");
/// Waiting for the video card.
pub static GPU: Marker = Marker::new("gpu");
/// The sound going out.
pub static OUTPUT: Marker = Marker::new("output");

const ALL: [&Marker; 5] = [&CONTROL, &PREP, &CONVERTER, &GPU, &OUTPUT];

/// One marker as text: `name=stage p=N still=Ss`, or `name=idle`.
fn describe(m: &Marker, now: u64) -> String {
    match m.stage_now() {
        Some(stage) => format!(
            "{}={} p={} still={:.1}s",
            m.name,
            stage,
            m.progress.load(Ordering::Relaxed),
            m.still_ms(now) as f64 / 1000.0
        ),
        None => format!("{}=idle", m.name),
    }
}

/// Every marker in a line (atomics only: a panic hook may ask).
pub fn markers_line() -> String {
    let now = now_ms();
    ALL.iter().map(|m| describe(m, now)).collect::<Vec<_>>().join("; ")
}

/// The stage most likely to be what a fault happened in: the marker that has
/// stood still longest, as (marker, stage, progress).
pub fn busiest() -> Option<(&'static str, &'static str, u64)> {
    let now = now_ms();
    ALL.iter()
        .filter_map(|m| m.stage_now().map(|s| (m.still_ms(now), m.name, s, m.progress.load(Ordering::Relaxed))))
        .max_by_key(|x| x.0)
        .map(|(_, n, s, p)| (n, s, p))
}

/// The markers of `of` that have just crossed the line, each once per
/// standstill.
fn newly_stalled<'a>(of: &[&'a Marker], now: u64, limit: u64) -> Vec<&'a Marker> {
    of.iter()
        .copied()
        .filter(|m| m.stage_now().is_some() && m.still_ms(now) > limit)
        .filter(|m| !m.told.swap(true, Ordering::Relaxed))
        .collect()
}

// ── the thread ───────────────────────────────────────────────────────────

/// Free memory as last read, in MB; `u64::MAX` is "not read yet".
static RAM_FREE_MB: AtomicU64 = AtomicU64::new(u64::MAX);
static VRAM_FREE_MB: AtomicU64 = AtomicU64::new(u64::MAX);

fn mb_text(v: u64) -> String {
    if v == u64::MAX { "?".into() } else { format!("{v} MB") }
}

fn mem_text() -> String {
    format!(
        "ram free {} vram free {}",
        mb_text(RAM_FREE_MB.load(Ordering::Relaxed)),
        mb_text(VRAM_FREE_MB.load(Ordering::Relaxed))
    )
}

/// A whole line in one write, through this module's handle. Shares its place
/// in the file with the logger's handle, so lines land one after another.
fn write_line(f: &mut std::fs::File, body: &str) {
    let _ = f.write_all(format!("[{}] {}\n", crate::audio::logging::ts(), body).as_bytes());
}

fn stall_line(m: &Marker, now: u64) -> String {
    format!(
        "[WATCHDOG] stall {}: no change for {:.0} s | {} | {}",
        m.name,
        m.still_ms(now) as f64 / 1000.0,
        markers_line(),
        mem_text()
    )
}

/// The memory reading lives on a thread apart: a driver stuck in the DXGI
/// call must not stop the log going to the disk.
fn start_mem_thread(mut log: Option<std::fs::File>) {
    let _ = std::thread::Builder::new().name("aura-watchdog-mem".into()).spawn(move || loop {
        std::thread::sleep(MEM_EVERY);
        if !ALL.iter().any(|m| m.stage_now().is_some()) {
            continue;
        }
        let (_, avail) = crate::audio::memory::ram_now_mb();
        RAM_FREE_MB.store(avail, Ordering::Relaxed);
        let vram = crate::audio::gpu::dxgi_memory::largest_adapter()
            .and_then(|(v, d)| crate::audio::gpu::dxgi_memory::query(v, d).ok())
            .map_or(u64::MAX, |m| m.free() >> 20);
        VRAM_FREE_MB.store(vram, Ordering::Relaxed);
        if let Some(f) = log.as_mut() {
            write_line(f, &format!("[MEM] {}", mem_text()));
        }
    });
}

/// Start looking. Called once from `main`, after the session log exists.
pub fn start() {
    let _ = now_ms();
    let log = crate::audio::logging::session_file_handle();
    let mem_log = log.as_ref().and_then(|f| f.try_clone().ok());
    start_mem_thread(mem_log);
    let mut log = log;
    let _ = std::thread::Builder::new().name("aura-watchdog".into()).spawn(move || loop {
        std::thread::sleep(TICK);
        let Some(f) = log.as_mut() else {
            continue;
        };
        let now = now_ms();
        for m in newly_stalled(&ALL, now, STALL_MS) {
            write_line(f, &stall_line(m, now));
        }
        // The values first: a log that outlives a power cut must not outlive what redacts it.
        crate::crash_report::known::sync();
        let _ = f.sync_data();
    });
}

// ── the session under way ────────────────────────────────────────────────

/// The session log of the run that ended without a clean exit, if one did.
static CRASHED: OnceLock<Option<PathBuf>> = OnceLock::new();
/// This run's session log.
static THIS: OnceLock<Option<PathBuf>> = OnceLock::new();

fn marker_path() -> Option<PathBuf> {
    crate::app_dir::root().map(|d| d.join("running"))
}

/// A folder's list of files, pushed to the disk. On Windows flushing a file
/// covers its entry there, and a folder cannot be opened this way.
fn sync_folder_of(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// A whole file, on the disk before this returns: it has to outlive a power cut.
pub fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    sync_folder_of(path)
}

/// What a `running` file found at start says about the last run, and leaves
/// `this_log` in its place. None: the last run ended cleanly. An empty path:
/// it ended uncleanly, and had no log of its own.
pub(crate) fn begin_at(marker: &Path, this_log: Option<&Path>) -> Option<PathBuf> {
    let before = std::fs::read(marker).ok().map(|b| PathBuf::from(String::from_utf8_lossy(&b).trim()));
    if let Some(dir) = marker.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = write_synced(marker, this_log.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default().as_bytes());
    before
}

/// This run has begun. After the single-instance check, so a second copy that
/// leaves at once does not touch the first one's file.
pub fn session_begin(this_log: Option<&Path>) {
    let before = marker_path().and_then(|m| begin_at(&m, this_log));
    let _ = CRASHED.set(before);
    let _ = THIS.set(this_log.map(Path::to_path_buf));
    if let Some(p) = this_log {
        let _ = sync_folder_of(p);
        crate::crash_report::known::open_sidecar(p);
    }
}

/// The last run ended without a clean exit: the log it wrote (may be empty,
/// when it could not make one).
pub fn crashed_session() -> Option<PathBuf> {
    CRASHED.get().cloned().flatten()
}

/// This run's session log.
pub fn this_session() -> Option<PathBuf> {
    THIS.get().cloned().flatten()
}

/// The program is leaving on its own terms.
pub fn session_end() {
    if let Some(m) = marker_path() {
        let _ = std::fs::remove_file(m);
    }
    crate::crash_report::known::close_sidecar();
    crate::crash_report::pending::discard_unasked();
}

/// Closing the console window, logging off and shutting down end the process
/// without an exit of its own, and none of them is a crash.
#[cfg(windows)]
pub fn watch_console_close() {
    unsafe extern "system" fn on_close(_kind: u32) -> i32 {
        session_end();
        0
    }
    // SAFETY: a handler for the life of the process; it touches no window.
    unsafe {
        winapi::um::consoleapi::SetConsoleCtrlHandler(Some(on_close), 1);
    }
}

#[cfg(not(windows))]
pub fn watch_console_close() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn leak(name: &'static str) -> &'static Marker {
        Box::leak(Box::new(Marker::new(name)))
    }

    #[test]
    fn a_marker_is_idle_until_entered_and_after_the_last_one_leaves() {
        let m = leak("t");
        assert_eq!(m.stage_now(), None);
        let a = m.enter(&"first");
        let b = m.enter(&"second");
        assert_eq!(m.stage_now(), Some("second"));
        drop(a);
        assert_eq!(m.stage_now(), Some("second"));
        drop(b);
        assert_eq!(m.stage_now(), None);
    }

    #[test]
    fn only_a_new_value_counts_as_movement() {
        let m = leak("t");
        let _g = m.enter(&"s");
        m.progress(5);
        m.changed_ms.store(1, Ordering::Relaxed);
        m.progress(5);
        assert_eq!(m.changed_ms.load(Ordering::Relaxed), 1);
        m.progress(6);
        assert!(m.changed_ms.load(Ordering::Relaxed) > 1);
    }

    #[test]
    fn hold_follows_its_loop() {
        let m = leak("t");
        m.hold(Some((&"playing", 10)));
        assert_eq!(m.stage_now(), Some("playing"));
        m.hold(None);
        assert_eq!(m.stage_now(), None);
    }

    #[test]
    fn a_standstill_is_told_once_and_movement_arms_it_again() {
        let m = leak("t");
        let _g = m.enter(&"s");
        let later = now_ms() + 1000;
        assert_eq!(newly_stalled(&[m], later, 500).len(), 1);
        assert_eq!(newly_stalled(&[m], later, 500).len(), 0, "one line per standstill");
        m.tick();
        let later = now_ms() + 1000;
        assert_eq!(newly_stalled(&[m], later, 500).len(), 1, "a new standstill is told again");
    }

    #[test]
    fn an_idle_marker_is_never_a_stall() {
        let m = leak("t");
        assert_eq!(newly_stalled(&[m], now_ms() + 100_000, 500).len(), 0);
    }

    #[test]
    fn the_stall_line_names_the_marker_and_every_other() {
        let m = leak("stuck");
        let _g = m.enter(&"hp lists");
        let line = stall_line(m, now_ms() + 31_000);
        assert!(line.starts_with("[WATCHDOG] stall stuck: no change for 31 s"), "{line}");
        assert!(line.contains("control="), "{line}");
        assert!(line.contains("ram free"), "{line}");
    }

    #[test]
    fn the_running_file_tells_an_unclean_end_from_a_clean_one() {
        let dir = std::env::temp_dir().join(format!("aura-wd-{}", std::process::id()));
        let marker = dir.join("running");
        let _ = std::fs::remove_file(&marker);
        assert_eq!(begin_at(&marker, Some(Path::new("a.log"))), None, "a clean start");
        assert_eq!(begin_at(&marker, Some(Path::new("b.log"))), Some(PathBuf::from("a.log")), "a is unclean");
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(begin_at(&marker, None), None, "after a clean end");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
