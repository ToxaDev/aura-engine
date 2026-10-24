//! The M-algorithm for M = 4·R paths on 4-wide vectors: each register holds
//! one value of four paths. One kernel, written once (`kernel!`) and built
//! twice: on AVX2 intrinsics (chosen at run time when the CPU has them) and on
//! plain arrays with the same lane semantics (`min`/`max` as `minpd`/`maxpd`,
//! the same operations in the same order), so both give the same bytes.
//!
//! Per sample: the children of every path (bit 0 and bit 1) get their costs
//! tagged with their index; the bit-0 list is sorted ascending and the bit-1
//! list descending by a bitonic network, and their element-wise minimum is
//! exactly the M cheapest of all 2M (the first step of a bitonic merge). The
//! survivors' parents are gathered by lane permutes; no data-dependent branch.

use super::{LoopN, DEAD, STATE_LIMIT};

/// What a fast search keeps between calls, as plain arrays.
#[derive(Clone, Copy)]
pub(crate) struct Paths<const N: usize, const R: usize> {
    pub x: [[[f64; 4]; R]; N],
    pub cost: [[f64; 4]; R],
    pub hist: [[u64; 4]; R],
}

impl<const N: usize, const R: usize> Paths<N, R> {
    /// Zero states, the first path live, every path holding `hist`.
    pub fn restart(&mut self, hist: u64) {
        self.x = [[[0.0; 4]; R]; N];
        self.cost = [[DEAD; 4]; R];
        self.cost[0][0] = 0.0;
        self.hist = [[hist; 4]; R];
    }

    fn best(&self) -> (usize, usize) {
        let mut at = (0, 0);
        for r in 0..R {
            for l in 0..4 {
                if self.cost[r][l] < self.cost[at.0][at.1] {
                    at = (r, l);
                }
            }
        }
        at
    }

    /// The slow half of the state guard (only when some state left the
    /// limit): paths out of it are dropped; false if the best one is.
    pub fn guard(&mut self) -> bool {
        let (br, bl) = self.best();
        let peak = |p: &Self, r: usize, l: usize| (0..N).fold(0.0f64, |m, k| m.max(p.x[k][r][l].abs()));
        if !(peak(self, br, bl) <= STATE_LIMIT) {
            return false;
        }
        for r in 0..R {
            for l in 0..4 {
                if !(peak(self, r, l) <= STATE_LIMIT) {
                    for k in 0..N {
                        self.x[k][r][l] = self.x[k][br][bl];
                    }
                    self.hist[r][l] = self.hist[br][bl];
                    self.cost[r][l] = DEAD;
                }
            }
        }
        true
    }

    pub fn best_hist(&self) -> u64 {
        let (r, l) = self.best();
        self.hist[r][l]
    }
}

/// The rest of the run state: input limit, dither, counters.
pub(crate) struct Front {
    pub limit: f64,
    pub rng: u64,
    pub limiting: bool,
    pub clamps: u64,
    pub resets: u64,
    /// Decision depth − 1.
    pub shift: u32,
    /// Peak of the triangular dither / 65 536.
    pub scale: f64,
}

const TAG_CLEAR: u64 = !((1u64 << super::TAG_BITS) - 1);

/// The kernel over a backend module that defines `F` (4 × f64), `I` (4 × u64)
/// and the lane operations used below.
macro_rules! kernel {
    ($(#[$attr:meta])*) => {
        /// Sorts 4R tagged costs held in `v` ascending (`desc` false) or
        /// descending: a bitonic network, fully unrolled for a given R.
        #[inline]
        $(#[$attr])*
        fn sort<const R: usize>(v: &mut [F; R], desc: bool) {
            let n = 4 * R;
            let mut k = 2;
            while k <= n {
                let mut j = k / 2;
                while j >= 1 {
                    if j >= 4 {
                        let jr = j / 4;
                        for r in 0..R {
                            let o = r ^ jr;
                            if o > r {
                                let up = ((r * 4) & k == 0) != desc;
                                let (lo, hi) = (min(v[r], v[o]), max(v[r], v[o]));
                                (v[r], v[o]) = if up { (lo, hi) } else { (hi, lo) };
                            }
                        }
                    } else {
                        for r in 0..R {
                            let x = v[r];
                            let p = if j == 2 { swap_halves(x) } else { swap_pairs(x) };
                            let (lo, hi) = (min(x, p), max(x, p));
                            // Lane i takes `hi` when it is the upper of its pair
                            // in an ascending block, or the lower in a
                            // descending one.
                            v[r] = if j == 2 {
                                if ((r * 4) & k == 0) != desc { blend_halves_up(lo, hi) } else { blend_halves_down(lo, hi) }
                            } else if k == 2 {
                                if desc { blend_pairs_down_up(lo, hi) } else { blend_pairs_up_down(lo, hi) }
                            } else if ((r * 4) & k == 0) != desc {
                                blend_pairs_up(lo, hi)
                            } else {
                                blend_pairs_down(lo, hi)
                            };
                        }
                    }
                    j /= 2;
                }
                k *= 2;
            }
        }

        /// Lane `l` of register `r` of the result is lane `loc` of register
        /// `reg` of `src`, (`loc`, `reg`) taken per lane from `perm` / `reg`.
        #[inline]
        $(#[$attr])*
        fn gather<const R: usize>(src: &[F; R], perm: I, reg: I) -> F {
            let mut out = permute(src[0], perm);
            for s in 1..R {
                out = select(eq(reg, splat_i(s as u64)), permute(src[s], perm), out);
            }
            out
        }

        #[inline]
        $(#[$attr])*
        fn gather_i<const R: usize>(src: &[I; R], perm: I, reg: I) -> I {
            let mut out = permute_i(src[0], perm);
            for s in 1..R {
                out = select_i(eq(reg, splat_i(s as u64)), permute_i(src[s], perm), out);
            }
            out
        }

        /// Runs `u` (of full modulation, a multiple of 8 samples) into `out`.
        $(#[$attr])*
        pub(crate) fn run<const N: usize, const R: usize>(
            p: &mut Paths<N, R>,
            lp: &LoopN<N>,
            f: &mut Front,
            u: &[f64],
            out: &mut [u8],
        ) {
            let scale = f.scale;
            let mut c = [splat(0.0); N];
            let mut g = [splat(0.0); N];
            let mut bv = [splat(0.0); N];
            for k in 0..N {
                (c[k], g[k], bv[k]) = (splat(lp.c[k]), splat(lp.g[k]), splat(lp.bv[k]));
            }
            let cout = splat(lp.cout);
            let (one, minus_one, dead, state_limit) = (splat(1.0), splat(-1.0), splat(DEAD), splat(STATE_LIMIT));
            let clear = splat_i(TAG_CLEAR);
            let (low1, low3, shift) = (splat_i(1), splat_i(3), f.shift);
            // Tags: bit-0 children 2m, bit-1 children 2m + 1, m = 4r + lane.
            let mut tags0 = [splat_i(0); R];
            let mut tags1 = [splat_i(0); R];
            for r in 0..R {
                let m = (4 * r) as u64;
                tags0[r] = lanes_i([2 * m, 2 * m + 2, 2 * m + 4, 2 * m + 6]);
                tags1[r] = add_i(tags0[r], low1);
            }
            let mut x: [[F; R]; N] = [[splat(0.0); R]; N];
            let mut cost = [splat(0.0); R];
            let mut hist = [splat_i(0); R];
            for r in 0..R {
                for k in 0..N {
                    x[k][r] = load(&p.x[k][r]);
                }
                cost[r] = load(&p.cost[r]);
                hist[r] = load_i(&p.hist[r]);
            }
            let limit = f.limit;
            for (byte_out, eight) in out.iter_mut().zip(u.chunks_exact(8)) {
                let mut byte = 0u64;
                for pair in eight.chunks_exact(2) {
                    let mut rng = f.rng;
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    f.rng = rng;
                    let tri = |hi: u64, lo: u64| ((hi & 0xFFFF) as i32 - (lo & 0xFFFF) as i32) as f64 * scale;
                    let d = [tri(rng >> 48, rng >> 32), tri(rng >> 16, rng)];
                    for h in 0..2 {
                        let mut s = pair[h];
                        if s.abs() > limit {
                            if !f.limiting {
                                f.limiting = true;
                                f.clamps += 1;
                            }
                            s = limit.copysign(s);
                        } else {
                            f.limiting = false;
                        }
                        let (us, ud) = (splat(s), splat(s + d[h]));
                        // Children's costs, and the parents' states before w.
                        let mut k0 = [splat(0.0); R];
                        let mut k1 = [splat(0.0); R];
                        let mut base: [[F; R]; N] = [[splat(0.0); R]; N];
                        for r in 0..R {
                            let v = add(mul(cout, x[N - 1][r]), ud);
                            let (e0, e1) = (add(v, one), sub(v, one));
                            k0[r] = tag(add(cost[r], mul(e0, e0)), clear, tags0[r]);
                            k1[r] = tag(add(cost[r], mul(e1, e1)), clear, tags1[r]);
                            // The resonators: old neighbours into x[i], new x[i]
                            // into x[i + 1] (LoopN::base).
                            base[0][r] = x[0][r];
                            let mut i = 1;
                            while i < N {
                                let b = sub(add(x[i][r], mul(c[i - 1], x[i - 1][r])), mul(g[i / 2], x[i + 1][r]));
                                base[i][r] = b;
                                base[i + 1][r] = add(x[i + 1][r], mul(c[i], b));
                                i += 2;
                            }
                        }
                        sort(&mut k0, false);
                        sort(&mut k1, true);
                        let mut sv = [splat(0.0); R];
                        let mut m = splat(DEAD * 4.0);
                        for r in 0..R {
                            sv[r] = min(k0[r], k1[r]);
                            m = min(m, sv[r]);
                        }
                        let best = hmin(m);
                        let best_clean = and(best, clear);
                        let mut committed_mask = 0u32;
                        let mut pending = [splat_i(0); R];
                        let hold = hist;
                        for r in 0..R {
                            let idx = bits(sv[r]);
                            let bit = and_i(idx, low1);
                            let parent = srl::<1>(idx);
                            let perm = and_i(parent, low3);
                            let reg = srl::<2>(parent);
                            let y = select(eq(bit, low1), one, minus_one);
                            let w = sub(us, y);
                            for k in 0..N {
                                x[k][r] = add(gather(&base[k], perm, reg), mul(bv[k], w));
                            }
                            hist[r] = or_i(sll::<1>(gather_i(&hold, perm, reg)), bit);
                            cost[r] = sub(and(sv[r], clear), best_clean);
                            let pend = and_i(srl_var(hist[r], shift), low1);
                            pending[r] = pend;
                            committed_mask |= mask(eq_f(sv[r], best)) & mask_i(eq(pend, low1));
                        }
                        let committed = (committed_mask != 0) as u64;
                        let want = splat_i(committed);
                        for r in 0..R {
                            cost[r] = select(eq(pending[r], want), cost[r], dead);
                        }
                        byte = byte << 1 | committed;
                    }
                }
                *byte_out = byte as u8;
                let mut peak = splat(0.0);
                for r in 0..R {
                    for k in 0..N {
                        peak = max(peak, abs(x[k][r]));
                    }
                }
                if mask(gt(peak, state_limit)) != 0 || mask(neq_self(peak)) != 0 {
                    for r in 0..R {
                        for k in 0..N {
                            store(&mut p.x[k][r], x[k][r]);
                        }
                        store(&mut p.cost[r], cost[r]);
                        store_i(&mut p.hist[r], hist[r]);
                    }
                    if !p.guard() {
                        let h = p.best_hist();
                        p.restart(h);
                        f.resets += 1;
                    }
                    for r in 0..R {
                        for k in 0..N {
                            x[k][r] = load(&p.x[k][r]);
                        }
                        cost[r] = load(&p.cost[r]);
                        hist[r] = load_i(&p.hist[r]);
                    }
                }
            }
            for r in 0..R {
                for k in 0..N {
                    store(&mut p.x[k][r], x[k][r]);
                }
                store(&mut p.cost[r], cost[r]);
                store_i(&mut p.hist[r], hist[r]);
            }
        }

    };
}

/// Plain arrays with the AVX2 lane semantics.
pub(super) mod portable {
    use super::*;

    type F = [f64; 4];
    type I = [u64; 4];

    #[inline(always)]
    fn map(a: F, b: F, f: impl Fn(f64, f64) -> f64) -> F {
        [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), f(a[3], b[3])]
    }
    #[inline(always)]
    fn map_i(a: I, b: I, f: impl Fn(u64, u64) -> u64) -> I {
        [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), f(a[3], b[3])]
    }
    #[inline(always)]
    fn splat(v: f64) -> F {
        [v; 4]
    }
    #[inline(always)]
    fn splat_i(v: u64) -> I {
        [v; 4]
    }
    #[inline(always)]
    fn lanes_i(v: [u64; 4]) -> I {
        v
    }
    #[inline(always)]
    fn load(v: &[f64; 4]) -> F {
        *v
    }
    #[inline(always)]
    fn load_i(v: &[u64; 4]) -> I {
        *v
    }
    #[inline(always)]
    fn store(d: &mut [f64; 4], v: F) {
        *d = v;
    }
    #[inline(always)]
    fn store_i(d: &mut [u64; 4], v: I) {
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
    /// `minpd`: the second operand unless the first is less.
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
        a.map(f64::abs)
    }
    #[inline(always)]
    fn and(a: F, m: I) -> F {
        let b = map_i(a.map(f64::to_bits), m, |x, y| x & y);
        b.map(f64::from_bits)
    }
    #[inline(always)]
    fn tag(c: F, clear: I, t: I) -> F {
        let b = map_i(map_i(c.map(f64::to_bits), clear, |x, y| x & y), t, |x, y| x | y);
        b.map(f64::from_bits)
    }
    #[inline(always)]
    fn bits(a: F) -> I {
        a.map(|x| x.to_bits() & !TAG_CLEAR)
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
    fn add_i(a: I, b: I) -> I {
        map_i(a, b, |x, y| x.wrapping_add(y))
    }
    #[inline(always)]
    fn srl<const S: u32>(a: I) -> I {
        a.map(|x| x >> S)
    }
    #[inline(always)]
    fn sll<const S: u32>(a: I) -> I {
        a.map(|x| x << S)
    }
    #[inline(always)]
    fn srl_var(a: I, s: u32) -> I {
        a.map(|x| x >> s)
    }
    /// Lane masks are all ones or zero, as AVX2 compares give.
    #[inline(always)]
    fn eq(a: I, b: I) -> I {
        map_i(a, b, |x, y| if x == y { !0 } else { 0 })
    }
    #[inline(always)]
    fn eq_f(a: F, b: F) -> I {
        [0, 1, 2, 3].map(|i| if a[i] == b[i] { !0 } else { 0 })
    }
    #[inline(always)]
    fn gt(a: F, b: F) -> I {
        [0, 1, 2, 3].map(|i| if a[i] > b[i] { !0 } else { 0 })
    }
    #[inline(always)]
    fn neq_self(a: F) -> I {
        a.map(|x| if x != x { !0 } else { 0 })
    }
    #[inline(always)]
    fn mask(m: I) -> u32 {
        (0..4).fold(0, |acc, i| acc | ((m[i] >> 63) as u32) << i)
    }
    #[inline(always)]
    fn mask_i(m: I) -> u32 {
        mask(m)
    }
    #[inline(always)]
    fn select(m: I, a: F, b: F) -> F {
        [0, 1, 2, 3].map(|i| if m[i] >> 63 == 1 { a[i] } else { b[i] })
    }
    #[inline(always)]
    fn select_i(m: I, a: I, b: I) -> I {
        [0, 1, 2, 3].map(|i| if m[i] >> 63 == 1 { a[i] } else { b[i] })
    }
    #[inline(always)]
    fn permute(a: F, p: I) -> F {
        p.map(|i| a[i as usize & 3])
    }
    #[inline(always)]
    fn permute_i(a: I, p: I) -> I {
        p.map(|i| a[i as usize & 3])
    }
    #[inline(always)]
    fn swap_pairs(a: F) -> F {
        [a[1], a[0], a[3], a[2]]
    }
    #[inline(always)]
    fn swap_halves(a: F) -> F {
        [a[2], a[3], a[0], a[1]]
    }
    #[inline(always)]
    fn hmin(a: F) -> F {
        let b = min(a, swap_pairs(a));
        min(b, swap_halves(b))
    }
    #[inline(always)]
    fn pick(lo: F, hi: F, m: u32) -> F {
        [0, 1, 2, 3].map(|i| if m >> i & 1 == 1 { hi[i] } else { lo[i] })
    }
    #[inline(always)]
    fn blend_pairs_up(lo: F, hi: F) -> F {
        pick(lo, hi, 0b1010)
    }
    #[inline(always)]
    fn blend_pairs_down(lo: F, hi: F) -> F {
        pick(lo, hi, 0b0101)
    }
    #[inline(always)]
    fn blend_pairs_up_down(lo: F, hi: F) -> F {
        pick(lo, hi, 0b0110)
    }
    #[inline(always)]
    fn blend_pairs_down_up(lo: F, hi: F) -> F {
        pick(lo, hi, 0b1001)
    }
    #[inline(always)]
    fn blend_halves_up(lo: F, hi: F) -> F {
        pick(lo, hi, 0b1100)
    }
    #[inline(always)]
    fn blend_halves_down(lo: F, hi: F) -> F {
        pick(lo, hi, 0b0011)
    }

    kernel!();
}

/// AVX2: every function is compiled for it; `run` is called only when the
/// CPU has it.
#[cfg(target_arch = "x86_64")]
pub(super) mod avx2 {
    use super::*;
    use std::arch::x86_64::*;

    type F = __m256d;
    type I = __m256i;

    #[inline]
    #[target_feature(enable = "avx2")]
    fn splat(v: f64) -> F {
        _mm256_set1_pd(v)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn splat_i(v: u64) -> I {
        _mm256_set1_epi64x(v as i64)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lanes_i(v: [u64; 4]) -> I {
        _mm256_setr_epi64x(v[0] as i64, v[1] as i64, v[2] as i64, v[3] as i64)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load(v: &[f64; 4]) -> F {
        // SAFETY: 32 readable bytes; unaligned load.
        unsafe { _mm256_loadu_pd(v.as_ptr()) }
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_i(v: &[u64; 4]) -> I {
        // SAFETY: 32 readable bytes; unaligned load.
        unsafe { _mm256_loadu_si256(v.as_ptr() as *const __m256i) }
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn store(d: &mut [f64; 4], v: F) {
        // SAFETY: 32 writable bytes; unaligned store.
        unsafe { _mm256_storeu_pd(d.as_mut_ptr(), v) }
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_i(d: &mut [u64; 4], v: I) {
        // SAFETY: 32 writable bytes; unaligned store.
        unsafe { _mm256_storeu_si256(d.as_mut_ptr() as *mut __m256i, v) }
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn add(a: F, b: F) -> F {
        _mm256_add_pd(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn sub(a: F, b: F) -> F {
        _mm256_sub_pd(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn mul(a: F, b: F) -> F {
        _mm256_mul_pd(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn min(a: F, b: F) -> F {
        _mm256_min_pd(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn max(a: F, b: F) -> F {
        _mm256_max_pd(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn abs(a: F) -> F {
        _mm256_andnot_pd(_mm256_set1_pd(-0.0), a)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn and(a: F, m: I) -> F {
        _mm256_and_pd(a, _mm256_castsi256_pd(m))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn tag(c: F, clear: I, t: I) -> F {
        _mm256_castsi256_pd(_mm256_or_si256(_mm256_and_si256(_mm256_castpd_si256(c), clear), t))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn bits(a: F) -> I {
        _mm256_andnot_si256(_mm256_set1_epi64x(TAG_CLEAR as i64), _mm256_castpd_si256(a))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn and_i(a: I, b: I) -> I {
        _mm256_and_si256(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn or_i(a: I, b: I) -> I {
        _mm256_or_si256(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn add_i(a: I, b: I) -> I {
        _mm256_add_epi64(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn srl<const S: i32>(a: I) -> I {
        _mm256_srli_epi64::<S>(a)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn sll<const S: i32>(a: I) -> I {
        _mm256_slli_epi64::<S>(a)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn srl_var(a: I, s: u32) -> I {
        _mm256_srl_epi64(a, _mm_cvtsi32_si128(s as i32))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn eq(a: I, b: I) -> I {
        _mm256_cmpeq_epi64(a, b)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn eq_f(a: F, b: F) -> I {
        _mm256_castpd_si256(_mm256_cmp_pd::<_CMP_EQ_OQ>(a, b))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn gt(a: F, b: F) -> I {
        _mm256_castpd_si256(_mm256_cmp_pd::<_CMP_GT_OQ>(a, b))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn neq_self(a: F) -> I {
        _mm256_castpd_si256(_mm256_cmp_pd::<_CMP_UNORD_Q>(a, a))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn mask(m: I) -> u32 {
        _mm256_movemask_pd(_mm256_castsi256_pd(m)) as u32
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn mask_i(m: I) -> u32 {
        mask(m)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn select(m: I, a: F, b: F) -> F {
        _mm256_blendv_pd(b, a, _mm256_castsi256_pd(m))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn select_i(m: I, a: I, b: I) -> I {
        _mm256_castpd_si256(select(m, _mm256_castsi256_pd(a), _mm256_castsi256_pd(b)))
    }
    /// 64-bit lane i ← lane p[i]: as 32-bit lanes (2p, 2p + 1).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn idx32(p: I) -> I {
        let t = _mm256_slli_epi64::<1>(p);
        _mm256_or_si256(t, _mm256_slli_epi64::<32>(_mm256_add_epi64(t, _mm256_set1_epi64x(1))))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn permute(a: F, p: I) -> F {
        _mm256_castsi256_pd(_mm256_permutevar8x32_epi32(_mm256_castpd_si256(a), idx32(p)))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn permute_i(a: I, p: I) -> I {
        _mm256_permutevar8x32_epi32(a, idx32(p))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn swap_pairs(a: F) -> F {
        _mm256_permute_pd::<0b0101>(a)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn swap_halves(a: F) -> F {
        _mm256_permute2f128_pd::<0x01>(a, a)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn hmin(a: F) -> F {
        let b = min(a, swap_pairs(a));
        min(b, swap_halves(b))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_pairs_up(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b1010>(lo, hi)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_pairs_down(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b0101>(lo, hi)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_pairs_up_down(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b0110>(lo, hi)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_pairs_down_up(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b1001>(lo, hi)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_halves_up(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b1100>(lo, hi)
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    fn blend_halves_down(lo: F, hi: F) -> F {
        _mm256_blend_pd::<0b0011>(lo, hi)
    }

    kernel!(#[target_feature(enable = "avx2")]);
}
