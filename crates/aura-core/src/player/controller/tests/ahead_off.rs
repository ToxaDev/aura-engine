//! Prepare ahead off (the default, Anton 31.10): nothing of the list is
//! analysed ahead, the next track is not pre-staged mid-track, and only its
//! gapless hand-over is prepared, in the last seconds of the track on the
//! air. A start lets go of every other track's preparation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::super::{
    analysis_list, analysis_order, get, off_lead_s, variant_key, Mode, Phase, PlayState, PlayerSettings, RepeatMode, Variant,
    PREPARE_AHEAD_DEFAULT, QUEUE_AHEAD_S,
};
use super::{dummy_desc, k, on_air, player_turn, track, wait_until, worker_quiet, Saved};

/// Prepare ahead off for one test, back on when it ends — also when an
/// assert fails: the player is the whole process's (its tests prepare ahead).
struct AheadOff;
impl AheadOff {
    fn new() -> AheadOff {
        get().prepare_ahead.store(false, Ordering::Release);
        AheadOff
    }
}
impl Drop for AheadOff {
    fn drop(&mut self) {
        get().prepare_ahead.store(true, Ordering::Release);
    }
}

fn aura() -> PlayerSettings {
    PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() }
}

/// A full variant for `s` of a 44.1 kHz source, played at ×2.
fn variant(s: &PlayerSettings) -> Arc<Variant> {
    Arc::new(Variant {
        stream: None,
        key: variant_key(s),
        src: Arc::new(crate::player::convolver::SourceBuf { l: vec![0.0; 8], r: vec![0.0; 8], rate: 44_100 }),
        out_rate: 88_200,
        l: 2,
        tp_target_dbtp: -0.5,
        tp_pred_lin: 0.1,
        lim_local: true,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct: false,
        source_id: String::new(),
        dsd: None,
    })
}

#[test]
fn a_new_install_does_not_prepare_ahead() {
    assert!(!PREPARE_AHEAD_DEFAULT);
}

/// The lead is the hand-over and the build together: never inside the last
/// QUEUE_AHEAD_S, and short for a light rack.
#[test]
fn the_next_track_is_begun_shortly_before_the_hand_over() {
    let mut y = track(995_401);
    y.duration_s = 300.0;
    let lead = off_lead_s(&aura(), &y);
    assert!(lead >= QUEUE_AHEAD_S + 8.0, "{lead}");
    assert!(lead < 60.0, "a light rack: {lead}");
}

/// Off, the pre-analysis worker is given nothing: a track is analysed when
/// it starts. On, it is given the list.
#[test]
fn off_the_list_is_not_analysed_ahead() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (x, y) = (track(995_411), track(995_412));
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura();
    }
    let on = analysis_list().len();
    let order = analysis_order(&p.st.lock().unwrap()).len();
    let off = {
        let _off = AheadOff::new();
        analysis_list().len()
    };
    saved.restore(&p);
    assert_eq!(on, order, "on: the whole list");
    assert!(on > 0);
    assert_eq!(off, 0, "off: nothing ahead");
}

/// Off, the next track's chain is not built mid-track, settled or not; it
/// is in the last `off_lead_s` once the track on the air has settled, and
/// unsettled only inside the last QUEUE_AHEAD_S (a seek near the end).
#[test]
fn off_the_next_track_is_built_only_near_the_end() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let _off = AheadOff::new();
    let (x, y) = (track(995_421), track(995_422));
    let s = aura();
    // `remaining` before the end of the track on the air, heard 50 s; K2's
    // flag up as mid-track it would be — off, the prewarm does not look at
    // it. `expect`: wait for the build to begin (within ms), else look once
    // after 200 ms. A job of an earlier test still running on a busy machine
    // (worker_quiet gives up after 5 s) can move the state under the call:
    // as the tick does, the call is made again until the build begins.
    let attempt = |remaining: f64, settled: bool| {
        let mut x = x.clone();
        x.duration_s = 50.0 + remaining;
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(x.id);
            st.settings = s.clone();
            st.repeat = RepeatMode::Off;
            st.queued_next = None;
            st.prewarmed = None;
            st.prewarm_failed = None;
            st.variants.insert((y.id, variant_key(&s)), variant(&s));
        }
        on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
        {
            let mut st = p.st.lock().unwrap();
            st.stream_direct = false;
            st.play = PlayState::Stopped;
        }
        p.out.hold.store(false, Ordering::Release);
        p.prestage.store(true, Ordering::Release);
        p.settled.store(settled, Ordering::Release);
        p.prewarm(Some(y.clone()), remaining);
        p.prestage.store(false, Ordering::Release);
        p.settled.store(false, Ordering::Release);
    };
    let begun = || {
        let st = p.st.lock().unwrap();
        st.prewarm_building.is_some() || st.prewarm_failed.is_some() || st.prewarmed.is_some()
    };
    let built = |remaining: f64, settled: bool, expect: bool| {
        attempt(remaining, settled);
        let got = if expect {
            wait_until(|| {
                if begun() {
                    return true;
                }
                attempt(remaining, settled);
                std::thread::sleep(std::time::Duration::from_millis(80));
                begun()
            })
        } else {
            std::thread::sleep(std::time::Duration::from_millis(200));
            begun()
        };
        wait_until(|| p.st.lock().unwrap().prewarm_building.is_none());
        let mut st = p.st.lock().unwrap();
        st.prewarm_failed = None;
        st.prewarmed = None;
        got
    };
    let lead = off_lead_s(&s, &y);
    let mid_track = built(550.0, true, false);
    let before_lead = built(lead + 5.0, true, false);
    let unsettled_in_lead = built(lead - 1.0, false, false);
    let in_lead = built(lead - 1.0, true, true);
    let at_the_door = built(QUEUE_AHEAD_S - 1.0, false, true);
    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    saved.restore(&p);

    assert!(!mid_track, "off, 550 s before the end: nothing is built");
    assert!(!before_lead, "off, before the last {lead} s: nothing is built");
    assert!(!unsettled_in_lead, "off, a seek just landed in the last {lead} s: it waits until heard");
    assert!(in_lead, "off, settled, in the last {lead} s: the hand-over is built");
    assert!(at_the_door, "off, the last QUEUE_AHEAD_S: built, settled or not");
}

/// Off, a start lets go of the next track's preparation too: tracks skipped
/// through each left one running on all the cores. Only the started one's
/// goes on.
#[test]
fn off_a_start_drops_every_other_tracks_preparation() {
    let _turn = player_turn();
    let p = get();
    let _off = AheadOff::new();
    let (started, next, old_next) = (track(995_431), track(995_432), track(995_437));
    let s = PlayerSettings::default();
    let saved = {
        let mut st = p.st.lock().unwrap();
        let queue = std::mem::replace(&mut st.queue, vec![started.clone(), next.clone(), old_next.clone()]);
        (queue, std::mem::replace(&mut st.repeat, RepeatMode::Off))
    };
    let preps: Vec<_> = [&started, &next, &old_next]
        .iter()
        .map(|t| {
            let key = k(t.id, &s.source_key());
            let cancel = Arc::new(AtomicBool::new(false));
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), cancel.clone());
            (key, cancel)
        })
        .collect();
    p.drop_preparations_but_start(started.id);
    let kept: Vec<bool> = {
        let st = p.st.lock().unwrap();
        preps.iter().map(|(key, _)| st.in_flight.contains(key) && st.prep_cancel.contains_key(key)).collect()
    };
    let told: Vec<bool> = preps.iter().map(|(_, c)| c.load(Ordering::Acquire)).collect();
    {
        let mut st = p.st.lock().unwrap();
        for (key, _) in &preps {
            st.in_flight.remove(key);
            st.prep_cancel.remove(key);
        }
        st.queue = saved.0;
        st.repeat = saved.1;
    }
    assert_eq!(kept, [true, false, false], "only the started track's goes on");
    assert_eq!(told, [false, true, true], "the others are told to stop");
}

/// Ticked off while the next track is prepared mid-track: that stops; the
/// track on the air's own goes on.
#[test]
fn ticking_it_off_stops_what_runs_ahead() {
    let _turn = player_turn();
    let p = get();
    let (cur, next) = (track(995_441), track(995_442));
    let s = PlayerSettings::default();
    let saved_current = std::mem::replace(&mut p.st.lock().unwrap().current, Some(cur.id));
    let preps: Vec<_> = [&cur, &next]
        .iter()
        .map(|t| {
            let key = k(t.id, &s.source_key());
            let cancel = Arc::new(AtomicBool::new(false));
            let mut st = p.st.lock().unwrap();
            st.in_flight.insert(key.clone());
            st.prep_cancel.insert(key.clone(), cancel.clone());
            (key, cancel)
        })
        .collect();
    p.set_prepare_ahead(false);
    let told: Vec<bool> = preps.iter().map(|(_, c)| c.load(Ordering::Acquire)).collect();
    p.set_prepare_ahead(true);
    {
        let mut st = p.st.lock().unwrap();
        for (key, _) in &preps {
            st.in_flight.remove(key);
            st.prep_cancel.remove(key);
        }
        st.current = saved_current;
    }
    assert_eq!(told, [false, true], "the next track's stops, the playing one's goes on");
}
