//! The notes of a live stream (TASK-27, stages 2b and 3): the bass's, the
//! voice's, the guitars', the keys' and the others' notes as a file has them
//! (`notes::objects`: Basic Pitch over each source, its pictures decoded and
//! each note measured; then each note on a place of its source, `song`), made
//! as the stream comes, ahead of what is heard.
//!
//! The sources come from `Sep` up to the end of its last segment. The note
//! network's windows lie on a file's grid from the first sample of the run of
//! segments (silence before it, as before a file's start): a window of whole
//! sound runs once; the ones over the last segment's own quarter, and the
//! grid's next one over what is there so far — silence after it, as a file's
//! last window —, run again each segment. So a note's start is known two
//! seconds or more before it is heard (the last segment ends 2.15 s ahead of
//! the place heard at least; what is whole, as little as 0.2 s).
//!
//! The pictures are decoded as a file's (`bp::transcribe`) from two seconds
//! before the first note not settled yet, their onsets inferred against the
//! strongest the run has had so far (a file's against its whole track's), and
//! the notes measured as a file's (`stem::notes_of`: ghosts, weak notes,
//! fragments joined; `stem::portrait`: where each sits). A note is settled
//! once it ends 20 frames inside the pictures of whole sound and starts 60 ms
//! before the first note still open: nothing still to come can change it then
//! (a fragment to join, a stronger note it is a harmonic of). The others are
//! the stream's own for now, made again each segment.
//!
//! f32: the network's boundary (§4.1), read by the picture's analysis;
//! nothing goes back to what is heard.

use std::collections::HashMap;

use super::super::notes::bp::{self, Windows, BINS, HOP, KEEP, KEYS, N_WIN, OLAP, WIN_FRAMES};
use super::super::notes::objects::TAIL;
use super::super::notes::stem::{self, Bands, PRow, SNote};
use super::super::stems;
use super::sep::Sep;

/// The sources with notes: the bass, the voice and the sources of the note
/// instruments (their index here is their `feats::PLACED` index).
pub const SOURCES: [usize; 5] = [stems::BASS, stems::VOCALS, stems::GUITAR, stems::PIANO, stems::OTHER];
/// The first of them that is a note instruments' source (its notes are split
/// into its instruments, as a file's guitars, keys and others).
pub const INST: usize = 2;
const SR: f64 = 44_100.0;
/// A note is settled once its end lies this many frames inside the pictures
/// of whole sound (a gap's tolerance, a fragment's join and a little more) …
const INSIDE: usize = 20;
/// … and it starts this many frames (60 ms: a ghost's reach) before the first
/// note still open.
const BEFORE_OPEN: usize = 6;
/// The decoding starts this many frames (2 s) before the first note not settled.
const BACK: usize = 172;
/// A note still open after this many frames (12 s) is settled as it is.
const LONGEST: usize = 1034;
/// The samples kept are dropped from the front in pieces of at least this many.
const TRIM: usize = 1 << 17;

/// A note of the stream (session seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LNote {
    pub on: f64,
    pub off: f64,
    /// MIDI, fractional.
    pub key: f32,
    /// Its level (dB: harmonics 1–8, like rms dBFS) and where it sits (−1 …
    /// +1; a ghost where its parent does).
    pub lvl: f32,
    pub pan: f32,
    pub ghost: bool,
}

/// A settled note and the place it went to (the song's number, the object's
/// id) once there was one; a note instruments' source's also what it is like.
#[derive(Clone, Debug)]
pub struct Placed {
    pub n: LNote,
    pub to: Option<(u32, u32)>,
    pub like: Option<Box<Like>>,
}

/// A note of a note instruments' source besides its time and key: its id
/// (the stream's), its sound, and when it was settled as a note what it is
/// like (its portrait), its level as a file measures an instrument's notes
/// (`SNote::lvl`) and the note it is a harmonic of (a ghost's).
#[derive(Clone, Debug)]
pub struct Like {
    pub id: u64,
    pub row: Option<PRow>,
    pub lvl: f64,
    pub parent: Option<u64>,
    pub sound: Sound,
}

/// A note's sound frame by frame, as a file's note instrument counts it
/// (`notes::objects`: its harmonics 1–8 from its onset to 0.15 s past its
/// end), session frame `f0` first.
#[derive(Clone, Debug, Default)]
pub struct Sound {
    pub f0: usize,
    pub e: Vec<f32>,
}

impl Sound {
    /// Its frames up to 0.15 s past `off` (session s): a note cut short.
    pub fn cut(&self, off: f64) -> Sound {
        let last = ((off + TAIL) * SR / stem::HOPS as f64).floor() as usize;
        let n = (last + 1).saturating_sub(self.f0).min(self.e.len());
        Sound { f0: self.f0, e: self.e[..n].to_vec() }
    }
}

/// What a round brought of one source: the notes settled now, and the
/// stream's own for now (in place of the round before's); of a note
/// instruments' source also per settled note its portrait, its level and the
/// settled note it is a harmonic of, and per note (settled, then own) its sound.
#[derive(Default, Debug)]
pub struct Round {
    pub settled: Vec<LNote>,
    pub own: Vec<LNote>,
    pub like: Vec<(PRow, f64, Option<usize>)>,
    pub sound: Vec<Sound>,
}

/// Note `n`'s sound frame by frame (its bands `b` measured from session
/// sample `s_a`).
fn sound_of(n: &SNote, b: &Bands, s_a: usize, norm: f64) -> Sound {
    let mut first = None;
    let mut e = Vec::new();
    for k in 0..b.len() {
        let t = b.t(k);
        if t >= n.on && t <= n.off + TAIL {
            first.get_or_insert(k);
            e.push((norm * (0..8).map(|h| 0.5 * (b.el[k][h] as f64 + b.er[k][h] as f64)).sum::<f64>()) as f32);
        }
    }
    Sound { f0: (s_a + stem::HOPS / 2) / stem::HOPS + b.j0 + first.unwrap_or(0), e }
}

/// One source's notes as the stream comes.
pub struct Track {
    /// The run's first session sample: the grid's origin.
    o: usize,
    /// The source, session sample `a0` first: whole below `afin`, the last
    /// segment's own after it.
    l: Vec<f32>,
    r: Vec<f32>,
    a0: usize,
    afin: usize,
    /// Mono at the network's rate, whole, run sample `x0` first.
    x: Vec<f32>,
    x0: usize,
    /// The network's pictures, frame `p0` first: of whole windows below
    /// `pfin`, then of the others (made again each round).
    note: Vec<f32>,
    onset: Vec<f32>,
    contour: Vec<f32>,
    p0: usize,
    pfin: usize,
    /// The notes starting below this frame are settled; the settled ones that
    /// reach past it (key, first frame, end), for their fragments.
    c: usize,
    done: Vec<(u8, usize, usize)>,
    /// The onsets inferred where a key's activation jumps (as a file's),
    /// against the strongest onset and jump of the whole pictures so far.
    infer: bool,
    top: f64,
    mx: f64,
    /// The source came on since the last round.
    fresh: bool,
    /// A note instruments' source: its notes come with what they are like.
    inst: bool,
}

/// The items of `v` that `keep` says.
fn pick<T>(v: Vec<T>, keep: &[bool]) -> Vec<T> {
    v.into_iter().zip(keep).filter(|(_, k)| **k).map(|(x, _)| x).collect()
}

impl Track {
    fn new(o: usize) -> Track {
        Track {
            o,
            l: Vec::new(),
            r: Vec::new(),
            a0: o,
            afin: o,
            x: Vec::new(),
            x0: 0,
            note: Vec::new(),
            onset: Vec::new(),
            contour: Vec::new(),
            p0: 0,
            pfin: 0,
            c: 0,
            done: Vec::new(),
            infer: true,
            top: f64::NEG_INFINITY,
            mx: 0.0,
            fresh: false,
            inst: false,
        }
    }

    /// The source in up to session sample `p`, whole below `f` (`read`: its
    /// channel from a session sample on): the last segment's own goes, what
    /// is whole now comes once, the new own after it.
    fn push(&mut self, read: &dyn Fn(usize, usize, &mut [f32]), f: usize, p: usize) {
        let k = self.afin - self.a0;
        self.l.truncate(k);
        self.r.truncate(k);
        let p = p.max(f);
        if p > self.afin {
            let n = p - self.afin;
            let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
            read(0, self.afin, &mut l);
            read(1, self.afin, &mut r);
            self.l.extend_from_slice(&l);
            self.r.extend_from_slice(&r);
        }
        self.afin = self.afin.max(f);
        self.fresh = true;
    }

    /// Run sample i of the source in mono (as `bp::to22` makes it), the sound
    /// ending at session sample `end`.
    fn mono(&self, i: usize, end: usize) -> Option<f32> {
        let s = self.o + i;
        (s >= self.a0 && s < end).then(|| {
            let k = s - self.a0;
            (self.l[k] + self.r[k]) * 0.5
        })
    }

    /// The pictures from frame `at` on go.
    fn cut(&mut self, at: usize) {
        let k = at.saturating_sub(self.p0);
        self.note.truncate(k * KEYS);
        self.onset.truncate(k * KEYS);
        self.contour.truncate(k * BINS);
    }

    /// The pictures up to what is in: the windows of whole sound not run yet,
    /// once; the others again, the grid's next one over what is there so far.
    fn pictures(&mut self, net: &mut dyn Windows) -> Result<(), String> {
        self.cut(self.pfin);
        let h = bp::halfband32();
        let (wend, end) = (self.afin, self.a0 + self.l.len());
        // at the network's rate: whole where every sample it reads is whole, then
        // the rest as of the sound ending where it is in so far
        let wrel = wend - self.o;
        let xf = if wrel > 20 { (wrel - 21) / 2 + 1 } else { 0 };
        let xfin = self.x0 + self.x.len();
        let more: Vec<f32> = (xfin..xf.max(xfin)).map(|o| bp::halve_at(&h, |c| self.mono(c, wend), o)).collect();
        self.x.extend_from_slice(&more);
        let xf = self.x0 + self.x.len();
        let xp = (end - self.o).div_ceil(2).max(xf);
        let own: Vec<f32> = (xf..xp).map(|o| bp::halve_at(&h, |c| self.mono(c, end), o)).collect();
        let x_at = |i: isize| -> f32 {
            if i < 0 {
                return 0.0;
            }
            let i = i as usize;
            if i < xf {
                i.checked_sub(self.x0).map_or(0.0, |k| self.x[k])
            } else if i < xp {
                own[i - xf]
            } else {
                0.0
            }
        };
        // the windows: the whole ones not run yet, then the others up to the
        // last one a file of this sound would have
        let w0 = self.pfin / KEEP;
        let ww = if xf + OLAP / 2 >= N_WIN { (xf + OLAP / 2 - N_WIN) / HOP + 1 } else { 0 }.max(w0);
        let we = (OLAP / 2 + xp).div_ceil(HOP).max(ww);
        if we > w0 {
            let nb = we - w0;
            let mut input = vec![0f32; nb * N_WIN];
            for (b, v) in input.chunks_exact_mut(N_WIN).enumerate() {
                let s = ((w0 + b) * HOP) as isize - (OLAP / 2) as isize;
                for (t, y) in v.iter_mut().enumerate() {
                    *y = x_at(s + t as isize);
                }
            }
            let got = net.windows(input, nb)?;
            for b in 0..nb {
                let w = w0 + b;
                // a window over sound not whole yet: its frames inside the sound so far, as a file's last
                let n = if w < ww { KEEP } else { (0..KEEP).take_while(|&j| bp::frame_sample(w * KEEP + j) < xp).count() };
                for ((src, dst), width) in got.iter().zip([&mut self.note, &mut self.onset, &mut self.contour]).zip([KEYS, KEYS, BINS]) {
                    let win = &src[b * WIN_FRAMES * width..(b + 1) * WIN_FRAMES * width];
                    dst.extend_from_slice(&bp::kept(win, width)[..n * width]);
                }
            }
        }
        self.pfin = ww * KEEP;
        Ok(())
    }

    /// The pictures decoded from two seconds before the first note not settled
    /// (a window's first frame: the transcript's times are then the run's less
    /// that frame's), the onsets inferred as a file's against the strongest the
    /// run has had: that frame, and the notes from the first not settled on
    /// that are not a settled one's fragments.
    fn decode(&mut self) -> Option<(usize, bp::Transcript)> {
        let pend = self.p0 + self.note.len() / KEYS;
        let a = (self.c.saturating_sub(BACK) / KEEP * KEEP).max(self.p0);
        if pend < a + 2 {
            return None;
        }
        let (n, k) = (pend - a, a - self.p0);
        let frames: Vec<f64> = self.note[k * KEYS..].iter().map(|&v| v as f64).collect();
        let mut on: Vec<f64> = self.onset[k * KEYS..].iter().map(|&v| v as f64).collect();
        let fd = bp::jumps(&frames, n);
        let whole = self.pfin.saturating_sub(a).min(n) * KEYS;
        self.top = on[..whole].iter().cloned().fold(self.top, f64::max);
        self.mx = fd[..whole].iter().cloned().fold(self.mx, f64::max);
        let top = on.iter().cloned().fold(self.top, f64::max);
        let mx = fd.iter().cloned().fold(self.mx, f64::max);
        if self.infer && mx > 0.0 {
            for (v, d) in on.iter_mut().zip(&fd) {
                *v = v.max(top * d / mx);
            }
        }
        let post = bp::Post {
            frames: n,
            note: self.note[k * KEYS..].to_vec(),
            onset: on.iter().map(|&v| v as f32).collect(),
            contour: self.contour[k * BINS..].to_vec(),
        };
        let tr = bp::transcribe(post, &bp::Params { infer_onsets: false, ..Default::default() });
        let keep: Vec<bool> = tr
            .notes
            .iter()
            .map(|nt| {
                let g = a + nt.i0;
                g >= self.c && !self.done.iter().any(|&(m, s, e)| m == nt.midi && s <= g && g < e)
            })
            .collect();
        let bp::Transcript { post, notes, tracks, track_peaks } = tr;
        Some((a, bp::Transcript { post, notes: pick(notes, &keep), tracks: pick(tracks, &keep), track_peaks: pick(track_peaks, &keep) }))
    }

    /// The notes found (first frame, end, key, in the run's frames): the ones
    /// that are settled now start below the frame returned (the new edge);
    /// those reaching past it are kept for their fragments.
    fn settle(&mut self, spans: &[(usize, usize, u8)]) -> usize {
        let open = spans.iter().filter(|s| s.1 + INSIDE > self.pfin && s.0 + LONGEST > self.pfin).map(|s| s.0).min();
        let lim = open.unwrap_or(usize::MAX).min(self.pfin.saturating_sub(INSIDE));
        let c = self.c.max(lim.saturating_sub(BEFORE_OPEN));
        for s in spans.iter().filter(|s| s.0 < c) {
            self.done.push((s.2, s.0, s.1));
        }
        self.c = c;
        self.done.retain(|d| d.2 > c);
        c
    }

    /// What the next round does not read goes: the pictures before its
    /// decoding's first frame, the mono before the next whole window's first
    /// sample, the source before what both of those read.
    fn trim(&mut self) {
        let a = (self.c.saturating_sub(BACK) / KEEP * KEEP).max(self.p0);
        let k = (a - self.p0).min(self.note.len() / KEYS);
        if k > 0 {
            self.note.drain(..k * KEYS);
            self.onset.drain(..k * KEYS);
            self.contour.drain(..k * BINS);
            self.p0 += k;
        }
        let xa = (self.pfin / KEEP * HOP).saturating_sub(OLAP / 2);
        if xa > self.x0 + TRIM {
            let n = (xa - self.x0).min(self.x.len());
            self.x.drain(..n);
            self.x0 += n;
        }
        let xf = self.x0 + self.x.len();
        let s = (self.o + 2 * bp::frame_sample(self.p0)).min(self.o + (2 * xf).saturating_sub(20));
        if s > self.a0 + TRIM {
            let n = (s - self.a0).min(self.afin - self.a0);
            self.l.drain(..n);
            self.r.drain(..n);
            self.a0 += n;
        }
    }

    /// A round: the pictures, the notes decoded and measured (`mix_ref`: the
    /// song's mix's loud level, for the weak ones), the settled and the own.
    fn round(&mut self, net: &mut dyn Windows, mix_ref: f64) -> Result<Round, String> {
        self.fresh = false;
        self.pictures(net)?;
        let mut out = Round::default();
        if let Some((a, tr)) = self.decode() {
            // the transcript's times are from frame a's sample: its sound from there
            let s_a = self.o + 2 * bp::frame_sample(a);
            debug_assert!(s_a >= self.a0, "the sound from {s_a} is kept from {}", self.a0);
            let from = s_a.saturating_sub(self.a0).min(self.l.len());
            let sn = stem::notes_of(tr, &self.l[from..], &self.r[from..], mix_ref);
            let por = stem::portrait(&sn);
            let spans: Vec<(usize, usize, u8)> = sn.notes.iter().map(|nt| (a + nt.i0, a + nt.i1, nt.midi)).collect();
            let c = self.settle(&spans);
            let t_a = s_a as f64 / SR;
            let (mut settled, mut own) = (Vec::new(), Vec::new());
            for (i, nt) in sn.notes.iter().enumerate().filter(|(_, nt)| !nt.weak) {
                let root = nt.ghost.unwrap_or(i);
                let pan = if por.pan[root].is_finite() { por.pan[root] as f32 } else { 0.0 };
                let n = LNote { on: t_a + nt.on, off: t_a + nt.off, key: nt.pitch as f32, lvl: nt.lvl as f32, pan, ghost: nt.ghost.is_some() };
                if spans[i].0 < c {
                    out.settled.push(n);
                    settled.push(i);
                } else {
                    out.own.push(n);
                    own.push(i);
                }
            }
            if self.inst {
                let norm = stem::norm();
                let at: HashMap<usize, usize> = settled.iter().enumerate().map(|(k, &i)| (i, k)).collect();
                out.like = settled.iter().map(|&i| (por.row(i), sn.notes[i].lvl, sn.notes[i].ghost.and_then(|g| at.get(&g).copied()))).collect();
                out.sound = settled.iter().chain(&own).map(|&i| sound_of(&sn.notes[i], &sn.bands[i], s_a, norm)).collect();
            }
        }
        self.trim();
        Ok(out)
    }
}

/// The sources' notes as the stream comes.
pub struct Notes {
    tracks: [Track; 5],
}

impl Notes {
    /// From session sample `o`: the first of a run of segments.
    pub fn new(o: usize) -> Notes {
        Notes {
            tracks: std::array::from_fn(|p| {
                let mut t = Track::new(o);
                t.inst = p >= INST;
                t
            }),
        }
    }

    /// The sources are whole up to here.
    pub fn end(&self) -> usize {
        self.tracks[0].afin
    }

    /// What the separation has of the sources now: whole below `f`, the last
    /// segment's own up to `p`.
    pub fn push(&mut self, sep: &Sep, f: usize, p: usize) {
        for (t, &src) in self.tracks.iter_mut().zip(&SOURCES) {
            t.push(&|ch, from, out: &mut [f32]| sep.read(src * 2 + ch, from as isize, out), f, p);
        }
    }

    /// Something came since the last round.
    pub fn fresh(&self) -> bool {
        self.tracks[0].fresh
    }
}

/// A round of the notes, out of the work's lock: the sources' state and what
/// the round needs (the song's mix's loud level, dB).
pub struct Job {
    pub notes: Notes,
    pub mix_ref: f64,
}

impl Job {
    pub fn run(&mut self, net: &mut dyn Windows) -> Result<[Round; 5], String> {
        let mut out: [Round; 5] = Default::default();
        for (t, r) in self.notes.tracks.iter_mut().zip(out.iter_mut()) {
            *r = t.round(net, self.mix_ref)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::sep::tests::signal;
    use super::*;

    /// Stands in for the note network: in each frame of a window (the 2048
    /// samples about its place in the window, frame f at sample 256·f), the
    /// key whose pitch is strongest (MIDI 21…84, by Goertzel), unless the
    /// frame is silent: that key's note and contour at 0.9, its onset too
    /// where the frame before had another key or none.
    pub struct Hum;

    impl Windows for Hum {
        fn windows(&mut self, input: Vec<f32>, nb: usize) -> Result<[Vec<f32>; 3], String> {
            const N: usize = 2048;
            const NK: usize = 64;
            let win: Vec<f32> = (0..N).map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / N as f64).cos()) as f32).collect();
            let coef: Vec<f32> = (0..NK)
                .map(|k| {
                    let f = 440.0 * 2f64.powf((bp::MIDI0 as f64 + k as f64 - 69.0) / 12.0);
                    (2.0 * (std::f64::consts::TAU * f / bp::SR as f64).cos()) as f32
                })
                .collect();
            let mut got = [vec![0f32; nb * WIN_FRAMES * KEYS], vec![0f32; nb * WIN_FRAMES * KEYS], vec![0f32; nb * WIN_FRAMES * BINS]];
            for w in 0..nb {
                let x = &input[w * N_WIN..(w + 1) * N_WIN];
                let mut prev = None;
                for f in 0..WIN_FRAMES {
                    let c0 = (f * bp::FFT_HOP) as isize - (N / 2) as isize;
                    let seg: Vec<f32> = (0..N)
                        .map(|i| {
                            let s = c0 + i as isize;
                            if s >= 0 && (s as usize) < N_WIN { x[s as usize] * win[i] } else { 0.0 }
                        })
                        .collect();
                    let e = seg.iter().map(|v| v * v).sum::<f32>() / N as f32;
                    let mut best = (0f32, 0usize);
                    for (k, &cf) in coef.iter().enumerate() {
                        let (mut s1, mut s2) = (0f32, 0f32);
                        for &v in &seg {
                            let s0 = v + cf * s1 - s2;
                            s2 = s1;
                            s1 = s0;
                        }
                        let p = s1 * s1 + s2 * s2 - cf * s1 * s2;
                        if p > best.0 {
                            best = (p, k);
                        }
                    }
                    let key = (e > 1e-5).then_some(best.1);
                    if let Some(k) = key {
                        let at = w * WIN_FRAMES + f;
                        got[0][at * KEYS + k] = 0.9;
                        got[2][at * BINS + 3 * k + 1] = 0.9;
                        if prev != key {
                            got[1][at * KEYS + k] = 0.9;
                        }
                    }
                    prev = key;
                }
            }
            Ok(got)
        }
    }

    fn feed(t: &mut Track, l: &[f32], r: &[f32], f: usize, p: usize) {
        let read = |ch: usize, from: usize, out: &mut [f32]| {
            let v = if ch == 0 { l } else { r };
            for (i, o) in out.iter_mut().enumerate() {
                *o = v.get(from + i).copied().unwrap_or(0.0);
            }
        };
        t.push(&read, f, p);
    }

    fn frames_of(t: &Track, k: usize) -> (&[f32], &[f32], &[f32]) {
        let j = k - t.p0;
        (&t.note[j * KEYS..(j + 1) * KEYS], &t.onset[j * KEYS..(j + 1) * KEYS], &t.contour[j * BINS..(j + 1) * BINS])
    }

    fn file_frame(p: &bp::Post, k: usize) -> (&[f32], &[f32], &[f32]) {
        (&p.note[k * KEYS..(k + 1) * KEYS], &p.onset[k * KEYS..(k + 1) * KEYS], &p.contour[k * BINS..(k + 1) * BINS])
    }

    /// A source coming as a stream, in blocks of any length with the last
    /// segment's own sound behind what is whole: the pictures of whole sound
    /// are a file's of the whole source, to the bit, and the others a file's
    /// of the source ending where it is in so far.
    #[test]
    fn the_pictures_as_the_stream_comes_are_a_files() {
        let n = 44_100 * 12 + 777;
        let (l, r) = signal(n);
        let want = bp::post_of(&mut Hum, &bp::to22(&l, &r)).unwrap();
        let mut t = Track::new(0);
        let (mut f, mut step) = (0usize, 1usize);
        while f < n {
            let p = (f + 30_000 + step * 61_031 % 150_000).min(n);
            f = p.saturating_sub(step * 7_919 % 86_000).max(f);
            if step % 5 == 0 {
                f = p;
            }
            feed(&mut t, &l, &r, f, p);
            t.pictures(&mut Hum).unwrap();
            let cut = bp::post_of(&mut Hum, &bp::to22(&l[..p], &r[..p])).unwrap();
            let pend = t.p0 + t.note.len() / KEYS;
            assert_eq!(pend, cut.frames, "the frames inside {p} samples");
            for k in t.pfin..pend {
                assert!(frames_of(&t, k) == file_frame(&cut, k), "step {step}: frame {k} of the sound so far");
            }
            step += 1;
        }
        assert!(t.pfin + 3 * KEEP > want.frames, "{} of {} frames whole", t.pfin, want.frames);
        for k in 0..t.pfin {
            assert!(frames_of(&t, k) == file_frame(&want, k), "whole frame {k}");
        }
    }

    /// Made-up pictures: notes struck and held, some with no onset (the
    /// melodia trick's), some carried over short gaps, a few at once on keys
    /// apart.
    fn made_up(n: usize) -> bp::Post {
        let mut p = bp::Post { frames: n, note: vec![0.0; n * KEYS], onset: vec![0.0; n * KEYS], contour: vec![0.0; n * BINS] };
        let mut seed = 4242u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let mut free = [0usize; KEYS];
        let mut t = 5;
        while t + 20 < n {
            let key = 20 + (rnd() * 50.0) as usize;
            let len = 6 + (rnd() * rnd() * 400.0) as usize;
            if free[key - 1..=key + 1].iter().all(|&f| f <= t) {
                let v = 0.45 + 0.45 * rnd();
                let mut end = (t + len).min(n);
                for i in t..end {
                    p.note[i * KEYS + key] = v * (0.9 + 0.1 * (i as f32 * 0.37).sin());
                }
                if rnd() < 0.7 {
                    p.onset[t * KEYS + key] = 0.6 + 0.4 * rnd();
                    p.onset[(t + 1) * KEYS + key] = 0.3;
                }
                if rnd() < 0.2 {
                    // carried on after a gap of a few frames, no onset
                    let s = end + 2 + (rnd() * 6.0) as usize;
                    let e = (s + 10 + (rnd() * 80.0) as usize).min(n);
                    for i in s.min(e)..e {
                        p.note[i * KEYS + key] = v;
                    }
                    end = e;
                }
                for f in &mut free[key - 1..=key + 1] {
                    *f = end + 2;
                }
            }
            t += 1 + (rnd() * 40.0) as usize;
        }
        p
    }

    /// The pictures of frames [from, to) put in after the whole ones (`pfin`
    /// on to `to` whole when `whole`), else made a little different (the last
    /// segment's own sound is not what the next makes of it).
    fn put(t: &mut Track, p: &bp::Post, pfin: usize, pend: usize, salt: usize) {
        t.cut(t.pfin);
        let from = t.p0 + t.note.len() / KEYS;
        for k in from..pend {
            let shade = if k < pfin { 1.0 } else { 0.6 + 0.3 * (((k + salt) * 7_919) % 13) as f32 / 13.0 };
            t.note.extend(p.note[k * KEYS..(k + 1) * KEYS].iter().map(|v| v * shade));
            t.onset.extend(p.onset[k * KEYS..(k + 1) * KEYS].iter().map(|v| v * shade));
            t.contour.extend_from_slice(&p.contour[k * BINS..(k + 1) * BINS]);
        }
        t.pfin = pfin;
    }

    /// Each note's portrait kept as a row and put together again with the
    /// others is the portrait they were measured in, the diffuseness made again
    /// over them all theirs (what a live stream's split weighs is a file's).
    #[test]
    fn rows_put_together_are_the_portrait() {
        let n = 900;
        let tr = bp::transcribe(made_up(n), &bp::Params { infer_onsets: false, ..Default::default() });
        let (l, r) = signal(2 * bp::frame_sample(n) + 8192);
        let sn = stem::notes_of(tr, &l, &r, -20.0);
        let por = stem::portrait(&sn);
        assert!(sn.notes.len() > 20, "{} notes", sn.notes.len());
        let rows: Vec<PRow> = (0..sn.notes.len()).map(|i| por.row(i)).collect();
        let mut back = stem::Portrait::from_rows(&rows);
        let used: Vec<usize> = (0..sn.notes.len()).filter(|&i| sn.notes[i].used()).collect();
        stem::diffuse(&mut back, &used);
        assert!(por.dres.iter().any(|v| v.is_finite()));
        for i in 0..rows.len() {
            assert_eq!(format!("{:?}", back.row(i)), format!("{:?}", por.row(i)), "note {i}");
        }
    }

    /// The pictures decoded as they come, the notes settled round by round:
    /// each settled note is one of the pictures decoded whole (frames, key),
    /// none twice — the frames not whole yet different each round — and with
    /// the last round's own (all whole by then) they are all of them.
    #[test]
    fn decoding_as_the_pictures_come_is_decoding_them_whole() {
        let n = 4_000;
        let p = made_up(n);
        let whole = bp::transcribe(
            bp::Post { frames: n, note: p.note.clone(), onset: p.onset.clone(), contour: p.contour.clone() },
            &bp::Params { infer_onsets: false, ..Default::default() },
        );
        let mut want: Vec<(usize, usize, u8)> = whole.notes.iter().map(|x| (x.i0, x.i1, x.midi)).collect();
        want.sort();
        assert!(want.len() > 80, "{} notes", want.len());
        let mut t = Track::new(0);
        t.infer = false;
        let (mut settled, mut own) = (Vec::new(), Vec::new());
        let (mut pfin, mut step) = (0usize, 1usize);
        while pfin < n || step < 3 {
            pfin = (pfin + 250 + step * 97 % 400).min(n);
            let pend = (pfin + step * 53 % 200).min(n);
            put(&mut t, &p, pfin, pend, step);
            if let Some((a, tr)) = t.decode() {
                let spans: Vec<(usize, usize, u8)> = tr.notes.iter().map(|x| (a + x.i0, a + x.i1, x.midi)).collect();
                let c = t.settle(&spans);
                settled.extend(spans.iter().copied().filter(|s| s.0 < c));
                own = spans.into_iter().filter(|s| s.0 >= c).collect();
            }
            t.trim();
            step += 1;
        }
        let mut s = settled.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), settled.len(), "a note settled twice");
        let stray: Vec<_> = settled.iter().filter(|x| !want.contains(x)).collect();
        assert!(stray.is_empty(), "settled but not in the whole: {stray:?}");
        let mut all = settled;
        all.extend(own);
        all.sort();
        assert_eq!(all, want);
    }
}
