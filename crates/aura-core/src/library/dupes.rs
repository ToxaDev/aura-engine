//! Copies of one track in the library, found when the index is grouped:
//!
//! 1. An exact copy: the same name (artist and title from the tags, else the
//!    file's name without its track number), the same kind of file, rate,
//!    bits and channels, the same size to the byte, and a length within half
//!    a second.
//! 2. The same audio with other tags: FLAC files whose STREAMINFO MD5 of the
//!    audio is the same (and the format), but whose sizes differ (tags
//!    edited, a cover put in).
//! 3. Another version: the same name, a length within two seconds, another
//!    kind of file or rate. Not a copy: never hidden, only told on the
//!    Duplicates page.
//!
//! Of each group of copies one is the main copy: shown, played and kept. The
//! others are hidden from the lists while the listener keeps them hidden.
//! An album all of whose tracks are copies of another album's is folded into
//! that album's card.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::index::{format_tag, Album, Track};

/// Lengths within this are the same track (an exact copy).
const SAME_S: f64 = 0.5;
/// Lengths within this are the same song (another version).
const VERSION_S: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Every file the same, to the byte.
    Exact,
    /// The same audio, other tags (FLAC MD5).
    Audio,
}

#[derive(Clone, Debug)]
pub struct Group {
    /// Indices into the tracks, the main copy among them.
    pub members: Vec<usize>,
    pub main: usize,
    pub kind: Kind,
}

/// Albums folded into one: `main` is shown, `copies` are copies of it.
#[derive(Clone, Debug)]
pub struct Twin {
    pub main: usize,
    pub copies: Vec<usize>,
    pub exact: bool,
}

#[derive(Default, Clone, Debug)]
pub struct Dupes {
    pub groups: Vec<Group>,
    /// Track → its group.
    pub group_of: HashMap<usize, usize>,
    /// Per track: a copy that is not the main one.
    pub hidden: Vec<bool>,
    /// Other versions of one song (information only).
    pub versions: Vec<Vec<usize>>,
    pub twins: Vec<Twin>,
    /// Album → its twin entry (as the main album or as a copy).
    pub album_twin: HashMap<usize, usize>,
    /// Converted versions beside each copy (`… [AE · …]`), by track.
    pub conversions: HashMap<usize, usize>,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// "01 - Numb", "01. Numb", "13 Numb" → "Numb". A name that is only a number,
/// or one starting with a year ("1979 …"), stays as it is.
pub fn strip_number(s: &str) -> String {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return s.to_string();
    }
    let rest = &s[digits..];
    if !rest.starts_with([' ', '-', '.', '_']) {
        return s.to_string();
    }
    let rest = rest.trim_start_matches([' ', '-', '.', '_']);
    if rest.is_empty() { s.to_string() } else { rest.to_string() }
}

fn stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

pub fn ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Artist and title as the library names a song: from the tags, else the
/// file's name without its number. The folder is not in it.
pub fn name_key(t: &Track) -> String {
    let st = stem(&t.path);
    let title = if t.tags.title.is_empty() || t.tags.title == st { strip_number(&st) } else { t.tags.title.clone() };
    format!("{}\0{}", norm(&t.tags.artist), norm(&title))
}

/// Everything an exact copy shares but the length.
fn exact_key(t: &Track) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        name_key(t),
        ext(&t.path),
        t.tags.sample_rate,
        t.tags.bits,
        t.tags.channels,
        t.size
    )
}

fn audio_md5(t: &Track) -> Option<&str> {
    t.tags.md5.as_deref().filter(|m| !m.is_empty() && m.bytes().any(|b| b != b'0'))
}

/// Whether `a` and `b` are exact copies of each other.
pub fn exact(a: &Track, b: &Track) -> bool {
    exact_key(a) == exact_key(b) && (a.tags.duration_s - b.tags.duration_s).abs() <= SAME_S
}

struct Uf(Vec<usize>);
impl Uf {
    fn find(&mut self, i: usize) -> usize {
        let mut r = i;
        while self.0[r] != r {
            r = self.0[r];
        }
        let mut i = i;
        while self.0[i] != r {
            let n = self.0[i];
            self.0[i] = r;
            i = n;
        }
        r
    }
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[b] = a;
        }
    }
}

/// Runs of `idx` (sorted by length) whose neighbours are within `within`
/// seconds.
fn by_length(tracks: &[Track], mut idx: Vec<usize>, within: f64) -> Vec<Vec<usize>> {
    idx.sort_by(|&a, &b| tracks[a].tags.duration_s.total_cmp(&tracks[b].tags.duration_s));
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in idx {
        match out.last_mut() {
            Some(run) if tracks[i].tags.duration_s - tracks[*run.last().unwrap()].tags.duration_s <= within => run.push(i),
            _ => out.push(vec![i]),
        }
    }
    out
}

/// How many converted versions (`<stem> [AE · …]`) are beside `path`.
pub fn conversions_beside(path: &str) -> usize {
    let p = Path::new(path);
    let (Some(dir), Some(st)) = (p.parent(), p.file_stem()) else { return 0 };
    let head = format!("{} [AE \u{b7} ", st.to_string_lossy());
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&head) && super::conversions::is_render_name(&n)
        })
        .count()
}

/// Inside `root` (a whole component prefix, any case on Windows).
pub fn under(path: &str, root: &Path) -> bool {
    let r = root.display().to_string();
    let r = r.trim_end_matches(['\\', '/']);
    if r.is_empty() {
        return false;
    }
    let (p, r) = if cfg!(windows) { (path.to_lowercase(), r.to_lowercase()) } else { (path.to_string(), r.to_string()) };
    p.len() > r.len() && p.starts_with(&r) && p[r.len()..].starts_with(['\\', '/'])
}

/// Which copy stays: ① has our conversions beside it, ② in the preferred
/// folder, ③ its album is fuller / has a cover, ④ came earlier, ⑤ the
/// shorter path. `Greater` = `a` is the better one.
fn better(a: usize, b: usize, tracks: &[Track], albums: &[Album], album_of: &[usize], conv: &HashMap<usize, usize>, prefer: Option<&Path>) -> Ordering {
    let c = |i: usize| conv.get(&i).copied().unwrap_or(0) > 0;
    let pref = |i: usize| prefer.is_some_and(|r| under(&tracks[i].path, r));
    let al = |i: usize| &albums[album_of[i]];
    c(a).cmp(&c(b))
        .then_with(|| pref(a).cmp(&pref(b)))
        .then_with(|| al(a).tracks.len().cmp(&al(b).tracks.len()))
        .then_with(|| al(a).cover.is_some().cmp(&al(b).cover.is_some()))
        .then_with(|| tracks[b].added.cmp(&tracks[a].added))
        .then_with(|| tracks[b].path.len().cmp(&tracks[a].path.len()))
        .then_with(|| tracks[b].path.cmp(&tracks[a].path))
}

/// The copies in `tracks` (grouped into `albums`; `album_of` = track → album).
/// `roots[0]` is the preferred folder.
pub fn find(tracks: &[Track], albums: &[Album], album_of: &[usize], roots: &[PathBuf]) -> Dupes {
    let n = tracks.len();
    let mut uf = Uf((0..n).collect());
    // 1. Exact copies.
    let mut by_exact: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        by_exact.entry(exact_key(t)).or_default().push(i);
    }
    for idx in by_exact.into_values().filter(|v| v.len() > 1) {
        for run in by_length(tracks, idx, SAME_S) {
            for w in run.windows(2) {
                uf.join(w[0], w[1]);
            }
        }
    }
    // 2. The same audio (FLAC MD5), the same format.
    let mut by_md5: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        if let Some(m) = audio_md5(t) {
            by_md5.entry(format!("{m}\0{}\0{}\0{}", t.tags.sample_rate, t.tags.bits, t.tags.channels)).or_default().push(i);
        }
    }
    for idx in by_md5.into_values().filter(|v| v.len() > 1) {
        for w in idx.windows(2) {
            uf.join(w[0], w[1]);
        }
    }
    let mut sets: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = uf.find(i);
        sets.entry(r).or_default().push(i);
    }
    let mut sets: Vec<Vec<usize>> = sets.into_values().filter(|v| v.len() > 1).collect();
    sets.sort_by_key(|v| v[0]);

    let mut conv = HashMap::new();
    for &i in sets.iter().flatten() {
        let c = conversions_beside(&tracks[i].path);
        if c > 0 {
            conv.insert(i, c);
        }
    }
    let prefer = roots.first().map(PathBuf::as_path);
    let mut groups: Vec<Group> = sets
        .into_iter()
        .map(|members| {
            let main = *members
                .iter()
                .max_by(|&&a, &&b| better(a, b, tracks, albums, album_of, &conv, prefer))
                .unwrap();
            let kind = if members.iter().all(|&m| exact(&tracks[m], &tracks[members[0]])) { Kind::Exact } else { Kind::Audio };
            Group { members, main, kind }
        })
        .collect();
    let mut group_of = HashMap::new();
    for (g, gr) in groups.iter().enumerate() {
        for &m in &gr.members {
            group_of.insert(m, g);
        }
    }

    // Albums whose every track is a copy of another album's: the fuller (then
    // the better) album first takes in the ones it covers.
    let album_rank = |a: usize| {
        let al = &albums[a];
        let c = al.tracks.iter().any(|t| conv.contains_key(t));
        let p = prefer.is_some_and(|r| under(&al.folder, r) || Path::new(&al.folder) == r);
        (al.tracks.len(), c, p, al.cover.is_some(), std::cmp::Reverse(al.added), std::cmp::Reverse(al.folder.len()))
    };
    let mut order: Vec<usize> = (0..albums.len()).collect();
    order.sort_by_key(|&a| std::cmp::Reverse(album_rank(a)));
    let mut twins: Vec<Twin> = Vec::new();
    let mut album_twin: HashMap<usize, usize> = HashMap::new();
    let mut done: HashSet<usize> = HashSet::new();
    for &v in &order {
        if album_twin.contains_key(&v) {
            continue;
        }
        done.insert(v);
        let mut covered: HashMap<usize, HashSet<usize>> = HashMap::new();
        for &t in &albums[v].tracks {
            let Some(&g) = group_of.get(&t) else { continue };
            for &m in &groups[g].members {
                let a = album_of[m];
                if a != v {
                    covered.entry(a).or_default().insert(m);
                }
            }
        }
        let mut copies: Vec<usize> = covered
            .into_iter()
            .filter(|(a, ts)| !done.contains(a) && !album_twin.contains_key(a) && ts.len() == albums[*a].tracks.len())
            .map(|(a, _)| a)
            .collect();
        if copies.is_empty() {
            continue;
        }
        copies.sort_unstable();
        let mut exact_all = true;
        for &a in &copies {
            for &t in &albums[a].tracks {
                let g = group_of[&t];
                // The main copy is the one in the album shown.
                if let Some(&mate) = groups[g].members.iter().find(|&&m| album_of[m] == v) {
                    groups[g].main = mate;
                    exact_all &= exact(&tracks[t], &tracks[mate]);
                }
            }
        }
        let k = twins.len();
        album_twin.insert(v, k);
        for &a in &copies {
            album_twin.insert(a, k);
            done.insert(a);
        }
        twins.push(Twin { main: v, copies, exact: exact_all });
    }

    let mut hidden = vec![false; n];
    for g in &groups {
        for &m in &g.members {
            hidden[m] = m != g.main;
        }
    }

    // 3. Other versions: one song in other formats (the main copies only).
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        let k = name_key(t);
        if !hidden[i] && !k.ends_with('\0') {
            by_name.entry(k).or_default().push(i);
        }
    }
    let fmt = |i: usize| {
        let t = &tracks[i];
        (ext(&t.path), t.tags.sample_rate, t.tags.bits, t.tags.dsd.clone())
    };
    let mut versions: Vec<Vec<usize>> = by_name
        .into_values()
        .filter(|v| v.len() > 1)
        .flat_map(|v| by_length(tracks, v, VERSION_S))
        .filter(|run| run.len() > 1 && run.iter().any(|&i| fmt(i) != fmt(run[0])))
        .collect();
    versions.sort_by_key(|v| v[0]);

    Dupes { groups, group_of, hidden, versions, twins, album_twin, conversions: conv }
}

impl Dupes {
    /// Tracks hidden as copies.
    pub fn hidden_count(&self) -> usize {
        self.hidden.iter().filter(|h| **h).count()
    }

    /// An album folded into another one's card.
    pub fn album_hidden(&self, a: usize) -> bool {
        self.album_twin.get(&a).is_some_and(|&k| self.twins[k].main != a)
    }

    /// The other copies of track `i`, paths.
    pub fn copies_of(&self, tracks: &[Track], i: usize) -> Vec<String> {
        self.group_of
            .get(&i)
            .map(|&g| self.groups[g].members.iter().filter(|&&m| m != i).map(|&m| tracks[m].path.clone()).collect())
            .unwrap_or_default()
    }
}

fn member_json(tracks: &[Track], albums: &[Album], album_of: &[usize], conv: &HashMap<usize, usize>, i: usize) -> Value {
    let t = &tracks[i];
    let al = &albums[album_of[i]];
    json!({
        "path": t.path, "size": t.size, "mtime": t.mtime, "added": t.added,
        "tag": format_tag(&t.tags, &t.path), "ext": ext(&t.path),
        "rate": t.tags.sample_rate, "bits": t.tags.bits, "channels": t.tags.channels, "dsd": t.tags.dsd,
        "durationS": t.tags.duration_s, "conversions": conv.get(&i).copied().unwrap_or(0),
        "albumTracks": al.tracks.len(), "cover": al.cover.is_some(), "album": al.id,
        // Exact copies of each other share this (and a length within 0.5 s).
        "ek": super::index::short_hash(&exact_key(t)),
    })
}

/// The Duplicates page: the groups, the folded albums and the other versions
/// with at least one file inside `scope` (all of them without one).
pub fn page(d: &Dupes, tracks: &[Track], albums: &[Album], album_of: &[usize], scope: Option<&Path>) -> Value {
    let inside = |i: usize| scope.is_none_or(|s| under(&tracks[i].path, s));
    let mj = |i: usize| member_json(tracks, albums, album_of, &d.conversions, i);
    let head = |i: usize| {
        let t = &tracks[i];
        json!({ "title": t.tags.title, "artist": t.tags.artist, "album": t.tags.album, "durationS": t.tags.duration_s })
    };
    // Twin rows: the copy album has two tracks or more (a single folded into
    // its album reads better as a group of its own).
    let mut twins = Vec::new();
    let mut twin_of_album: HashMap<usize, usize> = HashMap::new();
    for t in &d.twins {
        let copies: Vec<usize> = t.copies.iter().copied().filter(|&a| albums[a].tracks.len() > 1).collect();
        let all: Vec<usize> = std::iter::once(t.main).chain(copies.iter().copied()).collect();
        if copies.is_empty() || !all.iter().any(|&a| albums[a].tracks.iter().any(|&i| inside(i))) {
            continue;
        }
        let k = twins.len();
        for &a in &all {
            twin_of_album.insert(a, k);
        }
        let folders: Vec<Value> = all
            .iter()
            .map(|&a| {
                let al = &albums[a];
                let size: u64 = al.tracks.iter().map(|&i| tracks[i].size).sum();
                json!({
                    "folder": al.folder, "album": al.id, "tag": al.tag, "added": al.tracks.iter().map(|&i| tracks[i].added).min().unwrap_or(0),
                    "size": size, "tracks": al.tracks.len(),
                    "conversions": al.tracks.iter().map(|i| d.conversions.get(i).copied().unwrap_or(0)).sum::<usize>(),
                    "paths": al.tracks.iter().map(|&i| tracks[i].path.clone()).collect::<Vec<_>>(),
                    "rate": al.tracks.first().map(|&i| tracks[i].tags.sample_rate).unwrap_or(0),
                    "bits": al.tracks.first().map(|&i| tracks[i].tags.bits).unwrap_or(0),
                })
            })
            .collect();
        let m = &albums[t.main];
        twins.push(json!({
            "kind": if t.exact { "exact" } else { "audio" }, "title": m.title, "artist": m.artist,
            "tracks": m.tracks.len(), "main": 0, "folders": folders,
        }));
    }
    let mut groups = Vec::new();
    for g in &d.groups {
        if !g.members.iter().any(|&i| inside(i)) {
            continue;
        }
        let mut members = g.members.clone();
        members.sort_by_key(|&m| (m != g.main, tracks[m].path.to_lowercase()));
        // In a twin row when every member's album is in that row.
        let twin = {
            let ks: HashSet<Option<&usize>> = members.iter().map(|&m| twin_of_album.get(&album_of[m])).collect();
            match ks.into_iter().collect::<Vec<_>>()[..] {
                [Some(&k)] => Some(k),
                _ => None,
            }
        };
        let mut v = head(g.main);
        v["kind"] = json!(if g.kind == Kind::Exact { "exact" } else { "audio" });
        v["main"] = json!(0);
        v["twin"] = json!(twin);
        v["members"] = Value::Array(members.iter().map(|&m| mj(m)).collect());
        groups.push(v);
    }
    let mut versions = Vec::new();
    for run in &d.versions {
        if !run.iter().any(|&i| inside(i)) {
            continue;
        }
        let mut v = head(run[0]);
        v["kind"] = json!("versions");
        v["main"] = Value::Null;
        v["members"] = Value::Array(run.iter().map(|&m| mj(m)).collect());
        versions.push(v);
    }
    json!({ "groups": groups, "twins": twins, "versions": versions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tags::Tags;

    #[test]
    fn a_track_number_is_not_part_of_the_name() {
        assert_eq!(strip_number("01 - Numb"), "Numb");
        assert_eq!(strip_number("01. Numb"), "Numb");
        assert_eq!(strip_number("13 Numb"), "Numb");
        assert_eq!(strip_number("1979"), "1979");
        assert_eq!(strip_number("1979 Remaster"), "1979 Remaster");
        assert_eq!(strip_number("7"), "7");
        assert_eq!(strip_number("Numb"), "Numb");
        let t = |path: &str, title: &str| Track {
            path: path.into(), size: 1, mtime: 0, added: 0,
            tags: Tags { title: title.into(), ..Default::default() },
        };
        assert_eq!(name_key(&t("E:/a/01 - Numb.flac", "01 - Numb")), name_key(&t("C:/b/13 Numb.flac", "13 Numb")));
        assert_eq!(name_key(&t("E:/a/x.flac", "Numb  ")), name_key(&t("C:/b/y.flac", "numb")), "tags: case and spaces");
    }

    #[test]
    fn under_a_folder_means_a_whole_name() {
        assert!(under("E:\\MUSIC\\a.flac", Path::new("E:\\MUSIC")));
        assert!(under("E:\\MUSIC\\a.flac", Path::new("E:\\MUSIC\\")));
        assert!(!under("E:\\MUSIC2\\a.flac", Path::new("E:\\MUSIC")));
        assert!(!under("E:\\MUSIC", Path::new("E:\\MUSIC")));
    }
}
