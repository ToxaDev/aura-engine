//! The multistage upsampler in the player: stage 0 (`Bank::stage0`, branch 0 of the long filter at the source rate
//! through `PolyStream`) and the short chain after it (`stages::InterpStage`) against the converter's two-stage pass,
//! bit for bit from any start, on a file, a growing file and a live stream; and against today's single stage, to
//! within the short chain's stopband.
//!
//! The short chains here are designed at run time (Kaiser, flat to 19.5 kHz, 222 dB down from fs − 19.5 kHz), the
//! blobs are today's kind at R = L·fs (linear phase, flat to 17 kHz, 255 dB down from 19 kHz).

use std::sync::{Arc, Mutex, OnceLock};

use super::convolver::{Alignment, Bank, Input, PolyStream, SourceBuf, BLOCK};
use super::stages::{InterpStage, Stage};
use crate::audio::converter::dsp::ms_h1::H1Stage;
use crate::audio::converter::dsp::multistage::{interp_whole, MsPlan};

const FS: f64 = 44_100.0;
const PASS: f64 = 19_500.0;
const A_H1: f64 = 222.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q, mut k) = (1.0, 1.0, x * x / 4.0, 1.0);
    while term > sum * 1e-18 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Kaiser-windowed sinc, odd `taps`, cutoff `fc` (cycles per sample), DC gain `gain`; exactly symmetric, a
/// half-band (`fc` = 0.25) with exact zeros.
fn kaiser_lp(taps: usize, fc: f64, beta: f64, gain: f64) -> Vec<f64> {
    let m = (taps - 1) / 2;
    let i0b = bessel_i0(beta);
    let mut h = vec![0.0f64; taps];
    for k in 0..=m {
        let v = if k == 0 {
            2.0 * fc
        } else if fc == 0.25 && k % 2 == 0 {
            0.0
        } else {
            let x = std::f64::consts::PI * 2.0 * fc * k as f64;
            2.0 * fc * x.sin() / x
        };
        let r = k as f64 / m as f64;
        let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        h[m + k] = v * w;
        h[m - k] = v * w;
    }
    let s: f64 = h.iter().sum();
    for v in h.iter_mut() {
        *v *= gain / s;
    }
    h
}

/// A Kaiser design at `rate`: flat to `fp`, `a_db` down from `fstop`.
fn design(rate: f64, fp: f64, fstop: f64, a_db: f64, gain: f64, halfband: bool) -> Vec<f64> {
    let beta = 0.1102 * (a_db - 8.7);
    let dw = 2.0 * std::f64::consts::PI * (fstop - fp) / rate;
    let n = ((a_db - 7.95) / (2.285 * dw) + 1.0).ceil() as usize;
    let n = if halfband { 4 * n.max(3).div_ceil(4) + 3 } else { n | 1 };
    kaiser_lp(n, if halfband { 0.25 } else { (fp + fstop) / 2.0 / rate }, beta, gain)
}

fn leak(stages: Vec<(usize, Vec<f64>)>) -> &'static [H1Stage] {
    let v: Vec<H1Stage> =
        stages.into_iter().map(|(ratio, taps)| H1Stage { ratio, taps: Box::leak(taps.into_boxed_slice()) }).collect();
    Box::leak(v.into_boxed_slice())
}

/// The short chain at ×`l`: a half-band (×2); two half-bands (×4); a half-band and a ×4 polyphase stage (×8).
pub(crate) fn test_chain(l: usize) -> &'static [H1Stage] {
    static C: OnceLock<Vec<(usize, &'static [H1Stage])>> = OnceLock::new();
    let all = C.get_or_init(|| {
        let hb1 = || design(2.0 * FS, PASS, FS - PASS, A_H1, 2.0, true);
        vec![
            (2, leak(vec![(2, hb1())])),
            (4, leak(vec![(2, hb1()), (2, design(4.0 * FS, PASS, 2.0 * FS - PASS, A_H1, 2.0, true))])),
            (8, leak(vec![(2, hb1()), (4, design(8.0 * FS, PASS, 2.0 * FS - (FS - PASS), A_H1, 4.0, false))])),
        ]
    });
    all.iter().find(|(r, _)| *r == l).expect("a test chain for this ratio").1
}

/// Today's kind of blob at R = L·fs: linear phase, flat to 17 kHz, 255 dB down from 19 kHz.
pub(super) fn blob_linear(l: usize) -> Vec<f64> {
    static B: OnceLock<Mutex<Vec<(usize, Arc<Vec<f64>>)>>> = OnceLock::new();
    let mut g = B.get_or_init(Default::default).lock().unwrap();
    if let Some((_, b)) = g.iter().find(|(k, _)| *k == l) {
        return b.to_vec();
    }
    let b = Arc::new(design(l as f64 * FS, 17_000.0, 19_000.0, 255.0, 1.0, false));
    g.push((l, b.clone()));
    b.to_vec()
}

pub(super) fn noise(n: usize, seed: u64) -> Vec<f64> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    (0..n).map(|_| rng.gen_range(-0.9..0.9)).collect()
}

/// A long noise filter (several partitions in branch 0 at L = 1), symmetric when `sym`.
pub(super) fn noise_filter(taps: usize, seed: u64, sym: bool) -> Vec<f64> {
    let mut h: Vec<f64> = noise(taps, seed).iter().map(|v| v * 1e-3).collect();
    if sym {
        for i in 0..taps / 2 {
            h[taps - 1 - i] = h[i];
        }
    }
    h
}

/// The converter's two-stage pass on a small case: stage 0 = one `CpuDspProcessor` on branch 0 (L = 1), the whole
/// input then its flush, gain `scale0`, trim its latency + k0, as many frames as the chain reads; then the chain
/// over the whole of it (`interp_whole`). `len × L` frames.
fn converter_two_stage(coeffs: &[f64], plan: &MsPlan, x_l: &[f64], x_r: &[f64]) -> (Vec<f64>, Vec<f64>) {
    use crate::audio::dsp_core::CpuDspProcessor;
    use crate::audio::processor::DspProcessor;
    let h0: Vec<f64> = coeffs.iter().step_by(plan.l).copied().collect();
    let n_in = x_l.len();
    let out_len = n_in * plan.l;
    let frames = plan.stage0_frames(out_len as u64) as usize;
    let ola = CpuDspProcessor::output_latency_for(h0.len());
    let per = ola + plan.k0 + frames;
    let mut zl = vec![0.0; per];
    let mut zr = vec![0.0; per];
    let mut dsp = CpuDspProcessor::new_with_coefficients(&h0);
    let chunk = 32768;
    let (mut ol, mut or) = (vec![0.0; chunk], vec![0.0; chunk]);
    let mut pos = 0;
    while pos < per {
        let end = (pos + chunk).min(per);
        let a = end - pos;
        let il: Vec<f64> = (pos..end).map(|i| if i < n_in { x_l[i] } else { 0.0 }).collect();
        let ir: Vec<f64> = (pos..end).map(|i| if i < n_in { x_r[i] } else { 0.0 }).collect();
        dsp.process_audio(&il, &ir, &mut ol[..a], &mut or[..a], a);
        for i in 0..a {
            zl[pos + i] = ol[i] * plan.scale0;
            zr[pos + i] = or[i] * plan.scale0;
        }
        pos = end;
    }
    zl.drain(..ola + plan.k0);
    zr.drain(..ola + plan.k0);
    (interp_whole(plan, &zl, out_len), interp_whole(plan, &zr, out_len))
}

/// The plan and the stage-0 bank for `coeffs` at ×`l`, trimmed `d` today.
pub(super) fn stage0_bank(coeffs: &[f64], l: usize, d: usize) -> Arc<Bank> {
    let plan = MsPlan::new(d, l, coeffs.iter().sum(), test_chain(l));
    let h0: Vec<f64> = coeffs.iter().step_by(l).copied().collect();
    Arc::new(Bank::stage0("test", &h0, coeffs.len(), plan))
}

/// The player's multistage stream from output `start`: stage 0 from where the chain reads, then the chain.
pub(super) fn multi(bank: &Arc<Bank>, input: Input, start: u64, head: bool) -> InterpStage {
    let plan = bank.ms.clone().expect("a stage-0 bank");
    let total = input.total(plan.l);
    let s0 = InterpStage::stage0_start(&plan, start);
    let up = if head { PolyStream::with_head(bank.clone(), input, s0) } else { PolyStream::with_input(bank.clone(), input, s0) };
    InterpStage::new(Box::new(up), &plan, start, total)
}

/// `want` frames of `st`, read in odd, growing pieces (block crossings at every offset).
pub(super) fn read_odd(st: &mut dyn Stage, want: usize) -> (Vec<f64>, Vec<f64>) {
    let (mut l, mut r) = (vec![0.0; want], vec![0.0; want]);
    let mut off = 0;
    let mut step = 7_777;
    while off < want {
        let e = (off + step).min(want);
        st.read(&mut l[off..e], &mut r[off..e]);
        off = e;
        step = step * 3 / 2 + 1;
    }
    (l, r)
}

pub(super) fn assert_bits(what: &str, got: (&[f64], &[f64]), want: (&[f64], &[f64])) {
    assert_eq!(got.0.len(), want.0.len(), "{what}: length");
    for i in 0..got.0.len() {
        assert!(
            got.0[i].to_bits() == want.0[i].to_bits() && got.1[i].to_bits() == want.1[i].to_bits(),
            "{what} idx {i}: ({}, {}) != ({}, {})",
            got.0[i], got.1[i], want.0[i], want.1[i]
        );
    }
}

pub(super) fn peak_db(a: &[f64], b: &[f64]) -> f64 {
    let e = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max);
    20.0 * e.max(1e-300).log10()
}

/// Every trim the player plays: linear (several partitions in branch 0), TFS's look-ahead, minimum phase (none, a
/// filter shorter than a block at ×8), the band-weighted half of a real minimum-phase blob.
fn cases() -> Vec<(&'static str, Vec<f64>, usize, usize)> {
    let lin = noise_filter(2 * BLOCK * 4 + 1001, 7, true);
    let tfs = noise_filter(2 * BLOCK * 4 + 1001, 11, false);
    let min: Vec<f64> = noise(9_001, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
    let bw_blob = crate::audio::dsp_core::to_minimum_phase(&blob_linear(8));
    let bw = crate::audio::dsp_core::estimate_band_weighted_group_delay(&bw_blob, 8.0 * FS, 200.0, 6000.0);
    vec![
        ("linear x4", lin.clone(), 4, (lin.len() - 1) / 2),
        ("tfs x4", tfs, 4, 10_584),
        ("minimum x8", min, 8, 0),
        ("band-weighted x2", crate::audio::dsp_core::to_minimum_phase(&blob_linear(2)), 2, {
            let m = crate::audio::dsp_core::to_minimum_phase(&blob_linear(2));
            crate::audio::dsp_core::estimate_band_weighted_group_delay(&m, 2.0 * FS, 200.0, 6000.0)
        }),
        ("band-weighted x8", bw_blob, 8, bw),
    ]
}

/// Multi mode plays the converter's two-stage pass bit for bit from five starts (the track start, mid-block, a
/// block boundary of the output, a later odd frame, near the end): seek and play give the same numbers.
#[test]
fn multi_plays_the_converters_two_stage_pass_bit_for_bit_from_any_start() {
    let n_in = 3 * BLOCK + 999;
    let x_l = noise(n_in, 1);
    let x_r = noise(n_in, 2);
    let src = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
    for (name, coeffs, l, d) in cases() {
        let bank = stage0_bank(&coeffs, l, d);
        let plan = bank.ms.clone().unwrap();
        let (ref_l, ref_r) = converter_two_stage(&coeffs, &plan, &x_l, &x_r);
        let total = ref_l.len();
        assert_eq!(total, n_in * l);
        for s in [0usize, 12_345, BLOCK * l, BLOCK * l * 2 + 7, total - 5_000] {
            let mut st = multi(&bank, Input::File(src.clone()), s as u64, false);
            assert_eq!(st.total(), total as u64);
            let (gl, gr) = read_odd(&mut st, total - s);
            assert_bits(&format!("{name} k0 {} start {s}", plan.k0), (&gl, &gr), (&ref_l[s..], &ref_r[s..]));
            // Past the end: silence, nothing inside the track.
            let (mut tl, mut tr) = (vec![1.0; 100], vec![1.0; 100]);
            assert_eq!(st.read(&mut tl, &mut tr), 0);
            assert!(tl.iter().chain(&tr).all(|&v| v == 0.0));
        }
    }
}

/// Multi against today's single stage on today's kind of blobs (linear, TFS's look-ahead, minimum, band-weighted),
/// from the track start and from a seek: the short chain's stopband, nothing else.
#[test]
fn multi_nulls_against_single_below_200_db() {
    let n_in = 8_820;
    let x_l: Vec<f64> = noise(n_in, 21).iter().map(|v| v * 0.5).collect();
    let x_r: Vec<f64> = noise(n_in, 22).iter().map(|v| v * 0.5).collect();
    let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
    for l in [2usize, 8] {
        let lin = blob_linear(l);
        let min = crate::audio::dsp_core::to_minimum_phase(&lin);
        let k_tfs = (lin.len() - 1) / 2 * 3 / 4 | 1;
        let phases: [(&str, &[f64], Alignment, f64); 4] = [
            ("linear", &lin, Alignment::Linear, -200.0),
            ("tfs", &lin, Alignment::LookAhead(k_tfs), -203.0),
            ("minimum", &min, Alignment::None, -200.0),
            ("band-weighted", &min, Alignment::BandWeighted, -200.0),
        ];
        let out_rate = 44_100 * l as u32;
        for (name, h, align, limit) in phases {
            let single = Arc::new(Bank::from_coeffs("single", h, l, align, out_rate));
            let bank = stage0_bank(h, l, single.delay);
            let total = n_in * l;
            for s in [0usize, total / 3 + 5] {
                let (al, ar) = read_odd(&mut PolyStream::new(single.clone(), src.clone(), s as u64), total - s);
                let (bl, br) = read_odd(&mut multi(&bank, Input::File(src.clone()), s as u64, false), total - s);
                let db = peak_db(&al, &bl).max(peak_db(&ar, &br));
                println!("[ms] null x{l} {name} start {s}: {db:.1} dBFS");
                assert!(db <= limit, "x{l} {name} start {s}: {db:.1} dBFS > {limit}");
            }
        }
    }
}

/// The shipped chains (`multistage_chain`, 44.1 kHz family ×2…×16): the converter's two-stage pass bit for bit from
/// a seek, and the null against today's single stage on a linear blob.
#[test]
fn the_shipped_chains_play_the_converters_pass_and_null_against_single() {
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    let n_in = 8_820;
    let x_l: Vec<f64> = noise(n_in, 71).iter().map(|v| v * 0.5).collect();
    let x_r: Vec<f64> = noise(n_in, 72).iter().map(|v| v * 0.5).collect();
    let src = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
    for l in [2usize, 4, 8, 16] {
        let chain = multistage_chain(UpsamplerMode::Multi, 30_000_000, 44_100, l).expect("a shipped chain");
        let h = blob_linear(l);
        let d = (h.len() - 1) / 2;
        let plan = MsPlan::new(d, l, h.iter().sum(), chain);
        let h0: Vec<f64> = h.iter().step_by(l).copied().collect();
        let bank = Arc::new(Bank::stage0("shipped", &h0, h.len(), plan.clone()));
        let (ref_l, ref_r) = converter_two_stage(&h, &plan, &x_l, &x_r);
        let total = n_in * l;
        let s = total / 3 + 5;
        let (gl, gr) = read_odd(&mut multi(&bank, Input::File(src.clone()), s as u64, false), total - s);
        assert_bits(&format!("shipped x{l} start {s}"), (&gl, &gr), (&ref_l[s..], &ref_r[s..]));
        let single = Arc::new(Bank::from_coeffs("single", &h, l, Alignment::Linear, 44_100 * l as u32));
        let (al, ar) = read_odd(&mut PolyStream::new(single, src.clone(), 0), total);
        let db = peak_db(&al, &ref_l).max(peak_db(&ar, &ref_r));
        println!("[ms] shipped x{l}: null {db:.1} dBFS ({} stages, k0 {})", chain.len(), plan.k0);
        assert!(db <= -200.0, "shipped x{l}: {db:.1} dBFS");
    }
}

/// The player in Multi mode against the converter's own Multi route (`ms_route::render_in_ram`, stage 0 read off
/// the same `.npy` as both read it): bit for bit from several starts, linear (×8, several partitions in branch 0),
/// minimum (×4) and TFS (×8), on the shipped chains.
#[test]
fn player_multi_is_the_converters_multi_bit_for_bit() {
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    use crate::audio::converter::pipeline::ms_route::{render_in_ram, MsStage0, SYMMETRY_TAPS};
    use crate::audio::converter::state::FileConvState;
    let dir = std::env::temp_dir().join(format!("aura-ms-cross-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let n_in = 2 * BLOCK + 3_333;
    let (x_l, x_r) = (noise(n_in, 81), noise(n_in, 82));
    let src = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
    let lin = noise_filter(2 * BLOCK * 8 + 4_001, 83, true);
    let min = crate::audio::dsp_core::to_minimum_phase(&blob_linear(4));
    let tfs = noise_filter(BLOCK * 8 + 901, 84, false);
    let cases: [(&str, &[f64], usize, Alignment, bool); 3] = [
        ("linear", &lin, 8, Alignment::Linear, false),
        ("minimum", &min, 4, Alignment::None, false),
        ("tfs", &tfs, 8, Alignment::LookAhead(crate::audio::converter::dsp::lab::tfs::look_ahead(tfs.len(), 352_800)), true),
    ];
    for (name, h, l, align, is_tfs) in cases {
        let out_rate = 44_100 * l as u32;
        let chain = multistage_chain(UpsamplerMode::Multi, 30_000_000, 44_100, l).expect("a shipped chain");
        let path = dir.join(format!("{name}.npy"));
        crate::audio::dsp_core::save_npy_f64(&path, h).unwrap();
        let p = path.to_string_lossy().to_string();
        // The converter, as `ms_route::prepare` reads the blob.
        let b = crate::audio::dsp_core::load_npy_f64_branch(&p, l, 0).unwrap();
        let tail = h[h.len() - SYMMETRY_TAPS.min(h.len() / 2)..].to_vec();
        let ahead = is_tfs.then(|| crate::audio::converter::dsp::lab::tfs::look_ahead(b.full_len, out_rate));
        let ms = MsStage0::new(b, &tail, ahead, l, chain, None, false);
        let fs = Arc::new(FileConvState::new());
        let (cl, cr) = render_in_ram(&ms, &x_l, &x_r, n_in, n_in * l, false, 64, "cross", &fs, 0, 600).unwrap();
        // The player, as `phase_stage` loads it.
        let bank = Arc::new(Bank::load_stage0(&p, l, align, out_rate, chain).unwrap());
        let plan = bank.ms.clone().unwrap();
        assert_eq!((plan.d, plan.k0), (ms.plan.d, ms.plan.k0), "{name}: the same plan");
        let total = n_in * l;
        assert_eq!(cl.len(), total);
        for s in [0usize, 9_999, BLOCK * l + 1, total - 3_000] {
            let (gl, gr) = read_odd(&mut multi(&bank, Input::File(src.clone()), s as u64, false), total - s);
            assert_bits(&format!("{name} x{l} start {s}"), (&gl, &gr), (&cl[s..], &cr[s..]));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file whose source stages are still running plays what the whole file plays, bit for bit.
#[test]
fn multi_on_a_growing_file_is_the_whole_files_bit_for_bit() {
    let n_in = 2 * BLOCK + 4_321;
    let src = Arc::new(SourceBuf { l: noise(n_in, 31), r: noise(n_in, 32), rate: 44_100 });
    let coeffs = noise_filter(BLOCK * 8 + 77, 33, true);
    let bank = stage0_bank(&coeffs, 8, (coeffs.len() - 1) / 2);
    let total = n_in * 8;
    for s in [0usize, BLOCK * 8 + 3] {
        let grow = Arc::new(crate::player::grow::GrowSource::new(44_100, n_in));
        let (w, s2) = (grow.clone(), src.clone());
        let producer = std::thread::spawn(move || {
            let mut at = 0;
            while at < s2.len() {
                let e = (at + 5_000).min(s2.len());
                w.push(&s2.l[at..e], &s2.r[at..e]);
                at = e;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            w.finish();
        });
        let (fl, fr) = read_odd(&mut multi(&bank, Input::File(src.clone()), s as u64, false), total - s);
        let mut g = multi(&bank, Input::Grow(grow), s as u64, false);
        assert_eq!(g.total(), total as u64);
        let (gl, gr) = read_odd(&mut g, total - s);
        producer.join().unwrap();
        assert_bits(&format!("grow start {s}"), (&gl, &gr), (&fl, &fr));
    }
}

/// A live stream, fed while it plays, plays head and tail in stage 0: bit for bit what the file plays through head
/// and tail, and within f64 rounding (1e-13 of the peak) of the file's whole blocks.
#[test]
fn multi_on_a_live_stream_is_the_files_through_head_and_tail() {
    use super::radio::live::LiveSource;
    let n_in = 2 * BLOCK + 999;
    let (x_l, x_r) = (noise(n_in, 41), noise(n_in, 42));
    let file = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
    let coeffs = noise_filter(BLOCK * 4 + 501, 43, true);
    let bank = stage0_bank(&coeffs, 4, (coeffs.len() - 1) / 2);
    let total = n_in * 4;
    for s in [0usize, BLOCK * 4 + 7] {
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 1.0, 1e6));
        let w = live.clone();
        let (wl, wr) = (x_l.clone(), x_r.clone());
        let writer = std::thread::spawn(move || {
            let mut at = 0;
            let mut step = 5_003;
            while at < wl.len() {
                let e = (at + step).min(wl.len());
                w.push(&wl[at..e], &wr[at..e]);
                at = e;
                step = step * 5 / 4 + 1;
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            w.close();
        });
        let want = total - s;
        let (al, ar) = read_odd(&mut multi(&bank, Input::File(file.clone()), s as u64, false), want);
        let (hl, hr) = read_odd(&mut multi(&bank, Input::File(file.clone()), s as u64, true), want);
        let mut b = multi(&bank, Input::Live(live.clone()), s as u64, false);
        assert_eq!(b.total(), u64::MAX, "a stream has no end");
        let (bl, br) = read_odd(&mut b, want);
        writer.join().unwrap();
        assert_eq!(live.stats().starves, 0, "the writer kept up");
        assert_bits(&format!("live start {s}"), (&bl, &br), (&hl, &hr));
        let peak = al.iter().chain(&ar).fold(0f64, |p, v| p.max(v.abs()));
        for i in 0..want {
            assert!(
                (al[i] - bl[i]).abs() <= 1e-13 * peak && (ar[i] - br[i]).abs() <= 1e-13 * peak,
                "start {s} idx {i}: live ({}, {}) far from file ({}, {})",
                bl[i], br[i], al[i], ar[i]
            );
        }
    }
}

/// `Bank::load_stage0` reads branch 0 off the file: the same bank `Bank::stage0` builds from the whole filter in
/// memory (gain, trim, plan, partition spectra), today's trims — the band-weighted one measured on the whole filter.
#[test]
fn a_stage0_bank_loaded_from_a_file_is_branch_0_of_the_whole_filter() {
    let dir = std::env::temp_dir().join(format!("aura-ms-player-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let min = crate::audio::dsp_core::to_minimum_phase(&blob_linear(8));
    let lin = noise_filter(BLOCK * 8 * 2 + 9, 51, true);
    for (name, h, align) in [("lin", &lin, Alignment::Linear), ("tfs", &lin, Alignment::LookAhead(1_234)), ("bw", &min, Alignment::BandWeighted)] {
        let path = dir.join(format!("{name}.npy"));
        crate::audio::dsp_core::save_npy_f64(&path, h).unwrap();
        let p = path.to_string_lossy();
        let loaded = Bank::load_stage0(&p, 8, align, 352_800, test_chain(8)).expect("load");
        let single = Bank::from_coeffs(&p, h, 8, align, 352_800);
        let built = stage0_bank(h, 8, single.delay);
        let (a, b) = (loaded.ms.as_ref().unwrap(), built.ms.as_ref().unwrap());
        assert_eq!((a.l, a.d, a.k0, a.c), (b.l, b.d, b.k0, b.c), "{name}: plan");
        assert_eq!(a.d, single.delay, "{name}: today's trim");
        assert_eq!(a.scale0.to_bits(), single.scale.to_bits(), "{name}: the whole filter's L/Σh");
        assert_eq!((loaded.l, loaded.full_len, loaded.delay, loaded.ratio()), (1, h.len(), a.k0, 8), "{name}");
        assert_eq!(loaded.scale.to_bits(), built.scale.to_bits(), "{name}");
        let (sa, sb) = (loaded.branch_spectra(0), built.branch_spectra(0));
        assert_eq!(sa.len(), sb.len());
        for (pa, pb) in sa.iter().zip(sb) {
            assert!(pa.iter().zip(pb).all(|(x, y)| x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits()), "{name}: spectra");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real blobs: player Multi against player Single, 30M ×8 at 44.1 kHz, linear / minimum / TFS (TFS only when
/// its derived blob is already there — this test derives nothing), 1 s of noise from a seek 0.5 s in. The short
/// chain is the shipped one when there is one, else the runtime ×8 test chain.
///
///     AURA_FILTER_DIR=<blobs> cargo test --profile fast --bins multi_against_single_on_the_30m_blobs -- --ignored --nocapture
#[test]
#[ignore]
fn multi_against_single_on_the_30m_blobs() {
    use crate::audio::converter::dsp::filter::find_precomputed_filter;
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    let (taps, l, out_rate) = (30_000_000usize, 8usize, 352_800u32);
    let chain = multistage_chain(UpsamplerMode::Multi, taps, 44_100, l).unwrap_or_else(|| test_chain(l));
    let n_in = 66_150;
    let src = Arc::new(SourceBuf { l: noise(n_in, 61).iter().map(|v| v * 0.5).collect(), r: noise(n_in, 62).iter().map(|v| v * 0.5).collect(), rate: 44_100 });
    let tfs = crate::audio::converter::dsp::lab::tfs::cached(taps, 44_100, out_rate);
    let rows = [
        ("linear", find_precomputed_filter(taps, 44_100, out_rate, "linear_phase"), Alignment::Linear),
        ("minimum", find_precomputed_filter(taps, 44_100, out_rate, "minimum_phase"), Alignment::None),
        ("tfs", tfs, Alignment::LookAhead(crate::audio::converter::dsp::lab::tfs::look_ahead(taps, out_rate))),
    ];
    let start = 22_050 * l as u64;
    let want = (n_in * l) - start as usize;
    for (name, path, align) in rows {
        let Some(path) = path else {
            println!("[ms] 30M x8 {name}: no blob - skip");
            continue;
        };
        let t0 = std::time::Instant::now();
        let single = Arc::new(Bank::load(&path, l, align, out_rate).expect("single bank"));
        let (al, ar) = read_odd(&mut PolyStream::new(single.clone(), src.clone(), start), want);
        drop(single);
        let t1 = std::time::Instant::now();
        let bank = Arc::new(Bank::load_stage0(&path, l, align, out_rate, chain).expect("stage-0 bank"));
        let (bl, br) = read_odd(&mut multi(&bank, Input::File(src.clone()), start, false), want);
        let db = peak_db(&al, &bl).max(peak_db(&ar, &br));
        println!(
            "[ms] 30M x8 {name}: null {db:.1} dBFS (k0 {}, c {}; single {:.1} s, multi {:.1} s)",
            bank.ms.as_ref().unwrap().k0,
            bank.ms.as_ref().unwrap().c,
            (t1 - t0).as_secs_f64(),
            t1.elapsed().as_secs_f64()
        );
    }
}

// -- Stage 0 on the card ---------------------------------------------------------------------------------------------

/// A stage-0 bank holds branch 0 alone (⌈N/L⌉ taps at l = 1, what the card uploads and its VRAM is reckoned from) and
/// runs on past the source end as far as the chain reads, on the CPU's stream and the card's alike (`stream_total`).
#[test]
fn a_stage0_bank_holds_branch_0_and_runs_to_the_chains_end() {
    use super::radio::live::LiveSource;
    let coeffs = noise_filter(BLOCK * 8 * 2 + 9, 71, true);
    let single = Bank::from_coeffs("single", &coeffs, 8, Alignment::Linear, 352_800);
    assert_eq!((single.l, single.full_len, single.taps), (8, coeffs.len(), coeffs.len()));
    let bank = stage0_bank(&coeffs, 8, single.delay);
    assert_eq!((bank.l, bank.full_len, bank.taps), (1, coeffs.len(), coeffs.len().div_ceil(8)));
    assert_eq!(bank.max_partitions(), coeffs.len().div_ceil(8).div_ceil(BLOCK));
    let plan = bank.ms.clone().unwrap();
    let n_in = 3_000;
    let src = Arc::new(SourceBuf { l: noise(n_in, 72), r: noise(n_in, 73), rate: 44_100 });
    let file = Input::File(src.clone());
    assert_eq!(bank.stream_total(&file), plan.stage0_frames((n_in * 8) as u64));
    assert_eq!(PolyStream::new(bank.clone(), src.clone(), 0).total(), bank.stream_total(&file));
    assert_eq!(single.stream_total(&file), (n_in * 8) as u64);
    let live = Input::Live(Arc::new(LiveSource::new(44_100, 1 << 20, 1.0, 1e6)));
    assert_eq!((bank.stream_total(&live), single.stream_total(&live)), (u64::MAX, u64::MAX));
}

/// The card's VRAM for a multistage stage 0: one branch of ⌈N/L⌉ taps at l = 1 — its FDL ring (two channels), its
/// spectra, one accumulator and one staging buffer per channel. The table goes to the report.
#[test]
fn the_card_holds_stage_0_alone() {
    use super::gpu::{partitions, vram_demand, vram_demand_stage0, GPU_BINS};
    let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
    for (taps, l) in [(1_000_000usize, 8usize), (10_000_000, 8), (30_000_000, 2), (30_000_000, 4), (30_000_000, 8), (30_000_000, 16)] {
        let p = partitions(taps.div_ceil(l));
        let stage0 = vram_demand_stage0(taps, l);
        assert_eq!(stage0, ((2 * p + p + 2 + 2) * GPU_BINS * 16) as u64, "{taps} x{l}");
        assert_eq!(stage0, vram_demand(taps.div_ceil(l), 1));
        assert!(stage0 < vram_demand(taps, l), "{taps} x{l}");
        println!("[ms-vram] {taps} x{l}: P {p}, stage 0 {:.1} MiB, Single {:.1} MiB", mib(stage0), mib(vram_demand(taps, l)));
    }
}

/// `stage0_bank` under its own path and chain: the card's bank cache knows a bank by (path, l, taps), so test banks
/// of the same length must not share a name.
fn stage0_bank_named(path: &str, coeffs: &[f64], l: usize, d: usize, chain: &'static [H1Stage]) -> Arc<Bank> {
    let plan = MsPlan::new(d, l, coeffs.iter().sum(), chain);
    let h0: Vec<f64> = coeffs.iter().step_by(l).copied().collect();
    Arc::new(Bank::stage0(path, &h0, coeffs.len(), plan))
}

/// The player's multistage stream with stage 0 on the card (the CPU chain after it), from output `start`.
fn gpu_multi(bank: &Arc<Bank>, src: &Arc<SourceBuf>, start: u64, ctx: Arc<super::gpu::GpuPolyCtx>) -> InterpStage {
    let plan = bank.ms.clone().expect("a stage-0 bank");
    let input = Input::File(src.clone());
    let total = input.total(plan.l);
    let s0 = InterpStage::stage0_start(&plan, start);
    let up = super::gpu::GpuPolyStream::try_with_input(bank.clone(), input, s0, ctx).expect("a GPU is present but it refused stage 0");
    InterpStage::new(Box::new(up), &plan, start, total)
}

/// (peak, RMS) of the difference, dBFS.
fn diff_db(a: (&[f64], &[f64]), b: (&[f64], &[f64])) -> (f64, f64) {
    let (mut peak, mut sq) = (0f64, 0f64);
    for (x, y) in a.0.iter().zip(b.0).chain(a.1.iter().zip(b.1)) {
        let d = (x - y).abs();
        peak = peak.max(d);
        sq += d * d;
    }
    let rms = (sq / (2 * a.0.len()).max(1) as f64).sqrt();
    (20.0 * peak.max(1e-300).log10(), 20.0 * rms.max(1e-300).log10())
}

/// One row: the card's Multi against the CPU's from `start`, `want` frames; RMS ≤ −230 and peak ≤ −200 dBFS.
fn gpu_row(what: &str, bank: &Arc<Bank>, src: &Arc<SourceBuf>, start: u64, want: usize, ctx: &Arc<super::gpu::GpuPolyCtx>) {
    let t0 = std::time::Instant::now();
    let (cl, cr) = read_odd(&mut multi(bank, Input::File(src.clone()), start, false), want);
    let t1 = std::time::Instant::now();
    let mut g = gpu_multi(bank, src, start, ctx.clone());
    let (gl, gr) = read_odd(&mut g, want);
    assert!(!g.is_gpu_failed(), "{what}: the GPU stream failed");
    let (peak, rms) = diff_db((&gl, &gr), (&cl, &cr));
    println!(
        "[ms-gpu] {what} start {start}: GPU vs CPU peak {peak:.1} dBFS, RMS {rms:.1} dBFS (cpu {:.1} s, gpu {:.1} s)",
        (t1 - t0).as_secs_f64(),
        t1.elapsed().as_secs_f64()
    );
    assert!(rms <= -230.0 && peak <= -200.0, "{what} start {start}: peak {peak:.1} / RMS {rms:.1} dBFS");
}

/// Stage 0 on the card + the CPU's short chain against the CPU's Multi, on synthetic filters (linear with several
/// partitions in branch 0, TFS's look-ahead, minimum, band-weighted; ×2/×4/×8) from three starts: the DS
/// arithmetic's error, RMS ≤ −230 dBFS. Skips with no GPU.
///
///     cargo test --profile fast --bins gpu_stage0_matches_cpu_multi_synthetic -- --ignored --nocapture
#[test]
#[ignore]
fn gpu_stage0_matches_cpu_multi_synthetic() {
    let Some(ctx) = super::gpu::GpuPolyCtx::try_build() else {
        println!("[ms-gpu] no GPU - skip");
        return;
    };
    let n_in = 3 * BLOCK + 999;
    let src = Arc::new(SourceBuf { l: noise(n_in, 81), r: noise(n_in, 82), rate: 44_100 });
    for (name, coeffs, l, d) in cases() {
        let bank = stage0_bank_named(&format!("ms-gpu-test {name}"), &coeffs, l, d, test_chain(l));
        let total = n_in * l;
        for s in [0usize, BLOCK * l + 7, total / 2 + 3] {
            gpu_row(name, &bank, &src, s as u64, total - s, &ctx);
        }
    }
}

/// A stage-0 bank on the card over a live stream (the radio's route): the stream takes the bank's real taps (branch
/// 0 at l = 1, never the whole filter), has no end, and plays the CPU's live Multi (head and tail) within the DS
/// tolerance from the start and from a later output frame.
///
///     cargo test --profile fast --bins gpu_stage0_on_a_live_stream -- --ignored --nocapture
#[test]
#[ignore]
fn gpu_stage0_on_a_live_stream() {
    use super::radio::live::LiveSource;
    let Some(ctx) = super::gpu::GpuPolyCtx::try_build() else {
        println!("[ms-gpu] no GPU - skip");
        return;
    };
    let n_in = 2 * BLOCK + 999;
    let (x_l, x_r) = (noise(n_in, 101), noise(n_in, 102));
    let coeffs = noise_filter(BLOCK * 4 * 3 + 501, 103, true);
    let bank = stage0_bank_named("ms-gpu-test live", &coeffs, 4, (coeffs.len() - 1) / 2, test_chain(4));
    assert_eq!((bank.l, bank.taps), (1, coeffs.len().div_ceil(4)));
    let plan = bank.ms.clone().unwrap();
    let live = || {
        let s = Arc::new(LiveSource::new(44_100, 1 << 20, 1.0, 1e6));
        s.push(&x_l, &x_r);
        s.close();
        s
    };
    for s in [0u64, BLOCK as u64 * 4 + 7] {
        let want = n_in * 4 - s as usize;
        let (cl, cr) = read_odd(&mut multi(&bank, Input::Live(live()), s, false), want);
        let up = super::gpu::GpuPolyStream::try_with_input(bank.clone(), Input::Live(live()), InterpStage::stage0_start(&plan, s), ctx.clone())
            .expect("a GPU is present but it refused a live stage 0");
        assert_eq!(Stage::total(&up), u64::MAX, "a stream has no end");
        let mut g = InterpStage::new(Box::new(up), &plan, s, u64::MAX);
        let (gl, gr) = read_odd(&mut g, want);
        assert!(!g.is_gpu_failed(), "start {s}: the GPU stream failed");
        let (peak, rms) = diff_db((&gl, &gr), (&cl, &cr));
        println!("[ms-gpu] live x4 start {s}: GPU vs CPU peak {peak:.1} dBFS, RMS {rms:.1} dBFS");
        assert!(rms <= -230.0 && peak <= -200.0, "live start {s}: peak {peak:.1} / RMS {rms:.1} dBFS");
    }
}

/// The real blobs on the card: 1M and 30M ×8 at 44.1 kHz, linear / minimum / TFS (TFS only when its derived blob
/// is cached), the shipped ×8 chain, 1 s of noise from a seek 0.5 s in; then the VRAM a stage-0 stream takes
/// (DXGI free before and after, bank cache cleared) against `vram_demand_stage0`, for every ratio with a blob.
///
///     AURA_FILTER_DIR=<blobs> cargo test --profile fast --bins gpu_stage0_on_the_real_blobs -- --ignored --nocapture
#[test]
#[ignore]
fn gpu_stage0_on_the_real_blobs() {
    use crate::audio::converter::dsp::filter::find_precomputed_filter;
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    let Some(ctx) = super::gpu::GpuPolyCtx::try_build() else {
        println!("[ms-gpu] no GPU - skip");
        return;
    };
    let n_in = 66_150;
    let src = Arc::new(SourceBuf {
        l: noise(n_in, 91).iter().map(|v| v * 0.5).collect(),
        r: noise(n_in, 92).iter().map(|v| v * 0.5).collect(),
        rate: 44_100,
    });
    let mut measured = 0;
    for taps in [1_000_000usize, 30_000_000] {
        let (l, out_rate) = (8usize, 352_800u32);
        let chain = multistage_chain(UpsamplerMode::Multi, taps, 44_100, l).unwrap_or_else(|| test_chain(l));
        let tfs = crate::audio::converter::dsp::lab::tfs::cached(taps, 44_100, out_rate);
        let rows = [
            ("linear", find_precomputed_filter(taps, 44_100, out_rate, "linear_phase"), Alignment::Linear),
            ("minimum", find_precomputed_filter(taps, 44_100, out_rate, "minimum_phase"), Alignment::None),
            ("tfs", tfs, Alignment::LookAhead(crate::audio::converter::dsp::lab::tfs::look_ahead(taps, out_rate))),
        ];
        let start = 22_050 * l as u64;
        let want = n_in * l - start as usize;
        for (name, path, align) in rows {
            let Some(path) = path else {
                println!("[ms-gpu] {taps} x{l} {name}: no blob - skip");
                continue;
            };
            let bank = Arc::new(Bank::load_stage0(&path, l, align, out_rate, chain).expect("stage-0 bank"));
            gpu_row(&format!("{taps} x{l} {name}"), &bank, &src, start, want, &ctx);
            measured += 1;
        }
    }
    assert!(measured > 0, "a GPU is present but no blob was found - is AURA_FILTER_DIR set?");

    // VRAM of a playing stage-0 stream against the formula.
    let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
    for (taps, l) in [(1_000_000usize, 8usize), (10_000_000, 8), (30_000_000, 2), (30_000_000, 4), (30_000_000, 8), (30_000_000, 16)] {
        let out_rate = 44_100 * l as u32;
        let Some(path) = find_precomputed_filter(taps, 44_100, out_rate, "linear_phase") else {
            println!("[ms-vram] {taps} x{l}: no blob - skip");
            continue;
        };
        let Some(chain) = multistage_chain(UpsamplerMode::Multi, taps, 44_100, l) else {
            println!("[ms-vram] {taps} x{l}: no shipped chain - skip");
            continue;
        };
        let bank = Arc::new(Bank::load_stage0(&path, l, Alignment::Linear, out_rate, chain).expect("stage-0 bank"));
        ctx.clear_bank_cache();
        ctx.device.poll(wgpu::Maintain::Wait);
        let free0 = ctx.free_vram_bytes();
        let st = gpu_multi(&bank, &src, 0, ctx.clone());
        ctx.device.poll(wgpu::Maintain::Wait);
        let free1 = ctx.free_vram_bytes();
        let formula = super::gpu::vram_demand_stage0(bank.full_len, l);
        assert_eq!(formula, super::gpu::vram_demand(bank.taps, bank.l), "{taps} x{l}: the formula is the bank's");
        println!(
            "[ms-vram] {taps} x{l}: stage 0 takes {:.1} MiB (DXGI free {:.0} -> {:.0}); formula {:.1} MiB; Single formula {:.1} MiB",
            mib(free0.saturating_sub(free1)),
            mib(free0),
            mib(free1),
            mib(formula),
            mib(super::gpu::vram_demand(bank.full_len, l))
        );
        drop(st);
    }
    ctx.clear_bank_cache();
}
