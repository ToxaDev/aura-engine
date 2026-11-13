//! The folders of music the remote may see. The player takes file paths, and
//! anyone on the network can reach this server, so a path from a request counts
//! only inside these folders: nothing else on the disk can be listed, probed or
//! played through it. Choosing the folders needs a look at the disk, so that one
//! view lists folder names and nothing else.

use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "library_index.rs"]
pub mod index;
#[path = "library_tags.rs"]
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
        let index = Index::open(dir.as_ref().map(|d| d.join("library-index.json")), dir.map(|d| d.join("covers")));
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
            serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Q {
            q: Option<String>,
            id: String,
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
            "lib_artists" => Ok(self.index.artists(arg::<Q>(args)?.q.as_deref())),
            "lib_genres" => Ok(self.index.genres()),
            "lib_search" => Ok(self.index.search(arg::<Q>(args)?.q.as_deref().unwrap_or(""))),
            _ => Err(format!("{cmd} is not available on the streamer")),
        }
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
                aura_core::aelog!("[STREAMER] the music folders were not saved: {e}");
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
        let (dirs, files) = list(Path::new(path), true)?;
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
}
