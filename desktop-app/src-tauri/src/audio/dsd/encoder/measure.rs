//! The modulator measurement suite (docs/18 §7; the ten points of the stage-4
//! plan), on a software decoder: a cascade of ×2 Kaiser half-bands down to
//! 4 × the base rate (176.4 kHz) that keeps everything aliasing into
//! 0–50 kHz at least `DECODER_DB` down, then a Kaiser β = 28 FFT. Levels are
//! in dB re PCM full scale (a full-scale PCM sine is 0.5 of full modulation).
//!
//! Every function takes an encoder from `new_encoder`, so any kind (the
//! look-ahead ones too) is measured alike. The fast checks in `tests.rs` use
//! them at a few rates; `dsd_modulator_report` prints the whole table:
//! `cargo test --profile fast --bins dsd_modulator_report -- --ignored --nocapture`.

use std::f64::consts::PI;
use std::time::Instant;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use super::interp::Interpolator;
use super::modulator::Sdm;
use super::tables::{SdmCoefs, INPUT_LIMIT};
use super::{new_encoder, plain_table, coefs_for};
use crate::audio::dsd::{DsdEncoder, DsdRate, ModulatorKind, CATALOG, PCM_FULL_SCALE_MODULATION};

/// Power of a full-scale PCM sine in one-bit units.
pub(crate) const PCM_FS_POWER: f64 = 0.125;
/// The decoder's protection of 0–50 kHz against aliasing, dB.
pub(crate) const DECODER_DB: f64 = 250.0;
/// The band the decoder keeps clean, Hz.
const DECODER_PASS: f64 = 52_000.0;
/// FFT window.
const BETA: f64 = 28.0;
/// Bins left out on each side of a tone (the β = 28 main lobe is ±9 bins).
pub(crate) const SKIP: usize = 14;

pub(crate) fn rate(multiple: u32) -> DsdRate {
    DsdRate::new(44_100, multiple).unwrap()
}

/// The DoP-style input rate: 16 bits per PCM sample.
pub(crate) fn dop_input(multiple: u32) -> u32 {
    rate(multiple).bit_rate() / 16
}

pub(crate) fn encode(enc: &mut dyn DsdEncoder, pcm: &[f64]) -> Vec<u8> {
    let mut out = vec![0u8; pcm.len() * enc.ratio() / 8];
    enc.process(pcm, &mut out);
    out
}

pub(crate) fn bits(bytes: &[u8]) -> Vec<f64> {
    let mut v = Vec::with_capacity(bytes.len() * 8);
    for &b in bytes {
        for i in (0..8).rev() {
            v.push(if b >> i & 1 == 1 { 1.0 } else { -1.0 });
        }
    }
    v
}

pub(crate) fn sine(fs: f64, f: f64, amp: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| amp * (2.0 * PI * f * i as f64 / fs).sin()).collect()
}

pub(crate) fn db(p: f64) -> f64 {
    10.0 * p.log10()
}

/// dBFS (PCM) → peak amplitude.
pub(crate) fn amp(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
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

pub(crate) fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    let i0b = bessel_i0(beta);
    (0..n)
        .map(|i| {
            let r = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect()
}

/// A ×2 decimating Kaiser half-band: the taps at odd offsets from the centre
/// (nearest first); the centre tap is 0.5, the even ones zero.
/// Kaiser's length estimate is short for so deep a stop band: the length
/// grows until the stop band (from fin/2 − pass) measures `atten`.
fn half_band(fin: f64, pass: f64, atten: f64) -> Vec<f64> {
    let dw = 2.0 * PI * (fin / 2.0 - 2.0 * pass) / fin;
    let mut n = ((atten - 8.0) / (2.285 * dw)).ceil() as usize;
    n = (n + 1).div_ceil(4) * 4 - 1; // 4K − 1 taps
    let beta = 0.1102 * (atten - 8.7);
    loop {
        let w = kaiser(n, beta);
        let c = n / 2;
        let taps: Vec<f64> = (0..(c + 1) / 2)
            .map(|j| {
                let t = (2 * j + 1) as f64;
                (PI * t / 2.0).sin() / (PI * t) * w[c + 2 * j + 1]
            })
            .collect();
        let stop = (0..=2000)
            .map(|i| {
                let f = fin / 2.0 - pass + pass * i as f64 / 2000.0;
                let w = 2.0 * PI * f / fin;
                (0.5 + 2.0 * taps.iter().enumerate().map(|(j, t)| t * ((2 * j + 1) as f64 * w).cos()).sum::<f64>()).abs()
            })
            .fold(0.0, f64::max);
        if 20.0 * stop.log10() <= -atten || n > 4000 {
            return taps;
        }
        n += 4;
    }
}

struct Stage {
    taps: Vec<f64>,
    buf: Vec<f64>,
}

impl Stage {
    /// Appends `input`, writes every output it now can.
    fn run(&mut self, input: &[f64], out: &mut Vec<f64>) {
        self.buf.extend_from_slice(input);
        let k = self.taps.len();
        let span = 4 * k - 1;
        let mut i = 0;
        while i + span <= self.buf.len() {
            let w = &self.buf[i..i + span];
            let c = 2 * k - 1;
            let mut acc = 0.0;
            for (j, &h) in self.taps.iter().enumerate() {
                acc += h * (w[c - 2 * j - 1] + w[c + 2 * j + 1]);
            }
            out.push(0.5 * w[c] + acc);
            i += 2;
        }
        self.buf.drain(..i);
    }
}

/// One-bit stream (or any signal at the bit rate) → PCM at 4 × the base
/// rate, streaming.
pub(crate) struct Decoder {
    stages: Vec<Stage>,
    tmp: [Vec<f64>; 2],
}

impl Decoder {
    pub(crate) fn new(rate: DsdRate) -> Self {
        let mut fin = rate.bit_rate() as f64;
        let mut stages = Vec::new();
        let out = 4.0 * rate.base as f64;
        while fin > out * 1.5 {
            stages.push(Stage { taps: half_band(fin, DECODER_PASS * rate.base as f64 / 44_100.0, DECODER_DB), buf: Vec::new() });
            fin /= 2.0;
        }
        Decoder { stages, tmp: [Vec::new(), Vec::new()] }
    }

    pub(crate) fn out_rate(rate: DsdRate) -> f64 {
        4.0 * rate.base as f64
    }

    pub(crate) fn process(&mut self, x: &[f64], out: &mut Vec<f64>) {
        let [a, b] = &mut self.tmp;
        a.clear();
        a.extend_from_slice(x);
        for s in &mut self.stages {
            b.clear();
            s.run(a, b);
            std::mem::swap(a, b);
        }
        out.extend_from_slice(a);
    }

    /// The decoder's response at `f` (Hz), from its taps.
    pub(crate) fn gain(&self, rate: DsdRate, f: f64) -> f64 {
        let mut fin = rate.bit_rate() as f64;
        let mut g = 1.0;
        for s in &self.stages {
            let w = 2.0 * PI * f / fin;
            g *= 0.5 + 2.0 * s.taps.iter().enumerate().map(|(j, t)| t * ((2 * j + 1) as f64 * w).cos()).sum::<f64>();
            fin /= 2.0;
        }
        g
    }
}

/// One-sided power per bin, Kaiser β = 28; bins sum to the signal's power.
pub(crate) fn spectrum(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let w = kaiser(n, BETA);
    let s2: f64 = w.iter().map(|v| v * v).sum();
    let mut buf: Vec<Complex<f64>> = x.iter().zip(&w).map(|(a, b)| Complex::new(a * b, 0.0)).collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    (0..n / 2).map(|k| buf[k].norm_sqr() / (n as f64 * s2) * if k == 0 { 1.0 } else { 2.0 }).collect()
}

/// A decoded spectrum and its frequency grid.
pub(crate) struct Spectrum {
    pub(crate) p: Vec<f64>,
    pub(crate) hz_per_bin: f64,
}

impl Spectrum {
    pub(crate) fn of(x: &[f64], fs: f64) -> Self {
        Spectrum { p: spectrum(x), hz_per_bin: fs / x.len() as f64 }
    }

    pub(crate) fn bin(&self, f: f64) -> usize {
        (f / self.hz_per_bin).round() as usize
    }

    /// Power in a tone's ±SKIP bins.
    pub(crate) fn tone(&self, f: f64) -> f64 {
        let b = self.bin(f);
        self.p[b.saturating_sub(SKIP)..=(b + SKIP).min(self.p.len() - 1)].iter().sum()
    }

    /// Power in f1–f2 without the ±SKIP bins around each of `skip`.
    pub(crate) fn band(&self, f1: f64, f2: f64, skip: &[f64]) -> f64 {
        let skip: Vec<usize> = skip.iter().map(|&f| self.bin(f)).collect();
        (self.bin(f1)..=self.bin(f2).min(self.p.len() - 1))
            .filter(|&k| skip.iter().all(|&t| k + SKIP < t || k > t + SKIP))
            .map(|k| self.p[k])
            .sum()
    }

    /// The worst bin in 40 Hz–20 kHz (and clear of DC's main lobe) by its
    /// excess over its neighbourhood's median (dB), and its level (dB re PCM
    /// full scale) and frequency. Bins below `min_level` (dB re FS) do not
    /// compete.
    pub(crate) fn spur(&self, skip: &[f64], min_level: f64) -> (f64, f64, f64) {
        let (lo, hi) = (self.bin(40.0).max(2 * SKIP), self.bin(20_000.0));
        let skip: Vec<usize> = skip.iter().map(|&f| self.bin(f)).collect();
        let mut worst = (f64::MIN, 0.0, 0.0);
        let mut nb = Vec::with_capacity(256);
        for k in lo..=hi {
            if skip.iter().any(|&t| k + SKIP >= t && k <= t + SKIP) || db(self.p[k] / PCM_FS_POWER) < min_level {
                continue;
            }
            nb.clear();
            nb.extend((k.saturating_sub(100)..=(k + 100).min(self.p.len() - 1)).map(|j| self.p[j]));
            nb.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let excess = db(self.p[k] / nb[nb.len() / 2]);
            if excess > worst.0 {
                worst = (excess, db(self.p[k] / PCM_FS_POWER), k as f64 * self.hz_per_bin);
            }
        }
        worst
    }
}

/// Builds the kind's encoder fed from the DoP-style input rate.
pub(crate) fn encoder(kind: ModulatorKind, multiple: u32) -> Box<dyn DsdEncoder> {
    new_encoder(kind, dop_input(multiple), rate(multiple)).unwrap()
}

/// Encodes `pcm(input_rate, n)` and decodes it; returns the last `len`
/// decoded samples (the start-up dropped) and the encoder's overloads.
/// `offset` extra input samples of the same signal come first: a different
/// offset is a different dither realisation (the encoders' seeds are fixed).
pub(crate) fn decoded(
    kind: ModulatorKind,
    multiple: u32,
    len: usize,
    offset: usize,
    pcm: &dyn Fn(f64, usize) -> Vec<f64>,
) -> (Vec<f64>, u64) {
    let r = rate(multiple);
    let input_rate = dop_input(multiple) as f64;
    let mut enc = encoder(kind, multiple);
    let out_rate = Decoder::out_rate(r);
    let secs = len as f64 / out_rate + 0.03;
    let n = (secs * input_rate) as usize + offset;
    let x = pcm(input_rate, n);
    let mut dec = Decoder::new(r);
    let mut y = Vec::with_capacity(len + 8192);
    let mut bytes = Vec::new();
    for part in x.chunks(4096) {
        bytes.resize(part.len() * enc.ratio() / 8, 0);
        enc.process(part, &mut bytes);
        dec.process(&bits(&bytes), &mut y);
    }
    assert!(y.len() >= len, "too short: {} < {len}", y.len());
    (y[y.len() - len..].to_vec(), enc.overloads())
}

/// The spectrum of `decoded`, power-averaged over `seeds` offsets.
pub(crate) fn decoded_spectrum(
    kind: ModulatorKind,
    multiple: u32,
    len: usize,
    seeds: usize,
    pcm: &dyn Fn(f64, usize) -> Vec<f64>,
) -> (Spectrum, u64) {
    let fs = Decoder::out_rate(rate(multiple));
    let mut acc: Option<Spectrum> = None;
    let mut over = 0;
    for s in 0..seeds {
        let (y, o) = decoded(kind, multiple, len, s * 1013, pcm);
        over += o;
        let sp = Spectrum::of(&y, fs);
        match &mut acc {
            None => acc = Some(sp),
            Some(a) => a.p.iter_mut().zip(&sp.p).for_each(|(x, y)| *x += y),
        }
    }
    let mut a = acc.unwrap();
    a.p.iter_mut().for_each(|x| *x /= seeds as f64);
    (a, over)
}

/// Point 1: in-band noise at silence, 20 Hz–20 kHz and 20 Hz–50 kHz.
pub(crate) fn silence_noise(kind: ModulatorKind, multiple: u32, len: usize, seeds: usize) -> (f64, f64) {
    let (s, _) = decoded_spectrum(kind, multiple, len, seeds, &|_, n| vec![0.0; n]);
    (db(s.band(20.0, 20_000.0, &[]) / PCM_FS_POWER), db(s.band(20.0, 50_000.0, &[]) / PCM_FS_POWER))
}

/// A tone's numbers: level (dB re FS), N+D (everything else in 20 Hz–20
/// kHz, dB re FS), THD (harmonics 2–10 within 20 kHz, dB re the tone), noise
/// (N+D without the harmonics, dB re FS), overloads.
pub(crate) struct Tone {
    pub(crate) level: f64,
    pub(crate) nd: f64,
    pub(crate) thd: f64,
    pub(crate) noise: f64,
    pub(crate) overloads: u64,
}

/// Point 2: a sine at `dbfs`.
pub(crate) fn tone(kind: ModulatorKind, multiple: u32, f: f64, dbfs: f64, len: usize, seeds: usize) -> Tone {
    let a = amp(dbfs);
    let (s, overloads) = decoded_spectrum(kind, multiple, len, seeds, &|fs, n| sine(fs, f, a, n));
    let sig = s.tone(f);
    let harmonics: Vec<f64> = (2..=10).map(|h| h as f64 * f).filter(|&h| h < 20_000.0).collect();
    let hp: f64 = harmonics.iter().map(|&h| s.tone(h)).sum();
    let mut skip = harmonics.clone();
    skip.push(f);
    Tone {
        level: db(sig / PCM_FS_POWER),
        nd: db(s.band(20.0, 20_000.0, &[f]) / PCM_FS_POWER),
        thd: db(hp / sig),
        noise: db(s.band(20.0, 20_000.0, &skip) / PCM_FS_POWER),
        overloads,
    }
}

/// Point 3: SMPTE (60 Hz + 7 kHz, 4:1) and CCIF (19 + 20 kHz, 1:1) at a peak
/// of `dbfs`: the products' power re the signal's, dB.
pub(crate) fn imd(kind: ModulatorKind, multiple: u32, dbfs: f64, len: usize) -> (f64, f64) {
    let a = amp(dbfs);
    let (s, _) = decoded_spectrum(kind, multiple, len, 1, &|fs, n| {
        let (l, h) = (sine(fs, 60.0, 0.8 * a, n), sine(fs, 7_000.0, 0.2 * a, n));
        l.iter().zip(&h).map(|(x, y)| x + y).collect()
    });
    let side: f64 = (1..=4).flat_map(|k| [7_000.0 - 60.0 * k as f64, 7_000.0 + 60.0 * k as f64]).map(|f| s.tone(f)).sum();
    let smpte = db(side / s.tone(7_000.0));
    let (s, _) = decoded_spectrum(kind, multiple, len, 1, &|fs, n| {
        let (l, h) = (sine(fs, 19_000.0, 0.5 * a, n), sine(fs, 20_000.0, 0.5 * a, n));
        l.iter().zip(&h).map(|(x, y)| x + y).collect()
    });
    let prod: f64 = [1_000.0, 2_000.0, 17_000.0, 18_000.0, 21_000.0, 22_000.0].iter().map(|&f| s.tone(f)).sum();
    let ccif = db(prod / (s.tone(19_000.0) + s.tone(20_000.0)));
    (smpte, ccif)
}

/// The modulator input the encoder makes of `pcm`: interpolated, scaled to
/// modulation (the encoder's clamp of garbage left out: test signals have none).
pub(crate) fn upsampled(pcm: &[f64], input_rate: u32, rate: DsdRate) -> Vec<f64> {
    let ratio = (rate.bit_rate() / input_rate) as usize;
    let mut interp = Interpolator::new(ratio, input_rate as f64 / rate.base as f64);
    let mut u = Vec::with_capacity(pcm.len() * ratio);
    for part in pcm.chunks(interp.block_inputs()) {
        u.extend_from_slice(interp.process(part, |s| s * PCM_FULL_SCALE_MODULATION));
    }
    u
}

/// What a plain modulator did with `u`: the bits and its counters (clamps,
/// restarts).
pub(crate) struct RawRun {
    pub(crate) bits: Vec<f64>,
    pub(crate) counts: (u64, u64),
}

/// Runs a plain kind's modulator over `u` (a multiple of 8 samples), input
/// limit `limit`.
pub(crate) fn run_plain(kind: ModulatorKind, multiple: u32, u: &[f64], limit: f64) -> Option<RawRun> {
    if let Some((out, counts)) = super::lookahead::raw_run(kind, multiple, u, limit) {
        return Some(RawRun { bits: bits(&out), counts });
    }
    let k = coefs_for(plain_table(kind)?, rate(multiple))?;
    let mut out = vec![0u8; u.len() / 8];
    macro_rules! go {
        ($n:literal) => {{
            let mut s = Sdm::<$n>::with_limit(k, limit);
            for (uu, oo) in u.chunks(8 * 4096).zip(out.chunks_mut(4096)) {
                s.run(uu, oo);
            }
            (s.clamps, s.resets)
        }};
    }
    let counts = match k.a.len() {
        5 => go!(5),
        7 => go!(7),
        9 => go!(9),
        _ => return None,
    };
    Some(RawRun { bits: bits(&out), counts })
}

/// Point 4: the largest sine amplitude (of full modulation) at `f` the loop
/// alone (no input limit) keeps without a restart, to 0.0025: 50 ms per try.
pub(crate) fn msa(kind: ModulatorKind, multiple: u32, f: f64) -> Option<f64> {
    let fs = rate(multiple).bit_rate() as f64;
    let n = ((0.05 * fs) as usize) / 8 * 8;
    let (mut lo, mut hi) = (0.40, 0.90);
    while hi - lo > 0.0025 {
        let mid = 0.5 * (lo + hi);
        let r = run_plain(kind, multiple, &sine(fs, f, mid, n), 10.0)?;
        if r.counts.1 == 0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// The loop's error below ~25 kHz: decoded bits minus the decoded
/// (clamped) input, through the same decoder, then a Kaiser low-pass (pass
/// 20 kHz, stop 30 kHz, 190 dB; 1.3 ms long), as 1 ms power windows (dB re
/// FS).
fn error_windows(bits: &[f64], u: &[f64], limit: f64, rate: DsdRate) -> Vec<f64> {
    let (mut db_, mut du) = (Decoder::new(rate), Decoder::new(rate));
    let (mut yb, mut yu) = (Vec::new(), Vec::new());
    db_.process(bits, &mut yb);
    let clamped: Vec<f64> = u.iter().map(|&v| v.clamp(-limit, limit)).collect();
    du.process(&clamped, &mut yu);
    let fs = Decoder::out_rate(rate);
    let e: Vec<f64> = yb.iter().zip(&yu).map(|(a, b)| a - b).collect();
    let dw = 2.0 * PI * 10_000.0 / fs;
    let n = ((190.0 - 8.0) / (2.285 * dw)).ceil() as usize | 1;
    let w = kaiser(n, 0.1102 * (190.0 - 8.7));
    let (mid, fc) = ((n / 2) as f64, 25_000.0 / fs);
    let h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - mid;
            w[i] * if t == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * t).sin() / (PI * t) }
        })
        .collect();
    let lp: Vec<f64> = (0..e.len().saturating_sub(n)).map(|i| h.iter().zip(&e[i..]).map(|(a, b)| a * b).sum()).collect();
    // Back on the input's time axis: the low-pass delays by n / 2.
    let pad = vec![f64::NAN; n / 2];
    let lp: Vec<f64> = pad.into_iter().chain(lp).collect();
    let win = (fs / 1000.0) as usize;
    lp.chunks_exact(win).map(|c| db(c.iter().map(|v| v * v).sum::<f64>() / win as f64 / PCM_FS_POWER)).collect()
}

/// Points 4 and 9: how a plain loop comes out of an event. `u` at the bit
/// rate; the event ends at `end` seconds. Returns the counters, the time
/// (ms) after `end` until the 20 kHz error stays within 6 dB of its floor
/// before the event (`floor_until` s), and the peak error (dB re FS).
pub(crate) fn recovery(
    kind: ModulatorKind,
    multiple: u32,
    u: &[f64],
    limit: f64,
    floor_until: f64,
    end: f64,
) -> Option<((u64, u64), f64, f64)> {
    let r = rate(multiple);
    let run = run_plain(kind, multiple, u, limit)?;
    let w = error_windows(&run.bits, &u[..run.bits.len()], limit, r);
    // The low-pass's start-up: skip the first 3 ms.
    let mut floor: Vec<f64> = w[3..(floor_until * 1000.0) as usize - 1].to_vec();
    floor.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = floor[floor.len() / 2];
    let end_ms = (end * 1000.0).ceil() as usize;
    let last_bad = (end_ms..w.len().saturating_sub(2)).filter(|&i| w[i] > floor + 6.0).last();
    let rec = last_bad.map_or(0.0, |i| (i + 1 - end_ms) as f64);
    let peak = w[3..w.len().saturating_sub(2)].iter().cloned().fold(f64::MIN, f64::max);
    Some((run.counts, rec, peak))
}

/// Point 5: level error (dB) of a 1 kHz tone from −60 to −130 dBFS in 10 dB
/// steps; returns the worst |error|.
pub(crate) fn linearity(kind: ModulatorKind, multiple: u32, len: usize) -> f64 {
    (6..=13)
        .map(|k| {
            let l = -10.0 * k as f64;
            let t = tone(kind, multiple, 1_000.0, l, len, 1);
            (t.level - l).abs()
        })
        .fold(0.0, f64::max)
}

/// Point 6: the worst spur over DC inputs (PCM) and silence: (excess dB,
/// level dB, Hz, at DC).
pub(crate) fn idle_tones(kind: ModulatorKind, multiple: u32, len: usize, dcs: &[f64], min_level: f64) -> (f64, f64, f64, f64) {
    let mut worst = (f64::MIN, 0.0, 0.0, 0.0);
    for &dc in dcs {
        let (s, _) = decoded_spectrum(kind, multiple, len, 1, &|_, n| vec![dc; n]);
        let (e, l, f) = s.spur(&[], min_level);
        if e > worst.0 {
            worst = (e, l, f, dc);
        }
    }
    worst
}

/// Point 8: from the bit stream itself (2^19 bits, Kaiser β = 28), with a
/// 1 kHz tone at −6 dBFS: noise density at each of `at` (dB re FS per kHz,
/// ±2.5 % around it) and the power in `f1`–`f2` (dB re FS).
pub(crate) fn ultrasonic(kind: ModulatorKind, multiple: u32, at: &[f64], f1: f64, f2: f64) -> (Vec<f64>, f64) {
    let n = 1 << 19;
    let input = dop_input(multiple);
    let mut enc = encoder(kind, multiple);
    let pcm = sine(input as f64, 1_000.0, 0.5, n / 16 + 4096);
    let x = bits(&encode(enc.as_mut(), &pcm));
    let s = Spectrum::of(&x[x.len() - n..], rate(multiple).bit_rate() as f64);
    let dens = at
        .iter()
        .map(|&f| {
            let (a, b) = (s.bin(f * 0.975), s.bin(f * 1.025));
            let mean = s.p[a..=b].iter().sum::<f64>() / (b - a + 1) as f64;
            db(mean / s.hz_per_bin * 1000.0 / PCM_FS_POWER)
        })
        .collect();
    (dens, db(s.band(f1, f2, &[]) / PCM_FS_POWER))
}

/// Point 9's stimuli at the DoP-style input rate: a band-limited 1 kHz
/// square at 0 dBFS (overshoot ~9 %), steps 0 → +1 → −1 → 0.5 → 0, and a 1
/// kHz sine at +6 dBFS hard-clipped at ±1.4 (PCM), each 30 ms between
/// silences; the event spans 10–40 ms.
pub(crate) fn event(name: &str, fs: f64) -> Vec<f64> {
    let n = (0.06 * fs) as usize;
    let (a, b) = ((0.01 * fs) as usize, (0.04 * fs) as usize);
    (0..n)
        .map(|i| {
            if i < a || i >= b {
                return 0.0;
            }
            let t = (i - a) as f64 / fs;
            match name {
                "square" => (0..10).map(|h| (2 * h + 1) as f64).filter(|&o| o * 1000.0 < 20_000.0)
                    .map(|o| 4.0 / PI * (2.0 * PI * o * 1000.0 * t).sin() / o).sum::<f64>() * 0.92,
                "steps" => [1.0, -1.0, 0.5, 0.0][(i - a) * 4 / (b - a)],
                _ => (2.0 * (2.0 * PI * 1000.0 * t).sin()).clamp(-1.4, 1.4),
            }
        })
        .collect()
}

/// Point 10: ns per output bit per channel, and % of one core for stereo,
/// from the DoP-style input rate (one second of a −6 dBFS sine).
pub(crate) fn cpu(kind: ModulatorKind, multiple: u32) -> (f64, f64) {
    let fs = dop_input(multiple);
    let pcm = sine(fs as f64, 1000.0, 0.5, fs as usize);
    let mut enc = encoder(kind, multiple);
    let mut out = vec![0u8; pcm.len() * enc.ratio() / 8];
    enc.process(&pcm[..fs as usize / 10], &mut out[..fs as usize / 10 * enc.ratio() / 8]); // warm up
    let t = Instant::now();
    enc.process(&pcm, &mut out);
    let s = t.elapsed().as_secs_f64();
    (s * 1e9 / rate(multiple).bit_rate() as f64, 2.0 * s * 100.0)
}

/// The plain kinds this suite reports, best rank first.
pub(crate) fn plain_kinds() -> Vec<ModulatorKind> {
    CATALOG.iter().map(|i| i.kind).filter(|&k| plain_table(k).is_some()).collect()
}

fn multiples(kind: ModulatorKind) -> Vec<u32> {
    [64, 128, 256, 512, 1024].into_iter().filter(|&m| kind.supports(rate(m))).collect()
}

/// The whole table: every plain kind at every rate it is built for.
/// `cargo test --profile fast --bins dsd_modulator_report -- --ignored --nocapture`
#[test]
#[ignore]
fn dsd_modulator_report() {
    const LEN: usize = 1 << 16;
    let only: Option<Vec<String>> = std::env::var("DSD_REPORT_KINDS").ok().map(|v| v.split(',').map(|s| s.to_string()).collect());
    // DSD_REPORT_MULTIPLES=64,128,256 keeps the long DSD512/1024 rows out.
    let at: Option<Vec<u32>> =
        std::env::var("DSD_REPORT_MULTIPLES").ok().map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    // Every kind, the look-ahead ones too (DSD_REPORT_KINDS="SDM7 LA,SDM7" picks).
    let rows: Vec<(ModulatorKind, u32)> = CATALOG
        .iter()
        .map(|i| i.kind)
        .filter(|k| only.as_ref().map_or(true, |o| o.iter().any(|s| s == k.label())))
        .flat_map(|k| multiples(k).into_iter().map(move |m| (k, m)))
        .filter(|(_, m)| at.as_ref().map_or(true, |a| a.contains(m)))
        .collect();
    let name = |k: ModulatorKind, m: u32| format!("{} DSD{m}", k.label());
    let mut t1 = vec!["| modulator | noise 20k | noise 50k | N+D 0 dB | N+D −3 | N+D −6 | N+D −20 | N+D −60 | N+D −90 | THD 0 dB | THD −6 | overloads |".to_string(),
        "|---|---|---|---|---|---|---|---|---|---|---|---|".to_string()];
    let mut t2 = vec!["| modulator | SMPTE −6 | SMPTE −20 | CCIF −6 | CCIF −20 | MSA 1k | MSA 10k | linearity −60…−130 | idle spur | noise mod. |".to_string(),
        "|---|---|---|---|---|---|---|---|---|---|".to_string()];
    let mut t3 = vec!["| modulator | 50k | 100k | 200k | 20k…2×edge | burst 0.75 (loop alone) | square | steps | clipped +6 | ns/bit/ch | stereo % core |".to_string(),
        "|---|---|---|---|---|---|---|---|---|---|---|".to_string()];
    for &(k, m) in &rows {
        let t0 = Instant::now();
        let r = rate(m);
        let (n20, n50) = silence_noise(k, m, LEN, 3);
        let levels = [0.0, -3.0, -6.0, -20.0, -60.0, -90.0];
        let tones: Vec<Tone> = levels.iter().map(|&l| tone(k, m, 1_000.0, l, LEN, if l < -50.0 { 1 } else { 3 })).collect();
        let over: u64 = tones.iter().map(|t| t.overloads).sum();
        let nd: Vec<String> = tones.iter().map(|t| format!("{:.1}", t.nd)).collect();
        t1.push(format!(
            "| {} | {:.1} | {:.1} | {} | {:.1} | {:.1} | {} |",
            name(k, m),
            n20,
            n50,
            nd.join(" | "),
            tones[0].thd,
            tones[2].thd,
            over
        ));
        let (s6, c6) = imd(k, m, -6.0, LEN);
        let (s20, c20) = imd(k, m, -20.0, LEN);
        let (m1, m10) = (msa(k, m, 1_000.0).unwrap_or(f64::NAN), msa(k, m, 10_000.0).unwrap_or(f64::NAN));
        let lin = linearity(k, m, LEN);
        let dcs = [0.0, 1e-5, 3e-5, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2];
        let spur = idle_tones(k, m, LEN, &dcs, f64::MIN);
        let noises: Vec<f64> = std::iter::once(n20).chain(tones.iter().map(|t| t.noise)).collect();
        let spread = noises.iter().cloned().fold(f64::MIN, f64::max) - noises.iter().cloned().fold(f64::MAX, f64::min);
        let mdb = |a: f64| 20.0 * (a / 0.5).log10();
        t2.push(format!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3} ({:+.2} dB) | {:.3} ({:+.2} dB) | {:.3} dB | +{:.1} dB ({:.0} dB, {:.0} Hz, DC {:e}) | {:.1} dB |",
            name(k, m), s6, s20, c6, c20, m1, mdb(m1), m10, mdb(m10), lin, spur.0, spur.1, spur.2, spur.3, spread
        ));
        let edge = super::lookahead::band_edge(k, m).unwrap_or_else(|| coefs_for(plain_table(k).unwrap(), r).unwrap().band_edge);
        let top = if k.info().curve == crate::audio::dsd::NoiseCurve::Wide { edge } else { 2.0 * edge };
        let (dens, ultra) = ultrasonic(k, m, &[50e3, 100e3, 200e3], 20e3, top);
        // The loop alone: silence, a 1 kHz burst at 0.75 of full modulation
        // (beyond every loop's MSA) for 5 ms, then −20 dBFS.
        let fs = r.bit_rate() as f64;
        let n = ((0.06 * fs) as usize) / 8 * 8;
        let u: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let s = (2.0 * PI * 1000.0 * t).sin();
                if t < 0.02 { 0.0 } else if t < 0.025 { 0.75 * s } else { 0.05 * s }
            })
            .collect();
        let (c, ms, pk) = recovery(k, m, &u, 10.0, 0.02, 0.025).unwrap();
        let burst = format!("{ms:.0} ms / {pk:.0} dB ({}r)", c.1);
        let ev = |nm: &str| {
            let pcm = event(nm, dop_input(m) as f64);
            let mut u = upsampled(&pcm, dop_input(m), r);
            u.truncate(u.len() / 8 * 8);
            let (c, ms, _) = recovery(k, m, &u, INPUT_LIMIT, 0.01, 0.04).unwrap();
            format!("{}c {}r, {ms:.0} ms", c.0, c.1)
        };
        let (ns, pct) = cpu(k, m);
        t3.push(format!(
            "| {} | {:.0} | {:.0} | {:.0} | {:.1} | {} | {} | {} | {} | {:.2} | {:.1} |",
            name(k, m), dens[0], dens[1], dens[2], ultra, burst, ev("square"), ev("steps"), ev("clip"), ns, pct
        ));
        eprintln!("{} done in {:.1} s", name(k, m), t0.elapsed().as_secs_f64());
    }
    println!("\nIn-band noise and N+D (dB re PCM full scale, 20 Hz–20 kHz unless noted; 1 kHz tone; THD dB re the tone):\n");
    println!("{}", t1.join("\n"));
    println!("\nIMD (products re signal, dB), maximum stable amplitude of the loop alone (of full modulation; dB re PCM FS), low-level linearity (worst level error), idle tones (worst bin over its neighbourhood median, DC 0…0.01 PCM), noise modulation (spread of in-band noise from silence to 0 dBFS):\n");
    println!("{}", t2.join("\n"));
    println!("\nUltrasonic noise with a −6 dBFS 1 kHz tone (dB re PCM FS per kHz at 50/100/200 kHz; power in 20 kHz…2×band edge), recovery (ms after the event until the 20 kHz error is within 6 dB of its floor / peak error dB; c clamps, r restarts), CPU (--profile fast):\n");
    println!("{}", t3.join("\n"));
}

/// Double-double arithmetic (≈ 106-bit mantissa), for the precision check.
#[derive(Clone, Copy, Default)]
struct Dd(f64, f64);

impl Dd {
    fn quick(s: f64, e: f64) -> Dd {
        let h = s + e;
        Dd(h, e - (h - s))
    }
    fn add(self, b: Dd) -> Dd {
        let s = self.0 + b.0;
        let bb = s - self.0;
        let e = (self.0 - (s - bb)) + (b.0 - bb);
        Dd::quick(s, e + self.1 + b.1)
    }
    fn mul(self, b: Dd) -> Dd {
        let p = self.0 * b.0;
        let e = self.0.mul_add(b.0, -p) + (self.0 * b.1 + self.1 * b.0);
        Dd::quick(p, e)
    }
    fn of(x: f64) -> Dd {
        Dd(x, 0.0)
    }
}

/// The plain loop of `k` in double-double (no input limit, no guard): the
/// reference the f64 loop is compared with. Same dither sequence.
fn run_dd(k: &SdmCoefs, u: &[f64]) -> Vec<f64> {
    let n = k.a.len();
    let (a, c, g): (Vec<Dd>, Vec<Dd>, Vec<Dd>) =
        (k.a.iter().map(|&v| Dd::of(v)).collect(), k.c.iter().map(|&v| Dd::of(v)).collect(), k.g.iter().map(|&v| Dd::of(v)).collect());
    let cout = Dd::of(k.cout);
    let mut x = vec![Dd::default(); n];
    let mut dither = super::modulator::Dither::new(k.dither);
    let mut out = Vec::with_capacity(u.len());
    let neg = |v: Dd| Dd(-v.0, -v.1);
    for pair in u.chunks_exact(2) {
        let d = dither.pair();
        for j in 0..2 {
            let uu = Dd::of(pair[j]);
            let v = cout.mul(x[n - 1]).add(uu);
            let bit = v.0 + v.1 + d[j] >= 0.0;
            let y = if bit { 1.0 } else { -1.0 };
            let w = uu.add(Dd::of(-y));
            for r in (0..(n - 1) / 2).rev() {
                let i = 2 * r + 1;
                let ni = x[i].add(c[i - 1].mul(x[i - 1])).add(a[i].mul(w)).add(neg(g[r].mul(x[i + 1])));
                x[i + 1] = x[i + 1].add(c[i].mul(ni)).add(a[i + 1].mul(w));
                x[i] = ni;
            }
            x[0] = x[0].add(a[0].mul(w));
            out.push(y);
        }
    }
    out
}

/// Point "precision": does f64 limit the deepest loops? Each plain kind at
/// its highest rate, silence and a −6 dBFS 1 kHz tone, the f64 loop against
/// the same loop in double-double: in-band noise and N+D (dB re FS).
/// `cargo test --profile fast --bins dsd_precision_report -- --ignored --nocapture`
#[test]
#[ignore]
fn dsd_precision_report() {
    const LEN: usize = 1 << 16;
    for (kind, m) in [(ModulatorKind::Sdm9, 1024), (ModulatorKind::Sdm9, 512), (ModulatorKind::Sdm7, 1024), (ModulatorKind::Sdm7, 256)] {
        let r = rate(m);
        let k = coefs_for(plain_table(kind).unwrap(), r).unwrap();
        let input = dop_input(m);
        let out_rate = Decoder::out_rate(r);
        let n = ((LEN as f64 / out_rate + 0.03) * input as f64) as usize;
        let mut line = format!("{} DSD{m}:", kind.label());
        for (what, a) in [("silence", 0.0), ("-6 dBFS", 0.5)] {
            let mut u = upsampled(&sine(input as f64, 1000.0, a, n), input, r);
            u.truncate(u.len() / 8 * 8);
            let f64_bits = run_plain(kind, m, &u, 10.0).unwrap().bits;
            let dd_bits = run_dd(k, &u);
            let mut res = Vec::new();
            for b in [&f64_bits, &dd_bits] {
                let mut y = Vec::new();
                Decoder::new(r).process(b, &mut y);
                let s = Spectrum::of(&y[y.len() - LEN..], out_rate);
                res.push(db(s.band(20.0, 20_000.0, &[1000.0]) / PCM_FS_POWER));
            }
            line += &format!(" {what} f64 {:.1} / double-double {:.1} dB;", res[0], res[1]);
        }
        println!("{line}");
    }
}

/// Point 6 for the dither choice: the plain loop alone with DC inputs and
/// a few dither peaks; the worst idle bin and the silence noise.
/// `cargo test --profile fast --bins dsd_dither_study -- --ignored --nocapture`
#[test]
#[ignore]
fn dsd_dither_study() {
    const LEN: usize = 1 << 16;
    let dcs = [0.0, 1e-5, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2];
    let only: Option<Vec<String>> = std::env::var("DSD_REPORT_KINDS").ok().map(|v| v.split(',').map(|s| s.to_string()).collect());
    // DSD_REPORT_MULTIPLES=64,128,256 keeps the long DSD512/1024 rows out.
    let at: Option<Vec<u32>> =
        std::env::var("DSD_REPORT_MULTIPLES").ok().map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    let rows: Vec<(ModulatorKind, u32)> = plain_kinds()
        .into_iter()
        .filter(|k| only.as_ref().map_or(true, |o| o.iter().any(|s| s == k.label())))
        .flat_map(|k| multiples(k).into_iter().map(move |m| (k, m)))
        .filter(|(_, m)| at.as_ref().map_or(true, |a| a.contains(m)))
        .collect();
    for (kind, m) in rows {
        let r = rate(m);
        let k = coefs_for(plain_table(kind).unwrap(), r).unwrap();
        let fs = Decoder::out_rate(r);
        let n = ((LEN as f64 / fs + 0.02) * r.bit_rate() as f64) as usize / 8 * 8;
        for peak in [1.0 / 32.0, 1.0 / 16.0, 1.0 / 8.0, 1.0 / 4.0] {
            let mut worst = (f64::MIN, 0.0, 0.0, 0.0);
            let mut loud = (f64::MIN, 0.0, 0.0, 0.0);
            let mut silence = 0.0;
            for &dc in &dcs {
                let u = vec![dc * PCM_FULL_SCALE_MODULATION; n];
                let mut out = vec![0u8; n / 8];
                macro_rules! go {
                    ($n:literal) => {{
                        let mut s = Sdm::<$n>::with_limit(k, 10.0).dithered(peak);
                        s.run(&u, &mut out);
                    }};
                }
                match k.a.len() {
                    5 => go!(5),
                    7 => go!(7),
                    _ => go!(9),
                }
                let mut y = Vec::new();
                Decoder::new(r).process(&bits(&out), &mut y);
                let sp = Spectrum::of(&y[y.len() - LEN..], fs);
                if dc == 0.0 {
                    silence = db(sp.band(20.0, 20_000.0, &[]) / PCM_FS_POWER);
                }
                let (e, l, f) = sp.spur(&[], f64::MIN);
                if e > worst.0 {
                    worst = (e, l, f, dc);
                }
                let (e, l, f) = sp.spur(&[], -160.0);
                if e > loud.0 {
                    loud = (e, l, f, dc);
                }
            }
            println!(
                "{} DSD{m} dither 1/{:.0}: silence {silence:.1} dB, worst idle bin +{:.1} dB ({:.0} dB at {:.0} Hz, DC {:e}); above -160 dB: {}",
                kind.label(),
                1.0 / peak,
                worst.0,
                worst.1,
                worst.2,
                worst.3,
                if loud.0 == f64::MIN { "none".to_string() } else { format!("+{:.1} dB ({:.0} dB at {:.0} Hz, DC {:e})", loud.0, loud.1, loud.2, loud.3) }
            );
        }
    }
}
