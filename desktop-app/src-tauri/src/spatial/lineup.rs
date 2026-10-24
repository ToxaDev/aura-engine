//! The lineup test (Anton 28.09): a track's instrument inventory as its
//! objects. Every instrument of the track is found once, for the whole
//! track — the drums split into kit pieces (kick, snare, toms, hi-hat, ride,
//! crash), the other sources by the places they keep — and slot i is
//! instrument i from the first second to the last: always there, standing
//! still at its own place in a row left to right (all at one height, in the
//! inventory's order — whatever the scene, they stand in a line), its
//! energy its own level (0…1 over its own 30 dB), its onset a flash at each
//! of its hits (the scene "Lineup" draws exactly that).
//!
//! For now the inventory is made outside the app, by the research scripts
//! (outside the repository, `spatial\inv\lineup_maker.py`: HTDemucs stems, a
//! drum network for the kit, the stems' places), and only in a test run: with
//! `AURA_SPATIAL_INV_DIR` set, a track's inventory is `<dir>\<key>.json` (the
//! file name and a mark of the whole path, `key`); when it is not there yet
//! the app asks for it with `<key>.want` (the track's full path inside) and looks for
//! it once a second — the maker answers in about half a minute. Until then the
//! track has no objects. Without the variable nothing changes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;

use super::{FPS, SLOTS, VALS};

pub struct Lineup {
    /// the inventory's file
    json: PathBuf,
    inv: Option<Inventory>,
    next_look: Instant,
}

struct Inventory {
    /// frames a second of the levels
    fps: f64,
    objs: Vec<Obj>,
}

struct Obj {
    kind: u8,
    /// its place in the row, left to right
    x: f32,
    /// its level per frame, 0…255 for 0…1
    level: Vec<u8>,
    /// its hits (s), in order
    onsets: Vec<f32>,
}

/// An inventory name ("kick", "guitar L", …) → its kind.
fn kind_of(name: &str) -> u8 {
    match name.split_whitespace().next().unwrap_or("") {
        "kick" => super::K_KICK,
        "snare" => super::K_SNARE,
        "toms" => super::K_TOMS,
        "hh" | "hats" => super::K_HATS,
        "ride" | "crash" => super::K_CYMBAL,
        "bass" => super::K_BASS,
        "voice" => super::K_VOICE,
        "guitar" => super::K_GUITAR,
        "piano" => super::K_PIANO,
        _ => super::K_OTHER,
    }
}

/// Place i of n in the row: evenly from left to right, a margin at the ends.
fn row_x(i: usize, n: usize) -> f32 {
    -0.9 + 1.8 * (i as f32 + 0.5) / n.max(1) as f32
}

/// The lineup of a track in a test run (`AURA_SPATIAL_INV_DIR` set), else None.
pub fn open(path: &str) -> Option<Lineup> {
    let dir = std::env::var_os("AURA_SPATIAL_INV_DIR")?;
    Some(open_in(Path::new(&dir), path))
}

/// A track's name among the inventories: its file name and a mark of its
/// whole path (FNV-1a 32 of the path, lower case, back slashes), so two
/// files of one name in two albums are two tracks. The maker writes the
/// same name (lineup_export.py `key`).
pub fn key(path: &str) -> String {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let norm = path.replace('/', "\\").to_lowercase();
    let h = norm.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    format!("{name}~{h:08x}")
}

fn open_in(dir: &Path, path: &str) -> Lineup {
    let name = key(path);
    let json = dir.join(format!("{name}.json"));
    let mut l = Lineup { json, inv: None, next_look: Instant::now() };
    l.look();
    if l.inv.is_none() {
        let want = dir.join(format!("{name}.want"));
        match std::fs::write(&want, path) {
            Ok(()) => crate::aelog!("[SPATIAL] the lineup test: asked for the inventory of {name} (the maker answers in about half a minute)"),
            Err(e) => crate::aelog!("[SPATIAL] the lineup test: could not ask at {}: {e}", want.display()),
        }
    }
    l
}

impl Lineup {
    /// Is the inventory there (else the track shows nothing yet)?
    pub fn ready(&self) -> bool {
        self.inv.is_some()
    }

    /// Still waiting: look for the inventory, at most once a second.
    pub fn poll(&mut self) {
        if self.inv.is_none() && Instant::now() >= self.next_look {
            self.look();
        }
    }

    fn look(&mut self) {
        self.next_look = Instant::now() + Duration::from_secs(1);
        let Ok(text) = std::fs::read_to_string(&self.json) else { return };
        self.inv = parse(&text);
        match &self.inv {
            Some(i) => crate::aelog!("[SPATIAL] the lineup test: {} instruments from {}", i.objs.len(), self.json.display()),
            None => crate::aelog!("[SPATIAL] the lineup test: {} is not an inventory", self.json.display()),
        }
    }

    /// Frames f0 … f1−1 in the objects' layout (`[frame][SLOTS][VALS]`, as
    /// mod.rs `objects`): slot i = instrument i; none while it is not there.
    pub fn objects(&self, f0: usize, f1: usize) -> Vec<f32> {
        let mut out = vec![0f32; f1.saturating_sub(f0) * SLOTS * VALS];
        let Some(inv) = &self.inv else { return out };
        for (s, o) in inv.objs.iter().enumerate() {
            let t0 = f0 as f64 / FPS;
            // the first hit after the first frame
            let mut next = o.onsets.partition_point(|t| (*t as f64) <= t0);
            for j in f0..f1 {
                let t = j as f64 / FPS;
                let q = t * inv.fps;
                let i = q.floor() as usize;
                let a = o.level.get(i).copied().unwrap_or(0) as f32 / 255.0;
                let b = o.level.get(i + 1).copied().unwrap_or(0) as f32 / 255.0;
                let en = a + (b - a) * (q - i as f64) as f32;
                while next < o.onsets.len() && o.onsets[next] as f64 <= t {
                    next += 1;
                }
                // the flash of its last hit (the app's onsets fall in about 0.12 s)
                let on = if next > 0 { (-(t - o.onsets[next - 1] as f64) / 0.12).exp() as f32 } else { 0.0 };
                let (x, y, z) = (o.x, 0.5, 0.3);
                let v = [1.0, en, o.kind as f32 / 32.0, 1.0, x, y, z, 0.0, on, 1.0, 1.0, 0.0, x, y, z, 0.0];
                let k = ((j - f0) * SLOTS + s) * VALS;
                out[k..k + VALS].copy_from_slice(&v);
            }
        }
        out
    }
}

fn parse(text: &str) -> Option<Inventory> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let fps = v["fps"].as_f64().filter(|f| *f > 0.0)?;
    let list = v["objects"].as_array()?;
    let n = list.len().min(SLOTS);
    let mut objs = Vec::new();
    for (i, o) in list.iter().take(SLOTS).enumerate() {
        let level = base64::engine::general_purpose::STANDARD.decode(o["level"].as_str()?).ok()?;
        let mut onsets: Vec<f32> = o["onsets"].as_array()?.iter().filter_map(|t| t.as_f64()).map(|t| t as f32).collect();
        onsets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        objs.push(Obj { kind: kind_of(o["name"].as_str()?), x: row_x(i, n), level, onsets });
    }
    Some(Inventory { fps, objs })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(f: &dyn Fn(usize) -> u8) -> String {
        base64::engine::general_purpose::STANDARD.encode((0..200).map(f).collect::<Vec<u8>>())
    }

    fn two() -> String {
        // two instruments at 43 frames a second: a kick that hits at 1 s, a guitar that sounds from 1 s
        serde_json::json!({
            "fps": 43.0,
            "objects": [
                {"name": "kick", "pan": 0.0, "level": lv(&|i| if (43..48).contains(&i) { 255 } else { 0 }), "onsets": [1.0]},
                {"name": "guitar", "pan": 0.02, "level": lv(&|i| if i >= 43 { 128 } else { 0 }), "onsets": []},
            ]
        })
        .to_string()
    }

    #[test]
    fn an_inventory_is_its_objects_in_a_row() {
        let l = Lineup { json: PathBuf::new(), inv: parse(&two()), next_look: Instant::now() };
        let f = |t: f64| (t * FPS).round() as usize;
        let o = l.objects(f(0.5), f(2.0));
        let at = |t: f64, s: usize, v: usize| o[((f(t) - f(0.5)) * SLOTS + s) * VALS + v];
        // always there, of its kind, still — and in a row though both sound in the middle
        assert_eq!(at(0.5, 0, 0), 1.0);
        assert_eq!(at(0.5, 0, 2), super::super::K_KICK as f32 / 32.0);
        assert_eq!(at(0.5, 1, 2), super::super::K_GUITAR as f32 / 32.0);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(near(at(0.5, 0, 4), -0.45) && at(1.9, 0, 4) == at(0.5, 0, 4));
        assert!(near(at(0.5, 1, 4), 0.45) && at(0.5, 1, 5) == 0.5);
        assert_eq!(at(0.5, 0, 5), at(0.5, 1, 5));
        // the kick flashes at its hit and fades; the guitar sounds from 1 s
        assert_eq!(at(0.9, 0, 8), 0.0);
        assert!(at(1.01, 0, 8) > 0.8, "{}", at(1.01, 0, 8));
        assert!(at(1.5, 0, 8) < 0.05);
        assert!(at(0.9, 1, 1) < 0.01 && (at(1.5, 1, 1) - 0.5).abs() < 0.01);
        // slots past the inventory stay empty
        assert_eq!(at(1.0, 2, 0), 0.0);
        assert!(parse("{}").is_none());
    }

    #[test]
    fn a_missing_inventory_is_asked_for_and_taken_when_it_comes() {
        let dir = std::env::temp_dir().join(format!("aura-lineup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let track = r"E:\MUSIC\Some Album\01 - A Song.flac";
        let mut l = open_in(&dir, track);
        // asked: the request names the track; nothing shows meanwhile
        let k = key(track);
        assert!(k.starts_with("01 - A Song.flac~") && k.len() == "01 - A Song.flac~".len() + 8, "{k}");
        assert_eq!(std::fs::read_to_string(dir.join(format!("{k}.want"))).unwrap(), track);
        assert!(!l.ready());
        assert!(l.objects(0, 10).iter().all(|v| *v == 0.0));
        // one file name in another album is another track; the same path written otherwise is the same one
        assert_ne!(key(r"E:\MUSIC\Other Album\01 - A Song.flac"), k);
        assert_eq!(key("e:/music/some album/01 - A Song.flac"), k);
        // as the maker makes it (lineup_export.py `key`), Cyrillic too
        assert_eq!(k, "01 - A Song.flac~60584ddc");
        assert_eq!(key(r"E:\MUSIC\Бумбокс - Вахтерам.mp3"), "Бумбокс - Вахтерам.mp3~f057821b");
        // the maker answers; the next look takes it
        std::fs::write(dir.join(format!("{k}.json")), two()).unwrap();
        l.next_look = Instant::now();
        l.poll();
        assert!(l.ready());
        assert_eq!(l.objects(0, 10)[0], 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
