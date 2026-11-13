//! What the song's map is made of on a stream, frame by frame, from the
//! separated sources: the drums' bands (kick, snare, hats — the kit without
//! the drum network, as a file's `kit_bands_of`) and, for every source a
//! place can be found for (bass, voice, guitar, piano, other), its energy by
//! pan with its pitch centre, its coherence and its hits' flux (as
//! `place::place_objects` measures them) — kept per pan bin, so a place
//! found later, or moved, is measured over any frames still held.
//!
//! Every run of frames starts from the frames before it (the coherence and
//! the flux settle over them), as a file's chunks do: nothing is carried
//! from one run to the next, so a run can be made again (a segment's last
//! quarter, made whole by the next segment).

use std::collections::VecDeque;

use rustfft::num_complex::Complex32;

use super::super::analysis::{self, BANDS, B_HATS, B_KICK, B_SNARE, HOP, RAW};
use super::super::core::SOURCES;
use super::super::place::{self, pan_bin, Spec, COH_TAU, WARM};
use super::super::stems;
use super::pct::Pct;
use super::sep::Sep;

/// Pan bins of a place's histogram.
pub const PNB: usize = place::NB;
/// The sources places are found for, in the networks' order of index.
pub const PLACED: [usize; 5] = [stems::BASS, stems::VOCALS, stems::GUITAR, stems::PIANO, stems::OTHER];
/// The kit's bands: kick, snare, hats.
pub const KIT: [usize; 3] = [B_KICK, B_SNARE, B_HATS];
const NFFT: usize = place::NFFT;
const EPS: f32 = 1e-12;

/// A source's frame: per pan bin its energy, energy × ln f, energy ×
/// coherence and flux; the frame's level in its band (dB, scaled as
/// `place_objects`' `fr_db`) and its mean power (dB, `stems::frame_db`).
#[derive(Clone, Copy)]
pub struct SrcF {
    pub e: [f32; PNB],
    pub ef: [f32; PNB],
    pub ec: [f32; PNB],
    pub fl: [f32; PNB],
    pub lv: f32,
    pub db: f32,
}

impl Default for SrcF {
    fn default() -> SrcF {
        SrcF { e: [0.0; PNB], ef: [0.0; PNB], ec: [0.0; PNB], fl: [0.0; PNB], lv: -200.0, db: -200.0 }
    }
}

/// A session frame's values. `ok`: made from the separated sources (else a
/// gap: a segment skipped).
#[derive(Clone, Copy)]
pub struct Frame {
    pub ok: bool,
    pub kit: [[f32; RAW]; 3],
    pub src: [SrcF; 5],
    /// The mix's mean power (dB): what the sources' levels are against.
    pub mix_db: f32,
}

impl Default for Frame {
    fn default() -> Frame {
        Frame { ok: false, kit: [[0.0; RAW]; 3], src: [SrcF::default(); 5], mix_db: -200.0 }
    }
}

/// The frames held: session frame `base` first.
pub struct Feats {
    base: usize,
    v: VecDeque<Frame>,
    cap: usize,
    /// Per placed source: its bins' magnitudes (dB) so far — their 99.9th
    /// percentile scales its flux (`place_objects`' `top`).
    mags: [Pct; 5],
    specs: Vec<Spec>,
}

impl Feats {
    /// Holding the last `secs` seconds of frames.
    pub fn new(secs: f64) -> Feats {
        Feats {
            base: 0,
            v: VecDeque::new(),
            cap: (secs * analysis::FPS) as usize,
            mags: Default::default(),
            specs: PLACED.iter().map(|&s| Spec::new(s)).collect(),
        }
    }

    pub fn start(&self) -> usize {
        self.base
    }

    pub fn get(&self, j: usize) -> Option<&Frame> {
        j.checked_sub(self.base).and_then(|i| self.v.get(i))
    }

    /// Forget the frames from `j` on (the stream changed there).
    pub fn truncate(&mut self, j: usize) {
        if j <= self.base {
            self.v.clear();
            self.base = j;
        } else {
            self.v.truncate(j - self.base);
        }
    }

    fn put(&mut self, j: usize, f: Frame) {
        if self.v.is_empty() {
            self.base = j;
        }
        if j < self.base {
            return;
        }
        let i = j - self.base;
        if i < self.v.len() {
            self.v[i] = f;
        } else {
            while self.v.len() < i {
                self.v.push_back(Frame::default());
            }
            self.v.push_back(f);
        }
        while self.v.len() > self.cap {
            self.v.pop_front();
            self.base += 1;
        }
    }

    /// Frames [a, b) made from the sources `sep` holds and the mix
    /// (`mix`: f64 left, right and the session sample of their first);
    /// `last`: whole frames (no segment adds to them any more) — only these
    /// teach the sources' loud ends.
    pub fn make(&mut self, sep: &Sep, mix: (&[f64], &[f64], usize), a: usize, b: usize, last: bool) {
        if b <= a {
            return;
        }
        let from = ((a.saturating_sub(WARM)) * HOP) as isize - (NFFT / 2) as isize;
        let len = (b - a.saturating_sub(WARM)) * HOP + NFFT;
        let mut st: Vec<Vec<f32>> = vec![vec![0f32; len]; SOURCES * 2];
        for (sc, v) in st.iter_mut().enumerate() {
            sep.read(sc, from, v);
        }
        let srcs: Vec<(&[f32], &[f32])> = (0..SOURCES).map(|s| (&st[s * 2][..], &st[s * 2 + 1][..])).collect();
        // the kit: the drums' bands (a file's `kit_bands_of`)
        let stft = analysis::Stft::new();
        let reg = analysis::region(&stft, &srcs, true, from, a..b);
        let mut out: Vec<Frame> = (a..b)
            .map(|j| {
                let mut f = Frame::default();
                let i = j - a;
                for (k, &band) in KIT.iter().enumerate() {
                    let o = (i * BANDS + band) * RAW;
                    f.kit[k].copy_from_slice(&reg.band[o..o + RAW]);
                }
                let c = j * HOP;
                f.ok = sep.covered(c.saturating_sub(NFFT / 2), c + NFFT / 2);
                f.mix_db = super::rough::level_at(mix.0, mix.1, mix.2, j);
                f
            })
            .collect();
        // the places' values
        for (p, &s) in PLACED.iter().enumerate() {
            let top = self.mags[p].at(99.9, -150.0).map(|d| 10f32.powf(d / 20.0));
            let top = top.unwrap_or_else(|| first_top(&self.specs[p], srcs[s], from, a, b));
            self.place_run(p, srcs[s], from, a, b, top, &mut out, last);
        }
        for (i, f) in out.into_iter().enumerate() {
            self.put(a + i, f);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place_run(&mut self, p: usize, (l, r): (&[f32], &[f32]), from: isize, a: usize, b: usize, top: f32, out: &mut [Frame], last: bool) {
        let sp = &self.specs[p];
        let nb = sp.nb();
        let norm = 4.0 / (NFFT as f32 * NFFT as f32);
        let ca = (1.0 - (-(1.0 / place::FPS) / COH_TAU).exp()) as f32;
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut scratch = vec![Complex32::new(0.0, 0.0); sp.fft.get_inplace_scratch_len()];
        let (mut fl, mut fr) = (vec![Complex32::new(0.0, 0.0); nb], vec![Complex32::new(0.0, 0.0); nb]);
        let (mut c, mut pl, mut pr) = (vec![Complex32::new(0.0, 0.0); nb], vec![0f32; nb], vec![0f32; nb]);
        let mut prev = vec![0f32; nb];
        let w0 = a.saturating_sub(WARM);
        let mut mags = Vec::new();
        for t in w0..b {
            sp.frame_at(l, r, from, t, &mut buf, &mut scratch, &mut fl, &mut fr);
            let keep = t >= a;
            let mut f = SrcF::default();
            let mut tot = 0f64;
            for k in 0..nb {
                let (ml, mr) = (fl[k].norm(), fr[k].norm());
                let (el, er) = (ml * ml, mr * mr);
                let x = fl[k] * fr[k].conj();
                if t == w0 {
                    (c[k], pl[k], pr[k]) = (x, el, er);
                } else {
                    let ck = c[k];
                    c[k] = ck + (x - ck) * ca;
                    pl[k] += (el - pl[k]) * ca;
                    pr[k] += (er - pr[k]) * ca;
                }
                let e = el + er;
                let m = e.sqrt();
                let lg = (100.0 * m / (top + 1e-12)).ln_1p();
                if keep {
                    let pb = pan_bin(ml, mr);
                    let coh = c[k].norm() / (pl[k] * pr[k] + EPS).sqrt();
                    let coh = if c[k].re < 0.0 { 0.0 } else { coh.min(1.0) };
                    f.e[pb] += e;
                    f.ef[pb] += e * sp.ln_f[sp.k0 + k];
                    f.ec[pb] += e * coh;
                    if t > w0 {
                        f.fl[pb] += (lg - prev[k]).max(0.0);
                    }
                    tot += e as f64;
                    if last {
                        mags.push(20.0 * (m + 1e-12).log10());
                    }
                }
                prev[k] = lg;
            }
            if keep {
                f.lv = 10.0 * ((tot as f32) * norm + 1e-20).log10();
                f.db = mean_power(l, r, from, t);
                out[t - a].src[p] = f;
            }
        }
        for m in mags {
            self.mags[p].push(m);
        }
    }
}

/// A source's flux scale before any of its frames are whole: the 99.9th
/// percentile of its magnitudes over the frames at hand.
fn first_top(sp: &Spec, (l, r): (&[f32], &[f32]), from: isize, a: usize, b: usize) -> f32 {
    let nb = sp.nb();
    let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
    let mut scratch = vec![Complex32::new(0.0, 0.0); sp.fft.get_inplace_scratch_len()];
    let (mut fl, mut fr) = (vec![Complex32::new(0.0, 0.0); nb], vec![Complex32::new(0.0, 0.0); nb]);
    let mut v = Vec::with_capacity((b - a) * nb);
    for t in a..b {
        sp.frame_at(l, r, from, t, &mut buf, &mut scratch, &mut fl, &mut fr);
        for k in 0..nb {
            let (ml, mr) = (fl[k].norm(), fr[k].norm());
            v.push((ml * ml + mr * mr).sqrt());
        }
    }
    if v.is_empty() {
        return 1.0;
    }
    let i = (v.len() as f64 * 0.999) as usize;
    let i = i.min(v.len() - 1);
    *v.select_nth_unstable_by(i, |x, y| x.total_cmp(y)).1
}

/// `stems::frame_db` of frame `j` from f32 signals whose sample 0 is
/// session sample `from`.
fn mean_power(l: &[f32], r: &[f32], from: isize, j: usize) -> f32 {
    let c = (j * HOP) as isize - from;
    let a = (c - 1024).max(0) as usize;
    let b = ((c + 1024).max(0) as usize).min(l.len());
    let mut s = 0f64;
    for i in a..b.max(a) {
        let (x, y) = (l[i] as f64, r[i] as f64);
        s += (x * x + y * y) / 2.0;
    }
    (10.0 * (s / 2048.0 + 1e-20).log10()) as f32
}
