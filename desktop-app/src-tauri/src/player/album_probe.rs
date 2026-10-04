//! The converter's album level has to know what each track's output stage
//! will find before the track is rendered (`audio::converter::album`). The
//! player answers the same question for itself before it plays a track — a
//! short render of the same chain, `chain::output_peak` — and this hands the
//! converter that answer. The application installs it at start-up; the
//! engine on its own falls back to the source's 4× peaks.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::chain::{self, Resources, Variant};
use super::convolver::SourceBuf;
use super::settings::{Geometry, Mode, Phase, PlayerSettings};
use crate::audio::converter::album::PeakEstimate;
use crate::audio::converter::types::{ConvertSettings, PreparedAudio};

/// The player's cache folder, where the Hybrid-Phase envelopes are kept
/// (as `controller` names it): an envelope computed for a conversion is then
/// found again when the player plays the same track with the same stages.
fn cache_dir() -> PathBuf {
    crate::app_dir::root()
        .map(|d| d.join("player-cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("AuraEngine").join("player-cache"))
}

/// Filter banks and envelopes for the converter's probes, apart from the
/// player's own, so a batch never pushes the playing track's banks out.
fn resources() -> &'static Resources {
    static RES: OnceLock<Resources> = OnceLock::new();
    RES.get_or_init(|| Resources::new(cache_dir()))
}

/// The chain a conversion runs, as the player's settings: the converter's
/// three phase switches folded back into one (`PlayerSettings::to_engine`
/// the other way round). TFS wins over Hybrid-Phase, as it does in the
/// converter, and only with the built-in filters.
fn player_settings(s: &ConvertSettings) -> PlayerSettings {
    let phase = if s.lab.tfs_phase && s.custom_filter_path.is_none() {
        Phase::Tfs
    } else if s.hybrid_phase && s.lab.continuous_alpha {
        Phase::Alpha
    } else if s.hybrid_phase {
        Phase::Hybrid
    } else {
        Phase::Linear
    };
    let g = &s.lab.xtc_geometry;
    PlayerSettings {
        mode: Mode::Aura,
        fs_multiplier: s.fs_multiplier,
        taps: s.taps,
        phase,
        apodizing: s.apodizing,
        adaptive_apodizer: s.adaptive_apodizer,
        headroom_db: s.headroom_db,
        iir_dc_blocking: s.iir_dc_blocking,
        subsonic_hz: s.subsonic_hz,
        declip: s.lab.declip,
        isp: s.lab.isp,
        adaptive_headroom: s.lab.adaptive_headroom,
        xtc: s.lab.xtc,
        xtc_geometry: Geometry {
            speaker_span_mm: g.speaker_span_mm,
            left_distance_mm: g.left_distance_mm,
            right_distance_mm: g.right_distance_mm,
            head_width_mm: g.head_width_mm,
        },
        use_gpu: false,
    }
}

/// `audio::converter::album::PeakProbe`: the output peak of one prepared
/// track, from a 5k render of its chain (`chain::output_peak`, CPU, in
/// pieces). The prepared audio is lent to the render and handed back
/// untouched. None when the chain cannot be probed — no integer ratio to the
/// output rate, a filter missing — and the converter uses the source's peaks.
pub fn converter_probe(src: &Path, prep: &mut PreparedAudio, settings: &ConvertSettings) -> Option<PeakEstimate> {
    let rate = prep.sample_rate;
    if rate == 0 || settings.out_rate <= rate || settings.out_rate % rate != 0 {
        return None;
    }
    let ps = player_settings(settings);
    let hp = matches!(ps.phase, Phase::Hybrid | Phase::Alpha);
    let track_key = format!("conv:{}", src.display());
    let source = Arc::new(SourceBuf {
        l: std::mem::take(&mut prep.audio_l),
        r: std::mem::take(&mut prep.audio_r),
        rate,
    });
    let tp_source = crate::audio::converter::dsp::true_peak::measure_true_peak(&source.l, &source.r);
    let v = Variant {
        stream: None,
        key: ps.source_key(),
        src: source.clone(),
        out_rate: settings.out_rate,
        l: (settings.out_rate / rate) as usize,
        tp_target_dbtp: prep.true_peak_target_dbtp,
        tp_pred_lin: tp_source,
        lim_local: true,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct: false,
        source_id: chain::source_id(src),
    };
    let res = resources();
    // The probe renders the blend only once the onset envelope is there (the
    // player plays linear phase until then), so it is made first.
    let envelope_ok = !hp || res.envelope(&track_key, &v).is_ok();
    let target_lin = crate::audio::converter::dsp::true_peak::target_lin_for(prep.true_peak_target_dbtp);
    let peak = if envelope_ok {
        chain::output_peak(res, &track_key, &v, &ps, hp, target_lin)
    } else {
        None
    };
    // Nothing of this track is needed again: the conversion measures its own.
    res.forget_tracks_except(&[]);
    drop(v);
    match Arc::try_unwrap(source) {
        Ok(buf) => {
            prep.audio_l = buf.l;
            prep.audio_r = buf.r;
        }
        Err(shared) => {
            prep.audio_l = shared.l.clone();
            prep.audio_r = shared.r.clone();
        }
    }
    peak.map(|p| PeakEstimate { tp_lin: p.tp_lin, local: p.local, rendered: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::types::LabFeatures;

    fn settings() -> ConvertSettings {
        ConvertSettings {
            out_rate: 352_800,
            fs_multiplier: 8,
            taps: 1_000_000,
            precision: 64,
            custom_filter_path: None,
            use_gpu: false,
            use_fir_resampling: true,
            apodizing: 0,
            headroom_db: -0.5,
            adaptive_apodizer: true,
            hybrid_phase: true,
            iir_dc_blocking: false,
            lab: LabFeatures { tfs_phase: true, isp: true, ..LabFeatures::default() },
            subsonic_hz: 15,
        }
    }

    /// The fold back from the converter's switches is `to_engine`'s inverse
    /// for every phase the converter can run.
    #[test]
    fn the_phase_folds_back_as_the_converter_runs_it() {
        let mut s = settings();
        assert_eq!(player_settings(&s).phase, Phase::Tfs, "TFS wins over Hybrid-Phase");
        s.lab.tfs_phase = false;
        assert_eq!(player_settings(&s).phase, Phase::Hybrid);
        s.lab.continuous_alpha = true;
        assert_eq!(player_settings(&s).phase, Phase::Alpha);
        s.hybrid_phase = false;
        s.lab.continuous_alpha = false;
        assert_eq!(player_settings(&s).phase, Phase::Linear);
        s.lab.tfs_phase = true;
        s.custom_filter_path = Some("x.npy".into());
        assert_eq!(player_settings(&s).phase, Phase::Linear, "TFS needs the built-in filters");
        for phase in [Phase::Linear, Phase::Tfs, Phase::Hybrid, Phase::Alpha] {
            let p = PlayerSettings { phase, ..PlayerSettings::default() };
            let mut e = p.to_engine();
            e.fs_multiplier = p.fs_multiplier;
            assert_eq!(player_settings(&e).phase, phase);
        }
    }
}
