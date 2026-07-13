use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;

/// ── Dithering and 9th-order Noise Shaping (Wannamaker 1992) ──
/// Performs final 24-bit TPDF dithering before DAC/Encoder to prevent truncation distortion.
/// This runs strictly AT THE END to ensure 24-bit steps aren't broken by intermediary DSP scaling.
pub fn apply_dithering_and_noise_shaping(samples_l: &mut [f64], samples_r: &mut [f64], sample_rate: u32) {
    let n = samples_l.len();
    if n == 0 {
        return;
    }

    // Two INDEPENDENT generators: drawing L and R noise from one sequential
    // stream leaves the channels' dither partially correlated, which images
    // the dither floor toward the phantom center. Independent streams are
    // the textbook requirement for stereo TPDF dither.
    let mut rng_l = SmallRng::from_entropy();
    let mut rng_r = SmallRng::from_entropy();
    // 24-bit output headroom: ±(1.0 / 2^23)
    let q_step = 1.0 / 8_388_608.0; 
    let half_q = 0.5 * q_step;

    // Wannamaker-9 NS coefficients are tuned for 44.1/48 kHz psycho-acoustic
    // weighting. At 88.2/96 kHz the curve is no longer optimal (most of the
    // shaped noise sits below the audible band where it doesn't help) and at
    // higher rates it's actively pointless. So restrict to sr ≤ 48 kHz; for
    // everything above that just use pure TPDF dither — the quantization
    // noise floor is already inaudible because the entire signal lives below
    // Nyquist/2 anyway.
    let use_noise_shaping = sample_rate <= 48000;

    // Wannamaker 9-order psychoacoustically optimal parameters (1992)
    let ns_coeffs = [2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847];
    let mut err_hist_l = [0.0f64; 9];
    let mut err_hist_r = [0.0f64; 9];

    if use_noise_shaping {
        crate::aelog!("[CONV] Applying 24-bit TPDF Dither + 9th-Order Noise Shaping (Wannamaker-1992)");
    } else {
        crate::aelog!(
            "[CONV] Applying 24-bit pure TPDF Dither (Noise Shaping disabled for sr={} > 48 kHz)",
            sample_rate
        );
    }

    for i in 0..n {
        let val_l = samples_l[i];
        let val_r = samples_r[i];

        let mut shaped_l = val_l;
        let mut shaped_r = val_r;
        
        if use_noise_shaping {
            for j in 0..9 {
                shaped_l += err_hist_l[j] * ns_coeffs[j];
                shaped_r += err_hist_r[j] * ns_coeffs[j];
            }
        }

        let dither_l = rng_l.gen_range(-half_q..half_q) + rng_l.gen_range(-half_q..half_q);
        let dither_r = rng_r.gen_range(-half_q..half_q) + rng_r.gen_range(-half_q..half_q);

        let quant_l = ((shaped_l + dither_l) / q_step).round() * q_step;
        let quant_r = ((shaped_r + dither_r) / q_step).round() * q_step;

        // Error feedback uses the UNCLAMPED quantized value — clamping inside
        // the loop would inject the clip error into the shaper and destabilize
        // it. The output itself is clamped below so dither/NS excursions can
        // never push a sample past the largest representable 24-bit code
        // (ffmpeg's f64→s32 handling of >1.0 values is build-dependent).
        for j in (1..9).rev() {
            err_hist_l[j] = err_hist_l[j - 1];
            err_hist_r[j] = err_hist_r[j - 1];
        }
        err_hist_l[0] = shaped_l - quant_l;
        err_hist_r[0] = shaped_r - quant_r;

        let peak_guard = 1.0 - q_step;
        samples_l[i] = quant_l.clamp(-peak_guard, peak_guard);
        samples_r[i] = quant_r.clamp(-peak_guard, peak_guard);
    }
}
