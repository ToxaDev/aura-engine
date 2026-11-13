//! A source's notes → its instruments (the research's A2.1,
//! `notes\A\cluster.py` `instruments_gmm`; on MDB 73 % of the notes land in
//! their instrument's cluster and 20 of 24 instruments are found).
//!
//! The strong notes' portraits, standardised robustly and weighted by what
//! they tell (space, pan, timbre, envelope, level, pitch), are reduced to at
//! most 8 dimensions; a Gaussian mixture (full covariance, 1…6 components by
//! BIC) groups them; then every way of cutting the components in two is
//! tried and a cut is kept only when something a player cannot change tells
//! the two sides apart: the pan (a player does not move), the timbre
//! compared at equal pitch while both play at once, or how diffuse they are
//! while both play at once (one source is not dry and wet at the same
//! moment; a room change alternates) — and the two sides are not confused
//! with each other. Each side is then cut again the same way.

use super::stem::{Portrait, SNote};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Group {
    Space,
    Pan,
    Timbre,
    Envelope,
    Level,
    Pitch,
}

fn group_weight(g: Group) -> f64 {
    match g {
        Group::Space | Group::Pan | Group::Timbre => 1.0,
        Group::Envelope => 0.8,
        Group::Level => 0.4,
        Group::Pitch => 0.3,
    }
}

/// A portrait feature: its group, its weight there, the smallest scale that
/// means something (a robust sd is floored to it), and whether it is part
/// of a player's identity (timbre, attack, vibrato, register — space and
/// dynamics change with the room).
struct Feat(Group, f64, f64, bool);

use Group::*;
const FEATS: [Feat; 27] = [
    Feat(Space, 2.0, 0.05, false),    // dres
    Feat(Space, 0.4, 0.05, false),    // coh
    Feat(Space, 0.4, 2.0, false),     // width_db
    Feat(Pan, 1.0, 0.05, false),      // pan
    Feat(Space, 0.3, 0.05, false),    // tail_coh
    Feat(Timbre, 1.0, 1.0, true),     // tilt
    Feat(Timbre, 0.5, 1.0, true),     // oddeven
    Feat(Timbre, 0.5, 1.0, true),     // h1_rel
    Feat(Timbre, 0.5, 2.0, true),     // env 250
    Feat(Timbre, 0.5, 2.0, true),     // env 500
    Feat(Timbre, 0.7, 2.0, true),     // env 1000
    Feat(Timbre, 0.7, 2.0, true),     // env 2000
    Feat(Timbre, 0.5, 2.0, true),     // env 4000
    Feat(Timbre, 0.3, 2.0, true),     // env 8000
    Feat(Timbre, 0.4, 2.0, true),     // h2
    Feat(Timbre, 0.4, 2.0, true),     // h3
    Feat(Timbre, 0.4, 2.0, true),     // h4
    Feat(Timbre, 0.3, 2.0, true),     // h5
    Feat(Envelope, 1.0, 0.1, false),  // log dur
    Feat(Envelope, 1.0, 2.0, false),  // rise_db
    Feat(Envelope, 0.7, 0.2, true),   // log attack
    Feat(Envelope, 0.5, 2.0, false),  // sustain
    Feat(Envelope, 0.5, 5.0, false),  // decay
    Feat(Envelope, 0.3, 5.0, false),  // tail
    Feat(Envelope, 0.3, 0.2, true),   // vib_c (log1p)
    Feat(Level, 1.0, 2.0, false),     // lvl
    Feat(Pitch, 1.0, 1.0, true),      // pitch
];
const F_DRES: usize = 0;
const F_PAN: usize = 3;
const F_LOG_ATTACK: usize = 20;
const F_PITCH: usize = 26;

/// Feature f of note i (NaN = not measured).
fn value(p: &Portrait, i: usize, f: usize) -> f64 {
    match f {
        0 => p.dres[i],
        1 => p.coh[i],
        2 => p.width_db[i].clamp(-30.0, 10.0),
        3 => p.pan[i],
        4 => p.tail_coh[i],
        5 => p.tilt[i],
        6 => p.oddeven[i],
        7 => p.h1_rel[i],
        8..=13 => p.env[i][f - 8],
        14..=17 => p.h_rel[i][f - 13],
        18 => p.dur[i].max(0.03).ln(),
        19 => p.rise_db[i],
        20 => (p.attack[i] + 0.01).ln(),
        21 => p.sustain[i],
        22 => p.decay[i],
        23 => p.tail[i],
        24 => p.vib_c[i].ln_1p(),
        25 => p.lvl[i],
        _ => p.pitch[i],
    }
}

/// A merge when two groups' notes would be taken for each other this often (DEMP).
const DEMP: f64 = 0.10;
/// Timbre at equal pitch must differ this much (d′) while both play …
const T_ID: f64 = 2.5;
/// … or this much without (reverb alone shifts the measured timbre by ~2.4).
const T_ID_HI: f64 = 4.0;
/// Or the pan alone by this much.
const T_PAN: f64 = 2.5;
/// Or the diffuseness (dres) by this much while both play.
const T_SP: f64 = 1.5;
/// "While both play": the share of the smaller group's sounding time.
const CO_MIN: f64 = 0.5;
const CO_ID: f64 = 0.5;
/// An instrument has ≥ 8 strong notes and ≥ 5 % of the source's sounding time …
const MIN_NOTES: usize = 8;
const MIN_FRAC: f64 = 0.05;
/// … a source whose strong notes sound under 3 s in all has none (bleed).
const MIN_SOUND: f64 = 3.0;
/// Under 40 strong notes a source is one instrument.
const MIN_N: usize = 20;
/// Below C3 the harmonic profile is no timbre (dense harmonics).
const MIN_PITCH_ID: f64 = 48.0;
/// Mixture components at most.
const KMAX: usize = 6;
const N_INIT: usize = 3;

fn nan_median(v: &[f64]) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

fn mean_var(v: impl Iterator<Item = f64> + Clone) -> (f64, f64, usize) {
    let (mut s, mut n) = (0f64, 0usize);
    for x in v.clone() {
        s += x;
        n += 1;
    }
    if n == 0 {
        return (f64::NAN, f64::NAN, 0);
    }
    let m = s / n as f64;
    let var = v.map(|x| (x - m) * (x - m)).sum::<f64>() / n as f64;
    (m, var, n)
}

/// The used notes' portraits, standardised (median, MAD floored) and
/// weighted: `z[note][column]`, and the features kept (measured in ≥ 40 % of
/// the notes).
fn standardised(p: &Portrait, u: &[usize]) -> (Vec<Vec<f64>>, Vec<usize>) {
    let keep: Vec<usize> = (0..FEATS.len())
        .filter(|&f| u.iter().filter(|&&i| value(p, i, f).is_finite()).count() as f64 >= 0.4 * u.len() as f64)
        .collect();
    let mut tot = std::collections::HashMap::new();
    for &f in &keep {
        *tot.entry(FEATS[f].0 as u8).or_insert(0.0) += FEATS[f].1;
    }
    let mut z = vec![vec![0f64; keep.len()]; u.len()];
    for (c, &f) in keep.iter().enumerate() {
        let x: Vec<f64> = u.iter().map(|&i| value(p, i, f)).collect();
        let med = nan_median(&x);
        let dev: Vec<f64> = x.iter().map(|v| (v - med).abs()).collect();
        let mad = nan_median(&dev) * 1.4826;
        let (_, var, _) = mean_var(x.iter().copied().filter(|v| v.is_finite()));
        let sd = var.sqrt();
        let s = if mad > 1e-9 { mad } else if sd > 1e-9 { sd } else { 1.0 };
        let s = s.max(FEATS[f].2);
        let w = (group_weight(FEATS[f].0) * FEATS[f].1 / tot[&(FEATS[f].0 as u8)]).sqrt();
        for (r, v) in x.iter().enumerate() {
            let q = ((v - med) / s).clamp(-6.0, 6.0);
            z[r][c] = if q.is_finite() { q * w } else { 0.0 };
        }
    }
    (z, keep)
}

/// Eigen-decomposition of a symmetric matrix (Jacobi): (values, vectors as
/// columns), largest first.
fn eigen(a: &[Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = a.len();
    let mut a = a.to_vec();
    let mut v = vec![vec![0f64; n]; n];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _sweep in 0..100 {
        let off: f64 = (0..n).flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j))).map(|(i, j)| a[i][j] * a[i][j]).sum();
        let scale: f64 = (0..n).map(|i| a[i][i] * a[i][i]).sum::<f64>().max(1e-300);
        if off <= 1e-24 * scale {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in v.iter_mut() {
                    let (vkp, vkq) = (row[p], row[q]);
                    row[p] = c * vkp - s * vkq;
                    row[q] = s * vkp + c * vkq;
                }
            }
        }
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&x, &y| a[y][y].total_cmp(&a[x][x]));
    let vals = idx.iter().map(|&i| a[i][i]).collect();
    let vecs = idx.iter().map(|&i| (0..n).map(|k| v[k][i]).collect()).collect();
    (vals, vecs)
}

/// The weighted portrait's main directions: the fewest (2…8) that hold 90 %
/// of its variance.
fn embed(z: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let (n, p) = (z.len(), z[0].len());
    let mean: Vec<f64> = (0..p).map(|c| z.iter().map(|r| r[c]).sum::<f64>() / n as f64).collect();
    let zc: Vec<Vec<f64>> = z.iter().map(|r| r.iter().zip(&mean).map(|(a, m)| a - m).collect()).collect();
    let mut c = vec![vec![0f64; p]; p];
    for r in &zc {
        for i in 0..p {
            for j in i..p {
                c[i][j] += r[i] * r[j];
            }
        }
    }
    for i in 0..p {
        for j in 0..i {
            c[i][j] = c[j][i];
        }
    }
    let (vals, vecs) = eigen(&c);
    let tot: f64 = vals.iter().map(|v| v.max(0.0)).sum::<f64>().max(1e-12);
    let mut acc = 0.0;
    let mut d = vals.len();
    for (k, v) in vals.iter().enumerate() {
        acc += v.max(0.0) / tot;
        if acc >= 0.9 {
            d = k + 1;
            break;
        }
    }
    let d = d.clamp(2, 8).min(p);
    zc.iter().map(|r| (0..d).map(|k| r.iter().zip(&vecs[k]).map(|(a, b)| a * b).sum()).collect()).collect()
}

// ---------------------------------------------------------------- the mixture

/// SplitMix64: the mixture's starts, the same every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn d2(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// k-means (k-means++ with local trials, then Lloyd) → labels.
fn kmeans(y: &[Vec<f64>], k: usize, rng: &mut Rng) -> Vec<usize> {
    let n = y.len();
    let dim = y[0].len();
    if k == 1 {
        return vec![0; n];
    }
    let trials = 2 + (k as f64).ln() as usize;
    let mut centers = vec![y[(rng.next() % n as u64) as usize].clone()];
    let mut close: Vec<f64> = y.iter().map(|p| d2(p, &centers[0])).collect();
    let mut pot: f64 = close.iter().sum();
    for _ in 1..k {
        let mut cum = Vec::with_capacity(n);
        let mut s = 0.0;
        for &c in &close {
            s += c;
            cum.push(s);
        }
        let mut best: Option<(f64, usize, Vec<f64>)> = None;
        for _ in 0..trials {
            let r = rng.unit() * pot;
            let id = cum.partition_point(|&c| c < r).min(n - 1);
            let nd: Vec<f64> = y.iter().zip(&close).map(|(p, &c)| c.min(d2(p, &y[id]))).collect();
            let np: f64 = nd.iter().sum();
            if best.as_ref().is_none_or(|b| np < b.0) {
                best = Some((np, id, nd));
            }
        }
        let (np, id, nd) = best.unwrap();
        centers.push(y[id].clone());
        close = nd;
        pot = np;
    }
    let var: f64 = (0..dim)
        .map(|c| {
            let m = y.iter().map(|p| p[c]).sum::<f64>() / n as f64;
            y.iter().map(|p| (p[c] - m) * (p[c] - m)).sum::<f64>() / n as f64
        })
        .sum::<f64>()
        / dim as f64;
    let tol = var * 1e-4;
    let mut lab = vec![usize::MAX; n];
    for _ in 0..300 {
        let new: Vec<usize> = y
            .iter()
            .map(|p| (0..k).min_by(|&a, &b| d2(p, &centers[a]).total_cmp(&d2(p, &centers[b]))).unwrap())
            .collect();
        let same = new == lab;
        lab = new;
        if same {
            break;
        }
        let mut sums = vec![vec![0f64; dim]; k];
        let mut cnt = vec![0usize; k];
        for (p, &l) in y.iter().zip(&lab) {
            cnt[l] += 1;
            for c in 0..dim {
                sums[l][c] += p[c];
            }
        }
        let mut shift = 0.0;
        for j in 0..k {
            let c = if cnt[j] > 0 {
                sums[j].iter().map(|s| s / cnt[j] as f64).collect()
            } else {
                // an empty cluster takes the point farthest from its centre
                let far = (0..n).max_by(|&a, &b| d2(&y[a], &centers[lab[a]]).total_cmp(&d2(&y[b], &centers[lab[b]]))).unwrap();
                y[far].clone()
            };
            shift += d2(&c, &centers[j]);
            centers[j] = c;
        }
        if shift <= tol {
            lab = y
                .iter()
                .map(|p| (0..k).min_by(|&a, &b| d2(p, &centers[a]).total_cmp(&d2(p, &centers[b]))).unwrap())
                .collect();
            break;
        }
    }
    lab
}

/// Lower Cholesky factor of a symmetric positive-definite matrix.
fn cholesky(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut l = vec![vec![0f64; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let s: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();
            if i == j {
                let v = a[i][i] - s;
                if v <= 0.0 {
                    return None;
                }
                l[i][j] = v.sqrt();
            } else {
                l[i][j] = (a[i][j] - s) / l[j][j];
            }
        }
    }
    Some(l)
}

/// A Gaussian mixture with full covariances.
struct Gmm {
    w: Vec<f64>,
    mu: Vec<Vec<f64>>,
    /// Lower Cholesky factors of the covariances.
    chol: Vec<Vec<Vec<f64>>>,
}

const REG: f64 = 1e-3;

impl Gmm {
    /// The M step (sklearn's `_estimate_gaussian_parameters`, reg 1e-3).
    fn from_resp(y: &[Vec<f64>], resp: &[Vec<f64>]) -> Option<Gmm> {
        let (n, k, d) = (y.len(), resp[0].len(), y[0].len());
        let mut w = vec![0f64; k];
        let mut mu = vec![vec![0f64; d]; k];
        let mut chol = Vec::with_capacity(k);
        for j in 0..k {
            let nk: f64 = resp.iter().map(|r| r[j]).sum::<f64>() + 10.0 * f64::EPSILON;
            for (p, r) in y.iter().zip(resp) {
                for c in 0..d {
                    mu[j][c] += r[j] * p[c];
                }
            }
            for c in 0..d {
                mu[j][c] /= nk;
            }
            let mut cov = vec![vec![0f64; d]; d];
            for (p, r) in y.iter().zip(resp) {
                for a in 0..d {
                    let da = p[a] - mu[j][a];
                    for b in 0..=a {
                        cov[a][b] += r[j] * da * (p[b] - mu[j][b]);
                    }
                }
            }
            for a in 0..d {
                for b in 0..=a {
                    cov[a][b] /= nk;
                    cov[b][a] = cov[a][b];
                }
                cov[a][a] += REG;
            }
            chol.push(cholesky(&cov)?);
            w[j] = nk / n as f64;
        }
        Some(Gmm { w, mu, chol })
    }

    /// log p(y | component j) + log w_j, per point and component.
    fn log_joint(&self, y: &[Vec<f64>]) -> Vec<Vec<f64>> {
        let d = y[0].len();
        let c0 = -0.5 * d as f64 * (2.0 * std::f64::consts::PI).ln();
        y.iter()
            .map(|p| {
                (0..self.w.len())
                    .map(|j| {
                        let l = &self.chol[j];
                        // solve L z = p − μ
                        let mut z = vec![0f64; d];
                        for a in 0..d {
                            let s: f64 = (0..a).map(|b| l[a][b] * z[b]).sum();
                            z[a] = (p[a] - self.mu[j][a] - s) / l[a][a];
                        }
                        let maha: f64 = z.iter().map(|v| v * v).sum();
                        let logdet: f64 = (0..d).map(|a| l[a][a].ln()).sum();
                        c0 - 0.5 * maha - logdet + self.w[j].ln()
                    })
                    .collect()
            })
            .collect()
    }

    /// The E step: (mean log-likelihood, responsibilities).
    fn resp(&self, y: &[Vec<f64>]) -> (f64, Vec<Vec<f64>>) {
        let lj = self.log_joint(y);
        let mut ll = 0.0;
        let r = lj
            .iter()
            .map(|row| {
                let m = row.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let s = m + row.iter().map(|v| (v - m).exp()).sum::<f64>().ln();
                ll += s;
                row.iter().map(|v| (v - s).exp()).collect()
            })
            .collect();
        (ll / y.len() as f64, r)
    }

    /// Bayesian information criterion (lower is better).
    fn bic(&self, y: &[Vec<f64>]) -> f64 {
        let (n, k, d) = (y.len() as f64, self.w.len() as f64, y[0].len() as f64);
        let params = k * d * (d + 1.0) / 2.0 + k * d + k - 1.0;
        -2.0 * self.resp(y).0 * n + params * n.ln()
    }
}

/// sklearn's `GaussianMixture(k, "full", reg_covar=1e-3, n_init=3)`: k-means
/// starts, EM to a change of the mean log-likelihood under 1e-3 (at most 100
/// steps); the start that ends highest.
fn fit(y: &[Vec<f64>], k: usize, rng: &mut Rng, n_init: usize) -> Option<Gmm> {
    let mut best: Option<(f64, Gmm)> = None;
    for _ in 0..n_init {
        let lab = kmeans(y, k, rng);
        let resp: Vec<Vec<f64>> = lab.iter().map(|&l| (0..k).map(|j| (j == l) as u8 as f64).collect()).collect();
        let Some(mut g) = Gmm::from_resp(y, &resp) else { continue };
        let mut lb = f64::NEG_INFINITY;
        for _ in 0..100 {
            let (ll, r) = g.resp(y);
            let Some(ng) = Gmm::from_resp(y, &r) else { break };
            g = ng;
            let change = ll - lb;
            lb = ll;
            if change.abs() < 1e-3 {
                break;
            }
        }
        if best.as_ref().is_none_or(|b| lb > b.0) {
            best = Some((lb, g));
        }
    }
    best.map(|b| b.1)
}

// ---------------------------------------------------------------- the split tests

/// d′ of two groups' identity features compared at equal pitch: per
/// 2-semitone bin (from C3) where both have ≥ 3 notes, the difference of
/// their means, pooled; the spread within the bins. No common register → 0
/// (register alone is no evidence).
fn matched_identity(zi: &[Vec<f64>], pitch: &[f64], ia: &[bool], ib: &[bool]) -> f64 {
    let d = zi[0].len();
    if d == 0 {
        return 0.0;
    }
    let bin: Vec<i64> = pitch.iter().map(|p| (p / 2.0).floor() as i64).collect();
    let mut bins: Vec<i64> = bin.iter().zip(pitch).filter(|(_, &p)| p >= MIN_PITCH_ID).map(|(&b, _)| b).collect();
    bins.sort();
    bins.dedup();
    let (mut diffs, mut wts, mut res) = (Vec::new(), Vec::new(), Vec::<Vec<f64>>::new());
    let (mut na, mut nb) = (0, 0);
    let mean = |rows: &[usize]| -> Vec<f64> { (0..d).map(|c| rows.iter().map(|&i| zi[i][c]).sum::<f64>() / rows.len() as f64).collect() };
    for k in bins {
        let a: Vec<usize> = (0..zi.len()).filter(|&i| ia[i] && bin[i] == k).collect();
        let b: Vec<usize> = (0..zi.len()).filter(|&i| ib[i] && bin[i] == k).collect();
        if a.len() < 3 || b.len() < 3 {
            continue;
        }
        let (ma, mb) = (mean(&a), mean(&b));
        diffs.push(ma.iter().zip(&mb).map(|(x, y)| x - y).collect::<Vec<f64>>());
        wts.push(2.0 / (1.0 / a.len() as f64 + 1.0 / b.len() as f64));
        for &i in &a {
            res.push(zi[i].iter().zip(&ma).map(|(x, m)| x - m).collect());
        }
        for &i in &b {
            res.push(zi[i].iter().zip(&mb).map(|(x, m)| x - m).collect());
        }
        na += a.len();
        nb += b.len();
    }
    if diffs.is_empty() || na.min(nb) < 10 {
        return 0.0;
    }
    let ws: f64 = wts.iter().sum();
    let dm: Vec<f64> = (0..d).map(|c| diffs.iter().zip(&wts).map(|(v, w)| v[c] * w).sum::<f64>() / ws).collect();
    let nu = dm.iter().map(|v| v * v).sum::<f64>().sqrt();
    if nu < 1e-12 {
        return 0.0;
    }
    let (_, var, _) = mean_var(res.iter().map(|r| r.iter().zip(&dm).map(|(x, y)| x * y / nu).sum::<f64>()));
    nu / (var + 1e-12).sqrt()
}

/// The share of the smaller group's sounding time (0.1 s grid) the other
/// group sounds too.
fn cooccurrence(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    const STEP: f64 = 0.1;
    let t1 = a.iter().chain(b).map(|x| x.1).fold(f64::NEG_INFINITY, f64::max);
    let g = (t1 / STEP).ceil() as usize + 1;
    let act = |s: &[(f64, f64)]| -> Vec<bool> {
        let mut d = vec![0i64; g + 1];
        for &(on, off) in s {
            d[((on / STEP).floor().max(0.0) as usize).min(g)] += 1;
            d[((off / STEP).ceil().max(0.0) as usize).min(g)] -= 1;
        }
        let mut c = 0;
        (0..g).map(|i| {
            c += d[i];
            c > 0
        }).collect()
    };
    let (x, y) = (act(a), act(b));
    let m = x.iter().filter(|&&v| v).count().min(y.iter().filter(|&&v| v).count());
    if m == 0 {
        return 0.0;
    }
    x.iter().zip(&y).filter(|(p, q)| **p && **q).count() as f64 / m as f64
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// What tells two groups apart: timbre at equal pitch, pan, diffuseness,
/// how much they sound together.
#[derive(Clone, Copy, Debug, Default)]
struct Support {
    d_id: f64,
    d_pan: f64,
    d_sp: f64,
    co: f64,
}

impl Support {
    /// How far past its threshold the strongest supporting cue is (> 1 = a cut).
    fn margin(&self) -> f64 {
        let mut m = (self.d_pan / T_PAN).max(self.d_id / T_ID_HI);
        if self.co >= CO_ID {
            m = m.max(self.d_id / T_ID);
        }
        if self.co >= CO_MIN {
            m = m.max(self.d_sp / T_SP);
        }
        m
    }
}

/// The mixture's starts: the same every run, so a track's instruments are.
const SEED: u64 = 0x5eed_0001;

/// Instrument per note (−1: none) of the notes in `take` (strong, not
/// ghosts) and their ghosts; `split` false: one instrument (a bass).
pub fn instruments(p: &Portrait, notes: &[SNote], take: &[bool], split: bool) -> Vec<i32> {
    instruments_seeded(p, notes, take, split, SEED, N_INIT)
}

/// `instruments` with other starts for the mixture, and more or fewer of
/// them (to see how much the result owes to them).
pub fn instruments_seeded(p: &Portrait, notes: &[SNote], take: &[bool], split: bool, seed: u64, n_init: usize) -> Vec<i32> {
    let n = notes.len();
    let u: Vec<usize> = (0..n).filter(|&i| take[i]).collect();
    let mut lab = vec![-1i32; n];
    let sound: f64 = u.iter().map(|&i| (notes[i].off - notes[i].on).max(0.0)).sum();
    if u.len() < MIN_NOTES || sound < MIN_SOUND {
        return lab;
    }
    if u.len() < 2 * MIN_N || !split {
        for &i in &u {
            lab[i] = 0;
        }
        ghosts_follow(&mut lab, notes);
        return lab;
    }
    let (z, keep) = standardised(p, &u);
    let y = embed(&z);
    let mut rng = Rng(seed);
    let mut best: Option<(f64, Gmm)> = None;
    for k in 1..=KMAX {
        if u.len() < 10 * k {
            break;
        }
        let Some(g) = fit(&y, k, &mut rng, n_init) else { continue };
        let b = g.bic(&y);
        if best.as_ref().is_none_or(|x| b < x.0) {
            best = Some((b, g));
        }
    }
    let Some((_, g)) = best else {
        for &i in &u {
            lab[i] = 0;
        }
        ghosts_follow(&mut lab, notes);
        return lab;
    };
    let k = g.w.len();
    let (_, tau) = g.resp(&y);
    let cl: Vec<usize> = tau.iter().map(|r| (0..k).max_by(|&a, &b| r[a].total_cmp(&r[b])).unwrap()).collect();
    // identity columns for the equal-pitch test: timbre and vibrato, not pitch or attack
    let idm: Vec<usize> = (0..keep.len())
        .filter(|&c| FEATS[keep[c]].3 && keep[c] != F_PITCH && keep[c] != F_LOG_ATTACK)
        .collect();
    let zi: Vec<Vec<f64>> = z.iter().map(|r| idm.iter().map(|&c| r[c]).collect()).collect();
    let pan_col = keep.iter().position(|&f| f == F_PAN);
    let pit: Vec<f64> = u.iter().map(|&i| notes[i].pitch).collect();
    let spans: Vec<(f64, f64)> = u.iter().map(|&i| (notes[i].on, notes[i].off)).collect();
    let dres: Vec<f64> = u.iter().map(|&i| value(p, i, F_DRES)).collect();
    let dur: Vec<f64> = spans.iter().map(|(a, b)| (b - a).max(0.02)).collect();
    let tot_dur: f64 = dur.iter().sum();

    let members = |cs: &[usize]| -> Vec<bool> { cl.iter().map(|c| cs.contains(c)).collect() };
    let small = |cs: &[usize]| -> bool {
        let m = members(cs);
        m.iter().filter(|&&v| v).count() < MIN_NOTES || dur.iter().zip(&m).filter(|(_, &v)| v).map(|(d, _)| d).sum::<f64>() < MIN_FRAC * tot_dur
    };
    // DEMP (Hennig 2010): how often a group's notes would be given to the other
    let confused = |a: &[usize], b: &[usize]| -> f64 {
        let (mut pab, mut pba, mut sa, mut sb) = (0f64, 0f64, 0f64, 0f64);
        for r in &tau {
            let ta: f64 = a.iter().map(|&c| r[c]).sum();
            let tb: f64 = b.iter().map(|&c| r[c]).sum();
            sa += ta;
            sb += tb;
            if tb > ta {
                pab += ta;
            }
            if ta > tb {
                pba += tb;
            }
        }
        (pab / sa.max(1e-12)).max(pba / sb.max(1e-12))
    };
    let support = |ia: &[bool], ib: &[bool]| -> Support {
        let (ca, cb) = (ia.iter().filter(|&&v| v).count(), ib.iter().filter(|&&v| v).count());
        if ca < 3 || cb < 3 {
            return Support::default();
        }
        let d_id = if idm.is_empty() { 0.0 } else { matched_identity(&zi, &pit, ia, ib) };
        let fin = |m: &[bool]| dres.iter().zip(m).filter(|(v, &k)| k && v.is_finite()).map(|(v, _)| *v).collect::<Vec<f64>>();
        let (a, b) = (fin(ia), fin(ib));
        let d_sp = if a.len() < 3 || b.len() < 3 {
            0.0
        } else {
            let (ma, va, _) = mean_var(a.iter().copied());
            let (mb, vb, _) = mean_var(b.iter().copied());
            (ma - mb).abs() / (0.5 * (va + vb) + 0.05 * 0.05).sqrt()
        };
        let d_pan = pan_col.map_or(0.0, |c| {
            let (ma, va, _) = mean_var(z.iter().zip(ia).filter(|(_, &k)| k).map(|(r, _)| r[c]));
            let (mb, vb, _) = mean_var(z.iter().zip(ib).filter(|(_, &k)| k).map(|(r, _)| r[c]));
            (ma - mb).abs() / (0.5 * (va + vb) + 1e-12).sqrt()
        });
        let sa: Vec<(f64, f64)> = spans.iter().zip(ia).filter(|(_, &k)| k).map(|(s, _)| *s).collect();
        let sb: Vec<(f64, f64)> = spans.iter().zip(ib).filter(|(_, &k)| k).map(|(s, _)| *s).collect();
        Support { d_id: round2(d_id), d_pan: round2(d_pan), d_sp: round2(d_sp), co: round2(cooccurrence(&sa, &sb)) }
    };

    // every 2-way cut of the components; the supported one with the largest margin, then each side again
    fn decide(
        cs: Vec<usize>,
        small: &dyn Fn(&[usize]) -> bool,
        confused: &dyn Fn(&[usize], &[usize]) -> f64,
        members: &dyn Fn(&[usize]) -> Vec<bool>,
        support: &dyn Fn(&[bool], &[bool]) -> Support,
    ) -> Vec<Vec<usize>> {
        if cs.len() < 2 {
            return vec![cs];
        }
        let mut best: Option<(f64, Vec<usize>, Vec<usize>)> = None;
        for mask in 1u32..(1 << (cs.len() - 1)) {
            let a: Vec<usize> = cs.iter().enumerate().filter(|(k, _)| mask >> k & 1 == 1).map(|(_, &c)| c).collect();
            let b: Vec<usize> = cs.iter().copied().filter(|c| !a.contains(c)).collect();
            if small(&a) || small(&b) || confused(&a, &b) > DEMP {
                continue;
            }
            let mg = support(&members(&a), &members(&b)).margin();
            if best.as_ref().is_none_or(|x| mg > x.0) {
                best = Some((mg, a, b));
            }
        }
        match best {
            Some((mg, a, b)) if mg >= 1.0 => {
                let mut out = decide(a, small, confused, members, support);
                out.extend(decide(b, small, confused, members, support));
                out
            }
            _ => vec![cs],
        }
    }
    let groups = decide((0..k).collect(), &small, &confused, &members, &support);
    // each note to the group holding most of it; ids by size
    let la: Vec<usize> = tau
        .iter()
        .map(|r| {
            let t: Vec<f64> = groups.iter().map(|g| g.iter().map(|&c| r[c]).sum()).collect();
            (0..t.len()).max_by(|&a, &b| t[a].total_cmp(&t[b]).then(b.cmp(&a))).unwrap()
        })
        .collect();
    let mut count = vec![0usize; groups.len()];
    for &g in &la {
        count[g] += 1;
    }
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&g| std::cmp::Reverse(count[g]));
    let mut rank = vec![0i32; groups.len()];
    for (r, &g) in order.iter().enumerate() {
        rank[g] = r as i32;
    }
    for (j, &i) in u.iter().enumerate() {
        lab[i] = rank[la[j]];
    }
    ghosts_follow(&mut lab, notes);
    lab
}

/// Ghost notes (a harmonic of a stronger note) take their parent's
/// instrument (following ghosts of ghosts up to 8 steps).
pub fn ghosts_follow(lab: &mut [i32], notes: &[SNote]) {
    for i in 0..notes.len() {
        let mut g = notes[i].ghost;
        let mut hops = 0;
        while let Some(x) = g {
            match notes[x].ghost {
                Some(y) if hops < 8 => {
                    g = Some(y);
                    hops += 1;
                }
                _ => break,
            }
        }
        if let Some(x) = g {
            if lab[i] < 0 && !notes[i].weak {
                lab[i] = lab[x];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eigen_of_a_known_matrix() {
        // [[2, 1], [1, 2]]: 3 along (1, 1), 1 along (1, −1)
        let (v, e) = eigen(&[vec![2.0, 1.0], vec![1.0, 2.0]]);
        assert!((v[0] - 3.0).abs() < 1e-12 && (v[1] - 1.0).abs() < 1e-12);
        assert!((e[0][0].abs() - 0.5f64.sqrt()).abs() < 1e-12 && (e[0][0] - e[0][1]).abs() < 1e-12);
    }

    /// Two well apart blobs: BIC picks two components and each blob is one.
    #[test]
    fn a_mixture_finds_two_blobs() {
        let mut rng = Rng(7);
        let mut gauss = || {
            let (u, v) = (rng.unit().max(1e-12), rng.unit());
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        };
        let y: Vec<Vec<f64>> =
            (0..300).map(|i| { let c = if i < 150 { -3.0 } else { 3.0 }; vec![c + gauss(), gauss()] }).collect();
        let mut r = Rng(1);
        let bic: Vec<f64> = (1..=3).map(|k| fit(&y, k, &mut r, N_INIT).unwrap().bic(&y)).collect();
        assert!(bic[1] < bic[0] && bic[1] < bic[2], "{bic:?}");
        let g = fit(&y, 2, &mut r, N_INIT).unwrap();
        let (_, tau) = g.resp(&y);
        let side = |i: usize| (tau[i][0] > 0.5) as u8;
        assert!((0..150).all(|i| side(i) == side(0)) && (150..300).all(|i| side(i) != side(0)));
    }

    #[test]
    fn cooccurrence_is_the_smaller_groups_share() {
        let a = [(0.0, 1.0)];
        let b = [(0.5, 2.5)];
        // a sounds 10 steps, b 20; together 5 steps (0.5 … 1.0)
        assert!((cooccurrence(&a, &b) - 0.5).abs() < 1e-12);
        assert_eq!(cooccurrence(&[(0.0, 1.0)], &[(2.0, 3.0)]), 0.0);
    }

    #[test]
    fn no_common_register_is_no_timbre_evidence() {
        let zi: Vec<Vec<f64>> = (0..40).map(|i| vec![if i < 20 { 0.0 } else { 5.0 }]).collect();
        let pitch: Vec<f64> = (0..40).map(|i| if i < 20 { 60.0 } else { 72.0 }).collect();
        let ia: Vec<bool> = (0..40).map(|i| i < 20).collect();
        let ib: Vec<bool> = ia.iter().map(|v| !v).collect();
        assert_eq!(matched_identity(&zi, &pitch, &ia, &ib), 0.0);
        // the same register: a large difference
        let pitch: Vec<f64> = (0..40).map(|i| 60.0 + (i % 2) as f64 * 0.5).collect();
        let zi: Vec<Vec<f64>> = (0..40).map(|i| vec![if i < 20 { 0.0 } else { 5.0 } + 0.1 * (i % 3) as f64]).collect();
        assert!(matched_identity(&zi, &pitch, &ia, &ib) > 10.0);
    }
}
