//! The track map (Anton 28.09, the "with instruments" tier; the contract
//! `SPEC2-CONTRACT.md` §1, §2, §6): a track's instruments found once, for
//! the whole track — the kit piece by piece, the sources' places, the note
//! instruments, the room — each one fixed object with its place, width and
//! colour, and per frame how it sounds; every instrument's notes. The
//! track's job (job.rs) makes it in the background; it is kept on disk by
//! the audio's content, not by its path, and the scenes get it through
//! `span` and `spatial_map`.
//!
//! Slot order: kick, snare, toms, hats, ride, crash, bass…, voice…,
//! guitar…, piano…, other…, the ambience last (within a family left to
//! right); at most 32 — past that the quietest non-kit objects go.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::analysis::{depth, height, smooth};
use super::{K_AMBIENCE, K_BASS, K_CYMBAL, K_GUITAR, K_HATS, K_KICK, K_OTHER, K_PIANO, K_SNARE, K_TOMS, K_VOICE};

/// Bump: every cached map is made again.
pub const MAP_VERSION: u32 = 1;
/// The objects' frames: hop 512 at 44.1 kHz (frame j centred on sample 512·j).
pub const MAP_FPS: f64 = 44_100.0 / 512.0;
pub const MAX_OBJECTS: usize = 32;
/// The maps kept on disk at most; the least recently used go first.
const CACHE_CAP: u64 = 300 << 20;
/// The paths remembered (a file seen again finds its map without decoding).
const PATHS_CAP: usize = 20_000;
const MAGIC: &[u8; 8] = b"AURAMAP\0";

/// A track's instruments over the whole track.
#[derive(Clone, Default, Debug)]
pub struct TrackMap {
    /// the audio's content key (`content_key`)
    pub content: u64,
    /// frames at `MAP_FPS`
    pub frames: usize,
    /// ≤ 32, in slot order
    pub objects: Vec<MapObj>,
    /// every note of every object, by onset
    pub notes: Vec<Note>,
    /// the kit is the drums' bands (kick, snare, hats) — the drum network
    /// runs on the GPU only ("kit: bands", for the log and the checks)
    pub kit_bands: bool,
}

/// One instrument of the track.
#[derive(Clone, Default, Debug)]
pub struct MapObj {
    /// its kind (mod.rs K_…)
    pub kind: u8,
    /// its source: 0 drums 1 bass 2 other 3 vocals 4 guitar 5 piano, 255 the mix
    pub stem: u8,
    /// "kick", "guitar L", … (logs, tests)
    pub name: String,
    /// its place, −1 left … +1 right, and width, 0 a point … 1 wide: fixed
    pub x: f32,
    pub width: f32,
    /// RGB 0…1, fixed
    pub colour: [f32; 3],
    /// per frame, 0…255 for 0…1: it sounds; how loud (45 dB against the
    /// track's loud level); its height (40 Hz … 12 kHz); its depth (near …
    /// far); how alike its channels are
    pub presence: Vec<u8>,
    pub energy: Vec<u8>,
    pub y: Vec<u8>,
    pub z: Vec<u8>,
    pub coherence: Vec<u8>,
    /// its hits (the kit) or strong notes' onsets, s
    pub hits: Vec<f32>,
}

/// A note of an object.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Note {
    /// its object's slot
    pub obj: u8,
    /// MIDI 21…108; the fraction is its bend
    pub key: f32,
    /// on and off, s
    pub on: f32,
    pub off: f32,
    /// 0…1 on the object's own scale
    pub vel: f32,
    pub ghost: bool,
}

/// What an object's per-frame values are made of: its level (dB against the
/// mix's loud level; very low where silent), its height (0…1) and its
/// coherence (0…1), per frame at `MAP_FPS`.
pub struct Raw {
    pub db: Vec<f32>,
    pub y: Vec<f32>,
    pub coh: Vec<f32>,
}

/// A frame sounds within this many dB of the object's loud frames (and over
/// −60 dB of the mix's).
const SOUNDS_DB: f32 = 30.0;
/// Presence: it rises in 0.06 s, falls in 0.3 s, and only after 1 s of silence.
const RISE_S: f32 = 0.06;
const FALL_S: f32 = 0.3;
const HOLD_S: f64 = 1.0;
/// The depth moves over a second at least.
const Z_TAU: f32 = 1.0;

fn q(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The per-frame values of an object from its raw ones (`frames` long):
/// presence, energy, y, z, coherence as u8.
pub fn finish(raw: &Raw, frames: usize) -> [Vec<u8>; 5] {
    let mut loud: Vec<f32> = (0..frames).map(|t| raw.db.get(t).copied().unwrap_or(-200.0)).filter(|d| *d > -90.0).collect();
    loud.sort_by(|a, b| a.total_cmp(b));
    let loud = loud.get(loud.len() * 99 / 100).copied().unwrap_or(-60.0);
    finish_with(raw, frames, loud)
}

/// `finish` against a loud level known beforehand (dB, the p99 of the
/// object's levels over −90): a live stream's object, whose song is not over,
/// is measured against what it has played so far.
pub fn finish_with(raw: &Raw, frames: usize, loud: f32) -> [Vec<u8>; 5] {
    let at = |v: &Vec<f32>, t: usize, d: f32| v.get(t).copied().unwrap_or(d);
    let db: Vec<f32> = (0..frames).map(|t| at(&raw.db, t, -200.0)).collect();
    let floor = (loud - SOUNDS_DB).max(-60.0);
    let dt = 1.0 / MAP_FPS as f32;
    // presence: held over a second of silence, then let go slowly
    let hold = (HOLD_S * MAP_FPS).round() as usize;
    let (up, down) = (1.0 - (-dt / RISE_S).exp(), 1.0 - (-dt / FALL_S).exp());
    let mut presence = vec![0f32; frames];
    let (mut p, mut last) = (0f32, None::<usize>);
    for t in 0..frames {
        if db[t] >= floor {
            last = Some(t);
        }
        let target = if last.is_some_and(|l| t - l <= hold) { 1.0 } else { 0.0 };
        p += (target - p) * if target > p { up } else { down };
        presence[t] = p;
    }
    // energy: 45 dB under the track's loud level … at it, released over 0.1 s
    let rel = (-dt / 0.1).exp();
    let mut energy = vec![0f32; frames];
    let mut en = 0f32;
    for t in 0..frames {
        en = (((db[t] + 45.0) / 45.0).clamp(0.0, 1.0) * presence[t]).max(en * rel);
        energy[t] = en;
    }
    let gate: Vec<f32> = presence.iter().map(|p| p.clamp(0.02, 1.0)).collect();
    let mut y: Vec<f32> = (0..frames).map(|t| at(&raw.y, t, 0.5)).collect();
    smooth(&mut y, 0.08, &gate);
    let mut coh: Vec<f32> = (0..frames).map(|t| at(&raw.coh, t, 0.5)).collect();
    smooth(&mut coh, 0.1, &gate);
    let mut z: Vec<f32> = (0..frames).map(|t| depth(coh[t], db[t].max(-60.0))).collect();
    smooth(&mut z, Z_TAU, &gate);
    let u = |v: &[f32]| v.iter().map(|x| q(*x)).collect::<Vec<u8>>();
    [u(&presence), u(&energy), u(&y), u(&z), u(&coh)]
}

/// The height of a ln f centre (ln Hz), as analysis.rs places sounds.
pub fn height_of(ln_f: f32) -> f32 {
    height(ln_f)
}

/// The family order of the slots.
fn family(kind: u8) -> usize {
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
    matches!(kind, K_KICK | K_SNARE | K_TOMS | K_HATS | K_CYMBAL)
}

/// The objects in slot order (the kit in the order given — kick, snare,
/// toms, hats, ride, crash —, the others by family and left to right), at
/// most 32 (the quietest non-kit objects go), and the notes on their new
/// slots (a note of an object that went goes too), by onset.
pub fn assemble(content: u64, frames: usize, objects: Vec<MapObj>, notes: Vec<Note>, kit_bands: bool) -> TrackMap {
    let loud = |o: &MapObj| {
        let mut e = o.energy.clone();
        e.sort_unstable();
        e.get(e.len() * 99 / 100).copied().unwrap_or(0)
    };
    let mut idx: Vec<usize> = (0..objects.len()).collect();
    if idx.len() > MAX_OBJECTS {
        let mut cut: Vec<usize> = idx.iter().cloned().filter(|&i| !is_kit(objects[i].kind)).collect();
        cut.sort_by_key(|&i| loud(&objects[i]));
        let drop: Vec<usize> = cut.into_iter().take(idx.len() - MAX_OBJECTS).collect();
        idx.retain(|i| !drop.contains(i));
    }
    // stable: the kit keeps its piece order
    idx.sort_by(|&a, &b| {
        let (oa, ob) = (&objects[a], &objects[b]);
        family(oa.kind).cmp(&family(ob.kind)).then(if is_kit(oa.kind) {
            std::cmp::Ordering::Equal
        } else {
            oa.x.total_cmp(&ob.x)
        })
    });
    let slot_of: Vec<Option<u8>> =
        (0..objects.len()).map(|i| idx.iter().position(|&j| j == i).map(|s| s as u8)).collect();
    let mut notes: Vec<Note> = notes
        .into_iter()
        .filter_map(|n| slot_of.get(n.obj as usize).copied().flatten().map(|s| Note { obj: s, ..n }))
        .collect();
    notes.sort_by(|a, b| a.on.total_cmp(&b.on));
    let mut objects: Vec<Option<MapObj>> = objects.into_iter().map(Some).collect();
    let mut slots: Vec<MapObj> = idx.iter().map(|&i| objects[i].take().unwrap()).collect();
    place_hits(&mut slots, &notes, &|_, o| o.hits.clone());
    TrackMap { content, frames, objects: slots, notes, kit_bands }
}

/// The bass's and the voice's places flash on their strong notes (Anton
/// 3.10, `notes::objects::note_hits`), each over the onsets of its sound,
/// `sound(slot, place)`: a file's own hits (as its analysis found them; a map
/// made so already comes out the same), a stream's kept apart. The one way a
/// file's analysis, a map loaded from disk and a stream make them.
pub fn place_hits(objects: &mut [MapObj], notes: &[Note], sound: &dyn Fn(usize, &MapObj) -> Vec<f32>) {
    for (i, o) in objects.iter_mut().enumerate() {
        if o.kind == K_BASS || o.kind == K_VOICE {
            let mine: Vec<(f32, f32, bool)> = notes.iter().filter(|n| n.obj as usize == i).map(|n| (n.on, n.off, n.ghost)).collect();
            o.hits = super::notes::objects::note_hits(&sound(i, o), &mine);
        }
    }
}

/// The audio's content key: 64 bits of SHA-256 over the decoded sound at
/// 44.1 kHz as 16-bit stereo, and its length — the same sound renamed,
/// moved, re-tagged or in another lossless container has the same key.
pub fn content_key(l: &[f32], r: &[f32]) -> u64 {
    let n = l.len().min(r.len());
    let mut h = Sha256::new();
    h.update(b"aura-sound-44k-s16\0");
    h.update((n as u64).to_le_bytes());
    let mut buf = Vec::with_capacity(1 << 16);
    for i in 0..n {
        for v in [l[i], r[i]] {
            buf.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
        if buf.len() >= 1 << 16 {
            h.update(&buf);
            buf.clear();
        }
    }
    h.update(&buf);
    let d = h.finalize();
    u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

// ---- the map as bytes -------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
}

struct R<'a>(&'a [u8], usize);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
}

impl TrackMap {
    /// The map as bytes, with the map's and the pack's versions.
    pub fn to_bytes(&self, pack: u32) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.u32(MAP_VERSION);
        w.u32(pack);
        w.u64(self.content);
        w.u32(self.frames as u32);
        w.u8(self.kit_bands as u8);
        w.u32(self.objects.len() as u32);
        for o in &self.objects {
            w.u8(o.kind);
            w.u8(o.stem);
            w.bytes(o.name.as_bytes());
            for v in [o.x, o.width, o.colour[0], o.colour[1], o.colour[2]] {
                w.f32(v);
            }
            for v in [&o.presence, &o.energy, &o.y, &o.z, &o.coherence] {
                w.bytes(v);
            }
            w.u32(o.hits.len() as u32);
            o.hits.iter().for_each(|h| w.f32(*h));
        }
        w.u32(self.notes.len() as u32);
        for n in &self.notes {
            w.u8(n.obj);
            for v in [n.key, n.on, n.off, n.vel] {
                w.f32(v);
            }
            w.u8(n.ghost as u8);
        }
        w.0
    }

    /// The map from its bytes; None when they are not a map of this version
    /// made with this pack (it is made again).
    pub fn from_bytes(b: &[u8], pack: u32) -> Option<TrackMap> {
        let mut r = R(b, 0);
        if r.take(8)? != MAGIC || r.u32()? != MAP_VERSION || r.u32()? != pack {
            return None;
        }
        let content = r.u64()?;
        let frames = r.u32()? as usize;
        let kit_bands = r.u8()? != 0;
        let n = r.u32()? as usize;
        let mut objects = Vec::with_capacity(n.min(MAX_OBJECTS));
        for _ in 0..n {
            let kind = r.u8()?;
            let stem = r.u8()?;
            let name = String::from_utf8(r.bytes()?).ok()?;
            let (x, width) = (r.f32()?, r.f32()?);
            let colour = [r.f32()?, r.f32()?, r.f32()?];
            let mut per = || -> Option<Vec<u8>> { r.bytes().filter(|v| v.len() == frames) };
            let (presence, energy, y, z, coherence) = (per()?, per()?, per()?, per()?, per()?);
            let nh = r.u32()? as usize;
            let hits = (0..nh).map(|_| r.f32()).collect::<Option<Vec<f32>>>()?;
            objects.push(MapObj { kind, stem, name, x, width, colour, presence, energy, y, z, coherence, hits });
        }
        let nn = r.u32()? as usize;
        let mut notes = Vec::with_capacity(nn);
        for _ in 0..nn {
            let obj = r.u8()?;
            let (key, on, off, vel) = (r.f32()?, r.f32()?, r.f32()?, r.f32()?);
            let ghost = r.u8()? != 0;
            notes.push(Note { obj, key, on, off, vel, ghost });
        }
        // a map kept from before the places flashed on their notes flashes on them now
        place_hits(&mut objects, &notes, &|_, o| o.hits.clone());
        Some(TrackMap { content, frames, objects, notes, kit_bands })
    }
}

// ---- the cache on disk ------------------------------------------------------

/// Where maps are kept: `<spatial root>\maps`.
pub fn cache_dir() -> Option<PathBuf> {
    Some(super::pack::root()?.join("maps"))
}

fn map_file(dir: &Path, content: u64) -> PathBuf {
    dir.join(format!("{content:016x}.map"))
}

/// The cached map of `content`, made with `pack`; its use is marked (the
/// least recently used go first).
pub fn load_in(dir: &Path, content: u64, pack: u32) -> Option<TrackMap> {
    let f = std::fs::File::open(map_file(dir, content)).ok()?;
    let mut z = zip::ZipArchive::new(f).ok()?;
    let mut e = z.by_index(0).ok()?;
    let mut b = Vec::new();
    e.read_to_end(&mut b).ok()?;
    let m = TrackMap::from_bytes(&b, pack).filter(|m| m.content == content)?;
    let _ = std::fs::File::options().append(true).open(map_file(dir, content)).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    Some(m)
}

/// Keep a map (compressed, whole or not at all), then trim the cache to its cap.
pub fn save_in(dir: &Path, map: &TrackMap, pack: u32) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let dst = map_file(dir, map.content);
    let tmp = dst.with_extension("part");
    {
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut z = zip::ZipWriter::new(f);
        let opt = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("map.bin", opt).map_err(|e| e.to_string())?;
        z.write_all(&map.to_bytes(pack)).map_err(|e| e.to_string())?;
        z.finish().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
    trim(dir, CACHE_CAP);
    Ok(())
}

/// The least recently used maps go until the rest fit in `cap` bytes.
fn trim(dir: &Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "map"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, p) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

/// A file's mark, for the path memo: its size and when it was written.
fn file_mark(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), t))
}

/// The content key a path had when it was last decoded, if the file has not
/// changed since (its size and time): a track played again finds its map
/// without being decoded. The key stays the truth — the memo only saves the
/// decoding.
pub fn remembered_in(dir: &Path, path: &str) -> Option<u64> {
    let (size, time) = file_mark(Path::new(path))?;
    let memo: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("paths.json")).ok()?).ok()?;
    let e = memo.get(path)?.as_array()?;
    (e.first()?.as_u64()? == size && e.get(1)?.as_u64()? == time).then_some(())?;
    u64::from_str_radix(e.get(2)?.as_str()?, 16).ok()
}

/// Remember a path's content key.
pub fn remember_in(dir: &Path, path: &str, content: u64) {
    let Some((size, time)) = file_mark(Path::new(path)) else { return };
    let file = dir.join("paths.json");
    let mut memo: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    memo.remove(path);
    while memo.len() >= PATHS_CAP {
        let Some(k) = memo.keys().next().cloned() else { break };
        memo.remove(&k);
    }
    memo.insert(path.to_string(), serde_json::json!([size, time, format!("{content:016x}")]));
    let _ = std::fs::create_dir_all(dir);
    let tmp = file.with_extension("part");
    if std::fs::write(&tmp, serde_json::Value::Object(memo).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: u8, name: &str, x: f32, frames: usize, level: u8) -> MapObj {
        MapObj {
            kind,
            stem: 0,
            name: name.into(),
            x,
            width: 0.2,
            colour: [0.5; 3],
            presence: vec![255; frames],
            energy: vec![level; frames],
            y: vec![128; frames],
            z: vec![64; frames],
            coherence: vec![200; frames],
            hits: vec![1.0, 2.5],
        }
    }

    #[test]
    fn presence_comes_quickly_stays_over_short_rests_and_goes_slowly() {
        let fps = MAP_FPS;
        let frames = (10.0 * fps) as usize;
        let f = |t: f64| (t * fps) as usize;
        // it sounds 1–3 s and 3.5–5 s (a half-second rest), then stops
        let db: Vec<f32> = (0..frames).map(|i| if (f(1.0)..f(3.0)).contains(&i) || (f(3.5)..f(5.0)).contains(&i) { -6.0 } else { -120.0 }).collect();
        let raw = Raw { db, y: vec![0.3; frames], coh: vec![0.9; frames] };
        let [p, e, y, z, c] = finish(&raw, frames);
        assert!(p[f(0.9)] == 0 && p[f(1.1)] > 180, "rise {} {}", p[f(0.9)], p[f(1.1)]);
        assert_eq!(p[f(3.25)], 255, "a half-second rest keeps it");
        // held a second, then 0.3 s: ~0.2 half a second later, gone in two
        assert!(p[f(5.9)] == 255 && p[f(6.5)] < 60 && p[f(8.0)] == 0, "falls a second after: {} {} {}", p[f(5.9)], p[f(6.5)], p[f(8.0)]);
        // energy: −6 dB of the loud level → 39/45
        assert!((e[f(2.0)] as f32 / 255.0 - 39.0 / 45.0).abs() < 0.02);
        assert!(e[f(9.0)] == 0 && (y[f(2.0)] as f32 / 255.0 - 0.3).abs() < 0.01 && c[f(2.0)] > 220);
        // nearer (dry, loud) while it sounds than silent
        assert!(z[f(2.0)] < 110 && z[f(2.0)] < z[f(9.0)], "z {} {}", z[f(2.0)], z[f(9.0)]);
    }

    #[test]
    fn objects_stand_in_slot_order_and_notes_follow_them() {
        let n = 50;
        let mut objs = vec![
            obj(K_GUITAR, "guitar R", 0.5, n, 100),
            obj(K_AMBIENCE, "ambience", 0.0, n, 30),
            obj(K_SNARE, "snare", 0.0, n, 200),
            obj(K_KICK, "kick", 0.0, n, 220),
            obj(K_GUITAR, "guitar L", -0.5, n, 90),
            obj(K_BASS, "bass", 0.0, n, 150),
        ];
        let notes = vec![Note { obj: 5, key: 40.0, on: 2.0, off: 2.5, vel: 0.5, ghost: false }, Note { obj: 0, key: 64.0, on: 1.0, off: 1.2, vel: 0.9, ghost: false }];
        let m = assemble(7, n, objs.clone(), notes, false);
        let names: Vec<&str> = m.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["kick", "snare", "bass", "guitar L", "guitar R", "ambience"]);
        // the notes on their objects' new slots, by onset
        assert_eq!((m.notes[0].obj, m.notes[0].key), (4, 64.0));
        assert_eq!((m.notes[1].obj, m.notes[1].key), (2, 40.0));
        // more than 32: the quietest non-kit objects go, never the kit
        objs.extend((0..40).map(|i| obj(K_OTHER, &format!("other {i}"), i as f32 / 40.0, n, 10 + i as u8)));
        let m = assemble(7, n, objs, vec![], false);
        assert_eq!(m.objects.len(), MAX_OBJECTS);
        let has = |n: &str| m.objects.iter().any(|o| o.name == n);
        assert!(has("kick") && has("snare") && has("ambience") && has("other 14"));
        assert!((0..14).all(|i| !has(&format!("other {i}"))));
        assert_eq!(m.objects.last().unwrap().name, "ambience");
    }

    /// A map kept from before the places flashed on their notes (their hits
    /// the onsets of their sound) comes off the disk as a fresh analysis of
    /// the same sound makes it — and a fresh one the same again.
    #[test]
    fn an_old_map_off_the_disk_is_a_fresh_one() {
        let n = 400;
        let mut bass = obj(K_BASS, "bass", 0.0, n, 150);
        bass.hits = vec![0.2, 0.9, 1.5, 3.03, 4.0];
        let mut voice = obj(K_VOICE, "voice", 0.1, n, 120);
        voice.hits = vec![1.0, 2.0];
        let guitar = obj(K_GUITAR, "guitar", -0.4, n, 140);
        let notes = vec![
            Note { obj: 0, key: 40.0, on: 1.0, off: 2.0, vel: 0.9, ghost: false },
            Note { obj: 0, key: 52.0, on: 1.0, off: 1.4, vel: 0.5, ghost: true },
            Note { obj: 0, key: 43.0, on: 3.0, off: 3.5, vel: 0.8, ghost: false },
            Note { obj: 2, key: 64.0, on: 0.5, off: 0.9, vel: 0.7, ghost: false },
        ];
        let fresh = assemble(9, n, vec![bass.clone(), voice, guitar], notes, false);
        let b = fresh.objects.iter().position(|o| o.kind == K_BASS).unwrap();
        assert_eq!(fresh.objects[b].hits, vec![0.2, 0.9, 1.0, 3.0, 4.0], "its notes, and its sound's onsets where none sounds");
        let mut old = fresh.clone();
        old.objects[b].hits = bass.hits.clone();
        for m in [old, fresh.clone()] {
            let back = TrackMap::from_bytes(&m.to_bytes(3), 3).unwrap();
            for (a, w) in back.objects.iter().zip(&fresh.objects) {
                assert_eq!((&a.name, &a.hits), (&w.name, &w.hits));
            }
            assert_eq!(back.notes, fresh.notes);
        }
    }

    #[test]
    fn a_map_goes_to_disk_and_back_by_its_content() {
        let n = 1000;
        let objs = vec![obj(K_KICK, "kick", 0.0, n, 200), obj(K_VOICE, "voice", -0.1, n, 180)];
        let notes = vec![Note { obj: 1, key: 60.25, on: 0.5, off: 1.0, vel: 0.7, ghost: true }];
        let m = assemble(0xfeed_beef_1234_5678, n, objs, notes, true);
        let b = m.to_bytes(2);
        let back = TrackMap::from_bytes(&b, 2).unwrap();
        assert_eq!(back.to_bytes(2), b);
        assert!(back.kit_bands && back.notes[0].ghost && back.objects[1].name == "voice");
        // another map version or pack: made again
        assert!(TrackMap::from_bytes(&b, 3).is_none());
        let dir = std::env::temp_dir().join(format!("aura-maps-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        save_in(&dir, &m, 2).unwrap();
        let got = load_in(&dir, m.content, 2).unwrap();
        assert_eq!(got.to_bytes(2), b);
        assert!(load_in(&dir, m.content, 3).is_none() && load_in(&dir, 1, 2).is_none());
        let size = std::fs::metadata(map_file(&dir, m.content)).unwrap().len();
        assert!(size < b.len() as u64 / 4, "compressed {size} of {}", b.len());
        // the cap: the least recently used goes
        let mut old = m.clone();
        old.content = 1;
        save_in(&dir, &old, 2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        load_in(&dir, m.content, 2).unwrap();
        trim(&dir, size + 10);
        assert!(load_in(&dir, 1, 2).is_none() && load_in(&dir, m.content, 2).is_some());
        // the path memo: a file seen again, unchanged, gives its key without decoding
        let track = dir.join("a track.flac");
        std::fs::write(&track, b"sound").unwrap();
        let p = track.to_string_lossy().to_string();
        assert_eq!(remembered_in(&dir, &p), None);
        remember_in(&dir, &p, 0xabc);
        assert_eq!(remembered_in(&dir, &p), Some(0xabc));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&track, b"another sound").unwrap();
        assert_eq!(remembered_in(&dir, &p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_content_key_is_the_sound_not_the_file() {
        let mut l: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        l[1000] = 1000.0 / 32767.0;
        let r: Vec<f32> = l.iter().map(|v| v * 0.8).collect();
        let k = content_key(&l, &r);
        assert_eq!(k, content_key(&l.clone(), &r.clone()));
        // a difference under half a 16-bit step is the same sound; one step is not
        let mut l2 = l.clone();
        l2[1000] += 0.2 / 32767.0;
        assert_eq!(content_key(&l2, &r), k);
        l2[1000] += 1.0 / 32767.0;
        assert_ne!(content_key(&l2, &r), k);
        assert_ne!(content_key(&l[..44_000], &r[..44_000]), k);
    }
}
