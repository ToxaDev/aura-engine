use crate::audio::converter::decode::set_status;
use crate::audio::converter::encode::{build_output_name, encode_flac};
use crate::audio::converter::state::*;
use crate::audio::converter::types::{AudioFile, ConvertSettings, PreparedAudio};
use crate::audio::dsp_core::CpuDspProcessor;
use crate::audio::gpu::GpuDspProcessor;
use crate::audio::processor::DspProcessor;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub use crate::audio::converter::pipeline::prepare::*;

use crate::audio::converter::dsp::dither::apply_dithering_and_noise_shaping;
use crate::audio::converter::dsp::true_peak::apply_true_peak_normalization;
use crate::audio::converter::dsp::polyphase::polyphase_decompose;
use crate::audio::converter::dsp::filter::find_precomputed_filter;
use crate::audio::converter::utils::verify::verify_flac;

/// Run all L polyphase sub-filters over the full input plus a zero flush,
/// one convolver per phase. Returns per-phase output streams (L, R) of
/// exactly `total_input_samples + flush_input_samples` samples each — NOT
/// yet interleaved or gain-scaled; the caller does that.
///
/// CPU mode runs the phases **in parallel** via rayon: the sub-filters are
/// completely independent (own convolver state, disjoint output streams),
/// so wall-clock drops from L×T toward T. GPU mode stays sequential — all
/// phases share one wgpu device/queue and `device.poll(Wait)` serializes
/// submissions anyway, while VRAM would have to hold L filter spectra.
fn run_polyphase_pass(
    phases: &[Vec<f64>],
    audio_l: &[f64],
    audio_r: &[f64],
    total_input_samples: usize,
    flush_input_samples: usize,
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
) -> Result<Vec<(Vec<f64>, Vec<f64>)>, String> {
    use std::sync::atomic::AtomicU64;

    let l = phases.len();
    let chunk: usize = 32768;
    let per_phase_len = total_input_samples + flush_input_samples;
    let chunks_per_phase =
        (total_input_samples + chunk - 1) / chunk + (flush_input_samples + chunk - 1) / chunk;
    let total_chunks = ((chunks_per_phase * l).max(1)) as u64;
    let chunks_done = AtomicU64::new(0);

    let bump_progress = |done: u64| {
        let pct = pct_base + ((done as f64 / total_chunks as f64) * pct_span as f64) as u32;
        file_state
            .gpu_pct
            .store(pct.min(pct_base + pct_span), Ordering::Relaxed);
    };

    let run_phase = |phase: usize| -> Result<(Vec<f64>, Vec<f64>), String> {
        let mut dsp: Box<dyn DspProcessor> = if use_gpu {
            Box::new(GpuDspProcessor::new_with_coefficients(
                &phases[phase],
                precision,
            )?)
        } else {
            Box::new(CpuDspProcessor::new_with_coefficients(&phases[phase]))
        };

        let mut out_l = vec![0.0f64; per_phase_len];
        let mut out_r = vec![0.0f64; per_phase_len];
        let mut in_l_buf = vec![0.0f64; chunk];
        let mut in_r_buf = vec![0.0f64; chunk];

        // Pass 1: real input at SOURCE rate
        let mut pos = 0;
        while pos < total_input_samples {
            if CONV_CANCEL.load(Ordering::Relaxed) {
                return Err("Cancelled".to_string());
            }
            let end = (pos + chunk).min(total_input_samples);
            let actual = end - pos;
            in_l_buf[..actual].copy_from_slice(&audio_l[pos..end]);
            in_r_buf[..actual].copy_from_slice(&audio_r[pos..end]);
            dsp.process_audio(
                &in_l_buf[..actual],
                &in_r_buf[..actual],
                &mut out_l[pos..end],
                &mut out_r[pos..end],
                actual,
            );
            pos = end;
            bump_progress(chunks_done.fetch_add(1, Ordering::Relaxed) + 1);
        }

        // Pass 2: zero flush to drain the convolver's latency + filter delay
        let zero = vec![0.0f64; chunk];
        let mut fpos = 0;
        while fpos < flush_input_samples {
            if CONV_CANCEL.load(Ordering::Relaxed) {
                return Err("Cancelled".to_string());
            }
            let end = (fpos + chunk).min(flush_input_samples);
            let actual = end - fpos;
            let o = total_input_samples + fpos;
            dsp.process_audio(
                &zero[..actual],
                &zero[..actual],
                &mut out_l[o..o + actual],
                &mut out_r[o..o + actual],
                actual,
            );
            fpos = end;
            bump_progress(chunks_done.fetch_add(1, Ordering::Relaxed) + 1);
        }

        Ok((out_l, out_r))
        // DSP processor dropped here → frees GPU/CPU memory before the next phase
    };

    if use_gpu {
        let mut res = Vec::with_capacity(l);
        for phase in 0..l {
            set_status(&format!("{} {}/{} (GPU)...", status_label, phase + 1, l));
            res.push(run_phase(phase)?);
        }
        Ok(res)
    } else {
        use rayon::prelude::*;
        set_status(&format!(
            "{} — {} phases in parallel (CPU)...",
            status_label, l
        ));
        (0..l).into_par_iter().map(run_phase).collect()
    }
}

/// Interleave per-phase streams into the final output-rate signal:
/// output[i*L + phase] = stream[phase][i] * scale.
/// Parallelized over output frames (sequential writes, strided reads).
fn interleave_polyphase(
    phase_streams: &[(Vec<f64>, Vec<f64>)],
    scale: f64,
    out_l: &mut [f64],
    out_r: &mut [f64],
) {
    use rayon::prelude::*;
    let l = phase_streams.len();
    out_l
        .par_chunks_mut(l)
        .zip(out_r.par_chunks_mut(l))
        .enumerate()
        .for_each(|(i, (frame_l, frame_r))| {
            for (phase, (pl, pr)) in phase_streams.iter().enumerate() {
                if phase < frame_l.len() {
                    frame_l[phase] = pl[i] * scale;
                    frame_r[phase] = pr[i] * scale;
                }
            }
        });
}

/// h_k[m] = h[m*L + k] for k in 0..L
/// Phase 1 (CPU-only): decode + headroom + apodize.
/// Phase 2+3 (GPU + encode): runs on the GPU thread with pre-prepared audio.
pub fn process_one_prepared(
    src_path: &Path,
    prep: PreparedAudio,
    settings: &ConvertSettings,
    file_state: Arc<FileConvState>,
) -> Result<String, String> {
    let apod_tag = prep.apod_tag.clone();
    let audio_l = prep.audio_l;
    let audio_r = prep.audio_r;
    let total_input_samples = prep.total_input_samples;
    // Stub AudioFile for build_output_name (needs artist, title, sample_rate only)
    let audio = AudioFile {
        samples_l: vec![],
        samples_r: vec![],
        sample_rate: prep.sample_rate,
        artist: prep.artist,
        title: prep.title,
    };

    // (Hybrid-Phase envelope is computed after convolution, not here)

    // Check if integrated FIR resampling is possible
    // If the ratio isn't integer, snap to the nearest integer multiple of source rate
    let out_rate = crate::audio::converter::pipeline::calculate_snap(settings.out_rate, audio.sample_rate, settings.use_fir_resampling);
    // Broadcast actual output rate to frontend
    CONV_SNAPPED_RATE.store(out_rate, Ordering::Relaxed);

    let upsample_ratio = if audio.sample_rate > 0 && out_rate > audio.sample_rate {
        out_rate / audio.sample_rate
    } else {
        1
    };
    let is_integer_ratio = upsample_ratio > 1 && (out_rate % audio.sample_rate == 0);
    let use_integrated = settings.use_fir_resampling && is_integer_ratio && settings.taps > 0;

    if use_integrated {
        // ═══ POLYPHASE FIR RESAMPLING ═══
        // The filter IS the resampler. No separate resampler needed.
        // Decompose FIR into L sub-filters, process input at SOURCE rate,
        // interleave outputs → ~9× faster than naive zero-stuffing for 8× upsample.
        let l = upsample_ratio as usize;
        let n_out = total_input_samples * l;

        // ── Anti-imaging cutoff for polyphase upsampling ──
        // For upsampling ×L, the interpolation filter MUST cut at source_Nyquist.
        //
        // IMPORTANT: in generate_fir_coefficients, fc is normalized to Fs (NOT Fs/2).
        // Therefore: fc = source_Nyquist / output_rate = source_rate / (2 × output_rate)
        //
        //   48k → 384k (×8):  fc = 48000 / (2×384000) = 0.0625  → 24kHz cutoff  ✓
        //   96k → 384k (×4):  fc = 96000 / (2×384000) = 0.125   → 48kHz cutoff  ✓
        //  192k → 384k (×2):  fc = 192000/ (2×384000) = 0.25    → 96kHz cutoff  ✓
        //
        // ERROR THAT WAS HERE: fc = source/output (= 0.125 for ×8) → cutoff at 48kHz,
        // which is exactly on the first spectral image, barely attenuating it.
        let fc_norm = audio.sample_rate as f64 / (2.0 * out_rate as f64);
        crate::aelog!(
            "[CONV]   Anti-imaging fc: {:.6} ({:.1}kHz cutoff at {}kHz output)",
            fc_norm,
            fc_norm * out_rate as f64 / 1000.0,
            out_rate / 1000
        );

        // Load full filter coefficients — ONLY from pre-computed .npy (128-bit generated by fir-optimizer).
        // Resolution order: explicit custom path (trusted as-is) → the
        // per-ratio matrix via find_precomputed_filter (same resolver the
        // standard path uses; keyed on the OUTPUT rate so the cutoff always
        // matches this conversion's ratio).
        set_status("Loading FIR filter for polyphase decomposition...");
        let full_coeffs: Vec<f64> = if let Some(ref path) = settings.custom_filter_path {
            crate::aelog!(
                "[CONV]   [WARN] Custom filter loaded — verify its fc matches {:.4} for this ratio",
                fc_norm
            );
            crate::audio::dsp_core::load_npy_f64(path)?
        } else if let Some(path) =
            find_precomputed_filter(settings.taps, out_rate, "linear_phase")
        {
            crate::aelog!("[CONV]   Loading pre-computed linear-phase (matrix): {}", path);
            crate::audio::dsp_core::load_npy_f64(&path)?
        } else {
            return Err(format!(
                "FIR filter not found for taps={} target={}Hz (linear_phase).\n\
                 Run `python fir-optimizer/optimize.py --all-ratios` to populate \
                 the per-rate filter matrix in fir-optimizer/output/.\n\
                 Runtime 64-bit generation is disabled to guarantee 128-bit filter quality.",
                settings.taps, out_rate
            ));
        };

        // Decompose into L polyphase components
        let phases = polyphase_decompose(&full_coeffs, l);
        let sub_taps = phases[0].len();
        crate::aelog!(
            "[CONV] Polyphase FIR resampling: {}Hz → {}Hz (×{})",
            audio.sample_rate, out_rate, l
        );
        crate::aelog!(
            "[CONV]   Filter: {} taps → {} sub-filters × {} taps each",
            full_coeffs.len(),
            l,
            sub_taps
        );
        crate::aelog!(
            "[CONV]   Processing at INPUT rate ({}Hz) — polyphase optimized",
            audio.sample_rate
        );

        let mode_str = if settings.use_gpu { "GPU" } else { "CPU" };

        // Compute correct scale factor from actual filter DC gain
        // DC gain = sum of all coefficients. For upsampling:
        // - If DC gain ≈ 1.0 → scale = L (standard normalization)
        // - If DC gain ≈ L → scale = 1.0 (already compensated)
        let dc_gain: f64 = full_coeffs.iter().sum();
        let scale = l as f64 / dc_gain.abs().max(1e-10);
        crate::aelog!(
            "[CONV]   Filter DC gain: {:.6} → scale factor: {:.6}",
            dc_gain, scale
        );

        // ── Pre-calculate flush size ──
        // The sub-filter has group delay = (sub_taps-1)/2 input-rate samples.
        // After processing all real input, this many samples are "stuck" in the
        // filter's delay line and haven't been output yet.
        // We must flush them with zeros, otherwise trimming the LEADING delay
        // will equally truncate the END of the track by the same amount.
        //
        // ola_latency = total algorithmic pre-roll of the per-phase convolver
        // in input-rate samples: 2×32768 on CPU (one OLS block + one deferred
        // out_buf read block — see the HIGH-1 latency tests in dsp_core.rs),
        // 1×block_size(sub_taps) on GPU. Never hardcode a single block here.
        let ola_latency: usize = if settings.use_gpu {
            crate::audio::gpu::GpuDspProcessor::output_latency_for(sub_taps)
        } else {
            crate::audio::dsp_core::CpuDspProcessor::output_latency_for(sub_taps)
        };
        let sub_delay = (sub_taps - 1) / 2; // input-rate samples: linear filter group delay
        // +1 margin input sample: the exact full-filter group delay trimmed
        // below is (N-1)/2 output samples, which exceeds sub_delay*L by up to
        // (L-1)/2 samples — one extra flushed input sample (= L output
        // samples) covers that rounding.
        let flush_input_samples = ola_latency + sub_delay + 1; // total zeros to push through each phase

        crate::aelog!(
            "[CONV]   Flush plan: ola_latency={} + sub_delay={} + 1 = {} input samples ({:.3}s @ {}Hz)",
            ola_latency,
            sub_delay,
            flush_input_samples,
            flush_input_samples as f64 / audio.sample_rate as f64,
            audio.sample_rate
        );

        // ── Run all L sub-filters: parallel on CPU, sequential on GPU ──
        let phase_streams = run_polyphase_pass(
            &phases,
            &audio_l,
            &audio_r,
            total_input_samples,
            flush_input_samples,
            settings.use_gpu,
            settings.precision,
            &format!("Polyphase ({}, {} taps/phase)", mode_str, sub_taps),
            &file_state,
            0,
            600,
        )?;

        // ── Interleave phase streams into the output-rate signal ──
        // per-phase length × L == n_out_flush by construction.
        let n_out_flush = n_out + flush_input_samples * l;
        let required_mb = (n_out_flush as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;
        let (mut output_l, mut output_r) =
            crate::audio::memory::await_free_ram_and_allocate(required_mb, || {
                (vec![0.0f64; n_out_flush], vec![0.0f64; n_out_flush])
            });
        interleave_polyphase(&phase_streams, scale, &mut output_l, &mut output_r);
        drop(phase_streams);
        file_state.gpu_pct.store(620, Ordering::Relaxed);

        // ── Group delay trim ──
        // ola_latency and sub_delay were computed before the loop.
        // We trim the algorithmic latency (ola_latency × L output samples)
        // plus the EXACT full-filter group delay (N-1)/2 in output samples —
        // not sub_delay×L, whose per-phase integer division loses (L-1)/2
        // samples. After trim, truncate output to exactly n_out
        // (= total_input_samples * L) so no flush-tail silence leaks out.

        // Detect if linear-phase by checking symmetry of the full filter
        let is_linear_phase = {
            let n = full_coeffs.len();
            if n > 10 {
                let check_count = 50.min(n / 2);
                let mut sym_err = 0.0;
                for i in 0..check_count {
                    sym_err += (full_coeffs[i] - full_coeffs[n - 1 - i]).abs();
                }
                sym_err < 1e-6 * check_count as f64
            } else {
                false
            }
        };

        let group_delay_samples = if is_linear_phase {
            let total_delay = ola_latency * l + (full_coeffs.len() - 1) / 2;
            crate::aelog!(
                "[CONV]   Linear-phase detected: trimming {} samples ({:.3}s) leading delay",
                total_delay,
                total_delay as f64 / out_rate as f64
            );
            total_delay
        } else {
            // Minimum-phase: only OLA latency
            let total_delay = ola_latency * l;
            crate::aelog!(
                "[CONV]   Minimum-phase: trimming {} samples ({:.4}s) OLA latency",
                total_delay,
                total_delay as f64 / out_rate as f64
            );
            total_delay
        };

        // 1. Trim leading delay (latency of filter startup)
        if group_delay_samples > 0 && group_delay_samples < output_l.len() {
            output_l.drain(..group_delay_samples);
            output_r.drain(..group_delay_samples);
        }

        // 2. Truncate to exactly n_out (= total_input_samples * L)
        //    Removes the flush-tail silence that was appended to drain the filter.
        if output_l.len() > n_out {
            output_l.truncate(n_out);
            output_r.truncate(n_out);
        }
        crate::aelog!(
            "[CONV]   Polyphase output: {} samples = {:.3}s @ {}Hz",
            output_l.len(),
            output_l.len() as f64 / out_rate as f64,
            out_rate
        );

        // ═══ HYBRID-PHASE IN POLYPHASE FIR PATH ═══
        // Second pass: process source through minimum-phase filter via polyphase,
        // then apply zero-crossing hard switch between linear and minimum outputs.
        if settings.hybrid_phase {
            set_status("Hybrid-Phase: loading minimum-phase filter...");
            file_state.gpu_pct.store(610, Ordering::Relaxed);

            // ── Load minimum-phase filter (ratio-aware) ──
            // Routes through the canonical resolver in dsp/filter.rs.
            let min_path_str = find_precomputed_filter(settings.taps, out_rate, "minimum_phase")
                .ok_or_else(|| format!(
                    "Hybrid-Phase minimum-phase filter not found for taps={} target={}Hz.\n\
                     Run `python fir-optimizer/optimize.py --all-ratios` to populate \
                     the per-rate filter matrix.",
                    settings.taps, out_rate
                ))?;
            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): loading pre-computed {}",
                min_path_str
            );
            let min_coeffs: Vec<f64> = crate::audio::dsp_core::load_npy_f64(&min_path_str)?;

            // ── Polyphase decompose minimum-phase filter ──
            let min_phases = polyphase_decompose(&min_coeffs, l);
            let min_sub_taps = min_phases[0].len();
            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): {} taps → {} sub-filters × {} taps each (min-phase)",
                min_coeffs.len(),
                l,
                min_sub_taps
            );

            // Compute scale factor for minimum-phase filter
            let min_dc_gain: f64 = min_coeffs.iter().sum();
            let min_scale = l as f64 / min_dc_gain.abs().max(1e-10);

            // ──────────────────────────────────────────────────────────────────────────
            // TIME-ALIGNMENT (Hybrid-Phase Synchronization)
            // ──────────────────────────────────────────────────────────────────────────
            // Why we do this:
            // A Linear-Phase filter has constant group delay, which we trim exactly
            // via `(taps-1)/2`, aligning `y_linear` to t=0.00ms.
            // A Minimum-Phase filter has its energy front-loaded, BUT it intrinsically
            // possesses a frequency-dependent group delay in the passband.
            // For a 384kHz anti-imaging filter, this bulk delay is roughly ~50-60 samples.
            // If we do not trim this delay, `y_minimum` globally lags behind `y_linear`.
            // In the vocal range (3-4kHz), ~0.15ms of lag translates to a severe phase 
            // mismatch (e.g., 160°-210°). Crossfading signals with such opposite phases 
            // creates a steep first-derivative kink, triggering extreme Gibbs ringing in 
            // the TruePeak interpolator which causes audible clipping/ticks.
            //
            // Solution:
            // A highly effective practical approximation for the bulk passband group delay 
            // of the minimum-phase filter is its mathematical "Center of Gravity": 
            // sum(i * h[i]) / sum(h[i]).
            // We compute this intrinsic delay over the full filter length and add it to 
            // the OLA drain offset. This aligns `y_minimum` with `y_linear` closely enough 
            // to mitigate destructive phase cancellations during the crossfade.
            // ──────────────────────────────────────────────────────────────────────────
            // Band-weighted group-delay (200 Hz – 6 kHz). Replaces the
            // unweighted time-domain CG which over-counts ultrasonic delay.
            let min_filter_delay_samples = crate::audio::dsp_core::estimate_band_weighted_group_delay(
                &min_coeffs,
                out_rate as f64,
                200.0,
                6000.0,
            );
            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): Min-phase band-weighted (200-6000 Hz) group delay = {} output samples (DC gain={:.6})",
                min_filter_delay_samples, min_dc_gain
            );

            // ── Second polyphase pass: minimum-phase ──
            set_status("Hybrid-Phase: polyphase minimum-phase convolution...");
            file_state.gpu_pct.store(620, Ordering::Relaxed);

            // Per-phase algorithmic latency of the min-phase convolver
            // (min_sub_taps can differ from sub_taps, so recompute).
            let min_ola_latency: usize = if settings.use_gpu {
                crate::audio::gpu::GpuDspProcessor::output_latency_for(min_sub_taps)
            } else {
                crate::audio::dsp_core::CpuDspProcessor::output_latency_for(min_sub_taps)
            };
            // Flush enough zeros per phase to materialize everything the trim
            // below consumes: OLA latency + the band-weighted min-phase bulk
            // delay (min_filter_delay_samples is in OUTPUT samples → /l), +1
            // input sample of rounding margin.
            let min_flush_input = min_ola_latency + min_filter_delay_samples / l + 1;

            // ── Run min-phase sub-filters (parallel on CPU, sequential on GPU) ──
            let min_phase_streams = run_polyphase_pass(
                &min_phases,
                &audio_l,
                &audio_r,
                total_input_samples,
                min_flush_input,
                settings.use_gpu,
                settings.precision,
                "Hybrid-Phase: polyphase (min-phase)",
                &file_state,
                620,
                140,
            )?;

            let n_out_min_flush = n_out + (min_flush_input * l);
            let required_mb_min = (n_out_min_flush as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;
            let (mut min_output_l, mut min_output_r) =
                crate::audio::memory::await_free_ram_and_allocate(required_mb_min, || {
                    (vec![0.0f64; n_out_min_flush], vec![0.0f64; n_out_min_flush])
                });
            interleave_polyphase(
                &min_phase_streams,
                min_scale,
                &mut min_output_l,
                &mut min_output_r,
            );
            drop(min_phase_streams);

            // ── Trim minimum-phase group delay ──
            // We trim OLA algorithmic latency PLUS the calculated Center of Gravity
            // of the minimum-phase filter. This time-aligns y_minimum with y_linear 
            // well enough to prevent destructive phase cancellations during crossfades.
            let min_group_delay = (min_ola_latency * l) + min_filter_delay_samples;
            if min_group_delay > 0 && min_group_delay < min_output_l.len() {
                min_output_l.drain(..min_group_delay);
                min_output_r.drain(..min_group_delay);
            }

            // Ensure same length as linear output
            let final_len = output_l.len().min(min_output_l.len());
            output_l.truncate(final_len);
            output_r.truncate(final_len);
            min_output_l.truncate(final_len);
            min_output_r.truncate(final_len);

            // ── Compute blend envelope ──
            // Auto-generate HPSS envelope if not exists, then load it
            // ── HPSS envelope: native Rust (fast) ──
            set_status("Hybrid-Phase: generating HPSS envelope (Rust)...");
            file_state.gpu_pct.store(790, Ordering::Relaxed);
            crate::audio::hpss_native::generate_and_save(
                src_path,
                &audio_l,
                &audio_r,
                audio.sample_rate,
                &crate::audio::cancel_flag::get_atomic(),
            )
            .map_err(|e| format!("HPSS Native failed: {}", e))?;

            let envelope = crate::audio::hybrid_phase::load_external_envelope(
                src_path,
                final_len,
                out_rate as f64,
            )
            .ok_or_else(|| "Hybrid-Phase: failed to load HPSS envelope".to_string())?;

            // ── Zero-crossing hard switch blend (stereo-linked) ──
            // One switch plan from the mid difference → L and R change phase
            // at the same sample, keeping the phantom image stable on attacks.
            set_status("Hybrid-Phase: applying zero-crossing hard switch...");
            let (blended_l, blended_r) = crate::audio::hybrid_phase::blend_outputs_stereo(
                &output_l, &min_output_l,
                &output_r, &min_output_r,
                &envelope, 0, out_rate as f64,
            );
            output_l = blended_l;
            output_r = blended_r;

            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): blending complete, {} samples",
                output_l.len()
            );

            // Note: .onset_envelope.json was already written by hpss_native::generate_and_save()
        }

        // Apply final true peak protection against convolution overshoot
        apply_true_peak_normalization(&mut output_l, &mut output_r);

        // Final mathematical quantization step before Encode — same output
        // stage as the standard path (the polyphase branch used to skip
        // dither entirely, leaving raw f64→24-bit truncation to ffmpeg).
        apply_dithering_and_noise_shaping(&mut output_l, &mut output_r, out_rate);

        // Encode
        set_status("Encoding FLAC...");
        file_state.gpu_pct.store(800, Ordering::Relaxed);

        let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref());
        let output_dir = src_path.parent().unwrap_or(Path::new("."));
        let output_path = output_dir.join(&output_name);

        encode_flac(&output_l, &output_r, out_rate, &output_path)?;

        // Bit-perfect verification — parity with the standard path.
        set_status("Verifying bit-perfect output...");
        file_state.gpu_pct.store(950, Ordering::Relaxed);

        return match verify_flac(&output_path, &output_l, &output_r, &file_state) {
            Ok(_) => {
                let file_size = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let size_mb = file_size as f64 / 1_048_576.0;
                set_status(&format!(
                    "\u{2713} Done: {} ({:.1} MB)",
                    output_name, size_mb
                ));
                file_state.gpu_pct.store(1000, Ordering::Relaxed);
                Ok(output_path.to_string_lossy().to_string())
            }
            Err(err_msg) => {
                let failed_name = output_name.replace(".flac", "_UNVERIFIED.flac");
                let failed_path = output_dir.join(&failed_name);
                let _ = std::fs::rename(&output_path, &failed_path);

                set_status(&format!("Verification failed: {}", err_msg));
                file_state.gpu_pct.store(1000, Ordering::Relaxed);
                Err(format!("Verification failed: {}", err_msg))
            }
        };
    }

    // ═══ STANDARD PATH: Rubato resampling + FIR post-filter ═══

    // Save source-rate audio for HPSS (audio_l/audio_r will be moved during resampling)
    let hpss_src_l = if settings.hybrid_phase {
        audio_l.clone()
    } else {
        vec![]
    };
    let hpss_src_r = if settings.hybrid_phase {
        audio_r.clone()
    } else {
        vec![]
    };
    let hpss_src_sr = audio.sample_rate;

    // 2. Resample if needed
    let (resampled_l, resampled_r, out_rate) = if audio.sample_rate != settings.out_rate {
        set_status(&format!(
            "Resampling {}Hz \u{2192} {}Hz...",
            audio.sample_rate, settings.out_rate
        ));
        let ratio = settings.out_rate as f64 / audio.sample_rate as f64;
        // Larger chunk = fewer rubato kernel calls (8× reduction vs 4096)
        let chunk: usize = 32_768;
        // Process L and R channels fully in parallel — each gets its own stateful
        // SincFixedIn. rayon::join saturates two CPU threads simultaneously → ~2× speedup.
        // sinc_len & oversampling_factor: the standard rubato profile is
        // 256/256. We bump to 512/512 to push the resampler's stop-band
        // attenuation further (~−180 dB instead of ~−140 dB) and tighten
        // the sinc ripple, at the cost of ~2× CPU time on the (rare)
        // non-integer-ratio path. The integer-ratio path goes through the
        // 1M–30M tap polyphase FIR and is unaffected.
        let p_l = SincInterpolationParameters {
            sinc_len: 512,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 512,
            window: WindowFunction::BlackmanHarris2,
        };
        let p_r = SincInterpolationParameters {
            sinc_len: 512,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 512,
            window: WindowFunction::BlackmanHarris2,
        };
        let al = std::sync::Arc::new(audio_l);
        let ar = std::sync::Arc::new(audio_r);
        let al2 = al.clone();
        let ar2 = ar.clone();

        let make_channel = move |src: std::sync::Arc<Vec<f64>>, p: SincInterpolationParameters| -> Result<Vec<f64>, String> {
            let n = src.len();
            let mut rs = SincFixedIn::<f64>::new(ratio, 2.0, p, chunk, 1)
                .map_err(|e| format!("Resampler init: {}", e))?;
            let mut out = Vec::with_capacity((n as f64 * ratio * 1.02) as usize);
            let mut buf = vec![vec![0.0f64; chunk]];
            let mut pos = 0;
            while pos < n {
                if CONV_CANCEL.load(Ordering::Relaxed) {
                    return Err("Cancelled".to_string());
                }
                let actual = (pos + chunk).min(n) - pos;
                for i in 0..chunk {
                    buf[0][i] = if i < actual { src[pos + i] } else { 0.0 };
                }
                match rs.process(&buf, None) {
                    Ok(o)  => out.extend_from_slice(&o[0]),
                    Err(e) => return Err(format!("Resample error: {}", e)),
                }
                pos += chunk;
            }
            Ok(out)
        };

        let make_l = {
            let al = al2;
            move || make_channel(al, p_l)
        };
        let make_r = {
            let ar = ar2;
            move || make_channel(ar, p_r)
        };

        let (res_l, res_r) = rayon::join(make_l, make_r);
        let out_l = res_l?;
        let out_r = res_r?;

        // Guard against rare off-by-one between channels
        let len = out_l.len().min(out_r.len());
        let out_l = out_l[..len].to_vec();
        let out_r = out_r[..len].to_vec();

        file_state.gpu_pct.store(305, Ordering::Relaxed);
        crate::aelog!(
            "[CONV] Resampled: {} \u{2192} {} samples (parallel L+R, chunk={})",
            total_input_samples, out_l.len(), chunk
        );
        (out_l, out_r, settings.out_rate)
    } else {
        (audio_l, audio_r, audio.sample_rate)
    };


    let total_output_samples = resampled_l.len();

    // Save resampled data for Hybrid-Phase (FIR convolution consumes the input)
    let resampled_l_saved = if settings.hybrid_phase {
        resampled_l.clone()
    } else {
        vec![]
    };
    let resampled_r_saved = if settings.hybrid_phase {
        resampled_r.clone()
    } else {
        vec![]
    };

    // 3. FIR convolution (post-filter)
    set_status("Applying FIR convolution...");
    file_state.gpu_pct.store(300, Ordering::Relaxed);

    // ── Pre-computed FIR resolution (ratio-aware) ───────────────────────────
    //
    // We look up the pre-computed FIR blob keyed on the OUTPUT rate so the
    // filter's cutoff is correct for the current conversion. The naming
    // convention is `fir_<TAG>_<TARGET_HZ>_<phase>.npy`; `find_precomputed_filter`
    // also accepts the legacy single-rate file (`fir_<TAG>_<phase>.npy`) but
    // ONLY when the current rate matches the legacy design point (FS8).
    //
    // If no suitable file exists (e.g. the user has not yet run
    // `fir-optimizer/optimize.py --all-ratios`), we fall back to skipping the
    // post-FIR rather than applying the wrong filter — the rubato resampler
    // is configured with sinc_len=512 + oversampling=512 (≈ −180 dB stop-band)
    // which is more than enough anti-imaging for offline upsampling.
    //
    // The custom_filter_path branch is left untouched: if the user explicitly
    // points at a custom .npy, we trust them and skip the lookup.
    let precomputed_path = if settings.custom_filter_path.is_some() {
        settings.custom_filter_path.clone()
    } else if settings.taps > 0 {
        find_precomputed_filter(settings.taps, out_rate, "linear_phase")
    } else {
        None
    };
    let post_fir_available = precomputed_path.is_some();
    if settings.taps > 0 && !post_fir_available {
        crate::aelog!(
            "[CONV] WARNING: no pre-computed FIR found for taps={} target={}Hz \
             (linear_phase). SKIPPING post-FIR — rubato resampler (sinc_len=512, \
             ~−180 dB stop-band) output goes straight downstream. \
             Run `python fir-optimizer/optimize.py --all-ratios` to populate \
             the per-rate filter matrix.",
            settings.taps, out_rate
        );
    }

    let (final_l, final_r) = if settings.taps > 0 && post_fir_available {
        let mode_str = if settings.use_gpu { "GPU" } else { "CPU" };
        set_status(&format!("Applying FIR post-filter ({})...", mode_str));

        let coeffs_path = precomputed_path.as_deref().unwrap();
        crate::aelog!("[CONV] Loading pre-computed linear-phase: {}", coeffs_path);
        let coeffs = crate::audio::dsp_core::load_npy_f64(coeffs_path)?;
        let mut dsp: Box<dyn DspProcessor> = if settings.use_gpu {
            Box::new(GpuDspProcessor::new_with_coefficients(
                &coeffs,
                settings.precision,
            )?)
        } else {
            Box::new(CpuDspProcessor::new_with_coefficients(&coeffs))
        };

        // Total algorithmic pre-roll of the convolver (CPU: 2×b_size,
        // GPU: 1×b_size) — queried from the processor itself so the trim
        // and flush arithmetic below can never drift from the actual
        // implementation latency again (the old code hardcoded one b_size,
        // leaving 32768 samples of leading silence on the CPU path).
        let ola_latency = dsp.output_latency();
        let capacity = total_output_samples + settings.taps + ola_latency;
        let required_mb = (capacity as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;

        let (mut out_l, mut out_r) = crate::audio::memory::await_free_ram_and_allocate(required_mb, || {
            (
                Vec::with_capacity(capacity),
                Vec::with_capacity(capacity),
            )
        });
        let chunk = 32768;
        let mut in_l_buf = vec![0.0f64; chunk];
        let mut in_r_buf = vec![0.0f64; chunk];
        let mut out_l_buf = vec![0.0f64; chunk];
        let mut out_r_buf = vec![0.0f64; chunk];
        let mut pos = 0;

        // ── Pass 1: Process real input data ─────────────────────────
        while pos < total_output_samples {
            if CONV_CANCEL.load(Ordering::Relaxed) {
                return Err("Cancelled".to_string());
            }

            let end = (pos + chunk).min(total_output_samples);
            let actual = end - pos;
            for i in 0..actual {
                in_l_buf[i] = resampled_l[pos + i];
                in_r_buf[i] = resampled_r[pos + i];
            }
            for i in actual..chunk {
                in_l_buf[i] = 0.0;
                in_r_buf[i] = 0.0;
            }

            dsp.process_audio(
                &in_l_buf[..actual],
                &in_r_buf[..actual],
                &mut out_l_buf[..actual],
                &mut out_r_buf[..actual],
                actual,
            );

            for i in 0..actual {
                out_l.push(out_l_buf[i]);
                out_r.push(out_r_buf[i]);
            }

            pos += actual;
            let pct = 300 + (pos as f64 / total_output_samples as f64 * 400.0) as u32;
            file_state.gpu_pct.store(pct.min(700), Ordering::Relaxed);
        }

        // ── Pass 2: Flush FIR tail (feed zeros to extract remaining output) ─
        // The FIR convolver needs (taps + algorithmic latency) additional
        // samples to fully drain: the group-delay trim below consumes
        // (ola_latency + group_delay) leading samples, so without the extra
        // ola_latency here the tail of the track would be truncated.
        let flush_samples = settings.taps + ola_latency;
        let flush_blocks = (flush_samples + chunk - 1) / chunk;

        set_status("Flushing FIR tail...");
        let zero_in = vec![0.0f64; chunk];

        for fb in 0..flush_blocks {
            if CONV_CANCEL.load(Ordering::Relaxed) {
                return Err("Cancelled".to_string());
            }

            dsp.process_audio(&zero_in, &zero_in, &mut out_l_buf, &mut out_r_buf, chunk);

            for i in 0..chunk {
                out_l.push(out_l_buf[i]);
                out_r.push(out_r_buf[i]);
            }

            let pct = 700 + ((fb as f64 / flush_blocks as f64) * 50.0) as u32;
            file_state.gpu_pct.store(pct.min(750), Ordering::Relaxed);
        }

        // ── Group delay compensation ────────────────────────────────
        // OLA latency: b_size samples
        // Linear-phase FIR: group delay = (taps - 1) / 2
        // Minimum-phase: group delay ≈ 0
        //
        // Detect if this is a linear-phase filter by checking coefficient symmetry
        let is_linear_phase = if let Some(ref path) = settings.custom_filter_path {
            // For custom filters, check by loading first/last coefficients
            if let Ok(coeffs) = crate::audio::dsp_core::load_npy_f64(path) {
                let n = coeffs.len();
                if n > 10 {
                    let check = 50.min(n / 2);
                    let err: f64 = (0..check)
                        .map(|i| (coeffs[i] - coeffs[n - 1 - i]).abs())
                        .sum();
                    err < 1e-6 * check as f64
                } else {
                    false
                }
            } else {
                true
            } // assume linear-phase
        } else {
            true // built-in windows are always linear-phase
        };

        let group_delay = if is_linear_phase {
            let gd = (settings.taps - 1) / 2;
            crate::aelog!(
                "[CONV]   Linear-phase detected: group delay = {} samples ({:.2}s)",
                gd,
                gd as f64 / out_rate as f64
            );
            gd
        } else {
            crate::aelog!("[CONV]   Minimum-phase: group delay ≈ 0");
            0
        };

        // Total leading samples to trim: OLA latency + filter group delay.
        // ola_latency comes from DspProcessor::output_latency() — 2×b_size on
        // CPU, 1×b_size on GPU (see the HIGH-1 alignment tests in dsp_core.rs).
        let total_trim = ola_latency + group_delay;
        crate::aelog!(
            "[CONV]   Trimming {} leading samples (OLA={} + GD={})",
            total_trim, ola_latency, group_delay
        );

        if total_trim > 0 && total_trim < out_l.len() {
            out_l.drain(..total_trim);
            out_r.drain(..total_trim);
        }

        // ── Trim to original duration ───────────────────────────────
        // Ensure output matches the expected number of samples
        if out_l.len() > total_output_samples {
            out_l.truncate(total_output_samples);
            out_r.truncate(total_output_samples);
        }

        crate::aelog!(
            "[CONV]   Final output: {} samples ({:.2}s at {}Hz)",
            out_l.len(),
            out_l.len() as f64 / out_rate as f64,
            out_rate
        );

        (out_l, out_r)
    } else {
        (resampled_l, resampled_r)
    };

    // ═══ HYBRID-PHASE BLENDING ═══
    // Continuous envelope follower — no discrete transient detection needed.
    // Two-pass: forward (instant attack + hold + release) + backward (pre-ring lookahead)
    //
    // Hybrid-Phase needs BOTH the linear (already applied above as final_l/r)
    // and the matching minimum-phase filter at the SAME cutoff. The lookup
    // for the min-phase blob mirrors the linear one — it must succeed at
    // the same target rate, otherwise blending min-phase-at-wrong-cutoff
    // with clean linear output makes no acoustic sense. We require both:
    //   (a) post-FIR was actually applied (post_fir_available)
    //   (b) a matching minimum-phase blob exists at this rate
    let min_phase_available = settings.hybrid_phase
        && post_fir_available
        && find_precomputed_filter(settings.taps, out_rate, "minimum_phase").is_some();
    let (final_l, final_r) = if settings.hybrid_phase && min_phase_available {
        crate::audio::converter::pipeline::apply_hybrid_phase(final_l, final_r, settings, src_path, audio.sample_rate, out_rate, Arc::clone(&file_state), &resampled_l_saved, &resampled_r_saved, &hpss_src_l, &hpss_src_r, hpss_src_sr)?
    } else {
        if settings.hybrid_phase && !min_phase_available {
            crate::aelog!(
                "[CONV] WARNING: skipping Hybrid-Phase blending — \
                 either post-FIR was skipped or no matching minimum-phase \
                 filter was found for taps={} target={}Hz. Run \
                 `python fir-optimizer/optimize.py --all-ratios` to \
                 populate the per-rate filter matrix.",
                settings.taps, out_rate
            );
        }
        (final_l, final_r)
    };

    // Apply final true peak protection against convolution overshoot
    let mut final_l = final_l;
    let mut final_r = final_r;
    apply_true_peak_normalization(&mut final_l, &mut final_r);

    // Final mathematical quantization step before Encode
    apply_dithering_and_noise_shaping(&mut final_l, &mut final_r, out_rate);

    // 4. Encode FLAC
    set_status("Encoding FLAC...");
    file_state.gpu_pct.store(800, Ordering::Relaxed); // 80%

    let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref());
    let output_dir = src_path.parent().unwrap_or(Path::new("."));
    let output_path = output_dir.join(&output_name);

    encode_flac(&final_l, &final_r, out_rate, &output_path)?;

    // 5. Verification
    set_status("Verifying bit-perfect output...");
    file_state.gpu_pct.store(950, Ordering::Relaxed);

    match verify_flac(&output_path, &final_l, &final_r, &file_state) {
        Ok(_) => {
            let file_size = std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let size_mb = file_size as f64 / 1_048_576.0;
            set_status(&format!("✓ Done: {} ({:.1} MB)", output_name, size_mb));
            file_state.gpu_pct.store(1000, Ordering::Relaxed); // 100%
            Ok(output_path.to_string_lossy().to_string())
        }
        Err(err_msg) => {
            // Rename to _UNVERIFIED
            let failed_name = output_name.replace(".flac", "_UNVERIFIED.flac");
            let failed_path = output_dir.join(&failed_name);
            let _ = std::fs::rename(&output_path, &failed_path);
            
            set_status(&format!("Verification failed: {}", err_msg));
            file_state.gpu_pct.store(1000, Ordering::Relaxed);
            // Return Ok but with the _UNVERIFIED path, frontend will see it but we need frontend to know it failed.
            // Wait, if it fails, we should return Err to trigger the Red Cross.
            Err(format!("Verification failed: {}", err_msg))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::dsp::polyphase::polyphase_decompose;

    /// Kaiser-windowed sinc lowpass (linear-phase), sum-normalized to DC=1.
    /// Same construction as the dsp_core test helper.
    fn kaiser_lowpass(taps: usize, fc_norm: f64) -> Vec<f64> {
        let half = taps / 2;
        let beta = 8.0;
        let i0 = |x: f64| {
            let mut sum = 1.0;
            let mut term = 1.0;
            let x2 = (x / 2.0).powi(2);
            for k in 1..50 {
                term *= x2 / (k as f64).powi(2);
                sum += term;
                if term < 1e-15 * sum {
                    break;
                }
            }
            sum
        };
        let i0b = i0(beta);
        let mut h = Vec::with_capacity(taps);
        let mut s = 0.0;
        for i in 0..taps {
            let n = i as f64 - half as f64;
            let sinc = if n.abs() < 1e-12 {
                fc_norm
            } else {
                (std::f64::consts::PI * fc_norm * n).sin() / (std::f64::consts::PI * n)
            };
            let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
            let v = sinc * i0(beta * (1.0 - arg * arg).max(0.0).sqrt()) / i0b;
            h.push(v);
            s += v;
        }
        for v in h.iter_mut() {
            *v /= s;
        }
        h
    }

    /// REGRESSION GUARD (polyphase path): drive the REAL pass/interleave/trim
    /// pipeline (CPU) with an interpolation filter and verify:
    ///   L channel — an impulse at input position P lands at output P×L with
    ///     ~unity peak (alignment: OLA latency ×L + exact (N-1)/2 trim);
    ///   R channel — DC input reconstructs a flat 1.0 in steady state
    ///     (scale = L / dc_gain is the correct polyphase level compensation).
    #[test]
    fn polyphase_pass_alignment_and_dc_gain() {
        let l = 4usize;
        let taps = 8001usize;
        // Interpolation cutoff for ×L upsampling: fc = 1/L in the sinc
        // convention used across this codebase (1.0 = output Nyquist).
        let h = kaiser_lowpass(taps, 1.0 / l as f64);
        let dc_gain: f64 = h.iter().sum();
        let scale = l as f64 / dc_gain.abs();
        let phases = polyphase_decompose(&h, l);
        let sub_taps = phases[0].len();

        let ola_latency = crate::audio::dsp_core::CpuDspProcessor::output_latency_for(sub_taps);
        let sub_delay = (sub_taps - 1) / 2;
        let flush = ola_latency + sub_delay + 1; // mirrors process_one_prepared

        let total_input = 10_000usize;
        let p = 5_000usize;
        let mut in_l = vec![0.0f64; total_input];
        in_l[p] = 1.0;
        let in_r = vec![1.0f64; total_input];

        let file_state = Arc::new(FileConvState::new());
        let streams = run_polyphase_pass(
            &phases, &in_l, &in_r, total_input, flush,
            false, 64, "test", &file_state, 0, 600,
        )
        .expect("polyphase pass should succeed");

        let n_out = total_input * l;
        let n_out_flush = (total_input + flush) * l;
        let mut out_l = vec![0.0f64; n_out_flush];
        let mut out_r = vec![0.0f64; n_out_flush];
        interleave_polyphase(&streams, scale, &mut out_l, &mut out_r);

        // Same trim as the production linear-phase branch.
        let trim = ola_latency * l + (taps - 1) / 2;
        out_l.drain(..trim);
        out_l.truncate(n_out);
        out_r.drain(..trim);
        out_r.truncate(n_out);
        assert_eq!(out_l.len(), n_out);

        // ── Alignment: impulse at P → peak at P×L, ~unity amplitude ──
        let (idx, val) = out_l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, &v)| (i, v))
            .unwrap();
        assert_eq!(
            idx,
            p * l,
            "polyphase impulse misaligned: landed at {} instead of {}",
            idx,
            p * l
        );
        assert!(
            (val - 1.0).abs() < 0.05,
            "polyphase impulse peak {} should be ~1.0",
            val
        );

        // ── Level: DC reconstructs flat 1.0 away from the edges ──
        let guard = taps / l; // edge ramp region in output samples per side
        for n in (guard * 2)..(n_out - guard * 2) {
            assert!(
                (out_r[n] - 1.0).abs() < 1e-4,
                "polyphase DC gain error at {}: {}",
                n,
                out_r[n]
            );
        }
    }
}
