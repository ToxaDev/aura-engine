//! The live analyzer on a stream (the radio): S beside O.
//!
//! S is the stream as it came in — the live source's shadow
//! (`LiveSource::read_raw`), before the rack's source stages — read on the
//! very frames the output played. A stream chain's output index is its
//! source frame times the chain's factor L (BIT-PERFECT: L = 1, the shadow
//! itself), so what O measured at output index i is S's frame i / L: the two
//! meet by index, to the frame, not by an estimate of the delay between them.
//!
//! Measured live: loudness M and S, the true peak and the RMS of the last
//! 3 s, and the spectrogram — S's columns, and O's on S's grid: O's FFT is L
//! times longer and its hop L times larger, so column k is the same moment
//! and band j the same frequency in both (O's bands go on above S's Nyquist
//! at the same step, as the files' O tiles do). Times are the stream's own
//! clock — the source frame over its rate, the status's `positionS` — which
//! a reopened device (BIT-PERFECT on or off) keeps.
//!
//! It travels as the `STRM` tail of the live frame (AAN1, after SB-EXT),
//! only on a stream:
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | `"STRM"` |
//! | 4 | u8 version (2; 1 had no waveform) |
//! | 5 | u8 flags: 1 S measured, 2 BIT-PERFECT (O is S), 4 the shadow no longer held frames S wanted |
//! | 6 | u16 L |
//! | 8 | u32 the stream's rate (S) |
//! | 12 | u32 the output's rate (O) |
//! | 16 | f64 the stream time after the newest frame measured, s |
//! | 24 | f32 × 5: S LUFS-M, S LUFS-S, S true peak of the last 3 s (dBTP), S RMS of the last 3 s, O RMS of the last 3 s (dBFS) |
//! | 44 | u16 n, then n × (f32 time s, f32 LUFS-S, f32 LUFS-M): S's loudness since the last frame |
//! | then | S's columns, then O's: u16 bands, u32 hop (samples at its rate), u32 the first column's number (column k begins at sample k·hop), u16 n, n × bands bytes (0 = −120 dB, 255 = 0 dB) |
//! | then | S's waveform, then O's: u16 n, n × (u32 block, i16 × 6: min, max, RMS of L, then of R, of full scale); block k = the stream's frames k·256..(k+1)·256 (O's: output frames, × L) |
//!
//! The columns and the blocks are those made since the publish before last:
//! each goes out in two frames running. The page asks for the newest frame
//! (the route gives no other) a little slower than they come, so it misses
//! one now and then; with this it loses nothing, and it keeps a column or a
//! block it holds already as it is.

use std::collections::VecDeque;
use std::sync::{Arc, Weak};

use rustfft::num_complex::Complex;

use crate::player::radio::live::LiveSource;
use crate::player::radio::RadioShared;
use crate::player::render::ChainDesc;
use super::live::LoudnessPoint;
use super::loudness::LoudnessMeter;
use super::peaks::{lin_to_db, EburTruePeak};
use super::spectra::{
    spec_bands_on, spec_fft_len, spec_ln_step, LogBins, Stft, SPEC_BETA, SPEC_DB_FLOOR, SPEC_DB_RANGE, SPEC_F0_HZ,
    SPEC_LOG_BINS,
};
use super::stream_totals::{take_reset, Frames, TOTALS};

/// The live true peak's and RMS's window: 30 publishes of 100 ms (3 s), as O's.
const WINDOW_BLOCKS: usize = 30;

/// Columns held between two publishes at most (≈ 12 s).
const COLS_KEPT: usize = 256;

const FLAG_S: u8 = 0x01;
const FLAG_DIRECT: u8 = 0x02;
const FLAG_MISSED: u8 = 0x04;

/// The waveform's block: frames of the stream (S); O's is this times L —
/// the same stretch of time, as one entry of the files' wave tiles.
pub const ENV_BLOCK: u64 = 256;

/// A signal's waveform as it plays: per block of `per` frames (block k
/// holds frames k·per..(k+1)·per of the stream's own count), the least, the
/// greatest and the RMS of each channel, as i16 of full scale. The page
/// keeps them (its history); here wait the blocks finished since the last
/// publish, and those it sent (they go once more: a frame the page misses
/// loses nothing).
struct Env {
    per: u64,
    /// The block filling now (its number) and its sums: min, max, Σx² per channel.
    block: Option<u64>,
    n: u64,
    acc: [(f64, f64, f64); 2],
    done: Vec<(u32, [i16; 6])>,
    sent: Vec<(u32, [i16; 6])>,
}

impl Env {
    fn new(per: u64) -> Env {
        Env { per: per.max(1), block: None, n: 0, acc: [(0.0, 0.0, 0.0); 2], done: Vec::new(), sent: Vec::new() }
    }

    /// Frames `l`/`r` from frame `start` on.
    fn push(&mut self, start: u64, l: &[f64], r: &[f64]) {
        for (j, (&a, &b)) in l.iter().zip(r).enumerate() {
            let i = start + j as u64;
            let k = i / self.per;
            if self.block != Some(k) {
                self.finish();
                self.block = Some(k);
            }
            for (acc, x) in self.acc.iter_mut().zip([a, b]) {
                if self.n == 0 {
                    *acc = (x, x, 0.0);
                }
                acc.0 = acc.0.min(x);
                acc.1 = acc.1.max(x);
                acc.2 += x * x;
            }
            self.n += 1;
            if (i + 1) % self.per == 0 {
                self.finish();
            }
        }
    }

    /// The block filling now goes out (a jump ends it early).
    fn finish(&mut self) {
        let Some(k) = self.block.take() else { return };
        if self.n > 0 {
            let q = |v: f64| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            let n = self.n as f64;
            let [(a0, a1, a2), (b0, b1, b2)] = self.acc;
            self.done.push((k as u32, [q(a0), q(a1), q((a2 / n).sqrt()), q(b0), q(b1), q((b2 / n).sqrt())]));
        }
        self.n = 0;
    }

    /// u16 n, then n × (u32 block, i16 × 6: min L, max L, RMS L, min R, max R,
    /// RMS R): the blocks the last publish sent, then the ones finished since
    /// (sent once more next time).
    fn encode_take(&mut self, w: &mut Vec<u8>) {
        let n = (self.sent.len() + self.done.len()).min(u16::MAX as usize);
        w.extend_from_slice(&(n as u16).to_le_bytes());
        for (k, v) in self.sent.iter().chain(&self.done).take(n) {
            w.extend_from_slice(&k.to_le_bytes());
            for x in v {
                w.extend_from_slice(&x.to_le_bytes());
            }
        }
        self.sent = std::mem::take(&mut self.done);
    }
}

/// The mean square of the last 3 s, both channels, in 100 ms blocks.
struct Rms3 {
    block: usize,
    sum: f64,
    n: usize,
    ring: VecDeque<f64>,
}

impl Rms3 {
    fn new(rate: u32) -> Rms3 {
        Rms3 { block: ((rate as f64 * 0.1).round() as usize).max(1), sum: 0.0, n: 0, ring: VecDeque::new() }
    }

    fn push(&mut self, l: &[f64], r: &[f64]) {
        for (a, b) in l.iter().zip(r) {
            self.sum += a * a + b * b;
            self.n += 1;
            if self.n == self.block {
                if self.ring.len() == WINDOW_BLOCKS {
                    self.ring.pop_front();
                }
                self.ring.push_back(self.sum);
                self.sum = 0.0;
                self.n = 0;
            }
        }
    }

    /// dBFS (RMS of both channels, as the files' RMS); NaN before a sample
    /// or in digital silence.
    fn db(&self) -> f64 {
        let count = self.ring.len() * self.block + self.n;
        let sum = self.ring.iter().sum::<f64>() + self.sum;
        if count == 0 || sum <= 0.0 {
            return f64::NAN;
        }
        10.0 * (sum / (2 * count) as f64).log10()
    }
}

/// Spectrogram columns of one signal on a fixed grid: column k is the frame
/// that begins at sample k·hop (absolute), so two signals whose sample
/// indices are in the ratio of their hops make columns of the same moments.
struct Strip {
    stft: Stft,
    bands: LogBins,
    n_bands: usize,
    power: Vec<f64>,
    fft_buf: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    carry_l: Vec<f64>,
    carry_r: Vec<f64>,
    /// The absolute index of `carry_l[0]` (None: not aligned yet).
    at: Option<u64>,
    /// Columns made since the publish before last, the first of them number
    /// `first`; the first `sent` of them went out in the last publish (they
    /// go once more: a frame the page misses loses nothing).
    cols: VecDeque<Vec<u8>>,
    first: u64,
    sent: usize,
}

impl Strip {
    /// `n`-point frames at `rate`, `n_bands` log bands from 20 Hz at `ln_step`.
    fn new(n: usize, rate: u32, n_bands: usize, ln_step: f64) -> Strip {
        Strip {
            stft: Stft::new(n, n / 2, SPEC_BETA),
            bands: LogBins::with_step(n, rate, n_bands, SPEC_F0_HZ, ln_step),
            n_bands,
            power: Vec::new(),
            fft_buf: Vec::new(),
            scratch: Vec::new(),
            carry_l: Vec::new(),
            carry_r: Vec::new(),
            at: None,
            cols: VecDeque::new(),
            first: 0,
            sent: 0,
        }
    }

    /// Samples from absolute index `at` on. A jump (not where the last ones
    /// ended) starts afresh at the next multiple of the hop. `on_col` sees
    /// each column's power spectrum.
    fn push(&mut self, at: u64, l: &[f64], r: &[f64], mut on_col: impl FnMut(&[f64])) {
        let hop = self.stft.hop as u64;
        let (mut l, mut r) = (l, r);
        if self.at.map(|a| a + self.carry_l.len() as u64) != Some(at) {
            self.carry_l.clear();
            self.carry_r.clear();
            let skip = (((hop - at % hop) % hop) as usize).min(l.len());
            l = &l[skip..];
            r = &r[skip..];
            self.at = if l.is_empty() { None } else { Some(at + skip as u64) };
            if l.is_empty() {
                return;
            }
        }
        self.carry_l.extend_from_slice(l);
        self.carry_r.extend_from_slice(r);
        let n = self.stft.n;
        while self.carry_l.len() >= n {
            self.stft.frame_power_lr_with(
                &self.carry_l[..n],
                &self.carry_r[..n],
                &mut self.fft_buf,
                &mut self.scratch,
                &mut self.power,
            );
            on_col(&self.power);
            let mut col = vec![0u8; self.n_bands];
            self.bands.to_u8(&self.power, SPEC_DB_FLOOR, SPEC_DB_RANGE, &mut col);
            let a = self.at.unwrap_or(0);
            let k = a / hop;
            if self.cols.is_empty() || self.first + self.cols.len() as u64 != k {
                self.cols.clear();
                self.first = k;
                self.sent = 0;
            }
            if self.cols.len() == COLS_KEPT {
                self.cols.pop_front();
                self.first += 1;
                self.sent = self.sent.saturating_sub(1);
            }
            self.cols.push_back(col);
            self.carry_l.drain(..hop as usize);
            self.carry_r.drain(..hop as usize);
            self.at = Some(a + hop);
        }
    }

    /// The columns the last call sent and those made since: u16 bands, u32
    /// hop, u32 the first one's number, u16 count, the bytes. The new ones
    /// go once more next time.
    fn encode_take(&mut self, w: &mut Vec<u8>) {
        w.extend_from_slice(&(self.n_bands as u16).to_le_bytes());
        w.extend_from_slice(&(self.stft.hop as u32).to_le_bytes());
        w.extend_from_slice(&(self.first as u32).to_le_bytes());
        w.extend_from_slice(&(self.cols.len() as u16).to_le_bytes());
        for c in &self.cols {
            w.extend_from_slice(c);
        }
        self.cols.drain(..self.sent);
        self.first += self.sent as u64;
        self.sent = self.cols.len();
    }
}

/// What the live thread measures of a stream beside O (see the module doc).
pub struct StreamTap {
    radio: Weak<RadioShared>,
    live: Arc<LiveSource>,
    src_rate: u32,
    out_rate: u32,
    /// The chain's factor: output index = source frame × l.
    l: u64,
    /// BIT-PERFECT: the output is the shadow itself.
    direct: bool,
    /// The next source frame S takes (None: none taken yet).
    next_src: Option<u64>,
    s_loud: LoudnessMeter,
    s_tp: EburTruePeak,
    s_tp_ring: VecDeque<f64>,
    s_rms: Rms3,
    o_rms: Rms3,
    s_spec: Strip,
    o_spec: Strip,
    /// The waveform of S and of O (block = `ENV_BLOCK` stream frames).
    s_env: Env,
    o_env: Env,
    /// S's loudness points since the last publish.
    pts: Vec<LoudnessPoint>,
    /// Frames S wanted that the shadow no longer held.
    missed: bool,
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    /// Publishes until the totals' JSON is built again (once a second).
    json_due: u32,
}

/// Keep `slot` on the stream `desc` plays (a chain at `out_rate`): a new tap
/// when it is another session's or the chain's factor or rate changed;
/// none off a stream, or when the stream's rate times L is not the
/// output's (S and O would not meet by index).
pub fn ensure(slot: &mut Option<StreamTap>, desc: &ChainDesc, out_rate: u32) {
    if !desc.stream {
        *slot = None;
        return;
    }
    let l = desc.l.max(1) as u64;
    let direct = desc.source == "direct";
    if let Some(t) = slot.as_mut() {
        if Weak::ptr_eq(&t.radio, &desc.radio) && t.l == l && t.out_rate == out_rate {
            t.direct = direct;
            return;
        }
    }
    let live = desc.radio.upgrade().and_then(|r| r.live.get().cloned());
    *slot = live
        .filter(|lv| lv.rate() as u64 * l == out_rate as u64)
        .map(|lv| StreamTap::new(desc.radio.clone(), lv, out_rate, l, direct));
}

impl StreamTap {
    fn new(radio: Weak<RadioShared>, live: Arc<LiveSource>, out_rate: u32, l: u64, direct: bool) -> StreamTap {
        let src_rate = live.rate();
        let n = spec_fft_len(src_rate);
        let step = spec_ln_step(src_rate);
        StreamTap {
            radio,
            live,
            src_rate,
            out_rate,
            l,
            direct,
            next_src: None,
            s_loud: LoudnessMeter::new(src_rate as f64),
            s_tp: EburTruePeak::new(src_rate),
            s_tp_ring: VecDeque::with_capacity(WINDOW_BLOCKS),
            s_rms: Rms3::new(src_rate),
            o_rms: Rms3::new(out_rate),
            s_spec: Strip::new(n, src_rate, SPEC_LOG_BINS, step),
            o_spec: Strip::new(n * l as usize, out_rate, spec_bands_on(src_rate, out_rate), step),
            s_env: Env::new(ENV_BLOCK),
            o_env: Env::new(ENV_BLOCK * l),
            pts: Vec::new(),
            missed: false,
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            json_due: 0,
        }
    }

    /// The stream time after the newest frame measured, s.
    pub fn clock_s(&self) -> f64 {
        self.next_src.unwrap_or(0) as f64 / self.src_rate.max(1) as f64
    }

    /// The output frames `o_l`/`o_r` from chain output index `idx0` on: O's
    /// own measures, and S's on the source frames they complete, read from
    /// the shadow (a source frame is S's once its L output frames played).
    pub fn feed(&mut self, idx0: u64, o_l: &[f64], o_r: &[f64]) {
        let n = o_l.len().min(o_r.len());
        if n == 0 {
            return;
        }
        self.o_rms.push(&o_l[..n], &o_r[..n]);
        self.o_spec.push(idx0, &o_l[..n], &o_r[..n], |_| {});
        self.o_env.push(idx0, &o_l[..n], &o_r[..n]);
        let s1 = (idx0 + n as u64) / self.l;
        // Where S goes on: from the last frame taken, unless the output
        // jumped (a frame either way is rounding, not a jump).
        let s0 = match self.next_src {
            Some(s) if s + 1 >= idx0 / self.l && s <= idx0 / self.l + 1 => s,
            _ => idx0 / self.l,
        };
        if s1 <= s0 {
            return;
        }
        let first = self.live.raw_start().max(0) as u64;
        let from = s0.max(first);
        if from > s0 {
            self.missed = true;
        }
        let k = s1.saturating_sub(from) as usize;
        self.buf_l.resize(k, 0.0);
        self.buf_r.resize(k, 0.0);
        if k > 0 {
            self.live.read_raw(0, from as i64, &mut self.buf_l);
            self.live.read_raw(1, from as i64, &mut self.buf_r);
        }
        // The session's and the song's totals (stream_totals.rs).
        let mut totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        if take_reset() {
            totals.reset_session();
        }
        totals.feed(&self.radio, &Frames {
            src_rate: self.src_rate,
            n_fft: self.s_spec.stft.n,
            hop: self.s_spec.stft.hop,
            out_rate: self.out_rate,
            l: self.l,
            idx0,
            o_l: &o_l[..n],
            o_r: &o_r[..n],
            s_from: from,
            s_l: &self.buf_l,
            s_r: &self.buf_r,
        });
        if k > 0 {
            self.s_loud.push(&self.buf_l, &self.buf_r);
            self.s_tp.push(&self.buf_l, &self.buf_r);
            self.s_rms.push(&self.buf_l, &self.buf_r);
            self.s_spec.push(from, &self.buf_l, &self.buf_r, |p| totals.column(p));
            self.s_env.push(from, &self.buf_l, &self.buf_r);
        }
        self.next_src = Some(s1);
    }

    /// The `STRM` tail for the next live frame (10 Hz): S's values now, its
    /// loudness point, and the columns of S and O made since the last one.
    pub fn publish(&mut self) -> Vec<u8> {
        // The session's and the songs' totals for the page, once a second.
        if self.json_due == 0 {
            super::stream_totals::publish(self.direct);
            self.json_due = 10;
        }
        self.json_due -= 1;
        if self.s_tp_ring.len() == WINDOW_BLOCKS {
            self.s_tp_ring.pop_front();
        }
        self.s_tp_ring.push_back(self.s_tp.take_block_peak());
        let tp = self.s_tp_ring.iter().copied().fold(0.0f64, f64::max);
        let tp_db = if tp > 0.0 { lin_to_db(tp) } else { f64::NAN };
        let (m, s) = (self.s_loud.momentary(), self.s_loud.short_term());
        let measured = self.next_src.is_some();
        if measured && (m.is_finite() || s.is_finite()) {
            self.pts.push(LoudnessPoint { time_s: self.clock_s() as f32, lufs_s: s as f32, lufs_m: m as f32 });
        }
        let mut flags = 0u8;
        if measured {
            flags |= FLAG_S;
        }
        if self.direct {
            flags |= FLAG_DIRECT;
        }
        if self.missed {
            flags |= FLAG_MISSED;
        }
        let mut w = Vec::with_capacity(64 + self.pts.len() * 12 + (self.s_spec.cols.len() + self.o_spec.cols.len()) * 700);
        w.extend_from_slice(b"STRM");
        w.push(2);
        w.push(flags);
        w.extend_from_slice(&(self.l as u16).to_le_bytes());
        w.extend_from_slice(&self.src_rate.to_le_bytes());
        w.extend_from_slice(&self.out_rate.to_le_bytes());
        w.extend_from_slice(&self.clock_s().to_le_bytes());
        for v in [m, s, tp_db, self.s_rms.db(), self.o_rms.db()] {
            w.extend_from_slice(&(v as f32).to_le_bytes());
        }
        w.extend_from_slice(&(self.pts.len() as u16).to_le_bytes());
        for p in self.pts.drain(..) {
            w.extend_from_slice(&p.time_s.to_le_bytes());
            w.extend_from_slice(&p.lufs_s.to_le_bytes());
            w.extend_from_slice(&p.lufs_m.to_le_bytes());
        }
        self.s_spec.encode_take(&mut w);
        self.o_spec.encode_take(&mut w);
        self.s_env.encode_take(&mut w);
        self.o_env.encode_take(&mut w);
        w
    }
}

/// The `STRM` tail read back (tests).
#[cfg(test)]
#[derive(Debug)]
pub struct Strm {
    pub flags: u8,
    pub l: u16,
    pub src_rate: u32,
    pub out_rate: u32,
    pub clock_s: f64,
    /// S LUFS-M, S LUFS-S, S TP 3 s, S RMS 3 s, O RMS 3 s.
    pub values: [f32; 5],
    pub pts: Vec<(f32, f32, f32)>,
    /// (bands, hop, first, columns) of S, then of O.
    pub strips: [(u16, u32, u32, Vec<Vec<u8>>); 2],
    /// The waveform's blocks of S, then of O.
    pub env: [Vec<(u32, [i16; 6])>; 2],
}

#[cfg(test)]
pub fn decode(b: &[u8]) -> Strm {
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let f32_at = |o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    assert_eq!(&b[..4], b"STRM");
    assert_eq!(b[4], 2);
    let mut values = [0f32; 5];
    for (i, v) in values.iter_mut().enumerate() {
        *v = f32_at(24 + 4 * i);
    }
    let n = u16_at(44) as usize;
    let pts = (0..n).map(|i| (f32_at(46 + 12 * i), f32_at(50 + 12 * i), f32_at(54 + 12 * i))).collect();
    let mut o = 46 + 12 * n;
    let mut strip = || {
        let (bands, hop, first, k) = (u16_at(o), u32_at(o + 2), u32_at(o + 6), u16_at(o + 10) as usize);
        o += 12;
        let cols = (0..k).map(|i| b[o + i * bands as usize..o + (i + 1) * bands as usize].to_vec()).collect();
        o += k * bands as usize;
        (bands, hop, first, cols)
    };
    let s = strip();
    let os = strip();
    let mut env = || {
        let k = u16_at(o) as usize;
        o += 2;
        let blocks = (0..k).map(|i| {
            let p = o + i * 16;
            let mut v = [0i16; 6];
            for (j, x) in v.iter_mut().enumerate() {
                *x = i16::from_le_bytes([b[p + 4 + 2 * j], b[p + 5 + 2 * j]]);
            }
            (u32_at(p), v)
        }).collect();
        o += k * 16;
        blocks
    };
    let se = env();
    let oe = env();
    Strm {
        flags: b[5],
        l: u16_at(6),
        src_rate: u32_at(8),
        out_rate: u32_at(12),
        clock_s: f64::from_le_bytes(b[16..24].try_into().unwrap()),
        values,
        pts,
        strips: [s, os],
        env: [se, oe],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::output::OutputShared;
    use crate::player::radio::chain::RADIO_TRACK_ID;
    use crate::player::render::{Mark, Shared as RenderShared};
    use crate::player::settings::PlayerSettings;
    use crate::player::timeline::Timeline;
    use super::super::live::{drain_test_sink, Session};

    fn stream_desc(radio: &Arc<RadioShared>, l: usize, out_rate: u32, direct: bool) -> Arc<ChainDesc> {
        Arc::new(ChainDesc {
            source: if direct { "direct" } else { "live" },
            quick: false,
            taps: None,
            stages: Vec::new(),
            gain_db: 0.0,
            tp_db: None,
            ceiling_db: None,
            gpu_on: false,
            downgrade: None,
            downgrade_gen: 0,
            hp_deferred: false,
            stream: true,
            file: None,
            settings: Arc::new(PlayerSettings::default()),
            out_rate,
            l,
            variant: Weak::new(),
            radio: Arc::downgrade(radio),
        })
    }

    /// A stream session: its shadow `raw` (the stages hand on half of it).
    fn session_with(rate: u32, raw: &[f64]) -> Arc<RadioShared> {
        let shared = Arc::new(RadioShared::new("test://stream", &PlayerSettings::default()));
        let live = Arc::new(LiveSource::new(rate, 1 << 30, 0.5, 600.0));
        live.push_raw(raw, raw);
        let staged: Vec<f64> = raw.iter().map(|v| v * 0.5).collect();
        live.push(&staged, &staged);
        assert!(shared.live.set(live).is_ok());
        shared
    }

    /// A tone with a slow swell, loud enough for every gate.
    fn music(rate: u32, secs: f64) -> Vec<f64> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                (0.05 + 0.2 * (0.5 + 0.5 * (t * 0.7).sin())) * (2.0 * std::f64::consts::PI * 997.0 * t).sin()
            })
            .collect()
    }

    /// Play `out` (the chain's output, from index 0) through a live session
    /// in 20 ms reads (its wakeups); the STRM tails it published.
    fn play(desc: Arc<ChainDesc>, out_rate: u32, out: &[f64]) -> Vec<(super::super::live::LiveFrame, Strm)> {
        let tl = Arc::new(Timeline::new(out_rate, out.len() as f64 / out_rate as f64 + 1.0));
        let rs = RenderShared::new();
        rs.marks.lock().unwrap().push_back(Mark {
            frame: 0,
            track_id: RADIO_TRACK_ID,
            index: 0,
            rate: out_rate,
            l: desc.l,
            desc: desc.clone(),
        });
        tl.append(out, out);
        let mut s = Session::new(tl.clone(), rs, OutputShared::new(), out_rate, None, RADIO_TRACK_ID, 0, false, false, false);
        let step = out_rate as usize / 50;
        let mut done = 0;
        let mut buf = vec![0.0; step * 2];
        drain_test_sink();
        while done < out.len() {
            let k = step.min(out.len() - done);
            tl.read_into(&mut buf, k);
            done += k;
            s.allow_frames_for_test(k as u64 + 1);
            s.tick();
        }
        drain_test_sink()
            .into_iter()
            .filter(|f| !f.strm.is_empty())
            .map(|f| {
                let t = decode(&f.strm);
                (f, t)
            })
            .collect()
    }

    /// S is the shadow measured on the frames O played: its momentary and
    /// short-term loudness at every frame are an offline meter's on the
    /// shadow up to the stream time the frame names (O's points carry the
    /// same time), and S's and O's columns are the same moments and bands.
    #[test]
    fn a_streams_s_is_its_shadow_on_the_frames_the_output_played() {
        let (rate, l) = (44_100u32, 4usize);
        let out_rate = rate * l as u32;
        let raw = music(rate, 6.0);
        let shared = session_with(rate, &raw);
        // The output: the shadow held L times (the tone and its level the same).
        let out: Vec<f64> = raw.iter().flat_map(|&v| std::iter::repeat(v).take(l)).collect();
        let frames = play(stream_desc(&shared, l, out_rate, false), out_rate, &out);
        assert!(frames.len() > 40, "{} frames", frames.len());
        let mut checked = 0;
        for (f, t) in &frames {
            assert_eq!((t.l, t.src_rate, t.out_rate), (l as u16, rate, out_rate));
            assert_eq!(t.flags & (FLAG_S | FLAG_DIRECT | FLAG_MISSED), FLAG_S);
            let upto = (t.clock_s * rate as f64).round() as usize;
            let mut off = LoudnessMeter::new(rate as f64);
            off.push(&raw[..upto], &raw[..upto]);
            for (got, want) in [(t.values[0], off.momentary()), (t.values[1], off.short_term())] {
                if want.is_finite() {
                    assert!((got as f64 - want).abs() < 1e-3, "at {:.2} s: {got} vs {want}", t.clock_s);
                    checked += 1;
                } else {
                    assert!(got.is_nan(), "at {:.2} s: {got}", t.clock_s);
                }
            }
            // O's points and S's carry the stream time.
            for p in &f.loud_pts {
                assert!((p.time_s as f64 - t.clock_s).abs() < 1e-3, "{} vs {}", p.time_s, t.clock_s);
            }
            for p in &t.pts {
                assert!((p.0 as f64 - t.clock_s).abs() < 1e-3);
            }
            // The base frame carries no O columns on a stream: they come here.
            assert!(f.spec_cols.is_empty());
        }
        assert!(checked > 60, "{checked}");
        // Column k of S and of O: the same moment, the tone in the same band
        // at the same level.
        let (mut s_cols, mut o_cols) = (Vec::new(), Vec::new());
        for (_, t) in &frames {
            let [(sb, sh, s0, sc), (ob, oh, o0, oc)] = &t.strips;
            assert_eq!(*sb as usize, SPEC_LOG_BINS);
            assert_eq!(*ob as usize, spec_bands_on(rate, out_rate));
            assert_eq!(*oh, *sh * l as u32);
            for (i, c) in sc.iter().enumerate() {
                s_cols.push((*s0 as usize + i, c.clone()));
            }
            for (i, c) in oc.iter().enumerate() {
                o_cols.push((*o0 as usize + i, c.clone()));
            }
        }
        assert!(s_cols.len() > 100, "{}", s_cols.len());
        let mut pairs = 0;
        for (k, sc) in &s_cols {
            let Some((_, oc)) = o_cols.iter().find(|(j, _)| j == k) else { continue };
            let peak = |c: &[u8]| (0..SPEC_LOG_BINS).max_by_key(|&j| c[j]).unwrap();
            let (ps, po) = (peak(sc), peak(oc));
            assert_eq!(ps, po, "column {k}");
            assert!((sc[ps] as i32 - oc[po] as i32).abs() <= 1, "column {k}: {} vs {}", sc[ps], oc[po]);
            pairs += 1;
        }
        assert!(pairs + 2 >= s_cols.len(), "{pairs} of {}", s_cols.len());
    }

    /// BIT-PERFECT: L = 1 and O is the shadow itself — the flag says so, and
    /// S's columns and O's are the same bytes.
    #[test]
    fn bit_perfect_on_a_stream_is_one_signal() {
        let rate = 48_000u32;
        let raw = music(rate, 3.0);
        let shared = session_with(rate, &raw);
        let frames = play(stream_desc(&shared, 1, rate, true), rate, &raw);
        assert!(!frames.is_empty());
        let mut cols = 0;
        for (_, t) in &frames {
            assert_eq!(t.flags & FLAG_DIRECT, FLAG_DIRECT);
            assert_eq!(t.l, 1);
            let [(_, _, s0, sc), (_, _, o0, oc)] = &t.strips;
            assert_eq!((s0, sc), (o0, oc));
            cols += sc.len();
            if t.values[4].is_finite() {
                assert!((t.values[3] - t.values[4]).abs() < 1e-4, "{:?}", t.values);
            }
        }
        assert!(cols > 40, "{cols}");
    }

    /// The waveform's blocks: the least, the greatest and the RMS of each
    /// channel over 256 frames numbered from the stream's start, whatever
    /// the chunks they came in; a jump ends a block early.
    #[test]
    fn the_waveform_is_the_blocks_of_the_frames_heard() {
        let sig = |i: u64| ((i as f64) * 0.013).sin() * 0.5;
        let start = 1000u64;
        let total = 900usize;
        let l: Vec<f64> = (0..total as u64).map(|i| sig(start + i)).collect();
        let r: Vec<f64> = l.iter().map(|x| -0.5 * x).collect();
        let mut e = Env::new(ENV_BLOCK);
        let mut at = 0usize;
        for n in [7usize, 300, 1, 500, 92] {
            e.push(start + at as u64, &l[at..at + n], &r[at..at + n]);
            at += n;
        }
        let q = |v: f64| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let stats = |x: &[f64]| {
            let (mn, mx) = x.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            (q(mn), q(mx), q((x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()))
        };
        // Frames 1000..1900: block 3 from 1000 (part), 4, 5, 6 whole; 7 still filling.
        assert_eq!(e.done.iter().map(|d| d.0).collect::<Vec<_>>(), vec![3, 4, 5, 6]);
        for &(k, v) in &e.done {
            let a = (k as usize * 256).max(start as usize) - start as usize;
            let b = (k as usize + 1) * 256 - start as usize;
            let (l0, l1, l2) = stats(&l[a..b]);
            let (r0, r1, r2) = stats(&r[a..b]);
            assert_eq!(v, [l0, l1, l2, r0, r1, r2], "block {k}");
        }
        // A jump: block 7 goes out with what it had, the new place starts its own.
        e.push(5000, &l[..10], &r[..10]);
        assert_eq!(e.done.last().unwrap().0, 7);
        let mut w = Vec::new();
        e.encode_take(&mut w);
        assert_eq!(w.len(), 2 + 5 * 16);
        assert!(e.done.is_empty());
        // The next take sends them once more, the same bytes, then what
        // finished since; the one after, only that.
        let blocks = |w: &[u8]| -> Vec<u32> {
            let n = u16::from_le_bytes([w[0], w[1]]) as usize;
            (0..n).map(|i| u32::from_le_bytes(w[2 + i * 16..6 + i * 16].try_into().unwrap())).collect()
        };
        assert_eq!(blocks(&w), vec![3, 4, 5, 6, 7]);
        e.push(5010, &l[10..300], &r[10..300]);
        let mut w2 = Vec::new();
        e.encode_take(&mut w2);
        assert_eq!(blocks(&w2), vec![3, 4, 5, 6, 7, 19]);
        assert_eq!(&w2[2..2 + 5 * 16], &w[2..]);
        let mut w3 = Vec::new();
        e.encode_take(&mut w3);
        assert_eq!(blocks(&w3), vec![19]);
    }

    /// BIT-PERFECT: the waveforms of S and O are the same blocks.
    #[test]
    fn bit_perfect_waveforms_are_one() {
        let rate = 48_000u32;
        let raw = music(rate, 3.0);
        let shared = session_with(rate, &raw);
        let frames = play(stream_desc(&shared, 1, rate, true), rate, &raw);
        let (mut s, mut o) = (Vec::new(), Vec::new());
        for (_, t) in &frames {
            s.extend(t.env[0].iter().copied());
            o.extend(t.env[1].iter().copied());
        }
        assert!(s.len() > 400, "{} blocks of S", s.len());
        let common = s.len().min(o.len());
        assert_eq!(s[..common], o[..common]);
    }

    /// Each column and each waveform block goes out in two frames running,
    /// the same bytes both times, once within a frame: a page that reads
    /// every frame or every other one (it misses one now and then) has all
    /// of them, none lost between its first and its last.
    #[test]
    fn a_frame_missed_loses_no_column_and_no_block() {
        use std::collections::BTreeMap;
        let (rate, l) = (44_100u32, 2usize);
        let out_rate = rate * l as u32;
        let raw = music(rate, 4.0);
        let shared = session_with(rate, &raw);
        let out: Vec<f64> = raw.iter().flat_map(|&v| std::iter::repeat(v).take(l)).collect();
        let frames = play(stream_desc(&shared, l, out_rate, false), out_rate, &out);
        assert!(frames.len() > 30, "{} frames", frames.len());
        for which in 0..2 {
            // How often each went out: twice, never more.
            let mut times: BTreeMap<u32, usize> = BTreeMap::new();
            for (_, t) in &frames {
                let (_, _, first, cs) = &t.strips[which];
                for i in 0..cs.len() as u32 {
                    *times.entry(first + i).or_default() += 1;
                }
            }
            let twice = times.values().filter(|&&n| n == 2).count();
            assert!(times.values().all(|&n| n <= 2), "{which}: {times:?}");
            assert!(twice + 3 >= times.len(), "{which}: {twice} of {} columns sent twice", times.len());
            for step in [1usize, 2] {
                for phase in 0..step {
                    let mut cols: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
                    let mut blocks: BTreeMap<u32, [i16; 6]> = BTreeMap::new();
                    for (_, t) in frames.iter().skip(phase).step_by(step) {
                        let (_, _, first, cs) = &t.strips[which];
                        for (i, c) in cs.iter().enumerate() {
                            let k = first + i as u32;
                            if let Some(had) = cols.insert(k, c.clone()) {
                                assert_eq!(&had, c, "column {k}: the same bytes again");
                            }
                        }
                        let ks: Vec<u32> = t.env[which].iter().map(|b| b.0).collect();
                        assert!(ks.windows(2).all(|p| p[0] < p[1]), "blocks in a frame, once each: {ks:?}");
                        for &(k, v) in &t.env[which] {
                            if let Some(had) = blocks.insert(k, v) {
                                assert_eq!(had, v, "block {k}: the same values again");
                            }
                        }
                    }
                    let what = format!("signal {which}, every {step} from {phase}");
                    let ck: Vec<u32> = cols.keys().copied().collect();
                    assert!(ck.len() > 60, "{what}: {} columns", ck.len());
                    assert!(ck.windows(2).all(|p| p[1] == p[0] + 1), "{what}: columns {ck:?}");
                    let bk: Vec<u32> = blocks.keys().copied().collect();
                    assert!(bk.len() > 400, "{what}: {} blocks", bk.len());
                    assert!(bk.windows(2).all(|p| p[1] == p[0] + 1), "{what}: a block lost");
                }
            }
        }
    }

    /// A file's chain has no tap: its frames carry no STRM tail.
    #[test]
    fn a_track_has_no_stream_tap() {
        let shared = session_with(44_100, &music(44_100, 0.5));
        let mut slot = None;
        let desc = stream_desc(&shared, 8, 352_800, false);
        ensure(&mut slot, &desc, 352_800);
        assert!(slot.is_some());
        let mut file = (*desc).clone();
        file.stream = false;
        ensure(&mut slot, &file, 352_800);
        assert!(slot.is_none());
        // The stream's rate times L must be the output's.
        ensure(&mut slot, &desc, 384_000);
        assert!(slot.is_none());
    }

    /// Past the shadow's 40 s behind the reader: S begins where the shadow
    /// begins, and says it missed the frames before.
    #[test]
    fn s_says_when_the_shadow_no_longer_held_its_frames() {
        let rate = 44_100u32;
        let raw = music(rate, 70.0);
        let shared = Arc::new(RadioShared::new("test://stream", &PlayerSettings::default()));
        // The chain keeps 0.5 s behind the reader, the shadow its 40 s (cut in pieces of 2^20 frames); the reader at 68 s.
        let live = Arc::new(LiveSource::new(rate, rate as usize / 2, 0.5, 600.0));
        live.push_raw(&raw, &raw);
        live.push(&raw, &raw);
        live.ensure(68 * rate as i64);
        assert!(live.raw_start() > 0);
        assert!(shared.live.set(live).is_ok());
        let mut slot = None;
        ensure(&mut slot, &stream_desc(&shared, 1, rate, true), rate);
        let tap = slot.as_mut().unwrap();
        tap.feed(0, &raw[..4410], &raw[..4410]);
        let t = decode(&tap.publish());
        assert_eq!(t.flags & FLAG_MISSED, FLAG_MISSED);
    }

    #[test]
    fn rms_of_the_last_three_seconds() {
        let mut r = Rms3::new(1000);
        assert!(r.db().is_nan());
        // A full-scale square wave: 0 dBFS; 5 s of it, then 3 s at half.
        let a = vec![1.0; 5000];
        r.push(&a, &a);
        assert!(r.db().abs() < 1e-9, "{}", r.db());
        let h = vec![0.5; 3000];
        r.push(&h, &h);
        assert!((r.db() - 20.0 * 0.5f64.log10()).abs() < 1e-9, "{}", r.db());
    }
}
