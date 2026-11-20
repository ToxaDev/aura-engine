//! The continuous-alpha blend (αHP) split in two bands.
//!
//! A plain `α·min + (1 − α)·lin` mixes the two phases at every frequency.
//! At 14–20 kHz the linear- and minimum-phase renders are nearly in
//! antiphase, so a partial mix cancels: the top octave dipped 1–2.3 dB under
//! every hit and its level followed α up and down. Here the blend keeps α
//! below ~7 kHz and takes the top either whole from one phase or whole from
//! the other, chosen by an HF gate `s`:
//!
//! ```text
//!   d  = min − lin            (per channel)
//!   LPd = h * d               (sinc⁴ low-pass, linear phase, centred)
//!   y  = lin + s·d + (α − s)·LPd
//! ```
//!
//! Per frequency the weight of `min` is `s + (α − s)·H(f)` with `H` real and
//! in [0, 1], so the result is a convex mix of the two renders at every
//! frequency — no boost anywhere. `s` is 0 or 1, decided per analysis frame
//! from the mean α around it, and changes with the same 1 ms
//! `switch_fade` the hard switch uses.
//!
//! Every path — the in-RAM converter, the giant streaming converter and the
//! player — computes each output sample with `AlphaSplit::block`, from the
//! same window of samples in the same order, so their outputs match bit for
//! bit whatever their chunking. Samples outside `[0, total)` count as zero.

use super::hybrid_phase::{catmull_env_at, switch_fade, switch_params};

/// Frames on each side of a frame whose α the HF gate averages.
pub(crate) const GATE_HALF_FRAMES: isize = 2;

/// Mean α at which the HF gate takes the minimum phase.
const GATE_THRESHOLD: f64 = 0.5;

/// Outputs the low-pass computes in one pass (fits the L1/L2 caches).
const SUB_BLOCK: usize = 4096;

/// Boxcar length of the sinc⁴ kernel at `output_sr`: H = 0.5 near 7 kHz at
/// every rate (705.6k: 32, 352.8k: 16, 192k: 9, 96k: 4, 44.1k: 2).
pub(crate) fn boxcar_len(output_sr: f64) -> usize {
    ((output_sr / 22_050.0).round() as usize).max(2)
}

/// The centre half `h[D..]` of the sinc⁴ kernel: four boxcars of length `l`
/// convolved, divided by `l⁴`. `4l − 3` taps, centre `D = 2l − 2`. The
/// integer coefficients are exact in f64, so every path gets the same bits.
pub(crate) fn sinc4_half(l: usize) -> Vec<f64> {
    let mut k = vec![1.0f64; l];
    for _ in 0..3 {
        let mut next = vec![0.0f64; k.len() + l - 1];
        for (i, &v) in k.iter().enumerate() {
            for j in 0..l {
                next[i + j] += v;
            }
        }
        k = next;
    }
    let norm = (l as f64).powi(4);
    let d = 2 * l - 2;
    k[d..].iter().map(|&v| v / norm).collect()
}

/// The αHP blend of one track: kernel, fade and envelope rate.
pub(crate) struct AlphaSplit {
    /// `h[D..]`: `hc[0]` the centre tap, `hc[m]` the taps at ±m.
    hc: Vec<f64>,
    fade: usize,
    frames_to_output: f64,
}

/// Work buffers of `AlphaSplit::block`, kept between calls.
#[derive(Default)]
pub(crate) struct SplitScratch {
    d: Vec<f64>,
    lp: Vec<f64>,
    a: Vec<f64>,
    s: Vec<f64>,
}

impl AlphaSplit {
    pub(crate) fn new(output_sr: f64, frames_to_output: f64) -> AlphaSplit {
        let (_, fade, _) = switch_params(output_sr);
        AlphaSplit { hc: sinc4_half(boxcar_len(output_sr)), fade, frames_to_output }
    }

    /// Samples the low-pass looks ahead and back (`D`).
    pub(crate) fn delay(&self) -> usize {
        self.hc.len() - 1
    }

    /// HF gate of analysis frame `j`: mean α over ±`GATE_HALF_FRAMES`
    /// frames (frames outside the envelope count as 0) at or above 0.5.
    fn gate(smoothed: &[f64], j: isize) -> f64 {
        let mut sum = 0.0f64;
        for k in j - GATE_HALF_FRAMES..=j + GATE_HALF_FRAMES {
            if k >= 0 && (k as usize) < smoothed.len() {
                sum += smoothed[k as usize];
            }
        }
        if sum / (2 * GATE_HALF_FRAMES + 1) as f64 >= GATE_THRESHOLD {
            1.0
        } else {
            0.0
        }
    }

    /// α and the HF gate `s` at output sample `i`. Stateless: the gate of
    /// frame `j` holds from halfway after frame `j − 1` to halfway before
    /// frame `j + 1`, and a change crosses the halfway point in a
    /// `switch_fade` of the 1 ms switch fade.
    pub(crate) fn weights(&self, smoothed: &[f64], i: usize) -> (f64, f64) {
        let a = catmull_env_at(smoothed, self.frames_to_output, i) as f64;
        let f = self.frames_to_output;
        let jm = (i as f64 / f).floor();
        let dist = i as f64 - (jm + 0.5) * f;
        let half = self.fade as f64 / 2.0;
        let g0 = Self::gate(smoothed, jm as isize);
        let g1 = Self::gate(smoothed, jm as isize + 1);
        let s = if g0 != g1 && dist.abs() < half {
            g0 + (g1 - g0) * switch_fade((dist + half) / (2.0 * half))
        } else if dist < 0.0 {
            g0
        } else {
            g1
        };
        (a, s)
    }

    /// The blend of output samples `start..start + n` (`n = out_l.len()`).
    /// `lin_*`/`min_*` hold samples `start − D .. start + n + D` (zero where
    /// they fall outside the track). `note` gets every sample's α.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn block(
        &self,
        smoothed: &[f64],
        start: usize,
        lin_l: &[f64],
        min_l: &[f64],
        lin_r: &[f64],
        min_r: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        scratch: &mut SplitScratch,
        mut note: impl FnMut(usize, f64),
    ) {
        let n = out_l.len();
        let dd = self.delay();
        debug_assert!(lin_l.len() >= n + 2 * dd && min_l.len() >= n + 2 * dd);
        debug_assert!(lin_r.len() >= n + 2 * dd && min_r.len() >= n + 2 * dd);
        let mut b = 0;
        while b < n {
            let m = SUB_BLOCK.min(n - b);
            scratch.a.resize(m, 0.0);
            scratch.s.resize(m, 0.0);
            let mut split = false;
            for k in 0..m {
                let (a, s) = self.weights(smoothed, start + b + k);
                scratch.a[k] = a;
                scratch.s[k] = s;
                split |= a != s;
                note(start + b + k, a);
            }
            for (lin, min, out) in [(lin_l, min_l, &mut *out_l), (lin_r, min_r, &mut *out_r)] {
                let lin_w = &lin[b..b + m + 2 * dd];
                let min_w = &min[b..b + m + 2 * dd];
                let out = &mut out[b..b + m];
                if split {
                    scratch.d.clear();
                    scratch.d.extend(min_w.iter().zip(lin_w).map(|(&y, &x)| y - x));
                    self.lowpass(&scratch.d, m, &mut scratch.lp);
                }
                for k in 0..m {
                    let (a, s) = (scratch.a[k], scratch.s[k]);
                    let x = lin_w[k + dd];
                    out[k] = if a == 0.0 && s == 0.0 {
                        x
                    } else if a == s {
                        x + s * (min_w[k + dd] - x)
                    } else {
                        x + s * scratch.d[k + dd] + (a - s) * scratch.lp[k]
                    };
                }
            }
            b += m;
        }
    }

    /// `lp[c] = hc[0]·d[c + D] + Σₘ hc[m]·(d[c + D − m] + d[c + D + m])`,
    /// m = 1..=D in this order for every output — the same bits whatever
    /// window an output sits in.
    fn lowpass(&self, d: &[f64], m: usize, lp: &mut Vec<f64>) {
        let dd = self.delay();
        lp.clear();
        lp.extend(d[dd..dd + m].iter().map(|&v| self.hc[0] * v));
        for (t, &h) in self.hc.iter().enumerate().skip(1) {
            let lo = &d[dd - t..dd - t + m];
            let hi = &d[dd + t..dd + t + m];
            for c in 0..m {
                lp[c] += h * (lo[c] + hi[c]);
            }
        }
    }
}

/// HF gate of a whole envelope: (frames on the minimum phase, changes).
pub(crate) fn gate_stats(smoothed: &[f64]) -> (usize, usize) {
    let mut on = 0;
    let mut changes = 0;
    let mut prev = 0.0;
    for j in 0..smoothed.len() {
        let g = AlphaSplit::gate(smoothed, j as isize);
        if g == 1.0 {
            on += 1;
        }
        if j > 0 && g != prev {
            changes += 1;
        }
        prev = g;
    }
    (on, changes)
}

/// Logs how often the HF gate switches over a track (`env_sr` frames/s).
pub(crate) fn log_gate(smoothed: &[f64], env_sr: f64) {
    let (on, changes) = gate_stats(smoothed);
    let frames = smoothed.len().max(1) as f64;
    let minutes = frames / env_sr.max(1.0) / 60.0;
    crate::aelog!(
        "[aHP] HF gate: {} changes ({:.1}/min), on minimum {:.1}% of {} frames",
        changes,
        changes as f64 / minutes.max(1e-9),
        100.0 * on as f64 / frames,
        smoothed.len()
    );
}

/// `AlphaSplit::block` over a stream: samples go in as they come, the blend
/// comes out `D` samples behind. Starts at output sample `start`, with
/// nothing before it (as at the start of a track, or after a seek).
pub(crate) struct AlphaSplitStream {
    split: AlphaSplit,
    /// Output index of the next sample `emit` gives.
    pos: usize,
    lin_l: Vec<f64>,
    lin_r: Vec<f64>,
    min_l: Vec<f64>,
    min_r: Vec<f64>,
    scratch: SplitScratch,
}

impl AlphaSplitStream {
    pub(crate) fn new(split: AlphaSplit, start: usize) -> AlphaSplitStream {
        let dd = split.delay();
        AlphaSplitStream {
            split,
            pos: start,
            lin_l: vec![0.0; dd],
            lin_r: vec![0.0; dd],
            min_l: vec![0.0; dd],
            min_r: vec![0.0; dd],
            scratch: SplitScratch::default(),
        }
    }

    /// Output index of the next sample `emit` gives.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Input samples `emit(n)` still needs beyond what was pushed.
    pub(crate) fn needs(&self, n: usize) -> usize {
        (n + 2 * self.split.delay()).saturating_sub(self.lin_l.len())
    }

    /// The next input samples of both branches (equal lengths).
    pub(crate) fn push(&mut self, lin_l: &[f64], lin_r: &[f64], min_l: &[f64], min_r: &[f64]) {
        self.lin_l.extend_from_slice(lin_l);
        self.lin_r.extend_from_slice(lin_r);
        self.min_l.extend_from_slice(min_l);
        self.min_r.extend_from_slice(min_r);
    }

    /// `n` zero input samples (past the end of the track).
    pub(crate) fn push_zeros(&mut self, n: usize) {
        for v in [&mut self.lin_l, &mut self.lin_r, &mut self.min_l, &mut self.min_r] {
            v.resize(v.len() + n, 0.0);
        }
    }

    /// The next `out_l.len()` outputs; `needs(n)` must be 0.
    pub(crate) fn emit(
        &mut self,
        smoothed: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        note: impl FnMut(usize, f64),
    ) {
        let n = out_l.len();
        assert_eq!(self.needs(n), 0, "AlphaSplitStream::emit before its input");
        self.split.block(
            smoothed,
            self.pos,
            &self.lin_l,
            &self.min_l,
            &self.lin_r,
            &self.min_r,
            out_l,
            out_r,
            &mut self.scratch,
            note,
        );
        self.consume(n);
    }

    /// `emit` without notes, its pieces in parallel (the converter's long
    /// chunks). Same bits as `emit`.
    pub(crate) fn emit_par(&mut self, smoothed: &[f64], out_l: &mut [f64], out_r: &mut [f64]) {
        use rayon::prelude::*;
        const PIECE: usize = 1 << 15;
        let n = out_l.len();
        assert_eq!(self.needs(n), 0, "AlphaSplitStream::emit before its input");
        let dd = self.split.delay();
        let (split, pos) = (&self.split, self.pos);
        let (ll, lr, ml, mr) = (&self.lin_l, &self.lin_r, &self.min_l, &self.min_r);
        out_l
            .par_chunks_mut(PIECE)
            .zip(out_r.par_chunks_mut(PIECE))
            .enumerate()
            .for_each_init(SplitScratch::default, |scratch, (ci, (ol, or))| {
                let (b, e) = (ci * PIECE, ci * PIECE + ol.len() + 2 * dd);
                split.block(smoothed, pos + b, &ll[b..e], &ml[b..e], &lr[b..e], &mr[b..e], ol, or, scratch, |_, _| {});
            });
        self.consume(n);
    }

    fn consume(&mut self, n: usize) {
        for v in [&mut self.lin_l, &mut self.lin_r, &mut self.min_l, &mut self.min_r] {
            v.drain(..n);
        }
        self.pos += n;
    }
}

/// The αHP blend of a whole track in RAM, in place into `lin_*`. Chunks run
/// in parallel; each copies its window, with the samples around its edges
/// taken before any chunk writes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn blend_in_place(
    split: &AlphaSplit,
    smoothed: &[f64],
    lin_l: &mut [f64],
    min_l: &[f64],
    lin_r: &mut [f64],
    min_r: &[f64],
) {
    use rayon::prelude::*;
    const CHUNK: usize = 1 << 16;
    let len = lin_l.len();
    let dd = split.delay();
    // Samples a..b of `src`, zero outside 0..len.
    let window = |src: &[f64], a: isize, b: isize| -> Vec<f64> {
        (a..b).map(|i| if i >= 0 && (i as usize) < len { src[i as usize] } else { 0.0 }).collect()
    };
    let n_chunks = len.div_ceil(CHUNK);
    // halos[c]: lin around boundary c·CHUNK, samples c·CHUNK − D .. c·CHUNK + D.
    let halos: Vec<(Vec<f64>, Vec<f64>)> = (0..=n_chunks)
        .map(|c| {
            let b = (c * CHUNK).min(len) as isize;
            (window(lin_l, b - dd as isize, b + dd as isize), window(lin_r, b - dd as isize, b + dd as isize))
        })
        .collect();
    lin_l
        .par_chunks_mut(CHUNK)
        .zip(lin_r.par_chunks_mut(CHUNK))
        .enumerate()
        .for_each_init(SplitScratch::default, |scratch, (ci, (yl, yr))| {
            let b0 = ci * CHUNK;
            let n = yl.len();
            let lin_w = |own: &[f64], h0: &[f64], h1: &[f64]| -> Vec<f64> {
                let mut w = Vec::with_capacity(n + 2 * dd);
                w.extend_from_slice(&h0[..dd]);
                w.extend_from_slice(own);
                w.extend_from_slice(&h1[dd..]);
                w
            };
            let wl = lin_w(yl, &halos[ci].0, &halos[ci + 1].0);
            let wr = lin_w(yr, &halos[ci].1, &halos[ci + 1].1);
            let (a, e) = (b0 as isize - dd as isize, (b0 + n + dd) as isize);
            let ml = window(min_l, a, e);
            let mr = window(min_r, a, e);
            split.block(smoothed, b0, &wl, &ml, &wr, &mr, yl, yr, scratch, |_, _| {});
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
            })
            .collect()
    }

    /// An envelope that crosses 0.5 back and forth, with zero stretches.
    fn envelope(frames: usize) -> Vec<f64> {
        (0..frames)
            .map(|j| match j % 37 {
                0..=4 => 0.0,
                5..=12 => 0.9,
                13..=20 => 0.45 + 0.1 * ((j % 3) as f64),
                _ => 0.2,
            })
            .collect()
    }

    #[test]
    fn kernel_is_symmetric_and_sums_to_one() {
        for sr in [44_100.0, 96_000.0, 192_000.0, 352_800.0, 705_600.0, 768_000.0] {
            let l = boxcar_len(sr);
            let hc = sinc4_half(l);
            assert_eq!(hc.len(), 2 * l - 1, "{sr}");
            let sum = hc[0] + 2.0 * hc[1..].iter().sum::<f64>();
            assert!((sum - 1.0).abs() < 1e-12, "{sr}: {sum}");
            assert!(hc.windows(2).all(|w| w[0] >= w[1]), "{sr}: falls from the centre");
        }
        assert_eq!(boxcar_len(705_600.0), 32);
        assert_eq!(boxcar_len(768_000.0), 35);
        assert_eq!(boxcar_len(44_100.0), 2);
        assert_eq!(sinc4_half(2), vec![6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0]);
    }

    #[test]
    fn kernel_response_near_7khz() {
        let sr = 705_600.0;
        let hc = sinc4_half(boxcar_len(sr));
        let h = |f: f64| {
            let w = 2.0 * std::f64::consts::PI * f / sr;
            hc[0] + 2.0 * hc.iter().enumerate().skip(1).map(|(m, &c)| c * (w * m as f64).cos()).sum::<f64>()
        };
        assert!((h(7_000.0) - 0.5).abs() < 0.03, "{}", h(7_000.0));
        assert!(h(14_000.0) < 0.06 && h(14_000.0) >= 0.0, "{}", h(14_000.0));
        assert!(h(1_000.0) > 0.98);
        for f in (0..40).map(|k| k as f64 * 1_000.0) {
            assert!(h(f) > -1e-9 && h(f) < 1.0 + 1e-9, "{f}: {}", h(f));
        }
    }

    #[test]
    fn zero_envelope_gives_linear_exactly() {
        let n = 50_000;
        let (ll, lr, ml, mr) = (noise(n, 1), noise(n, 2), noise(n, 3), noise(n, 4));
        let (mut yl, mut yr) = (ll.clone(), lr.clone());
        let split = AlphaSplit::new(352_800.0, 352_800.0 / 86.0);
        blend_in_place(&split, &vec![0.0; 100], &mut yl, &ml, &mut yr, &mr);
        assert!(yl.iter().zip(&ll).all(|(a, b)| a.to_bits() == b.to_bits()));
        assert!(yr.iter().zip(&lr).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn ones_envelope_gives_minimum() {
        let n = 20_000;
        let (ll, lr, ml, mr) = (noise(n, 1), noise(n, 2), noise(n, 3), noise(n, 4));
        let (mut yl, mut yr) = (ll.clone(), lr.clone());
        let split = AlphaSplit::new(352_800.0, 352_800.0 / 100.0);
        blend_in_place(&split, &vec![1.0; 20], &mut yl, &ml, &mut yr, &mr);
        for i in 0..n {
            assert!((yl[i] - ml[i]).abs() < 1e-15 && (yr[i] - mr[i]).abs() < 1e-15, "{i}");
        }
    }

    #[test]
    fn gate_is_stateless_and_fades_at_the_frame_midpoint() {
        let sr = 96_000.0;
        let f = sr / 86.0;
        let split = AlphaSplit::new(sr, f);
        // Frames 0..2 at 0, from 3 on at 1: the mean over ±2 frames reaches
        // 0.5 at frame 3 (3 of 5 frames), so the gate opens between frames
        // 2 and 3.
        let env: Vec<f64> = (0..10).map(|j| if j >= 3 { 1.0 } else { 0.0 }).collect();
        let mid = (2.5 * f) as usize;
        let half = split.fade / 2;
        assert_eq!(split.weights(&env, mid - half - 2).1, 0.0);
        assert_eq!(split.weights(&env, mid + half + 2).1, 1.0);
        let s_mid = split.weights(&env, mid).1;
        assert!(s_mid > 0.3 && s_mid < 0.7, "{s_mid}");
        let mut prev = 0.0;
        for i in mid - half - 2..mid + half + 2 {
            let s = split.weights(&env, i).1;
            assert!(s >= prev, "gate falls back at {i}");
            prev = s;
        }
    }

    /// The in-RAM blend, the stream fed in any pieces and emitted in any
    /// pieces, all give the same bits.
    #[test]
    fn in_ram_and_stream_match_bit_for_bit() {
        let sr = 352_800.0;
        let f = sr / 86.0;
        let n = 150_000 + 777;
        let env = envelope((n as f64 / f) as usize + 4);
        let (ll, lr, ml, mr) = (noise(n, 11), noise(n, 12), noise(n, 13), noise(n, 14));
        let (mut yl, mut yr) = (ll.clone(), lr.clone());
        blend_in_place(&AlphaSplit::new(sr, f), &env, &mut yl, &ml, &mut yr, &mr);
        assert!(yl.iter().zip(&ll).any(|(a, b)| a != b), "the blend does something");
        for (feed, out) in [(4096usize, 4096usize), (1000, 333), (65_536, 7), (1, 5000)] {
            let mut st = AlphaSplitStream::new(AlphaSplit::new(sr, f), 0);
            let (mut sl, mut sr_) = (Vec::new(), Vec::new());
            let mut fed = 0;
            while sl.len() < n {
                let k = out.min(n - sl.len());
                while st.needs(k) > 0 {
                    if fed < n {
                        let e = (fed + feed).min(n);
                        st.push(&ll[fed..e], &lr[fed..e], &ml[fed..e], &mr[fed..e]);
                        fed = e;
                    } else {
                        let z = st.needs(k);
                        st.push_zeros(z);
                    }
                }
                let (mut a, mut b) = (vec![0.0; k], vec![0.0; k]);
                st.emit(&env, &mut a, &mut b, |_, _| {});
                sl.extend(a);
                sr_.extend(b);
            }
            assert!(sl.iter().zip(&yl).all(|(a, b)| a.to_bits() == b.to_bits()), "L {feed}/{out}");
            assert!(sr_.iter().zip(&yr).all(|(a, b)| a.to_bits() == b.to_bits()), "R {feed}/{out}");
        }
    }

    /// A tone at 16 kHz where `min` is `lin` inverted (the antiphase the
    /// split is for): with α at 0.5 the old blend cancels it, the split keeps
    /// it whole.
    #[test]
    fn antiphase_top_is_not_cancelled() {
        let sr = 352_800.0;
        let f = sr / 86.0;
        let n = 60_000;
        let lin: Vec<f64> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 16_000.0 * i as f64 / sr).sin()).collect();
        let min: Vec<f64> = lin.iter().map(|v| -v).collect();
        let env = vec![0.5; 30];
        let (mut yl, mut yr) = (lin.clone(), lin.clone());
        blend_in_place(&AlphaSplit::new(sr, f), &env, &mut yl, &min, &mut yr, &min);
        let rms = |v: &[f64]| (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
        let body = &yl[20_000..40_000];
        let ratio = rms(body) / rms(&lin[20_000..40_000]);
        assert!(ratio > 0.9, "16 kHz kept at {ratio}");
    }

    /// `gate_stats` counts the frames whose gate is on and the changes of
    /// `gate` from frame to frame.
    #[test]
    fn gate_stats_counts_frames_and_changes() {
        assert_eq!(gate_stats(&[0.0; 50]), (0, 0));
        assert_eq!(gate_stats(&[1.0; 50]), (50, 0));
        // Mean over 5 frames ≥ 0.5 (3 of 5 ones) at frames 8..=11 only.
        let mut env = vec![0.0; 20];
        env[8..=11].fill(1.0);
        assert_eq!(gate_stats(&env), (4, 2));
        let env = envelope(370);
        let (on, changes) = gate_stats(&env);
        let want_on = (0..370).filter(|&j| AlphaSplit::gate(&env, j) == 1.0).count();
        assert_eq!(on, want_on);
        assert!(changes > 0 && changes % 2 == 0, "{changes}");
    }

    /// CPU of the player's split: 1 s of 705.6 kHz stereo through
    /// `AlphaSplitStream::emit` in 4096-sample periods, the envelope crossing
    /// 0.5 all the time (the low-pass runs nearly everywhere).
    /// `cargo test --profile fast --bins alpha_split::tests::bench_emit -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_emit_705k_stereo() {
        let sr = 705_600.0;
        let f = sr / 86.13;
        let n = 705_600usize;
        let env: Vec<f64> = (0..(n as f64 / f) as usize + 8).map(|j| 0.3 + 0.4 * ((j % 7) as f64 / 6.0)).collect();
        let (ll, lr, ml, mr) = (noise(n, 1), noise(n, 2), noise(n, 3), noise(n, 4));
        for round in 0..3 {
            let mut st = AlphaSplitStream::new(AlphaSplit::new(sr, f), 0);
            let (mut a, mut b) = (vec![0.0; 4096], vec![0.0; 4096]);
            let (mut fed, mut done) = (0, 0);
            let t = std::time::Instant::now();
            while done < n {
                let k = 4096.min(n - done);
                while st.needs(k) > 0 {
                    if fed < n {
                        let e = (fed + 4096).min(n);
                        st.push(&ll[fed..e], &lr[fed..e], &ml[fed..e], &mr[fed..e]);
                        fed = e;
                    } else {
                        let z = st.needs(k);
                        st.push_zeros(z);
                    }
                }
                st.emit(&env, &mut a[..k], &mut b[..k], |_, _| {});
                done += k;
            }
            let ms = t.elapsed().as_secs_f64() * 1e3;
            println!("bench_emit round {round}: 1 s of 705.6k stereo in {ms:.1} ms ({:.1}% of one core)", ms / 10.0);
        }
    }
}
