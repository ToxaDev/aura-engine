//! `FdlRing` — GPU-resident frequency-domain delay line ring buffer.
//!
//! Holds P DS-encoded half spectra (NBIN bins at a stride of GPU_BINS) for
//! one stereo pair.
//! The ring is indexed by block number `j`; slot for block `j` is `j % P`.
//!
//! Every `GpuPolyStream` owns its own ring, the Hybrid-Phase pair included:
//! the linear and minimum streams start about 57 blocks apart, so one shared
//! ring could not serve both (coordinator decision K9).

use std::sync::Arc;

use super::GPU_BINS;
use crate::player::convolver::NBIN;

/// The GPU FDL ring for one stereo stream.
pub struct FdlRing {
    /// GPU buffer for left channel spectra: `P × GPU_BINS × 16 B` (DS complex).
    pub buf_l: wgpu::Buffer,
    /// GPU buffer for right channel spectra.
    pub buf_r: wgpu::Buffer,
    /// Number of partitions (P).
    pub partitions: usize,
}

impl FdlRing {
    /// Allocate empty (all-zero) FDL ring buffers on the device. wgpu
    /// zero-initialises a new buffer, so no zeros are uploaded from RAM.
    pub fn new(device: &wgpu::Device, partitions: usize) -> Arc<Self> {
        let slot_bytes = GPU_BINS * 16; // GPU_BINS DS complex values × 16 B each
        let total_bytes = (partitions * slot_bytes) as u64;
        let ring = |label| device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: total_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let buf_l = ring("player/gpu fdl_l");
        let buf_r = ring("player/gpu fdl_r");

        Arc::new(FdlRing {
            buf_l,
            buf_r,
            partitions,
        })
    }

    /// Upload a pre-computed DS-encoded spectrum to slot `j % P`.
    /// `spec_l` and `spec_r` must each be `NBIN * 16` bytes (the bins past
    /// NBIN in the slot stay zero).
    pub fn write_slot(&self, queue: &wgpu::Queue, j: i64, spec_l: &[u8], spec_r: &[u8]) {
        debug_assert_eq!(spec_l.len(), NBIN * 16);
        debug_assert_eq!(spec_r.len(), NBIN * 16);
        let slot = j.rem_euclid(self.partitions as i64) as usize;
        let offset = (slot * GPU_BINS * 16) as u64;
        queue.write_buffer(&self.buf_l, offset, spec_l);
        queue.write_buffer(&self.buf_r, offset, spec_r);
    }
}

// ── DS encoding helpers ──────────────────────────────────────────────────────

/// Encode a slice of f64 complex values (re, im interleaved: [re0, im0, re1, im1, ...])
/// as DS f32 vec4 values.  `dst` must have length `n * 16` bytes (n = src.len() / 2).
pub fn encode_spectrum_ds(src: &[rustfft::num_complex::Complex<f64>], dst: &mut Vec<u8>) {
    dst.clear();
    dst.resize(src.len() * 16, 0);
    encode_spectrum_ds_into(src, dst);
}

/// `encode_spectrum_ds` into a slice of exactly `src.len() * 16` bytes (the
/// filter bank writes straight into the mapped device buffer).
pub fn encode_spectrum_ds_into(src: &[rustfft::num_complex::Complex<f64>], dst: &mut [u8]) {
    debug_assert_eq!(dst.len(), src.len() * 16);
    for (c, d) in src.iter().zip(dst.chunks_exact_mut(16)) {
        let re_hi = c.re as f32;
        let re_lo = (c.re - re_hi as f64) as f32;
        let im_hi = c.im as f32;
        let im_lo = (c.im - im_hi as f64) as f32;
        d[0..4].copy_from_slice(&re_hi.to_le_bytes());
        d[4..8].copy_from_slice(&re_lo.to_le_bytes());
        d[8..12].copy_from_slice(&im_hi.to_le_bytes());
        d[12..16].copy_from_slice(&im_lo.to_le_bytes());
    }
}

/// Decode a DS-encoded complex spectrum back to f64 complex values.
/// `src` must be `n * 16` bytes; result has `n` Complex<f64> values.
pub fn decode_spectrum_ds(src: &[u8]) -> Vec<rustfft::num_complex::Complex<f64>> {
    let n = src.len() / 16;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let base = i * 16;
        let re_hi = f32::from_le_bytes(src[base..base+4].try_into().unwrap());
        let re_lo = f32::from_le_bytes(src[base+4..base+8].try_into().unwrap());
        let im_hi = f32::from_le_bytes(src[base+8..base+12].try_into().unwrap());
        let im_lo = f32::from_le_bytes(src[base+12..base+16].try_into().unwrap());
        let re = (re_hi as f64) + (re_lo as f64);
        let im = (im_hi as f64) + (im_lo as f64);
        out.push(rustfft::num_complex::Complex::new(re, im));
    }
    out
}
