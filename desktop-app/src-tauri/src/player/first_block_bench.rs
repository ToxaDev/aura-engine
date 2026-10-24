//! What a start waits for before its first sound, piece by piece, with the
//! real filters: the first variant (decode and the whole-track stages), the
//! chain's build, and the reads a new stream's pre-roll (0.3 s of output)
//! waits for, against the same amount of output once the chain runs.
//!
//!     AURA_FILTER_DIR=<blobs> cargo test --profile fast --bins first_block_cost -- --ignored --nocapture
//!
//! `AURA_K1_TRACK` plays a file of your own (else four minutes of something
//! with the shape of music); `AURA_BENCH_AFFINITY=0xF0000 RAYON_NUM_THREADS=4`
//! the four E-cores; `AURA_K1_ROUNDS` the measured rounds (5);
//! `AURA_K1_CASES` a filter on the case names (e.g. "x16").

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use super::chain::{build_chain, multistage_for, prepare_variant, Resources};
use super::convolver::{Alignment, Input, PolyStream};
use super::settings::{Geometry, Phase, PlayerSettings, Upsampler};
use super::stages::{InterpStage, Stage};

const STEP: usize = 8_192;

fn music(n: usize, rate: u32, seed: u64) -> Vec<f64> {
    let mut x = seed;
    let beat = (rate / 2) as usize;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((x >> 33) as f64 / u32::MAX as f64 - 0.5) * 0.05;
            let t = i as f64 / rate as f64;
            let tones = 0.3 * (2.0 * std::f64::consts::PI * 220.0 * t).sin() + 0.2 * (2.0 * std::f64::consts::PI * 1_760.0 * t).sin();
            let k = i % beat;
            let hit = if k < 2_000 { 0.9 * (-(k as f64) / 300.0).exp() * ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) } else { 0.0 };
            tones + noise + hit
        })
        .collect()
}

fn write_wav16(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
    let n = l.len();
    let data_len = (n * 4) as u32;
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
    for i in 0..n {
        for s in [l[i], r[i]] {
            let v = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
            f.write_all(&v.to_le_bytes()).unwrap();
        }
    }
}

#[cfg(windows)]
fn set_process_mask(mask: usize) -> bool {
    let Ok(mask) = u32::try_from(mask) else { return false };
    unsafe {
        use winapi::um::processthreadsapi::GetCurrentProcess;
        use winapi::um::winbase::SetProcessAffinityMask;
        SetProcessAffinityMask(GetCurrentProcess(), mask) != 0
    }
}

#[cfg(not(windows))]
fn set_process_mask(_mask: usize) -> bool {
    false
}

fn affinity() -> String {
    let spec = std::env::var("AURA_BENCH_AFFINITY").unwrap_or_default().trim().to_ascii_lowercase();
    if spec.is_empty() || spec == "all" {
        return "all".into();
    }
    let m = usize::from_str_radix(spec.trim_start_matches("0x"), 16).expect("AURA_BENCH_AFFINITY: a hex mask such as 0xF0000");
    assert!(set_process_mask(m), "SetProcessAffinityMask({:#x}) failed", m);
    format!("{}E", m.count_ones())
}

/// The listening room of the rack this was measured for: XTC on.
fn room() -> Geometry {
    Geometry { speaker_span_mm: 1600.0, left_distance_mm: 1096.5856099730654, right_distance_mm: 1096.5856099730654, head_width_mm: 180.0 }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Median and spread (min–max) of `v`.
fn stat(v: &[f64]) -> String {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!("{:>6.0} ({:.0}–{:.0})", s[s.len() / 2], s[0], s[s.len() - 1])
}

/// Read `frames` of output a STEP at a time; the time of each read.
fn reads(stage: &mut dyn Stage, frames: usize) -> Vec<f64> {
    let (mut bl, mut br) = (vec![0.0; STEP], vec![0.0; STEP]);
    let mut out = Vec::new();
    let mut got = 0;
    while got < frames {
        let t = Instant::now();
        stage.read(&mut bl, &mut br);
        out.push(ms(t));
        got += STEP;
    }
    out
}

#[test]
#[ignore]
fn first_block_cost() {
    let mask = affinity();
    let rounds: usize = std::env::var("AURA_K1_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let only = std::env::var("AURA_K1_CASES").unwrap_or_default();
    let dir = tempfile::tempdir().unwrap();
    let track: PathBuf = match std::env::var("AURA_K1_TRACK") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            let rate = 44_100u32;
            let n = (240 * rate) as usize;
            let p = dir.path().join("music.wav");
            write_wav16(&p, rate, &music(n, rate, 3), &music(n, rate, 4));
            p
        }
    };
    println!(
        "[k1] {} | mask {} | rayon {} | track {}",
        "first block cost",
        mask,
        rayon::current_num_threads(),
        track.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut cases = Vec::new();
    for fs in [4u32, 16] {
        for up in [Upsampler::Multi, Upsampler::Single] {
            for phase in [Phase::Linear, Phase::Alpha] {
                cases.push((fs, up, phase));
            }
        }
    }
    for (fs, up, phase) in cases {
        let name = format!("x{} {:?} {:?}", fs, up, phase);
        if !only.is_empty() && !only.split(',').any(|w| name.contains(w.trim())) {
            continue;
        }
        let s = PlayerSettings {
            fs_multiplier: fs,
            taps: 30_000_000,
            phase,
            subsonic_hz: 10,
            xtc: true,
            xtc_geometry: room(),
            use_gpu: false,
            upsampler: Some(up),
            ..PlayerSettings::default()
        };
        assert!(s.xtc_active(), "XTC on");
        let res = Resources::new(dir.path().join(format!("cache-{}-{:?}-{:?}", fs, up, phase)));
        let cancel = AtomicBool::new(false);
        let (mut t_var, mut t_build, mut t_pre, mut t_first, mut t_steady) = (vec![], vec![], vec![], vec![], vec![]);
        let (mut t_build_w, mut t_pre_w, mut t_steady_w) = (vec![], vec![], vec![]);
        let (mut t_ready, mut t_21, mut t_85) = (vec![], vec![], vec![]);
        let mut pre_steps = 0;
        for round in 0..=rounds {
            let t = Instant::now();
            let v = Arc::new(prepare_variant(&track, &s, true, &cancel).expect("the first variant"));
            let var_ms = ms(t);
            // Where the stream of source stages stands: at the variant's
            // return, and when it reached 21 s and 85 s (from the press).
            let grow = v.stream.as_ref().map(|ss| ss.grow.clone());
            let ready_at_var = grow.as_ref().map_or(0, |g| g.ready()) as f64 / v.src.rate as f64;
            let watch = grow.clone().map(|g| {
                let rate = v.src.rate as usize;
                std::thread::spawn(move || {
                    let mut hit = [0.0f64; 2];
                    for (k, secs) in [21usize, 85].into_iter().enumerate() {
                        let want = (secs * rate).min(g.total());
                        while g.ready() < want && !g.is_done() {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        }
                        hit[k] = ms(t);
                    }
                    hit
                })
            });
            let pre = (0.3 * v.out_rate as f64).ceil() as usize;
            let tk = format!("k1:{}", track.display());
            // As a start does: the chain on the stream just begun.
            let t = Instant::now();
            let mut chain = build_chain(&res, 1, &tk, &v, &s, 0, None, None, 0).expect("chain");
            let build_ms = ms(t);
            let r = reads(chain.stage.as_mut(), pre);
            pre_steps = r.len();
            let pre_ms: f64 = r.iter().sum();
            let first_ms = r[0];
            // The same amount of output once it runs: 3 s on, averaged.
            let more = reads(chain.stage.as_mut(), 3 * v.out_rate as usize);
            let steady_ms = more.iter().sum::<f64>() * pre as f64 / (more.len() * STEP) as f64;
            // The same start with the source stages through (no wait for
            // them): what the chain itself costs.
            if let Some(ss) = &v.stream {
                ss.grow.wait_done(&cancel);
            }
            let t = Instant::now();
            let mut chain2 = build_chain(&res, 1, &tk, &v, &s, 0, None, None, 0).expect("chain");
            let build_w = ms(t);
            let pre_w: f64 = reads(chain2.stage.as_mut(), pre).iter().sum();
            // And its steady cost with nothing else running beside it.
            let more_w = reads(chain2.stage.as_mut(), 3 * v.out_rate as usize);
            let steady_w = more_w.iter().sum::<f64>() * pre as f64 / (more_w.len() * STEP) as f64;
            let hit = watch.map(|h| h.join().unwrap()).unwrap_or_default();
            if round > 0 {
                t_ready.push(ready_at_var);
                t_21.push(hit[0]);
                t_85.push(hit[1]);
            }
            drop(chain2);
            drop(chain);
            if round == 0 {
                println!("[k1] {}: warm-up round: variant {:.0} ms, build {:.0} ms, pre-roll {:.0} ms", name, var_ms, build_ms, pre_ms);
                continue;
            }
            t_var.push(var_ms);
            t_build.push(build_ms);
            t_pre.push(pre_ms);
            t_first.push(first_ms);
            t_steady.push(steady_ms);
            t_build_w.push(build_w);
            t_pre_w.push(pre_w);
            t_steady_w.push(steady_w);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        println!(
            "[k1] {:<18} variant {} | build {} | pre-roll ({} reads) {} [first read {}] | same output later {} | source through: build {} pre-roll {} later {}",
            name,
            stat(&t_var),
            stat(&t_build),
            pre_steps,
            stat(&t_pre),
            stat(&t_first),
            stat(&t_steady),
            stat(&t_build_w),
            stat(&t_pre_w),
            stat(&t_steady_w)
        );
        println!(
            "[k1] {:<18} source stream: {:.1} s in at the variant's return | 21 s at {} | 85 s at {} (ms from the press)",
            name,
            t_ready.iter().sum::<f64>() / t_ready.len().max(1) as f64,
            stat(&t_21),
            stat(&t_85)
        );
    }
}

/// The convolver alone on a decoded track, the bank warm: its build (the
/// FDL's prime), its first read (the first block's multiply-accumulate and
/// inverse transforms) and a block later on, Single and the multistage stage
/// 0 (+ its short chain), ×4 and ×16.
#[test]
#[ignore]
fn first_block_convolver_cost() {
    let mask = affinity();
    let rounds: usize = std::env::var("AURA_K1_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let dir = tempfile::tempdir().unwrap();
    let rate = 44_100u32;
    let n = (240 * rate) as usize;
    let src = Arc::new(super::convolver::SourceBuf { l: music(n, rate, 3), r: music(n, rate, 4), rate });
    println!("[k1c] convolver alone | mask {} | rayon {}", mask, rayon::current_num_threads());
    let res = Resources::new(dir.path().join("cache"));
    for fs in [4u32, 16] {
        let out_rate = rate * fs;
        let l = fs as usize;
        for phase in ["linear_phase", "minimum_phase"] {
            let Some(path) = crate::audio::converter::dsp::filter::find_precomputed_filter(30_000_000, rate, out_rate, phase) else {
                println!("[k1c] no 30M {} at {}", phase, out_rate);
                continue;
            };
            let align = if phase == "linear_phase" { Alignment::Linear } else { Alignment::BandWeighted };
            let s = PlayerSettings { fs_multiplier: fs, taps: 30_000_000, upsampler: Some(Upsampler::Multi), ..PlayerSettings::default() };
            for multi in [true, false] {
                let bank = if multi {
                    let Some(chain) = multistage_for(&s, rate, l, false) else { continue };
                    res.bank_stage0(&path, l, align, out_rate, chain).expect("stage 0")
                } else {
                    res.bank(&path, l, align, out_rate).expect("bank")
                };
                let (mut t_build, mut t_first, mut t_later) = (vec![], vec![], vec![]);
                for round in 0..=rounds {
                    let t = Instant::now();
                    let mut st: Box<dyn Stage> = if multi {
                        let plan = bank.ms.clone().unwrap();
                        let up = PolyStream::with_input_shared(bank.clone(), Input::File(src.clone()), InterpStage::stage0_start(&plan, 0), None);
                        Box::new(InterpStage::new(Box::new(up), &plan, 0, (src.len() * l) as u64))
                    } else {
                        Box::new(PolyStream::with_input_shared(bank.clone(), Input::File(src.clone()), 0, None))
                    };
                    let build = ms(t);
                    let r = reads(st.as_mut(), (0.3 * out_rate as f64) as usize);
                    // A later block: the reads over the next 2 blocks' worth.
                    let blk = super::convolver::BLOCK * l;
                    let later = reads(st.as_mut(), 2 * blk);
                    if round == 0 {
                        continue;
                    }
                    t_build.push(build);
                    t_first.push(r.iter().sum());
                    // Per block: the slowest reads are the block computations.
                    t_later.push(later.iter().sum::<f64>() / 2.0);
                }
                println!(
                    "[k1c] x{:<2} {:<13} {:<6} depth {:>3} | build (prime) {} | first 0.3 s of output {} | one block later {}",
                    fs,
                    phase,
                    if multi { "Multi" } else { "Single" },
                    bank.max_partitions(),
                    stat(&t_build),
                    stat(&t_first),
                    stat(&t_later)
                );
            }
        }
    }
}

/// A track already at the output rate, handed out as it is (the stages
/// after the upsampler read it).
struct Pass {
    l: Arc<Vec<f64>>,
    r: Arc<Vec<f64>>,
    pos: u64,
}

impl Stage for Pass {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let mut inside = 0;
        for i in 0..out_l.len() {
            let k = self.pos as usize + i;
            if k < self.l.len() {
                out_l[i] = self.l[k];
                out_r[i] = self.r[k];
                inside += 1;
            } else {
                out_l[i] = 0.0;
                out_r[i] = 0.0;
            }
        }
        self.pos += out_l.len() as u64;
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.l.len() as u64
    }
}

/// The output-rate stages alone, from a track's start, on a source that
/// costs nothing: XTC, the output limiter, the subsonic guard, the alpha-HP
/// blend on a plan made as it plays, and all of them as `build_chain` puts
/// them together — the first 0.3 s of output against the same later.
#[test]
#[ignore]
fn first_block_stages_cost() {
    use super::blend::{AlphaStage, PlanFeed};
    use super::stages::{FirStage, GainStage, LimiterStage, XtcStage};
    let mask = affinity();
    let rounds: usize = std::env::var("AURA_K1_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let dir = tempfile::tempdir().unwrap();
    let res = Resources::new(dir.path().join("cache"));
    println!("[k1s] output-rate stages alone | mask {} | rayon {}", mask, rayon::current_num_threads());
    let src_rate = 44_100u32;
    let secs = 8usize;
    let src = Arc::new(super::convolver::SourceBuf {
        l: music(secs * src_rate as usize, src_rate, 3),
        r: music(secs * src_rate as usize, src_rate, 4),
        rate: src_rate,
    });
    for fs in [4u32, 16] {
        let rate = src_rate * fs;
        let n = secs * rate as usize;
        let l = Arc::new(music(n, rate, 5));
        let r = Arc::new(music(n, rate, 6));
        let s = PlayerSettings { fs_multiplier: fs, xtc: true, xtc_geometry: room(), subsonic_hz: 10, ..PlayerSettings::default() };
        assert!(s.xtc_active(), "XTC on");
        let xtc = res.xtc_pair(&s, rate).expect("xtc");
        let guard = res.guard(rate, 10);
        let pass = || -> Box<dyn Stage> { Box::new(Pass { l: l.clone(), r: r.clone(), pos: 0 }) };
        let pre = (0.3 * rate as f64) as usize;
        for case in ["pass", "xtc", "limiter", "guard", "alpha", "all"] {
            let (mut first, mut later) = (vec![], vec![]);
            for round in 0..=rounds {
                let mut st: Box<dyn Stage> = match case {
                    "pass" => pass(),
                    "xtc" => Box::new(XtcStage::new(pass(), &xtc.0, &xtc.1, 0)),
                    "limiter" => Box::new(LimiterStage::new(pass(), 10f64.powf(-0.5 / 20.0), rate, 0)),
                    "guard" => Box::new(FirStage::new(pass(), &guard, rate, 0)),
                    "alpha" => Box::new(AlphaStage::live(pass(), pass(), PlanFeed::new(Input::File(src.clone()), src_rate, rate as f64, 0))),
                    _ => {
                        let a = Box::new(AlphaStage::live(pass(), pass(), PlanFeed::new(Input::File(src.clone()), src_rate, rate as f64, 0)));
                        let x = Box::new(XtcStage::new(a, &xtc.0, &xtc.1, 0));
                        let g = Box::new(GainStage::new(x, 0.9));
                        let lim = Box::new(LimiterStage::new(g, 10f64.powf(-0.5 / 20.0), rate, 0));
                        Box::new(FirStage::new(lim, &guard, rate, 0))
                    }
                };
                let f: f64 = reads(st.as_mut(), pre).iter().sum();
                let more = reads(st.as_mut(), 2 * rate as usize);
                if round == 0 {
                    continue;
                }
                first.push(f);
                later.push(more.iter().sum::<f64>() * pre as f64 / (more.len() * STEP) as f64);
            }
            println!("[k1s] x{:<2} {:<8} first 0.3 s {} | the same later {} (guard {} taps)", fs, case, stat(&first), stat(&later), guard.len());
        }
    }
}

/// The source stages a start's first block waits for (its filter's
/// look-ahead: 21 s of source at ×16, 85 s at ×4, 30M), on the decoded
/// track: each alone and piped as a start runs them (`run_file`), with the
/// time the piped stream reached 21 s and 85 s.
#[test]
#[ignore]
fn first_block_source_stages_cost() {
    use super::source_stages::{stream_pool, SourcePlan, SourceStages, SourceTally};
    use std::sync::Mutex;
    let mask = affinity();
    let rounds: usize = std::env::var("AURA_K1_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let track = PathBuf::from(std::env::var("AURA_K1_TRACK").expect("AURA_K1_TRACK"));
    let s = PlayerSettings { subsonic_hz: 10, use_gpu: false, ..PlayerSettings::default() };
    let cancel = AtomicBool::new(false);
    let v = prepare_variant(&track, &s, true, &cancel).expect("variant");
    let src = v.src.clone();
    let rate = src.rate;
    let pool = stream_pool().expect("the stream pool");
    let threads = pool.current_num_threads();
    println!("[k1src] source stages | mask {} | stream pool {} | {:.0} s of source", mask, threads, src.len() as f64 / rate as f64);
    let marks = [21usize * rate as usize, 85 * rate as usize];
    for (name, isp, sub) in [("isp", true, 0u32), ("sub10", false, 10), ("isp+sub10", true, 10)] {
        let s = PlayerSettings { isp, subsonic_hz: sub, apodizing: 0, adaptive_apodizer: false, ..s.clone() };
        let plan = SourcePlan::new(&s, rate);
        let mut at = vec![vec![]; marks.len()];
        let mut whole = vec![];
        for round in 0..=rounds {
            let tally = Arc::new(Mutex::new(SourceTally::default()));
            let st = SourceStages::after_repairs(&plan, &[], &[], &[], &[], tally, threads);
            let t0 = Instant::now();
            let mut got = 0usize;
            let mut hit = vec![0.0; marks.len()];
            st.run_file(&src.l, &src.r, Some(pool), &|| true, &mut |a, _| {
                got += a.len();
                for (k, m) in marks.iter().enumerate() {
                    if hit[k] == 0.0 && got >= *m {
                        hit[k] = ms(t0);
                    }
                }
            });
            if round == 0 {
                continue;
            }
            for k in 0..marks.len() {
                at[k].push(hit[k]);
            }
            whole.push(ms(t0));
        }
        println!("[k1src] {:<10} piped: 21 s in {} | 85 s in {} | the whole track {}", name, stat(&at[0]), stat(&at[1]), stat(&whole));
    }
}

/// The prefill gate (`put_on_air`: 0.3 s of output before the device
/// starts) shorter: how far ahead of its device a start's first 2.5 s stay.
/// For each gate a start of its own: the chain renders as the render thread
/// does (on and on, at most `AHEAD` s ahead of the device), the device plays
/// in real time from the moment the gate's frames are there, and the full
/// variant with its output-peak probe — the work behind the music — begins
/// 0.3 s after that, as `foreground` lets it. The least lead per gate (ms;
/// below zero: frames the device would have gone without).
///
///     AURA_FILTER_DIR=<blobs> cargo test --profile fast --bins prefill_gate_lead -- --ignored --nocapture
///
/// `AURA_K4_GATES` the gates in s (0.3,0.15,0.12,0.1); `AURA_K1_ROUNDS` (3),
/// `AURA_K1_CASES`, `AURA_K1_TRACK`, `AURA_BENCH_AFFINITY` as above.
#[test]
#[ignore]
fn prefill_gate_lead() {
    use super::chain::{ceiling_lin, output_peak, plays_pair, prepare_full_variant};
    use std::time::Duration;
    const AHEAD: f64 = 1.5;
    const WATCH: f64 = 2.5;
    let mask = affinity();
    let rounds: usize = std::env::var("AURA_K1_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let only = std::env::var("AURA_K1_CASES").unwrap_or_default();
    let gates: Vec<f64> = std::env::var("AURA_K4_GATES")
        .ok()
        .map(|s| s.split(',').filter_map(|g| g.trim().parse().ok()).collect())
        .filter(|g: &Vec<f64>| !g.is_empty())
        .unwrap_or_else(|| vec![0.3, 0.15, 0.12, 0.1]);
    let dir = tempfile::tempdir().unwrap();
    let track: PathBuf = match std::env::var("AURA_K1_TRACK") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            let rate = 44_100u32;
            let n = (240 * rate) as usize;
            let p = dir.path().join("music.wav");
            write_wav16(&p, rate, &music(n, rate, 3), &music(n, rate, 4));
            p
        }
    };
    println!("[k4] prefill gate lead | mask {} | rayon {} | gates {:?}", mask, rayon::current_num_threads(), gates);
    for fs in [4u32, 16] {
        for phase in [Phase::Linear, Phase::Alpha] {
            let name = format!("x{} {:?}", fs, phase);
            if !only.is_empty() && !only.split(',').any(|w| name.contains(w.trim())) {
                continue;
            }
            let s = PlayerSettings {
                fs_multiplier: fs,
                taps: 30_000_000,
                phase,
                subsonic_hz: 10,
                xtc: true,
                xtc_geometry: room(),
                use_gpu: false,
                upsampler: Some(Upsampler::Multi),
                ..PlayerSettings::default()
            };
            let res = Arc::new(Resources::new(dir.path().join(format!("cache-k4-{}-{:?}", fs, phase))));
            let tk = format!("k4:{}", track.display());
            let mut least: Vec<Vec<f64>> = vec![vec![]; gates.len()];
            let mut gate_ms: Vec<Vec<f64>> = vec![vec![]; gates.len()];
            for round in 0..=rounds {
                for (gi, &gate) in gates.iter().enumerate() {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let v = Arc::new(prepare_variant(&track, &s, true, &cancel).expect("the first variant"));
                    let rate = v.out_rate as f64;
                    let mut chain = build_chain(&res, 1, &tk, &v, &s, 0, None, None, 0).expect("chain");
                    let (mut bl, mut br) = (vec![0.0; STEP], vec![0.0; STEP]);
                    let t0 = Instant::now();
                    let gate_frames = (gate * rate) as usize;
                    let (mut made, mut device_at, mut low, mut behind) = (0usize, None::<Instant>, f64::MAX, None);
                    while made < (WATCH * rate) as usize {
                        // At most AHEAD s ahead of a device that plays.
                        if let Some(d) = device_at {
                            let played = d.elapsed().as_secs_f64() * rate;
                            let over = made as f64 - played - AHEAD * rate;
                            if over > 0.0 {
                                std::thread::sleep(Duration::from_secs_f64(over / rate));
                            }
                        }
                        let before = made;
                        chain.stage.read(&mut bl, &mut br);
                        made += STEP;
                        if let Some(d) = device_at {
                            // Just before this read landed, the device had `before`.
                            let played = d.elapsed().as_secs_f64() * rate;
                            low = low.min((before as f64 - played) / rate * 1000.0);
                        } else if made >= gate_frames {
                            device_at = Some(Instant::now());
                            gate_ms[gi].push(ms(t0));
                            // The work behind the music, 0.3 s after the first sound.
                            let (track, s, res, tk, v, cancel) = (track.clone(), s.clone(), res.clone(), tk.clone(), v.clone(), cancel.clone());
                            behind = Some(std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_millis(300));
                                if let Ok(full) = prepare_full_variant(&track, &s, &cancel, v.stream.clone(), true, None) {
                                    let hp_on = plays_pair(&res, &tk, &full, &s);
                                    let _ = output_peak(&res, &tk, &full, &s, hp_on, ceiling_lin(&full, &s));
                                }
                            }));
                        }
                    }
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                    drop(chain);
                    if let Some(h) = behind {
                        let _ = h.join();
                    }
                    if round > 0 {
                        least[gi].push(low);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            for (gi, gate) in gates.iter().enumerate() {
                println!(
                    "[k4] {:<10} gate {:.2} s: first sound after {} ms | least lead in {} s {} ms",
                    name,
                    gate,
                    stat(&gate_ms[gi]),
                    WATCH,
                    stat(&least[gi])
                );
            }
        }
    }
}
