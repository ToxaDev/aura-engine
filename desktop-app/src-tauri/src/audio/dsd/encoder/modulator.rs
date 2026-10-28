//! The one-bit modulators: a CRFB loop filter of odd order N (5, 7 or 9; a
//! cascade of resonators with distributed feedback, Schreier & Temes)
//! realising a noise transfer function with zeros spread over the band and
//! a bounded out-of-band gain. Coefficients and the exact update order come
//! from `tools/dsd/design_sdm.py`; the input feeds every integrator with the
//! feedback's weight and the quantizer directly, so STF = 1 and the
//! integrators carry shaped noise only.
//!
//! [`LoopFilter`] and [`LoopState`] are the loop alone, so a look-ahead
//! search can run many copies of it; [`Sdm`] is the plain modulator on top.
//!
//! Stability guard: the input is limited to `INPUT_LIMIT` (PCM full scale is
//! 0.5 of full modulation; every set's out-of-band gain is the largest that
//! keeps its loop as stable as SDM5's, ~0.55–0.57), and if any state still
//! leaves `STATE_LIMIT` the loop restarts from zero. Both count as overloads,
//! once per episode.

use super::tables::{SdmCoefs, INPUT_LIMIT};

/// The states are scaled to peak at 1.0 for an input at the limit; an
/// unstable loop runs away to 10³ and beyond within a few hundred samples.
pub(crate) const STATE_LIMIT: f64 = 4.0;
/// SDM5's peak of the triangular dither added at the quantizer, of the
/// one-bit level (every set carries its own: `SdmCoefs::dither`). It is
/// noise-shaped like the quantization error and keeps the loop from settling
/// into a periodic pattern at silence or a low, steady input (an idle tone).
/// Measured on DSD64: 1/64 left a −159 dB spur 22 dB over the floor with a
/// −120 dBFS tone; 1/32 leaves none and costs 0.3 dB of silence noise; 1/8
/// cost 5 dB. Small DC inputs still leave tones with it (−144…−159 dB at
/// DSD64 to DSD512, `dsd_dither_study`); only 1/4 clears them, at ~8 dB of
/// noise and much of the loop's stability, so every set keeps 1/32.
pub(crate) const DITHER: f64 = 1.0 / 32.0;
/// Fixed seed, restored by `reset()`: two encoders built alike and fed the
/// same PCM give the same bytes (the DoP tests replay an encoder). Both
/// channels share it; the dither is far below the shaped noise either way.
pub(crate) const DITHER_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Triangular dither from 16-bit uniforms: one xorshift64 step per pair.
#[derive(Clone, Copy)]
pub(crate) struct Dither {
    rng: u64,
    scale: f64,
}

impl Dither {
    /// `peak` of the bit level (SDM5: `DITHER`).
    pub(crate) fn new(peak: f64) -> Self {
        Dither { rng: DITHER_SEED, scale: peak / 65_536.0 }
    }

    pub(crate) fn peak(&self) -> f64 {
        self.scale * 65_536.0
    }

    /// The dither for the next two samples, in time order.
    #[inline(always)]
    pub(crate) fn pair(&mut self) -> [f64; 2] {
        let scale = self.scale;
        let mut rng = self.rng;
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        self.rng = rng;
        let tri = |hi: u64, lo: u64| ((hi & 0xFFFF) as i32 - (lo & 0xFFFF) as i32) as f64 * scale;
        [tri(rng >> 48, rng >> 32), tri(rng >> 16, rng)]
    }
}

/// The coefficients of an order-N loop. `c` holds N − 1 entries and `g` one
/// per resonator, both padded with zeros to N.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoopFilter<const N: usize> {
    a: [f64; N],
    c: [f64; N],
    g: [f64; N],
    cout: f64,
    /// cout·(c[N−2]·a[N−2] + a[N−1]): how one sample's w = u − y reaches the
    /// next quantizer input through the last two states.
    q: f64,
}

/// Everything the loop remembers: Copy, so a search can keep many.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LoopState<const N: usize> {
    pub(crate) x: [f64; N],
    /// The next quantizer input before its own u and dither: cout·x[N−1],
    /// kept as the loop computes it.
    pub(crate) s: f64,
}

impl<const N: usize> Default for LoopState<N> {
    fn default() -> Self {
        LoopState { x: [0.0; N], s: 0.0 }
    }
}

impl<const N: usize> LoopFilter<N> {
    /// Resonators: x[2k+1], x[2k+2] for k < R.
    const R: usize = (N - 1) / 2;

    pub(crate) fn new(k: &SdmCoefs) -> Self {
        assert!(N % 2 == 1 && N >= 3, "odd orders only");
        assert_eq!(k.a.len(), N);
        assert_eq!(k.c.len(), N - 1);
        assert_eq!(k.g.len(), Self::R);
        let mut lf = LoopFilter { a: [0.0; N], c: [0.0; N], g: [0.0; N], cout: k.cout, q: 0.0 };
        lf.a.copy_from_slice(k.a);
        lf.c[..N - 1].copy_from_slice(k.c);
        lf.g[..Self::R].copy_from_slice(k.g);
        lf.q = k.cout * (lf.c[N - 2] * lf.a[N - 2] + lf.a[N - 1]);
        lf
    }

    /// The plain quantizer's bit for input `u` and dither `d` (the sum in
    /// SDM5's original order: s + (u + d), not v + d).
    #[inline(always)]
    pub(crate) fn decide(&self, st: &LoopState<N>, u: f64, d: f64) -> bool {
        st.s + (u + d) >= 0.0
    }

    /// Advances the loop by one sample with input `u` and output `bit`
    /// (true = +1). The sums run in SDM5's original order, so its bytes did
    /// not change when it moved onto this loop.
    #[inline(always)]
    pub(crate) fn step(&self, st: &mut LoopState<N>, u: f64, bit: bool) {
        let (a, c, g) = (&self.a, &self.c, &self.g);
        let x = &mut st.x;
        let (y, qy) = if bit { (1.0, self.q) } else { (-1.0, -self.q) };
        let w = u - y;
        // The next quantizer input, from the old states: off the chain from
        // one bit to the next but the final ∓ q.
        let r = Self::R - 1;
        let next = self.cout * (x[N - 1] + c[N - 2] * (x[N - 2] + c[N - 3] * x[N - 3] - g[r] * x[N - 1])) + self.q * u;
        st.s = next - qy;
        // Last resonator first: each one reads its neighbours' old states.
        let mut k = Self::R;
        while k > 0 {
            k -= 1;
            let i = 2 * k + 1;
            let n = x[i] + c[i - 1] * x[i - 1] + a[i] * w - g[k] * x[i + 1];
            if k == r {
                x[i + 1] += c[i] * n + a[i + 1] * w;
            } else {
                x[i + 1] = x[i + 1] + c[i] * n + a[i + 1] * w;
            }
            x[i] = n;
        }
        x[0] += a[0] * w;
    }

    /// The largest |state|; NaN if any state is NaN.
    #[inline(always)]
    pub(crate) fn peak(st: &LoopState<N>) -> f64 {
        let mut p = st.x[0].abs();
        for &v in &st.x[1..] {
            p = if v.abs() > p || v.is_nan() { v.abs() } else { p };
        }
        p
    }
}

/// The plain modulator: the loop filter, the dither, the input limit and the
/// restart guard.
pub(crate) struct Sdm<const N: usize> {
    lf: LoopFilter<N>,
    limit: f64,
    st: LoopState<N>,
    dither: Dither,
    limiting: bool,
    pub(crate) clamps: u64,
    pub(crate) resets: u64,
}

impl<const N: usize> Sdm<N> {
    pub(crate) fn new(k: &SdmCoefs) -> Self {
        Sdm::with_limit(k, INPUT_LIMIT)
    }

    /// For tests: a different input limit (above the stable range, to show
    /// the state guard).
    pub(crate) fn with_limit(k: &SdmCoefs, limit: f64) -> Self {
        let dither = Dither::new(k.dither);
        Sdm { lf: LoopFilter::new(k), limit, st: LoopState::default(), dither, limiting: false, clamps: 0, resets: 0 }
    }

    /// For tests: another dither peak (of the bit level).
    #[cfg(test)]
    pub(crate) fn dithered(mut self, peak: f64) -> Self {
        self.dither = Dither::new(peak);
        self
    }

    /// Modulates `u` (of full modulation, a multiple of 8 samples) into
    /// `out`, eight bits per byte, the earliest in the most significant bit.
    pub(crate) fn run(&mut self, u: &[f64], out: &mut [u8]) {
        debug_assert_eq!(u.len(), out.len() * 8);
        let lf = self.lf;
        let limit = self.limit;
        let mut st = self.st;
        let mut dither = self.dither;
        let mut limiting = self.limiting;
        let (mut clamps, mut resets) = (self.clamps, self.resets);
        for (byte_out, eight) in out.iter_mut().zip(u.chunks_exact(8)) {
            *byte_out = byte(&lf, &mut st, &mut dither, eight, limit, &mut limiting, &mut clamps);
            // Once per byte is plenty: a runaway takes hundreds of samples.
            if !(LoopFilter::peak(&st) <= STATE_LIMIT) {
                st = LoopState::default();
                resets += 1;
            }
        }
        self.st = st;
        self.dither = dither;
        self.limiting = limiting;
        (self.clamps, self.resets) = (clamps, resets);
    }

    pub(crate) fn reset(&mut self) {
        self.st = LoopState::default();
        self.dither = Dither::new(self.dither.peak());
        self.limiting = false;
    }

    pub(crate) fn overloads(&self) -> u64 {
        self.clamps + self.resets
    }

    #[cfg(test)]
    pub(crate) fn states(&self) -> [f64; N] {
        self.st.x
    }
}

/// Eight samples into one byte, MSB first.
#[inline(always)]
fn byte<const N: usize>(
    lf: &LoopFilter<N>,
    st: &mut LoopState<N>,
    dither: &mut Dither,
    eight: &[f64],
    limit: f64,
    limiting: &mut bool,
    clamps: &mut u64,
) -> u8 {
    let mut byte = 0u8;
    for pair in eight.chunks_exact(2) {
        let d = dither.pair();
        for k in 0..2 {
            let mut u = pair[k];
            if u.abs() > limit {
                if !*limiting {
                    *limiting = true;
                    *clamps += 1;
                }
                u = limit.copysign(u);
            } else {
                *limiting = false;
            }
            let bit = lf.decide(st, u, d[k]);
            lf.step(st, u, bit);
            byte = byte << 1 | bit as u8;
        }
    }
    byte
}
