//! Loudness measurement: momentary, short-term and integrated LUFS.
//!
//! Implements ITU-R BS.1770-4 / EBU R 128 loudness exactly as
//! `lra_variants.py` `subblock_power` / `integrated` / `rolling`
//! (lines 15-39).  All arithmetic is f64.
//!
//! # Sub-block structure
//!
//! The signal is K-weighted (see [`kweight`]), then squared-and-mean over
//! non-overlapping 100 ms sub-blocks (`sub = round(rate * 0.1)` samples).
//! The tail (< `sub` samples) is discarded, matching the Python's
//! `n // sub * sub` truncation.  Internally blocks are accumulated
//! one sample at a time so any push size works.
//!
//! # Windows
//!
//! | Window       | Sub-blocks | Hop        |
//! |-------------|-----------|------------|
//! | Momentary    | 4 (400 ms) | 1 (100 ms) |
//! | Short-term   | 30 (3 s)   | 1 (100 ms) |
//!
//! The first window is only available once `k` sub-blocks have been stored.
//!
//! # Integrated loudness (gated)
//!
//! 1. Absolute gate: discard 400 ms sub-block windows with summed power ≤ −70 LUFS.
//! 2. Relative gate: threshold = (mean energy of abs-gated windows) − 10 dB.
//! 3. Integrated LUFS = −0.691 + 10·log₁₀(energy mean of rel-gated windows).
//! Reference: lra_variants.py lines 34-39, ITU-R BS.1770-4 §2.5.
//!
//! Channel weights: 1.0 for L and R (standard for stereo, BS.1770 §2.2).
//!
//! # References
//!
//! - lra_variants.py lines 15-39
//! - ITU-R BS.1770-4, §§2.3–2.5
//! - EBU R 128 tech 3341

use super::kweight::KWeight;

/// The −0.691 dB offset defined in BS.1770-4 §2.5.
const LUFS_OFFSET: f64 = -0.691;

/// Convert mean squared energy to LUFS (−inf for zero energy).
#[inline]
fn energy_to_lufs(e: f64) -> f64 {
    if e <= 0.0 {
        f64::NEG_INFINITY
    } else {
        LUFS_OFFSET + 10.0 * e.log10()
    }
}

/// Integrated loudness and its relative gate over the 100 ms sub-block
/// powers `subs` (BS.1770-4 gated, two-stage gate) — what a meter fed the
/// same blocks reports (`LoudnessMeter::integrated_detail`). Both are NaN if
/// no data passes gating.
pub fn integrated_of(subs: &[f64]) -> (f64, f64) {
    let n = subs.len();
    if n < 4 {
        return (f64::NAN, f64::NAN);
    }
    // Form 400 ms windows (4 sub-blocks, hop 1) — matches rolling(P, 4)
    let num_windows = n - 4 + 1;
    let windows: Vec<f64> = (0..num_windows)
        .map(|i| {
            let sum: f64 = subs[i..i + 4].iter().sum();
            sum / 4.0
        })
        .collect();

    // Absolute gate: -70 LUFS
    let abs_gated: Vec<f64> = windows
        .iter()
        .copied()
        .filter(|&p| energy_to_lufs(p) > -70.0)
        .collect();

    if abs_gated.is_empty() {
        return (f64::NAN, f64::NAN);
    }

    // Relative gate: mean of abs-gated − 10 LU
    let mean_abs: f64 = abs_gated.iter().sum::<f64>() / abs_gated.len() as f64;
    let rel_gate = energy_to_lufs(mean_abs) - 10.0;

    // Relative-gated mean
    let rel_gated: Vec<f64> = abs_gated
        .iter()
        .copied()
        .filter(|&p| energy_to_lufs(p) > rel_gate)
        .collect();

    if rel_gated.is_empty() {
        return (f64::NAN, rel_gate);
    }

    let mean_rel: f64 = rel_gated.iter().sum::<f64>() / rel_gated.len() as f64;
    (energy_to_lufs(mean_rel), rel_gate)
}

/// The short-term loudness (3 s) of the 30 sub-blocks before `end`: one
/// entry of `LoudnessMeter::short_term_series`, f32 LUFS.
pub fn short_term_at(subs: &[f64], end: usize) -> f32 {
    let sum: f64 = subs[end - 30..end].iter().sum();
    energy_to_lufs(sum / 30.0) as f32
}

/// Running loudness meter.
///
/// Push stereo planar f64 samples in any block size; query momentary (400 ms),
/// short-term (3 s), integrated and their series at any time.
#[allow(dead_code)] // `rate` is kept with the meter it configured
pub struct LoudnessMeter {
    rate: f64,
    sub_len: usize, // samples per 100 ms sub-block

    // K-weighting filters, one per channel
    kw_l: KWeight,
    kw_r: KWeight,

    // Accumulator for the current sub-block
    acc_l: f64,
    acc_r: f64,
    acc_count: usize,

    // Ring buffer of sub-block summed powers (L+R, channel weight 1.0 each)
    // We keep enough for 30 sub-blocks (3 s short-term window).
    ring: Vec<f64>,    // capacity = 30 sub-blocks
    ring_head: usize,  // index of oldest entry
    ring_count: usize, // number of valid entries (0..=30)

    // All sub-block summed powers ever pushed (for integrated loudness gate)
    // We keep ALL momentary windows (4 sub-blocks, hop 1).
    // The integrated gate operates on them.
    // Note: Python's integrated() uses rolling(P, 4) — 4-sub-block windows.
    all_sub: Vec<f64>, // every sub-block summed power in order

    // Series caches: recomputed lazily if dirty
    frames_count: u64,

    // max_momentary / max_short_term trackers
    max_momentary: f64,
    max_short_term: f64,
}

impl LoudnessMeter {
    /// Create a new meter for the given sample rate.
    pub fn new(rate: f64) -> Self {
        assert!(rate >= 8_000.0 && rate <= 768_000.0, "rate out of range");
        let sub_len = (rate * 0.1).round() as usize;
        assert!(sub_len > 0);
        Self {
            rate,
            sub_len,
            kw_l: KWeight::new(rate),
            kw_r: KWeight::new(rate),
            acc_l: 0.0,
            acc_r: 0.0,
            acc_count: 0,
            ring: vec![0.0; 30],
            ring_head: 0,
            ring_count: 0,
            all_sub: Vec::new(),
            frames_count: 0,
            max_momentary: f64::NEG_INFINITY,
            max_short_term: f64::NEG_INFINITY,
        }
    }

    /// Number of PCM frames (per channel) pushed so far.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn frames_pushed(&self) -> u64 {
        self.frames_count
    }

    /// Push `l` and `r` sample slices (same length, planar).
    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        assert_eq!(l.len(), r.len(), "channel lengths must match");
        for (&ls, &rs) in l.iter().zip(r.iter()) {
            let yl = self.kw_l.process_sample(ls);
            let yr = self.kw_r.process_sample(rs);
            self.acc_l += yl * yl;
            self.acc_r += yr * yr;
            self.acc_count += 1;
            self.frames_count += 1;

            if self.acc_count == self.sub_len {
                self.flush_sub();
            }
        }
    }

    /// Flush a completed 100 ms sub-block into the ring.
    fn flush_sub(&mut self) {
        let mean_l = self.acc_l / self.sub_len as f64;
        let mean_r = self.acc_r / self.sub_len as f64;
        // Channel weight 1.0 each; summed power (BS.1770 §2.2 for stereo 1+1)
        let power = mean_l + mean_r;

        // Store in ring (cap 30)
        let idx = (self.ring_head + self.ring_count) % 30;
        if self.ring_count < 30 {
            self.ring[idx] = power;
            self.ring_count += 1;
        } else {
            // Ring is full: overwrite oldest
            self.ring[self.ring_head] = power;
            self.ring_head = (self.ring_head + 1) % 30;
        }

        // Store in all_sub for integrated gate
        self.all_sub.push(power);

        // Update max_momentary (need ≥ 4 sub-blocks)
        if self.ring_count >= 4 {
            let m = self.window_mean(4);
            let lufs = energy_to_lufs(m);
            if lufs > self.max_momentary {
                self.max_momentary = lufs;
            }
        }
        // Update max_short_term (need ≥ 30 sub-blocks)
        if self.ring_count >= 30 {
            let m = self.window_mean(30);
            let lufs = energy_to_lufs(m);
            if lufs > self.max_short_term {
                self.max_short_term = lufs;
            }
        }

        self.acc_l = 0.0;
        self.acc_r = 0.0;
        self.acc_count = 0;
    }

    /// Mean power of the most recent `k` sub-blocks from the ring.
    fn window_mean(&self, k: usize) -> f64 {
        assert!(k <= self.ring_count && k <= 30);
        let mut sum = 0.0;
        for i in 0..k {
            let idx = (self.ring_head + self.ring_count - k + i) % 30;
            sum += self.ring[idx];
        }
        sum / k as f64
    }

    /// Momentary loudness (last 400 ms = 4 sub-blocks), or NaN if < 4 blocks.
    ///
    /// Returns −∞ for silence, NaN if not enough data.
    pub fn momentary(&self) -> f64 {
        if self.ring_count < 4 {
            return f64::NAN;
        }
        energy_to_lufs(self.window_mean(4))
    }

    /// Short-term loudness (last 3 s = 30 sub-blocks), or NaN if < 30 blocks.
    pub fn short_term(&self) -> f64 {
        if self.ring_count < 30 {
            return f64::NAN;
        }
        energy_to_lufs(self.window_mean(30))
    }

    /// Integrated loudness (BS.1770-4 gated, two-stage gate).
    ///
    /// Returns NaN if no 400 ms window passes the absolute gate.
    pub fn integrated(&self) -> f64 {
        self.integrated_detail().0
    }

    /// Returns `(integrated_lufs, relative_gate_lufs)`.
    /// Both are NaN if no data passes gating.
    pub fn integrated_detail(&self) -> (f64, f64) {
        integrated_of(&self.all_sub)
    }

    /// Every 100 ms sub-block power pushed so far, in order.
    pub fn subs(&self) -> &[f64] {
        &self.all_sub
    }

    /// Maximum momentary loudness seen so far.
    pub fn max_momentary(&self) -> f64 {
        self.max_momentary
    }

    /// Maximum short-term loudness seen so far.
    pub fn max_short_term(&self) -> f64 {
        self.max_short_term
    }

    /// Momentary loudness series at 100 ms hop, f32 LUFS.
    ///
    /// Entry `i` is the momentary LUFS of window `[i, i+4)` sub-blocks.
    /// Returns f32::NEG_INFINITY for silence, f32::NAN if the window has < 4 blocks
    /// (only possible for the first few entries, which this method skips entirely —
    /// the first entry corresponds to sub-block index 3).
    pub fn momentary_series(&self) -> Vec<f32> {
        let n = self.all_sub.len();
        if n < 4 {
            return Vec::new();
        }
        (0..=(n - 4))
            .map(|i| {
                let sum: f64 = self.all_sub[i..i + 4].iter().sum();
                energy_to_lufs(sum / 4.0) as f32
            })
            .collect()
    }

    /// Short-term loudness series at 100 ms hop, f32 LUFS.
    ///
    /// Entry `i` is the short-term LUFS of window `[i, i+30)` sub-blocks.
    /// The first entry corresponds to sub-block index 29.
    pub fn short_term_series(&self) -> Vec<f32> {
        (30..=self.all_sub.len()).map(|end| short_term_at(&self.all_sub, end)).collect()
    }

    /// Reset to initial state (clears all history).
    #[cfg(test)]
    pub fn reset(&mut self) {
        self.kw_l.reset();
        self.kw_r.reset();
        self.acc_l = 0.0;
        self.acc_r = 0.0;
        self.acc_count = 0;
        self.ring = vec![0.0; 30];
        self.ring_head = 0;
        self.ring_count = 0;
        self.all_sub.clear();
        self.frames_count = 0;
        self.max_momentary = f64::NEG_INFINITY;
        self.max_short_term = f64::NEG_INFINITY;
    }

    /// Sample rate this meter was created for.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn rate(&self) -> f64 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::kweight::KWeight as KWeightFilter;
    use std::f64::consts::PI;

    /// Helper: generate a mono sine wave.
    fn sine(rate: f64, freq: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq / rate * i as f64).sin())
            .collect()
    }

    /// Compute the expected integrated LUFS for a sine of given amplitude at
    /// a given frequency, accounting for the actual K-weighting filter gain.
    ///
    /// The K-weighted steady-state RMS is measured directly by running the
    /// K-filter on a long warm-up section of the sine; the power from that
    /// measurement is the true expected power the meter will integrate.
    ///
    /// Same signal on both L and R channels → summed power = 2 × channel power.
    fn expected_integrated_lufs(rate: f64, freq: f64, amp: f64, n: usize) -> f64 {
        let mut kw = KWeightFilter::new(rate);
        let warmup = (rate * 0.5).round() as usize; // 500 ms warm-up

        // Warm-up: let filter settle
        for i in 0..warmup {
            let x = amp * (2.0 * PI * freq / rate * i as f64).sin();
            kw.process_sample(x);
        }

        // Measure steady-state power over `n` samples
        let mut sum_sq = 0.0_f64;
        for i in warmup..warmup + n {
            let x = amp * (2.0 * PI * freq / rate * i as f64).sin();
            let y = kw.process_sample(x);
            sum_sq += y * y;
        }
        let channel_power = sum_sq / n as f64;
        // L and R are the same signal → summed power = 2 * channel_power
        let total_power = 2.0 * channel_power;
        -0.691 + 10.0 * total_power.log10()
    }

    fn run_sine_test(rate: f64) {
        // Long enough for ≥ 30 gated 400 ms windows
        let duration_s = 10.0_f64.max(40.0 * 0.4); // at least 16 s
        let n = (rate * duration_s).round() as usize;
        let amp = 0.5_f64; // −6.02 dBFS, well above all gates

        let sig = sine(rate, 997.0, amp, n);

        let mut meter = LoudnessMeter::new(rate);
        meter.push(&sig, &sig);

        // Compute the analytically expected LUFS including K-filter gain
        // Use a 5-second measurement window for accuracy
        let meas_n = (rate * 5.0).round() as usize;
        let expected = expected_integrated_lufs(rate, 997.0, amp, meas_n);

        let got = meter.integrated();

        assert!(
            got.is_finite(),
            "integrated is not finite at {}Hz: {}",
            rate,
            got
        );
        assert!(
            (got - expected).abs() < 0.01,
            "rate={}: integrated={:.4} expected={:.4} diff={:.4}",
            rate,
            got,
            expected,
            got - expected
        );
    }

    #[test]
    fn integrated_sine_44100() {
        run_sine_test(44_100.0);
    }

    #[test]
    fn integrated_sine_48000() {
        run_sine_test(48_000.0);
    }

    #[test]
    fn integrated_sine_96000() {
        run_sine_test(96_000.0);
    }

    #[test]
    fn integrated_sine_192000() {
        run_sine_test(192_000.0);
    }

    #[test]
    fn integrated_sine_768000() {
        run_sine_test(768_000.0);
    }

    /// Streaming with random block sizes must match one-shot push.
    #[test]
    fn streaming_matches_oneshot() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let rate = 44_100.0_f64;
        let n = (rate * 3.5).round() as usize;
        let sig = sine(rate, 440.0, 0.3, n);

        // One-shot
        let mut m1 = LoudnessMeter::new(rate);
        m1.push(&sig, &sig);
        let i1 = m1.integrated();
        let ms1 = m1.momentary_series();
        let ss1 = m1.short_term_series();

        // Streaming with deterministic "random" sizes
        let mut m2 = LoudnessMeter::new(rate);
        let mut pos = 0;
        let mut h = DefaultHasher::new();
        while pos < n {
            1_u64.hash(&mut h);
            let chunk = ((h.finish() % 512) as usize + 1).min(n - pos);
            m2.push(&sig[pos..pos + chunk], &sig[pos..pos + chunk]);
            pos += chunk;
        }
        let i2 = m2.integrated();
        let ms2 = m2.momentary_series();
        let ss2 = m2.short_term_series();

        // Integrated must match exactly (same sub-blocks)
        let diff = (i1 - i2).abs();
        assert!(diff < 1e-10, "integrated mismatch: {} vs {}", i1, i2);
        assert_eq!(ms1.len(), ms2.len(), "momentary series length mismatch");
        assert_eq!(ss1.len(), ss2.len(), "short-term series length mismatch");
        for (a, b) in ms1.iter().zip(ms2.iter()) {
            assert!((a - b).abs() < 1e-5, "momentary series mismatch: {} vs {}", a, b);
        }
    }

    /// Absolute gate: silence should block gating and return NaN.
    #[test]
    fn absolute_gate_blocks_silence() {
        let rate = 48_000.0_f64;
        // 10 s of exact silence (power = 0 → LUFS = −∞ < −70)
        let n = (rate * 10.0) as usize;
        let silence = vec![0.0_f64; n];
        let mut m = LoudnessMeter::new(rate);
        m.push(&silence, &silence);
        assert!(m.integrated().is_nan(), "silence should give NaN integrated");
    }

    /// Series lengths match expected counts.
    #[test]
    fn series_lengths() {
        let rate = 48_000.0_f64;
        let sub = (rate * 0.1).round() as usize;
        // Push exactly 35 sub-blocks worth of samples
        let n = sub * 35;
        let sig = sine(rate, 440.0, 0.2, n);
        let mut m = LoudnessMeter::new(rate);
        m.push(&sig, &sig);
        // momentary: n_sub - 4 + 1 = 32
        assert_eq!(m.momentary_series().len(), 35 - 4 + 1, "momentary series");
        // short-term: n_sub - 30 + 1 = 6
        assert_eq!(m.short_term_series().len(), 35 - 30 + 1, "short-term series");
    }

    /// max_momentary and max_short_term must be >= current values.
    #[test]
    fn max_values_tracked() {
        let rate = 48_000.0_f64;
        let n = (rate * 5.0) as usize;
        let sig = sine(rate, 440.0, 0.5, n);
        let mut m = LoudnessMeter::new(rate);
        m.push(&sig, &sig);
        let mom = m.momentary();
        let st = m.short_term();
        // max must be >= current (may be higher from earlier)
        assert!(m.max_momentary() >= mom || mom.is_nan());
        assert!(m.max_short_term() >= st || st.is_nan());
    }

    /// Reset clears all state; subsequent push gives same result as fresh meter.
    #[test]
    fn reset_gives_fresh_results() {
        let rate = 48_000.0_f64;
        let n = (rate * 3.0) as usize;
        let sig = sine(rate, 1000.0, 0.3, n);

        let mut m = LoudnessMeter::new(rate);
        m.push(&sig, &sig);
        let i1 = m.integrated();

        m.reset();
        m.push(&sig, &sig);
        let i2 = m.integrated();

        assert!(
            (i1 - i2).abs() < 1e-10,
            "after reset: {} vs {}",
            i1,
            i2
        );
    }
}
