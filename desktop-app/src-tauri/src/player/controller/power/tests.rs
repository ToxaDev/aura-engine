//! Power: the pure helpers and the placement (fast), and the heavy runs of
//! the design's §4.2 (T1-T4, `#[ignore]`: filters, one test per process).
//!
//!     set AURA_FILTER_DIR=<fir-optimizer/output>
//!     cargo test --profile fast power::tests::k3_splice_sleep -- --ignored --nocapture --test-threads=1
//!     (the GPU targets of T1/T2 need --features gpu-player-tests)

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::player::convolver::SourceBuf;
use crate::player::output::OutputShared;
use crate::player::render::StageInfo;
use crate::player::stages::Stage;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn desc(taps: Option<&str>, hp: bool, gpu: bool, source: &'static str, quick: bool) -> Arc<ChainDesc> {
    let mut stages = vec![StageInfo { tok: "PFR".into(), st: 1, why: String::new() }];
    if hp {
        stages.push(StageInfo { tok: "HP".into(), st: 1, why: String::new() });
    }
    Arc::new(ChainDesc {
        source,
        quick,
        taps: taps.map(String::from),
        stages,
        gain_db: 0.0,
        tp_db: None,
        ceiling_db: None,
        gpu_on: gpu,
        downgrade: None,
        downgrade_gen: 0,
        hp_deferred: false,
        stream: false,
        file: None,
        settings: std::sync::Arc::new(crate::player::settings::PlayerSettings::default()),
        variant: std::sync::Weak::new(),
        radio: std::sync::Weak::new(),
        out_rate: 0,
        l: 1,
    })
}

/// Silence that knows where it is.
struct Counter {
    pos: u64,
}

impl Stage for Counter {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        out_l.fill(0.0);
        out_r.fill(0.0);
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

fn fake_chain(track_id: u64, rate: u32, pos: u64) -> Chain {
    Chain {
        live_gain: None,
        stage: Box::new(Counter { pos }),
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

fn mark(frame: u64, track_id: u64, index: u64, rate: u32, d: &Arc<ChainDesc>) -> Mark {
    Mark { frame, track_id, index, rate, l: 1, desc: d.clone() }
}

fn tiny_variant(direct: bool, src_rate: u32, l: usize) -> Arc<Variant> {
    Arc::new(Variant {
        stream: None,
        key: "tiny".into(),
        src: Arc::new(SourceBuf { l: vec![0.1; 4096], r: vec![0.1; 4096], rate: src_rate }),
        out_rate: src_rate * l as u32,
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

// ── Fast ─────────────────────────────────────────────────────────────────────

/// The key a description gives is the chain's calib_key; the cost identity
/// ignores quick/full and changes with taps, pair, GPU and source.
#[test]
fn desc_key_and_cost_identity() {
    let lin30 = desc(Some("30M"), false, false, "live", false);
    let hp30 = desc(Some("30M"), true, false, "live", false);
    assert_eq!(desc_key(&lin30, 352_800).as_deref(), Some("30M:352800"));
    assert_eq!(desc_key(&hp30, 384_000).as_deref(), Some("30M:384000:HP"));
    assert_eq!(desc_key(&desc(Some("5k"), false, false, "live", false), 352_800).as_deref(), Some("5k:352800"));
    assert_eq!(desc_key(&desc(None, false, false, "file", false), 352_800), None);
    assert_eq!(desc_key(&desc(Some("7M"), false, false, "live", false), 352_800), None);
    assert_eq!(taps_of(&hp30), Some(30_000_000));

    let quick = desc(Some("30M"), true, false, "live", true);
    assert_eq!(cost_id(&hp30), cost_id(&quick), "quick → full must keep the cost");
    assert_ne!(cost_id(&hp30), cost_id(&lin30));
    assert_ne!(cost_id(&hp30), cost_id(&desc(Some("30M"), true, true, "live", false)));
    assert_ne!(cost_id(&hp30), cost_id(&desc(Some("10M"), true, false, "live", false)));
    assert_ne!(desc_id(&hp30), desc_id(&quick));

    assert!(cpu_live(&hp30));
    assert!(!cpu_live(&desc(Some("30M"), true, true, "live", false)), "GPU chain");
    assert!(!cpu_live(&desc(None, false, false, "file", false)), "file");
    assert!(!cpu_live(&desc(None, false, false, "direct", false)), "direct");
}

/// One rung below, walking past rungs whose files are missing; a pair
/// needs both halves; never below 1M.
#[test]
fn rung_walk_over_existing_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rate = 352_800u32;
    let touch = |taps: usize, phase: &str| {
        let name = format!("fir_{}_{}_{}.npy", taps_label(taps).unwrap(), rate, phase);
        std::fs::write(dir.path().join(name), b"").unwrap();
    };
    let find = |taps: usize, _src: u32, rate: u32, phase: &str| {
        let p = dir.path().join(format!("fir_{}_{}_{}.npy", taps_label(taps)?, rate, phase));
        p.exists().then(|| p.to_string_lossy().to_string())
    };
    let below = |taps: usize, phase: Phase| rung_below_where(taps, |t| filters_exist(t, 44_100, rate, phase, &find));

    assert_eq!(below(30_000_000, Phase::Linear), None, "nothing on disk");
    touch(1_000_000, "linear_phase");
    touch(5_000_000, "minimum_phase");
    touch(5_000, "linear_phase");
    assert_eq!(below(30_000_000, Phase::Linear), Some(1_000_000), "10M and 5M missing");
    assert_eq!(below(30_000_000, Phase::Minimum), Some(5_000_000));
    assert_eq!(below(30_000_000, Phase::Hybrid), None, "no rung has both halves");
    touch(1_000_000, "minimum_phase");
    assert_eq!(below(30_000_000, Phase::Hybrid), Some(1_000_000));
    assert_eq!(below(30_000_000, Phase::Tfs), Some(1_000_000));
    assert_eq!(below(1_000_000, Phase::Linear), None, "5k is never a fallback");
    touch(10_000_000, "linear_phase");
    assert_eq!(below(30_000_000, Phase::Linear), Some(10_000_000));
    assert_eq!(below(10_000_000, Phase::Linear), Some(1_000_000));
    // Another rate has nothing.
    assert!(!filters_exist(1_000_000, 48_000, 384_000, Phase::Linear, &find));
}

/// A trace line is one JSON object: the fields, `ev`, `tMs`, `unixMs`.
#[test]
fn trace_line_shape() {
    let line = trace_line("k3.fire", json!({ "rtf": 0.9, "key": "30M:352800:HP", "readings": [0.9, 0.91] }), 12.34567, 1_700_000_000_123);
    assert!(!line.contains('\n'));
    let v: Value = serde_json::from_str(&line).expect("valid JSON");
    assert_eq!(v["ev"], "k3.fire");
    assert_eq!(v["tMs"], 12.346);
    assert_eq!(v["unixMs"], 1_700_000_000_123u64);
    assert_eq!(v["rtf"], 0.9);
    assert_eq!(v["key"], "30M:352800:HP");
    assert_eq!(v["readings"][1], 0.91);

    let v: Value = serde_json::from_str(&trace_line("x", json!(5), 0.0, 0)).unwrap();
    assert_eq!(v["value"], 5);
    assert_eq!(v["ev"], "x");
    let v: Value = serde_json::from_str(&trace_line("y", Value::Null, 1.0, 2)).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 3);
}

/// Missing → trial; provisional and steering away from the CPU → re-measured
/// once uncontended; anything else is used as it is.
#[test]
fn trial_need_rules() {
    let e = |ms: f64, provisional: bool| CalibEntry { block_cost_ms: ms, timestamp_secs: 0, provisional, ttl_secs: None };
    // RTF at 44.1 kHz = 743 / ms: 400 ms → 1.86, 100 ms → 7.4.
    assert_eq!(trial_need(None, false, 44_100), Some("missing"));
    assert_eq!(trial_need(None, true, 44_100), Some("missing"));
    assert_eq!(trial_need(Some(&e(400.0, true)), false, 44_100), Some("recheck"));
    assert_eq!(trial_need(Some(&e(400.0, true)), true, 44_100), None, "contended: keep it");
    assert_eq!(trial_need(Some(&e(100.0, true)), false, 44_100), None, "says CPU anyway");
    assert_eq!(trial_need(Some(&e(400.0, false)), false, 44_100), None);
}

fn track(id: u64, path: &str) -> TrackInfo {
    TrackInfo {
        id,
        path: path.into(),
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        year: String::new(),
        duration_s: 60.0,
        sample_rate: 44_100,
        bits: 16,
        channels: 2,
    }
}

/// Deferred HP: HP and alpha-HP whose envelope is not ready are routed and
/// timed as the linear chain they start as, under the linear key; ready
/// keeps the pair; other phases are never changed. `chain_phase` asks
/// chain::hp_ready with the caller's track key, as build_chain does, so a
/// variant no cache holds (a seek's, a BIT-PERFECT switch's) is answered
/// too. Over an empty cache folder hp_ready says "not ready".
#[test]
fn deferred_hp_trials_linear() {
    for p in [Phase::Hybrid, Phase::Alpha] {
        assert_eq!(built_phase(p, Some(false)), (Phase::Linear, true), "{p:?}");
        assert_eq!(built_phase(p, Some(true)), (p, false), "{p:?}");
        assert_eq!(built_phase(p, None), (p, false), "{p:?}");
    }
    for p in [Phase::Linear, Phase::Minimum, Phase::Tfs] {
        for r in [Some(false), Some(true), None] {
            assert_eq!(built_phase(p, r), (p, false), "{p:?} {r:?}");
        }
    }
    let key = |p: Phase| calibration::key(30_000_000, 352_800, matches!(p, Phase::Hybrid | Phase::Alpha));
    assert_eq!(key(built_phase(Phase::Hybrid, Some(false)).0).as_deref(), Some("30M:352800"));
    assert_eq!(key(built_phase(Phase::Hybrid, Some(true)).0).as_deref(), Some("30M:352800:HP"));

    // The player's own test: an uncached variant of a track no one has
    // seen (no envelope in memory, no source id to find a file by).
    let p = get();
    let v = tiny_variant(false, 44_100, 8);
    let s = |phase: Phase| PlayerSettings { phase, taps: 30_000_000, ..PlayerSettings::default() };
    let tkey = "t990041:never-cached.flac";
    assert_eq!(p.chain_phase(&s(Phase::Hybrid), &v, tkey), (Phase::Linear, true));
    assert_eq!(p.chain_phase(&s(Phase::Alpha), &v, tkey), (Phase::Linear, true));
    assert_eq!(p.chain_phase(&s(Phase::Linear), &v, tkey), (Phase::Linear, false));
    assert_eq!(p.chain_phase(&s(Phase::Tfs), &v, tkey), (Phase::Tfs, false));

    // The same composition over a resources object of its own: not ready,
    // then the envelope in memory for the track's "full" variant.
    let dir = tempfile::tempdir().expect("tempdir");
    let res = Resources::new(dir.path().to_path_buf());
    assert_eq!(built_phase(Phase::Alpha, Some(hp_ready(&res, "t3:c.flac", &v))), (Phase::Linear, true));
    let full = Arc::new(Variant {
        stream: None,
        key: "full".into(),
        src: v.src.clone(),
        out_rate: v.out_rate,
        l: v.l,
        tp_target_dbtp: -0.5,
        tp_pred_lin: 0.1,
        lim_local: true,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct: false,
        source_id: String::new(),
    });
    res.test_cache_track("t3:c.flac");
    assert!(hp_ready(&res, "t3:c.flac", &full));
    assert!(!hp_ready(&res, "t4:c.flac", &full), "another track's key");
    assert_eq!(built_phase(Phase::Hybrid, Some(hp_ready(&res, "t3:c.flac", &full))), (Phase::Hybrid, false));
}

/// One ladder for route_conv, K4 and the trace. A pair rung is bounded by its
/// linear half (RTF ≤ linear / PAIR_MIN_COST_RATIO): a pair with no entry of
/// its own routes on the bound; a pair entry faster than the bound is
/// capped; a slower one stays; with no linear entry the pair is as
/// measured. A single chain's ladder is never bounded, and the bound is
/// never stored.
#[test]
fn route_ladder_pair_bound() {
    let (rate, l) = (352_800u32, 8usize);
    let cost = |rtf: f64| calibration::cost_from_rtf(rtf, l, rate);
    let mut c = CalibrationStore::load_from(None);
    // A K3 deficit on a deferred start's linear stand-in (A4, K3b).
    c.record_deficit("30M:352800".into(), cost(0.66));
    c.insert_trial("10M:352800".into(), cost(1.8), false);
    c.insert_trial("10M:352800:HP".into(), cost(1.5), false);
    c.insert_trial("5M:352800:HP".into(), cost(2.0), false);
    c.insert_trial("1M:352800".into(), cost(30.0), false);
    c.insert_trial("1M:352800:HP".into(), cost(12.0), false);
    let at = |lad: &[TapRtf], t: usize| lad.iter().find(|r| r.taps == t).map(|r| r.rtf);
    let near = |got: Option<f64>, want: f64| got.map_or(false, |g| (g - want).abs() < 1e-9 * want.max(1.0));

    let pair = route_ladder(&c, rate, l, true);
    let r = policy::PAIR_MIN_COST_RATIO;
    assert!(near(at(&pair, 30_000_000), 0.66 / r), "no pair entry: the bound, {pair:?}");
    assert!(near(at(&pair, 10_000_000), 1.8 / r), "a faster pair entry is capped, {pair:?}");
    assert!(near(at(&pair, 5_000_000), 2.0), "no linear entry: as measured, {pair:?}");
    assert!(near(at(&pair, 1_000_000), 12.0), "a slower pair entry stays, {pair:?}");
    assert_eq!(at(&pair, 5_000), None);

    let single = route_ladder(&c, rate, l, false);
    assert!(near(at(&single, 30_000_000), 0.66));
    assert!(near(at(&single, 10_000_000), 1.8));
    assert_eq!(at(&single, 5_000_000), None);
    assert!(near(at(&single, 1_000_000), 30.0));
    assert!(c.entry("30M:352800:HP").is_none(), "the bound was stored");

    // Where it leads: the 30M pair at 0.44 is a downgrade on the CPU (never
    // 30M at full taps for want of a number) and the GPU when it is there.
    assert!(policy::best_cpu_taps(&pair, 30_000_000) < 30_000_000);
}

/// The HP swap's send check, as a table: every reason in its order, and the
/// ones worth a second pass.
#[test]
fn hp_send_check_table() {
    let ok = HpObs {
        generation: true,
        current: true,
        phase: true,
        timeline: true,
        stand_in: true,
        seek_pending: false,
        swap_pending: false,
        hold: false,
        placed: true,
        gpu_chain: false,
        gpu_failed: false,
    };
    assert_eq!(hp_send_check(&ok), None);
    assert_eq!(hp_send_check(&HpObs { gpu_chain: true, ..ok }), None, "a GPU chain on a healthy GPU");
    assert_eq!(hp_send_check(&HpObs { gpu_failed: true, ..ok }), None, "a CPU chain after K4");
    let cases: [(HpObs, &str); 10] = [
        (HpObs { generation: false, ..ok }, "generation"),
        (HpObs { current: false, ..ok }, "track"),
        (HpObs { phase: false, ..ok }, "phase"),
        (HpObs { timeline: false, ..ok }, "timeline"),
        (HpObs { gpu_chain: true, gpu_failed: true, ..ok }, "gpu-failed"),
        (HpObs { stand_in: false, ..ok }, "moved"),
        (HpObs { seek_pending: true, ..ok }, "seek"),
        (HpObs { swap_pending: true, ..ok }, "swap"),
        (HpObs { hold: true, ..ok }, "hold"),
        (HpObs { placed: false, ..ok }, "placement"),
    ];
    for (o, want) in cases {
        assert_eq!(hp_send_check(&o), Some(want), "{o:?}");
    }
    // A stale job never retries; the GPU failing or another chain landing does.
    assert_eq!(
        hp_send_check(&HpObs { generation: false, gpu_chain: true, gpu_failed: true, stand_in: false, ..ok }),
        Some("generation")
    );
    for why in ["gpu-failed", "moved", "swap"] {
        assert!(hp_retry(why), "{why}");
    }
    for why in ["generation", "track", "phase", "timeline", "seek", "hold", "placement", "not-deferred", "busy"] {
        assert!(!hp_retry(why), "{why}");
    }
}

/// The power pool: as wide as the render pool, its own threads, HIGHEST.
#[test]
fn power_pool_width_names_priority() {
    let pool = power_pool();
    assert_eq!(pool.current_num_threads(), calibration::cpu_width().max(2));
    let name = pool.install(|| std::thread::current().name().map(String::from));
    assert!(name.as_deref().unwrap_or("").starts_with("aura-power-"), "{name:?}");
    #[cfg(windows)]
    {
        let prio = pool.install(|| unsafe {
            use winapi::um::processthreadsapi::{GetCurrentThread, GetThreadPriority};
            GetThreadPriority(GetCurrentThread())
        });
        assert_eq!(prio, POWER_PRIORITY);
    }
}

/// Behind the write head by a crossfade and 50 ms; `chain.start` follows;
/// a chain already there is not moved; another track or no mark refuses.
#[test]
fn place_near_write_head_behind_the_write_head() {
    let rate = 48_000u32;
    let back = (render::XFADE_MS / 1000.0 * rate as f64) as u64 + (0.05 * rate as f64) as u64;
    let d = desc(Some("1M"), false, false, "live", false);

    let shared = Shared::new();
    let tl = Timeline::new(rate, 4.0);
    let two_s = vec![0.0; 2 * rate as usize];
    tl.append(&two_s, &two_s);
    shared.marks.lock().unwrap().push_back(mark(0, 7, 1_000_000, rate, &d));
    let w_index = 1_000_000 + 2 * rate as u64 - 1;

    let mut c = fake_chain(7, rate, 1_000_000 + 10_000);
    let p = place_near_write_head(&mut c, &shared, &tl).expect("placed");
    assert_eq!(c.start, w_index - back);
    assert_eq!(c.stage.position(), c.start);
    assert_eq!(p.start, c.start);
    assert_eq!(p.iterations, 1);
    assert!(!p.skip_path);

    // Already past it: left where it is.
    let mut c = fake_chain(7, rate, w_index);
    let p = place_near_write_head(&mut c, &shared, &tl).unwrap();
    assert_eq!((p.iterations, c.start), (0, w_index));

    // The write head is in another track, or at another rate.
    let mut c = fake_chain(8, rate, 1_000_000);
    assert_eq!(place_near_write_head(&mut c, &shared, &tl).unwrap_err(), "track-moved");
    let mut c = fake_chain(7, 44_100, 1_000_000);
    assert_eq!(place_near_write_head(&mut c, &shared, &tl).unwrap_err(), "rate");

    // Less buffered than the guard, a crossfade and `back`: the render
    // thread will splice at the earliest frame (the skip path) whatever a
    // chase does, so the chain is left where it is (no race with the old
    // chain on the same cores); the render thread skips it forward.
    let shared = Shared::new();
    let tl = Timeline::new(rate, 4.0);
    let thin = vec![0.0; (0.2 * rate as f64) as usize];
    tl.append(&thin, &thin);
    let mut c = fake_chain(7, rate, 500);
    assert_eq!(place_near_write_head(&mut c, &shared, &tl).unwrap_err(), "no-mark");
    shared.marks.lock().unwrap().push_back(mark(0, 7, 0, rate, &d));
    let p = place_near_write_head(&mut c, &shared, &tl).unwrap();
    assert!(p.skip_path, "{p:?}");
    assert_eq!((p.iterations, c.start, c.stage.position()), (0, 500, 500), "a thin buffer must not be chased");
    // Just under guard + crossfade + back (0.36 s): still thin. The track
    // and rate checks still apply.
    let shared = Shared::new();
    let tl = Timeline::new(rate, 4.0);
    let under = vec![0.0; (0.35 * rate as f64) as usize];
    tl.append(&under, &under);
    shared.marks.lock().unwrap().push_back(mark(0, 7, 0, rate, &d));
    let mut c = fake_chain(7, rate, 500);
    let p = place_in(None, &mut c, &shared, &tl).unwrap();
    assert_eq!((p.iterations, c.start, p.skip_path), (0, 500, true));
    let mut c = fake_chain(8, rate, 500);
    assert_eq!(place_in(None, &mut c, &shared, &tl).unwrap_err(), "track-moved");
    // Past it (0.4 s), on the calling thread: chased to the write head.
    let shared = Shared::new();
    let tl = Timeline::new(rate, 4.0);
    let enough = vec![0.0; (0.4 * rate as f64) as usize];
    tl.append(&enough, &enough);
    shared.marks.lock().unwrap().push_back(mark(0, 7, 0, rate, &d));
    let mut c = fake_chain(7, rate, 500);
    let p = place_in(None, &mut c, &shared, &tl).unwrap();
    assert_eq!(p.iterations, 1);
    assert_eq!(c.start, enough.len() as u64 - 1 - back);
}

/// The splice mark's frame and its index error against the mark before it.
#[test]
fn splice_mark_index_error() {
    let a = desc(Some("30M"), false, false, "live", false);
    let b = desc(Some("10M"), false, false, "live", false);
    let shared = Shared::new();
    shared.marks.lock().unwrap().push_back(mark(0, 1, 1000, 48_000, &a));
    shared.marks.lock().unwrap().push_back(mark(5000, 1, 6000, 48_000, &b));
    assert_eq!(splice_mark(&shared, &b), (Some(5000), Some(0)));
    assert_eq!(splice_mark(&shared, &a), (Some(0), None));
    shared.marks.lock().unwrap().back_mut().unwrap().index = 5800;
    assert_eq!(splice_mark(&shared, &b), (Some(5000), Some(-200)));
    let c = desc(Some("5M"), false, false, "live", false);
    assert_eq!(splice_mark(&shared, &c), (None, None));
}

/// Direct, 5k, and a rate without filters: no trial, no entry, no lock kept.
#[test]
fn pretrial_without_filters_stores_nothing() {
    let p = get();
    let mut s = PlayerSettings { phase: Phase::Linear, taps: 30_000_000, ..PlayerSettings::default() };
    // 111 113 Hz × 8: no filter is designed for it.
    let v = tiny_variant(false, 111_113, 8);
    let direct = tiny_variant(true, 111_113, 1);
    p.power_pretrial(&s, &direct, (s.phase, false));
    p.power_pretrial(&s, &v, (s.phase, false));
    s.taps = 5_000;
    p.power_pretrial(&s, &v, (s.phase, false));
    let c = p.calib();
    assert!(c.entry("30M:888904").is_none());
    assert!(c.entry("30M:111113").is_none());
    assert!(c.entry("5k:888904").is_none());
    drop(c);
    assert!(!TRIAL_RUNNING.load(Ordering::Acquire));
    assert!(lock_power(Duration::from_secs(5), &|| false).is_some(), "POWER_LOCK left held");
}

/// K3's target and chips. On the GPU: the audible taps, downgrade and gen
/// stay (no blink), no gen is used up and no rung is looked up. On the CPU:
/// exactly one rung below, reason "power", a new gen (the taps chip blinks,
/// and only because the taps went down). No rung below: K3 stays.
#[test]
fn k3_plan_chips() {
    use std::cell::Cell;
    let prev = Some(DowngradeInfo { from: "30M".into(), to: "10M".into(), reason: "gpu-failed".into(), asked: "30M".into() });
    let (asked, bumped) = (Cell::new(0), Cell::new(0));
    let below = |t: Option<usize>| {
        let asked = &asked;
        move || {
            asked.set(asked.get() + 1);
            t
        }
    };
    let next = || {
        bumped.set(bumped.get() + 1);
        8
    };

    let (taps, dg, gen) = k3_plan(true, 10_000_000, &prev, 7, 1_000_000, below(Some(5_000_000)), next).expect("gpu");
    assert_eq!((taps, gen), (10_000_000, 7));
    let dg = dg.expect("the audible downgrade is kept");
    assert_eq!((dg.from.as_str(), dg.to.as_str(), dg.reason.as_str()), ("30M", "10M", "gpu-failed"));
    assert_eq!((asked.get(), bumped.get()), (0, 0), "the GPU branch looked up a rung or used a gen");
    let (_, dg, gen) = k3_plan(true, 30_000_000, &None, 0, 1_000_000, below(None), next).expect("gpu");
    assert!(dg.is_none());
    assert_eq!(gen, 0);

    let (taps, dg, gen) = k3_plan(false, 10_000_000, &prev, 7, 1_000_000, below(Some(5_000_000)), next).expect("cpu");
    if policy::K3_MAX_RUNGS == 1 {
        assert_eq!(taps, 5_000_000, "one rung, whatever the model says");
    }
    assert!(taps < 10_000_000, "the CPU branch must lower the taps");
    assert_eq!(gen, 8);
    let dg = dg.expect("a CPU escalation carries a downgrade");
    assert_eq!((dg.from.as_str(), dg.to.as_str(), dg.reason.as_str()), ("10M", "5M", "power"));
    // The second step down still counts as the rack's 30M: the taps chip
    // blinks for it, never "switching" (night 1.10 review, M1).
    assert_eq!(dg.asked, "30M", "the size the rack asked for, not the one heard before the step");
    assert_eq!((asked.get(), bumped.get()), (1, 1));

    assert!(k3_plan(false, 1_000_000, &prev, 7, 1_000_000, below(None), next).is_none(), "the floor");
    assert_eq!((asked.get(), bumped.get()), (2, 1), "no gen is used up at the floor");

    let (_, dg, _) = k3_plan(false, 30_000_000, &None, 0, 1_000_000, below(Some(10_000_000)), next).expect("cpu");
    assert_eq!(dg.expect("a first step down").asked, "30M", "a first step down asked for what it came from");
}

/// `lock_power` gives up at the timeout, and at once when the job is stale.
#[test]
fn lock_power_gives_way() {
    let held = lock_power(Duration::from_secs(10), &|| false).expect("free");
    let t = Instant::now();
    assert!(lock_power(Duration::from_secs(3), &|| true).is_none());
    assert!(t.elapsed() < Duration::from_millis(500), "a stale job waited {:?}", t.elapsed());
    let t = Instant::now();
    assert!(lock_power(Duration::from_millis(50), &|| false).is_none());
    assert!(t.elapsed() >= Duration::from_millis(50));
    drop(held);
}

/// Two render threads in one process: the accessor keeps the first pool,
/// one worker per CPU of the affinity mask.
#[test]
fn render_pool_first_set_wins() {
    let start = || {
        let (tx, rx) = render::channel();
        let shared = Shared::new();
        render::spawn(rx, shared.clone(), OutputShared::new());
        let t0 = Instant::now();
        while !shared.alive.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(shared.alive.load(Ordering::Relaxed), "render thread did not start");
        tx
    };
    let tx1 = start();
    let first = render::render_pool().expect("render pool after the first spawn");
    let tx2 = start();
    let now = render::render_pool().expect("render pool after the second spawn");
    assert!(Arc::ptr_eq(&first, &now), "a later spawn replaced the first pool");
    assert_eq!(first.current_num_threads(), calibration::cpu_width().max(2));
    // Both loops idle in recv_timeout. Dropping a sender would end its
    // loop, and run() then writes player-buffer.json under %LOCALAPPDATA%:
    // keep them until the test process exits.
    std::mem::forget(tx1);
    std::mem::forget(tx2);
}

/// The render thread feeds its live windows to the calibration for CPU
/// chains only (K2): a GPU chain played for two windows leaves the store
/// empty, while a CPU chain beside it gets its entry.
#[test]
fn gpu_chain_never_calibrates() {
    let rate = 48_000u32;
    let key = "1M:48000";
    let play = |gpu: bool| {
        let (tx, rx) = render::channel();
        let shared = Shared::new();
        *shared.calibration.lock().unwrap() = CalibrationStore::load_from(None);
        render::spawn(rx, shared.clone(), OutputShared::new());
        let mut c = fake_chain(1, rate, 0);
        c.gpu_on = gpu;
        c.calib_key = key.into();
        let tl = Arc::new(Timeline::new(rate, 4.0));
        tx.send(Msg::Start { chain: c, timeline: tl.clone() }).unwrap();
        (tx, shared, Collector::start(tl))
    };
    let (tx_gpu, gpu, col_gpu) = play(true);
    let (tx_cpu, cpu, col_cpu) = play(false);
    let t0 = Instant::now();
    while cpu.calibration.lock().unwrap().entry(key).is_none() && t0.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = col_gpu.finish();
    let _ = col_cpu.finish();
    assert!(cpu.calibration.lock().unwrap().entry(key).is_some(), "the CPU chain was never calibrated");
    assert!(gpu.calibration.lock().unwrap().entry(key).is_none(), "a GPU chain wrote the calibration");
    // A loop that ends writes player-buffer.json under %LOCALAPPDATA%;
    // these idle until the test process exits (under 30 s of rendering
    // they write nothing).
    std::mem::forget(tx_gpu);
    std::mem::forget(tx_cpu);
}

/// O6 (render.rs): the RTF window restarts when a chain starts and when a
/// swap is placed, so the first reading after an idle gap covers 2 s of the
/// new chain, not one burst step. Idle past a window, then Start chain A:
/// the first reading comes 2 s later. Stop reading past a window (the render
/// idles on a full buffer), then jump to chain B, four times slower: the
/// next reading again comes 2 s after the swap, and it is B's alone.
#[test]
fn rtf_window_restarts_at_start_and_swap() {
    let rate = 44_100u32;
    let (tx, rx) = render::channel();
    let shared = Shared::new();
    render::spawn(rx, shared.clone(), OutputShared::new());
    // Longer than a window, and past run()'s read of the buffer target.
    std::thread::sleep(Duration::from_millis(2300));
    *shared.target_ahead_s.lock().unwrap() = 1.0;
    let reading_after = |before: u64, from: Instant| -> (Duration, u64) {
        loop {
            let now = shared.rtf_milli.load(Ordering::Relaxed);
            if now != before || from.elapsed() > Duration::from_secs(15) {
                return (from.elapsed(), now);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    };

    let tl = Arc::new(Timeline::new(rate, 4.0));
    let t_start = Instant::now();
    tx.send(Msg::Start { chain: slowed(fake_chain(1, rate, 0), 20.0), timeline: tl.clone() }).unwrap();
    let col = Collector::start(tl.clone());
    let (start_wait, rtf_a) = reading_after(0, t_start);
    let _ = col.finish();
    std::thread::sleep(Duration::from_millis(2300));
    let before = shared.rtf_milli.load(Ordering::Relaxed);
    let t_swap = Instant::now();
    tx.send(Msg::Swap {
        chain: slowed(fake_chain(2, rate, 0), 5.0),
        continuous: false,
        seek: false,
        side_buf: None,
        requested: t_swap,
    })
    .unwrap();
    let col = Collector::start(tl.clone());
    let (swap_wait, rtf_b) = reading_after(before, t_swap);
    let _ = col.finish();
    // Ending the loop would write player-buffer.json under %LOCALAPPDATA%.
    let _ = tx.send(Msg::Stop);
    std::mem::forget(tx);
    println!(
        "O6: first reading {:.0} ms after Start (RTF {:.3}), {:.0} ms after the swap (RTF {:.3}, before {:.3})",
        ms(start_wait),
        rtf_a as f64 / 1000.0,
        ms(swap_wait),
        rtf_b as f64 / 1000.0,
        before as f64 / 1000.0
    );
    assert!(rtf_a > 0, "no reading after Start");
    assert!(start_wait >= Duration::from_millis(1900), "the first window after Start closed after {start_wait:?}");
    assert!(rtf_b != before, "no reading after the swap");
    assert!(swap_wait >= Duration::from_millis(1900), "the first window after the swap closed after {swap_wait:?}");
    assert!(rtf_b < before, "the first window after the swap holds chain A's steps: {rtf_b} vs {before}");
}

// ── Heavy (T1-T4) ────────────────────────────────────────────────────────────

fn filters_set(test: &str) -> bool {
    if std::env::var_os("AURA_FILTER_DIR").map_or(true, |d| d.is_empty()) {
        println!("{test}: set AURA_FILTER_DIR to run");
        return false;
    }
    true
}

/// A loud two-channel multi-tone, `seconds` long, as a prepared variant.
fn synth_variant(src_rate: u32, out_rate: u32, seconds: f64) -> Arc<Variant> {
    let n = (src_rate as f64 * seconds) as usize;
    let tau = 2.0 * std::f64::consts::PI;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / src_rate as f64;
        let env = 0.75 + 0.25 * (tau * 0.5 * t).sin();
        let a = (tau * 220.0 * t).sin() * 0.3 + (tau * 997.0 * t).sin() * 0.15 + (tau * 3150.0 * t).sin() * 0.08;
        l.push(a * env);
        r.push((a * 0.9 + (tau * 61.0 * t).sin() * 0.05) * env);
    }
    Arc::new(Variant {
        stream: None,
        key: format!("synth{}", src_rate),
        src: Arc::new(SourceBuf { l, r, rate: src_rate }),
        out_rate,
        l: (out_rate / src_rate) as usize,
        tp_target_dbtp: -0.5,
        tp_pred_lin: 0.6,
        lim_local: true,
        tokens: vec![],
        notes: vec![],
        stages: vec![],
        quick: false,
        direct: false,
        source_id: String::new(),
    })
}

/// `%LOCALAPPDATA%` pointed at a scratch folder holding `player-buffer.json`
/// with `target_s`, for as long as it lives (render.rs reads and writes it).
struct ScratchAppData {
    old: Option<OsString>,
    _dir: tempfile::TempDir,
}

impl ScratchAppData {
    fn new(target_s: f64) -> ScratchAppData {
        let dir = tempfile::tempdir().expect("tempdir");
        let aura = dir.path().join("AuraEngine");
        std::fs::create_dir_all(&aura).unwrap();
        std::fs::write(aura.join("player-buffer.json"), format!("{{\"targetS\":{:.3}}}", target_s)).unwrap();
        let old = std::env::var_os("LOCALAPPDATA");
        unsafe { std::env::set_var("LOCALAPPDATA", dir.path()); }
        ScratchAppData { old, _dir: dir }
    }
}

impl Drop for ScratchAppData {
    fn drop(&mut self) {
        match self.old.take() {
            Some(v) => unsafe { std::env::set_var("LOCALAPPDATA", v) },
            None => unsafe { std::env::remove_var("LOCALAPPDATA") },
        }
    }
}

/// The process affinity mask narrowed for as long as it lives.
struct AffinityGuard {
    old: usize,
}

impl AffinityGuard {
    fn set(mask: usize) -> Option<AffinityGuard> {
        #[cfg(windows)]
        unsafe {
            use winapi::um::processthreadsapi::GetCurrentProcess;
            use winapi::um::winbase::{GetProcessAffinityMask, SetProcessAffinityMask};
            let (mut process, mut system) = (0usize, 0usize);
            if GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) == 0 {
                return None;
            }
            // winapi declares the mask as a DWORD: the first 32 CPUs only.
            if mask > u32::MAX as usize || SetProcessAffinityMask(GetCurrentProcess(), mask as u32) == 0 {
                return None;
            }
            Some(AffinityGuard { old: process })
        }
        #[cfg(not(windows))]
        {
            let _ = mask;
            None
        }
    }
}

impl Drop for AffinityGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use winapi::um::processthreadsapi::GetCurrentProcess;
            use winapi::um::winbase::SetProcessAffinityMask;
            SetProcessAffinityMask(GetCurrentProcess(), self.old as u32);
        }
    }
}

/// A chain slowed to `rtf` × real time by sleeping (no CPU contention).
struct SlowStage {
    inner: Box<dyn Stage>,
    rtf: f64,
    rate: f64,
}

impl Stage for SlowStage {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let t = Instant::now();
        let n = self.inner.read(out_l, out_r);
        let want = Duration::from_secs_f64(out_l.len() as f64 / self.rate / self.rtf);
        let took = t.elapsed();
        if took < want {
            std::thread::sleep(want - took);
        }
        n
    }
    fn position(&self) -> u64 {
        self.inner.position()
    }
    fn total(&self) -> u64 {
        self.inner.total()
    }
    fn is_gpu_failed(&self) -> bool {
        self.inner.is_gpu_failed()
    }
}

fn slowed(mut c: Chain, rtf: f64) -> Chain {
    let rate = c.out_rate as f64;
    let inner = std::mem::replace(&mut c.stage, Box::new(Counter { pos: 0 }));
    c.stage = Box::new(SlowStage { inner, rtf, rate });
    c
}

/// One 10 ms read of the collector.
#[derive(Clone, Copy)]
struct Chunk {
    read_before: u64,
    got: usize,
    /// Index in `mono` of this chunk's first frame.
    at: usize,
}

/// A real-time reader that zero-fills what an underrun leaves out (as the
/// output does) and logs every chunk, so timeline frames map to samples.
struct Collector {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    mono: Arc<Mutex<Vec<f64>>>,
    chunks: Arc<Mutex<Vec<Chunk>>>,
    chunk: usize,
}

impl Collector {
    fn start(tl: Arc<Timeline>) -> Collector {
        let stop = Arc::new(AtomicBool::new(false));
        let mono = Arc::new(Mutex::new(Vec::new()));
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let chunk = (tl.rate() / 100) as usize;
        let handle = {
            let (stop, mono, chunks) = (stop.clone(), mono.clone(), chunks.clone());
            std::thread::spawn(move || {
                let rate = tl.rate();
                let mut buf = vec![0.0f64; chunk * 2];
                let start = Instant::now();
                let mut consumed = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
                    while consumed + chunk as u64 <= due {
                        let read_before = tl.read_pos();
                        let got = tl.read_into(&mut buf, chunk);
                        if got < chunk {
                            tl.note_underrun(chunk - got);
                            buf[2 * got..].fill(0.0);
                        }
                        let mut m = mono.lock().unwrap();
                        chunks.lock().unwrap().push(Chunk {
                            read_before,
                            got,
                            at: m.len(),
                        });
                        for i in 0..chunk {
                            m.push((buf[2 * i] + buf[2 * i + 1]) * 0.5);
                        }
                        consumed += chunk as u64;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        Collector { stop, handle: Some(handle), mono, chunks, chunk }
    }

    fn finish(mut self) -> (Vec<f64>, Vec<Chunk>, usize) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
        let mono = std::mem::take(&mut *self.mono.lock().unwrap());
        let chunks = std::mem::take(&mut *self.chunks.lock().unwrap());
        (mono, chunks, self.chunk)
    }
}

/// The sample index of timeline frame `f`, if it was read.
fn sample_of(chunks: &[Chunk], f: u64) -> Option<usize> {
    chunks
        .iter()
        .find(|c| c.read_before <= f && f < c.read_before + c.got as u64)
        .map(|c| c.at + (f - c.read_before) as usize)
}

/// Frames zero-filled by chunks that started reading in `[from, to)`.
fn underrun_in(chunks: &[Chunk], chunk: usize, from: u64, to: u64) -> u64 {
    chunks
        .iter()
        .filter(|c| c.read_before >= from && c.read_before < to)
        .map(|c| (chunk - c.got) as u64)
        .sum()
}

fn rms_db(x: &[f64]) -> f64 {
    if x.is_empty() {
        return -200.0;
    }
    let ms = x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64;
    if ms < 1e-30 { -300.0 } else { 10.0 * ms.log10() }
}

/// End the render thread and wait until it has written its buffer file
/// (into the scratch `%LOCALAPPDATA%`, while that is still set).
fn stop_render(tx: Sender<Msg>, shared: &Shared) {
    let _ = tx.send(Msg::Stop);
    drop(tx);
    let t0 = Instant::now();
    while shared.alive.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The click threshold. The synthetic signal's largest step is ~3× its
/// median step, so the ratio is taken against the same signal's own peaks
/// next to the splice: over 2000 random positions without a splice it stays
/// at or under 1.005, and a 5 ms hole gives ~11.
const CLICK_RATIO_MAX: f64 = 1.5;

/// What one splice run measured.
#[derive(Debug, Default)]
struct SpliceRun {
    fire_s: f64,
    fire_rtf: f64,
    build_ms: f64,
    fire_to_sent_ms: f64,
    sent_to_heard_ms: f64,
    buffered_at_place_s: f64,
    iterations: u32,
    skip_path: bool,
    gpu_on: bool,
    under_before: u64,
    under_after: u64,
    zero_run_ms: f64,
    /// The largest sample step in [splice − xf, splice + 2·xf) over the
    /// largest in the windows of the same length just before and just
    /// after: ~1 for a clean splice of this signal, ≫ 1 for a click.
    click_ratio: f64,
    rms_before_db: f64,
    rms_after_db: f64,
    mark_err: Option<i64>,
}

impl SpliceRun {
    fn pass(&self) -> bool {
        self.under_after == 0
            && self.click_ratio < CLICK_RATIO_MAX
            && (self.rms_after_db - self.rms_before_db).abs() < 1.0
            && self.mark_err == Some(0)
    }
}

/// Render `old` into a 4 s timeline with a real-time reader, feed the
/// product's detector every 200 ms from the real `rtf_milli` and marks, and
/// on Fire build `s_new` in `pool`, place it with the product's placement
/// and send it. `late`: wait for the buffer to fall under 0.2 s first.
fn run_splice(
    res: &Resources,
    v: &Arc<Variant>,
    old: Chain,
    s_new: &PlayerSettings,
    gpu: Option<Arc<GpuPolyCtx>>,
    pool: &rayon::ThreadPool,
    late: bool,
    preroll_s: f64,
) -> SpliceRun {
    let rate = v.out_rate;
    let mut out = SpliceRun::default();
    let (tx, rx) = render::channel();
    let shared = Shared::new();
    render::spawn(rx, shared.clone(), OutputShared::new());
    let tl = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: old, timeline: tl.clone() }).unwrap();
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < preroll_s * rate as f64 && t0.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let col = Collector::start(tl.clone());

    let mut det = DeficitDetector::new();
    let (fire_at, fire_read) = loop {
        std::thread::sleep(Duration::from_millis(200));
        let (r, w) = match (shared.locate(tl.read_pos()), shared.locate(tl.write_pos().saturating_sub(1))) {
            (Some(r), Some(w)) => (r, w),
            _ => continue,
        };
        let s = DeficitSample {
            now_s: t0.elapsed().as_secs_f64(),
            track_id: 1,
            generation: 0,
            write_chain: desc_id(&w.desc),
            read_chain: desc_id(&r.desc),
            cost_id: cost_id(&w.desc),
            rtf_milli: shared.rtf_milli.load(Ordering::Relaxed),
            buffered_s: tl.buffered_frames() as f64 / rate as f64,
            target_s: *shared.target_ahead_s.lock().unwrap(),
            cpu_live: cpu_live(&w.desc),
            blocked: false,
        };
        if let DeficitVerdict::Fire { rtf, .. } = det.observe(&s) {
            out.fire_s = s.now_s;
            out.fire_rtf = rtf;
            break (Instant::now(), tl.read_pos());
        }
        assert!(t0.elapsed() < Duration::from_secs(90), "no Fire within 90 s");
    };
    if late {
        let t = Instant::now();
        while (tl.buffered_frames() as f64) > 0.2 * rate as f64 && t.elapsed() < Duration::from_secs(40) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let w = shared.locate(tl.write_pos().saturating_sub(1)).expect("write mark");
    let tb = Instant::now();
    let mut chain = pool
        .install(|| build_chain(res, 1, "power-t", v, s_new, w.index, gpu.clone(), None, 1))
        .expect("new chain");
    out.build_ms = ms(tb.elapsed());
    out.gpu_on = chain.gpu_on;
    let placed = place_in(Some(pool), &mut chain, &shared, &tl).expect("placed");
    out.iterations = placed.iterations;
    out.skip_path = placed.skip_path;
    out.buffered_at_place_s = tl.buffered_frames() as f64 / rate as f64;
    let before = shared.last_switch_ms.load(Ordering::Relaxed);
    let sent = Instant::now();
    out.fire_to_sent_ms = ms(sent.duration_since(fire_at));
    tx.send(Msg::Swap { chain, continuous: true, seek: false, side_buf: None, requested: sent }).unwrap();
    while shared.last_switch_ms.load(Ordering::Relaxed) == before && sent.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(2));
    }
    out.sent_to_heard_ms = ms(sent.elapsed());
    // The splice: the newest mark (never the read position).
    let (splice, prev) = {
        let g = shared.marks.lock().unwrap();
        let n = g.len();
        (g.back().cloned().expect("splice mark"), if n > 1 { Some(g[n - 2].clone()) } else { None })
    };
    out.mark_err = prev
        .filter(|p| p.track_id == splice.track_id && p.frame <= splice.frame)
        .map(|p| splice.index as i64 - (p.index + (splice.frame - p.frame)) as i64);
    let two_s = 2 * rate as u64;
    let t = Instant::now();
    while tl.read_pos() < splice.frame + two_s + rate as u64 / 5 && t.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let (mono, chunks, chunk) = col.finish();
    stop_render(tx, &shared);

    out.under_before = underrun_in(&chunks, chunk, fire_read, splice.frame);
    out.under_after = underrun_in(&chunks, chunk, splice.frame, splice.frame + two_s);
    let longest_zero = {
        let from = chunks.iter().find(|c| c.read_before >= fire_read).map_or(0, |c| c.at);
        let mut best = 0usize;
        let mut run = 0usize;
        for &x in &mono[from.min(mono.len())..] {
            if x.abs() < 1e-9 { run += 1; best = best.max(run); } else { run = 0; }
        }
        best
    };
    out.zero_run_ms = longest_zero as f64 / rate as f64 * 1000.0;
    if let Some(c) = sample_of(&chunks, splice.frame) {
        let xf = (render::XFADE_MS / 1000.0 * rate as f64) as usize;
        let win = rate as usize / 100;
        let step = |i: usize| (mono[i] - mono[i - 1]).abs();
        let peak_in = |a: usize, b: usize| (a.max(1)..b.min(mono.len())).map(step).fold(0.0f64, f64::max);
        let lo = c.saturating_sub(xf).max(1);
        let hi = (c + 2 * xf).min(mono.len());
        let len = hi - lo;
        let peak = peak_in(lo, hi);
        let control = peak_in(lo.saturating_sub(len), lo).max(peak_in(hi, hi + len));
        out.click_ratio = if control > 0.0 { peak / control } else { f64::INFINITY };
        out.rms_before_db = rms_db(&mono[c.saturating_sub(2 * win)..c.saturating_sub(win)]);
        out.rms_after_db = rms_db(&mono[(c + win).min(mono.len())..(c + 2 * win).min(mono.len())]);
    }
    out
}

/// T1: the K3 splice mechanics without contention. The old chain is 1M CPU
/// slowed to RTF 0.8 by sleeping; the product's detector fires on it; the
/// new chain goes in just behind the write head. PASS with ≥ 0.35 s
/// buffered at placement: no underrun in [splice, splice + 2 s], click
/// ratio < `CLICK_RATIO_MAX`, |ΔRMS| < 1 dB, mark index error 0 (5/5). The
/// late-fire run (≈ 0.2 s buffered) takes the skip path: the chain is not
/// chased, the render thread skips it forward from the old chain's index,
/// and its mark index error must be 0 too (it was -32769 before).
#[test]
#[ignore]
fn k3_splice_sleep() {
    if !filters_set("k3_splice_sleep") {
        return;
    }
    let _scratch = ScratchAppData::new(3.0);
    let reps: usize = std::env::var("AURA_T1_REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let res = Resources::new(std::env::temp_dir().join("aura-power-t1"));
    let v = synth_variant(44_100, 352_800, 120.0);
    let s_old = PlayerSettings { phase: Phase::Linear, taps: 1_000_000, ..PlayerSettings::default() };
    #[allow(unused_mut)]
    let mut cases: Vec<(&str, usize, Option<Arc<GpuPolyCtx>>, bool)> = vec![
        ("1M cpu", 1_000_000, None, false),
        ("10M cpu", 10_000_000, None, false),
        ("1M cpu, late fire", 1_000_000, None, true),
    ];
    #[cfg(feature = "gpu-player-tests")]
    match GpuPolyCtx::try_build() {
        Some(ctx) => cases.push(("1M gpu", 1_000_000, Some(ctx), false)),
        None => println!("k3_splice_sleep: no discrete GPU, GPU target skipped"),
    }
    let mut failed = Vec::new();
    for (name, taps, gpu, late) in cases {
        let s_new = PlayerSettings { taps, ..s_old.clone() };
        let n = if late { 1 } else { reps };
        for rep in 0..n {
            let old = build_chain(&res, 1, "power-t", &v, &s_old, 0, None, None, 0).expect("old chain");
            let m = run_splice(&res, &v, slowed(old, 0.8), &s_new, gpu.clone(), power_pool(), late, 2.5);
            println!("T1 {name} #{rep}: {m:?}");
            if !late && m.buffered_at_place_s >= 0.35 && !m.pass() {
                failed.push(format!("{name} #{rep}"));
            }
            if late && (m.mark_err != Some(0) || m.iterations != 0) {
                failed.push(format!("{name} #{rep}: mark error {:?}, {} chase iterations", m.mark_err, m.iterations));
            }
        }
    }
    assert!(failed.is_empty(), "T1 failed: {failed:?}");
}

/// What one K3 run through the product measured.
#[derive(Debug, Default)]
struct ProductRun {
    heard: bool,
    taps_after: Option<String>,
    gpu_after: bool,
    /// (from, to, reason)
    downgrade_after: Option<(String, String, String)>,
    gen_before: u32,
    gen_after: u32,
    /// The K3 entry's lifetime once the new chain is heard.
    entry_ttl: Option<u64>,
    under_after: u64,
    mark_err: Option<i64>,
    /// The largest rung heard after the splice.
    max_taps_after: Option<usize>,
}

/// The player itself plays `v` on a 10M linear CPU chain slowed to RTF 0.8
/// until what is heard changes (or 60 s), then 2 s more; then it stops.
fn product_case(p: &Player, v: &Arc<Variant>, id: u64, use_gpu: bool) -> ProductRun {
    let rate = v.out_rate;
    let s = PlayerSettings { phase: Phase::Linear, taps: 10_000_000, use_gpu, ..PlayerSettings::default() };
    let t = TrackInfo {
        id,
        path: format!("synth-{}.wav", id),
        title: "synth".into(),
        artist: String::new(),
        album: String::new(),
        year: String::new(),
        duration_s: 120.0,
        sample_rate: 44_100,
        bits: 24,
        channels: 2,
    };
    {
        let mut st = p.st.lock().unwrap();
        st.queue.push(t.clone());
        st.settings = s.clone();
        st.variants.insert((id, s.source_key()), v.clone());
    }
    let old = build_chain(&p.res, id, &track_key(&t), v, &s, 0, None, None, 0).expect("old chain");
    let tl = Arc::new(Timeline::new(rate, 4.0));
    *p.shared.target_ahead_s.lock().unwrap() = 3.0;
    let _ = p.render_tx.lock().unwrap().send(Msg::Start { chain: slowed(old, 0.8), timeline: tl.clone() });
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < 2.5 * rate as f64 && t0.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let col = Collector::start(tl.clone());
    {
        let mut st = p.st.lock().unwrap();
        st.timeline = Some(tl.clone());
        st.current = Some(id);
        st.play = PlayState::Playing;
    }
    let old_desc = p.shared.locate(tl.read_pos()).expect("the old chain's mark").desc;
    let mut run = ProductRun { gen_before: old_desc.downgrade_gen, ..ProductRun::default() };

    // The worker's own tick feeds the detector and fires K3.
    let t0 = Instant::now();
    let heard = loop {
        std::thread::sleep(Duration::from_millis(20));
        if let Some(m) = p.shared.locate(tl.read_pos()) {
            if !Arc::ptr_eq(&m.desc, &old_desc) {
                break Some(m);
            }
        }
        if t0.elapsed() > Duration::from_secs(60) {
            break None;
        }
    };
    let mut splice = None;
    if let Some(m) = &heard {
        run.heard = true;
        run.taps_after = m.desc.taps.clone();
        run.gpu_after = m.desc.gpu_on;
        run.downgrade_after = m.desc.downgrade.as_ref().map(|d| (d.from.clone(), d.to.clone(), d.reason.clone()));
        run.gen_after = m.desc.downgrade_gen;
        let (frame, err) = splice_mark(&p.shared, &m.desc);
        run.mark_err = err;
        let frame = frame.unwrap_or_else(|| tl.read_pos());
        splice = Some(frame);
        let t = Instant::now();
        while tl.read_pos() < frame + 2 * rate as u64 + rate as u64 / 5 && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
            if let Some(m) = p.shared.locate(tl.read_pos()) {
                run.max_taps_after = run.max_taps_after.max(taps_of(&m.desc));
            }
        }
        run.entry_ttl = p.calib().entry("10M:352800").and_then(|e| e.ttl_secs);
    }
    let (_, chunks, chunk) = col.finish();
    if let Some(frame) = splice {
        run.under_after = underrun_in(&chunks, chunk, frame, frame + 2 * rate as u64);
    }

    {
        let mut st = p.st.lock().unwrap();
        st.play = PlayState::Stopped;
        st.timeline = None;
        st.current = None;
        st.queue.retain(|x| x.id != id);
    }
    let _ = p.render_tx.lock().unwrap().send(Msg::Stop);
    let t = Instant::now();
    while K3_IN_FLIGHT.load(Ordering::Acquire) && t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    run
}

/// T1b: K3 through the product, end to end. The player (`get()`) plays a
/// 10M linear CPU chain slowed to RTF 0.8; its own tick feeds the detector
/// and fires, and the K3 thread decides, builds, places and sends. Switch
/// off → exactly one rung below with reason "power" and a new gen (the taps
/// chip blinks); with the feature, a discrete GPU and the switch on → 10M
/// on the GPU with the gen unchanged (no blink). Both: the K3 entry keeps
/// its 12 h lifetime, no underrun in [splice, splice + 2 s], mark index
/// error 0, the taps never go up.
#[test]
#[ignore]
fn k3_product_path() {
    if !filters_set("k3_product_path") {
        return;
    }
    let _scratch = ScratchAppData::new(3.0);
    let p = get();
    *p.calib() = CalibrationStore::load_from(None);
    let rate = 352_800u32;
    let v = synth_variant(44_100, rate, 120.0);
    #[allow(unused_mut)]
    let mut cases: Vec<(&str, bool)> = vec![("cpu", false)];
    #[cfg(feature = "gpu-player-tests")]
    match GpuPolyCtx::try_build() {
        Some(_) => cases.push(("gpu", true)),
        None => println!("k3_product_path: no discrete GPU, GPU case skipped"),
    }
    let one_below = rung_below_existing(10_000_000, 44_100, rate, Phase::Linear);
    let mut failed = Vec::new();
    for (i, (name, use_gpu)) in cases.into_iter().enumerate() {
        let m = product_case(&p, &v, 9_100 + i as u64, use_gpu);
        println!("T1b {name}: {m:?}");
        let mut why = Vec::new();
        if !m.heard {
            why.push("K3 never heard".to_string());
        } else if use_gpu {
            if !m.gpu_after || m.taps_after.as_deref() != Some("10M") {
                why.push(format!("expected 10M on the GPU, heard {:?} gpu {}", m.taps_after, m.gpu_after));
            }
            if m.gen_after != m.gen_before || m.downgrade_after.is_some() {
                why.push("the chips changed on a GPU escalation".into());
            }
        } else {
            let want = one_below.and_then(taps_label);
            if m.gpu_after || m.taps_after.as_deref() != want {
                why.push(format!("expected {:?} on the CPU, heard {:?} gpu {}", want, m.taps_after, m.gpu_after));
            }
            if m.gen_after == m.gen_before {
                why.push("a CPU escalation kept the gen (no blink)".into());
            }
            match &m.downgrade_after {
                Some((from, _, reason)) if from == "10M" && reason == "power" => {}
                other => why.push(format!("downgrade {other:?}")),
            }
        }
        if m.heard {
            if m.entry_ttl != Some(calibration::K3_TTL_SECS) {
                why.push(format!("K3 entry ttl {:?}", m.entry_ttl));
            }
            if m.under_after != 0 {
                why.push(format!("{} underrun frames after the splice", m.under_after));
            }
            if m.mark_err != Some(0) {
                why.push(format!("mark index error {:?}", m.mark_err));
            }
            if m.max_taps_after.map_or(false, |t| t > 10_000_000) {
                why.push("the taps went up".into());
            }
        }
        if !why.is_empty() {
            failed.push(format!("{name}: {}", why.join("; ")));
        }
    }
    assert!(failed.is_empty(), "T1b failed: {failed:?}");
}

/// T2: a real deficit. 30M Hybrid-Phase at 44.1 kHz is built on all CPUs,
/// then the process is narrowed to one E-core (`AURA_T2_MASK`, default
/// 0x10000) and plays; on Fire a 10M CPU chain (and with the feature the
/// GPU at 30M) is built in a pool at HIGHEST and at ABOVE_NORMAL. Reports
/// fire → sent, the build, sent → heard and the underruns around the splice
/// for each priority (Q10).
#[test]
#[ignore]
fn k3_splice_affinity() {
    if !filters_set("k3_splice_affinity") {
        return;
    }
    let mask = std::env::var("AURA_T2_MASK")
        .ok()
        .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0x10000);
    let _scratch = ScratchAppData::new(1.5);
    let res = Resources::new(std::env::temp_dir().join("aura-power-t2"));
    let v = synth_variant(44_100, 352_800, 90.0);
    let s_old = PlayerSettings { phase: Phase::Hybrid, taps: 30_000_000, ..PlayerSettings::default() };
    #[allow(unused_mut)]
    let mut targets: Vec<(&str, PlayerSettings, Option<Arc<GpuPolyCtx>>)> =
        vec![("10M cpu", PlayerSettings { taps: 10_000_000, ..s_old.clone() }, None)];
    #[cfg(feature = "gpu-player-tests")]
    if let Some(ctx) = GpuPolyCtx::try_build() {
        targets.push(("30M gpu", s_old.clone(), Some(ctx)));
    }
    for (prio_name, prio) in [("HIGHEST", POWER_PRIORITY), ("ABOVE_NORMAL", 1)] {
        for (name, s_new, gpu) in &targets {
            let old = build_chain(&res, 1, "power-t", &v, &s_old, 0, None, None, 0).expect("old chain");
            let Some(aff) = AffinityGuard::set(mask) else {
                println!("k3_splice_affinity: mask {mask:#x} refused by this machine");
                return;
            };
            let pool = build_pool(calibration::cpu_width().max(2), "t2-power", prio);
            let m = run_splice(&res, &v, old, s_new, gpu.clone(), &pool, false, 0.3);
            drop(pool);
            drop(aff);
            println!("T2 {prio_name} {name} (mask {mask:#x}): {m:?}");
        }
    }
}

/// T3: the pre-trial end to end on the product's Player, into a fresh
/// calibration file: 30M linear and Hybrid-Phase at 44.1 kHz (L 8) and
/// 192 kHz (L 2). The entry is stored confirmed and written; a second call
/// is a no-op. The `[POWER] pretrial` lines carry the components.
#[test]
#[ignore]
fn pretrial_end_to_end() {
    if !filters_set("pretrial_end_to_end") {
        return;
    }
    let _scratch = ScratchAppData::new(1.5);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("player-calibration.json");
    let p = get();
    *p.calib() = CalibrationStore::load_from(Some(path.clone()));
    for (src_rate, out_rate) in [(44_100u32, 352_800u32), (192_000, 384_000)] {
        let v = synth_variant(src_rate, out_rate, 60.0);
        for phase in [Phase::Linear, Phase::Hybrid] {
            let s = PlayerSettings { phase, taps: 30_000_000, ..PlayerSettings::default() };
            let key = calibration::key(s.taps, out_rate, phase == Phase::Hybrid).unwrap();
            // The chain as route_conv passes it for a track whose envelope
            // is ready: the pair is timed as a pair.
            let t = Instant::now();
            p.power_pretrial(&s, &v, (phase, false));
            let first = t.elapsed();
            let e = p.calib().entry(&key).cloned().unwrap_or_else(|| panic!("{key}: no entry"));
            let t = Instant::now();
            p.power_pretrial(&s, &v, (phase, false));
            let second = t.elapsed();
            let again = p.calib().entry(&key).cloned().unwrap_or_else(|| panic!("{key}: entry gone"));
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            println!(
                "T3 {key}: first {:.1} ms, second {:.3} ms, cost {:.2} ms, RTF {:.2}, provisional {}, in file {}",
                ms(first),
                ms(second),
                e.block_cost_ms,
                calibration::rtf_from_cost(e.block_cost_ms, src_rate),
                e.provisional,
                text.contains(&key)
            );
            assert!(!e.provisional, "{key}: an idle trial must be confirmed");
            assert!(text.contains(&key), "{key}: not written");
            // A second measurement would store another cost. The time bound
            // is looser than the design's 1 ms: the skip itself prints one
            // `[POWER]` line to the console.
            assert_eq!(again.block_cost_ms.to_bits(), e.block_cost_ms.to_bits(), "{key}: the second call measured again");
            assert_eq!(again.timestamp_secs, e.timestamp_secs, "{key}: the second call stored again");
            assert!(second < Duration::from_millis(10), "{key}: the second call took {:.3} ms", ms(second));
        }
    }
}

/// T3b: the deferred Hybrid-Phase start on the product's Player. A track
/// with no envelope yet (no source id, nothing in memory) and Hybrid-Phase
/// 30M at 44.1 kHz (L 8): chain_phase, asked with the track's key as
/// route_conv asks it, says linear, and the pre-trial stores the linear key
/// only. The pair key stays missing for the HP swap's own pre-trial; until
/// then the pair routes on its linear half (route_ladder's bound).
#[test]
#[ignore]
fn pretrial_deferred_hp() {
    if !filters_set("pretrial_deferred_hp") {
        return;
    }
    let _scratch = ScratchAppData::new(1.5);
    let dir = tempfile::tempdir().expect("tempdir");
    let p = get();
    *p.calib() = CalibrationStore::load_from(Some(dir.path().join("player-calibration.json")));
    let v = synth_variant(44_100, 352_800, 60.0);
    let tkey = track_key(&track(990_001, "deferred.flac"));
    let s = PlayerSettings { phase: Phase::Hybrid, taps: 30_000_000, ..PlayerSettings::default() };
    let phase = p.chain_phase(&s, &v, &tkey);
    let t = Instant::now();
    p.power_pretrial(&s, &v, phase);
    let took = t.elapsed();
    let lin = p.calib().entry("30M:352800").cloned();
    let pair = p.calib().entry("30M:352800:HP").cloned();
    let ladder = route_ladder(&p.calib(), 352_800, 8, true);
    println!(
        "T3b: {:.1} ms, phase {:?}, linear {:?}, pair {:?}, pair ladder {:?}",
        ms(took),
        phase,
        lin.as_ref().map(|e| e.block_cost_ms),
        pair.as_ref().map(|e| e.block_cost_ms),
        ladder
    );
    assert_eq!(phase, (Phase::Linear, true));
    assert!(lin.is_some(), "the linear key was not measured");
    assert!(pair.is_none(), "the pair was measured for a deferred start");
    assert_eq!(p.chain_phase(&s, &v, &tkey), (Phase::Linear, true), "a pre-trial made an envelope");
    let lin_rtf = calibration::rtf_from_cost(lin.unwrap().block_cost_ms, 44_100);
    let pair_rtf = ladder.iter().find(|r| r.taps == 30_000_000).map(|r| r.rtf);
    assert!(
        pair_rtf.map_or(false, |r| (r - lin_rtf / policy::PAIR_MIN_COST_RATIO).abs() < 1e-9 * lin_rtf),
        "the pair must route on its linear half: {pair_rtf:?} vs {lin_rtf}"
    );
}

/// H2: the deferred HP swap through the product. The player plays a
/// Hybrid-Phase 1M CPU track whose envelope is not ready (the linear
/// stand-in, built as start_track builds it); `hp_swap` computes the
/// envelope, routes (with the pair's pre-trial), builds the pair at the
/// write head, places it off the render thread and sends it. PASS: the pair
/// is heard (an HP stage, not deferred), its splice continues the index
/// (mark index error 0), the heard index never goes back, no underrun after
/// the splice, `queued_next` is cleared, and a second job for the same
/// start finds no stand-in to replace.
#[test]
#[ignore]
fn hp_swap_product_path() {
    if !filters_set("hp_swap_product_path") {
        return;
    }
    let _scratch = ScratchAppData::new(3.0);
    let p = get();
    *p.calib() = CalibrationStore::load_from(None);
    let rate = 352_800u32;
    let v = synth_variant(44_100, rate, 90.0);
    let id = 9_300u64;
    let t = TrackInfo { duration_s: 90.0, ..track(id, "synth-hp.wav") };
    let s = PlayerSettings { phase: Phase::Hybrid, taps: 1_000_000, use_gpu: false, ..PlayerSettings::default() };
    let (saved_settings, saved_hold) = (p.st.lock().unwrap().settings.clone(), p.out.hold.load(Ordering::Acquire));
    p.out.hold.store(false, Ordering::Release);
    {
        let mut st = p.st.lock().unwrap();
        st.queue.push(t.clone());
        st.settings = s.clone();
        st.variants.insert((id, s.source_key()), v.clone());
    }
    let tkey = track_key(&t);
    let (s_eff, gpu, dg, gen_chain) = p.route_conv(&s, &v, &tkey);
    let stand_in = build_chain(&p.res, id, &tkey, &v, &s_eff, 0, gpu, dg, gen_chain).expect("stand-in");
    assert!(stand_in.hp_deferred, "the envelope must not be ready yet");
    let tl = Arc::new(Timeline::new(rate, 4.0));
    *p.shared.target_ahead_s.lock().unwrap() = 3.0;
    let _ = p.render_tx.lock().unwrap().send(Msg::Start { chain: stand_in, timeline: tl.clone() });
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < 2.5 * rate as f64 && t0.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let col = Collector::start(tl.clone());
    let gen = {
        let mut st = p.st.lock().unwrap();
        st.timeline = Some(tl.clone());
        st.current = Some(id);
        st.play = PlayState::Playing;
        st.queued_next = Some(u64::MAX);
        st.generation
    };
    let old_desc = p.shared.locate(tl.read_pos()).expect("the stand-in's mark").desc;

    // Watch the heard index while the job runs on its own thread.
    let job = {
        let (v, tkey, s) = (v.clone(), tkey.clone(), s.clone());
        std::thread::spawn(move || get().hp_swap(id, gen, v, tkey, s))
    };
    let mut last_index = 0u64;
    let mut went_back: Option<(u64, u64)> = None;
    let t0 = Instant::now();
    let heard = loop {
        std::thread::sleep(Duration::from_millis(20));
        if let Some(m) = p.shared.locate(tl.read_pos()) {
            if m.index < last_index && went_back.is_none() {
                went_back = Some((last_index, m.index));
            }
            last_index = m.index;
            if !Arc::ptr_eq(&m.desc, &old_desc) {
                break Some(m);
            }
        }
        if t0.elapsed() > Duration::from_secs(90) {
            break None;
        }
    };
    let _ = job.join();
    let queued_after = p.st.lock().unwrap().queued_next;
    let mut splice = None;
    let mut mark_err = None;
    if let Some(m) = &heard {
        let (frame, err) = splice_mark(&p.shared, &m.desc);
        mark_err = err;
        splice = frame;
        let frame = frame.unwrap_or_else(|| tl.read_pos());
        let t = Instant::now();
        while tl.read_pos() < frame + 2 * rate as u64 + rate as u64 / 5 && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
            if let Some(m) = p.shared.locate(tl.read_pos()) {
                if m.index < last_index && went_back.is_none() {
                    went_back = Some((last_index, m.index));
                }
                last_index = m.index;
            }
        }
    }
    // A second job for the same start: the pair is on the air now.
    let second_t0 = Instant::now();
    p.hp_swap(id, gen, v.clone(), tkey.clone(), s.clone());
    let second_ms = ms(second_t0.elapsed());
    let still = p.shared.locate(tl.write_pos().saturating_sub(1)).map(|m| m.desc);
    let (_, chunks, chunk) = col.finish();
    let under_after = splice.map_or(0, |f| underrun_in(&chunks, chunk, f, f + 2 * rate as u64));

    {
        let mut st = p.st.lock().unwrap();
        st.play = PlayState::Stopped;
        st.timeline = None;
        st.current = None;
        st.queued_next = None;
        st.queue.retain(|x| x.id != id);
        st.variants.retain_tracks(|x| x != id);
        st.settings = saved_settings;
    }
    let _ = p.render_tx.lock().unwrap().send(Msg::Stop);
    p.out.hold.store(saved_hold, Ordering::Release);

    let heard = heard.expect("the pair was never heard");
    println!(
        "hp_swap_product_path: heard taps {:?} gpu {} hp {} deferred {}, mark index error {:?}, went back {:?}, underrun after {}, queued_next {:?}, second job {:.0} ms",
        heard.desc.taps,
        heard.desc.gpu_on,
        is_pair(&heard.desc),
        heard.desc.hp_deferred,
        mark_err,
        went_back,
        under_after,
        queued_after,
        second_ms
    );
    assert!(is_pair(&heard.desc) && !heard.desc.hp_deferred, "the pair was not what came after the stand-in");
    assert_eq!(mark_err, Some(0), "the splice must continue the index");
    assert_eq!(went_back, None, "the heard index went back");
    assert_eq!(under_after, 0, "underrun after the splice");
    assert_eq!(queued_after, None, "the HP swap must clear queued_next (CR-5)");
    assert!(still.map_or(false, |d| Arc::ptr_eq(&d, &heard.desc)), "the second job replaced the pair");
}

/// T4: the first live window after 2.5 s of idle must be dropped (the first
/// window of its key); the second is applied. Before O6 (render.rs) that
/// window held one step (a whole subsonic-guard block credited as 8192
/// frames), and the printed ratio was the size of V10; with O6 the window
/// restarts at Start and runs its 2 s, so the ratio shows only what the
/// first 2 s cost (the cold first step, the fill-up), and the calibration's
/// drop is the second line of defence.
#[test]
#[ignore]
fn first_window_after_idle() {
    if !filters_set("first_window_after_idle") {
        return;
    }
    let _scratch = ScratchAppData::new(1.5);
    let (tx, rx) = render::channel();
    let shared = Shared::new();
    *shared.calibration.lock().unwrap() = CalibrationStore::load_from(None);
    render::spawn(rx, shared.clone(), OutputShared::new());
    std::thread::sleep(Duration::from_millis(2500));

    let res = Resources::new(std::env::temp_dir().join("aura-power-t4"));
    let v = synth_variant(44_100, 352_800, 60.0);
    let s = PlayerSettings { phase: Phase::Linear, taps: 1_000_000, ..PlayerSettings::default() };
    let chain = build_chain(&res, 1, "power-t", &v, &s, 0, None, None, 0).expect("chain");
    let key = chain.calib_key.clone();
    let tl = Arc::new(Timeline::new(v.out_rate, 4.0));
    tx.send(Msg::Start { chain, timeline: tl.clone() }).unwrap();
    let col = Collector::start(tl.clone());

    let mut seen = Vec::new();
    let mut last = shared.rtf_milli.load(Ordering::Relaxed);
    let t0 = Instant::now();
    while seen.len() < 2 && t0.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
        let now = shared.rtf_milli.load(Ordering::Relaxed);
        if now != last && now > 0 {
            last = now;
            std::thread::sleep(Duration::from_millis(50));
            let cost = calibration::cost_from_rtf(now as f64 / 1000.0, v.l, v.out_rate);
            let stored = shared.calibration.lock().unwrap().block_cost_ms(&key);
            println!("T4 window {}: RTF {:.3}, cost {:.2} ms, store {:?}", seen.len() + 1, now as f64 / 1000.0, cost, stored);
            seen.push((cost, stored));
        }
    }
    let _ = col.finish();
    stop_render(tx, &shared);
    assert_eq!(seen.len(), 2, "two windows expected");
    assert!(seen[0].1.is_none(), "the first window after idle must be dropped");
    let applied = seen[1].1.expect("the second window must be applied");
    // rtf_milli is the window's RTF truncated to 1/1000.
    assert!((applied - seen[1].0).abs() < 0.01 * applied, "applied {applied}, window {}", seen[1].0);
    println!("T4 dropped / applied = {:.2}", seen[0].0 / seen[1].0);
}

/// K3b: one automatic step-down per track. A stand-in already stepped down
/// for power does not step again for the pair; a first step, a pair at the
/// same taps or a GPU failure's rung are not a second one.
#[test]
fn a_track_steps_its_taps_down_once() {
    let stepped = |taps: &str, reason: &str| {
        let mut d = (*desc(Some(taps), false, false, "live", false)).clone();
        d.downgrade = Some(DowngradeInfo { from: "30M".into(), to: taps.into(), reason: reason.into(), asked: "30M".into() });
        d
    };
    let m = |n: usize| n * 1_000_000;
    // 30M -> 10M by K3 (or at the start), the pair would need 5M: held.
    assert!(second_step_down(&stepped("10M", "power"), m(5)));
    // The pair fits at 10M: no second step.
    assert!(!second_step_down(&stepped("10M", "power"), m(10)));
    // Not stepped down yet: the pair's step is the first.
    assert!(!second_step_down(&desc(Some("30M"), false, false, "live", false), m(10)));
    // K4's rung is not a power step.
    assert!(!second_step_down(&stepped("10M", "gpu-failed"), m(5)));
}

/// K3's source (REVIEW-6e66ef1 #3). On, as it always was: the rack, its
/// full variant, else its quick one. Instant start off: the chain on the
/// air, one rung down — its own settings and variant, whole — never the
/// rack still being prepared (a linear stand-in of it, or its quick
/// variant).
#[test]
fn k3_with_instant_start_off_steps_down_the_chain_on_the_air() {
    // Declip on: no quick variant's key is the chain's own.
    let air_s = PlayerSettings { phase: Phase::Linear, taps: 5_000_000, declip: true, ..PlayerSettings::default() };
    // A rack change still being prepared: Hybrid-Phase and another source
    // key, its full variant not yet, its quick one cached.
    let rack = PlayerSettings { phase: Phase::Hybrid, isp: !air_s.isp, ..air_s.clone() };
    let (on_air, rack_quick) = (tiny_variant(false, 44_100, 8), tiny_variant(false, 44_100, 8));
    let mut d = (*desc(Some("5M"), false, false, "live", false)).clone();
    d.settings = Arc::new(air_s.clone());
    d.variant = Arc::downgrade(&on_air);
    let quick_key = rack.quick().source_key();
    let cached = |k: &str| (k == quick_key).then(|| rack_quick.clone());

    {
        let (s, v, quick, _) = k3_source(true, rack.clone(), &d, 1_000_000, cached).unwrap();
        assert!(s.phase == Phase::Hybrid && s.isp == rack.isp, "on: the rack");
        assert!(quick && Arc::ptr_eq(&v, &rack_quick), "on: its quick variant while the full one is prepared");
        assert_eq!(s.taps, 1_000_000);
    }
    {
        let (s, v, quick, _) = k3_source(false, rack.clone(), &d, 1_000_000, cached).unwrap();
        assert!(s.phase == Phase::Linear && s.isp == air_s.isp, "off: the chain on the air, not the rack being prepared");
        assert!(!quick && Arc::ptr_eq(&v, &on_air), "off: its own variant");
        assert_eq!(s.taps, 1_000_000, "one rung down");
    }
    drop(on_air);
    assert_eq!(
        k3_source(false, rack, &d, 1_000_000, cached).err(),
        Some("no-variant"),
        "off, its variant gone and not cached: nothing to build"
    );
}

/// Sample `i` is `i` (left) and `-i` (right): what was drawn says where.
struct Indexed {
    pos: u64,
    end: u64,
}

impl Stage for Indexed {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = (out_l.len() as u64).min(self.end.saturating_sub(self.pos)) as usize;
        for i in 0..n {
            out_l[i] = (self.pos + i as u64) as f64;
            out_r[i] = -((self.pos + i as u64) as f64);
        }
        out_l[n..].fill(0.0);
        out_r[n..].fill(0.0);
        self.pos += n as u64;
        n
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.end
    }
}

/// The HP chain's drawing: caught up to just ahead of the earliest
/// rewritable frame, then every sample kept to just past the write head,
/// the chain left at its end; it stops at the track's end. Refused over
/// another chain (a seek or a swap landed: the stand-in is not the newest
/// at the reader), another track, a BIT-PERFECT chain, and a chain built
/// past the reader (it then goes in at the write head, as before).
#[test]
fn draw_over_draws_from_the_reader_to_past_the_write_head() {
    let rate = 48_000u32;
    let d = desc(Some("1M"), false, false, "live", false);
    let on_air = || {
        let shared = Shared::new();
        let tl = Timeline::new(rate, 8.0);
        let three_s = vec![0.0; 3 * rate as usize];
        tl.append(&three_s, &three_s);
        let mut sink = vec![0.0; rate as usize];
        tl.read_into(&mut sink, rate as usize / 2);
        shared.marks.lock().unwrap().push_back(mark(0, 7, 1_000_000, rate, &d));
        (shared, tl)
    };
    let indexed = |track: u64, pos: u64, end: u64| {
        let mut c = fake_chain(track, rate, pos);
        c.stage = Box::new(Indexed { pos, end });
        c
    };
    let lead = (DRAW_LEAD_S * rate as f64) as u64;
    let past = (DRAW_PAST_WRITE_S * rate as f64) as u64;

    let (shared, tl) = on_air();
    let mut c = indexed(7, 1_000_000, u64::MAX / 4);
    let rd = draw_over(&mut c, &shared, &tl, &d).expect("drawn");
    let from = 1_000_000 + tl.earliest_rewrite() + lead;
    let to = 1_000_000 + tl.write_pos() + past;
    assert_eq!(rd.start, from);
    assert_eq!(rd.start + rd.l.len() as u64, to);
    assert_eq!(c.stage.position(), to, "the chain stands at the drawing's end");
    assert!(Arc::ptr_eq(&rd.over, &d));
    for (i, (&l, &r)) in rd.l.iter().zip(&rd.r).enumerate() {
        let want = (from + i as u64) as f64;
        assert_eq!((l, r), (want, -want), "sample {i}");
    }

    // The track ends inside the drawing.
    let (shared, tl) = on_air();
    let end = 1_000_000 + tl.write_pos() - 1000;
    let mut c = indexed(7, 1_000_000, end);
    let rd = draw_over(&mut c, &shared, &tl, &d).expect("drawn to the end");
    assert_eq!(rd.start + rd.l.len() as u64, end);

    // Refusals.
    let (shared, tl) = on_air();
    let other = desc(Some("1M"), false, false, "live", false);
    let mut c = indexed(7, 1_000_000, u64::MAX / 4);
    assert_eq!(draw_over(&mut c, &shared, &tl, &other).err(), Some("moved"));
    let mut c = indexed(8, 1_000_000, u64::MAX / 4);
    assert_eq!(draw_over(&mut c, &shared, &tl, &d).err(), Some("moved"));
    let mut c = indexed(7, 1_000_000, u64::MAX / 4);
    c.direct = true;
    assert_eq!(draw_over(&mut c, &shared, &tl, &d).err(), Some("direct"));
    let w = 1_000_000 + tl.write_pos();
    let mut c = indexed(7, w, u64::MAX / 4);
    assert_eq!(draw_over(&mut c, &shared, &tl, &d).err(), Some("ahead"));
    assert_eq!(c.stage.position(), w, "a refused chain is not moved");
    // A seek landed at the reader while the chain was built.
    let seek = desc(Some("1M"), false, false, "live", false);
    shared.marks.lock().unwrap().push_back(mark(tl.earliest_rewrite() - 10, 7, 5_000_000, rate, &seek));
    let mut c = indexed(7, 1_000_000, u64::MAX / 4);
    assert_eq!(draw_over(&mut c, &shared, &tl, &d).err(), Some("moved"));
}

/// Where the HP pair is built: ahead of the reader by the build, the guard
/// and the drawing, so that the reader is still a guard short of the start
/// when the drawing reaches the write head; never at or past where the
/// write head will be once built, less a guard; a drawing slower than real
/// time never catches it.
#[test]
fn draw_ahead_covers_the_build_and_the_drawing() {
    let g = 0.25;
    // CPU 30M as measured (build 2.2 s, drawing 2.6x, 3.9 s buffered).
    let x = draw_ahead_s(2.2, 3.9, Some(2.6), g).expect("ahead");
    assert!((x - (2.2 + g + (3.9 + DRAW_PAST_WRITE_S - g) / 2.6)).abs() < 1e-12, "{x}");
    // Simulate it: the build, then the drawing to the write head (moving).
    let t_draw = (3.9 - x + 2.2 + DRAW_PAST_WRITE_S) / (2.6 - 1.0);
    let reader_then = 2.2 + t_draw;
    assert!((x - reader_then - g).abs() < 1e-9, "the reader ends a guard short of the start");
    // Faster drawing, shorter lead; a faster build, too.
    assert!(draw_ahead_s(2.2, 3.9, Some(11.0), g).unwrap() < x);
    assert!(draw_ahead_s(0.5, 3.9, Some(2.6), g).unwrap() < x);
    // Unknown speed: the build and the guard only.
    assert_eq!(draw_ahead_s(1.0, 3.9, None, g), Some(1.0 + g));
    // Too slow, or too long for the buffer: at the write head as before.
    assert_eq!(draw_ahead_s(0.1, 3.9, Some(1.0), g), None);
    assert_eq!(draw_ahead_s(0.1, 3.9, Some(0.7), g), None);
    // The cap is where the write head will be once built: B + build.
    assert!(draw_ahead_s(2.2, 3.9, Some(2.6), g).unwrap() + g > 3.9, "past today's write head");
    assert_eq!(draw_ahead_s(0.1, 0.5, Some(1.5), g), None);
    assert_eq!(draw_ahead_s(0.2, 0.3, None, g), None);
    assert_eq!(draw_ahead_s(0.2, 0.7, None, g), Some(0.2 + g));
}

/// A chain built ahead of the reader (draw_ahead_s) draws from where it
/// stands, not from the reader.
#[test]
fn draw_over_from_a_chain_built_ahead() {
    let rate = 48_000u32;
    let d = desc(Some("1M"), false, false, "live", false);
    let shared = Shared::new();
    let tl = Timeline::new(rate, 8.0);
    let three_s = vec![0.0; 3 * rate as usize];
    tl.append(&three_s, &three_s);
    shared.marks.lock().unwrap().push_back(mark(0, 7, 1_000_000, rate, &d));
    let at = 1_000_000 + 2 * rate as u64;
    let mut c = fake_chain(7, rate, at);
    let rd = draw_over(&mut c, &shared, &tl, &d).expect("drawn");
    assert_eq!(rd.start, at);
    assert_eq!(rd.start + rd.l.len() as u64, 1_000_000 + tl.write_pos() + (DRAW_PAST_WRITE_S * rate as f64) as u64);
}
