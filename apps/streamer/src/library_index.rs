//! The library itself: every music file in the music folders with its tags,
//! kept on disk (`<data>/streamer/library-index.json`) so a restart does not read
//! them all again, grouped into albums, artists and genres for the remote. A
//! scan reads only what is new or changed since the last one (by size and time).
//! Covers come out small (`covers/`), so a grid of them is light over Wi-Fi.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::tags::{self as library_tags, Tags};

/// What the player can open (the same list as the app's file picker).
pub const AUDIO_EXT: [&str; 8] = ["wav", "flac", "mp3", "ogg", "aac", "m4a", "dsf", "dff"];

/// A finished scan older than this is done again (only the changes are read)
/// when a page next asks.
const FRESH: Duration = Duration::from_secs(15 * 60);
/// The side of a cover in the grid, in pixels (a tile is ~200 CSS px, ×2 for a
/// sharp screen).
const COVER_SIDE: u32 = 400;
/// Folders deeper than this are not walked (a link back up would never end).
const MAX_DEPTH: usize = 24;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    /// When the file came into being (created, else changed): "recently added".
    pub added: u64,
    #[serde(flatten)]
    pub tags: Tags,
}

#[derive(Clone, Debug)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: String,
    pub genres: Vec<String>,
    /// Indices into the tracks, in disc and track order.
    pub tracks: Vec<usize>,
    pub duration_s: f64,
    pub tag: String,
    pub hi_res: bool,
    pub dsd: bool,
    pub added: u64,
    pub folder: String,
    /// The cover's address (it changes with the file it comes from), if any.
    pub cover: Option<String>,
    cover_from: Option<CoverSrc>,
    /// Lower-case words to search in.
    hay: String,
}

/// Where an album's picture is: inside one of its files, or a file of its own.
#[derive(Clone, Debug)]
enum CoverSrc {
    Track(usize),
    File(PathBuf),
}

#[derive(Default)]
struct Built {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    by_id: HashMap<String, usize>,
}

#[derive(Default)]
struct Scan {
    running: bool,
    again: bool,
    last: Option<Instant>,
    error: Option<String>,
}

pub struct Index {
    file: Option<PathBuf>,
    covers: Option<PathBuf>,
    built: RwLock<Arc<Built>>,
    scan: Mutex<Scan>,
    done: AtomicUsize,
    total: AtomicUsize,
}

fn secs(t: std::io::Result<SystemTime>) -> u64 {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXT.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Hidden and system folders are not music.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('$') || name.eq_ignore_ascii_case("System Volume Information")
}

/// The converter's own output (`… [AE · …].flac`): it plays in its track's
/// place, it is not a track of its own (as in the converter's album scan).
fn is_render(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().contains("[AE \u{b7} "))
}

/// FNV-1a: a short id that stays the same from run to run.
fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// "CD1", "Disc 2", "disk 3": one disc of an album whose folder is the one up.
fn disc_folder(name: &str) -> bool {
    let n = name.to_lowercase();
    let rest = ["cd", "disc", "disk"].iter().find_map(|p| n.strip_prefix(p));
    rest.is_some_and(|r| {
        let r = r.trim_start_matches([' ', '_', '-', '.']);
        !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())
    })
}

/// "24/96", "16/44.1", "DSD64", or the file's kind for a lossy one.
pub fn format_tag(t: &Tags, path: &str) -> String {
    if !t.dsd.is_empty() {
        return t.dsd.clone();
    }
    if t.bits > 0 && t.sample_rate > 0 {
        let k = t.sample_rate as f64 / 1000.0;
        let k = if (k - k.round()).abs() < 1e-9 { format!("{}", k.round() as u32) } else { format!("{k:.1}") };
        return format!("{}/{k}", t.bits);
    }
    Path::new(path).extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default()
}

pub fn hi_res(t: &Tags) -> bool {
    !t.dsd.is_empty() || (t.bits > 16 && t.sample_rate >= 44_100) || (t.bits >= 16 && t.sample_rate > 48_000)
}

/// A cover image next to the files (what the engine's `probe_cover` looks for).
fn folder_art(dir: &Path) -> Option<PathBuf> {
    for stem in ["cover", "folder", "front", "album"] {
        for ext in ["jpg", "jpeg", "png"] {
            let p = dir.join(format!("{stem}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn group(tracks: Vec<Track>) -> Built {
    // Album: its folder (the one above a "CD1") and its album tag.
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut dirs: HashMap<String, PathBuf> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        let mut dir = Path::new(&t.path).parent().map(Path::to_path_buf).unwrap_or_default();
        if dir.file_name().is_some_and(|n| disc_folder(&n.to_string_lossy())) {
            if let Some(up) = dir.parent() {
                dir = up.to_path_buf();
            }
        }
        let key = format!("{}\0{}", dir.display().to_string().to_lowercase(), t.tags.album.to_lowercase());
        dirs.entry(key.clone()).or_insert(dir);
        groups.entry(key).or_default().push(i);
    }
    let mut albums = Vec::with_capacity(groups.len());
    for (key, mut idx) in groups {
        let dir = &dirs[&key];
        idx.sort_by(|&a, &b| {
            let (x, y) = (&tracks[a], &tracks[b]);
            (x.tags.disc_no, x.tags.track_no, x.path.to_lowercase()).cmp(&(y.tags.disc_no, y.tags.track_no, y.path.to_lowercase()))
        });
        let first = &tracks[idx[0]];
        let folder_name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let title = if first.tags.album.is_empty() { folder_name.clone() } else { first.tags.album.clone() };
        let pick = |f: &dyn Fn(&Track) -> &str| idx.iter().map(|&i| f(&tracks[i])).find(|s| !s.is_empty()).map(str::to_string);
        let artist = pick(&|t| &t.tags.album_artist).unwrap_or_else(|| {
            let mut a: Vec<&str> = idx.iter().map(|&i| tracks[i].tags.artist.as_str()).filter(|s| !s.is_empty()).collect();
            a.sort_unstable();
            a.dedup();
            match a.len() {
                0 => String::new(),
                1 => a[0].to_string(),
                _ => "Various Artists".into(),
            }
        });
        let year = pick(&|t| &t.tags.year).unwrap_or_default();
        let mut genres: Vec<String> = Vec::new();
        for &i in &idx {
            for g in tracks[i].tags.genre.split([';', '\0']).map(str::trim).filter(|g| !g.is_empty()) {
                if !genres.iter().any(|x| x.eq_ignore_ascii_case(g)) {
                    genres.push(g.to_string());
                }
            }
        }
        // The best format of its files names the album.
        let best = idx
            .iter()
            .map(|&i| &tracks[i])
            .max_by_key(|t| (!t.tags.dsd.is_empty(), t.tags.sample_rate, t.tags.bits))
            .unwrap();
        let tag = format_tag(&best.tags, &best.path);
        let cover_from = idx.iter().copied().find(|&i| tracks[i].tags.art).map(CoverSrc::Track).or_else(|| {
            folder_art(dir).or_else(|| Path::new(&first.path).parent().and_then(folder_art)).map(CoverSrc::File)
        });
        let id = hash(&key);
        let cover = cover_from.as_ref().map(|src| {
            let (from, when) = match src {
                CoverSrc::Track(i) => (tracks[*i].path.clone(), tracks[*i].mtime),
                CoverSrc::File(f) => (f.display().to_string(), std::fs::metadata(f).map(|m| secs(m.modified())).unwrap_or(0)),
            };
            format!("/api/lib/cover/{id}?v={}", &hash(&format!("{from}{when}"))[..8])
        });
        let mut hay = format!("{title} {artist} {year} {}", genres.join(" "));
        for &i in &idx {
            let t = &tracks[i].tags;
            hay.push_str(&format!(" {} {} {}", t.title, t.artist, t.composer));
        }
        albums.push(Album {
            id,
            title,
            artist,
            year,
            genres,
            duration_s: idx.iter().map(|&i| tracks[i].tags.duration_s).sum(),
            hi_res: hi_res(&best.tags),
            dsd: !best.tags.dsd.is_empty(),
            added: idx.iter().map(|&i| tracks[i].added).max().unwrap_or(0),
            folder: dir.display().to_string(),
            tag,
            cover,
            cover_from,
            hay: hay.to_lowercase(),
            tracks: idx,
        });
    }
    let by_id = albums.iter().enumerate().map(|(i, a)| (a.id.clone(), i)).collect();
    Built { tracks, albums, by_id }
}

/// Every word of `q` is somewhere in `hay`.
fn matches(hay: &str, words: &[String]) -> bool {
    words.iter().all(|w| hay.contains(w.as_str()))
}

fn words(q: Option<&str>) -> Vec<String> {
    q.unwrap_or("").to_lowercase().split_whitespace().map(str::to_string).collect()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumQuery {
    pub q: Option<String>,
    pub genre: Option<String>,
    pub artist: Option<String>,
    pub hi_res: bool,
    pub dsd: bool,
    /// "added" (the default), "title", "artist", "year".
    pub sort: Option<String>,
}

impl Index {
    pub fn open(file: Option<PathBuf>, covers: Option<PathBuf>) -> Arc<Index> {
        let tracks: Vec<Track> =
            file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Index {
            file,
            covers,
            built: RwLock::new(Arc::new(group(tracks))),
            scan: Mutex::new(Scan::default()),
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        })
    }

    fn built(&self) -> Arc<Built> {
        self.built.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Scan `roots` on a thread of its own, unless one runs (then once more
    /// after it, for the folders as they are now).
    pub fn rescan(self: &Arc<Self>, roots: impl Fn() -> Vec<PathBuf> + Send + 'static) {
        {
            let mut s = self.scan.lock().unwrap_or_else(|p| p.into_inner());
            if s.running {
                s.again = true;
                return;
            }
            s.running = true;
        }
        let me = self.clone();
        std::thread::Builder::new()
            .name("library-scan".into())
            .spawn(move || loop {
                let r = me.scan_now(&roots());
                let mut s = me.scan.lock().unwrap_or_else(|p| p.into_inner());
                s.error = r.err();
                s.last = Some(Instant::now());
                if !std::mem::take(&mut s.again) {
                    s.running = false;
                    break;
                }
            })
            .ok();
    }

    /// Scan when it never was, or long ago.
    pub fn freshen(self: &Arc<Self>, roots: impl Fn() -> Vec<PathBuf> + Send + 'static) {
        let stale = {
            let s = self.scan.lock().unwrap_or_else(|p| p.into_inner());
            !s.running && s.last.is_none_or(|t| t.elapsed() > FRESH)
        };
        if stale {
            self.rescan(roots);
        }
    }

    /// The scan itself, on this thread.
    pub fn scan_now(&self, roots: &[PathBuf]) -> Result<(), String> {
        let mut files = Vec::new();
        for r in roots {
            walk(r, 0, &mut files);
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|a, b| a.0 == b.0);
        let before = self.built();
        let known: HashMap<&str, &Track> = before.tracks.iter().map(|t| (t.path.as_str(), t)).collect();
        self.total.store(files.len(), Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        let tracks: Vec<Track> = files
            .par_iter()
            .filter_map(|(path, meta)| {
                let size = meta.len();
                let mtime = secs(meta.modified());
                let t = match known.get(path.as_str()) {
                    Some(k) if k.size == size && k.mtime == mtime => Some((*k).clone()),
                    k => match library_tags::read(Path::new(path)) {
                        Ok(tags) => Some(Track {
                            path: path.clone(),
                            size,
                            mtime,
                            // Copied in: its creation is when it came.
                            added: k.map(|k| k.added).unwrap_or_else(|| match secs(meta.created()) {
                                0 => mtime,
                                c => c,
                            }),
                            tags,
                        }),
                        Err(e) => {
                            aura_core::aelog!("[STREAMER] library: {e}");
                            None
                        }
                    },
                };
                self.done.fetch_add(1, Ordering::Relaxed);
                t
            })
            .collect();
        drop(known);
        let changed = tracks.len() != before.tracks.len()
            || tracks.iter().zip(before.tracks.iter()).any(|(a, b)| a.path != b.path || a.mtime != b.mtime || a.size != b.size);
        drop(before);
        if changed {
            self.save(&tracks);
            *self.built.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(group(tracks));
        }
        Ok(())
    }

    fn save(&self, tracks: &[Track]) {
        let Some(file) = &self.file else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension("json.tmp");
        let ok = serde_json::to_vec(tracks).ok().is_some_and(|b| std::fs::write(&tmp, b).is_ok());
        if !ok || std::fs::rename(&tmp, file).is_err() {
            aura_core::aelog!("[STREAMER] the library index was not saved");
        }
    }

    pub fn status(&self) -> Value {
        let b = self.built();
        let s = self.scan.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "scanning": s.running,
            "done": self.done.load(Ordering::Relaxed),
            "total": self.total.load(Ordering::Relaxed),
            "scanned": s.last.is_some(),
            "error": s.error,
            "tracks": b.tracks.len(),
            "albums": b.albums.len(),
        })
    }

    fn album_json(a: &Album) -> Value {
        json!({
            "id": a.id, "title": a.title, "artist": a.artist, "year": a.year, "genres": a.genres,
            "tracks": a.tracks.len(), "durationS": a.duration_s, "tag": a.tag, "hiRes": a.hi_res,
            "dsd": a.dsd, "added": a.added, "cover": a.cover, "folder": a.folder,
        })
    }

    pub fn albums(&self, q: &AlbumQuery) -> Value {
        let b = self.built();
        let w = words(q.q.as_deref());
        let mut list: Vec<&Album> = b
            .albums
            .iter()
            .filter(|a| matches(&a.hay, &w))
            .filter(|a| q.genre.as_ref().is_none_or(|g| a.genres.iter().any(|x| x.eq_ignore_ascii_case(g))))
            .filter(|a| q.artist.as_ref().is_none_or(|n| a.artist.eq_ignore_ascii_case(n)))
            .filter(|a| !q.hi_res || a.hi_res)
            .filter(|a| !q.dsd || a.dsd)
            .collect();
        let key = |s: &str| s.to_lowercase();
        match q.sort.as_deref().unwrap_or("added") {
            "title" => list.sort_by_key(|a| key(&a.title)),
            "artist" => list.sort_by_key(|a| (key(&a.artist), a.year.clone(), key(&a.title))),
            "year" => list.sort_by(|a, b| b.year.cmp(&a.year).then_with(|| key(&a.title).cmp(&key(&b.title)))),
            _ => list.sort_by(|a, b| b.added.cmp(&a.added).then_with(|| key(&a.title).cmp(&key(&b.title)))),
        }
        Value::Array(list.into_iter().map(Self::album_json).collect())
    }

    fn track_json(b: &Built, i: usize) -> Value {
        let t = &b.tracks[i];
        json!({
            "path": t.path, "title": t.tags.title, "artist": t.tags.artist, "album": t.tags.album,
            "trackNo": t.tags.track_no, "discNo": t.tags.disc_no, "durationS": t.tags.duration_s,
            "tag": format_tag(&t.tags, &t.path), "hiRes": hi_res(&t.tags),
        })
    }

    pub fn album(&self, id: &str) -> Result<Value, String> {
        let b = self.built();
        let a = b.by_id.get(id).map(|&i| &b.albums[i]).ok_or("No such album")?;
        let mut v = Self::album_json(a);
        v["list"] = Value::Array(a.tracks.iter().map(|&i| Self::track_json(&b, i)).collect());
        Ok(v)
    }

    /// Album artists, with how many albums each and one cover.
    pub fn artists(&self, q: Option<&str>) -> Value {
        let b = self.built();
        let w = words(q);
        let mut by: BTreeMap<String, (String, usize, usize, Option<String>)> = BTreeMap::new();
        for a in &b.albums {
            if a.artist.is_empty() || !matches(&a.artist.to_lowercase(), &w) {
                continue;
            }
            let e = by.entry(a.artist.to_lowercase()).or_insert_with(|| (a.artist.clone(), 0, 0, None));
            e.1 += 1;
            e.2 += a.tracks.len();
            if e.3.is_none() {
                e.3 = a.cover.clone();
            }
        }
        Value::Array(by.into_values().map(|(name, albums, tracks, cover)| json!({ "name": name, "albums": albums, "tracks": tracks, "cover": cover })).collect())
    }

    /// Genres, the most albums first.
    pub fn genres(&self) -> Value {
        let b = self.built();
        let mut by: HashMap<String, (String, usize, usize, Option<String>)> = HashMap::new();
        for a in &b.albums {
            for g in &a.genres {
                let e = by.entry(g.to_lowercase()).or_insert_with(|| (g.clone(), 0, 0, None));
                e.1 += 1;
                e.2 += a.tracks.len();
                if e.3.is_none() {
                    e.3 = a.cover.clone();
                }
            }
        }
        let mut v: Vec<_> = by.into_values().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        Value::Array(v.into_iter().map(|(name, albums, tracks, cover)| json!({ "name": name, "albums": albums, "tracks": tracks, "cover": cover })).collect())
    }

    /// Albums, artists and tracks that have every word of `q`.
    pub fn search(&self, q: &str) -> Value {
        let w = words(Some(q));
        if w.is_empty() {
            return json!({ "albums": [], "artists": [], "tracks": [] });
        }
        let b = self.built();
        let albums: Vec<Value> = b.albums.iter().filter(|a| matches(&a.hay, &w)).take(48).map(Self::album_json).collect();
        let tracks: Vec<Value> = b
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let g = &t.tags;
                matches(&format!("{} {} {} {}", g.title, g.artist, g.album, g.composer).to_lowercase(), &w)
            })
            .take(100)
            .map(|(i, _)| Self::track_json(&b, i))
            .collect();
        json!({ "albums": albums, "artists": self.artists(Some(q)), "tracks": tracks })
    }

    /// An album's cover, small, as JPEG; made once and kept in `covers/`.
    pub fn cover(&self, id: &str) -> Option<Vec<u8>> {
        let b = self.built();
        let a = b.by_id.get(id).map(|&i| &b.albums[i])?;
        let v = a.cover.as_deref()?.rsplit('=').next()?.to_string();
        let kept = self.covers.as_ref().map(|d| d.join(format!("{id}-{v}.jpg")));
        if let Some(bytes) = kept.as_ref().and_then(|k| std::fs::read(k).ok()) {
            return Some(bytes);
        }
        let raw = match a.cover_from.as_ref()? {
            CoverSrc::Track(i) => aura_core::player::probe::probe_cover(Path::new(&b.tracks[*i].path))?.0,
            CoverSrc::File(f) => std::fs::read(f).ok()?,
        };
        let small = shrink(&raw).unwrap_or(raw);
        if let Some(k) = kept {
            if let Some(d) = k.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(k, &small);
        }
        Some(small)
    }
}

/// A picture made `COVER_SIDE` on its longer side (never larger), as JPEG.
fn shrink(raw: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(raw).ok()?;
    let img = if img.width() > COVER_SIDE || img.height() > COVER_SIDE { img.thumbnail(COVER_SIDE, COVER_SIDE) } else { img };
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    img.to_rgb8().write_with_encoder(enc).ok()?;
    Some(out)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, std::fs::Metadata)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if is_hidden(&e.file_name().to_string_lossy()) {
            continue;
        }
        let p = e.path();
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        if meta.is_dir() {
            walk(&p, depth + 1, out);
        } else if is_audio(&p) && !is_render(&p) {
            out.push((p.display().to_string(), meta));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stereo WAV of silence.
    pub fn write_wav(p: &Path, rate: u32, bits: u16, frames: u32) {
        let block = 2 * bits / 8;
        let data = frames * block as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * block as u32).to_le_bytes());
        b.extend_from_slice(&block.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        b.resize(b.len() + data as usize, 0);
        std::fs::write(p, b).unwrap();
    }

    struct Tree(PathBuf);
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(name: &str) -> Tree {
        let d = std::env::temp_dir().join(format!("aura-idx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for sub in ["Alpha", "Beta/CD1", "Beta/CD2", ".hidden"] {
            std::fs::create_dir_all(d.join("music").join(sub)).unwrap();
        }
        write_wav(&d.join("music/Alpha/01 One.wav"), 96_000, 24, 960);
        write_wav(&d.join("music/Alpha/02 Two.wav"), 96_000, 24, 960);
        write_wav(&d.join("music/Beta/CD1/01 Three.wav"), 44_100, 16, 441);
        write_wav(&d.join("music/Beta/CD2/01 Four.wav"), 44_100, 16, 441);
        write_wav(&d.join("music/.hidden/x.wav"), 44_100, 16, 441);
        write_wav(&d.join("music/Alpha/01 One [AE \u{b7} 96k \u{b7} 1G].wav"), 96_000, 24, 960);
        std::fs::write(d.join("music/Beta/cover.jpg"), b"not really").unwrap();
        Tree(d)
    }

    #[test]
    fn folders_make_albums_discs_join_theirs_and_formats_name_them() {
        let t = tree("albums");
        let idx = Index::open(Some(t.0.join("state/index.json")), None);
        idx.scan_now(&[t.0.join("music")]).unwrap();
        let all = idx.albums(&AlbumQuery { sort: Some("title".into()), ..Default::default() });
        let all = all.as_array().unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0]["title"], "Alpha");
        assert_eq!(all[0]["tag"], "24/96");
        assert_eq!(all[0]["hiRes"], true);
        assert!(all[0]["cover"].is_null());
        assert_eq!(all[1]["title"], "Beta");
        assert_eq!(all[1]["tracks"], 2, "CD1 and CD2 are one album");
        assert_eq!(all[1]["tag"], "16/44.1");
        assert!(all[1]["cover"].as_str().unwrap().starts_with("/api/lib/cover/"));
        let hi = idx.albums(&AlbumQuery { hi_res: true, ..Default::default() });
        assert_eq!(hi.as_array().unwrap().len(), 1);
        let one = idx.album(all[0]["id"].as_str().unwrap()).unwrap();
        let names: Vec<_> = one["list"].as_array().unwrap().iter().map(|x| x["title"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["01 One", "02 Two"]);
        let found = idx.search("four");
        assert_eq!(found["tracks"].as_array().unwrap().len(), 1);
        assert_eq!(found["albums"][0]["title"], "Beta");
        assert!(idx.search("alpha nothing")["albums"].as_array().unwrap().is_empty(), "every word must match");
    }

    #[test]
    fn the_index_is_kept_and_only_changes_are_read_again() {
        let t = tree("kept");
        let file = t.0.join("state/index.json");
        Index::open(Some(file.clone()), None).scan_now(&[t.0.join("music")]).unwrap();
        let again = Index::open(Some(file.clone()), None);
        assert_eq!(again.status()["tracks"], 4, "read back from disk without a scan");
        std::fs::remove_file(t.0.join("music/Alpha/02 Two.wav")).unwrap();
        write_wav(&t.0.join("music/Alpha/03 New.wav"), 192_000, 24, 1920);
        again.scan_now(&[t.0.join("music")]).unwrap();
        let a = again.albums(&AlbumQuery { q: Some("alpha".into()), ..Default::default() });
        assert_eq!(a[0]["tracks"], 2);
        assert_eq!(a[0]["tag"], "24/192", "the best file names the album");
    }

    #[test]
    fn tags_name_rates_as_people_say_them() {
        let t = |bits, rate, dsd: &str| Tags { bits, sample_rate: rate, dsd: dsd.into(), ..Default::default() };
        assert_eq!(format_tag(&t(16, 44_100, ""), "a.flac"), "16/44.1");
        assert_eq!(format_tag(&t(24, 88_200, ""), "a.flac"), "24/88.2");
        assert_eq!(format_tag(&t(0, 44_100, ""), "a.mp3"), "MP3");
        assert_eq!(format_tag(&t(0, 2_822_400, "DSD64"), "a.dsf"), "DSD64");
        assert!(!hi_res(&t(16, 44_100, "")));
        assert!(hi_res(&t(24, 48_000, "")));
        assert!(!hi_res(&t(0, 96_000, "")), "a lossy file is not hi-res");
        assert!(disc_folder("CD1") && disc_folder("Disc 2") && disc_folder("disk-3"));
        assert!(!disc_folder("CDs") && !disc_folder("Discovery"));
    }

    #[test]
    fn a_cover_comes_out_small_and_is_kept() {
        let t = tree("cover");
        let mut big = image::RgbImage::new(1200, 900);
        big.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        big.save(t.0.join("music/Alpha/folder.png")).unwrap();
        let idx = Index::open(None, Some(t.0.join("covers")));
        idx.scan_now(&[t.0.join("music")]).unwrap();
        let a = idx.albums(&AlbumQuery { q: Some("alpha".into()), ..Default::default() });
        let id = a[0]["id"].as_str().unwrap();
        let jpg = idx.cover(id).unwrap();
        let img = image::load_from_memory(&jpg).unwrap();
        assert_eq!((img.width(), img.height()), (400, 300));
        assert_eq!(std::fs::read_dir(t.0.join("covers")).unwrap().count(), 1);
        assert!(idx.cover("nope").is_none());
    }
}
