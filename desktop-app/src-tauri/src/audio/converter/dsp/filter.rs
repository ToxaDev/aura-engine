//! Resolves the path of a pre-computed FIR filter blob from
//! `fir-optimizer/output/`.
//!
//! Naming convention (set by `fir-optimizer/optimize.py --all-ratios`):
//!
//!     fir_<TAG>_<TARGET_HZ>_<phase>.npy
//!
//! where
//!     TAG       ∈ {"1M", "5M", "10M", "30M"}
//!     TARGET_HZ = output sample rate in Hz, e.g. 88200, 352800
//!     phase     ∈ {"linear_phase", "minimum_phase"}
//!
//! Examples:
//!   fir_30M_352800_linear_phase.npy   (44.1 kHz × 8)
//!   fir_1M_88200_minimum_phase.npy    (44.1 kHz × 2)
//!   fir_5M_768000_linear_phase.npy    (48 kHz × 16)
//!
//! Backward-compat fallback: an older single-rate naming is kept for
//! systems that still have the legacy blobs but only at the FS8 design
//! point (44.1 → 352.8 kHz or 48 → 384 kHz). Any other ratio MUST have
//! the ratio-specific file — otherwise the runtime would silently apply
//! the wrong cutoff (the bug fixed in commit `ca1af01`).
//!
//!     fir_<TAG>_<phase>.npy           (legacy, FS8 only)

/// Resolve the path of the pre-computed FIR blob for `taps`, output rate
/// `target_rate_hz`, and phase type `phase_type`. Returns `None` when no
/// suitable file exists; the caller is expected to skip the post-FIR step
/// with a clear warning rather than silently apply a mismatched filter.
///
/// `phase_type` must be `"linear_phase"` or `"minimum_phase"`.
pub fn find_precomputed_filter(
    taps: usize,
    target_rate_hz: u32,
    phase_type: &str,
) -> Option<String> {
    let taps_label = match taps {
        t if t >= 25_000_000 => "30M",
        t if t >= 7_500_000 => "10M",
        t if t >= 2_500_000 => "5M",
        t if t >= 500_000 => "1M",
        _ => return None,
    };

    let primary = format!("fir_{}_{}_{}.npy", taps_label, target_rate_hz, phase_type);
    let legacy = format!("fir_{}_{}.npy", taps_label, phase_type);
    // Legacy filters are designed for FS8 (8× upsample): 44.1k → 352.8k or
    // 48k → 384k. Using them at any other ratio mis-applies the cutoff
    // (see ca1af01 commit message and docs/13-...). So we only accept the
    // legacy file when target_rate is one of those two design points.
    let legacy_ok_for_rate = matches!(target_rate_hz, 352_800 | 384_000);

    let search_dirs = [
        // Explicit override for blobs stored outside the repo layout.
        std::env::var_os("AURA_FILTER_DIR").map(std::path::PathBuf::from),
        // Repo root relative to the exe:
        // <root>/desktop-app/src-tauri/target/release/aura-engine.exe
        //   → ../../../../fir-optimizer/output
        std::env::current_exe().ok().and_then(|p| {
            p.parent().map(|d| {
                d.join("..")
                    .join("..")
                    .join("..")
                    .join("..")
                    .join("fir-optimizer")
                    .join("output")
            })
        }),
        // One level shallower, for layouts where fir-optimizer sits next
        // to src-tauri instead of the repo root.
        std::env::current_exe().ok().and_then(|p| {
            p.parent().map(|d| {
                d.join("..")
                    .join("..")
                    .join("..")
                    .join("fir-optimizer")
                    .join("output")
            })
        }),
        // Current working directory (running from a repo-root shell).
        Some(std::path::Path::new("fir-optimizer").join("output")),
    ];

    for dir_opt in &search_dirs {
        if let Some(ref dir) = dir_opt {
            // 1. Prefer the ratio-specific file
            let p = dir.join(&primary);
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
            // 2. Fall back to legacy ONLY when the requested rate matches
            //    the FS8 design point of the legacy blobs.
            if legacy_ok_for_rate {
                let p = dir.join(&legacy);
                if p.exists() {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }

    None
}
