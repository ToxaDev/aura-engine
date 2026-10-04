//! The chain a stream plays: the file chain's conversion side on a live
//! source — convolver (linear, minimum or TFS), XTC, the output limiter at
//! the rack's ceiling, the subsonic guard.
//!
//! The source stages run on the stream before it reaches the live source
//! (`source_stages`, on the decoder thread): DC (the 2 Hz high-pass), the
//! intersample repair, the subsonic filter, the static apodizer. What needs
//! the whole track has nothing to work on: declip and the adaptive apodizer
//! are off. The adaptive headroom decides on the stream's first seconds
//! (`LivePlan::decide_headroom`); the level is a slow gain looking ahead,
//! and the limiter holds the overs at the ceiling (the rack's Headroom,
//! −0.5 dBTP when it is off or the adaptive headroom keeps it).
//! Hybrid-Phase and alpha-HP play from the first sound: their plan grows with
//! the stream (`blend::PlanFeed`, the onset envelope read from the live
//! source). The subsonic filter runs again as the guard at the output rate.
//!
//! A filter looks ahead (`Bank::delay`), so the first block cannot be made
//! before that much of the stream is in: `LivePlan::first_need`. A shipped
//! linear filter looks ahead by half its length — 42.5 s for 30M at 352.8 kHz
//! — so a stream plays its own (`stream_linear`): the same |H| and, up to
//! 20 kHz, the same phase, 50 ms ahead.
//!
//! BIT-PERFECT plays none of it: the live source's shadow — the stream as
//! decoded, before the source stages — at the stream's rate, straight to the
//! device (`build_direct`), as a file's direct variant plays.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::audio::converter::dsp::filter::{find_precomputed_filter, missing_filter_error, taps_label, TAP_LADDER};
use crate::audio::converter::dsp::lab::stats::SourceStats;
use crate::audio::converter::dsp::lab::stream_linear;
use crate::audio::converter::dsp::lab::{headroom, CHAIN_DECLINED, CHAIN_RAN};
use crate::audio::converter::dsp::true_peak::TARGET_TRUE_PEAK_DBTP;
use crate::player::blend::{lead_in, live_plan_need, AlphaStage, HybridStage, PlanFeed};
use crate::player::chain::{Chain, Resources};
use crate::player::convolver::{Alignment, Bank, Input, PolyStream, SpectrumShare, BLOCK, HEAD_CHUNK};
use crate::player::gpu::ctx::GpuPolyCtx;
use crate::player::gpu::poly_stream::GpuPolyStream;
use crate::player::gpu::vram_demand_hp;
use crate::player::settings::{Phase, PlayerSettings};
use crate::player::source_stages::SourcePlan;
use crate::player::slow_gain::SlowGainStage;
use crate::player::stages::{skip_to, FirStage, LimiterStage, LimiterTally, Stage, XtcStage};

use super::live::LiveSource;
use super::RadioShared;

/// The track id a stream's chain carries (the queue's count up from 1).
pub const RADIO_TRACK_ID: u64 = u64::MAX - 1;

/// A stream's chain worked out before it is built.
pub struct LivePlan {
    /// The rack it plays (Hybrid-Phase and alpha-HP as their linear stand-in).
    pub s: PlayerSettings,
    pub out_rate: u32,
    pub l: usize,
    pub bank: Arc<Bank>,
    /// Hybrid-Phase and alpha-HP: the minimum-phase branch (band-weighted).
    pub min_bank: Option<Arc<Bank>>,
    /// Where the convolver branches begin (Hybrid-Phase: a lead-in early).
    pub branch_start: u64,
    /// The phase's token on the chain ("MIN", "TFS", "HP", "aHP"), none for linear.
    pub phase_token: Option<&'static str>,
    /// The linear filter (Hybrid-Phase's and alpha-HP's linear branch) is the
    /// stream's own (`stream_linear`), not the shipped one.
    pub stream_linear: bool,
    pub xtc: Option<Arc<(Vec<f64>, Vec<f64>)>>,
    pub guard: Option<Arc<Vec<f64>>>,
    pub ceiling_db: f64,
    /// The adaptive headroom's verdict for the badge — whether it kept the
    /// shipped ceiling, and why — once decided (`decide_headroom`).
    pub ahr: Option<(bool, String)>,
    /// Output index the chain starts at, and where its convolver starts
    /// (less the histories of the stages after it).
    pub start: u64,
    pub conv_start: u64,
    pub guard_in: u64,
    pub lim_in: u64,
    /// The filter's look-ahead, output frames.
    pub delay: u64,
    /// Source frames the convolver's first block needs (its prime included).
    pub first_need: i64,
    /// The source stages the stream runs before the live source.
    pub source: SourcePlan,
}

impl LivePlan {
    /// The filter's look-ahead in seconds: how far behind the stream a
    /// linear phase plays.
    pub fn delay_s(&self) -> f64 {
        self.delay as f64 / self.out_rate as f64
    }

    /// The adaptive headroom on the stream's first `secs` seconds: the
    /// ceiling the slow gain and the limiter hold, and the badge's verdict.
    pub fn decide_headroom(&mut self, stats: Option<&SourceStats>, secs: f64) {
        let (ceiling, ahr) = headroom_for(&self.s, stats, secs);
        self.ceiling_db = ceiling;
        self.ahr = ahr;
    }
}

/// The file path's rule (`prepare::output_ceiling`) on a stream's start:
/// with Headroom set and the adaptive headroom on, the shipped ceiling is
/// kept when lowering it would cost level for nothing (`headroom::decide`).
/// Returns the ceiling (dBTP) and, when the adaptive headroom decided,
/// whether it kept the shipped ceiling and why.
pub fn headroom_for(s: &PlayerSettings, stats: Option<&SourceStats>, secs: f64) -> (f64, Option<(bool, String)>) {
    if s.headroom_db >= 0.0 {
        return (TARGET_TRUE_PEAK_DBTP, None);
    }
    if !s.adaptive_headroom {
        return (s.headroom_db, None);
    }
    let d = headroom::decide(stats, s.headroom_db);
    let seen = match stats {
        Some(st) if st.peak_dbfs.is_finite() => {
            let enob = match (d.reason.as_str(), st.enob) {
                ("high-enob-low-peak", Some(e)) => format!(", ENOB {:.1}", e),
                _ => String::new(),
            };
            format!(": the stream's first {:.0} s peak at {:.1} dBFS{}", secs, st.peak_dbfs, enob)
        }
        Some(_) => format!(": the stream's first {:.0} s are silent", secs),
        None => String::new(),
    };
    if d.skip_gain {
        let why = format!("ceiling kept at {:.1} dBTP instead of {:.1}{}", TARGET_TRUE_PEAK_DBTP, s.headroom_db, seen);
        (TARGET_TRUE_PEAK_DBTP, Some((true, why)))
    } else {
        (s.headroom_db, Some((false, format!("ceiling lowered to {:.1} dBTP as asked{}", s.headroom_db, seen))))
    }
}

/// The output rate for a source at `src_rate` and the rack's FS multiplier:
/// its family base × FS, one FS step up while the source is at or above it
/// (the file path's rule). Returns the rate and the FS used.
pub fn out_rate_for(src_rate: u32, fs: u32) -> Result<(u32, u32), String> {
    let base = crate::audio::converter::pipeline::prepare::detect_family(src_rate).ok_or_else(|| {
        format!("{} Hz: only the 44.1 kHz and 48 kHz families can be upsampled", src_rate)
    })?;
    let mut fs = fs.max(1);
    while base * fs <= src_rate {
        if fs >= 16 {
            return Err(format!("{} Hz is already at the highest output rate", src_rate));
        }
        fs *= 2;
    }
    Ok((base * fs, fs))
}

/// How the phase's filter `taps` long is lined up for a stream at
/// `src_rate` played at `out_rate`: minimum phase plays as it is, TFS 30 ms
/// ahead (`tfs::look_ahead`); a linear filter — and Hybrid-Phase and alpha-HP
/// by their linear branch — 50 ms ahead when the stream can have its own
/// (`stream_linear::available`), else the shipped one a half ahead.
fn alignment(phase: Phase, taps: usize, src_rate: u32, out_rate: u32) -> Alignment {
    stream_alignment(phase, taps, out_rate, stream_linear::available(taps, src_rate, out_rate))
}

/// `alignment`, the stream's own linear filter looking `stream_k` output
/// frames ahead (None: the shipped one plays).
fn stream_alignment(phase: Phase, taps: usize, out_rate: u32, stream_k: Option<usize>) -> Alignment {
    match phase {
        Phase::Minimum => Alignment::None,
        Phase::Tfs => Alignment::LookAhead(crate::audio::converter::dsp::lab::tfs::look_ahead(taps, out_rate)),
        _ => stream_k.map_or(Alignment::Linear, Alignment::LookAhead),
    }
}

/// Source frames the convolver's first output needs (its prime included):
/// the branches begin at output index `branch_start`, the filter looks
/// `delay` output frames ahead. A live stream plays head and tail
/// (convolver.rs): it waits for the end of the `HEAD_CHUNK` piece holding
/// that first frame, not of its whole block.
fn first_need(branch_start: u64, delay: u64, l: usize) -> i64 {
    let t = ((branch_start + delay) / l as u64) as i64;
    (t / HEAD_CHUNK as i64 + 1) * HEAD_CHUNK as i64
}

/// Whether the filter files phase `phase` plays from are installed for
/// `taps` taking a stream at `src_rate` up to `out_rate` — the blobs of its
/// factor (`filter::design_rate`), as `plan` loads them; TFS from its own
/// blob or the pair it is derived from.
fn installed(phase: Phase, taps: usize, src_rate: u32, out_rate: u32) -> bool {
    let has = |kind: &str| find_precomputed_filter(taps, src_rate, out_rate, kind).is_some();
    match phase {
        Phase::Linear => has("linear_phase"),
        Phase::Minimum => has("minimum_phase"),
        Phase::Tfs => {
            crate::audio::converter::dsp::lab::tfs::cached(taps, src_rate, out_rate).is_some()
                || (has("linear_phase") && has("minimum_phase"))
        }
        Phase::Hybrid | Phase::Alpha => has("linear_phase") && has("minimum_phase"),
    }
}

/// The length asked for when `has` it, else the nearest on the ladder that
/// is there — the shorter of two as near (a long filter keeps a stream
/// waiting). None: no length is there.
pub fn nearest_installed(asked: usize, has: impl Fn(usize) -> bool) -> Option<usize> {
    if has(asked) {
        return Some(asked);
    }
    let at = TAP_LADDER.iter().position(|&t| t >= asked).unwrap_or(TAP_LADDER.len() - 1) as i64;
    let mut by_distance: Vec<(i64, usize)> =
        TAP_LADDER.iter().enumerate().map(|(i, &t)| ((i as i64 - at).abs(), t)).collect();
    // Stable sort, shortest first among equals.
    by_distance.sort_by_key(|&(d, t)| (d, t));
    by_distance.into_iter().map(|(_, t)| t).find(|&t| has(t))
}

/// The filter length that plays rack `s` on a stream at `src_rate`: the
/// rack's own when it is installed for the rack's FS and phase, else the
/// nearest installed one. None: nothing is (the plan says which file is
/// missing).
pub fn installed_length(s: &PlayerSettings, src_rate: u32) -> Option<usize> {
    let (out_rate, _) = out_rate_for(src_rate, s.fs_multiplier).ok()?;
    nearest_installed(s.taps, |t| installed(s.phase, t, src_rate, out_rate))
}

/// What a stream's filter makes it wait for before its first sound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterWait {
    /// Seconds of the stream the convolver's first block needs (`plan`'s
    /// `first_need`) — and Hybrid-Phase's or alpha-HP's plan, when it reads
    /// further (`blend::live_plan_need`); the network's margin comes on top,
    /// the same for every length.
    pub need_s: f64,
    /// How far ahead of what is heard the chain reads, seconds: the filter's
    /// look-ahead (`Bank::delay`, 0 for minimum phase), or Hybrid-Phase's or
    /// alpha-HP's plan when it reads further.
    pub look_ahead_s: f64,
}

/// The wait of rack `s` (its `taps` the length that plays,
/// `installed_length`) on a stream at `src_rate`, reckoned as `plan`
/// reckons it, without loading the filter.
pub fn filter_wait(s: &PlayerSettings, src_rate: u32) -> Result<FilterWait, String> {
    let (out_rate, _) = out_rate_for(src_rate, s.fs_multiplier)?;
    Ok(wait_for(s.phase, s.taps, src_rate, out_rate, alignment(s.phase, s.taps, src_rate, out_rate)))
}

/// `filter_wait` with the filter lined up as `align`.
fn wait_for(phase: Phase, taps: usize, src_rate: u32, out_rate: u32, align: Alignment) -> FilterWait {
    let l = (out_rate / src_rate) as usize;
    let delay = align.look_ahead(taps).unwrap_or(0) as u64;
    let mut need = first_need(0, delay, l);
    let mut look = delay as f64 / out_rate as f64;
    if matches!(phase, Phase::Hybrid | Phase::Alpha) {
        let plan = live_plan_need(src_rate, out_rate as f64) as i64;
        need = need.max(plan);
        look = look.max(plan as f64 / src_rate as f64);
    }
    FilterWait { need_s: need as f64 / src_rate as f64, look_ahead_s: look }
}

/// The linear filter of `taps` a stream at `src_rate` plays at `out_rate`
/// (Hybrid-Phase's and alpha-HP's linear branch too), and how it is lined up:
/// the stream's own, made now if it has to be (`stream_linear::obtain`), else
/// the shipped one — when the stream's own cannot be had (a short filter, no
/// pair to make it from) or could not be made (memory, a failed check: said
/// in the log). Whether it is the stream's own comes third.
fn linear_filter(taps: usize, src_rate: u32, out_rate: u32) -> Result<(String, Alignment, bool), String> {
    match stream_linear::obtain(taps, src_rate, out_rate) {
        Ok(Some((path, k))) => return Ok((path, Alignment::LookAhead(k), true)),
        Ok(None) => {}
        Err(e) => crate::aelog!(
            "[RADIO] the stream's own {} linear filter at {} Hz could not be made ({}): the shipped one plays",
            taps_label(taps).unwrap_or("?"),
            out_rate,
            e
        ),
    }
    let path = find_precomputed_filter(taps, src_rate, out_rate, "linear_phase")
        .ok_or_else(|| missing_filter_error(taps, src_rate, out_rate, "linear_phase"))?;
    Ok((path, Alignment::Linear, false))
}

/// Make the stream's own linear filter ahead of the stream that will play it
/// (`stream_linear::prepare_ahead`): the one rack `s` plays (its `taps` the
/// length that plays, `installed_length`) on a stream at `src_rate`, when it
/// plays one; else what was being made ahead stops.
pub fn prepare_ahead(s: &PlayerSettings, src_rate: u32) {
    let want = match s.phase {
        // BIT-PERFECT: the stream plays as decoded; the rack's filter is made
        // when the rack comes back.
        _ if s.mode == crate::player::settings::Mode::Direct => None,
        Phase::Linear | Phase::Hybrid | Phase::Alpha => out_rate_for(src_rate, s.fs_multiplier).ok().map(|(out, _)| (s.taps, src_rate, out)),
        Phase::Minimum | Phase::Tfs => None,
    };
    stream_linear::prepare_ahead(want);
}

/// Plan the chain for rack `s` on a stream at `src_rate`, starting at output
/// index `start`: the banks are loaded here (the slow part), so this can run
/// while the stream fills. `s` is the rack, its length too; not installed
/// for this rate, FS and phase, the nearest installed length plays
/// (`installed_length`).
pub fn plan(res: &Resources, s: &PlayerSettings, src_rate: u32, start: u64) -> Result<LivePlan, String> {
    let (out_rate, fs) = out_rate_for(src_rate, s.fs_multiplier)?;
    let l = (out_rate / src_rate) as usize;
    if out_rate % src_rate != 0 {
        return Err(format!("{} Hz cannot be upsampled by an integer factor to {} Hz", src_rate, out_rate));
    }
    let taps = installed_length(s, src_rate).unwrap_or(s.taps);
    if taps != s.taps {
        crate::aelog!(
            "[RADIO] {} taps are not installed for {} Hz ({:?}): {} play",
            taps_label(s.taps).unwrap_or("?"),
            out_rate,
            s.phase,
            taps_label(taps).unwrap_or("?")
        );
    }
    let s = PlayerSettings { fs_multiplier: fs, taps, ..s.clone() };
    // The filter of the stream's factor: a hi-res stream's own
    // (`filter::design_rate`).
    let find = |phase: &str| {
        find_precomputed_filter(s.taps, src_rate, out_rate, phase)
            .ok_or_else(|| missing_filter_error(s.taps, src_rate, out_rate, phase))
    };
    let (path, align, phase_token, own) = match s.phase {
        Phase::Minimum => (find("minimum_phase")?, Alignment::None, Some("MIN"), false),
        Phase::Tfs => {
            let cancel = AtomicBool::new(false);
            let p = crate::audio::converter::dsp::lab::tfs::resolve_or_derive(s.taps, src_rate, out_rate, &cancel)?;
            let align = stream_alignment(s.phase, s.taps, out_rate, None);
            (p.to_string_lossy().to_string(), align, Some("TFS"), false)
        }
        Phase::Hybrid | Phase::Alpha | Phase::Linear => {
            let (p, align, own) = linear_filter(s.taps, src_rate, out_rate)?;
            let token = match s.phase {
                Phase::Hybrid => Some("HP"),
                Phase::Alpha => Some("aHP"),
                _ => None,
            };
            (p, align, token, own)
        }
    };
    // Hybrid-Phase and alpha-HP: the minimum-phase branch beside it, lined
    // up as the file chain lines it up — the two built side by side.
    let (bank, min_bank) = match s.phase {
        Phase::Hybrid | Phase::Alpha => {
            // Its linear branch lined up as that filter plays (the stream's
            // own: 50 ms ahead).
            let (lin, min) = res.pair_banks(&path, align, &find("minimum_phase")?, l, out_rate)?;
            (lin, Some(min))
        }
        _ => (res.bank(&path, l, align, out_rate)?, None),
    };
    let delay = bank.delay as u64;

    let guard = if s.subsonic_hz != 0 { Some(res.guard(out_rate, s.subsonic_hz)) } else { None };
    let guard_in = match &guard {
        Some(g) => start.saturating_sub(FirStage::history(g.len())),
        None => start,
    };
    let lim_in = if s.isp { guard_in.saturating_sub(LimiterStage::history(out_rate)) } else { guard_in };
    let xtc = if s.xtc_active() { Some(res.xtc_pair(&s, out_rate)?) } else { None };
    let conv_start = match &xtc {
        Some(p) => lim_in - XtcStage::history(p.0.len(), lim_in),
        None => lim_in,
    };
    // Hybrid-Phase's branches begin a lead-in early (`blend::lead_in`).
    let branch_start = if s.phase == Phase::Hybrid { conv_start.saturating_sub(lead_in(out_rate as f64)) } else { conv_start };
    // Until the adaptive headroom has decided (`decide_headroom`).
    let ceiling_db = if s.headroom_db < 0.0 { s.headroom_db } else { TARGET_TRUE_PEAK_DBTP };
    let source = SourcePlan::new(&s, src_rate);
    Ok(LivePlan {
        source,
        s,
        out_rate,
        l,
        bank,
        min_bank,
        branch_start,
        phase_token,
        stream_linear: own,
        xtc,
        guard,
        ceiling_db,
        ahr: None,
        start,
        conv_start,
        guard_in,
        lim_in,
        delay,
        first_need: first_need(branch_start, delay, l),
    })
}

/// The resampler's line on the chain of `tl` taps ×`l` at `out_rate`; with
/// the stream's own linear filter (`own`, looking `delay` output frames
/// ahead), what it is and what it saves.
fn pfr_line(tl: &str, l: usize, out_rate: u32, delay: u64, own: bool) -> String {
    if !own {
        return format!("{} taps ×{}: the FIR is the resampler", tl, l);
    }
    // Whole numbers as such, else to a tenth.
    let num = |x: f64| if (x - x.round()).abs() < 1e-9 { format!("{:.0}", x) } else { format!("{:.1}", x) };
    let src_rate = out_rate / l as u32;
    let up_to = stream_linear::linear_up_to_hz(src_rate, out_rate);
    let span = ((stream_linear::wall_hz(src_rate, out_rate) - up_to) / 1000.0).round().max(1.0);
    let last = if span == 1.0 { "the last kilohertz".to_string() } else { format!("the last {} kilohertz", num(span)) };
    format!(
        "{} taps ×{}, the FIR is the resampler: linear phase up to {} kHz, minimum phase only in {} below the wall — a stream waits {} ms for its filter, not half of it",
        tl,
        l,
        num(up_to / 1000.0),
        last,
        num(delay as f64 * 1000.0 / out_rate as f64)
    )
}

/// One convolver of a stream's chain: on the card when `ctx` takes it, else
/// on the CPU, where a Hybrid-Phase pair's two share their input spectra
/// (`share`; the same numbers, bit for bit).
fn conv_stage(bank: &Arc<Bank>, input: &Input, at: u64, ctx: Option<Arc<GpuPolyCtx>>, share: Option<Arc<SpectrumShare>>) -> (Box<dyn Stage>, bool) {
    match ctx.and_then(|c| GpuPolyStream::try_with_input(bank.clone(), input.clone(), at, c)) {
        Some(g) => (Box::new(g), true),
        None => (Box::new(PolyStream::with_input_shared(bank.clone(), input.clone(), at, share)), false),
    }
}

/// Build the planned chain on `live`. Its first block must be in (`first_need`
/// frames), or the build waits for the network. From the session (`shared`):
/// where the stream's songs begin (the slow gain starts afresh there), and
/// the meter of what the slow gain holds read ahead (the stream in hand);
/// the chain names the session for the analyzer.
pub fn build(
    p: &LivePlan,
    live: &Arc<LiveSource>,
    shared: &Arc<RadioShared>,
    gpu_ctx: Option<Arc<GpuPolyCtx>>,
    tally: Arc<Mutex<LimiterTally>>,
) -> Chain {
    let songs = shared.songs.clone();
    let input = Input::Live(live.clone());
    let (mut stage, gpu_on): (Box<dyn Stage>, bool) = match &p.min_bank {
        None => conv_stage(&p.bank, &input, p.conv_start, gpu_ctx, None),
        Some(min) => {
            // The pair on the card only when both fit, as the file chain's
            // phase stage decides.
            let ctx = gpu_ctx.filter(|c| c.free_vram_bytes() as f64 * 0.75 >= vram_demand_hp(p.bank.full_len, p.l) as f64);
            let share = Some(SpectrumShare::new());
            let (a, ga) = conv_stage(&p.bank, &input, p.branch_start, ctx.clone(), share.clone());
            let (b, gb) = conv_stage(min, &input, p.branch_start, ctx, share);
            // The plan grows with the stream: its onset envelope is read from
            // the live source as the branches read it.
            let feed = PlanFeed::new(input.clone(), live.rate(), p.out_rate as f64, p.branch_start as usize);
            let st: Box<dyn Stage> = if p.s.phase == Phase::Hybrid {
                Box::new(HybridStage::live(a, b, feed, p.out_rate as f64, p.conv_start))
            } else {
                Box::new(AlphaStage::live(a, b, feed))
            };
            (st, ga || gb)
        }
    };
    let tl = taps_label(p.s.taps).unwrap_or("?");
    let mut tokens: Vec<String> = vec!["LIVE".into()];
    tokens.extend(p.source.tokens());
    let mut stages: Vec<(String, u8, String)> = vec![(
        "LIVE".into(),
        CHAIN_RAN,
        "a live stream: the source stages run as it arrives; what needs the whole track is off".into(),
    )];
    stages.extend(p.source.stages());
    // The adaptive headroom: lit when it kept the shipped ceiling, as on a
    // file (its token only then).
    if let Some((kept, why)) = &p.ahr {
        if *kept {
            tokens.push("AHR".into());
        }
        stages.push(("AHR".into(), if *kept { CHAIN_RAN } else { CHAIN_DECLINED }, why.clone()));
    }
    tokens.extend([tl.to_string(), "PFR".into()]);
    stages.push(("PFR".into(), CHAIN_RAN, pfr_line(tl, p.l, p.out_rate, p.delay, p.stream_linear)));
    if let Some(t) = p.phase_token {
        tokens.push(t.into());
    }
    match p.s.phase {
        Phase::Hybrid => stages.push((
            "HP".into(),
            CHAIN_RAN,
            "linear phase through sustained passages, minimum phase across attacks, switched at a zero crossing: the onset envelope is made as the stream arrives".into(),
        )),
        Phase::Alpha => {
            stages.push(("HP".into(), CHAIN_RAN, "linear and minimum phase, blended by the attack envelope made as the stream arrives".into()));
            stages.push(("aHP".into(), CHAIN_RAN, "a per-sample crossfade driven by the HPSS envelope instead of a switch".into()));
        }
        _ => {}
    }
    if let Some(x) = &p.xtc {
        tokens.push("XTC".into());
        stages.push(("XTC".into(), CHAIN_RAN, "crosstalk cancelled for the measured listening triangle".into()));
        stage = Box::new(XtcStage::new(stage, &x.0, &x.1, p.lim_in));
    }
    // The level: a stream has no whole render to measure — the slow gain
    // looks ahead as far as the stream has come (a block short of the live
    // source's end, less the filter's look-ahead), the limiter after it.
    let target = 10f64.powf(p.ceiling_db / 20.0);
    let (lv, l, delay) = (live.clone(), p.l as u64, p.delay);
    // A song start at source frame f is output index f·L.
    let sg = SlowGainStage::new(stage, target, p.out_rate)
        .with_horizon(move || ((lv.end().max(0) as u64).saturating_sub(BLOCK as u64) * l).saturating_sub(delay))
        .with_song_starts(move |i| songs.first_from(i.div_ceil(l) as i64).map(|f| f.max(0) as u64 * l))
        .with_queue_meter(shared.gain_queue.clone());
    let live_gain = Some(sg.live_gain());
    stage = Box::new(sg);
    if p.s.isp {
        tokens.push("ISP-L".into());
        stages.push(("ISP".into(), CHAIN_RAN, format!("the output limiter holds the overs at {:.1} dBTP", p.ceiling_db)));
        stage = Box::new(LimiterStage::new(stage, target, p.out_rate, p.guard_in).with_tally(tally));
    }
    if let Some(g) = &p.guard {
        tokens.push(format!("SUB{}-G", p.s.subsonic_hz));
        stages.push(("SUB".into(), CHAIN_RAN, "the subsonic guard on the output".into()));
        stage = Box::new(FirStage::new(stage, g, p.out_rate, p.start));
    }
    skip_to(stage.as_mut(), p.start);
    Chain {
        live_gain,
        stage,
        track_id: RADIO_TRACK_ID,
        out_rate: p.out_rate,
        l: p.l,
        start: p.start,
        tokens,
        gain: 1.0,
        tp_pred_db: 0.0,
        direct: false,
        disk_src: false,
        disk_file: None,
        variant_quick: false,
        notes: vec![],
        stages,
        gpu_on,
        downgrade: None,
        downgrade_gen: 0,
        // Its render time includes waits for the network: kept out of the
        // calibration store.
        calib_key: String::new(),
        hp_deferred: false,
        settings: Arc::new(p.s.clone()),
        variant: std::sync::Weak::new(),
        radio: Arc::downgrade(shared),
    }
}

/// BIT-PERFECT on a stream: the live source's shadow — the stream as
/// decoded, before the source stages — from its frame on, nothing done to
/// it. It waits for the network as a convolver does (`LiveSource::ensure`:
/// a starved stream reads the silence it was given), and goes on as long as
/// the stream does.
struct LivePass {
    live: Arc<LiveSource>,
    pos: u64,
}

impl Stage for LivePass {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len().min(out_r.len());
        let from = self.pos as i64;
        self.live.ensure(from + n as i64);
        self.live.read_raw(0, from, &mut out_l[..n]);
        self.live.read_raw(1, from, &mut out_r[..n]);
        self.pos += n as u64;
        n
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn total(&self) -> u64 {
        u64::MAX
    }
}

/// The chain BIT-PERFECT plays a stream with: the shadow from source frame
/// `start`, at the stream's rate, to the device as it is — no filter, no
/// level, no volume — as a file's direct variant plays (`build_chain`).
/// `s`: the rack as set (BIT-PERFECT on); `radio`: the session it plays.
pub fn build_direct(live: &Arc<LiveSource>, start: u64, s: &PlayerSettings, radio: std::sync::Weak<RadioShared>) -> Chain {
    Chain {
        live_gain: None,
        stage: Box::new(LivePass { live: live.clone(), pos: start }),
        track_id: RADIO_TRACK_ID,
        out_rate: live.rate(),
        l: 1,
        start,
        tokens: vec!["DIRECT".into()],
        gain: 1.0,
        tp_pred_db: 0.0,
        direct: true,
        disk_src: false,
        disk_file: None,
        variant_quick: false,
        notes: vec![],
        stages: vec![],
        gpu_on: false,
        downgrade: None,
        downgrade_gen: 0,
        calib_key: String::new(),
        hp_deferred: false,
        settings: Arc::new(s.clone()),
        variant: std::sync::Weak::new(),
        radio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIT-PERFECT's chain on a stream plays the shadow from its frame, to
    /// the bit — not what the source stages made of it — at the stream's
    /// rate, as a direct chain; past the stream's end (it closed) silence.
    #[test]
    fn bit_perfects_chain_plays_the_shadow_from_its_frame() {
        let live = Arc::new(LiveSource::new(48_000, 1 << 30, 0.5, 600.0));
        let raw: Vec<f64> = (0..10_000).map(|i| ((i as f64) * 0.377).sin() * 0.9).collect();
        let staged: Vec<f64> = raw.iter().map(|v| v * 0.5).collect();
        live.push_raw(&raw, &raw);
        live.push(&staged, &staged);
        live.close();
        let mut c = build_direct(&live, 1_234, &PlayerSettings::default(), std::sync::Weak::new());
        assert!(c.direct && c.l == 1 && c.out_rate == 48_000 && c.track_id == RADIO_TRACK_ID);
        assert_eq!((c.start, c.stage.position()), (1_234, 1_234));
        let (mut l, mut r) = (vec![9.0; 9_000], vec![9.0; 9_000]);
        assert_eq!(c.stage.read(&mut l, &mut r), 9_000);
        assert_eq!(c.stage.position(), 10_234);
        for i in 0..9_000 {
            let want = if 1_234 + i < 10_000 { raw[1_234 + i] } else { 0.0 };
            assert!(l[i].to_bits() == want.to_bits() && r[i].to_bits() == want.to_bits(), "frame {}", 1_234 + i);
        }
    }

    /// A stream's Hybrid-Phase pair on the CPU, its spectra shared, plays
    /// what the pair with its own plays — bit for bit, on a live stream that
    /// starves midway; both pairs read one source, in turns.
    #[test]
    fn a_streams_pair_sharing_its_spectra_plays_what_it_played() {
        use super::super::live::STARVE_WAIT;
        use rand::{Rng, SeedableRng};
        let noise = |n: usize, seed: u64| -> Vec<f64> {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect()
        };
        let (l, out_rate) = (4usize, 176_400u32);
        let taps = 3 * BLOCK * l + 1001;
        let mut h = noise(taps, 41);
        for i in 0..taps / 2 {
            h[taps - 1 - i] = h[i];
        }
        let h: Vec<f64> = h.iter().map(|v| v * 1e-3).collect();
        let m: Vec<f64> = noise(2 * BLOCK * l - 3, 42).iter().enumerate().map(|(i, v)| v * 1e-3 * (-(i as f64) / 40_000.0).exp()).collect();
        let lin = Arc::new(Bank::from_coeffs("lin", &h, l, Alignment::Linear, out_rate));
        let min = Arc::new(Bank::from_coeffs("min", &m, l, Alignment::BandWeighted, out_rate));
        let n_in = 7 * BLOCK + 333;
        let x_l = noise(n_in, 43);
        let x_r = noise(n_in, 44);
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 0.2, 1e6));
        let w = live.clone();
        let writer = std::thread::spawn(move || {
            let cut = 3 * BLOCK;
            w.push(&x_l[..cut], &x_r[..cut]);
            // Stopped until the readers ran into the edge and the stream
            // starved, however long they take to get there.
            let t0 = std::time::Instant::now();
            while w.stats().starves == 0 && t0.elapsed() < STARVE_WAIT * 100 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let (mut at, mut step) = (cut, 4_001);
            while at < x_l.len() {
                let e = (at + step).min(x_l.len());
                w.push(&x_l[at..e], &x_r[at..e]);
                at = e;
                step = step * 5 / 4 + 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            w.close();
        });
        let input = Input::Live(live.clone());
        let start = 0u64;
        let pair = |share: Option<Arc<SpectrumShare>>| -> HybridStage {
            let (a, _) = conv_stage(&lin, &input, start, None, share.clone());
            let (b, _) = conv_stage(&min, &input, start, None, share);
            let feed = PlanFeed::new(input.clone(), 44_100, out_rate as f64, start as usize);
            HybridStage::live(a, b, feed, out_rate as f64, start)
        };
        let mut shared = pair(Some(SpectrumShare::new()));
        let mut own = pair(None);
        let want = n_in * l;
        let (mut sl, mut sr, mut ol, mut or) = (vec![0.0; want], vec![0.0; want], vec![0.0; want], vec![0.0; want]);
        let (mut off, mut step) = (0, 6_007);
        while off < want {
            let e = (off + step).min(want);
            shared.read(&mut sl[off..e], &mut sr[off..e]);
            own.read(&mut ol[off..e], &mut or[off..e]);
            off = e;
            step = step * 3 / 2 + 1;
        }
        writer.join().unwrap();
        assert!(live.stats().starves >= 1, "the stream starved");
        for i in 0..want {
            assert!(
                sl[i].to_bits() == ol[i].to_bits() && sr[i].to_bits() == or[i].to_bits(),
                "idx {}: shared ({}, {}) != own ({}, {})", i, sl[i], sr[i], ol[i], or[i]
            );
        }
    }

    #[test]
    fn the_output_rate_follows_the_family_and_steps_over_the_source() {
        assert_eq!(out_rate_for(44_100, 2), Ok((88_200, 2)));
        assert_eq!(out_rate_for(48_000, 8), Ok((384_000, 8)));
        // A 96 kHz stream at ×2 would not be upsampled: one step up.
        assert_eq!(out_rate_for(96_000, 2), Ok((192_000, 4)));
        assert_eq!(out_rate_for(96_000, 8), Ok((384_000, 8)));
        assert!(out_rate_for(22_050, 2).is_err(), "an HE-AAC core rate is not a family");
    }

    #[test]
    fn a_length_not_installed_gives_way_to_the_nearest_shorter_first() {
        let only = |set: &'static [usize]| move |t: usize| set.contains(&t);
        // Installed: the length asked for plays.
        assert_eq!(nearest_installed(1_000_000, only(&[1_000_000, 30_000_000])), Some(1_000_000));
        // 1M missing, 5k and 5M as near: the shorter keeps the stream waiting less.
        assert_eq!(nearest_installed(1_000_000, only(&[5_000, 5_000_000])), Some(5_000));
        assert_eq!(nearest_installed(1_000_000, only(&[5_000_000, 30_000_000])), Some(5_000_000));
        assert_eq!(nearest_installed(30_000_000, only(&[1_000_000, 10_000_000])), Some(10_000_000));
        assert_eq!(nearest_installed(5_000, only(&[10_000_000])), Some(10_000_000));
        assert_eq!(nearest_installed(1_000_000, only(&[])), None);
    }

    #[test]
    fn the_filter_wait_is_the_plans_first_block() {
        let at = |phase: Phase, taps: usize, fs: u32| {
            let s = PlayerSettings { phase, fs_multiplier: fs, taps, ..PlayerSettings::default() };
            filter_wait(&s, 44_100).unwrap()
        };
        // 30M linear at FS×8: the look-ahead is half the filter, 42.5 s; the
        // first piece needs it whole, rounded up to pieces (HEAD_CHUNK).
        let w = at(Phase::Linear, 30_000_000, 8);
        assert!((w.look_ahead_s - 14_999_999.0 / 352_800.0).abs() < 1e-9, "{:?}", w);
        assert_eq!(w.need_s, first_need(0, 14_999_999, 8) as f64 / 44_100.0);
        assert!(w.need_s >= w.look_ahead_s && w.need_s <= w.look_ahead_s + HEAD_CHUNK as f64 / 44_100.0);
        // Hybrid-Phase and alpha-HP wait as linear phase does.
        for p in [Phase::Hybrid, Phase::Alpha] {
            assert_eq!(at(p, 30_000_000, 8), w);
        }
        // 1M linear at FS×8 under a second and a half; at FS×2 the same
        // length spans four times as long.
        assert!((at(Phase::Linear, 1_000_000, 8).look_ahead_s - 1.417).abs() < 1e-3);
        assert!((at(Phase::Linear, 1_000_000, 2).look_ahead_s - 5.669).abs() < 1e-3);
        // TFS looks 30 ms ahead at any length and FS: one piece.
        for (taps, fs) in [(30_000_000, 8), (1_000_000, 8), (1_000_000, 2), (5_000_000, 16)] {
            let t = at(Phase::Tfs, taps, fs);
            assert!((t.look_ahead_s - 0.030).abs() < 1e-12, "{taps} ×{fs}: {t:?}");
            assert_eq!(t.need_s, HEAD_CHUNK as f64 / 44_100.0, "{taps} ×{fs}");
        }
        // Minimum phase does not look ahead: one piece, 93 ms (a whole block
        // was 0.74 s).
        let m = at(Phase::Minimum, 30_000_000, 8);
        assert_eq!((m.look_ahead_s, m.need_s), (0.0, HEAD_CHUNK as f64 / 44_100.0));
        // Hybrid-Phase on 5k: its plan reads further ahead than the filter.
        let plan = live_plan_need(44_100, 352_800.0) as f64 / 44_100.0;
        for p in [Phase::Hybrid, Phase::Alpha] {
            let hp = at(p, 5_000, 8);
            assert!((hp.look_ahead_s - plan).abs() < 1e-12, "{p:?}: {hp:?}");
            assert_eq!(hp.need_s, at(Phase::Linear, 5_000, 8).need_s.max(plan));
        }
    }

    /// With its own linear filter a stream waits 50 ms for it, at any length:
    /// 30M ×8 one piece (93 ms) instead of 42.5 s. A hi-res stream plays its factor's
    /// file, whose 50 ms at the blob's rate are 25 ms at its own. Hybrid-Phase
    /// and alpha-HP then wait for their plan (~0.3 s), not for the filter;
    /// minimum phase and TFS are as they were.
    #[test]
    fn a_streams_own_linear_filter_waits_50_ms() {
        use crate::audio::converter::dsp::filter::design_rate;
        let wait = |phase: Phase, taps: usize, src: u32, out: u32| {
            let k = stream_linear::look_ahead(taps, design_rate(src, out));
            wait_for(phase, taps, src, out, stream_alignment(phase, taps, out, k))
        };
        let w = wait(Phase::Linear, 30_000_000, 44_100, 352_800);
        assert!((w.look_ahead_s - 0.050).abs() < 1e-12, "{w:?}");
        assert_eq!(w.need_s, first_need(0, 17_640, 8) as f64 / 44_100.0);
        assert_eq!(w.need_s, HEAD_CHUNK as f64 / 44_100.0, "one piece");
        let w = wait(Phase::Linear, 1_000_000, 48_000, 384_000);
        assert!((w.look_ahead_s - 0.050).abs() < 1e-12, "{w:?}");
        let w = wait(Phase::Linear, 1_000_000, 96_000, 384_000);
        assert!((w.look_ahead_s - 0.025).abs() < 1e-12, "hi-res: {w:?}");
        // 5k has no filter of its own to play: its half is shorter.
        let w = wait(Phase::Linear, 5_000, 44_100, 352_800);
        assert!((w.look_ahead_s - 2_499.0 / 352_800.0).abs() < 1e-12, "{w:?}");
        let plan = live_plan_need(44_100, 352_800.0);
        for p in [Phase::Hybrid, Phase::Alpha] {
            let w = wait(p, 30_000_000, 44_100, 352_800);
            assert!((w.look_ahead_s - plan as f64 / 44_100.0).abs() < 1e-12, "{p:?}: {w:?}");
            assert_eq!(w.need_s, first_need(0, 17_640, 8).max(plan as i64) as f64 / 44_100.0);
        }
        assert!(matches!(stream_alignment(Phase::Minimum, 30_000_000, 352_800, Some(17_640)), Alignment::None));
        assert!(matches!(stream_alignment(Phase::Tfs, 30_000_000, 352_800, Some(17_640)), Alignment::LookAhead(10_584)));
        assert!(matches!(stream_alignment(Phase::Linear, 30_000_000, 352_800, None), Alignment::Linear));
    }

    /// A stream with its own linear filter plays what the shipped one plays,
    /// in the band: one noise through both at ×2 and ×4 (a pair made as the
    /// shipped ones are, the stream's own made from it), each lined up by its
    /// trim; their difference in 20 Hz – 19.9 kHz is below −144 dBFS (a full-
    /// scale sine's line is 0). Trimmed a frame off, it is not.
    #[test]
    fn the_streams_linear_filter_plays_the_shipped_one_in_the_band() {
        use crate::audio::converter::dsp::lab::stream_linear::fixtures::{cepstral, designed};
        use crate::player::convolver::{PolyStream, SourceBuf};
        use rand::{Rng, SeedableRng};
        use rustfft::{num_complex::Complex, FftPlanner};
        for (l, n_log2, seg_log2) in [(2usize, 17u32, 17u32), (4, 18, 18)] {
            let (src, out) = (44_100u32, 44_100 * l as u32);
            let n = 1usize << n_log2;
            let lin = designed(n, out);
            let (own, _) = stream_linear::derive(&lin, &cepstral(&lin), out, &AtomicBool::new(false)).expect("made");
            let k = stream_linear::look_ahead(n, out).unwrap();
            let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
            let frames = 3 * src as usize;
            let x: Vec<f64> = (0..frames).map(|_| rng.gen_range(-0.5..0.5)).collect();
            let file = Arc::new(SourceBuf { l: x.clone(), r: x, rate: src });
            let total = frames * l;
            let play = |coeffs: &[f64], align: Alignment| {
                let bank = Arc::new(Bank::from_coeffs("test", coeffs, l, align, out));
                let mut st = PolyStream::new(bank, file.clone(), 0);
                let (mut a, mut b) = (vec![0.0; total], vec![0.0; total]);
                st.read(&mut a, &mut b);
                a
            };
            let shipped = play(&lin, Alignment::Linear);
            let seg = 1usize << seg_log2;
            let from = total / 2 - seg / 2;
            let w = crate::player::analytics::spectra::kaiser(seg, 20.0);
            let full_scale = w.iter().sum::<f64>() / 2.0;
            let in_band_dbfs = |y: &[f64]| {
                let mut buf: Vec<Complex<f64>> = (0..seg).map(|i| Complex::new((y[from + i] - shipped[from + i]) * w[i], 0.0)).collect();
                FftPlanner::<f64>::new().plan_fft_forward(seg).process(&mut buf);
                let worst = (1..seg / 2)
                    .filter(|&b| (20.0..=19_900.0).contains(&(b as f64 * out as f64 / seg as f64)))
                    .map(|b| buf[b].norm() / full_scale)
                    .fold(0.0f64, f64::max);
                20.0 * worst.max(1e-300).log10()
            };
            let lined_up = in_band_dbfs(&play(&own, Alignment::LookAhead(k)));
            assert!(lined_up < -144.0, "×{l}: {lined_up:.1} dBFS in the band");
            let off = in_band_dbfs(&play(&own, Alignment::LookAhead(k + 1)));
            assert!(off > -100.0, "×{l}: a frame off, {off:.1} dBFS");
            eprintln!("×{l}: {lined_up:.1} dBFS in the band; a frame off {off:.1}");
        }
    }

    /// The resampler's line says what the stream's own linear filter is: the
    /// line the master set for 1M ×8, its numbers from the filter's family and
    /// factor; the shipped filter keeps the line it had.
    #[test]
    fn the_resamplers_line_says_what_the_streams_linear_filter_is() {
        assert_eq!(
            pfr_line("1M", 8, 352_800, 17_640, true),
            "1M taps ×8, the FIR is the resampler: linear phase up to 20 kHz, minimum phase only in the last kilohertz below the wall — a stream waits 50 ms for its filter, not half of it"
        );
        assert_eq!(
            pfr_line("30M", 8, 384_000, 19_200, true),
            "30M taps ×8, the FIR is the resampler: linear phase up to 22 kHz, minimum phase only in the last kilohertz below the wall — a stream waits 50 ms for its filter, not half of it"
        );
        // 96 kHz ×4: the ×4 blob's crossover at its own Nyquist's fraction.
        assert_eq!(
            pfr_line("1M", 4, 384_000, 9_600, true),
            "1M taps ×4, the FIR is the resampler: linear phase up to 44 kHz, minimum phase only in the last 2 kilohertz below the wall — a stream waits 25 ms for its filter, not half of it"
        );
        assert_eq!(pfr_line("1M", 8, 352_800, 499_999, false), "1M taps ×8: the FIR is the resampler");
    }

    fn stats(peak_dbfs: f64, enob: Option<f64>) -> SourceStats {
        SourceStats { peak_lin: 10f64.powf(peak_dbfs / 20.0), peak_dbfs, hist: vec![], grid16: false, enob }
    }

    #[test]
    fn the_adaptive_headroom_decides_on_the_streams_start_as_on_a_file() {
        let s = PlayerSettings { headroom_db: -3.0, adaptive_headroom: true, ..PlayerSettings::default() };
        // A start that peaks below the asked ceiling: lowering it costs level
        // for nothing, the shipped ceiling is kept (lit).
        let (c, ahr) = headroom_for(&s, Some(&stats(-4.2, None)), 10.0);
        assert_eq!(c, -0.5);
        assert_eq!(
            ahr,
            Some((true, "ceiling kept at -0.5 dBTP instead of -3.0: the stream's first 10 s peak at -4.2 dBFS".into()))
        );
        // A loud start: the ceiling comes down as asked (the lamp off).
        let (c, ahr) = headroom_for(&s, Some(&stats(-0.1, None)), 8.3);
        assert_eq!(c, -3.0);
        assert_eq!(ahr, Some((false, "ceiling lowered to -3.0 dBTP as asked: the stream's first 8 s peak at -0.1 dBFS".into())));
        // A pristine source with a modest peak: kept, and the ENOB said why.
        let (c, ahr) = headroom_for(&s, Some(&stats(-2.0, Some(21.3))), 10.0);
        assert_eq!(c, -0.5);
        assert!(ahr.unwrap().1.ends_with("peak at -2.0 dBFS, ENOB 21.3"));
        // Silence so far: nothing to lower.
        let (c, ahr) = headroom_for(&s, Some(&stats(f64::NEG_INFINITY, None)), 10.0);
        assert_eq!((c, ahr.unwrap().1.ends_with("first 10 s are silent")), (-0.5, true));
        // The adaptive headroom off: the asked ceiling, no verdict; Headroom
        // off: the shipped one.
        let off = PlayerSettings { adaptive_headroom: false, ..s.clone() };
        assert_eq!(headroom_for(&off, Some(&stats(-4.2, None)), 10.0), (-3.0, None));
        let none = PlayerSettings { headroom_db: 0.0, ..s };
        assert_eq!(headroom_for(&none, Some(&stats(-4.2, None)), 10.0), (-0.5, None));
    }
}
