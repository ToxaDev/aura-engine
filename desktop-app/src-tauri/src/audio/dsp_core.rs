
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use std::sync::Arc;
use std::time::Instant;

use crate::audio::processor::DspProcessor;

/// Load a NumPy .npy file containing float64 coefficients.
/// Supports only the simplest npy format: {'descr': '<f8', 'fortran_order': False, 'shape': (N,)}
pub fn load_npy_f64(path: &str) -> Result<Vec<f64>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {}", path, e))?;
    let mut header = [0u8; 10];
    f.read_exact(&mut header)
        .map_err(|e| format!("Read error: {}", e))?;
    // Verify magic: \x93NUMPY
    if &header[0..6] != b"\x93NUMPY" {
        return Err("Not a valid .npy file".to_string());
    }
    let header_len = u16::from_le_bytes([header[8], header[9]]) as usize;
    let mut header_str = vec![0u8; header_len];
    f.read_exact(&mut header_str)
        .map_err(|e| format!("Read error: {}", e))?;
    let header_text = String::from_utf8_lossy(&header_str);

    // Parse shape from header (basic parsing)
    let shape_start = header_text.find("'shape':").ok_or("No shape in header")?;
    let paren_start = header_text[shape_start..]
        .find('(')
        .ok_or("Bad shape format")?
        + shape_start;
    let paren_end = header_text[paren_start..]
        .find(')')
        .ok_or("Bad shape format")?
        + paren_start;
    let shape_str = header_text[paren_start + 1..paren_end]
        .trim()
        .trim_end_matches(',');
    let num_elements: usize = shape_str
        .parse()
        .map_err(|_| format!("Bad shape: {}", shape_str))?;

    // Verify float64
    if !header_text.contains("'<f8'") && !header_text.contains("'<f8'") {
        // Also accept little-endian f8
        if !header_text.contains("f8") {
            return Err(format!(
                "Expected float64 (<f8), got header: {}",
                header_text
            ));
        }
    }

    // Read raw data
    let mut data_bytes = vec![0u8; num_elements * 8];
    f.read_exact(&mut data_bytes)
        .map_err(|e| format!("Read error: {}", e))?;
    let coeffs: Vec<f64> = data_bytes
        .chunks_exact(8)
        .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
        .collect();

    Ok(coeffs)
}

/// Maximum number of taps the FFT-based group-delay estimator will analyse.
/// For a minimum-phase filter, energy is concentrated in the leading
/// portion of the impulse response — the first ~131k taps capture
/// essentially all of the magnitude/phase information that matters for
/// passband group delay. Truncating past this keeps FFT cost bounded
/// (≈ 262k-point FFT instead of hundreds of millions of points on a
/// 30M-tap filter, which previously locked the pipeline for minutes).
///
/// FULL RATIONALE, FAILURE MODES, AND VALIDATION PROCEDURE:
///   docs/11-group-delay-truncation-tradeoff.md
/// Read it before changing this constant. Lowering it below 16 k starts
/// to lose sample accuracy; raising it past ~1 M makes the FFT take
/// seconds and consume hundreds of MB.
const GROUP_DELAY_ANALYSIS_TAPS: usize = 131_072;

/// Estimate the bulk passband group-delay of a minimum-phase FIR filter,
/// weighted by magnitude inside a perceptually-relevant frequency band.
///
/// The trivial time-domain centre-of-gravity (Σ i·h[i] / Σ h[i]) averages
/// delay over the entire spectrum, including the stop-band and ultrasonic
/// passband. For musical material the delay we *care* about aligning is in
/// the 200 Hz–6 kHz range. So we transform h(t) → H(ω), compute group delay
/// τ(ω) = -dφ/dω in samples, and average it weighted by |H(ω)|² inside the
/// target band. Yields a more accurate delay for time-aligning the
/// minimum-phase output with the linear-phase output during hybrid blending.
///
/// **Cost cap:** filters longer than `GROUP_DELAY_ANALYSIS_TAPS` are
/// truncated to that prefix before transformation. This is safe for
/// minimum-phase filters (energy front-loaded) and keeps the analysis
/// bounded — without it, a 30M-tap filter triggers a 134M-point FFT
/// that takes minutes and 2 GB RAM.
///
/// `output_sr` is the sample rate of the audio that will be filtered.
/// Returns delay in **output samples** (rounded, ≥ 0).
pub fn estimate_band_weighted_group_delay(
    coeffs: &[f64],
    output_sr: f64,
    band_lo_hz: f64,
    band_hi_hz: f64,
) -> usize {
    let n_full = coeffs.len();
    if n_full < 8 || output_sr <= 0.0 {
        return 0;
    }
    // Truncate to bounded analysis window for very long filters.
    let n = n_full.min(GROUP_DELAY_ANALYSIS_TAPS);
    // Pad to next power-of-two ≥ 2× analysis length for clean phase
    // differentiation. With n capped at 131072, n_fft maxes out at 262144.
    let n_fft = (n * 2).next_power_of_two().max(1024);
    let mut buf = vec![Complex::new(0.0, 0.0); n_fft];
    for i in 0..n {
        buf[i] = Complex::new(coeffs[i], 0.0);
    }
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n_fft);
    fft.process(&mut buf);

    // Unwrapped phase φ[k]
    let mut phase = vec![0.0f64; n_fft / 2 + 1];
    for k in 0..=n_fft / 2 {
        phase[k] = buf[k].im.atan2(buf[k].re);
    }
    for k in 1..phase.len() {
        let mut d = phase[k] - phase[k - 1];
        while d > std::f64::consts::PI {
            d -= 2.0 * std::f64::consts::PI;
        }
        while d < -std::f64::consts::PI {
            d += 2.0 * std::f64::consts::PI;
        }
        phase[k] = phase[k - 1] + d;
    }

    // Group delay in samples: τ[k] = -(φ[k+1] - φ[k-1]) · n_fft / (4π)
    // (central difference; converts dφ/dω into "samples of delay" at fs).
    let scale = n_fft as f64 / (4.0 * std::f64::consts::PI);
    let bin_hz = output_sr / n_fft as f64;
    let lo_bin = ((band_lo_hz / bin_hz).floor() as usize).max(1);
    let hi_bin = ((band_hi_hz / bin_hz).ceil() as usize).min(phase.len() - 2);
    if hi_bin <= lo_bin {
        return 0;
    }

    let mut weight_sum = 0.0f64;
    let mut tau_weighted = 0.0f64;
    for k in lo_bin..=hi_bin {
        let mag = buf[k].norm();
        let w = mag * mag; // power weighting
        let tau = -(phase[k + 1] - phase[k - 1]) * scale;
        tau_weighted += w * tau;
        weight_sum += w;
    }
    if weight_sum <= 0.0 {
        return 0;
    }
    let avg = tau_weighted / weight_sum;
    avg.round().max(0.0) as usize
}

/// Convert a generic linear-phase FIR filter to a strict Minimum Phase filter using Cepstral folding.
pub fn to_minimum_phase(h_linear: &[f64]) -> Vec<f64> {
    let n = h_linear.len();
    let n_fft = (n * 16).next_power_of_two();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n_fft);
    let ifft = planner.plan_fft_inverse(n_fft);

    let mut buf = vec![Complex::new(0.0, 0.0); n_fft];
    for i in 0..n {
        buf[i] = Complex::new(h_linear[i], 0.0);
    }
    fft.process(&mut buf);

    for c in buf.iter_mut() {
        let mag = c.norm().max(1e-300);
        *c = Complex::new(mag.ln(), 0.0);
    }

    ifft.process(&mut buf);
    let scale = 1.0 / (n_fft as f64);
    for c in buf.iter_mut() {
        *c *= scale;
    }

    let half = n_fft / 2;
    for i in 1..half {
        buf[i] *= 2.0;
    }
    for i in (half + 1)..n_fft {
        buf[i] = Complex::new(0.0, 0.0);
    }

    fft.process(&mut buf);

    for c in buf.iter_mut() {
        *c = c.exp();
    }

    ifft.process(&mut buf);
    let mut min_phase = Vec::with_capacity(n);
    for i in 0..n {
        min_phase.push(buf[i].re * scale);
    }

    // Hann tail fade to kill cepstral truncation ripple.
    // Skip entirely for very short filters where a fade region of any
    // sane length would consume most of the impulse response.
    let min_fade = 64usize;
    if n < 2 * min_fade {
        return min_phase;
    }

    // Use amplitude-aware fade_start instead of a fixed 10% slice:
    // we look from the end backwards for the last sample whose magnitude
    // exceeds 1e-5 × peak. Everything past that point is numerical residue
    // and gets faded — preserves more of the real impulse response than
    // a fixed 10% fade, which on long min-phase filters chops away energy.
    let max_abs = min_phase
        .iter()
        .map(|x| x.abs())
        .fold(0.0f64, f64::max)
        .max(1e-300);
    let threshold = max_abs * 1e-5;
    let mut last_significant = 0usize;
    for i in (0..n).rev() {
        if min_phase[i].abs() > threshold {
            last_significant = i;
            break;
        }
    }
    // Always keep at least 64 samples of fade and cap the fade region at 10%
    // of n so a pathological filter cannot end up un-faded.
    let max_fade = (n / 10).max(min_fade);
    let raw_start = last_significant.saturating_add(1);
    let fade_start = raw_start
        .max(n.saturating_sub(max_fade))
        .min(n.saturating_sub(min_fade));
    let fade_len = n - fade_start;
    if fade_len > 1 {
        for i in fade_start..n {
            let x = (i - fade_start) as f64 / (fade_len - 1) as f64;
            let fade = 0.5 * (1.0 + (std::f64::consts::PI * x).cos());
            min_phase[i] *= fade;
        }
    }

    // The tail fade drops a small amount of impulse energy, drifting the DC
    // gain below the original filter's. Renormalize back to the source DC
    // gain so magnitude response at 0 Hz (and overall level) is preserved
    // exactly (docs/10-gpu-bugfix-spec.md Fix #3).
    let dc_target: f64 = h_linear.iter().sum();
    let dc_current: f64 = min_phase.iter().sum();
    if dc_current.abs() > 1e-15 && dc_target.abs() > 1e-15 {
        let renorm = dc_target / dc_current;
        for v in min_phase.iter_mut() {
            *v *= renorm;
        }
    }

    min_phase
}

pub struct CpuDspProcessor {
    b_size: usize,
    num_blocks: usize,

    h_blocks: Vec<Vec<Complex<f64>>>,

    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,

    // Overlap-Save state (matching Chrome extension)
    save_buf_l: Vec<f64>,
    save_buf_r: Vec<f64>,

    // Frequency-domain delay line for partitioned convolution
    f_delay_l: Vec<Vec<Complex<f64>>>,
    f_delay_r: Vec<Vec<Complex<f64>>>,
    f_idx: usize,

    // Accumulation buffers for spreading work across calls (Kahan Summation)
    y_accum_l: Vec<Complex<f64>>,
    y_accum_r: Vec<Complex<f64>>,
    y_comp_l: Vec<Complex<f64>>,
    y_comp_r: Vec<Complex<f64>>,
    k_step: usize,     // Current partition index in the accumulation
    block_ready: bool, // Whether a full block FFT has been done

    // Pre-allocated FFT scratch buffers (eliminates hot-path heap allocation)
    fft_scratch_l: Vec<Complex<f64>>,
    fft_scratch_r: Vec<Complex<f64>>,

    // Double-buffer I/O (like Chrome extension)
    in_buf_l: Vec<f64>,
    in_buf_r: Vec<f64>,
    out_buf_l: Vec<f64>,
    out_buf_r: Vec<f64>,
    io_pos: usize,

    // Noise shaping variables moved to process.rs

    // Diagnostics
    pub clip_count: u64,
    pub nan_count: u64,
    pub max_abs_val: f64,
    call_count: u64,
    total_cpu_time_us: u64,
}

impl CpuDspProcessor {
    /// Canonical CPU OLA block size. Kept as an associated fn (mirroring
    /// `GpuDspProcessor::block_size`) so pipeline code can size flush/trim
    /// arithmetic BEFORE a processor instance exists.
    #[inline]
    pub fn block_size_for(_target_taps: usize) -> usize {
        32768 // For offline CPU OLA, 32768 optimally fits L3 caches
    }

    /// Total algorithmic output latency of the CPU convolver in samples.
    /// 2 × block_size: one block inherent to overlap-save + one block from
    /// the deferred out_buf read in `process_audio_internal` (proven by the
    /// `convolver_latency_is_two_blocks_and_unity_gain` test).
    #[inline]
    pub fn output_latency_for(target_taps: usize) -> usize {
        2 * Self::block_size_for(target_taps)
    }

    /// Create processor from pre-computed coefficients (loaded from .npy file)
    pub fn new_with_coefficients(coeffs: &[f64]) -> Self {
        use rayon::prelude::*;

        let t0 = Instant::now();
        let target_taps = coeffs.len();
        crate::aelog!("[DSP] ═══════════════════════════════════════");
        crate::aelog!("[DSP] Loading CUSTOM filter: {} taps", target_taps);

        let b_size = Self::block_size_for(target_taps);

        let num_blocks = (target_taps + b_size - 1) / b_size;
        let n = b_size * 2;
        crate::aelog!("[DSP] Partitions: {} × {} = FFT {}", num_blocks, b_size, n);

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);

        let f_delay_l: Vec<Vec<Complex<f64>>> = (0..num_blocks)
            .into_par_iter()
            .map(|_| vec![Complex::new(0.0, 0.0); n])
            .collect();
        let f_delay_r: Vec<Vec<Complex<f64>>> = (0..num_blocks)
            .into_par_iter()
            .map(|_| vec![Complex::new(0.0, 0.0); n])
            .collect();

        // FFT the custom coefficients into OLA blocks (parallel)
        let h_blocks: Vec<Vec<Complex<f64>>> = (0..num_blocks)
            .into_par_iter()
            .map(|b| {
                let mut planner = FftPlanner::new();
                let fft = planner.plan_fft_forward(n);
                let mut block = vec![Complex::new(0.0, 0.0); n];
                let offset = b * b_size;
                for i in 0..b_size {
                    if offset + i < target_taps {
                        block[i] = Complex::new(coeffs[offset + i], 0.0);
                    }
                }
                fft.process(&mut block);
                block
            })
            .collect();

        let mut core = Self {
            b_size,
            num_blocks,
            h_blocks,
            fft,
            ifft,
            save_buf_l: vec![0.0; b_size],
            save_buf_r: vec![0.0; b_size],
            f_delay_l,
            f_delay_r,
            f_idx: 0,
            y_accum_l: vec![Complex::new(0.0, 0.0); n],
            y_accum_r: vec![Complex::new(0.0, 0.0); n],
            y_comp_l: vec![Complex::new(0.0, 0.0); n],
            y_comp_r: vec![Complex::new(0.0, 0.0); n],
            k_step: 0,
            block_ready: false,
            fft_scratch_l: vec![Complex::new(0.0, 0.0); n],
            fft_scratch_r: vec![Complex::new(0.0, 0.0); n],
            in_buf_l: vec![0.0; b_size],
            in_buf_r: vec![0.0; b_size],
            out_buf_l: vec![0.0; b_size],
            out_buf_r: vec![0.0; b_size],
            io_pos: 0,
            clip_count: 0,
            nan_count: 0,
            max_abs_val: 0.0,
            // Dithering vars removed
            call_count: 0,
            total_cpu_time_us: 0,
        };

        core.start_new_block();
        crate::aelog!(
            "[DSP] Custom filter loaded in {:.2}s",
            t0.elapsed().as_secs_f64()
        );
        crate::aelog!("[DSP] ═══════════════════════════════════════");
        core
    }


    /// Start a new convolution block: forward FFT of [save_buf | in_buf]
    /// ZERO-ALLOC: uses pre-allocated scratch buffers + swap instead of vec! allocation
    fn start_new_block(&mut self) {
        // Fill pre-allocated scratch buffers (Overlap-Save: [previous_block | current_block])
        for i in 0..self.b_size {
            self.fft_scratch_l[i] = Complex::new(self.save_buf_l[i], 0.0);
            self.fft_scratch_r[i] = Complex::new(self.save_buf_r[i], 0.0);
            self.fft_scratch_l[i + self.b_size] = Complex::new(self.in_buf_l[i], 0.0);
            self.fft_scratch_r[i + self.b_size] = Complex::new(self.in_buf_r[i], 0.0);
        }

        // Save current input for next block's overlap
        self.save_buf_l.copy_from_slice(&self.in_buf_l);
        self.save_buf_r.copy_from_slice(&self.in_buf_r);

        // FFT in-place on scratch buffers (no allocation)
        self.fft.process(&mut self.fft_scratch_l);
        self.fft.process(&mut self.fft_scratch_r);

        // Store in circular delay line via O(1) swap (no copy, no allocation)
        self.f_idx = if self.f_idx == 0 {
            self.num_blocks - 1
        } else {
            self.f_idx - 1
        };
        std::mem::swap(&mut self.f_delay_l[self.f_idx], &mut self.fft_scratch_l);
        std::mem::swap(&mut self.f_delay_r[self.f_idx], &mut self.fft_scratch_r);

        // Zero Kahan accumulators
        for c in self.y_accum_l.iter_mut() { *c = Complex::new(0.0, 0.0); }
        for c in self.y_accum_r.iter_mut() { *c = Complex::new(0.0, 0.0); }
        for c in self.y_comp_l.iter_mut() { *c = Complex::new(0.0, 0.0); }
        for c in self.y_comp_r.iter_mut() { *c = Complex::new(0.0, 0.0); }
        self.k_step = 0;
        self.block_ready = true;
    }

    fn process_partitions(&mut self, chunk_size: usize) {
        if !self.block_ready || self.k_step >= self.num_blocks {
            return;
        }
        use rayon::prelude::*;
        let end_k = std::cmp::min(self.k_step + chunk_size, self.num_blocks);

        let k_step = self.k_step;
        let num_blocks = self.num_blocks;
        let f_idx = self.f_idx;
        let h_blocks = &self.h_blocks;
        let f_delay_l = &self.f_delay_l;
        let f_delay_r = &self.f_delay_r;
        let n = self.b_size * 2;

        // Adaptive chunk size for parallel decomposition:
        // Larger FFT sizes get larger chunks to reduce rayon overhead.
        let par_chunk = if n >= 65536 {
            4096
        } else if n >= 32768 {
            2048
        } else {
            512
        };

        // Spread the frequency bins across CPU cores.
        // We evaluate K partitions sequentially INSIDE each chunk for L1/L2 cache locality.
        self.y_accum_l
            .par_chunks_mut(par_chunk)
            .zip(self.y_accum_r.par_chunks_mut(par_chunk))
            .zip(self.y_comp_l.par_chunks_mut(par_chunk))
            .zip(self.y_comp_r.par_chunks_mut(par_chunk))
            .enumerate()
            .for_each(|(chunk_idx, (((y_l_chunk, y_r_chunk), c_l_chunk), c_r_chunk))| {
                let offset = chunk_idx * par_chunk;
                let chunk_len = y_l_chunk.len();

                for k in k_step..end_k {
                    let hist_idx = (f_idx + k) % num_blocks;
                    let h_slice = &h_blocks[k][offset..offset + chunk_len];
                    let dl_slice = &f_delay_l[hist_idx][offset..offset + chunk_len];
                    let dr_slice = &f_delay_r[hist_idx][offset..offset + chunk_len];

                    for i in 0..chunk_len {
                        // Kahan summation for Left
                        let prod_l = dl_slice[i] * h_slice[i];
                        let y_l = prod_l - c_l_chunk[i];
                        let t_l = y_l_chunk[i] + y_l;
                        c_l_chunk[i] = (t_l - y_l_chunk[i]) - y_l;
                        y_l_chunk[i] = t_l;

                        // Kahan summation for Right
                        let prod_r = dr_slice[i] * h_slice[i];
                        let y_r = prod_r - c_r_chunk[i];
                        let t_r = y_r_chunk[i] + y_r;
                        c_r_chunk[i] = (t_r - y_r_chunk[i]) - y_r;
                        y_r_chunk[i] = t_r;
                    }
                }
            });

        self.k_step = end_k;
    }

    /// Finish block: IFFT and extract valid output
    fn finish_block(&mut self) {
        // Complete any remaining partitions
        if self.k_step < self.num_blocks {
            self.process_partitions(self.num_blocks - self.k_step);
        }

        let n = self.b_size * 2;
        let scale = 1.0 / n as f64;

        self.ifft.process(&mut self.y_accum_l);
        self.ifft.process(&mut self.y_accum_r);

        // Overlap-Save: second half is the valid output
        for i in 0..self.b_size {
            self.out_buf_l[i] = self.y_accum_l[i + self.b_size].re * scale;
            self.out_buf_r[i] = self.y_accum_r[i + self.b_size].re * scale;
        }

        self.block_ready = false;
    }

    /// Main processing entry point — mirrors Chrome extension's process() function.
    /// Spreads partitioned convolution work across multiple small calls.
    fn process_audio_internal(
        &mut self,
        in_l: &[f64],
        in_r: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        num_frames: usize,
    ) {
        if num_frames == 0 {
            return;
        }

        let t0 = Instant::now();

        // Dither generation removed (now handled in process.rs)

        // How many process() calls fit in one B block?
        let calls_per_block = std::cmp::max(1, self.b_size / num_frames);
        let k_chunks = (self.num_blocks + calls_per_block - 1) / calls_per_block;

        // Process a fraction of partitions (spread work)
        self.process_partitions(k_chunks);

        for i in 0..num_frames {
            // Store input (f64 direct — no precision loss)
            self.in_buf_l[self.io_pos] = in_l[i];
            self.in_buf_r[self.io_pos] = in_r[i];

            // Read output from PREVIOUS block's results
            let mut out_val_l = self.out_buf_l[self.io_pos];
            let mut out_val_r = self.out_buf_r[self.io_pos];

            // Diagnostics: track NaN, clipping, and max amplitude
            if out_val_l.is_nan() || out_val_l.is_infinite() {
                self.nan_count += 1;
                out_val_l = 0.0;
            }
            if out_val_r.is_nan() || out_val_r.is_infinite() {
                self.nan_count += 1;
                out_val_r = 0.0;
            }
            let abs_l = out_val_l.abs();
            let abs_r = out_val_r.abs();
            if abs_l > self.max_abs_val {
                self.max_abs_val = abs_l;
            }
            if abs_r > self.max_abs_val {
                self.max_abs_val = abs_r;
            }
            if abs_l > 1.0 || abs_r > 1.0 {
                self.clip_count += 1;
            }
            // NOTE: No clamp here — let signal flow through naturally.
            // Converter handles normalization via apply_headroom().
            // Player handles clipping at the ASIO output stage.

            out_l[i] = out_val_l;
            out_r[i] = out_val_r;

            self.io_pos += 1;

            // Reached block boundary: finish current, start next
            if self.io_pos >= self.b_size {
                self.finish_block();
                self.start_new_block();
                self.io_pos = 0;
            }
        }

        // Timing diagnostics
        let elapsed_us = t0.elapsed().as_micros() as u64;
        self.call_count += 1;
        self.total_cpu_time_us += elapsed_us;

        if self.call_count <= 5 || self.call_count % 5000 == 0 {
            let avg_ms = self.total_cpu_time_us as f64 / self.call_count as f64 / 1000.0;
            crate::aelog!(
                "[DSP] #{} frames={} time={:.2}ms avg={:.2}ms clips={} nan={} (b_size={})",
                self.call_count,
                num_frames,
                elapsed_us as f64 / 1000.0,
                avg_ms,
                self.clip_count,
                self.nan_count,
                self.b_size
            );
        }
    }
}

impl DspProcessor for CpuDspProcessor {
    fn process_audio(
        &mut self,
        in_l: &[f64],
        in_r: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        num_frames: usize,
    ) {
        self.process_audio_internal(in_l, in_r, out_l, out_r, num_frames)
    }

    fn block_size(&self) -> usize {
        self.b_size
    }

    fn output_latency(&self) -> usize {
        2 * self.b_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Kaiser-windowed sinc lowpass (linear-phase, symmetric).
    fn linear_phase_lowpass(taps: usize, fc_norm: f64) -> Vec<f64> {
        let half = taps / 2;
        let beta = 8.0;
        // Bessel I0
        let i0 = |x: f64| {
            let mut sum = 1.0;
            let mut term = 1.0;
            let x2 = (x / 2.0).powi(2);
            for k in 1..50 {
                term *= x2 / (k as f64).powi(2);
                sum += term;
                if term < 1e-15 * sum {
                    break;
                }
            }
            sum
        };
        let i0_beta = i0(beta);
        let mut h = Vec::with_capacity(taps);
        let mut s = 0.0;
        for i in 0..taps {
            let n = i as f64 - half as f64;
            let sinc = if n.abs() < 1e-12 {
                fc_norm
            } else {
                (std::f64::consts::PI * fc_norm * n).sin() / (std::f64::consts::PI * n)
            };
            let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
            let kaiser_arg = beta * (1.0 - arg * arg).max(0.0).sqrt();
            let v = sinc * i0(kaiser_arg) / i0_beta;
            h.push(v);
            s += v;
        }
        for v in h.iter_mut() {
            *v /= s;
        }
        h
    }

    #[test]
    fn linear_phase_group_delay_matches_half_taps() {
        // 1001-tap symmetric lowpass at fc=0.45 fs/2 (well inside passband
        // for the 200–6000 Hz query at 48 kHz). Group delay should be ~500.
        let h = linear_phase_lowpass(1001, 0.45);
        let gd = estimate_band_weighted_group_delay(&h, 48_000.0, 200.0, 6_000.0);
        let expected = 500;
        assert!(
            gd.abs_diff(expected) <= 2,
            "linear-phase delay should be ~{} samples, got {}",
            expected,
            gd
        );
    }

    #[test]
    fn min_phase_group_delay_is_small() {
        // Same lowpass cepstrally folded to minimum phase. Group delay in
        // the perceptually relevant band should be much smaller than the
        // linear-phase analogue (well under taps/4).
        let h_lin = linear_phase_lowpass(1001, 0.45);
        let h_min = to_minimum_phase(&h_lin);
        let gd = estimate_band_weighted_group_delay(&h_min, 48_000.0, 200.0, 6_000.0);
        assert!(
            gd < 250,
            "min-phase delay should be << taps/2 = 500, got {}",
            gd
        );
    }

    #[test]
    fn band_weighted_handles_degenerate_input() {
        // Empty / too-short / silent — no panics, returns 0.
        assert_eq!(estimate_band_weighted_group_delay(&[], 48_000.0, 200.0, 6_000.0), 0);
        assert_eq!(estimate_band_weighted_group_delay(&[1.0; 4], 48_000.0, 200.0, 6_000.0), 0);
        assert_eq!(estimate_band_weighted_group_delay(&[0.0; 1024], 48_000.0, 200.0, 6_000.0), 0);
    }

    #[test]
    fn min_phase_preserves_dc_gain() {
        // Cepstral folding preserves |H(e^{jω})|; DC gain (Σ h[i]) of the
        // min-phase analogue should match the linear-phase original within
        // a small tolerance (we apply a tail Hann fade that may drop a
        // tiny amount of energy in the residue tail).
        let h_lin = linear_phase_lowpass(1001, 0.45);
        let h_min = to_minimum_phase(&h_lin);
        let dc_lin: f64 = h_lin.iter().sum();
        let dc_min: f64 = h_min.iter().sum();
        let ratio = dc_min / dc_lin;
        assert!(
            (ratio - 1.0).abs() < 0.02,
            "DC gain drift: lin={:.6} min={:.6} ratio={:.6}",
            dc_lin, dc_min, ratio
        );
    }

    // ── Helper: drive a processor exactly like process.rs does ──
    // Fixed 32768-sample chunks (mono into L, zeros into R), then a zero
    // flush, returning the full left-channel output stream.
    fn run_through(dsp: &mut CpuDspProcessor, input: &[f64], flush_blocks: usize) -> Vec<f64> {
        let b = 32768usize;
        let zeros = vec![0.0f64; b];
        let mut ol = vec![0.0f64; b];
        let mut or_ = vec![0.0f64; b];
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < input.len() {
            let actual = (pos + b).min(input.len()) - pos;
            dsp.process_audio(&input[pos..pos + actual], &zeros[..actual],
                              &mut ol[..actual], &mut or_[..actual], actual);
            out.extend_from_slice(&ol[..actual]);
            pos += actual;
        }
        for _ in 0..flush_blocks {
            dsp.process_audio(&zeros, &zeros, &mut ol, &mut or_, b);
            out.extend_from_slice(&ol);
        }
        out
    }

    fn argmax_abs(x: &[f64]) -> (usize, f64) {
        let mut idx = 0;
        let mut best = 0.0f64;
        for (i, &v) in x.iter().enumerate() {
            if v.abs() > best.abs() {
                best = v;
                idx = i;
            }
        }
        (idx, best)
    }

    /// AUDIT (latency): the partitioned overlap-save convolver delays the
    /// signal by exactly TWO b_size blocks — one inherent OLA block plus one
    /// extra from the deferred out_buf read in process_audio_internal. Unity
    /// gain must be exact.
    ///
    /// This is the ground truth behind `DspProcessor::output_latency()`
    /// (2×b_size on CPU); process.rs / hybrid_mixer.rs trim via that method.
    #[test]
    fn convolver_latency_is_two_blocks_and_unity_gain() {
        let b = 32768usize;
        let coeffs = vec![1.0f64]; // identity
        let mut dsp = CpuDspProcessor::new_with_coefficients(&coeffs);

        let total_in = 4 * b;
        let mut input = vec![0.0f64; total_in];
        input[0] = 1.0;

        let out = run_through(&mut dsp, &input, 3);
        let (idx, val) = argmax_abs(&out);
        println!("[AUDIT] identity impulse landed at output idx={idx} (val={val:.9}), b_size={b}");

        assert!((val - 1.0).abs() < 1e-9, "expected unity gain, got {val}");
        assert_eq!(idx, 2 * b, "true OLA latency is 2*b_size (process.rs trims only b_size)");
    }

    /// AUDIT (latency vs partition count): the 2-block latency must NOT grow
    /// with the number of FFT partitions — a 70000-tap filter forces 3
    /// partitions yet the identity impulse (coeff[0]=1, rest 0) must still
    /// land at exactly 2*b_size.
    #[test]
    fn convolver_latency_independent_of_partition_count() {
        let b = 32768usize;
        let mut coeffs = vec![0.0f64; 70_000]; // 3 partitions (ceil(70000/32768)=3)
        coeffs[0] = 1.0; // identity impulse response
        let mut dsp = CpuDspProcessor::new_with_coefficients(&coeffs);

        let total_in = 6 * b;
        let mut input = vec![0.0f64; total_in];
        input[0] = 1.0;

        let out = run_through(&mut dsp, &input, 5);
        let (idx, val) = argmax_abs(&out);
        println!("[AUDIT] 3-partition identity impulse at idx={idx} (val={val:.9})");
        assert!((val - 1.0).abs() < 1e-9, "expected unity gain, got {val}");
        assert_eq!(idx, 2 * b, "latency must be 2*b_size regardless of partition count");
    }

    /// AUDIT (level): a band-limited sine inside the passband must keep its
    /// amplitude (RMS-based, robust to discrete-peak sampling) and produce no
    /// NaN/Inf. Confirms the convolution + 1/N normalization preserve level.
    #[test]
    fn passband_sine_rms_preserved() {
        let b = 32768usize;
        let taps = 4097usize;
        let h = linear_phase_lowpass(taps, 0.5); // cutoff 0.25*fs
        let mut dsp = CpuDspProcessor::new_with_coefficients(&h);

        // Tone at 0.1*fs — well inside the passband.
        let total_in = 6 * b;
        let amp = 0.5;
        let input: Vec<f64> = (0..total_in)
            .map(|n| amp * (2.0 * std::f64::consts::PI * 0.1 * n as f64).sin())
            .collect();

        let out = run_through(&mut dsp, &input, 3);
        assert!(out.iter().all(|v| v.is_finite()), "output contains NaN/Inf");

        // Steady-state RMS away from edges (latency=2*b + filter ramp).
        let lat = 2 * b + (taps - 1) / 2;
        let start = lat + 8192;
        let end = (lat + total_in - 8192).min(out.len());
        let seg = &out[start..end];
        let rms = (seg.iter().map(|v| v * v).sum::<f64>() / seg.len() as f64).sqrt();
        let measured_amp = rms * std::f64::consts::SQRT_2; // sine amplitude from RMS
        let expected_rms = amp / std::f64::consts::SQRT_2;
        println!("[AUDIT] passband sine: measured amp={measured_amp:.6} (input {amp}), rms={rms:.6} expected {expected_rms:.6}");
        assert!((measured_amp - amp).abs() < 2e-3,
            "passband amplitude not preserved: got {measured_amp:.6}, expected {amp}");
    }

    /// REGRESSION GUARD (end-to-end alignment): replicate the EXACT trim
    /// logic of the standard post-FIR path in process.rs and verify an
    /// impulse at input position P lands at output position P.
    ///
    /// History: this test used to be `#[ignore]`d, documenting the HIGH-1
    /// defect where process.rs trimmed only ONE b_size block while the CPU
    /// convolver's true latency is 2×b_size (leading silence + tail
    /// truncation). process.rs now trims `dsp.output_latency() + group_delay`
    /// and flushes `taps + output_latency()`, which this test mirrors.
    #[test]
    fn process_rs_standard_trim_alignment() {
        let b = 32768usize;
        let taps = 1001usize;
        let group_delay = (taps - 1) / 2;
        let h = linear_phase_lowpass(taps, 0.25);
        let mut dsp = CpuDspProcessor::new_with_coefficients(&h);
        let ola_latency = dsp.output_latency();
        assert_eq!(ola_latency, 2 * b, "CPU convolver latency contract");

        let total_output_samples = 4 * b;
        let impulse_pos = 2 * b + 123; // safely inside, survives truncation
        let mut input = vec![0.0f64; total_output_samples];
        input[impulse_pos] = 1.0;

        // ── replicate process.rs: pass1 (real) + pass2 (flush) ──
        let zeros = vec![0.0f64; b];
        let mut ol = vec![0.0f64; b];
        let mut or_ = vec![0.0f64; b];
        let mut out_l: Vec<f64> = Vec::new();
        let mut pos = 0;
        while pos < total_output_samples {
            let actual = (pos + b).min(total_output_samples) - pos;
            dsp.process_audio(&input[pos..pos + actual], &zeros[..actual],
                              &mut ol[..actual], &mut or_[..actual], actual);
            out_l.extend_from_slice(&ol[..actual]);
            pos += actual;
        }
        let flush_samples = taps + ola_latency;
        let flush_blocks = (flush_samples + b - 1) / b;
        for _ in 0..flush_blocks {
            dsp.process_audio(&zeros, &zeros, &mut ol, &mut or_, b);
            out_l.extend_from_slice(&ol);
        }

        // process.rs trim: output_latency + group delay
        let total_trim = ola_latency + group_delay;
        out_l.drain(..total_trim);
        out_l.truncate(total_output_samples);

        let (idx, _v) = argmax_abs(&out_l);
        println!("[AUDIT] standard-path: impulse fed at {impulse_pos}, recovered at {idx} (delta {})",
                 idx as i64 - impulse_pos as i64);
        assert_eq!(idx, impulse_pos,
            "impulse misaligned by {} samples → leading silence + tail truncation",
            idx as i64 - impulse_pos as i64);
    }
}
