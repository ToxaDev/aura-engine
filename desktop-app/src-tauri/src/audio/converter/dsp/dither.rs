use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;

/// ── Dithering and 9th-order Noise Shaping (Wannamaker 1992) ──
/// Performs final 24-bit TPDF dithering before DAC/Encoder to prevent truncation distortion.
/// This runs strictly AT THE END to ensure 24-bit steps aren't broken by intermediary DSP scaling.
///
/// The state lives in a struct so the segmented (bounded-RAM) pipeline can
/// feed the SAME dither/noise-shaping chain chunk by chunk: the error
/// history and both RNG streams carry across calls exactly as they do
/// across loop iterations in the whole-buffer path.
pub struct DitherState {
    rng_l: SmallRng,
    rng_r: SmallRng,
    err_hist_l: [f64; 9],
    err_hist_r: [f64; 9],
    use_noise_shaping: bool,
}

/// Wannamaker 9-order psychoacoustically optimal parameters (1992)
const NS_COEFFS: [f64; 9] = [2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847];
/// 24-bit output headroom: ±(1.0 / 2^23)
const Q_STEP: f64 = 1.0 / 8_388_608.0;

impl DitherState {
    pub fn new(sample_rate: u32) -> Self {
        // Wannamaker-9 NS coefficients are tuned for 44.1/48 kHz psycho-acoustic
        // weighting. At 88.2/96 kHz the curve is no longer optimal (most of the
        // shaped noise sits below the audible band where it doesn't help) and at
        // higher rates it's actively pointless. So restrict to sr ≤ 48 kHz; for
        // everything above that just use pure TPDF dither — the quantization
        // noise floor is already inaudible because the entire signal lives below
        // Nyquist/2 anyway.
        let use_noise_shaping = sample_rate <= 48000;
        if use_noise_shaping {
            crate::aelog!("[CONV] Applying 24-bit TPDF Dither + 9th-Order Noise Shaping (Wannamaker-1992)");
        } else {
            crate::aelog!(
                "[CONV] Applying 24-bit pure TPDF Dither (Noise Shaping disabled for sr={} > 48 kHz)",
                sample_rate
            );
        }
        Self {
            // Two INDEPENDENT generators: drawing L and R noise from one sequential
            // stream leaves the channels' dither partially correlated, which images
            // the dither floor toward the phantom center. Independent streams are
            // the textbook requirement for stereo TPDF dither.
            rng_l: SmallRng::from_entropy(),
            rng_r: SmallRng::from_entropy(),
            err_hist_l: [0.0; 9],
            err_hist_r: [0.0; 9],
            use_noise_shaping,
        }
    }

    /// Dither + noise-shape one chunk in place. Chunks must arrive in
    /// time order; state carries across calls.
    ///
    /// `gain` is the true-peak normalization factor, folded in here instead
    /// of being applied by a separate pass over the buffer. `apply_true_peak_
    /// normalization` used to multiply the whole track in place and then this
    /// function read it straight back — a full read-modify-write over
    /// hundreds of MB for one multiply per sample. `x * 1.0` is exact in
    /// IEEE-754, so the un-normalized case is bit-identical to skipping it.
    ///
    /// Dispatch on `use_noise_shaping` happens ONCE per chunk rather than per
    /// sample: the branch used to sit inside the hot loop, where it blocked
    /// vectorization for the (much more common) sr > 48 kHz case.
    pub fn process(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        if self.use_noise_shaping {
            self.process_shaped(samples_l, samples_r, gain);
        } else {
            self.process_plain(samples_l, samples_r, gain);
        }
    }

    /// Pure TPDF dither, no error feedback (sr > 48 kHz).
    ///
    /// The error history is deliberately not maintained here: it is only ever
    /// READ under `use_noise_shaping`, which is fixed at construction, so for
    /// a plain instance those writes were dead stores.
    fn process_plain(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        let half_q = 0.5 * Q_STEP;
        let peak_guard = 1.0 - Q_STEP;
        for i in 0..samples_l.len() {
            // Two draws per channel, in L,L,R,R order — the RNG consumption
            // pattern is part of the output and must not change.
            let dither_l =
                self.rng_l.gen_range(-half_q..half_q) + self.rng_l.gen_range(-half_q..half_q);
            let dither_r =
                self.rng_r.gen_range(-half_q..half_q) + self.rng_r.gen_range(-half_q..half_q);

            let quant_l = ((samples_l[i] * gain + dither_l) / Q_STEP).round() * Q_STEP;
            let quant_r = ((samples_r[i] * gain + dither_r) / Q_STEP).round() * Q_STEP;

            samples_l[i] = quant_l.clamp(-peak_guard, peak_guard);
            samples_r[i] = quant_r.clamp(-peak_guard, peak_guard);
        }
    }

    /// TPDF dither + 9th-order Wannamaker noise shaping (sr ≤ 48 kHz).
    fn process_shaped(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        let half_q = 0.5 * Q_STEP;
        let peak_guard = 1.0 - Q_STEP;
        for i in 0..samples_l.len() {
            let mut shaped_l = samples_l[i] * gain;
            let mut shaped_r = samples_r[i] * gain;

            for j in 0..9 {
                shaped_l += self.err_hist_l[j] * NS_COEFFS[j];
                shaped_r += self.err_hist_r[j] * NS_COEFFS[j];
            }

            let dither_l =
                self.rng_l.gen_range(-half_q..half_q) + self.rng_l.gen_range(-half_q..half_q);
            let dither_r =
                self.rng_r.gen_range(-half_q..half_q) + self.rng_r.gen_range(-half_q..half_q);

            let quant_l = ((shaped_l + dither_l) / Q_STEP).round() * Q_STEP;
            let quant_r = ((shaped_r + dither_r) / Q_STEP).round() * Q_STEP;

            // Error feedback uses the UNCLAMPED quantized value — clamping inside
            // the loop would inject the clip error into the shaper and destabilize
            // it. The output itself is clamped below so dither/NS excursions can
            // never push a sample past the largest representable 24-bit code
            // (ffmpeg's f64→s32 handling of >1.0 values is build-dependent).
            //
            // copy_within is the same right-shift the old reverse loop did
            // (h[8]←h[7] … h[1]←h[0], dropping h[8]), as one 64-byte move.
            self.err_hist_l.copy_within(0..8, 1);
            self.err_hist_r.copy_within(0..8, 1);
            self.err_hist_l[0] = shaped_l - quant_l;
            self.err_hist_r[0] = shaped_r - quant_r;

            samples_l[i] = quant_l.clamp(-peak_guard, peak_guard);
            samples_r[i] = quant_r.clamp(-peak_guard, peak_guard);
        }
    }
}

#[cfg(test)]
impl DitherState {
    /// Deterministic constructor for equivalence tests. Production always
    /// seeds from entropy — two runs must not share a dither sequence.
    fn new_seeded(sample_rate: u32, seed: u64) -> Self {
        let mut s = Self::new(sample_rate);
        s.rng_l = SmallRng::seed_from_u64(seed);
        s.rng_r = SmallRng::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize, k: f64) -> Vec<f64> {
        (0..n).map(|i| k * ((i as f64) * 0.017).sin()).collect()
    }

    /// The true-peak gain used to be applied by its own pass over the whole
    /// buffer, then this stage read the scaled samples back. Folding the
    /// multiply into the dither loop must not move a single bit — for BOTH
    /// the noise-shaped (sr ≤ 48 k) and plain (sr > 48 k) branches, and for
    /// the gain == 1.0 case that used to skip the pass entirely.
    #[test]
    fn folded_gain_is_bit_identical_to_a_separate_gain_pass() {
        for &sr in &[44_100_u32, 352_800] {
            for &gain in &[1.0_f64, 0.813_742_9, 0.5] {
                let l0 = ramp(4096, 0.9);
                let r0 = ramp(4096, 0.7);

                // A: scale in a separate pass, then dither with no gain.
                let mut la: Vec<f64> = l0.iter().map(|v| v * gain).collect();
                let mut ra: Vec<f64> = r0.iter().map(|v| v * gain).collect();
                DitherState::new_seeded(sr, 0xC0FFEE).process(&mut la, &mut ra, 1.0);

                // B: fold the gain into the dither pass (production).
                let mut lb = l0.clone();
                let mut rb = r0.clone();
                DitherState::new_seeded(sr, 0xC0FFEE).process(&mut lb, &mut rb, gain);

                for i in 0..la.len() {
                    assert_eq!(
                        la[i].to_bits(),
                        lb[i].to_bits(),
                        "L[{}] differs at sr={} gain={}",
                        i, sr, gain
                    );
                    assert_eq!(
                        ra[i].to_bits(),
                        rb[i].to_bits(),
                        "R[{}] differs at sr={} gain={}",
                        i, sr, gain
                    );
                }
            }
        }
    }

    /// Splitting `process` into shaped/plain variants must not change which
    /// branch a given sample rate takes.
    #[test]
    fn noise_shaping_engages_only_at_or_below_48k() {
        for &(sr, want) in &[
            (44_100_u32, true),
            (48_000, true),
            (88_200, false),
            (352_800, false),
        ] {
            assert_eq!(DitherState::new(sr).use_noise_shaping, want, "sr={}", sr);
        }
    }

    /// The plain branch drops the error-history bookkeeping entirely. That is
    /// only sound because the history is never read when shaping is off.
    #[test]
    fn plain_branch_leaves_error_history_untouched() {
        let mut d = DitherState::new_seeded(352_800, 7);
        let mut l = ramp(512, 0.9);
        let mut r = ramp(512, 0.9);
        d.process(&mut l, &mut r, 1.0);
        assert_eq!(d.err_hist_l, [0.0; 9]);
        assert_eq!(d.err_hist_r, [0.0; 9]);
    }
}

/// Whole-buffer wrapper — the in-RAM path's entry point. `gain` comes from
/// `true_peak::true_peak_normalization_gain` and is applied inside the dither
/// loop rather than by a separate pass.
pub fn apply_dithering_and_noise_shaping(
    samples_l: &mut [f64],
    samples_r: &mut [f64],
    sample_rate: u32,
    gain: f64,
) {
    if samples_l.is_empty() {
        return;
    }
    DitherState::new(sample_rate).process(samples_l, samples_r, gain);
}
