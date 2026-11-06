//! Look-ahead modulators: correctness (determinism, chunking, the AVX2 and
//! portable builds agree, latency) and measurements on a reference decoder
//! (a long Kaiser FIR low-pass to 88.2 kHz, then a Kaiser-windowed FFT);
//! levels in dB re PCM full scale. The parameter study and the comparison
//! table are `#[ignore]` tests: run them in an optimised build,
//! `cargo test --profile fast --bins lookahead_study -- --ignored --nocapture`.

use std::f64::consts::PI;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use super::super::{tables::SDM5, Sdm5Encoder};
use crate::audio::dsd::{Search as KindSearch, CATALOG};
use super::*;

/// Power of a full-scale PCM sine in one-bit units.
const PCM_FS_POWER: f64 = 0.125;
const FFT_LEN: usize = 1 << 15;
const DEC_RATE: f64 = 88_200.0;

fn rate(multiple: u32) -> DsdRate {
    DsdRate::new(44_100, multiple).unwrap()
}

fn dop_input(multiple: u32) -> u32 {
    rate(multiple).bit_rate() / 16
}

fn encode(enc: &mut dyn DsdEncoder, pcm: &[f64]) -> Vec<u8> {
    let mut out = vec![0u8; pcm.len() * enc.ratio() / 8];
    enc.process(pcm, &mut out);
    out
}

fn bits(bytes: &[u8]) -> Vec<f64> {
    let mut v = Vec::with_capacity(bytes.len() * 8);
    for &b in bytes {
        for i in (0..8).rev() {
            v.push(if b >> i & 1 == 1 { 1.0 } else { -1.0 });
        }
    }
    v
}

fn sine(fs: f64, f: f64, amp: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| amp * (2.0 * PI * f * i as f64 / fs).sin()).collect()
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    loop {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        if term < sum * 1e-17 {
            return sum;
        }
        k += 1.0;
    }
}

fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    let i0b = bessel_i0(beta);
    (0..n)
        .map(|i| {
            let r = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect()
}

/// One-bit stream at 44.1 kHz × `multiple` → PCM at 88.2 kHz, ~170 dB stop
/// band from 68.2 kHz (nothing aliases into 0–20 kHz).
fn decode(x: &[f64], multiple: u32) -> Vec<f64> {
    let fs = 44_100.0 * multiple as f64;
    let d = (multiple / 2) as usize;
    let fc = 44_100.0;
    let trans = 2.0 * PI * 48_200.0 / fs;
    let n = ((170.0 - 8.0) / (2.285 * trans)).ceil() as usize | 1;
    let w = kaiser(n, 0.1102 * (170.0 - 8.7));
    let mid = (n / 2) as f64;
    let h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - mid;
            let s = if t == 0.0 { 2.0 * fc / fs } else { (2.0 * PI * fc / fs * t).sin() / (PI * t) };
            s * w[i]
        })
        .collect();
    (0..(x.len() - n) / d).map(|m| h.iter().zip(&x[m * d..]).map(|(a, b)| a * b).sum()).collect()
}

fn spectrum(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let w = kaiser(n, 20.0);
    let s2: f64 = w.iter().map(|v| v * v).sum();
    let mut buf: Vec<Complex<f64>> = x.iter().zip(&w).map(|(a, b)| Complex::new(a * b, 0.0)).collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    (0..n / 2).map(|k| buf[k].norm_sqr() / (n as f64 * s2) * if k == 0 { 1.0 } else { 2.0 }).collect()
}

fn db(p: f64) -> f64 {
    10.0 * p.log10()
}

fn bin(f: f64) -> usize {
    (f / DEC_RATE * FFT_LEN as f64).round() as usize
}

fn in_band(spec: &[f64], tone: Option<f64>) -> f64 {
    let skip = tone.map(bin);
    (bin(20.0)..=bin(20_000.0))
        .filter(|&k| skip.map_or(true, |t| k + 12 < t || k > t + 12))
        .map(|k| spec[k])
        .sum()
}

/// The worst bin above −160 dB in 20 Hz–20 kHz by its excess over the median
/// of ±100 bins (the SDM5 tests' idle-tone statistic), dB.
fn idle_spur(spec: &[f64], tone: Option<f64>) -> f64 {
    let mut worst = f64::MIN;
    let mut nb = Vec::with_capacity(256);
    for k in bin(20.0)..=bin(20_000.0) {
        if db(spec[k] / PCM_FS_POWER) < -160.0 || tone.map_or(false, |t| (k as i64 - bin(t) as i64).abs() <= 12) {
            continue;
        }
        nb.clear();
        nb.extend((k.saturating_sub(100)..=(k + 100).min(spec.len() - 1)).map(|j| spec[j]));
        nb.sort_by(|a, b| a.partial_cmp(b).unwrap());
        worst = worst.max(db(spec[k] / nb[nb.len() / 2]));
    }
    worst
}

/// Encodes `pcm(input_rate, n)` and returns the decoded spectrum of the last
/// FFT_LEN samples, and the overload count.
fn decoded_spectrum(
    make: &dyn Fn() -> Box<dyn DsdEncoder>,
    multiple: u32,
    pcm: impl Fn(f64, usize) -> Vec<f64>,
) -> (Vec<f64>, u64) {
    let input_rate = dop_input(multiple);
    let mut enc = make();
    let secs = (FFT_LEN as f64 + 2048.0) / DEC_RATE + 0.04;
    let n = (secs * input_rate as f64) as usize;
    let x = bits(&encode(enc.as_mut(), &pcm(input_rate as f64, n)));
    let y = decode(&x, multiple);
    (spectrum(&y[y.len() - FFT_LEN..]), enc.overloads())
}

/// Suite numbers of one modulator at one rate (dB re PCM full scale; N+D
/// re full scale too, so levels compare across tones).
#[derive(Clone, Copy, Debug)]
struct Numbers {
    silence: f64,
    nd6: f64,
    nd60: f64,
    spur: f64,
    overloads: u64,
}

fn measure(make: &dyn Fn() -> Box<dyn DsdEncoder>, multiple: u32) -> Numbers {
    let (silent, o0) = decoded_spectrum(make, multiple, |_, n| vec![0.0; n]);
    let tone_nd = |amp: f64| {
        let (s, o) = decoded_spectrum(make, multiple, |fs, n| sine(fs, 1000.0, amp, n));
        (db(in_band(&s, Some(1000.0)) / PCM_FS_POWER), idle_spur(&s, Some(1000.0)), o)
    };
    let (nd6, _, o1) = tone_nd(0.5);
    let (nd60, _, o2) = tone_nd(1e-3);
    let (_, spur_low, o3) = tone_nd(1e-6);
    let (dc, o4) = decoded_spectrum(make, multiple, |_, n| vec![2e-3; n]);
    Numbers {
        silence: db(in_band(&silent, None) / PCM_FS_POWER),
        nd6,
        nd60,
        spur: idle_spur(&silent, None).max(spur_low).max(idle_spur(&dc, None)),
        overloads: o0 + o1 + o2 + o3 + o4,
    }
}

/// The largest amplitude (of full modulation, step 0.02) the search runs
/// `secs` without a state reset, for a 1 kHz sine, a 10 kHz sine and DC, at
/// the bit rate directly with the input limit out of the way; every lower
/// step must pass as well.
fn msa(multiple: u32, setup: &LaSetup, secs: f64) -> [f64; 3] {
    let fs = rate(multiple).bit_rate() as f64;
    let n = ((secs * fs) as usize) & !7;
    let open = LaSetup { limit: 1.5, ..*setup };
    let mut found = [0.0; 3];
    for (w, f) in [1000.0, 10_000.0, 0.0].into_iter().enumerate() {
        for step in 20..50 {
            let amp = step as f64 * 0.02;
            let mut s = new_search(coefs_of(setup, multiple), &open);
            let u = if f == 0.0 { vec![amp; n] } else { sine(fs, f, amp, n) };
            let mut out = vec![0u8; n / 8];
            s.run(&u, &mut out);
            if s.overloads() > 0 {
                break;
            }
            found[w] = amp;
        }
    }
    found
}

/// SDM5 LA's search: the base the tests vary.
const SDM5_LA: LaSetup =
    LaSetup { order: 5, wide: false, paths: 8, depth: 49, gain: 1.7, limit: 0.57, dither: DITHER, trellis: false };

fn la_maker(multiple: u32, setup: LaSetup) -> impl Fn() -> Box<dyn DsdEncoder> {
    move || Box::new(LaEncoder::new(dop_input(multiple), rate(multiple), &setup).unwrap()) as Box<dyn DsdEncoder>
}

#[test]
fn every_built_kind_builds_and_reports_its_latency() {
    let la_kinds = CATALOG.iter().filter(|i| i.search != KindSearch::None);
    for kind in la_kinds.map(|i| i.kind) {
        for mult in [64, 128, 256, 512, 1024] {
            for base in [44_100, 48_000] {
                let r = DsdRate::new(base, mult).unwrap();
                if !kind.supports(r) {
                    continue;
                }
                let enc = super::super::new_encoder(kind, r.bit_rate() / 16, r).unwrap();
                assert_eq!(enc.ratio(), 16);
                let depth = setup_for(kind, mult).unwrap().depth as usize;
                assert_eq!(enc.latency(), (depth - 1).div_ceil(16));
            }
        }
    }
    let enc = super::super::new_encoder(ModulatorKind::Sdm5La, 44_100, DsdRate::DSD64).unwrap();
    assert_eq!(enc.latency(), 1);
    // The Wide kinds start at DSD128.
    assert!(super::super::new_encoder(ModulatorKind::Sdm7WideLa, 44_100, DsdRate::DSD64).is_err());
}

/// The first (N − 1) / 8 bytes after a start or a reset are DSD silence; the
/// stream after them is the same whatever came before the reset.
#[test]
fn preroll_then_reset_restarts() {
    let setup = setup_for(ModulatorKind::Sdm5La, 128).unwrap();
    let pcm = sine(352_800.0, 1000.0, 0.7, 3000);
    let mut fresh = LaEncoder::new(352_800, DsdRate::DSD128, &setup).unwrap();
    let want = encode(&mut fresh, &pcm);
    let pre = (setup.depth as usize - 1) / 8;
    assert!(want[..pre].iter().all(|&b| b == DSD_SILENCE));
    assert!(want[pre..pre + 16].iter().any(|&b| b != DSD_SILENCE));
    let mut used = LaEncoder::new(352_800, DsdRate::DSD128, &setup).unwrap();
    encode(&mut used, &sine(352_800.0, 3000.0, 0.9, 5000));
    used.reset();
    assert_eq!(encode(&mut used, &pcm), want);
}

/// The leading filler is exactly `latency() · ratio / 8` bytes of silence at
/// every ratio (the search's own N − 1 bits plus the delay line's bytes),
/// and the delay line keeps any split of the input equal to one block.
#[test]
fn filler_is_the_latency_at_every_ratio() {
    for (input, r) in [(705_600, DsdRate::DSD256), (44_100, DsdRate::DSD64), (44_100, DsdRate::DSD128), (88_200, DsdRate::DSD256)] {
        let setup = setup_for(ModulatorKind::Sdm5La, r.multiple).unwrap();
        let pcm = sine(input as f64, 1000.0, 0.7, 2000);
        let mut whole = LaEncoder::new(input, r, &setup).unwrap();
        let want = encode(&mut whole, &pcm);
        let fill = whole.latency() * whole.ratio() / 8;
        assert!(want[..fill].iter().all(|&b| b == DSD_SILENCE), "ratio {}", whole.ratio());
        assert!(want[fill..fill + 8].iter().any(|&b| b != DSD_SILENCE), "ratio {}", whole.ratio());
        let mut parts = LaEncoder::new(input, r, &setup).unwrap();
        let mut got = Vec::new();
        for chunk in pcm.chunks(37) {
            got.extend(encode(&mut parts, chunk));
        }
        assert_eq!(got, want, "ratio {}", whole.ratio());
    }
}

/// Any split of the input gives the same bytes as one block, and the AVX2
/// and the portable builds give the same bytes.
#[test]
fn chunked_and_portable_match_one_block() {
    for paths in [1, 4, 8, 16] {
        let setup = LaSetup { paths, depth: 33, gain: 1.6, limit: 0.6, ..SDM5_LA };
        let pcm = sine(705_600.0, 1234.0, 0.8, 20_000);
        let mut whole = LaEncoder::new(705_600, DsdRate::DSD256, &setup).unwrap();
        let want = encode(&mut whole, &pcm);
        let mut parts = LaEncoder::new(705_600, DsdRate::DSD256, &setup).unwrap();
        parts.force_portable();
        let mut got = Vec::new();
        let mut rng = 12345u64;
        let mut i = 0;
        while i < pcm.len() {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = ((rng >> 33) as usize % 1500).min(pcm.len() - i);
            got.extend(encode(&mut parts, &pcm[i..i + len]));
            i += len;
        }
        assert_eq!(got, want, "{paths} paths");
    }
}

/// The vector search and the scalar reference keep the same paths up to
/// rounding: the same noise to a fraction of a dB.
#[test]
fn vector_search_matches_the_reference() {
    for paths in [4, 8, 16] {
        let setup = LaSetup { paths, depth: 33, gain: 1.6, limit: 0.6, ..SDM5_LA };
        let fs = 44_100.0 * 64.0;
        let u = sine(fs, 1000.0, 0.25, 1 << 21);
        let nd = |mut s: Box<dyn LaLoop>| {
            let mut o = vec![0u8; u.len() / 8];
            s.run(&u, &mut o);
            let y = decode(&bits(&o), 64);
            let spec = spectrum(&y[y.len() - FFT_LEN..]);
            db(in_band(&spec, Some(1000.0)) / PCM_FS_POWER)
        };
        let k = coefs_of(&setup, 64);
        let (a, b) = (nd(new_search(k, &setup)), nd(new_reference_search(k, &setup)));
        println!("M{paths}: N+D vector {a:.2} dB, reference {b:.2} dB");
        assert!((a - b).abs() < 1.0, "M{paths}: {a} vs {b}");
    }
}

/// One path, depth 1 is the plain loop: its noise matches SDM5's on the
/// same NTF to a fraction of a dB (SDM5 evaluates the update in a different
/// order, so the bits are not identical).
#[test]
fn one_path_is_the_plain_loop() {
    let setup = LaSetup { paths: 1, depth: 1, gain: 1.5, limit: 0.55, ..SDM5_LA };
    let la = la_maker(64, setup);
    let plain = || Box::new(Sdm5Encoder::new(&SDM5, dop_input(64), rate(64)).unwrap()) as Box<dyn DsdEncoder>;
    let (a, _) = decoded_spectrum(&la, 64, |fs, n| sine(fs, 1000.0, 0.5, n));
    let (b, _) = decoded_spectrum(&plain, 64, |fs, n| sine(fs, 1000.0, 0.5, n));
    let (na, nb) = (db(in_band(&a, Some(1000.0))), db(in_band(&b, Some(1000.0))));
    println!("plain loop N+D: one-path search {na:.2} dB, SDM5 {nb:.2} dB");
    assert!((na - nb).abs() < 1.0, "{na} vs {nb}");
}

/// SDM5 LA against SDM5 at DSD64 and DSD128 (DSD256 in the report): deeper
/// in band at silence and at −6 dBFS, no idle tones, no overloads.
#[test]
fn sdm5_la_beats_sdm5() {
    for mult in [64, 128] {
        let setup = setup_for(ModulatorKind::Sdm5La, mult).unwrap();
        let la = measure(&la_maker(mult, setup), mult);
        let plain = measure(&|| Box::new(Sdm5Encoder::new(&SDM5, dop_input(mult), rate(mult)).unwrap()) as Box<dyn DsdEncoder>, mult);
        println!("DSD{mult}: SDM5 LA {la:?}
        SDM5    {plain:?}");
        assert!(la.nd6 < plain.nd6 - 10.0, "DSD{mult} −6 dBFS");
        assert!(la.silence < plain.silence - 3.0, "DSD{mult} silence");
        assert!(la.spur < 16.0, "DSD{mult} idle tone {}", la.spur);
        assert_eq!(la.overloads, 0);
    }
}

/// The look-ahead kinds of order 7 against their plain loops at their
/// lowest rate (SDM9 LA from DSD512: in the report): deeper at −6 dBFS, no
/// idle tones, no overloads.
#[test]
fn order7_la_kinds_beat_their_plain_loops() {
    let plain = |kind: ModulatorKind, mult: u32| {
        move || super::super::new_encoder(kind, dop_input(mult), rate(mult)).unwrap()
    };
    // Measured: N+D at −6 dBFS 12 / 19.5 / 14 dB lower, silence 5.7 / 13 /
    // 9.3 dB lower.
    for (kind, base, mult, nd, quiet) in [
        (ModulatorKind::Sdm7La, ModulatorKind::Sdm7, 64, 9.0, 3.0),
        (ModulatorKind::Sdm7LaPlus, ModulatorKind::Sdm7, 64, 15.0, 9.0),
        (ModulatorKind::Sdm7WideLa, ModulatorKind::Sdm7Wide, 128, 9.0, 4.0),
    ] {
        let la = measure(&la_maker(mult, setup_for(kind, mult).unwrap()), mult);
        let p = measure(&plain(base, mult), mult);
        println!("DSD{mult}: {kind:?} {la:?}
        {base:?} {p:?}");
        assert!(la.nd6 < p.nd6 - nd, "{kind:?} DSD{mult} −6 dBFS");
        assert!(la.silence < p.silence - quiet, "{kind:?} DSD{mult} silence");
        assert!(la.spur < 16.0, "{kind:?} DSD{mult} idle tone {}", la.spur);
        assert_eq!(la.overloads, 0, "{kind:?} DSD{mult}");
    }
}

/// The SDM5 tests' stimuli: no state reset ever; the input limit only for
/// the square and the steps (band-limited edges overshoot ~9 %).
#[test]
fn la_kinds_stable_for_the_test_signals() {
    use ModulatorKind as K;
    // SDM5 LA at length, the other kinds shorter (the debug build is slow).
    let runs = [(K::Sdm5La, 64, 0.05), (K::Sdm5La, 128, 0.05), (K::Sdm5La, 256, 0.05)]
        .into_iter()
        .chain([K::Sdm7La, K::Sdm7LaPlus, K::Sdm7WideLa, K::Sdm7Trellis].into_iter().flat_map(|k| [64, 128, 256].map(|m| (k, m, 0.02))))
        .chain([(K::Sdm9La, 512, 0.01), (K::Sdm9Trellis, 512, 0.01)])
        .filter(|&(k, m, _)| k.supports(rate(m)));
    for (kind, mult, secs) in runs {
        let setup = setup_for(kind, mult).unwrap();
        let fs = dop_input(mult) as f64;
        let n = (secs * fs) as usize;
        let amp = 10f64.powf(-0.5 / 20.0);
        let mut cases: Vec<(&str, Vec<f64>, bool)> = Vec::new();
        for f in [20.0, 1000.0, 10_000.0, 20_000.0] {
            cases.push(("sine -0.5 dBFS", sine(fs, f, amp, n), false));
        }
        let sq: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                (0..10).map(|h| (2 * h + 1) as f64).filter(|&o| o * 1000.0 < 20_000.0)
                    .map(|o| 4.0 / PI * (2.0 * PI * o * 1000.0 * t).sin() / o).sum::<f64>() * 0.92
            })
            .collect();
        cases.push(("square", sq, true));
        cases.push(("steps", (0..n).map(|i| [0.0, 1.0, -1.0, 0.5, 0.0][i * 5 / n]).collect(), true));
        let tri = |i: usize| {
            let t = 4.0 * i as f64 / n as f64;
            if t < 1.0 { -t } else if t < 3.0 { t - 2.0 } else { 4.0 - t }
        };
        cases.push(("dc sweep", (0..n).map(tri).collect(), false));
        cases.push(("silence", vec![0.0; n], false));
        for (name, pcm, may_clamp) in cases {
            let mut s = new_search(coefs_of(&setup, mult), &setup);
            let mut interp = Interpolator::new(16, (mult / 16) as f64);
            let mut resets_or_clamps = 0;
            for part in pcm.chunks(interp.block_inputs()) {
                let up = interp.process(part, |x| x * PCM_FULL_SCALE_MODULATION).to_vec();
                let mut out = vec![0u8; up.len() / 8];
                let before = s.overloads();
                s.run(&up, &mut out);
                resets_or_clamps += s.overloads() - before;
            }
            assert!(may_clamp || resets_or_clamps == 0, "{kind:?} DSD{mult} {name}: {resets_or_clamps} overloads");
            let mut probe = new_search(coefs_of(&setup, mult), &LaSetup { limit: 1.5, ..setup });
            let mut interp = Interpolator::new(16, (mult / 16) as f64);
            for part in pcm.chunks(interp.block_inputs()) {
                let up: Vec<f64> = interp.process(part, |x| x * PCM_FULL_SCALE_MODULATION).iter().map(|v| v.clamp(-setup.limit, setup.limit)).collect();
                let mut out = vec![0u8; up.len() / 8];
                probe.run(&up, &mut out);
            }
            assert_eq!(probe.overloads(), 0, "{kind:?} DSD{mult} {name}: state reset");
        }
    }
}

/// The loop the studies run on: LA_ORDER (5, 7, 9; default 5), LA_WIDE=1
/// (the Wide curve's zeros), LA_DITHER (the dither peak's inverse, default
/// 32), LA_GAIN (default the lowest gain built for the order).
fn study_base() -> LaSetup {
    let var = |n: &str| std::env::var(n).ok();
    let order: usize = var("LA_ORDER").and_then(|s| s.parse().ok()).unwrap_or(5);
    let wide = var("LA_WIDE").is_some_and(|s| s == "1");
    let dither = var("LA_DITHER").and_then(|s| s.parse::<f64>().ok()).map_or(DITHER, |d| 1.0 / d);
    let lowest = LA.iter().filter(|k| k.order == order && k.wide == wide).map(|k| k.gain).fold(f64::MAX, f64::min);
    let gain = var("LA_GAIN").and_then(|s| s.parse().ok()).unwrap_or(lowest);
    LaSetup { order, wide, gain, dither, ..SDM5_LA }
}

/// The parameter study: paths × depth × out-of-band gain at one rate
/// (LA_MULT, default 64). Prints a table row per setup.
#[test]
#[ignore]
fn lookahead_study() {
    let mult: u32 = std::env::var("LA_MULT").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
    // LA_GAINS="1.7,1.8", LA_SEARCH="m8:49,t64:49" (m = M-algorithm, t = trellis) narrow the grid.
    let small = std::env::var("LA_SMALL").is_ok();
    let mut gains: Vec<f64> =
        if small { vec![1.6, 1.7, 1.75, 1.8] } else { vec![1.5, 1.6, 1.7, 1.75, 1.8, 1.9, 2.0] };
    let mut searches: Vec<(bool, usize, u32)> = if small {
        vec![(false, 4, 33), (false, 8, 33), (false, 8, 49), (false, 16, 49)]
    } else {
        [(1, 1), (2, 33), (4, 17), (4, 33), (8, 33), (8, 49), (16, 33), (16, 49), (32, 49)]
            .map(|(m, n)| (false, m, n))
            .to_vec()
    };
    if let Ok(g) = std::env::var("LA_GAINS") {
        gains = g.split(',').map(|x| x.trim().parse().unwrap()).collect();
    }
    if let Ok(g) = std::env::var("LA_SEARCH") {
        searches = g
            .split(',')
            .map(|x| {
                let (m, n) = x.trim()[1..].split_once(':').unwrap();
                (x.trim().starts_with('t'), m.parse().unwrap(), n.parse().unwrap())
            })
            .collect();
    }
    let secs: f64 = std::env::var("LA_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(0.1);
    let base = study_base();
    let mut jobs = Vec::new();
    for &g in &gains {
        for &(trellis, m, n) in &searches {
            jobs.push(LaSetup { paths: m, depth: n, gain: g, limit: 0.5, trellis, ..base });
        }
    }
    println!(
        "order {}{} dither 1/{:.0}: DSD{mult} | gain | M | N | MSA 1k/10k/DC | silence | N+D -6 | N+D -60 | spur | overloads",
        base.order,
        if base.wide { " Wide" } else { "" },
        1.0 / base.dither
    );
    let next = std::sync::atomic::AtomicUsize::new(0);
    // Half the cores at most (LA_THREADS, default 12): a long all-core load is not wanted on the build PC.
    let cap: usize = std::env::var("LA_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get()).min(cap).min(jobs.len());
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&s) = jobs.get(i) else { break };
                let msa = msa(mult, &s, secs);
                let n = measure(&la_maker(mult, s), mult);
                println!(
                    "DSD{mult} | {} | {} | {} | {:.2}/{:.2}/{:.2} | {:.1} | {:.1} | {:.1} | {:.0} | {}",
                    s.gain, if s.trellis { format!("T{}", s.paths) } else { s.paths.to_string() }, s.depth, msa[0], msa[1], msa[2], n.silence, n.nd6, n.nd60, n.spur, n.overloads
                );
            });
        }
    });
}

/// The decoded stream (88.2 kHz) low-passed to 20 kHz (150 dB from 24.1 kHz).
fn band_20k(y: &[f64]) -> Vec<f64> {
    let trans = 2.0 * PI * 4_100.0 / DEC_RATE;
    let n = ((150.0 - 8.0) / (2.285 * trans)).ceil() as usize | 1;
    let w = kaiser(n, 0.1102 * (150.0 - 8.7));
    let (mid, fc) = ((n / 2) as f64, 22_050.0 / DEC_RATE);
    let h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - mid;
            (if t == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * t).sin() / (PI * t) }) * w[i]
        })
        .collect();
    (0..y.len() - n).map(|m| h.iter().zip(&y[m..]).map(|(a, b)| a * b).sum()).collect()
}

/// Block-parallel encoding: is a splice clean? The stream is encoded once
/// whole; at each seam `s` a second search starts `W` bits earlier from rest
/// (the warm-up), and its bits replace the whole run's from `s` on. Prints,
/// per warm-up, the worst 1 ms in-band power of (spliced − whole) decoded
/// around the seam, the same 10–30 ms later (two noise realizations, no
/// seam), and the whole run's own in-band noise per 1 ms at silence — all dB
/// re PCM full scale. LA_SEAM="t64:49:1.9" (search:depth:gain), LA_MULT,
/// LA_SEAM_W="1024,8192,65536".
#[test]
#[ignore]
fn lookahead_seam() {
    let mult: u32 = std::env::var("LA_MULT").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
    let spec = std::env::var("LA_SEAM").unwrap_or_else(|_| "m8:49:1.7".into());
    let parts: Vec<&str> = spec.split(':').collect();
    let setup = LaSetup {
        paths: parts[0][1..].parse().unwrap(),
        depth: parts[1].parse().unwrap(),
        gain: parts[2].parse().unwrap(),
        limit: 0.6,
        trellis: parts[0].starts_with('t'),
        ..study_base()
    };
    let warmups: Vec<usize> = std::env::var("LA_SEAM_W")
        .unwrap_or_else(|_| "1024,8192,65536".into())
        .split(',')
        .map(|w| w.trim().parse::<usize>().unwrap() & !7)
        .collect();
    let fs = rate(mult).bit_rate() as f64;
    let bits_per_ms = (fs / 1000.0) as usize;
    let dec_ms = 88usize;
    let n = ((0.4 * fs) as usize) & !7;
    let lead = 8 * bits_per_ms; // decoded span starts 8 ms before the seam
    let tail = 32 * bits_per_ms;
    let seams: Vec<usize> = (0..6).map(|i| ((0.1 * fs) as usize + i * 7 * bits_per_ms + 8 * i * 997) & !7).collect();
    let pow_ms = |x: &[f64], from: usize, to: usize| -> Vec<f64> {
        (from..to).map(|b| x[b * dec_ms..(b + 1) * dec_ms].iter().map(|v| v * v).sum::<f64>() / dec_ms as f64).collect()
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    for (name, u) in [("silence", vec![0.0; n]), ("1 kHz -6 dBFS", sine(fs, 1000.0, 0.25, n))] {
        let mut whole = new_search(coefs_of(&setup, mult), &setup);
        let mut seq = vec![0u8; n / 8];
        whole.run(&u, &mut seq);
        let seq_bits = bits(&seq);
        if name == "silence" {
            let y = band_20k(&decode(&seq_bits[n / 4..], mult));
            let floor = median(pow_ms(&y, 2, y.len() / dec_ms - 2));
            println!("DSD{mult} {spec} {name}: own in-band noise per 1 ms {:.1} dB", db(floor / PCM_FS_POWER));
        }
        for &w in &warmups {
            let (mut worst, mut later) = (0.0f64, 0.0f64);
            for &s in &seams {
                let mut b = new_search(coefs_of(&setup, mult), &setup);
                let mut out = vec![0u8; (s + tail + 8192 - (s - w)) / 8];
                b.run(&u[s - w..s - w + out.len() * 8], &mut out);
                let b_bits = bits(&out);
                let span = s - lead..s + tail;
                let mut spliced = seq_bits[span.clone()].to_vec();
                spliced[lead..].copy_from_slice(&b_bits[w..w + tail]);
                let d: Vec<f64> = decode(&spliced, mult).iter().zip(decode(&seq_bits[span], mult)).map(|(a, b)| a - b).collect();
                let y = band_20k(&d);
                let p = pow_ms(&y, 0, y.len() / dec_ms);
                // The decoders' delays put the seam ~7 ms into y.
                worst = worst.max(p[4..12].iter().cloned().fold(0.0, f64::max));
                later = later.max(median(p[18..p.len() - 1].to_vec()));
            }
            println!(
                "DSD{mult} {spec} {name} warm-up {w} bits: seam worst 1 ms {:.1} dB, 10-30 ms later {:.1} dB",
                db(worst / PCM_FS_POWER),
                db(later / PCM_FS_POWER)
            );
        }
    }
}

/// CPU of the search alone at the bit rate (one channel, 1 s of a −6 dBFS
/// sine), in ns per output bit and % of one core at DSD64…512.
#[test]
#[ignore]
fn lookahead_speed() {
    let cases = [(1, 1), (2, 33), (4, 33), (8, 33), (8, 49), (16, 49), (32, 49), (64, 57)].map(|(m, n)| (false, m, n));
    let trellis = [(16, 49), (64, 49), (256, 57)].map(|(m, n)| (true, m, n));
    let base = study_base();
    // The loop's coefficients at DSD256 (order 9: at DSD512, its lowest rate).
    let at = if base.order == 9 { 512 } else { 256 };
    for (t, m, n) in cases.into_iter().chain(trellis) {
        let setup = LaSetup { paths: m, depth: n, limit: 0.6, trellis: t, ..base };
        let fs = 44_100.0 * 256.0;
        let len = (fs * 0.25) as usize & !7;
        let u = sine(fs, 1000.0, 0.25, len);
        let mut out = vec![0u8; len / 8];
        let k = coefs_of(&base, at);
        let mut s = new_search(k, &setup);
        let t0 = std::time::Instant::now();
        s.run(&u, &mut out);
        let ns = t0.elapsed().as_secs_f64() * 1e9 / len as f64;
        let timed = |mut s: Box<dyn LaLoop>| {
            let mut o = vec![0u8; len / 8];
            let t0 = std::time::Instant::now();
            s.run(&u, &mut o);
            (t0.elapsed().as_secs_f64() * 1e9 / len as f64, o)
        };
        let mut p = new_search(k, &setup);
        p.force_portable();
        let (nsp, po) = timed(p);
        let (nsr, _) = timed(new_reference_search(k, &setup));
        assert!(po == out, "M{m}: the portable build differs");
        let pct = |mult: f64| ns * 44_100.0 * mult / 1e7;
        println!(
            "{}{m} N{n}: {ns:.1} ns/bit (portable {nsp:.1}, scalar reference {nsr:.1}); one channel DSD64 {:.0} %, DSD128 {:.0} %, DSD256 {:.0} %, DSD512 {:.0} % of a core",
            if t { "T" } else { "M" },
            pct(64.0),
            pct(128.0),
            pct(256.0),
            pct(512.0)
        );
    }
}

/// CPU of every catalog kind's whole encoder (interpolator + loop or search)
/// for one channel of a 44.1 kHz source, 0.25 s of a −6 dBFS 1 kHz sine, in
/// % of one core, at every rate it supports; LA_CPU_KINDS="Sdm7La,…" narrows
/// the kinds (by `{:?}` name). One thread, one kind at a time.
#[test]
#[ignore]
fn lookahead_kind_cpu() {
    let only = std::env::var("LA_CPU_KINDS").ok();
    println!("kind | DSD64 | DSD128 | DSD256 | DSD512 | DSD1024  (% of one core per channel; ns per bit)");
    for info in CATALOG.iter() {
        let name = format!("{:?}", info.kind);
        if only.as_ref().is_some_and(|o| !o.split(',').any(|k| k.trim() == name)) {
            continue;
        }
        let mut row = format!("{} |", info.label);
        for mult in [64, 128, 256, 512, 1024] {
            let r = DsdRate::new(44_100, mult).unwrap();
            let Ok(mut enc) = super::super::new_encoder(info.kind, 44_100, r) else {
                row += " - |";
                continue;
            };
            let pcm = sine(44_100.0, 1000.0, 0.5, 44_100 / 4);
            let mut out = vec![0u8; pcm.len() * enc.ratio() / 8];
            enc.process(&pcm[..2048], &mut out[..2048 * enc.ratio() / 8]);
            enc.reset();
            let t0 = std::time::Instant::now();
            enc.process(&pcm, &mut out);
            let secs = t0.elapsed().as_secs_f64();
            let ns = secs * 1e9 / (out.len() * 8) as f64;
            row += &format!(" {:.0} % ({ns:.1}) |", secs / 0.25 * 100.0);
        }
        println!("{row}");
    }
}
