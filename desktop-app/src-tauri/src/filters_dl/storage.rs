//! The filter files in the download folder, for the storage view, and
//! moving the ones the user picks to the Recycle Bin.

use std::path::{Path, PathBuf};

use super::manifest::Manifest;
use crate::audio::converter::dsp::filter as flt;

/// One file in the folder.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Item {
    pub name: String,
    pub size: u64,
    /// `"filter"`: a name the lookup can pick (whole, branch 0 and its two
    /// companions, or a legacy FS8 name); `"partial"`: an unfinished
    /// download; `"other"`: any other `fir_` file, which nothing reads.
    pub kind: &'static str,
    pub taps: Option<usize>,
    pub rate: Option<u32>,
    pub phase: Option<&'static str>,
    /// A legacy single-rate name (`fir_<taps>_<phase>.npy`, FS8 only).
    pub legacy: bool,
    /// In the signed list at its size: `Some(true)`; listed at another size:
    /// `Some(false)`; not listed or no list at hand: `None`.
    pub verified: Option<bool>,
}

fn taps_of(label: &str) -> Option<usize> {
    flt::TAP_LADDER.iter().copied().find(|t| flt::taps_label(*t) == Some(label))
}

fn phase_of(word: &str) -> Option<&'static str> {
    match word {
        "linear" => Some("linear_phase"),
        "minimum" => Some("minimum_phase"),
        _ => None,
    }
}

/// `(taps, rate, phase, legacy)` of a name the lookup can pick.
fn filter_name(name: &str) -> Option<(usize, Option<u32>, &'static str, bool)> {
    let rest = name.strip_prefix("fir_")?;
    let (stem, ext) = rest.split_once('.')?;
    let parts: Vec<&str> = stem.split('_').collect();
    match parts.as_slice() {
        [label, rate, phase, "phase"] if matches!(ext, "npy" | "b0.npy" | "b0.ends.npy" | "b0.json") => {
            let rate: u32 = rate.parse().ok()?;
            flt::TARGET_RATES.contains(&rate).then_some(())?;
            Some((taps_of(label)?, Some(rate), phase_of(phase)?, false))
        }
        [label, phase, "phase"] if ext == "npy" => Some((taps_of(label)?, None, phase_of(phase)?, true)),
        _ => None,
    }
}

/// Every `fir_` file in `dir`, by name.
pub fn list(dir: &Path, manifest: Option<&Manifest>) -> Vec<Item> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if !name.starts_with("fir_") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(ent.path()) else { continue };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let parsed = filter_name(&name);
        let kind = if name.ends_with(".part") {
            "partial"
        } else if parsed.is_some() {
            "filter"
        } else {
            "other"
        };
        let verified = manifest.and_then(|m| m.get(&name)).map(|e| e.size == size);
        out.push(Item {
            name,
            size,
            kind,
            taps: parsed.map(|p| p.0),
            rate: parsed.and_then(|p| p.1),
            phase: parsed.map(|p| p.2),
            legacy: parsed.is_some_and(|p| p.3),
            verified,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What became of one file asked to go to the Recycle Bin.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Removed {
    pub name: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// The path of `name` in `dir` when it is a plain `fir_` file there; the
/// names come from the interface and are never taken as paths.
fn checked(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let plain = name.starts_with("fir_")
        && !name.contains("..")
        && !name.contains(['/', '\\', ':'])
        && name.len() <= 160;
    if !plain {
        return Err("not a filter file name".to_string());
    }
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Ok(path),
        Ok(_) => Err("not a file".to_string()),
        Err(_) => Err("no such file".to_string()),
    }
}

/// Move exactly `names` from `dir` to the Recycle Bin with `to_bin`, one by
/// one, so one file that cannot go does not hold back the rest.
pub fn remove(dir: &Path, names: &[String], to_bin: &dyn Fn(&Path) -> Result<(), String>) -> Vec<Removed> {
    names
        .iter()
        .map(|name| {
            let r = checked(dir, name).and_then(|p| to_bin(&p));
            Removed { name: name.clone(), ok: r.is_ok(), error: r.err() }
        })
        .collect()
}

/// The system Recycle Bin (`trash`: Windows, macOS, the freedesktop trash).
pub fn to_recycle_bin(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::manifest::parse;
    use super::super::manifest::tests::{entry_json, manifest_json, SHA_A};
    use super::*;
    use std::sync::Mutex;

    fn put(dir: &Path, name: &str, len: usize) {
        std::fs::write(dir.join(name), vec![0u8; len]).unwrap();
    }

    #[test]
    fn the_folder_is_listed_by_kind() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        put(d, "fir_1G_705600_linear_phase.b0.npy", 10);
        put(d, "fir_1G_705600_linear_phase.b0.json", 2);
        put(d, "fir_1G_705600_linear_phase.b0.npy.part", 4);
        put(d, "fir_30M_minimum_phase.npy", 5);
        put(d, "fir_30M_352801_linear_phase.npy", 5);
        put(d, "fir_old_notes.txt", 1);
        put(d, "readme.txt", 1);
        std::fs::create_dir(d.join("fir_dir")).unwrap();
        let m = parse(&manifest_json(vec![
            entry_json("fir_1G_705600_linear_phase.b0.npy", 10, SHA_A, "1G", 705_600, "linear"),
            entry_json("fir_1G_705600_linear_phase.b0.json", 3, SHA_A, "1G", 705_600, "linear"),
        ]))
        .unwrap();
        let items = list(d, Some(&m));
        let by = |n: &str| items.iter().find(|i| i.name == n).unwrap().clone();
        assert_eq!(items.len(), 6);
        let b0 = by("fir_1G_705600_linear_phase.b0.npy");
        assert_eq!((b0.kind, b0.taps, b0.rate, b0.phase, b0.verified), ("filter", Some(1_000_000_000), Some(705_600), Some("linear_phase"), Some(true)));
        assert_eq!(by("fir_1G_705600_linear_phase.b0.json").verified, Some(false));
        assert_eq!(by("fir_1G_705600_linear_phase.b0.npy.part").kind, "partial");
        let legacy = by("fir_30M_minimum_phase.npy");
        assert!(legacy.legacy && legacy.kind == "filter" && legacy.rate.is_none() && legacy.verified.is_none());
        assert_eq!(by("fir_30M_352801_linear_phase.npy").kind, "other");
        assert_eq!(by("fir_old_notes.txt").kind, "other");
        assert!(list(&d.join("missing"), None).is_empty());
    }

    #[test]
    fn only_the_named_plain_files_go_to_the_bin() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("out");
        std::fs::create_dir(&d).unwrap();
        put(&d, "fir_a.npy", 1);
        put(&d, "fir_b.npy", 1);
        put(t.path(), "fir_outside.npy", 1);
        std::fs::create_dir(d.join("fir_dir")).unwrap();
        let binned = Mutex::new(Vec::new());
        let to_bin = |p: &Path| {
            binned.lock().unwrap().push(p.to_path_buf());
            if p.ends_with("fir_b.npy") { Err("in use".to_string()) } else { Ok(()) }
        };
        let names: Vec<String> = ["fir_a.npy", "fir_b.npy", "../fir_outside.npy", "..\\fir_outside.npy",
                                  "fir_dir", "fir_gone.npy", "other.npy", "C:fir_a.npy"]
            .iter().map(|s| s.to_string()).collect();
        let r = remove(&d, &names, &to_bin);
        assert_eq!(r.iter().filter(|x| x.ok).count(), 1);
        assert!(r[0].ok);
        assert_eq!(r[1].error.as_deref(), Some("in use"));
        assert_eq!(*binned.lock().unwrap(), vec![d.join("fir_a.npy"), d.join("fir_b.npy")]);
    }
}
