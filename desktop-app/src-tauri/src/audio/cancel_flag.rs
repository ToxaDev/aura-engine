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
