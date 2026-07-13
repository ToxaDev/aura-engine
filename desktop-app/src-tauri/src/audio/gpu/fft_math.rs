use super::processor::GpuDspProcessor;
use rustfft::{num_complex::Complex, FftPlanner};

impl GpuDspProcessor {
    pub(crate) fn bessel_i0(x: f64) -> f64 {
        let mut sum = 1.0;
        let mut term = 1.0;
        let mut k = 1.0;
        while term > 1e-12 {
            let part = x / (2.0 * k);
            term *= part * part;
            sum += term;
            k += 1.0;
        }
        sum
    }

    /// Legacy runtime FIR generator (f32, fc=0.45). No longer part of the
    /// conversion pipeline: the GPU processor is now built with a zeroed
    /// spectrum and only ever receives pre-computed .npy coefficients via
    /// `new_with_coefficients`. Kept for reference/experiments.
    #[allow(dead_code)]
    pub(crate) fn generate_fir(total_taps: usize, actual_taps: usize, window_type: i32) -> Vec<f32> {
        let pi = std::f64::consts::PI;
        let fc = 0.45;
        let mut h = vec![0.0f32; total_taps];
        for i in 0..actual_taps {
            if i % 500_000 == 0 && i > 0 && crate::audio::cancel_flag::check() {
                break;
            }
            let nd = i as f64 - (actual_taps as f64 - 1.0) / 2.0;
            let sinc = if nd == 0.0 {
                2.0 * fc
            } else {
                (2.0 * pi * fc * nd).sin() / (pi * nd)
            };
            let n_norm = i as f64 / (actual_taps as f64 - 1.0);
            let window = match window_type {
                0 => 0.54 - 0.46 * (2.0 * pi * n_norm).cos(),
                1 => 0.5 * (1.0 - (2.0 * pi * n_norm).cos()),
                2 => 0.42 - 0.5 * (2.0 * pi * n_norm).cos() + 0.08 * (4.0 * pi * n_norm).cos(),
                3 => {
                    0.355768 - 0.487396 * (2.0 * pi * n_norm).cos()
                        + 0.144232 * (4.0 * pi * n_norm).cos()
                        - 0.012604 * (6.0 * pi * n_norm).cos()
                }
                4 => {
                    let alpha = 9.0;
                    let arg = (2.0 * n_norm - 1.0).powi(2);
                    if arg <= 1.0 {
                        Self::bessel_i0(pi * alpha * (1.0 - arg).sqrt())
                            / Self::bessel_i0(pi * alpha)
                    } else {
                        0.0
                    }
                }
                _ => 1.0,
            };
            h[i] = (sinc * window) as f32;
        }
        h
    }

    /// Pack one Complex<f64> as a DS pair (re_hi, re_lo, im_hi, im_lo).
    /// Layout matches the GLSL vec4<f32> on the GPU side exactly.
    #[inline]
    fn pack_ds(c: Complex<f64>, out: &mut [f32]) {
        let re_hi = c.re as f32;
        let re_lo = (c.re - re_hi as f64) as f32;
        let im_hi = c.im as f32;
        let im_lo = (c.im - im_hi as f64) as f32;
        out[0] = re_hi;
        out[1] = re_lo;
        out[2] = im_hi;
        out[3] = im_lo;
    }

    /// Pre-compute partitioned H[ω] in f64 and pack as DS.
    /// Each partition's FFT runs in Complex<f64> through rustfft, so the
    /// spectrum that reaches the GPU never falls through f32 precision.
    #[allow(dead_code)] // f32 variant only used by the legacy generate_fir path
    pub(crate) fn compute_h_blocks_cpu_ds(
        h_time: &[f32],
        b_size: usize,
        n: usize,
        num_blocks: usize,
    ) -> Option<Vec<f32>> {
        use rayon::prelude::*;
        let results: Vec<Option<Vec<f32>>> = (0..num_blocks)
            .into_par_iter()
            .map(|b| {
                if crate::audio::cancel_flag::check() {
                    return None;
                }
                let mut planner = FftPlanner::<f64>::new();
                let fft = planner.plan_fft_forward(n);
                let mut block = vec![Complex::<f64>::new(0.0, 0.0); n];
                let offset = b * b_size;
                for i in 0..b_size {
                    if offset + i < h_time.len() {
                        block[i] = Complex::new(h_time[offset + i] as f64, 0.0);
                    }
                }
                fft.process(&mut block);
                let mut flat = vec![0.0f32; n * 4];
                for i in 0..n {
                    Self::pack_ds(block[i], &mut flat[i * 4..i * 4 + 4]);
                }
                Some(flat)
            })
            .collect();
        if results.iter().any(|r| r.is_none()) {
            return None;
        }
        let mut out = Vec::with_capacity(num_blocks * n * 4);
        for block in results.into_iter().flatten() {
            out.extend_from_slice(&block);
        }
        Some(out)
    }

    /// Same as compute_h_blocks_cpu_ds but starting from f64 coefficients.
    /// Used by `new_with_coefficients` so the user's 128-bit-generated FIR
    /// reaches the GPU without any f64→f32 round-trip on the way in.
    pub(crate) fn compute_h_blocks_cpu_ds_f64(
        h_time: &[f64],
        b_size: usize,
        n: usize,
        num_blocks: usize,
    ) -> Option<Vec<f32>> {
        use rayon::prelude::*;
        let results: Vec<Option<Vec<f32>>> = (0..num_blocks)
            .into_par_iter()
            .map(|b| {
                if crate::audio::cancel_flag::check() {
                    return None;
                }
                let mut planner = FftPlanner::<f64>::new();
                let fft = planner.plan_fft_forward(n);
                let mut block = vec![Complex::<f64>::new(0.0, 0.0); n];
                let offset = b * b_size;
                for i in 0..b_size {
                    if offset + i < h_time.len() {
                        block[i] = Complex::new(h_time[offset + i], 0.0);
                    }
                }
                fft.process(&mut block);
                let mut flat = vec![0.0f32; n * 4];
                for i in 0..n {
                    Self::pack_ds(block[i], &mut flat[i * 4..i * 4 + 4]);
                }
                Some(flat)
            })
            .collect();
        if results.iter().any(|r| r.is_none()) {
            return None;
        }
        let mut out = Vec::with_capacity(num_blocks * n * 4);
        for block in results.into_iter().flatten() {
            out.extend_from_slice(&block);
        }
        Some(out)
    }
}
