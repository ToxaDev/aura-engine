/// Shared cross-module cancellation flag.
/// Accessible from both `converter.rs` and `gpu_core.rs` without circular deps.
use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);

/// Returns `true` if cancellation was requested.
#[inline]
pub fn check() -> bool {
    CANCEL.load(Ordering::Relaxed)
}

/// Set or clear the cancellation flag.
#[inline]
pub fn set(v: bool) {
    CANCEL.store(v, Ordering::Relaxed);
}

/// Expose reference to the underlying AtomicBool (for passing to sub-modules).
#[inline]
pub fn get_atomic() -> &'static AtomicBool {
    &CANCEL
}

// ── Flags that answer only for themselves ───────────────────────────────
//
// The flag above is the converter's: a cancelled batch raises it, and it stays
// up until the next batch starts. Playback runs the same source stages
// (`prepare_audio_phase`) at any moment, a batch or no batch — and every one of
// them asks `file_or_global_cancelled`, so for as long as the flag stayed up
// after a cancel, every preparation the player started would stop at the first
// check and the player would fall silent.
//
// A caller whose work must not be stopped by the converter's cancel registers
// its own per-call flag here for the duration of the call; the global flag is
// then not consulted for that flag. The converter never registers one, and the
// registry is only read once the global flag is already up, so nothing about
// conversion — its cancel or its output — changes.

static SELF_ONLY: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Whether `flag` is registered as answering only for itself.
pub fn answers_only_for_itself(flag: &AtomicBool) -> bool {
    let addr = flag as *const AtomicBool as usize;
    SELF_ONLY.lock().map(|v| v.contains(&addr)).unwrap_or(false)
}

/// Registers `flag` until the returned guard is dropped.
pub fn self_only(flag: &AtomicBool) -> SelfOnlyGuard {
    let addr = flag as *const AtomicBool as usize;
    if let Ok(mut v) = SELF_ONLY.lock() {
        v.push(addr);
    }
    SelfOnlyGuard(addr)
}

pub struct SelfOnlyGuard(usize);

impl Drop for SelfOnlyGuard {
    fn drop(&mut self) {
        if let Ok(mut v) = SELF_ONLY.lock() {
            if let Some(i) = v.iter().position(|&a| a == self.0) {
                v.swap_remove(i);
            }
        }
    }
}
