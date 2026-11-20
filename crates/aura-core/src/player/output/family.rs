//! A 48 kHz family chain sent as the 44.1 kHz family's DSD rate (issue #8):
//! the output takes the timeline's rate down by 147/160 — 384 kHz to
//! 352.8 kHz for DSD128, 768 kHz to 705.6 kHz for DSD256 and up — on the
//! audio thread, just before the modulators. The chain stays as it is: its
//! long FIR does the source's reconstruction at the 48 kHz family's rate,
//! and what reaches this step holds nothing above a quarter of that rate
//! (the source's band; 96 kHz at most for a 192 kHz source at ×8 — and it
//! is music only up to 48 kHz for a 96 kHz one), so a short polyphase
//! filter (32 taps a phase) is all it takes: flat over that band, its
//! images and aliases more than 170 dB down.
//!
//! Many DACs (a TEAC UD-501 among them) take DSD at 2.8, 5.6 and 11.2 MHz
//! only: a 6.144 MHz DoP stream reached them as 384 kHz PCM, the DoP words
//! heard as quiet noise.

/// Output frames for every `DOWN` input frames.
pub(crate) const UP: usize = 147;
pub(crate) const DOWN: usize = 160;
/// Taps of each phase.
const K: usize = 32;
/// The Kaiser window's beta: stopband near −170 dB.
const BETA: f64 = 18.0;

/// The device rate a 48 kHz family timeline at `rate` goes out at, None
/// for any other rate.
pub(crate) fn device_rate(rate: u32) -> Option<u32> {
    (rate % 48_000 == 0 && rate > 0).then(|| rate / DOWN as u32 * UP as u32)
}

/// `frames` of the timeline's rate in device frames (rounded down).
pub(crate) fn to_device(frames: u64) -> u64 {
    frames * UP as u64 / DOWN as u64
}

/// `frames` of the device's rate in timeline frames (rounded down).
pub(crate) fn to_timeline(frames: u64) -> u64 {
    frames * DOWN as u64 / UP as u64
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The phases' taps: phase `p`, tap `j` at `p * K + j`, each phase in the
/// order of its inputs (oldest first), so a phase runs as one dot product
/// over the last `K` inputs.
fn design() -> Vec<f64> {
    let n = UP * K;
    let c = (n - 1) as f64 / 2.0;
    // The cut-off half way across the band left free: the output's
    // Nyquist, 147/320 of the input rate (in the upsampled rate's cycles).
    let fc = 1.0 / (2.0 * DOWN as f64);
    let i0b = bessel_i0(BETA);
    let mut proto = vec![0.0; n];
    for (t, h) in proto.iter_mut().enumerate() {
        let x = t as f64 - c;
        let sinc = if x == 0.0 { 1.0 } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * 2.0 * fc * x) };
        let r = x / c;
        let w = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        *h = UP as f64 * 2.0 * fc * sinc * w;
    }
    // Each phase to unity at DC exactly: no ripple from one output frame
    // to the next at 147 kHz's pattern.
    let mut taps = vec![0.0; n];
    for p in 0..UP {
        let mut sum = 0.0;
        for j in 0..K {
            // y[m] = sum_j h[p + UP j] x[i - j]: tap j takes the input j back.
            taps[p * K + (K - 1 - j)] = proto[p + UP * j];
            sum += proto[p + UP * j];
        }
        for j in 0..K {
            taps[p * K + j] /= sum;
        }
    }
    taps
}

/// The step on the audio thread: the timeline's frames in, the device's
/// out, its history carried across periods. Sized once (`new`); nothing
/// is allocated per period.
pub(crate) struct FamilyDown {
    taps: Vec<f64>,
    /// The last `K − 1` inputs, then the period's new ones.
    l: Vec<f64>,
    r: Vec<f64>,
    /// Inputs in `l`/`r` now.
    held: usize,
    /// Inputs dropped from the front so far: input `q` sits at
    /// `q + K − 1 − base`.
    base: u64,
    /// Output frames made, and input frames taken, since the start.
    made: u64,
    taken: u64,
}

impl FamilyDown {
    /// For periods of up to `max_out` device frames.
    pub(crate) fn new(max_out: usize) -> FamilyDown {
        let cap = K + Self::most_in(max_out) + 2;
        FamilyDown {
            taps: design(),
            l: vec![0.0; cap],
            r: vec![0.0; cap],
            // The history starts as silence: inputs −(K−1)..−1.
            held: K - 1,
            base: 0,
            made: 0,
            taken: 0,
        }
    }

    /// The most inputs `n` outputs can take.
    pub(crate) fn most_in(n: usize) -> usize {
        n * DOWN / UP + 2
    }

    /// The input frame output `m` ends on.
    fn last_in(m: u64) -> u64 {
        m * DOWN as u64 / UP as u64
    }

    /// The timeline frames `n` more outputs need.
    pub(crate) fn need(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (Self::last_in(self.made + n as u64 - 1) + 1).saturating_sub(self.taken) as usize
    }

    /// The group delay, in device frames (half the filter).
    pub(crate) fn delay_frames() -> usize {
        to_device((K / 2) as u64) as usize
    }

    /// `got` interleaved input frames in, up to `n` device frames out
    /// (interleaved, into `out`). Returns how many it made: every one whose
    /// last input is in.
    pub(crate) fn run(&mut self, input: &[f64], got: usize, out: &mut [f64], n: usize) -> usize {
        for i in 0..got {
            self.l[self.held + i] = input[2 * i];
            self.r[self.held + i] = input[2 * i + 1];
        }
        self.held += got;
        self.taken += got as u64;
        let mut made = 0;
        while made < n {
            let m = self.made;
            let i = Self::last_in(m);
            if i >= self.taken {
                break;
            }
            let p = (m * DOWN as u64 - i * UP as u64) as usize;
            // Input i sits at l[i − base + K − 1]; its K − 1 before it.
            let end = (i - self.base) as usize + K;
            let h = &self.taps[p * K..p * K + K];
            let (xl, xr) = (&self.l[end - K..end], &self.r[end - K..end]);
            let (mut yl, mut yr) = (0.0, 0.0);
            for j in 0..K {
                yl += h[j] * xl[j];
                yr += h[j] * xr[j];
            }
            out[2 * made] = yl;
            out[2 * made + 1] = yr;
            self.made += 1;
            made += 1;
        }
        // Keep the K − 1 inputs before the next output's last: input q sits
        // at q + K − 1 − base, so the first kept goes to 0.
        let keep_from = Self::last_in(self.made).min(self.taken);
        let drop = (keep_from.saturating_sub(self.base) as usize).min(self.held);
        if drop > 0 {
            self.l.copy_within(drop..self.held, 0);
            self.r.copy_within(drop..self.held, 0);
            self.held -= drop;
            self.base += drop as u64;
        }
        made
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_tone(rate_in: f64, f: f64, secs: f64, period: usize) -> Vec<f64> {
        let n_in = (rate_in * secs) as usize;
        let x: Vec<f64> = (0..n_in).map(|i| 0.5 * (2.0 * std::f64::consts::PI * f * i as f64 / rate_in).sin()).collect();
        let mut fd = FamilyDown::new(period);
        let mut out = Vec::new();
        let mut buf = vec![0.0; period * 2];
        let mut fed = 0;
        loop {
            let need = fd.need(period);
            let got = need.min(n_in - fed);
            let inter: Vec<f64> = x[fed..fed + got].iter().flat_map(|&v| [v, -v]).collect();
            fed += got;
            let made = fd.run(&inter, got, &mut buf, period);
            for k in 0..made {
                assert_eq!(buf[2 * k + 1], -buf[2 * k], "the channels as they came");
                out.push(buf[2 * k]);
            }
            if got < need {
                break;
            }
        }
        out
    }

    /// The residual against the ideal tone at the output rate, over the
    /// middle (the filter's delay taken off), in dB under the tone.
    fn residual_db(rate_in: f64, f: f64, period: usize) -> f64 {
        let out = run_tone(rate_in, f, 0.2, period);
        let rate_out = rate_in * UP as f64 / DOWN as f64;
        // The delay: (UP K − 1)/2 upsampled samples.
        let delay_in = (UP * K - 1) as f64 / 2.0 / UP as f64;
        let (mut err, mut sig) = (0.0f64, 0.0f64);
        for m in out.len() / 4..out.len() * 3 / 4 {
            let t_in = m as f64 * rate_in / rate_out - delay_in;
            let want = 0.5 * (2.0 * std::f64::consts::PI * f * t_in / rate_in).sin();
            err += (out[m] - want).powi(2);
            sig += want * want;
        }
        10.0 * (err / sig).log10()
    }

    #[test]
    fn the_rates_go_down_by_147_160() {
        assert_eq!(device_rate(384_000), Some(352_800));
        assert_eq!(device_rate(192_000), Some(176_400));
        assert_eq!(device_rate(768_000), Some(705_600));
        assert_eq!(device_rate(352_800), None);
        assert_eq!(to_device(160), 147);
        assert_eq!(to_timeline(147), 160);
    }

    /// Tones across the band a 96 kHz and a 192 kHz source can hold come
    /// through at the output rate as they were, whatever the period size:
    /// the residual (aliases, images, ripple) far below the 24-bit floor.
    #[test]
    fn tones_in_the_band_come_through_clean() {
        for &(f, period) in &[(1_000.0, 512), (19_997.0, 3_840), (47_000.0, 1_000), (90_000.0, 441)] {
            let db = residual_db(384_000.0, f, period);
            assert!(db < -150.0, "{f} Hz, period {period}: residual {db:.1} dB");
        }
    }

    /// Periods of any size join seamlessly: the output is the same as one
    /// long run's.
    #[test]
    fn periods_join_seamlessly() {
        let a = run_tone(384_000.0, 12_345.0, 0.05, 4_096);
        let b = run_tone(384_000.0, 12_345.0, 0.05, 37);
        let n = a.len().min(b.len());
        assert!(n > 15_000);
        for m in 0..n {
            assert!((a[m] - b[m]).abs() < 1e-12, "frame {m}");
        }
    }

    /// 160 inputs make 147 outputs, exactly, over any stretch.
    #[test]
    fn it_takes_160_for_147() {
        let mut fd = FamilyDown::new(1_470);
        let need = fd.need(1_470);
        assert_eq!(need, 1_599, "output 1469 ends on input 1598");
        let input = vec![0.0; need * 2];
        let mut out = vec![0.0; 1_470 * 2];
        assert_eq!(fd.run(&input, need, &mut out, 1_470), 1_470);
        assert_eq!(fd.need(147), 160);
    }
}
