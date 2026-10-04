//! The render thread: keeps the timeline filled from the current chain and
//! swaps chains without a gap.
//!
//! A swap arrives as a chain positioned at some output index of its track.
//! For a settings change the thread works out which timeline frame that
//! index belongs to (waiting, if need be, until the old chain has rendered
//! that far), reads the frames it is about to replace, crossfades them into
//! the new chain over `XFADE_MS` with the engine's `switch_fade` curve, and
//! rewrites the timeline from there. A seek or a new track goes in at the
//! earliest frame the reader allows. Either way the listener hears the
//! change a fraction of a second after asking for it, not after the whole
//! look-ahead has drained.
//!
//! Marks map timeline frames back to (track, output index), so the
//! controller can show a position and start a new chain at exactly the
//! sample the old one would have played next.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::calibration::{CalibHandle, new_handle};
use super::chain::{Chain, DowngradeInfo};
use super::convolver::BLOCK;
use super::output::OutputShared;
use super::settings::PlayerSettings;
use super::timeline::Timeline;

/// Crossfade length for a continuous settings change (same track, same position).
pub const XFADE_MS: f64 = 30.0;
/// Fade-in length for a jump (new track or seek): short enough to be
/// inaudible, long enough to avoid a click.
const JUMP_FADE_MS: f64 = 8.0;
/// Pre-roll for async seek: frames pre-rendered in the spawn thread before the
/// crossfade fires (total side_buf length = PRE_ROLL_SEEK_MS + XFADE_MS).
/// Gives enough buffer that the output thread never starves before the splice.
pub const PRE_ROLL_SEEK_MS: f64 = 300.0;
/// A redraw goes in only when at most this much of the old chain lies between
/// its end and the write head: the render thread draws that part with the new
/// chain after the splice, so the buffer it leaves is at most this much
/// thinner than before. A redraw the old chain ran further past is declined
/// and the swap goes in at the write head as before.
pub const REDRAW_MAX_TAIL_MS: f64 = 250.0;
/// Frames rendered per step (the prewarm reads a chain's first step ahead).
pub(super) const STEP: usize = 8192;
/// Initial render look-ahead when no learned value exists. The adaptive
/// buffer controller adjusts this up or down to match the machine.
const INITIAL_AHEAD_S: f64 = 1.5;
/// Minimum look-ahead: must cover the guard, two render steps and a margin.
const MIN_AHEAD_S: f64 = 0.6;
/// Maximum look-ahead: 75 % of the timeline capacity.
const MAX_AHEAD_FRAC: f64 = 0.75;

/// One stage in the audible chain: token, state (1=ran, 2=declined, 3=failed)
/// and the human-readable why text.
#[derive(Clone, Debug)]
pub struct StageInfo {
    pub tok: String,
    pub st: u8,
    pub why: String,
}

/// A snapshot of the chain that is audible at a given Mark. The FE-P layer
/// reads this to render the correct badges with their tooltips.
#[derive(Clone, Debug)]
pub struct ChainDesc {
    /// "live" = the rack rendered now; "file" = a converted file played from
    /// disk; "direct" = BIT-PERFECT pass-through of the original.
    pub source: &'static str,
    /// The quick variant (decode and DC removal only) is what is heard,
    /// while the full source stages are still being prepared.
    pub quick: bool,
    /// Filter size label ("30M", "1M", …); null for direct/file.
    pub taps: Option<String>,
    /// The pipeline stages in order.
    pub stages: Vec<StageInfo>,
    /// True-peak gain applied (≤ 0 dB; 0 = no gain change).
    pub gain_db: f64,
    /// Predicted true peak of the source variant, dBTP.
    pub tp_db: Option<f64>,
    /// The output ceiling the chain targets, dBTP.
    pub ceiling_db: Option<f64>,
    /// True when the convolver stage in this chain ran on the GPU.
    pub gpu_on: bool,
    /// Set when the chain was automatically reduced from a larger tap count.
    pub downgrade: Option<DowngradeInfo>,
    /// Value of `Shared.gpu_fallback_gen` at the time this chain was built
    /// (for the K6 `downgrade.gen` field in the status JSON).
    pub downgrade_gen: u32,
    /// True when this chain substitutes linear phase for a deferred HP/αHP.
    pub hp_deferred: bool,
    /// The chain plays a live stream (the radio's), whose linear filter is
    /// its own when it can have it (`stream_linear`).
    pub stream: bool,
    /// The converted file a disk chain plays.
    pub file: Option<String>,
    // analytics: §2.5 — effective settings for resp.rs / chain_info.rs
    pub settings: Arc<PlayerSettings>,
    // analytics: §2.5 — output sample rate for resp.rs
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub out_rate: u32,
    /// The chain's upsampling factor: FS for a 44.1/48 kHz source, less for a
    /// hi-res one (`out_rate` / the source rate).
    pub l: usize,
    /// The variant this chain plays (analytics: the source of what is heard).
    pub variant: std::sync::Weak<super::chain::Variant>,
    /// The radio session a stream's chain plays (`Chain::radio`).
    pub radio: std::sync::Weak<super::radio::RadioShared>,
}

impl ChainDesc {
    pub fn from_chain(c: &Chain) -> Arc<ChainDesc> {
        // The taps chip: the chain's token that is a rung of the ladder
        // ("30M", "1M", …), read off the ladder itself so a build with more
        // rungs names them too.
        use crate::audio::converter::dsp::filter::{taps_label, TAP_LADDER};
        let taps = c
            .tokens
            .iter()
            .find(|t| TAP_LADDER.iter().any(|&n| taps_label(n) == Some(t.as_str())))
            .cloned();

        // The stages exactly as build_chain listed them — the same tokens,
        // states and words as the converter's row for the same file.
        let stages: Vec<StageInfo> = c
            .stages
            .iter()
            .map(|(tok, st, why)| StageInfo { tok: tok.clone(), st: *st, why: why.clone() })
            .collect();

        let gain_db = 20.0 * c.gain.max(1e-12).log10();
        let tp_db = if c.direct { None } else { Some(c.tp_pred_db) };
        // Ceiling is target — what the gain holds the peak to.
        let ceiling_db = if c.direct || c.gain >= 1.0 { None } else {
            Some(c.tp_pred_db + gain_db)   // tp_pred + gain_applied = ceiling
        };

        Arc::new(ChainDesc {
            source: if c.disk_src { "file" } else if c.direct { "direct" } else { "live" },
            quick: c.variant_quick,
            taps,
            stages,
            gain_db,
            tp_db,
            ceiling_db,
            gpu_on: c.gpu_on,
            downgrade: c.downgrade.clone(),
            downgrade_gen: c.downgrade_gen,
            hp_deferred: c.hp_deferred,
            stream: c.track_id == super::radio::chain::RADIO_TRACK_ID,
            file: c.disk_file.clone(),
            // analytics: §2.5
            settings: c.settings.clone(),
            out_rate: c.out_rate,
            l: c.l,
            variant: c.variant.clone(),
            radio: c.radio.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub frame: u64,
    pub track_id: u64,
    pub index: u64,
    pub rate: u32,
    #[allow(dead_code)]
    pub l: usize,
    /// The chain that is audible from this Mark onward.
    pub desc: Arc<ChainDesc>,
}

pub enum Msg {
    /// Start rendering into a (fresh) timeline.
    Start { chain: Chain, timeline: Arc<Timeline> },
    /// Replace the current chain.
    /// `continuous`: new chain continues the same track position (settings
    /// change); false = jump (new track) or seek.
    /// `seek`: async seek — old audio kept playing during pre-roll; the
    /// side_buf carries `PRE_ROLL_SEEK_MS` of pre-rendered new audio and the
    /// crossfade is equal-power (no hold/jump_frame protocol unless the output
    /// is already held by a concurrent BIT-PERFECT switch).
    Swap {
        chain: Chain,
        continuous: bool,
        seek: bool,
        /// Pre-rendered seek audio (left, right), present when seek=true.
        /// Length = PRE_ROLL_SEEK_MS + XFADE_MS frames at chain.out_rate.
        side_buf: Option<(Vec<f64>, Vec<f64>)>,
        requested: Instant,
    },
    /// Play this chain right after the current one ends (gapless). `seq`
    /// numbers the sends: `Shared::queue_taken` says which one the thread
    /// has received.
    Queue { chain: Chain, seq: u64 },
    /// A continuous swap with the new chain's sound already drawn over the
    /// buffered audio of the chain it replaces (`redraw.over`): it goes in
    /// near the reader instead of at the write head. Declined (too late, the
    /// timeline moved on, a BIT-PERFECT stream), it is a plain continuous
    /// Swap of `chain`, which stands at the redraw's end.
    Redraw { chain: Chain, redraw: Redraw, requested: Instant },
    /// Forget the queued chain (the repeat mode changed under it).
    Unqueue,
    Stop,
}

/// What the controller reads without talking to the thread.
pub struct Shared {
    pub marks: Mutex<VecDeque<Mark>>,
    pub tokens: Mutex<Vec<String>>,
    pub notes: Mutex<Vec<String>>,
    pub gain: Mutex<f64>,
    /// The slow gain of the chain on the air, when it plays one.
    pub live_gain: Mutex<Option<Arc<AtomicU64>>>,
    pub tp_pred_db: Mutex<f64>,
    pub quick: AtomicBool,
    /// Audio seconds rendered per second of render work (last few seconds).
    pub rtf_milli: AtomicU64,
    /// The current chain has produced its last sample at this frame.
    pub ended_at: AtomicU64,
    /// Whether a queued chain is waiting.
    pub queued: AtomicBool,
    /// The `seq` of the last Msg::Queue this thread has received (taken,
    /// queued, or let go by a swap after): one sent later is on its way.
    pub queue_taken: AtomicU64,
    /// A swap is waiting to be placed.
    pub swap_pending: AtomicBool,
    /// A new chain is placed in the timeline but not yet read by the output
    /// (its first frame not heard): the sound is still the old one.
    pub swap_unheard: AtomicBool,
    /// Settings change → first new frame read by the output, milliseconds.
    pub last_switch_ms: AtomicU64,
    /// Switches heard so far (a new chain's first frame read by the output).
    /// What tells one switch from the next: two can take the same number of
    /// milliseconds, and `last_switch_ms` then does not change.
    pub switches: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub alive: AtomicBool,
    /// Adaptive buffer target (seconds). Initialised from the persisted value;
    /// updated as the machine's real-time factor is observed.
    pub target_ahead_s: Mutex<f64>,
    /// The visualizations' floor under the look-ahead (seconds, f64 bits; 0:
    /// none). While the big player shows a picture drawn from the sound to
    /// come, the render keeps at least this much ahead (Anton 27.09: "the
    /// visualizer foresees the sound, 2–3 s"). Only how far to render reads
    /// it: the adaptive target, its saved value and the power decisions do
    /// not.
    pub vis_floor_bits: AtomicU64,
    /// True from the moment seek_now spawns the prepare thread until the
    /// SeekReady job arrives at the worker. Drives status.jump.pending.
    pub seek_pending: AtomicBool,
    /// The latest requested seek target, as f64 bits (NAN when none pending).
    /// Updated by each new seek call; stale SeekReady jobs are discarded by
    /// the generation check so the page always sees the last requested target.
    pub seek_target_bits: AtomicU64,
    /// Set by the render thread when a GPU convolver stream reports an error.
    /// Never cleared within a track session (K4: "never removed").
    pub gpu_failed: AtomicBool,
    /// Set by tick() after it dispatches the GPU→CPU fallback swap (or after
    /// build_chain fails inside the handler).  Prevents tick() from retrying
    /// every 200 ms.  Cleared alongside `gpu_failed` at track start/stop/seek.
    pub gpu_swap_sent: AtomicBool,
    /// Incremented by the controller each time it performs a GPU→CPU fallback
    /// swap.  Used as the `gen` counter in the K6 `downgrade` JSON field.
    pub gpu_fallback_gen: AtomicU32,
    /// Timeline frame at which the GPU stream first produced zeros (set by the
    /// render thread in the same pass that sets `gpu_failed`).  The K4 handler
    /// reads this to cap the CPU-fallback splice so it never lands past the
    /// first GPU zero even when the buffer is below its target (e.g. right
    /// after a seek).  u64::MAX = not yet known.  Cleared alongside `gpu_failed`.
    pub gpu_zeros_from: AtomicU64,
    /// Shared calibration store for live block-cost updates (K2).
    pub calibration: CalibHandle,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            marks: Mutex::new(VecDeque::new()),
            tokens: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            gain: Mutex::new(1.0),
            live_gain: Mutex::new(None),
            tp_pred_db: Mutex::new(0.0),
            quick: AtomicBool::new(false),
            rtf_milli: AtomicU64::new(0),
            ended_at: AtomicU64::new(u64::MAX),
            queue_taken: AtomicU64::new(0),
            queued: AtomicBool::new(false),
            swap_pending: AtomicBool::new(false),
            swap_unheard: AtomicBool::new(false),
            last_switch_ms: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            error: Mutex::new(None),
            alive: AtomicBool::new(false),
            target_ahead_s: Mutex::new(INITIAL_AHEAD_S),
            vis_floor_bits: AtomicU64::new(0f64.to_bits()),
            seek_pending: AtomicBool::new(false),
            seek_target_bits: AtomicU64::new(f64::NAN.to_bits()),
            gpu_failed: AtomicBool::new(false),
            gpu_swap_sent: AtomicBool::new(false),
            gpu_fallback_gen: AtomicU32::new(0),
            gpu_zeros_from: AtomicU64::new(u64::MAX),
            calibration: new_handle(),
        })
    }

    /// (track, output index) at timeline frame `f`.
    pub fn locate(&self, f: u64) -> Option<Mark> {
        let g = self.marks.lock().unwrap();
        let m = g.iter().rev().find(|m| m.frame <= f)?;
        Some(Mark {
            frame: f,
            index: m.index + (f - m.frame),
            desc: m.desc.clone(),
            ..*m
        })
    }

    /// The chain placed last in the timeline, from its splice on.
    pub fn newest_mark(&self) -> Option<Mark> {
        self.marks.lock().unwrap().back().cloned()
    }

    fn push_mark(&self, m: Mark) {
        let mut g = self.marks.lock().unwrap();
        // A mark supersedes later ones: they described frames just rewritten.
        while g.back().map(|b| b.frame >= m.frame).unwrap_or(false) {
            g.pop_back();
        }
        g.push_back(m);
        while g.len() > 64 {
            g.pop_front();
        }
    }

    fn publish(&self, c: &Chain) {
        *self.tokens.lock().unwrap() = c.tokens.clone();
        *self.notes.lock().unwrap() = c.notes.clone();
        *self.gain.lock().unwrap() = c.gain;
        *self.live_gain.lock().unwrap() = c.live_gain.clone();
        *self.tp_pred_db.lock().unwrap() = c.tp_pred_db;
        self.quick.store(c.variant_quick, Ordering::Relaxed);
    }

    /// The gain the chain on the air plays at now: its slow gain's, or its
    /// scalar.
    pub fn gain_now(&self) -> f64 {
        match self.live_gain.lock().unwrap().as_ref() {
            Some(g) => f64::from_bits(g.load(Ordering::Relaxed)),
            None => *self.gain.lock().unwrap(),
        }
    }
}

pub fn spawn(rx: Receiver<Msg>, shared: Arc<Shared>, out: Arc<OutputShared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aura-render".into())
        .spawn(move || {
            raise_priority();
            // The render work gets its own worker pool at raised priority.
            // Background preparation (the engine's source stages, loading a
            // 30M filter) runs on rayon's global pool; sharing one pool let a
            // filter load starve playback for tens of milliseconds. One
            // worker per CPU the process may run on (its affinity mask).
            let pool = Arc::new(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(super::calibration::cpu_width().max(2))
                    .thread_name(|i| format!("aura-render-{}", i))
                    .start_handler(|_| raise_priority())
                    .build()
                    .expect("render thread pool"),
            );
            let _ = RENDER_POOL.set(pool.clone());
            pool.install(move || run(rx, shared, out))
        })
        .expect("spawn render thread")
}

/// The first render pool of the process: the player's (controller::get spawns one render
/// thread). Later spawns (e2e) keep their own pool: two render loops in one pool can nest.
static RENDER_POOL: std::sync::OnceLock<Arc<rayon::ThreadPool>> = std::sync::OnceLock::new();

/// The player's render pool once its render thread has started (logs, benches).
pub fn render_pool() -> Option<Arc<rayon::ThreadPool>> {
    RENDER_POOL.get().cloned()
}

/// Above the preparation threads, below the output thread (which runs at
/// MMCSS "Pro Audio").
fn raise_priority() {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
        const THREAD_PRIORITY_HIGHEST: i32 = 2;
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    std::sync::mpsc::channel()
}

/// A queued chain goes on gapless only in a stream it was built for: its
/// rate and its mode. The output applies no volume in a BIT-PERFECT stream,
/// so a DSP chain there played at full scale; and a direct chain in a DSP
/// stream is not bit-perfect. One the stream cannot take is not taken: the
/// track ends, and the controller starts the next on a stream of its own.
fn takes(stream_rate: u32, stream_direct: bool, c: &Chain) -> bool {
    c.out_rate == stream_rate && c.direct == stream_direct
}

enum Plan {
    /// Not yet: the old chain has not rendered far enough.
    Wait,
    /// Swap at `frame`; the new chain must be at output index `index` there.
    /// `append`: nothing unread lies beyond `frame`, no crossfade needed.
    At { frame: u64, index: u64, append: bool },
}

fn plan_swap(t: &Timeline, shared: &Shared, chain: &Chain, continuous: bool, xf: u64) -> Plan {
    let earliest = t.earliest_rewrite();
    let w = t.write_pos();
    let pos = chain.stage.position();
    if w <= earliest {
        // A continuous swap of the same track goes on from the old chain's
        // index at `w`, the next frame to write. A chain behind it (built at
        // the last written frame, and the render thread appends the step it
        // is running before it reads the Swap) is skipped forward, never
        // replayed; a chain ahead of it keeps its jump.
        let index = match shared.locate(w) {
            Some(m) if continuous && m.track_id == chain.track_id && m.rate == chain.out_rate && m.index > pos => m.index,
            _ => pos,
        };
        return Plan::At { frame: w, index, append: true };
    }
    if !continuous {
        return Plan::At { frame: earliest, index: pos, append: false };
    }
    let m = match shared.locate(earliest) {
        Some(m) if m.track_id == chain.track_id => m,
        _ => return Plan::At { frame: earliest, index: pos, append: false },
    };
    // m.index is the output index playing at `earliest`.
    if pos <= m.index {
        return Plan::At { frame: earliest, index: m.index, append: false };
    }
    let frame = earliest + (pos - m.index);
    if frame + xf > w {
        return Plan::Wait;
    }
    Plan::At { frame, index: pos, append: false }
}

struct SwapBufs {
    xf: usize,
    old_l: Vec<f64>,
    old_r: Vec<f64>,
    new_l: Vec<f64>,
    new_r: Vec<f64>,
}

impl SwapBufs {
    fn new(rate: u32) -> SwapBufs {
        let xf = ((XFADE_MS / 1000.0) * rate as f64) as usize;
        SwapBufs { xf, old_l: vec![0.0; xf], old_r: vec![0.0; xf], new_l: vec![0.0; xf], new_r: vec![0.0; xf] }
    }
}

enum SwapResult {
    Done(u64),
    Wait,
    Retry,
}

/// Try to crossfade into `chain` now.
fn try_swap(t: &Timeline, shared: &Shared, chain: &mut Chain, continuous: bool, bufs: &mut SwapBufs) -> SwapResult {
    let xf = bufs.xf;
    let (frame, index, append) = match plan_swap(t, shared, chain, continuous, xf as u64) {
        Plan::Wait => return SwapResult::Wait,
        Plan::At { frame, index, append } => (frame, index, append),
    };
    if index > chain.stage.position() {
        super::stages::skip_to(chain.stage.as_mut(), index);
    }
    let idx_at_frame = chain.stage.position();
    chain.stage.read(&mut bufs.new_l, &mut bufs.new_r);
    if append {
        t.append(&bufs.new_l, &bufs.new_r);
    } else {
        let w = t.write_pos();
        let have_old = w.saturating_sub(frame).min(xf as u64) as usize;
        t.peek(frame, &mut bufs.old_l[..have_old], &mut bufs.old_r[..have_old]);
        for i in have_old..xf {
            bufs.old_l[i] = 0.0;
            bufs.old_r[i] = 0.0;
        }
        if t.rewrite_from(frame).is_err() {
            // The reader came too close while this ran. The chain has moved
            // on by one crossfade; plan again on the next pass.
            return SwapResult::Retry;
        }
        for i in 0..xf {
            let a = crate::audio::hybrid_phase::switch_fade(i as f64 / xf as f64);
            bufs.new_l[i] = bufs.old_l[i] * (1.0 - a) + bufs.new_l[i] * a;
            bufs.new_r[i] = bufs.old_r[i] * (1.0 - a) + bufs.new_r[i] * a;
        }
        t.append(&bufs.new_l, &bufs.new_r);
    }
    let desc = ChainDesc::from_chain(&chain);
    // analytics: record_splice — read prev mark BEFORE pushing so we capture the "from" state
    let (from_src_index, track_from) = {
        let g = shared.marks.lock().unwrap();
        if let Some(prev) = g.back() {
            let elapsed = frame.saturating_sub(prev.frame);
            (prev.index.wrapping_add(elapsed), prev.track_id)
        } else { (0, 0) }
    };
    shared.push_mark(Mark { frame, track_id: chain.track_id, index: idx_at_frame, rate: chain.out_rate, l: chain.l, desc });
    // analytics: splice log entry — continuous swap (chain/settings change)
    t.record_splice(frame, from_src_index, idx_at_frame, track_from, chain.track_id);
    SwapResult::Done(frame)
}

/// A new chain's sound drawn off the render thread: `l[0]`, `r[0]` are its
/// output at index `start`, and the chain sent with it stands at
/// `start + l.len()`. `over` is the chain whose buffered audio it replaces;
/// it goes in only while that chain is the last one in the timeline.
pub struct Redraw {
    pub start: u64,
    pub l: Vec<f64>,
    pub r: Vec<f64>,
    pub over: Arc<ChainDesc>,
}

enum RedrawResult {
    /// Spliced at this frame.
    Done(u64),
    /// The reader came too close while it ran: plan again on the next pass.
    Retry,
    /// Not this redraw: the plain swap takes over.
    Decline(&'static str),
}

/// Splice `rd` into the timeline at the earliest frame it covers that may
/// still be rewritten, with the same crossfade as `try_swap`. Everything
/// from the end of the crossfade on is the redraw as drawn, sample for
/// sample; the chain then goes on from its end. The timeline holds audio
/// before the volume, so the output applies it to the redraw as to anything.
fn try_redraw(t: &Timeline, shared: &Shared, chain: &Chain, rd: &Redraw, bufs: &mut SwapBufs) -> RedrawResult {
    let xf = bufs.xf;
    let end = rd.start + rd.l.len() as u64;
    if chain.direct {
        return RedrawResult::Decline("direct");
    }
    if chain.stage.position() != end {
        return RedrawResult::Decline("chain");
    }
    let nm = match shared.newest_mark() {
        Some(m) => m,
        None => return RedrawResult::Decline("no-mark"),
    };
    if nm.track_id != chain.track_id || nm.rate != chain.out_rate || !Arc::ptr_eq(&nm.desc, &rd.over) {
        return RedrawResult::Decline("moved");
    }
    let earliest = t.earliest_rewrite();
    let w = t.write_pos();
    if w <= earliest || nm.frame > earliest {
        return RedrawResult::Decline("thin");
    }
    // Indices of the old chain at the earliest rewritable frame and at the
    // write head (one mark covers both: it is the newest and starts before).
    let i_earliest = nm.index + (earliest - nm.frame);
    let i_write = nm.index + (w - nm.frame);
    let at = rd.start.max(i_earliest);
    if at + xf as u64 > end.min(i_write) {
        return RedrawResult::Decline("late");
    }
    let max_tail = (REDRAW_MAX_TAIL_MS / 1000.0 * t.rate() as f64) as u64;
    if i_write > end + max_tail {
        return RedrawResult::Decline("tail");
    }
    let frame = nm.frame + (at - nm.index);
    let off = (at - rd.start) as usize;
    let n = (end - at) as u64;
    if frame + n > t.read_pos() + t.capacity_frames() {
        return RedrawResult::Decline("capacity");
    }
    t.peek(frame, &mut bufs.old_l, &mut bufs.old_r);
    if t.rewrite_from(frame).is_err() {
        return RedrawResult::Retry;
    }
    for i in 0..xf {
        let a = crate::audio::hybrid_phase::switch_fade(i as f64 / xf as f64);
        bufs.new_l[i] = bufs.old_l[i] * (1.0 - a) + rd.l[off + i] * a;
        bufs.new_r[i] = bufs.old_r[i] * (1.0 - a) + rd.r[off + i] * a;
    }
    t.append(&bufs.new_l, &bufs.new_r);
    // In steps, so the reader sees each one as soon as it is written.
    let mut i = off + xf;
    while i < rd.l.len() {
        let j = (i + STEP).min(rd.l.len());
        t.append(&rd.l[i..j], &rd.r[i..j]);
        i = j;
    }
    let desc = ChainDesc::from_chain(chain);
    shared.push_mark(Mark { frame, track_id: chain.track_id, index: at, rate: chain.out_rate, l: chain.l, desc });
    t.record_splice(frame, at, at, nm.track_id, chain.track_id);
    let rate = t.rate() as f64;
    crate::aelog!(
        "[REDRAW] {}",
        serde_json::json!({
            "track": chain.track_id,
            "spliceAheadS": (frame - t.read_pos().min(frame)) as f64 / rate,
            "redrawnS": n as f64 / rate,
            "skippedHeadS": off as f64 / rate,
            "tailS": i_write.saturating_sub(end) as f64 / rate,
            "overS": end.saturating_sub(i_write) as f64 / rate,
            "wasBufferedS": (w - frame) as f64 / rate,
        })
    );
    RedrawResult::Done(frame)
}

/// Path of the buffer-target persistence file.
fn buffer_json_path() -> Option<std::path::PathBuf> {
    crate::app_dir::root().map(|d| d.join("player-buffer.json"))
}

/// Load the persisted adaptive buffer target, or None on any failure.
fn load_buffer_target() -> Option<f64> {
    let p = buffer_json_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v["targetS"].as_f64().filter(|&t| t >= MIN_AHEAD_S && t <= 30.0)
}

/// Persist the adaptive buffer target (at most every 30 s); ignores write failures.
fn save_buffer_target(target_s: f64, last_save: &mut Instant) {
    if last_save.elapsed().as_secs() < 30 {
        return;
    }
    if let Some(p) = buffer_json_path() {
        if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
        let json = format!("{{\"targetS\":{:.3}}}", target_s);
        if std::fs::write(&p, json).is_ok() {
            *last_save = Instant::now();
        }
    }
}

/// How far ahead of the reader to render, in frames: the adaptive target, or
/// the visualizations' floor when that is more (within the ring's ceiling).
fn ahead_frames(shared: &Shared, t: &Timeline) -> f64 {
    let target_s = *shared.target_ahead_s.lock().unwrap();
    let floor_s = f64::from_bits(shared.vis_floor_bits.load(Ordering::Relaxed));
    let rate = t.rate() as f64;
    let ceil_s = t.capacity_frames() as f64 * MAX_AHEAD_FRAC / rate;
    target_s.max(floor_s.min(ceil_s)) * rate
}

/// O6: a fresh RTF window, so the first reading after a Start or a placed
/// swap never holds a single burst step after an idle gap, or two chains.
/// The calibration's clean-window filter stays as the second line.
fn restart_window(rendered_s: &mut f64, busy: &mut Duration, window_t0: &mut Instant) {
    *rendered_s = 0.0;
    *busy = Duration::ZERO;
    *window_t0 = Instant::now();
}

fn run(rx: Receiver<Msg>, shared: Arc<Shared>, out: Arc<OutputShared>) {
    shared.alive.store(true, Ordering::Relaxed);

    // Restore the persisted buffer target, or start from the initial guess.
    if let Some(t) = load_buffer_target() {
        *shared.target_ahead_s.lock().unwrap() = t;
    }
    let mut buf_save_t = Instant::now();

    let mut timeline: Option<Arc<Timeline>> = None;
    // The stream's mode: BIT-PERFECT (direct, no volume) or DSP. Its first
    // chain's, the one the device was opened for.
    let mut stream_direct = false;
    let mut cur: Option<Chain> = None;
    let mut next: Option<Chain> = None;
    // (chain, continuous, seek, side_buf, requested)
    let mut pending: Option<(Chain, bool, bool, Option<(Vec<f64>, Vec<f64>)>, Instant)> = None;
    // The redraw that came with the pending swap (Msg::Redraw), if any.
    let mut pending_redraw: Option<Redraw> = None;
    let mut bufs: Option<SwapBufs> = None;
    // A swap waiting for its first frame to be read, for the latency figure.
    let mut switch_probe: Option<(u64, Instant)> = None;
    let mut bl = vec![0.0; STEP];
    let mut br = vec![0.0; STEP];
    let mut rendered_s = 0.0f64;
    let mut busy = Duration::ZERO;
    let mut window_t0 = Instant::now();
    // Adaptive buffer: low-water mark tracking for the current observation window.
    let mut low_water_frames: u64 = u64::MAX;
    let mut underrun_since: u64 = 0;
    let mut adapt_t0 = Instant::now();

    loop {
        let full = match (&timeline, &cur) {
            (Some(t), Some(_)) => {
                t.buffered_frames() as f64 >= ahead_frames(&shared, t) || t.free_frames() < STEP as u64
            }
            _ => true,
        };
        let msg = if full && pending.is_none() {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.try_recv() {
                Ok(m) => Some(m),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        };

        if let Some(m) = msg {
            match m {
                Msg::Stop => {
                    cur = None;
                    next = None;
                    pending = None;  // also drops any side_buf (CR-6)
                    pending_redraw = None;
                    switch_probe = None;
                    shared.swap_unheard.store(false, Ordering::Relaxed);
                    timeline = None;
                    shared.marks.lock().unwrap().clear();
                    shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                    shared.queued.store(false, Ordering::Relaxed);
                    shared.swap_pending.store(false, Ordering::Relaxed);
                }
                Msg::Start { chain, timeline: t } => {
                    let f = t.write_pos();
                    let desc = ChainDesc::from_chain(&chain);
                    shared.marks.lock().unwrap().clear();
                    shared.push_mark(Mark { frame: f, track_id: chain.track_id, index: chain.start, rate: chain.out_rate, l: chain.l, desc });
                    shared.publish(&chain);
                    shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                    shared.queued.store(false, Ordering::Relaxed);
                    shared.swap_pending.store(false, Ordering::Relaxed);
                    bufs = Some(SwapBufs::new(t.rate()));
                    stream_direct = chain.direct;
                    timeline = Some(t);
                    cur = Some(chain);
                    next = None;
                    pending = None;  // also drops any side_buf (CR-6)
                    pending_redraw = None;
                    low_water_frames = u64::MAX;
                    underrun_since = 0;
                    adapt_t0 = Instant::now();
                    restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                }
                Msg::Queue { chain, seq } => {
                    // The current chain may have run out before its next came (a
                    // seek into its last seconds: the render is seconds ahead of
                    // the reader). While nothing is written past its end, the
                    // next goes on right there, as a gapless one would.
                    let late = cur.is_none() && pending.is_none() && timeline.as_ref().is_some_and(|t| {
                        let e = shared.ended_at.load(Ordering::Relaxed);
                        e != u64::MAX && t.write_pos() == e && takes(t.rate(), stream_direct, &chain)
                    });
                    if late {
                        let f = timeline.as_ref().map_or(0, |t| t.write_pos());
                        let desc = ChainDesc::from_chain(&chain);
                        shared.push_mark(Mark { frame: f, track_id: chain.track_id, index: chain.start, rate: chain.out_rate, l: chain.l, desc });
                        shared.publish(&chain);
                        shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                        shared.queued.store(false, Ordering::Relaxed);
                        cur = Some(chain);
                    } else {
                        shared.queued.store(true, Ordering::Relaxed);
                        next = Some(chain);
                    }
                    // Written after late/queued/ended_at so a concurrent
                    // start_track or loop check that races on queue_taken sees
                    // the chain already in place.
                    shared.queue_taken.store(seq, Ordering::Release);
                }
                Msg::Unqueue => {
                    shared.queued.store(false, Ordering::Relaxed);
                    next = None;
                }
                Msg::Swap { chain, continuous, seek, side_buf, requested } => {
                    // A newer request replaces an older one still waiting.
                    // CR-5: any swap supersedes the gapless queue.
                    // CR-6: old side_buf from a replaced seek is dropped here.
                    shared.swap_pending.store(true, Ordering::Relaxed);
                    next = None;
                    shared.queued.store(false, Ordering::Relaxed);
                    pending = Some((chain, continuous, seek, side_buf, requested));
                    pending_redraw = None;
                }
                Msg::Redraw { chain, redraw, requested } => {
                    // A continuous swap like any other, with its sound drawn.
                    shared.swap_pending.store(true, Ordering::Relaxed);
                    next = None;
                    shared.queued.store(false, Ordering::Relaxed);
                    pending = Some((chain, true, false, None, requested));
                    pending_redraw = Some(redraw);
                }
            }
            continue;
        }

        let t = match &timeline {
            Some(t) => t.clone(),
            None => continue,
        };

        if let Some((mut chain, continuous, seek, mut side_buf_opt, requested)) = pending.take() {
            if cur.is_some() {
                // ── Seek crossfade path (CR-1, CR-2, CR-5, CR-6) ─────────────
                // Old audio keeps playing; the pre-rendered side_buf is blended
                // with the old timeline audio at the earliest rewritable frame.
                // No hold/jump_frame protocol unless the output is already held
                // (e.g. a concurrent BIT-PERFECT switch — §2.4 correction).
                if seek {
                    if let Some((sb_l, sb_r)) = side_buf_opt.take() {
                        let frame = t.earliest_rewrite();
                        let xf = ((XFADE_MS / 1000.0) * chain.out_rate as f64) as usize;
                        let xf = xf.min(sb_l.len());
                        // Peek old audio at the splice point for the crossfade.
                        let w = t.write_pos();
                        let have_old = w.saturating_sub(frame).min(xf as u64) as usize;
                        let mut old_l = vec![0.0f64; xf];
                        let mut old_r = vec![0.0f64; xf];
                        if have_old > 0 {
                            t.peek(frame, &mut old_l[..have_old], &mut old_r[..have_old]);
                        }
                        // Attempt to rewrite from frame; retry next pass on failure.
                        if t.rewrite_from(frame).is_err() {
                            pending = Some((chain, continuous, seek, Some((sb_l, sb_r)), requested));
                            continue;
                        }
                        // Equal-power crossfade: old → new over xf frames.
                        let mut xf_l = vec![0.0f64; xf];
                        let mut xf_r = vec![0.0f64; xf];
                        for i in 0..xf {
                            let a = crate::audio::hybrid_phase::switch_fade(i as f64 / xf as f64);
                            xf_l[i] = old_l[i] * (1.0 - a) + sb_l[i] * a;
                            xf_r[i] = old_r[i] * (1.0 - a) + sb_r[i] * a;
                        }
                        t.append(&xf_l, &xf_r);
                        // Append the rest of the pre-roll after the crossfade.
                        if sb_l.len() > xf {
                            t.append(&sb_l[xf..], &sb_r[xf..]);
                        }
                        // CR-2: mark at chain.start (the seek target), not the
                        // post-pre-roll position.
                        let desc = ChainDesc::from_chain(&chain);
                        // analytics: record_splice — seek crossfade; read prev mark first
                        let (from_src_seek, track_from_seek) = {
                            let g = shared.marks.lock().unwrap();
                            if let Some(prev) = g.back() {
                                let elapsed = frame.saturating_sub(prev.frame);
                                (prev.index.wrapping_add(elapsed), prev.track_id)
                            } else { (0, 0) }
                        };
                        shared.push_mark(Mark { frame, track_id: chain.track_id, index: chain.start, rate: chain.out_rate, l: chain.l, desc });
                        // analytics: splice log entry — seek splice
                        t.record_splice(frame, from_src_seek, chain.start, track_from_seek, chain.track_id);
                        shared.publish(&chain);
                        shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                        shared.swap_pending.store(false, Ordering::Relaxed);
                        switch_probe = Some((frame, requested));
                        shared.swap_unheard.store(true, Ordering::Relaxed);
                        cur = Some(chain);
                        restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                        // If the output is already held (e.g. BIT-PERFECT switch
                        // was in progress), post the splice frame so it teleports
                        // there and resumes — §2.4 correction (CR-4).
                        if out.hold.load(Ordering::Acquire) {
                            out.jump_frame.store(frame, Ordering::Release);
                        }
                        continue;
                    } else {
                        // side_buf absent (should not happen) — fall through to
                        // the jump protocol as a safe fallback.
                    }
                }

                if !continuous {
                    // Jump protocol: write a short fade-in at the earliest
                    // writable frame and post that frame to the output thread.
                    // The output thread will teleport its read pointer there,
                    // ending the hold and ramping up. No crossfade with the
                    // old audio — the old audio is already silenced by the hold.
                    let frame = {
                        let e = t.earliest_rewrite();
                        // Rewrite from there; if the reader closed in, try once more.
                        if t.rewrite_from(e).is_ok() { e } else {
                            let e2 = t.earliest_rewrite();
                            let _ = t.rewrite_from(e2);
                            e2
                        }
                    };
                    // Advance the chain to its start position, if it fell
                    // behind (defensive: build_chain already positions it).
                    let want_pos = chain.start;
                    if chain.stage.position() < want_pos {
                        super::stages::skip_to(chain.stage.as_mut(), want_pos);
                    }
                    let fade_n = ((JUMP_FADE_MS / 1000.0) * chain.out_rate as f64) as usize;
                    let fade_n = fade_n.min(STEP).max(1);
                    chain.stage.read(&mut bl[..fade_n], &mut br[..fade_n]);
                    // Ramp 0 → 1 over the fade window.
                    for i in 0..fade_n {
                        let a = (i as f64 + 0.5) / fade_n as f64;
                        bl[i] *= a;
                        br[i] *= a;
                    }
                    t.append(&bl[..fade_n], &br[..fade_n]);
                    // The rest of a block before the output is told: its first
                    // period after the jump takes a whole device period at once,
                    // and the fade window alone can be shorter (the difference
                    // was an underrun at the start of the new track).
                    let more = STEP - fade_n;
                    if more > 0 {
                        let got = chain.stage.read(&mut bl[..more], &mut br[..more]).min(more);
                        t.append(&bl[..got], &br[..got]);
                    }
                    let desc = ChainDesc::from_chain(&chain);
                    // analytics: record_splice — jump (track switch / non-continuous swap)
                    let (from_src_jump, track_from_jump) = {
                        let g = shared.marks.lock().unwrap();
                        if let Some(prev) = g.back() {
                            let elapsed = frame.saturating_sub(prev.frame);
                            (prev.index.wrapping_add(elapsed), prev.track_id)
                        } else { (0, 0) }
                    };
                    shared.push_mark(Mark { frame, track_id: chain.track_id, index: want_pos, rate: chain.out_rate, l: chain.l, desc });
                    // analytics: splice log entry — jump splice (track change or seek fallback)
                    t.record_splice(frame, from_src_jump, want_pos, track_from_jump, chain.track_id);
                    shared.publish(&chain);
                    shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                    shared.swap_pending.store(false, Ordering::Relaxed);
                    // Tell the output thread to jump here. Release ordering so
                    // the timeline writes are visible before the frame number is.
                    out.jump_frame.store(frame, Ordering::Release);
                    switch_probe = Some((frame, requested));
                    shared.swap_unheard.store(true, Ordering::Relaxed);
                    cur = Some(chain);
                    restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                    continue;
                }
                let b = bufs.get_or_insert_with(|| SwapBufs::new(t.rate()));
                if let Some(rd) = pending_redraw.take() {
                    match try_redraw(&t, &shared, &chain, &rd, b) {
                        RedrawResult::Done(frame) => {
                            shared.publish(&chain);
                            shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                            shared.swap_pending.store(false, Ordering::Relaxed);
                            cur = Some(chain);
                            switch_probe = Some((frame, requested));
                            shared.swap_unheard.store(true, Ordering::Relaxed);
                            restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                            continue;
                        }
                        RedrawResult::Retry => {
                            pending_redraw = Some(rd);
                            pending = Some((chain, continuous, seek, side_buf_opt, requested));
                            continue;
                        }
                        RedrawResult::Decline(why) => {
                            crate::aelog!(
                                "[REDRAW] {}",
                                serde_json::json!({ "track": chain.track_id, "declined": why })
                            );
                        }
                    }
                }
                match try_swap(&t, &shared, &mut chain, continuous, b) {
                    SwapResult::Done(frame) => {
                        // O1: try_swap already pushed the mark at the correct index
                        // (chain.start at the splice frame, line 392).  A second
                        // push_mark here with chain.start.min(position()) would evict
                        // that mark and replace it with the wrong position after any
                        // skipped-continuous swap.
                        shared.publish(&chain);
                        shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                        shared.swap_pending.store(false, Ordering::Relaxed);
                        cur = Some(chain);
                        switch_probe = Some((frame, requested));
                        shared.swap_unheard.store(true, Ordering::Relaxed);
                        restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                        continue;
                    }
                    SwapResult::Retry => {
                        pending = Some((chain, continuous, seek, side_buf_opt, requested));
                        continue;
                    }
                    SwapResult::Wait => pending = Some((chain, continuous, seek, side_buf_opt, requested)),
                }
            } else {
                // The track ended while the swap waited: the new chain
                // simply continues at the end of what is buffered.
                pending_redraw = None;
                let f = t.write_pos();
                let desc = ChainDesc::from_chain(&chain);
                shared.push_mark(Mark { frame: f, track_id: chain.track_id, index: chain.stage.position(), rate: chain.out_rate, l: chain.l, desc });
                shared.publish(&chain);
                shared.ended_at.store(u64::MAX, Ordering::Relaxed);
                shared.swap_pending.store(false, Ordering::Relaxed);
                switch_probe = Some((f, requested));
                shared.swap_unheard.store(true, Ordering::Relaxed);
                cur = Some(chain);
                restart_window(&mut rendered_s, &mut busy, &mut window_t0); // O6
                // For a non-continuous swap (jump) with no old chain, the output
                // thread is still held from start_track. Signal the jump here so
                // it can release and start playing the new chain.
                if !continuous {
                    out.jump_frame.store(f, Ordering::Release);
                }
            }
        }

        if let Some((f, t0)) = switch_probe {
            if t.read_pos() >= f {
                shared.last_switch_ms.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                shared.switches.fetch_add(1, Ordering::Relaxed);
                shared.swap_unheard.store(false, Ordering::Relaxed);
                switch_probe = None;
            }
        }

        let c = match &mut cur {
            Some(c) => c,
            None => continue,
        };
        // A waiting swap needs the old chain to run ahead to its frame.
        let cap = if pending.is_some() {
            t.capacity_frames() as f64 * 0.9
        } else {
            ahead_frames(&shared, &t)
        };
        if t.buffered_frames() as f64 >= cap || t.free_frames() < STEP as u64 {
            continue;
        }

        let t0 = Instant::now();
        let n = STEP;
        let inside = c.stage.read(&mut bl[..n], &mut br[..n]);
        // K4: detect GPU convolver failure — set the shared flag once and leave
        // it set for the rest of this track session.
        if !shared.gpu_failed.load(Ordering::Relaxed) && c.stage.is_gpu_failed() {
            shared.gpu_failed.store(true, Ordering::Relaxed);
            // Record the timeline frame BEFORE the current append so that K4
            // can cap the CPU-fallback splice below the first GPU zero.
            shared.gpu_zeros_from.store(t.write_pos(), Ordering::Relaxed);
        }
        if inside < n {
            let end_frame = t.write_pos() + inside as u64;
            t.append(&bl[..inside], &br[..inside]);
            match next.take() {
                Some(nc) if takes(t.rate(), stream_direct, &nc) => {
                    let desc = ChainDesc::from_chain(&nc);
                    shared.push_mark(Mark { frame: end_frame, track_id: nc.track_id, index: nc.start, rate: nc.out_rate, l: nc.l, desc });
                    shared.publish(&nc);
                    shared.queued.store(false, Ordering::Relaxed);
                    cur = Some(nc);
                }
                other => {
                    next = other;
                    shared.ended_at.store(end_frame, Ordering::Relaxed);
                    cur = None;
                }
            }
        } else {
            t.append(&bl[..n], &br[..n]);
        }

        // Adaptive buffer: track low-water mark every ~2 s, adjust target.
        {
            let buf = t.buffered_frames();
            if buf < low_water_frames { low_water_frames = buf; }
            let ur = t.underrun_frames();
            let new_underruns = ur.saturating_sub(underrun_since);
            let elapsed = adapt_t0.elapsed().as_secs_f64();
            if elapsed >= 20.0 && low_water_frames != u64::MAX {
                let rate = t.rate() as f64;
                let cap = t.capacity_frames() as f64;
                let mut target = *shared.target_ahead_s.lock().unwrap();
                let low_s = low_water_frames as f64 / rate;
                let ceil_s = cap * MAX_AHEAD_FRAC / rate;
                if new_underruns > 0 || low_s < target * 0.25 {
                    // Underrun or critically low: grow quickly.
                    target = (target * 1.5 + 0.25).min(ceil_s);
                } else if low_s > target * 0.6 && elapsed >= 20.0 {
                    // Healthy: trim slowly.
                    target = (target * 0.9).max(MIN_AHEAD_S);
                }
                *shared.target_ahead_s.lock().unwrap() = target;
                save_buffer_target(target, &mut buf_save_t);
                low_water_frames = buf;   // reset for next window
                underrun_since = ur;
                adapt_t0 = Instant::now();
            }
        }
        busy += t0.elapsed();
        rendered_s += n as f64 / t.rate() as f64;
        if window_t0.elapsed() > Duration::from_secs(2) {
            if busy.as_secs_f64() > 0.0 {
                let rtf = rendered_s / busy.as_secs_f64();
                shared.rtf_milli.store((rtf * 1000.0) as u64, Ordering::Relaxed);
                // K2: live calibration update for CPU chains only.
                if let Some(c) = &cur {
                    if !c.gpu_on && !c.calib_key.is_empty() && rtf > 0.0 {
                        let block_dur_s = BLOCK as f64 * c.l as f64 / c.out_rate as f64;
                        let block_cost_ms = block_dur_s / rtf * 1000.0;
                        if block_cost_ms > 0.0 {
                            if let Ok(mut calib) = shared.calibration.try_lock() {
                                calib.update(&c.calib_key, block_cost_ms);
                            }
                        }
                    }
                }
            }
            rendered_s = 0.0;
            busy = Duration::ZERO;
            window_t0 = Instant::now();
        }
    }
    // Persist the final buffer target so the next session starts there.
    let target = *shared.target_ahead_s.lock().unwrap();
    if let Some(p) = buffer_json_path() {
        if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
        let json = format!("{{\"targetS\":{:.3}}}", target);
        let _ = std::fs::write(p, json);
    }
    shared.alive.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O1: push_mark evicts later marks but NOT earlier ones at the same frame.
    /// The Done branch of a continuous swap must NOT push a second mark
    /// (try_swap already pushed the correct one at that frame).
    /// This verifies the eviction logic: a mark at frame F supersedes marks
    /// at frames ≥ F; the second push_mark at the same frame replaces the first.
    #[test]
    fn push_mark_same_frame_eviction_and_o1_fix() {
        let shared = Shared::new();
        let rate = 44100u32;
        let desc = Arc::new(ChainDesc {
            source: "live", quick: false, taps: None, stages: vec![],
            gain_db: 0.0, tp_db: None, ceiling_db: None,
            gpu_on: false, downgrade: None, downgrade_gen: 0, hp_deferred: false, stream: false, file: None,
            settings: std::sync::Arc::new(crate::player::settings::PlayerSettings::default()),
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
            out_rate: 0,
            l: 1,
        });
        // Simulate old (buggy) behavior: try_swap pushes mark at frame 1000
        // with index 5000, then Done branch pushes again at 1000 with 4000.
        shared.push_mark(Mark { frame: 1000, track_id: 1, index: 5000, rate, l: 1,
            desc: desc.clone() });
        shared.push_mark(Mark { frame: 1000, track_id: 1, index: 4000, rate, l: 1,
            desc: desc.clone() });
        // Second push_mark at the same frame evicts and replaces the first —
        // this was the bug.
        let m = shared.locate(1000).unwrap();
        assert_eq!(m.index, 4000, "second push_mark replaces first at same frame");

        // Simulate fixed behavior: try_swap pushes once, Done branch does NOT
        // push again.  Only the first (correct) mark survives.
        let shared2 = Shared::new();
        shared2.push_mark(Mark { frame: 1000, track_id: 1, index: 5000, rate, l: 1,
            desc: desc.clone() });
        let m2 = shared2.locate(1000).unwrap();
        assert_eq!(m2.index, 5000, "O1 fixed: only try_swap's mark survives");
    }

    /// Silence that knows where it is.
    struct At(u64);

    impl crate::player::stages::Stage for At {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            out_l.fill(0.0);
            out_r.fill(0.0);
            self.0 += out_l.len() as u64;
            out_l.len()
        }
        fn position(&self) -> u64 {
            self.0
        }
        fn total(&self) -> u64 {
            u64::MAX / 4
        }
    }

    fn chain_at(track_id: u64, rate: u32, pos: u64) -> Chain {
        Chain {
            live_gain: None,
            stage: Box::new(At(pos)),
            track_id,
            out_rate: rate,
            l: 1,
            start: pos,
            tokens: vec![],
            gain: 1.0,
            tp_pred_db: 0.0,
            direct: false,
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
            settings: std::sync::Arc::new(crate::player::settings::PlayerSettings::default()),
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
        }
    }

    /// Less buffered than the guard (the append branch): a continuous swap of
    /// the same track goes on from the old chain's index at the write head.
    /// A chain behind it is skipped forward to it, and the splice mark is
    /// exact (K3 at 384 kHz: -8193 = one step plus the last written frame).
    /// A chain ahead of it, another track's chain and a jump keep their own
    /// position.
    #[test]
    fn append_swap_continues_the_old_index() {
        let rate = 48_000u32;
        let shared = Shared::new();
        let tl = Timeline::new(rate, 4.0);
        let few = vec![0.0; 1000];
        tl.append(&few, &few);
        assert!(tl.write_pos() <= tl.earliest_rewrite(), "the append branch needs a thin buffer");
        let desc = ChainDesc::from_chain(&chain_at(7, rate, 0));
        shared.push_mark(Mark { frame: 0, track_id: 7, index: 100_000, rate, l: 1, desc });
        let xf = ((XFADE_MS / 1000.0) * rate as f64) as u64;
        let plan = |c: &Chain, continuous: bool| match plan_swap(&tl, &shared, c, continuous, xf) {
            Plan::At { frame, index, append } => (frame, index, append),
            Plan::Wait => panic!("Wait on the append branch"),
        };
        assert_eq!(plan(&chain_at(7, rate, 90_000), true), (1000, 101_000, true), "behind: skipped forward");
        assert_eq!(plan(&chain_at(7, rate, 150_000), true), (1000, 150_000, true), "ahead: its own index");
        assert_eq!(plan(&chain_at(8, rate, 90_000), true), (1000, 90_000, true), "another track");
        assert_eq!(plan(&chain_at(7, rate, 90_000), false), (1000, 90_000, true), "a jump");
        assert_eq!(plan(&chain_at(7, 44_100, 90_000), true), (1000, 90_000, true), "another rate");

        let mut c = chain_at(7, rate, 101_000 - 8193);
        let mut bufs = SwapBufs::new(rate);
        match try_swap(&tl, &shared, &mut c, true, &mut bufs) {
            SwapResult::Done(frame) => assert_eq!(frame, 1000),
            _ => panic!("the swap was not placed"),
        }
        let m = shared.locate(1000).expect("splice mark");
        assert_eq!(m.index, 101_000, "the splice must continue the old chain's index");
        assert_eq!(c.stage.position(), 101_000 + bufs.xf as u64);
        assert_eq!(tl.write_pos(), 1000 + bufs.xf as u64);
    }

    /// A queued chain goes on gapless only in a stream of its rate and mode:
    /// a DSP chain in a BIT-PERFECT stream played at full scale (no volume
    /// there), a direct one in a DSP stream would not be bit-perfect.
    #[test]
    fn a_stream_takes_only_a_chain_of_its_rate_and_mode() {
        let dsp = chain_at(2, 88_200, 0);
        let mut direct = chain_at(3, 88_200, 0);
        direct.direct = true;
        assert!(takes(88_200, false, &dsp));
        assert!(!takes(88_200, true, &dsp), "a DSP chain in a BIT-PERFECT stream");
        assert!(takes(88_200, true, &direct));
        assert!(!takes(88_200, false, &direct), "a direct chain in a DSP stream");
        assert!(!takes(96_000, false, &dsp), "another rate");
    }

    /// Buffer-target persistence round-trips through valid JSON (J7).
    #[test]
    fn buffer_target_roundtrip() {
        let dir = std::env::temp_dir().join(format!("aura_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("player-buffer.json");
        let json = format!("{{\"targetS\":{:.3}}}", 2.345f64);
        std::fs::write(&p, &json).unwrap();
        // parse it back via the same logic as load_buffer_target
        let s = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let t = v["targetS"].as_f64().filter(|&x| x >= MIN_AHEAD_S && x <= 30.0).unwrap();
        assert!((t - 2.345).abs() < 0.001, "round-tripped value: {t}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A tone that knows where it is: sample `i` is a function of `i` alone,
    /// so two stages of the same tone agree wherever they started.
    pub(crate) struct Wave {
        pub pos: u64,
        pub k: f64,
    }

    impl Wave {
        pub fn at(k: f64, i: u64) -> (f64, f64) {
            ((i as f64 * k).sin() * 0.25, (i as f64 * k * 1.5).cos() * 0.25)
        }
    }

    impl crate::player::stages::Stage for Wave {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            for i in 0..out_l.len() {
                let (l, r) = Wave::at(self.k, self.pos + i as u64);
                out_l[i] = l;
                out_r[i] = r;
            }
            self.pos += out_l.len() as u64;
            out_l.len()
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            u64::MAX / 4
        }
    }

    pub(crate) fn wave_chain(track_id: u64, rate: u32, pos: u64, k: f64) -> Chain {
        let mut c = chain_at(track_id, rate, pos);
        c.stage = Box::new(Wave { pos, k });
        c
    }

    const K_OLD: f64 = 0.0123;
    const K_NEW: f64 = 0.0071;

    /// The old chain (track 7, index 100 000 at frame 0) with `buffered_s`
    /// rendered and the reader `read_s` in.
    fn old_on_air(rate: u32, buffered_s: f64, read_s: f64) -> (Arc<Shared>, Timeline, Arc<ChainDesc>) {
        let shared = Shared::new();
        let tl = Timeline::new(rate, 8.0);
        let mut old = wave_chain(7, rate, 100_000, K_OLD);
        let desc = ChainDesc::from_chain(&old);
        shared.push_mark(Mark { frame: 0, track_id: 7, index: 100_000, rate, l: 1, desc: desc.clone() });
        let n = ((buffered_s + read_s) * rate as f64) as usize;
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        old.stage.read(&mut l, &mut r);
        tl.append(&l, &r);
        let skip = (read_s * rate as f64) as usize;
        let mut sink = vec![0.0; skip * 2];
        assert_eq!(tl.read_into(&mut sink, skip), skip);
        (shared, tl, desc)
    }

    /// The new chain's sound from index `from` to `to`, and the chain at `to`.
    fn drawn(rate: u32, from: u64, to: u64, over: &Arc<ChainDesc>) -> (Chain, Redraw) {
        let mut c = wave_chain(7, rate, from, K_NEW);
        let n = (to - from) as usize;
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        c.stage.read(&mut l, &mut r);
        (c, Redraw { start: from, l, r, over: over.clone() })
    }

    /// The redraw goes in at its start near the reader, with the swap's
    /// crossfade, and from the end of the crossfade on the timeline holds
    /// the new chain's sound sample for sample, up to the redraw's end; the
    /// mark there carries the new chain's index.
    #[test]
    fn a_redraw_is_the_new_chain_bit_for_bit_after_the_crossfade() {
        let rate = 48_000u32;
        let (shared, tl, over) = old_on_air(rate, 3.0, 0.5);
        let earliest = tl.earliest_rewrite();
        let w = tl.write_pos();
        // Drawn from a little ahead of the reader to 0.1 s past the write head.
        let from = 100_000 + earliest + 2400;
        let to = 100_000 + w + 4800;
        let (chain, rd) = drawn(rate, from, to, &over);
        let mut bufs = SwapBufs::new(rate);
        let xf = bufs.xf as u64;
        let frame = match try_redraw(&tl, &shared, &chain, &rd, &mut bufs) {
            RedrawResult::Done(f) => f,
            RedrawResult::Retry => panic!("retry"),
            RedrawResult::Decline(why) => panic!("declined: {why}"),
        };
        assert_eq!(frame, earliest + 2400, "at the redraw's start");
        assert!((frame - tl.read_pos()) as f64 / (rate as f64) < 0.31, "near the reader");
        assert_eq!(tl.write_pos(), frame + (to - from), "the whole redraw written");
        let n = (tl.write_pos() - frame) as usize;
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        tl.peek(frame, &mut l, &mut r);
        for i in 0..n {
            let idx = from + i as u64;
            let (nl, nr) = Wave::at(K_NEW, idx);
            if (i as u64) < xf {
                let (ol, or) = Wave::at(K_OLD, idx);
                let a = crate::audio::hybrid_phase::switch_fade(i as f64 / xf as f64);
                assert_eq!(l[i], ol * (1.0 - a) + nl * a, "crossfade L at {i}");
                assert_eq!(r[i], or * (1.0 - a) + nr * a, "crossfade R at {i}");
            } else {
                assert_eq!(l[i].to_bits(), nl.to_bits(), "L at {i}");
                assert_eq!(r[i].to_bits(), nr.to_bits(), "R at {i}");
            }
        }
        let m = shared.locate(frame).unwrap();
        assert_eq!(m.index, from);
        assert!(!Arc::ptr_eq(&m.desc, &over), "the new chain's mark");
        assert_eq!(shared.locate(tl.write_pos() - 1).unwrap().index, to - 1);
    }

    /// The reader passed the redraw's start meanwhile: it goes in at the
    /// earliest rewritable frame, its head left out, still bit for bit.
    #[test]
    fn a_redraw_the_reader_overtook_goes_in_at_the_earliest_frame() {
        let rate = 48_000u32;
        let (shared, tl, over) = old_on_air(rate, 3.0, 0.5);
        let earliest = tl.earliest_rewrite();
        let from = 100_000 + earliest - 4800;
        let to = 100_000 + tl.write_pos();
        let (chain, rd) = drawn(rate, from, to, &over);
        let mut bufs = SwapBufs::new(rate);
        let xf = bufs.xf as u64;
        let frame = match try_redraw(&tl, &shared, &chain, &rd, &mut bufs) {
            RedrawResult::Done(f) => f,
            _ => panic!("not placed"),
        };
        assert_eq!(frame, earliest);
        let (mut l, mut r) = (vec![0.0; 1], vec![0.0; 1]);
        tl.peek(frame + xf, &mut l, &mut r);
        let (nl, nr) = Wave::at(K_NEW, 100_000 + frame + xf);
        assert_eq!((l[0].to_bits(), r[0].to_bits()), (nl.to_bits(), nr.to_bits()));
        assert_eq!(shared.locate(frame).unwrap().index, 100_000 + frame);
    }

    /// Declined, the timeline and the marks stay as they were: a redraw the
    /// reader passed entirely, one ending too far behind the write head (the
    /// render thread would stall drawing the rest), one over a chain that is
    /// no longer the newest (a seek landed while it was drawn), another
    /// track's, and any in a BIT-PERFECT stream.
    #[test]
    fn a_redraw_declined_leaves_the_timeline_as_it_was() {
        let rate = 48_000u32;
        type Make = dyn Fn(&Arc<Shared>, &Timeline, &Arc<ChainDesc>) -> (Chain, Redraw);
        let check = |label: &str, want: &str, f: &Make| {
            let (shared, tl, over) = old_on_air(rate, 3.0, 0.5);
            let (chain, rd) = f(&shared, &tl, &over);
            let (w, marks) = (tl.write_pos(), shared.marks.lock().unwrap().len());
            let mut bufs = SwapBufs::new(rate);
            match try_redraw(&tl, &shared, &chain, &rd, &mut bufs) {
                RedrawResult::Decline(why) => assert_eq!(why, want, "{label}"),
                _ => panic!("{label}: not declined"),
            }
            assert_eq!(tl.write_pos(), w, "{label}: the timeline was cut");
            assert_eq!(shared.marks.lock().unwrap().len(), marks, "{label}: a mark");
        };
        check("late", "late", &move |_, tl, over| {
            let e = 100_000 + tl.earliest_rewrite();
            drawn(rate, e - 24_000, e + 100, over)
        });
        check("tail", "tail", &move |_, tl, over| {
            let e = 100_000 + tl.earliest_rewrite();
            let w = 100_000 + tl.write_pos();
            drawn(rate, e, w - (REDRAW_MAX_TAIL_MS / 1000.0 * rate as f64) as u64 - 1, over)
        });
        check("seek", "moved", &move |shared, tl, over| {
            let e = 100_000 + tl.earliest_rewrite();
            let w = 100_000 + tl.write_pos();
            let out = drawn(rate, e, w, over);
            // A seek lands at the earliest rewritable frame meanwhile.
            let seek = ChainDesc::from_chain(&chain_at(7, rate, 900_000));
            shared.push_mark(Mark { frame: tl.earliest_rewrite(), track_id: 7, index: 900_000, rate, l: 1, desc: seek });
            out
        });
        check("other track", "moved", &move |_, tl, over| {
            let e = 100_000 + tl.earliest_rewrite();
            let (mut c, rd) = drawn(rate, e, 100_000 + tl.write_pos(), over);
            c.track_id = 8;
            (c, rd)
        });
        check("direct", "direct", &move |_, tl, over| {
            let e = 100_000 + tl.earliest_rewrite();
            let (mut c, rd) = drawn(rate, e, 100_000 + tl.write_pos(), over);
            c.direct = true;
            (c, rd)
        });
    }

    /// Through the render thread: a Redraw goes in at its start near the
    /// reader and the chain goes on from its end; a Swap sent right after it
    /// (a seek, a settings change) supersedes it whole.
    #[test]
    fn the_render_thread_splices_a_redraw_and_a_later_swap_supersedes_it() {
        let rate = 48_000u32;
        for superseded in [false, true] {
            let (tx, rx) = channel();
            let shared = Shared::new();
            spawn(rx, shared.clone(), OutputShared::new());
            let tl = Arc::new(Timeline::new(rate, 8.0));
            tx.send(Msg::Start { chain: wave_chain(7, rate, 100_000, K_OLD), timeline: tl.clone() }).unwrap();
            // Drawn once the render thread has filled its buffer (nothing
            // reads it): the old chain then stands still at the write head.
            let t0 = Instant::now();
            let mut seen = u64::MAX;
            while (tl.buffered_frames() as f64) < 0.6 * rate as f64 || tl.write_pos() != seen {
                assert!(t0.elapsed() < Duration::from_secs(10), "the buffer never settled");
                seen = tl.write_pos();
                std::thread::sleep(Duration::from_millis(50));
            }
            let over = shared.newest_mark().unwrap().desc;
            let from = 100_000 + tl.earliest_rewrite() + 4800;
            let to = 100_000 + tl.write_pos() + 4800;
            let (chain, rd) = drawn(rate, from, to, &over);
            tx.send(Msg::Redraw { chain, redraw: rd, requested: Instant::now() }).unwrap();
            let k_later = 0.0031;
            if superseded {
                let c = wave_chain(7, rate, from + 9600, k_later);
                tx.send(Msg::Swap { chain: c, continuous: true, seek: false, side_buf: None, requested: Instant::now() }).unwrap();
            }
            // Until the expected chain's mark is the newest (the redraw may go
            // in before the later swap arrives; the later swap then follows it)
            // and the render thread has written past its crossfade: the last
            // frame is then the chain's own sound, not the blend.
            let xf = SwapBufs::new(rate).xf as u64;
            let t0 = Instant::now();
            let m = loop {
                let m = shared.newest_mark().unwrap();
                let landed = if superseded { m.index != 100_000 && m.index != from } else { m.index == from };
                if landed && !shared.swap_pending.load(Ordering::Relaxed) && tl.write_pos() > m.frame + xf {
                    break m;
                }
                assert!(t0.elapsed() < Duration::from_secs(10), "the swap never landed past its crossfade");
                std::thread::sleep(Duration::from_millis(1));
            };
            let last = tl.write_pos() - 1;
            let (mut l, mut r) = (vec![0.0; 1], vec![0.0; 1]);
            tl.peek(last, &mut l, &mut r);
            let idx = shared.locate(last).unwrap().index;
            if superseded {
                assert_ne!(m.index, from, "the later swap went in");
                let (nl, nr) = Wave::at(k_later, idx);
                assert_eq!((l[0].to_bits(), r[0].to_bits()), (nl.to_bits(), nr.to_bits()), "the later swap's sound");
            } else {
                assert_eq!(m.index, from, "the redraw went in at its start");
                let (nl, nr) = Wave::at(K_NEW, idx);
                assert_eq!((l[0].to_bits(), r[0].to_bits()), (nl.to_bits(), nr.to_bits()), "the chain went on");
            }
            tx.send(Msg::Stop).unwrap();
        }
    }
}
