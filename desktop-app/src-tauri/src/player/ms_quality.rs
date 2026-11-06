//! Quality gate of the multistage upsampler's short chains (H1) against today's single stage.
//!
//! Today a blob h (designed at L·fs, every phase type) plays through `Bank`/`PolyStream` at L branches. The
//! multistage path plays its branch 0, h0[k] = h[k·L], at the source rate (L = 1) through the same convolver, then
//! the generated H1 chain (`ms_h1`) through the shared core (`multistage::interp_whole`); the trims (k0 at stage 0,
//! the chain's own) and stage 0's scale (L/Σh of the whole blob) come from `MsPlan`. The outputs are compared sample
//! by sample.
//!
//! Test-only, nothing here is wired into the player or the converter.
//!
//!     AURA_FILTER_DIR=<blobs> AURA_MS_OUT=<jsonl>
//!     AURA_MS_CONFIGS="30M:44100:8:linear,minimum,tfs,hp_min,stream_linear [...]"
//!     [AURA_MS_H1="cascade,direct"] [AURA_MS_SIGNALS="impulse,noise,sweep,tones,imd,music"]
//!     [AURA_MS_MUSIC="a.flac;b.mp3"] [AURA_MS_DERIVE=1: derive a missing TFS blob next to its pair]
//!     cargo test --profile fast --bins ms_quality -- --ignored --nocapture

use super::convolver::{Alignment, Bank, PolyStream, SourceBuf};
use crate::audio::converter::dsp::ms_h1::{self, H1Stage};
use crate::audio::converter::dsp::multistage::{interp_whole, MsPlan};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

fn taps_of(label: &str) -> usize {
    use crate::audio::converter::dsp::filter::{taps_label, TAP_LADDER};
    TAP_LADDER.iter().copied().find(|&t| taps_label(t) == Some(label)).unwrap_or_else(|| panic!("taps label {label}"))
}

/// A whole source through a bank: `len × L` frames, both channels.
fn run(bank: Arc<Bank>, l: Vec<f64>, r: Vec<f64>, rate: u32) -> (Vec<f64>, Vec<f64>) {
    let src = Arc::new(SourceBuf { l, r, rate });
    let mut st = PolyStream::new(bank, src, 0);
    let n = st.total() as usize;
    let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
    let mut off = 0;
    while off < n {
        let got = st.read(&mut a[off..], &mut b[off..]);
        if got == 0 {
            break;
        }
        off += got;
    }
    (a, b)
}

/// The blob a phase plays for `taps` from `src` to `out`, and its trim, as the player chooses them (`chain.rs`);
/// the Hybrid-Phase linear half is `linear` itself.
fn blob_for(phase: &str, taps: usize, src: u32, out: u32) -> Option<(String, Alignment)> {
    use crate::audio::converter::dsp::filter::find_precomputed_filter;
    use crate::audio::converter::dsp::lab::{stream_linear, tfs};
    match phase {
        "linear" => Some((find_precomputed_filter(taps, src, out, "linear_phase")?, Alignment::Linear)),
        "minimum" => Some((find_precomputed_filter(taps, src, out, "minimum_phase")?, Alignment::None)),
        "hp_min" => Some((find_precomputed_filter(taps, src, out, "minimum_phase")?, Alignment::BandWeighted)),
        "tfs" => {
            let path = match tfs::cached(taps, src, out) {
                Some(p) => p,
                None if std::env::var_os("AURA_MS_DERIVE").is_some() => {
                    let cancel = std::sync::atomic::AtomicBool::new(false);
                    tfs::resolve_or_derive(taps, src, out, &cancel).ok()?.to_string_lossy().to_string()
                }
                None => return None,
            };
            Some((path, Alignment::LookAhead(tfs::look_ahead(taps, out))))
        }
        "stream_linear" => stream_linear::cached(taps, src, out).map(|(p, k)| (p, Alignment::LookAhead(k))),
        _ => panic!("phase {phase}"),
    }
}

/// Stage 0 of a plan: branch 0 of today's blob at the source rate through the product convolver, trimmed by the
/// plan's `k0` and scaled by its `scale0` (L/Σh of the whole blob, D5).
fn stage0(h: &[f64], plan: &MsPlan, src: u32) -> Arc<Bank> {
    let h0: Vec<f64> = h.iter().step_by(plan.l).copied().collect();
    let mut bank = Bank::from_coeffs("stage0", &h0, 1, Alignment::LookAhead(plan.k0), src);
    bank.scale = plan.scale0;
    Arc::new(bank)
}

fn db(v: f64) -> f64 {
    20.0 * v.max(1e-300).log10()
}

/// Wall of the played band (Hz): optimize.py's midpoint of [nyq − 2 kHz, nyq], scaled for a hi-res source.
fn wall_hz(src: u32) -> f64 {
    let base = if src % 44_100 == 0 { 44_100.0 } else { 48_000.0 };
    src as f64 / 2.0 - 1000.0 * src as f64 / base
}

/// The test signals at `src`, stereo, short (3 s; music 10 s from the middle), each followed by a quarter second
/// of silence (the chain reads past the end of stage 0's output).
fn signals(which: &[String], src: u32, music: &[String]) -> Vec<(String, Vec<f64>, Vec<f64>)> {
    use rand::{Rng, SeedableRng};
    let fs = src as f64;
    let fc = wall_hz(src);
    let n = 3 * src as usize;
    let tau = std::f64::consts::TAU;
    let tones = |fr: &[f64], a: f64| -> Vec<f64> {
        (0..n).map(|i| fr.iter().map(|f| a * (tau * f * i as f64 / fs).sin()).sum()).collect()
    };
    let mut out = Vec::new();
    for w in which {
        match w.as_str() {
            "impulse" => {
                let mut x = vec![0.0; 2 * src as usize];
                x[src as usize / 2] = 0.5;
                out.push(("impulse".to_string(), x.clone(), x));
            }
            "noise" => {
                let g = |seed: u64| {
                    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
                    (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect::<Vec<f64>>()
                };
                out.push(("noise".to_string(), g(11), g(12)));
            }
            "sweep" => {
                // Log sweep 20 Hz -> 50 Hz under the wall, -6 dBFS.
                let (f1, f2, t) = (20.0f64, fc - 50.0, 3.0f64);
                let k = (f2 / f1).ln();
                let x: Vec<f64> =
                    (0..n).map(|i| 0.5 * (tau * f1 * t / k * ((i as f64 / fs / t * k).exp() - 1.0)).sin()).collect();
                out.push(("sweep".to_string(), x.clone(), x));
            }
            "tones" => {
                // Four tones from 2 kHz under the wall up to 10 Hz under it.
                let x = tones(&[fc - 2050.0, fc - 1050.0, fc - 50.0, fc - 10.0], 0.2);
                out.push(("tones".to_string(), x.clone(), x));
            }
            "imd" => {
                // CCIF pair (19 + 20 kHz at 44.1k), -8 dBFS each.
                let x = tones(&[fc - 2050.0, fc - 1050.0], 0.4);
                out.push(("imd".to_string(), x.clone(), x));
            }
            "music" => {
                for p in music {
                    let Ok(a) = crate::audio::converter::decode::decode_file(std::path::Path::new(p)) else {
                        println!("[ms] cannot decode {p}");
                        continue;
                    };
                    if a.sample_rate != src {
                        continue;
                    }
                    let n = a.samples_l.len().min(10 * src as usize);
                    let from = (a.samples_l.len() - n) / 2;
                    let name = std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().to_string());
                    out.push((
                        format!("music:{}", name.unwrap_or_default()),
                        a.samples_l[from..from + n].to_vec(),
                        a.samples_r[from..from + n].to_vec(),
                    ));
                }
            }
            other => panic!("signal {other}"),
        }
    }
    for s in out.iter_mut() {
        s.1.extend(std::iter::repeat_n(0.0, src as usize / 4));
        s.2.extend(std::iter::repeat_n(0.0, src as usize / 4));
    }
    out
}

/// Round to an n-bit grid (no dither), clamped as a DAC format would be.
fn quant(v: f64, bits: u32) -> i64 {
    let s = (1i64 << (bits - 1)) as f64;
    (v * s).round().clamp(-s, s - 1.0) as i64
}

/// Today's output `y` against the multistage `w` over the first `n` frames.
#[derive(Default, Clone, Copy)]
struct Cmp {
    null_peak: f64,
    null_rms_re_signal_db: f64,
    diff24: u64,
    max24: i64,
    diff16: u64,
    max16: i64,
    samples: usize,
    true_peak_delta_db: f64,
    best_shift: i64,
}

fn compare(y: &[f64], w: &[f64], n: usize) -> Cmp {
    let (mut pk, mut ss, mut rs, mut tp_y, mut tp_w) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut c = Cmp { samples: n, ..Cmp::default() };
    for i in 0..n {
        let e = w[i] - y[i];
        pk = pk.max(e.abs());
        ss += e * e;
        rs += y[i] * y[i];
        tp_y = tp_y.max(y[i].abs());
        tp_w = tp_w.max(w[i].abs());
        let q = quant(w[i], 24) - quant(y[i], 24);
        if q != 0 {
            c.diff24 += 1;
            c.max24 = c.max24.max(q.abs());
        }
        let q = quant(w[i], 16) - quant(y[i], 16);
        if q != 0 {
            c.diff16 += 1;
            c.max16 = c.max16.max(q.abs());
        }
    }
    // The alignment: the RMS difference at shifts -2..2 frames is least at 0.
    let shift_rms = |s: i64| -> f64 {
        (2..n - 2).map(|i| (w[(i as i64 + s) as usize] - y[i]).powi(2)).sum::<f64>()
    };
    c.best_shift = (-2i64..=2).min_by(|a, b| shift_rms(*a).total_cmp(&shift_rms(*b))).unwrap();
    c.null_peak = db(pk);
    c.null_rms_re_signal_db = db((ss / n as f64).sqrt()) - db((rs / n as f64).sqrt());
    c.true_peak_delta_db = db(tp_w) - db(tp_y);
    c
}

impl Cmp {
    fn json(&self) -> String {
        format!(
            "{{\"null_peak_dbfs\":{:.2},\"null_rms_re_signal_db\":{:.2},\"diff24_samples\":{},\"diff24_max_lsb\":{},\
             \"diff16_samples\":{},\"diff16_max_lsb\":{},\"samples\":{},\"true_peak_delta_db\":{:.3e},\"best_shift\":{}}}",
            self.null_peak,
            self.null_rms_re_signal_db,
            self.diff24,
            self.max24,
            self.diff16,
            self.max16,
            self.samples,
            self.true_peak_delta_db,
            self.best_shift
        )
    }

    /// The worse of two results, counts added.
    fn worst(self, o: Cmp) -> Cmp {
        Cmp {
            null_peak: self.null_peak.max(o.null_peak),
            null_rms_re_signal_db: self.null_rms_re_signal_db.max(o.null_rms_re_signal_db),
            diff24: self.diff24 + o.diff24,
            max24: self.max24.max(o.max24),
            diff16: self.diff16 + o.diff16,
            max16: self.max16.max(o.max16),
            samples: self.samples + o.samples,
            true_peak_delta_db: if self.true_peak_delta_db.abs() >= o.true_peak_delta_db.abs() {
                self.true_peak_delta_db
            } else {
                o.true_peak_delta_db
            },
            best_shift: if self.best_shift != 0 { self.best_shift } else { o.best_shift },
        }
    }
}

#[test]
#[ignore]
fn ms_quality() {
    let Ok(cfgs) = std::env::var("AURA_MS_CONFIGS") else {
        println!("[ms] AURA_MS_CONFIGS not set - skip");
        return;
    };
    let out_path = std::env::var("AURA_MS_OUT").expect("AURA_MS_OUT");
    let env_list = |k: &str, d: &str, sep: char| -> Vec<String> {
        std::env::var(k).unwrap_or_else(|_| d.into()).split(sep).filter(|s| !s.is_empty()).map(String::from).collect()
    };
    let kinds = env_list("AURA_MS_H1", "cascade,direct", ',');
    let sig_list = env_list("AURA_MS_SIGNALS", "impulse,noise,sweep,tones,imd,music", ',');
    let music = env_list("AURA_MS_MUSIC", "", ';');
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&out_path).expect("AURA_MS_OUT");

    for cfg in cfgs.split(' ').filter(|c| !c.is_empty()) {
        let parts: Vec<&str> = cfg.split(':').collect();
        let (label, src, l) = (parts[0], parts[1].parse::<u32>().unwrap(), parts[2].parse::<usize>().unwrap());
        let taps = taps_of(label);
        let rate = src * l as u32;
        let base = if src % 44_100 == 0 { 44_100 } else { 48_000 };
        let sigs = signals(&sig_list, src, &music);
        // One chain per kind; the one-stage x2 chain is the half-band itself, run once.
        let mut chains: Vec<(&str, &[H1Stage])> = Vec::new();
        for (k, c) in ms_h1::candidates(base, l) {
            if kinds.iter().any(|x| x == k) && !chains.iter().any(|(_, d)| std::ptr::eq(d[0].taps, c[0].taps) && d.len() == c.len()) {
                chains.push((k, c));
            }
        }
        for phase in parts[3].split(',') {
            let Some((path, align)) = blob_for(phase, taps, src, rate) else {
                println!("[ms] {label} {src}x{l} {phase}: no blob - skip");
                continue;
            };
            let t0 = Instant::now();
            let h = crate::audio::dsp_core::load_npy_f64(&path).expect("blob");
            let reference = Arc::new(Bank::from_coeffs(&path, &h, l, align, rate));
            let d = reference.delay;
            println!("[ms] {label} {src}x{l} {phase}: {path}, trim {d}, bank {:.1} s", t0.elapsed().as_secs_f64());
            let refs: Vec<(Vec<f64>, Vec<f64>)> =
                sigs.iter().map(|(_, a, b)| run(reference.clone(), a.clone(), b.clone(), src)).collect();
            let sum_h: f64 = h.iter().sum();
            for (kind, chain) in &chains {
                let plan = MsPlan::new(d, l, sum_h, chain);
                let bank0 = stage0(&h, &plan, src);
                let h1_taps: Vec<usize> = chain.iter().map(|s| s.taps.len()).collect();
                let mut all: Option<Cmp> = None;
                for ((name, xl, xr), (yl, yr)) in sigs.iter().zip(refs.iter()) {
                    let t = Instant::now();
                    let (zl, zr) = run(bank0.clone(), xl.clone(), xr.clone(), src);
                    // The signal's own length; the silence after it only feeds the chain.
                    let n = (xl.len() - src as usize / 4) * l;
                    let (wl, wr) = (interp_whole(&plan, &zl, n), interp_whole(&plan, &zr, n));
                    let (cl, cr) = (compare(yl, &wl, n), compare(yr, &wr, n));
                    writeln!(
                        out,
                        "{{\"cfg\":\"{label}:{src}x{l}\",\"phase\":\"{phase}\",\"blob\":\"{}\",\"trim\":{d},\
                         \"h1\":\"{kind}\",\"h1_taps\":{h1_taps:?},\"k0\":{},\"signal\":\"{name}\",\"left\":{},\"right\":{}}}",
                        path.replace('\\', "/"),
                        plan.k0,
                        cl.json(),
                        cr.json()
                    )
                    .unwrap();
                    println!("[ms]   {kind} {name}: null {:.2} / {:.2} dBFS ({:.1} s)", cl.null_peak, cr.null_peak, t.elapsed().as_secs_f64());
                    let w = cl.worst(cr);
                    all = Some(all.map_or(w, |a| a.worst(w)));
                }
                if let Some(a) = all {
                    println!("[ms] GATE {label}:{src}x{l} {phase} {kind} {h1_taps:?} k0 {}: {}", plan.k0, a.json());
                }
            }
        }
    }
}

/// Multi from branch-0 rungs (`fir_<TAG>_<rate>_<phase>.b0.npy`, 300M+) that have no whole blob to compare with: stage 0
/// is built as the converter builds it (`ms_route::MsStage0::new`, the first cascade chain), every output is checked for
/// NaN and level, and each config is nulled against the first config of the list at the same rate and phase (a longer
/// rung against a shorter one differs only near the wall).
///
///     AURA_FILTER_DIR=<b0 dir> AURA_MS_B0="300M:44100:8:linear,minimum 1G:44100:8:linear,minimum"
///     [AURA_MS_SIGNALS=...] cargo test --profile fast --bins ms_b0 -- --ignored --nocapture
#[test]
#[ignore]
fn ms_b0() {
    use crate::audio::converter::dsp::filter::find_precomputed_filter;
    use crate::audio::converter::pipeline::ms_route::{MsStage0, SYMMETRY_TAPS};
    use crate::audio::dsp_core::{branch0_tail, load_npy_f64_branch};
    let Ok(cfgs) = std::env::var("AURA_MS_B0") else {
        println!("[b0] AURA_MS_B0 not set - skip");
        return;
    };
    let sig_list: Vec<String> = std::env::var("AURA_MS_SIGNALS")
        .unwrap_or_else(|_| "impulse,noise,sweep,tones,imd".into())
        .split(',')
        .map(String::from)
        .collect();
    let mut firsts: Vec<(String, Vec<(Vec<f64>, Vec<f64>)>)> = Vec::new();
    for cfg in cfgs.split(' ').filter(|c| !c.is_empty()) {
        let parts: Vec<&str> = cfg.split(':').collect();
        let (label, src, l) = (parts[0], parts[1].parse::<u32>().unwrap(), parts[2].parse::<usize>().unwrap());
        let (taps, rate) = (taps_of(label), src * l as u32);
        let base = if src % 44_100 == 0 { 44_100 } else { 48_000 };
        let chain = ms_h1::candidates(base, l).into_iter().find(|(k, _)| *k == "cascade").expect("cascade chain").1;
        let sigs = signals(&sig_list, src, &[]);
        for phase in parts[3].split(',') {
            let path = find_precomputed_filter(taps, src, rate, &format!("{phase}_phase")).expect("b0 file");
            let t0 = Instant::now();
            let b = load_npy_f64_branch(&path, l, 0).expect("branch 0");
            let tail = branch0_tail(&path, SYMMETRY_TAPS.min(b.full_len / 2)).expect("tail");
            let ms = MsStage0::new(b, &tail, None, l, chain, None, false);
            let mut bank = Bank::from_coeffs("stage0", &ms.branch, 1, Alignment::LookAhead(ms.plan.k0), src);
            bank.scale = ms.plan.scale0;
            let bank = Arc::new(bank);
            println!("[b0] {label} {src}x{l} {phase}: {path}, d {} k0 {}, bank {:.1} s", ms.plan.d, ms.plan.k0, t0.elapsed().as_secs_f64());
            let key = format!("{src}x{l}:{phase}");
            let outs: Vec<(Vec<f64>, Vec<f64>)> = sigs
                .iter()
                .map(|(_, xl, xr)| {
                    let (zl, zr) = run(bank.clone(), xl.clone(), xr.clone(), src);
                    let n = (xl.len() - src as usize / 4) * l;
                    (interp_whole(&ms.plan, &zl, n), interp_whole(&ms.plan, &zr, n))
                })
                .collect();
            let first = firsts.iter().find(|(k, _)| *k == key).map(|(_, o)| o);
            for (i, ((name, _, _), (wl, wr))) in sigs.iter().zip(outs.iter()).enumerate() {
                let nan = wl.iter().chain(wr.iter()).filter(|v| !v.is_finite()).count();
                let peak = wl.iter().chain(wr.iter()).fold(0.0f64, |a, v| a.max(v.abs()));
                let null = first.map(|o| {
                    let n = wl.len().min(o[i].0.len());
                    compare(&o[i].0, wl, n).worst(compare(&o[i].1, wr, n)).json()
                });
                println!("[b0]   {name}: NaN {nan}, peak {:.2} dBFS, vs first {}", db(peak), null.unwrap_or_else(|| "-".into()));
                assert_eq!(nan, 0, "{label} {phase} {name}: NaN in the output");
            }
            if first.is_none() {
                firsts.push((key, outs));
            }
        }
    }
}
