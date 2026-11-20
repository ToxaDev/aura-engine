//! The M-algorithm for 8 paths in single precision: every register holds one
//! value of all eight paths, and the search is laid out for the latency of
//! its loop, which is what bounds a sequential search (`simd.rs` is bound by
//! the chain cost → sort → permute → state → cost, ~100 cycles a bit).
//!
//! What is different from `simd.rs`:
//!
//! * Costs are 32-bit integers (2²⁰ to one unit of squared error, the low
//!   five bits a tag), so the compare-exchange of the sort is `pminsd` /
//!   `pmaxsd` (one cycle, against four for `minpd`). The sixteen children are
//!   two lists of eight, one per 128-bit lane (the children of paths 0–3 and
//!   of paths 4–7), each sorted by a six-layer network whose comparators all
//!   sit between the two registers, so a layer is one min, one max and two
//!   in-lane shuffles; the eight cheapest are the pairwise minima of one list
//!   ascending against the other descending.
//! * The squared errors of the NEXT sample are computed for both children of
//!   every path while the sort runs, and gathered with the states, so after
//!   the selection a candidate's cost is one integer add away.
//! * The tag of a candidate carries the bit its path would commit, so the
//!   committed bit and the paths to drop come from the cheapest candidate's
//!   tag, with no trip through a scalar register.
//! * The states are `f32`, with the two places where single precision would
//!   be heard kept exact. Rounding in a state reaches the output shaped by
//!   the integrators before it, so only the first resonator matters:
//!   - x0 is `a0·Σ(u − y)`. It is not accumulated per path: Σu is one scalar
//!     double for all paths (`gr`), Σy an exact small integer per path (`D`),
//!     and `q = gr − D` is formed afresh every sample.
//!   - x1 is kept as `z + a1·q`; `z` takes only the small terms
//!     (`c0·x0 − g0·x2`) and is summed with its rounding error carried
//!     (two-sum), so nothing accumulates at x1's own magnitude either.
//!   The other states' rounding is shaped by two integrators or more and
//!   lies far below the quantization noise at every rate.
//!
//! One kernel (`kernel!`), built on AVX2 + FMA and on plain arrays with the
//! same lane semantics (`f32::mul_add` for the fused operations), so both
//! give the same bytes.

use super::simd::Front;
use super::{LaLoop, LaSetup, LoopN, DITHER_SEED, STATE_LIMIT};
use crate::audio::dsd::DSD_SILENCE;

/// A dropped path's cost: above any live one.
const DEAD: i32 = 1 << 30;
/// Costs are capped here, so a dropped path's children cannot overflow.
const CAP: i32 = DEAD + (1 << 29);
/// The tag: bit 0 the child's bit, bits 1–3 its parent, bit 4 the bit its
/// path commits if it is the cheapest.
const TAG: i32 = 31;
/// √(cost units per unit of squared error): v is carried times this.
const V_SCALE: f32 = 1024.0;
/// The part of v the states give is limited to this for the cost, so with
/// the input's part (`Coefs::new` checks it) |v| + 1 stays under 16 and a
/// squared error under 2²⁸.
const V_MAX: f32 = 13.0 * V_SCALE;

/// The loop filter in single precision, as the kernel uses it.
#[derive(Clone, Copy)]
pub(crate) struct Coefs<const N: usize> {
    a0: f32,
    a1: f32,
    /// c0·a0: x0's weight in the first resonator, on `q`.
    ca: f32,
    c: [f32; N],
    g: [f32; N],
    bv: [f32; N],
    /// cout · V_SCALE.
    sc: f32,
    /// cout · V_SCALE · bv of the last state: w's weight in the next v.
    kw: f64,
}

impl<const N: usize> Coefs<N> {
    fn new(lp: &LoopN<N>) -> Self {
        let kw = lp.cout * V_SCALE as f64 * lp.bv[N - 1];
        assert!(kw.abs() * 1.6 <= 1.9 * V_SCALE as f64, "the input's part of v is over the cost's range");
        Coefs {
            kw,
            a0: lp.bv[0] as f32,
            a1: lp.bv[1] as f32,
            ca: (lp.c[0] * lp.bv[0]) as f32,
            c: lp.c.map(|v| v as f32),
            g: lp.g.map(|v| v as f32),
            bv: lp.bv.map(|v| v as f32),
            sc: (lp.cout * V_SCALE as f64) as f32,
        }
    }
}

/// What the search keeps between calls.
#[derive(Clone, Copy)]
pub(crate) struct Paths<const N: usize> {
    /// `x[0]` is D (Σy less the part taken out of `gr`), `x[1]` z, the rest
    /// the states themselves.
    x: [[f32; 8]; N],
    zlo: [f32; 8],
    cost: [i32; 8],
    hlo: [i32; 8],
    hhi: [i32; 8],
    /// Σu, less the whole numbers moved into D.
    gr: f64,
}

impl<const N: usize> Paths<N> {
    /// Zero states, the first path live, every path holding the history
    /// (`hlo`, `hhi`: left-aligned, its oldest pending bit at bit 62).
    fn restart(&mut self, hlo: i32, hhi: i32) {
        self.x = [[0.0; 8]; N];
        self.zlo = [0.0; 8];
        self.cost = [DEAD; 8];
        self.cost[0] = 0;
        self.hlo = [hlo; 8];
        self.hhi = [hhi; 8];
        self.gr = 0.0;
    }

    fn best(&self) -> usize {
        (0..8).fold(0, |b, j| if self.cost[j] < self.cost[b] { j } else { b })
    }

    fn peak(&self, k: &Coefs<N>, j: usize) -> f32 {
        let q = self.gr as f32 - self.x[0][j];
        let x1 = k.a1.mul_add(q, self.x[1][j]);
        (2..N).fold((k.a0 * q).abs().max(x1.abs()), |m, i| m.max(self.x[i][j].abs()))
    }

    /// Paths whose states left the limit are dropped; false if the best did.
    fn guard(&mut self, k: &Coefs<N>) -> bool {
        let b = self.best();
        let limit = STATE_LIMIT as f32;
        if !(self.peak(k, b) <= limit) {
            return false;
        }
        for j in 0..8 {
            if !(self.peak(k, j) <= limit) {
                for i in 0..N {
                    self.x[i][j] = self.x[i][b];
                }
                self.zlo[j] = self.zlo[b];
                self.hlo[j] = self.hlo[b];
                self.hhi[j] = self.hhi[b];
                self.cost[j] = DEAD;
            }
        }
        true
    }

    fn best_hist(&self) -> (i32, i32) {
        let b = self.best();
        (self.hlo[b], self.hhi[b])
    }
}

macro_rules! kernel {
    ($(#[$attr:meta])*) => {
        /// The squared error, in cost units (tag bits not cleared), of a
        /// scaled quantizer input less the scaled bit.
        #[inline]
        $(#[$attr])*
        fn squared(e: F) -> I {
            cvt(mul(e, e))
        }

        /// The eight cheapest of the sixteen candidates (in no order) and
        /// the cheapest in every lane.
        #[inline]
        $(#[$attr])*
        fn select(k0: I, k1: I) -> (I, I) {
            // Each 128-bit lane: one list of eight, four in `a`, four in `b`.
            let (lo, hi) = (min_i(k0, k1), max_i(k0, k1));
            // Pairs (0,2) and (1,3) merged: (lo0,hi0,lo1,hi1) against (lo2,hi2,lo3,hi3).
            let (a, b) = (unpacklo(lo, hi), unpackhi(lo, hi));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            // The middle two of each four.
            let b = shuf::<0xB1>(hi);
            let (lo, hi) = (min_i(lo, b), max_i(lo, b));
            // Two sorted fours (lo0,lo1,hi1,hi0) and (lo2,lo3,hi3,hi2): the
            // bitonic merge, the second reversed.
            let (a, b) = (shuf2::<0x14>(lo, hi), shuf2::<0xBE>(hi, lo));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            let (a, b) = (shuf2::<0xB4>(lo, hi), shuf2::<0x1E>(lo, hi));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            let (a, b) = (shuf2::<0x88>(lo, hi), shuf2::<0xDD>(lo, hi));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            // Sorted: lo = (s0,s4,s2,s6), hi = (s1,s5,s3,s7) in each lane.
            // List X (lane 0) ascending against list Y (lane 1) descending.
            let p = perm2::<0x20>(lo, hi);
            let q = shuf::<0x1B>(perm2::<0x31>(hi, lo));
            let best = shuf::<0x00>(min_i(lo, perm2::<0x01>(lo, lo)));
            (min_i(p, q), best)
        }

        $(#[$attr])*
        pub(super) fn run<const N: usize>(p: &mut Paths<N>, k: &Coefs<N>, f: &mut Front, u: &[f64], out: &mut [u8]) {
            let scale = f.scale;
            let limit = f.limit;
            let mut c = [splat(0.0); N];
            let mut g = [splat(0.0); N];
            let mut bv = [splat(0.0); N];
            for i in 0..N {
                (c[i], g[i], bv[i]) = (splat(k.c[i]), splat(k.g[i]), splat(k.bv[i]));
            }
            let (a0, a1, ca, sc, kw) = (splat(k.a0), splat(k.a1), splat(k.ca), splat(k.sc), k.kw);
            let (v_max, v_min, v_one) = (splat(V_MAX), splat(-V_MAX), splat(V_SCALE));
            let (minus_one, state_limit) = (splat(-1.0), splat(STATE_LIMIT as f32));
            let (clear, cap, sixteen) = (splat_i(!TAG), splat_i(CAP), splat_i(16));
            let tag0 = lanes_i([0, 2, 4, 6, 8, 10, 12, 14]);
            let tag1 = lanes_i([1, 3, 5, 7, 9, 11, 13, 15]);
            // Where a new bit enters the (left-aligned) history.
            let new_bit = splat_i(1 << (63 - f.shift));

            let mut x = [splat(0.0); N];
            for i in 0..N {
                x[i] = load(&p.x[i]);
            }
            let mut zlo = load(&p.zlo);
            let mut cost = load_i(&p.cost);
            let mut hlo = load_i(&p.hlo);
            let mut hhi = load_i(&p.hhi);
            let mut gr = p.gr;

            for (byte_out, eight) in out.iter_mut().zip(u.chunks_exact(8)) {
                // Whole numbers out of Σu into D: both stay small.
                let whole = gr.round();
                gr -= whole;
                x[0] = sub(x[0], splat(whole as f32));
                // The input limit and the dither, for the byte's 8 samples.
                let mut s8 = [0.0f64; 8];
                if eight.iter().fold(false, |a, s| a | (s.abs() > limit)) {
                    for i in 0..8 {
                        let mut s = eight[i];
                        if s.abs() > limit {
                            if !f.limiting {
                                f.limiting = true;
                                f.clamps += 1;
                            }
                            s = limit.copysign(s);
                        } else {
                            f.limiting = false;
                        }
                        s8[i] = s;
                    }
                } else {
                    s8.copy_from_slice(eight);
                    f.limiting = false;
                }
                let mut d8 = [0.0f64; 8];
                for pi in 0..4 {
                    let mut rng = f.rng;
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    f.rng = rng;
                    let tri = |hi: u64, lo: u64| ((hi & 0xFFFF) as i32 - (lo & 0xFFFF) as i32) as f64 * scale;
                    d8[2 * pi] = tri(rng >> 48, rng >> 32);
                    d8[2 * pi + 1] = tri(rng >> 16, rng);
                }
                // us: the input; ud: input + dither, scaled; gr: Σu before
                // the sample; kv[child bit][next bit]: what w adds to the
                // next sample's v, less that sample's bit.
                let (mut usf, mut grf, mut udf) = ([0.0f32; 8], [0.0f32; 8], [0.0f32; 8]);
                let mut kv = [[[0.0f32; 8]; 2]; 2];
                for i in 0..8 {
                    usf[i] = s8[i] as f32;
                    udf[i] = ((s8[i] + d8[i]) * V_SCALE as f64) as f32;
                    grf[i] = gr as f32;
                    gr += s8[i];
                    let (w0, w1) = (kw * (s8[i] + 1.0), kw * (s8[i] - 1.0));
                    kv[0][0][i] = (w0 + V_SCALE as f64) as f32;
                    kv[0][1][i] = (w0 - V_SCALE as f64) as f32;
                    kv[1][0][i] = (w1 + V_SCALE as f64) as f32;
                    kv[1][1][i] = (w1 - V_SCALE as f64) as f32;
                }

                // The candidates of the byte's first sample, from the states.
                let v = max(min(fmadd(sc, x[N - 1], bload(&udf[0])), v_max), v_min);
                let cp = add_i(cost, and_i(srl::<26>(hhi), sixteen));
                let mut k0 = add_i(cp, add_i(and_i(squared(add(v, v_one)), clear), tag0));
                let mut k1 = add_i(cp, add_i(and_i(squared(sub(v, v_one)), clear), tag1));

                let mut byte = 0u32;
                for h in 0..8 {
                    let us = bload(&usf[h]);
                    let q = sub(bload(&grf[h]), x[0]);
                    // Every path's states before w (LoopN::base); z with its
                    // rounding error carried in zlo (exact when |z| ≥ |inc|,
                    // else to an ulp of inc, which is as far as inc itself is
                    // known).
                    let mut base = [splat(0.0); N];
                    let inc = add(fnmadd(g[0], x[2], mul(ca, q)), zlo);
                    let z = add(x[1], inc);
                    let zerr = sub(inc, sub(z, x[1]));
                    base[0] = x[0];
                    base[1] = z;
                    base[2] = fmadd(c[1], fmadd(a1, q, z), x[2]);
                    let mut i = 3;
                    while i < N {
                        let b = fnmadd(g[i / 2], x[i + 1], fmadd(c[i - 1], x[i - 1], x[i]));
                        base[i] = b;
                        base[i + 1] = fmadd(c[i], b, x[i + 1]);
                        i += 2;
                    }
                    // The next sample's squared errors for both children of
                    // every path (e[child bit][next bit]): its v is the
                    // states' part (limited) plus w's.
                    let zero = splat_i(0);
                    let (mut e00, mut e01, mut e10, mut e11) = (zero, zero, zero, zero);
                    if h < 7 {
                        let pv = max(min(fmadd(sc, base[N - 1], bload(&udf[h + 1])), v_max), v_min);
                        e00 = squared(add(pv, bload(&kv[0][0][h])));
                        e01 = squared(add(pv, bload(&kv[0][1][h])));
                        e10 = squared(add(pv, bload(&kv[1][0][h])));
                        e11 = squared(add(pv, bload(&kv[1][1][h])));
                    }

                    let (sv, best) = select(k0, k1);
                    let parent = srl::<1>(sv);
                    let bit = sll::<31>(sv);
                    let chosen = sra::<31>(bit);
                    byte = byte << 1 | (first(best) as u32 >> 4 & 1);
                    // Costs from the cheapest; paths that commit the other
                    // bit are dropped.
                    // (The tags' difference may borrow one unit of cost.)
                    let norm = min_i(and_i(sub_i(sv, best), clear), cap);
                    cost = max_i(norm, sll::<26>(and_i(xor_i(sv, best), sixteen)));

                    let y = xor_f(minus_one, bit);
                    let w = sub(us, y);
                    x[0] = add(perm(base[0], parent), y);
                    x[1] = perm(base[1], parent);
                    zlo = perm(zerr, parent);
                    for i in 2..N {
                        x[i] = fmadd(bv[i], w, perm(base[i], parent));
                    }
                    let (gl, gh) = (perm_i(hlo, parent), perm_i(hhi, parent));
                    hhi = or_i(sll::<1>(gh), srl::<31>(gl));
                    hlo = or_i(sll::<1>(gl), and_i(chosen, new_bit));

                    if h < 7 {
                        // The bit a path commits next: bit 62 of its history.
                        let cp = add_i(cost, and_i(srl::<25>(gh), sixteen));
                        let (is1, is0) = (and_i(chosen, clear), andnot_i(chosen, clear));
                        let g0 = or_i(and_i(is0, perm_i(e00, parent)), and_i(is1, perm_i(e10, parent)));
                        let g1 = or_i(and_i(is0, perm_i(e01, parent)), and_i(is1, perm_i(e11, parent)));
                        k0 = add_i(cp, add_i(g0, tag0));
                        k1 = add_i(cp, add_i(g1, tag1));
                    }
                }
                *byte_out = byte as u8;

                let q = sub(splat(gr as f32), x[0]);
                let mut bad = or_i(nle(abs(mul(a0, q)), state_limit), nle(abs(fmadd(a1, q, x[1])), state_limit));
                for i in 2..N {
                    bad = or_i(bad, nle(abs(x[i]), state_limit));
                }
                if mask(bad) != 0 {
                    for i in 0..N {
                        store(&mut p.x[i], x[i]);
                    }
                    store(&mut p.zlo, zlo);
                    store_i(&mut p.cost, cost);
                    store_i(&mut p.hlo, hlo);
                    store_i(&mut p.hhi, hhi);
                    p.gr = gr;
                    if !p.guard(k) {
                        let (hl, hh) = p.best_hist();
                        p.restart(hl, hh);
                        f.resets += 1;
                    }
                    for i in 0..N {
                        x[i] = load(&p.x[i]);
                    }
                    zlo = load(&p.zlo);
                    cost = load_i(&p.cost);
                    hlo = load_i(&p.hlo);
                    hhi = load_i(&p.hhi);
                    gr = p.gr;
                }
            }
            for i in 0..N {
                store(&mut p.x[i], x[i]);
            }
            store(&mut p.zlo, zlo);
            store_i(&mut p.cost, cost);
            store_i(&mut p.hlo, hlo);
            store_i(&mut p.hhi, hhi);
            p.gr = gr;
        }
    };
}

/// The 16-path search (SDM7 LA+): every per-path value in two registers
/// (`[_; 2]`, paths 0–7 and 8–15). What differs from the 8-path kernel:
///
/// * The tag is six bits: bit 0 the child's bit, bits 1–4 its parent, bit 5
///   the bit its path commits if it is the cheapest (`TAG16`).
/// * The selection keeps the 16 cheapest of 32 candidates (`select16`): the
///   two pairs of registers each sorted per 128-bit lane into lists of eight
///   (the 8-path network), then lists of sixteen merged lane by lane between
///   the pairs (lane 0's two lists, lane 1's), and the sixteen cheapest the
///   pairwise minima of lane 0's list ascending against lane 1's descending.
/// * A gather from 16 paths is two in-register permutes and a blend on the
///   parent's bit 3.
const TAG16: i32 = 63;

/// What the 16-path search keeps between calls: as `Paths`, two registers
/// of every value.
#[derive(Clone, Copy)]
pub(crate) struct Paths16<const N: usize> {
    x: [[[f32; 8]; 2]; N],
    zlo: [[f32; 8]; 2],
    cost: [[i32; 8]; 2],
    hlo: [[i32; 8]; 2],
    hhi: [[i32; 8]; 2],
    gr: f64,
}

impl<const N: usize> Paths16<N> {
    fn restart(&mut self, hlo: i32, hhi: i32) {
        self.x = [[[0.0; 8]; 2]; N];
        self.zlo = [[0.0; 8]; 2];
        self.cost = [[DEAD; 8]; 2];
        self.cost[0][0] = 0;
        self.hlo = [[hlo; 8]; 2];
        self.hhi = [[hhi; 8]; 2];
        self.gr = 0.0;
    }

    fn best(&self) -> (usize, usize) {
        let mut b = (0, 0);
        for r in 0..2 {
            for j in 0..8 {
                if self.cost[r][j] < self.cost[b.0][b.1] {
                    b = (r, j);
                }
            }
        }
        b
    }

    fn peak(&self, k: &Coefs<N>, (r, j): (usize, usize)) -> f32 {
        let q = self.gr as f32 - self.x[0][r][j];
        let x1 = k.a1.mul_add(q, self.x[1][r][j]);
        (2..N).fold((k.a0 * q).abs().max(x1.abs()), |m, i| m.max(self.x[i][r][j].abs()))
    }

    /// Paths whose states left the limit are dropped; false if the best did.
    fn guard(&mut self, k: &Coefs<N>) -> bool {
        let b = self.best();
        let limit = STATE_LIMIT as f32;
        if !(self.peak(k, b) <= limit) {
            return false;
        }
        for r in 0..2 {
            for j in 0..8 {
                if !(self.peak(k, (r, j)) <= limit) {
                    for i in 0..N {
                        self.x[i][r][j] = self.x[i][b.0][b.1];
                    }
                    self.zlo[r][j] = self.zlo[b.0][b.1];
                    self.hlo[r][j] = self.hlo[b.0][b.1];
                    self.hhi[r][j] = self.hhi[b.0][b.1];
                    self.cost[r][j] = DEAD;
                }
            }
        }
        true
    }

    fn best_hist(&self) -> (i32, i32) {
        let (r, j) = self.best();
        (self.hlo[r][j], self.hhi[r][j])
    }
}

macro_rules! kernel16 {
    ($(#[$attr:meta])*) => {
        /// Each 128-bit lane of (k0, k1) sorted, a list of eight: lo =
        /// (s0, s4, s2, s6), hi = (s1, s5, s3, s7). The first six layers of
        /// `select`.
        #[inline]
        $(#[$attr])*
        fn sort8(k0: I, k1: I) -> (I, I) {
            let (lo, hi) = (min_i(k0, k1), max_i(k0, k1));
            let (a, b) = (unpacklo(lo, hi), unpackhi(lo, hi));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            let b = shuf::<0xB1>(hi);
            let (lo, hi) = (min_i(lo, b), max_i(lo, b));
            let (a, b) = (shuf2::<0x14>(lo, hi), shuf2::<0xBE>(hi, lo));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            let (a, b) = (shuf2::<0xB4>(lo, hi), shuf2::<0x1E>(lo, hi));
            let (lo, hi) = (min_i(a, b), max_i(a, b));
            let (a, b) = (shuf2::<0x88>(lo, hi), shuf2::<0xDD>(lo, hi));
            (min_i(a, b), max_i(a, b))
        }

        /// A bitonic list of eight per lane, in the layout of `sort8`,
        /// sorted (distances 4, 2, 1; the layout comes back after three).
        #[inline]
        $(#[$attr])*
        fn merge8(mut lo: I, mut hi: I) -> (I, I) {
            for _ in 0..3 {
                let (a, b) = (shuf2::<0x88>(lo, hi), shuf2::<0xDD>(lo, hi));
                (lo, hi) = (min_i(a, b), max_i(a, b));
            }
            (lo, hi)
        }

        /// A value of the paths `p` name (0–15), `m` the sign of each one's
        /// bit 3: the register it is in.
        #[inline]
        $(#[$attr])*
        fn gather(v: [F; 2], p: I, m: I) -> F {
            blend(perm(v[0], p), perm(v[1], p), m)
        }
        #[inline]
        $(#[$attr])*
        fn gather_i(v: [I; 2], p: I, m: I) -> I {
            blend_i(perm_i(v[0], p), perm_i(v[1], p), m)
        }

        /// The sixteen cheapest of the 32 candidates (in no order, two
        /// registers) and the cheapest in every lane.
        #[inline]
        $(#[$attr])*
        fn select16(k0a: I, k1a: I, k0b: I, k1b: I) -> (I, I, I) {
            let (lo1, hi1) = sort8(k0a, k1a);
            let (lo2, hi2) = sort8(k0b, k1b);
            // The cheapest: the least of the four lists' first.
            let m = min_i(lo1, lo2);
            let best = shuf::<0x00>(min_i(m, perm2::<0x01>(m, m)));
            // Per lane, list A (pair 1) ascending then list B (pair 2)
            // descending is bitonic: the half-cleaner at distance 8 against
            // B reversed, then each half merged.
            let (blo, bhi) = (shuf::<0x1B>(hi2), shuf::<0x1B>(lo2));
            let (llo, lhi) = merge8(min_i(lo1, blo), min_i(hi1, bhi));
            let (hlo, hhi) = merge8(max_i(lo1, blo), max_i(hi1, bhi));
            // Sorted lists of sixteen: X = lane 0 of (L, H), Y = lane 1 of
            // (L, H). X ascending against Y descending.
            let r0 = min_i(perm2::<0x20>(llo, hlo), shuf::<0x1B>(perm2::<0x31>(hhi, lhi)));
            let r1 = min_i(perm2::<0x20>(lhi, hhi), shuf::<0x1B>(perm2::<0x31>(hlo, llo)));
            (r0, r1, best)
        }

        $(#[$attr])*
        pub(super) fn run16<const N: usize>(p: &mut Paths16<N>, k: &Coefs<N>, f: &mut Front, u: &[f64], out: &mut [u8]) {
            let scale = f.scale;
            let limit = f.limit;
            let mut c = [splat(0.0); N];
            let mut g = [splat(0.0); N];
            let mut bv = [splat(0.0); N];
            for i in 0..N {
                (c[i], g[i], bv[i]) = (splat(k.c[i]), splat(k.g[i]), splat(k.bv[i]));
            }
            let (a0, a1, ca, sc, kw) = (splat(k.a0), splat(k.a1), splat(k.ca), splat(k.sc), k.kw);
            let (v_max, v_min, v_one) = (splat(V_MAX), splat(-V_MAX), splat(V_SCALE));
            let (minus_one, state_limit) = (splat(-1.0), splat(STATE_LIMIT as f32));
            let (clear, cap, c32) = (splat_i(!TAG16), splat_i(CAP), splat_i(32));
            let tag0 = [lanes_i([0, 2, 4, 6, 8, 10, 12, 14]), lanes_i([16, 18, 20, 22, 24, 26, 28, 30])];
            let tag1 = [lanes_i([1, 3, 5, 7, 9, 11, 13, 15]), lanes_i([17, 19, 21, 23, 25, 27, 29, 31])];
            let new_bit = splat_i(1 << (63 - f.shift));

            let mut x = [[splat(0.0); 2]; N];
            for i in 0..N {
                for r in 0..2 {
                    x[i][r] = load(&p.x[i][r]);
                }
            }
            let mut zlo = [load(&p.zlo[0]), load(&p.zlo[1])];
            let mut cost = [load_i(&p.cost[0]), load_i(&p.cost[1])];
            let mut hlo = [load_i(&p.hlo[0]), load_i(&p.hlo[1])];
            let mut hhi = [load_i(&p.hhi[0]), load_i(&p.hhi[1])];
            let mut gr = p.gr;

            for (byte_out, eight) in out.iter_mut().zip(u.chunks_exact(8)) {
                let whole = gr.round();
                gr -= whole;
                for r in 0..2 {
                    x[0][r] = sub(x[0][r], splat(whole as f32));
                }
                let mut s8 = [0.0f64; 8];
                if eight.iter().fold(false, |a, s| a | (s.abs() > limit)) {
                    for i in 0..8 {
                        let mut s = eight[i];
                        if s.abs() > limit {
                            if !f.limiting {
                                f.limiting = true;
                                f.clamps += 1;
                            }
                            s = limit.copysign(s);
                        } else {
                            f.limiting = false;
                        }
                        s8[i] = s;
                    }
                } else {
                    s8.copy_from_slice(eight);
                    f.limiting = false;
                }
                let mut d8 = [0.0f64; 8];
                for pi in 0..4 {
                    let mut rng = f.rng;
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    f.rng = rng;
                    let tri = |hi: u64, lo: u64| ((hi & 0xFFFF) as i32 - (lo & 0xFFFF) as i32) as f64 * scale;
                    d8[2 * pi] = tri(rng >> 48, rng >> 32);
                    d8[2 * pi + 1] = tri(rng >> 16, rng);
                }
                let (mut usf, mut grf, mut udf) = ([0.0f32; 8], [0.0f32; 8], [0.0f32; 8]);
                let mut kv = [[[0.0f32; 8]; 2]; 2];
                for i in 0..8 {
                    usf[i] = s8[i] as f32;
                    udf[i] = ((s8[i] + d8[i]) * V_SCALE as f64) as f32;
                    grf[i] = gr as f32;
                    gr += s8[i];
                    let (w0, w1) = (kw * (s8[i] + 1.0), kw * (s8[i] - 1.0));
                    kv[0][0][i] = (w0 + V_SCALE as f64) as f32;
                    kv[0][1][i] = (w0 - V_SCALE as f64) as f32;
                    kv[1][0][i] = (w1 + V_SCALE as f64) as f32;
                    kv[1][1][i] = (w1 - V_SCALE as f64) as f32;
                }

                let mut k0 = [splat_i(0); 2];
                let mut k1 = [splat_i(0); 2];
                for r in 0..2 {
                    let v = max(min(fmadd(sc, x[N - 1][r], bload(&udf[0])), v_max), v_min);
                    let cp = add_i(cost[r], and_i(srl::<25>(hhi[r]), c32));
                    k0[r] = add_i(cp, add_i(and_i(squared(add(v, v_one)), clear), tag0[r]));
                    k1[r] = add_i(cp, add_i(and_i(squared(sub(v, v_one)), clear), tag1[r]));
                }

                let mut byte = 0u32;
                for h in 0..8 {
                    let us = bload(&usf[h]);
                    let mut base = [[splat(0.0); 2]; N];
                    let mut zerr = [splat(0.0); 2];
                    let zero = splat_i(0);
                    let mut e = [[[zero; 2]; 2]; 2];
                    for r in 0..2 {
                        let q = sub(bload(&grf[h]), x[0][r]);
                        let inc = add(fnmadd(g[0], x[2][r], mul(ca, q)), zlo[r]);
                        let z = add(x[1][r], inc);
                        zerr[r] = sub(inc, sub(z, x[1][r]));
                        base[0][r] = x[0][r];
                        base[1][r] = z;
                        base[2][r] = fmadd(c[1], fmadd(a1, q, z), x[2][r]);
                        let mut i = 3;
                        while i < N {
                            let b = fnmadd(g[i / 2], x[i + 1][r], fmadd(c[i - 1], x[i - 1][r], x[i][r]));
                            base[i][r] = b;
                            base[i + 1][r] = fmadd(c[i], b, x[i + 1][r]);
                            i += 2;
                        }
                        if h < 7 {
                            let pv = max(min(fmadd(sc, base[N - 1][r], bload(&udf[h + 1])), v_max), v_min);
                            e[0][0][r] = squared(add(pv, bload(&kv[0][0][h])));
                            e[0][1][r] = squared(add(pv, bload(&kv[0][1][h])));
                            e[1][0][r] = squared(add(pv, bload(&kv[1][0][h])));
                            e[1][1][r] = squared(add(pv, bload(&kv[1][1][h])));
                        }
                    }

                    let (s0, s1, best) = select16(k0[0], k1[0], k0[1], k1[1]);
                    byte = byte << 1 | (first(best) as u32 >> 5 & 1);
                    let y_us = us;
                    let (mut nlo, mut nhi) = ([zero; 2], [zero; 2]);
                    for (r, sv) in [s0, s1].into_iter().enumerate() {
                        let parent = srl::<1>(sv);
                        // Bit 3 of the parent (its register) to the sign.
                        let hi8 = sll::<27>(sv);
                        let bit = sll::<31>(sv);
                        let chosen = sra::<31>(bit);
                        let norm = min_i(and_i(sub_i(sv, best), clear), cap);
                        cost[r] = max_i(norm, sll::<25>(and_i(xor_i(sv, best), c32)));

                        let y = xor_f(minus_one, bit);
                        let w = sub(y_us, y);
                        x[0][r] = add(gather(base[0], parent, hi8), y);
                        x[1][r] = gather(base[1], parent, hi8);
                        zlo[r] = gather(zerr, parent, hi8);
                        for i in 2..N {
                            x[i][r] = fmadd(bv[i], w, gather(base[i], parent, hi8));
                        }
                        let (gl, gh) = (gather_i(hlo, parent, hi8), gather_i(hhi, parent, hi8));
                        // The other register still gathers from the old
                        // histories: the new ones wait in nlo / nhi.
                        nhi[r] = or_i(sll::<1>(gh), srl::<31>(gl));
                        nlo[r] = or_i(sll::<1>(gl), and_i(chosen, new_bit));
                        if h < 7 {
                            let cp = add_i(cost[r], and_i(srl::<24>(gh), c32));
                            let (is1, is0) = (and_i(chosen, clear), andnot_i(chosen, clear));
                            let g0 = or_i(and_i(is0, gather_i(e[0][0], parent, hi8)), and_i(is1, gather_i(e[1][0], parent, hi8)));
                            let g1 = or_i(and_i(is0, gather_i(e[0][1], parent, hi8)), and_i(is1, gather_i(e[1][1], parent, hi8)));
                            k0[r] = add_i(cp, add_i(g0, tag0[r]));
                            k1[r] = add_i(cp, add_i(g1, tag1[r]));
                        }
                    }
                    (hlo, hhi) = (nlo, nhi);
                }
                *byte_out = byte as u8;

                let mut bad = splat_i(0);
                for r in 0..2 {
                    let q = sub(splat(gr as f32), x[0][r]);
                    bad = or_i(bad, or_i(nle(abs(mul(a0, q)), state_limit), nle(abs(fmadd(a1, q, x[1][r])), state_limit)));
                    for i in 2..N {
                        bad = or_i(bad, nle(abs(x[i][r]), state_limit));
                    }
                }
                if mask(bad) != 0 {
                    for r in 0..2 {
                        for i in 0..N {
                            store(&mut p.x[i][r], x[i][r]);
                        }
                        store(&mut p.zlo[r], zlo[r]);
                        store_i(&mut p.cost[r], cost[r]);
                        store_i(&mut p.hlo[r], hlo[r]);
                        store_i(&mut p.hhi[r], hhi[r]);
                    }
                    p.gr = gr;
                    if !p.guard(k) {
                        let (hl, hh) = p.best_hist();
                        p.restart(hl, hh);
                        f.resets += 1;
                    }
                    for r in 0..2 {
                        for i in 0..N {
                            x[i][r] = load(&p.x[i][r]);
                        }
                        zlo[r] = load(&p.zlo[r]);
                        cost[r] = load_i(&p.cost[r]);
                        hlo[r] = load_i(&p.hlo[r]);
                        hhi[r] = load_i(&p.hhi[r]);
                    }
                    gr = p.gr;
                }
            }
            for r in 0..2 {
                for i in 0..N {
                    store(&mut p.x[i][r], x[i][r]);
                }
                store(&mut p.zlo[r], zlo[r]);
                store_i(&mut p.cost[r], cost[r]);
                store_i(&mut p.hlo[r], hlo[r]);
                store_i(&mut p.hhi[r], hhi[r]);
            }
            p.gr = gr;
        }
    };
}

/// Plain arrays with the AVX2 lane semantics.
mod portable {
    use super::*;

    type F = [f32; 8];
    type I = [i32; 8];

    #[inline(always)]
    fn map(a: F, b: F, f: impl Fn(f32, f32) -> f32) -> F {
        std::array::from_fn(|i| f(a[i], b[i]))
    }
    #[inline(always)]
    fn map_i(a: I, b: I, f: impl Fn(i32, i32) -> i32) -> I {
        std::array::from_fn(|i| f(a[i], b[i]))
    }
    #[inline(always)]
    fn splat(v: f32) -> F {
        [v; 8]
    }
    #[inline(always)]
    fn bload(v: &f32) -> F {
        [*v; 8]
    }
    #[inline(always)]
    fn splat_i(v: i32) -> I {
        [v; 8]
    }
    #[inline(always)]
    fn lanes_i(v: [i32; 8]) -> I {
        v
    }
    #[inline(always)]
    fn load(v: &[f32; 8]) -> F {
        *v
    }
    #[inline(always)]
    fn load_i(v: &[i32; 8]) -> I {
        *v
    }
    #[inline(always)]
    fn store(d: &mut [f32; 8], v: F) {
        *d = v;
    }
    #[inline(always)]
    fn store_i(d: &mut [i32; 8], v: I) {
        *d = v;
    }
    #[inline(always)]
    fn add(a: F, b: F) -> F {
        map(a, b, |x, y| x + y)
    }
    #[inline(always)]
    fn sub(a: F, b: F) -> F {
        map(a, b, |x, y| x - y)
    }
    #[inline(always)]
    fn mul(a: F, b: F) -> F {
        map(a, b, |x, y| x * y)
    }
    /// a·b + c, fused.
    #[inline(always)]
    fn fmadd(a: F, b: F, c: F) -> F {
        std::array::from_fn(|i| a[i].mul_add(b[i], c[i]))
    }
    /// c − a·b, fused.
    #[inline(always)]
    fn fnmadd(a: F, b: F, c: F) -> F {
        std::array::from_fn(|i| (-a[i]).mul_add(b[i], c[i]))
    }
    /// `minps`: the second operand unless the first is less.
    #[inline(always)]
    fn min(a: F, b: F) -> F {
        map(a, b, |x, y| if x < y { x } else { y })
    }
    #[inline(always)]
    fn max(a: F, b: F) -> F {
        map(a, b, |x, y| if x > y { x } else { y })
    }
    #[inline(always)]
    fn abs(a: F) -> F {
        a.map(f32::abs)
    }
    /// `cvtps2dq`: to the nearest, ties to even; 0x8000_0000 out of range.
    #[inline(always)]
    fn cvt(a: F) -> I {
        a.map(|x| if x >= -2_147_483_648.0 && x < 2_147_483_648.0 { x.round_ties_even() as i32 } else { i32::MIN })
    }
    #[inline(always)]
    fn andnot_i(m: I, a: I) -> I {
        map_i(m, a, |x, y| !x & y)
    }
    #[inline(always)]
    fn sra<const S: i32>(a: I) -> I {
        a.map(|x| x >> S)
    }
    /// The float's bits xor the mask's.
    #[inline(always)]
    fn xor_f(a: F, m: I) -> F {
        std::array::from_fn(|i| f32::from_bits(a[i].to_bits() ^ m[i] as u32))
    }
    #[inline(always)]
    fn and_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x & y)
    }
    #[inline(always)]
    fn or_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x | y)
    }
    #[inline(always)]
    fn xor_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x ^ y)
    }
    #[inline(always)]
    fn add_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x.wrapping_add(y))
    }
    #[inline(always)]
    fn sub_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x.wrapping_sub(y))
    }
    #[inline(always)]
    fn min_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x.min(y))
    }
    #[inline(always)]
    fn max_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x.max(y))
    }
    #[inline(always)]
    fn sll<const S: i32>(a: I) -> I {
        a.map(|x| ((x as u32) << S) as i32)
    }
    #[inline(always)]
    fn srl<const S: i32>(a: I) -> I {
        a.map(|x| ((x as u32) >> S) as i32)
    }
    #[inline(always)]
    fn first(a: I) -> i32 {
        a[0]
    }
    /// All ones where not a ≤ b (NaN included).
    #[inline(always)]
    fn nle(a: F, b: F) -> I {
        std::array::from_fn(|i| if a[i] <= b[i] { 0 } else { -1 })
    }
    #[inline(always)]
    fn mask(m: I) -> u32 {
        (0..8).fold(0, |acc, i| acc | ((m[i] as u32) >> 31) << i)
    }
    #[inline(always)]
    fn perm(a: F, p: I) -> F {
        p.map(|i| a[i as usize & 7])
    }
    #[inline(always)]
    fn perm_i(a: I, p: I) -> I {
        p.map(|i| a[i as usize & 7])
    }
    /// `blendvps`: `b` where the mask's sign bit is set, else `a`.
    #[inline(always)]
    fn blend(a: F, b: F, m: I) -> F {
        std::array::from_fn(|i| if m[i] < 0 { b[i] } else { a[i] })
    }
    #[inline(always)]
    fn blend_i(a: I, b: I, m: I) -> I {
        std::array::from_fn(|i| if m[i] < 0 { b[i] } else { a[i] })
    }
    #[inline(always)]
    fn unpacklo(a: I, b: I) -> I {
        [a[0], b[0], a[1], b[1], a[4], b[4], a[5], b[5]]
    }
    #[inline(always)]
    fn unpackhi(a: I, b: I) -> I {
        [a[2], b[2], a[3], b[3], a[6], b[6], a[7], b[7]]
    }
    /// `pshufd`: in each 128-bit lane, element i from element (M >> 2i) & 3.
    #[inline(always)]
    fn shuf<const M: i32>(a: I) -> I {
        std::array::from_fn(|i| a[(i & 4) + (M as usize >> (2 * (i & 3)) & 3)])
    }
    /// `shufps`: in each lane, two elements of `a`, then two of `b`.
    #[inline(always)]
    fn shuf2<const M: i32>(a: I, b: I) -> I {
        std::array::from_fn(|i| {
            let src = if i & 3 < 2 { &a } else { &b };
            src[(i & 4) + (M as usize >> (2 * (i & 3)) & 3)]
        })
    }
    /// `vperm2i128`: each half of the result is a half of `a` (0, 1) or `b` (2, 3).
    #[inline(always)]
    fn perm2<const M: i32>(a: I, b: I) -> I {
        std::array::from_fn(|i| {
            let sel = if i < 4 { M as usize & 3 } else { M as usize >> 4 & 3 };
            let src = if sel < 2 { &a } else { &b };
            src[(sel & 1) * 4 + (i & 3)]
        })
    }

    kernel!();
    kernel16!();

    #[cfg(test)]
    pub(crate) fn select_for_test(k0: [i32; 8], k1: [i32; 8]) -> ([i32; 8], [i32; 8]) {
        select(k0, k1)
    }

    #[cfg(test)]
    pub(crate) fn select16_for_test(k: [[i32; 8]; 4]) -> ([i32; 8], [i32; 8], [i32; 8]) {
        select16(k[0], k[1], k[2], k[3])
    }
}

/// AVX2 + FMA: `run` is called only when the CPU has both.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::*;
    use std::arch::x86_64::*;

    type F = __m256;
    type I = __m256i;

    macro_rules! op {
        ($(fn $name:ident$(<const $g:ident: i32>)?($($arg:ident: $ty:ty),*) -> $ret:ty $body:block)*) => {
            $(
                #[inline]
                #[target_feature(enable = "avx2,fma")]
                fn $name$(<const $g: i32>)?($($arg: $ty),*) -> $ret $body
            )*
        };
    }

    op! {
        fn splat(v: f32) -> F { _mm256_set1_ps(v) }
        fn bload(v: &f32) -> F {
            _mm256_broadcast_ss(v)
        }
        fn splat_i(v: i32) -> I { _mm256_set1_epi32(v) }
        fn lanes_i(v: [i32; 8]) -> I { _mm256_setr_epi32(v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]) }
        fn load(v: &[f32; 8]) -> F {
            // SAFETY: 32 readable bytes; unaligned load.
            unsafe { _mm256_loadu_ps(v.as_ptr()) }
        }
        fn load_i(v: &[i32; 8]) -> I {
            // SAFETY: 32 readable bytes; unaligned load.
            unsafe { _mm256_loadu_si256(v.as_ptr() as *const __m256i) }
        }
        fn add(a: F, b: F) -> F { _mm256_add_ps(a, b) }
        fn sub(a: F, b: F) -> F { _mm256_sub_ps(a, b) }
        fn mul(a: F, b: F) -> F { _mm256_mul_ps(a, b) }
        fn fmadd(a: F, b: F, c: F) -> F { _mm256_fmadd_ps(a, b, c) }
        fn fnmadd(a: F, b: F, c: F) -> F { _mm256_fnmadd_ps(a, b, c) }
        fn min(a: F, b: F) -> F { _mm256_min_ps(a, b) }
        fn max(a: F, b: F) -> F { _mm256_max_ps(a, b) }
        fn abs(a: F) -> F { _mm256_andnot_ps(_mm256_set1_ps(-0.0), a) }
        fn cvt(a: F) -> I { _mm256_cvtps_epi32(a) }
        fn and_i(a: I, b: I) -> I { _mm256_and_si256(a, b) }
        fn or_i(a: I, b: I) -> I { _mm256_or_si256(a, b) }
        fn xor_i(a: I, b: I) -> I { _mm256_xor_si256(a, b) }
        fn add_i(a: I, b: I) -> I { _mm256_add_epi32(a, b) }
        fn sub_i(a: I, b: I) -> I { _mm256_sub_epi32(a, b) }
        fn min_i(a: I, b: I) -> I { _mm256_min_epi32(a, b) }
        fn andnot_i(m: I, a: I) -> I { _mm256_andnot_si256(m, a) }
        fn sra<const S: i32>(a: I) -> I { _mm256_srai_epi32::<S>(a) }
        fn xor_f(a: F, m: I) -> F { _mm256_xor_ps(a, _mm256_castsi256_ps(m)) }
        fn max_i(a: I, b: I) -> I { _mm256_max_epi32(a, b) }
        fn sll<const S: i32>(a: I) -> I { _mm256_slli_epi32::<S>(a) }
        fn srl<const S: i32>(a: I) -> I { _mm256_srli_epi32::<S>(a) }
        fn first(a: I) -> i32 { _mm256_cvtsi256_si32(a) }
        fn nle(a: F, b: F) -> I { _mm256_castps_si256(_mm256_cmp_ps::<_CMP_NLE_UQ>(a, b)) }
        fn mask(m: I) -> u32 { _mm256_movemask_ps(_mm256_castsi256_ps(m)) as u32 }
        fn perm(a: F, p: I) -> F { _mm256_permutevar8x32_ps(a, p) }
        fn perm_i(a: I, p: I) -> I { _mm256_permutevar8x32_epi32(a, p) }
        fn unpacklo(a: I, b: I) -> I { _mm256_unpacklo_epi32(a, b) }
        fn unpackhi(a: I, b: I) -> I { _mm256_unpackhi_epi32(a, b) }
        fn shuf<const M: i32>(a: I) -> I { _mm256_shuffle_epi32::<M>(a) }
        fn shuf2<const M: i32>(a: I, b: I) -> I {
            _mm256_castps_si256(_mm256_shuffle_ps::<M>(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b)))
        }
        fn perm2<const M: i32>(a: I, b: I) -> I { _mm256_permute2x128_si256::<M>(a, b) }
        fn blend(a: F, b: F, m: I) -> F { _mm256_blendv_ps(a, b, _mm256_castsi256_ps(m)) }
        fn blend_i(a: I, b: I, m: I) -> I {
            _mm256_castps_si256(_mm256_blendv_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b), _mm256_castsi256_ps(m)))
        }
    }

    #[inline]
    #[target_feature(enable = "avx2,fma")]
    fn store(d: &mut [f32; 8], v: F) {
        // SAFETY: 32 writable bytes; unaligned store.
        unsafe { _mm256_storeu_ps(d.as_mut_ptr(), v) }
    }
    #[inline]
    #[target_feature(enable = "avx2,fma")]
    fn store_i(d: &mut [i32; 8], v: I) {
        // SAFETY: 32 writable bytes; unaligned store.
        unsafe { _mm256_storeu_si256(d.as_mut_ptr() as *mut __m256i, v) }
    }

    kernel!(#[target_feature(enable = "avx2,fma")]);
    kernel16!(#[target_feature(enable = "avx2,fma")]);
}

/// The driven kernel: `LaLoop` for 8 paths.
pub(crate) struct Fast8<const N: usize> {
    paths: Paths<N>,
    coefs: Coefs<N>,
    front: Front,
    preroll: u64,
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // read on x86-64 only
    simd: bool,
}

/// Whether a search can run on these kernels: 8 or 16 paths, a decision
/// depth of 33 or more (the history sits left-aligned in a word, the new bit
/// in its low one) and order 5 or more.
pub(crate) fn fits(order: usize, paths: usize, depth: u32) -> bool {
    order >= 5 && (paths == 8 || paths == 16) && depth >= 33
}

impl<const N: usize> Fast8<N> {
    pub(crate) fn new(lp: &LoopN<N>, setup: &LaSetup) -> Self {
        assert!(fits(N, setup.paths, setup.depth) && setup.paths == 8);
        let mut s = Fast8 {
            paths: Paths { x: [[0.0; 8]; N], zlo: [0.0; 8], cost: [0; 8], hlo: [0; 8], hhi: [0; 8], gr: 0.0 },
            coefs: Coefs::new(lp),
            front: Front {
                limit: setup.limit,
                rng: DITHER_SEED,
                limiting: false,
                clamps: 0,
                resets: 0,
                shift: setup.depth - 1,
                scale: setup.dither / 65_536.0,
            },
            // Left-aligned: the bit committed next at bit 62.
            preroll: (0..(setup.depth - 1) / 8).fold(0u64, |h, _| h << 8 | DSD_SILENCE as u64) << (64 - setup.depth),
            simd: available(),
        };
        s.reset();
        s
    }
}

/// Whether this CPU runs the kernel's AVX2 + FMA build. The portable build
/// gives the same bytes but is about 3 times slower than the double search's
/// portable build, so without AVX2 + FMA the player keeps the double search.
pub(crate) fn available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

impl<const N: usize> LaLoop for Fast8<N> {
    fn run(&mut self, u: &[f64], out: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        if self.simd {
            // SAFETY: AVX2 and FMA were detected at construction.
            return unsafe { avx2::run(&mut self.paths, &self.coefs, &mut self.front, u, out) };
        }
        portable::run(&mut self.paths, &self.coefs, &mut self.front, u, out)
    }

    fn reset(&mut self) {
        self.paths.restart(self.preroll as u32 as i32, (self.preroll >> 32) as u32 as i32);
        self.front.rng = DITHER_SEED;
        self.front.limiting = false;
    }

    fn counts(&self) -> (u64, u64) {
        (self.front.clamps, self.front.resets)
    }

    #[cfg(test)]
    fn force_portable(&mut self) {
        self.simd = false;
    }
}

/// The driven kernel: `LaLoop` for 16 paths.
pub(crate) struct Fast16<const N: usize> {
    paths: Paths16<N>,
    coefs: Coefs<N>,
    front: Front,
    preroll: u64,
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // read on x86-64 only
    simd: bool,
}

impl<const N: usize> Fast16<N> {
    pub(crate) fn new(lp: &LoopN<N>, setup: &LaSetup) -> Self {
        assert!(fits(N, setup.paths, setup.depth) && setup.paths == 16);
        let mut s = Fast16 {
            paths: Paths16 {
                x: [[[0.0; 8]; 2]; N],
                zlo: [[0.0; 8]; 2],
                cost: [[0; 8]; 2],
                hlo: [[0; 8]; 2],
                hhi: [[0; 8]; 2],
                gr: 0.0,
            },
            coefs: Coefs::new(lp),
            front: Front {
                limit: setup.limit,
                rng: DITHER_SEED,
                limiting: false,
                clamps: 0,
                resets: 0,
                shift: setup.depth - 1,
                scale: setup.dither / 65_536.0,
            },
            preroll: (0..(setup.depth - 1) / 8).fold(0u64, |h, _| h << 8 | DSD_SILENCE as u64) << (64 - setup.depth),
            simd: available(),
        };
        s.reset();
        s
    }
}

impl<const N: usize> LaLoop for Fast16<N> {
    fn run(&mut self, u: &[f64], out: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        if self.simd {
            // SAFETY: AVX2 and FMA were detected at construction.
            return unsafe { avx2::run16(&mut self.paths, &self.coefs, &mut self.front, u, out) };
        }
        portable::run16(&mut self.paths, &self.coefs, &mut self.front, u, out)
    }

    fn reset(&mut self) {
        self.paths.restart(self.preroll as u32 as i32, (self.preroll >> 32) as u32 as i32);
        self.front.rng = DITHER_SEED;
        self.front.limiting = false;
    }

    fn counts(&self) -> (u64, u64) {
        (self.front.clamps, self.front.resets)
    }

    #[cfg(test)]
    fn force_portable(&mut self) {
        self.simd = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selection network: the eight smallest of sixteen distinct values,
    /// and the smallest in every lane.
    #[test]
    fn select_keeps_the_eight_cheapest() {
        let mut rng = 0x1234_5678_9abc_def1u64;
        for _ in 0..20_000 {
            let mut v = [0i32; 16];
            for (i, e) in v.iter_mut().enumerate() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *e = ((rng >> 40) as i32 & !31) | i as i32;
            }
            let k0: [i32; 8] = std::array::from_fn(|i| v[i]);
            let k1: [i32; 8] = std::array::from_fn(|i| v[8 + i]);
            let (mut got, best) = portable::select_for_test(k0, k1);
            got.sort();
            v.sort();
            assert_eq!(got[..], v[..8]);
            assert_eq!(best, [v[0]; 8]);
        }
    }

    /// The 16-path selection: the sixteen smallest of 32 distinct values,
    /// and the smallest in every lane.
    #[test]
    fn select16_keeps_the_sixteen_cheapest() {
        let mut rng = 0x0bad_cafe_1234_5679u64;
        for _ in 0..20_000 {
            let mut v = [0i32; 32];
            for (i, e) in v.iter_mut().enumerate() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *e = ((rng >> 40) as i32 & !63) | i as i32;
            }
            let k: [[i32; 8]; 4] = std::array::from_fn(|r| std::array::from_fn(|i| v[8 * r + i]));
            let (a, b, best) = portable::select16_for_test(k);
            let mut got: Vec<i32> = a.iter().chain(b.iter()).copied().collect();
            got.sort();
            v.sort();
            assert_eq!(got[..], v[..16]);
            assert_eq!(best, [v[0]; 8]);
        }
    }
}
