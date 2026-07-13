use crate::audio::converter::state::FileConvState;
use crate::audio::converter::types::ConvertSettings;
use crate::audio::converter::decode::set_status;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use crate::audio::processor::DspProcessor;
use crate::audio::dsp_core::CpuDspProcessor;
use crate::audio::gpu::GpuDspProcessor;
use crate::audio::converter::state::CONV_CANCEL;

pub fn apply_hybrid_phase(
    final_l: Vec<f64>, final_r: Vec<f64>,
    settings: &ConvertSettings,
    src_path: &Path,
    _audio_sample_rate: u32,
    out_rate: u32,
    file_state: Arc<FileConvState>,
    resampled_l_saved: &[f64], resampled_r_saved: &[f64],
    hpss_src_l: &[f64], hpss_src_r: &[f64], hpss_src_sr: u32
) -> Result<(Vec<f64>, Vec<f64>), String> {
        file_state.gpu_pct.store(760, Ordering::Relaxed);

        // ── Load pre-computed minimum-phase filter (ratio-aware) ──
        // Routes through the canonical resolver in `dsp/filter.rs` which
        // tries `fir_<TAG>_<TARGET_HZ>_minimum_phase.npy` first and falls
        // back to the legacy `fir_<TAG>_minimum_phase.npy` only at the
        // FS8 design point.
        let min_path_str = crate::audio::converter::dsp::filter::find_precomputed_filter(
            settings.taps,
            out_rate,
            "minimum_phase",
        )
        .ok_or_else(|| format!(
            "Hybrid-Phase minimum-phase filter not found for taps={} target={}Hz.\n\
             Run `python fir-optimizer/optimize.py --all-ratios` to populate \
             the per-rate filter matrix in fir-optimizer/output/.",
            settings.taps, out_rate
        ))?;
        crate::aelog!(
            "[CONV] Hybrid-Phase: loading pre-computed {}",
            min_path_str
        );
        let min_coeffs: Vec<f64> = crate::audio::dsp_core::load_npy_f64(&min_path_str)?;

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
        // Band-weighted group-delay (200 Hz – 6 kHz, where the ear is most
        // sensitive to phase mismatches between linear- and minimum-phase
        // outputs during the hybrid crossfade).
        let min_filter_delay_samples = crate::audio::dsp_core::estimate_band_weighted_group_delay(
            &min_coeffs,
            out_rate as f64,
            200.0,
            6000.0,
        );
        crate::aelog!(
            "[CONV] Hybrid-Phase (GPU pipeline): Min-phase band-weighted (200-6000 Hz) group delay = {} output samples",
            min_filter_delay_samples
        );

        // ── Run second convolution with minimum-phase filter ──
        set_status("Hybrid-Phase: minimum-phase convolution...");
        file_state.gpu_pct.store(770, Ordering::Relaxed);

        let mut dsp_min: Box<dyn DspProcessor> = if settings.use_gpu {
            Box::new(GpuDspProcessor::new_with_coefficients(
                &min_coeffs,
                settings.precision,
            )?)
        } else {
            Box::new(CpuDspProcessor::new_with_coefficients(&min_coeffs))
        };

        let total_out = final_l.len();
        let chunk = 32768;
        // Algorithmic pre-roll of the convolver (CPU: 2×b_size, GPU: 1×b_size),
        // queried from the processor so trim/flush can never desync from the
        // implementation (the old code hardcoded 32768 for CPU — one block
        // short — which delayed y_minimum by 32768 samples vs y_linear and
        // smeared every hybrid crossfade on the CPU path).
        let ola_latency = dsp_min.output_latency();
        // Flush budget: everything the head-trim below consumes must be
        // materialized past total_out. Flushing the full `taps` tail (old
        // code) wasted a filter-length of zero convolution per file — the
        // discarded ringing tail beyond total_out is never encoded anyway.
        let flush_samples = ola_latency + min_filter_delay_samples + chunk;
        let capacity_min = total_out + flush_samples + chunk;
        let required_mb = (capacity_min as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;

        let (mut min_l, mut min_r) = crate::audio::memory::await_free_ram_and_allocate(required_mb, || {
            (
                Vec::with_capacity(capacity_min),
                Vec::with_capacity(capacity_min),
            )
        });
        let mut out_l_buf = vec![0.0f64; chunk];
        let mut out_r_buf = vec![0.0f64; chunk];

        let min_input_l = &resampled_l_saved;
        let min_input_r = &resampled_r_saved;
        let min_total = min_input_l.len();
        let mut pos = 0;
        let mut in_l_b = vec![0.0f64; chunk];
        let mut in_r_b = vec![0.0f64; chunk];

        while pos < min_total {
            if CONV_CANCEL.load(Ordering::Relaxed) {
                return Err("Cancelled".to_string());
            }
            let end = (pos + chunk).min(min_total);
            let actual = end - pos;
            for i in 0..actual {
                in_l_b[i] = min_input_l[pos + i];
                in_r_b[i] = min_input_r[pos + i];
            }
            for i in actual..chunk {
                in_l_b[i] = 0.0;
                in_r_b[i] = 0.0;
            }
            dsp_min.process_audio(
                &in_l_b[..actual],
                &in_r_b[..actual],
                &mut out_l_buf[..actual],
                &mut out_r_buf[..actual],
                actual,
            );
            for i in 0..actual {
                min_l.push(out_l_buf[i]);
                min_r.push(out_r_buf[i]);
            }
            pos += actual;
        }

        // Flush minimum-phase tail (see flush_samples rationale above)
        let zero_in = vec![0.0f64; chunk];
        let flush_blocks = (flush_samples + chunk - 1) / chunk;
        for _ in 0..flush_blocks {
            dsp_min.process_audio(&zero_in, &zero_in, &mut out_l_buf, &mut out_r_buf, chunk);
            for i in 0..chunk {
                min_l.push(out_l_buf[i]);
                min_r.push(out_r_buf[i]);
            }
        }

        // Trim minimum-phase OLA algorithmic latency + intrinsic filter delay
        // Minimum phase has algorithmic latency PLUS a small passband group delay, we remove both.
        let total_min_delay = ola_latency + min_filter_delay_samples;
        if total_min_delay < min_l.len() {
            min_l.drain(..total_min_delay);
            min_r.drain(..total_min_delay);
        }
        min_l.truncate(total_out);
        min_r.truncate(total_out);

        // ── HPSS envelope: native Rust (fast) ──
        set_status("Hybrid-Phase: generating HPSS envelope (Rust)...");
        file_state.gpu_pct.store(790, Ordering::Relaxed);
        crate::audio::hpss_native::generate_and_save(
            src_path,
            &hpss_src_l,
            &hpss_src_r,
            hpss_src_sr,
            &crate::audio::cancel_flag::get_atomic(),
        )
        .map_err(|e| format!("HPSS Native failed: {}", e))?;

        let envelope = crate::audio::hybrid_phase::load_external_envelope(
            src_path,
            total_out,
            settings.out_rate as f64,
        )
        .ok_or_else(|| "Hybrid-Phase: failed to load HPSS envelope".to_string())?;

        // Stereo-linked zero-crossing switch: one plan from the mid difference
        // applied to both channels, so L/R always switch at the same sample.
        let out_sr_f64 = out_rate as f64;
        let (blended_l, blended_r) = crate::audio::hybrid_phase::blend_outputs_stereo(
            &final_l, &min_l, &final_r, &min_r, &envelope, 0, out_sr_f64,
        );

        crate::aelog!(
            "[CONV] Hybrid-Phase blending complete: {} samples",
            blended_l.len()
        );

        // Note: .onset_envelope.json was already written by hpss_native::generate_and_save()

    Ok((blended_l, blended_r))
}
