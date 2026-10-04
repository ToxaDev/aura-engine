//! Building a playable chain: source variant → polyphase → phase stage →
//! XTC → limiter → subsonic guard → true-peak gain.
//!
//! The expensive, reusable pieces — source variants, filter banks, XTC
//! filters, subsonic guards, Hybrid-Phase envelopes and plans — are cached
//! here, so toggling a stage back and forth costs only priming.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::blend::{self, AlphaStage, Envelope, HybridStage, Plan, PlanFeed};
use super::convolver::{Alignment, Bank, Input, PolyStream, SourceBuf, SpectrumShare};
use super::gpu::ctx::GpuPolyCtx;
use super::gpu::poly_stream::GpuPolyStream;
use super::gpu::vram_demand_hp;
use super::settings::{Mode, Phase, PlayerSettings};
use super::stages::{skip_to, FirStage, GainStage, LimiterStage, Stage, XtcStage};

/// A track's audio after the engine's source stages, for one set of
/// source settings.
pub struct Variant {
    pub key: String,
    pub src: Arc<SourceBuf>,
    pub out_rate: u32,
    pub l: usize,
    /// The true-peak ceiling prepare decided on (dBTP).
    pub tp_target_dbtp: f64,
    /// True peak of the source variant, 4× (the engine's scanner). A
    /// prediction of the reconstructed output's peak.
    pub tp_pred_lin: f64,
    /// The output limiter may hold this variant's overs locally at its
    /// ceiling (`isp::output_limit_is_local` on the 4× view); false means
    /// they are dense or deep and the whole track comes down instead, as the
    /// converter does. True when nothing needs deciding (direct, the old
    /// quick variant, ISP off).
    pub lim_local: bool,
    /// Stage tokens the source side actually ran ("DC", "ISP", "SUB15", …).
    pub tokens: Vec<String>,
    /// One line per source stage outcome, for the status panel.
    pub notes: Vec<String>,
    /// The source half of the converter's per-file chain, `(token, state,
    /// why)` in pipeline order: only the stages that are on, each saying
    /// whether it ran or declined, in the engine's own words.
    pub stages: Vec<(String, u8, String)>,
    /// Quick variant: decode and DC only, while the full one is prepared.
    pub quick: bool,
    /// Direct mode: raw decode at the source rate, nothing else.
    pub direct: bool,
    /// The source file as it is on disk: path, size and modification time.
    /// Names the Hybrid-Phase envelope file so a later session can reuse it;
    /// empty when the file could not be looked at.
    pub source_id: String,
    /// Instant start's first variant: the stages a stream can run are still
    /// running on the track (`StreamSrc`), and its chains read what they
    /// have made. None for every other variant.
    pub stream: Option<Arc<StreamSrc>>,
}

/// The stages of a track that run as it plays (Instant start's first
/// variant). What needs the whole track — decode, the statistics, DC
/// removal, declip, the Headroom decision — has run on all of it; the
/// intersample repair, the subsonic filter and the static apodizer run on a
/// thread of their own, many times faster than the track plays, into `grow`
/// (`source_stages::SourceStages::after_repairs`). From the first sample the
/// chains on it play what the full variant's play — the Adaptive Apodizer
/// aside, which needs the whole track: its static preset, if the rack has
/// one, stands in until the full variant brings it. The full variant is made
/// from the stream (`prepare_full_variant`): it keeps what that needs.
pub struct StreamSrc {
    pub grow: Arc<super::grow::GrowSource>,
    /// The rack it streams for (`PlayerSettings::source_key`): its sound is
    /// that rack's, not every rack's with the quick key it is cached under.
    pub full_key: String,
    /// The source rate, the output rate and the FS step that rate took.
    pub rate: u32,
    pub out_rate: u32,
    pub fs: u32,
    /// The true-peak ceiling prepare decides on (dBTP).
    pub tp_target: f64,
    /// What reached the static apodizer, for the full variant's Adaptive
    /// Apodizer (both on): taken when that is made.
    pre_apod: Mutex<Option<Arc<super::grow::GrowSource>>>,
    head: StreamHead,
    /// What the stages reported at the end of the track.
    end: Mutex<Option<super::source_stages::StagesEnd>>,
}

impl StreamSrc {
    /// What reached the static apodizer, held for the full variant (tests).
    #[cfg(test)]
    pub(crate) fn pre_apod_weak(&self) -> Option<std::sync::Weak<super::grow::GrowSource>> {
        self.pre_apod.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(Arc::downgrade)
    }
}

/// A stream variant's copy of the track costs more than this: the old quick
/// variant plays instead (a long hi-res file already peaks high).
const STREAM_COPY_MAX_BYTES: u64 = 2 << 30;

/// What a chain of `v` reads: the stream its stages are still making, or the
/// decoded track.
pub fn input_of(v: &Variant) -> super::convolver::Input {
    match &v.stream {
        Some(s) => super::convolver::Input::Grow(s.grow.clone()),
        None => super::convolver::Input::File(v.src.clone()),
    }
}

/// A stream variant whose stages have been through the whole track, holding
/// their output as its source in place of the track before them — which can
/// then go. None while the stream runs, or for any other variant.
pub fn completed(v: &Variant) -> Option<Variant> {
    let s = v.stream.as_ref()?;
    let src = s.grow.complete()?;
    Some(Variant {
        key: v.key.clone(),
        src,
        out_rate: v.out_rate,
        l: v.l,
        tp_target_dbtp: v.tp_target_dbtp,
        tp_pred_lin: v.tp_pred_lin,
        lim_local: v.lim_local,
        tokens: v.tokens.clone(),
        notes: v.notes.clone(),
        stages: v.stages.clone(),
        quick: v.quick,
        direct: v.direct,
        source_id: v.source_id.clone(),
        stream: v.stream.clone(),
    })
}

/// Path, size and modification time of a source file, as one string.
pub(super) fn source_id(path: &Path) -> String {
    let Ok(m) = std::fs::metadata(path) else { return String::new() };
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{}|{}|{}", path.display(), m.len(), mtime)
}

/// Run the engine's `prepare_audio_phase` (Aura mode) or plain decode
/// (Direct mode) for one track.
pub fn prepare_variant(path: &Path, settings: &PlayerSettings, quick: bool, cancel: &AtomicBool) -> Result<Variant, String> {
    // A cancelled conversion leaves the converter's global cancel flag up
    // until its next batch; the player's preparations answer only to their
    // own flag, or they would all stop at the first check in the meantime.
    let _self_only = crate::audio::cancel_flag::self_only(cancel);
    if settings.mode == Mode::Direct {
        let a = crate::audio::converter::decode::decode_file(path)?;
        let rate = a.sample_rate;
        return Ok(Variant {
            stream: None,
            key: "direct".into(),
            src: Arc::new(SourceBuf { l: a.samples_l, r: a.samples_r, rate }),
            out_rate: rate,
            l: 1,
            tp_target_dbtp: 0.0,
            tp_pred_lin: 1.0,
            lim_local: true,
            tokens: vec!["DIRECT".into()],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: true,
            source_id: source_id(path),
        });
    }
    // Instant start's first variant streams the stages a stream can run
    // (`StreamSrc`); the old quick one is left for a copy too large.
    if quick {
        if let Some(v) = prepare_stream_variant(path, settings, cancel)? {
            return Ok(v);
        }
    }
    let base = if quick { settings.quick() } else { settings.clone() };
    // Its steps for the arming bar: the engine's status lines on this
    // thread name them (arming.rs).
    let prep = super::arming::prep_begin(path, &base.source_key(), !quick);
    // A source at or above the chosen output rate: go up an FS step until
    // there is something to upsample to (the converter would skip the file).
    let mut fs = base.fs_multiplier;
    loop {
        let mut s = PlayerSettings { fs_multiplier: fs, ..base.clone() }.to_engine();
        let prepared = crate::audio::converter::decode::with_step_listener(prep.listener(), || {
            crate::audio::converter::pipeline::prepare::prepare_audio_phase(path, &mut s, cancel, None)
        });
        match prepared {
            Ok(p) => {
                prep.step("measure");
                let l = (s.out_rate / p.sample_rate) as usize;
                if l < 1 || s.out_rate % p.sample_rate != 0 {
                    return Err(format!(
                        "{} Hz cannot be upsampled by an integer factor to {} Hz",
                        p.sample_rate, s.out_rate
                    ));
                }
                let tp = crate::audio::converter::dsp::true_peak::measure_true_peak(&p.audio_l, &p.audio_r);
                // The output limiter's track-wide decision, at the ceiling
                // build_chain will use (the variant key holds ISP and the
                // headroom, so it cannot change under this variant).
                let lim_local = quick || !base.isp || {
                    let target_db = if base.headroom_db < 0.0 { p.true_peak_target_dbtp } else { -0.5 };
                    crate::audio::converter::dsp::lab::isp::output_limit_is_local(
                        &p.audio_l, &p.audio_r, 10f64.powf(target_db / 20.0), p.sample_rate)
                };
                let (tokens, notes, stages) =
                    variant_texts(&base, &p.lab_chain, p.apod_tag.as_ref(), &p.lab_outcomes, p.aa_ui.as_ref());
                let key = PlayerSettings { fs_multiplier: fs, ..base.clone() }.source_key();
                let sid = source_id(path);
                // A Hybrid-Phase rack's pair finds this variant's envelope on
                // disk: no envelope work is ahead of it.
                let env_on_disk = matches!(base.phase, Phase::Hybrid | Phase::Alpha)
                    && !sid.is_empty()
                    && CACHE_DIR.get().is_some_and(|d| envelope_sidecar(d, &format!("{}|{}", sid, key)).exists());
                prep.finish(p.sample_rate, p.audio_l.len(), env_on_disk);
                return Ok(Variant {
                    stream: None,
                    key,
                    out_rate: s.out_rate,
                    l,
                    tp_target_dbtp: p.true_peak_target_dbtp,
                    tp_pred_lin: tp,
                    lim_local,
                    tokens,
                    notes,
                    stages,
                    quick,
                    direct: false,
                    source_id: sid,
                    src: Arc::new(SourceBuf { l: p.audio_l, r: p.audio_r, rate: p.sample_rate }),
                });
            }
            Err(e) if e.starts_with("SKIP_RATE") && fs < 16 => {
                fs *= 2;
                prep.retry();
                continue;
            }
            Err(e) if e.starts_with("SKIP_RATE") => {
                return Err("This file is already at the highest output rate — play it in Direct mode".into())
            }
            Err(e) if e.starts_with("BAD_RATE") => {
                return Err(format!(
                    "{}: only the 44.1 kHz and 48 kHz families can be upsampled — play it in Direct mode",
                    e.trim_start_matches("BAD_RATE:")
                ))
            }
            Err(e) => return Err(e),
        }
    }
}

/// The whole-track stages' results a stream starts from, kept for the full
/// variant made from it (`prepare_full_variant`): the engine's settings, the
/// statistics, and the tags, tokens and outcomes before the stream's stages
/// add theirs.
struct StreamHead {
    eng: crate::audio::converter::types::ConvertSettings,
    lab_stats: Option<crate::audio::converter::dsp::lab::stats::SourceStats>,
    lab_tags: Vec<(String, String)>,
    lab_chain: Vec<&'static str>,
    lab_outcomes: Vec<crate::audio::converter::dsp::lab::LabOutcome>,
}

/// A track's stream of source stages, just started (`start_stream`).
struct Started {
    ss: Arc<StreamSrc>,
    /// The track as the whole-track stages left it: what the stream reads.
    base: Arc<SourceBuf>,
    plan: super::source_stages::SourcePlan,
    /// The rack at the FS step the rate needed.
    s: PlayerSettings,
    /// The head's tokens and outcomes with the Headroom decision in, for the
    /// first variant's badges.
    chain: Vec<&'static str>,
    outcomes: Vec<crate::audio::converter::dsp::lab::LabOutcome>,
    /// The repair is on and nothing in the track is over full scale as
    /// decoded, nor did declip draw anything over it: every over the stream's
    /// repair converges on ends at full scale.
    overs_to_full_scale: bool,
}

/// What needs the whole track done on all of it by `prepare_audio_phase`'s
/// own code (decode, the statistics, DC removal, declip), and the stream of
/// the rest started on a thread of its own (`SourceStages::run_file`).
/// `urgent`: a listener waits for it — above normal, on the stream's own
/// pool; otherwise (the next track's prewarm) below normal. None where the
/// old way takes over: a copy of the track over `STREAM_COPY_MAX_BYTES`, or
/// a rate the file path has its own words for.
fn start_stream(
    path: &Path,
    settings: &PlayerSettings,
    cancel: &AtomicBool,
    urgent: bool,
    prep: &super::arming::Prep,
) -> Result<Option<Started>, String> {
    use crate::audio::converter::pipeline::prepare;
    use super::source_stages::{set_thread_priority, stream_pool, SourcePlan, SourceStages, SourceTally};
    if let Some((frames, _, _)) = crate::audio::converter::decode::probe_input_frames(path) {
        if frames.saturating_mul(16) > STREAM_COPY_MAX_BYTES {
            crate::aelog!("[PLAYER] {}: too long to stream its source stages — prepared the old way", path.display());
            return Ok(None);
        }
    }
    let head = crate::audio::converter::decode::with_step_listener(prep.listener(), || {
        crate::audio::converter::decode::set_status(&format!(
            "Decoding: {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let a = crate::audio::converter::decode::decode_file(path)?;
        let rate = a.sample_rate;
        let Ok((out_rate, fs)) = super::radio::chain::out_rate_for(rate, settings.fs_multiplier) else {
            return Ok(None);
        };
        let s = PlayerSettings { fs_multiplier: fs, ..settings.clone() };
        let mut eng = s.to_engine();
        eng.out_rate = out_rate;
        let (mut l, mut r) = (a.samples_l, a.samples_r);
        let head = prepare::source_head(&mut l, &mut r, rate, a.lossy, &eng, cancel)?;
        Ok::<_, String>(Some((s, eng, rate, out_rate, l, r, head)))
    })?;
    let Some((s, eng, rate, out_rate, l, r, head)) = head else { return Ok(None) };
    let prepare::SourceHead { lab_stats, lab_tags, lab_chain, lab_outcomes, declip_spans_l, declip_spans_r, hot_spans_l, hot_spans_r } = head;
    let overs_to_full_scale = s.isp
        && [&declip_spans_l, &declip_spans_r, &hot_spans_l, &hot_spans_r].iter().all(|v| v.is_empty());
    // The first variant's badges: the head's, the Headroom decision in.
    let (mut tags, mut chain, mut outcomes) = (lab_tags.clone(), lab_chain.clone(), lab_outcomes.clone());
    let tp_target = prepare::output_ceiling(&eng, lab_stats.as_ref(), &mut tags, &mut chain, &mut outcomes);
    let plan = SourcePlan::new(&s, rate);

    // The stream: the repair, the subsonic filter and the static apodizer,
    // from the track as the whole-track stages left it, into `grow` — and,
    // the Adaptive Apodizer on, what reaches the static one into `pre_apod`
    // as well, for the full variant's apodizer.
    let n = l.len();
    let base = Arc::new(SourceBuf { l, r, rate });
    let grow = Arc::new(super::grow::GrowSource::new(rate, n));
    let mut pre_apod = (s.adaptive_apodizer && plan.apodizing != 0).then(|| Arc::new(super::grow::GrowSource::new(rate, n)));
    let ss = Arc::new(StreamSrc {
        grow: grow.clone(),
        full_key: settings.source_key(),
        rate,
        out_rate,
        fs: s.fs_multiplier,
        tp_target,
        pre_apod: Mutex::new(pre_apod.clone()),
        head: StreamHead { eng, lab_stats, lab_tags, lab_chain, lab_outcomes },
        end: Mutex::new(None),
    });
    {
        let (base, plan, weak) = (base.clone(), plan.clone(), Arc::downgrade(&ss));
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        std::thread::Builder::new()
            .name("aura-source-stream".into())
            .spawn(move || {
                // Urgent: above the preparations and their probes, below the
                // render — a long linear filter's first block waits for the
                // stream to be its look-ahead in (30M ×8: 42 s of source) —
                // the work on the stream's own pool, never queued behind a
                // preparation's blocks. Else below normal, as the prewarm.
                set_thread_priority(if urgent { 1 } else { -1 });
                let t0 = std::time::Instant::now();
                let pool = if urgent { stream_pool() } else { None };
                let threads = stream_pool().map_or(1, |p| p.current_num_threads());
                let tally = Arc::new(Mutex::new(SourceTally::default()));
                let mut st = SourceStages::after_repairs(&plan, &declip_spans_l, &declip_spans_r, &hot_spans_l, &hot_spans_r, tally, threads);
                if let Some(pre) = &pre_apod {
                    st = st.with_tee_before_apodizer(pre.clone());
                }
                // It stops when nothing reads it any more (the track was left).
                let go_on = || Arc::strong_count(&grow) > 1;
                let end = st.run_file(&base.l, &base.r, pool, &go_on, &mut |a, b| grow.push(a, b));
                if let Some(end) = end {
                    crate::aelog!(
                        "[PLAYER] {}: the source stages streamed the whole track in {:.2} s{}",
                        name,
                        t0.elapsed().as_secs_f64(),
                        end.isp.as_ref().map_or(String::new(), |r| format!(
                            ", {} intersample overs corrected of {}{}",
                            r.fixed,
                            r.clusters,
                            if r.hot > 0 { format!(", hot={} left whole", r.hot) } else { String::new() }
                        ))
                    );
                    if let Some(ss) = weak.upgrade() {
                        *ss.end.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
                    }
                }
                // The side copy is through before the stream says so, and
                // let go of here: the full variant can take it as it is.
                if let Some(pre) = pre_apod.take() {
                    pre.finish();
                }
                grow.finish();
            })
            .map_err(|e| format!("The source stages' thread: {}", e))?;
    }
    Ok(Some(Started { ss, base, plan, s, chain, outcomes, overs_to_full_scale }))
}

/// Instant start's first variant (`StreamSrc`): what needs the whole track
/// done on all of it and the stream of the rest started (`start_stream`),
/// its chains reading the stream as it grows. None where the old quick
/// variant plays instead.
fn prepare_stream_variant(path: &Path, settings: &PlayerSettings, cancel: &AtomicBool) -> Result<Option<Variant>, String> {
    use crate::audio::converter::dsp::lab::{chain_state, CHAIN_DECLINED, CHAIN_RAN};
    let quick_key = settings.quick().source_key();
    let prep = super::arming::prep_begin(path, &quick_key, false);
    let Some(Started { ss, base, plan, s, chain: lab_chain, outcomes: lab_outcomes, overs_to_full_scale }) =
        start_stream(path, settings, cancel, true, &prep)?
    else {
        return Ok(None);
    };
    let declip_ran = lab_chain.contains(&"DC");

    // The chain's tokens and rows in the engine's order: declip, the streamed
    // repair and subsonic filter, AHR, the apodizer.
    let streamed = plan.tokens();
    let mut tokens: Vec<String> = Vec::new();
    if declip_ran {
        tokens.push("DC".into());
    }
    tokens.extend(streamed.iter().filter(|t| !t.starts_with("Apod")).cloned());
    if lab_chain.contains(&"AHR") {
        tokens.push("AHR".into());
    }
    tokens.extend(streamed.iter().filter(|t| t.starts_with("Apod")).cloned());
    let outcome = |feat: &str| lab_outcomes.iter().rev().find(|o| o.feature == feat);
    let mut stages: Vec<(String, u8, String)> = Vec::new();
    if let Some(o) = outcome("DECLIP").filter(|_| s.declip) {
        stages.push(("DC".into(), chain_state(o.level), o.text.clone()));
    }
    if plan.isp {
        stages.push(("ISP".into(), CHAIN_RAN, "intersample overs repaired as the track plays".into()));
    }
    if plan.subsonic_hz != 0 {
        stages.push(("SUB".into(), CHAIN_RAN, format!("linear-phase high-pass below {} Hz as the track plays", plan.subsonic_hz)));
    }
    if let Some(o) = outcome("AHR").filter(|_| s.adaptive_headroom) {
        stages.push(("AHR".into(), chain_state(o.level), o.text.clone()));
    }
    if s.adaptive_apodizer {
        let why = if plan.apodizing > 0 {
            "the static preset plays until the adaptive filter is ready"
        } else {
            "the adaptive filter comes in when it is ready"
        };
        stages.push(("AA".into(), CHAIN_DECLINED, why.into()));
    }
    let notes = lab_outcomes.iter().map(|o| format!("{}: {}", o.feature, o.text)).collect();
    // Its level by the full variant's rule, on the whole track as the head
    // left it: the first sound plays where the full variant will. A slow gain
    // starting at 0 dB took seconds to find a hot track's level, its limiter
    // cutting the overs meanwhile (a source at +7.4 dBTP: up to 5.7 dB, the
    // error 17 dB under the music), and the full variant then came in up to
    // 8 dB lower. The stream's repair comes after the head: where nothing is
    // over full scale as decoded and declip drew nothing over it, the overs it
    // converges on end at full scale — the peak the full variant will find
    // (a loud CD: +1.35 dBTP as decoded, +0.03 after the repair). Elsewhere
    // the repair leaves the peak where it is.
    let (tp, lim_local) = source_level(&base, settings, ss.tp_target, true);
    let tp = if overs_to_full_scale { tp.min(1.0) } else { tp };
    prep.finish(base.rate, base.len(), false);
    Ok(Some(Variant {
        key: PlayerSettings { fs_multiplier: s.fs_multiplier, ..settings.quick() }.source_key(),
        l: (ss.out_rate / base.rate) as usize,
        out_rate: ss.out_rate,
        tp_target_dbtp: ss.tp_target,
        src: base,
        tp_pred_lin: tp,
        lim_local,
        tokens,
        notes,
        stages,
        quick: true,
        direct: false,
        source_id: source_id(path),
        stream: Some(ss),
    }))
}

/// A first variant still fits `s`: an old quick one always (decode and DC
/// are the same for every rack with the quick key), a stream only for the
/// rack it streams for — its sound is that rack's repair, filter and
/// apodizer, and its badges say so.
pub fn first_variant_fits(v: &Variant, s: &PlayerSettings) -> bool {
    v.stream.as_ref().map_or(true, |ss| ss.full_key == s.source_key())
}

/// The full variant from a stream of source stages: the first variant's
/// (`from`, when it streams for this rack) or one started here, waited for
/// and taken as the source — what `prepare_variant` makes with
/// `prepare_audio_phase`, to the bit and with the same badges, but decoded
/// once and streamed many times faster. The Adaptive Apodizer, on, looks at
/// what the stream hands its static preset and runs as the file path runs
/// it (`prepare::adaptive_verdict`, `prepare::apply_apodizer`). `probe` runs
/// while the stream does, on the full variant to be — its source still
/// growing — when nothing after the stream changes it (no Adaptive
/// Apodizer): Instant start off's output probe. `urgent` as in
/// `start_stream`. The old way where no stream can be made.
/// A track's own 4× true peak and the output limiter's decision on it, in
/// pieces side by side — on the stream's own pool when a listener waits:
/// what a variant's level is decided from before its chain is rendered
/// (`level_gain`).
fn source_level(src: &SourceBuf, settings: &PlayerSettings, tp_target: f64, urgent: bool) -> (f64, bool) {
    let target_db = if settings.headroom_db < 0.0 { tp_target } else { -0.5 };
    let target_lin = 10f64.powf(target_db / 20.0);
    let peaks = || {
        let tp = crate::audio::converter::dsp::true_peak::measure_true_peak_parallel(&src.l, &src.r);
        let local = !settings.isp
            || crate::audio::converter::dsp::lab::isp::output_limit_is_local_parallel(&src.l, &src.r, target_lin, src.rate);
        (tp, local)
    };
    match super::source_stages::stream_pool().filter(|_| urgent) {
        Some(p) => p.install(peaks),
        None => peaks(),
    }
}

pub fn prepare_full_variant(
    path: &Path,
    settings: &PlayerSettings,
    cancel: &AtomicBool,
    from: Option<Arc<StreamSrc>>,
    urgent: bool,
    probe: Option<&dyn Fn(&Variant)>,
) -> Result<Variant, String> {
    use crate::audio::converter::pipeline::prepare;
    let _self_only = crate::audio::cancel_flag::self_only(cancel);
    if settings.mode == Mode::Direct {
        return prepare_variant(path, settings, false, cancel);
    }
    let from = from.filter(|ss| ss.full_key == settings.source_key());
    let prep = super::arming::prep_begin(path, &settings.source_key(), from.is_none());
    let (ss, own_base) = match from {
        Some(ss) => (ss, None),
        None => match start_stream(path, settings, cancel, urgent, &prep)? {
            Some(st) => (st.ss, Some(st.base)),
            None => {
                drop(prep);
                return prepare_variant(path, settings, false, cancel);
            }
        },
    };
    let rate = ss.rate;
    let (out_rate, l) = (ss.out_rate, (ss.out_rate / rate) as usize);
    let key = PlayerSettings { fs_multiplier: ss.fs, ..settings.clone() }.source_key();
    let sid = source_id(path);
    // The stream's stages, for the arming bar: mostly the repair.
    prep.step("isp");
    if let (Some(probe), Some(base), false) = (probe, &own_base, settings.adaptive_apodizer) {
        let peak = base.l.iter().chain(base.r.iter()).fold(0.0f64, |m, &x| m.max(x.abs()));
        probe(&Variant {
            key: key.clone(),
            src: base.clone(),
            out_rate,
            l,
            tp_target_dbtp: ss.tp_target,
            tp_pred_lin: peak,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: sid.clone(),
            stream: Some(ss.clone()),
        });
    }
    drop(own_base);
    if !ss.grow.wait_done(cancel) {
        return Err("Cancelled".into());
    }
    let (Some(streamed), true) = (ss.grow.complete(), ss.end.lock().unwrap_or_else(|e| e.into_inner()).is_some()) else {
        // Stopped short: nothing more to wait for — the old way.
        drop(prep);
        return prepare_variant(path, settings, false, cancel);
    };

    // What the file path reports, in its order: the head's, the repair's and
    // the subsonic filter's, the Headroom decision, the apodizer.
    let head = &ss.head;
    let eng = &head.eng;
    let (mut tags, mut chain, mut outcomes) = (head.lab_tags.clone(), head.lab_chain.clone(), head.lab_outcomes.clone());
    {
        let end = ss.end.lock().unwrap_or_else(|e| e.into_inner());
        let end = end.as_ref().expect("looked at above");
        if eng.lab.isp {
            prepare::isp_outcome(end.isp.as_ref(), &mut tags, &mut chain, &mut outcomes);
        }
        if eng.subsonic_hz != 0 {
            let (frames, e_l, e_r) = end.sub_removed.unwrap_or((streamed.len(), 0.0, 0.0));
            let taps = crate::audio::converter::apodize::design_subsonic_highpass(rate, eng.subsonic_hz).len();
            let rep = crate::audio::converter::apodize::subsonic_report(taps, frames, e_l, e_r);
            prepare::subsonic_outcome(eng.subsonic_hz, &rep, &mut chain, &mut outcomes);
        }
    }
    let tp_target = prepare::output_ceiling(eng, head.lab_stats.as_ref(), &mut tags, &mut chain, &mut outcomes);
    let mut aa_ui = None;
    let (src, apod_tag) = if settings.adaptive_apodizer {
        // What reached the static preset: the side copy, or the stream's
        // output where there is no static preset to reach.
        let side = ss.pre_apod.lock().unwrap_or_else(|e| e.into_inner()).take();
        let pre = match &side {
            Some(g) => g.complete(),
            None if eng.apodizing == 0 || rate > 48_000 => Some(streamed.clone()),
            None => None,
        };
        let Some(pre) = pre else {
            // Taken by an earlier full variant of this stream: the old way.
            drop(prep);
            return prepare_variant(path, settings, false, cancel);
        };
        drop(side);
        let (plan, ui) = crate::audio::converter::decode::with_step_listener(prep.listener(), || {
            prepare::adaptive_verdict(&pre.l, &pre.r, rate, eng, cancel, None)
        })?;
        aa_ui = Some(ui);
        match plan {
            Some(plan) => {
                // Its own copy through the filter: the side copy itself when
                // nothing else holds it.
                drop(streamed);
                let (mut al, mut ar) = match Arc::try_unwrap(pre) {
                    Ok(b) => (b.l, b.r),
                    Err(pre) => (pre.l.clone(), pre.r.clone()),
                };
                let tag = crate::audio::converter::decode::with_step_listener(prep.listener(), || {
                    prepare::apply_apodizer(&mut al, &mut ar, rate, eng, Some(&plan), true, &mut chain, cancel)
                })?;
                (Arc::new(SourceBuf { l: al, r: ar, rate }), tag)
            }
            // Declined: the static preset, which the stream ran.
            None => (streamed, prepare::static_apod_tag(eng, rate)),
        }
    } else {
        (streamed, prepare::static_apod_tag(eng, rate))
    };
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err("Cancelled".into());
    }

    // The source's own 4× peak and the output limiter's decision, as
    // `prepare_variant` measures them — in pieces side by side.
    prep.step("measure");
    let (tp, lim_local) = source_level(&src, settings, tp_target, urgent);
    let (tokens, notes, stages) = variant_texts(settings, &chain, apod_tag.as_ref(), &outcomes, aa_ui.as_ref());
    let env_on_disk = matches!(settings.phase, Phase::Hybrid | Phase::Alpha)
        && !sid.is_empty()
        && CACHE_DIR.get().is_some_and(|d| envelope_sidecar(d, &format!("{}|{}", sid, key)).exists());
    prep.finish(rate, src.len(), env_on_disk);
    Ok(Variant {
        stream: None,
        key,
        out_rate,
        l,
        tp_target_dbtp: tp_target,
        tp_pred_lin: tp,
        lim_local,
        tokens,
        notes,
        stages,
        quick: false,
        direct: false,
        source_id: sid,
        src,
    })
}

/// A 16-bit stereo WAV of `l`/`r` at `rate`, for other modules' tests.
#[cfg(test)]
pub(crate) fn write_test_wav16(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
    let n = l.len().min(r.len());
    let data = (n * 4) as u32;
    let mut b = Vec::with_capacity(44 + n * 4);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        for v in [l[i], r[i]] {
            b.extend_from_slice(&((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
        }
    }
    std::fs::write(path, b).expect("write the test file");
}

/// A full variant's tokens, notes and rows from what its preparation
/// reported: the chain's tokens (a static apodizer's tag after them, once),
/// one note per outcome, and the rows `source_stages` makes of them.
fn variant_texts(
    s: &PlayerSettings,
    lab_chain: &[&str],
    apod_tag: Option<&String>,
    outcomes: &[crate::audio::converter::dsp::lab::LabOutcome],
    aa_ui: Option<&(bool, String)>,
) -> (Vec<String>, Vec<String>, Vec<(String, u8, String)>) {
    let mut tokens: Vec<String> = lab_chain.iter().map(|t| t.to_string()).collect();
    if let Some(tag) = apod_tag {
        if !tokens.iter().any(|t| t == tag) {
            tokens.push(tag.clone());
        }
    }
    let notes = outcomes.iter().map(|o| format!("{}: {}", o.feature, o.text)).collect();
    (tokens, notes, source_stages_of(s, outcomes, aa_ui))
}

/// The source stages the way the converter's per-file chain reports them —
/// process.rs builds its row from the same outcomes with the same mapping
/// (DECLIP → DC, …, the Adaptive Apodizer's own verdict for AA) — so a badge
/// in the player says exactly what the same badge says on a converted row.
fn source_stages_of(
    s: &PlayerSettings,
    outcomes: &[crate::audio::converter::dsp::lab::LabOutcome],
    aa_ui: Option<&(bool, String)>,
) -> Vec<(String, u8, String)> {
    use crate::audio::converter::dsp::lab;
    let mut out = Vec::new();
    let mut from_outcome = |on: bool, feat: &str, tok: &str| {
        if !on {
            return;
        }
        if let Some(o) = outcomes.iter().rev().find(|o| o.feature == feat) {
            out.push((tok.to_string(), lab::chain_state(o.level), o.text.clone()));
        }
    };
    from_outcome(s.declip, "DECLIP", "DC");
    from_outcome(s.isp, "ISP", "ISP");
    from_outcome(s.subsonic_hz != 0, "SUB", "SUB");
    from_outcome(s.adaptive_headroom, "AHR", "AHR");
    if s.adaptive_apodizer {
        if let Some((treated, note)) = aa_ui {
            out.push(("AA".into(), if *treated { lab::CHAIN_RAN } else { lab::CHAIN_DECLINED }, note.clone()));
        }
    }
    out
}

/// A why with something added to it, as one line.
fn join_why(why: &str, more: &str) -> String {
    if why.is_empty() { more.to_string() } else { format!("{}; {}", why, more) }
}

/// Filter banks, XTC pairs, subsonic guards, envelopes and plans, cached.
pub struct Resources {
    banks: Mutex<Vec<(String, Arc<Bank>)>>,
    /// Banks being built now, by key: a second asker waits for the first
    /// rather than build it again (the pair's bank is ordered ahead of the
    /// chain that takes it).
    banks_loading: Mutex<std::collections::HashSet<String>>,
    banks_loaded: std::sync::Condvar,
    xtc: Mutex<HashMap<String, Arc<(Vec<f64>, Vec<f64>)>>>,
    guards: Mutex<HashMap<(u32, u32), Arc<Vec<f64>>>>,
    envs: Mutex<HashMap<String, Arc<Envelope>>>,
    /// One computation of an envelope at a time, a key: a second caller
    /// waits for the first and takes what it made (`envelope`).
    env_turns: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    plans: Mutex<HashMap<String, Arc<Plan>>>,
    /// The output-peak probe's short banks, apart from the real ones.
    probe_banks: Mutex<HashMap<String, Arc<Bank>>>,
    /// `output_peak` results, keyed by track, variant, phase, XTC, ceiling.
    out_peaks: Mutex<HashMap<String, OutPeak>>,
    /// The full chain's probe per track and rack (no phase-envelope or
    /// ceiling in the key): what a quick chain of the same track takes when
    /// it plays again (`quick_peak`).
    quick_hints: Mutex<HashMap<String, OutPeak>>,
    /// Per track: the rail the declip stage would repair from, or None
    /// (`declip::plateau_rail` on the quick variant's audio).
    clip_rails: Mutex<HashMap<String, Option<f64>>>,
    cache_dir: PathBuf,
}

/// Keep at most this many banks resident (30M two-bank chains need two).
const MAX_BANKS: usize = 3;

/// The key a bank is kept under: its filter, its branches and its alignment —
/// and for the band-weighted alignment the rate it plays at as well. That
/// delay is measured over 200 Hz – 6 kHz, a different stretch of the filter at
/// another rate, and one blob plays at more than one: the ×4 blob takes 48 kHz
/// to 192 kHz and 96 kHz to 384 kHz (`filter::design_rate`).
fn bank_key(path: &str, l: usize, align: Alignment, out_rate: u32) -> String {
    match align {
        Alignment::BandWeighted => format!("{}|{}|{:?}|{}", path, l, align, out_rate),
        _ => format!("{}|{}|{:?}", path, l, align),
    }
}

/// When and where the pair's banks were asked for, by path: (path, from, to,
/// thread) (tests).
#[cfg(test)]
static PAIR_BUILDS: Mutex<Vec<(String, std::time::Instant, std::time::Instant, std::thread::ThreadId)>> =
    Mutex::new(Vec::new());

/// `f`, asking for the bank at `path` for a pair (`pair_banks`); tests see
/// when it ran.
fn timed_build<R>(path: &str, f: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    let t0 = std::time::Instant::now();
    let r = f();
    #[cfg(test)]
    PAIR_BUILDS.lock().unwrap().push((path.to_string(), t0, std::time::Instant::now(), std::thread::current().id()));
    #[cfg(not(test))]
    let _ = path;
    r
}

/// The running player's cache directory: a preparation looks there for the
/// Hybrid-Phase envelope of the variant it made (the arming bar then knows
/// whether the pair will need one computed).
static CACHE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The file the onset envelope for `disk_key` (source id | variant key) is
/// kept in.
fn envelope_sidecar(cache_dir: &Path, disk_key: &str) -> PathBuf {
    cache_dir.join(format!("{:016x}{}", fnv(disk_key), ENVELOPE_SUFFIX))
}

impl Resources {
    pub fn new(cache_dir: PathBuf) -> Resources {
        let _ = std::fs::create_dir_all(&cache_dir);
        let _ = CACHE_DIR.set(cache_dir.clone());
        super::arming::load_in_background();
        Resources {
            banks: Mutex::new(Vec::new()),
            banks_loading: Mutex::new(std::collections::HashSet::new()),
            banks_loaded: std::sync::Condvar::new(),
            xtc: Mutex::new(HashMap::new()),
            guards: Mutex::new(HashMap::new()),
            envs: Mutex::new(HashMap::new()),
            env_turns: Mutex::new(HashMap::new()),
            plans: Mutex::new(HashMap::new()),
            probe_banks: Mutex::new(HashMap::new()),
            out_peaks: Mutex::new(HashMap::new()),
            quick_hints: Mutex::new(HashMap::new()),
            clip_rails: Mutex::new(HashMap::new()),
            cache_dir,
        }
    }

    /// A Hybrid-Phase or alpha-HP rack's pair: the minimum-phase bank it adds
    /// to the linear one, built into the cache ahead of the chain that takes
    /// it (`phase_stage`) — the same bank under the same key, built once
    /// (`bank` waits for one being built). Nothing for another rack.
    pub fn warm_pair_bank(&self, v: &Variant, s: &PlayerSettings) {
        if !matches!(s.phase, Phase::Hybrid | Phase::Alpha) || v.direct {
            return;
        }
        // The filter `phase_stage` takes: this variant's factor's.
        let src = v.out_rate / v.l.max(1) as u32;
        let Some(path) = crate::audio::converter::dsp::filter::find_precomputed_filter(s.taps, src, v.out_rate, "minimum_phase") else {
            return;
        };
        let t0 = std::time::Instant::now();
        match self.bank(&path, v.l, Alignment::BandWeighted, v.out_rate) {
            Ok(_) => crate::aelog!(
                "[PLAYER] HP pair: the minimum-phase bank is in the cache ({:.0} ms)",
                t0.elapsed().as_secs_f64() * 1e3
            ),
            Err(e) => crate::aelog!("[PLAYER] HP pair: the minimum-phase bank ahead: {}", e),
        }
    }

    /// `bank`, or for the output-peak probe a bank from its own small cache:
    /// its 5k filters must never push a real bank out of the three kept.
    fn bank_in(&self, probe: bool, path: &str, l: usize, align: Alignment, out_rate: u32) -> Result<Arc<Bank>, String> {
        if !probe {
            return self.bank(path, l, align, out_rate);
        }
        let key = bank_key(path, l, align, out_rate);
        if let Some(b) = self.probe_banks.lock().unwrap().get(&key) {
            return Ok(b.clone());
        }
        let b = Arc::new(Bank::load(path, l, align, out_rate)?);
        let mut g = self.probe_banks.lock().unwrap();
        if g.len() >= 8 {
            g.clear();
        }
        g.insert(key, b.clone());
        Ok(b)
    }

    pub fn bank(&self, path: &str, l: usize, align: Alignment, out_rate: u32) -> Result<Arc<Bank>, String> {
        let key = bank_key(path, l, align, out_rate);
        loop {
            // One build a key: the same bank asked for while it is being
            // built is waited for, then taken from the cache (one that failed
            // is tried again here). The cache is read under this lock: a
            // build ends by caching its bank, then letting go of its key.
            let loading = self.banks_loading.lock().unwrap();
            {
                let mut g = self.banks.lock().unwrap();
                if let Some(i) = g.iter().position(|(k, _)| *k == key) {
                    let e = g.remove(i);
                    let b = e.1.clone();
                    g.push(e);
                    return Ok(b);
                }
            }
            if loading.contains(&key) {
                drop(self.banks_loaded.wait_while(loading, |l| l.contains(&key)).unwrap());
                continue;
            }
            let mut loading = loading;
            loading.insert(key.clone());
            break;
        }
        // Whatever happens to the build, the key stops being built.
        struct Built<'a>(&'a Resources, String);
        impl Drop for Built<'_> {
            fn drop(&mut self) {
                self.0.banks_loading.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.1);
                self.0.banks_loaded.notify_all();
            }
        }
        let _built = Built(self, key.clone());
        let b = Arc::new(Bank::load(path, l, align, out_rate)?);
        crate::aelog!(
            "[PLAYER] bank {} ×{} — {} MB of spectra",
            Path::new(path).file_name().unwrap_or_default().to_string_lossy(),
            l,
            b.bytes() / (1 << 20)
        );
        let mut g = self.banks.lock().unwrap();
        g.push((key, b.clone()));
        while g.len() > MAX_BANKS {
            g.remove(0);
        }
        Ok(b)
    }

    /// The two banks of a Hybrid-Phase or alpha-HP pair: the linear one at
    /// `lin`, lined up as `lin_align` (a file's: `Alignment::Linear`; a
    /// stream's own linear filter looks ahead less, `stream_linear`), and the
    /// minimum-phase one lined up beside it at `min`, each the bank `bank`
    /// builds alone. Where either is not in the cache they are built side by
    /// side — the minimum-phase one on a thread of its own — not one after
    /// the other.
    ///
    /// A plain scoped thread, NOT `rayon::join`: `bank` waits on a condition
    /// variable for a key another caller is building, and chains are built on
    /// rayon workers as well (the power pool). A rayon worker waiting for a
    /// task it handed out runs other tasks meanwhile; one of them asking for a
    /// key that the same worker is building further down its own stack would
    /// wait for ever. A plain thread only waits.
    pub fn pair_banks(&self, lin: &str, lin_align: Alignment, min: &str, l: usize, out_rate: u32) -> Result<(Arc<Bank>, Arc<Bank>), String> {
        let (lin_key, min_key) = (
            bank_key(lin, l, lin_align, out_rate),
            bank_key(min, l, Alignment::BandWeighted, out_rate),
        );
        let cached = {
            let g = self.banks.lock().unwrap();
            [&lin_key, &min_key].iter().all(|key| g.iter().any(|(k, _)| k == *key))
        };
        if cached {
            return Ok((
                self.bank(lin, l, lin_align, out_rate)?,
                self.bank(min, l, Alignment::BandWeighted, out_rate)?,
            ));
        }
        let t0 = std::time::Instant::now();
        let (a, b) = std::thread::scope(|sc| {
            let other = std::thread::Builder::new()
                .name("aura-pair-bank".into())
                .spawn_scoped(sc, || timed_build(min, || self.bank(min, l, Alignment::BandWeighted, out_rate)));
            let a = timed_build(lin, || self.bank(lin, l, lin_align, out_rate));
            let b = match other {
                Ok(h) => h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
                // No thread to be had: one after the other.
                Err(_) => self.bank(min, l, Alignment::BandWeighted, out_rate),
            };
            (a, b)
        });
        let (a, b) = (a?, b?);
        crate::aelog!(
            "[PLAYER] HP pair: both banks in {:.0} ms, built side by side",
            t0.elapsed().as_secs_f64() * 1e3
        );
        Ok((a, b))
    }

    pub(super) fn xtc_pair(&self, s: &PlayerSettings, out_rate: u32) -> Result<Arc<(Vec<f64>, Vec<f64>)>, String> {
        let g = s.engine_geometry();
        let key = format!("{:?}|{}", s.xtc_geometry, out_rate);
        if let Some(p) = self.xtc.lock().unwrap().get(&key) {
            return Ok(p.clone());
        }
        use crate::audio::converter::dsp::lab::xtc;
        let geo = xtc::XtcGeometry {
            speaker_spacing_mm: g.speaker_span_mm,
            listener_distance_mm: g.depth_mm(),
            head_width_mm: g.head_width_mm,
            strength: 1.0,
        };
        let pair = Arc::new(xtc::design_xtc_filters(geo, out_rate)?);
        self.xtc.lock().unwrap().insert(key, pair.clone());
        Ok(pair)
    }

    pub(super) fn guard(&self, out_rate: u32, corner: u32) -> Arc<Vec<f64>> {
        let mut g = self.guards.lock().unwrap();
        g.entry((out_rate, corner))
            .or_insert_with(|| Arc::new(crate::audio::converter::apodize::design_subsonic_highpass(out_rate, corner)))
            .clone()
    }

    /// The Hybrid-Phase onset envelope of a variant. The engine writes it as
    /// a sidecar next to the "source"; the player hands it a path in its own
    /// cache directory, so nothing is ever written beside the user's music.
    pub(super) fn envelope(&self, track_key: &str, v: &Variant) -> Result<Arc<Envelope>, String> {
        let key = format!("{}|{}", track_key, v.key);
        if let Some(e) = self.envs.lock().unwrap().get(&key) {
            return Ok(e.clone());
        }
        // One computation a key: the Hybrid-Phase job of a track that started
        // while its prewarm was making the envelope made the same one beside
        // it — twice the work, at the start, and both wrote the same file. It
        // waits for the first one now, and takes what it made.
        let turn = self.env_turns.lock().unwrap().entry(key.clone()).or_default().clone();
        let _turn = turn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = self.envs.lock().unwrap().get(&key) {
            return Ok(e.clone());
        }
        // On disk the file is named after the source file (path, size,
        // modification time) and the variant, not after this session's track
        // number, so the next session finds it. generate_and_save reuses it
        // only when the detector version and the audio fingerprint inside it
        // match; anything else is computed again.
        let disk_key = if v.source_id.is_empty() { key.clone() } else { format!("{}|{}", v.source_id, v.key) };
        let stem = format!("{:016x}", fnv(&disk_key));
        let fake = self.cache_dir.join(format!("{}.flac", stem));
        let sidecar = envelope_sidecar(&self.cache_dir, &disk_key);
        // Read back from a file an earlier session wrote, or computed now:
        // only a computed one teaches the arming bar what it takes.
        let computed = !sidecar.exists();
        let t_env = std::time::Instant::now();
        let cancel = AtomicBool::new(false);
        let load = || -> Result<(Vec<f64>, f64), String> {
            crate::audio::hpss_native::generate_and_save(&fake, &v.src.l, &v.src.r, v.src.rate, &cancel)?;
            crate::audio::hybrid_phase::load_analysis_envelope(&fake)
                .ok_or_else(|| "onset envelope could not be read back".to_string())
        };
        let loaded = match load() {
            Ok(e) => Ok(e),
            // A file cut short (a crash while writing it) still carries the
            // version and the fingerprint at its head and passes the reuse
            // check, then fails to parse: drop it and compute afresh.
            Err(_) => {
                let _ = std::fs::remove_file(&sidecar);
                load()
            }
        };
        if computed && loaded.is_ok() {
            super::arming::learned_env(v.src.rate, v.src.len(), t_env.elapsed().as_secs_f64());
        }
        let (analysis, analysis_sr) = loaded?;
        // Used now: the age limit of prune_envelope_cache counts from here.
        let _ = std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        let e = Arc::new(Envelope { analysis, analysis_sr });
        self.envs.lock().unwrap().insert(key, e.clone());
        Ok(e)
    }

    /// The onset envelope under `key` (track key | variant key) is in memory.
    pub fn envelope_known(&self, key: &str) -> bool {
        self.envs.lock().unwrap().contains_key(key)
    }

    pub(super) fn plan(&self, track_key: &str, v: &Variant, env: &Envelope) -> Arc<Plan> {
        let key = format!("{}|{}|{}", track_key, v.key, v.out_rate);
        if let Some(p) = self.plans.lock().unwrap().get(&key) {
            return p.clone();
        }
        let total = v.src.len() * v.l;
        let p = Arc::new(blend::scan_plan(env, v.out_rate as f64, total));
        self.plans.lock().unwrap().insert(key, p.clone());
        p
    }

    // analytics: §2.4 — peek helpers (try_lock, never load, never reorder LRU)

    /// Peek a loaded bank without LRU reorder or disk I/O. Returns `None` on
    /// miss or lock contention.
    pub fn peek_bank(&self, path: &Path, l: usize, align: Alignment, out_rate: u32) -> Option<Arc<Bank>> {
        let key = bank_key(&path.display().to_string(), l, align, out_rate);
        let g = self.banks.try_lock().ok()?;
        g.iter().find(|(k, _)| *k == key).map(|(_, b)| b.clone())
    }

    /// Peek the subsonic guard taps without computing them. Returns `None` on
    /// miss or lock contention.
    pub fn peek_guard(&self, rate: u32, hz: u32) -> Option<Arc<[f64]>> {
        let g = self.guards.try_lock().ok()?;
        g.get(&(rate, hz)).map(|v| {
            let s: Arc<[f64]> = Arc::from(v.as_slice());
            s
        })
    }

    /// Peek the XTC filter pair without computing it. Returns `None` on miss
    /// or lock contention.
    pub fn peek_xtc(
        &self,
        settings: &PlayerSettings,
        rate: u32,
    ) -> Option<Arc<(Vec<f64>, Vec<f64>)>> {
        let key = format!("{:?}|{}", settings.xtc_geometry, rate);
        let g = self.xtc.try_lock().ok()?;
        g.get(&key).cloned()
    }

    /// Drop everything tied to tracks other than `keep` (variants live in
    /// the controller; envelopes and plans are keyed by track here). The
    /// controller calls it where it trims its variants, so a long playlist
    /// with Hybrid-Phase does not keep every track's envelope and plan.
    pub fn forget_tracks_except(&self, keep: &[String]) {
        // Keys are "<track key>|…"; match up to the separator, so one track
        // key that happens to prefix another cannot keep the other alive.
        let ok = |k: &String| keep.iter().any(|t| {
            k.len() > t.len() && k.starts_with(t.as_str()) && k.as_bytes()[t.len()] == b'|'
        });
        self.envs.lock().unwrap().retain(|k, _| ok(k));
        self.env_turns.lock().unwrap().retain(|k, _| ok(k));
        self.plans.lock().unwrap().retain(|k, _| ok(k));
        self.out_peaks.lock().unwrap().retain(|k, _| ok(k));
        self.quick_hints.lock().unwrap().retain(|k, _| ok(k));
        self.clip_rails.lock().unwrap().retain(|k, _| ok(k));
    }

    /// Tests elsewhere in the player: cache an envelope and a plan for a track.
    #[cfg(test)]
    pub(crate) fn test_cache_track(&self, track_key: &str) {
        let env = Arc::new(Envelope { analysis: vec![0.0; 4], analysis_sr: 100.0 });
        let plan = Arc::new(Plan { boundaries: vec![], use_min_0: false });
        self.envs.lock().unwrap().insert(format!("{track_key}|full"), env);
        self.plans.lock().unwrap().insert(format!("{track_key}|full|352800"), plan);
    }

    /// Tests elsewhere in the player: `v`'s onset envelope as a file on
    /// disk, as an earlier session left it (hp_ready looks no further than
    /// its name). Its path, to remove.
    #[cfg(test)]
    pub(crate) fn test_envelope_on_disk(&self, v: &Variant) -> PathBuf {
        let path = envelope_sidecar(&self.cache_dir, &format!("{}|{}", v.source_id, v.key));
        std::fs::write(&path, b"").unwrap();
        path
    }

    /// Tests elsewhere in the player: the track keys that still hold an
    /// envelope or a plan, sorted.
    #[cfg(test)]
    pub(crate) fn test_cached_tracks(&self) -> Vec<String> {
        let mut t: Vec<String> = self.envs.lock().unwrap().keys()
            .chain(self.plans.lock().unwrap().keys())
            .map(|k| k.split('|').next().unwrap_or_default().to_string())
            .collect();
        t.sort();
        t.dedup();
        t
    }
}

/// Hybrid-Phase envelope files the player leaves in its cache directory: one
/// per track and source variant (`Resources::envelope`), never removed by the
/// session that wrote them.
const ENVELOPE_SUFFIX: &str = ".onset_envelope.json";

/// Trim the player's envelope files: those older than `max_age` go, then the
/// oldest of the rest until they add up to at most `max_bytes`. Only files
/// directly in `dir` whose names end in `.onset_envelope.json` are touched —
/// nothing else in that folder, and no subfolder. Returns the number of files
/// removed and the bytes freed. A file that cannot be read or removed is
/// skipped, never an error: this is housekeeping.
pub fn prune_envelope_cache(
    dir: &Path,
    max_age: std::time::Duration,
    max_bytes: u64,
    now: std::time::SystemTime,
) -> (usize, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(ENVELOPE_SUFFIX))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((e.path(), m.modified().ok()?, m.len()))
        })
        .collect();
    let mut removed = 0usize;
    let mut freed = 0u64;
    let mut drop_file = |p: &Path, len: u64| {
        if std::fs::remove_file(p).is_ok() {
            removed += 1;
            freed += len;
            true
        } else {
            false
        }
    };
    // By age.
    files.retain(|(p, modified, len)| {
        let old = now.duration_since(*modified).map_or(false, |age| age > max_age);
        !(old && drop_file(p, *len))
    });
    // By size: oldest first.
    files.sort_by_key(|(_, modified, _)| *modified);
    let mut total: u64 = files.iter().map(|(_, _, len)| *len).sum();
    for (p, _, len) in &files {
        if total <= max_bytes {
            break;
        }
        if drop_file(p, *len) {
            total -= *len;
        }
    }
    (removed, freed)
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Returns true when the HP onset envelope for this track and variant is
/// available without blocking: already cached in memory, or written to disk
/// from a previous session. Does not load or compute anything.
pub fn hp_ready(res: &Resources, track_key: &str, v: &Variant) -> bool {
    let key = format!("{}|{}", track_key, v.key);
    if res.envs.lock().unwrap().contains_key(&key) {
        return true;
    }
    if v.source_id.is_empty() {
        return false;
    }
    let disk_key = format!("{}|{}", v.source_id, v.key);
    let stem = format!("{:016x}", fnv(&disk_key));
    res.cache_dir.join(format!("{}{}", stem, ENVELOPE_SUFFIX)).exists()
}

/// Information about an automatic tap-count downgrade that was applied when
/// building this chain.
#[derive(Clone, Debug)]
pub struct DowngradeInfo {
    /// Original taps label (e.g. "30M").
    pub from: String,
    /// Reduced taps label (e.g. "10M").
    pub to: String,
    /// "gpu-failed" when triggered by a mid-track GPU failure, "power" for
    /// a build-time RTF downgrade.
    pub reason: String,
    /// The size the rack asked for: `from` of the first step down, however
    /// many followed. A second step (K3 after the build's own) comes down
    /// from the size heard, and the taps chip took that for the rack's and
    /// said "switching" to the end of the track.
    pub asked: String,
}

impl DowngradeInfo {
    /// A step down from `from` to `to`, below the chain that plays now
    /// (`prev`: its own step down, if it was one to `from`).
    pub fn below(prev: Option<&DowngradeInfo>, from: String, to: String, reason: &str) -> DowngradeInfo {
        let asked = match prev {
            Some(p) if p.to == from => p.asked.clone(),
            _ => from.clone(),
        };
        DowngradeInfo { from, to, reason: reason.to_string(), asked }
    }
}

/// A chain ready to render, positioned at `start` (output index).
pub struct Chain {
    pub stage: Box<dyn Stage>,
    pub track_id: u64,
    pub out_rate: u32,
    pub l: usize,
    pub start: u64,
    pub tokens: Vec<String>,
    pub gain: f64,
    pub tp_pred_db: f64,
    pub direct: bool,
    /// True when this chain plays a pre-converted file from disk (source = "file").
    pub disk_src: bool,
    /// The file a disk chain plays (its full path), None otherwise.
    pub disk_file: Option<String>,
    pub variant_quick: bool,
    /// One line per source-stage outcome (what declip, ISP, AHR decided).
    pub notes: Vec<String>,
    /// The chain as the converter's row lists it: `(token, state, why)` for
    /// every stage that is on, in pipeline order. Empty for Direct and for a
    /// file played from disk (that file's own record has its chain).
    pub stages: Vec<(String, u8, String)>,
    /// True when any convolver stage in this chain ran on the GPU.
    pub gpu_on: bool,
    /// Set when the controller automatically reduced the tap count from the
    /// requested value (build-time or mid-track GPU failure).
    pub downgrade: Option<DowngradeInfo>,
    /// Value of `Shared.gpu_fallback_gen` at the time this chain was built.
    pub downgrade_gen: u32,
    /// Calibration key for the render thread live update (K2).
    /// Format: `"{taps_label}:{out_rate}"` or `"{taps_label}:{out_rate}:HP"`.
    pub calib_key: String,
    /// True when this chain plays linear phase as a stand-in for HP/αHP
    /// while the onset envelope is computed in the background.
    pub hp_deferred: bool,
    // analytics: effective settings this chain was built with (§2.4)
    pub settings: Arc<PlayerSettings>,
    /// The variant it plays: the analyzer reads the source of what is heard.
    pub variant: std::sync::Weak<Variant>,
    /// A slow gain's level now (f64 bits), where it plays one in place of
    /// the scalar `gain` (`slow_gain::SlowGainStage`).
    pub live_gain: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// The radio session whose stream it plays: the analyzer reads the
    /// stream as it came (the shadow) and its songs there. None for a track.
    pub radio: std::sync::Weak<super::radio::RadioShared>,
}

impl Chain {
    #[cfg(test)]
    pub fn describe(&self) -> String {
        self.tokens.join(" · ")
    }
}

/// Pass-through for Direct mode.
struct PassStage {
    src: Arc<SourceBuf>,
    pos: u64,
}

impl Stage for PassStage {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let len = self.src.len() as u64;
        let mut inside = 0;
        for i in 0..n {
            let p = self.pos + i as u64;
            if p < len {
                out_l[i] = self.src.l[p as usize];
                out_r[i] = self.src.r[p as usize];
                inside += 1;
            } else {
                out_l[i] = 0.0;
                out_r[i] = 0.0;
            }
        }
        self.pos += n as u64;
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.src.len() as u64
    }
}

/// A disk chain's stage: the converted file's samples as they are, as
/// `PassStage` passes a decoded track.
struct DiskStage {
    src: Arc<DiskSrc>,
    pos: u64,
}

impl Stage for DiskStage {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let from = self.pos.min(self.src.len() as u64) as usize;
        let inside = self.src.planes.read(from, &mut out_l[..n], &mut out_r[..n]);
        out_l[inside..n].fill(0.0);
        out_r[inside..n].fill(0.0);
        self.pos += n as u64;
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.src.len() as u64
    }
}

/// Try to build a single convolver stage on the GPU; fall back to CPU on
/// any failure.  Returns the stage and whether the GPU was used. A CPU
/// stream of a Hybrid-Phase pair shares its input spectra with the other
/// (`share`); a GPU stream keeps its own ring on the card.
fn make_conv_stage(
    bank:    Arc<Bank>,
    input:   Input,
    start:   u64,
    gpu_ctx: Option<Arc<GpuPolyCtx>>,
    share:   Option<Arc<SpectrumShare>>,
) -> (Box<dyn Stage>, bool) {
    if let Some(ctx) = gpu_ctx {
        if let Some(g) = GpuPolyStream::try_with_input(bank.clone(), input.clone(), start, ctx) {
            return (Box::new(g), true);
        }
    }
    (Box::new(PolyStream::with_input_shared(bank, input, start, share)), false)
}

/// A Hybrid-Phase pair whose envelope is not made yet is built at once, its
/// plan made as it plays (`blend::PlanFeed`), when its banks are loaded
/// already or it is expected to build within this; a cold large pair (30M on
/// the video card: ~4 s) leaves the first sound to linear phase and comes in
/// when it is ready, as it always did.
const PAIR_NOW_S: f64 = 1.0;

/// Instant start off: the pair is built always — nothing is heard before
/// the whole chain.
static PAIR_ALWAYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The player's Instant start tick, for the pair's rule (`PAIR_NOW_S`).
pub fn set_pair_always(on: bool) {
    PAIR_ALWAYS.store(on, std::sync::atomic::Ordering::Release);
}

/// A Hybrid-Phase pair for rack `s` needs no envelope to play from its
/// first sound: Instant start off builds it always, and a pair expected to
/// build within `PAIR_NOW_S` is built at once (its banks loaded already also
/// count, in `plays_pair`).
pub fn pair_without_envelope(s: &PlayerSettings) -> bool {
    PAIR_ALWAYS.load(std::sync::atomic::Ordering::Acquire)
        || super::arming::expected_build_s(s.taps, s.use_gpu, true) <= PAIR_NOW_S
}

/// Hybrid-Phase or alpha-HP on `v` plays its pair now: its envelope is
/// ready, or the pair can be built now with the plan made as it plays.
/// False for any other phase, and where linear phase stands in for the pair
/// until it comes (`hp_deferred`).
pub fn plays_pair(res: &Resources, track_key: &str, v: &Variant, s: &PlayerSettings) -> bool {
    if !matches!(s.phase, Phase::Hybrid | Phase::Alpha) {
        return false;
    }
    if hp_ready(res, track_key, v) || pair_without_envelope(s) {
        return true;
    }
    let src = v.out_rate / v.l.max(1) as u32;
    let find = |phase: &str| crate::audio::converter::dsp::filter::find_precomputed_filter(s.taps, src, v.out_rate, phase);
    let loaded = match (find("linear_phase"), find("minimum_phase")) {
        (Some(a), Some(b)) => {
            res.peek_bank(Path::new(&a), v.l, Alignment::Linear, v.out_rate).is_some()
                && res.peek_bank(Path::new(&b), v.l, Alignment::BandWeighted, v.out_rate).is_some()
        }
        _ => false,
    };
    loaded
}

/// The output ceiling a chain holds, dBTP.
fn ceiling_db(v: &Variant, s: &PlayerSettings) -> f64 {
    if s.headroom_db < 0.0 { v.tp_target_dbtp } else { -0.5 }
}

/// The same ceiling, linear.
pub fn ceiling_lin(v: &Variant, s: &PlayerSettings) -> f64 {
    10f64.powf(ceiling_db(v, s) / 20.0)
}

/// The scalar a chain plays at, by the converter's rule: nothing when the
/// peak is under the ceiling, nothing when the output limiter holds the
/// overs locally (and they are within its 6 dB), otherwise the whole track
/// down to the ceiling.
fn level_gain(pred_lin: f64, local: bool, target_db: f64, use_lim: bool) -> f64 {
    let over_db = 20.0 * pred_lin.max(1e-12).log10() - target_db;
    if over_db <= 0.0 || (use_lim && local && over_db <= 6.0) {
        1.0
    } else {
        10f64.powf(target_db / 20.0) / pred_lin
    }
}

/// The convolver stage(s) for the rack's phase at `v`'s rate, positioned at
/// `at`: the stage, whether any of it is on the GPU, and whether a
/// Hybrid-Phase rack stands in linear phase (its envelope is not ready, or
/// `hp_allowed` is false). `probe` takes the banks from the peak probe's own
/// cache, so its short filters never push a real bank out.
#[allow(clippy::too_many_arguments)]
fn phase_stage(
    res: &Resources,
    probe: bool,
    track_key: &str,
    v: &Variant,
    s: &PlayerSettings,
    at: u64,
    gpu_ctx: Option<Arc<GpuPolyCtx>>,
    hp_allowed: bool,
    tokens: &mut Vec<String>,
) -> Result<(Box<dyn Stage>, bool, bool), String> {
    let rate = v.out_rate;
    // The filter of this variant's factor: a hi-res source's own
    // (`filter::design_rate`).
    let src = rate / v.l.max(1) as u32;
    let cancel = AtomicBool::new(false);
    let find = |phase: &str| {
        crate::audio::converter::dsp::filter::find_precomputed_filter(s.taps, src, rate, phase)
            .ok_or_else(|| crate::audio::converter::dsp::filter::missing_filter_error(s.taps, src, rate, phase))
    };
    let bank = |path: &str, align: Alignment| res.bank_in(probe, path, v.l, align, rate);
    Ok(match s.phase {
        Phase::Linear => {
            let b = bank(&find("linear_phase")?, Alignment::Linear)?;
            let (st, g) = make_conv_stage(b, input_of(v), at, gpu_ctx, None);
            (st, g, false)
        }
        Phase::Minimum => {
            tokens.push("MIN".into());
            let b = bank(&find("minimum_phase")?, Alignment::None)?;
            let (st, g) = make_conv_stage(b, input_of(v), at, gpu_ctx, None);
            (st, g, false)
        }
        Phase::Tfs => {
            tokens.push("TFS".into());
            let p = crate::audio::converter::dsp::lab::tfs::resolve_or_derive(s.taps, src, rate, &cancel)?;
            let ahead = crate::audio::converter::dsp::lab::tfs::look_ahead(s.taps, rate);
            let b = bank(&p.to_string_lossy(), Alignment::LookAhead(ahead))?;
            let (st, g) = make_conv_stage(b, input_of(v), at, gpu_ctx, None);
            (st, g, false)
        }
        Phase::Hybrid | Phase::Alpha => {
            // The pair with its envelope's plan when that is ready; else with
            // the plan made as it plays (the probe's short pair always so);
            // else linear phase stands in, and the caller's background job
            // swaps the pair in when it is ready.
            let ready = hp_allowed && hp_ready(res, track_key, v);
            let live = hp_allowed && !ready && (probe || plays_pair(res, track_key, v, s));
            if !ready && !live {
                let b = bank(&find("linear_phase")?, Alignment::Linear)?;
                let (st, g) = make_conv_stage(b, input_of(v), at, gpu_ctx, None);
                (st, g, true)
            } else {
                let (lin_path, min_path) = (find("linear_phase")?, find("minimum_phase")?);
                // Built side by side (`Resources::pair_banks`); the probe's
                // short pair from its own cache.
                let (lin, min) = if probe {
                    (bank(&lin_path, Alignment::Linear)?, bank(&min_path, Alignment::BandWeighted)?)
                } else {
                    res.pair_banks(&lin_path, Alignment::Linear, &min_path, v.l, rate)?
                };
                let env = if ready { Some(res.envelope(track_key, v)?) } else { None };
                // F5: combined VRAM check for the full HP pair before constructing
                // either stream.  lin's try_new allocates ~1,219 MB; if min's
                // subsequent check then fails the chain is in an undefined half-GPU
                // state.  Gate both on the pair demand up front.
                let hp_gpu_ctx: Option<Arc<GpuPolyCtx>> = if let Some(ref ctx) = gpu_ctx {
                    let pair_demand = vram_demand_hp(lin.full_len, v.l);
                    let free = ctx.free_vram_bytes();
                    if (free as f64 * 0.75) >= pair_demand as f64 {
                        gpu_ctx.clone()
                    } else {
                        None
                    }
                } else {
                    None
                };
                let input = input_of(v);
                let share = Some(SpectrumShare::new());
                if s.phase == Phase::Hybrid {
                    tokens.push("HP".into());
                    let s0 = at.saturating_sub(blend::lead_in(rate as f64));
                    let (a, ga) = make_conv_stage(lin, input.clone(), s0, hp_gpu_ctx.clone(), share.clone());
                    let (b, gb) = make_conv_stage(min, input.clone(), s0, hp_gpu_ctx, share);
                    let st: Box<dyn Stage> = match &env {
                        Some(env) => Box::new(HybridStage::new(a, b, &res.plan(track_key, v, env), rate as f64, at)),
                        None => {
                            let feed = PlanFeed::new(input, v.src.rate, rate as f64, s0 as usize);
                            Box::new(HybridStage::live(a, b, feed, rate as f64, at))
                        }
                    };
                    (st, ga || gb, false)
                } else {
                    tokens.push("aHP".into());
                    let (a, ga) = make_conv_stage(lin, input.clone(), at, hp_gpu_ctx.clone(), share.clone());
                    let (b, gb) = make_conv_stage(min, input.clone(), at, hp_gpu_ctx, share);
                    let st: Box<dyn Stage> = match &env {
                        Some(env) => Box::new(AlphaStage::new(a, b, env, rate as f64)),
                        None => Box::new(AlphaStage::live(a, b, PlanFeed::new(input, v.src.rate, rate as f64, at as usize))),
                    };
                    (st, ga || gb, false)
                }
            }
        }
    })
}

/// What the converter's output stage will find on the finished render: its
/// true peak (linear, before any gain) and whether the output limiter may
/// hold its overs locally (`isp::limit_output`'s own rule).
#[derive(Clone, Copy, Debug)]
pub struct OutPeak {
    pub tp_lin: f64,
    pub local: bool,
}

/// The probe's filter: the same phase, 5k taps.
const PROBE_TAPS: usize = 5_000;

/// `OutPeak` for a chain before it plays, from a short render of the same
/// chain: the rack's phase (the Hybrid-Phase blend when `hp_on`) with a 5k
/// filter, XTC, at FS×2 (FS×4 for ×16), no gain, limiter or guard.
/// Its 4× view lands on the rack's output grid, where the converter looks for
/// overs. The source's own 4× peaks — the estimate before this — miss what a
/// phase or XTC adds: one track with TFS and XTC measured +1.17 dBTP on the
/// source and +1.80 on the render, and played live 0.64 dB louder than its
/// converted file; the probe finds +1.79. Cached per track, variant, phase,
/// XTC and ceiling (and the file itself: its path, size and time). None for
/// a quick or direct variant, or when the probe
/// cannot be built (a filter missing).
pub fn output_peak(
    res: &Resources,
    track_key: &str,
    v: &Variant,
    s: &PlayerSettings,
    hp_on: bool,
    target_lin: f64,
) -> Option<OutPeak> {
    use crate::audio::converter::dsp::{lab::isp, true_peak};
    if v.quick || v.direct || v.src.len() == 0 {
        return None;
    }
    let hp = hp_on && matches!(s.phase, Phase::Hybrid | Phase::Alpha);
    let xtc_key = if s.xtc_active() { format!("{:?}", s.xtc_geometry) } else { "-".into() };
    let key = format!("{}|{}|{}|{:?}|{}|{}|{:.6}", track_key, v.source_id, v.key, s.phase, hp, xtc_key, target_lin);
    if let Some(p) = res.out_peaks.lock().unwrap().get(&key) {
        return Some(*p);
    }
    let t0 = std::time::Instant::now();
    let l = v.l.max(1);
    // FS×2 for a rack at ×2, ×4 and ×8, ×4 for ×16: the scan then looks at
    // every 4× point (×8, ×16), every other (×4) or the samples (×2).
    let pl = if l <= 2 { l } else { (l / 4).max(2) };
    let k = l / pl;
    let rate = v.src.rate * pl as u32;
    // A full variant still to be (Instant start off: `prepare_full_variant`)
    // is probed on its stream as it grows.
    let pv = Variant {
        stream: v.stream.clone(),
        key: v.key.clone(),
        src: v.src.clone(),
        out_rate: rate,
        l: pl,
        tp_target_dbtp: v.tp_target_dbtp,
        tp_pred_lin: v.tp_pred_lin,
        lim_local: v.lim_local,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct: false,
        source_id: v.source_id.clone(),
    };
    let ps = PlayerSettings { taps: PROBE_TAPS, ..s.clone() };
    let xtc = if s.xtc_active() { Some(res.xtc_pair(&ps, rate).ok()?) } else { None };
    let n = pv.src.len() * pl;
    // Pieces of two million frames (64 MB in flight each), taken in turn by
    // up to half the cores, eight at most. Each renders a few samples past
    // its ends, so the 4× view inside it has its neighbours, and scans only
    // its own part.
    const PIECE: usize = 1 << 21;
    const MARGIN: usize = 8;
    let pieces = n.div_ceil(PIECE);
    let workers = (std::thread::available_parallelism().map_or(4, |x| x.get()) / 2).clamp(1, 8).min(pieces);
    let scan_rate = rate * k as u32;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let piece = |c: usize| -> Option<(f64, Vec<(usize, usize, f64)>)> {
        let a = c * PIECE;
        let b = ((c + 1) * PIECE).min(n);
        let from = a.saturating_sub(MARGIN);
        let to = (b + MARGIN).min(n);
        let at = match &xtc {
            Some(p) => from as u64 - XtcStage::history(p.0.len(), from as u64),
            None => from as u64,
        };
        let mut tokens = Vec::new();
        let (mut st, _, _) = phase_stage(res, true, track_key, &pv, &ps, at, None, hp, &mut tokens).ok()?;
        if let Some(p) = &xtc {
            st = Box::new(XtcStage::new(st, &p.0, &p.1, from as u64));
        }
        skip_to(st.as_mut(), from as u64);
        let m = to - from;
        let mut bl = vec![0.0f64; m];
        let mut br = vec![0.0f64; m];
        let mut done = 0;
        while done < m {
            let e = (done + (1 << 16)).min(m);
            st.read(&mut bl[done..e], &mut br[done..e]);
            done = e;
        }
        // The piece's own samples, in 4× points and on the scan grid.
        let (lo4, hi4) = ((a - from) * 4, (b - from) * 4);
        let mut tp = 0.0f64;
        let mut scan = isp::OverScan::new(target_lin, scan_rate);
        let mut idx = 0usize;
        true_peak::for_each_4x_block(&bl, &br, 1 << 16, |x, y| {
            let (j0, j1) = (idx, idx + x.len());
            let s0 = j0.max(lo4).min(j1);
            let s1 = j1.min(hi4).max(s0);
            for i in s0..s1 {
                tp = tp.max(x[i - j0].abs().max(y[i - j0].abs()));
            }
            if k == 4 && s1 > s0 {
                scan.push(&x[s0 - j0..s1 - j0], &y[s0 - j0..s1 - j0]);
            } else if k == 2 && s1 > s0 {
                // The 2× grid: the even 4× points (the piece starts on a
                // sample, so its parity is the track's).
                let (mut a2, mut b2) = (Vec::with_capacity((s1 - s0) / 2 + 1), Vec::with_capacity((s1 - s0) / 2 + 1));
                for i in (s0 + (s0 & 1)..s1).step_by(2) {
                    a2.push(x[i - j0]);
                    b2.push(y[i - j0]);
                }
                scan.push(&a2, &b2);
            }
            idx = j1;
        });
        if k == 1 {
            scan.push(&bl[a - from..b - from], &br[a - from..b - from]);
        }
        let (clusters, _) = scan.finish();
        let off = a * k;
        Some((tp, clusters.into_iter().map(|(s0, s1, p)| (s0 + off, s1 + off, p)).collect()))
    };
    let mut parts: Vec<(usize, Option<(f64, Vec<(usize, usize, f64)>)>)> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                sc.spawn(|| {
                    let mut got = Vec::new();
                    loop {
                        let c = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if c >= pieces {
                            break got;
                        }
                        got.push((c, piece(c)));
                    }
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    if parts.len() != pieces {
        return None;
    }
    parts.sort_by_key(|p| p.0);
    let mut tp = 0.0f64;
    let mut all = Vec::with_capacity(pieces);
    for (_, p) in parts {
        let (t, c) = p?;
        tp = tp.max(t);
        all.push(c);
    }
    let clusters = isp::join_cluster_runs(all, scan_rate);
    let local = isp::plan_output_limit(clusters, n * k, target_lin, scan_rate)
        .map_or(true, |p| p.report.fell_back.is_none());
    let out = OutPeak { tp_lin: tp, local };
    super::arming::learned_probe(v.src.rate, v.src.len(), t0.elapsed().as_secs_f64());
    crate::aelog!(
        "[PLAYER] output peak {:+.2} dBTP (source {:+.2}), overs {} — 5k probe ×{} in {:.2} s",
        20.0 * tp.max(1e-12).log10(),
        20.0 * v.tp_pred_lin.max(1e-12).log10(),
        if local { "held locally" } else { "take the whole track down" },
        pl,
        t0.elapsed().as_secs_f64()
    );
    res.out_peaks.lock().unwrap().insert(key, out);
    Some(out)
}

/// The quick-hint key: the track, the file, the full rack's source key, its
/// phase and XTC — what the full chain's peak depends on, less the
/// Hybrid-Phase envelope's readiness and the ceiling.
fn quick_hint_key(track_key: &str, v: &Variant, s: &PlayerSettings) -> String {
    let xtc_key = if s.xtc_active() { format!("{:?}", s.xtc_geometry) } else { "-".into() };
    format!("{}|{}|{}|{:?}|{}", track_key, v.source_id, s.source_key(), s.phase, xtc_key)
}

/// Declip's repair lifts a clipped master's peaks far over its rail: the
/// restored arcs land around 2.3× (`declip::CEILING_OVER_RAIL`'s note; Rich
/// Girl, 1.2 % clipped at 0 dBFS, measured +7.05 dBTP after the source stages).
const QUICK_DECLIP_OVER_RAIL: f64 = 2.3;
/// The other source stages the quick variant skips (ISP, SUB, AA) lift the
/// peaks a little too: Mal Bicho +1.35 → +2.58 dBTP.
const QUICK_STAGES_DB: f64 = 1.0;
/// XTC's cancellation filters add to the peaks (+3.6 dB on a measured track).
const QUICK_XTC_DB: f64 = 3.5;
/// A phase other than linear moves the peaks too (+0.6 dB measured with TFS).
const QUICK_PHASE_DB: f64 = 0.7;

/// The quick variant's output peak, which its own audio does not tell: it
/// has no declip, and no probe runs for it. The full chain's probe of the
/// same track and rack when one has run (a replay, a seek, a rack switched
/// back); else a guess on the high side — the declip rail, XTC and the
/// phase allowed for, and no local hold for the limiter — so that the
/// quick variant plays at or under the full one's level, never over it:
/// the full one then comes in with a glide up (`build_chain_glide`), not a
/// step down. Anton 26.09: the first seconds were louder, by up to 8 dB on a
/// clipped master with DC on.
fn quick_peak(res: &Resources, track_key: &str, v: &Variant, s: &PlayerSettings) -> OutPeak {
    if let Some(p) = res.quick_hints.lock().unwrap().get(&quick_hint_key(track_key, v, s)) {
        return *p;
    }
    let mut tp = v.tp_pred_lin;
    if s.isp || s.subsonic_hz != 0 || s.adaptive_apodizer {
        tp *= 10f64.powf(QUICK_STAGES_DB / 20.0);
    }
    if s.declip {
        let key = format!("{}|{}", track_key, v.source_id);
        let cached = res.clip_rails.lock().unwrap().get(&key).copied();
        let rail = match cached {
            Some(r) => r,
            None => {
                // A lossy source gets no declip (its peaks are the codec's):
                // no repaired rail to allow for. The codec from the file's
                // header, by the path its source id starts with.
                let path = v.source_id.rsplitn(3, '|').nth(2);
                let lossy = path.and_then(|p| crate::audio::converter::decode::probe_lossy_codec(Path::new(p)));
                let r = match lossy {
                    Some(_) => None,
                    None => crate::audio::converter::dsp::lab::declip::plateau_rail(&v.src.l, &v.src.r),
                };
                res.clip_rails.lock().unwrap().insert(key, r);
                r
            }
        };
        if let Some(rail) = rail {
            tp = tp.max(rail * QUICK_DECLIP_OVER_RAIL);
        }
    }
    if s.xtc_active() {
        tp *= 10f64.powf(QUICK_XTC_DB / 20.0);
    }
    if !matches!(s.phase, Phase::Linear) {
        tp *= 10f64.powf(QUICK_PHASE_DB / 20.0);
    }
    OutPeak { tp_lin: tp, local: false }
}

/// A quick chain's level hands over in this long a glide (seconds).
pub const QUICK_GLIDE_S: f64 = 1.5;

pub fn build_chain(
    res:          &Resources,
    track_id:     u64,
    track_key:    &str,
    v:            &Arc<Variant>,
    s:            &PlayerSettings,
    start:        u64,
    gpu_ctx:      Option<Arc<GpuPolyCtx>>,
    downgrade:    Option<DowngradeInfo>,
    downgrade_gen: u32,
) -> Result<Chain, String> {
    build_chain_glide(res, track_id, track_key, v, s, start, gpu_ctx, downgrade, downgrade_gen, None)
}

/// `build_chain`, its level gliding in from `glide_from` (the gain of the
/// quick chain it replaces) over `QUICK_GLIDE_S` — ahead of the output
/// limiter, which holds the peaks at the ceiling all the way.
#[allow(clippy::too_many_arguments)]
pub fn build_chain_glide(
    res:          &Resources,
    track_id:     u64,
    track_key:    &str,
    v:            &Arc<Variant>,
    s:            &PlayerSettings,
    start:        u64,
    gpu_ctx:      Option<Arc<GpuPolyCtx>>,
    downgrade:    Option<DowngradeInfo>,
    downgrade_gen: u32,
    glide_from:   Option<f64>,
) -> Result<Chain, String> {
    let total = (v.src.len() * v.l) as u64;
    let start = start.min(total);
    if v.direct {
        return Ok(Chain {
            live_gain: None,
            stage: Box::new(PassStage { src: v.src.clone(), pos: start }),
            track_id,
            out_rate: v.out_rate,
            l: 1,
            start,
            tokens: v.tokens.clone(),
            gain: 1.0,
            tp_pred_db: 0.0,
            direct: true,
            disk_src: false,
            disk_file: None,
            variant_quick: false,
            notes: vec![],
            stages: vec![],
            gpu_on: false,
            downgrade,
            downgrade_gen,
            calib_key: String::new(),
            hp_deferred: false,
            // analytics: §2.4
            settings: Arc::new(s.clone()),
            variant: Arc::downgrade(v),
            radio: std::sync::Weak::new(),
        });
    }

    // What the build takes, less a probe it runs on the way (arming.rs).
    let t_build = std::time::Instant::now();
    let probes_before = super::arming::probe_secs_here();
    let rate = v.out_rate;
    let use_guard = s.subsonic_hz != 0;
    let use_lim = s.isp;
    let use_xtc = s.xtc_active();

    // Positions from the output back to the convolver.
    let guard = if use_guard { Some(res.guard(rate, s.subsonic_hz)) } else { None };
    let s_guard_in = match &guard {
        Some(g) => start.saturating_sub(FirStage::history(g.len())),
        None => start,
    };
    let s_lim_in = if use_lim { s_guard_in.saturating_sub(LimiterStage::history(rate)) } else { s_guard_in };
    let xtc = if use_xtc { Some(res.xtc_pair(s, rate)?) } else { None };
    let s_xtc_in = match &xtc {
        Some(p) => s_lim_in - XtcStage::history(p.0.len(), s_lim_in),
        None => s_lim_in,
    };

    // The filter(s).
    let taps_label = crate::audio::converter::dsp::filter::taps_label(s.taps).unwrap_or("?");
    let mut tokens = v.tokens.clone();
    tokens.push(taps_label.to_string());
    // The player always renders the polyphase path (PFR = Polyphase FIR ran).
    // This makes the PFR badge always fire in the player strip, matching the
    // spec note that "the player always renders the polyphase path".
    tokens.push("PFR".into());
    let (phase_stage, any_gpu, hp_deferred) =
        phase_stage(res, false, track_key, v, s, s_xtc_in, gpu_ctx, true, &mut tokens)?;

    let mut stage = phase_stage;
    if let Some(p) = &xtc {
        tokens.push("XTC".into());
        stage = Box::new(XtcStage::new(stage, &p.0, &p.1, s_lim_in));
    }
    let target_db = ceiling_db(v, s);
    let target_lin = 10f64.powf(target_db / 20.0);

    // True-peak gain, the converter's behaviour before the render exists.
    // The converter measures its finished render; the player measures a
    // short render of the same chain (`output_peak`), and only a quick
    // variant, or a probe that could not run, falls back to the source's own
    // 4× peaks — which miss what a phase or XTC adds to the output's. With
    // the output limiter on and the overs sparse and within its 6 dB reach,
    // they are held locally and the level stays; otherwise the whole track
    // comes down to the ceiling. The gain goes in front of the limiter,
    // which then holds what the estimate missed.
    let probe = if v.stream.is_some() {
        None
    } else if v.quick {
        Some(quick_peak(res, track_key, v, s))
    } else {
        let p = output_peak(res, track_key, v, s, !hp_deferred, target_lin);
        if let Some(p) = p {
            res.quick_hints.lock().unwrap().insert(quick_hint_key(track_key, v, s), p);
        }
        p
    };
    let (pred_lin, local) = probe.map_or((v.tp_pred_lin, v.lim_local), |p| (p.tp_lin, p.local));
    let tp_db = 20.0 * pred_lin.max(1e-12).log10();
    // Instant start's first variant of a file plays at the level the same
    // rule gives the whole decoded track (`prepare_stream_variant`): no
    // render to measure yet, but the track is in hand before the first sound.
    let gain = level_gain(pred_lin, local, target_db, use_lim);
    stage = match glide_from {
        Some(g0) if !v.quick && (20.0 * (g0.max(1e-9) / gain).log10()).abs() > 0.3 => {
            crate::aelog!(
                "[PLAYER] quick → full: the level glides {:+.2} → {:+.2} dB over {:.1} s",
                20.0 * g0.max(1e-9).log10(),
                20.0 * gain.log10(),
                QUICK_GLIDE_S
            );
            Box::new(GainStage::gliding(stage, gain, g0, (QUICK_GLIDE_S * rate as f64) as u64))
        }
        _ => Box::new(GainStage::new(stage, gain)),
    };
    if use_lim {
        tokens.push("ISP-L".into());
        stage = Box::new(LimiterStage::new(stage, target_lin, rate, s_guard_in));
    }
    if let Some(g) = &guard {
        tokens.push(format!("SUB{}-G", s.subsonic_hz));
        stage = Box::new(FirStage::new(stage, g, rate, start));
    }
    // Stages that start a little early leave their position behind `start`.
    skip_to(stage.as_mut(), start);


    // The whole chain as a converted row would list it: the source stages the
    // variant reports, then the conversion side. The output limiter and the
    // subsonic guard are not stages of their own there — the ISP and SUB
    // toggles switch them — so they are told inside those two.
    use crate::audio::converter::dsp::lab::CHAIN_RAN;
    let mut stages = v.stages.clone();
    for st in stages.iter_mut() {
        if st.0 == "ISP" && use_lim {
            let more = if v.stream.is_some() {
                "the level follows the loudest moments ahead until the whole track is measured".to_string()
            } else if gain < 1.0 {
                format!(
                    "output peaks {:+.2} dBTP, too many or too deep to hold locally: the whole track {:+.2} dB to the {:.1} dBTP ceiling, as the converter does",
                    tp_db, 20.0 * gain.log10(), target_db
                )
            } else {
                format!("output limiter holds overs up to 6 dB at the {:.1} dBTP ceiling", target_db)
            };
            st.2 = join_why(&st.2, &more);
        }
        if st.0 == "SUB" && use_guard {
            st.2 = join_why(&st.2, if use_lim {
                "runs again on the finished render, after the output limiter"
            } else {
                "runs again on the finished render"
            });
        }
    }
    stages.push((
        "PFR".into(),
        CHAIN_RAN,
        format!("{} taps ×{}: the FIR is the resampler — one convolution, no second stage", taps_label, v.l),
    ));
    match s.phase {
        Phase::Hybrid if !hp_deferred => stages.push(("HP".into(), CHAIN_RAN,
            "linear phase through sustained passages, minimum phase across attacks, switched at a zero crossing".into())),
        Phase::Alpha if !hp_deferred => {
            stages.push(("HP".into(), CHAIN_RAN, "linear and minimum phase, blended by the attack envelope".into()));
            stages.push(("aHP".into(), CHAIN_RAN, "a per-sample crossfade driven by the HPSS envelope instead of a switch".into()));
        }
        Phase::Tfs => stages.push(("TFS".into(), CHAIN_RAN,
            "linear phase below 1.5 kHz, minimum phase above 4 kHz, blended between".into())),
        Phase::Linear | Phase::Minimum | Phase::Hybrid | Phase::Alpha => {}
    }
    if use_xtc {
        stages.push(("XTC".into(), CHAIN_RAN, "crosstalk cancelled for the measured listening triangle".into()));
    }

    let hp_suffix = if matches!(s.phase, Phase::Hybrid | Phase::Alpha) && !hp_deferred { ":HP" } else { "" };
    let probes = super::arming::probe_secs_here() - probes_before;
    // By the rack's GPU switch, as the bar looks it up (the policy's own
    // choice is made at build time, after the bar has planned).
    super::arming::learned_build(s.taps, s.use_gpu, !hp_suffix.is_empty(), (t_build.elapsed().as_secs_f64() - probes).max(0.0));
    Ok(Chain {
        live_gain: None,
        stage,
        track_id,
        out_rate: rate,
        l: v.l,
        start,
        tokens,
        gain,
        tp_pred_db: tp_db,
        direct: false,
        disk_src: false,
        disk_file: None,
        variant_quick: v.quick,
        notes: v.notes.clone(),
        stages,
        gpu_on: any_gpu,
        downgrade,
        downgrade_gen,
        calib_key: format!("{}:{}{}", taps_label, rate, hp_suffix),
        hp_deferred,
        // analytics: §2.4
        settings: Arc::new(s.clone()),
        variant: Arc::downgrade(v),
        radio: std::sync::Weak::new(),
    })
}

/// The stages a converted file went through, read off its name: the
/// converter writes every stage that fired into the `[AE · …]` tag, in the
/// order they ran (`… · f64 · ISP·SUB15·PFR·TFS·XTC]`). The page shows the
/// conversion's own record when it has one; this is what it shows when it
/// does not (a record from before the chain was kept, or none at all).
pub fn file_stages(name: &str) -> Vec<(String, u8, String)> {
    const HEAD: &str = "[AE \u{b7} ";
    let Some(a) = name.rfind(HEAD) else { return vec![] };
    let Some(len) = name[a..].find(']') else { return vec![] };
    let inner = &name[a + HEAD.len()..a + len];
    let is_tok = |t: &str| {
        matches!(t.chars().next(), Some(ch) if ch.is_ascii_uppercase() || ch == '\u{3b1}')
            && t.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '\u{3b1}' || ch == '-')
    };
    let why = "The converter ran this stage on the file (its name says so).";
    inner
        .split(" \u{b7} ")
        .filter(|part| !part.contains(' ') && part.split('\u{b7}').all(is_tok))
        .flat_map(|part| part.split('\u{b7}').map(|t| (t.to_string(), 1u8, why.to_string())).collect::<Vec<_>>())
        .collect()
}

/// A converted file decoded for a disk chain, at the output rate. Its
/// samples are kept as exact integers when every one is — a 24-bit FLAC,
/// which is what the converter writes: half the memory of f64, each sample
/// read back as the f64 the decoder made (`decode::Planes`).
pub struct DiskSrc {
    pub rate: u32,
    pub planes: crate::audio::converter::decode::Planes,
}

impl DiskSrc {
    pub fn len(&self) -> usize {
        self.planes.len()
    }
}

/// `path`, a converted file, decoded for a disk chain.
pub fn disk_file(path: &Path) -> Result<Arc<DiskSrc>, String> {
    let (planes, rate) = crate::audio::converter::decode::decode_file_pcm(path)?;
    crate::aelog!(
        "[PLAYER] converted file held as {}: {} MB",
        if matches!(planes, crate::audio::converter::decode::Planes::Int { .. }) { "exact integers" } else { "f64" },
        planes.bytes() / (1 << 20)
    );
    Ok(Arc::new(DiskSrc { rate, planes }))
}

/// Build a pass-through chain from a pre-converted file that was decoded by
/// the caller. The file was already processed by the converter at the output
/// rate; we just need to play the raw PCM without any further DSP.
pub fn build_file_chain(
    track_id: u64,
    src: Arc<DiskSrc>,
    start_s: f64,
    path: &std::path::Path,
) -> Chain {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rate = src.rate;
    let total = src.len() as u64;
    let start = ((start_s * rate as f64) as u64).min(total);
    Chain {
        live_gain: None,
        stage: Box::new(DiskStage { src, pos: start }),
        track_id,
        out_rate: rate,
        l: 1,
        start,
        tokens: vec!["FILE".into()],
        gain: 1.0,
        tp_pred_db: 0.0,
        direct: false,
        disk_src: true,
        disk_file: Some(path.display().to_string()),
        variant_quick: false,
        notes: vec![],
        stages: file_stages(&name),
        gpu_on: false,
        downgrade: None,
        downgrade_gen: 0,
        calib_key: String::new(),
        hp_deferred: false,
        // analytics: §2.4 — file chain has no source settings; use default
        settings: Arc::new(PlayerSettings::default()),
        variant: std::sync::Weak::new(),
        radio: std::sync::Weak::new(),
    }
}

#[cfg(test)]
mod file_stage_tests {
    use super::file_stages;

    fn toks(name: &str) -> Vec<String> {
        file_stages(name).into_iter().map(|t| t.0).collect()
    }

    /// A file played from disk shows the stages its name lists, and nothing
    /// from the rate, filter, precision or channel segments.
    #[test]
    fn the_file_name_gives_its_stages() {
        assert_eq!(toks("Ave [AE · 44.1k→352.8k · Kaiser 30M · f64 · ISP·SUB15·PFR·TFS·XTC].flac"), ["ISP", "SUB15", "PFR", "TFS", "XTC"]);
        assert_eq!(toks("03 Mal Bicho [AE · 44.1k→352.8k · Kaiser 10M · f64 · AA·HP].flac"), ["AA", "HP"]);
        assert_eq!(toks("x [AE · 44.1k→384k · Kaiser 10M · f64 · DC·SUB20·AA·PFR·HP · 6ch→2.0].flac"), ["DC", "SUB20", "AA", "PFR", "HP"]);
        assert_eq!(toks("x [AE · 44.1k→352.8k · Kaiser 30M · f64 · αHP].flac"), ["αHP"]);
        assert!(toks("x [AE · 44.1k→352.8k · Kaiser 30M · f64].flac").is_empty());
        assert!(toks("plain.flac").is_empty());
    }
}

#[cfg(test)]
mod cancel_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::audio::cancel_flag;
    use crate::audio::converter::apodize::file_or_global_cancelled;

    /// The converter's cancel stops the converter's work and leaves the
    /// player's alone; a flag of either kind still stops its own work.
    #[test]
    fn a_cancelled_conversion_does_not_stop_the_player() {
        let converter_file = AtomicBool::new(false);
        let player_prep = AtomicBool::new(false);
        let _guard = cancel_flag::self_only(&player_prep);

        cancel_flag::set(true);
        let converter_stops = file_or_global_cancelled(&converter_file);
        let player_stops = file_or_global_cancelled(&player_prep);
        cancel_flag::set(false);

        assert!(converter_stops, "the converter's own work must still stop");
        assert!(!player_stops, "the player's preparation must not");

        player_prep.store(true, Ordering::Relaxed);
        assert!(file_or_global_cancelled(&player_prep), "its own flag still stops it");
    }

    #[test]
    fn the_registration_ends_with_the_guard() {
        let f = AtomicBool::new(false);
        {
            let _g = cancel_flag::self_only(&f);
            assert!(cancel_flag::answers_only_for_itself(&f));
        }
        assert!(!cancel_flag::answers_only_for_itself(&f));
    }
}

/// What the Hybrid-Phase onset envelope costs on the way to the first sound.
/// Heavy (minutes, gigabytes): run by hand with the filters at hand —
///   AURA_FILTER_DIR=…/fir-optimizer/output cargo test --profile fast \
///       hp_envelope_cost -- --ignored --nocapture --test-threads=1
#[cfg(test)]
mod hp_envelope_bench {
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    use super::{build_chain, prepare_variant, Resources};
    use crate::player::settings::{Phase, PlayerSettings};

    /// Something with the shape of music: a few tones, noise, and a drum-like
    /// hit every half second (the envelope's work does not depend on content
    /// much, but a signal with onsets exercises every branch).
    fn music(n: usize, rate: u32, seed: u64) -> Vec<f64> {
        let mut x = seed;
        let beat = (rate / 2) as usize;
        (0..n).map(|i| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((x >> 33) as f64 / u32::MAX as f64 - 0.5) * 0.05;
            let t = i as f64 / rate as f64;
            let tones = 0.1 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()
                + 0.05 * (2.0 * std::f64::consts::PI * 1_760.0 * t).sin();
            let k = i % beat;
            let hit = if k < 2_000 { 0.5 * (-(k as f64) / 300.0).exp() * ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) } else { 0.0 };
            tones + noise + hit
        }).collect()
    }

    fn write_wav16(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
        let n = l.len();
        let data_len = (n * 4) as u32;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&(rate * 4).to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for i in 0..n {
            for s in [l[i], r[i]] {
                let v = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
                f.write_all(&v.to_le_bytes()).unwrap();
            }
        }
    }

    fn ms(t: Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0
    }

    /// What a chain built ahead still costs at its first sound: the first
    /// reads of a cold 30M chain at FS×2 from 48 kHz, a STEP (8 192 frames)
    /// at a time as the render reads, against the reads after. A new stream
    /// waits for 0.3 s of it before the device is opened.
    #[test]
    #[ignore]
    fn cold_chain_first_reads() {
        let dir = tempfile::tempdir().unwrap();
        let rate = 48_000u32;
        let n = (60 * rate) as usize;
        let (l, r) = (music(n, rate, 3), music(n, rate, 4));
        let wav = dir.path().join("cold.wav");
        write_wav16(&wav, rate, &l, &r);
        let res = Resources::new(dir.path().join("cache"));
        for phase in [Phase::Linear, Phase::Hybrid] {
            let s = PlayerSettings { phase, fs_multiplier: 2, taps: 30_000_000, use_gpu: false, ..Default::default() };
            let cancel = AtomicBool::new(false);
            let v = Arc::new(prepare_variant(&wav, &s, false, &cancel).unwrap());
            let tk = format!("t1:{}", wav.display());
            if phase == Phase::Hybrid {
                let env = res.envelope(&tk, &v).unwrap();
                res.plan(&tk, &v, &env);
            }
            let t = Instant::now();
            let mut chain = build_chain(&res, 1, &tk, &v, &s, 0, None, None, 0).unwrap();
            let build_ms = ms(t);
            let step = 8_192;
            let (mut bl, mut br) = (vec![0.0; step], vec![0.0; step]);
            let mut reads = Vec::new();
            for _ in 0..24 {
                let t = Instant::now();
                chain.stage.read(&mut bl, &mut br);
                reads.push(ms(t));
            }
            let first4: f64 = reads[..4].iter().sum();
            let next20: f64 = reads[4..].iter().sum();
            println!(
                "[cold] {:?} out {} Hz: build {:.0} ms (hp deferred {}), first 4 reads {:.0} ms {:?}, next 20 reads {:.0} ms ({:.1} ms each)",
                phase,
                v.out_rate,
                build_ms,
                chain.hp_deferred,
                first4,
                reads[..4].iter().map(|x| x.round() as i64).collect::<Vec<_>>(),
                next20,
                next20 / 20.0
            );
        }
    }

    #[test]
    #[ignore]
    fn hp_envelope_cost() {
        use crate::audio::{hpss_native, hybrid_phase};
        let dir = tempfile::tempdir().unwrap();
        // Anton's playback setting in the field logs: 30M taps, FS2, Hybrid-Phase, CPU.
        let s = PlayerSettings { phase: Phase::Hybrid, fs_multiplier: 2, taps: 30_000_000, ..Default::default() };
        let res = Resources::new(dir.path().join("cache"));
        println!("case         | envelope | reuse(read+check) | quick variant | build HP, envelope fresh | build HP, envelope cached | envelope share of start");
        for (mins, rate) in [(3u32, 44_100u32), (8, 44_100), (38, 44_100), (3, 192_000), (8, 192_000), (38, 192_000)] {
            let n = (mins * 60 * rate) as usize;
            let (l, r) = (music(n, rate, 1), music(n, rate, 2));

            // The envelope on its own, fresh, then the path that reuses the file.
            let fake = dir.path().join(format!("env_{mins}_{rate}.flac"));
            let t = Instant::now();
            hpss_native::generate_and_save(&fake, &l, &r, rate, &AtomicBool::new(false)).unwrap();
            let e_ms = ms(t);
            let t = Instant::now();
            hpss_native::generate_and_save(&fake, &l, &r, rate, &AtomicBool::new(false)).unwrap();
            hybrid_phase::load_analysis_envelope(&fake).expect("envelope reads back");
            let hit_ms = ms(t);

            // The start path: quick variant + chain (banks warm: the first
            // build of each rate loads them and is not counted).
            let big = mins == 38 && rate == 192_000;
            let (q_ms, fresh_ms, cached_ms) = if big {
                (f64::NAN, f64::NAN, f64::NAN)
            } else {
                let wav = dir.path().join(format!("src_{mins}_{rate}.wav"));
                write_wav16(&wav, rate, &l, &r);
                drop((l, r));
                let t = Instant::now();
                let v = Arc::new(prepare_variant(&wav, &s, true, &AtomicBool::new(false)).unwrap());
                let q_ms = ms(t);
                let key = format!("bench:{mins}:{rate}");
                let wipe = || {
                    res.forget_tracks_except(&[]);
                    for e in std::fs::read_dir(dir.path().join("cache")).unwrap().flatten() {
                        let _ = std::fs::remove_file(e.path());
                    }
                };
                build_chain(&res, 1, &key, &v, &s, 0, None, None, 0).unwrap(); // warm banks
                wipe();
                let t = Instant::now();
                build_chain(&res, 1, &key, &v, &s, 0, None, None, 0).unwrap();
                let fresh_ms = ms(t);
                let t = Instant::now();
                build_chain(&res, 1, &key, &v, &s, 0, None, None, 0).unwrap();
                let cached_ms = ms(t);
                wipe();
                let _ = std::fs::remove_file(&wav);
                (q_ms, fresh_ms, cached_ms)
            };
            let share = 100.0 * e_ms / (q_ms + fresh_ms);
            println!(
                "{:>2} min {:>6} | {:>7.0} ms | {:>7.1} ms | {:>9.0} ms | {:>9.0} ms | {:>9.0} ms | {:>5.1} %",
                mins, rate, e_ms, hit_ms, q_ms, fresh_ms, cached_ms, share
            );
        }
    }
}

#[cfg(test)]
mod resource_tests {
    use std::sync::Arc;

    use super::{bank_key, Resources};
    use crate::player::blend::{Envelope, Plan};
    use crate::player::convolver::{Alignment, Bank};

    /// One blob plays at more than one rate — the ×4 blob takes 48 kHz to
    /// 192 kHz and 96 kHz to 384 kHz — and a band-weighted bank's delay is
    /// measured over 200 Hz – 6 kHz where it plays, a different stretch of the
    /// filter at each rate: each rate keeps a bank of its own. The other
    /// alignments do not depend on the rate and share one.
    #[test]
    fn a_band_weighted_bank_is_kept_per_rate() {
        let p = "fir_1M_192000_minimum_phase.npy";
        assert_ne!(bank_key(p, 4, Alignment::BandWeighted, 192_000), bank_key(p, 4, Alignment::BandWeighted, 384_000));
        assert_eq!(bank_key(p, 4, Alignment::Linear, 192_000), bank_key(p, 4, Alignment::Linear, 384_000));
        assert_eq!(bank_key(p, 4, Alignment::None, 192_000), bank_key(p, 4, Alignment::None, 384_000));
        // A minimum-phase lowpass (one pole): its band-weighted delay at the
        // two rates is not the same number of samples.
        let h: Vec<f64> = (0..4096).map(|i| 0.01 * 0.99f64.powi(i)).collect();
        let at = |rate: u32| Bank::from_coeffs("t", &h, 4, Alignment::BandWeighted, rate).delay;
        assert_ne!(at(192_000), at(384_000));
    }

    /// Envelopes and plans of tracks that left the kept set go; the kept
    /// tracks' stay, every variant of them — and a track key that is a
    /// prefix of another ("t1:" of "t12:") keeps only its own.
    #[test]
    fn forgetting_tracks_drops_their_envelopes_and_plans() {
        let dir = std::env::temp_dir().join("aura-player-res-test");
        let res = Resources::new(dir);
        let env = || Arc::new(Envelope { analysis: vec![0.0; 4], analysis_sr: 100.0 });
        let plan = || Arc::new(Plan { boundaries: vec![], use_min_0: false });
        for track in ["t1:a.flac", "t2:b.flac", "t12:c.flac"] {
            for variant in ["full", "quick"] {
                res.envs.lock().unwrap().insert(format!("{track}|{variant}"), env());
                res.plans.lock().unwrap().insert(format!("{track}|{variant}|352800"), plan());
            }
        }

        res.forget_tracks_except(&["t1:a.flac".to_string(), "t12:c.flac".to_string()]);

        let mut envs: Vec<String> = res.envs.lock().unwrap().keys().cloned().collect();
        let mut plans: Vec<String> = res.plans.lock().unwrap().keys().cloned().collect();
        envs.sort();
        plans.sort();
        assert_eq!(envs, ["t12:c.flac|full", "t12:c.flac|quick", "t1:a.flac|full", "t1:a.flac|quick"]);
        assert_eq!(plans, [
            "t12:c.flac|full|352800", "t12:c.flac|quick|352800",
            "t1:a.flac|full|352800", "t1:a.flac|quick|352800",
        ]);

        // A prefix never keeps a longer key alive.
        res.forget_tracks_except(&["t1".to_string()]);
        assert!(res.envs.lock().unwrap().is_empty());
        assert!(res.plans.lock().unwrap().is_empty());

        // The case that happens with real paths: one file's path is the start
        // of another's ("a.flac" and "a.flac.bak" under the same track id).
        res.envs.lock().unwrap().insert("t1:C:\\music\\a.flac|full".to_string(), env());
        res.envs.lock().unwrap().insert("t1:C:\\music\\a.flac.bak|full".to_string(), env());
        res.forget_tracks_except(&["t1:C:\\music\\a.flac".to_string()]);
        let left: Vec<String> = res.envs.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, ["t1:C:\\music\\a.flac|full"]);
    }

    fn variant(source_id: &str) -> super::Variant {
        let rate = 44_100u32;
        let n = rate as usize * 5;
        let wave = |f: f64| (0..n).map(|i| {
            let t = i as f64 / rate as f64;
            // a tone with a hit every half second, so there are onsets
            0.2 * (2.0 * std::f64::consts::PI * f * t).sin()
                + if i % (rate as usize / 2) < 400 { 0.5 } else { 0.0 }
        }).collect::<Vec<f64>>();
        super::Variant {
            stream: None,
            key: "dc1|isp1|sub15".into(),
            src: Arc::new(crate::player::convolver::SourceBuf { l: wave(440.0), r: wave(660.0), rate }),
            out_rate: rate * 2,
            l: 2,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 1.0,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: source_id.into(),
        }
    }

    fn sidecars(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".onset_envelope.json")).collect();
        v.sort();
        v
    }

    /// A later session (another Resources, another track number) finds the
    /// envelope an earlier one wrote for the same source file and variant.
    #[test]
    fn the_envelope_is_reused_by_a_later_session() {
        let dir = tempfile::tempdir().unwrap();
        let v = variant("C:\\music\\a.flac|1234|99");
        let first = Resources::new(dir.path().to_path_buf()).envelope("t1:C:\\music\\a.flac", &v).unwrap();
        let files = sidecars(dir.path());
        assert_eq!(files.len(), 1);

        // Mark the stored envelope: only a read of this file can return it.
        let text = std::fs::read_to_string(&files[0]).unwrap();
        let at = text.find("\"envelope\": [").unwrap();
        let marked = format!("{}\"envelope\": [0.123456, 0.654321]\n}}\n", &text[..at]);
        std::fs::write(&files[0], marked).unwrap();

        let later = Resources::new(dir.path().to_path_buf()).envelope("t7:C:\\music\\a.flac", &v).unwrap();
        assert_eq!(later.analysis, [0.123456, 0.654321], "the later session did not read the stored file");
        assert_eq!(later.analysis_sr, first.analysis_sr);
        assert_eq!(sidecars(dir.path()).len(), 1);
    }

    /// Two callers ask for the same envelope at once — the Hybrid-Phase job
    /// of a track that started while its prewarm was making it: it is made
    /// once, and the second caller takes what the first one made. Both made
    /// their own, and wrote the same file.
    #[test]
    fn an_envelope_asked_for_twice_at_once_is_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let res = Arc::new(Resources::new(dir.path().to_path_buf()));
        let v = Arc::new(variant("C:\\music\\twice.flac|1234|99"));
        let go = Arc::new(std::sync::Barrier::new(2));
        let ask = || {
            let (res, v, go) = (res.clone(), v.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                res.envelope("t8:C:\\music\\twice.flac", &v)
            })
        };
        let (a, b) = (ask(), ask());
        let (a, b) = (a.join().unwrap(), b.join().unwrap());

        let (a, b) = (a.expect("the first caller's envelope"), b.expect("the second caller's envelope"));
        assert!(Arc::ptr_eq(&a, &b), "one envelope, taken by both callers");
        assert_eq!(sidecars(dir.path()).len(), 1);
    }

    /// A file cut short (a crash while writing) is dropped and computed
    /// again, never an error.
    #[test]
    fn a_file_cut_short_is_computed_again() {
        let dir = tempfile::tempdir().unwrap();
        let v = variant("C:\\music\\b.flac|5678|42");
        let whole = Resources::new(dir.path().to_path_buf()).envelope("t1:b", &v).unwrap();
        let file = sidecars(dir.path()).pop().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let cut = text.find("\"envelope\": [").unwrap() + 30;
        std::fs::write(&file, &text[..cut]).unwrap();

        let again = Resources::new(dir.path().to_path_buf()).envelope("t2:b", &v).unwrap();
        assert_eq!(again.analysis, whole.analysis);
        assert!(std::fs::read_to_string(&file).unwrap().trim_end().ends_with('}'), "the file is whole again");
    }

    /// The same path with another size or modification time is another file.
    #[test]
    fn a_changed_source_file_gets_its_own_envelope() {
        let dir = tempfile::tempdir().unwrap();
        Resources::new(dir.path().to_path_buf()).envelope("t1:c", &variant("C:\\music\\c.flac|100|1")).unwrap();
        Resources::new(dir.path().to_path_buf()).envelope("t1:c", &variant("C:\\music\\c.flac|100|2")).unwrap();
        Resources::new(dir.path().to_path_buf()).envelope("t1:c", &variant("C:\\music\\c.flac|101|2")).unwrap();
        assert_eq!(sidecars(dir.path()).len(), 3);
    }

    /// Envelope files older than the age limit go; then the oldest of the
    /// rest until the size limit holds. Anything that is not an envelope
    /// file, and anything in a subfolder, stays whatever its age.
    #[test]
    fn prune_removes_old_envelopes_then_the_oldest_over_budget() {
        use super::prune_envelope_cache;
        use std::time::{Duration, SystemTime};

        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 3600);
        let put = |name: &str, kb: usize, age_days: u64| {
            let p = dir.path().join(name);
            std::fs::write(&p, vec![b'0'; kb * 1024]).unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(now - day * age_days as u32).unwrap();
        };
        put("aaaaaaaaaaaaaaaa.onset_envelope.json", 10, 40); // too old
        put("bbbbbbbbbbbbbbbb.onset_envelope.json", 10, 20); // oldest in budget pass
        put("cccccccccccccccc.onset_envelope.json", 10, 10);
        put("dddddddddddddddd.onset_envelope.json", 10, 1);
        put("player-buffer.json", 10, 400); // not an envelope: never touched
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("eeee.onset_envelope.json"), b"x").unwrap();

        // 30 days, 25 KB: "a" goes by age (40 d); b+c+d = 30 KB > 25 KB, so
        // the oldest of those ("b") goes too; c+d = 20 KB fit.
        let (n, freed) = prune_envelope_cache(dir.path(), day * 30, 25 * 1024, now);
        assert_eq!((n, freed), (2, 20 * 1024));
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, [
            "cccccccccccccccc.onset_envelope.json",
            "dddddddddddddddd.onset_envelope.json",
            "player-buffer.json",
            "sub",
        ]);
        assert!(dir.path().join("sub").join("eeee.onset_envelope.json").exists());

        // A second pass has nothing left to do; a missing folder is no error.
        assert_eq!(prune_envelope_cache(dir.path(), day * 30, 25 * 1024, now), (0, 0));
        assert_eq!(prune_envelope_cache(&dir.path().join("nope"), day, 0, now), (0, 0));
    }
}

/// Tests for `hp_ready` and for the `hp_deferred` flag in `build_chain`.
#[cfg(test)]
mod hp_deferred_tests {
    use std::sync::Arc;

    use super::{fnv, hp_ready, Resources, Variant, ENVELOPE_SUFFIX};
    use crate::player::blend::Envelope;
    use crate::player::convolver::SourceBuf;

    fn make_variant(source_id: &str) -> Variant {
        let rate = 44_100u32;
        let n = rate as usize;
        let samples: Vec<f64> = (0..n).map(|i| (i as f64 / rate as f64).sin() * 0.1).collect();
        Variant {
            stream: None,
            key: "full".into(),
            src: Arc::new(SourceBuf { l: samples.clone(), r: samples, rate }),
            out_rate: rate * 2,
            l: 2,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.5,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: source_id.into(),
        }
    }

    /// No envelope in memory and no disk sidecar: hp_ready returns false.
    #[test]
    fn hp_ready_false_when_no_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        let v = make_variant("C:\\music\\a.flac|100|1");
        assert!(!hp_ready(&res, "t1:a.flac", &v));
    }

    /// Envelope inserted into the in-memory cache: hp_ready returns true.
    #[test]
    fn hp_ready_true_when_envelope_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        let v = make_variant("C:\\music\\b.flac|200|2");
        let key = format!("t2:b.flac|{}", v.key);
        let env = Arc::new(Envelope { analysis: vec![0.0; 4], analysis_sr: 100.0 });
        res.envs.lock().unwrap().insert(key, env);
        assert!(hp_ready(&res, "t2:b.flac", &v));
    }

    /// Sidecar file present on disk: hp_ready returns true without loading.
    #[test]
    fn hp_ready_true_when_sidecar_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        let v = make_variant("C:\\music\\c.flac|300|3");
        let disk_key = format!("{}|{}", v.source_id, v.key);
        let stem = format!("{:016x}", fnv(&disk_key));
        let sidecar = dir.path().join(format!("{}{}", stem, ENVELOPE_SUFFIX));
        std::fs::write(&sidecar, b"{}").unwrap();
        assert!(hp_ready(&res, "t3:c.flac", &v));
    }

    /// No source_id and no in-memory entry: hp_ready returns false (cannot
    /// form a stable disk key, so the disk is never checked).
    #[test]
    fn hp_ready_false_when_no_source_id() {
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        let v = make_variant(""); // empty source_id
        assert!(!hp_ready(&res, "t4:unknown", &v));
    }
}

/// The output-peak probe against the render it stands for.
#[cfg(test)]
mod output_peak_tests {
    use std::sync::Arc;

    use super::{build_chain, output_peak, Resources, Variant};
    use crate::audio::converter::dsp::{filter::find_precomputed_filter, lab::isp, true_peak};
    use crate::player::convolver::SourceBuf;
    use crate::player::settings::{Phase, PlayerSettings};

    /// A dense, hot 20 s at 44.1 kHz, band-limited the way music is: a bass
    /// line and bursts of a square-ish 3.15 kHz (harmonics to 16 kHz) whose
    /// peaks go over the ceiling all through the bursts.
    fn hot_source() -> SourceBuf {
        let rate = 44_100u32;
        let n = 20 * rate as usize;
        let mut l = vec![0.0f64; n];
        let mut r = vec![0.0f64; n];
        let w = 2.0 * std::f64::consts::PI;
        for i in 0..n {
            let t = i as f64 / rate as f64;
            let burst = if (t * 4.0).fract() < 0.3 { 1.0 } else { 0.0 };
            let sq: f64 = [1.0f64, 3.0, 5.0].iter().map(|&h| (w * 3_150.0 * h * t).sin() / h).sum::<f64>() * 0.6 * burst;
            let bass = 0.5 * (w * 55.0 * t).sin();
            l[i] = bass + sq;
            r[i] = bass - 0.7 * sq;
        }
        SourceBuf { l, r, rate }
    }

    /// The probe (5k taps at FS×2) against a 1M render at FS×8 — the smallest
    /// filter of the ladder: what is left between them is the filter length,
    /// the rate, the missing subsonic guard and the piece seams. The peak
    /// must agree to a few hundredths of a dB and the limiter's verdict
    /// exactly.
    #[test]
    fn the_probe_finds_the_renders_peak_and_verdict() {
        if find_precomputed_filter(5_000, 44_100, 88_200, "linear_phase").is_none()
            || find_precomputed_filter(1_000_000, 44_100, 352_800, "linear_phase").is_none()
        {
            println!("5k / 1M filters not found — skip");
            return;
        }
        let src = Arc::new(hot_source());
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        for phase in [Phase::Linear, Phase::Tfs] {
            let v = Arc::new(Variant {
                stream: None,
                key: format!("probe-test|{phase:?}"),
                src: src.clone(),
                out_rate: 352_800,
                l: 8,
                tp_target_dbtp: -0.5,
                tp_pred_lin: true_peak::measure_true_peak(&src.l, &src.r),
                lim_local: true,
                tokens: vec![],
                notes: vec![],
                stages: vec![],
                quick: false,
                direct: false,
                source_id: String::new(),
            });
            let s = PlayerSettings { taps: 1_000_000, phase, subsonic_hz: 0, ..PlayerSettings::default() };
            let target = 10f64.powf(-0.5 / 20.0);
            let p = output_peak(&res, "t", &v, &s, false, target).expect("the probe runs");
            // The render the converter would measure: no limiter, gain undone.
            let probe_s = PlayerSettings { isp: false, ..s.clone() };
            let mut c = build_chain(&res, 1, "t", &v, &probe_s, 0, None, None, 0).unwrap();
            let n = src.l.len() * 8;
            let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
            let mut at = 0;
            while at < n {
                let e = (at + 65_536).min(n);
                c.stage.read(&mut l[at..e], &mut r[at..e]);
                at = e;
            }
            for x in l.iter_mut().chain(r.iter_mut()) {
                *x /= c.gain;
            }
            let tp = true_peak::measure_true_peak(&l, &r);
            let rep = isp::limit_output(&mut l, &mut r, target, 352_800);
            let local = rep.map_or(true, |r| r.fell_back.is_none());
            let d = 20.0 * (p.tp_lin / tp).log10();
            assert!(d.abs() < 0.05, "{phase:?}: probe {:.3} dB off the render", d);
            assert_eq!(p.local, local, "{phase:?}: the limiter's verdict");
            // And it is cached: the second call is the same answer.
            let again = output_peak(&res, "t", &v, &s, false, target).unwrap();
            assert_eq!(again.tp_lin, p.tp_lin);
        }
    }
}

#[cfg(test)]
mod stream_variant_tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use std::sync::Mutex;

    use super::{ceiling_lin, completed, input_of, output_peak, prepare_full_variant, prepare_variant, Resources, Variant, PROBE_TAPS};
    use crate::player::convolver::Input;
    use crate::player::settings::PlayerSettings;

    fn write_wav16(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
        let data = (l.len() * 4) as u32;
        let mut b = Vec::with_capacity(44 + data as usize);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(),
                  rate.to_le_bytes().to_vec(), (rate * 4).to_le_bytes().to_vec(), 4u16.to_le_bytes().to_vec(),
                  16u16.to_le_bytes().to_vec()] {
            b.extend_from_slice(&v);
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..l.len() {
            for v in [l[i], r[i]] {
                b.extend_from_slice(&((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
            }
        }
        std::fs::write(path, b).expect("write the test file");
    }

    /// Instant start's first variant streams the stages a stream can run:
    /// its chains read the stream, and once the stream is through, the
    /// variant it becomes holds the full variant's source to the bit — the
    /// track as the whole-track stages left it is not kept beside it.
    #[test]
    fn the_first_variant_streams_into_the_full_variants_source() {
        let rate = 44_100u32;
        let n = 4 * rate as usize;
        let tone = |i: usize, f: f64, a: f64| a * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin();
        let mut l: Vec<f64> = (0..n).map(|i| tone(i, 440.0, 0.3) + tone(i, 4.0, 0.05) + 0.01).collect();
        let r: Vec<f64> = (0..n).map(|i| tone(i, 330.0, 0.3) - 0.008).collect();
        // A clipped stretch for declip, overs for the repair.
        for (i, x) in l.iter_mut().enumerate().take(150_000).skip(120_000) {
            *x = tone(i, 220.0, 1.4).clamp(-0.85, 0.85);
        }
        for k in 0..8 {
            l[60_000 + k] = if k % 2 == 0 { 0.68 } else { -0.68 };
        }
        let path = std::env::temp_dir().join(format!("aura-stream-variant-{}.wav", std::process::id()));
        write_wav16(&path, rate, &l, &r);
        let s = PlayerSettings {
            iir_dc_blocking: false,
            declip: true,
            isp: true,
            subsonic_hz: 15,
            apodizing: 2,
            adaptive_apodizer: false,
            adaptive_headroom: false,
            fs_multiplier: 2,
            ..PlayerSettings::default()
        };
        let cancel = AtomicBool::new(false);
        let first = Arc::new(prepare_variant(&path, &s, true, &cancel).expect("the first variant"));
        let full = prepare_variant(&path, &s, false, &cancel).expect("the full variant");
        let ss = first.stream.clone().expect("Instant start's first variant streams");
        assert!(first.quick && !first.direct);
        assert_eq!((first.out_rate, first.l), (full.out_rate, full.l));
        assert_eq!(first.tokens, ["DC", "ISP", "SUB15", "Apod-M"], "tokens {:?}", first.tokens);
        assert!(matches!(input_of(&first), Input::Grow(_)), "its chains read the stream");
        ss.grow.ensure(i64::MAX);
        let done = completed(&first).expect("the stream is through");
        assert!(done.src.l.iter().zip(&full.src.l).chain(done.src.r.iter().zip(&full.src.r)).all(|(a, b)| a.to_bits() == b.to_bits())
            && done.src.len() == full.src.len(), "the stream's output is the full variant's source");
        assert_eq!(done.tp_target_dbtp, full.tp_target_dbtp);
        assert!(Arc::ptr_eq(&done.src, &ss.grow.complete().unwrap()), "the stream's own buffer, not a copy");
        let _ = std::fs::remove_file(&path);
    }

    fn same_variant(what: &str, got: &Variant, want: &Variant) {
        let bits = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits());
        assert!(bits(&got.src.l, &want.src.l) && bits(&got.src.r, &want.src.r) && got.src.rate == want.src.rate,
            "{what}: the source differs from prepare_audio_phase's");
        assert_eq!((&got.key, got.out_rate, got.l), (&want.key, want.out_rate, want.l), "{what}");
        assert_eq!(got.tokens, want.tokens, "{what}: tokens");
        assert_eq!(got.notes, want.notes, "{what}: notes");
        assert_eq!(got.stages, want.stages, "{what}: rows");
        assert_eq!(got.tp_target_dbtp.to_bits(), want.tp_target_dbtp.to_bits(), "{what}: ceiling");
        assert_eq!(got.tp_pred_lin.to_bits(), want.tp_pred_lin.to_bits(), "{what}: the source's true peak");
        assert_eq!(got.lim_local, want.lim_local, "{what}: the limiter's decision");
        assert!(!got.quick && !got.direct && got.stream.is_none(), "{what}: a full variant");
    }

    /// The full variant made from a stream — one of its own (Instant start
    /// off, the prewarm) or the first variant's (Instant start on) — is the
    /// one `prepare_audio_phase` makes: the source to the bit, its tokens,
    /// notes and rows, the ceiling, the source's own true peak and the
    /// limiter's decision. With the Adaptive Apodizer on too: on a track it
    /// declines (the static preset, which the stream ran, stays) and on one
    /// it treats (its filter on what reaches the static preset, which the
    /// stream copies aside). Made from the first variant's stream with nothing
    /// after it, it holds that stream's own buffer — no second copy.
    #[test]
    fn the_full_variant_made_from_the_stream_is_prepare_audio_phases() {
        let rate = 44_100u32;
        let n = 4 * rate as usize;
        let tone = |i: usize, f: f64, a: f64| a * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin();
        let mut l: Vec<f64> = (0..n).map(|i| tone(i, 440.0, 0.3) + tone(i, 4.0, 0.05) + 0.01).collect();
        let r: Vec<f64> = (0..n).map(|i| tone(i, 330.0, 0.3) - 0.008).collect();
        for (i, x) in l.iter_mut().enumerate().take(150_000).skip(120_000) {
            *x = tone(i, 220.0, 1.4).clamp(-0.85, 0.85);
        }
        for k in 0..8 {
            l[60_000 + k] = if k % 2 == 0 { 0.68 } else { -0.68 };
        }
        let dir = std::env::temp_dir();
        let tones = dir.join(format!("aura-full-from-stream-{}.wav", std::process::id()));
        write_wav16(&tones, rate, &l, &r);
        // A track the Adaptive Apodizer treats.
        let ring = crate::audio::converter::apodize::aa_treated_fixture(6);
        let ringing = dir.join(format!("aura-full-from-stream-aa-{}.wav", std::process::id()));
        write_wav16(&ringing, rate, &ring, &ring);
        let rack = |iir: bool, declip: bool, isp: bool, sub: u32, apod: u32, aa: bool| PlayerSettings {
            iir_dc_blocking: iir,
            declip,
            isp,
            subsonic_hz: sub,
            apodizing: apod,
            adaptive_apodizer: aa,
            adaptive_headroom: true,
            headroom_db: -3.0,
            fs_multiplier: 2,
            use_gpu: false,
            ..PlayerSettings::default()
        };
        let cancel = AtomicBool::new(false);
        for (path, s) in [
            (&tones, rack(false, true, true, 15, 2, false)),
            (&tones, rack(true, false, false, 20, 0, true)),
            (&tones, rack(false, true, true, 15, 3, true)),
            (&ringing, rack(false, false, true, 15, 2, true)),
            (&ringing, rack(true, false, true, 0, 0, true)),
        ] {
            let what = format!("{} {}", path.file_name().unwrap().to_string_lossy(), s.source_key());
            let want = prepare_variant(path, &s, false, &cancel).expect("the full variant");
            let own = prepare_full_variant(path, &s, &cancel, None, true, None).expect("from a stream of its own");
            same_variant(&format!("{what}, its own stream"), &own, &want);
            let first = prepare_variant(path, &s, true, &cancel).expect("the first variant");
            let ss = first.stream.clone().expect("it streams");
            let made = prepare_full_variant(path, &s, &cancel, Some(ss.clone()), true, None).expect("from the first variant's");
            same_variant(&format!("{what}, the first variant's stream"), &made, &want);
            if !made.tokens.iter().any(|t| t == "AA") {
                assert!(Arc::ptr_eq(&made.src, &ss.grow.complete().unwrap()), "{what}: the stream's own buffer, not a copy");
            }
            if path == &ringing {
                assert!(want.tokens.iter().any(|t| t == "AA"), "{what}: the fixture is treated, {:?}", want.tokens);
            }
        }
        let _ = std::fs::remove_file(&tones);
        let _ = std::fs::remove_file(&ringing);
    }

    /// Instant start off probes the full variant's output while its stream
    /// runs: the probe's variant reads the stream as it grows, and what it
    /// finds is what the probe finds on the finished variant.
    #[test]
    fn the_full_variant_is_probed_on_its_stream_as_it_grows() {
        let rate = 44_100u32;
        let n = 3 * rate as usize;
        let tone = |i: usize, f: f64, a: f64| a * (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin();
        let l: Vec<f64> = (0..n).map(|i| tone(i, 440.0, 0.9) + tone(i, 13_000.0, 0.1)).collect();
        let r: Vec<f64> = (0..n).map(|i| tone(i, 330.0, 0.95)).collect();
        let path = std::env::temp_dir().join(format!("aura-probe-on-stream-{}.wav", std::process::id()));
        write_wav16(&path, rate, &l, &r);
        let s = PlayerSettings { isp: true, subsonic_hz: 15, apodizing: 2, adaptive_apodizer: false, fs_multiplier: 2, use_gpu: false, ..PlayerSettings::default() };
        let cancel = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let probed = Mutex::new(None);
        let res = Resources::new(dir.path().join("a"));
        let tk = "probe-on-stream";
        let probe = |v: &Variant| {
            assert!(matches!(input_of(v), Input::Grow(_)), "the probe reads the stream");
            *probed.lock().unwrap() = Some(output_peak(&res, tk, v, &s, false, ceiling_lin(v, &s)));
        };
        let full = prepare_full_variant(&path, &s, &cancel, None, true, Some(&probe)).expect("the full variant");
        let early = probed.lock().unwrap().take().expect("the probe ran while the stream did");
        // The same probe on the finished variant, from scratch.
        let fresh = Resources::new(dir.path().join("b"));
        let late = output_peak(&fresh, tk, &full, &s, false, ceiling_lin(&full, &s));
        let _ = std::fs::remove_file(&path);
        let filter = crate::audio::converter::dsp::filter::find_precomputed_filter(PROBE_TAPS, rate, 2 * rate, "linear_phase");
        match (early, late) {
            (Some(a), Some(b)) => assert!(a.tp_lin.to_bits() == b.tp_lin.to_bits() && a.local == b.local,
                "on the stream {:?}, on the finished variant {:?}", (a.tp_lin, a.local), (b.tp_lin, b.local)),
            (None, None) => assert!(filter.is_none(), "the probe's filter is there, yet neither probe ran"),
            (a, b) => panic!("one probe ran, the other did not: {:?} {:?}", a.map(|p| p.tp_lin), b.map(|p| p.tp_lin)),
        }
    }
}

#[cfg(test)]
mod bank_cache_tests {
    use std::sync::Arc;

    use super::Resources;
    use crate::player::convolver::Alignment;

    /// A bank asked for while it is being built is waited for, not built a
    /// second time: the pair's minimum-phase bank ordered ahead and the chain
    /// asking for it meanwhile get one and the same bank, kept once.
    #[test]
    fn a_bank_asked_for_while_it_is_built_is_built_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A filter long enough for four asks to overlap its build, as a .npy.
        let n = 1usize << 18;
        let mut x = 1u32;
        let taps: Vec<f64> = (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 8) as f64 / (1u64 << 24) as f64 * 1e-3
            })
            .collect();
        let mut h = format!("{{'descr': '<f8', 'fortran_order': False, 'shape': ({},), }}", n).into_bytes();
        while (10 + h.len() + 1) % 64 != 0 {
            h.push(b' ');
        }
        h.push(b'\n');
        let mut b = b"\x93NUMPY\x01\x00".to_vec();
        b.extend_from_slice(&(h.len() as u16).to_le_bytes());
        b.extend_from_slice(&h);
        for t in &taps {
            b.extend_from_slice(&t.to_le_bytes());
        }
        let path = dir.path().join("pair-min.npy");
        std::fs::write(&path, b).expect("write the filter");
        let res = Resources::new(dir.path().join("cache"));
        let p = path.to_string_lossy().to_string();
        let banks: Vec<Arc<super::Bank>> = std::thread::scope(|sc| {
            let asks: Vec<_> = (0..4).map(|_| sc.spawn(|| res.bank(&p, 2, Alignment::BandWeighted, 88_200).expect("the bank"))).collect();
            asks.into_iter().map(|a| a.join().unwrap()).collect()
        });
        assert!(banks.iter().all(|b| Arc::ptr_eq(b, &banks[0])), "one bank for all four");
        assert_eq!(res.banks.lock().unwrap().len(), 1, "kept once");
    }

    /// A filter of `n` taps of noise (seed `seed`) written as a .npy file.
    fn npy(path: &std::path::Path, n: usize, seed: u32) -> String {
        let mut x = seed;
        let mut h = format!("{{'descr': '<f8', 'fortran_order': False, 'shape': ({},), }}", n).into_bytes();
        while (10 + h.len() + 1) % 64 != 0 {
            h.push(b' ');
        }
        h.push(b'\n');
        let mut b = b"\x93NUMPY\x01\x00".to_vec();
        b.extend_from_slice(&(h.len() as u16).to_le_bytes());
        b.extend_from_slice(&h);
        for _ in 0..n {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            b.extend_from_slice(&((x >> 8) as f64 / (1u64 << 24) as f64 * 1e-3).to_le_bytes());
        }
        std::fs::write(path, b).expect("write the filter");
        path.to_string_lossy().to_string()
    }

    /// What a CPU chain on `bank` plays from the start of a stretch of noise,
    /// as bits.
    fn played(bank: Arc<super::Bank>) -> Vec<u64> {
        let mut x = 11u32;
        let mut noise = || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) as f64 / (1u64 << 24) as f64 - 0.5
        };
        let l: Vec<f64> = (0..44_100).map(|_| noise()).collect();
        let r: Vec<f64> = (0..44_100).map(|_| noise()).collect();
        let src = Arc::new(crate::player::convolver::SourceBuf { l, r, rate: 44_100 });
        let (mut st, _) = super::make_conv_stage(bank, crate::player::convolver::Input::File(src), 0, None, None);
        let (mut ol, mut or_) = (vec![0.0; 8_192], vec![0.0; 8_192]);
        let mut out = Vec::new();
        for _ in 0..4 {
            st.read(&mut ol, &mut or_);
            out.extend(ol.iter().chain(or_.iter()).map(|v| v.to_bits()));
        }
        out
    }

    /// A Hybrid-Phase pair's two banks are built side by side — on two
    /// threads — and each is the bank built alone: a chain on it plays the
    /// same samples to the bit. Each is kept once, and asked for again comes
    /// from the cache. (Whether the two builds also overlap in time is the
    /// scheduler's: on a busy 4-core CI runner the linear one, a few
    /// milliseconds here, was done before the other thread got a core.)
    #[test]
    fn a_pairs_two_banks_are_built_side_by_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lin = npy(&dir.path().join("pair-lin.npy"), 1 << 18, 3);
        let min = npy(&dir.path().join("pair-min.npy"), 1 << 18, 5);
        let res = Resources::new(dir.path().join("cache"));
        let (a, b) = res.pair_banks(&lin, Alignment::Linear, &min, 2, 88_200).expect("the pair");
        let spans: Vec<_> =
            super::PAIR_BUILDS.lock().unwrap().iter().filter(|s| s.0 == lin || s.0 == min).cloned().collect();
        assert_eq!(spans.len(), 2, "one build each");
        assert_ne!(spans[0].3, spans[1].3, "the two built on two threads: {:?}", spans);
        assert_eq!(res.banks.lock().unwrap().len(), 2, "each kept once");
        let (a2, b2) = res.pair_banks(&lin, Alignment::Linear, &min, 2, 88_200).expect("from the cache");
        assert!(Arc::ptr_eq(&a, &a2) && Arc::ptr_eq(&b, &b2), "asked again: the same banks");
        // A stream's own linear filter looks ahead less: its branch is lined
        // up as asked, a bank of its own; the minimum-phase one is the same.
        let (c, b3) = res.pair_banks(&lin, Alignment::LookAhead(1_234), &min, 2, 88_200).expect("lined up otherwise");
        assert_eq!((a.delay, c.delay), ((1usize << 18) / 2 - 1, 1_234));
        assert!(!Arc::ptr_eq(&a, &c) && Arc::ptr_eq(&b, &b3));
        let alone_a = Arc::new(super::Bank::load(&lin, 2, Alignment::Linear, 88_200).expect("alone"));
        let alone_b = Arc::new(super::Bank::load(&min, 2, Alignment::BandWeighted, 88_200).expect("alone"));
        assert!(played(a) == played(alone_a), "the linear bank plays as the one built alone");
        assert!(played(b) == played(alone_b), "the minimum-phase bank plays as the one built alone");
    }
}

#[cfg(test)]
mod disk_chain_tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::{build_file_chain, disk_file, PassStage};
    use crate::player::convolver::SourceBuf;
    use crate::player::stages::Stage;

    /// The converter's kind of file: 24-bit FLAC from its own encoder —
    /// tones and a little noise, both rails, a stretch of silence.
    fn converted_flac(path: &Path, rate: u32, n: usize) {
        let (mut l, mut r) = crate::audio::converter::decode::tests::grid_signal(n, 7);
        l[100..400].fill(0.0);
        r[100..400].fill(0.0);
        crate::audio::converter::encode::encode_flac(&l, &r, rate, path, &[]).expect("encode");
    }

    /// A converted file plays from exact integers what it played from f64,
    /// to the bit, in half the memory: from its start, from a seek, and on
    /// past its end.
    #[test]
    fn a_converted_file_plays_from_integers_what_it_played_from_f64() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("converted.flac");
        let (rate, n) = (88_200u32, 3 * 88_200 + 4_321);
        converted_flac(&path, rate, n);
        let a = crate::audio::converter::decode::decode_file(&path).expect("decode");
        let src = disk_file(&path).expect("decode for the disk chain");
        assert!(matches!(src.planes, crate::audio::converter::decode::Planes::Int { .. }), "kept as integers");
        assert_eq!((src.len(), src.rate), (n, rate));
        assert_eq!(src.planes.bytes(), n * 8, "half of f64's 16 bytes a frame");
        let old = Arc::new(SourceBuf { l: a.samples_l, r: a.samples_r, rate: a.sample_rate });
        let same = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits());
        for start_s in [0.0, 1.25] {
            let mut new = build_file_chain(1, src.clone(), start_s, &path);
            let mut was = PassStage { src: old.clone(), pos: new.start };
            assert_eq!(new.stage.total(), was.total());
            let (mut nl, mut nr) = (vec![0.0; 4_099], vec![0.0; 4_099]);
            let (mut wl, mut wr) = (vec![0.0; 4_099], vec![0.0; 4_099]);
            loop {
                let got = new.stage.read(&mut nl, &mut nr);
                assert_eq!(got, was.read(&mut wl, &mut wr));
                assert!(same(&nl, &wl) && same(&nr, &wr), "from {} s, at {}", start_s, new.stage.position());
                assert_eq!(new.stage.position(), was.position());
                if got == 0 {
                    break;
                }
            }
        }
    }
}
/// Instant start's first variant of a file: its level, against the full
/// variant's, and how long it takes to make.
#[cfg(test)]
mod first_variant_level {
    use std::f64::consts::PI;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::{ceiling_db, level_gain, prepare_full_variant, prepare_variant, Variant};
    use crate::player::settings::PlayerSettings;
    use crate::player::source_stages::tests::{programme, write_wav16, write_wav_f32};
    use crate::player::stages::{GainStage, LimiterStage, LimiterTally, Stage};

    /// A stereo signal as a stage.
    struct Pair {
        l: Vec<f64>,
        r: Vec<f64>,
        pos: u64,
    }

    impl Stage for Pair {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for i in 0..out_l.len().min(out_r.len()) {
                let p = self.pos as usize;
                let (a, b) = if p < self.l.len() {
                    inside += 1;
                    (self.l[p], self.r[p])
                } else {
                    (0.0, 0.0)
                };
                out_l[i] = a;
                out_r[i] = b;
                self.pos += 1;
            }
            inside
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.l.len() as u64
        }
    }

    /// What the always-acting limiter brings down behind `level` over the
    /// whole of `src` at its own rate (the filter left out).
    fn limited(level: Box<dyn Fn(Box<dyn Stage>) -> Box<dyn Stage>>, l: &[f64], r: &[f64], target: f64, rate: u32) -> LimiterTally {
        let tally = Arc::new(Mutex::new(LimiterTally::default()));
        let mut st = LimiterStage::new(level(Box::new(Pair { l: l.to_vec(), r: r.to_vec(), pos: 0 })), target, rate, 0).with_tally(tally.clone());
        let (mut ol, mut or) = (vec![0.0; 4_096], vec![0.0; 4_096]);
        let mut at = 0;
        while at < l.len() {
            st.read(&mut ol, &mut or);
            at += 4_096;
        }
        let t = tally.lock().unwrap().clone();
        t
    }

    /// The first variant of a file plays at the level the full variant's
    /// rule gives the whole decoded track: on a source over full scale (its
    /// overs left whole by the repair) the two come to the same scalar, and
    /// the limiter behind the first has nothing to hold — where the slow
    /// gain it replaces started at 0 dB and left the limiter to cut. On a CD
    /// whose overs between samples are sparse, both play at 0 dB; on one
    /// where they are dense, the repair takes them to full scale, the first
    /// variant counts them there, and the two differ by less than the 0.3 dB
    /// the full one would glide for.
    #[test]
    fn a_files_first_variant_plays_at_the_full_variants_level() {
        let rate = 44_100u32;
        let n = 4 * rate as usize;
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        // Hot: the programme 6 dB up, and a tone 7 dB over full scale.
        let (mut hl, mut hr) = programme(n, rate);
        for x in hl.iter_mut().chain(hr.iter_mut()) {
            *x *= 2.0;
        }
        for i in 60_000..80_000 {
            let v = 2.2 * (2.0 * PI * 150.0 * i as f64 / rate as f64).sin();
            hl[i] = v;
            hr[i] = 0.9 * v;
        }
        let hot = dir.join(format!("aura-first-level-hot-{pid}.wav"));
        write_wav_f32(&hot, rate, &hl, &hr);
        // CD-like, sparse: the programme as it is (overs between samples
        // where its patterns are).
        let (cl, cr) = programme(n, rate);
        let sparse = dir.join(format!("aura-first-level-sparse-{pid}.wav"));
        write_wav16(&sparse, rate, &cl, &cr);
        // CD-like, dense: a loud tone clipped at the rail all along.
        let dl: Vec<f64> = (0..n).map(|i| (1.6 * (2.0 * PI * 1_234.5 * i as f64 / rate as f64).sin()).clamp(-1.0, 1.0)).collect();
        let dense = dir.join(format!("aura-first-level-dense-{pid}.wav"));
        write_wav16(&dense, rate, &dl, &dl);
        let s = PlayerSettings {
            isp: true,
            declip: false,
            subsonic_hz: 0,
            apodizing: 0,
            adaptive_apodizer: false,
            adaptive_headroom: false,
            fs_multiplier: 2,
            use_gpu: false,
            ..PlayerSettings::default()
        };
        let cancel = AtomicBool::new(false);
        let gain = |v: &Variant| level_gain(v.tp_pred_lin, v.lim_local, ceiling_db(v, &s), s.isp);
        for (path, what) in [(&hot, "hot"), (&sparse, "sparse"), (&dense, "dense")] {
            let first = prepare_variant(path, &s, true, &cancel).expect("the first variant");
            let ss = first.stream.clone().expect("it streams");
            let full = prepare_full_variant(path, &s, &cancel, Some(ss), true, None).expect("the full variant");
            let (g1, g2) = (gain(&first), gain(&full));
            match what {
                "hot" => {
                    assert_eq!(g1.to_bits(), g2.to_bits(), "{what}: first {:+.3} dB, full {:+.3} dB", 20.0 * g1.log10(), 20.0 * g2.log10());
                    assert!(g1 < 10f64.powf(-6.0 / 20.0), "{what}: the whole track comes down: {:+.2} dB", 20.0 * g1.log10());
                    // The level stage at the source's rate over what the
                    // stream hands the chain: the scalar, then the limiter.
                    let target = 10f64.powf(ceiling_db(&first, &s) / 20.0);
                    let src = full.src.clone();
                    let now = limited(Box::new(move |up| Box::new(GainStage::new(up, g1))), &src.l, &src.r, target, rate);
                    assert_eq!(now.reduced, 0, "{what}: the limiter brought {} samples down", now.reduced);
                    // As it was: the slow gain from 0 dB, the limiter cutting.
                    let was = limited(Box::new(move |up| Box::new(crate::player::slow_gain::SlowGainStage::new(up, target, rate))), &src.l, &src.r, target, rate);
                    assert!(was.reduced > 1_000 && was.max_db < -3.0, "{what}: the slow gain left the limiter {} samples, down to {:.2} dB", was.reduced, was.max_db);
                }
                "sparse" => assert_eq!((g1, g2), (1.0, 1.0), "{what}"),
                _ => {
                    // The repair takes its overs to full scale: the first
                    // variant's peak, and the full one's but for what the
                    // repair could not finish — under the glide's 0.3 dB.
                    let d = 20.0 * (g1 / g2).log10();
                    assert!(g1 < 1.0 && d.abs() <= 0.3, "{what}: first {:+.3} dB, full {:+.3} dB", 20.0 * g1.log10(), 20.0 * g2.log10());
                }
            }
        }
        for p in [&hot, &sparse, &dense] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The first 30 s of a file as the player plays it under Instant start:
    /// the first variant until the full one takes over at AURA_AS_HEARD_SWAP
    /// seconds (6 by default), the full one then gliding over 1.5 s from where
    /// the first played (where they differ by more than 0.3 dB) — the way
    /// 1.5.0 did it (its repair; the slow gain from 0 dB, the limiter behind
    /// it) and the way it is now (the repair leaving hot clusters whole; the
    /// first variant at the full variant's rule on the whole track). At the
    /// source's rate, the filter left out, the levels as they come out — not
    /// matched: the slide is the point. The default rack's head (ceiling
    /// −0.5 dBTP). 24-bit WAVs into AURA_ISP_HOT_OUT. A measurement, not a
    /// test: AURA_ISP_HOT_FILES="a.mp3" `cargo test --profile fast --bins
    /// as_heard_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn as_heard_harness() {
        use crate::audio::converter::dsp::{lab::isp, true_peak};
        let files = std::env::var("AURA_ISP_HOT_FILES").unwrap_or_default();
        let out = std::path::PathBuf::from(std::env::var("AURA_ISP_HOT_OUT").expect("AURA_ISP_HOT_OUT"));
        let swap_s = std::env::var("AURA_AS_HEARD_SWAP").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(6.0);
        let s = PlayerSettings::default();
        let eng = s.to_engine();
        let cancel = AtomicBool::new(false);
        let target_db = -0.5f64;
        let target = 10f64.powf(target_db / 20.0);
        // The rule; `cap`: the overs counted at full scale, as the first
        // variant does where the repair will take them there.
        let rule = |l: &[f64], r: &[f64], rate: u32, cap: bool| {
            let tp = true_peak::measure_true_peak_parallel(l, r);
            let tp = if cap { tp.min(1.0) } else { tp };
            let local = isp::output_limit_is_local_parallel(l, r, target, rate);
            level_gain(tp, local, target_db, true)
        };
        let _ = std::fs::create_dir_all(&out);
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = Path::new(f.trim());
            let a = crate::audio::converter::decode::decode_file(path).expect("decode");
            let rate = a.sample_rate;
            let (mut hl, mut hr) = (a.samples_l, a.samples_r);
            let head = crate::audio::converter::pipeline::prepare::source_head(&mut hl, &mut hr, rate, a.lossy, &eng, &cancel).expect("the head");
            let n = hl.len().min((30.0 * rate as f64) as usize);
            let swap = ((swap_s * rate as f64) as usize).min(n);
            let hist = LimiterStage::history(rate) as usize;
            let glide = (super::QUICK_GLIDE_S * rate as f64) as u64;
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            for now in [false, true] {
                let (mut l, mut r) = (hl.clone(), hr.clone());
                let (hot_l, hot_r): (&[(usize, usize)], &[(usize, usize)]) = if now { (&head.hot_spans_l, &head.hot_spans_r) } else { (&[], &[]) };
                isp::run(&mut l, &mut r, &head.declip_spans_l, &head.declip_spans_r, hot_l, hot_r, &cancel);
                let g_full = rule(&l, &r, rate, false);
                let (mut ol, mut or) = (vec![0.0; n], vec![0.0; n]);
                // The first variant up to the swap.
                let tally1 = Arc::new(Mutex::new(LimiterTally::default()));
                let (level_stage, g_first_end): (Box<dyn Stage>, Box<dyn Fn() -> f64>) = if now {
                    let cap = [&head.declip_spans_l, &head.declip_spans_r, &head.hot_spans_l, &head.hot_spans_r].iter().all(|v| v.is_empty());
                    let g = rule(&hl, &hr, rate, cap);
                    (Box::new(GainStage::new(Box::new(Pair { l: l.clone(), r: r.clone(), pos: 0 }), g)), Box::new(move || g))
                } else {
                    let sg = crate::player::slow_gain::SlowGainStage::new(Box::new(Pair { l: l.clone(), r: r.clone(), pos: 0 }), target, rate);
                    let live = sg.live_gain();
                    (Box::new(sg), Box::new(move || f64::from_bits(live.load(std::sync::atomic::Ordering::Relaxed))))
                };
                let g_first_start = if now { g_first_end() } else { 1.0 };
                let mut first = LimiterStage::new(level_stage, target, rate, 0).with_tally(tally1.clone());
                let mut at = 0;
                while at < swap {
                    let e = (at + 4_096).min(swap);
                    first.read(&mut ol[at..e], &mut or[at..e]);
                    at = e;
                }
                let g0 = g_first_end();
                // The full variant from the swap on, gliding from the first's level.
                let tally2 = Arc::new(Mutex::new(LimiterTally::default()));
                let up = Box::new(Pair { l: l.clone(), r: r.clone(), pos: (swap - hist.min(swap)) as u64 });
                let glides = (20.0 * (g0.max(1e-9) / g_full).log10()).abs() > 0.3;
                let level: Box<dyn Stage> = if glides { Box::new(GainStage::gliding(up, g_full, g0, glide)) } else { Box::new(GainStage::new(up, g_full)) };
                let mut full = LimiterStage::new(level, target, rate, swap as u64).with_tally(tally2.clone());
                while at < n {
                    let e = (at + 4_096).min(n);
                    full.read(&mut ol[at..e], &mut or[at..e]);
                    at = e;
                }
                let label = if now { "T44" } else { "1.5.0" };
                let file = out.join(format!("{stem} first 30s as heard {label}.wav"));
                crate::audio::converter::pipeline::prepare::tests::write_wav24(&file, rate, &ol, &or);
                let (t1, t2) = (tally1.lock().unwrap().clone(), tally2.lock().unwrap().clone());
                eprintln!(
                    "HARNESS {stem} [{label}]: first variant {:+.2} dB at the start, {:+.2} dB at {swap_s:.1} s; the limiter brought {} samples down to {:.2} dB; full variant {:+.2} dB ({}), its limiter {} samples; written {}",
                    20.0 * g_first_start.log10(), 20.0 * g0.log10(), t1.reduced, t1.max_db, 20.0 * g_full.log10(),
                    if glides { "gliding 1.5 s" } else { "no glide" }, t2.reduced, file.display()
                );
            }
        }
    }

    /// How long Instant start's first variant of a file takes to make — what
    /// the first sound waits for before its chain is built — and the level it
    /// is given. A measurement, not a test: AURA_ISP_HOT_FILES="a.mp3|b.flac"
    /// `cargo test --profile fast --bins first_variant_delay_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn first_variant_delay_harness() {
        let files = std::env::var("AURA_ISP_HOT_FILES").unwrap_or_default();
        let s = PlayerSettings::default();
        let cancel = AtomicBool::new(false);
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = Path::new(f.trim());
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let mut times = Vec::new();
            for _ in 0..3 {
                let t0 = Instant::now();
                let v = match prepare_variant(path, &s, true, &cancel) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("HARNESS {name}: {e}");
                        break;
                    }
                };
                times.push(t0.elapsed().as_secs_f64());
                if times.len() == 3 {
                    eprintln!(
                        "HARNESS {name}: {} Hz, {:.1} s; first variant made in {:.0} / {:.0} / {:.0} ms (cold, then warm); stream {}; tp_pred {:+.2} dB, local {}, ceiling {:+.2}",
                        v.src.rate, v.src.len() as f64 / v.src.rate.max(1) as f64,
                        times[0] * 1000.0, times[1] * 1000.0, times[2] * 1000.0,
                        v.stream.is_some(), 20.0 * v.tp_pred_lin.max(1e-12).log10(), v.lim_local, super::ceiling_db(&v, &s)
                    );
                }
                drop(v);
                // The stream's thread stops once nothing reads it.
                std::thread::sleep(std::time::Duration::from_millis(1_500));
            }
        }
    }
}
