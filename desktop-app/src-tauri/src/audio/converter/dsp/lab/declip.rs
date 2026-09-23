use rustfft::FftPlanner;
use super::stats::SourceStats;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct DeclipReport {
    pub threshold_dbfs: f64,
    pub regions_total: usize,
    pub short_fixed: usize,
    pub plateaus_fixed: usize,
    pub longest_ms: f64,
    pub unrecoverable: usize,
    pub clipped_pct: f64,
    pub transient_skipped: usize,
    /// Half-open sample spans actually rewritten, per channel. Downstream
    /// passes (ISP) must not "correct" these — the reconstruction is
    /// deliberately above the old rail; level safety is the output true-peak
    /// stage's job, not theirs.
    pub repaired_l: Vec<(usize, usize)>,
    pub repaired_r: Vec<(usize, usize)>,
    /// Detected but left untouched (too long / transient-guarded), per channel.
    /// Nothing in the conversion reads these back — unlike `repaired_*`, which
    /// ISP must see. They are the record of what the pass decided against, for
    /// anything that wants to show its work.
    #[allow(dead_code)]
    pub untouched_l: Vec<(usize, usize)>,
    #[allow(dead_code)]
    pub untouched_r: Vec<(usize, usize)>,
}

#[derive(Debug)]
pub enum DeclipSkip {
    NoSignature,
    TooFew,
    /// Samples reach the rail, but never stay on it: the longest run is
    /// shorter than `MIN_RUN_GATE`. A limiter ceiling, not clipping.
    NoPlateaus { longest: usize },
    Cancelled,
}

// ── AR helpers (Levinson-Durbin) ────────────────────────────────────────────

fn levinson_durbin(r: &[f64], p: usize) -> Option<Vec<f64>> {
    if p == 0 || r.is_empty() || r[0] == 0.0 {
        return None;
    }
    let mut a = vec![0.0f64; p + 1];
    let mut err = r[0];
    let mut tmp = vec![0.0f64; p + 1];
    for i in 1..=p {
        let mut lambda = 0.0;
        for j in 1..i {
            lambda += a[j] * r[i - j];
        }
        lambda = (r[i] - lambda) / err;
        tmp[i] = lambda;
        for j in 1..i {
            tmp[j] = a[j] - lambda * a[i - j];
        }
        for j in 1..=i {
            a[j] = tmp[j];
        }
        err *= 1.0 - lambda * lambda;
        if err <= 0.0 {
            break;
        }
    }
    Some(a)
}

fn autocorrelation(samples: &[f64], p: usize) -> Vec<f64> {
    let n = samples.len();
    let mut r = vec![0.0f64; p + 1];
    for lag in 0..=p {
        let mut sum = 0.0;
        for i in 0..(n - lag) {
            sum += samples[i] * samples[i + lag];
        }
        r[lag] = sum;
    }
    r
}

// ── Threshold detection ──────────────────────────────────────────────────────

fn detect_threshold_from_histogram(hist: &[u64]) -> Option<f64> {
    let nbins = hist.len();
    if nbins < 64 {
        return None;
    }
    let min_bin = (0.5 * nbins as f64) as usize;
    for b in (min_bin..nbins).rev() {
        if hist[b] == 0 || b < 32 {
            continue;
        }
        let lo = b.saturating_sub(32);
        let mut window: Vec<u64> = hist[lo..b].to_vec();
        window.sort_unstable();
        let median = window[window.len() / 2];
        if median > 0 && hist[b] as f64 >= 8.0 * median as f64 {
            let center = (b as f64 + 0.5) / nbins as f64;
            return Some(center);
        }
    }
    None
}

fn flat_top_runs_in_channel(channel: &[f64], peak: f64) -> usize {
    let threshold = 0.9995 * peak;
    let mut count = 0;
    let n = channel.len();
    let mut i = 0;
    while i < n {
        if channel[i].abs() >= threshold {
            let start = i;
            while i < n && channel[i].abs() >= threshold {
                i += 1;
            }
            if i - start >= 2 {
                let run_slice = &channel[start..i];
                let mn = run_slice.iter().cloned().fold(f64::INFINITY, f64::min);
                let mx = run_slice.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                if mx - mn < 1e-6 {
                    count += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    count
}

/// Where the clipper actually stopped, read separately for the two rails.
///
/// `theta` comes from a histogram taken *before* the pipeline removes the
/// per-channel DC offset (the 16-bit comb test needs the raw signal). The
/// samples handed to the repair have had that offset taken out, which slides
/// the two rails in opposite directions by it. The gate under the rail is
/// only 1e-4 wide, so an offset of 1.3e-4 — an ordinary value, and exactly
/// what the track this was found on carries — drops the whole negative rail
/// below it. Every negative plateau then reads as clean signal and is left
/// clipped: on that file, 225 of 229 negative regions in the drum fragment
/// came out of the converter untouched while 167 of 223 positive ones were
/// repaired.
///
/// So read the rails off the data instead. Inside ±1 dB of the nominal rail
/// the level a clipper held is a spike, and a spike is what this looks for.
/// No spike on one side means that polarity was never clipped, and the
/// nominal rail is kept — a one-sided clip must not widen the gate on the
/// clean side.
fn rails_from_channel(channel: &[f64], theta: f64) -> (f64, f64) {
    const BINS: usize = 4096;
    let lo = theta * 10f64.powf(-1.0 / 20.0);
    let hi = theta * 10f64.powf(1.0 / 20.0);
    let span = hi - lo;
    if !(span > 0.0) {
        return (theta, theta);
    }

    // Per polarity: how many samples land in each bin, and the smallest
    // magnitude seen there — the plateau value itself, not the bin centre.
    let mut count = [vec![0u32; BINS], vec![0u32; BINS]];
    let mut floor = [vec![f64::INFINITY; BINS], vec![f64::INFINITY; BINS]];
    for &x in channel {
        let v = x.abs();
        if v >= lo && v <= hi {
            let b = ((((v - lo) / span) * BINS as f64) as usize).min(BINS - 1);
            let s = usize::from(x < 0.0);
            count[s][b] += 1;
            if v < floor[s][b] {
                floor[s][b] = v;
            }
        }
    }

    let pick = |c: &[u32], f: &[f64]| -> f64 {
        let (b, &n) = match c.iter().enumerate().max_by_key(|&(_, n)| *n) {
            Some(v) => v,
            None => return theta,
        };
        if n < 64 {
            return theta;
        }
        // A rail stands over its neighbourhood; a merely loud passage does
        // not. Median of the populated bins below the peak is the yardstick —
        // the same 8x signature `detect_threshold_from_histogram` looks for,
        // read at a finer scale.
        let mut below: Vec<u32> = c[..b].iter().copied().filter(|&v| v > 0).collect();
        if !below.is_empty() {
            below.sort_unstable();
            if (n as f64) < 8.0 * below[below.len() / 2] as f64 {
                return theta;
            }
        }
        if f[b].is_finite() {
            f[b]
        } else {
            theta
        }
    };

    (pick(&count[0], &floor[0]), pick(&count[1], &floor[1]))
}

// ── Plateau / region detection ───────────────────────────────────────────────

struct Region {
    start: usize,
    end: usize, // exclusive
    /// Polarity of the rail this region sits on. Part of the detector's
    /// contract and asserted in its tests; the repair takes polarity from
    /// the per-sample masks instead.
    #[allow(dead_code)]
    sign: f64,
}

fn detect_regions(channel: &[f64], theta: f64) -> Vec<Region> {
    // Rail band: |x| >= theta_low.  Gaps bridgeable when gap samples >= gap_min.
    let theta_low = theta * 10f64.powf(-1.0 / 20.0);
    let gap_min = theta * 10f64.powf(-3.0 / 20.0);
    let n = channel.len();
    let mut regions = Vec::new();

    for &sign in &[1.0f64, -1.0f64] {
        let mut i = 0;
        while i < n {
            // Look for a rail-band sample with matching polarity.
            if channel[i] * sign >= theta_low {
                let start = i;
                let mut end = i + 1;
                i = end;

                // Grow region, bridging small interior gaps.
                loop {
                    // Skip consecutive rail-band samples of same polarity.
                    while i < n && channel[i] * sign >= theta_low {
                        i += 1;
                    }
                    end = i;

                    // Try to bridge a gap of up to 8 samples.
                    let gap_start = i;
                    let mut gap_pos = i;
                    let mut gap_ok = true;
                    while gap_pos < n && gap_pos - gap_start < 8 {
                        if channel[gap_pos].abs() >= gap_min {
                            gap_pos += 1;
                        } else {
                            gap_ok = false;
                            break;
                        }
                    }

                    if gap_ok && gap_pos < n && channel[gap_pos] * sign >= theta_low {
                        // Bridge the gap.
                        i = gap_pos;
                    } else {
                        break;
                    }
                }

                // Region is [start, end).  Check quality: >= 60% in rail band, length >= 2.
                let region_len = end - start;
                if region_len >= 2 {
                    let in_rail = (start..end)
                        .filter(|&k| channel[k] * sign >= theta_low)
                        .count();
                    if in_rail as f64 / region_len as f64 >= 0.60 {
                        regions.push(Region { start, end, sign });
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    regions
}

// ── SPAIN-style iterative hard thresholding ───────────────────────────────────

/// Solve the symmetric positive-definite system G x = b (size m) in place via
/// Gaussian elimination with partial pivoting. Returns None when singular.
fn solve_sym(gram: &mut [f64], rhs: &mut [f64], m: usize) -> Option<Vec<f64>> {
    for col in 0..m {
        // Pivot.
        let mut piv = col;
        let mut piv_abs = gram[col * m + col].abs();
        for r in (col + 1)..m {
            let a = gram[r * m + col].abs();
            if a > piv_abs {
                piv_abs = a;
                piv = r;
            }
        }
        if piv_abs < 1e-14 {
            return None;
        }
        if piv != col {
            for c in 0..m {
                gram.swap(col * m + c, piv * m + c);
            }
            rhs.swap(col, piv);
        }
        let d = gram[col * m + col];
        for r in (col + 1)..m {
            let f = gram[r * m + col] / d;
            if f == 0.0 {
                continue;
            }
            for c in col..m {
                gram[r * m + c] -= f * gram[col * m + c];
            }
            rhs[r] -= f * rhs[col];
        }
    }
    let mut x = vec![0.0f64; m];
    for col in (0..m).rev() {
        let mut acc = rhs[col];
        for c in (col + 1)..m {
            acc -= gram[col * m + c] * x[c];
        }
        x[col] = acc / gram[col * m + col];
    }
    Some(x)
}
/// How far above the rail a reconstruction may reach, as a factor. It is a
/// guard against a bad fit, not a target: measured repairs on real loudness-war
/// masters land around 2.3x, so this leaves room without ever being the thing
/// that shapes the result.
const CEILING_OVER_RAIL: f64 = 4.0;

// ── Constrained Janssen interpolation ────────────────────────────────────────
//
// A clipped sample is not unknown. It carries a lower bound — the signal was at
// least as loud as the rail — and its untouched flanks carry the curvature the
// missing arc had to follow. Both facts go into the same small linear system.
//
// Fit an AR model to the window, solve for the values at the flattened
// positions that minimise the prediction residual, then pin back to the rail
// anything the solver placed below it (which is impossible) and solve again
// with those treated as known. Re-estimate the model from the filled signal
// and repeat. The Hessian of that objective is the autocorrelation of the AR
// coefficients, which is why this is one dense solve and not an iteration.
//
// Parameters were chosen by measurement, scored against the float decode of a
// lossy master — an independent reconstruction produced by the codec synthesis
// filterbank rather than by this code. On the reference fragment the result is
// 43.3 dB SNR at the clipped samples against 20.8 dB untouched, with a
// peak-height error of 0.07 dB RMS. Wider context measured *worse*: past about
// a thousand samples either side the model stops being local.

const AR_ORDER: usize = 192;
/// Analysis window and hop. Every unknown is solved in two windows that see
/// different context, and the two fits are crossfaded at the seam. A cheaper
/// form — one window placed around each run — was measured at 1.7 dB worse on
/// drums, and the saving is not worth having next to a 30M-tap convolution
/// that already runs for minutes.
const WINDOW: usize = 4096;
const HOP: usize = 2048;
/// Largest system handed to the solver at once. `solve_sym` is plain Gaussian
/// elimination, so this bounds a cubic cost that real music never approaches
/// (2-8 % clipped puts 100-350 unknowns in a window) but a heavily clipped
/// tone does. Above it the unknowns are solved in position-ordered blocks,
/// each seeing the others at their current values.
const MAX_SOLVE: usize = 512;
/// AR re-estimation passes. A third measured identical to the second.
const OUTER_ROUNDS: usize = 2;
/// Rail active-set passes inside one round.
const ACTIVE_ROUNDS: usize = 4;
/// Longest flat run the repair will touch, in samples (8.7 ms at 44.1 kHz).
/// Not a property of the solver — it will happily fill any length — but of the
/// evidence: accuracy was measured out to runs of 253 samples, and past that
/// there is nothing to justify drawing 20 ms of audio that no longer exists.
/// Longer runs are reported unrecoverable and left exactly as they are.
const MAX_RUN: usize = 384;

/// A run on the rail shorter than this is a limiter touching its ceiling,
/// not a clipper. Measured on Santana — Corazón (2014), a brickwall master
/// with 2,028 samples parked on one code in a channel and no clipping at all:
/// at a 12-LSB tolerance the longest run on any of its twelve tracks is 11
/// samples. Real clipping looks nothing like that — Rich Girl carries 4,596
/// runs of 17–64 samples and 618 of 65–384. Without this gate the solver
/// drew arcs over 4,359 one- and two-sample touches on Mal Bicho, lifted the
/// true peak from +1.41 to +3.62 dBTP, and the output ceiling took the track
/// down 0.83 dB further than the release build did — for nothing. A file
/// whose longest run is under the gate has nothing this stage can repair.
pub const MIN_RUN_GATE: usize = 17;

/// Longest run of consecutive samples on either rail, through the same gate
/// the repair uses.
fn longest_rail_run(channel: &[f64], rails: (f64, f64)) -> usize {
    let (gate_hi, gate_lo) = (rails.0 * (1.0 - 1e-4), rails.1 * (1.0 - 1e-4));
    let mut longest = 0usize;
    let mut cur = 0usize;
    for &x in channel {
        if x >= gate_hi || x <= -gate_lo {
            cur += 1;
            if cur > longest {
                longest = cur;
            }
        } else {
            cur = 0;
        }
    }
    longest
}

/// R[k] = Σ_i b_i b_{i+k} — the autocorrelation of the coefficient vector,
/// which is exactly the Hessian of the prediction-error objective.
fn coef_autocorr(b: &[f64]) -> Vec<f64> {
    let p = b.len() - 1;
    let mut r = vec![0.0f64; p + 1];
    for k in 0..=p {
        let mut acc = 0.0;
        for i in 0..=(p - k) {
            acc += b[i] * b[i + k];
        }
        r[k] = acc;
    }
    r
}

/// AR coefficients for `seg` in residual form, b[0] = 1, so that the residual
/// at n is Σ_j b_j x[n-j]. `levinson_durbin` returns the prediction form, so
/// the sign is flipped on the way out.
fn ar_model(seg: &[f64], order: usize) -> Option<Vec<f64>> {
    let ord = order.min(seg.len() / 3).max(4);
    let mut r = autocorrelation(seg, ord);
    if r[0] <= 0.0 {
        return None;
    }
    r[0] *= 1.0001; // ridge — keeps Levinson stable on near-singular frames
    let a = levinson_durbin(&r, ord)?;
    if a.len() <= ord {
        return None;
    }
    let mut b = vec![0.0f64; ord + 1];
    b[0] = 1.0;
    for j in 1..=ord {
        b[j] = -a[j];
    }
    Some(b)
}

// ── Per-channel repair ────────────────────────────────────────────────────────

struct ChannelRepairResult {
    regions_total: usize,
    short_fixed: usize,
    plateaus_fixed: usize,
    longest_samples: usize,
    unrecoverable: usize,
    transient_skipped: usize,
    repaired: Vec<(usize, usize)>,
    untouched: Vec<(usize, usize)>,
}

fn repair_channel(
    channel: &mut Vec<f64>,
    theta: f64,
    rails: (f64, f64),
    _sample_rate: u32,
    _planner: &mut FftPlanner<f64>,
    cancel: &AtomicBool,
) -> ChannelRepairResult {
    let n = channel.len();
    // Region detection bridges small gaps and demands 60% rail occupancy, so
    // it describes a clipped *event* better than a raw run of rail samples
    // does. The repair works on raw runs — a sample below the rail carries its
    // own value — but the report quotes regions, which is what a listener
    // would call one clipped moment.
    let regions = detect_regions(channel, theta);
    let regions_total = regions.len();
    let widest_region = regions.iter().map(|r| r.end - r.start).max().unwrap_or(0);

    // One gate per rail: the two stop being symmetric as soon as anything
    // upstream shifts the signal (see `rails_from_channel`).
    let (theta_hi, theta_lo) = rails;
    let gate_hi = theta_hi * (1.0 - 1e-4);
    let gate_lo = theta_lo * (1.0 - 1e-4);
    let mut hi_mask = vec![false; n];
    let mut lo_mask = vec![false; n];
    for i in 0..n {
        if channel[i] >= gate_hi {
            hi_mask[i] = true;
        } else if channel[i] <= -gate_lo {
            lo_mask[i] = true;
        }
    }

    let mut out = ChannelRepairResult {
        regions_total,
        short_fixed: 0,
        plateaus_fixed: 0,
        longest_samples: 0,
        unrecoverable: 0,
        transient_skipped: 0,
        repaired: Vec::new(),
        untouched: Vec::new(),
    };

    // Runs first: anything longer than MAX_RUN is refused outright rather than
    // half-filled, so its samples never enter the solver.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    {
        let mut i = 0;
        while i < n {
            if hi_mask[i] || lo_mask[i] {
                let mut j = i;
                while j + 1 < n && (hi_mask[j + 1] || lo_mask[j + 1]) {
                    j += 1;
                }
                runs.push((i, j + 1));
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }
    out.longest_samples =
        widest_region.max(runs.iter().map(|&(a, b)| b - a).max().unwrap_or(0));

    let mut unknown = vec![false; n];
    let mut any = false;
    for &(a, b) in &runs {
        if b - a <= MAX_RUN {
            for i in a..b {
                unknown[i] = true;
            }
            any = true;
        }
    }
    if !any {
        out.unrecoverable = runs.len();
        out.untouched = runs.clone();
        return out;
    }

    let ceiling_hi = CEILING_OVER_RAIL * theta_hi;
    let ceiling_lo = CEILING_OVER_RAIL * theta_lo;
    let mut solved = vec![false; n];

    let mut st = 0usize;
    let mut win_no = 0usize;
    loop {
        win_no += 1;
        if win_no % 16 == 0 && cancel.load(Ordering::Relaxed) {
            break;
        }
        let en = (st + WINDOW).min(n);
        let m = en - st;
        // ar_model drops the order to fit a short buffer, so the window only
        // has to be long enough to carry a usable model at all.
        let long_enough = m >= 128;
        let local: Vec<usize> = if long_enough {
            (0..m).filter(|&i| unknown[st + i]).collect()
        } else {
            Vec::new()
        };

        // Skip a window with nothing to do, and one with too little clean data
        // left to fit: the requirement is a number of undamaged samples, not a
        // ratio. Twice the model order is the floor — below that the fit starts
        // describing the damage instead of the signal.
        let clean = m - local.len();
        if !local.is_empty() && clean >= (AR_ORDER * 2).min(m / 4) {
            let mut seg: Vec<f64> = channel[st..en].to_vec();
            let mut pinned = vec![false; local.len()];

            for _round in 0..OUTER_ROUNDS {
                let b = match ar_model(&seg, AR_ORDER) {
                    Some(b) => b,
                    None => break,
                };
                let p = b.len() - 1;
                let rr = coef_autocorr(&b);
                pinned.iter_mut().for_each(|x| *x = false);

                for _pass in 0..ACTIVE_ROUNDS {
                    let mut act = Vec::with_capacity(local.len());
                    let mut act_of = Vec::with_capacity(local.len());
                    for (t, &pos) in local.iter().enumerate() {
                        if !pinned[t] {
                            act.push(pos);
                            act_of.push(t);
                        }
                    }
                    if act.is_empty() {
                        break;
                    }

                    let mut violated = false;
                    let mut blk = 0usize;
                    while blk < act.len() {
                        let bend = (blk + MAX_SOLVE).min(act.len());
                        let cols = &act[blk..bend];
                        let k = cols.len();

                        // Only this block is unknown right now; every other
                        // missing sample enters as the value it currently holds.
                        let mut is_unknown = vec![false; m];
                        for &x in cols {
                            is_unknown[x] = true;
                        }

                        let mut gram = vec![0.0f64; k * k];
                        for i in 0..k {
                            for j in 0..k {
                                let d = cols[i].abs_diff(cols[j]);
                                gram[i * k + j] = if d <= p { rr[d] } else { 0.0 };
                            }
                            gram[i * k + i] += 1e-9 * rr[0];
                        }

                        // R is zero past lag p, so only a 2p window around each
                        // unknown contributes to its right-hand side.
                        let mut rhs = vec![0.0f64; k];
                        for i in 0..k {
                            let c = cols[i];
                            let jlo = c.saturating_sub(p);
                            let jhi = (c + p + 1).min(m);
                            let mut acc = 0.0;
                            for j in jlo..jhi {
                                if !is_unknown[j] {
                                    acc += rr[c.abs_diff(j)] * seg[j];
                                }
                            }
                            rhs[i] = -acc;
                        }

                        let sol = match solve_sym(&mut gram, &mut rhs, k) {
                            Some(v) => v,
                            None => break,
                        };

                        for i in 0..k {
                            let pos = cols[i];
                            let g = pos + st;
                            let mut v = sol[i];
                            // Below the rail is not a candidate: the truth was
                            // above it.
                            if hi_mask[g] && v < theta_hi {
                                v = theta_hi;
                                pinned[act_of[blk + i]] = true;
                                violated = true;
                            } else if lo_mask[g] && v > -theta_lo {
                                v = -theta_lo;
                                pinned[act_of[blk + i]] = true;
                                violated = true;
                            }
                            seg[pos] = v.clamp(-ceiling_lo, ceiling_hi);
                        }
                        blk = bend;
                    }
                    if !violated {
                        break;
                    }
                }
            }

            // Crossfade the seam so the two windows covering an unknown agree,
            // and write only inside the runs — everything else comes back
            // bit-identical, which is what makes a null test meaningful.
            let f = (HOP / 2).min(m / 4);
            for &pos in &local {
                let g = pos + st;
                let mut w = 1.0f64;
                if f > 0 && st > 0 && pos < f {
                    w = 0.5
                        * (1.0
                            - (std::f64::consts::PI * (pos as f64 + 0.5) / f as f64).cos());
                }
                if f > 0 && en < n && pos + f >= m {
                    let t = pos + f - m;
                    w *= 0.5
                        * (1.0 + (std::f64::consts::PI * (t as f64 + 0.5) / f as f64).cos());
                }
                let prev = channel[g];
                let mut v = w * seg[pos] + (1.0 - w) * prev;
                v = if hi_mask[g] { v.max(theta_hi) } else { v.min(-theta_lo) };
                channel[g] = v.clamp(-ceiling_lo, ceiling_hi);
                solved[g] = true;
            }
        }

        if en >= n {
            break;
        }
        st += HOP;
    }

    for &(a, b) in &runs {
        if (a..b).all(|i| solved[i]) {
            if b - a <= 16 {
                out.short_fixed += 1;
            } else {
                out.plateaus_fixed += 1;
            }
            out.repaired.push((a, b));
        } else {
            out.unrecoverable += 1;
            out.untouched.push((a, b));
        }
    }
    out
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn run(
    l: &mut Vec<f64>,
    r: &mut Vec<f64>,
    stats: &SourceStats,
    sample_rate: u32,
    cancel: &AtomicBool,
) -> Result<DeclipReport, DeclipSkip> {
    if cancel.load(Ordering::Relaxed) {
        return Err(DeclipSkip::Cancelled);
    }
    let n = l.len();
    if n == 0 {
        return Err(DeclipSkip::Cancelled);
    }

    // Threshold: histogram spike first, then flat-top fallback.
    let theta = match detect_threshold_from_histogram(&stats.hist) {
        Some(t) => t,
        None => {
            let ft = flat_top_runs_in_channel(l, stats.peak_lin)
                + flat_top_runs_in_channel(r, stats.peak_lin);
            if ft >= 20 {
                stats.peak_lin
            } else {
                crate::aelog!("[DECLIP] no clipping signature — skipping");
                return Err(DeclipSkip::NoSignature);
            }
        }
    };

    let theta_dbfs = 20.0 * theta.log10();
    // Rails per channel and per polarity: the histogram gives one number for
    // both, and DC removal upstream has already moved them apart.
    let rails_l = rails_from_channel(l, theta);
    let rails_r = rails_from_channel(r, theta);
    let count_clipped = |ch: &[f64], (hi, lo): (f64, f64)| -> usize {
        let (g_hi, g_lo) = (hi * (1.0 - 1e-4), lo * (1.0 - 1e-4));
        ch.iter().filter(|&&x| x >= g_hi || x <= -g_lo).count()
    };
    let clipped_l = count_clipped(l, rails_l);
    let clipped_r = count_clipped(r, rails_r);
    let total = 2 * n;
    let clip_fraction = (clipped_l + clipped_r) as f64 / total as f64;
    let clipped_pct = clip_fraction * 100.0;

    if clip_fraction < 1e-5 {
        crate::aelog!(
            "[DECLIP] too few clipped samples (fraction={:.2e}) — skipping",
            clip_fraction
        );
        return Err(DeclipSkip::TooFew);
    }

    // A limiter dents the rail, it does not flatten it: peaks touch for a
    // sample or two and come straight back. Nothing to rebuild there — the
    // solver would only invent arcs, and did, until this gate existed.
    let longest_run = longest_rail_run(l, rails_l).max(longest_rail_run(r, rails_r));
    if longest_run < MIN_RUN_GATE {
        crate::aelog!(
            "[DECLIP] longest run on the rail is {} samples (gate {}) — a limiter ceiling, not clipping; standing down",
            longest_run, MIN_RUN_GATE
        );
        return Err(DeclipSkip::NoPlateaus { longest: longest_run });
    }

    // > 5%: process but will be flagged as warn in the report level.
    if clip_fraction > 0.05 {
        crate::aelog!(
            "[DECLIP] high clip fraction ({:.1}%) — processing with warning",
            clipped_pct
        );
    }

    crate::aelog!(
        "[DECLIP] threshold={:.4} ({:.2}dBFS) clip={:.2}%",
        theta, theta_dbfs, clipped_pct
    );
    crate::aelog!(
        "[DECLIP] rails as the signal carries them: L +{:.6}/-{:.6}  R +{:.6}/-{:.6}",
        rails_l.0, rails_l.1, rails_r.0, rails_r.1
    );

    if cancel.load(Ordering::Relaxed) {
        return Err(DeclipSkip::Cancelled);
    }

    let t0 = std::time::Instant::now();

    // The two channels share nothing the repair touches: separate buffers,
    // separate rails, and each pass reads and writes only its own. So they
    // run together, which on a two-core machine halves what is, on clipped
    // material, the most expensive pass on the source side.
    //
    // The sequencing that matters is *inside* a channel and is untouched:
    // windows still advance one at a time, because each one crossfades into
    // what the previous one wrote. Sample for sample this is the file the
    // serial version wrote.
    let (res_l, res_r) = rayon::join(
        || {
            let mut planner = FftPlanner::<f64>::new();
            repair_channel(l, theta, rails_l, sample_rate, &mut planner, cancel)
        },
        || {
            let mut planner = FftPlanner::<f64>::new();
            repair_channel(r, theta, rails_r, sample_rate, &mut planner, cancel)
        },
    );
    if cancel.load(Ordering::Relaxed) {
        return Err(DeclipSkip::Cancelled);
    }

    let elapsed_ms = t0.elapsed().as_millis();
    crate::aelog!(
        "[DECLIP] done in {}ms: short={} plateaus={} unr={} tskip={}",
        elapsed_ms,
        res_l.short_fixed + res_r.short_fixed,
        res_l.plateaus_fixed + res_r.plateaus_fixed,
        res_l.unrecoverable + res_r.unrecoverable,
        res_l.transient_skipped + res_r.transient_skipped
    );

    let longest_ms =
        (res_l.longest_samples.max(res_r.longest_samples)) as f64 * 1000.0 / sample_rate as f64;

    Ok(DeclipReport {
        threshold_dbfs: theta_dbfs,
        regions_total: res_l.regions_total + res_r.regions_total,
        short_fixed: res_l.short_fixed + res_r.short_fixed,
        plateaus_fixed: res_l.plateaus_fixed + res_r.plateaus_fixed,
        longest_ms,
        unrecoverable: res_l.unrecoverable + res_r.unrecoverable,
        clipped_pct,
        transient_skipped: res_l.transient_skipped + res_r.transient_skipped,
        repaired_l: res_l.repaired,
        repaired_r: res_r.repaired,
        untouched_l: res_l.untouched,
        untouched_r: res_r.untouched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn make_sine(n: usize, sample_rate: u32, freq: f64, amplitude: f64) -> Vec<f64> {
        (0..n)
            .map(|i| {
                amplitude
                    * (2.0 * std::f64::consts::PI * freq * i as f64 / sample_rate as f64).sin()
            })
            .collect()
    }

    fn stats_with_spike(theta: f64, n_bins: usize, peak: f64) -> SourceStats {
        let mut hist = vec![0u64; n_bins];
        let theta_bin = ((theta * n_bins as f64) as usize).min(n_bins - 1);
        for b in theta_bin.saturating_sub(32)..theta_bin {
            hist[b] = 1;
        }
        hist[theta_bin] = 100;
        SourceStats {
            peak_lin: peak,
            peak_dbfs: 20.0 * peak.log10(),
            hist,
            grid16: false,
            enob: None,
        }
    }

    fn stats_no_spike(peak: f64) -> SourceStats {
        SourceStats {
            peak_lin: peak,
            peak_dbfs: 20.0 * peak.log10(),
            hist: vec![0u64; 65536],
            grid16: false,
            enob: None,
        }
    }

    fn snr_db(signal: &[f64], error: &[f64]) -> f64 {
        let sig_pow: f64 = signal.iter().map(|x| x * x).sum();
        let err_pow: f64 = error.iter().map(|x| x * x).sum();
        if err_pow == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (sig_pow / err_pow).log10()
    }

    // ── Test a: mixed-tone SPAIN quality ─────────────────────────────────────
    //
    // Three sines at exact DFT bins for the 1024-sample SPAIN window, amplitudes
    // chosen so the sum peak (~0.88) slightly exceeds theta=0.80 only when all
    // three align constructively.  The result: ~17% of the signal is rail-band
    // (contaminated context) and ~83% is cleanly below theta_low, giving SPAIN
    // a rich anchor to recover the correct spectrum and push plateau samples back
    // to their true values above theta.
    //
    // 1024-sample SPAIN window is used for regions in the 17–384 sample range:
    //   next_pow2(max(1024, region_len*4)) = 1024.
    // Bin k → freq = k * sr / 1024.

    /// A signal clipped far below its true peak asks the reconstruction for
    /// more headroom than the ceiling allows. It must come back *bounded* and
    /// *not flat*: chopping every sample at the ceiling used to hand back a
    /// fresh plateau one ceiling higher, which sounds exactly like the
    /// clipping being repaired.
    #[test]
    fn a_reconstruction_over_the_ceiling_is_scaled_not_chopped() {
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.25f64; // clip hard: the true peak is 4x the rail

        let f1 = 2.0 * sr as f64 / 1024.0;
        let pi2 = 2.0 * std::f64::consts::PI;
        let reference: Vec<f64> = (0..n)
            .map(|i| (pi2 * f1 * i as f64 / sr as f64).sin())
            .collect();
        let clipped: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();

        let mut l = clipped.clone();
        let mut r = clipped.clone();
        let cancel = AtomicBool::new(false);
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();
        let report = run(&mut l, &mut r, &stats, sr, &cancel).expect("run must succeed");
        assert!(report.plateaus_fixed > 0, "the fixture must exercise the plateau path");

        let ceiling = theta * CEILING_OVER_RAIL;

        // Bounded: the guard still holds.
        let peak = l.iter().fold(0.0f64, |m, &x| m.max(x.abs()));
        assert!(
            peak <= ceiling * 1.001,
            "reconstruction must stay under the ceiling: peak {:.4} vs ceiling {:.4}",
            peak,
            ceiling
        );

        // Not flat: no long run of identical samples sitting at the ceiling.
        // A chopped arc produces exactly that; a scaled one cannot.
        let mut worst_run = 0usize;
        let mut run_len = 1usize;
        for i in 1..l.len() {
            let near_ceiling = l[i].abs() >= ceiling * 0.999;
            if near_ceiling && (l[i] - l[i - 1]).abs() <= 1e-12 {
                run_len += 1;
                worst_run = worst_run.max(run_len);
            } else {
                run_len = 1;
            }
        }
        assert!(
            worst_run < 3,
            "the repair left a {}-sample plateau at the ceiling - it was chopped, not scaled",
            worst_run
        );
    }

    /// Flatness, counted the way the inspector counts it: runs of identical
    /// samples at or above the rail. That is clipping as a *shape*, and it is
    /// what you hear. A repair that leaves the plateau flat has not repaired
    /// anything, however good its SNR looks.
    fn flat_samples(x: &[f64], theta: f64) -> usize {
        let gate = theta * (1.0 - 1e-4);
        let mut flat = 0usize;
        let mut run = 1usize;
        for i in 1..x.len() {
            if x[i].abs() >= gate && (x[i] - x[i - 1]).abs() <= 1e-9 {
                run += 1;
            } else {
                if run >= 3 {
                    flat += run;
                }
                run = 1;
            }
        }
        if run >= 3 {
            flat += run;
        }
        flat
    }

    /// The fit only sees unclipped samples, so nothing in it requires the
    /// answer inside a clipped run to be as loud as the rail. Without the
    /// consistency rounds the evaluation pins those samples to the rail and
    /// hands back the plateau it was asked to remove. This is the test that
    /// the plateaus actually go away.
    /// A kick-drum-shaped fixture: a low tone under a fast decay envelope, with
    /// broadband noise on top, clipped hard. Nothing here is sparse in a
    /// 1024-bin Fourier basis, which is exactly the case where the fit
    /// undershoots inside the run and the plateau used to survive.
    fn kick_like(n: usize, sr: u32) -> Vec<f64> {
        let pi2 = 2.0 * std::f64::consts::PI;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        };
        (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                let beat = t % 0.5;
                let env = (-beat * 9.0).exp();
                let sweep = 55.0 + 25.0 * (-beat * 30.0).exp();
                1.45 * env * (pi2 * sweep * beat).sin()
                    + 0.35 * env * (pi2 * sweep * 2.0 * beat).sin()
                    + 0.05 * rng() * env
            })
            .collect()
    }

    #[test]
    fn plateaus_do_not_survive_the_repair() {
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.8f64;

        let reference = kick_like(n, sr);
        let clipped: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();

        let before = flat_samples(&clipped, theta);
        assert!(before > 1000, "the fixture must be badly plateaued; got {}", before);

        let mut l = clipped.clone();
        let mut r = clipped.clone();
        let cancel = AtomicBool::new(false);
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();
        let rep = run(&mut l, &mut r, &stats, sr, &cancel).expect("run must succeed");

        let after = flat_samples(&l, theta);
        // There is no transient guard any more. It existed because the old
        // repair smeared attacks and had to be kept away from them; the AR
        // solver does not, and drum material is precisely where it was measured
        // (SNR at the clipped samples 12.5 -> 22.1 dB against the float decode
        // of the same master) and where the blind listening test was run. So
        // the assertion is now the thing that actually matters: on a fixture
        // built out of hard attacks, the plateaus go away.
        assert_eq!(
            rep.transient_skipped, 0,
            "the transient guard is gone; nothing should be skipped for being an attack"
        );
        assert!(
            after * 10 < before,
            "flatness must collapse across the whole fixture: {} -> {} ({:.1} % left)",
            before,
            after,
            after as f64 / before as f64 * 100.0
        );
    }

    /// The failure that sent us here: a repair that draws a peak taller than
    /// the one that was clipped away. Undershooting is safe — the result is
    /// still closer to the truth than the flat top was — but overshooting
    /// invents an excursion the recording never had, and on a kick drum that
    /// is audible as a grunt.
    ///
    /// Measured on a real master against the float decode of the same file,
    /// the engine this replaced put 16.2 % of runs more than 1 dB over the
    /// truth and the worst at +4.4 dB. This one was 0.4 % and never past 1 dB.
    #[test]
    fn reconstruction_does_not_invent_peaks() {
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.8f64;

        let reference = kick_like(n, sr);
        let clipped: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();

        let mut l = clipped.clone();
        let mut r = clipped.clone();
        let cancel = AtomicBool::new(false);
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();
        run(&mut l, &mut r, &stats, sr, &cancel).expect("run must succeed");

        // Walk the flat runs of the input and compare each rebuilt peak with
        // the one the reference actually had there.
        let gate = theta * (1.0 - 1e-4);
        let mut runs = 0usize;
        let mut over_1db = 0usize;
        let mut worst = 0.0f64;
        let mut i = 0usize;
        while i < n {
            if clipped[i].abs() >= gate {
                let start = i;
                while i < n && clipped[i].abs() >= gate {
                    i += 1;
                }
                let truth = (start..i).fold(0.0f64, |m, k| m.max(reference[k].abs()));
                let built = (start..i).fold(0.0f64, |m, k| m.max(l[k].abs()));
                if truth > 0.0 {
                    let err_db = 20.0 * (built / truth).log10();
                    runs += 1;
                    if err_db > 1.0 {
                        over_1db += 1;
                    }
                    if err_db > worst {
                        worst = err_db;
                    }
                }
            } else {
                i += 1;
            }
        }

        assert!(runs > 50, "the fixture must produce plenty of runs; got {}", runs);
        assert!(
            worst <= 3.0,
            "no run may be drawn more than 3 dB above the peak that was there; worst was +{:.2} dB",
            worst
        );
        assert!(
            over_1db * 10 <= runs,
            "at most a tenth of runs may overshoot by more than 1 dB; {} of {} did",
            over_1db,
            runs
        );
    }

    // ── Bench harness ────────────────────────────────────────────────────
    // Not part of the suite: `cargo test -- --ignored declip_bench --nocapture`.
    // Reads the 16-bit fragment the inspector exported, runs the engine that
    // actually ships, and writes 32-bit float so the reconstruction survives
    // the trip. Lets the Rust be measured against the Python reference on
    // byte-identical input.
    fn read_wav16(path: &str) -> (Vec<f64>, Vec<f64>, u32) {
        let raw = std::fs::read(path).expect("fixture wav must exist");
        // walk the RIFF chunks rather than assuming a 44-byte header
        let mut pos = 12usize;
        let mut sr = 44100u32;
        let mut ch = 2usize;
        let (mut lo, mut hi) = (0usize, 0usize);
        while pos + 8 <= raw.len() {
            let id = &raw[pos..pos + 4];
            let sz = u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]])
                as usize;
            let body = pos + 8;
            if id == b"fmt " {
                ch = u16::from_le_bytes([raw[body + 2], raw[body + 3]]) as usize;
                sr = u32::from_le_bytes([
                    raw[body + 4], raw[body + 5], raw[body + 6], raw[body + 7],
                ]);
            } else if id == b"data" {
                lo = body;
                hi = (body + sz).min(raw.len());
            }
            pos = body + sz + (sz & 1);
        }
        let n = (hi - lo) / 2 / ch;
        let mut l = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        for i in 0..n {
            let o = lo + i * 2 * ch;
            let a = i16::from_le_bytes([raw[o], raw[o + 1]]) as f64 / 32768.0;
            let b = if ch >= 2 {
                i16::from_le_bytes([raw[o + 2], raw[o + 3]]) as f64 / 32768.0
            } else {
                a
            };
            l.push(a);
            r.push(b);
        }
        (l, r, sr)
    }

    fn write_wav_f32(path: &str, l: &[f64], r: &[f64], sr: u32) {
        let n = l.len();
        let data = (n * 2 * 4) as u32;
        let mut o: Vec<u8> = Vec::with_capacity(44 + data as usize);
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&(36 + data).to_le_bytes());
        o.extend_from_slice(b"WAVEfmt ");
        o.extend_from_slice(&16u32.to_le_bytes());
        o.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        o.extend_from_slice(&2u16.to_le_bytes());
        o.extend_from_slice(&sr.to_le_bytes());
        o.extend_from_slice(&(sr * 2 * 4).to_le_bytes());
        o.extend_from_slice(&8u16.to_le_bytes());
        o.extend_from_slice(&32u16.to_le_bytes());
        o.extend_from_slice(b"data");
        o.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            o.extend_from_slice(&(l[i] as f32).to_le_bytes());
            o.extend_from_slice(&(r[i] as f32).to_le_bytes());
        }
        std::fs::write(path, o).expect("must be able to write the bench output");
    }

    #[test]
    #[ignore]
    fn declip_bench_on_the_exported_fragment() {
        const IN: &str =
            r"E:\MUSIC\Test\LAB\Justin_Bieber_-_Rich_Girl__33.944s-34.752s__original.wav";
        const OUT: &str = r"E:\MUSIC\Test\LAB\declip\rust_engine_out.wav";
        const EXPORT_GAIN_DB: f64 = -6.0205336449479265;

        let (mut l, mut r, sr) = read_wav16(IN);
        let g = 10f64.powf(-EXPORT_GAIN_DB / 20.0); // undo what the export applied
        for v in l.iter_mut().chain(r.iter_mut()) {
            *v *= g;
        }

        let cancel = AtomicBool::new(false);
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();
        let rep = run(&mut l, &mut r, &stats, sr, &cancel).expect("run must succeed");
        println!(
            "[BENCH] thr={:.2} dBFS regions={} short={} plateaus={} unrecoverable={} longest={:.2} ms",
            rep.threshold_dbfs, rep.regions_total, rep.short_fixed, rep.plateaus_fixed,
            rep.unrecoverable, rep.longest_ms
        );
        let peak = l.iter().chain(r.iter()).fold(0.0f64, |m, &x| m.max(x.abs()));
        println!("[BENCH] peak after repair {:+.3} dBFS", 20.0 * peak.log10());
        write_wav_f32(OUT, &l, &r, sr);
        println!("[BENCH] wrote {}", OUT);
    }

    #[test]
    fn test_a_mixed_tone_snr() {
        let sr = 44100u32;
        let n = sr as usize; // 1 second
        let theta = 0.8f64;

        // Exact DFT bins for the 1024-sample SPAIN window.
        // One dominant component (a1=1.00 > theta=0.80) creates clear plateaus
        // of ~105 samples.  The two minor components are tiny, so the clipping
        // error at the plateau (reference−theta ≈ 0.1–0.2) is much larger than
        // the SPAIN residual (a2+a3 components not recovered, ≈0.035 RMS).
        // This guarantees >> 12 dB SNR improvement even when the K schedule
        // grows past the optimal K=2 for the dominant component.
        let f1 = 2.0 * sr as f64 / 1024.0; //  86.1 Hz, bin 2 in N=1024
        let f2 = 5.0 * sr as f64 / 1024.0; // 215.3 Hz, bin 5 in N=1024
        let f3 = 9.0 * sr as f64 / 1024.0; // 387.6 Hz, bin 9 in N=1024
        let pi2 = 2.0 * std::f64::consts::PI;
        let reference: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                1.00 * (pi2 * f1 * t).sin()
                    + 0.04 * (pi2 * f2 * t).sin()
                    + 0.03 * (pi2 * f3 * t).sin()
            })
            .collect();
        // f1 creates plateaus of ≈105 samples (SPAIN range 17–384).
        // ~50% clean context → SPAIN converges to f1 amplitude accurately.

        let clipped: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();

        let mut l = clipped.clone();
        let mut r = clipped.clone();

        // Authentic stats from the clipped signal.
        let cancel = AtomicBool::new(false);
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();

        let report = run(&mut l, &mut r, &stats, sr, &cancel)
            .expect("run must succeed for clipped signal");

        assert!(
            report.plateaus_fixed > 0,
            "SPAIN must repair plateau regions; got plateaus_fixed={}",
            report.plateaus_fixed
        );

        // SNR measured only on deeply-clipped samples (|reference| > theta * 1.05),
        // where the clipping error is large enough to matter.
        let theta_gate = theta * (1.0 - 1e-4);
        let deep: Vec<usize> = (0..n)
            .filter(|&i| clipped[i].abs() >= theta_gate && reference[i].abs() > theta * 1.05)
            .collect();

        assert!(!deep.is_empty(), "must have deeply-clipped samples to measure");

        let ref_vals: Vec<f64> = deep.iter().map(|&i| reference[i]).collect();
        let before_err: Vec<f64> = deep.iter().map(|&i| clipped[i] - reference[i]).collect();
        let after_err: Vec<f64> = deep.iter().map(|&i| l[i] - reference[i]).collect();

        let snr_before = snr_db(&ref_vals, &before_err);
        let snr_after = snr_db(&ref_vals, &after_err);

        // With ~83% clean context per SPAIN window and exact-bin frequencies,
        // SPAIN converges to the true 3-sine spectrum and pushes plateau samples
        // toward their reference values.  Require >= 12 dB improvement.
        assert!(
            snr_after - snr_before >= 12.0,
            "SNR improvement on deeply-clipped samples must be >= 12 dB; \
             before={:.1} after={:.1} improvement={:.1}",
            snr_before, snr_after, snr_after - snr_before
        );

        // At least some repaired samples must exceed theta.
        let any_above = (0..n).any(|i| clipped[i].abs() >= theta_gate && l[i].abs() > theta * 1.001);
        assert!(any_above, "reconstructed peaks must exceed theta in repaired regions");
    }

    // ── Test b: wavy plateau → single merged region ───────────────────────────

    #[test]
    fn test_b_wavy_plateau_single_region() {
        let sr = 44100u32;
        let theta = 0.9f64;
        let theta_low = theta * 10f64.powf(-1.0 / 20.0);
        let n = 2048usize;

        // Build a plateau of 200 samples (all at theta), with ±0.02 ripple
        // and four 3-sample dips to -2 dB below rail.
        let plateau_start = 512usize;
        let plateau_len = 200usize;
        let plateau_end = plateau_start + plateau_len;

        let mut channel = vec![0.0f64; n];

        // Fill plateau with ripple.
        for i in plateau_start..plateau_end {
            let t = (i - plateau_start) as f64;
            // Ripple: ±0.02 around theta
            channel[i] = theta + 0.02 * (2.0 * std::f64::consts::PI * t / 40.0).sin();
        }

        // Insert four 3-sample dips at -2 dB below rail: |x| = theta * 10^(-2/20).
        let dip_level = theta * 10f64.powf(-2.0 / 20.0);
        for &dip_pos in &[560usize, 600, 640, 680] {
            for j in 0..3 {
                channel[dip_pos + j] = dip_level;
            }
        }

        // Verify that dip_level > gap_min.
        let gap_min = theta * 10f64.powf(-3.0 / 20.0);
        assert!(
            dip_level > gap_min,
            "dip_level={:.4} must exceed gap_min={:.4} for bridging to work",
            dip_level, gap_min
        );

        // Verify that ripple min (theta - 0.02) is still in rail band.
        assert!(
            theta - 0.02 >= theta_low,
            "ripple bottom must stay in rail band: {:.4} >= {:.4}",
            theta - 0.02, theta_low
        );

        let regions = detect_regions(&channel, theta);
        // For a single plateau (positive polarity), we expect exactly 1 region.
        assert!(
            regions.len() <= 2,
            "wavy plateau must detect as <= 2 regions (got {}), not fragmented",
            regions.len()
        );
        assert!(
            !regions.is_empty(),
            "plateau must produce at least one region"
        );

        // The region must cover the whole plateau.
        let r = &regions[0];
        assert!(
            r.start <= plateau_start && r.end >= plateau_end,
            "region must span the full plateau: [{},{}) vs plateau [{},{})",
            r.start, r.end, plateau_start, plateau_end
        );

        // Simulate the full run() call to check regions_total.
        let mut l = channel.clone();
        let mut rc = channel.clone();
        let stats = stats_with_spike(theta, 65536, theta + 0.02);
        let cancel = AtomicBool::new(false);
        let report = run(&mut l, &mut rc, &stats, sr, &cancel).unwrap();

        assert!(
            report.regions_total <= 2,
            "regions_total must be <= 2 for a single plateau; got {}",
            report.regions_total
        );
    }

    // ── Test c: bit-identity of all samples outside regions ───────────────────

    #[test]
    fn test_c_bit_identity() {
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.9f64;

        // Build a signal where some regions will be detected.
        let mut l: Vec<f64> = (0..n)
            .map(|i| 0.5 * (2.0 * std::f64::consts::PI * 200.0 * i as f64 / sr as f64).sin())
            .collect();
        let mut r = l.clone();

        // Inject plateau regions.
        let plateau_val = theta + 0.001;
        for start in [4000usize, 20000, 40000].iter() {
            for j in 0..100 {
                l[start + j] = plateau_val;
                r[start + j] = plateau_val;
            }
        }

        let l_before = l.clone();
        let r_before = r.clone();

        let stats = stats_with_spike(theta, 65536, plateau_val);
        let cancel = AtomicBool::new(false);
        let result = run(&mut l, &mut r, &stats, sr, &cancel);

        if result.is_err() {
            return;
        }

        // After repair, detect which indices belong to regions.
        let threshold_gate = theta * (1.0 - 1e-4);
        for i in 0..n {
            // A sample that was not at the clip level before must be unchanged.
            if l_before[i].abs() < threshold_gate {
                assert_eq!(
                    l[i], l_before[i],
                    "L sample {} outside all regions changed: before={} after={}",
                    i, l_before[i], l[i]
                );
            }
            if r_before[i].abs() < threshold_gate {
                assert_eq!(
                    r[i], r_before[i],
                    "R sample {} outside all regions changed: before={} after={}",
                    i, r_before[i], r[i]
                );
            }
        }
    }

    // ── The two channels in parallel are the two channels in sequence ─────────

    #[test]
    fn the_channels_are_repaired_independently() {
        // `run` repairs L and R at the same time. That is only allowed
        // because the repair of a channel reads and writes nothing but that
        // channel — so each one has to come out of the pair exactly as it
        // comes out on its own. This locks that down: give the two channels
        // *different* damage, so a pass that leaked state between them could
        // not accidentally agree.
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.9f64;
        let plateau_val = theta + 0.001;

        let tone = |freq: f64, amp: f64| -> Vec<f64> {
            (0..n)
                .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / sr as f64).sin())
                .collect()
        };

        let mut l = tone(200.0, 0.5);
        let mut r = tone(313.0, 0.45);
        // Different counts, different lengths, different places.
        for &start in [4000usize, 20000, 40000].iter() {
            for j in 0..100 {
                l[start + j] = plateau_val;
            }
        }
        for &start in [7000usize, 31000].iter() {
            for j in 0..60 {
                r[start + j] = -plateau_val;
            }
        }

        let mut l_alone = l.clone();
        let mut r_alone = r.clone();

        let stats = stats_with_spike(theta, 65536, plateau_val);
        let cancel = AtomicBool::new(false);
        let report = run(&mut l, &mut r, &stats, sr, &cancel)
            .expect("this material is clipped hard enough to be repaired");
        assert!(
            report.short_fixed + report.plateaus_fixed > 0,
            "the test proves nothing if nothing was repaired"
        );

        // The same rails `run` derives, then one channel at a time.
        let rails_l = rails_from_channel(&l_alone, theta);
        let rails_r = rails_from_channel(&r_alone, theta);
        let mut planner = FftPlanner::<f64>::new();
        repair_channel(&mut l_alone, theta, rails_l, sr, &mut planner, &cancel);
        repair_channel(&mut r_alone, theta, rails_r, sr, &mut planner, &cancel);

        assert_eq!(l, l_alone, "L differs from the same channel repaired alone");
        assert_eq!(r, r_alone, "R differs from the same channel repaired alone");
    }

    // ── Test d: genuine near-rail material — no modification ──────────────────

    #[test]
    fn test_d_genuine_near_rail() {
        let sr = 44100u32;
        let n = sr as usize * 4;

        // Pure sine at 0.99 amplitude — touches rail only at peaks, no flat tops.
        let original: Vec<f64> = (0..n)
            .map(|i| 0.99 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sr as f64).sin())
            .collect();
        let mut l = original.clone();
        let mut r = original.clone();

        let cancel = AtomicBool::new(false);
        // Use authentic stats from the sine itself.
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();

        let result = run(&mut l, &mut r, &stats, sr, &cancel);

        match result {
            Err(_) => {
                // Skipped entirely — no modification. Verify bit-identity.
                assert_eq!(l, original, "L must be bit-identical after skip");
                assert_eq!(r, original, "R must be bit-identical after skip");
            }
            Ok(report) => {
                // If the gate did not skip (e.g. some regions were found),
                // total modification energy must be below -80 dBFS.
                let diff_energy: f64 = l
                    .iter()
                    .zip(original.iter())
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f64>()
                    + r.iter()
                        .zip(original.iter())
                        .map(|(a, b)| (a - b).powi(2))
                        .sum::<f64>();
                let total_energy: f64 = original.iter().map(|x| x * x).sum::<f64>() * 2.0;
                let mod_dbfs = if diff_energy == 0.0 {
                    f64::NEG_INFINITY
                } else {
                    10.0 * (diff_energy / total_energy).log10()
                };
                assert!(
                    mod_dbfs < -80.0,
                    "modification energy must be < -80 dBFS for near-rail sine; got {:.1} dBFS (short={} plateaus={})",
                    mod_dbfs, report.short_fixed, report.plateaus_fixed
                );
            }
        }
    }

    // ── Test e: 1000-sample plateau is unrecoverable ─────────────────────────

    #[test]
    fn test_e_unrecoverable_plateau() {
        let sr = 44100u32;
        let n = 8192usize;
        let theta = 0.9f64;
        let plateau_val = theta + 0.001;

        let mut l: Vec<f64> = (0..n)
            .map(|i| 0.4 * (2.0 * std::f64::consts::PI * 200.0 * i as f64 / sr as f64).sin())
            .collect();
        let mut r = l.clone();

        // Inject a 1000-sample plateau.
        let plateau_start = 1000usize;
        for j in 0..1000 {
            l[plateau_start + j] = plateau_val;
            r[plateau_start + j] = plateau_val;
        }

        let plateau_l_before: Vec<f64> = l[plateau_start..plateau_start + 1000].to_vec();

        let stats = stats_with_spike(theta, 65536, plateau_val);
        let cancel = AtomicBool::new(false);
        let report = run(&mut l, &mut r, &stats, sr, &cancel).unwrap();

        // Must be counted as unrecoverable.
        assert!(
            report.unrecoverable >= 2,
            "1000-sample plateau must be unrecoverable (count >= 2 for L+R); got {}",
            report.unrecoverable
        );

        // Must be bit-identical.
        assert_eq!(
            &l[plateau_start..plateau_start + 1000],
            &plateau_l_before[..],
            "unrecoverable plateau samples in L must be untouched"
        );
    }

    // ── Test f: adapted v1 tests ──────────────────────────────────────────────

    #[test]
    fn test_f_clean_signal_untouched() {
        let sr = 44100u32;
        let n = sr as usize;
        let original: Vec<f64> = (0..n)
            .map(|i| 0.1 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sr as f64).sin())
            .collect();
        let mut l = original.clone();
        let mut r = original.clone();

        let stats = stats_no_spike(0.1);
        let cancel = AtomicBool::new(false);
        let result = run(&mut l, &mut r, &stats, sr, &cancel);

        assert!(result.is_err(), "clean signal with no signature must be skipped");
        assert_eq!(l, original, "L channel must be bit-identical");
        assert_eq!(r, original, "R channel must be bit-identical");
    }

    #[test]
    fn test_f_short_run_ar_path() {
        // Verify the AR path still works for short runs (1..=16 samples).
        let sr = 44100u32;
        let theta = 0.5f64;
        let freq = 1000.0;
        let n = 4096usize;

        let reference = make_sine(n, sr, freq, 1.0);

        // Find a positive run above theta near the middle.
        let mut run_start = 0usize;
        for i in 2000..2100 {
            if reference[i] >= theta && (i == 0 || reference[i - 1] < theta) {
                run_start = i;
                break;
            }
        }
        assert!(run_start > 0, "must find a rising-edge above theta");

        let mut run_end = run_start;
        while run_end < n && reference[run_end] >= theta {
            run_end += 1;
        }
        let run_len = run_end - run_start;
        assert!(
            run_len >= 1 && run_len <= 16,
            "run_len={} must be <= 16 for AR path test",
            run_len
        );

        let mut buf = reference.clone();
        for i in run_start..run_end {
            buf[i] = theta; // flat-top clip
        }

        // Call repair_channel directly.
        let cancel = AtomicBool::new(false);
        let mut planner = FftPlanner::<f64>::new();
        let res = repair_channel(&mut buf, theta, (theta, theta), sr, &mut planner, &cancel);

        assert!(res.short_fixed >= 1, "AR must fix the short run; got {}", res.short_fixed);

        let sig_pow: f64 = (run_start..run_end).map(|i| reference[i].powi(2)).sum();
        let err_before: f64 = (run_start..run_end).map(|i| (reference[i] - theta).powi(2)).sum();
        let err_after: f64 = (run_start..run_end).map(|i| (reference[i] - buf[i]).powi(2)).sum();

        let snr_before = if err_before > 0.0 { 10.0 * (sig_pow / err_before).log10() } else { f64::INFINITY };
        let snr_after = if err_after > 0.0 { 10.0 * (sig_pow / err_after).log10() } else { f64::INFINITY };

        assert!(
            snr_after - snr_before >= 19.0,
            "AR SNR improvement >= 19 dB; before={:.1} after={:.1}",
            snr_before, snr_after
        );
    }

    /// The rail the histogram reports and the rail the repair is handed stop
    /// being the same number the moment the pipeline takes a DC offset out
    /// between them — `prepare.rs` does exactly that, because the 16-bit comb
    /// test in `stats` needs the raw signal. One symmetric gate then covers
    /// one rail and misses the other.
    ///
    /// Field evidence: on `Justin Bieber - Rich Girl.mp3` the removed offset
    /// was -1.3e-4, the gate is 1e-4 wide, and the finished conversion came
    /// out with 225 of 229 negative regions in the drum fragment still
    /// clipped against 167 of 223 positive ones repaired. It read as "the
    /// chain after the declipper undoes its work"; it was the declipper never
    /// doing half of it.
    #[test]
    fn a_dc_offset_between_stats_and_repair_does_not_hide_a_rail() {
        let sr = 44100u32;
        let n = sr as usize / 4;
        let theta = 0.9f64;
        let reference = make_sine(n, sr, 220.0, 1.0);
        let clipped: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();

        // Stats first, on the raw signal — as prepare.rs orders it.
        let cancel = AtomicBool::new(false);
        let mut l = clipped.clone();
        let mut r = clipped.clone();
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();

        // ...then the offset comes out. 3e-4 is twice what the field file
        // carried and three times the width of the gate under the rail.
        const DC: f64 = 3e-4;
        for v in l.iter_mut().chain(r.iter_mut()) {
            *v -= DC;
        }
        let shifted = l.clone();

        // The rails the repair is actually handed.
        let (hi, lo) = rails_from_channel(&shifted, theta);
        let bin = (theta * (10f64.powf(1.0 / 20.0) - 10f64.powf(-1.0 / 20.0))) / 4096.0;
        assert!(
            (hi - (theta - DC)).abs() <= bin && (lo - (theta + DC)).abs() <= bin,
            "rails must be read off the data: got +{:.6}/-{:.6}, expected +{:.6}/-{:.6}",
            hi, lo, theta - DC, theta + DC
        );

        // The same signal through the old symmetric gate, to keep the bug
        // itself under test: one rail is missed outright.
        {
            let mut old = shifted.clone();
            let mut planner = FftPlanner::<f64>::new();
            repair_channel(&mut old, theta, (theta, theta), sr, &mut planner, &cancel);
            let touched = (0..n)
                .filter(|&i| shifted[i] > 0.0 && (old[i] - shifted[i]).abs() > 1e-9)
                .count();
            assert_eq!(
                touched, 0,
                "one gate for both rails is expected to miss the shifted one                  entirely - if this ever stops holding, the guard below has                  stopped guarding anything"
            );
        }

        let report = run(&mut l, &mut r, &stats, sr, &cancel).expect("run must succeed");
        assert!(report.plateaus_fixed > 0, "must repair plateaus at all");

        let mut touched_hi = 0usize;
        let mut touched_lo = 0usize;
        for i in 0..n {
            if (l[i] - shifted[i]).abs() > 1e-9 {
                if shifted[i] > 0.0 {
                    touched_hi += 1;
                } else {
                    touched_lo += 1;
                }
            }
        }
        assert!(touched_hi > 0, "positive rail must be repaired");
        assert!(touched_lo > 0, "negative rail must be repaired");
        // A sine clips symmetrically; neither rail may come out with a
        // fraction of the other's work.
        let (small, big) = if touched_hi < touched_lo {
            (touched_hi, touched_lo)
        } else {
            (touched_lo, touched_hi)
        };
        assert!(
            small as f64 >= 0.5 * big as f64,
            "both rails must get comparable work; hi={} lo={}",
            touched_hi, touched_lo
        );

        let peak_hi = l.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let peak_lo = -l.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(peak_hi > hi * 1.001, "positive peaks must be rebuilt above the rail");
        assert!(peak_lo > lo * 1.001, "negative peaks must be rebuilt above the rail");
    }

    /// A rail that was never clipped must not be invented: on a one-sided
    /// clip the clean polarity keeps the nominal threshold, so the gate never
    /// opens on untouched signal.
    #[test]
    fn an_unclipped_rail_keeps_the_nominal_threshold() {
        let sr = 44100u32;
        let n = sr as usize / 4;
        let theta = 0.9f64;
        // Positive half clipped at theta, negative half left alone.
        let channel: Vec<f64> = make_sine(n, sr, 220.0, 1.0)
            .into_iter()
            .map(|x| if x > theta { theta } else { x })
            .collect();

        let (hi, lo) = rails_from_channel(&channel, theta);
        let bin = (theta * (10f64.powf(1.0 / 20.0) - 10f64.powf(-1.0 / 20.0))) / 4096.0;
        assert!((hi - theta).abs() <= bin, "clipped rail must be found: {:.6}", hi);
        assert_eq!(lo, theta, "clean rail must fall back to the histogram value");
    }

    /// A brickwall limiter parks every peak on its ceiling for a sample or
    /// two and lets go. That is not clipping, and the stage must not touch
    /// it: on Corazón it drew 4,359 arcs over exactly this and cost the track
    /// 0.83 dB of level for nothing.
    #[test]
    fn a_limiter_ceiling_is_not_clipping() {
        let sr = 44100u32;
        let n = sr as usize;
        let theta = 0.9f64;
        // Peaks that only just reach the rail: a 220 Hz sine a hair above
        // theta, clamped, sits on the rail for about four samples a half-cycle
        // — a few hundred touches a second, which is what a limiter leaves
        // behind, and nothing a clipper would.
        let reference = make_sine(n, sr, 220.0, theta * 1.002);
        let limited: Vec<f64> = reference.iter().map(|&x| x.clamp(-theta, theta)).collect();
        let cancel = AtomicBool::new(false);
        let mut l = limited.clone();
        let mut r = limited.clone();
        let stats = super::super::stats::analyze(&l, &r, sr, &cancel).unwrap();
        let longest = longest_rail_run(&l, (theta, theta));
        assert!(
            longest >= 1 && longest < MIN_RUN_GATE,
            "fixture must touch the rail in runs shorter than the gate; longest={}",
            longest
        );
        match run(&mut l, &mut r, &stats, sr, &cancel) {
            Err(DeclipSkip::NoPlateaus { longest: got }) => assert_eq!(got, longest),
            other => panic!("expected NoPlateaus, got {:?}", other.map(|_| "Ok(report)")),
        }
        assert!(l == limited && r == limited, "a stage that stood down must not touch the buffer");

        // And the gate must not close on real clipping: the same rail with a
        // sine driven hard into it leaves runs far longer than the gate.
        let clipped: Vec<f64> = make_sine(n, sr, 220.0, 1.6)
            .iter()
            .map(|&x| x.clamp(-theta, theta))
            .collect();
        assert!(longest_rail_run(&clipped, (theta, theta)) >= MIN_RUN_GATE);
    }
}
