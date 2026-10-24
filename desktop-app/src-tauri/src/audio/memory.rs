use lazy_static::lazy_static;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use sysinfo::{System, SystemExt};

use crate::audio::converter::decode::set_status;

lazy_static! {
    static ref GLOBAL_RAM_LOCK: Mutex<System> = Mutex::new(System::new());
}

/// Sum of all outstanding `RamReservation`s in MB.
static RESERVED_MB: AtomicU64 = AtomicU64::new(0);

/// Total physical RAM in MB (refreshes once per call; cheap).
pub fn total_ram_mb() -> u64 {
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    sys.refresh_memory();
    sys.total_memory() / 1024 / 1024
}

/// Memory the system has available now, in MB — read again at most once a
/// second (the player's tick asks every 200 ms).
pub fn available_ram_mb() -> u64 {
    static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, mb)) = *last {
        if at.elapsed() < Duration::from_secs(1) {
            return mb;
        }
    }
    let mb = {
        let mut sys = GLOBAL_RAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        sys.refresh_memory();
        sys.available_memory() / 1024 / 1024
    };
    *last = Some((std::time::Instant::now(), mb));
    mb
}

/// Log this process's own memory footprint (working set + committed
/// private bytes) plus system-wide availability into the session log.
/// Called at batch start, after every finished file and around the heavy
/// stages, so a multi-hour batch leaves a memory CURVE in the log — the
/// only way to tell "high but stable peak" apart from "monotonic leak".
pub fn log_process_memory(tag: &str) {
    use sysinfo::{PidExt, ProcessExt};
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let (ws_mb, commit_mb, avail_mb) = {
        let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
        sys.refresh_process(pid);
        sys.refresh_memory();
        let (ws, commit) = sys
            .process(pid)
            .map(|p| (p.memory() / 1024 / 1024, p.virtual_memory() / 1024 / 1024))
            .unwrap_or((0, 0));
        (ws, commit, sys.available_memory() / 1024 / 1024)
    };
    crate::aelog!(
        "[MEM] {} — working set {} MB | commit {} MB | batch reserved {} MB | system avail {} MB",
        tag,
        ws_mb,
        commit_mb,
        RESERVED_MB.load(Ordering::Acquire),
        avail_mb
    );
}

/// RAII admission ticket for one file's peak RAM demand. Dropping it
/// releases the budget for the next waiting worker.
pub struct RamReservation {
    mb: u64,
}

impl Drop for RamReservation {
    fn drop(&mut self) {
        RESERVED_MB.fetch_sub(self.mb, Ordering::AcqRel);
    }
}

/// Admission control for the heavy per-file stage: blocks until the file's
/// ESTIMATED peak RAM demand fits alongside every other in-flight file.
///
/// This is what `await_free_ram*` alone cannot do: those gates check free
/// RAM at one allocation site, but a worker mid-file cannot back out — two
/// workers can both pass an early gate and then balloon together until the
/// whole system swaps. Reserving the full estimated peak UP FRONT means the
/// second worker simply doesn't start its file until the first one's
/// reservation is released.
///
/// Deadlock safety: when no reservations are outstanding the caller is
/// admitted unconditionally (a single file larger than the budget must
/// still convert — the per-allocation `await_free_ram*` gates remain the
/// backstop). A global cancel also admits immediately so cancellation is
/// never blocked by the queue.
pub fn reserve_ram(required_mb: u64, label: &str) -> RamReservation {
    loop {
        {
            let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
            let outstanding = RESERVED_MB.load(Ordering::Acquire);
            if outstanding == 0 {
                RESERVED_MB.fetch_add(required_mb, Ordering::AcqRel);
                return RamReservation { mb: required_mb };
            }
            sys.refresh_memory();
            let total_mb = sys.total_memory() / 1024 / 1024;
            let avail_mb = sys.available_memory() / 1024 / 1024;
            // Leave 4 GB of physical RAM to the OS, the webview and other
            // apps; never let concurrent conversions plan past that line.
            let budget_mb = total_mb.saturating_sub(4096);
            if outstanding + required_mb <= budget_mb
                && avail_mb.saturating_sub(2048) > required_mb
            {
                RESERVED_MB.fetch_add(required_mb, Ordering::AcqRel);
                return RamReservation { mb: required_mb };
            }
        }
        if crate::audio::converter::state::cancel_requested() {
            // Don't hold up cancellation — the caller re-checks the flag.
            return RamReservation { mb: 0 };
        }
        set_status(&format!(
            "{}: waiting for RAM budget ({:.1} GB estimated peak)...",
            label,
            required_mb as f64 / 1024.0
        ));
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// `reserve_ram` without the wait: the ticket if the budget admits it now,
/// None if it does not. For memory that is only worth holding when it is
/// free to hold — an album's tracks prepared ahead of their turn — and must
/// never be what a waiting worker waits on. Unlike `reserve_ram` it does not
/// admit unconditionally when nothing else is outstanding.
pub fn try_reserve_ram(required_mb: u64) -> Option<RamReservation> {
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    let outstanding = RESERVED_MB.load(Ordering::Acquire);
    sys.refresh_memory();
    let total_mb = sys.total_memory() / 1024 / 1024;
    let avail_mb = sys.available_memory() / 1024 / 1024;
    let budget_mb = total_mb.saturating_sub(4096);
    if outstanding + required_mb <= budget_mb && avail_mb.saturating_sub(2048) > required_mb {
        RESERVED_MB.fetch_add(required_mb, Ordering::AcqRel);
        Some(RamReservation { mb: required_mb })
    } else {
        None
    }
}

/// RAM the free-memory gates leave to the OS and other apps: 2 GB on a
/// 16 GB+ machine, an eighth of the RAM below that (1 GB on 8 GB), never
/// under 512 MB. A flat 2 GB asked an 8 GB laptop with ~2 GB free for
/// 4.4 GB before the first track could even start.
fn safety_margin_mb(total_mb: u64) -> u64 {
    (total_mb / 8).clamp(512, 2048)
}

/// How long a free-memory gate waits for other apps or workers to give
/// RAM back. Past it the caller goes ahead: on a small machine the RAM may
/// never come free, and the page file makes a slow conversion better than
/// one that waits forever. The per-file `reserve_ram` tickets still keep
/// parallel workers from ballooning together.
const LOW_RAM_MAX_WAIT: Duration = Duration::from_secs(15);

/// Whether a request fits what is available now, after the safety margin.
fn ram_fits(avail_mb: u64, total_mb: u64, required_mb: u64) -> bool {
    avail_mb.saturating_sub(safety_margin_mb(total_mb)) > required_mb
}

/// Checks the gate once under the global lock; on a fit (or once the wait
/// is over) runs `on_pass` while still holding the lock.
fn gate_once<F, R>(required_mb: u64, give_up: bool, requester: &str, on_pass: F) -> Result<R, F>
where
    F: FnOnce() -> R,
{
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    // Refresh ONLY memory info to save CPU overhead
    sys.refresh_memory();
    let total_mb = sys.total_memory() / 1024 / 1024;
    let avail_mb = sys.available_memory() / 1024 / 1024;
    if ram_fits(avail_mb, total_mb, required_mb) {
        return Ok(on_pass());
    }
    if give_up {
        crate::aelog!(
            "[MEM] {} goes ahead with low RAM — needs ~{} MB, system avail {} MB of {} MB (the page file takes the rest)",
            requester,
            required_mb,
            avail_mb,
            total_mb
        );
        return Ok(on_pass());
    }
    set_status(&format!(
        "SYSTEM RAM LOW. {} waiting for {:.1} GB free...",
        requester,
        (required_mb + safety_margin_mb(total_mb)) as f64 / 1024.0
    ));
    Err(on_pass)
}

fn await_gate<F, R>(required_mb: u64, requester: &str, mut on_pass: F) -> R
where
    F: FnOnce() -> R,
{
    let started = std::time::Instant::now();
    loop {
        // We lock inside the check so we don't hold the lock while sleeping.
        let give_up = started.elapsed() >= LOW_RAM_MAX_WAIT
            || crate::audio::converter::state::cancel_requested();
        match gate_once(required_mb, give_up, requester, on_pass) {
            Ok(r) => return r,
            Err(f) => on_pass = f,
        }
        // Sleep outside lock to let other threads check
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// Waits until the system has enough available RAM (keeping a safety margin
/// free, see `safety_margin_mb`), at most `LOW_RAM_MAX_WAIT`.
/// Once it does, the provided `alloc_fn` is executed BEFORE the global
/// memory lock is released. This guarantees that multiple threads will not bypass the gate
/// simultaneously and allocate massive buffers before the OS has time to reflect the usage in `sysinfo`.
pub fn await_free_ram_and_allocate<F, R>(required_mb: u64, alloc_fn: F) -> R
where
    F: FnOnce() -> R,
{
    await_gate(required_mb, "GPU thread", alloc_fn)
}

/// Waits until the system has enough available RAM before returning, at
/// most `LOW_RAM_MAX_WAIT`.
/// Used for synchronous protection where we don't need to hold the lock during allocation.
pub fn await_free_ram(required_mb: u64, requester_name: &str) {
    await_gate(required_mb, requester_name, || ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_scales_with_installed_ram() {
        assert_eq!(safety_margin_mb(4 * 1024), 512);
        assert_eq!(safety_margin_mb(8 * 1024), 1024);
        assert_eq!(safety_margin_mb(16 * 1024), 2048);
        assert_eq!(safety_margin_mb(64 * 1024), 2048);
    }

    #[test]
    fn eight_gb_laptop_fits_a_normal_track() {
        // The reported case: 8 GB, ~2 GB available. Decoding a 4-minute
        // 44.1 kHz track is estimated at ~600 MB — fits; the old flat
        // 2.5 GB request did not.
        assert!(ram_fits(1990, 8 * 1024, 600));
        assert!(!ram_fits(1990, 8 * 1024, 2500));
        // The same 2 GB free on a 32 GB machine keeps the full 2 GB margin.
        assert!(!ram_fits(1990, 32 * 1024, 600));
    }
}
