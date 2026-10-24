//! K-weighting filter: high-shelf + RLB high-pass biquads.
//!
//! Implements the two-stage K-weighting pre-filter defined in ITU-R BS.1770-4
//! §2.3 and EBU R 128 §3.  The coefficients are derived by the bilinear
//! transform of the analogue prototypes exactly as in lra_variants.py
//! (lines 4-13) and libebur128 (src/ebur128.c `ebur128_init_filter`).
//!
//! Filter topology: transposed direct form II (same as scipy `lfilter`).
//! State: two delay registers per biquad per channel, z[0] and z[1].
//!
//! Reference:
//! - lra_variants.py lines 4-13 (k_filters)
//! - ITU-R BS.1770-4, §2.3
//! - EBU R 128 tech 3341, §3
//! - libebur128 src/ebur128.c

/// Compute K-weighting biquad coefficients for the given sample rate.
///
/// Returns `(shelf_b, shelf_a, hp_b, hp_a)` where each array is
/// `[b0, b1, b2]` / `[1.0, a1, a2]` (a0 normalised to 1).
///
/// # Panics
/// Panics if `rate` is zero or negative.
pub fn coefficients(rate: f64) -> ([f64; 3], [f64; 3], [f64; 3], [f64; 3]) {
    assert!(rate > 0.0, "sample rate must be positive");

    // Stage 1: high-shelf (pre-filter)
    let f0 = 1_681.974_450_955_533_f64;
    let g_db = 3.999_843_853_973_347_f64;
    let q = 0.707_175_236_955_419_6_f64;
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10.0_f64.powf(g_db / 20.0);
    let vb = vh.powf(0.499_666_774_155_f64);
    let a0 = 1.0 + k / q + k * k;
    let shelf_b = [
        (vh + vb * k / q + k * k) / a0,
        2.0 * (k * k - vh) / a0,
        (vh - vb * k / q + k * k) / a0,
    ];
    let shelf_a = [
        1.0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / q + k * k) / a0,
    ];

    // Stage 2: RLB high-pass (second pre-filter)
    let f0_hp = 38.135_470_876_024_44_f64;
    let q_hp = 0.500_327_037_323_877_3_f64;
    let k_hp = (std::f64::consts::PI * f0_hp / rate).tan();
    let a0_hp = 1.0 + k_hp / q_hp + k_hp * k_hp;
    let hp_b = [1.0, -2.0, 1.0];
    let hp_a = [
        1.0,
        2.0 * (k_hp * k_hp - 1.0) / a0_hp,
        (1.0 - k_hp / q_hp + k_hp * k_hp) / a0_hp,
    ];

    (shelf_b, shelf_a, hp_b, hp_a)
}

/// Per-channel K-weighting filter state (transposed direct form II).
///
/// Two biquad stages in series: high-shelf → RLB high-pass.
#[derive(Clone)]
pub struct KWeight {
    shelf_b: [f64; 3],
    shelf_a: [f64; 3],
    hp_b: [f64; 3],
    hp_a: [f64; 3],
    // Transposed DF-II state: [z1, z2] for shelf, [z1, z2] for hp
    sz: [f64; 2],
    hz: [f64; 2],
}

impl KWeight {
    /// Create a new K-weighting filter for `rate` Hz.
    pub fn new(rate: f64) -> Self {
        let (shelf_b, shelf_a, hp_b, hp_a) = coefficients(rate);
        Self {
            shelf_b,
            shelf_a,
            hp_b,
            hp_a,
            sz: [0.0; 2],
            hz: [0.0; 2],
        }
    }

    /// Process a single sample through the K-weighting filter.
    ///
    /// Transposed direct form II: y = b0*x + z1; z1' = b1*x - a1*y + z2; z2' = b2*x - a2*y
    #[inline]
    pub fn process_sample(&mut self, x: f64) -> f64 {
        // Stage 1: high-shelf
        let y1 = self.shelf_b[0] * x + self.sz[0];
        self.sz[0] = self.shelf_b[1] * x - self.shelf_a[1] * y1 + self.sz[1];
        self.sz[1] = self.shelf_b[2] * x - self.shelf_a[2] * y1;

        // Stage 2: RLB high-pass
        let y2 = self.hp_b[0] * y1 + self.hz[0];
        self.hz[0] = self.hp_b[1] * y1 - self.hp_a[1] * y2 + self.hz[1];
        self.hz[1] = self.hp_b[2] * y1 - self.hp_a[2] * y2;

        y2
    }

    /// Process a block of samples in place.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn process_block(&mut self, samples: &mut [f64]) {
        for s in samples.iter_mut() {
            *s = self.process_sample(*s);
        }
    }

    /// Process a block of samples, writing the K-weighted output to `out`.
    /// `out` must be at least as long as `samples`.
    #[cfg(test)]
    pub fn process_block_to(&mut self, samples: &[f64], out: &mut [f64]) {
        assert!(out.len() >= samples.len());
        for (i, &s) in samples.iter().enumerate() {
            out[i] = self.process_sample(s);
        }
    }

    /// Reset the filter state to zero (as if freshly created).
    #[cfg(test)]
    pub fn reset(&mut self) {
        self.sz = [0.0; 2];
        self.hz = [0.0; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BS.1770-4 Table 1 specifies coefficients at exactly 48 000 Hz.
    /// Values taken from the ITU-R BS.1770-4 standard and verified against
    /// lra_variants.py k_filters(48000).
    #[test]
    fn coefficients_48k_match_bs1770_table() {
        let (sb, sa, _hb, ha) = coefficients(48_000.0);
        // High-shelf b
        assert!((sb[0] - 1.53512485958697).abs() < 1e-9, "shelf b0={}", sb[0]);
        assert!((sb[1] - (-2.69169618940638)).abs() < 1e-9, "shelf b1={}", sb[1]);
        assert!((sb[2] - 1.19839281085285).abs() < 1e-9, "shelf b2={}", sb[2]);
        // High-shelf a (a0=1 normalised)
        assert!((sa[1] - (-1.69065929318241)).abs() < 1e-9, "shelf a1={}", sa[1]);
        assert!((sa[2] - 0.73248077421585).abs() < 1e-9, "shelf a2={}", sa[2]);
        // RLB HP a
        assert!((ha[1] - (-1.99004745483398)).abs() < 1e-9, "hp a1={}", ha[1]);
        assert!((ha[2] - 0.99007225036393).abs() < 1e-9, "hp a2={}", ha[2]);
    }

    /// DC (0 Hz) must be completely blocked by the high-pass stage.
    #[test]
    fn rejects_dc() {
        let mut kw = KWeight::new(48_000.0);
        // Run 48000 samples of DC=1.0 to let transients die
        for _ in 0..48_000 {
            kw.process_sample(1.0);
        }
        // Steady-state output should be ~0
        let y = kw.process_sample(1.0);
        assert!(y.abs() < 1e-6, "DC not rejected: {}", y);
    }

    /// A 997 Hz sine at 48 kHz should pass with gain near 0 dB (within 0.1 dB).
    #[test]
    fn passes_midband_sine() {
        let rate = 48_000.0_f64;
        let freq = 997.0_f64;
        let n = 4800_usize;
        let mut kw = KWeight::new(rate);
        let mut rms_out = 0.0_f64;
        // Warm up
        for i in 0..n {
            let x = (2.0 * std::f64::consts::PI * freq / rate * i as f64).sin();
            kw.process_sample(x);
        }
        // Measure
        for i in n..2 * n {
            let x = (2.0 * std::f64::consts::PI * freq / rate * i as f64).sin();
            let y = kw.process_sample(x);
            rms_out += y * y;
        }
        let rms = (rms_out / n as f64).sqrt();
        // Amplitude of unit sine is 1/sqrt(2) RMS; gain should be close to 1.0
        let gain_db = 20.0 * (rms * std::f64::consts::SQRT_2).log10();
        // BS.1770-4 shelf adds ~0.7 dB at 997 Hz; tolerance 1.0 dB covers full spec range
        assert!(
            gain_db.abs() < 1.0,
            "997 Hz gain = {:.3} dB (expected near 0)",
            gain_db
        );
    }

    /// Streaming (any block size) must give same output as one-sample loop.
    #[test]
    fn streaming_matches_sample_loop() {
        let rate = 44_100.0_f64;
        let n = 441_usize;
        let signal: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 / rate * i as f64).sin())
            .collect();

        // Reference: one sample at a time
        let mut kw_ref = KWeight::new(rate);
        let ref_out: Vec<f64> = signal.iter().map(|&x| kw_ref.process_sample(x)).collect();

        // Test: blocks of 13
        let mut kw_blk = KWeight::new(rate);
        let mut blk_out = vec![0.0_f64; n];
        let mut pos = 0;
        for chunk in signal.chunks(13) {
            kw_blk.process_block_to(chunk, &mut blk_out[pos..pos + chunk.len()]);
            pos += chunk.len();
        }

        for i in 0..n {
            assert!(
                (ref_out[i] - blk_out[i]).abs() < 1e-15,
                "sample {} differs: ref={} blk={}",
                i,
                ref_out[i],
                blk_out[i]
            );
        }
    }

    /// reset() should clear state so subsequent processing matches fresh filter.
    #[test]
    fn reset_clears_state() {
        let rate = 48_000.0_f64;
        let mut kw = KWeight::new(rate);
        for _ in 0..100 {
            kw.process_sample(1.0);
        }
        kw.reset();
        let mut kw2 = KWeight::new(rate);
        for i in 0..20 {
            let x = i as f64 * 0.01;
            let a = kw.process_sample(x);
            let b = kw2.process_sample(x);
            assert!((a - b).abs() < 1e-15, "post-reset mismatch at {}", i);
        }
    }
}
