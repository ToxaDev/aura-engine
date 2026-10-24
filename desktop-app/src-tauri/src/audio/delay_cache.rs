//! The band-weighted group delay of a filter file, remembered.
//!
//! `dsp_core::estimate_band_weighted_group_delay` unwraps the head's phase one
//! turn at a time against the running unwrapped phase, and a minimum-phase
//! head's phase runs far: the 30M minimum-phase blob takes 0.5 s at 176.4k
//! and 3 s at 705.6k — a Hybrid-Phase rack's first start waited for it. The
//! delay is one integer, fixed by the file and the rate, so it is computed
//! once and kept in `player-cache\filter-delays.json`, keyed by the file's
//! path, size and modification time, the rate and the band. A file that
//! changed misses; a cache file that cannot be read is computed past and
//! written again. A path that is not a file (a test's filter) is computed
//! every time, as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A delay cache: in memory, and in `file` when there is one.
pub struct DelayCache {
    file: Option<PathBuf>,
    map: Mutex<Option<HashMap<String, usize>>>,
}

impl DelayCache {
    pub fn new(file: Option<PathBuf>) -> Self {
        DelayCache { file, map: Mutex::new(None) }
    }

    /// The delay of the filter at `path` at `out_rate` in the band, from the
    /// cache, else `compute()`d and remembered.
    pub fn get_or_compute(&self, path: &str, out_rate: u32, band: (f64, f64), compute: impl FnOnce() -> usize) -> usize {
        let Some(key) = key(Path::new(path), out_rate, band) else {
            return compute();
        };
        if let Some(d) = self.loaded().as_ref().and_then(|m| m.get(&key).copied()) {
            return d;
        }
        let d = compute();
        let mut g = self.loaded();
        let m = g.get_or_insert_with(HashMap::new);
        m.insert(key, d);
        if let Some(f) = &self.file {
            let _ = write_atomic(f, m);
        }
        d
    }

    /// The map, read from the file on first use (missing or unreadable: empty).
    fn loaded(&self) -> std::sync::MutexGuard<'_, Option<HashMap<String, usize>>> {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let m = self
                .file
                .as_ref()
                .and_then(|f| std::fs::read(f).ok())
                .and_then(|b| serde_json::from_slice::<HashMap<String, usize>>(&b).ok())
                .unwrap_or_default();
            *g = Some(m);
        }
        g
    }
}

/// The key: the file as it is now (path, size, modification time), the rate
/// and the band. None when the file cannot be looked at.
fn key(path: &Path, out_rate: u32, band: (f64, f64)) -> Option<String> {
    let md = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(format!("{}|{}|{}|{}|{}-{}", path.display(), md.len(), mtime, out_rate, band.0, band.1))
}

fn write_atomic(file: &Path, m: &HashMap<String, usize>) -> std::io::Result<()> {
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(m).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, file)
}

/// The app's cache: beside the Hybrid-Phase envelopes. Unit tests keep it in
/// memory only and never touch the user's folder.
pub fn global() -> &'static DelayCache {
    static CACHE: OnceLock<DelayCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let file = if cfg!(test) {
            None
        } else {
            Some(
                crate::app_dir::root()
                    .map(|d| d.join("player-cache"))
                    .unwrap_or_else(|| std::env::temp_dir().join("AuraEngine").join("player-cache"))
                    .join("filter-delays.json"),
            )
        };
        DelayCache::new(file)
    })
}

/// `estimate_band_weighted_group_delay` (200 Hz – 6 kHz) of the filter file
/// at `path`, whose head `coeffs` gives it, at `out_rate`: remembered.
pub fn band_weighted_delay(path: &str, coeffs: &[f64], out_rate: u32) -> usize {
    global().get_or_compute(path, out_rate, (200.0, 6000.0), || {
        crate::audio::dsp_core::estimate_band_weighted_group_delay(coeffs, out_rate as f64, 200.0, 6000.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp_core::estimate_band_weighted_group_delay;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aura-delay-cache-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A minimum-phase-like head: a decaying ring.
    fn head(n: usize, decay: f64) -> Vec<f64> {
        (0..n).map(|i| (i as f64 * 0.37).sin() * (-(i as f64) / decay).exp()).collect()
    }

    #[test]
    fn cached_delay_is_the_computed_one_and_survives_a_restart() {
        let d = tmp_dir("same");
        let filt = d.join("f.npy");
        std::fs::write(&filt, b"blob v1").unwrap();
        let h = head(4096, 300.0);
        let want = estimate_band_weighted_group_delay(&h, 176_400.0, 200.0, 6000.0);
        let file = d.join("filter-delays.json");
        let c = DelayCache::new(Some(file.clone()));
        let p = filt.to_str().unwrap();
        let calc = || estimate_band_weighted_group_delay(&h, 176_400.0, 200.0, 6000.0);
        assert_eq!(c.get_or_compute(p, 176_400, (200.0, 6000.0), calc), want);
        // Remembered: not computed again, in this cache or a new one from the file.
        assert_eq!(c.get_or_compute(p, 176_400, (200.0, 6000.0), || panic!("computed again")), want);
        let c2 = DelayCache::new(Some(file));
        assert_eq!(c2.get_or_compute(p, 176_400, (200.0, 6000.0), || panic!("computed again")), want);
        // Another rate is another delay.
        let mut ran = false;
        c2.get_or_compute(p, 705_600, (200.0, 6000.0), || {
            ran = true;
            0
        });
        assert!(ran);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_blob_misses() {
        let d = tmp_dir("changed");
        let filt = d.join("f.npy");
        std::fs::write(&filt, b"blob v1").unwrap();
        let c = DelayCache::new(Some(d.join("filter-delays.json")));
        let p = filt.to_str().unwrap();
        assert_eq!(c.get_or_compute(p, 96_000, (200.0, 6000.0), || 11), 11);
        std::fs::write(&filt, b"blob v2, longer").unwrap();
        assert_eq!(c.get_or_compute(p, 96_000, (200.0, 6000.0), || 22), 22);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_corrupt_cache_file_is_computed_past_and_written_again() {
        let d = tmp_dir("corrupt");
        let filt = d.join("f.npy");
        std::fs::write(&filt, b"blob").unwrap();
        let file = d.join("filter-delays.json");
        std::fs::write(&file, b"{ not json").unwrap();
        let c = DelayCache::new(Some(file.clone()));
        assert_eq!(c.get_or_compute(filt.to_str().unwrap(), 96_000, (200.0, 6000.0), || 7), 7);
        let back: HashMap<String, usize> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(back.values().copied().collect::<Vec<_>>(), vec![7]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_path_that_is_no_file_is_computed_every_time() {
        let c = DelayCache::new(None);
        let mut n = 0;
        for _ in 0..2 {
            c.get_or_compute("no such filter", 96_000, (200.0, 6000.0), || {
                n += 1;
                5
            });
        }
        assert_eq!(n, 2);
    }
}
