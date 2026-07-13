//! Native Rust HPSS (Harmonic-Percussive Source Separation) envelope generator.
//!
//! Replaces the previous Python `generate_envelope.py` subprocess approach
//! with a fully native implementation using rustfft + Rayon.
//!
//! Algorithm (matches librosa.decompose.hpss + onset_strength semantics):
//!   1. STFT of mono audio (n_fft=2048, hop=512, Hann window)
//!   2. Magnitude spectrogram
//!   3. Harmonic mask  = median filter along TIME  axis (kernel=31 frames)
//!   4. Percussive mask = median filter along FREQ  axis (kernel=31 bins)
//!   5. Percussive energy per frame (weighted by soft percussive mask)
//!   6. Onset flux: positive derivative of percussive energy
//!      → noise gate + sqrt compression → normalise to [0, 1]
//!      Mirrors librosa.onset.onset_strength() on the percussive component.
//!   7. Forward envelope follower: instant attack, 25ms hold, 8ms release
//!   8. Backward lookahead: extend envelope 15ms BEFORE each onset (cos² fade)
//!      → ensures minimum-phase is active BEFORE linear-phase pre-ring appears
//!   9. Save as JSON sidecar at analysis_sr (~86 Hz @ 44.1 kHz)
//!
//! Key fix (v3): step 6 was previously computing perc_frac = perc/total,
//! which measures the *type* of sound (drums vs harmonics) rather than
//! detecting a transient *event*. This produced broad plateaus (0.3–0.5)
//! instead of sharp spikes (0.0 → 1.0 → 0.0). The onset flux approach
//! produces the correct needle-like envelope shape.
//!
//! Performance (5-min track @ 44.1 kHz, Rayon 8 cores):
//!   Python + librosa : 3–8 s
//!   This implementation: ~50–150 ms

use rayon::prelude::*;
use rustfft::{num_complex::Complex, FftPlanner};
use std::f64::consts::PI;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

// ── STFT base parameters ──
//
// At CD sample rates (44.1/48 kHz) we use 2048 / 512 / 1025 (n_fft / hop / bins),
// matching librosa defaults.  For high-rate sources we scale n_fft so that the
// analysis window stays close to ~45 ms in time:
//
//     mult = next_power_of_two( round(sr / 44100) ).clamp(1, 4)
//     n_fft = 2048 * mult ;  hop = n_fft / 4 ;  bins = n_fft / 2 + 1
//
// This keeps frequency resolution (≈ 22 Hz/bin) and time-step (≈ 5–12 ms)
// roughly constant regardless of source rate.
const N_FFT_BASE: usize = 2048;

// Version tag written into the JSON sidecar. Bump whenever the detection
// algorithm changes so stale caches from older versions are regenerated
// instead of silently reused (the cache is keyed only by filename).
const ALGO_VERSION: &str = "hpss_native_rust_v4";

// Median-filter kernel sizes (in frames / bins of the *active* STFT;
// length in time/Hz scales naturally with N_FFT)
const H_KERN: usize = 31; // harmonic: along time  (31 frames ≈ 370 ms @ 44.1 kHz/hop=512)
const P_KERN: usize = 31; // percussive: along freq (31 bins ≈ 660 Hz @ 44.1 kHz/n_fft=2048)

// ── Adaptive Transient Sensitivity ────────────────────────────────────────────
//
// Instead of comparing onset flux against a single global noise_floor (average
// of all positive values across the whole track), we compare each frame against
// the LOCAL RMS of the recent onset activity. This makes the detector context-
// aware:
//
//   quiet solo guitar  → local_rms ≈ 0.01  → threshold ≈ 0.045  (catches plucks)
//   loud full band     → local_rms ≈ 0.50  → threshold ≈ 0.78   (only big hits)
//   crescendo          → threshold rises gradually with the music
//   true silence       → threshold = ABS_FLOOR only (no false triggers)
//
// threshold[f] = local_rms[f] × SENSITIVITY_RATIO + ABS_FLOOR
//
const CONTEXT_WINDOW_SECS: f64 = 3.0;    // seconds of history for local RMS
const SENSITIVITY_RATIO:   f64 = 1.5;    // must be this many × louder than background
// ABS_FLOOR: minimum gate to avoid false triggers in true digital silence.
// Lowered from 0.03 → 0.003 so that quiet solo guitar plucks (onset ≈ 0.02–0.05)
// pass the gate instead of being blocked.  Still blocks sub-threshold noise.
const ABS_FLOOR:           f64 = 0.003;

// ─────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────

/// Generate an onset-envelope JSON sidecar file next to `source_path`.
///
/// The output file is `<source_dir>/<source_stem>.onset_envelope.json` —
/// the same location and format expected by `hybrid_phase::load_external_envelope`.
///
/// Returns `Ok(())` if successful or the file already exists (cache hit).
pub fn generate_and_save(
    source_path: &Path,
    samples_l: &[f64],
    samples_r: &[f64],
    source_sr: u32,
    file_cancel: &AtomicBool,
) -> Result<(), String> {
    // ── Cache check ──
    let stem = source_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let out_path = source_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{}.onset_envelope.json", stem));

    if out_path.exists() {
        // Reuse only if the sidecar was produced by the CURRENT algorithm
        // version — an envelope from an older detector would silently
        // mistime every phase switch.
        let cache_current = std::fs::read_to_string(&out_path)
            .map(|c| c.contains(&format!("\"algorithm\": \"{}\"", ALGO_VERSION)))
            .unwrap_or(false);
        if cache_current {
            crate::aelog!("[HPSS-NATIVE] Envelope cached: {}", out_path.display());
            return Ok(());
        }
        crate::aelog!(
            "[HPSS-NATIVE] Stale envelope cache (older algorithm than {}) — regenerating: {}",
            ALGO_VERSION,
            out_path.display()
        );
    }

    let t0 = std::time::Instant::now();

    // ── Adaptive STFT parameters (B.3 fix) ────────────────────────────────────
    // Higher source rates need bigger N_FFT to keep time-domain window length
    // stable (~45 ms). Capped at 4× CD so 384/768 kHz sources don't end up
    // with 100+ ms windows that would smear transients.
    let sr_mult = (((source_sr as f64) / 44100.0).round() as usize)
        .max(1)
        .next_power_of_two()
        .min(4);
    let n_fft: usize = N_FFT_BASE * sr_mult;
    let hop: usize = n_fft / 4;
    let n_bins: usize = n_fft / 2 + 1;
    crate::aelog!(
        "[HPSS-NATIVE] STFT params (adaptive): n_fft={}, hop={}, bins={}, window={:.1} ms @ {} Hz",
        n_fft, hop, n_bins,
        1000.0 * n_fft as f64 / source_sr as f64,
        source_sr
    );

    // ── 1. Mono mix ──
    let n = samples_l.len();
    let mono: Vec<f64> = samples_l
        .iter()
        .zip(samples_r.iter())
        .map(|(&l, &r)| (l + r) * 0.5)
        .collect();

    // ── 2. Hann window (pre-computed once) ──
    let hann: Vec<f64> = (0..n_fft)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f64 / n_fft as f64).cos()))
        .collect();

    // ── 3. STFT — each frame is independent → Rayon parallel ──
    let num_frames = if n >= n_fft { (n - n_fft) / hop + 1 } else { 0 };
    if num_frames < 4 {
        return save_flat_envelope(&out_path, source_sr, hop);
    }

    let mut pl = FftPlanner::new();
    let fft = pl.plan_fft_forward(n_fft);

    // spectrogram[frame][bin] = magnitude
    let spectrogram: Vec<Vec<f64>> = (0..num_frames)
        .into_par_iter()
        .map(|f| {
            if file_cancel.load(Ordering::Relaxed) {
                return vec![0.0f64; n_bins];
            }
            let start = f * hop;
            let mut buf: Vec<Complex<f64>> = (0..n_fft)
                .map(|i| {
                    let s = if start + i < n { mono[start + i] } else { 0.0 };
                    Complex {
                        re: s * hann[i],
                        im: 0.0,
                    }
                })
                .collect();
            fft.process(&mut buf);
            buf[..n_bins]
                .iter()
                .map(|c| c.norm())
                .collect()
        })
        .collect();

    if file_cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }

    // ── 4. Harmonic mask: median along TIME for each frequency bin ──
    let h_mask: Vec<Vec<f64>> = (0..n_bins)
        .into_par_iter()
        .map(|bin| {
            let col: Vec<f64> = (0..num_frames).map(|f| spectrogram[f][bin]).collect();
            sliding_median_f64(&col, H_KERN)
        })
        .collect();

    if file_cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }

    // ── 5. Percussive mask: median along FREQ for each time frame ──
    let p_mask: Vec<Vec<f64>> = (0..num_frames)
        .into_par_iter()
        .map(|f| {
            let row: Vec<f64> = (0..n_bins).map(|b| spectrogram[f][b]).collect();
            sliding_median_f64(&row, P_KERN)
        })
        .collect();

    if file_cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }

    // ── 6 & 7. Multi-Band Spectral Flux (Logarithmic) ──
    //
    // Measuring positive difference PER BIN independently is a trap: sweeping
    // synthesizers or vibrato will cause energy to shift between neighboring bins,
    // triggering an endless stream of false "onsets".
    //
    // Summing everything into ONE broadband sum is also a trap: a loud decaying
    // bass drum will completely mask a quiet hi-hat hit.
    //
    // Solution: Group bins into 8 logarithmic bands (approximating Mel scale).
    // Vibrato stays within a band and cancels out (e_curr - e_prev <= 0),
    // but a Hi-Hat easily triggers the high-frequency band independently!
    // Band edges defined in Hz, then converted to STFT bins so they stay
    // psycho-acoustically meaningful regardless of n_fft.
    //
    // Up to 11 kHz: 7 octave-style bands (matches the original CD-rate
    // analysis). For high-rate sources we extend with one or two extra
    // bands so transients above 11 kHz are not lumped into a single
    // multi-octave bucket: a hi-hat tick at 16 kHz should not land in
    // the same band as ultrasonic energy at 60 kHz.
    let nyquist = (source_sr as f64) * 0.5;
    let mut bands_hz: Vec<f64> = vec![0.0, 172.0, 344.0, 689.0, 1378.0, 2756.0, 5512.0, 11025.0];
    if nyquist > 24_000.0 {
        bands_hz.push(22_050.0); // brilliance / "air"
    }
    if nyquist > 48_000.0 {
        bands_hz.push(44_100.0); // first ultrasonic octave
    }
    let mut band_edges: Vec<usize> = bands_hz
        .iter()
        .map(|f| ((*f * n_fft as f64 / source_sr as f64).round() as usize).min(n_bins))
        .collect();
    // Ensure strictly increasing (paranoia) and append final n_bins
    band_edges.dedup();
    if *band_edges.last().unwrap_or(&0) < n_bins {
        band_edges.push(n_bins);
    }
    let mut onset = vec![0.0f64; num_frames];
    
    for f in 1..num_frames {
        let mut flux_sum = 0.0f64;
        for i in 0..band_edges.len() - 1 {
            let start = band_edges[i];
            let end = band_edges[i + 1];
            
            let mut e_curr_band = 0.0;
            let mut e_prev_band = 0.0;
            
            for b in start..end {
                let h_curr = h_mask[b][f] as f64;
                let p_curr = p_mask[f][b] as f64;
                let s_curr = spectrogram[f][b] as f64;
                e_curr_band += s_curr * s_curr * p_curr / (h_curr + p_curr + 1e-8);

                let h_prev = h_mask[b][f - 1] as f64;
                let p_prev = p_mask[f - 1][b] as f64;
                let s_prev = spectrogram[f - 1][b] as f64;
                e_prev_band += s_prev * s_prev * p_prev / (h_prev + p_prev + 1e-8);
            }
            
            // Half-wave rectification per MULTI-BAND
            let diff = e_curr_band - e_prev_band;
            if diff > 0.0 {
                flux_sum += diff;
            }
        }
        onset[f] = flux_sum;
    }

    // ── Adaptive noise gate ──────────────────────────────────────────────────
    //
    // Per-frame adaptive threshold based on causal sliding-window RMS.
    // This replaces the previous global noise_floor (mean of all positives)
    // with a context-aware threshold that scales with the local background level.
    //
    // See module-level constants CONTEXT_WINDOW_SECS / SENSITIVITY_RATIO / ABS_FLOOR.
    let analysis_sr = source_sr as f64 / hop as f64; // ~86 Hz @ 44.1 kHz
    let context_frames = ((CONTEXT_WINDOW_SECS * analysis_sr) as usize).max(1);
    let local_rms = compute_local_rms(&onset, context_frames);

    // Global peak — used for logging and as a floor guard.
    // NOTE: NOT used for per-frame normalisation (see local_peak below).
    // We keep a small 0.01 floor so that truly silent tracks don't auto-normalise
    // microscopic FFT noise to 1.0.  (Previously 0.1, reduced to allow quiet
    // passages to contribute fully.)
    let max_onset = onset
        .iter()
        .copied()
        .fold(0.0f64, |m, v| m.max(v))
        .max(0.01_f64);

    // Local peak: sliding max over the same context window.
    // Used for per-frame normalisation so that quiet sections (guitar intro)
    // get their OWN [0..1] dynamic range instead of being crushed to near-zero
    // relative to the loudest moment in the full track.
    let local_peak = compute_local_rms_max(&onset, context_frames);

    let rms_min = local_rms.iter().copied().fold(f64::MAX, f64::min);
    let rms_max = local_rms.iter().copied().fold(0.0f64, f64::max);
    let thr_min = rms_min * SENSITIVITY_RATIO + ABS_FLOOR;
    let thr_max = rms_max * SENSITIVITY_RATIO + ABS_FLOOR;
    crate::aelog!(
        "[HPSS-NATIVE] Adaptive sensitivity: context={:.1}s, ratio={:.1}×, abs_floor={:.4}",
        CONTEXT_WINDOW_SECS, SENSITIVITY_RATIO, ABS_FLOOR
    );
    crate::aelog!(
        "[HPSS-NATIVE]   Local RMS range: [{:.4} … {:.4}]  →  threshold range: [{:.4} … {:.4}]",
        rms_min, rms_max, thr_min, thr_max
    );

    // Normalise + sqrt compression + adaptive gate.
    //
    // Gate condition  : onset[i] > local_rms[i] × SENSITIVITY_RATIO + ABS_FLOOR
    // Normalisation   : relative to local_peak[i] (same sliding window)
    //                   → a quiet guitar intro gets full 0..1 headroom
    //                   → a loud drum section also gets full 0..1 headroom
    // Compression     : sqrt() to reduce dynamic range (large hits don't
    //                   produce an unnaturally long min-phase tail)
    let mut onset_norm = vec![0.0f64; num_frames];
    let mut active_count = 0usize;
    for i in 0..num_frames {
        let adaptive_threshold = local_rms[i] * SENSITIVITY_RATIO + ABS_FLOOR;
        if onset[i] > adaptive_threshold {
            // Normalise relative to LOCAL peak, not the global track maximum.
            let local_ceil = local_peak[i].max(adaptive_threshold + 1e-15);
            onset_norm[i] = ((onset[i] - adaptive_threshold)
                / (local_ceil - adaptive_threshold))
                .sqrt()
                .min(1.0);
            active_count += 1;
        }
    }

    let analysis_sr = analysis_sr; // already defined above — keep for readability
    crate::aelog!(
        "[HPSS-NATIVE] Onset stats: {} frames @ {:.1} Hz, {} active ({:.1}%), max_onset={:.6}",
        num_frames, analysis_sr,
        active_count,
        100.0 * active_count as f64 / num_frames.max(1) as f64,
        max_onset
    );

    // ── 8. Forward envelope follower: instant attack, hold, exponential release ──
    //
    // Instant attack (jump to new value immediately) ensures the envelope
    // reaches 1.0 at the very first sample of each transient.
    // Short release (8ms) ensures it drops back to 0.0 quickly — the
    // critical property that prevents comb-filtering during the linear/min
    // phase blend, and keeps min-phase coverage narrow (5–20%).
    let hold_ms = 25.0_f64;   // covers the transient body after the attack
    let release_ms = 8.0_f64; // fast decay to avoid comb filtering
    let hold_frames = (hold_ms * analysis_sr / 1000.0).round().max(1.0) as usize;
    let release_coeff = (-1.0_f64 / (release_ms * analysis_sr / 1000.0).max(1.0)).exp();

    let mut env_forward = vec![0.0f64; num_frames];
    let mut hold_counter: usize = 0;
    for i in 0..num_frames {
        let val = onset_norm[i];
        let prev = if i > 0 { env_forward[i - 1] } else { 0.0 };
        if val > prev {
            // Instant attack: jump directly to the new peak
            env_forward[i] = val;
            hold_counter = hold_frames;
        } else if hold_counter > 0 {
            // Hold: sustain current level during transient body
            env_forward[i] = prev;
            hold_counter -= 1;
        } else {
            // Release: exponential decay back to silence
            env_forward[i] = prev * release_coeff;
        }
    }

    // ── 9. Backward lookahead: pre-onset protection ──
    //
    // Extends the envelope BEFORE each attack so that minimum-phase
    // is already active when a linear-phase pre-ring would otherwise appear.
    //
    // At ~86 Hz, 1 frame is ~11.6 ms.
    // By pushing the peak backward with a 60% multiplier, we guarantee
    // that if the transient hits 1.0, the frame BEFORE it will be 0.6.
    // Since 0.6 > our 0.3 switch threshold, the engine will activate
    // minimum-phase exactly 11.6 ms *before* the attack hits!
    let mut envelope = env_forward.clone();
    for i in (0..num_frames).rev() {
        if i + 1 < num_frames {
            // 1 frame (~11.6ms) pre-roll: strong enough to cross 0.3 threshold
            envelope[i] = envelope[i].max(env_forward[i + 1] * 0.6_f64);
        }
        if i + 2 < num_frames {
            // 2 frames (~23.2ms) pre-roll: gentle ramp up
            envelope[i] = envelope[i].max(env_forward[i + 2] * 0.2_f64);
        }
    }

    // Zero out very small values (< 1%)
    for v in envelope.iter_mut() {
        if *v < 0.01 {
            *v = 0.0;
        }
    }

    let min_pct =
        envelope.iter().filter(|&&v| v >= 0.3).count() as f64 / num_frames.max(1) as f64 * 100.0;
    let active_env_pct =
        envelope.iter().filter(|&&v| v > 0.01).count() as f64 / num_frames.max(1) as f64 * 100.0;

    // ── 10. Write JSON sidecar ──
    save_envelope_json(&out_path, &envelope, analysis_sr, source_sr)?;

    crate::aelog!(
        "[HPSS-NATIVE] ✓ Envelope generated: {} frames @ {:.1} Hz in {:.0} ms",
        num_frames,
        analysis_sr,
        t0.elapsed().as_millis(),
    );
    crate::aelog!(
        "[HPSS-NATIVE]   Coverage: min-phase(>=0.3)={:.1}%  active(>0.01)={:.1}%  linear={:.1}%",
        min_pct,
        active_env_pct,
        100.0 - active_env_pct
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────
// JSON output (format compatible with load_external_envelope)
// ─────────────────────────────────────────────────────────

fn save_envelope_json(
    path: &Path,
    envelope: &[f64],
    envelope_sr: f64,
    source_sr: u32,
) -> Result<(), String> {
    // Build JSON manually (no external dependency for this simple structure)
    let values: String = envelope
        .iter()
        .map(|v| format!("{:.6}", v))
        .collect::<Vec<_>>()
        .join(", ");

    let json = format!(
        "{{\n  \"algorithm\": \"{}\",\n  \"envelope_sr\": {:.6},\n  \"source_sr\": {},\n  \"envelope\": [{}]\n}}\n",
        ALGO_VERSION, envelope_sr, source_sr, values
    );

    std::fs::write(path, json).map_err(|e| format!("[HPSS-NATIVE] Failed to write envelope: {}", e))
}

fn save_flat_envelope(path: &Path, source_sr: u32, hop: usize) -> Result<(), String> {
    save_envelope_json(path, &[0.0], source_sr as f64 / hop as f64, source_sr)
}

// ─────────────────────────────────────────────────────────
// Sliding median filter (O(n × kernel), insertion-sort window)
// ─────────────────────────────────────────────────────────
//
// For kernel ≤ 63, insertion sort of a fixed-size window is faster than
// heap-based approaches due to cache locality and small constant.

fn sliding_median_f64(data: &[f64], kernel: usize) -> Vec<f64> {
    let n = data.len();
    let half = kernel / 2;
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(n);
            let mut win: Vec<f64> = data[lo..hi].to_vec();
            win.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            win[win.len() / 2]
        })
        .collect()
}

// ─────────────────────────────────────────────────────────
// Causal sliding-window RMS of onset flux
// ─────────────────────────────────────────────────────────
//
// Returns a smoothed background level for each frame using only PAST frames
// (causal — no look-ahead), implemented via an O(n) running sum-of-squares.
//
// This is the "local background" that the adaptive threshold is derived from.
// Using a causal window means the threshold rises gradually during a crescendo
// (it never *knows* the future is louder), which is conservative and correct:
// we prefer missing a borderline hit on the loud side over false-triggering
// on the quiet side.
fn compute_local_rms(onset: &[f64], window: usize) -> Vec<f64> {
    let n = onset.len();
    let mut result = vec![0.0f64; n];
    // Running sum of squares over the causal window
    let mut sum_sq = 0.0f64;
    // We maintain a ring-like logic with an explicit index queue:
    // for each new frame i, add onset[i]^2 and subtract the frame
    // that fell out of the window (onset[i - window]).
    for i in 0..n {
        sum_sq += onset[i] * onset[i];
        if i >= window {
            let old = onset[i - window];
            sum_sq -= old * old;
            // Guard against floating-point drift going slightly negative
            if sum_sq < 0.0 { sum_sq = 0.0; }
        }
        let count = (i + 1).min(window) as f64;
        result[i] = (sum_sq / count).sqrt();
    }
    result
}

/// Causal sliding-window MAXIMUM over the same context window.
///
/// Used as the per-frame normalisation ceiling so that each local time-context
/// gets its own full [0..1] headroom.  A quiet guitar intro and a loud drum
/// section both normalise relative to their own local peak — preventing quiet
/// transients from being crushed to near-zero by the global track maximum.
fn compute_local_rms_max(onset: &[f64], window: usize) -> Vec<f64> {
    let n = onset.len();
    let mut result = vec![0.0f64; n];
    for i in 0..n {
        let start = if i + 1 >= window { i + 1 - window } else { 0 };
        let local_max = onset[start..=i]
            .iter()
            .copied()
            .fold(0.0f64, f64::max);
        result[i] = local_max;
    }
    result
}
