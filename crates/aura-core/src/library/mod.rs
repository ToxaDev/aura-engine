//! The folders of music the library reads: the streamer's remote and the
//! desktop app share this code. The player takes file paths, and anyone on the
//! network can reach the streamer's server, so a path from a request counts
//! only inside these folders: nothing else on the disk can be listed, probed or
//! played through it. Choosing the folders needs a look at the disk, so that one
//! view lists folder names and nothing else.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub mod conversions;
pub mod dupes;
pub mod index;
pub mod tags;

use index::{is_audio, is_hidden, AlbumQuery, Index};

pub struct Library {
    roots: Arc<Mutex<Vec<PathBuf>>>,
    file: Option<PathBuf>,
    /// Every file's tags, as albums, artists and genres.
    pub index: Arc<Index>,
}

impl Library {
    /// The folders kept in `file`. A first run starts on `first`, if it is there.
    pub fn open(file: Option<PathBuf>, first: Option<PathBuf>) -> Arc<Library> {
        let kept: Option<Vec<PathBuf>> =
            file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok());
        let roots = kept.unwrap_or_else(|| first.into_iter().filter(|p| p.is_dir()).collect());
        let dir = file.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf);
        let index =
            Index::open_in(dir.as_ref().map(|d| d.join("library-index.json")), dir.map(|d| d.join("covers")), roots.clone());
        Arc::new(Library { roots: Arc::new(Mutex::new(roots)), file, index })
    }

    /// The folders as they are when it is asked (for a scan on its own thread).
    fn roots_fn(&self) -> impl Fn() -> Vec<PathBuf> + Send + 'static {
        let roots = self.roots.clone();
        move || roots.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The library's commands (`lib_*`) beyond the folders' own.
    pub fn command(&self, cmd: &str, args: Value) -> Result<Value, String> {
        fn arg<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, String> {
            // No arguments at all reads as none given.
            let args = if args.is_null() { json!({}) } else { args };
            serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Q {
            q: Option<String>,
            id: String,
            /// Show the copies too (DUPES: SHOWN).
            dupes: bool,
            /// The Duplicates page: only groups with a file inside this folder.
            scope: Option<String>,
            /// lib_trash: the files, and whether folders left with no music go too.
            paths: Vec<String>,
            folders: bool,
        }
        match cmd {
            "lib_status" => {
                self.index.freshen(self.roots_fn());
                let mut v = self.index.status();
                v["roots"] = json!(self.roots());
                Ok(v)
            }
            "lib_rescan" => {
                self.index.rescan(self.roots_fn());
                Ok(Value::Null)
            }
            "lib_albums" => Ok(self.index.albums(&arg::<AlbumQuery>(args)?)),
            "lib_album" => self.index.album(&arg::<Q>(args)?.id),
            "lib_artists" => {
                let a = arg::<Q>(args)?;
                Ok(self.index.artists(a.q.as_deref(), a.dupes))
            }
            "lib_genres" => Ok(self.index.genres(arg::<Q>(args)?.dupes)),
            "lib_search" => {
                let a = arg::<Q>(args)?;
                Ok(self.index.search(a.q.as_deref().unwrap_or(""), a.dupes))
            }
            "lib_dupes" => {
                let a = arg::<Q>(args)?;
                let scope = a.scope.filter(|s| !s.is_empty());
                if let Some(s) = &scope {
                    if !self.allowed(s) {
                        return Err("That folder is not in the library".into());
                    }
                }
                Ok(self.index.dupes_page(scope.as_deref()))
            }
            "lib_trash" => {
                let a = arg::<Q>(args)?;
                Ok(self.trash(&a.paths, a.folders))
            }
            _ => Err(format!("{cmd} is not a library command")),
        }
    }

    /// Copies to the system Recycle Bin, never straight off the disk. What
    /// plays now is skipped, and nothing while a conversion runs.
    pub fn trash(&self, paths: &[String], folders: bool) -> Value {
        let playing = crate::player::controller::get().heard().map(|h| h.1);
        let converting = crate::audio::converter::is_running();
        let busy = |p: &str| -> Option<String> {
            if converting {
                Some("a conversion is running".into())
            } else if playing.as_deref() == Some(p) {
                Some("it plays now".into())
            } else {
                None
            }
        };
        // A thread of its own: the Recycle Bin is a COM call that sets up its
        // apartment, which a pooled thread may have set up otherwise.
        std::thread::scope(|sc| {
            sc.spawn(|| self.trash_with(paths, folders, &busy, &crate::filters_dl::storage::to_recycle_bin)).join()
        })
        .unwrap_or_else(|_| json!({ "moved": 0, "bytes": 0, "folders": 0, "skipped": [], "error": "Moving files failed." }))
    }

    /// `trash` with what is busy and how a file goes given. Each file must be
    /// inside the music folders, a music file, in the index with the same size
    /// and time, and not the last copy of its track left.
    pub fn trash_with(
        &self,
        paths: &[String],
        folders: bool,
        busy: &(dyn Fn(&str) -> Option<String> + Sync),
        del: &(dyn Fn(&Path) -> Result<(), String> + Sync),
    ) -> Value {
        let mut gone: Vec<String> = Vec::new();
        let mut gone_set: HashSet<String> = HashSet::new();
        let mut skipped: Vec<Value> = Vec::new();
        let mut bytes = 0u64;
        let mut seen = HashSet::new();
        for p in paths {
            if !seen.insert(p.clone()) {
                continue;
            }
            let why = (|| -> Result<u64, String> {
                if !self.allowed(p) {
                    return Err("not in the music folders".into());
                }
                if !is_audio(Path::new(p)) {
                    return Err("not a music file".into());
                }
                if let Some(b) = busy(p) {
                    return Err(b);
                }
                let (size, mtime) = self.index.copy_check(p, &gone_set)?;
                let m = std::fs::metadata(p).map_err(|e| e.to_string())?;
                let now = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
                if m.len() != size || now != Some(mtime) {
                    return Err("changed since the last scan".into());
                }
                del(Path::new(p))?;
                Ok(size)
            })();
            match why {
                Ok(size) => {
                    bytes += size;
                    gone_set.insert(p.clone());
                    gone.push(p.clone());
                }
                Err(why) => skipped.push(json!({ "path": p, "why": why })),
            }
        }
        // Folders left with no music (a cover, a .cue, a .log): only when asked.
        let mut dirs_gone = 0;
        if folders {
            let mut dirs: Vec<PathBuf> = gone.iter().filter_map(|p| Path::new(p).parent().map(Path::to_path_buf)).collect();
            dirs.sort();
            dirs.dedup();
            for d in dirs {
                let ds = d.display().to_string();
                if self.is_root(&d) || !self.allowed(&ds) || has_music(&d, 0) {
                    continue;
                }
                if del(&d).is_ok() {
                    dirs_gone += 1;
                }
            }
        }
        self.index.forget(&gone);
        json!({ "moved": gone.len(), "bytes": bytes, "folders": dirs_gone, "skipped": skipped, "paths": gone })
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn save(&self, roots: &[PathBuf]) {
        let Some(file) = &self.file else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(roots) {
            if let Err(e) = std::fs::write(file, bytes) {
                crate::aelog!("[LIBRARY] the music folders were not saved: {e}");
            }
        }
    }

    pub fn add_root(&self, path: &str) -> Result<(), String> {
        let p = PathBuf::from(path);
        if !p.is_dir() {
            return Err(format!("Not a folder: {path}"));
        }
        let mut roots = self.roots.lock().unwrap_or_else(|p| p.into_inner());
        if !roots.contains(&p) {
            roots.push(p);
            self.save(&roots);
            drop(roots);
            self.index.rescan(self.roots_fn());
        }
        Ok(())
    }

    pub fn remove_root(&self, path: &str) {
        let mut roots = self.roots.lock().unwrap_or_else(|p| p.into_inner());
        roots.retain(|r| r.as_path() != Path::new(path));
        self.save(&roots);
        drop(roots);
        self.index.rescan(self.roots_fn());
    }

    /// Is `path` inside one of the folders? Links are followed first, so one
    /// inside a folder pointing out of it does not count; a path that is not
    /// there counts as outside.
    pub fn allowed(&self, path: &str) -> bool {
        let Ok(real) = std::fs::canonicalize(path) else { return false };
        self.roots().iter().filter_map(|r| std::fs::canonicalize(r).ok()).any(|r| real.starts_with(r))
    }

    /// The folders and music files in `path` (inside the folders); with no
    /// `path`, the folders themselves.
    pub fn browse(&self, path: Option<&str>) -> Result<Value, String> {
        let Some(path) = path else {
            let dirs: Vec<Value> = self
                .roots()
                .iter()
                .map(|r| json!({ "name": display_name(r), "path": r.display().to_string() }))
                .collect();
            return Ok(json!({ "path": null, "parent": null, "dirs": dirs, "files": [] }));
        };
        if !self.allowed(path) {
            return Err("That folder is not in the library".into());
        }
        let (dirs, mut files) = list(Path::new(path), true)?;
        // A file with copies elsewhere in the library shows ×N.
        let paths: Vec<String> = files.iter().map(|f| f["path"].as_str().unwrap_or_default().to_string()).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        for (f, n) in files.iter_mut().zip(self.index.copy_counts(&refs)) {
            if n > 0 {
                f["copies"] = json!(n);
            }
        }
        // Up stops at the library's own folders.
        let p = Path::new(path);
        let parent = p.parent().filter(|_| !self.is_root(p)).map(|q| q.display().to_string());
        Ok(json!({ "path": path, "parent": parent, "atRoot": self.is_root(p), "dirs": dirs, "files": files }))
    }

    fn is_root(&self, p: &Path) -> bool {
        let real = std::fs::canonicalize(p).ok();
        self.roots().iter().any(|r| std::fs::canonicalize(r).ok() == real)
    }
}

/// Any music file (a converted one too) in `dir` or under it.
fn has_music(dir: &Path, depth: usize) -> bool {
    if depth > 24 {
        return true;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return true };
    rd.flatten().any(|e| {
        let p = e.path();
        if p.is_dir() { has_music(&p, depth + 1) } else { is_audio(&p) }
    })
}

/// Folder names only, anywhere on the disk, for choosing a music folder; with
/// no `path`, the drives.
pub fn pick(path: Option<&str>) -> Result<Value, String> {
    let Some(path) = path else {
        return Ok(json!({ "path": null, "parent": null, "dirs": drives(), "files": [] }));
    };
    let p = Path::new(path);
    let (dirs, _) = list(p, false)?;
    let parent = p.parent().map(|q| q.display().to_string());
    Ok(json!({ "path": path, "parent": parent, "dirs": dirs, "files": [] }))
}

fn display_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn list(dir: &Path, with_files: bool) -> Result<(Vec<Value>, Vec<Value>), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if is_hidden(&name) {
            continue;
        }
        let p = e.path();
        // Followed, so a linked folder is a folder.
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        if meta.is_dir() {
            dirs.push((name.to_lowercase(), json!({ "name": name, "path": p.display().to_string() })));
        } else if with_files && is_audio(&p) {
            files.push((name.to_lowercase(), json!({ "name": name, "path": p.display().to_string(), "size": meta.len() })));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((dirs.into_iter().map(|d| d.1).collect(), files.into_iter().map(|f| f.1).collect()))
}

#[cfg(windows)]
fn drives() -> Vec<Value> {
    // Not A: and B:, where asking can take seconds.
    (b'C'..=b'Z')
        .map(|l| format!("{}:\\", l as char))
        .filter(|d| Path::new(d).is_dir())
        .map(|d| json!({ "name": d, "path": d }))
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<Value> {
    let mut out = vec![json!({ "name": "/", "path": "/" })];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.insert(0, json!({ "name": display_name(&home), "path": home.display().to_string() }));
    }
    out
}

/// The user's own music folder, for a first run.
pub fn default_music_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("AURA_MUSIC_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join("Music"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(name: &str) -> Tree {
        let d = std::env::temp_dir().join(format!("aura-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("music").join("Album")).unwrap();
        std::fs::create_dir_all(d.join("music").join(".hidden")).unwrap();
        std::fs::create_dir_all(d.join("private")).unwrap();
        std::fs::write(d.join("music").join("Album").join("b.flac"), b"x").unwrap();
        std::fs::write(d.join("music").join("Album").join("A.DSF"), b"x").unwrap();
        std::fs::write(d.join("music").join("Album").join("cover.jpg"), b"x").unwrap();
        std::fs::write(d.join("private").join("secret.flac"), b"x").unwrap();
        Tree(d)
    }

    fn lib(t: &Tree) -> Arc<Library> {
        Library::open(None, Some(t.0.join("music")))
    }

    #[test]
    fn only_paths_inside_the_folders_count() {
        let t = tree("allowed");
        let l = lib(&t);
        let inside = t.0.join("music").join("Album").join("b.flac");
        let outside = t.0.join("private").join("secret.flac");
        let climbing = t.0.join("music").join("..").join("private").join("secret.flac");
        assert!(l.allowed(inside.to_str().unwrap()));
        assert!(!l.allowed(outside.to_str().unwrap()));
        assert!(!l.allowed(climbing.to_str().unwrap()), "a .. does not climb out");
        assert!(!l.allowed(t.0.join("music").join("nothing.flac").to_str().unwrap()));
        assert!(l.browse(Some(t.0.join("private").to_str().unwrap())).is_err());
    }

    #[test]
    fn a_folder_lists_its_folders_and_music_in_order_and_nothing_else() {
        let t = tree("list");
        let l = lib(&t);
        let v = l.browse(Some(t.0.join("music").to_str().unwrap())).unwrap();
        let names = |k: &str| v[k].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names("dirs"), ["Album"], "not the hidden one");
        assert!(v["parent"].is_null(), "up stops at the library's own folder");
        let v = l.browse(Some(t.0.join("music").join("Album").to_str().unwrap())).unwrap();
        let names = v["files"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(names, ["A.DSF", "b.flac"], "no cover.jpg");
        assert!(v["parent"].is_string());
    }

    #[test]
    fn the_picker_shows_folder_names_only() {
        let t = tree("pick");
        let v = pick(Some(t.0.join("private").to_str().unwrap())).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 0, "no file names outside the library");
    }

    #[test]
    fn folders_are_kept_and_can_be_removed() {
        let t = tree("roots");
        let f = t.0.join("state").join("roots.json");
        let l = Library::open(Some(f.clone()), Some(t.0.join("music")));
        l.add_root(t.0.join("private").to_str().unwrap()).unwrap();
        assert!(l.add_root(t.0.join("nothing").to_str().unwrap()).is_err());
        let again = Library::open(Some(f.clone()), None);
        assert_eq!(again.roots().len(), 2, "only what was asked is kept");
        again.remove_root(t.0.join("music").to_str().unwrap());
        assert_eq!(Library::open(Some(f), None).roots(), vec![t.0.join("private")]);
    }

    #[test]
    fn a_first_run_without_the_music_folder_has_no_folders() {
        let t = tree("first");
        assert!(Library::open(None, Some(t.0.join("nope"))).roots().is_empty());
    }

    /// E and C hold the same two tracks, C a cover besides; `private` is
    /// outside the library with a third copy.
    fn copies(name: &str) -> (Tree, Library) {
        let d = std::env::temp_dir().join(format!("aura-lib-trash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for sub in ["E/Album", "C/Album", "private", "E/Solo"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        for dir in ["E/Album", "C/Album", "private"] {
            index::tests::write_wav(&d.join(dir).join("01 One.wav"), 44_100, 16, 441);
            index::tests::write_wav(&d.join(dir).join("02 Two.wav"), 44_100, 16, 882);
        }
        index::tests::write_wav(&d.join("E/Solo/03 Alone.wav"), 44_100, 16, 300);
        std::fs::write(d.join("C/Album/cover.jpg"), b"x").unwrap();
        let roots = vec![d.join("E"), d.join("C")];
        let l = Library { roots: Arc::new(Mutex::new(roots.clone())), file: None, index: Index::open(None, None) };
        l.index.scan_now(&roots).unwrap();
        (Tree(d), l)
    }

    fn rm(p: &Path) -> Result<(), String> {
        if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }.map_err(|e| e.to_string())
    }

    #[test]
    fn trash_keeps_the_last_copy_and_takes_nothing_from_outside() {
        let (t, l) = copies("last");
        let s = |p: &str| t.0.join(p.replace('/', std::path::MAIN_SEPARATOR_STR)).display().to_string();
        let paths = vec![s("E/Album/01 One.wav"), s("C/Album/01 One.wav"), s("private/02 Two.wav"), s("E/Solo/03 Alone.wav")];
        let r = l.trash_with(&paths, false, &|_| None, &rm);
        assert_eq!(r["moved"], 1, "{r}");
        assert!(!t.0.join("E/Album/01 One.wav").exists());
        assert!(t.0.join("C/Album/01 One.wav").exists(), "the last copy stays");
        assert!(t.0.join("private/02 Two.wav").exists());
        let why: Vec<&str> = r["skipped"].as_array().unwrap().iter().map(|x| x["why"].as_str().unwrap()).collect();
        assert_eq!(why, ["the last copy stays", "not in the music folders", "it has no copy"]);
        assert_eq!(l.index.status()["tracks"], 4, "the index forgot the file at once");
    }

    #[test]
    fn trash_skips_what_plays_and_what_changed_and_can_take_an_empty_folder() {
        let (t, l) = copies("busy");
        let s = |p: &str| t.0.join(p.replace('/', std::path::MAIN_SEPARATOR_STR)).display().to_string();
        let playing = s("C/Album/01 One.wav");
        let r = l.trash_with(&[playing.clone()], false, &|p| (p == playing).then(|| "it plays now".to_string()), &rm);
        assert_eq!(r["moved"], 0);
        assert_eq!(r["skipped"][0]["why"], "it plays now");
        // Changed since the scan: refused.
        std::fs::write(t.0.join("C/Album/02 Two.wav"), b"other").unwrap();
        let r = l.trash_with(&[s("C/Album/02 Two.wav")], false, &|_| None, &rm);
        assert_eq!(r["skipped"][0]["why"], "changed since the last scan");
        index::tests::write_wav(&t.0.join("C/Album/02 Two.wav"), 44_100, 16, 882);
        l.index.scan_now(&l.roots()).unwrap();
        // Both C files go; C/Album keeps only a cover, so it goes too.
        let r = l.trash_with(&[s("C/Album/01 One.wav"), s("C/Album/02 Two.wav")], true, &|_| None, &rm);
        assert_eq!(r["moved"], 2, "{r}");
        assert_eq!(r["folders"], 1);
        assert!(!t.0.join("C/Album").exists());
        assert!(t.0.join("C").exists(), "a music folder itself stays");
        assert!(t.0.join("E/Album/01 One.wav").exists());
    }

    #[test]
    fn the_duplicates_page_answers_through_the_library_command() {
        let (t, l) = copies("cmd");
        let v = l.command("lib_dupes", Value::Null).unwrap();
        assert_eq!(v["groups"].as_array().unwrap().len(), 2);
        assert!(l.command("lib_dupes", json!({ "scope": t.0.join("private").display().to_string() })).is_err());
        assert_eq!(l.command("lib_genres", Value::Null).unwrap(), json!([]));
    }

    #[test]
    fn a_folder_marks_its_files_with_copies() {
        let (t, l) = copies("browse");
        let v = l.browse(Some(t.0.join("C").join("Album").to_str().unwrap())).unwrap();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f["copies"] == 1), "{files:?}");
        let v = l.browse(Some(t.0.join("E").join("Solo").to_str().unwrap())).unwrap();
        assert!(v["files"][0].get("copies").is_none(), "a file of its own has none");
    }
}
