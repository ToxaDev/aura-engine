//! Multistage upsampling, the part the converter and the player share: the plan (where each stage trims, how much
//! history stage 0 keeps, the gain), the block FIR of the short chain and its streaming form.
//!
//! Today a blob h, designed at R = L·fs, plays at L polyphase branches:
//!
//! ```text
//!     out[m] = scale · Σ_n x[n] h[m + d − nL],   scale = L / Σh,   d = today's trim (output frames)
//! ```
//!
//! Multistage plays branch 0, h0[k] = h[kL], at the source rate (L = 1, stage 0) and then a short linear-phase
//! chain H1 (stages i = 1..n, ratio r_i, odd length N_i, D_i = (N_i − 1)/2, DC gain r_i) up to ×L. Every phase
//! (linear, minimum, TFS, stream-linear, both Hybrid-Phase halves) uses branch 0; d splits as
//!
//! ```text
//!     stage 0:   z[j] = scale0 · (h0 * x)[j + k0],      scale0 = L / Σh of the WHOLE blob
//!     stage i:   trims t_i = D_i of its own output frames (i < n)
//!     stage n:   trims t_n = D_n + d − k0·L
//! ```
//!
//! Each stage reads its input from index 0 on, with zeros before it. A stage-i output y (its own frames, trimmed)
//! is the causal output Y = y + t_i; Y needs the input frames from ⌊Y/r_i⌋ − ⌊(N_i − 1 − Y mod r_i)/r_i⌋ to ⌊Y/r_i⌋.
//! Walking that back from output 0 gives the first input frame each stage reads; `k0` is the LARGEST k in
//! 0 ..= d div L for which none of them is negative — then nothing at the track start is interpolated from silence
//! that today's output has as the filter's pre-ringing, with the least stage-0 history. (MS-2 proved the one-stage
//! case with k0 = d div L − c, c = 1 + Σ⌈D_i / (r_1⋯r_i)⌉; the walk is the same bound, exact per stage and per
//! rounding, for any number of stages.) If even k = 0 reads below index 0 (d < about c·L: minimum phase, the
//! band-weighted half), stage 0 starts at the source start, where z is truly zero before index 0.
//!
//! The same walk from any output s gives the stage-0 frame a seek starts from ([`MsPlan::stage0_start`]); the walk
//! forward from the last output gives how many stage-0 frames the end needs ([`MsPlan::stage0_frames`]) — the
//! chain itself holds nothing back, so there is no separate flush.
//!
//! Every output of the block FIR is one fixed sum over its taps in a fixed order (plain multiply and add, never
//! fused), whatever the chunk, the tile or the start: the converter and the player get the same bits.
#![cfg_attr(not(test), allow(dead_code))]

use super::ms_h1::{h1_chain, H1Stage};
use std::sync::Arc;

/// Which upsampler plays integer ratios: today's single stage, or stage 0 at the source rate plus the short chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpsamplerMode {
    Single,
    Multi,
}

impl UpsamplerMode {
    /// Multi is the default; `AURA_MULTISTAGE=0` → Single (the hidden fallback).
    pub fn from_env() -> UpsamplerMode {
        Self::from_value(std::env::var("AURA_MULTISTAGE").ok().as_deref())
    }

    fn from_value(v: Option<&str>) -> UpsamplerMode {
        match v.map(str::trim) {
            Some("0") => UpsamplerMode::Single,
            _ => UpsamplerMode::Multi,
        }
    }
}

/// Blobs shorter than this stay single-stage (1M, 5k, the output-peak probe).
pub const MIN_TAPS: usize = 5_000_000;

/// The short chain a `taps`-long blob plays through from `src_rate` at ×`l`, or `None` = single stage: Multi only,
/// L ∈ {2, 4, 8, 16, 32}, ≥ [`MIN_TAPS`], a standard source rate (44.1/88.2/176.4k, 48/96/192k) and a shipped chain.
pub fn multistage_chain(mode: UpsamplerMode, taps: usize, src_rate: u32, l: usize) -> Option<&'static [H1Stage]> {
    if mode != UpsamplerMode::Multi || taps < MIN_TAPS || !matches!(l, 2 | 4 | 8 | 16 | 32) {
        return None;
    }
    let base = match src_rate {
        44_100 | 88_200 | 176_400 => 44_100,
        48_000 | 96_000 | 192_000 => 48_000,
        _ => return None,
    };
    h1_chain(base, l)
}

/// Frames a piece of input is cut into on its way through the chain (memory, not arithmetic: any cut gives the
/// same bits).
const CHUNK: usize = 2048;
/// Outputs computed side by side in the block FIR.
const TILE: usize = 32;

/// One branch of a stage: output frame f, phase p = Σ_t taps[t] · w[off + t], w = input frames f − (K − 1) ..= f.
#[derive(Debug)]
struct Branch {
    off: usize,
    taps: Vec<f64>,
    /// The branch is its own mirror: pairs of frames are added first.
    sym: bool,
}

/// A short stage ready to run: ×`r` through `n` taps, as `r` branches of at most `k` taps.
#[derive(Debug)]
struct PolyStage {
    r: usize,
    n: usize,
    k: usize,
    /// Output frames this stage trims.
    trim: usize,
    branches: Vec<Branch>,
}

impl PolyStage {
    fn new(h: &[f64], r: usize, trim: usize) -> PolyStage {
        let n = h.len();
        assert!(r >= 1 && n >= r, "a stage needs at least `ratio` taps");
        let k = n.div_ceil(r);
        let branches = (0..r)
            .map(|p| {
                // b[t] = h[(K − 1 − t)·r + p]: the oldest frame first.
                let full: Vec<f64> = (0..k)
                    .map(|t| {
                        let idx = (k - 1 - t) * r + p;
                        if idx < n { h[idx] } else { 0.0 }
                    })
                    .collect();
                match (full.iter().position(|&v| v != 0.0), full.iter().rposition(|&v| v != 0.0)) {
                    (Some(first), Some(last)) => {
                        let taps = full[first..=last].to_vec();
                        let sym = taps.iter().zip(taps.iter().rev()).all(|(a, b)| a.to_bits() == b.to_bits());
                        Branch { off: first, taps, sym }
                    }
                    _ => Branch { off: 0, taps: Vec::new(), sym: false },
                }
            })
            .collect();
        PolyStage { r, n, k, trim, branches }
    }

    /// The first input frame the outputs from `s` (trimmed index) on read, signed.
    fn first_input(&self, s: i64) -> i64 {
        let y = s + self.trim as i64;
        let (r, n0, p0) = (self.r as i64, y.div_euclid(self.r as i64), y.rem_euclid(self.r as i64));
        n0 - (self.n as i64 - 1 - p0) / r
    }

    /// Multiplies per input frame (folded branches count half).
    fn mults_per_frame(&self) -> usize {
        self.branches.iter().map(|b| if b.sym { b.taps.len().div_ceil(2) } else { b.taps.len() }).sum()
    }
}

/// `y[i·r + p]` for every frame `i` of `x` past its `K − 1` history frames. The shared block function: each output
/// is the same sequence of operations wherever it falls.
fn run_block(st: &PolyStage, x: &[f64], y: &mut [f64]) {
    let n = x.len() + 1 - st.k;
    assert_eq!(y.len(), n * st.r);
    let r = st.r;
    // Tile by tile, every branch of a tile while its window is in L1.
    let mut i = 0;
    while i + TILE <= n {
        let yt = &mut y[i * r..(i + TILE) * r];
        for (p, b) in st.branches.iter().enumerate() {
            let acc = branch_tile::<TILE>(b, &x[i + b.off..]);
            for (u, v) in acc.iter().enumerate() {
                yt[u * r + p] = *v;
            }
        }
        i += TILE;
    }
    while i < n {
        for (p, b) in st.branches.iter().enumerate() {
            y[i * r + p] = branch_tile::<1>(b, &x[i + b.off..])[0];
        }
        i += 1;
    }
}

/// `T` consecutive outputs of one branch, each summed over the taps in the same order.
#[inline(always)]
fn branch_tile<const T: usize>(b: &Branch, w: &[f64]) -> [f64; T] {
    let len = b.taps.len();
    let mut acc = [0.0f64; T];
    if b.sym {
        let half = len / 2;
        for t in 0..half {
            let c = b.taps[t];
            let xa = &w[t..t + T];
            let xb = &w[len - 1 - t..len - 1 - t + T];
            for u in 0..T {
                acc[u] += c * (xa[u] + xb[u]);
            }
        }
        if len % 2 == 1 {
            let c = b.taps[half];
            let xm = &w[half..half + T];
            for u in 0..T {
                acc[u] += c * xm[u];
            }
        }
    } else {
        for t in 0..len {
            let c = b.taps[t];
            let xa = &w[t..t + T];
            for u in 0..T {
                acc[u] += c * xa[u];
            }
        }
    }
    acc
}

/// How a blob plays multistage: built once per (blob, phase, L, chain), shared by every stream.
#[derive(Clone, Debug)]
pub struct MsPlan {
    /// The ratio, output rate / source rate.
    pub l: usize,
    /// Today's trim, output frames.
    pub d: usize,
    /// Source frames stage 0 trims.
    pub k0: usize,
    /// Source frames of history stage 0 keeps before d div L: `d / l − k0`.
    pub c: usize,
    /// `L / Σh` of the whole blob: stage 0's gain (same expression as `Bank` and the converter).
    pub scale0: f64,
    stages: Arc<[PolyStage]>,
}

impl MsPlan {
    /// The plan for a blob whose single-stage trim is `d`, at ×`l`, with `sum_h` = Σh of the whole blob.
    pub fn new(d: usize, l: usize, sum_h: f64, chain: &[H1Stage]) -> MsPlan {
        Self::build(d, l, sum_h, chain, None)
    }

    fn build(d: usize, l: usize, sum_h: f64, chain: &[H1Stage], k0: Option<usize>) -> MsPlan {
        assert!(!chain.is_empty(), "an empty chain");
        assert_eq!(chain.iter().map(|s| s.ratio).product::<usize>(), l, "the chain's ratios must multiply to L");
        let delays: Vec<usize> = chain.iter().map(|s| (s.taps.len() - 1) / 2).collect();
        let stages_for = |k: usize| -> Vec<PolyStage> {
            let n = chain.len();
            chain
                .iter()
                .enumerate()
                .map(|(i, s)| PolyStage::new(s.taps, s.ratio, delays[i] + if i + 1 == n { d - k * l } else { 0 }))
                .collect()
        };
        // Whether the outputs from 0 on read no stage's input below index 0.
        let clean = |k: usize| -> bool {
            let mut s = 0i64;
            for st in stages_for(k).iter().rev() {
                s = st.first_input(s);
                if s < 0 {
                    return false;
                }
            }
            true
        };
        let k0 = k0.unwrap_or_else(|| {
            // `clean` holds for every k below the largest one that holds (a smaller k trims less).
            let (mut lo, mut hi) = (0usize, d / l);
            if !clean(lo) {
                return 0;
            }
            while lo < hi {
                let mid = lo + (hi - lo).div_ceil(2);
                if clean(mid) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            lo
        });
        MsPlan {
            l,
            d,
            k0,
            c: d / l - k0,
            scale0: l as f64 / sum_h.abs().max(1e-10),
            stages: stages_for(k0).into(),
        }
    }

    /// Output frames each H1 stage trims, in its own output frames.
    pub fn trims(&self) -> Vec<usize> {
        self.stages.iter().map(|s| s.trim).collect()
    }

    /// Stage-0 frames (trimmed index) the chain needs to produce outputs 0 .. `out_len`: for a whole track,
    /// `stage0_frames(len · L)` — past the source end, stage 0's tail.
    pub fn stage0_frames(&self, out_len: u64) -> u64 {
        if out_len == 0 {
            return 0;
        }
        let mut y = out_len - 1;
        for st in self.stages.iter().rev() {
            y = (y + st.trim as u64) / st.r as u64;
        }
        y + 1
    }

    /// The first stage-0 frame (trimmed index) a chain playing from output frame `s` reads: stage 0 starts there.
    pub fn stage0_start(&self, s: u64) -> u64 {
        let mut a = s as i64;
        for st in self.stages.iter().rev() {
            a = st.first_input(a).max(0);
        }
        a as u64
    }

    /// Multiplies per source frame through the whole chain (folding counted).
    pub fn mults_per_source_frame(&self) -> usize {
        let mut rate = 1;
        let mut total = 0;
        for st in self.stages.iter() {
            total += st.mults_per_frame() * rate;
            rate *= st.r;
        }
        total
    }
}

/// One stage's stream state: the input window (`K − 1` frames of history, then new ones) and the outputs still
/// to drop before the first one handed out.
struct StageState {
    x: Vec<f64>,
    skip: usize,
}

impl StageState {
    fn push(&mut self, st: &PolyStage, input: &[f64], out: &mut Vec<f64>) {
        self.x.extend_from_slice(input);
        let hist = st.k - 1;
        if self.x.len() <= hist {
            return;
        }
        let n = self.x.len() - hist;
        let base = out.len();
        out.resize(base + n * st.r, 0.0);
        run_block(st, &self.x, &mut out[base..]);
        if self.skip > 0 {
            let s = self.skip.min(n * st.r);
            out.drain(base..base + s);
            self.skip -= s;
        }
        self.x.drain(..n);
    }
}

/// The short chain as a stream, one channel: fed stage-0 frames from [`InterpChain::input_start`] on, it hands out
/// output frames from its start on — the same bits as the whole-track pass, from any start and in any pieces.
pub struct InterpChain {
    plan: MsPlan,
    states: Vec<StageState>,
    input_start: u64,
    pos: u64,
    a: Vec<f64>,
    b: Vec<f64>,
}

impl InterpChain {
    /// A chain whose first output is output frame `start` (0 = the track start).
    pub fn new(plan: &MsPlan, start: u64) -> InterpChain {
        let mut states: Vec<StageState> = Vec::with_capacity(plan.stages.len());
        let mut s = start as i64;
        for st in plan.stages.iter().rev() {
            let y = s + st.trim as i64;
            let (n0, p0) = (y / st.r as i64, (y % st.r as i64) as usize);
            let a = st.first_input(s).max(0);
            // Frames n0 − (K − 1) .. a − 1 are never read by the outputs handed out: zeros.
            let pad = (st.k as i64 - 1 - (n0 - a)) as usize;
            states.push(StageState { x: vec![0.0; pad], skip: p0 });
            s = a;
        }
        states.reverse();
        InterpChain { plan: plan.clone(), states, input_start: s as u64, pos: start, a: Vec::new(), b: Vec::new() }
    }

    /// The stage-0 frame (trimmed index) the first `process` input must start with: `plan.stage0_start(start)`.
    pub fn input_start(&self) -> u64 {
        self.input_start
    }

    /// The output frame the next appended output is.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Feed the next stage-0 frames; appends to `out` every output frame they complete.
    pub fn process(&mut self, input: &[f64], out: &mut Vec<f64>) {
        let before = out.len();
        let stages = self.plan.stages.clone();
        let last = stages.len() - 1;
        let (mut a, mut b) = (std::mem::take(&mut self.a), std::mem::take(&mut self.b));
        for piece in input.chunks(CHUNK) {
            if last == 0 {
                self.states[0].push(&stages[0], piece, out);
                continue;
            }
            a.clear();
            self.states[0].push(&stages[0], piece, &mut a);
            for i in 1..last {
                b.clear();
                self.states[i].push(&stages[i], &a, &mut b);
                std::mem::swap(&mut a, &mut b);
            }
            self.states[last].push(&stages[last], &a, out);
        }
        self.a = a;
        self.b = b;
        self.pos += (out.len() - before) as u64;
    }
}

/// The chain over a whole stage-0 signal `z` (trimmed index 0 on, at least `plan.stage0_frames(out_len)` frames):
/// output frames 0 .. `out_len`.
pub fn interp_whole(plan: &MsPlan, z: &[f64], out_len: usize) -> Vec<f64> {
    let need = plan.stage0_frames(out_len as u64) as usize;
    assert!(z.len() >= need, "{} stage-0 frames, {} needed", z.len(), need);
    let mut chain = InterpChain::new(plan, 0);
    let mut out = Vec::with_capacity(out_len + CHUNK * plan.l);
    chain.process(&z[(chain.input_start() as usize).min(need)..need], &mut out);
    out.truncate(out_len);
    out
}

/// The whole multistage pass by plain arithmetic, for tests and benches: stage 0 as a direct convolution with
/// branch 0 of `h`, then [`interp_whole`]. `x.len() · l` output frames, aligned like today's single stage.
pub fn two_stage_reference(x: &[f64], h: &[f64], l: usize, d: usize, chain: &[H1Stage]) -> Vec<f64> {
    let plan = MsPlan::new(d, l, h.iter().sum(), chain);
    reference_for(&plan, x, h)
}

fn reference_for(plan: &MsPlan, x: &[f64], h: &[f64]) -> Vec<f64> {
    let h0: Vec<f64> = h.iter().step_by(plan.l).copied().collect();
    let out_len = x.len() * plan.l;
    let frames = plan.stage0_frames(out_len as u64) as usize;
    let z: Vec<f64> = (0..frames)
        .map(|j| {
            let n = j + plan.k0;
            let lo = (n + 1).saturating_sub(h0.len());
            let mut s = 0.0;
            for i in lo..=n.min(x.len().saturating_sub(1)) {
                s += x[i] * h0[n - i];
            }
            plan.scale0 * s
        })
        .collect();
    interp_whole(plan, &z, out_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const FS: f64 = 44_100.0;
    /// The test blobs pass to 17 kHz and stop from 19 kHz; the chains are flat to 19.5 kHz and stop the images
    /// from fs − 19.5 kHz — the same layout as the product's wall inside the chain's passband.
    const PASS: f64 = 19_500.0;

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

    const A_H1: f64 = 222.0;

    /// The chains under test: (name, L, stages). Half-band ×2 alone; half-band + ×4 polyphase (S1's likely x8
    /// shape); three half-bands (×8), which walks the history back through every stage.
    fn chains() -> &'static [(&'static str, usize, &'static [H1Stage])] {
        static C: OnceLock<Vec<(&'static str, usize, &'static [H1Stage])>> = OnceLock::new();
        C.get_or_init(|| {
            let hb1 = || design(2.0 * FS, PASS, FS - PASS, A_H1, 2.0, true);
            vec![
                ("hb1 x2", 2, leak(vec![(2, hb1())])),
                ("hb1+p4 x8", 8, leak(vec![(2, hb1()), (4, design(8.0 * FS, PASS, 2.0 * FS - (FS - PASS), A_H1, 4.0, false))])),
                (
                    "hb1+hb2+hb3 x8",
                    8,
                    leak(vec![
                        (2, hb1()),
                        (2, design(4.0 * FS, PASS, 2.0 * FS - PASS, A_H1, 2.0, true)),
                        (2, design(8.0 * FS, PASS, 4.0 * FS - PASS, A_H1, 2.0, true)),
                    ]),
                ),
            ]
        })
    }

    /// Today's kind of blob at R = L·fs: linear phase, flat to 17 kHz, 255 dB down from 19 kHz (deep enough that
    /// branch 0's DC gain is L/Σh to 1e-12, as the product's blobs are).
    fn blob_linear(l: usize) -> &'static [f64] {
        static B: OnceLock<std::sync::Mutex<Vec<(usize, &'static [f64])>>> = OnceLock::new();
        let m = B.get_or_init(Default::default);
        let mut g = m.lock().unwrap();
        if let Some((_, b)) = g.iter().find(|(k, _)| *k == l) {
            return b;
        }
        let b: &'static [f64] = Box::leak(design(l as f64 * FS, 17_000.0, 19_000.0, 255.0, 1.0, false).into_boxed_slice());
        g.push((l, b));
        b
    }

    fn blob_min(l: usize) -> Vec<f64> {
        crate::audio::dsp_core::to_minimum_phase(blob_linear(l))
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
            })
            .collect()
    }

    /// Today's single stage by plain arithmetic: `out[m] = scale · Σ_n x[n] h[m + d − nL]`.
    fn single(x: &[f64], h: &[f64], l: usize, d: usize) -> Vec<f64> {
        let sum: f64 = h.iter().sum();
        let scale = l as f64 / sum.abs().max(1e-10);
        (0..x.len() * l)
            .map(|m| {
                let c = m + d;
                let hi = (c / l).min(x.len() - 1);
                let lo = (c + 1).saturating_sub(h.len()).div_ceil(l);
                let mut s = 0.0;
                for n in lo..=hi {
                    s += x[n] * h[c - n * l];
                }
                scale * s
            })
            .collect()
    }

    fn peak_db(a: &[f64], b: &[f64]) -> f64 {
        let e = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max);
        20.0 * e.max(1e-300).log10()
    }

    fn argmax(v: &[f64]) -> usize {
        v.iter().enumerate().fold((0, 0.0), |(bi, bv), (i, &x)| if x.abs() > bv { (i, x.abs()) } else { (bi, bv) }).0
    }

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// Every phase as (name, blob, d): linear, TFS (a fixed look-ahead), minimum (None), band-weighted.
    fn phases(l: usize) -> Vec<(&'static str, Vec<f64>, usize, f64)> {
        let lin = blob_linear(l).to_vec();
        let min = blob_min(l);
        let bw = crate::audio::dsp_core::estimate_band_weighted_group_delay(&min, l as f64 * FS, 200.0, 6000.0);
        let k_tfs = (lin.len() - 1) / 2 * 3 / 4 | 1;
        vec![
            ("linear", lin.clone(), (lin.len() - 1) / 2, -200.0),
            ("tfs", lin, k_tfs, -200.0),
            ("minimum", min.clone(), 0, -200.0),
            ("band_weighted", min, bw, -200.0),
        ]
    }

    /// D8, for the record: the band-weighted trim measured on branch 0 at the source rate (× L) against today's,
    /// on every shipped minimum-phase blob of a multistage rung. The plan uses today's (from `NpyBranch::head`,
    /// exactly equal by construction); this only shows how far the branch-0 shortcut would be off.
    ///
    ///     AURA_FILTER_DIR=<blobs> cargo test --profile fast --bins band_weighted_from_branch_0 -- --ignored --nocapture
    #[test]
    #[ignore]
    fn band_weighted_from_branch_0_against_today() {
        use crate::audio::dsp_core::{estimate_band_weighted_group_delay as bw, load_npy_f64};
        let Ok(dir) = std::env::var("AURA_FILTER_DIR") else {
            println!("[ms] AURA_FILTER_DIR not set - skip");
            return;
        };
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with("_minimum_phase.npy") && ["fir_5M_", "fir_10M_", "fir_30M_"].iter().any(|p| n.starts_with(p)))
            .collect();
        names.sort();
        println!("| blob | L | today (output frames) | head | branch 0 × L | differs |");
        for name in names {
            let Some(rate) = name.split('_').nth(2).and_then(|r| r.parse::<u32>().ok()) else { continue };
            let path = format!("{}/{}", dir.trim_end_matches(['/', '\\']), name);
            let h = load_npy_f64(&path).unwrap();
            let today = bw(&h, rate as f64, 200.0, 6000.0);
            // What `NpyBranch::head` holds: the estimator's prefix.
            let head = bw(&h[..h.len().min(1 << 17)], rate as f64, 200.0, 6000.0);
            assert_eq!(head, today, "{name}");
            for l in [2usize, 4, 8, 16, 32] {
                let src = rate / l as u32;
                if rate % l as u32 != 0 || multistage_chain_rate(src).is_none() {
                    continue;
                }
                let b0: Vec<f64> = h.iter().step_by(l).take(1 << 17).copied().collect();
                let from_b0 = bw(&b0, src as f64, 200.0, 6000.0) * l;
                println!("| {name} | {l} | {today} | {head} | {from_b0} | {} |", if from_b0 == today { "" } else { "YES" });
            }
        }
    }

    fn multistage_chain_rate(src: u32) -> Option<u32> {
        match src {
            44_100 | 88_200 | 176_400 => Some(44_100),
            48_000 | 96_000 | 192_000 => Some(48_000),
            _ => None,
        }
    }

    #[test]
    fn the_mode_comes_from_the_environment_value_and_the_gate_keeps_odd_cases_single() {
        assert_eq!(UpsamplerMode::from_value(None), UpsamplerMode::Multi);
        assert_eq!(UpsamplerMode::from_value(Some(" 0 ")), UpsamplerMode::Single);
        assert_eq!(UpsamplerMode::from_value(Some("1")), UpsamplerMode::Multi);
        assert_eq!(UpsamplerMode::from_value(Some("")), UpsamplerMode::Multi);
        assert_eq!(UpsamplerMode::from_env(), UpsamplerMode::from_value(std::env::var("AURA_MULTISTAGE").ok().as_deref()));
        use UpsamplerMode::*;
        for &(mode, taps, src, l) in &[
            (Single, 30_000_000, 44_100, 8),
            (Multi, 1_000_000, 44_100, 8),
            (Multi, 4_999_999, 48_000, 8),
            (Multi, 30_000_000, 22_050, 8),
            (Multi, 30_000_000, 32_000, 8),
            (Multi, 30_000_000, 352_800, 2),
            (Multi, 30_000_000, 44_100, 1),
            (Multi, 30_000_000, 44_100, 3),
            (Multi, 30_000_000, 44_100, 64),
        ] {
            assert!(multistage_chain(mode, taps, src, l).is_none(), "{mode:?} {taps} {src} x{l}");
        }
        for &(src, base) in &[(44_100, 44_100), (88_200, 44_100), (176_400, 44_100), (48_000, 48_000), (96_000, 48_000), (192_000, 48_000)] {
            for l in [2usize, 4, 8, 16, 32] {
                assert_eq!(multistage_chain(Multi, 5_000_000, src, l).is_some(), h1_chain(base, l).is_some());
            }
        }
    }

    #[test]
    fn the_block_fir_is_the_zero_stuffed_convolution_and_skips_zero_taps() {
        for (name, _, chain) in chains() {
            for s in chain.iter() {
                let st = PolyStage::new(s.taps, s.ratio, 0);
                let x = noise(700, 3);
                let mut w = vec![0.0; st.k - 1];
                w.extend_from_slice(&x);
                let mut y = vec![0.0; x.len() * s.ratio];
                run_block(&st, &w, &mut y);
                for (m, v) in y.iter().enumerate() {
                    let mut e = 0.0;
                    for (j, hv) in s.taps.iter().enumerate() {
                        if j <= m && (m - j) % s.ratio == 0 {
                            e += hv * x[(m - j) / s.ratio];
                        }
                    }
                    assert!((v - e).abs() < 1e-14, "{name} x{} m {m}", s.ratio);
                }
                if s.ratio == 2 {
                    // A half-band: one branch is the centre tap alone, the other folds.
                    assert_eq!(st.mults_per_frame(), 1 + st.branches.iter().map(|b| b.taps.len()).max().unwrap().div_ceil(2));
                }
            }
            let plan = MsPlan::new(0, chain.iter().map(|s| s.ratio).product(), 1.0, chain);
            println!("[ms] {name}: {} taps, {} multiplies per source frame", chain.iter().map(|s| s.taps.len()).sum::<usize>(), plan.mults_per_source_frame());
            assert!(plan.mults_per_source_frame() > 0);
        }
    }

    #[test]
    fn the_plan_trims_add_up_and_k0_keeps_just_enough_history() {
        for (name, l, chain) in chains() {
            let l = *l;
            for d in [0usize, 1, 3, 51, 200, 1394, 1395, 5000, 15_000_001] {
                let p = MsPlan::new(d, l, 1.0, chain);
                assert_eq!((p.d, p.l), (d, l));
                let n = chain.len();
                let trims = p.trims();
                for i in 0..n - 1 {
                    assert_eq!(trims[i], (chain[i].taps.len() - 1) / 2);
                }
                assert_eq!(trims[n - 1], (chain[n - 1].taps.len() - 1) / 2 + d - p.k0 * l, "{name} d {d}");
                assert_eq!(p.c, d / l - p.k0);
                // MS-2's one-stage bound, generalised: the exact walk keeps no more history than it plus one.
                let mut ratio = 1;
                let mut c_ms2 = 1;
                for s in chain.iter() {
                    ratio *= s.ratio;
                    c_ms2 += ((s.taps.len() - 1) / 2).div_ceil(ratio);
                }
                if d / l > c_ms2 + 1 {
                    if d == 5000 {
                        println!("[ms] {name}: c {} (MS-2 formula {c_ms2})", p.c);
                    }
                    assert!(p.c <= c_ms2 + 1, "{name} d {d}: c {} vs {}", p.c, c_ms2);
                    // The largest k0: one more source frame trimmed would read below index 0.
                    let q = MsPlan::build(d, l, 1.0, chain, Some(p.k0 + 1));
                    let mut s = 0i64;
                    assert!(q.stages.iter().rev().any(|st| {
                        s = st.first_input(s);
                        s < 0
                    }));
                }
                assert_eq!(p.stage0_frames(0), 0);
                assert!(p.stage0_start(0) == 0);
                // Monotone and consistent: the frames for n outputs cover the start of output n − 1.
                for out in [1u64, 2, 9, 1000, 35_280] {
                    assert!(p.stage0_frames(out) > p.stage0_start(out - 1));
                    assert!(p.stage0_frames(out) <= p.stage0_frames(out + 1));
                }
            }
        }
        let p = MsPlan::new(10, 2, 0.5, chains()[0].2);
        assert_eq!(p.scale0.to_bits(), (2.0f64 / 0.5f64.abs().max(1e-10)).to_bits());
    }

    #[test]
    fn an_impulse_lands_at_todays_index_for_every_phase() {
        for (name, l, chain) in chains() {
            for (phase, h, d, limit) in phases(*l) {
                let mut x = vec![0.0; 2000];
                x[3] = 0.5;
                x[1200] = -0.5;
                let want = single(&x, &h, *l, d);
                let got = two_stage_reference(&x, &h, *l, d, chain);
                assert_eq!(got.len(), want.len());
                let late = 1000 * l;
                assert_eq!(argmax(&got[late..]), argmax(&want[late..]), "{name} {phase}");
                assert_eq!(argmax(&got[..late]), argmax(&want[..late]), "{name} {phase}: at the track start");
                let e = peak_db(&got, &want);
                println!("[ms] impulse {name} {phase} d {d}: null {e:.1} dBFS");
                assert!(e <= limit, "{name} {phase}: null {e:.1} dBFS");
            }
        }
    }

    #[test]
    fn the_gain_at_dc_is_one() {
        for (name, l, chain) in chains() {
            for (phase, h, d, _) in phases(*l) {
                let x = vec![1.0; 3000];
                let y = two_stage_reference(&x, &h, *l, d, chain);
                // Away from the ends, where the blob's whole length is inside the signal.
                let a = (h.len() + 400 * l).next_multiple_of(*l);
                let b = a + (y.len() - h.len() - 400 * l - a) / l * l;
                // The gain at 0 Hz: the mean over each output period of L frames. Single frames also carry DC's
                // images at fs, 2fs, … — H1's stopband (≤ −210 dB by spec), as today's carry the blob's.
                let e_dc = y[a..b].chunks_exact(*l).map(|c| (c.iter().sum::<f64>() / *l as f64 - 1.0).abs()).fold(0.0, f64::max);
                let e = y[a..b].iter().map(|v| (v - 1.0).abs()).fold(0.0, f64::max);
                println!("[ms] dc {name} {phase}: gain {e_dc:.1e}, per frame {e:.1e}");
                assert!(e_dc <= 1e-12, "{name} {phase}: DC gain off by {e_dc:e}");
                assert!(e <= 10f64.powf(-210.0 / 20.0), "{name} {phase}: DC images {e:e}");
            }
        }
    }

    #[test]
    fn a_linear_phase_track_start_matches_today_to_200_db() {
        for (name, l, chain) in chains() {
            let h = blob_linear(*l);
            let d = (h.len() - 1) / 2;
            let x = noise(4410, 7);
            let want = single(&x, h, *l, d);
            let got = two_stage_reference(&x, h, *l, d, chain);
            let n10 = (FS as usize / 100) * l;
            let e10 = peak_db(&got[..n10], &want[..n10]);
            let e = peak_db(&got, &want);
            println!("[ms] start {name}: first 10 ms {e10:.1} dBFS, whole {e:.1} dBFS");
            assert!(e10 <= -200.0 && e <= -200.0, "{name}: {e10:.1} / {e:.1} dBFS");
        }
    }

    #[test]
    fn the_exact_history_gives_the_same_bits_as_a_full_one() {
        // k0 from the walk and k0 = 0 (all of stage 0's history) must agree bit for bit: nothing reads below 0.
        for (name, l, chain) in chains() {
            let h = blob_linear(*l);
            let d = (h.len() - 1) / 2;
            let x = noise(1500, 21);
            let sum: f64 = h.iter().sum();
            let exact = MsPlan::new(d, *l, sum, chain);
            assert!(exact.k0 > 0, "{name}");
            let full = MsPlan::build(d, *l, sum, chain, Some(0));
            assert_eq!(bits(&reference_for(&exact, &x, h)), bits(&reference_for(&full, &x, h)), "{name}");
        }
    }

    /// Stage 0's output for a whole track (trimmed, `stage0_frames` long) and the whole-track chain output.
    fn whole(name: &str, l: usize, chain: &[H1Stage]) -> (MsPlan, Vec<f64>, Vec<f64>) {
        let h = blob_linear(l);
        let (x, d) = (noise(9000, name.len() as u64), (h.len() - 1) / 2 + 3);
        let plan = MsPlan::new(d, l, h.iter().sum(), chain);
        let frames = plan.stage0_frames((x.len() * l) as u64) as usize;
        let z = noise(frames, 99);
        let y = interp_whole(&plan, &z, x.len() * l);
        (plan, z, y)
    }

    #[test]
    fn any_chunk_size_gives_the_same_bits() {
        for (name, l, chain) in chains() {
            let (plan, z, want) = whole(name, *l, chain);
            for chunk in [1usize, 7, 4096, 8192, 10_000] {
                let mut c = InterpChain::new(&plan, 0);
                let mut out = Vec::new();
                for piece in z.chunks(chunk) {
                    c.process(piece, &mut out);
                }
                assert_eq!(c.position(), out.len() as u64);
                out.truncate(want.len());
                assert_eq!(bits(&out), bits(&want), "{name} chunk {chunk}");
            }
        }
    }

    #[test]
    fn a_stream_from_any_start_plays_the_whole_track() {
        for (name, l, chain) in chains() {
            let (plan, z, want) = whole(name, *l, chain);
            let n = want.len() as u64;
            for start in [0u64, 1, 5, 7, 8, 9, 63, 1001, 4099, n / 2 + 3, n - 17, n - 1] {
                let mut c = InterpChain::new(&plan, start);
                assert_eq!(c.input_start(), plan.stage0_start(start), "{name} {start}");
                assert_eq!(c.position(), start);
                let mut out = Vec::new();
                for piece in z[c.input_start() as usize..].chunks(333) {
                    c.process(piece, &mut out);
                }
                let take = (n - start) as usize;
                assert!(out.len() >= take, "{name} {start}: {} < {take}", out.len());
                assert_eq!(bits(&out[..take]), bits(&want[start as usize..]), "{name} start {start}");
            }
        }
    }

    /// The chain as it was first written (6b2bea4), kept verbatim as the bit reference for the faster one.
    mod frozen {
        use super::super::H1Stage;

        const CHUNK: usize = 8192;
        const TILE: usize = 16;

        struct Branch {
            off: usize,
            taps: Vec<f64>,
            sym: bool,
        }

        pub struct Stage {
            r: usize,
            n: usize,
            k: usize,
            trim: usize,
            branches: Vec<Branch>,
        }

        impl Stage {
            fn new(h: &[f64], r: usize, trim: usize) -> Stage {
                let n = h.len();
                let k = n.div_ceil(r);
                let branches = (0..r)
                    .map(|p| {
                        let full: Vec<f64> = (0..k)
                            .map(|t| {
                                let idx = (k - 1 - t) * r + p;
                                if idx < n { h[idx] } else { 0.0 }
                            })
                            .collect();
                        match (full.iter().position(|&v| v != 0.0), full.iter().rposition(|&v| v != 0.0)) {
                            (Some(first), Some(last)) => {
                                let taps = full[first..=last].to_vec();
                                let sym = taps.iter().zip(taps.iter().rev()).all(|(a, b)| a.to_bits() == b.to_bits());
                                Branch { off: first, taps, sym }
                            }
                            _ => Branch { off: 0, taps: Vec::new(), sym: false },
                        }
                    })
                    .collect();
                Stage { r, n, k, trim, branches }
            }

            fn first_input(&self, s: i64) -> i64 {
                let y = s + self.trim as i64;
                let (r, n0, p0) = (self.r as i64, y.div_euclid(self.r as i64), y.rem_euclid(self.r as i64));
                n0 - (self.n as i64 - 1 - p0) / r
            }
        }

        fn run_block(st: &Stage, x: &[f64], y: &mut [f64]) {
            let n = x.len() + 1 - st.k;
            assert_eq!(y.len(), n * st.r);
            let r = st.r;
            for (p, b) in st.branches.iter().enumerate() {
                let mut i = 0;
                while i + TILE <= n {
                    let acc = branch_tile::<TILE>(b, &x[i + b.off..]);
                    for (u, v) in acc.iter().enumerate() {
                        y[(i + u) * r + p] = *v;
                    }
                    i += TILE;
                }
                while i < n {
                    y[i * r + p] = branch_tile::<1>(b, &x[i + b.off..])[0];
                    i += 1;
                }
            }
        }

        #[inline(always)]
        fn branch_tile<const T: usize>(b: &Branch, w: &[f64]) -> [f64; T] {
            let len = b.taps.len();
            let mut acc = [0.0f64; T];
            if b.sym {
                let half = len / 2;
                for t in 0..half {
                    let c = b.taps[t];
                    let xa = &w[t..t + T];
                    let xb = &w[len - 1 - t..len - 1 - t + T];
                    for u in 0..T {
                        acc[u] += c * (xa[u] + xb[u]);
                    }
                }
                if len % 2 == 1 {
                    let c = b.taps[half];
                    let xm = &w[half..half + T];
                    for u in 0..T {
                        acc[u] += c * xm[u];
                    }
                }
            } else {
                for t in 0..len {
                    let c = b.taps[t];
                    let xa = &w[t..t + T];
                    for u in 0..T {
                        acc[u] += c * xa[u];
                    }
                }
            }
            acc
        }

        struct StageState {
            x: Vec<f64>,
            skip: usize,
        }

        impl StageState {
            fn push(&mut self, st: &Stage, input: &[f64], out: &mut Vec<f64>) {
                self.x.extend_from_slice(input);
                let hist = st.k - 1;
                if self.x.len() <= hist {
                    return;
                }
                let n = self.x.len() - hist;
                let base = out.len();
                out.resize(base + n * st.r, 0.0);
                run_block(st, &self.x, &mut out[base..]);
                if self.skip > 0 {
                    let s = self.skip.min(n * st.r);
                    out.drain(base..base + s);
                    self.skip -= s;
                }
                self.x.drain(..n);
            }
        }

        /// `InterpChain` of 6b2bea4 for the chain `chain` with the plan's trims.
        pub struct Chain {
            stages: Vec<Stage>,
            states: Vec<StageState>,
            pub input_start: u64,
            a: Vec<f64>,
            b: Vec<f64>,
        }

        impl Chain {
            pub fn new(chain: &[H1Stage], trims: &[usize], start: u64) -> Chain {
                let stages: Vec<Stage> = chain.iter().zip(trims).map(|(s, &t)| Stage::new(s.taps, s.ratio, t)).collect();
                let mut states: Vec<StageState> = Vec::with_capacity(stages.len());
                let mut s = start as i64;
                for st in stages.iter().rev() {
                    let y = s + st.trim as i64;
                    let (n0, p0) = (y / st.r as i64, (y % st.r as i64) as usize);
                    let a = st.first_input(s).max(0);
                    let pad = (st.k as i64 - 1 - (n0 - a)) as usize;
                    states.push(StageState { x: vec![0.0; pad], skip: p0 });
                    s = a;
                }
                states.reverse();
                Chain { stages, states, input_start: s as u64, a: Vec::new(), b: Vec::new() }
            }

            pub fn process(&mut self, input: &[f64], out: &mut Vec<f64>) {
                let last = self.stages.len() - 1;
                let (mut a, mut b) = (std::mem::take(&mut self.a), std::mem::take(&mut self.b));
                for piece in input.chunks(CHUNK) {
                    if last == 0 {
                        self.states[0].push(&self.stages[0], piece, out);
                        continue;
                    }
                    a.clear();
                    self.states[0].push(&self.stages[0], piece, &mut a);
                    for i in 1..last {
                        b.clear();
                        self.states[i].push(&self.stages[i], &a, &mut b);
                        std::mem::swap(&mut a, &mut b);
                    }
                    self.states[last].push(&self.stages[last], &a, out);
                }
                self.a = a;
                self.b = b;
            }
        }
    }

    /// The shipped chains: (name, base rate, L, stages).
    fn shipped() -> Vec<(String, u32, usize, &'static [H1Stage])> {
        let mut v = Vec::new();
        for base in [44_100u32, 48_000] {
            for l in [2usize, 4, 8, 16, 32] {
                v.push((format!("{}k x{l}", base / 1000), base, l, h1_chain(base, l).unwrap()));
            }
        }
        v
    }

    #[test]
    fn the_fast_chain_gives_the_bits_of_the_first_one_on_every_shipped_chain() {
        for (name, _, l, chain) in shipped() {
            for &d in &[0usize, 5_000, 15_000_001] {
                let plan = MsPlan::new(d, l, 1.0, chain);
                let trims = plan.trims();
                let z = noise(30_000, (l * 31 + d) as u64);
                let n = (z.len() * l) as u64;
                for &chunk in &[1usize, 7, 4096, 8192, 10_000] {
                    for &start in &[0u64, 1, 7, 8, 63, 1001, 4099, n / 2 + 3, n - 1 - n / 7] {
                        let mut c = InterpChain::new(&plan, start);
                        let mut f = frozen::Chain::new(chain, &trims, start);
                        assert_eq!(c.input_start(), f.input_start, "{name} d {d} start {start}");
                        let (mut got, mut want) = (Vec::new(), Vec::new());
                        // Chunk 1 is slow through the frozen copy: a shorter stretch carries the same edges.
                        let src = &z[c.input_start() as usize..];
                        let src = if chunk == 1 { &src[..src.len().min(3000)] } else { src };
                        for piece in src.chunks(chunk) {
                            c.process(piece, &mut got);
                            f.process(piece, &mut want);
                        }
                        assert_eq!(got.len(), want.len(), "{name} d {d} chunk {chunk} start {start}");
                        assert!(!got.is_empty());
                        assert_eq!(bits(&got), bits(&want), "{name} d {d} chunk {chunk} start {start}");
                    }
                }
            }
        }
    }

    /// H1 alone, every shipped chain, 44.1/48k stereo (both channels one after the other on one thread: the CPU
    /// cost), 10 s, pieces of 4096 stage-0 frames: ms per audio second, median of 7 (min–max), the first chain
    /// (`frozen`) and today's alternating so both see the same machine. Pin it from outside (process affinity) for
    /// the E-cores.
    ///
    ///     cargo test --profile fast --bins h1_chain_speed -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn h1_chain_speed() {
        const SECS: usize = 10;
        const PIECE: usize = 4096;
        fn run(plan: &MsPlan, chain: &[H1Stage], z: &[f64], old: bool) -> usize {
            let mut out = Vec::with_capacity(z.len() * plan.l + 8192 * plan.l);
            if old {
                let mut c = frozen::Chain::new(chain, &plan.trims(), 0);
                for piece in z.chunks(PIECE) {
                    c.process(piece, &mut out);
                }
            } else {
                let mut c = InterpChain::new(plan, 0);
                for piece in z.chunks(PIECE) {
                    c.process(piece, &mut out);
                }
            }
            out.len()
        }
        fn median(mut v: Vec<f64>) -> (f64, f64, f64) {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (v[v.len() / 2], v[0], v[v.len() - 1])
        }
        println!("| chain | mults/frame | before ms/s | after ms/s | after/before |");
        println!("|---|---|---|---|---|");
        for (name, base, l, chain) in shipped() {
            let plan = MsPlan::new(15_000_001, l, 1.0, chain);
            let frames = base as usize * SECS;
            let (zl, zr) = (noise(frames, 1), noise(frames, 2));
            let _ = run(&plan, chain, &zl[..frames / 10], true) + run(&plan, chain, &zl[..frames / 10], false);
            let (mut old, mut new) = (Vec::new(), Vec::new());
            for _ in 0..7 {
                for (is_old, v) in [(true, &mut old), (false, &mut new)] {
                    let t = std::time::Instant::now();
                    let n = run(&plan, chain, &zl, is_old) + run(&plan, chain, &zr, is_old);
                    v.push(t.elapsed().as_secs_f64() * 1e3 / SECS as f64);
                    // The chain's trims hold back up to a few hundred source frames' worth.
                    assert!(n >= 2 * (frames - 1000) * l);
                }
            }
            let ((m0, a0, b0), (m1, a1, b1)) = (median(old), median(new));
            println!(
                "| {name} | {} | {m0:.2} ({a0:.2}–{b0:.2}) | {m1:.2} ({a1:.2}–{b1:.2}) | {:.3} |",
                plan.mults_per_source_frame(),
                m1 / m0
            );
        }
    }
}
