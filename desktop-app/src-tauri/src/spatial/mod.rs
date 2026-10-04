//! Spatial scenes (Anton 27.09): the song's instruments as objects in space —
//! where each one sits left to right, how far (dry and loud = near, diffuse and
//! quiet = far), how high (its pitch), how loud, when it strikes. The
//! visualization draws them; this module finds them.
//!
//! A track is decoded once at 44.1 kHz; a rough layer comes from the mix alone
//! at once (the bass and the highs as bands, the ambience, and the mix's own
//! parts by where they sit — of no known instrument); then, with the spatial
//! pack installed, the separation network splits it into six sources segment
//! by segment, starting where the listener is, and the full layer replaces the
//! rough one as it comes: the bass, kick, snare and hats as bands, and the
//! voice, the drums' toms and cymbals, guitars, piano and the rest as PARTS by
//! where they sit (parts.rs: a main part per source, and extra ones that split
//! off, sound and go — Anton's second round, 27.09). On a GPU a song takes
//! seconds; on a CPU about a tenth of its length.
//!
//! 32 object slots (a slot keeps its object while it lives):
//! 0–3 voice parts (0 the main), 4 bass, 5 kick, 6 snare, 7 hats, 8–11 drum
//! parts, 12–15 guitar parts, 16–17 piano parts, 18–23 other parts, 24 the
//! ambience, 25–31 the mix's parts (the rough layer only).
//!
//! Scenes ask for a window around what is heard (`span`), in the track's own
//! time, like the spectrum slices; asking is also what keeps the work going —
//! nobody asks for ten seconds, and the worker rests.

pub mod analysis;
pub mod core;
pub mod decode;
pub mod job;
pub mod kit;
pub mod lineup;
pub mod live;
pub mod map;
pub mod notes;
pub mod pack;
pub mod parts;
pub mod place;
pub mod stems;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use analysis::{BANDS, FPS, FULL_PARTS, HOP, NB, RAW, ROUGH_PARTS, WIDE, WRAW};
use core::SEG;
#[cfg(test)]
use core::SOURCES;
use parts::{Cell, Info};

pub const SLOTS: usize = 32;
pub const VALS: usize = 16;
// What an object is (its kind).
pub const K_VOICE: u8 = 1;
pub const K_BASS: u8 = 2;
pub const K_KICK: u8 = 3;
pub const K_SNARE: u8 = 4;
pub const K_HATS: u8 = 5;
pub const K_DRUMS: u8 = 6;
pub const K_GUITAR: u8 = 7;
pub const K_PIANO: u8 = 8;
pub const K_OTHER: u8 = 9;
pub const K_AMBIENCE: u8 = 10;
pub const K_MIX: u8 = 11;
// the lineup test's kit pieces (lineup.rs)
pub const K_TOMS: u8 = 12;
pub const K_CYMBAL: u8 = 13;
const KIND_NAMES: [&str; 14] =
    ["", "voice", "bass", "kick", "snare", "hats", "drums", "guitar", "piano", "other", "ambience", "mix", "toms", "cymbal"];
/// Band objects: (band, slot, kind).
const BAND_SLOTS: [(usize, usize, u8); BANDS] = [
    (analysis::B_BASS, 4, K_BASS),
    (analysis::B_KICK, 5, K_KICK),
    (analysis::B_SNARE, 6, K_SNARE),
    (analysis::B_HATS, 7, K_HATS),
    (analysis::B_AMB, 24, K_AMBIENCE),
];
/// The first slot of the rough layer's parts (they show only where the
/// separation has not reached).
const ROUGH_SLOT0: usize = 25;
/// The separated sources' wide layers (analysis.rs `WIDE`: voice, guitar,
/// piano, other) take slots 25… where the separation has been — the rough
/// layer's parts show only where it has not.
const WIDE_N: usize = WIDE.len();
const WIDE_SLOT0: usize = 25;
/// A wide layer shows when its diffuse part is no more than this far under the
/// focused one (dB): a sound's own reverb stays further under it (Ocean Wind's
/// lead alone: −9 dB; with the pad behind: −3…−5 dB), and it is full at 4 dB
/// closer. An instrument recorded wide (two microphones, a double-tracked
/// wall) shows its layer too — alike to the ear, not told apart by any cue
/// tried (28.09: level, envelope, register, a rise over the track's rule):
/// scenes draw the layer as the width behind the sound, a haze, not a light.
const WIDE_GATE_DB: f32 = 8.0;

/// Segments start this far apart (demucs' overlap of a quarter).
const STRIDE: usize = SEG * 3 / 4;
#[cfg(test)]
/// A segment reaches this far before and after the part of the track it is
/// used for (its "region"), so every frame of the region has its context.
const OV2: usize = (SEG - STRIDE) / 2;
const EPS: f32 = 1e-12;

/// Pan histograms of a layer: per frame and source, per pan bin its energy,
/// height (0…1) and coherence (the last two as u16).
struct Hist {
    p: usize,
    e: Vec<f32>,
    y: Vec<u16>,
    c: Vec<u16>,
}

impl Hist {
    fn new(frames: usize, p: usize) -> Hist {
        Hist { p, e: vec![0.0; frames * p * NB], y: vec![0; frames * p * NB], c: vec![0; frames * p * NB] }
    }
    fn put(&mut self, first: usize, reg: &analysis::Region) {
        let o = first * self.p * NB;
        let n = reg.he.len();
        self.e[o..o + n].copy_from_slice(&reg.he);
        for i in 0..n {
            self.y[o + i] = (analysis::height(reg.hf[i]) * 65535.0).round() as u16;
            self.c[o + i] = (reg.hc[i].clamp(0.0, 1.0) * 65535.0).round() as u16;
        }
    }
    fn e(&self, j: usize, p: usize) -> [f32; NB] {
        let o = (j * self.p + p) * NB;
        let mut a = [0f32; NB];
        a.copy_from_slice(&self.e[o..o + NB]);
        a
    }
    /// Energy and coherence (what the parts are followed by).
    fn ec(&self, j: usize, p: usize) -> ([f32; NB], [f32; NB]) {
        let o = (j * self.p + p) * NB;
        let (mut e, mut c) = ([0f32; NB], [0f32; NB]);
        for b in 0..NB {
            e[b] = self.e[o + b];
            c[b] = self.c[o + b] as f32 / 65535.0;
        }
        (e, c)
    }
    fn all(&self, j: usize, p: usize) -> ([f32; NB], [f32; NB], [f32; NB]) {
        let o = (j * self.p + p) * NB;
        let (mut e, mut y, mut c) = ([0f32; NB], [0f32; NB], [0f32; NB]);
        for b in 0..NB {
            e[b] = self.e[o + b];
            y[b] = self.y[o + b] as f32 / 65535.0;
            c[b] = self.c[o + b] as f32 / 65535.0;
        }
        (e, y, c)
    }
}

struct TrackData {
    path: String,
    /// Samples at 44.1 kHz and analysis frames.
    len: usize,
    frames: usize,
    ref_db: f32,
    rough_band: Vec<f32>,
    full_band: Vec<f32>,
    rough_h: Hist,
    full_h: Hist,
    seg_done: Vec<bool>,
    /// frames × SLOTS: who is in each part slot (the full layer's parts and the rough layer's)
    cells: Vec<Cell>,
    full_infos: HashMap<u32, Info>,
    rough_infos: HashMap<u32, Info>,
    band_p90_full: [f32; BANDS],
    band_p90_rough: [f32; BANDS],
    /// the full layer's wide layers: frames × WIDE × WRAW, and their loud levels
    full_wide: Vec<f32>,
    wide_p90: [f32; WIDE_N],
    gpu: Option<bool>,
    error: Option<String>,
    /// the lineup test's inventory, when a test run has one (lineup.rs)
    lineup: Option<lineup::Lineup>,
    /// the track's map (the "with instruments" tier), when found or made
    map: Option<Arc<map::TrackMap>>,
    /// the job's progress 0…1, and whether it has run (made the map or failed)
    progress: f32,
    job_done: bool,
}

impl TrackData {
    fn failed(path: &str, e: String) -> TrackData {
        TrackData {
            path: path.to_string(), len: 0, frames: 0, ref_db: 0.0, rough_band: Vec::new(), full_band: Vec::new(),
            rough_h: Hist::new(0, 1), full_h: Hist::new(0, FULL_PARTS.len()), seg_done: Vec::new(), cells: Vec::new(),
            full_infos: HashMap::new(), rough_infos: HashMap::new(), band_p90_full: [-60.0; BANDS],
            band_p90_rough: [-60.0; BANDS], full_wide: Vec::new(), wide_p90: [-60.0; WIDE_N], gpu: None, error: Some(e),
            lineup: None,
            map: None,
            progress: 0.0,
            job_done: true,
        }
    }
    fn segs(&self) -> usize {
        self.seg_done.len()
    }
    fn done(&self) -> usize {
        self.seg_done.iter().filter(|d| **d).count()
    }
    fn seg_of(&self, j: usize) -> usize {
        (j * HOP / STRIDE).min(self.segs().saturating_sub(1))
    }
    fn frame_done(&self, j: usize) -> bool {
        !self.seg_done.is_empty() && self.seg_done[self.seg_of(j)]
    }
    #[cfg(test)]
    /// Segment k's frames.
    fn seg_frames(&self, k: usize) -> std::ops::Range<usize> {
        let a = k * STRIDE;
        let b = ((k + 1) * STRIDE).min(self.len);
        (a + HOP - 1) / HOP..((b + HOP - 1) / HOP).min(self.frames)
    }

    #[cfg(test)]
    /// The full layer's parts followed again over every analysed run, and its
    /// bands' loud levels (after a segment came).
    fn retrack_full(&mut self) {
        for j in 0..self.frames {
            for s in 0..ROUGH_SLOT0 {
                self.cells[j * SLOTS + s] = Cell::default();
            }
        }
        self.full_infos.clear();
        let ready: Vec<usize> = (0..self.frames).filter(|&j| self.frame_done(j)).collect();
        for (b, _, _) in BAND_SLOTS {
            self.band_p90_full[b] = p90(ready.iter().map(|&j| self.full_band[(j * BANDS + b) * RAW]), self.ref_db);
        }
        for w in 0..WIDE_N {
            self.wide_p90[w] = p90(ready.iter().map(|&j| self.full_wide[(j * WIDE_N + w) * WRAW]), self.ref_db);
        }
        let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
        for k in 0..self.segs() {
            if !self.seg_done[k] {
                continue;
            }
            let f = self.seg_frames(k);
            match runs.last_mut() {
                Some(r) if r.end == f.start => r.end = f.end,
                _ => runs.push(f),
            }
        }
        let mut next = 0u32;
        for (p, def) in FULL_PARTS.iter().enumerate() {
            let src_p90 = p90(ready.iter().map(|&j| self.full_h.e(j, p).iter().sum::<f32>()), self.ref_db);
            for run in &runs {
                let h = &self.full_h;
                parts::track(def, |j| h.ec(j, p), run.clone(), self.ref_db, src_p90, SLOTS, &mut next, &mut self.cells, &mut self.full_infos);
            }
        }
    }
}

/// The 90th percentile of energies (as dB under `ref_db`), of the ones above
/// −90 dB; −60 when there are none.
fn p90(e: impl Iterator<Item = f32>, ref_db: f32) -> f32 {
    let mut v: Vec<f32> = e.map(|x| 10.0 * (x + EPS).log10() - ref_db).filter(|d| *d > -90.0).collect();
    if v.is_empty() {
        return -60.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() * 9 / 10]
}

struct Want {
    path: String,
    at_s: f64,
    when: Instant,
}

struct Shared {
    tracks: Mutex<VecDeque<Arc<Mutex<TrackData>>>>,
    want: Mutex<Option<Want>>,
    wake: Condvar,
}

fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| {
        std::thread::Builder::new()
            .name("spatial".into())
            .spawn(worker)
            .expect("spatial worker");
        Shared { tracks: Mutex::new(VecDeque::new()), want: Mutex::new(None), wake: Condvar::new() }
    })
}

fn find(path: &str) -> Option<Arc<Mutex<TrackData>>> {
    shared().tracks.lock().unwrap().iter().find(|t| t.lock().unwrap().path == path).cloned()
}

/// Keep the objects of the last two tracks (tens of MB each: the pan
/// histograms).
fn keep(t: Arc<Mutex<TrackData>>) {
    let mut q = shared().tracks.lock().unwrap();
    q.push_front(t);
    while q.len() > 2 {
        q.pop_back();
    }
}

/// Ask for `path` around second `at_s` (a scene is showing it).
fn want(path: &str, at_s: f64) {
    let s = shared();
    *s.want.lock().unwrap() = Some(Want { path: path.to_string(), at_s, when: Instant::now() });
    s.wake.notify_all();
}

/// Nobody asked for this long: the worker rests (and lets the GPU go).
const IDLE: Duration = Duration::from_secs(10);

fn worker() {
    let s = shared();
    // the next track of the queue, once made ahead (or tried)
    let mut ahead: Option<String> = None;
    // the track being worked on, decoded
    let mut audio: Option<(String, Arc<Vec<f32>>, Arc<Vec<f32>>)> = None;
    let mut busy = false;
    loop {
        let w = {
            let mut g = s.want.lock().unwrap();
            if !busy {
                g = s.wake.wait_timeout(g, Duration::from_millis(500)).unwrap().0;
            }
            g.as_ref().filter(|w| w.when.elapsed() < IDLE).map(|w| (w.path.clone(), w.at_s))
        };
        busy = false;
        let Some((path, _)) = w else {
            audio = None;
            continue;
        };
        let stop = || s.want.lock().unwrap().as_ref().map_or(true, |w| w.path != path || w.when.elapsed() > IDLE);

        // 1. the track decoded and its rough layer
        let data = match find(&path) {
            Some(d) => d,
            None => {
                let t0 = Instant::now();
                let (l, r) = match decode::decode_44k(std::path::Path::new(&path), &stop) {
                    Ok(v) => v,
                    Err(e) => {
                        crate::aelog!("[SPATIAL] cannot decode {path}: {e}");
                        keep(Arc::new(Mutex::new(TrackData::failed(&path, e))));
                        continue;
                    }
                };
                let d = Arc::new(Mutex::new(rough_layer(&path, &l, &r)));
                crate::aelog!(
                    "[SPATIAL] {}: decoded + rough objects in {:.1} s ({:.0} s of sound)",
                    short(&path), t0.elapsed().as_secs_f64(), l.len() as f64 / core::SR as f64
                );
                keep(d.clone());
                if pack::installed().is_none() {
                    crate::aelog!("[SPATIAL] no pack at {} — the objects stay rough (from the mix)", pack::why_not());
                }
                audio = Some((path.clone(), Arc::new(l), Arc::new(r)));
                busy = true;
                d
            }
        };
        if data.lock().unwrap().error.is_some() {
            continue;
        }

        // 2. the track's map: kept on disk by its sound's content, else made
        // by the job — the whole track, once, with the pack's networks
        let Some(dir) = pack::installed() else { continue };
        let maps = map::cache_dir();
        if !data.lock().unwrap().job_done {
            if audio.as_ref().map_or(true, |a| a.0 != path) {
                match decode::decode_44k(std::path::Path::new(&path), &stop) {
                    Ok((l, r)) => audio = Some((path.clone(), Arc::new(l), Arc::new(r))),
                    Err(e) => {
                        crate::aelog!("[SPATIAL] cannot decode {path}: {e}");
                        continue;
                    }
                }
            }
            let (_, l, r) = audio.as_ref().unwrap();
            let got = map_of(&dir, maps.as_deref(), &path, l, r, &data, &stop);
            let mut d = data.lock().unwrap();
            match got {
                Ok(m) => {
                    d.map = Some(Arc::new(m));
                    d.job_done = true;
                }
                Err(e) if e == "stopped" => {}
                Err(e) => {
                    crate::aelog!("[SPATIAL] {}: no map ({e}) — its objects stay rough", short(&path));
                    d.job_done = true;
                }
            }
            continue;
        }
        audio = None;
        // 3. the next track of the queue, made ahead into the cache (and kept)
        let Some(next) = next_track(&path) else { continue };
        if ahead.as_deref() == Some(next.as_str()) || find(&next).is_some_and(|t| t.lock().unwrap().job_done) {
            continue;
        }
        ahead = Some(next.clone());
        match decode::decode_44k(std::path::Path::new(&next), &stop) {
            Ok((l, r)) => {
                let t = Arc::new(Mutex::new(rough_layer(&next, &l, &r)));
                match map_of(&dir, maps.as_deref(), &next, &l, &r, &t, &stop) {
                    Ok(m) => {
                        let mut d = t.lock().unwrap();
                        d.map = Some(Arc::new(m));
                        d.job_done = true;
                        drop(d);
                        keep(t);
                        crate::aelog!("[SPATIAL] {}: its map made ahead", short(&next));
                    }
                    Err(e) if e == "stopped" => ahead = None,
                    Err(e) => crate::aelog!("[SPATIAL] {}: no map ahead ({e})", short(&next)),
                }
            }
            Err(e) => crate::aelog!("[SPATIAL] cannot decode {next}: {e}"),
        }
    }
}

/// The track after `heard` in the play queue.
fn next_track(heard: &str) -> Option<String> {
    let q = crate::player::controller::get().queue();
    let i = q.iter().position(|t| t.path == heard)?;
    q.get(i + 1).map(|t| t.path.clone())
}

/// A track's map: its content key (the path memo's, else its sound's), then
/// the cache, else the job (kept then); the job's progress to `data`.
fn map_of(
    dir: &std::path::Path, maps: Option<&std::path::Path>, path: &str, l: &[f32], r: &[f32], data: &Arc<Mutex<TrackData>>,
    stop: &dyn Fn() -> bool,
) -> Result<map::TrackMap, String> {
    let content = maps.and_then(|m| map::remembered_in(m, path)).unwrap_or_else(|| {
        let k = map::content_key(l, r);
        if let Some(m) = maps {
            map::remember_in(m, path, k);
        }
        k
    });
    if let Some(m) = maps.and_then(|d| map::load_in(d, content, pack::PIN.version)) {
        crate::aelog!("[SPATIAL] {}: its map from the cache ({content:016x})", short(path));
        return Ok(m);
    }
    let (band, ref_db) = {
        let d = data.lock().unwrap();
        (d.rough_band.clone(), d.ref_db)
    };
    let inp = job::Input { l, r, content, rough_band: &band, ref_db };
    let m = job::analyse(dir, &inp, &mut |p| data.lock().unwrap().progress = p, stop)?;
    if let Some(d) = maps {
        if let Err(e) = map::save_in(d, &m, pack::PIN.version) {
            crate::aelog!("[SPATIAL] the map not kept: {e}");
        }
    }
    Ok(m)
}

/// A map's id on the wire (`span`'s thirteenth header field; 0 = none).
fn map_id(m: &map::TrackMap) -> u32 {
    (m.content as u32).max(1)
}

/// The map of id `id` for the scenes (`spatial_map?id=`, contract §4): its
/// objects and notes, once per map; None when no kept track has it.
pub fn map_json(id: u32) -> Option<String> {
    if let Some(j) = live::map_json(id) {
        return Some(j);
    }
    let q = shared().tracks.lock().unwrap();
    for t in q.iter() {
        let d = t.lock().unwrap();
        let Some(m) = d.map.as_ref().filter(|m| map_id(m) == id) else { continue };
        let objects: Vec<serde_json::Value> = m
            .objects
            .iter()
            .map(|o| serde_json::json!({"kind": o.kind, "name": o.name, "stem": o.stem, "x": o.x, "width": o.width, "colour": o.colour}))
            .collect();
        let notes: Vec<serde_json::Value> = m.notes.iter().map(|n| serde_json::json!([n.obj, n.key, n.on, n.off, n.vel])).collect();
        return Some(
            serde_json::json!({"count": m.objects.len(), "objects": objects, "notes": notes, "kit": if m.kit_bands { "bands" } else { "drumsep" }})
                .to_string(),
        );
    }
    None
}

/// Frames f0 … f1−1 of a track's map in the objects' layout (contract §3):
/// presence, energy, kind, main, x, y, z, width, onset (its last hit, fading
/// in 0.12 s), coherence, age (since it last came in), notes sounding (÷16),
/// origin = its place.
fn map_frames(m: &map::TrackMap, f0: usize, f1: usize) -> Vec<f32> {
    let n = f1.saturating_sub(f0);
    let mut out = vec![0f32; n * SLOTS * VALS];
    let u = |v: &[u8], j: usize| v.get(j).copied().unwrap_or(0) as f32 / 255.0;
    let (t0, t1) = (f0 as f64 / FPS, f1 as f64 / FPS);
    // the notes that sound in the window, per object
    let mut sounding: Vec<Vec<(f64, f64)>> = vec![Vec::new(); m.objects.len()];
    for nt in m.notes.iter().take_while(|nt| (nt.on as f64) < t1) {
        if (nt.off as f64) > t0 {
            if let Some(v) = sounding.get_mut(nt.obj as usize) {
                v.push((nt.on as f64, nt.off as f64));
            }
        }
    }
    for (s, o) in m.objects.iter().enumerate().take(SLOTS) {
        let mut came = (0..f0.min(o.presence.len())).rev().find(|&j| o.presence[j] == 0).map_or(0, |j| j + 1);
        let mut next = o.hits.partition_point(|t| (*t as f64) <= t0);
        for j in f0..f1 {
            let t = j as f64 / FPS;
            if o.presence.get(j).copied().unwrap_or(0) == 0 {
                came = j + 1;
            }
            while next < o.hits.len() && o.hits[next] as f64 <= t {
                next += 1;
            }
            let on = if next > 0 { (-(t - o.hits[next - 1] as f64) / 0.12).exp() as f32 } else { 0.0 };
            let age = if j >= came { (j - came) as f64 / FPS } else { 0.0 };
            let notes = sounding[s].iter().filter(|(a, b)| *a <= t && t < *b).count();
            let (x, y, z) = (o.x, u(&o.y, j), u(&o.z, j));
            let v = [
                u(&o.presence, j), u(&o.energy, j), o.kind as f32 / 32.0, 1.0, x, y, z, o.width, on, u(&o.coherence, j),
                (age / 60.0).min(1.0) as f32, (notes as f32 / 16.0).min(1.0), x, y, z, 0.0,
            ];
            let k = ((j - f0) * SLOTS + s) * VALS;
            out[k..k + VALS].copy_from_slice(&v);
        }
    }
    out
}

fn short(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The track's frames and its rough layer (from the mix), in parallel chunks;
/// the mix's parts followed over the whole track.
fn rough_layer(path: &str, l: &[f32], r: &[f32]) -> TrackData {
    use rayon::prelude::*;
    let len = l.len();
    let frames = len / HOP + 1;
    const CH: usize = 2048;
    let chunks: Vec<(usize, analysis::Region)> = (0..(frames + CH - 1) / CH)
        .into_par_iter()
        .map(|i| {
            let st = analysis::Stft::new();
            let a = i * CH;
            (a, analysis::region(&st, &[(l, r)], false, 0, a..(a + CH).min(frames)))
        })
        .collect();
    let mut rough_band = vec![0f32; frames * BANDS * RAW];
    let mut rough_h = Hist::new(frames, 1);
    for (a, reg) in &chunks {
        let o = a * BANDS * RAW;
        rough_band[o..o + reg.band.len()].copy_from_slice(&reg.band);
        rough_h.put(*a, reg);
    }
    // the mix's loud level: the 99th percentile of its frames' energy
    let st = analysis::Stft::new();
    let mut bl = vec![rustfft::num_complex::Complex32::new(0.0, 0.0); analysis::NFFT];
    let mut e: Vec<f32> = (0..frames).step_by(4).map(|j| analysis::mix_energy(&st, l, r, (j * HOP) as isize, &mut bl)).collect();
    e.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p99 = e.get(e.len() * 99 / 100).copied().unwrap_or(1.0).max(1e-9);
    let ref_db = 10.0 * p99.log10();
    let segs = (len + STRIDE - 1) / STRIDE;
    let mut band_p90_rough = [-60.0; BANDS];
    for (b, _, _) in BAND_SLOTS {
        band_p90_rough[b] = p90((0..frames).map(|j| rough_band[(j * BANDS + b) * RAW]), ref_db);
    }
    let mut d = TrackData {
        path: path.to_string(),
        len,
        frames,
        ref_db,
        rough_band,
        full_band: vec![0f32; frames * BANDS * RAW],
        rough_h,
        full_h: Hist::new(frames, FULL_PARTS.len()),
        seg_done: vec![false; segs.max(1)],
        cells: vec![Cell::default(); frames * SLOTS],
        full_infos: HashMap::new(),
        rough_infos: HashMap::new(),
        band_p90_full: [-60.0; BANDS],
        band_p90_rough,
        full_wide: vec![0f32; frames * WIDE_N * WRAW],
        wide_p90: [-60.0; WIDE_N],
        gpu: None,
        error: None,
        lineup: lineup::open(path),
        map: None,
        progress: 0.0,
        job_done: false,
    };
    let mix_p90 = p90((0..frames).map(|j| d.rough_h.e(j, 0).iter().sum::<f32>()), ref_db);
    let mut next = 0u32;
    let h = &d.rough_h;
    parts::track(&ROUGH_PARTS[0], |j| h.ec(j, 0), 0..frames, ref_db, mix_p90, SLOTS, &mut next, &mut d.cells, &mut d.rough_infos);
    d
}

/// Segment `k` through the network, and its region's raw values (the old
/// full layer — only the dump tool makes it now).
#[cfg(test)]
fn separate_segment(c: &mut core::Core, l: &[f32], r: &[f32], k: usize) -> Result<(std::ops::Range<usize>, analysis::Region), String> {
    let len = l.len();
    let start = (k * STRIDE) as isize - OV2 as isize;
    let cut = |x: &[f32]| -> Vec<f32> {
        (0..SEG as isize)
            .map(|i| {
                let j = start + i;
                if j >= 0 && (j as usize) < len { x[j as usize] } else { 0.0 }
            })
            .collect()
    };
    let (sl, sr) = (cut(l), cut(r));
    let stems = c.separate(&sl, &sr)?;
    let a = k * STRIDE;
    let b = ((k + 1) * STRIDE).min(len);
    let frames = (a + HOP - 1) / HOP..((b + HOP - 1) / HOP).min(len / HOP + 1);
    let srcs: Vec<(&[f32], &[f32])> = (0..SOURCES)
        .map(|s| (&stems[(s * 2) * SEG..(s * 2 + 1) * SEG], &stems[(s * 2 + 1) * SEG..(s * 2 + 2) * SEG]))
        .collect();
    let st = analysis::Stft::new();
    let reg = analysis::region(&st, &srcs, true, start, frames.clone());
    Ok((frames, reg))
}

/// The objects around what is heard, for the scenes: from `from_ms` to
/// `to_ms` of the heard moment (in the track's own time).
///
/// Answer (little endian): fifteen f64 — the heard second of the track,
/// frames a second, the first frame's index, the number of frames, the
/// track's frames, the separation's progress (0…1; −1: no pack), on the GPU
/// (1), the CPU (0) or not yet (−1), where the heard frame's objects come from
/// (0 nothing yet, 1 the mix alone, 2 the separated sources), the player's
/// state (0 playing, 1 paused, 2 held), the track id, slots (32), values per
/// slot (16), the map's id (0: none; `map_json`), whether the instruments are
/// on their way (1: the work that brings them is under way or about to start;
/// 0: it has made them, failed, or there is no pack), and the frames' make
/// (a live stream's: it moves when frames given out before were made again
/// — the window is asked for again; a file's 0) — then per frame per
/// slot the values as u16 (0…65535 for 0…1;
/// the ones marked ± for −1…1): presence, energy, kind (÷32), main, x ±, y,
/// z, width, onset, coherence, age (÷60 s), 0, origin x ±, origin y, origin
/// z, 0. Empty while stopped. A live stream: `live::span`.
pub fn span(from_ms: f64, to_ms: f64) -> Vec<u8> {
    let Some((track_id, path, at_s, state)) = crate::player::controller::get().heard() else {
        return Vec::new();
    };
    if let Some(gen) = live::stream_of(&path) {
        return live::span(gen, track_id, at_s, state, from_ms, to_ms);
    }
    want(&path, at_s);
    let pack = pack::installed().is_some();
    let mut head = [at_s, FPS, 0.0, 0.0, 0.0, -1.0, -1.0, 0.0, state as f64, track_id as f64, SLOTS as f64, VALS as f64, 0.0, 0.0, 0.0];
    if pack {
        // not decoded yet: asked for just now, the work is about to start
        head[5] = 0.0;
        head[13] = 1.0;
    }
    let mut out = Vec::new();
    let put_head = |head: &[f64; 15], out: &mut Vec<u8>| head.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
    let Some(data) = find(&path) else {
        put_head(&head, &mut out);
        return out;
    };
    let mut d = data.lock().unwrap();
    if let Some(l) = d.lineup.as_mut() {
        l.poll();
    }
    if pack {
        head[5] = if d.map.is_some() { 1.0 } else { d.progress as f64 };
        head[13] = if instruments_coming(&d) { 1.0 } else { 0.0 };
    }
    head[12] = d.map.as_ref().map_or(0.0, |m| map_id(m) as f64);
    head[6] = d.gpu.map_or(-1.0, |g| g as u8 as f64);
    head[4] = d.frames as f64;
    if d.frames == 0 {
        put_head(&head, &mut out);
        return out;
    }
    head[7] = match &d.lineup {
        // the lineup test: its inventory, or nothing until it comes
        Some(l) => if l.ready() { 2.0 } else { 0.0 },
        None => if d.map.is_some() { 2.0 } else { 1.0 },
    };
    let f0 = ((at_s + from_ms.min(to_ms) / 1000.0) * FPS).floor().max(0.0) as usize;
    let f1 = (((at_s + from_ms.max(to_ms) / 1000.0) * FPS).ceil().max(0.0) as usize).min(d.frames);
    if f1 <= f0 {
        put_head(&head, &mut out);
        return out;
    }
    if d.lineup.as_ref().is_some_and(|l| !l.ready()) {
        // the lineup test waiting for its inventory: no frames at all, so the
        // page keeps none empty and takes the row the moment it comes
        put_head(&head, &mut out);
        return out;
    }
    let vals = objects(&d, f0, f1);
    drop(d);
    head[2] = f0 as f64;
    head[3] = (f1 - f0) as f64;
    put_head(&head, &mut out);
    out.reserve(vals.len() * 2);
    for (i, v) in vals.iter().enumerate() {
        let k = i % VALS;
        let q = if k == 4 || k == 12 { (v + 1.0) * 0.5 } else { *v };
        out.extend_from_slice(&((q.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes());
    }
    out
}

/// The track's instruments are on their way (with the pack): its map not made
/// yet and nothing that stopped the work for good — a decode that failed, a
/// job that ended without a map (a job stopped for another track goes on
/// when this one is asked for again). The lineup test: until its inventory.
fn instruments_coming(d: &TrackData) -> bool {
    match &d.lineup {
        Some(l) => !l.ready(),
        None => d.map.is_none() && !d.job_done && d.error.is_none(),
    }
}

/// Frames f0 … f1−1, every slot: `[frame][SLOTS][VALS]` (unencoded; kind ÷32
/// and age ÷60 already), finished over a margin each side (the smoothing runs
/// both ways).
fn objects(d: &TrackData, f0: usize, f1: usize) -> Vec<f32> {
    if let Some(l) = &d.lineup {
        return l.objects(f0, f1);
    }
    if let Some(m) = &d.map {
        return map_frames(m, f0, f1);
    }
    let m = (FPS * 1.5) as usize;
    let (w0, w1) = (f0.saturating_sub(m), (f1 + m).min(d.frames));
    let n = w1 - w0;
    let mut out = vec![0f32; (f1 - f0) * SLOTS * VALS];
    let put = |j: usize, s: usize, v: &[f32; VALS], out: &mut Vec<f32>| {
        if j >= f0 && j < f1 {
            let o = ((j - f0) * SLOTS + s) * VALS;
            out[o..o + VALS].copy_from_slice(v);
        }
    };
    let done: Vec<bool> = (w0..w1).map(|j| d.frame_done(j)).collect();

    // the bands
    for (b, slot, kind) in BAND_SLOTS {
        let mut raw = vec![0f32; n * RAW];
        let mut p90s = vec![0f32; n];
        for i in 0..n {
            let j = w0 + i;
            // the ambience always from the mix; the others from the layer the frame has
            let full = done[i] && b != analysis::B_AMB;
            let src = if full { &d.full_band } else { &d.rough_band };
            let o = (j * BANDS + b) * RAW;
            raw[i * RAW..(i + 1) * RAW].copy_from_slice(&src[o..o + RAW]);
            p90s[i] = if full { d.band_p90_full[b] } else { d.band_p90_rough[b] };
        }
        let fin = analysis::finish_band(&raw, n, d.ref_db, &p90s, b == analysis::B_AMB);
        for (i, f) in fin.iter().enumerate() {
            let [x, y, z, en, w, on, pr, co] = *f;
            put(w0 + i, slot, &[pr, en * pr, kind as f32 / 32.0, 1.0, x, y, z, w, on, co, 0.0, 0.0, x, y, z, 0.0], &mut out);
        }
    }

    // the separated sources' wide layers, where the separation has been: a
    // source's diffuse part as one wide, far object of its kind, shown only
    // when it is more than its focused part's reverb (WIDE_GATE_DB)
    if done.iter().any(|x| *x) {
        let one = vec![1f32; n];
        for (w, &p) in WIDE.iter().enumerate() {
            let mut raw = vec![0f32; n * RAW];
            let (mut we, mut fe) = (vec![0f32; n], vec![0f32; n]);
            for i in 0..n {
                if !done[i] {
                    continue;
                }
                let o = ((w0 + i) * WIDE_N + w) * WRAW;
                raw[i * RAW..(i + 1) * RAW].copy_from_slice(&d.full_wide[o..o + RAW]);
                we[i] = d.full_wide[o];
                fe[i] = d.full_wide[o + RAW];
            }
            // a pad or a wash is slow: the two levels over about a second
            analysis::smooth(&mut we, 0.8, &one);
            analysis::smooth(&mut fe, 0.8, &one);
            let mut gate: Vec<f32> = (0..n)
                .map(|i| ((10.0 * ((we[i] + EPS) / (fe[i] + EPS)).log10() + WIDE_GATE_DB) / 4.0).clamp(0.0, 1.0))
                .collect();
            analysis::smooth(&mut gate, 0.3, &one);
            let p90s = vec![d.wide_p90[w]; n];
            let fin = analysis::finish_band(&raw, n, d.ref_db, &p90s, false);
            let kind = FULL_PARTS[p].kind as f32 / 32.0;
            for (i, f) in fin.iter().enumerate() {
                if !done[i] {
                    continue;
                }
                let [x, y, z, en, wd, on, pr, co] = *f;
                let g = gate[i];
                let (z, wd) = (z.max(0.7), wd.max(0.8));
                put(w0 + i, WIDE_SLOT0 + w, &[pr * g, en * g, kind, 1.0, x, y, z, wd, on * g, co, 1.0, 0.0, x, y, z, 0.0], &mut out);
            }
        }
    }

    // the parts
    let hist_of = |s: usize| -> Option<(&Hist, usize, u8, bool)> {
        if s >= ROUGH_SLOT0 {
            return Some((&d.rough_h, 0, K_MIX, false));
        }
        FULL_PARTS
            .iter()
            .enumerate()
            .find(|(_, p)| s >= p.slot0 && s <= p.slot0 + p.extras)
            .map(|(i, p)| (&d.full_h, i, p.kind, true))
    };
    for s in 0..SLOTS {
        let Some((h, p, kind, full)) = hist_of(s) else { continue };
        let cells: Vec<Cell> = (0..n)
            .map(|i| if done[i] == full { d.cells[(w0 + i) * SLOTS + s] } else { Cell::default() })
            .collect();
        if cells.iter().all(|c| c.id == 0) {
            continue;
        }
        let infos = if full { &d.full_infos } else { &d.rough_infos };
        let fin = parts::finish(
            &cells,
            w0,
            |j, c| {
                let (e, y, co) = h.all(j, p);
                parts::measure(c, &e, &y, &co)
            },
            infos,
            |info| {
                let (j, os) = (info.origin_frame as usize, info.origin_slot as usize);
                let c = d.cells.get(j * SLOTS + os).copied().unwrap_or_default();
                if c.id == 0 {
                    return [info.origin_x, 0.5, 0.5];
                }
                let (e, y, co) = h.all(j, p);
                parts::place(&c, &e, &y, &co, d.ref_db)
            },
            d.ref_db,
        );
        for (i, f) in fin.iter().enumerate() {
            if f[0] <= 0.0 && cells[i].id == 0 {
                continue;
            }
            let [pr, en, main, x, y, z, w, on, co, age, ox, oy, oz] = *f;
            put(w0 + i, s, &[pr, en, kind as f32 / 32.0, main, x, y, z, w, on, co, (age / 60.0).min(1.0), 0.0, ox, oy, oz, 0.0], &mut out);
        }
    }
    out
}

/// For the page: is the pack there, how far the heard track's analysis is.
#[tauri::command]
pub fn spatial_track_status() -> serde_json::Value {
    let heard = crate::player::controller::get().heard();
    if let Some(gen) = heard.as_ref().and_then(|h| live::stream_of(&h.1)) {
        return live::status(gen);
    }
    let d = heard.as_ref().and_then(|h| find(&h.1));
    let (seconds, frames, done, segs, gpu, error) = match &d {
        Some(d) => {
            let d = d.lock().unwrap();
            (d.len as f64 / core::SR as f64, d.frames, d.done(), d.segs(), d.gpu, d.error.clone())
        }
        None => (0.0, 0, 0, 0, None, None),
    };
    serde_json::json!({
        "pack": pack::installed().is_some(),
        "track": heard.map(|h| h.1),
        "seconds": seconds, "frames": frames, "done": done, "segments": segs, "gpu": gpu, "error": error,
        "kinds": KIND_NAMES, "sources": core::NAMES,
    })
}

#[cfg(test)]
mod note_tests {
    use super::*;

    /// The span says the instruments are on their way until the map is made
    /// — not once the work has ended without one, nor for a track that could
    /// not be decoded (the scenes' note would turn for ever).
    #[test]
    fn the_instruments_are_on_their_way_until_the_map_or_the_end_of_the_work() {
        let mut d = TrackData::failed("x.flac", "cannot decode".into());
        assert!(!instruments_coming(&d), "a track that could not be decoded");
        d.error = None;
        d.job_done = false;
        assert!(instruments_coming(&d), "the work under way");
        d.job_done = true;
        assert!(!instruments_coming(&d), "the work ended without a map");
        d.job_done = false;
        d.map = Some(Arc::new(map::assemble(1, 0, Vec::new(), Vec::new(), true)));
        assert!(!instruments_coming(&d), "the map made");
    }
}

#[cfg(test)]
mod dump_tests {
    use super::*;

    /// A whole track's objects as the scenes get them, for looking at by hand:
    /// the rough layer, and with the pack (`AURA_SPATIAL_PACK_DIR`) the
    /// separated one.
    ///   AURA_SPATIAL_FILE=<track> AURA_SPATIAL_OUT=<prefix> [AURA_SPATIAL_PACK_DIR=<pack>]
    ///   cargo test --profile fast objects_dump -- --ignored --nocapture
    /// Writes `<prefix>-rough.f32` and `<prefix>-full.f32`: frames × SLOTS × VALS
    /// in the span's order.
    #[test]
    #[ignore]
    fn objects_dump() {
        let path = std::env::var("AURA_SPATIAL_FILE").expect("AURA_SPATIAL_FILE");
        let out = std::env::var("AURA_SPATIAL_OUT").expect("AURA_SPATIAL_OUT");
        let (l, r) = decode::decode_44k(std::path::Path::new(&path), &|| false).expect("decode");
        let mut d = rough_layer(&path, &l, &r);
        let write = |d: &TrackData, name: &str| {
            let v = objects(d, 0, d.frames);
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            std::fs::write(format!("{out}-{name}.f32"), bytes).unwrap();
        };
        write(&d, "rough");
        eprintln!("[DUMP] fps {FPS} frames {} slots {SLOTS} vals {VALS}", d.frames);
        if let Some(dir) = std::env::var_os("AURA_SPATIAL_PACK_DIR") {
            let mut c = core::Core::open(std::path::Path::new(&dir)).expect("the network");
            for k in 0..d.segs() {
                let (frames, reg) = separate_segment(&mut c, &l, &r, k).expect("a segment");
                let o = frames.start * BANDS * RAW;
                d.full_band[o..o + reg.band.len()].copy_from_slice(&reg.band);
                d.full_h.put(frames.start, &reg);
                let ow = frames.start * WIDE_N * WRAW;
                d.full_wide[ow..ow + reg.wide.len()].copy_from_slice(&reg.wide);
                d.seg_done[k] = true;
            }
            d.retrack_full();
            write(&d, "full");
        }
    }
}
