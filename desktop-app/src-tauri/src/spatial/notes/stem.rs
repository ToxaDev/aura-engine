//! A source's notes, measured one by one — the research's note core
//! (`notes\A\core.py`, `portrait.py`, checked against the truth of MDB and
//! synthetic sets): the notes Basic Pitch hears, flagged when a note is only
//! a harmonic of a stronger one (a ghost) or too quiet to count (weak); the
//! fragments of a held note joined; then each note's portrait — its
//! harmonics' profile, its envelope, where it sits and how diffuse it is.
//!
//! Every harmonic of a note is measured in a band of ±40 cents around
//! h·f0(t), f0 following the note's own pitch track, frame by frame (Hann
//! 4096, hop 512 at 44.1 kHz, frame j centred on sample 512·j), in each
//! channel and as the channels' cross-spectrum. The spectrum is made frame
//! by frame and handed to the notes sounding there: a whole stem's
//! spectrogram is never held.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::bp::{self, Post};

pub const SR: f64 = 44_100.0;
const N: usize = 4096;
pub const HOPS: usize = 512;
/// Bins of a frame's spectrum.
const F: usize = N / 2 + 1;
/// Harmonics measured.
pub const NH: usize = 12;
/// The half width of a harmonic's band.
const CENTS: f64 = 40.0;
/// A note's window: this long before its onset (a slow attack is heard
/// late) and after its offset (its release).
const PRE: f64 = 0.6;
const POST: f64 = 0.30;
/// A note is weak under −60 dB, or 40 dB under the mix's loud level.
const WEAK_ABS: f64 = -60.0;
const WEAK_REL: f64 = -40.0;
/// Two fragments of one pitch are one held note when the level dips less
/// than 3 dB where they meet (at most 60 ms apart) — unless it rises 6 dB
/// or more there with the network's onset at 0.9 or more: a new attack
/// (measured on the truth: a held pad's joints rise 2–3 dB with onsets
/// 0.6–0.7; a lead re-struck over it 9–15 dB, 0.92–0.97).
const MAX_DIP: f64 = 3.0;
const MAX_GAP: f64 = 0.06;
const MAX_RISE: f64 = 6.0;
const MAX_ONSET: f64 = 0.9;

fn hann() -> Vec<f32> {
    (0..N).map(|n| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N as f64).cos()) as f32).collect()
}

/// Band energy → the mean square of that component.
pub fn norm() -> f64 {
    2.0 / (N as f64 * hann().iter().map(|&w| w as f64 * w as f64).sum::<f64>())
}

fn db(v: f64) -> f64 {
    10.0 * (v + 1e-20).log10()
}

/// A note of a source and what is known of it.
#[derive(Clone, Debug)]
pub struct SNote {
    pub on: f64,
    pub off: f64,
    /// MIDI, fractional.
    pub pitch: f64,
    pub midi: u8,
    pub amp: f64,
    /// Its frames of the network's pictures.
    pub i0: usize,
    pub i1: usize,
    /// Its level (dB): the mean square of harmonics 1–8 over the note, like
    /// rms dBFS.
    pub lvl: f64,
    /// Too quiet to count.
    pub weak: bool,
    /// The stronger note it is a harmonic of (Basic Pitch hears the 4th and
    /// 8th harmonic of a saw-like tone as notes of their own).
    pub ghost: Option<usize>,
    /// Fragments joined into it.
    pub parts: usize,
}

impl SNote {
    /// Strong and not a ghost: the notes that decide instruments.
    pub fn used(&self) -> bool {
        !self.weak && self.ghost.is_none()
    }
}

/// A note's harmonics, frame by frame over its window (frames `j0 …`).
pub struct Bands {
    pub j0: usize,
    /// Frames inside the note (at least one).
    pub inn: Vec<bool>,
    /// Band energy of each harmonic in the left and right channel …
    pub el: Vec<[f32; NH]>,
    pub er: Vec<[f32; NH]>,
    /// … and the cross-spectrum L·R*.
    pub cx: Vec<[Complex32; NH]>,
    /// The harmonic's band stays under the top of the spectrum in every frame.
    pub whole: [bool; NH],
}

impl Bands {
    pub fn len(&self) -> usize {
        self.el.len()
    }

    /// Frame k's time (s).
    pub fn t(&self, k: usize) -> f64 {
        ((self.j0 + k) * HOPS) as f64 / SR
    }
}

/// What a band measurement needs of a note: its time and its pitch track.
struct Span<'a> {
    on: f64,
    off: f64,
    pitch: f64,
    i0: usize,
    track: &'a [f32],
}

/// f0 (Hz) at time t from a pitch track over the network's frames `i0 …`,
/// held at its ends (numpy's `interp`).
fn f0_at(s: &Span, t: f64) -> f64 {
    let p = s.track;
    let m = match p.len() {
        0 => s.pitch,
        1 => p[0] as f64,
        n => {
            let (a, b) = (bp::frame_time(s.i0), bp::frame_time(s.i0 + n - 1));
            if t <= a {
                p[0] as f64
            } else if t >= b {
                p[n - 1] as f64
            } else {
                // the frames are not evenly spaced across windows: find the pair
                let mut k = ((t - a) * bp::SR as f64 / bp::FFT_HOP as f64) as usize;
                k = k.min(n - 2);
                while k > 0 && bp::frame_time(s.i0 + k) > t {
                    k -= 1;
                }
                while k + 2 < n && bp::frame_time(s.i0 + k + 1) <= t {
                    k += 1;
                }
                let (ta, tb) = (bp::frame_time(s.i0 + k), bp::frame_time(s.i0 + k + 1));
                let (pa, pb) = (p[k] as f64, p[k + 1] as f64);
                (pb - pa) / (tb - ta) * (t - ta) + pa
            }
        }
    };
    440.0 * 2f64.powf((m - 69.0) / 12.0)
}

/// The spectrum of a stereo signal frame by frame (both channels by one
/// complex FFT).
struct Stft<'a> {
    l: &'a [f32],
    r: &'a [f32],
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    buf: Vec<Complex32>,
    /// This frame's left and right spectra, bins 0 … N/2.
    sl: Vec<Complex32>,
    sr: Vec<Complex32>,
}

impl<'a> Stft<'a> {
    fn new(l: &'a [f32], r: &'a [f32]) -> Stft<'a> {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
        let z = Complex32::new(0.0, 0.0);
        Stft { l, r, fft, win: hann(), buf: vec![z; N], sl: vec![z; F], sr: vec![z; F] }
    }

    /// Frames of the signal: frame j centred on sample 512·j.
    fn frames(&self) -> usize {
        1 + self.l.len().min(self.r.len()) / HOPS
    }

    fn frame(&mut self, j: usize) {
        let n = self.l.len().min(self.r.len());
        let s0 = (j * HOPS) as isize - (N / 2) as isize;
        for i in 0..N {
            let s = s0 + i as isize;
            self.buf[i] = if s >= 0 && (s as usize) < n {
                let w = self.win[i];
                Complex32::new(self.l[s as usize] * w, self.r[s as usize] * w)
            } else {
                Complex32::new(0.0, 0.0)
            };
        }
        self.fft.process(&mut self.buf);
        for k in 0..F {
            let a = self.buf[k];
            let b = self.buf[(N - k) % N].conj();
            self.sl[k] = (a + b) * 0.5;
            let d = a - b;
            self.sr[k] = Complex32::new(d.im * 0.5, -d.re * 0.5);
        }
    }
}

/// Every span's harmonic bands over [on − pre, off + post], one pass over
/// the signal's frames.
fn bands_of(l: &[f32], r: &[f32], spans: &[Span], pre: f64, post: f64) -> Vec<Bands> {
    let mut st = Stft::new(l, r);
    let t_all = st.frames();
    let df = SR / N as f64;
    let ratio = 2f64.powf(CENTS / 1200.0);
    let hop = SR / HOPS as f64;
    let mut out: Vec<Bands> = spans
        .iter()
        .map(|s| {
            let j0 = ((s.on - pre) * SR / HOPS as f64).floor().max(0.0) as usize;
            let j1 = ((((s.off + post) * SR / HOPS as f64).ceil() + 1.0).max(0.0) as usize).min(t_all);
            let j1 = j1.max(j0);
            let len = j1 - j0;
            let mut inn: Vec<bool> =
                (0..len).map(|k| {
                    let t = ((j0 + k) * HOPS) as f64 / SR;
                    t >= s.on - 1e-9 && t <= s.off + 1e-9
                }).collect();
            if len > 0 && !inn.iter().any(|&v| v) {
                let mid = 0.5 * (s.on + s.off);
                let k = (0..len)
                    .min_by(|&a, &b| {
                        let da = (((j0 + a) * HOPS) as f64 / SR - mid).abs();
                        let db = (((j0 + b) * HOPS) as f64 / SR - mid).abs();
                        da.total_cmp(&db)
                    })
                    .unwrap();
                inn[k] = true;
            }
            let z = Complex32::new(0.0, 0.0);
            Bands {
                j0,
                inn,
                el: vec![[0.0; NH]; len],
                er: vec![[0.0; NH]; len],
                cx: vec![[z; NH]; len],
                whole: [true; NH],
            }
        })
        .collect();
    let _ = hop;
    // the notes by first frame; the ones sounding at each frame
    let mut order: Vec<usize> = (0..spans.len()).filter(|&i| out[i].len() > 0).collect();
    order.sort_by_key(|&i| out[i].j0);
    let mut next = 0;
    let mut active: Vec<usize> = Vec::new();
    for j in 0..t_all {
        while next < order.len() && out[order[next]].j0 <= j {
            active.push(order[next]);
            next += 1;
        }
        active.retain(|&i| out[i].j0 + out[i].len() > j);
        if active.is_empty() {
            continue;
        }
        st.frame(j);
        let t = (j * HOPS) as f64 / SR;
        for &i in &active {
            let b = &mut out[i];
            let k = j - b.j0;
            let f0 = f0_at(&spans[i], t);
            for h in 0..NH {
                let fh = f0 * (h + 1) as f64;
                let kc = (fh / df).round_ties_even();
                let lo = (fh / ratio / df).floor().min(kc - 1.0);
                let hi = (fh * ratio / df).ceil().max(kc + 1.0);
                if !(hi < (F - 1) as f64) {
                    b.whole[h] = false;
                    continue;
                }
                let lo = lo.clamp(1.0, (F - 1) as f64) as usize;
                let hi = hi.clamp(1.0, (F - 1) as f64) as usize;
                let (mut el, mut er) = (0f64, 0f64);
                let (mut cr, mut ci) = (0f64, 0f64);
                for q in lo..=hi {
                    let (a, c) = (st.sl[q], st.sr[q]);
                    el += (a.re as f64).powi(2) + (a.im as f64).powi(2);
                    er += (c.re as f64).powi(2) + (c.im as f64).powi(2);
                    // a · conj(c)
                    cr += a.re as f64 * c.re as f64 + a.im as f64 * c.im as f64;
                    ci += a.im as f64 * c.re as f64 - a.re as f64 * c.im as f64;
                }
                b.el[k][h] = el as f32;
                b.er[k][h] = er as f32;
                b.cx[k][h] = Complex32::new(cr as f32, ci as f32);
            }
        }
    }
    out
}

/// The notes' spans (for `bands_of`).
fn spans<'a>(notes: &'a [SNote], tracks: &'a [Vec<f32>]) -> Vec<Span<'a>> {
    notes
        .iter()
        .zip(tracks)
        .map(|(n, t)| Span { on: n.on, off: n.off, pitch: n.pitch, i0: n.i0, track: t })
        .collect()
}

/// A note's level (dB): the mean square of harmonics 1…nh over its frames.
fn level(b: &Bands, norm: f64, nh: usize) -> f64 {
    let (mut s, mut c) = (0f64, 0usize);
    for k in (0..b.len()).filter(|&k| b.inn[k]) {
        s += (0..nh).map(|h| 0.5 * (b.el[k][h] as f64 + b.er[k][h] as f64)).sum::<f64>();
        c += 1;
    }
    db(norm * s / c.max(1) as f64)
}

/// `bp.mark_ghosts`: a note starting with a stronger one (within 60 ms) at
/// one of its harmonics 2…8 (±0.4 semitone), overlapping it for ≥ 60 % of
/// its length, is that note's ghost (the lowest such parent). A flag only: a
/// real octave doubling looks the same.
fn mark_ghosts(notes: &mut [SNote]) {
    let kst: Vec<f64> = (2..=8).map(|k| 12.0 * (k as f64).log2()).collect();
    let n = notes.len();
    let mut ghost = vec![None; n];
    for i in 0..n {
        let e = &notes[i];
        let mut best: Option<usize> = None;
        for (j, p) in notes.iter().enumerate() {
            if (p.on - e.on).abs() >= 0.06 || j == i || p.amp <= e.amp {
                continue;
            }
            let d = e.pitch - p.pitch;
            if !kst.iter().any(|k| (k - d).abs() < 0.4) {
                continue;
            }
            let ov = e.off.min(p.off) - e.on.max(p.on);
            if ov < 0.6 * (e.off - e.on) {
                continue;
            }
            if best.is_none_or(|b| p.pitch < notes[b].pitch) {
                best = Some(j);
            }
        }
        ghost[i] = best;
    }
    for (e, g) in notes.iter_mut().zip(ghost) {
        e.ghost = g;
    }
}

/// A source's notes with their bands and pitch tracks.
pub struct StemNotes {
    pub notes: Vec<SNote>,
    pub tracks: Vec<Vec<f32>>,
    pub bands: Vec<Bands>,
    /// The mix's loud level the weak gate used (dB).
    pub mix_ref: f64,
    pub post: Post,
}

/// `core.notes_of` + `core.merge_fragments`: the notes of a stereo source
/// (44.1 kHz) from its transcript, flagged, joined, measured.
pub fn notes_of(tr: bp::Transcript, l: &[f32], r: &[f32], mix_ref: f64) -> StemNotes {
    let norm = norm();
    let bp::Transcript { post, notes, tracks, .. } = tr;
    let mut notes: Vec<SNote> = notes
        .into_iter()
        .map(|n| SNote {
            on: n.on,
            off: n.off,
            pitch: n.pitch,
            midi: n.midi,
            amp: n.amp,
            i0: n.i0,
            i1: n.i1,
            lvl: 0.0,
            weak: false,
            ghost: None,
            parts: 1,
        })
        .collect();
    mark_ghosts(&mut notes);
    let bands = bands_of(l, r, &spans(&notes, &tracks), PRE, POST);
    for (e, b) in notes.iter_mut().zip(&bands) {
        e.lvl = level(b, norm, 8);
        e.weak = e.lvl < WEAK_ABS || e.lvl - mix_ref < WEAK_REL;
    }
    let s = StemNotes { notes, tracks, bands, mix_ref, post };
    merge_fragments(s, l, r)
}

/// Per-frame level (dB) of harmonics 1–4.
fn lev4(b: &Bands, norm: f64) -> Vec<f64> {
    (0..b.len()).map(|k| db(norm * (0..4).map(|h| 0.5 * (b.el[k][h] as f64 + b.er[k][h] as f64)).sum::<f64>())).collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// `core.junctions`: consecutive fragments of one key (strong notes, ≤ 0.35
/// semitone apart, the second starting −30…+60 ms from the first's end):
/// (a, b, dip, rise, onset) — how far the level of harmonics 1–4 dips at the
/// joint under the quieter fragment's median, how much it rises after the
/// joint, the network's onset strength there.
fn junctions(s: &StemNotes, norm: f64) -> Vec<(usize, usize, f64, f64, f64)> {
    let n = &s.notes;
    let mut idx: Vec<usize> = (0..n.len()).filter(|&i| n[i].used()).collect();
    idx.sort_by(|&a, &b| (n[a].midi, n[a].on).partial_cmp(&(n[b].midi, n[b].on)).unwrap());
    let mut out = Vec::new();
    for w in idx.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (na, nb) = (&n[a], &n[b]);
        if na.midi != nb.midi || (na.pitch - nb.pitch).abs() > 0.35 {
            continue;
        }
        let gap = nb.on - na.off;
        if !(-0.03..=MAX_GAP).contains(&gap) {
            continue;
        }
        let (ba, bb) = (&s.bands[a], &s.bands[b]);
        let (la, lb) = (lev4(ba, norm), lev4(bb, norm));
        let inner = |b: &Bands, l: &[f64]| median((0..b.len()).filter(|&k| b.inn[k]).map(|k| l[k]).collect());
        let body = inner(ba, &la).min(inner(bb, &lb));
        let within = |lo: f64, hi: f64| -> Vec<f64> {
            (0..ba.len()).filter(|&k| ba.t(k) >= lo && ba.t(k) <= hi).map(|k| la[k]).collect()
        };
        let m = within(nb.on - 0.04, nb.on + 0.03);
        if m.is_empty() {
            continue;
        }
        let (pre, post) = (within(nb.on - 0.12, nb.on - 0.04), within(nb.on, nb.on + 0.10));
        let rise = if !pre.is_empty() && !post.is_empty() {
            post.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - median(pre)
        } else {
            0.0
        };
        let c = nb.midi as usize - bp::MIDI0 as usize;
        let onset = (nb.i0.saturating_sub(2)..(nb.i0 + 3).min(s.post.frames))
            .map(|i| s.post.onset[i * bp::KEYS + c] as f64)
            .fold(f64::NEG_INFINITY, f64::max);
        let dip = body - m.iter().cloned().fold(f64::INFINITY, f64::min);
        out.push((a, b, dip, rise, onset));
    }
    out
}

/// `core.merge_fragments`: the fragments of held notes joined into one note;
/// the joined notes' pitch tracks, bands and levels measured again.
fn merge_fragments(s: StemNotes, l: &[f32], r: &[f32]) -> StemNotes {
    let norm = norm();
    let joints = junctions(&s, norm);
    let mut nxt = std::collections::HashMap::new();
    let mut prv = std::collections::HashSet::new();
    for &(a, b, dip, rise, onset) in &joints {
        if dip < MAX_DIP && !(rise >= MAX_RISE && onset >= MAX_ONSET) {
            nxt.insert(a, b);
            prv.insert(b);
        }
    }
    let StemNotes { notes, mix_ref, post, .. } = s;
    let mut new: Vec<SNote> = Vec::new();
    let mut old2new = vec![usize::MAX; notes.len()];
    for i in 0..notes.len() {
        if prv.contains(&i) {
            continue;
        }
        let mut chain = vec![i];
        while let Some(&b) = nxt.get(chain.last().unwrap()) {
            chain.push(b);
        }
        let k = new.len();
        for &c in &chain {
            old2new[c] = k;
        }
        let mut m = notes[i].clone();
        if chain.len() > 1 {
            let last = &notes[*chain.last().unwrap()];
            let du: Vec<f64> = chain.iter().map(|&c| (notes[c].off - notes[c].on).max(1e-3)).collect();
            let amp = chain.iter().zip(&du).map(|(&c, d)| d * notes[c].amp).sum::<f64>() / du.iter().sum::<f64>();
            m.off = last.off;
            m.i1 = last.i1;
            m.parts = chain.len();
            m.amp = amp;
        }
        new.push(m);
    }
    for e in &mut new {
        e.ghost = e.ghost.and_then(|g| (old2new[g] != usize::MAX).then_some(old2new[g]));
    }
    // by onset (stable)
    let mut order: Vec<usize> = (0..new.len()).collect();
    order.sort_by(|&a, &b| new[a].on.total_cmp(&new[b].on));
    let mut remap = vec![0; new.len()];
    for (r, &o) in order.iter().enumerate() {
        remap[o] = r;
    }
    let mut notes: Vec<SNote> = order.iter().map(|&o| new[o].clone()).collect();
    for e in &mut notes {
        e.ghost = e.ghost.map(|g| remap[g]);
    }
    let mut tracks = Vec::with_capacity(notes.len());
    for e in &mut notes {
        let (p, v) = bp::pitch_track(&post, e.i0, e.i1, e.midi);
        if e.parts > 1 {
            let top = v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            let g: Vec<f64> = p.iter().zip(&v).filter(|(_, &x)| x >= 0.5 * top).map(|(&x, _)| x).collect();
            if !g.is_empty() {
                e.pitch = median(g);
            }
        }
        tracks.push(p.iter().map(|&x| x as f32).collect::<Vec<f32>>());
    }
    let bands = bands_of(l, r, &spans(&notes, &tracks), PRE, POST);
    for (e, b) in notes.iter_mut().zip(&bands) {
        e.lvl = level(b, norm, 8);
        if e.parts > 1 {
            e.weak = e.lvl < WEAK_ABS || e.lvl - mix_ref < WEAK_REL;
        }
    }
    StemNotes { notes, tracks, bands, mix_ref, post }
}

/// The mix's loud level for the weak gate (`run_a1.mix_ref`): the sources
/// summed (in f32, in order), then p95 of the mean power of 2048-sample
/// blocks (dB).
pub fn mix_ref(src: &[[&[f32]; 2]; 6]) -> f64 {
    let n = src.iter().flat_map(|s| s.iter()).map(|c| c.len()).min().unwrap_or(0);
    let mut lv = Vec::with_capacity(n / 2048);
    for b in 0..n / 2048 {
        let mut p = 0f64;
        for i in b * 2048..(b + 1) * 2048 {
            let (mut l, mut r) = (0f32, 0f32);
            for s in src {
                l += s[0][i];
                r += s[1][i];
            }
            p += (l as f64 * l as f64 + r as f64 * r as f64) / 2.0;
        }
        lv.push(db(p / 2048.0));
    }
    crate::spatial::stems::percentile(&lv, 95.0)
}

// ------------------------------------------------------------------ portraits

/// Octave bands (centres, Hz) of the spectral envelope.
pub const ENV_BANDS: [f64; 6] = [250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0];
/// Harmonics above this are not used.
const FMAX: f64 = 16_000.0;
/// A note collides with another only if they overlap ≥ 25 % of its time.
const MIN_OV: f64 = 0.25;

/// Each note's portrait (`portrait.features`, the part the instruments
/// use); NaN where it cannot be measured.
#[derive(Clone, Debug, Default)]
pub struct Portrait {
    pub dur: Vec<f64>,
    pub pitch: Vec<f64>,
    /// dB over its free harmonics ≤ 8
    pub lvl: Vec<f64>,
    /// The harmonic profile: dB under the strongest (≥ −60), per harmonic.
    pub h_rel: Vec<[f64; NH]>,
    /// Its slope over log2(harmonic number), dB an octave.
    pub tilt: Vec<f64>,
    /// Odd over even harmonics beyond the slope (dB).
    pub oddeven: Vec<f64>,
    pub h1_rel: Vec<f64>,
    /// The profile in octave bands (`ENV_BANDS`).
    pub env: Vec<[f64; 6]>,
    pub attack: Vec<f64>,
    pub rise_db: Vec<f64>,
    pub decay: Vec<f64>,
    pub sustain: Vec<f64>,
    pub tail: Vec<f64>,
    pub tail_coh: Vec<f64>,
    /// Vibrato depth (cents).
    pub vib_c: Vec<f64>,
    /// −1 left … +1 right.
    pub pan: Vec<f64>,
    pub coh: Vec<f64>,
    /// Side over mid (dB).
    pub width_db: Vec<f64>,
    /// Coherence against this source's own coherence at the same
    /// frequencies: > 0 drier than its sources there, < 0 more diffuse.
    pub dres: Vec<f64>,
    /// Per harmonic: coherence, energy, frequency (for `dres`).
    pub coh_h: Vec<[f64; NH]>,
    pub e_h: Vec<[f64; NH]>,
    pub f_h: Vec<[f64; NH]>,
}

/// One note's portrait (a row of `Portrait`): a live stream keeps its notes'
/// rows and puts them together again (`Portrait::from_rows`) to split a
/// song's source as more of its notes come. NaN: not measured.
#[derive(Clone, Copy, Debug)]
pub struct PRow {
    pub dur: f64,
    pub pitch: f64,
    pub lvl: f64,
    pub h_rel: [f64; NH],
    pub tilt: f64,
    pub oddeven: f64,
    pub h1_rel: f64,
    pub env: [f64; 6],
    pub attack: f64,
    pub rise_db: f64,
    pub decay: f64,
    pub sustain: f64,
    pub tail: f64,
    pub tail_coh: f64,
    pub vib_c: f64,
    pub pan: f64,
    pub coh: f64,
    pub width_db: f64,
    pub dres: f64,
    pub coh_h: [f64; NH],
    pub e_h: [f64; NH],
    pub f_h: [f64; NH],
}

impl Default for PRow {
    fn default() -> PRow {
        let nan = f64::NAN;
        PRow {
            dur: nan,
            pitch: nan,
            lvl: nan,
            h_rel: [nan; NH],
            tilt: nan,
            oddeven: nan,
            h1_rel: nan,
            env: [nan; 6],
            attack: nan,
            rise_db: nan,
            decay: nan,
            sustain: nan,
            tail: nan,
            tail_coh: nan,
            vib_c: nan,
            pan: nan,
            coh: nan,
            width_db: nan,
            dres: nan,
            coh_h: [nan; NH],
            e_h: [nan; NH],
            f_h: [nan; NH],
        }
    }
}

impl Portrait {
    /// Note `i`'s row.
    pub fn row(&self, i: usize) -> PRow {
        PRow {
            dur: self.dur[i],
            pitch: self.pitch[i],
            lvl: self.lvl[i],
            h_rel: self.h_rel[i],
            tilt: self.tilt[i],
            oddeven: self.oddeven[i],
            h1_rel: self.h1_rel[i],
            env: self.env[i],
            attack: self.attack[i],
            rise_db: self.rise_db[i],
            decay: self.decay[i],
            sustain: self.sustain[i],
            tail: self.tail[i],
            tail_coh: self.tail_coh[i],
            vib_c: self.vib_c[i],
            pan: self.pan[i],
            coh: self.coh[i],
            width_db: self.width_db[i],
            dres: self.dres[i],
            coh_h: self.coh_h[i],
            e_h: self.e_h[i],
            f_h: self.f_h[i],
        }
    }

    /// The portrait of these notes' rows, in their order.
    pub fn from_rows<'a>(rows: impl IntoIterator<Item = &'a PRow>) -> Portrait {
        let mut p = Portrait::default();
        for r in rows {
            p.dur.push(r.dur);
            p.pitch.push(r.pitch);
            p.lvl.push(r.lvl);
            p.h_rel.push(r.h_rel);
            p.tilt.push(r.tilt);
            p.oddeven.push(r.oddeven);
            p.h1_rel.push(r.h1_rel);
            p.env.push(r.env);
            p.attack.push(r.attack);
            p.rise_db.push(r.rise_db);
            p.decay.push(r.decay);
            p.sustain.push(r.sustain);
            p.tail.push(r.tail);
            p.tail_coh.push(r.tail_coh);
            p.vib_c.push(r.vib_c);
            p.pan.push(r.pan);
            p.coh.push(r.coh);
            p.width_db.push(r.width_db);
            p.dres.push(r.dres);
            p.coh_h.push(r.coh_h);
            p.e_h.push(r.e_h);
            p.f_h.push(r.f_h);
        }
        p
    }
}

/// The diffuseness of notes measured apart made again against the curve of
/// all of them (`used`: the strong notes), as `portrait` makes it for the
/// notes it measures together.
pub fn diffuse(p: &mut Portrait, used: &[usize]) {
    p.dres.iter_mut().for_each(|v| *v = f64::NAN);
    add_diffuse(p, used);
}

fn hw(f: f64) -> f64 {
    (f * (2f64.powf(CENTS / 1200.0) - 1.0)).max(1.5 * SR / N as f64)
}

fn midi_hz(m: f64) -> f64 {
    440.0 * 2f64.powf((m - 69.0) / 12.0)
}

/// Harmonic h of note i overlaps a harmonic band of another sounding strong
/// note: it carries both and is left out.
fn collisions(notes: &[SNote]) -> Vec<[bool; NH]> {
    let used: Vec<usize> = (0..notes.len()).filter(|&i| notes[i].used()).collect();
    let f0: Vec<f64> = notes.iter().map(|e| midi_hz(e.pitch)).collect();
    notes
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut col = [false; NH];
            let d = e.off - e.on;
            for &k in &used {
                let o = &notes[k];
                let ov = o.off.min(e.off) - o.on.max(e.on);
                if k == i || ov < (MIN_OV * d).max(0.02) {
                    continue;
                }
                for (h, c) in col.iter_mut().enumerate() {
                    let fi = (h + 1) as f64 * f0[i];
                    let g = (fi / f0[k]).round_ties_even().max(1.0);
                    let fk = g * f0[k];
                    if (fi - fk).abs() < hw(fi) + hw(fk) && g <= NH as f64 {
                        *c = true;
                    }
                }
            }
            col
        })
        .collect()
}

fn slope(t: &[f64], y: &[f64]) -> f64 {
    if t.len() < 3 {
        return f64::NAN;
    }
    let (mt, my) = (t.iter().sum::<f64>() / t.len() as f64, y.iter().sum::<f64>() / y.len() as f64);
    let d: f64 = t.iter().map(|v| (v - mt) * (v - mt)).sum();
    if d > 0.0 {
        t.iter().zip(y).map(|(a, b)| (a - mt) * (b - my)).sum::<f64>() / d
    } else {
        f64::NAN
    }
}

/// The spread (population sd) of y around its straight line.
fn wobble(y: &[f64]) -> f64 {
    if y.len() < 12 {
        return f64::NAN;
    }
    let t: Vec<f64> = (0..y.len()).map(|i| i as f64).collect();
    let (mt, my) = (t.iter().sum::<f64>() / t.len() as f64, y.iter().sum::<f64>() / y.len() as f64);
    let d: f64 = t.iter().map(|v| (v - mt) * (v - mt)).sum();
    let a = t.iter().zip(y).map(|(p, q)| (p - mt) * (q - my)).sum::<f64>() / d;
    let r: Vec<f64> = t.iter().zip(y).map(|(p, q)| q - (my + a * (p - mt))).collect();
    let mr = r.iter().sum::<f64>() / r.len() as f64;
    (r.iter().map(|v| (v - mr) * (v - mr)).sum::<f64>() / r.len() as f64).sqrt()
}

/// Least squares y = c0·x + c1 → (c0, c1).
fn line(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let d: f64 = x.iter().map(|v| (v - mx) * (v - mx)).sum();
    let c0 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum::<f64>() / d;
    (c0, my - c0 * mx)
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (mut s, mut c) = (0f64, 0usize);
    for x in v {
        s += x;
        c += 1;
    }
    if c == 0 {
        f64::NAN
    } else {
        s / c as f64
    }
}

/// `portrait.features` (what the instruments use) + `add_diffuse` (dres).
pub fn portrait(s: &StemNotes) -> Portrait {
    let norm = norm();
    let notes = &s.notes;
    let n = notes.len();
    let col = collisions(notes);
    let nan = f64::NAN;
    let mut p = Portrait {
        dur: vec![nan; n],
        pitch: vec![nan; n],
        lvl: vec![nan; n],
        h_rel: vec![[nan; NH]; n],
        tilt: vec![nan; n],
        oddeven: vec![nan; n],
        h1_rel: vec![nan; n],
        env: vec![[nan; 6]; n],
        attack: vec![nan; n],
        rise_db: vec![nan; n],
        decay: vec![nan; n],
        sustain: vec![nan; n],
        tail: vec![nan; n],
        tail_coh: vec![nan; n],
        vib_c: vec![nan; n],
        pan: vec![nan; n],
        coh: vec![nan; n],
        width_db: vec![nan; n],
        dres: vec![nan; n],
        coh_h: vec![[nan; NH]; n],
        e_h: vec![[nan; NH]; n],
        f_h: vec![[nan; NH]; n],
    };
    let used: Vec<usize> = (0..n).filter(|&i| notes[i].used()).collect();
    // where the previous strong note at this pitch ends (the attack is looked for after it)
    let prev_off: Vec<f64> = (0..n)
        .map(|i| {
            let e = &notes[i];
            let m = used
                .iter()
                .filter(|&&k| k != i && (notes[k].pitch - e.pitch).abs() < 0.5 && notes[k].on < e.on - 0.02)
                .map(|&k| notes[k].off)
                .fold(f64::NEG_INFINITY, f64::max);
            if m.is_finite() { m.min(e.on) } else { f64::NEG_INFINITY }
        })
        .collect();
    let lg = |v: f64| db(norm * v);
    for i in 0..n {
        let (e, b) = (&notes[i], &s.bands[i]);
        p.dur[i] = e.off - e.on;
        p.pitch[i] = e.pitch;
        let f0m = midi_hz(e.pitch);
        let ok: [bool; NH] = std::array::from_fn(|h| b.whole[h] && ((h + 1) as f64 * f0m) < FMAX);
        let av: [bool; NH] = std::array::from_fn(|h| ok[h] && !col[i][h]);
        let mut eh: [bool; NH] = std::array::from_fn(|h| av[h] && h < 8);
        if !eh.iter().any(|&v| v) {
            eh = std::array::from_fn(|h| ok[h] && h < 8);
        }
        if !eh.iter().any(|&v| v) {
            continue;
        }
        let len = b.len();
        let e2 = |k: usize, h: usize| 0.5 * (b.el[k][h] as f64 + b.er[k][h] as f64);
        let env: Vec<f64> = (0..len).map(|k| (0..NH).filter(|&h| eh[h]).map(|h| e2(k, h)).sum()).collect();
        let edb: Vec<f64> = env.iter().map(|&v| lg(v)).collect();
        let tt: Vec<f64> = (0..len).map(|k| b.t(k)).collect();
        let ji: Vec<usize> = (0..len).filter(|&k| b.inn[k]).collect();
        if ji.is_empty() {
            continue;
        }
        let mut pk = ji[0];
        for &k in &ji {
            if env[k] > env[pk] {
                pk = k;
            }
        }
        p.lvl[i] = lg(mean(ji.iter().map(|&k| env[k])));
        let body: Vec<usize> = ji.iter().copied().filter(|&k| edb[k] >= edb[pk] - 10.0).collect();
        let avh: Vec<usize> = (0..NH).filter(|&h| av[h]).collect();
        if !avh.is_empty() {
            let hd: Vec<f64> = avh.iter().map(|&h| lg(mean(body.iter().map(|&k| e2(k, h))))).collect();
            let top = hd.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let rel: Vec<f64> = hd.iter().map(|v| v - top).collect();
            for (j, &h) in avh.iter().enumerate() {
                p.h_rel[i][h] = rel[j].max(-60.0);
            }
            if avh.len() >= 3 {
                let x: Vec<f64> = avh.iter().map(|&h| ((h + 1) as f64).log2()).collect();
                let (c0, c1) = line(&x, &rel);
                p.tilt[i] = c0;
                let res: Vec<f64> = x.iter().zip(&rel).map(|(a, b)| b - (c0 * a + c1)).collect();
                let od = mean(avh.iter().zip(&res).filter(|(&h, _)| (h + 1) % 2 == 1 && h > 0).map(|(_, &v)| v));
                let ev = mean(avh.iter().zip(&res).filter(|(&h, _)| (h + 1) % 2 == 0).map(|(_, &v)| v));
                if od.is_finite() && ev.is_finite() {
                    p.oddeven[i] = od - ev;
                }
            }
            if av[0] {
                p.h1_rel[i] = rel[0];
            }
            for (k, &fc) in ENV_BANDS.iter().enumerate() {
                let (lo, hi) = (fc / 2f64.sqrt(), fc * 2f64.sqrt());
                let v = mean(
                    avh.iter()
                        .zip(&rel)
                        .filter(|(&h, _)| {
                            let f = (h + 1) as f64 * f0m;
                            f >= lo && f < hi
                        })
                        .map(|(_, &r)| r.max(-60.0)),
                );
                if v.is_finite() {
                    p.env[i][k] = v;
                }
            }
        }
        // the attack: from the lowest point before the peak (not before the
        // previous note at this pitch ends, at most PRE before the onset)
        let a: Vec<f64> = env.iter().map(|v| v.sqrt()).collect();
        let from = (e.on - PRE).max(prev_off[i]);
        let js = tt.partition_point(|&t| t < from).min(pk);
        let (mut jlo, mut j, mut amin) = (pk, pk, a[pk]);
        while j > js && a[j - 1] <= amin * 1.413 {
            j -= 1;
            if a[j] < amin {
                amin = a[j];
                jlo = j;
            }
        }
        let alo = a[jlo];
        p.rise_db[i] = 20.0 * ((a[pk] + 1e-12) / (alo + 1e-12)).log10();
        let lo10 = alo + 0.1 * (a[pk] - alo);
        let hi90 = alo + 0.9 * (a[pk] - alo);
        let cross = |level: f64, jf: usize, jt: usize| -> f64 {
            for j in jf..=jt {
                if a[j] >= level {
                    if j == jf {
                        return tt[j];
                    }
                    let f = (level - a[j - 1]) / (a[j] - a[j - 1]).max(1e-12);
                    return tt[j - 1] + f * (tt[j] - tt[j - 1]);
                }
            }
            tt[jt]
        };
        p.attack[i] = (cross(hi90, jlo, pk) - cross(lo10, jlo, pk)).max(0.0);
        let after: Vec<usize> = ji.iter().copied().filter(|&k| k >= pk).collect();
        p.decay[i] = slope(&after.iter().map(|&k| tt[k]).collect::<Vec<_>>(), &after.iter().map(|&k| edb[k]).collect::<Vec<_>>());
        p.sustain[i] = edb[*ji.last().unwrap()] - edb[pk];
        // the tail: after the offset until another strong note at this pitch starts
        let tend = used
            .iter()
            .filter(|&&k| {
                k != i
                    && notes[k].on > e.off - 0.02
                    && notes[k].on < e.off + POST
                    && (notes[k].pitch - e.pitch).abs() < 0.5
            })
            .map(|&k| notes[k].on)
            .fold(e.off + POST, f64::min);
        let jt: Vec<usize> = (0..len).filter(|&k| tt[k] > e.off && tt[k] < tend - 0.01).collect();
        if jt.len() >= 3 {
            p.tail[i] = slope(&jt.iter().map(|&k| tt[k]).collect::<Vec<_>>(), &jt.iter().map(|&k| edb[k]).collect::<Vec<_>>());
            p.tail_coh[i] = coherence(b, &jt, &eh);
        }
        let tr = &s.tracks[i];
        if tr.len() >= 20 {
            p.vib_c[i] = wobble(&tr.iter().map(|&v| 100.0 * v as f64).collect::<Vec<_>>());
        }
        // where it sits, over its frames and free harmonics (all whole ones if none is free)
        let sh = if av.iter().any(|&v| v) { av } else { ok };
        let (mut sl, mut sr, mut sc) = ([0f64; NH], [0f64; NH], [(0f64, 0f64); NH]);
        for &k in &ji {
            for h in (0..NH).filter(|&h| sh[h]) {
                sl[h] += b.el[k][h] as f64;
                sr[h] += b.er[k][h] as f64;
                sc[h].0 += b.cx[k][h].re as f64;
                sc[h].1 += b.cx[k][h].im as f64;
            }
        }
        let (gl, gr) = (sl.iter().sum::<f64>().sqrt(), sr.iter().sum::<f64>().sqrt());
        p.pan[i] = (gr - gl) / (gr + gl).max(1e-20);
        p.coh[i] = coherence(b, &ji, &sh);
        let side: f64 = (0..NH).filter(|&h| sh[h]).map(|h| sl[h] + sr[h] - 2.0 * sc[h].0).sum();
        let mid: f64 = (0..NH).filter(|&h| sh[h]).map(|h| sl[h] + sr[h] + 2.0 * sc[h].0).sum();
        p.width_db[i] = 10.0 * (side.max(1e-30) / mid.max(1e-30)).log10();
        for &h in &avh {
            let c2 = sc[h].0 * sc[h].0 + sc[h].1 * sc[h].1;
            p.coh_h[i][h] = c2 / (sl[h] * sr[h]).max(1e-30);
            p.e_h[i][h] = norm * (sl[h] + sr[h]) / ji.len().max(1) as f64;
        }
        for h in 0..NH {
            p.f_h[i][h] = (h + 1) as f64 * f0m;
        }
    }
    add_diffuse(&mut p, &used);
    p
}

/// The energy-weighted coherence of the given harmonics over the given frames.
fn coherence(b: &Bands, frames: &[usize], hs: &[bool; NH]) -> f64 {
    let (mut num, mut den) = (0f64, 0f64);
    for h in (0..NH).filter(|&h| hs[h]) {
        let (mut l, mut r, mut cr, mut ci) = (0f64, 0f64, 0f64, 0f64);
        for &k in frames {
            l += b.el[k][h] as f64;
            r += b.er[k][h] as f64;
            cr += b.cx[k][h].re as f64;
            ci += b.cx[k][h].im as f64;
        }
        let w = l + r;
        num += (cr * cr + ci * ci) / (l * r).max(1e-30) * w;
        den += w;
    }
    num / den.max(1e-30)
}

/// `portrait.add_diffuse`: a note's coherence against the source's own
/// coherence-vs-frequency curve (1/3-octave bands, energy-weighted over the
/// strong notes' free harmonics — a spaced pair's coherence falls with
/// frequency, so raw coherence would split one wide instrument by register).
fn add_diffuse(p: &mut Portrait, used: &[usize]) {
    let edges: Vec<f64> = (0..25).map(|k| (50f64.log2() + k as f64 * (1.0 / 3.0)).exp2()).collect();
    let band = |f: f64| (edges.partition_point(|&e| e <= f) as isize - 1).clamp(0, edges.len() as isize - 1) as usize;
    let (mut num, mut den) = (vec![0f64; edges.len()], vec![0f64; edges.len()]);
    for &i in used {
        for h in 0..NH {
            let (c, e, f) = (p.coh_h[i][h], p.e_h[i][h], p.f_h[i][h]);
            if c.is_finite() && e.is_finite() && e > 0.0 {
                let k = band(f);
                num[k] += e.sqrt() * c;
                den[k] += e.sqrt();
            }
        }
    }
    let good: Vec<usize> = (0..edges.len()).filter(|&k| den[k] > 0.0).collect();
    if good.is_empty() {
        return;
    }
    let raw: Vec<f64> = (0..edges.len()).map(|k| num[k] / den[k].max(1e-30)).collect();
    // gaps filled by straight lines between the bands that have notes (held at the ends)
    let curve: Vec<f64> = (0..edges.len())
        .map(|k| {
            let pos = good.partition_point(|&g| g < k);
            if pos < good.len() && good[pos] == k {
                raw[k]
            } else if pos == 0 {
                raw[good[0]]
            } else if pos == good.len() {
                raw[*good.last().unwrap()]
            } else {
                let (a, b) = (good[pos - 1], good[pos]);
                raw[a] + (k - a) as f64 * (raw[b] - raw[a]) / (b - a) as f64
            }
        })
        .collect();
    for i in 0..p.dres.len() {
        let (mut s, mut w) = (0f64, 0f64);
        for h in 0..NH {
            let r = p.coh_h[i][h] - curve[band(p.f_h[i][h])];
            let e = p.e_h[i][h];
            if r.is_finite() && e.is_finite() {
                s += r * e.sqrt();
                w += e.sqrt();
            }
        }
        if w > 0.0 {
            p.dres[i] = s / w;
        }
    }
}
