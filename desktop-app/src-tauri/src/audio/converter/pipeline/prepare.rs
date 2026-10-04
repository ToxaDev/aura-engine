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
        lossy,
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

    let SourceHead {
        lab_stats,
        mut lab_tags,
        mut lab_chain,
        mut lab_outcomes,
        declip_spans_l,
        declip_spans_r,
        hot_spans_l,
        hot_spans_r,
    } = source_head(&mut audio_l, &mut audio_r, sample_rate, lossy, settings, file_cancel)?;

    // ── LAB: ISP scan + correction (ceiling = 1.0 linear, pre-headroom) ──
    if settings.lab.isp {
        set_status("Lab: intersample peak scan...");
        let isp_report = lab::isp::run(
            &mut audio_l, &mut audio_r,
            &declip_spans_l, &declip_spans_r,
            &hot_spans_l, &hot_spans_r,
            file_cancel,
        );
        isp_outcome(isp_report.as_ref(), &mut lab_tags, &mut lab_chain, &mut lab_outcomes);
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".to_string());
        }
    }

    // ── Subsonic filter (optional, off by default) ──────────────────────
    //
    // After the repairs, not straight after DC blocking: Declip and ISP read
    // sample values against the rail, and a high-pass in front of them would
    // tilt the flat tops and the ceiling they look for. Everything from here on — the
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
        subsonic_outcome(settings.subsonic_hz, &rep, &mut lab_chain, &mut lab_outcomes);
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
    let true_peak_target_dbtp =
        output_ceiling(settings, lab_stats.as_ref(), &mut lab_tags, &mut lab_chain, &mut lab_outcomes);

    // AA verdict for the queue UI: (treated, tooltip). None = AA disabled.
    let mut aa_ui: Option<(bool, String)> = None;
    let mut plan: Option<ApodizerPlan> = None;
    if settings.adaptive_apodizer {
        let (p, ui) = adaptive_verdict(&audio_l, &audio_r, sample_rate, settings, file_cancel, album_analysis)?;
        plan = p;
        aa_ui = Some(ui);
    }
    // Filename tag for whichever apodizing actually runs.
    let apod_tag = apply_apodizer(
        &mut audio_l,
        &mut audio_r,
        sample_rate,
        settings,
        plan.as_ref(),
        false,
        &mut lab_chain,
        file_cancel,
    )?;

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
        album: None,
    })
}

/// What `prepare_audio_phase` does to a decoded track before the stages that
/// can also run on a stream: the source statistics (on the raw samples), DC
/// removal and declip, with what they report and the spans declip repaired.
/// The player's instant start runs it on the whole track and streams the rest
/// (`player::source_stages`), so both come out the same to the bit.
pub(crate) struct SourceHead {
    pub lab_stats: Option<lab::stats::SourceStats>,
    pub lab_tags: Vec<(String, String)>,
    pub lab_chain: Vec<&'static str>,
    pub lab_outcomes: Vec<lab::LabOutcome>,
    pub declip_spans_l: Vec<(usize, usize)>,
    pub declip_spans_r: Vec<(usize, usize)>,
    /// The samples over full scale as decoded, before DC removal
    /// (`isp::hot_spans`; empty with ISP off): the repair leaves the clusters
    /// that reach them whole.
    pub hot_spans_l: Vec<(usize, usize)>,
    pub hot_spans_r: Vec<(usize, usize)>,
}

/// `lossy`: the lossy codec the track was decoded from (`AudioFile::lossy`).
pub(crate) fn source_head(
    audio_l: &mut Vec<f64>,
    audio_r: &mut Vec<f64>,
    sample_rate: u32,
    lossy: Option<&str>,
    settings: &ConvertSettings,
    file_cancel: &AtomicBool,
) -> Result<SourceHead, String> {
    let total_input_samples = audio_l.len();

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

    // The samples over full scale as decoded — the source's level there, not
    // an over between its samples: the ISP leaves them whole. Found before DC
    // removal moves them; an integer source has none.
    let (hot_spans_l, hot_spans_r) = if settings.lab.isp {
        (lab::isp::hot_spans(audio_l, 0), lab::isp::hot_spans(audio_r, 0))
    } else {
        (Vec::new(), Vec::new())
    };

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
    // Not on a lossy source: its peaks are the codec's, and since the decoder
    // stopped clamping them (1.2.9) nothing has flattened them. What a lossy
    // decode shows the gate is its overs past full scale (all of them in the
    // histogram's top bin, a rail at 0 dBFS) or a limiter's ceiling the codec
    // blurred — runs over the rail, none of them flat — and the repair drew
    // arcs over real samples: 1.2 % of a loud MP3 rewritten at −30 dB to the
    // signal, another's peak raised 2.8 dB (the ceiling then took the whole
    // track down). A clipped master's flat tops do not survive a codec: on
    // its lossy copies the gate stays shut.
    if let (true, Some(codec)) = (settings.lab.declip, lossy) {
        crate::aelog!("[DECLIP] lossy source ({}): the peaks are the codec's — standing down", codec);
        lab_outcomes.push(lab::LabOutcome {
            feature: "DECLIP",
            level: "none",
            text: format!("lossy source ({}): the peaks are the codec's, not clipping — left as decoded", codec),
        });
    } else if settings.lab.declip {
        if let Some(ref stats) = lab_stats {
            set_status("Lab: declipping source...");
            match lab::declip::run(audio_l, audio_r, stats, sample_rate, file_cancel) {
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

    Ok(SourceHead { lab_stats, lab_tags, lab_chain, lab_outcomes, declip_spans_l, declip_spans_r, hot_spans_l, hot_spans_r })
}

/// The true-peak ceiling the output is normalised to: the rack's Headroom,
/// or the shipped one where the Adaptive Headroom keeps it (from the raw
/// source's statistics, `source_head`), with what that decision reports.
pub(crate) fn output_ceiling(
    settings: &ConvertSettings,
    lab_stats: Option<&lab::stats::SourceStats>,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) -> f64 {
    let mut true_peak_target_dbtp =
        crate::audio::converter::dsp::true_peak::TARGET_TRUE_PEAK_DBTP;
    if settings.headroom_db < 0.0 {
        let lower_ceiling = if settings.lab.adaptive_headroom {
            let decision = lab::headroom::decide(lab_stats, settings.headroom_db);
            if decision.skip_gain {
                crate::aelog!(
                    "[AHR] Keeping the shipped ceiling instead of {} dB: {}",
                    settings.headroom_db, decision.reason
                );
                // Emit AHR token and AURA_HEADROOM tag.
                lab_chain.push("AHR");
                let (peak_str, enob_str, grid_str) = match lab_stats {
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
                let peak_label = match lab_stats {
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
                let reason = lab::headroom::decide(lab_stats, settings.headroom_db).reason;
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
    true_peak_target_dbtp
}

/// What the intersample repair reports on a track (`isp::run`'s report, or
/// a stream's: `IspStream::finish`): its tag, its token when it fixed
/// anything, its log line and its outcome.
pub(crate) fn isp_outcome(
    report: Option<&lab::isp::IspReport>,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    // The clusters left whole at the source's samples over full scale: said
    // only where there are any, so a CD's tag and lines read as they did.
    let hot = report.map_or(0, |r| r.hot);
    let hot_text = |n: usize| format!("{} source peaks above full scale left whole for the output level to lower", n);
    if let Some(report) = report {
        let mut tag_val = format!(
            "max={:+.2}dBTP;clusters={};fixed={};unfixed={};residual={:+.2}dBTP",
            report.max_dbtp, report.clusters, report.fixed,
            report.unfixed, report.residual_dbtp
        );
        if hot > 0 {
            tag_val.push_str(&format!(";hot={}", hot));
        }
        lab_tags.push(("AURA_ISP".to_string(), tag_val));
        if report.fixed >= 1 {
            lab_chain.push("ISP");
        }
        crate::aelog!(
            "[ISP] max={:+.2}dBTP clusters={} fixed={} unfixed={} residual={:+.2}dBTP{}",
            report.max_dbtp, report.clusters, report.fixed,
            report.unfixed, report.residual_dbtp,
            if hot > 0 { format!(" hot={}", hot) } else { String::new() }
        );
    }
    let tail = if hot > 0 { format!("; {}", hot_text(hot)) } else { String::new() };
    let isp_outcome = match report {
        Some(r) if r.unfixed > 0 => lab::LabOutcome {
            feature: "ISP",
            level: "warn",
            text: format!(
                "{} intersample overs could not be corrected (max {:+.2} dBTP){}",
                r.unfixed, r.max_dbtp, tail
            ),
        },
        Some(r) if r.fixed > 0 => lab::LabOutcome {
            feature: "ISP",
            level: "ok",
            text: format!(
                "{} intersample overs corrected (max {:+.2} dBTP \u{2192} {:+.2} dBTP){}",
                r.fixed, r.max_dbtp, r.residual_dbtp, tail
            ),
        },
        Some(r) if r.hot > 0 => lab::LabOutcome {
            feature: "ISP",
            level: "none",
            text: format!(
                "no intersample overs between samples; {} (max {:+.2} dBTP)",
                hot_text(r.hot), r.max_dbtp
            ),
        },
        _ => lab::LabOutcome {
            feature: "ISP",
            level: "none",
            text: "no intersample overs found".to_string(),
        },
    };
    lab_outcomes.push(isp_outcome);
}

/// What the subsonic filter reports on a track: its token and outcome.
pub(crate) fn subsonic_outcome(
    corner_hz: u32,
    rep: &SubsonicReport,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    lab_chain.push(match corner_hz {
        10 => "SUB10",
        15 => "SUB15",
        _ => "SUB20",
    });
    lab_outcomes.push(lab::LabOutcome {
        feature: "SUB",
        level: "ok",
        text: format!(
            "linear-phase high-pass below {} Hz ({} taps); removed {:.1} / {:.1} dBFS RMS",
            corner_hz, rep.taps, rep.removed_rms_db[0], rep.removed_rms_db[1]
        ),
    });
}

/// The Adaptive Apodizer's verdict on a track as the stages before it left
/// it: the plan it treats the track with (None: it declines), and what the
/// queue shows for it, (treated, tooltip).
pub(crate) fn adaptive_verdict(
    audio_l: &[f64],
    audio_r: &[f64],
    sample_rate: u32,
    settings: &ConvertSettings,
    file_cancel: &AtomicBool,
    album_analysis: Option<&SourceAnalysis>,
) -> Result<(Option<ApodizerPlan>, (bool, String)), String> {
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
            own_analysis = analyze_source(audio_l, audio_r, sample_rate, file_cancel);
            own_analysis.as_ref()
        }
    };
    if file_or_global_cancelled(file_cancel) {
        return Err("Cancelled".to_string());
    }
    let verdict = analysis.map(|a| decide_apodizer_verdict(a, sample_rate));
    let album_mark = if album_analysis.is_some() { " (album verdict)" } else { "" };
    let plan = verdict.as_ref().and_then(|v| v.plan.clone());
    let aa_ui = match (&plan, &verdict) {
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
    };
    match &plan {
        Some(plan) => crate::aelog!(
            "[CONV] Adaptive Apodizer v3: {}{} → fc = {:.0} Hz, {} taps, β = {:.0}",
            if album_analysis.is_some() { "[album verdict] " } else { "" },
            plan.reason,
            plan.fc_hz,
            plan.taps,
            plan.beta
        ),
        // No actionable signature. Do NOT swallow the user's static
        // preset: enabling "Adaptive" must never silently disable a
        // manually selected strength on clean recordings.
        None if settings.apodizing > 0 => crate::aelog!(
            "[CONV] Adaptive Apodizer: no actionable signature — falling back to static preset (strength={})",
            settings.apodizing
        ),
        None => crate::aelog!(
            "[CONV] Adaptive Apodizer: no actionable signature, leaving source untouched"
        ),
    }
    Ok((plan, aa_ui))
}

/// The apodizer a track gets: the Adaptive Apodizer's `plan`, else the
/// static preset (a no-op for strength 0 and hi-res sources) — which, with
/// `static_done`, the track has had already (Instant start's stream runs
/// it). Returns the filename tag of a static preset.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_apodizer(
    audio_l: &mut Vec<f64>,
    audio_r: &mut Vec<f64>,
    sample_rate: u32,
    settings: &ConvertSettings,
    plan: Option<&ApodizerPlan>,
    static_done: bool,
    lab_chain: &mut Vec<&'static str>,
    file_cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    if let Some(plan) = plan {
        let nyquist = sample_rate as f64 / 2.0;
        let fc_norm = (plan.fc_hz / nyquist).min(0.99);
        let apod_coeffs = generate_apodizing_coeffs_adaptive(sample_rate, fc_norm, plan.taps, plan.beta);
        apply_custom_apodizing(audio_l, audio_r, &apod_coeffs, settings.use_gpu, settings.precision, file_cancel)?;
        // AA is a stage of the chain now, not a separate segment of the
        // filename: it goes in with everything else, in the order it ran.
        lab_chain.push("AA");
        return Ok(None);
    }
    if !static_done {
        apply_apodizing(
            audio_l,
            audio_r,
            sample_rate,
            settings.apodizing,
            settings.use_gpu,
            settings.precision,
            file_cancel,
        )?;
    }
    Ok(static_apod_tag(settings, sample_rate))
}

/// The filename tag of the static apodizer preset, where it runs
/// (`apply_apodizing` is a no-op for strength 0 or hi-res sources).
pub(crate) fn static_apod_tag(settings: &ConvertSettings, sample_rate: u32) -> Option<String> {
    match settings.apodizing {
        1 if sample_rate <= 48000 => Some("Apod".to_string()),
        2 if sample_rate <= 48000 => Some("Apod-M".to_string()),
        3 if sample_rate <= 48000 => Some("Apod-S".to_string()),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A loud lossy decode as declip's gate sees one: a tone up to full scale
    /// (997 Hz: its crests fall on every phase of the sample grid, so the bins
    /// under full scale fill) and, now and then, a crest riding over it for
    /// some 37 samples — overs past full scale, all of them in the
    /// statistics' top bin, long enough for the gate.
    fn overs() -> Vec<f64> {
        let mut x: Vec<f64> = (0..44_100).map(|i| 0.99999 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / 44_100.0).sin()).collect();
        for at in (1_000..44_000).step_by(2_000) {
            for k in 0..100 {
                x[at + k] = 1.2 * (std::f64::consts::PI * k as f64 / 100.0).sin();
            }
        }
        x
    }

    /// A lossy source is not declipped: its peaks past full scale are the
    /// codec's, and the repair drew arcs over real samples (1.2 % of a loud
    /// MP3 rewritten at −30 dB to the signal, another's peak raised 2.8 dB).
    /// Its badge says why. The same samples from a lossless source still
    /// open the gate — so the converter's output changes for lossy files only.
    #[test]
    fn declip_leaves_a_lossy_source_as_decoded() {
        let s = crate::player::settings::PlayerSettings {
            declip: true,
            isp: false,
            subsonic_hz: 0,
            adaptive_headroom: false,
            ..Default::default()
        }
        .to_engine();
        let cancel = AtomicBool::new(false);
        let x = overs();
        // DC removal alone: what a lossy source comes out as.
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let dc_only: Vec<f64> = x.iter().map(|v| v - mean).collect();

        let (mut l, mut r) = (x.clone(), x.clone());
        let lossy = source_head(&mut l, &mut r, 44_100, Some("MP3"), &s, &cancel).expect("the head");
        assert_eq!(l, dc_only, "a lossy source: declip leaves it");
        assert_eq!(r, dc_only);
        assert!(!lossy.lab_chain.contains(&"DC"), "no declip token: {:?}", lossy.lab_chain);
        assert!(lossy.declip_spans_l.is_empty() && lossy.declip_spans_r.is_empty());
        let o = lossy.lab_outcomes.iter().find(|o| o.feature == "DECLIP").expect("a DECLIP outcome");
        assert_eq!((o.level, o.text.as_str()), ("none", "lossy source (MP3): the peaks are the codec's, not clipping — left as decoded"));

        let (mut l, mut r) = (x.clone(), x);
        let lossless = source_head(&mut l, &mut r, 44_100, None, &s, &cancel).expect("the head");
        assert!(lossless.lab_chain.contains(&"DC"), "the gate opens on these samples: {:?}", lossless.lab_outcomes.iter().map(|o| &o.text).collect::<Vec<_>>());
        assert_ne!(l, dc_only, "a lossless source is repaired as before");
    }

    /// The samples over full scale are found as decoded, before DC removal
    /// moves them: one at 1.05 that the mean takes under full scale counts,
    /// the rail at −1.0 that it takes past full scale does not. With the
    /// repair off, none are looked for.
    #[test]
    fn the_head_finds_the_samples_over_full_scale_before_dc_removal() {
        let cancel = AtomicBool::new(false);
        let mut x: Vec<f64> = (0..44_100).map(|i| 0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44_100.0).sin() + 0.06).collect();
        x[1_000] = 1.05;
        x[2_000] = -1.0;
        x[3_000] = 1.2;
        x[3_001] = 1.3;
        for isp in [true, false] {
            let s = crate::player::settings::PlayerSettings { isp, declip: false, iir_dc_blocking: false, adaptive_headroom: false, ..Default::default() }
                .to_engine();
            let (mut l, mut r) = (x.clone(), x.iter().map(|v| v * 0.5).collect::<Vec<f64>>());
            let head = source_head(&mut l, &mut r, 44_100, Some("MP3"), &s, &cancel).expect("the head");
            assert!(l[1_000] < 1.0 && l[2_000] < -1.0, "the mean moved both: {} {}", l[1_000], l[2_000]);
            if isp {
                assert_eq!(head.hot_spans_l, vec![(1_000, 1_001), (3_000, 3_002)]);
            } else {
                assert!(head.hot_spans_l.is_empty());
            }
            assert!(head.hot_spans_r.is_empty());
        }
    }

    /// What the repair says of a track: a CD's tag and lines as before; with
    /// clusters left whole at the source's samples over full scale, the tag
    /// counts them and the line says so — after what was corrected, or on its
    /// own when nothing between samples was over.
    #[test]
    fn the_isp_outcome_says_what_it_left_whole() {
        let rep = |fixed: usize, unfixed: usize, hot: usize| lab::isp::IspReport {
            max_dbtp: 7.42,
            clusters: fixed + unfixed,
            fixed,
            unfixed,
            hot,
            residual_dbtp: 6.81,
            fixed_spans_l: Vec::new(),
            fixed_spans_r: Vec::new(),
        };
        let say = |r: Option<&lab::isp::IspReport>| {
            let (mut tags, mut chain, mut outcomes) = (Vec::new(), Vec::new(), Vec::new());
            isp_outcome(r, &mut tags, &mut chain, &mut outcomes);
            let tag = tags.iter().find(|(k, _)| k == "AURA_ISP").map(|(_, v)| v.clone());
            (tag, chain, outcomes[0].level, outcomes[0].text.clone())
        };
        let (tag, chain, level, text) = say(Some(&rep(10, 2, 0)));
        assert_eq!(tag.as_deref(), Some("max=+7.42dBTP;clusters=12;fixed=10;unfixed=2;residual=+6.81dBTP"));
        assert_eq!((chain, level, text.as_str()), (vec!["ISP"], "warn", "2 intersample overs could not be corrected (max +7.42 dBTP)"));
        let (tag, _, level, text) = say(Some(&rep(150, 1, 3_976)));
        assert_eq!(tag.as_deref(), Some("max=+7.42dBTP;clusters=151;fixed=150;unfixed=1;residual=+6.81dBTP;hot=3976"));
        assert_eq!((level, text.as_str()), ("warn",
            "1 intersample overs could not be corrected (max +7.42 dBTP); 3976 source peaks above full scale left whole for the output level to lower"));
        let (_, chain, level, text) = say(Some(&rep(150, 0, 3_976)));
        assert_eq!((chain, level, text.as_str()), (vec!["ISP"], "ok",
            "150 intersample overs corrected (max +7.42 dBTP \u{2192} +6.81 dBTP); 3976 source peaks above full scale left whole for the output level to lower"));
        let (tag, chain, level, text) = say(Some(&rep(0, 0, 3_976)));
        assert_eq!(tag.as_deref(), Some("max=+7.42dBTP;clusters=0;fixed=0;unfixed=0;residual=+6.81dBTP;hot=3976"));
        assert_eq!((chain, level, text.as_str()), (Vec::<&str>::new(), "none",
            "no intersample overs between samples; 3976 source peaks above full scale left whole for the output level to lower (max +7.42 dBTP)"));
        let (tag, _, level, text) = say(None);
        assert_eq!((tag, level, text.as_str()), (None, "none", "no intersample overs found"));
    }

    /// Error energy against the music's in each band, dB: `err` and `a`
    /// summed over both channels.
    pub(crate) fn band_error_db(err: (&[f64], &[f64]), a: (&[f64], &[f64]), rate: u32, bands: &[(f64, f64)]) -> Vec<f64> {
        let n = a.0.len();
        let mut planner = realfft::RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let power = |x: &[f64]| -> Vec<f64> {
            let mut buf = x.to_vec();
            let mut spec = fft.make_output_vec();
            fft.process(&mut buf, &mut spec).expect("the excerpt's spectrum");
            spec.iter().map(|c| c.norm_sqr()).collect()
        };
        let (pe, pa) = ([power(err.0), power(err.1)], [power(a.0), power(a.1)]);
        bands
            .iter()
            .map(|&(lo, hi)| {
                let sum = |p: &[Vec<f64>; 2]| -> f64 {
                    p.iter()
                        .map(|v| v.iter().enumerate().filter(|(k, _)| {
                            let f = *k as f64 * rate as f64 / n as f64;
                            f >= lo && f < hi
                        }).map(|(_, e)| e).sum::<f64>())
                        .sum()
                };
                10.0 * (sum(&pe).max(1e-300) / sum(&pa).max(1e-300)).log10()
            })
            .collect()
    }

    /// 24-bit PCM WAV, plain rounding.
    pub(crate) fn write_wav24(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
        let n = l.len().min(r.len());
        let data = (n * 6) as u32;
        let mut b = Vec::with_capacity(44 + n * 6);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 6).to_le_bytes());
        b.extend_from_slice(&6u16.to_le_bytes());
        b.extend_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            for v in [l[i], r[i]] {
                let q = (v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                b.extend_from_slice(&q.to_le_bytes()[..3]);
            }
        }
        std::fs::write(path, b).expect("write the WAV");
    }

    pub(crate) fn fnv_bits(l: &[f64], r: &[f64]) -> u64 {
        l.iter().chain(r).fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x.to_bits()).wrapping_mul(0x0000_0100_0000_01b3))
    }

    /// What the intersample repair does to real tracks, on the engine's own
    /// decode and the head the player's default rack runs (DC removal,
    /// declip standing down on a lossy source): its report, the samples it
    /// rewrote (and how many of them were over full scale as decoded), the
    /// error against the source as the repair receives it ("A"), whole and
    /// by band over an excerpt, and a hash of the result. With
    /// AURA_ISP_HOT_OUT, the excerpt as WAVs at the source rate, each under
    /// −0.5 dBTP and at A's RMS: A and the repair's (label
    /// AURA_ISP_HOT_LABEL). A measurement, not a test:
    /// AURA_ISP_HOT_FILES="a.flac|b.mp3" `cargo test --profile fast --bins
    /// hot_source_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn hot_source_harness() {
        use crate::audio::converter::dsp::true_peak::measure_true_peak;
        let files = std::env::var("AURA_ISP_HOT_FILES").unwrap_or_default();
        let out = std::env::var("AURA_ISP_HOT_OUT").ok().map(std::path::PathBuf::from);
        let label = std::env::var("AURA_ISP_HOT_LABEL").unwrap_or_else(|_| "isp".into());
        let secs = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(d);
        let (t0, t1) = (secs("AURA_ISP_HOT_T0", 30.0), secs("AURA_ISP_HOT_T1", 70.0));
        let bands = [(0.0, 2_000.0), (2_000.0, 5_000.0), (5_000.0, 10_000.0), (10_000.0, 1e9)];
        let s = crate::player::settings::PlayerSettings::default().to_engine();
        let cancel = AtomicBool::new(false);
        fn rms(x: &[f64], y: &[f64]) -> f64 {
            (x.iter().chain(y).map(|v| v * v).sum::<f64>() / (2 * x.len()).max(1) as f64).sqrt()
        }
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = Path::new(f.trim());
            let a = match decode_file(path) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("HARNESS {}: cannot decode: {e}", path.display());
                    continue;
                }
            };
            let rate = a.sample_rate;
            let raw_peak = a.samples_l.iter().chain(&a.samples_r).fold(0.0f64, |m, v| m.max(v.abs()));
            let hot_mask: Vec<bool> = a.samples_l.iter().chain(&a.samples_r).map(|v| v.abs() > 1.0).collect();
            let n_hot = hot_mask.iter().filter(|&&h| h).count();
            let (mut l, mut r) = (a.samples_l, a.samples_r);
            let n = l.len();
            let head = source_head(&mut l, &mut r, rate, a.lossy, &s, &cancel).expect("the head");
            let (al, ar) = (l.clone(), r.clone());
            // AURA_ISP_HOT_OFF: the repair as 1.5.0 made it, the hot spans not given.
            let off = std::env::var("AURA_ISP_HOT_OFF").is_ok_and(|v| v == "1");
            let (hot_l, hot_r): (&[(usize, usize)], &[(usize, usize)]) =
                if off { (&[], &[]) } else { (&head.hot_spans_l, &head.hot_spans_r) };
            let rep = lab::isp::run(&mut l, &mut r, &head.declip_spans_l, &head.declip_spans_r, hot_l, hot_r, &cancel);
            let changed: Vec<bool> = l.iter().chain(&r).zip(al.iter().chain(&ar)).map(|(x, y)| x.to_bits() != y.to_bits()).collect();
            let n_changed = changed.iter().filter(|&&c| c).count();
            let n_changed_hot = changed.iter().zip(&hot_mask).filter(|(&c, &h)| c && h).count();
            let (el, er): (Vec<f64>, Vec<f64>) = (l.iter().zip(&al).map(|(x, y)| x - y).collect(), r.iter().zip(&ar).map(|(x, y)| x - y).collect());
            let err_db = 20.0 * (rms(&el, &er).max(1e-300) / rms(&al, &ar)).log10();
            eprintln!(
                "HARNESS {} [{label}] {} Hz {} lossy={:?}: {} frames, raw peak {:+.2} dBFS, samples over full scale {} ({:.3} %)",
                path.file_name().unwrap_or_default().to_string_lossy(), rate, a.channels, a.lossy, n,
                20.0 * raw_peak.max(1e-300).log10(), n_hot, 100.0 * n_hot as f64 / (2 * n).max(1) as f64
            );
            match &rep {
                Some(rep) => eprintln!(
                    "HARNESS   report: max {:+.2} dBTP, clusters {}, fixed {}, unfixed {}, hot {}, residual {:+.2} dBTP; hot spans {} / {}",
                    rep.max_dbtp, rep.clusters, rep.fixed, rep.unfixed, rep.hot, rep.residual_dbtp, head.hot_spans_l.len(), head.hot_spans_r.len()
                ),
                None => eprintln!("HARNESS   report: none (nothing over); hot spans {} / {}", head.hot_spans_l.len(), head.hot_spans_r.len()),
            }
            eprintln!(
                "HARNESS   rewritten {} samples ({:.3} %), {} of them over full scale as decoded; error to A {:.1} dB; hash {:#018x}",
                n_changed, 100.0 * n_changed as f64 / (2 * n).max(1) as f64, n_changed_hot, err_db, fnv_bits(&l, &r)
            );
            let (i0, i1) = (((t0 * rate as f64) as usize).min(n), ((t1 * rate as f64) as usize).min(n));
            if i1 > i0 + 1_024 {
                let (xl, xr, yl, yr) = (&al[i0..i1], &ar[i0..i1], &l[i0..i1], &r[i0..i1]);
                let (dl, dr): (Vec<f64>, Vec<f64>) = (yl.iter().zip(xl).map(|(y, x)| y - x).collect(), yr.iter().zip(xr).map(|(y, x)| y - x).collect());
                let e = band_error_db((&dl, &dr), (xl, xr), rate, &bands);
                eprintln!(
                    "HARNESS   excerpt {t0:.0}-{t1:.0} s: error to A {:.1} dB; by band 0-2 / 2-5 / 5-10 / 10+ kHz: {:.1} / {:.1} / {:.1} / {:.1} dB",
                    20.0 * (rms(&dl, &dr).max(1e-300) / rms(xl, xr)).log10(), e[0], e[1], e[2], e[3]
                );
                if let Some(dir) = &out {
                    let _ = std::fs::create_dir_all(dir);
                    let ceiling = 10f64.powf(-0.5 / 20.0);
                    let ga = ceiling / measure_true_peak(xl, xr);
                    let (sal, sar): (Vec<f64>, Vec<f64>) = (xl.iter().map(|v| v * ga).collect(), xr.iter().map(|v| v * ga).collect());
                    let gy = ceiling / measure_true_peak(yl, yr);
                    let (mut syl, mut syr): (Vec<f64>, Vec<f64>) = (yl.iter().map(|v| v * gy).collect(), yr.iter().map(|v| v * gy).collect());
                    let m = rms(&sal, &sar) / rms(&syl, &syr);
                    syl.iter_mut().chain(syr.iter_mut()).for_each(|v| *v *= m);
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let tag = format!("{t0:.0}-{t1:.0}s");
                    write_wav24(&dir.join(format!("{stem} {tag} A-decode.wav")), rate, &sal, &sar);
                    write_wav24(&dir.join(format!("{stem} {tag} {label}.wav")), rate, &syl, &syr);
                    eprintln!("HARNESS   written: {} ({tag} A-decode / {label}), A at {:+.2} dB, {label} at {:+.2} dB then {:+.2} dB to A's RMS",
                        dir.display(), 20.0 * ga.log10(), 20.0 * gy.log10(), 20.0 * m.log10());
                }
            }
        }
    }
}
