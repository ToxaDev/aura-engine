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
//!   7. Forward envelope follower: instant attack, 25 ms hold (2 frames),
//!      release of one analysis frame — 11.6 ms at 44.1 kHz, see step 8
//!   8. Backward lookahead: 1–2 frames of pre-roll before each onset
//!      (×0.6 and ×0.2, i.e. ~11.6 and ~23.2 ms @ 44.1 kHz)
//!      → minimum phase is already active where linear-phase pre-ring would sit
//!   9. Save as JSON sidecar at analysis_sr (~86 Hz @ 44.1 kHz)
//!
//! What this detects, precisely: the *start of a sound*. It measures the rise
//! of percussive energy between frames — not whether the reconstruction filter
//! would actually ring on this material. On music the two coincide, because an
//! attack is both a beginning and a broadband event; on synthetic material they
//! come apart, and a steady square wave with arbitrarily steep edges produces
//! no trigger at all. See `docs/06-hybrid-phase-proof.md` §1.
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
// algorithm changes so envelopes from older versions are regenerated instead
// of silently reused. The sidecar also carries a fingerprint of the audio it
// was computed from — see `source_fingerprint` — because the file name it is
// stored under says nothing about the content.
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
/// Returns `Ok(())` if successful, or if a sidecar for exactly this audio and
/// this detector version is already there (cache hit).
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

    // Fingerprint of what is about to be analysed. The sidecar is addressed by
    // file name, so the name alone cannot say whether it describes this audio.
    let fingerprint = source_fingerprint(samples_l, samples_r, source_sr);

    if out_path.exists() {
        // Reuse only if the sidecar was produced by the CURRENT algorithm
        // version AND from exactly this audio. An envelope from an older
        // detector, or from whatever used to live under this name, would
        // silently mistime every phase switch.
        let cached = std::fs::read_to_string(&out_path).unwrap_or_default();
        let algo_ok = cached.contains(&format!("\"algorithm\": \"{}\"", ALGO_VERSION));
        let source_ok = cached.contains(&format!("\"source_fingerprint\": \"{}\"", fingerprint));
        if algo_ok && source_ok {
            crate::aelog!("[HPSS-NATIVE] Envelope cached: {}", out_path.display());
            return Ok(());
        }
        crate::aelog!(
            "[HPSS-NATIVE] Stale envelope cache ({}) — regenerating: {}",
            if algo_ok {
                "written for different audio under this name"
            } else {
                "written by an older detector"
            },
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

    // ── 1. Frame count / degenerate input ──
    let n = samples_l.len();
    let num_frames = if n >= n_fft { (n - n_fft) / hop + 1 } else { 0 };
    if num_frames < 4 {
        return save_flat_envelope(&out_path, source_sr, hop, &fingerprint, n);
    }

    // ── 2. Hann window (pre-computed once) ──
    let hann: Vec<f64> = (0..n_fft)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f64 / n_fft as f64).cos()))
        .collect();

    let mut pl = FftPlanner::new();
    let fft = pl.plan_fft_forward(n_fft);

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

    // ── 3–7. Blockwise STFT → HPSS masks → multi-band spectral flux ──
    //
    // The straightforward implementation materializes the full spectrogram
    // plus BOTH median masks: frames × bins × 8 B × 3 ≈ 21 GB for a
    // 38-minute 192 kHz source. All the math is LOCAL though — the harmonic
    // median spans ±15 frames in time, the percussive median ±15 bins in
    // frequency (within one frame), and the flux needs one previous frame —
    // so we process time in blocks with a 15-frame margin. Interior median
    // windows never touch the block edges, so the result is identical to
    // the whole-track computation; RAM holds one extended block (≤ ~400 MB
    // at 192 kHz) instead of the full matrices.
    const BLOCK_FRAMES: usize = 4096;
    let margin = H_KERN / 2; // 15 frames each side for the time median

    let mut onset = vec![0.0f64; num_frames];
    let mut f0 = 0usize;
    while f0 < num_frames {
        if file_cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }
        let f1 = (f0 + BLOCK_FRAMES).min(num_frames);
        // Extend by max(margin, 1) on the left so frame f0−1 (flux) and the
        // median windows are both available; clamp at the global edges —
        // sliding_median_f64 clamps its window exactly the same way the
        // whole-track version did there.
        let ext_lo = f0.saturating_sub(margin.max(1));
        let ext_hi = (f1 + margin).min(num_frames);
        let ext_len = ext_hi - ext_lo;

        // STFT magnitudes of the extended block (frames-major), mono mix
        // computed on the fly — no full-track mono copy.
        let spec: Vec<Vec<f64>> = (ext_lo..ext_hi)
            .into_par_iter()
            .map(|f| {
                if file_cancel.load(Ordering::Relaxed) {
                    return vec![0.0f64; n_bins];
                }
                let start = f * hop;
                let mut buf: Vec<Complex<f64>> = (0..n_fft)
                    .map(|i| {
                        let s = if start + i < n {
                            (samples_l[start + i] + samples_r[start + i]) * 0.5
                        } else {
                            0.0
                        };
                        Complex { re: s * hann[i], im: 0.0 }
                    })
                    .collect();
                fft.process(&mut buf);
                buf[..n_bins].iter().map(|c| c.norm()).collect()
            })
            .collect();

        // Percussive mask: median along FREQ for each time frame
        let p_mask: Vec<Vec<f64>> = (0..ext_len)
            .into_par_iter()
            .map(|fl| sliding_median_f64(&spec[fl], P_KERN))
            .collect();

        // Harmonic mask: median along TIME for each frequency bin
        let h_mask: Vec<Vec<f64>> = (0..n_bins)
            .into_par_iter()
            .map(|bin| {
                let col: Vec<f64> = (0..ext_len).map(|fl| spec[fl][bin]).collect();
                sliding_median_f64(&col, H_KERN)
            })
            .collect();

        if file_cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }

        // Per-band percussive energies for frames [first−1, f1) — each
        // frame's energy is computed once and reused as the next frame's
        // "previous" (numerically identical to the old per-pair recompute).
        let first = if f0 == 0 { 1 } else { f0 };
        let band_energy = |f: usize| -> Vec<f64> {
            let fl = f - ext_lo;
            let mut e = vec![0.0f64; band_edges.len() - 1];
            for (i, e_band) in e.iter_mut().enumerate() {
                for b in band_edges[i]..band_edges[i + 1] {
                    let h = h_mask[b][fl];
                    let p = p_mask[fl][b];
                    let s = spec[fl][b];
                    *e_band += s * s * p / (h + p + 1e-8);
                }
            }
            e
        };
        let energies: Vec<Vec<f64>> = ((first - 1)..f1)
            .into_par_iter()
            .map(band_energy)
            .collect();

        for f in first..f1 {
            let idx = f - (first - 1);
            let mut flux_sum = 0.0f64;
            for i in 0..energies[idx].len() {
                // Half-wave rectification per MULTI-BAND
                let diff = energies[idx][i] - energies[idx - 1][i];
                if diff > 0.0 {
                    flux_sum += diff;
                }
            }
            onset[f] = flux_sum;
        }

        f0 = f1;
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
    // A short release drops it back to 0.0 quickly — the critical property
    // that prevents comb-filtering during the linear/min phase blend, and
    // keeps min-phase coverage narrow (5–20%).
    //
    // Read the two constants as intent, not as achieved values: the envelope
    // lives on the analysis grid, so neither can be finer than one frame.
    //   hold    25 ms → 2 frames → 23.2 ms at 44.1 kHz. Representable.
    //   release  8 ms → 0.69 frames → `.max(1.0)` binds → one frame, 11.6 ms,
    //            i.e. exactly one e-fold per frame. The clamp is load-bearing
    //            at every source rate up to 192 kHz (analysis grid 86–94 Hz);
    //            only 352.8/384 kHz sources, whose hop falls to ~5.4 ms, ever
    //            reach the 8 ms the constant names.
    // One frame is the floor the grid allows. Making the release genuinely
    // shorter would mean a shorter hop, not a smaller number here.
    let hold_ms = 25.0_f64;   // covers the transient body after the attack
    let release_ms = 8.0_f64; // see above: clamped to one frame in practice
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
    save_envelope_json(&out_path, &envelope, analysis_sr, source_sr, &fingerprint, n)?;

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
    fingerprint: &str,
    source_samples: usize,
) -> Result<(), String> {
    // Build JSON manually (no external dependency for this simple structure).
    //
    // Order matters: `hybrid_phase::load_external_envelope` finds the end of
    // the envelope array with `rfind(']')`, so `envelope` has to stay the last
    // key. New fields go in front of it.
    let values: String = envelope
        .iter()
        .map(|v| format!("{:.6}", v))
        .collect::<Vec<_>>()
        .join(", ");

    let json = format!(
        "{{\n  \"algorithm\": \"{}\",\n  \"source_fingerprint\": \"{}\",\n  \"source_samples\": {},\n  \"envelope_sr\": {:.6},\n  \"source_sr\": {},\n  \"envelope\": [{}]\n}}\n",
        ALGO_VERSION, fingerprint, source_samples, envelope_sr, source_sr, values
    );

    std::fs::write(path, json).map_err(|e| format!("[HPSS-NATIVE] Failed to write envelope: {}", e))
}

fn save_flat_envelope(
    path: &Path,
    source_sr: u32,
    hop: usize,
    fingerprint: &str,
    source_samples: usize,
) -> Result<(), String> {
    save_envelope_json(
        path,
        &[0.0],
        source_sr as f64 / hop as f64,
        source_sr,
        fingerprint,
        source_samples,
    )
}

// ─────────────────────────────────────────────────────────
// Source fingerprint
// ─────────────────────────────────────────────────────────
//
// The sidecar lives next to the source and is addressed by file name only.
// That is not enough to know it belongs to the audio in front of us: edit a
// source in place, keep the name, and the previous file's switch points would
// be applied to the new audio without a word in the log.
//
// So the sidecar carries a mark of what was analysed, and the cache is only
// honoured when the mark matches. Hashing the samples rather than the file's
// size and timestamp has two advantages: the mark survives a copy or a restore
// from backup, and it also changes when a pre-DSP stage (headroom, apodizer)
// alters what the detector actually sees.
//
// Cost is bounded by construction — both ends in full plus a fixed number of
// strided probes — so a 38-minute 192 kHz source costs the same microseconds
// as a 3-minute one.

/// Samples hashed verbatim at each end of each channel.
const FP_ENDS: usize = 8192;
/// Upper bound on strided probes per channel.
const FP_PROBES: usize = 131_072;

#[inline]
fn fnv_mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x0000_0100_0000_01b3) // FNV-1a 64-bit prime
}

fn source_fingerprint(samples_l: &[f64], samples_r: &[f64], source_sr: u32) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit offset basis
    h = fnv_mix(h, samples_l.len() as u64);
    h = fnv_mix(h, samples_r.len() as u64);
    h = fnv_mix(h, source_sr as u64);

    for ch in [samples_l, samples_r] {
        let n = ch.len();
        if n == 0 {
            continue;
        }
        let head = n.min(FP_ENDS);
        for &s in &ch[..head] {
            h = fnv_mix(h, s.to_bits());
        }
        for &s in &ch[n.saturating_sub(FP_ENDS)..] {
            h = fnv_mix(h, s.to_bits());
        }
        let step = (n / FP_PROBES).max(1);
        let mut i = 0;
        while i < n {
            h = fnv_mix(h, ch[i].to_bits());
            i += step;
        }
    }
    format!("{:016x}", h)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize, seed: f64) -> Vec<f64> {
        (0..n)
            .map(|i| (i as f64 * 0.001 + seed).sin() * 0.5)
            .collect()
    }

    #[test]
    fn fingerprint_is_stable_for_identical_audio() {
        let l = ramp(50_000, 0.0);
        let r = ramp(50_000, 1.0);
        assert_eq!(
            source_fingerprint(&l, &r, 44_100),
            source_fingerprint(&l, &r, 44_100)
        );
    }

    #[test]
    fn fingerprint_tracks_the_edit_that_broke_the_cache() {
        // Prepending silence is exactly the in-place edit that used to be
        // invisible to the sidecar cache: same file name, different audio.
        let l = ramp(50_000, 0.0);
        let shifted: Vec<f64> = std::iter::repeat(0.0).take(985).chain(l.iter().copied()).collect();
        assert_ne!(
            source_fingerprint(&l, &l, 44_100),
            source_fingerprint(&shifted, &shifted, 44_100)
        );
    }

    #[test]
    fn fingerprint_separates_channels_and_rates() {
        let l = ramp(50_000, 0.0);
        let r = ramp(50_000, 1.0);
        assert_ne!(
            source_fingerprint(&l, &r, 44_100),
            source_fingerprint(&r, &l, 44_100),
            "channel order must matter"
        );
        assert_ne!(
            source_fingerprint(&l, &r, 44_100),
            source_fingerprint(&l, &r, 48_000),
            "source rate must matter"
        );
    }

    #[test]
    fn fingerprint_sees_an_edit_far_from_both_ends() {
        // The strided probes, not the verbatim ends, have to catch this one.
        let l = ramp(2_000_000, 0.0);
        let mut edited = l.clone();
        for s in edited[900_000..900_512].iter_mut() {
            *s = 0.0;
        }
        assert_ne!(
            source_fingerprint(&l, &l, 44_100),
            source_fingerprint(&edited, &edited, 44_100)
        );
    }

    #[test]
    fn sidecar_carries_the_fingerprint_before_the_envelope() {
        // load_external_envelope locates the array end with rfind(']'), so
        // every added key has to stay in front of "envelope".
        let dir = std::env::temp_dir();
        let path = dir.join("ae_test_hpss_fingerprint.onset_envelope.json");
        save_envelope_json(&path, &[0.0, 1.0, 0.5], 86.132812, 44_100, "abc123", 41_502).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(text.contains("\"source_fingerprint\": \"abc123\""));
        assert!(text.contains("\"source_samples\": 41502"));
        assert!(
            text.find("\"source_fingerprint\"").unwrap() < text.find("\"envelope\": [").unwrap(),
            "new keys must precede the envelope array"
        );
        assert!(text.rfind(']').unwrap() > text.find("\"envelope\": [").unwrap());
    }
}
