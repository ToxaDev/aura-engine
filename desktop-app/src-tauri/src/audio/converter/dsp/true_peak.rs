/// Target true-peak ceiling (linear). −0.5 dBTP gives consumer DACs and
/// downstream codec converters enough headroom to never inter-sample-clip,
/// which is the de-facto streaming standard. Applied only when the
/// measured true peak exceeds it; quieter material is left untouched.
pub(crate) const TARGET_TRUE_PEAK_DBTP: f64 = -0.5;

/// Linear target used by both the in-RAM normalizer and the streaming
/// (segmented) gain stage.
/// A ceiling in dBTP as a linear amplitude.
pub(crate) fn target_lin_for(target_dbtp: f64) -> f64 {
    10.0_f64.powf(target_dbtp / 20.0)
}

/// 4× Polyphase Sinc (Lanczos-4, 8-tap) coefficients, DC-normalized to 1.0.
fn lanczos4_poly_coeffs() -> [[f64; 8]; 3] {
    let mut poly_coeffs = [[0.0_f64; 8]; 3];
    for (p, phase) in (1..=3).enumerate() {
        let t = phase as f64 / 4.0; // 0.25, 0.50, 0.75
        let mut sum = 0.0;
        for k in 0..8 {
            let offset = k as f64 - 3.0; // offsets: -3, -2, -1, 0, 1, 2, 3, 4
            let x = t - offset;
            let val = if x.abs() < 1e-10 {
                1.0
            } else if x.abs() < 4.0 {
                let pi_x = std::f64::consts::PI * x;
                let pi_x_4 = std::f64::consts::PI * x / 4.0;
                (pi_x.sin() / pi_x) * (pi_x_4.sin() / pi_x_4)
            } else {
                0.0
            };
            poly_coeffs[p][k] = val;
            sum += val;
        }
        // Normalize DC gain to 1.0 to perfectly preserve peak amplitudes
        for k in 0..8 {
            poly_coeffs[p][k] /= sum;
        }
    }
    poly_coeffs
}

/// Measure the true peak (raw + 4× Lanczos-4 inter-sample) of a whole buffer.
pub fn measure_true_peak(samples_l: &[f64], samples_r: &[f64]) -> f64 {
    let n = samples_l.len();
    if n == 0 {
        return 0.0;
    }
    let poly_coeffs = lanczos4_poly_coeffs();

    // Helper: read sample with reflective edge padding so we never index
    // outside the valid range. This lets us interpolate over the full signal.
    let read = |buf: &[f64], idx: i64| -> f64 {
        if buf.is_empty() {
            return 0.0;
        }
        let last = (buf.len() as i64) - 1;
        let i = if idx < 0 {
            (-idx).min(last)
        } else if idx > last {
            (2 * last - idx).max(0)
        } else {
            idx
        };
        buf[i as usize]
    };

    let mut true_peak = 0.0_f64;
    for i in 0..n {
        let raw = samples_l[i].abs().max(samples_r[i].abs());
        if raw > true_peak {
            true_peak = raw;
        }
        for p in 0..3 {
            let mut il = 0.0;
            let mut ir = 0.0;
            for k in 0..8 {
                let idx = (i as i64) + (k as i64) - 3;
                let c = poly_coeffs[p][k];
                il += read(samples_l, idx) * c;
                ir += read(samples_r, idx) * c;
            }
            let peak = il.abs().max(ir.abs());
            if peak > true_peak {
                true_peak = peak;
            }
        }
    }
    true_peak
}

/// Streaming true-peak scanner for the segmented (bounded-RAM) pipeline.
/// Feeds arrive in time order; `finish()` returns the same value
/// `measure_true_peak` would return on the concatenated signal — the
/// scanner keeps an 8-sample window backlog and replicates the reflective
/// edge padding at the global start and end.
pub struct TruePeakScan {
    total: usize,
    coeffs: [[f64; 8]; 3],
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    base: usize, // global index of buf[0]
    next: usize, // next global index to evaluate
    peak: f64,
}

impl TruePeakScan {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            coeffs: lanczos4_poly_coeffs(),
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            base: 0,
            next: 0,
            peak: 0.0,
        }
    }

    #[inline]
    fn read(&self, buf: &[f64], idx: i64) -> f64 {
        // Same reflective mapping as measure_true_peak, in GLOBAL indices,
        // then translated into the backlog window.
        let last = (self.total as i64) - 1;
        let g = if idx < 0 {
            (-idx).min(last)
        } else if idx > last {
            (2 * last - idx).max(0)
        } else {
            idx
        } as usize;
        buf[g - self.base]
    }

    fn scan_ready(&mut self, frontier_known: usize, at_end: bool) {
        // Sample i needs indices i−3..=i+4. Without the end reflection we
        // can evaluate i only when i+4 < frontier_known; at the end the
        // reflection folds back inside the backlog.
        let limit = if at_end {
            self.total
        } else {
            frontier_known.saturating_sub(4)
        };
        while self.next < limit {
            let i = self.next as i64;
            let raw = self
                .read(&self.buf_l, i)
                .abs()
                .max(self.read(&self.buf_r, i).abs());
            if raw > self.peak {
                self.peak = raw;
            }
            for p in 0..3 {
                let mut il = 0.0;
                let mut ir = 0.0;
                for k in 0..8 {
                    let idx = i + (k as i64) - 3;
                    let c = self.coeffs[p][k];
                    il += self.read(&self.buf_l, idx) * c;
                    ir += self.read(&self.buf_r, idx) * c;
                }
                let peak = il.abs().max(ir.abs());
                if peak > self.peak {
                    self.peak = peak;
                }
            }
            self.next += 1;
        }
        // Drop everything older than next−3 (still needed for the window)
        // while keeping the tail that end-reflection may fold back into:
        // reflected indices reach down to 2*last − (total+... ) — i.e. at
        // most 4 below `last`, so keeping 8 samples of slack is plenty.
        let keep_from = self.next.saturating_sub(8);
        if keep_from > self.base {
            let drop = keep_from - self.base;
            self.buf_l.drain(..drop);
            self.buf_r.drain(..drop);
            self.base = keep_from;
        }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.buf_l.extend_from_slice(l);
        self.buf_r.extend_from_slice(r);
        let frontier = self.base + self.buf_l.len();
        self.scan_ready(frontier, false);
    }

    pub fn finish(mut self) -> f64 {
        self.scan_ready(self.total, true);
        self.peak
    }
}

/// Normalize signal peaks against inter-sample clipping.
///
/// Measures true peak via 4× Lanczos-4 polyphase sinc interpolation across
/// the ENTIRE signal (including the first 3 and last 5 samples — earlier
/// versions skipped them, which could miss inter-sample peaks at fade edges).
/// If the measured peak exceeds the −0.5 dBTP target, applies a single linear
/// gain so the loudest interpolated point lands exactly at the target.
///
/// The gain is RETURNED, not applied: the caller hands it to the dither
/// stage, which folds the multiply into its own pass. Applying it here meant
/// a separate full read-modify-write over the output buffer (2 × n × 8 bytes
/// read + written, hundreds of MB at 352.8 kHz) for one multiply per sample.
/// Returns 1.0 when the peak is already under target.
/// `target_dbtp` is the ceiling this file is normalised to.
///
/// It used to be the constant above for every file, and the Headroom control
/// set a gain at the other end of the chain instead — which this then
/// cancelled exactly, because an absolute ceiling does not care what the
/// signal was scaled by on the way in. Two conversions of one file at -3.0
/// and -0.5 came out at the same level, -0.50 dBFS and -15.69 LUFS both. The
/// control names this number now, which is what its label always said.
pub fn true_peak_normalization_gain(
    samples_l: &[f64],
    samples_r: &[f64],
    target_dbtp: f64,
) -> f64 {
    let n = samples_l.len();
    if n == 0 {
        return 1.0;
    }
    let true_peak = measure_true_peak(samples_l, samples_r);

    let true_peak_db = 20.0 * (true_peak + 1e-300).log10();
    let target_lin = target_lin_for(target_dbtp);
    crate::aelog!(
        "[CONV] Output True peak: {:.2} dBTP ({:.6})  target: {:.2} dBTP ({:.6})",
        true_peak_db, true_peak, target_dbtp, target_lin
    );

    // ── Normalize only if peak exceeds the target ──
    if true_peak > target_lin {
        let reduction = target_lin / true_peak;
        crate::aelog!(
            "[CONV] Peak exceeds {:.2} dBTP — normalizing: gain {:.6} ({:.2} dB)",
            target_dbtp,
            reduction,
            20.0 * reduction.log10()
        );
        reduction
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_lin() -> f64 {
        10.0_f64.powf(TARGET_TRUE_PEAK_DBTP / 20.0)
    }

    /// Measure + apply, the way production composes it (production folds the
    /// multiply into the dither pass instead of running it standalone; see
    /// `dither::DitherState::process`).
    fn apply_true_peak_normalization(l: &mut [f64], r: &mut [f64]) {
        let g = true_peak_normalization_gain(l, r, TARGET_TRUE_PEAK_DBTP);
        for s in l.iter_mut() {
            *s *= g;
        }
        for s in r.iter_mut() {
            *s *= g;
        }
    }

    #[test]
    fn quiet_signal_left_alone() {
        // Peak −6 dB: well below the −0.5 dBTP target → no gain change.
        let mut l: Vec<f64> = (0..1024).map(|i| 0.5 * ((i as f64) * 0.1).sin()).collect();
        let mut r = l.clone();
        let l0 = l.clone();
        apply_true_peak_normalization(&mut l, &mut r);
        // Bit-exact: no scaling should have occurred.
        for i in 0..l.len() {
            assert!((l[i] - l0[i]).abs() < 1e-15);
        }
    }

    #[test]
    fn loud_signal_brought_to_target() {
        // Build a signal whose peak is +0.3 dB above the target. After
        // normalisation, no sample should exceed target_lin (within 1 ULP).
        let mut l: Vec<f64> = (0..1024).map(|i| 1.05 * ((i as f64) * 0.07).sin()).collect();
        let mut r = l.clone();
        apply_true_peak_normalization(&mut l, &mut r);
        let post = l.iter().chain(r.iter()).fold(0.0_f64, |m, v| m.max(v.abs()));
        // Target compliance: at most ~1 LSB above the linear target. The
        // intersample peak detector is tighter than the raw sample max, so
        // raw samples will land slightly below the target — that's the
        // whole point of −0.5 dBTP headroom.
        assert!(
            post <= target_lin() + 1e-9,
            "post-normalisation peak {:.6} should not exceed target {:.6}",
            post,
            target_lin()
        );
    }

    #[test]
    fn empty_input_does_not_panic() {
        let mut l: Vec<f64> = vec![];
        let mut r: Vec<f64> = vec![];
        apply_true_peak_normalization(&mut l, &mut r);
        assert!(l.is_empty() && r.is_empty());
    }

    #[test]
    fn edge_samples_are_inspected() {
        // Place a near-target spike at sample 0. Old code (i in 3..n-5)
        // ignored sample 0 for the inter-sample interp pass, so a spike
        // at the very start could escape detection. With reflective edge
        // padding it must be seen, and the signal must be normalised.
        let mut l = vec![0.0_f64; 256];
        l[0] = 1.05;
        let mut r = l.clone();
        let l_before = l[0];
        apply_true_peak_normalization(&mut l, &mut r);
        // Either the gain dropped (we detected and limited) or, in the
        // worst case, the value rounded down very slightly. Either way
        // the post-value must be ≤ target.
        assert!(
            l[0].abs() <= target_lin() + 1e-9,
            "edge sample {} not limited (was {:.6}, now {:.6}, target {:.6})",
            0, l_before, l[0], target_lin()
        );
    }

    fn tone(n: usize, amp: f64) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / 48_000.0).sin())
            .collect()
    }

    /// The Headroom control names this number now, so it is the one that has
    /// to land. Before, it set a gain at the far end of the chain that this
    /// function then cancelled exactly, and every file came out at -0.5
    /// whatever the control said.
    #[test]
    fn the_ceiling_is_the_one_asked_for() {
        for target in [-0.5_f64, -1.0, -3.0, -6.0] {
            let mut l = tone(8192, 0.98);
            let mut r = l.clone();
            let g = true_peak_normalization_gain(&l, &r, target);
            for v in l.iter_mut().chain(r.iter_mut()) {
                *v *= g;
            }
            let after = 20.0 * measure_true_peak(&l, &r).log10();
            assert!(
                (after - target).abs() < 0.05,
                "asked for {:.1} dBTP, landed on {:.3}",
                target, after
            );
        }
    }

    /// Asking for room a file already has must not take level off it: the
    /// ceiling is a ceiling, never a target to pull quiet material up or down to.
    #[test]
    fn a_file_already_under_the_ceiling_is_untouched() {
        let l = tone(8192, 0.25); // about -12 dBFS
        let r = l.clone();
        assert_eq!(true_peak_normalization_gain(&l, &r, -3.0), 1.0);
    }

}
