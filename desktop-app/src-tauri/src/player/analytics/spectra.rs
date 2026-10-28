//! STFT engine, Welch long-term PSD, band-level helpers and effective-bandwidth
//! detector.
//!
//! **Normalization — dBFS-sine**: a full-scale sinusoid (peak amplitude 1.0)
//! reads 0 dBFS at its bin, for any FFT size and sample rate. The coherent
//! gain of the window `CG = mean(w)` compensates for the window's amplitude
//! loss: each bin magnitude is divided by `N · CG / 2`.
//!
//! Derivation: for `x[n] = A·cos(2π·k·n/N)` the windowed DFT bin at `k` is
//! `X[k] ≈ A · N · CG / 2` (the `W[2k]` aliasing term is negligible for a
//! steep window). Dividing by `N · CG / 2` gives `A`. For A = 1 → 0 dBFS. ✓

use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::sync::Arc;

// ─── Bessel I0 / Kaiser window ───────────────────────────────────────────────

/// Modified Bessel function of the first kind, order 0.
///
/// Series: `I₀(x) = Σₖ (x/2)^{2k} / (k!)²`
/// Ref: Abramowitz & Stegun §9.8.1; converges absolutely for all finite x.
pub fn bessel_i0(x: f64) -> f64 {
    let x2 = (x * 0.5) * (x * 0.5);
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    for k in 1u32..=60 {
        term *= x2 / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Kaiser window of length `n` with shape parameter `beta`.
///
/// `beta = 28.0` (the module default) gives theoretical sidelobes ≈ −263 dB
/// at f64 precision.
/// Ref: Harris, "On the Use of Windows for Harmonic Analysis with the DFT",
/// Proc. IEEE, 1978, Table I.
pub fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    let i0_beta = bessel_i0(beta);
    let n1 = (n - 1) as f64;
    (0..n)
        .map(|i| {
            let t = 2.0 * i as f64 / n1 - 1.0; // t ∈ [−1, 1]
            bessel_i0(beta * (1.0 - t * t).max(0.0).sqrt()) / i0_beta
        })
        .collect()
}

// ─── STFT ────────────────────────────────────────────────────────────────────

/// Short-time Fourier transform engine with per-frame magnitude output in
/// dBFS-sine normalization (see module doc).
pub struct Stft {
    pub n: usize,
    pub hop: usize,
    window: Vec<f64>,
    /// `2 / (n · CG)` — reciprocal of the coherent-gain normalization factor.
    norm: f64,
    fft: Arc<dyn Fft<f64>>,
}

impl Stft {
    /// Create an engine with a Kaiser window (beta = 28.0 by default, giving
    /// sidelobes ≈ −263 dB).
    pub fn new(n: usize, hop: usize, beta: f64) -> Stft {
        let window = kaiser(n, beta);
        let cg: f64 = window.iter().sum::<f64>() / n as f64; // coherent gain
        let norm = 2.0 / (n as f64 * cg);
        let fft = FftPlanner::<f64>::new().plan_fft_forward(n);
        Stft { n, hop, window, norm, fft }
    }

    /// Analyze one frame of exactly `n` samples. Returns `n/2 + 1` magnitude
    /// bins in dBFS-sine. The caller owns the frame; no internal buffering.
    #[cfg(test)]
    pub fn frame_mag(&self, x: &[f64]) -> Vec<f64> {
        debug_assert_eq!(x.len(), self.n);
        let mut buf: Vec<Complex<f64>> =
            x.iter().zip(&self.window).map(|(&s, &w)| Complex::new(s * w, 0.0)).collect();
        self.fft.process(&mut buf);
        let half = self.n / 2 + 1;
        buf[..half]
            .iter()
            .map(|c| {
                let mag = c.norm() * self.norm;
                20.0 * mag.max(1e-120).log10()
            })
            .collect()
    }

    /// Returns the full windowed complex spectrum (length n, positive and
    /// negative frequencies). Used internally for Welch cross-spectrum.
    fn frame_complex(&self, x: &[f64]) -> Vec<Complex<f64>> {
        debug_assert_eq!(x.len(), self.n);
        let mut buf: Vec<Complex<f64>> =
            x.iter().zip(&self.window).map(|(&s, &w)| Complex::new(s * w, 0.0)).collect();
        self.fft.process(&mut buf);
        buf
    }

    /// The mean of the L and R power spectra of one frame (`n/2 + 1` bins,
    /// linear power in dBFS-sine: a full-scale sine in both channels reads 1).
    /// One complex FFT carries both channels (L real, R imaginary):
    /// `|X_L[k]|² + |X_R[k]|² = (|Z[k]|² + |Z[n−k]|²) / 2`.
    pub fn frame_power_lr(&self, l: &[f64], r: &[f64], out: &mut Vec<f64>) {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        self.frame_power_lr_with(l, r, &mut buf, &mut scratch, out);
    }

    /// `frame_power_lr` with the caller's buffers (no allocation per frame).
    pub fn frame_power_lr_with(
        &self,
        l: &[f64],
        r: &[f64],
        buf: &mut Vec<Complex<f64>>,
        scratch: &mut Vec<Complex<f64>>,
        out: &mut Vec<f64>,
    ) {
        debug_assert!(l.len() == self.n && r.len() == self.n);
        buf.clear();
        buf.extend(l.iter().zip(r).zip(&self.window).map(|((&a, &b), &w)| Complex::new(a * w, b * w)));
        let need = self.fft.get_inplace_scratch_len();
        if scratch.len() < need {
            scratch.resize(need, Complex::new(0.0, 0.0));
        }
        self.fft.process_with_scratch(buf, &mut scratch[..need]);
        let half = self.n / 2 + 1;
        let k2 = self.norm * self.norm / 4.0;
        out.clear();
        out.push(2.0 * buf[0].norm_sqr() * k2);
        out.extend((1..half).map(|k| (buf[k].norm_sqr() + buf[self.n - k].norm_sqr()) * k2));
    }
}

// ─── Spectrogram rows ────────────────────────────────────────────────────────

/// The spectrogram's rows: `SPEC_LOG_BINS` log-spaced bands from
/// `SPEC_F0_HZ` to Nyquist, the same for the AAST tiles, the live O columns
/// and a stream's columns. Below `SPEC_SUB_BLEND` they come from the sub
/// band (a decimated signal and a ~3 s window: see `SubFir`).
pub const SPEC_LOG_BINS: usize = 512;
pub const SPEC_F0_HZ: f64 = 5.0;
/// Kaiser β for the spectrogram: sidelobes ≈ −122 dB, just under the
/// 120 dB display range, with a main lobe of ±5 bins (β = 28 is ±9).
pub const SPEC_BETA: f64 = 16.0;

/// The spectrogram's FFT length: 4096 at 44.1/48 kHz, doubled with each
/// doubling of the rate, so a bin stays ≈ 11 Hz wide.
pub fn spec_fft_len(rate: u32) -> usize {
    4096 * (rate as usize).div_ceil(48_000).next_power_of_two()
}

/// Folds an FFT power spectrum into log-spaced bands. A band wider than an
/// FFT bin takes the mean power of the bins inside it; a narrower one (the
/// lows) interpolates between the two nearest bins at its centre.
pub struct LogBins {
    /// Per band: `(lo, hi, pos)` — the mean of `power[lo..hi]`, or with
    /// `lo == hi` the interpolation at fractional bin `pos`.
    bands: Vec<(usize, usize, f64)>,
}

impl LogBins {
    pub fn new(n_fft: usize, rate: u32, n_bands: usize, f0: f64) -> LogBins {
        let nyq = rate as f64 / 2.0;
        LogBins::with_step(n_fft, rate, n_bands, f0, (nyq / f0).ln() / n_bands as f64)
    }

    /// Band k spans `f0·e^(k·ln_step) .. f0·e^((k+1)·ln_step)`: the output's
    /// bands continue the source's at the same step above its Nyquist.
    pub fn with_step(n_fft: usize, rate: u32, n_bands: usize, f0: f64, ln_step: f64) -> LogBins {
        LogBins::with_bin(n_fft, rate as f64 / n_fft as f64, n_bands, f0, ln_step)
    }

    /// The same for an FFT whose bins are `df` Hz apart (a decimated signal).
    pub fn with_bin(n_fft: usize, df: f64, n_bands: usize, f0: f64, ln_step: f64) -> LogBins {
        let half = n_fft / 2 + 1;
        let edge = |k: usize| f0 * (k as f64 * ln_step).exp() / df;
        let bands = (0..n_bands).map(|k| {
            let (j0, j1) = (edge(k), edge(k + 1));
            let lo = (j0.ceil() as usize).min(half - 1);
            let hi = (j1.ceil() as usize).min(half);
            if hi > lo { (lo, hi, 0.0) } else { (lo, lo, (j0 * j1).sqrt()) }
        }).collect();
        LogBins { bands }
    }

    /// The power of bands `b0..b1` into `out[..b1 - b0]`.
    pub fn band_power(&self, power: &[f64], b0: usize, b1: usize, out: &mut [f64]) {
        let last = power.len() - 1;
        for (o, &(lo, hi, pos)) in out.iter_mut().zip(&self.bands[b0..b1]) {
            *o = if hi > lo {
                power[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
            } else {
                let i = (pos.floor() as usize).min(last);
                let t = pos - i as f64;
                power[i] * (1.0 - t) + power[(i + 1).min(last)] * t
            };
        }
    }

    /// Band levels as bytes: `0` = `floor_db`, `255` = `floor_db + range_db`.
    #[cfg(test)]
    pub fn to_u8(&self, power: &[f64], floor_db: f64, range_db: f64, out: &mut [u8]) {
        let mut p = vec![0.0; self.bands.len()];
        self.band_power(power, 0, self.bands.len(), &mut p);
        for (o, &v) in out.iter_mut().zip(&p) {
            *o = power_to_u8(v, floor_db, range_db);
        }
    }
}

#[inline]
pub fn power_to_u8(p: f64, floor_db: f64, range_db: f64) -> u8 {
    let db = 10.0 * p.max(1e-30).log10();
    ((db - floor_db) / range_db * 255.0).clamp(0.0, 255.0).round() as u8
}

// ─── Sub band: the lows from a decimated signal ──────────────────────────────

/// The sub band's FFT length (on the decimated signal): 2048 points at
/// 689 Hz (44.1 kHz family) or 750 Hz (48 kHz family) — bins 0.34 / 0.37 Hz,
/// a window of 2.97 / 2.73 s, main lobe (Kaiser 16) ±1.75 Hz.
pub const SPEC_SUB_FFT: usize = 2048;
/// Where the sub band hands over to the full-rate FFTs (Hz; raised cosine
/// in log frequency): the sub band alone below the first, none above the
/// second. The long FFT's main lobe is ±14 Hz: from 28 Hz up it no longer
/// carries a tone under 14 Hz into the blend (a hump at 20–27 Hz).
pub const SPEC_SUB_BLEND: (f64, f64) = (28.0, 45.0);
/// The decimating filter is flat up to here (above the blend) and stops at
/// the decimated rate minus it, so nothing folds into 0..SUB_PASS_HZ.
const SUB_PASS_HZ: f64 = 50.0;
const SUB_STOP_DB: f64 = 130.0;

/// The decimation factor of the sub band: 64 at 44.1/48 kHz, doubled with
/// each doubling of the rate (as `spec_fft_len`).
pub fn spec_sub_factor(rate: u32) -> usize {
    spec_fft_len(rate) / 64
}

/// The sub band's anti-alias low-pass and decimation: a Kaiser-windowed
/// sinc (≈ 130 dB stop band, cut-off at half the decimated rate), evaluated
/// only at the samples kept. Decimated sample j is
/// `y[j] = Σ h[i]·x[j·d − k + i]`, `i = 0..=2k`, with x zero outside the
/// signal: centred on input sample j·d (no delay), and the same whether the
/// signal is decimated whole (`decimate`) or as it comes (`SubDecimator`).
pub struct SubFir {
    pub d: usize,
    /// Half the filter's length.
    pub k: usize,
    h: Vec<f64>,
}

impl SubFir {
    pub fn new(rate: u32) -> SubFir {
        let d = spec_sub_factor(rate);
        let fs = rate as f64;
        // Kaiser's design rule (Oppenheim & Schafer §7.6).
        let transition = 2.0 * std::f64::consts::PI * (fs / d as f64 - 2.0 * SUB_PASS_HZ) / fs;
        let k = ((SUB_STOP_DB - 7.95) / (2.285 * transition) / 2.0).ceil() as usize;
        let beta = 0.1102 * (SUB_STOP_DB - 8.7);
        let w = kaiser(2 * k + 1, beta);
        let fc = 0.5 / d as f64; // cycles per sample
        let mut h: Vec<f64> = (0..=2 * k).map(|i| {
            let t = i as f64 - k as f64;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
            };
            sinc * w[i]
        }).collect();
        let sum: f64 = h.iter().sum();
        for v in &mut h { *v /= sum; }
        SubFir { d, k, h }
    }

    /// The decimated rate (Hz) of a signal at `rate`.
    pub fn rate_out(&self, rate: u32) -> f64 {
        rate as f64 / self.d as f64
    }

    /// Decimated samples `0..=(n − 1 + k) / d` of a whole signal of `n`.
    pub fn decimate<T: Copy + Into<f64>>(&self, x: &[T]) -> Vec<f64> {
        let n = x.len();
        if n == 0 {
            return Vec::new();
        }
        let (d, k) = (self.d as isize, self.k as isize);
        (0..=(n + self.k - 1) / self.d).map(|j| {
            let c = j as isize * d;
            let lo = (c - k).max(0);
            let hi = (c + k).min(n as isize - 1);
            let mut acc = 0.0;
            for i in lo..=hi {
                acc += self.h[(i - c + k) as usize] * x[i as usize].into();
            }
            acc
        }).collect()
    }
}

/// A signal decimated whole for the sub band (L and R on two threads).
pub struct SubSignal {
    pub l: Vec<f64>,
    pub r: Vec<f64>,
}

impl SubSignal {
    pub fn of<T: Copy + Into<f64> + Sync>(l: &[T], r: &[T], rate: u32) -> SubSignal {
        let fir = SubFir::new(rate);
        let (dl, dr) = std::thread::scope(|sc| {
            let h = sc.spawn(|| fir.decimate(r));
            (fir.decimate(l), h.join().unwrap_or_default())
        });
        SubSignal { l: dl, r: dr }
    }

    pub fn source(&self) -> SliceSource<'_, f64> {
        SliceSource { l: &self.l, r: &self.r }
    }
}

/// The sub band's decimation as samples come (no allocation after `new`):
/// the last 2k+1 input samples in a ring, a decimated sample each `d`.
pub struct SubDecimator {
    fir: SubFir,
    ring_l: Vec<f64>,
    ring_r: Vec<f64>,
    mask: usize,
    /// Input samples pushed, and the next decimated sample's index.
    t: u64,
    next_j: u64,
}

impl SubDecimator {
    pub fn new(rate: u32) -> SubDecimator {
        let fir = SubFir::new(rate);
        let size = (2 * fir.k + 1).next_power_of_two();
        SubDecimator { fir, ring_l: vec![0.0; size], ring_r: vec![0.0; size], mask: size - 1, t: 0, next_j: 0 }
    }

    pub fn fir(&self) -> &SubFir {
        &self.fir
    }

    /// Back to the start of a signal (nothing pushed).
    pub fn reset(&mut self) {
        self.ring_l.fill(0.0);
        self.ring_r.fill(0.0);
        self.t = 0;
        self.next_j = 0;
    }

    /// Decimated samples made so far.
    #[cfg(test)]
    pub fn made(&self) -> u64 {
        self.next_j
    }

    /// Push input; `out` sees each decimated (L, R) once it is complete
    /// (its last input sample, j·d + k, is in).
    pub fn push(&mut self, l: &[f64], r: &[f64], mut out: impl FnMut(f64, f64)) {
        let (d, k) = (self.fir.d as u64, self.fir.k as u64);
        for (&a, &b) in l.iter().zip(r) {
            let p = self.t as usize & self.mask;
            self.ring_l[p] = a;
            self.ring_r[p] = b;
            self.t += 1;
            if self.t == self.next_j * d + k + 1 {
                let start = (self.next_j * d).wrapping_sub(k) as usize & self.mask;
                out(self.dot(&self.ring_l, start), self.dot(&self.ring_r, start));
                self.next_j += 1;
            }
        }
    }

    /// The end of the signal: the decimated samples its last input still
    /// reaches (as `SubFir::decimate`).
    pub fn flush(&mut self, mut out: impl FnMut(f64, f64)) {
        let zeros = [0.0f64; 64];
        let mut left = 2 * self.fir.k;
        while left > 0 {
            let m = left.min(zeros.len());
            self.push(&zeros[..m], &zeros[..m], &mut out);
            left -= m;
        }
    }

    /// `Σ h[i]·ring[start + i]` in input order (the ring wraps once at most).
    fn dot(&self, ring: &[f64], start: usize) -> f64 {
        let h = &self.fir.h;
        let first = (ring.len() - start).min(h.len());
        let mut acc = 0.0;
        for (a, b) in h[..first].iter().zip(&ring[start..start + first]) {
            acc += a * b;
        }
        for (a, b) in h[first..].iter().zip(&ring[..h.len() - first]) {
            acc += a * b;
        }
        acc
    }
}

/// The weight of the sub band at `f` Hz (1 below the blend, 0 above).
pub fn sub_weight(f: f64) -> f64 {
    1.0 - blend(f, SPEC_SUB_BLEND)
}

/// The sub band of columns made as samples come (the live O columns, a
/// stream's): each column's lows from the latest `SPEC_SUB_FFT` decimated
/// samples — a window ending where the column's frame ends. Nothing is
/// allocated after `new`.
pub struct SubTrail {
    dec: SubDecimator,
    /// The latest SPEC_SUB_FFT decimated samples, oldest at `pos`.
    ring_l: Vec<f64>,
    ring_r: Vec<f64>,
    pos: usize,
    stft: Stft,
    bins: LogBins,
    /// Bands `0..weight.len()` take the sub band at `weight[b]`.
    weight: Vec<f64>,
    fl: Vec<f64>,
    fr: Vec<f64>,
    buf: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    power: Vec<f64>,
    bands: Vec<f64>,
}

impl SubTrail {
    /// For columns of a signal at `rate` in bands from SPEC_F0_HZ at `ln_step`.
    pub fn new(rate: u32, n_bands: usize, ln_step: f64) -> SubTrail {
        let dec = SubDecimator::new(rate);
        let n = SPEC_SUB_FFT;
        let df = dec.fir().rate_out(rate) / n as f64;
        let centre = |k: usize| SPEC_F0_HZ * ((k as f64 + 0.5) * ln_step).exp();
        let weight: Vec<f64> = (0..n_bands).map(|k| sub_weight(centre(k))).take_while(|&w| w > 1e-12).collect();
        let nb = weight.len();
        let stft = Stft::new(n, n, SPEC_BETA);
        let need = stft.fft.get_inplace_scratch_len();
        SubTrail {
            dec,
            ring_l: vec![0.0; n],
            ring_r: vec![0.0; n],
            pos: 0,
            stft,
            bins: LogBins::with_bin(n, df, nb, SPEC_F0_HZ, ln_step),
            weight,
            fl: vec![0.0; n],
            fr: vec![0.0; n],
            buf: Vec::with_capacity(n),
            scratch: vec![Complex::new(0.0, 0.0); need],
            power: Vec::with_capacity(n / 2 + 1),
            bands: vec![0.0; nb],
        }
    }

    /// Back to silence (a new signal, or a jump in it).
    pub fn reset(&mut self) {
        self.dec.reset();
        self.ring_l.fill(0.0);
        self.ring_r.fill(0.0);
        self.pos = 0;
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        let (rl, rr, pos) = (&mut self.ring_l, &mut self.ring_r, &mut self.pos);
        let n = rl.len();
        self.dec.push(l, r, |a, b| {
            rl[*pos] = a;
            rr[*pos] = b;
            *pos = (*pos + 1) % n;
        });
    }

    /// A column's band powers (bands from SPEC_F0_HZ, from its own frame):
    /// the lows blended in from the sub band's latest window.
    pub fn blend_into(&mut self, bands: &mut [f64]) {
        let tail = self.ring_l.len() - self.pos;
        self.fl[..tail].copy_from_slice(&self.ring_l[self.pos..]);
        self.fl[tail..].copy_from_slice(&self.ring_l[..self.pos]);
        self.fr[..tail].copy_from_slice(&self.ring_r[self.pos..]);
        self.fr[tail..].copy_from_slice(&self.ring_r[..self.pos]);
        self.stft.frame_power_lr_with(&self.fl, &self.fr, &mut self.buf, &mut self.scratch, &mut self.power);
        let nb = self.weight.len().min(bands.len());
        self.bins.band_power(&self.power, 0, nb, &mut self.bands[..nb]);
        for b in 0..nb {
            let w = self.weight[b];
            bands[b] = w * self.bands[b] + (1.0 - w) * bands[b];
        }
    }
}

// ─── Multi-resolution spectrogram columns ────────────────────────────────────

/// The columns' byte scale: 0 = −120 dB, 255 = 0 dB.
pub const SPEC_DB_FLOOR: f64 = -120.0;
pub const SPEC_DB_RANGE: f64 = 120.0;
/// Where the resolutions hand over (Hz; raised-cosine blends in log
/// frequency): the long FFT below the first pair, the short one above the
/// second, the reference between.
pub const SPEC_LOW_BLEND: (f64, f64) = (150.0, 300.0);
pub const SPEC_HIGH_BLEND: (f64, f64) = (2_000.0, 4_000.0);

/// The log step of the spectrogram's bands for a signal at `rate`: 512
/// bands from 20 Hz to its Nyquist. Another signal's bands (the output's,
/// at a higher rate) take the same step, so band k is the same everywhere.
pub fn spec_ln_step(rate: u32) -> f64 {
    (rate as f64 / 2.0 / SPEC_F0_HZ).ln() / SPEC_LOG_BINS as f64
}

/// The bands a signal at `rate` has on the grid of a source at `src_rate`:
/// the source's 512, and above its Nyquist as many more as fit under this
/// rate's own.
pub fn spec_bands_on(src_rate: u32, rate: u32) -> usize {
    let extra = ((rate as f64 / src_rate as f64).ln() / spec_ln_step(src_rate) + 1e-9).floor();
    SPEC_LOG_BINS + extra.max(0.0) as usize
}

/// Samples for the spectrogram (shared by the threads computing tiles).
pub trait SpecSource: Sync {
    /// Samples `[start, start + l.len())` into `l` and `r`, zeros outside
    /// the signal.
    fn frame(&self, start: isize, l: &mut [f64], r: &mut [f64]);
}

/// A signal held whole: f64 (a pass's own buffer) or f32 (the zoom's copy,
/// exact for 16/24-bit and float files).
pub struct SliceSource<'a, T> {
    pub l: &'a [T],
    pub r: &'a [T],
}

impl<T: Copy + Into<f64> + Sync> SpecSource for SliceSource<'_, T> {
    fn frame(&self, start: isize, l: &mut [f64], r: &mut [f64]) {
        copy_frame(self.l, self.r, 0, start, l, r);
    }
}

/// Samples `[start, start + l.len())` of a signal whose `sl[0]` is sample
/// `base`, zeros outside it.
fn copy_frame<T: Copy + Into<f64>>(sl: &[T], sr: &[T], base: usize, start: isize, l: &mut [f64], r: &mut [f64]) {
    l.fill(0.0);
    r.fill(0.0);
    let n = l.len() as isize;
    let (have0, have1) = (base as isize, (base + sl.len()) as isize);
    let a = start.max(have0);
    let b = (start + n).min(have1);
    for i in a..b {
        let (d, s) = ((i - start) as usize, (i - have0) as usize);
        l[d] = sl[s].into();
        r[d] = sr[s].into();
    }
}

/// One FFT length of the multi-resolution spectrogram.
struct Res {
    stft: Stft,
    bins: LogBins,
    /// Columns from one computed frame to the next (≥ 1); the columns
    /// between take the power interpolated between the two.
    stride: usize,
    /// Frames averaged into each column (≥ 1; > 1 when a column is longer
    /// than the frames' spacing, so no transient falls between frames).
    per_col: usize,
    /// Scale to the reference's reading (per-bin power density).
    scale: f64,
    /// The bands this length contributes to, and each band's weight.
    b0: usize,
    b1: usize,
    weight: Vec<f64>,
    /// 1, or the sub band's decimation (its frames are of the decimated
    /// signal, the `sub` source) and its filter's half-length.
    dec: usize,
    reach: usize,
}

/// Scratch buffers of one thread.
#[derive(Default)]
pub struct SpecScratch {
    fl: Vec<f64>,
    fr: Vec<f64>,
    buf: Vec<Complex<f64>>,
    fft: Vec<Complex<f64>>,
    power: Vec<f64>,
}

/// Spectrogram columns of `hop` samples in log bands, from three FFT
/// lengths and the sub band: the sub band below ~35 Hz (`SubFir`: bins
/// 0.34 Hz wide, a ~3 s window), 4× the reference below ~200 Hz (bins 2.7 Hz
/// wide, so bass notes separate), the reference (`spec_fft_len`: ≈ 93 ms)
/// in the middle, and ¼ of it above ~3 kHz (≈ 23 ms: sharp transients) when
/// the columns are short enough to show that.
///
/// Levels are those of the reference (the mean power per FFT bin, a full-
/// scale sine in its bin reads 0 dB): the short FFT's per-bin power is
/// scaled to the reference's bin width, so noise and a band's tones read the
/// same across the blend; the long one and the sub band are not scaled
/// (their bands are narrower than a bin, where a tone reads its own level at
/// any length), so their noise floor sits lower — 6 dB, and 15 dB in the
/// sub band: the finer resolution.
///
/// Every frame is centred on its column (column c = samples
/// `[c·hop, (c+1)·hop)`), the same for any signal on the same time grid,
/// and a column's value never depends on which other columns are computed
/// with it: tiles computed apart line up.
pub struct SpecEngine {
    pub hop: usize,
    pub n_bands: usize,
    res: Vec<Res>,
}

/// Raised-cosine step from 0 (at or below `a`) to 1 (at or above `b`), in
/// log frequency.
fn blend(f: f64, (a, b): (f64, f64)) -> f64 {
    let t = ((f.ln() - a.ln()) / (b.ln() - a.ln())).clamp(0.0, 1.0);
    0.5 - 0.5 * (std::f64::consts::PI * t).cos()
}

impl SpecEngine {
    /// Columns of `hop` samples of a signal at `rate`, `n_bands` bands from
    /// SPEC_F0_HZ at `ln_step` (see `spec_ln_step`).
    pub fn new(rate: u32, hop: usize, n_bands: usize, ln_step: f64) -> SpecEngine {
        let hop = hop.max(1);
        let n_ref = spec_fft_len(rate);
        let fir = SubFir::new(rate);
        let centre = |k: usize| SPEC_F0_HZ * ((k as f64 + 0.5) * ln_step).exp();
        let short_on = hop <= n_ref / 16;
        let above_sub = |f: f64| blend(f, SPEC_SUB_BLEND);
        // (FFT length, decimation, weight at f)
        let lens: [(usize, usize, Box<dyn Fn(f64) -> f64>); 4] = [
            (SPEC_SUB_FFT, fir.d, Box::new(sub_weight)),
            (n_ref * 4, 1, Box::new(move |f| (1.0 - blend(f, SPEC_LOW_BLEND)) * above_sub(f))),
            (n_ref, 1, Box::new(move |f| blend(f, SPEC_LOW_BLEND) - if short_on { blend(f, SPEC_HIGH_BLEND) } else { 0.0 })),
            (n_ref / 4, 1, Box::new(move |f| if short_on { blend(f, SPEC_HIGH_BLEND) } else { 0.0 })),
        ];
        let res = lens.into_iter().filter_map(|(n, dec, w)| {
            let weight: Vec<f64> = (0..n_bands).map(|k| w(centre(k)).max(0.0)).collect();
            let b0 = weight.iter().position(|&x| x > 1e-12)?;
            let b1 = weight.iter().rposition(|&x| x > 1e-12)? + 1;
            // A frame every n/8 samples (of its own signal): Kaiser 16's
            // power window is ~n/11 wide (σ), so a click between two frames
            // still reads −1 dB.
            let spacing = (n / 8).max(1) * dec;
            let (stride, per_col) = if spacing >= hop { (spacing / hop, 1) } else { (1, hop.div_ceil(spacing)) };
            let df = rate as f64 / (n * dec) as f64;
            Some(Res {
                stft: Stft::new(n, n, SPEC_BETA),
                bins: LogBins::with_bin(n, df, n_bands, SPEC_F0_HZ, ln_step),
                stride,
                per_col,
                scale: if n * dec < n_ref { (n * dec) as f64 / n_ref as f64 } else { 1.0 },
                b0,
                b1,
                weight,
                dec,
                reach: if dec > 1 { fir.k } else { 0 },
            })
        }).collect();
        SpecEngine { hop, n_bands, res }
    }

    /// The decimated sample a frame centred on input sample `c` is centred on.
    fn sub_centre(res: &Res, c: isize) -> isize {
        (c + (res.dec / 2) as isize).div_euclid(res.dec as isize)
    }

    /// Centre sample of frame `j` of column `c` (per_col frames), or of the
    /// frame computed at column `c` (stride).
    fn centre(&self, res: &Res, c: usize, j: usize) -> isize {
        (c * self.hop + (2 * j + 1) * self.hop / (2 * res.per_col)) as isize
    }

    /// The centre of the last frame column `c` takes.
    fn last_centre(&self, res: &Res, c: usize) -> isize {
        if res.stride > 1 {
            self.centre(res, (c / res.stride + 1) * res.stride, 0)
        } else {
            self.centre(res, c, res.per_col - 1)
        }
    }

    /// The centre of the first frame columns from `c` on take.
    fn first_centre(&self, res: &Res, c: usize) -> isize {
        if res.stride > 1 {
            self.centre(res, c / res.stride * res.stride, 0)
        } else {
            self.centre(res, c, 0)
        }
    }

    /// The last input sample (exclusive) column `c` needs — the sub band's
    /// through the decimated samples it takes (each reaching k past its own).
    pub fn needs_until(&self, c: usize) -> usize {
        self.res.iter().map(|res| {
            let n = res.stft.n as isize;
            let last = self.last_centre(res, c);
            if res.dec > 1 {
                let j1 = Self::sub_centre(res, last) - n / 2 + n; // exclusive
                ((j1 - 1) * res.dec as isize + res.reach as isize + 1).max(0) as usize
            } else {
                (last - n / 2 + n).max(0) as usize
            }
        }).max().unwrap_or(0)
    }

    /// The first input sample columns from `c` on need (the sub band reads
    /// the decimated signal: see `sub_needs_from`).
    pub fn needs_from(&self, c: usize) -> usize {
        self.res.iter().filter(|res| res.dec == 1).map(|res| {
            (self.first_centre(res, c) - res.stft.n as isize / 2).max(0) as usize
        }).min().unwrap_or(0)
    }

    /// The first decimated sample columns from `c` on need.
    pub fn sub_needs_from(&self, c: usize) -> usize {
        self.res.iter().filter(|res| res.dec > 1).map(|res| {
            (Self::sub_centre(res, self.first_centre(res, c)) - res.stft.n as isize / 2).max(0) as usize
        }).min().unwrap_or(0)
    }

    /// One frame's band power (bands b0..b1 of `res`, scaled): of `src`, or
    /// of `sub` (the decimated signal) for the sub band.
    fn frame_bands(
        &self,
        res: &Res,
        src: &dyn SpecSource,
        sub: &dyn SpecSource,
        centre: isize,
        s: &mut SpecScratch,
        out: &mut Vec<f64>,
    ) {
        let n = res.stft.n;
        s.fl.resize(n, 0.0);
        s.fr.resize(n, 0.0);
        if res.dec > 1 {
            sub.frame(Self::sub_centre(res, centre) - (n / 2) as isize, &mut s.fl, &mut s.fr);
        } else {
            src.frame(centre - (n / 2) as isize, &mut s.fl, &mut s.fr);
        }
        res.stft.frame_power_lr_with(&s.fl, &s.fr, &mut s.buf, &mut s.fft, &mut s.power);
        out.resize(res.b1 - res.b0, 0.0);
        res.bins.band_power(&s.power, res.b0, res.b1, out);
        if res.scale != 1.0 {
            for v in out.iter_mut() { *v *= res.scale; }
        }
    }

    /// Columns `c0..c1` as bytes (`n_bands` per column, SPEC_DB_FLOOR..0 dB)
    /// of `src` and of `sub`, the same signal decimated by `SubFir`.
    pub fn columns(
        &self,
        src: &dyn SpecSource,
        sub: &dyn SpecSource,
        c0: usize,
        c1: usize,
        s: &mut SpecScratch,
    ) -> Vec<Vec<u8>> {
        let nb = self.n_bands;
        let mut acc = vec![0.0f64; (c1 - c0) * nb];
        let mut f = Vec::new();
        for res in &self.res {
            let w = &res.weight[res.b0..res.b1];
            if res.stride > 1 {
                // Frames at every stride-th column, interpolated between.
                let s0 = res.stride;
                let (k0, k1) = (c0 / s0, (c1 - 1) / s0 + 1);
                let frames: Vec<Vec<f64>> = (k0..=k1).map(|k| {
                    let mut v = Vec::new();
                    self.frame_bands(res, src, sub, self.centre(res, k * s0, 0), s, &mut v);
                    v
                }).collect();
                for c in c0..c1 {
                    let k = c / s0;
                    let t = (c - k * s0) as f64 / s0 as f64;
                    let (fa, fb) = (&frames[k - k0], &frames[k - k0 + 1]);
                    let row = &mut acc[(c - c0) * nb + res.b0..(c - c0) * nb + res.b1];
                    for i in 0..row.len() {
                        row[i] += w[i] * ((1.0 - t) * fa[i] + t * fb[i]);
                    }
                }
            } else {
                let inv = 1.0 / res.per_col as f64;
                for c in c0..c1 {
                    for j in 0..res.per_col {
                        self.frame_bands(res, src, sub, self.centre(res, c, j), s, &mut f);
                        let row = &mut acc[(c - c0) * nb + res.b0..(c - c0) * nb + res.b1];
                        for i in 0..row.len() {
                            row[i] += w[i] * f[i] * inv;
                        }
                    }
                }
            }
        }
        acc.chunks(nb)
            .map(|p| p.iter().map(|&v| power_to_u8(v, SPEC_DB_FLOOR, SPEC_DB_RANGE)).collect())
            .collect()
    }
}

// ─── Welch long-term spectrum ─────────────────────────────────────────────────

/// Streaming Welch long-term power spectral density estimator with L/R
/// cross-spectrum.
///
/// Pushing any block size (including sizes smaller than n) gives the same
/// result as a one-shot call over the concatenated signal, because frames are
/// extracted from a continuous carry buffer at fixed hop positions.
///
/// Ref: Welch, "The use of fast Fourier transform for the estimation of power
/// spectra", IEEE Trans. Audio Electroacoust., 1967.
pub struct WelchAccum {
    /// Running sum of |X_L[k]|² per bin.
    power_l: Vec<f64>,
    /// Running sum of |X_R[k]|² per bin.
    power_r: Vec<f64>,
    /// Running sum of X_L[k] · X_R[k]* (L/R cross-spectrum).
    cross: Vec<Complex<f64>>,
    n_frames: usize,
    stft: Stft,
    carry_l: Vec<f64>,
    carry_r: Vec<f64>,
}

impl WelchAccum {
    /// Create a new accumulator. `beta = 28.0` is a good default.
    pub fn new(n: usize, hop: usize, beta: f64) -> WelchAccum {
        let half = n / 2 + 1;
        WelchAccum {
            power_l: vec![0.0; half],
            power_r: vec![0.0; half],
            cross: vec![Complex::new(0.0, 0.0); half],
            n_frames: 0,
            stft: Stft::new(n, hop, beta),
            carry_l: Vec::new(),
            carry_r: Vec::new(),
        }
    }

    /// Push a block of samples. Any block size is accepted. Frames are
    /// extracted at multiples of `hop` from the start of the stream.
    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.carry_l.extend_from_slice(l);
        self.carry_r.extend_from_slice(r);
        let n = self.stft.n;
        let hop = self.stft.hop;
        let half = n / 2 + 1;
        while self.carry_l.len() >= n {
            let xl = self.stft.frame_complex(&self.carry_l[..n]);
            let xr = self.stft.frame_complex(&self.carry_r[..n]);
            for k in 0..half {
                self.power_l[k] += xl[k].norm_sqr();
                self.power_r[k] += xr[k].norm_sqr();
                self.cross[k] += xl[k] * xr[k].conj();
            }
            self.n_frames += 1;
            self.carry_l.drain(..hop);
            self.carry_r.drain(..hop);
        }
    }

    /// Finalize and return `(psd_l, psd_r, cross_spectrum)`.
    ///
    /// `psd_l` and `psd_r` are in **dBFS power** (10·log₁₀ of the averaged
    /// squared magnitude with the same coherent-gain normalization as
    /// `Stft::frame_mag`, so a full-scale sine reads ≈ 0 dBFS-power).
    ///
    /// `cross_spectrum` is the complex average `Σ X_L[k]·X_R[k]* / n_frames`,
    /// useful for estimating interaural cross-correlation and for XTC
    /// filter modelling.
    pub fn finish(&self) -> (Vec<f64>, Vec<f64>, Vec<Complex<f64>>) {
        let half = self.stft.n / 2 + 1;
        if self.n_frames == 0 {
            let floor = vec![-200.0; half];
            return (floor.clone(), floor, vec![Complex::new(0.0, 0.0); half]);
        }
        let inv = 1.0 / self.n_frames as f64;
        // norm² converts the raw squared FFT magnitude to the same dBFS-sine
        // basis as frame_mag (squared: divide by (N·CG/2)² = (1/norm)²).
        let norm2 = self.stft.norm * self.stft.norm;
        let to_db = |sum: f64| 10.0 * (sum * inv * norm2).max(1e-240).log10();
        let pl: Vec<f64> = self.power_l.iter().map(|&p| to_db(p)).collect();
        let pr: Vec<f64> = self.power_r.iter().map(|&p| to_db(p)).collect();
        let cross: Vec<Complex<f64>> =
            self.cross.iter().map(|&c| c * (inv * norm2)).collect();
        (pl, pr, cross)
    }
}

// ─── Band-level helpers ───────────────────────────────────────────────────────

/// Peak and RMS level of the spectral region `[lo_hz, hi_hz)`.
///
/// `spectrum` is in dBFS (from `Stft::frame_mag` or `WelchAccum::finish`).
/// `sample_rate` is the signal's sample rate in Hz.
///
/// Returns `(peak_db, rms_db)`. When no bins fall in range returns
/// `(−200, −200)`.
pub fn band_level(spectrum: &[f64], lo_hz: f64, hi_hz: f64, sample_rate: f64) -> (f64, f64) {
    let n_fft = (spectrum.len() - 1) * 2;
    let bin_hz = sample_rate / n_fft as f64;
    let lo_bin = (lo_hz / bin_hz).floor() as usize;
    let hi_bin = ((hi_hz / bin_hz).ceil() as usize + 1).min(spectrum.len());
    if lo_bin >= hi_bin {
        return (-200.0, -200.0);
    }
    let slice = &spectrum[lo_bin..hi_bin];
    let peak = slice.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let power_sum: f64 = slice.iter().map(|&db| 10f64.powf(db / 10.0)).sum();
    let rms_db = 10.0 * (power_sum / slice.len() as f64).max(1e-240).log10();
    (peak, rms_db)
}

/// Effective bandwidth: the highest frequency (Hz) where the long-term spectrum
/// is at or above the noise floor by `margin_db`.
///
/// **Algorithm:**
/// 1. Estimate the noise floor as the median of the spectrum in 100 Hz–1 kHz
///    (a stable, program-material-rich band).
/// 2. Threshold = noise_floor + margin_db.
/// 3. Scan downward from Nyquist; return the frequency of the first bin that
///    exceeds the threshold.
///
/// **Interpretation:** a value near `sample_rate / 2` indicates genuine
/// high-frequency content; a value well below Nyquist (e.g. < 20 kHz for a
/// 96 kHz file) suggests a bandwidth-limited source that has been synthetically
/// extended — the "fake hi-res" detector. `margin_db = 40.0` is a reasonable
/// default (40 dB above the noise floor).
pub fn effective_bandwidth(spectrum: &[f64], sample_rate: f64, margin_db: f64) -> f64 {
    let n_fft = (spectrum.len() - 1) * 2;
    let bin_hz = sample_rate / n_fft as f64;

    // Noise-floor estimate: median in 100 Hz – 1000 Hz.
    let lo = (100.0 / bin_hz).floor() as usize;
    let hi = ((1000.0 / bin_hz).ceil() as usize + 1).min(spectrum.len());
    if lo >= hi {
        return 0.0;
    }
    let mut ref_slice: Vec<f64> = spectrum[lo..hi].to_vec();
    ref_slice.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_floor = ref_slice[ref_slice.len() / 2];
    let threshold = noise_floor + margin_db;

    // Scan downward from Nyquist.
    for i in (0..spectrum.len()).rev() {
        if spectrum[i] >= threshold {
            return i as f64 * bin_hz;
        }
    }
    0.0
}

// ─── Log-band display helpers ─────────────────────────────────────────────────

/// Map bin `k` (of an FFT of size `n_fft`) to a log-frequency display band
/// index in `[0, n_bands)`, or `None` if outside `[lo_hz, hi_hz]`.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub fn bin_to_log_band(
    k: usize,
    n_fft: usize,
    sample_rate: f64,
    lo_hz: f64,
    hi_hz: f64,
    n_bands: usize,
) -> Option<usize> {
    let bin_hz = sample_rate / n_fft as f64;
    let f = k as f64 * bin_hz;
    if f < lo_hz || f > hi_hz || n_bands == 0 {
        return None;
    }
    let log_lo = lo_hz.ln();
    let log_hi = hi_hz.ln();
    if (log_hi - log_lo).abs() < 1e-15 {
        return Some(0);
    }
    let frac = (f.ln() - log_lo) / (log_hi - log_lo);
    let band = (frac * n_bands as f64) as usize;
    Some(band.min(n_bands - 1))
}

/// Aggregate a spectrum into `n_bands` logarithmically-spaced display bands.
///
/// Each band holds the **maximum** dBFS value of the bins that fall in it.
/// Bands with no contributing bins are set to `floor_db`.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub fn to_log_bands(
    spectrum: &[f64],
    sample_rate: f64,
    lo_hz: f64,
    hi_hz: f64,
    n_bands: usize,
    floor_db: f64,
) -> Vec<f64> {
    let n_fft = (spectrum.len() - 1) * 2;
    let mut bands = vec![floor_db; n_bands];
    for (k, &db) in spectrum.iter().enumerate() {
        if let Some(b) = bin_to_log_band(k, n_fft, sample_rate, lo_hz, hi_hz, n_bands) {
            if db > bands[b] {
                bands[b] = db;
            }
        }
    }
    bands
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-scale cosine at an integer bin must read 0 dBFS within 0.01 dB,
    /// for several FFT sizes and sample rates.
    #[test]
    fn sine_amplitude_readback() {
        for &n in &[512usize, 1024, 4096, 8192] {
            for &_rate in &[44100u32, 96000, 192000] {
                let stft = Stft::new(n, n / 2, 28.0);
                // Choose an integer bin well away from DC and Nyquist.
                let k = n / 8;
                let x: Vec<f64> = (0..n)
                    .map(|i| (2.0 * std::f64::consts::PI * k as f64 * i as f64 / n as f64).cos())
                    .collect();
                let mag = stft.frame_mag(&x);
                let db = mag[k];
                assert!(
                    db.abs() < 0.01,
                    "n={} k={}: full-scale cosine reads {:.4} dBFS (want ≈0)",
                    n,
                    k,
                    db
                );
            }
        }
    }

    /// Kaiser sidelobes must be below −240 dB for beta=28, measured at f64
    /// precision on a pure tone at an integer bin.
    #[test]
    fn kaiser_sidelobes_below_240_db() {
        let n = 8192usize;
        let beta = 28.0f64;
        let k = n / 4; // well away from DC and Nyquist
        let window = kaiser(n, beta);
        let mut buf: Vec<Complex<f64>> = window
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let phase = 2.0 * std::f64::consts::PI * k as f64 * i as f64 / n as f64;
                Complex::new(w * phase.cos(), w * phase.sin())
            })
            .collect();
        let fft = FftPlanner::<f64>::new().plan_fft_forward(n);
        fft.process(&mut buf);
        let peak = buf[k].norm();
        let sidelobe = buf
            .iter()
            .enumerate()
            // beta=28 main lobe extends ~10 bins each side; exclude 15 to be safe
            .filter(|&(i, _)| (i as isize - k as isize).abs() > 15)
            .map(|(_, c)| c.norm())
            .fold(0.0f64, f64::max);
        let ratio_db = 20.0 * (sidelobe / peak.max(1e-300)).log10();
        assert!(
            ratio_db < -240.0,
            "Kaiser beta={beta} sidelobe at {ratio_db:.1} dB (want < -240 dB)"
        );
    }

    /// Welch PSD of white noise must be approximately flat (within 3 dB across
    /// 100 Hz – 10 kHz in a 44.1 kHz signal).
    #[test]
    fn welch_white_noise_flat() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
        let n = 4096usize;
        let sample_rate = 44100.0f64;
        let n_samples = n * 256;
        let noise: Vec<f64> = (0..n_samples).map(|_| rng.gen_range(-1.0f64..1.0)).collect();
        let mut wa = WelchAccum::new(n, n / 2, 28.0);
        wa.push(&noise, &noise);
        let (pl, _pr, _cross) = wa.finish();
        let bin_hz = sample_rate / n as f64;
        let lo = (100.0 / bin_hz) as usize;
        let hi = (10000.0 / bin_hz) as usize;
        let slice = &pl[lo..hi];
        let min = slice.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = slice.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            max - min < 3.0,
            "Welch white noise: {:.1} dB range in 100 Hz–10 kHz (want < 3 dB)",
            max - min
        );
    }

    /// Streaming and one-shot Welch give identical results.
    #[test]
    fn welch_streaming_equals_oneshot() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
        let n = 1024usize;
        let n_samples = n * 64;
        let sig: Vec<f64> = (0..n_samples).map(|_| rng.gen_range(-1.0f64..1.0)).collect();

        // One-shot.
        let mut wa_one = WelchAccum::new(n, n / 2, 6.0);
        wa_one.push(&sig, &sig);
        let (pl_one, _, _) = wa_one.finish();

        // Streaming in small chunks of 77 samples.
        let mut wa_str = WelchAccum::new(n, n / 2, 6.0);
        for chunk in sig.chunks(77) {
            wa_str.push(chunk, chunk);
        }
        let (pl_str, _, _) = wa_str.finish();

        assert_eq!(pl_one.len(), pl_str.len());
        for (a, b) in pl_one.iter().zip(&pl_str) {
            assert!(
                (a - b).abs() < 1e-10,
                "streaming vs oneshot diverge: {a} vs {b}"
            );
        }
    }

    #[test]
    fn frame_power_lr_reads_a_full_scale_sine_as_0_db() {
        let n = 4096;
        let stft = Stft::new(n, n, SPEC_BETA);
        let k = 300;
        let x: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * k as f64 * i as f64 / n as f64).sin())
            .collect();
        let silent = vec![0.0; n];
        let mut p = Vec::new();
        stft.frame_power_lr(&x, &x, &mut p);
        assert!((10.0 * p[k].log10()).abs() < 0.01, "both channels: {}", 10.0 * p[k].log10());
        // One channel only: the mean of L and R power is 3 dB down.
        stft.frame_power_lr(&x, &silent, &mut p);
        assert!((10.0 * p[k].log10() + 3.0103).abs() < 0.01);
        // An anti-phase pair still shows (a mono mix would cancel it).
        let neg: Vec<f64> = x.iter().map(|v| -v).collect();
        stft.frame_power_lr(&x, &neg, &mut p);
        assert!((10.0 * p[k].log10()).abs() < 0.01);
    }

    fn tone(rate: u32, f: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin()).collect()
    }

    fn band_of(f: f64, rate: u32) -> usize {
        ((f / SPEC_F0_HZ).ln() / spec_ln_step(rate)) as usize
    }

    fn db(byte: u8) -> f64 {
        SPEC_DB_FLOOR + byte as f64 / 255.0 * SPEC_DB_RANGE
    }

    /// A −6 dBFS tone reads −6 dB in the lows (the long FFT, bands narrower
    /// than its bins) and in the reference's narrow bands; at any hop.
    #[test]
    fn multires_reads_a_tone_at_its_level() {
        let rate = 44_100;
        let n = rate as usize * 4;
        for &hop in &[128usize, 646, 4000] {
            let eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
            for &f in &[50.0f64, 110.0, 440.0] {
                let x = tone(rate, f, 0.5, n);
                let src = SliceSource { l: &x, r: &x };
                let sub = SubSignal::of(&x, &x, rate);
                let c = n / 2 / hop;
                let cols = eng.columns(&src, &sub.source(), c, c + 1, &mut SpecScratch::default());
                let col = &cols[0];
                let peak = (0..SPEC_LOG_BINS).max_by_key(|&k| col[k]).unwrap();
                assert!((peak as i64 - band_of(f, rate) as i64).abs() <= 2, "{f} Hz hop {hop}: band {peak}");
                assert!((db(col[peak]) + 6.02).abs() < 0.8, "{f} Hz hop {hop}: {:.2} dB", db(col[peak]));
            }
        }
    }

    /// Where only the reference contributes, the engine is the reference.
    #[test]
    fn multires_mids_are_the_reference() {
        use rand::{Rng, SeedableRng};
        let rate = 48_000;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(3);
        let n = 200_000;
        let l: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.3..0.3)).collect();
        let r: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.3..0.3)).collect();
        let hop = 1000;
        let eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
        let c = 90;
        let sub = SubSignal::of(&l, &r, rate);
        let got = &eng.columns(&SliceSource { l: &l, r: &r }, &sub.source(), c, c + 1, &mut SpecScratch::default())[0];
        let nf = spec_fft_len(rate);
        let stft = Stft::new(nf, nf, SPEC_BETA);
        let bins = LogBins::new(nf, rate, SPEC_LOG_BINS, SPEC_F0_HZ);
        let (mut fl, mut fr) = (vec![0.0; nf], vec![0.0; nf]);
        // hop 1000 > nf/8 = 512: two frames per column, averaged.
        let mut p = vec![0.0; nf / 2 + 1];
        for j in 0..2 {
            let centre = (c * hop + (2 * j + 1) * hop / 4) as isize;
            SliceSource { l: &l, r: &r }.frame(centre - nf as isize / 2, &mut fl, &mut fr);
            let mut q = Vec::new();
            stft.frame_power_lr(&fl, &fr, &mut q);
            for (a, b) in p.iter_mut().zip(&q) { *a += b / 2.0; }
        }
        let mut want = vec![0u8; SPEC_LOG_BINS];
        bins.to_u8(&p, SPEC_DB_FLOOR, SPEC_DB_RANGE, &mut want);
        for k in band_of(320.0, rate)..band_of(1900.0, rate) {
            assert!((got[k] as i32 - want[k] as i32).abs() <= 1, "band {k}: {} vs {}", got[k], want[k]);
        }
    }

    /// The short FFT (highs, short columns) reads noise at the reference's
    /// level: no step at the blend.
    #[test]
    fn multires_short_fft_keeps_the_noise_level() {
        use rand::{Rng, SeedableRng};
        let rate = 44_100;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(9);
        let n = 400_000;
        let l: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.3..0.3)).collect();
        let src = SliceSource { l: &l, r: &l };
        let sub = SubSignal::of(&l, &l, rate);
        let mean_db = |hop: usize, lo: f64, hi: f64| {
            let eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
            let c0 = 20_000 / hop;
            let cols = eng.columns(&src, &sub.source(), c0, c0 + 200_000 / hop, &mut SpecScratch::default());
            let (a, b) = (band_of(lo, rate), band_of(hi, rate));
            // Mean power (a mean of dB would favour the smoother estimate).
            let sum: f64 = cols.iter().map(|c| (a..b).map(|k| 10f64.powf(db(c[k]) / 10.0)).sum::<f64>()).sum();
            10.0 * (sum / (cols.len() * (b - a)) as f64).log10()
        };
        // hop 128 has the short FFT above 3 kHz, hop 646 does not.
        let fine = mean_db(128, 6_000.0, 16_000.0);
        let base = mean_db(646, 6_000.0, 16_000.0);
        assert!((fine - base).abs() < 0.5, "short {fine:.2} dB vs reference {base:.2} dB");
    }

    /// Columns computed in one piece or in several are the same bytes.
    #[test]
    fn multires_columns_do_not_depend_on_the_chunks() {
        use rand::{Rng, SeedableRng};
        let rate = 44_100;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(5);
        let n: usize = 150_000;
        let l: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect();
        let r: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect();
        let src = SliceSource { l: &l, r: &r };
        let sub = SubSignal::of(&l, &r, rate);
        for &hop in &[128usize, 300, 2000] {
            let eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
            let n_cols = n.div_ceil(hop);
            let mut s = SpecScratch::default();
            let whole = eng.columns(&src, &sub.source(), 0, n_cols, &mut s);
            let mut parts = Vec::new();
            let mut c = 0;
            for step in [7usize, 64, 1, 33].iter().cycle() {
                if c >= n_cols { break; }
                let e = (c + step).min(n_cols);
                parts.extend(eng.columns(&src, &sub.source(), c, e, &mut s));
                c = e;
            }
            assert_eq!(whole, parts, "hop {hop}");
        }
    }

    /// A click lands in the same column of a signal and of the same signal
    /// at twice the rate on the doubled hop (S and O tiles line up); the
    /// output's first 512 bands are the source's.
    #[test]
    fn multires_output_columns_line_up_with_the_source() {
        let (rate, l2) = (44_100u32, 2usize);
        assert_eq!(spec_bands_on(rate, rate), SPEC_LOG_BINS);
        let nb = spec_bands_on(rate, rate * 2);
        assert_eq!(nb, 554);
        let top = SPEC_F0_HZ * (nb as f64 * spec_ln_step(rate)).exp();
        assert!(top <= rate as f64 && top > rate as f64 * 0.98, "top band edge {top}");
        let n: usize = 100_000;
        let at = 61_234;
        let mut x = vec![0.0; n];
        x[at] = 1.0;
        let mut y = vec![0.0; n * l2];
        y[at * l2] = 1.0;
        let hop = 200;
        let es = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
        let eo = SpecEngine::new(rate * 2, hop * l2, nb, spec_ln_step(rate));
        let (xs, ys) = (SubSignal::of(&x, &x, rate), SubSignal::of(&y, &y, rate * 2));
        let cs = es.columns(&SliceSource { l: &x, r: &x }, &xs.source(), 0, n.div_ceil(hop), &mut SpecScratch::default());
        let co = eo.columns(&SliceSource { l: &y, r: &y }, &ys.source(), 0, n.div_ceil(hop), &mut SpecScratch::default());
        let k = band_of(8_000.0, rate);
        let peak = |cols: &[Vec<u8>]| (0..cols.len()).max_by_key(|&c| cols[c][k]).unwrap();
        assert_eq!(peak(&cs), at / hop);
        assert_eq!(peak(&co), at / hop);
    }

    /// One column in the middle of `x` (L = R) at `hop`, in dB per band.
    fn mid_column(rate: u32, hop: usize, x: &[f64]) -> Vec<f64> {
        let eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
        let sub = SubSignal::of(x, x, rate);
        let c = x.len() / 2 / hop;
        let cols = eng.columns(&SliceSource { l: x, r: x }, &sub.source(), c, c + 1, &mut SpecScratch::default());
        cols[0].iter().map(|&b| db(b)).collect()
    }

    fn band_centre(k: usize, rate: u32) -> f64 {
        SPEC_F0_HZ * ((k as f64 + 0.5) * spec_ln_step(rate)).exp()
    }

    /// The sub band: 8 and 12 Hz at −6 dBFS land in their band at their
    /// level (±1 dB), on both rate families and at several hops; the bands
    /// 3 Hz away, up to the blend's top, are under −100 dB (no 11 Hz smear,
    /// no hump where the long FFT takes over).
    #[test]
    fn sub_band_reads_8_and_12_hz_at_their_level() {
        for &rate in &[44_100u32, 48_000, 176_400] {
            let n = rate as usize * 8;
            for &hop in &[256usize, 646, 4096] {
                for &f in &[8.0f64, 12.0] {
                    let col = mid_column(rate, hop, &tone(rate, f, 0.5, n));
                    let peak = (0..SPEC_LOG_BINS).max_by(|&a, &b| col[a].total_cmp(&col[b])).unwrap();
                    assert!((peak as i64 - band_of(f, rate) as i64).abs() <= 2, "{rate} {f} Hz hop {hop}: band {peak}");
                    assert!((col[peak] + 6.02).abs() < 1.0, "{rate} {f} Hz hop {hop}: {:.2} dB", col[peak]);
                    for k in 0..SPEC_LOG_BINS {
                        let fc = band_centre(k, rate);
                        if (fc - f).abs() > 3.0 && fc < SPEC_SUB_BLEND.1 {
                            assert!(col[k] < -100.0, "{rate} {f} Hz hop {hop}: {fc:.2} Hz reads {:.1} dB", col[k]);
                        }
                    }
                }
            }
        }
    }

    /// Nothing folds into the sub band: 1 kHz, and the tones that would land
    /// on 8 Hz after the decimation (the decimated rate ± 8 Hz, twice it),
    /// at 0 dBFS, leave every band below 20 Hz under −100 dB.
    #[test]
    fn sub_band_has_no_aliases() {
        for &rate in &[44_100u32, 96_000] {
            let fd = SubFir::new(rate).rate_out(rate);
            let n = rate as usize * 6;
            for &f in &[1_000.0, fd - 8.0, fd + 8.0, 2.0 * fd + 8.0, 5_000.0] {
                let col = mid_column(rate, 646, &tone(rate, f, 1.0, n));
                for k in (0..SPEC_LOG_BINS).take_while(|&k| band_centre(k, rate) < 20.0) {
                    assert!(col[k] < -100.0, "{rate}: {f:.1} Hz shows {:.1} dB at {:.2} Hz", col[k], band_centre(k, rate));
                }
            }
        }
    }

    /// A DC offset stays out of the bands (they start at 5 Hz): −20 dBFS of
    /// DC under a 100 Hz tone leaves the bands from 5 Hz to 25 Hz under
    /// −100 dB, and the tone reads its level.
    #[test]
    fn sub_band_keeps_dc_out() {
        let rate = 44_100;
        let n = rate as usize * 6;
        let x: Vec<f64> = tone(rate, 100.0, 0.5, n).iter().map(|v| v + 0.1).collect();
        let col = mid_column(rate, 646, &x);
        for k in (0..SPEC_LOG_BINS).take_while(|&k| band_centre(k, rate) < 25.0) {
            assert!(col[k] < -100.0, "{:.2} Hz reads {:.1} dB", band_centre(k, rate), col[k]);
        }
        let peak = (0..SPEC_LOG_BINS).max_by(|&a, &b| col[a].total_cmp(&col[b])).unwrap();
        assert!((col[peak] + 6.02).abs() < 1.0 && (peak as i64 - band_of(100.0, rate) as i64).abs() <= 2);
    }

    /// The decimating filter: flat to 0.001 dB through the sub band and the
    /// blend, ≥ 120 dB down where it would fold into it.
    #[test]
    fn sub_fir_passes_the_lows_and_stops_what_would_fold() {
        for &rate in &[44_100u32, 48_000, 192_000, 705_600] {
            let fir = SubFir::new(rate);
            let fd = fir.rate_out(rate);
            let gain = |f: f64| {
                let w = 2.0 * std::f64::consts::PI * f / rate as f64;
                let (mut re, mut im) = (0.0, 0.0);
                for (i, &h) in fir.h.iter().enumerate() {
                    let t = i as f64 - fir.k as f64;
                    re += h * (w * t).cos();
                    im -= h * (w * t).sin();
                }
                20.0 * (re * re + im * im).sqrt().max(1e-300).log10()
            };
            for f in [0.0, 5.0, 12.0, 20.0, 40.0, 50.0] {
                assert!(gain(f).abs() < 0.001, "{rate}: {f} Hz {:.4} dB", gain(f));
            }
            for f in [fd - 50.0, fd - 8.0, fd + 20.0, 2.0 * fd, 1_000.0] {
                assert!(gain(f) < -120.0, "{rate}: {f:.1} Hz {:.1} dB", gain(f));
            }
        }
    }

    /// Decimated as it comes (any chunks, then the end) is the same as
    /// decimated whole, sample for sample.
    #[test]
    fn sub_decimator_streams_what_decimate_gives() {
        use rand::{Rng, SeedableRng};
        let rate = 48_000;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(11);
        let n = 100_003;
        let l: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect();
        let r: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect();
        let fir = SubFir::new(rate);
        let (wl, wr) = (fir.decimate(&l), fir.decimate(&r));
        let mut dec = SubDecimator::new(rate);
        let (mut sl, mut sr) = (Vec::new(), Vec::new());
        let mut at = 0;
        for step in [1usize, 777, 64, 5000, 3].iter().cycle() {
            if at >= n { break; }
            let e = (at + step).min(n);
            dec.push(&l[at..e], &r[at..e], |a, b| { sl.push(a); sr.push(b); });
            at = e;
        }
        dec.flush(|a, b| { sl.push(a); sr.push(b); });
        assert_eq!(sl, wl);
        assert_eq!(sr, wr);
        assert_eq!(dec.made() as usize, wl.len());
    }

    /// The trailing sub band of streamed columns reads 8 Hz at its level
    /// once its window has filled, in the band the tiles put it in.
    #[test]
    fn sub_trail_reads_8_hz_at_its_level() {
        let rate = 44_100;
        let x = tone(rate, 8.0, 0.5, rate as usize * 5);
        let step = spec_ln_step(rate);
        let mut trail = SubTrail::new(rate, SPEC_LOG_BINS, step);
        for c in x.chunks(4410) {
            trail.push(c, c);
        }
        let mut bands = vec![1e-30; SPEC_LOG_BINS];
        trail.blend_into(&mut bands);
        let peak = (0..SPEC_LOG_BINS).max_by(|&a, &b| bands[a].total_cmp(&bands[b])).unwrap();
        let peak_db = 10.0 * bands[peak].log10();
        assert!((peak as i64 - band_of(8.0, rate) as i64).abs() <= 2, "band {peak}");
        assert!((peak_db + 6.02).abs() < 1.0, "{peak_db:.2} dB");
        // Above the blend the column's own frame is untouched.
        assert_eq!(bands[band_of(100.0, rate)], 1e-30);
        trail.reset();
        let mut bands = vec![1e-30; SPEC_LOG_BINS];
        trail.blend_into(&mut bands);
        assert!(10.0 * bands[peak].log10() < -200.0, "reset: silence");
    }

    /// Pink noise (−3 dB/octave per bin): the step of the noise floor where
    /// the sub band takes over (its bins are 8× narrower than the long
    /// FFT's: −9 dB), measured on the tiles; the Whole spectrum (one Welch
    /// PSD of 65536 points) has no step there.
    #[test]
    fn sub_band_noise_floor_step_on_pink_noise() {
        use rand::{Rng, SeedableRng};
        let rate = 44_100;
        let n = rate as usize * 40;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(21);
        // Paul Kellet's pink filter (±0.05 dB above 9.2 Hz at 44.1 kHz).
        let mut b = [0.0f64; 7];
        let x: Vec<f64> = (0..n).map(|_| {
            let w: f64 = rng.gen_range(-1.0..1.0);
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.1538520;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let y = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
            b[6] = w * 0.115926;
            y * 0.05
        }).collect();
        // Mean of level + 10·log10(f) (flat for pink) over the bands in lo..hi.
        let eng = SpecEngine::new(rate, 4096, SPEC_LOG_BINS, spec_ln_step(rate));
        let sub = SubSignal::of(&x, &x, rate);
        let c0 = 4 * rate as usize / 4096;
        let cols = eng.columns(&SliceSource { l: &x, r: &x }, &sub.source(), c0, c0 + 200, &mut SpecScratch::default());
        let flat = |lo: f64, hi: f64| {
            let ks: Vec<usize> = (0..SPEC_LOG_BINS).filter(|&k| (lo..hi).contains(&band_centre(k, rate))).collect();
            let p: f64 = cols.iter().map(|c| ks.iter().map(|&k| 10f64.powf(db(c[k]) / 10.0) * band_centre(k, rate)).sum::<f64>()).sum();
            10.0 * (p / (cols.len() * ks.len()) as f64).log10()
        };
        let step = flat(10.0, 18.0) - flat(50.0, 90.0);
        println!("pink noise, tiles: sub band vs long FFT floor {step:.2} dB");
        assert!((step + 9.03).abs() < 1.5, "tiles: {step:.2} dB");
        let mut wa = WelchAccum::new(65_536, 32_768, 28.0);
        wa.push(&x, &x);
        let (pl, _, _) = wa.finish();
        let df = rate as f64 / 65_536.0;
        let whole = |lo: f64, hi: f64| {
            let (a, b) = ((lo / df) as usize, (hi / df) as usize);
            let p: f64 = (a..b).map(|i| 10f64.powf(pl[i] / 10.0) * i as f64 * df).sum();
            10.0 * (p / (b - a) as f64).log10()
        };
        let wstep = whole(10.0, 18.0) - whole(50.0, 90.0);
        println!("pink noise, Whole: {wstep:.2} dB");
        assert!(wstep.abs() < 1.0, "Whole: {wstep:.2} dB");
    }

    /// What the sub band costs a 4-minute 44.1 kHz track's tiles (one
    /// thread): the decimation, and the tiles with and without its frames.
    /// `cargo test --profile fast --bins sub_band_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sub_band_cost() {
        use rand::{Rng, SeedableRng};
        use std::time::Instant;
        let rate = 44_100;
        let n = rate as usize * 240;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        let l: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.3..0.3)).collect();
        let r: Vec<f64> = (0..n).map(|_| rng.gen_range(-0.3..0.3)).collect();
        let t = Instant::now();
        let sub = SubSignal::of(&l, &r, rate);
        let t_dec = t.elapsed().as_secs_f64();
        let hop = 256usize.max(n.div_ceil(16_384));
        let n_cols = n.div_ceil(hop);
        let mut eng = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
        let tiles = |eng: &SpecEngine| {
            let t = Instant::now();
            let mut s = SpecScratch::default();
            let src = SliceSource { l: &l, r: &r };
            let mut c = 0;
            while c < n_cols {
                let e = (c + 64).min(n_cols);
                std::hint::black_box(eng.columns(&src, &sub.source(), c, e, &mut s));
                c = e;
            }
            t.elapsed().as_secs_f64()
        };
        let with = tiles(&eng);
        eng.res.retain(|res| res.dec == 1);
        let without = tiles(&eng);
        println!("4 min at 44.1 kHz, hop {hop}: decimation {t_dec:.3} s (2 threads); tiles {with:.3} s with the sub band, {without:.3} s without (1 thread)");
    }

    #[test]
    fn log_bins_put_a_tone_in_its_band() {
        let rate = 44_100u32;
        let n = spec_fft_len(rate);
        assert_eq!(n, 4096);
        assert_eq!(spec_fft_len(96_000), 8192);
        assert_eq!(spec_fft_len(352_800), 32768);
        let stft = Stft::new(n, n, SPEC_BETA);
        let bins = LogBins::new(n, rate, SPEC_LOG_BINS, SPEC_F0_HZ);
        for &f in &[60.0f64, 1000.0, 15000.0] {
            let x: Vec<f64> = (0..n)
                .map(|i| 0.5 * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin())
                .collect();
            let mut p = Vec::new();
            stft.frame_power_lr(&x, &x, &mut p);
            let mut col = vec![0u8; SPEC_LOG_BINS];
            bins.to_u8(&p, -120.0, 120.0, &mut col);
            let peak = (0..SPEC_LOG_BINS).max_by_key(|&i| col[i]).unwrap();
            let span = (rate as f64 / 2.0 / SPEC_F0_HZ).ln();
            // Within one FFT bin (the lows can't do better than that) or
            // the band's own width (the highs).
            let fc = SPEC_F0_HZ * (span * (peak as f64 + 0.5) / SPEC_LOG_BINS as f64).exp();
            let tol = (rate as f64 / n as f64).max(fc * ((span / SPEC_LOG_BINS as f64).exp() - 1.0));
            assert!((fc - f).abs() <= tol, "{f} Hz: band {peak} at {fc:.1} Hz");
            // −6 dBFS sine: close to 0.95 × 255 in the lows, lower where a
            // band averages many bins.
            assert!(col[peak] > 180, "{f} Hz: level {}", col[peak]);
            // Far from the tone (a factor of 5.18 below it, 120 bands of
            // the 20 Hz grid before 1.6.2, checked from there up as then):
            // at the floor.
            let far = f / 5.18;
            assert!(far < 20.0 || col[band_of(far, rate)] < 10, "{f} Hz: {far:.1} Hz reads {}", col[band_of(far, rate)]);
        }
    }
}
