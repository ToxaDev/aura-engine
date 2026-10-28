//! This app's load on the processor and on the video card, once a second
//! (Anton 19.10: he wanted to see what the engine costs while it plays and
//! converts). The badge beside the version shows the last second; the load
//! window draws the last hour, kept here so it is there when the window opens.
//!
//! The processor: this process's CPU time over the second, as a share of all
//! logical cores (100 = every core busy the whole second).
//!
//! The video card: the time our own GPU work kept it busy. Every GPU block
//! holds a span from its submit until its result is read back (the converter's
//! OLA block, the player's stream block); spans open at once on several
//! threads count once. That is the card working on our requests, with the
//! copies in and out and the driver's queue time included — an upper bound,
//! and the same measure on every system the app runs on. GPU work that does
//! not go through our queue (the spatial network, the page's own drawing) is
//! not in it.
//!
//! The memory: this process's working set (Anton 25.10, after an 8 GB machine
//! had its conversions refused for RAM) — what the system counts as ours in
//! RAM now, the figure the task manager shows.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One hour of seconds.
pub const HISTORY: usize = 3600;
const TICK: Duration = Duration::from_secs(1);

/// One second of load.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Sample {
    /// When the second ended, in ms since 1970.
    pub t: u64,
    /// This app's share of all cores, 0–100.
    pub cpu: f64,
    /// This app's share of the video card's time, 0–100.
    pub gpu: f64,
    /// This app's working set, MB (0 when it could not be read).
    pub ram: u32,
    /// The player was playing.
    pub play: bool,
    /// A conversion was running.
    pub conv: bool,
}

/// The last `cap` items, oldest first.
pub struct Ring<T> {
    items: VecDeque<T>,
    cap: usize,
}

impl<T: Clone> Ring<T> {
    pub const fn new(cap: usize) -> Self {
        Ring { items: VecDeque::new(), cap }
    }

    pub fn push(&mut self, v: T) {
        if self.cap == 0 {
            return;
        }
        while self.items.len() >= self.cap {
            self.items.pop_front();
        }
        self.items.push_back(v);
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.items.iter().cloned().collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// `part` of `whole` as a percentage, 0–100, to a tenth.
fn share(part: f64, whole: f64) -> f64 {
    if !(whole > 0.0) || !part.is_finite() {
        return 0.0;
    }
    let p = (part / whole * 100.0).clamp(0.0, 100.0);
    (p * 10.0).round() / 10.0
}

/// CPU time `cpu` used over `wall` on a machine of `cores` logical cores.
pub fn cpu_percent(cpu: Duration, wall: Duration, cores: usize) -> f64 {
    share(cpu.as_secs_f64(), wall.as_secs_f64() * cores as f64)
}

/// GPU busy time `busy` over `wall`.
pub fn gpu_percent(busy: Duration, wall: Duration) -> f64 {
    share(busy.as_secs_f64(), wall.as_secs_f64())
}

/// Bytes as whole MB, rounded.
fn mb(bytes: u64) -> u32 {
    (bytes.saturating_add(1 << 19) >> 20).min(u32::MAX as u64) as u32
}

/// This process's memory, read once a second: one `System` that only ever
/// holds this process, refreshed without CPU, disk or user lookups.
struct Memory {
    sys: sysinfo::System,
    pid: sysinfo::Pid,
}

impl Memory {
    /// Builds this process's entry now: on Windows sysinfo reads the memory
    /// only when it refreshes an entry it already holds, not when it makes one.
    fn new() -> Self {
        use sysinfo::{PidExt, ProcessRefreshKind, SystemExt};
        let mut m = Memory { sys: sysinfo::System::new(), pid: sysinfo::Pid::from_u32(std::process::id()) };
        m.sys.refresh_process_specifics(m.pid, ProcessRefreshKind::new());
        m
    }

    /// The working set in MB, or 0.
    fn working_set_mb(&mut self) -> u32 {
        use sysinfo::{ProcessExt, ProcessRefreshKind, SystemExt};
        if !self.sys.refresh_process_specifics(self.pid, ProcessRefreshKind::new()) {
            return 0;
        }
        self.sys.process(self.pid).map_or(0, |p| mb(p.memory()))
    }
}

/// The union of open spans: busy while at least one is open.
struct Busy {
    open: u32,
    since: Option<Instant>,
    total: Duration,
}

impl Busy {
    const fn new() -> Self {
        Busy { open: 0, since: None, total: Duration::ZERO }
    }

    fn begin(&mut self, now: Instant) {
        if self.open == 0 {
            self.since = Some(now);
        }
        self.open += 1;
    }

    fn end(&mut self, now: Instant) {
        if self.open == 0 {
            return;
        }
        self.open -= 1;
        if self.open == 0 {
            if let Some(s) = self.since.take() {
                self.total += now.saturating_duration_since(s);
            }
        }
    }

    /// Busy time so far, the span still open counted up to `now`.
    fn total_at(&self, now: Instant) -> Duration {
        match self.since {
            Some(s) if self.open > 0 => self.total + now.saturating_duration_since(s),
            _ => self.total,
        }
    }
}

static GPU_BUSY: Mutex<Busy> = Mutex::new(Busy::new());
static SAMPLES: Mutex<Ring<Sample>> = Mutex::new(Ring::new(HISTORY));
/// The sampler's own time: ticks and their summed wall time.
static COST: Mutex<(u64, Duration)> = Mutex::new((0, Duration::ZERO));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Our GPU work in flight: from the submit until the result is back. Dropped,
/// it ends.
pub struct GpuSpan(());

pub fn gpu_span() -> GpuSpan {
    lock(&GPU_BUSY).begin(Instant::now());
    GpuSpan(())
}

impl Drop for GpuSpan {
    fn drop(&mut self) {
        lock(&GPU_BUSY).end(Instant::now());
    }
}

/// This process's CPU time so far (user + kernel, all threads).
#[cfg(windows)]
fn process_cpu_time() -> Option<Duration> {
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::processthreadsapi::{GetCurrentProcess, GetProcessTimes};
    let zero = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    // SAFETY: the pseudo-handle of this process and four FILETIMEs we own.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    // 100 ns units.
    let ticks = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Some(Duration::from_nanos((ticks(kernel) + ticks(user)).saturating_mul(100)))
}

#[cfg(unix)]
fn process_cpu_time() -> Option<Duration> {
    // SAFETY: getrusage fills the struct we own.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return None;
    }
    let tv = |t: libc::timeval| Duration::new(t.tv_sec.max(0) as u64, (t.tv_usec.max(0) as u32).min(999_999) * 1000);
    Some(tv(ru.ru_utime) + tv(ru.ru_stime))
}

#[cfg(not(any(windows, unix)))]
fn process_cpu_time() -> Option<Duration> {
    None
}

/// What one tick reads.
struct Reading {
    at: Instant,
    cpu: Option<Duration>,
    gpu: Duration,
}

impl Reading {
    fn now() -> Self {
        let at = Instant::now();
        Reading { at, cpu: process_cpu_time(), gpu: lock(&GPU_BUSY).total_at(at) }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The second between two readings.
fn sample(prev: &Reading, cur: &Reading, cores: usize, ram: u32, play: bool, conv: bool) -> Sample {
    let wall = cur.at.saturating_duration_since(prev.at);
    let cpu = match (prev.cpu, cur.cpu) {
        (Some(a), Some(b)) => cpu_percent(b.saturating_sub(a), wall, cores),
        _ => 0.0,
    };
    Sample { t: unix_ms(), cpu, gpu: gpu_percent(cur.gpu.saturating_sub(prev.gpu), wall), ram, play, conv }
}

/// The sampler: one thread, once a second, for the life of the process.
/// The main and load windows hear each second as `perf:tick`.
pub fn start(app: tauri::AppHandle) {
    use tauri::Emitter;
    let spawned = std::thread::Builder::new().name("perf".into()).spawn(move || {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let mut memory = Memory::new();
        let mut prev = Reading::now();
        let mut play = false;
        let mut next = prev.at + TICK;
        loop {
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            // A machine that slept: start again from now rather than catch up.
            next = if Instant::now() > next + TICK { Instant::now() + TICK } else { next + TICK };
            let cur = Reading::now();
            play = crate::player::controller::is_playing_now().unwrap_or(play);
            let ram = memory.working_set_mb();
            let s = sample(&prev, &cur, cores, ram, play, crate::audio::converter::manager::is_running());
            prev = cur;
            lock(&SAMPLES).push(s);
            // Only the main window (its load badge) and the load window listen;
            // the others are not woken every second for nothing.
            for label in ["main", "perf"] {
                let _ = app.emit_to(label, "perf:tick", s);
            }
            let spent = prev.at.elapsed();
            let mut c = lock(&COST);
            c.0 += 1;
            c.1 += spent;
            if c.0 == 60 {
                crate::aelog!("[PERF] the load sampler takes {} us a second", (c.1 / 60).as_micros());
            }
        }
    });
    if let Err(e) = spawned {
        crate::aelog!("[PERF] no load sampler: {e}");
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    samples: Vec<Sample>,
    /// The sampler's own average wall time per tick, in µs.
    tick_us: u64,
    /// The machine's RAM, MB, for the scale of the memory graph.
    ram_total_mb: u64,
}

/// The kept seconds, oldest first, for the load window.
#[tauri::command]
pub fn perf_history() -> History {
    let samples = lock(&SAMPLES).to_vec();
    let (n, spent) = *lock(&COST);
    History {
        samples,
        tick_us: if n > 0 { (spent / n as u32).as_micros() as u64 } else { 0 },
        ram_total_mb: crate::audio::memory::total_ram_mb(),
    }
}

/// Open (or bring up) the load window: frameless like the others, where it
/// was last time (window_state.rs). `async` for the same reason as the
/// analyzer's (wry#583).
#[tauri::command]
pub async fn perf_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl};
    if let Some(w) = app.get_webview_window("perf") {
        let _ = w.unminimize();
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let w = tauri::WebviewWindowBuilder::new(&app, "perf", WebviewUrl::App("perf.html".into()))
        .title("Aura Engine - Load")
        .inner_size(680.0, 560.0)
        .min_inner_size(380.0, 360.0)
        .resizable(true)
        .decorations(false)
        .transparent(true)
        // No Tauri 2 shadow (its 1px border and DWM frame): as on Tauri 1.
        .shadow(false)
        .visible(false)
        .use_https_scheme(true)
        .build()
        .map_err(|e| e.to_string())?;
    crate::window_state::restore_all(&w.as_ref().window());
    let _ = w.show();
    let _ = w.set_focus();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn cpu_is_a_share_of_all_cores() {
        assert_eq!(cpu_percent(Duration::from_millis(500), S, 1), 50.0);
        // One core busy on a 32-core machine.
        assert_eq!(cpu_percent(S, S, 32), 3.1);
        // Every core busy.
        assert_eq!(cpu_percent(Duration::from_secs(16), S, 16), 100.0);
        // A late tick: the same work over two seconds is half the load.
        assert_eq!(cpu_percent(S, 2 * S, 4), 12.5);
    }

    #[test]
    fn percentages_are_clamped_and_never_nan() {
        // More CPU time than cores × wall (accounting granularity) reads 100.
        assert_eq!(cpu_percent(Duration::from_secs(5), S, 4), 100.0);
        assert_eq!(cpu_percent(S, Duration::ZERO, 8), 0.0);
        assert_eq!(cpu_percent(S, S, 0), 0.0);
        assert_eq!(gpu_percent(2 * S, S), 100.0);
        assert_eq!(gpu_percent(Duration::ZERO, S), 0.0);
        assert_eq!(gpu_percent(S, Duration::ZERO), 0.0);
        assert_eq!(gpu_percent(Duration::from_millis(123), S), 12.3);
    }

    #[test]
    fn overlapping_spans_count_once() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut b = Busy::new();
        // A: 0–300, B: 100–500 (overlap), C: 700–800.
        b.begin(at(0));
        b.begin(at(100));
        b.end(at(300));
        assert_eq!(b.total_at(at(400)), Duration::from_millis(400), "B still open");
        b.end(at(500));
        b.begin(at(700));
        b.end(at(800));
        assert_eq!(b.total_at(at(1000)), Duration::from_millis(600));
        // An end without a begin changes nothing.
        b.end(at(900));
        assert_eq!(b.total_at(at(1000)), Duration::from_millis(600));
    }

    #[test]
    fn a_span_open_across_ticks_splits_between_them() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut b = Busy::new();
        b.begin(at(600));
        let first = b.total_at(at(1000));
        b.end(at(1300));
        let second = b.total_at(at(2000)) - first;
        assert_eq!(gpu_percent(first, S), 40.0);
        assert_eq!(gpu_percent(second, S), 30.0);
    }

    #[test]
    fn a_sample_is_the_delta_between_readings() {
        let t0 = Instant::now();
        let prev = Reading { at: t0, cpu: Some(Duration::from_secs(10)), gpu: Duration::from_millis(50) };
        let cur = Reading { at: t0 + S, cpu: Some(Duration::from_millis(12_000)), gpu: Duration::from_millis(300) };
        let s = sample(&prev, &cur, 8, 640, true, false);
        assert_eq!((s.cpu, s.gpu, s.ram, s.play, s.conv), (25.0, 25.0, 640, true, false));
        // No CPU clock: 0, not garbage.
        let none = Reading { at: t0 + S, cpu: None, gpu: Duration::from_millis(50) };
        assert_eq!(sample(&prev, &none, 8, 0, false, true).cpu, 0.0);
    }

    #[test]
    fn bytes_round_to_whole_mb() {
        assert_eq!(mb(0), 0);
        assert_eq!(mb((1 << 19) - 1), 0);
        assert_eq!(mb(1 << 19), 1);
        assert_eq!(mb(1300 << 20), 1300);
        assert_eq!(mb(u64::MAX), u32::MAX);
    }

    #[test]
    fn a_sample_and_the_history_carry_the_memory() {
        let s = Sample { t: 1, cpu: 2.5, gpu: 0.0, ram: 1234, play: false, conv: true };
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(v["ram"], 1234);
        let h = History { samples: vec![s], tick_us: 90, ram_total_mb: 8192 };
        let v = serde_json::to_value(h).unwrap();
        assert_eq!(v["ramTotalMb"], 8192);
        assert_eq!(v["tickUs"], 90);
        assert_eq!(v["samples"][0]["ram"], 1234);
    }

    #[test]
    fn this_process_has_a_working_set() {
        let mut m = Memory::new();
        // The first read already counts (the entry was built in new()), and so does the next.
        // (Not "it grows when we allocate": other tests in this process allocate and free too.)
        assert!(m.working_set_mb() > 0, "no working set on the first read");
        assert!(m.working_set_mb() > 0, "no working set on the second read");
    }

    #[test]
    fn the_ring_keeps_the_last_items() {
        let mut r = Ring::new(3);
        for i in 0..5 {
            r.push(i);
        }
        assert_eq!(r.to_vec(), vec![2, 3, 4]);
        assert_eq!(r.len(), 3);
        let mut empty: Ring<u8> = Ring::new(0);
        empty.push(1);
        assert_eq!(empty.len(), 0);
        let h: Ring<Sample> = Ring::new(HISTORY);
        assert_eq!(h.cap, 3600);
    }

    #[test]
    fn this_process_has_a_cpu_clock_that_moves() {
        let a = process_cpu_time().expect("a CPU clock");
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(60) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let b = process_cpu_time().expect("a CPU clock");
        assert!(b > a, "CPU time did not move: {a:?} -> {b:?}");
    }

    #[test]
    fn a_span_marks_the_card_busy() {
        let before = lock(&GPU_BUSY).total_at(Instant::now());
        {
            let _s = gpu_span();
            std::thread::sleep(Duration::from_millis(20));
        }
        let after = lock(&GPU_BUSY).total_at(Instant::now());
        assert!(after - before >= Duration::from_millis(20), "{before:?} -> {after:?}");
    }
}
