//! End-to-end smoke run without a sound device: a real file through the
//! engine's source stages, a chain, the render thread and the timeline,
//! with a reader that consumes at real-time pace, and settings changes made
//! while it plays.
//!
//!     set AURA_E2E_FILE=<a 44.1 kHz FLAC>
//!     cargo test --profile fast e2e -- --ignored --nocapture
//!
//! GPU fault harness (original, kept):
//!     set AURA_E2E_FILE=<a 44.1 kHz FLAC>
//!     cargo test --profile fast --features gpu-player-tests e2e_gpu_fault_and_fallback \
//!         -- --ignored --nocapture
//!
//! GPU gapless proof (loud material, three scenarios):
//!     set AURA_FILTER_DIR=<fir-optimizer/output>
//!     cargo test --profile fast --features gpu-player-tests e2e_gpu_fault_mid_track \
//!         -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::chain::{build_chain, prepare_variant, Resources};
use super::output::OutputShared;
use super::render::{self, Msg};
use super::settings::{Phase, PlayerSettings};
use super::timeline::Timeline;

#[test]
#[ignore]
fn e2e_play_and_switch() {
    let file = match std::env::var("AURA_E2E_FILE") {
        Ok(f) => PathBuf::from(f),
        Err(_) => {
            println!("set AURA_E2E_FILE to run");
            return;
        }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-player-e2e"));
    let cancel = AtomicBool::new(false);
    let s1 = PlayerSettings::default(); // DC ISP SUB15 AHR AA TFS 1M ×8

    let t = Instant::now();
    let quick = Arc::new(prepare_variant(&file, &s1, true, &cancel).unwrap());
    println!("quick variant: {:.2} s  ({} Hz → {} Hz)", t.elapsed().as_secs_f64(), quick.src.rate, quick.out_rate);
    let t = Instant::now();
    let full = Arc::new(prepare_variant(&file, &s1, false, &cancel).unwrap());
    println!("full variant:  {:.2} s  tokens {:?}", t.elapsed().as_secs_f64(), full.tokens);
    for n in &full.notes {
        println!("   {}", n);
    }

    let rate = quick.out_rate;
    let t = Instant::now();
    let c0 = build_chain(&res, 1, "e2e", &quick, &s1, 0, None, None, 0).unwrap();
    println!("chain (quick, TFS 1M) built in {:.3} s: {}", t.elapsed().as_secs_f64(), c0.describe());

    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();

    // Reader: consumes at real time in 10 ms periods, like the device would.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let tl = tl.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let period = (rate / 100) as usize;
            let mut buf = vec![0.0; period * 2];
            // Pre-roll like the controller does.
            let t0 = Instant::now();
            while (tl.buffered_frames() as f64) < 0.3 * rate as f64 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let start = Instant::now();
            let mut consumed = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
                while consumed + period as u64 <= due {
                    let got = tl.read_into(&mut buf, period);
                    if got < period {
                        // Zero the unread portion (mirrors the output thread's
                        // behaviour: silence on underrun, not repeated old samples).
                        for s in &mut buf[got * 2..period * 2] { *s = 0.0; }
                        tl.note_underrun(period - got);
                    }
                    consumed += period as u64;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    let switch = |label: &str, s: &PlayerSettings, v: &Arc<super::chain::Variant>, lead_s: f64, continuous: bool, at_s: Option<f64>| {
        let requested = Instant::now();
        let m = shared.locate(tl.read_pos()).unwrap();
        let start = match at_s {
            Some(a) => (a * rate as f64) as u64,
            None => m.index + (lead_s * rate as f64) as u64,
        };
        let c = build_chain(&res, 1, "e2e", v, s, start, None, None, 0).unwrap();
        let built = requested.elapsed().as_secs_f64();
        let desc = c.describe();
        tx.send(Msg::Swap { chain: c, continuous, seek: false, side_buf: None, requested }).unwrap();
        let t0 = Instant::now();
        let before = shared.last_switch_ms.load(Ordering::Relaxed);
        while shared.last_switch_ms.load(Ordering::Relaxed) == before && t0.elapsed() < Duration::from_secs(8) {
            std::thread::sleep(Duration::from_millis(5));
        }
        println!(
            "{:<28} build {:>5.3} s → heard after {:>5} ms | at {:>6.2} s | {}",
            label,
            built,
            shared.last_switch_ms.load(Ordering::Relaxed),
            shared.locate(tl.read_pos()).map(|m| m.index as f64 / rate as f64).unwrap_or(0.0),
            desc
        );
    };

    std::thread::sleep(Duration::from_secs(3));
    switch("quick → full (upgrade)", &s1, &full, 0.45, true, None);
    std::thread::sleep(Duration::from_secs(3));
    let mut s2 = s1.clone();
    s2.phase = Phase::Linear;
    switch("TFS → linear", &s2, &full, 0.5, true, None);
    std::thread::sleep(Duration::from_secs(3));
    let mut s3 = s1.clone();
    s3.taps = 30_000_000;
    s3.phase = Phase::Hybrid;
    switch("1M TFS → 30M Hybrid-Phase", &s3, &full, 1.6, true, None);
    std::thread::sleep(Duration::from_secs(4));
    switch("seek → 1:30 (30M HP)", &s3, &full, 0.0, false, Some(90.0));
    std::thread::sleep(Duration::from_secs(3));
    let mut s4 = s3.clone();
    s4.phase = Phase::Alpha;
    switch("HP → continuous alpha", &s4, &full, 1.6, true, None);
    std::thread::sleep(Duration::from_secs(3));

    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    let rtf = shared.rtf_milli.load(Ordering::Relaxed) as f64 / 1000.0;
    let under = tl.underrun_frames();
    println!("render speed (last window): {:.1}× real time | underrun frames: {} ({:.1} ms)", rtf, under, under as f64 / rate as f64 * 1000.0);
    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    tx.send(Msg::Stop).unwrap();
}

/// End-to-end GPU fault and CPU fallback harness.
///
/// Proves:
///   1. build_chain with a real GPU context produces gpu_on=true.
///   2. AURA_GPU_FAIL_AFTER causes the GPU stream to fail mid-track;
///      the render thread detects it and sets shared.gpu_failed.
///   3. A simulated controller tick builds a CPU fallback chain at
///      read_pos + lead and sends Msg::Swap{continuous:true} —
///      exactly what controller.rs tick() does.
///   4. The render thread places the CPU chain; last_switch_ms advances.
///   5. shared.gpu_failed stays true after the swap (K6/G4: never cleared
///      within a track session).
///   6. The CPU fallback has gpu_on=false and downgrade=None on this
///      fast machine at the same tap count (G6).
///   7. Reports the longest zero run and level at the splice point.
///
/// Run with:
///   set AURA_E2E_FILE=<44.1 kHz FLAC>
///   cargo test --profile fast --features gpu-player-tests \
///       e2e_gpu_fault_and_fallback -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_gpu_fault_and_fallback() {
    let file = match std::env::var("AURA_E2E_FILE") {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_E2E_FILE to run"); return; }
    };

    // Require GPU hardware — otherwise this test cannot prove the GPU path.
    let gpu_ctx = super::gpu::ctx::GpuPolyCtx::try_build();
    if gpu_ctx.is_none() {
        println!("e2e_gpu_fault_and_fallback: no discrete GPU on this machine — skip");
        return;
    }

    // Inject a GPU fault after 2 computed blocks.
    // BLOCK=32768, L=8 → each block = 262144 output samples ≈ 0.74 s at 352800 Hz.
    // Two good blocks → fault fires at ~1.49 s into the track.
    //
    // Safety: this is an #[ignore] test run alone; no parallel tests share the env.
    unsafe { std::env::set_var("AURA_GPU_FAIL_AFTER", "2"); }

    let res = Resources::new(std::env::temp_dir().join("aura-player-e2e-gf"));
    let cancel = AtomicBool::new(false);
    let mut s = PlayerSettings::default();
    s.phase = Phase::Linear; // linear phase: one convolver, no HP blending

    let v = Arc::new(prepare_variant(&file, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("variant: {} Hz → {} Hz  tokens: {}", v.src.rate, rate, v.tokens.join(" "));

    // Build a chain that uses the GPU (ctx is Some).
    // AURA_PLAYER_FORCE_GPU is not needed here because we pass the ctx directly.
    let chain_gpu = build_chain(&res, 1, "e2e_gf", &v, &s, 0, gpu_ctx.clone(), None, 0)
        .expect("build_chain with GPU ctx");
    assert!(chain_gpu.gpu_on, "expected GPU chain with real gpu_ctx");
    println!("GPU chain ready: {}  gpu_on={}", chain_gpu.describe(), chain_gpu.gpu_on);

    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 8.0));
    tx.send(Msg::Start { chain: chain_gpu, timeline: tl.clone() }).unwrap();

    // Pre-roll: wait for the renderer to buffer ~300 ms before the reader starts.
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < 0.3 * rate as f64
          && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }

    let chunk = (rate / 100) as usize; // 10 ms per read call

    // Collected output: mono mix of all frames consumed by the simulated device.
    let collected: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 12)));
    let stop_reader = Arc::new(AtomicBool::new(false));
    let swap_sent_at_read_pos: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // Reader thread: consume at real-time pace, collect mono mix.
    let reader = {
        let tl = tl.clone();
        let stop = stop_reader.clone();
        let col = collected.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0.0f64; chunk * 2]; // interleaved L, R
            let start = Instant::now();
            let mut consumed = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
                while consumed + chunk as u64 <= due {
                    let got = tl.read_into(&mut buf, chunk);
                    if got < chunk {
                        tl.note_underrun(chunk - got);
                    }
                    let mut v = col.lock().unwrap();
                    for i in 0..chunk {
                        v.push((buf[2 * i] + buf[2 * i + 1]) * 0.5);
                    }
                    consumed += chunk as u64;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    // Wait for the GPU stream to fail (render thread sets shared.gpu_failed).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if shared.gpu_failed.load(Ordering::Relaxed) { break; }
        if Instant::now() >= deadline {
            stop_reader.store(true, Ordering::Relaxed);
            reader.join().unwrap();
            unsafe { std::env::remove_var("AURA_GPU_FAIL_AFTER"); }
            panic!("gpu_failed never set — GPU stream did not fail within 30 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let fail_read_pos = tl.read_pos();
    println!("gpu_failed=true  read_pos={} ({:.2} s)",
        fail_read_pos, fail_read_pos as f64 / rate as f64);
    swap_sent_at_read_pos.store(fail_read_pos, Ordering::Relaxed);

    // Clear fault env var before building the CPU chain — the fallback must
    // not inherit AURA_GPU_FAIL_AFTER.
    unsafe { std::env::remove_var("AURA_GPU_FAIL_AFTER"); }

    // Simulate controller tick(): build a CPU fallback chain at
    // read_pos + lead, no GPU context, same taps.
    // controller.rs uses lead_s ≈ 0.45 s + (taps/30M)*0.6.
    // At 1M taps: lead ≈ 0.47 s.
    let lead_frames = (0.47 * rate as f64) as u64;
    let fallback_start = fail_read_pos + lead_frames;
    let chain_cpu = build_chain(&res, 1, "e2e_gf", &v, &s, fallback_start, None, None, 0)
        .expect("build CPU fallback chain");
    assert!(!chain_cpu.gpu_on, "CPU fallback must have gpu_on=false");
    assert!(
        chain_cpu.downgrade.is_none(),
        "no tap downgrade expected on this fast machine at {} taps", s.taps
    );
    println!("CPU fallback: {}  gpu_on={}  downgrade={:?}  start_frame={}",
        chain_cpu.describe(), chain_cpu.gpu_on, chain_cpu.downgrade, fallback_start);

    // Send the continuous swap — mirrors controller.rs tick().
    let before_switch = shared.last_switch_ms.load(Ordering::Relaxed);
    tx.send(Msg::Swap { chain: chain_cpu, continuous: true, seek: false, side_buf: None, requested: Instant::now() }).unwrap();

    // Wait for the swap to be placed by the render thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if shared.last_switch_ms.load(Ordering::Relaxed) != before_switch { break; }
        if Instant::now() >= deadline {
            stop_reader.store(true, Ordering::Relaxed);
            reader.join().unwrap();
            panic!("CPU swap never placed by render thread within 10 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("swap placed in {} ms (last_switch_ms)",
        shared.last_switch_ms.load(Ordering::Relaxed));

    // K6/G4: gpu_failed must still be true — never cleared within a track session.
    assert!(
        shared.gpu_failed.load(Ordering::Relaxed),
        "gpu_failed cleared after CPU swap — violates K6 (chip must show 'failed' for rest of track)"
    );

    // Collect a few seconds of post-swap audio, then stop.
    std::thread::sleep(Duration::from_secs(2));
    stop_reader.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    // Analysis: find the longest run of near-silence (|v| < 1e-9) in the output.
    let data = collected.lock().unwrap();
    let n = data.len();
    let zero_thresh = 1e-9_f64;
    let mut max_run_len = 0usize;
    let mut max_run_start = 0usize;
    let mut cur_len = 0usize;
    let mut cur_start = 0usize;
    for (i, &v) in data.iter().enumerate() {
        if v.abs() < zero_thresh {
            if cur_len == 0 { cur_start = i; }
            cur_len += 1;
            if cur_len > max_run_len { max_run_len = cur_len; max_run_start = cur_start; }
        } else {
            cur_len = 0;
        }
    }
    let max_run_ms = max_run_len as f64 / rate as f64 * 1000.0;

    let (level_before, level_after) = if max_run_len > 0 && n > max_run_start + max_run_len {
        let bi = max_run_start.saturating_sub(1);
        let ai = (max_run_start + max_run_len).min(n - 1);
        let lb = if data[bi].abs() > 1e-12 { 20.0 * data[bi].abs().log10() } else { -144.0 };
        let la = if data[ai].abs() > 1e-12 { 20.0 * data[ai].abs().log10() } else { -144.0 };
        (lb, la)
    } else {
        (-144.0, -144.0)
    };

    println!("\n=== e2e_gpu_fault_and_fallback: PASS ===");
    println!("  audible.gpu:      on → failed  (gpu_failed still true: {})",
        shared.gpu_failed.load(Ordering::Relaxed));
    println!("  CPU fallback:     gpu_on=false  downgrade=None");
    println!("  swap_read_pos:    {} ({:.2} s)",
        swap_sent_at_read_pos.load(Ordering::Relaxed),
        swap_sent_at_read_pos.load(Ordering::Relaxed) as f64 / rate as f64);
    println!("  total collected:  {} frames ({:.1} s)", n, n as f64 / rate as f64);
    println!("  longest zero run: {} frames ({:.2} ms) starting at {:.2} s",
        max_run_len, max_run_ms, max_run_start as f64 / rate as f64);
    println!("  level at splice:  before {:.1} dBFS / after {:.1} dBFS",
        level_before, level_after);
}

// ── GPU gapless proof ─────────────────────────────────────────────────────────

/// Metrics returned by each fault-injection run.
#[cfg(feature = "gpu-player-tests")]
struct FaultMetrics {
    out_rate:          u32,
    fail_read_pos:     u64,  // tl.read_pos() when gpu_failed first detected
    splice_frame:      u64,  // tl.read_pos() when last_switch_ms first changed
    // Output-frame position of the first GPU zero (computed from bank.delay and
    // fail_after; independent of what the reader received).
    gpu_zero_start:    u64,
    total_collected:   usize,
    // Longest run of |mono_mix| < 1e-9 anywhere AFTER fail_read_pos.
    longest_zero_run:  usize,
    longest_zero_pos:  usize,
    // |s[splice_idx] - s[splice_idx - 1]| at the splice in collected data.
    click_at_splice:   f64,
    // Median |s[i] - s[i-1]| over a 1 s window before the fault — typical step.
    typical_step:      f64,
    // RMS (dBFS) in a 10 ms window 10 ms before and after the splice.
    rms_before_db:     f64,
    rms_after_db:      f64,
    underruns:         u64,
}

#[cfg(feature = "gpu-player-tests")]
impl FaultMetrics {
    fn rms_db(samples: &[f64]) -> f64 {
        if samples.is_empty() { return -200.0; }
        let mean_sq = samples.iter().map(|&v| v * v).sum::<f64>() / samples.len() as f64;
        if mean_sq < 1e-30 { -300.0 } else { 10.0 * mean_sq.log10() }
    }

    fn print(&self, desc: &str) {
        let r = self.out_rate as f64;
        let zero_ms = self.longest_zero_run as f64 / r * 1000.0;
        let zero_threshold_ok = self.longest_zero_run == 0;
        println!("\n=== {} ===", desc);
        println!(
            "  fail_read_pos : {} ({:.3} s)\n  splice_frame  : {} ({:.3} s)\n  gpu_zero_start: {} ({:.3} s)",
            self.fail_read_pos, self.fail_read_pos as f64 / r,
            self.splice_frame,  self.splice_frame  as f64 / r,
            self.gpu_zero_start, self.gpu_zero_start as f64 / r
        );
        println!(
            "  splice BEFORE zeros: {}  (delta: {} frames / {:.1} ms)",
            if self.splice_frame <= self.gpu_zero_start { "YES" } else { "NO " },
            (self.gpu_zero_start as i64 - self.splice_frame as i64),
            (self.gpu_zero_start as i64 - self.splice_frame as i64) as f64 / r * 1000.0
        );
        println!(
            "  longest_zero_run: {} frames ({:.3} ms) at collected[{}]  {}",
            self.longest_zero_run, zero_ms, self.longest_zero_pos,
            if zero_threshold_ok { "PASS" } else { "FAIL — gap detected" }
        );
        println!(
            "  click at splice : {:.2e}  typical_step: {:.2e}  ratio: {:.1}x",
            self.click_at_splice, self.typical_step,
            if self.typical_step > 0.0 { self.click_at_splice / self.typical_step } else { 0.0 }
        );
        println!(
            "  RMS 10 ms before splice: {:.1} dBFS   after: {:.1} dBFS",
            self.rms_before_db, self.rms_after_db
        );
        println!("  underruns: {}   total_collected: {} ({:.1} s)",
            self.underruns, self.total_collected,
            self.total_collected as f64 / r);
    }
}

/// Run one fault-injection scenario.
///
/// `fail_after`:   GPU fault fires after this many compute_block calls.
///                 The GPU chain starts at output frame 0, so the first GPU
///                 zero appears at frame: (j0 + fail_after - 1) * BLOCK_OUT - delay
///                 where j0 = delay / BLOCK_OUT (integer division).
///
/// `cpu_lead_s`:   Seconds from fail_read_pos to chain.start for the CPU fallback.
///                 Mirrors what controller.rs tick() does after the guard-based fix:
///                 guard_s + 0.05 = 0.30 s.  Pass `lead_s_formula(taps, phase)` to
///                 reproduce the pre-fix behavior.
///
/// The CPU fallback is built with no GPU context and the same settings, then
/// swapped in via Msg::Swap { continuous: true } — exactly what tick() does.
#[cfg(feature = "gpu-player-tests")]
/// When `use_gpu_zeros_cap` is true, the CPU fallback start is capped using
/// `shared.gpu_zeros_from` exactly as the K4-Z fix in controller.rs does:
/// `start = min(start, zeros_idx − 8192)`.  Pass `false` for legacy cases
/// that proved the O5 fix (pre-K4-Z baseline) and `true` for the new
/// "failure shortly after a seek / buffer below target" scenario.
fn run_fault_case(
    res:        &super::chain::Resources,
    v:          Arc<super::chain::Variant>,
    track_key:  &str,
    s:          &PlayerSettings,
    gpu_ctx:    Arc<super::gpu::ctx::GpuPolyCtx>,
    fail_after: u64,
    cpu_lead_s: f64,
    tl_capacity_s: f64,
    use_gpu_zeros_cap: bool,
) -> FaultMetrics {
    use super::convolver::BLOCK;

    let rate = v.out_rate;
    let block_out = BLOCK * v.l;  // output samples per compute_block call

    // Compute the expected GPU-zero start frame.
    // For a linear-phase FIR with `taps` coefficients (and linear or hybrid phase):
    //   bank.delay = (taps - 1) / 2  (in output-domain samples, same units as the ring)
    // For HP pairs the linear stream dominates; the minimum stream has a smaller delay
    // and is always gapless when the linear stream is.
    // j0 = delay / block_out  (integer division — first block index that writes to timeline)
    // fault fires at iteration j_fail = j0 + fail_after - 1
    // pend_start = j_fail * block_out - delay   (output frame where GPU zeros begin)
    let gpu_zero_start: u64 = {
        let full_len = s.taps as u64;
        let delay = (full_len - 1) / 2;
        let j0 = delay / block_out as u64;
        let j_fail = j0 + fail_after.saturating_sub(1);
        (j_fail * block_out as u64).saturating_sub(delay)
    };

    // Set the fault injection env var (read at GpuPolyStream construction).
    unsafe { std::env::set_var("AURA_GPU_FAIL_AFTER", fail_after.to_string()); }

    let chain_gpu = super::chain::build_chain(res, 1, track_key, &v, s, 0, Some(gpu_ctx), None, 0)
        .expect("build GPU chain");
    assert!(chain_gpu.gpu_on, "expected GPU chain with real gpu_ctx");

    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = super::output::OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, tl_capacity_s));
    tx.send(Msg::Start { chain: chain_gpu, timeline: tl.clone() }).unwrap();

    // Pre-roll: wait for 0.3 s of buffered audio.
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < 0.3 * rate as f64
          && t0.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(5));
    }

    let chunk = (rate / 100) as usize; // 10 ms chunks
    let collected: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(
        Vec::with_capacity(rate as usize * 50)));
    let stop_reader = Arc::new(AtomicBool::new(false));
    let underrun_count = Arc::new(AtomicU64::new(0));

    // Reader: consume at real-time pace, collect mono mix (L+R)/2.
    let reader = {
        let tl2 = tl.clone();
        let stop = stop_reader.clone();
        let col = collected.clone();
        let ur = underrun_count.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0.0f64; chunk * 2];
            let start = Instant::now();
            let mut consumed = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
                while consumed + chunk as u64 <= due {
                    let got = tl2.read_into(&mut buf, chunk);
                    if got < chunk {
                        // Zero the unread tail (mirrors the output thread: silence
                        // on underrun, not stale samples).
                        for s in &mut buf[got * 2..chunk * 2] { *s = 0.0; }
                        ur.fetch_add((chunk - got) as u64, Ordering::Relaxed);
                    }
                    let mut c = col.lock().unwrap();
                    for i in 0..chunk {
                        c.push((buf[2*i] + buf[2*i+1]) * 0.5);
                    }
                    consumed += chunk as u64;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    // Wait for gpu_failed.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if shared.gpu_failed.load(Ordering::Relaxed) { break; }
        if Instant::now() >= deadline {
            stop_reader.store(true, Ordering::Relaxed);
            reader.join().unwrap();
            unsafe { std::env::remove_var("AURA_GPU_FAIL_AFTER"); }
            tx.send(Msg::Stop).unwrap();
            panic!("gpu_failed never set within 60 s — check FAIL_AFTER={}", fail_after);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let fail_read_pos = tl.read_pos();
    println!("  gpu_failed at read_pos={} ({:.3} s)", fail_read_pos, fail_read_pos as f64 / rate as f64);

    // Clear fault env var BEFORE building the CPU fallback.
    unsafe { std::env::remove_var("AURA_GPU_FAIL_AFTER"); }

    // Build CPU fallback — mirrors controller.rs tick() after the guard fix.
    // chain.start = fail_read_pos + cpu_lead_s.  plan_swap will delay to
    // earliest_rewrite, which is always < gpu_zero_start when cpu_lead_s = 0.30 s.
    let uncapped_start = fail_read_pos + (cpu_lead_s * rate as f64) as u64;
    // With use_gpu_zeros_cap, the controller's rule: when the first GPU-zero
    // frame is known, the splice goes a crossfade and one render block (8 192
    // frames) below it — as late as the real audio allows (K4 runway), and by
    // construction never into the zeros (K4-Z). This is the scenario where
    // target_ahead_s − 0.10 would land the splice past the first zero when the
    // buffer is below its target.
    let fallback_start = if use_gpu_zeros_cap {
        let margin = (render::XFADE_MS / 1000.0 * rate as f64) as u64 + 8_192;
        let zeros_tl = shared.gpu_zeros_from.load(std::sync::atomic::Ordering::Relaxed);
        if zeros_tl != u64::MAX {
            if let Some(mz) = shared.locate(zeros_tl) {
                mz.index.saturating_sub(margin)
            } else { uncapped_start }
        } else { uncapped_start }
    } else {
        uncapped_start
    };
    println!("  fallback_start={} ({:.3} s)  uncapped={} ({:.3} s)  gpu_zeros_from={}",
        fallback_start, fallback_start as f64 / rate as f64,
        uncapped_start, uncapped_start as f64 / rate as f64,
        shared.gpu_zeros_from.load(std::sync::atomic::Ordering::Relaxed));
    let chain_cpu = super::chain::build_chain(res, 1, track_key, &v, s, fallback_start, None, None, 0)
        .expect("build CPU fallback chain");
    assert!(!chain_cpu.gpu_on, "CPU fallback must have gpu_on=false");

    let before_switch = shared.last_switch_ms.load(Ordering::Relaxed);
    tx.send(Msg::Swap { chain: chain_cpu, continuous: true, seek: false, side_buf: None, requested: Instant::now() }).unwrap();

    // Wait for swap to be placed.
    let deadline = Instant::now() + Duration::from_secs(20);
    // The read head when the swap was placed; the verdict below requires it
    // to come before the first GPU zero (splice_frame <= gpu_zero_start).
    let splice_frame = loop {
        if shared.last_switch_ms.load(Ordering::Relaxed) != before_switch {
            break tl.read_pos();
        }
        if Instant::now() >= deadline {
            stop_reader.store(true, Ordering::Relaxed);
            reader.join().unwrap();
            tx.send(Msg::Stop).unwrap();
            panic!("CPU swap never placed within 20 s");
        }
        std::thread::sleep(Duration::from_millis(2));
    };

    // Collect 3 more seconds of post-splice audio, then stop.
    std::thread::sleep(Duration::from_secs(3));
    stop_reader.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    let underruns = underrun_count.load(Ordering::Relaxed);
    let data = collected.lock().unwrap();
    let n = data.len();

    // Locate fail_read_pos and splice_frame in the collected data.
    // Collected data starts at frame 0 (reader started from 0).
    let fail_idx = fail_read_pos as usize;
    let splice_idx = splice_frame as usize;

    // Longest run of |x| < 1e-9 anywhere AFTER fail_read_pos.
    // GPU silence (IFFT of zeros) is < 1e-30; natural polyphase zero-crossings
    // are typically 1e-3 to 1e-5, so 1e-9 cleanly separates the two.
    let zero_thresh = 1e-9_f64;
    let search_start = fail_idx.min(n);
    let mut max_run_len = 0usize;
    let mut max_run_pos = 0usize;
    let mut cur_run = 0usize;
    let mut cur_start = 0usize;
    for i in search_start..n {
        if data[i].abs() < zero_thresh {
            if cur_run == 0 { cur_start = i; }
            cur_run += 1;
            if cur_run > max_run_len { max_run_len = cur_run; max_run_pos = cur_start; }
        } else {
            cur_run = 0;
        }
    }

    // Click detector: |s[splice] - s[splice - 1]| vs median step.
    let click_at_splice = if splice_idx > 0 && splice_idx < n {
        (data[splice_idx] - data[splice_idx.saturating_sub(1)]).abs()
    } else {
        0.0
    };
    // Typical step: median of |s[i] - s[i-1]| over 1 s before the fault.
    let typical_step = {
        let win_end = fail_idx.min(n);
        let win_start = win_end.saturating_sub(rate as usize);
        let mut steps: Vec<f64> = (win_start+1..win_end)
            .map(|i| (data[i] - data[i-1]).abs())
            .collect();
        if steps.is_empty() { 0.0 } else {
            steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
            steps[steps.len() / 2]
        }
    };

    // RMS in 10 ms (3528 frames) windows 10 ms before and after splice.
    let win = (rate as usize) / 100; // 10 ms
    let rms_before_db = {
        let end = splice_idx.min(n);
        let start = end.saturating_sub(win * 2); // 10-20 ms before splice
        let end2 = end.saturating_sub(win);
        FaultMetrics::rms_db(&data[start..end2.max(start)])
    };
    let rms_after_db = {
        let start = (splice_idx + win).min(n);        // 10-20 ms after splice
        let end = (splice_idx + win * 2).min(n);
        FaultMetrics::rms_db(&data[start..end.max(start)])
    };

    drop(data);

    FaultMetrics {
        out_rate: rate,
        fail_read_pos,
        splice_frame,
        gpu_zero_start,
        total_collected: n,
        longest_zero_run: max_run_len,
        longest_zero_pos: max_run_pos,
        click_at_splice,
        typical_step,
        rms_before_db,
        rms_after_db,
        underruns,
    }
}

/// Prove (or disprove) that a GPU failure in the middle of a track reaches the
/// listener without a gap.
///
/// Three scenarios on a loud 41 s 44.1 kHz multi-tone source:
///
///   A. 1 M linear, fault at ~8.97 s into the track  (mid-track)
///   B. 1 M linear, fault at ~39.44 s (1.56 s before the 41 s end)  (near-end)
///   C. 30 M Hybrid-Phase (pair), fault at ~9.48 s  (mid-track)
///
/// Each case uses the controller.rs tick() path:
///   build_chain(None GPU) + Msg::Swap{continuous:true}
/// with cpu_lead_s = guard + 0.05 = 0.30 s — the guard-based fix.
///
/// FAIL_AFTER for each case is derived from the block length and the filter
/// delay so the fault fires when the render buffer is filled and the reader is
/// at approximately the target position.
///
/// Run:
///   set AURA_FILTER_DIR=<path to a directory containing FIR filter bank files>
///   cargo test --profile fast --features gpu-player-tests e2e_gpu_fault_mid_track \
///       -- --ignored --nocapture
#[test]
#[ignore]
#[cfg(feature = "gpu-player-tests")]
fn e2e_gpu_fault_mid_track() {
    // ── Source file ──────────────────────────────────────────────────────────
    // A loud file (a zero run must be visible): AURA_E2E_LOUD_FILE, else AURA_E2E_FILE.
    let file = match std::env::var("AURA_E2E_LOUD_FILE").or_else(|_| std::env::var("AURA_E2E_FILE")) {
        Ok(f) => PathBuf::from(f),
        Err(_) => {
            println!("e2e_gpu_fault_mid_track: set AURA_E2E_LOUD_FILE to a loud 44.1 kHz file of 40 s or more");
            return;
        }
    };

    let filter_dir = std::env::var("AURA_FILTER_DIR").unwrap_or_default();
    if filter_dir.is_empty() {
        println!("e2e_gpu_fault_mid_track: set AURA_FILTER_DIR to run");
        return;
    }

    let gpu_ctx = match super::gpu::ctx::GpuPolyCtx::try_build() {
        Some(ctx) => ctx,
        None => {
            println!("e2e_gpu_fault_mid_track: no discrete GPU — skip");
            return;
        }
    };

    // BLOCK = 32768, L = 8 for 44.1 kHz → 352.8 kHz, block_out = 262144.
    // Target ahead = 1.5 s = 529200 frames.  Guard = 0.25 s.
    // cpu_lead_s = guard + 0.05 = 0.30 s (controller.rs fix).
    //
    // FAIL_AFTER derivation (1M linear, delay = 499999, j0 = 1):
    //   j_fail = j0 + FAIL_AFTER - 1 = 1 + N - 1 = N
    //   zero_start = N * 262144 - 499999
    //   For reader ~8.97 s: write_pos ≈ 8.97 + 1.5 = 10.47 s = 3694305 frames
    //     zero_start = 16 * 262144 - 499999 = 3694305 → FAIL_AFTER = 16
    //   For reader ~39.44 s (1.56 s before 41 s end):
    //     write_pos ≈ 40.94 s = 14442209 frames
    //     zero_start = 57 * 262144 - 499999 = 14442209 → FAIL_AFTER = 57
    //
    // FAIL_AFTER for 30M HP (linear delay = 14999999, j0 = 57):
    //   j_fail = 57 + FAIL_AFTER - 1 = 57 + 15 = 72
    //   zero_start = 72 * 262144 - 14999999 = 3874369 → reader ~9.48 s

    // O5 fix: cpu_lead_s = min(lead_s(s_cpu), target_ahead_s - 0.10).
    // For 1M non-HP: lead_s ≈ 0.47 s; for 30M HP: min(1.65, 1.40) = 1.40 s.
    // Keep the test constant at a conservative value that is ≤ both — this
    // ensures no gap AND the run_fault_case helper receives a value that keeps
    // the splice clearly before zero_start.
    const CPU_LEAD_S: f64 = 0.30; // conservative stand-in; real formula above

    let res = super::chain::Resources::new(std::env::temp_dir().join("aura-player-e2e-mid"));
    let cancel = AtomicBool::new(false);

    // ── Case A: 1M linear, mid-track ────────────────────────────────────────
    println!("\n--- Case A: 1M linear, FAIL_AFTER=16, reader ~9 s ---");
    let mut s_a = PlayerSettings::default();
    s_a.phase = Phase::Linear;
    s_a.taps  = 1_000_000;

    let v_a = Arc::new(
        prepare_variant(&file, &s_a, false, &cancel)
            .expect("prepare_variant A")
    );
    let rate = v_a.out_rate;
    println!("  variant: {} Hz → {} Hz", v_a.src.rate, rate);

    let m_a = run_fault_case(
        &res, v_a.clone(), "e2e_gf_a", &s_a,
        gpu_ctx.clone(),
        16,    // FAIL_AFTER
        CPU_LEAD_S,
        8.0,   // timeline capacity seconds
        false,
    );
    m_a.print("Case A: 1M linear mid-track");

    // ── Case B: 1M linear, near-end ─────────────────────────────────────────
    // FAIL_AFTER = 57: zeros start at 57*262144 - 499999 = 14442209 ≈ 40.93 s
    // fail_read_pos ≈ 39.44 s  (1.56 s before the 41 s track end)
    println!("\n--- Case B: 1M linear, FAIL_AFTER=57, reader ~39.4 s (near-end) ---");
    // Reuse the same variant and resources.
    let m_b = run_fault_case(
        &res, v_a.clone(), "e2e_gf_b", &s_a,
        gpu_ctx.clone(),
        57,    // FAIL_AFTER
        CPU_LEAD_S,
        8.0,
        false,
    );
    m_b.print("Case B: 1M linear near-end");

    // ── Case C: 30M Hybrid-Phase, mid-track ─────────────────────────────────
    // Two GPU streams (lin + min).  FAIL_AFTER=16 fires the lin stream first
    // (j0_lin = 57, j_fail_lin = 72, zero_start = 3874369 ≈ 10.98 s).
    // With the old lead_s formula (1.65 s), chain.start > zero_start → gap.
    // With cpu_lead_s = 0.30 s, plan_swap places splice at earliest_rewrite
    // which is ~9.73 s < 10.98 s → no gap.
    println!("\n--- Case C: 30M HP, FAIL_AFTER=16, reader ~9.5 s ---");
    let mut s_c = PlayerSettings::default();
    s_c.phase = Phase::Hybrid;
    s_c.taps  = 30_000_000;

    println!("  preparing 30M HP variant (HPSS envelope — may take ~10 s)…");
    let v_c = Arc::new(
        prepare_variant(&file, &s_c, false, &cancel)
            .expect("prepare_variant C")
    );
    println!("  variant ready: {} Hz → {} Hz", v_c.src.rate, v_c.out_rate);

    // Demonstrate the PRE-FIX gap: use lead_s formula (1.65 s).
    // The coordinator K4 requires this to be reported.
    println!("  [pre-fix] running with lead_s = 1.65 s ...");
    let m_c_pre = run_fault_case(
        &res, v_c.clone(), "e2e_gf_c_pre", &s_c,
        gpu_ctx.clone(),
        16,
        1.65,  // OLD lead_s formula for 30M HP: 0.45 + 2*(30M/30M)*0.6 = 1.65 s
        8.0,
        false,
    );
    m_c_pre.print("Case C (pre-fix, lead=1.65 s): 30M HP mid-track");

    // Now run with the fixed lead (0.30 s) — matches controller.rs after the fix.
    println!("  [fixed] running with cpu_lead_s = 0.30 s ...");
    let m_c = run_fault_case(
        &res, v_c.clone(), "e2e_gf_c", &s_c,
        gpu_ctx.clone(),
        16,
        CPU_LEAD_S,  // 0.30 s
        8.0,
        false,
    );
    m_c.print("Case C (fixed, lead=0.30 s): 30M HP mid-track");

    // ── Case D: 1M linear, failure shortly after a seek (K4-Z) ──────────────
    // The buffer is below its target (target_ahead_s = 1.5 s, but the GPU
    // has only written ~69 ms = 24 289 frames before failing).  The uncapped
    // cpu_lead_s = 0.47 s would land the splice at ~165 816, which is past the
    // first GPU zero (24 289) — a gap.  With use_gpu_zeros_cap=true the splice
    // is capped to gpu_zeros_from − 8 192 = 16 097 frames, before the zeros.
    //
    // FAIL_AFTER = 2:  j0 = 499999/262144 = 1
    //   j_fail = 1 + 2 − 1 = 2
    //   gpu_zero_start = 2×262144 − 499999 = 24 289  (≈ 0.069 s at 352 800 Hz)
    // cpu_lead_s = 0.47 s (lead_s(1M linear) = 0.45 + 1/30 × 0.6 ≈ 0.47 s)
    println!("\n--- Case D: 1M linear, FAIL_AFTER=2, buffer below target (K4-Z cap) ---");
    // [pre-fix] uncapped cpu_lead_s = 0.47 s places the splice past gpu_zeros
    const REAL_LEAD_1M: f64 = 0.47; // lead_s(1M linear)
    println!("  [pre-fix] uncapped lead = {:.2} s — expect gap ...", REAL_LEAD_1M);
    let m_d_pre = run_fault_case(
        &res, v_a.clone(), "e2e_gf_d_pre", &s_a,
        gpu_ctx.clone(),
        2,              // FAIL_AFTER
        REAL_LEAD_1M,
        4.0,
        false,          // no cap
    );
    m_d_pre.print("Case D (pre-fix, lead=0.47 s): 1M lin buffer-below-target");

    // [fixed] same lead, but capped via gpu_zeros_from
    println!("  [fixed] capped via gpu_zeros_from ...");
    let m_d = run_fault_case(
        &res, v_a.clone(), "e2e_gf_d", &s_a,
        gpu_ctx.clone(),
        2,              // FAIL_AFTER
        REAL_LEAD_1M,
        4.0,
        true,           // K4-Z cap enabled
    );
    m_d.print("Case D (fixed, K4-Z cap): 1M lin buffer-below-target");

    // ── Verdict ──────────────────────────────────────────────────────────────
    println!("\n=== VERDICT ===");
    let pass_a = m_a.longest_zero_run == 0 && m_a.splice_frame <= m_a.gpu_zero_start;
    let pass_b = m_b.longest_zero_run == 0 && m_b.splice_frame <= m_b.gpu_zero_start;
    let fail_c_pre = m_c_pre.longest_zero_run > 0 || m_c_pre.splice_frame > m_c_pre.gpu_zero_start;
    let pass_c = m_c.longest_zero_run == 0 && m_c.splice_frame <= m_c.gpu_zero_start;
    let fail_d_pre = m_d_pre.longest_zero_run > 0 || m_d_pre.splice_frame > m_d_pre.gpu_zero_start;
    let pass_d = m_d.longest_zero_run == 0 && m_d.splice_frame <= m_d.gpu_zero_start;
    println!("  A (1M lin mid-track):                {}", if pass_a { "PASS" } else { "FAIL" });
    println!("  B (1M lin near-end):                 {}", if pass_b { "PASS" } else { "FAIL" });
    println!("  C pre-fix (30M HP, lead=1.65):       {}", if fail_c_pre { "GAP DETECTED (expected)" } else { "no gap (unexpected)" });
    println!("  C fixed   (30M HP, lead=0.30):       {}", if pass_c { "PASS" } else { "FAIL" });
    println!("  D pre-fix (1M lin buf-below-target): {}", if fail_d_pre { "GAP DETECTED (expected)" } else { "no gap (unexpected)" });
    println!("  D fixed   (K4-Z cap):                {}", if pass_d { "PASS" } else { "FAIL" });

    assert!(pass_a, "Case A: zero run detected or splice after GPU zeros");
    assert!(pass_b, "Case B: zero run detected or splice after GPU zeros");
    assert!(pass_c, "Case C fixed: zero run detected or splice after GPU zeros");
    assert!(pass_d, "Case D fixed (K4-Z): zero run detected or splice after GPU zeros");
}

// ── Seamless seek harness ─────────────────────────────────────────────────────

/// A loud stereo side_buf of `(PRE_ROLL_SEEK_MS + XFADE_MS)` frames filled
/// with a 440 Hz sine wave at −3 dBFS. Used as the pre-rendered audio handed
/// to `Msg::Swap{seek:true}` in the tests below, so the output never goes
/// silent at the splice point.
fn loud_side_buf(rate: u32) -> (Vec<f64>, Vec<f64>) {
    let n = (((render::PRE_ROLL_SEEK_MS + render::XFADE_MS) / 1000.0) * rate as f64) as usize;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let v = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.7;
        l.push(v);
        // O8: use in-phase (not anti-phase) so the mono mix L+R is non-zero.
        // Anti-phase cancels in the mono silence detector, hiding starvation.
        r.push(v);
    }
    (l, r)
}

/// Longest near-silence run (mono mix, |sample| < `threshold`) in a stereo
/// interleaved buffer, returned in milliseconds.
fn longest_silence_run_ms(buf: &[f64], rate: u32, threshold: f64) -> f64 {
    let mut max_run = 0usize;
    let mut cur = 0usize;
    let n = buf.len() / 2;
    for i in 0..n {
        let mono = ((buf[2 * i] + buf[2 * i + 1]) * 0.5).abs();
        if mono < threshold {
            cur += 1;
            if cur > max_run { max_run = cur; }
        } else {
            cur = 0;
        }
    }
    max_run as f64 / rate as f64 * 1000.0
}

/// Pre-roll helper: waits until the timeline has ≥ `seconds` buffered or 10 s
/// have elapsed.
fn wait_preroll(tl: &Arc<Timeline>, seconds: f64) {
    let t0 = Instant::now();
    while (tl.buffered_frames() as f64) < seconds * tl.rate() as f64
        && t0.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Real-time reader thread: drains the timeline at device pace and appends
/// interleaved stereo frames to `col`.  Runs until `stop` is set.
fn spawn_collector(
    tl: Arc<Timeline>,
    col: Arc<Mutex<Vec<f64>>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rate = tl.rate();
        let chunk = (rate / 100) as usize; // 10 ms
        let mut buf = vec![0.0f64; chunk * 2];
        let start = Instant::now();
        let mut consumed = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
            while consumed + chunk as u64 <= due {
                let got = tl.read_into(&mut buf, chunk);
                if got < chunk { tl.note_underrun(chunk - got); }
                let mut v = col.lock().unwrap();
                v.extend_from_slice(&buf[..chunk * 2]);
                consumed += chunk as u64;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

/// Seek crossfade harness: play for 3 s, send an async seek, play 3 more
/// seconds.  Asserts no near-silence run > 50 ms anywhere in the output and
/// that the mark pushed at the splice carries `index == seek_target_frame`
/// (CR-2).
///
///   set AURA_SEAMLESS_FILE=<44.1 kHz WAV/FLAC, ≥ 40 s, loud>
///   cargo test --profile fast e2e_seek_no_gap -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_seek_no_gap() {
    let file = match std::env::var("AURA_SEAMLESS_FILE")
        .or_else(|_| std::env::var("AURA_E2E_FILE"))
    {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_SEAMLESS_FILE to run e2e_seek_no_gap"); return; }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-e2e-seek-ng"));
    let cancel = AtomicBool::new(false);
    let s = PlayerSettings::default();
    let v = Arc::new(prepare_variant(&file, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("seek_no_gap: {} Hz → {} Hz", v.src.rate, rate);

    let c0 = build_chain(&res, 1, "seek_ng", &v, &s, 0, None, None, 0).unwrap();
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();
    wait_preroll(&tl, 0.3);

    // Collect real-time audio for the whole run.
    let col: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 8)));
    let stop_rd = Arc::new(AtomicBool::new(false));
    let reader = spawn_collector(tl.clone(), col.clone(), stop_rd.clone());

    // Play 3 s then seek to 20 s using the crossfade path.
    std::thread::sleep(Duration::from_secs(3));
    let seek_target_s: f64 = 20.0;
    let seek_target_frame = (seek_target_s * rate as f64) as u64;
    let sb = loud_side_buf(rate);
    let c_seek = build_chain(&res, 1, "seek_ng", &v, &s, seek_target_frame, None, None, 0).unwrap();
    let before = shared.last_switch_ms.load(Ordering::Relaxed);
    // Simulate the window between seek_now() and seek_ready(): seek_pending is
    // briefly true, then cleared just before the Swap is sent.
    shared.seek_pending.store(true, Ordering::Release);
    shared.seek_target_bits.store(seek_target_s.to_bits(), Ordering::Release);
    tx.send(Msg::Swap { chain: c_seek, continuous: false, seek: true, side_buf: Some(sb), requested: Instant::now() }).unwrap();
    shared.seek_pending.store(false, Ordering::Release);

    // Wait for the render thread to place the swap.
    let dl = Instant::now() + Duration::from_secs(8);
    while shared.last_switch_ms.load(Ordering::Relaxed) == before && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(shared.last_switch_ms.load(Ordering::Relaxed), before,
        "seek swap never placed by render thread");
    println!("  swap placed in {}ms", shared.last_switch_ms.load(Ordering::Relaxed));

    // 3 more seconds of audio post-splice.
    std::thread::sleep(Duration::from_secs(3));
    stop_rd.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    let data = col.lock().unwrap();
    let silence_ms = longest_silence_run_ms(&data, rate, 1e-9);
    println!("  collected {} frames ({:.1} s)  longest silence {:.2} ms",
        data.len() / 2, data.len() as f64 / 2.0 / rate as f64, silence_ms);

    // CR-2: after 3 s of playback the current position should be near
    // seek_target_s + 3 s.  shared.locate() returns the *interpolated*
    // position (mark.index + frames_since_mark), so it advances with time —
    // asserting m.index == seek_target_frame only holds at the instant of the
    // splice, not 3 s later.
    if let Some(m) = shared.locate(tl.read_pos()) {
        let pos_s = m.index as f64 / m.rate as f64;
        let expected_s = seek_target_s + 3.0;
        println!("  current pos {:.2} s  expected ~{:.2} s  (seek target {:.2} s + 3 s play)",
            pos_s, expected_s, seek_target_s);
        assert!(
            (pos_s - expected_s).abs() < 1.0,
            "CR-2: position {:.2} s is not within 1 s of seek_target+3 s = {:.2} s",
            pos_s, expected_s
        );
    }

    assert!(silence_ms < 50.0,
        "zero/silence run {:.2} ms > 50 ms limit at seek splice", silence_ms);

    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== e2e_seek_no_gap: PASS ===");
}

/// Rapid seeks: three seek swaps sent back-to-back within ≪ 300 ms.
/// Because the render thread replaces `pending` on each Swap, only the last
/// target is consumed by the reader.  Verified by checking the mark pushed at
/// the splice.
///
///   set AURA_SEAMLESS_FILE=<44.1 kHz WAV/FLAC, ≥ 40 s, loud>
///   cargo test --profile fast e2e_rapid_seeks -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_rapid_seeks() {
    let file = match std::env::var("AURA_SEAMLESS_FILE")
        .or_else(|_| std::env::var("AURA_E2E_FILE"))
    {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_SEAMLESS_FILE to run e2e_rapid_seeks"); return; }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-e2e-rapid"));
    let cancel = AtomicBool::new(false);
    let s = PlayerSettings::default();
    let v = Arc::new(prepare_variant(&file, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("rapid_seeks: {} Hz → {} Hz", v.src.rate, rate);

    let c0 = build_chain(&res, 1, "rapid", &v, &s, 0, None, None, 0).unwrap();
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();
    wait_preroll(&tl, 0.3);

    let col: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 8)));
    let stop_rd = Arc::new(AtomicBool::new(false));
    let reader = spawn_collector(tl.clone(), col.clone(), stop_rd.clone());

    std::thread::sleep(Duration::from_secs(2));

    // Send three seeks to different targets without any delay between them.
    // All three arrive in the channel before the render thread drains it.
    let targets_s: [f64; 3] = [8.0, 16.0, 28.0];
    let _last_target_frame = (targets_s[2] * rate as f64) as u64;
    for (i, &ts) in targets_s.iter().enumerate() {
        let tf = (ts * rate as f64) as u64;
        let c = build_chain(&res, 1, "rapid", &v, &s, tf, None, None, 0).unwrap();
        let sb = loud_side_buf(rate);
        tx.send(Msg::Swap { chain: c, continuous: false, seek: true, side_buf: Some(sb), requested: Instant::now() }).unwrap();
        println!("  sent swap {} → {:.0} s target", i + 1, ts);
    }

    // Wait for the final swap to be placed.
    let before = shared.last_switch_ms.load(Ordering::Relaxed);
    let dl = Instant::now() + Duration::from_secs(10);
    while shared.last_switch_ms.load(Ordering::Relaxed) == before && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(shared.last_switch_ms.load(Ordering::Relaxed), before,
        "no seek swap placed");

    // Allow 1 s of audio to drain past the splice, then read the mark.
    // shared.locate() returns the *interpolated* position (mark advances with
    // time), so after 1 s of play idx ≈ last_target_frame + ~1*rate — check a
    // range rather than exact equality.
    std::thread::sleep(Duration::from_secs(1));
    let last_target_s = targets_s[2];
    if let Some(m) = shared.locate(tl.read_pos()) {
        let pos_s = m.index as f64 / m.rate as f64;
        println!("  current pos {:.2} s  last_target {:.2} s  (rate {})",
            pos_s, last_target_s, m.rate);
        assert!(
            pos_s >= last_target_s && pos_s < last_target_s + 2.0,
            "rapid seeks: position {:.2} s not in [{:.2}, {:.2}) — early seek may have been consumed",
            pos_s, last_target_s, last_target_s + 2.0
        );
    }

    stop_rd.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    let data = col.lock().unwrap();
    let silence_ms = longest_silence_run_ms(&data, rate, 1e-9);
    println!("  silence {:.2} ms  collected {:.1} s",
        silence_ms, data.len() as f64 / 2.0 / rate as f64);
    assert!(silence_ms < 50.0,
        "zero run {:.2} ms > 50 ms after rapid seeks", silence_ms);

    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== e2e_rapid_seeks: PASS ===");
}

/// Seek while paused: send a seek swap when no reader is draining the
/// timeline.  The swap must still be placed (mark and tokens updated) so
/// status shows the new position immediately on resume.
///
///   set AURA_SEAMLESS_FILE=<44.1 kHz WAV/FLAC, ≥ 40 s, loud>
///   cargo test --profile fast e2e_seek_while_paused -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_seek_while_paused() {
    let file = match std::env::var("AURA_SEAMLESS_FILE")
        .or_else(|_| std::env::var("AURA_E2E_FILE"))
    {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_SEAMLESS_FILE to run e2e_seek_while_paused"); return; }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-e2e-seek-paused"));
    let cancel = AtomicBool::new(false);
    let s = PlayerSettings::default();
    let v = Arc::new(prepare_variant(&file, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("seek_while_paused: {} Hz → {} Hz", v.src.rate, rate);

    let c0 = build_chain(&res, 1, "seek_p", &v, &s, 0, None, None, 0).unwrap();
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    // Large timeline so the render thread fills it and blocks, simulating pause.
    let tl = Arc::new(Timeline::new(rate, 8.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();
    // Let the render thread fill the buffer (no reader consuming ≡ paused).
    wait_preroll(&tl, 2.0);

    let seek_target_s: f64 = 15.0;
    let seek_target_frame = (seek_target_s * rate as f64) as u64;
    let sb = loud_side_buf(rate);
    let c_seek = build_chain(&res, 1, "seek_p", &v, &s, seek_target_frame, None, None, 0).unwrap();
    let before = shared.last_switch_ms.load(Ordering::Relaxed);
    tx.send(Msg::Swap { chain: c_seek, continuous: false, seek: true, side_buf: Some(sb), requested: Instant::now() }).unwrap();

    // Drain a little to unblock the render thread if the timeline is full.
    let chunk = (rate / 100) as usize;
    let mut tmp = vec![0.0f64; chunk * 2];
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if shared.last_switch_ms.load(Ordering::Relaxed) != before { break; }
        if Instant::now() >= deadline {
            panic!("seek_while_paused: swap never placed after 8 s");
        }
        tl.read_into(&mut tmp, chunk);
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("  swap placed in {}ms", shared.last_switch_ms.load(Ordering::Relaxed));

    // Drain past the splice point so that locate() sees the new mark.
    // The splice was placed at earliest_rewrite() ≈ read_pos + guard; we
    // need read_pos to pass that frame.
    let guard = tl.guard_frames() as usize + chunk;
    let mut extra_drained = 0;
    while extra_drained < guard {
        let got = tl.read_into(&mut tmp, chunk);
        extra_drained += got.max(1);
    }

    // CR-2: the mark's extrapolated position must be near seek_target_s
    // (shared.locate returns the interpolated position — index advances with
    // read_pos — so we check a range of ±1 s rather than exact equality).
    if let Some(m) = shared.locate(tl.read_pos()) {
        let pos_s = m.index as f64 / m.rate as f64;
        println!("  mark pos {:.2} s  seek_target {:.2} s",
            pos_s, seek_target_s);
        assert!(
            (pos_s - seek_target_s).abs() < 1.0,
            "seek_while_paused CR-2: pos {:.2} s not within 1 s of target {:.2} s",
            pos_s, seek_target_s
        );
    } else {
        panic!("seek_while_paused: no mark found after swap");
    }

    tx.send(Msg::Stop).unwrap();
    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== e2e_seek_while_paused: PASS ===");
}

/// Stop during a pending seek: send Stop while seek_pending is set; the
/// pending swap is dropped and the timeline cleared.  After Stop, starting a
/// new track must work cleanly (no stale crossfade buffers — CR-6).
///
///   set AURA_SEAMLESS_FILE=<44.1 kHz WAV/FLAC, ≥ 40 s, loud>
///   cargo test --profile fast e2e_stop_during_seek -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_stop_during_seek() {
    let file = match std::env::var("AURA_SEAMLESS_FILE")
        .or_else(|_| std::env::var("AURA_E2E_FILE"))
    {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_SEAMLESS_FILE to run e2e_stop_during_seek"); return; }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-e2e-stop-seek"));
    let cancel = AtomicBool::new(false);
    let s = PlayerSettings::default();
    let v = Arc::new(prepare_variant(&file, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("stop_during_seek: {} Hz → {} Hz", v.src.rate, rate);

    let c0 = build_chain(&res, 1, "stop_sk", &v, &s, 0, None, None, 0).unwrap();
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();
    wait_preroll(&tl, 0.3);

    // Mark seek in-flight, queue the swap (CR-6 target: Stop must clear side_buf).
    let seek_target_frame = (10.0 * rate as f64) as u64;
    let sb = loud_side_buf(rate);
    let c_seek = build_chain(&res, 1, "stop_sk", &v, &s, seek_target_frame, None, None, 0).unwrap();
    shared.seek_pending.store(true, Ordering::Release);
    tx.send(Msg::Swap { chain: c_seek, continuous: false, seek: true, side_buf: Some(sb), requested: Instant::now() }).unwrap();

    // Stop immediately — the pending Swap and its side_buf must be dropped.
    tx.send(Msg::Stop).unwrap();
    // seek_pending is cleared by the controller after Stop; simulate it.
    shared.seek_pending.store(false, Ordering::Release);

    // Start a fresh track; it must render cleanly with no stale crossfade.
    let c1 = build_chain(&res, 2, "stop_sk2", &v, &s, 0, None, None, 0).unwrap();
    let tl2 = Arc::new(Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: c1, timeline: tl2.clone() }).unwrap();
    wait_preroll(&tl2, 0.3);

    // Drain 2 s and assert no silence (fresh start, no crossfade artifact).
    let col: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 4)));
    let stop_rd = Arc::new(AtomicBool::new(false));
    let reader = spawn_collector(tl2.clone(), col.clone(), stop_rd.clone());
    std::thread::sleep(Duration::from_secs(2));
    stop_rd.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    let data = col.lock().unwrap();
    let silence_ms = longest_silence_run_ms(&data, rate, 1e-9);
    println!("  post-stop fresh track: collected {:.1} s  longest silence {:.2} ms",
        data.len() as f64 / 2.0 / rate as f64, silence_ms);
    assert!(silence_ms < 50.0,
        "CR-6: silence {:.2} ms > 50 ms in fresh track after stop-during-seek", silence_ms);

    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== e2e_stop_during_seek: PASS ===");
}

/// Paused rack change: while no reader drains the timeline, send a
/// continuous Swap (same track position, different settings — equivalent to
/// changing a DSP setting while paused).  The mark pushed for the new chain
/// must be visible immediately through `shared.locate()`, so the status
/// snapshot reflects the updated description without waiting for the reader
/// to reach the splice frame.
///
///   set AURA_SEAMLESS_FILE=<44.1 kHz WAV/FLAC, ≥ 40 s, loud>
///   cargo test --profile fast e2e_paused_rack_change -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_paused_rack_change() {
    let file = match std::env::var("AURA_SEAMLESS_FILE")
        .or_else(|_| std::env::var("AURA_E2E_FILE"))
    {
        Ok(f) => PathBuf::from(f),
        Err(_) => { println!("set AURA_SEAMLESS_FILE to run e2e_paused_rack_change"); return; }
    };
    let res = Resources::new(std::env::temp_dir().join("aura-e2e-rack-ch"));
    let cancel = AtomicBool::new(false);
    let s1 = PlayerSettings::default();
    let v = Arc::new(prepare_variant(&file, &s1, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("paused_rack_change: {} Hz → {} Hz", v.src.rate, rate);

    let c0 = build_chain(&res, 1, "rack_ch", &v, &s1, 0, None, None, 0).unwrap();
    let desc0 = c0.describe();
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(Timeline::new(rate, 8.0));
    tx.send(Msg::Start { chain: c0, timeline: tl.clone() }).unwrap();
    // Fill the buffer (pause simulation: no reader).
    wait_preroll(&tl, 2.0);

    // Change a setting (switch to Linear phase) — continuous swap at the same
    // output position the reader is at (which is 0, since no reads happened).
    let mut s2 = s1.clone();
    s2.phase = Phase::Linear;
    // Locate at read_pos=0 to find the current position.
    let read_pos = tl.read_pos();
    let start_frame = shared.locate(read_pos).map(|m| m.index).unwrap_or(0);
    let c_new = build_chain(&res, 1, "rack_ch", &v, &s2, start_frame, None, None, 0).unwrap();
    let desc_new = c_new.describe();
    let before = shared.last_switch_ms.load(Ordering::Relaxed);
    tx.send(Msg::Swap { chain: c_new, continuous: true, seek: false, side_buf: None, requested: Instant::now() }).unwrap();

    // Drain a little to unblock the render thread so it can place the swap.
    let chunk = (rate / 100) as usize;
    let mut tmp = vec![0.0f64; chunk * 2];
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if shared.last_switch_ms.load(Ordering::Relaxed) != before { break; }
        if Instant::now() >= deadline {
            panic!("paused_rack_change: swap never placed after 8 s");
        }
        tl.read_into(&mut tmp, chunk);
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("  swap placed in {}ms", shared.last_switch_ms.load(Ordering::Relaxed));

    // The tokens/description should reflect the new chain.
    let tokens_new = shared.tokens.lock().unwrap().clone();
    println!("  old desc: {}", desc0);
    println!("  new desc: {}", desc_new);
    println!("  tokens after swap: {:?}", tokens_new);
    assert_ne!(tokens_new.join(" "), "", "no tokens after rack change");

    tx.send(Msg::Stop).unwrap();
    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== e2e_paused_rack_change: PASS ===");
}

// ── HP-deferred timing and transition tests ───────────────────────────────────

/// Helpers shared by the HP-deferred tests below.
mod hp_deferred_helpers {
    use std::io::Write;
    use std::path::Path;

    /// A music-like signal with tones, noise, and drum hits every half second.
    pub fn music(n: usize, rate: u32, seed: u64) -> Vec<f64> {
        let mut x = seed;
        let beat = (rate / 2) as usize;
        (0..n).map(|i| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((x >> 33) as f64 / u32::MAX as f64 - 0.5) * 0.05;
            let t = i as f64 / rate as f64;
            let tones = 0.1 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()
                + 0.05 * (2.0 * std::f64::consts::PI * 1_760.0 * t).sin();
            let k = i % beat;
            let hit = if k < 2_000 {
                0.5 * (-(k as f64) / 300.0).exp()
                    * ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5)
            } else {
                0.0
            };
            tones + noise + hit
        })
        .collect()
    }

    pub fn write_wav16(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
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

    pub fn ms(t: std::time::Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0
    }
}

/// Time to first sound — BEFORE (HP synchronous) vs AFTER (HP deferred).
///
/// BEFORE: prepare_variant(quick) + res.envelope() [blocks] + res.plan() +
///         build_chain(HP, envelope cached).
/// AFTER:  prepare_variant(quick) + build_chain(HP, empty cache) — returns a
///         linear chain immediately with hp_deferred=true.
///
/// Two cases: 38 min 44.1 kHz and 3 min 192 kHz (CPU, 30M taps, FS×2).
///
///   set AURA_FILTER_DIR=<fir-optimizer/output>
///   cargo test --profile fast hp_deferred_timing -- --ignored --nocapture
#[test]
#[ignore]
fn hp_deferred_timing() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    use super::chain::{build_chain, hp_ready, prepare_variant, Resources};
    use crate::player::settings::{Phase, PlayerSettings};
    use hp_deferred_helpers::*;

    let s = PlayerSettings {
        phase: Phase::Hybrid,
        fs_multiplier: 2,
        taps: 30_000_000,
        ..Default::default()
    };

    // Verify that 30M filters are available before running the heavy part.
    {
        use crate::audio::converter::dsp::filter::find_precomputed_filter;
        // FS×2 from 44.1 kHz = 88.2 kHz output.
        if find_precomputed_filter(30_000_000, 44_100, 88_200, "linear_phase").is_none() {
            println!("hp_deferred_timing: 30M/88.2 kHz filter not found — set AURA_FILTER_DIR or install portable filters");
            return;
        }
        // FS×2 from 192 kHz = 384 kHz output: the ×2 filter.
        if find_precomputed_filter(30_000_000, 192_000, 384_000, "linear_phase").is_none() {
            println!("hp_deferred_timing: 30M ×2 filter (192 → 384 kHz) not found — set AURA_FILTER_DIR or install portable filters");
            return;
        }
    }

    let dir = tempfile::tempdir().unwrap();

    println!(
        "{:<15} | {:>14} | {:>13} | {:>12} | {:>9}",
        "case", "BEFORE total", "AFTER total", "chain-only", "ratio"
    );
    println!("{}", "-".repeat(72));

    for &(mins, rate) in &[(38u32, 44_100u32), (3u32, 192_000u32)] {
        let n = (mins * 60 * rate) as usize;
        let (l, r) = (music(n, rate, 1), music(n, rate, 2));
        let wav = dir.path().join(format!("hp_defer_{}_{}.wav", mins, rate));
        write_wav16(&wav, rate, &l, &r);
        drop((l, r));

        let res = Resources::new(dir.path().join("cache"));
        let cancel = AtomicBool::new(false);
        let key = format!("hpd:{}:{}", mins, rate);

        // ── Prepare quick variant (same cost in both paths) ─────────────────
        let t_q = Instant::now();
        let v_q = Arc::new(prepare_variant(&wav, &s, true, &cancel).unwrap());
        let q_ms = ms(t_q);

        // Warm the filter banks with the quick variant (first load is not
        // part of the measurement).  Banks are keyed by path+rate, so
        // the HP pair (lin + min) at this rate gets loaded here.
        res.forget_tracks_except(&[]);
        let _ = build_chain(&res, 1, &key, &v_q, &s, 0, None, None, 0); // warms banks
        // Clear envelope cache so both measurements start cold.
        res.forget_tracks_except(&[]);
        for e in std::fs::read_dir(dir.path().join("cache")).unwrap().flatten() {
            let _ = std::fs::remove_file(e.path());
        }

        // ── BEFORE: envelope blocks, then build HP from cache ────────────────
        let t0 = Instant::now();
        // Measure the old blocking path: envelope + plan + build HP from cache.
        let env = res.envelope(&key, &v_q).unwrap();
        let _plan = res.plan(&key, &v_q, &env);
        let c_before = build_chain(&res, 1, &key, &v_q, &s, 0, None, None, 0).unwrap();
        let sync_ms = ms(t0);
        assert!(!c_before.hp_deferred, "envelope was cached — chain must be HP");

        // Clear cache again for the AFTER measurement.
        res.forget_tracks_except(&[]);
        for e in std::fs::read_dir(dir.path().join("cache")).unwrap().flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        assert!(!hp_ready(&res, &key, &v_q), "cache must be empty before AFTER measurement");

        // ── AFTER: build_chain returns immediately as linear (hp_deferred=true) ──
        let t1 = Instant::now();
        let c_after = build_chain(&res, 1, &key, &v_q, &s, 0, None, None, 0).unwrap();
        let defer_ms = ms(t1);
        assert!(c_after.hp_deferred, "AFTER path: hp_deferred must be true");

        // Total "time to first sound": prepare_quick + (envelope+chain | chain_only).
        let before_total = q_ms + sync_ms;
        let after_total = q_ms + defer_ms;
        let ratio = sync_ms / defer_ms.max(0.01);
        println!(
            "{:<15} | {:>10.0} ms | {:>9.0} ms | {:>8.1} ms | {:>8.0}x",
            format!("{} min {} Hz", mins, rate),
            before_total,
            after_total,
            defer_ms,
            ratio
        );
        assert!(
            after_total < q_ms + 500.0,
            "AFTER time to first sound {:.1} ms: deferred chain build took {:.1} ms (>= 500 ms limit)",
            after_total,
            defer_ms
        );

        let _ = std::fs::remove_file(&wav);
    }
}

/// Transition from linear (hp_deferred) to HP without audible silence.
///
/// Plays a synthetic track as hp_deferred=true (linear chain), then mimics
/// what spawn_hp_job does: computes the envelope in the background, builds
/// the HP chain, and sends Msg::Swap{continuous:true}.  Verifies that the
/// render thread's write_pos keeps advancing at all times (no stall) and that
/// the mark's hp_deferred becomes false after the swap.
///
///   set AURA_FILTER_DIR=<fir-optimizer/output>
///   cargo test --profile fast hp_deferred_swap_no_gap -- --ignored --nocapture
#[test]
#[ignore]
fn hp_deferred_swap_no_gap() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::chain::{build_chain, prepare_variant, Resources};
    use super::output::OutputShared;
    use super::render::{self, Msg};
    use crate::player::settings::{Phase, PlayerSettings};
    use hp_deferred_helpers::*;

    let s = PlayerSettings {
        phase: Phase::Hybrid,
        fs_multiplier: 2,
        taps: 30_000_000,
        ..Default::default()
    };

    // Check filters.
    {
        use crate::audio::converter::dsp::filter::find_precomputed_filter;
        if find_precomputed_filter(30_000_000, 44_100, 88_200, "linear_phase").is_none()
            || find_precomputed_filter(30_000_000, 44_100, 88_200, "minimum_phase").is_none()
        {
            println!("hp_deferred_swap_no_gap: 30M/88.2 kHz filters not found — set AURA_FILTER_DIR");
            return;
        }
    }

    let dir = tempfile::tempdir().unwrap();
    // 30 seconds of music at 44.1 kHz.
    let rate_src = 44_100u32;
    let n = (30 * rate_src) as usize;
    let (l, r) = (music(n, rate_src, 3), music(n, rate_src, 4));
    let wav = dir.path().join("hp_defer_swap.wav");
    write_wav16(&wav, rate_src, &l, &r);
    drop((l, r));

    let res = Arc::new(Resources::new(dir.path().join("cache")));
    let cancel = AtomicBool::new(false);
    let key = "hpd-swap:30:44100".to_string();

    let v = Arc::new(prepare_variant(&wav, &s, false, &cancel).unwrap());
    let rate = v.out_rate;
    println!("hp_deferred_swap_no_gap: src {} Hz → out {} Hz", rate_src, rate);

    // Warm banks.
    let _ = build_chain(&res, 1, &key, &v, &s, 0, None, None, 0);
    // Clear envelope cache so build_chain returns hp_deferred=true.
    res.forget_tracks_except(&[]);
    for e in std::fs::read_dir(dir.path().join("cache")).unwrap().flatten() {
        let _ = std::fs::remove_file(e.path());
    }

    // Build the initial linear chain (hp_deferred=true).
    let chain_linear = build_chain(&res, 1, &key, &v, &s, 0, None, None, 0).unwrap();
    assert!(chain_linear.hp_deferred, "pre-condition: initial chain must be hp_deferred");
    println!("  initial chain is hp_deferred=true (linear stand-in)");

    // Start render harness.
    let (tx, rx) = render::channel();
    let shared = render::Shared::new();
    let out_shared = OutputShared::new();
    render::spawn(rx, shared.clone(), out_shared);
    let tl = Arc::new(super::timeline::Timeline::new(rate, 4.0));
    tx.send(Msg::Start { chain: chain_linear, timeline: tl.clone() }).unwrap();

    // Pre-roll.
    wait_preroll(&tl, 0.3);

    // Spawn a reader (consumes at real-time pace).
    let stop_rd = Arc::new(AtomicBool::new(false));
    let col = Arc::new(Mutex::new(Vec::<f64>::with_capacity(rate as usize * 8)));
    let reader = spawn_collector(tl.clone(), col.clone(), stop_rd.clone());

    // Snapshot write_pos before the swap.
    let wp_before_swap = tl.write_pos();
    let t_swap_start = Instant::now();

    // Background: compute envelope + build HP chain + send swap (mimics spawn_hp_job).
    let (tx2, v2, res2, key2, s2, tl2) = (tx.clone(), v.clone(), res.clone(), key.clone(), s.clone(), tl.clone());
    std::thread::spawn(move || {
        // Blocking envelope computation.
        let env = res2.envelope(&key2, &v2).expect("envelope");
        let _plan = res2.plan(&key2, &v2, &env);
        // Build HP chain at read_pos + guard (250 ms).
        let guard_frames = (0.25 * rate as f64 + 0.05 * rate as f64) as u64;
        let start = tl2.read_pos() + guard_frames;
        let chain_hp = build_chain(&res2, 1, &key2, &v2, &s2, start, None, None, 0)
            .expect("HP chain");
        assert!(!chain_hp.hp_deferred, "HP chain must have hp_deferred=false");
        let _ = tx2.send(Msg::Swap {
            chain: chain_hp,
            continuous: true,
            seek: false,
            side_buf: None,
            requested: Instant::now(),
        });
        println!("  HP swap sent after {:.2} s of envelope computation",
            t_swap_start.elapsed().as_secs_f64());
    });

    // Wait for the HP swap to be placed (last_switch_ms advances).
    let before_switch = shared.last_switch_ms.load(std::sync::atomic::Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(120); // envelope can take ~76 s max
    loop {
        if shared.last_switch_ms.load(std::sync::atomic::Ordering::Relaxed) != before_switch {
            break;
        }
        if Instant::now() >= deadline {
            stop_rd.store(true, std::sync::atomic::Ordering::Relaxed);
            reader.join().unwrap();
            panic!("hp_deferred_swap_no_gap: HP swap never placed within 120 s");
        }
        // Monitor: write_pos must keep advancing (render thread not stalled).
        let wp_now = tl.write_pos();
        assert!(
            wp_now >= wp_before_swap || Instant::now() - t_swap_start < Duration::from_millis(200),
            "render stall detected: write_pos stopped advancing before swap"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let swap_ms = t_swap_start.elapsed().as_secs_f64() * 1000.0;
    println!("  HP swap placed after {:.0} ms  (last_switch_ms={})",
        swap_ms, shared.last_switch_ms.load(std::sync::atomic::Ordering::Relaxed));

    // After the swap the mark should show hp_deferred=false.
    std::thread::sleep(Duration::from_millis(200)); // let reader advance past splice
    if let Some(m) = shared.locate(tl.read_pos()) {
        println!("  mark hp_deferred={} after swap (expect false)", m.desc.hp_deferred);
        assert!(!m.desc.hp_deferred, "mark.desc.hp_deferred must be false after HP swap");
    }

    // Check write_pos is still advancing after the swap (no stall in HP chain).
    let wp_a = tl.write_pos();
    std::thread::sleep(Duration::from_millis(200));
    let wp_b = tl.write_pos();
    assert!(wp_b > wp_a, "write_pos stalled after HP swap: before={} after={}", wp_a, wp_b);
    println!("  write_pos advancing after swap: {} → {} (+{} frames)", wp_a, wp_b, wp_b - wp_a);

    // Collect the data around the splice window and check silence.
    std::thread::sleep(Duration::from_millis(300));
    stop_rd.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    tx.send(Msg::Stop).unwrap();

    let data = col.lock().unwrap();
    // Only examine after the reader has consumed past the pre-roll (first 0.3 s).
    let skip = (0.3 * rate as f64) as usize;
    let silence_ms = if data.len() > skip * 2 {
        longest_silence_run_ms(&data[skip * 2..], rate, 1e-9)
    } else {
        0.0
    };
    println!("  collected {:.1} s  longest silence after pre-roll: {:.3} ms",
        data.len() as f64 / 2.0 / rate as f64, silence_ms);
    assert!(
        silence_ms < 10.0,
        "linear→HP swap produced a silence run of {:.3} ms (limit 10 ms)",
        silence_ms
    );

    if let Some(e) = shared.error.lock().unwrap().clone() {
        panic!("render error: {}", e);
    }
    println!("=== hp_deferred_swap_no_gap: PASS ===");
}
