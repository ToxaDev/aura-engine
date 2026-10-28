//! The parts of a source by where they sit (Anton 27.09: "a backing voice on
//! the right should split off the voice, sound and fade; a second guitar
//! coming in on the right should appear there").
//!
//! Per source, per frame, its pan histogram (analysis.rs) is smoothed in time
//! and across pan; its peaks with a real dip between them (3 dB) are parts.
//! - The MAIN part keeps to its own peak — the nearest strong one — and moves
//!   to another only when that one is 6 dB louder for 0.3 s; it lives through
//!   breaths and rests (1.2 s) and goes out when the source is quiet longer.
//! - EXTRA parts are the other clear peaks, at least 0.2 away from the main,
//!   within `rel_db` of the loudest and above the source's own floor; one is
//!   born after `birth` frames in a row, keeps its slot while it lives, is
//!   held 0.3 s after its peak goes (fading), then its slot is free again. It
//!   remembers where the main part was when it was born — scenes fly it out
//!   from there.
//!
//! The tracking runs over the frames in order (a contiguous run of analysed
//! frames), so it is done again over the whole track when the separation
//! adds a segment — a few milliseconds.

use std::collections::HashMap;

use super::analysis::{depth, pan_centre, smooth, PartSrc, FPS, NB};

pub const MAIN_HOLD: f32 = 1.2;
pub const EXTRA_HOLD: f32 = 0.3;
/// An extra part must stand this long in its own place before it is born.
/// A reverb's tail or an echo in a side lasts a fraction of a second — the
/// ear hears it as the room of the sound it follows, not as another sound
/// (Anton 28.09 on Ave: the voice's "extras" lived 0.24 s, 14 a minute,
/// while the singer never moved; the few that lasted over a second were
/// real, like double-tracked guitars).
pub const EXTRA_BIRTH_S: f32 = 0.45;
/// ... and be a sound, not a wash: coherent between the channels (a panned
/// source is near 1, a stereo reverb's tail far less).
const EXTRA_COH: f32 = 0.6;
/// From this far to a side a sound is one-sided (coherence tells nothing there).
const ONE_SIDED: f32 = 0.7;
/// Frames a pending extra part may go unseen and still be pending.
const PENDING_GAP: usize = 4;
/// How slowly an extra part follows its peak (a time constant, s).
const EXTRA_FOLLOW_S: f32 = 0.15;
/// The main part changes place only to a peak this much louder (9 dB) for
/// this long: a kit's or a room's other side flares up often and briefly.
const MAIN_CHALLENGE: f32 = 8.0;
const MAIN_CHALLENGE_S: f32 = 0.8;
const MIN_SEP: f32 = 0.2;
const DIP: f32 = 0.5;
const MATCH: f32 = 0.15;
const MAIN_NEAR: f32 = 0.35;
const EPS: f32 = 1e-12;

/// A slot in a frame: which part is there and where its peak is.
#[derive(Clone, Copy, Default)]
pub struct Cell {
    /// 0: nobody
    pub id: u32,
    pub lo: u8,
    pub hi: u8,
    /// frames since its peak was last seen (it fades meanwhile)
    pub since: u8,
    pub main: bool,
    pub pos: f32,
}

/// A part's life.
#[derive(Clone, Copy)]
pub struct Info {
    pub birth: u32,
    /// where the main part was when this one was born: its place across, and
    /// the frame and slot to read the rest from
    pub origin_x: f32,
    pub origin_frame: u32,
    pub origin_slot: u16,
    /// frames it is held after its peak goes
    pub hold: u16,
}

struct Peak {
    e: f32,
    pos: f32,
    lo: usize,
    hi: usize,
    b: usize,
}

fn peaks(s: &[f32; NB]) -> Vec<Peak> {
    let mut out: Vec<Peak> = Vec::new();
    for b in 0..NB {
        let l = if b > 0 { s[b - 1] } else { 0.0 };
        let r = if b + 1 < NB { s[b + 1] } else { 0.0 };
        if !(s[b] >= l && s[b] > r && s[b] > 0.0) {
            continue;
        }
        let mut lo = b;
        while lo > 0 && s[lo - 1] < s[lo] {
            lo -= 1;
        }
        let mut hi = b;
        while hi + 1 < NB && s[hi + 1] < s[hi] {
            hi += 1;
        }
        let den = l - 2.0 * s[b] + r;
        let off = if b > 0 && b + 1 < NB && den.abs() > 1e-20 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
        let p = Peak { e: s[lo..=hi].iter().sum(), pos: pan_centre(b) + off * 2.0 / NB as f32, lo, hi, b };
        // a shallow dip to the peak before: one part, not two
        if let Some(q) = out.last_mut() {
            let dip = s[q.b..=p.b].iter().cloned().fold(f32::MAX, f32::min);
            if dip > DIP * s[q.b].min(s[p.b]) {
                if p.e > q.e {
                    let (e, lo) = (q.e + p.e, q.lo);
                    *q = Peak { e, lo, ..p };
                } else {
                    q.e += p.e;
                    q.hi = p.hi;
                }
                continue;
            }
        }
        out.push(p);
    }
    out
}

struct Track {
    id: u32,
    pos: f32,
    lo: usize,
    hi: usize,
    since: u32,
    count: usize,
    miss: usize,
    slot: usize,
    origin_x: f32,
    challenge: usize,
}

/// Follow the parts of one source over the frames `run` (contiguous), from
/// its histogram energies and coherences `he(j)`; write the cells of its
/// slots (`cells[j * slots + s]`) and the parts' lives.
pub fn track(
    def: &PartSrc,
    he: impl Fn(usize) -> ([f32; NB], [f32; NB]),
    run: std::ops::Range<usize>,
    ref_db: f32,
    p90: f32,
    slots: usize,
    next_id: &mut u32,
    cells: &mut [Cell],
    infos: &mut HashMap<u32, Info>,
) {
    let mut t = Tracker::new(def, ref_db, p90);
    for j in run {
        let (h, c) = he(j);
        t.step(j, &h, &c, slots, next_id, cells, infos);
    }
}

/// `track`'s state between frames, for a source whose frames come as it
/// plays (a live stream): the frames one at a time, in order, each with the
/// levels known by then.
pub struct Tracker<'a> {
    def: &'a PartSrc,
    a: f32,
    floor: f32,
    ref_db: f32,
    rel: f32,
    main_hold: u32,
    extra_hold: u32,
    birth: usize,
    follow: f32,
    follow_extra: f32,
    hs: [f32; NB],
    hsc: [f32; NB],
    main: Option<Track>,
    alive: Vec<Track>,
    pending: Vec<Track>,
}

impl<'a> Tracker<'a> {
    pub fn new(def: &'a PartSrc, ref_db: f32, p90: f32) -> Tracker<'a> {
        Tracker {
            def,
            a: 1.0 - (-(1.0 / FPS as f32) / def.tau).exp(),
            floor: (-42f32).max(p90 - 22.0),
            ref_db,
            rel: 10f32.powf(-def.rel_db / 10.0),
            main_hold: (MAIN_HOLD * FPS as f32) as u32,
            extra_hold: (EXTRA_HOLD * FPS as f32) as u32,
            birth: def.birth.max((EXTRA_BIRTH_S * FPS as f32) as usize),
            follow: 1.0 - (-(1.0 / FPS as f32) / def.follow_s).exp(),
            follow_extra: 1.0 - (-(1.0 / FPS as f32) / EXTRA_FOLLOW_S).exp(),
            hs: [0f32; NB],
            hsc: [0f32; NB],
            main: None,
            alive: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// The levels it measures against from now on (a stream's grow as it plays).
    pub fn set_levels(&mut self, ref_db: f32, p90: f32) {
        self.ref_db = ref_db;
        self.floor = (-42f32).max(p90 - 22.0);
    }

    /// Frame `j`: its histogram energies `h` and coherences `c`.
    #[allow(clippy::too_many_arguments)]
    pub fn step(&mut self, j: usize, h: &[f32; NB], c: &[f32; NB], slots: usize, next_id: &mut u32, cells: &mut [Cell], infos: &mut HashMap<u32, Info>) {
        let def = self.def;
        let (a, floor, ref_db, rel) = (self.a, self.floor, self.ref_db, self.rel);
        let (main_hold, extra_hold, birth, follow, follow_extra) =
            (self.main_hold, self.extra_hold, self.birth, self.follow, self.follow_extra);
        let Tracker { hs, hsc, main, alive, pending, .. } = self;
        let new_id = |next: &mut u32| {
            *next += 1;
            *next
        };
        let across = |v: &[f32; NB]| {
            let mut s = [0f32; NB];
            for b in 0..NB {
                let l = if b > 0 { v[b - 1] } else { 0.0 };
                let r = if b + 1 < NB { v[b + 1] } else { 0.0 };
                s[b] = 0.25 * l + 0.5 * v[b] + 0.25 * r;
            }
            s
        };
        for b in 0..NB {
            hs[b] += a * (h[b] - hs[b]);
            hsc[b] += a * (h[b] * c[b] - hsc[b]);
        }
        let s = across(&hs);
        let sc = across(&hsc);
        // a peak's coherence: its energy's mean
        let coh = |p: &Peak| sc[p.lo..=p.hi].iter().sum::<f32>() / (s[p.lo..=p.hi].iter().sum::<f32>() + EPS);
        let pk = peaks(&s);
        let pmax = pk.iter().map(|p| p.e).fold(0.0, f32::max);
        let mut good: Vec<&Peak> =
            pk.iter().filter(|p| 10.0 * (p.e + EPS).log10() - ref_db > floor && p.e >= pmax * rel).collect();
        good.sort_by(|x, y| y.e.partial_cmp(&x.e).unwrap_or(std::cmp::Ordering::Equal));

        // the main part: its own peak, or the strongest one when it has none
        let mut own: Option<&Peak> = None;
        if let Some(m) = main.as_mut() {
            own = good
                .iter()
                .filter(|p| (p.pos - m.pos).abs() < MAIN_NEAR && p.e >= pmax * 0.063)
                .min_by(|x, y| (x.pos - m.pos).abs().partial_cmp(&(y.pos - m.pos).abs()).unwrap())
                .copied();
            let top = good.first().copied();
            match (own, top) {
                (Some(o), Some(t)) if !std::ptr::eq(o, t) && t.e > o.e * MAIN_CHALLENGE => {
                    m.challenge += 1;
                    if m.challenge as f32 > MAIN_CHALLENGE_S * FPS as f32 {
                        own = Some(t);
                        m.challenge = 0;
                    }
                }
                _ => m.challenge = 0,
            }
        } else if let Some(t) = good.first().copied() {
            let id = new_id(next_id);
            infos.insert(id, Info { birth: j as u32, origin_x: t.pos, origin_frame: j as u32, origin_slot: def.slot0 as u16, hold: main_hold as u16 });
            *main = Some(Track { id, pos: t.pos, lo: t.lo, hi: t.hi, since: 0, count: 0, miss: 0, slot: def.slot0, origin_x: t.pos, challenge: 0 });
            own = Some(t);
        }
        let mut rest: Vec<&Peak> = Vec::new();
        if let Some(m) = main.as_mut() {
            if let Some(o) = own {
                m.pos += follow * (o.pos - m.pos);
                m.lo = o.lo;
                m.hi = o.hi;
                m.since = 0;
            } else {
                m.since += 1;
            }
            let mpos = m.pos;
            rest = good.iter().filter(|p| own.map_or(true, |o| !std::ptr::eq(**p, o)) && (p.pos - mpos).abs() >= MIN_SEP).copied().collect();
        }

        // extra parts: matched to the ones alive (or about to be) by place
        let mut used: Vec<u32> = Vec::new();
        for p in rest {
            let pick = |ts: &mut Vec<Track>, used: &Vec<u32>| -> Option<usize> {
                ts.iter()
                    .enumerate()
                    .filter(|(_, t)| !used.contains(&t.id) && (t.pos - p.pos).abs() < MATCH)
                    .min_by(|x, y| (x.1.pos - p.pos).abs().partial_cmp(&(y.1.pos - p.pos).abs()).unwrap())
                    .map(|(i, _)| i)
            };
            // a part that lives keeps to its peak whatever it is like now; a
            // wash (incoherent) is no sign of a new one — except far to a
            // side, where a sound is in one channel only and the other
            // carries other sounds: unalike, yet a place
            let t = if let Some(i) = pick(alive, &used) {
                &mut alive[i]
            } else if coh(p) < EXTRA_COH && p.pos.abs() < ONE_SIDED {
                continue;
            } else if let Some(i) = pick(pending, &used) {
                &mut pending[i]
            } else {
                let id = new_id(next_id);
                let origin_x = main.as_ref().map_or(p.pos, |m| m.pos);
                pending.push(Track { id, pos: p.pos, lo: p.lo, hi: p.hi, since: 0, count: 0, miss: 0, slot: 0, origin_x, challenge: 0 });
                pending.last_mut().unwrap()
            };
            used.push(t.id);
            t.pos += follow_extra * (p.pos - t.pos);
            t.lo = p.lo;
            t.hi = p.hi;
            t.since = 0;
            t.miss = 0;
            t.count += 1;
        }
        let mut i = 0;
        while i < pending.len() {
            if !used.contains(&pending[i].id) {
                pending[i].miss += 1;
                if pending[i].miss > PENDING_GAP {
                    pending.swap_remove(i);
                    continue;
                }
            } else if pending[i].count >= birth && alive.len() < def.extras {
                // born: the first free slot after the main one
                let taken: Vec<usize> = alive.iter().map(|t| t.slot).collect();
                if let Some(slot) = (def.slot0 + 1..=def.slot0 + def.extras).find(|s| !taken.contains(s)) {
                    let mut t = pending.swap_remove(i);
                    t.slot = slot;
                    infos.insert(t.id, Info {
                        birth: j as u32,
                        origin_x: t.origin_x,
                        origin_frame: j as u32,
                        origin_slot: def.slot0 as u16,
                        hold: extra_hold as u16,
                    });
                    alive.push(t);
                    continue;
                }
            }
            i += 1;
        }
        alive.retain_mut(|t| {
            if !used.contains(&t.id) {
                t.since += 1;
            }
            t.since <= extra_hold
        });
        for t in alive.iter() {
            cells[j * slots + t.slot] =
                Cell { id: t.id, lo: t.lo as u8, hi: t.hi as u8, since: t.since.min(255) as u8, main: false, pos: t.pos };
        }
        if main.as_ref().is_some_and(|m| m.since > main_hold) {
            *main = None;
        }
        if let Some(m) = main.as_ref() {
            cells[j * slots + m.slot] = Cell { id: m.id, lo: m.lo as u8, hi: m.hi as u8, since: m.since.min(255) as u8, main: true, pos: m.pos };
        }
    }
}

/// A part's values in one frame from its cell and the histogram:
/// (energy, height, coherence, pan spread).
pub fn measure(c: &Cell, he: &[f32; NB], hy: &[f32; NB], hc: &[f32; NB]) -> (f32, f32, f32, f32) {
    let (lo, hi) = (c.lo as usize, (c.hi as usize).min(NB - 1));
    let (mut e, mut y, mut co, mut ss) = (0f32, 0f32, 0f32, 0f32);
    for b in lo..=hi {
        e += he[b];
        y += he[b] * hy[b];
        co += he[b] * hc[b];
        ss += he[b] * (pan_centre(b) - c.pos).powi(2);
    }
    if e <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (e, y / e, co / e, (ss / e).sqrt())
}

/// What a scene sees of one part slot over `n` frames: `cells[i]` its cell in
/// frame i (id 0: nobody), `m(i, c)` → measure() of it, `origin(info)` → the
/// place the part came from. Out per frame: presence, energy, main, x, y, z,
/// width, onset, coherence, age, origin x y z.
pub fn finish(
    cells: &[Cell],
    first_frame: usize,
    m: impl Fn(usize, &Cell) -> (f32, f32, f32, f32),
    infos: &HashMap<u32, Info>,
    origin: impl Fn(&Info) -> [f32; 3],
    ref_db: f32,
) -> Vec<[f32; 13]> {
    let n = cells.len();
    let mut out = vec![[0f32; 13]; n];
    let dt = 1.0 / FPS as f32;
    let rel = (-dt / 0.1).exp();
    let dec = (-dt / 0.12).exp();
    let mut i = 0;
    while i < n {
        let id = cells[i].id;
        let mut e = i + 1;
        while e < n && cells[e].id == id {
            e += 1;
        }
        if id != 0 {
            let run = i..e;
            let len = e - i;
            let info = infos.get(&id).copied();
            let raw: Vec<(f32, f32, f32, f32)> = run.clone().map(|k| m(first_frame + k, &cells[k])).collect();
            let db: Vec<f32> = raw.iter().map(|r| 10.0 * (r.0 + EPS).log10() - ref_db).collect();
            let hold = info.map_or((MAIN_HOLD * FPS as f32) as f32, |f| f.hold as f32).max(1.0);
            let presence: Vec<f32> = run
                .clone()
                .map(|k| {
                    let age = info.map_or(1e9, |f| (first_frame + k) as f32 - f.birth as f32);
                    // a main part comes with its sound; an extra one lights up
                    // gently where it stands (it was heard there for a while)
                    let fade_s = if cells[k].main { 0.06 } else { 0.25 };
                    let fade_in = (age / (fade_s * FPS as f32)).clamp(0.0, 1.0);
                    let t = (cells[k].since as f32 / hold).clamp(0.0, 1.0);
                    let fade_out = 1.0 - t * t * (3.0 - 2.0 * t);
                    fade_in * fade_out
                })
                .collect();
            let one = vec![1f32; len];
            let mut x: Vec<f32> = run.clone().map(|k| cells[k].pos).collect();
            smooth(&mut x, 0.05, &one);
            let mut y: Vec<f32> = raw.iter().map(|r| r.1).collect();
            let mut coh: Vec<f32> = raw.iter().map(|r| r.2).collect();
            smooth(&mut y, 0.08, &one);
            smooth(&mut coh, 0.1, &one);
            let mut z: Vec<f32> = (0..len).map(|k| depth(coh[k], db[k])).collect();
            smooth(&mut z, 0.15, &one);
            let mut width: Vec<f32> = (0..len).map(|k| (1.5 * raw[k].3).max(1.0 - coh[k]).clamp(0.0, 1.0)).collect();
            smooth(&mut width, 0.1, &one);
            let o = info.map_or([x[0], y[0], z[0]], |f| origin(&f));
            let (mut env, mut en) = (0f32, 0f32);
            for k in 0..len {
                let jump = if k > 0 { db[k] - db[k - 1] } else { 0.0 };
                env = (((jump - 2.5) / 6.0).clamp(0.0, 1.0) * presence[k]).max(env * dec);
                en = ((db[k] + 45.0) / 45.0).clamp(0.0, 1.0).max(en * rel);
                let age = info.map_or(0.0, |f| ((first_frame + i + k) as f32 - f.birth as f32) / FPS as f32);
                out[i + k] = [
                    presence[k],
                    en * presence[k].max(0.0),
                    cells[i + k].main as u8 as f32,
                    x[k].clamp(-1.0, 1.0),
                    y[k],
                    z[k],
                    width[k],
                    env,
                    coh[k],
                    age.max(0.0),
                    o[0],
                    o[1],
                    o[2],
                ];
            }
        }
        i = e;
    }
    out
}

/// A part's height and depth in a frame (for the place a part came from).
pub fn place(c: &Cell, he: &[f32; NB], hy: &[f32; NB], hc: &[f32; NB], ref_db: f32) -> [f32; 3] {
    let (e, y, coh, _) = measure(c, he, hy, hc);
    [c.pos, y, depth(coh, 10.0 * (e + EPS).log10() - ref_db)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::analysis::FULL_PARTS;

    fn bump(at: f32, e: f32, h: &mut [f32; NB]) {
        for b in 0..NB {
            let d = (pan_centre(b) - at) / 0.08;
            h[b] += e * (-d * d).exp();
        }
    }

    const COHERENT: [f32; NB] = [1.0; NB];

    /// A voice in the middle all along, a backing voice on the right for one
    /// second: the main part stays (one id), an extra part is born on the
    /// right, from the main's place, lives about its second, and goes.
    #[test]
    fn a_backing_voice_splits_off_and_goes() {
        let def = &FULL_PARTS[0];
        let n = (4.0 * FPS) as usize;
        let (on, off) = ((1.0 * FPS) as usize, (2.0 * FPS) as usize);
        let he = |j: usize| {
            let mut h = [0f32; NB];
            bump(0.0, 1.0, &mut h);
            if j >= on && j < off {
                bump(0.6, 0.1, &mut h);
            }
            (h, COHERENT)
        };
        let slots = 32;
        let mut cells = vec![Cell::default(); n * slots];
        let mut infos = HashMap::new();
        let mut next = 0;
        track(def, he, 0..n, 0.0, 0.0, slots, &mut next, &mut cells, &mut infos);
        let main_ids: std::collections::HashSet<u32> = (5..n).map(|j| cells[j * slots + def.slot0].id).collect();
        assert_eq!(main_ids.len(), 1, "one main part all along");
        assert!((5..n).all(|j| cells[j * slots + def.slot0].main));
        let extra = |j: usize| cells[j * slots + def.slot0 + 1];
        assert_eq!(extra(on - 1).id, 0);
        let mid = (1.5 * FPS) as usize;
        assert!(extra(mid).id != 0 && (extra(mid).pos - 0.6).abs() < 0.1, "extra at {}", extra(mid).pos);
        let info = infos[&extra(mid).id];
        assert!(info.origin_x.abs() < 0.1, "born from the main's place");
        let late = info.birth as usize - on;
        assert!((late as f32) < (EXTRA_BIRTH_S + 0.15) * FPS as f32, "born {late} frames after it began");
        assert_eq!(extra((2.5 * FPS) as usize).id, 0, "gone after its hold");
    }

    /// A voice in the middle with its reverb: after each phrase a side tail
    /// as loud as the dying voice for a quarter second, and a steady wash on
    /// both sides, incoherent. Nothing splits off and the voice never moves
    /// (Anton 28.09, Ave: lights flew about while the singer stood still).
    #[test]
    fn a_reverb_tail_is_no_part() {
        let def = &FULL_PARTS[0];
        let n = (6.0 * FPS) as usize;
        let phrase = (0.8 * FPS) as usize;
        let tail = (0.25 * FPS) as usize;
        let he = |j: usize| {
            let mut h = [0f32; NB];
            let mut c = [0.3f32; NB];
            let k = j % phrase;
            if k < phrase - tail {
                bump(0.0, 1.0, &mut h);
            } else {
                bump(0.0, 0.1, &mut h);
                bump(if (j / phrase) % 2 == 0 { -0.6 } else { 0.6 }, 0.1, &mut h);
            }
            // the wash: spread evenly across (no place of its own)
            for v in h.iter_mut() {
                *v += 0.01;
            }
            for b in NB / 2 - 2..NB / 2 + 2 {
                c[b] = 0.95;
            }
            (h, c)
        };
        let slots = 32;
        let mut cells = vec![Cell::default(); n * slots];
        let mut infos = HashMap::new();
        let mut next = 0;
        track(def, he, 0..n, 0.0, 0.0, slots, &mut next, &mut cells, &mut infos);
        for j in 5..n {
            for e in 1..=def.extras {
                assert_eq!(cells[j * slots + def.slot0 + e].id, 0, "an extra part at frame {j}");
            }
            let m = cells[j * slots + def.slot0];
            assert!(m.id == 0 || m.pos.abs() < 0.15, "the voice moved to {} at frame {j}", m.pos);
        }
    }

    /// Two equal parts, left and right: the main keeps to one of them, the
    /// other is one extra part all along (no flicker between them).
    #[test]
    fn two_equal_guitars_are_two_steady_parts() {
        let def = &FULL_PARTS[2];
        let n = (3.0 * FPS) as usize;
        let he = |j: usize| {
            let mut h = [0f32; NB];
            let w = 1.0 + 0.3 * ((j as f32) * 0.37).sin();
            bump(-0.45, w, &mut h);
            bump(0.45, 2.0 - w, &mut h);
            (h, COHERENT)
        };
        let slots = 32;
        let mut cells = vec![Cell::default(); n * slots];
        let mut infos = HashMap::new();
        let mut next = 0;
        track(def, he, 0..n, 0.0, 0.0, slots, &mut next, &mut cells, &mut infos);
        let from = (0.5 * FPS) as usize;
        let mains: std::collections::HashSet<u32> = (from..n).map(|j| cells[j * slots + def.slot0].id).collect();
        let extras: std::collections::HashSet<u32> = (from..n).map(|j| cells[j * slots + def.slot0 + 1].id).collect();
        assert_eq!(mains.len(), 1);
        assert_eq!(extras.len(), 1);
        let (a, b) = (cells[n / 2 * slots + def.slot0].pos, cells[n / 2 * slots + def.slot0 + 1].pos);
        assert!((a.abs() - 0.45).abs() < 0.1 && (b.abs() - 0.45).abs() < 0.1 && a * b < 0.0, "{a} {b}");
    }
}
