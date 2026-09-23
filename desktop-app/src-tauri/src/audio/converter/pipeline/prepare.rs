use crate::audio::converter::apodize::*;
use crate::audio::converter::decode::{decode_file, set_status};
use crate::audio::converter::dsp::lab;
use crate::audio::converter::types::{AudioFile, ConvertSettings, PreparedAudio};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Given a source sample rate, returns Some(family_base) for standard families, or None.
/// Standard families:
///   44100 Hz: 44.1, 88.2, 176.4, 352.8, 705.6 kHz
///   48000 Hz: 48, 96, 192, 384, 768 kHz
pub(crate) fn detect_family(src_rate: u32) -> Option<u32> {
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
/// `album_analysis` = pooled source forensics for this file's folder
/// (manager's album pre-scan); when Some, the per-file analysis is skipped
/// and the album-wide evidence drives the verdict so every sibling of the
/// folder receives the same cutoff.
///
/// Special error codes:
///   "BAD_RATE:<hz>"            — non-standard sample rate, file skipped
///   "SKIP_RATE:<src>:<target>" — source rate >= target, upsampling not needed
pub fn prepare_audio_phase(
    src_path: &Path,
    settings: &mut ConvertSettings,
    file_cancel: &AtomicBool,
    album_analysis: Option<&SourceAnalysis>,
) -> Result<PreparedAudio, String> {
    set_status(&format!(
        "Decoding: {}",
        src_path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let audio = decode_file(src_path)?;
    // Take ownership of the decoded samples immediately instead of cloning
    // them further down — nothing else reads audio.samples_*, and the old
    // clone doubled prep RAM (an extra 7 GB on a 38-minute 192 kHz file).
    let AudioFile {
        samples_l,
        samples_r,
        sample_rate,
        channels: source_channels,
        artist,
        title,
    } = audio;
    let mut audio_l = samples_l;
    let mut audio_r = samples_r;
    let total_input_samples = audio_l.len();

    // ── FS Multiplier: resolve out_rate from PGGB-style family detection ──
    match detect_family(sample_rate) {
        None => {
            // Non-standard sample rate — cannot process
            return Err(format!("BAD_RATE:{}", sample_rate));
        }
        Some(family_base) => {
            let target_rate = family_base * settings.fs_multiplier;
            if sample_rate >= target_rate {
                // Source is already at or above target — skip
                return Err(format!("SKIP_RATE:{}:{}", sample_rate, target_rate));
            }
            settings.out_rate = target_rate;
            crate::aelog!(
                "[CONV] FS{}: family {}kHz × {} = {}kHz (source: {}Hz)",
                settings.fs_multiplier,
                family_base / 1000,
                settings.fs_multiplier,
                target_rate / 1000,
                sample_rate
            );
        }
    }

    // ── LAB: shared source stats (BEFORE DC removal — the 16-bit grid test
    //    reads first-differences of the raw signal; DC offset shifts all
    //    differences by a constant and breaks the comb detection).  ──
    let need_stats = settings.lab.declip
        || settings.lab.adaptive_headroom
        || settings.lab.isp;
    let mut lab_stats: Option<lab::stats::SourceStats> = None;
    if need_stats {
        set_status("Lab: analyzing source...");
        lab_stats = lab::stats::analyze(&audio_l, &audio_r, sample_rate, file_cancel);
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".to_string());
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
            let r = (-2.0 * std::f64::consts::PI * fc / sample_rate as f64).exp();
            let mut y_l = 0.0;
            let mut y_r = 0.0;
            let mut x_prev_l = audio_l[0];
            let mut x_prev_r = audio_r[0];

            crate::aelog!(
                "[CONV] Applying 2 Hz IIR High-pass for dynamic DC removal (r={:.10})",
                r
            );
            for i in 0..total_input_samples {
                let x_l = audio_l[i];
                let x_r = audio_r[i];
                y_l = x_l - x_prev_l + r * y_l;
                y_r = x_r - x_prev_r + r * y_r;
                x_prev_l = x_l;
                x_prev_r = x_r;
                audio_l[i] = y_l;
                audio_r[i] = y_r;
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
            let dc_l = audio_l.iter().sum::<f64>() / total_input_samples as f64;
            let dc_r = audio_r.iter().sum::<f64>() / total_input_samples as f64;
            crate::aelog!(
                "[CONV] Static DC offset removed: L={:.6e}, R={:.6e}",
                dc_l, dc_r
            );
            for s in audio_l.iter_mut() {
                *s -= dc_l;
            }
            for s in audio_r.iter_mut() {
                *s -= dc_r;
            }
        }
    }

    // Accumulated lab tags, chain tokens, and per-feature outcomes for the UI report.
    let mut lab_tags: Vec<(String, String)> = Vec::new();
    let mut lab_chain: Vec<&'static str> = Vec::new();
    let mut lab_outcomes: Vec<lab::LabOutcome> = Vec::new();
    // Spans repaired by declip; ISP must leave them alone.
    let mut declip_spans_l: Vec<(usize, usize)> = Vec::new();
    let mut declip_spans_r: Vec<(usize, usize)> = Vec::new();

    // ── LAB: declip (pre-headroom scale, pre-DC already removed) ──
    if settings.lab.declip {
        if let Some(ref stats) = lab_stats {
            set_status("Lab: declipping source...");
            match lab::declip::run(&mut audio_l, &mut audio_r, stats, sample_rate, file_cancel) {
                Ok(report) => {
                    let anything_fixed = report.short_fixed + report.plateaus_fixed;
                    let tag_val = format!(
                        "thr={:.2}dBFS;reg={};s={};p={};unr={};tskip={};pct={:.1}",
                        report.threshold_dbfs, report.regions_total,
                        report.short_fixed, report.plateaus_fixed,
                        report.unrecoverable, report.transient_skipped, report.clipped_pct
                    );
                    lab_tags.push(("AURA_DECLIP".to_string(), tag_val));
                    if anything_fixed >= 1 {
                        lab_chain.push("DC");
                    }
                    crate::aelog!(
                        "[DECLIP] threshold={:.2}dBFS regions={} short={} plateaus={} unr={} longest={:.1}ms clipped={:.1}%",
                        report.threshold_dbfs, report.regions_total,
                        report.short_fixed, report.plateaus_fixed,
                        report.unrecoverable, report.longest_ms, report.clipped_pct
                    );
                    let level = if anything_fixed == 0 {
                        "none"
                    } else if report.unrecoverable > 0 || report.clipped_pct > 5.0 {
                        "warn"
                    } else {
                        "ok"
                    };
                    let mut text = format!(
                        "fixed {} short + {} plateaus (longest {:.1} ms, {:.1}% clipped)",
                        report.short_fixed, report.plateaus_fixed,
                        report.longest_ms, report.clipped_pct
                    );
                    if report.unrecoverable > 0 {
                        text.push_str(&format!(", {} too long", report.unrecoverable));
                    }
                    lab_outcomes.push(lab::LabOutcome { feature: "DECLIP", level, text });
                    declip_spans_l = report.repaired_l.clone();
                    declip_spans_r = report.repaired_r.clone();
                }
                Err(lab::declip::DeclipSkip::TooFew)
                | Err(lab::declip::DeclipSkip::NoSignature) => {
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "DECLIP",
                        level: "none",
                        text: "no clipping found".to_string(),
                    });
                }
                Err(lab::declip::DeclipSkip::NoPlateaus { longest }) => {
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "DECLIP",
                        level: "none",
                        text: format!(
                            "ceiling touches only (longest run {} samples, gate {}): a limiter, not a clipper",
                            longest,
                            lab::declip::MIN_RUN_GATE
                        ),
                    });
                }
                Err(lab::declip::DeclipSkip::Cancelled) => {}
            }
            if file_or_global_cancelled(file_cancel) {
                return Err("Cancelled".to_string());
            }
        }
    }

    // ── LAB: ISP scan + correction (ceiling = 1.0 linear, pre-headroom) ──
    if settings.lab.isp {
        set_status("Lab: intersample peak scan...");
        let isp_report = lab::isp::run(
            &mut audio_l, &mut audio_r,
            &declip_spans_l, &declip_spans_r,
            file_cancel,
        );
        if let Some(ref report) = isp_report {
            let tag_val = format!(
                "max={:+.2}dBTP;clusters={};fixed={};unfixed={};residual={:+.2}dBTP",
                report.max_dbtp, report.clusters, report.fixed,
                report.unfixed, report.residual_dbtp
            );
            lab_tags.push(("AURA_ISP".to_string(), tag_val));
            if report.fixed >= 1 {
                lab_chain.push("ISP");
            }
            crate::aelog!(
                "[ISP] max={:+.2}dBTP clusters={} fixed={} unfixed={} residual={:+.2}dBTP",
                report.max_dbtp, report.clusters, report.fixed,
                report.unfixed, report.residual_dbtp
            );
        }
        let isp_outcome = match &isp_report {
            Some(r) if r.unfixed > 0 => lab::LabOutcome {
                feature: "ISP",
                level: "warn",
                text: format!(
                    "{} intersample overs could not be corrected (max {:+.2} dBTP)",
                    r.unfixed, r.max_dbtp
                ),
            },
            Some(r) if r.fixed > 0 => lab::LabOutcome {
                feature: "ISP",
                level: "ok",
                text: format!(
                    "{} intersample overs corrected (max {:+.2} dBTP \u{2192} {:+.2} dBTP)",
                    r.fixed, r.max_dbtp, r.residual_dbtp
                ),
            },
            _ => lab::LabOutcome {
                feature: "ISP",
                level: "none",
                text: "no intersample overs found".to_string(),
            },
        };
        lab_outcomes.push(isp_outcome);
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".to_string());
        }
    }

    // ── Subsonic filter (optional, off by default) ──────────────────────
    //
    // Upstream runs this straight after DC blocking. Here it runs after the
    // repairs instead: Declip, the de-limiter and ISP read sample values
    // against the rail, and a high-pass in front of them would tilt the flat
    // tops and the ceiling they look for. Everything from here on — the
    // headroom decision, the apodizer's forensics, the filter — sees the
    // source without what it removes. Spec and tests: apodize.rs,
    // `apply_subsonic_filter`.
    if settings.subsonic_hz != 0 {
        let rep = apply_subsonic_filter(
            &mut audio_l,
            &mut audio_r,
            sample_rate,
            settings.subsonic_hz,
            file_cancel,
        )?;
        lab_chain.push(match settings.subsonic_hz {
            10 => "SUB10",
            15 => "SUB15",
            _ => "SUB20",
        });
        lab_outcomes.push(lab::LabOutcome {
            feature: "SUB",
            level: "ok",
            text: format!(
                "linear-phase high-pass below {} Hz ({} taps); removed {:.1} / {:.1} dBFS RMS",
                settings.subsonic_hz, rep.taps, rep.removed_rms_db[0], rep.removed_rms_db[1]
            ),
        });
    }

    // ── Headroom: the ceiling the output is normalised to ──────────────
    //
    // This used to be a gain applied right here, before the filter. It was
    // cancelled exactly at the other end: the true-peak normaliser sets an
    // ABSOLUTE ceiling, so whatever came off here it took off that much less.
    // Two conversions of one file at -3.0 dB and -0.5 dB came out at the same
    // level — -0.50 dBFS and -15.69 LUFS both — and v1.2.1 without any lab
    // pass did the same. The control did nothing on any loud master.
    //
    // It names the ceiling now, which is what its label always said: Off
    // leaves the shipped -0.5 dBTP, -3 dB puts the output peak on -3.0 dBTP.
    // Nothing is scaled before the filter any more, so the source reaches the
    // convolution exactly as it was decoded and repaired.
    let mut true_peak_target_dbtp =
        crate::audio::converter::dsp::true_peak::TARGET_TRUE_PEAK_DBTP;
    if settings.headroom_db < 0.0 {
        let lower_ceiling = if settings.lab.adaptive_headroom {
            let decision = lab::headroom::decide(lab_stats.as_ref(), settings.headroom_db);
            if decision.skip_gain {
                crate::aelog!(
                    "[AHR] Keeping the shipped ceiling instead of {} dB: {}",
                    settings.headroom_db, decision.reason
                );
                // Emit AHR token and AURA_HEADROOM tag.
                lab_chain.push("AHR");
                let (peak_str, enob_str, grid_str) = match lab_stats.as_ref() {
                    Some(s) => (
                        format!("{:.2}dBFS", s.peak_dbfs),
                        s.enob.map(|e| format!("{:.1}", e)).unwrap_or_else(|| "none".to_string()),
                        if s.grid16 { "yes" } else { "no" }.to_string(),
                    ),
                    None => ("N/A".to_string(), "none".to_string(), "no".to_string()),
                };
                let headroom_label = format!("{:.0}dB", settings.headroom_db.abs());
                lab_tags.push((
                    "AURA_HEADROOM".to_string(),
                    format!(
                        "{}skipped;peak={};enob={};grid16={}",
                        headroom_label, peak_str, enob_str, grid_str
                    ),
                ));
                // AHR ok: the ceiling was left where it ships.
                let peak_label = match lab_stats.as_ref() {
                    Some(s) => format!("{:.2} dBFS", s.peak_dbfs),
                    None => "unknown".to_string(),
                };
                lab_outcomes.push(lab::LabOutcome {
                    feature: "AHR",
                    level: "ok",
                    text: format!(
                        "ceiling kept at {:.1} dBTP instead of {:.1}: {}, peak {}",
                        crate::audio::converter::dsp::true_peak::TARGET_TRUE_PEAK_DBTP,
                        settings.headroom_db, decision.reason, peak_label
                    ),
                });
                false
            } else {
                true
            }
        } else {
            true
        };
        if lower_ceiling {
            true_peak_target_dbtp = settings.headroom_db;
            crate::aelog!(
                "[CONV] Headroom: output ceiling {:.1} dBTP (shipped {:.1})",
                settings.headroom_db,
                crate::audio::converter::dsp::true_peak::TARGET_TRUE_PEAK_DBTP
            );
            // AHR enabled but it had no reason to intervene.
            if settings.lab.adaptive_headroom {
                let reason = lab::headroom::decide(lab_stats.as_ref(), settings.headroom_db).reason;
                lab_outcomes.push(lab::LabOutcome {
                    feature: "AHR",
                    level: "none",
                    text: format!(
                        "ceiling lowered to {:.1} dBTP as asked: {}",
                        settings.headroom_db, reason
                    ),
                });
            }
        }
    }

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
    // AA verdict for the queue UI: (treated, tooltip). None = AA disabled.
    let mut aa_ui: Option<(bool, String)> = None;

    if settings.adaptive_apodizer {
        // v3 source forensics runs at ANY container rate: for hi-res
        // containers the cliff detector unmasks upsampled 44.1/48k masters
        // ("fake hi-res") and the ring detector then works against the
        // ORIGINAL Nyquist. True hi-res sources produce no verdict and
        // remain untouched, exactly like the old skip — but now by
        // measurement rather than by container rate.
        //
        // v3.1: when the manager pre-scanned this file's folder, the pooled
        // album evidence replaces the per-file analysis — the whole folder
        // then shares one verdict (fc depends only on the analysis, so
        // siblings at different container rates still get identical fc).
        let own_analysis;
        let analysis: Option<&SourceAnalysis> = match album_analysis {
            Some(pooled) => Some(pooled),
            None => {
                set_status("Adaptive Apodizer: source analysis...");
                own_analysis =
                    analyze_source(&audio_l, &audio_r, sample_rate, file_cancel);
                own_analysis.as_ref()
            }
        };
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".to_string());
        }
        let verdict = analysis.map(|a| decide_apodizer_verdict(a, sample_rate));
        let album_mark = if album_analysis.is_some() { " (album verdict)" } else { "" };
        let plan = verdict.as_ref().and_then(|v| v.plan.clone());
        aa_ui = Some(match (&plan, &verdict) {
            (Some(p), _) => (
                true,
                format!(
                    "Adaptive Apodizer: cutoff {:.0} Hz, {} taps, β {:.0} — {}{}",
                    p.fc_hz, p.taps, p.beta, p.reason, album_mark
                ),
            ),
            (None, Some(v)) => (
                false,
                format!(
                    "Adaptive Apodizer off: {}{}{}",
                    v.skip_note,
                    album_mark,
                    if settings.apodizing > 0 && sample_rate <= 48_000 {
                        " — static preset applied instead"
                    } else {
                        ""
                    }
                ),
            ),
            (None, None) => (
                false,
                "Adaptive Apodizer off: track too short or quiet to analyze".to_string(),
            ),
        });
        if let Some(plan) = plan {
            crate::aelog!(
                "[CONV] Adaptive Apodizer v3: {}{} → fc = {:.0} Hz, {} taps, β = {:.0}",
                if album_analysis.is_some() { "[album verdict] " } else { "" },
                plan.reason,
                plan.fc_hz,
                plan.taps,
                plan.beta
            );
            let nyquist = sample_rate as f64 / 2.0;
            let fc_norm = (plan.fc_hz / nyquist).min(0.99);
            let apod_coeffs = generate_apodizing_coeffs_adaptive(
                sample_rate,
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
            // AA is a stage of the chain now, not a separate segment of the
            // filename: it goes in with everything else, in the order it ran.
            lab_chain.push("AA");
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
                sample_rate,
                settings.apodizing,
                settings.use_gpu,
                settings.precision,
                file_cancel,
            )?;
            if settings.apodizing > 0 && sample_rate <= 48000 {
                apod_tag = static_tag(settings.apodizing);
            }
        }
    } else {
        apply_apodizing(
            &mut audio_l,
            &mut audio_r,
            sample_rate,
            settings.apodizing,
            settings.use_gpu,
            settings.precision,
            file_cancel,
        )?;
        // apply_apodizing is a no-op for strength=0 or hi-res sources.
        if settings.apodizing > 0 && sample_rate <= 48000 {
            apod_tag = static_tag(settings.apodizing);
        }
    }

    if file_or_global_cancelled(file_cancel) {
        return Err("Cancelled".to_string());
    }

    Ok(PreparedAudio {
        audio_l,
        audio_r,
        sample_rate: sample_rate,
        source_channels,
        total_input_samples,
        artist: artist,
        title: title,
        apod_tag,
        aa_ui,
        lab_tags,
        lab_chain,
        true_peak_target_dbtp,
        lab_outcomes,
    })
}
