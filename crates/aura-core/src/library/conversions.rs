//! The converter's versions in the music folders: the files it wrote beside
//! their sources (`<stem> [AE · <tags>].flac|dsf`), what their names say, and
//! deleting them (only those, never a source). Shared by the app's Conversions
//! view and the streamer's.
//!
//! `scan` reads directory listings only, never a file's header, so it stays
//! quick on thousands of files; the stamps are read by `player_render_checks`.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

/// Where the converter's tag block starts in a file name.
const HEAD: &str = "[AE \u{b7} ";
/// Between the block's segments.
const SEP: &str = " \u{b7} ";

/// The extensions a source may have, in the order one is preferred when two
/// share a stem (the converter's album scan reads the same set).
const SOURCE_EXT: [&str; 15] = [
    "flac", "wav", "wave", "aif", "aiff", "dsf", "dff", "wv", "caf", "m4a", "aac", "mp3", "ogg", "oga", "opus",
];

/// What a converted file's name says about it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    /// The name before the block: the source's stem.
    pub stem: String,
    /// "pcm" (FLAC) or "dsd" (DSF).
    pub kind: &'static str,
    /// The output rate in Hz (PCM only).
    pub rate: Option<u32>,
    /// The DSD multiple (64 … 1024), DSD only.
    pub dsd: Option<u32>,
    /// The source's rate, when the name has one (`44.1k→…`).
    pub src_rate: Option<u32>,
    pub modulator: Option<String>,
    /// "Kaiser", "AURA", "Auto", "AutoShift".
    pub filter: Option<String>,
    /// The filter's size as the name spells it ("10M", "5k", "212Mˢ").
    pub taps: Option<String>,
    /// The hidden single-stage fallback's mark beside the filter.
    pub single: bool,
    pub precision: Option<u32>,
    pub stages: Vec<String>,
    /// Every segment of the block, as written.
    pub tags: Vec<String>,
}

/// The rates a name's kHz figure may stand for: "22.1k" is 22050, not 22100
/// (the name keeps one decimal).
const RATES: [u32; 20] = [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000, 705_600, 768_000, 1_411_200, 1_536_000,
];

/// "44.1k" → 44100, "384k" → 384000; a figure within 0.3 % of a standard rate
/// is that rate.
fn parse_khz(s: &str) -> Option<u32> {
    let k: f64 = s.strip_suffix('k')?.parse().ok()?;
    if !(k.is_finite() && k > 0.0 && k < 100_000.0) {
        return None;
    }
    let hz = (k * 1000.0).round() as u32;
    Some(RATES.iter().copied().find(|&r| (r as f64 - hz as f64).abs() <= r as f64 * 0.003).unwrap_or(hz))
}

/// "DSD256" → 256.
fn parse_dsd(s: &str) -> Option<u32> {
    s.strip_prefix("DSD")?.parse().ok().filter(|m: &u32| *m > 0)
}

/// The block and extension of a converted file's name, or None when the name
/// does not end in a `[AE · …]` block before `.flac` / `.dsf`.
fn split_name(name: &str) -> Option<(&str, &str, &'static str)> {
    let dot = name.rfind('.')?;
    let (base, ext) = (&name[..dot], &name[dot + 1..]);
    let kind = if ext.eq_ignore_ascii_case("flac") {
        "pcm"
    } else if ext.eq_ignore_ascii_case("dsf") {
        "dsd"
    } else {
        return None;
    };
    let inner_end = base.strip_suffix(']')?;
    let a = inner_end.rfind(HEAD)?;
    let inner = &inner_end[a + HEAD.len()..];
    if inner.is_empty() || inner.contains(']') {
        return None;
    }
    Some((base[..a].trim_end(), inner, kind))
}

/// Whether `name` is the converter's output: a final `[AE · …]` block and the
/// extension flac or dsf. The guard of `conv_trash`.
pub fn is_render_name(name: &str) -> bool {
    split_name(name).is_some()
}

/// Read a converted file's name (see `build_output_name`): the rate segment,
/// a DSF render's modulator, the filter and its size, the precision, then the
/// stages that ran and the source's width.
pub fn parse_name(name: &str) -> Option<Parsed> {
    let (stem, inner, kind) = split_name(name)?;
    let tags: Vec<String> = inner.split(SEP).map(str::to_string).collect();
    let mut p = Parsed { stem: stem.to_string(), kind, tags: tags.clone(), ..Default::default() };
    let mut rest = &tags[..];
    // The rate: "44.1k→384k", "384k" or "44.1k→DSD256".
    if let Some(first) = rest.first() {
        let (src, out) = match first.split_once('\u{2192}') {
            Some((a, b)) => (Some(a), b),
            None => (None, first.as_str()),
        };
        let dsd = parse_dsd(out);
        let rate = parse_khz(out);
        if dsd.is_some() || rate.is_some() {
            p.src_rate = src.and_then(parse_khz);
            p.dsd = dsd;
            p.rate = if dsd.is_some() { None } else { rate };
            rest = &rest[1..];
            // A DSD render names its modulator next.
            if dsd.is_some() {
                if let Some(m) = rest.first().filter(|m| m.starts_with("SDM")) {
                    p.modulator = Some(m.clone());
                    rest = &rest[1..];
                }
            }
        }
    }
    // The filter and its size: "Kaiser 10M", "Kaiser 10M Single".
    if let Some(f) = rest.first().filter(|f| f.contains(' ')) {
        let mut words = f.split(' ');
        p.filter = words.next().map(str::to_string);
        p.taps = words.next().map(str::to_string);
        p.single = words.any(|w| w == crate::audio::converter::dsp::ms_text::SINGLE_NAME_MARK);
        rest = &rest[1..];
    }
    p.precision = rest
        .iter()
        .find_map(|t| t.strip_prefix('f').filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())))
        .and_then(|d| d.parse().ok());
    // The stages, read as the player reads them, off the segments past the
    // rate and the modulator (a plain modulator's "SDM5" is no stage).
    let chain = format!("{HEAD}{}]", rest.join(SEP));
    p.stages = crate::player::chain::file_stages(&chain).into_iter().map(|(t, _, _)| t).collect();
    Some(p)
}

/// The source of a converted file with stem `stem` among the audio files of
/// its folder (`names`, lower case → as on disk): the same stem with a source
/// extension, the preferred one first.
pub fn source_for<'a>(stem: &str, names: &'a HashMap<String, String>) -> Option<&'a str> {
    let low = stem.to_lowercase();
    SOURCE_EXT.iter().find_map(|ext| names.get(&format!("{low}.{ext}")).map(String::as_str))
}

/// The folders to walk: each once, and none inside another one.
fn distinct_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut keyed: Vec<(PathBuf, PathBuf)> = roots
        .into_iter()
        .filter(|r| r.is_dir())
        .map(|r| (std::fs::canonicalize(&r).unwrap_or_else(|_| r.clone()), r))
        .collect();
    keyed.sort_by_key(|(k, _)| k.components().count());
    let mut kept: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (k, r) in keyed {
        if !kept.iter().any(|(kk, _)| k.starts_with(kk)) {
            kept.push((k, r));
        }
    }
    kept.into_iter().map(|(_, r)| r).collect()
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64)
}

/// Every converted file under `roots`, and the number of folders read.
pub fn scan(roots: Vec<PathBuf>) -> (Vec<Value>, usize) {
    let mut files = Vec::new();
    let mut dirs = 0usize;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut stack = distinct_roots(roots);
    while let Some(dir) = stack.pop() {
        // A folder reached twice (a junction back up the tree) is read once.
        if !seen.insert(std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        dirs += 1;
        let mut audio: HashMap<String, String> = HashMap::new();
        let mut renders: Vec<(String, std::fs::Metadata)> = Vec::new();
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                if !crate::library::index::is_hidden(&name) {
                    stack.push(e.path());
                }
            } else if ft.is_file() {
                if is_render_name(&name) {
                    if let Ok(m) = e.metadata() {
                        renders.push((name, m));
                    }
                } else {
                    audio.insert(name.to_lowercase(), name);
                }
            }
        }
        for (name, m) in renders {
            let Some(p) = parse_name(&name) else { continue };
            let src = source_for(&p.stem, &audio).map(|n| dir.join(n));
            let src_size = src.as_ref().and_then(|s| std::fs::metadata(s).ok()).map(|m| m.len());
            files.push(json!({
                "path": dir.join(&name),
                "source": src,
                "sourceSize": src_size,
                "size": m.len(),
                "mtimeMs": ms(m.modified()),
                "kind": p.kind,
                "rate": p.rate,
                "dsd": p.dsd,
                "srcRate": p.src_rate,
                "modulator": p.modulator,
                "filter": p.filter,
                "taps": p.taps,
                "single": p.single,
                "precision": p.precision,
                "stages": p.stages,
                "tags": p.tags,
            }));
        }
    }
    (files, dirs)
}

#[derive(Debug, Serialize)]
pub struct Trashed {
    pub path: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// `path` when it is a version this app wrote and it is there: the
/// converter's `[AE · …]` name AND its stamp inside the file (`AURA_RENDER`),
/// never anything else, so a source can not go through this view even when
/// someone named it like a render.
fn checked(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !is_render_name(&name) {
        return Err("not a converted file".to_string());
    }
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err("not a file".to_string()),
        Err(_) => return Err("no such file".to_string()),
    }
    let ours = crate::audio::converter::stamp::read_render_stamp(&p)
        .ok()
        .flatten()
        .is_some_and(|tags| tags.iter().any(|(k, _)| k == crate::audio::converter::stamp::KEY_RENDER));
    if !ours {
        return Err("not written by this app".to_string());
    }
    Ok(p)
}

/// Delete exactly `paths` with `del`, one by one, so one that can not go
/// does not hold back the rest.
pub fn trash_with(paths: &[String], del: &dyn Fn(&Path) -> Result<(), String>) -> Vec<Trashed> {
    paths
        .iter()
        .map(|path| {
            let r = checked(path).and_then(|p| del(&p));
            Trashed { path: path.clone(), ok: r.is_ok(), error: r.err() }
        })
        .collect()
}

/// `scan` as the Conversions view takes it: `{ files, scanned, ms }`.
pub fn scan_json(roots: Vec<PathBuf>) -> Value {
    let t = Instant::now();
    let (files, scanned) = scan(roots);
    json!({ "files": files, "scanned": scanned, "ms": t.elapsed().as_millis() as u64 })
}

/// Delete converted files for good (Anton 22.11: not to the Recycle Bin,
/// which renders of hundreds of MB would fill); anything else is refused.
/// A plain file removal: the Recycle Bin's COM call panicked on a pooled
/// thread whose apartment was already set up otherwise (0x80010106).
pub fn delete(paths: &[String]) -> Vec<Trashed> {
    let r = trash_with(paths, &|p| std::fs::remove_file(p).map_err(|e| e.to_string()));
    let gone = r.iter().filter(|t| t.ok).count();
    crate::aelog!("[LIBRARY] conversions: {gone} of {} deleted", r.len());
    for t in r.iter().filter(|t| !t.ok) {
        crate::aelog!("[LIBRARY] conversions: kept {} ({})", t.path, t.error.as_deref().unwrap_or(""));
    }
    r
}

/// The versions of `sources` made with the settings signature `sig`: each a
/// render beside its source whose stamp carries `sig`'s hash
/// (`stamp::KEY_SETTINGS`) and still is what the source sounds like in this
/// build (`stamp::check`), `[{ source, out, kind, dsd, rate, mtimeMs }]`,
/// in the order of `sources` and under each as it was asked, the newest
/// first for one source. So a player
/// with no record of a conversion (the streamer, another machine) still plays
/// the version that fits its rack. Each folder is listed once; only the
/// headers of the renders whose name points at one of `sources` are read.
pub fn matching(sig: &str, sources: &[PathBuf]) -> Vec<Value> {
    use crate::audio::converter::stamp;
    if sig.is_empty() {
        return Vec::new();
    }
    let want = stamp::settings_hash(sig);
    let mut by_dir: Vec<(PathBuf, Vec<&PathBuf>)> = Vec::new();
    for s in sources {
        let Some(dir) = s.parent() else { continue };
        match by_dir.iter_mut().find(|(d, _)| d == dir) {
            Some((_, v)) => v.push(s),
            None => by_dir.push((dir.to_path_buf(), vec![s])),
        }
    }
    let mut found: HashMap<String, Vec<(Option<u64>, Value)>> = HashMap::new();
    for (dir, srcs) in &by_dir {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut audio: HashMap<String, String> = HashMap::new();
        let mut renders: Vec<String> = Vec::new();
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if is_render_name(&name) {
                renders.push(name);
            } else {
                audio.insert(name.to_lowercase(), name);
            }
        }
        let asked: HashSet<String> = srcs
            .iter()
            .filter_map(|s| s.file_name())
            .map(|n| n.to_string_lossy().to_lowercase())
            .collect();
        for name in renders {
            let Some(p) = parse_name(&name) else { continue };
            let Some(src) = source_for(&p.stem, &audio) else { continue };
            if !asked.contains(&src.to_lowercase()) {
                continue;
            }
            let out = dir.join(&name);
            let Ok(Some(tags)) = stamp::read_render_stamp(&out) else { continue };
            if !tags.iter().any(|(k, v)| k == stamp::KEY_SETTINGS && v.eq_ignore_ascii_case(&want)) {
                continue;
            }
            if stamp::verdict(Some(&tags), crate::spatial::netkick::net_now()) != stamp::Verdict::Valid {
                continue;
            }
            let mtime = std::fs::metadata(&out).ok().and_then(|m| ms(m.modified()));
            let source = dir.join(src);
            found.entry(source.to_string_lossy().to_lowercase()).or_default().push((
                mtime,
                json!({
                    "source": source,
                    "out": out,
                    "kind": p.kind,
                    "dsd": p.dsd,
                    "rate": p.rate,
                    "mtimeMs": mtime,
                }),
            ));
        }
    }
    let mut res = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    for s in sources {
        // The source as the folder spells it: the key the renders were filed under.
        let Some(dir) = s.parent() else { continue };
        let Some(n) = s.file_name() else { continue };
        let key = dir.join(n).to_string_lossy().to_lowercase();
        if !done.insert(key.clone()) {
            continue;
        }
        if let Some(mut v) = found.remove(&key) {
            v.sort_by(|a, b| b.0.cmp(&a.0));
            // Each under the source as it was asked, so the caller finds it.
            res.extend(v.into_iter().map(|(_, mut j)| {
                j["source"] = json!(s);
                j
            }));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    // Names as `build_output_name` writes them (its own tests pin these).
    const PCM: &str = "03 Mal Bicho [AE · 44.1k→352.8k · Kaiser 10M · f64 · DC\u{b7}SUB20\u{b7}AA\u{b7}HP].flac";
    const DSD: &str = "03 Mal Bicho [AE · 44.1k→DSD256 · SDM5 · Kaiser 10M · f64 · AA].dsf";

    #[test]
    fn a_pcm_name_is_read() {
        let p = parse_name(PCM).unwrap();
        assert_eq!(p.stem, "03 Mal Bicho");
        assert_eq!((p.kind, p.rate, p.dsd, p.src_rate), ("pcm", Some(352_800), None, Some(44_100)));
        assert_eq!((p.filter.as_deref(), p.taps.as_deref(), p.single), (Some("Kaiser"), Some("10M"), false));
        assert_eq!(p.precision, Some(64));
        assert_eq!(p.stages, ["DC", "SUB20", "AA", "HP"]);
        assert_eq!(p.tags.len(), 4);
        assert_eq!(p.modulator, None);
    }

    #[test]
    fn a_dsd_name_is_read_without_its_modulator_as_a_stage() {
        let p = parse_name(DSD).unwrap();
        assert_eq!((p.kind, p.rate, p.dsd, p.src_rate), ("dsd", None, Some(256), Some(44_100)));
        assert_eq!(p.modulator.as_deref(), Some("SDM5"));
        assert_eq!(p.stages, ["AA"]);
        let la = parse_name("x [AE · 48k→DSD512 · SDM7 LA · Kaiser 5k · f32].dsf").unwrap();
        assert_eq!((la.dsd, la.src_rate, la.modulator.as_deref()), (Some(512), Some(48_000), Some("SDM7 LA")));
        assert_eq!((la.taps.as_deref(), la.precision), (Some("5k"), Some(32)));
        assert!(la.stages.is_empty());
    }

    #[test]
    fn rates_taps_and_marks_are_read() {
        let p = parse_name("a [AE · 384k · AutoShift 212M\u{2e2} · f64 · aHP · 6ch→2.0].flac").unwrap();
        assert_eq!((p.rate, p.src_rate), (Some(384_000), None));
        assert_eq!((p.filter.as_deref(), p.taps.as_deref()), (Some("AutoShift"), Some("212M\u{2e2}")));
        assert_eq!(p.stages, ["aHP"]);
        assert_eq!(p.tags.last().map(String::as_str), Some("6ch→2.0"));
        let s = parse_name("b [AE · 22.1k→705.6k · Kaiser 1G Single · f64 · AA\u{b7}HP+2].FLAC").unwrap();
        assert_eq!((s.src_rate, s.rate, s.single), (Some(22_050), Some(705_600), true));
        assert_eq!(s.stages, ["AA", "HP+2"]);
    }

    #[test]
    fn only_a_final_block_before_flac_or_dsf_counts() {
        assert!(parse_name("song.flac").is_none());
        assert!(parse_name("song [AE · 384k · Kaiser 10M · f64].wav").is_none());
        assert!(parse_name("song [AE · 384k · Kaiser 10M · f64] (copy).flac").is_none());
        assert!(parse_name("song [AE · ].flac").is_none());
        assert!(parse_name("[AE · 384k · Kaiser 10M · f64].flac").is_some());
        assert!(!is_render_name("song [live].flac"));
    }

    #[test]
    fn the_source_is_the_same_stem_preferring_lossless() {
        let names: HashMap<String, String> = ["03 Mal Bicho.mp3", "03 MAL BICHO.Flac", "other.flac"]
            .iter()
            .map(|n| (n.to_lowercase(), n.to_string()))
            .collect();
        assert_eq!(source_for("03 Mal Bicho", &names), Some("03 MAL BICHO.Flac"));
        assert_eq!(source_for("04 Gone", &names), None);
    }

    #[test]
    fn a_scan_finds_renders_with_their_sources_once() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("A");
        std::fs::create_dir_all(a.join(".hidden")).unwrap();
        std::fs::write(a.join("03 Mal Bicho.wav"), vec![0u8; 7]).unwrap();
        std::fs::write(a.join(PCM), vec![0u8; 3]).unwrap();
        std::fs::write(a.join(DSD), vec![0u8; 5]).unwrap();
        std::fs::write(a.join(".hidden").join(PCM), b"x").unwrap();
        std::fs::write(a.join("notes.txt"), b"x").unwrap();
        // The root and a folder inside it: walked once.
        let (files, dirs) = scan(vec![a.clone(), t.path().to_path_buf(), t.path().join("missing")]);
        assert_eq!(dirs, 2);
        assert_eq!(files.len(), 2);
        let pcm = files.iter().find(|f| f["kind"] == "pcm").unwrap();
        assert_eq!(pcm["size"], 3);
        assert_eq!(pcm["sourceSize"], 7);
        assert!(pcm["source"].as_str().unwrap().ends_with("03 Mal Bicho.wav"));
        assert_eq!(pcm["rate"], 352_800);
        assert!(pcm["mtimeMs"].as_u64().is_some());
    }

    /// A FLAC with only a VORBIS_COMMENT holding `tags`.
    fn flac(tags: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(3u32.to_le_bytes());
        body.extend(b"AE ");
        body.extend((tags.len() as u32).to_le_bytes());
        for t in tags {
            body.extend((t.len() as u32).to_le_bytes());
            body.extend(t.as_bytes());
        }
        let mut f = b"fLaC".to_vec();
        f.push(0x80 | 4);
        f.extend(&(body.len() as u32).to_be_bytes()[1..]);
        f.extend(body);
        f
    }

    #[test]
    fn trash_refuses_anything_but_a_converted_file() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        let source = d.join("03 Mal Bicho.flac");
        let render = d.join(PCM);
        let wav = d.join("song [AE · 384k · Kaiser 10M · f64].wav");
        // Named like a render, but not stamped by this app: a user's file.
        let named = d.join("named [AE · 384k · Kaiser 10M · f64].flac");
        std::fs::write(&source, flac(&["AURA_RENDER=2"])).unwrap();
        std::fs::write(&render, flac(&["AURA_RENDER=2", "AURA_STAGES=FIR:1"])).unwrap();
        std::fs::write(&wav, b"x").unwrap();
        std::fs::write(&named, flac(&["ARTIST=someone"])).unwrap();
        std::fs::create_dir(d.join(DSD)).unwrap();
        let s = |p: &Path| p.display().to_string();
        let paths =
            vec![s(&source), s(&wav), s(&d.join(DSD)), s(&d.join("gone [AE · 384k].flac")), s(&named), s(&render)];
        let binned = std::cell::RefCell::new(Vec::new());
        let r = trash_with(&paths, &|p| {
            binned.borrow_mut().push(p.to_path_buf());
            Ok(())
        });
        let ok: Vec<bool> = r.iter().map(|t| t.ok).collect();
        assert_eq!(ok, [false, false, false, false, false, true]);
        assert_eq!(r[0].error.as_deref(), Some("not a converted file"));
        assert_eq!(r[2].error.as_deref(), Some("not a file"));
        assert_eq!(r[3].error.as_deref(), Some("no such file"));
        assert_eq!(r[4].error.as_deref(), Some("not written by this app"));
        assert_eq!(*binned.borrow(), [render]);
        assert!(source.exists());
    }
    #[test]
    fn the_version_made_with_the_rack_is_found_by_its_stamp() {
        use crate::audio::converter::stamp;
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        let stamped = |sig: &str| {
            let tags: Vec<String> =
                stamp::stamp_tags(&[], false, Some(sig), 0, "").into_iter().map(|(k, v)| format!("{k}={v}")).collect();
            flac(&tags.iter().map(String::as_str).collect::<Vec<_>>())
        };
        let source = d.join("03 Mal Bicho.flac");
        std::fs::write(&source, b"src").unwrap();
        // Made with this rack; made with another; this rack's hash but an
        // older generation of the sound; another track's version.
        std::fs::write(d.join(PCM), stamped("{\"a\":1}")).unwrap();
        std::fs::write(d.join(DSD), stamped("{\"a\":2}")).unwrap();
        let old_hash = format!("AURA_SETTINGS={}", stamp::settings_hash("{\"a\":1}"));
        std::fs::write(d.join("03 Mal Bicho [AE · 44.1k→384k · Kaiser 10M · f64].flac"), flac(&["AURA_RENDER=0", &old_hash]))
            .unwrap();
        std::fs::write(d.join("04 Other.flac"), b"src").unwrap();
        std::fs::write(d.join("04 Other [AE · 44.1k→352.8k · Kaiser 10M · f64].flac"), stamped("{\"a\":1}")).unwrap();
        // The source asked in another case, twice: one answer.
        let asked = d.join("03 mal bicho.FLAC");
        let r = matching("{\"a\":1}", &[asked.clone(), asked]);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0]["out"].as_str().unwrap().ends_with(PCM));
        assert!(r[0]["source"].as_str().unwrap().ends_with("03 mal bicho.FLAC"));
        assert_eq!(r[0]["kind"], "pcm");
        let r = matching("{\"a\":2}", &[source.clone()]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["dsd"], 256);
        assert!(matching("{\"a\":3}", &[source.clone()]).is_empty());
        assert!(matching("", &[source]).is_empty());
    }
}
