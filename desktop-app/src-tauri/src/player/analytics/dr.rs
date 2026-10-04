//! TT Dynamic Range (DR) score as defined by the Pleasurize Music Foundation
//! and implemented by `dr14_t.meter` / `foo_dr_meter`.
//!
//! # Algorithm
//!
//! Reference: `simon-r/dr14_t.meter`, file `dr14tmeter/compute_dr14.py`
//! <https://github.com/simon-r/dr14_t.meter>
//!
//! Key facts (from DR-ALGORITHM.md, the exact port of dr14tmeter 1.0.10):
//!
//! 1. **Block length**: `3 × Fs` — but at 44100 Hz a correction of `delta_fs = 60`
//!    is added: `block_samples = 3 × (Fs + 60)` = 132 480.  All other rates use
//!    `3 × Fs` exactly.
//!
//! 2. **Number of blocks**: `seg_cnt = ⌊N / block_samples⌋ + 1`.  The last block
//!    is always included as a partial block (even when empty).
//!
//! 3. **Last (partial) block**: covers `Y[curr_sam : N-1]` — dropping the very
//!    last sample (known dr14tmeter off-by-one bug, replicated for byte-exact
//!    agreement).  When the signal length is an exact multiple of `block_samples`
//!    the partial block is empty (rms = 0, peak = 0).
//!
//! 4. **RMS formula**: `rms = √( 2 · Σ xᵢ² / N_block )` (√2 scaling).
//!
//! 5. **Sorting**: `rms` and `peaks` are sorted **independently** in ascending
//!    order (each channel separately).
//!
//! 6. **Best-block count**: `n_blk = max(1, ⌊seg_cnt × 0.20⌋)`.
//!
//! 7. **RMS of top blocks**: `rms_sum = Σ rms[top n_blk]²`; selected from the
//!    **ascending-sorted RMS** array (highest-RMS blocks).
//!
//! 8. **Second-highest peak**: `peaks[seg_cnt − 2]` from the ascending-sorted
//!    peaks array.  When `seg_cnt = 1` Python's index −1 wraps to index 0 (the
//!    only peak).
//!
//! 9. **DR per channel**: `−20 · log₁₀( √(rms_sum / n_blk) / peak2 )`.
//!    When `rms_sum < 1 / 2²⁴` (near-silence), `ch_dr = 0`.
//!
//! 10. **Track DR**: `round( mean(DR_L, DR_R) )` — uses Rust `f64::round()`
//!     (round-half-away-from-zero), which is close enough to Python banker's
//!     rounding for practical signals (differs only at exact half-integers).

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata for one 3-second block, exposed for the UI layer.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct BlockInfo {
    /// Sample index of the first sample in this block.
    pub start_sample: u64,
    /// Block RMS with √2 factor (linear, not dB).
    pub rms: f64,
    /// Block sample peak (linear).
    pub peak: f64,
    /// `true` when this block is among the top-20 % used for DR computation.
    pub in_top20: bool,
}

/// DR measurement results for one channel.
#[derive(Debug, Clone)]
pub struct ChannelDr {
    /// `−20 · log₁₀( rms_top / peak2 )` — exact f64 value.
    pub dr_exact: f64,
    /// `rms_top`: RMS of the top-20 % blocks (linear).
    pub rms_top: f64,
    /// Second-highest block peak across all blocks (linear).
    pub peak2: f64,
}

/// Full DR result for a stereo signal.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct DrResult {
    /// Left-channel measurement.
    pub l: ChannelDr,
    /// Right-channel measurement.
    pub r: ChannelDr,
    /// Track DR rounded to the nearest integer (the number displayed in DR meters).
    pub dr_rounded: i32,
    /// Track DR as exact f64 (mean of L and R `dr_exact`).
    pub dr_exact: f64,
    /// Number of blocks used (`seg_cnt` = full blocks + 1 partial).
    pub n_blocks: usize,
    /// Per-block metadata for the UI histogram layer.
    pub blocks_l: Vec<BlockInfo>,
    pub blocks_r: Vec<BlockInfo>,
}

// ─────────────────────────────────────────────────────────────────────────────
// One-shot computation
// ─────────────────────────────────────────────────────────────────────────────

/// DR14 block length for a given sample rate.
///
/// At 44 100 Hz the reference adds a `delta_fs = 60` correction:
/// `block_samples = 3 × (Fs + 60) = 132 480`.
/// All other rates: `block_samples = 3 × Fs`.
fn dr_block_samples(sample_rate: u32) -> usize {
    let delta: u32 = if sample_rate == 44_100 { 60 } else { 0 };
    3 * (sample_rate + delta) as usize
}

/// Compute the TT DR score for a complete stereo signal.
///
/// Returns `None` only when the signal is empty or both channels are silent
/// (peak ≤ 0 after computing RMS).
pub fn compute_dr(l: &[f64], r: &[f64], sample_rate: u32) -> Option<DrResult> {
    let n = l.len().min(r.len());
    if n == 0 {
        return None;
    }

    let block_samples = dr_block_samples(sample_rate);
    if block_samples == 0 {
        return None;
    }

    // seg_cnt = floor(N / block_samples) + 1  (always ≥ 1)
    let n_full_blocks = n / block_samples;
    let seg_cnt = n_full_blocks + 1;

    // ── Build per-block (rms, peak, start_sample) for one channel ─────────────
    let build_blocks = |ch: &[f64]| -> Vec<(f64, f64, u64)> {
        let mut out = Vec::with_capacity(seg_cnt);

        // Full blocks
        for k in 0..n_full_blocks {
            let start = k * block_samples;
            let blk = &ch[start..start + block_samples];
            let sum_sq: f64 = blk.iter().map(|&x| x * x).sum();
            let rms = (2.0 * sum_sq / block_samples as f64).sqrt();
            let peak = blk.iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);
            out.push((rms, peak, start as u64));
        }

        // Partial last block: Y[curr_sam : n-1]  (drops the very last sample)
        let curr_sam = n_full_blocks * block_samples;
        // Range curr_sam..n-1 (exclusive end = n-1 drops last sample)
        let partial_end = n.saturating_sub(1); // n − 1, clamped at 0
        if curr_sam < partial_end {
            let blk = &ch[curr_sam..partial_end];
            let blk_n = blk.len() as f64;
            let sum_sq: f64 = blk.iter().map(|&x| x * x).sum();
            let rms = (2.0 * sum_sq / blk_n).sqrt();
            let peak = blk.iter().map(|&x| x.abs()).fold(0.0_f64, f64::max);
            out.push((rms, peak, curr_sam as u64));
        } else {
            // Empty partial block (signal is exact multiple of block_samples)
            out.push((0.0, 0.0, curr_sam as u64));
        }

        out
    };

    let raw_l = build_blocks(l);
    let raw_r = build_blocks(r);

    debug_assert_eq!(raw_l.len(), seg_cnt);
    debug_assert_eq!(raw_r.len(), seg_cnt);

    dr_from_blocks(&raw_l, &raw_r)
}

/// DR from per-block `(rms, peak, start_sample)` of both channels, the last
/// block the partial one (possibly `(0, 0)`), as `compute_dr` builds them.
fn dr_from_blocks(raw_l: &[(f64, f64, u64)], raw_r: &[(f64, f64, u64)]) -> Option<DrResult> {
    let seg_cnt = raw_l.len();
    if seg_cnt == 0 || raw_r.len() != seg_cnt {
        return None;
    }
    // n_blk = max(1, floor(seg_cnt × 0.20))
    let n_blk = ((seg_cnt as f64 * 0.20).floor() as usize).max(1);

    // ── Per-channel DR ─────────────────────────────────────────────────────────
    let channel_dr_fn = |raw: &[(f64, f64, u64)]| -> Option<(ChannelDr, Vec<bool>)> {
        let nc = raw.len(); // = seg_cnt

        // Sort peaks independently (ascending) to find second-highest peak
        let mut sorted_peaks: Vec<f64> = raw.iter().map(|&(_, p, _)| p).collect();
        sorted_peaks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Second-highest peak: index nc-2 in ascending sort.
        // When nc=1, Python's index -1 wraps to 0 — same element.
        let peak2_idx = nc.saturating_sub(2);
        let peak2 = sorted_peaks[peak2_idx];

        // Sort rms independently (ascending) to select top n_blk
        // We need to track original indices for in_top20 annotation.
        let mut rms_order: Vec<usize> = (0..nc).collect();
        rms_order.sort_unstable_by(|&a, &b| {
            raw[a].0.partial_cmp(&raw[b].0).unwrap_or(std::cmp::Ordering::Equal)
        });

        // Top n_blk are the last n_blk entries in ascending order.
        let top_start = nc - n_blk;
        let rms_sum: f64 = rms_order[top_start..]
            .iter()
            .map(|&i| raw[i].0 * raw[i].0)
            .sum();

        // Near-silence threshold: 1 / 2^24
        const SILENCE_THRESHOLD: f64 = 1.0 / (1u32 << 24) as f64;

        let dr_exact = if rms_sum < SILENCE_THRESHOLD {
            0.0
        } else {
            if peak2 <= 0.0 {
                return None;
            }
            let rms_top = (rms_sum / n_blk as f64).sqrt();
            -20.0 * (rms_top / peak2).log10()
        };

        let rms_top = if rms_sum < SILENCE_THRESHOLD {
            0.0
        } else {
            (rms_sum / n_blk as f64).sqrt()
        };

        // Build in_top20 annotation (indexed by original block order)
        let mut in_top20 = vec![false; nc];
        for &i in &rms_order[top_start..] {
            in_top20[i] = true;
        }

        Some((ChannelDr { dr_exact, rms_top, peak2 }, in_top20))
    };

    let (ch_l, top20_l) = channel_dr_fn(raw_l)?;
    let (ch_r, top20_r) = channel_dr_fn(raw_r)?;

    let dr_exact = (ch_l.dr_exact + ch_r.dr_exact) / 2.0;
    let dr_rounded = dr_exact.round() as i32;

    // ── BlockInfo ──────────────────────────────────────────────────────────────
    let make_infos = |raw: &[(f64, f64, u64)], top20: &[bool]| -> Vec<BlockInfo> {
        raw.iter()
            .zip(top20.iter())
            .map(|(&(rms, peak, start_sample), &in_top)| BlockInfo {
                start_sample,
                rms,
                peak,
                in_top20: in_top,
            })
            .collect()
    };

    Some(DrResult {
        l: ch_l,
        r: ch_r,
        dr_rounded,
        dr_exact,
        n_blocks: seg_cnt,
        blocks_l: make_infos(raw_l, &top20_l),
        blocks_r: make_infos(raw_r, &top20_r),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming push
// ─────────────────────────────────────────────────────────────────────────────

/// Streaming DR accumulator.
///
/// Feed blocks of any size via [`push`]; call [`finish`] once all samples
/// have been pushed.  Internally buffers the full signal — DR computation
/// requires a global sort across all blocks.
///
/// [`push`]: DrStreamer::push
/// [`finish`]: DrStreamer::finish
#[cfg(test)]
pub struct DrStreamer {
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    sample_rate: u32,
}

#[cfg(test)]
impl DrStreamer {
    pub fn new(sample_rate: u32) -> Self {
        Self { buf_l: Vec::new(), buf_r: Vec::new(), sample_rate }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        self.buf_l.extend_from_slice(l);
        self.buf_r.extend_from_slice(r);
    }

    pub fn finish(self) -> Option<DrResult> {
        compute_dr(&self.buf_l, &self.buf_r, self.sample_rate)
    }
}

/// DR of a stream so far, holding only the per-block stats (a few bytes per
/// 3 s, so a live output at any rate): the unfinished block counts as the
/// partial last one, as in `compute_dr`.
pub struct DrLive {
    block: usize,
    fill: usize,
    start: u64,
    sq: [f64; 2],
    pk: [f64; 2],
    raw_l: Vec<(f64, f64, u64)>,
    raw_r: Vec<(f64, f64, u64)>,
}

impl DrLive {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            block: dr_block_samples(sample_rate).max(1),
            fill: 0,
            start: 0,
            sq: [0.0; 2],
            pk: [0.0; 2],
            raw_l: Vec::new(),
            raw_r: Vec::new(),
        }
    }

    /// The signal goes on at `sample_rate` (a stream's output reopened at
    /// another rate): the block begun so far counts as one, the next ones
    /// last 3 s at the new rate.
    pub fn rate_changed(&mut self, sample_rate: u32) {
        let block = dr_block_samples(sample_rate).max(1);
        if block == self.block {
            return;
        }
        if self.fill > 0 {
            let (bl, br) = self.current();
            self.raw_l.push(bl);
            self.raw_r.push(br);
            self.start += self.fill as u64;
            self.fill = 0;
            self.sq = [0.0; 2];
            self.pk = [0.0; 2];
        }
        self.block = block;
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        for (&a, &b) in l.iter().zip(r) {
            self.sq[0] += a * a;
            self.sq[1] += b * b;
            self.pk[0] = self.pk[0].max(a.abs());
            self.pk[1] = self.pk[1].max(b.abs());
            self.fill += 1;
            if self.fill == self.block {
                let (bl, br) = self.current();
                self.raw_l.push(bl);
                self.raw_r.push(br);
                self.start += self.block as u64;
                self.fill = 0;
                self.sq = [0.0; 2];
                self.pk = [0.0; 2];
            }
        }
    }

    fn current(&self) -> ((f64, f64, u64), (f64, f64, u64)) {
        if self.fill == 0 {
            return ((0.0, 0.0, self.start), (0.0, 0.0, self.start));
        }
        let n = self.fill as f64;
        ((( 2.0 * self.sq[0] / n).sqrt(), self.pk[0], self.start),
         (( 2.0 * self.sq[1] / n).sqrt(), self.pk[1], self.start))
    }

    /// `None` before any sample.
    pub fn result(&self) -> Option<DrResult> {
        if self.raw_l.is_empty() && self.fill == 0 {
            return None;
        }
        let (bl, br) = self.current();
        let mut l = self.raw_l.clone();
        let mut r = self.raw_r.clone();
        l.push(bl);
        r.push(br);
        dr_from_blocks(&l, &r)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn sine_block(n: usize, freq: f64, fs: u32, amp: f64) -> Vec<f64> {
        (0..n)
            .map(|i| amp * f64::sin(2.0 * PI * freq / fs as f64 * i as f64))
            .collect()
    }

    /// Produce a signal with `n_blocks` identical 3-second sine blocks of the
    /// given amplitude.  All blocks have the same RMS and peak, so DR should
    /// be well-defined and finite.
    fn uniform_signal(n_blocks: usize, fs: u32, amp: f64) -> Vec<f64> {
        let block = 3 * fs as usize;
        sine_block(block * n_blocks, 1000.0, fs, amp)
    }

    // ── Basic sanity ─────────────────────────────────────────────────────────

    /// Empty signal → None.
    #[test]
    fn empty_returns_none() {
        let fs = 48_000u32;
        assert!(compute_dr(&[], &[], fs).is_none());
    }

    /// Silence → Some with dr=0.  The reference (dr14tmeter) sets ch_dr=0 when
    /// rms_sum < 1/2^24 rather than returning an error.
    #[test]
    fn silence_returns_zero_dr() {
        let fs = 48_000u32;
        let sig = vec![0.0; 3 * fs as usize * 10];
        let result = compute_dr(&sig, &sig, fs);
        assert!(result.is_some(), "silence should return Some(dr=0)");
        assert_eq!(result.unwrap().dr_rounded, 0);
    }

    /// Even a single partial block (< 3 s) should produce a result.
    #[test]
    fn short_signal_produces_result() {
        // 1 second of sine at 48 kHz — far shorter than one 3-second block.
        let fs = 48_000u32;
        let sig = sine_block(fs as usize, 1000.0, fs, 0.5);
        // With new algorithm: n_full_blocks=0, seg_cnt=1, n_blk=1 → Some.
        assert!(compute_dr(&sig, &sig, fs).is_some());
    }

    /// Block size at 44100 Hz must include the delta_fs = 60 correction.
    #[test]
    fn block_samples_44100_uses_delta_fs() {
        assert_eq!(dr_block_samples(44_100), 132_480);
        assert_eq!(dr_block_samples(48_000), 144_000);
        assert_eq!(dr_block_samples(96_000), 288_000);
    }

    // ── DR formula verification ───────────────────────────────────────────────

    /// With identical blocks all at amplitude A, all peaks equal A, all RMS
    /// equal A/√2 · √2 = A (with the √2 factor in the spec).
    /// DR = −20·log₁₀(rms_top / peak2) = −20·log₁₀(A / A) = 0 dB.
    #[test]
    fn identical_blocks_dr_near_zero() {
        let fs = 48_000u32;
        let sig = uniform_signal(10, fs, 0.8);
        let result = compute_dr(&sig, &sig, fs).unwrap();
        assert!(
            result.dr_exact.abs() < 0.5,
            "expected DR ≈ 0 dB for uniform signal, got {:.2}",
            result.dr_exact
        );
    }

    /// The √2 factor in the RMS: a full-cycle sine at amplitude A has
    /// spec_rms = √(2 · (A²/2)) = A.  Check it against the formula directly.
    #[test]
    fn rms_sqrt2_factor_correct() {
        let fs = 48_000u32;
        let block_n = 3 * fs as usize;
        let sig: Vec<f64> = (0..block_n)
            .map(|i| f64::sin(2.0 * PI * i as f64 / block_n as f64))
            .collect();
        let sum_sq: f64 = sig.iter().map(|&x| x * x).sum();
        let rms_spec = (2.0 * sum_sq / block_n as f64).sqrt();
        let amp = 1.0_f64;
        assert!(
            (rms_spec - amp).abs() < 0.01,
            "rms_spec={:.4} amp={:.4}", rms_spec, amp
        );
    }

    // ── n_blocks and partial block ────────────────────────────────────────────

    /// A partial tail block (< 3 s) is included — seg_cnt = n_full + 1.
    #[test]
    fn partial_tail_included() {
        let fs = 48_000u32;
        let block = 3 * fs as usize;
        let n_blocks = 6;
        let n_exact = block * n_blocks;
        let n_extra = n_exact + block / 2; // half a trailing block

        let sig = uniform_signal(n_extra / block + 1, fs, 0.5);
        let sig = &sig[..n_extra];

        let r = compute_dr(sig, sig, fs).unwrap();
        // seg_cnt = 6 full + 1 partial = 7
        assert_eq!(r.n_blocks, n_blocks + 1,
            "expected seg_cnt = n_full+1 = {}", n_blocks + 1);
    }

    /// Signal that is an exact multiple of block_samples: seg_cnt = n_full + 1
    /// with an empty partial block of rms=0/peak=0.
    #[test]
    fn exact_multiple_adds_empty_partial() {
        let fs = 48_000u32;
        let sig = uniform_signal(6, fs, 0.5);
        // sig.len() = 6 * 3 * 48000 = 864000, exact multiple
        let r = compute_dr(&sig, &sig, fs).unwrap();
        assert_eq!(r.n_blocks, 7); // 6 full + 1 empty partial
    }

    // ── Streaming == one-shot ─────────────────────────────────────────────────

    #[test]
    fn streaming_matches_oneshot() {
        let fs = 44_100u32;
        let sig = uniform_signal(8, fs, 0.7);

        let oneshot = compute_dr(&sig, &sig, fs).unwrap();

        let mut streamer = DrStreamer::new(fs);
        let mut i = 0;
        let chunk = 7919; // prime chunk size
        while i < sig.len() {
            let end = (i + chunk).min(sig.len());
            streamer.push(&sig[i..end], &sig[i..end]);
            i = end;
        }
        let streaming = streamer.finish().unwrap();

        assert_eq!(oneshot.n_blocks, streaming.n_blocks);
        assert!(
            (oneshot.dr_exact - streaming.dr_exact).abs() < 1e-10,
            "one-shot={:.6} streaming={:.6}", oneshot.dr_exact, streaming.dr_exact
        );
    }

    // ── BlockInfo annotation ──────────────────────────────────────────────────

    /// The count of `in_top20 == true` blocks must equal `n_blk`.
    #[test]
    fn block_info_top20_count() {
        let fs = 48_000u32;
        let sig = uniform_signal(10, fs, 0.6);
        let result = compute_dr(&sig, &sig, fs).unwrap();
        let count_l = result.blocks_l.iter().filter(|b| b.in_top20).count();
        // n_blk = max(1, floor(seg_cnt * 0.2)) — must match annotation count
        let n_blk = ((result.n_blocks as f64 * 0.2).floor() as usize).max(1);
        assert_eq!(count_l, n_blk,
            "in_top20 count={} but n_blk={}", count_l, n_blk);
    }

    #[test]
    fn dr_live_matches_compute_dr() {
        let fs = 44_100u32;
        // 20 s: loud and quiet 3 s stretches, so the blocks differ.
        let n = fs as usize * 20;
        let l: Vec<f64> = (0..n).map(|i| {
            let amp = if (i / (fs as usize * 3)) % 2 == 0 { 0.9 } else { 0.1 };
            amp * (2.0 * PI * 997.0 * i as f64 / fs as f64).sin()
        }).collect();
        let r: Vec<f64> = l.iter().map(|v| v * 0.5).collect();
        let whole = compute_dr(&l, &r, fs).unwrap();
        let mut live = DrLive::new(fs);
        for (cl, cr) in l.chunks(4410).zip(r.chunks(4410)) { live.push(cl, cr); }
        let got = live.result().unwrap();
        assert_eq!(got.n_blocks, whole.n_blocks);
        // compute_dr drops the very last sample of the partial block.
        assert!((got.dr_exact - whole.dr_exact).abs() < 0.01, "{} vs {}", got.dr_exact, whole.dr_exact);
        assert!(DrLive::new(fs).result().is_none());
    }
}
