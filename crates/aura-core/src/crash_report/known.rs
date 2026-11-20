//! The private values this session has met: tracks' paths and tags, stations'
//! addresses. The log prints some of them bare (a track's title has no shape
//! a pattern could know), so the redactor has to be told them by value.
//!
//! A session that crashed left no memory of them, so they are also written
//! down beside its log (`<log>.known`, on this machine only, next to a log
//! that holds the same text). A clean exit removes that file.
//!
//! Nothing here logs: `remember` is called from the player's own paths.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Path,
    Title,
    Tag,
    Url,
    /// An audio device's name or id: it may no longer be in the list when the report is made.
    Device,
}

impl Kind {
    fn letter(self) -> char {
        match self {
            Kind::Path => 'p',
            Kind::Title => 't',
            Kind::Tag => 'g',
            Kind::Url => 'u',
            Kind::Device => 'd',
        }
    }

    fn from_letter(c: char) -> Option<Kind> {
        match c {
            'p' => Some(Kind::Path),
            't' => Some(Kind::Title),
            'g' => Some(Kind::Tag),
            'u' => Some(Kind::Url),
            'd' => Some(Kind::Device),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Store {
    values: Vec<(Kind, String)>,
    seen: std::collections::HashSet<(Kind, String)>,
    sidecar: Option<(std::fs::File, PathBuf)>,
    /// Written since the last `sync`.
    unsynced: bool,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// One value, written as `<kind letter><value>`, on one line.
fn encode(kind: Kind, value: &str) -> String {
    format!("{}{}\n", kind.letter(), value.replace(['\r', '\n'], " "))
}

fn decode(text: &str) -> Vec<(Kind, String)> {
    text.lines()
        .filter_map(|l| {
            let mut c = l.chars();
            let k = Kind::from_letter(c.next()?)?;
            Some((k, c.as_str().to_string()))
        })
        .collect()
}

/// The file beside a session's log.
pub fn sidecar_of(log: &Path) -> PathBuf {
    let mut s = log.as_os_str().to_os_string();
    s.push(".known");
    PathBuf::from(s)
}

/// From now on, what is remembered is also written beside `log`.
pub fn open_sidecar(log: &Path) {
    let path = sidecar_of(log);
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    STORE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Store::default).sidecar = Some((file, path));
}

/// The session ended cleanly: its values are no longer needed on disk.
pub fn close_sidecar() {
    let taken = STORE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|s| s.sidecar.take());
    if let Some((file, path)) = taken {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
}

/// What was written beside the log, to the disk with the log (the watchdog's
/// tick). Never waits: that thread must go on when everything else is stuck.
pub fn sync() {
    let Ok(mut guard) = STORE.try_lock() else { return };
    let Some(store) = guard.as_mut().filter(|s| s.unsynced) else { return };
    if let Some((file, _)) = store.sidecar.as_ref() {
        let _ = file.sync_data();
    }
    store.unsynced = false;
}

pub fn remember(kind: Kind, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let store = guard.get_or_insert_with(Store::default);
    if !store.seen.insert((kind, value.to_string())) {
        return;
    }
    store.values.push((kind, value.to_string()));
    if let Some((file, _)) = store.sidecar.as_mut() {
        let _ = file.write_all(encode(kind, value).as_bytes());
        store.unsynced = true;
    }
}

/// A track added to the list: its path and tags.
pub fn remember_track(t: &crate::player::probe::TrackInfo) {
    remember(Kind::Path, &t.path);
    remember(Kind::Title, &t.title);
    remember(Kind::Tag, &t.artist);
    remember(Kind::Tag, &t.album);
}

/// A song a station says it plays, as a whole and as `Artist - Title`.
pub fn remember_song(text: &str, album: Option<&str>) {
    remember(Kind::Title, text);
    if let Some((artist, title)) = text.split_once(" - ") {
        remember(Kind::Tag, artist);
        remember(Kind::Title, title);
    }
    if let Some(album) = album {
        remember(Kind::Tag, album);
    }
}

/// What this session has met so far.
pub fn current() -> Vec<(Kind, String)> {
    STORE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.values.clone()).unwrap_or_default()
}

/// What the session that wrote `log` had met.
pub fn of_session(log: &Path) -> Vec<(Kind, String)> {
    std::fs::read(sidecar_of(log)).map(|b| decode(&String::from_utf8_lossy(&b))).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_come_back_as_written_one_per_line() {
        let text = encode(Kind::Path, "C:\\Музыка\\a b.flac") + &encode(Kind::Title, "two\nlines") + &encode(Kind::Url, "http://x/y");
        assert_eq!(
            decode(&text),
            vec![
                (Kind::Path, "C:\\Музыка\\a b.flac".to_string()),
                (Kind::Title, "two lines".to_string()),
                (Kind::Url, "http://x/y".to_string()),
            ]
        );
    }

    #[test]
    fn a_line_of_unknown_kind_is_skipped() {
        assert_eq!(decode("zzz\n\ngTag\n"), vec![(Kind::Tag, "Tag".to_string())]);
    }
}
