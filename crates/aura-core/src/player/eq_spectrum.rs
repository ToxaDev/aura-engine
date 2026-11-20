//! The EQ window's analyzer: the spectrum before the EQ (the render's
//! timeline) and after it (`output::post_tap`) at one frame, on the graph's
//! own frequency axis — a log grid 10 Hz … 30 kHz — in dB, as f32.
//!
//! Reply (`/player/eq_spectrum`), little-endian: u8 version (1), u8 flags
//! (bit 0: `post` is the tap's, else a copy of `pre` — no tap there, as in
//! Direct mode where nothing comes between), u16 n, u32 rate, u32 FFT size,
//! then n f32 `pre`, n f32 `post`. A level is the peak amplitude of a sine
//! in that band (a full-scale sine reads 0 dB), `FLOOR_DB` for none.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rustfft::{num_complex::Complex, Fft, FftPlanner};

pub const F_MIN: f64 = 10.0;
pub const F_MAX: f64 = 30_000.0;
pub const FLOOR_DB: f32 = -200.0;
pub const MAX_POINTS: usize = 1024;
const HEADER: usize = 12;

/// The analysis window for resolution `res` (0 Low … 3 Max): 1024, 2048,
/// 4096, 8192 frames at 44.1k, the same length of time at any rate (the
/// nearest power of two).
pub fn fft_size(rate: u32, res: u8) -> usize {
    let base = 1024.0 * (1u32 << res.min(3)) as f64;
    let x = base * rate.max(1) as f64 / 44_100.0;
    1usize << (x.log2().round().max(8.0) as u32)
}

/// The grid's band centres: `n` log-spaced from F_MIN to F_MAX.
pub fn grid(n: usize) -> Vec<f64> {
    let span = (F_MAX / F_MIN).ln();
    (0..n).map(|i| F_MIN * (span * i as f64 / (n.max(2) - 1) as f64).exp()).collect()
}

fn plan(n: usize) -> Arc<dyn Fft<f64>> {
    static PLANS: OnceLock<Mutex<HashMap<usize, Arc<dyn Fft<f64>>>>> = OnceLock::new();
    PLANS.get_or_init(Default::default).lock().unwrap()
        .entry(n)
        .or_insert_with(|| FftPlanner::<f64>::new().plan_fft_forward(n))
        .clone()
}

/// The Hann-windowed power of `x`'s bins 0 ..= N/2, scaled so a sine's
/// peak bin reads its amplitude squared.
pub fn power(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let w = |i: usize| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
    let sum_w: f64 = (0..n).map(w).sum();
    let mut buf: Vec<Complex<f64>> = x.iter().enumerate().map(|(i, &v)| Complex { re: v * w(i), im: 0.0 }).collect();
    let p = plan(n);
    let mut scratch = vec![Complex::default(); p.get_inplace_scratch_len()];
    p.process_with_scratch(&mut buf, &mut scratch);
    let k = 2.0 / sum_w;
    buf[..=n / 2].iter().map(|c| c.norm_sqr() * k * k).collect()
}

/// The bins' power (`power`, of an `fft`-frame window at `rate`) on the
/// grid `centres`, in dB: a band as wide as a bin or more reads its
/// loudest bin; a narrower one (the low end) the power interpolated at its
/// centre, so the grid has no steps where several bands share a bin.
pub fn bands_db(p: &[f64], rate: u32, fft: usize, centres: &[f64], out: &mut [f32]) {
    let per_hz = fft as f64 / rate as f64;
    let top = (p.len() - 1) as f64;
    let n = centres.len();
    let edge = |i: usize, j: usize| (centres[i] * centres[j]).sqrt() * per_hz;
    for (i, o) in out.iter_mut().enumerate().take(n) {
        let c = centres[i] * per_hz;
        let lo = if i > 0 { edge(i - 1, i) } else { c * c / edge(i, (i + 1).min(n - 1)).max(1e-9) };
        let hi = if i + 1 < n { edge(i, i + 1) } else { c * c / edge(i - 1, i) };
        let v = if c >= top {
            0.0
        } else if hi - lo < 1.0 {
            let (b0, t) = (c.floor() as usize, c.fract());
            p[b0] * (1.0 - t) + p[(b0 + 1).min(p.len() - 1)] * t
        } else {
            let (a, b) = (lo.round().max(1.0) as usize, (hi.round() as usize).min(p.len() - 1));
            p[a..=b.max(a)].iter().fold(0.0f64, |m, &x| m.max(x))
        };
        *o = if v > 1e-20 { (10.0 * v.log10()) as f32 } else { FLOOR_DB };
    }
}

/// The reply for `pre` (and `post`, None: the tap had none) windows of
/// `rate` frames, mono, on an `n`-point grid.
pub fn reply(pre: &[f64], post: Option<&[f64]>, rate: u32, n: usize) -> Vec<u8> {
    let fft = pre.len();
    let centres = grid(n);
    let mut a = vec![FLOOR_DB; n];
    bands_db(&power(pre), rate, fft, &centres, &mut a);
    let b = match post {
        Some(x) => {
            let mut b = vec![FLOOR_DB; n];
            bands_db(&power(x), rate, fft, &centres, &mut b);
            b
        }
        None => a.clone(),
    };
    let mut out = Vec::with_capacity(HEADER + 8 * n);
    out.extend_from_slice(&[1, post.is_some() as u8]);
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(fft as u32).to_le_bytes());
    for v in a.iter().chain(&b) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::dsp::eq::{EqBand, EqDesign, EqRun, EqState};

    fn sine(f: f64, amp: f64, rate: u32, n: usize) -> Vec<f64> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin()).collect()
    }

    fn f32s(b: &[u8]) -> Vec<f32> {
        b.chunks(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    /// The grid point nearest `f`.
    fn at(n: usize, f: f64) -> usize {
        let g = grid(n);
        (0..n).min_by(|&a, &b| (g[a] / f).ln().abs().total_cmp(&(g[b] / f).ln().abs())).unwrap()
    }

    #[test]
    fn the_window_lasts_as_long_at_every_rate() {
        assert_eq!([0, 1, 2, 3].map(|r| fft_size(44_100, r)), [1024, 2048, 4096, 8192]);
        assert_eq!(fft_size(48_000, 1), 2048);
        assert_eq!(fft_size(96_000, 1), 4096);
        assert_eq!(fft_size(768_000, 3), 131_072);
        assert_eq!(fft_size(705_600, 0), 16_384);
        let g = grid(256);
        assert!((g[0] - F_MIN).abs() < 1e-9 && (g[255] - F_MAX).abs() < 1e-6);
    }

    #[test]
    fn a_full_scale_sine_reads_0_db_at_its_band() {
        for (rate, res) in [(44_100, 1), (48_000, 0), (96_000, 3), (192_000, 2)] {
            let fft = fft_size(rate, res);
            for f in [40.0, 100.0, 1000.0, 12_345.0] {
                let r = reply(&sine(f, 1.0, rate, fft), None, rate, 400);
                let v = f32s(&r[HEADER..]);
                // The grid's peak near the tone (its points lie beside it).
                let i = at(400, f);
                let db = v[i - 3..=i + 3].iter().fold(f32::MIN, |m, &x| m.max(x));
                // Hann's scalloping: a tone between bins reads up to 1.42 dB low.
                assert!(db < 0.05 && db > -1.6, "{rate} {f}: {db}");
                // Far from it (20 bins and more): the window's skirt.
                if 3.0 * f * fft as f64 / rate as f64 >= 20.0 {
                    assert!(v[at(400, f * 4.0)] < -60.0, "{rate} {f}");
                }
            }
        }
    }

    #[test]
    fn narrow_low_bands_have_no_steps() {
        let (rate, fft) = (48_000, 2048);
        let x: Vec<f64> = (0..fft).map(|i| ((i * 7919) % 1000) as f64 / 1000.0 - 0.5).collect();
        let r = reply(&x, None, rate, 512);
        let v = f32s(&r[HEADER..HEADER + 4 * 512]);
        // Under 100 Hz many bands share a bin (23 Hz): interpolated, the
        // curve has no flat runs of equal values.
        let lo: Vec<f32> = v[..at(512, 100.0)].to_vec();
        assert!(lo.windows(3).filter(|w| w[0] == w[1] && w[1] == w[2]).count() == 0);
    }

    #[test]
    fn the_reply_is_pre_and_post_after_its_header() {
        let x = sine(1000.0, 0.5, 48_000, 2048);
        let r = reply(&x, Some(&x), 48_000, 300);
        assert_eq!(r.len(), HEADER + 8 * 300);
        assert_eq!(&r[..4], &[1, 1, 44, 1]);
        assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(r[8..12].try_into().unwrap()), 2048);
        assert_eq!(reply(&x, None, 48_000, 300)[1], 0);
        let v = f32s(&r[HEADER..]);
        assert_eq!(v[..300], v[300..]);
    }

    /// Through a known bell the post spectrum stands above the pre by the
    /// bell's own response, tone by tone.
    #[test]
    fn post_is_pre_times_the_eq_response() {
        let rate = 48_000;
        let st = EqState { on: true, bands: vec![EqBand { f: 1000.0, gain: 9.0, q: 1.0, ..Default::default() }], preamp: Some(0.0), ..Default::default() };
        let d = EqDesign::new(&st, rate);
        let fft = fft_size(rate, 2);
        for f in [100.0, 400.0, 700.0, 1000.0, 1500.0, 3000.0, 10_000.0] {
            // Settled: the filter's start trimmed off.
            let x = sine(f, 0.25, rate, fft + rate as usize / 4);
            let (mut l, mut r) = (x.clone(), x.clone());
            EqRun::new().process(&d, &mut l, &mut r);
            let pre = &x[x.len() - fft..];
            let post = &l[l.len() - fft..];
            let out = reply(pre, Some(post), rate, 400);
            let v = f32s(&out[HEADER..]);
            let i = at(400, f);
            let got = v[400 + i] - v[i];
            let want = d.response_db(f) as f32;
            assert!((got - want).abs() < 0.5, "{f}: {got} vs {want}");
        }
    }
}
