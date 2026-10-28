//! The radio panel's own records — favorites, recent stations, the stations
//! that failed on their format — in the app's data folder
//! (`radio\stations.json`), so they outlive the WebView's storage. The page
//! owns their shape; this keeps them whole: each write is a new file renamed
//! over the old one.

use std::path::Path;

/// A record larger than this is not the panel's.
const MAX_BYTES: usize = 512 * 1024;
const FILE: &str = "stations.json";

/// Write `bytes` to `path` whole or not at all: into a file beside it, then
/// renamed over it (the folder made first).
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
}

/// The panel's records kept in `dir`, or None (none kept, or not readable
/// as the panel's).
pub(super) fn load_from(dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(dir.join(FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.is_object().then_some(v)
}

/// Keep the panel's records `json` in `dir`: a JSON object, not too large.
pub(super) fn save_to(dir: &Path, json: &str) -> Result<(), String> {
    if json.len() > MAX_BYTES {
        return Err(format!("the radio's records are {} bytes, more than {}", json.len(), MAX_BYTES));
    }
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("the radio's records: {}", e))?;
    if !v.is_object() {
        return Err("the radio's records are not an object".into());
    }
    write_atomic(&dir.join(FILE), json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_records_are_kept_whole() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("radio");
        assert_eq!(load_from(&dir), None, "nothing kept yet");
        let rec = r#"{"v":1,"favorites":[{"name":"RP","url":"https://stream.radioparadise.com/flac"}],"recent":[],"failed":{"9617a958-0601-11e8-ae97-52543be04c81":1}}"#;
        save_to(&dir, rec).unwrap();
        assert_eq!(load_from(&dir).unwrap()["favorites"][0]["name"], "RP");
        assert!(!dir.join("stations.tmp").exists(), "the file beside it is renamed away");
        // A write that is not the panel's leaves the kept one as it was.
        assert!(save_to(&dir, "[1,2]").is_err());
        assert!(save_to(&dir, "{broken").is_err());
        assert!(save_to(&dir, &format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_BYTES))).is_err());
        assert_eq!(load_from(&dir).unwrap()["v"], 1);
        // A newer write replaces it.
        save_to(&dir, r#"{"v":1,"favorites":[]}"#).unwrap();
        assert_eq!(load_from(&dir).unwrap()["favorites"], serde_json::json!([]));
        // A damaged file reads as none.
        std::fs::write(dir.join(FILE), b"{\"v\":1,").unwrap();
        assert_eq!(load_from(&dir), None);
    }
}
