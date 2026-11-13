//! AutoEq profiles for the EQ window: the list of headphones AutoEq has
//! results for, and one result's ParametricEQ text. Only on the user's
//! search — nothing here runs in the background.
//!
//! AutoEq (github.com/jaakkopasanen/AutoEq, MIT) lists its results in two
//! Markdown files: `results/INDEX.md`, every result (`- [Name](./source/rig/Name)
//! by source on rig`), and `results/README.md`, the recommended one per model.
//! Both are read into one list kept in `<app data>/eq/autoeq-index.json` and
//! asked again at most once a week, with the ETag, so an unchanged list costs
//! a 304.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/";
const INDEX_FILE: &str = "INDEX.md";
const REC_FILE: &str = "README.md";
/// The kept list is asked again after a week.
const MAX_AGE_S: u64 = 7 * 24 * 3600;
/// INDEX.md is ~850 KB; room to grow, not to fill the memory.
const LIST_CAP: usize = 16 * 1024 * 1024;
/// A ParametricEQ.txt is under 1 KB.
const PROFILE_CAP: usize = 64 * 1024;
const OFFLINE: &str = "Can't reach AutoEq. Check the internet connection and try again.";

/// One result: `name` as AutoEq writes it, `source` who measured it, `rig`
/// what on (may be empty), `path` the folder under `results/`, decoded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rig: String,
    pub path: String,
    /// AutoEq's recommended result for this model.
    #[serde(default)]
    pub rec: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// Unix seconds of the last answer from the server (a 304 counts).
    fetched: u64,
    #[serde(default)]
    etag_index: Option<String>,
    #[serde(default)]
    etag_rec: Option<String>,
    entries: Vec<Entry>,
}

/// What the window gets: the list, when it was last checked, and whether
/// this is the kept list because the server could not be reached.
#[derive(Serialize)]
pub struct IndexReply {
    pub entries: Vec<Entry>,
    pub fetched: u64,
    pub stale: bool,
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn cache_path() -> Option<PathBuf> {
    crate::app_dir::root().map(|r| r.join("eq").join("autoeq-index.json"))
}

fn read_cache() -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(cache_path()?).ok()?).ok()
}

fn write_cache(c: &Cache) {
    let Some(p) = cache_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(bytes) = serde_json::to_vec(c) {
        let tmp = p.with_extension("json.part");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(format!("AuraEngine/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .https_only(true)
            .build()
            .unwrap_or_default()
    })
}

/// `%20` and UTF-8 escapes back to text; a broken escape stays as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One list line `- [Name](./a/b%20c/Name) by source on rig` →
/// (name, decoded path, "source on rig" or "").
fn parse_line(line: &str) -> Option<(String, String, String)> {
    let rest = line.trim_end().strip_prefix("- [")?;
    let mid = rest.find("](./")?;
    let name = rest[..mid].trim();
    let after = &rest[mid + 4..];
    // The path is escaped, so it holds no spaces; it ends at the link's ')'.
    let end = after.find(char::is_whitespace).unwrap_or(after.len());
    let path = after[..end].strip_suffix(')')?;
    let tail = after[end..].trim();
    let by = tail.strip_prefix("by ").unwrap_or("").trim();
    let path = percent_decode(path);
    if name.is_empty() || path.is_empty() || !safe_path(&path) {
        return None;
    }
    Some((name.to_string(), path, by.to_string()))
}

/// INDEX.md + README.md → the list; `rec` marks paths README.md lists.
fn build(index_md: &str, rec: &HashSet<String>) -> Vec<Entry> {
    let mut out: Vec<Entry> = index_md
        .lines()
        .filter_map(parse_line)
        .map(|(name, path, by)| {
            let (source, rig) = match by.split_once(" on ") {
                Some((s, r)) => (s.trim().to_string(), r.trim().to_string()),
                None => (by.clone(), String::new()),
            };
            // An INDEX line without "by" still names its source as the
            // first folder.
            let source = if source.is_empty() { path.split('/').next().unwrap_or("").to_string() } else { source };
            let rec = rec.contains(&path);
            Entry { name, source, rig, path, rec }
        })
        .collect();
    out.dedup_by(|a, b| a.path == b.path);
    out
}

fn rec_paths(readme_md: &str) -> HashSet<String> {
    readme_md.lines().filter_map(parse_line).map(|(_, p, _)| p).collect()
}

/// A path is AutoEq's: relative, no `..`, no backslashes or empty parts.
fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.contains('\\')
        && !p.contains(':')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// The ParametricEQ.txt of a result folder: `<path>/<last part> ParametricEQ.txt`.
fn profile_url(path: &str) -> Option<reqwest::Url> {
    if !safe_path(path) {
        return None;
    }
    let mut url = reqwest::Url::parse(BASE_URL).ok()?;
    let last = path.rsplit('/').next()?;
    {
        let mut seg = url.path_segments_mut().ok()?;
        seg.pop_if_empty();
        for s in path.split('/') {
            seg.push(s);
        }
        seg.push(&format!("{last} ParametricEQ.txt"));
    }
    Some(url)
}

fn fresh(c: &Cache, now: u64) -> bool {
    !c.entries.is_empty() && now.saturating_sub(c.fetched) < MAX_AGE_S
}

/// A list file's answer: new text with its ETag, or unchanged (304).
enum Got {
    New(String, Option<String>),
    Same,
}

async fn get_list(file: &str, etag: Option<&str>) -> Result<Got, String> {
    let mut req = client().get(format!("{BASE_URL}{file}"));
    if let Some(e) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, e);
    }
    let resp = req.send().await.map_err(|e| format!("{file}: {e}"))?;
    let st = resp.status();
    if st == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Got::Same);
    }
    if !st.is_success() {
        return Err(format!("{file}: HTTP {}", st.as_u16()));
    }
    let tag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
    if resp.content_length().is_some_and(|n| n as usize > LIST_CAP) {
        return Err(format!("{file} is too large"));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("{file}: {e}"))?;
    if bytes.len() > LIST_CAP {
        return Err(format!("{file} is too large"));
    }
    Ok(Got::New(String::from_utf8_lossy(&bytes).into_owned(), tag))
}

/// Folds the two answers into the kept list. An unchanged INDEX keeps the
/// kept entries; an unchanged README keeps the kept `rec` marks.
fn merge(old: Option<Cache>, index: Got, rec: Got, now: u64) -> Result<Cache, String> {
    let old = old.unwrap_or_default();
    let (rec_set, etag_rec) = match rec {
        Got::New(text, tag) => (rec_paths(&text), tag),
        Got::Same => (old.entries.iter().filter(|e| e.rec).map(|e| e.path.clone()).collect(), old.etag_rec.clone()),
    };
    let (entries, etag_index) = match index {
        Got::New(text, tag) => (build(&text, &rec_set), tag),
        Got::Same => {
            let mut e = old.entries.clone();
            for x in &mut e {
                x.rec = rec_set.contains(&x.path);
            }
            (e, old.etag_index.clone())
        }
    };
    if entries.is_empty() {
        return Err("AutoEq's list came back empty".into());
    }
    Ok(Cache { fetched: now, etag_index, etag_rec, entries })
}

async fn refresh(old: Option<Cache>) -> Result<Cache, String> {
    // A kept list without entries has nothing to reuse on a 304.
    let old = old.filter(|c| !c.entries.is_empty());
    let index = get_list(INDEX_FILE, old.as_ref().and_then(|c| c.etag_index.as_deref())).await?;
    let rec = get_list(REC_FILE, old.as_ref().and_then(|c| c.etag_rec.as_deref())).await?;
    merge(old, index, rec, now_s())
}

/// The list of AutoEq results: the kept one while it is under a week old,
/// else asked again; offline, the kept one marked `stale`.
#[tauri::command]
pub async fn eq_autoeq_index() -> Result<IndexReply, String> {
    let old = read_cache();
    let now = now_s();
    if let Some(c) = old.as_ref().filter(|c| fresh(c, now)) {
        return Ok(IndexReply { entries: c.entries.clone(), fetched: c.fetched, stale: false });
    }
    match refresh(old).await {
        Ok(c) => {
            write_cache(&c);
            Ok(IndexReply { entries: c.entries, fetched: c.fetched, stale: false })
        }
        Err(e) => match read_cache().filter(|c| !c.entries.is_empty()) {
            Some(c) => {
                eprintln!("[eq] AutoEq list not refreshed: {e}");
                Ok(IndexReply { entries: c.entries, fetched: c.fetched, stale: true })
            }
            None => {
                eprintln!("[eq] AutoEq list: {e}");
                Err(OFFLINE.into())
            }
        },
    }
}

/// One result's ParametricEQ text. Only paths of the kept list are asked.
#[tauri::command]
pub async fn eq_autoeq_fetch(path: String) -> Result<String, String> {
    let known = read_cache().is_some_and(|c| c.entries.iter().any(|e| e.path == path));
    if !known {
        return Err("This headphone is not in the AutoEq list".into());
    }
    let url = profile_url(&path).ok_or("This headphone is not in the AutoEq list")?;
    let resp = client().get(url).send().await.map_err(|e| {
        eprintln!("[eq] AutoEq profile: {e}");
        OFFLINE.to_string()
    })?;
    let st = resp.status();
    if !st.is_success() {
        return Err(format!("AutoEq has no parametric EQ for this result (HTTP {})", st.as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|_| OFFLINE.to_string())?;
    if bytes.len() > PROFILE_CAP {
        return Err("The AutoEq profile is too large".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "# Index\nThis is a list of all equalization profiles.\n\n\
- [1Custom SA02](./crinacle/711%20in-ear/1Custom%20SA02) by crinacle on 711\n\
- [1MORE Aero (ANC Off)](./HypetheSonics/GRAS%20RA0045%20in-ear/1MORE%20Aero%20(ANC%20Off)) by HypetheSonics on GRAS RA0045\n\
- [HD 600](./oratory1990/over-ear/Sennheiser%20HD%20600) by oratory1990\n\
- [HD 600](./Innerfidelity/over-ear/Sennheiser%20HD%20600) by Innerfidelity\n\
- [écoute TH1](./Kuulokenurkka/over-ear/%C3%A9coute%20TH1) by Kuulokenurkka\n\
- [Evil](./../../etc/passwd) by nobody\n\
not a list line\n";

    const README: &str = "# Recommended Results\n\n\
- [HD 600](./oratory1990/over-ear/Sennheiser%20HD%20600)\n\
- [1Custom SA02](./crinacle/711%20in-ear/1Custom%20SA02)\n";

    fn rec() -> HashSet<String> {
        rec_paths(README)
    }

    #[test]
    fn the_index_reads_names_sources_rigs_and_paths() {
        let e = build(INDEX, &rec());
        assert_eq!(e.len(), 5, "the line with .. and the prose are left out");
        assert_eq!(
            e[0],
            Entry {
                name: "1Custom SA02".into(),
                source: "crinacle".into(),
                rig: "711".into(),
                path: "crinacle/711 in-ear/1Custom SA02".into(),
                rec: true,
            }
        );
        assert_eq!(e[1].name, "1MORE Aero (ANC Off)");
        assert_eq!(e[1].path, "HypetheSonics/GRAS RA0045 in-ear/1MORE Aero (ANC Off)");
        assert_eq!(e[1].rig, "GRAS RA0045");
        assert!(!e[1].rec);
        assert_eq!((e[2].source.as_str(), e[2].rig.as_str(), e[2].rec), ("oratory1990", "", true));
        assert!(!e[3].rec, "the other measurement of the same model is not the recommended one");
        assert_eq!(e[4].path, "Kuulokenurkka/over-ear/écoute TH1");
    }

    #[test]
    fn percent_escapes_decode_and_broken_ones_stay() {
        assert_eq!(percent_decode("a%20b%C3%A9"), "a bé");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
        assert_eq!(percent_decode("%é"), "%é");
    }

    #[test]
    fn the_profile_url_escapes_each_part() {
        let u = profile_url("HypetheSonics/GRAS RA0045 in-ear/1MORE Aero (ANC Off)").unwrap();
        assert_eq!(
            u.as_str(),
            "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/HypetheSonics/GRAS%20RA0045%20in-ear/\
1MORE%20Aero%20(ANC%20Off)/1MORE%20Aero%20(ANC%20Off)%20ParametricEQ.txt"
        );
        let u = profile_url("a/b#c/d?e").unwrap();
        assert!(u.as_str().ends_with("/a/b%23c/d%3Fe/d%3Fe%20ParametricEQ.txt"), "{u}");
    }

    #[test]
    fn paths_outside_the_results_are_refused() {
        for p in ["", "../x", "a/../b", "a//b", "/abs", "a\\b", "C:/x", "a/./b"] {
            assert!(profile_url(p).is_none(), "{p}");
        }
    }

    #[test]
    fn a_kept_list_is_used_for_a_week() {
        let c = Cache { fetched: 1000, entries: build(INDEX, &rec()), ..Default::default() };
        assert!(fresh(&c, 1000 + MAX_AGE_S - 1));
        assert!(!fresh(&c, 1000 + MAX_AGE_S));
        assert!(!fresh(&Cache { fetched: 1000, ..Default::default() }, 1001), "an empty list is never fresh");
    }

    #[test]
    fn unchanged_answers_keep_the_kept_list() {
        let old = Cache {
            fetched: 1,
            etag_index: Some("i1".into()),
            etag_rec: Some("r1".into()),
            entries: build(INDEX, &rec()),
        };
        let kept = old.entries.clone();
        let c = merge(Some(old), Got::Same, Got::Same, 99).unwrap();
        assert_eq!(c.entries, kept);
        assert_eq!((c.fetched, c.etag_index.as_deref(), c.etag_rec.as_deref()), (99, Some("i1"), Some("r1")));
    }

    #[test]
    fn a_new_readme_moves_the_marks_on_the_kept_entries() {
        let old = Cache { fetched: 1, entries: build(INDEX, &rec()), ..Default::default() };
        let readme = "- [HD 600](./Innerfidelity/over-ear/Sennheiser%20HD%20600)\n";
        let c = merge(Some(old), Got::Same, Got::New(readme.into(), Some("r2".into())), 5).unwrap();
        let marked: Vec<&str> = c.entries.iter().filter(|e| e.rec).map(|e| e.source.as_str()).collect();
        assert_eq!(marked, ["Innerfidelity"]);
        assert_eq!(c.etag_rec.as_deref(), Some("r2"));
    }

    #[test]
    fn a_new_index_keeps_the_marks_of_an_unchanged_readme() {
        let old = Cache { fetched: 1, entries: build(INDEX, &rec()), ..Default::default() };
        let c = merge(Some(old), Got::New(INDEX.into(), Some("i2".into())), Got::Same, 5).unwrap();
        assert_eq!(c.entries.iter().filter(|e| e.rec).count(), 2);
        assert_eq!(c.etag_index.as_deref(), Some("i2"));
    }

    #[test]
    fn an_empty_list_is_an_error_not_a_cache() {
        assert!(merge(None, Got::New("# nothing".into(), None), Got::New(String::new(), None), 5).is_err());
    }

    #[test]
    fn the_kept_list_reads_back() {
        let c = Cache { fetched: 7, etag_index: Some("x".into()), etag_rec: None, entries: build(INDEX, &rec()) };
        let back: Cache = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back.entries, c.entries);
        assert_eq!(back.fetched, 7);
    }
}
