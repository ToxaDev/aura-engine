use crate::audio::converter::apodize::*;
use crate::audio::converter::decode::{decode_file, set_status};
use crate::audio::converter::types::{ConvertSettings, PreparedAudio};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Given a source sample rate, returns Some(family_base) for standard families, or None.
/// Standard families:
///   44100 Hz: 44.1, 88.2, 176.4, 352.8, 705.6 kHz
///   48000 Hz: 48, 96, 192, 384, 768 kHz
fn detect_family(src_rate: u32) -> Option<u32> {
    for base in [44100u32, 48000u32] {
        let mut r = base;
        while r <= src_rate * 2 {
            if r == src_rate {
                return Some(base);
            }
            r *= 2;
        }
    }
    None
}

/// Runs in a background thread while the previous file is GPU-processing.
/// `file_cancel` = per-file AtomicBool from FileConvState (cancelled by X button).
///
/// Special error codes:
///   "BAD_RATE:<hz>"            — non-standard sample rate, file skipped
///   "SKIP_RATE:<src>:<target>" — source rate >= target, upsampling not needed
pub fn prepare_audio_phase(
    src_path: &Path,
    settings: &mut ConvertSettings,
    file_cancel: &AtomicBool,
) -> Result<PreparedAudio, String> {
    set_status(&format!(
        "Decoding: {}",
        src_path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut audio = decode_file(src_path)?;
    let total_input_samples = audio.samples_l.len();

    // ── FS Multiplier: resolve out_rate from PGGB-style family detection ──
    match detect_family(audio.sample_rate) {
        None => {
            // Non-standard sample rate — cannot process
            return Err(format!("BAD_RATE:{}", audio.sample_rate));
        }
        Some(family_base) => {
            let target_rate = family_base * settings.fs_multiplier;
            if audio.sample_rate >= target_rate {
                // Source is already at or above target — skip
                return Err(format!("SKIP_RATE:{}:{}", audio.sample_rate, target_rate));
            }
            settings.out_rate = target_rate;
            crate::aelog!(
                "[CONV] FS{}: family {}kHz × {} = {}kHz (source: {}Hz)",
                settings.fs_multiplier,
                family_base / 1000,
                settings.fs_multiplier,
                target_rate / 1000,
                audio.sample_rate
            );
        }
    }

    // DC BLOCKING: Remove constant offset or filter dynamic offset before convolution
    if total_input_samples > 0 {
        if settings.iir_dc_blocking {
            // Exact 1-pole HPF coefficient (closed form). The previous
            // approximation r = 1 - 2π·fc/fs drifts from the true pole
            // location at sub-Hz cutoffs; using exp(-2π·fc/fs) is exact and
            // costs one extra evaluation per file.
            let fc = 2.0_f64;
            let r = (-2.0 * std::f64::consts::PI * fc / audio.sample_rate as f64).exp();
            let mut y_l = 0.0;
            let mut y_r = 0.0;
            let mut x_prev_l = audio.samples_l[0];
            let mut x_prev_r = audio.samples_r[0];

            crate::aelog!(
                "[CONV] Applying 2 Hz IIR High-pass for dynamic DC removal (r={:.10})",
                r
            );
            for i in 0..total_input_samples {
                let x_l = audio.samples_l[i];
                let x_r = audio.samples_r[i];
                y_l = x_l - x_prev_l + r * y_l;
                y_r = x_r - x_prev_r + r * y_r;
                x_prev_l = x_l;
                x_prev_r = x_r;
                audio.samples_l[i] = y_l;
                audio.samples_r[i] = y_r;
            }
        } else {
            // Per-channel static DC removal (no threshold).
            //
            // An earlier version subtracted a single common offset
            // (sum_l + sum_r) / (2N) from BOTH channels. That removes only
            // the M-component of the DC and leaves the S-component intact:
            // L=+δ, R=−δ → common=0 → both channels keep their offset and
            // the stereo image gains a DC shift in side. Per-channel removal
            // zeros DC on each channel independently, which is what we want.
            let dc_l = audio.samples_l.iter().sum::<f64>() / total_input_samples as f64;
            let dc_r = audio.samples_r.iter().sum::<f64>() / total_input_samples as f64;
            crate::aelog!(
                "[CONV] Static DC offset removed: L={:.6e}, R={:.6e}",
                dc_l, dc_r
            );
            for s in audio.samples_l.iter_mut() {
                *s -= dc_l;
            }
            for s in audio.samples_r.iter_mut() {
                *s -= dc_r;
            }
        }
    }

    if settings.headroom_db < 0.0 {
        let gain = 10.0_f64.powf(settings.headroom_db / 20.0);
        crate::aelog!(
            "[CONV] Applying PRE-DSP Headroom: {:.1} dB (gain={:.6})",
            settings.headroom_db, gain
        );
        for s in audio.samples_l.iter_mut() {
            *s *= gain;
        }
        for s in audio.samples_r.iter_mut() {
            *s *= gain;
        }
    }

    let mut audio_l = audio.samples_l.clone();
    let mut audio_r = audio.samples_r.clone();

    // Filename tag for whichever apodizing actually runs below.
    let static_tag = |strength: u32| -> Option<String> {
        match strength {
            1 => Some("Apod".to_string()),
            2 => Some("Apod-M".to_string()),
            3 => Some("Apod-S".to_string()),
            _ => None,
        }
    };
    let mut apod_tag: Option<String> = None;

    if settings.adaptive_apodizer {
        // v3 source forensics runs at ANY container rate: for hi-res
        // containers the cliff detector unmasks upsampled 44.1/48k masters
        // ("fake hi-res") and the ring detector then works against the
        // ORIGINAL Nyquist. True hi-res sources produce no verdict and
        // remain untouched, exactly like the old skip — but now by
        // measurement rather than by container rate.
        set_status("Adaptive Apodizer: source analysis...");
        let analysis = analyze_source(&audio_l, &audio_r, audio.sample_rate, file_cancel);
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".to_string());
        }
        let plan = analysis
            .as_ref()
            .and_then(|a| decide_apodizer(a, audio.sample_rate));
        if let Some(plan) = plan {
            crate::aelog!(
                "[CONV] Adaptive Apodizer v3: {} → fc = {:.0} Hz, {} taps, β = {:.0}",
                plan.reason,
                plan.fc_hz,
                plan.taps,
                plan.beta
            );
            let nyquist = audio.sample_rate as f64 / 2.0;
            let fc_norm = (plan.fc_hz / nyquist).min(0.99);
            let apod_coeffs = generate_apodizing_coeffs_adaptive(
                audio.sample_rate,
                fc_norm,
                plan.taps,
                plan.beta,
            );
            apply_custom_apodizing(
                &mut audio_l,
                &mut audio_r,
                &apod_coeffs,
                settings.use_gpu,
                settings.precision,
                file_cancel,
            )?;
            apod_tag = Some("AA".to_string());
        } else {
            // No actionable signature. Do NOT swallow the user's static
            // preset: enabling "Adaptive" must never silently disable a
            // manually selected strength on clean recordings.
            // (apply_apodizing itself no-ops for strength=0 and hi-res.)
            if settings.apodizing > 0 {
                crate::aelog!(
                    "[CONV] Adaptive Apodizer: no actionable signature — falling back to static preset (strength={})",
                    settings.apodizing
                );
            } else {
                crate::aelog!(
                    "[CONV] Adaptive Apodizer: no actionable signature, leaving source untouched"
                );
            }
            apply_apodizing(
                &mut audio_l,
                &mut audio_r,
                audio.sample_rate,
                settings.apodizing,
                settings.use_gpu,
                settings.precision,
                file_cancel,
            )?;
            if settings.apodizing > 0 && audio.sample_rate <= 48000 {
                apod_tag = static_tag(settings.apodizing);
            }
        }
    } else {
        apply_apodizing(
            &mut audio_l,
            &mut audio_r,
            audio.sample_rate,
            settings.apodizing,
            settings.use_gpu,
            settings.precision,
            file_cancel,
        )?;
        // apply_apodizing is a no-op for strength=0 or hi-res sources.
        if settings.apodizing > 0 && audio.sample_rate <= 48000 {
            apod_tag = static_tag(settings.apodizing);
        }
    }

    if file_or_global_cancelled(file_cancel) {
        return Err("Cancelled".to_string());
    }

    Ok(PreparedAudio {
        audio_l,
        audio_r,
        sample_rate: audio.sample_rate,
        total_input_samples,
        artist: audio.artist,
        title: audio.title,
        apod_tag,
    })
}
