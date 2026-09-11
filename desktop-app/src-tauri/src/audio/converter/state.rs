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

/// Adaptive Apodizer per-file UI state (FileConvState.aa_state)
pub const AA_NONE: u32 = 0;    // AA disabled or verdict not known yet
pub const AA_TREATED: u32 = 1; // apodizer applied
pub const AA_SKIPPED: u32 = 2; // analyzed, deliberately left untouched

/// Per-file conversion state, shared between prep thread and GPU thread via Arc.
#[allow(dead_code)]
pub struct FileConvState {
    pub stage: AtomicU32,
    pub gpu_pct: AtomicU32,
    /// Badge code (BADGE_*) surfaced to the frontend UI.
    pub badge: AtomicU32,
    /// Adaptive Apodizer verdict (AA_*) + its human-readable tooltip.
    pub aa_state: AtomicU32,
    pub aa_note: Mutex<String>,
    pub cancelled: AtomicBool,
    pub error_msg: Mutex<String>,
    pub output_path: Mutex<String>,
    /// Queue revision at which this file last changed a DISCRETE field
    /// (stage/badge/AA/error/output — not the continuously-moving gpu_pct).
    /// `get_file_statuses(known_rev)` returns only files with
    /// last_rev > known_rev plus the currently active ones, so the 5 Hz UI
    /// poll ships a handful of entries instead of the entire queue — with
    /// 685 files that's the difference between ~150 KB and ~300 B per poll
    /// through the Tauri IPC bridge.
    pub last_rev: AtomicU32,
}
impl FileConvState {
    pub fn new() -> Self {
        Self {
            stage: AtomicU32::new(STAGE_PENDING),
            gpu_pct: AtomicU32::new(0),
            badge: AtomicU32::new(BADGE_NONE),
            aa_state: AtomicU32::new(AA_NONE),
            aa_note: Mutex::new(String::new()),
            cancelled: AtomicBool::new(false),
            error_msg: Mutex::new(String::new()),
            output_path: Mutex::new(String::new()),
            last_rev: AtomicU32::new(0),
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn set_stage(&self, s: u32) {
        self.stage.store(s, Ordering::Relaxed);
        self.touch();
    }
    pub fn stage(&self) -> u32 {
        self.stage.load(Ordering::Relaxed)
    }
    /// Mark this file as changed: bump the global queue revision and stamp
    /// it on the file. Call after every discrete state mutation (badge, AA
    /// verdict, error message, output path) — set_stage does it itself.
    pub fn touch(&self) {
        let rev = CONV_QUEUE_REV.fetch_add(1, Ordering::AcqRel) + 1;
        self.last_rev.store(rev, Ordering::Release);
    }
}

/// Global queue revision, bumped by FileConvState::touch(). The frontend
/// echoes the last revision it has seen; get_file_statuses returns only
/// entries newer than that.
pub static CONV_QUEUE_REV: AtomicU32 = AtomicU32::new(0);

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
