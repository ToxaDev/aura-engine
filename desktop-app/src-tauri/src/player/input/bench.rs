//! The input's delay measured without a device or a driver: a player in
//! this process plays into the link as fast as it lets it (a host with its
//! own buffer does), the reader feeds the input's session, its real chain is
//! read at a device's pace by the clock (the render thread's way), and what
//! the player sent against what the device has played is the delay from the
//! player to the sound — less the device's own buffer. Nothing is played.
//!
//!   AURA_FILTER_DIR=<fir-optimizer\output> AURA_INPUT_RACKS="1M,linear,2,cpu|5M,linear,8,gpu"
//!   [AURA_INPUT_BENCH_SECS=10] [AURA_INPUT_BENCH_RATE=44100] [AURA_INPUT_BENCH_SUB=15]
//!   [AURA_INPUT_BENCH_OUT=<file.jsonl>] [AURA_INPUT_BENCH_PERIOD_MS=10] (the device takes a period at a time)
//!   cargo test --profile fast --bins input_latency_bench -- --ignored --nocapture --test-threads=1
//!
//! A rack is `taps,phase,fs,cpu|gpu` (phase: linear, minimum, tfs, hybrid,
//! alpha). Each half second a row: the delay and what it is made of —
//! the link and the source stages (sent, not in the live source yet), the
//! live source ahead of the chain, the slow gain's queue, the chain's own
//! look-ahead (the rest), and what is rendered ahead of the device. At the
//! end, how evenly the player was called (the driver calls it when a block
//! fits the link): the intervals against its buffer's period, the calls
//! that came bunched, and the steps the chain took its source in.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aura_link::{region_bytes, Client, HeapRegion, LocalSignals, Mode, Segment, Server};
use serde_json::json;

use super::feed::Feed;
use super::link::{rate_of, Events, Link};
use crate::player::chain::Resources;
use crate::player::gpu::ctx::GpuPolyCtx;
use crate::player::radio::chain as live_chain;
use crate::player::radio::Session;
use crate::player::settings::{Phase, PlayerSettings};

const RING: usize = 1 << 21;
const INT32: u32 = 18;
/// Frames rendered per step (the render thread's).
const STEP: usize = 8192;
/// The player's block (the driver's preferred buffer).
const BLOCK: u64 = 2048;

fn parse_rack(r: &str, sub: u32) -> Option<(PlayerSettings, bool)> {
    let f: Vec<&str> = r.split(',').map(str::trim).collect();
    if f.len() != 4 {
        return None;
    }
    let taps = match f[0] {
        "5k" => 5_000,
        "1M" => 1_000_000,
        "5M" => 5_000_000,
        "10M" => 10_000_000,
        "30M" => 30_000_000,
        _ => return None,
    };
    let phase = match f[1] {
        "linear" => Phase::Linear,
        "minimum" => Phase::Minimum,
        "tfs" => Phase::Tfs,
        "hybrid" => Phase::Hybrid,
        "alpha" => Phase::Alpha,
        _ => return None,
    };
    let fs: u32 = f[2].parse().ok()?;
    let s = PlayerSettings { taps, phase, fs_multiplier: fs, subsonic_hz: sub, ..PlayerSettings::default() };
    Some((s, f[3] == "gpu"))
}

/// The controller in the bench: a session for the format, its feed attached.
#[derive(Default)]
struct Player {
    settings: Mutex<PlayerSettings>,
    link: Mutex<Option<Arc<Link>>>,
    session: Mutex<Option<Arc<Session>>>,
}

impl Events for Player {
    fn format(&self, seg: Segment) {
        let s = self.settings.lock().unwrap().clone();
        let session = Arc::new(Session::start_input(&s, rate_of(&seg), seg.channels, "bench").unwrap());
        *self.session.lock().unwrap() = Some(session.clone());
        let link = self.link.lock().unwrap().clone().unwrap();
        link.attach(Feed::new(session.shared.clone(), seg.channels).unwrap());
    }
    fn started(&self) {}
}

fn on() -> bool {
    true
}

/// The player: a quiet tone (−50 dBFS), in blocks, as fast as the link
/// takes them (the driver's way: a block when it fits); `sent` counts its
/// frames, `calls` when each block was asked for.
fn host(mut client: Client, rate: u64, sent: Arc<AtomicU64>, quit: Arc<AtomicBool>, calls: Arc<Mutex<Vec<Instant>>>) -> Client {
    // The driver's thread runs as "Pro Audio" in the player's process: here,
    // above the pools the chain computes on, or they hold the player up.
    #[cfg(windows)]
    // SAFETY: this thread's own priority.
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
        const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
    client.begin(rate as f64, 2, INT32);
    let amp = 10f64.powf(-50.0 / 20.0);
    let w = 2.0 * std::f64::consts::PI * 440.0 / rate as f64;
    while !quit.load(Ordering::Acquire) {
        if client.mode(Duration::from_secs(5)) == Mode::Released || !client.room(BLOCK * 2) {
            client.wait_room(Duration::from_millis(5));
            continue;
        }
        calls.lock().unwrap().push(Instant::now());
        let n = sent.load(Ordering::Acquire);
        client.write_with((BLOCK * 2) as usize, |i| {
            let f = (n + i as u64 / 2) as f64;
            // Whole in 32 bits, as the driver hands them.
            ((amp * (w * f).sin() * 2_147_483_648.0).round() / 2_147_483_648.0) as f64
        });
        sent.fetch_add(BLOCK, Ordering::AcqRel);
    }
    client
}

fn run_one(rack: &str, rate: u64, sub: u32, secs: f64, res: &Resources) -> serde_json::Value {
    let (s, want_gpu) = parse_rack(rack, sub).expect("rack: taps,phase,fs,cpu|gpu");
    let region = HeapRegion::new(region_bytes(RING));
    let sig = LocalSignals::default();
    let server = Server::create(Box::new(region.clone()), Box::new(sig.clone()), RING, 1);
    let client = Client::open(Box::new(region), Box::new(sig), 2, |_| true).unwrap();
    let p = Arc::new(Player { settings: Mutex::new(s.clone()), ..Default::default() });
    let link = Link::spawn(server, p.clone(), on);
    *p.link.lock().unwrap() = Some(link.clone());
    link.listen();
    let sent = Arc::new(AtomicU64::new(0));
    let quit = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let asks = Arc::new(Mutex::new(Vec::<(Instant, i64)>::new()));
    let h = {
        let (sent, quit, calls) = (sent.clone(), quit.clone(), calls.clone());
        std::thread::spawn(move || host(client, rate, sent, quit, calls))
    };
    let t0 = Instant::now();
    while p.session.lock().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(5), "no session");
        std::thread::sleep(Duration::from_millis(2));
    }
    let session = p.session.lock().unwrap().clone().unwrap();
    let r = (|| -> Result<serde_json::Value, String> {
        // The controller's radio_prepare, less its routing: what gathered
        // while the banks loaded past the chain's level is passed over.
        let live = session.wait_live(Duration::from_secs(5)).map_err(|e| e.1)?;
        let mut plan = live_chain::plan(res, &s, rate as u32, 0)?;
        let keep = (session.shared.margin_s * rate as f64) as i64 + plan.source.latency_frames() as i64;
        let sh = &session.shared;
        let latency_s = plan.source.latency_frames() as f64 / rate as f64;
        let guard_s = (sh.sub_delay_s(rate as u32, plan.out_rate) - latency_s).max(0.0);
        let reads_s = sh.render_ahead_s().unwrap_or(0.0) + sh.input_gain_window_s() + plan.delay_s() + guard_s;
        let over = live.end() - keep - plan.first_need.max((reads_s * rate as f64) as i64);
        if over > 0 {
            plan = live_chain::plan(res, &s, rate as u32, over as u64 * plan.l as u64)?;
            println!("[bench] {:.2} s passed over while the chain was built", over as f64 / rate as f64);
        }
        let need = plan.first_need + keep;
        session.mark(|i, _| {
            i.need_s = need as f64 / rate as f64;
            i.out_rate = plan.out_rate;
            i.delay_s = plan.delay_s();
        });
        session.wait_frames(&live, need).map_err(|e| e.1)?;
        session.shared.first_look(|st, secs| plan.decide_headroom(st, secs));
        let gpu = if want_gpu { GpuPolyCtx::try_build() } else { None };
        let mut chain = live_chain::build(&plan, &live, &session.shared, gpu, session.tally.clone());
        session.mark(|i, ms| i.t_built_ms = Some(ms));
        let out_rate = chain.out_rate as f64;
        let l = chain.l as f64;
        let mut ahead_s = render_ahead(&chain);
        let first_ahead_s = ahead_s;
        let period_ms: f64 =
            std::env::var("AURA_INPUT_BENCH_PERIOD_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0.0);
        let period_frames = (period_ms / 1000.0 * out_rate).round();
        let (mut bl, mut br) = (vec![0.0; STEP], vec![0.0; STEP]);
        let mut rendered = 0u64;
        let mut worst_step = Duration::ZERO;
        // The player's pre-roll (`put_on_air`): 0.3 s rendered before the
        // device opens, and the input's first chain waited for until the
        // source holds its level (`RadioShared::level_short`) — the render
        // getting its look-ahead meanwhile; 3 s at most.
        let t_gate = Instant::now();
        let gate_ahead = ahead_s * out_rate;
        while ((rendered as f64) < 0.3 * out_rate || session.shared.level_short(rendered, gate_ahead, chain.out_rate))
            && t_gate.elapsed() < Duration::from_secs(3)
        {
            if (rendered as f64) < gate_ahead {
                chain.stage.read(&mut bl, &mut br);
                rendered += STEP as u64;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        println!("[bench] the pre-roll took {} ms", t_gate.elapsed().as_millis());
        session.mark(|i, ms| i.t_on_air_ms = Some(ms));
        let t_play = Instant::now();
        let mut underruns = 0u32;
        let mut underrun_s = 0.0f64;
        let mut late = false;
        let mut rows = Vec::new();
        let mut next_row = 0.0;
        // The source's level past what the chain asked for, every turn: the
        // times the chain waited for it (once its level settled, 10 s on),
        // the low over each 4 s; and the least the render held ahead.
        let (mut waits, mut waiting, mut lows, mut low, mut low_at) = (0u32, false, Vec::<f64>::new(), i64::MAX, 0.0f64);
        let mut render_low = f64::MAX;
        while t_play.elapsed().as_secs_f64() < secs {
            let t = t_play.elapsed().as_secs_f64();
            let level = live.ahead_of_asked();
            if level < 0 && !waiting && t >= 10.0 {
                waits += 1;
            }
            waiting = level < 0;
            low = low.min(level);
            if t >= low_at + 4.0 {
                lows.push(low as f64 / rate as f64);
                low = i64::MAX;
                low_at = t;
            }
            if t >= 1.0 {
                render_low = render_low.min((rendered as f64 - t * out_rate) / out_rate);
            }
            // The device takes a period at a time, at its start
            // (`AURA_INPUT_BENCH_PERIOD_MS`; 0, by default: frame by frame).
            let consumed = match period_frames {
                p if p >= 1.0 => (t_play.elapsed().as_secs_f64() * out_rate / p).ceil() * p,
                _ => t_play.elapsed().as_secs_f64() * out_rate,
            };
            if (rendered as f64) < consumed {
                if !late {
                    underruns += 1;
                    late = true;
                    // The render's way: the session keeps more ahead.
                    session.shared.render_underrun();
                    ahead_s = render_ahead(&chain);
                }
                underrun_s += (consumed - rendered as f64) / out_rate;
                rendered = consumed as u64;
            } else {
                late = false;
            }
            if (rendered as f64) < consumed + ahead_s * out_rate {
                let t = Instant::now();
                chain.stage.read(&mut bl, &mut br);
                worst_step = worst_step.max(t.elapsed());
                rendered += STEP as u64;
                asks.lock().unwrap().push((Instant::now(), live.end() - live.ahead_of_asked()));
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
            let t = t_play.elapsed().as_secs_f64();
            if t >= next_row {
                next_row += 0.5;
                let consumed = t * out_rate;
                let heard = consumed / l / rate as f64;
                let sent_s = sent.load(Ordering::Acquire) as f64 / rate as f64;
                let in_s = live.stats().real_frames as f64 / rate as f64;
                let live_s = live.ahead() as f64 / rate as f64;
                let gain_s = session.shared.gain_queue.load(Ordering::Relaxed) as f64 / out_rate;
                let render_s = (rendered as f64 - consumed) / out_rate;
                let total = sent_s - heard;
                let chain_s = in_s - heard - live_s - gain_s - render_s;
                let link_s = session.shared.link_frames.load(Ordering::Relaxed) as f64 / rate as f64;
                // What the status says, by its own counters, against the truth.
                let said = session.shared.input_delay(Some((heard * rate as f64) as i64))["totalS"].as_f64();
                rows.push(json!({
                    "t": (t * 1000.0).round() / 1000.0,
                    "totalS": total,
                    "statusS": said,
                    "linkS": link_s,
                    "stagesS": sent_s - in_s - link_s,
                    "liveS": live_s,
                    "gainS": gain_s,
                    "chainS": chain_s,
                    "renderS": render_s,
                }));
                println!(
                    "[bench] {:>5.2} s  total {:.3} (status {:.3})  link {:.3}  stages {:.3}  live {:.3}  gain {:.3}  chain {:.3}  render {:.3}",
                    t, total, said.unwrap_or(f64::NAN), link_s, sent_s - in_s - link_s, live_s, gain_s, chain_s, render_s
                );
            }
        }
        let st = live.stats();
        let seen = calls_seen(&calls.lock().unwrap(), &asks.lock().unwrap(), t_play + Duration::from_secs(2), rate);
        let by_ten = pace_by_ten(&calls.lock().unwrap(), t_play, BLOCK as f64 / rate as f64);
        println!(
            "[bench] the chain waited {waits} times from 10 s on; the level's low by 4 s {}; the render at least {render_low:.3} s ahead; the pace by 10 s, % off the rate: {}",
            lows.iter().map(|l| format!("{l:.3}")).collect::<Vec<_>>().join(" "),
            by_ten.iter().map(|p| format!("{p:+.3}")).collect::<Vec<_>>().join(" ")
        );
        Ok(json!({
            "rack": rack, "rate": rate, "sub": sub, "periodMs": period_ms, "gpu": chain.gpu_on, "outRate": chain.out_rate,
            "needS": need as f64 / rate as f64, "delayS": plan.delay_s(),
            "stagesS": plan.source.latency_frames() as f64 / rate as f64,
            "renderAheadS": [first_ahead_s, ahead_s], "worstStepMs": worst_step.as_secs_f64() * 1000.0,
            "underruns": underruns, "underrunS": underrun_s, "starves": st.starves,
            "waits": waits, "lowsS": lows, "renderLowS": render_low, "paceBy10Pct": by_ten,
            "calls": seen, "last": rows.last().cloned(), "rows": rows,
        }))
    })();
    quit.store(true, Ordering::Release);
    session.stop();
    let _ = h.join();
    link.stop();
    match r {
        Ok(v) => v,
        Err(e) => json!({ "rack": rack, "error": e }),
    }
}

/// The pace of calls `period` apart at the stream's rate, % off it, each
/// second: the slope of the calls over the 5 s from there (least squares,
/// so a call a few milliseconds early or late moves it by little).
fn pace_by_the_second(t: &[Instant], period: f64) -> Vec<f64> {
    let Some(&t0) = t.first() else { return Vec::new() };
    let s: Vec<f64> = t.iter().map(|&x| (x - t0).as_secs_f64()).collect();
    let mut out = Vec::new();
    let mut from = 0.0;
    while from + 5.0 <= s[s.len() - 1] {
        let w: Vec<(f64, f64)> = s.iter().enumerate().filter(|(_, &x)| x >= from && x < from + 5.0).map(|(i, &x)| (i as f64, x)).collect();
        let n = w.len() as f64;
        let (mi, mx) = (w.iter().map(|p| p.0).sum::<f64>() / n, w.iter().map(|p| p.1).sum::<f64>() / n);
        let b = w.iter().map(|p| (p.0 - mi) * (p.1 - mx)).sum::<f64>() / w.iter().map(|p| (p.0 - mi).powi(2)).sum::<f64>();
        out.push((period / b - 1.0) * 100.0);
        from += 1.0;
    }
    out
}

/// The pace of the calls over each 10 s from `from` on, % off the rate:
/// their slope over it (least squares — a call late at an end alone would
/// read as a tenth of a percent).
fn pace_by_ten(calls: &[Instant], from: Instant, period: f64) -> Vec<f64> {
    let s: Vec<f64> = calls.iter().filter(|&&t| t >= from).map(|&t| (t - from).as_secs_f64()).collect();
    let mut out = Vec::new();
    let mut a = 0.0;
    while s.last().is_some_and(|&l| a + 10.0 <= l) {
        let w: Vec<(f64, f64)> = s.iter().enumerate().filter(|(_, &x)| x >= a && x < a + 10.0).map(|(i, &x)| (i as f64, x)).collect();
        if w.len() > 1 {
            let n = w.len() as f64;
            let (mi, mx) = (w.iter().map(|p| p.0).sum::<f64>() / n, w.iter().map(|p| p.1).sum::<f64>() / n);
            let b = w.iter().map(|p| (p.0 - mi) * (p.1 - mx)).sum::<f64>() / w.iter().map(|p| (p.0 - mi).powi(2)).sum::<f64>();
            out.push((period / b - 1.0) * 100.0);
        }
        a += 10.0;
    }
    out
}

/// The player's calls once the start is over (`from`): the intervals
/// between them against its buffer's period — mean, spread, longest, the
/// share over 1.5 periods — and how many came bunched (under 5 ms apart);
/// and the steps the chain took its source in (`asks`: what it had asked
/// for after each render step).
fn calls_seen(calls: &[Instant], asks: &[(Instant, i64)], from: Instant, rate: u64) -> serde_json::Value {
    let period = BLOCK as f64 / rate as f64;
    let t: Vec<Instant> = calls.iter().copied().filter(|&t| t >= from).collect();
    let iv: Vec<f64> = t.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
    let n = iv.len().max(1) as f64;
    let mean = iv.iter().sum::<f64>() / n;
    let sd = (iv.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
    let max = iv.iter().copied().fold(0.0, f64::max);
    let over = iv.iter().filter(|&&x| x > 1.5 * period).count() as f64 / n;
    let mut bunches = std::collections::BTreeMap::<usize, u32>::new();
    let mut run = 1;
    for &x in &iv {
        if x < 0.005 {
            run += 1;
        } else {
            *bunches.entry(run).or_default() += 1;
            run = 1;
        }
    }
    *bunches.entry(run).or_default() += 1;
    let mut steps = std::collections::BTreeMap::<i64, u32>::new();
    for w in asks.iter().filter(|a| a.0 >= from).collect::<Vec<_>>().windows(2) {
        if w[1].1 > w[0].1 {
            *steps.entry(w[1].1 - w[0].1).or_default() += 1;
        }
    }
    println!(
        "[bench] calls: {} every {:.1} ms (sd {:.1}, max {:.1}; {:.1} % over 1.5 periods) for a period of {:.1} ms; bunches (calls: times) {:?}; the chain's steps (frames: times) {:?}",
        t.len(), mean * 1e3, sd * 1e3, max * 1e3, over * 100.0, period * 1e3, bunches, steps
    );
    let pace = pace_by_the_second(&t, period);
    let change = pace.windows(2).map(|w| (w[1] - w[0]).abs() * 1e4).fold(0.0, f64::max);
    let (lo, hi) = pace.iter().fold((f64::MAX, f64::MIN), |(a, b), &p| (a.min(p), b.max(p)));
    println!(
        "[bench] pace by the second (over 5 s), % off the rate: {}; from {lo:+.3} to {hi:+.3}, the largest change from one second to the next {change:.0} ppm",
        pace.iter().map(|p| format!("{p:+.3}")).collect::<Vec<_>>().join(" "),
    );
    json!({
        "periodMs": period * 1e3, "meanMs": mean * 1e3, "sdMs": sd * 1e3, "maxMs": max * 1e3, "over": over,
        "pacePct": [lo, hi], "paceChangePpm": change,
        "bunches": bunches.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "chainSteps": steps.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
    })
}

/// How far ahead of the device the render thread keeps this chain: its
/// session's own (`RadioShared::render_ahead_s`), or the render's first
/// target (1.5 s) for a session without one.
fn render_ahead(chain: &crate::player::chain::Chain) -> f64 {
    chain.radio.upgrade().and_then(|r| r.render_ahead_s()).unwrap_or(1.5)
}

#[test]
#[ignore]
fn input_latency_bench() {
    let racks = std::env::var("AURA_INPUT_RACKS").unwrap_or_else(|_| "1M,linear,2,cpu".into());
    let secs: f64 = std::env::var("AURA_INPUT_BENCH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0);
    let rate: u64 = std::env::var("AURA_INPUT_BENCH_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(44_100);
    let sub: u32 = std::env::var("AURA_INPUT_BENCH_SUB").ok().and_then(|v| v.parse().ok()).unwrap_or(15);
    let res = Resources::new(std::env::temp_dir().join("aura-input-bench"));
    let mut out = std::env::var("AURA_INPUT_BENCH_OUT").ok().map(|p| std::fs::File::create(p).expect("the bench's output"));
    for rack in racks.split([';', '|']).map(str::trim).filter(|r| !r.is_empty()) {
        let v = run_one(rack, rate, sub, secs, &res);
        let mut short = v.clone();
        if let Some(o) = short.as_object_mut() {
            o.remove("rows");
        }
        println!("[bench] {}", short);
        if let Some(f) = out.as_mut() {
            writeln!(f, "{}", v).unwrap();
        }
    }
}
