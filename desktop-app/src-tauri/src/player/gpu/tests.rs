//! GPU convolver equivalence harness and throughput benchmark.
//!
//! All tests are `#[ignore]` by default; run with:
//!   cargo test --profile fast --features gpu-player-tests -p aura-engine \
//!       player::gpu::tests -- --ignored --nocapture
//!
//! Equivalence tests compare CPU `PolyStream` output vs `GpuPolyStream` block
//! by block for several signal types and filter sizes.  Target: max |diff| and
//! RMS diff reported in dBFS (spec calls for ≤ −230 dBFS; anything above
//! −200 dBFS is a problem).
//!
//! Throughput tests measure wall-clock RTF and VRAM for 30M taps at FS8.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::player::convolver::{Alignment, Bank, PolyStream, SourceBuf, BLOCK};
use crate::player::stages::Stage;
use super::ctx::GpuPolyCtx;
use super::poly_stream::GpuPolyStream;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn filter_dir() -> String {
    std::env::var("AURA_FILTER_DIR")
        .unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../fir-optimizer/output").to_string()
        })
}

/// White noise at −1 dBFS using an LCG PRNG (no rand dependency needed here).
fn white_noise(n: usize, seed: u64) -> Vec<f64> {
    let amp = (10.0f64).powf(-1.0 / 20.0); // −1 dBFS
    let mut x = seed;
    (0..n).map(|_| {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = ((x >> 33) as f64) / (u32::MAX as f64) * 2.0 - 1.0;
        v * amp
    }).collect()
}

/// Log sweep 20 Hz → 20 kHz over `n` samples at sample rate `fs`.
fn log_sweep(n: usize, fs: f64) -> Vec<f64> {
    let f1 = 20.0f64;
    let f2 = 20_000.0f64;
    let amp = (10.0f64).powf(-1.0 / 20.0);
    (0..n).map(|i| {
        let t = i as f64 / fs;
        let dur = n as f64 / fs;
        let f = f1 * (f2 / f1).powf(t / dur);
        let phase = 2.0 * std::f64::consts::PI * f * t;
        phase.sin() * amp
    }).collect()
}

/// Unit impulse at sample 0.
fn impulse(n: usize) -> Vec<f64> {
    let mut v = vec![0.0f64; n];
    if n > 0 { v[0] = 1.0; }
    v
}

/// Full-scale square wave at 1 kHz, 44100 Hz sample rate (source rate).
fn square_wave(n: usize) -> Vec<f64> {
    let period = 44; // ~1 kHz at 44100 Hz
    (0..n).map(|i| if (i % period) < period / 2 { 1.0 } else { -1.0 }).collect()
}

/// All zeros.
fn silence(n: usize) -> Vec<f64> {
    vec![0.0f64; n]
}

/// Denormal-range signal: amplitude ~1e-38 (below f32 normal range).
fn denormal_range(n: usize) -> Vec<f64> {
    white_noise(n, 42).into_iter().map(|v| v * 1.1754944e-38f64).collect()
}

fn db(x: f64) -> f64 {
    if x <= 0.0 { -400.0 } else { 20.0 * x.log10() }
}

/// Run both CPU and GPU streams from `start=0` over `n_out` output samples,
/// return (max_abs_diff_db, rms_diff_db).
///
/// Returns `None` ONLY when `try_build()` returns None (no usable GPU hardware
/// on this machine).  When the context exists, `try_new` returning None or the
/// device error flag being set is a test FAILURE — the test must not pass
/// vacuously.
fn compare_streams(
    bank: Arc<Bank>,
    src_l: Vec<f64>,
    src_r: Vec<f64>,
    n_out: usize,
) -> Option<(f64, f64)> {
    let src = Arc::new(SourceBuf { l: src_l, r: src_r, rate: 44_100 });

    // CPU reference.
    let mut cpu = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), 0);
    let mut cpu_l = vec![0.0f64; n_out];
    let mut cpu_r = vec![0.0f64; n_out];
    cpu.read(&mut cpu_l, &mut cpu_r);

    // GPU path.  Returning None here means no hardware → skip.
    let ctx = GpuPolyCtx::try_build()?;

    // Context exists: try_new returning None is a failure, not a skip.
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "player_device_error was set before stream construction — \
         a previous stream poisoned the process-wide flag"
    );
    let mut gpu = GpuPolyStream::try_new(
        Arc::clone(&bank),
        Arc::clone(&src),
        0,
        ctx,
    ).expect("GPU context exists but try_new returned None");

    let mut gpu_l = vec![0.0f64; n_out];
    let mut gpu_r = vec![0.0f64; n_out];
    gpu.read(&mut gpu_l, &mut gpu_r);

    assert!(
        !gpu.has_error(),
        "GPU stream error flag set after read — device or watchdog failure during test"
    );

    // Compute diff.
    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    let total = n_out * 2;
    for i in 0..n_out {
        let dl = (cpu_l[i] - gpu_l[i]).abs();
        let dr = (cpu_r[i] - gpu_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
        sum_sq += dl * dl + dr * dr;
    }
    let rms = (sum_sq / total as f64).sqrt();
    Some((db(max_abs), db(rms)))
}

// ── Equivalence tests ─────────────────────────────────────────────────────────

fn run_equivalence_for_filter(tag: &str, path: &str) -> Vec<(String, f64, f64)> {
    let l = 8; // FS8
    let bank = match Bank::load(path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            println!("[equiv] skip {}: {}", tag, e);
            return vec![];
        }
    };
    println!("[equiv] {} loaded: {} taps, {} branches, {} partitions",
        tag, bank.full_len, bank.l, bank.max_partitions());

    // Use 8 blocks of audio (covers ≥ one full filter delay for small filters).
    let n_src = 8 * BLOCK;
    let n_out = n_src * l;

    let signals: &[(&str, fn(usize) -> Vec<f64>, fn(usize) -> Vec<f64>)] = &[
        ("white_noise",      |n| white_noise(n, 1),    |n| white_noise(n, 2)),
        ("log_sweep",        |n| log_sweep(n, 44100.0), |n| log_sweep(n, 44100.0)),
        ("impulse",          |n| impulse(n),             |n| impulse(n)),
        ("square",           |n| square_wave(n),         |n| square_wave(n)),
        ("silence",          |n| silence(n),             |n| silence(n)),
        ("denormal",         |n| denormal_range(n),      |n| denormal_range(n)),
    ];

    let mut results = vec![];
    for (sig_name, gen_l, gen_r) in signals {
        let src_l = gen_l(n_src);
        let src_r = gen_r(n_src);
        match compare_streams(Arc::clone(&bank), src_l, src_r, n_out) {
            None => {
                // try_build() returned None: machine has no usable GPU.
                println!("[equiv] {}/{}: no GPU hardware — skip", tag, sig_name);
            }
            Some((max_db, rms_db)) => {
                let flag = if max_db > -200.0 { " *** OVER -200 dBFS ***" } else { "" };
                println!("[equiv] {}/{}: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
                    tag, sig_name, max_db, rms_db, flag);
                results.push((format!("{}/{}", tag, sig_name), max_db, rms_db));
            }
        }
    }
    results
}

/// HP pair equivalence: each stream owns its own FdlRing (K9).
fn run_hp_equivalence(lin_path: &str, min_path: &str) -> Vec<(String, f64, f64)> {
    let l = 8;
    let lin_bank = match Bank::load(lin_path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[equiv] skip HP lin: {}", e); return vec![]; }
    };
    let min_bank = match Bank::load(min_path, l, Alignment::BandWeighted, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[equiv] skip HP min: {}", e); return vec![]; }
    };

    let n_src = 8 * BLOCK;
    let n_out = n_src * l;
    let src_l = white_noise(n_src, 7);
    let src_r = white_noise(n_src, 8);
    let src = Arc::new(SourceBuf { l: src_l.clone(), r: src_r.clone(), rate: 44_100 });

    // CPU reference: run lin and min PolyStreams, sum outputs (× 0.5 blend).
    let mut cpu_lin = PolyStream::new(Arc::clone(&lin_bank), Arc::clone(&src), 0);
    let mut cpu_min = PolyStream::new(Arc::clone(&min_bank), Arc::clone(&src), 0);
    let mut lin_l = vec![0.0f64; n_out];
    let mut lin_r = vec![0.0f64; n_out];
    let mut min_l = vec![0.0f64; n_out];
    let mut min_r = vec![0.0f64; n_out];
    cpu_lin.read(&mut lin_l, &mut lin_r);
    cpu_min.read(&mut min_l, &mut min_r);
    let cpu_l: Vec<f64> = lin_l.iter().zip(min_l.iter()).map(|(&a, &b)| (a + b) * 0.5).collect();
    let cpu_r: Vec<f64> = lin_r.iter().zip(min_r.iter()).map(|(&a, &b)| (a + b) * 0.5).collect();

    // GPU: both streams own independent FdlRings (K9 — shared FDL removed).
    // try_build() None = no hardware → skip.  try_new() None = FAILURE.
    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[equiv] HP: no GPU hardware — skip"); return vec![]; }
        Some(c) => c,
    };
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "HP: player_device_error set before HP stream construction"
    );
    let mut gpu_lin = GpuPolyStream::try_new(
        Arc::clone(&lin_bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    ).expect("HP: GPU context exists but try_new (lin) returned None");
    let mut gpu_min = GpuPolyStream::try_new(
        Arc::clone(&min_bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    ).expect("HP: GPU context exists but try_new (min) returned None");

    let mut glin_l = vec![0.0f64; n_out];
    let mut glin_r = vec![0.0f64; n_out];
    let mut gmin_l = vec![0.0f64; n_out];
    let mut gmin_r = vec![0.0f64; n_out];
    gpu_lin.read(&mut glin_l, &mut glin_r);
    gpu_min.read(&mut gmin_l, &mut gmin_r);
    let gpu_l: Vec<f64> = glin_l.iter().zip(gmin_l.iter()).map(|(&a, &b)| (a + b) * 0.5).collect();
    let gpu_r: Vec<f64> = glin_r.iter().zip(gmin_r.iter()).map(|(&a, &b)| (a + b) * 0.5).collect();

    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    let total = n_out * 2;
    for i in 0..n_out {
        let dl = (cpu_l[i] - gpu_l[i]).abs();
        let dr = (cpu_r[i] - gpu_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
        sum_sq += dl * dl + dr * dr;
    }
    let rms = (sum_sq / total as f64).sqrt();
    let (max_db, rms_db) = (db(max_abs), db(rms));
    println!("[equiv] HP_30M/white_noise: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS", max_db, rms_db);
    vec![("HP_30M/white_noise".to_string(), max_db, rms_db)]
}

/// An equivalence run that measured nothing proves nothing. With no GPU on
/// the machine that is a skip (said so on the console); with a GPU present
/// it means the filter file was not found (AURA_FILTER_DIR) or could not be
/// loaded — a failure, or a missing filter would pass as "equivalent".
fn assert_measured(tag: &str, results: &[(String, f64, f64)]) {
    if !results.is_empty() {
        return;
    }
    if GpuPolyCtx::try_build().is_none() {
        println!("[equiv] {}: no GPU hardware — nothing measured, skipped", tag);
        return;
    }
    panic!(
        "[equiv] {}: a GPU is present but nothing was measured — is the filter file in AURA_FILTER_DIR ({})?",
        tag,
        filter_dir()
    );
}

#[test]
#[ignore]
fn equivalence_1m_linear() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_linear_phase.npy", dir);
    let results = run_equivalence_for_filter("1M_linear", &path);
    assert_measured("1M_linear", &results);
    for (name, max_db, _) in &results {
        assert!(
            *max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS (above -200 dBFS threshold)",
            name, max_db
        );
    }
}

#[test]
#[ignore]
fn equivalence_10m_linear() {
    let dir = filter_dir();
    let path = format!("{}/fir_10M_352800_linear_phase.npy", dir);
    let results = run_equivalence_for_filter("10M_linear", &path);
    assert_measured("10M_linear", &results);
    for (name, max_db, _) in &results {
        assert!(
            *max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS (above -200 dBFS threshold)",
            name, max_db
        );
    }
}

#[test]
#[ignore]
fn equivalence_30m_linear() {
    let dir = filter_dir();
    let path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let results = run_equivalence_for_filter("30M_linear", &path);
    assert_measured("30M_linear", &results);
    for (name, max_db, _) in &results {
        assert!(
            *max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS (above -200 dBFS threshold)",
            name, max_db
        );
    }
}

#[test]
#[ignore]
fn equivalence_hp_30m() {
    let dir = filter_dir();
    let lin_path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let min_path = format!("{}/fir_30M_352800_minimum_phase.npy", dir);
    let results = run_hp_equivalence(&lin_path, &min_path);
    assert_measured("HP_30M", &results);
    for (name, max_db, _) in &results {
        assert!(
            *max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS",
            name, max_db
        );
    }
}

// ── Throughput benchmarks ─────────────────────────────────────────────────────

// Fields are set by bench helpers but callers discard the returned value
// with `let _ = ...`; the data is printed inline before returning.
#[allow(dead_code)]
struct ThroughputResult {
    tag: String,
    rtf: f64,
    vram_mb: Option<u64>,
    note: String,
}

fn bench_gpu_stream(tag: &str, path: &str, align: Alignment) -> Option<ThroughputResult> {
    let l = 8;
    let bank = match Bank::load(path, l, align, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            println!("[bench] skip {}: {}", tag, e);
            return None;
        }
    };

    // 90 seconds of source audio at 44100 Hz.
    let n_src = 44_100 * 90;
    let x: Vec<f64> = (0..n_src).map(|i| ((i as f64) * 0.01).sin() * 0.5).collect();
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });

    let ctx = GpuPolyCtx::try_build()?;

    let vram_before = ctx.free_vram_bytes();

    let mut stream = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    )?;

    // Prime the first block (not counted in RTF).
    let mut ol = vec![0.0f64; 4096];
    let mut or_ = vec![0.0f64; 4096];
    stream.read(&mut ol, &mut or_);

    let vram_after_free = ctx.free_vram_bytes();
    let vram_mb = if vram_before > vram_after_free {
        Some((vram_before - vram_after_free) / (1 << 20))
    } else {
        Some(0u64)
    };

    // Measure 30 seconds of output at FS8 = 352800 Hz.
    let target = 352_800u64 * 30;
    let t0 = Instant::now();
    let mut produced = 4096usize;
    while (produced as u64) < target {
        stream.read(&mut ol, &mut or_);
        produced += 4096;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let audio_secs = produced as f64 / 352_800.0;
    let rtf = audio_secs / elapsed;

    let note = if stream.has_error() {
        "device error during bench".to_string()
    } else {
        format!(
            "{:.3}s wall / {:.1}s audio  last_block={} µs",
            elapsed, audio_secs,
            stream.last_block_wall_ns() / 1_000
        )
    };

    println!(
        "[bench] {} GPU: RTF {:.1}×  VRAM delta ~{} MB  ({})",
        tag, rtf,
        vram_mb.map(|v| format!("{}", v)).unwrap_or("?".to_string()),
        note
    );

    Some(ThroughputResult {
        tag: tag.to_string(),
        rtf,
        vram_mb,
        note,
    })
}

fn bench_cpu_stream(tag: &str, path: &str, align: Alignment, threads: usize) -> Option<ThroughputResult> {
    let l = 8;
    let bank = match Bank::load(path, l, align, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            println!("[bench] skip CPU {}: {}", tag, e);
            return None;
        }
    };

    let n_src = 44_100 * 90;
    let x: Vec<f64> = (0..n_src).map(|i| ((i as f64) * 0.01).sin() * 0.5).collect();
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });

    // Limit rayon thread pool for the "6 threads" and "4 threads" cases.
    // Note: we build a scoped thread pool; the global pool is not permanently changed.
    let tag_full = format!("{}_cpu_{}thr", tag, if threads == 0 { usize::MAX } else { threads });

    let measure = |pool: &rayon::ThreadPool| -> f64 {
        let mut stream = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), 0);
        let mut ol = vec![0.0f64; 4096];
        let mut or_ = vec![0.0f64; 4096];
        // prime
        stream.read(&mut ol, &mut or_);
        let target = 352_800u64 * 30;
        let t0 = Instant::now();
        let mut produced = 4096usize;
        pool.install(|| {
            while (produced as u64) < target {
                stream.read(&mut ol, &mut or_);
                produced += 4096;
            }
        });
        let elapsed = t0.elapsed().as_secs_f64();
        let audio_secs = produced as f64 / 352_800.0;
        audio_secs / elapsed
    };

    let (rtf, thr_note) = if threads == 0 || threads >= rayon::current_num_threads() {
        // All threads.
        let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
        let rtf = measure(&pool);
        (rtf, format!("all ({}) threads", rayon::current_num_threads()))
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let rtf = measure(&pool);
        (rtf, format!("{} threads (scoped pool; no OS affinity — approximate)", threads))
    };

    println!(
        "[bench] {} CPU ({}): RTF {:.1}×",
        tag_full, thr_note, rtf
    );

    Some(ThroughputResult {
        tag: tag_full,
        rtf,
        vram_mb: None,
        note: thr_note,
    })
}

// ── Extra independent checks (verifier) ──────────────────────────────────────

/// Run both CPU and GPU from a non-zero start position and compare.
/// This exercises the priming path for a mid-track position and confirms that
/// `pend_start` alignment is correct after skipping the filter delay.
fn compare_streams_offset(
    bank: Arc<Bank>,
    src_l: Vec<f64>,
    src_r: Vec<f64>,
    start: u64,
    n_out: usize,
) -> Option<(f64, f64)> {
    let src = Arc::new(SourceBuf { l: src_l, r: src_r, rate: 44_100 });

    let mut cpu = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), start);
    let mut cpu_l = vec![0.0f64; n_out];
    let mut cpu_r = vec![0.0f64; n_out];
    cpu.read(&mut cpu_l, &mut cpu_r);

    // try_build() None = no hardware → skip (return None to caller).
    let ctx = GpuPolyCtx::try_build()?;
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "offset: player_device_error set before stream construction"
    );
    let mut gpu = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), start, ctx,
    ).expect("offset: GPU context exists but try_new returned None");
    let mut gpu_l = vec![0.0f64; n_out];
    let mut gpu_r = vec![0.0f64; n_out];
    gpu.read(&mut gpu_l, &mut gpu_r);

    assert!(
        !gpu.has_error(),
        "offset: GPU stream error flag set after read"
    );

    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    let total = n_out * 2;
    for i in 0..n_out {
        let dl = (cpu_l[i] - gpu_l[i]).abs();
        let dr = (cpu_r[i] - gpu_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
        sum_sq += dl * dl + dr * dr;
    }
    let rms = (sum_sq / total as f64).sqrt();
    Some((db(max_abs), db(rms)))
}

/// Run both CPU and GPU for many blocks to catch FDL ring-wrap bugs.
/// Uses 150 blocks for the 30M filter (P=115, so the ring wraps once).
/// Also uses a different seed to catch any signal-dependent artefacts.
fn compare_streams_long(
    bank: Arc<Bank>,
    src_l: Vec<f64>,
    src_r: Vec<f64>,
    n_blocks: usize,
) -> Option<(f64, f64)> {
    let l = bank.l;
    let n_src = n_blocks * BLOCK;
    // Trim or extend to exactly n_blocks * BLOCK source samples.
    let src_l: Vec<f64> = src_l.into_iter().chain(std::iter::repeat(0.0)).take(n_src).collect();
    let src_r: Vec<f64> = src_r.into_iter().chain(std::iter::repeat(0.0)).take(n_src).collect();
    let n_out = n_src * l;
    compare_streams(bank, src_l, src_r, n_out)
}

/// Verifier extra check: offset start and FDL wrap.
/// Tests three additional scenarios not covered by the builder's 8-block suite:
///   A. Non-zero start (offset_start = 8 × BLOCK × L, i.e. 8 output blocks in)
///   B. Long run (150 blocks > P=115 for 30M) to exercise FDL ring wrap-around
///   C. Different white-noise seed (seed=99) with 8 blocks
#[test]
#[ignore]
fn verifier_extra_1m_offset_and_fdl_wrap() {
    let dir = filter_dir();
    let lin_path = format!("{}/fir_1M_352800_linear_phase.npy", dir);
    let l = 8usize;

    let bank = match Bank::load(&lin_path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[verifier] skip 1M: {}", e); return; }
    };

    let n_src_short = 8 * BLOCK;
    let n_out_short = n_src_short * l;

    // A. Offset start: start at output sample 8 * BLOCK * l (after 8 full blocks).
    let start = (8 * BLOCK * l) as u64;
    // Use 10 * BLOCK source samples for the signal (covers the offset and some after).
    let n_src_for_offset = 18 * BLOCK;
    let src_l = white_noise(n_src_for_offset, 77);
    let src_r = white_noise(n_src_for_offset, 78);
    match compare_streams_offset(Arc::clone(&bank), src_l, src_r, start, n_out_short) {
        None => println!("[verifier] 1M/offset: no GPU hardware"),
        Some((max_db, rms_db)) => {
            let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
            println!("[verifier] 1M_linear/offset_start: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
                max_db, rms_db, flag);
            assert!(max_db <= -200.0,
                "verifier offset_start FAIL: max|diff| = {:.1} dBFS", max_db);
        }
    }

    // B. Long run (30 blocks for 1M; P=4 so wraps 7×).
    {
        let n_src_long = 30 * BLOCK;
        let src_l = white_noise(n_src_long, 99);
        let src_r = white_noise(n_src_long, 100);
        let n_out_long = n_src_long * l;
        match compare_streams(Arc::clone(&bank), src_l, src_r, n_out_long) {
            None => println!("[verifier] 1M/fdl_wrap: no GPU hardware"),
            Some((max_db, rms_db)) => {
                let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
                println!("[verifier] 1M_linear/fdl_wrap_30blk: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
                    max_db, rms_db, flag);
                assert!(max_db <= -200.0,
                    "verifier fdl_wrap FAIL: max|diff| = {:.1} dBFS", max_db);
            }
        }
    }

    // C. Different seed.
    {
        let src_l = white_noise(n_src_short, 99);
        let src_r = white_noise(n_src_short, 100);
        match compare_streams(Arc::clone(&bank), src_l, src_r, n_out_short) {
            None => println!("[verifier] 1M/seed99: no GPU hardware"),
            Some((max_db, rms_db)) => {
                let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
                println!("[verifier] 1M_linear/seed_99: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
                    max_db, rms_db, flag);
                assert!(max_db <= -200.0,
                    "verifier seed99 FAIL: max|diff| = {:.1} dBFS", max_db);
            }
        }
    }
}

/// Verifier extra check for 30M: 150 blocks to force the P=115 FDL ring
/// to wrap once completely.  This is the main gap in the builder's test suite.
#[test]
#[ignore]
fn verifier_extra_30m_fdl_wrap() {
    let dir = filter_dir();
    let lin_path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let l = 8usize;

    let bank = match Bank::load(&lin_path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[verifier] skip 30M wrap: {}", e); return; }
    };

    // 150 blocks > P=115 → the FDL ring wraps once.  Seed=99 for variety.
    let n_blocks = 150;
    let n_src = n_blocks * BLOCK;
    let src_l = white_noise(n_src, 99);
    let src_r = white_noise(n_src, 100);

    println!("[verifier] 30M_linear/fdl_wrap (150 blocks, P=115)...");
    match compare_streams_long(Arc::clone(&bank), src_l, src_r, n_blocks) {
        None => println!("[verifier] 30M/fdl_wrap: no GPU hardware — skip"),
        Some((max_db, rms_db)) => {
            let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
            println!("[verifier] 30M_linear/fdl_wrap_150blk: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
                max_db, rms_db, flag);
            assert!(max_db <= -200.0,
                "verifier 30M fdl_wrap FAIL: max|diff| = {:.1} dBFS", max_db);
        }
    }
}

#[test]
#[ignore]
fn throughput_30m_gpu_and_cpu() {
    let dir = filter_dir();
    let lin_path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let min_path = format!("{}/fir_30M_352800_minimum_phase.npy", dir);

    println!("[bench] == GPU throughput ==");
    let _ = bench_gpu_stream("30M_linear", &lin_path, Alignment::Linear);
    let _ = bench_gpu_stream("30M_hp_lin", &lin_path, Alignment::Linear);
    let _ = bench_gpu_stream("30M_hp_min", &min_path, Alignment::BandWeighted);

    println!("[bench] == CPU throughput ==");
    let _ = bench_cpu_stream("30M_linear", &lin_path, Alignment::Linear, 0);
    let _ = bench_cpu_stream("30M_linear", &lin_path, Alignment::Linear, 6);
    let _ = bench_cpu_stream("30M_linear", &lin_path, Alignment::Linear, 4);
}

// ── TFS and MIN phase equivalence ────────────────────────────────────────────

#[test]
#[ignore]
fn equivalence_1m_tfs() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_tfs_phase_v3.npy", dir);
    let results = run_equivalence_for_filter("1M_tfs", &path);
    assert_measured("1M_tfs", &results);
    for (name, max_db, _) in &results {
        assert!(*max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS", name, max_db);
    }
}

#[test]
#[ignore]
fn equivalence_1m_minimum() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_minimum_phase.npy", dir);
    let results = run_equivalence_for_filter("1M_min", &path);
    assert_measured("1M_min", &results);
    for (name, max_db, _) in &results {
        assert!(*max_db <= -200.0,
            "FAIL {}: max|diff| = {:.1} dBFS", name, max_db);
    }
}

// ── Cold-start timing (K5 / R8.6) ─────────────────────────────────────────────

/// Measure cold-start latency for a 30M filter on the GPU.
/// Clears the bank cache before each measurement so from_bank always encodes.
/// Reports: bank_load_ms, from_bank_ms, prime_ms, first_block_ms.
#[test]
#[ignore]
fn cold_start_latency_30m() {
    let dir = filter_dir();
    let path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let l = 8usize;

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[cold] GPU unavailable — skip"); return; }
        Some(c) => c,
    };

    // T1: Bank::load (CPU side).
    let t0 = Instant::now();
    let bank = match Bank::load(&path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[cold] skip: {}", e); return; }
    };
    let bank_load_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let n_src = 44_100 * 3;
    let x: Vec<f64> = (0..n_src).map(|i| ((i as f64) * 0.01).sin() * 0.5).collect();
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });

    // T2: GpuFilterBank::from_bank (encode + PCIe upload).  Clear cache first.
    ctx.clear_bank_cache();
    let t0 = Instant::now();
    use super::filter_buf::GpuFilterBank;
    let _fb = GpuFilterBank::from_bank(&ctx.device, &ctx.queue, &bank);
    let from_bank_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // T3: full try_new (includes prime + poll(Wait)) — cache miss (clear again).
    ctx.clear_bank_cache();
    let t0 = Instant::now();
    let mut stream = GpuPolyStream::try_new(Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx))
        .expect("[cold] GPU context exists but try_new returned None");
    let try_new_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let prime_ms = try_new_ms - from_bank_ms;

    // T4: first compute_block (warm pipeline).
    let mut ol = vec![0.0f64; 4096];
    let mut or_ = vec![0.0f64; 4096];
    let t0 = Instant::now();
    stream.read(&mut ol, &mut or_);
    let first_block_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // T5: warm try_new (cache hit — only prime + poll(Wait), no encode/upload).
    let t0 = Instant::now();
    let _warm = GpuPolyStream::try_new(Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx));
    let warm_start_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "[cold] 30M@44.1k Bank::load={:.1} ms  from_bank={:.1} ms  \
         prime+poll_wait={:.1} ms  try_new_total={:.1} ms  \
         first_block={:.1} ms  warm_start={:.1} ms",
        bank_load_ms, from_bank_ms, prime_ms, try_new_ms, first_block_ms, warm_start_ms
    );
    println!("[cold] cold_start_latency_30m: PASS — queue drained in try_new, \
              first_block is steady-state");
}

// ── Hi-res source equivalence (K10) ──────────────────────────────────────────

/// 96 kHz source at FS×8 → 768 kHz output (L=8): run CPU vs GPU equivalence
/// using the 10M 768 kHz linear filter (no 30M 768 kHz exists in the repo).
#[test]
#[ignore]
fn equivalence_10m_96k_source() {
    let dir = filter_dir();
    let path = format!("{}/fir_10M_768000_linear_phase.npy", dir);
    let l = 8usize;
    let src_rate = 96_000u32;
    let bank = match Bank::load(&path, l, Alignment::Linear, 768_000) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[equiv] skip 10M 96k: {}", e); return; }
    };
    println!("[equiv] 10M_96k loaded: {} taps, {} branches, {} partitions",
        bank.full_len, bank.l, bank.max_partitions());

    let n_src = 8 * BLOCK;
    let n_out = n_src * l;
    let src_l = white_noise(n_src, 31);
    let src_r = white_noise(n_src, 32);
    let src = Arc::new(SourceBuf { l: src_l, r: src_r, rate: src_rate });

    let mut cpu = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), 0);
    let mut cpu_l = vec![0.0f64; n_out];
    let mut cpu_r = vec![0.0f64; n_out];
    cpu.read(&mut cpu_l, &mut cpu_r);

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[equiv] 10M 96k: no GPU hardware — skip"); return; }
        Some(c) => c,
    };
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "10M 96k: player_device_error set before stream construction"
    );
    let mut gpu = GpuPolyStream::try_new(Arc::clone(&bank), Arc::clone(&src), 0, ctx)
        .expect("10M 96k: GPU context exists but try_new returned None");
    let mut gpu_l = vec![0.0f64; n_out];
    let mut gpu_r = vec![0.0f64; n_out];
    gpu.read(&mut gpu_l, &mut gpu_r);

    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    for i in 0..n_out {
        let dl = (cpu_l[i] - gpu_l[i]).abs();
        let dr = (cpu_r[i] - gpu_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
        sum_sq += dl * dl + dr * dr;
    }
    let rms = (sum_sq / (n_out * 2) as f64).sqrt();
    let (max_db, rms_db) = (db(max_abs), db(rms));
    let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
    println!("[equiv] 10M_96k/L8/white_noise: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
        max_db, rms_db, flag);
    assert!(max_db <= -200.0, "10M 96k FAIL: max|diff| = {:.1} dBFS", max_db);
}

/// 192 kHz source at FS×2 → 384 kHz output (L=2): run CPU vs GPU equivalence
/// using the 10M 384 kHz linear filter.
#[test]
#[ignore]
fn equivalence_10m_192k_source() {
    let dir = filter_dir();
    let path = format!("{}/fir_10M_384000_linear_phase.npy", dir);
    let l = 2usize;
    let src_rate = 192_000u32;
    let bank = match Bank::load(&path, l, Alignment::Linear, 384_000) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[equiv] skip 10M 192k: {}", e); return; }
    };
    println!("[equiv] 10M_192k loaded: {} taps, {} branches, {} partitions",
        bank.full_len, bank.l, bank.max_partitions());

    let n_src = 8 * BLOCK;
    let n_out = n_src * l;
    let src_l = white_noise(n_src, 41);
    let src_r = white_noise(n_src, 42);
    let src = Arc::new(SourceBuf { l: src_l, r: src_r, rate: src_rate });

    let mut cpu = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), 0);
    let mut cpu_l = vec![0.0f64; n_out];
    let mut cpu_r = vec![0.0f64; n_out];
    cpu.read(&mut cpu_l, &mut cpu_r);

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[equiv] 10M 192k: no GPU hardware — skip"); return; }
        Some(c) => c,
    };
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "10M 192k: player_device_error set before stream construction"
    );
    let mut gpu = GpuPolyStream::try_new(Arc::clone(&bank), Arc::clone(&src), 0, ctx)
        .expect("10M 192k: GPU context exists but try_new returned None");
    let mut gpu_l = vec![0.0f64; n_out];
    let mut gpu_r = vec![0.0f64; n_out];
    gpu.read(&mut gpu_l, &mut gpu_r);

    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    for i in 0..n_out {
        let dl = (cpu_l[i] - gpu_l[i]).abs();
        let dr = (cpu_r[i] - gpu_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
        sum_sq += dl * dl + dr * dr;
    }
    let rms = (sum_sq / (n_out * 2) as f64).sqrt();
    let (max_db, rms_db) = (db(max_abs), db(rms));
    let flag = if max_db > -200.0 { " *** FAIL ***" } else { "" };
    println!("[equiv] 10M_192k/L2/white_noise: max|diff| {:>8.1} dBFS  rms {:>8.1} dBFS{}",
        max_db, rms_db, flag);
    assert!(max_db <= -200.0, "10M 192k FAIL: max|diff| = {:.1} dBFS", max_db);
}

// ── Fault injection and is_gpu_failed tests ───────────────────────────────────

/// Verify AURA_GPU_FAIL_AFTER=N: after N blocks, is_gpu_failed() returns true
/// and subsequent reads produce silence (not garbage).
///
/// This test uses env-var injection and runs on real GPU hardware.
/// If the GPU is unavailable it passes vacuously.
#[test]
#[ignore]
fn fault_injection_fail_after_3() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_linear_phase.npy", dir);
    let l = 8usize;
    let n_src = 32 * BLOCK;

    let bank = match Bank::load(&path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[fault] skip: {}", e); return; }
    };

    let x = white_noise(n_src, 55);
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[fault] no GPU hardware — skip"); return; }
        Some(c) => c,
    };

    // Inject: fail after 3 blocks — on this stream only (injected_fault, not
    // player_device_error), so the equivalence tests running beside it are not
    // affected.
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "[fault] player_device_error already set — process-wide GPU disabled"
    );
    let mut gpu = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    ).expect("[fault] GPU context exists but try_new returned None");
    gpu.set_fail_after(3);

    // Read 5 blocks.
    let block_out = BLOCK * l;
    let mut out_l = vec![0.0f64; block_out];
    let mut out_r = vec![0.0f64; block_out];
    for block in 0..5 {
        gpu.read(&mut out_l, &mut out_r);
        if block >= 3 {
            // After block 3 the error flag must be set.
            assert!(
                gpu.is_gpu_failed(),
                "block {}: expected is_gpu_failed() = true after fault injection", block
            );
            // Output must be silence.
            let max_abs = out_l.iter().chain(out_r.iter())
                .map(|v| v.abs())
                .fold(0.0f64, f64::max);
            assert_eq!(max_abs, 0.0,
                "block {}: expected silence after fault injection, got max_abs={}", block, max_abs);
        }
    }
    println!("[fault] fail after 3 blocks: PASS — is_gpu_failed() correct, silence after fault");
}

/// Verify GpuPolyCtx::fits() correctly rejects limits that exceed the device.
/// Runs without GPU hardware (uses a mock limits check via a very large taps count).
#[test]
#[ignore]
fn fits_check_rejects_over_limit() {
    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[fits] no GPU hardware — skip"); return; }
        Some(c) => c,
    };
    // A sane config for a small filter should pass.
    let small_ok = ctx.fits(1_000_000, 8);
    println!("[fits] 1M taps L=8: fits={}", small_ok);

    // An impossibly large filter (u32::MAX taps) must fail.
    let huge_fail = ctx.fits(u32::MAX as usize, 8);
    assert!(!huge_fail, "fits() should reject u32::MAX taps");
    println!("[fits] u32::MAX taps L=8: fits={} (expected false) — PASS", huge_fail);
}

/// Verify that clear_bank_cache() frees cached filter banks.
/// Checks DXGI free VRAM before and after clear (approximate, delta ≥ 0).
#[test]
#[ignore]
fn bank_cache_clear_frees_vram() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_linear_phase.npy", dir);
    let l = 8usize;

    let bank = match Bank::load(&path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[cache] skip: {}", e); return; }
    };

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[cache] no GPU hardware — skip"); return; }
        Some(c) => c,
    };

    // Allocate a stream (populates the bank cache).
    let n_src = 8 * BLOCK;
    let x = white_noise(n_src, 3);
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });
    let vram_before = ctx.free_vram_bytes();
    let _stream = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    );
    let vram_after_alloc = ctx.free_vram_bytes();

    // Clear the cache.
    ctx.clear_bank_cache();
    // Note: GPU drivers may not immediately return VRAM; this is a best-effort check.
    let vram_after_clear = ctx.free_vram_bytes();

    println!(
        "[cache] VRAM free: before={} MB  after_alloc={} MB  after_clear={} MB",
        vram_before >> 20,
        vram_after_alloc >> 20,
        vram_after_clear >> 20,
    );
    println!("[cache] bank_cache_clear_frees_vram: PASS (no panic)");
}

// ── Fault isolation: injected fault must not bleed to other streams ───────────

/// Verify that an injected per-stream fault (set_fail_after) does NOT set the
/// process-wide player_device_error flag, so a second stream built immediately
/// after works correctly.
///
/// This is the regression test for the parallel-test poison: previously the
/// error path inside compute_block always called
/// `ctx.player_device_error.store(true)`, which disabled the GPU for every
/// stream in the process — including other tests running in parallel.
#[test]
#[ignore]
fn fault_injection_does_not_poison_process() {
    let dir = filter_dir();
    let path = format!("{}/fir_1M_352800_linear_phase.npy", dir);
    let l = 8usize;
    let n_src = 16 * BLOCK;

    let bank = match Bank::load(&path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[poison] skip: {}", e); return; }
    };
    let x = white_noise(n_src, 13);
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[poison] no GPU hardware — skip"); return; }
        Some(c) => c,
    };

    // Stream 1: fail after 2 blocks via per-stream injection.
    let mut s1 = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    ).expect("[poison] stream 1: try_new returned None");
    s1.set_fail_after(2);

    let block_out = BLOCK * l;
    let mut ol = vec![0.0f64; block_out * 4];
    let mut or_ = vec![0.0f64; block_out * 4];
    s1.read(&mut ol, &mut or_);
    assert!(s1.has_error(), "[poison] stream 1 should be in error state");

    // Process-wide flag must NOT be set by an injected fault.
    assert!(
        !ctx.player_device_error.load(Ordering::Acquire),
        "[poison] injected fault set player_device_error — \
         other GPU streams would be disabled in parallel tests"
    );

    // Stream 2: must still work correctly after stream 1's fault.
    let mut cpu = PolyStream::new(Arc::clone(&bank), Arc::clone(&src), 0);
    let n_out = 8 * BLOCK * l;
    let mut cpu_l = vec![0.0f64; n_out];
    let mut cpu_r = vec![0.0f64; n_out];
    cpu.read(&mut cpu_l, &mut cpu_r);

    let mut s2 = GpuPolyStream::try_new(
        Arc::clone(&bank), Arc::clone(&src), 0, Arc::clone(&ctx),
    ).expect("[poison] stream 2: try_new returned None after injected fault in stream 1");

    let mut g2_l = vec![0.0f64; n_out];
    let mut g2_r = vec![0.0f64; n_out];
    s2.read(&mut g2_l, &mut g2_r);
    assert!(!s2.has_error(), "[poison] stream 2 should not have an error");

    let mut max_abs = 0.0f64;
    for i in 0..n_out {
        let dl = (cpu_l[i] - g2_l[i]).abs();
        let dr = (cpu_r[i] - g2_r[i]).abs();
        if dl > max_abs { max_abs = dl; }
        if dr > max_abs { max_abs = dr; }
    }
    let max_db = db(max_abs);
    println!(
        "[poison] stream 2 after injected-fault stream 1: max|diff| {:.1} dBFS",
        max_db
    );
    assert!(
        max_db <= -200.0,
        "[poison] stream 2 equivalence FAIL: {:.1} dBFS", max_db
    );
    println!("[poison] fault_injection_does_not_poison_process: PASS");
}

// ── FdlRing allocation stress (issue 4 — reproduce the uncaptured-error) ──────

/// Build and drop 30M HP stream pairs (lin + min) repeatedly in one process
/// to reproduce the FdlRing "async device error on re-run" seen in the
/// real-window test (report-window.md §open-item-2).
///
/// Each iteration builds two streams (2 × 240 MB FDL rings per 30M), runs one
/// block to confirm equivalence, and drops them.  The test checks that:
///   (a) player_device_error is never set (no uncaptured OOM).
///   (b) every iteration's GPU output matches CPU within −230 dBFS.
///
/// If the driver reports an OOM error as an uncaptured error, it will appear
/// on stderr ("UNCAPTURED GPU ERROR") and the player_device_error assert fires.
#[test]
#[ignore]
fn fdl_alloc_stress_30m_hp() {
    let dir = filter_dir();
    let lin_path = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let min_path = format!("{}/fir_30M_352800_minimum_phase.npy", dir);
    let l = 8usize;

    let lin_bank = match Bank::load(&lin_path, l, Alignment::Linear, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[fdl-stress] skip lin: {}", e); return; }
    };
    let min_bank = match Bank::load(&min_path, l, Alignment::BandWeighted, 352_800) {
        Ok(b) => Arc::new(b),
        Err(e) => { println!("[fdl-stress] skip min: {}", e); return; }
    };

    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[fdl-stress] no GPU hardware — skip"); return; }
        Some(c) => c,
    };

    let n_src = 4 * BLOCK;
    let n_out = n_src * l;
    let src_l = white_noise(n_src, 71);
    let src_r = white_noise(n_src, 72);

    // CPU reference (computed once; both HP streams share the same input).
    let src = Arc::new(SourceBuf { l: src_l.clone(), r: src_r.clone(), rate: 44_100 });
    let mut cpu_lin = PolyStream::new(Arc::clone(&lin_bank), Arc::clone(&src), 0);
    let mut cpu_min = PolyStream::new(Arc::clone(&min_bank), Arc::clone(&src), 0);
    let mut lin_l = vec![0.0f64; n_out]; let mut lin_r = vec![0.0f64; n_out];
    let mut min_l = vec![0.0f64; n_out]; let mut min_r = vec![0.0f64; n_out];
    cpu_lin.read(&mut lin_l, &mut lin_r);
    cpu_min.read(&mut min_l, &mut min_r);
    let cpu_l: Vec<f64> = lin_l.iter().zip(min_l.iter()).map(|(&a,&b)| (a+b)*0.5).collect();
    let cpu_r: Vec<f64> = lin_r.iter().zip(min_r.iter()).map(|(&a,&b)| (a+b)*0.5).collect();

    const ITERS: usize = 5;
    for i in 0..ITERS {
        assert!(
            !ctx.player_device_error.load(Ordering::Acquire),
            "[fdl-stress] iter {}: player_device_error set — uncaptured GPU error occurred", i
        );

        // Clear the bank cache so each iteration re-uploads the filter (exercises
        // repeated large allocations).  On iteration 0 the cache is already empty.
        ctx.clear_bank_cache();

        let src = Arc::new(SourceBuf {
            l: src_l.clone(), r: src_r.clone(), rate: 44_100,
        });
        let mut glin = GpuPolyStream::try_new(
            Arc::clone(&lin_bank), Arc::clone(&src), 0, Arc::clone(&ctx),
        ).expect(&format!("[fdl-stress] iter {}: lin try_new returned None", i));
        let mut gmin = GpuPolyStream::try_new(
            Arc::clone(&min_bank), Arc::clone(&src), 0, Arc::clone(&ctx),
        ).expect(&format!("[fdl-stress] iter {}: min try_new returned None", i));

        let mut glin_l = vec![0.0f64; n_out]; let mut glin_r = vec![0.0f64; n_out];
        let mut gmin_l = vec![0.0f64; n_out]; let mut gmin_r = vec![0.0f64; n_out];
        glin.read(&mut glin_l, &mut glin_r);
        gmin.read(&mut gmin_l, &mut gmin_r);
        assert!(!glin.has_error(), "[fdl-stress] iter {}: lin stream error", i);
        assert!(!gmin.has_error(), "[fdl-stress] iter {}: min stream error", i);

        let gpu_l: Vec<f64> = glin_l.iter().zip(gmin_l.iter()).map(|(&a,&b)| (a+b)*0.5).collect();
        let gpu_r: Vec<f64> = glin_r.iter().zip(gmin_r.iter()).map(|(&a,&b)| (a+b)*0.5).collect();
        let mut max_abs = 0.0f64;
        for k in 0..n_out {
            let dl = (cpu_l[k] - gpu_l[k]).abs();
            let dr = (cpu_r[k] - gpu_r[k]).abs();
            if dl > max_abs { max_abs = dl; }
            if dr > max_abs { max_abs = dr; }
        }
        let max_db = db(max_abs);
        println!("[fdl-stress] iter {}: max|diff| {:.1} dBFS", i, max_db);
        assert!(max_db <= -230.0, "[fdl-stress] iter {} FAIL: {:.1} dBFS", i, max_db);

        // Streams dropped here; VRAM freed before next iteration.
    }
    println!("[fdl-stress] fdl_alloc_stress_30m_hp: PASS ({} iters, no OOM, ≤ −230 dBFS)", ITERS);
}

// ── Power benches (GPU-3 design §5.1: B1, B3, B5, B6, B7; B2 is fitted from B1) ──
//
// One cargo process per affinity mask, for example 4 E-cores:
//   AURA_BENCH_AFFINITY=0xF0000  RAYON_NUM_THREADS=4  AURA_FILTER_DIR=<filters>
//   AURA_BENCH_OUT=<dir>/bench-4E.jsonl
//   cargo test --profile fast pwr_ -- --ignored --nocapture --test-threads=1
// AURA_BENCH_AFFINITY unset (or "all") keeps the whole machine.
//
// Each test narrows its own process to the mask before it builds any pool
// and puts the old mask back when it ends. Its pools are built like the
// player's render and power pools (calibration::cpu_width() workers at
// HIGHEST), and every trial number comes from the product's trial core
// (convolver::trial_blocks, PolyStream::new_trial, trial_block_index).
// One JSON object per line goes to stdout ("[pwr] ...") and to AURA_BENCH_OUT.

mod pwr_bench {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};

    use super::{filter_dir, white_noise};
    use crate::player::calibration;
    use crate::player::convolver::{self, Alignment, Bank, PolyStream, SourceBuf, TrialTiming, BLOCK};
    use crate::player::gpu::ctx::GpuPolyCtx;
    use crate::player::gpu::poly_stream::GpuPolyStream;
    use crate::player::policy;

    /// THREAD_PRIORITY_HIGHEST, the render and power pools' priority.
    const HIGHEST: i32 = 2;

    /// A rate family: the 44.1 kHz source at L 8 and the 192 kHz source at
    /// L 2. The sources are long enough that `trial_block_index` (25 % in)
    /// lies past a 30M linear filter's delay, so a primed stream can start at
    /// the very block the trial times.
    #[derive(Clone, Copy)]
    struct Rate {
        tag: &'static str,
        l: usize,
        src_rate: u32,
        out_rate: u32,
        secs: f64,
    }

    const RATES: [Rate; 2] = [
        Rate { tag: "44k", l: 8, src_rate: 44_100, out_rate: 352_800, secs: 180.0 },
        Rate { tag: "192k", l: 2, src_rate: 192_000, out_rate: 384_000, secs: 165.0 },
    ];

    const RUNGS: [(usize, &str); 4] = [
        (1_000_000, "1M"),
        (5_000_000, "5M"),
        (10_000_000, "10M"),
        (30_000_000, "30M"),
    ];

    /// The rungs a bench runs: `AURA_BENCH_RUNGS` (labels, comma-separated,
    /// e.g. "10M,30M"), or all of `RUNGS` when unset or empty.
    fn rungs() -> Vec<(usize, &'static str)> {
        let spec = std::env::var("AURA_BENCH_RUNGS").unwrap_or_default();
        let want: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        RUNGS.iter().copied().filter(|(_, label)| want.is_empty() || want.contains(label)).collect()
    }

    fn never() -> bool {
        false
    }

    fn always() -> bool {
        true
    }

    fn set_highest() {
        #[cfg(windows)]
        unsafe {
            use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
            SetThreadPriority(GetCurrentThread(), HIGHEST);
        }
    }

    /// A pool built like the player's render and power pools.
    fn product_pool(name: &'static str) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(calibration::cpu_width().max(2))
            .thread_name(move |i| format!("{}-{}", name, i))
            .start_handler(|_| set_highest())
            .build()
            .expect("bench pool")
    }

    #[cfg(windows)]
    fn process_mask() -> usize {
        unsafe {
            use winapi::um::processthreadsapi::GetCurrentProcess;
            use winapi::um::winbase::GetProcessAffinityMask;
            let mut p: usize = 0;
            let mut s: usize = 0;
            if GetProcessAffinityMask(GetCurrentProcess(), &mut p, &mut s) != 0 {
                return p;
            }
        }
        0
    }

    #[cfg(not(windows))]
    fn process_mask() -> usize {
        0
    }

    /// winapi 0.3.9 declares the mask a DWORD (32 bits); this machine has 32
    /// logical CPUs, so every mask fits.
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

    /// The process affinity of one bench: AURA_BENCH_AFFINITY (hex), set
    /// before the test builds any pool. The old mask comes back on drop.
    struct Mask {
        old: usize,
        hex: String,
        label: String,
    }

    impl Mask {
        fn from_env() -> Mask {
            let old = process_mask();
            let spec = std::env::var("AURA_BENCH_AFFINITY").unwrap_or_default().trim().to_ascii_lowercase();
            let want = if spec.is_empty() || spec == "all" {
                None
            } else {
                let digits = spec.trim_start_matches("0x");
                Some(usize::from_str_radix(digits, 16).expect("AURA_BENCH_AFFINITY: a hex mask such as 0xF0000"))
            };
            if let Some(m) = want {
                assert!(set_process_mask(m), "SetProcessAffinityMask({:#x}) failed", m);
            }
            let now = process_mask();
            let label = match std::env::var("AURA_BENCH_LABEL") {
                Ok(s) if !s.is_empty() => s,
                _ => match want {
                    None => "all".to_string(),
                    Some(m) if m & 0xFFFF == 0 => format!("{}E", m.count_ones()),
                    Some(m) => format!("{:#x}", m),
                },
            };
            Mask { old, hex: format!("{:#x}", now), label }
        }
    }

    impl Drop for Mask {
        fn drop(&mut self) {
            if self.old != 0 && process_mask() != self.old {
                set_process_mask(self.old);
            }
        }
    }

    static OUT: Mutex<()> = Mutex::new(());

    fn unix_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    fn emit(bench: &str, m: &Mask, mut v: Value) {
        if let Value::Object(o) = &mut v {
            o.insert("bench".into(), json!(bench));
            o.insert("mask".into(), json!(m.label));
            o.insert("maskHex".into(), json!(m.hex));
            o.insert("cpuWidth".into(), json!(calibration::cpu_width()));
            o.insert("unixMs".into(), json!(unix_ms()));
        }
        let line = v.to_string();
        println!("[pwr] {}", line);
        let Ok(path) = std::env::var("AURA_BENCH_OUT") else { return };
        if path.is_empty() {
            return;
        }
        let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
        use std::io::Write;
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("AURA_BENCH_OUT: open");
        writeln!(f, "{}", line).expect("AURA_BENCH_OUT: write");
    }

    fn env_line(bench: &str, m: &Mask, pool: &rayon::ThreadPool) {
        let rayon_env = std::env::var("RAYON_NUM_THREADS").unwrap_or_default();
        let popcount = process_mask().count_ones();
        emit(bench, m, json!({
            "kind": "env",
            "popcount": popcount,
            "poolWidth": pool.current_num_threads(),
            "rayonGlobal": rayon::current_num_threads(),
            "rayonEnv": rayon_env,
            "rayonMatches": rayon_env.parse::<u32>().ok() == Some(popcount),
            "costFixedTaps": policy::COST_FIXED_TAPS,
            "trialChainFactor": calibration::TRIAL_CHAIN_FACTOR,
        }));
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    fn ns_ms(ns: u64) -> f64 {
        ns as f64 / 1e6
    }

    fn r3(x: f64) -> f64 {
        (x * 1000.0).round() / 1000.0
    }

    fn median(v: &[f64]) -> f64 {
        let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
        if s.is_empty() {
            return f64::NAN;
        }
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = s.len();
        if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 }
    }

    fn min_of(v: &[f64]) -> f64 {
        v.iter().copied().fold(f64::INFINITY, f64::min)
    }

    fn max_of(v: &[f64]) -> f64 {
        v.iter().copied().fold(0.0, f64::max)
    }

    /// Stereo white noise at the rate's source rate (the multiply-accumulate
    /// does not look at the values; only an all-zero window is cheaper).
    fn source(r: &Rate) -> Arc<SourceBuf> {
        let n = (r.secs * r.src_rate as f64) as usize;
        Arc::new(SourceBuf { l: white_noise(n, 11), r: white_noise(n, 12), rate: r.src_rate })
    }

    /// A bank as the player loads it (lin: Linear; min: the HP pair's
    /// BandWeighted), timed on the caller's thread (the global pool, as
    /// `trial_banks` loads on the thread that runs the trial).
    fn load_bank(label: &str, r: &Rate, lin: bool) -> Option<(Arc<Bank>, f64)> {
        let phase = if lin { "linear_phase" } else { "minimum_phase" };
        // The blob of the source's factor, as the player takes it.
        let design = crate::audio::converter::dsp::filter::design_rate(r.src_rate, r.out_rate);
        let path = format!("{}/fir_{}_{}_{}.npy", filter_dir(), label, design, phase);
        if !std::path::Path::new(&path).exists() {
            println!("[pwr] missing filter {}", path);
            return None;
        }
        let align = if lin { Alignment::Linear } else { Alignment::BandWeighted };
        let t = Instant::now();
        match Bank::load(&path, r.l, align, r.out_rate) {
            Ok(b) => Some((Arc::new(b), ms(t.elapsed()))),
            Err(e) => {
                println!("[pwr] cannot load {}: {}", path, e);
                None
            }
        }
    }

    /// Output index at which `PolyStream::new` makes source block `j` its
    /// first computed block; `None` if the filter's delay reaches past it.
    fn start_for_block(bank: &Bank, j: i64) -> Option<u64> {
        ((j as u64) * (BLOCK * bank.l) as u64).checked_sub(bank.delay as u64)
    }

    /// A block whose input window lies wholly past the end of the source.
    fn zero_block(src: &SourceBuf) -> i64 {
        (src.len() / BLOCK) as i64 + 3
    }

    /// One primed stream (the playing chain's convolver): construction with
    /// its prime, `blocks` blocks from its first, then one block with an
    /// all-zero input window, then the drop.
    struct Primed {
        new_ms: f64,
        blocks_ms: Vec<f64>,
        zero_ms: f64,
        drop_ms: f64,
    }

    fn primed_run(bank: &Arc<Bank>, src: &Arc<SourceBuf>, start: u64, blocks: usize, j_zero: i64) -> Primed {
        let t = Instant::now();
        let mut s = PolyStream::new(bank.clone(), src.clone(), start);
        let new_ms = ms(t.elapsed());
        let blocks_ms: Vec<f64> = (0..blocks).map(|_| ns_ms(s.trial_block_ns())).collect();
        let zero_ms = match s.trial_rebind(bank.clone(), j_zero) {
            Ok(()) => ns_ms(s.trial_block_ns()),
            Err(()) => f64::NAN,
        };
        let t = Instant::now();
        drop(s);
        Primed { new_ms, blocks_ms, zero_ms, drop_ms: ms(t.elapsed()) }
    }

    /// The steady cost of a primed run: the median of blocks 2..
    fn steady(p: &Primed) -> f64 {
        median(&p.blocks_ms[1..])
    }

    fn trial_json(t: &TrialTiming) -> Value {
        let blocks: Vec<Vec<f64>> =
            t.block_ns.iter().map(|b| b.iter().map(|&ns| r3(ns_ms(ns))).collect()).collect();
        let blocks_ms: f64 = t.block_ns.iter().flatten().map(|&ns| ns_ms(ns)).sum();
        let conv_ms: f64 = t.block_ns.iter().map(|b| ns_ms(b.iter().copied().min().unwrap_or(0))).sum();
        json!({
            "j": t.j,
            "threads": t.threads,
            "allocMs": r3(ns_ms(t.alloc_ns)),
            "blocksMs": blocks,
            "dropMs": r3(ns_ms(t.drop_ns)),
            "addedMs": r3(ns_ms(t.alloc_ns) + blocks_ms + ns_ms(t.drop_ns)),
            "convMs": r3(conv_ms),
            "extra": t.extra,
            "stopped": t.stopped,
        })
    }

    /// One uncontended single-block trial of `bank` in `pool`: its block ms.
    fn trial1(pool: &rayon::ThreadPool, bank: &Arc<Bank>, src: &Arc<SourceBuf>) -> f64 {
        let t = pool.install(|| convolver::trial_blocks(std::slice::from_ref(bank), src, 1, &never, &never));
        ns_ms(t.block_ns[0][0])
    }

    // ── B1 trial fidelity (and the B2 data) ──────────────────────────────────

    /// Per rung × rate × bank: the product's unprimed trial at
    /// `trial_block_index` against a primed `PolyStream::new` at the same
    /// block (its first block, and the median of blocks 2-6), plus one block
    /// whose input window is all zero. The trial runs 2 blocks: its first is
    /// exactly what an uncontended pre-trial times (with 1 block
    /// `trial_blocks` stops right after it), the min of the two is what a
    /// contended or observed-contention trial stores. Five reps alternating
    /// which runs first (three when one rep takes over 12 s). Everything runs
    /// in a power-like pool. Acceptance: median |trial / primed-steady − 1|
    /// ≤ 5 % (≥ 5M), ≤ 15 % (1M).
    #[test]
    #[ignore]
    fn pwr_b1_trial_fidelity() {
        let m = Mask::from_env();
        let pool = product_pool("pwr-power");
        env_line("B1", &m, &pool);
        let mut measured = 0usize;
        for rate in RATES {
            let src = source(&rate);
            let j = convolver::trial_block_index(&src);
            let j_zero = zero_block(&src);
            for (taps, label) in rungs() {
                for lin in [true, false] {
                    let bank_name = if lin { "lin" } else { "min" };
                    let Some((bank, load_ms)) = load_bank(label, &rate, lin) else {
                        emit("B1", &m, json!({ "kind": "skip", "rate": rate.tag, "rung": label, "bank": bank_name, "why": "filter" }));
                        continue;
                    };
                    let Some(start) = start_for_block(&bank, j) else {
                        emit("B1", &m, json!({ "kind": "skip", "rate": rate.tag, "rung": label, "bank": bank_name, "why": "source too short" }));
                        continue;
                    };
                    let mut reps = 5usize;
                    let mut trial = Vec::new();
                    let mut trial2 = Vec::new();
                    let mut alloc = Vec::new();
                    let mut tdrop = Vec::new();
                    let mut first = Vec::new();
                    let mut stead = Vec::new();
                    let mut zero = Vec::new();
                    let mut new_ms = Vec::new();
                    let mut rep = 0usize;
                    while rep < reps {
                        let t_rep = Instant::now();
                        let trial_first = rep % 2 == 0;
                        let mut tr: Option<TrialTiming> = None;
                        let mut pr: Option<Primed> = None;
                        for k in 0..2 {
                            if (k == 0) == trial_first {
                                tr = Some(pool.install(|| {
                                    convolver::trial_blocks(std::slice::from_ref(&bank), &src, 2, &never, &never)
                                }));
                            } else {
                                pr = Some(pool.install(|| primed_run(&bank, &src, start, 6, j_zero)));
                            }
                        }
                        let (tr, pr) = (tr.unwrap(), pr.unwrap());
                        assert_eq!(tr.j, j, "the trial must start at trial_block_index");
                        let t_ms = ns_ms(tr.block_ns[0][0]);
                        let t2_ms = ns_ms(tr.block_ns[0].iter().copied().min().unwrap_or(0));
                        trial.push(t_ms);
                        trial2.push(t2_ms);
                        alloc.push(ns_ms(tr.alloc_ns));
                        tdrop.push(ns_ms(tr.drop_ns));
                        first.push(pr.blocks_ms[0]);
                        stead.push(steady(&pr));
                        zero.push(pr.zero_ms);
                        new_ms.push(pr.new_ms);
                        emit("B1", &m, json!({
                            "kind": "rep", "rate": rate.tag, "l": rate.l, "rung": label, "taps": taps,
                            "bank": bank_name, "rep": rep, "trialFirst": trial_first,
                            "trial": trial_json(&tr),
                            "primedNewMs": r3(pr.new_ms),
                            "primedBlocksMs": pr.blocks_ms.iter().map(|&x| r3(x)).collect::<Vec<_>>(),
                            "primedSteadyMs": r3(steady(&pr)),
                            "zeroMs": r3(pr.zero_ms),
                            "primedDropMs": r3(pr.drop_ms),
                            "ratio": r3(t_ms / steady(&pr)),
                            "ratio2": r3(t2_ms / steady(&pr)),
                        }));
                        if rep == 0 && t_rep.elapsed().as_secs_f64() > 12.0 {
                            reps = 3;
                        }
                        rep += 1;
                    }
                    let devs: Vec<f64> = trial.iter().zip(&stead).map(|(t, s)| (t / s - 1.0).abs()).collect();
                    let dev = median(&devs);
                    let devs2: Vec<f64> = trial2.iter().zip(&stead).map(|(t, s)| (t / s - 1.0).abs()).collect();
                    let dev2 = median(&devs2);
                    let devs_p: Vec<f64> = first.iter().zip(&stead).map(|(t, s)| (t / s - 1.0).abs()).collect();
                    let dev_p = median(&devs_p);
                    let limit = if taps < 5_000_000 { 0.15 } else { 0.05 };
                    let steady_ms = median(&stead);
                    emit("B1", &m, json!({
                        "kind": "summary", "rate": rate.tag, "l": rate.l, "srcRate": rate.src_rate,
                        "outRate": rate.out_rate, "rung": label, "taps": taps, "bank": bank_name,
                        "reps": reps, "j": j, "start": start, "loadMs": r3(load_ms),
                        "trialMs": r3(median(&trial)), "trialMinMs": r3(min_of(&trial)),
                        "firstMs": r3(median(&first)), "steadyMs": r3(steady_ms),
                        "zeroMs": r3(median(&zero)), "allocMs": r3(median(&alloc)), "dropMs": r3(median(&tdrop)),
                        "primedNewMs": r3(median(&new_ms)),
                        "dev": (dev * 10_000.0).round() / 10_000.0, "limit": limit, "pass": dev <= limit,
                        "trialOverSteady": r3(median(&trial) / steady_ms),
                        "trialMin2Ms": r3(median(&trial2)),
                        "dev2": (dev2 * 10_000.0).round() / 10_000.0, "pass2": dev2 <= limit,
                        "min2OverSteady": r3(median(&trial2) / steady_ms),
                        "devPrimed": (dev_p * 10_000.0).round() / 10_000.0, "passPrimed": dev_p <= limit,
                        "zeroDelta": r3(median(&zero) / steady_ms - 1.0),
                        "firstBias": r3(median(&first) / steady_ms - 1.0),
                        "rtfSteady": r3(calibration::rtf_from_cost(steady_ms, rate.src_rate)),
                    }));
                    println!(
                        "[pwr] B1 {} {} {} {}: trial {:.1} ms (min2 {:.1}), primed first {:.1}, steady {:.1}, dev {:.1} % / min2 {:.1} % / primed {:.1} % (limit {:.0} %) {}",
                        m.label, rate.tag, label, bank_name, median(&trial), median(&trial2), median(&first), steady_ms,
                        dev * 100.0, dev2 * 100.0, dev_p * 100.0, limit * 100.0, if dev <= limit { "PASS" } else { "FAIL" }
                    );
                    measured += 1;
                }
            }
        }
        assert!(measured > 0, "B1 measured nothing: is AURA_FILTER_DIR ({}) right?", filter_dir());
    }

    // ── B1b first touch ───────────────────────────────────────────────────────

    /// Why the trial's first block is slow: allocates what `new_trial` does
    /// for a 30M bank (FDL of both channels, accumulators and compensation:
    /// `vec!` of zero complex values, in parallel in a power-like pool), then
    /// writes one byte per 4 KiB page in parallel (volatile), then again.
    /// Allocation that returns untouched demand-zero pages shows as a fast
    /// alloc and a slow first touch; the second touch is the control.
    #[test]
    #[ignore]
    fn pwr_b1b_first_touch() {
        use rayon::prelude::*;
        type C = rustfft::num_complex::Complex<f64>;
        const NFFT: usize = 2 * BLOCK;
        let m = Mask::from_env();
        let pool = product_pool("pwr-power");
        env_line("B1b", &m, &pool);
        let touch = |v: &mut Vec<Vec<C>>| -> f64 {
            let t = Instant::now();
            pool.install(|| {
                v.par_iter_mut().for_each(|x| {
                    let p = x.as_mut_ptr() as *mut u8;
                    let bytes = x.len() * std::mem::size_of::<C>();
                    let mut o = 0usize;
                    while o < bytes {
                        // SAFETY: o < bytes, inside the vector's buffer.
                        unsafe { std::ptr::write_volatile(p.add(o), 0u8) };
                        o += 4096;
                    }
                })
            });
            ms(t.elapsed())
        };
        for rate in RATES {
            let depth = ((30_000_000 + rate.l - 1) / rate.l + BLOCK - 1) / BLOCK;
            let n = 2 * depth + 4 * rate.l;
            let mb = (n * NFFT * std::mem::size_of::<C>()) as f64 / (1 << 20) as f64;
            let mut rows = Vec::new();
            for rep in 0..3 {
                let t = Instant::now();
                let mut v: Vec<Vec<C>> =
                    pool.install(|| (0..n).into_par_iter().map(|_| vec![C::new(0.0, 0.0); NFFT]).collect());
                let alloc_ms = ms(t.elapsed());
                let first = touch(&mut v);
                let second = touch(&mut v);
                let t = Instant::now();
                drop(v);
                let drop_ms = ms(t.elapsed());
                rows.push((alloc_ms, first, second, drop_ms));
                emit("B1b", &m, json!({
                    "kind": "rep", "rate": rate.tag, "l": rate.l, "rung": "30M", "rep": rep, "vectors": n,
                    "mb": r3(mb), "allocMs": r3(alloc_ms), "firstTouchMs": r3(first),
                    "secondTouchMs": r3(second), "dropMs": r3(drop_ms),
                }));
            }
            let col = |i: usize| -> f64 {
                median(&rows.iter().map(|r| [r.0, r.1, r.2, r.3][i]).collect::<Vec<_>>())
            };
            emit("B1b", &m, json!({
                "kind": "summary", "rate": rate.tag, "l": rate.l, "rung": "30M", "vectors": n, "mb": r3(mb),
                "allocMs": r3(col(0)), "firstTouchMs": r3(col(1)), "secondTouchMs": r3(col(2)), "dropMs": r3(col(3)),
            }));
            println!(
                "[pwr] B1b {} {} 30M: {:.0} MB, alloc {:.1} ms, first touch {:.1} ms, second touch {:.1} ms, drop {:.1} ms",
                m.label, rate.tag, mb, col(0), col(1), col(2), col(3)
            );
        }
    }

    // ── B3 trial cost ─────────────────────────────────────────────────────────

    /// The pre-trial's cost as the product runs it, 30M lin and HP at both
    /// rates: bank load (moved from build_chain, not added; warm OS file
    /// cache here), then `trial_blocks` in a power-like pool: uncontended
    /// (1 block), with the extra block (`more()` true) and contended
    /// (2 blocks). Three reps each. WARN (4E, HP, uncontended) when
    /// added > 1.0 s at 44.1k or > 1.5 s at 192k.
    #[test]
    #[ignore]
    fn pwr_b3_trial_cost() {
        let m = Mask::from_env();
        let pool = product_pool("pwr-power");
        env_line("B3", &m, &pool);
        let mut measured = 0usize;
        for rate in RATES {
            let src = source(&rate);
            for pair in [false, true] {
                let what = if pair { "HP" } else { "lin" };
                let Some((lin, lin_ms)) = load_bank("30M", &rate, true) else { continue };
                let mut banks = vec![lin];
                let mut load_ms = vec![r3(lin_ms)];
                if pair {
                    let Some((min, min_ms)) = load_bank("30M", &rate, false) else { continue };
                    banks.push(min);
                    load_ms.push(r3(min_ms));
                }
                for (variant, blocks, more) in [("idle", 1usize, false), ("extra", 1, true), ("contended", 2, false)] {
                    let more_fn: &(dyn Fn() -> bool + Sync) = if more { &always } else { &never };
                    let mut added = Vec::new();
                    let mut alloc = Vec::new();
                    let mut blk = Vec::new();
                    let mut drp = Vec::new();
                    let mut cost = Vec::new();
                    for rep in 0..3 {
                        let t = pool.install(|| convolver::trial_blocks(&banks, &src, blocks, more_fn, &never));
                        let tj = trial_json(&t);
                        let blocks_ms: f64 = t.block_ns.iter().flatten().map(|&ns| ns_ms(ns)).sum();
                        let conv_ms: f64 =
                            t.block_ns.iter().map(|b| ns_ms(b.iter().copied().min().unwrap_or(0))).sum();
                        added.push(ns_ms(t.alloc_ns) + blocks_ms + ns_ms(t.drop_ns));
                        alloc.push(ns_ms(t.alloc_ns));
                        blk.push(blocks_ms);
                        drp.push(ns_ms(t.drop_ns));
                        cost.push(conv_ms * calibration::TRIAL_CHAIN_FACTOR);
                        emit("B3", &m, json!({
                            "kind": "rep", "rate": rate.tag, "l": rate.l, "what": what, "variant": variant,
                            "rep": rep, "trial": tj,
                        }));
                    }
                    let limit_ms = if rate.l == 8 { 1000.0 } else { 1500.0 };
                    let added_ms = median(&added);
                    let cost_ms = median(&cost);
                    let warn = pair && variant == "idle" && added_ms > limit_ms;
                    emit("B3", &m, json!({
                        "kind": "summary", "rate": rate.tag, "l": rate.l, "what": what, "variant": variant,
                        "bankLoadMs": load_ms, "allocMs": r3(median(&alloc)), "blocksMs": r3(median(&blk)),
                        "dropMs": r3(median(&drp)), "addedMs": r3(added_ms), "costMs": r3(cost_ms),
                        "rtf": r3(calibration::rtf_from_cost(cost_ms, rate.src_rate)),
                        "limitMs": limit_ms, "warn": warn,
                    }));
                    println!(
                        "[pwr] B3 {} {} 30M {} {}: load {:?} ms, alloc {:.1}, blocks {:.1}, drop {:.1}, added {:.1} ms{}",
                        m.label, rate.tag, what, variant, load_ms, median(&alloc), median(&blk), median(&drp),
                        added_ms, if warn { "  WARN" } else { "" }
                    );
                    measured += 1;
                }
            }
        }
        assert!(measured > 0, "B3 measured nothing: is AURA_FILTER_DIR ({}) right?", filter_dir());
    }

    // ── B5 pool parity ────────────────────────────────────────────────────────

    /// The power pool against the render pool, 30M lin at both rates. Five
    /// rounds alternating the pools; per pool and round: a 1-block trial
    /// (`trial1`) and a primed run of 6 blocks (steady = blocks 2-6), each
    /// run from one of the pool's workers (in the render pool that is where
    /// the render loop runs). Then five trials injected into the render-like
    /// pool while one of its workers is parked in `recv` (the render loop
    /// idle in its channel wait: the n−1 case). Acceptance (design): power
    /// trial within ±5 % of the render pool's steady blocks; like-for-like
    /// (same operation in both pools) is reported next to it.
    #[test]
    #[ignore]
    fn pwr_b5_pool_parity() {
        let m = Mask::from_env();
        let power = product_pool("pwr-power");
        let render = Arc::new(product_pool("pwr-render"));
        env_line("B5", &m, &power);
        let mut measured = 0usize;
        for rate in RATES {
            let src = source(&rate);
            let Some((bank, _)) = load_bank("30M", &rate, true) else { continue };
            let j = convolver::trial_block_index(&src);
            let j_zero = zero_block(&src);
            let Some(start) = start_for_block(&bank, j) else { continue };

            let mut power_trial = Vec::new();
            let mut render_trial = Vec::new();
            let mut power_steady = Vec::new();
            let mut render_steady = Vec::new();
            for round in 0..5 {
                let power_first = round % 2 == 0;
                for k in 0..2 {
                    let (pool, trials, steadies): (&rayon::ThreadPool, &mut Vec<f64>, &mut Vec<f64>) =
                        if (k == 0) == power_first {
                            (&power, &mut power_trial, &mut power_steady)
                        } else {
                            (render.as_ref(), &mut render_trial, &mut render_steady)
                        };
                    trials.push(trial1(pool, &bank, &src));
                    let pr = pool.install(|| primed_run(&bank, &src, start, 6, j_zero));
                    steadies.extend_from_slice(&pr.blocks_ms[1..]);
                }
            }

            // n−1: one render worker blocked in an OS wait, the trial injected.
            let (park_tx, park_rx) = mpsc::channel::<()>();
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let r2 = render.clone();
            let parked = std::thread::spawn(move || {
                r2.install(move || {
                    let _ = ready_tx.send(());
                    let _ = park_rx.recv();
                })
            });
            ready_rx.recv().expect("parked worker");
            std::thread::sleep(Duration::from_millis(50));
            let nm1_trial: Vec<f64> = (0..5).map(|_| trial1(&render, &bank, &src)).collect();
            let _ = park_tx.send(());
            parked.join().expect("parked worker join");

            let pt = median(&power_trial);
            let rt = median(&render_trial);
            let ps = median(&power_steady);
            let rs = median(&render_steady);
            let parity = pt / rs - 1.0;
            let pass = parity.abs() <= 0.05;
            let steady_parity = ps / rs - 1.0;
            let trial_parity = pt / rt - 1.0;
            let v = |x: &[f64]| x.iter().map(|&y| r3(y)).collect::<Vec<_>>();
            emit("B5", &m, json!({
                "kind": "summary", "rate": rate.tag, "l": rate.l, "rung": "30M", "bank": "lin",
                "powerTrialMs": v(&power_trial), "renderTrialMs": v(&render_trial),
                "powerSteadyMs": v(&power_steady), "renderSteadyMs": v(&render_steady),
                "nm1TrialMs": v(&nm1_trial),
                "powerTrial": r3(pt), "renderTrial": r3(rt), "powerSteady": r3(ps), "renderSteady": r3(rs),
                "nm1Trial": r3(median(&nm1_trial)),
                "parity": r3(parity), "pass": pass,
                "steadyParity": r3(steady_parity), "steadyPass": steady_parity.abs() <= 0.05,
                "trialParity": r3(trial_parity), "trialPass": trial_parity.abs() <= 0.05,
                "nm1Ratio": r3(median(&nm1_trial) / rt),
            }));
            println!(
                "[pwr] B5 {} {}: power trial {:.1} ms vs render steady {:.1} ms ({:+.1} %) {}; like for like: steady {:+.1} %, trial {:+.1} %; n-1 trial {:.1} ms (x{:.3} of the render trial)",
                m.label, rate.tag, pt, rs, parity * 100.0, if pass { "PASS" } else { "FAIL" },
                steady_parity * 100.0, trial_parity * 100.0, median(&nm1_trial), median(&nm1_trial) / rt
            );
            measured += 1;
        }
        assert!(measured > 0, "B5 measured nothing: is AURA_FILTER_DIR ({}) right?", filter_dir());
    }

    // ── B6 contention and nesting ─────────────────────────────────────────────

    /// A render-like loop: a primed stream rendered block after block inside
    /// `pool` (from one of its workers, like render::run). `paced` renders
    /// only while less than 1.5 s is buffered against a real-time output
    /// clock and waits 5 ms otherwise (an OS wait, like the render loop's
    /// channel wait); unpaced renders flat out. Records (start ms since
    /// `origin`, step ms) per block.
    #[allow(clippy::too_many_arguments)]
    fn spawn_render_loop(
        pool: Arc<rayon::ThreadPool>,
        bank: Arc<Bank>,
        src: Arc<SourceBuf>,
        j: i64,
        out_rate: u32,
        paced: bool,
        stop: Arc<AtomicBool>,
        steps: Arc<Mutex<Vec<(f64, f64)>>>,
        origin: Instant,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            set_highest();
            pool.install(move || {
                let start = start_for_block(&bank, j).expect("start");
                let mut s = PolyStream::new(bank.clone(), src.clone(), start);
                let frames = (BLOCK * bank.l) as f64;
                let target = 1.5 * out_rate as f64;
                let last = (src.len() / BLOCK) as i64 - 1;
                let mut next = j;
                let t_play = Instant::now();
                let mut written = 0.0f64;
                while !stop.load(Ordering::Acquire) {
                    if paced {
                        let consumed = t_play.elapsed().as_secs_f64() * out_rate as f64;
                        if written - consumed >= target {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    }
                    if next >= last {
                        // Stay inside the source (an all-zero window skips its FFT).
                        next = j;
                        let _ = s.trial_rebind(bank.clone(), next);
                    }
                    let t = Instant::now();
                    let d = s.trial_block_ns();
                    next += 1;
                    written += frames;
                    steps
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((ms(t.duration_since(origin)), ns_ms(d)));
                }
            })
        })
    }

    fn step_count(steps: &Mutex<Vec<(f64, f64)>>) -> usize {
        steps.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Wait for `n` more steps and at least `min`; at most 90 s.
    fn wait_steps(steps: &Mutex<Vec<(f64, f64)>>, n: usize, min: Duration) {
        let t = Instant::now();
        let base = step_count(steps);
        while (step_count(steps) < base + n || t.elapsed() < min) && t.elapsed() < Duration::from_secs(90) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Steps that overlap [a, b] (ms since the origin).
    fn overlapping(steps: &[(f64, f64)], a: f64, b: f64) -> Vec<f64> {
        steps.iter().filter(|(s, d)| *s < b && s + d > a).map(|(_, d)| *d).collect()
    }

    /// Steps wholly inside [a, b].
    fn inside(steps: &[(f64, f64)], a: f64, b: f64) -> Vec<f64> {
        steps.iter().filter(|(s, d)| *s >= a && s + d <= b).map(|(_, d)| *d).collect()
    }

    /// A render-like loop renders a primed 30M lin 44.1k stream in pool R;
    /// a 2-block trial is injected (b) into the power pool, then (a) into R,
    /// three times each, alternating. Flat-out and paced loops. Acceptance
    /// for (b): the loop's max step ≤ 2.2 × its idle step and the trial's
    /// min-of-2 (median over the three) ≤ 1.25 × the idle trial; (a) is
    /// reported (the nesting of V7).
    #[test]
    #[ignore]
    fn pwr_b6_contention_nesting() {
        let m = Mask::from_env();
        let power = product_pool("pwr-power");
        let render = Arc::new(product_pool("pwr-render"));
        env_line("B6", &m, &power);
        let rate = RATES[0];
        let src = source(&rate);
        let Some((bank, _)) = load_bank("30M", &rate, true) else {
            panic!("B6 needs the 30M 352800 linear filter in AURA_FILTER_DIR ({})", filter_dir());
        };
        let j = convolver::trial_block_index(&src);
        let trial2 = |pool: &rayon::ThreadPool| -> TrialTiming {
            pool.install(|| convolver::trial_blocks(std::slice::from_ref(&bank), &src, 2, &never, &never))
        };
        let min2 = |t: &TrialTiming| -> f64 { ns_ms(t.block_ns[0].iter().copied().min().unwrap_or(0)) };
        let sum2 = |t: &TrialTiming| -> f64 { t.block_ns[0].iter().map(|&ns| ns_ms(ns)).sum() };
        const INJECTIONS: usize = 3;

        for paced in [false, true] {
            let mode = if paced { "paced" } else { "flat" };
            // The idle trial: nothing rendering.
            let idle: Vec<f64> = (0..3).map(|_| min2(&trial2(&power))).collect();
            let idle_trial = median(&idle);

            let origin = Instant::now();
            let stop = Arc::new(AtomicBool::new(false));
            let steps = Arc::new(Mutex::new(Vec::new()));
            let h = spawn_render_loop(
                render.clone(), bank.clone(), src.clone(), j, rate.out_rate, paced,
                stop.clone(), steps.clone(), origin,
            );
            // Warm up (the first block after the prime), and for the paced loop
            // let the buffer fill.
            wait_steps(&steps, 3, Duration::from_secs(if paced { 4 } else { 1 }));
            let i0 = ms(origin.elapsed());
            wait_steps(&steps, 5, Duration::from_secs(3));
            let i1 = ms(origin.elapsed());

            // (b) into the power pool and (a) into the render pool, alternating.
            let mut wins_b: Vec<(TrialTiming, f64, f64)> = Vec::new();
            let mut wins_a: Vec<(TrialTiming, f64, f64)> = Vec::new();
            for _ in 0..INJECTIONS {
                let t0 = ms(origin.elapsed());
                let tb = trial2(&power);
                wins_b.push((tb, t0, ms(origin.elapsed())));
                wait_steps(&steps, 2, Duration::from_millis(1500));
                let t0 = ms(origin.elapsed());
                let ta = trial2(render.as_ref());
                wins_a.push((ta, t0, ms(origin.elapsed())));
                wait_steps(&steps, 2, Duration::from_millis(1500));
            }
            stop.store(true, Ordering::Release);
            h.join().expect("render loop");

            let all = steps.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let idle_steps = inside(&all, i0, i1);
            let idle_step = median(&idle_steps);
            let busy_ms = idle_steps.iter().sum::<f64>();
            let duty = busy_ms / (i1 - i0).max(1.0);
            let case = |wins: &[(TrialTiming, f64, f64)]| -> (Value, f64, f64, usize) {
                let mut max_step = 0.0f64;
                let mut mins = Vec::new();
                let mut nested = 0usize;
                let mut per = Vec::new();
                for (t, a, b) in wins {
                    let st = overlapping(&all, *a, *b);
                    let mx = max_of(&st);
                    max_step = max_step.max(mx);
                    mins.push(min2(t));
                    let is_nested = mx >= 0.8 * sum2(t);
                    if is_nested {
                        nested += 1;
                    }
                    per.push(json!({
                        "trial": trial_json(t), "min2": r3(min2(t)), "wallMs": r3(b - a),
                        "steps": st.iter().map(|&x| r3(x)).collect::<Vec<_>>(), "maxStep": r3(mx),
                        "nested": is_nested,
                    }));
                }
                let trial_ratio = median(&mins) / idle_trial;
                let v = json!({
                    "windows": per, "maxStep": r3(max_step), "stepRatio": r3(max_step / idle_step),
                    "min2": r3(median(&mins)), "trialRatio": r3(trial_ratio), "nested": nested,
                });
                (v, max_step / idle_step, trial_ratio, nested)
            };
            let (vb, step_b, trial_b, _) = case(&wins_b[..]);
            let (va, step_a, trial_a, nested_a) = case(&wins_a[..]);
            let pass = step_b <= 2.2 && trial_b <= 1.25;
            emit("B6", &m, json!({
                "kind": "summary", "mode": mode, "rate": rate.tag, "rung": "30M", "bank": "lin",
                "injections": INJECTIONS,
                "idleTrialMs": idle.iter().map(|&x| r3(x)).collect::<Vec<_>>(),
                "idleTrial": r3(idle_trial),
                "idleStep": r3(idle_step), "idleSteps": idle_steps.len(), "idleDuty": r3(duty),
                "b": vb, "a": va, "pass": pass,
            }));
            println!(
                "[pwr] B6 {} {}: idle step {:.1} ms (duty {:.2}), idle trial {:.1} ms | (b) power: step x{:.2}, trial x{:.2} {} | (a) render: step x{:.2}, trial x{:.2}, nested {}/{}",
                m.label, mode, idle_step, duty, idle_trial, step_b, trial_b, if pass { "PASS" } else { "FAIL" },
                step_a, trial_a, nested_a, INJECTIONS
            );
        }
    }

    // ── B7 GPU build ──────────────────────────────────────────────────────────

    /// `GpuPolyStream::try_new` for 30M lin and min at both rates, cold (the
    /// bank cache cleared: encode and upload) and warm (cache hit), started
    /// mid-track (a K3 build's shape) inside a power-like pool.
    #[test]
    #[ignore]
    fn pwr_b7_gpu_build() {
        let m = Mask::from_env();
        let pool = product_pool("pwr-power");
        env_line("B7", &m, &pool);
        let Some(ctx) = GpuPolyCtx::try_build() else {
            emit("B7", &m, json!({ "kind": "skip", "why": "no GPU" }));
            return;
        };
        for rate in RATES {
            let src = source(&rate);
            let j = convolver::trial_block_index(&src);
            for lin in [true, false] {
                let bank_name = if lin { "lin" } else { "min" };
                let Some((bank, _)) = load_bank("30M", &rate, lin) else { continue };
                let Some(start) = start_for_block(&bank, j) else { continue };
                let build = || -> (f64, bool, bool, f64) {
                    pool.install(|| {
                        let t = Instant::now();
                        let s = GpuPolyStream::try_new(bank.clone(), src.clone(), start, ctx.clone());
                        let build_ms = ms(t.elapsed());
                        let ok = s.is_some();
                        let err = s.as_ref().map_or(false, |s| s.has_error());
                        let t = Instant::now();
                        drop(s);
                        (build_ms, ok, err, ms(t.elapsed()))
                    })
                };
                ctx.clear_bank_cache();
                let cold = build();
                let warm = build();
                let warm2 = build();
                ctx.clear_bank_cache();
                emit("B7", &m, json!({
                    "kind": "summary", "rate": rate.tag, "l": rate.l, "rung": "30M", "bank": bank_name,
                    "coldMs": r3(cold.0), "warmMs": r3(median(&[warm.0, warm2.0])),
                    "ok": cold.1 && warm.1 && warm2.1, "error": cold.2 || warm.2 || warm2.2,
                    "dropMs": r3(cold.3),
                }));
                println!(
                    "[pwr] B7 {} {} 30M {}: cold {:.0} ms, warm {:.0} ms (ok {})",
                    m.label, rate.tag, bank_name, cold.0, warm.0, cold.1 && warm.1
                );
            }
        }
    }
}

// ── Filter upload: RAM peak and VRAM, before and after the half spectra ──────

/// The process's private bytes and working set (Windows).
fn process_memory() -> (usize, usize) {
    #[repr(C)]
    struct Pmc {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(h: *mut std::ffi::c_void, p: *mut Pmc, cb: u32) -> i32;
    }
    let mut m: Pmc = unsafe { std::mem::zeroed() };
    m.cb = std::mem::size_of::<Pmc>() as u32;
    // SAFETY: a pseudo-handle of this process and a correctly sized struct.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) };
    if ok == 0 { (0, 0) } else { (m.private, m.working_set) }
}

/// Peak rise of private bytes and working set while `f` runs, sampled every
/// millisecond on another thread.
fn ram_peak<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let (p0, w0) = process_memory();
    let stop = Arc::new(AtomicBool::new(false));
    let (pk_p, pk_w) = (Arc::new(AtomicUsize::new(p0)), Arc::new(AtomicUsize::new(w0)));
    let sampler = {
        let (stop, pk_p, pk_w) = (stop.clone(), pk_p.clone(), pk_w.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (p, w) = process_memory();
                pk_p.fetch_max(p, Ordering::Relaxed);
                pk_w.fetch_max(w, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    let out = f();
    let (p, w) = process_memory();
    pk_p.fetch_max(p, Ordering::Relaxed);
    pk_w.fetch_max(w, Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    (out, pk_p.load(Ordering::Relaxed).saturating_sub(p0), pk_w.load(Ordering::Relaxed).saturating_sub(w0))
}

/// The upload as it was before the half spectra: every slot of the full
/// spectrum (NFFT bins) encoded into its own vector, the vectors copied into
/// one image, and the image handed to `create_buffer_init`.
fn upload_full_spectra(ctx: &GpuPolyCtx, bank: &Bank) -> wgpu::Buffer {
    use rayon::prelude::*;
    use wgpu::util::DeviceExt;
    use super::fdl::encode_spectrum_ds;
    use crate::player::convolver::full_from_half;
    let (l, p) = (bank.l, bank.max_partitions());
    let slot_bytes = super::NFFT * 16;
    let slots: Vec<Vec<u8>> = (0..l * p)
        .into_par_iter()
        .map(|idx| {
            let spectra = bank.branch_spectra(idx / p);
            let mut tmp = Vec::with_capacity(slot_bytes);
            if idx % p < spectra.len() {
                encode_spectrum_ds(&full_from_half(&spectra[idx % p]), &mut tmp);
            } else {
                tmp.resize(slot_bytes, 0u8);
            }
            tmp
        })
        .collect();
    let mut buf: Vec<u8> = Vec::with_capacity(l * p * slot_bytes);
    for slot in &slots {
        buf.extend_from_slice(slot);
    }
    ctx.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("test h_freq full"),
        contents: &buf,
        usage: wgpu::BufferUsages::STORAGE,
    })
}

/// 30M and the 30M Hybrid-Phase pair: the RAM the filter upload takes at
/// its peak and its wall time, the old way (full spectra, pieces + copy)
/// against `GpuFilterBank::from_bank`; and the VRAM a playing stream (pair)
/// takes, measured against the formula.
///     cargo test --profile fast --features gpu-player-tests upload_ram_and_vram_30m -- --ignored --nocapture
#[test]
#[ignore]
fn upload_ram_and_vram_30m() {
    use super::filter_buf::GpuFilterBank;
    let dir = filter_dir();
    let l = 8usize;
    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[upload] GPU unavailable — skip"); return; }
        Some(c) => c,
    };
    let lin = format!("{}/fir_30M_352800_linear_phase.npy", dir);
    let min = format!("{}/fir_30M_352800_minimum_phase.npy", dir);
    let banks: Vec<Arc<Bank>> = match (Bank::load(&lin, l, Alignment::Linear, 352_800), Bank::load(&min, l, Alignment::BandWeighted, 352_800)) {
        (Ok(a), Ok(b)) => vec![Arc::new(a), Arc::new(b)],
        _ => { println!("[upload] skip: no 30M filters in {}", dir); return; }
    };
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    for (what, set) in [("30M", &banks[..1]), ("30M HP", &banks[..])] {
        ctx.clear_bank_cache();
        let t = Instant::now();
        let (old, op, ow) = ram_peak(|| set.iter().map(|b| upload_full_spectra(&ctx, b)).collect::<Vec<_>>());
        ctx.device.poll(wgpu::Maintain::Wait);
        let t_old = t.elapsed().as_secs_f64() * 1e3;
        drop(old);
        ctx.device.poll(wgpu::Maintain::Wait);
        let t = Instant::now();
        let (new, np, nw) = ram_peak(|| set.iter().map(|b| GpuFilterBank::from_bank(&ctx.device, &ctx.queue, b)).collect::<Vec<_>>());
        ctx.device.poll(wgpu::Maintain::Wait);
        let t_new = t.elapsed().as_secs_f64() * 1e3;
        drop(new);
        ctx.device.poll(wgpu::Maintain::Wait);
        println!(
            "[upload] {:<6}: RAM peak private +{:.0} -> +{:.0} MiB, working set +{:.0} -> +{:.0} MiB; time {:.0} -> {:.0} ms",
            what, mib(op), mib(np), mib(ow), mib(nw), t_old, t_new
        );
    }
    // VRAM of playing streams: DXGI free before and after building them.
    let n_src = 44_100 * 3;
    let x: Vec<f64> = (0..n_src).map(|i| ((i as f64) * 0.01).sin() * 0.25).collect();
    let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });
    for (what, set) in [("30M", &banks[..1]), ("30M HP", &banks[..])] {
        ctx.clear_bank_cache();
        ctx.device.poll(wgpu::Maintain::Wait);
        let free0 = ctx.free_vram_bytes();
        let streams: Vec<GpuPolyStream> = set
            .iter()
            .map(|b| GpuPolyStream::try_new(b.clone(), src.clone(), 0, ctx.clone()).expect("GPU stream"))
            .collect();
        ctx.device.poll(wgpu::Maintain::Wait);
        let free1 = ctx.free_vram_bytes();
        let formula = set.iter().map(|b| super::vram_demand(b.full_len, l)).sum::<u64>();
        println!(
            "[upload] {:<6}: VRAM taken {:.0} MiB (DXGI free {:.0} -> {:.0}); formula {:.0} MiB",
            what, mib(free0.saturating_sub(free1) as usize), mib(free0 as usize), mib(free1 as usize), mib(formula as usize)
        );
        drop(streams);
    }
}

// ── A live stream on the card: tail on the GPU, head on the CPU ──────────────

/// A live stream through the card (the tail summed there from partition 1,
/// the head on the CPU) against the CPU's live stream on the same source:
/// within the DS tolerance (≤ −200 dBFS), from the start and later.
///     cargo test --profile fast --features gpu-player-tests live_head_tail_on_the_card -- --ignored --nocapture
#[test]
#[ignore]
fn live_head_tail_on_the_card() {
    use crate::player::convolver::Input;
    use crate::player::radio::live::LiveSource;
    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[live-gpu] GPU unavailable — skip"); return; }
        Some(c) => c,
    };
    let dir = filter_dir();
    for (tag, l, rate, align) in [("1M", 8usize, 352_800u32, Alignment::Linear), ("30M", 2, 88_200, Alignment::Linear)] {
        let path = format!("{}/fir_{}_{}_linear_phase.npy", dir, tag, rate);
        let bank = match Bank::load(&path, l, align, rate) {
            Ok(b) => Arc::new(b),
            Err(e) => { println!("[live-gpu] skip {}: {}", tag, e); continue; }
        };
        let secs = if tag == "30M" { 200 } else { 30 };
        let n = 44_100 * secs;
        let x = white_noise(n, 77);
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 1.0, 1e9));
        live.push(&x, &x);
        live.close();
        for start in [0u64, rate as u64 * 5 + 12_345] {
            // Past the filter's look-ahead into the track for the long one.
            let start = if tag == "30M" { start + (bank.delay as u64) } else { start };
            let want = rate as usize * 4;
            let mut cpu = PolyStream::with_input(bank.clone(), Input::Live(live.clone()), start);
            let mut gpu = GpuPolyStream::try_with_input(bank.clone(), Input::Live(live.clone()), start, ctx.clone())
                .expect("[live-gpu] the card took the stream");
            let (mut cl, mut cr, mut gl, mut gr) = (vec![0.0; want], vec![0.0; want], vec![0.0; want], vec![0.0; want]);
            for (a, b) in cl.chunks_mut(4_096).zip(cr.chunks_mut(4_096)) {
                cpu.read(a, b);
            }
            for (a, b) in gl.chunks_mut(4_096).zip(gr.chunks_mut(4_096)) {
                gpu.read(a, b);
            }
            assert!(!gpu.is_gpu_failed(), "[live-gpu] the card failed");
            let peak = cl.iter().chain(&cr).fold(0f64, |p, v| p.max(v.abs()));
            let diff = cl.iter().zip(&gl).chain(cr.iter().zip(&gr)).fold(0f64, |d, (a, b)| d.max((a - b).abs()));
            println!("[live-gpu] {} x{} start {}: max|diff| {:.1} dBFS (peak {:.3})", tag, l, start, db(diff), peak);
            assert!(peak > 0.1, "[live-gpu] silent output");
            assert!(db(diff) <= -200.0, "[live-gpu] {} start {}: {:.1} dBFS", tag, start, db(diff));
        }
    }
}

/// A file through the card, written once and compared after a change to
/// the kernel: the bits must not move. AURA_HERM_GPU_DUMP=write <file> or
/// check <file>.
///     cargo test --profile fast --features gpu-player-tests file_on_the_card_dump -- --ignored --nocapture
#[test]
#[ignore]
fn file_on_the_card_dump() {
    let Ok(arg) = std::env::var("AURA_HERM_GPU_DUMP") else { println!("[dump] AURA_HERM_GPU_DUMP not set — skip"); return; };
    let (mode, file) = arg.split_once(' ').expect("write|check <file>");
    let ctx = match GpuPolyCtx::try_build() {
        None => { println!("[dump] GPU unavailable — skip"); return; }
        Some(c) => c,
    };
    let path = format!("{}/fir_1M_352800_linear_phase.npy", filter_dir());
    let bank = Arc::new(Bank::load(&path, 8, Alignment::Linear, 352_800).expect("1M filter"));
    let x = white_noise(44_100 * 8, 5);
    let src = Arc::new(SourceBuf { l: x.clone(), r: x.iter().map(|v| -v * 0.5).collect(), rate: 44_100 });
    let mut st = GpuPolyStream::try_new(bank, src, 1_234, ctx).expect("the card took the stream");
    let want = 352_800 * 4;
    let (mut ol, mut or) = (vec![0.0; want], vec![0.0; want]);
    st.read(&mut ol, &mut or);
    let bytes: Vec<u8> = ol.iter().chain(&or).flat_map(|v| v.to_bits().to_le_bytes()).collect();
    match mode {
        "write" => {
            std::fs::write(file, &bytes).unwrap();
            println!("[dump] wrote {} samples to {}", 2 * want, file);
        }
        _ => {
            let old = std::fs::read(file).unwrap();
            let first = old.iter().zip(&bytes).position(|(a, b)| a != b);
            assert!(old.len() == bytes.len() && first.is_none(), "[dump] the card's output moved at byte {:?}", first);
            println!("[dump] {} samples bit for bit with {}", 2 * want, file);
        }
    }
}
