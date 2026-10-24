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

use crate::audio::converter::dsp::multistage::UpsamplerMode;
use crate::audio::converter::types::{ConvertSettings, LabFeatures, XtcGeometry};
use crate::audio::dsd::{encoder::new_encoder, DsdRate, ModulatorKind};

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

/// What the player hands the device in Aura mode: PCM at the rack's FS, or
/// the chain's output as one-bit DSD over DoP. BIT-PERFECT always plays PCM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OutputFormat {
    #[default]
    Pcm,
    /// DSD`multiple` (64, 128 or 256) of the source's family. The chain runs
    /// at the DoP carrier — ×4, ×8, ×16 — whatever the rack's FS.
    Dsd {
        multiple: u32,
        #[serde(default)]
        modulator: ModulatorKind,
    },
}

/// The DSD multiples DoP carries over a WASAPI stream (DSD256 = 705.6 kHz).
pub const DOP_MULTIPLES: [u32; 3] = [64, 128, 256];

impl OutputFormat {
    /// The FS the chain runs at for this output: DoP's carrier is a sixteenth
    /// of the DSD rate. None for PCM (the rack's own FS).
    pub fn forced_fs(self) -> Option<u32> {
        match self {
            OutputFormat::Pcm => None,
            OutputFormat::Dsd { multiple, .. } => Some(multiple / 16),
        }
    }

    /// Whether the player can play this output format, for the settings
    /// command: a DSD rate DoP carries and a modulator for playback (not the
    /// converter's only), built for that rate and in this version — else it would fail only when the stream opens.
    /// The error says why.
    pub fn check(self) -> Result<(), String> {
        let OutputFormat::Dsd { multiple, modulator } = self else { return Ok(()) };
        let Some(rate) = DsdRate::new(44_100, multiple).filter(|_| DOP_MULTIPLES.contains(&multiple)) else {
            return Err(format!("Output format: DSD{multiple} cannot be sent over DoP"));
        };
        let info = modulator.info();
        if !info.realtime {
            return Err(format!("Output format: {} is for the converter only", info.label));
        }
        if !modulator.supports(rate) {
            return Err(format!("Output format: {} is built for DSD{} and up", info.label, info.min_multiple));
        }
        if new_encoder(modulator, rate.dop_carrier(), rate).is_err() {
            return Err(format!("Output format: {} is not available in this version", info.label));
        }
        Ok(())
    }

    /// The DSD rate a DoP stream carries at `out_rate`, the chain's output
    /// rate (a source at or above the carrier steps FS up, and the DSD rate
    /// with it, as PCM does): None for PCM, or when `out_rate` is no DoP
    /// carrier of DSD64–DSD256 — the caller plays PCM then.
    pub fn dsd_at(self, out_rate: u32) -> Option<(DsdRate, ModulatorKind)> {
        let OutputFormat::Dsd { modulator, .. } = self else { return None };
        let base = [44_100, 48_000].into_iter().find(|b| out_rate % b == 0)?;
        let multiple = out_rate / base * 16;
        if !DOP_MULTIPLES.contains(&multiple) {
            return None;
        }
        DsdRate::new(base, multiple).map(|r| (r, modulator))
    }
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
    /// Which upsampler plays integer ratios: a hidden switch, the way back to
    /// single-stage (no control in the interface). None = `AURA_MULTISTAGE`
    /// (Multi unless it is 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upsampler: Option<Upsampler>,
    /// PCM or DSD over DoP (`OutputFormat`); ignored in Direct mode.
    pub output: OutputFormat,
}

/// The hidden upsampler switch as the page sends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upsampler {
    Single,
    Multi,
}

impl Upsampler {
    /// The upsampler a page's switch picks (the player's and the converter's
    /// alike): the switch when sent, else the environment.
    pub fn resolve(switch: Option<Upsampler>) -> UpsamplerMode {
        match switch {
            Some(Upsampler::Single) => UpsamplerMode::Single,
            Some(Upsampler::Multi) => UpsamplerMode::Multi,
            None => UpsamplerMode::from_env(),
        }
    }
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
            upsampler: None,
            output: OutputFormat::Pcm,
        }
    }
}

impl PlayerSettings {
    /// The upsampler this rack plays integer ratios with: the hidden setting
    /// when the page sent one, else the environment (`AURA_MULTISTAGE`, Multi
    /// unless it is 0).
    pub fn upsampler_mode(&self) -> UpsamplerMode {
        Upsampler::resolve(self.upsampler)
    }

    /// The rack as the chain runs it: DSD output forces the FS to the DoP
    /// carrier's (`OutputFormat::forced_fs`), so a change of the output
    /// format re-prepares the chain as an FS change does. In Direct mode the
    /// output is PCM and nothing changes.
    pub fn for_output(mut self) -> PlayerSettings {
        if self.mode == Mode::Aura {
            if let Some(fs) = self.output.forced_fs() {
                self.fs_multiplier = fs;
            }
        }
        self
    }

    /// The DSD rate this rack sends a stream at `out_rate` as (DoP), or None
    /// for PCM: BIT-PERFECT, the PCM format, or a rate DoP cannot carry.
    pub fn dsd_at(&self, out_rate: u32) -> Option<(DsdRate, ModulatorKind)> {
        if self.mode == Mode::Direct {
            return None;
        }
        self.output.dsd_at(out_rate)
    }

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
            upsampler: self.upsampler_mode(),
            dsd: None,
            rack_sig: None,
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

    /// The hidden upsampler switch: absent on a fresh page (the default and
    /// the environment decide), and when sent it wins over the environment.
    #[test]
    fn the_hidden_upsampler_switch_wins_when_sent() {
        let s: PlayerSettings = serde_json::from_str(PAGE_PAYLOAD).unwrap();
        assert_eq!(s.upsampler, None);
        assert!(!serde_json::to_string(&s).unwrap().contains("upsampler"), "not sent back when unset");
        let multi = PAGE_PAYLOAD.replace(r#""mode": "aura","#, r#""mode": "aura", "upsampler": "multi","#);
        let s: PlayerSettings = serde_json::from_str(&multi).unwrap();
        assert_eq!(s.upsampler, Some(Upsampler::Multi));
        assert_eq!(s.upsampler_mode(), UpsamplerMode::Multi);
        let single = PlayerSettings { upsampler: Some(Upsampler::Single), ..s };
        assert_eq!(single.upsampler_mode(), UpsamplerMode::Single);
    }

    /// Multistage is the default: a page (or a save from before the switch)
    /// without the field plays multistage unless `AURA_MULTISTAGE=0`; the
    /// switch, when sent, wins over the environment either way.
    #[test]
    fn the_default_is_multistage_and_single_is_the_fallback() {
        let old: PlayerSettings = serde_json::from_str(PAGE_PAYLOAD).unwrap();
        assert_eq!(old.upsampler_mode(), UpsamplerMode::from_env());
        if std::env::var_os("AURA_MULTISTAGE").is_none() {
            assert_eq!(old.upsampler_mode(), UpsamplerMode::Multi);
            assert_eq!(PlayerSettings::default().to_engine().upsampler, UpsamplerMode::Multi);
        }
        assert_eq!(Upsampler::resolve(None), UpsamplerMode::from_env());
        assert_eq!(Upsampler::resolve(Some(Upsampler::Single)), UpsamplerMode::Single);
        assert_eq!(Upsampler::resolve(Some(Upsampler::Multi)), UpsamplerMode::Multi);
        let single = PAGE_PAYLOAD.replace(r#""mode": "aura","#, r#""mode": "aura", "upsampler": "single","#);
        let s: PlayerSettings = serde_json::from_str(&single).unwrap();
        assert_eq!(s.upsampler_mode(), UpsamplerMode::Single);
        assert_eq!(s.to_engine().upsampler, UpsamplerMode::Single);
        assert!(serde_json::to_string(&s).unwrap().contains(r#""upsampler":"single""#), "kept on the way back");
    }

    /// No `output` in the payload (every page before DSD): PCM. DSD as the
    /// page sends it; the modulator may be left out.
    #[test]
    fn the_output_format_defaults_to_pcm_and_reads_dsd() {
        let s: PlayerSettings = serde_json::from_str(PAGE_PAYLOAD).unwrap();
        assert_eq!(s.output, OutputFormat::Pcm);
        let json = PAGE_PAYLOAD.replace(r#""xtc": false,"#, r#""xtc": false, "output": {"kind": "dsd", "multiple": 256, "modulator": "sdm5"},"#);
        let s: PlayerSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(s.output, OutputFormat::Dsd { multiple: 256, modulator: ModulatorKind::Sdm5 });
        let json = PAGE_PAYLOAD.replace(r#""xtc": false,"#, r#""xtc": false, "output": {"kind": "dsd", "multiple": 64},"#);
        let s: PlayerSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(s.output, OutputFormat::Dsd { multiple: 64, modulator: ModulatorKind::Sdm5 });
        let back = serde_json::to_value(OutputFormat::Pcm).unwrap();
        assert_eq!(back, serde_json::json!({ "kind": "pcm" }));
    }

    /// The settings command takes every modulator built for playback at
    /// every DoP rate it is built for, and says why it takes no other — never
    /// SDM5 in its place.
    #[test]
    fn the_output_format_check_takes_what_is_built_and_names_what_is_not() {
        use crate::audio::dsd::CATALOG;
        let dsd = |multiple, modulator| OutputFormat::Dsd { multiple, modulator };
        assert_eq!(OutputFormat::Pcm.check(), Ok(()));
        for i in CATALOG.iter() {
            for multiple in DOP_MULTIPLES {
                let rate = DsdRate::new(44_100, multiple).unwrap();
                let built = i.realtime && new_encoder(i.kind, rate.dop_carrier(), rate).is_ok();
                let got = dsd(multiple, i.kind).check();
                if built {
                    assert_eq!(got, Ok(()), "{} at DSD{multiple}", i.label);
                } else {
                    let err = got.expect_err(i.label);
                    assert!(err.starts_with("Output format: ") && err.contains(i.label), "{err}");
                }
            }
        }
        assert_eq!(dsd(256, ModulatorKind::Sdm5).check(), Ok(()));
        assert_eq!(dsd(64, ModulatorKind::Sdm7).check(), Ok(()));
        assert_eq!(dsd(64, ModulatorKind::Sdm7Wide).check(), Err("Output format: SDM7 Wide is built for DSD128 and up".into()));
        assert_eq!(dsd(256, ModulatorKind::Sdm9).check(), Err("Output format: SDM9 is built for DSD512 and up".into()));
        assert_eq!(dsd(256, ModulatorKind::Sdm9Trellis).check(), Err("Output format: SDM9 Trellis is for the converter only".into()));
        // Not built yet in this version (look-ahead lands with stage 4): named, never SDM5 instead.
        if new_encoder(ModulatorKind::Sdm7LaPlus, 176_400, DsdRate::DSD64).is_err() {
            assert_eq!(dsd(64, ModulatorKind::Sdm7LaPlus).check(), Err("Output format: SDM7 LA+ is not available in this version".into()));
        }
        assert_eq!(dsd(64, ModulatorKind::Sdm7Trellis).check(), Err("Output format: SDM7 Trellis is for the converter only".into()));
        assert_eq!(dsd(512, ModulatorKind::Sdm5).check(), Err("Output format: DSD512 cannot be sent over DoP".into()));
        assert_eq!(dsd(32, ModulatorKind::Sdm5).check(), Err("Output format: DSD32 cannot be sent over DoP".into()));
    }

    /// DSD64/128/256 run the chain at ×4/×8/×16; PCM and BIT-PERFECT keep
    /// the rack's FS. The DSD rate follows the chain's output rate: a 48 kHz
    /// family source plays 48 kHz family DSD, a source at or above the
    /// carrier steps up; a rate DoP cannot carry is PCM.
    #[test]
    fn dsd_output_forces_the_fs_and_follows_the_output_rate() {
        let dsd = |m| PlayerSettings { output: OutputFormat::Dsd { multiple: m, modulator: ModulatorKind::Sdm5 }, ..PlayerSettings::default() };
        assert_eq!(dsd(64).for_output().fs_multiplier, 4);
        assert_eq!(dsd(128).for_output().fs_multiplier, 8);
        assert_eq!(dsd(256).for_output().fs_multiplier, 16);
        assert_eq!(PlayerSettings::default().for_output().fs_multiplier, 8);
        let direct = PlayerSettings { mode: Mode::Direct, ..dsd(64) };
        assert_eq!(direct.clone().for_output().fs_multiplier, 8);
        assert_eq!(direct.dsd_at(176_400), None);
        let s = dsd(64);
        assert_eq!(s.dsd_at(176_400), Some((DsdRate::DSD64, ModulatorKind::Sdm5)));
        assert_eq!(s.dsd_at(705_600), Some((DsdRate::DSD256, ModulatorKind::Sdm5)));
        assert_eq!(s.dsd_at(384_000), Some((DsdRate::new(48_000, 128).unwrap(), ModulatorKind::Sdm5)));
        assert_eq!(s.dsd_at(88_200), None); // DSD32: no
        assert_eq!(s.dsd_at(1_411_200), None); // DSD512: not over DoP
        assert_eq!(s.dsd_at(96_001), None);
        assert_eq!(PlayerSettings::default().dsd_at(176_400), None);
    }

    #[test]
    fn a_partial_geometry_does_not_refuse_the_whole_payload() {
        let json = PAGE_PAYLOAD.replace(r#""headWidthMm": 180"#, r#""unknownField": 1"#);
        let s: PlayerSettings = serde_json::from_str(&json).expect("missing geometry fields default");
        assert_eq!(s.xtc_geometry.head_width_mm, 0.0);
        assert!(!s.xtc_active());
    }
}
