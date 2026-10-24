//! The note instruments of a track (the Spec 2 contract, §2): each pitched
//! source's notes, the instruments found among them, and what the map
//! needs of each instrument — its place, width, colour, level and pitch
//! height frame by frame, its hits and its notes; and the bass's and the
//! voice's notes with a pan each, for the place objects those sources get.
//!
//! As the research's lineup inventory of 28.09 (`notes\C\lineup_notes.py`,
//! seen by Anton in the player): guitar, piano and "other" sources are
//! split into instruments by their notes (A2.1); an instrument whose notes
//! sound again, ≥ 6 dB quieter, in a bigger instrument of another source is
//! that one's bleed and is dropped.

use super::{bp, cluster, stem};
use crate::spatial::stems::{self, Stems};

/// The sources split into note instruments, in the map's order.
const NOTE_STEMS: [(usize, &str); 3] = [(stems::GUITAR, "guitar"), (stems::PIANO, "piano"), (stems::OTHER, "other")];
/// A note's release counted in its instrument's level.
pub(crate) const TAIL: f64 = 0.15;
/// Chord notes starting within 30 ms are one hit.
pub(crate) const CHORD: f64 = 0.03;
/// Bleed: ≥ half of an instrument's notes sound again (same pitch, same
/// time) in a bigger one of another source, ≥ 6 dB quieter.
const DUP: f64 = 0.5;
const BLEED_DB: f64 = 6.0;
/// The pitch height: 40 Hz → 0, 12 kHz → 1.
const Y_LO: f64 = 40.0;
const Y_HI: f64 = 12_000.0;
/// y follows the top note among those within 10 dB of the loudest.
pub(crate) const Y_WITHIN_DB: f64 = 10.0;
/// A note's velocity: its level on its instrument's own 30 dB.
pub(crate) const VEL_DB: f64 = 30.0;
/// A hit of the sound this near a note's is that note's.
const SEAM_S: f64 = 0.06;

/// A note on the map.
#[derive(Clone, Debug)]
pub struct MapNote {
    /// MIDI, fractional (the bend).
    pub key: f32,
    pub on: f32,
    pub off: f32,
    /// 0…1: its level on its instrument's own 30 dB.
    pub vel: f32,
    /// A harmonic of a stronger note, kept with it.
    pub ghost: bool,
}

/// What an instrument's notes say of its sound (for its colour): medians
/// over its strong notes.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // read by the tests; part of the reported result
pub struct Timbre {
    pub pitch: f32,
    /// dB an octave of the harmonic profile (bright > dark).
    pub tilt: f32,
    /// Odd harmonics over even beyond the slope (dB: hollow/clarinet-like > 0).
    pub oddeven: f32,
    /// 10→90 % rise (s).
    pub attack: f32,
    /// Decay after the peak (dB/s; a plucked string < a held tone).
    pub decay: f32,
}

/// A note instrument of a source.
pub struct NoteObj {
    /// The source (`stems::GUITAR` …).
    pub stem: u8,
    /// "guitar", "other 2" …
    pub name: String,
    /// Its place, −1 left … +1 right: its notes' mean pan, by level.
    pub x: f32,
    /// 0 a point … 1 wide.
    pub width: f32,
    pub colour: [f32; 3],
    /// Per frame (44 100 / 512 a second, frame j at sample 512·j): its notes'
    /// harmonics 1–8 while they sound and 0.15 s after (dB, like rms dBFS).
    pub energy_db: Vec<f32>,
    /// Per frame, 0…1: the pitch height of its top note (held in silence).
    pub y: Vec<f32>,
    /// Its strong notes' onsets (s).
    pub hits: Vec<f32>,
    pub notes: Vec<MapNote>,
    #[allow(dead_code)] // read by the tests; part of the reported result
    pub timbre: Timbre,
}

/// A note of the bass or the voice with its pan, for `assign_by_pan`.
#[derive(Clone, Debug)]
pub struct PanNote {
    pub note: MapNote,
    /// −1 left … +1 right.
    pub pan: f32,
    pub lvl_db: f32,
}

pub struct NotesOut {
    /// The note instruments, in the map's order (guitar, piano, other; by pitch inside).
    pub objects: Vec<NoteObj>,
    /// The sources that got note instruments (a source that is there but got
    /// none gets place objects instead).
    pub stems_with_instruments: Vec<u8>,
    pub bass: Vec<PanNote>,
    pub voice: Vec<PanNote>,
}

pub(crate) fn y_of(pitch: f64) -> f32 {
    let f = 440.0 * 2f64.powf((pitch - 69.0) / 12.0);
    ((f / Y_LO).ln() / (Y_HI / Y_LO).ln()).clamp(0.0, 1.0) as f32
}

/// One instrument's notes (indices into the source's notes) → its object.
#[derive(Clone)]
pub(crate) struct Found {
    pub(crate) stem: usize,
    /// Its strong notes (not ghosts) …
    pub(crate) main: Vec<usize>,
    /// … and every note it keeps (+ ghosts).
    pub(crate) all: Vec<usize>,
    pub(crate) pitch: f64,
    /// (on, off, pitch, lvl) of its strong notes, for the bleed rule.
    pub(crate) shape: Vec<(f64, f64, f64, f64)>,
}

fn weighted_pan(p: &stem::Portrait, s: &stem::StemNotes, idx: &[usize]) -> f64 {
    let (mut num, mut den) = (0f64, 0f64);
    for &i in idx {
        if p.pan[i].is_finite() {
            let w = 10f64.powf(s.notes[i].lvl / 10.0);
            num += w * p.pan[i];
            den += w;
        }
    }
    if den > 0.0 {
        num / den
    } else {
        0.0
    }
}

/// The research's cross-source bleed rule on the found instruments.
pub(crate) fn absorb(found: Vec<Found>) -> Vec<Found> {
    let keep: Vec<bool> = found
        .iter()
        .enumerate()
        .map(|(ai, a)| {
            !found.iter().enumerate().any(|(bi, b)| {
                if bi == ai || b.stem == a.stem || b.shape.len() <= a.shape.len() {
                    return false;
                }
                let (mut hit, mut dl) = (0usize, Vec::new());
                for &(on, off, p, lv) in &a.shape {
                    let best = b
                        .shape
                        .iter()
                        .filter_map(|&(bon, boff, bp, blv)| {
                            let ov = boff.min(off) - bon.max(on);
                            let d = (boff - bon).min(off - on);
                            ((bp - p).abs() < 0.5 && ov >= 0.5 * d).then_some((ov, blv))
                        })
                        .max_by(|x, y| x.0.total_cmp(&y.0));
                    if let Some((_, blv)) = best {
                        hit += 1;
                        dl.push(lv - blv);
                    }
                }
                let share = hit as f64 / a.shape.len().max(1) as f64;
                share >= DUP && !dl.is_empty() && stem_median(dl) <= -BLEED_DB
            })
        })
        .collect();
    found.into_iter().zip(keep).filter(|(_, k)| *k).map(|(f, _)| f).collect()
}

fn stem_median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// A source's notes, measured (and, for a note source, split into instruments).
struct Source {
    k: usize,
    s: stem::StemNotes,
    p: stem::Portrait,
    lab: Vec<i32>,
}

/// The pan notes of a source: its strong notes and their ghosts (a ghost
/// takes its parent's pan).
fn pan_notes(s: &stem::StemNotes, p: &stem::Portrait) -> Vec<PanNote> {
    s.notes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.weak)
        .map(|(i, n)| {
            let root = n.ghost.unwrap_or(i);
            let pan = if p.pan[root].is_finite() { p.pan[root] } else { 0.0 };
            PanNote {
                note: MapNote { key: n.pitch as f32, on: n.on as f32, off: n.off as f32, vel: 0.0, ghost: n.ghost.is_some() },
                pan: pan as f32,
                lvl_db: n.lvl as f32,
            }
        })
        .collect()
}

/// Velocities on the source's own 30 dB (p99 of its strong notes' levels at the top).
fn set_vel(notes: &mut [PanNote]) {
    let lv: Vec<f64> = notes.iter().filter(|n| !n.note.ghost).map(|n| n.lvl_db as f64).collect();
    let top = stems::percentile(&lv, 99.0);
    for n in notes {
        n.note.vel = ((n.lvl_db as f64 - (top - VEL_DB)) / VEL_DB).clamp(0.0, 1.0) as f32;
    }
}

/// Everything the notes give a track's map. The network hears the sources
/// one after another; they are measured and split side by side.
pub fn analyse(model: &mut bp::Model, stems: &Stems) -> Result<NotesOut, String> {
    use rayon::prelude::*;
    let n = stems.mix[0].len().min(stems.mix[1].len());
    let frames = 1 + n / stem::HOPS;
    let mix_ref = stem::mix_ref(&stems.src);
    let mix_db = stems.mix_db();
    let mut out = NotesOut { objects: Vec::new(), stems_with_instruments: Vec::new(), bass: Vec::new(), voice: Vec::new() };
    let wanted: Vec<usize> = [stems::BASS, stems::VOCALS]
        .into_iter()
        .chain(NOTE_STEMS.iter().map(|x| x.0))
        .filter(|&k| stems.present(k, mix_db))
        .collect();
    let mut heard = Vec::with_capacity(wanted.len());
    for &k in &wanted {
        let [l, r] = stems.src[k];
        heard.push((k, bp::transcribe(model.run(&bp::to22(l, r))?, &bp::Params::default())));
    }
    let sources: Vec<Source> = heard
        .into_par_iter()
        .map(|(k, tr)| {
            let [l, r] = stems.src[k];
            let s = stem::notes_of(tr, l, r, mix_ref);
            let p = stem::portrait(&s);
            let lab = if k == stems::BASS || k == stems::VOCALS {
                Vec::new()
            } else {
                let take: Vec<bool> = s.notes.iter().map(|e| e.used()).collect();
                cluster::instruments(&p, &s.notes, &take, true)
            };
            Source { k, s, p, lab }
        })
        .collect();
    for src in sources.iter().filter(|x| x.k == stems::BASS || x.k == stems::VOCALS) {
        let mut pn = pan_notes(&src.s, &src.p);
        set_vel(&mut pn);
        if src.k == stems::BASS {
            out.bass = pn;
        } else {
            out.voice = pn;
        }
    }
    // the note sources' instruments, then the bleed rule across sources
    let mut found = Vec::new();
    for src in sources.iter().filter(|x| x.k != stems::BASS && x.k != stems::VOCALS) {
        let (k, s, lab) = (src.k, &src.s, &src.lab);
        let take: Vec<bool> = s.notes.iter().map(|e| e.used()).collect();
        for c in 0..lab.iter().copied().max().unwrap_or(-1) + 1 {
            let main: Vec<usize> = (0..s.notes.len()).filter(|&i| lab[i] == c && take[i]).collect();
            let all: Vec<usize> = (0..s.notes.len()).filter(|&i| lab[i] == c && !s.notes[i].weak).collect();
            if main.is_empty() {
                continue;
            }
            let w: Vec<f64> = main.iter().map(|&i| 10f64.powf(s.notes[i].lvl / 10.0)).collect();
            let pitch = main.iter().zip(&w).map(|(&i, w)| s.notes[i].pitch * w).sum::<f64>() / w.iter().sum::<f64>();
            let shape = main.iter().map(|&i| (s.notes[i].on, s.notes[i].off, s.notes[i].pitch, s.notes[i].lvl)).collect();
            found.push(Found { stem: k, main, all, pitch, shape });
        }
    }
    let kept = absorb(found);
    // names and order: by family, by pitch inside
    for &(k, family) in &NOTE_STEMS {
        let mut fam: Vec<&Found> = kept.iter().filter(|f| f.stem == k).collect();
        if fam.is_empty() {
            continue;
        }
        fam.sort_by(|a, b| a.pitch.total_cmp(&b.pitch));
        out.stems_with_instruments.push(k as u8);
        let src = sources.iter().find(|x| x.k == k).unwrap();
        for (j, f) in fam.iter().enumerate() {
            let name = if fam.len() == 1 { family.to_string() } else { format!("{family} {}", j + 1) };
            out.objects.push(object(f, name, j, &src.s, &src.p, frames));
        }
    }
    Ok(out)
}

/// An instrument's map object.
fn object(f: &Found, name: String, index: usize, s: &stem::StemNotes, p: &stem::Portrait, frames: usize) -> NoteObj {
    let norm = stem::norm();
    // level: its notes' harmonics 1–8, while they sound and a little after
    let mut e = vec![0f64; frames];
    for &i in &f.all {
        let (n, b) = (&s.notes[i], &s.bands[i]);
        for k in 0..b.len() {
            let t = b.t(k);
            if t >= n.on && t <= n.off + TAIL && b.j0 + k < frames {
                e[b.j0 + k] += norm * (0..8).map(|h| 0.5 * (b.el[k][h] as f64 + b.er[k][h] as f64)).sum::<f64>();
            }
        }
    }
    let energy_db: Vec<f32> = e.iter().map(|&v| (10.0 * (v + 1e-20).log10()) as f32).collect();
    let top = stems::percentile(&energy_db.iter().map(|&v| v as f64).collect::<Vec<_>>(), 99.0);
    // hits: its strong notes' onsets, a chord's notes one hit
    let mut on: Vec<f64> = f.main.iter().map(|&i| s.notes[i].on).collect();
    on.sort_by(|a, b| a.total_cmp(b));
    let mut hits: Vec<f32> = Vec::new();
    for t in on {
        if hits.last().is_none_or(|&h| t - h as f64 > CHORD) {
            hits.push(t as f32);
        }
    }
    // pitch height per frame: the top note among the loud ones sounding, held
    let mut y = vec![f32::NAN; frames];
    let mut lead: Vec<Option<(f64, f64)>> = vec![None; frames]; // (loudest lvl, top pitch within 10 dB)
    let mut sounding: Vec<Vec<(f64, f64)>> = vec![Vec::new(); frames];
    for &i in &f.main {
        let n = &s.notes[i];
        let (a, b) = ((n.on * stem::SR / stem::HOPS as f64).ceil() as usize, (n.off * stem::SR / stem::HOPS as f64).floor() as usize);
        for j in a..=b.min(frames - 1) {
            sounding[j].push((n.lvl, n.pitch));
        }
    }
    for j in 0..frames {
        if let Some(loud) = sounding[j].iter().map(|x| x.0).reduce(f64::max) {
            let topp = sounding[j].iter().filter(|x| x.0 >= loud - Y_WITHIN_DB).map(|x| x.1).fold(f64::NEG_INFINITY, f64::max);
            lead[j] = Some((loud, topp));
        }
    }
    let first = lead.iter().flatten().next().map(|x| y_of(x.1)).unwrap_or(0.5);
    let mut held = first;
    for j in 0..frames {
        if let Some((_, pch)) = lead[j] {
            held = y_of(pch);
        }
        y[j] = held;
    }
    let notes: Vec<MapNote> = f
        .all
        .iter()
        .map(|&i| {
            let n = &s.notes[i];
            MapNote {
                key: n.pitch as f32,
                on: n.on as f32,
                off: n.off as f32,
                vel: ((n.lvl - (top - VEL_DB)) / VEL_DB).clamp(0.0, 1.0) as f32,
                ghost: n.ghost.is_some(),
            }
        })
        .collect();
    let x = weighted_pan(p, s, &f.main) as f32;
    // width: how decorrelated its notes are, or how far apart they sit
    let (mut cw, mut w, mut pw) = (0f64, 0f64, 0f64);
    for &i in &f.main {
        let wt = 10f64.powf(s.notes[i].lvl / 10.0);
        if p.coh[i].is_finite() && p.pan[i].is_finite() {
            cw += wt * p.coh[i];
            pw += wt * (p.pan[i] - x as f64).powi(2);
            w += wt;
        }
    }
    let width = if w > 0.0 { (1.0 - cw / w).max(2.0 * (pw / w).sqrt()).clamp(0.0, 1.0) } else { 0.0 } as f32;
    let med = |v: Vec<f64>| -> f32 {
        let v: Vec<f64> = v.into_iter().filter(|x| x.is_finite()).collect();
        if v.is_empty() { f32::NAN } else { stem_median(v) as f32 }
    };
    let timbre = Timbre {
        pitch: f.pitch as f32,
        tilt: med(f.main.iter().map(|&i| p.tilt[i]).collect()),
        oddeven: med(f.main.iter().map(|&i| p.oddeven[i]).collect()),
        attack: med(f.main.iter().map(|&i| p.attack[i]).collect()),
        decay: med(f.main.iter().map(|&i| p.decay[i]).collect()),
    };
    let kind = match f.stem {
        stems::GUITAR => crate::spatial::K_GUITAR,
        stems::PIANO => crate::spatial::K_PIANO,
        _ => crate::spatial::K_OTHER,
    };
    let colour = super::colour::colour(kind, Some(&timbre), index);
    NoteObj { stem: f.stem as u8, name, x, width, colour, energy_db, y, hits, notes, timbre }
}

/// The hits of a place of the bass or the voice (Anton 3.10: it flashes on
/// its notes, as a note instrument does), from its notes (on, off, s; and
/// whether a ghost): its strong notes' onsets, a chord's notes one hit; and
/// the onsets of its sound (`sound`, s) wherever none of its notes sounds and
/// none starts within 60 ms. A place's hits made so already come out the same.
pub fn note_hits(sound: &[f32], notes: &[(f32, f32, bool)]) -> Vec<f32> {
    let mut on: Vec<f32> = notes.iter().filter(|n| !n.2).map(|n| n.0).collect();
    on.sort_by(|a, b| a.total_cmp(b));
    let mut hits: Vec<f32> = Vec::new();
    for t in on {
        if hits.last().is_none_or(|&h| (t - h) as f64 > CHORD) {
            hits.push(t);
        }
    }
    // where a note sounds, or starts 60 ms either way: the note alone
    let mut cover: Vec<(f32, f32)> = notes.iter().map(|n| (n.0 - SEAM_S as f32, n.1.max(n.0 + SEAM_S as f32))).collect();
    cover.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut runs: Vec<(f32, f32)> = Vec::new();
    for (a, b) in cover {
        match runs.last_mut() {
            Some(r) if a <= r.1 => r.1 = r.1.max(b),
            _ => runs.push((a, b)),
        }
    }
    let quiet = |t: f32| {
        let k = runs.partition_point(|r| r.0 < t);
        k == 0 || t >= runs[k - 1].1
    };
    hits.extend(sound.iter().copied().filter(|&t| quiet(t)));
    hits.sort_by(|a, b| a.total_cmp(b));
    hits
}

/// The bass's or the voice's notes → the place objects of that source
/// (their x and their energy per map frame at `fps`): each note to the
/// nearest by pan; a note in the middle third between two neighbours to the
/// one louder over the note (a backing voice close to its own place stays
/// there even while the lead is louder). `None` when there is no object.
pub fn assign_by_pan(notes: &[PanNote], objs: &[(f32, &[u8])], fps: f64) -> Vec<Option<usize>> {
    let mut order: Vec<usize> = (0..objs.len()).collect();
    order.sort_by(|&a, &b| objs[a].0.total_cmp(&objs[b].0));
    let loud = |o: usize, n: &MapNote| -> f64 {
        let e = objs[o].1;
        let (a, b) = ((n.on as f64 * fps) as usize, (n.off as f64 * fps).ceil() as usize);
        let s: Vec<f64> = (a..=b).filter(|&j| j < e.len()).map(|j| e[j] as f64).collect();
        if s.is_empty() { 0.0 } else { s.iter().sum::<f64>() / s.len() as f64 }
    };
    notes
        .iter()
        .map(|pn| {
            if order.is_empty() {
                return None;
            }
            let p = pn.pan;
            // the neighbours on each side of the note's pan
            let right = order.iter().position(|&o| objs[o].0 >= p);
            Some(match right {
                Some(0) => order[0],
                None => *order.last().unwrap(),
                Some(r) => {
                    let (a, b) = (order[r - 1], order[r]);
                    let f = (p - objs[a].0) / (objs[b].0 - objs[a].0).max(1e-6);
                    if f < 1.0 / 3.0 {
                        a
                    } else if f > 2.0 / 3.0 {
                        b
                    } else if loud(a, &pn.note) >= loud(b, &pn.note) {
                        a
                    } else {
                        b
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pan: f32, on: f32) -> PanNote {
        PanNote { note: MapNote { key: 60.0, on, off: on + 0.5, vel: 1.0, ghost: false }, pan, lvl_db: -20.0 }
    }

    /// Nearest by pan; in the middle third between two places the louder one.
    #[test]
    fn notes_go_to_the_nearest_place_or_the_louder_one() {
        let fps = 10.0;
        // a centre place loud in the first second, a right place loud in the second
        let centre: Vec<u8> = (0..20).map(|j| if j < 10 { 200 } else { 20 }).collect();
        let right: Vec<u8> = (0..20).map(|j| if j < 10 { 20 } else { 200 }).collect();
        let objs = [(0.6, &right[..]), (0.0, &centre[..])];
        let got = assign_by_pan(
            &[note(-0.5, 0.0), note(0.05, 1.2), note(0.55, 0.1), note(0.3, 0.1), note(0.3, 1.2), note(0.9, 0.0)],
            &objs,
            fps,
        );
        assert_eq!(got, vec![Some(1), Some(1), Some(0), Some(1), Some(0), Some(0)]);
        assert_eq!(assign_by_pan(&[note(0.0, 0.0)], &[], fps), vec![None]);
    }

    /// A place flashes on its strong notes (a chord one hit, a ghost none);
    /// on the onsets of its sound wherever none of its notes sounds and none
    /// starts within 60 ms; with no notes on its sound's alone; and its hits
    /// made so come out the same.
    #[test]
    fn a_place_flashes_on_its_notes_and_on_its_sound_between_them() {
        let sound = [0.1, 0.5, 0.97, 1.3, 2.0, 2.45, 3.01, 4.2, 4.98, 6.0];
        // notes 1.0–1.8 (a chord: 1.02), 2.5–2.9 (a ghost 2.5–2.7 with it), 3.0–4.0, 5.0–5.5, a ghost 4.15–4.3 alone
        let notes = [(2.5, 2.9, false), (1.0, 1.8, false), (1.02, 1.5, false), (2.5, 2.7, true), (3.0, 4.0, false), (5.0, 5.5, false), (4.15, 4.3, true)];
        let got = note_hits(&sound, &notes);
        assert_eq!(got, vec![0.1, 0.5, 1.0, 2.0, 2.5, 3.0, 5.0, 6.0]);
        assert_eq!(note_hits(&got, &notes), got, "made so already: the same");
        assert_eq!(note_hits(&sound, &[]), sound.to_vec());
    }

    #[test]
    fn heights_run_from_40_hz_to_12_khz() {
        // MIDI 28 = 41.2 Hz, 127 = 12.5 kHz
        assert!(y_of(28.0) < 0.01 && y_of(127.0) == 1.0);
        let a4 = y_of(69.0) as f64;
        assert!((a4 - (440f64 / 40.0).ln() / 300f64.ln()).abs() < 1e-6);
    }
}
