//! A song's map on a live stream (the "with instruments" tier): its
//! instruments found as the song plays, from the separated sources as they
//! come (`feats`), and the bass's and the voice's notes on their places
//! (`set_notes`).
//!
//! - The kit from the drums' bands: kick, snare, hats (a file's
//!   `kit_bands_of`, as on a card without the drum network), there from the
//!   song's start; where the drum network has taken the drums apart
//!   (`drums`), each of the three goes over to its piece's values over half
//!   a second (no jump in its level, no flash twice), and the toms, the ride
//!   and the crash become instruments of their own once they play (a file's
//!   gates on what the song has played so far, for good).
//! - The places of the bass, the voice, the guitars, the piano and the rest:
//!   each source's histogram by pan grows over the song (`place::places` on
//!   it every segment); a place found later is an instrument of its own from
//!   then on, one that goes keeps its slot, silent.
//! - The room: the mix's diffuse layer (the stand-in's ambience band).
//!
//! Per frame every value is a file's: the same functions (`finish_band`,
//! `map::finish`, the onsets' peaks) run over a stretch of frames — the
//! three seconds before the first one written settle them, the 0.6 s after
//! the last one (`LAG`) is what they look ahead to — against the song's
//! levels so far instead of the whole track's. A frame is written once its
//! sources are whole (no segment adds to them any more), and before that,
//! for the place being heard, from a segment's last quarter alone; the next
//! segment writes it again.
//!
//! The slots (AURA-VIS-SPEC-INSTRUMENTS.md F1, on a stream): until a scene
//! has been shown the song's map they are in a file's order; from then on
//! they only grow — an instrument found later takes the next slot, the end of
//! the row, and keeps it to the song's end. Its place and width settle over
//! its first ten seconds, then stay.

use std::collections::HashMap;

use super::super::analysis::{self, BANDS, B_AMB, FPS, RAW};
use super::super::job::{kind_of_stem, peaks, width_of};
use super::super::kit::{self, NP};
use super::super::map::{self, MapObj, Note, Raw, TrackMap, MAP_FPS, MAX_OBJECTS};
use super::super::notes::colour::colour;
use super::super::notes::objects::{absorb, assign_by_pan, note_hits, y_of, Found, MapNote, PanNote, Timbre, Y_WITHIN_DB};
use super::super::place::{self, nice, onset_candidates, ON_FLOOR_DB};
use super::super::stems::PRESENT_DB;
use super::super::{K_AMBIENCE, K_CYMBAL, K_HATS, K_KICK, K_SNARE, K_TOMS};
use super::drums::{self, DrumFeats};
use super::feats::{Feats, Frame, PLACED, PNB};
use super::inst::{self, Fig, In, Split, To, NOBODY};
use super::notes::{LNote, Placed, Sound, INST, SOURCES};
use super::pct::Pct;
use super::rough::Rough;

/// Frames a value waits for the ones after it (the onsets' look-ahead: a
/// band's half-second baseline and its peaks' 50 ms; the smoothing).
pub const LAG: usize = 52;
/// Frames before the first one written that a stretch starts from.
const MARGIN: usize = 258;
/// An instrument's place and width settle over its first seconds.
const SETTLE_S: f64 = 10.0;
/// A source's frames that must sound (within 30 dB of its loud end) before
/// its places are looked for.
const SOUNDING: usize = 43;
/// The kit's bands' onset thresholds (a file's `kit_bands_of`: R1 on MDB).
const KIT_THR: [f32; 3] = [0.5, 0.05, 0.3];
const KIT_KINDS: [(u8, &str); 3] = [(K_KICK, "kick"), (K_SNARE, "snare"), (K_HATS, "hh")];
/// A new place must be seen this many looks running (a look a segment)
/// before it is an instrument — except a source's first.
const NEW_PLACE_LOOKS: u32 = 2;
const EPS: f32 = 1e-12;
const NORM: f32 = 4.0 / (place::NFFT as f32 * place::NFFT as f32);
/// The kit's pieces from the song's start — kick, snare, hats — as the drum
/// network's pieces.
const KIT_PIECE: [usize; 3] = [0, 1, 3];
/// The drum network's pieces that are instruments of their own once they
/// play: the toms, the ride, the crash.
const LATE_PIECES: [usize; 3] = [2, 4, 5];
/// A kit piece goes over from the drums' bands to the drum network's values
/// over this many frames (half a second).
const SEAM: usize = 43;
/// Two hits closer than this, one from the bands and one from the network,
/// are one.
const SEAM_HIT_S: f64 = 0.06;
/// Frames the drum network's hits look ahead (their candidates' local mean).
const DLAG: usize = 12;
/// A note's velocity: its level on its source's own 30 dB (a file's).
const VEL_DB: f64 = 30.0;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum What {
    Kit(usize),
    /// A piece of the drum network (its index in the network's order).
    Piece(usize),
    /// A place of a placed source (`PLACED` index): its pan bins [lo, hi), or
    /// none now (it went: silent, its slot kept).
    Place { p: usize, range: Option<(usize, usize)> },
    /// A note instrument of a split source (`PLACED` index): its values come
    /// from its notes (`Song::write_inst`).
    Inst(usize),
    Room,
}

struct Meta {
    id: u32,
    what: What,
    /// The session frame it was found at.
    born: usize,
    settled: bool,
    /// Its levels (dB, unscaled by the song's loud level) over whole frames.
    db: Pct,
    /// A place's flux (dB) and its onsets' levels (the floor's percentile).
    flux: Pct,
    hits_lv: Pct,
    /// While it settles: its x, and its levels and coherences for its width.
    xs: Vec<f32>,
    wdb: Vec<f32>,
    wcoh: Vec<f32>,
    /// A place: the onsets of its sound (song s) — it flashes on its strong
    /// notes over them (`place_hits`).
    ehits: Vec<f32>,
    /// A note instrument: its sound's levels (dB) over the frames heard, the
    /// frames below this taught, and the session frame from which it took over
    /// a place with the place's values just before.
    ndb: Pct,
    ntaught: usize,
    seam: Option<(usize, [u8; 5])>,
}

impl Meta {
    fn new(id: u32, what: What, born: usize) -> Meta {
        Meta {
            id,
            what,
            born,
            settled: false,
            db: Pct::default(),
            flux: Pct::default(),
            hits_lv: Pct::default(),
            xs: Vec::new(),
            wdb: Vec::new(),
            wcoh: Vec::new(),
            ehits: Vec::new(),
            ndb: Pct::default(),
            ntaught: 0,
            seam: None,
        }
    }
}

#[derive(Default)]
struct Source {
    hist: Vec<f64>,
    lv: Pct,
    db: Pct,
    sounding: usize,
    looked: bool,
    /// Places seen, not instruments yet: (pan, looks running).
    pending: Vec<(f32, u32)>,
    found: usize,
    /// Its places as the last look found them (pan bins [lo, hi)).
    regs: Vec<(usize, usize)>,
}

/// What the song's whole frames from the drum network taught of a piece.
#[derive(Default)]
struct PieceStat {
    /// Its levels (dB) and its onset function's flux (dB of it).
    db: Pct,
    flux: Pct,
    /// Its hits' candidates' levels: at its own setting, at the gates'.
    cand: Pct,
    gcand: Pct,
    /// Its hits at the gates' setting.
    ghits: usize,
    /// Its spectrum's centre, and its left and right levels (sums).
    e: f64,
    ef: f64,
    al: f64,
    ar: f64,
    /// One of the song's instruments: the gates passed (once, for good).
    plays: bool,
}

/// The drum network's frames for the songs: whole below `fin` (past it, from
/// its last chunk alone); `on`: it runs, and the kit's final values wait for
/// its whole frames.
#[derive(Clone, Copy)]
pub struct Drums<'a> {
    pub feats: &'a DrumFeats,
    pub fin: usize,
    pub on: bool,
}

/// One song of the stream: its stand-in and its map.
/// The notes go into `map.notes` in the song's own seconds, on their objects'
/// slots, by onset (`set_notes`): `map_frames` counts them, `map_json` gives a
/// window of them in the session's seconds.
pub struct Song {
    pub n: u32,
    /// Its first session frame, and where the next song begins (when known).
    pub f0: usize,
    pub end: Option<usize>,
    pub rough: Rough,
    pub map: TrackMap,
    meta: Vec<Meta>,
    pub version: u32,
    /// A scene has been given its map: from now on the slots only grow.
    pub shown: bool,
    /// Per song frame: made from the separated sources.
    pub ok: Vec<bool>,
    /// The next session frame whose whole values teach the song.
    learn_next: usize,
    /// Session frames below: final values written.
    emit_fin: usize,
    src: [Source; 5],
    kit_db: [Pct; 3],
    room_db: Pct,
    mix_db: Pct,
    full_said: bool,
    next_id: u32,
    /// The drum network's pieces over the song.
    dr: [PieceStat; NP],
    /// The next session frame whose whole drum values teach the song.
    dlearn_next: usize,
    /// Session frames below: the kit's final values written (while the drum
    /// network runs its whole frames come later than the sources').
    kemit_fin: usize,
    /// The song has whole frames from the drum network.
    drum_seen: bool,
    cymbals: usize,
    /// The note instruments' sources (`notes::INST` on) split into their
    /// instruments, and the last clusters of each (the bleed rule weighs them all).
    split: [Split; 3],
    found: [Vec<Found>; 3],
}

impl Song {
    pub fn new(n: u32, f0: usize, path: String) -> Song {
        Song {
            n,
            f0,
            end: None,
            rough: Rough::new(path),
            map: TrackMap { content: 0, frames: 0, objects: Vec::new(), notes: Vec::new(), kit_bands: true },
            meta: Vec::new(),
            version: 0,
            shown: false,
            ok: Vec::new(),
            learn_next: f0,
            emit_fin: f0,
            src: Default::default(),
            kit_db: Default::default(),
            room_db: Pct::default(),
            mix_db: Pct::default(),
            full_said: false,
            next_id: 0,
            dr: Default::default(),
            dlearn_next: f0,
            kemit_fin: f0,
            drum_seen: false,
            cymbals: 0,
            split: Default::default(),
            found: Default::default(),
        }
    }

    /// The note instruments of the split sources (slot, id, `PLACED` index),
    /// and each note instruments' source's split (split, splits, merged).
    #[cfg(test)]
    pub(super) fn inst_view(&self) -> (Vec<(usize, u32, usize)>, [(bool, u32, u32); 3]) {
        let figs = (0..self.meta.len())
            .filter_map(|i| match self.meta[i].what {
                What::Inst(p) => Some((i, self.meta[i].id, p)),
                _ => None,
            })
            .collect();
        (figs, std::array::from_fn(|q| (self.split[q].on, self.split[q].splits, self.split[q].merges)))
    }

    /// Object `i` is a note instrument of a split source.
    #[cfg(test)]
    pub(super) fn is_inst(&self, i: usize) -> bool {
        matches!(self.meta[i].what, What::Inst(_))
    }

    /// Values are known for song frames below this.
    pub fn ready(&self) -> usize {
        self.map.frames
    }

    /// Song frame `j`'s instruments are there (made from the separated sources).
    pub fn has(&self, j: usize) -> bool {
        j < self.map.frames && self.ok.get(j).copied().unwrap_or(false) && !self.map.objects.is_empty()
    }

    /// The song ends at session frame `at` (a song's start found there):
    /// what was made past it goes.
    pub fn end_at(&mut self, at: usize) {
        self.end = Some(at);
        let j = at.saturating_sub(self.f0);
        self.rough.truncate(j);
        if self.map.frames > j {
            self.map.frames = j;
            self.ok.truncate(j);
            let t = j as f64 / FPS;
            for o in self.map.objects.iter_mut() {
                for v in [&mut o.presence, &mut o.energy, &mut o.y, &mut o.z, &mut o.coherence] {
                    v.truncate(j);
                }
                o.hits.retain(|h| (*h as f64) < t);
            }
            for m in self.meta.iter_mut() {
                m.ehits.retain(|h| (*h as f64) < t);
            }
            self.map.notes.retain(|n| (n.on as f64) < t);
        }
        self.learn_next = self.learn_next.min(at);
        self.emit_fin = self.emit_fin.min(at);
        self.dlearn_next = self.dlearn_next.min(at);
        self.kemit_fin = self.kemit_fin.min(at);
    }

    fn mix_loud(&self) -> f32 {
        self.mix_db.all(99.0).unwrap_or(-20.0)
    }

    /// The mix's loud level the notes' weak gate goes by (a file's
    /// `stem::mix_ref`: p95 of its levels), over the song so far.
    pub fn mix_ref(&self) -> f64 {
        self.mix_db.all(95.0).map_or(-20.0, |v| v as f64)
    }

    /// The sources' notes (session seconds, `notes::SOURCES`) on the song's
    /// places of them, as a file's bass and voice (`notes::objects::assign_by_pan`):
    /// a settled note with its onset in the song goes to the place nearest by
    /// pan once, and keeps it (one with no place of its source yet waits); the
    /// stream's own for now go anew each time. The map's notes are made of
    /// them — the song's seconds, its end the last; velocities on each
    /// source's own 30 dB (the p99 of its strong notes in the song so far at
    /// the top) — and its version moves when they change.
    /// The places flash on them then (`place_hits`). A note instruments'
    /// source with notes enough is split into its instruments first
    /// (`split_sources`): its notes go to them (`inst_notes`), and their values
    /// are made of them (`write_insts`; `own_sound`: the sound of the stream's
    /// own, `feats`: their places' coherence, `heard`: the session second heard).
    #[allow(clippy::too_many_arguments)]
    pub fn set_notes(&mut self, placed: &mut [Vec<Placed>; 5], own: &[Vec<LNote>; 5], own_sound: &[Vec<Sound>; 5], feats: &Feats, heard: f64, split: bool) {
        self.split_sources(placed, heard, split);
        let t0 = self.f0 as f64 / FPS;
        let t1 = self.end.map_or(f64::INFINITY, |e| e as f64 / FPS);
        let inside = |n: &LNote| n.on >= t0 && n.on < t1;
        let pan_note = |n: &LNote| PanNote {
            note: MapNote { key: n.key, on: (n.on - t0) as f32, off: (n.off - t0) as f32, vel: 0.0, ghost: n.ghost },
            pan: n.pan,
            lvl_db: n.lvl,
        };
        let mut notes = Vec::new();
        let mut own_to: Vec<Vec<Option<u32>>> = vec![Vec::new(); PLACED.len() - INST];
        for (p, (placed, own)) in placed.iter_mut().zip(own).enumerate() {
            if p >= INST && self.split[p - INST].on {
                own_to[p - INST] = self.inst_notes(p, placed, own, &mut notes, heard);
                continue;
            }
            // the source's places there now (else every one it had): slot, id
            let mine: Vec<(usize, u32, bool)> = (0..self.meta.len())
                .filter_map(|i| match self.meta[i].what {
                    What::Place { p: q, range } if q == p => Some((i, self.meta[i].id, range.is_some())),
                    _ => None,
                })
                .collect();
            let here: Vec<(usize, u32)> = if mine.iter().any(|m| m.2) { mine.iter().filter(|m| m.2).map(|m| (m.0, m.1)).collect() } else { mine.iter().map(|m| (m.0, m.1)).collect() };
            let objs: Vec<(f32, &[u8])> = here.iter().map(|&(i, _)| (self.map.objects[i].x, &self.map.objects[i].energy[..])).collect();
            let slot = |id: u32| self.meta.iter().position(|m| m.id == id);
            let todo: Vec<usize> = (0..placed.len())
                .filter(|&k| inside(&placed[k].n) && placed[k].to.is_none_or(|(n, id)| n != self.n || slot(id).is_none()))
                .collect();
            let pn: Vec<PanNote> = todo.iter().map(|&k| pan_note(&placed[k].n)).collect();
            for (&k, to) in todo.iter().zip(assign_by_pan(&pn, &objs, MAP_FPS)) {
                placed[k].to = to.map(|j| (self.n, here[j].1));
            }
            let own: Vec<&LNote> = own.iter().filter(|n| inside(n)).collect();
            let pn: Vec<PanNote> = own.iter().map(|n| pan_note(n)).collect();
            let own_to = assign_by_pan(&pn, &objs, MAP_FPS);
            let lv: Vec<f64> = placed.iter().map(|q| &q.n).filter(|n| inside(n)).chain(own.iter().copied()).filter(|n| !n.ghost).map(|n| n.lvl as f64).collect();
            let top = if lv.is_empty() { 0.0 } else { percentile(&lv, 99.0) };
            let note = |n: &LNote, s: usize| Note {
                obj: s.min(255) as u8,
                key: n.key,
                on: (n.on - t0) as f32,
                off: (n.off.min(t1) - t0) as f32,
                vel: ((n.lvl as f64 - (top - VEL_DB)) / VEL_DB).clamp(0.0, 1.0) as f32,
                ghost: n.ghost,
            };
            for q in placed.iter().filter(|q| inside(&q.n)) {
                if let Some(s) = q.to.filter(|t| t.0 == self.n).and_then(|t| slot(t.1)) {
                    notes.push(note(&q.n, s));
                }
            }
            for (n, to) in own.iter().zip(own_to) {
                if let Some(j) = to {
                    notes.push(note(n, here[j].0));
                }
            }
        }
        notes.sort_by(|a, b| a.on.total_cmp(&b.on));
        if notes != self.map.notes {
            self.map.notes = notes;
            self.version = self.version.wrapping_add(1);
        }
        self.place_hits();
        self.write_insts(placed, own, own_sound, &own_to, feats, heard);
    }

    /// The places flash on their strong notes over the onsets of their sound,
    /// as a file's bass and voice (`map::place_hits`): a place with no notes
    /// on the onsets of its sound alone.
    fn place_hits(&mut self) {
        for (i, o) in self.map.objects.iter_mut().enumerate() {
            if let What::Place { .. } = self.meta[i].what {
                let mine: Vec<(f32, f32, bool)> = self.map.notes.iter().filter(|n| n.obj as usize == i).map(|n| (n.on, n.off, n.ghost)).collect();
                o.hits = note_hits(&self.meta[i].ehits, &mine);
            }
        }
    }

    /// What its separated sources brought: the frames whole below session
    /// frame `fin` and those below `prov` from a segment alone; and what the
    /// drum network brought (`dr`).
    pub fn run(&mut self, feats: &Feats, fin: usize, prov: usize, dr: Drums) {
        let stop = self.end.unwrap_or(usize::MAX);
        let (fin, prov) = (fin.min(stop), prov.min(stop));
        let dfin = dr.fin.min(stop);
        self.learn_next = self.learn_next.max(feats.start()).max(self.f0);
        let before = self.signature();
        // 1. what the whole frames teach
        for j in self.learn_next..fin {
            if let Some(f) = feats.get(j).filter(|f| f.ok) {
                let f = *f;
                self.learn(j, &f);
            }
        }
        self.learn_next = self.learn_next.max(fin);
        // 1b. what the drum network's whole frames teach; the pieces that play from now on
        let playing = self.learn_drums(dr.feats, dfin);
        // 2. the kit and the room, with the song's first separated frame
        if self.meta.is_empty() && (self.f0.max(feats.start())..fin.max(prov)).any(|j| feats.get(j).is_some_and(|f| f.ok)) {
            let at = self.f0.max(feats.start());
            for k in 0..3 {
                self.add(What::Kit(k), at);
            }
            self.add(What::Room, at);
        }
        // 2b. a piece found playing: an instrument of its own, written from the frames held
        let dfrom = self.f0.max(dr.feats.start());
        for p in playing {
            let Some(id) = self.add_piece(p, self.dlearn_next) else { continue };
            let Some(i) = self.meta.iter().position(|m| m.id == id) else { continue };
            if self.kemit_fin > dfrom {
                self.write(i, feats, dr.feats, dfrom, self.kemit_fin, self.kemit_fin, true);
            }
        }
        // 3. the places; the ones found now learn from the frames held and
        // are written from there
        let from = self.f0.max(feats.start());
        for id in self.look() {
            let Some(i) = self.meta.iter().position(|m| m.id == id) else { continue };
            self.teach(i, feats, from, self.learn_next);
            if self.emit_fin > from {
                self.write(i, feats, dr.feats, from, self.emit_fin, self.emit_fin, true);
            }
        }
        // 4. the values: whole, then ahead from the last segment alone; the
        // kit's whole ones, while the drum network runs, as far as its whole frames
        let fin_to = if fin >= stop { fin } else { fin.saturating_sub(LAG) };
        let (kit_fin_to, kit_w1) = if dr.on {
            (fin_to.min(if dfin >= stop { dfin } else { dfin.saturating_sub(LAG) }), fin.min(dfin))
        } else {
            (fin_to, fin)
        };
        let is_kit: Vec<bool> = self.meta.iter().map(|m| matches!(m.what, What::Kit(_) | What::Piece(_))).collect();
        if fin_to > self.emit_fin {
            for i in (0..self.meta.len()).filter(|&i| !is_kit[i]) {
                self.write(i, feats, dr.feats, self.emit_fin, fin_to, fin, true);
            }
            self.emit_fin = fin_to;
        }
        if kit_fin_to > self.kemit_fin {
            for i in (0..self.meta.len()).filter(|&i| is_kit[i]) {
                self.write(i, feats, dr.feats, self.kemit_fin, kit_fin_to, kit_w1, true);
            }
            self.kemit_fin = kit_fin_to;
        }
        let prov_to = if prov >= stop { prov } else { prov.saturating_sub(LAG) };
        for i in 0..self.meta.len() {
            let e0 = if is_kit[i] { self.kemit_fin } else { self.emit_fin };
            if prov_to > e0 {
                self.write(i, feats, dr.feats, e0, prov_to, prov, false);
            }
        }
        self.map.kit_bands = !self.drum_seen;
        let ready = prov_to.max(self.emit_fin).saturating_sub(self.f0);
        if ready > self.map.frames {
            self.map.frames = ready;
        }
        self.ok.resize(self.map.frames, false);
        for j in self.f0.max(feats.start())..self.f0 + self.map.frames {
            self.ok[j - self.f0] = feats.get(j).is_some_and(|f| f.ok);
        }
        for o in self.map.objects.iter_mut() {
            for v in [&mut o.presence, &mut o.energy, &mut o.y, &mut o.z, &mut o.coherence] {
                v.resize(v.len().max(self.map.frames), 0);
            }
        }
        self.place_hits();
        if self.signature() != before {
            self.version = self.version.wrapping_add(1);
        }
    }

    /// What changes the map's id: the instruments, their places and widths.
    fn signature(&self) -> Vec<(u8, u32, u32)> {
        self.map.objects.iter().map(|o| (o.kind, o.x.to_bits(), o.width.to_bits())).collect()
    }

    fn learn(&mut self, j: usize, f: &Frame) {
        self.mix_db.push(f.mix_db);
        for k in 0..3 {
            self.kit_db[k].push(10.0 * (f.kit[k][0] + EPS).log10());
        }
        if let Some(e) = self.rough_band(j).map(|r| r[0]) {
            self.room_db.push(10.0 * (e + EPS).log10());
        }
        for (p, s) in self.src.iter_mut().enumerate() {
            let sf = &f.src[p];
            s.lv.push(sf.lv);
            s.db.push(sf.db);
            if s.hist.is_empty() {
                s.hist = vec![0.0; PNB];
            }
            let loud = s.lv.all(99.0).unwrap_or(sf.lv);
            if sf.lv > loud - place::ACTIVE_DB {
                let tot: f32 = sf.e.iter().sum::<f32>() + 1e-20;
                for b in 0..PNB {
                    s.hist[b] += (sf.e[b] / tot) as f64;
                }
                s.sounding += 1;
            }
        }
        for m in self.meta.iter_mut() {
            if let What::Place { p, range: Some((lo, hi)) } = m.what {
                let sf = &f.src[p];
                let e: f32 = sf.e[lo..hi].iter().sum();
                m.db.push(10.0 * (e * NORM + 1e-20).log10());
                m.flux.push(10.0 * (sf.fl[lo..hi].iter().sum::<f32>() + 1e-12).log10());
            }
        }
    }

    /// The stand-in's ambience band at session frame `j` (`[RAW]`).
    fn rough_band(&self, j: usize) -> Option<&[f32]> {
        let k = j.checked_sub(self.f0)?;
        let o = (k * BANDS + B_AMB) * RAW;
        self.rough.d.rough_band.get(o..o + RAW)
    }

    /// The sources' places looked at again: their objects follow them; a
    /// place seen anew (twice running, but a source's first look) is a new
    /// instrument. The new objects' ids.
    fn look(&mut self) -> Vec<u32> {
        let mix = self.mix_db.all(99.0);
        let born = self.learn_next;
        let mut made = Vec::new();
        for p in 0..PLACED.len() {
            let s = &self.src[p];
            let there = s.sounding >= SOUNDING
                && match (s.db.all(99.0), mix) {
                    (Some(a), Some(m)) => a - m >= PRESENT_DB as f32,
                    _ => false,
                };
            if !there {
                continue;
            }
            let hist = s.hist.clone();
            let regs = place::places(&hist).0;
            self.src[p].regs = regs.clone();
            if p >= INST && self.split[p - INST].on {
                // a split source's instruments are its notes'
                continue;
            }
            let mine: Vec<usize> = (0..self.meta.len()).filter(|&i| matches!(self.meta[i].what, What::Place { p: q, .. } if q == p)).collect();
            let mut used = vec![false; mine.len()];
            let mut fresh: Vec<((usize, usize), f32)> = Vec::new();
            for &(a, b) in &regs {
                let w: f64 = hist[a..b].iter().map(|h| h + 1e-12).sum();
                let pan = ((a..b).map(|i| (i as f64 / PNB as f64 * 2.0 - 1.0 + 1.0 / PNB as f64) * (hist[i] + 1e-12)).sum::<f64>() / w) as f32;
                let (lo, hi) = (a as f32 / PNB as f32 * 2.0 - 1.0, b as f32 / PNB as f32 * 2.0 - 1.0);
                // the object standing there (the nearest one inside the place)
                let at = mine
                    .iter()
                    .enumerate()
                    .filter(|&(k, &i)| !used[k] && self.map.objects[i].x >= lo - 0.05 && self.map.objects[i].x <= hi + 0.05)
                    .min_by(|x, y| (self.map.objects[*x.1].x - pan).abs().total_cmp(&(self.map.objects[*y.1].x - pan).abs()))
                    .map(|(k, &i)| (k, i));
                match at {
                    Some((k, i)) => {
                        used[k] = true;
                        self.meta[i].what = What::Place { p, range: Some((a, b)) };
                        if !self.meta[i].settled {
                            self.map.objects[i].x = pan;
                        }
                    }
                    None => fresh.push(((a, b), pan)),
                }
            }
            for (k, &i) in mine.iter().enumerate() {
                if !used[k] {
                    self.meta[i].what = What::Place { p, range: None };
                }
            }
            let first = !self.src[p].looked;
            self.src[p].looked = true;
            let mut pending = std::mem::take(&mut self.src[p].pending);
            let mut keep = Vec::new();
            let sides = regs.len() > 1;
            for ((a, b), pan) in fresh {
                let seen = pending.iter().position(|(x, _)| (x - pan).abs() < 0.1).map(|i| pending.swap_remove(i).1 + 1).unwrap_or(1);
                if first || seen >= NEW_PLACE_LOOKS {
                    made.extend(self.add_place(p, (a, b), pan, sides, born));
                } else {
                    keep.push((pan, seen));
                }
            }
            self.src[p].pending = keep;
        }
        made
    }

    /// A new object learns its levels from the whole frames [from, to) held.
    fn teach(&mut self, i: usize, feats: &Feats, from: usize, to: usize) {
        let What::Place { p, range: Some((lo, hi)) } = self.meta[i].what else { return };
        let m = &mut self.meta[i];
        for j in from..to {
            if let Some(f) = feats.get(j).filter(|f| f.ok) {
                let sf = &f.src[p];
                m.db.push(10.0 * (sf.e[lo..hi].iter().sum::<f32>() * NORM + 1e-20).log10());
                m.flux.push(10.0 * (sf.fl[lo..hi].iter().sum::<f32>() + 1e-12).log10());
            }
        }
    }

    fn add_place(&mut self, p: usize, (a, b): (usize, usize), pan: f32, sides: bool, at: usize) -> Option<u32> {
        let stem = PLACED[p];
        let side = if pan.abs() < 0.2 { "C" } else if pan < 0.0 { "L" } else { "R" };
        let index = self.src[p].found;
        let name = if sides || index > 0 { format!("{} {side}", nice(stem)) } else { nice(stem).to_string() };
        let kd = kind_of_stem(stem);
        let o = MapObj { kind: kd, stem: stem as u8, name, x: pan, width: 0.3, colour: colour(kd, None, index), ..Default::default() };
        let id = self.insert(o, What::Place { p, range: Some((a, b)) }, at)?;
        self.src[p].found += 1;
        Some(id)
    }

    fn add(&mut self, what: What, at: usize) -> Option<u32> {
        let o = match what {
            What::Kit(k) => {
                let (kd, name) = KIT_KINDS[k];
                MapObj { kind: kd, stem: super::super::stems::DRUMS as u8, name: name.into(), x: 0.0, width: 0.2, colour: colour(kd, None, 0), ..Default::default() }
            }
            What::Room => MapObj {
                kind: K_AMBIENCE,
                stem: 255,
                name: "ambience".into(),
                x: 0.0,
                width: 1.0,
                colour: colour(K_AMBIENCE, None, 0),
                ..Default::default()
            },
            What::Place { .. } | What::Piece(_) | What::Inst(_) => return None,
        };
        self.insert(o, what, at)
    }

    /// The drum network's whole frames up to `dfin` teach the song its
    /// pieces' levels, hits and places; the pieces that play from now on —
    /// the file's gates (kit.rs `gate`) on what the song has played so far:
    /// within 24 dB of the loudest piece, over −35 dB of the mix's loud
    /// level, eight hits — once, for good. Of those, the ones that become
    /// instruments of their own (the toms, the ride, the crash).
    fn learn_drums(&mut self, df: &DrumFeats, dfin: usize) -> Vec<usize> {
        let from = self.dlearn_next.max(self.f0).max(df.start());
        let to = dfin.saturating_sub(DLAG);
        if to <= from {
            return Vec::new();
        }
        // the candidates of [from, to) from a stretch reaching DLAG either side
        let s0 = from.saturating_sub(DLAG).max(df.start()).max(self.f0);
        let (lo, hi) = (2 * (from - s0), 2 * (to - s0));
        let mut seen = false;
        for p in 0..NP {
            let st = &mut self.dr[p];
            for j in from..to {
                let Some(f) = df.get(j).filter(|f| f.ok) else { continue };
                st.db.push(f.db[p]);
                for h in 0..2 {
                    st.flux.push(10.0 * (f.flux[p][h] + 1e-12).log10());
                }
                st.e += f.e[p];
                st.ef += f.ef[p];
                st.al += f.al[p];
                st.ar += f.ar[p];
                seen = true;
            }
            let (flux, pow) = onset_rows(df, p, s0, dfin);
            let top = flux_top(st, &flux);
            let own = drums::candidates(&flux, &pow, top, kit::DETECT[p].0);
            own.iter().filter(|c| (lo..hi).contains(&c.0)).for_each(|c| st.cand.push(c.1));
            let gate = if kit::DETECT[p].0 == kit::GATE_DETECT.0 { own } else { drums::candidates(&flux, &pow, top, kit::GATE_DETECT.0) };
            let new: Vec<f32> = gate.iter().filter(|c| (lo..hi).contains(&c.0)).map(|c| c.1).collect();
            new.iter().for_each(|l| st.gcand.push(*l));
            if let Some(loud) = st.gcand.at(95.0, -300.0) {
                st.ghits += new.iter().filter(|l| **l >= loud - kit::GATE_DETECT.1).count();
            }
        }
        self.drum_seen |= seen;
        self.dlearn_next = to;
        let mix = self.mix_loud();
        let loud: Vec<f32> = self.dr.iter().map(|s| s.db.all(99.5).map_or(-300.0, |v| v - mix)).collect();
        let top = loud.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let mut now = Vec::new();
        for (p, s) in self.dr.iter_mut().enumerate() {
            if !s.plays && loud[p] >= (kit::FLOOR_DB as f32).max(top - kit::UNDER_DB as f32) && s.ghits >= kit::MIN_HITS {
                s.plays = true;
                crate::aelog!("[SPATIAL] live: song {} — the drum network hears its {} play", self.n, kit::PIECES[p]);
                if LATE_PIECES.contains(&p) {
                    now.push(p);
                }
            }
        }
        now
    }

    /// A piece of the drum network as an instrument of its own (the toms, a
    /// cymbal), as a file's (`job::kit_pieces`).
    fn add_piece(&mut self, p: usize, at: usize) -> Option<u32> {
        let kd = if p == 2 { K_TOMS } else { K_CYMBAL };
        let index = if kd == K_CYMBAL { self.cymbals } else { 0 };
        let o = MapObj {
            kind: kd,
            stem: super::super::stems::DRUMS as u8,
            name: kit::PIECES[p].into(),
            x: self.piece_pan(p),
            width: if kd == K_CYMBAL { 0.35 } else { 0.2 },
            colour: colour(kd, None, index),
            ..Default::default()
        };
        let id = self.insert(o, What::Piece(p), at)?;
        if kd == K_CYMBAL {
            self.cymbals += 1;
        }
        Some(id)
    }

    /// A piece's place over the song so far: its mean level right against
    /// left (kit.rs).
    fn piece_pan(&self, p: usize) -> f32 {
        let s = &self.dr[p];
        ((s.ar - s.al) / (s.al + s.ar + 1e-12)).clamp(-1.0, 1.0) as f32
    }

    /// Piece `p`'s values from the drum network over frames [w0, w0 + n) —
    /// presence, energy, height, depth, coherence as a file's piece
    /// (`map::finish` against the song's loud level of it) —, its hits in
    /// [e0, e1) (seconds of the stretch), and per frame how far a kit piece
    /// has gone over to them from the drums' bands (0…1 over `SEAM` frames
    /// from where its frames begin). None: no frame of it there.
    ///
    /// A kit piece's (`bands`: its values from the drums' bands and their
    /// loud level) are on the bands' scale — the piece's levels moved by the
    /// two loud levels' difference — and where the network has no frame the
    /// bands' stand in: its presence and energy go on across the seam.
    #[allow(clippy::too_many_arguments)]
    fn drum_values(&self, p: usize, df: &DrumFeats, w0: usize, n: usize, e0: usize, e1: usize, bands: Option<(&Raw, f32)>) -> Option<([Vec<u8>; 5], Vec<f32>, Vec<f32>)> {
        let mix = self.mix_loud();
        let st = &self.dr[p];
        let mut db = bands.map_or_else(|| vec![-200f32; n], |b| b.0.db.clone());
        let mut y = bands.map_or_else(|| vec![0.5f32; n], |b| b.0.y.clone());
        let mut coh = bands.map_or_else(|| vec![0f32; n], |b| b.0.coh.clone());
        let (mut flux, mut pow) = (vec![0f32; 2 * n], vec![-200f32; 2 * n]);
        let mut wgt = vec![0f32; n];
        let mut mine = vec![false; n];
        // its frames running into the stretch
        let mut run = (1..=SEAM).take_while(|k| w0 >= *k && df.get(w0 - k).is_some_and(|f| f.ok)).count();
        let (mut e, mut ef) = (0f64, 0f64);
        let mut lv = Vec::new();
        for t in 0..n {
            match df.get(w0 + t).filter(|f| f.ok) {
                Some(f) => {
                    mine[t] = true;
                    lv.push(f.db[p] - mix);
                    coh[t] = f.coh[p];
                    flux[2 * t..2 * t + 2].copy_from_slice(&f.flux[p]);
                    pow[2 * t..2 * t + 2].copy_from_slice(&f.pow[p]);
                    e += f.e[p];
                    ef += f.ef[p];
                    run += 1;
                }
                None => run = 0,
            }
            wgt[t] = (run as f32 / SEAM as f32).min(1.0);
        }
        if lv.is_empty() {
            return None;
        }
        let lnf = (if st.e > 0.0 { st.ef / st.e } else if e > 0.0 { ef / e } else { 0.0 }) as f32;
        // its loud level: the song's so far, else the stretch's
        let loud_p = st.db.at(99.0, mix - 90.0).map(|v| v - mix).unwrap_or_else(|| {
            let mut s: Vec<f32> = lv.iter().cloned().filter(|d| *d > -90.0).collect();
            s.sort_by(|a, b| a.total_cmp(b));
            s.get(s.len() * 99 / 100).copied().unwrap_or(-60.0)
        });
        let (shift, loud) = bands.map_or((0.0, loud_p), |b| (b.1 - loud_p, b.1));
        let mut k = 0;
        for t in (0..n).filter(|&t| mine[t]) {
            db[t] = lv[k] + shift;
            y[t] = map::height_of(lnf);
            k += 1;
        }
        let vals = map::finish_with(&Raw { db, y, coh }, n, loud);
        let top = flux_top(st, &flux);
        let cand = drums::candidates(&flux, &pow, top, kit::DETECT[p].0);
        let floor = st.cand.at(95.0, -300.0).unwrap_or_else(|| kit::percentile32(&cand.iter().map(|c| c.1).collect::<Vec<f32>>(), 95.0)) - kit::DETECT[p].1;
        let (a, b) = (2 * (e0 - w0), 2 * (e1 - w0));
        let hits = cand
            .into_iter()
            .filter(|c| c.1 >= floor && c.0 >= a && c.0 < b)
            .map(|c| ((c.0 * kit::ON_HOP) as f64 / kit::SR) as f32)
            .collect();
        Some((vals, hits, wgt))
    }

    /// A new object in its slot: in a file's order until a scene has had
    /// the map, else at the end. Its id.
    pub(super) fn insert(&mut self, o: MapObj, what: What, born: usize) -> Option<u32> {
        if self.map.objects.len() >= MAX_OBJECTS {
            if !self.full_said {
                self.full_said = true;
                crate::aelog!("[SPATIAL] live: song {} has {} instruments — no more are added", self.n, MAX_OBJECTS);
            }
            return None;
        }
        let mut o = o;
        let n = self.map.frames;
        for v in [&mut o.presence, &mut o.energy, &mut o.y, &mut o.z, &mut o.coherence] {
            v.resize(n, 0);
        }
        let at = if self.shown {
            self.map.objects.len()
        } else {
            let key = |x: &MapObj| (family(x.kind), if is_kit(x.kind) { 0.0 } else { x.x });
            let (f, x) = key(&o);
            self.map.objects.iter().position(|q| {
                let (g, y) = key(q);
                g > f || (g == f && !is_kit(o.kind) && y > x)
            }).unwrap_or(self.map.objects.len())
        };
        self.next_id += 1;
        let id = self.next_id;
        self.map.objects.insert(at, o);
        self.meta.insert(at, Meta::new(id, what, born));
        // the notes of the objects after it move with them
        for n in self.map.notes.iter_mut().filter(|n| n.obj as usize >= at) {
            n.obj += 1;
        }
        Some(id)
    }

    /// Object `i`'s values for session frames [e0, e1), from a stretch of
    /// frames [e0 − MARGIN, w1); `whole`: the frames are whole (they teach it
    /// its place and width while it settles, its onsets' levels).
    #[allow(clippy::too_many_arguments)]
    fn write(&mut self, i: usize, feats: &Feats, drums: &DrumFeats, e0: usize, e1: usize, w1: usize, whole: bool) {
        if e1 <= e0 || matches!(self.meta[i].what, What::Inst(_)) {
            // (a note instrument's values come with its notes: `write_inst`)
            return;
        }
        let w0 = e0.saturating_sub(MARGIN).max(self.f0).max(feats.start());
        let w1 = w1.max(e1);
        // the frames still held only (a long break: the ones before are gone)
        let e0 = e0.max(w0);
        if w1 <= w0 || e1 <= e0 {
            return;
        }
        let n = w1 - w0;
        let ref_db = self.rough.d.ref_db;
        let what = self.meta[i].what;
        let (vals, hits, x_now, pan_now) = match what {
            What::Kit(k) => {
                let mut raw = vec![0f32; n * RAW];
                for t in 0..n {
                    if let Some(f) = feats.get(w0 + t).filter(|f| f.ok) {
                        raw[t * RAW..(t + 1) * RAW].copy_from_slice(&f.kit[k]);
                    }
                }
                let db: Vec<f32> = (0..n).map(|t| 10.0 * (raw[t * RAW] + EPS).log10() - ref_db).collect();
                let p90 = self.kit_db[k].at(90.0, ref_db - 90.0).map_or(-60.0, |v| v - ref_db);
                let loud = self.kit_db[k].at(99.0, ref_db - 90.0).map_or(-60.0, |v| v - ref_db);
                let fin = analysis::finish_band(&raw, n, ref_db, &vec![p90; n], false);
                let on: Vec<f32> = fin.iter().map(|f| f[5]).collect();
                let xs: Vec<f32> = (e0 - w0..e1 - w0).filter(|&t| fin[t][6] > 0.5).map(|t| fin[t][0]).collect();
                let r = Raw { db, y: fin.iter().map(|f| f[1]).collect(), coh: fin.iter().map(|f| f[7]).collect() };
                let vb = map::finish_with(&r, n, loud);
                let hb = peaks(&on, KIT_THR[k]);
                match self.drum_values(KIT_PIECE[k], drums, w0, n, e0, e1, Some((&r, loud))) {
                    None => (vb, hb, Some(xs), None),
                    Some((vd, hd, wgt)) => {
                        // over from the bands to the drum network: each value by its weight, each
                        // hit from the one that weighs more there, never one from each at once
                        let blend = |a: &[u8], b: &[u8]| -> Vec<u8> { (0..n).map(|t| (a[t] as f32 * (1.0 - wgt[t]) + b[t] as f32 * wgt[t]).round() as u8).collect() };
                        let v: [Vec<u8>; 5] = std::array::from_fn(|q| blend(&vb[q], &vd[q]));
                        let w_at = |h: f32| wgt[((h as f64 * FPS).round() as usize).min(n - 1)];
                        let mut all: Vec<(f32, bool)> = hb
                            .into_iter()
                            .filter(|h| w_at(*h) < 0.5)
                            .map(|h| (h, false))
                            .chain(hd.into_iter().filter(|h| w_at(*h) >= 0.5).map(|h| (h, true)))
                            .collect();
                        all.sort_by(|a, b| a.0.total_cmp(&b.0));
                        let mut hits: Vec<(f32, bool)> = Vec::with_capacity(all.len());
                        for h in all {
                            if hits.last().is_some_and(|l| l.1 != h.1 && ((h.0 - l.0) as f64) < SEAM_HIT_S) {
                                continue;
                            }
                            hits.push(h);
                        }
                        let p = KIT_PIECE[k];
                        let pan = (self.dr[p].e > 0.0).then(|| self.piece_pan(p));
                        (v, hits.into_iter().map(|h| h.0).collect(), if pan.is_some() { None } else { Some(xs) }, pan)
                    }
                }
            }
            What::Piece(p) => match self.drum_values(p, drums, w0, n, e0, e1, None) {
                Some((v, h, _)) => (v, h, None, Some(self.piece_pan(p))),
                None => (std::array::from_fn(|_| vec![0u8; n]), Vec::new(), None, None),
            },
            What::Inst(_) => return,
            What::Room => {
                let mut raw = vec![0f32; n * RAW];
                for t in 0..n {
                    if let Some(r) = self.rough_band(w0 + t) {
                        raw[t * RAW..(t + 1) * RAW].copy_from_slice(r);
                    }
                }
                let db: Vec<f32> = (0..n).map(|t| 10.0 * (raw[t * RAW] + EPS).log10() - ref_db).collect();
                let p90 = self.rough.d.band_p90_rough[B_AMB];
                let loud = self.room_db.at(99.0, ref_db - 90.0).map_or(-60.0, |v| v - ref_db);
                let fin = analysis::finish_band(&raw, n, ref_db, &vec![p90; n], true);
                let r = Raw { db, y: fin.iter().map(|f| f[1]).collect(), coh: fin.iter().map(|f| f[7]).collect() };
                let [presence, energy, y, _, coherence] = map::finish_with(&r, n, loud);
                let z = fin.iter().map(|f| (f[2].clamp(0.0, 1.0) * 255.0).round() as u8).collect();
                ([presence, energy, y, z, coherence], Vec::new(), None, None)
            }
            What::Place { p, range } => {
                let mix = self.mix_loud();
                let (mut db, mut y, mut coh) = (vec![-200f32; n], vec![0f32; n], vec![0f32; n]);
                let (mut flux, mut power) = (vec![0f64; n], vec![-200f32; n]);
                if let Some((lo, hi)) = range {
                    for t in 0..n {
                        let Some(f) = feats.get(w0 + t).filter(|f| f.ok) else { continue };
                        let s = &f.src[p];
                        let e: f32 = s.e[lo..hi].iter().sum();
                        if e > 0.0 {
                            y[t] = map::height_of(s.ef[lo..hi].iter().sum::<f32>() / e);
                            coh[t] = s.ec[lo..hi].iter().sum::<f32>() / e;
                        }
                        db[t] = 10.0 * (e * NORM + 1e-20).log10() - mix;
                        flux[t] = s.fl[lo..hi].iter().sum::<f32>() as f64;
                        power[t] = 10.0 * (e + 1e-20).log10();
                    }
                }
                let m = &self.meta[i];
                let loud = m.db.at(99.0, mix - 90.0).map_or(-60.0, |v| v - mix);
                let r = Raw { db: db.clone(), y, coh: coh.clone() };
                let v = map::finish_with(&r, n, loud);
                let top = m.flux.at(99.5, -200.0).map_or_else(|| percentile(&flux, 99.5), |d| 10f64.powf(d as f64 / 10.0)) + 1e-9;
                let cand = onset_candidates(&flux, &power, top);
                let floor = m.hits_lv.at(95.0, -300.0).unwrap_or_else(|| {
                    let mut l: Vec<f32> = cand.iter().map(|c| c.1).collect();
                    l.sort_by(|a, b| a.total_cmp(b));
                    l.get(l.len() * 95 / 100).copied().unwrap_or(-200.0)
                }) - ON_FLOOR_DB;
                let mut hits = Vec::new();
                let mut lvs = Vec::new();
                for (t, lv) in cand {
                    if t >= e0 - w0 && t < e1 - w0 {
                        lvs.push(lv);
                        if lv >= floor {
                            hits.push((t as f64 / FPS) as f32);
                        }
                    }
                }
                if whole {
                    let m = &mut self.meta[i];
                    lvs.into_iter().for_each(|l| m.hits_lv.push(l));
                    if !m.settled {
                        m.wdb.extend_from_slice(&db[e0 - w0..e1 - w0]);
                        m.wcoh.extend_from_slice(&coh[e0 - w0..e1 - w0]);
                    }
                }
                (v, hits, None, None)
            }
        };
        // the values into the song's frames
        let (s0, s1) = (e0 - self.f0, e1 - self.f0);
        let o = &mut self.map.objects[i];
        for (dst, src) in [&mut o.presence, &mut o.energy, &mut o.y, &mut o.z, &mut o.coherence].into_iter().zip(vals.iter()) {
            if dst.len() < s1 {
                dst.resize(s1, 0);
            }
            dst[s0..s1].copy_from_slice(&src[e0 - w0..e1 - w0]);
        }
        // its hits: those of [e0, e1) (seconds of the stretch → of the song); a place keeps its sound's
        // apart and flashes on its strong notes over them (Anton 3.10)
        let (t0, t1) = (s0 as f64 / FPS, s1 as f64 / FPS);
        let shift = (w0 - self.f0) as f64 / FPS;
        let new = hits.into_iter().map(|h| h as f64 + shift).filter(|h| *h >= t0 && *h < t1).map(|h| h as f32);
        if let What::Place { .. } = what {
            let m = &mut self.meta[i];
            m.ehits.retain(|h| (*h as f64) < t0);
            m.ehits.extend(new);
            o.hits = m.ehits.clone();
        } else {
            o.hits.retain(|h| (*h as f64) < t0);
            o.hits.extend(new);
        }
        // settling: its place (the kit's) and width over its first seconds
        let m = &mut self.meta[i];
        if whole && !m.settled {
            if let Some(px) = pan_now {
                o.x = px;
            } else if let Some(xs) = x_now {
                m.xs.extend(xs);
                let mut s = m.xs.clone();
                s.sort_by(|a, b| a.total_cmp(b));
                if let Some(x) = s.get(s.len() / 2) {
                    o.x = *x;
                }
            }
            if !m.wdb.is_empty() {
                o.width = width_of(&m.wdb, &m.wcoh);
            }
            if (e1.saturating_sub(m.born)) as f64 / FPS >= SETTLE_S {
                m.settled = true;
                m.xs = Vec::new();
                m.wdb = Vec::new();
                m.wcoh = Vec::new();
            }
        }
    }

    /// The instruments of split source `p` the splits weigh (`inst::Fig`):
    /// its own; before its first split, its places. Where each sits, and its
    /// pitch (its strong notes' in the song, by level).
    fn figs(&self, p: usize, placed: &[Placed]) -> Vec<Fig> {
        let t0 = self.f0 as f64 / FPS;
        let t1 = self.end.map_or(f64::INFINITY, |e| e as f64 / FPS);
        let mut sum: HashMap<u32, (f64, f64)> = HashMap::new();
        for q in placed.iter().filter(|q| q.n.on >= t0 && q.n.on < t1 && !q.n.ghost) {
            if let Some((_, id)) = q.to.filter(|t| t.0 == self.n) {
                let w = 10f64.powf(q.n.lvl as f64 / 10.0);
                let e = sum.entry(id).or_default();
                e.0 += w * q.n.key as f64;
                e.1 += w;
            }
        }
        let first = !self.split[p - INST].on;
        self.meta
            .iter()
            .zip(&self.map.objects)
            .filter(|(m, _)| match m.what {
                What::Inst(q) => q == p,
                What::Place { p: q, .. } => first && q == p,
                _ => false,
            })
            .map(|(m, o)| Fig { id: m.id, x: o.x, key: sum.get(&m.id).filter(|s| s.1 > 0.0).map_or(60.0, |s| (s.0 / s.1) as f32) })
            .collect()
    }

    /// The note instruments' sources due a split (`inst::Split::due`) split
    /// over the song's notes so far (`inst::split`), the bleed rule over every
    /// source's last clusters; their instruments born or held to (`assign`).
    /// On a source's first split its places no instrument took go silent.
    /// `split` false: no source is split (its notes stay on its places).
    fn split_sources(&mut self, placed: &mut [Vec<Placed>; 5], heard: f64, split: bool) {
        let t0 = self.f0 as f64 / FPS;
        let t1 = self.end.map_or(f64::INFINITY, |e| e as f64 / FPS);
        let idx: Vec<Vec<usize>> = (INST..PLACED.len())
            .map(|p| {
                (0..placed[p].len())
                    .filter(|&k| {
                        let q = &placed[p][k];
                        q.n.on >= t0 && q.n.on < t1 && q.like.as_ref().is_some_and(|l| l.row.is_some())
                    })
                    .collect()
            })
            .collect();
        let strong: Vec<usize> = (0..idx.len()).map(|q| idx[q].iter().filter(|&&k| !placed[INST + q][k].n.ghost).count()).collect();
        let due: Vec<bool> = (0..idx.len()).map(|q| self.split[q].due(strong[q], split)).collect();
        if !due.contains(&true) {
            return;
        }
        // a busy source's latest notes only (`inst::WINDOW` strong ones)
        let idx: Vec<Vec<usize>> = idx
            .into_iter()
            .enumerate()
            .map(|(q, v)| {
                let mut on: Vec<f64> = v.iter().map(|&k| &placed[INST + q][k].n).filter(|n| !n.ghost).map(|n| n.on).collect();
                if on.len() <= inst::WINDOW {
                    return v;
                }
                on.sort_by(|a, b| b.total_cmp(a));
                let from = on[inst::WINDOW - 1];
                v.into_iter().filter(|&k| placed[INST + q][k].n.on >= from).collect()
            })
            .collect();
        let ins_of = |q: usize| -> Vec<In> {
            idx[q]
                .iter()
                .map(|&k| {
                    let x = &placed[INST + q][k];
                    let l = x.like.as_ref().expect("a settled note's");
                    In { id: l.id, n: &x.n, row: l.row.as_ref().expect("its row"), lvl: l.lvl, parent: l.parent }
                })
                .collect()
        };
        let mut fresh: Vec<Option<Vec<Found>>> = (0..idx.len()).map(|q| due[q].then(|| inst::split(SOURCES[INST + q], &ins_of(q), t0))).collect();
        let kept = absorb((0..idx.len()).flat_map(|q| fresh[q].clone().unwrap_or_else(|| self.found[q].clone())).collect());
        let mut born = Vec::new();
        for q in (0..idx.len()).filter(|&q| due[q]) {
            let found = fresh[q].take().expect("split now");
            let keep: Vec<bool> = found.iter().map(|f| kept.iter().any(|k| k.stem == f.stem && k.main == f.main)).collect();
            let ins = ins_of(q);
            let to: Vec<Option<u32>> = idx[q].iter().map(|&k| placed[INST + q][k].to.filter(|t| t.0 == self.n).map(|t| t.1)).collect();
            let figs = self.figs(INST + q, &placed[INST + q]);
            let first = !self.split[q].on;
            let out = self.split[q].assign(&found, &keep, &ins, &to, &figs, heard);
            self.split[q].at = strong[q];
            let desc: Vec<(f32, f32, f64, f32)> = out.born.iter().map(|b| inst::describe(&ins, b)).collect();
            self.found[q] = found;
            born.push((q, first, out, desc));
        }
        for (q, first, out, desc) in born {
            let p = INST + q;
            let ids: Vec<u32> = (0..out.born.len()).map(|b| self.bear(p, out.over[b], desc[b], heard).unwrap_or(NOBODY)).collect();
            for (i, t) in out.give {
                let id = match t {
                    To::Fig(id) => id,
                    To::Born(b) => ids[b],
                };
                placed[p][idx[q][i]].to = Some((self.n, id));
            }
            if first {
                for m in self.meta.iter_mut() {
                    if let What::Place { p: q, range } = &mut m.what {
                        if *q == p {
                            *range = None;
                        }
                    }
                }
                crate::aelog!("[SPATIAL] live: song {} — its {} split into {} instrument(s)", self.n, nice(PLACED[p]), ids.len());
            }
            if !ids.is_empty() {
                self.version = self.version.wrapping_add(1);
            }
        }
    }

    /// A newborn instrument of source `p` (where it sits, how wide, its
    /// pitch, its sound's brightness): it takes over the place whose notes it
    /// holds (`over`), else a place of its source gone silent within
    /// `inst::TAKE_PAN` of it, else a new slot; its colour by its register and
    /// brightness, once (a place it takes over keeps its place and width). Its id.
    fn bear(&mut self, p: usize, over: Option<u32>, (x, width, pitch, tilt): (f32, f32, f64, f32), heard: f64) -> Option<u32> {
        let stem = PLACED[p];
        let kd = kind_of_stem(stem);
        let index = self.meta.iter().filter(|m| matches!(m.what, What::Inst(q) if q == p)).count();
        let col = colour(kd, Some(&Timbre { pitch: pitch as f32, tilt, ..Default::default() }), index);
        let place = |i: usize, silent: bool| matches!(self.meta[i].what, What::Place { p: q, range } if q == p && (!silent || range.is_none()));
        let take = over.and_then(|id| (0..self.meta.len()).find(|&i| self.meta[i].id == id && place(i, false))).or_else(|| {
            (0..self.meta.len())
                .filter(|&i| place(i, true) && (self.map.objects[i].x - x).abs() <= inst::TAKE_PAN)
                .min_by(|&a, &b| (self.map.objects[a].x - x).abs().total_cmp(&(self.map.objects[b].x - x).abs()))
        });
        let now = ((heard * FPS) as usize).max(self.f0);
        match take {
            Some(i) => {
                // from past what the place has written whole, and not before the place heard
                let sf = self.emit_fin.max(now);
                let o = &mut self.map.objects[i];
                let k = (sf - self.f0).checked_sub(1);
                let held = |v: &Vec<u8>| k.and_then(|k| v.get(k)).copied().unwrap_or(0);
                let hold = [held(&o.presence), held(&o.energy), held(&o.y), held(&o.z), held(&o.coherence)];
                o.colour = col;
                let m = &mut self.meta[i];
                m.what = What::Inst(p);
                m.seam = Some((sf, hold));
                m.settled = true;
                Some(m.id)
            }
            None => {
                let o = MapObj { kind: kd, stem: stem as u8, name: format!("{} {}", nice(stem), index + 1), x, width, colour: col, ..Default::default() };
                self.insert(o, What::Inst(p), now)
            }
        }
    }

    /// A split source's notes on its instruments: a settled note no split gave
    /// out goes to the nearest instrument, once; the stream's own for now to
    /// the nearest each time. Its newborn instruments' places and widths from
    /// their notes over their first ten seconds. The map's notes of them
    /// (`notes`); where the own ones went.
    fn inst_notes(&mut self, p: usize, placed: &mut [Placed], own: &[LNote], notes: &mut Vec<Note>, heard: f64) -> Vec<Option<u32>> {
        let t0 = self.f0 as f64 / FPS;
        let t1 = self.end.map_or(f64::INFINITY, |e| e as f64 / FPS);
        let inside = |n: &LNote| n.on >= t0 && n.on < t1;
        let figs = self.figs(p, placed);
        for q in placed.iter_mut().filter(|q| inside(&q.n)) {
            if q.to.is_none_or(|t| t.0 != self.n) {
                q.to = Some((self.n, inst::nearest(&figs, q.n.pan, q.n.key).unwrap_or(NOBODY)));
            }
        }
        let own_to: Vec<Option<u32>> = own.iter().map(|n| if inside(n) { inst::nearest(&figs, n.pan, n.key) } else { None }).collect();
        // a newborn's place and width settle over its first seconds
        let now = (heard * FPS) as usize;
        for i in 0..self.meta.len() {
            if !matches!(self.meta[i].what, What::Inst(q) if q == p) || self.meta[i].settled {
                continue;
            }
            let id = self.meta[i].id;
            let ins: Vec<In> = placed
                .iter()
                .filter(|q| q.to == Some((self.n, id)))
                .filter_map(|q| {
                    let l = q.like.as_ref()?;
                    Some(In { id: l.id, n: &q.n, row: l.row.as_ref()?, lvl: l.lvl, parent: l.parent })
                })
                .collect();
            if !ins.is_empty() {
                let (x, width, _, _) = inst::describe(&ins, &(0..ins.len()).collect::<Vec<_>>());
                let o = &mut self.map.objects[i];
                o.x = x;
                o.width = width;
            }
            if now.saturating_sub(self.meta[i].born) as f64 / FPS >= SETTLE_S {
                self.meta[i].settled = true;
            }
        }
        let slot = |id: u32| self.meta.iter().position(|m| m.id == id);
        // velocities on each instrument's own 30 dB: its sound's loud level (a file's), else its notes'
        let lv: Vec<f64> = placed.iter().filter(|q| inside(&q.n) && !q.n.ghost).map(|q| q.n.lvl as f64).collect();
        let notes_top = if lv.is_empty() { 0.0 } else { percentile(&lv, 99.0) };
        let note = |n: &LNote, s: usize| {
            let top = self.meta[s].ndb.all(99.0).map_or(notes_top, |v| v as f64);
            Note {
                obj: s.min(255) as u8,
                key: n.key,
                on: (n.on - t0) as f32,
                off: (n.off.min(t1) - t0) as f32,
                vel: ((n.lvl as f64 - (top - VEL_DB)) / VEL_DB).clamp(0.0, 1.0) as f32,
                ghost: n.ghost,
            }
        };
        for q in placed.iter().filter(|q| inside(&q.n)) {
            if let Some(s) = q.to.filter(|t| t.0 == self.n && t.1 != NOBODY).and_then(|t| slot(t.1)) {
                notes.push(note(&q.n, s));
            }
        }
        for (n, to) in own.iter().zip(&own_to) {
            if let Some(s) = to.and_then(slot) {
                notes.push(note(n, s));
            }
        }
        own_to
    }

    /// The split sources' instruments' values frame by frame from two
    /// seconds before the place heard to the last frame known (`write_inst`),
    /// and their hits: their strong notes' (a chord's once), and before one
    /// took over a place, the place's.
    #[allow(clippy::too_many_arguments)]
    fn write_insts(&mut self, placed: &[Vec<Placed>; 5], own: &[Vec<LNote>; 5], own_sound: &[Vec<Sound>; 5], own_to: &[Vec<Option<u32>>], feats: &Feats, heard: f64) {
        let t0 = self.f0 as f64 / FPS;
        let e1 = self.f0 + self.map.frames;
        let now = (heard * FPS) as usize;
        let e0 = now.saturating_sub((2.0 * FPS) as usize).max(self.f0);
        for i in 0..self.meta.len() {
            let What::Inst(p) = self.meta[i].what else { continue };
            let id = self.meta[i].id;
            let mut mine: Vec<(&LNote, &Sound)> =
                placed[p].iter().filter(|q| q.to == Some((self.n, id))).filter_map(|q| q.like.as_ref().map(|l| (&q.n, &l.sound))).collect();
            if let Some(to) = own_to.get(p - INST) {
                mine.extend(own[p].iter().zip(&own_sound[p]).zip(to).filter(|(_, t)| **t == Some(id)).map(|((n, s), _)| (n, s)));
            }
            self.write_inst(i, p, &mine, feats, e0, e1, now);
            let seam = self.meta[i].seam.map_or(f64::NEG_INFINITY, |s| s.0 as f64 / FPS - t0);
            let sound: Vec<f32> = self.meta[i].ehits.iter().copied().filter(|h| (*h as f64) < seam).collect();
            let ns: Vec<(f32, f32, bool)> = mine.iter().map(|(n, _)| ((n.on - t0) as f32, (n.off - t0) as f32, n.ghost)).collect();
            self.map.objects[i].hits = note_hits(&sound, &ns);
        }
    }

    /// Instrument `i`'s values (of source `p`) for session frames [e0, e1)
    /// from its notes (each with its sound), as a file's note instrument's
    /// (`job::note_obj`): its level its notes' sound against the song's loud
    /// level of it, its height its top note's of those within 10 dB of the
    /// loudest sounding (held), its coherence its source's place around it —
    /// over a stretch from `MARGIN` frames before; past where it took over a
    /// place, from the place's last values over half a second. The frames
    /// heard (below `heard`) teach it its loud level, once.
    #[allow(clippy::too_many_arguments)]
    fn write_inst(&mut self, i: usize, p: usize, mine: &[(&LNote, &Sound)], feats: &Feats, e0: usize, e1: usize, heard: usize) {
        let seam = self.meta[i].seam;
        let e0 = e0.max(seam.map_or(0, |s| s.0));
        let w0 = e0.saturating_sub(MARGIN).max(self.f0).max(feats.start());
        let e0 = e0.max(w0);
        if e1 <= e0 {
            return;
        }
        let n = e1 - w0;
        let mut e = vec![0f64; n];
        for (_, s) in mine {
            for (k, v) in s.e.iter().enumerate() {
                let f = s.f0 + k;
                if f >= w0 && f < e1 {
                    e[f - w0] += *v as f64;
                }
            }
        }
        let edb: Vec<f32> = e.iter().map(|v| (10.0 * (v + 1e-20).log10()) as f32).collect();
        let mix = self.mix_loud();
        let m = &mut self.meta[i];
        for f in m.ntaught.max(w0)..heard.min(e1) {
            m.ndb.push(edb[f - w0]);
        }
        m.ntaught = m.ntaught.max(heard.min(e1));
        let loud = m.ndb.all(99.0).map(|v| v - mix).unwrap_or_else(|| {
            let mut s: Vec<f32> = edb.iter().map(|v| v - mix).filter(|d| *d > -90.0).collect();
            s.sort_by(|a, b| a.total_cmp(b));
            s.get(s.len() * 99 / 100).copied().unwrap_or(-60.0)
        });
        let db: Vec<f32> = edb.iter().map(|v| v - mix).collect();
        // its height: the top note among its loud strong notes sounding, held (from the one before)
        let strong: Vec<&LNote> = mine.iter().map(|x| x.0).filter(|n| !n.ghost).collect();
        let mut sounding: Vec<Vec<(f32, f32)>> = vec![Vec::new(); n];
        for nt in &strong {
            let (a, b) = ((nt.on * FPS).ceil() as usize, (nt.off * FPS).floor() as usize);
            for f in a.max(w0)..(b + 1).min(e1) {
                sounding[f - w0].push((nt.lvl, nt.key));
            }
        }
        let lead: Vec<Option<f32>> = sounding
            .iter()
            .map(|v| {
                let loud = v.iter().map(|x| x.0).reduce(f32::max)?;
                Some(v.iter().filter(|x| x.0 >= loud - Y_WITHIN_DB as f32).map(|x| x.1).fold(f32::NEG_INFINITY, f32::max))
            })
            .collect();
        let before = strong.iter().filter(|nt| ((nt.on * FPS).ceil() as usize) < w0).max_by(|a, b| a.on.total_cmp(&b.on)).map(|nt| nt.key);
        let mut held = before.or_else(|| lead.iter().flatten().next().copied()).map_or(0.5, |k| y_of(k as f64));
        let y: Vec<f32> = lead
            .iter()
            .map(|l| {
                if let Some(k) = l {
                    held = y_of(*k as f64);
                }
                held
            })
            .collect();
        // its coherence: its source's place around it (a file's), else a file's with none
        let x = self.map.objects[i].x;
        let bin = |a: usize| a as f32 / PNB as f32 * 2.0 - 1.0;
        let dist = |&(a, b): &(usize, usize)| {
            let (lo, hi) = (bin(a), bin(b));
            if x < lo {
                lo - x
            } else if x > hi {
                x - hi
            } else {
                0.0
            }
        };
        let range = self.src[p].regs.iter().copied().min_by(|r, s| dist(r).total_cmp(&dist(s)));
        let mut coh = vec![0.7f32; n];
        if let Some((lo, hi)) = range {
            for (t, c) in coh.iter_mut().enumerate() {
                *c = 0.0;
                if let Some(f) = feats.get(w0 + t).filter(|f| f.ok) {
                    let s = &f.src[p];
                    let en: f32 = s.e[lo..hi].iter().sum();
                    if en > 0.0 {
                        *c = s.ec[lo..hi].iter().sum::<f32>() / en;
                    }
                }
            }
        }
        let vals = map::finish_with(&Raw { db, y, coh }, n, loud);
        let s1 = e1 - self.f0;
        let o = &mut self.map.objects[i];
        for (q, (dst, src)) in [&mut o.presence, &mut o.energy, &mut o.y, &mut o.z, &mut o.coherence].into_iter().zip(vals.iter()).enumerate() {
            if dst.len() < s1 {
                dst.resize(s1, 0);
            }
            for f in e0..e1 {
                // over from the place it took over: from its last values to these over half a second
                let (w, hold) = seam.map_or((1.0, 0.0), |s| (((f - s.0) as f32 / SEAM as f32).min(1.0), s.1[q] as f32));
                dst[f - self.f0] = (hold * (1.0 - w) + src[f - w0] as f32 * w).round() as u8;
            }
        }
    }
}

/// numpy's percentile of the flux over a stretch (no song level yet).
fn percentile(v: &[f64], q: f64) -> f64 {
    super::super::stems::percentile(v, q)
}

/// Piece `p`'s onset function over frames [a, b) (two values a frame): its
/// flux and its power (dB), nothing where the drum network has no frame.
fn onset_rows(df: &DrumFeats, p: usize, a: usize, b: usize) -> (Vec<f32>, Vec<f32>) {
    let n = b.saturating_sub(a);
    let (mut flux, mut pow) = (vec![0f32; 2 * n], vec![-200f32; 2 * n]);
    for t in 0..n {
        if let Some(f) = df.get(a + t).filter(|f| f.ok) {
            flux[2 * t..2 * t + 2].copy_from_slice(&f.flux[p]);
            pow[2 * t..2 * t + 2].copy_from_slice(&f.pow[p]);
        }
    }
    (flux, pow)
}

/// The flux a piece's hits are measured against: its 99.5th percentile over
/// the song so far (before the song has any, over the stretch at hand).
fn flux_top(st: &PieceStat, flux: &[f32]) -> f32 {
    st.flux.at(99.5, -150.0).map(|d| 10f32.powf(d / 10.0)).unwrap_or_else(|| kit::percentile32(flux, 99.5)) + 1e-9
}

fn family(kind: u8) -> usize {
    use super::super::{K_BASS, K_CYMBAL, K_GUITAR, K_OTHER, K_PIANO, K_TOMS, K_VOICE};
    match kind {
        K_KICK => 0,
        K_SNARE => 1,
        K_TOMS => 2,
        K_HATS => 3,
        K_CYMBAL => 4,
        K_BASS => 5,
        K_VOICE => 6,
        K_GUITAR => 7,
        K_PIANO => 8,
        K_OTHER => 9,
        K_AMBIENCE => 11,
        _ => 10,
    }
}

fn is_kit(kind: u8) -> bool {
    use super::super::{K_CYMBAL, K_TOMS};
    matches!(kind, K_KICK | K_SNARE | K_TOMS | K_HATS | K_CYMBAL)
}
