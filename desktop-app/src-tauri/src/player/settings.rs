//! The settings the interface sends, and how they map onto the engine's own
//! `ConvertSettings`.
//!
//! The field set is the converter's, so a chain set up in one sounds the same
//! in the other. Two additions: `mode` (Aura, or Direct — bit-perfect, no DSP
//! at all) and `phase`, which folds the converter's three phase switches
//! (Hybrid-Phase, TFS, continuous alpha) into one choice, plus plain minimum
//! phase, which the converter does not offer but which a player wants for
//! comparison.

use serde::{Deserialize, Serialize};

use crate::audio::converter::types::{ConvertSettings, LabFeatures, XtcGeometry};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Aura,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Linear,
    Minimum,
    Tfs,
    Hybrid,
    Alpha,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Geometry {
    pub speaker_span_mm: f64,
    pub left_distance_mm: f64,
    pub right_distance_mm: f64,
    pub head_width_mm: f64,
}

impl Default for Geometry {
    fn default() -> Self {
        Geometry { speaker_span_mm: 0.0, left_distance_mm: 0.0, right_distance_mm: 0.0, head_width_mm: 0.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlayerSettings {
    pub mode: Mode,
    pub fs_multiplier: u32,
    pub taps: usize,
    pub phase: Phase,
    pub apodizing: u32,
    pub adaptive_apodizer: bool,
    pub headroom_db: f64,
    pub iir_dc_blocking: bool,
    pub subsonic_hz: u32,
    pub declip: bool,
    pub isp: bool,
    pub adaptive_headroom: bool,
    pub xtc: bool,
    pub xtc_geometry: Geometry,
    /// Send the convolver to the GPU when a discrete GPU is available and the
    /// CPU's predicted real-time factor is below the margin threshold.
    /// Mapped from the JS field `useGpu` (the converter's GPU checkbox, K7).
    /// Default: false (off until the user enables it).
    /// NOT included in `source_key()` — changing this does not re-prepare.
    #[serde(rename = "useGpu")]
    pub use_gpu: bool,
}

impl Default for PlayerSettings {
    /// The converter's fresh-install defaults (1.3.x): DC, ISP, SUB15, AHR,
    /// AA and TFS on; Hybrid-Phase and XTC off; FS×8, 1M taps, −0.5 dB.
    /// Static apodizing 0: the converter's interface zeroes it while AA is
    /// on, so AA's fallback there is no apodizing at all.
    fn default() -> Self {
        PlayerSettings {
            mode: Mode::Aura,
            fs_multiplier: 8,
            taps: 1_000_000,
            phase: Phase::Tfs,
            apodizing: 0,
            adaptive_apodizer: true,
            headroom_db: -0.5,
            iir_dc_blocking: false,
            subsonic_hz: 15,
            declip: true,
            isp: true,
            adaptive_headroom: true,
            xtc: false,
            xtc_geometry: Geometry::default(),
            use_gpu: false,
        }
    }
}

impl PlayerSettings {
    pub fn engine_geometry(&self) -> XtcGeometry {
        XtcGeometry {
            speaker_span_mm: self.xtc_geometry.speaker_span_mm,
            left_distance_mm: self.xtc_geometry.left_distance_mm,
            right_distance_mm: self.xtc_geometry.right_distance_mm,
            head_width_mm: self.xtc_geometry.head_width_mm,
        }
    }

    /// XTC is on only with a usable triangle — the converter's gate.
    pub fn xtc_active(&self) -> bool {
        self.xtc && self.engine_geometry().is_usable()
    }

    /// The engine settings that `prepare_audio_phase` and the filter
    /// resolution read. `out_rate` is filled in per file by prepare.
    pub fn to_engine(&self) -> ConvertSettings {
        ConvertSettings {
            out_rate: 0,
            fs_multiplier: self.fs_multiplier,
            taps: self.taps,
            precision: 0,
            custom_filter_path: None,
            // The source stages run on the CPU in the player: the GPU is
            // not guaranteed to be free while music plays, and none of them
            // is large enough to need it.
            use_gpu: false,
            use_fir_resampling: true,
            apodizing: self.apodizing,
            headroom_db: self.headroom_db,
            adaptive_apodizer: self.adaptive_apodizer,
            hybrid_phase: matches!(self.phase, Phase::Hybrid | Phase::Alpha),
            iir_dc_blocking: self.iir_dc_blocking,
            lab: LabFeatures {
                declip: self.declip,
                isp: self.isp,
                tfs_phase: self.phase == Phase::Tfs,
                continuous_alpha: self.phase == Phase::Alpha,
                adaptive_headroom: self.adaptive_headroom,
                xtc: self.xtc_active(),
                xtc_geometry: self.engine_geometry(),
            },
            subsonic_hz: self.subsonic_hz,
        }
    }

    /// Everything that changes what `prepare_audio_phase` produces. Two
    /// settings with the same key share a source variant.
    pub fn source_key(&self) -> String {
        format!(
            "dc{}|dcl{}|isp{}|sub{}|apod{}|aa{}|hr{}|ahr{}|fs{}",
            self.iir_dc_blocking as u8,
            self.declip as u8,
            self.isp as u8,
            self.subsonic_hz,
            self.apodizing,
            self.adaptive_apodizer as u8,
            self.headroom_db,
            self.adaptive_headroom as u8,
            self.fs_multiplier,
        )
    }

    /// The settings for the quick first variant: decode and DC removal only,
    /// so sound can start before the whole-file repairs have run.
    pub fn quick(&self) -> PlayerSettings {
        PlayerSettings {
            declip: false,
            isp: false,
            subsonic_hz: 0,
            apodizing: 0,
            adaptive_apodizer: false,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly what `collectPlayerSettings()` in settings.js sends on a fresh
    /// install (XTC unmeasured: zero triangle, the geometry window's default
    /// head width). v0.1 sent the geometry in the window's own shape and the
    /// backend refused every settings change, silently, for the whole
    /// session — keep this sample in step with settings.js.
    const PAGE_PAYLOAD: &str = r#"{
        "mode": "aura", "fsMultiplier": 8, "taps": 1000000, "phase": "tfs",
        "apodizing": 0, "adaptiveApodizer": true, "headroomDb": -0.5,
        "iirDcBlocking": false, "subsonicHz": 15, "declip": true, "isp": true,
        "adaptiveHeadroom": true, "xtc": false,
        "xtcGeometry": { "speakerSpanMm": 0, "leftDistanceMm": 0,
                         "rightDistanceMm": 0, "headWidthMm": 180 }
    }"#;

    #[test]
    fn the_page_payload_is_the_default_chain() {
        let s: PlayerSettings = serde_json::from_str(PAGE_PAYLOAD).expect("the page's JSON must deserialize");
        let want = PlayerSettings {
            xtc_geometry: Geometry { head_width_mm: 180.0, ..Geometry::default() },
            ..PlayerSettings::default()
        };
        assert_eq!(s, want);
    }

    #[test]
    fn a_partial_geometry_does_not_refuse_the_whole_payload() {
        let json = PAGE_PAYLOAD.replace(r#""headWidthMm": 180"#, r#""unknownField": 1"#);
        let s: PlayerSettings = serde_json::from_str(&json).expect("missing geometry fields default");
        assert_eq!(s.xtc_geometry.head_width_mm, 0.0);
        assert!(!s.xtc_active());
    }
}
