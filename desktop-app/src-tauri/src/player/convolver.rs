//! Streaming polyphase convolution — the converter's CPU convolver, made
//! seekable.
//!
//! The converter upsamples with `run_polyphase_pass`: one
//! `CpuDspProcessor` per polyphase branch, each a uniformly partitioned
//! overlap-save convolver (block B = 32768, real FFT of N = 2B kept as its
//! N/2 + 1 bins, partition spectra, Kahan accumulation over partitions,
//! inverse real FFT), fed the whole track from its
//! first sample. That is the right machine for a file, and the wrong one for
//! a player in two ways:
//!
//! * **It cannot start in the middle.** Its frequency-domain delay line (FDL)
//!   only fills by running every earlier block through the full multiply-
//!   accumulate. Starting 30M taps at minute three that way costs tens of
//!   seconds.
//! * **Every branch keeps its own copy of the input spectra**, although all
//!   branches see the same input. At 30M taps ×8 that is 1.8 GB of identical
//!   spectra.
//!
//! This convolver does the same arithmetic — same B, same FFT, same partition
//! spectra, the same Kahan update in the same partition order, the same two
//! scalings — with one FDL shared by all branches and a `prime` step that only
//! transforms the history blocks (no multiply-accumulate) before computing the
//! first block. The block grid is anchored at the first sample of the file, so
//! what comes out does not depend on where playback started: a sample is the
//! same number whether the listener pressed Play at 0:00 or seeked to it.
//!
//! Output index `m` (output rate, counted from the start of the file) is
//!
//! ```text
//!     out[m] = scale · y_p[(m + D) / L],   p = (m + D) mod L
//! ```
//!
//! where `y_p` is branch `p`'s causal convolution with the source and `D` the
//! trim the converter applies after its pass: `(N_full − 1) / 2` for linear
//! phase and TFS, `0` for a plain minimum-phase filter, and the band-weighted
//! group delay for the minimum-phase half of Hybrid-Phase. The equality with
//! the converter is a test in this file, from several start positions.

use crate::audio::dsp_core::{c2r, r2c, C2r, R2c};
use rayon::prelude::*;
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex;
use std::sync::Arc;

type C = Complex<f64>;

/// Partition length. Must equal `CpuDspProcessor::block_size_for()`; the
/// equality test fails if the engine ever changes it.
pub const BLOCK: usize = 32768;
const NFFT: usize = 2 * BLOCK;
/// Bins of a real signal's spectrum kept: 0 ..= NFFT/2. The rest is the
/// conjugate mirror and is never stored (the converter does the same).
pub const NBIN: usize = NFFT / 2 + 1;

/// The full `2·(n−1)`-bin spectrum of a real signal from its half: bin
/// `N − k` is the conjugate of bin `k`.
pub(crate) fn full_from_half(half: &[C]) -> Vec<C> {
    let n = 2 * (half.len() - 1);
    let mut v = Vec::with_capacity(n);
    v.extend_from_slice(half);
    for k in (1..half.len() - 1).rev() {
        v.push(half[k].conj());
    }
    v
}

/// Bins per parallel work item in the multiply-accumulate. Any split gives
/// the same numbers (each bin's sum runs over the partitions in order); this
/// one keeps a work item's accumulators in L2.
const BIN_CHUNK: usize = 4096;

/// How the output of a bank is aligned — the converter's trim.
#[derive(Clone, Copy, Debug)]
pub enum Alignment {
    /// `(N_full − 1) / 2`: linear phase.
    Linear,
    /// `0`: a minimum-phase filter played on its own.
    None,
    /// The band-weighted (200 Hz – 6 kHz) group delay of a minimum-phase
    /// filter, which Hybrid-Phase uses to line it up with the linear branch.
    BandWeighted,
    /// A fixed look-ahead, output frames: TFS, whose window starts this far
    /// ahead of its centre (`tfs::look_ahead`).
    LookAhead(usize),
}

impl Alignment {
    /// The trim of a filter `len` long (`Bank::delay`), output frames. The
    /// band-weighted one is measured on the coefficients themselves: None.
    pub fn look_ahead(self, len: usize) -> Option<usize> {
        match self {
            Alignment::Linear => Some(len.saturating_sub(1) / 2),
            Alignment::None => Some(0),
            Alignment::BandWeighted => None,
            Alignment::LookAhead(k) => Some(k),
        }
    }
}

/// Source audio at the source rate, shared by every chain built on it.
pub struct SourceBuf {
    pub l: Vec<f64>,
    pub r: Vec<f64>,
    pub rate: u32,
}

impl SourceBuf {
    pub fn len(&self) -> usize {
        self.l.len()
    }

    #[inline]
    fn at(ch: &[f64], i: i64) -> f64 {
        if i < 0 || i as usize >= ch.len() {
            0.0
        } else {
            ch[i as usize]
        }
    }
}

/// What a convolver reads: a decoded file, every frame there from the
/// start; a live stream that grows while it plays (`radio::LiveSource`); or
/// a file whose source stages are still running (`grow::GrowSource`). The
/// block grid, the history and the arithmetic are the same; a stream has no
/// end, and a block waits for its frames (`ensure`).
#[derive(Clone)]
pub enum Input {
    File(Arc<SourceBuf>),
    Live(Arc<super::radio::live::LiveSource>),
    Grow(Arc<super::grow::GrowSource>),
}

impl Input {
    /// Output length at `l` branches: the file's `len × L`, or none.
    pub fn total(&self, l: usize) -> u64 {
        match self {
            Input::File(s) => (s.len() * l) as u64,
            Input::Live(_) => u64::MAX,
            Input::Grow(g) => (g.total() * l) as u64,
        }
    }

    /// Make the source frames below `upto` readable: a stream waits for the
    /// network, or gives silence when it is starved; a growing file waits
    /// for its source stages.
    #[inline]
    pub fn ensure(&self, upto: i64) {
        match self {
            Input::File(_) => {}
            Input::Live(s) => s.ensure(upto),
            Input::Grow(g) => g.ensure(upto),
        }
    }

    /// Channel `ch`'s frames from `from` into `x` (zero where there are
    /// none); true when any is non-zero.
    fn window(&self, ch: usize, from: i64, x: &mut [f64]) -> bool {
        match self {
            Input::File(s) => {
                let src = if ch == 0 { &s.l } else { &s.r };
                let mut any = false;
                for (i, o) in x.iter_mut().enumerate() {
                    *o = SourceBuf::at(src, from + i as i64);
                    any |= *o != 0.0;
                }
                any
            }
            Input::Live(s) => s.read(ch, from, x),
            Input::Grow(g) => g.read(ch, from, x),
        }
    }

    /// The half spectrum of `fft.len()` frames from `from`, channel `ch`;
    /// silence gives exact zeros without the transform.
    fn window_spectrum(&self, fft: &R2c, ch: usize, from: i64) -> Vec<C> {
        let n = fft.len();
        let mut x = vec![0.0f64; n];
        let any = self.window(ch, from, &mut x);
        let mut buf = vec![C::new(0.0, 0.0); n / 2 + 1];
        if any {
            r2c(fft, &mut x, &mut buf);
        }
        buf
    }

    /// Block `j`'s input spectrum, channel `ch` (0 left, 1 right).
    pub fn spectrum(&self, fft: &R2c, ch: usize, j: i64) -> Vec<C> {
        let from = (j - 1) * BLOCK as i64;
        match self {
            Input::File(s) => input_spectrum(fft, if ch == 0 { &s.l } else { &s.r }, j),
            Input::Live(s) => read_spectrum(fft, |x| s.read(ch, from, x)),
            Input::Grow(g) => read_spectrum(fft, |x| g.read(ch, from, x)),
        }
    }
}

/// A block's input spectrum from frames `read` copies out (zero where there
/// are none): `input_spectrum`'s arithmetic, the transform skipped for
/// silence as there.
fn read_spectrum(fft: &R2c, read: impl FnOnce(&mut [f64]) -> bool) -> Vec<C> {
    let mut x = vec![0.0f64; NFFT];
    let any = read(&mut x);
    let mut buf = vec![C::new(0.0, 0.0); NBIN];
    if any {
        r2c(fft, &mut x, &mut buf);
    }
    buf
}

/// A filter, split into its polyphase branches and transformed into
/// partition spectra. Built once per (filter, L) and shared.
pub struct Bank {
    pub path: String,
    pub l: usize,
    pub full_len: usize,
    /// `L / Σh` — the converter's gain compensation for the zero-stuffing.
    pub scale: f64,
    /// Output-rate trim, see [`Alignment`].
    pub delay: usize,
    /// `[branch][partition]`, each the half spectrum (`NBIN` bins).
    spectra: Vec<Vec<Vec<C>>>,
    fft: R2c,
    ifft: C2r,
    /// Each branch's first `BLOCK` taps (its partition 0), for a live
    /// stream's head (`HeadLevels`), and those levels once made.
    head: Vec<Vec<f64>>,
    levels: std::sync::OnceLock<Arc<HeadLevels>>,
}

impl Bank {
    /// Load a `.npy` filter and prepare it for `l` branches.
    pub fn load(path: &str, l: usize, align: Alignment, out_rate: u32) -> Result<Bank, String> {
        let coeffs = crate::audio::dsp_core::load_npy_f64(path)?;
        if coeffs.is_empty() {
            return Err(format!("Filter {} is empty", path));
        }
        Ok(Bank::from_coeffs(path, &coeffs, l, align, out_rate))
    }

    pub fn from_coeffs(path: &str, coeffs: &[f64], l: usize, align: Alignment, out_rate: u32) -> Bank {
        // Same expressions as process.rs, in the same order.
        let dc_gain: f64 = coeffs.iter().sum();
        let scale = l as f64 / dc_gain.abs().max(1e-10);
        let full_len = coeffs.len();
        let delay = align.look_ahead(full_len).unwrap_or_else(|| {
            crate::audio::dsp_core::estimate_band_weighted_group_delay(coeffs, out_rate as f64, 200.0, 6000.0)
        });

        let branches = crate::audio::converter::dsp::polyphase::polyphase_decompose(coeffs, l);

        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(NFFT);
        let ifft = planner.plan_fft_inverse(NFFT);

        // Every (branch, partition) half spectrum, exactly as
        // CpuDspProcessor::new_with_coefficients builds them.
        let jobs: Vec<(usize, usize)> = branches
            .iter()
            .enumerate()
            .flat_map(|(p, sub)| (0..partitions(sub.len())).map(move |b| (p, b)))
            .collect();
        let built: Vec<((usize, usize), Vec<C>)> = jobs
            .into_par_iter()
            .map(|(p, b)| {
                let sub = &branches[p];
                let mut x = vec![0.0f64; NFFT];
                let offset = b * BLOCK;
                for i in 0..BLOCK {
                    if offset + i < sub.len() {
                        x[i] = sub[offset + i];
                    }
                }
                let mut block = vec![C::new(0.0, 0.0); NBIN];
                r2c(&fft, &mut x, &mut block);
                ((p, b), block)
            })
            .collect();

        let mut spectra: Vec<Vec<Vec<C>>> = branches
            .iter()
            .map(|sub| Vec::with_capacity(partitions(sub.len())))
            .collect();
        for ((p, b), block) in built {
            debug_assert_eq!(spectra[p].len(), b);
            spectra[p].push(block);
        }

        let head = branches.iter().map(|b| b[..b.len().min(BLOCK)].to_vec()).collect();
        Bank {
            path: path.to_string(),
            l,
            full_len,
            scale,
            delay,
            spectra,
            fft,
            ifft,
            head,
            levels: std::sync::OnceLock::new(),
        }
    }

    /// The head's levels (made on first use, by a live stream).
    fn head_levels(&self) -> Arc<HeadLevels> {
        self.levels.get_or_init(|| Arc::new(HeadLevels::new(&self.head))).clone()
    }

    /// Largest partition count over the branches: the FDL depth.
    pub fn max_partitions(&self) -> usize {
        self.spectra.iter().map(|s| s.len()).max().unwrap_or(1).max(1)
    }

    /// Bytes held by the partition spectra.
    pub fn bytes(&self) -> usize {
        self.spectra.iter().map(|s| s.len()).sum::<usize>() * NBIN * std::mem::size_of::<C>()
    }

    /// Partition spectra for `branch` (length = partition count for that branch).
    /// Each partition is the half spectrum: `NBIN` complex values, bins
    /// 0 ..= NFFT/2 (`full_from_half` gives the rest).
    pub fn branch_spectra(&self, branch: usize) -> &[Vec<C>] {
        &self.spectra[branch]
    }
}

fn partitions(taps: usize) -> usize {
    (taps + BLOCK - 1) / BLOCK
}

// -- Head and tail (a live stream) --------------------------------------------
//
// A block of B frames cannot be computed before its last frame is in: a live
// stream would wait B/fs (0.74 s at 44.1 kHz) for its first block. A live
// stream splits each branch's filter instead: the TAIL - partitions 1..P-1,
// the same spectra, Kahan and order - needs the input only up to the block's
// start, and is computed there; the HEAD - partition 0, the first B taps - is
// convolved in the time grid of short levels, each a uniform overlap-save of
// one segment of the taps:
//
//     taps [0, 4096) in 4096-frame pieces, [4096, 8192) in 4096-frame pieces,
//     [8192, 16384) in 8192-frame pieces, [16384, 32768) in 16384-frame pieces.
//
// A level whose segment starts at its piece's length (all but the first)
// needs only input that is in before its piece starts; the first waits for
// the 4096 frames of its piece. The block is handed out in pieces of
// HEAD_CHUNK frames: the stream waits at most 4096 frames (93 ms at 44.1 kHz)
// past where it plays. Head + tail is the same convolution: the output moves
// by f64 rounding only (a test holds it to 1e-13 of the peak). Files keep the
// whole block, bit for bit with the converter.

/// The piece of a live stream's block handed out at once, in branch frames.
pub const HEAD_CHUNK: usize = 4096;

/// The head's levels: (piece length, first tap). Together, partition 0.
const LEVELS: [(usize, usize); 4] = [(4096, 0), (4096, 4096), (8192, 8192), (16384, 16384)];

/// Spectra of the head's levels for every branch, and their transforms.
pub struct HeadLevels {
    /// Forward and inverse transforms of 2s for s = 4096, 8192, 16384.
    plans: [(R2c, C2r); 3],
    /// `[branch][level]`: the half spectrum of that level's taps, padded to 2s.
    h: Vec<[Vec<C>; 4]>,
}

fn plan_of(s: usize) -> usize {
    match s {
        4096 => 0,
        8192 => 1,
        _ => 2,
    }
}

impl HeadLevels {
    fn new(head: &[Vec<f64>]) -> HeadLevels {
        let mut planner = RealFftPlanner::<f64>::new();
        let plans = [4096usize, 8192, 16384].map(|s| (planner.plan_fft_forward(2 * s), planner.plan_fft_inverse(2 * s)));
        let h = head
            .par_iter()
            .map(|taps| {
                LEVELS.map(|(s, o)| {
                    let mut x = vec![0.0f64; 2 * s];
                    for i in 0..s {
                        if o + i < taps.len() {
                            x[i] = taps[o + i];
                        }
                    }
                    let mut out = vec![C::new(0.0, 0.0); s + 1];
                    r2c(&plans[plan_of(s)].0, &mut x, &mut out);
                    out
                })
            })
            .collect();
        HeadLevels { plans, h }
    }
}

thread_local! {
    /// A worker's product and transform scratch for the head's levels.
    static HEAD_SCRATCH: std::cell::RefCell<(Vec<C>, Vec<C>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
}

/// A live stream's head-and-tail state.
pub(crate) struct HeadTail {
    lv: Arc<HeadLevels>,
    /// Next piece of block `next_block` (0 .. BLOCK / HEAD_CHUNK).
    pub(crate) chunk: usize,
    /// The FDL holds every block up to this one.
    pub(crate) filled: i64,
    /// `[branch * 2 + channel][level]`: the level's inverse transform (2s
    /// frames; its second half is the level's piece).
    out: Vec<[Vec<f64>; 4]>,
    /// Spectra (left, right) of the last 4096-piece's window, by piece.
    prev4: Option<(i64, [Vec<C>; 2])>,
    /// `[branch * 2 + channel]`: a piece's output before it is interleaved.
    cols: Vec<Vec<f64>>,
}

impl HeadTail {
    /// Piece `c` of a block (head piece `m`) into `pend_*`. The input must
    /// be in up to the piece's end. The head's levels due now: every piece
    /// the two 4096-levels (windows of pieces m and m - 1), the 8192-level
    /// when its piece starts here (the window of the 8192-piece before), the
    /// 16384-level likewise. Each sample is the tail (`tail(branch * 2 +
    /// channel, frame of the block)`, its inverse transform unscaled) and the
    /// levels, each with its own 1/N, added in a fixed order; then the
    /// polyphase scale. Output order as `compute_block`'s.
    pub(crate) fn piece(
        &mut self,
        input: &Input,
        m: i64,
        c: usize,
        l: usize,
        scale: f64,
        tail: impl Fn(usize, usize) -> f64 + Sync,
        pend_l: &mut Vec<f64>,
        pend_r: &mut Vec<f64>,
    ) {
        let lv = self.lv.clone();
        let s4 = HEAD_CHUNK as i64;
        // The windows due, both channels, in one parallel pass; piece m - 1's
        // is kept from the last piece.
        let prev = match self.prev4.take() {
            Some((k, x)) if k == m - 1 => Some(x),
            _ => None,
        };
        let mut wins: Vec<(usize, i64)> = vec![(0, (m - 1) * s4)];
        if prev.is_none() {
            wins.push((0, (m - 2) * s4));
        }
        if c % 2 == 0 {
            wins.push((1, (m / 2 - 2) * 2 * s4));
        }
        if c % 4 == 0 {
            wins.push((2, (m / 4 - 2) * 4 * s4));
        }
        let jobs: Vec<(usize, i64, usize)> = wins.iter().flat_map(|&(pi, from)| [(pi, from, 0), (pi, from, 1)]).collect();
        let mut specs = jobs
            .into_par_iter()
            .map(|(pi, from, ch)| input.window_spectrum(&lv.plans[pi].0, ch, from))
            .collect::<Vec<_>>()
            .into_iter();
        let mut pair = || -> [Vec<C>; 2] { [specs.next().unwrap(), specs.next().unwrap()] };
        let x4 = pair();
        let x4p = match prev {
            Some(x) => x,
            None => pair(),
        };
        let x8 = (c % 2 == 0).then(&mut pair);
        let x16 = (c % 4 == 0).then(&mut pair);
        let due: [Option<&[Vec<C>; 2]>; 4] = [Some(&x4), Some(&x4p), x8.as_ref(), x16.as_ref()];

        // Every (branch, channel) in parallel: its due levels (product and
        // inverse, reused buffers), then its column of the piece.
        let inv_n = 1.0 / NFFT as f64;
        let inv = LEVELS.map(|(s, _)| 1.0 / (2 * s) as f64);
        let off8 = (c % 2) * HEAD_CHUNK;
        let off16 = (c % 4) * HEAD_CHUNK;
        self.out.par_iter_mut().zip(self.cols.par_iter_mut()).enumerate().for_each(|(idx, (outs, col))| {
            HEAD_SCRATCH.with(|cell| {
                let (prod, scratch) = &mut *cell.borrow_mut();
                for (li, y) in outs.iter_mut().enumerate() {
                    let Some(x) = due[li] else { continue };
                    let (s, _) = LEVELS[li];
                    let ifft = &lv.plans[plan_of(s)].1;
                    let h = &lv.h[idx / 2][li];
                    prod.clear();
                    prod.extend(x[idx % 2].iter().zip(h).map(|(a, b)| a * b));
                    prod[0].im = 0.0;
                    let n = prod.len();
                    prod[n - 1].im = 0.0;
                    scratch.resize(ifft.get_scratch_len(), C::new(0.0, 0.0));
                    if let Err(e) = ifft.process_with_scratch(prod, y, scratch) {
                        crate::aelog!("[DSP] c2r failed: {}", e);
                    }
                }
            });
            let (a, b) = (&outs[0][4096..], &outs[1][4096..]);
            let (c8, c16) = (&outs[2][8192 + off8..], &outs[3][16384 + off16..]);
            for t in 0..HEAD_CHUNK {
                let v = tail(idx, c * HEAD_CHUNK + t) * inv_n + a[t] * inv[0] + b[t] * inv[1] + c8[t] * inv[2] + c16[t] * inv[3];
                col[t] = v * scale;
            }
        });
        self.prev4 = Some((m, x4));

        pend_l.clear();
        pend_r.clear();
        for t in 0..HEAD_CHUNK {
            for p in 0..l {
                pend_l.push(self.cols[2 * p][t]);
                pend_r.push(self.cols[2 * p + 1][t]);
            }
        }
    }

    pub(crate) fn new(bank: &Bank, filled: i64) -> HeadTail {
        let n = 2 * bank.l;
        HeadTail {
            lv: bank.head_levels(),
            chunk: 0,
            filled,
            out: (0..n).map(|_| LEVELS.map(|(s, _)| vec![0.0; 2 * s])).collect(),
            prev4: None,
            cols: (0..n).map(|_| vec![0.0; HEAD_CHUNK]).collect(),
        }
    }
}

/// Block `j`'s input spectra, left and right.
type Spectra = Arc<[Vec<C>; 2]>;

/// The input spectra of a Hybrid-Phase pair, shared: both streams read the
/// same input, so block `j`'s spectra are the same numbers for both. The one
/// that needs a block first transforms it and leaves it here; the other takes
/// it while either still holds it in its ring. Nothing is kept here: a block
/// lives as long as a ring holds it, so the pair holds the union of its two
/// rings (P + the pair's distance in blocks) instead of two of them. A block
/// gone from both is transformed again — the same numbers.
#[derive(Default)]
pub struct SpectrumShare {
    blocks: std::sync::Mutex<std::collections::BTreeMap<i64, std::sync::Weak<[Vec<C>; 2]>>>,
}

impl SpectrumShare {
    pub fn new() -> Arc<SpectrumShare> {
        Arc::new(SpectrumShare::default())
    }

    fn get(&self, j: i64) -> Option<Spectra> {
        let g = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&j).and_then(|w| w.upgrade())
    }

    /// Leave `s` as block `j`'s; the one already here when another stream
    /// left it meanwhile (the same numbers).
    fn put(&self, j: i64, s: Spectra) -> Spectra {
        let mut g = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(have) = g.get(&j).and_then(|w| w.upgrade()) {
            return have;
        }
        g.insert(j, Arc::downgrade(&s));
        if g.len() % 64 == 0 {
            g.retain(|_, w| w.strong_count() > 0);
        }
        s
    }

    /// Blocks some stream of the pair still holds.
    #[cfg(test)]
    fn live(&self) -> usize {
        let g = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        g.values().filter(|w| w.strong_count() > 0).count()
    }
}

/// Block `j`'s spectra: from `share` when the pair's other stream has them,
/// else transformed here (and left in `share`). The transform runs without
/// the share's lock.
fn block_spectra(input: &Input, fft: &R2c, share: Option<&SpectrumShare>, j: i64) -> Spectra {
    if let Some(s) = share.and_then(|sh| sh.get(j)) {
        return s;
    }
    let (sl, sr) = rayon::join(|| input.spectrum(fft, 0, j), || input.spectrum(fft, 1, j));
    let s = Arc::new([sl, sr]);
    match share {
        Some(sh) => sh.put(j, s),
        None => s,
    }
}

/// One bank running over one source, producing aligned output-rate samples
/// from a chosen position onwards.
pub struct PolyStream {
    bank: Arc<Bank>,
    input: Input,
    /// FDL: ring of input spectra (left and right of a block). Slot of block
    /// `j` is `j mod depth`.
    depth: usize,
    fdl: Vec<Spectra>,
    /// The pair's shared spectra (Hybrid-Phase), or none.
    share: Option<Arc<SpectrumShare>>,
    /// A live stream's head and tail; none for a file (whole blocks).
    ht: Option<HeadTail>,
    /// Next block to compute.
    next_block: i64,
    /// Per (branch, channel) accumulator and Kahan compensation (half
    /// spectra), and the accumulator's inverse transform.
    acc: Vec<Vec<C>>,
    comp: Vec<Vec<C>>,
    time: Vec<Vec<f64>>,
    /// Aligned output of the last computed block not yet handed out.
    pend_l: Vec<f64>,
    pend_r: Vec<f64>,
    pend_pos: usize,
    /// Absolute output index of `pend_*[pend_pos]`.
    pend_start: u64,
    /// Absolute output index of the next sample `read` returns.
    pos: u64,
    /// Output length of the whole track (`len × L`), as the converter trims
    /// it; a live stream has none (`u64::MAX`).
    total: u64,
    /// Wall time of the most recently completed compute_block(), in nanoseconds.
    /// Zero before any block has been computed. Used by the calibration store.
    last_block_wall_ns: u64,
}

impl PolyStream {
    /// Start producing at output index `start`. Only the history blocks are
    /// transformed; nothing is multiplied until the first `read`.
    #[cfg(test)]
    pub fn new(bank: Arc<Bank>, src: Arc<SourceBuf>, start: u64) -> PolyStream {
        PolyStream::with_input(bank, Input::File(src), start)
    }

    /// `new` over any input: a file, or a live stream (`radio`).
    #[cfg(test)]
    pub fn with_input(bank: Arc<Bank>, input: Input, start: u64) -> PolyStream {
        PolyStream::with_input_shared(bank, input, start, None)
    }

    /// `with_input` for one stream of a pair over the same input: block
    /// spectra are taken from and left in `share`. The output is the same,
    /// bit for bit.
    pub fn with_input_shared(bank: Arc<Bank>, input: Input, start: u64, share: Option<Arc<SpectrumShare>>) -> PolyStream {
        let head = matches!(input, Input::Live(_));
        PolyStream::build(bank, input, start, share, head)
    }

    /// A stream with the head and tail on any input (a live stream has them
    /// always): the tests compare it with the whole blocks on a file.
    #[cfg(test)]
    pub fn with_head(bank: Arc<Bank>, input: Input, start: u64) -> PolyStream {
        PolyStream::build(bank, input, start, None, true)
    }

    fn build(bank: Arc<Bank>, input: Input, start: u64, share: Option<Arc<SpectrumShare>>, head: bool) -> PolyStream {
        let depth = bank.max_partitions();
        let l = bank.l;
        let total = input.total(l);
        let c = start + bank.delay as u64;
        let t = (c / l as u64) as i64;
        let j0 = t / BLOCK as i64;

        let mut s = PolyStream {
            depth,
            fdl: {
                let zero: Spectra = Arc::new([vec![C::new(0.0, 0.0); NBIN], vec![C::new(0.0, 0.0); NBIN]]);
                vec![zero; depth]
            },
            share,
            ht: head.then(|| HeadTail::new(&bank, j0 - 1)),
            next_block: j0,
            acc: (0..2 * l).map(|_| vec![C::new(0.0, 0.0); NBIN]).collect(),
            comp: (0..2 * l).map(|_| vec![C::new(0.0, 0.0); NBIN]).collect(),
            time: (0..2 * l).map(|_| vec![0.0; NFFT]).collect(),
            pend_l: Vec::with_capacity(BLOCK * l),
            pend_r: Vec::with_capacity(BLOCK * l),
            pend_pos: 0,
            pend_start: 0,
            pos: start,
            total,
            bank,
            input,
            last_block_wall_ns: 0,
        };
        s.prime(j0);
        s
    }

    // Calibration and diagnostics may need to know which bank drives a CPU
    // stream (e.g. to look up the corresponding timing record).
    #[allow(dead_code)]
    pub fn bank(&self) -> &Arc<Bank> {
        &self.bank
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Transform the `depth − 1` blocks before `j0` into the FDL.
    fn prime(&mut self, j0: i64) {
        let depth = self.depth as i64;
        let first = j0 - (depth - 1);
        let blocks: Vec<i64> = (first..j0).collect();
        // The history ends where block j0's window starts its second half.
        self.input.ensure(j0 * BLOCK as i64);
        let input = &self.input;
        let fft = &self.bank.fft;
        let share = self.share.as_deref();
        let spectra: Vec<(i64, Spectra)> = blocks
            .into_par_iter()
            .map(|j| (j, block_spectra(input, fft, share, j)))
            .collect();
        for (j, s) in spectra {
            self.fdl[j.rem_euclid(depth) as usize] = s;
        }
    }

    /// Wall time of the most recently completed compute_block(), nanoseconds.
    /// Zero before any block has been computed. Used by the calibration store.
    #[allow(dead_code)]
    pub fn last_block_wall_ns(&self) -> u64 {
        self.last_block_wall_ns
    }

    /// Compute block `next_block`: its input spectrum, then every branch.
    fn compute_block(&mut self) {
        let t0 = std::time::Instant::now();
        let j = self.next_block;
        let depth = self.depth as i64;
        let slot = j.rem_euclid(depth) as usize;
        {
            // Block j's window reaches source frame (j + 1)·B.
            self.input.ensure((j + 1) * BLOCK as i64);
            self.fdl[slot] = block_spectra(&self.input, &self.bank.fft, self.share.as_deref(), j);
        }
        self.mac(j, 0);

        let l = self.bank.l;
        let bank = &self.bank;

        // Scatter into aligned output order: causal index c = t·L + p, output
        // index m = c − D. Two scalings, in the engine's order: 1/N, then
        // the polyphase scale.
        let inv_n = 1.0 / NFFT as f64;
        let scale = bank.scale;
        let d = bank.delay as i64;
        let c0 = j * (BLOCK * l) as i64;
        let m0 = c0 - d;
        self.pend_l.clear();
        self.pend_r.clear();
        for t in 0..BLOCK {
            for p in 0..l {
                let vl = self.time[2 * p][t + BLOCK] * inv_n;
                let vr = self.time[2 * p + 1][t + BLOCK] * inv_n;
                self.pend_l.push(vl * scale);
                self.pend_r.push(vr * scale);
            }
        }
        // pend[0] sits at output index m0 (which may be negative near the
        // start of a file; those samples are never read).
        let skip = if (self.pos as i64) > m0 { (self.pos as i64 - m0) as usize } else { 0 };
        self.pend_pos = skip.min(self.pend_l.len());
        self.pend_start = (m0 + self.pend_pos as i64).max(0) as u64;
        self.next_block = j + 1;
        self.last_block_wall_ns = t0.elapsed().as_nanos() as u64;
        #[cfg(all(test, feature = "gpu-player-tests"))]
        trace::push(trace::Ev::new(t0, self.last_block_wall_ns, j, self.bank.delay, false));
    }

    /// Multiply-accumulate block `j` over partitions `k0..`, then the inverse
    /// transforms into `time`. Work item = (branch, channel, bin range); the
    /// arithmetic per bin is the engine's, partition by partition in order.
    /// `k0 = 1` is the head-and-tail stream's tail (`compute_chunk`).
    fn mac(&mut self, j: i64, k0: usize) {
        let l = self.bank.l;
        let depth = self.depth as i64;
        let bank = &self.bank;
        let fdl = &self.fdl;
        struct Item<'a> {
            p: usize,
            ch: usize,
            off: usize,
            acc: &'a mut [C],
            comp: &'a mut [C],
        }
        let mut items: Vec<Item> = Vec::with_capacity(2 * l * (NBIN / BIN_CHUNK + 1));
        for (idx, (acc, comp)) in self.acc.iter_mut().zip(self.comp.iter_mut()).enumerate() {
            let p = idx / 2;
            let ch = idx % 2;
            for ((a, c), n) in acc
                .chunks_mut(BIN_CHUNK)
                .zip(comp.chunks_mut(BIN_CHUNK))
                .zip(0..)
            {
                items.push(Item { p, ch, off: n * BIN_CHUNK, acc: a, comp: c });
            }
        }
        items.into_par_iter().for_each(|it| {
            for v in it.acc.iter_mut() {
                *v = C::new(0.0, 0.0);
            }
            for v in it.comp.iter_mut() {
                *v = C::new(0.0, 0.0);
            }
            let parts = &bank.spectra[it.p];
            let len = it.acc.len();
            for (k, h_full) in parts.iter().enumerate().skip(k0) {
                let hist = (j - k as i64).rem_euclid(depth) as usize;
                let h = &h_full[it.off..it.off + len];
                let d = &fdl[hist][it.ch][it.off..it.off + len];
                for i in 0..len {
                    // Kahan summation — CpuDspProcessor::process_partitions.
                    let prod = d[i] * h[i];
                    let y = prod - it.comp[i];
                    let t = it.acc[i] + y;
                    it.comp[i] = (t - it.acc[i]) - y;
                    it.acc[i] = t;
                }
            }
        });

        // Inverse transforms, one per (branch, channel).
        let ifft = &self.bank.ifft;
        self.acc
            .par_iter_mut()
            .zip(self.time.par_iter_mut())
            .for_each(|(a, y)| c2r(ifft, a, y));
    }

    /// A live stream's next piece (`HEAD_CHUNK` branch frames) of block
    /// `next_block`: at the block's first piece its tail (partitions 1..,
    /// input up to the block's start); for every piece the head's levels
    /// (input up to the piece's end), added in a fixed order to the tail.
    fn compute_chunk(&mut self) {
        let t0 = std::time::Instant::now();
        let mut ht = self.ht.take().expect("a head-and-tail stream");
        let j = self.next_block;
        let c = ht.chunk;
        let per = (BLOCK / HEAD_CHUNK) as i64;
        let m = j * per + c as i64;
        let depth = self.depth as i64;
        if c == 0 {
            // Every block before j into the FDL (its frames are in: the
            // last piece of block j - 1 waited for them), then the tail.
            while ht.filled < j - 1 {
                let b = ht.filled + 1;
                self.input.ensure((b + 1) * BLOCK as i64);
                self.fdl[b.rem_euclid(depth) as usize] = block_spectra(&self.input, &self.bank.fft, self.share.as_deref(), b);
                ht.filled = b;
            }
            self.mac(j, 1);
        }
        // The piece's last frame.
        self.input.ensure((m + 1) * HEAD_CHUNK as i64);

        let l = self.bank.l;
        let c0 = (j * BLOCK as i64 + c as i64 * HEAD_CHUNK as i64) * l as i64;
        let m0 = c0 - self.bank.delay as i64;
        let time = &self.time;
        ht.piece(&self.input, m, c, l, self.bank.scale, |idx, tb| time[idx][BLOCK + tb], &mut self.pend_l, &mut self.pend_r);
        let skip = if (self.pos as i64) > m0 { (self.pos as i64 - m0) as usize } else { 0 };
        self.pend_pos = skip.min(self.pend_l.len());
        self.pend_start = (m0 + self.pend_pos as i64).max(0) as u64;
        ht.chunk = c + 1;
        if ht.chunk == BLOCK / HEAD_CHUNK {
            ht.chunk = 0;
            self.next_block = j + 1;
        }
        self.ht = Some(ht);
        self.last_block_wall_ns = t0.elapsed().as_nanos() as u64;
    }

    /// Fill `out_l`/`out_r` with the next samples. Past the end of the track
    /// (the converter's `len × L`) the output is silence. Returns how many
    /// samples were inside the track.
    pub fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
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
            if self.pend_pos >= self.pend_l.len() {
                if self.ht.is_some() {
                    self.compute_chunk();
                } else {
                    self.compute_block();
                }
                continue;
            }
            debug_assert_eq!(self.pend_start, self.pos);
            let avail = self.pend_l.len() - self.pend_pos;
            let left_in_track = (self.total - self.pos) as usize;
            let take = avail.min(n - done).min(left_in_track);
            out_l[done..done + take]
                .copy_from_slice(&self.pend_l[self.pend_pos..self.pend_pos + take]);
            out_r[done..done + take]
                .copy_from_slice(&self.pend_r[self.pend_pos..self.pend_pos + take]);
            self.pend_pos += take;
            self.pend_start += take as u64;
            self.pos += take as u64;
            done += take;
            inside += take;
        }
        inside
    }
}

// ── Calibration pre-trial (K2) ───────────────────────────────────────────────
//
// A block costs the same whatever the FDL holds (the multiply-accumulate runs
// over every partition without looking at the values; the only shortcut is
// the input FFT of an all-zero window, which `trial_block_index` avoids), so
// a trial times real blocks of a stream that was never primed.

impl PolyStream {
    /// Calibration pre-trial (K2): a stream whose next block is source block
    /// `j`, not primed (its FDL is zero). FDL, accumulators and compensation
    /// are allocated and their pages touched in parallel on the caller's
    /// pool, and `pend_*` are touched once, so the timed block takes no page
    /// faults. Its output is never read.
    pub fn new_trial(bank: Arc<Bank>, src: Arc<SourceBuf>, j: i64) -> PolyStream {
        let depth = bank.max_partitions();
        let l = bank.l;
        let total = (src.len() * l) as u64;
        // A zeroed `vec!` comes back as untouched demand-zero pages, and the
        // timed block then takes the faults (bench B1b: +23 to +89 ms at 30M
        // on 4 E-cores). They are taken here instead, in the allocation.
        let zeroed = |n: usize| -> Vec<Vec<C>> {
            (0..n)
                .into_par_iter()
                .map(|_| {
                    let mut v = vec![C::new(0.0, 0.0); NBIN];
                    touch_pages(&mut v);
                    v
                })
                .collect()
        };
        // The inverse transforms' outputs, touched the same way.
        let time: Vec<Vec<f64>> = (0..2 * l)
            .into_par_iter()
            .map(|_| {
                let mut v = Vec::with_capacity(NFFT);
                v.resize(NFFT, 0.0);
                v
            })
            .collect();
        // resize writes every element (a zeroed `vec!` may leave the pages
        // untouched until the scatter writes them).
        let mut pend_l = Vec::with_capacity(BLOCK * l);
        let mut pend_r = Vec::with_capacity(BLOCK * l);
        pend_l.resize(BLOCK * l, 0.0);
        pend_r.resize(BLOCK * l, 0.0);
        pend_l.clear();
        pend_r.clear();
        let (fl, fr) = (zeroed(depth), zeroed(depth));
        PolyStream {
            depth,
            fdl: fl.into_iter().zip(fr).map(|(a, b)| Arc::new([a, b])).collect(),
            share: None,
            ht: None,
            next_block: j,
            acc: zeroed(2 * l),
            comp: zeroed(2 * l),
            time,
            pend_l,
            pend_r,
            pend_pos: 0,
            pend_start: 0,
            pos: 0,
            total,
            bank,
            input: Input::File(src),
            last_block_wall_ns: 0,
        }
    }

    /// Pre-trial of an HP pair: reuse this allocation for `bank` (the other
    /// half; the lin and min files of a rung have the same size) and make
    /// source block `j` the next. `Err` when the FDL depth or L differ.
    pub fn trial_rebind(&mut self, bank: Arc<Bank>, j: i64) -> Result<(), ()> {
        if bank.max_partitions() != self.depth || bank.l != self.bank.l {
            return Err(());
        }
        self.bank = bank;
        self.next_block = j;
        Ok(())
    }

    /// Pre-trial: compute one whole block; its wall time, nanoseconds.
    pub fn trial_block_ns(&mut self) -> u64 {
        self.compute_block();
        self.last_block_wall_ns
    }
}

/// What `trial_blocks` measured.
#[derive(Clone, Debug)]
pub struct TrialTiming {
    /// The source block the trial started at.
    pub j: i64,
    /// Worker threads of the pool it ran in.
    pub threads: usize,
    /// Allocating the stream(s).
    pub alloc_ns: u64,
    /// `[bank][block]` wall time of each block.
    pub block_ns: Vec<Vec<u64>>,
    /// One extra block was run per bank (`more()` said so).
    pub extra: bool,
    /// Freeing the stream.
    pub drop_ns: u64,
    /// `stop()` ended it early; the timings are incomplete.
    pub stopped: bool,
}

/// The pre-trial core, shared by the player and the benches. Call it inside
/// the pool whose speed is wanted. For each bank in order (the order
/// `HybridStage` runs them), `blocks` timed blocks from
/// `trial_block_index(src)`; when `blocks == 1` and `more()` is true after a
/// bank's block, one extra block for that bank (`extra`). The banks of a pair
/// share one allocation when they can. `stop()` is polled before every block.
pub fn trial_blocks(
    banks: &[Arc<Bank>],
    src: &Arc<SourceBuf>,
    blocks: usize,
    more: &(dyn Fn() -> bool + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> TrialTiming {
    let j = trial_block_index(src);
    let mut t = TrialTiming {
        j,
        threads: rayon::current_num_threads(),
        alloc_ns: 0,
        block_ns: Vec::with_capacity(banks.len()),
        extra: false,
        drop_ns: 0,
        stopped: false,
    };
    let mut st: Option<PolyStream> = None;
    'banks: for bank in banks {
        let rebound = match st.as_mut() {
            Some(s) => s.trial_rebind(bank.clone(), j).is_ok(),
            None => false,
        };
        if !rebound {
            if stop() {
                t.stopped = true;
                break;
            }
            let td = std::time::Instant::now();
            drop(st.take());
            t.drop_ns += td.elapsed().as_nanos() as u64;
            let ta = std::time::Instant::now();
            st = Some(PolyStream::new_trial(bank.clone(), src.clone(), j));
            t.alloc_ns += ta.elapsed().as_nanos() as u64;
        }
        let Some(s) = st.as_mut() else { break };
        let mut times = Vec::with_capacity(blocks + 1);
        for _ in 0..blocks {
            if stop() {
                t.stopped = true;
                t.block_ns.push(times);
                break 'banks;
            }
            times.push(s.trial_block_ns());
        }
        if blocks == 1 && more() {
            if stop() {
                t.stopped = true;
                t.block_ns.push(times);
                break;
            }
            times.push(s.trial_block_ns());
            t.extra = true;
        }
        t.block_ns.push(times);
    }
    let td = std::time::Instant::now();
    drop(st);
    t.drop_ns += td.elapsed().as_nanos() as u64;
    t
}

/// Where a trial starts: the first block `j ≥ max(1, n_blocks / 4)` whose
/// input window `[(j−1)B, (j+1)B)` holds a non-zero sample in either
/// channel, so the input FFT is not skipped. Scans at most 64 blocks, then
/// settles for the first (a silent track skips that FFT in playback too).
pub fn trial_block_index(src: &SourceBuf) -> i64 {
    let n = src.len();
    if n < BLOCK {
        return 1;
    }
    let first = ((n / BLOCK) / 4).max(1);
    for j in first..first + 64 {
        let a = (j - 1) * BLOCK;
        if a >= n {
            break;
        }
        let b = ((j + 1) * BLOCK).min(n);
        if src.l[a..b].iter().any(|&v| v != 0.0) || src.r[a..b].iter().any(|&v| v != 0.0) {
            return j as i64;
        }
    }
    first as i64
}

/// Per-block timing of the convolvers (case C and the benches): one event per
/// computed block, off until `enable(true)`. Test builds only.
#[cfg(all(test, feature = "gpu-player-tests"))]
pub mod trace {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    const CAP: usize = 100_000;

    static ON: AtomicBool = AtomicBool::new(false);
    static T0: OnceLock<Instant> = OnceLock::new();
    static EVENTS: Mutex<Vec<Ev>> = Mutex::new(Vec::new());

    /// One computed block.
    #[derive(Clone, Copy, Debug)]
    pub struct Ev {
        /// Start, nanoseconds since the trace's origin (`t0()`).
        pub start_ns: u64,
        pub dur_ns: u64,
        /// Block index.
        pub j: i64,
        /// The bank's output trim (tells the halves of a pair apart).
        pub delay: usize,
        pub gpu: bool,
    }

    impl Ev {
        pub fn new(start: Instant, dur_ns: u64, j: i64, delay: usize, gpu: bool) -> Ev {
            let start_ns = T0.get()
                .map_or(0, |t0| start.saturating_duration_since(*t0).as_nanos() as u64);
            Ev { start_ns, dur_ns, j, delay, gpu }
        }
    }

    /// The origin of `start_ns`: the first `enable` (or the first call here).
    pub fn t0() -> Instant {
        *T0.get_or_init(Instant::now)
    }

    pub fn enable(on: bool) {
        t0();
        ON.store(on, Ordering::Relaxed);
    }

    /// Take the events recorded so far.
    pub fn drain() -> Vec<Ev> {
        std::mem::take(&mut *EVENTS.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn push(ev: Ev) {
        if !ON.load(Ordering::Relaxed) {
            return;
        }
        let mut g = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() < CAP {
            g.push(ev);
        }
    }
}

/// FFT of `[x[(j−1)B .. jB) | x[jB .. (j+1)B)]` — the overlap-save input of
/// block `j`, exactly what `start_new_block` transforms when the converter
/// reaches it (zeros before the first sample and after the last).
/// One store in every 4 KiB page of `v` (and its last element, for a buffer
/// that does not start on a page), so the pages are mapped now and not inside
/// a timed block. Volatile: a plain zero store into zeroed memory may be
/// removed.
fn touch_pages(v: &mut [C]) {
    const PER_PAGE: usize = 4096 / std::mem::size_of::<C>();
    let p = v.as_mut_ptr();
    let mut i = 0;
    while i < v.len() {
        // SAFETY: i < v.len(), inside the slice.
        unsafe { std::ptr::write_volatile(p.add(i), C::new(0.0, 0.0)) };
        i += PER_PAGE;
    }
    if let Some(last) = v.len().checked_sub(1) {
        // SAFETY: last < v.len().
        unsafe { std::ptr::write_volatile(p.add(last), C::new(0.0, 0.0)) };
    }
}

fn input_spectrum(fft: &R2c, x: &[f64], j: i64) -> Vec<C> {
    let mut w = vec![0.0f64; NFFT];
    let base = (j - 1) * BLOCK as i64;
    let mut any = false;
    for i in 0..NFFT {
        let v = SourceBuf::at(x, base + i as i64);
        if v != 0.0 {
            any = true;
        }
        w[i] = v;
    }
    let mut buf = vec![C::new(0.0, 0.0); NBIN];
    if any {
        r2c(fft, &mut w, &mut buf);
    }
    // An all-zero input transforms to exact zeros, which is what the
    // converter's zero-initialised FDL holds; skipping the FFT is identical.
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::processor::DspProcessor;
    use rustfft::FftPlanner;

    /// The converter's polyphase pass, reproduced from process.rs for a
    /// small case: one CpuDspProcessor per branch, whole input then flush,
    /// strided scatter, trim `2B·L + ahead` (the filter's look-ahead: D for
    /// linear phase, K for TFS, 0 for minimum phase), truncate to `len × L`.
    fn converter_reference(coeffs: &[f64], l: usize, x_l: &[f64], x_r: &[f64], ahead: usize) -> (Vec<f64>, Vec<f64>) {
        use crate::audio::dsp_core::CpuDspProcessor;
        let phases = crate::audio::converter::dsp::polyphase::polyphase_decompose(coeffs, l);
        let dc: f64 = coeffs.iter().sum();
        let scale = l as f64 / dc.abs().max(1e-10);
        let sub_taps = phases[0].len();
        let ola = CpuDspProcessor::output_latency_for(sub_taps);
        let flush = ola + ahead.div_ceil(l).max((sub_taps - 1) / 2) + 1;
        let n_in = x_l.len();
        let per = n_in + flush;
        let mut out_l = vec![0.0; per * l];
        let mut out_r = vec![0.0; per * l];
        let chunk = 32768;
        for (p, sub) in phases.iter().enumerate() {
            let mut dsp = CpuDspProcessor::new_with_coefficients(sub);
            let mut ol = vec![0.0; chunk];
            let mut or = vec![0.0; chunk];
            let mut pos = 0;
            while pos < per {
                let end = (pos + chunk).min(per);
                let a = end - pos;
                let il: Vec<f64> = (pos..end).map(|i| if i < n_in { x_l[i] } else { 0.0 }).collect();
                let ir: Vec<f64> = (pos..end).map(|i| if i < n_in { x_r[i] } else { 0.0 }).collect();
                dsp.process_audio(&il, &ir, &mut ol[..a], &mut or[..a], a);
                for i in 0..a {
                    out_l[(pos + i) * l + p] = ol[i] * scale;
                    out_r[(pos + i) * l + p] = or[i] * scale;
                }
                pos = end;
            }
        }
        let trim = ola * l + ahead;
        out_l.drain(..trim);
        out_r.drain(..trim);
        out_l.truncate(n_in * l);
        out_r.truncate(n_in * l);
        (out_l, out_r)
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
        (0..n).map(|_| rng.gen_range(-0.9..0.9)).collect()
    }

    fn check(coeffs: &[f64], l: usize, n_in: usize, align: Alignment) {
        let x_l = noise(n_in, 1);
        let x_r = noise(n_in, 2);
        let ahead = align.look_ahead(coeffs.len()).expect("a fixed trim");
        let (ref_l, ref_r) = converter_reference(coeffs, l, &x_l, &x_r, ahead);
        let bank = Arc::new(Bank::from_coeffs("test", coeffs, l, align, 352_800));
        let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
        let total = ref_l.len();
        // Start at 0, mid-block, on a block boundary, and near the end.
        let starts = [0usize, 12_345, BLOCK * l, BLOCK * l * 3 + 7, total - 5_000];
        for &s in &starts {
            let mut st = PolyStream::new(bank.clone(), src.clone(), s as u64);
            let want = total - s;
            let mut got_l = vec![0.0; want];
            let mut got_r = vec![0.0; want];
            // Read in odd-sized pieces to exercise block crossings.
            let mut off = 0;
            let mut step = 7_777;
            while off < want {
                let e = (off + step).min(want);
                st.read(&mut got_l[off..e], &mut got_r[off..e]);
                off = e;
                step = step * 3 / 2 + 1;
            }
            for i in 0..want {
                assert!(
                    got_l[i].to_bits() == ref_l[s + i].to_bits() && got_r[i].to_bits() == ref_r[s + i].to_bits(),
                    "start {} idx {}: stream ({}, {}) != converter ({}, {})",
                    s, i, got_l[i], got_r[i], ref_l[s + i], ref_r[s + i]
                );
            }
        }
    }

    /// A live stream, fed while it plays, gives the file's numbers: the same
    /// source — any f64, as the source stages hand on — read by a file input
    /// and by a `LiveSource` its writer fills in odd pieces with pauses, from
    /// the start and from a later position (primed from the stream's
    /// history). Kept in f32, as a live source once kept it, the same source
    /// plays other numbers.
    /// A live stream plays head and tail (it waits for a piece, not a whole
    /// block): bit for bit what the file plays through head and tail, and
    /// within f64 rounding (1e-13 of the peak, -260 dBFS for a full-scale
    /// signal) of the file's whole blocks, which the converter's are.
    fn check_live(coeffs: &[f64], l: usize, n_in: usize, align: Alignment) {
        use super::super::radio::live::LiveSource;
        let x_l = noise(n_in, 3);
        let x_r = noise(n_in, 4);
        assert!(x_l.iter().any(|&v| v as f32 as f64 != v), "a source no f32 holds");
        let bank = Arc::new(Bank::from_coeffs("test", coeffs, l, align, 352_800));
        let file = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
        let total = n_in * l;
        let mut from_start = None;
        for &s in &[0usize, BLOCK * l * 2 + 7] {
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
            let mut a = PolyStream::new(bank.clone(), file.clone(), s as u64);
            let mut h = PolyStream::with_head(bank.clone(), Input::File(file.clone()), s as u64);
            let mut b = PolyStream::with_input(bank.clone(), Input::Live(live.clone()), s as u64);
            assert_eq!(b.total(), u64::MAX, "a stream has no end");
            let want = total - s;
            let (mut al, mut ar) = (vec![0.0; want], vec![0.0; want]);
            let (mut hl, mut hr) = (vec![0.0; want], vec![0.0; want]);
            let (mut bl, mut br) = (vec![0.0; want], vec![0.0; want]);
            a.read(&mut al, &mut ar);
            h.read(&mut hl, &mut hr);
            let mut off = 0;
            let mut step = 7_777;
            while off < want {
                let e = (off + step).min(want);
                b.read(&mut bl[off..e], &mut br[off..e]);
                off = e;
                step = step * 3 / 2 + 1;
            }
            writer.join().unwrap();
            assert_eq!(live.stats().starves, 0, "the writer kept up");
            let peak = al.iter().chain(&ar).fold(0f64, |p, v| p.max(v.abs()));
            for i in 0..want {
                assert!(
                    hl[i].to_bits() == bl[i].to_bits() && hr[i].to_bits() == br[i].to_bits(),
                    "start {} idx {}: live ({}, {}) != file through head and tail ({}, {})",
                    s, i, bl[i], br[i], hl[i], hr[i]
                );
                assert!(
                    (al[i] - bl[i]).abs() <= 1e-13 * peak && (ar[i] - br[i]).abs() <= 1e-13 * peak,
                    "start {} idx {}: live ({}, {}) far from file ({}, {})",
                    s, i, bl[i], br[i], al[i], ar[i]
                );
            }
            if s == 0 {
                from_start = Some((al, ar));
            }
        }
        // In f32, as a live source once kept it: other numbers — the
        // rounding's, far down, but not the file's.
        let (al, ar) = from_start.expect("read from the start above");
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 1.0, 1e6));
        let f32_kept = |x: &[f64]| -> Vec<f64> { x.iter().map(|&v| v as f32 as f64).collect() };
        live.push(&f32_kept(&x_l), &f32_kept(&x_r));
        live.close();
        let mut b = PolyStream::with_input(bank.clone(), Input::Live(live), 0);
        let (mut bl, mut br) = (vec![0.0; total], vec![0.0; total]);
        b.read(&mut bl, &mut br);
        let (peak, diff) = al.iter().zip(&bl).chain(ar.iter().zip(&br)).fold((0f64, 0f64), |(p, d), (x, y)| (p.max(x.abs()), d.max((x - y).abs())));
        let db = 20.0 * (diff / peak).log10();
        assert!(diff > 0.0 && db < -120.0, "kept in f32: {:.1} dB under the peak", db);
    }

    #[test]
    fn a_live_stream_plays_what_the_file_plays_linear() {
        let taps = 3 * BLOCK * 4 + 1001;
        let mut h: Vec<f64> = noise(taps, 7).iter().map(|v| v * 1e-3).collect();
        for i in 0..taps / 2 {
            h[taps - 1 - i] = h[i];
        }
        check_live(&h, 4, 5 * BLOCK + 999, Alignment::Linear);
    }

    #[test]
    fn a_live_stream_plays_what_the_file_plays_min() {
        let taps = 9_001;
        let h: Vec<f64> = noise(taps, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        check_live(&h, 8, 3 * BLOCK + 17, Alignment::None);
    }

    /// A file whose source stages are still running, read while its
    /// producer pushes it in odd pieces with pauses, gives the file's numbers
    /// to the bit — from the start and from a later position — and ends
    /// where the file ends.
    fn check_grow(coeffs: &[f64], l: usize, n_in: usize, align: Alignment) {
        use super::super::grow::GrowSource;
        let x_l = noise(n_in, 5);
        let x_r = noise(n_in, 6);
        let bank = Arc::new(Bank::from_coeffs("test", coeffs, l, align, 352_800));
        let file = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
        let total = n_in * l;
        for &s in &[0usize, BLOCK * l * 2 + 7] {
            let grow = Arc::new(GrowSource::new(44_100, n_in));
            let w = grow.clone();
            let (wl, wr) = (x_l.clone(), x_r.clone());
            let producer = std::thread::spawn(move || {
                let mut at = 0;
                let mut step = 4_001;
                while at < wl.len() {
                    let e = (at + step).min(wl.len());
                    w.push(&wl[at..e], &wr[at..e]);
                    at = e;
                    step = step * 5 / 4 + 1;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                w.finish();
            });
            let mut a = PolyStream::new(bank.clone(), file.clone(), s as u64);
            let mut b = PolyStream::with_input(bank.clone(), Input::Grow(grow.clone()), s as u64);
            assert_eq!(b.total(), total as u64, "a growing file ends where the file does");
            let want = total - s + 999;
            let (mut al, mut ar) = (vec![0.0; want], vec![0.0; want]);
            let (mut bl, mut br) = (vec![0.0; want], vec![0.0; want]);
            let na = a.read(&mut al, &mut ar);
            let mut nb = 0;
            let mut off = 0;
            let mut step = 6_666;
            while off < want {
                let e = (off + step).min(want);
                nb += b.read(&mut bl[off..e], &mut br[off..e]);
                off = e;
                step = step * 3 / 2 + 1;
            }
            producer.join().unwrap();
            assert_eq!(na, nb, "start {}: the same frames inside the track", s);
            for i in 0..want {
                assert!(
                    al[i].to_bits() == bl[i].to_bits() && ar[i].to_bits() == br[i].to_bits(),
                    "start {} idx {}: growing ({}, {}) != file ({}, {})",
                    s, i, bl[i], br[i], al[i], ar[i]
                );
            }
        }
    }

    #[test]
    fn a_growing_file_plays_what_the_file_plays_linear() {
        let taps = 3 * BLOCK * 4 + 1001;
        let mut h: Vec<f64> = noise(taps, 7).iter().map(|v| v * 1e-3).collect();
        for i in 0..taps / 2 {
            h[taps - 1 - i] = h[i];
        }
        check_grow(&h, 4, 5 * BLOCK + 999, Alignment::Linear);
    }

    #[test]
    fn a_growing_file_plays_what_the_file_plays_min() {
        let taps = 9_001;
        let h: Vec<f64> = noise(taps, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        check_grow(&h, 8, 3 * BLOCK + 17, Alignment::None);
    }

    /// A Hybrid-Phase pair's banks at L = 4: linear (4 partitions per
    /// branch, its output ~1.5 blocks ahead of the other's) and minimum
    /// phase (2 partitions).
    fn pair_banks() -> (Arc<Bank>, Arc<Bank>) {
        let taps = 3 * BLOCK * 4 + 1001;
        let mut lin: Vec<f64> = noise(taps, 7).iter().map(|v| v * 1e-3).collect();
        for i in 0..taps / 2 {
            lin[taps - 1 - i] = lin[i];
        }
        let mt = 2 * BLOCK * 4 - 3;
        let min: Vec<f64> = noise(mt, 9).iter().enumerate().map(|(i, v)| v * 1e-3 * (-(i as f64) / 40_000.0).exp()).collect();
        (
            Arc::new(Bank::from_coeffs("lin", &lin, 4, Alignment::Linear, 176_400)),
            Arc::new(Bank::from_coeffs("min", &min, 4, Alignment::None, 176_400)),
        )
    }

    /// The blocks a pair's two rings hold, counted once.
    fn ring_union(a: &PolyStream, b: &PolyStream) -> usize {
        let mut s = std::collections::BTreeSet::new();
        for st in [a, b] {
            // The newest block in the ring: a head-and-tail stream takes a
            // block in when the next one starts.
            let top = st.ht.as_ref().map_or(st.next_block - 1, |h| h.filled);
            for k in 0..st.depth as i64 {
                s.insert(top - k);
            }
        }
        s.len()
    }

    /// Two pairs over one input, read in turns as `HybridStage` reads (the
    /// linear stream, then the minimum-phase one): the pair that shares its
    /// spectra gives what the pair with its own gives, bit for bit; the
    /// spectra it keeps are its two rings' union, and they go with the pair.
    fn check_pair_share(input: Input, want: usize, start: u64) {
        let (lin, min) = pair_banks();
        let share = SpectrumShare::new();
        let mut a = PolyStream::with_input_shared(lin.clone(), input.clone(), start, Some(share.clone()));
        let mut b = PolyStream::with_input_shared(min.clone(), input.clone(), start, Some(share.clone()));
        let mut c = PolyStream::with_input(lin.clone(), input.clone(), start);
        let mut d = PolyStream::with_input(min.clone(), input, start);
        let mut out: [(Vec<f64>, Vec<f64>); 4] = std::array::from_fn(|_| (vec![0.0; want], vec![0.0; want]));
        let (mut off, mut step) = (0, 5_001);
        let mut checked = false;
        while off < want {
            let e = (off + step).min(want);
            for (st, o) in [&mut a, &mut b, &mut c, &mut d].into_iter().zip(out.iter_mut()) {
                st.read(&mut o.0[off..e], &mut o.1[off..e]);
            }
            if a.next_block > a.depth as i64 && !checked {
                assert_eq!(share.live(), ring_union(&a, &b), "the shared spectra are the rings' union");
                assert!(share.live() < a.depth + b.depth, "the pair holds fewer spectra than two rings");
                checked = true;
            }
            off = e;
            step = step * 3 / 2 + 1;
        }
        assert!(checked, "the rings were checked");
        for (x, y, what) in [(&out[0], &out[2], "linear"), (&out[1], &out[3], "minimum")] {
            for i in 0..want {
                assert!(
                    x.0[i].to_bits() == y.0[i].to_bits() && x.1[i].to_bits() == y.1[i].to_bits(),
                    "{} start {} idx {}: shared ({}, {}) != own ({}, {})",
                    what, start, i, x.0[i], x.1[i], y.0[i], y.1[i]
                );
            }
        }
        drop((a, b));
        assert_eq!(share.live(), 0, "the spectra go with the pair");
    }

    #[test]
    fn a_pair_sharing_its_spectra_plays_what_it_played_file() {
        let n_in = 7 * BLOCK + 333;
        let src = Arc::new(SourceBuf { l: noise(n_in, 31), r: noise(n_in, 32), rate: 44_100 });
        for start in [0u64, (BLOCK * 4 * 3 + 11) as u64] {
            check_pair_share(Input::File(src.clone()), n_in * 4 - start as usize, start);
        }
    }

    #[test]
    fn a_pair_sharing_its_spectra_plays_what_it_played_growing() {
        use super::super::grow::GrowSource;
        let n_in = 7 * BLOCK + 333;
        let (x_l, x_r) = (noise(n_in, 33), noise(n_in, 34));
        let grow = Arc::new(GrowSource::new(44_100, n_in));
        let w = grow.clone();
        let producer = std::thread::spawn(move || {
            let (mut at, mut step) = (0, 3_001);
            while at < x_l.len() {
                let e = (at + step).min(x_l.len());
                w.push(&x_l[at..e], &x_r[at..e]);
                at = e;
                step = step * 5 / 4 + 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            w.finish();
        });
        check_pair_share(Input::Grow(grow), n_in * 4, 0);
        producer.join().unwrap();
    }

    /// A live stream that starves in the middle (the writer stops for longer
    /// than `STARVE_WAIT`): the silence and the fades are written into the
    /// source by index, so the pair that shares its spectra still plays
    /// what the pair with its own plays.
    #[test]
    fn a_pair_sharing_its_spectra_plays_what_it_played_live_with_a_starve() {
        use super::super::radio::live::{LiveSource, STARVE_WAIT};
        let n_in = 7 * BLOCK + 333;
        let x_l = noise(n_in, 35);
        let x_r = noise(n_in, 36);
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 0.2, 1e6));
        let w = live.clone();
        let writer = std::thread::spawn(move || {
            let cut = 3 * BLOCK;
            w.push(&x_l[..cut], &x_r[..cut]);
            // Stopped until the readers ran into the edge and the stream
            // starved, however long they take to get there.
            let t0 = std::time::Instant::now();
            while w.stats().starves == 0 && t0.elapsed() < STARVE_WAIT * 100 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let (mut at, mut step) = (cut, 4_001);
            while at < x_l.len() {
                let e = (at + step).min(x_l.len());
                w.push(&x_l[at..e], &x_r[at..e]);
                at = e;
                step = step * 5 / 4 + 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            w.close();
        });
        check_pair_share(Input::Live(live.clone()), n_in * 4, 0);
        writer.join().unwrap();
        assert!(live.stats().starves >= 1, "the stream starved");
    }

    /// Head and tail against whole blocks on a file: the same convolution,
    /// the output within f64 rounding — from the start, mid-piece, on a
    /// block boundary and later; a filter of several partitions, one shorter
    /// than a block, and one shorter than the head's first level.
    #[test]
    fn head_and_tail_play_what_whole_blocks_play() {
        let taps = 3 * BLOCK * 4 + 1001;
        let mut lin: Vec<f64> = noise(taps, 7).iter().map(|v| v * 1e-3).collect();
        for i in 0..taps / 2 {
            lin[taps - 1 - i] = lin[i];
        }
        let min: Vec<f64> = noise(9_001, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        let short: Vec<f64> = noise(2_001, 11).iter().map(|v| v * 1e-2).collect();
        for (h, l, align) in [(&lin, 4usize, Alignment::Linear), (&min, 8, Alignment::None), (&short, 2, Alignment::None)] {
            let n_in = 5 * BLOCK + 999;
            let src = Arc::new(SourceBuf { l: noise(n_in, 51), r: noise(n_in, 52), rate: 44_100 });
            let bank = Arc::new(Bank::from_coeffs("test", h, l, align, 176_400));
            let total = n_in * l;
            for s in [0usize, 12_345, BLOCK * l, BLOCK * l * 2 + HEAD_CHUNK * l * 3 + 7] {
                let want = total - s;
                let mut a = PolyStream::new(bank.clone(), src.clone(), s as u64);
                let mut b = PolyStream::with_head(bank.clone(), Input::File(src.clone()), s as u64);
                let (mut al, mut ar, mut bl, mut br) = (vec![0.0; want], vec![0.0; want], vec![0.0; want], vec![0.0; want]);
                a.read(&mut al, &mut ar);
                let (mut off, mut step) = (0, 3_333);
                while off < want {
                    let e = (off + step).min(want);
                    b.read(&mut bl[off..e], &mut br[off..e]);
                    off = e;
                    step = step * 3 / 2 + 1;
                }
                let peak = al.iter().chain(&ar).fold(0f64, |p, v| p.max(v.abs()));
                let diff = al.iter().zip(&bl).chain(ar.iter().zip(&br)).fold(0f64, |d, (x, y)| d.max((x - y).abs()));
                println!("L={} start {}: max {:.1} dB under the peak {:.3}", l, s, 20.0 * (diff / peak).max(1e-300).log10(), peak);
                assert!(peak > 0.0 && diff <= 1e-13 * peak, "L={} start {}: head and tail {:e} off at peak {}", l, s, diff, peak);
            }
        }
    }

    /// A live stream's first piece waits only for its own frames: with
    /// HEAD_CHUNK frames in and the network silent, the first HEAD_CHUNK
    /// output frames of every branch come without a starve.
    #[test]
    fn a_live_stream_waits_for_a_piece_not_a_block() {
        use super::super::radio::live::LiveSource;
        let h: Vec<f64> = noise(9_001, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        let l = 4;
        let bank = Arc::new(Bank::from_coeffs("test", &h, l, Alignment::None, 176_400));
        let live = Arc::new(LiveSource::new(44_100, usize::MAX / 4, 1.0, 1e6));
        let x = noise(HEAD_CHUNK, 61);
        live.push(&x, &x);
        let mut st = PolyStream::with_input(bank, Input::Live(live.clone()), 0);
        let (mut ol, mut or) = (vec![0.0; HEAD_CHUNK * l], vec![0.0; HEAD_CHUNK * l]);
        // A wait for frames that never come ends in a starve (the event,
        // not the clock: the run may be slow).
        st.read(&mut ol, &mut or);
        assert_eq!(live.stats().starves, 0, "the first piece waited for nothing more");
        assert!(ol.iter().any(|&v| v != 0.0), "it played the piece");
    }

    /// Several partitions per branch (sub-filter > B), L = 4, linear trim.
    #[test]
    fn matches_converter_bit_for_bit_linear() {
        let taps = 3 * BLOCK * 4 + 1001; // 4 partitions per branch
        let mut h: Vec<f64> = noise(taps, 7).iter().map(|v| v * 1e-3).collect();
        // Symmetric, like the real linear-phase blobs.
        for i in 0..taps / 2 {
            h[taps - 1 - i] = h[i];
        }
        check(&h, 4, 5 * BLOCK + 999, Alignment::Linear);
    }

    /// TFS: a filter whose window starts K ahead of its centre plays on the
    /// stream with the trim the converter gives it (K, not half the filter),
    /// bit for bit.
    #[test]
    fn matches_converter_bit_for_bit_look_ahead() {
        let taps = 3 * BLOCK * 4 + 1001;
        let h: Vec<f64> = noise(taps, 11).iter().map(|v| v * 1e-3).collect();
        check(&h, 4, 5 * BLOCK + 999, Alignment::LookAhead(10_584));
    }

    /// Minimum-phase style (no trim), L = 8, filter shorter than one block.
    #[test]
    fn matches_converter_bit_for_bit_min_short() {
        let taps = 9_001;
        let h: Vec<f64> = noise(taps, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        check(&h, 8, 3 * BLOCK + 17, Alignment::None);
    }

    /// The pre-trial core on a small pair: the start block has a non-zero
    /// window, every block is timed, the pair shares one allocation, and a
    /// bank of another depth or L is refused by `trial_rebind`.
    #[test]
    fn trial_small_bank() {
        let taps = 9_001;
        let h: Vec<f64> = noise(taps, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        let lin = Arc::new(Bank::from_coeffs("lin", &h, 8, Alignment::Linear, 352_800));
        let min = Arc::new(Bank::from_coeffs("min", &h, 8, Alignment::None, 352_800));

        // Silent for three blocks, then noise: 25 % of 8 blocks is block 2,
        // whose window [B, 3B) is silent; block 3's window reaches the noise.
        let n = 8 * BLOCK;
        let mut x_l = noise(n, 1);
        let mut x_r = noise(n, 2);
        for i in 0..3 * BLOCK {
            x_l[i] = 0.0;
            x_r[i] = 0.0;
        }
        let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
        let j = trial_block_index(&src);
        assert_eq!(j, 3);
        let (a, b) = ((j as usize - 1) * BLOCK, (j as usize + 1) * BLOCK);
        assert!(src.l[a..b].iter().any(|&v| v != 0.0), "trial window is silent");

        let never = || false;
        let always = || true;
        let t = trial_blocks(&[lin.clone(), min.clone()], &src, 2, &never, &never);
        assert!(!t.stopped && !t.extra);
        assert_eq!(t.j, 3);
        assert!(t.threads >= 1);
        assert!(t.alloc_ns > 0);
        assert_eq!(t.block_ns.len(), 2);
        for b in &t.block_ns {
            assert_eq!(b.len(), 2);
            assert!(b.iter().all(|&ns| ns > 0), "{:?}", t.block_ns);
        }

        // One block per bank, then one more each while `more()` holds.
        let t = trial_blocks(&[lin.clone(), min.clone()], &src, 1, &always, &never);
        assert!(t.extra);
        assert!(t.block_ns.iter().all(|b| b.len() == 2), "{:?}", t.block_ns);

        // Stopped before the first block.
        let t = trial_blocks(&[lin.clone(), min.clone()], &src, 2, &never, &always);
        assert!(t.stopped);
        assert!(t.block_ns.iter().all(|b| b.is_empty()));

        // A silent or very short source starts at block 1.
        let silent = SourceBuf { l: vec![0.0; 4 * BLOCK], r: vec![0.0; 4 * BLOCK], rate: 44_100 };
        assert_eq!(trial_block_index(&silent), 1);
        let short = SourceBuf { l: vec![0.5; 100], r: vec![0.5; 100], rate: 44_100 };
        assert_eq!(trial_block_index(&short), 1);

        // Rebind: equal depth and L yes; another depth or another L no.
        let mut st = PolyStream::new_trial(lin.clone(), src.clone(), j);
        assert!(st.trial_block_ns() > 0);
        assert!(st.trial_rebind(min.clone(), j).is_ok());
        assert!(st.trial_block_ns() > 0);
        let long: Vec<f64> = noise(8 * BLOCK + 101, 5).iter().map(|v| v * 1e-3).collect();
        let deep = Arc::new(Bank::from_coeffs("deep", &long, 8, Alignment::None, 352_800));
        assert_eq!(deep.max_partitions(), 2);
        assert!(st.trial_rebind(deep, j).is_err());
        let four = Arc::new(Bank::from_coeffs("four", &h, 4, Alignment::None, 352_800));
        assert_eq!(four.max_partitions(), lin.max_partitions());
        assert!(st.trial_rebind(four, j).is_err());
    }

    /// The block trace (case C, the benches): once enabled, one event per
    /// computed block with its index, its bank's trim, the device and a
    /// start after the trace's origin.
    #[cfg(feature = "gpu-player-tests")]
    #[test]
    fn trace_records_trial_blocks() {
        let taps = 9_001;
        let h: Vec<f64> = noise(taps, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        let lin = Arc::new(Bank::from_coeffs("lin", &h, 8, Alignment::Linear, 352_800));
        let src = Arc::new(SourceBuf { l: noise(8 * BLOCK, 1), r: noise(8 * BLOCK, 2), rate: 44_100 });
        let never = || false;
        trace::enable(true);
        let origin = trace::t0();
        let t = trial_blocks(&[lin.clone()], &src, 3, &never, &never);
        trace::enable(false);
        let span = origin.elapsed().as_nanos() as u64;
        // Other tests may compute blocks meanwhile: keep this bank's.
        let mine: Vec<trace::Ev> = trace::drain()
            .into_iter()
            .filter(|e| !e.gpu && e.delay == lin.delay && e.j >= t.j && e.j < t.j + 3)
            .collect();
        assert!(mine.len() >= 3, "{} events for the trial's 3 blocks", mine.len());
        assert!(mine.iter().all(|e| e.dur_ns > 0 && e.start_ns <= span), "{mine:?}");
    }

    /// The trial's page touch stores zeros only, at any length (a last
    /// partial page too), so a trial stream stays unprimed.
    #[test]
    fn touch_pages_keeps_zeros() {
        for n in [0usize, 1, 255, 256, 257, 4096 + 3] {
            let mut v = vec![C::new(0.0, 0.0); n];
            touch_pages(&mut v);
            assert!(v.iter().all(|c| c.re == 0.0 && c.im == 0.0), "length {n}");
        }
    }

    /// The design of a blob of rate `rate` as `optimize.py --all-ratios` makes
    /// it, short: sinc × Kaiser β = 14, the cutoff in the middle of the last
    /// 2 kHz below its 44.1/48 kHz source's Nyquist.
    fn designed_at(rate: u32, taps: usize) -> Vec<f64> {
        let base = if rate % 44_100 == 0 { 44_100.0 } else { 48_000.0 };
        let fc = (base / 2.0 - 1_000.0) / rate as f64;
        let w = crate::player::analytics::spectra::kaiser(taps, 14.0);
        let mid = (taps - 1) as f64 / 2.0;
        (0..taps)
            .map(|i| {
                let x = i as f64 - mid;
                let s = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
                s * w[i]
            })
            .collect()
    }

    /// A 96 kHz source keeps what it has above 23 kHz. It takes the blob of
    /// its own ×4 factor, whose wall stands at 46 kHz where it plays; the
    /// output rate's blob it used to take is a 48 kHz source's, walled at
    /// 23 kHz, and a 30 kHz tone did not come through it.
    #[test]
    fn a_hi_res_source_keeps_what_it_has_above_the_cd_wall() {
        use crate::audio::converter::dsp::filter::design_rate;
        let (src, out) = (96_000u32, 384_000u32);
        let l = (out / src) as usize;
        let n = 4 * BLOCK;
        let amp = 0.1;
        let tone: Vec<f64> = (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * 30_000.0 * i as f64 / src as f64).sin())
            .collect();
        let source = Arc::new(SourceBuf { l: tone.clone(), r: tone, rate: src });
        // The tone's level through the blob designed at `rate`, dB re the
        // input, over the steady middle of the output.
        let level = |rate: u32| -> f64 {
            let bank = Arc::new(Bank::from_coeffs("test", &designed_at(rate, 8_191), l, Alignment::Linear, out));
            let mut st = PolyStream::new(bank, source.clone(), 0);
            let total = n * l;
            let (mut ol, mut or) = (vec![0.0; total], vec![0.0; total]);
            st.read(&mut ol, &mut or);
            let mid = &ol[total / 4..3 * total / 4];
            let rms = (mid.iter().map(|x| x * x).sum::<f64>() / mid.len() as f64).sqrt();
            20.0 * (rms / (amp / 2f64.sqrt())).log10()
        };
        assert_eq!(design_rate(src, out), 192_000);
        let now = level(design_rate(src, out));
        let before = level(out);
        println!("30 kHz of 96 kHz at ×4: {now:.6} dB through the ×4 blob, {before:.1} dB through the ×8 one");
        assert!(now.abs() < 0.01, "the 30 kHz tone of a 96 kHz source came out at {now:.4} dB");
        assert!(before < -100.0, "the output rate's blob passed 30 kHz at {before:.1} dB");
    }

    /// Mean square of `x` in each band, dB re a full-scale sine: one
    /// transform of the whole span under one Hann window, so two rates of the
    /// same stretch of time are weighed alike.
    fn band_levels(x: &[f64], rate: u32, bands: &[(f64, f64)]) -> Vec<f64> {
        let n = x.len();
        let w = |i: usize| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
        let w2: f64 = (0..n).map(|i| w(i) * w(i)).sum();
        let mut buf: Vec<C> = (0..n).map(|i| C::new(x[i] * w(i), 0.0)).collect();
        FftPlanner::<f64>::new().plan_fft_forward(n).process(&mut buf);
        let bin = rate as f64 / n as f64;
        bands
            .iter()
            .map(|&(lo, hi)| {
                let k0 = (lo / bin).ceil() as usize;
                let k1 = ((hi / bin).floor() as usize).min(n / 2);
                let one_side: f64 = buf[k0..=k1].iter().map(|c| c.norm_sqr()).sum();
                let ms = 2.0 * one_side / (n as f64 * w2);
                10.0 * (ms / 0.5).max(1e-30).log10()
            })
            .collect()
    }

    /// The measure for the report: 30 s of a hi-res file through the 1M
    /// filter at FS×8, through the blob of its factor (now) and through the
    /// output rate's (before), and what each leaves above 23 kHz.
    /// `AURA_HIRES_FILE` names the file (only read), `AURA_FILTER_DIR` the
    /// blobs. `cargo test --profile fast --bins hi_res_measure -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hi_res_measure_above_the_cd_wall() {
        use crate::audio::converter::dsp::filter::{design_rate, find_blob};
        let Some(path) = std::env::var_os("AURA_HIRES_FILE") else {
            println!("hi_res_measure: set AURA_HIRES_FILE");
            return;
        };
        let a = crate::audio::converter::decode::decode_file(std::path::Path::new(&path)).expect("decode");
        let src = a.sample_rate;
        let (out, _) = crate::player::radio::chain::out_rate_for(src, 8).expect("a family rate");
        let l = (out / src) as usize;
        // 30 s from the middle, its ends faded over half a second: a cut
        // would spread a click across every band.
        let len = (30 * src as usize).min(a.samples_l.len());
        let from = (a.samples_l.len() - len) / 2;
        let fade = src as usize / 2;
        let take = |ch: &[f64]| -> Vec<f64> {
            (0..len)
                .map(|i| {
                    let e = i.min(len - 1 - i);
                    let g = if e < fade { 0.5 - 0.5 * (std::f64::consts::PI * e as f64 / fade as f64).cos() } else { 1.0 };
                    ch[from + i] * g
                })
                .collect()
        };
        let source = Arc::new(SourceBuf { l: take(&a.samples_l), r: take(&a.samples_r), rate: src });
        let nyq = src as f64 / 2.0;
        let bands: Vec<(f64, f64)> = [(20_000.0, 23_000.0), (23_000.0, 30_000.0), (30_000.0, 40_000.0), (40_000.0, 46_000.0)]
            .into_iter()
            .filter(|&(lo, _)| lo < nyq)
            .collect();
        // The steady middle: two seconds off each end.
        let mid = |x: &[f64], rate: u32| -> Vec<f64> {
            let cut = 2 * rate as usize;
            band_levels(&x[cut..x.len() - cut], rate, &bands)
        };
        let r2 = |v: &[f64]| v.iter().map(|x| (x * 100.0).round() / 100.0).collect::<Vec<_>>();
        println!("hi_res_measure: {} Hz → {} Hz (×{}), 30 s, left channel", src, out, l);
        println!("  bands (Hz): {:?}", bands);
        println!("  source        : {:?}", r2(&mid(&source.l, src)));
        for (name, rate) in [("now  (×4 blob)", design_rate(src, out)), ("before (×8 blob)", out)] {
            let p = find_blob(1_000_000, rate, "linear_phase").expect("the 1M blob (AURA_FILTER_DIR)");
            let bank = Arc::new(Bank::load(&p, l, Alignment::Linear, out).expect("bank"));
            let mut st = PolyStream::new(bank, source.clone(), 0);
            let n = source.len() * l;
            let (mut ol, mut or) = (vec![0.0; n], vec![0.0; n]);
            st.read(&mut ol, &mut or);
            let got = mid(&ol, out);
            println!("  {name}: {:?}  ({})", r2(&got), p);
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// Head and tail written once and compared after a change to its code:
    /// the bits must not move. Synthetic filters (lin L = 8 of several
    /// partitions, min L = 4), a file from two starts and a Hybrid-Phase
    /// pair sharing its spectra. AURA_HERM_HEAD_DUMP=write <file> or check
    /// <file>.
    ///     cargo test --profile fast convolver::bench::head_tail_dump -- --ignored --nocapture
    #[test]
    #[ignore]
    fn head_tail_dump() {
        let Ok(arg) = std::env::var("AURA_HERM_HEAD_DUMP") else { println!("[dump] AURA_HERM_HEAD_DUMP not set - skip"); return; };
        let (mode, file) = arg.split_once(' ').expect("write|check <file>");
        let noise = |n: usize, seed: u64| -> Vec<f64> {
            use rand::{Rng, SeedableRng};
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            (0..n).map(|_| rng.gen_range(-0.5..0.5)).collect()
        };
        let taps = 5 * BLOCK * 8 + 777;
        let mut lin = noise(taps, 1);
        for i in 0..taps / 2 {
            lin[taps - 1 - i] = lin[i];
        }
        let lin: Vec<f64> = lin.iter().map(|v| v * 1e-3).collect();
        let min: Vec<f64> = noise(3 * BLOCK * 4, 2).iter().enumerate().map(|(i, v)| v * 1e-3 * (-(i as f64) / 60_000.0).exp()).collect();
        let n_in = 9 * BLOCK + 555;
        let src = Arc::new(SourceBuf { l: noise(n_in, 3), r: noise(n_in, 4), rate: 44_100 });
        let mut bits: Vec<u8> = Vec::new();
        let mut take = |st: &mut PolyStream, n: usize| {
            let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
            let (mut off, mut step) = (0, 2_999);
            while off < n {
                let e = (off + step).min(n);
                st.read(&mut a[off..e], &mut b[off..e]);
                off = e;
                step = step * 3 / 2 + 1;
            }
            for v in a.iter().chain(&b) {
                bits.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        };
        let blin = Arc::new(Bank::from_coeffs("lin", &lin, 8, Alignment::Linear, 352_800));
        let bmin4 = Arc::new(Bank::from_coeffs("min", &min, 4, Alignment::None, 176_400));
        for s in [0u64, (BLOCK * 8 * 2 + 4096 * 8 * 5 + 3) as u64] {
            let mut st = PolyStream::with_head(blin.clone(), Input::File(src.clone()), s);
            take(&mut st, BLOCK * 8 * 3);
        }
        let mut st = PolyStream::with_head(bmin4.clone(), Input::File(src.clone()), 0);
        take(&mut st, BLOCK * 4 * 4);
        // A pair sharing its spectra (the head's windows too, when shared).
        let lin4 = Arc::new(Bank::from_coeffs("lin4", &lin, 4, Alignment::Linear, 176_400));
        let minb = Arc::new(Bank::from_coeffs("minb", &min, 4, Alignment::BandWeighted, 176_400));
        let share = SpectrumShare::new();
        let mut a = PolyStream::build(lin4, Input::File(src.clone()), 0, Some(share.clone()), true);
        let mut b = PolyStream::build(minb, Input::File(src.clone()), 0, Some(share), true);
        for _ in 0..12 {
            take(&mut a, 4096 * 4 + 17);
            take(&mut b, 4096 * 4 + 17);
        }
        match mode {
            "write" => {
                std::fs::write(file, &bits).unwrap();
                println!("[dump] wrote {} bytes to {}", bits.len(), file);
            }
            _ => {
                let old = std::fs::read(file).unwrap();
                let first = old.iter().zip(&bits).position(|(x, y)| x != y);
                assert!(old.len() == bits.len() && first.is_none(), "[dump] head and tail moved at byte {:?}", first);
                println!("[dump] {} bytes bit for bit with {}", bits.len(), file);
            }
        }
    }

    /// A live stream's head and tail on real filters: the time of 10 s of
    /// output against whole blocks (the price of the short wait), and how far
    /// the output moves.
    ///     cargo test --profile fast convolver::bench::head_tail_real_blobs -- --ignored --nocapture
    #[test]
    #[ignore]
    fn head_tail_real_blobs() {
        let dir = std::env::var("AURA_FILTER_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../fir-optimizer/output").to_string());
        let n = 44_100 * 240;
        let x: Vec<f64> = (0..n).map(|i| ((i as f64) * 0.013).sin() * 0.25 + ((i as f64) * 0.29).sin() * 0.1).collect();
        let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });
        for (tag, l, rate, phase, align) in [
            ("1M", 2usize, 88_200u32, "linear_phase", Alignment::Linear),
            ("1M", 8, 352_800, "linear_phase", Alignment::Linear),
            ("30M", 2, 88_200, "linear_phase", Alignment::Linear),
            ("30M", 8, 352_800, "linear_phase", Alignment::Linear),
            ("30M", 8, 352_800, "minimum_phase", Alignment::BandWeighted),
        ] {
            let path = format!("{}/fir_{}_{}_{}.npy", dir, tag, rate, phase);
            let Ok(bank) = Bank::load(&path, l, align, rate) else {
                println!("skip {}", path);
                continue;
            };
            let bank = Arc::new(bank);
            let start = rate as u64 * 30;
            let want = rate as usize * 10;
            let run = |head: bool| {
                let mut st = if head {
                    PolyStream::with_head(bank.clone(), Input::File(src.clone()), start)
                } else {
                    PolyStream::new(bank.clone(), src.clone(), start)
                };
                let (mut ol, mut or) = (vec![0.0; want], vec![0.0; want]);
                let t = Instant::now();
                for (a, b) in ol.chunks_mut(4096).zip(or.chunks_mut(4096)) {
                    st.read(a, b);
                }
                (t.elapsed().as_secs_f64(), ol, or)
            };
            // The fastest of three runs each (the machine's other load).
            let best = |head: bool| {
                let (mut t, a, b) = run(head);
                for _ in 0..2 {
                    t = t.min(run(head).0);
                }
                (t, a, b)
            };
            let (t_whole, wl, wr) = best(false);
            let (t_head, hl, hr) = best(true);
            let peak = wl.iter().chain(&wr).fold(0f64, |p, v| p.max(v.abs()));
            let diff = wl.iter().zip(&hl).chain(wr.iter().zip(&hr)).fold(0f64, |d, (a, b)| d.max((a - b).abs()));
            println!(
                "{:>3} x{} {:<13} P {:>3} | 10 s: whole {:.3} s (RTF {:.0}x), head+tail {:.3} s (RTF {:.0}x), x{:.2} | diff max {:.1} dBFS ({:.1} dB under the peak)",
                tag, l, phase, bank.max_partitions(), t_whole, 10.0 / t_whole, t_head, 10.0 / t_head, t_head / t_whole,
                20.0 * diff.max(1e-300).log10(), 20.0 * (diff / peak).max(1e-300).log10()
            );
            assert!(diff <= 1e-13 * peak, "{} x{} {}: {:e}", tag, l, phase, diff);
        }
    }

    /// A real Hybrid-Phase pair (30M at ×2 and ×8): the input spectra it
    /// holds with them shared against two rings, and the time of a pair's
    /// block both ways; the outputs are compared bit for bit.
    ///     cargo test --profile fast convolver::bench::pair_share_real_blobs -- --ignored --nocapture
    #[test]
    #[ignore]
    fn pair_share_real_blobs() {
        let dir = std::env::var("AURA_FILTER_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../fir-optimizer/output").to_string());
        let n = 44_100 * 150;
        let x: Vec<f64> = (0..n).map(|i| ((i as f64) * 0.013).sin() * 0.25 + ((i as f64) * 0.29).sin() * 0.1).collect();
        let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });
        let slot = 2 * NBIN * std::mem::size_of::<C>();
        for (l, rate) in [(2usize, 88_200u32), (8, 352_800)] {
            let lin_p = format!("{}/fir_30M_{}_linear_phase.npy", dir, rate);
            let min_p = format!("{}/fir_30M_{}_minimum_phase.npy", dir, rate);
            let (Ok(lin), Ok(min)) = (Bank::load(&lin_p, l, Alignment::Linear, rate), Bank::load(&min_p, l, Alignment::BandWeighted, rate)) else {
                println!("skip 30M x{}", l);
                continue;
            };
            let (lin, min) = (Arc::new(lin), Arc::new(min));
            let start = rate as u64 * 60;
            let blocks = 6usize;
            let want = BLOCK * l * blocks;
            let mut outs = Vec::new();
            for shared in [false, true] {
                let share = shared.then(SpectrumShare::new);
                let t = Instant::now();
                let mut a = PolyStream::with_input_shared(lin.clone(), Input::File(src.clone()), start, share.clone());
                let mut b = PolyStream::with_input_shared(min.clone(), Input::File(src.clone()), start, share.clone());
                let t_prime = t.elapsed().as_secs_f64();
                let (mut al, mut ar, mut bl, mut br) = (vec![0.0; want], vec![0.0; want], vec![0.0; want], vec![0.0; want]);
                let t = Instant::now();
                for k in 0..blocks {
                    let r = k * BLOCK * l..(k + 1) * BLOCK * l;
                    a.read(&mut al[r.clone()], &mut ar[r.clone()]);
                    b.read(&mut bl[r.clone()], &mut br[r]);
                }
                let t_block = t.elapsed().as_secs_f64() / blocks as f64;
                let held = match &share {
                    Some(s) => s.live(),
                    None => a.depth + b.depth,
                };
                println!(
                    "30M x{} {:<6}: P {} + {}, distance {} blocks | spectra held {} = {} MiB | prime {:.3} s | pair block {:.1} ms",
                    l, if shared { "shared" } else { "own" }, a.depth, b.depth, a.next_block - b.next_block,
                    held, held * slot >> 20, t_prime, t_block * 1e3
                );
                outs.push((al, ar, bl, br));
            }
            let (o, s) = (&outs[0], &outs[1]);
            let same = |x: &[f64], y: &[f64]| x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits());
            assert!(same(&o.0, &s.0) && same(&o.1, &s.1) && same(&o.2, &s.2) && same(&o.3, &s.3), "30M x{}: shared != own", l);
        }
    }

    /// Real filters from the converter's matrix: load, prime, sustained speed.
    ///     cargo test --profile fast convolver::bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_blobs_speed() {
        let dir = std::env::var("AURA_FILTER_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../fir-optimizer/output").to_string());
        let secs = 90usize;
        let n = 44_100 * secs;
        let x: Vec<f64> = (0..n).map(|i| ((i as f64) * 0.013).sin() * 0.5).collect();
        let src = Arc::new(SourceBuf { l: x.clone(), r: x, rate: 44_100 });
        for (tag, align) in [("1M", Alignment::Linear), ("10M", Alignment::Linear), ("30M", Alignment::Linear), ("30M", Alignment::BandWeighted)] {
            let phase = if matches!(align, Alignment::Linear) { "linear_phase" } else { "minimum_phase" };
            let path = format!("{}/fir_{}_352800_{}.npy", dir, tag, phase);
            if !std::path::Path::new(&path).exists() {
                println!("skip {}", path);
                continue;
            }
            let t0 = Instant::now();
            let bank = Arc::new(Bank::load(&path, 8, align, 352_800).unwrap());
            let t_load = t0.elapsed().as_secs_f64();
            let start = 352_800u64 * 30;
            let t1 = Instant::now();
            let mut st = PolyStream::new(bank.clone(), src.clone(), start);
            let t_prime = t1.elapsed().as_secs_f64();
            let mut ol = vec![0.0; 4096];
            let mut or = vec![0.0; 4096];
            let t2 = Instant::now();
            st.read(&mut ol, &mut or);
            let t_first = t2.elapsed().as_secs_f64();
            let want = 352_800 * 10;
            let t3 = Instant::now();
            let mut got = 4096;
            while got < want {
                st.read(&mut ol, &mut or);
                got += 4096;
            }
            let t_run = t3.elapsed().as_secs_f64();
            println!(
                "{:>4} {:<13} depth {:>3} spectra {:>5} MB | load {:>5.2}s | prime {:>5.3}s | first sample {:>5.3}s | 10 s audio in {:>5.2}s → RTF {:>5.1}× | D={}",
                tag, phase, bank.max_partitions(), bank.bytes() / (1 << 20), t_load, t_prime, t_prime + t_first, t_run, 10.0 / t_run, bank.delay
            );
        }
    }
}

/// The half-spectrum arithmetic against the full complex one it replaced:
/// the same partitions, the same Kahan order, the same scalings, with every
/// spectrum kept at all `NFFT` bins and the complex inverse. The difference
/// is rounding of f64 (the real transform is not the complex one bit for
/// bit), so it is held to 1e-13 of the peak (−260 dBFS for a full-scale
/// signal); the measured values are printed.
#[cfg(test)]
mod half_check {
    use super::*;
    use rustfft::{Fft, FftPlanner};
    use std::time::Instant;

    /// The previous `PolyStream` over a file, as it was: full spectra,
    /// complex FFT and inverse. Its parallel layout is kept too, so its
    /// block time is a fair "before".
    struct FullStream {
        l: usize,
        scale: f64,
        spectra: Vec<Vec<Vec<C>>>,
        fft: Arc<dyn Fft<f64>>,
        ifft: Arc<dyn Fft<f64>>,
        src: Arc<SourceBuf>,
        depth: usize,
        fdl: [Vec<Vec<C>>; 2],
        acc: Vec<Vec<C>>,
        comp: Vec<Vec<C>>,
    }

    impl FullStream {
        fn new(coeffs: &[f64], bank: &Bank, src: Arc<SourceBuf>) -> FullStream {
            let l = bank.l;
            let branches = crate::audio::converter::dsp::polyphase::polyphase_decompose(coeffs, l);
            let mut planner = FftPlanner::<f64>::new();
            let fft = planner.plan_fft_forward(NFFT);
            let ifft = planner.plan_fft_inverse(NFFT);
            // All (branch, partition) jobs in one parallel pass, as the bank was built.
            let jobs: Vec<(usize, usize)> = branches
                .iter()
                .enumerate()
                .flat_map(|(p, sub)| (0..partitions(sub.len())).map(move |b| (p, b)))
                .collect();
            let built: Vec<((usize, usize), Vec<C>)> = jobs
                .into_par_iter()
                .map(|(p, b)| {
                    let sub = &branches[p];
                    let mut block = vec![C::new(0.0, 0.0); NFFT];
                    for i in 0..BLOCK {
                        if b * BLOCK + i < sub.len() {
                            block[i] = C::new(sub[b * BLOCK + i], 0.0);
                        }
                    }
                    fft.process(&mut block);
                    ((p, b), block)
                })
                .collect();
            let mut spectra: Vec<Vec<Vec<C>>> = branches.iter().map(|_| Vec::new()).collect();
            for ((p, _), block) in built {
                spectra[p].push(block);
            }
            let depth = spectra.iter().map(|s| s.len()).max().unwrap_or(1).max(1);
            let zeros = |n: usize| -> Vec<Vec<C>> { (0..n).map(|_| vec![C::new(0.0, 0.0); NFFT]).collect() };
            FullStream {
                l,
                scale: bank.scale,
                spectra,
                fft,
                ifft,
                src,
                depth,
                fdl: [zeros(depth), zeros(depth)],
                acc: zeros(2 * l),
                comp: zeros(2 * l),
            }
        }

        fn spectrum(&self, ch: usize, j: i64) -> Vec<C> {
            let x = if ch == 0 { &self.src.l } else { &self.src.r };
            let mut buf = vec![C::new(0.0, 0.0); NFFT];
            let base = (j - 1) * BLOCK as i64;
            let mut any = false;
            for i in 0..NFFT {
                let v = SourceBuf::at(x, base + i as i64);
                any |= v != 0.0;
                buf[i] = C::new(v, 0.0);
            }
            if any {
                self.fft.process(&mut buf);
            }
            buf
        }

        /// History of block `j0`, as `prime`.
        fn prime(&mut self, j0: i64) {
            let depth = self.depth as i64;
            for j in j0 - (depth - 1)..j0 {
                let slot = j.rem_euclid(depth) as usize;
                self.fdl[0][slot] = self.spectrum(0, j);
                self.fdl[1][slot] = self.spectrum(1, j);
            }
        }

        /// Block `j` in output order (`B·L` samples from output index
        /// `j·B·L − D`), as `compute_block`.
        fn block(&mut self, j: i64) -> (Vec<f64>, Vec<f64>) {
            let depth = self.depth as i64;
            let slot = j.rem_euclid(depth) as usize;
            let (sl, sr) = rayon::join(|| self.spectrum(0, j), || self.spectrum(1, j));
            self.fdl[0][slot] = sl;
            self.fdl[1][slot] = sr;
            let spectra = &self.spectra;
            let fdl = &self.fdl;
            let mut items: Vec<(usize, usize, usize, &mut [C], &mut [C])> = Vec::new();
            for (idx, (acc, comp)) in self.acc.iter_mut().zip(self.comp.iter_mut()).enumerate() {
                for ((a, c), n) in acc.chunks_mut(BIN_CHUNK).zip(comp.chunks_mut(BIN_CHUNK)).zip(0..) {
                    items.push((idx / 2, idx % 2, n * BIN_CHUNK, a, c));
                }
            }
            items.into_par_iter().for_each(|(p, ch, off, acc, comp)| {
                acc.fill(C::new(0.0, 0.0));
                comp.fill(C::new(0.0, 0.0));
                let len = acc.len();
                for (k, h_full) in spectra[p].iter().enumerate() {
                    let hist = (j - k as i64).rem_euclid(depth) as usize;
                    let h = &h_full[off..off + len];
                    let d = &fdl[ch][hist][off..off + len];
                    for i in 0..len {
                        let prod = d[i] * h[i];
                        let y = prod - comp[i];
                        let t = acc[i] + y;
                        comp[i] = (t - acc[i]) - y;
                        acc[i] = t;
                    }
                }
            });
            let ifft = &self.ifft;
            self.acc.par_iter_mut().for_each(|a| ifft.process(a));
            let inv_n = 1.0 / NFFT as f64;
            let (mut ol, mut or) = (Vec::with_capacity(BLOCK * self.l), Vec::with_capacity(BLOCK * self.l));
            for t in 0..BLOCK {
                for p in 0..self.l {
                    ol.push(self.acc[2 * p][t + BLOCK].re * inv_n * self.scale);
                    or.push(self.acc[2 * p + 1][t + BLOCK].re * inv_n * self.scale);
                }
            }
            (ol, or)
        }
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
        (0..n).map(|_| rng.gen_range(-0.9..0.9)).collect()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-300).log10()
    }

    /// `blocks` blocks from source block `j0`: new against old. Returns
    /// (max |diff|, RMS diff, peak of the old output, time per block new,
    /// time per block old) — the times in seconds.
    fn compare(coeffs: &[f64], bank: Arc<Bank>, src: Arc<SourceBuf>, j0: i64, blocks: usize) -> (f64, f64, f64, f64, f64) {
        let l = bank.l;
        let d = bank.delay as i64;
        // From `j0` blocks after the first whose output starts inside the track.
        let j0 = j0 + (d + (BLOCK * l) as i64 - 1) / (BLOCK * l) as i64;
        let mut old = FullStream::new(coeffs, &bank, src.clone());
        old.prime(j0);
        // The new stream starts where block j0's output starts.
        let m0 = j0 * (BLOCK * l) as i64 - d;
        let start = m0.max(0) as u64;
        let skip = (start as i64 - m0) as usize;
        let mut st = PolyStream::new(bank.clone(), src, start);
        assert_eq!(st.next_block, j0, "the stream starts at block j0");
        let (mut max, mut sum, mut n, mut peak) = (0.0f64, 0.0f64, 0usize, 0.0f64);
        let (mut t_new, mut t_old) = (0.0, 0.0);
        for b in 0..blocks {
            let t = Instant::now();
            let (ol, or) = old.block(j0 + b as i64);
            t_old += t.elapsed().as_secs_f64();
            let from = if b == 0 { skip } else { 0 };
            let want = ol.len() - from;
            let (mut nl, mut nr) = (vec![0.0; want], vec![0.0; want]);
            let t = Instant::now();
            let inside = st.read(&mut nl, &mut nr);
            t_new += t.elapsed().as_secs_f64();
            // Past the end of the track the stream gives silence.
            for i in 0..inside {
                for (a, o) in [(nl[i], ol[from + i]), (nr[i], or[from + i])] {
                    let e = (a - o).abs();
                    max = max.max(e);
                    sum += e * e;
                    n += 1;
                    peak = peak.max(o.abs());
                }
            }
        }
        (max, (sum / n as f64).sqrt(), peak, t_new / blocks as f64, t_old / blocks as f64)
    }

    /// Bank spectra: the half, mirrored, against the full complex FFT of the
    /// same partition (the response and the stopband come from these).
    fn bank_spectra_match(coeffs: &[f64], bank: &Bank) -> f64 {
        let old = FullStream::new(coeffs, bank, Arc::new(SourceBuf { l: vec![], r: vec![], rate: 44_100 }));
        let mut worst = 0.0f64;
        for p in 0..bank.l {
            let half = bank.branch_spectra(p);
            assert_eq!(half.len(), old.spectra[p].len());
            for (h, f) in half.iter().zip(&old.spectra[p]) {
                assert_eq!(h.len(), NBIN);
                let full = full_from_half(h);
                let peak = f.iter().map(|v| v.norm()).fold(0.0, f64::max).max(1e-300);
                for (a, b) in full.iter().zip(f) {
                    worst = worst.max((a - b).norm() / peak);
                }
            }
        }
        worst
    }

    #[test]
    fn half_spectra_match_the_full_arithmetic() {
        let lin_taps = 3 * BLOCK * 4 + 1001;
        let mut lin: Vec<f64> = noise(lin_taps, 7).iter().map(|v| v * 1e-3).collect();
        for i in 0..lin_taps / 2 {
            lin[lin_taps - 1 - i] = lin[i];
        }
        let min: Vec<f64> = noise(9_001, 9).iter().enumerate().map(|(i, v)| v * (-(i as f64) / 900.0).exp()).collect();
        for (h, l, align) in [(&lin, 4usize, Alignment::Linear), (&min, 8, Alignment::None)] {
            let n_in = 6 * BLOCK + 999;
            let src = Arc::new(SourceBuf { l: noise(n_in, 1), r: noise(n_in, 2), rate: 44_100 });
            let bank = Arc::new(Bank::from_coeffs("test", h, l, align, 352_800));
            let spec = bank_spectra_match(h, &bank);
            assert!(spec <= 1e-13, "bank spectra differ by {:e} of the peak", spec);
            let full_bytes = bank.bytes() / NBIN * NFFT;
            assert_eq!(bank.bytes() * NFFT, full_bytes * NBIN, "the bank keeps NBIN bins per partition");
            for j0 in [0i64, 2] {
                let (max, rms, peak, _, _) = compare(h, bank.clone(), src.clone(), j0, 4);
                println!(
                    "L={} j0={}: max {:.1} dBFS, RMS {:.1} dBFS (peak {:.3}); bank spectra {:.1} dB",
                    l, j0, db(max), db(rms), peak, db(spec)
                );
                assert!(max <= 1e-13 * peak.max(1.0), "max diff {:e} at peak {}", max, peak);
            }
        }
    }

    /// The inverse never sees a non-zero imaginary part where a real
    /// signal has none: an accumulated half spectrum, sent straight to the
    /// transform without `c2r`'s clearing, is taken without an error.
    #[test]
    fn the_inverse_takes_an_accumulated_spectrum() {
        let taps = 3 * BLOCK + 77;
        let h: Vec<f64> = noise(taps, 11).iter().map(|v| v * 1e-3).collect();
        let n_in = 4 * BLOCK;
        let src = Arc::new(SourceBuf { l: noise(n_in, 12), r: noise(n_in, 13), rate: 44_100 });
        let bank = Arc::new(Bank::from_coeffs("test", &h, 2, Alignment::None, 88_200));
        let mut st = PolyStream::new(bank.clone(), src, 0);
        st.compute_block();
        st.compute_block();
        // The block above was inverted by `c2r`; rebuild one accumulator the
        // same way and check its edge bins, then invert it unaided.
        let j = st.next_block - 1;
        let depth = st.depth as i64;
        let mut acc = vec![C::new(0.0, 0.0); NBIN];
        let mut comp = vec![C::new(0.0, 0.0); NBIN];
        for (k, hk) in bank.branch_spectra(0).iter().enumerate() {
            let d = &st.fdl[(j - k as i64).rem_euclid(depth) as usize][0];
            for i in 0..NBIN {
                let prod = d[i] * hk[i];
                let y = prod - comp[i];
                let t = acc[i] + y;
                comp[i] = (t - acc[i]) - y;
                acc[i] = t;
            }
        }
        assert_eq!(acc[0].im, 0.0, "bin 0 stays real");
        assert_eq!(acc[NBIN - 1].im, 0.0, "bin N/2 stays real");
        let mut out = vec![0.0; NFFT];
        assert!(bank.ifft.process(&mut acc, &mut out).is_ok(), "the inverse refused the spectrum");
        for t in 0..BLOCK {
            let want = st.time[0][t + BLOCK];
            assert_eq!(out[t + BLOCK].to_bits(), want.to_bits(), "sample {}", t);
        }
    }

    /// Real filters: new against old on blocks well inside the track, the
    /// bank build, the block time and the memory, before and after.
    ///     cargo test --profile fast half_check -- --ignored --nocapture
    #[test]
    #[ignore]
    fn half_spectra_real_blobs() {
        let dir = std::env::var("AURA_FILTER_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../fir-optimizer/output").to_string());
        let secs = 150usize;
        let n = 44_100 * secs;
        // Music-like: two tones and noise, −6 dBFS.
        let nz = noise(n, 21);
        let x_l: Vec<f64> = (0..n).map(|i| 0.25 * ((i as f64) * 0.013).sin() + 0.1 * ((i as f64) * 0.31).sin() + 0.1 * nz[i]).collect();
        let x_r: Vec<f64> = (0..n).map(|i| 0.25 * ((i as f64) * 0.017).sin() + 0.1 * nz[n - 1 - i]).collect();
        let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
        for (tag, align) in [
            ("1M", Alignment::Linear),
            ("1M", Alignment::BandWeighted),
            ("30M", Alignment::Linear),
            ("30M", Alignment::BandWeighted),
        ] {
            let phase = if matches!(align, Alignment::Linear) { "linear_phase" } else { "minimum_phase" };
            let path = format!("{}/fir_{}_352800_{}.npy", dir, tag, phase);
            if !std::path::Path::new(&path).exists() {
                println!("skip {}", path);
                continue;
            }
            let coeffs = crate::audio::dsp_core::load_npy_f64(&path).unwrap();
            // Build times with the linear trim (its delay costs nothing), so
            // both are the polyphase split and the transforms.
            let t = Instant::now();
            drop(Bank::from_coeffs(&path, &coeffs, 8, Alignment::Linear, 352_800));
            let t_new = t.elapsed().as_secs_f64();
            let bank = Arc::new(Bank::from_coeffs(&path, &coeffs, 8, align, 352_800));
            let t = Instant::now();
            let old = FullStream::new(&coeffs, &bank, src.clone());
            let t_old = t.elapsed().as_secs_f64();
            let old_bytes = old.spectra.iter().map(|s| s.len()).sum::<usize>() * NFFT * std::mem::size_of::<C>();
            drop(old);
            let spec = bank_spectra_match(&coeffs, &bank);
            // Eight blocks (6 s) after the first output block.
            let j0 = 8;
            let (max, rms, peak, b_new, b_old) = compare(&coeffs, bank.clone(), src.clone(), j0, 6);
            let depth = bank.max_partitions();
            let fdl = |bins: usize| 2 * depth * bins * std::mem::size_of::<C>();
            println!(
                "{:>3} {:<13} P {:>3} | diff max {:.1} dBFS RMS {:.1} dBFS (peak {:.2}) spectra {:.1} dB | build {:.3} -> {:.3} s (incl. polyphase) | block {:.1} -> {:.1} ms (RTF {:.0}x -> {:.0}x) | bank {} -> {} MiB, FDL {} -> {} MiB",
                tag, phase, depth, db(max), db(rms), peak, db(spec), t_old, t_new, b_old * 1e3, b_new * 1e3,
                BLOCK as f64 / 44_100.0 / b_old, BLOCK as f64 / 44_100.0 / b_new,
                old_bytes >> 20, bank.bytes() >> 20, fdl(NFFT) >> 20, fdl(NBIN) >> 20
            );
            assert!(max <= 1e-13 * peak.max(1.0), "{} {}: max diff {:e}", tag, phase, max);
            assert!(spec <= 1e-13, "{} {}: bank spectra differ by {:e}", tag, phase, spec);
        }
    }
}
