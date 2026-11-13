//! DSD → PCM: low-pass and decimate a one-bit stream into PCM for the
//! engine's chain (DSD files played or converted as PCM) and for measuring
//! the encoder's output.
//!
//! Every rate comes out at twice its family base — 88.2 kHz for DSD64 to
//! DSD1024, 96 kHz for the 48 kHz family: a hi-res source to the chain, half
//! the memory of 176.4 kHz. The band follows the rate (`Bands`): DSD64's
//! shaped noise rises right past 20 kHz, so it is flat to 20 kHz and
//! ≥ 150 dB down from 30; DSD128 and up carry clean audio higher (their
//! noise starts at ~40 and ~80 kHz), so they are flat to 40 kHz and ≥ 150 dB
//! down from the output's Nyquist (44.1 or 48 kHz). Three kinds of
//! linear-phase stages, all designed here from the rate (Kaiser-windowed
//! sinc):
//!
//! 1. bit-domain FIR, ÷8: the one-bit samples are never turned into numbers —
//!    each byte of the window indexes a table of its eight taps' sum (the
//!    dsd2pcm way). Flat to the stop edge; ≥ 160 dB down around every
//!    multiple of its output rate, so nothing folds below the stop edge (the
//!    idle pattern 0x69's 352.8 kHz tone included).
//! 2. half-band ÷2 stages down to four times the base, flat to the stop
//!    edge, ≥ 150 dB down where their output would fold below it.
//! 3. the final ÷2: flat to the pass edge (ripple ~1e-6 dB), ≥ 150 dB down
//!    from the stop edge — the shaped noise above the format's band does not
//!    reach the chain.
//!
//! Level: the output is scaled by 1 / `PCM_FULL_SCALE_MODULATION`, so the
//! SACD reference — 50 % modulation — is PCM full scale and the encoder
//! followed by this decoder is unity. Louder DSD passages decode past ±1.0
//! and stay so: the chain's output ceiling brings them down, as it does a
//! hot lossy file's.
//!
//! Output sample k is centred on one-bit sample k · `factor()`: decoded
//! PCM lines up with the file from its first sample.

use super::dsf::DsdRead;
use super::{DsdRate, PCM_FULL_SCALE_MODULATION};

/// The decoded band of a rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bands {
    /// Flat (all stages) to here.
    pub pass_hz: f64,
    /// The final stage is ≥ 150 dB down from here on; the earlier stages
    /// keep the band below it free of anything folded.
    pub stop_hz: f64,
}

impl Bands {
    pub fn of(rate: DsdRate) -> Bands {
        if rate.multiple <= 64 {
            Bands { pass_hz: 20_000.0, stop_hz: 30_000.0 }
        } else {
            Bands { pass_hz: 40_000.0, stop_hz: rate.base as f64 }
        }
    }
}

const BIT_STAGE_DB: f64 = 160.0;
const PCM_STAGE_DB: f64 = 150.0;

/// DSD (modulation ±1) → PCM: 50 % modulation is full scale.
pub const DSD_TO_PCM_GAIN: f64 = 1.0 / PCM_FULL_SCALE_MODULATION;

/// The table index of a byte that is not there (before the stream starts,
/// after it ends): it adds nothing.
const NO_BYTE: u16 = 256;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

fn kaiser_beta(db: f64) -> f64 {
    if db > 50.0 {
        0.1102 * (db - 8.7)
    } else {
        0.5842 * (db - 21.0).powf(0.4) + 0.07886 * (db - 21.0)
    }
}

/// Taps (odd) a Kaiser design needs for `db` of stopband over a transition
/// `width` (a fraction of the sample rate), with a little to spare.
fn kaiser_taps(db: f64, width: f64) -> usize {
    let n = ((db - 7.95) / (14.36 * width)).ceil() as usize + 8;
    n | 1
}

/// A linear-phase low-pass, `n` (odd) taps, cut-off `fc` (a fraction of the
/// sample rate), DC gain `gain`.
fn kaiser_lowpass(n: usize, fc: f64, db: f64, gain: f64) -> Vec<f64> {
    let beta = kaiser_beta(db);
    let mid = (n / 2) as f64;
    let i0b = bessel_i0(beta);
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t) };
            let r = t / mid;
            sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|x| *x *= gain / sum);
    h
}

/// Stage 1: ÷8 straight from the bits. The window is `ka` bytes (`ka`
/// even); its first tap is a zero, so the odd-length filter's centre falls
/// on a byte boundary: output k is centred on bit 8k.
#[derive(Clone)]
struct BitStage {
    ka: usize,
    /// `tables[w][byte]`: the sum of window byte w's eight taps for that
    /// byte (bit 1 = +1, bit 0 = −1); entry `NO_BYTE` is 0.
    tables: Vec<[f64; 257]>,
    /// The last `ka` byte indices, twice over (a window is one slice).
    ring: Vec<u16>,
    pos: usize,
    seen: u64,
}

impl BitStage {
    fn new(bit_rate: f64, guard: f64) -> BitStage {
        let out_rate = bit_rate / 8.0;
        let width = (out_rate - 2.0 * guard) / bit_rate;
        let n = kaiser_taps(BIT_STAGE_DB, width);
        let ka = ((n + 1).div_ceil(8) + 1) & !1;
        let n = 8 * ka - 1;
        let h = kaiser_lowpass(n, (out_rate / 2.0) / bit_rate, BIT_STAGE_DB, 1.0);
        let mut win = vec![0.0; 8 * ka];
        win[1..].copy_from_slice(&h);
        let tables = (0..ka)
            .map(|w| {
                let mut t = [0.0; 257];
                for (b, v) in t.iter_mut().enumerate().take(256) {
                    *v = (0..8).map(|p| if b & (0x80 >> p) != 0 { win[8 * w + p] } else { -win[8 * w + p] }).sum();
                }
                t
            })
            .collect();
        BitStage { ka, tables, ring: vec![NO_BYTE; 2 * ka], pos: 0, seen: 0 }
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.ring.fill(NO_BYTE);
        self.pos = 0;
        self.seen = 0;
    }

    #[inline]
    fn push(&mut self, b: u16) -> Option<f64> {
        self.pos = if self.pos + 1 == self.ka { 0 } else { self.pos + 1 };
        self.ring[self.pos] = b;
        self.ring[self.pos + self.ka] = b;
        self.seen += 1;
        if self.seen < (self.ka / 2) as u64 {
            return None;
        }
        let win = &self.ring[self.pos + 1..self.pos + 1 + self.ka];
        // Two sums side by side (ka is even): independent additions.
        let (mut s0, mut s1) = (0.0, 0.0);
        for (b, t) in win.chunks_exact(2).zip(self.tables.chunks_exact(2)) {
            s0 += t[0][b[0] as usize];
            s1 += t[1][b[1] as usize];
        }
        Some(s0 + s1)
    }
}

/// A ÷2 FIR stage (odd length): output k is centred on input 2k.
#[derive(Clone)]
struct HalfStage {
    taps: Vec<f64>,
    /// The non-zero taps by index, used when most are zero (a half-band).
    sparse: Option<Vec<(usize, f64)>>,
    ring: Vec<f64>,
    pos: usize,
    seen: u64,
}

impl HalfStage {
    fn new(taps: Vec<f64>) -> HalfStage {
        let nz: Vec<(usize, f64)> = taps.iter().copied().enumerate().filter(|&(_, t)| t != 0.0).collect();
        let sparse = (nz.len() * 3 < taps.len() * 2).then_some(nz);
        let n = taps.len();
        HalfStage { taps, sparse, ring: vec![0.0; 2 * n], pos: 0, seen: 0 }
    }

    /// A half-band from `rate`: flat to `guard`, ≥ 150 dB down where its
    /// output folds below it. Every other tap is zero.
    fn half_band(rate: f64, guard: f64) -> HalfStage {
        let width = (rate / 2.0 - 2.0 * guard) / rate;
        let n = kaiser_taps(PCM_STAGE_DB, width);
        let n = if n % 4 == 1 { n + 2 } else { n }; // zeros at both ends otherwise
        let mut h = kaiser_lowpass(n, 0.25, PCM_STAGE_DB, 1.0);
        let mid = n / 2;
        for (i, t) in h.iter_mut().enumerate() {
            if i != mid && (i as isize - mid as isize) % 2 == 0 {
                *t = 0.0;
            }
        }
        HalfStage::new(h)
    }

    /// The last stage, to `2 · base`: flat to the pass edge, ≥ 150 dB down
    /// from the stop edge.
    fn last(rate: f64, b: Bands, gain: f64) -> HalfStage {
        let n = kaiser_taps(PCM_STAGE_DB, (b.stop_hz - b.pass_hz) / rate);
        HalfStage::new(kaiser_lowpass(n, (b.pass_hz + b.stop_hz) / 2.0 / rate, PCM_STAGE_DB, gain))
    }

    fn delay(&self) -> usize {
        self.taps.len() / 2
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
        self.seen = 0;
    }

    #[inline]
    fn push(&mut self, x: f64) -> Option<f64> {
        let n = self.taps.len();
        self.pos = if self.pos + 1 == n { 0 } else { self.pos + 1 };
        self.ring[self.pos] = x;
        self.ring[self.pos + n] = x;
        let i = self.seen;
        self.seen += 1;
        let d = self.delay() as u64;
        if i < d || (i - d) % 2 != 0 {
            return None;
        }
        let win = &self.ring[self.pos + 1..self.pos + 1 + n];
        Some(match &self.sparse {
            Some(nz) => nz.iter().map(|&(k, t)| t * win[k]).sum(),
            None => dot(win, &self.taps),
        })
    }
}

/// The PCM rate DSD at `rate` decodes to: twice its family base.
pub fn pcm_rate(rate: DsdRate) -> u32 {
    2 * rate.base
}

/// PCM samples `samples` one-bit samples at `rate` decode to: those
/// centred on one of them.
pub fn pcm_len(rate: DsdRate, samples: u64) -> u64 {
    samples.div_ceil((rate.multiple / 2) as u64)
}

/// `a · b` in four running sums, which the compiler can keep in vector
/// registers (one sum is a chain of dependent additions).
#[inline]
fn dot(a: &[f64], b: &[f64]) -> f64 {
    let mut s = [0.0; 4];
    let (ca, cb) = (a.chunks_exact(4), b.chunks_exact(4));
    let tail: f64 = ca.remainder().iter().zip(cb.remainder()).map(|(x, y)| x * y).sum();
    for (x, y) in ca.zip(cb) {
        for k in 0..4 {
            s[k] += x[k] * y[k];
        }
    }
    (s[0] + s[1]) + (s[2] + s[3]) + tail
}

/// One channel of DSD (MSB-first bytes) → PCM at `out_rate()`. Bytes in,
/// f64 out, any supported rate: the file reader and the analyzer's "after
/// the modulator" view both use it.
#[derive(Clone)]
pub struct DsdDecimator {
    rate: DsdRate,
    bits: BitStage,
    halves: Vec<HalfStage>,
    bufs: Vec<Vec<f64>>,
}

impl DsdDecimator {
    pub fn new(rate: DsdRate) -> DsdDecimator {
        let bit_rate = rate.bit_rate() as f64;
        let b = Bands::of(rate);
        let bits = BitStage::new(bit_rate, b.stop_hz);
        let mut halves = Vec::new();
        let mut r = bit_rate / 8.0;
        while r > 4.0 * rate.base as f64 + 0.5 {
            halves.push(HalfStage::half_band(r, b.stop_hz));
            r /= 2.0;
        }
        halves.push(HalfStage::last(r, b, DSD_TO_PCM_GAIN));
        let bufs = vec![Vec::new(); halves.len() + 1];
        DsdDecimator { rate, bits, halves, bufs }
    }

    /// Twice the family base: 88 200 or 96 000.
    pub fn out_rate(&self) -> u32 {
        pcm_rate(self.rate)
    }

    /// One-bit samples per output sample.
    pub fn factor(&self) -> u64 {
        (self.rate.multiple / 2) as u64
    }

    /// Output samples a stream of `samples` one-bit samples makes: those
    /// centred on one of them.
    pub fn output_len(&self, samples: u64) -> usize {
        pcm_len(self.rate, samples) as usize
    }

    /// Decodes the next bytes, appending what they complete to `out`. The
    /// output trails the input by the filters' half-lengths; `flush` gives
    /// the rest.
    pub fn process(&mut self, bytes: &[u8], out: &mut Vec<f64>) {
        let mut cur = std::mem::take(&mut self.bufs[0]);
        cur.clear();
        for &b in bytes {
            if let Some(y) = self.bits.push(b as u16) {
                cur.push(y);
            }
        }
        self.cascade(cur, out, false);
    }

    /// The end of the stream: the samples still inside the filters (and a
    /// few past the end, which a caller that knows the length truncates —
    /// `output_len`).
    pub fn flush(&mut self, out: &mut Vec<f64>) {
        let mut cur = std::mem::take(&mut self.bufs[0]);
        cur.clear();
        for _ in 0..self.bits.ka / 2 {
            if let Some(y) = self.bits.push(NO_BYTE) {
                cur.push(y);
            }
        }
        self.cascade(cur, out, true);
    }

    #[cfg(test)]
    /// Forgets the stream: the next byte starts a new one.
    pub fn reset(&mut self) {
        self.bits.reset();
        self.halves.iter_mut().for_each(HalfStage::reset);
    }

    fn cascade(&mut self, mut cur: Vec<f64>, out: &mut Vec<f64>, flush: bool) {
        for k in 0..self.halves.len() {
            let mut next = std::mem::take(&mut self.bufs[k + 1]);
            next.clear();
            let st = &mut self.halves[k];
            for &x in &cur {
                if let Some(y) = st.push(x) {
                    next.push(y);
                }
            }
            if flush {
                for _ in 0..st.delay() {
                    if let Some(y) = st.push(0.0) {
                        next.push(y);
                    }
                }
            }
            self.bufs[k] = cur;
            cur = next;
        }
        out.extend_from_slice(&cur);
        let last = self.halves.len();
        self.bufs[last] = cur;
    }
}

/// The last byte of a channel holding `valid` (1–7) samples: the bits past
/// them alternate, which averages to nothing, instead of whatever the file
/// padded with (a run of zeros would decode as a small step at the very end).
pub fn tail_byte(b: u8, valid: u32) -> u8 {
    let keep = !(0xFFu8 >> valid);
    (b & keep) | (0x55 & !keep)
}

/// Bytes per channel in one piece of a parallel decode: 1 MB (~3 s of
/// DSD64).
const SEGMENT: u64 = 1 << 20;

impl DsdDecimator {
    /// Bytes per output sample (`factor() / 8`): a decode started at a
    /// multiple of it is in step with one started at the beginning.
    fn unit(&self) -> u64 {
        self.factor() / 8
    }

    /// Bytes of input that reach an output sample through all the stages,
    /// with room to spare, in whole units: a decode started this far before
    /// a sample (and fed this far past it) computes it from the same values
    /// in the same order as one that ran from the start.
    fn reach_bytes(&self) -> u64 {
        let mut span = self.bits.ka as u64;
        let mut step = 1;
        for h in &self.halves {
            span += h.taps.len() as u64 * step;
            step *= 2;
        }
        let u = self.unit();
        (span + 2 * u).div_ceil(u) * u
    }
}

/// A whole DSD file as PCM: left and right (a mono file's one channel on
/// both) at the decimator's rate, exactly `output_len(samples)` long.
/// Decoded in 1 MB pieces on every core, bit for bit what one decoder run
/// from start to end makes.
pub fn decode_all(reader: &mut dyn DsdRead) -> Result<(Vec<f64>, Vec<f64>, u32), String> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    decode_pieces(reader, SEGMENT, threads)
}

/// `decode_all` on about `threads` threads (one per channel at least), each
/// of which runs `each` first — the player decodes behind the music this way,
/// on a few cores at a lower priority. The same samples, bit for bit.
pub fn decode_all_with(reader: &mut dyn DsdRead, threads: usize, each: fn()) -> Result<(Vec<f64>, Vec<f64>, u32), String> {
    decode_pieces_with(reader, SEGMENT, threads.max(1), each)
}

/// `decode_all` in pieces of `segment` bytes per channel, `threads` at a
/// time. Each piece is decoded by a fresh decoder from `reach_bytes` before
/// its start to as far past its end; its own samples are kept.
fn decode_pieces(reader: &mut dyn DsdRead, segment: u64, threads: usize) -> Result<(Vec<f64>, Vec<f64>, u32), String> {
    decode_pieces_with(reader, segment, threads, || {})
}

/// `decode_pieces` with `each` run first on every thread it starts.
fn decode_pieces_with(reader: &mut dyn DsdRead, segment: u64, threads: usize, each: fn()) -> Result<(Vec<f64>, Vec<f64>, u32), String> {
    let info = reader.info().clone();
    let proto = DsdDecimator::new(info.rate);
    let out_rate = proto.out_rate();
    let (unit, reach) = (proto.unit(), proto.reach_bytes());
    let seg = segment.max(unit).div_ceil(unit) * unit;
    let total = info.bytes();
    let len = proto.output_len(info.samples);
    let tail_bits = (info.samples % 8) as u32;
    let ch = info.channels;
    let mut outs: Vec<Vec<f64>> = vec![vec![0.0; len]; ch];
    {
        // Piece s writes output samples [s · seg/unit, (s + 1) · seg/unit).
        let per_piece = (seg / unit) as usize;
        let mut slots: Vec<std::slice::ChunksMut<'_, f64>> = outs.iter_mut().map(|o| o.chunks_mut(per_piece)).collect();
        let pieces = total.div_ceil(seg);
        let batch = (threads / ch).max(1) as u64;
        let mut first = 0;
        while first < pieces {
            let last = (first + batch).min(pieces);
            let mut jobs = Vec::new();
            for p in first..last {
                let a = p * seg;
                let b = (a + seg).min(total);
                let (lo, hi) = (a.saturating_sub(reach), (b + reach).min(total));
                let mut bufs = vec![vec![0u8; (hi - lo) as usize]; ch];
                reader.seek(lo)?;
                let mut got = 0;
                while got < bufs[0].len() {
                    let mut views: Vec<&mut [u8]> = bufs.iter_mut().map(|v| &mut v[got..]).collect();
                    let n = reader.read(&mut views)?;
                    if n == 0 {
                        return Err("DSD data ended early".to_string());
                    }
                    got += n;
                }
                if hi == total && tail_bits != 0 {
                    for v in bufs.iter_mut() {
                        let k = v.len() - 1;
                        v[k] = tail_byte(v[k], tail_bits);
                    }
                }
                jobs.push((lo, hi, a, bufs));
            }
            std::thread::scope(|sc| {
                for (lo, hi, a, bufs) in &jobs {
                    for (c, buf) in bufs.iter().enumerate() {
                        let Some(out) = slots[c].next() else { continue };
                        let mut dec = proto.clone();
                        let (lo, hi, a) = (*lo, *hi, *a);
                        sc.spawn(move || {
                            each();
                            let mut y = Vec::with_capacity(buf.len() / unit as usize + 64);
                            dec.process(buf, &mut y);
                            if hi == total {
                                dec.flush(&mut y);
                            }
                            let skip = ((a - lo) / unit) as usize;
                            let n = out.len().min(y.len().saturating_sub(skip));
                            debug_assert_eq!(n, out.len(), "a piece made fewer samples than it covers");
                            out[..n].copy_from_slice(&y[skip..skip + n]);
                        });
                    }
                }
            });
            first = last;
        }
    }
    let l = outs.swap_remove(0);
    let r = if outs.is_empty() { l.clone() } else { outs.swap_remove(0) };
    Ok((l, r, out_rate))
}

/// One-bit test signals.
#[cfg(test)]
pub(crate) mod test_signal {
    use super::PCM_FULL_SCALE_MODULATION;

    /// A second-order one-bit modulator at the bit rate, fed `x(t)` sample
    /// by sample (PCM full scale = 50 % modulation): bytes MSB first.
    pub fn modulate(x: impl Fn(f64) -> f64, bit_rate: f64, bits: usize) -> Vec<u8> {
        let (mut i1, mut i2, mut y) = (0.0f64, 0.0f64, -1.0f64);
        let mut out = vec![0u8; bits / 8];
        for n in 0..bits {
            let s = x(n as f64 / bit_rate) * PCM_FULL_SCALE_MODULATION;
            i1 += s - y;
            i2 += i1 - y;
            let bit = i2 >= 0.0;
            y = if bit { 1.0 } else { -1.0 };
            out[n / 8] |= (bit as u8) << (7 - n % 8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_signal::modulate;
    use super::*;

    /// |H(f)| in dB of `taps` at `f` (a fraction of their sample rate).
    fn response_db(taps: &[f64], f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f;
        let (mut re, mut im) = (0.0, 0.0);
        for (i, t) in taps.iter().enumerate() {
            re += t * (w * i as f64).cos();
            im -= t * (w * i as f64).sin();
        }
        20.0 * (re * re + im * im).sqrt().max(1e-300).log10()
    }

    fn worst(taps: &[f64], gain: f64, from: f64, to: f64, rate: f64, pass: bool) -> f64 {
        let g = 20.0 * gain.log10();
        let steps = 2000;
        let mut w: f64 = if pass { 0.0 } else { -400.0 };
        for i in 0..=steps {
            let f = from + (to - from) * i as f64 / steps as f64;
            let d = response_db(taps, f / rate) - g;
            w = if pass { w.max(d.abs()) } else { w.max(d) };
        }
        w
    }

    /// The bit stage's taps as an ordinary filter.
    fn bit_taps(s: &BitStage) -> Vec<f64> {
        (0..s.ka)
            .flat_map(|w| (0..8).map(move |p| (w, p)))
            .map(|(w, p)| {
                let t = &s.tables[w];
                // Byte with only bit p set minus byte with none: 2 · tap.
                (t[0x80 >> p] - t[0]) / 2.0
            })
            .collect()
    }

    #[test]
    fn every_stage_meets_its_numbers_at_every_rate() {
        for rate in [DsdRate::DSD64, DsdRate::DSD128, DsdRate::DSD256, DsdRate::new(44_100, 512).unwrap(), DsdRate::new(48_000, 64).unwrap(), DsdRate::new(48_000, 1024).unwrap()] {
            let d = DsdDecimator::new(rate);
            let b = Bands::of(rate);
            let guard = b.stop_hz;
            let fs = rate.bit_rate() as f64;
            let bt = bit_taps(&d.bits);
            assert!(worst(&bt, 1.0, 0.0, guard, fs, true) < 1e-5, "{rate:?} bit stage passband");
            let r1 = fs / 8.0;
            let mut k = 1.0;
            while k * r1 < fs / 2.0 {
                let s = worst(&bt, 1.0, k * r1 - guard, (k * r1 + guard).min(fs / 2.0), fs, false);
                assert!(s < -158.0, "{rate:?} bit stage at {} kHz: {s:.1} dB", k * r1 / 1e3);
                k += 1.0;
            }
            let mut r = r1;
            let (last, halves) = d.halves.split_last().unwrap();
            for h in halves {
                assert!(worst(&h.taps, 1.0, 0.0, guard, r, true) < 1e-5);
                let s = worst(&h.taps, 1.0, r / 2.0 - guard, r / 2.0, r, false);
                assert!(s < -148.0, "{rate:?} half-band at {} kHz: {s:.1} dB", r / 1e3);
                r /= 2.0;
            }
            assert_eq!(r, 4.0 * rate.base as f64);
            assert!(worst(&last.taps, DSD_TO_PCM_GAIN, 0.0, b.pass_hz, r, true) < 1e-4, "{rate:?} passband ±0.0001 dB");
            let s = worst(&last.taps, DSD_TO_PCM_GAIN, b.stop_hz, r / 2.0, r, false);
            assert!(s < -148.0, "{rate:?} last stage stopband: {s:.1} dB");
            assert_eq!(d.out_rate(), 2 * rate.base);
        }
    }

    fn decode(rate: DsdRate, bytes: &[u8]) -> Vec<f64> {
        let mut d = DsdDecimator::new(rate);
        let mut out = Vec::new();
        for piece in bytes.chunks(10_007) {
            d.process(piece, &mut out);
        }
        d.flush(&mut out);
        out.truncate(d.output_len(bytes.len() as u64 * 8));
        out
    }

    /// Amplitude and phase of a tone at `f` in `x` (least squares on sin and
    /// cos), and the RMS of what is left.
    fn tone_fit(x: &[f64], f: f64, rate: f64, t0: f64) -> (f64, f64, f64) {
        let w = 2.0 * std::f64::consts::PI * f / rate;
        let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            let (s, c) = ((w * (i as f64 + t0)).sin(), (w * (i as f64 + t0)).cos());
            ss += s * s;
            sc += s * c;
            cc += c * c;
            xs += v * s;
            xc += v * c;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
        let mut res = 0.0;
        for (i, v) in x.iter().enumerate() {
            let e = v - a * (w * (i as f64 + t0)).sin() - b * (w * (i as f64 + t0)).cos();
            res += e * e;
        }
        ((a * a + b * b).sqrt(), b.atan2(a), (res / x.len() as f64).sqrt())
    }

    #[test]
    fn a_tone_decodes_at_its_level_in_time_with_low_noise() {
        // −6 dBFS 1 kHz sine, DSD64, 0.25 s: level, phase (time alignment)
        // and the residual — the second-order test modulator's in-band
        // noise is the floor, nothing the decoder adds.
        let rate = DsdRate::DSD64;
        let fs = rate.bit_rate() as f64;
        let f = 1_000.0;
        let bits = (fs * 0.25) as usize / 8 * 8;
        let bytes = modulate(|t| 0.5 * (2.0 * std::f64::consts::PI * f * t).sin(), fs, bits);
        let out = decode(rate, &bytes);
        assert_eq!(out.len(), bits / 32);
        let mid = &out[2000..out.len() - 2000];
        let (amp, phase, res) = tone_fit(mid, f, 88_200.0, 2000.0);
        let db = 20.0 * (amp / 0.5).log10();
        assert!(db.abs() < 0.01, "level {db:.4} dB");
        assert!(phase.abs() < 1e-3, "phase {phase:.2e} rad: the output is centred on its bits");
        let snr = 20.0 * (amp / std::f64::consts::SQRT_2 / res).log10();
        assert!(snr > 60.0, "SNR {snr:.1} dB");
    }

    #[test]
    fn levels_past_the_sacd_reference_are_not_clipped() {
        // 70 % modulation = PCM 1.4 (+2.9 dBFS): decoded as it is.
        let rate = DsdRate::DSD128;
        let fs = rate.bit_rate() as f64;
        let bits = (fs * 0.1) as usize / 8 * 8;
        let bytes = modulate(|t| 1.4 * (2.0 * std::f64::consts::PI * 500.0 * t).sin(), fs, bits);
        let out = decode(rate, &bytes);
        let (amp, _, _) = tone_fit(&out[2000..out.len() - 2000], 500.0, 88_200.0, 2000.0);
        assert!((amp - 1.4).abs() < 0.002, "amplitude {amp}");
        assert!(out.iter().any(|v| v.abs() > 1.3));
    }

    #[test]
    fn ultrasonic_patterns_and_idle_decode_to_silence() {
        // Square waves at 176.4, 88.2 and 44.1 kHz (their harmonics land on
        // every folding frequency, 176.4 and 88.2 straight onto DC), the
        // idle pattern 0x69 and bit-rate alternation: nothing below 30 kHz
        // in any of them, so nothing must come out.
        for rate in [DsdRate::DSD64, DsdRate::DSD128, DsdRate::DSD256] {
            let n = rate.bit_rate() as usize / 8 / 10;
            let m = rate.multiple as usize;
            let patterns: [(&str, Box<dyn Fn(usize) -> u8>); 6] = [
                // 58.8 kHz (DSD64: 29.4 kHz, its transition band — 0x69 again there).
                ("58.8k", Box::new(move |i| if m < 128 { 0x69 } else if (i % (3 * m / 32)) < 3 * m / 64 { 0xFF } else { 0x00 })),
                ("0x69", Box::new(|_| 0x69)),
                ("0x55", Box::new(|_| 0x55)),
                ("176.4k", Box::new(move |i| if (i * 64 / rate.multiple as usize) % 2 == 0 { 0xFF } else { 0x00 })),
                ("88.2k", Box::new(move |i| if (i * 32 / rate.multiple as usize) % 2 == 0 { 0xFF } else { 0x00 })),
                ("44.1k", Box::new(move |i| if (i * 16 / rate.multiple as usize) % 2 == 0 { 0xFF } else { 0x00 })),
            ];
            for (name, p) in patterns.iter() {
                let bytes: Vec<u8> = (0..n).map(p).collect();
                let out = decode(rate, &bytes);
                let edge = 1000;
                let peak = out[edge..out.len() - edge].iter().fold(0.0f64, |m, v| m.max(v.abs()));
                let db = 20.0 * peak.max(1e-300).log10();
                assert!(db < -130.0, "{rate:?} {name}: {db:.1} dBFS");
            }
        }
    }

    #[test]
    fn noise_above_the_band_does_not_come_through() {
        // An idle second-order modulator: its noise rises steeply past
        // 20 kHz. Spectrum of the output: below 20 kHz the modulator's own
        // floor; from 30 kHz to Nyquist, 150 dB under it.
        use realfft::RealFftPlanner;
        let rate = DsdRate::DSD64;
        let fs = rate.bit_rate() as f64;
        let n_out = 1 << 15;
        let bits = (n_out + 4000) * 32;
        let bytes = modulate(|t| 0.25 * (2.0 * std::f64::consts::PI * 997.0 * t).sin(), fs, bits);
        let out = decode(rate, &bytes);
        let x = &out[2000..2000 + n_out];
        let mut buf: Vec<f64> = x
            .iter()
            .enumerate()
            .map(|(i, v)| v * (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n_out as f64).cos()))
            .collect();
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n_out);
        let mut spec = fft.make_output_vec();
        fft.process(&mut buf, &mut spec).unwrap();
        let band = |lo: f64, hi: f64| {
            let (a, b) = ((lo / 88_200.0 * n_out as f64) as usize, (hi / 88_200.0 * n_out as f64) as usize);
            spec[a..b].iter().map(|c| c.norm_sqr()).sum::<f64>() / (b - a) as f64
        };
        // Per-bin power: the modulator's noise between 15 and 20 kHz…
        let audio = band(15_000.0, 20_000.0);
        // …and what is left past the guard.
        let ultra = band(31_000.0, 44_000.0);
        let db = 10.0 * (ultra / audio).log10();
        assert!(db < -100.0, "30–44 kHz is {db:.1} dB under the 15–20 kHz floor");
    }

    #[test]
    fn chunking_and_reset_do_not_change_the_output() {
        let rate = DsdRate::DSD128;
        let fs = rate.bit_rate() as f64;
        let bytes = modulate(|t| 0.3 * (2.0 * std::f64::consts::PI * 3_000.0 * t).sin(), fs, 400_000 * 8);
        let whole = {
            let mut d = DsdDecimator::new(rate);
            let mut out = Vec::new();
            d.process(&bytes, &mut out);
            d.flush(&mut out);
            out
        };
        let mut d = DsdDecimator::new(rate);
        let mut junk = Vec::new();
        d.process(&[0x12, 0x34, 0x56], &mut junk);
        d.reset();
        let mut out = Vec::new();
        for piece in bytes.chunks(333) {
            d.process(piece, &mut out);
        }
        d.flush(&mut out);
        assert_eq!(out, whole);
    }

    #[test]
    fn tail_byte_keeps_the_valid_bits() {
        assert_eq!(tail_byte(0b1110_0000, 3), 0b1111_0101);
        assert_eq!(tail_byte(0xFF, 1), 0b1101_0101);
        assert_eq!(tail_byte(0x00, 7), 0x01);
    }

    #[test]
    fn a_whole_file_decodes_both_channels_to_its_length() {
        use super::super::dsf::test_files::{dsf, write};
        use super::super::dsf::open;
        let rate = DsdRate::DSD64;
        let fs = rate.bit_rate() as f64;
        let samples = 300_000 * 8 - 3;
        let l = modulate(|t| 0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin(), fs, 300_000 * 8);
        let r = modulate(|t| 0.25 * (2.0 * std::f64::consts::PI * 2_000.0 * t).sin(), fs, 300_000 * 8);
        let (_d, path) = write(&dsf(&[l, r], 2_822_400, samples, 4096, false, None), "w.dsf");
        let mut rd = open(&path).unwrap();
        let (ol, or, out_rate) = decode_all(&mut *rd).unwrap();
        assert_eq!(out_rate, 88_200);
        assert_eq!(ol.len(), (samples as usize).div_ceil(32));
        assert_eq!(or.len(), ol.len());
        let (al, _, _) = tone_fit(&ol[2000..ol.len() - 2000], 1_000.0, 88_200.0, 2000.0);
        let (ar, _, _) = tone_fit(&or[2000..or.len() - 2000], 2_000.0, 88_200.0, 2000.0);
        assert!((al - 0.5).abs() < 0.001 && (ar - 0.25).abs() < 0.001, "{al} {ar}");
    }

    /// Decoding speed, one channel, in multiples of real time (run by hand:
    /// `cargo test --release --bins decode_speed -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn decode_speed() {
        use super::super::dsf::test_files::noise;
        for rate in [DsdRate::DSD64, DsdRate::DSD128, DsdRate::DSD256, DsdRate::new(44_100, 512).unwrap()] {
            let seconds = 10.0;
            let bytes = noise((rate.bit_rate() as f64 / 8.0 * seconds) as usize, 5);
            let mut d = DsdDecimator::new(rate);
            let mut out = Vec::new();
            let t = std::time::Instant::now();
            d.process(&bytes, &mut out);
            let s = t.elapsed().as_secs_f64();
            println!("{rate:?}: {:.0}x real time per channel", seconds / s);
        }
    }

    #[test]
    fn the_band_follows_the_rate() {
        // DSD64: flat to 20 kHz, 30 kHz on stopped. DSD128 and up: a 35 kHz
        // tone comes through at its level; 44.1 kHz on is stopped.
        assert_eq!(Bands::of(DsdRate::DSD64), Bands { pass_hz: 20_000.0, stop_hz: 30_000.0 });
        assert_eq!(Bands::of(DsdRate::DSD128), Bands { pass_hz: 40_000.0, stop_hz: 44_100.0 });
        assert_eq!(Bands::of(DsdRate::new(48_000, 256).unwrap()), Bands { pass_hz: 40_000.0, stop_hz: 48_000.0 });
        for (rate, through) in [(DsdRate::DSD64, false), (DsdRate::DSD128, true), (DsdRate::DSD256, true), (DsdRate::new(44_100, 512).unwrap(), true)] {
            let fs = rate.bit_rate() as f64;
            let bits = (fs * 0.1) as usize / 8 * 8;
            let bytes = modulate(|t| 0.5 * (2.0 * std::f64::consts::PI * 35_000.0 * t).sin(), fs, bits);
            let out = decode(rate, &bytes);
            let (amp, _, _) = tone_fit(&out[2000..out.len() - 2000], 35_000.0, 88_200.0, 2000.0);
            let db = 20.0 * (amp / 0.5).max(1e-300).log10();
            if through {
                assert!(db.abs() < 0.01, "{rate:?}: 35 kHz at {db:.4} dB");
            } else {
                assert!(db < -120.0, "{rate:?}: 35 kHz at {db:.1} dB");
            }
        }
    }

    /// A DSD file held in memory.
    struct MemDsd {
        info: super::super::dsf::DsdInfo,
        data: Vec<Vec<u8>>,
        pos: u64,
    }

    impl MemDsd {
        fn new(rate: DsdRate, samples: u64, data: Vec<Vec<u8>>) -> MemDsd {
            let info = super::super::dsf::DsdInfo { rate, channels: data.len(), samples, tags: Vec::new(), visuals: Vec::new() };
            MemDsd { info, data, pos: 0 }
        }
    }

    impl DsdRead for MemDsd {
        fn info(&self) -> &super::super::dsf::DsdInfo {
            &self.info
        }
        fn read(&mut self, out: &mut [&mut [u8]]) -> Result<usize, String> {
            let n = out[0].len().min(self.info.bytes().saturating_sub(self.pos) as usize);
            let at = self.pos as usize;
            for (o, d) in out.iter_mut().zip(&self.data) {
                o[..n].copy_from_slice(&d[at..at + n]);
            }
            self.pos += n as u64;
            Ok(n)
        }
        fn seek(&mut self, byte: u64) -> Result<(), String> {
            self.pos = byte.min(self.info.bytes());
            Ok(())
        }
        fn reopen(&self) -> Result<Box<dyn DsdRead>, String> {
            Ok(Box::new(MemDsd { info: self.info.clone(), data: self.data.clone(), pos: 0 }))
        }
    }

    #[test]
    fn pieces_decode_bit_for_bit_as_one_run() {
        use super::super::dsf::test_files::noise;
        for rate in [DsdRate::DSD64, DsdRate::DSD128, DsdRate::DSD256, DsdRate::new(48_000, 64).unwrap()] {
            let fs = rate.bit_rate() as f64;
            let bytes = 150_003;
            let samples = bytes as u64 * 8 - 3;
            let l = modulate(|t| 0.5 * (2.0 * std::f64::consts::PI * 1_234.0 * t).sin(), fs, bytes * 8);
            let r = noise(bytes, 11);
            // One decoder from start to end, the last byte's padding as decode_all treats it.
            let one = |mut v: Vec<u8>| {
                let k = v.len() - 1;
                v[k] = tail_byte(v[k], 5);
                let mut d = DsdDecimator::new(rate);
                let mut out = Vec::new();
                d.process(&v, &mut out);
                d.flush(&mut out);
                out.truncate(d.output_len(samples));
                out
            };
            let (sl, sr) = (one(l.clone()), one(r.clone()));
            for (segment, threads) in [(7_001, 3), (40_000, 8), (1 << 20, 2)] {
                let mut m = MemDsd::new(rate, samples, vec![l.clone(), r.clone()]);
                let (pl, pr, _) = decode_pieces(&mut m, segment, threads).unwrap();
                assert!(pl == sl && pr == sr, "{rate:?}, pieces of {segment}: not bit-identical");
            }
        }
    }

    /// `decode_all` on a four-minute stereo track (run by hand:
    /// `cargo test --profile fast --bins decode_all_speed -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn decode_all_speed() {
        use super::super::dsf::test_files::noise;
        for rate in [DsdRate::DSD64, DsdRate::DSD256] {
            let bytes = rate.bit_rate() as usize / 8 * 240;
            let ch = noise(bytes, 3);
            let mut m = MemDsd::new(rate, bytes as u64 * 8, vec![ch.clone(), ch]);
            let t = std::time::Instant::now();
            let (l, _, _) = decode_all(&mut m).unwrap();
            println!("{rate:?}: 4-min stereo track in {:.2} s ({} samples)", t.elapsed().as_secs_f64(), l.len());
        }
    }
}
