//! The research's measure of the note instruments on the MDB songs (their
//! separated sources in `spatial\inv\cache`): every source's notes, their
//! portraits and instruments written as the research's notes files, for its
//! scorer and for a note-by-note comparison with its own files
//! (`tiers\notes\score_eval.py`, `compare_eval.py`).
//!
//! `AURA_NOTES_MDB` = the cache folder, `AURA_NOTES_EVAL_OUT` = where the
//! files go; the network from `AURA_SPATIAL_PACK_DIR` / `AURA_NOTES_MODEL`.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::{bp, cluster, stem};

const SONGS: [&str; 6] = ["BebopJazz", "ModalJazz", "FusionJazz", "LatinJazz", "Beatles", "Britpop"];
const STEMS: [(usize, &str); 5] = [(1, "bass"), (2, "other"), (3, "vocals"), (4, "guitar"), (5, "piano")];

fn f16(h: u16) -> f32 {
    let s = ((h >> 15) as u32) << 31;
    let e = ((h >> 10) & 0x1f) as u32;
    let m = (h & 0x3ff) as u32;
    let bits = match (e, m) {
        (0, 0) => s,
        (0, _) => {
            // subnormal: normalise
            let (mut e, mut m) = (127 - 15 + 1, m);
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            s | (e << 23) | ((m & 0x3ff) << 13)
        }
        (31, _) => s | 0x7f80_0000 | (m << 13),
        _ => s | ((e + 127 - 15) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// The `stems` array of a research npz — (6, 2, samples), f16 or f32 — as
/// (samples, `[source][channel][sample]` f32).
pub(crate) fn npz_stems(path: &Path) -> (usize, Vec<f32>) {
    let (shape, v) = npz_array(path, "stems");
    assert_eq!(&shape[..2], &[6, 2]);
    (shape[2], v)
}

/// An array of a research npz (f16 or f32, C order) as f32.
fn npz_array(path: &Path, name: &str) -> (Vec<usize>, Vec<f32>) {
    let mut za = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut b = Vec::new();
    za.by_name(&format!("{name}.npy")).unwrap().read_to_end(&mut b).unwrap();
    let (hl, start) = if b[6] == 1 {
        (u16::from_le_bytes([b[8], b[9]]) as usize, 10)
    } else {
        (u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize, 12)
    };
    let head = std::str::from_utf8(&b[start..start + hl]).unwrap();
    assert!(head.contains("'fortran_order': False"), "{head}");
    let s = head.split("'shape': (").nth(1).unwrap();
    let shape: Vec<usize> = s[..s.find(')').unwrap()].split(',').filter_map(|v| v.trim().parse().ok()).collect();
    let data = &b[start + hl..];
    let v: Vec<f32> = if head.contains("'<f2'") {
        data.chunks_exact(2).map(|c| f16(u16::from_le_bytes([c[0], c[1]]))).collect()
    } else if head.contains("'<f4'") {
        data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    } else {
        panic!("dtype: {head}")
    };
    (shape, v)
}

/// The note instruments of the reference tracks (`AURA_NOTES_TRACKS` = the
/// research stems folder: the mix and the six sources; MDB songs from
/// `AURA_NOTES_MDB`, their mix = the sources' sum), printed, and written to
/// `AURA_NOTES_EVAL_OUT\objects.json` for the colour page.
#[test]
#[ignore]
fn reference_tracks() {
    let (Some(tracks), Some(mdb), Some(out)) = (
        std::env::var_os("AURA_NOTES_TRACKS"),
        std::env::var_os("AURA_NOTES_MDB"),
        std::env::var_os("AURA_NOTES_EVAL_OUT"),
    ) else {
        return;
    };
    let pack = std::env::var_os("AURA_SPATIAL_PACK_DIR").unwrap();
    let model = std::env::var_os("AURA_NOTES_MODEL").unwrap();
    let mut net = bp::Model::open(Path::new(&pack), Path::new(&model)).unwrap();
    let mut doc = Vec::new();
    for name in ["oceanwind", "vakhteram", "backinblack", "jam", "BebopJazz", "Beatles"] {
        let p = PathBuf::from(&tracks).join(format!("{name}.npz"));
        let (len, data, mix) = if p.exists() {
            let (len, data) = npz_stems(&p);
            let (_, mix) = npz_array(&p, "mix");
            (len, data, mix)
        } else {
            let (len, data) = npz_stems(&PathBuf::from(&mdb).join(format!("{name}-stems.npz")));
            // the sources' sum, in f32 and in order, as the research's mix of an MDB song
            let mut mix = vec![0f32; 2 * len];
            for k in 0..6 {
                for (m, v) in mix.iter_mut().zip(&data[k * 2 * len..(k + 1) * 2 * len]) {
                    *m += v;
                }
            }
            (len, data, mix)
        };
        let ch = |k: usize, c: usize| &data[(k * 2 + c) * len..(k * 2 + c + 1) * len];
        let stems = crate::spatial::stems::Stems {
            src: std::array::from_fn(|k| [ch(k, 0), ch(k, 1)]),
            mix: [&mix[..len], &mix[len..2 * len]],
        };
        let t0 = std::time::Instant::now();
        let o = super::analyse(&mut net, &stems).unwrap();
        let dt = t0.elapsed();
        let line: Vec<String> = o
            .objects
            .iter()
            .map(|n| format!("{}({:+.2} w{:.2}, {} hits, {} notes)", n.name, n.x, n.width, n.hits.len(), n.notes.len()))
            .collect();
        eprintln!(
            "{name:12} {:5.0} s in {dt:.1?}: {} | bass {} notes, voice {} notes | stems {:?}",
            len as f64 / 44_100.0,
            line.join(" | "),
            o.bass.len(),
            o.voice.len(),
            o.stems_with_instruments
        );
        doc.push(json!({
            "track": name,
            "objects": o.objects.iter().map(|n| json!({
                "name": n.name, "stem": n.stem, "x": n.x, "width": n.width, "colour": n.colour,
                "hits": n.hits.len(), "notes": n.notes.len(),
                "timbre": { "pitch": n.timbre.pitch, "tilt": n.timbre.tilt, "oddeven": n.timbre.oddeven,
                            "attack": n.timbre.attack, "decay": n.timbre.decay },
            })).collect::<Vec<_>>(),
        }));
    }
    // the colours of the objects without notes (kit pieces, places, the room)
    use crate::spatial as sp;
    let palette: Vec<serde_json::Value> = [
        ("kick", sp::K_KICK),
        ("snare", sp::K_SNARE),
        ("toms", sp::K_TOMS),
        ("hats", sp::K_HATS),
        ("cymbal", sp::K_CYMBAL),
        ("bass", sp::K_BASS),
        ("voice", sp::K_VOICE),
        ("ambience", sp::K_AMBIENCE),
    ]
    .iter()
    .map(|&(n, k)| json!({ "name": n, "colour": super::colour::colour(k, None, 0), "second": super::colour::colour(k, None, 1) }))
    .collect();
    std::fs::create_dir_all(&out).unwrap();
    let all = json!({ "tracks": doc, "palette": palette });
    std::fs::write(PathBuf::from(&out).join("objects.json"), all.to_string()).unwrap();
}

fn r(v: f64, k: i32) -> serde_json::Value {
    if v.is_finite() {
        let m = 10f64.powi(k);
        json!((v * m).round() / m)
    } else {
        serde_json::Value::Null
    }
}

/// A source's notes file, as the research writes it (`run_a2.write`).
fn notes_file(song: &str, stem_name: &str, s: &stem::StemNotes, p: &stem::Portrait, lab: &[i32]) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = s
        .notes
        .iter()
        .enumerate()
        .map(|(i, e)| {
            json!({
                "id": i, "on": r(e.on, 4), "off": r(e.off, 4), "pitch": r(e.pitch, 3), "amp": r(e.amp, 4),
                "inst": lab[i],
                "feat": {
                    "dur": r(p.dur[i], 3), "parts": e.parts, "tilt": r(p.tilt[i], 3), "oddeven": r(p.oddeven[i], 3),
                    "h1_rel": r(p.h1_rel[i], 3), "attack": r(p.attack[i], 3), "rise_db": r(p.rise_db[i], 3),
                    "decay": r(p.decay[i], 3), "sustain": r(p.sustain[i], 3), "tail": r(p.tail[i], 3),
                    "tail_coh": r(p.tail_coh[i], 3), "vib_c": r(p.vib_c[i], 3), "pan": r(p.pan[i], 3),
                    "coh": r(p.coh[i], 3), "width_db": r(p.width_db[i], 3), "dres": r(p.dres[i], 3),
                    "lvl": r(p.lvl[i], 3), "midi": e.midi, "lvl_db": r(e.lvl, 1), "weak": e.weak,
                    "ghost": e.ghost.map_or(-1, |g| g as i64),
                    "env": p.env[i].iter().map(|&v| r(v, 1)).collect::<Vec<_>>(),
                    "h_rel": p.h_rel[i].iter().map(|&v| r(v, 1)).collect::<Vec<_>>(),
                }
            })
        })
        .collect();
    json!({ "track": song, "stem": stem_name, "sr": 44100, "notes": rows })
}

/// Every MDB song's five pitched sources → notes files (A2.1: the bass one
/// instrument, the others clustered).
#[test]
#[ignore]
fn mdb_notes_files() {
    let (Some(dir), Some(out)) = (std::env::var_os("AURA_NOTES_MDB"), std::env::var_os("AURA_NOTES_EVAL_OUT")) else {
        return;
    };
    let pack = std::env::var_os("AURA_SPATIAL_PACK_DIR").unwrap();
    let model = std::env::var_os("AURA_NOTES_MODEL").unwrap();
    let mut net = bp::Model::open(Path::new(&pack), Path::new(&model)).unwrap();
    let only = std::env::var("AURA_NOTES_SONGS").ok();
    for song in SONGS.iter().filter(|s| only.as_ref().is_none_or(|o| o.split(',').any(|x| x == **s))) {
        let (len, data) = npz_stems(&PathBuf::from(&dir).join(format!("{song}-stems.npz")));
        let ch = |k: usize, c: usize| &data[(k * 2 + c) * len..(k * 2 + c + 1) * len];
        let src: [[&[f32]; 2]; 6] = std::array::from_fn(|k| [ch(k, 0), ch(k, 1)]);
        let mix_ref = stem::mix_ref(&src);
        let dst = PathBuf::from(&out).join(song);
        std::fs::create_dir_all(&dst).unwrap();
        for (k, name) in STEMS {
            let t0 = std::time::Instant::now();
            let tr = bp::transcribe(net.run(&bp::to22(src[k][0], src[k][1])).unwrap(), &bp::Params::default());
            let t1 = t0.elapsed();
            let s = stem::notes_of(tr, src[k][0], src[k][1], mix_ref);
            let t2 = t0.elapsed();
            let p = stem::portrait(&s);
            let take: Vec<bool> = s.notes.iter().map(|n| n.used()).collect();
            let var = |n: &str| std::env::var(n).ok().and_then(|v| v.parse::<u64>().ok());
            let lab = match (var("AURA_NOTES_SEED"), var("AURA_NOTES_N_INIT")) {
                (None, None) => cluster::instruments(&p, &s.notes, &take, k != 1),
                (seed, n) => {
                    cluster::instruments_seeded(&p, &s.notes, &take, k != 1, seed.unwrap_or(0x5eed_0001), n.unwrap_or(3) as usize)
                }
            };
            let t3 = t0.elapsed();
            let doc = notes_file(song, name, &s, &p, &lab);
            std::fs::write(dst.join(format!("{name}.notes.json")), doc.to_string()).unwrap();
            let used = take.iter().filter(|&&v| v).count();
            let inst = lab.iter().copied().max().unwrap_or(-1) + 1;
            eprintln!(
                "{song:10} {name:6} {:5} notes, {used:5} used, {inst} instruments | notes {t1:.1?} measured {t2:.1?} all {t3:.1?}",
                s.notes.len()
            );
        }
    }
}

#[test]
fn half_floats() {
    assert_eq!(f16(0x3c00), 1.0);
    assert_eq!(f16(0xc000), -2.0);
    assert_eq!(f16(0x0001), 2f32.powi(-24));
    assert_eq!(f16(0x7bff), 65504.0);
    assert_eq!(f16(0x3555), 0.333_251_95);
}
