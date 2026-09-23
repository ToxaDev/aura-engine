/// The listening triangle, in millimetres, exactly as measured in the dialog.
///
/// This is the only setting in the product that describes the ROOM rather
/// than the file, and it is required rather than optional: a crosstalk
/// canceller built from guessed distances does not cancel less, it cancels
/// the wrong thing and reinforces what it was meant to remove. The interface
/// will not let XTC come up until these are filled in, and `is_usable` is the
/// same gate restated on this side so a hand-written payload cannot slip past.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct XtcGeometry {
    pub speaker_span_mm: f64,
    pub left_distance_mm: f64,
    pub right_distance_mm: f64,
    pub head_width_mm: f64,
}

impl XtcGeometry {
    /// Whether these four numbers describe a triangle someone could sit in.
    /// Mirrors `isGeometryValid` in xtc-geom.js; the two are meant to agree.
    pub fn is_usable(&self) -> bool {
        let (s, l, r, h) = (
            self.speaker_span_mm,
            self.left_distance_mm,
            self.right_distance_mm,
            self.head_width_mm,
        );
        (200.0..=5000.0).contains(&s)
            && (300.0..=8000.0).contains(&l)
            && (300.0..=8000.0).contains(&r)
            && (120.0..=220.0).contains(&h)
            && l + r > s
            && (l - r).abs() < s
            // A head level with the speakers has zero depth: the model's right
            // triangle collapses and there is no filter to build.
            && self.depth_mm() > 50.0
    }

    /// Full angle subtended by the speakers at the head, in degrees. Law of
    /// cosines, so an off-centre chair needs no special case.
    pub fn span_angle_deg(&self) -> f64 {
        let (s, l, r) = (
            self.speaker_span_mm,
            self.left_distance_mm,
            self.right_distance_mm,
        );
        let c = ((l * l + r * r - s * s) / (2.0 * l * r)).clamp(-1.0, 1.0);
        c.acos().to_degrees()
    }

    /// The filter is symmetric, so it can only be built for one distance.
    pub fn mean_distance_mm(&self) -> f64 {
        (self.left_distance_mm + self.right_distance_mm) / 2.0
    }

    /// PERPENDICULAR distance from the line joining the speakers to the ear
    /// plane — which is what `design_xtc_filters` means by `listener_distance_mm`.
    ///
    /// This is not what the dialog asks for, and the difference is the whole
    /// reason this function exists. A tape measure can reach from a speaker to
    /// your head; it cannot reach to an imaginary line, so the dialog asks for
    /// the two direct distances and the depth is derived here by trilateration:
    ///
    ///     x = (dL² − dR²) / 2S            lateral offset of the head
    ///     y = √(dL² − (x + S/2)²)         depth, the leg the model wants
    ///
    /// Passing the direct distance instead is a real error and was one here:
    /// far away the two barely differ (3000 mm direct over a 2000 mm span is
    /// 2828 mm deep — 38.9° against a modelled 36.9°), but close in they part
    /// company completely. At 1600 mm span and 860 mm direct the true depth is
    /// 316 mm and the span angle 136.9°, while the direct figure would have the
    /// model build for 85.9° — a different room.
    pub fn depth_mm(&self) -> f64 {
        let s = self.speaker_span_mm;
        let (l, r) = (self.left_distance_mm, self.right_distance_mm);
        let x = (l * l - r * r) / (2.0 * s);
        let leg = x + s / 2.0;
        (l * l - leg * leg).max(0.0).sqrt()
    }

    pub fn asymmetry_mm(&self) -> f64 {
        (self.left_distance_mm - self.right_distance_mm).abs()
    }
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LabFeatures {
    pub declip: bool,
    pub isp: bool,
    pub tfs_phase: bool,
    pub continuous_alpha: bool,
    pub adaptive_headroom: bool,
    pub xtc: bool,
    pub xtc_geometry: XtcGeometry,
}

#[derive(Clone)]
pub struct ConvertSettings {
    pub out_rate: u32,        // Computed per-file: family_base * fs_multiplier
    pub fs_multiplier: u32,   // FS value: 2, 4, 8, or 16
    pub taps: usize,
    pub precision: u32,
    pub win_type: i32,
    pub custom_filter_path: Option<String>,
    pub use_gpu: bool,
    pub use_fir_resampling: bool, // Integrated FIR resampling (zero-stuff + FIR, no rubato)
    pub apodizing: u32,           // 0=off, 1=gentle, 2=moderate, 3=strong
    pub headroom_db: f64,         // headroom in dB (0.0 = off, -0.5, -1.0, -3.0)
    pub adaptive_apodizer: bool,  // Per-file ADC ringing detection
    pub hybrid_phase: bool,       // Dual-phase transient blending
    pub iir_dc_blocking: bool,    // Optional 1st-order IIR high-pass instead of global mean
    pub lab: LabFeatures,
    pub subsonic_hz: u32,         // Linear-phase subsonic high-pass corner: 0 = off, else 10/15/20 Hz
}

#[allow(dead_code)]
pub struct AudioFile {
    pub samples_l: Vec<f64>,
    pub samples_r: Vec<f64>,
    pub sample_rate: u32,
    /// Channels in the SOURCE, not in `samples_*` — which are always the
    /// front pair. Kept so the output filename can say a wider file was
    /// narrowed rather than leaving that only in the log.
    pub channels: usize,
    pub artist: String,
    pub title: String,
}
/// Audio ready for GPU convolution (decode + headroom + apodize already done).
pub struct PreparedAudio {
    pub audio_l: Vec<f64>,
    pub audio_r: Vec<f64>,
    pub sample_rate: u32,
    /// Source channel count, carried through for the output filename.
    pub source_channels: usize,
    pub total_input_samples: usize,
    pub artist: String,
    pub title: String,
    /// Filename tag for the apodizing that ACTUALLY ran in prepare
    /// ("AA" for adaptive, "Apod"/"Apod-M"/"Apod-S" for a static preset,
    /// None when no apodizing was applied). The old code derived the tag
    /// from settings alone, so files were labelled "AA" even when the
    /// detector decided the source was clean and applied nothing.
    pub apod_tag: Option<String>,
    /// Adaptive Apodizer verdict for the queue UI: (treated, tooltip).
    /// None when AA was not enabled for this batch. `treated == false`
    /// means the detector analyzed the track and left it untouched; the
    /// tooltip carries the reason.
    pub aa_ui: Option<(bool, String)>,
    /// VORBIS_COMMENT key/value pairs accumulated by the source passes.
    pub lab_tags: Vec<(String, String)>,
    /// Applied-feature tokens for AURA_CHAIN and filename (e.g. "DC", "ISP").
    pub lab_chain: Vec<&'static str>,
    /// The true-peak ceiling this file is normalised to, in dBTP. The
    /// shipped -0.5 unless the Headroom control asked for a lower one and
    /// Adaptive Headroom did not veto it.
    pub true_peak_target_dbtp: f64,
    /// Stage outcomes collected during the prepare phase (DECLIP, ISP, AHR).
    /// process.rs appends TFS/ALPHA/XTC outcomes and pushes to the report registry.
    pub lab_outcomes: Vec<crate::audio::converter::dsp::lab::LabOutcome>,
}
