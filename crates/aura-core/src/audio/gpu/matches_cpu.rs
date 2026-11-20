//! The GPU convolver against the CPU one on the real filters, and against
//! what an earlier build of itself put out.
//!
//! The CPU convolver is the f64 reference. The GPU one runs in double-single
//! arithmetic, so the two agree to its rounding (about −280 dBFS), never bit
//! for bit; a difference anywhere near the −200 dBFS bound means a wrong
//! partition, a wrong bin or a channel leaking into the other. The signal has
//! a stretch where only the left channel plays and one where only the right
//! does, so a leak cannot hide under the other channel's level.
//!
//! Needs the GPU and the .npy matrix, so it is ignored:
//!
//!     cargo test --profile fast --bins -- --ignored gpu_matches_cpu --nocapture
//!
//! `AURA_GPU_DUMP=<dir>` writes this build's GPU output for each case there;
//! `AURA_GPU_REF=<dir>` compares this build's GPU output with what an earlier
//! build wrote. `AURA_EQ_CASES` (comma-separated names) picks cases;
//! `AURA_EQ_NO_CPU=1` skips the CPU reference. `AURA_FILTER_DIR` points at
//! the matrix (default: the repository's `fir-optimizer/output`).

#![cfg(test)]

use super::processor::GpuDspProcessor;
use crate::audio::dsp_core::{load_npy_f64, CpuDspProcessor};
use crate::audio::processor::DspProcessor;
use std::path::PathBuf;
use std::time::Instant;

struct Case {
    name: &'static str,
    file: &'static str,
    /// Polyphase stride; 1 is the whole filter (the Hybrid-Phase pass).
    l: usize,
    phases: &'static [usize],
    /// Input length in GPU blocks.
    blocks: f64,
    /// Output compared beyond the input, at most this many GPU blocks of the
    /// filter's tail (the CPU reference is slow on the whole 30M filter).
    tail_blocks: usize,
}

const CASES: [Case; 4] = [
    Case { name: "1M-x8", file: "fir_1M_352800_linear_phase.npy", l: 8, phases: &[0, 3, 7], blocks: 3.5, tail_blocks: 2 },
    Case { name: "10M-x8", file: "fir_10M_352800_linear_phase.npy", l: 8, phases: &[0, 7], blocks: 2.5, tail_blocks: 1 },
    Case { name: "30M-x8", file: "fir_30M_352800_linear_phase.npy", l: 8, phases: &[0, 7], blocks: 2.5, tail_blocks: 2 },
    Case { name: "30M-min", file: "fir_30M_352800_minimum_phase.npy", l: 1, phases: &[0], blocks: 0.75, tail_blocks: 2 },
];

fn filter_dir() -> PathBuf {
    std::env::var_os("AURA_FILTER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fir-optimizer/output")
        })
}

/// Music-like and deterministic: a few partials with slow amplitude motion
/// and a little noise, about −6 dBFS at the peaks. The first half is stereo,
/// then a quarter with the right channel silent, then a quarter with the
/// left channel silent.
fn signal(n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };
    let fs = 44_100.0;
    let tau = std::f64::consts::TAU;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / fs;
        let am = 0.6 + 0.4 * (tau * 0.37 * t).sin();
        let a = 0.18 * (tau * 220.0 * t).sin()
            + 0.12 * am * (tau * 1_318.5 * t + 0.4).sin()
            + 0.06 * (tau * 5_274.0 * t + 1.1).sin() * (tau * 2.1 * t).cos()
            + 0.03 * (tau * 15_870.0 * t).sin()
            + 0.01 * noise();
        let b = 0.16 * (tau * 277.2 * t + 0.7).sin()
            + 0.11 * am * (tau * 1_760.0 * t).sin()
            + 0.05 * (tau * 7_040.0 * t + 0.2).sin()
            + 0.03 * (tau * 19_100.0 * t + 2.0).sin()
            + 0.01 * noise();
        let part = i * 4 / n.max(1);
        l.push(if part == 3 { 0.0 } else { a });
        r.push(if part == 2 { 0.0 } else { b });
    }
    (l, r)
}

/// Run `n_in` input samples and then zeros through `p` until `want` output
/// samples past its latency exist; return those, aligned to the input.
fn convolve(p: &mut dyn DspProcessor, in_l: &[f64], in_r: &[f64], want: usize) -> (Vec<f64>, Vec<f64>) {
    let lat = p.output_latency();
    let total = lat + want;
    let chunk = 32_768usize;
    let mut out_l = vec![0.0f64; total];
    let mut out_r = vec![0.0f64; total];
    let zeros = vec![0.0f64; chunk];
    let mut pos = 0;
    while pos < total {
        let c = chunk.min(total - pos);
        let (a, b): (&[f64], &[f64]) = if pos + c <= in_l.len() {
            (&in_l[pos..pos + c], &in_r[pos..pos + c])
        } else if pos >= in_l.len() {
            (&zeros[..c], &zeros[..c])
        } else {
            // A chunk straddling the end of the input: split it.
            let k = in_l.len() - pos;
            let mut ol = vec![0.0; c];
            let mut or = vec![0.0; c];
            p.process_audio(&in_l[pos..], &in_r[pos..], &mut ol[..k], &mut or[..k], k);
            p.process_audio(&zeros[..c - k], &zeros[..c - k], &mut ol[k..], &mut or[k..], c - k);
            out_l[pos..pos + c].copy_from_slice(&ol);
            out_r[pos..pos + c].copy_from_slice(&or);
            pos += c;
            continue;
        };
        p.process_audio(a, b, &mut out_l[pos..pos + c], &mut out_r[pos..pos + c], c);
        pos += c;
    }
    out_l.drain(..lat);
    out_r.drain(..lat);
    (out_l, out_r)
}

fn dbfs(v: f64) -> f64 {
    20.0 * v.max(1e-300).log10()
}

/// (max |a − b| in dBFS, RMS of a − b in dBFS, share of bit-identical samples),
/// the differences taken at `scale` — the gain the converter applies to this
/// output afterwards (a polyphase phase comes out at 1/L of the signal and
/// is multiplied by L).
fn diff(a: &[f64], b: &[f64], scale: f64) -> (f64, f64, f64) {
    let n = a.len().min(b.len());
    let mut max = 0.0f64;
    let mut sum = 0.0f64;
    let mut same = 0usize;
    for i in 0..n {
        let d = (a[i] - b[i]).abs() * scale;
        max = max.max(d);
        sum += d * d;
        if a[i].to_bits() == b[i].to_bits() {
            same += 1;
        }
    }
    (dbfs(max), dbfs((sum / n.max(1) as f64).sqrt()), same as f64 / n.max(1) as f64)
}

fn write_f64(path: &PathBuf, l: &[f64], r: &[f64]) {
    let mut bytes = Vec::with_capacity(l.len() * 16);
    for i in 0..l.len() {
        bytes.extend_from_slice(&l[i].to_le_bytes());
        bytes.extend_from_slice(&r[i].to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write dump");
}

fn read_f64(path: &PathBuf) -> Option<(Vec<f64>, Vec<f64>)> {
    let bytes = std::fs::read(path).ok()?;
    let mut l = Vec::with_capacity(bytes.len() / 16);
    let mut r = Vec::with_capacity(bytes.len() / 16);
    for c in bytes.chunks_exact(16) {
        l.push(f64::from_le_bytes(c[..8].try_into().unwrap()));
        r.push(f64::from_le_bytes(c[8..].try_into().unwrap()));
    }
    Some((l, r))
}

#[test]
#[ignore]
fn gpu_matches_cpu_on_real_filters() {
    let only: Option<Vec<String>> = std::env::var("AURA_EQ_CASES")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    let dump = std::env::var_os("AURA_GPU_DUMP").map(PathBuf::from);
    let reference = std::env::var_os("AURA_GPU_REF").map(PathBuf::from);
    let no_cpu = std::env::var_os("AURA_EQ_NO_CPU").is_some();
    if let Some(d) = &dump {
        std::fs::create_dir_all(d).expect("dump dir");
    }

    let mut worst = f64::NEG_INFINITY;
    let mut worst_ref = f64::NEG_INFINITY;
    for case in CASES.iter() {
        if let Some(o) = &only {
            if !o.iter().any(|n| n == case.name) {
                continue;
            }
        }
        let path = filter_dir().join(case.file);
        let coeffs = match load_npy_f64(&path.to_string_lossy()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[EQ] {}: {} not loaded ({}) — skipped", case.name, path.display(), e);
                continue;
            }
        };
        let scale = case.l as f64;
        let phases = if case.l == 1 {
            vec![coeffs]
        } else {
            crate::audio::converter::dsp::polyphase::polyphase_decompose(&coeffs, case.l)
        };

        for &ph in case.phases {
            let h = &phases[ph];
            let b = GpuDspProcessor::block_size(h.len());
            let n_in = (case.blocks * b as f64) as usize;
            let want = n_in + h.len().min(case.tail_blocks * b);
            let (in_l, in_r) = signal(n_in);

            let t = Instant::now();
            let mut gpu = match GpuDspProcessor::new_with_coefficients(h, 64) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("[EQ] {} p{}: no GPU ({}) — skipped", case.name, ph, e);
                    continue;
                }
            };
            let k = gpu.num_blocks;
            let (gl, gr) = convolve(&mut gpu, &in_l, &in_r, want);
            drop(gpu);
            let gpu_s = t.elapsed().as_secs_f64();
            let peak = gl.iter().chain(gr.iter()).fold(0.0f64, |m, v| m.max(v.abs()));

            let mut line = format!(
                "[EQ] {} p{}: {} taps, b={} K={} | {} in, {} out compared | GPU {:.1} s | peak {:.1} dBFS",
                case.name, ph, h.len(), b, k, n_in, want, gpu_s, dbfs(peak * scale)
            );

            if !no_cpu {
                let t = Instant::now();
                let mut cpu = CpuDspProcessor::new_with_coefficients(h);
                let (cl, cr) = convolve(&mut cpu, &in_l, &in_r, want);
                let (ml, rl, _) = diff(&gl, &cl, scale);
                let (mr, rr, _) = diff(&gr, &cr, scale);
                line += &format!(
                    " | vs CPU ({:.1} s): L max {:.1} rms {:.1}, R max {:.1} rms {:.1} dBFS",
                    t.elapsed().as_secs_f64(), ml, rl, mr, rr
                );
                worst = worst.max(ml).max(mr);
                assert!(
                    ml < -200.0 && mr < -200.0,
                    "{} phase {}: GPU differs from the CPU reference by {:.1} / {:.1} dBFS",
                    case.name, ph, ml, mr
                );
            }

            let file = format!("{}-p{}.f64", case.name, ph);
            if let Some(d) = &reference {
                match read_f64(&d.join(&file)) {
                    Some((ol, or)) => {
                        let (ml, rl, sl) = diff(&gl, &ol, scale);
                        let (mr, rr, sr) = diff(&gr, &or, scale);
                        line += &format!(
                            " | vs earlier GPU: L max {:.1} rms {:.1} ({:.1}% identical), R max {:.1} rms {:.1} ({:.1}% identical)",
                            ml, rl, sl * 100.0, mr, rr, sr * 100.0
                        );
                        worst_ref = worst_ref.max(ml).max(mr);
                    }
                    None => line += " | no earlier GPU output to compare",
                }
            }
            if let Some(d) = &dump {
                write_f64(&d.join(&file), &gl, &gr);
            }
            eprintln!("{}", line);
        }
    }
    eprintln!(
        "[EQ] worst vs CPU {:.1} dBFS, worst vs earlier GPU {:.1} dBFS",
        worst, worst_ref
    );
    if reference.is_some() {
        assert!(worst_ref < -200.0, "GPU output moved by {:.1} dBFS against the earlier build", worst_ref);
    }
}
