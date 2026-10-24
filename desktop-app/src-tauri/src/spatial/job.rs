//! A track's job (Anton 28.09, the "with instruments" tier; the contract
//! `SPEC2-CONTRACT.md` §2, §6): the whole track analysed once, in the
//! background, into its map (map.rs) —
//! - its six sources by HTDemucs over the whole track (as the research's
//!   `separate_track.py`: segments three quarters apart, faded by triangles);
//! - the kit: DrumSep on the drums source, on the GPU only (kit.rs); without
//!   it the drums' bands (kick, snare, hats), and the map says so;
//! - the notes and note instruments of guitar, piano and other, the bass's
//!   and the voice's notes («Ноты»: notes::objects);
//! - the places (place.rs) of the bass and the voice, and of every source
//!   that is there but got no note instruments; the coherence of a note
//!   instrument comes from its source's place around it;
//! - the room: the mix's diffuse layer.

use std::path::{Path, PathBuf};

use super::analysis::{self, BANDS, B_AMB, B_HATS, B_KICK, B_SNARE, RAW};
use super::map::{self, MapObj, Note, Raw, TrackMap, MAP_FPS};
use super::notes;
use super::stems::{self, Stems};
use super::{core, kit, place};
use super::{K_AMBIENCE, K_BASS, K_CYMBAL, K_GUITAR, K_HATS, K_KICK, K_OTHER, K_PIANO, K_SNARE, K_TOMS, K_VOICE};

/// The networks' files in the pack (pack v2); `AURA_KIT_MODEL` /
/// `AURA_NOTES_MODEL` name others (tests).
const DRUMS_FILE: &str = super::pack::DRUMS;
const NOTES_FILE: &str = super::pack::PITCH;
const EPS: f32 = 1e-12;

/// What the job starts from: the track decoded at 44.1 kHz, its content key,
/// and its rough layer's bands (`[frame][BANDS][RAW]`, the mix's loud level
/// `ref_db` on their scale).
pub struct Input<'a> {
    pub l: &'a [f32],
    pub r: &'a [f32],
    pub content: u64,
    pub rough_band: &'a [f32],
    pub ref_db: f32,
}

fn model(env: &str, dir: &Path, file: &str) -> PathBuf {
    std::env::var_os(env).map(PathBuf::from).unwrap_or_else(|| dir.join(file))
}

/// The track's map, with the pack in `dir`. `progress` hears 0…1; `stop`
/// true ends it ("stopped").
pub fn analyse(dir: &Path, inp: &Input, progress: &mut dyn FnMut(f32), stop: &dyn Fn() -> bool) -> Result<TrackMap, String> {
    let len = inp.l.len().min(inp.r.len());
    let frames = len / analysis::HOP + 1;
    let t0 = std::time::Instant::now();
    // 1. the six sources, whole (the separation's session closes before the kit's opens)
    let (src, gpu) = {
        let mut c = core::Core::open(dir)?;
        let s = separate_all(&mut c, &inp.l[..len], &inp.r[..len], &mut |p| progress(0.45 * p), stop)?;
        (s, c.gpu)
    };
    let t_sep = t0.elapsed().as_secs_f64();
    let st = Stems { src: std::array::from_fn(|s| [&src[s * 2][..], &src[s * 2 + 1][..]]), mix: [&inp.l[..len], &inp.r[..len]] };
    let mix_db = st.mix_db();
    let mut objs: Vec<MapObj> = Vec::new();
    let mut notes: Vec<Note> = Vec::new();

    // 2. the kit and 3. the notes, side by side (neither waits for the other)
    let t1 = std::time::Instant::now();
    let (pieces, (na, t_notes)) = std::thread::scope(|s| {
        let notes = s.spawn(|| {
            let t = std::time::Instant::now();
            (notes_of(dir, &st), t.elapsed().as_secs_f64())
        });
        let pieces = kit_pieces(dir, st.src[stems::DRUMS], mix_db, frames, &mut |p| progress(0.45 + 0.4 * p), stop);
        (pieces, notes.join().unwrap_or((None, 0.0)))
    });
    let pieces = pieces?;
    let kit_bands = pieces.is_none();
    objs.extend(pieces.unwrap_or_else(|| kit_bands_of(&st, frames, inp.ref_db)));
    let t_kit = t1.elapsed().as_secs_f64();
    if stop() {
        return Err("stopped".into());
    }
    progress(0.9);

    // 4. the places of every source that is there
    let present: Vec<usize> = [stems::BASS, stems::VOCALS, stems::GUITAR, stems::PIANO, stems::OTHER]
        .into_iter()
        .filter(|&k| st.present(k, mix_db))
        .collect();
    let places: Vec<(usize, Vec<place::PlaceObj>)> = {
        use rayon::prelude::*;
        present.par_iter().map(|&k| (k, place::place_objects(st.src[k][0], st.src[k][1], k, mix_db))).collect()
    };
    let with_notes: Vec<usize> = na.as_ref().map(|n| n.stems_with_instruments.iter().map(|s| *s as usize).collect()).unwrap_or_default();
    for (k, ps) in &places {
        if with_notes.contains(k) {
            continue;
        }
        let base = objs.len();
        for (i, p) in ps.iter().enumerate() {
            objs.push(place_obj(p, frames, i));
        }
        // the bass's and the voice's notes on their places (which flash on them: `map::place_hits`)
        if let Some(n) = na.as_ref().filter(|_| *k == stems::BASS || *k == stems::VOCALS) {
            let list = if *k == stems::BASS { &n.bass } else { &n.voice };
            let targets: Vec<(f32, &[u8])> = objs[base..].iter().map(|o| (o.x, &o.energy[..])).collect();
            for (pn, to) in list.iter().zip(notes::objects::assign_by_pan(list, &targets, MAP_FPS)) {
                if let Some(t) = to {
                    notes.push(note(&pn.note, base + t));
                }
            }
        }
    }
    // the note instruments, their coherence from their source's place around them
    if let Some(n) = &na {
        for (i, o) in n.objects.iter().enumerate() {
            let coh = places
                .iter()
                .find(|(k, _)| *k == o.stem as usize)
                .and_then(|(_, ps)| ps.iter().find(|p| o.x >= p.lo && o.x <= p.hi).or(ps.first()))
                .map(|p| p.coh.clone())
                .unwrap_or_default();
            let idx = objs.len();
            objs.push(note_obj(o, &coh, mix_db, frames, i));
            notes.extend(o.notes.iter().map(|m| note(m, idx)));
        }
    }
    // 5. the room
    objs.push(ambience(inp.rough_band, frames, inp.ref_db));

    let map = map::assemble(inp.content, frames, objs, notes, kit_bands);
    progress(1.0);
    crate::aelog!(
        "[SPATIAL] the track's map: {} objects, {} notes; kit {}; {:.0} s of sound: sources {:.1} s ({}), kit {:.1} s, notes {:.1} s, all {:.1} s",
        map.objects.len(), map.notes.len(), if kit_bands { "from the drums' bands" } else { "DrumSep" },
        len as f64 / core::SR as f64, t_sep, if gpu { "GPU" } else { "CPU" }, t_kit, t_notes, t0.elapsed().as_secs_f64()
    );
    Ok(map)
}

fn note(m: &notes::objects::MapNote, obj: usize) -> Note {
    Note { obj: obj.min(255) as u8, key: m.key, on: m.on, off: m.off, vel: m.vel, ghost: m.ghost }
}

/// HTDemucs over the whole track as `separate_track.py`: a segment every
/// three quarters of one, centred when the track ends inside it, weighted
/// by a triangle; `[source·2 + channel][len]`.
pub fn separate_all(c: &mut dyn core::Separate, l: &[f32], r: &[f32], progress: &mut dyn FnMut(f32), stop: &dyn Fn() -> bool) -> Result<Vec<Vec<f32>>, String> {
    let (len, seg) = (l.len(), core::SEG);
    let stride = (0.75 * seg as f64) as usize;
    let half = seg / 2;
    let top = (seg - half).max(half) as f32;
    let w: Vec<f32> = (0..seg).map(|i| if i < half { (i + 1) as f32 } else { (seg - i) as f32 } / top).collect();
    let mut out = vec![vec![0f32; len]; core::SOURCES * 2];
    let mut sw = vec![0f32; len];
    let segs = len.div_ceil(stride).max(1);
    for (k, off) in (0..len).step_by(stride).enumerate() {
        if stop() {
            return Err("stopped".into());
        }
        let n = seg.min(len - off);
        let start = off as isize - ((seg - n) / 2) as isize;
        let cut = |x: &[f32]| -> Vec<f32> {
            (0..seg as isize)
                .map(|i| {
                    let j = start + i;
                    if j >= 0 && (j as usize) < len { x[j as usize] } else { 0.0 }
                })
                .collect()
        };
        let o = c.separate(&cut(l), &cut(r))?;
        let trim = (seg - n) / 2;
        for (s, dst) in out.iter_mut().enumerate() {
            let src = &o[s * seg + trim..s * seg + trim + n];
            for i in 0..n {
                dst[off + i] += w[i] * src[i];
            }
        }
        for i in 0..n {
            sw[off + i] += w[i];
        }
        progress((k + 1) as f32 / segs as f32);
    }
    for dst in out.iter_mut() {
        for (v, s) in dst.iter_mut().zip(&sw) {
            *v /= *s;
        }
    }
    Ok(out)
}

/// The kit by DrumSep, when the GPU takes it; None: the bands instead.
fn kit_pieces(dir: &Path, drums: [&[f32]; 2], mix_db: f64, frames: usize, progress: &mut dyn FnMut(f32), stop: &dyn Fn() -> bool) -> Result<Option<Vec<MapObj>>, String> {
    let file = model("AURA_KIT_MODEL", dir, DRUMS_FILE);
    if !file.is_file() {
        crate::aelog!("[SPATIAL] no drum network at {} — the kit from the drums' bands", file.display());
        return Ok(None);
    }
    let mut k = match kit::Kit::open(dir, &file, false) {
        Ok(k) => k,
        Err(e) => {
            crate::aelog!("[SPATIAL] the kit from the drums' bands: {e}");
            return Ok(None);
        }
    };
    let len = drums[0].len();
    let mut run = kit::KitRun::new(len, kit::OVERLAP);
    let mut net = |l: &[f32], r: &[f32]| k.chunk(l, r);
    const BLOCK: usize = 1 << 20;
    for a in (0..len).step_by(BLOCK) {
        if stop() {
            return Err("stopped".into());
        }
        let b = (a + BLOCK).min(len);
        run.push(&mut net, &drums[0][a..b], &drums[1][a..b])?;
        progress(run.progress());
    }
    let out = run.finish(&mut net, mix_db)?;
    let kind = |n: &str| match n {
        "kick" => K_KICK,
        "snare" => K_SNARE,
        "toms" => K_TOMS,
        "hh" => K_HATS,
        _ => K_CYMBAL,
    };
    let mut cymbals = 0;
    Ok(Some(
        out.pieces
            .iter()
            .filter(|p| p.plays)
            .map(|p| {
                let kd = kind(p.name);
                let idx = if kd == K_CYMBAL { cymbals += 1; cymbals - 1 } else { 0 };
                let raw = Raw {
                    db: p.db.iter().map(|d| (*d as f64 - mix_db) as f32).collect(),
                    y: vec![map::height_of(p.lnf); frames],
                    coh: p.coh.clone(),
                };
                let [presence, energy, y, z, coherence] = map::finish(&raw, frames);
                MapObj {
                    kind: kd,
                    stem: stems::DRUMS as u8,
                    name: p.name.to_string(),
                    x: p.pan,
                    width: if kd == K_CYMBAL { 0.35 } else { 0.2 },
                    colour: notes::colour::colour(kd, None, idx),
                    presence,
                    energy,
                    y,
                    z,
                    coherence,
                    hits: p.onsets.clone(),
                }
            })
            .collect(),
    ))
}

/// The kit from the drums source's bands (no GPU for DrumSep): kick, snare
/// and hats as the app's band objects, their hits the onset peaks over the
/// research's best threshold for each (R1: F 0.74 / 0.55 / 0.73 on MDB).
fn kit_bands_of(st: &Stems, frames: usize, ref_db: f32) -> Vec<MapObj> {
    use rayon::prelude::*;
    let srcs: Vec<(&[f32], &[f32])> = (0..6).map(|s| (st.src[s][0], st.src[s][1])).collect();
    const CH: usize = 2048;
    let chunks: Vec<(usize, Vec<f32>)> = (0..frames.div_ceil(CH))
        .into_par_iter()
        .map(|i| {
            let s = analysis::Stft::new();
            let a = i * CH;
            (a, analysis::region(&s, &srcs, true, 0, a..(a + CH).min(frames)).band)
        })
        .collect();
    let mut band = vec![0f32; frames * BANDS * RAW];
    for (a, b) in &chunks {
        band[a * BANDS * RAW..a * BANDS * RAW + b.len()].copy_from_slice(b);
    }
    [(B_KICK, K_KICK, "kick", 0.5f32), (B_SNARE, K_SNARE, "snare", 0.05), (B_HATS, K_HATS, "hh", 0.3)]
        .into_iter()
        .map(|(b, kd, name, thr)| {
            let raw: Vec<f32> = (0..frames).flat_map(|j| band[(j * BANDS + b) * RAW..(j * BANDS + b + 1) * RAW].to_vec()).collect();
            let db: Vec<f32> = (0..frames).map(|j| 10.0 * (raw[j * RAW] + EPS).log10() - ref_db).collect();
            let fin = analysis::finish_band(&raw, frames, ref_db, &vec![p90(&db); frames], false);
            let on: Vec<f32> = fin.iter().map(|f| f[5]).collect();
            let mut xs: Vec<f32> = fin.iter().filter(|f| f[6] > 0.5).map(|f| f[0]).collect();
            xs.sort_by(|a, b| a.total_cmp(b));
            let raw = Raw { db, y: fin.iter().map(|f| f[1]).collect(), coh: fin.iter().map(|f| f[7]).collect() };
            let [presence, energy, y, z, coherence] = map::finish(&raw, frames);
            MapObj {
                kind: kd,
                stem: stems::DRUMS as u8,
                name: name.into(),
                x: xs.get(xs.len() / 2).copied().unwrap_or(0.0),
                width: 0.2,
                colour: notes::colour::colour(kd, None, 0),
                presence,
                energy,
                y,
                z,
                coherence,
                hits: peaks(&on, thr),
            }
        })
        .collect()
}

/// The 90th percentile of levels over −90 dB (−60 when none): a band's loud level.
fn p90(db: &[f32]) -> f32 {
    let mut v: Vec<f32> = db.iter().cloned().filter(|d| *d > -90.0).collect();
    v.sort_by(|a, b| a.total_cmp(b));
    v.get(v.len() * 9 / 10).copied().unwrap_or(-60.0)
}

/// kit.py `peaks`: the envelope's local maxima over `thr`, ±50 ms apart,
/// a flat top once. Seconds.
pub(super) fn peaks(env: &[f32], thr: f32) -> Vec<f32> {
    let n = env.len();
    let h = ((2.0 * 0.05 * MAP_FPS) as usize | 1) as isize / 2;
    let refl = |j: isize| -> usize {
        let mut j = j;
        loop {
            if j < 0 {
                j = -j - 1;
            } else if j >= n as isize {
                j = 2 * n as isize - 1 - j;
            } else {
                return j as usize;
            }
        }
    };
    (0..n)
        .filter(|&i| {
            let m = (-h..=h).map(|k| env[refl(i as isize + k)]).fold(f32::NEG_INFINITY, f32::max);
            env[i] == m && env[i] > thr && (i == 0 || env[i] != env[i - 1])
        })
        .map(|i| (i as f64 / MAP_FPS) as f32)
        .collect()
}

pub(super) fn kind_of_stem(stem: usize) -> u8 {
    match stem {
        stems::BASS => K_BASS,
        stems::VOCALS => K_VOICE,
        stems::GUITAR => K_GUITAR,
        stems::PIANO => K_PIANO,
        _ => K_OTHER,
    }
}

/// How wide a sound is: how unalike its channels are while it sounds.
pub(super) fn width_of(db: &[f32], coh: &[f32]) -> f32 {
    let mut s: Vec<f32> = db.iter().cloned().filter(|d| *d > -90.0).collect();
    s.sort_by(|a, b| a.total_cmp(b));
    let loud = s.get(s.len() * 99 / 100).copied().unwrap_or(-60.0);
    let c: Vec<f32> = db.iter().zip(coh).filter(|(d, _)| **d > loud - 30.0).map(|(_, c)| *c).collect();
    if c.is_empty() { 0.3 } else { (1.0 - c.iter().sum::<f32>() / c.len() as f32).clamp(0.0, 1.0) }
}

fn place_obj(p: &place::PlaceObj, frames: usize, index: usize) -> MapObj {
    let kd = kind_of_stem(p.stem);
    let raw = Raw { db: p.db.clone(), y: p.lnf.iter().map(|f| map::height_of(*f)).collect(), coh: p.coh.clone() };
    let [presence, energy, y, z, coherence] = map::finish(&raw, frames);
    MapObj {
        kind: kd,
        stem: p.stem as u8,
        name: p.name.clone(),
        x: p.pan,
        width: width_of(&p.db, &p.coh),
        colour: notes::colour::colour(kd, None, index),
        presence,
        energy,
        y,
        z,
        coherence,
        hits: p.onsets.clone(),
    }
}

fn note_obj(o: &notes::objects::NoteObj, coh: &[f32], mix_db: f64, frames: usize, _index: usize) -> MapObj {
    let raw = Raw {
        db: o.energy_db.iter().map(|d| (*d as f64 - mix_db) as f32).collect(),
        y: o.y.clone(),
        coh: if coh.is_empty() { vec![0.7; frames] } else { coh.to_vec() },
    };
    let [presence, energy, y, z, coherence] = map::finish(&raw, frames);
    MapObj {
        kind: kind_of_stem(o.stem as usize),
        stem: o.stem,
        name: o.name.clone(),
        x: o.x,
        width: o.width,
        colour: o.colour,
        presence,
        energy,
        y,
        z,
        coherence,
        hits: o.hits.clone(),
    }
}

/// The room: the mix's diffuse layer (the rough layer's ambience band), far.
fn ambience(rough_band: &[f32], frames: usize, ref_db: f32) -> MapObj {
    let raw: Vec<f32> = (0..frames)
        .flat_map(|j| rough_band.get((j * BANDS + B_AMB) * RAW..(j * BANDS + B_AMB + 1) * RAW).map(|s| s.to_vec()).unwrap_or(vec![0.0; RAW]))
        .collect();
    let db: Vec<f32> = (0..frames).map(|j| 10.0 * (raw[j * RAW] + EPS).log10() - ref_db).collect();
    let fin = analysis::finish_band(&raw, frames, ref_db, &vec![p90(&db); frames], true);
    let r = Raw { db, y: fin.iter().map(|f| f[1]).collect(), coh: fin.iter().map(|f| f[7]).collect() };
    let [presence, energy, y, _, coherence] = map::finish(&r, frames);
    MapObj {
        kind: K_AMBIENCE,
        stem: 255,
        name: "ambience".into(),
        x: 0.0,
        width: 1.0,
        colour: notes::colour::colour(K_AMBIENCE, None, 0),
        presence,
        energy,
        y,
        z: fin.iter().map(|f| (f[2].clamp(0.0, 1.0) * 255.0).round() as u8).collect(),
        coherence,
        hits: Vec::new(),
    }
}

/// «Ноты»' part, when the pack has the note network: None without it (every
/// source that is there then gets its places).
fn notes_of(dir: &Path, st: &Stems) -> Option<notes::objects::NotesOut> {
    let file = model("AURA_NOTES_MODEL", dir, NOTES_FILE);
    if !file.is_file() {
        crate::aelog!("[SPATIAL] no note network at {} — places only", file.display());
        return None;
    }
    let mut m = notes::bp::Model::open(dir, &file).map_err(|e| crate::aelog!("[SPATIAL] the note network: {e}")).ok()?;
    notes::objects::analyse(&mut m, st).map_err(|e| crate::aelog!("[SPATIAL] the notes: {e}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_peaks_are_kits() {
        // local maxima over the threshold, 50 ms apart, a flat top once
        let mut env = vec![0f32; 100];
        env[10] = 0.6;
        env[12] = 0.8;
        env[40] = 0.4;
        env[41] = 0.4;
        env[70] = 0.2;
        let p = peaks(&env, 0.3);
        let at = |i: usize| (i as f64 / MAP_FPS) as f32;
        assert_eq!(p, vec![at(12), at(40)]);
    }

    /// A whole track through the job with the pack (AURA_SPATIAL_PACK_DIR,
    /// AURA_KIT_MODEL, AURA_NOTES_MODEL), for a look and the time it takes:
    /// AURA_SPATIAL_FILE=<track> cargo test --profile fast job::tests::a_track -- --ignored --nocapture
    #[test]
    #[ignore]
    fn a_track_gives_its_map() {
        let (Some(file), Some(pack)) = (std::env::var_os("AURA_SPATIAL_FILE"), std::env::var_os("AURA_SPATIAL_PACK_DIR")) else { return };
        let (l, r) = super::super::decode::decode_44k(Path::new(&file), &|| false).expect("decode");
        let content = map::content_key(&l, &r);
        let rough = super::super::rough_layer(&file.to_string_lossy(), &l, &r);
        let inp = Input { l: &l, r: &r, content, rough_band: &rough.rough_band, ref_db: rough.ref_db };
        let t = std::time::Instant::now();
        let m = analyse(Path::new(&pack), &inp, &mut |_| {}, &|| false).expect("the map");
        eprintln!("{:.1} s for {:.0} s of sound; kit_bands {}", t.elapsed().as_secs_f64(), l.len() as f64 / 44100.0, m.kit_bands);
        for o in &m.objects {
            let e = o.energy.iter().map(|v| *v as u32).max().unwrap_or(0);
            eprintln!("  {:10} kind {:2} x {:+.2} w {:.2} hits {:4} energy max {e}", o.name, o.kind, o.x, o.width, o.hits.len());
        }
        eprintln!("  notes {}", m.notes.len());
        if let Ok(out) = std::env::var("AURA_MAP_OUT") {
            std::fs::write(out, m.to_bytes(0)).unwrap();
        }
        assert!(m.objects.iter().any(|o| o.kind == K_AMBIENCE));
    }
}
