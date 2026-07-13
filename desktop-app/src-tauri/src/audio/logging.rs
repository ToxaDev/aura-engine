//! Centralised logger for the audio pipeline.
//!
//! Two pieces of functionality:
//!
//! 1. `aelog!` macro — `println!`-flavoured but every line carries a local-
//!    time timestamp `[HH:MM:SS.mmm]`.  Lets you correlate captured stdout
//!    against wall-clock events.
//!
//! 2. Heartbeat — a background thread that, when started, prints a single
//!    self-overwriting status line at the bottom of the console:
//!
//!        [01:18:30.123] · idle  4.7s
//!
//!    The line refreshes twice a second.  If the pipeline hangs on some
//!    long step, the `idle` counter keeps ticking, so you can see exactly
//!    how long it has been stuck.  Normal `aelog!` lines transparently
//!    erase the heartbeat, print themselves, and the heartbeat reappears
//!    on the next tick.
//!
//! Use `start_heartbeat()` at the beginning of a conversion run and
//! `stop_heartbeat()` at the end (`manager::convert_files` does this).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ── Public API ───────────────────────────────────────────────────────────

/// Returns the current local time formatted as `HH:MM:SS.mmm`.
pub fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Acquire the stdout-serialisation lock and erase any heartbeat line that
/// is currently sitting at the cursor.  `aelog!` calls this before its
/// inner println so the heartbeat thread cannot interleave bytes inside
/// our log output.  The returned guard MUST stay alive for the whole
/// println — the macro takes care of that.
pub fn lock_out() -> std::sync::MutexGuard<'static, ()> {
    let g = OUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    erase_heartbeat();
    g
}

/// Update the "last log" timestamp used by the idle counter.  Called
/// automatically by `aelog!` after every print.
pub fn touch() {
    LAST_LOG_MS.store(now_ms(), Ordering::Relaxed);
}

/// Spawn the heartbeat thread (idempotent).  Safe to call multiple times.
pub fn start_heartbeat() {
    if HEARTBEAT_RUN.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    touch(); // reset idle counter so we don't show "idle 9999s" right away
    thread::spawn(|| {
        while HEARTBEAT_RUN.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            // Skip if someone is in the middle of a real log line.
            if let Ok(_g) = OUT_LOCK.try_lock() {
                print_heartbeat();
            }
        }
    });
}

/// Stop the heartbeat thread and erase its line so the next prompt starts
/// at column 0.  Idempotent.
pub fn stop_heartbeat() {
    HEARTBEAT_RUN.store(false, Ordering::Relaxed);
    if let Ok(_g) = OUT_LOCK.lock() {
        erase_heartbeat();
    }
}

// ── Macro ────────────────────────────────────────────────────────────────

/// `println!`-flavoured macro that prefixes every line with a local-time
/// timestamp and integrates with the heartbeat (acquires the stdout lock,
/// erases any in-flight heartbeat line, prints, updates the idle counter).
#[macro_export]
macro_rules! aelog {
    () => {{
        let _g = $crate::audio::logging::lock_out();
        println!("[{}]", $crate::audio::logging::ts());
        $crate::audio::logging::touch();
    }};
    ($($arg:tt)*) => {{
        let _g = $crate::audio::logging::lock_out();
        println!("[{}] {}", $crate::audio::logging::ts(), format_args!($($arg)*));
        $crate::audio::logging::touch();
    }};
}

// ── Internals ────────────────────────────────────────────────────────────

/// Width to which every heartbeat line is padded, in chars.  Must be ≥
/// the longest format we'd ever produce.  80 leaves comfortable margin
/// over `  [HH:MM:SS.mmm] · idle XXX.Xs` (~ 30 chars).
const HEARTBEAT_WIDTH: usize = 80;

static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static HEARTBEAT_RUN: AtomicBool = AtomicBool::new(false);

/// Single mutex serialising every write to stdout that touches the
/// heartbeat (so the heartbeat thread can never paint bytes in the
/// middle of an `aelog!` line, and vice versa).
static OUT_LOCK: Mutex<()> = Mutex::new(());

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn print_heartbeat() {
    let now = now_ms();
    let last = LAST_LOG_MS.load(Ordering::Relaxed);
    let idle_s = now.saturating_sub(last) as f64 / 1000.0;
    // Two-space indent visually separates the heartbeat from log lines.
    let body = format!("  [{}] \u{00B7} idle {:>5.1}s", ts(), idle_s);
    // Pad to fixed width with spaces so any previous heartbeat (which may
    // have been longer) is fully overwritten on the same line. \r returns
    // the cursor to column 0 without emitting a newline, so the line
    // refreshes in place.
    print!("\r{:<width$}", body, width = HEARTBEAT_WIDTH);
    let _ = io::stdout().flush();
}

fn erase_heartbeat() {
    // Overwrite with spaces and reset cursor to column 0 so the next
    // println! starts cleanly. We don't use ANSI \x1b[2K for portability
    // with consoles that have VT mode disabled.
    print!("\r{}\r", " ".repeat(HEARTBEAT_WIDTH));
    let _ = io::stdout().flush();
}
