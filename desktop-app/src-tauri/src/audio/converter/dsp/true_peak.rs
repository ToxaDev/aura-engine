/// Target true-peak ceiling (linear). −0.5 dBTP gives consumer DACs and
/// downstream codec converters enough headroom to never inter-sample-clip,
/// which is the de-facto streaming standard. Applied only when the
/// measured true peak exceeds it; quieter material is left untouched.
const TARGET_TRUE_PEAK_DBTP: f64 = -0.5;

/// Normalize signal peaks against inter-sample clipping.
///
/// Measures true peak via 4× Lanczos-4 polyphase sinc interpolation across
/// the ENTIRE signal (including the first 3 and last 5 samples — earlier
/// versions skipped them, which could miss inter-sample peaks at fade edges).
/// If the measured peak exceeds the −0.5 dBTP target, applies a single linear
/// gain so the loudest interpolated point lands exactly at the target.
pub fn apply_true_peak_normalization(samples_l: &mut [f64], samples_r: &mut [f64]) {
    let n = samples_l.len();
    if n == 0 {
        return;
    }

    // ── Precompute 4× Polyphase Sinc (Lanczos-4, 8-tap) coefficients ──
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

    // ── Phase 1: scan all samples for raw and inter-sample peaks ──
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

    let true_peak_db = 20.0 * (true_peak + 1e-300).log10();
    let target_lin = 10.0_f64.powf(TARGET_TRUE_PEAK_DBTP / 20.0);
    crate::aelog!(
        "[CONV] Output True peak: {:.2} dBTP ({:.6})  target: {:.2} dBTP ({:.6})",
        true_peak_db, true_peak, TARGET_TRUE_PEAK_DBTP, target_lin
    );

    // ── Normalize only if peak exceeds the target ──
    if true_peak > target_lin {
        let reduction = target_lin / true_peak;
        crate::aelog!(
            "[CONV] Peak exceeds {:.2} dBTP — normalizing: gain {:.6} ({:.2} dB)",
            TARGET_TRUE_PEAK_DBTP,
            reduction,
            20.0 * reduction.log10()
        );
        for s in samples_l.iter_mut() {
            *s *= reduction;
        }
        for s in samples_r.iter_mut() {
            *s *= reduction;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_lin() -> f64 {
        10.0_f64.powf(TARGET_TRUE_PEAK_DBTP / 20.0)
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
}
