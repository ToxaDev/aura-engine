use super::processor::GpuDspProcessor;
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex;

impl GpuDspProcessor {
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

    /// Pre-compute the partitioned filter spectrum H[ω] in f64 and pack it
    /// as DS: `num_blocks` half spectra (bins 0 ..= n/2 of each partition's
    /// n-point transform, zero-padded to `half_stride(n)` slots).
    ///
    /// Each partition goes through realfft in f64 — the same r2c the CPU
    /// convolver uses — so the spectrum reaches the GPU rounded to DS once
    /// and never falls through f32 on the way. The f64 coefficients come
    /// straight from the .npy, the user's 128-bit-generated FIR without an
    /// f64→f32 round-trip.
    pub(crate) fn compute_h_blocks_cpu_ds_f64(
        h_time: &[f64],
        b_size: usize,
        n: usize,
        num_blocks: usize,
    ) -> Option<Vec<f32>> {
        use rayon::prelude::*;
        let stride = Self::half_stride(n);
        // One plan, shared by every partition: rustfft's tables are not
        // small at N = 4M, and building one per partition used to cost
        // hundreds of MB at 30M taps.
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
        let mut out = vec![0.0f32; num_blocks * stride * 4];
        let done = out
            .par_chunks_mut(stride * 4)
            .enumerate()
            .map(|(b, dst)| {
                if crate::audio::cancel_flag::check() {
                    return false;
                }
                let mut x = vec![0.0f64; n];
                let offset = b * b_size;
                let end = (offset + b_size).min(h_time.len());
                if offset < end {
                    x[..end - offset].copy_from_slice(&h_time[offset..end]);
                }
                let mut spectrum = vec![Complex::new(0.0, 0.0); n / 2 + 1];
                crate::audio::dsp_core::r2c(&fft, &mut x, &mut spectrum);
                for (i, c) in spectrum.iter().enumerate() {
                    Self::pack_ds(*c, &mut dst[i * 4..i * 4 + 4]);
                }
                true
            })
            .collect::<Vec<bool>>();
        done.iter().all(|&ok| ok).then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::FftPlanner;

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
            })
            .collect()
    }

    /// The arithmetic of gpu_ola.comp.glsl, in f64: two real channels
    /// through one complex transform each way. The split must give both
    /// channels' half spectra (times two), and the joined spectrum's
    /// inverse must give both filtered channels (times 2N) — the same as
    /// filtering each channel on its own with r2c/c2r. With the imaginary
    /// parts of bins 0 and N/2 dropped, as the kernel drops them.
    #[test]
    fn two_real_channels_go_through_one_complex_transform() {
        let n = 1024usize;
        let half = n / 2;
        let l = noise(n, 0x1234_5678_9abc_def1);
        let r: Vec<f64> = noise(n, 0x0fed_cba9_8765_4321).iter().map(|v| v * 1e-3).collect();
        let taps = noise(n / 2, 0x5555_aaaa_3333_cccc);

        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let mut real = RealFftPlanner::<f64>::new();
        let r2c = real.plan_fft_forward(n);
        let c2r = real.plan_fft_inverse(n);

        let half_of = |x: &[f64]| {
            let mut t = x.to_vec();
            t.resize(n, 0.0);
            let mut s = vec![Complex::new(0.0, 0.0); half + 1];
            crate::audio::dsp_core::r2c(&r2c, &mut t, &mut s);
            s
        };
        let (xl, xr, h) = (half_of(&l), half_of(&r), half_of(&taps));

        // Forward: Z = FFT(L + iR), then the split.
        let mut z: Vec<Complex<f64>> = (0..n).map(|i| Complex::new(l[i], r[i])).collect();
        fwd.process(&mut z);
        let mut two_xl = Vec::with_capacity(half + 1);
        let mut two_xr = Vec::with_capacity(half + 1);
        for k in 0..=half {
            let (a, b) = (z[k], z[(n - k) % n]);
            two_xl.push(Complex::new(a.re + b.re, a.im - b.im));
            two_xr.push(Complex::new(a.im + b.im, b.re - a.re));
        }
        let peak_l = xl.iter().fold(0.0f64, |m, c| m.max(c.norm()));
        for k in 0..=half {
            assert!((two_xl[k] - xl[k] * 2.0).norm() < 1e-12 * peak_l, "L bin {}", k);
            assert!((two_xr[k] - xr[k] * 2.0).norm() < 1e-12 * peak_l, "R bin {}", k);
        }

        // Multiply-accumulate (one partition), join, one inverse.
        let mut al: Vec<Complex<f64>> = (0..=half).map(|k| two_xl[k] * h[k]).collect();
        let mut ar: Vec<Complex<f64>> = (0..=half).map(|k| two_xr[k] * h[k]).collect();
        for k in [0, half] {
            al[k].im = 0.0;
            ar[k].im = 0.0;
        }
        let mut w = vec![Complex::new(0.0, 0.0); n];
        for i in 0..=half {
            w[i] = Complex::new(al[i].re - ar[i].im, al[i].im + ar[i].re);
            if i != 0 && i != half {
                w[n - i] = Complex::new(al[i].re + ar[i].im, ar[i].re - al[i].im);
            }
        }
        inv.process(&mut w);

        // Reference: each channel filtered on its own.
        let filtered = |x: &[Complex<f64>]| {
            let mut s: Vec<Complex<f64>> = (0..=half).map(|k| x[k] * h[k]).collect();
            let mut t = vec![0.0f64; n];
            crate::audio::dsp_core::c2r(&c2r, &mut s, &mut t);
            t
        };
        let (yl, yr) = (filtered(&xl), filtered(&xr));
        let peak = yl.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for i in 0..n {
            // c2r is unscaled (×N); the joined inverse carries 2N.
            assert!((w[i].re / 2.0 - yl[i]).abs() < 1e-12 * peak, "L sample {}", i);
            assert!((w[i].im / 2.0 - yr[i]).abs() < 1e-12 * peak, "R sample {}", i);
        }
    }
}
