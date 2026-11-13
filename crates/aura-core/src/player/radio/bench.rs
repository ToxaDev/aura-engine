//! The radio experiment without a device: a real stream, its live chain
//! read at a device's pace by the clock (the render thread's way: up to
//! 1.5 s ahead of the reader), measured. Nothing is played.
//!
//!   AURA_RADIO_URL=https://… AURA_RADIO_RACKS="1M,linear,8,cpu;5k,linear,2,cpu"
//!   AURA_RADIO_SECS=30 AURA_RADIO_OUT=<file.jsonl> [AURA_RADIO_DUMP=<dir>]
//!   cargo test --profile fast --bins radio_bench -- --ignored --nocapture --test-threads=1
//!
//! A rack is `taps,phase,fs,cpu|gpu` (phase: linear, minimum, tfs, hybrid).
//! With AURA_RADIO_DUMP the first connection's bytes are decoded again as a
//! file (the file player's decoder) and compared with what the stream gave.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::player::chain::Resources;
use crate::player::gpu::ctx::GpuPolyCtx;
use crate::player::settings::{Phase, PlayerSettings};

use super::chain as live_chain;
use super::Session;

const STEP: usize = 8192;
const AHEAD_S: f64 = 1.5;

fn parse_rack(r: &str) -> Option<(PlayerSettings, bool)> {
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
    let s = PlayerSettings { taps, phase, fs_multiplier: fs, ..PlayerSettings::default() };
    Some((s, f[3] == "gpu"))
}

fn run_one(url: &str, rack: &str, secs: f64, res: &Resources) -> serde_json::Value {
    let (s, want_gpu) = parse_rack(rack).expect("rack: taps,phase,fs,cpu|gpu");
    let session = Session::start(url, &s);
    let r = (|| -> Result<serde_json::Value, String> {
        let live = session.wait_live(Duration::from_secs(30)).map_err(|e| e.1)?;
        let rate = live.rate();
        let t_plan = Instant::now();
        let plan = live_chain::plan(res, &s, rate, 0)?;
        let plan_s = t_plan.elapsed().as_secs_f64();
        let need = plan.first_need + (session.shared.margin_s * rate as f64) as i64 + plan.source.latency_frames() as i64;
        let gpu = if want_gpu { GpuPolyCtx::try_build() } else { None };
        session.mark(|i, ms| i.t_planned_ms = Some(ms));
        session.wait_frames(&live, need).map_err(|e| e.1)?;
        session.mark(|i, ms| i.t_ready_ms = Some(ms));
        let t_build = Instant::now();
        let vram_mb = |g: &Option<std::sync::Arc<GpuPolyCtx>>| g.as_ref().map(|c| c.free_vram_bytes() as f64 / 1_048_576.0);
        let free_before = vram_mb(&gpu);
        let gpu_probe = gpu.clone();
        let mut chain = live_chain::build(&plan, &live, &session.shared, gpu, session.tally.clone());
        let build_s = t_build.elapsed().as_secs_f64();
        let free_after = vram_mb(&gpu_probe);
        session.mark(|i, ms| i.t_built_ms = Some(ms));
        let out_rate = chain.out_rate as f64;
        let (mut bl, mut br) = (vec![0.0; STEP], vec![0.0; STEP]);
        let mut rendered = 0u64;
        let mut busy = Duration::ZERO;
        // The player's pre-roll: 0.3 s rendered before the device opens.
        let mut pre = (Vec::new(), Vec::new());
        while (rendered as f64) < 0.3 * out_rate {
            let t0 = Instant::now();
            chain.stage.read(&mut bl, &mut br);
            busy += t0.elapsed();
            pre.0.extend_from_slice(&bl);
            pre.1.extend_from_slice(&br);
            rendered += STEP as u64;
        }
        let preroll_s = t_build.elapsed().as_secs_f64() - build_s;
        // The device starts reading now: output frame k is heard k frames on.
        let t_play = Instant::now();
        let play_at = session.shared.elapsed_s();
        let mut first_sound: Option<f64> = None;
        for i in 0..pre.0.len() {
            if pre.0[i].abs().max(pre.1[i].abs()) > 1e-5 {
                first_sound = Some(play_at + i as f64 / out_rate);
                break;
            }
        }
        let mut underrun_events = 0u32;
        let mut underrun_s = 0.0f64;
        let mut late = false;
        let mut peak = 0.0f64;
        let mut rows = Vec::new();
        let mut next_row = 0.0;
        while t_play.elapsed().as_secs_f64() < secs {
            let consumed = t_play.elapsed().as_secs_f64() * out_rate;
            if (rendered as f64) < consumed {
                // The device would have read past what is rendered.
                if !late {
                    underrun_events += 1;
                    late = true;
                }
                underrun_s += (consumed - rendered as f64) / out_rate;
                rendered = consumed as u64;
            } else {
                late = false;
            }
            if (rendered as f64) < consumed + AHEAD_S * out_rate {
                let t0 = Instant::now();
                chain.stage.read(&mut bl, &mut br);
                busy += t0.elapsed();
                for i in 0..STEP {
                    let a = bl[i].abs().max(br[i].abs());
                    peak = peak.max(a);
                    if first_sound.is_none() && a > 1e-5 {
                        first_sound = Some(play_at + (rendered + i as u64) as f64 / out_rate);
                    }
                }
                rendered += STEP as u64;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
            let t = t_play.elapsed().as_secs_f64();
            if t >= next_row {
                next_row += 5.0;
                let st = live.stats();
                rows.push(json!({
                    "t": t,
                    "bufferS": live.ahead() as f64 / rate as f64,
                    "inS": st.real_frames as f64 / rate as f64,
                    "silentS": st.silent_frames as f64 / rate as f64,
                    "starves": st.starves,
                }));
            }
        }
        let played_s = t_play.elapsed().as_secs_f64();
        let st = live.stats();
        let info = session.shared.info.lock().unwrap().clone();
        let lt = session.tally.lock().unwrap().clone();
        let rtf = (rendered as f64 / out_rate) / busy.as_secs_f64().max(1e-9);
        let out = json!({
            "url": url, "rack": rack, "gpu": chain.gpu_on, "codec": info.codec, "rate": rate,
            "bits": info.bits, "outRate": chain.out_rate, "delayS": plan.delay_s(),
            "needS": need as f64 / rate as f64, "planS": plan_s, "buildS": build_s, "prerollS": preroll_s,
            "tConnectedMs": info.t_connected_ms, "tFirstByteMs": info.t_first_byte_ms,
            "tFirstPcmMs": info.t_first_pcm_ms, "tPlannedMs": info.t_planned_ms,
            "tReadyMs": info.t_ready_ms, "tBuiltMs": info.t_built_ms,
            "firstSoundS": first_sound, "playedS": played_s, "rtf": rtf,
            "underrunEvents": underrun_events, "underrunS": underrun_s,
            "starves": st.starves, "silentS": st.silent_frames as f64 / rate as f64,
            "maxWaitMs": st.max_wait_ms, "gaps": st.gaps, "peak": peak,
            "limiter": {
                "reducedPct": if lt.samples > 0 { 100.0 * lt.reduced as f64 / lt.samples as f64 } else { 0.0 },
                "meanDb": if lt.reduced > 0 { lt.reduced_db_sum / lt.reduced as f64 } else { 0.0 },
                "maxDb": lt.max_db,
            },
            "source": session.shared.source_tally.lock().unwrap().clone(),
            "tokens": chain.tokens,
            "vramFreeMb": [free_before, free_after],
            "arrival": info.arrival, "bytes": info.bytes, "connects": info.connects,
            "resets": info.resets, "titles": info.titles, "title": info.title,
            "decodeErrors": info.decode_errors, "joinGaps": info.join_gaps, "hls": info.hls, "rows": rows,
        });
        Ok(out)
    })();
    // The file decoder on the same bytes, when they were kept: once the
    // connection is closed.
    session.stop();
    std::thread::sleep(Duration::from_millis(500));
    let dump_cmp = compare_dump(&session);
    match r {
        Ok(mut v) => {
            v["dump"] = dump_cmp;
            v
        }
        Err(e) => {
            let i = session.shared.info.lock().unwrap().clone();
            json!({
                "url": url, "rack": rack, "error": e, "codec": i.codec, "rate": i.rate, "bits": i.bits,
                "contentType": i.content_type, "icyBr": i.icy_br, "connects": i.connects,
                "lastError": i.last_error, "bytes": i.bytes, "arrival": i.arrival, "hls": i.hls,
            })
        }
    }
}

/// Decode the first connection's dump as a file and compare it, frame by
/// frame, with what the live source holds from its start.
fn compare_dump(session: &Session) -> serde_json::Value {
    let Some(dir) = std::env::var_os("AURA_RADIO_DUMP") else { return serde_json::Value::Null };
    let Some(live) = session.shared.live.get() else { return serde_json::Value::Null };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let s = p.to_string_lossy();
                    s.contains("-1.") && !s.contains(".cut.")
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let Some(mut path) = files.last().cloned() else { return json!({ "error": "no dump" }) };
    // An Ogg dump ends inside a page: a copy without it decodes as a file.
    if path.extension().is_some_and(|e| e == "ogg") {
        if let Ok(b) = std::fs::read(&path) {
            if let Some(cut) = b.windows(4).rposition(|w| w == b"OggS") {
                let p2 = path.with_extension("cut.ogg");
                if std::fs::write(&p2, &b[..cut]).is_ok() {
                    path = p2;
                }
            }
        }
    }
    let a = match crate::audio::converter::decode::decode_file(&path) {
        Ok(a) => a,
        Err(e) => return json!({ "file": path.display().to_string(), "error": e }),
    };
    let n = a.samples_l.len();
    let mut l = vec![0.0; n];
    let mut r = vec![0.0; n];
    live.read(0, 0, &mut l);
    live.read(1, 0, &mut r);
    let mut same = 0usize;
    let mut first_diff = None;
    let mut max_diff = 0.0f64;
    for i in 0..n {
        let d = (l[i] - a.samples_l[i]).abs().max((r[i] - a.samples_r[i]).abs());
        if d == 0.0 {
            same += 1;
        } else {
            first_diff.get_or_insert(i);
            max_diff = max_diff.max(d);
        }
    }
    json!({
        "file": path.display().to_string(), "fileFrames": n, "fileRate": a.sample_rate,
        "liveFrames": live.end(), "identical": same, "firstDiff": first_diff, "maxDiff": max_diff,
    })
}

/// The process's memory now (working set), MB.
fn rss_mb() -> f64 {
    use sysinfo::{PidExt, ProcessExt, System, SystemExt};
    thread_local! {
        static SYS: std::cell::RefCell<System> = std::cell::RefCell::new(System::new());
    }
    let pid = sysinfo::Pid::from_u32(std::process::id());
    SYS.with(|s| {
        let mut sys = s.borrow_mut();
        sys.refresh_processes();
        sys.process(pid).map_or(0.0, |p| p.memory() as f64 / 1_048_576.0)
    })
}

/// A song cut out of a block (frames from..from+n), as a 16-bit WAV: the
/// block is 16-bit FLAC, so nothing is lost.
fn write_wav16(path: &std::path::Path, rate: u32, l: &[f64], r: &[f64]) {
    let n = l.len();
    let mut b = Vec::with_capacity(44 + n * 4);
    let data = (n * 4) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        for v in [l[i], r[i]] {
            b.extend_from_slice(&((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
        }
    }
    std::fs::write(path, b).expect("write the song");
}

/// A Radio Paradise block played the file way: the whole block, and one
/// song cut out of it, through the full rack's preparation (and the
/// Hybrid-Phase envelope) — time and the process's peak memory.
///   AURA_RP_BLOCK=<block.flac> AURA_RP_SONG_MS=<elapsed>,<duration>
///   cargo test --profile fast --bins rp_block_prepare -- --ignored --nocapture --test-threads=1
#[test]
#[ignore]
fn rp_block_prepare() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let block = std::path::PathBuf::from(std::env::var("AURA_RP_BLOCK").expect("AURA_RP_BLOCK"));
    let song = std::env::var("AURA_RP_SONG_MS").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?))
    });
    // The peak: a thread samples the working set every 50 ms.
    let peak = std::sync::Arc::new(std::sync::Mutex::new(0.0f64));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let (pk, st) = (peak.clone(), stop.clone());
    let sampler = std::thread::spawn(move || {
        while !st.load(Ordering::Relaxed) {
            let m = rss_mb();
            let mut p = pk.lock().unwrap();
            *p = p.max(m);
            drop(p);
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let reset_peak = |p: &std::sync::Arc<std::sync::Mutex<f64>>| *p.lock().unwrap() = rss_mb();
    let res = Resources::new(std::env::temp_dir().join("aura-rp-block"));
    let measure = |path: &std::path::Path, label: &str| -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("what".into(), json!(label));
        reset_peak(&peak);
        let base = rss_mb();
        let t = Instant::now();
        let a = crate::audio::converter::decode::decode_file(path).expect("decode");
        out.insert("decodeS".into(), json!(t.elapsed().as_secs_f64()));
        out.insert("minutes".into(), json!(a.samples_l.len() as f64 / a.sample_rate as f64 / 60.0));
        drop(a);
        let cancel = AtomicBool::new(false);
        for (name, phase) in [("full TFS", Phase::Tfs), ("full HP", Phase::Hybrid)] {
            reset_peak(&peak);
            let s = PlayerSettings { phase, ..PlayerSettings::default() };
            let t = Instant::now();
            let v = crate::player::chain::prepare_variant(path, &s, false, &cancel).expect("prepare");
            let prep = t.elapsed().as_secs_f64();
            let mut env_s = None;
            if phase == Phase::Hybrid {
                let t = Instant::now();
                let r = res.envelope(&format!("rp|{}", label), &v);
                env_s = Some((t.elapsed().as_secs_f64(), r.is_ok()));
            }
            out.insert(
                name.into(),
                json!({ "prepareS": prep, "envelope": env_s, "tokens": v.tokens, "peakMb": *peak.lock().unwrap() - base,
                        "sourceMb": (v.src.len() * 16) as f64 / 1_048_576.0 }),
            );
        }
        serde_json::Value::Object(out)
    };
    let whole = measure(&block, "block");
    println!("RESULT {}", whole);
    if let Some((from_ms, dur_ms)) = song {
        let a = crate::audio::converter::decode::decode_file(&block).expect("decode");
        let r = a.sample_rate as f64;
        let (i0, n) = ((from_ms / 1000.0 * r) as usize, (dur_ms / 1000.0 * r) as usize);
        let i1 = (i0 + n).min(a.samples_l.len());
        let wav = std::env::temp_dir().join("aura-rp-song.wav");
        write_wav16(&wav, a.sample_rate, &a.samples_l[i0..i1], &a.samples_r[i0..i1]);
        drop(a);
        let one = measure(&wav, "song");
        println!("RESULT {}", one);
        let _ = std::fs::remove_file(&wav);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
}

#[test]
#[ignore]
fn radio_bench() {
    // Several streams: separated by '|'.
    let urls = std::env::var("AURA_RADIO_URL").expect("AURA_RADIO_URL");
    let racks = std::env::var("AURA_RADIO_RACKS").unwrap_or_else(|_| "1M,linear,8,cpu".into());
    let secs: f64 = std::env::var("AURA_RADIO_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30.0);
    let out = std::env::var("AURA_RADIO_OUT").ok();
    let cache = std::env::temp_dir().join("aura-radio-bench");
    let res = Resources::new(cache);
    let runs: Vec<(String, String)> = urls
        .split('|')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .flat_map(|u| racks.split(';').map(str::trim).filter(|r| !r.is_empty()).map(move |r| (u.to_string(), r.to_string())))
        .collect();
    for (url, rack) in &runs {
        let (url, rack) = (url.as_str(), rack.as_str());
        let v = run_one(url, rack, secs, &res);
        let line = v.to_string();
        println!("RESULT {}", line);
        if let Some(p) = &out {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", line);
            }
        }
        // Let the server see one listener at a time.
        std::thread::sleep(Duration::from_millis(500));
    }
}
