//! `aura-engine --render-bench <audio file> <taps> <fs multiplier> [multi|single] [seconds of output] [tfs|linear|minimum|alpha|hybrid]`:
//! how fast the player's CPU render path produces output with the real filter
//! of that size, in multiples of real time (output frames / wall time). Used
//! to compare build profiles (`docs/04-developer-guide.md`, "Build profiles").
//! The filter is found like the player finds it (`AURA_FILTER_DIR`, then the
//! folder next to the exe).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use super::chain::{build_chain, prepare_variant, Resources};
use super::settings::{Phase, PlayerSettings, Upsampler};

const STEP: usize = 8_192;

/// The parsed command line: file, taps, fs multiplier, upsampler, seconds.
#[derive(Debug, PartialEq)]
struct Args {
    file: String,
    taps: usize,
    fs: u32,
    up: Upsampler,
    secs: usize,
    phase: Phase,
}

/// `None` when the arguments are not a complete `--render-bench` line.
fn parse(args: &[String]) -> Option<Args> {
    Some(Args {
        file: args.get(1)?.clone(),
        taps: args.get(2)?.parse().ok()?,
        fs: args.get(3)?.parse().ok()?,
        up: match args.get(4).map(|s| s.as_str()) {
            Some("single") => Upsampler::Single,
            _ => Upsampler::Multi,
        },
        secs: args.get(5).and_then(|s| s.parse().ok()).unwrap_or(30),
        phase: match args.get(6).map(|s| s.as_str()) {
            Some("linear") => Phase::Linear,
            Some("minimum") => Phase::Minimum,
            Some("alpha") => Phase::Alpha,
            Some("hybrid") => Phase::Hybrid,
            _ => Phase::Tfs,
        },
    })
}

/// Seconds of output made per second of wall time.
fn times_real(frames: usize, out_rate: u32, wall_s: f64) -> f64 {
    frames as f64 / out_rate as f64 / wall_s
}

/// `true` when the argument was this diagnostic and has run.
pub fn handle_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) != Some("--render-bench") {
        return false;
    }
    match parse(&args) {
        Some(a) => run(&a.file, a.taps, a.fs, a.up, a.secs, a.phase),
        None => println!("usage: --render-bench <audio file> <taps> <fs multiplier> [multi|single] [seconds of output, 30] [tfs|linear|minimum|alpha|hybrid]"),
    }
    true
}

fn run(file: &str, taps: usize, fs: u32, up: Upsampler, secs: usize, phase: Phase) {
    let s = PlayerSettings { fs_multiplier: fs, taps, phase, use_gpu: false, upsampler: Some(up), ..PlayerSettings::default() };
    let cache = std::env::temp_dir().join(format!("aura-render-bench-{}", std::process::id()));
    let res = Resources::new(cache.clone());
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let v = match prepare_variant(std::path::Path::new(file), &s, true, &cancel) {
        Ok(v) => Arc::new(v),
        Err(e) => {
            println!("render-bench: cannot prepare {file}: {e}");
            return;
        }
    };
    let prep = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut chain = match build_chain(&res, 1, &format!("rb:{file}"), &v, &s, 0, None, None, 0) {
        Ok(c) => c,
        Err(e) => {
            println!("render-bench: chain: {e}");
            return;
        }
    };
    let build = t.elapsed().as_secs_f64();
    let (mut bl, mut br) = (vec![0.0; STEP], vec![0.0; STEP]);
    // The first second is warm-up (pre-roll, cold caches): not counted.
    let warm = v.out_rate as usize;
    let mut got = 0;
    while got < warm {
        chain.stage.read(&mut bl, &mut br);
        got += STEP;
    }
    let want = secs * v.out_rate as usize;
    let (mut frames, t) = (0usize, Instant::now());
    while frames < want {
        chain.stage.read(&mut bl, &mut br);
        frames += STEP;
    }
    let wall = t.elapsed().as_secs_f64();
    let rt = times_real(frames, v.out_rate, wall);
    println!(
        "render-bench: taps {taps} x{fs} {up:?} {phase:?} out {} Hz | prepare {prep:.1} s, chain {build:.1} s | {:.1} s of output in {wall:.2} s = {rt:.2}x real time",
        v.out_rate,
        frames as f64 / v.out_rate as f64,
    );
    drop(chain);
    let _ = std::fs::remove_dir_all(cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_full_line_parses_and_defaults_fill_the_rest() {
        let a = parse(&line(&["--render-bench", "a.wav", "1000000", "16"])).unwrap();
        assert_eq!(a, Args { file: "a.wav".into(), taps: 1_000_000, fs: 16, up: Upsampler::Multi, secs: 30, phase: Phase::Tfs });
        let b = parse(&line(&["--render-bench", "a.wav", "5", "4", "single", "20", "linear"])).unwrap();
        assert_eq!((b.up, b.secs, b.phase), (Upsampler::Single, 20, Phase::Linear));
    }

    #[test]
    fn an_incomplete_line_is_refused() {
        assert!(parse(&line(&["--render-bench"])).is_none());
        assert!(parse(&line(&["--render-bench", "a.wav", "x", "16"])).is_none());
        assert!(parse(&line(&["--render-bench", "a.wav", "5"])).is_none());
    }

    #[test]
    fn real_time_factor_is_output_seconds_over_wall() {
        assert!((times_real(705_600, 705_600, 0.5) - 2.0).abs() < 1e-9);
    }
}
