//! The kit on a live stream (TASK-27, stage 2): DrumSep over the drums
//! source as the separation makes it, chunk by chunk ahead of what is heard,
//! and what is measured of each piece frame by frame — what a file's kit
//! (kit.rs) measures over the whole track, made as the stream comes.
//!
//! The drums come from `Sep` up to the end of its last segment, its last
//! quarter too, as that segment alone made it, read once: a chunk is whole
//! 1.5 to 3 s after its drums are in, and the drums made whole run ahead of
//! the place heard by as little as 0.2 s each cycle. The chunks are a file's
//! (`KitRun`): 2.96 s, half a chunk apart, faded into each other; a sample is
//! whole once both chunks over it have run, and the last half of the last
//! chunk is that chunk's own until the next one comes (as `Sep`'s last
//! quarter). Behind (a busy card), a chunk whose sound would be heard before
//! it could be ready is not run: the grid starts afresh ahead.
//!
//! The pieces' sound is held some seconds and measured in runs of frames,
//! each from the frames before it, as `feats` does: a piece's level, its
//! onset function (the log-spectral flux of its mono sound and its power,
//! two per frame), how alike its channels are, its spectrum's centre and its
//! left and right levels. A song's levels, hits and pieces are found from
//! these against what the song has played so far (`song`).
//!
//! f32: the network's boundary (§4.1), read by the analysis of the picture;
//! nothing goes back to what is heard.

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::super::analysis::HOP;
use super::super::kit::{self, CHUNK, NP};

/// The samples kept are dropped from the front in pieces of at least this many.
const TRIM: usize = 1 << 18;
/// Frames before a run's first that settle its values (the coherence's 60 ms).
pub const WARM: usize = 30;
/// A frame reads this many samples either side of its centre.
const HALF: usize = kit::LV_N / 2;

/// The drum network's chunks over the stream, and the pieces' sound.
pub struct DrumRun {
    /// The grid's origin: chunk i reads session samples from
    /// `s0 + i·step − pad`, `CHUNK` of them.
    s0: usize,
    step: usize,
    pad: usize,
    /// The drums: `in_l[0]` is session sample `in0`.
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    in0: usize,
    /// The next chunk to run.
    next: usize,
    /// The pieces' faded sums over the next chunk's samples.
    acc: Vec<f32>,
    wsum: Vec<f32>,
    fade: Vec<f32>,
    /// The pieces' sound, `out[piece·2 + channel][k]` = session sample
    /// `out0 + k`: whole below `fin`, the last chunk's own up to `prov`.
    out: Vec<Vec<f32>>,
    out0: usize,
    fin: usize,
    prov: usize,
    /// The runs of samples some chunk has, in order.
    runs: Vec<(usize, usize)>,
    pub done: u64,
    pub skipped: u64,
}

impl DrumRun {
    /// The grid from session sample `s0` (the drums before it read as silence).
    pub fn new(s0: usize) -> DrumRun {
        let step = CHUNK / kit::OVERLAP;
        let mut fade = vec![1f32; CHUNK];
        // np.linspace(0, 1, FADE) and back, as KitRun's
        for i in 0..kit::FADE {
            let v = (i as f64 / (kit::FADE - 1) as f64) as f32;
            fade[i] = v;
            fade[CHUNK - 1 - i] = v;
        }
        DrumRun {
            s0,
            step,
            pad: CHUNK - step,
            in_l: Vec::new(),
            in_r: Vec::new(),
            in0: s0,
            next: 0,
            acc: vec![0f32; NP * 2 * CHUNK],
            wsum: vec![0f32; CHUNK],
            fade,
            out: vec![Vec::new(); NP * 2],
            out0: s0,
            fin: s0,
            prov: s0,
            runs: Vec::new(),
            done: 0,
            skipped: 0,
        }
    }

    /// The drums are in up to here (exclusive).
    pub fn end(&self) -> usize {
        self.in0 + self.in_l.len()
    }

    /// The pieces' sound is whole below this.
    pub fn fin(&self) -> usize {
        self.fin
    }

    /// The pieces' sound (whole, or the last chunk's own) is there below this.
    pub fn prov(&self) -> usize {
        self.prov
    }

    /// Whether every sample of [a, b) has the pieces' sound.
    pub fn covered(&self, a: usize, b: usize) -> bool {
        self.runs.iter().any(|r| r.0 <= a && b <= r.1)
    }

    /// Where the run of samples the last chunk is in begins (the grid's
    /// origin before any chunk ran).
    pub fn run_start(&self) -> usize {
        self.runs.last().map_or(self.s0, |r| r.0)
    }

    /// The drums' next samples, from `end()` on.
    pub fn push(&mut self, l: &[f32], r: &[f32]) {
        let n = l.len().min(r.len());
        self.in_l.extend_from_slice(&l[..n]);
        self.in_r.extend_from_slice(&r[..n]);
    }

    /// The drums go on from session sample `at` (past `end()`: the samples
    /// between are silence — no segment had them).
    pub fn skip_to(&mut self, at: usize) {
        let gap = at.saturating_sub(self.end());
        self.in_l.resize(self.in_l.len() + gap, 0.0);
        self.in_r.resize(self.in_r.len() + gap, 0.0);
    }

    /// The first session sample chunk `i` reads.
    pub fn start(&self, i: usize) -> isize {
        (self.s0 + i * self.step) as isize - self.pad as isize
    }

    /// The next chunk to run with the drums in, the place heard at `heard`
    /// and a chunk taking about `budget` samples' time: its number and its
    /// drums (left, right). One whose sound would all be heard before it is
    /// ready is not run: the grid starts afresh ahead. None: none is due.
    pub fn pick(&mut self, heard: usize, budget: usize) -> Option<(usize, Vec<f32>, Vec<f32>)> {
        let soon = heard + budget;
        let need = (self.start(self.next) + CHUNK as isize) as usize;
        if self.end() < need {
            return None;
        }
        if need <= soon {
            self.restart(soon.max(self.prov));
            self.skipped += 1;
            let need = (self.start(0) + CHUNK as isize) as usize;
            if self.end() < need {
                return None;
            }
        }
        let a = self.start(self.next);
        Some((self.next, self.cut(&self.in_l, a), self.cut(&self.in_r, a)))
    }

    fn cut(&self, v: &[f32], a: isize) -> Vec<f32> {
        (0..CHUNK as isize)
            .map(|k| {
                let j = a + k - self.in0 as isize;
                if j >= 0 && (j as usize) < v.len() { v[j as usize] } else { 0.0 }
            })
            .collect()
    }

    /// A new grid from session sample `s0` (not before what is whole): the
    /// last chunk's own sound goes, the chunks run from there.
    pub fn restart(&mut self, s0: usize) {
        let s0 = s0.max(self.fin);
        self.cut_out(self.fin);
        self.prov = self.fin;
        self.s0 = s0;
        self.next = 0;
        self.acc.fill(0.0);
        self.wsum.fill(0.0);
    }

    fn cut_out(&mut self, at: usize) {
        let k = at.saturating_sub(self.out0);
        for v in self.out.iter_mut() {
            v.truncate(k);
        }
        for r in self.runs.iter_mut() {
            r.1 = r.1.min(at);
        }
        self.runs.retain(|r| r.1 > r.0);
    }

    /// Chunk `i`'s pieces (`[piece][channel][CHUNK]`) in; a chunk of a grid
    /// started afresh since is let go.
    pub fn add(&mut self, i: usize, y: &[f32]) -> Result<(), String> {
        if y.len() != NP * 2 * CHUNK {
            return Err(format!("the drum network gave {} values, not {}", y.len(), NP * 2 * CHUNK));
        }
        if i != self.next {
            return Ok(());
        }
        for s in 0..NP * 2 {
            let (a, b) = (&mut self.acc[s * CHUNK..(s + 1) * CHUNK], &y[s * CHUNK..(s + 1) * CHUNK]);
            for k in 0..CHUNK {
                a[k] += b[k] * self.fade[k];
            }
        }
        for k in 0..CHUNK {
            self.wsum[k] += self.fade[k];
        }
        self.next += 1;
        self.done += 1;
        let start = self.start(i);
        // the grid's own samples of it: from its origin
        let a = start.max(self.s0 as isize) as usize;
        let end = (start + CHUNK as isize) as usize;
        // the last chunk's own sound goes; the gap since (a grid started afresh) is silence
        self.cut_out(self.fin);
        let have = self.out0 + self.out[0].len();
        for v in self.out.iter_mut() {
            v.resize(v.len() + a.saturating_sub(have), 0.0);
        }
        let o = (a as isize - start) as usize;
        for s in 0..NP * 2 {
            let src = &self.acc[s * CHUNK..(s + 1) * CHUNK];
            let w = &self.wsum;
            self.out[s].extend((o..CHUNK).map(|k| src[k] / w[k].max(1e-6)));
        }
        match self.runs.last_mut() {
            Some(r) if r.1 >= a => r.1 = r.1.max(end),
            _ => self.runs.push((a, end)),
        }
        self.fin = self.fin.max((start + self.step as isize).max(0) as usize);
        self.prov = end;
        // no later chunk reaches its first `step` samples: the sums move on
        for s in 0..NP * 2 {
            let v = &mut self.acc[s * CHUNK..(s + 1) * CHUNK];
            v.copy_within(self.step.., 0);
            v[CHUNK - self.step..].fill(0.0);
        }
        self.wsum.copy_within(self.step.., 0);
        self.wsum[CHUNK - self.step..].fill(0.0);
        Ok(())
    }

    /// Piece·channel `pc` from session sample `from`: zero where no chunk has it.
    pub fn read(&self, pc: usize, from: isize, out: &mut [f32]) {
        let v = &self.out[pc];
        for (i, o) in out.iter_mut().enumerate() {
            let k = from + i as isize - self.out0 as isize;
            *o = if k >= 0 && (k as usize) < v.len() { v[k as usize] } else { 0.0 };
        }
    }

    /// Forget the samples before `keep_from` (the drums: never what the next
    /// chunk reads; the pieces: never past what is whole).
    pub fn trim(&mut self, keep_from: usize) {
        let need = self.start(self.next).max(0) as usize;
        let k_in = keep_from.min(need);
        if k_in > self.in0 + TRIM {
            let n = (k_in - self.in0).min(self.in_l.len());
            self.in_l.drain(..n);
            self.in_r.drain(..n);
            self.in0 += n;
        }
        let k_out = keep_from.min(self.fin);
        if k_out > self.out0 + TRIM {
            let n = (k_out - self.out0).min(self.out[0].len());
            for v in self.out.iter_mut() {
                v.drain(..n);
            }
            self.out0 += n;
            self.runs.retain(|r| r.1 > k_out);
        }
    }
}

/// What is measured of the pieces in one frame (session frame j: the 2048
/// samples around session sample 512·j), per piece in the network's order.
#[derive(Clone, Copy)]
pub struct DrumFrame {
    /// The pieces' sound covers the frame (else a gap: no chunk ran there).
    pub ok: bool,
    /// The level (dB): the mean power of the 2048 samples (`Piece::db`).
    pub db: [f32; NP],
    /// The onset function's two frames in it (hop 256): the log-spectral
    /// flux of the mono sound and its power (dB).
    pub flux: [[f32; 2]; NP],
    pub pow: [[f32; 2]; NP],
    /// How alike the channels are, 0…1 (`Field`).
    pub coh: [f32; NP],
    /// The energy of the field's bins and its ln f moment (the spectrum's
    /// centre over a song), and the |L|, |R| sums of the frame's own 512
    /// samples (a piece's place over a song).
    pub e: [f64; NP],
    pub ef: [f64; NP],
    pub al: [f64; NP],
    pub ar: [f64; NP],
}

impl Default for DrumFrame {
    fn default() -> DrumFrame {
        DrumFrame {
            ok: false,
            db: [-200.0; NP],
            flux: [[0.0; 2]; NP],
            pow: [[-200.0; 2]; NP],
            coh: [0.0; NP],
            e: [0.0; NP],
            ef: [0.0; NP],
            al: [0.0; NP],
            ar: [0.0; NP],
        }
    }
}

/// The frames held: session frame `base` first.
pub struct DrumFeats {
    base: usize,
    v: VecDeque<DrumFrame>,
    cap: usize,
    on_fft: Arc<dyn Fft<f32>>,
    /// np.hanning(1024): symmetric
    on_win: Vec<f32>,
    field_fft: Arc<dyn Fft<f32>>,
    /// periodic Hann 2048
    field_win: Vec<f32>,
    ln_f: Vec<f32>,
}

impl DrumFeats {
    /// Holding the last `secs` seconds of frames.
    pub fn new(secs: f64) -> DrumFeats {
        let mut p = FftPlanner::<f32>::new();
        let on_win = (0..kit::ON_N)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (kit::ON_N - 1) as f64).cos()) as f32)
            .collect();
        let field_win = (0..kit::LV_N)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / kit::LV_N as f64).cos()) as f32)
            .collect();
        let ln_f = (0..=kit::LV_N / 2).map(|k| ((k as f64 * kit::SR / kit::LV_N as f64).max(20.0) as f32).ln()).collect();
        DrumFeats {
            base: 0,
            v: VecDeque::new(),
            cap: (secs * super::super::analysis::FPS) as usize,
            on_fft: p.plan_fft_forward(kit::ON_N),
            on_win,
            field_fft: p.plan_fft_forward(kit::LV_N),
            field_win,
            ln_f,
        }
    }

    pub fn start(&self) -> usize {
        self.base
    }

    pub fn get(&self, j: usize) -> Option<&DrumFrame> {
        j.checked_sub(self.base).and_then(|i| self.v.get(i))
    }

    /// Forget the frames from `j` on (the stream changed there).
    pub fn truncate(&mut self, j: usize) {
        if j <= self.base {
            self.v.clear();
            self.base = j;
        } else {
            self.v.truncate(j - self.base);
        }
    }

    fn put(&mut self, j: usize, f: DrumFrame) {
        if self.v.is_empty() {
            self.base = j;
        }
        if j < self.base {
            return;
        }
        let i = j - self.base;
        if i < self.v.len() {
            self.v[i] = f;
        } else {
            while self.v.len() < i {
                self.v.push_back(DrumFrame::default());
            }
            self.v.push_back(f);
        }
        while self.v.len() > self.cap {
            self.v.pop_front();
            self.base += 1;
        }
    }

    /// Frames [a, b) from the pieces' sound `run` holds, each run starting
    /// `WARM` frames before `a` (the coherence's smoothing, the flux's frame
    /// before): nothing is carried from one run to the next, so a run can be
    /// made again (the last chunk's half, made whole by the next chunk).
    pub fn make(&mut self, run: &DrumRun, a: usize, b: usize) {
        use rayon::prelude::*;
        if b <= a {
            return;
        }
        let w0 = a.saturating_sub(WARM);
        // the samples read: the warm-up's first window … the last frame's
        let from = (w0 * HOP) as isize - HALF as isize;
        let len = (b - w0) * HOP + 2 * HALF;
        let pieces: Vec<Piece> = (0..NP)
            .into_par_iter()
            .map(|p| {
                let (mut l, mut r) = (vec![0f32; len], vec![0f32; len]);
                run.read(p * 2, from, &mut l);
                run.read(p * 2 + 1, from, &mut r);
                self.piece(&l, &r, from, w0, a, b)
            })
            .collect();
        for j in a..b {
            let c = j * HOP;
            let mut f = DrumFrame { ok: run.covered(c.saturating_sub(HALF), c + HALF), ..Default::default() };
            for (p, pc) in pieces.iter().enumerate() {
                let k = j - a;
                f.db[p] = pc.db[k];
                f.flux[p] = pc.flux[k];
                f.pow[p] = pc.pow[k];
                f.coh[p] = pc.coh[k];
                f.e[p] = pc.e[k];
                f.ef[p] = pc.ef[k];
                f.al[p] = pc.al[k];
                f.ar[p] = pc.ar[k];
            }
            self.put(j, f);
        }
    }

    /// One piece's frames [a, b), from its sound `l`, `r` whose sample 0 is
    /// session sample `from`, its warm-up from frame `w0`.
    fn piece(&self, l: &[f32], r: &[f32], from: isize, w0: usize, a: usize, b: usize) -> Piece {
        let n = b - a;
        let at = |s: isize| -> Option<usize> {
            let k = s - from;
            (k >= 0 && (k as usize) < l.len()).then_some(k as usize)
        };
        let mut pc = Piece {
            db: vec![0.0; n],
            flux: vec![[0.0; 2]; n],
            pow: vec![[0.0; 2]; n],
            coh: vec![0.0; n],
            e: vec![0.0; n],
            ef: vec![0.0; n],
            al: vec![0.0; n],
            ar: vec![0.0; n],
        };
        // the level: four hops of 512 around the frame (`PieceFeat::finish`), each summed in order
        let hop_pow = |h: isize| -> f64 {
            let mut s = 0f64;
            for t in h * kit::LV_HOP as isize..(h + 1) * kit::LV_HOP as isize {
                if let Some(k) = at(t) {
                    let (x, y) = (l[k] as f64, r[k] as f64);
                    s += (x * x + y * y) / 2.0;
                }
            }
            s
        };
        for j in a..b {
            let s: f64 = (j as isize - 2..=j as isize + 1).map(hop_pow).sum();
            pc.db[j - a] = (10.0 * (s / kit::LV_N as f64 + 1e-20).log10()) as f32;
            let (mut sl, mut sr) = (0f64, 0f64);
            for t in (j * kit::LV_HOP) as isize..((j + 1) * kit::LV_HOP) as isize {
                if let Some(k) = at(t) {
                    sl += l[k].abs() as f64;
                    sr += r[k].abs() as f64;
                }
            }
            pc.al[j - a] = sl;
            pc.ar[j - a] = sr;
        }
        // the onset function (`PieceFeat::frame`): two frames of hop 256 a frame, the one before the first for its flux
        let mut buf = vec![Complex32::new(0.0, 0.0); kit::ON_N];
        let mut scratch = vec![Complex32::new(0.0, 0.0); self.on_fft.get_inplace_scratch_len()];
        let mut prev = vec![0f32; kit::ON_BINS];
        let (m0, m1) = (2 * a - (a > 0) as usize, 2 * b);
        for m in m0..m1 {
            let s0 = (m * kit::ON_HOP) as isize - (kit::ON_N / 2) as isize;
            for (i, v) in buf.iter_mut().enumerate() {
                let x = at(s0 + i as isize).map_or(0.0, |k| (l[k] + r[k]) / 2.0);
                *v = Complex32::new(x * self.on_win[i], 0.0);
            }
            self.on_fft.process_with_scratch(&mut buf, &mut scratch);
            let (mut d, mut p) = (0f64, 0f64);
            for (k, pv) in prev.iter_mut().enumerate() {
                let mg = buf[k].norm();
                let lg = (100.0 * mg).ln_1p();
                if m > m0 {
                    d += (lg - *pv).max(0.0) as f64;
                }
                *pv = lg;
                p += (mg * mg) as f64;
            }
            if m >= 2 * a {
                // (the session's very first onset frame has no flux, as `PieceFeat`'s first)
                let (j, h) = (m / 2 - a, m % 2);
                pc.flux[j][h] = d as f32;
                pc.pow[j][h] = 10.0 * (p as f32 + 1e-20).log10();
            }
        }
        // the field (`Field::frame`): its smoothing from the warm-up's first frame
        let mut fb = vec![Complex32::new(0.0, 0.0); kit::LV_N];
        let mut fs = vec![Complex32::new(0.0, 0.0); self.field_fft.get_inplace_scratch_len()];
        let nb = kit::LV_N / 2 + 1;
        let (mut c, mut pl, mut pr) = (vec![Complex32::new(0.0, 0.0); nb], vec![0f32; nb], vec![0f32; nb]);
        let al = 1.0 - (-(1.0 / kit::LV_FPS) / 0.06).exp() as f32;
        let (half, mhalf_i) = (Complex32::new(0.5, 0.0), Complex32::new(0.0, -0.5));
        for j in w0..b {
            let s0 = (j * kit::LV_HOP) as isize - HALF as isize;
            for (i, v) in fb.iter_mut().enumerate() {
                *v = match at(s0 + i as isize) {
                    Some(k) => Complex32::new(l[k] * self.field_win[i], r[k] * self.field_win[i]),
                    None => Complex32::new(0.0, 0.0),
                };
            }
            self.field_fft.process_with_scratch(&mut fb, &mut fs);
            let (mut e_sum, mut ec_sum, mut ef_sum) = (0f64, 0f64, 0f64);
            for k in 1..kit::LV_N / 2 {
                let f = k as f64 * kit::SR / kit::LV_N as f64;
                if !(kit::FIELD_LO..=kit::FIELD_HI).contains(&f) {
                    continue;
                }
                let z = fb[k];
                let zc = fb[kit::LV_N - k].conj();
                let (bl, br) = ((z + zc) * half, (z - zc) * mhalf_i);
                let (el, er) = (bl.norm_sqr(), br.norm_sqr());
                let x = bl * br.conj();
                if j == w0 {
                    (c[k], pl[k], pr[k]) = (x, el, er);
                } else {
                    let ck = c[k];
                    c[k] = ck + (x - ck) * al;
                    pl[k] += (el - pl[k]) * al;
                    pr[k] += (er - pr[k]) * al;
                }
                let coh = if c[k].re < 0.0 { 0.0 } else { (c[k].norm() / (pl[k] * pr[k] + 1e-12).sqrt()).min(1.0) };
                let e = (el + er) as f64;
                e_sum += e;
                ec_sum += e * coh as f64;
                ef_sum += e * self.ln_f[k] as f64;
            }
            if j >= a {
                let k = j - a;
                pc.coh[k] = if e_sum > 0.0 { (ec_sum / e_sum) as f32 } else { 0.0 };
                pc.e[k] = e_sum;
                pc.ef[k] = ef_sum;
            }
        }
        pc
    }
}

/// One piece's values over a run of frames.
struct Piece {
    db: Vec<f32>,
    flux: Vec<[f32; 2]>,
    pow: Vec<[f32; 2]>,
    coh: Vec<f32>,
    e: Vec<f64>,
    ef: Vec<f64>,
    al: Vec<f64>,
    ar: Vec<f64>,
}

/// kit.rs `onsets`' candidates over a stretch of a piece's onset frames
/// (hop 256: its flux, its power dB), against the flux's 99.5th percentile
/// `top` known beforehand (a song's so far): a flux peak over its local mean
/// (0.2 s) + `delta`, the only one within ±30 ms; each with its level (the
/// most power in the 50 ms from it). (frame of the stretch, level).
pub fn candidates(flux: &[f32], pow: &[f32], top: f32, delta: f32) -> Vec<(usize, f32)> {
    let n = flux.len();
    if n == 0 {
        return Vec::new();
    }
    let fr = kit::SR / kit::ON_HOP as f64;
    let d: Vec<f32> = flux.iter().map(|v| v / top).collect();
    let loc = kit::uniform_filter(&d, (0.2 * fr) as usize | 1);
    let mx = kit::max_filter(&d, (2.0 * kit::ON_GAP * fr) as usize | 1, 0);
    let w = (0.05 * fr) as usize;
    let lv = kit::max_filter(pow, w | 1, -((w / 2) as isize));
    (0..n).filter(|&i| d[i] == mx[i] && d[i] > loc[i] + delta).map(|i| (i, lv[i])).collect()
}


#[cfg(test)]
mod tests;
