//! The media library in the window (the M view and the landscape mode): the
//! same library the streamer serves (aura_core::library), kept in the app's
//! data folder (`library/`: the music folders, the index, the small covers).
//!
//! One command, `lib_call`, takes the library's own command names, as the
//! streamer's remote sends them: `lib_status`, `lib_rescan`, `lib_albums`,
//! `lib_album`, `lib_artists`, `lib_genres`, `lib_search`, and the folders'
//! `lib_roots`, `lib_add_root`, `lib_remove_root`, `lib_browse`, `lib_pick`.
//! Covers come through the `aura` protocol (`/lib/cover/<id>`).

use aura_core::library::{self, Library};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

static LIB: OnceLock<Arc<Library>> = OnceLock::new();

/// The library, opened at its first use. A first run reads the user's own
/// music folder, if there is one.
pub fn lib() -> &'static Arc<Library> {
    LIB.get_or_init(|| {
        let dir = crate::app_dir::root().map(|d| d.join("library"));
        Library::open(dir.map(|d| d.join("music-folders.json")), library::default_music_dir())
    })
}

/// A library command: what the streamer's `lib_*` commands answer.
pub fn call(lib: &Library, cmd: &str, args: Value) -> Result<Value, String> {
    let path = |args: &Value| args.get("path").and_then(Value::as_str).map(str::to_string);
    match cmd {
        "lib_roots" => Ok(json!(lib.roots())),
        "lib_add_root" => {
            let p = path(&args).ok_or("no path")?;
            lib.add_root(&p).map(|_| Value::Null)
        }
        "lib_remove_root" => {
            let p = path(&args).ok_or("no path")?;
            lib.remove_root(&p);
            Ok(Value::Null)
        }
        "lib_browse" => lib.browse(path(&args).as_deref()),
        "lib_pick" => library::pick(path(&args).as_deref()),
        c if c.starts_with("lib_") => lib.command(c, args),
        _ => Err(format!("{cmd} is not a library command")),
    }
}

/// Off the window's thread: a first status reads the index from the disk.
#[tauri::command(async)]
pub fn lib_call(cmd: String, args: Option<Value>) -> Result<Value, String> {
    call(lib(), &cmd, args.unwrap_or(Value::Null))
}

/// An album's cover, small, as JPEG (`/lib/cover/<id>` in the `aura` protocol).
pub fn cover(id: &str) -> Option<Vec<u8>> {
    lib().index.cover(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folders_and_the_library_answer_through_one_command() {
        let d = std::env::temp_dir().join(format!("aura-libcmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("music").join("A")).unwrap();
        let l = Library::open(Some(d.join("state").join("roots.json")), None);
        assert_eq!(call(&l, "lib_roots", Value::Null).unwrap(), json!([]));
        let m = d.join("music").display().to_string();
        call(&l, "lib_add_root", json!({ "path": m })).unwrap();
        assert_eq!(call(&l, "lib_roots", Value::Null).unwrap(), json!([m]));
        let b = call(&l, "lib_browse", json!({ "path": m })).unwrap();
        assert_eq!(b["dirs"][0]["name"], "A");
        assert!(call(&l, "lib_status", Value::Null).unwrap().is_object());
        assert!(call(&l, "lib_add_root", json!({})).is_err());
        assert!(call(&l, "player_play", Value::Null).is_err());
        call(&l, "lib_remove_root", json!({ "path": m })).unwrap();
        assert_eq!(call(&l, "lib_roots", Value::Null).unwrap(), json!([]));
        let _ = std::fs::remove_dir_all(&d);
    }
}
