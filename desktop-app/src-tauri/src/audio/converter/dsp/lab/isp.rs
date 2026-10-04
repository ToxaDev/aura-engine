use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct IspReport {
    pub max_dbtp: f64,
    pub clusters: usize,
    pub fixed: usize,
    pub unfixed: usize,
    /// Clusters whose repair would reach a sample that is itself over full
    /// scale in the source (`hot_spans`): the source's level there, not an
    /// over between its samples. Left whole for the output level to lower;
    /// not among `clusters`.
    pub hot: usize,
    pub residual_dbtp: f64,
    /// Half-open sample spans this pass actually rewrote, per channel. A span
    /// is recorded only when a correction was really applied — not merely
    /// attempted. Nothing in the conversion reads them back: they are the
    /// record of where the pass touched the file, for anything that wants to
    /// show its work.
    #[allow(dead_code)]
    pub fixed_spans_l: Vec<(usize, usize)>,
    #[allow(dead_code)]
    pub fixed_spans_r: Vec<(usize, usize)>,
}

const CEILING: f64 = 1.0;
// Per-sample correction magnitude cap: ≈ 0.5 dB at full scale.
const MAX_DELTA: f64 = 0.056;
const MAX_ITER: usize = 12;
/// The samples a cluster's repair reads and writes around it: its region
/// (8 positions each side) and the window each position reads (3 samples
/// before it, 4 after). A hot sample anywhere in [start − 11, end + 12]
/// leaves the cluster whole (`hot_spans`).
const REACH_BEFORE: usize = 11;
const REACH_AFTER: usize = 12;

/// Lanczos-4 polyphase coefficients (3 inter-sample phases), DC-normalized.
/// Copied from dsp/true_peak.rs — true_peak.rs is not modified.
fn lanczos4_poly_coeffs() -> [[f64; 8]; 3] {
    let mut c = [[0.0_f64; 8]; 3];
    for (pi, phase) in (1u32..=3).enumerate() {
        let t = phase as f64 / 4.0; // 0.25, 0.50, 0.75
        let mut sum = 0.0_f64;
        for k in 0..8_usize {
            let x = t - (k as f64 - 3.0); // offset: -3,-2,-1,0,1,2,3,4
            let v = if x.abs() < 1e-10 {
                1.0
            } else if x.abs() < 4.0 {
                let px = std::f64::consts::PI * x;
                (px.sin() / px) * ((px / 4.0).sin() / (px / 4.0))
            } else {
                0.0
            };
            c[pi][k] = v;
            sum += v;
        }
        for k in 0..8 {
            c[pi][k] /= sum;
        }
    }
    c
}

/// Sample read with reflective edge padding (same convention as true_peak.rs).
#[inline]
fn read_s(buf: &[f64], idx: i64) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let last = (buf.len() as i64) - 1;
    let i = if idx < 0 {
        (-idx).min(last)
    } else if idx > last {
        (2 * last - idx).max(0)
    } else {
        idx
    };
    buf[i as usize]
}

/// Evaluate all 3 inter-sample phases at position n.
/// Returns (max_abs, signed_worst_value, worst_phase_coefficients).
#[inline]
fn eval_phases(buf: &[f64], n: usize, coeffs: &[[f64; 8]; 3]) -> (f64, f64, [f64; 8]) {
    let mut max_abs = 0.0_f64;
    let mut worst_val = 0.0_f64;
    let mut worst_w = [0.0_f64; 8];
    for pi in 0..3 {
        let mut val = 0.0_f64;
        for k in 0..8 {
            val += read_s(buf, (n as i64) + (k as i64) - 3) * coeffs[pi][k];
        }
        if val.abs() > max_abs {
            max_abs = val.abs();
            worst_val = val;
            worst_w = coeffs[pi];
        }
    }
    (max_abs, worst_val, worst_w)
}

/// First pass over one channel: measure initial true peak and locate exceedances.
fn scan_channel(buf: &[f64], coeffs: &[[f64; 8]; 3]) -> (f64, Vec<bool>) {
    let n = buf.len();
    let mut max_tp = buf.iter().fold(0.0_f64, |m, &s| m.max(s.abs()));
    let mut exceed = vec![false; n];
    for i in 0..n {
        let (abs, _, _) = eval_phases(buf, i, coeffs);
        if abs > max_tp {
            max_tp = abs;
        }
        if abs > CEILING {
            exceed[i] = true;
        }
    }
    (max_tp, exceed)
}

/// Group consecutive exceedance flags into (start, end) inclusive clusters.
fn build_clusters(exceed: &[bool]) -> Vec<(usize, usize)> {
    let mut clusters = Vec::new();
    let mut i = 0;
    while i < exceed.len() {
        if exceed[i] {
            let start = i;
            while i < exceed.len() && exceed[i] {
                i += 1;
            }
            clusters.push((start, i - 1));
        } else {
            i += 1;
        }
    }
    clusters
}

/// Apply closed-form minimal-norm corrections to one channel.
/// Returns (fixed, unfixed, residual_max_tp).
fn correct_channel(
    buf: &mut [f64],
    clusters: &[(usize, usize)],
    coeffs: &[[f64; 8]; 3],
) -> (usize, usize, f64, Vec<(usize, usize)>) {
    let n = buf.len();
    let mut fixed = 0;
    let mut unfixed = 0;
    let mut spans: Vec<(usize, usize)> = Vec::new();

    for &(c_start, c_end) in clusters {
        let r_start = c_start.saturating_sub(8);
        let r_end = (c_end + 8).min(n - 1);

        let mut converged = false;
        let mut touched = false;
        for _iter in 0..MAX_ITER {
            // Find the worst remaining exceedance in the local region.
            let mut w_abs = 0.0_f64;
            let mut w_val = 0.0_f64;
            let mut w_pos = r_start;
            let mut w_w = [0.0_f64; 8];

            for pos in r_start..=r_end {
                let (abs, val, w) = eval_phases(buf, pos, coeffs);
                if abs > CEILING && abs > w_abs {
                    w_abs = abs;
                    w_val = val;
                    w_pos = pos;
                    w_w = w;
                }
            }

            if w_abs <= CEILING {
                converged = true;
                break;
            }

            let sum_w2: f64 = w_w.iter().map(|&x| x * x).sum();
            if sum_w2 < 1e-15 {
                break;
            }

            // δ_k = −(p − C·sign(p)) · w_k / Σw_k²
            // Brings p towards ±C (signed).
            let target = CEILING * w_val.signum();
            let excess = w_val - target;
            let mut deltas = [0.0_f64; 8];
            for k in 0..8 {
                deltas[k] = -excess * w_w[k] / sum_w2;
            }

            // Cap: max per-sample |δ| ≤ MAX_DELTA.
            let max_d = deltas.iter().fold(0.0_f64, |m, &d| m.max(d.abs()));
            let scale = if max_d > MAX_DELTA { MAX_DELTA / max_d } else { 1.0 };

            for k in 0..8 {
                let idx = (w_pos as i64) + (k as i64) - 3;
                if idx >= 0 && (idx as usize) < n {
                    buf[idx as usize] += deltas[k] * scale;
                    touched = true;
                }
            }
        }

        if converged {
            fixed += 1;
        } else {
            unfixed += 1;
        }
        if touched {
            spans.push((r_start, (r_end + 1).min(n)));
        }
    }

    // Measure residual true peak over the whole channel.
    let mut res_max = buf.iter().fold(0.0_f64, |m, &s| m.max(s.abs()));
    for i in 0..n {
        let (abs, _, _) = eval_phases(buf, i, coeffs);
        if abs > res_max {
            res_max = abs;
        }
    }
    (fixed, unfixed, res_max, spans)
}

/// Drop clusters overlapping any skip span (±8-sample margin — the correction
/// touches the 8 neighbouring samples of an exceedance).
fn filter_skipped(
    clusters: Vec<(usize, usize)>,
    skip: &[(usize, usize)],
) -> (Vec<(usize, usize)>, usize) {
    if skip.is_empty() {
        return (clusters, 0);
    }
    let mut kept = Vec::with_capacity(clusters.len());
    let mut dropped = 0usize;
    'outer: for (cs, ce) in clusters {
        for &(ss, se) in skip {
            if ce + 8 > ss && cs < se + 8 {
                dropped += 1;
                continue 'outer;
            }
        }
        kept.push((cs, ce));
    }
    (kept, dropped)
}

/// The runs of samples over full scale (|x| > 1) in a source as it was
/// decoded, as half-open spans counted from `from`. There a cluster of overs
/// is the source's level, not an over between its samples, and the repair
/// leaves it whole: pulling the waveform down to full scale there carves the
/// music — a loud MP3 had 1.4 % of its samples rewritten, the error 21 dB
/// under the music above 10 kHz — and the output level brings those places
/// down with the rest of the track. Taken before DC removal, which moves
/// samples: an integer source decodes to [−1, 1) and has none, so a CD is
/// repaired as it always was.
pub fn hot_spans(x: &[f64], from: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    for (i, v) in x.iter().enumerate() {
        if v.abs() > 1.0 {
            open.get_or_insert(i);
        } else if let Some(s) = open.take() {
            spans.push((from + s, from + i));
        }
    }
    if let Some(s) = open {
        spans.push((from + s, from + x.len()));
    }
    spans
}

/// Whether a hot span meets the reach of cluster [cs, ce]
/// ([cs − REACH_BEFORE, ce + REACH_AFTER]). `at` moves past the spans that
/// end before it: the clusters come in order, so those end before every
/// later cluster's reach as well.
#[inline]
fn reaches_hot(hot: &[(usize, usize)], at: &mut usize, cs: usize, ce: usize) -> bool {
    while *at < hot.len() && hot[*at].1 + REACH_BEFORE <= cs {
        *at += 1;
    }
    hot.get(*at).is_some_and(|&(hs, _)| hs <= ce + REACH_AFTER)
}

/// Drop the clusters whose repair would reach a hot sample, and count them.
fn filter_hot(clusters: Vec<(usize, usize)>, hot: &[(usize, usize)]) -> (Vec<(usize, usize)>, usize) {
    if hot.is_empty() {
        return (clusters, 0);
    }
    let mut kept = Vec::with_capacity(clusters.len());
    let (mut at, mut left) = (0usize, 0usize);
    for (cs, ce) in clusters {
        if reaches_hot(hot, &mut at, cs, ce) {
            left += 1;
        } else {
            kept.push((cs, ce));
        }
    }
    (kept, left)
}

/// Intersample peak scan and correction. Returns None on cancel or no exceedance.
/// `skip_l`/`skip_r`: sample spans repaired by declip — their reconstructed
/// peaks intentionally exceed the old rail and must not be squashed back;
/// the output true-peak stage owns level safety for them.
/// `hot_l`/`hot_r`: the source's samples over full scale (`hot_spans`, taken
/// before DC removal) — a cluster whose repair would reach one is left whole.
pub fn run(
    l: &mut [f64],
    r: &mut [f64],
    skip_l: &[(usize, usize)],
    skip_r: &[(usize, usize)],
    hot_l: &[(usize, usize)],
    hot_r: &[(usize, usize)],
    cancel: &AtomicBool,
) -> Option<IspReport> {
    if l.is_empty() {
        return None;
    }

    let coeffs = lanczos4_poly_coeffs();

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let (max_l, exceed_l) = scan_channel(l, &coeffs);

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let (max_r, exceed_r) = scan_channel(r, &coeffs);

    let initial_max = max_l.max(max_r);
    let max_dbtp = 20.0 * (initial_max.max(1e-300)).log10();

    let (clusters_l, dropped_l) = filter_skipped(build_clusters(&exceed_l), skip_l);
    let (clusters_r, dropped_r) = filter_skipped(build_clusters(&exceed_r), skip_r);
    if dropped_l + dropped_r > 0 {
        crate::aelog!(
            "[ISP] {} cluster(s) inside declip-repaired spans left untouched",
            dropped_l + dropped_r
        );
    }
    let (clusters_l, hot_cl) = filter_hot(clusters_l, hot_l);
    let (clusters_r, hot_cr) = filter_hot(clusters_r, hot_r);
    let hot = hot_cl + hot_cr;
    if hot > 0 {
        crate::aelog!("[ISP] {} cluster(s) reaching a source sample over full scale left whole", hot);
    }

    if clusters_l.is_empty() && clusters_r.is_empty() && hot == 0 {
        return None;
    }

    crate::aelog!(
        "[ISP] Initial max {:.2} dBTP; clusters L={} R={}",
        max_dbtp,
        clusters_l.len(),
        clusters_r.len()
    );

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let (fl, ul, res_l, spans_l) = correct_channel(l, &clusters_l, &coeffs);

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let (fr, ur, res_r, spans_r) = correct_channel(r, &clusters_r, &coeffs);

    let residual_max = res_l.max(res_r);
    let residual_dbtp = 20.0 * (residual_max.max(1e-300)).log10();

    let report = IspReport {
        max_dbtp,
        clusters: clusters_l.len() + clusters_r.len(),
        fixed: fl + fr,
        unfixed: ul + ur,
        hot,
        residual_dbtp,
        fixed_spans_l: spans_l,
        fixed_spans_r: spans_r,
    };

    crate::aelog!(
        "[ISP] fixed={} unfixed={} residual={:.2} dBTP",
        report.fixed,
        report.unfixed,
        report.residual_dbtp
    );

    Some(report)
}

// ── The same scan and repair, on a stream ────────────────────────────────────
//
// `run` takes the whole track, and needs it only to go through it once: the
// scan reads the 8 samples around a position, a cluster closes at the first
// position that is not over, and its repair reads and writes nothing more
// than 11 samples before the cluster and 12 after it. So a stream can be
// repaired as it arrives, a few samples behind: `IspStream` scans the
// original samples as they come in, repairs each closed cluster once the 12
// samples after it are there — in order, on what the clusters before it left
// — and hands a sample on once no cluster still to come can reach it. The
// arithmetic is `run`'s line for line, the edge reflection too (at the end
// once `finish` says where the end is), and the test at the bottom holds the
// two together bit for bit, whatever sizes the stream comes in. A live stream
// has no declip spans (declip does not run on one); a file streamed after
// declip has run on the whole of it passes its spans (`with_skips`), and a
// cluster that reaches into one is left as it is, as `run` leaves it. The
// source's samples over full scale (`hot_spans`) come with the stream: a
// file's all at once (`with_hot`), a live stream's with each piece
// (`add_hot`). A cluster is looked at for them when its repair is due — the
// 12 samples after it are in by then, and so are their spans.

/// The longest run of overs the stream waits for. A longer one (a source
/// parked above full scale) is repaired as it stands and the scan goes on
/// with a new cluster: a stream cannot hold its output back without end. Up
/// to this length the result is `run`'s.
const STREAM_MAX_OPEN: usize = 1 << 16;

/// What an `IspStream` has done so far.
#[derive(Clone, Debug, Default)]
pub struct IspStreamStats {
    pub clusters: usize,
    pub fixed: usize,
    pub unfixed: usize,
    /// Clusters left whole at a source sample over full scale (`IspReport::hot`).
    pub hot: usize,
    /// True peak of the stream as it came in so far, dBTP.
    pub max_dbtp: f64,
}

/// One channel of `IspStream`: `scan_channel` and `correct_channel` on a
/// stretch of the stream that moves on.
struct IspChan {
    /// Original samples from `base` on: the scan reads these.
    orig: Vec<f64>,
    /// The same samples as repaired: the corrections read and write these.
    work: Vec<f64>,
    base: usize,
    /// Positions below this have been scanned.
    scanned: usize,
    /// Start of the run of overs the scan is in.
    open: Option<usize>,
    /// Closed clusters waiting for the samples their repair reaches.
    pending: VecDeque<(usize, usize)>,
    /// Samples below this were handed on.
    emitted: usize,
    /// Positions below this went into the residual peak.
    res_at: usize,
    raw_max: f64,
    max_tp: f64,
    out_raw_max: f64,
    res_max: f64,
    clusters: usize,
    fixed: usize,
    unfixed: usize,
    /// Declip's spans, in order and merged where they overlap: a cluster
    /// that reaches into one (±8) is not repaired (`filter_skipped`).
    skip: Vec<(usize, usize)>,
    skip_at: usize,
    dropped: usize,
    /// The source's samples over full scale, in order (`hot_spans`): a
    /// cluster whose repair would reach one is left whole (`filter_hot`).
    hot: Vec<(usize, usize)>,
    hot_at: usize,
    hot_left: usize,
}

impl IspChan {
    fn new(skip: &[(usize, usize)]) -> IspChan {
        let mut sorted = skip.to_vec();
        sorted.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
        for (s, e) in sorted {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        IspChan {
            orig: Vec::new(),
            work: Vec::new(),
            base: 0,
            scanned: 0,
            open: None,
            pending: VecDeque::new(),
            emitted: 0,
            res_at: 0,
            raw_max: 0.0,
            max_tp: 0.0,
            out_raw_max: 0.0,
            res_max: 0.0,
            clusters: 0,
            fixed: 0,
            unfixed: 0,
            skip: merged,
            skip_at: 0,
            dropped: 0,
            hot: Vec::new(),
            hot_at: 0,
            hot_left: 0,
        }
    }

    /// A cluster has closed: it waits for its repair, unless it reaches into
    /// a declip span (`filter_skipped`'s test). Clusters close in order, so
    /// spans that end before this one cannot reach any later one either.
    fn close(&mut self, cs: usize, ce: usize) {
        while self.skip_at < self.skip.len() && self.skip[self.skip_at].1 + 8 <= cs {
            self.skip_at += 1;
        }
        if let Some(&(ss, se)) = self.skip.get(self.skip_at) {
            if ce + 8 > ss && cs < se + 8 {
                self.dropped += 1;
                return;
            }
        }
        self.pending.push_back((cs, ce));
    }

    fn push(&mut self, x: &[f64]) {
        self.raw_max = x.iter().fold(self.raw_max, |m, &s| m.max(s.abs()));
        self.orig.extend_from_slice(x);
        self.work.extend_from_slice(x);
    }

    /// `read_s` in stream indices: `total` is the stream's length once known.
    #[inline]
    fn at(buf: &[f64], base: usize, total: Option<usize>, idx: i64) -> f64 {
        let last = total.map_or(i64::MAX, |n| n as i64 - 1);
        let i = if idx < 0 {
            (-idx).min(last)
        } else if idx > last {
            (2 * last - idx).max(0)
        } else {
            idx
        };
        buf[i as usize - base]
    }

    /// The 8 samples position `n` reads (`n − 3 ..= n + 4`): straight from
    /// the buffer inside the stream, through `at` at its ends, where they
    /// reflect.
    #[inline]
    fn window(buf: &[f64], base: usize, total: Option<usize>, n: usize) -> [f64; 8] {
        let mut w = [0.0_f64; 8];
        if n >= 3 && total.map_or(true, |t| n + 4 < t) {
            w.copy_from_slice(&buf[n - 3 - base..n + 5 - base]);
        } else {
            for (k, x) in w.iter_mut().enumerate() {
                *x = Self::at(buf, base, total, (n as i64) + (k as i64) - 3);
            }
        }
        w
    }

    /// `eval_phases` in stream indices.
    #[inline]
    fn phases(buf: &[f64], base: usize, total: Option<usize>, n: usize, coeffs: &[[f64; 8]; 3]) -> (f64, f64, [f64; 8]) {
        let w = Self::window(buf, base, total, n);
        let mut max_abs = 0.0_f64;
        let mut worst_val = 0.0_f64;
        let mut worst_w = [0.0_f64; 8];
        for pi in 0..3 {
            let mut val = 0.0_f64;
            for k in 0..8 {
                val += w[k] * coeffs[pi][k];
            }
            if val.abs() > max_abs {
                max_abs = val.abs();
                worst_val = val;
                worst_w = coeffs[pi];
            }
        }
        (max_abs, worst_val, worst_w)
    }

    /// `phases`' peak alone, all the scan and the residual read.
    #[inline]
    fn peak(buf: &[f64], base: usize, total: Option<usize>, n: usize, coeffs: &[[f64; 8]; 3]) -> f64 {
        let w = Self::window(buf, base, total, n);
        let mut max_abs = 0.0_f64;
        for c in coeffs {
            let mut val = 0.0_f64;
            for k in 0..8 {
                val += w[k] * c[k];
            }
            if val.abs() > max_abs {
                max_abs = val.abs();
            }
        }
        max_abs
    }

    /// `scan_channel` and `build_clusters` as far as the samples in reach:
    /// position i reads up to i + 4, which must be in (`end`), or the stream
    /// is over (`total`) and the end reflects.
    fn scan(&mut self, end: usize, total: Option<usize>, coeffs: &[[f64; 8]; 3]) {
        let limit = match total {
            Some(n) => n,
            None => end.saturating_sub(4),
        };
        while self.scanned < limit {
            let i = self.scanned;
            let abs = Self::peak(&self.orig, self.base, total, i, coeffs);
            if abs > self.max_tp {
                self.max_tp = abs;
            }
            if abs > CEILING {
                match self.open {
                    None => self.open = Some(i),
                    Some(cs) if i - cs >= STREAM_MAX_OPEN => {
                        self.close(cs, i - 1);
                        self.open = Some(i);
                    }
                    Some(_) => {}
                }
            } else if let Some(cs) = self.open.take() {
                self.close(cs, i - 1);
            }
            self.scanned += 1;
        }
        if let (Some(n), Some(cs)) = (total, self.open) {
            if self.scanned >= n {
                self.open = None;
                self.close(cs, n - 1);
            }
        }
    }

    /// `correct_channel` on each closed cluster whose repair is in reach,
    /// in order — or, where it would reach a hot sample, nothing
    /// (`filter_hot`): the samples it reaches are in, and their spans with them.
    fn repair(&mut self, end: usize, total: Option<usize>, coeffs: &[[f64; 8]; 3]) {
        let n = total.unwrap_or(usize::MAX);
        while let Some(&(c_start, c_end)) = self.pending.front() {
            if total.is_none() && end < c_end + 13 {
                break;
            }
            self.pending.pop_front();
            if reaches_hot(&self.hot, &mut self.hot_at, c_start, c_end) {
                self.hot_left += 1;
                continue;
            }
            self.clusters += 1;
            let r_start = c_start.saturating_sub(8);
            let r_end = (c_end + 8).min(n - 1);

            let mut converged = false;
            for _iter in 0..MAX_ITER {
                let mut w_abs = 0.0_f64;
                let mut w_val = 0.0_f64;
                let mut w_pos = r_start;
                let mut w_w = [0.0_f64; 8];

                for pos in r_start..=r_end {
                    let (abs, val, w) = Self::phases(&self.work, self.base, total, pos, coeffs);
                    if abs > CEILING && abs > w_abs {
                        w_abs = abs;
                        w_val = val;
                        w_pos = pos;
                        w_w = w;
                    }
                }

                if w_abs <= CEILING {
                    converged = true;
                    break;
                }

                let sum_w2: f64 = w_w.iter().map(|&x| x * x).sum();
                if sum_w2 < 1e-15 {
                    break;
                }

                let target = CEILING * w_val.signum();
                let excess = w_val - target;
                let mut deltas = [0.0_f64; 8];
                for k in 0..8 {
                    deltas[k] = -excess * w_w[k] / sum_w2;
                }

                let max_d = deltas.iter().fold(0.0_f64, |m, &d| m.max(d.abs()));
                let scale = if max_d > MAX_DELTA { MAX_DELTA / max_d } else { 1.0 };

                for k in 0..8 {
                    let idx = (w_pos as i64) + (k as i64) - 3;
                    if idx >= 0 && (idx as usize) < n {
                        self.work[idx as usize - self.base] += deltas[k] * scale;
                    }
                }
            }

            if converged {
                self.fixed += 1;
            } else {
                self.unfixed += 1;
            }
        }
    }

    /// Samples below this will not change again: no cluster to come reaches
    /// further back than 11 samples before its start.
    fn final_upto(&self, end: usize, total: Option<usize>) -> usize {
        if let Some(n) = total {
            if self.scanned >= n && self.open.is_none() && self.pending.is_empty() {
                return n;
            }
        }
        let mut lo = self.scanned;
        if let Some(cs) = self.open {
            lo = lo.min(cs);
        }
        if let Some(&(cs, _)) = self.pending.front() {
            lo = lo.min(cs);
        }
        lo.saturating_sub(11).min(end)
    }

    /// The residual peak over the final samples (`correct_channel`'s last
    /// pass), then hand `[emitted, upto)` on and drop what nothing reads again.
    fn take(&mut self, upto: usize, total: Option<usize>, coeffs: &[[f64; 8]; 3], out: &mut Vec<f64>) {
        let limit = match total {
            Some(n) if upto >= n => n,
            _ => upto.saturating_sub(4),
        };
        while self.res_at < limit {
            let abs = Self::peak(&self.work, self.base, total, self.res_at, coeffs);
            if abs > self.res_max {
                self.res_max = abs;
            }
            self.res_at += 1;
        }
        let from = self.emitted - self.base;
        let to = upto - self.base;
        self.out_raw_max = self.work[from..to].iter().fold(self.out_raw_max, |m, &s| m.max(s.abs()));
        out.extend_from_slice(&self.work[from..to]);
        self.emitted = upto;
        let keep = self.emitted.min(self.res_at).min(self.scanned).saturating_sub(64);
        if keep > self.base + 4096 {
            let d = keep - self.base;
            self.orig.drain(..d);
            self.work.drain(..d);
            self.base = keep;
        }
        if self.hot_at > 4096 {
            self.hot.drain(..self.hot_at);
            self.hot_at = 0;
        }
    }
}

/// `run` on a stream that arrives a piece at a time: the same scan, the same
/// clusters and the same repairs, bit for bit, handed on a few samples behind
/// the input (the longest cluster in flight plus 27 samples).
pub struct IspStream {
    coeffs: [[f64; 8]; 3],
    ch: [IspChan; 2],
    end: usize,
    out_l: Vec<f64>,
    out_r: Vec<f64>,
    /// The two channels go through each piece side by side (`in_parallel`).
    parallel: bool,
}

impl Default for IspStream {
    fn default() -> Self {
        IspStream::new()
    }
}

impl IspStream {
    pub fn new() -> IspStream {
        IspStream::with_skips(&[], &[])
    }

    /// A file streamed after declip ran on the whole of it: the clusters
    /// that reach into its spans (`skip_l`, `skip_r`, sample indices from the
    /// stream's start) are left alone, as `run` leaves them.
    pub fn with_skips(skip_l: &[(usize, usize)], skip_r: &[(usize, usize)]) -> IspStream {
        IspStream {
            coeffs: lanczos4_poly_coeffs(),
            ch: [IspChan::new(skip_l), IspChan::new(skip_r)],
            end: 0,
            out_l: Vec::new(),
            out_r: Vec::new(),
            parallel: false,
        }
    }

    /// The channels repaired side by side, on the rayon pool the caller runs
    /// in: each is its own scan and its own clusters, so the result is the
    /// same. For a file streamed many times faster than it plays, in large
    /// pieces; a live stream's small ones go one channel after the other.
    pub fn in_parallel(mut self) -> IspStream {
        self.parallel = true;
        self
    }

    /// The source's samples over full scale (`hot_spans`, from the stream's
    /// start), all known before it starts: a file's, taken on the whole track
    /// before DC removal. A cluster whose repair would reach one is left
    /// whole, as `run` leaves it.
    pub fn with_hot(mut self, hot_l: &[(usize, usize)], hot_r: &[(usize, usize)]) -> IspStream {
        self.add_hot(hot_l, hot_r);
        self
    }

    /// More of them, for samples still to come: a live stream's, found on
    /// each piece before its DC removal and given before the piece is pushed,
    /// in order.
    pub fn add_hot(&mut self, hot_l: &[(usize, usize)], hot_r: &[(usize, usize)]) {
        self.ch[0].hot.extend_from_slice(hot_l);
        self.ch[1].hot.extend_from_slice(hot_r);
    }

    fn step(&mut self, total: Option<usize>, emit: &mut dyn FnMut(&[f64], &[f64])) {
        let (end, coeffs, par) = (self.end, &self.coeffs, self.parallel);
        let [a, b] = &mut self.ch;
        let advance = |c: &mut IspChan| {
            c.scan(end, total, coeffs);
            c.repair(end, total, coeffs);
        };
        if par {
            rayon::join(|| advance(a), || advance(b));
        } else {
            advance(a);
            advance(b);
        }
        let upto = a.final_upto(end, total).min(b.final_upto(end, total));
        if upto > a.emitted {
            let (ol, or) = (&mut self.out_l, &mut self.out_r);
            ol.clear();
            or.clear();
            if par {
                rayon::join(|| a.take(upto, total, coeffs, ol), || b.take(upto, total, coeffs, or));
            } else {
                a.take(upto, total, coeffs, ol);
                b.take(upto, total, coeffs, or);
            }
            emit(&self.out_l, &self.out_r);
        }
    }

    /// Take the next samples; `emit` gets every sample that is final now, the
    /// same count for both channels, in order.
    pub fn push(&mut self, l: &[f64], r: &[f64], emit: &mut dyn FnMut(&[f64], &[f64])) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        self.ch[0].push(&l[..n]);
        self.ch[1].push(&r[..n]);
        self.end += n;
        self.step(None, emit);
    }

    /// The stream is over: the rest is scanned and repaired with the end where
    /// it is, and handed on. Returns `run`'s report on the whole stream —
    /// None where `run` returns None (nothing over, or nothing at all).
    pub fn finish(mut self, emit: &mut dyn FnMut(&[f64], &[f64])) -> Option<IspReport> {
        if self.end == 0 {
            return None;
        }
        let total = Some(self.end);
        self.step(total, emit);
        let [a, b] = &self.ch;
        if a.dropped + b.dropped > 0 {
            crate::aelog!("[ISP] {} cluster(s) inside declip-repaired spans left untouched", a.dropped + b.dropped);
        }
        let hot = a.hot_left + b.hot_left;
        if hot > 0 {
            crate::aelog!("[ISP] {} cluster(s) reaching a source sample over full scale left whole", hot);
        }
        if a.clusters + b.clusters + hot == 0 {
            return None;
        }
        let initial_max = a.raw_max.max(a.max_tp).max(b.raw_max.max(b.max_tp));
        let residual_max = a.out_raw_max.max(a.res_max).max(b.out_raw_max.max(b.res_max));
        Some(IspReport {
            max_dbtp: 20.0 * (initial_max.max(1e-300)).log10(),
            clusters: a.clusters + b.clusters,
            fixed: a.fixed + b.fixed,
            unfixed: a.unfixed + b.unfixed,
            hot,
            residual_dbtp: 20.0 * (residual_max.max(1e-300)).log10(),
            fixed_spans_l: Vec::new(),
            fixed_spans_r: Vec::new(),
        })
    }

    pub fn stats(&self) -> IspStreamStats {
        let [a, b] = &self.ch;
        let max = a.raw_max.max(a.max_tp).max(b.raw_max.max(b.max_tp));
        IspStreamStats {
            clusters: a.clusters + b.clusters,
            fixed: a.fixed + b.fixed,
            unfixed: a.unfixed + b.unfixed,
            hot: a.hot_left + b.hot_left,
            max_dbtp: 20.0 * max.max(1e-300).log10(),
        }
    }
}

// ── Output-side local true-peak limiting ─────────────────────────────────────
//
// The pass above corrects the overs the SOURCE carries. Measured on Santana —
// Corazón (2014), Mal Bicho: it fixed 10,277 of 10,812 clusters and handed
// the filter a signal at +0.03 dBTP — and the output still needed −3.15 dB
// against the release build's −3.42. A quarter of a decibel. The peak that
// costs the level is not in the source; it is what a 30M-tap reconstruction
// and the hybrid-phase engine make of a waveform parked against a limiter
// ceiling for the whole track. The render comes out near +2.6 dBTP whatever
// happened before the filter. So the correction that can buy the level back
// has to sit here, after the filter and the phase engine, on the output
// peaks themselves.
//
// What this is: a look-ahead peak limiter with a raised-cosine gain dip
// around each over, stereo-linked, applied only where the output exceeds the
// ceiling. What it is not: a repair. It trades a few milliseconds of gain
// reduction at a handful of peaks for not turning the whole file down. It
// refuses — leaving the global scalar to its usual job — when the overs are
// not sparse: more than MAX_LOCAL_GR at any one cluster, or gain reduction
// over more than MAX_GR_FRACTION of the file, means the material is over
// everywhere and "local" would be a lie.

pub struct OutputLimitReport {
    pub clusters: usize,
    /// Deepest gain reduction any cluster asked for, dB (≤ 0).
    pub max_gr_db: f64,
    /// Samples with gain below 1.0, as milliseconds and as a fraction.
    pub gr_ms: f64,
    pub gr_fraction: f64,
    /// Why nothing was applied, if nothing was.
    pub fell_back: Option<&'static str>,
}

const OUT_ATTACK_MS: f64 = 1.5;
const OUT_RELEASE_MS: f64 = 1.5;
/// Overs closer than this are one event and share one dip.
const OUT_GAP_MS: f64 = 2.0;
/// −6 dB. A cluster that needs more than this is not an overshoot, it is the
/// programme, and the global ceiling should have it.
const MAX_LOCAL_GR: f64 = 0.501_187_2;
const MAX_GR_FRACTION: f64 = 0.05;

pub fn limit_output(
    l: &mut [f64],
    r: &mut [f64],
    target_lin: f64,
    out_rate: u32,
) -> Option<OutputLimitReport> {
    limit_output_inner(l, r, target_lin, out_rate, true)
}

/// `limit_output` without its refusals: every over comes down to the
/// ceiling. For the player's windowed limiter, whose track-wide decision was
/// made before playback (`output_limit_is_local`, and the global gain when
/// it says no): a quarter-second window that happens to be dense must still
/// be held, or its overs reach the DAC.
pub fn limit_output_always(
    l: &mut [f64],
    r: &mut [f64],
    target_lin: f64,
    out_rate: u32,
) -> Option<OutputLimitReport> {
    limit_output_inner(l, r, target_lin, out_rate, false)
}

/// `limit_output`'s decision for a whole track before it is rendered: made on
/// the source's 4× true-peak view (what the reconstruction filter turns into
/// output peaks), with `limit_output`'s own rule. True: local limiting at the
/// ceiling is honest (or nothing is over); false: the overs are not sparse or
/// too deep, and the whole track must come down instead.
pub fn output_limit_is_local(l: &[f64], r: &[f64], target_lin: f64, rate: u32) -> bool {
    let rate4 = rate.saturating_mul(4);
    let mut scan = OverScan::new(target_lin, rate4);
    crate::audio::converter::dsp::true_peak::for_each_4x_block(l, r, 1 << 16, |a, b| scan.push(a, b));
    let (clusters, n) = scan.finish();
    match plan_output_limit(clusters, n, target_lin, rate4) {
        None => true,
        Some(p) => p.report.fell_back.is_none(),
    }
}

/// `output_limit_is_local` in pieces side by side on the current rayon pool:
/// each piece's 4× view from its own samples and the 3 before and 4 after it
/// (zeros past either end of the whole, as there), its overs found by an
/// `OverScan` of its own, and the runs joined as one scan finds them
/// (`join_cluster_runs`) — the same decision.
pub fn output_limit_is_local_parallel(l: &[f64], r: &[f64], target_lin: f64, rate: u32) -> bool {
    use rayon::prelude::*;
    let n = l.len().min(r.len());
    let rate4 = rate.saturating_mul(4);
    const PIECE: usize = 1 << 18;
    let parts: Vec<Vec<(usize, usize, f64)>> = (0..n.div_ceil(PIECE))
        .into_par_iter()
        .map(|k| {
            let (a, b) = (k * PIECE, ((k + 1) * PIECE).min(n));
            let (w0, w1) = (a.saturating_sub(3), (b + 4).min(n));
            let (lo4, hi4) = ((a - w0) * 4, (b - w0) * 4);
            let mut scan = OverScan::new(target_lin, rate4);
            let mut idx = 0usize;
            crate::audio::converter::dsp::true_peak::for_each_4x_block(&l[w0..w1], &r[w0..w1], 1 << 16, |x, y| {
                let (j0, j1) = (idx, idx + x.len());
                let (s0, s1) = (j0.max(lo4).min(j1), j1.min(hi4).max(j0.max(lo4).min(j1)));
                if s1 > s0 {
                    scan.push(&x[s0 - j0..s1 - j0], &y[s0 - j0..s1 - j0]);
                }
                idx = j1;
            });
            let (clusters, _) = scan.finish();
            clusters.into_iter().map(|(c0, c1, p)| (c0 + 4 * a, c1 + 4 * a, p)).collect()
        })
        .collect();
    let clusters = join_cluster_runs(parts, rate4);
    match plan_output_limit(clusters, n * 4, target_lin, rate4) {
        None => true,
        Some(p) => p.report.fell_back.is_none(),
    }
}

fn limit_output_inner(
    l: &mut [f64],
    r: &mut [f64],
    target_lin: f64,
    out_rate: u32,
    may_refuse: bool,
) -> Option<OutputLimitReport> {
    let n = l.len().min(r.len());
    if n == 0 || !(target_lin > 0.0) || out_rate == 0 {
        return None;
    }
    let over_at = |i: usize| l[i].abs().max(r[i].abs());
    let ms = |v: f64| ((v / 1000.0) * out_rate as f64).round() as usize;
    let (attack, release, gap) = (ms(OUT_ATTACK_MS).max(1), ms(OUT_RELEASE_MS).max(1), ms(OUT_GAP_MS));

    // Clusters of over-samples, merged across gaps shorter than OUT_GAP_MS.
    let mut clusters: Vec<(usize, usize, f64)> = Vec::new(); // start, end (inclusive), peak
    let mut i = 0usize;
    while i < n {
        if over_at(i) > target_lin {
            let start = i;
            let mut end = i;
            let mut peak = over_at(i);
            let mut j = i + 1;
            while j < n && j - end <= gap {
                let v = over_at(j);
                if v > target_lin {
                    end = j;
                    if v > peak {
                        peak = v;
                    }
                }
                j += 1;
            }
            clusters.push((start, end, peak));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    if clusters.is_empty() {
        return None;
    }

    // The gain curve: unity, with one raised-cosine dip per cluster, the
    // deepest dip winning where they overlap.
    let mut gain = vec![1.0f64; n];
    let mut max_gr = 1.0f64;
    for &(s, e, p) in &clusters {
        let g = target_lin / p;
        if g < max_gr {
            max_gr = g;
        }
        let a0 = s.saturating_sub(attack);
        for i in a0..s {
            let t = (i - a0) as f64 / attack as f64;
            let w = 0.5 * (1.0 - (std::f64::consts::PI * t).cos());
            let v = 1.0 - (1.0 - g) * w;
            if v < gain[i] {
                gain[i] = v;
            }
        }
        for i in s..=e {
            if g < gain[i] {
                gain[i] = g;
            }
        }
        let r1 = (e + 1 + release).min(n);
        for i in (e + 1)..r1 {
            let t = (i - e) as f64 / release as f64;
            let w = 0.5 * (1.0 + (std::f64::consts::PI * t).cos());
            let v = 1.0 - (1.0 - g) * w;
            if v < gain[i] {
                gain[i] = v;
            }
        }
    }
    let covered = gain.iter().filter(|&&g| g < 1.0).count();
    let fraction = covered as f64 / n as f64;
    let mut report = OutputLimitReport {
        clusters: clusters.len(),
        max_gr_db: 20.0 * max_gr.log10(),
        gr_ms: covered as f64 * 1000.0 / out_rate as f64,
        gr_fraction: fraction,
        fell_back: None,
    };
    if may_refuse && max_gr < MAX_LOCAL_GR {
        report.fell_back = Some("a cluster needs more than 6 dB");
        return Some(report);
    }
    if may_refuse && fraction > MAX_GR_FRACTION {
        report.fell_back = Some("overs are not sparse");
        return Some(report);
    }
    for i in 0..n {
        l[i] *= gain[i];
        r[i] *= gain[i];
    }
    Some(report)
}

// ── The same limiter, for a file that never sits in memory whole ─────────────
//
// The segmented route writes a very long file through temp files a chunk at a
// time, and `limit_output` wants the whole buffer. It does not need it: finding
// the overs is one pass in order, and the gain at any sample depends only on
// the clusters near it. So the route runs it in two passes — `OverScan` over
// the render to find the clusters, `plan_output_limit` to make the same
// decision `limit_output` makes, then `LimiterPlan::apply` a chunk at a time.
// The arithmetic is `limit_output`'s, line for line; the test at the bottom
// holds the two together bit for bit, and any change to one has to be made to
// the other.

/// `limit_output`'s cluster search, one sample at a time.
pub struct OverScan {
    target: f64,
    gap: usize,
    pos: usize,
    open: Option<(usize, usize, f64)>,
    clusters: Vec<(usize, usize, f64)>,
}

impl OverScan {
    pub fn new(target_lin: f64, out_rate: u32) -> Self {
        let gap = ((OUT_GAP_MS / 1000.0) * out_rate as f64).round() as usize;
        Self { target: target_lin, gap, pos: 0, open: None, clusters: Vec::new() }
    }

    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        for (&a, &b) in l.iter().zip(r.iter()) {
            let i = self.pos;
            let v = a.abs().max(b.abs());
            // A cluster closes at the first sample more than `gap` past its
            // last over; that sample is then looked at afresh, as
            // limit_output's outer loop does.
            if let Some((_, end, _)) = self.open {
                if i - end > self.gap {
                    self.clusters.push(self.open.take().unwrap());
                }
            }
            match self.open.as_mut() {
                Some((_, end, peak)) => {
                    if v > self.target {
                        *end = i;
                        if v > *peak {
                            *peak = v;
                        }
                    }
                }
                None => {
                    if v > self.target {
                        self.open = Some((i, i, v));
                    }
                }
            }
            self.pos += 1;
        }
    }

    /// The clusters, and how many samples were scanned.
    pub fn finish(mut self) -> (Vec<(usize, usize, f64)>, usize) {
        if let Some(c) = self.open.take() {
            self.clusters.push(c);
        }
        (self.clusters, self.pos)
    }
}

/// Clusters found by several `OverScan`s over consecutive stretches of one
/// signal (their indices already global), as one scan over the whole would
/// have found them: a cluster that runs across a seam is one cluster.
pub fn join_cluster_runs(parts: Vec<Vec<(usize, usize, f64)>>, out_rate: u32) -> Vec<(usize, usize, f64)> {
    let gap = ((OUT_GAP_MS / 1000.0) * out_rate as f64).round() as usize;
    let mut out: Vec<(usize, usize, f64)> = Vec::new();
    for c in parts.into_iter().flatten() {
        match out.last_mut() {
            Some(last) if c.0 <= last.1 + gap => {
                last.1 = last.1.max(c.1);
                last.2 = last.2.max(c.2);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `limit_output`'s decision and gain curve, for a file of `n` samples.
pub struct LimiterPlan {
    clusters: Vec<(usize, usize, f64)>,
    target: f64,
    attack: usize,
    release: usize,
    n: usize,
    pub report: OutputLimitReport,
}

/// What `limit_output` would decide for these clusters: None where it returns
/// None, otherwise the same report. The dips are applied only if
/// `report.fell_back` is None.
pub fn plan_output_limit(
    clusters: Vec<(usize, usize, f64)>,
    n: usize,
    target_lin: f64,
    out_rate: u32,
) -> Option<LimiterPlan> {
    if n == 0 || !(target_lin > 0.0) || out_rate == 0 || clusters.is_empty() {
        return None;
    }
    let ms = |v: f64| ((v / 1000.0) * out_rate as f64).round() as usize;
    let (attack, release) = (ms(OUT_ATTACK_MS).max(1), ms(OUT_RELEASE_MS).max(1));
    let mut max_gr = 1.0f64;
    for &(_, _, p) in &clusters {
        let g = target_lin / p;
        if g < max_gr {
            max_gr = g;
        }
    }
    let mut plan = LimiterPlan {
        clusters,
        target: target_lin,
        attack,
        release,
        n,
        report: OutputLimitReport {
            clusters: 0,
            max_gr_db: 20.0 * max_gr.log10(),
            gr_ms: 0.0,
            gr_fraction: 0.0,
            fell_back: None,
        },
    };
    plan.report.clusters = plan.clusters.len();
    // Samples with gain below 1.0, counted a window at a time.
    let mut covered = 0usize;
    let mut g = vec![0.0f64; 1 << 20];
    let mut at = 0usize;
    while at < n {
        let len = g.len().min(n - at);
        plan.gain_into(at, &mut g[..len]);
        covered += g[..len].iter().filter(|&&v| v < 1.0).count();
        at += len;
    }
    let fraction = covered as f64 / n as f64;
    plan.report.gr_ms = covered as f64 * 1000.0 / out_rate as f64;
    plan.report.gr_fraction = fraction;
    if max_gr < MAX_LOCAL_GR {
        plan.report.fell_back = Some("a cluster needs more than 6 dB");
    } else if fraction > MAX_GR_FRACTION {
        plan.report.fell_back = Some("overs are not sparse");
    }
    Some(plan)
}

impl LimiterPlan {
    /// The gain curve for samples [start, start + out.len()).
    pub fn gain_into(&self, start: usize, out: &mut [f64]) {
        for v in out.iter_mut() {
            *v = 1.0;
        }
        let end = start + out.len();
        // Clusters are in order and their dips are too; skip those whose dip
        // ends before this window.
        let first = self
            .clusters
            .partition_point(|&(_, e, _)| (e + 1 + self.release).min(self.n) <= start);
        for &(s, e, p) in &self.clusters[first..] {
            let a0 = s.saturating_sub(self.attack);
            if a0 >= end {
                break;
            }
            let g = self.target / p;
            for i in a0.max(start)..s.min(end) {
                let t = (i - a0) as f64 / self.attack as f64;
                let w = 0.5 * (1.0 - (std::f64::consts::PI * t).cos());
                let v = 1.0 - (1.0 - g) * w;
                if v < out[i - start] {
                    out[i - start] = v;
                }
            }
            for i in s.max(start)..(e + 1).min(end) {
                if g < out[i - start] {
                    out[i - start] = g;
                }
            }
            let r1 = (e + 1 + self.release).min(self.n);
            for i in (e + 1).max(start)..r1.min(end) {
                let t = (i - e) as f64 / self.release as f64;
                let w = 0.5 * (1.0 + (std::f64::consts::PI * t).cos());
                let v = 1.0 - (1.0 - g) * w;
                if v < out[i - start] {
                    out[i - start] = v;
                }
            }
        }
    }

    /// Whether the dips go in — `limit_output` applies them only when it
    /// did not fall back.
    pub fn applies(&self) -> bool {
        self.report.fell_back.is_none()
    }

    /// Apply the dips to samples [start, start + l.len()), as `limit_output`
    /// would to the same samples of the whole buffer. `scratch` is reused.
    pub fn apply(&self, start: usize, l: &mut [f64], r: &mut [f64], scratch: &mut Vec<f64>) {
        if !self.applies() {
            return;
        }
        let len = l.len().min(r.len());
        scratch.resize(len, 1.0);
        self.gain_into(start, &mut scratch[..len]);
        for i in 0..len {
            l[i] *= scratch[i];
            r[i] *= scratch[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::sync::atomic::AtomicBool;

    /// Construct the 8-sample adversarial pattern that maximises the Lanczos-4
    /// phase-0.25 interpolated value.  For each tap k, the sample sign is chosen
    /// to match sign(w_k(0.25)); the resulting true peak ≈ A·Σ|w_k| ≈ 1.42 for
    /// A=0.99 — well above the 1.0 ceiling.  This is the concrete realisation of
    /// the "alternating near-fullscale" scenario from the spec.
    fn adversarial_pattern(amplitude: f64) -> Vec<f64> {
        // Signs derived from the (pre-normalisation) phase-0.25 Lanczos-4 weights.
        // Negative weight → negative sample sign so w_k·x_k > 0.
        //   k : 0  1  2  3  4  5  6  7
        //   w : -  +  -  +  +  -  +  -
        let signs = [-1.0_f64, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -1.0];
        signs.iter().map(|&s| s * amplitude).collect()
    }

    #[test]
    fn adversarial_pattern_gets_fixed() {
        let cancel = AtomicBool::new(false);
        // The phase-0.50 kernel has Σ|w_k| ≈ 1.715 (the largest of the three phases),
        // so the minimum amplitude for exceedance is 1.0/1.715 ≈ 0.583.  At 0.60 the
        // initial true peak ≈ 1.029 (+0.25 dBTP).  The excess is small enough that the
        // correction converges in one step without hitting the per-step cap, keeping
        // total per-sample delta ≈ 0.022, well below MAX_DELTA — matching the spec's
        // "sample deltas < 0.5 dB" criterion.
        let amplitude = 0.60_f64; // Σ|w_k(0.50)|·0.60 ≈ 1.029 > 1.0
        let mut buf = vec![0.0_f64; 256];
        let offset = 100;
        let pattern = adversarial_pattern(amplitude);
        for (i, &v) in pattern.iter().enumerate() {
            buf[offset + i] = v;
        }
        let l_orig = buf.clone();
        let mut l = buf.clone();
        let mut r = buf.clone();

        let report = run(&mut l, &mut r, &[], &[], &[], &[], &cancel);
        assert!(
            report.is_some(),
            "Expected exceedance: Σ|w_k(0.50)|·{} ≈ {:.4} > 1.0",
            amplitude,
            amplitude * 1.715
        );
        let rep = report.unwrap();
        assert!(rep.clusters >= 2, "Expected clusters in both channels");
        assert!(rep.fixed > 0, "Expected at least one cluster fixed");
        assert!(
            rep.residual_dbtp <= 0.0,
            "Residual {:.3} dBTP must be ≤ 0.0 dBTP after correction",
            rep.residual_dbtp
        );
        // Total per-sample change stays within MAX_DELTA when the initial
        // exceedance is small (the cap is slack; correction converges in one step).
        let max_delta = l
            .iter()
            .zip(l_orig.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            max_delta <= MAX_DELTA + 1e-9,
            "Max per-sample delta {:.5} exceeds cap {:.3}",
            max_delta,
            MAX_DELTA
        );
    }

    #[test]
    fn clean_minus6db_sine_untouched() {
        let cancel = AtomicBool::new(false);
        // 440 Hz at −6 dBFS; true peak well below 1.0, no correction expected.
        let n = 2048;
        let mut l: Vec<f64> = (0..n)
            .map(|i| 0.5 * (2.0 * PI * 440.0 * i as f64 / 44100.0).sin())
            .collect();
        let mut r = l.clone();
        let l_orig = l.clone();
        let r_orig = r.clone();

        let report = run(&mut l, &mut r, &[], &[], &[], &[], &cancel);
        assert!(
            report.is_none(),
            "Clean -6 dB sine should return None (no intersample exceedance)"
        );
        for i in 0..n {
            assert_eq!(
                l[i], l_orig[i],
                "Sample l[{}] modified on clean signal",
                i
            );
            assert_eq!(
                r[i], r_orig[i],
                "Sample r[{}] modified on clean signal",
                i
            );
        }
    }

    /// A programme with intersample overs everywhere a stream can trip on
    /// them: at the very start and end (the reflection), close enough for
    /// their repairs to overlap, across the chunk edges the test feeds, too
    /// strong to fix, and long runs of a tone above full scale.
    fn isp_stream_cases() -> Vec<(Vec<f64>, Vec<f64>)> {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        let n = 20_000;
        let mut prog = |amp: f64, f: f64| -> Vec<f64> {
            (0..n).map(|i| amp * (2.0 * PI * f * i as f64 / 44_100.0).sin() + 0.05 * noise()).collect()
        };
        let mut l = prog(0.3, 440.0);
        let mut r = prog(0.25, 330.0);
        let put = |x: &mut Vec<f64>, at: usize, amp: f64| {
            for (k, v) in adversarial_pattern(amp).into_iter().enumerate() {
                if at + k < x.len() {
                    x[at + k] = v;
                }
            }
        };
        for &(at, amp) in &[(0, 0.7), (100, 0.62), (112, 0.66), (1_000, 0.9), (4_090, 0.64), (8_189, 0.75), (10_000, 0.99), (19_992, 0.7), (19_997, 0.8)] {
            put(&mut l, at, amp);
        }
        for &(at, amp) in &[(2, 0.65), (36, 0.7), (4_093, 0.61), (12_000, 0.95), (12_011, 0.68), (19_995, 0.66)] {
            put(&mut r, at, amp);
        }
        // A 90 Hz tone over full scale for a while: runs of overs dozens of
        // samples long.
        for i in 5_000..7_000 {
            l[i] = 1.08 * (2.0 * PI * 90.0 * i as f64 / 44_100.0).sin();
        }
        let clean_l: Vec<f64> = (0..n).map(|i| 0.5 * (2.0 * PI * 440.0 * i as f64 / 44_100.0).sin()).collect();
        let mut cases = vec![(l, r), (clean_l.clone(), clean_l)];
        // Streams shorter than the scan's reach.
        for len in [1usize, 3, 7, 12] {
            let x: Vec<f64> = (0..len).map(|i| if i % 2 == 0 { 1.2 } else { -0.9 }).collect();
            cases.push((x.clone(), x.iter().map(|v| v * 0.5).collect()));
        }
        cases
    }

    /// `IspStream` over `l`/`r` in the given chunk sizes, its channels side
    /// by side or not: what it handed on, and its report.
    fn stream_isp(l: &[f64], r: &[f64], chunks: &[usize], parallel: bool) -> (Vec<f64>, Vec<f64>, Option<IspReport>) {
        let mut s = if parallel { IspStream::new().in_parallel() } else { IspStream::new() };
        let (mut ol, mut or) = (Vec::new(), Vec::new());
        let (mut at, mut k) = (0usize, 0usize);
        while at < l.len() {
            let c = chunks[k % chunks.len()].min(l.len() - at);
            s.push(&l[at..at + c], &r[at..at + c], &mut |a, b| {
                ol.extend_from_slice(a);
                or.extend_from_slice(b);
            });
            at += c;
            k += 1;
        }
        let rep = s.finish(&mut |a, b| {
            ol.extend_from_slice(a);
            or.extend_from_slice(b);
        });
        (ol, or, rep)
    }

    fn same_bits(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    /// The stream repairs exactly what `run` repairs on the whole buffer:
    /// every sample to the bit and the same report, whatever sizes the
    /// stream arrives in — one sample at a time, odd sizes, chunk edges
    /// inside clusters and inside their repairs, the whole at once — with
    /// its channels one after the other or side by side.
    #[test]
    fn streaming_isp_matches_run_bit_for_bit() {
        let cancel = AtomicBool::new(false);
        for (ci, (l, r)) in isp_stream_cases().iter().enumerate() {
            let (mut bl, mut br) = (l.clone(), r.clone());
            let batch = run(&mut bl, &mut br, &[], &[], &[], &[], &cancel);
            if ci == 0 {
                let rep = batch.as_ref().expect("the first case has overs");
                assert!(rep.clusters > 20 && rep.fixed > 0 && rep.unfixed > 0, "clusters {} fixed {} unfixed {}", rep.clusters, rep.fixed, rep.unfixed);
            }
            for (chunks, par) in [(&[l.len()][..], false), (&[1][..], false), (&[37][..], false), (&[4_096][..], false),
                (&[1_000, 3, 65_536, 7][..], false), (&[l.len()][..], true), (&[37][..], true), (&[1_000, 3, 65_536, 7][..], true)] {
                let (sl, sr, rep) = stream_isp(l, r, chunks, par);
                assert!(same_bits(&sl, &bl) && same_bits(&sr, &br), "case {ci}: samples differ, chunks {chunks:?}");
                match (&batch, &rep) {
                    (None, None) => {}
                    (Some(a), Some(b)) => {
                        assert_eq!((a.clusters, a.fixed, a.unfixed), (b.clusters, b.fixed, b.unfixed), "case {ci}, chunks {chunks:?}");
                        assert_eq!(a.max_dbtp.to_bits(), b.max_dbtp.to_bits(), "case {ci}, chunks {chunks:?}");
                        assert_eq!(a.residual_dbtp.to_bits(), b.residual_dbtp.to_bits(), "case {ci}, chunks {chunks:?}");
                    }
                    _ => panic!("case {ci}, chunks {chunks:?}: one reported and the other did not"),
                }
            }
        }
    }

    /// A file streamed after declip leaves the clusters that reach into
    /// declip's spans as `run` leaves them: the same samples to the bit and
    /// the same report, the spans given out of order, nested and touching.
    #[test]
    fn streaming_isp_with_declip_spans_matches_run_bit_for_bit() {
        let cancel = AtomicBool::new(false);
        for (ci, (l, r)) in isp_stream_cases().iter().enumerate() {
            let n = l.len();
            let mut skip_l: Vec<(usize, usize)> = (0..n / 2_000).map(|k| (k * 2_000 + 300, k * 2_000 + 700)).collect();
            skip_l.reverse();
            skip_l.push((310, 330));
            skip_l.push((420, 500));
            let skip_r: Vec<(usize, usize)> = (0..n / 5_000).map(|k| (k * 5_000 + 40, k * 5_000 + 41)).collect();
            let (mut bl, mut br) = (l.clone(), r.clone());
            let batch = run(&mut bl, &mut br, &skip_l, &skip_r, &[], &[], &cancel);
            let (mut all_l, mut all_r) = (l.clone(), r.clone());
            let unskipped = run(&mut all_l, &mut all_r, &[], &[], &[], &[], &cancel);
            if ci == 0 {
                let (a, b) = (batch.as_ref().expect("overs"), unskipped.as_ref().expect("overs"));
                assert!(a.clusters < b.clusters, "the spans hold some clusters back: {} of {}", a.clusters, b.clusters);
            }
            for (chunks, par) in [(&[n][..], false), (&[1][..], false), (&[1_000, 3, 65_536, 7][..], false), (&[1_000, 3, 65_536, 7][..], true)] {
                let s = IspStream::with_skips(&skip_l, &skip_r);
                let mut s = if par { s.in_parallel() } else { s };
                let (mut sl, mut sr) = (Vec::new(), Vec::new());
                let (mut at, mut k) = (0usize, 0usize);
                while at < n {
                    let c = chunks[k % chunks.len()].min(n - at);
                    s.push(&l[at..at + c], &r[at..at + c], &mut |a, b| {
                        sl.extend_from_slice(a);
                        sr.extend_from_slice(b);
                    });
                    at += c;
                    k += 1;
                }
                let rep = s.finish(&mut |a, b| {
                    sl.extend_from_slice(a);
                    sr.extend_from_slice(b);
                });
                assert!(same_bits(&sl, &bl) && same_bits(&sr, &br), "case {ci}: samples differ, chunks {chunks:?}");
                match (&batch, &rep) {
                    (None, None) => {}
                    (Some(a), Some(b)) => {
                        assert_eq!((a.clusters, a.fixed, a.unfixed), (b.clusters, b.fixed, b.unfixed), "case {ci}, chunks {chunks:?}");
                        assert_eq!(a.residual_dbtp.to_bits(), b.residual_dbtp.to_bits(), "case {ci}, chunks {chunks:?}");
                    }
                    _ => panic!("case {ci}, chunks {chunks:?}: one reported and the other did not"),
                }
            }
        }
    }

    /// The output limiter's decision and the true peak, made in pieces side
    /// by side, are the whole-buffer ones: overs at the seams of the pieces,
    /// at both ends, sparse (held locally) and dense (the track comes down).
    #[test]
    fn the_limiters_decision_and_the_true_peak_in_pieces_are_the_whole_ones() {
        use crate::audio::converter::dsp::true_peak::{measure_true_peak, measure_true_peak_parallel};
        let n = 700_000usize;
        let piece = 1usize << 18;
        let base: Vec<f64> = (0..n).map(|i| 0.5 * (2.0 * PI * 997.0 * i as f64 / 44_100.0).sin()).collect();
        let mut sparse_l = base.clone();
        let sparse_r: Vec<f64> = base.iter().map(|v| v * 0.8).collect();
        for &at in &[0usize, 3, piece - 2, piece + 1, 2 * piece - 4, n - 3] {
            for (k, v) in adversarial_pattern(0.9).into_iter().enumerate() {
                if at + k < n {
                    sparse_l[at + k] = v;
                }
            }
        }
        let dense: Vec<f64> = (0..n).map(|i| 1.05 * (2.0 * PI * 3_000.0 * i as f64 / 44_100.0).sin()).collect();
        for (l, r) in [(&sparse_l, &sparse_r), (&dense, &base), (&base, &base)] {
            for target in [0.89, 0.6] {
                assert_eq!(
                    output_limit_is_local_parallel(l, r, target, 44_100),
                    output_limit_is_local(l, r, target, 44_100),
                    "target {target}"
                );
            }
            assert_eq!(measure_true_peak_parallel(l, r).to_bits(), measure_true_peak(l, r).to_bits());
        }
        assert!(output_limit_is_local(&sparse_l, &sparse_r, 0.89, 44_100), "sparse overs are held locally");
        assert!(!output_limit_is_local(&dense, &base, 0.6, 44_100), "dense ones are not");
    }

    /// The stream hands samples on a few behind what came in, not at the end:
    /// 15 behind with nothing over, and no more than the cluster in flight
    /// plus 27 while one is.
    #[test]
    fn streaming_isp_hands_samples_on_as_it_goes() {
        let n = 50_000;
        let mut x: Vec<f64> = (0..n).map(|i| 0.4 * (2.0 * PI * 1_000.0 * i as f64 / 44_100.0).sin()).collect();
        for (k, v) in adversarial_pattern(0.7).into_iter().enumerate() {
            x[30_000 + k] = v;
        }
        let mut s = IspStream::new();
        let mut out = 0usize;
        let mut at = 0usize;
        while at < n {
            let c = 1_000.min(n - at);
            s.push(&x[at..at + c], &x[at..at + c], &mut |a, _| out += a.len());
            at += c;
            assert!(out + 64 >= at, "{} handed on of {} in", out, at);
            if at < 29_000 {
                assert_eq!(out, at - 15, "nothing over: 15 samples behind");
            }
        }
        let st = s.stats();
        assert!(st.clusters >= 1 && st.fixed >= 1, "{st:?}");
        s.finish(&mut |a, _| out += a.len());
        assert_eq!(out, n);
    }

    fn out_sine(n: usize, rate: u32, freq: f64, amp: f64) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin())
            .collect()
    }

    /// Three isolated overshoots on a −6 dBFS programme: each is trimmed to
    /// the ceiling, nothing further than an attack-plus-release away from
    /// them changes by a single bit, and the report says so.
    #[test]
    fn output_limiter_trims_only_where_it_is_over() {
        let rate = 352_800u32;
        let n = rate as usize / 2;
        let target = 10f64.powf(-0.5 / 20.0); // −0.5 dBTP
        let mut l = out_sine(n, rate, 440.0, 0.5);
        let mut r = l.clone();
        let spikes = [40_000usize, 90_000, 150_000];
        for &c in &spikes {
            // Push outward from whatever the programme is doing there, so the
            // crest lands between 1.0 and 1.5 whichever half-cycle it hits.
            let sgn = if l[c + 20] >= 0.0 { 1.0 } else { -1.0 };
            for k in 0..40 {
                let w = (std::f64::consts::PI * k as f64 / 40.0).sin();
                l[c + k] += sgn * 1.0 * w;
                r[c + k] += sgn * 0.8 * w;
            }
        }
        let before_l = l.clone();
        let before_r = r.clone();
        let rep = limit_output(&mut l, &mut r, target, rate).expect("there are overs");
        assert!(rep.fell_back.is_none(), "sparse overs must not fall back: {:?}", rep.fell_back);
        assert_eq!(rep.clusters, 3);
        let peak = l.iter().chain(r.iter()).fold(0.0f64, |m, &x| m.max(x.abs()));
        assert!(peak <= target * (1.0 + 1e-9), "peak {:.6} must be at or under {:.6}", peak, target);
        assert!(rep.max_gr_db < -0.5 && rep.max_gr_db > -6.0, "expected a dip of a few dB, got {:.2}", rep.max_gr_db);
        assert!(rep.gr_fraction < 0.03, "three 40-sample events plus their ramps must cover only a few percent of the file, got {:.3}", rep.gr_fraction);
        let guard = ((1.5 / 1000.0) * rate as f64).round() as usize + 2;
        let near = |i: usize| spikes.iter().any(|&c| i + guard >= c && i <= c + 40 + guard);
        for i in 0..n {
            if !near(i) {
                assert!(l[i] == before_l[i] && r[i] == before_r[i], "sample {} away from any over must be bit-identical", i);
            }
        }
    }

    /// A file that is over everywhere is not a case for local correction:
    /// the buffer must come back untouched and the report must say why.
    #[test]
    fn output_limiter_refuses_when_overs_are_not_sparse() {
        let rate = 352_800u32;
        let n = rate as usize / 4;
        let target = 10f64.powf(-0.5 / 20.0);
        let mut l = out_sine(n, rate, 440.0, 1.1);
        let mut r = l.clone();
        let before = l.clone();
        let rep = limit_output(&mut l, &mut r, target, rate).expect("there are overs");
        assert_eq!(rep.fell_back, Some("overs are not sparse"));
        assert!(l == before && r == before);
    }

    /// One over that would need 10 dB is the programme, not an overshoot.
    #[test]
    fn output_limiter_refuses_a_deep_over() {
        let rate = 352_800u32;
        let n = rate as usize / 4;
        let target = 10f64.powf(-0.5 / 20.0);
        let mut l = out_sine(n, rate, 440.0, 0.3);
        let mut r = l.clone();
        for k in 0..20 {
            l[50_000 + k] = 3.0 * (std::f64::consts::PI * k as f64 / 20.0).sin();
        }
        let before = l.clone();
        let rep = limit_output(&mut l, &mut r, target, rate).expect("there is an over");
        assert_eq!(rep.fell_back, Some("a cluster needs more than 6 dB"));
        assert!(l == before);
    }

    /// Scans of separate stretches, joined, find the clusters one scan over
    /// the whole finds — also where a seam cuts a cluster or falls in the gap
    /// between two overs that belong together.
    #[test]
    fn joined_scans_match_one_scan() {
        let rate = 352_800u32;
        let target = 0.5;
        let n = 40_000;
        let mut l = vec![0.0f64; n];
        let r = vec![0.0f64; n];
        // Clusters: a run, two overs 400 samples apart (one cluster at a
        // 706-sample gap), two 900 apart (two clusters), one at the very end.
        for i in 1_000..1_050 { l[i] = 0.8; }
        l[10_000] = 0.9;
        l[10_400] = 0.7;
        l[20_000] = 0.6;
        l[20_900] = 0.6;
        l[n - 1] = 0.75;
        let mut whole = OverScan::new(target, rate);
        whole.push(&l, &r);
        let (want, _) = whole.finish();
        for seams in [vec![1_020], vec![10_200, 20_450], vec![5_000, 10_001, 30_000], vec![10_000, 10_400]] {
            let mut parts = Vec::new();
            let mut at = 0;
            for &e in seams.iter().chain(std::iter::once(&n)) {
                let mut sc = OverScan::new(target, rate);
                sc.push(&l[at..e], &r[at..e]);
                let (c, _) = sc.finish();
                parts.push(c.into_iter().map(|(a, b, p)| (a + at, b + at, p)).collect());
                at = e;
            }
            assert_eq!(join_cluster_runs(parts, rate), want, "seams at {seams:?}");
        }
    }

    /// Run the streaming pair over `l`/`r` in the given chunk sizes and return
    /// what it produced, with the report — the segmented route's way of doing
    /// what `limit_output` does in one call.
    fn stream_limit(
        l: &[f64],
        r: &[f64],
        target: f64,
        rate: u32,
        chunks: &[usize],
    ) -> (Vec<f64>, Vec<f64>, Option<OutputLimitReport>) {
        let n = l.len();
        let mut scan = OverScan::new(target, rate);
        let (mut at, mut k) = (0usize, 0usize);
        while at < n {
            let c = chunks[k % chunks.len()].min(n - at);
            scan.push(&l[at..at + c], &r[at..at + c]);
            at += c;
            k += 1;
        }
        let (clusters, scanned) = scan.finish();
        assert_eq!(scanned, n);
        let (mut ol, mut or) = (l.to_vec(), r.to_vec());
        let Some(plan) = plan_output_limit(clusters, n, target, rate) else {
            return (ol, or, None);
        };
        let mut scratch = Vec::new();
        let (mut at, mut k) = (0usize, 0usize);
        while at < n {
            let c = chunks[(k + 1) % chunks.len()].min(n - at);
            plan.apply(at, &mut ol[at..at + c], &mut or[at..at + c], &mut scratch);
            at += c;
            k += 1;
        }
        (ol, or, Some(plan.report))
    }

    /// The streaming limiter against `limit_output`, bit for bit, on every
    /// case the tests above cover — sparse overs, overs everywhere, one deep
    /// over, nothing over — and with chunk edges falling inside clusters and
    /// inside their dips.
    #[test]
    fn streaming_limiter_matches_limit_output() {
        let rate = 352_800u32;
        let target = 10f64.powf(-0.5 / 20.0);
        let n = rate as usize / 2;
        let mut cases: Vec<(Vec<f64>, Vec<f64>)> = Vec::new();
        // Sparse: many short overs, some closer than the merge gap.
        let mut l = out_sine(n, rate, 440.0, 0.5);
        let mut r = out_sine(n, rate, 440.0, 0.45);
        for (idx, &c) in [3_000usize, 3_500, 40_000, 40_300, 90_000, 150_000, 175_990].iter().enumerate() {
            let sgn = if l[c + 20] >= 0.0 { 1.0 } else { -1.0 };
            for k in 0..40 {
                let w = (PI * k as f64 / 40.0).sin();
                l[c + k] += sgn * (0.8 + 0.1 * idx as f64) * w;
                r[c + k] += sgn * 0.6 * w;
            }
        }
        cases.push((l, r));
        // Over everywhere.
        cases.push((out_sine(n, rate, 440.0, 1.1), out_sine(n, rate, 440.0, 1.1)));
        // One deep over.
        let mut l = out_sine(n, rate, 440.0, 0.3);
        for k in 0..20 {
            l[50_000 + k] = 3.0 * (PI * k as f64 / 20.0).sin();
        }
        let r = out_sine(n, rate, 440.0, 0.3);
        cases.push((l, r));
        // Nothing over.
        cases.push((out_sine(n, rate, 440.0, 0.8), out_sine(n, rate, 440.0, 0.8)));

        for (ci, (l, r)) in cases.iter().enumerate() {
            let (mut bl, mut br) = (l.clone(), r.clone());
            let batch = limit_output(&mut bl, &mut br, target, rate);
            for chunks in [&[n][..], &[1_000usize, 37, 65_536, 511][..]] {
                let (sl, sr, rep) = stream_limit(l, r, target, rate, chunks);
                assert!(sl == bl && sr == br, "case {}: samples differ, chunks {:?}", ci, chunks);
                match (&batch, &rep) {
                    (None, None) => {}
                    (Some(a), Some(b)) => {
                        assert_eq!(a.clusters, b.clusters, "case {}", ci);
                        assert_eq!(a.max_gr_db.to_bits(), b.max_gr_db.to_bits(), "case {}", ci);
                        assert_eq!(a.gr_ms.to_bits(), b.gr_ms.to_bits(), "case {}", ci);
                        assert_eq!(a.gr_fraction.to_bits(), b.gr_fraction.to_bits(), "case {}", ci);
                        assert_eq!(a.fell_back, b.fell_back, "case {}", ci);
                    }
                    _ => panic!("case {}: one reported and the other did not", ci),
                }
            }
        }
    }

    /// Nothing over the ceiling: the stage must say so by returning None and
    /// must not have touched a sample to find out.
    #[test]
    fn output_limiter_is_a_no_op_under_the_ceiling() {
        let rate = 352_800u32;
        let n = rate as usize / 4;
        let target = 10f64.powf(-0.5 / 20.0);
        let mut l = out_sine(n, rate, 440.0, 0.8);
        let mut r = l.clone();
        let before = l.clone();
        assert!(limit_output(&mut l, &mut r, target, rate).is_none());
        assert!(l == before && r == before);
    }

    #[test]
    fn limit_output_always_holds_a_dense_window() {
        // Overs on every other millisecond: limit_output refuses (not
        // sparse), the always-acting variant brings every one down.
        let rate = 88_200u32;
        let n = rate as usize / 4;
        let make = || -> Vec<f64> {
            (0..n).map(|i| {
                let v = 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64).sin();
                if (i / 88) % 2 == 0 && i % 88 == 40 { 1.4 } else { v }
            }).collect()
        };
        let target = 10f64.powf(-0.5 / 20.0);
        let (mut l, mut r) = (make(), make());
        let rep = limit_output(&mut l, &mut r, target, rate).unwrap();
        assert_eq!(rep.fell_back, Some("overs are not sparse"));
        let (mut l, mut r) = (make(), make());
        let rep = limit_output_always(&mut l, &mut r, target, rate).unwrap();
        assert!(rep.fell_back.is_none());
        let peak = l.iter().chain(&r).fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= target + 1e-12, "peak {peak} over the ceiling {target}");
    }

    #[test]
    fn output_limit_prediction_tells_sparse_from_dense() {
        let rate = 44_100u32;
        let n = rate as usize * 4;
        let target = 10f64.powf(-0.5 / 20.0);
        // A quiet tone with a handful of isolated overs: local is honest.
        let mut sparse: Vec<f64> = (0..n)
            .map(|i| 0.3 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin())
            .collect();
        for k in 0..8 { sparse[k * rate as usize / 2 + 1000] = 1.2; }
        assert!(output_limit_is_local(&sparse, &sparse, target, rate));
        // A tone parked at full scale: over everywhere, the whole track must come down.
        let dense: Vec<f64> = (0..n)
            .map(|i| 1.1 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64).sin())
            .collect();
        assert!(!output_limit_is_local(&dense, &dense, target, rate));
        // Nothing over the ceiling: nothing to decide.
        let quiet: Vec<f64> = sparse.iter().map(|v| v * 0.5).collect();
        assert!(output_limit_is_local(&quiet, &quiet, target, rate));
    }

    /// A 16-bit programme made with integers only — what a CD brings to the
    /// repair: triangle waves and noise, intersample overs of every strength
    /// up to the rail (fixable and not, at both ends, close together), and a
    /// stretch clipped flat at the rail. Its samples are codes over 32768, as
    /// the decoder makes them, so none is over full scale.
    fn cd_programme() -> (Vec<f64>, Vec<f64>) {
        let n = 30_000usize;
        let mut seed = 0x0123_4567_89ab_cdefu64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 52) as i64 - 2_048
        };
        let tri = |i: usize, period: usize, amp: i64| -> i64 {
            let (p, h) = ((i % period) as i64, period as i64 / 2);
            let v = if p < h { p } else { period as i64 - p };
            (2 * v - h) * amp / h
        };
        let mut wave = |a: (usize, i64), b: (usize, i64)| -> Vec<i64> {
            (0..n).map(|i| tri(i, a.0, a.1) + tri(i, b.0, b.1) + noise()).collect()
        };
        let mut l = wave((101, 9_000), (37, 4_000));
        let mut r = wave((89, 8_000), (23, 3_000));
        // `adversarial_pattern`'s signs, at codes up to the rail.
        let signs = [-1i64, 1, -1, 1, 1, -1, 1, -1];
        let put = |x: &mut Vec<i64>, at: usize, code: i64| {
            for (k, &s) in signs.iter().enumerate() {
                if at + k < x.len() {
                    x[at + k] = if s > 0 { code } else { -code - 1 };
                }
            }
        };
        for &(at, code) in &[(0, 32_767), (40, 26_000), (52, 30_000), (1_000, 32_767), (4_093, 21_000), (8_189, 25_000),
            (12_000, 32_767), (29_992, 32_767), (29_997, 24_000)] {
            put(&mut l, at, code);
        }
        for &(at, code) in &[(3, 22_000), (5_000, 32_767), (5_011, 28_000), (16_000, 19_500), (29_990, 30_000)] {
            put(&mut r, at, code);
        }
        // A loud triangle cut flat at the rail.
        for (i, x) in l.iter_mut().enumerate().take(22_000).skip(20_000) {
            *x = tri(i, 160, 60_000);
        }
        let to = |x: Vec<i64>| -> Vec<f64> { x.into_iter().map(|c| c.clamp(-32_768, 32_767) as f64 / 32_768.0).collect() };
        (to(l), to(r))
    }

    fn fnv(h: u64, v: u64) -> u64 {
        (h ^ v).wrapping_mul(0x0000_0100_0000_01b3)
    }

    /// What the repair makes of a source with no sample over full scale —
    /// every CD — pinned to its bits as 1.5.0 made them, whole and as a
    /// stream, with declip's spans and without: one hash of the repaired
    /// samples and the reports. The Lanczos coefficients come from the
    /// platform's `sin`, so their hash is pinned beside it: where it differs,
    /// both are to be taken again from 1.5.0's code on that platform.
    #[test]
    fn a_source_within_full_scale_is_repaired_as_in_1_5_0() {
        const COEFFS: u64 = 0xdee1_5438_2176_6219;
        const REPAIRED: u64 = 0x37a3_ea9d_c25b_ba6c;
        let cancel = AtomicBool::new(false);
        let coeffs = lanczos4_poly_coeffs().iter().flatten().fold(0xcbf2_9ce4_8422_2325u64, |h, c| fnv(h, c.to_bits()));
        let (l, r) = cd_programme();
        let (hot_l, hot_r) = (hot_spans(&l, 0), hot_spans(&r, 0));
        assert!(hot_l.is_empty() && hot_r.is_empty(), "the rail, −1.0, is not over full scale");
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let spans: [(&[(usize, usize)], &[(usize, usize)]); 2] = [(&[], &[]), (&[(1_000, 1_010), (20_100, 20_300)], &[(5_005, 5_006)])];
        for (skip_l, skip_r) in spans {
            let (mut bl, mut br) = (l.clone(), r.clone());
            let rep = run(&mut bl, &mut br, skip_l, skip_r, &hot_l, &hot_r, &cancel).expect("the programme has overs");
            assert!(rep.fixed > 0 && rep.unfixed > 0 && rep.hot == 0, "fixed {} unfixed {} hot {}", rep.fixed, rep.unfixed, rep.hot);
            let mut s = IspStream::with_skips(skip_l, skip_r).with_hot(&hot_l, &hot_r);
            let (mut sl, mut sr) = (Vec::new(), Vec::new());
            let (mut at, mut k) = (0usize, 0usize);
            while at < l.len() {
                let c = [1_000usize, 3, 65_536, 7][k % 4].min(l.len() - at);
                s.push(&l[at..at + c], &r[at..at + c], &mut |a, b| {
                    sl.extend_from_slice(a);
                    sr.extend_from_slice(b);
                });
                at += c;
                k += 1;
            }
            let srep = s.finish(&mut |a, b| {
                sl.extend_from_slice(a);
                sr.extend_from_slice(b);
            });
            let srep = srep.expect("the stream reports as run does");
            assert!(same_bits(&sl, &bl) && same_bits(&sr, &br), "the stream differs from run");
            assert_eq!((srep.clusters, srep.fixed, srep.unfixed), (rep.clusters, rep.fixed, rep.unfixed));
            h = bl.iter().chain(&br).fold(h, |h, x| fnv(h, x.to_bits()));
            for v in [rep.clusters as u64, rep.fixed as u64, rep.unfixed as u64, rep.max_dbtp.to_bits(), rep.residual_dbtp.to_bits()] {
                h = fnv(h, v);
            }
        }
        assert_eq!((coeffs, h), (COEFFS, REPAIRED), "GOLDEN coeffs {coeffs:#018x} repaired {h:#018x}");
    }

    /// The runs of samples over full scale, counted from where the piece
    /// starts in the stream; full scale itself is not over, and neither is
    /// any sample an integer source decodes to.
    #[test]
    fn hot_spans_are_the_runs_over_full_scale() {
        let x = [0.5, 1.0, -1.0, 1.000_000_1, 1.2, 0.9, -1.5, -1.0, 2.0, 3.0];
        assert_eq!(hot_spans(&x, 0), vec![(3, 5), (6, 7), (8, 10)]);
        assert_eq!(hot_spans(&x, 1_000), vec![(1_003, 1_005), (1_006, 1_007), (1_008, 1_010)]);
        assert!(hot_spans(&[], 7).is_empty());
        for bits in [16u32, 24, 32] {
            let full = (1i64 << (bits - 1)) as f64;
            let codes = [-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1, -1, 0, 1];
            let x: Vec<f64> = codes.iter().map(|&c| c as f64 / full).collect();
            assert!(hot_spans(&x, 0).is_empty(), "{bits}-bit codes are within full scale");
        }
    }

    /// A cluster's repair reads and writes the samples from 11 before it to
    /// 12 after it: a hot sample there leaves it whole, one further out does
    /// not — on either side, and with the spans and clusters in order.
    #[test]
    fn a_cluster_is_left_whole_when_a_hot_sample_is_within_its_reach() {
        let c = vec![(100usize, 105usize)];
        for (span, left) in [((117, 118), 1), ((118, 119), 0), ((89, 90), 1), ((88, 89), 0), ((50, 95), 1), ((104, 105), 1), ((0, 90), 1), ((0, 89), 0)] {
            let (kept, n) = filter_hot(c.clone(), &[span]);
            assert_eq!((kept.len(), n), (1 - left, left), "span {span:?}");
        }
        let (kept, n) = filter_hot(vec![(10, 12), (40, 45), (100, 105), (300, 310)], &[(20, 30), (118, 200), (322, 323)]);
        assert_eq!((kept, n), (vec![(100, 105)], 3));
        let (kept, n) = filter_hot(vec![(0, 3), (500, 501)], &[]);
        assert_eq!((kept, n), (vec![(0, 3), (500, 501)], 0));
    }

    /// A programme with the source's level over full scale in places — a
    /// stretch of a loud tone and single samples, some within reach of
    /// intersample overs that are within full scale, some not — and overs
    /// between samples elsewhere. With its hot spans, the repair writes no
    /// sample but within the reach of the clusters it repairs, none of which
    /// holds a hot sample: those are as decoded. Without them (1.5.0), it cut
    /// them down.
    #[test]
    fn hot_samples_and_the_clusters_that_reach_them_are_left_whole() {
        let cancel = AtomicBool::new(false);
        let n = 30_000;
        let mut l: Vec<f64> = (0..n).map(|i| 0.3 * (2.0 * PI * 440.0 * i as f64 / 44_100.0).sin()).collect();
        let mut r: Vec<f64> = (0..n).map(|i| 0.25 * (2.0 * PI * 330.0 * i as f64 / 44_100.0).sin()).collect();
        let put = |x: &mut Vec<f64>, at: usize, amp: f64| {
            for (k, v) in adversarial_pattern(amp).into_iter().enumerate() {
                x[at + k] = v;
            }
        };
        for &(at, amp) in &[(1_000, 0.7), (3_000, 0.99), (8_000, 0.62), (8_040, 0.8), (20_000, 0.75), (25_000, 0.66)] {
            put(&mut l, at, amp);
            put(&mut r, at + 500, amp);
        }
        // The level over full scale: a loud tone, and single samples next to
        // overs within full scale (at 8 000 and 20 000: in reach; at 25 000:
        // 40 samples away).
        for i in 12_000..14_000 {
            l[i] = 1.3 * (2.0 * PI * 90.0 * i as f64 / 44_100.0).sin();
        }
        l[8_012] = 1.4;
        r[20_510] = -1.25;
        l[25_048] = 1.5;
        let (hot_l, hot_r) = (hot_spans(&l, 0), hot_spans(&r, 0));
        let (mut nl, mut nr) = (l.clone(), r.clone());
        let rep = run(&mut nl, &mut nr, &[], &[], &hot_l, &hot_r, &cancel).expect("overs");
        assert!(rep.hot >= 3 && rep.clusters >= 8 && rep.fixed > 0, "hot {} clusters {} fixed {}", rep.hot, rep.clusters, rep.fixed);
        for (x, y, hot) in [(&l, &nl, &hot_l), (&r, &nr, &hot_r)] {
            let coeffs = lanczos4_poly_coeffs();
            let (_, exceed) = scan_channel(x, &coeffs);
            let (kept, _) = filter_hot(build_clusters(&exceed), hot);
            let in_reach = |i: usize| kept.iter().any(|&(cs, ce)| i + REACH_BEFORE >= cs && i <= ce + REACH_AFTER);
            for i in 0..n {
                if x[i].to_bits() != y[i].to_bits() {
                    assert!(in_reach(i), "sample {i} changed outside every repaired cluster's reach");
                }
            }
            for &(s, e) in hot.iter() {
                assert!(same_bits(&x[s..e], &y[s..e]), "hot samples {s}..{e} changed");
            }
        }
        // The far single sample's own overs are left whole; the overs within
        // full scale 40 samples before it are repaired.
        assert!(!same_bits(&l[25_000..25_008], &nl[25_000..25_008]), "the overs within full scale are repaired");
        let (mut ol, mut or) = (l.clone(), r.clone());
        let old = run(&mut ol, &mut or, &[], &[], &[], &[], &cancel).expect("overs");
        assert_eq!(old.hot, 0);
        assert!(!same_bits(&l[12_000..14_000], &ol[12_000..14_000]), "without the spans, the loud tone is carved");
    }

    /// `IspStream` with hot spans in the given chunk sizes: given all at once
    /// (a file) or with each piece (a live stream, its spans found on the
    /// piece itself).
    fn stream_isp_hot(l: &[f64], r: &[f64], chunks: &[usize], parallel: bool, up_front: bool) -> (Vec<f64>, Vec<f64>, Option<IspReport>) {
        let mut s = if up_front { IspStream::new().with_hot(&hot_spans(l, 0), &hot_spans(r, 0)) } else { IspStream::new() };
        if parallel {
            s = s.in_parallel();
        }
        let (mut ol, mut or) = (Vec::new(), Vec::new());
        let (mut at, mut k) = (0usize, 0usize);
        while at < l.len() {
            let c = chunks[k % chunks.len()].min(l.len() - at);
            if !up_front {
                s.add_hot(&hot_spans(&l[at..at + c], at), &hot_spans(&r[at..at + c], at));
            }
            s.push(&l[at..at + c], &r[at..at + c], &mut |a, b| {
                ol.extend_from_slice(a);
                or.extend_from_slice(b);
            });
            at += c;
            k += 1;
        }
        let rep = s.finish(&mut |a, b| {
            ol.extend_from_slice(a);
            or.extend_from_slice(b);
        });
        (ol, or, rep)
    }

    /// With the source's samples over full scale, the stream leaves the
    /// clusters that reach them as `run` does, bit for bit and with the same
    /// report — their spans given all at once or a piece at a time, in any
    /// sizes, the channels one after the other or side by side.
    #[test]
    fn streaming_isp_with_hot_samples_matches_run_bit_for_bit() {
        let cancel = AtomicBool::new(false);
        for (ci, (l, r)) in isp_stream_cases().iter().enumerate() {
            let (mut bl, mut br) = (l.clone(), r.clone());
            let batch = run(&mut bl, &mut br, &[], &[], &hot_spans(l, 0), &hot_spans(r, 0), &cancel);
            if ci == 0 {
                let rep = batch.as_ref().expect("the first case has overs");
                assert!(rep.hot > 0 && rep.fixed > 0, "hot {} fixed {}", rep.hot, rep.fixed);
            }
            for (chunks, par) in [(&[l.len()][..], false), (&[1][..], false), (&[37][..], false), (&[4_096][..], true),
                (&[1_000, 3, 65_536, 7][..], false), (&[1_000, 3, 65_536, 7][..], true)] {
                for up_front in [true, false] {
                    let (sl, sr, rep) = stream_isp_hot(l, r, chunks, par, up_front);
                    assert!(same_bits(&sl, &bl) && same_bits(&sr, &br), "case {ci}: samples differ, chunks {chunks:?}, up front {up_front}");
                    match (&batch, &rep) {
                        (None, None) => {}
                        (Some(a), Some(b)) => {
                            assert_eq!((a.clusters, a.fixed, a.unfixed, a.hot), (b.clusters, b.fixed, b.unfixed, b.hot), "case {ci}, chunks {chunks:?}");
                            assert_eq!(a.max_dbtp.to_bits(), b.max_dbtp.to_bits(), "case {ci}, chunks {chunks:?}");
                            assert_eq!(a.residual_dbtp.to_bits(), b.residual_dbtp.to_bits(), "case {ci}, chunks {chunks:?}");
                        }
                        _ => panic!("case {ci}, chunks {chunks:?}: one reported and the other did not"),
                    }
                }
            }
        }
    }

    /// One cluster's repair as `correct_channel` makes it; with
    /// `all_or_nothing`, a cluster that does not converge is put back as it
    /// was. For the measurement below.
    fn repair_cluster(buf: &mut [f64], cs: usize, ce: usize, coeffs: &[[f64; 8]; 3], all_or_nothing: bool) -> bool {
        let n = buf.len();
        let (r_start, r_end) = (cs.saturating_sub(8), (ce + 8).min(n - 1));
        let (lo, hi) = (cs.saturating_sub(REACH_BEFORE), (ce + REACH_AFTER + 1).min(n));
        let keep = if all_or_nothing { buf[lo..hi].to_vec() } else { Vec::new() };
        let mut converged = false;
        for _ in 0..MAX_ITER {
            let (mut w_abs, mut w_val, mut w_pos, mut w_w) = (0.0f64, 0.0f64, r_start, [0.0f64; 8]);
            for pos in r_start..=r_end {
                let (abs, val, w) = eval_phases(buf, pos, coeffs);
                if abs > CEILING && abs > w_abs {
                    (w_abs, w_val, w_pos, w_w) = (abs, val, pos, w);
                }
            }
            if w_abs <= CEILING {
                converged = true;
                break;
            }
            let sum_w2: f64 = w_w.iter().map(|&x| x * x).sum();
            if sum_w2 < 1e-15 {
                break;
            }
            let excess = w_val - CEILING * w_val.signum();
            let mut deltas = [0.0f64; 8];
            for k in 0..8 {
                deltas[k] = -excess * w_w[k] / sum_w2;
            }
            let max_d = deltas.iter().fold(0.0f64, |m, &d| m.max(d.abs()));
            let scale = if max_d > MAX_DELTA { MAX_DELTA / max_d } else { 1.0 };
            for k in 0..8 {
                let idx = (w_pos as i64) + (k as i64) - 3;
                if idx >= 0 && (idx as usize) < n {
                    buf[idx as usize] += deltas[k] * scale;
                }
            }
        }
        if all_or_nothing && !converged {
            buf[lo..hi].copy_from_slice(&keep);
        }
        converged
    }

    /// "All or nothing" on real tracks: what the repair leaves where a
    /// cluster does not converge (cut part of the way: neither the peak
    /// fixed nor the wave whole) against putting such a cluster back as it
    /// was — the samples rewritten, the error to the source as the repair
    /// gets it, the true peak left, and what the output stage would then do
    /// at −0.5 dBTP (hold the overs locally, or take the whole track down).
    /// The track as the player's default rack prepares it. A measurement,
    /// not a test: AURA_ISP_HOT_FILES="a.flac|b.flac" `cargo test --profile
    /// fast --bins all_or_nothing_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn all_or_nothing_harness() {
        use crate::audio::converter::dsp::true_peak::measure_true_peak;
        let files = std::env::var("AURA_ISP_HOT_FILES").unwrap_or_default();
        let target = 10f64.powf(-0.5 / 20.0);
        let s = crate::player::settings::PlayerSettings::default().to_engine();
        let cancel = AtomicBool::new(false);
        let coeffs = lanczos4_poly_coeffs();
        fn rms(x: &[f64], y: &[f64]) -> f64 {
            (x.iter().chain(y).map(|v| v * v).sum::<f64>() / (2 * x.len()).max(1) as f64).sqrt()
        }
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = std::path::Path::new(f.trim());
            let a = match crate::audio::converter::decode::decode_file(path) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("HARNESS {}: cannot decode: {e}", path.display());
                    continue;
                }
            };
            let rate = a.sample_rate;
            let (mut l, mut r) = (a.samples_l, a.samples_r);
            let head = crate::audio::converter::pipeline::prepare::source_head(&mut l, &mut r, rate, a.lossy, &s, &cancel).expect("the head");
            let (mut nl, mut nr) = (l.clone(), r.clone());
            let rep = run(&mut nl, &mut nr, &head.declip_spans_l, &head.declip_spans_r, &head.hot_spans_l, &head.hot_spans_r, &cancel);
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let Some(rep) = rep else {
                eprintln!("HARNESS {name}: nothing over");
                continue;
            };
            let mut outs = Vec::new();
            for aon in [false, true] {
                let (mut ol, mut or) = (l.clone(), r.clone());
                let mut unconverged = 0usize;
                for (x, skip, hot) in [(&mut ol, &head.declip_spans_l, &head.hot_spans_l), (&mut or, &head.declip_spans_r, &head.hot_spans_r)] {
                    let (_, exceed) = scan_channel(x, &coeffs);
                    let (clusters, _) = filter_skipped(build_clusters(&exceed), skip);
                    let (clusters, _) = filter_hot(clusters, hot);
                    for (cs, ce) in clusters {
                        unconverged += !repair_cluster(x, cs, ce, &coeffs, aon) as usize;
                    }
                }
                if !aon {
                    assert!(same_bits(&ol, &nl) && same_bits(&or, &nr), "{name}: the copy of the repair differs from run");
                }
                outs.push((ol, or, unconverged));
            }
            eprintln!(
                "HARNESS {name}: clusters {}, fixed {}, unfixed {} ({:.1} %), hot {}; true peak before the repair {:+.2} dBTP",
                rep.clusters, rep.fixed, rep.unfixed, 100.0 * rep.unfixed as f64 / rep.clusters.max(1) as f64, rep.hot, rep.max_dbtp
            );
            for (label, (ol, or, unconverged)) in ["as it is (unfixed cut part of the way)", "all or nothing (unfixed put back)"].iter().zip(&outs) {
                let changed = ol.iter().chain(or.iter()).zip(l.iter().chain(&r)).filter(|(x, y)| x.to_bits() != y.to_bits()).count();
                let (el, er): (Vec<f64>, Vec<f64>) = (ol.iter().zip(&l).map(|(x, y)| x - y).collect(), or.iter().zip(&r).map(|(x, y)| x - y).collect());
                let tp = measure_true_peak(ol, or);
                let over_db = 20.0 * (tp / target).log10();
                let local = output_limit_is_local(ol, or, target, rate);
                let level = if over_db <= 0.0 || (local && over_db <= 6.0) { 0.0 } else { -over_db };
                eprintln!(
                    "HARNESS   {label}: {unconverged} unconverged; rewritten {} samples ({:.3} %), error to the source {:.1} dB; true peak {:+.2} dBTP; output stage at -0.5 dBTP: {} -> level {level:+.2} dB",
                    changed, 100.0 * changed as f64 / (2 * l.len()) as f64,
                    20.0 * (rms(&el, &er).max(1e-300) / rms(&l, &r)).log10(), 20.0 * tp.log10(),
                    if local { "overs held locally" } else { "the whole track down" }
                );
            }
        }
    }
}
