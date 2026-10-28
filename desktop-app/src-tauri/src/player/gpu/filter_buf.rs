//! `GpuFilterBank` — filter spectra uploaded to the GPU in DS format.
//!
//! One `GpuFilterBank` per `Bank` per process.  Created on demand by
//! `GpuPolyCtx::bank_cache` and destroyed when the LRU entry is evicted.
//!
//! Layout of `h_freq` buffer:
//!   branch p, partition k → offset `(p * P + k) * GPU_BINS * 16` bytes
//! (the half spectrum, NBIN bins; the bins up to GPU_BINS are zero)
//! where P = `partitions` (max over branches, same for all here since we
//! use the same sub-filter length for all branches — polyphase produces equal-
//! length branches when the total filter length is a multiple of L, otherwise
//! the last branch may be one tap shorter; we pad the GPU upload to P
//! partitions per branch regardless).

use std::sync::Arc;
use rayon::prelude::*;

use super::GPU_BINS;
use super::fdl::encode_spectrum_ds_into;
use crate::player::convolver::{Bank, NBIN};

/// GPU-resident filter spectra for one `Bank`.
pub struct GpuFilterBank {
    /// h_freq buffer: `L × P × GPU_BINS × 16 B` (DS complex vec4).
    pub h_freq: wgpu::Buffer,
}

impl GpuFilterBank {
    /// Upload `bank.spectra` to the device.  Called at most once per (path, L).
    ///
    /// The buffer is created mapped and every (branch, partition) slot is
    /// encoded straight into it in parallel (rayon): no per-slot vectors and
    /// no assembled copy in RAM. The bytes are the ones the slot-by-slot
    /// encoding gave.
    pub fn from_bank(device: &wgpu::Device, _queue: &wgpu::Queue, bank: &Bank) -> Arc<Self> {
        let l = bank.l;
        let p = bank.max_partitions();
        let slot_bytes = GPU_BINS * 16;
        let total_bytes = l * p * slot_bytes;

        let h_freq = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player/gpu h_freq"),
            size: total_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: true,
        });
        {
            let mut view = h_freq.slice(..).get_mapped_range_mut();
            fill_slots(bank, &mut view);
        }
        h_freq.unmap();

        crate::aelog!(
            "[player/gpu] GpuFilterBank: L={} P={} bins={} → {:.0} MB",
            l, p, GPU_BINS,
            total_bytes as f64 / (1 << 20) as f64
        );

        Arc::new(GpuFilterBank { h_freq })
    }
}

/// The `h_freq` image of `bank` in `dst` (`L × P × GPU_BINS × 16` bytes):
/// slot `[branch * P + k]` holds partition k's NBIN bins DS-encoded, then
/// zeros; a partition the branch does not have is all zeros.
pub fn fill_slots(bank: &Bank, dst: &mut [u8]) {
    let p = bank.max_partitions();
    let slot_bytes = GPU_BINS * 16;
    debug_assert_eq!(dst.len(), bank.l * p * slot_bytes);
    dst.par_chunks_mut(slot_bytes).enumerate().for_each(|(idx, slot)| {
        let spectra = bank.branch_spectra(idx / p);
        let k = idx % p;
        let (enc, pad) = slot.split_at_mut(NBIN * 16);
        if k < spectra.len() {
            encode_spectrum_ds_into(&spectra[k], enc);
        } else {
            enc.fill(0);
        }
        pad.fill(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::convolver::{Alignment, BLOCK};
    use super::super::fdl::encode_spectrum_ds;

    /// The one-buffer image is the slot-by-slot encoding laid out at the
    /// GPU stride, byte for byte, padding and missing partitions zero; and
    /// every slot and branch starts on a 256-byte boundary.
    #[test]
    fn one_buffer_image_is_the_slot_by_slot_encoding() {
        assert_eq!((GPU_BINS * 16) % 256, 0);
        assert!(GPU_BINS >= NBIN && GPU_BINS - NBIN < 16);
        // Branches of unequal partition counts: 2·B·L + 1 taps at L = 3.
        let taps = 2 * BLOCK * 3 + 1;
        let h: Vec<f64> = (0..taps).map(|i| ((i as f64) * 0.37).sin() * (-(i as f64) / 40_000.0).exp()).collect();
        let bank = Bank::from_coeffs("test", &h, 3, Alignment::None, 132_300);
        let p = bank.max_partitions();
        let slot = GPU_BINS * 16;
        let mut img = vec![0xAAu8; 3 * p * slot];
        fill_slots(&bank, &mut img);
        let mut want = vec![0u8; 3 * p * slot];
        for b in 0..3 {
            for (k, s) in bank.branch_spectra(b).iter().enumerate() {
                let mut enc = Vec::new();
                encode_spectrum_ds(s, &mut enc);
                let off = (b * p + k) * slot;
                want[off..off + enc.len()].copy_from_slice(&enc);
            }
        }
        assert_eq!(bank.branch_spectra(2).len(), p - 1, "a branch with a partition fewer");
        assert!(img == want, "the image differs from the slot-by-slot encoding");
    }
}
