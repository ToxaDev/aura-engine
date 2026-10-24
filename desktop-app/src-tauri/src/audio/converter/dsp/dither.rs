use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;

mod ns_high;

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
    /// Above 48 kHz: the rate's shaper from `ns_high`, zero-padded to
    /// `HIGH_ORDER` taps (a zero tap adds an exact 0.0). None = plain TPDF.
    high_coeffs: Option<[f64; HIGH_ORDER]>,
    high_hist_l: [f64; HIGH_ORDER],
    high_hist_r: [f64; HIGH_ORDER],
    /// The output grid's step: 1/2^23 for 24-bit, 1/2^15 for a 16-bit
    /// device. A power of two, so dividing by it is exact like the constant was.
    q: f64,
}

/// Wannamaker 9-order psychoacoustically optimal parameters (1992)
const NS_COEFFS: [f64; 9] = [2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847];
/// 24-bit output headroom: ±(1.0 / 2^23)
const Q_STEP: f64 = 1.0 / 8_388_608.0;
/// The grid step for `bits` (16 or 24; anything else is taken as 24).
fn q_step(bits: u32) -> f64 {
    if bits == 16 { 1.0 / 32_768.0 } else { Q_STEP }
}
/// Longest shaper in `ns_high::NS_HIGH`.
const HIGH_ORDER: usize = 16;

/// `AURA_NS_HIGH=0` turns the shaping above 48 kHz off (plain TPDF there, as
/// before it existed): the other side of an A/B.
fn high_shaping_allowed() -> bool {
    std::env::var("AURA_NS_HIGH").map_or(true, |v| v.trim() != "0")
}

fn high_coeffs(sample_rate: u32) -> Option<[f64; HIGH_ORDER]> {
    let (_, c) = ns_high::NS_HIGH.iter().find(|(sr, _)| *sr == sample_rate)?;
    let mut padded = [0.0; HIGH_ORDER];
    padded[..c.len()].copy_from_slice(c);
    Some(padded)
}

impl DitherState {
    pub fn new(sample_rate: u32) -> Self {
        Self::with_high_shaping(sample_rate, high_shaping_allowed())
    }

    /// `allow_high`: shape the noise above 48 kHz (where `ns_high` has the
    /// rate); false keeps plain TPDF there. At or below 48 kHz it changes nothing.
    pub fn with_high_shaping(sample_rate: u32, allow_high: bool) -> Self {
        Self::with_options(sample_rate, allow_high, 24)
    }

    /// For a device that takes `bits`-bit samples (16 or 24): the dither and
    /// the shaping land the output on that grid, so packing it is exact. A
    /// 16-bit device fed the 24-bit result would have it truncated undithered.
    pub fn for_bits(sample_rate: u32, bits: u32) -> Self {
        Self::with_options(sample_rate, high_shaping_allowed(), bits)
    }

    fn with_options(sample_rate: u32, allow_high: bool, bits: u32) -> Self {
        let bits = if bits == 16 { 16 } else { 24 };
        // Wannamaker-9 NS coefficients are tuned for 44.1/48 kHz psycho-acoustic
        // weighting; they stay at sr ≤ 48 kHz. Above that the band past 20 kHz
        // is wide enough to take the requantization noise out of 0..20 kHz with
        // a mild rise up there: each output rate has its own shaper (`ns_high`,
        // 13 dB less in-band noise at 88.2 kHz, 57-64 dB at 352.8 kHz and up).
        // A rate without one keeps pure TPDF.
        let use_noise_shaping = sample_rate <= 48000;
        let high_coeffs = if use_noise_shaping || !allow_high { None } else { high_coeffs(sample_rate) };
        if use_noise_shaping {
            crate::aelog!("[CONV] Applying {}-bit TPDF Dither + 9th-Order Noise Shaping (Wannamaker-1992)", bits);
        } else if high_coeffs.is_some() {
            crate::aelog!("[CONV] Applying {}-bit TPDF Dither + noise shaping out of 0-20 kHz (sr={})", bits, sample_rate);
        } else {
            crate::aelog!(
                "[CONV] Applying {}-bit pure TPDF Dither (Noise Shaping disabled for sr={} > 48 kHz)",
                bits,
                sample_rate
            );
        }
        let s = Self {
            // Two INDEPENDENT generators: drawing L and R noise from one sequential
            // stream leaves the channels' dither partially correlated, which images
            // the dither floor toward the phantom center. Independent streams are
            // the textbook requirement for stereo TPDF dither.
            rng_l: SmallRng::from_entropy(),
            rng_r: SmallRng::from_entropy(),
            err_hist_l: [0.0; 9],
            err_hist_r: [0.0; 9],
            use_noise_shaping,
            high_coeffs,
            high_hist_l: [0.0; HIGH_ORDER],
            high_hist_r: [0.0; HIGH_ORDER],
            q: q_step(bits),
        };
        // Test builds only: a whole-batch render (the A/B test) can pin the
        // sequence, so two renders that differ in one stage differ only there.
        #[cfg(test)]
        let s = match std::env::var("AURA_TEST_DITHER_SEED").ok().and_then(|v| v.parse::<u64>().ok()) {
            Some(seed) => Self::new_seeded_from(s, seed),
            None => s,
        };
        s
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
    /// Whether this state shapes the noise above 48 kHz (`ns_high`).
    #[cfg(test)]
    pub(crate) fn shapes_high(&self) -> bool {
        self.high_coeffs.is_some()
    }

    pub fn process(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        if self.use_noise_shaping {
            self.process_shaped(samples_l, samples_r, gain);
        } else if let Some(c) = self.high_coeffs {
            self.process_high(samples_l, samples_r, gain, &c);
        } else {
            self.process_plain(samples_l, samples_r, gain);
        }
    }

    /// TPDF dither + the rate's shaper above 48 kHz. Same loop as
    /// `process_shaped` (the error fed back is the unclamped one, the RNG
    /// order L,L,R,R), with `c` in place of the Wannamaker taps. The
    /// quantizer is the f64 grid, which cannot overload, so the error stays
    /// within 1.5 LSB and the FIR loop cannot run away.
    fn process_high(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64, c: &[f64; HIGH_ORDER]) {
        let (q, half_q) = (self.q, 0.5 * self.q);
        let peak_guard = 1.0 - q;
        for i in 0..samples_l.len() {
            let mut shaped_l = samples_l[i] * gain;
            let mut shaped_r = samples_r[i] * gain;

            for j in 0..HIGH_ORDER {
                shaped_l += self.high_hist_l[j] * c[j];
                shaped_r += self.high_hist_r[j] * c[j];
            }

            let dither_l =
                self.rng_l.gen_range(-half_q..half_q) + self.rng_l.gen_range(-half_q..half_q);
            let dither_r =
                self.rng_r.gen_range(-half_q..half_q) + self.rng_r.gen_range(-half_q..half_q);

            let quant_l = ((shaped_l + dither_l) / q).round() * q;
            let quant_r = ((shaped_r + dither_r) / q).round() * q;

            self.high_hist_l.copy_within(0..HIGH_ORDER - 1, 1);
            self.high_hist_r.copy_within(0..HIGH_ORDER - 1, 1);
            self.high_hist_l[0] = shaped_l - quant_l;
            self.high_hist_r[0] = shaped_r - quant_r;

            samples_l[i] = quant_l.clamp(-peak_guard, peak_guard);
            samples_r[i] = quant_r.clamp(-peak_guard, peak_guard);
        }
    }

    /// Pure TPDF dither, no error feedback (sr > 48 kHz without a shaper).
    ///
    /// The error history is deliberately not maintained here: it is only ever
    /// READ under `use_noise_shaping`, which is fixed at construction, so for
    /// a plain instance those writes were dead stores.
    fn process_plain(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        let (q, half_q) = (self.q, 0.5 * self.q);
        let peak_guard = 1.0 - q;
        for i in 0..samples_l.len() {
            // Two draws per channel, in L,L,R,R order — the RNG consumption
            // pattern is part of the output and must not change.
            let dither_l =
                self.rng_l.gen_range(-half_q..half_q) + self.rng_l.gen_range(-half_q..half_q);
            let dither_r =
                self.rng_r.gen_range(-half_q..half_q) + self.rng_r.gen_range(-half_q..half_q);

            let quant_l = ((samples_l[i] * gain + dither_l) / q).round() * q;
            let quant_r = ((samples_r[i] * gain + dither_r) / q).round() * q;

            samples_l[i] = quant_l.clamp(-peak_guard, peak_guard);
            samples_r[i] = quant_r.clamp(-peak_guard, peak_guard);
        }
    }

    /// TPDF dither + 9th-order Wannamaker noise shaping (sr ≤ 48 kHz).
    fn process_shaped(&mut self, samples_l: &mut [f64], samples_r: &mut [f64], gain: f64) {
        let (q, half_q) = (self.q, 0.5 * self.q);
        let peak_guard = 1.0 - q;
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

            let quant_l = ((shaped_l + dither_l) / q).round() * q;
            let quant_r = ((shaped_r + dither_r) / q).round() * q;

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
        Self::new_seeded_from(Self::new(sample_rate), seed)
    }

    fn new_seeded_from(mut s: Self, seed: u64) -> Self {
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

    /// Which branch a sample rate takes: Wannamaker at or below 48 kHz, the
    /// rate's own shaper above (plain TPDF with it turned off).
    #[test]
    fn noise_shaping_branch_by_rate() {
        for &(sr, wannamaker, high) in &[
            (44_100_u32, true, false),
            (48_000, true, false),
            (88_200, false, true),
            (352_800, false, true),
            (1_536_000, false, true),
        ] {
            let d = DitherState::with_high_shaping(sr, true);
            assert_eq!(d.use_noise_shaping, wannamaker, "sr={}", sr);
            assert_eq!(d.high_coeffs.is_some(), high, "sr={}", sr);
            assert!(DitherState::with_high_shaping(sr, false).high_coeffs.is_none(), "sr={}", sr);
        }
    }

    /// FNV-1a over the output bits of a seeded run, fed in uneven chunks so
    /// the state carry across `process` calls is part of what is hashed.
    fn golden_hash(mut d: DitherState) -> u64 {
        let mut l = ramp(20_000, 0.9);
        let mut r: Vec<f64> = ramp(20_000, 0.7).iter().map(|v| -v).collect();
        let mut at = 0;
        for n in [1, 999, 4096, 7_000, 7_904] {
            d.process(&mut l[at..at + n], &mut r[at..at + n], 0.813_742_9);
            at += n;
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for v in l.iter().chain(r.iter()) {
            for b in v.to_bits().to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
        h
    }

    fn seeded(sr: u32, allow_high: bool, seed: u64) -> DitherState {
        DitherState::new_seeded_from(DitherState::with_high_shaping(sr, allow_high), seed)
    }

    /// The shaping above 48 kHz must leave everything else as it was: these
    /// hashes were taken from the code before it existed (279b713). At or
    /// below 48 kHz the Wannamaker path, whatever `allow_high` says; above,
    /// with the shaping off, today's plain TPDF, bit for bit.
    #[test]
    fn untouched_paths_are_bit_identical_to_before_high_shaping() {
        for &sr in &[44_100_u32, 48_000] {
            for allow in [true, false] {
                assert_eq!(golden_hash(seeded(sr, allow, 0xC0FFEE)), 0xe632_7833_9272_77fa, "sr={sr} allow={allow}");
            }
        }
        for &sr in &[88_200_u32, 352_800, 1_536_000] {
            assert_eq!(golden_hash(seeded(sr, false, 0xC0FFEE)), 0xd15d_c5a3_c770_1bc2, "sr={sr}");
        }
        // A rate above 48 kHz without a shaper of its own stays plain too.
        assert!(seeded(64_000, true, 1).high_coeffs.is_none());
    }

    /// The plain branch drops the error-history bookkeeping entirely. That is
    /// only sound because the history is never read when shaping is off.
    #[test]
    fn plain_branch_leaves_error_history_untouched() {
        let mut d = seeded(352_800, false, 7);
        let mut l = ramp(512, 0.9);
        let mut r = ramp(512, 0.9);
        d.process(&mut l, &mut r, 1.0);
        assert_eq!(d.err_hist_l, [0.0; 9]);
        assert_eq!(d.err_hist_r, [0.0; 9]);
        assert_eq!(d.high_hist_l, [0.0; HIGH_ORDER]);
    }

    /// Every table row fits the padded loop, and the rates are the output
    /// rates above 48 kHz (×2..×32 of 44.1/48 kHz), each once.
    #[test]
    fn high_tables_cover_the_output_rates() {
        let mut rates: Vec<u32> = ns_high::NS_HIGH.iter().map(|(sr, c)| {
            assert!(!c.is_empty() && c.len() <= HIGH_ORDER, "sr={sr}");
            *sr
        }).collect();
        rates.sort();
        let mut want: Vec<u32> = [2, 4, 8, 16, 32].iter().flat_map(|m| [44_100 * m, 48_000 * m]).collect();
        want.sort();
        assert_eq!(rates, want);
    }

    /// Requantization noise (output minus the exact input) of a seeded run:
    /// mean power in 0..20 kHz and over the whole band, and the peak of the
    /// smoothed spectrum, each in dB against white noise of the same power
    /// as plain TPDF's (3 LSB²/12). Averaged periodograms, Hann window.
    fn noise_spectrum(sr: u32, allow_high: bool, level: f64) -> (f64, f64, f64) {
        use rustfft::{num_complex::Complex, FftPlanner};
        const N: usize = 1 << 15;
        const BLOCKS: usize = 24;
        let tone = |i: usize| level * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / sr as f64).sin();
        let x: Vec<f64> = (0..N * BLOCKS).map(tone).collect();
        let mut l = x.clone();
        let mut r = x.clone();
        seeded(sr, allow_high, 0x5EED).process(&mut l, &mut r, 1.0);
        let fft = FftPlanner::<f64>::new().plan_fft_forward(N);
        let hann: Vec<f64> = (0..N).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos()).collect();
        let wpow: f64 = hann.iter().map(|w| w * w).sum();
        let mut psd = vec![0.0; N / 2];
        for b in 0..BLOCKS {
            let mut buf: Vec<Complex<f64>> =
                (0..N).map(|i| Complex::new((l[b * N + i] - x[b * N + i]) / Q_STEP * hann[i], 0.0)).collect();
            fft.process(&mut buf);
            for k in 0..N / 2 {
                psd[k] += buf[k].norm_sqr() / wpow / BLOCKS as f64;
            }
        }
        // White noise of variance 0.25 LSB² (TPDF + rounding) reads 0.25 here.
        let db = |p: f64| 10.0 * (p / 0.25).log10();
        let band = (20_000.0 / sr as f64 * N as f64) as usize;
        let inband = psd[1..band].iter().sum::<f64>() / (band - 1) as f64;
        let total = psd[1..].iter().sum::<f64>() / (N / 2 - 1) as f64;
        let peak = psd.chunks(64).map(|c| c.iter().sum::<f64>() / c.len() as f64).fold(0.0, f64::max);
        (db(inband), db(total), db(peak))
    }

    /// At every rate with a shaper: far less noise in 0..20 kHz than plain
    /// TPDF, the total and the peak within the design bounds (+10 / +20 dB),
    /// near full scale too. Prints the table:
    /// `cargo test --profile fast --bins high_shaping_moves_noise -- --nocapture`
    #[test]
    fn high_shaping_moves_noise_out_of_the_audio_band() {
        for &(sr, _) in ns_high::NS_HIGH.iter() {
            let (p_in, p_tot, _) = noise_spectrum(sr, false, 0.5);
            for level in [0.5, 0.999] {
                let (s_in, s_tot, s_peak) = noise_spectrum(sr, true, level);
                println!(
                    "NS {sr:>7} Hz level {level}: in-band {s_in:7.1} dB (TPDF {p_in:5.1}), total {s_tot:+5.1} (TPDF {p_tot:+4.1}), peak {s_peak:+5.1}"
                );
                let want = if sr < 100_000 { 10.0 } else if sr < 300_000 { 25.0 } else { 45.0 };
                assert!(s_in < p_in - want, "sr={sr} level={level}: in-band {s_in:.1} vs TPDF {p_in:.1}");
                assert!(s_tot < 10.5 && s_peak < 20.5, "sr={sr} level={level}: total {s_tot:.1}, peak {s_peak:.1}");
            }
        }
    }

    /// A 16-bit device: every sample lands on the 16-bit grid (packing it is
    /// exact, nothing is truncated after the dither) within ±32767, and a
    /// tone of 0.3 LSB — which plain rounding would erase — comes through at
    /// its level: the dither keeps the quantizer linear. Above 48 kHz the
    /// rate's shaper still takes the noise out of 0..20 kHz on this grid.
    #[test]
    fn sixteen_bit_output_is_dithered_on_its_own_grid() {
        use rustfft::{num_complex::Complex, FftPlanner};
        let q = 1.0 / 32_768.0;
        for sr in [44_100, 48_000, 96_000, 705_600, 1_536_000] {
            const N: usize = 1 << 15;
            const BLOCKS: usize = 8;
            let amp = 0.3 * q;
            let w = 2.0 * std::f64::consts::PI * 997.0 / sr as f64;
            let x: Vec<f64> = (0..N * BLOCKS).map(|i| amp * (w * i as f64).sin()).collect();
            let run = |allow_high: bool| {
                let mut l = x.clone();
                let mut r = x.clone();
                DitherState::new_seeded_from(DitherState::with_options(sr, allow_high, 16), 0x16B1)
                    .process(&mut l, &mut r, 1.0);
                l
            };
            let l = run(true);
            for &v in &l {
                let k = v * 32_768.0;
                assert!(k == k.round() && k.abs() <= 32_767.0, "sr={sr}: {v} off the 16-bit grid");
            }
            // The tone's level in the output: projection on sin.
            let s: f64 = l.iter().enumerate().map(|(i, v)| v * (w * i as f64).sin()).sum::<f64>() * 2.0 / l.len() as f64;
            assert!((s / amp - 1.0).abs() < 0.1, "sr={sr}: a 0.3 LSB tone came out at {:.3} of its level", s / amp);
            if sr > 48_000 {
                let fft = FftPlanner::<f64>::new().plan_fft_forward(N);
                let band = (20_000.0 / sr as f64 * N as f64) as usize;
                let inband = |y: &[f64]| {
                    let mut p = 0.0;
                    for b in 0..BLOCKS {
                        let mut buf: Vec<Complex<f64>> =
                            (0..N).map(|i| Complex::new((y[b * N + i] - x[b * N + i]) / q, 0.0)).collect();
                        fft.process(&mut buf);
                        p += buf[1..band].iter().map(|c| c.norm_sqr()).sum::<f64>();
                    }
                    p
                };
                let gain_db = 10.0 * (inband(&l) / inband(&run(false))).log10();
                assert!(gain_db < -10.0, "sr={sr}: 16-bit in-band noise only {gain_db:.1} dB under plain TPDF");
            }
        }
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
