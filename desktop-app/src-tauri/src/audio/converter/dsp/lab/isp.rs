use std::sync::atomic::{AtomicBool, Ordering};

pub struct IspReport {
    pub max_dbtp: f64,
    pub clusters: usize,
    pub fixed: usize,
    pub unfixed: usize,
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

/// Intersample peak scan and correction. Returns None on cancel or no exceedance.
/// `skip_l`/`skip_r`: sample spans repaired by declip — their reconstructed
/// peaks intentionally exceed the old rail and must not be squashed back;
/// the output true-peak stage owns level safety for them.
pub fn run(
    l: &mut [f64],
    r: &mut [f64],
    skip_l: &[(usize, usize)],
    skip_r: &[(usize, usize)],
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

    if clusters_l.is_empty() && clusters_r.is_empty() {
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
    if max_gr < MAX_LOCAL_GR {
        report.fell_back = Some("a cluster needs more than 6 dB");
        return Some(report);
    }
    if fraction > MAX_GR_FRACTION {
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

        let report = run(&mut l, &mut r, &[], &[], &cancel);
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

        let report = run(&mut l, &mut r, &[], &[], &cancel);
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
}
