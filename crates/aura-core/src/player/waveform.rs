//! A track's amplitude for the foot's seek bar (the Library look and the
//! streamer's wide look): the file read once from start to end, below normal
//! priority, into `BINS` slices of its length, each slice's peak and RMS.
//!
//! Kept twice: the last few in memory, and every one on disk as a small file
//! in `player-cache\waveform` keyed by the file as it is (path, size,
//! modification time) and `VERSION` — the next play of the track draws at
//! once. A new ask for another file stops the one being read (the listener
//! moved on); an ask for the file already being read waits for it.
//!
//! A DSD file is decimated here as it is read (`DsdDecimator`), one channel a
//! thread, never whole in memory; any other file goes through the engine's
//! own decode in pieces (`decode::decode_in_pieces`), and only the files that
//! cannot be decoded so (HE-AAC, a chained Ogg) are decoded whole.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use serde::Serialize;

/// Slices of a track: the bar is at most ~1000 px wide in a big window, and
/// the page takes the loudest slice of those under each of its bars.
pub const BINS: usize = 1000;
/// Bump on any change to what a slice holds: the files made before miss.
pub const VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"AURA-WF1";
/// Tracks kept in memory.
const KEEP: usize = 24;
/// Files kept on disk: past it the oldest fifth goes (a file is ~2 KB).
const MAX_FILES: usize = 20_000;
/// The level a slice's byte starts from: 0 is this or quieter, 255 is 0 dBFS
/// or louder (a DSD file's peaks may pass full scale).
const FLOOR_DB: f64 = -60.0;

/// What the page draws: per slice, the peak of either channel and the RMS of
/// both, as `level` bytes (0 = −60 dBFS or quieter, 255 = 0 dBFS), and the
/// length the slices cover.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waveform {
    pub peak: Vec<u8>,
    pub rms: Vec<u8>,
    pub duration_s: f64,
}

/// A linear level as the byte a slice holds.
fn level(x: f64) -> u8 {
    if x <= 0.0 {
        return 0;
    }
    let db = 20.0 * x.log10();
    (((db - FLOOR_DB) / -FLOOR_DB) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// The slices, filled as the samples come.
struct Acc {
    frames: u64,
    i: u64,
    peak: Vec<f64>,
    sq: Vec<f64>,
    n: Vec<u32>,
}

impl Acc {
    fn new(frames: u64) -> Acc {
        Acc { frames: frames.max(1), i: 0, peak: vec![0.0; BINS], sq: vec![0.0; BINS], n: vec![0; BINS] }
    }

    fn push(&mut self, l: &[f64], r: &[f64]) {
        for (a, b) in l.iter().zip(r) {
            if self.i >= self.frames {
                return;
            }
            let k = ((self.i as u128 * BINS as u128) / self.frames as u128) as usize;
            let p = a.abs().max(b.abs());
            if p > self.peak[k] {
                self.peak[k] = p;
            }
            self.sq[k] += 0.5 * (a * a + b * b);
            self.n[k] += 1;
            self.i += 1;
        }
    }

    fn finish(self, rate: u32) -> Waveform {
        let rms = self.sq.iter().zip(&self.n).map(|(s, &n)| if n > 0 { level((s / n as f64).sqrt()) } else { 0 }).collect();
        Waveform {
            peak: self.peak.iter().map(|&p| level(p)).collect(),
            rms,
            duration_s: self.i as f64 / rate.max(1) as f64,
        }
    }
}

/// The key of the file asked for last: a reading for another stops.
static WANT: AtomicU64 = AtomicU64::new(0);

fn hash(key: &str) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish() | 1
}

const STALE: &str = "another track was asked for";

/// What is held and what is being read now.
#[derive(Default)]
struct Held {
    kept: Vec<(String, Waveform)>,
    reading: Vec<String>,
}

fn held() -> &'static (Mutex<Held>, Condvar) {
    static H: OnceLock<(Mutex<Held>, Condvar)> = OnceLock::new();
    H.get_or_init(Default::default)
}

/// The amplitude of `path`: from memory, from disk, or read now (seconds for
/// a long DSD file). Err for a file that cannot be read, a stream, or when
/// another file was asked for meanwhile.
pub fn get(path: &str) -> Result<Waveform, String> {
    let id = super::chain::source_id(Path::new(path));
    if id.is_empty() {
        return Err(format!("Cannot open {path}"));
    }
    let key = format!("{id}|v{VERSION}");
    let me = hash(&key);
    WANT.store(me, Ordering::Relaxed);
    let (lock, cv) = held();
    {
        let mut h = lock.lock().unwrap();
        loop {
            if let Some(i) = h.kept.iter().position(|(k, _)| *k == key) {
                let e = h.kept.remove(i);
                let w = e.1.clone();
                h.kept.push(e);
                return Ok(w);
            }
            if !h.reading.contains(&key) {
                break;
            }
            h = cv.wait(h).unwrap();
            if WANT.load(Ordering::Relaxed) != me {
                return Err(STALE.into());
            }
        }
        h.reading.push(key.clone());
    }
    let got = load(&key).map(Ok).unwrap_or_else(|| {
        let t = std::time::Instant::now();
        let r = read(Path::new(path), &|| WANT.load(Ordering::Relaxed) != me);
        if let Ok(w) = &r {
            crate::aelog!("[WAVE] {} s of {} read in {:.2} s", w.duration_s.round(), path, t.elapsed().as_secs_f64());
            save(&key, w);
        }
        r
    });
    let mut h = lock.lock().unwrap();
    h.reading.retain(|k| *k != key);
    if let Ok(w) = &got {
        h.kept.push((key, w.clone()));
        if h.kept.len() > KEEP {
            h.kept.remove(0);
        }
    }
    cv.notify_all();
    got
}

/// Reads the whole file below normal priority; `stale` stops it.
fn read(path: &Path, stale: &(dyn Fn() -> bool + Sync)) -> Result<Waveform, String> {
    let was = crate::audio::os_thread::priority();
    crate::audio::os_thread::set_priority(BELOW_NORMAL);
    let r = crate::audio::logging::quietly(|| {
        if crate::audio::dsd::dsf::sniff(path).is_some() {
            read_dsd(path, stale)
        } else {
            read_pcm(path, stale)
        }
    });
    crate::audio::os_thread::set_priority(was);
    r
}

/// THREAD_PRIORITY_BELOW_NORMAL.
const BELOW_NORMAL: i32 = -1;

fn read_pcm(path: &Path, stale: &dyn Fn() -> bool) -> Result<Waveform, String> {
    use crate::audio::converter::decode;
    if let Some((frames, rate, _)) = decode::probe_input_frames(path) {
        let plain = std::cell::Cell::new(true);
        let mut acc = Acc::new(frames);
        let mut gone = false;
        let r = decode::decode_in_pieces(path, frames as usize, &mut |ok| plain.set(ok), &mut |l, r| {
            if !plain.get() {
                return false;
            }
            if stale() {
                gone = true;
                return false;
            }
            acc.push(l, r);
            true
        });
        if gone {
            return Err(STALE.into());
        }
        if plain.get() && acc.i > 0 {
            // A count off by a few frames from the header's still draws the
            // same track.
            if let Err(e) = r {
                crate::aelog!("[WAVE] {}: {e}; drawn from what was read", path.display());
            }
            return Ok(acc.finish(rate));
        }
        if plain.get() {
            r?;
        }
    }
    if stale() {
        return Err(STALE.into());
    }
    // Not decodable in pieces: whole.
    let f = decode::decode_file(path)?;
    let mut acc = Acc::new(f.samples_l.len() as u64);
    acc.push(&f.samples_l, &f.samples_r);
    Ok(acc.finish(f.sample_rate))
}

fn read_dsd(path: &Path, stale: &(dyn Fn() -> bool + Sync)) -> Result<Waveform, String> {
    use crate::audio::dsd::decimate::DsdDecimator;
    let mut reader = crate::audio::dsd::dsf::open(path)?;
    let info = reader.info().clone();
    let ch = info.channels.clamp(1, 2);
    let mut decs: Vec<DsdDecimator> = (0..ch).map(|_| DsdDecimator::new(info.rate)).collect();
    let rate = decs[0].out_rate();
    let mut acc = Acc::new(decs[0].output_len(info.samples) as u64);
    const CHUNK: usize = 1 << 18;
    let mut bufs = vec![vec![0u8; CHUNK]; ch];
    let mut outs: Vec<Vec<f64>> = vec![Vec::new(); ch];
    let mut end = false;
    while !end {
        if stale() {
            return Err(STALE.into());
        }
        let n = {
            let mut views: Vec<&mut [u8]> = bufs.iter_mut().map(|v| &mut v[..]).collect();
            reader.read(&mut views)?
        };
        end = n == 0;
        std::thread::scope(|s| {
            for ((d, o), b) in decs.iter_mut().zip(outs.iter_mut()).zip(&bufs) {
                s.spawn(move || {
                    crate::audio::os_thread::set_priority(BELOW_NORMAL);
                    o.clear();
                    if end {
                        d.flush(o);
                    } else {
                        d.process(&b[..n], o);
                    }
                });
            }
        });
        let (l, r) = (&outs[0], &outs[ch - 1]);
        let m = l.len().min(r.len());
        acc.push(&l[..m], &r[..m]);
    }
    Ok(acc.finish(rate))
}

// ── the files on disk ────────────────────────────────────────────────────

fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_DIR.with(|d| d.borrow().clone())
    }
    #[cfg(not(test))]
    {
        Some(
            crate::app_dir::root()
                .map(|d| d.join("player-cache"))
                .unwrap_or_else(|| std::env::temp_dir().join("AuraEngine").join("player-cache"))
                .join("waveform"),
        )
    }
}

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn file_of(key: &str) -> Option<PathBuf> {
    dir().map(|d| d.join(format!("{:016x}.wave", hash(key))))
}

fn encode(key: &str, w: &Waveform) -> Vec<u8> {
    let mut b = Vec::with_capacity(32 + key.len() + 2 * BINS);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&(key.len() as u32).to_le_bytes());
    b.extend_from_slice(key.as_bytes());
    b.extend_from_slice(&w.duration_s.to_le_bytes());
    b.extend_from_slice(&(w.peak.len() as u32).to_le_bytes());
    b.extend_from_slice(&w.peak);
    b.extend_from_slice(&w.rms);
    b
}

fn parse(key: &str, b: &[u8]) -> Option<Waveform> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(at..at + n)?;
        at += n;
        Some(s)
    };
    if take(8)? != MAGIC {
        return None;
    }
    let kl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    if take(kl)? != key.as_bytes() {
        return None;
    }
    let duration_s = f64::from_le_bytes(take(8)?.try_into().ok()?);
    let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    if n != BINS || !duration_s.is_finite() {
        return None;
    }
    let peak = take(n)?.to_vec();
    let rms = take(n)?.to_vec();
    Some(Waveform { peak, rms, duration_s })
}

fn load(key: &str) -> Option<Waveform> {
    parse(key, &std::fs::read(file_of(key)?).ok()?)
}

/// Best effort: a cache that cannot be written only reads the track again.
fn save(key: &str, w: &Waveform) {
    let (Some(d), Some(f)) = (dir(), file_of(key)) else { return };
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, encode(key, w)).is_ok() && std::fs::rename(&tmp, &f).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    trim(&d);
}

fn trim(d: &Path) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "wave"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= MAX_FILES {
        return;
    }
    files.sort();
    for (_, p) in files.iter().take(files.len() / 5) {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_bytes() {
        assert_eq!(level(0.0), 0);
        assert_eq!(level(1.0), 255);
        assert_eq!(level(2.0), 255);
        assert_eq!(level(0.001), 0); // −60 dB
        assert!((127..=128).contains(&level(10f64.powf(-30.0 / 20.0))));
    }

    #[test]
    fn slices_cover_the_track_in_order() {
        // 10 000 frames: the first half quiet, the second loud.
        let n = 10_000;
        let l: Vec<f64> = (0..n).map(|i| if i < n / 2 { 0.01 } else { 0.5 }).collect();
        let mut acc = Acc::new(n as u64);
        for (a, b) in l.chunks(777).zip(l.chunks(777)) {
            acc.push(a, b);
        }
        let w = acc.finish(1000);
        assert_eq!(w.peak.len(), BINS);
        assert!((w.duration_s - 10.0).abs() < 1e-9);
        assert_eq!(w.peak[0], level(0.01));
        assert_eq!(w.peak[BINS - 1], level(0.5));
        assert_eq!(w.rms[BINS / 2 - 1], level(0.01));
        assert_eq!(w.rms[BINS / 2], level(0.5));
        assert!(acc_frames_past_the_end_are_dropped());
    }

    fn acc_frames_past_the_end_are_dropped() -> bool {
        let mut acc = Acc::new(10);
        acc.push(&[1.0; 20], &[1.0; 20]);
        acc.i == 10
    }

    #[test]
    fn a_file_round_trips_and_a_changed_key_misses() {
        let w = Waveform { peak: (0..BINS).map(|i| i as u8).collect(), rms: vec![7; BINS], duration_s: 123.5 };
        let b = encode("a|1|2|v1", &w);
        assert_eq!(parse("a|1|2|v1", &b), Some(w));
        assert_eq!(parse("a|1|3|v1", &b), None);
        assert_eq!(parse("a|1|2|v1", &b[..b.len() - 1]), None);
    }

    #[test]
    fn a_wav_file_is_read_and_kept() {
        let d = tempfile::tempdir().unwrap();
        TEST_DIR.with(|t| *t.borrow_mut() = Some(d.path().join("cache")));
        let path = d.path().join("ramp.wav");
        let rate = 8000u32;
        let frames = rate as usize * 3;
        let l: Vec<f64> = (0..frames)
            // Louder along the file: the last slice is the loudest.
            .map(|i| (i as f64 / frames as f64) * 0.9 * if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        crate::audio::converter::pipeline::prepare::tests::write_wav24(&path, rate, &l, &l);
        let w = get(path.to_str().unwrap()).unwrap();
        assert!((w.duration_s - 3.0).abs() < 0.01, "{}", w.duration_s);
        assert!(w.peak[BINS - 1] > w.peak[BINS / 2] && w.peak[BINS / 2] > w.peak[10]);
        assert!(std::fs::read_dir(d.path().join("cache")).unwrap().count() == 1);
        assert_eq!(get(path.to_str().unwrap()).unwrap(), w);
    }
}
