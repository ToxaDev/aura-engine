use super::stats::SourceStats;

pub struct HeadroomDecision {
    pub skip_gain: bool,
    pub reason: String,
}

/// Decide whether to skip the headroom gain for this file.
/// Called only when `adaptive_headroom` is on.
///
/// Skip conditions (either is sufficient):
///   1. peak_dbfs <= headroom_db + 0.05  — file already has natural headroom
///      for the requested gain; applying it would reduce level unnecessarily.
///   2. enob > 20.0 && peak_dbfs < -1.0 — very clean (>20-bit effective)
///      source with a modest peak; headroom gain would degrade a pristine
///      high-bit-depth file without benefit.
///
/// Reason strings: "natural-headroom", "high-enob-low-peak", "peak-high",
/// "no-stats". These mirror the AURA_HEADROOM tag value.
pub fn decide(stats: Option<&SourceStats>, headroom_db: f64) -> HeadroomDecision {
    let Some(s) = stats else {
        // No stats available — apply gain conservatively.
        return HeadroomDecision {
            skip_gain: false,
            reason: "no-stats".to_string(),
        };
    };

    // Condition 1: peak is already below the headroom target (± 0.05 dB slack).
    if s.peak_dbfs <= headroom_db + 0.05 {
        return HeadroomDecision {
            skip_gain: true,
            reason: "natural-headroom".to_string(),
        };
    }

    // Condition 2: high-ENOB source with moderate peak — preserve fidelity.
    if let Some(enob) = s.enob {
        if enob > 20.0 && s.peak_dbfs < -1.0 {
            return HeadroomDecision {
                skip_gain: true,
                reason: "high-enob-low-peak".to_string(),
            };
        }
    }

    // Peak is high enough that the gain is warranted.
    HeadroomDecision {
        skip_gain: false,
        reason: "peak-high".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::dsp::lab::stats::SourceStats;

    fn make_stats(peak_dbfs: f64, enob: Option<f64>) -> SourceStats {
        SourceStats {
            peak_lin: 10.0_f64.powf(peak_dbfs / 20.0),
            peak_dbfs,
            hist: vec![0u64; 65536],
            grid16: false,
            enob,
        }
    }

    #[test]
    fn stub_default_does_not_skip() {
        let d = decide(None, -3.0);
        assert!(!d.skip_gain);
    }

    #[test]
    fn no_stats_reason() {
        let d = decide(None, -3.0);
        assert_eq!(d.reason, "no-stats");
        assert!(!d.skip_gain);
    }

    #[test]
    fn natural_headroom_skips() {
        // peak −3.0 dBFS, headroom −3.0 dB: peak ≤ −3.0 + 0.05 → skip
        let s = make_stats(-3.0, None);
        let d = decide(Some(&s), -3.0);
        assert!(d.skip_gain, "should skip when peak is within the headroom target");
        assert_eq!(d.reason, "natural-headroom");
    }

    #[test]
    fn natural_headroom_slack_boundary() {
        // peak at exactly headroom + 0.05 — still skips
        let s = make_stats(-2.95, None);
        let d = decide(Some(&s), -3.0);
        assert!(d.skip_gain);
        assert_eq!(d.reason, "natural-headroom");

        // peak slightly above the slack — no longer skips via this condition
        let s2 = make_stats(-2.94, None);
        let d2 = decide(Some(&s2), -3.0);
        // (may still skip via high-ENOB condition if enob > 20 and peak < -1)
        // With enob=None this falls through to peak-high
        assert!(!d2.skip_gain);
        assert_eq!(d2.reason, "peak-high");
    }

    #[test]
    fn high_enob_low_peak_skips() {
        // ENOB 21.0, peak −2.0 dBFS: qualifies for high-ENOB skip
        let s = make_stats(-2.0, Some(21.0));
        let d = decide(Some(&s), -3.0);
        assert!(d.skip_gain);
        assert_eq!(d.reason, "high-enob-low-peak");
    }

    #[test]
    fn high_enob_but_high_peak_does_not_skip() {
        // ENOB 21.0, but peak is −0.5 dBFS (≥ −1.0) — apply gain
        let s = make_stats(-0.5, Some(21.0));
        let d = decide(Some(&s), -3.0);
        assert!(!d.skip_gain);
        assert_eq!(d.reason, "peak-high");
    }

    #[test]
    fn low_enob_with_low_peak_does_not_skip_via_enob() {
        // ENOB 14.0 (not > 20), peak −2.0 dBFS — no skip via ENOB condition
        // peak −2.0 > headroom −3.0 + 0.05 = −2.95 → no natural-headroom skip either
        let s = make_stats(-2.0, Some(14.0));
        let d = decide(Some(&s), -3.0);
        assert!(!d.skip_gain);
        assert_eq!(d.reason, "peak-high");
    }

    #[test]
    fn enob_none_does_not_trigger_high_enob_path() {
        let s = make_stats(-2.0, None);
        let d = decide(Some(&s), -3.0);
        assert!(!d.skip_gain);
        assert_eq!(d.reason, "peak-high");
    }

    #[test]
    fn headroom_zero_db_no_skip_if_peak_high() {
        // headroom_db = 0.0: only skip if peak ≤ 0.05 dBFS
        let s = make_stats(-0.1, None);
        let d = decide(Some(&s), 0.0);
        assert!(d.skip_gain); // -0.1 ≤ 0.0 + 0.05 = 0.05
        assert_eq!(d.reason, "natural-headroom");

        let s2 = make_stats(0.1, None);
        let d2 = decide(Some(&s2), 0.0);
        assert!(!d2.skip_gain); // 0.1 > 0.05
    }
}
