//! What the pages keep: the rack, the lists, the volume. In the desktop app that
//! is the webview's `localStorage`; here it is on the server, so a phone and a
//! tablet see one state and a new device does not come in as a first run and
//! reset the rack. The bridge (`web/remote/shim.js`) mirrors it into the
//! browser's `localStorage`, and the pages read and write that as they always did.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_KEY: usize = 256;
const MAX_VALUE: usize = 8 << 20;
const MAX_TOTAL: usize = 64 << 20;

pub struct Kv {
    map: Mutex<HashMap<String, String>>,
    file: Option<PathBuf>,
    dirty: AtomicBool,
}

impl Kv {
    /// What was kept in `file`; nothing kept is an empty store (a first run).
    pub fn open(file: Option<PathBuf>) -> Arc<Kv> {
        let map = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Kv { map: Mutex::new(map), file, dirty: AtomicBool::new(false) })
    }

    pub fn all(&self) -> HashMap<String, String> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Set `key`, or remove it with None. An oversized write is refused whole.
    pub fn set(&self, key: String, value: Option<String>) -> Result<(), String> {
        if key.is_empty() || key.len() > MAX_KEY {
            return Err("bad key".into());
        }
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        match value {
            Some(v) => {
                if v.len() > MAX_VALUE {
                    return Err("too large".into());
                }
                let others: usize =
                    map.iter().filter(|(k, _)| **k != key).map(|(k, v)| k.len() + v.len()).sum();
                if others + key.len() + v.len() > MAX_TOTAL {
                    return Err("too large".into());
                }
                map.insert(key, v);
            }
            None => {
                map.remove(&key);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Onto the disk if anything changed: whole or not at all.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(file) = &self.file else { return };
        let bytes = {
            let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
            serde_json::to_vec(&*map).unwrap_or_default()
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension("tmp");
        let done = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, file));
        if let Err(e) = done {
            aura_core::aelog!("[STREAMER] the pages' state was not saved: {e}");
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Saves every two seconds if anything changed.
    pub fn keep_saved(self: &Arc<Self>) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("aura-kv".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            me.flush();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aura-kv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("pages.json")
    }

    #[test]
    fn what_is_set_comes_back_after_a_restart_and_a_removed_key_does_not() {
        let f = temp("restart");
        let kv = Kv::open(Some(f.clone()));
        kv.set("a".into(), Some("1".into())).unwrap();
        kv.set("b".into(), Some("2".into())).unwrap();
        kv.set("b".into(), None).unwrap();
        kv.flush();
        let again = Kv::open(Some(f.clone()));
        assert_eq!(again.all().get("a").map(String::as_str), Some("1"));
        assert!(!again.all().contains_key("b"));
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }

    #[test]
    fn a_broken_file_is_an_empty_store_not_a_failure() {
        let f = temp("broken");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"{not json").unwrap();
        assert!(Kv::open(Some(f.clone())).all().is_empty());
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }

    #[test]
    fn an_oversized_write_is_refused() {
        let kv = Kv::open(None);
        assert!(kv.set("".into(), Some("x".into())).is_err());
        assert!(kv.set("k".repeat(MAX_KEY + 1), Some("x".into())).is_err());
        assert!(kv.set("k".into(), Some("x".repeat(MAX_VALUE + 1))).is_err());
        for i in 0..7 {
            kv.set(format!("k{i}"), Some("x".repeat(MAX_VALUE))).unwrap();
        }
        assert!(kv.set("k7".into(), Some("x".repeat(MAX_VALUE))).is_err(), "over the total");
        kv.set("k0".into(), Some("y".repeat(MAX_VALUE))).expect("a key's old value does not count");
    }
}
