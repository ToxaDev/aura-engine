//! The radio experiment on the transport: a stream started by a hidden
//! command (`player_radio`), its chain planned while it fills and built
//! once the filter's look-ahead and a margin are in, then put on the air
//! with `play_chain` — the way to the device every track takes (volume,
//! test mute, guard). A track chosen, Stop or a device lost end it; a rack
//! change tunes in again with the new rack.
//!
//! BIT-PERFECT plays the stream as decoded (the live source's shadow) at its
//! own rate, no rack, no volume, as a file's direct variant plays; turned on
//! or off under a stream on the air, the other chain takes over from the
//! place being heard, the way a track's switch does it (`bp_ready`).

use super::*;

/// The stream's chain being built anew for a rack change on the air
/// (`radio_retune`): the number of that build while it runs, 0 when none.
/// The status says it (`radio.switching`): the page's bar runs meanwhile.
static SWITCHING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SWITCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The build `n` is over (its chain taken over, or given up): unless a
/// later one is running.
fn switch_over(n: u64) {
    let _ = SWITCHING.compare_exchange(n, 0, Ordering::AcqRel, Ordering::Acquire);
}
use crate::player::radio::chain as live_chain;
use crate::player::radio::live::LiveSource;
use crate::player::radio::Session;
use crate::player::source_stages::SourcePlan;

/// A switch of mode under a stream on the air: the output's fade before the
/// device opens again (`bp_ready`'s 120 ms, and its timer's margin). The new
/// chain starts this far past the place heard when it was asked for.
const MODE_SWITCH_FADE_S: f64 = 0.13;

/// The rack a stream's chain plays, for the status and the log.
fn rack_line(plan: &live_chain::LivePlan) -> String {
    format!(
        "{} {:?} FS×{} → {} Hz{}{}{}",
        crate::audio::converter::dsp::filter::taps_label(plan.s.taps).unwrap_or("?"),
        plan.s.phase,
        plan.s.fs_multiplier,
        plan.out_rate,
        if plan.source.tokens().is_empty() { String::new() } else { format!(", source {}", plan.source.tokens().join("·")) },
        if plan.s.isp { format!(", ISP-L {:.1} dBTP", plan.ceiling_db) } else { String::new() },
        if plan.s.subsonic_hz != 0 { format!(", SUB{}-G", plan.s.subsonic_hz) } else { String::new() },
    )
}

/// BIT-PERFECT on a stream at `rate`, for the status and the log.
fn direct_line(rate: u32, bits: u32) -> String {
    match bits {
        0 => format!("BIT-PERFECT {} Hz, the decoder's output in the widest integer the device takes", rate),
        b => format!("BIT-PERFECT {} Hz, {}-bit", rate, b),
    }
}

impl Player {
    /// Play the stream at `url` (the radio experiment).
    pub fn radio(&self, url: String) {
        self.send_job(Job::Radio { url });
    }

    pub(super) fn radio_active(&self) -> bool {
        self.radio.lock().unwrap().is_some()
    }

    fn radio_session(&self, gen: u64) -> Option<Arc<Session>> {
        self.radio.lock().unwrap().as_ref().filter(|(g, _)| *g == gen).map(|(_, s)| s.clone())
    }

    /// Session `gen`'s shared state, while it is the radio's (the stream's
    /// instruments read its shadow and its song starts: `spatial::live`).
    pub fn radio_shared(&self, gen: u64) -> Option<Arc<crate::player::radio::RadioShared>> {
        self.radio_session(gen).map(|s| s.shared.clone())
    }

    /// End the stream (the caller stops the device, if it should).
    pub(super) fn radio_stop(&self) {
        let s = self.radio.lock().unwrap().take();
        if let Some((_, s)) = s {
            s.stop();
        }
    }

    /// The stream's part of the status; `heard`: the source frame heard, when
    /// its chain is on the air.
    pub(super) fn radio_status(&self, heard: Option<i64>) -> Value {
        let s = self.radio.lock().unwrap().as_ref().map(|(_, s)| s.clone());
        s.map_or(Value::Null, |s| {
            let mut v = s.status_json(heard);
            // A new chain being built for a rack change: its number (the
            // page's bar runs while it is there), else null.
            let n = SWITCHING.load(Ordering::Acquire);
            v["switching"] = if n > 0 { Value::from(n) } else { Value::Null };
            // Why the scenes with instruments go without them here ("gpu",
            // "memory"), else null: the menu says so.
            v["instruments"] = crate::spatial::live::why_not().map_or(Value::Null, |w| Value::from(w.word()));
            v
        })
    }

    /// The kind of the radio's failure (radio::ERR_*) while its words are
    /// the player's error: the page shows its own text for it.
    pub(super) fn radio_error_kind(&self, error: Option<&str>) -> Option<&'static str> {
        let e = error?;
        self.radio_err.lock().unwrap().as_ref().filter(|(_, text)| text == e).map(|(kind, _)| *kind)
    }

    /// A failure of the radio's, said in the player's error line; its kind
    /// goes with it (radio_error_kind).
    fn radio_fail(&self, kind: &'static str, error: String) {
        let text = format!("Radio: {}", error);
        *self.radio_err.lock().unwrap() = Some((kind, text.clone()));
        self.fail(text);
    }

    pub(super) fn radio_start(&self, url: String) {
        crate::aelog!("[UI] radio: {}", url);
        // What plays fades out (the output's hold ramp, as a track's switch)
        // and the device stays open in silence until the stream's chain
        // comes: one station after another, or a stream after a track, with
        // no click and no reopening of the device for the same rate and mode.
        // Paused or stopped, nothing is held: the stream starts afresh.
        if !self.radio_hold_air() {
            self.stop_now();
        }
        let gen = self.radio_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let settings = {
            let mut st = self.st.lock().unwrap();
            st.error = None;
            st.current = None;
            st.pending = Some(("Tuning in".into(), Instant::now()));
            st.settings.clone()
        };
        let session = Arc::new(Session::start(&url, &settings));
        *self.radio.lock().unwrap() = Some((gen, session.clone()));
        let spawned = std::thread::Builder::new().name("aura-radio-start".into()).spawn(move || {
            let p = get();
            match p.radio_prepare(&session, &settings) {
                Ok(chain) => p.send_internal(Job::RadioReady { chain: Box::new(chain), gen, continuous: false }),
                Err((kind, error)) => p.send_internal(Job::RadioFailed { gen, kind, error }),
            }
        });
        if let Err(e) = spawned {
            self.radio_failed(gen, crate::player::radio::ERR_OTHER, e.to_string());
        }
    }

    /// The rack changed (or the device): a chain at the place being heard,
    /// primed from the stream's history, when the new filter's look-ahead is
    /// already in the stream — the render thread crossfades into it as into
    /// a track's new rack. When it looks further ahead than the stream holds
    /// (or the rate changes, or the stream is not on the air yet), the
    /// stream is tuned in again at its live edge. BIT-PERFECT turned on or
    /// off switches the mode in place (`radio_mode_switch`); while it stays
    /// on, the stream plays as decoded whatever the rack under it.
    pub(super) fn radio_retune(&self) {
        let cur = self.radio.lock().unwrap().as_ref().map(|(g, s)| (*g, s.clone()));
        let Some((gen, session)) = cur else { return };
        let settings = self.stream_settings();
        let on_air = session.shared.info.lock().unwrap().t_on_air_ms.is_some();
        let live = session.shared.live.get().cloned();
        let url = session.shared.url.clone();
        let url_here = url.clone();
        let direct = settings.mode == Mode::Direct;
        let direct_now = self.st.lock().unwrap().stream_direct;
        match live {
            Some(live) if on_air && !session.stopped() && direct != direct_now => {
                self.radio_mode_switch(session, live, settings);
            }
            Some(_) if on_air && !session.stopped() && direct => {
                crate::aelog!("[RADIO] the rack changed under BIT-PERFECT: the stream plays on as decoded");
            }
            Some(live) if on_air && !session.stopped() => {
                let t0 = Instant::now();
                let n = SWITCHES.fetch_add(1, Ordering::AcqRel) + 1;
                SWITCHING.store(n, Ordering::Release);
                let spawned = std::thread::Builder::new().name("aura-radio-switch".into()).spawn(move || {
                    let p = get();
                    let built = p.radio_switch(&session, &live, &settings, false);
                    switch_over(n);
                    match built {
                        Ok(chain) => {
                            crate::aelog!("[RADIO] rack change: the new chain takes over in place (built in {} ms)", t0.elapsed().as_millis());
                            p.send_internal(Job::RadioReady { chain: Box::new(chain), gen, continuous: true });
                        }
                        Err(why) => {
                            crate::aelog!("[RADIO] rack change: {} — tuning in again at the live edge", why);
                            p.send_internal(Job::Radio { url });
                        }
                    }
                });
                if spawned.is_err() {
                    switch_over(n);
                    self.radio_start(url_here);
                }
            }
            _ => {
                crate::aelog!("[RADIO] the rack or the device changed: tuning in again");
                self.radio_start(url_here);
            }
        }
    }

    /// BIT-PERFECT turned on or off under the stream on the air: the other
    /// chain from the place being heard — the shadow as decoded
    /// (`radio_direct_here`), or the rack primed from the stream's history
    /// (`radio_switch`) — then the switch a track's is (`bp_ready`): the
    /// output fades, the device opens again in the other mode, and the chain
    /// goes on from that place. The rack not to be had there (its source
    /// stages are not the stream's, its filter looks further ahead than the
    /// stream holds…): the stream is tuned in again at its live edge.
    fn radio_mode_switch(&self, session: Arc<Session>, live: Arc<LiveSource>, settings: PlayerSettings) {
        let generation = {
            let mut st = self.st.lock().unwrap();
            st.rate_switch_gen = Some(st.generation);
            st.generation
        };
        let direct = settings.mode == Mode::Direct;
        let bits = if direct { session.direct_bits() } else { 24 };
        let url = session.shared.url.clone();
        let what = if direct { "BIT-PERFECT on" } else { "BIT-PERFECT off" };
        let t0 = Instant::now();
        let spawned = std::thread::Builder::new().name("aura-radio-mode".into()).spawn(move || {
            let p = get();
            let chain = if direct { p.radio_direct_here(&session, &live, &settings) } else { p.radio_switch(&session, &live, &settings, true) };
            match chain {
                Ok(chain) => {
                    crate::aelog!(
                        "[RADIO] {}: the new chain from {:.2} s of the stream (built in {} ms); the output fades and the device opens again",
                        what,
                        chain.start as f64 / chain.out_rate.max(1) as f64,
                        t0.elapsed().as_millis()
                    );
                    p.send_internal(Job::BpReady { chain: Box::new(chain), generation, direct, bits });
                }
                Err(why) => {
                    {
                        let mut st = p.st.lock().unwrap();
                        if st.rate_switch_gen == Some(generation) {
                            st.rate_switch_gen = None;
                        }
                    }
                    crate::aelog!("[RADIO] {}: {} — tuning in again at the live edge", what, why);
                    p.send_internal(Job::Radio { url });
                }
            }
        });
        if spawned.is_err() {
            let url = {
                let mut st = self.st.lock().unwrap();
                st.rate_switch_gen = None;
                drop(st);
                self.radio.lock().unwrap().as_ref().map(|(_, s)| s.shared.url.clone())
            };
            if let Some(url) = url {
                self.radio_start(url);
            }
        }
    }

    /// The source frame being heard of the stream's chain on the air (a
    /// seek is not one of a stream's: `going_on`).
    fn radio_heard_src(&self) -> Option<i64> {
        let st = self.st.lock().unwrap();
        let tl = st.timeline.as_ref()?;
        let m = self.going_on(tl)?;
        (m.track_id == live_chain::RADIO_TRACK_ID).then(|| (m.index / m.l.max(1) as u64) as i64)
    }

    /// BIT-PERFECT's chain from the place being heard, past the output's
    /// fade: the shadow from there — or from its first frame, should the
    /// place be older than the shadow keeps.
    fn radio_direct_here(&self, session: &Session, live: &Arc<LiveSource>, settings: &PlayerSettings) -> Result<Chain, String> {
        let heard = self.radio_heard_src().ok_or("no place on the air")?;
        let rate = live.rate();
        let mut at = heard + (MODE_SWITCH_FADE_S * rate as f64) as i64;
        let first = live.raw_start();
        if at < first {
            crate::aelog!("[RADIO] BIT-PERFECT on: the shadow keeps the stream from {:.2} s, not {:.2} s — from there", first as f64 / rate as f64, at as f64 / rate as f64);
            at = first;
        }
        let bits = session.direct_bits();
        session.mark(|i, _| {
            i.rack = direct_line(rate, bits);
            i.route = "direct".into();
            i.delay_s = 0.0;
            i.out_rate = rate;
        });
        Ok(live_chain::build_direct(live, at.max(0) as u64, settings, Arc::downgrade(&session.shared)))
    }

    /// A chain for `settings` at the place being heard (a lead ahead), on the
    /// same stream: Err says why it cannot be made in place. `reopen`: it
    /// goes on the air on a stream of its own (BIT-PERFECT turned off), not
    /// crossfaded in on the one there — its output rate need not be that
    /// stream's, and it starts past the output's fade.
    fn radio_switch(&self, session: &Session, live: &Arc<LiveSource>, settings: &PlayerSettings, reopen: bool) -> Result<Chain, String> {
        if settings.mode == Mode::Direct {
            return Err("BIT-PERFECT plays the stream as decoded".into());
        }
        // The card to the chain while it is built: the stream's instruments
        // let go of it first (the sound first).
        let _card = crate::spatial::live::GpuHold::take();
        let rate = live.rate();
        // The stream's source was made by the rack it was tuned in with: other
        // source stages need it made again, from the live edge.
        if session.shared.source_plan(rate).key() != SourcePlan::new(settings, rate).key() {
            return Err("the source stages changed".into());
        }
        // The banks first (the slow part), while the old chain plays on.
        let probe = live_chain::plan(&self.res, settings, rate, 0)?;
        if !reopen {
            let tl_rate = self.st.lock().unwrap().timeline.as_ref().map(|t| t.rate());
            if tl_rate != Some(probe.out_rate) {
                return Err(format!("the output rate changes to {} Hz", probe.out_rate));
            }
        }
        let (gpu, route, down) = self.radio_route(&probe);
        if down.is_some() {
            return Err(route);
        }
        let at = {
            let st = self.st.lock().unwrap();
            let tl = st.timeline.as_ref().ok_or("no stream on the air")?;
            let m = self.going_on(tl).ok_or("no place on the air")?;
            if m.track_id != live_chain::RADIO_TRACK_ID {
                return Err("another chain on the air".into());
            }
            if reopen {
                // The source frame heard, on the new chain's grid.
                (m.index / m.l.max(1) as u64) * probe.l as u64
                    + ((self.lead_s(settings) + MODE_SWITCH_FADE_S) * probe.out_rate as f64) as u64
            } else {
                m.index + (self.lead_s(settings) * probe.out_rate as f64) as u64
            }
        };
        let mut plan = live_chain::plan(&self.res, settings, rate, at)?;
        // The session's adaptive headroom decision, for this rack's Headroom.
        session.shared.first_look(|st, secs| plan.decide_headroom(st, secs));
        // The new filter's first block, and a second of slack, must be in.
        let need = plan.first_need + rate as i64;
        if live.end() < need {
            return Err(format!(
                "the new filter looks {:.2} s ahead, {:.2} s more than the stream holds",
                plan.delay_s(),
                (need - live.end()) as f64 / rate as f64
            ));
        }
        session.mark(|i, _| {
            i.route = route.clone();
            i.delay_s = plan.delay_s();
            if reopen {
                i.rack = rack_line(&plan);
                i.out_rate = plan.out_rate;
            }
        });
        Ok(live_chain::build(&plan, live, &session.shared, gpu, session.tally.clone()))
    }

    /// Where the convolver of a stream's chain goes: the policy a track's
    /// chain is routed by (`route_conv`), less the pre-trial (no file to
    /// time a block of). Returns the card, a word for the log and the
    /// smaller tap count the policy asks for, if it does.
    fn radio_route(&self, p: &live_chain::LivePlan) -> (Option<Arc<GpuPolyCtx>>, String, Option<usize>) {
        let s = &p.s;
        let gpu_ctx = if self.shared.gpu_failed.load(Ordering::Acquire) { None } else { GpuPolyCtx::try_build() };
        let free_vram = gpu_ctx.as_ref().map(|c| c.free_vram_bytes()).unwrap_or(0);
        let tap_rtfs: Vec<TapRtf> = {
            let calib = self.shared.calibration.lock().unwrap_or_else(|e| e.into_inner());
            power::route_ladder(&calib, p.out_rate, p.l, p.min_bank.is_some())
        };
        let rtf = tap_rtfs.iter().find(|t| t.taps == s.taps).map(|t| t.rtf);
        // what the stream's instruments leave free beside their session
        crate::spatial::live::set_conv_vram(if s.use_gpu { vram_for(s.phase, s.taps, p.l) } else { 0 });
        let inputs = PolicyInputs {
            use_gpu: s.use_gpu,
            gpu_ctx,
            free_vram_bytes: free_vram,
            vram_demand: vram_for(s.phase, s.taps, p.l),
            conversion_running: crate::audio::converter::manager::is_running(),
            taps: s.taps,
            tap_rtfs,
        };
        let rtf = rtf.map_or("no calibration".to_string(), |r| format!("CPU RTF {:.2}", r));
        match policy::decide(&inputs) {
            PolicyDecision::Cpu => (None, format!("cpu ({})", rtf), None),
            PolicyDecision::Gpu { ctx } => (Some(ctx), format!("gpu ({})", rtf), None),
            PolicyDecision::CpuDowngraded { to_taps, reason } => {
                (None, format!("cpu, down to {} taps ({}; {})", to_taps, reason, rtf), Some(to_taps))
            }
        }
    }

    /// Off the worker: wait for the stream's format, plan the chain (its
    /// banks load meanwhile), wait for its look-ahead and the margin, build.
    /// BIT-PERFECT: `radio_prepare_direct`.
    fn radio_prepare(&self, session: &Session, settings: &PlayerSettings) -> Result<Chain, (&'static str, String)> {
        use crate::player::radio::kind_of;
        let live = session.wait_live(Duration::from_secs(30))?;
        if settings.mode == Mode::Direct {
            return self.radio_prepare_direct(session, settings, &live);
        }
        let rate = live.rate();
        let planned = |s: &PlayerSettings| live_chain::plan(&self.res, s, rate, 0).map_err(|e| (kind_of(&e), e));
        let mut plan = planned(settings)?;
        // The card to the chain while it is routed and built: the stream's
        // instruments let go of it first (the sound first).
        let _card = crate::spatial::live::GpuHold::take();
        let (mut gpu, route, down) = self.radio_route(&plan);
        if let Some(to) = down {
            plan = planned(&PlayerSettings { taps: to, ..settings.clone() })?;
            gpu = None;
        }
        // The source stages hand the stream on in block-sized steps: their
        // hold-back goes on top of the margin.
        let need = plan.first_need + (session.shared.margin_s * rate as f64) as i64 + plan.source.latency_frames() as i64;
        let rack = rack_line(&plan);
        session.mark(|i, ms| {
            i.t_planned_ms = Some(ms);
            i.rack = rack.clone();
            i.route = route.clone();
            i.delay_s = plan.delay_s();
            i.need_s = need as f64 / rate as f64;
            i.out_rate = plan.out_rate;
        });
        crate::aelog!(
            "[RADIO] chain planned: {} ({}); the filter looks {:.2} s ahead; waiting for {:.2} s of the stream ({:.2} s in)",
            rack,
            route,
            plan.delay_s(),
            need as f64 / rate as f64,
            live.end() as f64 / rate as f64
        );
        session.wait_frames(&live, need)?;
        session.mark(|i, ms| i.t_ready_ms = Some(ms));
        // The adaptive headroom on the stream's first seconds (all of them
        // when the margin has brought them in): the session's one decision.
        session.shared.first_look(|st, secs| plan.decide_headroom(st, secs));
        if let Some((_, why)) = &plan.ahr {
            crate::aelog!("[RADIO] adaptive headroom: {}", why);
            session.mark(|i, _| i.rack = rack_line(&plan));
        }
        let chain = live_chain::build(&plan, &live, &session.shared, gpu, session.tally.clone());
        session.mark(|i, ms| i.t_built_ms = Some(ms));
        crate::aelog!(
            "[RADIO] chain built ({}) at {:.2} s of the session",
            if chain.gpu_on { "GPU" } else { "CPU" },
            session.shared.elapsed_s()
        );
        Ok(chain)
    }

    /// BIT-PERFECT from the start: the shadow — the stream as decoded, at its
    /// rate — once the margin is in, and what the source stages hold back
    /// (they run beside it for the rack to come back to, and the shadow goes
    /// in with what they hand on). No filter: any rate the device takes.
    fn radio_prepare_direct(&self, session: &Session, settings: &PlayerSettings, live: &Arc<LiveSource>) -> Result<Chain, (&'static str, String)> {
        let rate = live.rate();
        let need = (session.shared.margin_s * rate as f64) as i64 + session.shared.source_plan(rate).latency_frames() as i64;
        let rack = direct_line(rate, session.direct_bits());
        session.mark(|i, ms| {
            i.t_planned_ms = Some(ms);
            i.rack = rack.clone();
            i.route = "direct".into();
            i.delay_s = 0.0;
            i.need_s = need as f64 / rate as f64;
            i.out_rate = rate;
        });
        crate::aelog!(
            "[RADIO] {}: waiting for {:.2} s of the stream ({:.2} s in)",
            rack,
            need as f64 / rate as f64,
            live.end() as f64 / rate as f64
        );
        session.wait_frames(live, need)?;
        session.mark(|i, ms| {
            i.t_ready_ms = Some(ms);
            i.t_built_ms = Some(ms);
        });
        Ok(live_chain::build_direct(live, 0, settings, Arc::downgrade(&session.shared)))
    }

    pub(super) fn radio_ready(&self, chain: Chain, gen: u64, continuous: bool) {
        let Some(session) = self.radio_session(gen) else { return };
        if session.stopped() {
            return;
        }
        if continuous {
            // A rack change in place: the render thread crossfades into the
            // new chain at its start, as for a track's new rack.
            let at = chain.start as f64 / chain.out_rate.max(1) as f64;
            let _ = self.render_tx.lock().unwrap().send(Msg::Swap {
                chain,
                continuous: true,
                seek: false,
                side_buf: None,
                requested: Instant::now(),
            });
            crate::aelog!("[RADIO] rack change: switching in place at {:.2} s of the stream", at);
            return;
        }
        // Another rate opens the device again: what was held has faded out
        // first (it has, the network took longer than a period; the wait is
        // for the device's buffer). The same rate and mode goes in by the
        // jump of a track's switch: the held output ramps up into the new
        // chain.
        let other_rate = self.st.lock().unwrap().timeline.as_ref().is_some_and(|t| t.rate() != chain.out_rate);
        if other_rate {
            self.fade_out();
        }
        // BIT-PERFECT: the device at the stream's own depth (`direct_bits`).
        let direct = chain.direct;
        let bits = if direct { session.direct_bits() } else { 24 };
        // "Tuning in" stays until the stream is on the air: let go before,
        // a fresh start read "stopped" for a poll.
        let aired = self.play_chain(chain, direct, bits);
        self.set_pending(None);
        match aired {
            Ok(()) => {
                session.mark(|i, ms| i.t_on_air_ms = Some(ms));
                self.radio_reaired(direct);
                crate::aelog!("[RADIO] on the air at {:.2} s of the session", session.shared.elapsed_s());
            }
            Err(e) => {
                self.radio_stop();
                self.stop_now();
                self.fail(e);
            }
        }
    }

    /// The stream's chain went on the air on a stream of its own (tuned in,
    /// or BIT-PERFECT switched): the delay's corridor reads that device's
    /// clock, and BIT-PERFECT has no slow gain reading ahead (`buffer_s`).
    pub(super) fn radio_reaired(&self, direct: bool) {
        let s = self.radio.lock().unwrap().as_ref().map(|(_, s)| s.clone());
        if let Some(s) = s {
            s.set_timeline(self.st.lock().unwrap().timeline.clone());
            if direct {
                s.shared.gain_queue.store(0, Ordering::Relaxed);
            }
        }
    }

    /// A station or a track on the air goes silent for the stream to come:
    /// the output ramps down (hold) and plays silence with the device open,
    /// the session before ends, and what Stop would let go of is let go —
    /// except the device. False when nothing plays (stopped, or paused):
    /// the caller stops the player instead.
    fn radio_hold_air(&self) -> bool {
        let playing = {
            let st = self.st.lock().unwrap();
            st.output.is_some() && st.play == PlayState::Playing
        };
        if !playing {
            return false;
        }
        self.out.jump_frame.store(u64::MAX, Ordering::Release);
        self.out.hold.store(true, Ordering::Release);
        self.radio_stop();
        *self.k4_waits.lock().unwrap() = None;
        self.apply_deferred.store(false, Ordering::Release);
        let prewarmed = {
            let mut st = self.st.lock().unwrap();
            if st.queued_next.take().is_some() {
                let _ = self.render_tx.lock().unwrap().send(Msg::Unqueue);
            }
            st.loop_at = u64::MAX;
            st.resume_at = None;
            st.waiting = None;
            st.seek_after = None;
            st.generation += 1;
            st.arm_epoch += 1;
            st.prewarmed.take()
        };
        drop(prewarmed);
        self.shared.seek_pending.store(false, Ordering::Release);
        self.shared.seek_target_bits.store(f64::NAN.to_bits(), Ordering::Release);
        crate::aelog!("[RADIO] what plays fades out; the device stays open for the stream");
        true
    }

    /// The stream on the air failed (a format the decoder gave up on after
    /// a reconnect, …): stopped with its error, as a failed start is.
    pub(super) fn radio_watch(&self) {
        let cur = self.radio.lock().unwrap().as_ref().map(|(g, s)| (*g, s.clone()));
        if let Some((gen, s)) = cur {
            if let Some((kind, error)) = s.failure() {
                self.radio_failed(gen, kind, error);
            }
        }
    }

    pub(super) fn radio_failed(&self, gen: u64, kind: &'static str, error: String) {
        if self.radio_session(gen).is_none() {
            return;
        }
        self.radio_stop();
        // What is on the air for this stream ends: its chain, or the output
        // held for it (silent since the stream was asked for). A track the
        // listener chose meanwhile has taken the session away already.
        if self.st.lock().unwrap().output.is_some() {
            self.fade_out();
            self.stop_now();
        }
        self.set_pending(None);
        if kind != crate::player::radio::STOPPED {
            self.radio_fail(kind, error);
        }
    }
}
#[cfg(test)]
mod switching_tests {
    use super::*;

    /// A build over clears the mark the page's bar runs on only when it is
    /// the build running: an earlier one ending leaves a later one's.
    #[test]
    fn a_later_build_keeps_the_mark() {
        SWITCHING.store(70_001, Ordering::Release);
        switch_over(70_000);
        assert_eq!(SWITCHING.load(Ordering::Acquire), 70_001);
        switch_over(70_001);
        assert_eq!(SWITCHING.load(Ordering::Acquire), 0);
    }
}
