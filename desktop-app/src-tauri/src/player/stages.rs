//! Output-rate stages that follow the polyphase FIR.
//!
//! In the converter these run once over the finished render. Here they run
//! on a stream that can start anywhere, so each stage says how much input
//! *before* its first output sample it needs (`history`) and pulls whatever
//! look-ahead it needs from the stage before it. The chain builder starts the
//! convolver early by the sum of the histories and the stages discard what
//! precedes the start.
//!
//! * [`XtcStage`] — the converter's `apply_xtc`, block for block: the same
//!   8192-sample blocks on the same grid (anchored at the first output
//!   sample of the file), the same FFT size, the same order of accumulation
//!   and the same bulk-delay trim. Numbers equal the converter's.
//! * [`LimiterStage`] — `isp::limit_output` on overlapping windows. The gain
//!   dip around each over is local (≤ a few ms), so windows with a margin on
//!   both sides reproduce it; the converter's whole-file give-up rule
//!   (> 5 % of the file under gain reduction) becomes per-window.
//! * [`FirStage`] — a linear-phase FIR on a stream (the subsonic guard): the
//!   converter's `apply_subsonic_guard`, partition for partition.
//! * [`GainStage`] — the true-peak gain.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::convolver::PolyStream;
use crate::audio::converter::apodize::{PartsWork, SubsonicParts};

type C = Complex<f64>;

/// Something that produces output-rate stereo samples in order.
pub trait Stage: Send {
    /// Fill both buffers with the next samples (silence past the end of the
    /// track). Returns how many were inside the track.
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize;
    /// Absolute output index of the next sample `read` returns.
    fn position(&self) -> u64;
    /// Output length of the track at this rate.
    fn total(&self) -> u64;
    /// Returns true when the underlying GPU device reported an error.
    /// CPU stages always return false; `GpuPolyStream` overrides this.
    fn is_gpu_failed(&self) -> bool {
        false
    }
}

impl Stage for PolyStream {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        PolyStream::read(self, out_l, out_r)
    }
    fn position(&self) -> u64 {
        PolyStream::position(self)
    }
    fn total(&self) -> u64 {
        PolyStream::total(self)
    }
}

/// Read and throw away samples until `stage` is at `target`.
pub fn skip_to(stage: &mut dyn Stage, target: u64) {
    let mut bl = vec![0.0; 16384];
    let mut br = vec![0.0; 16384];
    while stage.position() < target {
        let n = ((target - stage.position()) as usize).min(bl.len());
        stage.read(&mut bl[..n], &mut br[..n]);
    }
}

// ───────────────────────────── XTC ─────────────────────────────

const XTC_BLOCK: usize = 8192;

pub struct XtcStage {
    up: Box<dyn Stage>,
    m: usize,
    bulk: usize,
    fft_n: usize,
    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,
    hd: Vec<C>,
    hc: Vec<C>,
    norm: f64,
    total: u64,
    /// Next block to transform (its input starts at `next_block · 8192`).
    next_block: u64,
    /// Accumulator over pre-trim indices `acc_base ..`.
    acc_l: Vec<f64>,
    acc_r: Vec<f64>,
    acc_base: u64,
    /// Next output index to emit (output index i = pre-trim index i + bulk).
    pos: u64,
    xl: Vec<C>,
    xr: Vec<C>,
    yl: Vec<C>,
    yr: Vec<C>,
    inl: Vec<f64>,
    inr: Vec<f64>,
}

impl XtcStage {
    /// Input history the stage needs before output index `start`.
    pub fn history(m: usize, start: u64) -> u64 {
        start - Self::first_block(m, start) * XTC_BLOCK as u64
    }

    fn first_block(m: usize, start: u64) -> u64 {
        let bulk = (m - 1) / 2;
        let a = start as i64 + bulk as i64 - (XTC_BLOCK as i64 + m as i64 - 2);
        if a <= 0 {
            0
        } else {
            a as u64 / XTC_BLOCK as u64
        }
    }

    /// `up` must be positioned at `first_block · 8192`, i.e. at
    /// `start − history(m, start)`.
    pub fn new(up: Box<dyn Stage>, h_direct: &[f64], h_cross: &[f64], start: u64) -> XtcStage {
        let m = h_direct.len();
        assert_eq!(m, h_cross.len());
        let fft_n = (XTC_BLOCK + m - 1).next_power_of_two();
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(fft_n);
        let ifft = planner.plan_fft_inverse(fft_n);
        let spec = |h: &[f64]| {
            let mut v = vec![C::new(0.0, 0.0); fft_n];
            for (i, &x) in h.iter().enumerate() {
                v[i] = C::new(x, 0.0);
            }
            fft.process(&mut v);
            v
        };
        let hd = spec(h_direct);
        let hc = spec(h_cross);
        let first = Self::first_block(m, start);
        debug_assert_eq!(up.position(), first * XTC_BLOCK as u64);
        let total = up.total();
        XtcStage {
            up,
            m,
            bulk: (m - 1) / 2,
            fft_n,
            fft,
            ifft,
            hd,
            hc,
            norm: 1.0 / fft_n as f64,
            total,
            next_block: first,
            acc_l: Vec::new(),
            acc_r: Vec::new(),
            acc_base: first * XTC_BLOCK as u64,
            pos: start,
            xl: vec![C::new(0.0, 0.0); fft_n],
            xr: vec![C::new(0.0, 0.0); fft_n],
            yl: vec![C::new(0.0, 0.0); fft_n],
            yr: vec![C::new(0.0, 0.0); fft_n],
            inl: vec![0.0; XTC_BLOCK],
            inr: vec![0.0; XTC_BLOCK],
        }
    }

    /// Pre-trim indices below this are final (no later block reaches them).
    fn final_upto(&self) -> u64 {
        self.next_block * XTC_BLOCK as u64
    }

    fn do_block(&mut self) {
        let start = self.next_block * XTC_BLOCK as u64;
        // The converter's last block is short: input stops at the end of the
        // track. Beyond it the upstream returns silence, which is the same.
        let blen = if start >= self.total {
            0
        } else {
            ((self.total - start) as usize).min(XTC_BLOCK)
        };
        self.up.read(&mut self.inl[..XTC_BLOCK], &mut self.inr[..XTC_BLOCK]);
        for c in self.xl.iter_mut() {
            *c = C::new(0.0, 0.0);
        }
        for c in self.xr.iter_mut() {
            *c = C::new(0.0, 0.0);
        }
        for i in 0..blen {
            self.xl[i] = C::new(self.inl[i], 0.0);
            self.xr[i] = C::new(self.inr[i], 0.0);
        }
        {
            let (fft, xl, xr) = (&self.fft, &mut self.xl, &mut self.xr);
            rayon::join(|| fft.process(xl), || fft.process(xr));
        }
        for i in 0..self.fft_n {
            self.yl[i] = self.xl[i] * self.hd[i] + self.xr[i] * self.hc[i];
            self.yr[i] = self.xr[i] * self.hd[i] + self.xl[i] * self.hc[i];
        }
        {
            let (ifft, yl, yr) = (&self.ifft, &mut self.yl, &mut self.yr);
            rayon::join(|| ifft.process(yl), || ifft.process(yr));
        }
        // Accumulate into pre-trim indices start .. start+blen+m−1, in block order.
        // A live stream has no end (`total` is u64::MAX): its convolution has
        // none either — a plain sum would wrap and drop all but the first taps.
        let seg = blen + self.m - 1;
        let conv_len = self.total.saturating_add(self.m as u64 - 1);
        let need = (start + seg as u64 - self.acc_base) as usize;
        if self.acc_l.len() < need {
            self.acc_l.resize(need, 0.0);
            self.acc_r.resize(need, 0.0);
        }
        if blen > 0 {
            for i in 0..seg {
                let idx = start + i as u64;
                if idx < conv_len {
                    let k = (idx - self.acc_base) as usize;
                    self.acc_l[k] += self.yl[i].re * self.norm;
                    self.acc_r[k] += self.yr[i].re * self.norm;
                }
            }
        }
        self.next_block += 1;
    }
}

impl Stage for XtcStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if self.pos >= self.total {
                for i in done..n {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                break;
            }
            // Pre-trim index of the next output sample.
            let a = self.pos + self.bulk as u64;
            if a >= self.final_upto() {
                self.do_block();
                continue;
            }
            let ready = (self.final_upto() - a) as usize;
            let take = ready.min(n - done).min((self.total - self.pos) as usize);
            for i in 0..take {
                let k = (a + i as u64 - self.acc_base) as usize;
                out_l[done + i] = self.acc_l.get(k).copied().unwrap_or(0.0);
                out_r[done + i] = self.acc_r.get(k).copied().unwrap_or(0.0);
            }
            done += take;
            inside += take;
            self.pos += take as u64;
            // Drop accumulator entries nothing will read again.
            let keep_from = self.pos + self.bulk as u64;
            if keep_from > self.acc_base + 65536 {
                let drop = (keep_from - self.acc_base) as usize;
                let drop = drop.min(self.acc_l.len());
                self.acc_l.drain(..drop);
                self.acc_r.drain(..drop);
                self.acc_base += drop as u64;
            }
        }
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.total
    }
}

// ─────────────────────────── ISP output limiter ───────────────────────────

/// Window of output the limiter decides on at once, and the margin on each
/// side it sees but does not emit. The margin covers attack + cluster merge
/// gap + release (1.5 + 2 + 1.5 ms) with room to spare.
const LIM_WINDOW_MS: f64 = 250.0;
const LIM_MARGIN_MS: f64 = 12.0;

pub struct LimiterStage {
    up: Box<dyn Stage>,
    target_lin: f64,
    rate: u32,
    win: usize,
    margin: usize,
    /// Upstream samples from `buf_base` on.
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    buf_base: u64,
    /// Start of the next window to build.
    next_win: u64,
    total: u64,
    /// Limited output of the current window, which starts at `out_start`.
    out_l: Vec<f64>,
    out_r: Vec<f64>,
    out_start: u64,
    out_pos: usize,
    pub windows_limited: u64,
    pub windows_declined: u64,
    /// What the limiter did, for a caller that asks (`with_tally`).
    tally: Option<Arc<Mutex<LimiterTally>>>,
}

/// How much a limiter worked: over the samples it emitted, how many it
/// brought down and by how much (the gain is the output's peak over the
/// input's, sample by sample).
#[derive(Clone, Debug, Default)]
pub struct LimiterTally {
    pub samples: u64,
    /// Samples brought down.
    pub reduced: u64,
    /// Sum of their reductions, dB (≤ 0): the mean is this over `reduced`.
    pub reduced_db_sum: f64,
    /// The deepest reduction, dB (≤ 0).
    pub max_db: f64,
    pub windows_limited: u64,
}

impl LimiterStage {
    pub fn history(rate: u32) -> u64 {
        (rate as f64 * LIM_MARGIN_MS / 1000.0).ceil() as u64
    }

    /// `up` must be positioned at `start − history(rate)` (or 0).
    pub fn new(up: Box<dyn Stage>, target_lin: f64, rate: u32, start: u64) -> LimiterStage {
        let margin = Self::history(rate) as usize;
        let win = ((rate as f64 * LIM_WINDOW_MS / 1000.0) as usize).max(1024);
        let total = up.total();
        let base = up.position();
        LimiterStage {
            up,
            target_lin,
            rate,
            win,
            margin,
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            buf_base: base,
            next_win: start,
            total,
            out_l: Vec::new(),
            out_r: Vec::new(),
            out_start: start,
            out_pos: 0,
            windows_limited: 0,
            windows_declined: 0,
            tally: None,
        }
    }

    /// Count what this limiter does into `t` (the radio experiment's meter).
    pub fn with_tally(mut self, t: Arc<Mutex<LimiterTally>>) -> LimiterStage {
        self.tally = Some(t);
        self
    }

    fn fill_to(&mut self, upto: u64) {
        let have = self.buf_base + self.buf_l.len() as u64;
        if upto > have {
            let n = (upto - have) as usize;
            let old = self.buf_l.len();
            self.buf_l.resize(old + n, 0.0);
            self.buf_r.resize(old + n, 0.0);
            let (l, r) = (&mut self.buf_l[old..], &mut self.buf_r[old..]);
            self.up.read(l, r);
        }
    }

    fn next_window(&mut self) {
        let a = self.next_win;
        let b = (a + self.win as u64).min(self.total);
        let lo = a.saturating_sub(self.margin as u64).max(self.buf_base);
        let hi = b + self.margin as u64;
        self.fill_to(hi);
        let s = (lo - self.buf_base) as usize;
        let e = (hi - self.buf_base) as usize;
        let mut wl = self.buf_l[s..e].to_vec();
        let mut wr = self.buf_r[s..e].to_vec();
        let before = self.tally.as_ref().map(|_| (wl.clone(), wr.clone()));
        // Always acting: the track-wide decision (local limiting or the
        // global gain in front of this stage) was made in build_chain.
        let limited = match crate::audio::converter::dsp::lab::isp::limit_output_always(&mut wl, &mut wr, self.target_lin, self.rate) {
            Some(rep) if rep.fell_back.is_some() => {
                self.windows_declined += 1;
                false
            }
            Some(_) => {
                self.windows_limited += 1;
                true
            }
            None => false,
        };
        let off = (a - lo) as usize;
        let len = (b - a) as usize;
        if let (Some(t), Some((bl, br))) = (&self.tally, before) {
            let mut t = t.lock().unwrap_or_else(|e| e.into_inner());
            t.samples += len as u64;
            t.windows_limited += limited as u64;
            if limited {
                for i in off..off + len {
                    let x = bl[i].abs().max(br[i].abs());
                    if x > 1e-9 {
                        let g = wl[i].abs().max(wr[i].abs()) / x;
                        if g < 0.999_999 {
                            let db = 20.0 * g.max(1e-12).log10();
                            t.reduced += 1;
                            t.reduced_db_sum += db;
                            t.max_db = t.max_db.min(db);
                        }
                    }
                }
            }
        }
        self.out_l.clear();
        self.out_r.clear();
        self.out_l.extend_from_slice(&wl[off..off + len]);
        self.out_r.extend_from_slice(&wr[off..off + len]);
        self.out_start = a;
        self.out_pos = 0;
        self.next_win = b;
        // Keep only the margin behind the next window.
        let keep_from = b.saturating_sub(self.margin as u64).max(self.buf_base);
        let drop = ((keep_from - self.buf_base) as usize).min(self.buf_l.len());
        self.buf_l.drain(..drop);
        self.buf_r.drain(..drop);
        self.buf_base += drop as u64;
    }
}

impl Stage for LimiterStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if self.out_pos >= self.out_l.len() {
                if self.next_win >= self.total {
                    for i in done..n {
                        out_l[i] = 0.0;
                        out_r[i] = 0.0;
                    }
                    break;
                }
                self.next_window();
                continue;
            }
            let take = (self.out_l.len() - self.out_pos).min(n - done);
            out_l[done..done + take].copy_from_slice(&self.out_l[self.out_pos..self.out_pos + take]);
            out_r[done..done + take].copy_from_slice(&self.out_r[self.out_pos..self.out_pos + take]);
            self.out_pos += take;
            done += take;
            inside += take;
        }
        inside
    }
    fn position(&self) -> u64 {
        self.out_start + self.out_pos as u64
    }
    fn total(&self) -> u64 {
        self.total
    }
}

// ─────────────────────── streaming linear-phase FIR ───────────────────────

/// A linear-phase FIR on a stream, aligned (its bulk delay removed): the
/// subsonic guard. The subsonic filter's partitions (`SubsonicParts`, about
/// 93 ms), a block at a time, so it reads a partition and half the filter
/// ahead; its blocks start at its first input sample. From a track's first
/// sample it is the converter's guard (`apply_subsonic_guard`) to the bit.
pub struct FirStage {
    up: Box<dyn Stage>,
    delay: u64,
    parts: SubsonicParts,
    /// The input block before the next (silence before the first), per
    /// channel.
    prev_l: Vec<f64>,
    prev_r: Vec<f64>,
    /// The spectra of the last blocks, oldest first, and the next one's.
    ring_l: VecDeque<Vec<C>>,
    ring_r: VecDeque<Vec<C>>,
    new_l: Vec<C>,
    new_r: Vec<C>,
    work_l: PartsWork,
    work_r: PartsWork,
    /// Input index of the next block's first sample.
    in_pos: u64,
    /// Causal output of the last block, indices `y_base ..`.
    y_l: Vec<f64>,
    y_r: Vec<f64>,
    y_base: u64,
    pos: u64,
    total: u64,
    tmp_l: Vec<f64>,
    tmp_r: Vec<f64>,
}

impl FirStage {
    pub fn history(taps: usize) -> u64 {
        // Output m needs input m + D − (M − 1) .. m + D; D = (M − 1) / 2.
        ((taps - 1) - (taps - 1) / 2) as u64
    }

    /// `up` must be positioned at `start − history(taps)` (or 0); `rate` is
    /// its rate, which sets the partition.
    pub fn new(up: Box<dyn Stage>, coeffs: &[f64], rate: u32, start: u64) -> FirStage {
        let m = coeffs.len();
        let parts = SubsonicParts::new(coeffs, rate);
        let b = parts.block_len();
        let in_pos = up.position();
        let total = up.total();
        FirStage {
            up,
            delay: ((m - 1) / 2) as u64,
            prev_l: vec![0.0; b],
            prev_r: vec![0.0; b],
            ring_l: VecDeque::new(),
            ring_r: VecDeque::new(),
            new_l: vec![C::new(0.0, 0.0); b + 1],
            new_r: vec![C::new(0.0, 0.0); b + 1],
            work_l: parts.work(),
            work_r: parts.work(),
            parts,
            in_pos,
            y_l: vec![0.0; b],
            y_r: vec![0.0; b],
            y_base: in_pos,
            pos: start,
            total,
            tmp_l: vec![0.0; b],
            tmp_r: vec![0.0; b],
        }
    }

    fn do_block(&mut self) {
        let b = self.parts.block_len();
        self.up.read(&mut self.tmp_l, &mut self.tmp_r);
        let p = &self.parts;
        let (ring_l, ring_r) = (&self.ring_l, &self.ring_r);
        rayon::join(
            || {
                p.spectrum(&self.prev_l, &self.tmp_l, &mut self.work_l, &mut self.new_l);
                let xs = std::iter::once(&self.new_l[..]).chain(ring_l.iter().rev().map(|s| &s[..]));
                p.block(xs, &mut self.work_l, &mut self.y_l);
            },
            || {
                p.spectrum(&self.prev_r, &self.tmp_r, &mut self.work_r, &mut self.new_r);
                let xs = std::iter::once(&self.new_r[..]).chain(ring_r.iter().rev().map(|s| &s[..]));
                p.block(xs, &mut self.work_r, &mut self.y_r);
            },
        );
        p.keep(&mut self.ring_l, &mut self.new_l);
        p.keep(&mut self.ring_r, &mut self.new_r);
        std::mem::swap(&mut self.prev_l, &mut self.tmp_l);
        std::mem::swap(&mut self.prev_r, &mut self.tmp_r);
        // Causal output indices in_pos .. in_pos + b (valid once the history
        // before `in_pos` was real input; the first block's early samples
        // are discarded by the start offset).
        self.y_base = self.in_pos;
        self.in_pos += b as u64;
    }
}

impl Stage for FirStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if self.pos >= self.total {
                for i in done..n {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                break;
            }
            let c = self.pos + self.delay; // causal index
            if c < self.y_base || c >= self.y_base + self.y_l.len() as u64 {
                self.do_block();
                continue;
            }
            let k = (c - self.y_base) as usize;
            let take = (self.y_l.len() - k).min(n - done).min((self.total - self.pos) as usize);
            out_l[done..done + take].copy_from_slice(&self.y_l[k..k + take]);
            out_r[done..done + take].copy_from_slice(&self.y_r[k..k + take]);
            self.pos += take as u64;
            done += take;
            inside += take;
        }
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.total
    }
}

// ───────────────────────────── gain ─────────────────────────────

pub struct GainStage {
    up: Box<dyn Stage>,
    pub gain: f64,
    /// A glide into `gain`: from this gain, over `glide_len` frames, of
    /// which `glide_left` are still to go (smoothstep). None: `gain` flat.
    glide_from: Option<f64>,
    glide_len: u64,
    glide_left: u64,
}

impl GainStage {
    pub fn new(up: Box<dyn Stage>, gain: f64) -> GainStage {
        GainStage { up, gain, glide_from: None, glide_len: 0, glide_left: 0 }
    }

    /// `gain` reached from `from` over `frames`: the quick variant's level
    /// handing over to the full one's without a step.
    pub fn gliding(up: Box<dyn Stage>, gain: f64, from: f64, frames: u64) -> GainStage {
        let frames = frames.max(1);
        GainStage { up, gain, glide_from: Some(from), glide_len: frames, glide_left: frames }
    }
}

impl Stage for GainStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = self.up.read(out_l, out_r);
        if let (Some(from), true) = (self.glide_from, self.glide_left > 0) {
            let len = self.glide_len as f64;
            for i in 0..n {
                let u = if self.glide_left > 0 { 1.0 - self.glide_left as f64 / len } else { 1.0 };
                let w = u * u * (3.0 - 2.0 * u);
                let g = from + (self.gain - from) * w;
                out_l[i] *= g;
                out_r[i] *= g;
                self.glide_left = self.glide_left.saturating_sub(1);
            }
            if self.glide_left == 0 {
                self.glide_from = None;
            }
            return n;
        }
        if self.gain != 1.0 {
            for v in out_l.iter_mut() {
                *v *= self.gain;
            }
            for v in out_r.iter_mut() {
                *v *= self.gain;
            }
        }
        n
    }
    fn position(&self) -> u64 {
        self.up.position()
    }
    fn total(&self) -> u64 {
        self.up.total()
    }
}

// ───────────────────────────── Read ahead ─────────────────────────────

/// A chain's first samples, read ahead of its start: the prewarm reads them
/// right after the build, off the air. The first block of a convolver costs
/// about ten of the ones after it (250 ms for a 30M bank, 500 ms for a
/// Hybrid-Phase pair), and a new stream waited for it in silence before the
/// device was opened — at a change of rate, at ⏭. They are handed out
/// first, then the chain reads on.
pub struct Ahead {
    up: Box<dyn Stage>,
    l: Vec<f64>,
    r: Vec<f64>,
    /// How many of them were inside the track.
    inside: usize,
    /// Absolute output index of `l[0]`.
    start: u64,
    /// The next of them to hand out.
    at: usize,
}

impl Ahead {
    /// `up` with its next `n` samples read now.
    pub fn read(mut up: Box<dyn Stage>, n: usize) -> Ahead {
        let start = up.position();
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        let inside = up.read(&mut l, &mut r);
        Ahead { up, l, r, inside, start, at: 0 }
    }
}

impl Stage for Ahead {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let left = self.l.len() - self.at;
        if left == 0 {
            return self.up.read(out_l, out_r);
        }
        let n = out_l.len();
        let take = left.min(n);
        out_l[..take].copy_from_slice(&self.l[self.at..self.at + take]);
        out_r[..take].copy_from_slice(&self.r[self.at..self.at + take]);
        let inside = self.inside.saturating_sub(self.at).min(take);
        self.at += take;
        if take == n {
            return inside;
        }
        inside + self.up.read(&mut out_l[take..], &mut out_r[take..])
    }
    fn position(&self) -> u64 {
        if self.at < self.l.len() {
            self.start + self.at.min(self.inside) as u64
        } else {
            self.up.position()
        }
    }
    fn total(&self) -> u64 {
        self.up.total()
    }
}

#[cfg(test)]
mod ahead_tests {
    use super::{skip_to, Ahead, Stage};

    /// A track of `total` samples: sample i is i (left) and −i (right).
    struct Ramp {
        pos: u64,
        total: u64,
    }

    impl Stage for Ramp {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for (l, r) in out_l.iter_mut().zip(out_r.iter_mut()) {
                if self.pos < self.total {
                    *l = self.pos as f64;
                    *r = -(self.pos as f64);
                    self.pos += 1;
                    inside += 1;
                } else {
                    *l = 0.0;
                    *r = 0.0;
                }
            }
            inside
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.total
        }
    }

    /// Reads of `sizes` from a stage: what came out, how much was inside the
    /// track, and the position after each read.
    fn run(stage: &mut dyn Stage, sizes: &[usize]) -> (Vec<f64>, Vec<f64>, Vec<usize>, Vec<u64>) {
        let (mut ls, mut rs, mut ins, mut pos) = (vec![], vec![], vec![], vec![]);
        for &n in sizes {
            let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
            ins.push(stage.read(&mut l, &mut r));
            ls.extend(l);
            rs.extend(r);
            pos.push(stage.position());
        }
        (ls, rs, ins, pos)
    }

    /// A chain read ahead gives what it would have given, read for read:
    /// the same samples, the same count inside the track, the same position
    /// — reads smaller and larger than what was read ahead, across its end,
    /// and past the end of the track.
    #[test]
    fn a_stage_read_ahead_reads_as_it_would_have() {
        let sizes = [100, 3_000, 8_192, 1, 20_000, 9_000];
        for (start, total, ahead) in [(0u64, 30_000u64, 8_192usize), (500, 30_000, 8_192), (0, 5_000, 8_192), (0, 30_000, 0)] {
            let mut plain = Ramp { pos: start, total };
            let want = run(&mut plain, &sizes);
            let mut read_ahead = Ahead::read(Box::new(Ramp { pos: start, total }), ahead);
            assert_eq!(read_ahead.position(), start, "nothing handed out yet: at its start");
            let got = run(&mut read_ahead, &sizes);
            assert_eq!(got, want, "start {start}, total {total}, ahead {ahead}");
        }
    }

    /// A start skips to its point through the read-ahead samples as through
    /// the chain itself (the jump path's `skip_to`).
    #[test]
    fn a_skip_goes_through_the_samples_read_ahead() {
        let mut s = Ahead::read(Box::new(Ramp { pos: 0, total: 30_000 }), 8_192);
        skip_to(&mut s, 5_000);
        let (mut l, mut r) = (vec![0.0; 4], vec![0.0; 4]);
        s.read(&mut l, &mut r);
        assert_eq!(l, [5_000.0, 5_001.0, 5_002.0, 5_003.0]);
        skip_to(&mut s, 10_000);
        s.read(&mut l, &mut r);
        assert_eq!(l, [10_000.0, 10_001.0, 10_002.0, 10_003.0]);
    }
}
