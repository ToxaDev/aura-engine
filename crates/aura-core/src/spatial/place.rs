//! A source's lasting places (Anton 28.09, the instrument inventory): where
//! in the stereo field a separated source keeps sounding over the whole
//! track, and one object per place — a guitar on the left and one on the
//! right, the bass in the middle. The research's `inventory.py`
//! (`place_objects`); on MDB it found 69 of 88 instruments.
//!
//! The whole track's pan histogram of the source in its band: every
//! sounding frame (within 30 dB of the source's loud frames) counts the
//! same — its energy spread over 40 pan bins by each bin's amplitude pan
//! (R − L)/(R + L). The histogram is smoothed, its peaks are the places,
//! two peaks without a dip of 3 dB between them are one place, a place with
//! less than 8 % of the histogram is dropped, and the places split the pan
//! range at the dips between them. Per place, per frame: the source's
//! energy there (its level, dB against the mix's loud level), its
//! log-spectral flux there (its hits), the centre of its spectrum (its
//! height) and how alike its two channels are (its coherence: dry or
//! diffuse).
//!
//! Frames: hop 512 at 44.1 kHz (`len / 512 + 1`, frame j centred on sample
//! 512·j), a 2048 Hann window (numpy's symmetric one, as the research).

use std::sync::Arc;

use rayon::prelude::*;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::stems::{percentile, BASS, GUITAR, OTHER, PIANO, VOCALS};

/// Pan bins across −1…+1.
pub const NB: usize = 40;
pub(super) const NFFT: usize = 2048;
const HOP: usize = 512;
const BINS: usize = NFFT / 2 + 1;
const SR: f64 = super::core::SR as f64;
pub const FPS: f64 = SR / HOP as f64;
/// A frame sounds within this many dB of the source's loud frames.
pub(super) const ACTIVE_DB: f32 = 30.0;
/// Two peaks are one place without a dip of this many dB between them.
const DIP_DB: f64 = 3.0;
/// A place holds at least this share of the histogram.
const MIN_SHARE: f64 = 0.08;
/// The hits (inventory.py `flux_onsets`): a flux peak over its local mean
/// (0.2 s) + DELTA, the only one within ±GAP, its level within FLOOR_DB of
/// the loud hits.
const ON_DELTA: f64 = 0.06;
const ON_GAP: f64 = 0.05;
pub(super) const ON_FLOOR_DB: f32 = 20.0;
/// The coherence: running spectra over 60 ms, settled over WARM frames
/// before a chunk of frames (as analysis.rs).
pub(super) const COH_TAU: f64 = 0.06;
pub(super) const WARM: usize = 30;
const EPS: f32 = 1e-12;

/// The band a source is looked at in (inventory.py `BAND`).
fn band(stem: usize) -> (f64, f64) {
    match stem {
        BASS => (25.0, 2000.0),
        VOCALS => (80.0, 12000.0),
        GUITAR => (80.0, 10000.0),
        PIANO => (40.0, 10000.0),
        _ => (60.0, 14000.0),
    }
}

/// Its objects' name (inventory.py `NICE`).
pub fn nice(stem: usize) -> &'static str {
    match stem {
        BASS => "bass",
        VOCALS => "voice",
        GUITAR => "guitar",
        PIANO => "piano",
        OTHER => "other",
        _ => "drums",
    }
}

/// One place of a source over the whole track.
pub struct PlaceObj {
    pub stem: usize,
    /// "guitar L", "bass", … (the side when the source has several places)
    pub name: String,
    /// its place (the histogram's centre there) and its pan region, −1…+1
    pub pan: f32,
    pub lo: f32,
    pub hi: f32,
    /// per frame: its level, dB against the mix's loud level
    pub db: Vec<f32>,
    /// its hits, s
    pub onsets: Vec<f32>,
    /// per frame: the ln f centre of its energy (ln Hz; 0 while silent)
    pub lnf: Vec<f32>,
    /// per frame: how alike its channels are, 0…1 (energy-weighted)
    pub coh: Vec<f32>,
}

/// The STFT of a stereo source in its band: numpy's symmetric Hann, the
/// source zero outside, left + i·right in one FFT.
pub(super) struct Spec {
    pub(super) fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    /// the band's bins [k0, k1)
    pub(super) k0: usize,
    k1: usize,
    pub(super) ln_f: Vec<f32>,
}

impl Spec {
    pub(super) fn new(stem: usize) -> Spec {
        let win = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (NFFT - 1) as f64).cos()) as f32)
            .collect();
        // np.fft.rfftfreq(2048, 1 / SR), both ends in
        let df = 1.0 / (NFFT as f64 * (1.0 / SR));
        let (lo, hi) = band(stem);
        let inb: Vec<usize> = (0..BINS).filter(|k| *k as f64 * df >= lo && *k as f64 * df <= hi).collect();
        let ln_f = (0..BINS).map(|k| ((k as f64 * df).max(20.0) as f32).ln()).collect();
        Spec { fft: FftPlanner::<f32>::new().plan_fft_forward(NFFT), win, k0: inb[0], k1: inb[inb.len() - 1] + 1, ln_f }
    }

    pub(super) fn nb(&self) -> usize {
        self.k1 - self.k0
    }

    /// Frame t's left and right spectra over the band.
    fn frame(&self, l: &[f32], r: &[f32], t: usize, buf: &mut [Complex32], scratch: &mut [Complex32], fl: &mut [Complex32], fr: &mut [Complex32]) {
        self.frame_at(l, r, 0, t, buf, scratch, fl, fr)
    }

    /// `frame` of signals whose sample 0 is sample `base` of the frames' grid.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn frame_at(
        &self, l: &[f32], r: &[f32], base: isize, t: usize, buf: &mut [Complex32], scratch: &mut [Complex32], fl: &mut [Complex32],
        fr: &mut [Complex32],
    ) {
        let s = t as isize * HOP as isize - (NFFT / 2) as isize - base;
        let n = l.len() as isize;
        for i in 0..NFFT {
            let j = s + i as isize;
            buf[i] = if j >= 0 && j < n {
                Complex32::new(l[j as usize] * self.win[i], r[j as usize] * self.win[i])
            } else {
                Complex32::new(0.0, 0.0)
            };
        }
        self.fft.process_with_scratch(buf, scratch);
        let half = Complex32::new(0.5, 0.0);
        let mhalf_i = Complex32::new(0.0, -0.5);
        for (o, k) in (self.k0..self.k1).enumerate() {
            let z = buf[k];
            let zc = buf[(NFFT - k) % NFFT].conj();
            fl[o] = (z + zc) * half;
            fr[o] = (z - zc) * mhalf_i;
        }
    }
}

/// A bin's pan bin (inventory.py: `((pan + 1) / 2 · NB)` cut to an integer).
pub(super) fn pan_bin(ml: f32, mr: f32) -> usize {
    let pan = (mr - ml) / (mr + ml + 1e-9);
    (((pan + 1.0) / 2.0 * NB as f32) as isize).clamp(0, NB as isize - 1) as usize
}

/// The frames in chunks, in parallel: `f(frames)` for each chunk, in order.
fn chunks<T: Send>(frames: usize, f: impl Fn(std::ops::Range<usize>) -> T + Sync) -> Vec<T> {
    const CH: usize = 1024;
    (0..frames.div_ceil(CH)).into_par_iter().map(|i| f(i * CH..((i + 1) * CH).min(frames))).collect()
}

/// scipy.ndimage.gaussian_filter1d(x, sigma) (truncate 4, mode "reflect").
fn gaussian(x: &[f64], sigma: f64) -> Vec<f64> {
    let rad = (4.0 * sigma + 0.5) as isize;
    let w: Vec<f64> = (-rad..=rad).map(|i| (-0.5 * (i as f64 / sigma).powi(2)).exp()).collect();
    let sum: f64 = w.iter().sum();
    let n = x.len() as isize;
    let refl = |j: isize| -> usize {
        let mut j = j;
        loop {
            if j < 0 {
                j = -j - 1;
            } else if j >= n {
                j = 2 * n - 1 - j;
            } else {
                return j as usize;
            }
        }
    };
    (0..n).map(|i| (-rad..=rad).map(|k| w[(k + rad) as usize] / sum * x[refl(i + k)]).sum()).collect()
}

/// The first index of the least value (numpy's argmin).
fn argmin(v: &[f64]) -> usize {
    (0..v.len()).fold(0, |b, i| if v[i] < v[b] { i } else { b })
}

fn argmax(v: &[f64]) -> usize {
    (0..v.len()).fold(0, |b, i| if v[i] > v[b] { i } else { b })
}

/// inventory.py `places`: the histogram's places as pan-bin regions [a, b),
/// and the smoothed histogram.
pub(super) fn places(hist: &[f64]) -> (Vec<(usize, usize)>, Vec<f64>) {
    let h = gaussian(hist, 1.0);
    let top = h.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let pk: Vec<usize> = (0..NB)
        .filter(|&i| h[i] > 0.0 && h[i] >= h[i.saturating_sub(1)] && h[i] >= h[(i + 1).min(NB - 1)])
        .filter(|&i| h[i] >= 0.1 * top)
        .collect();
    // neighbours without a deep enough dip between them are one place (the higher peak)
    let mut keep: Vec<usize> = Vec::new();
    for i in pk {
        if let Some(&j) = keep.last() {
            let valley = h[j..=i].iter().cloned().fold(f64::INFINITY, f64::min);
            if valley > h[i].min(h[j]) * 10f64.powf(-DIP_DB / 10.0) {
                if h[i] > h[j] {
                    *keep.last_mut().unwrap() = i;
                }
                continue;
            }
        }
        keep.push(i);
    }
    if keep.is_empty() {
        return (vec![(0, NB)], h);
    }
    let split = |peaks: &[usize]| -> Vec<(usize, usize)> {
        let mut edges = vec![0];
        for w in peaks.windows(2) {
            edges.push(w[0] + argmin(&h[w[0]..=w[1]]));
        }
        edges.push(NB);
        (0..peaks.len()).map(|i| (edges[i], edges[i + 1])).collect()
    };
    let regs = split(&keep);
    let total: f64 = hist.iter().sum();
    let share: Vec<f64> = regs.iter().map(|(a, b)| hist[*a..*b].iter().sum::<f64>() / total).collect();
    let mut kept: Vec<(usize, usize)> = regs.iter().zip(&share).filter(|(_, s)| **s >= MIN_SHARE).map(|(r, _)| *r).collect();
    if kept.is_empty() {
        // the largest share (ties: the later region, as Python's max over (share, region))
        let best = (0..regs.len()).fold(0, |b, i| if (share[i], regs[i]) >= (share[b], regs[b]) { i } else { b });
        kept = vec![regs[best]];
    }
    if kept.len() > 1 {
        let cs: Vec<usize> = kept.iter().map(|(a, b)| a + argmax(&h[*a..*b])).collect();
        kept = split(&cs);
    }
    (kept, h)
}

/// numpy.percentile of a band's values of which `zeros` more are 0 (the
/// bins outside a place), q in 0…100.
fn percentile_with_zeros(vals: &mut [f32], zeros: usize, q: f64) -> f64 {
    let n = vals.len() + zeros;
    if n == 0 {
        return 0.0;
    }
    let pos = q / 100.0 * (n - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    let mut at = |r: usize| -> f64 {
        if r < zeros {
            0.0
        } else {
            let k = r - zeros;
            *vals.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1 as f64
        }
    };
    let (a, b) = (at(lo), at(hi));
    a + (b - a) * (pos - lo as f64)
}

/// The source's places over the whole track, each its object (`stem`: which
/// source, stems::BASS …; `mix_db`: the mix's loud level, `Stems::mix_db`).
pub fn place_objects(l: &[f32], r: &[f32], stem: usize, mix_db: f64) -> Vec<PlaceObj> {
    let len = l.len().min(r.len());
    let (l, r) = (&l[..len], &r[..len]);
    let frames = len / HOP + 1;
    let sp = Spec::new(stem);
    let nb = sp.nb();
    let norm = 4.0 / (NFFT as f32 * NFFT as f32);

    // 1. per frame: its level and its energy per pan bin
    let per: Vec<(Vec<f32>, Vec<f32>)> = chunks(frames, |fs| {
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut scratch = vec![Complex32::new(0.0, 0.0); sp.fft.get_inplace_scratch_len()];
        let (mut fl, mut fr) = (vec![Complex32::new(0.0, 0.0); nb], vec![Complex32::new(0.0, 0.0); nb]);
        let mut lv = Vec::with_capacity(fs.len());
        let mut eb = vec![0f32; fs.len() * NB];
        for (i, t) in fs.enumerate() {
            sp.frame(l, r, t, &mut buf, &mut scratch, &mut fl, &mut fr);
            let mut tot = 0f64;
            for k in 0..nb {
                let (ml, mr) = (fl[k].norm(), fr[k].norm());
                let e = ml * ml + mr * mr;
                eb[i * NB + pan_bin(ml, mr)] += e;
                tot += e as f64;
            }
            lv.push(10.0 * ((tot as f32) * norm + 1e-20).log10());
        }
        (lv, eb)
    });
    let fr_db: Vec<f32> = per.iter().flat_map(|(v, _)| v.iter().cloned()).collect();
    let eb: Vec<f32> = per.into_iter().flat_map(|(_, e)| e).collect();

    // 2. the whole track's histogram over its sounding frames, and its places
    let p99 = percentile(&fr_db.iter().map(|v| *v as f64).collect::<Vec<f64>>(), 99.0) as f32;
    let mut hist = vec![0f64; NB];
    for t in 0..frames {
        if fr_db[t] > p99 - ACTIVE_DB {
            let e = &eb[t * NB..(t + 1) * NB];
            let tot: f32 = e.iter().sum::<f32>() + 1e-20;
            for b in 0..NB {
                hist[b] += (e[b] / tot) as f64;
            }
        }
    }
    let (regs, _) = places(&hist);

    // 3. per place: its level per frame, and the loud end of its magnitudes (for the flux)
    let db: Vec<Vec<f32>> = regs
        .iter()
        .map(|(a, b)| {
            (0..frames)
                .map(|t| {
                    let e: f32 = eb[t * NB + a..t * NB + b].iter().sum();
                    (10.0 * (e * norm + 1e-20).log10() as f64 - mix_db) as f32
                })
                .collect()
        })
        .collect();
    let region_of = |pb: usize| regs.iter().position(|(a, b)| pb >= *a && pb < *b).unwrap_or(0);
    let mags: Vec<Vec<Vec<f32>>> = chunks(frames, |fs| {
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut scratch = vec![Complex32::new(0.0, 0.0); sp.fft.get_inplace_scratch_len()];
        let (mut fl, mut fr) = (vec![Complex32::new(0.0, 0.0); nb], vec![Complex32::new(0.0, 0.0); nb]);
        let mut out = vec![Vec::new(); regs.len()];
        for t in fs {
            sp.frame(l, r, t, &mut buf, &mut scratch, &mut fl, &mut fr);
            for k in 0..nb {
                let (ml, mr) = (fl[k].norm(), fr[k].norm());
                out[region_of(pan_bin(ml, mr))].push((ml * ml + mr * mr).sqrt());
            }
        }
        out
    });
    let top: Vec<f32> = (0..regs.len())
        .into_par_iter()
        .map(|g| {
            let mut v: Vec<f32> = mags.iter().flat_map(|c| c[g].iter().cloned()).collect();
            let zeros = nb * frames - v.len();
            percentile_with_zeros(&mut v, zeros, 99.9) as f32
        })
        .collect();
    drop(mags);

    // 4. per place per frame: its flux, the centre of its spectrum and its coherence
    let a = (1.0 - (-(1.0 / FPS) / COH_TAU).exp()) as f32;
    let per: Vec<Vec<(Vec<f64>, Vec<f32>, Vec<f32>)>> = chunks(frames, |fs| {
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut scratch = vec![Complex32::new(0.0, 0.0); sp.fft.get_inplace_scratch_len()];
        let (mut fl, mut fr) = (vec![Complex32::new(0.0, 0.0); nb], vec![Complex32::new(0.0, 0.0); nb]);
        let (mut c, mut pl, mut pr) = (vec![Complex32::new(0.0, 0.0); nb], vec![0f32; nb], vec![0f32; nb]);
        // per bin: last frame's log magnitude in each place (0 outside it)
        let mut prev = vec![vec![0f32; nb]; regs.len()];
        let mut out: Vec<(Vec<f64>, Vec<f32>, Vec<f32>)> = vec![(Vec::new(), Vec::new(), Vec::new()); regs.len()];
        let from = fs.start.saturating_sub(WARM);
        for t in from..fs.end {
            sp.frame(l, r, t, &mut buf, &mut scratch, &mut fl, &mut fr);
            let mut d = vec![0f64; regs.len()];
            let mut ew = vec![(0f64, 0f64, 0f64); regs.len()];
            for k in 0..nb {
                let (ml, mr) = (fl[k].norm(), fr[k].norm());
                let (el, er) = (ml * ml, mr * mr);
                let x = fl[k] * fr[k].conj();
                if t == from {
                    (c[k], pl[k], pr[k]) = (x, el, er);
                } else {
                    let ck = c[k];
                    c[k] = ck + (x - ck) * a;
                    pl[k] += (el - pl[k]) * a;
                    pr[k] += (er - pr[k]) * a;
                }
                let g = region_of(pan_bin(ml, mr));
                let e = el + er;
                for (gi, p) in prev.iter_mut().enumerate() {
                    let lg = if gi == g { (100.0 * e.sqrt() / (top[gi] + 1e-12)).ln_1p() } else { 0.0 };
                    if t > 0 && t + 1 > fs.start {
                        d[gi] += (lg - p[k]).max(0.0) as f64;
                    }
                    p[k] = lg;
                }
                let coh = c[k].norm() / (pl[k] * pr[k] + EPS).sqrt();
                let coh = if c[k].re < 0.0 { 0.0 } else { coh.min(1.0) };
                ew[g].0 += e as f64;
                ew[g].1 += e as f64 * sp.ln_f[sp.k0 + k] as f64;
                ew[g].2 += e as f64 * coh as f64;
            }
            if t >= fs.start {
                for g in 0..regs.len() {
                    let (e, f, co) = ew[g];
                    out[g].0.push(d[g]);
                    out[g].1.push(if e > 0.0 { (f / e) as f32 } else { 0.0 });
                    out[g].2.push(if e > 0.0 { (co / e) as f32 } else { 0.0 });
                }
            }
        }
        out
    });

    regs.iter()
        .enumerate()
        .map(|(g, &(a, b))| {
            let flux: Vec<f64> = per.iter().flat_map(|c| c[g].0.iter().cloned()).collect();
            let lnf: Vec<f32> = per.iter().flat_map(|c| c[g].1.iter().cloned()).collect();
            let coh: Vec<f32> = per.iter().flat_map(|c| c[g].2.iter().cloned()).collect();
            // its power per frame, dB (inventory.py: of the magnitudes' squares, unscaled)
            let power: Vec<f32> =
                (0..frames).map(|t| 10.0 * (eb[t * NB + a..t * NB + b].iter().sum::<f32>() + 1e-20).log10()).collect();
            let c = (a + b) as f64 / 2.0 / NB as f64 * 2.0 - 1.0;
            let side = if c.abs() < 0.2 { "C" } else if c < 0.0 { "L" } else { "R" };
            let w: f64 = hist[a..b].iter().map(|h| h + 1e-12).sum();
            let pan = (a..b).map(|i| (i as f64 / NB as f64 * 2.0 - 1.0 + 1.0 / NB as f64) * (hist[i] + 1e-12)).sum::<f64>() / w;
            PlaceObj {
                stem,
                name: if regs.len() > 1 { format!("{} {side}", nice(stem)) } else { nice(stem).to_string() },
                pan: pan as f32,
                lo: (a as f64 / NB as f64 * 2.0 - 1.0) as f32,
                hi: (b as f64 / NB as f64 * 2.0 - 1.0) as f32,
                db: db[g].clone(),
                onsets: flux_onsets(&flux, &power),
                lnf,
                coh,
            }
        })
        .collect()
}

/// inventory.py `flux_onsets` on a place's flux and power per frame: a flux
/// peak over its local mean + `ON_DELTA` (the flux against its 99.5th
/// percentile), the only one within ±`ON_GAP`, its level (the most power in
/// the next 5 frames) within `ON_FLOOR_DB` of the 95th percentile of the
/// peaks' levels. Seconds.
fn flux_onsets(flux: &[f64], power: &[f32]) -> Vec<f32> {
    let n = flux.len();
    if n == 0 {
        return Vec::new();
    }
    let top = percentile(flux, 99.5) + 1e-9;
    let cand = onset_candidates(flux, power, top);
    if cand.is_empty() {
        return Vec::new();
    }
    let loud = percentile(&cand.iter().map(|&(_, l)| l as f64).collect::<Vec<f64>>(), 95.0) as f32;
    cand.into_iter().filter(|&(_, l)| l >= loud - ON_FLOOR_DB).map(|(i, _)| (i as f64 / FPS) as f32).collect()
}

/// `flux_onsets`' candidates against the flux's loud end `top` (its 99.5th
/// percentile): each frame and its level (the most power in its next 5).
/// A live stream's place sets `top` and the levels' floor from its song so
/// far.
pub(super) fn onset_candidates(flux: &[f64], power: &[f32], top: f64) -> Vec<(usize, f32)> {
    let n = flux.len();
    let d: Vec<f64> = flux.iter().map(|v| v / top).collect();
    let refl = |j: isize| -> usize {
        let mut j = j;
        loop {
            if j < 0 {
                j = -j - 1;
            } else if j >= n as isize {
                j = 2 * n as isize - 1 - j;
            } else {
                return j as usize;
            }
        }
    };
    let us = (0.2 * FPS) as usize | 1;
    let uh = (us / 2) as isize;
    let loc: Vec<f64> = (0..n as isize).map(|i| (-uh..=uh).map(|k| d[refl(i + k)]).sum::<f64>() / us as f64).collect();
    let ms = (2.0 * ON_GAP * FPS) as usize | 1;
    let mh = (ms / 2) as isize;
    let mx: Vec<f64> = (0..n as isize).map(|i| (-mh..=mh).map(|k| d[refl(i + k)]).fold(f64::NEG_INFINITY, f64::max)).collect();
    // the most power over [i, i + 4] (maximum_filter1d size 5, origin −2)
    let lv: Vec<f32> = (0..n as isize).map(|i| (0..5).map(|k| power[refl(i + k)]).fold(f32::NEG_INFINITY, f32::max)).collect();
    (0..n).filter(|&i| d[i] == mx[i] && d[i] > loc[i] + ON_DELTA).map(|i| (i, lv[i])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bumps(parts: &[(f64, f64, f64)]) -> Vec<f64> {
        (0..NB).map(|i| parts.iter().map(|(c, s, k)| k * (-0.5 * ((i as f64 - c) / s).powi(2)).exp()).sum()).collect()
    }

    /// places_ref.py: inventory.places on made-up histograms, and scipy's gaussian.
    #[test]
    fn places_are_the_researchs() {
        let two = bumps(&[(10.0, 2.0, 1.0), (30.0, 2.5, 0.6)]);
        let (regs, h) = places(&two);
        assert_eq!(regs, vec![(0, 19), (19, 40)]);
        for (i, want) in [(0, 4.5319e-05), (10, 0.894429974), (19, 0.000403617), (30, 0.557087478), (39, 0.002511182)] {
            assert!((h[i] - want).abs() < 2e-9, "h[{i}] {} vs {want}", h[i]);
        }
        assert_eq!(places(&bumps(&[(20.0, 8.0, 1.0)])).0, vec![(0, 40)]);
        // two peaks without a 3 dB dip between them: one place
        assert_eq!(places(&bumps(&[(16.0, 3.0, 1.0), (23.0, 3.0, 0.9)])).0, vec![(0, 40)]);
        // a small third peak (under 8 %) goes; the two others split at the FIRST of two equal dips
        assert_eq!(places(&bumps(&[(8.0, 2.0, 1.0), (32.0, 2.0, 1.0), (20.0, 1.0, 0.12)])).0, vec![(0, 16), (16, 40)]);
        let g = gaussian(&[0.0, 3.0, 1.0, 0.0, 5.0, 2.0, 0.0, 0.0, 4.0, 1.0], 1.0);
        let want = [0.946979861, 1.474658239, 1.404078659, 1.721784311, 2.546482273, 2.030438798, 0.974562561, 1.156985657, 1.918996773, 1.825032869];
        assert!(g.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-8), "{g:?}");
    }

    /// A guitar on the left and another on the right, each strumming its own
    /// rhythm: two places, at their pans, each with its own hits.
    #[test]
    fn a_guitar_left_and_one_right_are_two_places() {
        let len = 44_100 * 12;
        // plucks: a 3 ms rise, a 0.12 s decay
        let pluck = |t: f64, first: f64, every: f64| {
            if t < first {
                return 0.0;
            }
            let dt = (t - first) % every;
            (1.0 - (-dt / 0.003).exp()) * (-dt / 0.12).exp()
        };
        let (mut l, mut r) = (vec![0f32; len], vec![0f32; len]);
        for n in 0..len {
            let t = n as f64 / SR;
            let a = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * pluck(t, 0.5, 0.5) * 0.3;
            let b = (2.0 * std::f64::consts::PI * 660.0 * t).sin() * pluck(t, 0.6, 0.7) * 0.3;
            l[n] = (a + 0.25 * b) as f32;
            r[n] = (0.2 * a + b) as f32;
        }
        let objs = place_objects(&l, &r, GUITAR, -10.0);
        let names: Vec<&str> = objs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["guitar L", "guitar R"]);
        assert!((objs[0].pan + 0.64).abs() < 0.06 && (objs[1].pan - 0.61).abs() < 0.06, "{} {}", objs[0].pan, objs[1].pan);
        // hits: every 0.5 s from 0.5 s on the left, every 0.7 s from 0.6 s on the right
        let left: Vec<f64> = (1..24).map(|k| k as f64 * 0.5).collect();
        let right: Vec<f64> = (0..17).map(|k| 0.6 + k as f64 * 0.7).collect();
        for (o, want) in [(&objs[0], left), (&objs[1], right)] {
            let hit = want.iter().filter(|t| o.onsets.iter().any(|x| (*x as f64 - **t).abs() < 0.03)).count();
            assert!(hit + 1 >= want.len() && o.onsets.len() <= want.len() + 1, "{}: {:?} vs {want:?}", o.name, o.onsets);
            // a pure tone is dry: its channels alike
            let c: Vec<f32> = o.coh.iter().cloned().filter(|c| *c > 0.0).collect();
            assert!(c.iter().sum::<f32>() / c.len() as f32 > 0.95);
            // the centre of its spectrum is its tone
            let f = if o.name.ends_with('L') { 440f32 } else { 660f32 };
            let lnf: Vec<f32> = o.lnf.iter().cloned().filter(|v| *v > 0.0).collect();
            assert!(((lnf.iter().sum::<f32>() / lnf.len() as f32) - f.ln()).abs() < 0.1);
        }
    }

    // ---- against the research's Python (place_refs.py; refs in AURA_KIT_REFS) ----

    fn npy_f32(path: &std::path::Path) -> Vec<f32> {
        let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
        b[10 + hl..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    /// On MDB songs' HTDemucs stems: the same places, pans, levels and hits
    /// as inventory.py.
    #[test]
    #[ignore]
    fn places_on_mdb_are_the_pythons() {
        let Some(d) = std::env::var_os("AURA_KIT_REFS").map(std::path::PathBuf::from) else { return };
        for song in ["Beatles", "80sRock", "Rockabilly", "Shadows", "LatinJazz"] {
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(d.join(format!("place-{song}.json"))).unwrap()).unwrap();
            let len = doc["samples"].as_u64().unwrap() as usize;
            let st = npy_f32(&d.join(format!("place-{song}-stems.npy")));
            let mix = npy_f32(&d.join(format!("place-{song}-mix.npy")));
            let mix_db = super::super::stems::loud_db(&mix[..len], &mix[len..]);
            assert!((mix_db - doc["ref_db"].as_f64().unwrap()).abs() < 1e-6);
            for (k, name) in [(BASS, "bass"), (VOCALS, "vocals"), (GUITAR, "guitar"), (PIANO, "piano"), (OTHER, "other")] {
                let ent = &doc["stems"][name];
                if !ent["passes"].as_bool().unwrap() {
                    continue;
                }
                let (l, r) = (&st[(k * 2) * len..(k * 2 + 1) * len], &st[(k * 2 + 1) * len..(k * 2 + 2) * len]);
                let objs = place_objects(l, r, k, mix_db);
                let want = ent["objects"].as_array().unwrap();
                assert_eq!(objs.len(), want.len(), "{song} {name}: places");
                for (o, w) in objs.iter().zip(want) {
                    let wdb: Vec<f64> = w["db"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
                    assert_eq!(o.db.len(), wdb.len());
                    // within 60 dB of its loudest frame (lower, the float sums' order
                    // shows); a bin right on the edge between two places may fall on
                    // the other side (its pan's last bit): rare frames, small
                    let top = wdb.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let diffs: Vec<f64> =
                        o.db.iter().zip(&wdb).filter(|(_, b)| **b > top - 60.0).map(|(a, b)| (*a as f64 - b).abs()).collect();
                    let e = diffs.iter().cloned().fold(0f64, f64::max);
                    let off = diffs.iter().filter(|x| **x > 1e-3).count();
                    let won: Vec<f64> = w["onsets"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
                    let same = o.onsets.iter().filter(|a| won.iter().any(|b| (**a as f64 - b).abs() < 1e-3)).count();
                    eprintln!(
                        "{song:10} {:9} pan {:+.4} ({:+.4})  levels max diff {e:.1e} dB ({off} of {} frames over 1e-3)  hits {} ({}), {same} the same",
                        o.name, o.pan, w["pan"].as_f64().unwrap(), diffs.len(), o.onsets.len(), won.len()
                    );
                    assert_eq!(o.name, w["name"].as_str().unwrap());
                    assert!((o.pan as f64 - w["pan"].as_f64().unwrap()).abs() < 1e-4);
                    assert!((o.lo as f64 - w["lo"].as_f64().unwrap()).abs() < 1e-6 && (o.hi as f64 - w["hi"].as_f64().unwrap()).abs() < 1e-6);
                    assert!(e < 0.05 && off * 1000 <= diffs.len(), "levels differ by {e} dB in {off} frames");
                    assert!(same + 2 >= won.len() && o.onsets.len() <= won.len() + 2, "{:?} vs {won:?}", o.onsets);
                }
            }
        }
    }
}
