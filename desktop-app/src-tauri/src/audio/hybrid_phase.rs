//! ═══════════════════════════════════════════════════════════════════
//! AuraEngine — Hybrid-Phase Transient Blending Engine v2
//! ═══════════════════════════════════════════════════════════════════
//!
//! Continuous envelope follower replaces discrete transient detection.
//! Two-pass algorithm ensures complete coverage of every transient:
//!
//! 1. Forward pass: instant attack, configurable hold + exponential decay
//! 2. Backward pass: lookahead extends envelope BEFORE each onset
//!
//! Architecture:
//! ```
//!   Source audio → multi-band envelope follower → blend envelope
//!
//!   Convolved audio:
//!     y_linear  ──┐
//!     y_minimum ──┼── blend: y = env·y_min + (1-env)·y_lin
//!     envelope  ──┘
//! ```

use std::f64::consts::PI;
use std::path::Path;

/// Per-sample blending envelope for hybrid-phase output mixing.
///
/// 0.0 = use linear phase, 1.0 = use minimum phase
pub struct BlendEnvelope {
    pub envelope: Vec<f32>,
    /// Analysis envelope at reduced rate (kept for diagnostics / future sidecar use)
    #[allow(dead_code)]
    pub analysis_envelope: Vec<f64>,
    /// Sample rate of analysis_envelope (typically 100 Hz)
    #[allow(dead_code)]
    pub analysis_sr: f64,
}

impl BlendEnvelope {
    /// Get the blending factor for a specific output sample.
    #[inline]
    pub fn get(&self, idx: usize) -> f32 {
        if idx < self.envelope.len() {
            self.envelope[idx]
        } else {
            0.0
        }
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.envelope.len()
    }
}

/// Load a pre-computed onset envelope from a JSON sidecar file.
///
/// Looks for `<source_stem>.onset_envelope.json` next to the source file.
/// The sidecar is written by `hpss_native::generate_and_save`, which marks it
/// with the detector version and a fingerprint of the audio it was computed
/// from — the file name alone does not say which audio that was.
///
/// Returns `Some(BlendEnvelope)` if found and valid, `None` otherwise.
/// Catmull-Rom interpolation of the analysis-rate envelope at output sample
/// `i`. Single source of truth for BOTH the materialized envelope
/// (`load_external_envelope`) and the on-the-fly evaluation used by the
/// segmented giant path (which cannot afford the multi-GB full-rate vector).
/// Clamped to [0, 1] — Catmull-Rom can overshoot a few percent near steep
/// attacks.
#[inline]
pub fn catmull_env_at(analysis: &[f64], frames_to_output: f64, i: usize) -> f32 {
    let env_at = |idx: isize| -> f64 {
        if idx < 0 || idx as usize >= analysis.len() {
            0.0
        } else {
            analysis[idx as usize]
        }
    };
    let frame_pos = i as f64 / frames_to_output;
    let i1 = frame_pos as isize;
    let t = frame_pos - i1 as f64;
    let p0 = env_at(i1 - 1);
    let p1 = env_at(i1);
    let p2 = env_at(i1 + 1);
    let p3 = env_at(i1 + 2);
    let v = 0.5
        * ((2.0 * p1)
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t);
    v.clamp(0.0, 1.0) as f32
}

/// Load ONLY the analysis-rate envelope (~86 Hz, a few hundred KB even for
/// hour-long tracks) plus its rate. The segmented giant path evaluates
/// `catmull_env_at` on the fly instead of materializing the full-rate
/// envelope (3.5 GB for a 38-minute 192 kHz source at ×2).
pub fn load_analysis_envelope(source_path: &Path) -> Option<(Vec<f64>, f64)> {
    let stem = source_path.file_stem()?.to_string_lossy().to_string();
    let sidecar_name = format!("{}.onset_envelope.json", stem);
    let sidecar_path = source_path.parent()?.join(&sidecar_name);
    if !sidecar_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&sidecar_path).ok()?;
    let envelope_sr = extract_json_number(&content, "envelope_sr").unwrap_or(100.0);
    let env_start = content.find("\"envelope\": [")?;
    let env_end = content.rfind(']')?;
    let bracket_offset = content[env_start..].find('[')?;
    let arr_start = env_start + bracket_offset + 1;
    if arr_start >= env_end {
        return None;
    }
    let analysis: Vec<f64> = content[arr_start..env_end]
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();
    if analysis.is_empty() {
        return None;
    }
    Some((analysis, envelope_sr))
}

pub fn load_external_envelope(
    source_path: &Path,
    total_output_samples: usize,
    output_sr: f64,
) -> Option<BlendEnvelope> {
    // Build expected path: <source_dir>/<source_stem>.onset_envelope.json
    let stem = source_path.file_stem()?.to_string_lossy().to_string();
    let sidecar_name = format!("{}.onset_envelope.json", stem);
    let sidecar_path = source_path.parent()?.join(&sidecar_name);

    if !sidecar_path.exists() {
        return None;
    }

    crate::aelog!(
        "[HYBRID-PHASE] Found external envelope: {}",
        sidecar_path.display()
    );

    // Read and parse JSON
    let content = match std::fs::read_to_string(&sidecar_path) {
        Ok(c) => c,
        Err(e) => {
            crate::aelog!("[HYBRID-PHASE] Failed to read external envelope: {}", e);
            return None;
        }
    };

    // Simple JSON parsing — extract envelope_sr and envelope array
    // We avoid pulling in serde just for this one file.
    let envelope_sr = extract_json_number(&content, "envelope_sr").unwrap_or(100.0);
    let algorithm = extract_json_string(&content, "algorithm").unwrap_or_default();

    // Extract envelope array. A malformed sidecar (e.g. truncated download,
    // hand-edited JSON) must not panic — propagate None instead so the
    // caller falls back to the default behaviour (full linear-phase).
    let env_start = match content.find("\"envelope\": [") {
        Some(p) => p,
        None => {
            crate::aelog!("[HYBRID-PHASE] Invalid envelope JSON: no \"envelope\": [");
            return None;
        }
    };
    let env_end = match content.rfind(']') {
        Some(p) => p,
        None => {
            crate::aelog!("[HYBRID-PHASE] Invalid envelope JSON: no closing ]");
            return None;
        }
    };
    let bracket_offset = match content[env_start..].find('[') {
        Some(p) => p,
        None => {
            crate::aelog!("[HYBRID-PHASE] Invalid envelope JSON: missing [ after key");
            return None;
        }
    };
    let arr_start = env_start + bracket_offset + 1;
    if arr_start >= env_end {
        crate::aelog!("[HYBRID-PHASE] Invalid envelope JSON: empty / inverted array span");
        return None;
    }
    let arr_str = &content[arr_start..env_end];

    let analysis_envelope: Vec<f64> = arr_str
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();

    if analysis_envelope.is_empty() {
        crate::aelog!("[HYBRID-PHASE] Empty envelope data");
        return None;
    }

    crate::aelog!(
        "[HYBRID-PHASE] Loaded external envelope: {} samples @ {}Hz ({})",
        analysis_envelope.len(),
        envelope_sr as u32,
        algorithm
    );

    // Upsample the ~86 Hz analysis envelope to the output rate.
    //
    // Catmull-Rom (C1-continuous) instead of linear interpolation: linear
    // upsampling by a factor of ~4000–9000 leaves a kink in the envelope's
    // first derivative at every analysis frame boundary (every ~11.6 ms),
    // making the 0.3 switch-threshold crossing jitter when neighbouring
    // frames are nearly equal. The spline removes the kinks with zero added
    // latency. (Formula lives in catmull_env_at — shared with the segmented
    // giant path, which evaluates it on the fly instead of materializing.)
    use rayon::prelude::*;
    let frames_to_output = output_sr / envelope_sr;
    let mut envelope = vec![0.0f32; total_output_samples];
    envelope.par_iter_mut().enumerate().for_each(|(i, out)| {
        *out = catmull_env_at(&analysis_envelope, frames_to_output, i);
    });

    // Stats — threshold 0.3 matches blend_outputs() and hpss_native reporting
    let active_samples = envelope.iter().filter(|&&v| v >= 0.3).count();
    let active_pct = 100.0 * active_samples as f64 / total_output_samples.max(1) as f64;
    let nonzero_samples = envelope.iter().filter(|&&v| v > 0.01).count();
    let nonzero_pct = 100.0 * nonzero_samples as f64 / total_output_samples.max(1) as f64;

    crate::aelog!(
        "[HYBRID-PHASE] External envelope: active(>=0.3)={:.1}%, nonzero={:.1}%",
        active_pct, nonzero_pct
    );

    Some(BlendEnvelope {
        envelope,
        analysis_envelope,
        analysis_sr: envelope_sr,
    })
}

/// Extract a numeric value from JSON by key (simple parser, no serde needed)
fn extract_json_number(json: &str, key: &str) -> Option<f64> {
    let pattern = format!("\"{}\":", key);
    let pos = json.find(&pattern)?;
    let after = &json[pos + pattern.len()..];
    let value_str: String = after
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    value_str.parse().ok()
}

/// Extract a string value from JSON by key (simple parser, no serde needed)
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":", key);
    let pos = json.find(&pattern)?;
    let after = &json[pos + pattern.len()..];
    let trimmed = after.trim();
    if !trimmed.starts_with('"') {
        return None;
    }
    let inner = &trimmed[1..];
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

// ── Switch-plan machinery (shared by mono and stereo blend) ──────────────
//
// The envelope decides WHEN to be in min-phase; the zero-crossing snap
// decides the EXACT switch sample. The plan (scan → snap → segments) is
// computed once and then applied to each channel, so both channels always
// switch at the same instant. If each channel snapped independently (old
// behaviour), a centred transient could switch phase in L up to the full
// ±5 ms search window earlier than in R — an interchannel timing artifact
// orders of magnitude above the ~10-20 µs ITD audibility threshold, smearing
// the phantom image on exactly the transients Hybrid-Phase is meant to fix.

/// Envelope level above which the blend switches to minimum phase.
pub(crate) const SWITCH_THRESHOLD: f32 = 0.3;

/// Time-based switch windows (independent of output sample rate):
///   5 ms search window for zero-crossing snap,
///   1 ms switch fade, 20 ms retriggerable hold.
/// Single source of truth for the batch plan and the streaming blender.
///
/// The fade used to be 0.083 ms. The snap zeroes the MID difference, so each
/// channel still steps by its share of the side difference, and 83 µs cannot
/// hide a step: every switch sprayed a tick across the whole spectrum. On a
/// clipped CD at 176.4k / 30M, content above 24 kHz peaked at −42.9 dBFS, and
/// all 895 ticks above −80 dBFS sat within 0.1 ms of a switch. A 1 ms fade on
/// `switch_fade`'s curve takes that to −103.1 dBFS, as clean as a pure
/// linear-phase render. The price is the two branches overlapping for 1 ms:
/// against a click-free splice, 14–21 kHz in the 3 ms around a switch comes
/// out 0.32 dB lower (median), 8–14 kHz 0.05 dB, below 8 kHz nothing.
pub(crate) fn switch_params(output_sr: f64) -> (usize, usize, usize) {
    let search_window: usize = ((output_sr * 0.005).round() as usize).max(64);
    let switch_fade_len: usize = ((output_sr * 0.001).round() as usize).max(8);
    let min_cooldown: usize = ((output_sr * 0.020).round() as usize).max(256);
    (search_window, switch_fade_len, min_cooldown)
}

/// Blend weight at position `t` ∈ [0, 1] through a switch fade: the running
/// integral of a Hann window. Its slope and curvature are both zero at the
/// ends, so the fade's own spectrum falls off far faster than a raised-cosine
/// ramp of the same length — at 1 ms that is the difference between −90 and
/// −103 dBFS of residue above the band, for a smaller top-octave dip.
/// The batch plan and the streaming blender must use this one function, or
/// their outputs stop matching bit for bit.
#[inline]
pub(crate) fn switch_fade(t: f64) -> f64 {
    t - (2.0 * PI * t).sin() / (2.0 * PI)
}

/// Snap one envelope boundary to the nearest true zero-crossing of the
/// linear-minus-minimum difference signal within ±search_window. Shared by
/// the batch switch plan and the segmented streaming blender — the two must
/// pick IDENTICAL switch samples.
pub(crate) fn snap_boundary(
    b: usize,
    len: usize,
    search_window: usize,
    diff: &(impl Fn(usize) -> f64 + ?Sized),
) -> usize {
    let lo = b.saturating_sub(search_window);
    let hi = (b + search_window).min(len);

    let mut best_idx = b;
    let mut min_dist_to_b = usize::MAX;
    let mut best_diff = f64::MAX;
    let mut found_zero = false;

    // Ensure valid range
    if lo >= hi {
        return b;
    }

    let mut last_sign = diff(lo).signum();

    for j in lo..hi {
        let d = diff(j);
        let sign = d.signum();

        // True zero-crossing (phase intersection)
        if sign != last_sign && j > lo {
            found_zero = true;
            let prev_d = diff(j - 1).abs();
            let curr_d = d.abs();

            // Which sample is closer to strict 0.0 amplitude diff
            let local_best_idx = if prev_d < curr_d { j - 1 } else { j };
            let dist = local_best_idx.abs_diff(b);

            // Favor the zero-crossing physically nearest to the original envelope boundary
            if dist < min_dist_to_b {
                min_dist_to_b = dist;
                best_idx = local_best_idx;
            }
        } else if !found_zero {
            // Fallback: If no crossing found yet, maintain absolute minimum distance
            let abs_d = d.abs();
            if abs_d < best_diff {
                best_diff = abs_d;
                best_idx = j;
            }
        }
        last_sign = sign;
    }
    best_idx
}

struct Seg {
    start: usize,
    end: usize,
    is_min: bool,
}
struct Xfade {
    start: usize,
    end: usize,
    from_min: bool,
}

struct SwitchPlan {
    flat_segs: Vec<Seg>,
    xfades: Vec<Xfade>,
    n_boundaries: usize,
}

/// Build the switch plan for a signal of `len` samples.
///
/// `diff(j)` returns the linear-minus-minimum difference used for the
/// zero-crossing snap: the channel's own difference for mono blending, or
/// the MID difference ((dL + dR)/2) for stereo-linked blending.
///
/// Time-based windows (independent of output sample rate):
///   5 ms search window for zero-crossing snap
///   20 ms cooldown to prevent rapid toggling
///   1 ms switch fade centred on the snapped sample — see `switch_params`
///     for why it is not shorter and `switch_fade` for its curve.
fn compute_switch_plan<F>(
    len: usize,
    envelope: &BlendEnvelope,
    offset: usize,
    output_sr: f64,
    diff: F,
) -> SwitchPlan
where
    F: Fn(usize) -> f64 + Sync,
{
    use rayon::prelude::*;

    let threshold: f32 = SWITCH_THRESHOLD;
    let (search_window, switch_fade_len, min_cooldown) = switch_params(output_sr);
    let half_fade = switch_fade_len / 2;

    // ── Step 1: Scan for phase boundaries with Retriggerable Hold-Timer ──
    let mut boundaries: Vec<usize> = Vec::new();
    let mut min_active = envelope.get(offset) >= threshold;
    let mut min_end = 0usize;
    let use_min_0 = min_active;

    for i in 1..len {
        let m = envelope.get(offset + i) >= threshold;
        if m {
            if !min_active {
                // Instantly trigger attack
                min_active = true;
                boundaries.push(i);
            }
            // Retrigger the hold timer: we must stay in min-phase
            // for at least min_cooldown samples AFTER the last high envelope frame.
            min_end = i + min_cooldown;
        } else {
            if min_active && i >= min_end {
                // Hold timer expired, safe to return to linear phase
                min_active = false;
                boundaries.push(i);
            }
        }
    }

    // ── Step 2: Snap each boundary to true zero-crossing **in parallel** ──
    let snapped: Vec<usize> = boundaries
        .par_iter()
        .map(|&b| snap_boundary(b, len, search_window, &diff))
        .collect();

    // ── Step 3: Build non-overlapping segment + crossfade lists ──
    let mut flat_segs: Vec<Seg> = Vec::with_capacity(snapped.len() + 1);
    let mut xfades: Vec<Xfade> = Vec::with_capacity(snapped.len());

    let mut pos = 0usize;
    let mut cur_min = use_min_0;

    for &sw in &snapped {
        let fade_start = sw.saturating_sub(half_fade).max(pos);
        let fade_end = (sw + half_fade).min(len).max(fade_start);

        if fade_start > pos {
            flat_segs.push(Seg {
                start: pos,
                end: fade_start,
                is_min: cur_min,
            });
        }
        if fade_end > fade_start {
            xfades.push(Xfade {
                start: fade_start,
                end: fade_end,
                from_min: cur_min,
            });
        }
        pos = fade_end;
        cur_min = !cur_min;
    }
    if pos < len {
        flat_segs.push(Seg {
            start: pos,
            end: len,
            is_min: cur_min,
        });
    }

    SwitchPlan {
        flat_segs,
        xfades,
        n_boundaries: boundaries.len(),
    }
}

/// Materialize a switch plan for one channel.
fn apply_switch_plan(
    plan: &SwitchPlan,
    y_linear: &[f64],
    y_minimum: &[f64],
    len: usize,
) -> Vec<f64> {
    let mut output = vec![0.0f64; len];
    output.copy_from_slice(&y_linear[..len]);
    apply_switch_plan_inplace(plan, &mut output, y_minimum);
    output
}

/// Apply a switch plan writing the result INTO `y_linear`. Every output
/// sample depends only on the same index of the two inputs, so overwriting
/// the linear buffer in place is exact — this avoids materializing a third
/// full-length track copy (multi-GB at 705.6/768 kHz).
fn apply_switch_plan_inplace(plan: &SwitchPlan, y_linear: &mut [f64], y_minimum: &[f64]) {
    // Flat segments: linear segments are already in place; min segments are
    // a straight memcpy from y_minimum.
    for seg in &plan.flat_segs {
        if seg.is_min {
            y_linear[seg.start..seg.end].copy_from_slice(&y_minimum[seg.start..seg.end]);
        }
    }

    // Switch fade — reads both signals at idx before writing idx.
    for xf in &plan.xfades {
        let fade_len = xf.end - xf.start;
        for k in 0..fade_len {
            let idx = xf.start + k;
            let t = k as f64 / fade_len.max(1) as f64;
            let blend = switch_fade(t);
            let (from, to) = if xf.from_min {
                (y_minimum[idx], y_linear[idx])
            } else {
                (y_linear[idx], y_minimum[idx])
            };
            y_linear[idx] = from * (1.0 - blend) + to * blend;
        }
    }
}

/// Blend two output buffers using zero-crossing hard switch (single channel).
///
/// Instead of amplitude-domain crossfading (which causes comb filtering),
/// this performs a BINARY switch between linear and minimum phase outputs.
/// The switch point is snapped to the nearest zero-crossing of the
/// difference signal (y_linear - y_minimum), where both signals are equal.
/// A 1 ms fade (`switch_fade`) keeps the switch from spraying a tick.
///
/// Production stereo code should use `blend_outputs_stereo`, which shares
/// one switch plan across both channels; this single-channel version keeps
/// the same behaviour for mono use and tests.
#[allow(dead_code)] // exercised by tests; kept as the mono reference implementation
pub fn blend_outputs(
    y_linear: &[f64],
    y_minimum: &[f64],
    envelope: &BlendEnvelope,
    offset: usize,
    output_sr: f64,
) -> Vec<f64> {
    let len = y_linear.len().min(y_minimum.len());
    if len == 0 {
        return vec![];
    }

    let plan = compute_switch_plan(len, envelope, offset, output_sr, |j| {
        y_linear[j] - y_minimum[j]
    });
    let output = apply_switch_plan(&plan, y_linear, y_minimum, len);

    crate::aelog!(
        "[HybridPhase] Blend generated: {} snapped boundaries ({} debounced), {} crossfades.",
        plan.flat_segs.len().saturating_sub(1),
        plan.n_boundaries,
        plan.xfades.len(),
    );
    output
}

/// Stereo-linked zero-crossing hard switch.
///
/// One switch plan is computed from the MID difference signal
/// ((dL + dR) / 2, where d = y_linear − y_minimum) and applied to BOTH
/// channels, so L and R always change phase at the same sample. The mid
/// signal is used purely for ANALYSIS (choosing the switch instant); the
/// audio itself never passes through an M/S transform — each channel's
/// output is still bit-for-bit its own y_linear or y_minimum outside the
/// switch fades. This removes the interchannel switch-time drift of
/// per-channel snapping without touching the recorded panorama.
#[allow(dead_code)] // exercised by tests; production paths use the in-place variant
pub fn blend_outputs_stereo(
    y_linear_l: &[f64],
    y_minimum_l: &[f64],
    y_linear_r: &[f64],
    y_minimum_r: &[f64],
    envelope: &BlendEnvelope,
    offset: usize,
    output_sr: f64,
) -> (Vec<f64>, Vec<f64>) {
    let len = y_linear_l
        .len()
        .min(y_minimum_l.len())
        .min(y_linear_r.len())
        .min(y_minimum_r.len());
    let mut out_l = y_linear_l[..len].to_vec();
    let mut out_r = y_linear_r[..len].to_vec();
    blend_outputs_stereo_inplace(
        &mut out_l,
        &y_minimum_l[..len],
        &mut out_r,
        &y_minimum_r[..len],
        envelope,
        offset,
        output_sr,
    );
    (out_l, out_r)
}

/// Asymmetric envelope smoother: instant attack, exponential release.
/// Applied once at analysis_sr before per-sample interpolation.
/// τ ≈ 30 ms ensures the blend weight decays smoothly after transients.
pub(crate) fn smooth_analysis_envelope(input: &[f64], analysis_sr: f64) -> Vec<f64> {
    if input.is_empty() {
        return vec![];
    }
    let tau = 0.030_f64; // 30 ms
    let release = (-1.0_f64 / (analysis_sr.max(1.0) * tau)).exp();
    let mut out = Vec::with_capacity(input.len());
    let mut prev = 0.0f64;
    for &raw in input {
        let v = if raw >= prev { raw } else { prev * release };
        let v = v.clamp(0.0, 1.0);
        out.push(v);
        prev = v;
    }
    out
}

/// Continuous-alpha per-sample weighted crossfade driven by the smoothed
/// analysis envelope. Replaces the binary zero-crossing switch when
/// `settings.lab.continuous_alpha` is on.
///
/// alpha_i = catmull_env_at(smoothed_analysis, frames_to_output, offset+i)
/// y_linear[i] = alpha_i * y_minimum[i] + (1-alpha_i) * y_linear[i]
///
/// Both channels share the same alpha (stereo-linked). In-place: result
/// overwrites y_linear_l / y_linear_r without allocating new full-length
/// buffers. rayon par_chunks over the output for throughput.
pub fn blend_outputs_stereo_continuous(
    y_linear_l: &mut [f64],
    y_minimum_l: &[f64],
    y_linear_r: &mut [f64],
    y_minimum_r: &[f64],
    envelope: &BlendEnvelope,
    offset: usize,
    output_sr: f64,
) {
    let len = y_linear_l
        .len()
        .min(y_minimum_l.len())
        .min(y_linear_r.len())
        .min(y_minimum_r.len());
    if len == 0 {
        return;
    }

    // Smooth the analysis-rate envelope once (instant attack, ~30 ms release).
    let smoothed = smooth_analysis_envelope(&envelope.analysis_envelope, envelope.analysis_sr);
    let frames_to_output = output_sr / envelope.analysis_sr.max(1.0);

    // Per-sample weighted blend; rayon par_chunks so α source is read-only
    // across threads. L and R use the same alpha — stereo-linked by construction.
    use rayon::prelude::*;
    const BLEND_CHUNK: usize = 4096;
    y_linear_l[..len]
        .par_chunks_mut(BLEND_CHUNK)
        .zip(y_minimum_l[..len].par_chunks(BLEND_CHUNK))
        .zip(y_linear_r[..len].par_chunks_mut(BLEND_CHUNK))
        .zip(y_minimum_r[..len].par_chunks(BLEND_CHUNK))
        .enumerate()
        .for_each(|(ci, (((yl_c, yml_c), yr_c), ymr_c))| {
            let base = offset + ci * BLEND_CHUNK;
            for k in 0..yl_c.len() {
                let alpha = catmull_env_at(&smoothed, frames_to_output, base + k) as f64;
                yl_c[k] = alpha * yml_c[k] + (1.0 - alpha) * yl_c[k];
                yr_c[k] = alpha * ymr_c[k] + (1.0 - alpha) * yr_c[k];
            }
        });

    crate::aelog!(
        "[ALPHA] Continuous hybrid blend: {} samples, {} analysis frames @ {:.1} Hz",
        len,
        smoothed.len(),
        envelope.analysis_sr
    );
}

/// In-place stereo-linked blend: the result replaces `y_linear_l`/`y_linear_r`.
/// Same switch plan and arithmetic as `blend_outputs_stereo`, but without
/// allocating two more full-length output buffers — at 705.6/768 kHz those
/// are ~1.4 GB each, and the whole point of the hard switch is that every
/// output sample is a same-index function of the two inputs.
/// All four slices must have equal length (callers truncate beforehand).
pub fn blend_outputs_stereo_inplace(
    y_linear_l: &mut [f64],
    y_minimum_l: &[f64],
    y_linear_r: &mut [f64],
    y_minimum_r: &[f64],
    envelope: &BlendEnvelope,
    offset: usize,
    output_sr: f64,
) {
    let len = y_linear_l
        .len()
        .min(y_minimum_l.len())
        .min(y_linear_r.len())
        .min(y_minimum_r.len());
    if len == 0 {
        return;
    }

    let plan = {
        let (lin_l, lin_r) = (&*y_linear_l, &*y_linear_r);
        compute_switch_plan(len, envelope, offset, output_sr, |j| {
            0.5 * ((lin_l[j] - y_minimum_l[j]) + (lin_r[j] - y_minimum_r[j]))
        })
    };
    apply_switch_plan_inplace(&plan, &mut y_linear_l[..len], &y_minimum_l[..len]);
    apply_switch_plan_inplace(&plan, &mut y_linear_r[..len], &y_minimum_r[..len]);

    crate::aelog!(
        "[HybridPhase] Stereo-linked blend: {} boundaries, {} crossfades — L/R switch at identical samples (mid-difference snap)",
        plan.n_boundaries,
        plan.xfades.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_envelope(n: usize, value: f32) -> BlendEnvelope {
        BlendEnvelope {
            envelope: vec![value; n],
            analysis_envelope: vec![],
            analysis_sr: 100.0,
        }
    }

    #[test]
    fn pure_linear_path_returns_y_linear() {
        // envelope < 0.3 everywhere → output should be y_linear bit-for-bit.
        let n = 4096;
        let y_lin: Vec<f64> = (0..n).map(|i| (i as f64) * 0.001).collect();
        let y_min: Vec<f64> = (0..n).map(|i| -(i as f64) * 0.001).collect();
        let env = flat_envelope(n, 0.0);
        let out = blend_outputs(&y_lin, &y_min, &env, 0, 384_000.0);
        assert_eq!(out.len(), n);
        for i in 0..n {
            assert!((out[i] - y_lin[i]).abs() < 1e-12, "linear path mismatch at {}", i);
        }
    }

    #[test]
    fn pure_min_path_returns_y_minimum() {
        let n = 4096;
        let y_lin: Vec<f64> = (0..n).map(|i| (i as f64) * 0.001).collect();
        let y_min: Vec<f64> = (0..n).map(|i| -(i as f64) * 0.001).collect();
        let env = flat_envelope(n, 1.0);
        let out = blend_outputs(&y_lin, &y_min, &env, 0, 384_000.0);
        assert_eq!(out.len(), n);
        for i in 0..n {
            assert!((out[i] - y_min[i]).abs() < 1e-12, "min path mismatch at {}", i);
        }
    }

    #[test]
    fn empty_inputs_yield_empty_output() {
        let env = flat_envelope(0, 0.0);
        let out = blend_outputs(&[], &[], &env, 0, 384_000.0);
        assert!(out.is_empty());
    }

    #[test]
    fn stereo_blend_matches_mono_when_channels_identical() {
        // With identical L and R content the mid difference equals the
        // per-channel difference, so the stereo plan must reproduce the
        // mono result bit-for-bit on both channels.
        let n = 32768;
        let y_lin: Vec<f64> = (0..n).map(|i| (i as f64 * 0.01).sin()).collect();
        let y_min: Vec<f64> = (0..n).map(|i| (i as f64 * 0.01 + 1.1).sin() * 0.9).collect();
        let mut env = vec![0.0f32; n];
        for v in env.iter_mut().take(20000).skip(12000) {
            *v = 1.0;
        }
        let env = BlendEnvelope {
            envelope: env,
            analysis_envelope: vec![],
            analysis_sr: 100.0,
        };
        let mono = blend_outputs(&y_lin, &y_min, &env, 0, 384_000.0);
        let (sl, sr_) = blend_outputs_stereo(&y_lin, &y_min, &y_lin, &y_min, &env, 0, 384_000.0);
        assert_eq!(sl.len(), mono.len());
        for i in 0..n {
            assert!(
                (sl[i] - mono[i]).abs() < 1e-15 && (sr_[i] - mono[i]).abs() < 1e-15,
                "stereo/mono divergence at {}",
                i
            );
        }
    }

    #[test]
    fn stereo_blend_switches_both_channels_at_same_instant() {
        // Different content per channel → per-channel zero-crossings differ,
        // but the shared mid-difference plan must select the SAME phase
        // source for L and R at every sample outside the switch fades.
        let n = 65536;
        let y_lin_l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.010).sin()).collect();
        let y_min_l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.010 + 1.3).sin() * 0.9).collect();
        let y_lin_r: Vec<f64> = (0..n).map(|i| (i as f64 * 0.017 + 0.4).cos()).collect();
        let y_min_r: Vec<f64> = (0..n).map(|i| (i as f64 * 0.017 + 2.1).cos() * 1.1).collect();
        let mut env = vec![0.0f32; n];
        for v in env.iter_mut().take(28000).skip(20000) {
            *v = 1.0;
        }
        let env = BlendEnvelope {
            envelope: env,
            analysis_envelope: vec![],
            analysis_sr: 100.0,
        };
        let (out_l, out_r) =
            blend_outputs_stereo(&y_lin_l, &y_min_l, &y_lin_r, &y_min_r, &env, 0, 384_000.0);

        let mut mismatches = 0usize;
        let mut ambiguous = 0usize;
        for i in 0..n {
            let sel = |out: f64, lin: f64, min: f64| -> Option<bool> {
                if out == lin && out != min {
                    Some(false)
                } else if out == min && out != lin {
                    Some(true)
                } else {
                    None // crossfade sample or coincidental equality
                }
            };
            match (
                sel(out_l[i], y_lin_l[i], y_min_l[i]),
                sel(out_r[i], y_lin_r[i], y_min_r[i]),
            ) {
                (Some(a), Some(b)) => {
                    if a != b {
                        mismatches += 1;
                    }
                }
                _ => ambiguous += 1,
            }
        }
        // Two switches, one fade each; every other sample is a pure branch.
        let (_, fade_len, _) = switch_params(384_000.0);
        assert!(
            ambiguous <= 2 * fade_len + 64,
            "too many ambiguous samples: {} (two fades of {})",
            ambiguous,
            fade_len
        );
        assert_eq!(
            mismatches, 0,
            "L and R selected different phase sources at {} samples",
            mismatches
        );
    }

    #[test]
    fn external_envelope_interpolation_stays_in_bounds() {
        // Catmull-Rom can overshoot near steep steps — the loader must clamp
        // to [0, 1] and hit the analysis values exactly at frame centres.
        let dir = std::env::temp_dir();
        let src = dir.join("ae_test_env_src.flac");
        let sidecar = dir.join("ae_test_env_src.onset_envelope.json");
        std::fs::write(
            &sidecar,
            "{\n  \"algorithm\": \"test\",\n  \"envelope_sr\": 100.0,\n  \"source_sr\": 44100,\n  \"envelope\": [0.0, 0.0, 1.0, 0.0, 0.8, 0.9, 0.0]\n}\n",
        )
        .unwrap();
        let total = 7 * 3840; // 7 frames at 384 kHz / 100 Hz
        let env = load_external_envelope(&src, total, 384_000.0).expect("envelope should load");
        let _ = std::fs::remove_file(&sidecar);

        assert_eq!(env.envelope.len(), total);
        for (i, &v) in env.envelope.iter().enumerate() {
            assert!((0.0..=1.0).contains(&v), "out of bounds at {}: {}", i, v);
        }
        // Frame centre (t=0) reproduces the analysis value exactly.
        assert!((env.envelope[2 * 3840] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn windows_scale_with_output_sr() {
        // The function should accept any positive sample rate and produce a
        // result of the same length without panicking — proves the adaptive
        // search/cooldown formulas don't underflow at low rates or overflow
        // at very high ones.
        for &sr in &[44_100.0_f64, 48_000.0, 192_000.0, 384_000.0, 768_000.0] {
            let n = 8192;
            let y_lin = vec![0.0_f64; n];
            let y_min = vec![0.0_f64; n];
            let env = flat_envelope(n, 0.0);
            let out = blend_outputs(&y_lin, &y_min, &env, 0, sr);
            assert_eq!(out.len(), n, "len mismatch at sr={}", sr);
        }
    }

    // ── Continuous-alpha tests ───────────────────────────────────────────────

    #[test]
    fn continuous_blend_zero_envelope_gives_linear() {
        // When analysis envelope is all zeros, alpha = 0 everywhere.
        // The linear buffer must be bit-identical on output.
        let n = 8192usize;
        let y_lin_l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.01).sin() * 0.5).collect();
        let y_lin_r: Vec<f64> = (0..n).map(|i| (i as f64 * 0.013).cos() * 0.4).collect();
        let y_min_l = vec![0.9f64; n];
        let y_min_r = vec![-0.7f64; n];
        let orig_l = y_lin_l.clone();
        let orig_r = y_lin_r.clone();
        let mut out_l = y_lin_l;
        let mut out_r = y_lin_r;
        let env = BlendEnvelope {
            envelope: vec![0.0f32; n],
            analysis_envelope: vec![0.0f64; 20],
            analysis_sr: 100.0,
        };
        blend_outputs_stereo_continuous(
            &mut out_l, &y_min_l, &mut out_r, &y_min_r, &env, 0, 352_800.0,
        );
        for i in 0..n {
            assert!(
                (out_l[i] - orig_l[i]).abs() < 1e-14 && (out_r[i] - orig_r[i]).abs() < 1e-14,
                "zero envelope changed output at {}: L {}!={}, R {}!={}",
                i, out_l[i], orig_l[i], out_r[i], orig_r[i]
            );
        }
    }

    #[test]
    fn continuous_blend_ones_envelope_gives_minimum() {
        // When analysis envelope is all ones, alpha = 1 everywhere.
        // Output must equal y_minimum.
        let n = 8192usize;
        let y_lin_l = vec![0.3f64; n];
        let y_lin_r = vec![-0.4f64; n];
        let y_min_l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.02).sin() * 0.6).collect();
        let y_min_r: Vec<f64> = (0..n).map(|i| -(i as f64 * 0.015).cos() * 0.5).collect();
        let exp_l = y_min_l.clone();
        let exp_r = y_min_r.clone();
        let mut out_l = y_lin_l;
        let mut out_r = y_lin_r;
        let env = BlendEnvelope {
            envelope: vec![1.0f32; n],
            analysis_envelope: vec![1.0f64; 20],
            analysis_sr: 100.0,
        };
        blend_outputs_stereo_continuous(
            &mut out_l, &y_min_l, &mut out_r, &y_min_r, &env, 0, 352_800.0,
        );
        for i in 0..n {
            assert!(
                (out_l[i] - exp_l[i]).abs() < 1e-14 && (out_r[i] - exp_r[i]).abs() < 1e-14,
                "ones envelope output mismatch at {}: L {}!={}, R {}!={}",
                i, out_l[i], exp_l[i], out_r[i], exp_r[i]
            );
        }
    }

    #[test]
    fn continuous_blend_needle_envelope_no_weight_discontinuity() {
        // Needle (single frame = 1.0, rest 0.0): the per-sample blend weight
        // must vary continuously — max jump < 0.02 at 352.8 kHz.
        // With y_linear = 0 and y_minimum = 1 the output directly equals alpha.
        let out_sr = 352_800.0_f64;
        let analysis_sr = 86.0_f64;
        let n_frames = 40usize;
        let frames_to_output = out_sr / analysis_sr;
        let n = (n_frames as f64 * frames_to_output).ceil() as usize + 1;

        let mut analysis = vec![0.0f64; n_frames];
        analysis[10] = 1.0;

        let mut out_l = vec![0.0f64; n];
        let mut out_r = vec![0.0f64; n];
        let y_min = vec![1.0f64; n];

        let env = BlendEnvelope {
            envelope: vec![0.0f32; n],
            analysis_envelope: analysis,
            analysis_sr,
        };
        blend_outputs_stereo_continuous(
            &mut out_l, &y_min, &mut out_r, &y_min, &env, 0, out_sr,
        );

        let mut max_jump = 0.0f64;
        for i in 1..n {
            let j = (out_l[i] - out_l[i - 1]).abs();
            if j > max_jump {
                max_jump = j;
            }
        }
        assert!(
            max_jump < 0.02,
            "max per-sample alpha jump {} >= 0.02 at 352.8 kHz",
            max_jump
        );
    }
}

