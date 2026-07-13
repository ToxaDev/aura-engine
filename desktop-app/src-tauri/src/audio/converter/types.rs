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
}

#[allow(dead_code)]
pub struct AudioFile {
    pub samples_l: Vec<f64>,
    pub samples_r: Vec<f64>,
    pub sample_rate: u32,
    pub artist: String,
    pub title: String,
}
/// Audio ready for GPU convolution (decode + headroom + apodize already done).
pub struct PreparedAudio {
    pub audio_l: Vec<f64>,
    pub audio_r: Vec<f64>,
    pub sample_rate: u32,
    pub total_input_samples: usize,
    pub artist: String,
    pub title: String,
    /// Filename tag for the apodizing that ACTUALLY ran in prepare
    /// ("AA" for adaptive, "Apod"/"Apod-M"/"Apod-S" for a static preset,
    /// None when no apodizing was applied). The old code derived the tag
    /// from settings alone, so files were labelled "AA" even when the
    /// detector decided the source was clean and applied nothing.
    pub apod_tag: Option<String>,
}
