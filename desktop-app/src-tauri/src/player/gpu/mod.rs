//! Player GPU streaming convolver.
//!
//! `GpuPolyStream` implements the `Stage` trait and performs partitioned
//! overlap-save convolution on the GPU using DS (double-single f32) precision
//! for the CMUL-ACCUM inner loop.  The FFT and IFFT run on the CPU with
//! realfft f64 (identical to `PolyStream`: half spectra); only the
//! multiply-accumulate over the P partitions is offloaded to the GPU.
//!
//! One `GpuPolyCtx` per process (player-owned wgpu device, never shared with
//! the engine's converter).  One `GpuFilterBank` per `Bank` (holds h_freq on
//! device, ~480 MB for 30M linear FS8).  Each `GpuPolyStream` owns its own
//! `FdlRing` — including both streams in an HP/αHP pair.
//!
//! House rule: nothing in `audio/` is imported except the public GPU API
//! (`dxgi_memory`).

pub mod bank_cache;
pub mod ctx;
pub mod fdl;
pub mod filter_buf;
pub mod poly_stream;

#[cfg(any(test, feature = "gpu-player-tests"))]
pub mod tests;

#[allow(unused_imports)]
pub use ctx::GpuPolyCtx;
#[allow(unused_imports)]
pub use poly_stream::GpuPolyStream;

use crate::player::convolver::{BLOCK, NBIN};

/// NFFT = 2 * BLOCK (same as the CPU PolyStream).
pub const NFFT: usize = 2 * BLOCK;

/// Bins per spectrum slot on the device: the half spectrum's NBIN = NFFT/2 + 1
/// rounded up to a multiple of 16, so every slot and every branch starts on a
/// 256-byte boundary (storage binding offsets must). The bins past NBIN are
/// zero in the filter and the delay line, so they accumulate zero and are
/// never read back.
pub const GPU_BINS: usize = (NBIN + 15) / 16 * 16;

/// Returns the VRAM in bytes required for one `GpuPolyStream` with `taps`
/// total taps at polyphase factor `l`.  Every stream owns its own FdlRing.
///
/// Per-branch sub-filter: `taps / l` taps.
/// Partitions per branch: P = ⌈sub_taps / BLOCK⌉.
/// FDL ring (owned, two channels): 2 * P * GPU_BINS * 16 B.
/// h_freq per branch (l branches): l * P * GPU_BINS * 16 B.
/// Accumulators (l branches × 2 channels): l * 2 * GPU_BINS * 16 B.
/// Readback staging (l × 2 × GPU_BINS × 16 B).
/// Note: the GpuFilterBank (h_freq buffer) is shared via the LRU cache;
/// this formula counts it once per stream to give a conservative upper bound
/// for the DXGI free-VRAM check.
pub fn vram_demand(taps: usize, l: usize) -> u64 {
    let sub_taps = (taps + l - 1) / l;
    let p = partitions(sub_taps);
    let bins = GPU_BINS;
    let bds = 16usize; // bytes per DS complex (vec4 f32)

    let fdl     = 2 * p * bins * bds;   // owned FdlRing (L + R)
    let h_freq  = l * p * bins * bds;   // h_freq buffer (all branches)
    let acc     = l * 2 * bins * bds;   // accumulators
    let staging = l * 2 * bins * bds;   // readback staging

    (fdl + h_freq + acc + staging) as u64
}

/// Returns the combined VRAM demand for an HP/αHP pair (lin + min streams),
/// each owning its own FdlRing.  The GpuFilterBank for each bank is separate
/// (lin and min use different filter banks), so both are counted.
pub fn vram_demand_hp(taps: usize, l: usize) -> u64 {
    vram_demand(taps, l).saturating_mul(2)
}

/// Number of OLS partitions for a sub-filter of `taps` taps.
pub fn partitions(taps: usize) -> usize {
    (taps + BLOCK - 1) / BLOCK
}
