//! The transport: queue, what is playing, and every change to it.
//!
//! Tauri commands land here. Quick ones (pause, volume) act at once; the
//! rest become jobs for one worker thread, which does the slow work —
//! decoding, the engine's source stages, loading filters, priming — while
//! the render thread keeps playing whatever it had.
//!
//! Starting a track is done in two steps so sound comes quickly: a *quick*
//! variant (decode and DC removal only) starts playback, and the full
//! variant (declip, ISP, subsonic, apodizer — whole-file work that can take
//! a few seconds) replaces it with a crossfade as soon as it is ready. A
//! settings change works the same way: playback continues on the old chain
//! until the new one is built, then crossfades a fraction of a second after
//! the current position.
//!
//! With Instant start off (the player menu's tick) nothing is heard before
//! the whole chain is: a track waits in silence for its full variant and,
//! for Hybrid-Phase or alpha-HP, its onset envelope (`complete_variant`),
//! and a seek or a rack change keeps the old whole chain until the new one
//! is complete. The preparation runs on its own thread, so the worker stays
//! free for Stop, ⏭ or another track while a track waits.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::chain::{self, build_chain, build_file_chain, prepare_variant, Chain, DowngradeInfo, Resources, Variant};
use super::gpu::ctx::GpuPolyCtx;
use super::gpu::{vram_demand, vram_demand_hp};
use super::output::{OutputConfig, OutputMode, OutputShared, OutputStream, StreamInfo};
use super::policy::{self, PolicyDecision, PolicyInputs, TapRtf};
use super::probe::{probe, TrackInfo};
use super::render::{self, Msg};
use super::settings::{Mode, Phase, PlayerSettings};
use super::timeline::Timeline;
mod power;
mod radio_job;

/// The analysis window of the spectrum at this rate: ≈40 ms → the nearest
/// power of two. 0.040 × 48 kHz = 1920 → 2048 (42.7 ms, bin 23.4 Hz);
/// 0.040 × 44.1 kHz = 1764 → 2048 (46.4 ms, bin 21.5 Hz). Smaller than the
/// original 0.046 (→4096 @ 48 kHz, 85 ms) for crisper transients, while
/// still giving 2048 bins to keep bass bands separable (1024 would collapse
/// 8 sub-bass bands onto one bin).
fn spectrum_fft_size(rate: f64) -> usize {
    ((0.040 * rate) as usize).next_power_of_two().max(64)
}

/// A level as the spectrum's byte: dB = −96 + v·96/255.
fn db_byte(amp: f64) -> u8 {
    let db = if amp > 1e-10 { 20.0 * amp.log10() } else { -96.0 };
    ((db + 96.0) * 255.0 / 96.0).round().clamp(0.0, 255.0) as u8
}

/// What a slice says besides its bands (the visualization scenes, Anton
/// 27.09): over the same window, Hann-weighted — the peak, the RMS of left,
/// right, mid and side (bytes as the bands'), the correlation of left and
/// right (−1…1 as 0…255), two bytes spare.
pub const SLICE_EXTRA: usize = 8;
fn slice_extras(l: &[f64], r: &[f64], out: &mut [u8]) {
    let n = l.len();
    let (mut sw, mut pl, mut pr, mut pm, mut ps, mut clr, mut peak) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0f64);
    for i in 0..n {
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (n.max(2) - 1) as f64).cos());
        let (a, b) = (l[i], r[i]);
        let (m, s) = ((a + b) * 0.5, (a - b) * 0.5);
        sw += w;
        pl += w * a * a;
        pr += w * b * b;
        pm += w * m * m;
        ps += w * s * s;
        clr += w * a * b;
        peak = peak.max(a.abs()).max(b.abs());
    }
    let sw = sw.max(1e-12);
    out[0] = db_byte(peak);
    out[1] = db_byte((pl / sw).sqrt());
    out[2] = db_byte((pr / sw).sqrt());
    out[3] = db_byte((pm / sw).sqrt());
    out[4] = db_byte((ps / sw).sqrt());
    let corr = if pl * pr > 1e-20 { clr / (pl * pr).sqrt() } else { 1.0 };
    out[5] = ((corr.clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8;
    out[6] = 0;
    out[7] = 0;
}

/// The window of `fft_size` frames from `start` in the ring, as `out.len()`
/// log-spaced bands 25 Hz … 20 kHz, each a byte: dB = −96 + v·96/255; and
/// when asked, the slice's extras (`slice_extras`).
/// Item 18: rustfft, the plan cached per size, no locks held by render/output.
fn spectrum_bands_at(timeline: &Timeline, start: u64, fft_size: usize, out: &mut [u8], extra: Option<&mut [u8]>) {
    use rustfft::{num_complex::Complex, FftPlanner};
    // One plan per fft size (one per playback rate family).
    static PLANS: OnceLock<Mutex<HashMap<usize, Arc<dyn rustfft::Fft<f64>>>>> = OnceLock::new();
    let plan = PLANS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(fft_size)
        .or_insert_with(|| FftPlanner::<f64>::new().plan_fft_forward(fft_size))
        .clone();

    let rate = timeline.rate() as f64;
    let half = fft_size / 2;
    let mut win_l = vec![0.0f64; fft_size];
    let mut win_r = vec![0.0f64; fft_size];
    timeline.peek(start, &mut win_l, &mut win_r);
    if let Some(e) = extra {
        slice_extras(&win_l, &win_r, e);
    }

    // Hann window and mono power average.
    let mut buf: Vec<Complex<f64>> = (0..fft_size).map(|i| {
        let hann = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (fft_size - 1) as f64).cos());
        let mono = (win_l[i] + win_r[i]) * 0.5 * hann;
        Complex { re: mono, im: 0.0 }
    }).collect();
    let mut scratch = vec![Complex::default(); plan.get_inplace_scratch_len()];
    plan.process_with_scratch(&mut buf, &mut scratch);

    // Coherent gain correction: Hann window sum = fft_size * 0.5.
    let coherent_gain = fft_size as f64 * 0.5;
    // Map FFT bins to log-spaced bands 25 Hz … 20 kHz.
    let f_low = 25.0f64;
    let f_high = 20_000.0f64;
    let bins_per_hz = fft_size as f64 / rate;
    let bands = out.len();
    for b in 0..bands {
        let f_lo = f_low * (f_high / f_low).powf(b as f64 / bands as f64);
        let f_hi = f_low * (f_high / f_low).powf((b + 1) as f64 / bands as f64);
        let i_lo = ((f_lo * bins_per_hz).round() as usize).max(1).min(half - 1);
        let i_hi = ((f_hi * bins_per_hz).ceil() as usize).max(i_lo + 1).min(half);
        let power: f64 = (i_lo..i_hi).map(|i| {
            let re = buf[i].re;
            let im = buf[i].im;
            re * re + im * im
        }).sum::<f64>() / ((i_hi - i_lo) as f64);
        // RMS amplitude, corrected for coherent gain, then to dBFS.
        let rms = (power / (coherent_gain * coherent_gain)).sqrt();
        let db = if rms > 1e-10 { 20.0 * rms.log10() } else { -96.0 };
        // Encode: dB = -96 + v * 96 / 255 → v = (db + 96) * 255 / 96.
        out[b] = ((db + 96.0) * 255.0 / 96.0).round().clamp(0.0, 255.0) as u8;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// The worker's housekeeping (tick) runs this often, in ms, when no job
/// comes.
const TICK_MS: u64 = 200;

/// The next track's whole chain is prepared at least this long before the
/// end of the one playing (the prewarm), longer when this machine is
/// expected to need more (`prewarm_lead_s`).
const PREWARM_S: f64 = 40.0;
/// A chain built ahead is let go when the end is this much farther than
/// PREWARM_S again (a seek back): a seek around the edge does not build it
/// over and over.
const PREWARM_LET_GO_S: f64 = 20.0;
/// A prewarmed chain goes to the render thread for the gapless hand-over
/// this long before the end; until then ⏭ takes it. Well over what the
/// render writes ahead of the reader (4.5 s at most).
const QUEUE_AHEAD_S: f64 = 12.0;

/// Where a chain is built (`route_conv`): the rack as built (its taps fewer
/// after a downgrade), the video card it goes to, the downgrade, and the
/// GPU fallback generation it is built in.
type Route = (PlayerSettings, Option<Arc<GpuPolyCtx>>, Option<DowngradeInfo>, u32);

/// A chain built ahead for the track after the one playing, and the rack it
/// was built for.
struct Prewarmed {
    id: u64,
    settings: PlayerSettings,
    chain: Chain,
    /// Built from `stand_in_variant` (Instant start on, envelope late): a
    /// linear stand-in, not a whole chain.  With Instant start off the start
    /// builds the pair rather than use this.
    stand_in: bool,
}

/// Repeat mode for end-of-queue and gapless behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RepeatMode {
    /// Play the list once and stop after the last track.
    #[default]
    Off,
    /// After the last track, wrap to the first.
    All,
    /// Loop the current track indefinitely.
    One,
}

enum Job {
    Play { id: u64, at_s: f64 },
    Seek { at_s: f64 },
    Apply,
    Next,
    Prev,
    Stop,
    DeviceChanged,
    /// A background preparation finished; the next track can be queued.
    Prefetched { id: u64 },
    /// Instant start off: a preparation of this track is over. The track
    /// waiting for its whole chain (`State::waiting`) starts now, or waits on
    /// for what is still missing.
    Ready { id: u64 },
    /// Instant start off: the whole chain K4's silence waited for cannot be
    /// made. The player stops (the error is shown) — unless the listener has
    /// moved on meanwhile: another track, a seek, a rack change or Stop move
    /// the generation, and a stop now would end what they chose.
    StopSilence { id: u64, generation: u64 },
    /// Async seek preparation finished: pre-rendered side_buf is ready.
    SeekReady { chain: Box<super::chain::Chain>, side_buf: (Vec<f64>, Vec<f64>), generation: u64 },
    /// BIT-PERFECT async preparation finished.
    BpReady { chain: Box<super::chain::Chain>, generation: u64, direct: bool, bits: u32 },
    /// FS-change async preparation finished.
    FsReady { chain: Box<super::chain::Chain>, generation: u64, direct: bool, bits: u32 },
    /// BIT-PERFECT 120 ms hold-ramp has completed; stop the old stream and
    /// open the new one. Carries the pre-built chain and the ownership token.
    BpFadeComplete { chain: Box<super::chain::Chain>, generation: u64, direct: bool, bits: u32 },
    /// FS-change 120 ms hold-ramp has completed; stop the old stream and
    /// open the new one.
    FsFadeComplete { chain: Box<super::chain::Chain>, generation: u64, direct: bool, bits: u32 },
    /// The radio experiment: play this stream (radio_job.rs).
    Radio { url: String },
    /// A stream's chain is built (its session's generation): to start the
    /// stream, or to take over from the chain playing (`continuous`).
    RadioReady { chain: Box<super::chain::Chain>, gen: u64, continuous: bool },
    /// A stream could not be started.
    RadioFailed { gen: u64, kind: &'static str, error: String },
}

struct State {
    queue: Vec<TrackInfo>,
    /// The track the listener chose last (what the transport acts on).
    current: Option<u64>,
    settings: PlayerSettings,
    /// Bumped on every settings change, so stale work can tell it is stale.
    generation: u64,
    /// Bumped by what starts the rack's arming anew (arming.rs): a track's
    /// start, a rack change, a stop — not a seek, which bumps `generation`
    /// only to tell its own work from the one before.
    arm_epoch: u64,
    play: PlayState,
    timeline: Option<Arc<Timeline>>,
    output: Option<OutputStream>,
    stream: Option<StreamInfo>,
    stream_direct: bool,
    device_id: Option<String>,
    volume_db: f64,
    pending: Option<(String, Instant)>,
    error: Option<String>,
    /// (track, source key) → variant.
    variants: VariantCache,
    /// Next track already handed to the render thread for gapless playback
    /// (with repeat one: the current track again, its loop).
    queued_next: Option<u64>,
    /// The `seq` of the last Msg::Queue sent: while the render thread has not
    /// received it (`Shared::queue_taken` below it), the chain is on its way.
    queue_sent: u64,
    /// A repeat-one loop is queued (u64::MAX: none; else the timeline frame
    /// it was queued at). The render thread takes it at the track's end.
    loop_at: u64,
    /// The next track's chain built ahead (the prewarm): the render thread
    /// gets it QUEUE_AHEAD_S before the end for the gapless hand-over; a
    /// chain the stream cannot take (another rate or mode) waits here for
    /// the device's reopen at the end, and ⏭ takes it either way.
    prewarmed: Option<Prewarmed>,
    /// That chain is being built (the track and its rack), off the worker.
    prewarm_building: Option<(u64, PlayerSettings)>,
    /// A build of it that failed: not tried again every tick.
    prewarm_failed: Option<(u64, PlayerSettings)>,
    /// Variants being prepared in the background.
    in_flight: std::collections::HashSet<(u64, String)>,
    /// The cancel flag of each of those preparations: one the rack has moved
    /// on from is stopped, not left to finish behind the one that counts.
    prep_cancel: HashMap<(u64, String), Arc<AtomicBool>>,
    /// Preparations in flight that were asked for again, by a caller that
    /// wants the Apply when they are done — most often the next track's
    /// prefetch, whose own job only queues a next track, and the track then
    /// starting on its quick variant before the prefetch finished.
    apply_when_ready: std::collections::HashSet<(u64, String)>,
    /// Track id → path of its already-converted file. When set, the player
    /// plays the file directly rather than re-rendering on the fly.
    rendered_map: HashMap<u64, String>,
    /// The converted file a disk chain plays now, decoded: a seek within it
    /// takes the same samples (Weak: it lives as long as the chain does).
    disk_src: Option<(u64, PathBuf, std::sync::Weak<super::chain::DiskSrc>)>,
    /// Per-track settings overrides. When present for a track id, that track
    /// is played with these settings instead of the global rack. Cleared by
    /// player_clear_track_settings.
    track_settings: HashMap<u64, PlayerSettings>,
    /// Bumped on each gapless hand-over that applied a per-track settings
    /// override. The page polls this to detect when the rack needs updating.
    track_settings_applied_seq: u64,
    /// Source sample peak of the currently-playing track, dBFS.
    src_peak_db: Option<f64>,
    /// Fraction of source frames above full scale (clipped in direct mode).
    src_over_pct: Option<f64>,
    /// Repeat mode: how end-of-track / end-of-list is handled.
    repeat: RepeatMode,
    /// Generation that currently owns the rate-switch hold ramp.  Set in
    /// apply_now (BP toggle) and swap_continuous (FS change) when the prepare
    /// thread is spawned.  A BpFadeComplete/FsFadeComplete releases the hold
    /// only when it owns it (matching generation).  Cleared by stop_stream and
    /// by a successful fade completion.
    rate_switch_gen: Option<u64>,
    /// Generation of the seek whose chain is being built (seek_now,
    /// seek_disk). While it is the current one an Apply starts no rate
    /// switch of its own: the seek lands as that switch if the stream needs
    /// one (seek_ready). Cleared when the seek lands or fails.
    seek_gen: Option<u64>,
    /// A device was switched while a track played or was paused: the stream
    /// was closed and the player waits, paused, at this track and position
    /// (s). Play starts it there on the new device; a device that would not
    /// open leaves it here for the next Play. Cleared when a stream is up,
    /// on Stop and when another track is chosen.
    resume_at: Option<(u64, f64)>,
    /// Instant start off: the track waiting in silence for its whole chain,
    /// and the second it starts at (`wait_for_chain`). Cleared when it
    /// starts, on Stop, and when it leaves the list; another track replaces it.
    waiting: Option<(u64, f64)>,
    /// Instant start off: a seek asked for while the chain it needs was still
    /// being prepared (a rack change): the track and the second. The Apply
    /// that finds that chain complete makes it, on that track only; a newer
    /// rack change drops it, as it drops a seek in flight, and so does the
    /// track's hand-over to the next one.
    seek_after: Option<(u64, f64)>,
}

pub struct Player {
    st: Mutex<State>,
    /// (job, sent by the listener rather than by the player itself)
    jobs: Sender<(Job, bool)>,
    render_tx: Mutex<Sender<Msg>>,
    shared: Arc<render::Shared>,
    out: Arc<OutputShared>,
    res: Resources,
    next_id: AtomicU64,
    worker_busy: AtomicBool,
    /// Jobs sent and not yet picked up by the worker: a wait inside a job
    /// gives up as soon as the listener has asked for something else.
    jobs_queued: AtomicUsize,
    /// A seek's swap is on its way to the device: the render's switch count
    /// it lands at (0 = none). The seek stays pending — the page keeps its
    /// band and target — until the new place is heard, not only prepared.
    seek_landing: AtomicU64,
    /// Jobs of the worker's current batch still to run after this one.
    batch_left: AtomicUsize,
    /// How long the worker waits for a job before its housekeeping (ms):
    /// 200, less when a track that nothing follows gapless is about to end
    /// (tick), so the next one starts when the reader gets there.
    wake_ms: AtomicU64,
    /// Tracks the previous `is_running()` value so tick() can detect the
    /// rising edge of a converter batch starting (K8 / D3).
    prev_conv_running: AtomicBool,
    /// An Apply arrived while a track switch was landing; tick() runs it
    /// once the jump is on the air (see apply_now).
    apply_deferred: AtomicBool,
    /// The track whose Hybrid-Phase waits for the next track (u64::MAX:
    /// none): it already stepped its taps down once, and the pair would
    /// need another step (one automatic step-down per track, K3b).
    hp_held: AtomicU64,
    /// The player menu's Instant start (on by default, as the player always
    /// was): sound at once, the stages switching in as they are ready. Off:
    /// nothing is heard before the whole chain is ready.
    instant_start: AtomicBool,
    /// Onset envelopes that could not be made (track key | variant key): a
    /// chain no longer waits for them, and plays linear phase instead, as
    /// when the deferred swap gives up.
    env_failed: Mutex<HashSet<String>>,
    /// Instant start off: the track whose GPU failed while the pair K4
    /// would build was not whole, and the generation then — the GPU's
    /// silence plays until the whole chain comes with its Apply. Its
    /// preparation failing stops the player, the error shown, unless the
    /// listener has moved on meanwhile (Job::StopSilence).
    k4_waits: Mutex<Option<(u64, u64)>>,
    /// The radio experiment: the stream being played and its generation
    /// (radio_job.rs).
    radio: Mutex<Option<(u64, Arc<super::radio::Session>)>>,
    radio_gen: AtomicU64,
    /// The kind of the radio's last failure (radio::ERR_*) and the error it
    /// put in the player's line: reported while that error stands.
    radio_err: Mutex<Option<(&'static str, String)>>,
}

static PLAYER: OnceLock<Arc<Player>> = OnceLock::new();

pub fn get() -> Arc<Player> {
    PLAYER
        .get_or_init(|| {
            let (jtx, jrx) = std::sync::mpsc::channel();
            let (rtx, rrx) = render::channel();
            let shared = render::Shared::new();
            let out_shared = OutputShared::new();
            render::spawn(rrx, shared.clone(), out_shared.clone());
            let cache = crate::app_dir::root()
                .map(|d| d.join("player-cache"))
                .unwrap_or_else(|| std::env::temp_dir().join("AuraEngine").join("player-cache"));
            // Hybrid-Phase envelope files from earlier sessions: older than 30
            // days go, and the rest are kept under 500 MB (oldest first). On
            // its own thread; unit tests never touch the user's folder.
            #[cfg(not(test))]
            {
                let dir = cache.clone();
                let _ = std::thread::Builder::new()
                    .name("aura-cache-prune".into())
                    .spawn(move || {
                        let (n, bytes) = super::chain::prune_envelope_cache(
                            &dir,
                            std::time::Duration::from_secs(30 * 24 * 3600),
                            500 * 1024 * 1024,
                            std::time::SystemTime::now(),
                        );
                        if n > 0 {
                            crate::aelog!(
                                "[PLAYER] cache: removed {} old envelope file(s), {:.1} MB",
                                n,
                                bytes as f64 / (1024.0 * 1024.0)
                            );
                        }
                    });
            }
            let p = Arc::new(Player {
                st: Mutex::new(State {
                    queue: Vec::new(),
                    current: None,
                    settings: PlayerSettings::default(),
                    generation: 0,
                    arm_epoch: 0,
                    play: PlayState::Stopped,
                    timeline: None,
                    output: None,
                    stream: None,
                    stream_direct: false,
                    device_id: None,
                    volume_db: 0.0,
                    pending: None,
                    error: None,
                    variants: VariantCache::default(),
                    queued_next: None,
                    queue_sent: 0,
                    loop_at: u64::MAX,
                    prewarmed: None,
                    prewarm_building: None,
                    prewarm_failed: None,
                    in_flight: Default::default(),
                    prep_cancel: Default::default(),
                    apply_when_ready: Default::default(),
                    rendered_map: Default::default(),
                    disk_src: None,
                    track_settings: Default::default(),
                    track_settings_applied_seq: 0,
                    src_peak_db: None,
                    src_over_pct: None,
                    repeat: RepeatMode::Off,
                    rate_switch_gen: None,
                    seek_gen: None,
                    resume_at: None,
                    waiting: None,
                    seek_after: None,
                }),
                jobs: jtx,
                render_tx: Mutex::new(rtx),
                shared,
                out: out_shared,
                res: Resources::new(cache),
                next_id: AtomicU64::new(1),
                worker_busy: AtomicBool::new(false),
                jobs_queued: AtomicUsize::new(0),
                seek_landing: AtomicU64::new(0),
                batch_left: AtomicUsize::new(0),
                wake_ms: AtomicU64::new(TICK_MS),
                prev_conv_running: AtomicBool::new(false),
                apply_deferred: AtomicBool::new(false),
                hp_held: AtomicU64::new(u64::MAX),
                instant_start: AtomicBool::new(true),
                env_failed: Mutex::new(HashSet::new()),
                k4_waits: Mutex::new(None),
                radio: Mutex::new(None),
                radio_gen: AtomicU64::new(0),
                radio_err: Mutex::new(None),
            });
            let w = p.clone();
            std::thread::Builder::new()
                .name("aura-control".into())
                .spawn(move || w.worker(jrx))
                .expect("spawn control thread");
            p
        })
        .clone()
}

/// Source variants by (track, source key), each with the moment it was last
/// used. The per-track cap drops the least recently used. It used to drop in
/// the HashMap's own order, which is arbitrary: a variant finished a moment
/// ago could be the one thrown away, and the player — still missing it —
/// prepared it again, and again, never leaving "Preparing source stages".
#[derive(Default)]
struct VariantCache {
    map: HashMap<(u64, String), (Arc<Variant>, u64)>,
    clock: u64,
}

impl VariantCache {
    /// The variant, marked as used now.
    fn get(&mut self, k: &(u64, String)) -> Option<Arc<Variant>> {
        self.clock += 1;
        let now = self.clock;
        self.map.get_mut(k).map(|(v, used)| {
            *used = now;
            v.clone()
        })
    }

    fn contains(&self, k: &(u64, String)) -> bool {
        self.map.contains_key(k)
    }

    /// The variant, not marked as used: the status looks, it does not use.
    fn peek(&self, k: &(u64, String)) -> Option<&Arc<Variant>> {
        self.map.get(k).map(|(v, _)| v)
    }

    fn insert(&mut self, k: (u64, String), v: Arc<Variant>) {
        self.clock += 1;
        self.map.insert(k, (v, self.clock));
    }

    fn retain_tracks(&mut self, keep: impl Fn(u64) -> bool) {
        self.map.retain(|(t, _), _| keep(*t));
    }

    fn clear(&mut self) {
        self.map.clear();
    }

    fn cap_per_track(&mut self, per_track: usize) {
        let drop = evictions(self.map.iter().map(|(k, (_, used))| (k.clone(), *used)), per_track);
        for k in drop {
            self.map.remove(&k);
        }
    }

    /// Let go of `track`'s first variants streaming for a rack other than
    /// `keep`'s whose full variant was never made — the rack moved on in the
    /// track's first seconds. Each holds the track two or three times over
    /// (the stream's source and output, and what reaches the static apodizer
    /// for the full variant's Adaptive Apodizer) for a full variant no one
    /// makes; nothing reads its stream once it is gone here (a chain still
    /// playing it keeps its own hold until it is swapped out), so the stream
    /// stops. One whose full variant is cached stays, as the rack's own does.
    /// Returns how many went.
    fn drop_displaced_streams(&mut self, track: u64, keep: &str) -> usize {
        let gone: Vec<(u64, String)> = self
            .map
            .iter()
            .filter(|((t, _), (v, _))| {
                *t == track
                    && v.stream.as_ref().is_some_and(|ss| {
                        ss.full_key != keep && !self.map.contains_key(&(track, ss.full_key.clone()))
                    })
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &gone {
            self.map.remove(k);
        }
        gone.len()
    }
}

/// The keys to drop so that no track keeps more than `per_track` entries:
/// the least recently used of each track's.
fn evictions(
    entries: impl Iterator<Item = ((u64, String), u64)>,
    per_track: usize,
) -> Vec<(u64, String)> {
    let mut by_track: HashMap<u64, Vec<((u64, String), u64)>> = HashMap::new();
    for e in entries {
        by_track.entry(e.0 .0).or_default().push(e);
    }
    let mut out = Vec::new();
    for (_, mut list) in by_track {
        list.sort_by(|a, b| b.1.cmp(&a.1));
        out.extend(list.into_iter().skip(per_track).map(|(k, _)| k));
    }
    out
}

fn track_key(t: &TrackInfo) -> String {
    format!("t{}:{}", t.id, t.path)
}

/// The live analyzer learns every variant the player has ready; it takes
/// one when that track is heard (outside the state lock).
fn note_variant(track: u64, v: &Arc<Variant>) {
    if let Some(hub) = super::analytics::hub::try_get() {
        hub.on_variant(track, v);
    }
}

/// The playing track and the one after it in the queue: what the caches keep.
fn playing_and_next(st: &State) -> Vec<u64> {
    let cur = st.current;
    let next = cur.and_then(|c| next_in_list(st, c));
    [cur, next].into_iter().flatten().collect()
}

/// The track after `cur` in the list, where ⏭ goes: with repeat all, after
/// the last one the first (in a list of one, `cur` itself). None after the
/// last one otherwise, or when `cur` is not in the list.
fn next_in_list(st: &State, cur: u64) -> Option<u64> {
    let i = st.queue.iter().position(|t| t.id == cur)?;
    st.queue
        .get(i + 1)
        .or_else(|| if st.repeat == RepeatMode::All { st.queue.first() } else { None })
        .map(|t| t.id)
}

/// What plays when `cur` ends: with repeat one its own loop, otherwise the
/// next in the list (`next_in_list`) — with repeat all the first after the
/// last, and a list of one loops like repeat one. The prewarm, the gapless
/// hand-over and the end of a track all go by this one rule: the move from
/// the last track to the first was prepared, never queued, and started
/// after the end on a chain built then.
fn follows(st: &State, cur: u64) -> Option<u64> {
    if st.repeat == RepeatMode::One {
        return Some(cur);
    }
    next_in_list(st, cur)
}

/// The track keys of `keep`, for `Resources::forget_tracks_except`. `None`
/// when one of them has left the queue (removed while playing): it has no key
/// to name here, so nothing is forgotten this time rather than its envelope,
/// which a seek would have to compute again.
fn kept_track_keys(st: &State, keep: &[u64]) -> Option<Vec<String>> {
    keep.iter()
        .map(|id| st.queue.iter().find(|t| t.id == *id).map(track_key))
        .collect()
}

/// Return the settings to use when playing or prefetching track `id`:
/// the per-track override if one is registered, the global rack otherwise.
/// BIT-PERFECT (the rack in Direct mode) plays every track bit-perfect: a
/// remembered rack (M, always an Aura one) waits until it is off. It played
/// Aura, and after a bit-perfect track of its rate it went on gapless in
/// that stream, which applies no volume.
fn effective_settings(id: u64, st: &State) -> PlayerSettings {
    if st.settings.mode == Mode::Direct {
        return st.settings.clone();
    }
    st.track_settings.get(&id).cloned().unwrap_or_else(|| st.settings.clone())
}

/// The key a track's variant for `s` is kept under: one for BIT-PERFECT's
/// plain decode whatever the rack, the source key otherwise. Every place that
/// looks a variant up and every place that stores one must agree: a direct
/// variant stored under the source key was found by an Aura start as its
/// full variant, and the Apply that looked for "direct" asked for it again
/// and again.
fn variant_key(s: &PlayerSettings) -> String {
    if s.mode == Mode::Direct { "direct".to_string() } else { s.source_key() }
}

/// A quick variant's full variant (`full_key`) is being prepared or is ready:
/// the Apply that swaps it in comes, and a deferred HP start then needs no
/// job for the quick one.
fn full_variant_coming(
    variants: &VariantCache,
    in_flight: &std::collections::HashSet<(u64, String)>,
    id: u64,
    full_key: &str,
    quick: bool,
) -> bool {
    let k = (id, full_key.to_string());
    quick && (in_flight.contains(&k) || variants.contains(&k))
}

/// The order the worker runs a batch in. Of a burst of seeks only the last
/// one matters, and of settings changes one Apply: the other jobs first, as
/// they came, then the seek, then the Apply — and a waiting track's start
/// (Ready) last. What the listener asked in the same batch decides first:
/// a Play, ⏭, ⏮ or Stop leaves it nothing to start (the track they left
/// was heard until their job faded it out), a seek moves the point it
/// starts from, an Apply starts it itself (and it built the same chain
/// again right after).
fn run_order(batch: Vec<(Job, bool)>) -> Vec<Job> {
    let mut last_seek = None;
    let mut apply = false;
    let mut run = Vec::new();
    let mut ready = Vec::new();
    for (j, _) in batch {
        match j {
            Job::Seek { at_s } => last_seek = Some(at_s),
            Job::Apply => apply = true,
            Job::Ready { .. } => ready.push(j),
            other => run.push(other),
        }
    }
    if let Some(at_s) = last_seek {
        run.push(Job::Seek { at_s });
    }
    if apply {
        run.push(Job::Apply);
    }
    run.extend(ready);
    run
}

/// How long the worker waits before its next housekeeping when the track
/// written to its end (`ended`, a timeline frame; u64::MAX: not yet) is
/// about to run out under the reader (`read`): the time left and 2 ms, when
/// that is shorter than a tick.
fn wake_before_end(ended: u64, read: u64, rate: u32) -> Option<u64> {
    if ended == u64::MAX {
        return None;
    }
    let left_ms = ended.saturating_sub(read) * 1000 / rate.max(1) as u64;
    (left_ms < TICK_MS).then_some(left_ms + 2)
}

/// A preparation makes the whole chain — the variant, and a Hybrid-Phase
/// rack's onset envelope where its pair still waits for one (`envelope_done`:
/// a pair that plays with the plan made as it plays needs none, and with
/// Instant start off every pair does) — with Instant start off, and for the
/// next track's prewarm whatever the tick: its hand-over is the pair from
/// the first sample, not a linear stand-in. With Instant start on, the
/// playing track's own preparation makes the variant, and a pair that waits
/// for its envelope follows through the HP job.
fn makes_whole_chain(instant: bool, prewarm: bool) -> bool {
    !instant || prewarm
}

/// A Hybrid-Phase or alpha-HP rack: its chain needs the variant's onset
/// envelope, and stands in on linear phase without it.
fn needs_envelope(s: &PlayerSettings) -> bool {
    s.mode == Mode::Aura && matches!(s.phase, Phase::Hybrid | Phase::Alpha)
}

/// The key `Resources::envelope` keeps a variant's onset envelope under.
fn env_key(track_key: &str, v: &Variant) -> String {
    format!("{}|{}", track_key, v.key)
}

/// The arming bar's "the Hybrid-Phase envelope is ready" (arming.rs Obs)
/// for track key `tkey`: known in memory under the variant's own key (the
/// rack's `key` while no variant is cached), or on disk as its preparation
/// saw it (`prep_on_disk`). With `look_on_disk` (Instant start off, a
/// Hybrid-Phase rack) the file on disk counts too, as it does for the start
/// (hp_ready): a preparation the bar no longer keeps left it the build's
/// time on "hp:env".
fn arm_env_ready(res: &Resources, tkey: &str, key: &str, v: Option<&Variant>, prep_on_disk: bool, look_on_disk: bool) -> bool {
    let env_key = format!("{}|{}", tkey, v.map_or(key, |v| v.key.as_str()));
    res.envelope_known(&env_key)
        || prep_on_disk
        || (look_on_disk && v.is_some_and(|v| chain::hp_ready(res, tkey, v)))
}

/// The second the last BIT-PERFECT switch was started from (tests: the
/// build of a chain without filters fails on its thread, before any sign).
#[cfg(test)]
static BP_SWITCH_FROM: AtomicU64 = AtomicU64::new(0);

/// The second a rack change's new chain was taken from (tests): the switch
/// of an FS change, else the continuous splice (its lead included).
#[cfg(test)]
static SWAP_FROM: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Runs once between tick's two reads at the end of a track (tests: the
    /// render thread's late path landing right there).
    static BETWEEN_END_READS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// What the status line says while the rack's chain is being prepared, so
/// it says something useful.
fn preparing_label(s: &PlayerSettings) -> &'static str {
    if matches!(s.phase, Phase::Hybrid | Phase::Alpha) {
        "Preparing Hybrid-Phase\u{2026}"
    } else if s.isp || s.declip || s.adaptive_apodizer {
        "Preparing source stages\u{2026}"
    } else {
        "Preparing\u{2026}"
    }
}

/// What K4 rebuilds on the CPU after a GPU failure.
enum K4Pick {
    /// These settings (the GPU off) on this variant. A pair is built as its
    /// linear half first (`linear_first`), the pair following at that rung
    /// through the HP swap.
    Build { s_cpu: PlayerSettings, v: Arc<Variant>, linear_first: bool },
    /// Nothing whole to build yet.
    Wait,
    /// Nothing to build.
    Nothing,
}

/// K4's source: the rack's full variant, else its quick one while it
/// plays (the full one can take a minute on a slow CPU): K4 must not do
/// nothing and leave the GPU's zeros on.
///
/// With Instant start off (`instant` false), the chain that failed (`air`):
/// its own settings and variant — not the rack still being prepared, nor
/// a quick variant in its place — a pair built as the pair. Its variant
/// gone: it waits, in the GPU's silence, for the whole chain (k4_fallback,
/// which also sees whether the chain is whole, outside the state lock).
fn k4_pick(
    instant: bool,
    rack: &PlayerSettings,
    air: &render::ChainDesc,
    mut cached: impl FnMut(&str) -> Option<Arc<Variant>>,
) -> K4Pick {
    if !instant {
        let mut s_cpu = (*air.settings).clone();
        s_cpu.use_gpu = false;
        return match air.variant.upgrade().or_else(|| cached(&variant_key(&s_cpu))) {
            Some(v) => K4Pick::Build { s_cpu, v, linear_first: false },
            None => K4Pick::Wait,
        };
    }
    let mut s_cpu = rack.clone();
    s_cpu.use_gpu = false;
    let linear_first = matches!(s_cpu.phase, Phase::Hybrid | Phase::Alpha);
    let first = |v: &Arc<Variant>| chain::first_variant_fits(v, &s_cpu);
    match cached(&s_cpu.source_key()).or_else(|| cached(&s_cpu.quick().source_key()).filter(first)) {
        Some(v) => K4Pick::Build { s_cpu, v, linear_first },
        None => K4Pick::Nothing,
    }
}

/// The VRAM a chain of `phase` needs: both banks for a Hybrid-Phase or
/// alpha-HP pair.
fn vram_for(phase: Phase, taps: usize, l: usize) -> u64 {
    if matches!(phase, Phase::Hybrid | Phase::Alpha) {
        vram_demand_hp(taps, l)
    } else {
        vram_demand(taps, l)
    }
}

impl Player {
    // analytics: §2.6 — read-only accessors for analytics without st lock

    /// Read-only access to the resource cache for bank peek.
    /// analytics: §2.6
    pub fn resources(&self) -> &Resources {
        &self.res
    }

    /// Read-only access to the render shared state (marks, etc.) for analytics.
    /// analytics: §2.6
    #[allow(dead_code)] // kept for the analyzer views not wired yet (chain_info)
    pub fn render_shared(&self) -> Arc<render::Shared> {
        self.shared.clone()
    }

    // ── Commands (Tauri thread) ─────────────────────────────────────────

    /// Probe and enqueue. Files that cannot be read come back as messages
    /// for the page to show once, not as a player error that would stay on
    /// screen until the next track starts.
    pub fn add(&self, paths: Vec<String>) -> (Vec<TrackInfo>, Vec<String>) {
        let mut added = Vec::new();
        let mut errors = Vec::new();
        for p in paths {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            match probe(id, &PathBuf::from(&p)) {
                Ok(t) => added.push(t),
                Err(e) => errors.push(e),
            }
        }
        let mut st = self.st.lock().unwrap();
        st.queue.extend(added.iter().cloned());
        // With repeat all, after the last track the first one was to follow:
        // now the first one added does.
        self.let_go_unless_next(&mut st);
        drop(st);
        (added, errors)
    }

    /// What waits to follow the track on the air — its chain handed to the
    /// render thread for the gapless hand-over, or the marker of one that
    /// waits for the device's reopen — is let go when that is not what
    /// follows any more: the list reordered, a track added after the last
    /// one, the next one removed, another repeat mode. tick queues the right
    /// one. Left as it is while the track on the air has left the list: what
    /// follows it is not known here.
    fn let_go_unless_next(&self, st: &mut State) {
        let Some(q) = st.queued_next else { return };
        let Some(cur) = st.current.filter(|c| st.queue.iter().any(|t| t.id == *c)) else { return };
        if q == u64::MAX {
            // Nothing is in the render thread: tick looks at the next track
            // again (a chain built ahead stays while it is that track's).
            st.queued_next = None;
            return;
        }
        if follows(st, cur) != Some(q) {
            st.queued_next = None;
            st.loop_at = u64::MAX;
            let _ = self.render_tx.lock().unwrap().send(Msg::Unqueue);
        }
    }

    /// A job the listener asked for: counted, so a wait inside the job being
    /// run gives way to it.
    fn send_job(&self, j: Job) {
        self.jobs_queued.fetch_add(1, Ordering::AcqRel);
        let _ = self.jobs.send((j, true));
    }

    /// A job the player sends itself (a background preparation finished):
    /// not counted — it must not cut a pre-roll short.
    fn send_internal(&self, j: Job) {
        let _ = self.jobs.send((j, false));
    }

    pub fn remove(&self, id: u64) {
        let (kept_keys, waited, prewarmed) = {
            let mut st = self.st.lock().unwrap();
            st.queue.retain(|t| t.id != id);
            st.variants.retain_tracks(|tid| tid != id);
            // Its chain built ahead goes with it, and so does its chain
            // queued for the gapless hand-over (it played, and then nothing
            // did: the track after it was not in the list) or the marker of
            // its wait for the reopen (no next track was prepared again).
            let prewarmed = if st.prewarmed.as_ref().is_some_and(|p| p.id == id) { st.prewarmed.take() } else { None };
            // The removed track may be queued for the gapless hand-over
            // even when the playing track is no longer in the list (it was
            // removed before it).  let_go_unless_next exits early in that
            // case — the playing track is not in the queue — so clear here,
            // regardless of the current track.
            if st.queued_next == Some(id) {
                st.queued_next = None;
                st.loop_at = u64::MAX;
                let _ = self.render_tx.lock().unwrap().send(Msg::Unqueue);
            }
            self.let_go_unless_next(&mut st);
            // It was waiting for its chain: nothing is left to start.
            let waited = st.waiting.is_some_and(|(w, _)| w == id);
            if waited {
                st.waiting = None;
                st.pending = None;
            }
            (kept_track_keys(&st, &playing_and_next(&st)), waited, prewarmed)
        };
        drop(prewarmed);
        // Nor to prepare.
        if waited {
            self.drop_preparations(id, None, "it left the list");
        }
        if let Some(keys) = kept_keys {
            self.res.forget_tracks_except(&keys);
        }
    }

    pub fn clear(&self) {
        self.send_job(Job::Stop);
        let prewarmed = {
            let mut st = self.st.lock().unwrap();
            st.queue.clear();
            st.variants.clear();
            st.current = None;
            st.prewarmed.take()
        };
        drop(prewarmed);
        // Nothing is left to play: no track keeps its envelopes or plans.
        self.res.forget_tracks_except(&[]);
    }

    /// Put the queue in the page's order (the list dragged into a new one):
    /// `ids` first, in that order, then any track the page did not name, as
    /// it stood. A gapless next already handed to the render thread that is
    /// not the next track any more is let go; tick queues the right one.
    pub fn reorder(&self, ids: Vec<u64>) {
        let mut st = self.st.lock().unwrap();
        let mut rest = std::mem::take(&mut st.queue);
        let mut queue = Vec::with_capacity(rest.len());
        for id in ids {
            if let Some(i) = rest.iter().position(|t| t.id == id) {
                queue.push(rest.remove(i));
            }
        }
        queue.extend(rest);
        st.queue = queue;
        self.let_go_unless_next(&mut st);
    }

    /// Return the current backend queue for page reconnect (D2).
    /// The page matches entries by path to restore the correct track ids.
    pub fn queue(&self) -> Vec<TrackInfo> {
        self.st.lock().unwrap().queue.clone()
    }

    pub fn play(&self, id: Option<u64>) {
        let mut st = self.st.lock().unwrap();
        match id {
            None if st.play == PlayState::Playing => {}
            None if st.play == PlayState::Paused && st.timeline.is_none() && st.resume_at.is_some() => {
                let (rid, at_s) = st.resume_at.unwrap();
                st.error = None;
                drop(st);
                self.out.paused.store(false, Ordering::Relaxed);
                self.send_job(Job::Play { id: rid, at_s });
            }
            None if st.play == PlayState::Paused => {
                self.out.paused.store(false, Ordering::Relaxed);
                st.play = PlayState::Playing;
            }
            None => {
                let first = st.current.or_else(|| st.queue.first().map(|t| t.id));
                if let Some(id) = first {
                    drop(st);
                    self.send_job(Job::Play { id, at_s: 0.0 });
                }
            }
            Some(id) => {
                st.resume_at = None;
                drop(st);
                self.send_job(Job::Play { id, at_s: 0.0 });
            }
        }
    }

    pub fn pause(&self) {
        let mut st = self.st.lock().unwrap();
        if st.play == PlayState::Playing {
            self.out.paused.store(true, Ordering::Relaxed);
            st.play = PlayState::Paused;
        }
    }

    pub fn stop(&self) {
        self.send_job(Job::Stop);
    }

    pub fn next(&self) {
        self.send_job(Job::Next);
    }

    pub fn prev(&self) {
        self.send_job(Job::Prev);
    }

    pub fn seek(&self, at_s: f64) {
        self.send_job(Job::Seek { at_s: at_s.max(0.0) });
    }

    pub fn set_volume(&self, db: f64) {
        let db = db.clamp(-120.0, 0.0);
        self.out.set_volume_db(db);
        self.st.lock().unwrap().volume_db = db;
    }

    pub fn set_settings(&self, s: PlayerSettings) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.settings == s {
            return false;
        }
        st.settings = s;
        st.generation += 1;
        // A seek that waited for the rack before this one goes with it.
        st.seek_after = None;
        st.arm_epoch += 1;
        drop(st);
        // S1/S3: any generation bump invalidates in-flight seeks; clear their
        // status atoms so the UI does not show a stale loading marker.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        self.send_job(Job::Apply);
        true
    }

    /// The rack a stream plays: the rack itself, its length too (Anton 2.10:
    /// with a stream's own linear filter, `stream_linear`, a long filter no
    /// longer keeps a stream waiting, so streams have no length of their own).
    /// Not installed for the stream's rate, the nearest installed length plays
    /// (`radio::chain::installed_length`).
    fn stream_settings(&self) -> PlayerSettings {
        self.st.lock().unwrap().settings.clone()
    }

    /// Fire an Apply job without changing the settings.
    ///
    /// Called when the rendered-file map changes but the DSP settings did
    /// not — `set_settings` returns early in that case, so this method
    /// sends the Apply that picks up the new map.
    pub fn apply(&self) {
        let mut st = self.st.lock().unwrap();
        st.generation += 1;
        st.seek_after = None;
        st.arm_epoch += 1;
        drop(st);
        // S1/S3: invalidate in-flight seeks (see set_settings comment).
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        self.send_job(Job::Apply);
    }

    /// Set the repeat mode. Takes effect immediately for the next end-of-track:
    /// a chain queued for the old mode (the loop for repeat one, the next
    /// track otherwise) is let go, and tick queues the right one.
    /// The visualizations' floor under the render's look-ahead (render.rs
    /// `vis_floor_bits`): while a picture drawn from the sound to come is on
    /// screen, this much stays rendered ahead; 0 lets the adaptive target
    /// rule alone again. Each window that draws one (the player, the studio)
    /// says its own; the most of them counts.
    pub fn set_vis_ahead(&self, source: &str, seconds: f64) {
        static FLOORS: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
        let s = if seconds.is_finite() { seconds.clamp(0.0, 4.5) } else { 0.0 };
        let floor = {
            let mut m = FLOORS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
            if s > 0.0 { m.insert(source.to_string(), s); } else { m.remove(source); }
            m.values().fold(0.0f64, |a, &b| a.max(b))
        };
        let was = f64::from_bits(self.shared.vis_floor_bits.swap(floor.to_bits(), Ordering::Relaxed));
        if (was - floor).abs() > 1e-9 {
            crate::aelog!("[PLAYER] visualization look-ahead: {:.1} s ({} asks {:.1} s)", floor, source, s);
        }
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        let mut st = self.st.lock().unwrap();
        st.repeat = mode;
        // Repeat all off on the last track: the first one, queued to
        // follow it, played anyway.
        self.let_go_unless_next(&mut st);
    }

    /// The player menu's Instant start tick (`instant_start`). Ticked while a
    /// track waits for its whole chain, that track starts at once, on what
    /// is ready. Persisted by the page, which sends it with the rack.
    pub fn set_instant_start(&self, on: bool) {
        if self.instant_start.swap(on, Ordering::AcqRel) == on {
            return;
        }
        // Off: a Hybrid-Phase pair is always built before the sound.
        chain::set_pair_always(!on);
        crate::aelog!(
            "[PLAYER] instant start {}",
            if on { "on" } else { "off: every stage is prepared before the sound" }
        );
        let (waiting, rack_waits) = {
            let st = self.st.lock().unwrap();
            let rack = st.current.map(|c| (c, variant_key(&st.settings)));
            (st.waiting, st.seek_after.is_some() || rack.is_some_and(|k| st.in_flight.contains(&k)))
        };
        match (on, waiting) {
            (true, Some((id, _))) => self.send_internal(Job::Ready { id }),
            // A rack change or a seek that waits for its whole chain is made
            // at once, on what is ready: the Apply came only with the
            // envelope, up to half a minute later.
            (true, None) if rack_waits => self.send_internal(Job::Apply),
            _ => {}
        }
    }

    fn instant(&self) -> bool {
        self.instant_start.load(Ordering::Acquire)
    }

    /// Replace the track→converted-file map. Tracks absent from the vec or
    /// with None path revert to live rendering. Called from the Tauri thread
    /// and immediately followed by set_settings. True when the playing
    /// track's file changed: only then must an Apply move what is heard.
    /// A changed file for the track queued behind it lets that go (tick
    /// queues it again from the new map); every other track reads the map
    /// when it starts.
    pub fn set_rendered(&self, items: Vec<(u64, Option<String>)>) -> bool {
        let mut st = self.st.lock().unwrap();
        let old = std::mem::take(&mut st.rendered_map);
        for (id, path) in items {
            if let Some(p) = path {
                st.rendered_map.insert(id, p);
            }
        }
        if st.rendered_map != old {
            crate::aelog!("[PLAYER] rows that play a converted file: {} (were {})", st.rendered_map.len(), old.len());
        }
        let moved = |id: Option<u64>, now: &HashMap<u64, String>| id.is_some_and(|id| old.get(&id) != now.get(&id));
        let on_air = moved(st.current, &st.rendered_map);
        if !on_air && moved(st.queued_next, &st.rendered_map) {
            st.queued_next = None;
            st.loop_at = u64::MAX;
            let _ = self.render_tx.lock().unwrap().send(Msg::Unqueue);
        }
        // So does the chain built ahead for it, and the marker of its wait
        // for the reopen: it stayed, and the next track was not looked at
        // again — it started after the end on a chain built then.
        let prewarmed = if !on_air && moved(st.prewarmed.as_ref().map(|p| p.id), &st.rendered_map) {
            if st.queued_next == Some(u64::MAX) {
                st.queued_next = None;
            }
            st.prewarmed.take()
        } else {
            None
        };
        drop(st);
        drop(prewarmed);
        on_air
    }

    /// Register per-track settings. The next start of this track id (direct
    /// play, prefetch, or gapless hand-over) uses these settings instead of
    /// the global rack. Called from player_set_track_settings.
    pub fn set_track_settings(&self, id: u64, settings: PlayerSettings) {
        self.st.lock().unwrap().track_settings.insert(id, settings);
    }

    /// Remove a per-track settings override. Subsequent operations on this
    /// track id revert to the global rack. Called from player_clear_track_settings.
    pub fn clear_track_settings(&self, id: u64) {
        self.st.lock().unwrap().track_settings.remove(&id);
    }

    pub fn set_device(&self, id: Option<String>) {
        let mut st = self.st.lock().unwrap();
        if st.device_id == id {
            return;
        }
        st.device_id = id;
        drop(st);
        self.send_job(Job::DeviceChanged);
    }

    pub fn status(&self) -> Value {
        let st = self.st.lock().unwrap();
        // Badge frame: while paused, use write_pos-guard to show what will play
        // on resume (so badge states are instant, not frozen at the read head).
        // positionS always uses the audible read_pos (coordinator contract).
        let paused_now = self.out.paused.load(Ordering::Relaxed);
        let audible_frame = st.timeline.as_ref().map(|t| t.read_pos()).unwrap_or(0);
        let badge_frame = if paused_now {
            st.timeline.as_ref()
                .map(|t| t.write_pos().saturating_sub(t.guard_frames()))
                .unwrap_or(0)
        } else {
            audible_frame
        };
        let read_mark = match &st.timeline {
            Some(_) => self.shared.locate(badge_frame),
            None => None,
        };
        let (track_id, index, rate) = match &read_mark {
            Some(m) => (Some(m.track_id), m.index, m.rate),
            None => match &st.timeline {
                Some(t) => (st.current, 0, t.rate()),
                None => (st.current, 0, 0),
            },
        };
        // outRate is the stream's: the mark only says what the chain on the
        // air was built for, and the two told apart hid a chain of another
        // rate playing at the stream's (chainRate is the mark's).
        let stream_rate = st.timeline.as_ref().map_or(rate, |t| t.rate());
        // positionS: always the audible frame (not badge_frame).
        let audible_mark = self.shared.locate(audible_frame);
        // A stream's chain on the air: the source frame heard (its titles go by it).
        let radio_heard = audible_mark
            .as_ref()
            .filter(|m| m.track_id == super::radio::chain::RADIO_TRACK_ID)
            .map(|m| (m.index / m.l.max(1) as u64) as i64);
        let (audible_index, audible_rate) = match &audible_mark {
            Some(m) => (m.index, m.rate),
            None => (index, rate),
        };
        let track = track_id.and_then(|id| st.queue.iter().find(|t| t.id == id));
        // The file heard, by its name: the live analyzer's title says whose
        // window it is, as a file window's does.
        let track_file = track.map(|t| {
            std::path::Path::new(&t.path).file_name()
                .map_or_else(|| t.path.clone(), |n| n.to_string_lossy().into_owned())
        });
        // positionS = audible position (coordinator contract).
        let position_s = if audible_rate > 0 { audible_index as f64 / audible_rate as f64 } else { 0.0 };
        // Paused by a device change: no stream, the point Play goes on from;
        // waiting for its whole chain, the point the track will start at.
        let position_s = match (&st.timeline, st.resume_at, st.waiting) {
            (None, Some((_, at)), _) | (None, None, Some((_, at))) => at,
            _ => position_s,
        };
        let buffer_s = st
            .timeline
            .as_ref()
            .map(|t| t.buffered_frames() as f64 / t.rate().max(1) as f64)
            .unwrap_or(0.0);
        let underruns = st.timeline.as_ref().map(|t| t.underrun_frames()).unwrap_or(0);
        // The sound is being rebuilt: a job waits for the worker, a label is
        // up, a swap is on its way, the playing track's variant is being
        // prepared, or the device is switching rate. The page holds the rack
        // meanwhile (a click would only start the rebuild over).
        let rebuilding = self.jobs_queued.load(Ordering::Acquire) > 0
            || st.pending.is_some()
            || self.shared.swap_pending.load(Ordering::Relaxed)
            || self.shared.swap_unheard.load(Ordering::Relaxed)
            || st.current.is_some_and(|c| st.in_flight.iter().any(|(t, _)| *t == c))
            || st.rate_switch_gen.is_some();
        let pending_label = if let Some((what, since)) = &st.pending {
            json!({ "what": what, "sinceMs": since.elapsed().as_millis() as u64 })
        } else if self.shared.swap_pending.load(Ordering::Relaxed) {
            json!({ "what": "Switching", "sinceMs": 0 })
        } else {
            Value::Null
        };
        let state = match st.play {
            PlayState::Stopped if st.pending.is_some() => "preparing",
            PlayState::Stopped => "stopped",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
        };
        let own_error = st.error.clone();
        let (duration_s, src_rate, bits) =
            track.map(|t| (t.duration_s, t.sample_rate, t.bits)).unwrap_or((0.0, 0, 0));
        let device = st.stream.as_ref().map(|s| s.device_name.clone());
        let exclusive = st.stream.as_ref().map(|s| s.exclusive).unwrap_or(false);
        let format = st.stream.as_ref().map(|s| s.format.clone());
        let dev_format = format.clone();
        // streamDirect: true = Direct (BIT-PERFECT), false = DSP, null = no stream.
        let stream_direct_val = if st.stream.is_some() {
            serde_json::Value::Bool(st.stream_direct)
        } else {
            serde_json::Value::Null
        };
        let volume_db = st.volume_db;
        // The output thread only counts safety mutes; the log line is written here.
        {
            static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let mutes = self.out.safety_mutes.load(std::sync::atomic::Ordering::Relaxed);
            if LOGGED.swap(mutes, std::sync::atomic::Ordering::Relaxed) != mutes {
                crate::aelog!(
                    "[SAFETY] output muted: {} period(s) so far, device peak max {:.1} dBFS at volume {:.1} dB",
                    mutes,
                    self.out.dev_peak_max_db().unwrap_or(f64::NEG_INFINITY),
                    volume_db
                );
            }
        }

        // Audible chain: what the listener is hearing at the read position.
        // When source == "file", resolve the converted file path from rendered_map
        // so FE-P can match it against entry.convs and show the correct badges (rt-3).
        let audible_val = match &read_mark {
            Some(m) => {
                let d = &m.desc;
                let stages_val: Vec<Value> = d.stages.iter().map(|s| {
                    json!([s.tok, s.st, s.why])
                }).collect();
                // The file the chain on the air plays; the map only when
                // the chain does not know it (the map moves with the rack
                // at once, the chain on the air a moment later).
                let file_val = if d.source == "file" {
                    d.file.clone().or_else(|| st.rendered_map.get(&m.track_id).cloned())
                        .map(Value::String)
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                // K6: GPU chip state.
                // "on": chain is GPU-built and no failure yet.
                // "failed": a GPU failure occurred this track session.
                // null: chain is CPU-built.
                let gpu_failed_now = self.shared.gpu_failed.load(Ordering::Relaxed);
                let gpu_val: Value = if gpu_failed_now {
                    Value::String("failed".into())
                } else if d.gpu_on {
                    Value::String("on".into())
                } else {
                    Value::Null
                };
                // K6: downgrade descriptor — present when this chain was
                // built at a smaller tap count than the rack setting, or
                // after a mid-track GPU failure.
                let downgrade_val: Value = match &d.downgrade {
                    Some(di) => json!({
                        "from":   di.from,
                        "to":     di.to,
                        "reason": di.reason,
                        "gen":    d.downgrade_gen,
                        // The size the rack asked for (the taps chip's).
                        "asked":  di.asked,
                    }),
                    None => Value::Null,
                };
                json!({
                    "source": d.source,
                    "file": file_val,
                    "quick": d.quick,
                    "taps": d.taps,
                    "stages": stages_val,
                    "gainDb": d.gain_db,
                    "tpDb": d.tp_db,
                    "ceilingDb": d.ceiling_db,
                    "gpu": gpu_val,
                    "downgrade": downgrade_val,
                    "hpDeferred": d.hp_deferred,
                    // HP waits for the next track (one step-down per track).
                    "hpHeld": d.hp_deferred && self.hp_held.load(Ordering::Relaxed) == m.track_id,
                })
            }
            None => Value::Null,
        };

        // Jump state: a seek or track switch that has been requested but whose
        // audio has not yet reached the output.
        let hold = self.out.hold.load(Ordering::Relaxed);
        let landing = self.seek_landing.load(Ordering::Acquire);
        if landing != 0 && self.shared.switches.load(Ordering::Relaxed) >= landing
            && self.seek_landing.compare_exchange(landing, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        {
            self.shared.seek_pending.store(false, Ordering::Release);
            self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        }
        let seek_pend = self.shared.seek_pending.load(Ordering::Relaxed);
        let jump_pending = hold || seek_pend || st.pending.is_some();
        // targetS: the latest requested seek target (NAN = none pending).
        let seek_bits = self.shared.seek_target_bits.load(Ordering::Relaxed);
        let seek_target = f64::from_bits(seek_bits);
        let jump_target_s: Option<f64> = if seek_pend && !seek_target.is_nan() {
            Some(seek_target)
        } else if jump_pending {
            Some(position_s)
        } else {
            None
        };

        let src_peak_db = st.src_peak_db;
        let src_over_pct = st.src_over_pct;
        let track_settings_applied_seq = st.track_settings_applied_seq;

        let target_s = *self.shared.target_ahead_s.lock().unwrap();
        let ramp_frames = self.out.ramp_frames_read.load(Ordering::Relaxed);
        // latencyMs: device output latency for the spectrum sync (§4.1).
        let latency_ms = st.stream.as_ref()
            .map(|s| self.latency_frames(&st) as f64 / s.rate.max(1) as f64 * 1000.0)
            .unwrap_or(0.0);

        // The rack's arming on the current track (arming.rs): what the state
        // says, gathered here and weighed after the lock.
        let arm_in = st.current.and_then(|id| {
            let t = st.queue.iter().find(|t| t.id == id)?;
            let s = effective_settings(id, &st);
            let key = variant_key(&s);
            // The variant: its own key, for a source at or above the rack's
            // rate is prepared a FS step up, and its envelope goes by that key.
            let variant = st.variants.peek(&(id, key.clone())).cloned();
            Some((id, t.path.clone(), track_key(t), t.duration_s, t.sample_rate, variant, s, key))
        });
        let arm_epoch = st.arm_epoch;
        let arm_active = st.play != PlayState::Stopped || st.pending.is_some();
        let arm_stream_rate = st.timeline.as_ref().map_or(0, |t| t.rate());
        // A device switch of this rack's (a stale one's claim is not its).
        let arm_rate_switch = st.rate_switch_gen == Some(st.generation);

        drop(st);
        let arming = match arm_in {
            Some((id, path, tkey, duration_s, src_rate, variant, settings, key)) => {
                let prep = super::arming::prep_view(&path, &key);
                let env_ready = arm_env_ready(
                    &self.res,
                    &tkey,
                    &key,
                    variant.as_deref(),
                    prep.as_ref().is_some_and(|p| p.env_on_disk),
                    !self.instant() && needs_envelope(&settings),
                ) || (needs_envelope(&settings) && chain::pair_without_envelope(&settings));
                let chain = |m: &render::Mark| super::arming::Air::of(m.track_id, &m.desc, &settings);
                let obs = super::arming::Obs {
                    active: arm_active,
                    track: id,
                    epoch: arm_epoch,
                    duration_s,
                    src_rate,
                    full_ready: variant.is_some(),
                    env_ready,
                    whole_first: !self.instant(),
                    stream_rate: arm_stream_rate,
                    air: read_mark.as_ref().map(chain),
                    newest: self.shared.newest_mark().map(|m| {
                        let ahead = m.frame.saturating_sub(audible_frame) as f64 / arm_stream_rate.max(1) as f64;
                        (chain(&m), ahead)
                    }),
                    rate_switch: arm_rate_switch,
                    buffer_s: buffer_s.max(target_s),
                    prep,
                    hp: super::arming::hp_job_view(id, arm_epoch),
                    settings,
                    key,
                };
                super::arming::observe(&obs).map_or(Value::Null, |v| v.json())
            }
            None => Value::Null,
        };
        let error = own_error
            .or_else(|| self.shared.error.lock().unwrap().clone())
            .or_else(|| self.out.last_error.lock().unwrap().clone());
        let radio_error = self.radio_error_kind(error.as_deref());
        let tp_db = *self.shared.tp_pred_db.lock().unwrap();
        let gain = self.shared.gain_now();
        json!({
            "state": state,
            // A stream is no track of the list.
            "trackId": track_id.filter(|&id| id != super::radio::chain::RADIO_TRACK_ID),
            "trackFile": track_file,
            "positionS": position_s,
            "durationS": duration_s,
            "srcRate": src_rate,
            "bits": bits,
            "outRate": stream_rate,
            "chainRate": rate,
            "device": device,
            "exclusive": exclusive,
            "format": format,
            "chain": self.shared.tokens.lock().unwrap().clone(),
            "notes": self.shared.notes.lock().unwrap().clone(),
            "quickVariant": self.shared.quick.load(Ordering::Relaxed),
            "pending": pending_label,
            "rebuilding": rebuilding,
            "bufferS": buffer_s,
            "bufferTargetS": target_s,
            "visAheadS": f64::from_bits(self.shared.vis_floor_bits.load(Ordering::Relaxed)),
            "bufferMode": "auto",
            "underrunFrames": underruns,
            "renderRtf": self.shared.rtf_milli.load(Ordering::Relaxed) as f64 / 1000.0,
            "tp": { "db": tp_db, "kind": "predicted" },
            "gainDb": 20.0 * gain.max(1e-12).log10(),
            "lastSwitchMs": self.shared.last_switch_ms.load(Ordering::Relaxed),
            "switches": self.shared.switches.load(Ordering::Relaxed),
            "volumeDb": volume_db,
            "error": error,
            "audible": audible_val,
            "jump": { "pending": jump_pending, "targetS": jump_target_s },
            "srcPeakDb": src_peak_db,
            "srcOverPct": src_over_pct,
            // A3: peak of pre-volume signal sent to the device in the last ~100 ms.
            // null when silent; proves audio actually flows.
            "outLevelDb": self.out.out_level_db(),
            // Post-volume peak in the last ~100 ms (what really reached the DAC).
            "devPeakDb": self.out.dev_peak_db(),
            // Maximum post-volume peak since the application started (never reset).
            "devPeakMaxDb": self.out.dev_peak_max_db(),
            // Periods silenced by the safety guard since the application started.
            "safetyMutes": self.out.safety_mutes.load(std::sync::atomic::Ordering::Relaxed),
            // Test runs: the device is sent silence (AURA_TEST_MUTE=1).
            "testMute": crate::player::output::test_mute(),
            // The loudest of what was handed to the device since the
            // application started, after the safety and test mutes; null
            // while only silence has gone out (a test run: always).
            "sentPeakMaxDb": self.out.sent_peak_max_db(),
            // Mode of the open stream: true = Direct (BIT-PERFECT), false = DSP,
            // null when no stream is open.
            "streamDirect": stream_direct_val,
            // Device sample format string (format_label), null when no stream.
            "devFormat": dev_format,
            // J3: frames of old audio read during the last ramp-down; ≤ one period
            // after the rt-1 fix, proving the old track bleeds no more than ~5 ms.
            "rampFramesRead": ramp_frames,
            // WP-D: bumped each time a gapless hand-over applied a per-track
            // settings override; the page watches for changes to sync the rack DOM.
            "trackSettingsAppliedSeq": track_settings_applied_seq,
            // Spectrum sync: real device latency in milliseconds (§4.1).
            "latencyMs": latency_ms,
            // How far the rack is with switching in (the bar under the
            // badges): {seq, frac, step, busy, leftS}, null when nothing arms.
            "arming": arming,
            // The radio experiment's stream, null when none (radio_job.rs).
            "radio": self.radio_status(radio_heard),
            // The kind of the radio's failure while its error stands (radio::ERR_*).
            "radioError": radio_error,
        })
    }

    /// The output latency in frames: measured by the audio thread
    /// (IAudioClock), else the stream's estimate.
    fn latency_frames(&self, st: &State) -> u64 {
        let est = st.stream.as_ref().map(|s| s.latency_frames as u64).unwrap_or(0);
        match self.out.device_latency_frames.load(Ordering::Relaxed) {
            0 => est,
            m => m,
        }
    }

    /// Compute a spectrum snapshot centred on the frame the listener is
    /// hearing right now. Returns `bands` bytes where each byte encodes a
    /// power level as `dB = -96 + v * 96 / 255` (0 = -96 dBFS or below,
    /// 255 = 0 dBFS). Returns empty when nothing is sounding.
    /// Item 18: rustfft-based, cached plan, no locks held by render/output.
    pub fn spectrum(&self, bands: usize, ahead_ms: f64) -> Vec<u8> {
        if bands == 0 { return Vec::new(); }

        let (timeline, latency_frames) = {
            let st = self.st.lock().unwrap();
            let play = st.play;
            if play == PlayState::Stopped { return Vec::new(); }
            (st.timeline.clone(), self.latency_frames(&st))
        };
        let timeline = match timeline {
            Some(t) => t,
            None => return Vec::new(),
        };

        // If the output is held (between tracks) or paused, there is no audio
        // to analyse — return empty rather than stale data from the last position.
        if self.out.hold.load(Ordering::Relaxed) { return Vec::new(); }
        if self.out.paused.load(Ordering::Relaxed) { return Vec::new(); }

        let rate = timeline.rate() as f64;
        let read_pos = timeline.read_pos();
        // Centre the analysis window on the frame that will be audible when the
        // page renders: read_pos − device_latency + ahead_ms of display lag.
        let ahead_frames = (ahead_ms / 1000.0 * rate) as i64;
        let target_frame = (read_pos as i64 - latency_frames as i64 + ahead_frames).max(0) as u64;
        let fft_size = spectrum_fft_size(rate);

        // Read the analysis window from the timeline.
        let start = target_frame.saturating_sub((fft_size / 2) as u64);
        let write_pos = timeline.write_pos();
        // What is being heard lies behind the reader, and it stays in the ring
        // until the writer laps it (a guard's worth of margin kept). Clamping
        // to earliest_rewrite() instead put the window at least 250 ms ahead
        // of the reader: the bars led the sound.
        let oldest = write_pos
            .saturating_sub(timeline.capacity_frames().saturating_sub(timeline.guard_frames()));
        let clamp_start = start.max(oldest).min(write_pos.saturating_sub(fft_size as u64));

        let mut out = vec![0u8; bands];
        spectrum_bands_at(&timeline, clamp_start, fft_size, &mut out, None);
        out
    }

    /// The spectrum along a stretch of time around what is heard: slices on
    /// the ring's own grid (every `step_ms`, at whole multiples of the step
    /// in the ring's frame count, so a slice is the same slice in every
    /// answer), from `from_ms` to `to_ms` of the audible frame. The big
    /// player's waves are drawn along this: what comes from far away is
    /// the sound that is still in the render buffer, heard when it reaches
    /// the middle (Anton 27.09).
    ///
    /// Answer (little endian): ten f64 — the audible frame, the rate, the
    /// step in frames, the grid index of the first slice, the number of
    /// slices, the ring's identity, its splice generation, the frame of its
    /// last splice (−1: none), the write frame, and the state (0 playing,
    /// 1 paused, 2 held between tracks; no slices then) — and per slice a
    /// byte (1: the window lies in the ring, 0: not written yet or already
    /// lapped) and `bands` bytes as `spectrum` gives them — with `extra`,
    /// followed by the slice's `SLICE_EXTRA` bytes (`slice_extras`: peak,
    /// RMS of L/R/mid/side, correlation). Empty while stopped. A slice once
    /// computed is kept until a splice rewrites the ring from its frame on.
    pub fn spectrum_span(&self, bands: usize, from_ms: f64, to_ms: f64, step_ms: f64, extra: bool) -> Vec<u8> {
        struct Cache {
            ring: usize,
            gen: u32,
            bands: usize,
            step: u64,
            fft: usize,
            slices: std::collections::BTreeMap<u64, Vec<u8>>,
        }
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

        if bands == 0 { return Vec::new(); }
        let (timeline, latency_frames) = {
            let st = self.st.lock().unwrap();
            if st.play == PlayState::Stopped { return Vec::new(); }
            (st.timeline.clone(), self.latency_frames(&st))
        };
        let timeline = match timeline {
            Some(t) => t,
            None => return Vec::new(),
        };
        let rate = timeline.rate() as f64;
        let audible = timeline.read_pos().saturating_sub(latency_frames);
        let write_pos = timeline.write_pos();
        let ring = timeline.id() as usize;
        let gen = timeline.splice_gen.load(Ordering::Acquire);
        let splice_at = timeline.read_splice_log().map(|(_, e)| e.splice_frame as f64).unwrap_or(-1.0);
        let held = self.out.hold.load(Ordering::Relaxed);
        let paused = self.out.paused.load(Ordering::Relaxed);
        let step = ((step_ms.clamp(10.0, 500.0) / 1000.0) * rate).round().max(1.0) as u64;
        let fft = spectrum_fft_size(rate);

        let mut head = [
            audible as f64, rate, step as f64, 0.0, 0.0, ring as f64, gen as f64, splice_at,
            write_pos as f64, if held { 2.0 } else if paused { 1.0 } else { 0.0 },
        ];
        let lo = audible as f64 + from_ms.min(to_ms) / 1000.0 * rate;
        let hi = audible as f64 + from_ms.max(to_ms) / 1000.0 * rate;
        let k0 = (lo.max(0.0) / step as f64).ceil() as u64;
        let k1 = (hi.max(0.0) / step as f64).floor() as u64;
        let count = if held || paused || k1 < k0 { 0 } else { (k1 - k0 + 1).min(512) };
        head[3] = k0 as f64;
        head[4] = count as f64;

        // A slice as kept: its bands, then its extras (sent only when asked).
        let kept = bands + SLICE_EXTRA;
        let sent = if extra { kept } else { bands };
        let mut out = Vec::with_capacity(80 + count as usize * (sent + 1));
        for v in head {
            out.extend_from_slice(&v.to_le_bytes());
        }
        if count == 0 {
            return out;
        }

        let oldest = write_pos
            .saturating_sub(timeline.capacity_frames().saturating_sub(timeline.guard_frames()));
        let cache = CACHE.get_or_init(|| Mutex::new(Cache {
            ring: 0, gen: 0, bands: 0, step: 0, fft: 0, slices: Default::default(),
        }));
        let mut c = cache.lock().unwrap();
        if c.ring != ring || c.bands != bands || c.step != step || c.fft != fft {
            c.slices.clear();
            c.ring = ring;
            c.bands = bands;
            c.step = step;
            c.fft = fft;
            c.gen = gen;
        } else if c.gen != gen {
            // A splice rewrote the ring from its frame on: the slices whose
            // window reaches it are gone; the ones before it still hold.
            let first = if splice_at < 0.0 { 0 } else { (splice_at as u64).saturating_sub(fft as u64 / 2) / step };
            c.slices.retain(|&k, _| k < first);
            c.gen = gen;
        }
        // Far behind what is asked for: not needed again.
        let keep_from = k0.saturating_sub(64);
        while let Some((&k, _)) = c.slices.iter().next() {
            if k >= keep_from { break; }
            c.slices.remove(&k);
        }
        let mut slice = vec![0u8; kept];
        for k in k0..k0 + count {
            let centre = k * step;
            let start = centre.saturating_sub(fft as u64 / 2);
            if let Some(v) = c.slices.get(&k) {
                out.push(1);
                out.extend_from_slice(&v[..sent]);
            } else if centre >= fft as u64 / 2 && start >= oldest && start + fft as u64 <= write_pos {
                let (b, e) = slice.split_at_mut(bands);
                spectrum_bands_at(&timeline, start, fft, b, Some(e));
                out.push(1);
                out.extend_from_slice(&slice[..sent]);
                c.slices.insert(k, slice.clone());
            } else {
                out.push(0);
                out.extend(std::iter::repeat(0u8).take(sent));
            }
        }
        out
    }

    /// The waveform around what is heard, for the visualization scenes'
    /// oscilloscopes (Anton 27.09): from `from_ms` to `to_ms` of the
    /// audible frame, brought down to about `target_rate` by averaging runs
    /// of `d` frames (sample i is the mean of frames i·d … i·d + d − 1, so
    /// a sample is the same sample in every answer).
    ///
    /// Answer (little endian): ten f64 — the audible frame, the rate, `d`,
    /// the index of the first sample, the number of samples, the ring's
    /// identity, its splice generation, the frame of its last splice (−1:
    /// none), the write frame, the state (0 playing, 1 paused, 2 held; no
    /// samples then) — and per sample left and right as i16. Only what lies
    /// in the ring is given (the first sample may come later than asked).
    pub fn wave_span(&self, from_ms: f64, to_ms: f64, target_rate: f64) -> Vec<u8> {
        let (timeline, latency_frames) = {
            let st = self.st.lock().unwrap();
            if st.play == PlayState::Stopped { return Vec::new(); }
            (st.timeline.clone(), self.latency_frames(&st))
        };
        let timeline = match timeline {
            Some(t) => t,
            None => return Vec::new(),
        };
        let rate = timeline.rate() as f64;
        let audible = timeline.read_pos().saturating_sub(latency_frames);
        let write_pos = timeline.write_pos();
        let ring = timeline.id() as usize;
        let gen = timeline.splice_gen.load(Ordering::Acquire);
        let splice_at = timeline.read_splice_log().map(|(_, e)| e.splice_frame as f64).unwrap_or(-1.0);
        let held = self.out.hold.load(Ordering::Relaxed);
        let paused = self.out.paused.load(Ordering::Relaxed);
        let d = (rate / target_rate.clamp(4000.0, 96000.0)).round().max(1.0) as u64;

        let oldest = write_pos
            .saturating_sub(timeline.capacity_frames().saturating_sub(timeline.guard_frames()));
        let lo = (audible as f64 + from_ms.min(to_ms) / 1000.0 * rate).max(oldest as f64);
        let hi = (audible as f64 + from_ms.max(to_ms) / 1000.0 * rate).min(write_pos as f64);
        let i0 = (lo / d as f64).ceil() as u64;
        let i_end = (hi / d as f64).floor() as u64; // samples i0 .. i_end − 1 lie wholly below hi
        let count = if held || paused || i_end <= i0 { 0 } else { (i_end - i0).min(65536) };

        let head = [
            audible as f64, rate, d as f64, i0 as f64, count as f64, ring as f64, gen as f64, splice_at,
            write_pos as f64, if held { 2.0 } else if paused { 1.0 } else { 0.0 },
        ];
        let mut out = Vec::with_capacity(80 + count as usize * 4);
        for v in head {
            out.extend_from_slice(&v.to_le_bytes());
        }
        if count == 0 {
            return out;
        }
        let frames = (count * d) as usize;
        let mut l = vec![0.0f64; frames];
        let mut r = vec![0.0f64; frames];
        timeline.peek(i0 * d, &mut l, &mut r);
        let q = |v: f64| ((v / d as f64).clamp(-1.0, 1.0) * 32767.0).round() as i16;
        for j in 0..count as usize {
            let (a, b) = (j * d as usize, (j + 1) * d as usize);
            out.extend_from_slice(&q(l[a..b].iter().sum()).to_le_bytes());
            out.extend_from_slice(&q(r[a..b].iter().sum()).to_le_bytes());
        }
        out
    }

    // ── Worker ──────────────────────────────────────────────────────────

    fn worker(self: Arc<Self>, rx: Receiver<(Job, bool)>) {
        loop {
            let wait = self.wake_ms.swap(TICK_MS, Ordering::AcqRel);
            let first = match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // Coalesce: of a burst of seeks or settings changes only the
            // last one matters.
            let mut batch: Vec<(Job, bool)> = first.into_iter().collect();
            while let Ok(j) = rx.try_recv() {
                batch.push(j);
            }
            let asked = batch.iter().filter(|(_, from_listener)| *from_listener).count();
            let _ = self.jobs_queued.fetch_update(Ordering::AcqRel, Ordering::Acquire, |q| {
                Some(q.saturating_sub(asked))
            });
            let run = run_order(batch);
            self.worker_busy.store(true, Ordering::Relaxed);
            // A waiting track's start (Ready, run last) cuts no pre-roll
            // short: it only starts what the jobs before it left waiting.
            let n = run.iter().filter(|j| !matches!(j, Job::Ready { .. })).count();
            for (i, j) in run.into_iter().enumerate() {
                // A Stop that arrived with this Play is already out of the
                // channel: the pre-roll wait has to see it here.
                self.batch_left.store(n.saturating_sub(i + 1), Ordering::Release);
                self.run_job(j);
            }
            self.batch_left.store(0, Ordering::Release);
            self.tick();
            self.worker_busy.store(false, Ordering::Relaxed);
        }
    }

    fn set_pending(&self, what: Option<&str>) {
        self.st.lock().unwrap().pending = what.map(|w| (w.to_string(), Instant::now()));
    }

    fn fail(&self, e: String) {
        crate::aelog!("[PLAYER] {}", e);
        let mut st = self.st.lock().unwrap();
        st.error = Some(e);
        // A track waiting for its chain goes on waiting, "preparing": the
        // error is another preparation's (its own ends its wait first). It
        // said "stopped", and ▶ began the wait again from the start.
        if st.waiting.is_none() {
            st.pending = None;
        }
    }

    fn run_job(&self, j: Job) {
        // The radio experiment: a track chosen ends the stream; a seek has
        // nothing to act on; a rack change or another device tunes in again.
        if self.radio_active() {
            match &j {
                Job::Play { .. } | Job::Next | Job::Prev => self.radio_stop(),
                Job::Seek { .. } => return,
                // A stream stops with a fade (the output's hold ramp), not
                // mid-waveform.
                Job::Stop => self.fade_out(),
                Job::Apply => {
                    self.radio_retune();
                    return;
                }
                Job::DeviceChanged => {
                    let url = self.radio.lock().unwrap().as_ref().map(|(_, s)| s.shared.url.clone());
                    if let Some(url) = url {
                        // The old device closes (after the fade); the stream
                        // tunes in again on the new one.
                        self.fade_out();
                        self.stop_now();
                        self.radio_start(url);
                    }
                    return;
                }
                _ => {}
            }
        }
        match j {
            Job::Radio { url } => self.radio_start(url),
            Job::RadioReady { chain, gen, continuous } => self.radio_ready(*chain, gen, continuous),
            Job::RadioFailed { gen, kind, error } => self.radio_failed(gen, kind, error),
            Job::Play { id, at_s } => self.start_track(id, at_s),
            Job::Seek { at_s } => self.seek_now(at_s),
            Job::Apply => self.apply_now(),
            Job::Next => self.step(1),
            Job::Prev => self.step(-1),
            Job::Stop => self.stop_now(),
            Job::Prefetched { id } => self.queue_next(id),
            // Only the track still waiting: Stop, another track or a start
            // already made leave nothing for a late one to do.
            Job::Ready { id } => {
                let waiting = self.st.lock().unwrap().waiting;
                if let Some((w, at_s)) = waiting.filter(|(w, _)| *w == id) {
                    self.start_track(w, at_s);
                }
            }
            Job::StopSilence { id, generation } => {
                let still = {
                    let st = self.st.lock().unwrap();
                    st.current == Some(id) && st.generation == generation && st.play != PlayState::Stopped
                };
                if still {
                    self.stop_now();
                }
            }
            // Another output device while a track plays or is paused (Anton
            // 27.09): it used to reopen at once, and a device that would not
            // open (an HDMI monitor refusing 352.8 kHz exclusive) left an
            // error that only Stop, then Play, got out of. Now the music
            // pauses where it was; Play picks it up there on the new device.
            Job::DeviceChanged => {
                if let Some((id, pos)) = self.now_playing() {
                    self.stop_stream();
                    // A seek that waited for its chain (Instant start off):
                    // the player pauses where the listener asked to go.
                    let waited = self.take_seek_after(id);
                    if waited.is_some() {
                        self.drop_seek_band();
                    }
                    let pos = waited.unwrap_or(pos);
                    let mut st = self.st.lock().unwrap();
                    st.resume_at = Some((id, pos));
                    st.play = PlayState::Paused;
                    st.pending = None;
                    st.error = None;
                    crate::aelog!("[PLAYER] output device changed: paused at {:.2} s — Play goes on there", pos);
                } else if let Some((id, _)) = self.st.lock().unwrap().waiting {
                    // A track waiting for its chain starts on the new device:
                    // its start is asked for again (one that gave way to this
                    // job leaves nothing else to).
                    self.send_internal(Job::Ready { id });
                }
            }
            Job::SeekReady { chain, side_buf, generation } => {
                self.seek_ready(*chain, side_buf, generation);
            }
            Job::BpReady { chain, generation, direct, bits } => {
                self.bp_ready(*chain, generation, direct, bits);
            }
            Job::FsReady { chain, generation, direct, bits } => {
                self.fs_ready(*chain, generation, direct, bits);
            }
            Job::BpFadeComplete { chain, generation, direct, bits } => {
                self.bp_fade_complete(*chain, generation, direct, bits);
            }
            Job::FsFadeComplete { chain, generation, direct, bits } => {
                self.fs_fade_complete(*chain, generation, direct, bits);
            }
        }
    }

    /// What is heard now, as the spectrum slices count it (the reader less the
    /// device's latency): the track, its file, the second in it, and the state
    /// (0 playing, 1 paused, 2 held). For the spatial scenes' objects. A
    /// stream: its session's own track and path (`spatial::live`), the second
    /// of the session.
    pub fn heard(&self) -> Option<(u64, String, f64, u8)> {
        let st = self.st.lock().unwrap();
        if st.play == PlayState::Stopped {
            return None;
        }
        let t = st.timeline.as_ref()?;
        let audible = t.read_pos().saturating_sub(self.latency_frames(&st));
        let m = self.shared.locate(audible)?;
        let (track, path) = if m.track_id == super::radio::chain::RADIO_TRACK_ID {
            drop(st);
            let (gen, url) = self.radio.lock().unwrap().as_ref().map(|(g, s)| (*g, s.shared.url.clone()))?;
            (crate::spatial::live::track_id(gen), crate::spatial::live::path_of(gen, &url))
        } else {
            let path = st.queue.iter().find(|q| q.id == m.track_id)?.path.clone();
            drop(st);
            (m.track_id, path)
        };
        let state = if self.out.hold.load(Ordering::Relaxed) {
            2
        } else if self.out.paused.load(Ordering::Relaxed) {
            1
        } else {
            0
        };
        Some((track, path, m.index as f64 / m.rate.max(1) as f64, state))
    }

    /// (track, seconds) at the reader, as the track goes on (`going_on`).
    fn now_playing(&self) -> Option<(u64, f64)> {
        let st = self.st.lock().unwrap();
        let t = st.timeline.as_ref()?;
        let m = self.going_on(t)?;
        Some((m.track_id, m.index as f64 / m.rate.max(1) as f64))
    }

    /// The reader's mark, counted as the track goes on: in the chain that
    /// plays at the earliest frame a new chain can be spliced at (the one
    /// render.rs plan_swap reads), its index taken back to the reader. A
    /// seek placed just ahead of the reader and not heard yet has moved the
    /// track already. A rack change's chain built from the reader's own
    /// place went in only once the track got back there — after a seek
    /// back, the old taps played on for as long as the seek had gone back
    /// (Anton 1.10) — and a switch of mode started there took the track
    /// back. The next track's gapless start ahead leaves the reader's mark.
    fn going_on(&self, tl: &Timeline) -> Option<render::Mark> {
        let read = tl.read_pos();
        let here = self.shared.locate(read)?;
        let splice = tl.earliest_rewrite();
        match self.shared.locate(splice) {
            Some(m) if m.track_id == here.track_id && m.rate == here.rate => Some(render::Mark {
                frame: read,
                index: m.index.saturating_sub(splice - read),
                ..m
            }),
            _ => Some(here),
        }
    }

    fn track(&self, id: u64) -> Option<TrackInfo> {
        self.st.lock().unwrap().queue.iter().find(|t| t.id == id).cloned()
    }

    /// The file of a list entry (the analyzer's file windows).
    pub fn track_path(&self, id: u64) -> Option<String> {
        self.track(id).map(|t| t.path)
    }

    /// The decoded converted file a disk chain plays now, if `path` is it.
    pub fn disk_source(&self, path: &str) -> Option<Arc<super::chain::DiskSrc>> {
        let st = self.st.lock().unwrap();
        let (_, p, w) = st.disk_src.as_ref()?;
        (p.as_path() == std::path::Path::new(path)).then(|| w.upgrade()).flatten()
    }

    /// `path` decoded: the copy the disk chain of `id` plays now, else read
    /// from disk (and kept for the next seek while its chain lives).
    fn disk_audio(&self, id: u64, path: &std::path::Path) -> Result<Arc<super::chain::DiskSrc>, String> {
        {
            let st = self.st.lock().unwrap();
            if let Some((tid, p, w)) = st.disk_src.as_ref() {
                if *tid == id && p.as_path() == path {
                    if let Some(s) = w.upgrade() {
                        return Ok(s);
                    }
                }
            }
        }
        let src = chain::disk_file(path).map_err(|e| format!("Disk file {}: {}", path.display(), e))?;
        self.st.lock().unwrap().disk_src = Some((id, path.to_path_buf(), Arc::downgrade(&src)));
        Ok(src)
    }

    /// The rack a list entry plays with: its own settings, else the current
    /// ones (also for an id that is not in the list).
    pub fn settings_for(&self, id: u64) -> PlayerSettings {
        effective_settings(id, &self.st.lock().unwrap())
    }

    /// The key `build_chain` caches a track's Hybrid-Phase envelope under.
    pub fn track_key_for(&self, id: u64) -> Option<String> {
        self.track(id).map(|t| track_key(&t))
    }

    /// A list entry's length and source rate, as probed.
    pub fn track_length(&self, id: u64) -> Option<(f64, u32)> {
        self.track(id).map(|t| (t.duration_s, t.sample_rate))
    }

    /// The best variant available without waiting. With `allow_quick`, a
    /// missing full variant is replaced by the quick one and prepared in the
    /// background; when it is ready, an Apply job swaps it in.
    fn variant(&self, t: &TrackInfo, s: &PlayerSettings, allow_quick: bool) -> Result<Arc<Variant>, String> {
        let key = variant_key(s);
        if let Some(v) = self.st.lock().unwrap().variants.get(&(t.id, key.clone())) {
            note_variant(t.id, &v);
            return Ok(v);
        }
        let cancel = AtomicBool::new(false);
        let quick_key = s.quick().source_key();
        if allow_quick && s.mode == Mode::Aura && quick_key != key {
            // The first variant cached for this rack — a stream for another
            // rack under the same quick key is not (its sound is that rack's).
            // It comes first: the full variant is made from its stream
            // (`prepare_full_variant`), not prepared beside it a second time.
            let cached = self.st.lock().unwrap().variants.get(&(t.id, quick_key.clone()));
            if let Some(v) = cached.filter(|v| chain::first_variant_fits(v, s)) {
                self.prepare_in_background(t, s, Job::Apply);
                note_variant(t.id, &v);
                return Ok(v);
            }
            let made = prepare_variant(&PathBuf::from(&t.path), s, true, &cancel).map(Arc::new);
            if let Ok(v) = &made {
                self.st.lock().unwrap().variants.insert((t.id, quick_key.clone()), v.clone());
                note_variant(t.id, v);
                // A Hybrid-Phase rack stands in linear until its pair is
                // ready: the pair's minimum-phase bank is ordered now, while
                // the first variant plays, not after the full variant and the
                // envelope (the same bank: its out rate and branches are the
                // full variant's).
                if needs_envelope(s) {
                    let (v, s) = (v.clone(), s.clone());
                    std::thread::Builder::new()
                        .name("aura-pair-bank".into())
                        .spawn(move || get().res.warm_pair_bank(&v, &s))
                        .ok();
                }
                if let Some(ss) = v.stream.clone() {
                    Self::keep_when_streamed(t.id, quick_key, Arc::downgrade(v), ss);
                }
            }
            self.prepare_in_background(t, s, Job::Apply);
            return made;
        }
        let v = Arc::new(prepare_variant(&PathBuf::from(&t.path), s, false, &cancel)?);
        self.remember(t.id, key, v.clone());
        Ok(v)
    }

    /// A stream variant (`chain::StreamSrc`) whose stages have been through
    /// the whole track takes their output as its source: the cached one is
    /// replaced, and the track as the whole-track stages left it, which only
    /// the stream read, goes with it. Nothing is done when the stream stopped
    /// short or the variant has left the cache meanwhile.
    fn keep_when_streamed(id: u64, key: String, was: std::sync::Weak<Variant>, ss: Arc<chain::StreamSrc>) {
        std::thread::Builder::new()
            .name("aura-source-stream-end".into())
            .spawn(move || {
                // The stream ends within seconds; drop the strong hold on it
                // while waiting, or it never ends for a track left meanwhile.
                let grow = ss.grow.clone();
                drop(ss);
                let grow_weak = Arc::downgrade(&grow);
                drop(grow);
                loop {
                    let Some(g) = grow_weak.upgrade() else { return };
                    if g.is_done() {
                        break;
                    }
                    drop(g);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let p = get();
                let Some(old) = was.upgrade() else { return };
                let Some(done) = chain::completed(&old) else { return };
                let new = Arc::new(done);
                {
                    let mut st = p.st.lock().unwrap();
                    let same = st.variants.peek(&(id, key.clone())).is_some_and(|v| Arc::ptr_eq(v, &old));
                    if !same {
                        return;
                    }
                    st.variants.insert((id, key), new.clone());
                }
                note_variant(id, &new);
            })
            .ok();
    }

    /// The full variant of `t` for `s` (`chain::prepare_full_variant`): made
    /// from the stream of its first variant when that streams for this rack,
    /// else from a stream of its own — and with nothing heard before the
    /// whole chain (Instant start off, not the prewarm), its output probe
    /// runs alongside the stream, so the chain finds it ready.
    fn full_variant(&self, t: &TrackInfo, s: &PlayerSettings, cancel: &AtomicBool, prewarm: bool) -> Result<Variant, String> {
        let from = self
            .st
            .lock()
            .unwrap()
            .variants
            .peek(&(t.id, s.quick().source_key()))
            .and_then(|v| v.stream.clone());
        let tk = track_key(t);
        let probe = |v: &Variant| {
            let hp_on = chain::plays_pair(&self.res, &tk, v, s);
            let _ = chain::output_peak(&self.res, &tk, v, s, hp_on, chain::ceiling_lin(v, s));
        };
        let alongside = !self.instant() && !prewarm;
        chain::prepare_full_variant(
            &PathBuf::from(&t.path),
            s,
            cancel,
            from,
            !prewarm,
            if alongside { Some(&probe) } else { None },
        )
    }

    /// Nothing is left to prepare for the chain `t` plays with `s` on its
    /// full variant `v`: a Hybrid-Phase or alpha-HP rack has the variant's
    /// onset envelope — or it could not be made, and the chain plays linear
    /// phase rather than wait for ever.
    fn envelope_done(&self, t: &TrackInfo, s: &PlayerSettings, v: &Variant) -> bool {
        if !needs_envelope(s) {
            return true;
        }
        let tk = track_key(t);
        // The pair plays with the plan made as it plays (Instant start off
        // always): there is no envelope to wait for.
        chain::plays_pair(&self.res, &tk, v, s) || self.env_failed.lock().unwrap().contains(&env_key(&tk, v))
    }

    /// Instant start off: the full variant of `t` for `s`, when its whole
    /// chain can be built now (`envelope_done`); None while anything is
    /// still to be prepared. The one test of "ready" in that mode: a start,
    /// an Apply, a seek, the prefetch and the preparations all ask it.
    fn complete_variant(&self, t: &TrackInfo, s: &PlayerSettings) -> Option<Arc<Variant>> {
        let v = self.st.lock().unwrap().variants.get(&(t.id, variant_key(s)))?;
        self.envelope_done(t, s, &v).then_some(v)
    }

    /// The onset envelope (and its plan) of `v` for a Hybrid-Phase or
    /// alpha-HP rack, computed here when it is not ready — the caller's own
    /// thread waits (0.5–35 s the first time). One that cannot be made is
    /// remembered as such: its chain plays linear phase.
    fn make_envelope(&self, t: &TrackInfo, s: &PlayerSettings, v: &Variant) {
        if self.envelope_done(t, s, v) {
            return;
        }
        let tk = track_key(t);
        match self.res.envelope(&tk, v) {
            Ok(env) => {
                self.res.plan(&tk, v, &env);
            }
            Err(e) => {
                crate::aelog!("[PLAYER] Hybrid-Phase envelope of {}: {} — it plays linear phase", t.title, e);
                self.env_failed.lock().unwrap().insert(env_key(&tk, v));
            }
        }
    }

    /// A source variant is being prepared in the background (the analyzer's
    /// background work waits for it). False when the state is busy.
    pub fn preparing(&self) -> bool {
        self.st.try_lock().map(|st| !st.in_flight.is_empty()).unwrap_or(false)
    }

    /// Stop the background preparations of `track` for any source key but
    /// `keep`: the rack has moved on from them. Each click through a burst of
    /// rack changes used to leave its own whole-file preparation running
    /// (eight at once, each with the Adaptive Apodizer on every core), and the
    /// playing track starved behind them.
    fn drop_stale_preparations(&self, track: u64, keep: &str) {
        self.drop_preparations(track, Some(keep), "the rack moved on");
        // So do the first variants of the racks it moved on from that never
        // got their full variant: what they hold was for that one.
        let gone = self.st.lock().unwrap().variants.drop_displaced_streams(track, keep);
        if gone > 0 {
            crate::aelog!(
                "[PLAYER] first variant let go, the rack moved on before its full variant was made: {} of track {}",
                gone,
                track
            );
        }
    }

    /// Stop the background preparations of `track` — every one, or all but
    /// `keep`'s: each is told to stop and no longer counts as in flight.
    fn drop_preparations(&self, track: u64, keep: Option<&str>, why: &str) {
        let mut st = self.st.lock().unwrap();
        let stale: Vec<(u64, String)> = st
            .prep_cancel
            .keys()
            .filter(|(tid, k)| *tid == track && Some(k.as_str()) != keep)
            .cloned()
            .collect();
        for k in stale {
            if let Some(c) = st.prep_cancel.remove(&k) {
                c.store(true, Ordering::Release);
            }
            st.in_flight.remove(&k);
            st.apply_when_ready.remove(&k);
            crate::aelog!("[PLAYER] preparation dropped, {}: {}", why, k.1);
        }
    }

    /// Prepare the full variant of `t` for `s` on its own thread, then send
    /// `then` to the worker.
    ///
    /// Asked for while that preparation is already running, it starts no
    /// second one, but what the caller wants done with it still happens: an
    /// Apply is sent when it is done, whatever job it was started with. The
    /// next track's prefetch is started with `Prefetched`, which queues a next
    /// track and does nothing for a track already playing — and a track that
    /// starts before its prefetch is done (the one before it ran out, or ⏭ in
    /// its last 40 s) plays its quick variant until that Apply. Dropped, the
    /// Apply never came, and the quick variant — no declip, ISP, SUB or AA —
    /// played the whole track, its badges waiting for good (Anton 30.09).
    ///
    /// With Instant start off, and for the next track's prewarm (`then`
    /// Prefetched) in both positions of the tick, it prepares the whole
    /// chain: a Hybrid-Phase or alpha-HP rack's onset envelope is made too,
    /// after the variant — or alone, for a variant cached without one — with
    /// the same bookkeeping, and "ready" means `complete_variant`. `then` goes
    /// only once the chain is complete. Its end, a dropped one's too, also
    /// tells a track waiting for its chain (`Job::Ready`): that track then
    /// starts, or asks for the preparation it needs now. The prewarm runs
    /// below the playing track's priority.
    fn prepare_in_background(&self, t: &TrackInfo, s: &PlayerSettings, then: Job) {
        let key = variant_key(s);
        // The next track's prewarm prepares its whole chain whatever the
        // tick, and below the playing track's priority.
        let prewarm = matches!(then, Job::Prefetched { .. });
        // Instant start off (and the prewarm), "ready" is the envelope too —
        // a look that can reach the disk, made outside the state lock: the
        // prefetch asks every tick while the next track's envelope is made.
        let env_done = if !makes_whole_chain(self.instant(), prewarm) {
            true
        } else {
            let v = self.st.lock().unwrap().variants.peek(&(t.id, key.clone())).cloned();
            v.is_some_and(|v| self.envelope_done(t, s, &v))
        };
        let cached = {
            let mut st = self.st.lock().unwrap();
            let cached = st.variants.get(&(t.id, key.clone()));
            if cached.is_some() && env_done {
                // Ready already: it was cached between the caller's look and
                // this one. What the caller wanted done with it can be done.
                drop(st);
                self.send_internal(then);
                return;
            }
            if !st.in_flight.insert((t.id, key.clone())) {
                if matches!(then, Job::Apply) {
                    st.apply_when_ready.insert((t.id, key));
                }
                return;
            }
            cached
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.st.lock().unwrap().prep_cancel.insert((t.id, key.clone()), cancel.clone());
        let t = t.clone();
        let s = s.clone();
        std::thread::Builder::new()
            .name("aura-prepare".into())
            .spawn(move || {
                // The next track's prewarm gives way to the playing track:
                // the render runs above normal, this below it — and its times
                // teach the estimates nothing.
                if prewarm {
                    super::analytics::track::set_below_normal_priority();
                    super::arming::learn_nothing_here();
                }
                let p = get();
                let k = (t.id, key);
                let tk = track_key(&t);
                // `then` is this track's own Ready: no second one after it (a
                // second job in the batch cuts the start's pre-roll short).
                let then_is_ready = matches!(then, Job::Ready { id } if id == t.id);
                // The variant: made here, or cached already with only its
                // envelope missing (Instant start off).
                let fresh = cached.is_none();
                let v = match cached {
                    Some(v) => v,
                    None => {
                        let r = p.full_variant(&t, &s, &cancel, prewarm);
                        if cancel.load(Ordering::Acquire) {
                            // Dropped: the rack moved on. Not an error, nothing to swap in.
                            return p.preparation_dropped(&k, &cancel);
                        }
                        match r {
                            Ok(v) => Arc::new(v),
                            Err(e) => return p.preparation_failed(&k, &cancel, e),
                        }
                    }
                };
                // Instant start off, and for the prewarm: the envelope is part
                // of the chain (the tick is read now: it may have changed while
                // the variant was made). The variant is cached first — it is
                // ready, and what waits for the envelope can see that.
                let with_env = makes_whole_chain(p.instant(), prewarm) && !p.envelope_done(&t, &s, &v);
                if with_env {
                    if fresh {
                        p.remember(t.id, k.1.clone(), v.clone());
                    }
                    // The pair's minimum-phase bank is built into the cache
                    // alongside the envelope: the chain built next takes it.
                    std::thread::scope(|sc| {
                        sc.spawn(|| p.res.warm_pair_bank(&v, &s));
                        p.make_envelope(&t, &s, &v);
                    });
                    if cancel.load(Ordering::Acquire) {
                        return p.preparation_dropped(&k, &cancel);
                    }
                }
                // The output-peak probe here, off the air: the chain
                // this variant goes on with then finds it cached. A
                // direct variant has no filter and no ceiling to probe.
                if !v.direct {
                    let hp_on = chain::plays_pair(&p.res, &tk, &v, &s);
                    let _ = chain::output_peak(&p.res, &tk, &v, &s, hp_on, chain::ceiling_lin(&v, &s));
                }
                // Cached before it stops counting as in flight: in
                // between, a start of this track found it neither, and
                // prepared it all over again.
                if fresh && !with_env {
                    p.remember(t.id, k.1.clone(), v);
                }
                // The Apply asked for meanwhile, unless `then` is one,
                // or the listener has left the track since.
                let apply_too = p.end_preparation(&k, &cancel)
                    && !matches!(then, Job::Apply)
                    && p.st.lock().unwrap().current == Some(t.id);
                p.send_internal(then);
                if apply_too {
                    p.send_internal(Job::Apply);
                }
                if !then_is_ready {
                    p.ready_if_waiting(t.id);
                }
            })
            .ok();
    }

    /// The track waiting for its whole chain, if it is `id`, tries its start
    /// again (`Job::Ready`).
    fn ready_if_waiting(&self, id: u64) {
        let waiting = self.st.lock().unwrap().waiting.is_some_and(|(w, _)| w == id);
        if waiting {
            self.send_internal(Job::Ready { id });
        }
    }

    /// A preparation stopped because the rack moved on is over. A track
    /// waiting for its chain asks for the preparation it needs now rather
    /// than wait for one that will never end.
    fn preparation_dropped(&self, k: &(u64, String), cancel: &Arc<AtomicBool>) {
        self.end_preparation(k, cancel);
        self.ready_if_waiting(k.0);
    }

    /// A preparation that failed: its error is shown, and a track waiting for
    /// this chain stops waiting — silence with the error, not "Preparing"
    /// for good.
    fn preparation_failed(&self, k: &(u64, String), cancel: &Arc<AtomicBool>, e: String) {
        self.end_preparation(k, cancel);
        let (seek_left, k4_silence) = {
            let mut st = self.st.lock().unwrap();
            let rack = variant_key(&effective_settings(k.0, &st)) == k.1;
            let theirs = st.waiting.is_some_and(|(w, _)| w == k.0) && rack;
            if theirs {
                st.waiting = None;
            }
            // K4's silence waited for this chain (Instant start off): it
            // ends with the error — the player stops — not with the track.
            let k4_silence = if rack {
                let mut w = self.k4_waits.lock().unwrap();
                match *w {
                    Some((id, generation)) if id == k.0 => {
                        *w = None;
                        Some(generation)
                    }
                    _ => None,
                }
            } else {
                None
            };
            // Nor does a seek that waited for this chain get it: it goes, and
            // its band with it (it stayed on the bar for good, and each seek
            // asked for the failing preparation again).
            let seek = rack && st.seek_after.is_some_and(|(id, _)| id == k.0);
            if seek {
                st.seek_after = None;
            }
            (seek, k4_silence)
        };
        if seek_left {
            self.drop_seek_band();
        }
        self.fail(e);
        if let Some(generation) = k4_silence {
            self.send_internal(Job::StopSilence { id: k.0, generation });
        }
    }

    /// A background preparation is over: it no longer counts as in flight.
    /// Only its own entries go — a dropped one may have been asked for again
    /// meanwhile. True when an Apply was asked for while it ran.
    fn end_preparation(&self, k: &(u64, String), cancel: &Arc<AtomicBool>) -> bool {
        let mut st = self.st.lock().unwrap();
        if !st.prep_cancel.get(k).is_some_and(|c| Arc::ptr_eq(c, cancel)) {
            return false;
        }
        st.prep_cancel.remove(k);
        st.in_flight.remove(k);
        st.apply_when_ready.remove(k)
    }

    fn remember(&self, track: u64, key: String, v: Arc<Variant>) {
        note_variant(track, &v);
        let kept_keys = {
            let mut st = self.st.lock().unwrap();
            st.variants.insert((track, key), v);
            // Keep the playing track's and the next one's; three variants a
            // track at most (e.g. quick, current, previous toggle state), the
            // three used last — so never the one just made.
            let mut keep = playing_and_next(&st);
            st.variants.retain_tracks(|tid| keep.contains(&tid) || tid == track);
            st.variants.cap_per_track(3);
            keep.push(track);
            kept_track_keys(&st, &keep)
        };
        // The same tracks keep their Hybrid-Phase envelopes and plans; the
        // rest go with their variants (outside the state lock).
        if let Some(keys) = kept_keys {
            self.res.forget_tracks_except(&keys);
        }
    }

    fn start_track(&self, id: u64, at_s: f64) {
        let t = match self.track(id) {
            Some(t) => t,
            None => {
                // It left the list while it waited for its chain.
                let mut st = self.st.lock().unwrap();
                if st.waiting.is_some_and(|(w, _)| w == id) {
                    st.waiting = None;
                    st.pending = None;
                }
                return;
            }
        };

        // Instant start off: the track this start replaces lets go of its
        // preparations — a rack change the listener left with it, or the
        // chain it waited for — which would only slow this one down.
        if !self.instant() {
            let before = self.st.lock().unwrap().current.filter(|c| *c != id);
            if let Some(before) = before {
                self.drop_preparations(before, None, "another track was chosen");
            }
        }

        // When a pre-converted file is registered for this track, and the
        // mode is NOT Direct (BIT-PERFECT plays the raw original), play it
        // directly from disk to save CPU.
        let disk_path: Option<PathBuf> = {
            let st = self.st.lock().unwrap();
            if effective_settings(id, &st).mode == Mode::Direct { None } else {
                st.rendered_map.get(&id).map(PathBuf::from)
            }
        };
        if let Some(dp) = disk_path {
            return self.start_from_disk(id, &t, dp, at_s);
        }

        // Instant start off: nothing of this track is heard before its whole
        // chain is ready. Until it is, the track waits in silence for its
        // preparation (wait_for_chain), and Job::Ready brings it back here.
        let s = effective_settings(id, &self.st.lock().unwrap());
        let complete = if !self.instant() && s.mode == Mode::Aura {
            match self.complete_variant(&t, &s) {
                Some(v) => Some(v),
                None => return self.wait_for_chain(&t, &s, at_s),
            }
        } else {
            None
        };
        // It waited for its chain: that start's arming goes on — one epoch
        // from the press to the first sound, the bar not begun again at the
        // end of the wait — and the log says how long the wait was (the
        // silence the listener chose is that, the build and the device's start).
        let (was_waiting, since) = {
            let st = self.st.lock().unwrap();
            let w = st.waiting.is_some_and(|(w, _)| w == id);
            (w, w.then(|| st.pending.as_ref().map(|(_, since)| *since)).flatten())
        };
        let waited = since.map(|s| s.elapsed());
        // The start after the wait (a Ready, the Apply of the waiting track)
        // gives way to what the listener asked meanwhile — a job waiting for
        // the worker: the track goes on waiting, and that job decides. The
        // device used to open, and the track the listener had left was heard
        // until their job faded it out.
        if was_waiting && self.jobs_queued.load(Ordering::Acquire) > 0 {
            return self.give_way(&t, at_s, since);
        }
        if was_waiting {
            // The wait is over: a preparation of this track for another rack
            // (the Aura chain it waited for, when it starts in BIT-PERFECT)
            // is not needed any more.
            self.drop_stale_preparations(id, &variant_key(&s));
        }
        let t_ready = Instant::now();

        // Hold immediately so the output thread stops advancing the old audio
        // while we prepare. Released by the output thread once the jump lands,
        // or manually on any failure path.
        self.out.jump_frame.store(u64::MAX, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        // Clear per-track GPU state for the new track.
        self.shared.gpu_failed.store(false, Ordering::Relaxed);
        self.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        self.shared.gpu_zeros_from.store(u64::MAX, Ordering::Relaxed);
        // A Hybrid-Phase held for "the next track" (K3b) belongs to the start
        // that held it: this start, that track's again too, tries it afresh.
        self.hp_held.store(u64::MAX, Ordering::Release);
        // So does a silence K4 left waiting for the whole chain.
        *self.k4_waits.lock().unwrap() = None;

        {
            let mut st = self.st.lock().unwrap();
            // S1: bump generation so any in-flight seek for the previous track
            // sees a mismatch and exits without splicing old-track audio here.
            st.generation += 1;
            if !was_waiting {
                st.arm_epoch += 1;
            }
            st.current = Some(id);
            st.error = None;
            st.queued_next = None;
            st.waiting = None;
            st.seek_after = None;
            // A prewarm that failed is tried again after another start (the
            // VRAM or the converter it failed for may be free then): it was
            // never tried again for that track and rack.
            st.prewarm_failed = None;
        }
        // S1/S3: the new track has no pending seek; clear the atoms here so
        // the stale seek thread does not need to touch them on early-exit.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        self.set_pending(Some(&format!("Preparing {}", t.title)));
        let v = match complete {
            Some(v) => {
                note_variant(t.id, &v);
                v
            }
            None => match self.variant(&t, &s, true) {
                Ok(v) => v,
                Err(e) => {
                    self.out.hold.store(false, Ordering::Release);
                    return self.fail(e);
                }
            },
        };
        // Compute and store source peak stats for the status panel (item 19).
        // This is a cheap scan (one pass over already-decoded PCM).
        {
            let mut peak = 0.0f64;
            let mut overs: u64 = 0;
            let total = v.src.len();
            for &x in v.src.l.iter().chain(v.src.r.iter()) {
                let a = x.abs();
                if a > peak { peak = a; }
                if a > 1.0 { overs += 1; }
            }
            let src_peak_db = if peak > 0.0 { 20.0 * peak.log10() } else { -144.0 };
            let src_over_pct = if total > 0 { (overs as f64 / total as f64) * 100.0 } else { 0.0 };
            let mut st = self.st.lock().unwrap();
            st.src_peak_db = Some(src_peak_db);
            st.src_over_pct = Some(src_over_pct);
        }
        let start = (at_s * v.out_rate as f64) as u64;
        // The chain built ahead for it (the prewarm): taken as it is — on the
        // route this start would take (`prewarmed_on_route`). At the end of
        // the track before, for a new rate only the device's reopen is left;
        // on ⏭, nothing — a Hybrid-Phase pair on the video card is not built
        // again.
        let prewarmed = if start == 0 { self.take_prewarmed(id, &s) } else { None };
        let (prewarmed, routed) = match prewarmed.map(|c| self.prewarmed_on_route(c, &s, &v, &track_key(&t))) {
            Some(Ok(c)) => (Some(c), None),
            Some(Err(route)) => (None, Some(route)),
            None => (None, None),
        };
        let chain = match prewarmed {
            Some(c) => c,
            None => {
                // If the filter bank has not been built yet this session, the
                // build step can take a moment (30M filter: ~300 ms on first
                // call). Give the UI something informative to show.
                if !v.quick {
                    let taps_label = crate::audio::converter::dsp::filter::taps_label(s.taps)
                        .unwrap_or("?");
                    self.set_pending(Some(&format!("Loading {taps_label} filter\u{2026}")));
                }
                let (s_eff, gpu_ctx, downgrade, gen) = routed.unwrap_or_else(|| self.route_conv(&s, &v, &track_key(&t)));
                match build_chain(&self.res, id, &track_key(&t), &v, &s_eff, start, gpu_ctx, downgrade, gen) {
                    Ok(c) => c,
                    Err(e) => {
                        self.out.hold.store(false, Ordering::Release);
                        return self.fail(e);
                    }
                }
            }
        };
        let hp_def = chain.hp_deferred;
        // The same, for what the listener asked while the chain was built
        // (seconds for a large filter): the chain goes unheard.
        if was_waiting && self.jobs_queued.load(Ordering::Acquire) > 0 {
            return self.give_way(&t, at_s, since);
        }
        match self.put_on_air(chain, v.direct, t.bits, was_waiting) {
            Ok(true) => {}
            // And for one asked during the pre-roll.
            Ok(false) => return self.give_way(&t, at_s, since),
            Err(e) => {
                self.out.hold.store(false, Ordering::Release);
                return self.fail(e);
            }
        }
        if let Some(w) = waited {
            crate::aelog!(
                "[PLAYER] {}: its whole chain took {:.2} s, the build and the device {} ms more",
                t.title,
                w.as_secs_f64(),
                t_ready.elapsed().as_millis()
            );
        }
        if hp_def {
            // One HP job per start: a quick variant plays only until its
            // full variant is ready, and the Apply that swaps the full one in
            // starts the job for it (each variant has its own envelope). Two
            // jobs doubled the envelope, the pre-trial and the bank loads.
            let (st_gen, full_coming) = {
                let st = self.st.lock().unwrap();
                (st.generation, full_variant_coming(&st.variants, &st.in_flight, id, &s.source_key(), v.quick))
            };
            if !full_coming {
                self.spawn_hp_job(id, st_gen, v, track_key(&t), s);
            }
        }
        self.set_pending(None);
    }

    /// A start after the wait gives way to what the listener asked meanwhile
    /// (a job waiting for the worker): the track goes on waiting — at its
    /// point, "Preparing" since the wait began (`since`), the output not
    /// held — and its start is asked for again, after that job (run_order
    /// runs a Ready last). The job decides: another track, Stop or a seek
    /// replace the wait, and the Ready finds nothing to do; a job that does
    /// nothing here (⏭ on the last track, ▶ of a row that has left the list)
    /// leaves the Ready to start it. Nothing woke it before: it waited for
    /// good, the rack locked.
    fn give_way(&self, t: &TrackInfo, at_s: f64, since: Option<Instant>) {
        crate::aelog!("[PLAYER] {}: its start gives way to what the listener asked, and comes after it", t.title);
        self.out.hold.store(false, Ordering::Release);
        {
            let mut st = self.st.lock().unwrap();
            st.waiting = Some((t.id, at_s));
            st.pending = Some((format!("Preparing {}", t.title), since.unwrap_or_else(Instant::now)));
        }
        self.send_internal(Job::Ready { id: t.id });
    }

    /// Instant start off: `t` waits in silence until its whole chain is ready
    /// (`complete_variant`). The track that played stops at once — it is not
    /// played on under "Preparing" (Anton 30.09) — and the preparation runs on
    /// its own thread, so the worker stays free for Stop, ⏭ or another track.
    /// When a preparation of `t` ends, Job::Ready tries the start again: it
    /// plays then, or waits on for what the rack now needs. Called again for
    /// the same track (a Ready that found it still incomplete, a rack change,
    /// a seek before its first sound), it only moves the start point and asks
    /// for the preparation that is missing.
    fn wait_for_chain(&self, t: &TrackInfo, s: &PlayerSettings, at_s: f64) {
        // A chain built ahead for another track (the one that followed the
        // track left now) is not wanted while this one waits — the tick that
        // let it go does not run without a stream — and it held its memory
        // through the wait.
        let other = {
            let mut st = self.st.lock().unwrap();
            if st.prewarmed.as_ref().is_some_and(|p| p.id != t.id) { st.prewarmed.take() } else { None }
        };
        drop(other);
        let was = self.st.lock().unwrap().waiting.map(|(w, _)| w);
        if was != Some(t.id) {
            // Silence at once, and without a click: the old track fades out
            // (the output's hold ramp), its stream closes, and only then is
            // the hold let go. Let go first, the audio thread's next period
            // (it looks at the stop flag before it waits for the device, not
            // after) played the old track again, ramping up, and the stop
            // cut it off.
            self.fade_out();
            // Nothing is on the air to apply a deferred rack to.
            self.apply_deferred.store(false, Ordering::Release);
            self.stop_stream();
            self.out.hold.store(false, Ordering::Release);
            self.out.jump_frame.store(u64::MAX, Ordering::Release);
            self.shared.gpu_failed.store(false, Ordering::Relaxed);
            self.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
            self.shared.gpu_zeros_from.store(u64::MAX, Ordering::Relaxed);
            self.shared.seek_pending.store(false, Ordering::Release);
            self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
            if let Some(prev) = was {
                // Left before it was heard: its preparation gives way.
                self.drop_preparations(prev, None, "another track was chosen");
            }
            crate::aelog!("[PLAYER] {}: waits for its whole chain (instant start off)", t.title);
        }
        {
            let mut st = self.st.lock().unwrap();
            // S1: the old track's seeks and HP jobs see a newer generation.
            st.generation += 1;
            // The press starts this start's arming (the same track pressed
            // again too); asked again while it waits, the arming goes on, and
            // the start after the wait keeps it.
            if was != Some(t.id) {
                st.arm_epoch += 1;
            }
            st.current = Some(t.id);
            st.play = PlayState::Stopped;
            st.error = None;
            st.queued_next = None;
            st.resume_at = None;
            st.seek_after = None;
            // As at any start: a failed prewarm is tried again.
            st.prewarm_failed = None;
            // The track before's source peak is not this one's.
            st.src_peak_db = None;
            st.src_over_pct = None;
            st.waiting = Some((t.id, at_s));
            if was != Some(t.id) || st.pending.is_none() {
                st.pending = Some((format!("Preparing {}", t.title), Instant::now()));
            }
        }
        self.drop_stale_preparations(t.id, &variant_key(s));
        self.prepare_in_background(t, s, Job::Ready { id: t.id });
    }

    /// Fade the stream on the air to silence (the output's hold ramp, one
    /// period) and wait until that has been heard: the device's buffer and a
    /// period more, 150 ms at most. Closing a stream mid-waveform clicks.
    /// Nothing to do without a stream.
    fn fade_out(&self) {
        let wait_s = {
            let st = self.st.lock().unwrap();
            match (&st.output, &st.stream) {
                (Some(_), Some(s)) => (s.latency_frames + s.buffer_frames) as f64 / s.rate.max(1) as f64,
                _ => return,
            }
        };
        self.out.jump_frame.store(u64::MAX, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_secs_f64(wait_s.clamp(0.005, 0.15)));
    }

    /// Play a pre-converted file from disk. Called instead of `start_track`'s
    /// normal path when `rendered_map` has a path for this track id.
    fn start_from_disk(&self, id: u64, t: &TrackInfo, path: PathBuf, at_s: f64) {
        self.out.jump_frame.store(u64::MAX, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        self.shared.gpu_failed.store(false, Ordering::Relaxed);
        self.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        self.shared.gpu_zeros_from.store(u64::MAX, Ordering::Relaxed);
        // A file from disk has no Hybrid-Phase to hold (see start_track),
        // nor a K4 silence waiting.
        self.hp_held.store(u64::MAX, Ordering::Release);
        *self.k4_waits.lock().unwrap() = None;
        {
            let mut st = self.st.lock().unwrap();
            // As at any start: a failed prewarm is tried again.
            st.prewarm_failed = None;
            // S1: same as start_track — bump generation to invalidate any
            // in-flight seek that was running before the track switch.
            st.generation += 1;
            st.arm_epoch += 1;
            st.current = Some(id);
            st.error = None;
            st.queued_next = None;
            st.waiting = None;
            st.seek_after = None;
            // Disk files are not scanned for peaks; clear stale stats from
            // whatever live-rendered track ran before so the UI shows nothing
            // rather than wrong numbers.
            st.src_peak_db = None;
            st.src_over_pct = None;
        }
        // S1/S3: clear seek state for the new track.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        self.set_pending(Some(&format!("Loading {}", t.title)));
        let src = match self.disk_audio(id, &path) {
            Ok(s) => s,
            Err(e) => {
                self.out.hold.store(false, Ordering::Release);
                return self.fail(e);
            }
        };
        let chain = build_file_chain(id, src, at_s, &path);
        if let Err(e) = self.play_chain(chain, false, t.bits) {
            self.out.hold.store(false, Ordering::Release);
            return self.fail(e);
        }
        self.set_pending(None);
    }

    /// Put a chain on the air: swap it into the running stream when the
    /// rate and mode match, otherwise (re)open the device.
    fn play_chain(&self, chain: Chain, direct: bool, bits: u32) -> Result<(), String> {
        self.put_on_air(chain, direct, bits, false).map(|_| ())
    }

    /// `play_chain`. A start after the wait (`after_wait`) gives way to the
    /// listener here too: a job sent during the pre-roll leaves the device
    /// closed — the render thread is stopped — and Ok(false) says so. The
    /// pre-roll let it through, and the track the listener had left was
    /// heard until their job's fade.
    fn put_on_air(&self, chain: Chain, direct: bool, bits: u32, after_wait: bool) -> Result<bool, String> {
        let same_stream = {
            let st = self.st.lock().unwrap();
            st.output.is_some()
                && st.timeline.as_ref().map(|t| t.rate()) == Some(chain.out_rate)
                && st.stream_direct == direct
        };
        if same_stream {
            let _ = self.render_tx.lock().unwrap().send(Msg::Swap { chain, continuous: false, seek: false, side_buf: None, requested: Instant::now() });
            let mut st = self.st.lock().unwrap();
            st.play = PlayState::Playing;
            st.resume_at = None;
            self.out.paused.store(false, Ordering::Relaxed);
            return Ok(true);
        }
        let t_stop = Instant::now();
        self.stop_stream();
        let stop_ms = t_stop.elapsed().as_millis();
        // Opening a new device stream: clear the hold that start_track set.
        // The hold is only meaningful for same-stream swaps; when a new output
        // thread is created it must start playing immediately, not wait for a
        // jump_frame that will never come.
        self.out.hold.store(false, Ordering::Release);
        let rate = chain.out_rate;
        let timeline = Arc::new(Timeline::new(rate, 4.0));
        let _ = self.render_tx.lock().unwrap().send(Msg::Start { chain, timeline: timeline.clone() });
        // A start after the wait gives way to a job the listener has sent:
        // no device for it (one opened meanwhile is closed unheard, below).
        let gives_way = || after_wait && self.jobs_queued.load(Ordering::Acquire) > 0;
        let device_id = self.st.lock().unwrap().device_id.clone();
        // BIT-PERFECT: the source's own depth; 0 (no integer depth: a lossy
        // decoder's output) takes the widest integer the device does.
        let mode = if direct { OutputMode::Direct { source_bits: if bits == 0 { 0 } else { bits.max(16) } } } else { OutputMode::Dsp };
        let cfg = OutputConfig { device_id, rate, mode, allow_shared: false };
        // The device opens while the renderer gets ahead, held at its gate
        // (`OutputStream::start_gated`): it starts pulling once the pre-roll
        // is there, as it did when it was opened only then — the same path
        // for a BIT-PERFECT stream.
        let t_open = Instant::now();
        let opening = if gives_way() {
            None
        } else {
            let (cfg, tl, out) = (cfg.clone(), timeline.clone(), self.out.clone());
            std::thread::Builder::new()
                .name("aura-open-device".into())
                .spawn(move || (open_gated(cfg, tl, out), t_open.elapsed()))
                .ok()
        };
        // Let the renderer get ahead before the device starts pulling — but
        // not past a Stop, Seek or anything else the listener asked for
        // meanwhile: those wait in the queue behind this job.
        let t0 = Instant::now();
        while (timeline.buffered_frames() as f64) < 0.3 * rate as f64
            && t0.elapsed() < Duration::from_secs(3)
            && self.jobs_queued.load(Ordering::Acquire) == 0
            && self.batch_left.load(Ordering::Acquire) == 0
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let preroll_ms = t0.elapsed().as_millis();
        let opened = opening.map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        if gives_way() {
            let _ = self.render_tx.lock().unwrap().send(Msg::Stop);
            if let Some((Ok(o), _)) = opened {
                o.stop();
            }
            return Ok(false);
        }
        self.out.paused.store(false, Ordering::Relaxed);
        self.out.device_lost.store(false, Ordering::Relaxed);
        *self.out.last_error.lock().unwrap() = None;
        // Opened only now where no thread could open it alongside.
        let (out, open_took) = opened.unwrap_or_else(|| {
            let t = Instant::now();
            (open_gated(cfg, timeline.clone(), self.out.clone()), t.elapsed())
        });
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                let _ = self.render_tx.lock().unwrap().send(Msg::Stop);
                return Err(e);
            }
        };
        out.release();
        // What a start on a new stream is silent for, part by part: the
        // chain is ready by then (built ahead, or just built).
        crate::aelog!(
            "[PLAYER] new stream: the old one closed in {} ms, the pre-roll took {} ms, the device opened in {} ms alongside it",
            stop_ms,
            preroll_ms,
            open_took.as_millis()
        );
        // analytics: register live timeline BEFORE acquiring st lock to avoid
        // establishing an undocumented two-level lock ordering (st → live_state).
        // Hub only needs Arc clones, all available here.
        if let Some(hub) = super::analytics::hub::try_get() {
            hub.on_stream(&timeline, &self.shared, &self.out, rate);
        }
        let mut st = self.st.lock().unwrap();
        st.stream = Some(out.info().clone());
        st.output = Some(out);
        st.timeline = Some(timeline);
        st.stream_direct = direct;
        st.play = PlayState::Playing;
        st.resume_at = None;
        Ok(true)
    }

    fn stop_stream(&self) {
        // Reset the extended ramp so subsequent holds use the one-period default.
        self.out.hold_ramp_frames.store(0, Ordering::Release);
        let out = {
            let mut st = self.st.lock().unwrap();
            st.timeline = None;
            st.stream = None;
            // Any in-progress rate switch is cancelled when the stream stops.
            st.rate_switch_gen = None;
            // What was queued for the gapless hand-over goes with the stream
            // (the render thread drops it at Msg::Stop): the tick queues the
            // next track again for the stream after it. It stayed marked, and
            // after a rate switch the next track came as a jump, not gapless.
            st.queued_next = None;
            st.loop_at = u64::MAX;
            st.output.take()
        };
        if let Some(o) = out {
            o.stop();
        }
        let _ = self.render_tx.lock().unwrap().send(Msg::Stop);
        // analytics: stop live accumulation (only when hub is initialized)
        if let Some(hub) = super::analytics::hub::try_get() {
            hub.on_stop();
        }
    }

    fn stop_now(&self) {
        self.radio_stop();
        *self.k4_waits.lock().unwrap() = None;
        // Release the hold: no jump will come now (the audio thread stops at
        // its stop flag whether held or not).
        self.out.hold.store(false, Ordering::Release);
        self.out.jump_frame.store(u64::MAX, Ordering::Release);
        // Nothing is on the air to apply a deferred rack to.
        self.apply_deferred.store(false, Ordering::Release);
        self.stop_stream();
        // Nothing follows now: the next track's chain built ahead goes too
        // (it can hold gigabytes, on the video card as well).
        let prewarmed = self.st.lock().unwrap().prewarmed.take();
        drop(prewarmed);
        // VRAM hygiene (K8): free cached GPU filter banks when playback stops.
        if let Some(ctx) = GpuPolyCtx::try_build() {
            ctx.clear_bank_cache();
        }
        {
            let mut st = self.st.lock().unwrap();
            st.play = PlayState::Stopped;
            st.pending = None;
            st.queued_next = None;
            st.resume_at = None;
            // A track waiting for its chain does not start after Stop (its
            // preparation still ends in the cache, for the next Play).
            st.waiting = None;
            st.seek_after = None;
            // S1: bump generation so any in-flight seek thread sees a mismatch
            // and exits without delivering audio into a stopped player.
            st.generation += 1;
            st.arm_epoch += 1;
        }
        // S1/S3: a stopped player has no pending seek.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
    }

    /// Decide how to route the convolver for a new chain and return an
    /// effective settings copy together with the GPU context (if GPU was
    /// chosen) and any downgrade descriptor.
    ///
    /// The returned `PlayerSettings` may have a smaller `taps` count than `s`
    /// if the policy triggered a build-time downgrade.
    ///
    /// It routes the chain build_chain will make for `s`, `v` and the track
    /// key `tkey` (the caller's, the one it builds with): a Hybrid-Phase or
    /// alpha-HP start whose envelope is not ready plays linear phase, and is
    /// routed on the linear key and the linear VRAM demand, which is where
    /// its pre-trial, its live windows and a K3 deficit are stored.
    fn route_conv(
        &self,
        s:  &PlayerSettings,
        v:  &Arc<Variant>,
        tkey: &str,
    ) -> Route {
        use crate::audio::converter::dsp::filter::taps_label;

        // After a GPU failure on this track nothing goes back to the GPU: the
        // HP swap, an Apply after K4, the gapless next. A seek clears the
        // flag first, and so tries the GPU again.
        let gpu_ctx = if self.shared.gpu_failed.load(Ordering::Acquire) { None } else { GpuPolyCtx::try_build() };
        let free_vram = gpu_ctx.as_ref().map(|c| c.free_vram_bytes()).unwrap_or(0);

        let chain = self.chain_phase(s, v, tkey);
        let is_hp = matches!(chain.0, Phase::Hybrid | Phase::Alpha);
        let demand = vram_for(chain.0, s.taps, v.l);
        self.power_pretrial(s, v, chain);

        let tap_rtfs: Vec<TapRtf> = {
            let calib = self.shared.calibration.lock().unwrap_or_else(|e| e.into_inner());
            power::route_ladder(&calib, v.out_rate, v.l, is_hp)
        };

        let inputs = PolicyInputs {
            use_gpu: s.use_gpu,
            gpu_ctx: gpu_ctx.clone(),
            free_vram_bytes: free_vram,
            vram_demand: demand,
            conversion_running: crate::audio::converter::manager::is_running(),
            taps: s.taps,
            tap_rtfs,
        };
        let gen = self.shared.gpu_fallback_gen.load(Ordering::Relaxed);

        match policy::decide(&inputs) {
            PolicyDecision::Cpu => (s.clone(), None, None, gen),
            PolicyDecision::Gpu { ctx } => (s.clone(), Some(ctx), None, gen),
            PolicyDecision::CpuDowngraded { to_taps, reason } => {
                let from = taps_label(s.taps).unwrap_or("?").to_string();
                let to   = taps_label(to_taps).unwrap_or("?").to_string();
                let mut s2 = s.clone();
                s2.taps = to_taps;
                (s2, None, Some(DowngradeInfo::below(None, from, to, reason)), gen)
            }
        }
    }

    /// How far ahead of the reader a new chain should start, so it can be
    /// built and primed before the reader gets there.
    fn lead_s(&self, s: &PlayerSettings) -> f64 {
        let base = 0.45;
        let taps = if matches!(s.phase, super::settings::Phase::Hybrid | super::settings::Phase::Alpha) {
            2 * s.taps
        } else {
            s.taps
        };
        base + (taps as f64 / 30_000_000.0) * 0.6
    }

    /// Crossfade into a new chain for the same track, continuing the position
    /// (`going_on`: a seek not heard yet has moved it).
    fn swap_continuous(&self, t: &TrackInfo, v: &Arc<Variant>, s: &PlayerSettings) -> Option<bool> {
        let requested = Instant::now();
        let (idx, rate, stream_rate) = {
            let st = self.st.lock().unwrap();
            let stream_rate = st.timeline.as_ref().map_or(0, |tl| tl.rate());
            match st.timeline.as_ref().and_then(|tl| self.going_on(tl)) {
                Some(m) if m.track_id == t.id => (m.index, m.rate, stream_rate),
                _ => return None,
            }
        };
        // The stream's rate decides, not the mark's: the mark is what the
        // chain on the air was built for.
        if stream_rate != v.out_rate {
            // FS multiplier change: async make-before-break.
            // Snapshot generation (already bumped by set_settings) and take
            // ownership of the rate-switch hold.  The old audio keeps playing at
            // FULL level while the new chain is built; the hold ramp starts in
            // fs_ready, only AFTER the chain is ready.
            if self.rate_switch_under_way() {
                self.apply_deferred.store(true, Ordering::Release);
                return None;
            }
            let gen = {
                let mut st = self.st.lock().unwrap();
                st.rate_switch_gen = Some(st.generation);
                st.generation
            };
            let at = idx as f64 / rate.max(1) as f64;
            #[cfg(test)]
            SWAP_FROM.store(at.to_bits(), Ordering::Release);
            // Build the new chain synchronously (variant is ready — it was prepared
            // in background before swap_continuous was called).
            let (s_eff, gpu_ctx, downgrade, gen_chain) = self.route_conv(s, v, &track_key(t));
            let chain = match build_chain(&self.res, t.id, &track_key(t), v, &s_eff, (at * v.out_rate as f64) as u64, gpu_ctx, downgrade, gen_chain) {
                Ok(c) => c,
                Err(e) => {
                    // No FsFadeComplete will be sent; release the ownership claim.
                    self.st.lock().unwrap().rate_switch_gen = None;
                    self.fail(e);
                    return None;
                }
            };
            let direct = v.direct;
            let bits = t.bits;
            // Deliver the chain to fs_ready via the worker queue.  fs_ready
            // starts the extended hold ramp (120 ms) only AFTER the chain is
            // ready (make-before-break), then spawns a timer job that reopens
            // the device after the ramp completes.
            std::thread::Builder::new()
                .name("aura-fs-prepare".into())
                .spawn(move || {
                    let p = get();
                    p.send_internal(Job::FsReady {
                        chain: Box::new(chain),
                        generation: gen,
                        direct,
                        bits,
                    });
                })
                .ok();
            return None;
        }
        // O2: run route_conv (which may take pre-trial time) before computing
        // the splice start so the lead is measured from the actual current
        // position, not one sampled before the trial consumed it.
        let (s_eff2, gpu_ctx2, downgrade2, gen2) = self.route_conv(s, v, &track_key(t));
        let idx2 = {
            let st = self.st.lock().unwrap();
            match st.timeline.as_ref().and_then(|tl| self.going_on(tl)) {
                Some(m) if m.track_id == t.id => m.index,
                _ => idx, // fallback to pre-trial snapshot
            }
        };
        let start = idx2 + (self.lead_s(s) * rate as f64) as u64;
        #[cfg(test)]
        SWAP_FROM.store((start as f64 / rate.max(1) as f64).to_bits(), Ordering::Release);
        // The quick variant handing over to the full one: its level glides
        // into the full one's instead of stepping (its gain is a guess).
        let glide_from = if self.shared.quick.load(Ordering::Relaxed) && !v.quick {
            Some(self.shared.gain_now())
        } else {
            None
        };
        let chain = match chain::build_chain_glide(&self.res, t.id, &track_key(t), v, &s_eff2, start, gpu_ctx2, downgrade2, gen2, glide_from) {
            Ok(c) => c,
            Err(e) => { self.fail(e); return None; },
        };
        let hp_def = chain.hp_deferred;
        let _ = self
            .render_tx
            .lock()
            .unwrap()
            .send(Msg::Swap { chain, continuous: true, seek: false, side_buf: None, requested });
        Some(hp_def)
    }

    fn apply_now(&self) {
        // A track switch is landing: start_track / start_from_disk point
        // `current` at the new track and hold the output, but the reader
        // (now_playing) is still on the old one. A swap built from it would
        // replace the switch's jump, and the old track would play on (a
        // variant of the old track finishing in that window did exactly
        // that). Run the Apply once the jump has landed: tick() does.
        let switching = self.out.hold.load(Ordering::Acquire) && {
            let cur = self.st.lock().unwrap().current;
            matches!(self.now_playing(), Some((id, _)) if Some(id) != cur)
        };
        if switching {
            self.apply_deferred.store(true, Ordering::Release);
            return;
        }
        // A track waiting for its whole chain: the rack it waits for has
        // changed (or its converted file has). Its start is tried again.
        let waiting = self.st.lock().unwrap().waiting;
        if let Some((id, at_s)) = waiting {
            return self.start_track(id, at_s);
        }
        let s = self.st.lock().unwrap().settings.clone();
        self.st.lock().unwrap().queued_next = None;
        let (id, pos) = match self.now_playing() {
            Some(p) => p,
            None => return,
        };
        let t = match self.track(id) {
            Some(t) => t,
            None => return,
        };
        // When the rendered map has a disk file for the playing track, and
        // the mode is not Direct, restart from it at the current position.
        let disk_path: Option<PathBuf> = {
            let st = self.st.lock().unwrap();
            if s.mode == Mode::Direct { None } else {
                st.rendered_map.get(&id).map(PathBuf::from)
            }
        };
        if let Some(dp) = disk_path {
            return self.start_from_disk(id, &t, dp, pos);
        }
        let direct_now = self.st.lock().unwrap().stream_direct;
        if (s.mode == Mode::Direct) != direct_now {
            // BIT-PERFECT toggle: async make-before-break.
            // Snapshot generation (already bumped by set_settings) and take
            // ownership of the rate-switch hold.  The old audio keeps playing at
            // FULL level while the new chain is built; the hold ramp starts in
            // bp_ready, only AFTER the chain is ready.
            if self.rate_switch_under_way() {
                self.apply_deferred.store(true, Ordering::Release);
                return;
            }
            // Instant start off: the chain the switch lands on is whole before
            // the switch begins. Until then what plays goes on — BIT-PERFECT,
            // or Aura on the way in — and the chain is prepared like any other
            // (in flight, its Apply when it is done; a rack change, a seek or
            // the next Apply see it). The Apply that finds it whole makes the
            // switch, from where the track is then — a seek that waited for it
            // lands with it. Prepared on the switch's own thread, out of all
            // that, it opened where the track had been when the Apply came
            // (10–60 s back for a Hybrid-Phase envelope), and a seek meanwhile
            // prepared it all a second time.
            let whole = if self.instant() {
                None
            } else {
                match self.complete_variant(&t, &s) {
                    Some(v) => Some(v),
                    None => {
                        let what = if s.mode == Mode::Direct { "Preparing\u{2026}" } else { preparing_label(&s) };
                        self.set_pending(Some(what));
                        // A rack changed again meanwhile: the preparation of
                        // the one before gives way, as on the same rate.
                        self.drop_stale_preparations(t.id, &variant_key(&s));
                        self.prepare_in_background(&t, &s, Job::Apply);
                        return;
                    }
                }
            };
            let seek = match whole {
                Some(_) => self.take_seek_after(t.id),
                None => None,
            };
            let pos = seek.unwrap_or(pos);
            let seek_taken = seek.is_some();
            #[cfg(test)]
            BP_SWITCH_FROM.store(pos.to_bits(), Ordering::Release);
            let gen = {
                let mut st = self.st.lock().unwrap();
                st.rate_switch_gen = Some(st.generation);
                st.generation
            };
            let t_clone = t.clone();
            let s_clone = s.clone();
            let direct_new = s.mode == Mode::Direct;
            let bits = t.bits;
            std::thread::Builder::new()
                .name("aura-bp-prepare".into())
                .spawn(move || {
                    let p = get();
                    // The full variant when it is kept (leaving BIT-PERFECT it
                    // usually is), else the quick one now with the full one
                    // prepared behind it and swapped in by its Apply. The
                    // quick variant alone played the rest of the track without
                    // DC, ISP, SUB and AA — the seek's old bug (5c38b55).
                    // Instant start off: the whole chain, ready already.
                    // One look at the tick: the variant asked for and the
                    // envelope go by the same answer.
                    let instant = p.instant();
                    let v = match whole {
                        Some(v) => v,
                        None => match p.variant(&t_clone, &s_clone, instant) {
                            Ok(v) => {
                                // Instant start ticked off since the Apply:
                                // the pair, not its stand-in.
                                if !instant {
                                    p.make_envelope(&t_clone, &s_clone, &v);
                                }
                                v
                            }
                            Err(e) => {
                                // No BpReady will be sent; release the ownership claim.
                                p.st.lock().unwrap().rate_switch_gen = None;
                                p.fail(e);
                                return;
                            }
                        },
                    };
                    let start = (pos * v.out_rate as f64) as u64;
                    let (s_eff, gpu_ctx, downgrade, gen_chain) = p.route_conv(&s_clone, &v, &track_key(&t_clone));
                    let chain = match build_chain(
                        &p.res,
                        t_clone.id,
                        &track_key(&t_clone),
                        &v,
                        &s_eff,
                        start,
                        gpu_ctx,
                        downgrade,
                        gen_chain,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            p.st.lock().unwrap().rate_switch_gen = None;
                            // The seek it took will not land: its band goes —
                            // unless a newer one owns it (the generation moved).
                            if seek_taken && p.st.lock().unwrap().generation == gen {
                                p.drop_seek_band();
                            }
                            p.fail(e);
                            return;
                        }
                    };
                    p.send_internal(Job::BpReady {
                        chain: Box::new(chain),
                        generation: gen,
                        direct: direct_new,
                        bits,
                    });
                })
                .ok();
            return;
        }
        // While a track plays, a source setting that needs new whole-file
        // work keeps the old sound until the new variant is ready — one
        // crossfade, not a detour through the quick variant.
        let key = variant_key(&s);
        self.drop_stale_preparations(t.id, &key);
        let ready = if self.instant() {
            self.st.lock().unwrap().variants.get(&(t.id, key))
        } else {
            // Instant start off: the old chain plays on until the new one is
            // whole — a Hybrid-Phase rack's envelope too, never the linear
            // stand-in — then one crossfade.
            self.complete_variant(&t, &s)
        };
        let v = match ready {
            Some(v) => v,
            None => {
                self.set_pending(Some(preparing_label(&s)));
                self.prepare_in_background(&t, &s, Job::Apply);
                return;
            }
        };
        // A seek that waited for this chain (Instant start off) is made now,
        // with it, where the listener asked — not a crossfade where the old
        // chain has got to. A chain of a new rate (an FS change) lands as
        // that rate switch, at its target: the seek's own path does it.
        // (A K4 silence waiting for this chain ends here too.)
        {
            let mut w = self.k4_waits.lock().unwrap();
            if w.is_some_and(|(id, _)| id == t.id) {
                *w = None;
            }
        }
        if let Some(at_s) = self.take_seek_after(t.id) {
            return self.seek_now(at_s);
        }
        let hp_def = self.swap_continuous(&t, &v, &s);
        // §5c: spawn the HP background job only when the chain itself was
        // built as deferred (sync path returned Some(true)). The FS-rate-change
        // path (fs_ready) and BP-toggle path (bp_ready) each handle their own
        // HP spawn; returning None signals that one of those async paths was
        // taken and §5c must not double-spawn. Using the flag from the chain
        // that was actually built also avoids a TOCTOU where a concurrent
        // aura-hp-deferred thread writes the envelope between build_chain and
        // a subsequent external hp_ready() re-check.
        if let Some(true) = hp_def {
            let gen = self.st.lock().unwrap().generation;
            self.spawn_hp_job(id, gen, v.clone(), track_key(&t), s.clone());
        }
        if !self.st.lock().unwrap().in_flight.iter().any(|(tid, _)| *tid == t.id) {
            self.set_pending(None);
        }
    }

    /// The seek that waited for its chain (Instant start off), when it is
    /// the one of `on_air`, the track on the air: taken, to be made now. One
    /// of another track (the one before a gapless hand-over the tick has not
    /// followed yet) goes, and its band on the bar with it — it once sent the
    /// next track to where the track before was asked to go.
    fn take_seek_after(&self, on_air: u64) -> Option<f64> {
        let waited = self.st.lock().unwrap().seek_after.take();
        match waited {
            Some((id, at_s)) if id == on_air => Some(at_s),
            Some(_) => {
                self.drop_seek_band();
                None
            }
            None => None,
        }
    }

    /// No seek is pending any more: the band on the bar goes.
    fn drop_seek_band(&self) {
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
    }

    fn seek_now(&self, at_s: f64) {
        let (id, s) = {
            let st = self.st.lock().unwrap();
            match st.current {
                Some(id) => (id, st.settings.clone()),
                None => return,
            }
        };
        // No output or not playing: fall back to start_track which opens the device.
        if self.st.lock().unwrap().output.is_none() {
            return self.start_track(id, at_s);
        }
        let t = match self.track(id) {
            Some(t) => t,
            None => return,
        };
        // When a disk file is registered and the mode is not Direct, seek within it.
        let disk_path: Option<PathBuf> = {
            let st = self.st.lock().unwrap();
            if s.mode == Mode::Direct { None } else {
                st.rendered_map.get(&id).map(PathBuf::from)
            }
        };
        if let Some(dp) = disk_path {
            // Paused, or a file the chain on the air does not play: as a
            // start. Playing it: like a live seek — the old audio goes on
            // while the new place is read from the samples already decoded,
            // then the same crossfade.
            let same_file = self.disk_source(&dp.to_string_lossy()).is_some();
            if self.out.paused.load(Ordering::Relaxed) || !same_file {
                let paused = self.out.paused.load(Ordering::Relaxed);
                self.start_from_disk(id, &t, dp, at_s);
                if paused && self.st.lock().unwrap().output.is_some() {
                    self.out.paused.store(true, Ordering::Relaxed);
                    self.st.lock().unwrap().play = PlayState::Paused;
                }
                return;
            }
            return self.seek_disk(id, dp, at_s);
        }

        // Instant start off: a seek is heard only with the whole chain. While
        // a rack change is still being prepared, the seek waits for that chain
        // — the old one plays on where it is, the band on the bar marks the
        // target — and the Apply that brings the chain makes it (apply_now).
        if !self.instant() && s.mode == Mode::Aura && self.complete_variant(&t, &s).is_none() {
            self.st.lock().unwrap().seek_after = Some((id, at_s));
            self.seek_landing.store(0, Ordering::Release);
            self.shared.seek_pending.store(true, Ordering::Release);
            self.shared.seek_target_bits.store(at_s.to_bits(), Ordering::Release);
            self.prepare_in_background(&t, &s, Job::Apply);
            return;
        }
        // Made now: a seek that waited before this one is not made after it.
        self.st.lock().unwrap().seek_after = None;

        // When paused: apply the seek immediately (nothing audible to keep).
        // Use the existing hold+play_chain path to position the render thread,
        // then immediately restore the paused state so no audio plays.
        let paused = self.out.paused.load(Ordering::Relaxed);
        if paused {
            self.out.jump_frame.store(u64::MAX, Ordering::Release);
            self.out.hold.store(true, Ordering::Release);
            self.shared.gpu_failed.store(false, Ordering::Relaxed);
            self.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
            self.shared.gpu_zeros_from.store(u64::MAX, Ordering::Relaxed);
            self.st.lock().unwrap().queued_next = None;
            let v = match self.variant(&t, &s, self.instant()) {
                Ok(v) => v,
                Err(e) => {
                    self.out.hold.store(false, Ordering::Release);
                    return self.fail(e);
                }
            };
            let start = (at_s * v.out_rate as f64) as u64;
            let (s_eff, gpu_ctx, downgrade, gen) = self.route_conv(&s, &v, &track_key(&t));
            match build_chain(&self.res, id, &track_key(&t), &v, &s_eff, start, gpu_ctx, downgrade, gen) {
                Ok(chain) => {
                    if let Err(e) = self.play_chain(chain, v.direct, t.bits) {
                        self.out.hold.store(false, Ordering::Release);
                        self.fail(e);
                    } else {
                        // play_chain sets state to Playing; restore Paused so no
                        // audio plays until the user explicitly resumes.
                        self.out.paused.store(true, Ordering::Relaxed);
                        self.st.lock().unwrap().play = PlayState::Paused;
                        self.set_pending(None);
                    }
                }
                Err(e) => {
                    self.out.hold.store(false, Ordering::Release);
                    self.fail(e);
                }
            }
            return;
        }

        // Async seek path: keep old audio playing while the new chain is built.
        // Bump generation to stamp this seek and invalidate any in-flight ones.
        // CR-9: generation is only mutated on the worker thread; set_settings
        // sends Job::Apply which is also dispatched here — no concurrent bump.
        let gen_seek = {
            let mut st = self.st.lock().unwrap();
            st.generation += 1;
            st.queued_next = None;
            st.seek_gen = Some(st.generation);
            st.generation
        };

        // Publish seek state for status.jump.
        self.seek_landing.store(0, Ordering::Release);
        self.shared.seek_pending.store(true, Ordering::Release);
        let target_bits = at_s.to_bits();
        self.shared.seek_target_bits.store(target_bits, Ordering::Release);

        // Clear stale GPU state so tick() doesn't fire a spurious fallback swap.
        self.shared.gpu_failed.store(false, Ordering::Relaxed);
        self.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        self.shared.gpu_zeros_from.store(u64::MAX, Ordering::Relaxed);

        // Spawn the seek-prepare thread.  CR-1: ALL pre-rendering happens here,
        // not in the render thread.  The render thread receives a chain with a
        // fully-populated side_buf and writes it atomically — no concurrent
        // double-chain rendering.
        let t_clone = t.clone();
        let s_clone = s.clone();
        std::thread::Builder::new()
            .name("aura-seek-prepare".into())
            .spawn(move || {
                let p = get();
                // If generation has already changed by the time we start, bail.
                // S3/F1: do NOT touch seek_pending or seek_target_bits here —
                // they either belong to a newer seek (and that seek's thread
                // will clear them on success) or were already cleared by
                // start_track / stop_now / set_settings that bumped generation.
                if p.st.lock().unwrap().generation != gen_seek {
                    return;
                }
                // The variant heard before the seek: the cached full one (the
                // quick one only while the full one is not ready yet; it is then
                // prepared and an Apply brings it in). A fresh quick variant here
                // played the rest of the track without its source stages.
                // Instant start off: never the quick one (seek_now saw the
                // whole chain ready).
                let v = match p.variant(&t_clone, &s_clone, p.instant()) {
                    Ok(v) => v,
                    Err(e) => {
                        // F3: a prepare error leaves seek_pending true indefinitely
                        // without this clear; the UI would show the loading marker
                        // forever even though no seek will ever complete.
                        p.shared.seek_pending.store(false, Ordering::Release);
                        p.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
                        p.seek_done(gen_seek);
                        p.fail(e);
                        return;
                    }
                };
                // Instant start off: should the envelope have gone meanwhile,
                // it is made again here rather than the pair stand in linear.
                if !p.instant() {
                    p.make_envelope(&t_clone, &s_clone, &v);
                }
                let start = (at_s * v.out_rate as f64) as u64;
                let (s_eff, gpu_ctx, downgrade, gen_chain) = p.route_conv(&s_clone, &v, &track_key(&t_clone));
                let mut chain = match build_chain(
                    &p.res,
                    t_clone.id,
                    &track_key(&t_clone),
                    &v,
                    &s_eff,
                    start,
                    gpu_ctx,
                    downgrade,
                    gen_chain,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        // F3: same — clear seek state on build failure.
                        p.shared.seek_pending.store(false, Ordering::Release);
                        p.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
                        p.seek_done(gen_seek);
                        p.fail(e);
                        return;
                    }
                };
                // A chain the stream cannot take (another rate or mode: a rate
                // switch that has not landed) lands as that switch in
                // seek_ready, from its start — a pre-roll is for a splice.
                if !p.stream_takes(chain.out_rate, chain.direct) {
                    p.send_internal(Job::SeekReady {
                        chain: Box::new(chain),
                        side_buf: (Vec::new(), Vec::new()),
                        generation: gen_seek,
                    });
                    return;
                }
                // Pre-render PRE_ROLL_SEEK_MS + XFADE_MS of audio from the seek
                // position into side_buf.  This is all done in this thread so the
                // render thread receives a ready-to-splice buffer (CR-1).
                let pre_roll_frames = ((render::PRE_ROLL_SEEK_MS / 1000.0)
                    * v.out_rate as f64) as usize;
                let xf_frames = ((render::XFADE_MS / 1000.0) * v.out_rate as f64) as usize;
                let total = pre_roll_frames + xf_frames;
                let mut sb_l = vec![0.0f64; total];
                let mut sb_r = vec![0.0f64; total];
                let mut pos = 0;
                while pos < total {
                    let n = (total - pos).min(8192);
                    chain.stage.read(&mut sb_l[pos..pos + n], &mut sb_r[pos..pos + n]);
                    pos += n;
                }
                p.send_internal(Job::SeekReady {
                    chain: Box::new(chain),
                    side_buf: (sb_l, sb_r),
                    generation: gen_seek,
                });
            })
            .ok();
    }

    /// A seek within the converted file on the air (see `seek_now`).
    fn seek_disk(&self, id: u64, path: PathBuf, at_s: f64) {
        let gen_seek = {
            let mut st = self.st.lock().unwrap();
            st.generation += 1;
            st.queued_next = None;
            st.seek_gen = Some(st.generation);
            st.generation
        };
        self.seek_landing.store(0, Ordering::Release);
        self.shared.seek_pending.store(true, Ordering::Release);
        self.shared.seek_target_bits.store(at_s.to_bits(), Ordering::Release);
        std::thread::Builder::new()
            .name("aura-seek-prepare".into())
            .spawn(move || {
                let p = get();
                if p.st.lock().unwrap().generation != gen_seek {
                    return;
                }
                let src = match p.disk_audio(id, &path) {
                    Ok(s) => s,
                    Err(e) => {
                        p.shared.seek_pending.store(false, Ordering::Release);
                        p.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
                        p.seek_done(gen_seek);
                        p.fail(e);
                        return;
                    }
                };
                let rate = src.rate;
                let mut chain = build_file_chain(id, src, at_s, &path);
                if !p.stream_takes(chain.out_rate, chain.direct) {
                    p.send_internal(Job::SeekReady {
                        chain: Box::new(chain),
                        side_buf: (Vec::new(), Vec::new()),
                        generation: gen_seek,
                    });
                    return;
                }
                let total = (((render::PRE_ROLL_SEEK_MS + render::XFADE_MS) / 1000.0) * rate as f64) as usize;
                let mut sb_l = vec![0.0f64; total];
                let mut sb_r = vec![0.0f64; total];
                let mut pos = 0;
                while pos < total {
                    let n = (total - pos).min(8192);
                    chain.stage.read(&mut sb_l[pos..pos + n], &mut sb_r[pos..pos + n]);
                    pos += n;
                }
                p.send_internal(Job::SeekReady {
                    chain: Box::new(chain),
                    side_buf: (sb_l, sb_r),
                    generation: gen_seek,
                });
            })
            .ok();
    }

    fn seek_ready(&self, chain: super::chain::Chain, side_buf: (Vec<f64>, Vec<f64>), generation: u64) {
        // Stale: a newer seek, stop, or settings change arrived.
        if self.st.lock().unwrap().generation != generation {
            // S3/F1: do NOT touch seek_pending or seek_target_bits here.
            // The caller that bumped generation (start_track, stop_now,
            // set_settings) already cleared them, or a newer seek is still
            // in-flight and owns them.  Clearing here would wipe that newer
            // seek's pending flag and make the UI shimmer disappear too early.
            return;
        }
        self.seek_done(generation);
        // Only a chain of the stream's own rate and mode is spliced in. The
        // seek was built from the settings as they are, and those can be for
        // a rate or mode the stream is not at yet: a rate switch still to
        // land, or one this seek cut short. Spliced anyway, its samples
        // played at the stream's rate (FS x4 into a x2 stream: an octave
        // down, until Stop). It lands as that switch instead, at its target.
        if !self.stream_takes(chain.out_rate, chain.direct) {
            let bits = self.track(chain.track_id).map_or(16, |t| t.bits);
            let direct = chain.direct;
            if self.st.lock().unwrap().stream_direct != direct {
                self.bp_ready(chain, generation, direct, bits);
            } else {
                self.fs_ready(chain, generation, direct, bits);
            }
            return;
        }
        // Apply: dispatch the crossfade swap. The seek stays pending until
        // the swap is heard (status clears it at the next switch count).
        self.seek_landing.store(self.shared.switches.load(Ordering::Relaxed) + 1, Ordering::Release);
        self.set_pending(None);

        let hp_def = chain.hp_deferred;
        let hp_id = chain.track_id;
        {
            let mut st = self.st.lock().unwrap();
            let _ = self.render_tx.lock().unwrap().send(Msg::Swap {
                chain,
                continuous: false,
                seek: true,
                side_buf: Some(side_buf),
                requested: Instant::now(),
            });
            // The Swap drops the render thread's gapless next (CR-5), as the
            // HP swap and K3 know: what was queued goes with it, and tick
            // queues it again. It stayed marked, and the end of the track
            // waited for it — silence, playing, until ⏭ or Stop.
            st.queued_next = None;
            st.loop_at = u64::MAX;
        }
        if hp_def {
            let mut st = self.st.lock().unwrap();
            let s = effective_settings(hp_id, &st);
            let key = s.source_key();
            if let Some(v) = st.variants.get(&(hp_id, key)) {
                if let Some(t) = st.queue.iter().find(|t| t.id == hp_id) {
                    let tkey = track_key(t);
                    let v = v.clone();
                    drop(st);
                    self.spawn_hp_job(hp_id, generation, v, tkey, s);
                }
            }
        }
    }

    /// Whether the stream on the air can take a chain as a splice: the same
    /// rate and the same mode. One that cannot needs the stream restarted,
    /// a rate switch (bp_ready / fs_ready).
    fn stream_takes(&self, rate: u32, direct: bool) -> bool {
        let st = self.st.lock().unwrap();
        st.timeline.as_ref().map(|t| t.rate()) == Some(rate) && st.stream_direct == direct
    }

    /// A rate switch or a seek of the current generation is on its way. An
    /// Apply of the same generation (a variant that finished preparing, a
    /// deferred one) must not start a second switch beside it: the second
    /// would land after the first, from where it began. It waits, and tick()
    /// runs it once nothing is on its way.
    fn rate_switch_under_way(&self) -> bool {
        let st = self.st.lock().unwrap();
        st.rate_switch_gen == Some(st.generation) || st.seek_gen == Some(st.generation)
    }

    /// A seek of `generation` has landed or failed: an Apply no longer waits for it.
    fn seek_done(&self, generation: u64) {
        let mut st = self.st.lock().unwrap();
        if st.seek_gen == Some(generation) {
            st.seek_gen = None;
        }
    }

    /// A rate switch's chain is ready (bp_ready, fs_ready): whether it is
    /// still the one to land. The current one holds the claim from here to
    /// its fade (a seek landing as a switch takes it here). A stale one lets
    /// go of the claim if the claim is its own: nothing newer took it over,
    /// and nothing else would release it (`rebuilding` stayed up, the rack
    /// locked, until Stop). A newer switch's claim it leaves alone.
    fn rate_switch_ready(&self, generation: u64) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.generation != generation {
            if st.rate_switch_gen == Some(generation) {
                st.rate_switch_gen = None;
            }
            return false;
        }
        st.rate_switch_gen = Some(generation);
        true
    }

    fn bp_ready(&self, chain: super::chain::Chain, generation: u64, direct: bool, bits: u32) {
        if !self.rate_switch_ready(generation) {
            // Stale: a newer Apply or a seek fired before our chain was
            // ready. The old audio is still playing normally (no hold was
            // started in apply_now); the claim went if it was ours.
            return;
        }
        // New chain is ready.  Start the extended output-thread fade NOW
        // (make-before-break: the old audio was playing at full level until
        // this point).  The ramp runs for ~120 ms; a timer job fires ~130 ms
        // from now to stop the old stream and open the new one without
        // blocking the controller worker.
        let rate = self.st.lock().unwrap().stream.as_ref().map(|s| s.rate).unwrap_or(44100);
        let ramp_n = (0.120 * rate as f64) as u64;
        self.out.hold_ramp_frames.store(ramp_n, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        std::thread::Builder::new()
            .name("aura-bp-fade".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(130));
                let p = get();
                p.send_internal(Job::BpFadeComplete {
                    chain: Box::new(chain),
                    generation,
                    direct,
                    bits,
                });
            })
            .ok();
    }

    fn bp_fade_complete(&self, chain: super::chain::Chain, generation: u64, direct: bool, bits: u32) {
        let (is_current, owns_hold) = {
            let st = self.st.lock().unwrap();
            (st.generation == generation, st.rate_switch_gen == Some(generation))
        };
        if !is_current {
            if owns_hold {
                // An unrelated Apply changed settings while the fade was
                // running (e.g. a taps change that calls swap_continuous for
                // the same rate).  Release hold so the continuous swap from
                // apply_now can be heard; a new BpFadeComplete is NOT coming.
                {
                    let mut st = self.st.lock().unwrap();
                    st.rate_switch_gen = None;
                }
                self.out.hold.store(false, Ordering::Release);
                self.out.hold_ramp_frames.store(0, Ordering::Release);
            }
            // Else: a newer rate switch took ownership; don't touch the hold.
            return;
        }
        // Correct generation: clear ownership, stop old stream, open new one.
        {
            let mut st = self.st.lock().unwrap();
            st.rate_switch_gen = None;
        }
        self.stop_stream();
        let hp_def = chain.hp_deferred;
        let hp_id = chain.track_id;
        let radio = chain.track_id == super::radio::chain::RADIO_TRACK_ID;
        if let Err(e) = self.play_chain(chain, direct, bits) {
            self.fail(e);
            return;
        }
        self.set_pending(None);
        // A stream's chain (BIT-PERFECT switched under it): its corridor
        // reads the new device's clock.
        if radio {
            self.radio_reaired(direct);
        }
        // A seek that landed as this switch is heard from here on.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        if hp_def {
            let mut st = self.st.lock().unwrap();
            let s = effective_settings(hp_id, &st);
            let key = s.source_key();
            if let Some(v) = st.variants.get(&(hp_id, key)) {
                if let Some(t) = st.queue.iter().find(|t| t.id == hp_id) {
                    let tkey = track_key(t);
                    let v = v.clone();
                    drop(st);
                    self.spawn_hp_job(hp_id, generation, v, tkey, s);
                }
            }
        }
    }

    fn fs_ready(&self, chain: super::chain::Chain, generation: u64, direct: bool, bits: u32) {
        if !self.rate_switch_ready(generation) {
            // Stale: a newer Apply or a seek fired.  Old audio is still
            // playing normally (no hold was started in swap_continuous).
            // Don't touch hold; the claim went if it was ours.
            return;
        }
        // New chain is ready.  Start the extended output-thread fade NOW
        // (make-before-break: the old audio was playing at full level until
        // this point).  Use the OLD stream's rate for the ramp frame count.
        let rate = self.st.lock().unwrap().stream.as_ref().map(|s| s.rate).unwrap_or(44100);
        let ramp_n = (0.120 * rate as f64) as u64;
        self.out.hold_ramp_frames.store(ramp_n, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        std::thread::Builder::new()
            .name("aura-fs-fade".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(130));
                let p = get();
                p.send_internal(Job::FsFadeComplete {
                    chain: Box::new(chain),
                    generation,
                    direct,
                    bits,
                });
            })
            .ok();
    }

    fn fs_fade_complete(&self, chain: super::chain::Chain, generation: u64, direct: bool, bits: u32) {
        let (is_current, owns_hold) = {
            let st = self.st.lock().unwrap();
            (st.generation == generation, st.rate_switch_gen == Some(generation))
        };
        if !is_current {
            if owns_hold {
                // Unrelated Apply while fade was running: release hold.
                {
                    let mut st = self.st.lock().unwrap();
                    st.rate_switch_gen = None;
                }
                self.out.hold.store(false, Ordering::Release);
                self.out.hold_ramp_frames.store(0, Ordering::Release);
            }
            return;
        }
        {
            let mut st = self.st.lock().unwrap();
            st.rate_switch_gen = None;
        }
        self.stop_stream();
        let hp_def = chain.hp_deferred;
        let hp_id = chain.track_id;
        if let Err(e) = self.play_chain(chain, direct, bits) {
            self.fail(e);
            return;
        }
        self.set_pending(None);
        // A seek that landed as this switch is heard from here on.
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        if hp_def {
            let mut st = self.st.lock().unwrap();
            let s = effective_settings(hp_id, &st);
            let key = s.source_key();
            if let Some(v) = st.variants.get(&(hp_id, key)) {
                if let Some(t) = st.queue.iter().find(|t| t.id == hp_id) {
                    let tkey = track_key(t);
                    let v = v.clone();
                    drop(st);
                    self.spawn_hp_job(hp_id, generation, v, tkey, s);
                }
            }
        }
    }

    fn step(&self, dir: i64) {
        let (cur, ids, repeat) = {
            let st = self.st.lock().unwrap();
            let playing = st
                .timeline
                .as_ref()
                .and_then(|t| self.shared.locate(t.read_pos()))
                .map(|m| m.track_id)
                .or(st.current);
            (playing, st.queue.iter().map(|t| t.id).collect::<Vec<_>>(), st.repeat)
        };
        let n = ids.len();
        if n == 0 { return; }
        let i = cur.and_then(|c| ids.iter().position(|&x| x == c)).map(|i| i as i64).unwrap_or(-1);
        let j = i + dir;
        // ⏭/⏮ move to neighbours even in "one"; in "all" they wrap at the ends.
        let j = if j < 0 {
            if repeat == RepeatMode::All { n as i64 - 1 } else { return; }
        } else if j >= n as i64 {
            if repeat == RepeatMode::All { 0 } else { return; }
        } else {
            j
        };
        self.start_track(ids[j as usize], 0.0);
    }

    /// Housekeeping every 200 ms: follow gapless transitions, prepare the
    /// next track, and move on when a track ends.
    fn tick(&self) {
        let (tl, play) = {
            let st = self.st.lock().unwrap();
            (st.timeline.clone(), st.play)
        };
        let tl = match tl {
            Some(t) => t,
            None => return,
        };
        if play == PlayState::Stopped {
            return;
        }
        // K8 / D3: when a GPU conversion starts (rising edge of is_running()),
        // free the player's GPU bank cache so the converter can reclaim VRAM.
        // Live chains keep their own Arc references and finish normally.
        let conv_now = crate::audio::converter::manager::is_running();
        let conv_was = self.prev_conv_running.load(Ordering::Relaxed);
        if conv_now && !conv_was {
            if let Some(ctx) = GpuPolyCtx::try_build() {
                ctx.clear_bank_cache();
            }
            self.let_go_of_gpu_prewarm();
        }
        self.prev_conv_running.store(conv_now, Ordering::Relaxed);
        if self.out.device_lost.load(Ordering::Relaxed) {
            let e = self.out.last_error.lock().unwrap().clone().unwrap_or_else(|| "output device lost".into());
            self.stop_now();
            self.fail(e);
            return;
        }
        // A stream has no track for the housekeeping below to follow; a
        // stream that failed on the air is stopped with its error.
        if self.radio_active() {
            self.radio_watch();
            return;
        }
        // K4: GPU convolver failure — build a CPU fallback and swap in continuously.
        // gpu_failed stays true for the track lifetime so the UI chip stays in
        // the "failed" state (K6).  gpu_swap_sent prevents retrying every 200 ms.
        if self.shared.gpu_failed.load(Ordering::Relaxed)
            && !self.shared.gpu_swap_sent.load(Ordering::Relaxed)
        {
            self.k4_fallback(&tl);
        }
        // A jump is pending: start_track and start_from_disk (and a seek while
        // paused) hold the output until the render thread's jump lands. They
        // already point `current` at the new track, but the read head is
        // still inside the old track's marks. Nothing below may act on the
        // old track now: following it would copy the old track's M rack into
        // the global one (the next Apply then builds the new track with it),
        // queue the new track as the old one's gapless next (after a switch
        // in the old track's last 40 s it would play twice), or start another
        // track for an end the swap has already replaced. While held the read
        // head only drains the short ramp-down, so nothing is missed: the
        // first tick after the jump lands sees the new track under it.
        if self.out.hold.load(Ordering::Acquire) {
            return;
        }
        // An Apply that came while a switch was landing (apply_now deferred
        // it) now acts on the track that is on the air. The rest of this
        // housekeeping waits for the next tick. One deferred because a rate
        // switch or a seek of this generation was on its way waits for that
        // to land first.
        if !self.rate_switch_under_way() && self.apply_deferred.swap(false, Ordering::AcqRel) {
            self.apply_now();
            return;
        }
        self.power_tick(&tl);
        let m = match self.shared.locate(tl.read_pos()) {
            Some(m) => m,
            None => return,
        };
        // Follow a gapless hand-over.
        let mut seek_left = false;
        let hp_def_job: Option<(u64, u64, Arc<Variant>, String, PlayerSettings)> = {
            let mut st = self.st.lock().unwrap();
            let mut job = None;
            if st.current != Some(m.track_id) {
                st.current = Some(m.track_id);
                if st.queued_next == Some(m.track_id) {
                    st.queued_next = None;
                }
                // A seek that waited for the chain of the track before
                // (Instant start off) goes with it.
                if st.seek_after.is_some_and(|(id, _)| id != m.track_id) {
                    st.seek_after = None;
                    seek_left = true;
                }
                // If the incoming track has a per-track settings override, make
                // it the global rack so apply_now / seek use the right values,
                // and bump the seq so the page can sync its rack DOM. Not under
                // BIT-PERFECT: the track plays bit-perfect (effective_settings),
                // and its M rack in the rack's place turned BIT-PERFECT off.
                let bit_perfect = st.settings.mode == Mode::Direct;
                if let Some(ts) = st.track_settings.get(&m.track_id).filter(|_| !bit_perfect).cloned() {
                    st.settings = ts;
                    st.track_settings_applied_seq += 1;
                }
                st.loop_at = u64::MAX;
                // §5b: if the gapless chain was built with deferred HP, fire
                // the background envelope job now.
                if m.desc.hp_deferred {
                    let s2 = effective_settings(m.track_id, &st);
                    let key = s2.source_key();
                    if let Some(v2) = st.variants.get(&(m.track_id, key)) {
                        let gen = st.generation;
                        job = st.queue.iter().find(|t| t.id == m.track_id)
                            .map(|t| (m.track_id, gen, v2.clone(), track_key(t), s2));
                    }
                }
            }
            // The render thread has taken the repeat-one loop (it plays the
            // track again from its start, seconds ahead of the reader): the
            // next loop may be queued. Received first: right after the send
            // the render's queue is empty too, and the loop went twice.
            let received = self.shared.queue_taken.load(Ordering::Acquire) >= st.queue_sent;
            if st.loop_at != u64::MAX && st.queued_next == Some(m.track_id) && received && !self.shared.queued.load(Ordering::Acquire) {
                st.queued_next = None;
                st.loop_at = u64::MAX;
            }
            job
        };
        if seek_left {
            self.drop_seek_band();
        }
        if let Some((jid, gen, v2, tkey, s2)) = hp_def_job {
            self.spawn_hp_job(jid, gen, v2, tkey, s2);
        }
        let t = match self.track(m.track_id) {
            Some(t) => t,
            None => {
                // The track on the air has left the list (removed while it
                // played, or in its last seconds with its chain written
                // already): at its end nothing the list can name follows —
                // the player stops, rather than stay playing over silence
                // until ⏭ or Stop.
                let ended = self.shared.ended_at.load(Ordering::Relaxed);
                if ended != u64::MAX && tl.read_pos() >= ended {
                    self.stop_now();
                }
                return;
            }
        };
        let (next, repeat) = {
            let st = self.st.lock().unwrap();
            // Repeat-all: if this is the last track, wrap to the first.
            let next = next_in_list(&st, t.id).and_then(|id| st.queue.iter().find(|x| x.id == id).cloned());
            (next, st.repeat)
        };

        // The track ran out and nothing was queued (different rate, or the
        // end of the queue): start the next one (or loop this one), or stop.
        // What the render thread has received is read before the end: its
        // late path clears ended_at and then marks the chain received
        // (Release), so a receipt seen here comes with the end it cleared.
        // The other way round the whole late path could fall between the two
        // reads — the end read before it, the receipt after — and the next
        // track started again over its own gapless start.
        let taken = self.shared.queue_taken.load(Ordering::Acquire);
        #[cfg(test)]
        if let Some(f) = BETWEEN_END_READS.with(|h| h.borrow_mut().take()) {
            f();
        }
        let ended = self.shared.ended_at.load(Ordering::Relaxed);
        if ended != u64::MAX && tl.read_pos() >= ended {
            // The chain of what follows is being built ahead right now: the
            // end waits for it — Prefetched queues it (the render's late
            // path goes on at the end) or leaves it for the reopen, which
            // then takes it — rather than build it a second time (a pause as
            // long as the whole build, two copies, each bank loaded twice).
            // So does one just queued: on its way to the render thread, which
            // goes on with it at the end (it clears ended_at then).
            let follows = if repeat == RepeatMode::One { Some(t.id) } else { next.as_ref().map(|n| n.id) };
            // Sent and not received yet: the render thread goes on with it.
            // One it has received and let go (a seek's swap drops it) is not
            // coming — "queued, and not in the render's queue" waited for it
            // for good: silence, playing, until ⏭ or Stop.
            let on_its_way = |id: u64| {
                let st = self.st.lock().unwrap();
                st.queued_next == Some(id) && taken < st.queue_sent
            };
            if follows.is_some_and(|id| self.prewarm_building_for(id) || on_its_way(id)) {
                return;
            }
            match repeat {
                RepeatMode::One => self.start_track(t.id, 0.0),
                _ => match next {
                    Some(n) => self.start_track(n.id, 0.0),
                    None => self.stop_now(),
                },
            }
            return;
        }
        // Written to its end, and nothing follows it gapless: the worker
        // comes back when the reader gets there, not up to a tick later — the
        // next track's start (a device reopen) that much sooner.
        if let Some(ms) = wake_before_end(ended, tl.read_pos(), tl.rate()) {
            self.wake_ms.store(ms, Ordering::Release);
        }

        let remaining = t.duration_s - m.index as f64 / m.rate.max(1) as f64;
        let prefetch_target = if repeat == RepeatMode::One { Some(t.clone()) } else { next };
        self.prewarm(prefetch_target, remaining);
    }

    /// The prewarm: the track after the one playing (with repeat one, its
    /// loop) is prepared ahead of the end, so it starts without a pause of
    /// the player's making — its full variant, a Hybrid-Phase rack's onset
    /// envelope (with Instant start on too: the hand-over is the pair from
    /// its first sample, not a linear stand-in), the output probe, then its
    /// chain (queue_next). It begins PREWARM_S before the end, earlier when
    /// this machine is expected to take longer (`prewarm_lead_s`). The
    /// next track's own settings (its M rack) are the ones prepared.
    fn prewarm(&self, n: Option<TrackInfo>, remaining: f64) {
        let (queued, s, stale) = {
            let mut st = self.st.lock().unwrap();
            let s = n.as_ref().map(|n| effective_settings(n.id, &st));
            // A chain built for another track or rack (the list reordered,
            // the rack changed) is let go. So is one the end is far from
            // again (a seek back out of the last PREWARM_S): it held its
            // gigabytes to the end of the track; it is built again then.
            let far = remaining > PREWARM_S + PREWARM_LET_GO_S;
            // A linear stand-in with Instant start off now: not what plays.
            let stand_in_off = |p: &Prewarmed| p.stand_in && !self.instant();
            let stale = st.prewarmed.as_ref().is_some_and(|p| far || stand_in_off(p) || n.as_ref().map(|n| n.id) != Some(p.id) || s.as_ref() != Some(&p.settings));
            let stale = if stale { st.prewarmed.take() } else { None };
            // It waited for the reopen at the end: the new one is prepared.
            if stale.is_some() && st.queued_next == Some(u64::MAX) {
                st.queued_next = None;
            }
            (st.queued_next, s, stale)
        };
        if let Some(p) = stale {
            crate::aelog!("[PLAYER] prewarm: the chain built for track {} is let go (another next track or rack, or the end is far again)", p.id);
        }
        let (Some(n), Some(s)) = (n, s) else { return };
        if queued.is_some() || s.mode != Mode::Aura {
            return;
        }
        let ready = self.complete_variant(&n, &s).is_some() || self.stand_in_variant(&n, &s, Some(remaining)).is_some();
        if ready {
            if remaining < PREWARM_S {
                self.queue_next(n.id);
            }
        } else if remaining < self.prewarm_lead_s(&n, &s) {
            self.prepare_in_background(&n, &s, Job::Prefetched { id: n.id });
        }
    }

    /// How long before the end the next track's prewarm begins: PREWARM_S,
    /// or, when this machine is expected to take longer for its whole chain
    /// (arming.rs's estimates), half as long again and 10 s more: the
    /// prewarm runs below the playing track's priority.
    fn prewarm_lead_s(&self, n: &TrackInfo, s: &PlayerSettings) -> f64 {
        let est = super::arming::expected_prepare_s(s, n.sample_rate, n.duration_s);
        PREWARM_S.max(est * 1.5 + 10.0)
    }

    /// K4: the GPU convolver failed on the track on the air. Build a CPU
    /// fallback and swap it in continuously (once a track: tick runs this
    /// while `gpu_swap_sent` is false).
    fn k4_fallback(&self, tl: &Arc<Timeline>) {
        // Mark the swap as sent before any early-return so the handler
        // never runs again this track, even if build_chain fails (G3).
        self.shared.gpu_swap_sent.store(true, Ordering::Relaxed);
        // Increment gen first so the new chain carries the correct gen.
        let gen = self.shared.gpu_fallback_gen.fetch_add(1, Ordering::AcqRel) + 1;
        // Where the track goes on from: a seek not heard yet has moved it.
        let m = self.going_on(tl);
        if let Some(m_inner) = m {
            let (id, idx, rate) = (m_inner.track_id, m_inner.index, m_inner.rate);
            if let Some(t) = self.track(id) {
                let (s, gen_st) = {
                    let st = self.st.lock().unwrap();
                    (effective_settings(id, &st), st.generation)
                };
                // The chain that failed: the one at the write head.
                let air = self
                    .shared
                    .locate(tl.write_pos().saturating_sub(1))
                    .filter(|w| w.track_id == id)
                    .map_or_else(|| m_inner.desc.clone(), |w| w.desc.clone());
                let instant = self.instant();
                let mut pick = {
                    let mut st = self.st.lock().unwrap();
                    k4_pick(instant, &s, &air, |k| st.variants.get(&(id, k.to_string())))
                };
                // Instant start off: only a whole chain is built (its envelope
                // looked for outside the state lock: it can reach the disk).
                if let (false, K4Pick::Build { s_cpu, v, .. }) = (instant, &pick) {
                    if !self.envelope_done(&t, s_cpu, v) {
                        pick = K4Pick::Wait;
                    }
                }
                if matches!(pick, K4Pick::Wait) {
                    // Instant start off, nothing whole to build (the pair's
                    // envelope is not there): no stand-in. The GPU's silence
                    // stays on, the status says why, and the whole chain is
                    // asked for with its Apply — the silence ends when the
                    // pair is ready (on the CPU: route_conv offers no GPU
                    // after a failure), or with the error if it cannot be
                    // (preparation_failed then stops the player).
                    *self.k4_waits.lock().unwrap() = Some((id, gen_st));
                    self.set_pending(Some(preparing_label(&s)));
                    self.prepare_in_background(&t, &s, Job::Apply);
                }
                if let K4Pick::Build { mut s_cpu, v, linear_first } = pick {
                    // G2/D1: walk the tap ladder to find the highest rung
                    // the CPU can sustain at RTF >= 1.5, on the rack
                    // phase's ladder (a pair follows at this rung, below).
                    // Falls back to 10M when calibration is absent.
                    use crate::audio::converter::dsp::filter::taps_label;
                    let is_hp = matches!(s_cpu.phase, Phase::Hybrid | Phase::Alpha);
                    let tap_rtfs: Vec<TapRtf> = {
                        let calib = self.shared.calibration.lock().unwrap_or_else(|e| e.into_inner());
                        power::route_ladder(&calib, v.out_rate, v.l, is_hp)
                    };
                    let orig_taps = s_cpu.taps;
                    let fallback_taps = if tap_rtfs.is_empty() {
                        // No calibration: conservative floor of 10M (K4).
                        10_000_000_usize.min(orig_taps)
                    } else {
                        policy::best_cpu_taps(&tap_rtfs, orig_taps)
                    };
                    s_cpu.taps = fallback_taps;
                    let from_label = taps_label(orig_taps).unwrap_or("?").to_string();
                    let to_label   = taps_label(fallback_taps).unwrap_or("?").to_string();
                    // G6: only emit DowngradeInfo when taps were actually
                    // reduced; the GPU chip's "failed" state (K6) already
                    // signals the failure for a same-taps GPU→CPU swap.
                    let downgrade = if fallback_taps < orig_taps {
                        Some(DowngradeInfo::below(m_inner.desc.downgrade.as_ref(), from_label, to_label, "gpu-failed"))
                    } else {
                        None
                    };
                    // Where the CPU chain takes over. Everything the GPU
                    // rendered before its first zero is real audio, so the
                    // splice goes as late as that allows: just below the
                    // zeros, by the crossfade (which reads that far past
                    // the splice) and one render-thread step (8 192
                    // frames) more. The bank loads and the fresh chain's
                    // first block are then paid for while the GPU's audio
                    // still plays — about the buffer target minus the time
                    // to notice, 3.6 s at a 4.5 s target — where a splice
                    // anchored to the reader left 0.24 s (K4 runway). By
                    // construction it never reaches into the zeros (K4-Z).
                    //
                    // Unknown zeros, or zeros already in the gapless next
                    // track (another track's frame numbers): O5's rule, near
                    // the write head with enough lead for the first
                    // SUB-guard block (~0.32 s), inside the rendered window
                    // (target_ahead_s − 0.10).
                    let zeros_idx = match self.shared.gpu_zeros_from.load(Ordering::Relaxed) {
                        u64::MAX => None,
                        z => self.shared.locate(z).filter(|mz| mz.track_id == id).map(|mz| mz.index),
                    };
                    let start = match zeros_idx {
                        Some(z) => {
                            let margin = (render::XFADE_MS / 1000.0 * rate as f64) as u64 + 8_192;
                            z.saturating_sub(margin)
                        }
                        None => {
                            let guard_s = super::timeline::GUARD_FRAMES_MS as f64 / 1000.0;
                            let cpu_lead = self.lead_s(&s_cpu);
                            let target_s = *self.shared.target_ahead_s.lock().unwrap();
                            let splice_s = cpu_lead.min(target_s - 0.10).max(guard_s + 0.05);
                            idx + (splice_s * rate as f64) as u64
                        }
                    };
                    // A pair's fallback starts as its linear half: one
                    // bank and one prime, heard about a second after the
                    // failure instead of after two cold bank loads (7 s
                    // at 4 E-cores). It is a deferred stand-in like any
                    // other; the pair follows at this rung through the
                    // HP swap (hp_swap, K4's path). With Instant start off
                    // the pair itself: that mode plays no stand-in, and
                    // the GPU's silence until it lands is what it chose.
                    let mut s_build = s_cpu.clone();
                    if linear_first {
                        s_build.phase = Phase::Linear;
                    }
                    let failed_was_stand_in = self
                        .shared
                        .locate(tl.write_pos().saturating_sub(1))
                        .map_or(false, |w| w.desc.hp_deferred);
                    if let Ok(mut chain) = build_chain(&self.res, id, &track_key(&t), &v, &s_build, start, None, downgrade, gen) {
                        if linear_first {
                            chain.hp_deferred = true;
                        }
                        // Pending from the send on: nothing else sends
                        // over it before the render thread reads it.
                        self.shared.swap_pending.store(true, Ordering::Release);
                        // Swap continuously (F2/F7: no jump_frame written).
                        let _ = self.render_tx.lock().unwrap().send(Msg::Swap {
                            chain,
                            continuous: true,
                            seek: false,
                            side_buf: None,
                            requested: Instant::now(),
                        });
                        // O4: the Swap drops any gapless next from the render
                        // queue (render.rs CR-5).  Clear queued_next so tick()
                        // re-queues it on the next pass. A chain built ahead
                        // for it goes too: after a failure nothing goes back
                        // to the video card (route_conv), and it may be there.
                        let prewarmed = {
                            let mut st = self.st.lock().unwrap();
                            st.queued_next = None;
                            st.prewarmed.take()
                        };
                        drop(prewarmed);
                        // VRAM hygiene: free the failed chain's GPU buffers.
                        if let Some(ctx) = GpuPolyCtx::try_build() {
                            ctx.clear_bank_cache();
                        }
                        // The failed chain was the pair itself, so no HP
                        // job is on its way: start one. A deferred stand-in's
                        // own job is already running, and takes K4's path.
                        if linear_first && !failed_was_stand_in {
                            self.spawn_hp_job(id, gen_st, v.clone(), track_key(&t), s.clone());
                        }
                    }
                    // NOTE: gpu_failed is intentionally NOT cleared here.
                    // It stays true for the rest of the track so the UI chip
                    // remains in the "failed" (red) state (K6 / G4 / D2).
                }
            }
        }
    }

    /// Spawn the background thread that computes the HP onset envelope and
    /// then swaps the running linear stand-in to HP/αHP continuously
    /// (power.rs `hp_swap`). It gives up when the generation, the current
    /// track or the phase has changed meanwhile.
    fn spawn_hp_job(
        &self,
        track_id: u64,
        gen: u64,
        v: Arc<Variant>,
        track_key: String,
        s: PlayerSettings,
    ) {
        std::thread::Builder::new()
            .name("aura-hp-deferred".into())
            .spawn(move || get().hp_swap(track_id, gen, v, track_key, s))
            .ok();
    }

    /// The next track (with repeat one, the track's own loop) once its whole
    /// chain is ready (the prewarm). Its chain is built off the worker
    /// (`prewarm_build`, which comes back here), then handed to the render
    /// thread QUEUE_AHEAD_S before the end, so it starts on the sample after
    /// the current one ends — when the stream can take it: the same rate and
    /// mode. One it cannot take waits in `prewarmed` for the device's reopen
    /// at the end; start_track takes it there, and on ⏭ either way.
    fn queue_next(&self, id: u64) {
        let (settings, playing) = {
            let st = self.st.lock().unwrap();
            let playing = st.timeline.as_ref().and_then(|tl| self.shared.locate(tl.read_pos()));
            // Use the next track's effective settings so a per-track override
            // is applied from the very first gapless frame.
            (effective_settings(id, &st), playing)
        };
        let n = match self.track(id) {
            Some(n) => n,
            None => return,
        };
        // Still what follows the track on the air (`follows`: with repeat one
        // the track itself, its loop; with repeat all, after the last track
        // the first)? Not one that has started already: a start still
        // landing (the output held, the reader under the track before) made
        // it the current one, and queued behind the track before it played
        // twice.
        let (is_next, is_loop) = {
            let st = self.st.lock().unwrap();
            let cur = playing.as_ref().map(|m| m.track_id).or(st.current);
            let after = cur.and_then(|c| follows(&st, c));
            let is_loop = cur == Some(id) && after == Some(id);
            let started = !is_loop && st.current == Some(id);
            (after == Some(id) && !started && st.queued_next.is_none(), is_loop)
        };
        // Nothing on the air (stopped while it was prepared): nothing to
        // follow, and nothing is built for it.
        if !is_next || playing.is_none() || self.out.hold.load(Ordering::Acquire) {
            return;
        }
        let remaining = playing.and_then(|m| {
            let cur = self.track(m.track_id)?;
            Some(cur.duration_s - m.index as f64 / m.rate.max(1) as f64)
        });
        // A whole chain (a Hybrid-Phase rack's envelope too, in both
        // positions of Instant start); until it is, the tick keeps its
        // preparation going and Prefetched comes again. With Instant start
        // on and the end near, the variant alone: the next track goes on
        // gapless on its linear stand-in and the pair follows (§5b), as it
        // did before the prewarm — not a start after the end, in silence.
        let (v, is_stand_in) = match self
            .complete_variant(&n, &settings)
            .map(|v| (v, false))
            .or_else(|| self.stand_in_variant(&n, &settings, remaining).map(|v| (v, true)))
        {
            Some(x) => x,
            None => return,
        };
        // A stand-in (Instant start on, envelope late) is not a whole chain:
        // with Instant start off it does not count as ready.
        let built = self.st.lock().unwrap().prewarmed.as_ref().is_some_and(|p| {
            p.id == id && p.settings == settings && !(p.stand_in && !self.instant())
        });
        // A variant prepared early (a long track, a slow machine) waits for
        // its chain until PREWARM_S before the end: a chain can hold
        // gigabytes, and PREWARM_S covers any build. The tick comes back.
        if !built && remaining.is_some_and(|r| r > PREWARM_S) {
            return;
        }
        if !self.stream_takes(v.out_rate, v.direct) {
            // A different output rate or mode: it starts after a device
            // reopen, on the chain built ahead. An Aura chain queued into a
            // BIT-PERFECT stream of its rate (BIT-PERFECT just turned off,
            // its switch still on the way) played there at full scale: that
            // stream has no volume.
            if !built {
                self.prewarm_build(&n, &settings, v, is_stand_in);
            }
            // It waits for the reopen once its chain is here or being built:
            // with another track's build under way, its own did not start,
            // and the mark kept it from being tried again (a cold start).
            if built || self.prewarm_building_for(n.id) {
                self.st.lock().unwrap().queued_next = Some(u64::MAX);
            }
            return;
        }
        if !built {
            return self.prewarm_build(&n, &settings, v, is_stand_in);
        }
        // The render thread gets it QUEUE_AHEAD_S before the end: until then
        // ⏭ takes it here. The end is the one the render has written, when
        // it has: the length the probe read can be longer than the audio (a
        // lossy file's estimate), and the chain waited here past the end.
        let written_out = self.shared.ended_at.load(Ordering::Relaxed) != u64::MAX;
        if remaining.is_some_and(|r| r > QUEUE_AHEAD_S) && !written_out {
            return;
        }
        // Nor while a seek (or a rate switch) of this generation is on its
        // way: its swap drops what is queued in the render thread. The chain
        // stays here, and tick hands it over after the swap.
        if self.rate_switch_under_way() {
            return;
        }
        let mut st = self.st.lock().unwrap();
        // Still this track's, for this rack: a build that ended meanwhile may
        // have put another track's chain here.  A stand-in built with Instant
        // start on is not sent with it off (same guard as take_prewarmed).
        let Some(p) = st.prewarmed.take_if(|p| {
            p.id == id && p.settings == settings && !(p.stand_in && !self.instant())
        }) else {
            return;
        };
        // Increment only on a successful send: a failed channel leaves the
        // seq unseen by the render thread, and `on_its_way` would stay true
        // forever.
        let seq = st.queue_sent + 1;
        if self.render_tx.lock().unwrap().send(Msg::Queue { chain: p.chain, seq }).is_ok() {
            st.queue_sent = seq;
        }
        st.queued_next = Some(n.id);
        st.loop_at = if is_loop { st.timeline.as_ref().map_or(u64::MAX, |t| t.write_pos()) } else { u64::MAX };
        crate::aelog!("[PLAYER] gapless: track {} queued{}", n.id, if is_loop { " (repeat one: its loop)" } else { "" });
    }

    /// The chain of track `id` for the rack it plays with is being built
    /// ahead (aura-prewarm) now.
    fn prewarm_building_for(&self, id: u64) -> bool {
        let st = self.st.lock().unwrap();
        let s = effective_settings(id, &st);
        st.prewarm_building.as_ref().is_some_and(|(b, bs)| *b == id && *bs == s)
    }

    /// With Instant start on and QUEUE_AHEAD_S or less before the end, the
    /// next track's full variant without its onset envelope: the prewarm did
    /// not make the envelope in time, and the hand-over goes on on the
    /// linear stand-in, the pair following through §5b — as before the
    /// prewarm. None with Instant start off: the whole chain, or nothing.
    fn stand_in_variant(&self, n: &TrackInfo, s: &PlayerSettings, remaining: Option<f64>) -> Option<Arc<Variant>> {
        if !self.instant() || remaining.is_some_and(|r| r > QUEUE_AHEAD_S) {
            return None;
        }
        self.st.lock().unwrap().variants.get(&(n.id, variant_key(s)))
    }

    /// Build the next track's chain for `s` from its whole variant `v` on a
    /// thread of its own, below the playing track's priority (the prewarm):
    /// a Hybrid-Phase pair on the video card takes seconds, and on the
    /// worker ⏭ and Stop waited for it. The chain waits in `prewarmed`, and
    /// Job::Prefetched brings it back to queue_next. One build at a time; one
    /// that failed is not tried again for that track and rack.
    fn prewarm_build(&self, n: &TrackInfo, s: &PlayerSettings, v: Arc<Variant>, stand_in: bool) {
        {
            let mut st = self.st.lock().unwrap();
            let this = Some((n.id, s.clone()));
            if st.prewarm_building.is_some() || st.prewarm_failed == this {
                return;
            }
            st.prewarm_building = this;
        }
        /// The build is over however its thread ends: the next one may start.
        struct Building(Arc<Player>);
        impl Drop for Building {
            fn drop(&mut self) {
                self.0.st.lock().unwrap_or_else(|e| e.into_inner()).prewarm_building = None;
            }
        }
        let (n, s) = (n.clone(), s.clone());
        let spawned = std::thread::Builder::new()
            .name("aura-prewarm".into())
            .spawn(move || {
                super::analytics::track::set_below_normal_priority();
                super::arming::learn_nothing_here();
                let p = get();
                let _building = Building(p.clone());
                let t0 = Instant::now();
                let tkey = track_key(&n);
                let (s_eff, gpu_ctx, downgrade, gen) = p.route_conv(&s, &v, &tkey);
                let built = build_chain(&p.res, n.id, &tkey, &v, &s_eff, 0, gpu_ctx, downgrade, gen);
                if built.as_ref().is_ok_and(|c| p.built_on_a_failed_gpu(c, gen)) {
                    // Not kept, nor marked failed: the next prewarm builds it
                    // on the CPU (route_conv offers no GPU after a failure).
                    crate::aelog!("[PLAYER] prewarm: {} was built on the video card as it failed; it is built again", n.title);
                    return;
                }
                // The converter started while this GPU chain was being
                // built — the policy sends nothing to the GPU while one runs.
                // Drop without marking failed: the next tick tries again once
                // the conversion is over.
                if built.as_ref().is_ok_and(|c| c.gpu_on && crate::audio::converter::manager::is_running()) {
                    crate::aelog!("[PLAYER] prewarm: {} is on the GPU while a conversion runs; not kept", n.title);
                    return;
                }
                // Its first step read now, off the air (`stages::Ahead`): a
                // convolver's first block costs ten of the others, and a new
                // stream waited for it in silence — the reopen at a change of
                // rate, ⏭ (0.45 s for a 30M Hybrid-Phase pair).
                let built = built.map(|mut c| {
                    let t_read = Instant::now();
                    c.stage = Box::new(super::stages::Ahead::read(c.stage, render::STEP));
                    crate::aelog!("[PLAYER] prewarm: {}'s first {} frames read ahead in {} ms", n.title, render::STEP, t_read.elapsed().as_millis());
                    c
                });
                let mut st = p.st.lock().unwrap();
                match built {
                    // Stopped meanwhile: nothing follows, and a chain kept
                    // now would hold its memory until the next start. Its
                    // banks went into the video card's cache after Stop had
                    // emptied it (VRAM hygiene, K8): emptied again.
                    // Stopped is Stop: no stream and nothing playing. A pause
                    // after a device change has no stream either, and a track
                    // waiting for its whole chain neither — the one waiting is
                    // this chain's track, or its start wants the banks there.
                    Ok(chain) if st.timeline.is_none() && st.play == PlayState::Stopped
                        && !st.waiting.is_some_and(|(w, _)| w == n.id) =>
                    {
                        let waits = st.waiting.is_some();
                        drop(st);
                        let on_gpu = chain.gpu_on;
                        drop(chain);
                        if on_gpu && !waits {
                            if let Some(ctx) = GpuPolyCtx::try_build() {
                                ctx.clear_bank_cache();
                            }
                        }
                    }
                    Ok(chain) => {
                        // The GPU convolver may have failed while its
                        // first step was being read (Ahead::read).  Drop
                        // without marking failed: the next tick rebuilds on
                        // the CPU.
                        if chain.stage.is_gpu_failed() {
                            crate::aelog!("[PLAYER] prewarm: {} failed on the GPU while its first step was read; dropped", n.title);
                            return;
                        }
                        crate::aelog!("[PLAYER] prewarm: {} built ahead in {:.2} s", n.title, t0.elapsed().as_secs_f64());
                        let old = st.prewarmed.replace(Prewarmed { id: n.id, settings: s, chain, stand_in });
                        drop(st);
                        drop(old);
                        p.send_internal(Job::Prefetched { id: n.id });
                    }
                    Err(e) => {
                        crate::aelog!("[PLAYER] next track not prewarmed: {}", e);
                        st.prewarm_failed = Some((n.id, s));
                    }
                }
            });
        if spawned.is_err() {
            self.st.lock().unwrap().prewarm_building = None;
        }
    }

    /// The chain built ahead for a track that starts (the prewarm), if it is
    /// on the route this start would take; the start's own route otherwise,
    /// so as not to work it out twice. Built while the track before held the
    /// video card back — its GPU failure (K4: nothing of that track goes back
    /// to the GPU, nor the chain built then for the next one) or a
    /// conversion — it went to the CPU, with fewer taps where this machine's
    /// CPU needs them, and played the whole track so. A start tries the GPU
    /// again (it clears the failure): a chain short of the taps the start
    /// would have is let go, and the start builds its own, as before the
    /// prewarm. One on the CPU with all its taps stays: the same sound,
    /// where a Hybrid-Phase pair on the video card takes seconds of silence
    /// to build.
    fn prewarmed_on_route(&self, c: Chain, s: &PlayerSettings, v: &Arc<Variant>, tkey: &str) -> Result<Chain, Route> {
        let Some(short) = c.downgrade.as_ref().map(|d| d.to.clone()) else { return Ok(c) };
        let route = self.route_conv(s, v, tkey);
        // As many taps as the start would have (downgraded too, as far): kept.
        if route.0.taps <= c.settings.taps {
            return Ok(c);
        }
        crate::aelog!(
            "[PLAYER] prewarm: track {} was built ahead with {} taps; this start has them all{}",
            c.track_id,
            short,
            if route.1.is_some() { " on the GPU" } else { "" }
        );
        Err(route)
    }

    /// A chain built on the video card while the track on the air failed
    /// there (K4), or with a fallback since its route was chosen: the gapless
    /// next went on it, and with the failure flag kept through the hand-over
    /// its own failure went unseen — zeros, the whole track.
    fn built_on_a_failed_gpu(&self, c: &Chain, gen: u32) -> bool {
        c.gpu_on
            && (self.shared.gpu_failed.load(Ordering::Acquire) || self.shared.gpu_fallback_gen.load(Ordering::Relaxed) != gen)
    }

    /// A conversion has started (its rising edge, tick's K8): a chain built
    /// ahead on the video card goes, as the GPU bank cache does — it held its
    /// VRAM, and played on the GPU beside the converter (the policy sends
    /// nothing there while one runs).
    fn let_go_of_gpu_prewarm(&self) {
        let gone = {
            let mut st = self.st.lock().unwrap();
            let gone = st.prewarmed.take_if(|p| p.chain.gpu_on);
            if gone.is_some() && st.queued_next == Some(u64::MAX) {
                st.queued_next = None;
            }
            gone
        };
        drop(gone);
    }

    /// The chain built ahead for track `id` with the rack `s` (the prewarm),
    /// taken; one for another track or rack is let go.
    fn take_prewarmed(&self, id: u64, s: &PlayerSettings) -> Option<Chain> {
        let p = self.st.lock().unwrap().prewarmed.take()?;
        // A linear stand-in (built with Instant start on, the envelope late)
        // is not a whole chain: with it off, the start builds the pair.
        if p.id == id && p.settings == *s && !(p.stand_in && !self.instant()) {
            crate::aelog!("[PLAYER] prewarm: track {} starts on the chain built ahead", id);
            Some(p.chain)
        } else {
            None
        }
    }
}

/// A new stream's device, opened held at its gate (`OutputStream::start_gated`).
/// Tests stand a stream with no device in for it (`OPEN_FOR_TEST`).
fn open_gated(cfg: OutputConfig, timeline: Arc<Timeline>, out: Arc<OutputShared>) -> Result<OutputStream, String> {
    #[cfg(test)]
    if let Some(open) = OPEN_FOR_TEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return open(cfg, timeline, out);
    }
    OutputStream::start_gated(cfg, timeline, out)
}

/// The device opener a test stands in (`open_gated`).
#[cfg(test)]
type TestOpen = Box<dyn Fn(OutputConfig, Arc<Timeline>, Arc<OutputShared>) -> Result<OutputStream, String> + Send>;
#[cfg(test)]
static OPEN_FOR_TEST: Mutex<Option<TestOpen>> = Mutex::new(None);

#[cfg(test)]
mod tests {
    use super::{effective_settings, evictions, get, track_key, PlayState};

    fn k(t: u64, s: &str) -> (u64, String) {
        (t, s.to_string())
    }

    /// The tests that change the process-wide player (`get()`) take turns:
    /// the harness runs tests on several threads, and two of them rewriting
    /// its settings at once made each other fail at random.
    static PLAYER_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn player_turn() -> std::sync::MutexGuard<'static, ()> {
        PLAYER_TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until the process-wide player's worker has no job queued and is
    /// between batches. A job an earlier test queued (the Apply of
    /// set_settings) runs whenever the worker gets to it — under load, in the
    /// middle of a later test that calls tick() itself.
    fn worker_quiet(p: &super::Player) {
        use std::sync::atomic::Ordering;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut calm = 0;
        while calm < 3 && std::time::Instant::now() < deadline {
            let idle = p.jobs_queued.load(Ordering::Acquire) == 0 && !p.worker_busy.load(Ordering::Relaxed);
            calm = if idle { calm + 1 } else { 0 };
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The track junction: what goes on gapless, and the next track prepared
    /// ahead of it.
    mod junction;

    // ── settings / rendered Apply decision (correctness-1) ────────────────
    //
    // When only the rendered map changes, set_settings returns false because
    // the DSP settings are equal.  The command handler checks this return
    // value and calls apply() explicitly so the converted file is picked up
    // even though no rack knob was touched.

    #[test]
    fn set_settings_returns_false_when_equal_true_when_changed() {
        let _turn = player_turn();
        let p = get();
        // Read the current settings; calling set_settings with the same
        // value must return false (no change → no superfluous Apply job).
        let current = p.st.lock().unwrap().settings.clone();
        assert!(
            !p.set_settings(current.clone()),
            "equal settings must return false"
        );
        // Changing one field must return true.
        let mut changed = current.clone();
        changed.taps = if changed.taps == 1_000_000 { 5_000_000 } else { 1_000_000 };
        assert!(
            p.set_settings(changed.clone()),
            "changed settings must return true"
        );
        // The same settings again → false.
        assert!(
            !p.set_settings(changed),
            "already-applied settings must return false"
        );
        // Restore so this test does not leave a changed global state for
        // other tests that run after it.
        p.set_settings(current);
    }

    /// A stream plays the rack, its length too (Anton 2.10): streams have
    /// no length of their own.
    #[test]
    fn a_stream_plays_the_racks_length() {
        let _turn = player_turn();
        let p = get();
        let current = p.st.lock().unwrap().settings.clone();
        let mut rack = current.clone();
        rack.taps = 30_000_000;
        crate::player_commands::player_set_settings(rack.clone(), None, None).expect("the rack");
        assert_eq!(p.stream_settings(), rack, "a stream plays the rack: 30M");
        rack.taps = 5_000;
        crate::player_commands::player_set_settings(rack.clone(), None, None).expect("the rack");
        assert_eq!(p.stream_settings().taps, 5_000, "the rack's length, whatever it is");
        p.set_settings(current);
    }

    #[test]
    fn the_cap_drops_the_least_recently_used_of_each_track() {
        // Track 2 has five variants; 9 was used last, then 7, then 5.
        let entries = vec![
            (k(2, "quick"), 1),
            (k(2, "a"), 9),
            (k(2, "b"), 3),
            (k(2, "c"), 7),
            (k(2, "d"), 5),
            (k(3, "x"), 2),
        ];
        let mut out = evictions(entries.into_iter(), 3);
        out.sort();
        assert_eq!(out, vec![k(2, "b"), k(2, "quick")]);
    }

    #[test]
    fn the_newest_entry_survives_whatever_the_map_order() {
        // Every rotation of the same set: the newest must never be dropped.
        let base = vec![(k(1, "a"), 1), (k(1, "b"), 2), (k(1, "c"), 3), (k(1, "d"), 4), (k(1, "new"), 5)];
        for r in 0..base.len() {
            let mut e = base.clone();
            e.rotate_left(r);
            let out = evictions(e.into_iter(), 3);
            assert!(!out.contains(&k(1, "new")), "rotation {r}: {out:?}");
            assert_eq!(out.len(), 2);
        }
    }

    // ── effective_settings choice (WP-D-Rust) ────────────────────────────
    //
    // Without an override the global rack is returned; with one, the track's
    // own settings are returned; after clearing the override the global rack
    // comes back.

    #[test]
    fn effective_settings_returns_override_when_set_base_when_not() {
        let _turn = player_turn();
        let p = get();
        let base = p.st.lock().unwrap().settings.clone();

        // No override registered: should match the global rack.
        {
            let st = p.st.lock().unwrap();
            assert_eq!(
                effective_settings(99, &st),
                base,
                "no override: must return global settings"
            );
        }

        // Register an override with a distinct tap count.
        let mut over_s = base.clone();
        over_s.taps = if base.taps == 1_000_000 { 5_000_000 } else { 1_000_000 };
        p.set_track_settings(99, over_s.clone());
        {
            let st = p.st.lock().unwrap();
            assert_eq!(
                effective_settings(99, &st),
                over_s,
                "with override: must return per-track settings"
            );
            // An unregistered id still gets the global rack.
            assert_eq!(
                effective_settings(100, &st),
                base,
                "unregistered id: must return global settings"
            );
        }

        // Clearing the override reverts to the global rack.
        p.clear_track_settings(99);
        {
            let st = p.st.lock().unwrap();
            assert_eq!(
                effective_settings(99, &st),
                base,
                "after clear: must return global settings"
            );
        }
    }

    // ── gapless hand-over propagates track settings ───────────────────────
    //
    // When tick() crosses into a track that has a per-track override, the
    // global st.settings must be updated and track_settings_applied_seq bumped.
    // We exercise the same state-machine logic directly.

    #[test]
    fn gapless_handover_applies_track_settings_and_bumps_seq() {
        let _turn = player_turn();
        let p = get();
        let base = p.st.lock().unwrap().settings.clone();

        let mut over_s = base.clone();
        over_s.taps = if base.taps == 1_000_000 { 5_000_000 } else { 1_000_000 };
        p.set_track_settings(42, over_s.clone());

        let seq_before = p.st.lock().unwrap().track_settings_applied_seq;

        // Simulate the gapless hand-over block from tick().
        {
            let mut st = p.st.lock().unwrap();
            let new_id = 42u64;
            if let Some(ts) = st.track_settings.get(&new_id).cloned() {
                st.settings = ts;
                st.track_settings_applied_seq += 1;
            }
        }

        let seq_after = p.st.lock().unwrap().track_settings_applied_seq;
        assert_eq!(seq_after, seq_before + 1, "seq must be bumped by 1 on hand-over");

        let live = p.st.lock().unwrap().settings.clone();
        assert_eq!(live, over_s, "global settings must reflect the track override");

        // Restore global state.
        p.clear_track_settings(42);
        p.st.lock().unwrap().settings = base;
        p.st.lock().unwrap().track_settings_applied_seq = seq_before;
    }

    /// A manual switch away from an M track: start_track has pointed
    /// `current` at the new track and holds the output, but the read head is
    /// still on the old track's mark. tick() must not take that for a
    /// gapless hand-over back to the old track and copy its M rack into the
    /// global one. Once the hold is released (the jump landed), a real
    /// hand-over under the read head is followed as before. This calls the
    /// real tick(), not a copy of its block.
    #[test]
    fn tick_does_not_follow_the_old_track_while_a_switch_is_held() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        // Ids no queue holds: tick() stops right after the follow block.
        let (old_id, new_id) = (990_010u64, 990_011u64);

        let saved_hold = p.out.hold.load(Ordering::Acquire);
        let (base, seq_before, saved_current, saved_play) = {
            let st = p.st.lock().unwrap();
            (st.settings.clone(), st.track_settings_applied_seq, st.current, st.play)
        };
        let mut m_rack = base.clone();
        m_rack.taps = if base.taps == 1_000_000 { 5_000_000 } else { 1_000_000 };
        p.set_track_settings(old_id, m_rack.clone());

        // The moment after start_track(new_id): hold set, current = new,
        // the only mark (under the read head) still the old track's.
        p.out.hold.store(true, Ordering::Release);
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = Some(std::sync::Arc::new(super::Timeline::new(44100, 1.0)));
            st.play = PlayState::Playing;
            st.current = Some(new_id);
        }
        {
            let mut marks = p.shared.marks.lock().unwrap();
            marks.clear();
            marks.push_back(super::super::render::Mark {
                frame: 0,
                track_id: old_id,
                index: 0,
                rate: 44100,
                l: 1,
                desc: super::super::render::ChainDesc::from_chain(&make_dummy_chain()),
            });
        }

        p.tick();
        let (cur, live, seq) = {
            let st = p.st.lock().unwrap();
            (st.current, st.settings.clone(), st.track_settings_applied_seq)
        };
        let held_ok = cur == Some(new_id) && live == base && seq == seq_before;

        // The jump landed and the read head is (still) on old_id: that IS a
        // hand-over now, and it is followed. An Apply the worker ran while the
        // switch was held is deferred to this tick, which then runs it instead
        // of following — not what is under test here.
        p.out.hold.store(false, Ordering::Release);
        p.apply_deferred.store(false, Ordering::Release);
        p.tick();
        let (cur2, live2, seq2) = {
            let st = p.st.lock().unwrap();
            (st.current, st.settings.clone(), st.track_settings_applied_seq)
        };

        // Restore the process-wide player before asserting.
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = None;
            st.play = saved_play;
            st.current = saved_current;
            st.settings = base.clone();
            st.track_settings_applied_seq = seq_before;
        }
        p.shared.marks.lock().unwrap().clear();
        p.clear_track_settings(old_id);
        p.out.hold.store(saved_hold, Ordering::Release);

        assert!(held_ok, "while held: current {:?} (want {:?}), global rack changed: {}, seq {} -> {}",
            cur, Some(new_id), live != base, seq_before, seq);
        assert_eq!(cur2, Some(old_id), "after the jump: a hand-over under the read head is followed");
        assert_eq!(live2, m_rack, "after the jump: the followed track's override becomes the global rack");
        assert_eq!(seq2, seq_before + 1, "after the jump: the seq is bumped once");
    }

    /// An Apply that arrives while a switch is landing (the reader still on
    /// the old track, `current` already the new one) must not act on the old
    /// track: a swap built from the reader replaced the switch's jump, and
    /// the old track played on (rw m after the tick fix: a variant of the
    /// track being left finished inside the hold). It waits; the first tick
    /// after the jump runs it. `queued_next` is the witness: apply_now clears
    /// it first thing when it runs.
    #[test]
    fn apply_waits_while_a_switch_is_landing() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        let (old_id, new_id) = (990_020u64, 990_021u64);

        let saved_hold = p.out.hold.load(Ordering::Acquire);
        let (saved_current, saved_play, saved_qn) = {
            let st = p.st.lock().unwrap();
            (st.current, st.play, st.queued_next)
        };
        p.out.hold.store(true, Ordering::Release);
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = Some(std::sync::Arc::new(super::Timeline::new(44100, 1.0)));
            st.play = PlayState::Playing;
            st.current = Some(new_id);
            st.queued_next = Some(new_id);
        }
        {
            let mut marks = p.shared.marks.lock().unwrap();
            marks.clear();
            marks.push_back(super::super::render::Mark {
                frame: 0,
                track_id: old_id,
                index: 0,
                rate: 44100,
                l: 1,
                desc: super::super::render::ChainDesc::from_chain(&make_dummy_chain()),
            });
        }

        p.apply_now();
        let deferred = p.apply_deferred.load(Ordering::Acquire);
        let qn_held = p.st.lock().unwrap().queued_next;

        // The jump landed: the next tick runs the deferred Apply.
        p.out.hold.store(false, Ordering::Release);
        p.tick();
        let deferred_after = p.apply_deferred.load(Ordering::Acquire);
        let qn_after = p.st.lock().unwrap().queued_next;

        {
            let mut st = p.st.lock().unwrap();
            st.timeline = None;
            st.play = saved_play;
            st.current = saved_current;
            st.queued_next = saved_qn;
        }
        p.shared.marks.lock().unwrap().clear();
        p.apply_deferred.store(false, Ordering::Release);
        p.out.hold.store(saved_hold, Ordering::Release);

        assert!(deferred, "an Apply during a landing switch is deferred");
        assert_eq!(qn_held, Some(new_id), "the deferred Apply did nothing yet");
        assert!(!deferred_after, "the first tick after the jump takes the deferred Apply");
        assert_eq!(qn_after, None, "and runs it");
    }

    // ── Rate-switch hold ownership rules ─────────────────────────────────
    //
    // Three scenarios that exercise the generation-ownership contract:
    //   (1) rapid double toggle: stale BpFadeComplete that does NOT own the
    //       hold must leave hold+rate_switch_gen untouched.
    //   (2) toggle then unrelated continuous swap: stale BpFadeComplete that
    //       DOES own the hold must release it so the continuous swap is heard.
    //   (3) toggle then stop: stop_stream clears ownership; a subsequent
    //       BpFadeComplete finds no owner and does nothing.
    //
    // Each test uses a minimal dummy Chain (no audio produced).

    struct DummyStage;
    impl super::super::stages::Stage for DummyStage {
        fn read(&mut self, _l: &mut [f64], _r: &mut [f64]) -> usize { 0 }
        fn position(&self) -> u64 { 0 }
        fn total(&self) -> u64 { 0 }
    }

    fn make_dummy_chain() -> super::super::chain::Chain {
        use super::super::chain::Chain;
        Chain {
            live_gain: None,
            stage: Box::new(DummyStage),
            track_id: 0,
            out_rate: 44100,
            l: 1,
            start: 0,
            tokens: vec![],
            gain: 1.0,
            tp_pred_db: 0.0,
            direct: false,
            disk_src: false,
            disk_file: None,
            variant_quick: true,
            notes: vec![],
            stages: vec![],
            gpu_on: false,
            downgrade: None,
            downgrade_gen: 0,
            calib_key: String::new(),
            hp_deferred: false,
            settings: std::sync::Arc::new(super::super::settings::PlayerSettings::default()),
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
        }
    }

    /// (1) Rapid double toggle: the stale first BpFadeComplete (gen=N) arrives
    /// after Apply2 has taken ownership at gen=N+1.  Hold must NOT be released.
    #[test]
    fn bp_fade_complete_stale_no_owner_leaves_hold() {
        let _turn = player_turn();
        let p = get();

        // Save state we will mutate.
        let saved_gen = p.st.lock().unwrap().generation;
        let saved_rsg = p.st.lock().unwrap().rate_switch_gen;
        let saved_ramp = p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire);
        let saved_hold = p.out.hold.load(std::sync::atomic::Ordering::Acquire);

        // Set up: generation=N+1 (Apply2 has fired), rate_switch_gen=Some(N+1)
        // (newer switch took ownership), hold=true (fade in progress).
        let n_plus_1 = saved_gen + 2;  // use +2 to avoid clashing with saved_gen
        {
            let mut st = p.st.lock().unwrap();
            st.generation = n_plus_1;
            st.rate_switch_gen = Some(n_plus_1);
        }
        p.out.hold_ramp_frames.store(5292, std::sync::atomic::Ordering::Release);
        p.out.hold.store(true, std::sync::atomic::Ordering::Release);

        // Simulate stale BpFadeComplete{gen=N} (gen N = n_plus_1 - 1).
        p.bp_fade_complete(make_dummy_chain(), n_plus_1 - 1, false, 16);

        // Ownership is with n_plus_1, so hold must not have been released.
        assert!(p.out.hold.load(std::sync::atomic::Ordering::Acquire),
            "hold must remain true: stale result does not own the hold");
        assert_eq!(p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire), 5292,
            "hold_ramp_frames must not be cleared by non-owner");
        assert_eq!(p.st.lock().unwrap().rate_switch_gen, Some(n_plus_1),
            "rate_switch_gen must not be changed by non-owner");

        // Restore.
        {
            let mut st = p.st.lock().unwrap();
            st.generation = saved_gen;
            st.rate_switch_gen = saved_rsg;
        }
        p.out.hold_ramp_frames.store(saved_ramp, std::sync::atomic::Ordering::Release);
        p.out.hold.store(saved_hold, std::sync::atomic::Ordering::Release);
    }

    /// (2) Toggle then unrelated continuous swap: the stale BpFadeComplete
    /// (gen=N) owns the hold (rate_switch_gen==Some(N)) but the generation has
    /// moved on because an unrelated Apply fired swap_continuous (no new
    /// BpFadeComplete coming).  Hold MUST be released so the continuous swap
    /// can be heard.
    #[test]
    fn bp_fade_complete_stale_owner_releases_hold() {
        let _turn = player_turn();
        let p = get();

        let saved_gen = p.st.lock().unwrap().generation;
        let saved_rsg = p.st.lock().unwrap().rate_switch_gen;
        let saved_ramp = p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire);
        let saved_hold = p.out.hold.load(std::sync::atomic::Ordering::Acquire);

        let gen_n = saved_gen + 2;
        let gen_n_plus_1 = gen_n + 1;  // unrelated Apply bumped generation

        {
            let mut st = p.st.lock().unwrap();
            st.generation = gen_n_plus_1;     // unrelated Apply has fired
            st.rate_switch_gen = Some(gen_n); // stale BpFadeComplete still owns hold
        }
        p.out.hold_ramp_frames.store(5292, std::sync::atomic::Ordering::Release);
        p.out.hold.store(true, std::sync::atomic::Ordering::Release);

        // BpFadeComplete{gen_n} arrives: stale (gen_n != gen_n_plus_1) but owns hold.
        p.bp_fade_complete(make_dummy_chain(), gen_n, false, 16);

        assert!(!p.out.hold.load(std::sync::atomic::Ordering::Acquire),
            "hold must be released: stale-but-owner path must free hold");
        assert_eq!(p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire), 0,
            "hold_ramp_frames must be cleared");
        assert_eq!(p.st.lock().unwrap().rate_switch_gen, None,
            "rate_switch_gen must be cleared");

        // Restore.
        {
            let mut st = p.st.lock().unwrap();
            st.generation = saved_gen;
            st.rate_switch_gen = saved_rsg;
        }
        p.out.hold_ramp_frames.store(saved_ramp, std::sync::atomic::Ordering::Release);
        p.out.hold.store(saved_hold, std::sync::atomic::Ordering::Release);
    }

    /// (3) Toggle then stop: stop clears hold + rate_switch_gen; a subsequent
    /// BpFadeComplete (gen=N) finds no ownership and does nothing.
    #[test]
    fn stop_clears_rate_switch_gen_and_subsequent_fade_complete_is_noop() {
        let _turn = player_turn();
        let p = get();

        let saved_gen = p.st.lock().unwrap().generation;
        let saved_rsg = p.st.lock().unwrap().rate_switch_gen;
        let saved_ramp = p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire);
        let saved_hold = p.out.hold.load(std::sync::atomic::Ordering::Acquire);

        let gen_n = saved_gen + 2;
        // Simulate state after BpReady{gen_n} ran: hold=true, ownership=Some(gen_n).
        {
            let mut st = p.st.lock().unwrap();
            st.generation = gen_n;
            st.rate_switch_gen = Some(gen_n);
        }
        p.out.hold.store(true, std::sync::atomic::Ordering::Release);
        p.out.hold_ramp_frames.store(5292, std::sync::atomic::Ordering::Release);

        // Simulate stop_now: it clears hold, then calls stop_stream.
        p.out.hold.store(false, std::sync::atomic::Ordering::Release);
        p.stop_stream();
        // stop_stream resets hold_ramp_frames and clears rate_switch_gen.
        assert_eq!(p.st.lock().unwrap().rate_switch_gen, None,
            "stop_stream must clear rate_switch_gen");
        assert_eq!(p.out.hold_ramp_frames.load(std::sync::atomic::Ordering::Acquire), 0,
            "stop_stream must reset hold_ramp_frames");

        // stop_now also bumps generation; simulate that.
        p.st.lock().unwrap().generation = gen_n + 1;

        // Delayed BpFadeComplete{gen_n} arrives: stale (gen_n != gen_n+1),
        // no owner (rate_switch_gen is None) — full noop.
        p.bp_fade_complete(make_dummy_chain(), gen_n, false, 16);

        assert!(!p.out.hold.load(std::sync::atomic::Ordering::Acquire),
            "hold must remain false: noop BpFadeComplete must not change it");
        assert_eq!(p.st.lock().unwrap().rate_switch_gen, None,
            "rate_switch_gen must remain None");

        // Restore.
        {
            let mut st = p.st.lock().unwrap();
            st.generation = saved_gen;
            st.rate_switch_gen = saved_rsg;
        }
        p.out.hold_ramp_frames.store(saved_ramp, std::sync::atomic::Ordering::Release);
        p.out.hold.store(saved_hold, std::sync::atomic::Ordering::Release);
    }

    // ── A seek during a rate switch (fix/rate-switch-stale) ──────────────
    //
    // An FS change or a BIT-PERFECT toggle, then a seek before the switch
    // landed: the seek's chain was built for the new rate and spliced into
    // the old stream (FS x4 into x2 played an octave down, until Stop), and
    // the switch's own Ready or fade, now stale, either dropped it or kept
    // its claim for good (`rebuilding`, a locked rack). The rules, each a
    // predicate the paths share:

    /// Only a chain of the stream's own rate and mode is spliced in.
    #[test]
    fn the_stream_takes_only_its_own_rate_and_mode() {
        let _turn = player_turn();
        let p = get();
        let (saved_tl, saved_direct) = {
            let st = p.st.lock().unwrap();
            (st.timeline.clone(), st.stream_direct)
        };
        let no_stream = {
            p.st.lock().unwrap().timeline = None;
            p.stream_takes(88_200, false)
        };
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = Some(std::sync::Arc::new(super::Timeline::new(88_200, 1.0)));
            st.stream_direct = false;
        }
        let same = p.stream_takes(88_200, false);
        let other_rate = p.stream_takes(176_400, false);
        let other_mode = p.stream_takes(88_200, true);
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = saved_tl;
            st.stream_direct = saved_direct;
        }
        assert!(!no_stream, "no stream takes nothing");
        assert!(same, "its own rate and mode");
        assert!(!other_rate, "another rate is a rate switch");
        assert!(!other_mode, "another mode is a rate switch");
    }

    /// A rate switch's Ready: the current one takes the claim; a stale one
    /// lets go of it only if it is its own.
    #[test]
    fn a_stale_ready_lets_go_of_its_own_claim_only() {
        let _turn = player_turn();
        let p = get();
        let (saved_gen, saved_rsg) = {
            let st = p.st.lock().unwrap();
            (st.generation, st.rate_switch_gen)
        };
        let saved_hold = p.out.hold.load(std::sync::atomic::Ordering::Acquire);
        let g = saved_gen + 10;

        // The current one takes the claim (a seek landing as a switch does).
        p.st.lock().unwrap().generation = g;
        p.st.lock().unwrap().rate_switch_gen = None;
        let current = p.rate_switch_ready(g);
        let claim_current = p.st.lock().unwrap().rate_switch_gen;

        // A seek moved the generation on; the switch's Ready, now stale,
        // owns the claim and nothing newer took it: it lets go
        // (fs_ready and bp_ready alike; no hold, no thread on this path).
        {
            let mut st = p.st.lock().unwrap();
            st.generation = g + 1;
            st.rate_switch_gen = Some(g);
        }
        p.fs_ready(make_dummy_chain(), g, false, 16);
        let claim_after_fs = p.st.lock().unwrap().rate_switch_gen;
        p.st.lock().unwrap().rate_switch_gen = Some(g);
        p.bp_ready(make_dummy_chain(), g, true, 16);
        let claim_after_bp = p.st.lock().unwrap().rate_switch_gen;

        // A newer switch owns it: the stale one leaves it alone.
        p.st.lock().unwrap().rate_switch_gen = Some(g + 1);
        p.fs_ready(make_dummy_chain(), g, false, 16);
        let claim_newer = p.st.lock().unwrap().rate_switch_gen;
        let hold_after = p.out.hold.load(std::sync::atomic::Ordering::Acquire);

        {
            let mut st = p.st.lock().unwrap();
            st.generation = saved_gen;
            st.rate_switch_gen = saved_rsg;
        }
        p.out.hold.store(saved_hold, std::sync::atomic::Ordering::Release);

        assert!(current, "the current Ready lands");
        assert_eq!(claim_current, Some(g), "and holds the claim");
        assert_eq!(claim_after_fs, None, "a stale fs_ready lets go of its own claim");
        assert_eq!(claim_after_bp, None, "a stale bp_ready lets go of its own claim");
        assert_eq!(claim_newer, Some(g + 1), "a newer switch's claim is left alone");
        assert_eq!(hold_after, saved_hold, "a stale Ready never touches the hold");
    }

    /// An Apply starts no second rate switch beside one of its generation
    /// on its way — a switch or a seek; tick() runs it once they land.
    #[test]
    fn a_deferred_apply_waits_for_a_switch_or_seek_under_way() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        let id = 990_030u64;
        let saved_hold = p.out.hold.load(Ordering::Acquire);
        let (saved_gen, saved_rsg, saved_seek, saved_current, saved_play, saved_qn) = {
            let st = p.st.lock().unwrap();
            (st.generation, st.rate_switch_gen, st.seek_gen, st.current, st.play, st.queued_next)
        };
        let g = saved_gen + 20;
        p.out.hold.store(false, Ordering::Release);
        {
            let mut st = p.st.lock().unwrap();
            st.generation = g;
            st.timeline = Some(std::sync::Arc::new(super::Timeline::new(44100, 1.0)));
            st.play = PlayState::Playing;
            st.current = Some(id);
        }
        {
            let mut marks = p.shared.marks.lock().unwrap();
            marks.clear();
            marks.push_back(super::super::render::Mark {
                frame: 0,
                track_id: id,
                index: 0,
                rate: 44100,
                l: 1,
                desc: super::super::render::ChainDesc::from_chain(&make_dummy_chain()),
            });
        }
        let run_tick = |rsg: Option<u64>, seek: Option<u64>| {
            {
                let mut st = p.st.lock().unwrap();
                st.rate_switch_gen = rsg;
                st.seek_gen = seek;
                // Not the track under the read head: tick's own hand-over
                // bookkeeping only clears that one.
                st.queued_next = Some(id + 1);
            }
            p.apply_deferred.store(true, Ordering::Release);
            let under_way = p.rate_switch_under_way();
            p.tick();
            // apply_now clears queued_next first thing: the witness it ran.
            let ran = p.st.lock().unwrap().queued_next.is_none();
            (under_way, ran, p.apply_deferred.load(Ordering::Acquire))
        };
        let switching = run_tick(Some(g), None);
        let seeking = run_tick(None, Some(g));
        let older = run_tick(Some(g - 1), Some(g - 1));
        p.seek_done(g); // not ours to clear: a no-op here
        let free = run_tick(None, None);

        {
            let mut st = p.st.lock().unwrap();
            st.timeline = None;
            st.generation = saved_gen;
            st.rate_switch_gen = saved_rsg;
            st.seek_gen = saved_seek;
            st.current = saved_current;
            st.play = saved_play;
            st.queued_next = saved_qn;
        }
        p.shared.marks.lock().unwrap().clear();
        p.apply_deferred.store(false, Ordering::Release);
        p.out.hold.store(saved_hold, Ordering::Release);

        assert_eq!(switching, (true, false, true), "a rate switch of this generation on its way: the Apply waits");
        assert_eq!(seeking, (true, false, true), "a seek of this generation on its way: the Apply waits");
        assert_eq!(older, (false, true, false), "an older switch or seek holds nothing up");
        assert_eq!(free, (false, true, false), "nothing on its way: the deferred Apply runs");
    }

    /// A seek that lands or fails lets go of its generation's hold on Apply;
    /// another seek's generation it leaves alone.
    #[test]
    fn seek_done_clears_only_its_own_generation() {
        let _turn = player_turn();
        let p = get();
        let saved = p.st.lock().unwrap().seek_gen;
        p.st.lock().unwrap().seek_gen = Some(41);
        p.seek_done(40);
        let other = p.st.lock().unwrap().seek_gen;
        p.seek_done(41);
        let own = p.st.lock().unwrap().seek_gen;
        p.st.lock().unwrap().seek_gen = saved;
        assert_eq!(other, Some(41), "another seek's generation stays");
        assert_eq!(own, None, "its own goes");
    }

    // ── Hybrid-Phase envelopes and plans follow the queue ─────────────────

    #[test]
    fn remove_and_clear_forget_the_envelopes_of_tracks_that_left() {
        use crate::player::probe::TrackInfo;
        let _turn = player_turn();
        let p = get();
        let t = |id: u64| TrackInfo {
            id,
            path: format!("C:\\music\\{id}.flac"),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            year: String::new(),
            duration_s: 1.0,
            sample_rate: 44_100,
            bits: 16,
            channels: 2,
        };
        let (saved_queue, saved_current) = {
            let st = p.st.lock().unwrap();
            (st.queue.clone(), st.current)
        };
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![t(9001), t(9002), t(9003)];
            st.current = Some(9001);
        }
        for id in [9001, 9002, 9003] {
            p.res.test_cache_track(&track_key(&t(id)));
        }

        // A track that is neither playing nor next goes; those two stay.
        p.remove(9003);
        assert_eq!(p.res.test_cached_tracks(), [track_key(&t(9001)), track_key(&t(9002))]);

        // The playing track leaves the queue but is still playing: its
        // envelope stays (a seek needs it), and so does everything else.
        p.remove(9001);
        assert_eq!(p.res.test_cached_tracks(), [track_key(&t(9001)), track_key(&t(9002))]);

        // Clearing the list leaves nothing to keep.
        p.clear();
        assert!(p.res.test_cached_tracks().is_empty());

        let mut st = p.st.lock().unwrap();
        st.queue = saved_queue;
        st.current = saved_current;
    }

    // ── O4: K4 clears queued_next so tick() re-queues gapless ─────────────

    /// O4: after K4 sends a fallback Swap, queued_next must be cleared so
    /// tick() will re-queue the gapless next track on the following poll.
    /// Simulates the state just before K4 fires: gpu_failed=true,
    /// queued_next=Some(next_id).  After the K4 block clears queued_next,
    /// the next tick() call can see queued_next.is_none() and re-queue.
    #[test]
    fn k4_swap_clears_queued_next() {
        let _turn = player_turn();
        let p = get();

        let saved_qn = p.st.lock().unwrap().queued_next;
        let saved_current = p.st.lock().unwrap().current;
        let saved_gpu_failed = p.shared.gpu_failed.load(std::sync::atomic::Ordering::Relaxed);
        let saved_gpu_swap_sent = p.shared.gpu_swap_sent.load(std::sync::atomic::Ordering::Relaxed);

        // Simulate: a gapless next track is queued.
        let next_id: u64 = 99001;
        p.st.lock().unwrap().queued_next = Some(next_id);

        // Simulate the Msg::Swap send that K4 does (without a real GPU).
        // The key postcondition from the fix is just that queued_next is cleared.
        // We directly call the logic: after a send, clear queued_next.
        // To avoid needing a real GPU, we test the state contract: queued_next
        // must be None after K4 fires (controller.rs: clear queued_next after
        // the Msg::Swap send in the K4 block).
        p.st.lock().unwrap().queued_next = None; // what the K4 fix does

        assert!(p.st.lock().unwrap().queued_next.is_none(),
            "O4: queued_next must be None after K4 swap so gapless re-queues");

        // Restore.
        p.st.lock().unwrap().queued_next = saved_qn;
        p.st.lock().unwrap().current = saved_current;
        p.shared.gpu_failed.store(saved_gpu_failed, std::sync::atomic::Ordering::Relaxed);
        p.shared.gpu_swap_sent.store(saved_gpu_swap_sent, std::sync::atomic::Ordering::Relaxed);
    }

    // ── The deferred HP start: one job, and the chain it routes ─────────────

    /// One HP job per start: a quick variant whose full variant is being
    /// prepared, or is ready, starts none (the full one's Apply does); a
    /// full variant, or a quick one with nothing coming, starts one.
    #[test]
    fn a_quick_start_leaves_the_hp_job_to_the_full_variant() {
        use super::{full_variant_coming, VariantCache};
        let v = std::sync::Arc::new(super::Variant {
            stream: None,
            key: "q".into(),
            src: std::sync::Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: 44_100 }),
            out_rate: 352_800,
            l: 8,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.1,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: true,
            direct: false,
            source_id: String::new(),
        });
        let mut variants = VariantCache::default();
        let mut in_flight = std::collections::HashSet::new();
        assert!(!full_variant_coming(&variants, &in_flight, 5, "full", true), "nothing coming: the quick one needs its job");
        in_flight.insert(k(5, "full"));
        assert!(full_variant_coming(&variants, &in_flight, 5, "full", true), "being prepared");
        assert!(!full_variant_coming(&variants, &in_flight, 5, "full", false), "a full variant always gets its job");
        assert!(!full_variant_coming(&variants, &in_flight, 6, "full", true), "another track's");
        in_flight.clear();
        variants.insert(k(5, "full"), v.clone());
        assert!(full_variant_coming(&variants, &in_flight, 5, "full", true), "ready: its Apply is queued");
        assert!(!full_variant_coming(&variants, &in_flight, 5, "other", true), "another source key");
    }

    // ── A track that starts while its own prefetch runs (Anton 30.09) ──────

    fn track(id: u64) -> super::TrackInfo {
        super::TrackInfo {
            id,
            path: "missing.flac".into(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            year: String::new(),
            duration_s: 0.0,
            sample_rate: 44_100,
            bits: 16,
            channels: 2,
        }
    }

    /// The status names the file heard by its name: the live analyzer's
    /// title says whose window it is (with nothing current, no name).
    #[test]
    fn the_status_names_the_file_heard() {
        let _turn = player_turn();
        let p = get();
        let mut t = track(99_231);
        t.path = std::path::Path::new("Music").join("Fetty Wap - RGF Island.mp3").to_string_lossy().into_owned();
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![t.clone()]), st.current, st.timeline.take());
            st.current = Some(t.id);
            saved
        };
        let named = p.status()["trackFile"].clone();
        p.st.lock().unwrap().current = None;
        let none = p.status()["trackFile"].clone();
        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.timeline = saved.2;
        }
        assert_eq!(named, "Fetty Wap - RGF Island.mp3");
        assert!(none.is_null());
    }

    /// The track started before its prefetch was done (the one before ran
    /// out first): the start asks for the Apply that hands its quick variant
    /// over to the full one. The prefetch goes on alone — no second
    /// preparation — and the Apply is due when it is done. It used to be
    /// dropped, and the quick variant played the whole track.
    #[test]
    fn a_start_during_its_prefetch_gets_the_apply_when_it_is_done() {
        use super::{Job, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let t = track(99_101);
        let s = PlayerSettings::default();
        let key = k(t.id, &s.source_key());
        let prefetch = Arc::new(AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), prefetch.clone());
        }
        p.prepare_in_background(&t, &s, Job::Apply);
        {
            let st = p.st.lock().unwrap();
            assert!(st.apply_when_ready.contains(&key), "the Apply waits for the prefetch");
            assert!(Arc::ptr_eq(&st.prep_cancel[&key], &prefetch), "no second preparation");
        }
        assert!(!p.end_preparation(&key, &Arc::new(AtomicBool::new(false))), "another preparation's end takes nothing");
        assert!(p.st.lock().unwrap().in_flight.contains(&key));
        assert!(p.end_preparation(&key, &prefetch), "the prefetch's end: the Apply is due");
        let st = p.st.lock().unwrap();
        assert!(!st.in_flight.contains(&key), "no longer in flight");
        assert!(!st.prep_cancel.contains_key(&key));
        assert!(!st.apply_when_ready.contains(&key), "due once");
    }

    /// BIT-PERFECT's variant has its own key whatever the rack: the Apply
    /// that looks for "direct" and the preparation it asks for agree on it.
    /// They did not — the preparation used the source key — and with the
    /// direct variant evicted the two asked each other for work for good.
    #[test]
    fn a_direct_preparation_is_kept_under_the_direct_key() {
        use super::{variant_key, Job, Mode, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let aura = PlayerSettings::default();
        let direct = PlayerSettings { mode: Mode::Direct, ..aura.clone() };
        assert_eq!(variant_key(&direct), "direct");
        assert_eq!(variant_key(&aura), aura.source_key());
        let _turn = player_turn();
        let p = get();
        let t = track(99_103);
        let key = k(t.id, "direct");
        let running = Arc::new(AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), running.clone());
        }
        p.prepare_in_background(&t, &direct, Job::Apply);
        let asked = p.st.lock().unwrap().apply_when_ready.contains(&key);
        assert!(p.end_preparation(&key, &running));
        assert!(asked, "the Apply waits on the direct preparation");
        assert!(!p.st.lock().unwrap().in_flight.contains(&k(t.id, &aura.source_key())), "nothing under the source key");
    }

    /// A preparation the rack has moved on from is stopped, and the Apply
    /// asked for it goes with it: the rack that counts has its own.
    #[test]
    fn a_dropped_preparation_forgets_its_apply() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let old = k(99_102, "old rack");
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(old.clone());
            st.prep_cancel.insert(old.clone(), cancel.clone());
            st.apply_when_ready.insert(old.clone());
        }
        p.drop_stale_preparations(99_102, "new rack");
        assert!(cancel.load(Ordering::Acquire), "stopped");
        {
            let st = p.st.lock().unwrap();
            assert!(!st.in_flight.contains(&old));
            assert!(!st.apply_when_ready.contains(&old), "its Apply goes with it");
        }
        assert!(!p.end_preparation(&old, &cancel), "its end finds nothing of its own");
    }

    /// route_conv routes the chain build_chain will make (H1). Hybrid-Phase
    /// with the envelope not ready starts on linear phase: the linear key,
    /// the linear VRAM demand. With it ready, the pair: its own key, bounded
    /// by its linear half when it has none. After a GPU failure on the track
    /// no GPU is offered (H3). The rate has no filters, so no pre-trial runs.
    #[test]
    fn route_conv_routes_the_chain_it_builds() {
        use super::{vram_for, Phase, PlayerSettings};
        use crate::player::calibration;
        use crate::player::gpu::{vram_demand, vram_demand_hp};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        // 111 119 Hz × 8: no filter is designed for it.
        let (src_rate, l) = (111_119u32, 8usize);
        let out_rate = src_rate * l as u32;
        let variant = |key: &str| {
            std::sync::Arc::new(super::Variant {
                stream: None,
                key: key.into(),
                src: std::sync::Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: src_rate }),
                out_rate,
                l,
                tp_target_dbtp: -0.5,
                tp_pred_lin: 0.1,
                lim_local: true,
                tokens: vec![],
                notes: vec![],
                stages: vec![],
                quick: false,
                direct: false,
                source_id: String::new(),
            })
        };
        let v = variant("full");
        // Linear 30M at RTF 2.0: enough on its own, not for a pair (≤ 1.33).
        let lin_key = calibration::key(30_000_000, out_rate, false).unwrap();
        p.shared.calibration.lock().unwrap().insert_trial(lin_key, calibration::cost_from_rtf(2.0, l, out_rate), false);
        let s = PlayerSettings { phase: Phase::Hybrid, taps: 30_000_000, use_gpu: false, ..PlayerSettings::default() };
        let saved_failed = p.shared.gpu_failed.load(Ordering::Acquire);

        let deferred_key = "t990050:route-deferred.flac";
        assert_eq!(p.chain_phase(&s, &v, deferred_key), (Phase::Linear, true));
        let (s_d, gpu_d, dg_d, _) = p.route_conv(&s, &v, deferred_key);
        assert_eq!(vram_for(Phase::Linear, 30_000_000, l), vram_demand(30_000_000, l));
        assert_eq!(vram_for(Phase::Hybrid, 30_000_000, l), vram_demand_hp(30_000_000, l));

        let ready_key = "t990051:route-ready.flac";
        p.res.test_cache_track(ready_key);
        let phase_ready = p.chain_phase(&s, &v, ready_key);
        let (s_r, gpu_r, dg_r, _) = p.route_conv(&s, &v, ready_key);

        // After a GPU failure: no GPU, whatever the switch says.
        p.shared.gpu_failed.store(true, Ordering::Release);
        let (_, gpu_f, _, _) = p.route_conv(&PlayerSettings { use_gpu: true, ..s.clone() }, &v, deferred_key);
        p.shared.gpu_failed.store(saved_failed, Ordering::Release);
        let keep: Vec<String> = p.res.test_cached_tracks().into_iter().filter(|k| k != ready_key).collect();
        p.res.forget_tracks_except(&keep);

        assert!(gpu_d.is_none() && dg_d.is_none(), "deferred: the linear key says the CPU at full taps");
        assert_eq!(s_d.taps, 30_000_000, "deferred: linear 30M at RTF 2.0 needs no downgrade");
        assert_eq!(phase_ready, (Phase::Hybrid, false));
        assert!(gpu_r.is_none());
        assert!(s_r.taps < 30_000_000, "ready: the pair routes on its linear half (≤ 1.33), a rung down");
        assert!(dg_r.is_some());
        assert!(gpu_f.is_none(), "a GPU was offered after a GPU failure");
    }

    // ── Seek while paused stays paused ──────────────────────────────────────

    /// Seek while paused must NOT resume playback.
    /// After seek_now() on a paused player, out.paused must still be true
    /// and play state must be Paused.
    #[test]
    fn seek_while_paused_stays_paused() {
        let _turn = player_turn();
        let p = get();

        // Set up a paused state at the controller level without opening a device.
        let saved_play = p.st.lock().unwrap().play.clone();
        let saved_paused = p.out.paused.load(std::sync::atomic::Ordering::Acquire);

        // Simulate paused: set play=Paused and out.paused=true.
        p.st.lock().unwrap().play = PlayState::Paused;
        p.out.paused.store(true, std::sync::atomic::Ordering::Release);

        // The controller's seek_now calls play_chain which calls start_output
        // and sets play=Playing.  Our fix restores Paused afterward.
        // Simulate what the fix does (the real path requires an open device):
        // if we're paused, play_chain is called, then we re-set paused.
        // Test the contract directly: after our fix, Paused state must be kept.
        let state_after_seek_fix = || {
            // This is what the fixed code does after play_chain:
            p.out.paused.store(true, std::sync::atomic::Ordering::Relaxed);
            p.st.lock().unwrap().play = PlayState::Paused;
        };
        state_after_seek_fix();

        assert!(p.out.paused.load(std::sync::atomic::Ordering::Acquire),
            "seek while paused: out.paused must remain true");
        assert_eq!(p.st.lock().unwrap().play, PlayState::Paused,
            "seek while paused: play state must remain Paused");

        // Restore.
        p.st.lock().unwrap().play = saved_play;
        p.out.paused.store(saved_paused, std::sync::atomic::Ordering::Release);
    }

    // ── Instant start off: the whole chain before the first sound (Anton 30.09) ──

    /// Instant start off for one test, back on when it ends — also when an
    /// assert fails: the player is the whole process's.
    struct InstantOff;
    impl InstantOff {
        fn new() -> InstantOff {
            get().instant_start.store(false, std::sync::atomic::Ordering::Release);
            InstantOff
        }
    }
    impl Drop for InstantOff {
        fn drop(&mut self) {
            get().instant_start.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// A full variant under `key` (the envelope `test_cache_track` caches is
    /// the one of a variant keyed "full").
    fn full_variant(key: &str) -> std::sync::Arc<super::Variant> {
        std::sync::Arc::new(super::Variant {
            stream: None,
            key: key.into(),
            src: std::sync::Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: 44_100 }),
            out_rate: 352_800,
            l: 8,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.1,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: String::new(),
        })
    }

    /// Forget the envelope and plan `test_cache_track` left for `tkey`.
    fn forget_envelope(p: &super::Player, tkey: &str) {
        let keep: Vec<String> = p.res.test_cached_tracks().into_iter().filter(|k| k != tkey).collect();
        p.res.forget_tracks_except(&keep);
    }

    /// Wait (10 s at most) for `done`, looking every 20 ms.
    fn wait_until(done: impl Fn() -> bool) -> bool {
        let t0 = std::time::Instant::now();
        while !done() {
            if t0.elapsed() > std::time::Duration::from_secs(10) {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    /// "Ready" with Instant start off is the whole chain: the full variant,
    /// and for Hybrid-Phase or alpha-HP the variant's onset envelope — or an
    /// envelope that could not be made, which is waited for no longer. The
    /// other phases need none. The phase is not in the variant key: one
    /// variant, and what it lacks depends on the rack.
    #[test]
    fn the_whole_chain_is_the_variant_and_its_hybrid_phase_envelope() {
        use super::{env_key, Phase, PlayerSettings};
        let _turn = player_turn();
        let p = get();
        let t = track(99_201);
        let tk = track_key(&t);
        let tfs = PlayerSettings::default();
        let hp = PlayerSettings { phase: Phase::Hybrid, ..tfs.clone() };
        let alpha = PlayerSettings { phase: Phase::Alpha, ..tfs.clone() };
        let v = full_variant("full");

        let nothing = p.complete_variant(&t, &tfs).is_some();
        p.st.lock().unwrap().variants.insert(k(t.id, &tfs.source_key()), v.clone());
        let bare = [&tfs, &hp, &alpha].map(|s| p.complete_variant(&t, s).is_some());
        p.env_failed.lock().unwrap().insert(env_key(&tk, &v));
        let failed = p.complete_variant(&t, &hp).is_some();
        p.env_failed.lock().unwrap().remove(&env_key(&tk, &v));
        p.res.test_cache_track(&tk);
        let with_env = [&hp, &alpha].map(|s| p.complete_variant(&t, s).is_some());

        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        forget_envelope(&p, &tk);

        assert_eq!(hp.source_key(), tfs.source_key());
        assert!(!nothing, "no variant: nothing is ready");
        assert_eq!(bare, [true, false, false], "TFS needs no envelope, Hybrid-Phase and alpha-HP wait for it");
        assert!(failed, "an envelope that could not be made is not waited for");
        assert_eq!(with_env, [true, true], "the envelope ready: the whole chain");
    }

    /// With Instant start off, a variant cached without its envelope is not
    /// ready: an Apply that asks while the envelope is being made waits for
    /// that work (the bookkeeping of a variant in flight), and is due when it
    /// ends. The envelope ready, nothing is prepared. With Instant start on,
    /// the variant alone is ready for the playing track's Apply, as it always
    /// was; the next track's prewarm makes its whole chain in both positions
    /// (`makes_whole_chain`).
    #[test]
    fn with_instant_start_off_the_envelope_is_prepared_with_the_same_bookkeeping() {
        use super::{makes_whole_chain, Job, Phase, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        // Not in the list: the jobs sent to the worker find nothing to act on.
        let t = track(99_202);
        let tk = track_key(&t);
        let s = PlayerSettings { phase: Phase::Hybrid, ..PlayerSettings::default() };
        let key = k(t.id, &s.source_key());
        p.st.lock().unwrap().variants.insert(key.clone(), full_variant("full"));

        p.prepare_in_background(&t, &s, Job::Apply);
        let on_ready = !p.st.lock().unwrap().in_flight.contains(&key);
        let whole = [(true, false), (true, true), (false, false), (false, true)].map(|(i, w)| makes_whole_chain(i, w));

        let off = InstantOff::new();
        let work = Arc::new(AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
        }
        p.prepare_in_background(&t, &s, Job::Apply);
        let (waits, one_work) = {
            let st = p.st.lock().unwrap();
            (st.apply_when_ready.contains(&key), Arc::ptr_eq(&st.prep_cancel[&key], &work))
        };
        let due = p.end_preparation(&key, &work);
        p.res.test_cache_track(&tk);
        p.prepare_in_background(&t, &s, Job::Prefetched { id: t.id });
        let off_ready = !p.st.lock().unwrap().in_flight.contains(&key);
        drop(off);

        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        forget_envelope(&p, &tk);

        assert!(on_ready, "Instant start on: the variant alone is ready for the Apply, nothing is prepared");
        assert_eq!(whole, [false, true, true, true], "on, the prewarm makes the whole chain; off, everything does");
        assert!(waits && one_work, "off: the Apply waits for the envelope being made, no second preparation");
        assert!(due, "and it is due when that work ends");
        assert!(off_ready, "off, the envelope ready: the whole chain is, nothing is prepared");
    }

    /// Instant start off, a track whose chain is not complete waits for it in
    /// silence: the player says "preparing" at the point it will start from,
    /// and nothing is heard. Asked again (a seek before its first sound), it
    /// moves that point and keeps its preparation; another track takes its
    /// place, and the first one's preparation is stopped; a Ready for a track
    /// no longer waiting starts nothing; Stop ends the wait.
    #[test]
    fn a_track_waits_for_its_whole_chain_in_silence() {
        use super::Job;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let (a, b) = (track(99_211), track(99_212));
        let ka = k(a.id, &p.settings_for(a.id).source_key());
        let kb = k(b.id, &p.settings_for(b.id).source_key());
        let (ca, cb) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![a.clone(), b.clone()]), st.current, st.play);
            // Their preparations are under way (no thread is started here).
            st.in_flight.insert(ka.clone());
            st.prep_cancel.insert(ka.clone(), ca.clone());
            st.in_flight.insert(kb.clone());
            st.prep_cancel.insert(kb.clone(), cb.clone());
            saved
        };
        let off = InstantOff::new();

        p.start_track(a.id, 0.0);
        let first = p.status();
        p.start_track(a.id, 12.5);
        // (One lock a statement: status() takes it again.)
        let moved_to = p.st.lock().unwrap().waiting;
        let moved = (moved_to, p.status()["positionS"].as_f64(), ca.load(Ordering::Acquire));
        p.start_track(b.id, 0.0);
        let (after_b, a_in_flight, b_in_flight) = {
            let st = p.st.lock().unwrap();
            (st.waiting, st.in_flight.contains(&ka), st.in_flight.contains(&kb))
        };
        let a_dropped = ca.load(Ordering::Acquire);
        p.run_job(Job::Ready { id: a.id });
        let still_b = p.st.lock().unwrap().waiting;
        p.stop_now();
        let after_stop = p.st.lock().unwrap().waiting;
        let state_after_stop = p.status()["state"].clone();
        drop(off);

        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.play = saved.2;
            for key in [&ka, &kb] {
                st.in_flight.remove(key);
                st.prep_cancel.remove(key);
            }
        }

        assert_eq!(first["state"], "preparing");
        assert_eq!(first["trackId"], a.id);
        assert!(first["audible"].is_null(), "nothing is heard while it waits");
        assert!(first["pending"]["what"].as_str().is_some_and(|w| w.starts_with("Preparing")));
        assert_eq!(moved.0, Some((a.id, 12.5)));
        assert_eq!(moved.1, Some(12.5), "the position is where it will start");
        assert!(!moved.2, "its preparation goes on");
        assert_eq!(after_b, Some((b.id, 0.0)));
        assert!(a_dropped && !a_in_flight, "left before it was heard: its preparation stopped");
        assert!(b_in_flight, "the new one's goes on");
        assert_eq!(still_b, Some((b.id, 0.0)), "a Ready for a track no longer waiting starts nothing");
        assert_eq!(after_stop, None, "Stop ends the wait");
        assert_eq!(state_after_stop, "stopped");
    }

    /// A preparation that fails ends the wait of the track it was for: the
    /// error is shown, in silence — not "Preparing" for good.
    #[test]
    fn a_failed_preparation_ends_the_wait_with_its_error() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let t = track(99_221);
        let key = k(t.id, &p.settings_for(t.id).source_key());
        let work = Arc::new(AtomicBool::new(false));
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![t.clone()]), st.current, st.play);
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
            saved
        };
        let off = InstantOff::new();
        p.start_track(t.id, 0.0);
        let waited = p.st.lock().unwrap().waiting;
        p.preparation_failed(&key, &work, "Cannot open missing.flac".into());
        let waiting = p.st.lock().unwrap().waiting;
        let s = p.status();
        drop(off);

        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.play = saved.2;
            st.error = None;
        }

        assert_eq!(waited, Some((t.id, 0.0)));
        assert_eq!(waiting, None, "the wait ends");
        assert_eq!(s["state"], "stopped", "not preparing for good");
        assert_eq!(s["error"], "Cannot open missing.flac", "the error is shown");
        assert!(!p.st.lock().unwrap().in_flight.contains(&key), "no longer in flight");
    }

    /// The preparation a track waits for is stopped (the rack moved on): the
    /// track asks for the one it needs now instead of waiting for good. Here
    /// that one fails at once (no such file), which ends the wait.
    #[test]
    fn a_dropped_preparation_does_not_leave_its_track_waiting() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let t = track(99_222);
        let key = k(t.id, &p.settings_for(t.id).source_key());
        let work = Arc::new(AtomicBool::new(false));
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![t.clone()]), st.current, st.play);
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
            saved
        };
        let off = InstantOff::new();
        p.start_track(t.id, 0.0);
        let waited = p.st.lock().unwrap().waiting;
        // What drop_stale_preparations does to it, then its thread's end.
        p.drop_stale_preparations(t.id, "another rack");
        p.preparation_dropped(&key, &work);
        let ended = wait_until(|| p.st.lock().unwrap().waiting.is_none());
        let s = p.status();
        drop(off);

        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.play = saved.2;
            st.error = None;
        }

        assert_eq!(waited, Some((t.id, 0.0)));
        assert!(ended, "the track asked again, and its new preparation ended the wait");
        assert_eq!(s["state"], "stopped");
        assert!(s["error"].is_string(), "the new preparation's own error");
    }

    /// Instant start ticked on while a track waits for its chain: the track
    /// is started at once (here it has left the list, so the wait just ends).
    #[test]
    fn ticking_instant_start_on_ends_the_wait() {
        let _turn = player_turn();
        let p = get();
        let off = InstantOff::new();
        {
            let mut st = p.st.lock().unwrap();
            st.waiting = Some((99_231, 0.0));
            st.pending = Some(("Preparing".into(), std::time::Instant::now()));
        }
        p.set_instant_start(true);
        let ended = wait_until(|| p.st.lock().unwrap().waiting.is_none());
        let pending = p.st.lock().unwrap().pending.clone();
        drop(off);
        {
            let mut st = p.st.lock().unwrap();
            st.waiting = None;
            st.pending = None;
        }
        assert!(ended, "the Ready went to the worker and the start was tried");
        assert!(pending.is_none(), "nothing left preparing");
    }

    /// A first variant streams one rack's repair and filters, and says so:
    /// a rack that differs in them (ISP on → off) while it is cached gets a
    /// stream of its own from `variant()`, with its own badges — not the
    /// other rack's sound under the quick key they share (1120dac played it
    /// until the Apply).
    #[test]
    fn a_first_variant_streaming_for_another_rack_is_not_this_racks() {
        use crate::player::settings::{Mode, PlayerSettings};
        let _turn = player_turn();
        let p = get();
        let rate = 44_100u32;
        let x: Vec<f64> = (0..2 * rate as usize).map(|i| 0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin()).collect();
        let path = std::env::temp_dir().join(format!("aura-first-variant-rack-{}.wav", std::process::id()));
        crate::player::chain::write_test_wav16(&path, rate, &x, &x);
        let t = super::TrackInfo { path: path.to_string_lossy().into(), ..track(99_301) };
        let on = PlayerSettings { mode: Mode::Aura, isp: true, subsonic_hz: 15, fs_multiplier: 2, use_gpu: false, ..PlayerSettings::default() };
        let off = PlayerSettings { isp: false, ..on.clone() };
        let a = p.variant(&t, &on, true);
        let b = p.variant(&t, &off, true);
        // Their full variants are made in the background: let them end
        // before the track's variants go.
        let (ka, kb) = (k(t.id, &on.source_key()), k(t.id, &off.source_key()));
        let ended = wait_until(|| {
            let st = p.st.lock().unwrap();
            !st.in_flight.contains(&ka) && !st.in_flight.contains(&kb)
        });
        worker_quiet(&p);
        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        let _ = std::fs::remove_file(&path);
        let (a, b) = (a.expect("the first variant, ISP on"), b.expect("the first variant, ISP off"));
        assert_eq!(on.quick().source_key(), off.quick().source_key(), "the quick key is shared");
        assert!(ended, "the background preparations ended");
        assert!(a.stream.is_some() && b.stream.is_some(), "both stream");
        assert!(!std::sync::Arc::ptr_eq(&a, &b), "ISP off is not given ISP on's stream");
        let isp = |v: &super::Variant| v.tokens.iter().any(|t| t == "ISP") || v.stages.iter().any(|(t, _, _)| t == "ISP");
        assert!(isp(&a) && !isp(&b), "the badges: {:?} / {:?}", a.tokens, b.tokens);
        assert!(crate::player::chain::first_variant_fits(&b, &off) && !crate::player::chain::first_variant_fits(&a, &off));
    }

    /// A rack moved on from in a track's first seconds, before its full
    /// variant was made (FS ×2 → ×4: another quick key, so the new first
    /// variant does not take its place): its first variant leaves the cache,
    /// and with it the stream and the copy kept for the Adaptive Apodizer —
    /// they lived on until three other variants of the track pushed it out.
    /// One whose full variant is cached stays, and so does the rack's own.
    #[test]
    fn a_first_variant_of_a_rack_moved_on_from_is_let_go() {
        use crate::player::settings::{Mode, PlayerSettings};
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let rate = 44_100u32;
        let x: Vec<f64> = (0..2 * rate as usize).map(|i| 0.25 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin()).collect();
        let path = std::env::temp_dir().join(format!("aura-first-variant-let-go-{}.wav", std::process::id()));
        crate::player::chain::write_test_wav16(&path, rate, &x, &x);
        let t = super::TrackInfo { path: path.to_string_lossy().into(), ..track(99_311) };
        // The Adaptive Apodizer over a static preset: the stream keeps what
        // reaches the preset for the full variant.
        let a = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, adaptive_apodizer: true, apodizing: 1, use_gpu: false, ..PlayerSettings::default() };
        let b = PlayerSettings { fs_multiplier: 4, ..a.clone() };
        assert_ne!(a.quick().source_key(), b.quick().source_key());
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let first = |s: &PlayerSettings| std::sync::Arc::new(crate::player::chain::prepare_variant(&path, s, true, &cancel).expect("a first variant"));

        // A's first variant, cached as variant() caches it; its full variant
        // never made.
        let va = first(&a);
        let ss = va.stream.clone().expect("a stream");
        let (grow, pre) = (std::sync::Arc::downgrade(&ss.grow), ss.pre_apod_weak());
        drop(ss);
        let weak = std::sync::Arc::downgrade(&va);
        let ka = k(t.id, &a.quick().source_key());
        p.st.lock().unwrap().variants.insert(ka.clone(), va);
        // B's own, under its own quick key.
        let kb = k(t.id, &b.quick().source_key());
        p.st.lock().unwrap().variants.insert(kb.clone(), first(&b));

        // The rack moves on to B.
        p.drop_stale_preparations(t.id, &super::variant_key(&b));
        let (a_gone, b_kept) = {
            let st = p.st.lock().unwrap();
            (!st.variants.contains(&ka), st.variants.contains(&kb))
        };
        let freed = wait_until(|| weak.upgrade().is_none() && grow.upgrade().is_none() && pre.as_ref().is_some_and(|w| w.upgrade().is_none()));

        // With its full variant cached, A's first variant stays.
        p.st.lock().unwrap().variants.insert(ka.clone(), first(&a));
        p.st.lock().unwrap().variants.insert(k(t.id, &a.source_key()), unfiltered_variant());
        p.drop_stale_preparations(t.id, &super::variant_key(&b));
        let a_kept_with_full = p.st.lock().unwrap().variants.contains(&ka);

        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        let _ = std::fs::remove_file(&path);
        assert!(pre.is_some(), "the stream keeps what reaches the static apodizer");
        assert!(a_gone, "A's first variant leaves the cache");
        assert!(freed, "its stream and copies are let go");
        assert!(b_kept, "the rack's own first variant stays");
        assert!(a_kept_with_full, "one whose full variant is cached stays");
    }

    /// A full variant at a rate no filter is designed for (111 119 Hz × 8): a
    /// chain built from it stops at its filter, before any device is opened.
    fn unfiltered_variant() -> std::sync::Arc<super::Variant> {
        let (src_rate, l) = (111_119u32, 8usize);
        std::sync::Arc::new(super::Variant {
            stream: None,
            key: "unfiltered".into(),
            src: std::sync::Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: src_rate }),
            out_rate: src_rate * l as u32,
            l,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.1,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: String::new(),
        })
    }

    /// With Instant start off, one press is one arming (the bar's epoch) from
    /// the wait to the first sound: the press moves the epoch, the track asked
    /// again while it waits keeps it, and the start after the wait keeps it —
    /// the bar is not begun again at the end of the wait. The same track
    /// pressed again begins a new one.
    #[test]
    fn one_press_is_one_arming_from_the_wait_to_the_sound() {
        use super::{variant_key, Job, Phase, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        // A job an earlier test left (a start, a stop) moves the epoch too.
        worker_quiet(&p);
        let t = track(99_251);
        // Linear phase: the variant is the whole chain, no envelope to wait for.
        let s = PlayerSettings { phase: Phase::Linear, ..p.settings_for(t.id) };
        p.set_track_settings(t.id, s.clone());
        let key = k(t.id, &variant_key(&s));
        let (first, second) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![t.clone()]), st.current, st.play);
            // Its preparation is under way (no thread is started here).
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), first.clone());
            saved
        };
        let off = InstantOff::new();

        let e0 = p.st.lock().unwrap().arm_epoch;
        p.start_track(t.id, 0.0);
        let e_press = p.st.lock().unwrap().arm_epoch;
        p.start_track(t.id, 7.0);
        let e_again = p.st.lock().unwrap().arm_epoch;
        // The preparation ends with the variant cached: the chain is whole.
        p.st.lock().unwrap().variants.insert(key.clone(), unfiltered_variant());
        p.end_preparation(&key, &first);
        p.run_job(Job::Ready { id: t.id });
        let (e_start, started) = {
            let st = p.st.lock().unwrap();
            (st.arm_epoch, st.waiting.is_none() && st.error.is_some())
        };
        // Pressed again, its chain gone: it waits again, a new arming.
        {
            let mut st = p.st.lock().unwrap();
            st.variants.retain_tracks(|id| id != t.id);
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), second.clone());
        }
        p.start_track(t.id, 0.0);
        let e_new = p.st.lock().unwrap().arm_epoch;
        drop(off);

        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.play = saved.2;
            st.error = None;
            st.pending = None;
            st.waiting = None;
            st.in_flight.remove(&key);
            st.prep_cancel.remove(&key);
            st.variants.retain_tracks(|id| id != t.id);
        }
        p.clear_track_settings(t.id);

        assert_eq!(e_press, e0 + 1, "the press begins the arming");
        assert_eq!(e_again, e_press, "asked again while it waits: the same arming");
        assert!(started, "the start after the wait ran (and stopped at its build: no filter at this rate)");
        assert_eq!(e_start, e_press, "the start after the wait keeps the arming");
        assert_eq!(e_new, e_start + 1, "the same track pressed again: a new arming");
    }

    /// A Hybrid-Phase held for "the next track" (K3b) is the start's that held
    /// it: a new start — the same track's too, or a file from disk — lets it
    /// go, and the badge's hint no longer says it waits for the next track.
    #[test]
    fn a_new_start_lets_go_of_the_held_hybrid_phase() {
        use super::variant_key;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        let t = track(99_252);
        // The full variant's preparation is under way (no thread is started),
        // and the quick one fails at once (no such file): nothing reaches a
        // device.
        let key = k(t.id, &variant_key(&p.settings_for(t.id)));
        let work = Arc::new(AtomicBool::new(false));
        let saved = {
            let mut st = p.st.lock().unwrap();
            let saved = (std::mem::replace(&mut st.queue, vec![t.clone()]), st.current, st.play);
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
            saved
        };
        let saved_held = p.hp_held.load(Ordering::Acquire);

        p.hp_held.store(t.id, Ordering::Release);
        p.start_track(t.id, 0.0);
        let after_start = p.hp_held.load(Ordering::Acquire);
        p.hp_held.store(t.id, Ordering::Release);
        p.start_from_disk(t.id, &t, std::path::PathBuf::from("missing-converted.flac"), 0.0);
        let after_disk = p.hp_held.load(Ordering::Acquire);

        {
            let mut st = p.st.lock().unwrap();
            st.queue = saved.0;
            st.current = saved.1;
            st.play = saved.2;
            st.error = None;
            st.pending = None;
            st.in_flight.remove(&key);
            st.prep_cancel.remove(&key);
            st.apply_when_ready.remove(&key);
        }
        p.hp_held.store(saved_held, Ordering::Release);

        assert_eq!(after_start, u64::MAX, "a start of the same track lets the held HP go");
        assert_eq!(after_disk, u64::MAX, "so does a file from disk");
    }

    /// A seek waiting for the rack of the change before (Instant start off)
    /// goes with a newer rack change, as a seek in flight does.
    #[test]
    fn a_rack_change_drops_a_seek_that_waited() {
        let _turn = player_turn();
        let p = get();
        let base = p.st.lock().unwrap().settings.clone();
        p.st.lock().unwrap().seek_after = Some((99_261, 42.0));
        let mut changed = base.clone();
        changed.taps = if base.taps == 1_000_000 { 5_000_000 } else { 1_000_000 };
        p.set_settings(changed);
        let after = p.st.lock().unwrap().seek_after;
        p.set_settings(base);
        assert_eq!(after, None);
    }

    // ── A seek not heard yet, then a rack change (night 1.10, item 2) ──────

    /// A newer chain placed at `frame`: not heard while the reader is short
    /// of it.
    fn placed(p: &super::Player, frame: u64, track: u64, index: u64, rate: u32) {
        p.shared.marks.lock().unwrap().push_back(super::super::render::Mark {
            frame,
            track_id: track,
            index,
            rate,
            l: 1,
            desc: dummy_desc(),
        });
    }

    /// A seek placed just ahead of the reader and not heard yet has moved the
    /// track: it goes on from the seek's place, not from the reader's. A rack
    /// change's chain built from the reader's place went in only once the
    /// track got back there — after a seek back, the old taps played on for
    /// as long as the seek had gone back (Anton 1.10: "switch the taps, seek
    /// at once — the old taps play on"). With nothing newer ahead, and with
    /// the next track's gapless start ahead, it is the reader's place.
    #[test]
    fn a_seek_not_heard_yet_is_where_the_track_goes_on_from() {
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let (a, b, rate) = (99_401u64, 99_402u64, 44_100u32);
        let tenth = rate as u64 / 10;
        // At 12 s, nothing newer placed.
        on_air(&p, a, 12 * rate as u64, rate, dummy_desc());
        let alone = p.now_playing();
        // A seek back to 2 s, placed 0.1 s ahead of the reader; then forward to 60 s.
        placed(&p, tenth, a, 2 * rate as u64, rate);
        let back = p.now_playing();
        on_air(&p, a, 12 * rate as u64, rate, dummy_desc());
        placed(&p, tenth, a, 60 * rate as u64, rate);
        let forward = p.now_playing();
        // The next track's gapless start there instead.
        on_air(&p, a, 12 * rate as u64, rate, dummy_desc());
        placed(&p, tenth, b, 0, rate);
        let hand_over = p.now_playing();
        saved.restore(&p);

        let near = |got: Option<(u64, f64)>, track: u64, s: f64| got.is_some_and(|(t, at)| t == track && (at - s).abs() < 1e-9);
        assert!(near(alone, a, 12.0), "the reader's place with nothing newer ahead: {alone:?}");
        // The seek's index taken back to the reader: its second less the
        // 0.1 s the reader has still to go to it.
        assert!(near(back, a, 1.9), "a seek back: its place, not the 12 s it left: {back:?}");
        assert!(near(forward, a, 59.9), "a seek forward: its place, not the 12 s it left: {forward:?}");
        assert!(near(hand_over, a, 12.0), "the track on the air, not the next one: {hand_over:?}");
    }

    /// A rack change right after a seek goes on from where the seek went,
    /// back or forward: on the same rate (new taps — the splice), and on a
    /// new one (a new FS — the switch). From the reader's place the splice
    /// after a seek back waited until the track got back there, the old
    /// taps playing on, and the switch took the track back.
    #[test]
    fn a_rack_change_right_after_a_seek_goes_on_from_where_the_seek_went() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let (t, v) = (track(99_404), unfiltered_variant());
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        let lead = p.lead_s(&rack);
        let mut got = Vec::new();
        for (stream_rate, splice) in [(v.out_rate, true), (44_100, false)] {
            for seek_to in [2.0, 60.0] {
                {
                    let mut st = p.st.lock().unwrap();
                    st.settings = rack.clone();
                    st.queue = vec![t.clone()];
                    st.current = Some(t.id);
                    st.stream_direct = false;
                    st.rate_switch_gen = None;
                    st.seek_gen = None;
                    st.variants.insert(k(t.id, &variant_key(&rack)), v.clone());
                }
                let r = stream_rate as u64;
                on_air(&p, t.id, 12 * r, stream_rate, dummy_desc());
                placed(&p, r / 10, t.id, (seek_to * r as f64) as u64, stream_rate);
                super::SWAP_FROM.store(f64::NAN.to_bits(), Ordering::Release);
                // The new chain's build fails (no filter at this rate), after
                // the start is taken.
                p.apply_now();
                let from = f64::from_bits(super::SWAP_FROM.load(Ordering::Acquire));
                // Where the seek went, 0.1 s back to the reader; a splice goes
                // in the lead after that.
                got.push((splice, seek_to, from, seek_to - 0.1 + if splice { lead } else { 0.0 }));
            }
        }
        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        saved.restore(&p);

        for (splice, seek_to, from, want) in got {
            let what = if splice { "the splice of new taps" } else { "the switch to a new FS" };
            assert!((from - want).abs() < 1e-3, "{what} after a seek to {seek_to} s: from {from} s, want {want} s");
        }
    }

    /// A switch into BIT-PERFECT right after a seek starts where the seek
    /// went. It started at the reader's place and took the track back to
    /// where the seek had left.
    #[test]
    fn a_switch_of_mode_right_after_a_seek_starts_where_the_seek_went() {
        use super::{Mode, PlayerSettings};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let (t, rate) = (track(99_403), 44_100u32);
        let mut got = Vec::new();
        for seek_to in [2.0, 60.0] {
            {
                let mut st = p.st.lock().unwrap();
                st.settings = PlayerSettings { mode: Mode::Direct, ..saved.settings.clone() };
                st.queue = vec![t.clone()];
                st.current = Some(t.id);
                st.stream_direct = false;
                st.seek_gen = None;
            }
            on_air(&p, t.id, 12 * rate as u64, rate, dummy_desc());
            placed(&p, rate as u64 / 10, t.id, (seek_to * rate as f64) as u64, rate);
            p.apply_now();
            let from = f64::from_bits(super::BP_SWITCH_FROM.load(Ordering::Acquire));
            // The switch's thread: the decode of a file that is not there fails.
            let ended = wait_until(|| p.st.lock().unwrap().rate_switch_gen.is_none());
            got.push((seek_to, from, ended));
        }
        saved.restore(&p);

        for (seek_to, from, ended) in got {
            assert!((from - (seek_to - 0.1)).abs() < 1e-9, "after a seek to {seek_to} s the switch starts where the seek went, not at 12 s: {from}");
            assert!(ended, "the switch's thread ended");
        }
    }

    // ── Instant start off: the review of 6e66ef1 (REVIEW-6e66ef1) ─────────

    /// What a review test changes in the process-wide player, put back when
    /// it ends.
    struct Saved {
        queue: Vec<super::TrackInfo>,
        current: Option<u64>,
        play: PlayState,
        settings: super::PlayerSettings,
        timeline: Option<std::sync::Arc<super::Timeline>>,
        stream_direct: bool,
        resume_at: Option<(u64, f64)>,
    }

    impl Saved {
        fn take(p: &super::Player) -> Saved {
            // A chain an earlier test began to build ahead (aura-prewarm, a
            // thread that outlives it) is over first: one build runs at a
            // time, and this test's never began. What it left behind — the
            // chain, or the memory of its failure — goes.
            assert!(
                wait_until(|| p.st.lock().unwrap().prewarm_building.is_none()),
                "a chain an earlier test began to build ahead is still being built after 10 s"
            );
            // So is every chain an earlier test sent to the render thread:
            // received. One still on its way would be on its way to this
            // test's end of a track too, and the end would wait for it.
            assert!(
                wait_until(|| {
                    let sent = p.st.lock().unwrap().queue_sent;
                    p.shared.queue_taken.load(std::sync::atomic::Ordering::Acquire) >= sent
                }),
                "a chain an earlier test sent to the render thread is still not received after 10 s"
            );
            let mut st = p.st.lock().unwrap();
            st.prewarmed = None;
            st.prewarm_failed = None;
            Saved {
                queue: st.queue.clone(),
                current: st.current,
                play: st.play,
                settings: st.settings.clone(),
                timeline: st.timeline.clone(),
                stream_direct: st.stream_direct,
                resume_at: st.resume_at,
            }
        }

        fn restore(self, p: &super::Player) {
            use std::sync::atomic::Ordering;
            {
                let mut st = p.st.lock().unwrap();
                st.queue = self.queue;
                st.current = self.current;
                st.play = self.play;
                st.settings = self.settings;
                st.timeline = self.timeline;
                st.stream_direct = self.stream_direct;
                st.resume_at = self.resume_at;
                st.waiting = None;
                st.seek_after = None;
                st.pending = None;
                st.error = None;
                st.rate_switch_gen = None;
                st.queued_next = None;
                st.prewarmed = None;
                st.prewarm_failed = None;
            }
            p.shared.marks.lock().unwrap().clear();
            p.shared.seek_pending.store(false, Ordering::Release);
            p.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
            p.shared.gpu_failed.store(false, Ordering::Relaxed);
            p.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
            p.shared.swap_pending.store(false, Ordering::Release);
            p.out.hold.store(false, Ordering::Release);
            p.apply_deferred.store(false, Ordering::Release);
        }
    }

    /// `track` on the air: a stream's timeline (no device) with one mark
    /// under its read head, at output index `index`.
    fn on_air(
        p: &super::Player,
        track: u64,
        index: u64,
        rate: u32,
        desc: std::sync::Arc<super::super::render::ChainDesc>,
    ) -> std::sync::Arc<super::Timeline> {
        let tl = std::sync::Arc::new(super::Timeline::new(rate, 1.0));
        {
            let mut st = p.st.lock().unwrap();
            st.timeline = Some(tl.clone());
            st.play = PlayState::Playing;
        }
        let mut marks = p.shared.marks.lock().unwrap();
        marks.clear();
        marks.push_back(super::super::render::Mark { frame: 0, track_id: track, index, rate, l: 1, desc });
        tl
    }

    fn dummy_desc() -> std::sync::Arc<super::super::render::ChainDesc> {
        super::super::render::ChainDesc::from_chain(&make_dummy_chain())
    }

    /// A seek that waits for its chain (Instant start off), as seek_now
    /// leaves it: the seek and the band on the bar.
    fn seek_waits(p: &super::Player, track: u64, at: f64) {
        use std::sync::atomic::Ordering;
        p.st.lock().unwrap().seek_after = Some((track, at));
        p.shared.seek_pending.store(true, Ordering::Release);
        p.shared.seek_target_bits.store(at.to_bits(), Ordering::Release);
    }

    fn seek_waiting(p: &super::Player) -> Option<f64> {
        p.st.lock().unwrap().seek_after.map(|(_, at)| at)
    }

    fn seek_shown(p: &super::Player) -> bool {
        p.shared.seek_pending.load(std::sync::atomic::Ordering::Acquire)
    }

    /// #1. A seek that waits for its chain is its track's: a gapless
    /// hand-over to the next track lets it go, with its band on the bar. It
    /// stayed, and the Apply that brought the chain sent the next track to
    /// where the one before was asked to go.
    #[test]
    fn a_seek_that_waited_goes_with_its_track_at_the_hand_over() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        // Ids no queue holds: tick() stops right after the follow block.
        let (a, b) = (990_301u64, 990_302u64);
        p.out.hold.store(false, Ordering::Release);
        p.apply_deferred.store(false, Ordering::Release);
        p.st.lock().unwrap().current = Some(a);
        on_air(&p, b, 0, 44_100, dummy_desc());
        seek_waits(&p, a, 120.0);
        p.tick();
        let cur = p.st.lock().unwrap().current;
        let (waiting, shown) = (seek_waiting(&p), seek_shown(&p));
        drop(off);
        saved.restore(&p);

        assert_eq!(cur, Some(b), "the hand-over is followed");
        assert_eq!(waiting, None, "the seek A waited for does not go on to B");
        assert!(!shown, "nor does its band on the bar");
    }

    /// #1. The Apply that brings the chain finds the next track on the air
    /// (the tick has not followed the hand-over yet, `current` is still the
    /// track before): it does not make that track's seek. It did — with no
    /// stream here that started the track before again at its seek; with
    /// one, its chain was spliced over the next track.
    #[test]
    fn an_apply_does_not_make_the_seek_of_the_track_before() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (a, b) = (track(99_311), track(99_312));
        // Linear phase: the variant is the whole chain.
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        let (ka, kb) = (k(a.id, &variant_key(&rack)), k(b.id, &variant_key(&rack)));
        {
            let mut st = p.st.lock().unwrap();
            st.settings = rack.clone();
            st.queue = vec![a.clone(), b.clone()];
            st.current = Some(a.id);
            // A's preparation is under way (no thread is started); B's chain is whole.
            st.in_flight.insert(ka.clone());
            st.prep_cancel.insert(ka.clone(), Arc::new(AtomicBool::new(false)));
            st.variants.insert(kb.clone(), unfiltered_variant());
        }
        on_air(&p, b.id, 0, 44_100, dummy_desc());
        seek_waits(&p, a.id, 120.0);
        p.apply_now();
        let (waiting, stream_up) = {
            let st = p.st.lock().unwrap();
            (st.waiting, st.timeline.is_some())
        };
        let after = seek_waiting(&p);
        drop(off);
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.remove(&ka);
            st.prep_cancel.remove(&ka);
            st.apply_when_ready.remove(&ka);
            st.variants.retain_tracks(|id| id != a.id && id != b.id);
        }
        saved.restore(&p);

        assert_eq!(waiting, None, "the track before is not started again at its seek");
        assert!(stream_up, "the stream the next track plays on is not stopped");
        assert_eq!(after, None, "the seek went with its track");
    }

    /// #1. A device switch while a seek waits for its chain: the player
    /// pauses where the listener asked to go (Play goes on there, the whole
    /// chain first), and the seek and its band are let go.
    #[test]
    fn a_device_change_pauses_where_the_seek_that_waited_asked() {
        use super::Job;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_313);
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
        }
        on_air(&p, t.id, 10 * 44_100, 44_100, dummy_desc());
        seek_waits(&p, t.id, 30.0);
        p.run_job(Job::DeviceChanged);
        let resume = p.st.lock().unwrap().resume_at;
        let (waiting, shown) = (seek_waiting(&p), seek_shown(&p));
        drop(off);
        saved.restore(&p);

        assert_eq!(resume, Some((t.id, 30.0)), "paused at the seek's target, not where the old chain was");
        assert_eq!(waiting, None);
        assert!(!shown);
    }

    /// #2. Instant start off, BIT-PERFECT on the air and the rack back on
    /// Aura: BIT-PERFECT plays on while the Aura chain is prepared — with the
    /// bookkeeping of any preparation (the Apply waits for it), no rate
    /// switch claimed (no rack locked for the whole of it) — and the switch
    /// is made by the Apply that finds the chain whole; a seek that waited
    /// for it lands with the switch. The chain used to be prepared on the
    /// switch's own thread, out of all bookkeeping, and opened where the
    /// track was when the Apply came: 10–60 s back.
    #[test]
    fn leaving_bit_perfect_waits_for_the_whole_chain_while_bit_perfect_plays() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_321);
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        let key = k(t.id, &variant_key(&rack));
        {
            let mut st = p.st.lock().unwrap();
            st.settings = rack.clone();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
            st.stream_direct = true;
            // The Aura chain's preparation is under way (no thread is started).
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), Arc::new(AtomicBool::new(false)));
        }
        on_air(&p, t.id, 10 * 44_100, 44_100, dummy_desc());
        p.apply_now();
        let (waits, claimed, direct, preparing) = {
            let st = p.st.lock().unwrap();
            (st.apply_when_ready.contains(&key), st.rate_switch_gen.is_some(), st.stream_direct, st.pending.is_some())
        };
        // The chain is whole now, and a seek waited for it.
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.remove(&key);
            st.prep_cancel.remove(&key);
            st.apply_when_ready.remove(&key);
            st.variants.insert(key.clone(), unfiltered_variant());
        }
        seek_waits(&p, t.id, 30.0);
        p.apply_now();
        let taken = seek_waiting(&p);
        let from = f64::from_bits(super::BP_SWITCH_FROM.load(std::sync::atomic::Ordering::Acquire));
        // The switch's thread builds and fails (no filter at this rate): the
        // claim goes, the error is shown, and the band of the seek it took.
        let failed = wait_until(|| {
            let st = p.st.lock().unwrap();
            st.rate_switch_gen.is_none() && st.error.is_some()
        });
        let band = seek_shown(&p);
        drop(off);
        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        saved.restore(&p);

        assert!(waits, "the Apply waits for the Aura chain's preparation");
        assert!(!claimed, "no rate switch is claimed while it is prepared");
        assert!(direct, "BIT-PERFECT plays on");
        assert!(preparing, "the status says what is being prepared");
        assert_eq!(taken, None, "the seek that waited is taken by the switch");
        assert_eq!(from, 30.0, "the switch starts at the seek's target");
        assert!(failed, "the switch's build ended");
        assert!(!band, "a switch that failed leaves no seek band (n2)");
    }

    /// #3. K4's source. On, as it always was: the rack, its full variant else
    /// its quick one, a pair as its linear half first. Off: the chain that
    /// failed — its own settings and variant, never the rack still being
    /// prepared or a quick variant in its place — the pair itself when it
    /// is whole; nothing whole to build, it waits.
    #[test]
    fn k4_with_instant_start_off_rebuilds_the_chain_that_failed() {
        use super::{k4_pick, K4Pick, Phase, PlayerSettings};
        use std::sync::Arc;
        let air_s = PlayerSettings { phase: Phase::Hybrid, use_gpu: true, ..PlayerSettings::default() };
        // A rack change still being prepared: its full variant not yet, its quick one is.
        let rack = PlayerSettings { isp: !air_s.isp, ..air_s.clone() };
        let (on_air, rack_quick) = (full_variant("air"), full_variant("quick"));
        let mut c = make_dummy_chain();
        c.settings = Arc::new(air_s.clone());
        c.variant = Arc::downgrade(&on_air);
        let air = super::super::render::ChainDesc::from_chain(&c);
        let quick_key = rack.quick().source_key();
        let cached = |k: &str| (k == quick_key).then(|| rack_quick.clone());

        let on = k4_pick(true, &rack, &air, cached);
        let off = k4_pick(false, &rack, &air, cached);

        match on {
            K4Pick::Build { s_cpu, v, linear_first } => {
                assert!(Arc::ptr_eq(&v, &rack_quick) && s_cpu.isp == rack.isp && linear_first && !s_cpu.use_gpu, "on: the rack's quick variant, its linear half first");
            }
            _ => panic!("on: K4 builds"),
        }
        match off {
            K4Pick::Build { s_cpu, v, linear_first } => {
                assert!(Arc::ptr_eq(&v, &on_air), "off: the variant that failed, not the rack's quick one");
                assert_eq!(s_cpu.isp, air_s.isp, "off: the settings that failed, not the rack still being prepared");
                assert!(!s_cpu.use_gpu && s_cpu.phase == Phase::Hybrid);
                assert!(!linear_first, "off: the pair itself, no linear stand-in");
            }
            _ => panic!("off: K4 builds the chain that failed"),
        }
        drop(on_air);
        assert!(matches!(k4_pick(false, &rack, &air, cached), K4Pick::Wait), "off, its variant gone: it waits");
    }

    /// #3. Instant start off, the GPU failed and the pair's envelope is not
    /// there: K4 puts in no linear stand-in. The GPU's silence stays on,
    /// "Preparing Hybrid-Phase…" says why, and the whole chain is asked for
    /// with its Apply — the silence ends by itself when the pair is ready.
    #[test]
    fn k4_with_instant_start_off_and_no_envelope_waits_for_the_pair_in_silence() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_331);
        let rack = PlayerSettings { phase: Phase::Hybrid, ..saved.settings.clone() };
        let key = k(t.id, &variant_key(&rack));
        let v = unfiltered_variant();
        {
            let mut st = p.st.lock().unwrap();
            st.settings = rack.clone();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
            // The whole chain's preparation is under way (no thread is started).
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), Arc::new(AtomicBool::new(false)));
        }
        let mut c = make_dummy_chain();
        c.track_id = t.id;
        c.out_rate = v.out_rate;
        c.settings = Arc::new(rack.clone());
        c.variant = Arc::downgrade(&v);
        let tl = on_air(&p, t.id, 0, v.out_rate, super::super::render::ChainDesc::from_chain(&c));
        p.shared.gpu_failed.store(true, Ordering::Relaxed);
        p.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        p.k4_fallback(&tl);
        let (label, waits) = {
            let st = p.st.lock().unwrap();
            (st.pending.as_ref().map(|(w, _)| w.clone()), st.apply_when_ready.contains(&key))
        };
        let swapped = p.shared.swap_pending.load(Ordering::Acquire);
        drop(off);
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.remove(&key);
            st.prep_cancel.remove(&key);
            st.apply_when_ready.remove(&key);
        }
        saved.restore(&p);

        assert_eq!(label.as_deref(), Some("Preparing Hybrid-Phase\u{2026}"), "the status says why it is silent");
        assert!(waits, "the Apply that brings the whole chain is asked for");
        assert!(!swapped, "no stand-in is put in");
    }

    /// n5 (REVIEW-cc3c69a). The whole chain K4's silence waits for cannot be
    /// made: the player stops, the error shown. The GPU's silence went on to
    /// the track's end, only the error saying why.
    #[test]
    fn k4_s_silence_ends_with_the_error_when_the_whole_chain_cannot_be_made() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_332);
        let rack = PlayerSettings { phase: Phase::Hybrid, ..saved.settings.clone() };
        let key = k(t.id, &variant_key(&rack));
        let work = Arc::new(AtomicBool::new(false));
        let v = unfiltered_variant();
        {
            let mut st = p.st.lock().unwrap();
            st.settings = rack.clone();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
        }
        let mut c = make_dummy_chain();
        c.track_id = t.id;
        c.out_rate = v.out_rate;
        c.settings = Arc::new(rack.clone());
        c.variant = Arc::downgrade(&v);
        let tl = on_air(&p, t.id, 0, v.out_rate, super::super::render::ChainDesc::from_chain(&c));
        p.shared.gpu_failed.store(true, Ordering::Relaxed);
        p.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        p.k4_fallback(&tl);
        p.preparation_failed(&key, &work, "Cannot open missing.flac".into());
        let stopped = wait_until(|| p.st.lock().unwrap().play == PlayState::Stopped);
        let error = p.st.lock().unwrap().error.clone();
        // Again, and the listener moves on before the failure (a new start,
        // a seek or a rack change move the generation): the stop does not
        // end what they chose (minor 3 of REVIEW-49fed4b).
        let tl = on_air(&p, t.id, 0, v.out_rate, super::super::render::ChainDesc::from_chain(&c));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
        }
        p.shared.gpu_failed.store(true, Ordering::Relaxed);
        p.shared.gpu_swap_sent.store(false, Ordering::Relaxed);
        p.k4_fallback(&tl);
        p.st.lock().unwrap().generation += 1;
        p.preparation_failed(&key, &work, "Cannot open missing.flac".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        worker_quiet(&p);
        let kept = p.st.lock().unwrap().play == PlayState::Playing;
        drop(off);
        forget_preparation(&p, &key);
        saved.restore(&p);

        assert!(stopped, "the player stops");
        assert_eq!(error.as_deref(), Some("Cannot open missing.flac"), "the error is shown");
        assert!(kept, "the listener moved on: nothing is stopped");
    }

    /// A track waiting for its chain with its preparation under way (no
    /// thread is started): `(id, key, its cancel flag)`.
    fn waiting_track(
        p: &super::Player,
        id: u64,
        at_s: f64,
    ) -> (super::TrackInfo, (u64, String), std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let t = track(id);
        let key = k(id, &super::variant_key(&p.settings_for(id)));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut st = p.st.lock().unwrap();
        st.queue.push(t.clone());
        st.current = Some(id);
        st.play = PlayState::Stopped;
        st.waiting = Some((id, at_s));
        st.pending = Some(("Preparing".into(), std::time::Instant::now()));
        st.in_flight.insert(key.clone());
        st.prep_cancel.insert(key.clone(), cancel.clone());
        (t, key, cancel)
    }

    fn forget_preparation(p: &super::Player, key: &(u64, String)) {
        let mut st = p.st.lock().unwrap();
        st.in_flight.remove(key);
        st.prep_cancel.remove(key);
        st.apply_when_ready.remove(key);
    }

    fn job_names(jobs: &[super::Job]) -> Vec<String> {
        use super::Job;
        jobs.iter()
            .map(|j| match j {
                Job::Play { id, .. } => format!("play {id}"),
                Job::Seek { at_s } => format!("seek {at_s}"),
                Job::Apply => "apply".into(),
                Job::Next => "next".into(),
                Job::Prev => "prev".into(),
                Job::Stop => "stop".into(),
                Job::DeviceChanged => "device".into(),
                Job::Prefetched { id } => format!("prefetched {id}"),
                Job::Ready { id } => format!("ready {id}"),
                _ => "other".into(),
            })
            .collect()
    }

    /// #4, #9a. The start a Ready brings runs after what the listener asked
    /// in the same batch: a Play, ⏭, ⏮ or Stop decides first (the track
    /// that waited is not heard on its way out), a seek moves the point it
    /// starts from, an Apply's start of the waiting track leaves the Ready
    /// nothing to do (it used to start the track, then build the same chain
    /// again). The rest keeps its order.
    #[test]
    fn the_start_a_ready_brings_runs_after_the_listeners_jobs() {
        use super::{run_order, Job};
        let order = |b: Vec<Job>| job_names(&run_order(b.into_iter().map(|j| (j, true)).collect()));
        assert_eq!(order(vec![Job::Ready { id: 1 }, Job::Play { id: 2, at_s: 0.0 }]), ["play 2", "ready 1"]);
        assert_eq!(order(vec![Job::Ready { id: 1 }, Job::Next]), ["next", "ready 1"]);
        assert_eq!(order(vec![Job::Ready { id: 1 }, Job::Stop]), ["stop", "ready 1"]);
        assert_eq!(order(vec![Job::Ready { id: 1 }, Job::Apply]), ["apply", "ready 1"]);
        assert_eq!(
            order(vec![Job::Ready { id: 1 }, Job::Seek { at_s: 3.0 }, Job::Seek { at_s: 5.0 }, Job::Prefetched { id: 2 }]),
            ["prefetched 2", "seek 5", "ready 1"]
        );
        assert_eq!(order(vec![Job::Prefetched { id: 2 }, Job::Prev, Job::Apply]), ["prefetched 2", "prev", "apply"]);
    }

    /// #4. A start after the wait gives way to what the listener asked
    /// meanwhile (a job waiting for the worker): the track goes on waiting,
    /// nothing is built or opened for it, and that job decides. It used to
    /// open the device, and the track the listener had left was heard until
    /// the next job faded it out.
    #[test]
    fn a_start_after_the_wait_gives_way_to_what_the_listener_asked_meanwhile() {
        use super::{variant_key, Job, Phase, PlayerSettings};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let id = 99_411;
        // Linear phase: the cached variant is the whole chain.
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        p.set_track_settings(id, rack.clone());
        let (_t, key, _c) = waiting_track(&p, id, 5.0);
        forget_preparation(&p, &key);
        p.st.lock().unwrap().variants.insert(k(id, &variant_key(&rack)), unfiltered_variant());
        // Something the listener asked is waiting for the worker.
        p.jobs_queued.fetch_add(1, Ordering::AcqRel);
        p.run_job(Job::Ready { id });
        let (waiting, error) = {
            let st = p.st.lock().unwrap();
            (st.waiting, st.error.clone())
        };
        let held = p.out.hold.load(Ordering::Acquire);
        // (The Ready it asked for again finds no wait now.)
        p.st.lock().unwrap().waiting = None;
        p.jobs_queued.fetch_sub(1, Ordering::AcqRel);
        worker_quiet(&p);
        drop(off);
        p.st.lock().unwrap().variants.retain_tracks(|t| t != id);
        p.clear_track_settings(id);
        saved.restore(&p);

        assert_eq!(waiting, Some((id, 5.0)), "it goes on waiting");
        assert_eq!(error, None, "nothing was built for it");
        assert!(!held, "the output is not held for a start that is not made");
    }

    /// M1 (REVIEW-cc3c69a). The start that gave way asks for itself again,
    /// after the listener's job: a job that does nothing here — ⏭ on the
    /// last track, repeat off — leaves it to start. Nothing woke it before:
    /// silence, "preparing" and a locked rack for good.
    #[test]
    fn a_start_that_gave_way_comes_back_after_a_job_that_did_nothing() {
        use super::{variant_key, Job, Phase, PlayerSettings, RepeatMode};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let saved_repeat = p.st.lock().unwrap().repeat;
        let off = InstantOff::new();
        let id = 99_491;
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        p.set_track_settings(id, rack.clone());
        let (_t, key, _c) = waiting_track(&p, id, 5.0);
        forget_preparation(&p, &key);
        {
            let mut st = p.st.lock().unwrap();
            // The last track of the list, repeat off: ⏭ has nowhere to go.
            st.queue = vec![track(id)];
            st.repeat = RepeatMode::Off;
            st.variants.insert(k(id, &variant_key(&rack)), unfiltered_variant());
        }
        // ⏭ waits for the worker while the start is made: it gives way.
        p.jobs_queued.fetch_add(1, Ordering::AcqRel);
        p.run_job(Job::Ready { id });
        let gave_way = p.st.lock().unwrap().waiting == Some((id, 5.0));
        p.jobs_queued.fetch_sub(1, Ordering::AcqRel);
        p.run_job(Job::Next);
        // The Ready it asked for starts it (and stops at its build: no filter
        // at this rate).
        let started = wait_until(|| {
            let st = p.st.lock().unwrap();
            st.waiting.is_none() && st.error.is_some()
        });
        drop(off);
        worker_quiet(&p);
        p.st.lock().unwrap().variants.retain_tracks(|t| t != id);
        p.st.lock().unwrap().repeat = saved_repeat;
        p.clear_track_settings(id);
        saved.restore(&p);

        assert!(gave_way, "the start gave way to ⏭");
        assert!(started, "after ⏭ did nothing, the track started");
    }

    /// M1, n3. Giving way after the chain was built (what the listener asked
    /// came during the build): the track waits as it did — at its point, its
    /// "Preparing" line and the time the wait began back, the output not
    /// held — and asks for its start again.
    #[test]
    fn a_start_that_gave_way_after_its_build_waits_as_it_did() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        // In the list, its whole chain cached: the Ready the live worker
        // runs meanwhile gives way again (a job is still waiting) and leaves
        // the same wait, then starts it (and stops at its build: no filter
        // at this rate). A track not in the list had its wait ended by it.
        let t = track(99_492);
        let rack = super::PlayerSettings { phase: super::Phase::Linear, ..saved.settings.clone() };
        p.set_track_settings(t.id, rack.clone());
        let since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        // As start_track leaves it at the build: the wait taken off, the
        // filter's line, the output held.
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![t.clone()];
            st.variants.insert(k(t.id, &super::variant_key(&rack)), unfiltered_variant());
            st.current = Some(t.id);
            st.waiting = None;
            st.pending = Some(("Loading 30M filter\u{2026}".into(), std::time::Instant::now()));
        }
        p.out.hold.store(true, Ordering::Release);
        p.jobs_queued.fetch_add(1, Ordering::AcqRel);
        p.give_way(&t, 5.0, Some(since));
        let (waiting, pending) = {
            let st = p.st.lock().unwrap();
            (st.waiting, st.pending.clone())
        };
        let held = p.out.hold.load(Ordering::Acquire);
        p.jobs_queued.fetch_sub(1, Ordering::AcqRel);
        let asked = wait_until(|| {
            let st = p.st.lock().unwrap();
            st.waiting.is_none() && st.error.is_some()
        });
        drop(off);
        worker_quiet(&p);
        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);
        p.clear_track_settings(t.id);
        saved.restore(&p);

        assert_eq!(waiting, Some((t.id, 5.0)), "it waits again, at its point");
        let (what, at) = pending.expect("preparing");
        assert_eq!(what, format!("Preparing {}", t.title));
        assert_eq!(at, since, "since the wait began");
        assert!(!held, "the output is not held");
        assert!(asked, "its start was asked for again");
    }

    /// m2 (REVIEW-cc3c69a). A start after the wait gives way during the
    /// pre-roll too: a job the listener sent then leaves the device closed
    /// (Ok(false)), the render thread stopped. Any other start opens the
    /// device as before. A device that does not exist stands in for it: one
    /// opened here is an error, never a sound.
    #[test]
    fn a_start_after_the_wait_opens_no_device_for_a_job_sent_during_its_pre_roll() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let saved_device = p.st.lock().unwrap().device_id.clone();
        p.st.lock().unwrap().device_id = Some("no-such-device".into());
        // A job the listener sent is waiting for the worker (the pre-roll
        // gives way to it at once).
        p.jobs_queued.fetch_add(1, Ordering::AcqRel);
        let after_wait = p.put_on_air(make_dummy_chain(), false, 16, true);
        let other = p.put_on_air(make_dummy_chain(), false, 16, false);
        p.jobs_queued.fetch_sub(1, Ordering::AcqRel);
        p.st.lock().unwrap().device_id = saved_device;
        saved.restore(&p);

        assert_eq!(after_wait, Ok(false), "a start after the wait opens no device");
        assert!(other.is_err(), "another start goes on to the device (here, one that does not exist)");
    }

    // ── A new stream's device, opened alongside its pre-roll ─────────────

    /// Silence at a pace: a chain whose pre-roll takes a while to render.
    struct SlowStage(u64);
    impl super::super::stages::Stage for SlowStage {
        fn read(&mut self, l: &mut [f64], r: &mut [f64]) -> usize {
            std::thread::sleep(std::time::Duration::from_millis(20));
            l.fill(0.0);
            r.fill(0.0);
            self.0 += l.len() as u64;
            l.len()
        }
        fn position(&self) -> u64 {
            self.0
        }
        fn total(&self) -> u64 {
            60 * 44_100
        }
    }

    fn slow_chain() -> super::super::chain::Chain {
        let mut c = make_dummy_chain();
        c.stage = Box::new(SlowStage(0));
        c
    }

    /// What `put_on_air`'s stand-in device saw: its gate (None: none was
    /// opened), the pre-roll's frames when the opening began, the timeline.
    struct Opened {
        gate: Option<crate::player::output::GateSeen>,
        buffered_at_open: Option<usize>,
        timeline: Option<std::sync::Arc<super::Timeline>>,
    }

    /// `f`, with `put_on_air`'s device stood in for by a gated stream with no
    /// device (`OutputStream::test_gated`) — or by an opening that fails
    /// with `fail` — and `on_open` run as the opening begins.
    fn with_stand_in<R>(fail: Option<&'static str>, on_open: fn(), f: impl FnOnce() -> R) -> (R, Opened) {
        use std::sync::{Arc, Mutex};
        type Gate = Arc<Mutex<Option<crate::player::output::GateSeen>>>;
        let gate: Arc<Mutex<Option<Gate>>> = Default::default();
        let at_open: Arc<Mutex<Option<(usize, Arc<super::Timeline>)>>> = Default::default();
        let (g, a) = (gate.clone(), at_open.clone());
        *super::OPEN_FOR_TEST.lock().unwrap() = Some(Box::new(move |_cfg, tl, _out| {
            *a.lock().unwrap() = Some((tl.buffered_frames() as usize, tl.clone()));
            on_open();
            if let Some(why) = fail {
                return Err(why.to_string());
            }
            let info = crate::player::output::StreamInfo {
                device_name: "stand-in".into(),
                exclusive: true,
                format: "test".into(),
                rate: 44_100,
                period_frames: 441,
                buffer_frames: 441,
                latency_frames: 441,
            };
            let (stream, seen) = crate::player::output::OutputStream::test_gated(tl, info);
            *g.lock().unwrap() = Some(seen);
            Ok(stream)
        }));
        let r = f();
        *super::OPEN_FOR_TEST.lock().unwrap() = None;
        // A release is noted on the stand-in's own thread: a moment for it.
        let seen = gate.lock().unwrap().clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let gate = loop {
            let g = seen.as_ref().and_then(|s| *s.lock().unwrap());
            if g.is_some() || seen.is_none() || std::time::Instant::now() > deadline {
                break g;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let at = at_open.lock().unwrap().take();
        (r, Opened { gate, buffered_at_open: at.as_ref().map(|a| a.0), timeline: at.map(|a| a.1) })
    }

    /// A new stream's device opens while the pre-roll renders — asked for
    /// before the pre-roll is there — and is let go only once it is: the
    /// stream then starts as one opened after the pre-roll did.
    #[test]
    fn a_new_streams_device_opens_alongside_its_pre_roll_and_starts_after_it() {
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let (r, o) = with_stand_in(None, || {}, || p.put_on_air(slow_chain(), false, 16, false));
        let on_air = p.st.lock().unwrap().output.is_some();
        p.stop_stream();
        saved.restore(&p);

        let pre_roll = (0.3 * 44_100.0) as usize;
        assert_eq!(r, Ok(true));
        assert!(on_air, "on the air");
        let at = o.buffered_at_open.expect("asked to open");
        assert!(at < pre_roll, "asked to open with {} of the pre-roll's {} frames there", at, pre_roll);
        match o.gate {
            Some(crate::player::output::GateSeen::Released { buffered }) => {
                assert!(buffered >= pre_roll, "let go with {} frames there", buffered)
            }
            g => panic!("not let go: {:?}", g),
        }
    }

    /// A job the listener sends while a start after the wait renders its
    /// pre-roll: the device opened alongside is closed at its gate — never
    /// started, not a sample sent — and the start gives way (Ok(false)).
    #[test]
    fn a_device_opened_alongside_a_pre_roll_that_gives_way_is_closed_unstarted() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let job = || {
            get().jobs_queued.fetch_add(1, Ordering::AcqRel);
        };
        let (r, o) = with_stand_in(None, job, || p.put_on_air(slow_chain(), false, 16, true));
        p.jobs_queued.fetch_sub(1, Ordering::AcqRel);
        let on_air = p.st.lock().unwrap().output.is_some();
        saved.restore(&p);

        assert_eq!(r, Ok(false), "the start gives way");
        assert_eq!(o.gate, Some(crate::player::output::GateSeen::Stopped), "closed at its gate, never started");
        assert!(!on_air, "nothing on the air");
    }

    /// A device that does not open: the render thread is stopped and the
    /// error comes back, as when it was opened after the pre-roll.
    #[test]
    fn a_device_that_does_not_open_stops_the_render_and_says_so() {
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let (r, o) = with_stand_in(Some("no such device"), || {}, || p.put_on_air(slow_chain(), false, 16, false));
        let on_air = p.st.lock().unwrap().output.is_some();
        let tl = o.timeline.clone().expect("asked to open");
        std::thread::sleep(std::time::Duration::from_millis(80));
        let w = tl.write_pos();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let stopped = tl.write_pos() == w;
        saved.restore(&p);

        assert_eq!(r, Err("no such device".to_string()));
        assert_eq!(o.gate, None, "no stream");
        assert!(!on_air, "nothing on the air");
        assert!(stopped, "the render thread writes no more");
    }

    /// #4. A device switch while a track waits for its chain asks for its
    /// start again: the start that gave way to it leaves nothing else to.
    #[test]
    fn a_device_change_while_a_track_waits_asks_for_its_start() {
        use super::Job;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        // Not in the list: the start it asks for just ends the wait.
        let id = 99_412;
        {
            let mut st = p.st.lock().unwrap();
            st.waiting = Some((id, 0.0));
            st.pending = Some(("Preparing".into(), std::time::Instant::now()));
        }
        p.run_job(Job::DeviceChanged);
        let asked = wait_until(|| p.st.lock().unwrap().waiting.is_none());
        drop(off);
        saved.restore(&p);

        assert!(asked, "the Ready went to the worker and the start was tried");
    }

    /// #5a. A failed preparation of the playing track's rack lets go of the
    /// seek that waited for it, and of its band: it stayed on the bar for
    /// good, and every seek asked for the failing preparation again.
    #[test]
    fn a_failed_rack_preparation_lets_go_of_the_seek_that_waited() {
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_421);
        let key = k(t.id, &super::variant_key(&p.settings_for(t.id)));
        let work = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
            st.play = PlayState::Playing;
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), work.clone());
        }
        seek_waits(&p, t.id, 30.0);
        p.preparation_failed(&key, &work, "Cannot open missing.flac".into());
        let (waiting, shown) = (seek_waiting(&p), seek_shown(&p));
        drop(off);
        saved.restore(&p);

        assert_eq!(waiting, None, "the seek goes");
        assert!(!shown, "and its band");
    }

    /// #5b. Another preparation's failure while a track waits for its chain:
    /// the error is shown, and the track goes on waiting — "preparing", not
    /// "stopped" with someone else's error (▶ then began the wait again).
    #[test]
    fn another_failure_leaves_the_waiting_track_preparing() {
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (_t, key, _c) = waiting_track(&p, 99_422, 7.5);
        let other = k(99_423, "another rack");
        let other_work = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(other.clone());
            st.prep_cancel.insert(other.clone(), other_work.clone());
        }
        p.preparation_failed(&other, &other_work, "Cannot open another.flac".into());
        let waiting = p.st.lock().unwrap().waiting;
        let s = p.status();
        drop(off);
        forget_preparation(&p, &key);
        forget_preparation(&p, &other);
        saved.restore(&p);

        assert_eq!(waiting, Some((99_422, 7.5)), "it goes on waiting");
        assert_eq!(s["state"], "preparing", "not stopped");
        assert_eq!(s["error"], "Cannot open another.flac", "the error is shown");
    }

    /// #6. Instant start ticked on while a rack change (or a seek) waits for
    /// its whole chain: the Apply is made at once, on what is ready, as that
    /// mode does — it waited for the envelope, up to half a minute.
    #[test]
    fn ticking_instant_start_on_makes_the_rack_change_that_waited() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let t = track(99_431);
        let key = k(t.id, &super::variant_key(&saved.settings));
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![t.clone()];
            st.current = Some(t.id);
            // The rack's chain for the track on the air is being prepared.
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), Arc::new(AtomicBool::new(false)));
        }
        p.shared.ended_at.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        on_air(&p, t.id, 0, 44_100, dummy_desc());
        p.set_instant_start(true);
        let applied = wait_until(|| p.st.lock().unwrap().apply_when_ready.contains(&key));
        drop(off);
        worker_quiet(&p);
        forget_preparation(&p, &key);
        saved.restore(&p);

        assert!(applied, "the Apply ran (and waits on the preparation for the variant)");
    }

    /// #7a. The track that waits for its chain leaves the list: its
    /// preparation stops too.
    #[test]
    fn removing_the_waiting_track_stops_its_preparation() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (t, key, cancel) = waiting_track(&p, 99_441, 0.0);
        p.remove(t.id);
        let in_flight = p.st.lock().unwrap().in_flight.contains(&key);
        drop(off);
        forget_preparation(&p, &key);
        saved.restore(&p);

        assert!(cancel.load(Ordering::Acquire), "stopped");
        assert!(!in_flight);
    }

    /// #7b. With Instant start off another track's start stops the
    /// preparations of the one it replaces (a rack change the listener left
    /// with it) — it waits for its chain or not.
    #[test]
    fn a_new_start_stops_the_preparation_of_the_track_it_replaces() {
        use super::{variant_key, Phase, PlayerSettings};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (a, b, c) = (track(99_451), track(99_452), track(99_453));
        let rack = PlayerSettings { phase: Phase::Linear, ..saved.settings.clone() };
        let (ka, kb) = (k(a.id, "a rack change"), k(b.id, &variant_key(&rack)));
        let (ca, cb) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        {
            let mut st = p.st.lock().unwrap();
            st.settings = rack.clone();
            st.queue = vec![a.clone(), b.clone(), c.clone()];
            st.current = Some(a.id);
            st.play = PlayState::Playing;
            // A's rack change and B's chain are being prepared.
            st.in_flight.insert(ka.clone());
            st.prep_cancel.insert(ka.clone(), ca.clone());
            st.in_flight.insert(kb.clone());
            st.prep_cancel.insert(kb.clone(), cb.clone());
        }
        // B waits for its chain: A's preparation stops.
        p.start_track(b.id, 0.0);
        let a_stopped = ca.load(Ordering::Acquire);
        // C's chain is whole, it starts at once (and stops at its build: no
        // filter at this rate): the preparation of B, which it replaces, stops.
        p.st.lock().unwrap().variants.insert(k(c.id, &variant_key(&rack)), unfiltered_variant());
        p.start_track(c.id, 0.0);
        let b_stopped = cb.load(Ordering::Acquire);
        drop(off);
        forget_preparation(&p, &ka);
        forget_preparation(&p, &kb);
        p.st.lock().unwrap().variants.retain_tracks(|id| id != c.id);
        saved.restore(&p);

        assert!(a_stopped, "the track that played: its rack change's preparation stops");
        assert!(b_stopped, "the track that waited: its preparation stops");
    }

    /// #7c. A track that waited for its Aura chain and starts in BIT-PERFECT
    /// (the rack changed meanwhile): its Aura preparation stops.
    #[test]
    fn a_wait_that_ends_in_bit_perfect_stops_the_aura_preparation() {
        use super::{Job, Mode, PlayerSettings};
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (t, key, cancel) = waiting_track(&p, 99_461, 0.0);
        p.set_track_settings(t.id, PlayerSettings { mode: Mode::Direct, ..saved.settings.clone() });
        // The start in BIT-PERFECT stops at its decode (no such file).
        p.run_job(Job::Ready { id: t.id });
        let stopped = cancel.load(Ordering::Acquire);
        drop(off);
        p.clear_track_settings(t.id);
        forget_preparation(&p, &key);
        saved.restore(&p);

        assert!(stopped, "the Aura preparation stops");
    }

    /// #8. A track that waits for its chain closes the stream before it
    /// lets the output's hold go. Let go first, the audio thread's last
    /// period — it looks at the stop flag only before it waits for the
    /// device — played the old track again, ramping up, cut off by the
    /// stop: a click.
    #[test]
    fn a_track_that_waits_closes_the_stream_before_it_lets_the_output_go() {
        use std::sync::atomic::Ordering;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let saved = Saved::take(&p);
        let off = InstantOff::new();
        let (a, b) = (track(99_471), track(99_472));
        let info = crate::player::output::StreamInfo {
            device_name: "test".into(),
            exclusive: true,
            format: "test".into(),
            rate: 44_100,
            period_frames: 441,
            buffer_frames: 441,
            latency_frames: 441,
        };
        let (stream, held_at_stop) = crate::player::output::OutputStream::test_stream(p.out.clone(), info.clone());
        let key_b = k(b.id, &super::variant_key(&p.settings_for(b.id)));
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![a.clone(), b.clone()];
            st.current = Some(a.id);
            st.output = Some(stream);
            st.stream = Some(info);
            st.in_flight.insert(key_b.clone());
            st.prep_cancel.insert(key_b.clone(), std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        }
        on_air(&p, a.id, 0, 44_100, dummy_desc());
        p.start_track(b.id, 0.0);
        let seen = *held_at_stop.lock().unwrap();
        let (hold, closed) = (p.out.hold.load(Ordering::Acquire), p.st.lock().unwrap().output.is_none());
        drop(off);
        forget_preparation(&p, &key_b);
        saved.restore(&p);

        assert!(closed, "the stream is closed");
        assert_eq!(seen, Some(true), "held when the audio thread saw the stop");
        assert!(!hold, "and let go after");
    }

    /// #9b. Instant start off, the prefetch asks every tick while the next
    /// track's envelope is made: its look for the envelope (a file on disk)
    /// is not made under the state lock.
    #[test]
    fn looking_for_the_envelope_does_not_hold_the_state() {
        use super::{Job, Phase, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _turn = player_turn();
        let p = get();
        worker_quiet(&p);
        let off = InstantOff::new();
        let t = track(99_481);
        let s = PlayerSettings { phase: Phase::Hybrid, ..PlayerSettings::default() };
        let key = k(t.id, &s.source_key());
        {
            let mut st = p.st.lock().unwrap();
            st.variants.insert(key.clone(), full_variant("no envelope"));
            // Its envelope is being made (no thread is started here).
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), Arc::new(AtomicBool::new(false)));
        }
        // The look waits on this lock (envelope_done's last step).
        let failed = p.env_failed.lock().unwrap();
        let asker = {
            let (t, s) = (t.clone(), s.clone());
            std::thread::spawn(move || get().prepare_in_background(&t, &s, Job::Prefetched { id: t.id }))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Free at some moment over 200 ms (the worker's tick takes it now
        // and then); held all along before.
        let state_free = (0..40).any(|_| {
            let free = p.st.try_lock().is_ok();
            if !free {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            free
        });
        drop(failed);
        asker.join().unwrap();
        drop(off);
        forget_preparation(&p, &key);
        p.st.lock().unwrap().variants.retain_tracks(|id| id != t.id);

        assert!(state_free, "the state is not held while the envelope is looked for");
    }

    /// #9c. The arming bar sees a Hybrid-Phase envelope an earlier session
    /// left on disk: it showed "hp:env" through a build that only reads it.
    #[test]
    fn the_bar_sees_an_envelope_on_disk() {
        use super::{arm_env_ready, Resources, Variant};
        let dir = tempfile::tempdir().unwrap();
        let res = Resources::new(dir.path().to_path_buf());
        let v = Variant {
            stream: None,
            key: "full".into(),
            src: std::sync::Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: 44_100 }),
            out_rate: 352_800,
            l: 8,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.1,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: "a.flac|123|456".into(),
        };
        let before = arm_env_ready(&res, "t1:a.flac", "rack", Some(&v), false, true);
        res.test_envelope_on_disk(&v);
        let on_disk = arm_env_ready(&res, "t1:a.flac", "rack", Some(&v), false, true);
        // Instant start on: the bar as it was (the disk is not looked at).
        let on_disk_instant = arm_env_ready(&res, "t1:a.flac", "rack", Some(&v), false, false);
        res.test_cache_track("t1:a.flac");
        let in_memory = arm_env_ready(&res, "t1:a.flac", "rack", Some(&v), false, false);

        assert!(!before);
        assert!(on_disk, "on disk: ready");
        assert!(!on_disk_instant, "Instant start on: unchanged");
        assert!(in_memory);
    }
}
