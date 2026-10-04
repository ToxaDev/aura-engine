//! Provenance model: how certain a metric value is, given the signal chain.
//!
//! ## Rules
//!
//! | Chain condition | Best allowed provenance |
//! |-----------------|------------------------|
//! | HP or aHP active (token `"HP"` or `"aHP"`) | `Measured` only |
//! | ISP output limiter active (token `"ISP-L"`) — but no HP/aHP | `Forecast` |
//! | Purely LTI chain | `Analytic` |
//!
//! **HP / aHP reasoning:**
//! `Phase::Hybrid` (token `"HP"`) switches between the linear-phase and
//! minimum-phase branches at onset boundaries detected from the source.
//! `Phase::Alpha` (tokens `"HP"` + `"aHP"`) blends the two branches
//! per-sample via the HPSS onset envelope.
//! Both operations are signal-adaptive and nonlinear; no closed-form
//! expression predicts their effect on loudness, LRA or true peak.
//! The token presence is authoritative (see `chain.rs`, `build_chain`,
//! `Phase::Hybrid | Phase::Alpha` arms).
//!
//! **ISP output limiter reasoning:**
//! `LimiterStage` (token `"ISP-L"`) is a nonlinear gain rider; its effect
//! on loudness/LRA/TP is bounded by design (≤ 6 dB of limiting) but is not
//! analytically predictable without a full render pass. A model-based
//! `Forecast` is valid; `Analytic` is not.
//! Token reference: `chain.rs` `build_chain` → `tokens.push("ISP-L")`.

use crate::player::render::ChainDesc;

/// The confidence level of a metric value.
#[derive(Clone, Debug, PartialEq)]
pub enum Provenance {
    /// Derived by exact formula from the filter's linear transfer function and
    /// the (assumed) source statistics. Valid only when the chain is purely
    /// LTI (no HP/aHP, no nonlinear stages).
    Analytic,

    /// HP/αHP was requested but the onset envelope is not yet ready; the chain
    /// is currently playing **linear phase**. Analytic |H| is valid for this
    /// interval, but the frontend should show a distinct "HP not yet on"
    /// label so the user knows the measurement reflects the stand-in chain.
    /// Once `ChainDesc.hp_deferred` becomes false and the HP token is audible,
    /// provenance reverts to `Measured`.
    HpDeferred,

    /// A model-based estimate with bounded error; not exact. Valid when the
    /// ISP output limiter is active but HP/aHP are not (the limiter's gain
    /// trajectory can be bounded but not predicted exactly).
    Forecast,

    /// Computed directly from rendered output samples. `coverage` is the
    /// fraction of the output that was observed (0.0 = not started,
    /// 1.0 = complete). Always valid; required when HP or aHP are active.
    Measured { coverage: f32 },

    /// The metric did not change from a previous measurement (e.g. a
    /// source-only metric reused after a playback-settings change that leaves
    /// the source intact).
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    Unchanged,

    /// The metric cannot be produced; the reason is given.
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    Unavailable(String),
}

/// Decide the strictest provenance class allowed for output-dependent metrics
/// (integrated loudness, LRA, true peak) given the current chain description.
///
/// Strict ordering: `Analytic` = `HpDeferred` > `Forecast` > `Measured`.
/// The function returns the *best* (most informative) provenance that is
/// *valid* for the chain described by `desc`.
///
/// - HP/αHP *audible* (`has_hp && !hp_deferred`) → `Measured { coverage: 0.0 }`.
/// - HP/αHP *requested but deferred* (`hp_deferred`) → `HpDeferred`
///   (linear phase is audible; analytic |H| is valid with an "HP pending" label).
/// - Stage `"ISP-L"` (and no HP/aHP audible) → `Forecast`.
/// - Otherwise → `Analytic`.
pub fn allowed_provenance(desc: &ChainDesc) -> Provenance {
    let has_hp = desc.stages.iter().any(|s| s.tok == "HP" || s.tok == "aHP");
    let has_isp_l = desc.stages.iter().any(|s| s.tok == "ISP-L");

    if has_hp && !desc.hp_deferred {
        // Adaptive phase blending is audible: only direct measurement is valid.
        Provenance::Measured { coverage: 0.0 }
    } else if desc.hp_deferred {
        // HP was requested; the chain is playing linear phase as a stand-in.
        // Analytic |H| is valid, but the frontend must label this interval
        // distinctly as "HP not yet on".
        Provenance::HpDeferred
    } else if has_isp_l {
        // Nonlinear limiter: bounded but not analytic.
        Provenance::Forecast
    } else {
        Provenance::Analytic
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::render::{ChainDesc, StageInfo};

    fn make_desc(toks: &[&str]) -> ChainDesc {
        make_desc_with_deferred(toks, false)
    }

    fn make_desc_with_deferred(toks: &[&str], hp_deferred: bool) -> ChainDesc {
        use crate::player::settings::PlayerSettings;
        ChainDesc {
            source: "live",
            quick: false,
            taps: None,
            stages: toks
                .iter()
                .map(|&t| StageInfo { tok: t.to_string(), st: 1, why: String::new() })
                .collect(),
            gain_db: 0.0,
            tp_db: None,
            ceiling_db: None,
            gpu_on: false,
            downgrade: None,
            downgrade_gen: 0,
            hp_deferred,
            stream: false,
            file: None,
            // analytics: §2.5 stub for tests
            settings: std::sync::Arc::new(PlayerSettings::default()),
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
            out_rate: 0,
            l: 1,
        }
    }

    // ── HP / aHP always force Measured ──────────────────────────────────────

    #[test]
    fn hp_forces_measured() {
        let desc = make_desc(&["DC", "PFR", "HP"]);
        assert!(
            matches!(allowed_provenance(&desc), Provenance::Measured { .. }),
            "HP token must force Measured provenance"
        );
    }

    #[test]
    fn ahp_forces_measured() {
        let desc = make_desc(&["DC", "PFR", "HP", "aHP"]);
        assert!(
            matches!(allowed_provenance(&desc), Provenance::Measured { .. }),
            "aHP token must force Measured provenance"
        );
    }

    /// HP must never yield Analytic or Forecast.
    #[test]
    fn hp_never_analytic_or_forecast() {
        for toks in [vec!["HP"], vec!["HP", "ISP-L"]] {
            let desc = make_desc(&toks);
            let prov = allowed_provenance(&desc);
            assert!(
                !matches!(prov, Provenance::Analytic),
                "HP must not allow Analytic (toks={toks:?})"
            );
            assert!(
                !matches!(prov, Provenance::Forecast),
                "HP must not allow Forecast (toks={toks:?})"
            );
        }
    }

    /// aHP must never yield Analytic or Forecast.
    #[test]
    fn ahp_never_analytic_or_forecast() {
        for toks in [vec!["aHP"], vec!["aHP", "ISP-L"], vec!["HP", "aHP"]] {
            let desc = make_desc(&toks);
            let prov = allowed_provenance(&desc);
            assert!(
                !matches!(prov, Provenance::Analytic),
                "aHP must not allow Analytic (toks={toks:?})"
            );
            assert!(
                !matches!(prov, Provenance::Forecast),
                "aHP must not allow Forecast (toks={toks:?})"
            );
        }
    }

    // ── hp_deferred: linear phase is audible, HP is pending ─────────────────

    /// When hp_deferred is true the chain plays linear phase; analytic |H| is
    /// valid, but we return the distinct HpDeferred label.
    #[test]
    fn hp_deferred_gives_hp_deferred() {
        // hp_deferred=true: onset envelope not ready, linear phase playing.
        let desc = make_desc_with_deferred(&["DC", "PFR"], true);
        assert!(
            matches!(allowed_provenance(&desc), Provenance::HpDeferred),
            "hp_deferred=true must give HpDeferred provenance"
        );
    }

    /// HpDeferred must NOT be Measured or Analytic.
    #[test]
    fn hp_deferred_not_measured_or_analytic() {
        let desc = make_desc_with_deferred(&["DC", "PFR"], true);
        let prov = allowed_provenance(&desc);
        assert!(
            !matches!(prov, Provenance::Measured { .. }),
            "hp_deferred must not be Measured"
        );
        assert!(
            !matches!(prov, Provenance::Analytic),
            "hp_deferred must not be plain Analytic"
        );
    }

    /// Once hp_deferred is false and HP token is present, revert to Measured.
    #[test]
    fn hp_active_not_deferred_gives_measured() {
        let desc = make_desc_with_deferred(&["DC", "PFR", "HP"], false);
        assert!(
            matches!(allowed_provenance(&desc), Provenance::Measured { .. }),
            "HP token with hp_deferred=false must give Measured"
        );
    }

    // ── ISP-L gives Forecast (absent HP/aHP) ────────────────────────────────

    #[test]
    fn isp_l_gives_forecast() {
        let desc = make_desc(&["DC", "ISP", "PFR", "ISP-L"]);
        assert!(
            matches!(allowed_provenance(&desc), Provenance::Forecast),
            "ISP-L without HP must give Forecast"
        );
    }

    #[test]
    fn isp_l_never_analytic() {
        let desc = make_desc(&["ISP-L"]);
        assert!(
            !matches!(allowed_provenance(&desc), Provenance::Analytic),
            "ISP-L must not allow Analytic"
        );
    }

    // ── Clean LTI chain is Analytic ──────────────────────────────────────────

    #[test]
    fn linear_phase_chain_is_analytic() {
        let desc = make_desc(&["DC", "PFR"]);
        assert!(matches!(allowed_provenance(&desc), Provenance::Analytic));
    }

    #[test]
    fn empty_chain_is_analytic() {
        let desc = make_desc(&[]);
        assert!(matches!(allowed_provenance(&desc), Provenance::Analytic));
    }

    #[test]
    fn minimum_phase_without_isp_l_is_analytic() {
        let desc = make_desc(&["DC", "PFR", "MIN"]);
        assert!(matches!(allowed_provenance(&desc), Provenance::Analytic));
    }

    #[test]
    fn tfs_without_isp_l_is_analytic() {
        let desc = make_desc(&["DC", "PFR", "TFS"]);
        assert!(matches!(allowed_provenance(&desc), Provenance::Analytic));
    }
}
