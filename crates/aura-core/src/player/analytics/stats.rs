//! Signal statistics: DC offset, RMS, crest factor, stereo correlation,
//! balance, mid/side energy, bit-depth detection, and a thin wrapper around
//! the engine's own source-analysis pass.
//!
//! All computation is in f64; results are returned as f64 (dBFS, linear, or
//! raw ratios).
//!
//! # PLR (Peak-to-Loudness Ratio)
//!
//! PLR = true_peak_dBTP − integrated_LUFS.  Both operands come from other
//! analytics modules (peaks:: and loudness::), so this module accepts them as
//! optional inputs rather than re-measuring them.  Pass both to get PLR;
//! either missing yields `None`.
//!
//! # Bit-depth detection
//!
//! The detector tests first differences `d[k] = x[k+1] − x[k]` against
//! known PCM grids (16, 20, 24 bit).  A grid matches when ≥ 99 % of non-zero
//! differences are within a relative + absolute tolerance of the nearest grid
//! step.  The coarsest matching grid is reported as the detected bit depth.
//! This mirrors the engine's `lab::stats::analyze` grid-16 test and extends
//! it to 20-bit and 24-bit containers.
//!
//! # Engine stats wrapper
//!
//! [`engine_analyze`] delegates to
//! [`crate::audio::converter::dsp::lab::stats::analyze`], which computes peak,
//! a 65536-bin amplitude histogram, the 16-bit grid verdict, and ENOB.

#[cfg(test)]
use std::sync::atomic::AtomicBool;

// ─────────────────────────────────────────────────────────────────────────────
// Per-channel statistics
// ─────────────────────────────────────────────────────────────────────────────

/// Statistics for a single channel.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct ChannelStats {
    /// Mean sample value (DC offset), linear.
    pub dc_offset: f64,
    /// RMS level: `20 · log₁₀( √(Σ xᵢ² / N) )` in dBFS.
    /// `NEG_INFINITY` for a silent channel.
    pub rms_dbfs: f64,
    /// Crest factor: `peak_dbfs − rms_dbfs` in dB.
    /// `0.0` when rms is −∞.
    pub crest_factor_db: f64,
    /// Minimum sample value (most negative).
    pub min_val: f64,
    /// Maximum sample value (most positive).
    pub max_val: f64,
    /// `max( |min_val|, |max_val| )` — absolute peak, linear.
    pub peak_lin: f64,
    /// `20 · log₁₀( peak_lin )` in dBFS.  `NEG_INFINITY` for silence.
    pub peak_dbfs: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Stereo statistics
// ─────────────────────────────────────────────────────────────────────────────

/// Full stereo + joint statistics.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct StereoStats {
    pub l: ChannelStats,
    pub r: ChannelStats,

    /// Pearson correlation coefficient between L and R samples, in [−1, 1].
    /// `0.0` when either channel is silent.
    pub correlation: f64,

    /// L/R balance: `rms_l_dbfs − rms_r_dbfs` in dB.
    /// Positive = L is louder.  `0.0` when both are −∞.
    pub balance_db: f64,

    /// RMS of the mid signal `(L + R) / 2` in dBFS.
    pub mid_rms_dbfs: f64,

    /// RMS of the side signal `(L − R) / 2` in dBFS.
    pub side_rms_dbfs: f64,

    /// `mid_rms_dbfs − side_rms_dbfs` in dB.  Positive = more mid than side
    /// (typical stereo music).  `NAN` when either component is silent.
    pub mid_side_ratio_db: f64,

    /// Detected source bit depth (16, 20, or 24) based on first-difference
    /// grid analysis, or `None` when the signal does not align to any
    /// known PCM grid (continuous-resolution or dithered 32-bit float).
    pub bit_depth: Option<u8>,

    /// Peak-to-Loudness Ratio = true_peak_dBTP − integrated_LUFS.
    /// `None` unless both inputs are provided to [`compute_stats`].
    pub plr_db: Option<f64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Computation
// ─────────────────────────────────────────────────────────────────────────────

/// Compute full stereo statistics for a complete signal.
///
/// * `true_peak_dbtp` — from `peaks::EburTruePeak` or `peaks::engine_true_peak`
///   (already converted to dBTP via `20·log₁₀`), used for PLR.
/// * `integrated_lufs` — from `loudness::LoudnessMeter`, used for PLR.
///
/// Both are `Option<f64>`; when either is `None`, `plr_db` is also `None`.
pub fn compute_stats(
    l: &[f64],
    r: &[f64],
    true_peak_dbtp: Option<f64>,
    integrated_lufs: Option<f64>,
) -> StereoStats {
    let n = l.len().min(r.len());

    let ls = channel_stats(&l[..n]);
    let rs = channel_stats(&r[..n]);

    let correlation = if n == 0 { 0.0 } else { pearson(&l[..n], &r[..n]) };

    let balance_db = if ls.rms_dbfs.is_finite() && rs.rms_dbfs.is_finite() {
        ls.rms_dbfs - rs.rms_dbfs
    } else {
        0.0
    };

    let (mid_rms_dbfs, side_rms_dbfs) = mid_side_rms(&l[..n], &r[..n]);
    let mid_side_ratio_db = if mid_rms_dbfs.is_finite() && side_rms_dbfs.is_finite() {
        mid_rms_dbfs - side_rms_dbfs
    } else {
        f64::NAN
    };

    let bit_depth = detect_bit_depth(&l[..n], &r[..n]);

    let plr_db = match (true_peak_dbtp, integrated_lufs) {
        (Some(tp), Some(lufs)) => Some(tp - lufs),
        _ => None,
    };

    StereoStats {
        l: ls,
        r: rs,
        correlation,
        balance_db,
        mid_rms_dbfs,
        side_rms_dbfs,
        mid_side_ratio_db,
        bit_depth,
        plr_db,
    }
}

/// Streaming statistics accumulator.
///
/// Accumulates the full signal and computes on [`finish`].
///
/// [`finish`]: StatsStreamer::finish
#[cfg(test)]
pub struct StatsStreamer {
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    true_peak_dbtp: Option<f64>,
    integrated_lufs: Option<f64>,
}

#[cfg(test)]
impl StatsStreamer {
    pub fn new() -> Self {
        Self { buf_l: Vec::new(), buf_r: Vec::new(), true_peak_dbtp: None, integrated_lufs: None }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.buf_l.extend_from_slice(l);
        self.buf_r.extend_from_slice(r);
    }

    /// Set the true peak (dBTP) for PLR computation.  May be called at any
    /// time before [`finish`].
    pub fn set_true_peak_dbtp(&mut self, tp: f64) { self.true_peak_dbtp = Some(tp); }

    /// Set the integrated loudness (LUFS) for PLR computation.  May be called
    /// at any time before [`finish`].
    pub fn set_integrated_lufs(&mut self, lufs: f64) { self.integrated_lufs = Some(lufs); }

    pub fn finish(self) -> StereoStats {
        compute_stats(&self.buf_l, &self.buf_r, self.true_peak_dbtp, self.integrated_lufs)
    }
}

#[cfg(test)]
impl Default for StatsStreamer {
    fn default() -> Self { Self::new() }
}

// ─────────────────────────────────────────────────────────────────────────────
// Engine stats wrapper
// ─────────────────────────────────────────────────────────────────────────────

/// Re-export of the engine's per-file source statistics.
#[cfg(test)]
pub use crate::audio::converter::dsp::lab::stats::SourceStats as EngineSourceStats;

/// Run the engine's source-analysis pass on a signal.
///
/// Delegates to [`crate::audio::converter::dsp::lab::stats::analyze`], which
/// computes:
/// - `peak_lin`, `peak_dbfs`
/// - 65536-bin amplitude histogram
/// - `grid16`: whether the signal is quantized to the 16-bit PCM grid
/// - `enob`: effective number of bits from quiet frames
///
/// Returns `None` on empty input or when the `cancel` flag fires.
#[cfg(test)]
pub fn engine_analyze(
    l: &[f64],
    r: &[f64],
    sample_rate: u32,
    cancel: &AtomicBool,
) -> Option<EngineSourceStats> {
    crate::audio::converter::dsp::lab::stats::analyze(l, r, sample_rate, cancel)
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers
// ─────────────────────────────────────────────────────────────────────────────

fn channel_stats(ch: &[f64]) -> ChannelStats {
    let n = ch.len();
    if n == 0 {
        return ChannelStats {
            dc_offset: 0.0,
            rms_dbfs: f64::NEG_INFINITY,
            crest_factor_db: 0.0,
            min_val: 0.0,
            max_val: 0.0,
            peak_lin: 0.0,
            peak_dbfs: f64::NEG_INFINITY,
        };
    }

    let mut sum = 0.0_f64;
    let mut sum_sq = 0.0_f64;
    let mut min_val = ch[0];
    let mut max_val = ch[0];

    for &x in ch {
        sum += x;
        sum_sq += x * x;
        if x < min_val { min_val = x; }
        if x > max_val { max_val = x; }
    }

    let dc_offset = sum / n as f64;
    let rms_lin = (sum_sq / n as f64).sqrt();
    let rms_dbfs = if rms_lin > 0.0 { 20.0 * rms_lin.log10() } else { f64::NEG_INFINITY };
    let peak_lin = min_val.abs().max(max_val.abs());
    let peak_dbfs = if peak_lin > 0.0 { 20.0 * peak_lin.log10() } else { f64::NEG_INFINITY };
    let crest_factor_db = if rms_dbfs.is_finite() { peak_dbfs - rms_dbfs } else { 0.0 };

    ChannelStats { dc_offset, rms_dbfs, crest_factor_db, min_val, max_val, peak_lin, peak_dbfs }
}

/// Pearson correlation coefficient.
fn pearson(l: &[f64], r: &[f64]) -> f64 {
    let n = l.len().min(r.len());
    if n == 0 { return 0.0; }
    let nf = n as f64;

    let sum_l: f64 = l[..n].iter().sum();
    let sum_r: f64 = r[..n].iter().sum();
    let mean_l = sum_l / nf;
    let mean_r = sum_r / nf;

    let mut cov = 0.0_f64;
    let mut var_l = 0.0_f64;
    let mut var_r = 0.0_f64;

    for i in 0..n {
        let dl = l[i] - mean_l;
        let dr = r[i] - mean_r;
        cov += dl * dr;
        var_l += dl * dl;
        var_r += dr * dr;
    }

    let denom = (var_l * var_r).sqrt();
    if denom < 1e-300 { 0.0 } else { cov / denom }
}

/// Compute mid and side RMS levels in dBFS.
/// mid = (L + R) / 2,  side = (L − R) / 2.
fn mid_side_rms(l: &[f64], r: &[f64]) -> (f64, f64) {
    let n = l.len().min(r.len());
    if n == 0 {
        return (f64::NEG_INFINITY, f64::NEG_INFINITY);
    }

    let mut sum_m = 0.0_f64;
    let mut sum_s = 0.0_f64;
    for i in 0..n {
        let m = (l[i] + r[i]) * 0.5;
        let s = (l[i] - r[i]) * 0.5;
        sum_m += m * m;
        sum_s += s * s;
    }

    let to_db = |ss: f64| {
        let rms = (ss / n as f64).sqrt();
        if rms > 0.0 { 20.0 * rms.log10() } else { f64::NEG_INFINITY }
    };
    (to_db(sum_m), to_db(sum_s))
}

/// Detect the source PCM bit depth from first differences.
///
/// Tests the sequence `d[k] = ch[k+1] − ch[k]` against the grids for 16-bit
/// (step = 2^−15 = 1/32768), 20-bit (step = 2^−19), and 24-bit (step = 2^−23).
/// Returns the **coarsest** matching grid, i.e., the first of 16, 20, 24 that
/// has ≥ 99 % of non-zero differences aligned to within `1×10⁻⁹ · |d| + 1×10⁻¹⁵`.
/// Returns `None` when no grid matches (float-resolution or dithered material).
///
/// A DC-shifted signal is handled correctly because first differences cancel
/// any constant offset.
fn detect_bit_depth(l: &[f64], r: &[f64]) -> Option<u8> {
    const GRIDS: &[(u8, f64)] = &[
        (16, 1.0 / 32_768.0),         // 2^−15
        (20, 1.0 / 524_288.0),        // 2^−19
        (24, 1.0 / 8_388_608.0),      // 2^−23
    ];
    const MATCH_THRESHOLD: f64 = 0.99;
    const TARGET_DIFFS: usize = 1_000_000;

    for &(bits, step) in GRIDS {
        if grid_matches(l, r, step, TARGET_DIFFS, MATCH_THRESHOLD) {
            return Some(bits);
        }
    }
    None
}

fn grid_matches(
    l: &[f64],
    r: &[f64],
    step: f64,
    target: usize,
    threshold: f64,
) -> bool {
    // Total first-differences across both channels
    let total_diffs =
        l.len().saturating_sub(1) + r.len().saturating_sub(1);
    if total_diffs < 100 {
        return false;
    }
    let stride = (total_diffs / target).max(1);

    let mut on_grid: u64 = 0;
    let mut checked: u64 = 0;

    for ch in [l, r] {
        let nd = ch.len().saturating_sub(1);
        let mut k = 0usize;
        while k < nd {
            let d = ch[k + 1] - ch[k];
            // Skip exact zeros (DC, silence): uninformative for grid detection
            if d != 0.0 {
                let nearest = (d / step).round() * step;
                let err = (d - nearest).abs();
                let tol = 1e-9 * d.abs() + 1e-15;
                if err <= tol {
                    on_grid += 1;
                }
                checked += 1;
            }
            k += stride;
        }
    }

    checked >= 100 && (on_grid as f64 / checked as f64) >= threshold
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn sine(n: usize, amp: f64) -> Vec<f64> {
        (0..n)
            .map(|i| amp * f64::sin(2.0 * PI * 997.0 * i as f64 / 48_000.0))
            .collect()
    }

    // ── ChannelStats ─────────────────────────────────────────────────────────

    #[test]
    fn dc_offset_detected() {
        let l = vec![0.1, 0.2, 0.3]; // mean = 0.2
        let r = vec![0.0; 3];
        let st = compute_stats(&l, &r, None, None);
        assert!((st.l.dc_offset - 0.2).abs() < 1e-12);
    }

    #[test]
    fn rms_dbfs_for_unit_sine() {
        // A sine of amplitude A has RMS = A / √2, so RMS dBFS = 20·log10(A/√2).
        let n = 48_000;
        let amp = 0.5_f64;
        let sig = sine(n, amp);
        let st = compute_stats(&sig, &sig, None, None);
        let expected = 20.0 * (amp / 2.0_f64.sqrt()).log10();
        assert!(
            (st.l.rms_dbfs - expected).abs() < 0.1,
            "rms_dbfs={:.3} expected≈{:.3}", st.l.rms_dbfs, expected
        );
    }

    #[test]
    fn peak_lin_is_max_abs() {
        let l = vec![0.0, -0.9, 0.7];
        let r = vec![0.5, 0.3, -0.1];
        let st = compute_stats(&l, &r, None, None);
        assert!((st.l.peak_lin - 0.9).abs() < 1e-15);
        assert!((st.r.peak_lin - 0.5).abs() < 1e-15);
    }

    #[test]
    fn crest_factor_correct() {
        let l = vec![0.0, -0.9, 0.7];
        let r = vec![0.0; 3];
        let st = compute_stats(&l, &r, None, None);
        let expected = st.l.peak_dbfs - st.l.rms_dbfs;
        assert!((st.l.crest_factor_db - expected).abs() < 1e-9);
    }

    #[test]
    fn empty_channel_is_neg_inf() {
        let st = compute_stats(&[], &[], None, None);
        assert!(st.l.rms_dbfs == f64::NEG_INFINITY);
        assert!(st.r.peak_dbfs == f64::NEG_INFINITY);
    }

    // ── Stereo metrics ────────────────────────────────────────────────────────

    /// Identical L and R must have correlation = 1.0.
    #[test]
    fn correlation_identical() {
        let sig = sine(4096, 0.7);
        let st = compute_stats(&sig, &sig, None, None);
        assert!((st.correlation - 1.0).abs() < 1e-9);
    }

    /// Inverted channels must have correlation = −1.0.
    #[test]
    fn correlation_inverted() {
        let sig = sine(4096, 0.7);
        let neg: Vec<f64> = sig.iter().map(|&x| -x).collect();
        let st = compute_stats(&sig, &neg, None, None);
        assert!((st.correlation + 1.0).abs() < 1e-9);
    }

    /// Mono signal: mid = signal, side = 0 → mid_rms = rms, side_rms = -∞.
    #[test]
    fn mid_side_mono() {
        let sig = sine(4096, 0.7);
        let st = compute_stats(&sig, &sig, None, None);
        // side is exactly (L - R)/2 = 0
        assert!(st.side_rms_dbfs == f64::NEG_INFINITY);
    }

    /// Perfectly out-of-phase: mid = 0, side = signal → mid_rms = -∞.
    #[test]
    fn mid_side_antiphase() {
        let sig = sine(4096, 0.7);
        let neg: Vec<f64> = sig.iter().map(|&x| -x).collect();
        let st = compute_stats(&sig, &neg, None, None);
        assert!(st.mid_rms_dbfs == f64::NEG_INFINITY);
    }

    /// Equal amplitude L and R at the same level: balance_db ≈ 0.
    #[test]
    fn balance_equal_channels() {
        let sig = sine(4096, 0.6);
        let st = compute_stats(&sig, &sig, None, None);
        assert!(st.balance_db.abs() < 1e-9);
    }

    // ── PLR ──────────────────────────────────────────────────────────────────

    #[test]
    fn plr_computed_when_both_provided() {
        let sig = sine(4096, 0.5);
        let st = compute_stats(&sig, &sig, Some(-0.5), Some(-14.0));
        let plr = st.plr_db.expect("PLR must be Some");
        assert!((plr - (-0.5 - (-14.0))).abs() < 1e-12);
    }

    #[test]
    fn plr_none_when_missing_input() {
        let sig = sine(4096, 0.5);
        let st = compute_stats(&sig, &sig, None, Some(-14.0));
        assert!(st.plr_db.is_none());
    }

    // ── Bit-depth detection ───────────────────────────────────────────────────

    /// A ramp quantized to the 16-bit grid must be detected as 16-bit.
    #[test]
    fn detect_16bit_ramp() {
        let n = 2_100_000usize;
        let l: Vec<f64> = (0..n)
            .map(|i| {
                let k = (i as i32 % 65536 - 32768) as i16;
                k as f64 / 32_768.0
            })
            .collect();
        let r = l.clone();
        let st = compute_stats(&l, &r, None, None);
        assert_eq!(
            st.bit_depth,
            Some(16),
            "expected 16-bit detected, got {:?}", st.bit_depth
        );
    }

    /// High-resolution float noise must not match any grid.
    #[test]
    fn detect_none_for_float_noise() {
        let n = 2_100_000usize;
        let mut state: u64 = 0x5a4bcdef12345678;
        let l: Vec<f64> = (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
            })
            .collect();
        let r = l.clone();
        let st = compute_stats(&l, &r, None, None);
        assert!(
            st.bit_depth.is_none(),
            "float noise should not match any grid, got {:?}", st.bit_depth
        );
    }

    // ── Streaming == one-shot ─────────────────────────────────────────────────

    #[test]
    fn streaming_matches_oneshot() {
        let sig = sine(8192, 0.75);
        let neg: Vec<f64> = sig.iter().map(|&x| -x * 0.9).collect();

        let oneshot = compute_stats(&sig, &neg, Some(-0.6), Some(-13.5));

        let mut streamer = StatsStreamer::new();
        streamer.set_true_peak_dbtp(-0.6);
        streamer.set_integrated_lufs(-13.5);
        let mut i = 0;
        while i < sig.len() {
            let end = (i + 512).min(sig.len());
            streamer.push(&sig[i..end], &neg[i..end]);
            i = end;
        }
        let streaming = streamer.finish();

        assert!((oneshot.l.rms_dbfs - streaming.l.rms_dbfs).abs() < 1e-10);
        assert!((oneshot.r.rms_dbfs - streaming.r.rms_dbfs).abs() < 1e-10);
        assert!((oneshot.correlation - streaming.correlation).abs() < 1e-10);
        assert_eq!(oneshot.plr_db, streaming.plr_db);
        assert_eq!(oneshot.bit_depth, streaming.bit_depth);
    }

    // ── Engine wrapper ────────────────────────────────────────────────────────

    #[test]
    fn engine_analyze_basic() {
        use std::sync::atomic::AtomicBool;
        let cancel = AtomicBool::new(false);
        let n = 200_000usize;
        let l: Vec<f64> = (0..n)
            .map(|i| 0.5 * f64::sin(2.0 * PI * 1000.0 * i as f64 / 48_000.0))
            .collect();
        let r = l.clone();
        let result = engine_analyze(&l, &r, 48_000, &cancel);
        assert!(result.is_some(), "engine_analyze must return Some for valid input");
        let s = result.unwrap();
        assert!((s.peak_lin - 0.5).abs() < 1e-4, "peak_lin={}", s.peak_lin);
    }
}
