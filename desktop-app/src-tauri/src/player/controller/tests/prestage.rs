//! K2: the next track pre-staged whole once the track on the air has
//! settled — when it begins, what lets it go, and that what it plays is what
//! a cold start plays, to the bit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::super::{
    get, head_frames, render, Job, variant_key, Mode, Phase, PlayState, PlayerSettings, Prewarmed, Timeline, Variant, PREFILL_GATE_S,
    PRESTAGE_AFTER_S, PRESTAGE_HEAD_S, TEST_FREE_MB,
};
use super::{dummy_desc, make_dummy_chain, on_air, player_turn, track, wait_until, worker_quiet, Saved};

/// A full variant for `s` of a source at `rate`, played at `rate × l`.
fn variant(s: &PlayerSettings, rate: u32, l: usize) -> Arc<Variant> {
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
        direct: false,
        source_id: String::new(),
        dsd: None,
    })
}

/// The head read ahead holds the pre-roll a new stream waits for and more
/// (PRESTAGE_HEAD_S), in whole render steps (one at least), and stays
/// small: under 6.5 MB at 768 kHz.
#[test]
fn the_head_read_ahead_is_the_pre_roll_and_more_in_whole_steps() {
    for rate in [44_100u32, 88_200, 176_400, 352_800, 705_600, 768_000] {
        let n = head_frames(rate);
        let want = (PRESTAGE_HEAD_S * rate as f64).ceil() as usize;
        assert_eq!(n % render::STEP, 0, "{rate}: whole steps");
        assert!(n >= render::STEP, "{rate}: one step at least");
        assert!(n as f64 >= PREFILL_GATE_S * rate as f64, "{rate}: the whole pre-roll");
        assert!(n >= want, "{rate}: the whole head");
        assert!(n - render::STEP < want, "{rate}: no step more than it needs");
        assert!(n * 2 * 8 < 6_500_000, "{rate}: {} bytes", n * 16);
    }
}

/// The track on the air settles once it has been heard PRESTAGE_AFTER_S
/// since its start or its last seek, with nothing of its own in preparation
/// and no seek on its way.
#[test]
fn a_track_settles_once_it_has_been_heard_a_while() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let rate = 88_200u32;
    let tl = Timeline::new(rate, 1.0);
    let half = vec![0.0; rate as usize / 2];
    let mut buf = vec![0.0; rate as usize];
    // Six seconds through the timeline: the reader at 6 s.
    for _ in 0..12 {
        tl.append(&half, &half);
        tl.read_into(&mut buf, rate as usize / 2);
    }
    let x = track(995_101);
    let mark = |at_s: f64| render::Mark { frame: (at_s * rate as f64) as u64, track_id: x.id, index: 0, rate, l: 1, desc: dummy_desc() };
    let key = (x.id, "k".to_string());
    {
        let mut st = p.st.lock().unwrap();
        st.rate_switch_gen = None;
        st.seek_gen = None;
        st.in_flight.retain(|(id, _)| *id != x.id);
    }
    p.shared.seek_pending.store(false, Ordering::Release);
    let heard_long = p.settled_for_prestage(&mark(0.0), &tl);
    let just_sought = p.settled_for_prestage(&mark(6.0 - PRESTAGE_AFTER_S + 1.0), &tl);
    p.st.lock().unwrap().in_flight.insert(key.clone());
    let preparing = p.settled_for_prestage(&mark(0.0), &tl);
    p.st.lock().unwrap().in_flight.remove(&key);
    p.shared.seek_pending.store(true, Ordering::Release);
    let seeking = p.settled_for_prestage(&mark(0.0), &tl);
    p.shared.seek_pending.store(false, Ordering::Release);
    saved.restore(&p);

    assert!(heard_long, "heard 6 s, nothing on its way: settled");
    assert!(!just_sought, "heard 4 s since a seek: not yet");
    assert!(!preparing, "its own variant still in preparation: not yet");
    assert!(!seeking, "a seek on its way: not yet");
}

/// What tick reads the time heard from: the mark as placed at the start or
/// the seek, not the one located at the reader (its frame is the reader's,
/// so the track never settled and nothing was pre-staged early).
#[test]
fn the_time_heard_counts_from_where_the_mark_was_placed() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let rate = 88_200u32;
    let x = track(995_151);
    let mark = |at_s: f64| render::Mark { frame: (at_s * rate as f64) as u64, track_id: x.id, index: 0, rate, l: 1, desc: dummy_desc() };
    let reader = 6 * rate as u64;
    {
        let mut st = p.st.lock().unwrap();
        st.rate_switch_gen = None;
        st.seek_gen = None;
        st.in_flight.retain(|(id, _)| *id != x.id);
    }
    p.shared.seek_pending.store(false, Ordering::Release);
    let tl = Timeline::new(rate, 1.0);
    let half = vec![0.0; rate as usize / 2];
    let mut buf = vec![0.0; rate as usize];
    for _ in 0..12 {
        tl.append(&half, &half);
        tl.read_into(&mut buf, rate as usize / 2);
    }
    let heard = |marks: &[render::Mark]| {
        {
            let mut g = p.shared.marks.lock().unwrap();
            g.clear();
            g.extend(marks.iter().cloned());
        }
        let located = p.shared.locate(reader).map(|m| m.frame);
        let placed = p.shared.placed_at(reader);
        (located, placed.as_ref().map(|m| m.frame), placed.is_some_and(|m| p.settled_for_prestage(&m, &tl)))
    };
    let (located, placed_start, from_start) = heard(&[mark(0.0)]);
    let (_, placed_seek, from_seek) = heard(&[mark(0.0), mark(4.0)]);
    p.shared.marks.lock().unwrap().clear();
    saved.restore(&p);

    assert_eq!(located, Some(reader), "located at the reader: its frame is the reader's");
    assert_eq!(placed_start, Some(0), "placed at the start");
    assert!(from_start, "heard 6 s since the start: settled");
    assert_eq!(placed_seek, Some(4 * rate as u64), "placed at the seek");
    assert!(!from_seek, "heard 2 s since the seek: not yet");
}

/// Mid-track (far from the end) the next track's chain is built only once
/// the track on the air has settled (`prestage`): before K2 it waited for
/// the last PREWARM_S, and ⏭ mid-track started cold.
#[test]
fn the_next_track_is_built_mid_track_once_the_track_on_the_air_has_settled() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let (mut x, y) = (track(995_201), track(995_202));
    x.duration_s = 600.0;
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let tried = |early: bool| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone()];
            st.current = Some(x.id);
            st.settings = aura.clone();
            st.queued_next = None;
            st.prewarmed = None;
            st.prewarm_failed = None;
            st.variants.insert((y.id, variant_key(&aura)), variant(&aura, 44_100, 2));
        }
        on_air(&p, x.id, 50 * 88_200, 88_200, dummy_desc());
        {
            let mut st = p.st.lock().unwrap();
            st.stream_direct = false;
            st.play = PlayState::Stopped;
        }
        p.prestage.store(early, Ordering::Release);
        p.prewarm(Some(y.clone()), 550.0);
        p.prestage.store(false, Ordering::Release);
        // Its build begins (and stops at its filter here: tests have none).
        let tried = wait_until(|| {
            let st = p.st.lock().unwrap();
            st.prewarm_failed.as_ref().map(|b| b.0) == Some(y.id) || st.prewarmed.as_ref().is_some_and(|w| w.id == y.id)
        });
        wait_until(|| p.st.lock().unwrap().prewarm_building.is_none());
        let built = {
            let mut st = p.st.lock().unwrap();
            st.prewarm_failed = None;
            st.prewarmed = None;
            st.prewarm_building.is_some()
        };
        tried || built
    };
    let early = tried(true);
    let not_settled = {
        let mut st = p.st.lock().unwrap();
        st.prewarm_failed = None;
        drop(st);
        p.prestage.store(false, Ordering::Release);
        p.prewarm(Some(y.clone()), 550.0);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let st = p.st.lock().unwrap();
        st.prewarm_building.is_some() || st.prewarm_failed.is_some() || st.prewarmed.is_some()
    };
    p.st.lock().unwrap().variants.retain_tracks(|id| id != y.id);
    saved.restore(&p);

    assert!(early, "settled, 550 s before the end: the next track's chain is built");
    assert!(!not_settled, "not settled, 550 s before the end: nothing is built yet");
}

/// What lets go of a track pre-staged early: a rack change (the chain is
/// another rack's), another next track (one slot: the memory of one track),
/// and the end far again with memory short. A seek far from the end with
/// memory free keeps it.
#[test]
fn a_track_pre_staged_early_goes_on_a_rack_change_another_next_or_memory_short() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let other = PlayerSettings { taps: 6_000, ..aura.clone() };
    let (x, y, z) = (track(995_301), track(995_302), track(995_303));
    let kept = |rack: &PlayerSettings, next: &super::super::TrackInfo, free_mb: u64| {
        {
            let mut st = p.st.lock().unwrap();
            st.queue = vec![x.clone(), y.clone(), z.clone()];
            st.current = Some(x.id);
            st.settings = rack.clone();
            st.play = PlayState::Stopped;
            st.queued_next = Some(u64::MAX);
            st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
        }
        TEST_FREE_MB.store(free_mb, Ordering::Release);
        // Not settled (a seek just landed): only what lets go is looked at.
        p.prestage.store(false, Ordering::Release);
        p.prewarm(Some(next.clone()), 300.0);
        TEST_FREE_MB.store(u64::MAX, Ordering::Release);
        let st = p.st.lock().unwrap();
        st.prewarmed.as_ref().is_some_and(|w| w.id == y.id)
    };
    let same = kept(&aura, &y, 8_192);
    let rack_changed = kept(&other, &y, 8_192);
    let next_changed = kept(&aura, &z, 8_192);
    let memory_short = kept(&aura, &y, 512);
    {
        let mut st = p.st.lock().unwrap();
        st.prewarmed = None;
        st.queued_next = None;
    }
    saved.restore(&p);

    assert!(same, "same next track and rack, memory free, 300 s before the end: kept");
    assert!(!rack_changed, "the rack changed: let go");
    assert!(!next_changed, "another next track: let go (one slot)");
    assert!(!memory_short, "far from the end with memory short: let go");
}

/// The output device changed while the next track's chain is held: it was
/// built for the old device (rate cap, exclusive or shared), so it goes.
#[test]
fn a_track_pre_staged_early_is_let_go_on_a_device_change() {
    let _turn = player_turn();
    let p = get();
    worker_quiet(&p);
    let saved = Saved::take(&p);
    let aura = PlayerSettings { mode: Mode::Aura, fs_multiplier: 2, taps: 5_000, phase: Phase::Linear, ..PlayerSettings::default() };
    let (x, y) = (track(995_401), track(995_402));
    {
        let mut st = p.st.lock().unwrap();
        st.queue = vec![x.clone(), y.clone()];
        st.current = Some(x.id);
        st.settings = aura.clone();
        st.play = PlayState::Stopped;
        st.queued_next = Some(u64::MAX);
        st.prewarmed = Some(Prewarmed { id: y.id, settings: aura.clone(), chain: make_dummy_chain(), stand_in: false });
    }
    p.run_job(Job::DeviceChanged);
    let (held, queued) = {
        let st = p.st.lock().unwrap();
        (st.prewarmed.is_some(), st.queued_next)
    };
    saved.restore(&p);

    assert!(!held, "the output device changed: the chain built ahead is let go");
    assert_eq!(queued, None, "the next track is not marked as queued any more");
}

// ── To the bit (real filters: AURA_FILTER_DIR) ──────────────────────────

fn write_wav16(path: &std::path::Path, rate: u32, l: &[f64], r: &[f64]) {
    use std::io::Write;
    let data_len = (l.len() * 4) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * 4).to_le_bytes()).unwrap();
    f.write_all(&4u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for i in 0..l.len() {
        for s in [l[i], r[i]] {
            f.write_all(&((s.clamp(-1.0, 1.0) * 32_767.0).round() as i16).to_le_bytes()).unwrap();
        }
    }
}

/// The fixture track (4 s at 44.1 kHz, clipped or not) as a WAV in `dir`.
fn fixture_wav(dir: &std::path::Path, clipped: bool) -> std::path::PathBuf {
    let (l, r) = crate::player::analysis_cache::tests::fixture(clipped);
    let path = dir.join(format!("k2-{clipped}.wav"));
    write_wav16(&path, 44_100, &l, &r);
    path
}

/// The racks the to-the-bit tests play: Single and Multi, linear and
/// minimum phase, 1M taps at FS×2, declip on, on the CPU.
fn racks() -> Vec<PlayerSettings> {
    use crate::player::settings::Upsampler;
    [(Upsampler::Single, Phase::Linear), (Upsampler::Multi, Phase::Linear), (Upsampler::Multi, Phase::Minimum)]
        .into_iter()
        .map(|(up, phase)| PlayerSettings {
            taps: 1_000_000,
            fs_multiplier: 2,
            phase,
            upsampler: Some(up),
            use_gpu: false,
            declip: true,
            ..PlayerSettings::default()
        })
        .collect()
}

/// A pre-staged start plays what a cold start plays, to the bit: the chain
/// built ahead with its head read (`head_frames`, as prewarm_build reads
/// it) against one built at the start — read as the render's jump reads it
/// (a fade's worth, the rest of a step, then steps) and in odd sizes.
#[test]
#[ignore]
fn a_pre_staged_start_plays_as_a_cold_start_to_the_bit() {
    use crate::player::chain::{build_chain, prepare_variant, Resources};
    use crate::player::stages::Ahead;
    let dir = tempfile::tempdir().unwrap();
    let res = Resources::new(dir.path().join("cache"));
    let cancel = AtomicBool::new(false);
    let step = render::STEP;
    for clipped in [false, true] {
        let wav = fixture_wav(dir.path(), clipped);
        for s in racks() {
            let v = Arc::new(prepare_variant(&wav, &s, false, &cancel).expect("variant"));
            let head = head_frames(v.out_rate);
            let mut patterns = vec![vec![441, step - 441], vec![step; 2]];
            patterns[0].extend(std::iter::repeat(step).take(head / step + 3));
            patterns[1].extend(std::iter::repeat(step).take(head / step + 3));
            patterns.push(vec![100, 3_000, head - 1, 2, 20_000, step]);
            for sizes in &patterns {
                let play = |pre: bool| {
                    let mut c = build_chain(&res, 1, "k2", &v, &s, 0, None, None, 0).expect("chain");
                    if pre {
                        c.stage = Box::new(Ahead::read(c.stage, head));
                    }
                    let mut out = Vec::new();
                    for &n in sizes {
                        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
                        let got = c.stage.read(&mut l, &mut r);
                        out.push(got as u64);
                        out.extend(l.iter().chain(r.iter()).map(|x| x.to_bits()));
                        out.push(c.stage.position());
                    }
                    out
                };
                let cold = play(false);
                assert!(cold.iter().any(|&b| b != 0 && f64::from_bits(b) != 0.0), "the chain plays something");
                assert_eq!(play(true), cold, "{:?} {:?} clipped {clipped}, reads {:?}", s.upsampler, s.phase, &sizes[..3]);
            }
            println!("pre-staged == cold: {:?} {:?} clipped {clipped}", s.upsampler, s.phase);
        }
    }
}

/// The gapless junction is the same with the next track pre-staged: the
/// render thread plays A and goes on with B queued — B built cold, or built
/// ahead with its head read — and the timeline holds the same samples
/// across the junction, to the bit.
#[test]
#[ignore]
fn the_gapless_junction_is_the_same_with_the_next_track_pre_staged() {
    use crate::player::chain::{build_chain, prepare_variant, Resources};
    use crate::player::output::OutputShared;
    use crate::player::stages::Ahead;
    let dir = tempfile::tempdir().unwrap();
    let res = Resources::new(dir.path().join("cache"));
    let cancel = AtomicBool::new(false);
    let (wa, wb) = (fixture_wav(dir.path(), false), fixture_wav(dir.path(), true));
    for s in racks() {
        let va = Arc::new(prepare_variant(&wa, &s, false, &cancel).expect("variant A"));
        let vb = Arc::new(prepare_variant(&wb, &s, false, &cancel).expect("variant B"));
        let rate = va.out_rate;
        // A, then 2 s of B.
        let want = (6 * rate) as usize;
        let junction = |pre: bool| {
            let a = build_chain(&res, 1, "k2-a", &va, &s, 0, None, None, 0).expect("chain A");
            let mut b = build_chain(&res, 2, "k2-b", &vb, &s, 0, None, None, 0).expect("chain B");
            if pre {
                b.stage = Box::new(Ahead::read(b.stage, head_frames(rate)));
            }
            let (tx, rx) = render::channel();
            let shared = render::Shared::new();
            let handle = render::spawn(rx, shared.clone(), OutputShared::new());
            let tl = Arc::new(Timeline::new(rate, 4.0));
            tx.send(render::Msg::Start { chain: a, timeline: tl.clone() }).unwrap();
            tx.send(render::Msg::Queue { chain: b, seq: 1 }).unwrap();
            let mut got = Vec::with_capacity(want * 2);
            let mut buf = vec![0.0; 8_192 * 2];
            let t0 = std::time::Instant::now();
            while got.len() < want * 2 && t0.elapsed() < std::time::Duration::from_secs(120) {
                let n = tl.read_into(&mut buf, 8_192.min(want - got.len() / 2));
                if n == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                got.extend(buf[..n * 2].iter().map(|x| x.to_bits()));
            }
            let _ = tx.send(render::Msg::Stop);
            drop(tx);
            let _ = handle.join();
            got
        };
        let cold = junction(false);
        assert_eq!(cold.len(), want * 2, "{:?} {:?}: A and 2 s of B rendered", s.upsampler, s.phase);
        assert_eq!(junction(true), cold, "{:?} {:?}: the junction, B pre-staged", s.upsampler, s.phase);
        println!("junction same: {:?} {:?}", s.upsampler, s.phase);
    }
}
