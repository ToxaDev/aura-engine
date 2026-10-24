//! Look-ahead one-bit modulators: the loop filter of the plain modulator,
//! but each bit is chosen by a search over many candidate bit sequences
//! instead of by the quantizer's sign alone (the M-algorithm: Kato; Janssen &
//! Reefman, "efficient trellis" sigma-delta modulation).
//!
//! Every sample, each of the `M` surviving paths is extended by both bits;
//! the `M` cheapest of the `2M` children survive. A path's cost is the sum of
//! its squared quantizer errors `(v − y)²`, where `v` is the quantizer input
//! (loop filter output + input + dither) and `y = ±1` the chosen bit: the
//! error the plain quantizer minimises one sample at a time, minimised over
//! the whole path. It needs only `v`, so only survivors update their states.
//! The bit of sample `t` is committed at `t + N − 1` from the cheapest path,
//! and paths that disagree with it are dropped; `N − 1` is a whole number of
//! bytes, and the first `(N − 1) / 8` bytes after a reset are DSD silence.
//!
//! The search lets the loop stay stable with a more aggressive noise
//! transfer function than the plain quantizer tolerates (a higher
//! out-of-band gain, so deeper noise in the band), and lets it run nearer
//! full modulation.
//!
//! Paths are kept as columns (`x[state][path]`), so the arithmetic runs on
//! all paths at once; on x86-64 with AVX2 the same code is compiled a second
//! time for it and chosen at construction. There is no fused multiply-add
//! anywhere, so both give the same bytes.

mod simd;
mod tables;
#[cfg(test)]
mod tests;
mod trellis;

use super::interp::Interpolator;
use crate::audio::dsd::{
    encoder_ratio, DsdEncoder, DsdError, DsdRate, ModulatorKind, NoiseCurve, DSD_SILENCE,
    PCM_FULL_SCALE_MODULATION,
};
use tables::{LaCoefs, LA};
use trellis::Trellis;

/// The search of every look-ahead kind at a rate multiple, chosen by the
/// study in `tests.rs` (`lookahead_study`). The real-time kinds keep one
/// channel within about half a core (LA+ ~80 %): the search narrows as the
/// bit rate grows.
///
/// SDM5 LA: 8 paths, 48 samples deep, the NTF at an out-of-band gain of 1.7
/// (SDM5: 1.5). Against SDM5 at DSD64/128/256: N+D at −6 dBFS 13 dB lower,
/// silence 5 dB lower, stable to 0.60–0.62 of full modulation (SDM5 0.56);
/// 36 % of one core per channel at DSD256. Gain 1.6 would be stable to
/// ~0.68 but 3.5 dB less deep; 16 paths add ~1 dB for three times the CPU.
///
/// Every other setup takes the highest gain whose maximum stable amplitude
/// (the least of 1 kHz, 10 kHz and DC, 0.1–0.3 s runs) is at least 0.60, over
/// the input limit 0.57. Above DSD256 the real-time kinds keep 4 paths (2
/// paths never reach 0.60 and run slower than 4). SDM7 LA (8 paths, 1.55)
/// against SDM7 at DSD64: N+D at −6 dBFS 12.5 dB lower, silence 5.5 dB lower,
/// stable to 0.64 (SDM7 0.595); at DSD256 silence 6 dB lower. SDM7 LA+ (16
/// paths, 1.65) adds 7 dB at DSD64/128; above that its search would not fit
/// a core, so it runs as SDM7 LA. SDM9 (DSD512 up) sits at the measurement
/// floor (−207…−213 dB) already: its search only adds stability (0.64 vs
/// 0.54).
///
/// The converter grades search deeper than real time allows: 32 paths at
/// DSD64/128, 16 at DSD256/512, 8 at DSD1024, within ~330 ns per bit per
/// channel at DSD256 (a 4-minute track in ~15 minutes, a channel per core).
/// Encoding in blocks is not possible: a splice after any warm-up leaves an
/// error of −35…−68 dB re full scale (the loops never converge on one
/// state), so a channel is one sequential search. The Viterbi trellis
/// (`trellis.rs`) lost to the M-algorithm at equal CPU (64 states: stable to
/// 0.54 against 0.62 for 32 paths), so the grades named Trellis run the
/// M-algorithm.
pub(crate) fn setup_for(kind: ModulatorKind, multiple: u32) -> Option<LaSetup> {
    use ModulatorKind as K;
    let info = kind.info();
    let (paths, depth, gain) = match (kind, multiple) {
        (K::Sdm5La, ..=256) => (8, 49, 1.7),
        (K::Sdm5La, _) => (4, 49, 1.5),
        (K::Sdm7La, ..=256) => (8, 49, 1.55),
        (K::Sdm7La, _) => (4, 49, 1.45),
        (K::Sdm7WideLa, ..=128) => (8, 49, 1.6),
        (K::Sdm7WideLa, 256) => (8, 49, 1.55),
        (K::Sdm7WideLa, _) => (4, 49, 1.5),
        (K::Sdm7LaPlus, ..=128) => (16, 57, 1.65),
        (K::Sdm7LaPlus, 256) => (8, 49, 1.55),
        (K::Sdm7LaPlus, _) => (4, 49, 1.45),
        (K::Sdm9La, _) => (4, 49, 1.42),
        (K::Sdm7Trellis, ..=128) => (32, 57, 1.65),
        (K::Sdm7Trellis, 256) => (16, 57, 1.65),
        (K::Sdm7Trellis, 512) => (16, 57, 1.6),
        (K::Sdm7Trellis, _) => (8, 49, 1.55),
        (K::Sdm9Trellis, ..=512) => (16, 57, 1.55),
        (K::Sdm9Trellis, _) => (8, 49, 1.5),
        _ => return None,
    };
    Some(LaSetup {
        order: info.order as usize,
        wide: info.curve == NoiseCurve::Wide,
        paths: paths.max(1),
        depth,
        gain,
        limit: 0.57,
        dither: DITHER,
        trellis: false,
    })
}

pub(crate) fn new_encoder(
    kind: ModulatorKind,
    input_rate: u32,
    rate: DsdRate,
) -> Result<Box<dyn DsdEncoder>, DsdError> {
    match setup_for(kind, rate.multiple) {
        Some(setup) if kind.supports(rate) => Ok(Box::new(LaEncoder::new(input_rate, rate, &setup)?)),
        _ => Err(DsdError::UnsupportedModulator { kind, multiple: rate.multiple }),
    }
}

/// A path whose states leave this is dropped; the best path leaving it
/// restarts the loop from zero (counted as an overload). The states are
/// scaled to about 1.0 at full use (see `tools/dsd/lookahead_design.py`).
pub(crate) const STATE_LIMIT: f64 = 4.0;

/// Peak of the triangular dither added to `v`, of the one-bit level (as SDM5).
pub(crate) const DITHER: f64 = super::modulator::DITHER;
const DITHER_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// The cost of a dropped path: far above any live one, still finite.
const DEAD: f64 = 1e30;

/// Low mantissa bits of a candidate's cost that carry its index: positive
/// doubles order as their bit patterns, so a tagged cost sorts by cost with
/// ties broken by index, deterministically. 7 bits: up to 128 candidates.
const TAG_BITS: u32 = 7;
const TAG_MASK: u64 = (1 << TAG_BITS) - 1;

/// The largest number of paths a search keeps.
pub(crate) const MAX_PATHS: usize = 64;

#[inline(always)]
fn tag(cost: f64, index: usize) -> f64 {
    f64::from_bits((cost.to_bits() & !TAG_MASK) | index as u64)
}

/// How one look-ahead modulator searches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LaSetup {
    /// Loop filter order: 5, 7 or 9.
    pub order: usize,
    /// The Wide noise curve's zeros.
    pub wide: bool,
    /// Paths kept (`M`): 1, 2, 4, 8, 16, 32 or 64.
    pub paths: usize,
    /// Decision depth `N`: a bit is committed `N − 1` samples late.
    /// `N ≡ 1 (mod 8)`, at most 57.
    pub depth: u32,
    /// Out-of-band gain of the noise transfer function (a set in `LA`).
    pub gain: f64,
    /// Input limit, of full one-bit modulation (PCM full scale is 0.5).
    pub limit: f64,
    /// Peak of the triangular dither, of the one-bit level.
    pub dither: f64,
    /// The trellis (one path per pattern of the last log2(paths) bits)
    /// instead of the M-algorithm.
    pub trellis: bool,
}

/// The loop filter of order N in the form the search needs: a new state is
/// `base(x) + bv·w`, with `w = u − y`. The update of the plain modulators
/// (`modulator.rs`): x1 integrates w; in each resonator (x_i, x_i+1) x_i
/// takes its old neighbours, x_i+1 the new x_i. `c` (N − 1 entries) and `g`
/// (one per resonator, indexed by i / 2) are padded with zeros to N.
#[derive(Clone, Copy)]
pub(crate) struct LoopN<const N: usize> {
    pub(crate) c: [f64; N],
    pub(crate) g: [f64; N],
    pub(crate) cout: f64,
    pub(crate) bv: [f64; N],
}

impl<const N: usize> LoopN<N> {
    pub(crate) fn new(k: &LaCoefs) -> Self {
        assert!(N % 2 == 1 && k.order == N && k.a.len() == N && k.c.len() == N - 1 && k.g.len() == N / 2);
        let mut lp = LoopN { c: [0.0; N], g: [0.0; N], cout: k.cout, bv: [0.0; N] };
        lp.c[..N - 1].copy_from_slice(k.c);
        lp.g[..N / 2].copy_from_slice(k.g);
        lp.bv[0] = k.a[0];
        let mut i = 1;
        while i < N {
            lp.bv[i] = k.a[i];
            lp.bv[i + 1] = k.a[i + 1] + lp.c[i] * k.a[i];
            i += 2;
        }
        lp
    }

    /// One path's states before w (the scalar searches; the vector kernel
    /// does the same per lane).
    #[inline(always)]
    fn base(&self, x: &[f64; N]) -> [f64; N] {
        let mut b = [0.0; N];
        b[0] = x[0];
        let mut i = 1;
        while i < N {
            let n = x[i] + self.c[i - 1] * x[i - 1] - self.g[i / 2] * x[i + 1];
            b[i] = n;
            b[i + 1] = x[i + 1] + self.c[i] * n;
            i += 2;
        }
        b
    }
}

/// One search over many paths of the loop filter: everything per sample.
/// The driver around it limits the input, makes the dither, packs the bytes
/// and restarts the loop when the best path runs away.
pub(crate) trait Kernel {
    /// Zero states, one live path holding `hist` (the bits still pending).
    fn restart(&mut self, hist: u64);
    /// The best path's bit history.
    fn best_hist(&self) -> u64;
    /// One sample for every path; returns the committed bit.
    fn step(&mut self, u: f64, d: f64) -> u64;
    /// Once per byte: drops paths whose states left `STATE_LIMIT`; false if
    /// the best one did.
    fn guard(&mut self) -> bool;
}

/// The M-algorithm over `M` paths.
pub(crate) struct Search<const N: usize, const M: usize> {
    lp: LoopN<N>,
    x: [[f64; M]; N],
    cost: [f64; M],
    hist: [u64; M],
    /// N − 1: the bit leaving the decision window.
    shift: u32,
}

impl<const N: usize, const M: usize> Search<N, M> {
    pub(crate) fn new(lp: LoopN<N>, shift: u32) -> Self {
        assert!(M.is_power_of_two() && M <= MAX_PATHS);
        Search { lp, x: [[0.0; M]; N], cost: [DEAD; M], hist: [0; M], shift }
    }
}

impl<const N: usize, const M: usize> Kernel for Search<N, M> {
    fn restart(&mut self, hist: u64) {
        self.x = [[0.0; M]; N];
        self.cost = [DEAD; M];
        self.cost[0] = 0.0;
        self.hist = [hist; M];
    }

    fn best_hist(&self) -> u64 {
        self.hist[0]
    }

    #[inline(always)]
    fn step(&mut self, u: f64, d: f64) -> u64 {
        let (lp, cout, bv) = (self.lp, self.lp.cout, self.lp.bv);
        // Both children of every path: index 2m + bit.
        let mut cand = [[0.0f64; M]; 2];
        for m in 0..M {
            let v = cout * self.x[N - 1][m] + u + d;
            let (e0, e1) = (v + 1.0, v - 1.0);
            cand[0][m] = tag(self.cost[m] + e0 * e0, 2 * m);
            cand[1][m] = tag(self.cost[m] + e1 * e1, 2 * m + 1);
        }
        // Rank of every child among all 2M; the first M ranks survive, in
        // cost order, so slot 0 is the best path.
        let mut order = [0u8; 2 * MAX_PATHS];
        for b in 0..2 {
            for m in 0..M {
                let t = cand[b][m];
                let mut r = 0usize;
                for j in 0..M {
                    r += (cand[0][j] < t) as usize + (cand[1][j] < t) as usize;
                }
                order[r] = (2 * m + b) as u8;
            }
        }
        // The parents' states before w.
        let mut base = [[0.0f64; M]; N];
        for m in 0..M {
            let b = lp.base(&std::array::from_fn(|k| self.x[k][m]));
            for k in 0..N {
                base[k][m] = b[k];
            }
        }
        let mut nx = [[0.0f64; M]; N];
        let mut ncost = [0.0f64; M];
        let mut nhist = [0u64; M];
        let mut wv = [0.0f64; M];
        for j in 0..M {
            let i = order[j] as usize;
            let (p, bit) = (i >> 1, i & 1);
            for k in 0..N {
                nx[k][j] = base[k][p];
            }
            ncost[j] = cand[bit][p];
            nhist[j] = self.hist[p] << 1 | bit as u64;
            wv[j] = u - if bit == 1 { 1.0 } else { -1.0 };
        }
        for k in 0..N {
            for j in 0..M {
                nx[k][j] += bv[k] * wv[j];
            }
        }
        let best = ncost[0];
        let shift = self.shift;
        let committed = nhist[0] >> shift & 1;
        for j in 0..M {
            let agrees = nhist[j] >> shift & 1 == committed;
            ncost[j] = if agrees { ncost[j] - best } else { DEAD };
        }
        self.x = nx;
        self.cost = ncost;
        self.hist = nhist;
        committed
    }

    #[inline(always)]
    fn guard(&mut self) -> bool {
        let peak = |x: &[[f64; M]; N], j: usize| (0..N).fold(0.0f64, |p, k| p.max(x[k][j].abs()));
        if !(peak(&self.x, 0) <= STATE_LIMIT) {
            return false;
        }
        for j in 1..M {
            if !(peak(&self.x, j) <= STATE_LIMIT) {
                for k in 0..N {
                    self.x[k][j] = self.x[k][0];
                }
                self.hist[j] = self.hist[0];
                self.cost[j] = DEAD;
            }
        }
        true
    }
}

/// The object-safe face of a driven kernel.
pub(crate) trait LaLoop: Send {
    fn run(&mut self, u: &[f64], out: &mut [u8]);
    fn reset(&mut self);
    /// Input clamps and restarts, each counted once per episode.
    fn counts(&self) -> (u64, u64);
    fn overloads(&self) -> u64 {
        let (c, r) = self.counts();
        c + r
    }
    #[cfg(test)]
    fn force_portable(&mut self);
}

/// Input limit, dither, byte packing and the restart guard around a kernel.
struct Driver<K> {
    k: K,
    /// What every path's history starts with: (N − 1) / 8 silence bytes.
    preroll: u64,
    limit: f64,
    /// Dither peak / 65 536.
    scale: f64,
    rng: u64,
    limiting: bool,
    clamps: u64,
    resets: u64,
    avx2: bool,
}

impl<K: Kernel + Send> Driver<K> {
    fn new(k: K, setup: &LaSetup) -> Self {
        let preroll = (0..(setup.depth - 1) / 8).fold(0u64, |h, _| h << 8 | DSD_SILENCE as u64);
        let mut d = Driver {
            k,
            preroll,
            limit: setup.limit,
            scale: setup.dither / 65_536.0,
            rng: DITHER_SEED,
            limiting: false,
            clamps: 0,
            resets: 0,
            avx2: avx2_available(),
        };
        d.reset();
        d
    }

    #[inline(always)]
    fn run_inner(&mut self, u: &[f64], out: &mut [u8]) {
        debug_assert_eq!(u.len(), out.len() * 8);
        let (limit, scale) = (self.limit, self.scale);
        for (byte_out, eight) in out.iter_mut().zip(u.chunks_exact(8)) {
            let mut byte = 0u64;
            for pair in eight.chunks_exact(2) {
                let mut rng = self.rng;
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                self.rng = rng;
                let tri = |hi: u64, lo: u64| ((hi & 0xFFFF) as i32 - (lo & 0xFFFF) as i32) as f64 * scale;
                let d = [tri(rng >> 48, rng >> 32), tri(rng >> 16, rng)];
                for k in 0..2 {
                    let mut s = pair[k];
                    if s.abs() > limit {
                        if !self.limiting {
                            self.limiting = true;
                            self.clamps += 1;
                        }
                        s = limit.copysign(s);
                    } else {
                        self.limiting = false;
                    }
                    byte = byte << 1 | self.k.step(s, d[k]);
                }
            }
            *byte_out = byte as u8;
            if !self.k.guard() {
                let h = self.k.best_hist();
                self.k.restart(h);
                self.resets += 1;
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    fn run_avx2(&mut self, u: &[f64], out: &mut [u8]) {
        self.run_inner(u, out)
    }

    fn run_portable(&mut self, u: &[f64], out: &mut [u8]) {
        self.run_inner(u, out)
    }
}

fn avx2_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

impl<K: Kernel + Send> LaLoop for Driver<K> {
    fn run(&mut self, u: &[f64], out: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 was detected at construction.
            return unsafe { self.run_avx2(u, out) };
        }
        self.run_portable(u, out)
    }

    fn reset(&mut self) {
        self.k.restart(self.preroll);
        self.rng = DITHER_SEED;
        self.limiting = false;
    }

    fn counts(&self) -> (u64, u64) {
        (self.clamps, self.resets)
    }

    #[cfg(test)]
    fn force_portable(&mut self) {
        self.avx2 = false;
    }
}

/// The M-algorithm on 4-wide vectors (`simd.rs`), M = 4·R.
struct Fast<const N: usize, const R: usize> {
    paths: simd::Paths<N, R>,
    lp: LoopN<N>,
    front: simd::Front,
    preroll: u64,
    avx2: bool,
}

impl<const N: usize, const R: usize> Fast<N, R> {
    fn new(lp: LoopN<N>, setup: &LaSetup) -> Self {
        let mut f = Fast {
            paths: simd::Paths { x: [[[0.0; 4]; R]; N], cost: [[0.0; 4]; R], hist: [[0; 4]; R] },
            lp,
            front: simd::Front {
                limit: setup.limit,
                rng: DITHER_SEED,
                limiting: false,
                clamps: 0,
                resets: 0,
                shift: setup.depth - 1,
                scale: setup.dither / 65_536.0,
            },
            preroll: (0..(setup.depth - 1) / 8).fold(0u64, |h, _| h << 8 | DSD_SILENCE as u64),
            avx2: avx2_available(),
        };
        f.reset();
        f
    }
}

impl<const N: usize, const R: usize> LaLoop for Fast<N, R> {
    fn run(&mut self, u: &[f64], out: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 was detected at construction.
            return unsafe { simd::avx2::run(&mut self.paths, &self.lp, &mut self.front, u, out) };
        }
        simd::portable::run(&mut self.paths, &self.lp, &mut self.front, u, out)
    }

    fn reset(&mut self) {
        self.paths.restart(self.preroll);
        self.front.rng = DITHER_SEED;
        self.front.limiting = false;
    }

    fn counts(&self) -> (u64, u64) {
        (self.front.clamps, self.front.resets)
    }

    #[cfg(test)]
    fn force_portable(&mut self) {
        self.avx2 = false;
    }
}

pub(crate) fn new_search(k: &LaCoefs, setup: &LaSetup) -> Box<dyn LaLoop> {
    assert!(setup.depth % 8 == 1 && setup.depth <= 57, "depth {} must be 8k + 1 ≤ 57", setup.depth);
    match k.order {
        5 => search_n::<5>(k, setup),
        7 => search_n::<7>(k, setup),
        9 => search_n::<9>(k, setup),
        n => panic!("order {n}: no look-ahead loop"),
    }
}

fn search_n<const N: usize>(k: &LaCoefs, setup: &LaSetup) -> Box<dyn LaLoop> {
    let lp = LoopN::<N>::new(k);
    match (setup.trellis, setup.paths) {
        (false, 4) => Box::new(Fast::<N, 1>::new(lp, setup)),
        (false, 8) => Box::new(Fast::<N, 2>::new(lp, setup)),
        (false, 16) => Box::new(Fast::<N, 4>::new(lp, setup)),
        (false, 32) => Box::new(Fast::<N, 8>::new(lp, setup)),
        (false, 64) => Box::new(Fast::<N, 16>::new(lp, setup)),
        _ => reference_n::<N>(k, setup),
    }
}

/// The scalar M-algorithm (any M, one path per array slot) and the trellis.
#[cfg(test)]
pub(crate) fn new_reference_search(k: &LaCoefs, setup: &LaSetup) -> Box<dyn LaLoop> {
    match k.order {
        5 => reference_n::<5>(k, setup),
        7 => reference_n::<7>(k, setup),
        9 => reference_n::<9>(k, setup),
        n => panic!("order {n}: no look-ahead loop"),
    }
}

fn reference_n<const N: usize>(k: &LaCoefs, setup: &LaSetup) -> Box<dyn LaLoop> {
    let (lp, shift) = (LoopN::<N>::new(k), setup.depth - 1);
    fn drive<K: Kernel + Send + 'static>(k: K, setup: &LaSetup) -> Box<dyn LaLoop> {
        Box::new(Driver::new(k, setup))
    }
    match (setup.trellis, setup.paths) {
        (false, 1) => drive(Search::<N, 1>::new(lp, shift), setup),
        (false, 2) => drive(Search::<N, 2>::new(lp, shift), setup),
        (false, 4) => drive(Search::<N, 4>::new(lp, shift), setup),
        (false, 8) => drive(Search::<N, 8>::new(lp, shift), setup),
        (false, 16) => drive(Search::<N, 16>::new(lp, shift), setup),
        (false, 32) => drive(Search::<N, 32>::new(lp, shift), setup),
        (false, 64) => drive(Search::<N, 64>::new(lp, shift), setup),
        (true, 4) => drive(Trellis::<N, 4>::new(lp, shift), setup),
        (true, 8) => drive(Trellis::<N, 8>::new(lp, shift), setup),
        (true, 16) => drive(Trellis::<N, 16>::new(lp, shift), setup),
        (true, 32) => drive(Trellis::<N, 32>::new(lp, shift), setup),
        (true, 64) => drive(Trellis::<N, 64>::new(lp, shift), setup),
        (true, 128) => drive(Trellis::<N, 128>::new(lp, shift), setup),
        (true, 256) => drive(Trellis::<N, 256>::new(lp, shift), setup),
        (t, n) => panic!("{n} paths (trellis {t}): not a supported search width"),
    }
}

/// The loop filter set of an order and curve for a rate and an out-of-band
/// gain. The 48 kHz family uses the same sets.
pub(crate) fn coefs(order: usize, wide: bool, multiple: u32, gain: f64) -> &'static LaCoefs {
    LA.iter()
        .find(|k| k.order == order && k.wide == wide && k.multiple == multiple && (k.gain - gain).abs() < 1e-9)
        .unwrap_or_else(|| panic!("no look-ahead set for order {order} (wide {wide}) at DSD{multiple}, gain {gain}"))
}

/// The set a setup runs on at a rate.
pub(crate) fn coefs_of(setup: &LaSetup, multiple: u32) -> &'static LaCoefs {
    coefs(setup.order, setup.wide, multiple, setup.gain)
}

/// For the measurement suite (`measure.rs`): a look-ahead kind's search over
/// `u` (at the bit rate, a multiple of 8 samples) with input limit `limit`,
/// aligned with `u` (the decision delay dropped, the tail flushed with
/// zeros). The bytes and (clamps, restarts); None for other kinds.
#[cfg(test)]
pub(crate) fn raw_run(kind: ModulatorKind, multiple: u32, u: &[f64], limit: f64) -> Option<(Vec<u8>, (u64, u64))> {
    let setup = LaSetup { limit, ..setup_for(kind, multiple)? };
    let mut s = new_search(coefs_of(&setup, multiple), &setup);
    let delay = (setup.depth as usize - 1) / 8;
    let mut padded = u.to_vec();
    padded.resize(u.len() + 8 * delay, 0.0);
    let mut out = vec![0u8; padded.len() / 8];
    for (uu, oo) in padded.chunks(8 * 4096).zip(out.chunks_mut(4096)) {
        s.run(uu, oo);
    }
    out.drain(..delay);
    Some((out, s.counts()))
}

/// For the measurement suite: the band edge of a look-ahead kind's set.
#[cfg(test)]
pub(crate) fn band_edge(kind: ModulatorKind, multiple: u32) -> Option<f64> {
    setup_for(kind, multiple).map(|s| coefs_of(&s, multiple).band_edge)
}

/// PCM → DSD for one channel with a look-ahead modulator.
///
/// The search delays its bits by N − 1 (whole bytes); when that is not a
/// whole number of input samples, `pad` more silence bytes go first (a byte
/// delay line), so the output for input sample k starts at bit
/// `(k + latency) · ratio` as the contract asks.
pub(crate) struct LaEncoder {
    ratio: usize,
    interp: Interpolator,
    search: Box<dyn LaLoop>,
    latency: usize,
    pad: usize,
    delay: [u8; PAD_MAX],
    at: usize,
}

/// The longest byte delay line: one input sample at the largest ratio (1024).
const PAD_MAX: usize = 128;

impl LaEncoder {
    pub(crate) fn new(input_rate: u32, rate: DsdRate, setup: &LaSetup) -> Result<Self, DsdError> {
        let ratio = encoder_ratio(input_rate, rate)?;
        let bits = setup.depth as usize - 1;
        let latency = bits.div_ceil(ratio);
        let pad = (latency * ratio - bits) / 8;
        assert!(pad <= PAD_MAX);
        Ok(LaEncoder {
            ratio,
            interp: Interpolator::new(ratio, input_rate as f64 / rate.base as f64),
            search: new_search(coefs_of(setup, rate.multiple), setup),
            latency,
            pad,
            delay: [DSD_SILENCE; PAD_MAX],
            at: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn force_portable(&mut self) {
        self.search.force_portable();
    }
}

impl DsdEncoder for LaEncoder {
    fn ratio(&self) -> usize {
        self.ratio
    }

    fn process(&mut self, pcm: &[f64], out: &mut [u8]) {
        assert_eq!(out.len(), pcm.len() * self.ratio / 8);
        let block = self.interp.block_inputs();
        for (part, bytes) in pcm.chunks(block).zip(out.chunks_mut(block * self.ratio / 8)) {
            let up = self.interp.process(part, |s| {
                if s.is_nan() { 0.0 } else { s.clamp(-4.0, 4.0) * PCM_FULL_SCALE_MODULATION }
            });
            self.search.run(up, bytes);
        }
        if self.pad > 0 {
            let (delay, mut at) = (&mut self.delay[..self.pad], self.at);
            for b in out.iter_mut() {
                std::mem::swap(b, &mut delay[at]);
                at = if at + 1 == delay.len() { 0 } else { at + 1 };
            }
            self.at = at;
        }
    }

    fn reset(&mut self) {
        self.interp.reset();
        self.search.reset();
        self.delay = [DSD_SILENCE; PAD_MAX];
        self.at = 0;
    }

    fn overloads(&self) -> u64 {
        self.search.overloads()
    }

    fn latency(&self) -> usize {
        self.latency
    }
}
