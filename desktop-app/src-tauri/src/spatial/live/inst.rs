//! A note instrument's source split into its instruments on a live stream
//! (TASK-27, stage 3; AURA-VIS-SPEC-INSTRUMENTS.md F1): as a file's are, by
//! their notes (`notes::cluster` over the notes' portraits, then the bleed
//! rule across the sources), but over the song's notes so far, and again as
//! more come.
//!
//! What a split says is held to what the splits before said by the notes
//! themselves: a cluster is the instrument most of its notes already went to,
//! however the mixture numbers its clusters this time. A cluster that reaches
//! over instruments a split took for one gives its new notes to the nearest
//! of them (no instrument found goes dark because one split merged it). A
//! cluster no instrument holds takes an instrument no other cluster holds this
//! time when a quarter of its notes went to it (a split drawn otherwise this
//! time is no new instrument); one with none is a new instrument only once it
//! is seen two splits running — until then its notes go to the nearest one —, and
//! the newborn takes of its notes only those not heard yet: what was heard
//! stays where it was seen. Every other note goes to an instrument once.
//!
//! The first split takes the source's places as its instruments: each place
//! is taken over by the cluster nearest to it by pan (within `TAKE_PAN`; the
//! nearest pairs first), the others are born at once.

use std::collections::HashMap;

use super::super::notes::cluster;
use super::super::notes::objects::Found;
use super::super::notes::stem::{self, PRow, Portrait, SNote};
use super::notes::LNote;

/// The note instruments' sources are split into their instruments (else
/// their notes stay on their places by pan, as the bass's and the voice's).
pub const SPLIT: bool = true;
/// A source is split once it has this many strong notes in the song: below
/// it a file's split gives one instrument anyway.
pub const SPLIT_FROM: usize = 40;
/// It is split again once its strong notes have grown by this many, or by
/// this share of them (a long song's notes are split less often).
const AGAIN_N: usize = 8;
const AGAIN_SHARE: f64 = 0.2;
/// A split weighs a source's latest strong notes, this many at most (about
/// five minutes of a busy part: the time a split takes stays bounded).
pub const WINDOW: usize = 2000;
/// A cluster is an instrument's when at least this share of its notes given
/// out so far went to that instrument …
const MATCH: f64 = 0.5;
/// … and it reaches over another one too when that one holds this share.
const SPAN: f64 = 0.25;
/// A cluster no instrument holds is a new instrument once seen this many
/// splits running (seen again: it has half the notes it had).
const NEW_SPLITS: u32 = 2;
/// The nearest instrument to a note: pan over this plus pitch over this
/// (semitones).
const NEAR_PAN: f32 = 0.25;
const NEAR_KEY: f32 = 12.0;
/// A newborn takes a place of its source gone silent this near by pan.
pub const TAKE_PAN: f32 = 0.3;

/// A note given to nobody: its cluster went in the bleed rule, or it has none.
pub const NOBODY: u32 = 0;

/// A song's note of a source, for its split.
pub struct In<'a> {
    pub id: u64,
    pub n: &'a LNote,
    pub row: &'a PRow,
    /// Its level as its instrument's is measured (a file's `SNote::lvl`).
    pub lvl: f64,
    /// The note it is a harmonic of (a ghost's).
    pub parent: Option<u64>,
}

/// A source's notes split as a file's are: their portraits (the diffuseness
/// against the song's own), `cluster::instruments`, and each instrument's
/// notes for the bleed rule across the sources (`Found`: `main` and `all`
/// index `notes`). Their seconds from `t0` (the song's start).
pub fn split(source: usize, notes: &[In], t0: f64) -> Vec<Found> {
    let at: HashMap<u64, usize> = notes.iter().enumerate().map(|(i, x)| (x.id, i)).collect();
    let sn: Vec<SNote> = notes
        .iter()
        .map(|x| {
            let ghost = x.parent.and_then(|p| at.get(&p).copied());
            SNote {
                on: x.n.on - t0,
                off: x.n.off - t0,
                pitch: x.row.pitch,
                midi: x.row.pitch.round().clamp(0.0, 127.0) as u8,
                amp: 0.0,
                i0: 0,
                i1: 0,
                lvl: x.lvl,
                // a harmonic of a note not here: neither its own nor any instrument's
                weak: x.n.ghost && ghost.is_none(),
                ghost,
                parts: 1,
            }
        })
        .collect();
    let mut por = Portrait::from_rows(notes.iter().map(|x| x.row));
    let used: Vec<usize> = (0..sn.len()).filter(|&i| sn[i].used()).collect();
    stem::diffuse(&mut por, &used);
    let take: Vec<bool> = sn.iter().map(|e| e.used()).collect();
    let lab = cluster::instruments(&por, &sn, &take, true);
    (0..lab.iter().copied().max().unwrap_or(-1) + 1)
        .filter_map(|c| {
            let main: Vec<usize> = (0..sn.len()).filter(|&i| lab[i] == c && take[i]).collect();
            let all: Vec<usize> = (0..sn.len()).filter(|&i| lab[i] == c && !sn[i].weak).collect();
            if main.is_empty() {
                return None;
            }
            let w: Vec<f64> = main.iter().map(|&i| 10f64.powf(sn[i].lvl / 10.0)).collect();
            let pitch = main.iter().zip(&w).map(|(&i, w)| sn[i].pitch * w).sum::<f64>() / w.iter().sum::<f64>();
            let shape = main.iter().map(|&i| (sn[i].on, sn[i].off, sn[i].pitch, sn[i].lvl)).collect();
            Some(Found { stem: source, main, all, pitch, shape })
        })
        .collect()
}

/// An instrument of the song's source: its object's id, where it sits and
/// its pitch (where the notes nearest to it go).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fig {
    pub id: u32,
    pub x: f32,
    pub key: f32,
}

/// The nearest of `figs` to a note at `pan` of `key`.
pub fn nearest(figs: &[Fig], pan: f32, key: f32) -> Option<u32> {
    let d = |f: &Fig| (pan - f.x).abs() / NEAR_PAN + (key - f.key).abs() / NEAR_KEY;
    figs.iter().min_by(|a, b| d(a).total_cmp(&d(b))).map(|f| f.id)
}

/// Where a note goes now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum To {
    Fig(u32),
    /// The `born`-th cluster born now.
    Born(usize),
}

/// What a split decided: the notes that go somewhere now (index into the
/// notes), and the clusters born now (the notes of each, `main` first).
#[derive(Default, Debug)]
pub struct Outcome {
    pub give: Vec<(usize, To)>,
    pub born: Vec<Vec<usize>>,
    /// Of the clusters born now, the instrument whose notes most of it took
    /// over (the first split: the place it takes over).
    pub over: Vec<Option<u32>>,
}

/// A cluster seen that no instrument holds: its notes, how many splits running.
struct Cand {
    notes: Vec<u64>,
    seen: u32,
}

/// What the song holds of a source's split.
#[derive(Default)]
pub struct Split {
    /// The source has been split: its notes go to its instruments.
    pub on: bool,
    /// Its strong notes at the last split.
    pub at: usize,
    cands: Vec<Cand>,
    /// For the measures: the splits, and those with a cluster over two
    /// instruments or more.
    pub splits: u32,
    pub merges: u32,
}

impl Split {
    /// The source is split now: first with `SPLIT_FROM` strong notes, then
    /// whenever they have grown enough.
    pub fn due(&self, strong: usize, split: bool) -> bool {
        if !self.on {
            return split && strong >= SPLIT_FROM;
        }
        strong >= self.at + AGAIN_N.max((self.at as f64 * AGAIN_SHARE).ceil() as usize)
    }

    /// One split's clusters (`found`, `kept`: not gone in the bleed rule)
    /// against the instruments so far (`figs`; on the first split the
    /// source's places), the notes' instruments now (`to`, an object id or
    /// none), the place heard (`heard`, session s).
    pub fn assign(&mut self, found: &[Found], kept: &[bool], notes: &[In], to: &[Option<u32>], figs: &[Fig], heard: f64) -> Outcome {
        let first = !self.on;
        self.on = true;
        self.splits += 1;
        let at: HashMap<u32, usize> = figs.iter().enumerate().map(|(k, f)| (f.id, k)).collect();
        let cl: Vec<&Found> = found.iter().zip(kept).filter(|(_, k)| **k).map(|(f, _)| f).collect();
        // each cluster's notes given out so far, by instrument
        let held: Vec<Vec<usize>> = cl
            .iter()
            .map(|c| {
                let mut v = vec![0usize; figs.len()];
                for &i in &c.all {
                    if let Some(&k) = to[i].and_then(|id| at.get(&id)) {
                        v[k] += 1;
                    }
                }
                v
            })
            .collect();
        let given: Vec<usize> = held.iter().map(|v| v.iter().sum()).collect();
        let (mut of_c, mut of_k) = (vec![None; cl.len()], vec![None; figs.len()]);
        if first {
            // a place to the cluster nearest to it by pan, one to one, the nearest first
            let xs: Vec<f32> = cl.iter().map(|c| describe(notes, &c.main).0).collect();
            let mut pairs: Vec<(f32, usize, usize)> = (0..cl.len())
                .flat_map(|c| (0..figs.len()).map(move |k| (c, k)))
                .map(|(c, k)| ((xs[c] - figs[k].x).abs(), c, k))
                .filter(|p| p.0 <= TAKE_PAN)
                .collect();
            pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            for (_, c, k) in pairs {
                if of_c[c].is_none() && of_k[k].is_none() {
                    of_c[c] = Some(k);
                    of_k[k] = Some(c);
                }
            }
        } else {
            // a cluster to the instrument that holds most of its notes, one to one, the largest first
            let mut pairs: Vec<(usize, usize, usize)> =
                (0..cl.len()).flat_map(|c| (0..figs.len()).map(move |k| (c, k))).map(|(c, k)| (held[c][k], c, k)).filter(|p| p.0 > 0).collect();
            pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            for &(n, c, k) in &pairs {
                if of_c[c].is_none() && of_k[k].is_none() && n as f64 >= MATCH * given[c] as f64 {
                    of_c[c] = Some(k);
                    of_k[k] = Some(c);
                }
            }
            // then one none holds to an instrument left without a cluster, holding a quarter of its notes
            for (n, c, k) in pairs {
                if of_c[c].is_none() && of_k[k].is_none() && n as f64 >= SPAN * given[c] as f64 {
                    of_c[c] = Some(k);
                    of_k[k] = Some(c);
                }
            }
        }
        let mut out = Outcome::default();
        let mut cands = Vec::new();
        let mut merged = false;
        let mut born_of: Vec<Option<usize>> = vec![None; cl.len()];
        for (c, f) in cl.iter().enumerate() {
            let ids: Vec<u64> = f.all.iter().map(|&i| notes[i].id).collect();
            match of_c[c] {
                Some(k) if !first => {
                    // the instruments it reaches over that no other cluster holds
                    let near: Vec<Fig> = (0..figs.len())
                        .filter(|&g| g == k || (of_k[g].is_none() && held[c][g] as f64 >= SPAN * given[c] as f64))
                        .map(|g| figs[g])
                        .collect();
                    merged |= near.len() > 1;
                    for &i in f.all.iter().filter(|&&i| to[i].is_none()) {
                        if let Some(id) = nearest(&near, notes[i].n.pan, notes[i].row.pitch as f32) {
                            out.give.push((i, To::Fig(id)));
                        }
                    }
                }
                _ => {
                    let seen = self
                        .cands
                        .iter()
                        .find(|k| k.notes.iter().filter(|x| ids.contains(x)).count() * 2 >= k.notes.len())
                        .map_or(1, |k| k.seen + 1);
                    if first || seen >= NEW_SPLITS {
                        born_of[c] = Some(out.born.len());
                        let mut v = f.main.clone();
                        v.extend(f.all.iter().filter(|i| !f.main.contains(i)));
                        out.born.push(v);
                        out.over.push(of_c[c].map(|k| figs[k].id));
                    } else {
                        cands.push(Cand { notes: ids, seen });
                        for &i in f.all.iter().filter(|&&i| to[i].is_none()) {
                            if let Some(id) = nearest(figs, notes[i].n.pan, notes[i].row.pitch as f32) {
                                out.give.push((i, To::Fig(id)));
                            }
                        }
                    }
                }
            }
        }
        // the clusters born now (on the first split: every cluster) take their notes not heard yet,
        // and the ones no instrument had
        for (c, f) in cl.iter().enumerate() {
            let Some(b) = born_of[c] else { continue };
            for &i in &f.all {
                if to[i].is_none() || notes[i].n.on > heard {
                    out.give.push((i, To::Born(b)));
                }
            }
        }
        // the notes of no cluster (the bleed rule's, a lone harmonic) go to nobody; on the first
        // split also those not heard yet that a place had
        let mut inside = vec![false; notes.len()];
        for f in &cl {
            for &i in &f.all {
                inside[i] = true;
            }
        }
        for i in (0..notes.len()).filter(|&i| !inside[i] && (to[i].is_none() || (first && notes[i].n.on > heard))) {
            out.give.push((i, To::Fig(NOBODY)));
        }
        self.cands = cands;
        self.merges += merged as u32;
        self.at = notes.iter().filter(|x| !x.n.ghost).count();
        out
    }
}

/// What a newborn instrument is like, from its notes (index into `notes`,
/// its strong ones first): where it sits (its strong notes' pan, by level),
/// how wide (how decorrelated they are, or how far apart: a file's), its
/// pitch (by level) and its harmonic profile's slope (their median: its
/// colour's brightness).
pub fn describe(notes: &[In], idx: &[usize]) -> (f32, f32, f64, f32) {
    let main: Vec<usize> = idx.iter().copied().filter(|&i| !notes[i].n.ghost).collect();
    let (mut num, mut den, mut pnum) = (0f64, 0f64, 0f64);
    for &i in &main {
        let w = 10f64.powf(notes[i].lvl / 10.0);
        pnum += w * notes[i].row.pitch;
        if notes[i].row.pan.is_finite() {
            num += w * notes[i].row.pan;
            den += w;
        }
    }
    let x = if den > 0.0 { num / den } else { 0.0 };
    let (mut cw, mut w, mut pw) = (0f64, 0f64, 0f64);
    for &i in &main {
        let wt = 10f64.powf(notes[i].lvl / 10.0);
        let r = notes[i].row;
        if r.coh.is_finite() && r.pan.is_finite() {
            cw += wt * r.coh;
            pw += wt * (r.pan - x).powi(2);
            w += wt;
        }
    }
    let width = if w > 0.0 { (1.0 - cw / w).max(2.0 * (pw / w).sqrt()).clamp(0.0, 1.0) } else { 0.0 };
    let tw: f64 = main.iter().map(|&i| 10f64.powf(notes[i].lvl / 10.0)).sum();
    let pitch = if tw > 0.0 { pnum / tw } else { f64::NAN };
    let mut tilt: Vec<f64> = main.iter().map(|&i| notes[i].row.tilt).filter(|v| v.is_finite()).collect();
    tilt.sort_by(|a, b| a.total_cmp(b));
    let tilt = match tilt.len() {
        0 => f32::NAN,
        n if n % 2 == 1 => tilt[n / 2] as f32,
        n => ((tilt[n / 2 - 1] + tilt[n / 2]) / 2.0) as f32,
    };
    (x as f32, width as f32, pitch, tilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lnote(on: f64, key: f32, pan: f32) -> LNote {
        LNote { on, off: on + 0.3, key, lvl: -20.0, pan, ghost: false }
    }

    /// Notes and their rows: (on, key, pan) each.
    fn made(v: &[(f64, f32, f32)]) -> (Vec<LNote>, Vec<PRow>) {
        let n: Vec<LNote> = v.iter().map(|&(on, key, pan)| lnote(on, key, pan)).collect();
        let r: Vec<PRow> = v.iter().map(|&(_, key, pan)| PRow { pitch: key as f64, pan: pan as f64, ..PRow::default() }).collect();
        (n, r)
    }

    fn ins<'a>(n: &'a [LNote], r: &'a [PRow]) -> Vec<In<'a>> {
        n.iter().zip(r).enumerate().map(|(i, (n, r))| In { id: 100 + i as u64, n, row: r, lvl: -20.0, parent: None }).collect()
    }

    fn found(main: Vec<usize>) -> Found {
        Found { stem: 4, all: main.clone(), main, pitch: 60.0, shape: Vec::new() }
    }

    fn to_of(out: &Outcome, i: usize) -> Option<To> {
        out.give.iter().rev().find(|g| g.0 == i).map(|g| g.1)
    }

    /// The first split: the cluster nearest to a place by pan takes it over
    /// (its notes not heard yet that another place had come with it), every
    /// cluster is born at once; later the same notes keep their instruments
    /// however the clusters are numbered, the new ones go to theirs, a
    /// cluster reaching over two instruments feeds both, and a notes' cluster
    /// none holds is born only on its second split running — taking of its
    /// notes only those not heard yet.
    #[test]
    fn a_split_holds_to_the_notes_instruments() {
        // 0–5 left (A), 6–11 right (B), 12–15 left again: notes at 1 s apart
        let v: Vec<(f64, f32, f32)> = (0..16).map(|i| (i as f64, if (6..12).contains(&i) { 64.0 } else { 60.0 }, if (6..12).contains(&i) { 0.6 } else { -0.6 })).collect();
        let (n, r) = made(&v);
        let x = ins(&n, &r);
        let places = [Fig { id: 7, x: -0.6, key: 60.0 }, Fig { id: 9, x: 0.6, key: 60.0 }];
        // by pan all went to the left place but two right ones (8, 9), which went to the right place
        let mut to: Vec<Option<u32>> = (0..16).map(|i| Some(if i == 8 || i == 9 { 9 } else { 7 })).collect();
        let mut s = Split::default();
        assert!(!s.due(39, true) && s.due(40, true) && !s.due(40, false));
        // one cluster of the lefts, one of the rights; heard at 7.5 s
        let a = found((0..6).chain(12..16).collect());
        let b = found((6..12).collect());
        let out = s.assign(&[a, b], &[true, true], &x, &to, &places, 7.5);
        assert_eq!(out.born.len(), 2, "every cluster of the first split is an instrument");
        assert_eq!(out.over, vec![Some(7), Some(9)], "each place to the cluster nearest by pan");
        // the rights not heard yet (8 … 11) go to the newborn; 6 and 7 (heard, on the left place) stay
        for i in 8..12 {
            assert_eq!(to_of(&out, i), Some(To::Born(1)), "note {i}");
        }
        assert_eq!(to_of(&out, 6), None);
        assert_eq!(to_of(&out, 7), None);
        // the song makes the newborns' objects: the lefts are 7, the rights 11
        for (i, t) in &out.give {
            to[*i] = Some(match t {
                To::Born(0) => 7,
                To::Born(_) => 11,
                To::Fig(id) => *id,
            });
        }
        // (the right place 9 taken over by the rights: here their instrument is 11, as on a new slot)
        let figs = [Fig { id: 7, x: -0.6, key: 60.0 }, Fig { id: 11, x: 0.6, key: 64.0 }];
        // more notes: 16–19 left, 20–23 right; the clusters come numbered the other way round
        let v2: Vec<(f64, f32, f32)> = v.iter().copied().chain((16..24).map(|i| (i as f64, if i < 20 { 60.0 } else { 64.0 }, if i < 20 { -0.6 } else { 0.6 }))).collect();
        let (n2, r2) = made(&v2);
        let x2 = ins(&n2, &r2);
        to.resize(24, None);
        let rights = found((6..12).chain(20..24).collect());
        let lefts = found((0..6).chain(12..20).collect());
        let out = s.assign(&[rights, lefts], &[true, true], &x2, &to, &figs, 15.5);
        assert!(out.born.is_empty());
        for i in 16..20 {
            assert_eq!(to_of(&out, i), Some(To::Fig(7)));
        }
        for i in 20..24 {
            assert_eq!(to_of(&out, i), Some(To::Fig(11)));
        }
        assert!(out.give.iter().all(|g| g.0 >= 16), "the notes given before keep their instruments: {:?}", out.give);
        for (i, t) in &out.give {
            if let To::Fig(id) = t {
                to[*i] = Some(*id);
            }
        }
        // one cluster of all (the split merged them): the new notes go to the nearest of the two
        let v3: Vec<(f64, f32, f32)> = v2.iter().copied().chain([(24.0, 60.0, -0.6), (25.0, 64.0, 0.6)]).collect();
        let (n3, r3) = made(&v3);
        let x3 = ins(&n3, &r3);
        to.resize(26, None);
        let out = s.assign(&[found((0..26).collect())], &[true], &x3, &to, &figs, 23.5);
        assert_eq!((to_of(&out, 24), to_of(&out, 25)), (Some(To::Fig(7)), Some(To::Fig(11))));
        assert_eq!(s.merges, 1);
        to[24] = Some(7);
        to[25] = Some(11);
        // a pad an octave up comes in on the left (26 …): a cluster of its own none holds — a
        // candidate first, its notes to the nearest; born on its second split running, taking of its
        // notes only those not heard yet; a cluster gone in the bleed rule counts for nothing
        let lefts = || found((0..6).chain(12..20).chain([24]).collect());
        let rights = || found((6..12).chain(20..24).chain([25]).collect());
        let v4: Vec<(f64, f32, f32)> = v3.iter().copied().chain((26..30).map(|i| (i as f64, 72.0, -0.6))).collect();
        let (n4, r4) = made(&v4);
        let x4 = ins(&n4, &r4);
        to.resize(30, None);
        let out = s.assign(&[found(vec![0, 1]), lefts(), rights(), found(vec![26, 27])], &[false, true, true, true], &x4[..28], &to[..28], &figs, 25.5);
        assert!(out.born.is_empty(), "a candidate first");
        assert_eq!((to_of(&out, 26), to_of(&out, 27)), (Some(To::Fig(7)), Some(To::Fig(7))));
        to[26] = Some(7);
        to[27] = Some(7);
        let out = s.assign(&[rights(), found((26..30).collect()), lefts()], &[true, true, true], &x4, &to, &figs, 26.5);
        assert_eq!(out.born.len(), 1, "born on its second split running");
        let mut moved: Vec<usize> = out.give.iter().filter(|g| g.1 == To::Born(0)).map(|g| g.0).collect();
        moved.sort();
        assert_eq!(moved, vec![27, 28, 29], "only its notes not heard yet");
        assert!(out.give.iter().all(|g| g.0 >= 26), "{:?}", out.give);
    }

    /// A cluster none holds with a quarter of its notes on an instrument no
    /// cluster holds this time takes that one (its new notes go there, not to
    /// the nearest), split after split: no new instrument for it.
    #[test]
    fn a_cluster_takes_an_instrument_left_without_one() {
        // 0–9 went to 1 (left), 10–29 to 2 (right), 30–32 to 3 (left too); 33–34 are new, on the right
        let v: Vec<(f64, f32, f32)> = (0..35).map(|i| (i as f64, 60.0, if (10..30).contains(&i) || i > 32 { 0.6 } else { -0.6 })).collect();
        let (n, r) = made(&v);
        let x = ins(&n, &r);
        let to: Vec<Option<u32>> = (0..35).map(|i| match i { 0..=9 => Some(1), 10..=29 => Some(2), 30..=32 => Some(3), _ => None }).collect();
        let figs = [Fig { id: 1, x: -0.6, key: 60.0 }, Fig { id: 2, x: 0.6, key: 60.0 }, Fig { id: 3, x: -0.6, key: 60.0 }];
        let mut s = Split { on: true, ..Split::default() };
        for _ in 0..3 {
            let (a, b, c) = (found((0..10).collect()), found((10..24).collect()), found((24..35).collect()));
            let out = s.assign(&[a, b, c], &[true, true, true], &x, &to, &figs, 0.0);
            assert!(out.born.is_empty(), "{:?}", out.born);
            assert_eq!((to_of(&out, 33), to_of(&out, 34)), (Some(To::Fig(3)), Some(To::Fig(3))));
        }
    }

    /// The nearest instrument: by pan, then by pitch.
    #[test]
    fn a_note_goes_to_the_nearest_instrument() {
        let figs = [Fig { id: 1, x: -0.5, key: 60.0 }, Fig { id: 2, x: 0.5, key: 60.0 }, Fig { id: 3, x: 0.5, key: 84.0 }];
        assert_eq!(nearest(&figs, -0.3, 70.0), Some(1));
        assert_eq!(nearest(&figs, 0.4, 62.0), Some(2));
        assert_eq!(nearest(&figs, 0.4, 80.0), Some(3));
        assert_eq!(nearest(&[], 0.0, 60.0), None);
    }
}
