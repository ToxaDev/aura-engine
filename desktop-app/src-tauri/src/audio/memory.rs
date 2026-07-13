use lazy_static::lazy_static;
use std::sync::Mutex;
use std::time::Duration;
use sysinfo::{System, SystemExt};

use crate::audio::converter::decode::set_status;

lazy_static! {
    static ref GLOBAL_RAM_LOCK: Mutex<System> = Mutex::new(System::new());
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
