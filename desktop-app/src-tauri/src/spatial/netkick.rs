//! Hybrid-Phase's second opinion on kicks: the kicks the separation network
//! hears in its drums source, for the player's onset detector.
//!
//! A steady kick can sit 14 dB under the bass inside a loud wall of synths:
//! the mix's low band does not rise on it, so the detector (which keeps such
//! a wall linear, and rightly) never switches on it either. The drums source
//! of HTDemucs has the kick alone — its 0–172 Hz band jumps by tens of dB on
//! every one. Measured on seven tracks (HL19): every kick of the wall is
//! found, a pad intro gets none, and taken only where the detector sees a
//! dense texture (`OnsetStream`) the other tracks keep their envelope.
//!
//! The list is made once per sound (its content key, as the spatial maps),
//! in the background, and kept on disk next to them; the GPU or the CPU give
//! the same list frame for frame. Without the instruments pack there is no
//! list and the detector decides alone.
//!
//! The rest of the kit (`NetMode::Kit`): the snare and the hats hide under a
//! wall the same way. DrumSep takes the drums source apart on the GPU; each
//! piece's hits by the kicks' rule, its level against the mix's loud level in
//! its own band (hats are quiet next to the whole mix). Measured (HL21): under
//! the wall the hats are caught, and the dense texture gate keeps the other
//! tracks as they are. Without the GPU the drums source's bands stand in for
//! the pieces (they hear the hats less well).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use rustfft::{num_complex::Complex, FftPlanner};

use super::owner::{self, Claim};
use super::stems::Stems;
use super::{analysis, core, decode, job, kit, map, pack, stems};

/// Bump when the lists would come out different.
pub const ALGO: u32 = 2;

/// What Hybrid-Phase takes from the network: nothing (the detector alone),
/// the kicks, or the whole kit (kicks, snare, hats).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    Off,
    Kick,
    #[default]
    Kit,
}

/// The list's grid: the network's rate, the detector's STFT at it.
pub(super) const SR: f64 = 44_100.0;
pub(super) const NFFT: usize = 2048;
pub(super) const HOP: usize = 512;
pub(super) const FPS: f64 = SR / HOP as f64;

/// A kick: its frame on the 44.1 kHz / hop 512 grid (the frame where its
/// rise crosses the threshold — where it starts, a quarter frame after
/// DrumSep's own kick on average) and how sure the rule is of it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub frame: u32,
    pub conf: f32,
}

/// A track's confident kicks, in time order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kicks {
    pub hits: Vec<Hit>,
}

/// Kicks under this confidence are not kept (.7 gives the same lists: the
/// confidence is nearly binary on kicks).
pub(super) const CONF_MIN: f64 = 0.5;

pub(super) fn db(p: f64) -> f64 {
    10.0 * (p + 1e-12).log10()
}

pub(super) fn ramp(x: f64, from: f64, to: f64) -> f64 {
    ((x - from) / (to - from)).clamp(0.0, 1.0)
}

/// Per frame: the power of `x` (mono) in bins `[lo, hi)` of a Hann-windowed
/// 2048-point spectrum, frames `[512 f, 512 f + 2048)`.
fn band_powers(x: &[f32], bins: &[(usize, usize)]) -> Vec<Vec<f64>> {
    band_powers_at(x, 0, bins, 0..frames_of(x.len()))
}

/// The list's frames in `len` samples.
pub(super) fn frames_of(len: usize) -> usize {
    if len >= NFFT { (len - NFFT) / HOP + 1 } else { 0 }
}

/// `band_powers` of the track's `frames` from `x`, its samples from track
/// sample `x0` on (`x` holds every sample those frames read).
pub(super) fn band_powers_at(x: &[f32], x0: usize, bins: &[(usize, usize)], frames: std::ops::Range<usize>) -> Vec<Vec<f64>> {
    let hann: Vec<f64> = (0..NFFT).map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos())).collect();
    let fft = FftPlanner::<f64>::new().plan_fft_forward(NFFT);
    let mut out = vec![Vec::with_capacity(frames.len()); bins.len()];
    let mut buf = vec![Complex { re: 0.0, im: 0.0 }; NFFT];
    for f in frames {
        for (i, c) in buf.iter_mut().enumerate() {
            *c = Complex { re: x[f * HOP - x0 + i] as f64 * hann[i], im: 0.0 };
        }
        fft.process(&mut buf);
        for (o, &(lo, hi)) in out.iter_mut().zip(bins) {
            o.push(buf[lo..hi.min(NFFT / 2 + 1)].iter().map(|c| c.norm_sqr()).sum());
        }
    }
    out
}

/// The rise of level `l` (dB) at each frame: its top over `[f − 1, f + 2]`
/// against its floor over `[f − 6, f − 1]`; and that top.
fn rise(l: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let n = l.len();
    let mut r = vec![0.0; n];
    let mut top = vec![0.0; n];
    for f in 0..n {
        let a = l[f.saturating_sub(1)..(f + 3).min(n)].iter().copied().fold(f64::MIN, f64::max);
        let b = if f == 0 { l[0] } else { l[f.saturating_sub(6)..f].iter().copied().fold(f64::MAX, f64::min) };
        r[f] = a - b;
        top[f] = a;
    }
    (r, top)
}

/// numpy's `percentile` (linear between the closest ranks).
pub(super) fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let x = q / 100.0 * (s.len() - 1) as f64;
    let (i, t) = (x.floor() as usize, x.fract());
    if i + 1 < s.len() { s[i] + t * (s[i + 1] - s[i]) } else { s[i] }
}

/// The hits of one piece: its power `piece` per frame, `other`'s in the same
/// band, the mix's level `mix_db` and the loud level `loud` its level is read
/// against. A hit: the piece's level jumps by 6 dB or more within ~35 ms, to
/// within 15 dB of its own top over the last 3 s and no more than 40 dB under
/// the mix, 90 ms after the last one at least. Its confidence: how far it
/// jumps (8 → 18 dB), how loud it is against `loud` (−40 → −28 dB), and that
/// `other` (pads, synths) does not jump with it as much (a change of the
/// whole mix the piece may only share).
pub(super) fn hits_of(piece: &[f64], other: &[f64], mix_db: &[f64], loud: f64) -> Vec<Hit> {
    let n = piece.len().min(other.len()).min(mix_db.len());
    let l: Vec<f64> = piece[..n].iter().map(|&p| db(p)).collect();
    let (r, top) = rise(&l);
    let (r_other, _) = rise(&other[..n].iter().map(|&p| db(p)).collect::<Vec<_>>());
    let ctx = (3.0 * FPS) as usize;
    let mut hits = Vec::new();
    let mut prev_x = false;
    let mut last: Option<usize> = None;
    for f in 0..n {
        let top3 = l[(f + 1).saturating_sub(ctx)..=f].iter().copied().fold(f64::MIN, f64::max);
        let x = r[f] >= 6.0 && top[f] >= top3 - 15.0 && top[f] - mix_db[f] >= -40.0;
        let edge = x && !prev_x;
        prev_x = x;
        if !edge || last.is_some_and(|j| (f - j) as f64 <= 0.09 * FPS) {
            continue;
        }
        last = Some(f);
        // Level and leak are read where the level steps most (within 4
        // frames), as they were measured.
        let steep = (f..(f + 4).min(n))
            .map(|k| (k, if k == 0 { 0.0 } else { l[k] - l[k - 1] }))
            .fold((f, f64::MIN), |b, (k, d)| if d > b.1 { (k, d) } else { b })
            .0;
        let lvl = db(piece[steep..(steep + 3).min(n)].iter().copied().fold(0.0, f64::max)) - loud;
        let leak = r_other[steep.saturating_sub(3)..(steep + 2).min(n)].iter().copied().fold(f64::MIN, f64::max) - r[f];
        let conf = ramp(r[f], 8.0, 18.0) * ramp(lvl, -40.0, -28.0) * (1.0 - ramp(leak, -6.0, 0.0));
        if conf >= CONF_MIN {
            hits.push(Hit { frame: f as u32, conf: conf as f32 });
        }
    }
    hits
}

pub(super) fn bin(hz: f64) -> usize {
    (hz * NFFT as f64 / SR).round() as usize
}

pub(super) fn levels(p: &[f64]) -> Vec<f64> {
    p.iter().map(|&p| db(p)).collect()
}

/// The kicks of a track from its mix and its drums and other sources, all
/// mono at 44.1 kHz: the drums' 0–172 Hz band, `other`'s 0–344 Hz, the
/// level against the whole mix's loud level.
pub fn kicks_of(mix: &[f32], drums: &[f32], other: &[f32]) -> Kicks {
    let mix_db = levels(&band_powers(mix, &[(0, usize::MAX)]).remove(0));
    let dr = band_powers(drums, &[(0, bin(172.0))]).remove(0);
    let ot = band_powers(other, &[(0, bin(344.0))]).remove(0);
    let loud = percentile(&mix_db, 99.5);
    Kicks { hits: hits_of(&dr, &ot, &mix_db, loud) }
}

/// The snare's band and the hats' (as the spatial maps' `B_SNARE`, `B_HATS`).
pub(super) fn snare_band() -> (usize, usize) {
    (bin(150.0), bin(4000.0))
}
pub(super) fn hats_band() -> (usize, usize) {
    (bin(6000.0), bin(16000.0))
}

/// Where the snare and the hats come from.
#[derive(Clone, Copy, Debug)]
pub enum Pieces<'a> {
    /// DrumSep's snare and hats (mono)
    Net { snare: &'a [f32], hats: &'a [f32] },
    /// the drums source's snare and hats bands (mono)
    Bands { drums: &'a [f32] },
}

/// The snare's and the hats' hits of a track, in one list in time order:
/// each piece by the kicks' rule, `other` in the piece's band, its level
/// against the mix's loud level in that band.
pub fn pieces_of(mix: &[f32], other: &[f32], from: Pieces) -> Kicks {
    let (sb, hb) = (snare_band(), hats_band());
    let m = band_powers(mix, &[(0, usize::MAX), sb, hb]);
    let o = band_powers(other, &[sb, hb]);
    let p = match from {
        Pieces::Net { snare, hats } => {
            vec![band_powers(snare, &[(0, usize::MAX)]).remove(0), band_powers(hats, &[(0, usize::MAX)]).remove(0)]
        }
        Pieces::Bands { drums } => band_powers(drums, &[sb, hb]),
    };
    let mix_db = levels(&m[0]);
    let mut hits = Vec::new();
    for i in 0..2 {
        let loud = percentile(&levels(&m[1 + i]), 99.5);
        hits.extend(hits_of(&p[i], &o[i], &mix_db, loud));
    }
    hits.sort_by_key(|h| h.frame);
    hits.dedup_by_key(|h| h.frame);
    Kicks { hits }
}

/// The detector's frame at source rate `sr` that the list's frame `j` falls
/// in: by the time of the frames' centres, so 48 k's frames (hop 512 at
/// 48 kHz) and 96 k's (hop 1024) land where the sound is.
pub fn frame_at(j: u32, sr: u32) -> usize {
    let (n_fft, hop) = crate::audio::hpss_native::stft_params(sr);
    let t = (j as f64 * HOP as f64 + NFFT as f64 / 2.0) / SR;
    ((t * sr as f64 - n_fft as f64 / 2.0) / hop as f64).round().max(0.0) as usize
}

pub(super) fn mono(l: &[f32], r: &[f32]) -> Vec<f32> {
    l.iter().zip(r).map(|(a, b)| 0.5 * (a + b)).collect()
}

/// The kicks from the six sources `job::separate_all` makes and the mix.
pub fn kicks_of_sources(l: &[f32], r: &[f32], src: &[Vec<f32>]) -> Kicks {
    let s = |k: usize| mono(&src[k * 2], &src[k * 2 + 1]);
    kicks_of(&mono(l, r), &s(stems::DRUMS), &s(stems::OTHER))
}

/// The snare's and hats' hits: DrumSep's (the mono pieces its run kept,
/// `job::kit_run` with taps), else the drums source's bands; and which.
pub(super) fn pieces_from(mix: [&[f32]; 2], drums: [&[f32]; 2], other: &[f32], run: Option<&mut kit::KitRun>) -> (Kicks, &'static str) {
    let m = mono(mix[0], mix[1]);
    match run.and_then(|r| r.mono_tap.take()).filter(|t| t.len() == 2) {
        Some(mut tap) => {
            let h = tap.pop().unwrap().1;
            let s = tap.pop().unwrap().1;
            (pieces_of(&m, other, Pieces::Net { snare: &s, hats: &h }), "drumsep")
        }
        None => (pieces_of(&m, other, Pieces::Bands { drums: &mono(drums[0], drums[1]) }), "bands"),
    }
}

// ---- kept on disk ------------------------------------------------------------

/// A track's lists: its kicks, and its snare's and hats' hits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lists {
    pub kicks: Kicks,
    pub pieces: Kicks,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    algo: u32,
    hits: Vec<Hit>,
    pieces: Vec<Hit>,
    /// where the snare and hats came from: "drumsep" or "bands"
    from: String,
    /// Beside the source: the sound's content key (hex), and the file's size
    /// and time when it was made (the same file is read without decoding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time: Option<u64>,
}

impl Kept {
    fn new(lists: &Lists, from: &str) -> Kept {
        Kept { algo: ALGO, hits: lists.kicks.hits.clone(), pieces: lists.pieces.hits.clone(), from: from.into(), content: None, size: None, time: None }
    }

    fn take(self) -> Option<Found> {
        (self.algo == ALGO).then(|| (Lists { kicks: Kicks { hits: self.hits }, pieces: Kicks { hits: self.pieces } }, self.from))
    }
}

/// Kept lists and where their snare and hats came from ("drumsep" or "bands").
type Found = (Lists, String);

fn read_kept(file: &Path) -> Option<Kept> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

fn write_kept(file: &Path, kept: &Kept) -> Result<(), String> {
    let tmp = file.with_extension("part");
    let text = serde_json::to_string(kept).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Where the lists are kept when the source's folder takes no file (beside
/// the spatial maps).
pub fn cache_dir() -> Option<PathBuf> {
    Some(pack::root()?.join("hpkicks"))
}

fn kept_file(dir: &Path, content: u64) -> PathBuf {
    dir.join(format!("{content:016x}.json"))
}

#[cfg(test)]
pub fn load_in(dir: &Path, content: u64) -> Option<Lists> {
    found_in(dir, content).map(|k| k.0)
}

fn found_in(dir: &Path, content: u64) -> Option<Found> {
    read_kept(&kept_file(dir, content))?.take()
}

pub fn save_in(dir: &Path, content: u64, lists: &Lists, from: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_kept(&kept_file(dir, content), &Kept::new(lists, from))
}

/// The lists' file beside the source: `<file name>.hp_kit.json` (the whole
/// name: `a.flac` and `a.mp3` in one folder are two sounds).
pub fn sidecar_of(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    Some(path.with_file_name(format!("{name}.hp_kit.json")))
}

/// The lists beside the source when that very file made them (its size and
/// time): read without decoding a sample.
#[cfg(test)]
pub fn beside_unchanged(path: &Path) -> Option<Lists> {
    found_beside_unchanged(path).map(|k| k.0)
}

fn found_beside_unchanged(path: &Path) -> Option<Found> {
    let k = read_kept(&sidecar_of(path)?)?;
    let (size, time) = map::file_mark(path)?;
    (k.size == Some(size) && k.time == Some(time)).then_some(())?;
    k.take()
}

/// The lists beside the source when they are of its sound (the file copied
/// or touched since: the content key decides).
fn beside_of_content(path: &Path, content: u64) -> Option<Found> {
    let k = read_kept(&sidecar_of(path)?)?;
    (k.content.as_deref() == Some(format!("{content:016x}").as_str())).then_some(())?;
    k.take()
}

/// The lists of the file at `path` by its sound's key: beside it (the file
/// copied or touched since), else in the cache.
fn of_content(path: &str, content: u64) -> Option<Found> {
    beside_of_content(Path::new(path), content).or_else(|| found_in(&cache_dir()?, content))
}

/// Whether the lists of the file at `path` (its sound `content`) are kept.
pub(super) fn known_of(path: &str, content: u64) -> bool {
    known(path).is_some() || of_content(path, content).is_some()
}

/// The lists Line Up's job made with a track's map: in the cache only —
/// nothing goes beside the listener's music unless Hybrid-Phase is on.
pub(super) fn keep_cached(content: u64, lists: &Lists, from: &str) {
    match cache_dir().ok_or_else(|| "no folder for the lists".to_string()).and_then(|d| save_in(&d, content, lists, from).map(|_| d)) {
        Ok(d) => crate::aelog!("[HP-NET] the lists made with the track's map: {} kicks, {} snare/hats hits ({from}), kept in {}", lists.kicks.hits.len(), lists.pieces.hits.len(), d.display()),
        Err(e) => crate::aelog!("[HP-NET] the lists made with the track's map not kept: {e}"),
    }
}

/// The lists beside the source, with its sound's key and the file's mark.
pub fn save_beside(path: &Path, content: u64, lists: &Lists, from: &str) -> Result<PathBuf, String> {
    let file = sidecar_of(path).ok_or("no file name")?;
    let mark = map::file_mark(path);
    let kept = Kept { content: Some(format!("{content:016x}")), size: mark.map(|m| m.0), time: mark.map(|m| m.1), ..Kept::new(lists, from) };
    write_kept(&file, &kept)?;
    hide(&file);
    Ok(file)
}

/// The file out of the folder's listing in Explorer (Windows; elsewhere the
/// name stays as it is). Best effort: a visible file works the same.
fn hide(file: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use winapi::um::fileapi::{GetFileAttributesW, SetFileAttributesW, INVALID_FILE_ATTRIBUTES};
        use winapi::um::winnt::FILE_ATTRIBUTE_HIDDEN;
        let w: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path.
        unsafe {
            let a = GetFileAttributesW(w.as_ptr());
            if a != INVALID_FILE_ATTRIBUTES && a & FILE_ATTRIBUTE_HIDDEN == 0 {
                SetFileAttributesW(w.as_ptr(), a | FILE_ATTRIBUTE_HIDDEN);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = file;
}

/// The lists of the file at `path` found without decoding it: beside it, or
/// in the cache by the content key the path memo remembers.
pub fn known(path: &str) -> Option<Lists> {
    found(path).map(|k| k.0)
}

fn found(path: &str) -> Option<Found> {
    let p = Path::new(path);
    if let Some(k) = found_beside_unchanged(p) {
        return Some(k);
    }
    let content = map::cache_dir().as_deref().and_then(|m| map::remembered_in(m, path))?;
    beside_of_content(p, content).or_else(|| found_in(&cache_dir()?, content))
}

/// Kept for good: beside the source (the same file is never analysed
/// again, whatever the rack), else in the cache when its folder takes no file.
fn keep(path: &str, content: u64, lists: &Lists, from: &str) {
    match save_beside(Path::new(path), content, lists, from) {
        Ok(f) => crate::aelog!("[HP-NET] the lists kept beside the source: {}", f.display()),
        Err(e) => {
            let to = cache_dir().ok_or_else(|| "no folder for the lists".to_string());
            match to.and_then(|d| save_in(&d, content, lists, from).map(|_| d)) {
                Ok(d) => crate::aelog!("[HP-NET] the source's folder takes no file ({e}): the lists kept in {}", d.display()),
                Err(e2) => crate::aelog!("[HP-NET] the lists not kept: {e}; {e2}"),
            }
        }
    }
}

// ---- the player's requests ---------------------------------------------------

/// How far a track's lists are: the stage (`decode`, `stems`, `kit`) and the
/// whole of the work done, 0…1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Progress {
    pub stage: &'static str,
    pub done: f32,
    /// seconds left, as the work so far went (None until it has gone a while)
    pub eta_s: Option<f64>,
}

/// The share of the whole each stage takes (measured on Rebel Path, HL21):
/// on the GPU DrumSep takes longer than HTDemucs; on the CPU the kit is the
/// drums' bands, a moment.
fn stage_span(stage: &str, gpu: bool) -> (f32, f32) {
    let stems_end = if gpu { 0.42 } else { 0.95 };
    match stage {
        "decode" => (0.0, 0.03),
        "stems" => (0.03, stems_end),
        _ => (stems_end, 1.0),
    }
}

/// A track's lists as the player waits for them: the kicks first, the snare
/// and hats a little later (DrumSep runs after HTDemucs); empty until made
/// (and for good when they cannot be made — the detector then decides alone).
#[derive(Default)]
pub struct Slot {
    list: OnceLock<Kicks>,
    pieces: OnceLock<Kicks>,
    /// why no lists will come
    failed: OnceLock<String>,
    /// (stage, the whole done, when the work began, on the GPU)
    at: Mutex<(&'static str, f32, Option<std::time::Instant>, bool)>,
    /// what to do when it is settled (made or failed)
    then: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    /// The lists in parts (`progressive`), as they come.
    parts: Mutex<Parts>,
    grew: Condvar,
    /// the list frame the player reads on, + 1 (0: none)
    reading: std::sync::atomic::AtomicU64,
}

/// A track's lists in parts: each run's cover so far (0: the whole run's),
/// whether the player may play on them (`early`: the network is fast
/// enough), the start a player waits for and the track's frames.
#[derive(Default)]
struct Parts {
    covers: Vec<(usize, super::progressive::Cover)>,
    early: bool,
    start: Option<(u32, u32)>,
    total: u32,
}

#[cfg(test)]
thread_local! {
    /// The tests' wait for the parts (this thread's).
    pub(crate) static TEST_WAIT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A start plays on the lists in parts once they cover this much past it…
pub const START_COVER_S: f64 = 3.0;
/// …and this much before it (the warm-up a chain begun mid-track reads).
pub const START_BACK_S: f64 = 3.6;
/// The network's pace a start plays on the lists in parts at: slower, the
/// whole lists are waited for as before.
pub const EARLY_MIN_RT: f64 = 1.5;

/// The list frame at `s` seconds (rounded down).
pub fn list_frame(s: f64) -> u32 {
    (s.max(0.0) * FPS) as u32
}

/// The list frame about where the detector's frame `f` at `sr` lies (a
/// hint for the runs: a few frames early).
pub fn list_frame_of(f: usize, sr: u32) -> usize {
    let (n_fft, hop) = crate::audio::hpss_native::stft_params(sr);
    let t = (f as f64 * hop as f64 + n_fft as f64 / 2.0) / sr as f64;
    ((t * SR - NFFT as f64 / 2.0) / HOP as f64).max(0.0) as usize
}

/// The detector frames at `sr` a cover of list frames `[from, upto)` holds
/// whole: every list frame that lands on them is in it.
pub fn det_span(from: u32, upto: u32, sr: u32) -> (usize, usize) {
    let lo = if from == 0 { 0 } else { frame_at(from - 1, sr) + 1 };
    (lo, frame_at(upto, sr))
}

/// How long the detector may wait for the lists in parts to reach a frame:
/// on the render thread (`lead` Some) only while what is buffered on the
/// air outlasts it with a margin — before the sound (nothing on the air) a
/// little; elsewhere (a seek's chain made beside the music) a while.
pub fn wait_budget(lead: Option<Option<f64>>) -> std::time::Duration {
    #[cfg(test)]
    if let Some(ms) = TEST_WAIT_MS.with(|w| w.get()) {
        return std::time::Duration::from_millis(ms);
    }
    let s = match lead {
        None => 3.0,
        Some(None) => 2.0,
        Some(Some(buffered_s)) => (buffered_s - 0.35).clamp(0.0, 3.0),
    };
    std::time::Duration::from_secs_f64(s)
}

impl Slot {
    pub fn ready(&self) -> Option<&Kicks> {
        self.list.get()
    }

    pub fn pieces(&self) -> Option<&Kicks> {
        self.pieces.get()
    }

    pub fn fill(&self, kicks: Kicks) {
        let _ = self.list.set(kicks);
    }

    pub fn fill_pieces(&self, pieces: Kicks) {
        let _ = self.pieces.set(pieces);
    }

    fn fill_lists(&self, k: Lists) {
        self.fill(k.kicks);
        self.fill_pieces(k.pieces);
    }

    /// Why no lists come, when they will not.
    pub fn failed(&self) -> Option<&str> {
        self.failed.get().map(|s| s.as_str())
    }

    /// Made, or never to be: nothing more comes.
    pub fn settled(&self) -> bool {
        self.pieces.get().is_some() || self.failed.get().is_some()
    }

    /// `f` once settled (at once when it is).
    pub fn when_settled(&self, f: impl FnOnce() + Send + 'static) {
        let mut then = self.then.lock().unwrap();
        if self.settled() {
            drop(then);
            f();
        } else {
            then.push(Box::new(f));
        }
    }

    /// Settled: what waited for it is done (after the lists or the failure
    /// are set — `when_settled` looks at them under the same lock).
    fn settle(&self) {
        drop(self.parts.lock().unwrap());
        self.grew.notify_all();
        let then = std::mem::take(&mut *self.then.lock().unwrap());
        for f in then {
            f();
        }
    }

    /// Run `id`'s lists so far (`progressive`).
    pub(crate) fn publish(&self, id: usize, cover: super::progressive::Cover, total: usize) {
        let mut p = self.parts.lock().unwrap();
        p.total = total as u32;
        match p.covers.iter_mut().find(|(i, _)| *i == id) {
            Some(c) => c.1 = cover,
            None => p.covers.push((id, cover)),
        }
        drop(p);
        self.grew.notify_all();
    }

    pub(crate) fn set_early(&self) {
        self.parts.lock().unwrap().early = true;
        self.grew.notify_all();
    }

    /// The lists come in parts the player may play on.
    pub fn in_parts(&self) -> bool {
        self.parts.lock().unwrap().early
    }

    /// The player reads list frame `j` on (the runs make that part first).
    pub fn read_at(&self, j: usize) {
        self.reading.store(j as u64 + 1, Ordering::Release);
    }

    fn reading(&self) -> Option<usize> {
        match self.reading.load(Ordering::Acquire) {
            0 => None,
            j => Some(j as usize - 1),
        }
    }

    /// A start at `at_s` waits for its lists: they are made from there.
    pub fn want_s(&self, at_s: f64) {
        let (a, b) = (list_frame(at_s - START_BACK_S), list_frame(at_s + START_COVER_S));
        self.parts.lock().unwrap().start = Some((a, b));
        self.read_at(a as usize);
    }

    /// The start asked for (`want_s`) may play on the lists in parts.
    pub fn startable(&self) -> bool {
        let p = self.parts.lock().unwrap();
        let Some((a, b)) = p.start.filter(|_| p.early) else { return false };
        let b = b.min(p.total);
        p.covers.iter().any(|(_, c)| c.from <= a && c.upto >= b)
    }

    /// The cover that holds detector frame `f` at `sr`, with the detector
    /// frames it holds whole.
    pub fn cover_for(&self, f: usize, sr: u32) -> Option<(usize, usize, super::progressive::Cover)> {
        let p = self.parts.lock().unwrap();
        if !p.early {
            return None;
        }
        p.covers
            .iter()
            .map(|(_, c)| (det_span(c.from, c.upto, sr), c))
            .filter(|((lo, hi), _)| *lo <= f && f < *hi)
            .max_by_key(|((_, hi), _)| *hi)
            .map(|((lo, hi), c)| (lo, hi, c.clone()))
    }

    /// Wait up to `max` for a cover of detector frame `f` (or the slot to
    /// settle, or the parts to stop); true when one is there. `steer`: the
    /// runs are told the player reads there (work behind the music does
    /// not steer them, and waits out a start not yet in parts too).
    pub fn wait_cover(&self, f: usize, sr: u32, max: std::time::Duration, steer: bool) -> bool {
        if steer {
            self.read_at(list_frame_of(f, sr).saturating_sub(4));
        }
        let end = std::time::Instant::now() + max;
        let mut p = self.parts.lock().unwrap();
        loop {
            let has = p.early && p.covers.iter().any(|(_, c)| {
                let (lo, hi) = det_span(c.from, c.upto, sr);
                lo <= f && f < hi
            });
            if has {
                return true;
            }
            let now = std::time::Instant::now();
            if (steer && !p.early) || now >= end || self.settled() {
                return false;
            }
            p = self.grew.wait_timeout(p, end - now).unwrap().0;
        }
    }

    fn fail(&self, why: String) {
        let _ = self.failed.set(why);
        self.settle();
    }

    fn begin(&self) {
        let mut at = self.at.lock().unwrap();
        *at = ("decode", 0.0, Some(std::time::Instant::now()), at.3);
    }

    fn on_gpu(&self, gpu: bool) {
        self.at.lock().unwrap().3 = gpu;
    }

    /// `frac` of `stage` done.
    fn step(&self, stage: &'static str, frac: f32) {
        let mut at = self.at.lock().unwrap();
        let (a, b) = stage_span(stage, at.3);
        let done = a + (b - a) * frac.clamp(0.0, 1.0);
        // never backwards (a stage's first step after the last one's end)
        *at = (stage, done.max(if at.0 == stage { at.1 } else { 0.0 }), at.2, at.3);
    }

    pub fn progress(&self) -> Progress {
        if self.settled() {
            return Progress { stage: "", done: 1.0, eta_s: Some(0.0) };
        }
        let (stage, mut done, since, gpu) = *self.at.lock().unwrap();
        // In parts on the GPU: how much of the start's span is covered.
        if gpu && stage != "decode" {
            let p = self.parts.lock().unwrap();
            if let Some((a, b)) = p.start {
                let b = b.min(p.total.max(a + 1));
                let got = p.covers.iter().filter(|(_, c)| c.from <= a).map(|(_, c)| c.upto.min(b).saturating_sub(a)).max().unwrap_or(0);
                done = 0.03 + 0.97 * got as f32 / (b - a).max(1) as f32;
            }
        }
        let spent = since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let eta_s = (done > 0.05 && spent > 0.5).then(|| spent * (1.0 - done as f64) / done as f64);
        Progress { stage, done, eta_s }
    }
}

/// What the network gives Hybrid-Phase: `AURA_HP_NETKICK=0|kick|kit` (1 =
/// kick) over the player's setting.
pub fn mode(setting: NetMode) -> NetMode {
    mode_of(std::env::var("AURA_HP_NETKICK").ok().as_deref(), setting)
}

fn mode_of(env: Option<&str>, setting: NetMode) -> NetMode {
    match env {
        Some("0") | Some("off") => NetMode::Off,
        Some("1") | Some("kick") => NetMode::Kick,
        Some("kit") => NetMode::Kit,
        _ => setting,
    }
}

/// The paths waiting for the worker: the ones the listener waits for (newest
/// first), then the ones made ahead (the next tracks, newest first).
#[derive(Default, Debug)]
struct Queue {
    urgent: VecDeque<String>,
    later: VecDeque<String>,
}

const LATER_MAX: usize = 3;

impl Queue {
    /// `path` asked for; a path the listener waits for is not made "later"
    /// by a request without them.
    fn push(&mut self, path: &str, urgent: bool) {
        if !urgent && self.urgent.iter().any(|p| p == path) {
            return;
        }
        self.urgent.retain(|p| p != path);
        self.later.retain(|p| p != path);
        if urgent {
            self.urgent.push_front(path.to_string());
        } else {
            self.later.push_front(path.to_string());
            self.later.truncate(LATER_MAX);
        }
    }

    /// A job stopped for one the listener waits for: made after the rest.
    fn push_back_later(&mut self, path: &str) {
        if self.urgent.iter().chain(&self.later).all(|p| p != path) {
            self.later.push_back(path.to_string());
        }
    }

    /// The listener left `path`: it is made with the rest ahead.
    fn demote(&mut self, path: &str) {
        if let Some(i) = self.urgent.iter().position(|p| p == path) {
            self.urgent.remove(i);
            self.later.push_front(path.to_string());
            self.later.truncate(LATER_MAX);
        }
    }

    fn pop(&mut self) -> Option<(String, bool)> {
        self.urgent.pop_front().map(|p| (p, true)).or_else(|| self.later.pop_front().map(|p| (p, false)))
    }
}

/// The job the worker is on.
struct Now {
    path: String,
    /// the listener waits for it: it runs at normal priority
    urgent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

struct Service {
    /// The last tracks asked for, newest first.
    slots: Mutex<VecDeque<(String, Arc<Slot>)>>,
    queue: Mutex<Queue>,
    wake: Condvar,
    now: Mutex<Option<Now>>,
}

const SLOTS_KEPT: usize = 8;

static SERVICE: OnceLock<Service> = OnceLock::new();

fn service() -> &'static Service {
    SERVICE.get_or_init(|| {
        let _ = std::thread::Builder::new().name("hp-netkick".into()).spawn(worker);
        Service { slots: Mutex::new(VecDeque::new()), queue: Mutex::new(Queue::default()), wake: Condvar::new(), now: Mutex::new(None) }
    })
}

/// The list of the file at `path`, as it comes: at once from the memory or
/// the disk when made before, else made in the background (the newest
/// request first). None without the instruments pack.
pub fn request(path: &str) -> Option<Arc<Slot>> {
    prepare(path, false)
}

/// The lists of the file at `path`: settled at once when kept on disk, else
/// made by the worker — first when the listener waits for them (`urgent`:
/// at normal priority, and any other job stops for it and is made after),
/// else after those, below normal priority (the next tracks, made while one
/// plays). None without the instruments pack.
pub fn prepare(path: &str, urgent: bool) -> Option<Arc<Slot>> {
    pack::installed()?;
    let s = service();
    let slot = {
        let mut slots = s.slots.lock().unwrap();
        if let Some((_, slot)) = slots.iter().find(|(p, _)| p == path) {
            slot.clone()
        } else {
            let slot = Arc::new(Slot::default());
            slots.push_front((path.to_string(), slot.clone()));
            slots.truncate(SLOTS_KEPT);
            drop(slots);
            if let Some(k) = known(path) {
                crate::aelog!("[HP-NET] {path}: {} kicks, {} snare/hats hits from the disk", k.kicks.hits.len(), k.pieces.hits.len());
                slot.fill_lists(k);
                slot.settle();
            }
            slot
        }
    };
    if slot.settled() {
        return Some(slot);
    }
    {
        let now = s.now.lock().unwrap();
        if let Some(n) = now.as_ref() {
            if n.path == path {
                if urgent {
                    n.urgent.store(true, Ordering::Release);
                }
                return Some(slot);
            }
            if urgent {
                n.stop.store(true, Ordering::Release);
            }
        }
    }
    s.queue.lock().unwrap().push(path, urgent);
    s.wake.notify_all();
    Some(slot)
}

/// The listener left `path` before its lists were made: they are finished
/// below normal priority (kept for its next play), after the ones waited for.
pub fn demote(path: &str) {
    let Some(s) = SERVICE.get() else { return };
    if let Some(n) = s.now.lock().unwrap().as_ref().filter(|n| n.path == path) {
        n.urgent.store(false, Ordering::Release);
    }
    s.queue.lock().unwrap().demote(path);
}

fn worker() {
    let s = service();
    loop {
        let (path, urgent) = {
            let mut q = s.queue.lock().unwrap();
            loop {
                if let Some(p) = q.pop() {
                    break p;
                }
                q = s.wake.wait(q).unwrap();
            }
        };
        let Some(slot) = s.slots.lock().unwrap().iter().find(|(p, _)| *p == path).map(|(_, s)| s.clone()) else { continue };
        if slot.settled() {
            continue;
        }
        let (urgent, stop) = (Arc::new(AtomicBool::new(urgent)), Arc::new(AtomicBool::new(false)));
        *s.now.lock().unwrap() = Some(Now { path: path.clone(), urgent: urgent.clone(), stop: stop.clone() });
        // The priority follows the listener: up while they wait, down when
        // they leave (looked at between the network's segments).
        let prio = std::cell::Cell::new(None::<bool>);
        let _way = crate::player::foreground::give_way_here();
        let knobs = super::threads::Knobs::get();
        let stopped = || {
            let u = urgent.load(Ordering::Acquire);
            if prio.get() != Some(u) {
                use crate::audio::os_thread::{set_priority, BELOW_NORMAL, NORMAL};
                let p = knobs.prio.unwrap_or(if u { NORMAL } else { BELOW_NORMAL });
                set_priority(p);
                super::threads::set_priority(p);
                // the parallel work from now on: the light pool when made ahead
                super::threads::set_light(!u || knobs.threads.is_some());
                prio.set(Some(u));
            }
            if knobs.gap_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(knobs.gap_ms));
            }
            // Made ahead, it gives way between segments to a start or seek
            // and to music on the air that runs short.
            if !u {
                if let Some(w) = crate::player::foreground::GiveWay::current() {
                    w.pause(&stop);
                }
            }
            stop.load(Ordering::Acquire)
        };
        stopped();
        // Made ahead: after a start or seek is heard, not over its first block.
        if !urgent.load(Ordering::Acquire) {
            crate::player::foreground::wait_quiet(&stop);
        }
        let made = make(&path, &slot, &stopped, true);
        match made {
            Ok((tail, _)) => {
                slot.settle();
                // Line Up's map from the same separation: made ahead (below
                // normal priority, the light pool, giving way), after the
                // start is heard — and before the next tracks' lists, so
                // that one heavy job runs in the background at a time.
                if let Some(t) = tail {
                    urgent.store(false, Ordering::Release);
                    stopped();
                    // the start the lists let go begins a moment later: it
                    // is waited for, then the quiet after it
                    for _ in 0..20 {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    crate::player::foreground::wait_quiet(&stop);
                    t.run(&path, &stopped);
                }
            }
            Err(_) if stop.load(Ordering::Acquire) => {
                crate::aelog!("[HP-NET] {path}: stopped for a track the listener waits for — made after it");
                s.queue.lock().unwrap().push_back_later(&path);
            }
            Err(e) => {
                crate::aelog!("[HP-NET] {path}: no kick list ({e}) — the detector alone");
                slot.fail(e);
            }
        }
        *s.now.lock().unwrap() = None;
    }
}

// ---- the converter's -----------------------------------------------------------

/// The lists a render's Hybrid-Phase takes: the player's for the same file
/// (one cache, one separation), with where the snare and hats came from and
/// whether the whole kit is used (else the kicks alone).
#[derive(Clone, Debug, PartialEq)]
pub struct ForRender {
    pub lists: Lists,
    pub from: String,
    pub kit: bool,
}

impl ForRender {
    /// What the render stamp says of the detector: `kit:<from>`, `kick`.
    pub fn stamp(&self) -> String {
        if self.kit {
            format!("kit:{}", self.from)
        } else {
            "kick".into()
        }
    }
}

/// Whether a render's Hybrid-Phase takes the network's hits now: the
/// network on (`AURA_HP_NETKICK` aside, always) and the pack installed.
/// Never in the tests (the machine's pack must not decide them).
pub fn net_now() -> bool {
    !cfg!(test) && mode(NetMode::default()) != NetMode::Off && pack::installed().is_some()
}

#[cfg(test)]
pub(crate) static TEST_LISTS: Mutex<Vec<(String, ForRender)>> = Mutex::new(Vec::new());

/// A file's lists for a render, made on this thread when not kept (the
/// player's way, without Line Up's map after it): None when the network is
/// off or the pack is missing, or they cannot be made — the detector alone
/// (v6) then. `progress` is told how far, every quarter second.
pub fn for_render(path: &str, stop: &dyn Fn() -> bool, progress: &(dyn Fn(Progress) + Sync)) -> Option<ForRender> {
    #[cfg(test)]
    {
        let _ = (stop, progress);
        TEST_LISTS.lock().unwrap().iter().find(|(p, _)| p == path).map(|(_, f)| f.clone())
    }
    #[cfg(not(test))]
    {
        if !net_now() {
            return None;
        }
        let net = mode(NetMode::default());
        let slot = Slot::default();
        let done = AtomicBool::new(false);
        let made = std::thread::scope(|sc| {
            sc.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if !done.load(Ordering::Acquire) {
                        progress(slot.progress());
                    }
                }
            });
            let made = make(path, &slot, stop, false);
            done.store(true, Ordering::Release);
            made
        });
        match made {
            Ok((_, from)) => {
                let lists = Lists { kicks: slot.ready()?.clone(), pieces: slot.pieces()?.clone() };
                Some(ForRender { lists, from, kit: net == NetMode::Kit })
            }
            Err(e) => {
                crate::aelog!("[HP-NET] {path}: no lists for the render ({e}) — the detector alone");
                None
            }
        }
    }
}

/// The memory free after the separation for Line Up's map to be made from
/// its sources (they stay while the notes run); below it they go at once.
const TAIL_RAM_MB: u64 = 2048;

/// What a separation made for the lists still gives: Line Up's map — the
/// notes, the places and the room from its six sources and DrumSep's run,
/// as the map job makes it (`job::analyse`), kept in the cache. It holds
/// the track's separation (`owner`): Line Up waits for it, not separating
/// again.
struct Tail {
    l: Vec<f32>,
    r: Vec<f32>,
    src: Vec<Vec<f32>>,
    run: Option<kit::KitRun>,
    content: u64,
    dir: PathBuf,
    guard: owner::Guard,
}

impl Tail {
    fn run(self, path: &str, stop: &dyn Fn() -> bool) {
        let Tail { l, r, src, run, content, dir, guard } = self;
        let Some(maps) = map::cache_dir() else { return };
        if stop() {
            return;
        }
        let avail = crate::audio::memory::available_ram_mb();
        if avail < TAIL_RAM_MB / 2 {
            crate::aelog!("[HP-NET] {path}: the map not made from the lists' sources ({avail} MB free) — Line Up makes its own");
            return;
        }
        let t0 = std::time::Instant::now();
        crate::audio::memory::log_process_memory("[HP-NET] the map from the lists' sources: start");
        let made = (|| -> Result<map::TrackMap, String> {
            let len = l.len().min(r.len());
            let frames = len / analysis::HOP + 1;
            let st = Stems { src: std::array::from_fn(|s| [&src[s * 2][..], &src[s * 2 + 1][..]]), mix: [&l[..len], &r[..len]] };
            let (band, ref_db) = super::threads::par(|| super::mix_bands(&l, &r));
            guard.progress(job::AT_KIT + 0.02);
            let na = job::notes_of(&dir, &st, stop);
            if stop() {
                return Err("stopped".into());
            }
            guard.progress(0.95);
            let inp = job::Input { l: &l, r: &r, content, rough_band: &band, ref_db };
            job::finish_map(&inp, &st, frames, run, na, stop)
        })();
        crate::audio::memory::log_process_memory("[HP-NET] the map from the lists' sources: end");
        match made {
            Ok(m) => {
                guard.progress(1.0);
                crate::aelog!(
                    "[HP-NET] {path}: Line Up's map from the lists' sources: {} objects, {} notes in {:.1} s",
                    m.objects.len(), m.notes.len(), t0.elapsed().as_secs_f64()
                );
                if let Err(e) = map::save_in(&maps, &m, pack::PIN.version) {
                    crate::aelog!("[SPATIAL] the map not kept: {e}");
                }
            }
            Err(e) => crate::aelog!("[HP-NET] {path}: no map from the lists' sources ({e}) — Line Up makes its own"),
        }
    }
}

/// The map job's progress (`owner`, 0…1 over the sources, the kit, the
/// rest) as the slot's stages.
fn step_of_owner(slot: &Slot, p: f32) {
    if p < job::AT_STEMS {
        slot.step("stems", p / job::AT_STEMS);
    } else {
        slot.step("kit", (p - job::AT_STEMS) / (job::AT_KIT - job::AT_STEMS));
    }
}

/// A file's lists into `slot`: the disk's (beside it, by the path memo, else
/// by its sound's content key), else HTDemucs on the whole track — the GPU
/// when it has room, else the CPU — for the kicks, then DrumSep on its drums
/// (the GPU only; else the drums' bands) for the snare and hats; kept. One
/// separation of a track at a time (`owner`): when Line Up's job is at it,
/// the lists come from it. Made here on the GPU, its sources stay for Line
/// Up's map (the `Tail`, after the sound has started) when memory allows
/// and `tail_ok`. With it, where the snare and hats came from.
fn make(path: &str, slot: &Slot, stop: &dyn Fn() -> bool, tail_ok: bool) -> Result<(Option<Tail>, String), String> {
    let t0 = std::time::Instant::now();
    slot.begin();
    let dir = pack::installed().ok_or("no instruments pack")?;
    let from_disk = |(k, from): Found| {
        crate::aelog!("[HP-NET] {path}: {} kicks, {} snare/hats hits ({from}) from the disk", k.kicks.hits.len(), k.pieces.hits.len());
        slot.fill_lists(k);
        Ok((None, from))
    };
    if let Some(k) = found(path) {
        return from_disk(k);
    }
    let (l, r) = decode::decode_44k(Path::new(path), stop)?;
    let content = map::content_key(&l, &r);
    if let Some(m) = map::cache_dir().as_deref() {
        map::remember_in(m, path, content);
    }
    if let Some(k) = of_content(path, content) {
        return from_disk(k);
    }
    let guard = match owner::owners().claim(path) {
        Claim::Mine(g) => Some(g),
        Claim::Theirs(w) => {
            crate::aelog!("[HP-NET] {path}: Line Up's job separates it — the lists come with its map");
            if !w.wait_lists(stop, &mut |p| step_of_owner(slot, p)) {
                return Err("stopped".into());
            }
            if let Some(k) = of_content(path, content) {
                return from_disk(k);
            }
            // it ended without them: made here (as the only one, when it can)
            match owner::owners().claim(path) {
                Claim::Mine(g) => Some(g),
                Claim::Theirs(_) => None,
            }
        }
    };
    let owned = |p: f32| {
        if let Some(g) = &guard {
            g.progress(p);
        }
    };
    let t_dec = t0.elapsed().as_secs_f64();
    slot.step("stems", 0.0);
    // Some: the kit ran beside the separation (the lists in parts)
    let mut beside: Option<Option<kit::KitRun>> = None;
    let (mut src, gpu) = {
        let mut c = core::Core::open(&dir)?;
        slot.on_gpu(c.gpu);
        match parts_kit(&dir, tail_ok && c.gpu) {
            Some(k) => {
                let w = in_parts(&mut c, k, &l, &r, slot, &owned, stop)?;
                beside = Some(w.run);
                (w.src, c.gpu)
            }
            None => {
                let tick = &mut |p| {
                    slot.step("stems", p);
                    owned(job::AT_STEMS * p);
                };
                (job::separate_all(&mut c, &l, &r, tick, stop)?, c.gpu)
            }
        }
    };
    let t_sep = t0.elapsed().as_secs_f64() - t_dec;
    let kicks = kicks_of_sources(&l, &r, &src);
    slot.fill(kicks.clone());
    // Line Up's map from these sources, as its job makes them (the GPU's),
    // unless it is kept already or the memory is short.
    let tail = tail_ok
        && guard.is_some()
        && gpu
        && map::cache_dir().is_some_and(|m| map::load_in(&m, content, pack::PIN.version).is_none())
        && crate::audio::memory::available_ram_mb() >= TAIL_RAM_MB;
    let other = mono(&src[stems::OTHER * 2], &src[stems::OTHER * 2 + 1]);
    if !tail {
        // the snare and hats need only the drums source now
        for (k, s) in src.iter_mut().enumerate() {
            if k / 2 != stems::DRUMS {
                *s = Vec::new();
            }
        }
    }
    let drums = [&src[stems::DRUMS * 2][..], &src[stems::DRUMS * 2 + 1][..]];
    let t1 = std::time::Instant::now();
    slot.step("kit", 0.0);
    let tick = &mut |p| {
        slot.step("kit", p);
        owned(job::AT_STEMS + (job::AT_KIT - job::AT_STEMS) * p);
    };
    let mut run = match beside {
        Some(run) => run,
        None => match job::kit_run(&dir, &job::kit_file(&dir), drums, true, "[HP-NET] the snare and hats", tick, stop) {
            Ok(r) => r,
            Err(_) if stop() => return Err("stopped".into()),
            Err(e) => {
                crate::aelog!("[HP-NET] the snare and hats from the drums' bands: {e}");
                None
            }
        },
    };
    let (pieces, from) = pieces_from([&l, &r], drums, &other, run.as_mut());
    drop(other);
    let t_kit = t1.elapsed().as_secs_f64();
    let lists = Lists { kicks, pieces: pieces.clone() };
    slot.fill_pieces(pieces);
    keep(path, content, &lists, from);
    crate::aelog!(
        "[HP-NET] {path}: {} kicks, {} snare/hats hits ({from}); {:.0} s of sound: decode {:.1} s, sources {:.1} s ({}), kit {:.1} s, all {:.1} s",
        lists.kicks.hits.len(), lists.pieces.hits.len(), l.len() as f64 / SR, t_dec, t_sep, if gpu { "GPU" } else { "CPU" }, t_kit,
        t0.elapsed().as_secs_f64()
    );
    crate::audio::memory::log_process_memory("[HP-NET] the lists made");
    let tail = match guard {
        Some(guard) if tail => Some(Tail { l, r, src, run, content, dir, guard }),
        _ => None,
    };
    Ok((tail, from.to_string()))
}

/// The lists in parts (`AURA_HP_PARTS=0`: off): on the GPU, for the
/// player. With the kit's network: open beside the separation (None when
/// it does not open there — the whole way then, the kit after the
/// separation, as before); without it, the drums' bands.
fn parts_kit(dir: &Path, on: bool) -> Option<Option<kit::Kit>> {
    if !on || std::env::var("AURA_HP_PARTS").is_ok_and(|v| v == "0") {
        return None;
    }
    let file = job::kit_file(dir);
    if !file.is_file() {
        return Some(None);
    }
    match kit::Kit::open(dir, &file, false) {
        Ok(k) => Some(Some(k)),
        Err(e) => {
            crate::aelog!("[HP-NET] the kit does not open beside the separation ({e}) — the whole lists before the start");
            None
        }
    }
}

/// The runs' word to the slot: its covers, the player's place, and the
/// whole run's progress; the first cover tells whether the network is fast
/// enough to play on the parts.
struct SlotIo<'a> {
    slot: &'a Slot,
    owned: &'a dyn Fn(f32),
    t0: std::time::Instant,
    total: usize,
    first: bool,
}

impl super::progressive::Io for SlotIo<'_> {
    fn reading(&self) -> Option<usize> {
        self.slot.reading()
    }

    fn publish(&mut self, id: usize, cover: super::progressive::Cover) {
        if id == 0 && self.first && cover.upto > 0 {
            self.first = false;
            let wall = self.t0.elapsed().as_secs_f64();
            let x_rt = cover.upto as f64 / FPS / wall.max(1e-3);
            let early = x_rt >= EARLY_MIN_RT;
            crate::aelog!(
                "[firperf] hp first-ready ms={:.0} audio_s={:.1} x_rt={x_rt:.1} parts={}",
                wall * 1e3, cover.upto as f64 / FPS, if early { "on" } else { "off (slow: the whole lists first)" }
            );
            if early {
                self.slot.set_early();
            }
        }
        self.slot.publish(id, cover, self.total);
    }

    fn tick(&mut self, stems: f32, kit: f32) {
        if stems < 1.0 {
            self.slot.step("stems", stems);
        } else {
            self.slot.step("kit", kit);
        }
        (self.owned)(job::AT_STEMS * stems.min(1.0) + (job::AT_KIT - job::AT_STEMS) * kit);
    }
}

/// HTDemucs and the kit (`kit`, else the drums' bands) over the track in
/// parts, the lists made as they come (`progressive`); the same sources
/// and kit run as the whole way.
fn in_parts(c: &mut core::Core, kit: Option<kit::Kit>, l: &[f32], r: &[f32], slot: &Slot, owned: &dyn Fn(f32), stop: &dyn Fn() -> bool) -> Result<super::progressive::Whole, String> {
    let t0 = std::time::Instant::now();
    let mut io = SlotIo { slot, owned, t0, total: frames_of(l.len().min(r.len())), first: true };
    let has = kit.is_some();
    let mut k = kit;
    let mut net = |a: &[f32], b: &[f32]| k.as_mut().unwrap().chunk(a, b);
    let w = super::progressive::run(c, if has { Some(&mut net) } else { None }, l, r, &mut io, stop)?;
    crate::aelog!("[firperf] hp lists in parts all_s={:.1} audio_s={:.0}", t0.elapsed().as_secs_f64(), l.len() as f64 / SR);
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quiet low drone with a loud pad in `other` and kicks every 0.66 s
    /// in `drums` from `from_s`; the mix is their sum.
    fn track(secs: f64, from_s: f64, kick_amp: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = (secs * SR) as usize;
        let mut drums = vec![0f32; n];
        let mut other = vec![0f32; n];
        let mut seed = 7u64;
        for (i, o) in other.iter_mut().enumerate() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5;
            let t = i as f64 / SR;
            *o = 0.3 * (2.0 * std::f64::consts::PI * 220.0 * t).sin() as f32 + 0.05 * noise;
        }
        let mut t = from_s;
        while t < secs - 0.2 {
            let at = (t * SR) as usize;
            for k in 0..(0.15 * SR) as usize {
                let tt = k as f64 / SR;
                let f = 50.0 + 60.0 * (-tt * 30.0).exp();
                drums[at + k] += kick_amp * ((-tt * 18.0).exp() * (2.0 * std::f64::consts::PI * f * tt).sin()) as f32;
            }
            t += 0.66;
        }
        let mix: Vec<f32> = drums.iter().zip(&other).map(|(a, b)| a + b).collect();
        (mix, drums, other)
    }

    #[test]
    fn every_kick_of_the_drums_source_is_found_at_its_start() {
        let (mix, drums, other) = track(12.0, 1.0, 0.5);
        let k = kicks_of(&mix, &drums, &other);
        let want: Vec<f64> = (0..20).map(|i| 1.0 + 0.66 * i as f64).filter(|&t| t < 11.8).collect();
        assert_eq!(k.hits.len(), want.len(), "{:?}", k.hits);
        for (h, t) in k.hits.iter().zip(&want) {
            // The frame whose window [512 j, 512 j + 2048) the kick enters.
            let centre = (h.frame as f64 * HOP as f64 + NFFT as f64 / 2.0) / SR;
            assert!((centre - t).abs() < 2.0 * NFFT as f64 / SR, "kick at {t} s marked at {centre} s");
            assert!(h.conf >= 0.5);
        }
    }

    #[test]
    fn a_pad_alone_gives_no_kick() {
        let (mix, drums, other) = track(8.0, 99.0, 0.5);
        assert!(drums.iter().all(|&x| x == 0.0));
        assert!(kicks_of(&mix, &drums, &other).hits.is_empty());
    }

    #[test]
    fn a_kick_far_under_the_mix_is_not_sure_enough() {
        let (mix, drums, other) = track(8.0, 1.0, 0.002);
        assert!(kicks_of(&mix, &drums, &other).hits.is_empty());
    }

    #[test]
    fn frames_map_by_time_to_each_rate() {
        // 44.1 k: the same grid.
        for j in [0u32, 1, 100, 21_600] {
            assert_eq!(frame_at(j, 44_100), j as usize);
        }
        // 88.2 k: hop 1024, the same frame times.
        assert_eq!(frame_at(100, 88_200), 100);
        for sr in [48_000u32, 96_000, 192_000] {
            let (n_fft, hop) = crate::audio::hpss_native::stft_params(sr);
            for j in [10u32, 1000, 20_000] {
                let t = (j as f64 * 512.0 + 1024.0) / 44_100.0;
                let f = frame_at(j, sr);
                let centre = (f as f64 * hop as f64 + n_fft as f64 / 2.0) / sr as f64;
                assert!((centre - t).abs() <= 0.5 * hop as f64 / sr as f64 + 1e-9, "{sr}: {j} → {f}");
            }
        }
        // 48 k, hop 512: 2:00 lands 1.6 s of frames later than 44.1 k's index.
        assert_eq!(frame_at(10_336, 48_000), 11_250);
    }

    #[test]
    fn the_list_is_kept_and_read_back_by_version() {
        let dir = tempfile::tempdir().unwrap();
        let k = Lists {
            kicks: Kicks { hits: vec![Hit { frame: 3, conf: 0.9 }, Hit { frame: 77, conf: 1.0 }] },
            pieces: Kicks { hits: vec![Hit { frame: 40, conf: 0.6 }] },
        };
        save_in(dir.path(), 0xabc, &k, "drumsep").unwrap();
        assert_eq!(load_in(dir.path(), 0xabc), Some(k));
        assert_eq!(load_in(dir.path(), 0xabd), None);
        // the kicks-only lists of the first version are made again
        std::fs::write(kept_file(dir.path(), 0xabc), r#"{"algo":1,"hits":[]}"#).unwrap();
        assert_eq!(load_in(dir.path(), 0xabc), None);
    }

    /// Bursts of noise every 0.66 s from `from_s` (`high`: its difference,
    /// mostly over 6 kHz — hats; else as it is — a snare).
    fn bursts(n: usize, from_s: f64, amp: f32, high: bool) -> Vec<f32> {
        let mut x = vec![0f32; n];
        let mut seed = 11u64;
        let mut prev = 0f32;
        let mut t = from_s;
        while t < n as f64 / SR - 0.2 {
            let at = (t * SR) as usize;
            for k in 0..(0.12 * SR) as usize {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let noise = ((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5;
                let v = if high { noise - prev } else { noise };
                prev = noise;
                x[at + k] += amp * (-(k as f64 / SR) * if high { 60.0 } else { 25.0 }).exp() as f32 * v;
            }
            t += 0.66;
        }
        x
    }

    fn near(hits: &[Hit], t: f64) -> bool {
        hits.iter().any(|h| ((h.frame as f64 * HOP as f64 + NFFT as f64 / 2.0) / SR - t).abs() < 2.0 * NFFT as f64 / SR)
    }

    #[test]
    fn drumsep_snare_and_hats_under_a_pad_are_found() {
        let (mix0, _, other) = track(12.0, 99.0, 0.5);
        let n = mix0.len();
        let snare = bursts(n, 1.0, 0.4, false);
        let hats = bursts(n, 1.33, 0.4, true);
        let mix: Vec<f32> = (0..n).map(|i| mix0[i] + snare[i] + hats[i]).collect();
        let p = pieces_of(&mix, &other, Pieces::Net { snare: &snare, hats: &hats });
        let want: Vec<f64> = (0..40).map(|i| 1.0 + 0.33 * i as f64).filter(|&t| t < 11.8).collect();
        for t in &want {
            assert!(near(&p.hits, *t), "a hit at {t} s missed: {:?}", p.hits);
        }
        assert!(p.hits.len() <= want.len() + 2, "{} hits for {} bursts", p.hits.len(), want.len());
        assert!(p.hits.windows(2).all(|w| w[0].frame < w[1].frame));
    }

    #[test]
    fn the_drums_bands_stand_in_for_the_pieces() {
        let (mix0, _, other) = track(12.0, 99.0, 0.5);
        let n = mix0.len();
        let snare = bursts(n, 1.0, 0.4, false);
        let mix: Vec<f32> = (0..n).map(|i| mix0[i] + snare[i]).collect();
        let p = pieces_of(&mix, &other, Pieces::Bands { drums: &snare });
        let want: Vec<f64> = (0..20).map(|i| 1.0 + 0.66 * i as f64).filter(|&t| t < 11.8).collect();
        for t in &want {
            assert!(near(&p.hits, *t), "a snare at {t} s missed: {:?}", p.hits);
        }
    }

    #[test]
    fn no_drums_give_no_snare_or_hats() {
        let (mix, drums, other) = track(8.0, 99.0, 0.5);
        let p = pieces_of(&mix, &other, Pieces::Net { snare: &drums, hats: &drums });
        assert!(p.hits.is_empty());
        assert!(pieces_of(&mix, &other, Pieces::Bands { drums: &drums }).hits.is_empty());
    }

    #[test]
    fn modes_by_name() {
        assert_eq!(mode_of(Some("0"), NetMode::Kit), NetMode::Off);
        assert_eq!(mode_of(Some("1"), NetMode::Off), NetMode::Kick);
        assert_eq!(mode_of(Some("kick"), NetMode::Kit), NetMode::Kick);
        assert_eq!(mode_of(Some("kit"), NetMode::Off), NetMode::Kit);
        assert_eq!(mode_of(None, NetMode::Kit), NetMode::Kit);
        assert_eq!(mode_of(Some("?"), NetMode::Kick), NetMode::Kick);
        assert_eq!(NetMode::default(), NetMode::Kit);
    }

    /// Time to the list on a real track, the GPU's or (AURA_SPATIAL_CPU=1)
    /// the CPU's, its kicks in 10–55 s and how many of the beat part's
    /// (55–95 s) the detector's envelope catches too (±3 frames, ≥ .3):
    ///   AURA_SPATIAL_PACK_DIR=<pack> AURA_SPATIAL_FILE=<track>
    ///   cargo test --profile fast --bins netkick::tests::a_real_track -- --ignored --nocapture
    #[test]
    #[ignore]
    fn a_real_track() {
        let (Some(file), Some(pack)) = (std::env::var_os("AURA_SPATIAL_FILE"), std::env::var_os("AURA_SPATIAL_PACK_DIR")) else { return };
        let t0 = std::time::Instant::now();
        let (l, r) = decode::decode_44k(Path::new(&file), &|| false).unwrap();
        let t_dec = t0.elapsed().as_secs_f64();
        let mut c = core::Core::open(Path::new(&pack)).unwrap();
        let gpu = c.gpu;
        let src = job::separate_all(&mut c, &l, &r, &mut |_| {}, &|| false).unwrap();
        let t_sep = t0.elapsed().as_secs_f64() - t_dec;
        let k = kicks_of_sources(&l, &r, &src);
        let all = t0.elapsed().as_secs_f64();
        let secs = |h: &Hit| (h.frame as f64 * HOP as f64 + NFFT as f64 / 2.0) / SR;
        let wall: Vec<f64> = k.hits.iter().map(secs).filter(|t| (10.0..55.0).contains(t)).collect();
        let mut ioi: Vec<f64> = wall.windows(2).map(|w| w[1] - w[0]).collect();
        ioi.sort_by(|a, b| a.total_cmp(b));
        let x: Vec<f64> = l.iter().zip(&r).map(|(a, b)| 0.5 * (*a as f64 + *b as f64)).collect();
        let mut st = crate::audio::hpss_native::OnsetStream::new(44_100, 0);
        st.finish(x.len(), &mut |at, a, b| {
            a.copy_from_slice(&x[at..at + a.len()]);
            b.copy_from_slice(&x[at..at + b.len()]);
        });
        let env = st.envelope();
        let beat: Vec<usize> = k.hits.iter().filter(|h| (55.0..95.0).contains(&secs(h))).map(|h| h.frame as usize).collect();
        let caught = beat.iter().filter(|&&f| env[f.saturating_sub(3)..(f + 4).min(env.len())].iter().any(|&v| v >= 0.3)).count();
        eprintln!(
            "{} kicks; {:.0} s of sound: decode {:.1} s, sources {:.1} s ({}), all {:.1} s; 10-55 s: {} kicks, IOI median {:.3} s; 55-95 s: v6 catches {}/{}",
            k.hits.len(), l.len() as f64 / SR, t_dec, t_sep, if gpu { "GPU" } else { "CPU" }, all,
            wall.len(), ioi.get(ioi.len() / 2).copied().unwrap_or(0.0), caught, beat.len()
        );
    }

    /// A render's lists of a real track, made as the converter makes them
    /// (`make` on the calling thread), with the time and the process's peak
    /// memory. By hand, with a pack root where nothing is kept yet:
    ///   AURA_SPATIAL_DIR=<root> AURA_SPATIAL_FILE=<track>
    ///   cargo test --profile fast --bins netkick::tests::a_real_render -- --ignored --nocapture
    #[test]
    #[ignore]
    fn a_real_render() {
        let Some(file) = std::env::var_os("AURA_SPATIAL_FILE") else { return };
        let path = file.to_string_lossy().to_string();
        let before = crate::audio::memory::peak_working_set_mb();
        let t0 = std::time::Instant::now();
        let slot = Slot::default();
        let (_, from) = make(&path, &slot, &|| false, true).unwrap();
        let count = |k: Option<&Kicks>| k.map_or(0, |k| k.hits.len());
        eprintln!(
            "{} kicks, {} snare/hats hits ({from}) in {:.1} s; peak working set {} MB (before {} MB)",
            count(slot.ready()), count(slot.pieces()), t0.elapsed().as_secs_f64(),
            crate::audio::memory::peak_working_set_mb(), before
        );
    }

    fn some_lists() -> Lists {
        Lists {
            kicks: Kicks { hits: vec![Hit { frame: 3, conf: 0.9 }] },
            pieces: Kicks { hits: vec![Hit { frame: 40, conf: 0.6 }, Hit { frame: 41, conf: 0.7 }] },
        }
    }

    #[test]
    fn the_lists_beside_the_source_are_read_without_decoding_while_the_file_is_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a song.flac");
        std::fs::write(&src, b"some sound").unwrap();
        let file = save_beside(&src, 0x1234, &some_lists(), "drumsep").unwrap();
        assert_eq!(file, dir.path().join("a song.flac.hp_kit.json"));
        assert_eq!(beside_unchanged(&src), Some(some_lists()));
        assert_eq!(known(src.to_str().unwrap()), Some(some_lists()));
        // another file of the same name (another extension) has none
        let other = dir.path().join("a song.mp3");
        std::fs::write(&other, b"some sound").unwrap();
        assert_eq!(beside_unchanged(&other), None);
        // the file changed: the content key decides (the worker decodes it)
        std::fs::write(&src, b"some other sound").unwrap();
        assert_eq!(beside_unchanged(&src), None);
        assert_eq!(beside_of_content(&src, 0x1234).map(|k| k.0), Some(some_lists()));
        assert_eq!(beside_of_content(&src, 0x1235).map(|k| k.0), None);
        // another version of the lists is made again
        let text = std::fs::read_to_string(&file).unwrap().replace(&format!("\"algo\":{ALGO}"), "\"algo\":1");
        let tmp = dir.path().join("edited");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert_eq!(beside_of_content(&src, 0x1234).map(|k| k.0), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_lists_beside_the_source_are_hidden_and_made_again_over_the_hidden_file() {
        use std::os::windows::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a song.flac");
        std::fs::write(&src, b"some sound").unwrap();
        let hidden = |f: &Path| std::fs::metadata(f).unwrap().file_attributes() & winapi::um::winnt::FILE_ATTRIBUTE_HIDDEN != 0;
        let file = save_beside(&src, 0x1234, &some_lists(), "drumsep").unwrap();
        assert!(hidden(&file));
        save_beside(&src, 0x1235, &some_lists(), "bands").unwrap();
        assert!(hidden(&file));
        assert_eq!(beside_of_content(&src, 0x1235).map(|k| k.0), Some(some_lists()));
        assert!(!dir.path().join("a song.flac.hp_kit.part").exists());
    }

    #[test]
    fn the_queue_puts_the_waited_for_track_first_and_the_left_one_after() {
        let mut q = Queue::default();
        q.push("next1", false);
        q.push("next2", false);
        q.push("now", true);
        // a request without the listener does not make it "later"
        q.push("now", false);
        assert_eq!(q.pop(), Some(("now".into(), true)));
        assert_eq!(q.pop(), Some(("next2".into(), false)));
        q.push("a", true);
        q.push("b", true);
        q.demote("a");
        assert_eq!(q.pop(), Some(("b".into(), true)));
        assert_eq!(q.pop(), Some(("a".into(), false)));
        assert_eq!(q.pop(), Some(("next1".into(), false)));
        assert_eq!(q.pop(), None);
        // a stopped job comes after the rest, once
        q.push("x", false);
        q.push_back_later("stopped");
        q.push_back_later("stopped");
        q.push_back_later("x");
        assert_eq!(q.later, ["x", "stopped"]);
        for i in 0..5 {
            q.push(&format!("n{i}"), false);
        }
        assert_eq!(q.later.len(), LATER_MAX);
    }

    #[test]
    fn a_slot_tells_its_progress_and_who_waits_for_it() {
        let slot = Arc::new(Slot::default());
        slot.begin();
        slot.on_gpu(true);
        assert_eq!(slot.progress().stage, "decode");
        slot.step("stems", 0.5);
        let p = slot.progress();
        assert_eq!(p.stage, "stems");
        assert!((p.done - (0.03 + 0.39 * 0.5)).abs() < 1e-6, "{p:?}");
        slot.step("stems", 0.25);
        assert!(slot.progress().done >= p.done, "never backwards");
        slot.step("kit", 1.0);
        assert!((slot.progress().done - 1.0).abs() < 1e-6);
        let woke = Arc::new(AtomicBool::new(false));
        let w = woke.clone();
        slot.when_settled(move || w.store(true, Ordering::Release));
        slot.fill(Kicks::default());
        assert!(!slot.settled() && !woke.load(Ordering::Acquire), "the kicks alone do not settle it");
        slot.fill_pieces(Kicks::default());
        slot.settle();
        assert!(woke.load(Ordering::Acquire));
        assert_eq!(slot.progress().done, 1.0);
        // settled already: at once
        let w2 = Arc::new(AtomicBool::new(false));
        let w = w2.clone();
        slot.when_settled(move || w.store(true, Ordering::Release));
        assert!(w2.load(Ordering::Acquire));
        // a failed one settles too
        let failed = Slot::default();
        failed.fail("no room".into());
        assert!(failed.settled() && failed.ready().is_none());
        assert_eq!(failed.failed(), Some("no room"));
    }

    #[test]
    fn numpy_percentile() {
        let v: Vec<f64> = (0..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 99.5), 99.5);
        assert_eq!(percentile(&[1.0, 3.0], 50.0), 2.0);
    }

    /// A start plays on the lists in parts once the network is known fast
    /// enough and a cover holds its warm-up and first seconds — the whole
    /// run's, or a region's for a start far in; near the end, as far as the
    /// track goes.
    #[test]
    fn a_start_plays_on_the_parts_once_they_cover_it() {
        use super::super::progressive::Cover;
        let slot = Slot::default();
        let total = 5000usize;
        let c = |from: u32, upto: u32| Cover { from, upto, kicks: vec![], pieces: vec![] };
        slot.want_s(0.0);
        slot.publish(0, c(0, list_frame(START_COVER_S)), total);
        assert!(!slot.startable(), "the network's pace not known yet");
        slot.set_early();
        assert!(slot.startable());
        slot.want_s(30.0);
        assert!(!slot.startable());
        assert_eq!(slot.reading(), Some(list_frame(30.0 - START_BACK_S) as usize));
        slot.publish(1, c(list_frame(25.0), list_frame(34.0)), total);
        assert!(slot.startable());
        let end_s = total as f64 / FPS;
        slot.want_s(end_s - 1.0);
        assert!(!slot.startable());
        slot.publish(2, c(list_frame(end_s - 6.0), total as u32), total);
        assert!(slot.startable());
        // the detector's frames a cover holds whole, at 44.1 k and 96 k
        for sr in [44_100u32, 96_000] {
            let (lo, hi) = det_span(100, 900, sr);
            for j in 0..1200u32 {
                let f = frame_at(j, sr);
                if (lo..hi).contains(&f) {
                    assert!((100..900).contains(&j), "{sr}: list frame {j} lands on {f} in [{lo}, {hi})");
                }
            }
            // most of the cover's 800 list frames, in the detector's frames
            let hop = crate::audio::hpss_native::stft_params(sr).1;
            assert!((hi - lo) as f64 * hop as f64 / sr as f64 > 700.0 * HOP as f64 / SR, "{sr}: [{lo}, {hi})");
        }
    }
}
