//! Interpolation from the input PCM rate up to the DSD bit rate: a cascade of
//! ×2 equiripple half-band stages (`tables::HALF_BANDS`). The input is already
//! band-limited by the engine's long FIR, so each stage only has to keep the
//! base band flat and push its images below the modulator's shaped noise;
//! the faster the stage, the wider its transition band and the shorter it is
//! (3–9 taps per side; 48 only for an input at the base rate itself).
//!
//! Blocks of up to `BLOCK` output samples go through stage by stage, each
//! stage's history kept in front of its input so the filter loop runs over
//! one contiguous slice (and vectorises).

use super::tables::HALF_BANDS;

/// Output samples per block (at the bit rate).
pub(crate) const BLOCK: usize = 4096;

/// One ×2 stage. Of every output pair the first is the symmetric filtering
/// phase and the second the input itself (the half-band's centre tap).
struct HalfBand {
    /// The filtering phase's taps, from the centre outwards, gain 2 included.
    taps: &'static [f64],
    /// The last `2K − 1` inputs, then room for one block of input.
    buf: Vec<f64>,
}

impl HalfBand {
    fn new(taps: &'static [f64], max_in: usize) -> Self {
        HalfBand { taps, buf: vec![0.0; 2 * taps.len() - 1 + max_in] }
    }

    fn hist(&self) -> usize {
        2 * self.taps.len() - 1
    }

    /// Filters the `n` inputs already written after the history into `dst`
    /// (2n samples), then keeps the last inputs as the new history.
    fn run(&mut self, n: usize, dst: &mut [f64]) {
        let k = self.taps.len();
        let h = self.hist();
        let buf = &self.buf[..h + n];
        for (j, pair) in dst[..2 * n].chunks_exact_mut(2).enumerate() {
            // The window of the 2K inputs ending at input j, oldest first.
            let w = &buf[j..j + 2 * k];
            let (older, newer) = w.split_at(k);
            let mut acc = 0.0;
            for (t, &c) in self.taps.iter().enumerate() {
                acc += c * (newer[t] + older[k - 1 - t]);
            }
            pair[0] = acc;
            pair[1] = newer[0];
        }
        self.buf.copy_within(n..n + h, 0);
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
    }
}

/// The whole cascade for one channel: `ratio` output samples per input sample.
pub(crate) struct Interpolator {
    ratio: usize,
    stages: Vec<HalfBand>,
    /// The last stage's output (one block).
    out: Vec<f64>,
}

impl Interpolator {
    /// `ratio` is a power of two; `input_multiple` is the input rate over the
    /// family's base rate (44.1 or 48 kHz), possibly below 1.
    pub(crate) fn new(ratio: usize, input_multiple: f64) -> Self {
        let max_in = (BLOCK / ratio).max(1);
        let mut stages = Vec::new();
        let mut m = input_multiple;
        let mut n = max_in;
        let mut r = ratio;
        while r > 1 {
            stages.push(HalfBand::new(taps_for(m), n));
            m *= 2.0;
            n *= 2;
            r /= 2;
        }
        Interpolator { ratio, stages, out: vec![0.0; max_in * ratio] }
    }

    /// Input samples per block.
    pub(crate) fn block_inputs(&self) -> usize {
        self.out.len() / self.ratio
    }

    /// Interpolates up to `block_inputs()` samples, each passed through `f`
    /// on the way in; returns `input.len() * ratio` samples in time order.
    pub(crate) fn process(&mut self, input: &[f64], f: impl Fn(f64) -> f64) -> &[f64] {
        let n = input.len();
        debug_assert!(n <= self.block_inputs());
        if self.stages.is_empty() {
            for (o, &x) in self.out.iter_mut().zip(input) {
                *o = f(x);
            }
            return &self.out[..n];
        }
        let h0 = self.stages[0].hist();
        for (o, &x) in self.stages[0].buf[h0..h0 + n].iter_mut().zip(input) {
            *o = f(x);
        }
        let mut len = n;
        let last = self.stages.len() - 1;
        for s in 0..=last {
            if s < last {
                let (cur, rest) = self.stages.split_at_mut(s + 1);
                let next = &mut rest[0];
                let h = next.hist();
                cur[s].run(len, &mut next.buf[h..h + 2 * len]);
            } else {
                self.stages[s].run(len, &mut self.out[..2 * len]);
            }
            len *= 2;
        }
        &self.out[..len]
    }

    pub(crate) fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
    }

    /// Each stage's taps, first stage first (for tests).
    #[cfg(test)]
    pub(crate) fn taps_per_stage(&self) -> Vec<&'static [f64]> {
        self.stages.iter().map(|s| s.taps).collect()
    }
}

/// The stage design for an input at `m` × the base rate: the table entry with
/// the largest multiple not above `m` (the first one below the base rate).
fn taps_for(m: f64) -> &'static [f64] {
    let mut taps = HALF_BANDS[0].1;
    for &(mult, t) in HALF_BANDS.iter() {
        if mult as f64 <= m + 1e-9 {
            taps = t;
        }
    }
    taps
}
