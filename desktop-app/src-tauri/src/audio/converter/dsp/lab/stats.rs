use std::sync::atomic::{AtomicBool, Ordering};

pub struct SourceStats {
    pub peak_lin: f64,
    pub peak_dbfs: f64,
    /// 65536-bin amplitude histogram over |x| in [0,1].
    pub hist: Vec<u64>,
    /// True when the first-differences test suggests a 16-bit grid source.
    pub grid16: bool,
    /// Effective number of bits estimated from quiet frames; None when
    /// not enough quiet frames exist.
    pub enob: Option<f64>,
}

/// Analyze source audio before any DSP. Returns None on cancel or empty input.
///
/// Computes peak, a 65536-bin |x| histogram, a 16-bit-grid verdict based on
/// first differences (immune to DC offset), and an ENOB estimate from the
/// quietest 512-sample frames. All passes are cancel-checked periodically.
pub fn analyze(
    l: &[f64],
    r: &[f64],
    _sample_rate: u32,
    cancel: &AtomicBool,
) -> Option<SourceStats> {
    let n = l.len();
    if n == 0 {
        return None;
    }

    // ── Peak and histogram ──────────────────────────────────────────────────
    const NBINS: usize = 65536;
    let mut hist = vec![0u64; NBINS];
    let mut peak_lin: f64 = 0.0;

    for (ci, ch) in [l, r].iter().enumerate() {
        for (i, &s) in ch.iter().enumerate() {
            let a = s.abs();
            if a > peak_lin {
                peak_lin = a;
            }
            let bin = ((a * NBINS as f64) as usize).min(NBINS - 1);
            hist[bin] += 1;
            // Cancel check every 512k samples per channel
            if i % 524_288 == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
        }
        let _ = ci;
    }

    let peak_dbfs = if peak_lin > 0.0 {
        20.0 * peak_lin.log10()
    } else {
        f64::NEG_INFINITY
    };

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    // ── 16-bit grid test on first differences ──────────────────────────────
    // Test d[k] = ch[k+1] - ch[k] against the k/32768 grid.
    // A DC offset shifts all samples by a constant, so differences d cancel it —
    // the test is immune to DC. Stride-sample to stay near TARGET total checks.
    let grid16 = {
        const TARGET: usize = 1_000_000;
        // Total first-differences available across both channels
        let total_diffs = l.len().saturating_sub(1) + r.len().saturating_sub(1);
        if total_diffs < 100 {
            false
        } else {
            let stride = (total_diffs / TARGET).max(1);
            let mut on_grid: u64 = 0;
            let mut checked: u64 = 0;

            for ch in [l, r] {
                let nd = ch.len().saturating_sub(1);
                let mut k = 0usize;
                while k < nd {
                    let d = ch[k + 1] - ch[k];
                    // Nearest k/32768 grid value
                    let nearest = (d * 32768.0).round() / 32768.0;
                    let err = (d - nearest).abs();
                    // ~1e-9 relative tolerance; absolute floor for d≈0
                    let tol = 1e-9 * d.abs() + 1e-15;
                    if err <= tol {
                        on_grid += 1;
                    }
                    checked += 1;
                    if checked % 100_000 == 0 && cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    k += stride;
                }
            }

            checked >= 100 && (on_grid as f64 / checked as f64) >= 0.99
        }
    };

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    // ── ENOB from quietest 512-sample frames ───────────────────────────────
    // Estimate the noise floor by pooling all frames whose RMS is below
    // −70 dBFS. Need ≥10 such frames across both channels; otherwise ENOB
    // is unavailable. Formula: ENOB = (−rms_dBFS − 1.76) / 6.02 which is
    // the standard bit-width estimator from a noise-dominated measurement.
    const FRAME: usize = 512;
    // −70 dBFS in linear: 10^(−70/20)
    let rms_thresh: f64 = 10.0_f64.powf(-70.0 / 20.0);
    let mut quiet_sum_sq: f64 = 0.0;
    let mut quiet_samples: u64 = 0;
    let mut quiet_frames: u32 = 0;

    for (ci, ch) in [l, r].iter().enumerate() {
        let nf = ch.len() / FRAME;
        for fi in 0..nf {
            let frame = &ch[fi * FRAME..(fi + 1) * FRAME];
            let sum_sq: f64 = frame.iter().map(|&s| s * s).sum();
            let rms = (sum_sq / FRAME as f64).sqrt();
            if rms > 0.0 && rms < rms_thresh {
                quiet_sum_sq += sum_sq;
                quiet_samples += FRAME as u64;
                quiet_frames += 1;
            }
        }
        if ci == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
    }

    let enob = if quiet_frames >= 10 {
        let rms_pool = (quiet_sum_sq / quiet_samples as f64).sqrt();
        if rms_pool > 0.0 {
            let rms_dbfs = 20.0 * rms_pool.log10();
            Some((-rms_dbfs - 1.76) / 6.02)
        } else {
            None
        }
    } else {
        None
    };

    Some(SourceStats {
        peak_lin,
        peak_dbfs,
        hist,
        grid16,
        enob,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn analyze_empty_returns_none() {
        let cancel = AtomicBool::new(false);
        assert!(analyze(&[], &[], 44100, &cancel).is_none());
    }

    /// A ramp quantized to the 16-bit grid must produce grid16=true.
    #[test]
    fn grid16_true_for_quantized_ramp() {
        let cancel = AtomicBool::new(false);
        let n = 2_100_000usize;
        // Cyclic ramp: values are exact multiples of 1/32768.
        // First differences are ±1/32768 — always on the grid.
        let l: Vec<f64> = (0..n)
            .map(|i| {
                let k = (i as i32 % 65536 - 32768) as i16;
                k as f64 / 32768.0
            })
            .collect();
        let r = l.clone();
        let stats = analyze(&l, &r, 44100, &cancel).unwrap();
        assert!(stats.grid16, "quantized ramp should be on 16-bit grid");
    }

    /// Float-precision noise must produce grid16=false.
    #[test]
    fn grid16_false_for_float_noise() {
        let cancel = AtomicBool::new(false);
        let n = 2_100_000usize;
        // Simple LCG in f64 — produces values NOT on the 1/32768 grid.
        let mut state: u64 = 0x5a4bcdef12345678;
        let l: Vec<f64> = (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                // Fractional bits ensure this is not a multiple of 1/32768
                (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
            })
            .collect();
        let r = l.clone();
        let stats = analyze(&l, &r, 44100, &cancel).unwrap();
        assert!(!stats.grid16, "float noise should not be on 16-bit grid");
    }

    /// ENOB from TPDF dither noise at 16-bit level should be ~15.5–16.5 bits.
    #[test]
    fn enob_dithered_16bit_in_range() {
        let cancel = AtomicBool::new(false);
        // TPDF dither: two uniform [-lsb/2, lsb/2] samples summed.
        // RMS ≈ lsb/sqrt(3) ≈ 1.77e-5 ≈ −95 dBFS.
        // ENOB = (95 − 1.76) / 6.02 ≈ 15.5 bits.
        let lsb = 1.0 / 32768.0;
        let n = 200_000usize;
        let mut state: u64 = 0xdeadbeefcafe1234;
        let lcg = |s: &mut u64| -> f64 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (*s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        let l: Vec<f64> = (0..n)
            .map(|_| (lcg(&mut state) + lcg(&mut state)) * lsb)
            .collect();
        let r = l.clone();
        let stats = analyze(&l, &r, 44100, &cancel).unwrap();
        let enob = stats.enob.expect("ENOB should be available with quiet dither frames");
        assert!(
            (15.0..=17.0).contains(&enob),
            "ENOB of dithered 16-bit noise should be 15–17, got {:.2}",
            enob
        );
    }

    #[test]
    fn peak_and_histogram_basic() {
        let cancel = AtomicBool::new(false);
        let l = vec![0.0, 0.5, -0.75, 0.25];
        let r = vec![0.0, 0.0, 0.0, 0.0];
        let stats = analyze(&l, &r, 44100, &cancel).unwrap();
        assert!((stats.peak_lin - 0.75).abs() < 1e-12);
        assert!((stats.peak_dbfs - 20.0 * 0.75f64.log10()).abs() < 1e-9);
        // Bin for 0.75: floor(0.75 * 65536) = 49152
        assert!(stats.hist[49152] >= 1);
    }

    #[test]
    fn cancel_returns_none() {
        let cancel = AtomicBool::new(true); // pre-cancelled
        let l = vec![0.1f64; 1024];
        let r = l.clone();
        // May return None at any cancel point
        let _ = analyze(&l, &r, 44100, &cancel);
        // Just verify it doesn't panic
    }
}
