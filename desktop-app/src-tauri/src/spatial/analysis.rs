//! What the spatial scenes' objects are made of, per frame (n_fft 2048, hop
//! 512 at 44.1 kHz: 86 frames a second, frame j centred on sample 512·j).
//!
//! Per source and bin: energy, the amplitude pan (R−L)/(R+L) (the tangent
//! law, ≈ linear in the heard angle) and the inter-channel coherence over 60
//! ms — per bin, because spaced microphones turn the phase of L·R* along
//! frequency and a band's sum would cancel.
//!
//! Two kinds of raw values come out, kept for the whole track:
//! - BANDS: one object each — the bass, the kick (the drums' lows), the snare
//!   (the drums' middle near the centre), the hats (the drums' highs) and the
//!   ambience (the mix's diffuse part). Energy, pan, pan spread, ln f centroid,
//!   coherence, anti-phase share, flux.
//! - PAN HISTOGRAMS of the sources that can have parts in several places —
//!   voice, drums (toms and cymbals), guitar, piano, other (and the mix, for
//!   the rough layer): 20 bins across −1…+1, each bin's energy (direct sound
//!   weighted up), ln f centroid and coherence. Anton 27.09: a backing voice
//!   on the right, a second guitar coming in on the right — parts of a
//!   source by where they sit; parts.rs finds and follows them.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub const NFFT: usize = 2048;
pub const HOP: usize = 512;
pub const BINS: usize = NFFT / 2 + 1;
pub const FPS: f64 = super::core::SR as f64 / HOP as f64;
/// Band objects: bass, kick, snare, hats, ambience.
pub const BANDS: usize = 5;
pub const B_BASS: usize = 0;
pub const B_KICK: usize = 1;
pub const B_SNARE: usize = 2;
pub const B_HATS: usize = 3;
pub const B_AMB: usize = 4;
/// Raw per band object: energy, pan, pan spread, ln f centroid, coherence, anti-phase share, flux.
pub const RAW: usize = 7;
/// Pan bins of a histogram.
pub const NB: usize = 20;
const EPS: f32 = 1e-12;
/// Frames run before a region's first, to settle the coherence and the flux.
const WARM: usize = 30;

pub fn pan_centre(b: usize) -> f32 {
    -1.0 + (b as f32 + 0.5) * 2.0 / NB as f32
}

#[derive(Clone, Copy)]
enum Pan {
    All,
    Centre(f32),
}

impl Pan {
    fn w(self, p: f32) -> f32 {
        match self {
            Pan::All => 1.0,
            Pan::Centre(half) => (1.0 - p.abs() / half).clamp(0.0, 1.0),
        }
    }
}

struct BandDef {
    band: usize,
    /// index into the sources (`core::NAMES`: drums, bass, other, vocals, guitar, piano); the rough layer: 0 = the mix
    src: usize,
    lo: f32,
    hi: f32,
    pan: Pan,
}

const FULL_BANDS: &[BandDef] = &[
    BandDef { band: B_BASS, src: 1, lo: 25.0, hi: 2000.0, pan: Pan::All },
    BandDef { band: B_KICK, src: 0, lo: 25.0, hi: 130.0, pan: Pan::All },
    BandDef { band: B_SNARE, src: 0, lo: 150.0, hi: 4000.0, pan: Pan::Centre(0.35) },
    BandDef { band: B_HATS, src: 0, lo: 6000.0, hi: 16000.0, pan: Pan::All },
];
/// From the mix alone: only what a band tells for sure.
const ROUGH_BANDS: &[BandDef] = &[
    BandDef { band: B_BASS, src: 0, lo: 25.0, hi: 150.0, pan: Pan::All },
    BandDef { band: B_HATS, src: 0, lo: 6000.0, hi: 16000.0, pan: Pan::All },
];

/// A source that can have parts in several places, and how its parts are
/// found (parts.rs).
pub struct PartSrc {
    #[allow(dead_code)] // what it is, for reading and the tests
    pub name: &'static str,
    /// index into the sources; the rough layer: 0 = the mix
    pub src: usize,
    pub lo: f32,
    pub hi: f32,
    /// the kind its objects carry (mod.rs K_…)
    pub kind: u8,
    /// its slots: the main part in `slot0`, the extra parts after it
    pub slot0: usize,
    pub extras: usize,
    /// frames in a row a new extra part must be seen to be born
    pub birth: usize,
    /// an extra part is at most this many dB under the source's loudest peak
    pub rel_db: f32,
    /// the histogram's smoothing in time (s), and how slowly the main part
    /// follows its peak (a time constant, s: the ear takes a place over a few
    /// tenths of a second, so a flicker between near peaks — a kit, a
    /// reverberant voice — is no move)
    pub tau: f32,
    pub follow_s: f32,
}

pub const FULL_PARTS: [PartSrc; 5] = [
    PartSrc { name: "voice", src: 3, lo: 80.0, hi: 12000.0, kind: super::K_VOICE, slot0: 0, extras: 3, birth: 4, rel_db: 20.0, tau: 0.06, follow_s: 0.25 },
    PartSrc { name: "drums", src: 0, lo: 130.0, hi: 6000.0, kind: super::K_DRUMS, slot0: 8, extras: 3, birth: 3, rel_db: 18.0, tau: 0.03, follow_s: 0.4 },
    PartSrc { name: "guitar", src: 4, lo: 80.0, hi: 10000.0, kind: super::K_GUITAR, slot0: 12, extras: 3, birth: 8, rel_db: 12.0, tau: 0.15, follow_s: 0.4 },
    PartSrc { name: "piano", src: 5, lo: 40.0, hi: 10000.0, kind: super::K_PIANO, slot0: 16, extras: 1, birth: 6, rel_db: 15.0, tau: 0.15, follow_s: 0.4 },
    PartSrc { name: "other", src: 2, lo: 60.0, hi: 14000.0, kind: super::K_OTHER, slot0: 18, extras: 5, birth: 8, rel_db: 14.0, tau: 0.2, follow_s: 0.4 },
];
pub const ROUGH_PARTS: [PartSrc; 1] = [
    PartSrc { name: "mix", src: 0, lo: 100.0, hi: 8000.0, kind: super::K_MIX, slot0: 25, extras: 6, birth: 6, rel_db: 14.0, tau: 0.12, follow_s: 0.4 },
];

pub struct Stft {
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    pub ln_f: Vec<f32>,
}

impl Stft {
    pub fn new() -> Stft {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(NFFT);
        // periodic Hann (numpy hanning(N + 1)[:-1])
        let win = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos()) as f32)
            .collect();
        let ln_f = (0..BINS)
            .map(|k| ((k as f32 * super::core::SR as f32 / NFFT as f32).max(20.0)).ln())
            .collect();
        Stft { fft, win, ln_f }
    }

    fn band(&self, lo: f32, hi: f32) -> Vec<f32> {
        (0..BINS)
            .map(|k| {
                let f = k as f32 * super::core::SR as f32 / NFFT as f32;
                if f >= lo && f < hi { 1.0 } else { 0.0 }
            })
            .collect()
    }

    /// One channel's spectrum of the frame centred on sample `centre` of `x`;
    /// outside `x`, silence.
    fn frame(&self, x: &[f32], centre: isize, buf: &mut [Complex32]) {
        let start = centre - (NFFT / 2) as isize;
        for i in 0..NFFT {
            let j = start + i as isize;
            let v = if j >= 0 && (j as usize) < x.len() { x[j as usize] } else { 0.0 };
            buf[i] = Complex32::new(v * self.win[i], 0.0);
        }
        self.fft.process(buf);
    }
}

/// Per-bin state of one source: the running cross- and auto-spectra.
struct Coh {
    c: Vec<Complex32>,
    pl: Vec<f32>,
    pr: Vec<f32>,
    started: bool,
}

impl Coh {
    fn new() -> Coh {
        Coh { c: vec![Complex32::new(0.0, 0.0); BINS], pl: vec![0.0; BINS], pr: vec![0.0; BINS], started: false }
    }
}

/// A source's per-bin values of one frame.
struct Bins {
    e: Vec<f32>,
    pa: Vec<f32>,
    coh: Vec<f32>,
}

impl Bins {
    fn new() -> Bins {
        Bins { e: vec![0.0; BINS], pa: vec![0.0; BINS], coh: vec![0.0; BINS] }
    }
}

fn bins(st: &Stft, l: &[f32], r: &[f32], centre: isize, cs: &mut Coh, bl: &mut [Complex32], br: &mut [Complex32], out: &mut Bins) {
    st.frame(l, centre, bl);
    st.frame(r, centre, br);
    let a = 1.0 - (-(1.0 / FPS) / 0.06).exp() as f32;
    for k in 0..BINS {
        let (lk, rk) = (bl[k], br[k]);
        let (al2, ar2) = (lk.norm_sqr(), rk.norm_sqr());
        let (al, ar) = (al2.sqrt(), ar2.sqrt());
        out.e[k] = al2 + ar2;
        out.pa[k] = (ar - al) / (ar + al + EPS);
        let x = lk * rk.conj();
        if cs.started {
            let c = cs.c[k];
            cs.c[k] = c + (x - c) * a;
            cs.pl[k] += (al2 - cs.pl[k]) * a;
            cs.pr[k] += (ar2 - cs.pr[k]) * a;
        } else {
            cs.c[k] = x;
            cs.pl[k] = al2;
            cs.pr[k] = ar2;
        }
        let coh = cs.c[k].norm() / (cs.pl[k] * cs.pr[k] + EPS).sqrt();
        out.coh[k] = if cs.c[k].re < 0.0 { -coh } else { coh };
    }
    cs.started = true;
}

/// How a band object weighs each bin.
#[derive(Clone, Copy, PartialEq)]
enum Weight {
    /// its energy
    Plain,
    /// its diffuse share squared (the mix's ambience)
    Ambience,
    /// its incoherent share (1 − coherence², the rest of what `focused`
    /// takes), where the bin is not one-sided: a sound panned far to one
    /// side is unalike in its two channels too (the other carries other
    /// sounds), yet it is a place, not a width — the parts find it
    Wide,
}

/// A band object's raw values of one frame; `prev` = its per-bin log
/// magnitude of the frame before (for the flux), updated.
fn measure(st: &Stft, b: &Bins, band: &[f32], pan: Pan, weight: Weight, prev: &mut [f32], first: bool, out: &mut [f32]) {
    let (mut e_sum, mut x_sum, mut xx_sum, mut f_sum, mut c_sum, mut anti, mut flux) = (0f32, 0f32, 0f32, 0f32, 0f32, 0f32, 0f32);
    for k in 0..BINS {
        let mut w = b.e[k] * band[k];
        if w > 0.0 {
            w *= pan.w(b.pa[k]);
            match weight {
                Weight::Plain => {}
                Weight::Ambience => {
                    let dd = (1.0 - b.coh[k].abs()).clamp(0.0, 1.0);
                    w *= dd * dd;
                }
                Weight::Wide => {
                    let c = b.coh[k].max(0.0);
                    w *= (1.0 - c * c) * ((0.85 - b.pa[k].abs()) / 0.3).clamp(0.0, 1.0);
                }
            }
        }
        e_sum += w;
        x_sum += w * b.pa[k];
        xx_sum += w * b.pa[k] * b.pa[k];
        f_sum += w * st.ln_f[k];
        c_sum += w * b.coh[k].max(0.0);
        if b.coh[k] < -0.5 {
            anti += w;
        }
        let lm = (1.0 + 100.0 * w.sqrt()).ln();
        if !first {
            flux += (lm - prev[k]).max(0.0);
        }
        prev[k] = lm;
    }
    let e = e_sum + EPS;
    let x = x_sum / e;
    out[0] = e_sum;
    out[1] = x;
    out[2] = (xx_sum / e - x * x).max(0.0).sqrt();
    out[3] = f_sum / e;
    out[4] = c_sum / e;
    out[5] = anti / e;
    out[6] = flux;
}

/// A source's energy by pan, one frame: each bin split between the two
/// nearest pan bins; direct sound (coherent) weighted up so a reverb's wash
/// does not make peaks. Per pan bin: energy, ln f centroid, coherence.
fn histogram(st: &Stft, b: &Bins, band: &[f32], e: &mut [f32], f: &mut [f32], c: &mut [f32]) {
    e.fill(0.0);
    f.fill(0.0);
    c.fill(0.0);
    for k in 0..BINS {
        if band[k] == 0.0 || b.e[k] <= 0.0 {
            continue;
        }
        let coh = b.coh[k].max(0.0);
        let w = b.e[k] * (0.3 + 0.7 * coh);
        let pos = (b.pa[k] + 1.0) * 0.5 * NB as f32 - 0.5;
        let fl = pos.floor();
        let fr = (pos - fl).clamp(0.0, 1.0);
        let b0 = (fl as isize).clamp(0, NB as isize - 1) as usize;
        let b1 = (b0 + 1).min(NB - 1);
        for (bb, ww) in [(b0, w * (1.0 - fr)), (b1, w * fr)] {
            e[bb] += ww;
            f[bb] += ww * st.ln_f[k];
            c[bb] += ww * coh;
        }
    }
    for bb in 0..NB {
        if e[bb] > 0.0 {
            f[bb] /= e[bb];
            c[bb] /= e[bb];
        }
    }
}

/// The separated sources that have a WIDE layer (Anton 28.09, Angelux -
/// Ocean Wind: a second synth far behind, wide in the stereo, sat in the same
/// stem as the lead, at the same place — both at the centre — so the parts
/// could not tell them apart). A source's wide layer is its diffuse part —
/// the channels unalike (a pad spread across, a string wash, a big reverb) —
/// against its focused part (the channels alike): `FULL_PARTS` indices.
pub const WIDE: [usize; 4] = [0, 2, 3, 4];
/// Raw per wide layer: the band object's RAW of its diffuse part, then the
/// source's focused energy in the same band.
pub const WRAW: usize = RAW + 1;

/// The energy of a source's focused part in a band: each bin weighted by its
/// coherence squared.
fn focused(b: &Bins, band: &[f32]) -> f32 {
    let mut e = 0f32;
    for k in 0..BINS {
        if band[k] > 0.0 && b.e[k] > 0.0 {
            let c = b.coh[k].max(0.0);
            e += b.e[k] * band[k] * c * c;
        }
    }
    e
}

/// A region's raw values: band objects `[n][BANDS][RAW]` (the ones the layer
/// has; the rest 0), the histograms `[n][P][NB]` (energy, ln f, coherence)
/// and, in the full layer, the wide layers `[n][WIDE][WRAW]`.
pub struct Region {
    pub band: Vec<f32>,
    pub he: Vec<f32>,
    pub hf: Vec<f32>,
    pub hc: Vec<f32>,
    pub wide: Vec<f32>,
}

/// The raw values of `frames` (track frame indices, in order), from stereo
/// signals whose sample 0 is track sample `base`. `full`: `sources` are the
/// six separated ones and the full layer is made (no ambience: it comes from
/// the mix, in the rough layer); else `sources` is the mix alone and the rough
/// layer is made (its bands, the ambience, the mix's histogram). The first
/// `WARM` frames before `frames` settle the state and are not kept.
pub fn region(st: &Stft, sources: &[(&[f32], &[f32])], full: bool, base: isize, frames: std::ops::Range<usize>) -> Region {
    let n = frames.len();
    let bdefs = if full { FULL_BANDS } else { ROUGH_BANDS };
    let pdefs: &[PartSrc] = if full { &FULL_PARTS } else { &ROUGH_PARTS };
    let np = pdefs.len();
    let bbands: Vec<Vec<f32>> = bdefs.iter().map(|d| st.band(d.lo, d.hi)).collect();
    let pbands: Vec<Vec<f32>> = pdefs.iter().map(|d| st.band(d.lo, d.hi)).collect();
    let amb_band = st.band(60.0, 16000.0);
    let nsrc = sources.len();
    let mut coh: Vec<Coh> = (0..nsrc).map(|_| Coh::new()).collect();
    let mut prev = vec![vec![0f32; BINS]; bdefs.len() + 1];
    let mut prev_w = vec![vec![0f32; BINS]; WIDE.len()];
    let mut b: Vec<Bins> = (0..nsrc).map(|_| Bins::new()).collect();
    let mut bl = vec![Complex32::new(0.0, 0.0); NFFT];
    let mut br = vec![Complex32::new(0.0, 0.0); NFFT];
    let mut out = Region {
        band: vec![0.0; n * BANDS * RAW],
        he: vec![0.0; n * np * NB],
        hf: vec![0.0; n * np * NB],
        hc: vec![0.0; n * np * NB],
        wide: vec![0.0; if full { n * WIDE.len() * WRAW } else { 0 }],
    };
    let (mut e, mut f, mut c) = ([0f32; NB], [0f32; NB], [0f32; NB]);
    let from = frames.start.saturating_sub(WARM);
    let mut tmp = [0f32; RAW];
    for (i, j) in (from..frames.end).enumerate() {
        let centre = (j * HOP) as isize - base;
        let keep = j >= frames.start;
        let row = j.saturating_sub(frames.start);
        for (s, (l, r)) in sources.iter().enumerate() {
            bins(st, l, r, centre, &mut coh[s], &mut bl, &mut br, &mut b[s]);
        }
        for (di, def) in bdefs.iter().enumerate() {
            measure(st, &b[def.src], &bbands[di], def.pan, Weight::Plain, &mut prev[di], i == 0, &mut tmp);
            if keep {
                let o = (row * BANDS + def.band) * RAW;
                out.band[o..o + RAW].copy_from_slice(&tmp);
            }
        }
        if full {
            for (w, &p) in WIDE.iter().enumerate() {
                let def = &pdefs[p];
                measure(st, &b[def.src], &pbands[p], Pan::All, Weight::Wide, &mut prev_w[w], i == 0, &mut tmp);
                if keep {
                    let o = (row * WIDE.len() + w) * WRAW;
                    out.wide[o..o + RAW].copy_from_slice(&tmp);
                    out.wide[o + RAW] = focused(&b[def.src], &pbands[p]);
                }
            }
        }
        if !full {
            let pi = bdefs.len();
            measure(st, &b[0], &amb_band, Pan::All, Weight::Ambience, &mut prev[pi], i == 0, &mut tmp);
            if keep {
                let o = (row * BANDS + B_AMB) * RAW;
                out.band[o..o + RAW].copy_from_slice(&tmp);
            }
        }
        if keep {
            for (p, def) in pdefs.iter().enumerate() {
                histogram(st, &b[def.src], &pbands[p], &mut e, &mut f, &mut c);
                let o = (row * np + p) * NB;
                out.he[o..o + NB].copy_from_slice(&e);
                out.hf[o..o + NB].copy_from_slice(&f);
                out.hc[o..o + NB].copy_from_slice(&c);
            }
        }
    }
    out
}

/// Energy of a frame of the mix (both channels, all bins) — for the track's
/// loudness reference.
pub fn mix_energy(st: &Stft, l: &[f32], r: &[f32], centre: isize, bl: &mut [Complex32]) -> f32 {
    let mut e = 0f32;
    for x in [l, r] {
        st.frame(x, centre, bl);
        e += bl[..BINS].iter().map(|c| c.norm_sqr()).sum::<f32>();
    }
    e
}

/// Zero-lag smoothing: a one-pole forward, then backward; `gate` (0…1) slows
/// it where the object is silent, so its place holds instead of wandering.
pub fn smooth(v: &mut [f32], tau: f32, gate: &[f32]) {
    let a0 = 1.0 - (-(1.0 / FPS as f32) / tau).exp();
    for i in 1..v.len() {
        v[i] = v[i - 1] + a0 * gate[i] * (v[i] - v[i - 1]);
    }
    for i in (0..v.len().saturating_sub(1)).rev() {
        v[i] = v[i + 1] + a0 * gate[i] * (v[i] - v[i + 1]);
    }
}

/// log f (ln Hz) → height 0…1 (40 Hz … 12 kHz).
pub fn height(ln_f: f32) -> f32 {
    let ln40 = 40f32.ln();
    ((ln_f - ln40) / (12000f32.ln() - ln40)).clamp(0.0, 1.0)
}

/// Depth 0 near … 1 far: diffuse (low coherence) and quiet (dB under the mix's loud level) is far.
pub fn depth(coh: f32, db: f32) -> f32 {
    (0.12 + 0.5 * (1.0 - coh) + 0.38 * (-db / 40.0).clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

/// A band object as a scene sees it, over `n` frames of raw values `[n][RAW]`
/// (energy 0 where nothing is known). Out: per frame x, y, z, energy, width,
/// onset, presence, coherence. It is there (presence) when it is within 44 dB
/// of the mix's loud level AND within 24 dB of its own loud level (`p90`, per
/// frame: the layer it came from) — so a stem's leakage while its instrument
/// rests does not keep it lit (Anton 27.09).
pub fn finish_band(raw: &[f32], n: usize, ref_db: f32, p90: &[f32], ambience: bool) -> Vec<[f32; 8]> {
    let mut out = vec![[0f32; 8]; n];
    let g = |i: usize, k: usize| raw[i * RAW + k];
    if !(0..n).any(|i| g(i, 0) > 0.0) {
        return out;
    }
    let dt = 1.0 / FPS as f32;
    let rel = (-dt / 0.1).exp();
    let dec = (-dt / 0.12).exp();
    let half = (FPS / 2.0) as usize;
    let db: Vec<f32> = (0..n).map(|i| 10.0 * (g(i, 0) + EPS).log10() - ref_db).collect();
    let one = vec![1f32; n];
    let mut presence: Vec<f32> = db
        .iter()
        .zip(p90)
        .map(|(d, p)| ((d + 44.0) / 14.0).clamp(0.0, 1.0).min(((d - (p - 24.0)) / 8.0).clamp(0.0, 1.0)))
        .collect();
    smooth(&mut presence, 0.2, &one);
    let gate: Vec<f32> = presence.iter().map(|p| p.clamp(0.02, 1.0)).collect();
    let mut x: Vec<f32> = (0..n).map(|i| g(i, 1)).collect();
    let mut coh: Vec<f32> = (0..n).map(|i| if g(i, 5) > 0.5 { 0.0 } else { g(i, 4) }).collect();
    let mut y: Vec<f32> = (0..n).map(|i| height(g(i, 3))).collect();
    smooth(&mut x, 0.08, &gate);
    smooth(&mut coh, 0.1, &gate);
    smooth(&mut y, 0.08, &gate);
    let mut z: Vec<f32> = (0..n).map(|i| depth(coh[i], db[i])).collect();
    smooth(&mut z, 0.15, &gate);
    let mut width: Vec<f32> = (0..n).map(|i| (1.5 * g(i, 2)).max(1.0 - coh[i]).clamp(0.0, 1.0)).collect();
    smooth(&mut width, 0.1, &gate);
    let flux: Vec<f32> = (0..n).map(|i| g(i, 6)).collect();
    let mut pre = vec![0f64; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + flux[i] as f64;
    }
    let (mut env, mut en) = (0f32, 0f32);
    for i in 0..n {
        let (a, b) = (i.saturating_sub(half), (i + half + 1).min(n));
        let base = ((pre[b] - pre[a]) / (b - a) as f64) as f32;
        let o = ((flux[i] / (base + 1e-6) - 1.4) / 2.5).clamp(0.0, 1.0) * (presence[i] * 2.0).min(1.0);
        env = o.max(env * dec);
        en = (((db[i] + 45.0) / 45.0).clamp(0.0, 1.0) * presence[i]).max(en * rel);
        out[i] = [x[i].clamp(-1.0, 1.0), y[i], z[i], en, width[i], env, presence[i], coh[i]];
        if ambience {
            out[i][0] = 0.0;
            out[i][2] = (0.75 + 0.25 * (1.0 - coh[i])).clamp(0.0, 1.0);
            out[i][4] = 1.0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone panned left in "other" makes a histogram peak left of centre,
    /// at the pan its levels give, near and coherent.
    #[test]
    fn a_left_tone_peaks_left_in_its_histogram() {
        let st = Stft::new();
        let n = 44100 * 2;
        let tone: Vec<f32> = (0..n).map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 44100.0).sin() * 0.3).collect();
        let l: Vec<f32> = tone.clone();
        let r: Vec<f32> = tone.iter().map(|v| v * 0.25).collect();
        let z = vec![0f32; n];
        let srcs: Vec<(&[f32], &[f32])> = vec![(&z, &z), (&z, &z), (&l, &r), (&z, &z), (&z, &z), (&z, &z)];
        let frames = 10..150;
        let reg = region(&st, &srcs, true, 0, frames.clone());
        let np = FULL_PARTS.len();
        let other = FULL_PARTS.iter().position(|p| p.name == "other").unwrap();
        let o = (70 * np + other) * NB;
        let h = &reg.he[o..o + NB];
        let top = (0..NB).max_by(|a, b| h[*a].partial_cmp(&h[*b]).unwrap()).unwrap();
        // (0.25 − 1)/(1.25) = −0.6
        assert!((pan_centre(top) + 0.6).abs() <= 0.1, "peak at {}", pan_centre(top));
        assert!(reg.hc[o + top] > 0.9, "coh {}", reg.hc[o + top]);
        assert!((height(reg.hf[o + top]) - 0.42).abs() < 0.05, "y {}", height(reg.hf[o + top]));
        // the voice's histogram is empty
        let v = FULL_PARTS.iter().position(|p| p.name == "voice").unwrap();
        let ov = (70 * np + v) * NB;
        assert!(reg.he[ov..ov + NB].iter().all(|e| *e == 0.0));
    }
}
