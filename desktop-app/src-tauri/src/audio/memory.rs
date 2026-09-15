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
        if crate::audio::converter::state::CONV_CANCEL.load(Ordering::Relaxed) {
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

/// Waits until the system has enough available RAM (taking into account a 2 GB safety margin)
/// Once the required RAM is available, the provided `alloc_fn` is executed BEFORE the global
/// memory lock is released. This guarantees that multiple threads will not bypass the gate
/// simultaneously and allocate massive buffers before the OS has time to reflect the usage in `sysinfo`.
pub fn await_free_ram_and_allocate<F, R>(required_mb: u64, alloc_fn: F) -> R
where
    F: FnOnce() -> R,
{
    loop {
        // We lock inside the loop so we don't hold the lock while polling/sleeping globally.
        {
            let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
            // Refresh ONLY memory info to save CPU overhead
            sys.refresh_memory();
            let avail_mb = sys.available_memory() / 1024 / 1024;
            let safe_free_mb = 2048; // keep 2048 MB free

            if avail_mb.saturating_sub(safe_free_mb) > required_mb {
                // Execute the allocation while holding the lock! 
                // This ensures that when the NEXT thread locks `GLOBAL_RAM_LOCK` to check,
                // the `alloc_fn` has already materialized its arrays in memory, causing the OS
                // memory manager to register the usage and thus updating `refresh_memory()` accurately.
                let result = alloc_fn();
                return result;
            }
        }

        // If we didn't return, it means RAM is too low.
        set_status(&format!("SYSTEM RAM LOW. GPU thread waiting for {:.1} GB free...", (required_mb + 2048) as f64 / 1024.0));
        
        // Sleep outside lock to let other threads check
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// Waits until the system has enough available RAM before returning.
/// Used for synchronous protection where we don't need to hold the lock during allocation.
pub fn await_free_ram(required_mb: u64, requester_name: &str) {
    loop {
        {
            let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
            sys.refresh_memory();
            let avail_mb = sys.available_memory() / 1024 / 1024;
            let safe_free_mb = 2048; // keep 2 GB free

            if avail_mb.saturating_sub(safe_free_mb) > required_mb {
                return;
            }
        }
        set_status(&format!("SYSTEM RAM LOW. {} waiting for {:.1} GB free...", requester_name, (required_mb + 2048) as f64 / 1024.0));
        std::thread::sleep(Duration::from_millis(1500));
    }
}
