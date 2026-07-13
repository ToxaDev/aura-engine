use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const STAGE_PENDING: u32 = 0;
pub const STAGE_PREPARING: u32 = 1;
pub const STAGE_GPU_CONV: u32 = 2;
pub const STAGE_ENCODING: u32 = 3;
pub const STAGE_DONE: u32 = 4;
pub const STAGE_ERROR: u32 = 5;
pub const STAGE_CANCELLED: u32 = 6;
#[allow(dead_code)] pub const STAGE_BAD_RATE: u32 = 7;   // Non-standard sample rate
#[allow(dead_code)] pub const STAGE_SKIP_RATE: u32 = 8;  // Source rate >= target rate

/// Badge codes surfaced to the frontend (stored in FileConvState.badge)
pub const BADGE_NONE: u32 = 0;
pub const BADGE_BAD: u32 = 1;            // Non-standard sample rate
pub const BADGE_SKIP: u32 = 2;           // Source >= target, skipped
pub const BADGE_VERIFIED_FAIL: u32 = 3;  // Bit-perfect check failed

/// Per-file conversion state, shared between prep thread and GPU thread via Arc.
#[allow(dead_code)]
pub struct FileConvState {
    pub stage: AtomicU32,
    pub gpu_pct: AtomicU32,
    /// Badge code (BADGE_*) surfaced to the frontend UI.
    pub badge: AtomicU32,
    pub cancelled: AtomicBool,
    pub error_msg: Mutex<String>,
    pub output_path: Mutex<String>,
}
impl FileConvState {
    pub fn new() -> Self {
        Self {
            stage: AtomicU32::new(STAGE_PENDING),
            gpu_pct: AtomicU32::new(0),
            badge: AtomicU32::new(BADGE_NONE),
            cancelled: AtomicBool::new(false),
            error_msg: Mutex::new(String::new()),
            output_path: Mutex::new(String::new()),
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn set_stage(&self, s: u32) {
        self.stage.store(s, Ordering::Relaxed);
    }
    pub fn stage(&self) -> u32 {
        self.stage.load(Ordering::Relaxed)
    }
}

// ═══ Global conversion state ═══
lazy_static::lazy_static! {
    pub static ref CONV_PROGRESS:    AtomicU32  = AtomicU32::new(0);      // current file: 0-1000
    pub static ref CONV_RUNNING:     AtomicBool = AtomicBool::new(false);
    pub static ref CONV_CANCEL:      AtomicBool = AtomicBool::new(false); // cancel current or all
    pub static ref CONV_CANCEL_FILE: AtomicBool = AtomicBool::new(false); // true = single-file cancel
    pub static ref CONV_STATUS:      Mutex<String> = Mutex::new(String::new());
    pub static ref CONV_OUTPUT:      Mutex<String> = Mutex::new(String::new());
    pub static ref CONV_SNAPPED_RATE:AtomicU32  = AtomicU32::new(0);
    /// Per-file states, one Arc<FileConvState> per file in the current batch.
    pub static ref CONV_FILE_STATES: Mutex<Vec<Arc<FileConvState>>> = Mutex::new(Vec::new());
}
