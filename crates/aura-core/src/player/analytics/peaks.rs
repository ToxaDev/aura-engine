//! Peak detection: sample peak, EBU R128 / ITU BS.1770-4 true peak, engine
//! true peak, and full-scale / clip-run analysis.
//!
//! # EBU true-peak algorithm
//!
//! Reference implementation: Jan Kokemüller / Sebastian Dröge, `ebur128` crate
//! `src/interp.rs` and `src/true_peak.rs` (MIT licence).
//! <https://github.com/sdroege/ebur128>
//!
//! ## Filter design
//!
//! The filter has 48 total taps (constant across all factors).  Coefficients
//! are a Hanning-windowed sinc lowpass with the window convention used by the
//! sdroege reference (window denominator = TAPS = 48, **not** TAPS − 1):
//!
//! ```text
//! h[j] = w(j) · sinc(m / F),   j ∈ [0, 48),   m = j − 24
//! w(j) = 0.5 · (1 − cos(2π j / 48))
//! sinc(x) = sin(π x) / (π x),  sinc(0) = 1
//! F = interpolation factor (4 or 2)
//! ```
//!
//! The polyphase decomposition stores tap `j` for phase `p` at flat index
//! `j · F + p`.  For each input sample the convolver produces `F` output
//! samples at fractional times `0, 1/F, 2/F, …, (F−1)/F`.
//!
//! ## Factor selection
//!
//! Rule from `sdroege/ebur128 src/true_peak.rs`:
//!
//! | sample rate       | factor | taps / phase |
//! |-------------------|--------|--------------|
//! | < 96 000 Hz       | 4      | 12           |
//! | 96 000–191 999 Hz | 2      | 24           |
//! | ≥ 192 000 Hz      | 1      | —            |
//!
//! At ≥ 192 kHz the signal already has enough temporal resolution; the
//! sdroege reference returns `None` for the upsampler, and this struct reduces
//! to a raw-sample scanner.
//!
//! ## f64 vs f32
//!
//! The sdroege reference uses `f32` arithmetic throughout for speed; its own
//! tests accept a tolerance of `abs ≤ 4 × 10⁻⁶`.  This analytics
//! implementation uses `f64` everywhere for maximum precision, at the cost of
//! ~2× the arithmetic throughput — acceptable for offline analysis.

use std::f64::consts::PI;

// ─────────────────────────────────────────────────────────────────────────────
// Sample peak
// ─────────────────────────────────────────────────────────────────────────────

/// Per-channel sample peak (linear, ≥ 0).
///
/// Returns `(peak_l, peak_r)`.  If either slice is empty the corresponding
/// value is `0.0`.
pub fn sample_peak(l: &[f64], r: &[f64]) -> (f64, f64) {
    let pk = |ch: &[f64]| ch.iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);
    (pk(l), pk(r))
}

// ─────────────────────────────────────────────────────────────────────────────
// EBU R128 polyphase true-peak interpolator
// ─────────────────────────────────────────────────────────────────────────────

const TAPS: usize = 48;

/// Build the flat polyphase coefficient array of length `TAPS`.
///
/// `coeffs[j * factor + p]` is the sub-filter coefficient for input tap `j`
/// and output phase `p`.
fn build_coefficients(factor: usize) -> Vec<f64> {
    let mut h = vec![0.0_f64; TAPS];
    let window = TAPS as f64; // 48.0  (sdroege convention, NOT TAPS − 1)
    for j in 0..TAPS {
        let w = 0.5 * (1.0 - f64::cos(2.0 * PI * j as f64 / window));
        let m = j as f64 - window / 2.0; // j − 24.0
        let sinc = if m.abs() < 1e-6 {
            1.0 // sinc(0) = 1
        } else {
            let arg = m * PI / factor as f64;
            arg.sin() / arg
        };
        h[j] = w * sinc;
    }
    h
}

/// Select the interpolation factor from the sample rate.
///
/// Matches the rule in `sdroege/ebur128 src/true_peak.rs`:
/// `rate < 96_000 → 4`,  `rate < 192_000 → 2`,  otherwise `1`.
fn factor_for_rate(rate: u32) -> usize {
    if rate < 96_000 { 4 } else if rate < 192_000 { 2 } else { 1 }
}

/// Streaming EBU R128 / ITU BS.1770-4 true-peak interpolator.
///
/// Call [`push`] with successive audio blocks (any block size ≥ 1).  The
/// running per-channel true peak is available at any time via [`peak_l`] /
/// [`peak_r`].
///
/// The filter state (history ring buffer) persists across calls, so
/// chunked results are bit-identical to processing the concatenated signal
/// in a single call.
///
/// [`push`]: EburTruePeak::push
/// [`peak_l`]: EburTruePeak::peak_l
/// [`peak_r`]: EburTruePeak::peak_r
pub struct EburTruePeak {
    factor: usize,
    active_taps: usize, // = TAPS / factor
    /// Flat polyphase coefficients, length TAPS.
    /// `coeffs[j * factor + p]` = coefficient for tap `j`, phase `p`.
    coeffs: Vec<f64>,
    /// Ring buffer — newest sample at `hist[head]`.
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    /// Index where the *next* sample will be written.
    head: usize,
    peak_l: f64,
    peak_r: f64,
    /// Count of interpolated outputs (including raw samples) that exceeded
    /// 1.0 (0 dBTP) during streaming.
    pub tp_overs: u32,
    /// Runs of consecutive outputs over 0 dBTP (either channel): one event
    /// per run however long.
    pub tp_over_events: u32,
    in_over: bool,
    /// Peak (both channels) since the last `take_block_peak`.
    block_peak: f64,
}

impl EburTruePeak {
    pub fn new(sample_rate: u32) -> Self {
        let factor = factor_for_rate(sample_rate);
        let active_taps = TAPS / factor;
        let coeffs = build_coefficients(factor);
        Self {
            factor,
            active_taps,
            coeffs,
            hist_l: vec![0.0; active_taps],
            hist_r: vec![0.0; active_taps],
            head: 0,
            peak_l: 0.0,
            peak_r: 0.0,
            tp_overs: 0,
            tp_over_events: 0,
            in_over: false,
            block_peak: 0.0,
        }
    }

    /// Push a block of planar samples.  Both slices are processed up to
    /// `min(l.len(), r.len())` samples.
    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        let n = l.len().min(r.len());
        for i in 0..n {
            self.push_one(l[i], r[i]);
        }
    }

    #[inline]
    fn push_one(&mut self, sl: f64, sr: f64) {
        let at = self.active_taps;

        // Write new sample into ring at the current head position.
        self.hist_l[self.head] = sl;
        self.hist_r[self.head] = sr;

        // Raw sample peak (captures the signal at the exact sample grid).
        self.check_peak(sl.abs(), sr.abs());

        // Polyphase convolution: produce `factor` interpolated outputs per
        // input sample.  At factor = 1 the inner branch is skipped; only raw
        // samples are checked.
        if self.factor > 1 {
            for p in 0..self.factor {
                let mut ol = 0.0_f64;
                let mut or_ = 0.0_f64;
                for j in 0..at {
                    let c = self.coeffs[j * self.factor + p];
                    // Tap j → ring index: head is tap 0, going backwards.
                    let idx = (self.head + at - j) % at;
                    ol += self.hist_l[idx] * c;
                    or_ += self.hist_r[idx] * c;
                }
                self.check_peak(ol.abs(), or_.abs());
            }
        }

        // Advance head to the next slot.
        self.head = (self.head + 1) % at;
    }

    #[inline]
    fn check_peak(&mut self, al: f64, ar: f64) {
        if al > self.peak_l { self.peak_l = al; }
        if ar > self.peak_r { self.peak_r = ar; }
        let m = al.max(ar);
        if m > self.block_peak { self.block_peak = m; }
        let over = al > 1.0 || ar > 1.0;
        if over {
            self.tp_overs += 1;
            if !self.in_over { self.tp_over_events += 1; }
        }
        self.in_over = over;
    }

    /// The true peak (linear, both channels) since the previous call.
    pub fn take_block_peak(&mut self) -> f64 {
        std::mem::take(&mut self.block_peak)
    }

    /// Running true peak, left channel (linear, ≥ 0).
    #[cfg(test)]
    pub fn peak_l(&self) -> f64 { self.peak_l }

    /// Running true peak, right channel (linear, ≥ 0).
    #[cfg(test)]
    pub fn peak_r(&self) -> f64 { self.peak_r }

    /// Both channels as `(peak_l, peak_r)`.
    #[cfg(test)]
    pub fn peaks(&self) -> (f64, f64) { (self.peak_l, self.peak_r) }

    /// Max of left and right peaks (linear).
    pub fn peak_max(&self) -> f64 { self.peak_l.max(self.peak_r) }

    /// True peak in dBTP for left channel.
    pub fn peak_l_dbtp(&self) -> f64 { lin_to_db(self.peak_l) }

    /// True peak in dBTP for right channel.
    pub fn peak_r_dbtp(&self) -> f64 { lin_to_db(self.peak_r) }
}

/// Convert a linear amplitude to dBFS / dBTP.
#[inline]
pub fn lin_to_db(lin: f64) -> f64 {
    20.0 * (lin + 1e-300).log10()
}

// ─────────────────────────────────────────────────────────────────────────────
// Engine true peak (delegates to audio::converter::dsp::true_peak)
// ─────────────────────────────────────────────────────────────────────────────

/// Measure the true peak of a complete buffer using the engine's own
/// Lanczos-4 × 4 polyphase scanner.
///
/// Delegates to [`crate::audio::converter::dsp::true_peak::measure_true_peak`],
/// which uses a 4× over-sampling Lanczos-4 kernel with reflective edge padding.
/// Returns the peak as a linear amplitude (max over both channels).
pub fn engine_true_peak(l: &[f64], r: &[f64]) -> f64 {
    crate::audio::converter::dsp::true_peak::measure_true_peak(l, r)
}

/// Streaming wrapper for the engine's true-peak scanner.
///
/// [`crate::audio::converter::dsp::true_peak::TruePeakScan`] requires the
/// total sample count at construction time (for correct reflective edge
/// padding at the end of the signal).  This wrapper accumulates blocks into
/// a `Vec` and calls `measure_true_peak` on the concatenated buffer in
/// `finish()`.
///
/// Memory usage: O(N) in total signal length.  For offline analytics this
/// is acceptable; for bounded-memory streaming of a live signal use
/// `TruePeakScan` directly with a known length.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub struct EngineTruePeakStreamer {
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
}

#[allow(dead_code)] // kept for the analyzer views not wired yet
impl Default for EngineTruePeakStreamer {
    fn default() -> Self { Self::new() }
}

#[allow(dead_code)] // kept for the analyzer views not wired yet
impl EngineTruePeakStreamer {
    pub fn new() -> Self {
        Self { buf_l: Vec::new(), buf_r: Vec::new() }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.buf_l.extend_from_slice(l);
        self.buf_r.extend_from_slice(r);
    }

    /// Consume the streamer and return the measured true peak (linear, max
    /// over L and R).
    pub fn finish(self) -> f64 {
        crate::audio::converter::dsp::true_peak::measure_true_peak(&self.buf_l, &self.buf_r)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Clip analysis
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum number of clip-run start positions stored in
/// [`ClipReport::first_positions`].
pub const CLIP_POSITIONS_CAP: usize = 64;

/// Clip / over analysis results for a stereo signal.
///
/// "Rail run" definition: a run of ≥ 2 consecutive samples in the same
/// channel where every sample is within `1 × 10⁻⁹` of that channel's
/// absolute peak, provided the peak ≥ −0.5 dBFS (linear ≥ ≈ 0.9441).
/// This detects hard-clipped flat-top events while ignoring near-zero
/// signals that trivially hit their own "peak" constantly.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct ClipReport {
    /// Count of samples with |x| ≥ full-scale threshold.
    ///
    /// Threshold = `1.0 − LSB / 2` where `LSB = 2^(1 − bits)` for a known
    /// bit depth (`bits` argument to [`clip_report`]), or `1.0` when unknown.
    pub full_scale_samples: u64,

    /// Count of rail runs (length ≥ 2) across both channels combined.
    pub rail_runs: u32,

    /// Count of rail runs with length ≥ 17 samples (~0.35 ms at 48 kHz;
    /// audible as a hard-clipped flat top).
    pub runs_ge_17: u32,

    /// Length in samples of the longest single rail run found.
    pub longest_run: u32,

    /// Up to [`CLIP_POSITIONS_CAP`] earliest rail-run positions.
    /// Tuple: `(sample_offset, run_length, channel)` where 0 = L, 1 = R.
    pub first_positions: Vec<(u64, u32, u8)>,

    /// Count of samples with |x| > 1.0 (strict digital over), both channels.
    pub overs: u64,

    /// Count of EBU-interpolated outputs that exceeded 1.0 (0 dBTP), as
    /// produced by [`EburTruePeak`] during streaming.
    pub tp_overs: u32,
}

impl Default for ClipReport {
    fn default() -> Self {
        Self {
            full_scale_samples: 0,
            rail_runs: 0,
            runs_ge_17: 0,
            longest_run: 0,
            first_positions: Vec::new(),
            overs: 0,
            tp_overs: 0,
        }
    }
}

/// Compute a [`ClipReport`] for a complete stereo buffer in one shot.
///
/// * `bits` — known source bit depth (e.g. `Some(16)`, `Some(24)`).  When
///   `None`, the full-scale threshold is exactly `1.0`.
///
/// The clip analysis makes two passes: the first finds the per-channel
/// absolute peak (needed for the rail-run tolerance), the second counts events.
pub fn clip_report(l: &[f64], r: &[f64], bits: Option<u8>) -> ClipReport {
    let n = l.len().min(r.len());
    if n == 0 {
        return ClipReport::default();
    }

    // Full-scale threshold
    let fs_threshold = match bits {
        Some(b) if b >= 1 => {
            let lsb = 2.0_f64.powi(1 - b as i32);
            1.0 - lsb / 2.0
        }
        _ => 1.0,
    };

    // Rail-run minimum amplitude: −0.5 dBFS → 10^(−0.5/20)
    const RAIL_MIN_DBFS: f64 = -0.5;
    let rail_min = 10.0_f64.powf(RAIL_MIN_DBFS / 20.0);
    const RAIL_TOL: f64 = 1e-9;

    // Pass 1: per-channel peak
    let peak_l = l[..n].iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);
    let peak_r = r[..n].iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);

    // Pass 2: count events
    let mut report = ClipReport::default();

    for (ch_idx, (ch, peak)) in [(l, peak_l), (r, peak_r)].iter().enumerate() {
        let ch: &[f64] = &ch[..n];
        let peak = *peak;
        let active = peak >= rail_min;

        let mut run_start: Option<u64> = None;
        let mut run_len: u32 = 0;

        for (i, &s) in ch.iter().enumerate() {
            let a = s.abs();

            // Full-scale count
            if a >= fs_threshold {
                report.full_scale_samples += 1;
            }

            // Strict over count
            if a > 1.0 {
                report.overs += 1;
            }

            // Rail run detection
            if active && (peak - a).abs() <= RAIL_TOL {
                if run_start.is_none() {
                    run_start = Some(i as u64);
                    run_len = 1;
                } else {
                    run_len += 1;
                }
            } else {
                // Flush run if it just ended
                if let Some(start) = run_start.take() {
                    if run_len >= 2 {
                        record_run(
                            &mut report,
                            start,
                            run_len,
                            ch_idx as u8,
                        );
                    }
                }
                run_len = 0;
            }
        }
        // Flush any trailing run
        if let Some(start) = run_start.take() {
            if run_len >= 2 {
                record_run(&mut report, start, run_len, ch_idx as u8);
            }
        }
    }

    report
}

#[inline]
fn record_run(report: &mut ClipReport, start: u64, len: u32, ch: u8) {
    report.rail_runs += 1;
    if len >= 17 {
        report.runs_ge_17 += 1;
    }
    if len > report.longest_run {
        report.longest_run = len;
    }
    if report.first_positions.len() < CLIP_POSITIONS_CAP {
        report.first_positions.push((start, len, ch));
    }
}

/// Streaming clip analyzer.
///
/// Accumulates the full signal (to know the per-channel peak before the
/// rail-run pass) and runs the EBU true-peak interpolator in tandem for
/// `tp_overs` counting.  Call [`push`] repeatedly, then [`finish`] once.
///
/// [`push`]: ClipAnalyzer::push
/// [`finish`]: ClipAnalyzer::finish
#[cfg(test)]
pub struct ClipAnalyzer {
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    ebur: EburTruePeak,
    bits: Option<u8>,
}

#[cfg(test)]
impl ClipAnalyzer {
    pub fn new(sample_rate: u32, bits: Option<u8>) -> Self {
        Self {
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            ebur: EburTruePeak::new(sample_rate),
            bits,
        }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.buf_l.extend_from_slice(l);
        self.buf_r.extend_from_slice(r);
        self.ebur.push(l, r);
    }

    /// Consume the analyzer, run the two-pass clip analysis, and return the
    /// [`ClipReport`].
    pub fn finish(self) -> ClipReport {
        let tp_overs = self.ebur.tp_overs;
        let mut report = clip_report(&self.buf_l, &self.buf_r, self.bits);
        report.tp_overs = tp_overs;
        report
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EburTruePeak ─────────────────────────────────────────────────────────

    /// Generate `n` samples of `amp · sin(2π f/fs · k + phase_rad)`.
    fn sine(n: usize, fs: u32, freq: f64, amp: f64, phase_rad: f64) -> Vec<f64> {
        (0..n)
            .map(|k| amp * f64::sin(2.0 * PI * freq / fs as f64 * k as f64 + phase_rad))
            .collect()
    }

    /// A 45°-phase-shifted sine at fs/4 has all samples at ±1/√2 but a true
    /// peak of 1.0.  The ratio is 20·log10(√2) ≈ +3.01 dB above sample peak.
    #[test]
    fn fs4_45deg_sine_tp_is_3db_above_sp() {
        let fs = 48_000u32;
        let n = 2048;
        let amp = 1.0_f64;
        // Phase 45° = π/4
        let sig = sine(n, fs, fs as f64 / 4.0, amp, PI / 4.0);
        let sp = sig.iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);

        let mut ebur = EburTruePeak::new(fs);
        ebur.push(&sig, &sig);
        let tp = ebur.peak_l();

        let diff_db = 20.0 * (tp / sp).log10();
        assert!(
            (diff_db - 3.01).abs() < 0.15,
            "expected TP ≈ SP + 3.01 dB, got {:.3} dB (SP={:.4} TP={:.4})",
            diff_db, sp, tp
        );
    }

    /// Streaming (chunked) must produce the same result as one-shot.
    #[test]
    fn ebur_streaming_equals_oneshot() {
        let fs = 48_000u32;
        let n = 4096;
        let sig = sine(n, fs, 997.0, 0.9, 0.0);

        // One-shot
        let mut ebur_one = EburTruePeak::new(fs);
        ebur_one.push(&sig, &sig);
        let (pl_one, pr_one) = ebur_one.peaks();

        // Chunked (various chunk sizes including odd ones)
        let mut ebur_chunk = EburTruePeak::new(fs);
        let mut i = 0;
        let chunk = 97;
        while i < n {
            let end = (i + chunk).min(n);
            ebur_chunk.push(&sig[i..end], &sig[i..end]);
            i = end;
        }
        let (pl_c, pr_c) = ebur_chunk.peaks();

        assert!(
            (pl_one - pl_c).abs() < 1e-12,
            "L: one-shot={:.8} chunked={:.8}", pl_one, pl_c
        );
        assert!(
            (pr_one - pr_c).abs() < 1e-12,
            "R: one-shot={:.8} chunked={:.8}", pr_one, pr_c
        );
    }

    /// Empty push must not panic.
    #[test]
    fn ebur_empty_push() {
        let mut e = EburTruePeak::new(44_100);
        e.push(&[], &[]);
        assert_eq!(e.peaks(), (0.0, 0.0));
    }

    /// At ≥192 kHz the factor is 1 (no upsampling); raw sample peak returned.
    #[test]
    fn ebur_192k_factor_one() {
        assert_eq!(factor_for_rate(192_000), 1);
        assert_eq!(factor_for_rate(384_000), 1);
        let sig = vec![0.5_f64; 64];
        let mut e = EburTruePeak::new(192_000);
        e.push(&sig, &sig);
        assert!((e.peak_l() - 0.5).abs() < 1e-12);
    }

    // ── Sample peak ──────────────────────────────────────────────────────────

    #[test]
    fn sample_peak_basic() {
        let l = vec![0.0, -0.8, 0.3];
        let r = vec![0.5, 0.2, -0.1];
        let (pl, pr) = sample_peak(&l, &r);
        assert!((pl - 0.8).abs() < 1e-15);
        assert!((pr - 0.5).abs() < 1e-15);
    }

    #[test]
    fn sample_peak_empty() {
        assert_eq!(sample_peak(&[], &[]), (0.0, 0.0));
    }

    // ── Clip report ───────────────────────────────────────────────────────────

    /// Four consecutive samples at exactly ±1.0 constitute one rail run of 4.
    #[test]
    fn clip_rail_run_detected() {
        let l = vec![1.0, 1.0, 1.0, 1.0, 0.5, 0.3];
        let r = vec![0.0; 6];
        let rep = clip_report(&l, &r, None);
        assert_eq!(rep.rail_runs, 1, "one run expected");
        assert_eq!(rep.longest_run, 4);
        assert_eq!(rep.first_positions[0].1, 4);
        assert_eq!(rep.first_positions[0].2, 0); // L channel
    }

    /// Runs below −0.5 dBFS must NOT be counted.
    #[test]
    fn clip_low_peak_no_rail_run() {
        // Peak 0.1 is far below −0.5 dBFS threshold
        let l = vec![0.1, 0.1, 0.1, 0.1];
        let r = vec![0.0; 4];
        let rep = clip_report(&l, &r, None);
        assert_eq!(rep.rail_runs, 0);
    }

    /// Strict overs (|x| > 1.0) are counted separately.
    #[test]
    fn clip_overs_counted() {
        let l = vec![1.01, 0.99, 1.001];
        let r = vec![0.0; 3];
        let rep = clip_report(&l, &r, None);
        assert_eq!(rep.overs, 2); // 1.01 and 1.001
    }

    /// 16-bit threshold: 1.0 − 1/65536 ≈ 0.99998.
    #[test]
    fn clip_16bit_threshold() {
        let threshold = 1.0 - 1.0 / 65536.0;
        let l = vec![threshold, threshold - 1e-10, 1.0];
        let r = vec![0.0; 3];
        let rep = clip_report(&l, &r, Some(16));
        // samples at `threshold` and `1.0` exceed or equal the threshold
        assert!(rep.full_scale_samples >= 2);
    }

    /// The streaming ClipAnalyzer must agree with the one-shot clip_report.
    #[test]
    fn clip_analyzer_streaming_matches_oneshot() {
        let l: Vec<f64> = (0..512)
            .map(|i| f64::sin(2.0 * PI * 997.0 * i as f64 / 48_000.0))
            .collect();
        let r = l.clone();
        let oneshot = clip_report(&l, &r, Some(24));

        let mut analyzer = ClipAnalyzer::new(48_000, Some(24));
        let mut i = 0;
        while i < l.len() {
            let end = (i + 63).min(l.len());
            analyzer.push(&l[i..end], &r[i..end]);
            i = end;
        }
        let streaming = analyzer.finish();

        assert_eq!(oneshot.rail_runs, streaming.rail_runs);
        assert_eq!(oneshot.overs, streaming.overs);
        assert_eq!(oneshot.full_scale_samples, streaming.full_scale_samples);
        assert_eq!(oneshot.longest_run, streaming.longest_run);
    }

    #[test]
    fn tp_over_events_count_runs() {
        let mut tp = EburTruePeak::new(192_000); // factor 1: raw samples only
        let x = [0.5, 1.2, 1.3, 0.5, 0.2, 1.1, 0.0];
        tp.push(&x, &[0.0; 7]);
        assert_eq!(tp.tp_overs, 3);
        assert_eq!(tp.tp_over_events, 2);
    }
}
