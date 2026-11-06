use crate::audio::converter::apodize::apply_subsonic_guard;
use crate::audio::converter::decode::set_status;
use crate::audio::converter::encode::{build_output_name, encode_flac};
use crate::audio::converter::state::*;
use crate::audio::converter::types::{AudioFile, ConvertSettings, PreparedAudio};
use crate::audio::processor::DspProcessor;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub use crate::audio::converter::pipeline::prepare::*;

/// Assemble the final tag list (VORBIS_COMMENT, or TXXX in a DSF): the
/// stamp every render carries ([`stamp`]: version, generation of the sound,
/// the stages that ran), then — when lab features fired — the lab source
/// tags with their top-level markers (AURAENGINE_LAB and AURA_CHAIN).
fn build_lab_vorbis_tags(
    lab_tags: &[(String, String)],
    lab_chain: &[&'static str],
    dsd: bool,
    rack_sig: Option<&str>,
    hp_sens: i8,
    hp_net: &str,
) -> Vec<(String, String)> {
    let mut out = crate::audio::converter::stamp::stamp_tags(lab_chain, dsd, rack_sig, hp_sens, hp_net);
    if lab_tags.is_empty() && lab_chain.is_empty() {
        return out;
    }
    out.push(("AURAENGINE_LAB".to_string(), "1".to_string()));
    if !lab_chain.is_empty() {
        out.push((
            "AURA_CHAIN".to_string(),
            crate::audio::converter::dsp::lab::in_chain_order(lab_chain).join(","),
        ));
    }
    out.extend_from_slice(lab_tags);
    out
}

/// A multistage render says so in its tags, with the chain it ran
/// (`AURA_UPSAMPLER`); so does a render of the hidden single-stage fallback
/// (`single`). A render that is single-stage either way carries no such tag,
/// so its file stays as it was.
fn push_upsampler_tag(
    tags: &mut Vec<(String, String)>,
    ms: Option<&[crate::audio::converter::dsp::ms_h1::H1Stage]>,
    single_fallback: bool,
) {
    use crate::audio::converter::dsp::ms_text;
    if let Some(chain) = ms {
        tags.push((ms_text::TAG_KEY.to_string(), ms_text::tag(chain)));
    } else if single_fallback {
        tags.push((ms_text::TAG_KEY.to_string(), ms_text::SINGLE_TAG.to_string()));
    }
}

/// The source's own tags and pictures for its render (`source_tags.rs`),
/// artist and title as the decode repaired them.
fn source_tags_for(audio: &AudioFile, src_path: &Path) -> crate::audio::converter::source_tags::SourceTags {
    let mut tags = crate::audio::converter::source_tags::read(src_path);
    tags.repair_artist_title(&audio.artist, &audio.title);
    tags
}

/// XTC: crosstalk cancellation on the finished render.
///
/// Runs on both paths and at the very end of both, on the output-rate stereo
/// pair, immediately before the true-peak gain is measured — so the ceiling
/// sees the filter's low-frequency lift and holds the output down to it. That
/// is why an XTC render arrives quieter, and why an A/B against it has to be
/// level-matched before it means anything.
///
/// Refuses rather than guesses. Geometry that is not a triangle, or a mono
/// file where there is no crosstalk to cancel, produce a declined outcome
/// with the reason in it — never a filter built from filled-in blanks.
fn run_xtc(
    enabled: bool,
    geom: &crate::audio::converter::types::XtcGeometry,
    out_l: &mut Vec<f64>,
    out_r: &mut Vec<f64>,
    out_rate: u32,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<crate::audio::converter::dsp::lab::LabOutcome>,
) {
    use crate::audio::converter::dsp::lab::xtc;
    if !xtc_geometry_usable(enabled, geom, lab_outcomes) {
        return;
    }
    set_status("XTC: crosstalk cancellation...");
    record_xtc(xtc::run(out_l, out_r, out_rate, geom), lab_tags, lab_chain, lab_outcomes);
}

/// Whether XTC is on and has a triangle to build for; the refusal is recorded
/// here, for both routes.
fn xtc_geometry_usable(
    enabled: bool,
    geom: &crate::audio::converter::types::XtcGeometry,
    lab_outcomes: &mut Vec<crate::audio::converter::dsp::lab::LabOutcome>,
) -> bool {
    use crate::audio::converter::dsp::lab;
    if !enabled {
        return false;
    }
    if !geom.is_usable() {
        crate::aelog!("[XTC] declined: listening geometry is missing or not a triangle");
        lab_outcomes.push(lab::LabOutcome {
            feature: "XTC",
            level: "warn",
            text: "declined — no usable listening geometry".to_string(),
        });
        return false;
    }
    true
}

/// The tag, the chain token, the log block and the rack row for one XTC run,
/// whichever route ran it.
fn record_xtc(
    result: Result<crate::audio::converter::dsp::lab::xtc::XtcReport, String>,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<crate::audio::converter::dsp::lab::LabOutcome>,
) {
    use crate::audio::converter::dsp::lab;
    match result {
        Ok(report) => {
            lab_tags.push(("AURA_XTC".to_string(), report.tag_value()));
            lab_chain.push("XTC");
            report.log();
            lab_outcomes.push(lab::LabOutcome {
                feature: "XTC",
                level: report.level(),
                text: report.summary(),
            });
        }
        Err(e) => {
            crate::aelog!("[XTC] declined: {}", e);
            lab_outcomes.push(lab::LabOutcome {
                feature: "XTC",
                level: "warn",
                text: format!("declined — {}", e),
            });
        }
    }
}

use crate::audio::converter::dsp::dither::apply_dithering_and_noise_shaping;
use crate::audio::converter::dsp::true_peak::ceiling_gain_after;
use crate::audio::converter::dsp::polyphase::polyphase_decompose;
use crate::audio::converter::dsp::filter::find_precomputed_filter;
use crate::audio::converter::dsp::lab;
use crate::audio::converter::dsp::eq;
use crate::audio::converter::utils::verify::verify_flac;

/// Shared strided view over the final output buffer for the polyphase pass.
/// Phase `p` owns every L-th slot starting at offset `p`, so the write sets
/// of concurrently running phases are disjoint by construction — that
/// disjointness is the entire safety argument for the unsynchronized writes.
/// (Also used by the segmented giant-file pass in pipeline/segmented.rs.)
pub(crate) struct StridedOut {
    pub(crate) ptr: *mut f64,
    pub(crate) len: usize,
}
unsafe impl Send for StridedOut {}
unsafe impl Sync for StridedOut {}
impl StridedOut {
    /// SAFETY: callers must guarantee that no two threads ever write the
    /// same `idx` (the polyphase pass writes idx = frame*L + phase, and
    /// each thread has a fixed `phase`), and that `idx < self.len`.
    #[inline]
    pub(crate) unsafe fn write(&self, idx: usize, v: f64) {
        debug_assert!(idx < self.len);
        *self.ptr.add(idx) = v;
    }
}

/// Run all L polyphase sub-filters over the full input plus a zero flush,
/// one convolver per phase, writing every produced sample — already gain-
/// scaled — STRAIGHT into its strided slot of the final output-rate buffer:
/// `out[frame*L + phase] = y_phase[frame] * scale`.
///
/// The old design collected per-phase streams and interleaved them
/// afterwards, which materialized a SECOND full-length copy of the track
/// (multi-GB at 705.6/768 kHz) just to reorder samples. Writing strided
/// directly removes that copy and the extra interleave pass entirely.
/// `out_l`/`out_r` must be exactly (total_input_samples +
/// flush_input_samples) × L samples long.
///
/// CPU mode runs the phases **in parallel** via rayon: the sub-filters are
/// completely independent (own convolver state, disjoint strided slots),
/// so wall-clock drops from L×T toward T. GPU mode stays sequential — all
/// phases share one wgpu device/queue and `device.poll(Wait)` serializes
/// submissions anyway, while VRAM would have to hold L filter spectra.
/// `filter_path` is the `.npy` the sub-filters were decomposed from. It is
/// used only as a cache identity for the GPU filter spectra — pass `None`
/// when the coefficients have no stable provenance (tests, synthesized
/// filters) and every phase will simply be transformed from scratch.
pub(crate) fn run_polyphase_pass(
    phases: &[Vec<f64>],
    audio_l: &[f64],
    audio_r: &[f64],
    total_input_samples: usize,
    flush_input_samples: usize,
    scale: f64,
    out_l: &mut [f64],
    out_r: &mut [f64],
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    filter_path: Option<&str>,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
) -> Result<(), String> {
    run_polyphase_pass_from(
        phases,
        audio_l,
        audio_r,
        total_input_samples,
        flush_input_samples,
        scale,
        out_l,
        out_r,
        use_gpu,
        precision,
        status_label,
        filter_path.map(|path| FilterSource { path, ratio: phases.len() }),
        file_state,
        pct_base,
        pct_span,
    )
}

/// Where a pass's sub-filters came from: the `.npy` and the ratio it was
/// decomposed at. Sub-filter `p` of the pass is polyphase branch `p` of that
/// decomposition — for the multistage stage 0, one phase that is branch 0 of
/// the blob at its ratio, so it shares Single's phase-0 spectrum in the GPU
/// cache.
#[derive(Clone, Copy)]
pub(crate) struct FilterSource<'a> {
    pub(crate) path: &'a str,
    pub(crate) ratio: usize,
}

/// `run_polyphase_pass` with the sub-filters' provenance spelled out. The
/// installed alternate pass is offered only a whole decomposition (one phase
/// per ratio step), the one thing its contract can describe.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_polyphase_pass_from(
    phases: &[Vec<f64>],
    audio_l: &[f64],
    audio_r: &[f64],
    total_input_samples: usize,
    flush_input_samples: usize,
    scale: f64,
    out_l: &mut [f64],
    out_r: &mut [f64],
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    source: Option<FilterSource>,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
) -> Result<(), String> {
    use std::sync::atomic::AtomicU64;

    let l = phases.len();
    let filter_path = source.filter(|s| s.ratio == l).map(|s| s.path);
    let chunk: usize = 32768;
    let per_phase_len = total_input_samples + flush_input_samples;
    assert_eq!(
        out_l.len(),
        per_phase_len * l,
        "polyphase output buffer must be (input+flush)×L samples"
    );
    assert_eq!(out_r.len(), out_l.len());
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

    // ── an installed alternate pass gets first refusal ───────────────────
    //
    // Only when the coefficients came out of a file, because that file is
    // the whole of what the other side is given to work from — a pass over
    // synthesized coefficients (tests) has nothing to hand it and goes
    // straight down the built-in route. See dsp::polyphase_engine.
    if let (Some(engine), Some(path)) = (
        crate::audio::converter::dsp::polyphase_engine::installed(),
        filter_path,
    ) {
        let sub_taps = phases.iter().map(|p| p.len()).max().unwrap_or(0);
        let ola_latency = if use_gpu {
            crate::audio::gpu::GpuDspProcessor::output_latency_for(sub_taps)
        } else {
            crate::audio::dsp_core::CpuDspProcessor::output_latency_for(sub_taps)
        };
        let report = |fraction: f64| {
            let pct = pct_base + (fraction.clamp(0.0, 1.0) * pct_span as f64) as u32;
            file_state
                .gpu_pct
                .store(pct.min(pct_base + pct_span), Ordering::Relaxed);
        };
        let request = crate::audio::converter::dsp::polyphase_engine::PolyphaseRequest {
            filter_path: path,
            l,
            sub_taps,
            audio_l,
            audio_r,
            total_input_samples,
            flush_input_samples,
            ola_latency,
            scale,
            progress: &report,
        };
        set_status(&format!("{} (external)...", status_label));
        if engine.run(&request, out_l, out_r)? {
            file_state
                .gpu_pct
                .store(pct_base + pct_span, Ordering::Relaxed);
            return Ok(());
        }
        crate::aelog!("[CONV]   external polyphase pass declined — running the built-in one");
    }

    // The &mut borrows above guarantee exclusive access for the whole pass;
    // the raw-pointer views only exist so rayon phases can share them.
    let shared_l = StridedOut { ptr: out_l.as_mut_ptr(), len: out_l.len() };
    let shared_r = StridedOut { ptr: out_r.as_mut_ptr(), len: out_r.len() };

    // The `[firperf]` line: the pass's time and the partitions its phases
    // summed of all.
    let t_pass = std::time::Instant::now();
    let parts_mac = AtomicU64::new(0);
    let parts_full = AtomicU64::new(0);

    let run_phase = |phase: usize| -> Result<(), String> {
        // L is part of the identity: the same .npy decomposed at a
        // different ratio yields entirely different sub-filters.
        let id = source.map(|s| {
            crate::audio::gpu::filter_cache::FilterId::polyphase(s.path, phase, s.ratio)
        });
        let mut dsp: Box<dyn DspProcessor + Send> = crate::audio::gpu::build_convolver(
            &phases[phase],
            precision,
            id.as_ref(),
            use_gpu,
        )?;

        let mut in_l_buf = vec![0.0f64; chunk];
        let mut in_r_buf = vec![0.0f64; chunk];
        let mut out_l_buf = vec![0.0f64; chunk];
        let mut out_r_buf = vec![0.0f64; chunk];

        // Scatter one processed chunk into the strided output slots.
        let scatter = |frame0: usize, ol: &[f64], or_: &[f64]| {
            for i in 0..ol.len() {
                let idx = (frame0 + i) * l + phase;
                // SAFETY: this thread is the only writer of indices
                // ≡ phase (mod L); frame0+i < per_phase_len so idx < len.
                unsafe {
                    shared_l.write(idx, ol[i] * scale);
                    shared_r.write(idx, or_[i] * scale);
                }
            }
        };

        // Pass 1: real input at SOURCE rate
        let mut pos = 0;
        while pos < total_input_samples {
            if file_state.stop_requested() {
                return Err("Cancelled".to_string());
            }
            let end = (pos + chunk).min(total_input_samples);
            let actual = end - pos;
            in_l_buf[..actual].copy_from_slice(&audio_l[pos..end]);
            in_r_buf[..actual].copy_from_slice(&audio_r[pos..end]);
            dsp.process_audio(
                &in_l_buf[..actual],
                &in_r_buf[..actual],
                &mut out_l_buf[..actual],
                &mut out_r_buf[..actual],
                actual,
            );
            scatter(pos, &out_l_buf[..actual], &out_r_buf[..actual]);
            pos = end;
            bump_progress(chunks_done.fetch_add(1, Ordering::Relaxed) + 1);
        }

        // Pass 2: zero flush to drain the convolver's latency + filter delay
        let zero = vec![0.0f64; chunk];
        let mut fpos = 0;
        while fpos < flush_input_samples {
            if file_state.stop_requested() {
                return Err("Cancelled".to_string());
            }
            let end = (fpos + chunk).min(flush_input_samples);
            let actual = end - fpos;
            dsp.process_audio(
                &zero[..actual],
                &zero[..actual],
                &mut out_l_buf[..actual],
                &mut out_r_buf[..actual],
                actual,
            );
            scatter(
                total_input_samples + fpos,
                &out_l_buf[..actual],
                &out_r_buf[..actual],
            );
            fpos = end;
            bump_progress(chunks_done.fetch_add(1, Ordering::Relaxed) + 1);
        }

        let (m, f) = dsp.partition_counts();
        parts_mac.fetch_add(m, Ordering::Relaxed);
        parts_full.fetch_add(f, Ordering::Relaxed);
        Ok(())
        // DSP processor dropped here → frees GPU/CPU memory before the next phase
    };

    if use_gpu {
        for phase in 0..l {
            set_status(&format!("{} {}/{} (GPU)...", status_label, phase + 1, l));
            run_phase(phase)?;
        }
    } else {
        use rayon::prelude::*;
        set_status(&format!(
            "{} — {} phases in parallel (CPU)...",
            status_label, l
        ));
        (0..l)
            .into_par_iter()
            .map(run_phase)
            .collect::<Result<Vec<()>, String>>()?;
    }
    crate::aelog!(
        "{}",
        crate::player::firperf::conv_line(
            "pass",
            l,
            t_pass.elapsed().as_secs_f64(),
            (parts_mac.load(Ordering::Relaxed), parts_full.load(Ordering::Relaxed)),
            per_phase_len,
        )
    );
    Ok(())
}

/// h_k[m] = h[m*L + k] for k in 0..L
/// Phase 1 (CPU-only): decode + headroom + apodize.
/// Phase 2+3 (GPU + encode): runs on the GPU thread with pre-prepared audio.
/// Hand the queue row the chain for this file.
///
/// One entry per stage that is switched ON, in pipeline order, each saying
/// whether it ran or was reached and declined. A stage that is on but has no
/// entry yet simply has not been reached — the row draws that as waiting, so
/// it is left out rather than guessed at.
///
/// AA, PFR and HP are not lab features and keep their verdicts elsewhere, so
/// the caller passes them: only the caller knows which route the file took.
/// Called twice per file — once the source passes are done, and again at the
/// end — so the row fills in while the conversion runs.
/// The output half of the ISP stage: local true-peak limiting on the render,
/// after the filter and the phase engine, before the global ceiling measures
/// what is left. See `lab::isp::limit_output` for why the source-side pass
/// alone cannot buy the level back.
///
/// `album_gain` is the album level this render still has to take (1.0 for a
/// file on its own). The limiter holds the render at the ceiling divided by
/// it, which is the same arithmetic as taking the gain first and holding at
/// the ceiling — the dips are ratios to the target — without a pass over the
/// buffer: the dither pass takes the gain.
#[allow(clippy::too_many_arguments)]
fn run_output_limiter(
    on: bool,
    l: &mut [f64],
    r: &mut [f64],
    target_dbtp: f64,
    album_gain: f64,
    out_rate: u32,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    if !on {
        return;
    }
    let target_lin =
        crate::audio::converter::dsp::true_peak::target_lin_for(target_dbtp) / album_gain;
    record_output_limiter(
        lab::isp::limit_output(l, r, target_lin, out_rate),
        target_dbtp,
        lab_tags,
        lab_chain,
        lab_outcomes,
    );
}

/// The log line, tag, chain token and rack row for one run of the output
/// limiter, whichever route ran it.
fn record_output_limiter(
    result: Option<lab::isp::OutputLimitReport>,
    target_dbtp: f64,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    match result {
        None => {
            crate::aelog!(
                "[ISP] output: nothing above {:.1} dBTP - the ceiling costs this file nothing",
                target_dbtp
            );
        }
        Some(rep) => {
            // The whole-track part only when there is one: every other
            // file's tag stays as it was.
            let whole = if rep.pre_gain_db < 0.0 { format!(";whole={:+.2}dB", rep.pre_gain_db) } else { String::new() };
            lab_tags.push((
                "AURA_ISP_OUT".to_string(),
                format!(
                    "clusters={};gr_max={:+.2}dB;gr_ms={:.1};gr_pct={:.2};fallback={}{}",
                    rep.clusters, rep.max_gr_db, rep.gr_ms,
                    100.0 * rep.gr_fraction, rep.fell_back.unwrap_or("none"), whole
                ),
            ));
            match rep.fell_back {
                Some(why) => {
                    crate::aelog!(
                        "[ISP] output: {} over-cluster(s), but {} - leaving the global ceiling to it",
                        rep.clusters, why
                    );
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "ISP",
                        level: "warn",
                        text: format!(
                            "output overs are not a local case ({}): {} clusters, {:.1}% of the file, deepest {:+.2} dB - global ceiling applied instead",
                            why, rep.clusters, 100.0 * rep.gr_fraction, rep.max_gr_db
                        ),
                    });
                }
                None if rep.pre_gain_db < 0.0 => {
                    crate::aelog!(
                        "[ISP] output: {} over-cluster(s), deepest {:+.2} dB: held locally by {:+.2} dB and the whole track {:+.2} dB, {:.1} ms of gain reduction ({:.2}% of the file)",
                        rep.clusters, rep.max_gr_db, rep.max_gr_db - rep.pre_gain_db, rep.pre_gain_db, rep.gr_ms, 100.0 * rep.gr_fraction
                    );
                    if !lab_chain.contains(&"ISP") {
                        lab_chain.push("ISP");
                    }
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "ISP",
                        level: "ok",
                        text: format!(
                            "output: {} overs, deepest {:+.2} dB: held locally by {:+.2} dB, the whole track {:+.2} dB for the rest; {:.1} ms of gain reduction ({:.2}% of the file)",
                            rep.clusters, rep.max_gr_db, rep.max_gr_db - rep.pre_gain_db, rep.pre_gain_db, rep.gr_ms, 100.0 * rep.gr_fraction
                        ),
                    });
                }
                None => {
                    crate::aelog!(
                        "[ISP] output: {} over-cluster(s) trimmed locally, deepest {:+.2} dB, {:.1} ms of gain reduction ({:.2}% of the file)",
                        rep.clusters, rep.max_gr_db, rep.gr_ms, 100.0 * rep.gr_fraction
                    );
                    if !lab_chain.contains(&"ISP") {
                        lab_chain.push("ISP");
                    }
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "ISP",
                        level: "ok",
                        text: format!(
                            "output: {} overs trimmed locally, deepest {:+.2} dB, {:.1} ms of gain reduction ({:.2}% of the file); whatever the global ceiling still takes is on the status line",
                            rep.clusters, rep.max_gr_db, rep.gr_ms, 100.0 * rep.gr_fraction
                        ),
                    });
                }
            }
        }
    }
}

/// The output stages HP off runs, for measuring its level on the
/// linear-phase half of a Hybrid-Phase render (`segmented::hp_off_level`).
fn hp_off_stages<'a>(
    settings: &ConvertSettings,
    xtc: Option<&'a lab::xtc::XtcPlan>,
    target_dbtp: f64,
    album_gain: f64,
) -> crate::audio::converter::pipeline::segmented::GiantOutputStages<'a> {
    crate::audio::converter::pipeline::segmented::GiantOutputStages {
        xtc,
        limiter_target: settings.lab.isp.then(|| {
            crate::audio::converter::dsp::true_peak::target_lin_for(target_dbtp) / album_gain
        }),
        limiter_always: false,
        subsonic_hz: settings.subsonic_hz,
    }
}

/// HP off's level for a Hybrid-Phase render in RAM, from its linear-phase
/// half before the blend.
fn hp_off_level_in_ram(
    settings: &ConvertSettings,
    l: &[f64],
    r: &[f64],
    out_rate: u32,
    target_dbtp: f64,
    album_gain: f64,
    file_state: &Arc<FileConvState>,
) -> Result<crate::audio::converter::pipeline::segmented::HpOffLevel, String> {
    use crate::audio::converter::pipeline::segmented as seg;
    // The plan run_xtc would build; a geometry it declines is no XTC here
    // either, and its refusal is recorded when it runs.
    let xtc = if settings.lab.xtc && settings.lab.xtc_geometry.is_usable() {
        lab::xtc::plan(out_rate, &settings.lab.xtc_geometry).ok()
    } else {
        None
    };
    let chain = seg::HpOffChain {
        stages: hp_off_stages(settings, xtc.as_ref(), target_dbtp, album_gain),
        target_dbtp,
        album_gain,
    };
    seg::hp_off_level_in_ram(&chain, l, r, out_rate, &file_state.cancelled)
}

/// The output limiter's slot for a Hybrid-Phase render in RAM: every over
/// held at the ceiling of the level HP off takes (`hp`), refusing nothing —
/// the level was decided on the linear-phase half.
#[allow(clippy::too_many_arguments)]
fn run_hp_hold(
    isp: bool,
    hp: &crate::audio::converter::pipeline::segmented::HpOffLevel,
    l: &mut [f64],
    r: &mut [f64],
    target_dbtp: f64,
    album_gain: f64,
    out_rate: u32,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    let target_lin =
        crate::audio::converter::dsp::true_peak::target_lin_for(target_dbtp) / (album_gain * hp.gain());
    let hold = lab::isp::limit_output_always(l, r, target_lin, out_rate);
    record_hp_level(isp, hp, hold, target_dbtp, lab_tags, lab_chain, lab_outcomes);
}

/// The tags, log lines and rows of a Hybrid-Phase render's level: the ISP
/// limiter's as HP off would have them (its decision is HP off's), and
/// `AURA_HP_LEVEL` — the level taken from HP off and what the hold did.
fn record_hp_level(
    isp: bool,
    hp: &crate::audio::converter::pipeline::segmented::HpOffLevel,
    hold: Option<lab::isp::OutputLimitReport>,
    target_dbtp: f64,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) {
    if isp {
        let lin = hp.limiter.as_ref().map(|r| lab::isp::OutputLimitReport {
            clusters: r.clusters,
            max_gr_db: r.max_gr_db,
            gr_ms: r.gr_ms,
            gr_fraction: r.gr_fraction,
            fell_back: r.fell_back,
            pre_gain_db: r.pre_gain_db,
        });
        record_output_limiter(lin, target_dbtp, lab_tags, lab_chain, lab_outcomes);
    }
    let (clusters, max_db, ms) = hold.as_ref().map_or((0, 0.0, 0.0), |h| (h.clusters, h.max_gr_db, h.gr_ms));
    crate::aelog!(
        "[HP] level of HP off ({:+.2} dB); its own overs held at the ceiling: {} cluster(s), deepest {:+.2} dB, {:.1} ms",
        20.0 * hp.gain().log10(), clusters, max_db, ms
    );
    lab_tags.push((
        "AURA_HP_LEVEL".to_string(),
        format!("ref={:+.2}dB;hold_clusters={};hold_max={:+.2}dB;hold_ms={:.1}", 20.0 * hp.gain().log10(), clusters, max_db, ms),
    ));
}

/// The ceiling's gain on a finished render in RAM (`ceiling_gain_after`), and
/// what of it the album level reports as its residue. A Hybrid-Phase render
/// (`hp`) takes HP off's level first, so its gain is that level times what
/// the ceiling still takes on top, and its residue HP off's ceiling times it.
fn hp_ceiling(
    hp: &Option<crate::audio::converter::pipeline::segmented::HpOffLevel>,
    l: &[f64],
    r: &[f64],
    target_dbtp: f64,
    album_gain: f64,
) -> (f64, f64) {
    match hp {
        None => {
            let c = ceiling_gain_after(l, r, target_dbtp, album_gain);
            (c, c)
        }
        Some(h) => {
            let c = ceiling_gain_after(l, r, target_dbtp, album_gain * h.gain());
            (h.gain() * c, h.ceiling * c)
        }
    }
}

/// The album level's tag, log line and rack row for one file, when album
/// level is on for the batch; returns the gain its output stages take — 1.0,
/// nothing at all, for a file on its own and for an album none of whose
/// tracks needs a cut. See `converter::album`.
fn note_album_level(
    row: Option<&crate::audio::converter::album::AlbumRow>,
    lab_tags: &mut Vec<(String, String)>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) -> f64 {
    use crate::audio::converter::album::AlbumRow;
    match row {
        None => 1.0,
        Some(AlbumRow::Alone(why)) => {
            crate::aelog!("[ALBUM] {}", why);
            lab_outcomes.push(lab::LabOutcome { feature: "ALB", level: "none", text: why.clone() });
            1.0
        }
        Some(AlbumRow::Level(a)) => {
            crate::aelog!("[ALBUM] {}", a.summary());
            lab_tags.push(("AURA_ALBUM_GAIN".to_string(), a.tag_value()));
            lab_outcomes.push(lab::LabOutcome {
                feature: "ALB",
                level: if a.gain < 1.0 { "ok" } else { "none" },
                text: a.summary(),
            });
            a.applied()
        }
    }
}

/// What the ceiling still took from a track after its album level. The
/// limiter holds what the estimate missed locally, so this is normally
/// nothing or a rounding of the 4× view; more than that is the estimate
/// falling short, and the track then sits that much under its album's
/// balance — said in the log and on the row.
fn note_album_residue(album_gain: f64, peak_gain: f64, lab_outcomes: &mut Vec<lab::LabOutcome>) {
    if album_gain == 1.0 || !(peak_gain < 1.0) {
        return;
    }
    let db = 20.0 * peak_gain.log10();
    crate::aelog!(
        "[ALBUM] the ceiling took another {:.2} dB after the album level: the estimate fell short by that much",
        db
    );
    // Under 0.05 dB is the estimate's rounding, not a change anyone could
    // hear in the balance: the log keeps it, the row does not.
    if db > -0.05 {
        return;
    }
    if let Some(o) = lab_outcomes.iter_mut().rev().find(|o| o.feature == "ALB") {
        o.text.push_str(&format!(
            "; the ceiling took another {:.2} dB after it, so this track sits that much under the album's balance",
            db
        ));
        if db < -0.1 {
            o.level = "warn";
        }
    }
}

/// The listener's EQ for this render, when it changes the sound: its tag and
/// chain token go in now, before the file is named and tagged. The tail runs
/// it on the samples with the ceiling's gain in (`eq::EqTail`), just before
/// the dither or the modulator, as the player's output thread does.
fn open_eq(
    state: Option<&eq::EqState>,
    out_rate: u32,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
) -> Option<eq::EqTail> {
    // A render takes the speakers only with its own EQ switch, not the
    // player's "keep with EQ off".
    let state = &eq::EqState { speakers_always: false, ..state?.clone() };
    let tail = eq::EqTail::new(Some(state), out_rate)?;
    let d = tail.design();
    let mut tag = format!("bands={};gain={};level={:.2}dB;out={:.2}dB", state.active_bands().len(), d.mode.name(), d.level_db, d.output_db);
    for t in speaker_texts(d) {
        tag.push_str(&format!(";{t}"));
    }
    lab_tags.push(("AURA_EQ".to_string(), tag));
    lab_chain.push("EQ");
    Some(tail)
}

/// The EQ's balance and delay for its tag and log line, those off centre:
/// "balance=R1.50dB" (L/R, or M/S for mid/side), "delay=L2.04ms" (the late
/// channel).
fn speaker_texts(d: &eq::EqDesign) -> Vec<String> {
    let mut out = Vec::new();
    if d.balance_db != 0.0 {
        let side = match (d.balance_mode, d.balance_db > 0.0) {
            (eq::BalanceMode::LeftRight, true) => 'R',
            (eq::BalanceMode::LeftRight, false) => 'L',
            (eq::BalanceMode::MidSide, true) => 'S',
            (eq::BalanceMode::MidSide, false) => 'M',
        };
        out.push(format!("balance={}{:.2}dB", side, d.balance_db.abs()));
    }
    if d.delay_ms != 0.0 {
        out.push(format!("delay={}{:.2}ms", if d.delay_ms > 0.0 { 'R' } else { 'L' }, d.delay_ms.abs()));
    }
    out
}

/// The end of a render in RAM: the listener's EQ (`open_eq`), the ceiling
/// measured on what it gave (`hp_ceiling`) and the album residue. The
/// cascade is linear, so it runs first with no gain and the gain the ceiling
/// takes goes in after it: the EQ's boosts and output gain are held under
/// the target like the rest of the chain. Returns the ceiling's gain and the
/// gain still left for the dither or the modulator (1.0 once the EQ ran).
#[allow(clippy::too_many_arguments)]
fn eq_then_ceiling(
    state: Option<&eq::EqState>,
    hp_off: &Option<crate::audio::converter::pipeline::segmented::HpOffLevel>,
    l: &mut [f64],
    r: &mut [f64],
    target_dbtp: f64,
    album_gain: f64,
    out_rate: u32,
    lab_tags: &mut Vec<(String, String)>,
    lab_chain: &mut Vec<&'static str>,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) -> (f64, f64) {
    let mut tail = open_eq(state, out_rate, lab_tags, lab_chain);
    if let Some(t) = tail.as_mut() {
        t.cascade(l, r);
    }
    let (peak_gain, residue) = hp_ceiling(hp_off, l, r, target_dbtp, album_gain);
    note_album_residue(album_gain, residue, lab_outcomes);
    match tail.as_mut() {
        Some(t) => {
            t.level(l, r, album_gain * peak_gain);
            record_eq(t, lab_outcomes);
            (peak_gain, 1.0)
        }
        None => (peak_gain, album_gain * peak_gain),
    }
}

/// The EQ's log line and rack row once the tail has run: the peak it gave
/// and what it clipped at full scale.
fn record_eq(tail: &eq::EqTail, lab_outcomes: &mut Vec<lab::LabOutcome>) {
    let d = tail.design();
    let peak_db = 20.0 * tail.peak.max(1e-12).log10();
    let mut text = format!(
        "gain {} {:+.1} dB, output {:+.1} dB, peak after it {:.2} dBFS",
        d.mode.name(), d.level_db, d.output_db, peak_db
    );
    for t in speaker_texts(d) {
        text.push_str(&format!(", {t}"));
    }
    if tail.clipped > 0 {
        text.push_str(&format!("; {} sample(s) clipped at full scale (lower the gain)", tail.clipped));
    }
    crate::aelog!("[EQ] {}", text);
    lab_outcomes.push(lab::LabOutcome {
        feature: "EQ",
        level: if tail.clipped > 0 { "warn" } else { "ok" },
        text,
    });
}

/// The subsonic filter's second pass: the same high-pass, designed for the
/// OUTPUT rate and applied after the output limiter, immediately before the
/// ceiling is measured.
///
/// The first pass runs on the source, in `pipeline::prepare`, and is what
/// actually takes a disc's rumble out. Everything after it is linear -
/// headroom is a scalar, the apodizer and the filter are convolutions, XTC
/// is a filter - with two exceptions that multiply the signal by a curve
/// that moves: the Hybrid-Phase blend and the limiter's gain dips. That is
/// amplitude modulation. Bass times a gain that dips for 1.5 ms thousands of
/// times over a track puts energy back below the corner, and on a limited
/// master it came back to about 40 dB under the bass where the filter
/// promises 100 dB down - measured at 2.7 Hz: -125 dBFS with the filter
/// alone, -57 once the limiter fired.
///
/// The first pass cannot reach that, because it has already run. This is the
/// last point at which the buffer is still the file, so this is where the
/// promise has to be kept. It runs before `true_peak_normalization_gain` on
/// purpose: the peak the normaliser reads has to be the peak that is written.
///
/// The segmented route for very long files runs the same pass, streamed
/// (`SubsonicGuardStream`), in the same place: since 1.3.4 it has the output
/// limiter too.
fn run_output_subsonic(
    corner_hz: u32,
    l: &mut [f64],
    r: &mut [f64],
    out_rate: u32,
    lab_outcomes: &mut Vec<lab::LabOutcome>,
) -> Result<(), String> {
    if corner_hz == 0 {
        return Ok(());
    }
    let rep = apply_subsonic_guard(
        l,
        r,
        out_rate,
        corner_hz,
        crate::audio::cancel_flag::get_atomic(),
    )?;
    record_subsonic_guard(lab_outcomes, corner_hz, &rep);
    Ok(())
}

fn record_subsonic_guard(
    lab_outcomes: &mut Vec<lab::LabOutcome>,
    corner_hz: u32,
    rep: &crate::audio::converter::apodize::SubsonicReport,
) {
    note_subsonic_outcome(
        lab_outcomes,
        corner_hz,
        format!(
            "output-rate pass after the limiter ({} taps) took out another {:.1} / {:.1} dBFS RMS",
            rep.taps, rep.removed_rms_db[0], rep.removed_rms_db[1]
        ),
    );
}

/// Add a sentence to the SUB row rather than a second SUB row: the rack and
/// the lab report both show one line per feature, and the last one written
/// would otherwise be the only one anybody sees.
fn note_subsonic_outcome(
    lab_outcomes: &mut Vec<lab::LabOutcome>,
    corner_hz: u32,
    note: String,
) {
    match lab_outcomes.iter_mut().rev().find(|o| o.feature == "SUB") {
        Some(o) => {
            o.text.push_str("; ");
            o.text.push_str(&note);
        }
        None => lab_outcomes.push(lab::LabOutcome {
            feature: "SUB",
            level: "ok",
            text: format!("linear-phase high-pass below {} Hz; {}", corner_hz, note),
        }),
    }
}

fn publish_lab_chain(
    file_state: &FileConvState,
    settings: &ConvertSettings,
    outcomes: &[lab::LabOutcome],
    aa: Option<(bool, String)>,
    pfr: Option<bool>,
    ms: Option<&[crate::audio::converter::dsp::ms_h1::H1Stage]>,
    hp: Option<(bool, String)>,
) {
    let mut e: Vec<(&str, u8, String)> = Vec::new();
    let from_outcome = |e: &mut Vec<(&str, u8, String)>, on: bool, feat: &str, tok: &'static str| {
        if !on {
            return;
        }
        if let Some(o) = outcomes.iter().rev().find(|o| o.feature == feat) {
            e.push((tok, lab::chain_state(o.level), o.text.clone()));
        }
    };

    from_outcome(&mut e, settings.lab.declip, "DECLIP", "DC");
    from_outcome(&mut e, settings.lab.isp, "ISP", "ISP");
    from_outcome(&mut e, settings.subsonic_hz != 0, "SUB", "SUB");
    from_outcome(&mut e, settings.lab.adaptive_headroom, "AHR", "AHR");
    if settings.adaptive_apodizer {
        if let Some((treated, note)) = aa {
            e.push((
                "AA",
                if treated { lab::CHAIN_RAN } else { lab::CHAIN_DECLINED },
                note,
            ));
        }
    }
    if settings.use_fir_resampling {
        if let Some(ran) = pfr {
            e.push(if ran {
                ("PFR", lab::CHAIN_RAN, pfr_why(settings.taps, ms))
            } else {
                ("PFR", lab::CHAIN_DECLINED,
                 "not on this route - the file went through the library resampler".to_string())
            });
        }
    }
    if settings.hybrid_phase {
        if let Some((ran, why)) = hp {
            e.push((
                "HP",
                if ran { lab::CHAIN_RAN } else { lab::CHAIN_DECLINED },
                why,
            ));
        }
    }
    from_outcome(&mut e, settings.lab.continuous_alpha, "ALPHA", "aHP");
    from_outcome(&mut e, settings.lab.tfs_phase, "TFS", "TFS");
    from_outcome(&mut e, settings.lab.xtc, "XTC", "XTC");
    // Album level is a batch switch, not a file setting: its row entry is
    // there exactly when the manager gave the file a verdict.
    let album_on = outcomes.iter().any(|o| o.feature == "ALB");
    from_outcome(&mut e, album_on, "ALB", "ALB");

    file_state.set_lab_chain(lab::chain_json(&e));
}

/// Hybrid-Phase either rendered both branches or TFS took its place. Both are
/// worth saying out loud in the row: the second is the one that looks like a
/// feature silently not working.
/// The converter's PFR row when the FIR ran as the resampler: one
/// convolution, or multistage through `ms`.
fn pfr_why(taps: usize, ms: Option<&[crate::audio::converter::dsp::ms_h1::H1Stage]>) -> String {
    match ms {
        Some(chain) => {
            let label = crate::audio::converter::dsp::filter::taps_label(taps).unwrap_or("?");
            let l = chain.iter().map(|s| s.ratio).product();
            crate::audio::converter::dsp::ms_text::pfr_text(label, l, chain)
        }
        None => "the FIR was the resampler: one convolution, no second stage".to_string(),
    }
}

fn hp_verdict(settings: &ConvertSettings, tfs_active: bool, ran: bool, hp_net: &str) -> Option<(bool, String)> {
    if !settings.hybrid_phase {
        return None;
    }
    Some(if ran {
        (true, format!("both phase branches rendered and blended at zero crossings; hits: {}", hp_net_text(hp_net)))
    } else if tfs_active {
        (false, "TFS Phase took its place for this file".to_string())
    } else {
        (false, "the post-FIR stage this needs did not run for this file".to_string())
    })
}

/// What a render's Hybrid-Phase hits come from, as its stamp keeps it
/// (`stamp::KEY_HP_NET`): the drum kit's lists, the kicks, or the detector
/// alone.
fn hp_net_stamp(net: Option<&crate::spatial::netkick::ForRender>) -> String {
    net.map_or_else(|| "v6".to_string(), |n| n.stamp())
}

/// The same for the rack row.
fn hp_net_text(stamp: &str) -> String {
    match stamp.strip_prefix("kit:") {
        Some(from) => format!("the drum kit ({from})"),
        None if stamp == "kick" => "the network's kicks".to_string(),
        None => "detector v6".to_string(),
    }
}

/// The Adaptive Apodizer verdict as the file state already holds it — manager
/// stores it there before the conversion starts, so it needs no threading.
fn aa_verdict(file_state: &FileConvState) -> Option<(bool, String)> {
    let st = file_state.aa_state.load(Ordering::Relaxed);
    if st == crate::audio::converter::state::AA_NONE {
        return None;
    }
    Some((
        st == crate::audio::converter::state::AA_TREATED,
        file_state.aa_note.lock().unwrap().clone(),
    ))
}

/// The integrated (polyphase) route's convolution: today's single stage, or
/// the multistage stage 0 plus the short chain.
enum IntegratedPass {
    Single(SinglePlan),
    Multi(crate::audio::converter::pipeline::ms_route::MsStage0),
}

/// Today's single-stage pass for one file: the decomposed blob, its gain,
/// the zeros that drain it and the trim — shared by the in-RAM and the
/// segmented routes.
pub(crate) struct SinglePlan {
    pub(crate) phases: Vec<Vec<f64>>,
    pub(crate) sub_taps: usize,
    pub(crate) scale: f64,
    pub(crate) flush_input_samples: usize,
    pub(crate) group_delay_samples: usize,
}

/// Load and decompose the blob at `lin_path_str` for the single-stage pass ×`l`.
pub(crate) fn single_pass_plan(
    lin_path_str: &str,
    l: usize,
    settings: &ConvertSettings,
    tfs_active: bool,
    src_rate: u32,
    out_rate: u32,
) -> Result<SinglePlan, String> {
    let full_coeffs: Vec<f64> = crate::audio::dsp_core::load_npy_f64(lin_path_str)?;

    // Decompose into L polyphase components
    let phases = polyphase_decompose(&full_coeffs, l);
    let sub_taps = phases[0].len();
    crate::aelog!(
        "[CONV] Polyphase FIR resampling: {}Hz → {}Hz (×{})",
        src_rate, out_rate, l
    );
    crate::aelog!(
        "[CONV]   Filter: {} taps → {} sub-filters × {} taps each",
        full_coeffs.len(),
        l,
        sub_taps
    );
    crate::aelog!(
        "[CONV]   Processing at INPUT rate ({}Hz) — polyphase optimized",
        src_rate
    );


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

    // Detect linear phase by coefficient symmetry NOW, so the full
    // coefficient blob (up to ~240 MB at 30M taps) can be freed before
    // the convolution pass instead of sitting in RAM until the trim.
    // TFS filters are asymmetric and look ahead by their own window
    // (`tfs::look_ahead`, 30 ms): that is their trim.
    let full_len = full_coeffs.len();
    let tfs_ahead = tfs_active.then(|| crate::audio::converter::dsp::lab::tfs::look_ahead(full_len, out_rate));
    let is_linear_phase = if tfs_ahead.is_some() {
        false
    } else {
        let c = crate::audio::converter::pipeline::ms_route::SYMMETRY_TAPS.min(full_len / 2);
        crate::audio::converter::pipeline::ms_route::linear_by_symmetry(
            &full_coeffs[..c],
            &full_coeffs[full_len - c..],
            full_len,
        )
    };
    drop(full_coeffs);

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
    // Input-rate samples the filter looks ahead: a linear one by its
    // group delay, TFS by its window — 30 ms, not the 42 s of zeros half
    // a 30M filter at ×8 would push through.
    let sub_delay = match tfs_ahead {
        Some(k) => k.div_ceil(l),
        None => (sub_taps - 1) / 2,
    };
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
        flush_input_samples as f64 / src_rate as f64,
        src_rate
    );

    // ── Group delay trim plan ──
    // We trim the algorithmic latency (ola_latency × L output samples)
    // plus the EXACT full-filter group delay (N-1)/2 in output samples —
    // not sub_delay×L, whose per-phase integer division loses (L-1)/2
    // samples. Computed BEFORE the pass so the segmented giant route
    // reuses identical arithmetic. (is_linear_phase was detected from
    // coefficient symmetry above, before full_coeffs was freed.)
    let group_delay_samples = if let Some(k) = tfs_ahead {
        let total_delay = ola_latency * l + k;
        crate::aelog!(
            "[TFS] Trimming {} samples: OLA latency and the filter's {:.0} ms look-ahead",
            total_delay,
            k as f64 * 1000.0 / out_rate as f64
        );
        total_delay
    } else if is_linear_phase {
        let total_delay = ola_latency * l + (full_len - 1) / 2;
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
    Ok(SinglePlan {
        phases,
        sub_taps,
        scale,
        flush_input_samples,
        group_delay_samples,
    })
}

/// The DSF tail of both in-RAM routes, in place of dither and FLAC: the
/// render (`gain` applied) through the modulator into the file beside the
/// source, read back and checked. Returns the output path.
#[allow(clippy::too_many_arguments)]
fn finish_dsd_in_ram(
    dsd: crate::audio::converter::dsd_out::DsdExport,
    l: &[f64],
    r: &[f64],
    out_rate: u32,
    gain: f64,
    audio: &AudioFile,
    src_path: &Path,
    output_name: &str,
    flac_tags: &[(String, String)],
    pictures: &[crate::audio::converter::source_tags::Picture],
    file_state: &Arc<FileConvState>,
    album_status: &str,
    ceiling_note: String,
) -> Result<String, String> {
    use crate::audio::converter::dsd_out;
    set_status(&format!("Encoding DSF (DSD{}, {})...", dsd.multiple, dsd.kind.label()));
    file_state.gpu_pct.store(800, Ordering::Relaxed);
    let output_path = src_path.parent().unwrap_or(Path::new(".")).join(output_name);
    let id3 = dsd_out::tag_for(&audio.artist, &audio.title, flac_tags, pictures);
    let n = l.len().max(1);
    let done = dsd_out::write_in_ram(l, r, out_rate, gain, dsd, &output_path, &id3, &file_state.cancelled, |k| {
        let pct = 800 + ((k as f64 / n as f64) * 140.0) as u32;
        file_state.gpu_pct.store(pct.min(940), Ordering::Relaxed);
    })
    .inspect_err(|e| {
        set_status(e);
        file_state.gpu_pct.store(1000, Ordering::Relaxed);
    })?;
    let size_mb = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
    set_status(&format!(
        "\u{2713} Done: {} ({:.1} MB{}{}{})",
        output_name,
        size_mb,
        album_status,
        ceiling_note,
        dsd_out::overload_note(done.overloads)
    ));
    file_state.gpu_pct.store(1000, Ordering::Relaxed);
    Ok(output_path.to_string_lossy().to_string())
}

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
    // Lab metadata accumulated during prepare phase.
    let mut lab_tags: Vec<(String, String)> = prep.lab_tags;
    let mut lab_chain: Vec<&'static str> = prep.lab_chain;
    let true_peak_target_dbtp = prep.true_peak_target_dbtp;
    // Outcomes collected in prepare phase (DECLIP, ISP, CODEC, AHR); this
    // function appends TFS/ALPHA/XTC and then pushes to the report registry.
    let mut lab_outcomes: Vec<lab::LabOutcome> = prep.lab_outcomes;
    // The album level this file takes (the manager set it once the album's
    // tracks were all prepared). It goes in front of the output limiter on
    // every route below; 1.0 is nothing at all.
    let album_row = prep.album;
    let seg_plan = prep.seg_plan;
    // The drum kit's lists Hybrid-Phase takes, as the player does (made by
    // the manager after prepare); None: the detector alone.
    let hp_net = prep.hp_net;
    let hp_net_stamp = hp_net_stamp(hp_net.as_ref());
    let album_gain = note_album_level(album_row.as_ref(), &mut lab_tags, &mut lab_outcomes);
    let album_status = crate::audio::converter::album::status_note(album_row.as_ref());
    // The source passes are done by now; the rest of the chain has not been
    // reached. Publishing here is what makes the row fill in during the
    // conversion rather than all at once when it ends.
    publish_lab_chain(
        &file_state, settings, &lab_outcomes, aa_verdict(&file_state), None, None, None,
    );
    // Guard: continuous_alpha is meaningful only with hybrid_phase.
    if settings.lab.continuous_alpha && !settings.hybrid_phase {
        crate::aelog!("[ALPHA] continuous_alpha ignored: hybrid_phase is off");
        lab_outcomes.push(lab::LabOutcome {
            feature: "ALPHA",
            level: "none",
            text: "continuous alpha ignored: hybrid phase is off".to_string(),
        });
    }
    // Stub AudioFile for build_output_name (needs artist, title, sample_rate
    // and the source channel count only)
    let audio = AudioFile {
        samples_l: vec![],
        samples_r: vec![],
        sample_rate: prep.sample_rate,
        channels: prep.source_channels,
        artist: prep.artist,
        title: prep.title,
        lossy: None,
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
        // standard path uses; keyed on this conversion's source and output
        // rates, so a hi-res source takes the blob of its own factor and the
        // wall lands just under its own Nyquist — filter::design_rate).
        set_status("Loading FIR filter for polyphase decomposition...");
        // The resolved path is kept, not just consumed: it is the identity
        // under which each sub-filter's spectrum is cached across the batch
        // (see gpu::filter_cache).
        // TFS phase mode: resolve or derive a TFS filter instead of the
        // standard linear-phase blob. Hybrid is disabled for this file
        // because TFS already blends the two phase responses.
        let tfs_requested = settings.lab.tfs_phase && settings.custom_filter_path.is_none();
        // TFS this machine can neither find nor derive (`tfs::unavailable`):
        // the file converts in linear phase and its row says why, rather
        // than fail. Hybrid-Phase stays off as TFS asked.
        let tfs_skip = if tfs_requested {
            crate::audio::converter::dsp::lab::tfs::unavailable(settings.taps, audio.sample_rate, out_rate)
        } else {
            None
        };
        let tfs_active = tfs_requested && tfs_skip.is_none();
        if let Some(why) = &tfs_skip {
            crate::aelog!("[TFS] skipped: {}", why);
            lab_outcomes.push(lab::LabOutcome {
                feature: "TFS",
                level: "warn",
                text: format!("{} — converted in linear phase", why),
            });
        }
        if tfs_requested && settings.hybrid_phase {
            crate::aelog!("[TFS] hybrid disabled for TFS mode");
        }
        if tfs_active {
            crate::aelog!("[TFS] TFS phase mode active — hybrid disabled for this file");
        }
        let lin_path_str: String = if let Some(ref path) = settings.custom_filter_path {
            crate::aelog!(
                "[CONV]   [WARN] Custom filter loaded — verify its fc matches {:.4} for this ratio",
                fc_norm
            );
            path.clone()
        } else if tfs_active {
            // TFS: derive or load cached TFS filter.
            use crate::audio::converter::dsp::lab::tfs;
            let cancel_flag = &file_state.cancelled;
            match tfs::resolve_or_derive(settings.taps, audio.sample_rate, out_rate, &cancel_flag) {
                Ok(path) => {
                    let ps = path.to_string_lossy().to_string();
                    crate::aelog!("[TFS] Using TFS filter: {}", ps);
                    lab_tags.push(("AURA_PHASE".to_string(), "tfs;xover=1500-4000Hz".to_string()));
                    lab_chain.push("TFS");
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "TFS",
                        level: "ok",
                        text: "TFS phase filter applied (derived/cached)".to_string(),
                    });
                    ps
                }
                Err(e) => {
                    crate::aelog!("[TFS] ❌ {}", e);
                    return Err(format!("[TFS] {}", e));
                }
            }
        } else if let Some(path) =
            find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "linear_phase")
        {
            crate::aelog!("[CONV]   Loading pre-computed linear-phase (matrix): {}", path);
            path
        } else {
            let msg = crate::audio::converter::dsp::filter::missing_filter_error(
                settings.taps,
                audio.sample_rate,
                out_rate,
                "linear_phase",
            );
            crate::aelog!("[CONV] ❌ {}", msg);
            return Err(msg);
        };
        let mode_str = if settings.use_gpu { "GPU" } else { "CPU" };
        // Multistage (`UpsamplerMode::Multi`): the blob's branch 0 at the
        // source rate plus the short chain, reading branch 0 alone; None keeps
        // today's single-stage pass, which loads and decomposes the blob.
        let pass = match crate::audio::converter::pipeline::ms_route::prepare(
            settings,
            tfs_active,
            &lin_path_str,
            audio.sample_rate,
            out_rate,
            l,
        )? {
            Some(ms) => IntegratedPass::Multi(ms),
            None => IntegratedPass::Single(single_pass_plan(
                &lin_path_str,
                l,
                settings,
                tfs_active,
                audio.sample_rate,
                out_rate,
            )?),
        };
        // The short chain a multistage pass runs: the render's PFR row, tag
        // and name say so.
        let ms_chain = match &pass {
            IntegratedPass::Multi(_) => crate::audio::converter::pipeline::ms_route::chain_for(
                settings,
                settings.taps,
                audio.sample_rate,
                l,
            ),
            IntegratedPass::Single(_) => None,
        };
        // Single-stage by the hidden switch where multistage would have run:
        // the render's tag and name say so.
        let single_fallback =
            crate::audio::converter::pipeline::ms_route::single_fallback(settings, settings.taps, audio.sample_rate, l);
        // The Hybrid-Phase minimum-phase half follows the linear one's route.
        let lin_multi = matches!(pass, IntegratedPass::Multi(_));

        // ═══ GIANT ROUTE: segmented, bounded-RAM pipeline ═══
        // A file whose in-RAM peak (full output-rate buffers, ×2 with
        // hybrid) would eat a large share of physical RAM, or more than is
        // free, is converted through temp-file segments instead — same
        // convolvers, same trim/blend math, constant working set. The
        // manager chose the route before admitting the file; see
        // pipeline/segmented.rs.
        let seg_plan = seg_plan.unwrap_or_else(|| {
            use crate::audio::converter::pipeline::segmented as seg;
            let in_ram = seg::in_ram_peak_mb(total_input_samples as u64, l as u64, settings.hybrid_phase);
            seg::plan_route(in_ram + in_ram / 5 + 256, l as u64)
        });
        if seg_plan.segmented {
            // Prepare min-phase parameters up front (the in-RAM path loads
            // them after the linear pass; the arithmetic is identical).
            // Multistage: the minimum-phase half's stage 0 (branch 0 alone).
            let ms_min = if settings.hybrid_phase && !tfs_requested && lin_multi {
                let min_path_str =
                    find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "minimum_phase").ok_or_else(
                        || {
                            crate::audio::converter::dsp::filter::missing_filter_error(
                                settings.taps,
                                audio.sample_rate,
                                out_rate,
                                "minimum_phase",
                            )
                        },
                    )?;
                let ms_min = crate::audio::converter::pipeline::ms_route::prepare_min(
                    settings,
                    &min_path_str,
                    audio.sample_rate,
                    out_rate,
                    l,
                )?;
                Some(ms_min.ok_or("Hybrid-Phase: the minimum-phase half is not multistage while the linear one is")?)
            } else {
                None
            };
            let hybrid_data: Option<(Vec<Vec<f64>>, f64, usize, usize, String)> = if settings.hybrid_phase
                && !tfs_requested
                && !lin_multi
            {
                let min_path_str =
                    find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "minimum_phase").ok_or_else(
                        || {
                            crate::audio::converter::dsp::filter::missing_filter_error(
                                settings.taps,
                                audio.sample_rate,
                                out_rate,
                                "minimum_phase",
                            )
                        },
                    )?;
                crate::aelog!(
                    "[CONV] Hybrid-Phase (FIR, giant): loading pre-computed {}",
                    min_path_str
                );
                let min_coeffs: Vec<f64> = crate::audio::dsp_core::load_npy_f64(&min_path_str)?;
                let min_phases = polyphase_decompose(&min_coeffs, l);
                let min_sub_taps = min_phases[0].len();
                let min_dc_gain: f64 = min_coeffs.iter().sum();
                let min_scale = l as f64 / min_dc_gain.abs().max(1e-10);
                let min_filter_delay_samples =
                    crate::audio::dsp_core::estimate_band_weighted_group_delay(
                        &min_coeffs,
                        out_rate as f64,
                        200.0,
                        6000.0,
                    );
                crate::aelog!(
                    "[CONV] Hybrid-Phase (FIR, giant): Min-phase band-weighted (200-6000 Hz) group delay = {} output samples (DC gain={:.6})",
                    min_filter_delay_samples, min_dc_gain
                );
                drop(min_coeffs);
                let min_ola_latency: usize = if settings.use_gpu {
                    crate::audio::gpu::GpuDspProcessor::output_latency_for(min_sub_taps)
                } else {
                    crate::audio::dsp_core::CpuDspProcessor::output_latency_for(min_sub_taps)
                };
                let min_flush_input = min_ola_latency + min_filter_delay_samples / l + 1;
                let min_trim = (min_ola_latency * l) + min_filter_delay_samples;
                Some((min_phases, min_scale, min_flush_input, min_trim, min_path_str))
            } else {
                None
            };

            lab_chain.push("PFR");
            if hybrid_data.is_some() || ms_min.is_some() {
                lab_chain.push("HP");
                // blend_giant_hybrid runs the continuous blend on this route
                // too; without its own entry the file said only "HP".
                if settings.lab.continuous_alpha {
                    lab_outcomes.push(lab::LabOutcome {
                        feature: "ALPHA",
                        level: "ok",
                        text: "continuous alpha blend applied (segmented path)".to_string(),
                    });
                    lab_chain.push("aHP");
                }
            }
            use crate::audio::converter::pipeline::segmented as seg;
            // The output stages, in the in-RAM route's order and with its
            // arithmetic, streamed: XTC, the ISP output limiter, the subsonic
            // guard. Until 1.3.4 this route ran none of them, so a long file
            // lost XTC outright and took the global ceiling for every over.
            let xtc_plan = if xtc_geometry_usable(settings.lab.xtc, &settings.lab.xtc_geometry, &mut lab_outcomes) {
                match lab::xtc::plan(out_rate, &settings.lab.xtc_geometry) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        record_xtc(Err(e), &mut lab_tags, &mut lab_chain, &mut lab_outcomes);
                        None
                    }
                }
            } else {
                None
            };
            let hp_off_chain = (hybrid_data.is_some() || ms_min.is_some()).then(|| seg::HpOffChain {
                stages: hp_off_stages(settings, xtc_plan.as_ref(), true_peak_target_dbtp, album_gain),
                target_dbtp: true_peak_target_dbtp,
                album_gain,
            });
            let output_dir = src_path.parent().unwrap_or(Path::new("."));
            let hybrid_params = hybrid_data.as_ref().map(|(p, s, f, t, path)| {
                seg::GiantHybridParams {
                    min_phases: p,
                    min_scale: *s,
                    min_flush_input: *f,
                    min_trim: *t,
                    min_filter_path: Some(path.as_str()),
                }
            });
            let mut render = match &pass {
                IntegratedPass::Single(sp) => seg::render_giant(
                    src_path,
                    audio_l,
                    audio_r,
                    total_input_samples,
                    audio.sample_rate,
                    out_rate,
                    n_out,
                    &sp.phases,
                    Some(lin_path_str.as_str()),
                    sp.scale,
                    sp.flush_input_samples,
                    sp.group_delay_samples,
                    hybrid_params,
                    seg_plan.seg_input_samples,
                    settings,
                    hp_net.as_ref(),
                    hp_off_chain.as_ref(),
                    &file_state,
                    output_dir,
                )?,
                // Multi: hybrid_params is None, the minimum-phase half is `ms_min`.
                IntegratedPass::Multi(ms) => seg::render_giant_multi(
                    src_path,
                    ms,
                    ms_min.as_ref(),
                    audio_l,
                    audio_r,
                    total_input_samples,
                    audio.sample_rate,
                    out_rate,
                    n_out,
                    seg_plan.seg_input_samples,
                    settings,
                    hp_net.as_ref(),
                    hp_off_chain.as_ref(),
                    &file_state,
                    output_dir,
                )?,
            };
            drop(pass);
            let hp_multi = ms_min.is_some();
            drop(ms_min);

            // A Hybrid-Phase render takes the level HP off would: measured on
            // the linear-phase half before the blend, held below by the
            // limiter at that level's ceiling.
            let hp_off = render.hp_off.take();
            let level_album = album_gain * hp_off.as_ref().map_or(1.0, |h| h.gain());
            let stages = seg::GiantOutputStages {
                xtc: xtc_plan.as_ref(),
                // Held at the ceiling the album level leaves, as on the
                // in-memory routes (`run_output_limiter`).
                limiter_target: (settings.lab.isp || hp_off.is_some()).then(|| {
                    crate::audio::converter::dsp::true_peak::target_lin_for(true_peak_target_dbtp)
                        / level_album
                }),
                limiter_always: hp_off.is_some(),
                subsonic_hz: settings.subsonic_hz,
            };
            let done = seg::run_giant_output_stages(&mut render, &stages, &file_state)?;
            if let (Some(plan), Some((before, after))) = (xtc_plan.as_ref(), done.xtc) {
                record_xtc(Ok(plan.report(before, after)), &mut lab_tags, &mut lab_chain, &mut lab_outcomes);
            }
            match &hp_off {
                Some(h) => record_hp_level(settings.lab.isp, h, done.limiter, true_peak_target_dbtp, &mut lab_tags, &mut lab_chain, &mut lab_outcomes),
                None if settings.lab.isp => record_output_limiter(
                    done.limiter,
                    true_peak_target_dbtp,
                    &mut lab_tags,
                    &mut lab_chain,
                    &mut lab_outcomes,
                ),
                None => {}
            }
            if let Some(rep) = &done.subsonic {
                record_subsonic_guard(&mut lab_outcomes, settings.subsonic_hz, rep);
            }

            // The listener's EQ, run by the streamed tail (`open_eq`).
            let mut eq_tail = open_eq(settings.lab.eq.as_ref(), out_rate, &mut lab_tags, &mut lab_chain);
            // Named from the finished chain, as the in-RAM route does.
            let output_name =
                build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref(), &lab_chain, single_fallback);
            let mut giant_flac_tags = build_lab_vorbis_tags(&lab_tags, &lab_chain, settings.dsd.is_some(), settings.rack_sig.as_deref(), settings.lab.hp_sens, &hp_net_stamp);
            push_upsampler_tag(&mut giant_flac_tags, ms_chain, single_fallback);
            let src_tags = source_tags_for(&audio, src_path);
            let giant_flac_tags = src_tags.with(giant_flac_tags);
            let dsd_tag = settings.dsd.map(|_| {
                crate::audio::converter::dsd_out::tag_for(&audio.artist, &audio.title, &giant_flac_tags, &src_tags.pictures)
            });
            let (giant_result, ceiling_gain) = seg::finish_giant(
                render,
                true_peak_target_dbtp,
                level_album,
                &album_status,
                &file_state,
                &output_name,
                &giant_flac_tags,
                &src_tags.pictures,
                settings.dsd.zip(dsd_tag.as_deref()),
                eq_tail.as_mut(),
            )?;
            if let Some(t) = &eq_tail {
                record_eq(t, &mut lab_outcomes);
            }
            note_album_residue(album_gain, ceiling_gain * hp_off.as_ref().map_or(1.0, |h| h.ceiling), &mut lab_outcomes);
            publish_lab_chain(
                &file_state, settings, &lab_outcomes, aa_verdict(&file_state),
                Some(true), ms_chain, hp_verdict(settings, tfs_requested, hybrid_data.is_some() || hp_multi, &hp_net_stamp),
            );
            lab::print_lab_report(&output_name, &lab_outcomes);
            return Ok(giant_result);
        }

        let (mut output_l, mut output_r) = match pass {
            // Stage 0 at the source rate, then the chain: n_out frames,
            // already on today's alignment (the plan carries the trim).
            IntegratedPass::Multi(ms) => {
                let out = crate::audio::converter::pipeline::ms_route::render_in_ram(
                    &ms,
                    &audio_l,
                    &audio_r,
                    total_input_samples,
                    n_out,
                    settings.use_gpu,
                    settings.precision,
                    &format!("Multistage stage 0 ({}, {} taps)", mode_str, ms.branch.len()),
                    &file_state,
                    0,
                    600,
                )?;
                lab_chain.push("PFR");
                drop(ms);
                crate::audio::memory::log_process_memory("after multistage pass");
                file_state.gpu_pct.store(620, Ordering::Relaxed);
                out
            }
            IntegratedPass::Single(SinglePlan {
                phases,
                sub_taps,
                scale,
                flush_input_samples,
                group_delay_samples,
            }) => {
                // ── Run all L sub-filters: parallel on CPU, sequential on GPU ──
                // Each phase writes its samples straight into the strided slots of
                // the final output buffer (out[frame*L+phase]) — no intermediate
                // per-phase streams, no interleave copy. per-phase length × L ==
                // n_out_flush by construction.
                let n_out_flush = n_out + flush_input_samples * l;
                let required_mb = (n_out_flush as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;
                let (mut output_l, mut output_r) =
                    crate::audio::memory::await_free_ram_and_allocate(required_mb, || {
                        (vec![0.0f64; n_out_flush], vec![0.0f64; n_out_flush])
                    });
                run_polyphase_pass(
                    &phases,
                    &audio_l,
                    &audio_r,
                    total_input_samples,
                    flush_input_samples,
                    scale,
                    &mut output_l,
                    &mut output_r,
                    settings.use_gpu,
                    settings.precision,
                    &format!("Polyphase ({}, {} taps/phase)", mode_str, sub_taps),
                    Some(lin_path_str.as_str()),
                    &file_state,
                    0,
                    600,
                )?;
                lab_chain.push("PFR");
                drop(phases); // frees the decomposed filter copy before the trim/hybrid stages
                crate::audio::memory::log_process_memory("after linear polyphase pass");
                file_state.gpu_pct.store(620, Ordering::Relaxed);

                // ── Group delay trim ──
                // group_delay_samples was computed before the pass (shared with the
                // giant route). After trim, truncate output to exactly n_out
                // (= total_input_samples * L) so no flush-tail silence leaks out.

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
                (output_l, output_r)
            }
        };
        crate::aelog!(
            "[CONV]   Polyphase output: {} samples = {:.3}s @ {}Hz",
            output_l.len(),
            output_l.len() as f64 / out_rate as f64,
            out_rate
        );

        // ═══ HYBRID-PHASE IN POLYPHASE FIR PATH ═══
        // Second pass: process source through minimum-phase filter via polyphase,
        // then apply zero-crossing hard switch between linear and minimum outputs.
        // TFS mode disables hybrid: the TFS filter already blends phase responses.
        // A Hybrid-Phase render takes the level HP off would (`hp_off`).
        let mut hp_off = None;
        if settings.hybrid_phase && !tfs_requested {
            set_status("Hybrid-Phase: loading minimum-phase filter...");
            file_state.gpu_pct.store(610, Ordering::Relaxed);

            // ── Load minimum-phase filter (ratio-aware) ──
            // Routes through the canonical resolver in dsp/filter.rs.
            let min_path_str = find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "minimum_phase")
                .ok_or_else(|| {
                    crate::audio::converter::dsp::filter::missing_filter_error(
                        settings.taps,
                        audio.sample_rate,
                        out_rate,
                        "minimum_phase",
                    )
                })?;
            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): loading pre-computed {}",
                min_path_str
            );
            // Multistage: this half's stage 0 (branch 0 of the minimum-phase
            // blob, today's band-weighted trim) through the same chain, on
            // today's output frames; else today's single-stage pass.
            let ms_min = if lin_multi {
                let ms_min = crate::audio::converter::pipeline::ms_route::prepare_min(
                    settings,
                    &min_path_str,
                    audio.sample_rate,
                    out_rate,
                    l,
                )?;
                Some(ms_min.ok_or("Hybrid-Phase: the minimum-phase half is not multistage while the linear one is")?)
            } else {
                None
            };
            let (mut min_output_l, mut min_output_r) = if let Some(ms_min) = ms_min {
                set_status("Hybrid-Phase: multistage minimum-phase pass...");
                file_state.gpu_pct.store(620, Ordering::Relaxed);
                crate::audio::converter::pipeline::ms_route::render_in_ram(
                    &ms_min,
                    &audio_l,
                    &audio_r,
                    total_input_samples,
                    n_out,
                    settings.use_gpu,
                    settings.precision,
                    &format!("Hybrid-Phase: multistage stage 0 (min-phase, {} taps)", ms_min.branch.len()),
                    &file_state,
                    620,
                    140,
                )?
            } else {
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
                drop(min_coeffs); // decomposed into min_phases; free the full blob before the pass

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
                // Strided direct write, same as the linear pass: no per-phase
                // streams, no interleave copy.
                let n_out_min_flush = n_out + (min_flush_input * l);
                let required_mb_min = (n_out_min_flush as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;
                let (mut min_output_l, mut min_output_r) =
                    crate::audio::memory::await_free_ram_and_allocate(required_mb_min, || {
                        (vec![0.0f64; n_out_min_flush], vec![0.0f64; n_out_min_flush])
                    });
                run_polyphase_pass(
                    &min_phases,
                    &audio_l,
                    &audio_r,
                    total_input_samples,
                    min_flush_input,
                    min_scale,
                    &mut min_output_l,
                    &mut min_output_r,
                    settings.use_gpu,
                    settings.precision,
                    "Hybrid-Phase: polyphase (min-phase)",
                    Some(min_path_str.as_str()),
                    &file_state,
                    620,
                    140,
                )?;
                drop(min_phases);

                // ── Trim minimum-phase group delay ──
                // We trim OLA algorithmic latency PLUS the calculated Center of Gravity
                // of the minimum-phase filter. This time-aligns y_minimum with y_linear 
                // well enough to prevent destructive phase cancellations during crossfades.
                let min_group_delay = (min_ola_latency * l) + min_filter_delay_samples;
                if min_group_delay > 0 && min_group_delay < min_output_l.len() {
                    min_output_l.drain(..min_group_delay);
                    min_output_r.drain(..min_group_delay);
                }
                (min_output_l, min_output_r)
            };

            // Ensure same length as linear output
            let final_len = output_l.len().min(min_output_l.len());
            output_l.truncate(final_len);
            output_r.truncate(final_len);
            min_output_l.truncate(final_len);
            min_output_r.truncate(final_len);

            // ── Compute blend envelope ──
            file_state.gpu_pct.store(790, Ordering::Relaxed);
            let envelope = if let Some(net) = &hp_net {
                // The drum kit's lists, as the player takes them: made in
                // memory, the detector's sidecar stays its own.
                set_status("Hybrid-Phase: envelope with the drum kit...");
                let (analysis, env_sr) = crate::audio::hpss_native::envelope_with_net(&audio_l, &audio_r, audio.sample_rate, &net.lists, net.kit);
                drop(audio_l);
                drop(audio_r);
                crate::audio::hybrid_phase::envelope_from(analysis, env_sr, final_len, out_rate as f64)
            } else {
                // Auto-generate HPSS envelope if not exists, then load it
                // ── HPSS envelope: native Rust (fast) ──
                set_status("Hybrid-Phase: generating HPSS envelope (Rust)...");
                crate::audio::hpss_native::generate_and_save(
                    src_path,
                    &audio_l,
                    &audio_r,
                    audio.sample_rate,
                    &file_state.cancelled,
                )
                .map_err(|e| format!("HPSS Native failed: {}", e))?;
                // Source-rate input is no longer needed past HPSS — free it
                // before the blend allocates the envelope.
                drop(audio_l);
                drop(audio_r);

                crate::audio::hybrid_phase::load_external_envelope(
                    src_path,
                    final_len,
                    out_rate as f64,
                )
                .ok_or_else(|| "Hybrid-Phase: failed to load HPSS envelope".to_string())?
            };

            // ── Zero-crossing hard switch blend (stereo-linked) ──
            // One switch plan from the mid difference → L and R change phase
            // at the same sample, keeping the phantom image stable on attacks.
            // In-place: the result lands in output_l/output_r without
            // allocating two more full-length buffers.
            hp_off = Some(hp_off_level_in_ram(
                settings, &output_l, &output_r, out_rate, true_peak_target_dbtp, album_gain, &file_state,
            )?);
            if settings.lab.continuous_alpha {
                set_status("Hybrid-Phase: applying continuous-alpha blend (FIR path)...");
                crate::aelog!("[ALPHA] Continuous hybrid blend enabled (FIR path)");
                crate::audio::hybrid_phase::blend_outputs_stereo_continuous(
                    &mut output_l, &min_output_l,
                    &mut output_r, &min_output_r,
                    &envelope, out_rate as f64, settings.lab.hp_sens,
                );
                lab_outcomes.push(lab::LabOutcome {
                    feature: "ALPHA",
                    level: "ok",
                    text: "continuous alpha blend applied (FIR polyphase path)".to_string(),
                });
                lab_chain.push("aHP");
            } else {
                set_status("Hybrid-Phase: applying zero-crossing hard switch...");
                crate::audio::hybrid_phase::blend_outputs_stereo_inplace(
                    &mut output_l, &min_output_l,
                    &mut output_r, &min_output_r,
                    &envelope, 0, out_rate as f64, settings.lab.hp_sens,
                );
            }
            drop(min_output_l);
            drop(min_output_r);
            crate::audio::memory::log_process_memory("after hybrid blend");

            lab_chain.push("HP");
            crate::aelog!(
                "[CONV] Hybrid-Phase (FIR): blending complete, {} samples",
                output_l.len()
            );

            // Note: without the kit, .onset_envelope.json was already written by hpss_native::generate_and_save()
        } else {
            // Non-hybrid: the source-rate input was only needed by the
            // polyphase pass above — free it before true-peak/encode/verify.
            drop(audio_l);
            drop(audio_r);
        }

        // A cancel is looked at between the stages of the finished render:
        // none of them looks at it inside, and together they run for many
        // seconds on a long file at ×16 — a batch cancelled here went on
        // that long before it let the next one start.
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }
        // XTC before the ceiling is measured: the filter lifts the bottom
        // end, and the ceiling is what pays for it.
        run_xtc(
            settings.lab.xtc, &settings.lab.xtc_geometry,
            &mut output_l, &mut output_r, out_rate,
            &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
        );
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }

        // Final true-peak protection against convolution overshoot. The gain
        // is measured here and applied inside the dither pass below, so the
        // buffer is traversed once instead of twice.
        match &hp_off {
            Some(h) => run_hp_hold(
                settings.lab.isp, h, &mut output_l, &mut output_r, true_peak_target_dbtp, album_gain,
                out_rate, &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
            ),
            None => run_output_limiter(
                settings.lab.isp, &mut output_l, &mut output_r, true_peak_target_dbtp, album_gain,
                out_rate, &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
            ),
        }
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }
        // The subsonic filter again, at the output rate, on what the limiter
        // and the phase blend actually left. Before the ceiling is measured.
        run_output_subsonic(
            settings.subsonic_hz, &mut output_l, &mut output_r, out_rate, &mut lab_outcomes,
        )?;
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }
        // The listener's EQ before the ceiling is measured, so its boosts are
        // held under the target too (`eq_then_ceiling`); then nothing is left
        // for the dither or the modulator to apply.
        let (peak_gain, tail_gain) = eq_then_ceiling(
            settings.lab.eq.as_ref(), &hp_off, &mut output_l, &mut output_r, true_peak_target_dbtp, album_gain,
            out_rate, &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
        );

        // DSF: the modulator takes the f64 render; no PCM dither.
        if let Some(dsd) = settings.dsd {
            let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref(), &lab_chain, single_fallback);
            let mut dsd_tags = build_lab_vorbis_tags(&lab_tags, &lab_chain, settings.dsd.is_some(), settings.rack_sig.as_deref(), settings.lab.hp_sens, &hp_net_stamp);
            push_upsampler_tag(&mut dsd_tags, ms_chain, single_fallback);
            let src_tags = source_tags_for(&audio, src_path);
            let path = finish_dsd_in_ram(
                dsd, &output_l, &output_r, out_rate, tail_gain, &audio, src_path,
                &output_name, &src_tags.with(dsd_tags), &src_tags.pictures, &file_state,
                &album_status, lab::ceiling_note(peak_gain, true_peak_target_dbtp),
            )?;
            publish_lab_chain(
                &file_state, settings, &lab_outcomes, aa_verdict(&file_state),
                Some(true), ms_chain, hp_verdict(settings, tfs_requested, settings.hybrid_phase && !tfs_requested, &hp_net_stamp),
            );
            lab::print_lab_report(&output_name, &lab_outcomes);
            return Ok(path);
        }

        // Final mathematical quantization step before Encode — same output
        // stage as the standard path (the polyphase branch used to skip
        // dither entirely, leaving raw f64→24-bit truncation to ffmpeg).
        // The album level goes in here with the ceiling's own gain.
        apply_dithering_and_noise_shaping(
            &mut output_l, &mut output_r, out_rate, tail_gain,
        );

        // Encode
        set_status("Encoding FLAC...");
        file_state.gpu_pct.store(800, Ordering::Relaxed);

        // Build final VORBIS_COMMENT tag list and filename.
        let mut flac_tags = build_lab_vorbis_tags(&lab_tags, &lab_chain, settings.dsd.is_some(), settings.rack_sig.as_deref(), settings.lab.hp_sens, &hp_net_stamp);
        push_upsampler_tag(&mut flac_tags, ms_chain, single_fallback);
        let src_tags = source_tags_for(&audio, src_path);
        let flac_tags = src_tags.with(flac_tags);
        let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref(), &lab_chain, single_fallback);
        let output_dir = src_path.parent().unwrap_or(Path::new("."));
        let output_path = output_dir.join(&output_name);

        encode_flac(&output_l, &output_r, out_rate, &output_path, &flac_tags, &src_tags.pictures)?;
        crate::audio::memory::log_process_memory("after encode");

        // Bit-perfect verification — parity with the standard path.
        set_status("Verifying bit-perfect output...");
        file_state.gpu_pct.store(950, Ordering::Relaxed);

        return match verify_flac(&output_path, &output_l, &output_r, out_rate, &file_state) {
            Ok(_) => {
                let file_size = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let size_mb = file_size as f64 / 1_048_576.0;
                set_status(&format!(
                    "\u{2713} Done: {} ({:.1} MB{}{})",
                    output_name,
                    size_mb,
                    album_status,
                    lab::ceiling_note(peak_gain, true_peak_target_dbtp)
                ));
                file_state.gpu_pct.store(1000, Ordering::Relaxed);
                publish_lab_chain(
                    &file_state, settings, &lab_outcomes, aa_verdict(&file_state),
                    Some(true), ms_chain, hp_verdict(settings, tfs_requested, settings.hybrid_phase && !tfs_requested, &hp_net_stamp),
                );
                lab::print_lab_report(&output_name, &lab_outcomes);
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

        let fs: &FileConvState = &file_state; // a copyable view for both channels
        let make_channel = move |src: std::sync::Arc<Vec<f64>>, p: SincInterpolationParameters| -> Result<Vec<f64>, String> {
            let n = src.len();
            let mut rs = SincFixedIn::<f64>::new(ratio, 2.0, p, chunk, 1)
                .map_err(|e| format!("Resampler init: {}", e))?;
            let mut out = Vec::with_capacity((n as f64 * ratio * 1.02) as usize);
            let mut buf = vec![vec![0.0f64; chunk]];
            let mut pos = 0;
            while pos < n {
                if fs.stop_requested() {
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
    // We look up the pre-computed FIR blob keyed on this conversion's source
    // and output rates so the filter's cutoff is correct for it: a hi-res
    // source takes the blob of its own factor (`filter::design_rate`). The
    // naming convention is `fir_<TAG>_<TARGET_HZ>_<phase>.npy`;
    // `find_precomputed_filter` also accepts the legacy single-rate file
    // (`fir_<TAG>_<phase>.npy`) but ONLY at the legacy design point (FS8).
    //
    // If no suitable file exists, the conversion FAILS. It used to skip the
    // post-FIR and let the rubato resampler's output go downstream, which was
    // the worst possible outcome: the file still appeared, the UI still said
    // "30M Taps", and nothing but a line buried in the console told the
    // listener they were hearing an ordinary resampler instead. A missing
    // filter is a setup problem, and the honest thing is to say so and stop.
    //
    // The custom_filter_path branch is left untouched: if the user explicitly
    // points at a custom .npy, we trust them and skip the lookup.
    let precomputed_path = if settings.custom_filter_path.is_some() {
        settings.custom_filter_path.clone()
    } else if settings.taps > 0 {
        match find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "linear_phase") {
            Some(path) => Some(path),
            None => {
                let msg = crate::audio::converter::dsp::filter::missing_filter_error(
                    settings.taps,
                    audio.sample_rate,
                    out_rate,
                    "linear_phase",
                );
                crate::aelog!("[CONV] ❌ {}", msg);
                return Err(msg);
            }
        }
    } else {
        None
    };
    let post_fir_available = precomputed_path.is_some();

    let (final_l, final_r) = if settings.taps > 0 && post_fir_available {
        let mode_str = if settings.use_gpu { "GPU" } else { "CPU" };
        set_status(&format!("Applying FIR post-filter ({})...", mode_str));

        let coeffs_path = precomputed_path.as_deref().unwrap();
        crate::aelog!("[CONV] Loading pre-computed linear-phase: {}", coeffs_path);
        let coeffs = crate::audio::dsp_core::load_npy_f64(coeffs_path)?;
        let id = crate::audio::gpu::filter_cache::FilterId::whole(coeffs_path);
        let mut dsp: Box<dyn DspProcessor + Send> = crate::audio::gpu::build_convolver(
            &coeffs,
            settings.precision,
            Some(&id),
            settings.use_gpu,
        )?;

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
            if file_state.stop_requested() {
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

            // extend_from_slice lowers to a memcpy; the old per-sample push
            // paid a capacity check and a length bump per element and could
            // not vectorize.
            out_l.extend_from_slice(&out_l_buf[..actual]);
            out_r.extend_from_slice(&out_r_buf[..actual]);

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
            if file_state.stop_requested() {
                return Err("Cancelled".to_string());
            }

            dsp.process_audio(&zero_in, &zero_in, &mut out_l_buf, &mut out_r_buf, chunk);

            out_l.extend_from_slice(&out_l_buf[..chunk]);
            out_r.extend_from_slice(&out_r_buf[..chunk]);

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
            true // the built-in (Kaiser) filter is always linear-phase
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
    // and the matching minimum-phase filter at the SAME cutoff — blending
    // min-phase-at-the-wrong-cutoff with clean linear output makes no
    // acoustic sense.
    //
    // Either piece missing is an error, not a reason to quietly hand back the
    // linear-only render: the user ticked Hybrid-Phase, the output filename
    // would still carry the "HP" tag, and only a console line would ever say
    // the transient engine never ran.
    if settings.hybrid_phase {
        if !post_fir_available {
            let msg = "Hybrid-Phase needs the post-FIR stage, which is not active for this \
                       configuration. Select a tap count with filter files available."
                .to_string();
            crate::aelog!("[CONV] ❌ {}", msg);
            return Err(msg);
        }
        if find_precomputed_filter(settings.taps, audio.sample_rate, out_rate, "minimum_phase").is_none() {
            let msg = crate::audio::converter::dsp::filter::missing_filter_error(
                settings.taps,
                audio.sample_rate,
                out_rate,
                "minimum_phase",
            );
            crate::aelog!("[CONV] ❌ Hybrid-Phase cannot run. {}", msg);
            return Err(msg);
        }
    }
    // A Hybrid-Phase render takes the level HP off would: measured here, on
    // the linear-phase render, before the blend.
    let mut hp_off = None;
    let (final_l, final_r) = if settings.hybrid_phase {
        lab_chain.push("HP");
        hp_off = Some(hp_off_level_in_ram(
            settings, &final_l, &final_r, out_rate, true_peak_target_dbtp, album_gain, &file_state,
        )?);
        let result = crate::audio::converter::pipeline::apply_hybrid_phase(final_l, final_r, settings, src_path, audio.sample_rate, out_rate, Arc::clone(&file_state), &resampled_l_saved, &resampled_r_saved, &hpss_src_l, &hpss_src_r, hpss_src_sr, hp_net.as_ref())?;
        if settings.lab.continuous_alpha {
            lab_outcomes.push(lab::LabOutcome {
                feature: "ALPHA",
                level: "ok",
                text: "continuous alpha blend applied (standard path)".to_string(),
            });
            lab_chain.push("aHP");
        }
        result
    } else {
        (final_l, final_r)
    };

    // Final true-peak protection against convolution overshoot; the gain is
    // folded into the dither pass rather than applied in its own traversal.
    let mut final_l = final_l;
    let mut final_r = final_r;
    run_xtc(
        settings.lab.xtc, &settings.lab.xtc_geometry,
        &mut final_l, &mut final_r, out_rate,
        &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
    );
    match &hp_off {
        Some(h) => run_hp_hold(
            settings.lab.isp, h, &mut final_l, &mut final_r, true_peak_target_dbtp, album_gain, out_rate,
            &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
        ),
        None => run_output_limiter(
            settings.lab.isp, &mut final_l, &mut final_r, true_peak_target_dbtp, album_gain, out_rate,
            &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
        ),
    }
    // The subsonic filter again, at the output rate, on what the limiter and
    // the phase blend actually left. Before the ceiling is measured.
    run_output_subsonic(
        settings.subsonic_hz, &mut final_l, &mut final_r, out_rate, &mut lab_outcomes,
    )?;
    // The listener's EQ, then the ceiling on what it gave (`eq_then_ceiling`).
    let (peak_gain, tail_gain) = eq_then_ceiling(
        settings.lab.eq.as_ref(), &hp_off, &mut final_l, &mut final_r, true_peak_target_dbtp, album_gain,
        out_rate, &mut lab_tags, &mut lab_chain, &mut lab_outcomes,
    );

    // Final mathematical quantization step before Encode; the album level
    // goes in with the ceiling's own gain.
    // (Not for DSF: the modulator takes the f64 render.)
    if settings.dsd.is_none() {
        apply_dithering_and_noise_shaping(&mut final_l, &mut final_r, out_rate, tail_gain);
    }

    // 4. Encode FLAC
    set_status("Encoding FLAC...");
    file_state.gpu_pct.store(800, Ordering::Relaxed); // 80%

    // TFS applies only to the integrated FIR path; make that visible instead of
    // silently ignoring the toggle on the standard (rubato) path.
    if settings.lab.tfs_phase {
        crate::aelog!("[TFS] TFS phase applies to the Polyphase FIR path only — ignored on the standard path");
        lab_outcomes.push(lab::LabOutcome {
            feature: "TFS",
            level: "none",
            text: "applies to Polyphase FIR path only (enable FIR resampling)".to_string(),
        });
    }

    if let Some(dsd) = settings.dsd {
        let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref(), &lab_chain, false);
        let src_tags = source_tags_for(&audio, src_path);
        let dsd_tags = src_tags.with(build_lab_vorbis_tags(&lab_tags, &lab_chain, settings.dsd.is_some(), settings.rack_sig.as_deref(), settings.lab.hp_sens, &hp_net_stamp));
        let path = finish_dsd_in_ram(
            dsd, &final_l, &final_r, out_rate, tail_gain, &audio, src_path,
            &output_name, &dsd_tags, &src_tags.pictures, &file_state,
            &album_status, lab::ceiling_note(peak_gain, true_peak_target_dbtp),
        )?;
        publish_lab_chain(
            &file_state, settings, &lab_outcomes, aa_verdict(&file_state),
            Some(false), None, hp_verdict(settings, false, settings.hybrid_phase, &hp_net_stamp),
        );
        lab::print_lab_report(&output_name, &lab_outcomes);
        return Ok(path);
    }

    // Build final VORBIS_COMMENT tag list and filename.
    let src_tags = source_tags_for(&audio, src_path);
    let flac_tags = src_tags.with(build_lab_vorbis_tags(&lab_tags, &lab_chain, settings.dsd.is_some(), settings.rack_sig.as_deref(), settings.lab.hp_sens, &hp_net_stamp));
    let output_name = build_output_name(&audio, settings, src_path, out_rate, apod_tag.as_deref(), &lab_chain, false);
    let output_dir = src_path.parent().unwrap_or(Path::new("."));
    let output_path = output_dir.join(&output_name);

    encode_flac(&final_l, &final_r, out_rate, &output_path, &flac_tags, &src_tags.pictures)?;

    // 5. Verification
    set_status("Verifying bit-perfect output...");
    file_state.gpu_pct.store(950, Ordering::Relaxed);

    match verify_flac(&output_path, &final_l, &final_r, out_rate, &file_state) {
        Ok(_) => {
            let file_size = std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let size_mb = file_size as f64 / 1_048_576.0;
            set_status(&format!(
                "✓ Done: {} ({:.1} MB{}{})",
                output_name,
                size_mb,
                album_status,
                lab::ceiling_note(peak_gain, true_peak_target_dbtp)
            ));
            file_state.gpu_pct.store(1000, Ordering::Relaxed); // 100%
            publish_lab_chain(
                &file_state, settings, &lab_outcomes, aa_verdict(&file_state),
                // TFS never applies on this route (it is refused above), so
                // Hybrid-Phase is the only thing that could have rendered the
                // phase, and it did if it was asked to.
                Some(false), None, hp_verdict(settings, false, settings.hybrid_phase, &hp_net_stamp),
            );
            lab::print_lab_report(&output_name, &lab_outcomes);
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

    /// The PFR row and the tags say how the render was made: single stage
    /// word for word as before and no tag; multistage names its chain.
    #[test]
    fn every_render_carries_the_stamp_with_or_without_lab() {
        use crate::audio::converter::stamp;
        // No lab feature fired: the stamp alone (it used to be no tags at all).
        let plain = build_lab_vorbis_tags(&[], &[], false, Some("rack"), 0, "");
        assert_eq!(plain, stamp::stamp_tags(&[], false, Some("rack"), 0, ""));
        assert!(plain.iter().all(|(k, _)| k != "AURAENGINE_LAB"));
        // A lab render: the stamp first, then the lab keys as before.
        let lab_tags = vec![("AURA_DECLIP".to_string(), "ran".to_string())];
        let lab = build_lab_vorbis_tags(&lab_tags, &["DC"], true, None, 0, "");
        assert_eq!(lab[..3], stamp::stamp_tags(&["DC"], true, None, 0, "")[..]);
        assert_eq!(lab[2].1, "FIR:1,DSD:1,DC:1");
        assert_eq!(lab[3], ("AURAENGINE_LAB".to_string(), "1".to_string()));
        assert_eq!(lab[4], ("AURA_CHAIN".to_string(), "DC".to_string()));
        assert_eq!(lab[5], lab_tags[0]);
        // Written by the encoder, read back from the headers: a valid render.
        let dir = tempfile::tempdir().unwrap();
        for (name, tags) in [("plain.flac", &plain), ("lab.flac", &lab)] {
            let path = dir.path().join(name);
            let n = 4096;
            let l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.01).sin() * 0.25).collect();
            encode_flac(&l, &l, 88_200, &path, tags, &[]).expect("encode");
            assert_eq!(stamp::read_render_stamp(&path).unwrap().as_ref(), Some(tags));
            assert!(stamp::playable(&path));
            let raw = std::fs::read(&path).unwrap();
            let vendor = stamp::vendor();
            assert!(raw.windows(vendor.len()).any(|w| w == vendor.as_bytes()));
        }
    }

    #[test]
    fn pfr_row_and_tags_name_the_upsampler() {
        use crate::audio::converter::dsp::ms_h1::h1_chain;
        assert_eq!(pfr_why(30_000_000, None), "the FIR was the resampler: one convolution, no second stage");
        let chain = h1_chain(44_100, 8).expect("the shipped x8 chain");
        let (a, b) = (chain[0].taps.len(), chain[1].taps.len());
        assert_eq!(
            pfr_why(30_000_000, Some(chain)),
            format!(
                "30M taps at the source rate + short exact interpolator ×8 (×2 · ×4, {a} + {b} taps): \
                 matches single-stage to −204…−213 dBFS"
            )
        );
        let mut tags = vec![("AURA_CHAIN".to_string(), "DC".to_string())];
        push_upsampler_tag(&mut tags, None, false);
        assert_eq!(tags.len(), 1, "single stage either way adds no tag");
        push_upsampler_tag(&mut tags, Some(chain), false);
        assert_eq!(tags[1], ("AURA_UPSAMPLER".to_string(), format!("multistage x2({a})+x4({b})")));
        // The hidden single-stage fallback says so: it is no longer the default.
        let mut tags = Vec::new();
        push_upsampler_tag(&mut tags, None, true);
        assert_eq!(tags, vec![("AURA_UPSAMPLER".to_string(), "single".to_string())]);
    }

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
        let n_out = total_input * l;
        let n_out_flush = (total_input + flush) * l;
        let mut out_l = vec![0.0f64; n_out_flush];
        let mut out_r = vec![0.0f64; n_out_flush];
        run_polyphase_pass(
            &phases, &in_l, &in_r, total_input, flush, scale,
            &mut out_l, &mut out_r,
            false, 64, "test", None, &file_state, 0, 600,
        )
        .expect("polyphase pass should succeed");

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

    /// ✕ on one of two files converting at once stops that file only: its
    /// pass ends "Cancelled", the other's runs to the end, and the batch's
    /// cancel (CONV_CANCEL) never goes up. Before, the one-file cancel raised
    /// the global flag and the file beside it ended Cancelled too.
    #[test]
    fn cancel_file_stops_only_its_own_file() {
        use crate::audio::converter::manager::cancel_file;
        let l = 2usize;
        let h = kaiser_lowpass(255, 1.0 / l as f64);
        let scale = l as f64 / h.iter().sum::<f64>().abs();
        let phases = polyphase_decompose(&h, l);
        let a = Arc::new(FileConvState::new());
        let b = Arc::new(FileConvState::new());
        *CONV_FILE_STATES.lock().unwrap() = vec![Arc::clone(&a), Arc::clone(&b)];
        let start = Arc::new(std::sync::Barrier::new(3));
        let run = |state: Arc<FileConvState>, n: usize| {
            let phases = phases.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                let _scope = enter_file(Some(Arc::clone(&state)));
                let input = vec![0.25f64; n];
                let mut out_l = vec![0.0f64; (n + 4096) * l];
                let mut out_r = vec![0.0f64; (n + 4096) * l];
                start.wait();
                run_polyphase_pass(
                    &phases, &input, &input, n, 4096, scale, &mut out_l, &mut out_r,
                    false, 64, "test", None, &state, 100, 600,
                )
            })
        };
        // A is long enough to be still running when its ✕ comes.
        let ta = run(Arc::clone(&a), 8_000_000);
        let tb = run(Arc::clone(&b), 400_000);
        start.wait();
        while a.gpu_pct.load(Ordering::Relaxed) <= 100 {
            std::thread::yield_now();
        }
        cancel_file(0);
        assert!(!CONV_CANCEL.load(Ordering::Relaxed), "a one-file cancel is not the batch's");
        assert_eq!(ta.join().unwrap(), Err("Cancelled".to_string()));
        assert_eq!(tb.join().unwrap(), Ok(()), "the file beside it converts to the end");
        assert!(a.is_cancelled() && !b.is_cancelled());
        CONV_FILE_STATES.lock().unwrap().clear();
    }

    /// The checks answer for the file the thread works on, and only there.
    #[test]
    fn cancel_scope_follows_the_thread() {
        let f = Arc::new(FileConvState::new());
        f.cancelled.store(true, Ordering::Relaxed);
        assert_eq!(cancel_requested(), CONV_CANCEL.load(Ordering::Relaxed));
        {
            let _s = enter_file(Some(Arc::clone(&f)));
            assert!(cancel_requested());
            let other = std::thread::spawn(|| cancel_requested() && !CONV_CANCEL.load(Ordering::Relaxed));
            assert!(!other.join().unwrap(), "another thread does not inherit the file");
            let handed = current_file();
            assert!(std::thread::spawn(move || {
                let _s = enter_file(handed);
                cancel_requested()
            })
            .join()
            .unwrap());
        }
        assert!(current_file().is_none(), "the scope ends with its guard");
    }

    /// Off, never set or flat, the EQ leaves the chain, the tags and the
    /// tail as they were (the dither and the modulator get the gain as
    /// before); on, its token and tag are in before the file is named.
    #[test]
    fn the_eq_is_in_the_chain_only_when_it_changes_the_sound() {
        use crate::audio::converter::dsp::eq::{EqBand, EqState};
        let bell = |gain: f64| EqState { on: true, bands: vec![EqBand { gain, ..Default::default() }], ..Default::default() };
        for off in [None, Some(EqState::default()), Some(EqState { on: false, ..bell(6.0) }), Some(bell(0.0))] {
            let (mut tags, mut chain) = (vec![], vec!["XTC"]);
            assert!(open_eq(off.as_ref(), 96_000, &mut tags, &mut chain).is_none(), "{off:?}");
            assert!(tags.is_empty() && chain == ["XTC"]);
        }
        let (mut tags, mut chain) = (vec![], vec!["XTC"]);
        let mut t = open_eq(Some(&bell(6.0)), 96_000, &mut tags, &mut chain).unwrap();
        assert_eq!(chain, ["XTC", "EQ"]);
        assert_eq!(tags[0].0, "AURA_EQ");
        assert_eq!(tags[0].1, "bands=1;gain=manual;level=0.00dB;out=0.00dB");
        let (mut l, mut r) = (vec![0.25; 64], vec![-0.25; 64]);
        t.process(&mut l, &mut r, 0.5);
        let mut rows = vec![];
        record_eq(&t, &mut rows);
        assert_eq!((rows[0].feature, rows[0].level), ("EQ", "ok"));
        let names = crate::audio::converter::stamp::stamp_tags(&chain, false, None, 0, "");
        let stages = names.iter().find(|(k, _)| k == crate::audio::converter::stamp::KEY_STAGES).unwrap();
        assert!(stages.1.ends_with(&format!("XTC:1,EQ:{}", crate::audio::converter::dsp::eq::STAGE_VERSION)), "{}", stages.1);
    }

    /// A +12 dB bell and +6 dB of output gain on a tone 1 dB under full
    /// scale: the ceiling measured after the EQ holds the true peak at the
    /// target, nothing is clipped, and the samples are the ones `process`
    /// gives with that gain in — at the album level and with none.
    #[test]
    fn the_ceiling_holds_the_true_peak_after_the_eq() {
        use crate::audio::converter::dsp::eq::{EqBand, EqState, EqTail};
        use crate::audio::converter::dsp::true_peak::{measure_true_peak, target_lin_for};
        let rate = 96_000;
        let state = EqState {
            on: true,
            gain: "manual".into(),
            output: 6.0,
            bands: vec![EqBand { gain: 12.0, ..Default::default() }],
            ..Default::default()
        };
        let tone: Vec<f64> = (0..rate as usize / 10)
            .map(|i| 0.89 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64 + 0.3).sin())
            .collect();
        for album in [1.0, 0.7] {
            let (mut l, mut r) = (tone.clone(), tone.iter().map(|x| -0.5 * x).collect::<Vec<_>>());
            let (dry_l, dry_r) = (l.clone(), r.clone());
            let (mut tags, mut chain, mut rows) = (vec![], vec![], vec![]);
            let (peak_gain, tail_gain) =
                eq_then_ceiling(Some(&state), &None, &mut l, &mut r, -1.0, album, rate, &mut tags, &mut chain, &mut rows);
            assert_eq!(tail_gain, 1.0, "the EQ took the gain");
            assert!(album * peak_gain < 0.15, "~18 dB of EQ taken back: {peak_gain}");
            let tp = measure_true_peak(&l, &r);
            assert!((tp - target_lin_for(-1.0)).abs() < 1e-9, "{tp}");
            let eq_row = rows.iter().find(|o| o.feature == "EQ").unwrap();
            assert_eq!(eq_row.level, "ok", "{}", eq_row.text);
            let mut t = EqTail::new(Some(&state), rate).unwrap();
            let (mut pl, mut pr) = (dry_l, dry_r);
            t.process(&mut pl, &mut pr, album * peak_gain);
            assert_eq!(t.clipped, 0);
            let diff = pl.iter().zip(&l).chain(pr.iter().zip(&r)).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            assert!(diff < 1e-12, "{diff}");
        }
        // No EQ: the buffer is left as it was and the gain goes on to the dither.
        let (mut l, mut r) = (tone.clone(), tone.clone());
        let (mut tags, mut chain, mut rows) = (vec![], vec![], vec![]);
        let (peak_gain, tail_gain) = eq_then_ceiling(None, &None, &mut l, &mut r, -1.0, 0.7, rate, &mut tags, &mut chain, &mut rows);
        assert!(l == tone && chain.is_empty());
        assert!(peak_gain == 1.0 && tail_gain == 0.7);
    }
}
