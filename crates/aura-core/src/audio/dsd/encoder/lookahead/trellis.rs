//! The trellis search (Viterbi with path merging): one path per pattern of
//! the last K bits, S = 2^K paths. Every sample each pattern s' = (s·2 + b)
//! mod S keeps the cheaper of its two predecessors (s'/2 and s'/2 + S/2)
//! extended by b — add, compare, select, with no sorting, so the work grows
//! only linearly with S and runs on all paths at once. Paths that agree on
//! their last K bits are the ones that merge; the commit and the drop of
//! disagreeing paths are the M-algorithm's.

use super::{Kernel, LoopN, DEAD};

pub(crate) struct Trellis<const N: usize, const S: usize> {
    lp: LoopN<N>,
    x: [[f64; S]; N],
    cost: [f64; S],
    hist: [u64; S],
    shift: u32,
}

impl<const N: usize, const S: usize> Trellis<N, S> {
    pub(crate) fn new(lp: LoopN<N>, shift: u32) -> Self {
        assert!(S.is_power_of_two() && S >= 2);
        Trellis { lp, x: [[0.0; S]; N], cost: [DEAD; S], hist: [0; S], shift }
    }
}

impl<const N: usize, const S: usize> Kernel for Trellis<N, S> {
    fn restart(&mut self, hist: u64) {
        self.x = [[0.0; S]; N];
        self.cost = [DEAD; S];
        // The live path sits in the pattern its last bits give.
        let s = (hist as usize) & (S - 1);
        self.cost[s] = 0.0;
        self.hist = [hist; S];
    }

    fn best_hist(&self) -> u64 {
        self.hist[best_index(&self.cost)]
    }

    #[inline(always)]
    fn step(&mut self, u: f64, d: f64) -> u64 {
        let (lp, cout, bv) = (self.lp, self.lp.cout, self.lp.bv);
        let half = S / 2;
        let mut base = [[0.0f64; S]; N];
        let mut c = [[0.0f64; S]; 2];
        for m in 0..S {
            let v = cout * self.x[N - 1][m] + u + d;
            let (e0, e1) = (v + 1.0, v - 1.0);
            c[0][m] = self.cost[m] + e0 * e0;
            c[1][m] = self.cost[m] + e1 * e1;
            let b = lp.base(&std::array::from_fn(|k| self.x[k][m]));
            for k in 0..N {
                base[k][m] = b[k];
            }
        }
        let w = [u + 1.0, u - 1.0];
        let mut nx = [[0.0f64; S]; N];
        let mut ncost = [0.0f64; S];
        let mut nhist = [0u64; S];
        for j in 0..half {
            for b in 0..2 {
                let (lo, hi) = (c[b][j], c[b][j + half]);
                let take_hi = hi < lo;
                let p = if take_hi { j + half } else { j };
                let s = 2 * j + b;
                ncost[s] = if take_hi { hi } else { lo };
                nhist[s] = self.hist[p] << 1 | b as u64;
                for k in 0..N {
                    nx[k][s] = base[k][p] + bv[k] * w[b];
                }
            }
        }
        let best = best_index(&ncost);
        let (bc, shift) = (ncost[best], self.shift);
        let committed = nhist[best] >> shift & 1;
        for s in 0..S {
            let agrees = nhist[s] >> shift & 1 == committed;
            ncost[s] = if agrees { ncost[s] - bc } else { DEAD };
        }
        self.x = nx;
        self.cost = ncost;
        self.hist = nhist;
        committed
    }

    #[inline(always)]
    fn guard(&mut self) -> bool {
        let best = best_index(&self.cost);
        let peak = |x: &[[f64; S]; N], j: usize| (0..N).fold(0.0f64, |p, k| p.max(x[k][j].abs()));
        if !(peak(&self.x, best) <= super::STATE_LIMIT) {
            return false;
        }
        for j in 0..S {
            if !(peak(&self.x, j) <= super::STATE_LIMIT) {
                for k in 0..N {
                    self.x[k][j] = self.x[k][best];
                }
                self.cost[j] = DEAD;
            }
        }
        true
    }
}

/// The cheapest path, the lowest index on a tie.
#[inline(always)]
fn best_index<const S: usize>(cost: &[f64; S]) -> usize {
    let mut min = cost[0];
    for &c in &cost[1..] {
        min = if c < min { c } else { min };
    }
    let mut at = S;
    for j in (0..S).rev() {
        at = if cost[j] == min { j } else { at };
    }
    at
}
