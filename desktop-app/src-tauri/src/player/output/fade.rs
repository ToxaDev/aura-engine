//! The output's fade: one envelope for every point where the sound stops or
//! starts again — a seek, a rack change, ⏭, a pause and its resume, Stop, a
//! new stream, the render running dry (Anton 28.10: the sound broke off and
//! came back at full level, a thump each time). Planned stops fade out over
//! `FADE_MS` of the music still on its way; what starts mid-track fades in
//! over the same. A track's own beginning gets only a short ramp
//! (`SHORT_MS`), so its attack is kept; between the switches the envelope is
//! exactly 1 and leaves the samples untouched (BIT-PERFECT stays exact).

/// The length of a fade, out or in.
pub const FADE_MS: f64 = 250.0;

/// The longest ramp a track's own start gets (a period of the device, if
/// that is shorter): no click, its attack kept.
pub const SHORT_MS: f64 = 10.0;

/// `FADE_MS` in frames at `rate`.
pub fn fade_frames(rate: u32) -> usize {
    ((FADE_MS / 1000.0 * rate as f64).round() as usize).max(1)
}

/// `SHORT_MS` in frames at `rate`, a period `avail` at most.
pub fn short_frames(rate: u32, avail: usize) -> usize {
    ((SHORT_MS / 1000.0 * rate as f64).round() as usize).min(avail).max(1)
}

/// The gain at `x` of the way from silence (0) to full (1): a raised cosine,
/// flat at both ends, so neither the start nor the end of a fade clicks.
pub fn shape(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    0.5 - 0.5 * (std::f64::consts::PI * x).cos()
}

/// Where the gain stands (`x`, 0 silent … 1 full, through `shape`) and where
/// it goes: `step` a sample, up (fade-in), down (fade-out) or 0 (still). A
/// fade turned round midway goes back from where it stands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    x: f64,
    step: f64,
}

impl Envelope {
    /// Full and still: the music as it is.
    pub fn full() -> Self {
        Self { x: 1.0, step: 0.0 }
    }

    /// Silent and still.
    pub fn silent() -> Self {
        Self { x: 0.0, step: 0.0 }
    }

    pub fn is_full(&self) -> bool {
        self.x >= 1.0 && self.step >= 0.0
    }

    /// Nothing more to hear: silent, and not on its way up.
    pub fn is_silent(&self) -> bool {
        self.x <= 0.0 && self.step <= 0.0
    }

    pub fn fading_out(&self) -> bool {
        self.step < 0.0 && self.x > 0.0
    }

    pub fn fading_in(&self) -> bool {
        self.step > 0.0 && self.x < 1.0
    }

    /// Down to silence over `frames` (a whole fade's pace, from where it
    /// stands: a fade-in turned round halfway takes half of them).
    pub fn fade_out(&mut self, frames: usize) {
        if self.x > 0.0 {
            self.step = -1.0 / frames.max(1) as f64;
        }
    }

    /// Up to full over `frames`, at the same pace.
    pub fn fade_in(&mut self, frames: usize) {
        if self.x < 1.0 {
            self.step = 1.0 / frames.max(1) as f64;
        }
    }

    /// A fade-out that has to be over within `frames` (the music it may
    /// read ends there): steeper if it would run longer; none left, silent
    /// at once.
    pub fn end_within(&mut self, frames: usize) {
        if frames == 0 {
            self.cut();
        } else if self.fading_out() && self.x / -self.step > frames as f64 {
            self.step = -self.x / frames as f64;
        }
    }

    /// Silent at once (the music ran out: nothing is left to fade).
    pub fn cut(&mut self) {
        *self = Self::silent();
    }

    /// Full at once (a DSD file as it is: its words go out whole).
    pub fn set_full(&mut self) {
        *self = Self::full();
    }

    /// The envelope over a period, sample by sample. Full and still, the
    /// samples are not touched; silent and still, they are zeroed.
    pub fn apply(&mut self, l: &mut [f64], r: &mut [f64]) {
        if self.step == 0.0 {
            if self.x >= 1.0 {
                return;
            }
            if self.x <= 0.0 {
                l.fill(0.0);
                r.fill(0.0);
                return;
            }
        }
        // Within half a step of an end is the end: the sum of the steps
        // does not land on it exactly.
        let snap = 0.5 * self.step.abs();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            self.x += self.step;
            if self.x >= 1.0 - snap {
                self.x = 1.0;
            } else if self.x <= snap {
                self.x = 0.0;
            }
            let g = shape(self.x);
            *a *= g;
            *b *= g;
        }
        if self.x >= 1.0 || self.x <= 0.0 {
            self.step = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(e: &mut Envelope, n: usize) -> Vec<f64> {
        let mut l = vec![1.0; n];
        let mut r = vec![1.0; n];
        e.apply(&mut l, &mut r);
        assert_eq!(l, r);
        l
    }

    #[test]
    fn a_fade_out_runs_smoothly_to_silence_in_its_frames() {
        let n = fade_frames(44_100);
        assert_eq!(n, 11_025);
        let mut e = Envelope::full();
        e.fade_out(n);
        assert!(e.fading_out());
        let g = run(&mut e, n);
        // Monotone down, starting just under full and ending at 0.
        assert!(g.windows(2).all(|w| w[1] <= w[0]));
        assert!(g[0] > 0.999_999 && g[0] < 1.0);
        assert_eq!(*g.last().unwrap(), 0.0);
        assert!(e.is_silent() && !e.fading_out());
        // No step larger than the raised cosine's steepest (π/2n).
        let max_step = g.windows(2).map(|w| w[0] - w[1]).fold(0.0, f64::max);
        assert!(max_step <= std::f64::consts::FRAC_PI_2 / n as f64 + 1e-12);
        // Halfway it is at half.
        assert!((g[n / 2 - 1] - 0.5).abs() < 1e-3);
        // Silent and still: zeros after.
        assert!(run(&mut e, 16).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn a_fade_in_rises_from_silence_to_full_in_its_frames() {
        let n = fade_frames(96_000);
        let mut e = Envelope::silent();
        e.fade_in(n);
        let g = run(&mut e, n + 10);
        assert!(g.windows(2).all(|w| w[1] >= w[0]));
        assert!(g[0] > 0.0 && g[0] < 1e-6);
        assert_eq!(g[n - 1], 1.0);
        assert!(g[n..].iter().all(|&v| v == 1.0));
        assert!(e.is_full() && !e.fading_in());
    }

    #[test]
    fn a_fade_turned_round_goes_back_from_where_it_stands() {
        let n = 1000;
        let mut e = Envelope::full();
        e.fade_out(n);
        let down = run(&mut e, 300);
        e.fade_in(n);
        let up = run(&mut e, 300);
        // No jump at the turn: the first sample up is next to the last down.
        assert!((up[0] - down[299]).abs() < 0.01);
        assert!(e.is_full());
        assert_eq!(*up.last().unwrap(), 1.0);
        // A fade-out asked of silence, or a fade-in of full, does nothing.
        let mut s = Envelope::silent();
        s.fade_out(n);
        assert!(s.is_silent() && !s.fading_out());
        let mut f = Envelope::full();
        f.fade_in(n);
        assert_eq!(f, Envelope::full());
    }

    /// Between the switches the samples go out bit for bit as they came:
    /// no multiply at all, not even by 1.0.
    #[test]
    fn full_and_still_the_samples_are_bit_identical() {
        let src: Vec<f64> = (0..4096).map(|i| ((i as f64 * 0.37).sin() * 0.9) + f64::MIN_POSITIVE * i as f64).collect();
        let mut l = src.clone();
        let mut r: Vec<f64> = src.iter().map(|v| -v).collect();
        let mut e = Envelope::full();
        e.apply(&mut l, &mut r);
        assert!(l.iter().zip(&src).all(|(a, b)| a.to_bits() == b.to_bits()));
        assert!(r.iter().zip(&src).all(|(a, b)| a.to_bits() == (-b).to_bits()));
        // And after a fade-in has finished.
        let mut e = Envelope::silent();
        e.fade_in(10);
        run(&mut e, 10);
        let mut l2 = src.clone();
        let mut r2 = src.clone();
        e.apply(&mut l2, &mut r2);
        assert!(l2.iter().zip(&src).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn the_short_ramp_is_ten_ms_or_a_period() {
        assert_eq!(short_frames(44_100, 441 * 4), 441);
        assert_eq!(short_frames(705_600, 100), 100);
        assert_eq!(short_frames(44_100, 0), 1);
        assert_eq!(fade_frames(705_600), 176_400);
        assert_eq!(shape(0.0), 0.0);
        assert_eq!(shape(1.0), 1.0);
    }
}
