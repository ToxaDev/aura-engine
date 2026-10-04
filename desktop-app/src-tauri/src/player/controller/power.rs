//! Power: the calibration pre-trial (K2), the mid-track deficit escalation
//! (K3) and the deferred Hybrid-Phase swap.
//!
//! Called from controller.rs: `power_pretrial` and `route_ladder` from
//! route_conv, before the policy decides (K4 reads `route_ladder` too);
//! `power_tick` from tick(), after the GPU-failure block; `hp_swap` on the
//! thread spawn_hp_job starts. What it keeps between calls lives in the
//! statics below. Every event goes to the log as one `[POWER] {json}` line,
//! never from the render thread.
//!
//! Trials and K3 builds run in their own pool: as wide as the render pool and
//! at its priority, but a separate rayon registry. A job injected into the
//! render pool can be picked up by the render loop's own worker while it
//! waits inside a join, and the loop then stalls for the whole job.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::{effective_settings, get, track_key, variant_key, PlayState, Player, State};
use crate::audio::converter::dsp::filter::{find_precomputed_filter, missing_filter_error, taps_label, TAP_LADDER};
use crate::player::calibration::{self, CalibEntry, CalibrationStore, FlushJob};
use crate::player::chain::{build_chain, Chain, DowngradeInfo, Resources, Variant};
#[cfg(test)]
use crate::player::chain::hp_ready;
use crate::player::convolver::{self, Alignment, Bank};
use crate::player::gpu::ctx::GpuPolyCtx;
use crate::player::policy::{self, DeficitDetector, DeficitSample, DeficitVerdict, TapRtf};
use crate::player::probe::TrackInfo;
use crate::player::render::{self, ChainDesc, Mark, Msg, Shared};
use crate::player::settings::{Mode, Phase, PlayerSettings};
use crate::player::stages;
use crate::player::timeline::Timeline;

/// Origin of `tMs` in the trace and of the detector's clock.
static T0: OnceLock<Instant> = OnceLock::new();
/// Trials and K3 builds: `power_width()` workers at `POWER_PRIORITY`.
static POWER_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
/// One heavy power-pool job at a time (a trial or a K3 build).
static POWER_LOCK: Mutex<()> = Mutex::new(());
/// A trial is timing blocks: its slowdown must not count as a deficit.
static TRIAL_RUNNING: AtomicBool = AtomicBool::new(false);
/// When the last trial ended. A trial on the worker (queue_next, a job) runs
/// between two ticks, so the tick never sees `TRIAL_RUNNING`; the windows it
/// slowed close after it.
static TRIAL_ENDED: Mutex<Option<Instant>> = Mutex::new(None);
/// From a K3 fire until its thread has sent or given up.
static K3_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static DETECTOR: Mutex<DeficitDetector> = Mutex::new(DeficitDetector::new());
/// What the tick observers saw last (audible chain, underruns, K3/K4).
static WATCH: Mutex<Watch> = Mutex::new(Watch::new());
/// The `pool` event is written once.
static POOL_LOGGED: AtomicBool = AtomicBool::new(false);
/// The detector's last verdict, as `verdict_kind`, for the transition events.
static LAST_VERDICT: AtomicU8 = AtomicU8::new(0);

/// THREAD_PRIORITY_HIGHEST, the render pool's (Q10).
const POWER_PRIORITY: i32 = 2;

/// K3 counts no window for this long after a trial: a render window (2 s)
/// and a step.
const TRIAL_SETTLE: Duration = Duration::from_millis(2500);

/// A trial or a K3 build holds the live calibration windows at most this
/// long (should it never come back).
const LIVE_HOLD_MAX: Duration = Duration::from_secs(60);

/// The HP swap waits at most this long for a swap, seek or switch in flight
/// (and after a GPU failure for K4's chain) to land before it gives up.
const HP_WAIT_MAX: Duration = Duration::from_secs(10);

/// Trace state kept between ticks.
struct Watch {
    /// The read side's chain description (its address).
    audible: usize,
    /// The timeline the underrun count belongs to.
    timeline: usize,
    /// Underrun total at the last `underrun` event, and when.
    underrun_logged: u64,
    underrun_at: Option<Instant>,
    /// `gpu_swap_sent` at the last tick (K4's rising edge).
    gpu_swap_sent: bool,
    k4: Option<K4Watch>,
    k3: Option<K3Watch>,
}

impl Watch {
    const fn new() -> Watch {
        Watch {
            audible: 0,
            timeline: 0,
            underrun_logged: 0,
            underrun_at: None,
            gpu_swap_sent: false,
            k4: None,
            k3: None,
        }
    }
}

/// K4 sent its CPU fallback; waiting to hear it.
struct K4Watch {
    before: usize,
    taps_before: Option<String>,
}

/// K3 sent its chain; waiting to hear it, then 2 s more.
struct K3Watch {
    track: u64,
    fire_at: Instant,
    sent_at: Instant,
    before: usize,
    taps: Option<String>,
    gpu: bool,
    /// (heard at, underrun total then, mark index error)
    spliced: Option<(Instant, u64, Option<i64>)>,
}

/// What `k3_fire` hands the K3 thread, taken under one `st` lock.
struct K3Snap {
    /// The generation the detector saw.
    gen: u64,
    id: u64,
    track: TrackInfo,
    /// Effective settings, at the audible taps.
    s: PlayerSettings,
    v: Arc<Variant>,
    used_quick: bool,
    full_key: String,
    /// The rung that fell behind.
    audible: usize,
    /// The chain at the write head, held so its address cannot be reused.
    write_desc: Arc<ChainDesc>,
    read_ptr: usize,
    downgrade: Option<DowngradeInfo>,
    downgrade_gen: u32,
    tl: Arc<Timeline>,
    fire_at: Instant,
}

/// Where K3 goes: the GPU at the same taps, or a CPU rung below.
struct K3Target {
    taps: usize,
    gpu: Option<Arc<GpuPolyCtx>>,
    downgrade: Option<DowngradeInfo>,
    gen: u32,
}

enum K3End {
    Sent,
    /// Already at the lowest rung with filters on disk.
    Stay,
}

/// Where `place_near_write_head` left a chain.
#[derive(Debug)]
struct Placement {
    start: u64,
    iterations: u32,
    place_ms: f64,
    /// The splice will be at the earliest rewritable frame, with a skip on
    /// the render thread (the buffer was thinner than guard + crossfade).
    skip_path: bool,
}

impl Player {
    /// The calibration pre-trial (K2): when the key this chain will be built
    /// for has no fresh measurement, time real blocks of its filter(s) in the
    /// power pool and store the cost, so the lookup that follows in
    /// route_conv routes on it. Runs on the thread that called route_conv;
    /// holds neither `st` nor the calibration lock while measuring.
    ///
    /// `chain` is `(phase, deferred)`, the phase that chain plays
    /// (`chain_phase`, which route_conv asks once for everything it routes).
    /// A Hybrid-Phase or alpha-HP chain whose envelope is not ready yet starts
    /// on linear phase (the deferred start): only the linear bank is timed,
    /// under the linear key. Loading the minimum-phase bank here would put
    /// back the cold load the deferred start avoids; the pair is timed when
    /// the HP swap (hp_swap) calls route_conv again.
    pub(super) fn power_pretrial(&self, s: &PlayerSettings, v: &Arc<Variant>, chain: (Phase, bool)) {
        let t_start = Instant::now();
        trace_pool_once();
        let (phase, hp_deferred) = chain;
        let pair = matches!(phase, Phase::Hybrid | Phase::Alpha);
        let key = calibration::key(s.taps, v.out_rate, pair);
        let skip = |reason: &str| {
            trace("pretrial.skip", json!({ "key": key, "reason": reason, "hpDeferred": hp_deferred }))
        };
        if v.direct || s.mode == Mode::Direct {
            return skip("direct");
        }
        if s.taps < policy::MIN_FALLBACK_TAPS {
            return skip("small");
        }
        let Some(key) = key.clone() else { return };
        if std::env::var_os("AURA_PLAYER_NO_PRETRIAL").as_deref() == Some(std::ffi::OsStr::new("1")) {
            return skip("disabled");
        }
        let src_rate = v.out_rate / (v.l as u32).max(1);

        let (tl, gen0, prep) = {
            let st = self.st.lock().unwrap();
            (st.timeline.clone(), st.generation, !st.in_flight.is_empty())
        };
        // Whether a CPU chain is playing, and its runway: read again after
        // the wait for the power lock, which a K3 build can hold for seconds.
        let load = || {
            let buffered_s = tl
                .as_ref()
                .map(|t| t.buffered_frames() as f64 / t.rate().max(1) as f64)
                .unwrap_or(0.0);
            (self.cpu_rendering(tl.as_deref()), buffered_s)
        };
        let (contended, buffered_s) = load();
        if trial_need(self.calib().entry(&key), contended, src_rate).is_none() {
            return skip("fresh");
        }
        // A contended trial slows the playing chain for a block or two.
        if contended && buffered_s < policy::CONTENDED_TRIAL_MIN_BUFFER_S {
            return skip("thin-buffer");
        }

        // A newer listener action wins: a generation bump, or (on the
        // worker) a job waiting behind this one, or after it in the batch
        // the worker is running (a Stop sent together with the Play).
        let on_worker = std::thread::current().name() == Some("aura-control");
        let stop = || {
            let gen = self.st.lock().map(|st| st.generation).unwrap_or(gen0);
            gen != gen0
                || (on_worker
                    && (self.jobs_queued.load(Ordering::Acquire) > 0 || self.batch_left.load(Ordering::Acquire) > 0))
        };
        let t_wait = Instant::now();
        let Some(power) = lock_power(Duration::from_secs(3), &stop) else {
            return skip(if stop() { "stale" } else { "busy" });
        };
        let wait_ms = ms(t_wait.elapsed());
        // Another thread may have measured it while this one waited, and
        // the playing chain may have drained meanwhile.
        let (contended, buffered_s) = load();
        let Some(reason) = trial_need(self.calib().entry(&key), contended, src_rate) else {
            return skip("fresh");
        };
        if contended && buffered_s < policy::CONTENDED_TRIAL_MIN_BUFFER_S {
            return skip("thin-buffer");
        }

        let t_banks = Instant::now();
        let banks = match trial_banks(&self.res, s.taps, phase, v) {
            Ok(b) => b,
            Err(e) => {
                let first = e.lines().next().unwrap_or("").to_string();
                return skip(&format!("nofilter:{}", first));
            }
        };
        let banks_ms = ms(t_banks.elapsed());

        // Observed contention: the render thread wrote a CPU chain's audio
        // while the first block was timed.
        let w0 = tl.as_ref().map(|t| t.write_pos());
        let more = || match (tl.as_ref(), w0) {
            (Some(t), Some(w0)) => t.write_pos() != w0 && self.write_side_cpu(t),
            _ => false,
        };
        let blocks = if contended { 2 } else { 1 };
        // The playing chain's windows while this runs are not its cost.
        self.calib().hold_live(Instant::now() + LIVE_HOLD_MAX);
        TRIAL_RUNNING.store(true, Ordering::Release);
        let t = power_pool().install(|| convolver::trial_blocks(&banks, &v.src, blocks, &more, &stop));
        *TRIAL_ENDED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        TRIAL_RUNNING.store(false, Ordering::Release);
        self.calib().hold_live(Instant::now());
        drop(banks);

        let block_ms: Vec<Vec<f64>> = t
            .block_ns
            .iter()
            .map(|b| b.iter().map(|&ns| r3(ns as f64 / 1e6)).collect())
            .collect();
        let alloc_ms = t.alloc_ns as f64 / 1e6;
        let drop_ms = t.drop_ns as f64 / 1e6;
        let blocks_ms: f64 = t.block_ns.iter().flatten().map(|&ns| ns as f64 / 1e6).sum();
        let times = json!({
            "wait": r3(wait_ms),
            "banks": r3(banks_ms),
            "alloc": r3(alloc_ms),
            "blocks": block_ms,
            "drop": r3(drop_ms),
            "added": r3(alloc_ms + blocks_ms + drop_ms),
            "total": r3(ms(t_start.elapsed())),
        });
        if t.stopped {
            drop(power);
            trace("pretrial.abort", json!({ "key": key, "reason": "stale", "ms": times }));
            return;
        }

        // The K2 unit: one block of each bank (a pair is both), the fastest
        // of the blocks timed, scaled from the convolver to the whole chain.
        let conv_ms: f64 = t
            .block_ns
            .iter()
            .map(|b| b.iter().copied().min().unwrap_or(0) as f64 / 1e6)
            .sum();
        let cost_ms = conv_ms * calibration::TRIAL_CHAIN_FACTOR;
        let provisional = contended || t.extra;
        let (stored, job) = {
            let mut c = self.calib();
            let stored = cost_ms > 0.0 && trial_need(c.entry(&key), contended, src_rate).is_some();
            if stored {
                c.insert_trial(key.clone(), cost_ms, provisional);
            }
            (stored, c.take_flush(true))
        };
        drop(power);
        if let Some(job) = job {
            self.write_flush(job, true);
        }
        trace("pretrial", json!({
            "key": key,
            "thread": std::thread::current().name().unwrap_or("?"),
            "reason": reason,
            "hpDeferred": hp_deferred,
            "contended": contended,
            "extra": t.extra,
            "provisional": provisional,
            "stored": stored,
            "prepInFlight": prep,
            "bufferedS": r3(buffered_s),
            "j": t.j,
            "threads": t.threads,
            "ms": times,
            "costMs": r3(cost_ms),
            "factor": calibration::TRIAL_CHAIN_FACTOR,
            "rtf": r3(calibration::rtf_from_cost(cost_ms, src_rate)),
        }));
    }

    /// Every tick while not stopped (worker): flush live calibration updates,
    /// trace what is heard, and feed the deficit detector (K3).
    pub(super) fn power_tick(&self, tl: &Arc<Timeline>) {
        trace_pool_once();
        // The render thread's live windows reach the file at most every 10 s;
        // the render thread itself never writes.
        let job = self.shared.calibration.try_lock().ok().and_then(|mut c| c.take_flush(false));
        if let Some(job) = job {
            self.write_flush(job, false);
        }

        let (r, w) = match (
            self.shared.locate(tl.read_pos()),
            self.shared.locate(tl.write_pos().saturating_sub(1)),
        ) {
            (Some(r), Some(w)) => (r, w),
            _ => return,
        };
        let (play, generation, duration_s, rack) = {
            let st = self.st.lock().unwrap();
            (
                st.play,
                st.generation,
                st.queue.iter().find(|t| t.id == r.track_id).map(|t| t.duration_s),
                effective_settings(r.track_id, &st),
            )
        };
        let now = Instant::now();
        let buffered_s = tl.buffered_frames() as f64 / tl.rate().max(1) as f64;
        let target_s = *self.shared.target_ahead_s.lock().unwrap();
        let rtf_milli = self.shared.rtf_milli.load(Ordering::Relaxed);

        self.watch(tl, &r, &rack, buffered_s, target_s, rtf_milli, now);

        // Anything else that owns the moment keeps K3 out of it: a user's
        // Swap may still be in the channel, K4 has the track, the track is
        // ending, or a trial's slowdown would be blamed on the chain.
        let remaining_s = duration_s.map_or(0.0, |d| d - secs(r.index, r.rate));
        let trial_settling = TRIAL_ENDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map_or(false, |t| now.saturating_duration_since(t) < TRIAL_SETTLE);
        let blocked = play != PlayState::Playing
            || self.out.paused.load(Ordering::Relaxed)
            || self.out.hold.load(Ordering::Relaxed)
            || self.shared.seek_pending.load(Ordering::Relaxed)
            || self.shared.swap_pending.load(Ordering::Relaxed)
            || self.shared.gpu_failed.load(Ordering::Relaxed)
            || self.shared.gpu_swap_sent.load(Ordering::Relaxed)
            || self.shared.ended_at.load(Ordering::Relaxed) != u64::MAX
            || w.track_id != r.track_id
            || remaining_s < policy::K3_MIN_REMAINING_S
            || TRIAL_RUNNING.load(Ordering::Acquire)
            || trial_settling
            || K3_IN_FLIGHT.load(Ordering::Acquire);
        let sample = DeficitSample {
            now_s: since_t0(now).as_secs_f64(),
            track_id: r.track_id,
            generation,
            write_chain: desc_id(&w.desc),
            read_chain: desc_id(&r.desc),
            cost_id: cost_id(&w.desc),
            rtf_milli,
            buffered_s,
            target_s,
            cpu_live: cpu_live(&w.desc),
            blocked,
        };
        let verdict = DETECTOR.lock().unwrap_or_else(|e| e.into_inner()).observe(&sample);
        trace_verdict(&verdict, &sample);
        if let DeficitVerdict::Fire { rtf, readings } = verdict {
            self.k3_fire(rtf, readings, &r, &w, tl, &sample);
        }
    }

    /// K3 fired (worker): record the deficit, snapshot what the escalation
    /// needs, and hand it to the `aura-power-k3` thread. Cheap.
    fn k3_fire(&self, rtf: f64, readings: Vec<f64>, r: &Mark, w: &Mark, tl: &Arc<Timeline>, s: &DeficitSample) {
        K3_IN_FLIGHT.store(true, Ordering::Release);
        let fire_at = Instant::now();
        // Until the K3 chain is sent, the old chain's windows measure this
        // deficit and the build beside it, not a new cost.
        self.calib().hold_live(fire_at + LIVE_HOLD_MAX);
        let key = desc_key(&w.desc, w.rate);
        let cost_ms = calibration::cost_from_rtf(rtf.max(1e-3), w.l, w.rate);
        trace("k3.fire", json!({
            "track": r.track_id,
            "indexS": r3(secs(w.index, w.rate)),
            "key": key,
            "rtf": r3(rtf),
            "readings": readings,
            "costMs": r3(cost_ms),
            "bufferedS": r3(s.buffered_s),
            "targetS": r3(s.target_s),
            "underrunTotal": tl.underrun_frames(),
        }));
        // Every later build of this key (this escalation, an Apply, a seek,
        // the next track) now sees the rung falling behind: in memory at
        // once, in the file after the K3 thread is on its way (the write
        // took up to 0.4 s on a slow CPU).
        let flush = key.as_ref().and_then(|k| {
            let mut c = self.calib();
            c.record_deficit(k.clone(), cost_ms);
            c.take_flush(true)
        });
        let snap = match self.k3_snapshot(r, w, tl, fire_at, s.generation) {
            Ok(s) => s,
            Err(reason) => {
                self.calib().hold_live(Instant::now());
                K3_IN_FLIGHT.store(false, Ordering::Release);
                trace("k3.abandoned", json!({ "reason": reason, "track": r.track_id }));
                if let Some(job) = flush {
                    self.write_flush(job, true);
                }
                return;
            }
        };
        let spawned = std::thread::Builder::new()
            .name("aura-power-k3".into())
            .spawn(move || get().k3_run(snap));
        if spawned.is_err() {
            self.calib().hold_live(Instant::now());
            K3_IN_FLIGHT.store(false, Ordering::Release);
            trace("k3.abandoned", json!({ "reason": "spawn", "track": r.track_id }));
        }
        if let Some(job) = flush {
            self.write_flush(job, true);
        }
    }

    /// Everything the K3 thread needs, under one `st` lock: the variant is
    /// the full one when it is cached, else the quick one. `gen` is the
    /// generation the detector saw: a seek or a settings change since then
    /// (the flush above does file I/O) wins.
    fn k3_snapshot(
        &self,
        r: &Mark,
        w: &Mark,
        tl: &Arc<Timeline>,
        fire_at: Instant,
        gen: u64,
    ) -> Result<K3Snap, &'static str> {
        let audible = taps_of(&w.desc).ok_or("no-rung")?;
        let mut st = self.st.lock().unwrap();
        if st.generation != gen {
            return Err("generation");
        }
        let track = st.queue.iter().find(|t| t.id == r.track_id).cloned().ok_or("no-track")?;
        let rack = effective_settings(r.track_id, &st);
        let instant = self.instant();
        let (s, v, used_quick, full_key) =
            k3_source(instant, rack, &w.desc, audible, |k| st.variants.get(&(r.track_id, k.to_string())))?;
        drop(st);
        // Instant start off: only a whole chain is stepped down (its
        // envelope looked for outside the state lock: it can reach the disk).
        if !instant && !self.envelope_done(&track, &s, &v) {
            return Err("incomplete");
        }
        if v.out_rate != w.rate || v.direct {
            return Err("rate");
        }
        Ok(K3Snap {
            gen,
            id: r.track_id,
            track,
            s,
            v,
            used_quick,
            full_key,
            audible,
            write_desc: w.desc.clone(),
            read_ptr: desc_id(&r.desc),
            downgrade: w.desc.downgrade.clone(),
            downgrade_gen: w.desc.downgrade_gen,
            tl: tl.clone(),
            fire_at,
        })
    }

    /// The K3 thread.
    fn k3_run(&self, snap: K3Snap) {
        let end = self.k3_escalate(&snap);
        self.calib().hold_live(Instant::now());
        K3_IN_FLIGHT.store(false, Ordering::Release);
        let since_fire = r3(ms(snap.fire_at.elapsed()));
        match end {
            Ok(K3End::Sent) => trace("k3.sent", json!({ "track": snap.id, "msSinceFire": since_fire })),
            Ok(K3End::Stay) => trace("k3.stay", json!({
                "track": snap.id,
                "reason": "floor",
                "taps": taps_label(snap.audible),
                "msSinceFire": since_fire,
            })),
            Err(reason) => trace("k3.abandoned", json!({ "track": snap.id, "reason": reason, "msSinceFire": since_fire })),
        }
    }

    /// Decide, build in the power pool, place just behind the write head and
    /// send, unless a listener action or the end of the track came first.
    fn k3_escalate(&self, snap: &K3Snap) -> Result<K3End, String> {
        // a. The build-time policy on the deficit just recorded: RTF < 1.2 is
        //    below the margin, so the GPU at the same taps when it is eligible
        //    (switch on, discrete GPU, VRAM, no conversion); else one rung down.
        let (s_model, gpu_ctx, _, _) = self.route_conv(&snap.s, &snap.v, &track_key(&snap.track));
        let target_on = |gpu: Option<Arc<GpuPolyCtx>>| {
            let (taps, downgrade, gen) = k3_plan(
                gpu.is_some(),
                snap.audible,
                &snap.downgrade,
                snap.downgrade_gen,
                s_model.taps,
                || rung_below_existing(snap.audible, snap.v.out_rate / snap.v.l.max(1) as u32, snap.v.out_rate, snap.s.phase),
                || self.shared.gpu_fallback_gen.fetch_add(1, Ordering::AcqRel) + 1,
            )?;
            Some(K3Target { taps, gpu, downgrade, gen })
        };
        let mut refused = false;
        let Some(mut target) = target_on(gpu_ctx) else {
            return Ok(K3End::Stay);
        };
        loop {
            trace("k3.decide", json!({
                "track": snap.id,
                "target": if target.gpu.is_some() { "gpu" } else { "cpu" },
                "from": taps_label(snap.audible),
                "to": taps_label(target.taps),
                "gen": target.gen,
                "variant": if snap.used_quick { "quick" } else { "full" },
                "refusedGpu": refused,
            }));
            // b. Build in the power pool: at HIGHEST it is not starved by the
            //    saturated render threads, and it cannot nest into them. A
            //    seek or a settings change seen before the build wins at
            //    once (its own build must not wait behind this one).
            let t_wait = Instant::now();
            let stale = || self.k3_stale(snap);
            let power = lock_power(Duration::from_secs(3), &|| stale().is_some());
            let wait_ms = ms(t_wait.elapsed());
            if let Some(why) = stale() {
                return Err(why.into());
            }
            let w = self
                .shared
                .locate(snap.tl.write_pos().saturating_sub(1))
                .ok_or_else(|| "moved".to_string())?;
            if w.track_id != snap.id || !Arc::ptr_eq(&w.desc, &snap.write_desc) {
                return Err("moved".into());
            }
            let mut s_k3 = snap.s.clone();
            s_k3.taps = target.taps;
            let t_build = Instant::now();
            let built = power_pool().install(|| {
                build_chain(
                    &self.res,
                    snap.id,
                    &track_key(&snap.track),
                    &snap.v,
                    &s_k3,
                    w.index,
                    target.gpu.clone(),
                    target.downgrade.clone(),
                    target.gen,
                )
            });
            let mut chain = built.map_err(|e| format!("build: {}", e.lines().next().unwrap_or("")))?;
            let built_json = json!({
                "track": snap.id,
                "buildMs": r3(ms(t_build.elapsed())),
                "lockWaitMs": r3(wait_ms),
                "lockHeld": power.is_some(),
                "gpuOn": chain.gpu_on,
                "refusedGpu": target.gpu.is_some() && !chain.gpu_on,
            });
            trace("k3.built", built_json);
            // GpuPolyStream refused (VRAM) and build_chain fell back to the
            // CPU at the same taps, which is the rung that fell behind: once
            // more, as a CPU escalation.
            if target.gpu.is_some() && !chain.gpu_on && !refused {
                drop(chain);
                drop(power);
                refused = true;
                target = match target_on(None) {
                    Some(t) => t,
                    None => return Ok(K3End::Stay),
                };
                continue;
            }

            // c. Just behind the write head: the buffered runway survives.
            let placed = place_near_write_head(&mut chain, &self.shared, &snap.tl);
            let tl = &snap.tl;
            let read = tl.read_pos();
            let write = tl.write_pos();
            let rate = tl.rate().max(1);
            trace("k3.placed", match &placed {
                Ok(p) => json!({
                    "track": snap.id,
                    "iterations": p.iterations,
                    "placeMs": r3(p.place_ms),
                    "startS": r3(secs(p.start, chain.out_rate)),
                    "writeS": r3(self.shared.locate(write.saturating_sub(1)).map_or(0.0, |m| secs(m.index, m.rate))),
                    "readS": r3(self.shared.locate(read).map_or(0.0, |m| secs(m.index, m.rate))),
                    "bufferedS": r3(write.saturating_sub(read) as f64 / rate as f64),
                    "skipPath": p.skip_path,
                }),
                Err(e) => json!({ "track": snap.id, "error": e }),
            });

            // d. Send under the st lock (order st → render_tx), so a seek or
            //    a settings change either comes after it or stops it.
            let gpu = chain.gpu_on;
            let sent = {
                let mut st = self.st.lock().unwrap();
                match self.k3_send_check(&st, snap, &placed) {
                    None => {
                        // Pending from the send on: the render thread reads
                        // the channel only between steps (a step can take
                        // about 2 s at 384 kHz), and nothing else may send
                        // over this Swap meanwhile.
                        self.shared.swap_pending.store(true, Ordering::Release);
                        let _ = self.render_tx.lock().unwrap().send(Msg::Swap {
                            chain,
                            continuous: true,
                            seek: false,
                            side_buf: None,
                            requested: Instant::now(),
                        });
                        // The Swap drops the render thread's gapless next
                        // (CR-5); tick queues it again.
                        st.queued_next = None;
                        Ok(())
                    }
                    Some(why) => Err((why, chain)),
                }
            };
            // e. VRAM hygiene (K8): a CPU chain leaves nothing GPU live.
            if !gpu {
                if let Some(ctx) = GpuPolyCtx::try_build() {
                    ctx.clear_bank_cache();
                }
            }
            drop(power);
            return match sent {
                Ok(()) => {
                    WATCH.lock().unwrap_or_else(|e| e.into_inner()).k3 = Some(K3Watch {
                        track: snap.id,
                        fire_at: snap.fire_at,
                        sent_at: Instant::now(),
                        before: snap.read_ptr,
                        taps: taps_label(target.taps).map(String::from),
                        gpu,
                        spliced: None,
                    });
                    Ok(K3End::Sent)
                }
                Err((why, chain)) => {
                    // An abandoned GPU chain frees its buffers here.
                    drop(chain);
                    Err(why.to_string())
                }
            };
        }
    }

    /// A listener action since the fire: K3 gives way before it builds.
    fn k3_stale(&self, snap: &K3Snap) -> Option<&'static str> {
        let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if st.generation != snap.gen {
            Some("generation")
        } else if st.play != PlayState::Playing {
            Some("not-playing")
        } else {
            None
        }
    }

    /// Why the K3 chain must not be sent now (None = send).
    fn k3_send_check(
        &self,
        st: &State,
        snap: &K3Snap,
        placed: &Result<Placement, &'static str>,
    ) -> Option<&'static str> {
        let w = self.shared.locate(snap.tl.write_pos().saturating_sub(1));
        if st.generation != snap.gen {
            Some("generation")
        } else if st.play != PlayState::Playing {
            Some("not-playing")
        } else if !st.timeline.as_ref().map_or(false, |t| Arc::ptr_eq(t, &snap.tl)) {
            // Stop → Play of the same track: a new session.
            Some("timeline")
        } else if !w.map_or(false, |m| Arc::ptr_eq(&m.desc, &snap.write_desc)) {
            Some("moved")
        } else if self.shared.seek_pending.load(Ordering::Relaxed) {
            Some("seek")
        } else if self.shared.swap_pending.load(Ordering::Relaxed) {
            Some("swap")
        } else if self.out.hold.load(Ordering::Relaxed) {
            Some("hold")
        } else if self.out.paused.load(Ordering::Relaxed) {
            Some("paused")
        } else if self.shared.gpu_failed.load(Ordering::Relaxed) || self.shared.gpu_swap_sent.load(Ordering::Relaxed) {
            Some("gpu-fallback")
        } else if self.shared.ended_at.load(Ordering::Relaxed) != u64::MAX {
            Some("ended")
        } else if let Err(e) = placed {
            Some(*e)
        } else if snap.used_quick && st.variants.contains(&(snap.id, snap.full_key.clone())) {
            // The full variant is ready: the Apply it sends wins.
            Some("full-ready")
        } else {
            None
        }
    }

    /// The deferred Hybrid-Phase swap, on its own thread (`aura-hp-deferred`,
    /// spawn_hp_job): once the envelope is ready, replace the linear stand-in
    /// on the air with the pair. Route first (the pair's pre-trial runs
    /// before any start is fixed), then build at the write head, place off
    /// the render thread and send under `st` after the checks. A quick
    /// variant's job gives way to the full variant's. After a GPU failure on
    /// the track the pair is built on the CPU at K4's rung with K4's chips,
    /// and it never goes back to the GPU.
    pub(super) fn hp_swap(&self, track_id: u64, gen: u64, v: Arc<Variant>, tkey: String, s: PlayerSettings) {
        // The arming bar follows the job (arming.rs): its envelope, the pair
        // sent, or why it sent nothing. Dropped — a panic — it gave up.
        let epoch = self.st.lock().unwrap_or_else(|e| e.into_inner()).arm_epoch;
        let job = crate::player::arming::hp_job_begin(track_id, epoch);
        let t0 = Instant::now();
        let abandon = |reason: &str| {
            trace("hp.abandoned", json!({
                "track": track_id,
                "reason": reason,
                "quick": v.quick,
                "msSinceStart": r3(ms(t0.elapsed())),
            }))
        };
        // Blocking: the onset envelope (0.5-76 s the first time). The pair's
        // minimum-phase bank is built into the cache alongside it rather than
        // after it (0.85-1.1 s for 30M): the chain built next takes it there.
        let env = std::thread::scope(|sc| {
            sc.spawn(|| self.res.warm_pair_bank(&v, &s));
            self.res.envelope(&tkey, &v)
        });
        let env = match env {
            Ok(e) => e,
            Err(e) => {
                crate::aelog!("[PLAYER] HP deferred envelope: {}", e);
                abandon("envelope");
                return job.end(Some("envelope"));
            }
        };
        // Warm the plan cache; build_chain uses the cached entry.
        self.res.plan(&tkey, &v, &env);
        job.envelope_ready();
        let envelope_ms = ms(t0.elapsed());
        if let Some(why) = self.hp_stale(track_id, gen, &v, &s) {
            abandon(why);
            return job.end(Some(why));
        }
        // Once more when the GPU failed meanwhile (the pair then follows
        // K4) or another chain landed first (route again on what plays).
        for attempt in 0..2u32 {
            match self.hp_attempt(track_id, gen, &v, &tkey, &s, attempt, envelope_ms, t0) {
                Ok(()) => return job.end(None),
                Err(why) if attempt == 0 && hp_retry(&why) => {
                    trace("hp.retry", json!({ "track": track_id, "reason": why }));
                }
                Err(why) => {
                    abandon(&why);
                    return job.end(Some(&why));
                }
            }
        }
    }

    /// One pass of hp_swap: route, let the air settle, build at the write
    /// head, place, send. `Err` is why nothing was sent.
    #[allow(clippy::too_many_arguments)]
    fn hp_attempt(
        &self,
        track_id: u64,
        gen: u64,
        v: &Arc<Variant>,
        tkey: &str,
        s: &PlayerSettings,
        attempt: u32,
        envelope_ms: f64,
        t0: Instant,
    ) -> Result<(), String> {
        // a. Route (with the pair's pre-trial), unless the GPU has failed on
        //    this track: the pair then follows K4's rung (d).
        let t_route = Instant::now();
        let routed = if self.shared.gpu_failed.load(Ordering::Acquire) {
            None
        } else {
            Some(self.route_conv(s, v, tkey))
        };
        let route_ms = ms(t_route.elapsed());
        let tl = self.st.lock().unwrap().timeline.clone().ok_or("stopped")?;

        // b. The chain to replace is the one on the air: a swap, a seek or a
        //    track switch in flight lands first; after a GPU failure, K4's
        //    CPU chain does.
        let t_wait = Instant::now();
        loop {
            if let Some(why) = self.hp_stale(track_id, gen, v, s) {
                return Err(why.into());
            }
            let failed_on_air = self.shared.gpu_failed.load(Ordering::Acquire)
                && self
                    .shared
                    .locate(tl.write_pos().saturating_sub(1))
                    .map_or(true, |w| w.desc.gpu_on);
            let busy = self.shared.swap_pending.load(Ordering::Acquire)
                || self.out.hold.load(Ordering::Acquire)
                || self.shared.seek_pending.load(Ordering::Acquire)
                || failed_on_air;
            if !busy {
                break;
            }
            if t_wait.elapsed() > HP_WAIT_MAX {
                return Err("busy".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let wait_ms = ms(t_wait.elapsed());

        // c. It must be this track's deferred stand-in: a K3, K4 or Apply
        //    chain built after the envelope is the pair already.
        let w = self.shared.locate(tl.write_pos().saturating_sub(1)).ok_or("no-mark")?;
        if w.track_id != track_id {
            return Err("track-moved".into());
        }
        if !w.desc.hp_deferred {
            return Err("not-deferred".into());
        }

        // d. What to build. After a GPU failure: K4's rung (chosen on the
        //    pair's ladder) and K4's chips, on the CPU, so nothing blinks.
        let k4 = self.shared.gpu_failed.load(Ordering::Acquire);
        let (s_eff, gpu_ctx, downgrade, dgen) = match routed {
            Some(r) if !k4 => r,
            _ if k4 => {
                let mut s_cpu = s.clone();
                s_cpu.use_gpu = false;
                s_cpu.taps = taps_of(&w.desc).unwrap_or(s.taps);
                (s_cpu, None, w.desc.downgrade.clone(), w.desc.downgrade_gen)
            }
            _ => self.route_conv(s, v, tkey),
        };
        // One automatic step-down per track (K3b): a stand-in already
        // stepped down for power stays as it is rather than step again for
        // the pair; Hybrid-Phase comes with the next track.
        if !k4 && second_step_down(&w.desc, s_eff.taps) {
            self.hp_held.store(track_id, Ordering::Release);
            trace("hp.held", json!({
                "track": track_id,
                "from": w.desc.taps,
                "pairTaps": taps_label(s_eff.taps),
                "downgrade": w.desc.downgrade.as_ref().map(|d| json!({ "from": d.from, "to": d.to, "reason": d.reason })),
            }));
            crate::aelog!(
                "[PLAYER] HP waits for the next track: {} is already one step down, the pair would need {}",
                w.desc.taps.as_deref().unwrap_or("?"),
                taps_label(s_eff.taps).unwrap_or("?")
            );
            return Err("one-step-down".into());
        }
        trace("hp.decide", json!({
            "track": track_id,
            "attempt": attempt,
            "target": if gpu_ctx.is_some() { "gpu" } else { "cpu" },
            "from": w.desc.taps,
            "to": taps_label(s_eff.taps),
            "viaK4": k4,
            "quick": v.quick,
            "downgrade": downgrade.as_ref().map(|d| json!({ "from": d.from, "to": d.to, "reason": d.reason, "gen": dgen })),
            "envelopeMs": r3(envelope_ms),
            "routeMs": r3(route_ms),
            "waitMs": r3(wait_ms),
            "msSinceStart": r3(ms(t0.elapsed())),
        }));

        // e. Build on this thread (the global pool at normal priority: the
        //    stand-in plays meanwhile) ahead of the reader by what the build
        //    and the drawing are expected to take (draw_ahead_s), so its
        //    sound can be drawn over what the stand-in has buffered (f) and
        //    none of it is drawn behind the reader. When the drawing would
        //    not catch the write head (or the reader is on another chain):
        //    at the write head, as before.
        let rate = tl.rate().max(1) as f64;
        let (learned_build, learned_draw) = crate::player::arming::expected_hp_swap(s_eff.taps, s_eff.use_gpu);
        let (build_s, build_from) = match learned_build {
            Some(b) => (b, "learned"),
            None => (crate::player::arming::expected_build_s(s_eff.taps, s_eff.use_gpu, true), "bar"),
        };
        let (rho, rho_from) = match learned_draw {
            Some(r) => (Some(r), "learned"),
            None => {
                let key = format!("{}:{}:HP", taps_label(s_eff.taps).unwrap_or("?"), v.out_rate);
                match self.calib().rtf_for(&key, v.out_rate / (v.l as u32).max(1)) {
                    Some(r) => (Some(r), "calibration"),
                    None => (None, "none"),
                }
            }
        };
        let buffered_s = tl.buffered_frames() as f64 / rate;
        let ahead_s = draw_ahead_s(build_s, buffered_s, rho, tl.guard_frames() as f64 / rate);
        trace("hp.ahead", json!({
            "track": track_id,
            "expectedBuildMs": r3(build_s * 1000.0),
            "buildFrom": build_from,
            "rtf": rho.map(r3),
            "rtfFrom": rho_from,
            "bufferedS": r3(buffered_s),
            "aheadS": ahead_s.map(r3),
        }));
        let at_reader = || {
            let a = ahead_s?;
            match self.shared.locate(tl.read_pos() + (a * rate) as u64) {
                Some(m) if m.track_id == track_id && Arc::ptr_eq(&m.desc, &w.desc) => Some(m.index),
                _ => None,
            }
        };
        let build = |s_b: &PlayerSettings, start: u64, gpu: Option<Arc<GpuPolyCtx>>, d: Option<DowngradeInfo>, g: u32| {
            build_chain(&self.res, track_id, tkey, v, s_b, start, gpu, d, g)
                .map_err(|e| format!("build: {}", e.lines().next().unwrap_or("")))
        };
        let t_build = Instant::now();
        let s_learn = s_eff.clone();
        let mut chain = build(&s_eff, at_reader().unwrap_or(w.index + 1), gpu_ctx.clone(), downgrade, dgen)?;
        let refused = gpu_ctx.is_some() && !chain.gpu_on;
        if refused {
            // GpuPolyStream refused (VRAM) and the build fell back to the CPU
            // at the same taps: a CPU route of its own instead.
            drop(chain);
            let mut s_cpu = s.clone();
            s_cpu.use_gpu = false;
            let (s2, _, d2, g2) = self.route_conv(&s_cpu, v, tkey);
            let w2 = self.shared.locate(tl.write_pos().saturating_sub(1)).ok_or("no-mark")?;
            chain = build(&s2, at_reader().unwrap_or(w2.index + 1), None, d2, g2)?;
        }
        let build_secs = t_build.elapsed().as_secs_f64();
        trace("hp.built", json!({
            "track": track_id,
            "buildMs": r3(build_secs * 1000.0),
            "expectedBuildMs": r3(build_s * 1000.0),
            "gpuOn": chain.gpu_on,
            "refusedGpu": refused,
            "taps": ChainDesc::from_chain(&chain).taps,
        }));

        // f. Draw its sound from just ahead of the reader to just past the
        //    write head, off the render thread: the render thread splices it
        //    in near the reader. Else (or should it decline) the chain goes
        //    in just behind the write head, as before.
        let t_draw = Instant::now();
        let redraw = draw_over(&mut chain, &self.shared, &tl, &w.desc);
        let draw_s = t_draw.elapsed().as_secs_f64();
        // What the next pair plans with (a refused GPU build is not this
        // rack's build). A short drawing (a thin buffer) says little.
        if !refused {
            let drawn_s = redraw.as_ref().map_or(0.0, |rd| rd.l.len() as f64 / chain.out_rate.max(1) as f64);
            let rtf = (drawn_s >= 0.5 && draw_s > 0.0).then(|| drawn_s / draw_s);
            crate::player::arming::learned_hp_swap(s_learn.taps, s_learn.use_gpu, build_secs, rtf);
        }
        trace("hp.drawn", match &redraw {
            Ok(rd) => json!({
                "track": track_id,
                "drawMs": r3(ms(t_draw.elapsed())),
                "rtf": r3(rd.l.len() as f64 / chain.out_rate.max(1) as f64 / draw_s.max(1e-6)),
                "fromS": r3(secs(rd.start, chain.out_rate)),
                "drawnS": r3(secs(rd.l.len() as u64, chain.out_rate)),
                "aheadOfReaderS": r3(secs(
                    self.shared.locate(tl.read_pos()).map_or(0, |m| rd.start.saturating_sub(m.index)),
                    chain.out_rate,
                )),
            }),
            Err(e) => json!({ "track": track_id, "error": e }),
        });
        let redraw = redraw.ok();
        let placed = match &redraw {
            Some(rd) => Ok(Placement {
                start: rd.start,
                iterations: 0,
                place_ms: ms(t_draw.elapsed()),
                skip_path: false,
            }),
            None => place_in(None, &mut chain, &self.shared, &tl),
        };
        let read = tl.read_pos();
        let write = tl.write_pos();
        let rate = tl.rate().max(1);
        trace("hp.placed", match &placed {
            Ok(p) => json!({
                "track": track_id,
                "iterations": p.iterations,
                "placeMs": r3(p.place_ms),
                "startS": r3(secs(p.start, chain.out_rate)),
                "writeS": r3(self.shared.locate(write.saturating_sub(1)).map_or(0.0, |m| secs(m.index, m.rate))),
                "readS": r3(self.shared.locate(read).map_or(0.0, |m| secs(m.index, m.rate))),
                "bufferedS": r3(write.saturating_sub(read) as f64 / rate as f64),
                "skipPath": p.skip_path,
            }),
            Err(e) => json!({ "track": track_id, "error": e }),
        });

        // g. Send under `st` (order st → render_tx, as K3), after the checks.
        let sent = {
            let mut st = self.st.lock().unwrap();
            let o = HpObs {
                generation: st.generation == gen,
                current: st.current == Some(track_id),
                phase: matches!(effective_settings(track_id, &st).phase, Phase::Hybrid | Phase::Alpha),
                timeline: st.timeline.as_ref().map_or(false, |t| Arc::ptr_eq(t, &tl)),
                stand_in: self
                    .shared
                    .locate(tl.write_pos().saturating_sub(1))
                    .map_or(false, |m| Arc::ptr_eq(&m.desc, &w.desc)),
                seek_pending: self.shared.seek_pending.load(Ordering::Acquire),
                swap_pending: self.shared.swap_pending.load(Ordering::Acquire),
                hold: self.out.hold.load(Ordering::Acquire),
                placed: placed.is_ok(),
                gpu_chain: chain.gpu_on,
                gpu_failed: self.shared.gpu_failed.load(Ordering::Acquire),
            };
            match hp_send_check(&o) {
                None => {
                    // Pending from the send on (see k3_escalate).
                    self.shared.swap_pending.store(true, Ordering::Release);
                    let msg = match redraw {
                        Some(redraw) => Msg::Redraw { chain, redraw, requested: Instant::now() },
                        None => Msg::Swap {
                            chain,
                            continuous: true,
                            seek: false,
                            side_buf: None,
                            requested: Instant::now(),
                        },
                    };
                    let _ = self.render_tx.lock().unwrap().send(msg);
                    // The Swap drops the render thread's gapless next
                    // (CR-5); tick queues it again.
                    st.queued_next = None;
                    Ok(())
                }
                Some(why) => Err((why, chain)),
            }
        };
        match sent {
            Ok(()) => {
                trace("hp.sent", json!({
                    "track": track_id,
                    "attempt": attempt,
                    "msSinceStart": r3(ms(t0.elapsed())),
                }));
                Ok(())
            }
            Err((why, chain)) => {
                // An abandoned GPU chain frees its buffers here.
                drop(chain);
                Err(why.to_string())
            }
        }
    }

    /// Why the HP job for `track_id` is stale, or None: a newer generation,
    /// another track, a phase that is not HP any more, or (for a quick
    /// variant's job) the full variant is ready. Its Apply starts a job of
    /// its own: each variant has its own envelope.
    fn hp_stale(&self, track_id: u64, gen: u64, v: &Variant, s: &PlayerSettings) -> Option<&'static str> {
        let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if st.generation != gen {
            Some("generation")
        } else if st.current != Some(track_id) {
            Some("track")
        } else if !matches!(effective_settings(track_id, &st).phase, Phase::Hybrid | Phase::Alpha) {
            Some("phase")
        } else if v.quick && st.variants.contains(&(track_id, s.source_key())) {
            Some("full-ready")
        } else {
            None
        }
    }

    /// The tick's observers: what is heard, underruns, K3's splice, K4's.
    fn watch(
        &self,
        tl: &Arc<Timeline>,
        r: &Mark,
        rack: &PlayerSettings,
        buffered_s: f64,
        target_s: f64,
        rtf_milli: u64,
        now: Instant,
    ) {
        let rptr = desc_id(&r.desc);
        let under = tl.underrun_frames();
        let mut wt = WATCH.lock().unwrap_or_else(|e| e.into_inner());

        // By its own number: a new timeline can take the old one's address.
        let tl_id = tl.id() as usize;
        if wt.timeline != tl_id {
            wt.timeline = tl_id;
            wt.underrun_logged = 0;
            wt.underrun_at = None;
        }
        if under > wt.underrun_logged && wt.underrun_at.map_or(true, |t| now.duration_since(t) >= Duration::from_secs(1)) {
            trace("underrun", json!({
                "add": under - wt.underrun_logged,
                "total": under,
                "readS": r3(secs(r.index, r.rate)),
                "bufS": r3(buffered_s),
                "tgtS": r3(target_s),
                "rtf": rtf_milli as f64 / 1000.0,
                "taps": r.desc.taps,
                "gpu": r.desc.gpu_on,
            }));
            wt.underrun_logged = under;
            wt.underrun_at = Some(now);
        }

        // K4's rising edge: its fallback is on the way.
        let sent = self.shared.gpu_swap_sent.load(Ordering::Relaxed);
        if sent && !wt.gpu_swap_sent {
            wt.k4 = Some(K4Watch { before: rptr, taps_before: r.desc.taps.clone() });
        }
        if !sent {
            wt.k4 = None;
        }
        wt.gpu_swap_sent = sent;

        if wt.audible != rptr {
            wt.audible = rptr;
            let key = desc_key(&r.desc, r.rate);
            let pair = is_pair(&r.desc);
            let (entry, ladder) = {
                let c = self.calib();
                (
                    key.as_deref().and_then(|k| c.entry(k)).map(entry_json),
                    ladder_json(&route_ladder(&c, r.rate, r.l, pair)),
                )
            };
            trace("audible", json!({
                "track": r.track_id,
                "source": r.desc.source,
                "quick": r.desc.quick,
                "taps": r.desc.taps,
                "gpu": r.desc.gpu_on,
                "gpuFailed": self.shared.gpu_failed.load(Ordering::Relaxed),
                "downgrade": downgrade_json(&r.desc),
                "readS": r3(secs(r.index, r.rate)),
                "key": key,
                "entry": entry,
                "ladder": ladder,
                "rtf": rtf_milli as f64 / 1000.0,
                "bufferedS": r3(buffered_s),
                "targetS": r3(target_s),
            }));

            if let Some(k4) = wt.k4.take() {
                if k4.before != rptr {
                    // What K4 should have picked: its own formula
                    // (controller.rs tick) over the calibration now.
                    let pair_rack = matches!(rack.phase, Phase::Hybrid | Phase::Alpha);
                    let raw = route_ladder(&self.calib(), r.rate, r.l, pair_rack);
                    let expect = if raw.is_empty() {
                        10_000_000usize.min(rack.taps)
                    } else {
                        policy::best_cpu_taps(&raw, rack.taps)
                    };
                    trace("k4.observed", json!({
                        "track": r.track_id,
                        "tapsBefore": k4.taps_before,
                        "tapsAfter": r.desc.taps,
                        "downgrade": downgrade_json(&r.desc),
                        "key": key,
                        "entry": entry,
                        "ladder": ladder,
                        "expect": taps_label(expect),
                    }));
                } else {
                    wt.k4 = Some(k4);
                }
            }

            if let Some(k) = wt.k3.as_mut() {
                if k.spliced.is_none() && rptr != k.before && r.track_id == k.track {
                    let (frame, err) = splice_mark(&self.shared, &r.desc);
                    k.spliced = Some((now, under, err));
                    trace("k3.spliced", json!({
                        "track": r.track_id,
                        "spliceFrame": frame,
                        "spliceS": r3(secs(r.index, r.rate)),
                        "msSinceFire": r3(ms(now.duration_since(k.fire_at))),
                        "msSinceSent": r3(ms(now.duration_since(k.sent_at))),
                        "bufferedS": r3(buffered_s),
                        "underrunTotal": under,
                        "taps": r.desc.taps,
                        "gpu": r.desc.gpu_on,
                        "matches": r.desc.taps == k.taps && r.desc.gpu_on == k.gpu,
                    }));
                }
            }
        }

        let done = match wt.k3.as_ref() {
            Some(k) => match k.spliced {
                Some((at, under_then, err)) if now.duration_since(at) >= Duration::from_secs(2) => {
                    trace("k3.after", json!({
                        "track": k.track,
                        "underrunDelta": under.saturating_sub(under_then),
                        "underrunTotal": under,
                        "markIndexError": err,
                    }));
                    true
                }
                // Never heard (a newer action replaced it): stop waiting.
                None => r.track_id != k.track || now.duration_since(k.sent_at) > Duration::from_secs(30),
                _ => false,
            },
            None => false,
        };
        if done {
            wt.k3 = None;
        }
    }

    /// A CPU chain is being rendered for the listener right now (K2's
    /// contended case).
    fn cpu_rendering(&self, tl: Option<&Timeline>) -> bool {
        let Some(tl) = tl else { return false };
        self.shared.alive.load(Ordering::Relaxed)
            && !self.out.paused.load(Ordering::Relaxed)
            && !self.out.hold.load(Ordering::Relaxed)
            && self.write_side_cpu(tl)
    }

    /// The chain at the write head is a live CPU chain.
    fn write_side_cpu(&self, tl: &Timeline) -> bool {
        self.shared
            .locate(tl.write_pos().saturating_sub(1))
            .map_or(false, |m| m.desc.source == "live" && !m.desc.gpu_on)
    }

    /// The phase the chain build_chain makes next for `s`, `v` and the track
    /// key `tkey` will play, and whether that is the deferred Hybrid-Phase
    /// start: HP or alpha-HP plays its pair when its envelope is ready or the
    /// pair can be built now with the plan made as it plays, else linear
    /// phase stands in. The same test build_chain makes (chain::plays_pair,
    /// the same key), so a seek's or a BIT-PERFECT switch's fresh variant is
    /// answered too. Takes no lock of the player's.
    pub(super) fn chain_phase(&self, s: &PlayerSettings, v: &Variant, tkey: &str) -> (Phase, bool) {
        if !matches!(s.phase, Phase::Hybrid | Phase::Alpha) {
            return (s.phase, false);
        }
        built_phase(s.phase, Some(crate::player::chain::plays_pair(&self.res, tkey, v, s)))
    }

    fn calib(&self) -> MutexGuard<'_, CalibrationStore> {
        self.shared.calibration.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write a taken flush outside the store's lock; a failed write leaves
    /// the store dirty for the next one.
    fn write_flush(&self, job: FlushJob, forced: bool) {
        let entries = job.entries;
        let t = Instant::now();
        let res = job.write();
        if res.is_err() {
            self.calib().mark_dirty();
        }
        trace("calib.flush", json!({
            "entries": entries,
            "bytes": res.as_ref().ok(),
            "ms": r3(ms(t.elapsed())),
            "forced": forced,
            "ok": res.is_ok(),
            "error": res.as_ref().err().map(|e| e.to_string()),
        }));
    }
}

/// Why a trial is needed for an entry, or `None`: missing, or provisional
/// with an RTF that steers away from the CPU (re-measured once uncontended).
fn trial_need(e: Option<&CalibEntry>, contended: bool, src_rate: u32) -> Option<&'static str> {
    match e {
        None => Some("missing"),
        Some(e)
            if e.provisional
                && !contended
                && calibration::rtf_from_cost(e.block_cost_ms, src_rate) < policy::CPU_MARGIN_THRESHOLD =>
        {
            Some("recheck")
        }
        Some(_) => None,
    }
}

/// `(phase, deferred)`: HP or alpha-HP with the envelope known not to be
/// ready (`Some(false)`) plays linear phase; unknown (`None`) keeps the pair.
fn built_phase(phase: Phase, ready: Option<bool>) -> (Phase, bool) {
    match phase {
        Phase::Hybrid | Phase::Alpha if ready == Some(false) => (Phase::Linear, true),
        p => (p, false),
    }
}

fn power_width() -> usize {
    calibration::cpu_width().max(2)
}

fn power_pool() -> &'static rayon::ThreadPool {
    POWER_POOL.get_or_init(|| build_pool(power_width(), "aura-power", POWER_PRIORITY))
}

/// A pool of `width` workers named `{name}-{i}` at thread priority `priority`.
fn build_pool(width: usize, name: &'static str, priority: i32) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(width)
        .thread_name(move |i| format!("{}-{}", name, i))
        .start_handler(move |_| set_priority(priority))
        .build()
        .expect("power thread pool")
}

fn set_priority(priority: i32) {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
        SetThreadPriority(GetCurrentThread(), priority);
    }
    #[cfg(not(windows))]
    let _ = priority;
}

/// POWER_LOCK, waiting up to `wait`; `None` at the timeout, or as soon as
/// `cancel()` says the job is stale (a newer listener action).
fn lock_power(wait: Duration, cancel: &dyn Fn() -> bool) -> Option<MutexGuard<'static, ()>> {
    let t0 = Instant::now();
    loop {
        match POWER_LOCK.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        if t0.elapsed() >= wait || cancel() {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The banks build_chain will load for `taps` at `phase` (the phase the chain
/// plays: `chain_phase`, so linear for a deferred HP start): the same `find`
/// and `res.bank` arguments as build_chain's per-phase selection in chain.rs
/// ("The filter(s)"; its deferred branch loads the linear bank exactly as
/// Linear does), so build_chain then finds them in the cache and the load is
/// moved into the trial, not added. Keep the two in step: a difference costs
/// an extra load, not a wrong measurement.
fn trial_banks(res: &Resources, taps: usize, phase: Phase, v: &Variant) -> Result<Vec<Arc<Bank>>, String> {
    let rate = v.out_rate;
    let src = rate / v.l.max(1) as u32;
    let find = |phase: &str| {
        find_precomputed_filter(taps, src, rate, phase).ok_or_else(|| missing_filter_error(taps, src, rate, phase))
    };
    Ok(match phase {
        Phase::Linear => vec![res.bank(&find("linear_phase")?, v.l, Alignment::Linear, rate)?],
        Phase::Minimum => vec![res.bank(&find("minimum_phase")?, v.l, Alignment::None, rate)?],
        Phase::Tfs => {
            let cancel = AtomicBool::new(false);
            let p = crate::audio::converter::dsp::lab::tfs::resolve_or_derive(taps, src, rate, &cancel)?;
            let ahead = crate::audio::converter::dsp::lab::tfs::look_ahead(taps, rate);
            vec![res.bank(&p.to_string_lossy(), v.l, Alignment::LookAhead(ahead), rate)?]
        }
        Phase::Hybrid | Phase::Alpha => {
            let (lin, min) = res.pair_banks(&find("linear_phase")?, Alignment::Linear, &find("minimum_phase")?, v.l, rate)?;
            vec![lin, min]
        }
    })
}

/// Advance `chain` to just behind the write head (one crossfade and 50 ms
/// back), so the splice keeps the whole buffered runway and plan_swap finds
/// it without a Wait or a skip on the render thread. `chain.start` is set
/// to where it ends up: the render thread's mark after the swap reads it.
fn place_near_write_head(chain: &mut Chain, shared: &Shared, tl: &Timeline) -> Result<Placement, &'static str> {
    place_in(Some(power_pool()), chain, shared, tl)
}

/// The placement, skipping in `pool` (`None`: on the calling thread).
///
/// With less buffered than the guard, a crossfade and `back`, the splice is
/// at the earliest rewritable frame whatever a chase does, and the chase
/// races the old chain on the same cores (K3 at 4 E-cores: 5.6 s to gain
/// 0.1 s). Then the chain stays where it is: the render thread skips it
/// forward to the old chain's index once it has dropped the old chain
/// (plan_swap's append branch), at full speed.
fn place_in(pool: Option<&rayon::ThreadPool>, chain: &mut Chain, shared: &Shared, tl: &Timeline) -> Result<Placement, &'static str> {
    let t0 = Instant::now();
    let rate = chain.out_rate as f64;
    let xf = (render::XFADE_MS / 1000.0 * rate) as u64;
    let back = xf + (0.05 * rate) as u64;
    let thin = tl.buffered_frames() < tl.guard_frames() + xf + back;
    let mut iterations = 0;
    // The write head moves on while the chain skips.
    for _ in 0..3 {
        let w = shared.locate(tl.write_pos().saturating_sub(1)).ok_or("no-mark")?;
        if w.track_id != chain.track_id {
            return Err("track-moved");
        }
        if w.rate != chain.out_rate {
            return Err("rate");
        }
        let p = w.index.saturating_sub(back);
        if thin || chain.stage.position() >= p {
            break;
        }
        iterations += 1;
        match pool {
            Some(pool) => pool.install(|| stages::skip_to(chain.stage.as_mut(), p)),
            None => stages::skip_to(chain.stage.as_mut(), p),
        }
    }
    chain.start = chain.stage.position();
    let skip_path = thin
        || shared
            .locate(tl.earliest_rewrite())
            .map_or(false, |m| m.track_id == chain.track_id && chain.start <= m.index);
    Ok(Placement { start: chain.start, iterations, place_ms: ms(t0.elapsed()), skip_path })
}

/// How far past the write head `draw_over` draws: the stand-in goes on
/// rendering until the render thread reads the swap, and what it writes
/// meanwhile is still covered.
const DRAW_PAST_WRITE_S: f64 = 0.1;
/// How far ahead of the earliest rewritable frame the drawing starts: the
/// reader moves on while the chain catches up to it.
const DRAW_LEAD_S: f64 = 0.05;

/// Draw `chain`'s sound over what `over` has buffered: catch up to just
/// ahead of the earliest rewritable frame, then render (keeping every
/// sample) to just past the write head, following it as it moves. The
/// chain is left at the drawing's end. Run on the calling thread, as the
/// placement's chase. Err: the timeline is not `over`'s any more.
fn draw_over(chain: &mut Chain, shared: &Shared, tl: &Timeline, over: &Arc<ChainDesc>) -> Result<render::Redraw, &'static str> {
    let rate = chain.out_rate as f64;
    let on_over = |f: u64| -> Result<Mark, &'static str> {
        let m = shared.locate(f).ok_or("no-mark")?;
        if m.track_id != chain.track_id || m.rate != chain.out_rate || !Arc::ptr_eq(&m.desc, over) {
            return Err("moved");
        }
        Ok(m)
    };
    if chain.direct {
        return Err("direct");
    }
    // A chain built ahead of that draws from where it stands, up to the
    // write head.
    let from = on_over(tl.earliest_rewrite() + (DRAW_LEAD_S * rate) as u64)?;
    if chain.stage.position() >= on_over(tl.write_pos().saturating_sub(1))?.index {
        return Err("ahead");
    }
    if chain.stage.position() < from.index {
        stages::skip_to(chain.stage.as_mut(), from.index);
    }
    let start = chain.stage.position();
    let past = (DRAW_PAST_WRITE_S * rate) as u64;
    let (mut l, mut r) = (Vec::new(), Vec::new());
    let mut bl = vec![0.0; 16384];
    let mut br = vec![0.0; 16384];
    // The write head moves on while the chain draws: follow it a few times.
    for _ in 0..4 {
        let target = on_over(tl.write_pos().saturating_sub(1))?.index + 1 + past;
        let mut pos = chain.stage.position();
        if pos >= target {
            break;
        }
        while pos < target {
            let n = ((target - pos) as usize).min(bl.len());
            let got = chain.stage.read(&mut bl[..n], &mut br[..n]).min(n);
            l.extend_from_slice(&bl[..got]);
            r.extend_from_slice(&br[..got]);
            if got < n {
                // The track's end.
                return Ok(render::Redraw { start, l, r, over: over.clone() });
            }
            pos = chain.stage.position();
        }
    }
    Ok(render::Redraw { start, l, r, over: over.clone() })
}

/// Where to build the HP pair, in seconds ahead of the reader: by the time
/// it is built (`build_s`) and has drawn (at `rtf` seconds of audio a
/// second) up to the write head, which runs on `buffered_s` ahead of the
/// reader, the reader must still be a guard short of its start. With the
/// start at reader + x: the drawing takes (B - x + b + past) / (rtf - 1),
/// and x = b + T + guard gives x = b + guard + (B + past - guard) / rtf.
/// An unknown speed counts only the build and the guard (the drawing's
/// head may then be passed: drawn for nothing, as before). None, at the
/// write head as before: a drawing that never catches the write head, or a
/// start less than a guard short of where the write head will be once the
/// pair is built (it runs on by the build meanwhile).
fn draw_ahead_s(build_s: f64, buffered_s: f64, rtf: Option<f64>, guard_s: f64) -> Option<f64> {
    let x = match rtf {
        Some(r) if r <= 1.0 => return None,
        Some(r) => build_s + guard_s + (buffered_s + DRAW_PAST_WRITE_S - guard_s).max(0.0) / r,
        None => build_s + guard_s,
    };
    (x + guard_s <= buffered_s + build_s).then_some(x)
}

/// What the HP swap's send check looks at, taken under `st`.
#[derive(Clone, Copy, Debug)]
struct HpObs {
    /// The generation the job started under is still the current one.
    generation: bool,
    /// The track is still the one the listener chose.
    current: bool,
    /// Hybrid-Phase or alpha-HP is still asked for it.
    phase: bool,
    /// The same playback session (a Stop → Play makes a new timeline).
    timeline: bool,
    /// The write head still carries the stand-in the chain was built after.
    stand_in: bool,
    seek_pending: bool,
    swap_pending: bool,
    hold: bool,
    /// The placement found the write head on this track, at this rate.
    placed: bool,
    /// The chain renders on the GPU, and the GPU failed on this track.
    gpu_chain: bool,
    gpu_failed: bool,
}

/// Why the built HP chain must not be sent now (None = send). A GPU chain is
/// never sent once the GPU has failed on the track.
fn hp_send_check(o: &HpObs) -> Option<&'static str> {
    if !o.generation {
        Some("generation")
    } else if !o.current {
        Some("track")
    } else if !o.phase {
        Some("phase")
    } else if !o.timeline {
        Some("timeline")
    } else if o.gpu_chain && o.gpu_failed {
        Some("gpu-failed")
    } else if !o.stand_in {
        Some("moved")
    } else if o.seek_pending {
        Some("seek")
    } else if o.swap_pending {
        Some("swap")
    } else if o.hold {
        Some("hold")
    } else if !o.placed {
        Some("placement")
    } else {
        None
    }
}

/// The reasons worth one more pass of the HP swap: the GPU failed meanwhile
/// (the pair then follows K4's CPU rung), or another chain landed or is
/// landing (route again on what plays then).
fn hp_retry(why: &str) -> bool {
    matches!(why, "gpu-failed" | "moved" | "swap")
}

/// Whether building the pair at `pair_taps` would step a stand-in down a
/// second time on its track: the stand-in is already below the rack's taps
/// for power (at the start or by K3) and the pair is lower still. A GPU
/// failure's rung (K4) is not a step the pair takes.
fn second_step_down(stand_in: &ChainDesc, pair_taps: usize) -> bool {
    let stepped = stand_in.downgrade.as_ref().is_some_and(|d| d.reason == "power");
    stepped && taps_of(stand_in).is_some_and(|t| pair_taps < t)
}

/// What K3 steps down, at the audible rung: `(settings, variant,
/// used_quick, full_key)`. The rack as it is, on its full variant — else
/// its quick one while that plays. With Instant start off (`instant`
/// false) the chain on the air (`air`) — its own settings and variant —
/// never the rack still being prepared: a rack change to Hybrid-Phase has
/// its variant cached before its envelope, and K3 put in the new rack's
/// linear stand-in (or its quick variant), which that mode never plays.
/// (That it is whole is k3_snapshot's to see, outside the state lock.)
fn k3_source(
    instant: bool,
    rack: PlayerSettings,
    air: &ChainDesc,
    audible: usize,
    mut cached: impl FnMut(&str) -> Option<Arc<Variant>>,
) -> Result<(PlayerSettings, Arc<Variant>, bool, String), &'static str> {
    let mut s = if instant { rack } else { (*air.settings).clone() };
    if s.mode == Mode::Direct {
        return Err("direct");
    }
    s.taps = audible;
    let full_key = s.source_key();
    if !instant {
        let v = air.variant.upgrade().or_else(|| cached(&variant_key(&s))).ok_or("no-variant")?;
        let quick = v.quick;
        return Ok((s, v, quick, full_key));
    }
    let (v, used_quick) = match cached(&full_key) {
        Some(v) => (v, false),
        // A first variant streaming for another rack is not this rack's sound.
        None => (
            cached(&s.quick().source_key())
                .filter(|v| crate::player::chain::first_variant_fits(v, &s))
                .ok_or("no-variant")?,
            true,
        ),
    };
    Ok((s, v, used_quick, full_key))
}

/// Where K3 goes, and what the chips show: `(taps, downgrade, gen)`.
/// On the GPU (`on_gpu`) the audible taps, downgrade and gen stay as they
/// are, so nothing blinks. Otherwise one rung below on the CPU (`below`,
/// asked only then; `None` = no rung below, K3 stays) with reason "power"
/// under a new gen (`next_gen`, bumped only then), so the taps chip blinks.
fn k3_plan(
    on_gpu: bool,
    audible: usize,
    downgrade: &Option<DowngradeInfo>,
    downgrade_gen: u32,
    model_taps: usize,
    below: impl FnOnce() -> Option<usize>,
    next_gen: impl FnOnce() -> u32,
) -> Option<(usize, Option<DowngradeInfo>, u32)> {
    if on_gpu {
        return Some((audible, downgrade.clone(), downgrade_gen));
    }
    let one = below()?;
    let taps = if policy::K3_MAX_RUNGS == 1 { one } else { model_taps.min(one) };
    let info = DowngradeInfo::below(
        downgrade.as_ref(),
        taps_label(audible).unwrap_or("?").to_string(),
        taps_label(taps).unwrap_or("?").to_string(),
        "power",
    );
    Some((taps, Some(info), next_gen()))
}

/// The first rung below `taps` whose filters for `phase` that take a source
/// at `src` up to `rate` are on disk; never below `MIN_FALLBACK_TAPS`.
fn rung_below_existing(taps: usize, src: u32, rate: u32, phase: Phase) -> Option<usize> {
    rung_below_where(taps, |t| filters_exist(t, src, rate, phase, &find_precomputed_filter))
}

fn rung_below_where(taps: usize, has: impl Fn(usize) -> bool) -> Option<usize> {
    let mut t = taps;
    while let Some(below) = policy::rung_below(t) {
        if has(below) {
            return Some(below);
        }
        t = below;
    }
    None
}

/// Whether the blobs a chain of `phase` loads exist, asked of the resolver
/// the build uses. TFS is derived from the linear and minimum pair.
fn filters_exist(
    taps: usize,
    src: u32,
    rate: u32,
    phase: Phase,
    find: &dyn Fn(usize, u32, u32, &str) -> Option<String>,
) -> bool {
    let has = |p: &str| find(taps, src, rate, p).is_some();
    match phase {
        Phase::Linear => has("linear_phase"),
        Phase::Minimum => has("minimum_phase"),
        Phase::Tfs | Phase::Hybrid | Phase::Alpha => has("linear_phase") && has("minimum_phase"),
    }
}

/// The ladder rung a chain description names.
fn taps_of(d: &ChainDesc) -> Option<usize> {
    let label = d.taps.as_deref()?;
    TAP_LADDER.iter().copied().find(|&t| taps_label(t) == Some(label))
}

/// A Hybrid-Phase or alpha-HP pair: both push an "HP" stage (chain.rs).
fn is_pair(d: &ChainDesc) -> bool {
    d.stages.iter().any(|s| s.tok == "HP")
}

/// The calibration key of the chain `d` describes, at `rate`.
fn desc_key(d: &ChainDesc, rate: u32) -> Option<String> {
    calibration::key(taps_of(d)?, rate, is_pair(d))
}

fn desc_id(d: &Arc<ChainDesc>) -> usize {
    Arc::as_ptr(d) as *const () as usize
}

/// What a chain costs to render: taps, pair, GPU, source. A swap that keeps
/// it (quick → full) keeps the detector's running deficit.
fn cost_id(d: &ChainDesc) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.taps.hash(&mut h);
    is_pair(d).hash(&mut h);
    d.gpu_on.hash(&mut h);
    d.source.hash(&mut h);
    h.finish()
}

/// A live chain on the CPU at a ladder rung: what K3 can escalate.
fn cpu_live(d: &ChainDesc) -> bool {
    d.source == "live" && !d.gpu_on && taps_of(d).is_some()
}

/// The newest mark carrying `desc`: its frame, and how far its index is from
/// the index the mark before it runs on to at that frame (0 = a continuous
/// splice).
fn splice_mark(shared: &Shared, desc: &Arc<ChainDesc>) -> (Option<u64>, Option<i64>) {
    let g = shared.marks.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = g.iter().rposition(|m| Arc::ptr_eq(&m.desc, desc)) else { return (None, None) };
    let m = &g[i];
    let err = i
        .checked_sub(1)
        .map(|j| &g[j])
        .filter(|p| p.track_id == m.track_id && p.rate == m.rate && p.frame <= m.frame)
        .map(|p| m.index as i64 - (p.index + (m.frame - p.frame)) as i64);
    (Some(m.frame), err)
}

/// The rungs at `rate` that route_conv, K4 and the trace route single
/// (`pair` false) or pair chains on. A pair rung is bounded by its linear
/// half: RTF ≤ linear / `PAIR_MIN_COST_RATIO`. So a pair with no entry of its
/// own routes on its linear half (not on the CPU at full taps for want of a
/// number), a K3 deficit recorded on a deferred start's linear stand-in
/// reaches the pair, and an optimistic pair entry is capped. The bound is
/// never stored: the pre-trial still sees the pair's key missing.
pub(super) fn route_ladder(c: &CalibrationStore, rate: u32, l: usize, pair: bool) -> Vec<TapRtf> {
    let src_rate = rate / (l as u32).max(1);
    let measured = |t: usize, pair: bool| calibration::key(t, rate, pair).and_then(|k| c.rtf_for(&k, src_rate));
    TAP_LADDER
        .iter()
        .filter_map(|&t| {
            let own = measured(t, pair);
            let bound = if pair { measured(t, false).map(|lin| lin / policy::PAIR_MIN_COST_RATIO) } else { None };
            let rtf = match (own, bound) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => a.or(b)?,
            };
            Some(TapRtf { taps: t, rtf })
        })
        .collect()
}

fn ladder_json(measured: &[TapRtf]) -> Value {
    Value::Array(
        policy::fill_ladder(measured)
            .into_iter()
            .map(|(r, m)| json!([taps_label(r.taps), r3(r.rtf), m]))
            .collect(),
    )
}

fn entry_json(e: &CalibEntry) -> Value {
    json!({ "ms": r3(e.block_cost_ms), "provisional": e.provisional, "ttl": e.ttl_secs })
}

fn downgrade_json(d: &ChainDesc) -> Value {
    match &d.downgrade {
        Some(g) => json!({ "from": g.from, "to": g.to, "reason": g.reason, "gen": d.downgrade_gen }),
        None => Value::Null,
    }
}

fn verdict_kind(v: &DeficitVerdict) -> u8 {
    match v {
        DeficitVerdict::Idle => 0,
        DeficitVerdict::Arming { .. } => 1,
        DeficitVerdict::Watching { .. } => 2,
        DeficitVerdict::Fire { .. } => 3,
        DeficitVerdict::Spent => 4,
    }
}

/// `k3.arm` / `k3.watch` / `k3.reset` on the detector's transitions.
fn trace_verdict(v: &DeficitVerdict, s: &DeficitSample) {
    let kind = verdict_kind(v);
    let was = LAST_VERDICT.swap(kind, Ordering::Relaxed);
    if kind == was {
        return;
    }
    let ev = match (was, kind) {
        (_, 1) => "k3.arm",
        (_, 2) => "k3.watch",
        (1 | 2, 0) => "k3.reset",
        _ => return,
    };
    let (windows, held_s, readings) = match v {
        DeficitVerdict::Arming { windows } => (Some(*windows), None, None),
        DeficitVerdict::Watching { held_s, readings } => (None, Some(r3(*held_s)), Some(*readings)),
        _ => (None, None, None),
    };
    trace(ev, json!({
        "track": s.track_id,
        "windows": windows,
        "heldS": held_s,
        "readings": readings,
        "rtf": s.rtf_milli as f64 / 1000.0,
        "bufferedS": r3(s.buffered_s),
        "targetS": r3(s.target_s),
        "blocked": s.blocked,
        "cpuLive": s.cpu_live,
    }));
}

fn trace_pool_once() {
    if POOL_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    trace("pool", json!({
        "cpuWidth": calibration::cpu_width(),
        "renderPool": render::render_pool().map(|p| p.current_num_threads()),
        "powerPool": power_width(),
        "mask": affinity_mask().map(|m| format!("{:#x}", m)),
        "machine": calibration::machine_id(),
        "rayonGlobal": rayon::current_num_threads(),
    }));
}

/// The process affinity mask (the current processor group only).
fn affinity_mask() -> Option<u64> {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::GetCurrentProcess;
        use winapi::um::winbase::GetProcessAffinityMask;
        let mut process: usize = 0;
        let mut system: usize = 0;
        if GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) != 0 {
            return Some(process as u64);
        }
    }
    None
}

/// One `[POWER] {json}` line: the fields plus `ev`, `tMs` (since the first
/// power call) and `unixMs`.
fn trace(ev: &str, fields: Value) {
    let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let line = trace_line(ev, fields, ms(since_t0(Instant::now())), unix_ms);
    crate::aelog!("[POWER] {}", line);
}

fn trace_line(ev: &str, fields: Value, t_ms: f64, unix_ms: u64) -> String {
    let mut m = match fields {
        Value::Object(m) => m,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    m.insert("ev".into(), Value::from(ev));
    m.insert("tMs".into(), json!(r3(t_ms)));
    m.insert("unixMs".into(), json!(unix_ms));
    Value::Object(m).to_string()
}

fn since_t0(now: Instant) -> Duration {
    now.saturating_duration_since(*T0.get_or_init(Instant::now))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn secs(frames: u64, rate: u32) -> f64 {
    frames as f64 / rate.max(1) as f64
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests;
