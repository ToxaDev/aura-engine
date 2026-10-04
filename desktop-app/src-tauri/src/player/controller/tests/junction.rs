//! The track junction: only a chain of the stream's own rate and mode goes
//! on gapless, and BIT-PERFECT plays a remembered track bit-perfect.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::super::{
    effective_settings, get, playing_and_next, variant_key, wake_before_end, Mode, Msg, Phase, PlayState, PlayerSettings,
    Prewarmed, RepeatMode, Variant, QUEUE_AHEAD_S,
};
use super::{dummy_desc, make_dummy_chain, on_air, player_turn, track, worker_quiet, Saved};

/// A full variant for `s` of a source at `rate`, played at `rate × l`.
fn variant(s: &PlayerSettings, rate: u32, l: usize, direct: bool) -> Arc<Variant> {
    Arc::new(Variant {
        stream: None,
        key: variant_key(s),
        src: Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate }),
        out_rate: rate * l as u32,
        l,
        tp_target_dbtp: -0.5,
        tp_pred_lin: 0.1,
        lim_local: true,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct,
        source_id: String::new(),
    })
}

/// Only a chain of the stream's own rate and mode goes on gapless. A
/// BIT-PERFECT stream at 88.2 kHz (a hi-res track played bit-perfect, the
/// switch to the Aura rack not landed yet) and a CD-rate next track whose
/// Aura chain plays at 88.2 kHz too (FS×2): that chain is not queued into
/// the stream, which has no volume and played it at full scale; the track
/// starts after a device reopen. A stream of its mode takes it (it goes on
/// to be built, and stops here at its filter: tests have none).
#[test]
fn a_bit_perfect_stream_takes_no_gapless_aura_chain() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(991_101), track(991_102));
    x.duration_s = 60.0;
    // 5k taps: no power pre-trial for so small a filter.
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let v = variant(&aura, 44_100, 2, false);
    // 10 s left of x, y's chain built ahead: the render thread's turn.
    let reset = |direct: bool| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(x.id);
            st.settings = aura.clone();
            st.queued_next = None;
            st.variants.insert((y.id, variant_key(&aura)), v.clone());
            st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        }
        on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
        let mut st = p.st.lock().unwrap();
        st.stream_direct = direct;
        st.play = PlayState::Stopped;
    };

    reset(true);
    p.queue_next(y.id);
    let into_bit_perfect = p.st.lock().unwrap().queued_next;
    reset(false);
    p.queue_next(y.id);
    let into_dsp = p.st.lock().unwrap().queued_next;

    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    // A chain that was built after all (filters found) leaves the render.
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    saved.restore(&p);

    assert_eq!(into_bit_perfect, Some(u64::MAX), "a BIT-PERFECT stream: the next track starts on a stream of its own");
    assert_eq!(into_dsp, Some(y.id), "a DSP stream of its rate takes it gapless");
}

/// The same through the tick, the way it happens: BIT-PERFECT has just been
/// turned off (the rack is Aura again), its switch has not landed (the
/// stream is still the direct one), and the track on the air is in its last
/// 40 s. The prefetch finds the next track's Aura variant ready and would
/// queue it into the direct stream: if the track ended before the switch
/// landed, that chain played there at full scale. It starts after the
/// reopen instead.
#[test]
fn bit_perfect_just_off_the_tick_queues_no_aura_chain_into_its_stream() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(991_301), track(991_302));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = None;
        st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
    }
    // x on the air 50 s in: 10 s left, the prefetch's window.
    on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
    p.st.lock().unwrap().stream_direct = true;
    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    p.tick();
    let queued = p.st.lock().unwrap().queued_next;

    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    saved.restore(&p);

    assert_eq!(queued, Some(u64::MAX), "the next track waits for a stream of its own");
}

/// BIT-PERFECT plays every track bit-perfect, a remembered one too: its M
/// rack (always an Aura one) waits until BIT-PERFECT is off. The hand-over
/// to it leaves the rack as it is: it made the M rack the rack, and
/// BIT-PERFECT was off in the player while the page showed it on.
#[test]
fn bit_perfect_is_above_a_remembered_rack() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    // Ids no queue holds: tick() stops right after the follow block.
    let (x, y) = (991_201u64, 991_202u64);
    let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
    let m_rack = PlayerSettings { mode: Mode::Aura, phase: Phase::Hybrid, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.settings = direct.clone();
        st.current = Some(x);
    }
    p.set_track_settings(y, m_rack.clone());
    let seq0 = p.st.lock().unwrap().track_settings_applied_seq;
    let under_bp = effective_settings(y, &p.st.lock().unwrap());
    // The hand-over to y is on the air, in the BIT-PERFECT stream.
    on_air(&p, y, 0, 44_100, dummy_desc());
    p.st.lock().unwrap().stream_direct = true;
    p.tick();
    let (cur, rack, seq) = {
        let st = p.st.lock().unwrap();
        (st.current, st.settings.clone(), st.track_settings_applied_seq)
    };
    // BIT-PERFECT off: the remembered rack is the track's again.
    p.st.lock().unwrap().settings = PlayerSettings::default();
    let bp_off = effective_settings(y, &p.st.lock().unwrap());

    p.clear_track_settings(y);
    p.st.lock().unwrap().track_settings_applied_seq = seq0;
    saved.restore(&p);

    assert_eq!(under_bp, direct, "under BIT-PERFECT the remembered track plays bit-perfect");
    assert_eq!(cur, Some(y), "the hand-over is followed");
    assert_eq!(rack, direct, "the rack stays BIT-PERFECT");
    assert_eq!(seq, seq0, "nothing for the page to take over");
    assert_eq!(bp_off, m_rack, "BIT-PERFECT off: the remembered rack");
}

// ── The prewarm ─────────────────────────────────────────────────────────

/// The next track's chain built ahead is kept until QUEUE_AHEAD_S before
/// the end — ⏭ takes it meanwhile — then handed to the render thread for
/// the gapless hand-over. A stream that cannot take it (another rate or
/// mode) leaves it waiting for the device's reopen at the end.
#[test]
fn a_prewarmed_chain_goes_on_gapless_near_the_end_or_waits_for_the_reopen() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(991_401), track(991_402));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let at = |secs: u64, direct: bool| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(x.id);
            st.settings = aura.clone();
            st.queued_next = None;
            st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
            if st.prewarmed.is_none() {
                st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
            }
        }
        on_air(&p, x.id, secs * 88_200, 88_200, dummy_desc());
        let mut st = p.st.lock().unwrap();
        st.stream_direct = direct;
        // The worker's own tick stays out of it.
        st.play = PlayState::Stopped;
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.as_ref().map(|w| w.id))
    };

    at(20, false);
    p.queue_next(y.id);
    let early = state();
    at(60 - QUEUE_AHEAD_S as u64 + 2, false);
    p.queue_next(y.id);
    let near = state();
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    at(20, true);
    p.queue_next(y.id);
    let reopen = state();

    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    saved.restore(&p);

    assert_eq!(early, (None, Some(y.id)), "40 s before the end: kept here, for ⏭");
    assert_eq!(near, (Some(y.id), None), "in the last QUEUE_AHEAD_S: to the render thread, gapless");
    assert_eq!(reopen, (Some(u64::MAX), Some(y.id)), "a stream of another mode: kept for the reopen at the end");
}

/// A start takes the chain built ahead for that track and rack (the reopen
/// at the end, or ⏭); a start of another track, or with another rack,
/// lets it go.
#[test]
fn a_start_takes_the_chain_built_ahead_for_it_only() {
    let _turn = player_turn();
    let p = get();
    let aura = PlayerSettings { mode: Mode::Aura, phase: Phase::Linear, ..PlayerSettings::default() };
    let other = PlayerSettings { phase: Phase::Hybrid, ..aura.clone() };
    let put = || p.st.lock().unwrap().prewarmed = Some(Prewarmed { id: 991_502, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    let left = || p.st.lock().unwrap().prewarmed.is_some();

    put();
    let taken = p.take_prewarmed(991_502, &aura).is_some();
    let after_taken = left();
    put();
    let other_track = p.take_prewarmed(991_503, &aura).is_some();
    let after_other_track = left();
    put();
    let other_rack = p.take_prewarmed(991_502, &other).is_some();
    let after_other_rack = left();

    assert!(taken && !after_taken, "its own: taken");
    assert!(!other_track && !after_other_track, "another track's start: let go");
    assert!(!other_rack && !after_other_rack, "another rack: let go");
}

/// The chain built ahead for a track that is not the next one any more, or
/// for a rack it no longer has, is let go (it can hold gigabytes, on the
/// video card too); one that waited for the reopen gets the next one
/// prepared again.
#[test]
fn a_chain_built_ahead_for_another_track_or_rack_is_let_go() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    // BIT-PERFECT: nothing of the next track is prepared after the look.
    let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let (y, z) = (track(991_602), track(991_603));
    let set = |settings: &PlayerSettings| {
        let mut st = p.st.lock().unwrap();
        st.settings = direct.clone();
        st.play = PlayState::Stopped;
        st.queued_next = Some(u64::MAX);
        st.prewarmed = Some(Prewarmed { id: y.id, settings: settings.clone(), chain: make_dummy_chain(), stand_in: false });
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some())
    };

    set(&aura);
    p.prewarm(Some(y.clone()), 10.0);
    let rack_changed = state();
    set(&direct);
    p.prewarm(Some(z.clone()), 10.0);
    let next_changed = state();
    set(&direct);
    p.prewarm(Some(y.clone()), 10.0);
    let same = state();
    let still = effective_settings(y.id, &p.st.lock().unwrap()) == direct;

    saved.restore(&p);

    assert_eq!(rack_changed, (None, false), "another rack: let go, and the next track prepared again");
    assert_eq!(next_changed, (None, false), "another next track: let go");
    assert_eq!(same, (Some(u64::MAX), true), "the same track and rack: kept for the reopen");
    assert!(still);
}

/// Stopping the stream lets go of what was queued in it — the render drops
/// it at Msg::Stop — and the tick queues the next track again for the
/// stream after it. It stayed marked, and after a rate switch the next
/// track came as a jump, not gapless.
#[test]
fn a_stopped_stream_takes_its_queue_with_it() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let mark = |q: u64| {
        let mut st = p.st.lock().unwrap();
        st.queued_next = Some(q);
        st.loop_at = 1234;
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.loop_at)
    };

    mark(991_701);
    p.stop_stream();
    let queued = state();
    mark(u64::MAX);
    p.stop_stream();
    let reopen = state();

    saved.restore(&p);

    assert_eq!(queued, (None, u64::MAX), "the chain queued in it goes with it");
    assert_eq!(reopen, (None, u64::MAX), "a track that waited for a reopen is looked at again for the new stream");
}

/// A track written to its end with nothing to follow it gapless: the worker
/// comes back when the reader gets there, not up to a tick later.
#[test]
fn the_worker_wakes_when_the_reader_reaches_the_end() {
    assert_eq!(wake_before_end(u64::MAX, 0, 88_200), None, "not written to its end yet");
    assert_eq!(wake_before_end(1_000 + 4_410, 1_000, 88_200), Some(52), "50 ms left");
    assert_eq!(wake_before_end(1_000 + 88_200, 1_000, 88_200), None, "a second left: the tick is soon enough");
    assert_eq!(wake_before_end(1_000, 2_000, 88_200), Some(2), "the reader is there");
}

// ── A prewarm late for the end (review of dc8d643, MAJOR 1) ─────────────

/// (a) Instant start on, the end near (QUEUE_AHEAD_S or less) and the next
/// track's envelope not made in time: it goes on gapless on its linear
/// stand-in, the pair following through §5b, as before the prewarm. It
/// waited for the envelope, and the end came first: a start in silence.
#[test]
fn instant_on_near_the_end_the_next_goes_on_its_stand_in() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(991_801), track(991_802));
    x.duration_s = 60.0;
    let hp = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Hybrid, ..PlayerSettings::default() };
    let key = (y.id, variant_key(&hp));
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = hp.clone();
        st.queued_next = None;
        st.variants.insert(key.clone(), variant(&hp, 44_100, 2, false));
        // Its envelope is still being made.
        st.in_flight.insert(key.clone());
        st.prep_cancel.insert(key.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
    }
    on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
    {
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        st.play = PlayState::Stopped;
    }
    p.prewarm(Some(y.clone()), 10.0);
    // The stand-in's build begins (and stops at its filter here: tests have none).
    let built = super::wait_until(|| {
        let st = p.st.lock().unwrap();
        st.prewarm_building.as_ref().map(|b| b.0) == Some(y.id) || st.prewarm_failed.as_ref().map(|b| b.0) == Some(y.id)
    });

    {
        let mut st = p.st.lock().unwrap();
        st.in_flight.remove(&key);
        st.prep_cancel.remove(&key);
        st.variants.retain_tracks(|id| id != y.id);
    }
    super::wait_until(|| p.st.lock().unwrap().prewarm_building.is_none());
    saved.restore(&p);

    assert!(built, "10 s before the end, no envelope: the stand-in is built for the hand-over");
}

/// (b) The end of a track while the chain of the next one is being built
/// ahead: the end waits for that build — Prefetched queues it, or leaves it
/// for the reopen that then takes it — rather than start the next track and
/// build its chain a second time (a pause as long as the whole build, two
/// copies, each bank loaded twice).
#[test]
fn the_end_waits_for_the_chain_being_built_ahead() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (x, y) = (track(991_901), track(991_902));
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = None;
        st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
        st.prewarm_building = Some((y.id, aura.clone()));
    }
    on_air(&p, x.id, 0, 88_200, dummy_desc());
    // x has run out under the reader.
    p.shared.ended_at.store(0, Ordering::Relaxed);
    p.tick();
    let cur = p.st.lock().unwrap().current;

    {
        let mut st = p.st.lock().unwrap();
        st.prewarm_building = None;
        st.variants.retain_tracks(|id| id != y.id);
    }
    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    p.out.hold.store(false, Ordering::Release);
    saved.restore(&p);

    assert_eq!(cur, Some(x.id), "the end waits for the build: the next track is not started on a chain of its own");
}

/// (c) A track that has started already is nobody's next: ⏭ to it while
/// its chain waited here (the reader still under the track before), or a
/// start landing (the output held). Its chain went into the render behind
/// the track before, and it played twice.
#[test]
fn a_started_track_is_not_queued_behind_the_one_before() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(992_001), track(992_002));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let set = |current: u64| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(current);
            st.settings = aura.clone();
            st.queued_next = None;
            st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
            st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        }
        // 10 s left of x: the chain would go to the render now.
        on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        st.play = PlayState::Stopped;
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some())
    };

    set(y.id);
    p.queue_next(y.id);
    let started = state();
    set(x.id);
    p.out.hold.store(true, Ordering::Release);
    p.queue_next(y.id);
    let held = state();
    p.out.hold.store(false, Ordering::Release);

    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    saved.restore(&p);

    assert_eq!(started, (None, true), "started already (⏭): not queued behind the track before");
    assert_eq!(held, (None, true), "a start landing: nothing is queued");
}

// ── What follows: repeat all, a track removed or added (review of dc8d643, MAJOR 2–3) ──

/// MAJOR 2. With repeat all the first track follows the last one: its chain
/// built ahead goes on gapless like any next track's. It was prepared, never
/// queued, and after the end of the list it started on a chain built then (a
/// pause; with Instant start off, silence). A list of one loops like repeat
/// one.
#[test]
fn repeat_all_queues_the_first_track_after_the_last() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let repeat0 = p.st.lock().unwrap().repeat;
    let (x, mut y) = (track(992_101), track(992_102));
    y.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    // `follows` of `y` in `list`: its chain built ahead, y on the air with
    // 10 s left (the render thread's turn).
    let at = |list: Vec<super::super::TrackInfo>, follows: u64| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = list;
            st.current = Some(y.id);
            st.repeat = RepeatMode::All;
            st.settings = aura.clone();
            st.queued_next = None;
            st.loop_at = u64::MAX;
            st.variants.insert((follows, variant_key(&aura)), variant(&aura, 44_100, 2, false));
            st.prewarmed = Some(Prewarmed { id: follows, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        }
        on_air(&p, y.id, 50 * 88_200, 88_200, dummy_desc());
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        // The worker's own tick stays out of it.
        st.play = PlayState::Stopped;
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some(), st.loop_at != u64::MAX)
    };

    at(vec![x.clone(), y.clone()], x.id);
    p.queue_next(x.id);
    let wrap = state();
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    at(vec![y.clone()], y.id);
    p.queue_next(y.id);
    let alone = state();
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);

    {
        let mut st = p.st.lock().unwrap();
        st.variants.retain_tracks(|id| id != x.id && id != y.id);
        st.repeat = repeat0;
        st.loop_at = u64::MAX;
    }
    saved.restore(&p);

    assert_eq!(wrap, (Some(x.id), false, false), "the last track: the first goes on gapless");
    assert_eq!(alone, (Some(y.id), false, true), "a list of one: its loop");
}

/// MAJOR 2. The caches keep the playing track's and the next one's variants
/// and envelopes: with repeat all, after the last track, the first one's —
/// the one prepared to follow it.
#[test]
fn the_caches_keep_the_first_track_after_the_last_with_repeat_all() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let repeat0 = p.st.lock().unwrap().repeat;
    let (x, y) = (track(992_201), track(992_202));
    let kept = |repeat: RepeatMode| {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(y.id);
        st.repeat = repeat;
        playing_and_next(&st)
    };

    let all = kept(RepeatMode::All);
    let off = kept(RepeatMode::Off);

    p.st.lock().unwrap().repeat = repeat0;
    saved.restore(&p);

    assert_eq!(all, vec![y.id, x.id], "repeat all: the first follows the last");
    assert_eq!(off, vec![y.id], "repeat off: nothing follows the last");
}

/// MAJOR 3. A track removed from the list (by hand, or with the playlist
/// switched) while its chain was queued for the gapless hand-over: that
/// chain is let go. It played, then the tick found no such track and nothing
/// started — silence while playing. Removed while it waited for the reopen:
/// the marker goes too, or no next track was prepared again. Another track
/// removed, or the playing one, leaves what follows as it is.
#[test]
fn a_removed_next_track_is_let_go_from_the_hand_over() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let repeat0 = p.st.lock().unwrap().repeat;
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let (x, y, z) = (track(992_301), track(992_302), track(992_303));
    let set = |queued: u64| {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone(), z.clone()];
        st.current = Some(x.id);
        st.repeat = RepeatMode::Off;
        st.play = PlayState::Stopped;
        st.queued_next = Some(queued);
        st.loop_at = u64::MAX;
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    };
    let queued = || p.st.lock().unwrap().queued_next;

    set(y.id);
    p.remove(y.id);
    let next_removed = queued();
    set(u64::MAX);
    p.remove(y.id);
    let reopen_removed = queued();
    set(y.id);
    p.remove(z.id);
    let other_removed = queued();
    set(y.id);
    p.remove(x.id);
    let playing_removed = queued();
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);

    p.st.lock().unwrap().repeat = repeat0;
    saved.restore(&p);

    assert_eq!(next_removed, None, "the next track removed: its gapless chain is let go");
    assert_eq!(reopen_removed, None, "removed while it waited for the reopen: the next one is prepared again");
    assert_eq!(other_removed, Some(y.id), "another track removed: the next one stays");
    assert_eq!(playing_removed, Some(y.id), "the playing track removed: the next one still follows it");
}

/// The queued next is let go when it is removed even if the track on
/// the air is no longer in the list (it was removed before it).
/// `let_go_unless_next` exits early when the current track is gone, so
/// the second removal used to leave `queued_next` pointing at the removed
/// track — it played gapless, then the player stopped.
#[test]
fn remove_lets_go_of_queued_next_when_current_left_the_list() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let (x, y) = (track(994_101), track(994_102));
    {
        let mut st = p.st.lock().unwrap();
        // x is current but already gone from the list; y is queued next and
        // still there — as after «+ New playlist» removes A first, then B.
        st.queue = vec![y.clone()];
        st.current = Some(x.id);
        st.play = PlayState::Stopped;
        st.queued_next = Some(y.id);
        st.loop_at = u64::MAX;
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    }
    p.remove(y.id);
    let after = p.st.lock().unwrap().queued_next;

    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    saved.restore(&p);

    assert_eq!(after, None, "queued next removed while current is already gone: let go");
}

/// A short silent WAV in the temp folder, for `add`.
fn silent_wav(name: &str) -> String {
    let path = std::env::temp_dir().join(name);
    let n = 4_410u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n * 4).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n * 4).to_le_bytes());
    b.resize(b.len() + (n * 4) as usize, 0);
    std::fs::write(&path, b).unwrap();
    path.to_string_lossy().into_owned()
}

/// MAJOR 2, the rule's other ends. With repeat all on the last track the
/// first one is queued to follow it: a track added after the last one
/// follows it instead, and repeat turned off leaves nothing to follow — the
/// first one queued played anyway. A list of one keeps its loop while repeat
/// all is asked again.
#[test]
fn what_follows_the_last_track_moves_with_the_list_and_the_repeat() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let repeat0 = p.st.lock().unwrap().repeat;
    let (x, y) = (track(992_401), track(992_402));
    let set = |list: Vec<super::super::TrackInfo>, queued: u64| {
        let mut st = p.st.lock().unwrap();
        st.queue = list;
        st.current = Some(y.id);
        st.repeat = RepeatMode::All;
        st.play = PlayState::Stopped;
        st.queued_next = Some(queued);
        st.loop_at = u64::MAX;
    };
    let queued = || p.st.lock().unwrap().queued_next;
    let wav = silent_wav("aura-junction-992403.wav");

    set(vec![x.clone(), y.clone()], x.id);
    let (added, errors) = p.add(vec![wav.clone()]);
    let after_add = queued();
    set(vec![x.clone(), y.clone()], x.id);
    p.set_repeat(RepeatMode::Off);
    let repeat_off = queued();
    set(vec![y.clone()], y.id);
    p.set_repeat(RepeatMode::All);
    let alone = queued();
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);

    p.st.lock().unwrap().repeat = repeat0;
    saved.restore(&p);
    let _ = std::fs::remove_file(&wav);

    assert!(errors.is_empty() && added.len() == 1, "the WAV is read: {errors:?}");
    assert_eq!(after_add, None, "a track added after the last one: the first one queued is let go");
    assert_eq!(repeat_off, None, "repeat off on the last track: nothing follows it");
    assert_eq!(alone, Some(y.id), "a list of one: its loop stays");
}

// ── The review's MINOR 4 and 8 ──────────────────────────────────────────

/// MINOR 4. A chain built ahead while the track before held the video card
/// back (its GPU failure, a conversion) went to the CPU with fewer taps, and
/// played the whole track so: its start, which would have them all, lets it
/// go and builds its own. One with all its taps stays, without a look at the
/// route (on the CPU too: the same sound, where the pair on the video card is
/// seconds of silence to build).
#[test]
fn a_chain_built_ahead_short_of_taps_is_let_go_at_its_start() {
    let _turn = player_turn();
    let p = get();
    // 5k taps on the CPU: the route never takes taps away from so small a filter.
    let s = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, use_gpu: false, ..PlayerSettings::default() };
    let v = variant(&s, 44_100, 2, false);
    let whole = make_dummy_chain();
    let downgraded = |taps: usize| {
        let mut c = make_dummy_chain();
        c.downgrade = Some(crate::player::chain::DowngradeInfo::below(None, "30M".into(), "10M".into(), "power"));
        c.settings = Arc::new(PlayerSettings { taps, ..s.clone() });
        c
    };

    let kept = p.prewarmed_on_route(whole, &s, &v, "t992501:missing.flac");
    let let_go = p.prewarmed_on_route(downgraded(2_500), &s, &v, "t992501:missing.flac");
    // Downgraded once, as far as the start would be (review 2, MINOR 4a).
    let as_many = p.prewarmed_on_route(downgraded(5_000), &s, &v, "t992501:missing.flac");

    assert!(kept.is_ok(), "all its taps: taken");
    assert!(as_many.is_ok(), "as many taps as the start would have: taken");
    match let_go {
        Ok(_) => panic!("short of taps the start has: it must be let go"),
        Err(route) => assert!(route.2.is_none() && route.0.taps == 5_000, "the start builds all its taps"),
    }
}

/// MINOR 8. A converted file for the next track lets go of its chain built
/// ahead, and of the marker of its wait for the reopen: the marker stayed,
/// and the next track was never looked at again.
#[test]
fn a_converted_next_track_lets_go_of_its_wait_for_the_reopen() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let (x, y) = (track(992_701), track(992_702));
    let map0 = {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.play = PlayState::Stopped;
        st.queued_next = Some(u64::MAX);
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        st.rendered_map.clone()
    };

    let mut items: Vec<(u64, Option<String>)> = map0.iter().map(|(id, f)| (*id, Some(f.clone()))).collect();
    items.push((y.id, Some("y-converted.flac".into())));
    p.set_rendered(items);
    let after = {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some())
    };

    p.st.lock().unwrap().rendered_map = map0;
    saved.restore(&p);

    assert_eq!(after, (None, false), "the chain and the marker go: the next track is looked at again");
}

/// MINOR 8. A seek back out of the last PREWARM_S lets go of the chain
/// built ahead (gigabytes held to the end of the track); a seek just around
/// the edge keeps it (it is not built over and over).
#[test]
fn a_chain_built_ahead_is_let_go_when_the_end_is_far_again() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (x, y) = (track(992_801), track(992_802));
    let kept_at = |remaining: f64| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(x.id);
            st.settings = aura.clone();
            st.play = PlayState::Stopped;
            st.queued_next = Some(u64::MAX);
            st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        }
        p.prewarm(Some(y.clone()), remaining);
        let st = p.st.lock().unwrap();
        (st.prewarmed.is_some(), st.queued_next)
    };

    let edge = kept_at(50.0);
    let far = kept_at(100.0);

    saved.restore(&p);

    assert_eq!(edge, (true, Some(u64::MAX)), "around the edge: kept");
    assert_eq!(far, (false, None), "far from the end again: let go, and looked at again near it");
}

/// MINOR 8. A track that waits for its whole chain (Instant start off) lets
/// go of the chain built ahead for another track — the tick that lets it go
/// runs only with a stream — and a failed prewarm is tried again after a
/// start (it never was, for that track and rack).
#[test]
fn a_track_that_waits_lets_go_of_the_chain_built_for_another() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (a, b, c) = (track(992_901), track(992_902), track(992_903));
    let kc = (c.id, variant_key(&aura));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![a.clone(), b.clone(), c.clone()];
        st.current = Some(a.id);
        st.settings = aura.clone();
        st.play = PlayState::Stopped;
        st.prewarmed = Some(Prewarmed { id: b.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        st.prewarm_failed = Some((b.id, aura.clone()));
        // C's preparation is under way (no thread is started here).
        st.in_flight.insert(kc.clone());
        st.prep_cancel.insert(kc.clone(), cancel.clone());
    }
    let off = super::InstantOff::new();

    p.start_track(c.id, 0.0);
    let (waits, kept, failed) = {
        let st = p.st.lock().unwrap();
        (st.waiting.map(|w| w.0), st.prewarmed.is_some(), st.prewarm_failed.is_some())
    };
    p.stop_now();
    drop(off);

    {
        let mut st = p.st.lock().unwrap();
        st.in_flight.remove(&kc);
        st.prep_cancel.remove(&kc);
    }
    saved.restore(&p);

    assert_eq!(waits, Some(c.id), "C waits for its whole chain");
    assert!(!kept, "B's chain built ahead is let go");
    assert!(!failed, "B's failed prewarm may be tried again");
}

/// The render has written the track to its end while the length the probe
/// read leaves 20 s (a lossy file's estimate can be longer than the audio):
/// the chain built ahead goes to the render now. It waited for an end that
/// was not there, and the next track came after it as a jump.
#[test]
fn a_track_written_to_its_end_takes_the_chain_built_ahead_now() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(993_001), track(993_002));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = None;
        st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    }
    // 20 s left by the probe's length.
    on_air(&p, x.id, 40 * 88_200, 88_200, dummy_desc());
    {
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        st.play = PlayState::Stopped;
    }
    // The render has written x to its end.
    p.shared.ended_at.store(1, Ordering::Relaxed);
    p.queue_next(y.id);
    let state = {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some())
    };

    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    saved.restore(&p);

    assert_eq!(state, (Some(y.id), false), "written to its end: the chain goes on gapless now");
}

// ── A seek in the last seconds (second review of c8a30f2, MAJOR 1) ──────

/// (a) A seek's swap drops the render thread's gapless next (CR-5): what
/// was queued goes with it, as the HP swap and K3 know. It stayed marked,
/// and the end of the track waited for it — silence, playing, until ⏭.
#[test]
fn a_seek_lets_go_of_the_next_its_swap_drops() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(993_101), track(993_102));
    x.duration_s = 60.0;
    let chain = make_dummy_chain();
    let rate = chain.out_rate;
    p.out.hold.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.queued_next = Some(y.id);
        st.loop_at = 1234;
    }
    on_air(&p, x.id, 55 * rate as u64, rate, dummy_desc());
    let generation = {
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        st.play = PlayState::Stopped;
        st.generation
    };
    p.seek_ready(chain, (vec![0.0; 16], vec![0.0; 16]), generation);
    let after = {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.loop_at)
    };

    // The swap waits in the render thread for a stream of its own: let it go.
    let _ = p.render_tx.lock().unwrap().send(Msg::Stop);
    saved.restore(&p);

    assert_eq!(after, (None, u64::MAX), "the seek's swap took the queued next with it");
}

/// (b) While a seek of this generation is on its way, the chain built ahead
/// stays here: its swap would drop it from the render thread.
#[test]
fn the_chain_built_ahead_waits_for_a_seek_on_its_way() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(993_201), track(993_202));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = None;
        st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    }
    // 10 s left: the render thread's turn — but a seek is on its way.
    on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
    {
        let mut st = p.st.lock().unwrap();
        st.stream_direct = false;
        st.play = PlayState::Stopped;
        st.seek_gen = Some(st.generation);
    }
    p.queue_next(y.id);
    let state = {
        let st = p.st.lock().unwrap();
        (st.queued_next, st.prewarmed.is_some())
    };

    let _ = p.render_tx.lock().unwrap().send(Msg::Unqueue);
    {
        let mut st = p.st.lock().unwrap();
        st.seek_gen = None;
        st.variants.retain_tracks(|id| id != y.id);
    }
    saved.restore(&p);

    assert_eq!(state, (None, true), "kept here until the seek has landed");
}

/// (c) The end of a track waits for a queued chain only while it is on its
/// way to the render thread. One the render has received and let go (a
/// seek's swap) is not coming: the next track starts. It waited for it for
/// good.
#[test]
fn the_end_does_not_wait_for_a_chain_the_render_let_go() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (x, y) = (track(993_301), track(993_302));
    let ky = (y.id, variant_key(&aura));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        // Queued, received, and let go: the render's queue is empty.
        st.queued_next = Some(y.id);
        // Y's preparation is under way (no thread is started here).
        st.in_flight.insert(ky.clone());
        st.prep_cancel.insert(ky.clone(), cancel.clone());
    }
    p.shared.queued.store(false, Ordering::Release);
    let off = super::InstantOff::new();
    on_air(&p, x.id, 0, 88_200, dummy_desc());
    // x has run out under the reader.
    p.shared.ended_at.store(0, Ordering::Relaxed);
    p.tick();
    let cur = p.st.lock().unwrap().current;
    p.stop_now();
    drop(off);

    {
        let mut st = p.st.lock().unwrap();
        st.in_flight.remove(&ky);
        st.prep_cancel.remove(&ky);
    }
    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    saved.restore(&p);

    assert_eq!(cur, Some(y.id), "the next track starts (here: waits for its whole chain)");
}

/// (c) A chain sent and not yet received by the render thread is on its
/// way: the end of the track waits for it (the render goes on with it).
#[test]
fn the_end_waits_for_a_chain_on_its_way_to_the_render() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (x, y) = (track(993_351), track(993_352));
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = Some(y.id);
        // Sent, not received yet: a send no Msg::Queue went with.
        st.queue_sent += 1;
    }
    on_air(&p, x.id, 0, 88_200, dummy_desc());
    p.shared.ended_at.store(0, Ordering::Relaxed);
    p.tick();
    let cur = p.st.lock().unwrap().current;

    // The send is taken back: no Msg::Queue of that seq ever reaches the
    // render thread. Left in place, a chain was "on its way" until the next
    // real one was received, and a test that came in between with a chain
    // the render had let go (the_end_does_not_wait_for_a_chain_the_render_let_go)
    // waited for it for good.
    p.st.lock().unwrap().queue_sent -= 1;
    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    saved.restore(&p);

    assert_eq!(cur, Some(x.id), "on its way: the end waits for it");
}

/// (c) The render thread's late path lands between the tick's two reads at
/// the end of a track: it goes on with the chain at the end (ended_at back to
/// "not written out") and then says it has it. An end read before that and a
/// receipt read after it started the next track again, from its start, over
/// its own gapless start. The receipt is read first: one seen comes with the
/// end it cleared.
#[test]
fn a_chain_received_between_the_ticks_two_reads_is_not_started_again() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (x, y) = (track(993_361), track(993_362));
    let ky = (y.id, variant_key(&aura));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    let seq = {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = Some(y.id);
        // Sent, not received yet (no Msg::Queue goes with it: the render's
        // late path below is the test's own).
        st.queue_sent += 1;
        // A start of y waits for its preparation (no thread is started here).
        st.in_flight.insert(ky.clone());
        st.prep_cancel.insert(ky.clone(), cancel.clone());
        st.queue_sent
    };
    let off = super::InstantOff::new();
    on_air(&p, x.id, 0, 88_200, dummy_desc());
    // x has run out under the reader.
    p.shared.ended_at.store(0, Ordering::Relaxed);
    // The render's late path, right between the two reads: y goes on at x's
    // end, then the receipt — sent and received, as a real send leaves them.
    let shared = p.shared.clone();
    super::super::BETWEEN_END_READS.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            shared.ended_at.store(u64::MAX, Ordering::Relaxed);
            shared.queue_taken.store(seq, Ordering::Release);
        }))
    });
    p.tick();
    let cur = p.st.lock().unwrap().current;
    p.stop_now();
    drop(off);

    {
        let mut st = p.st.lock().unwrap();
        st.in_flight.remove(&ky);
        st.prep_cancel.remove(&ky);
    }
    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    saved.restore(&p);

    assert_eq!(cur, Some(x.id), "y went on gapless at x's end: the tick does not start it again");
}

// ── The second review's MINOR and small points ──────────────────────────

/// MINOR 2. A linear stand-in built ahead with Instant start on (the
/// envelope late) is not a whole chain: with Instant start off, the start
/// builds the pair rather than play the stand-in.
#[test]
fn a_stand_in_built_with_instant_on_is_not_taken_with_it_off() {
    let _turn = player_turn();
    let p = get();
    let hp = PlayerSettings { mode: Mode::Aura, phase: Phase::Hybrid, ..PlayerSettings::default() };
    let put = || {
        let mut c = make_dummy_chain();
        c.hp_deferred = true;
        p.st.lock().unwrap().prewarmed = Some(Prewarmed { id: 993_402, settings: hp.clone(), chain: c, stand_in: true });
    };

    put();
    let on = p.take_prewarmed(993_402, &hp).is_some();
    put();
    let off = {
        let _off = super::InstantOff::new();
        p.take_prewarmed(993_402, &hp).is_some()
    };
    p.st.lock().unwrap().prewarmed = None;

    assert!(on, "Instant start on: its stand-in starts, the pair follows");
    assert!(!off, "Instant start off: the start builds the pair");
}

/// A whole linear chain built ahead with the HP envelope not yet done
/// (`hp_deferred = true`, `stand_in = false`: it came from `complete_variant`,
/// not the stand-in variant) is kept and taken even with Instant start off —
/// it is a whole chain, just without its HP pair yet (§5b handles that).  A
/// chain that really is a stand-in (`stand_in = true`) is still rejected.
///
/// Regression of the MINOR 2 fix: before, `stand_in_off` / `take_prewarmed`
/// checked `hp_deferred`, not `stand_in`, so every chain whose envelope was
/// still pending looked like a stand-in and was dropped in a 40→12 s loop.
#[test]
fn a_whole_hp_deferred_chain_is_kept_with_instant_off_a_stand_in_is_not() {
    let _turn = player_turn();
    let p = get();
    let hp = PlayerSettings { mode: Mode::Aura, phase: Phase::Hybrid, ..PlayerSettings::default() };
    let put = |si: bool| {
        let mut c = make_dummy_chain();
        c.hp_deferred = true;
        p.st.lock().unwrap().prewarmed = Some(Prewarmed { id: 994_002, settings: hp.clone(), chain: c, stand_in: si });
    };
    let _off = super::InstantOff::new();

    put(false);
    let whole_taken = p.take_prewarmed(994_002, &hp).is_some();
    put(true);
    let stand_in_taken = p.take_prewarmed(994_002, &hp).is_some();
    p.st.lock().unwrap().prewarmed = None;

    assert!(whole_taken, "whole chain (env pending), Instant start off: taken as-is");
    assert!(!stand_in_taken, "linear stand-in, Instant start off: rejected, start builds the pair");
}

/// MINOR 3. A chain built on the video card while the track on the air
/// failed there (K4), or with a fallback since its route was chosen, is not
/// kept; a chain on the CPU, or one built on a GPU that has not failed, is.
#[test]
fn a_chain_built_on_a_failing_video_card_is_not_kept() {
    let _turn = player_turn();
    let p = get();
    let mut gpu = make_dummy_chain();
    gpu.gpu_on = true;
    let cpu = make_dummy_chain();
    let gen = p.shared.gpu_fallback_gen.load(Ordering::Relaxed);

    p.shared.gpu_failed.store(true, Ordering::Relaxed);
    let failed = (p.built_on_a_failed_gpu(&gpu, gen), p.built_on_a_failed_gpu(&cpu, gen));
    p.shared.gpu_failed.store(false, Ordering::Relaxed);
    let fallback_since = p.built_on_a_failed_gpu(&gpu, gen.wrapping_add(1));
    let sound = p.built_on_a_failed_gpu(&gpu, gen);

    assert_eq!(failed, (true, false), "the GPU failed: the GPU chain goes, the CPU one stays");
    assert!(fallback_since, "a fallback since its route was chosen: it goes");
    assert!(!sound, "no failure: it stays");
}

/// MINOR 4b. A conversion starting lets go of a chain built ahead on the
/// video card (and of its wait for the reopen); one on the CPU stays.
#[test]
fn a_conversion_lets_go_of_a_chain_built_ahead_on_the_video_card() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let put = |gpu_on: bool| {
        let mut c = make_dummy_chain();
        c.gpu_on = gpu_on;
        let mut st = p.st.lock().unwrap();
        st.play = PlayState::Stopped;
        st.queued_next = Some(u64::MAX);
        st.prewarmed = Some(Prewarmed { id: 993_502, settings: aura.clone(), chain: c, stand_in: false });
    };
    let state = || {
        let st = p.st.lock().unwrap();
        (st.prewarmed.is_some(), st.queued_next)
    };

    put(true);
    p.let_go_of_gpu_prewarm();
    let gpu = state();
    put(false);
    p.let_go_of_gpu_prewarm();
    let cpu = state();

    saved.restore(&p);

    assert_eq!(gpu, (false, None), "on the GPU: let go, and looked at again");
    assert_eq!(cpu, (true, Some(u64::MAX)), "on the CPU: kept");
}

/// MINOR 5. The track on the air left the list (removed in its last
/// seconds, its chain written already): at its end the player stops. It
/// stayed playing over silence until ⏭ or Stop.
#[test]
fn a_track_that_left_the_list_stops_the_player_at_its_end() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (x, y) = (track(993_601), track(993_602));
    p.out.hold.store(false, Ordering::Release);
    p.apply_deferred.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        // y plays, and is not in the list any more.
        st.queue = vec![x.clone()];
        st.current = Some(y.id);
    }
    on_air(&p, y.id, 0, 88_200, dummy_desc());
    p.shared.ended_at.store(0, Ordering::Relaxed);
    p.tick();
    let play = p.st.lock().unwrap().play;

    p.shared.ended_at.store(u64::MAX, Ordering::Relaxed);
    saved.restore(&p);

    assert_eq!(play, PlayState::Stopped, "its end stops the player");
}

/// Small point 2. The next track waits for the reopen once its own chain is
/// there or being built: with another track's build under way its build did
/// not start, and the mark kept it from being tried again (a cold start).
#[test]
fn the_reopen_mark_waits_for_its_own_build() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(993_701), track(993_702));
    x.duration_s = 60.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    p.out.hold.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.queued_next = None;
        st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2, false));
        // Another track's build is under way.
        st.prewarm_building = Some((993_799, aura.clone()));
    }
    on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
    {
        // A BIT-PERFECT stream: the Aura chain waits for the reopen.
        let mut st = p.st.lock().unwrap();
        st.stream_direct = true;
        st.play = PlayState::Stopped;
    }
    p.queue_next(y.id);
    let queued = p.st.lock().unwrap().queued_next;

    {
        let mut st = p.st.lock().unwrap();
        st.prewarm_building = None;
        st.variants.retain_tracks(|id| id != y.id);
    }
    saved.restore(&p);

    assert_eq!(queued, None, "not marked: its own build is tried at the next tick");
}

/// Small point 5. A start from disk, as any start, lets a failed prewarm
/// be tried again.
#[test]
fn a_start_from_disk_tries_a_failed_prewarm_again() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, ..PlayerSettings::default() };
    let t = track(993_801);
    p.st.lock().unwrap().prewarm_failed = Some((993_802, aura));
    // The file is missing: the start fails after its bookkeeping.
    p.start_from_disk(t.id, &t, std::path::PathBuf::from("missing-993801.flac"), 0.0);
    let failed = p.st.lock().unwrap().prewarm_failed.is_some();

    saved.restore(&p);

    assert!(!failed, "tried again after the start");
}
