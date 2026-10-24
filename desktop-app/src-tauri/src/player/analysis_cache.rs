//! A track's whole-track analysis, remembered.
//!
//! A live track's first sound waits for what needs the whole track: the
//! decode, the source statistics, DC removal and declip
//! (`prepare::source_head`), then the 4× true peak and the output limiter's
//! decision on what they left (`chain::source_level`) and the quick level's
//! clip rail (`declip::plateau_rail`). Declip alone took 1.5 s on a clipped
//! master. All of it is a pure function of the file as decoded and a few
//! rack switches, so it is kept per file in `player-cache\analysis`: the
//! head's results, the samples declip wrote — the whole of what it changed,
//! every other sample comes back bit-identical — and the levels. The next
//! start of the track decodes it, removes DC by the same code and puts
//! declip's samples back: the same track to the bit, without the analysis.
//!
//! A record is keyed by the file as it is (path, size, modification time),
//! the switches the head reads, the app's version and `VERSION`; anything
//! else misses. One that cannot be read is computed past and written again:
//! the cache never fails a start. The records are kept under `MAX_BYTES`,
//! the least recently used going first.
//!
//! A worker below normal priority fills the cache ahead for the list
//! (`spawn_worker`): one track at a time, the next ones first, out of the
//! way of a start (`start_begins`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::audio::converter::dsp::lab::{stats::SourceStats, LabOutcome};
use crate::audio::converter::pipeline::prepare::{self, SourceHead};
use crate::audio::converter::types::ConvertSettings;

/// Bump on any change to what `prepare::source_head` computes (the
/// statistics, the hot spans, DC removal, declip), to `chain::source_level`
/// or to `declip::plateau_rail`: the records made before it then miss.
/// `tests::the_head_of_a_fixed_track_is_unchanged` fails when their output
/// moves, as a reminder.
pub const VERSION: u32 = 2;
const MAGIC: &[u8; 8] = b"AURA-TA1";
const SUFFIX: &str = ".track-analysis";
/// The records on disk are kept under this, the least recently used going
/// first (a record is 0.1–0.3 MB; a heavily clipped track's up to ~2 MB).
pub const MAX_BYTES: u64 = 256 << 20;

/// The record's key: None when the file could not be looked at.
pub fn key(source_id: &str, eng: &ConvertSettings) -> Option<String> {
    if source_id.is_empty() {
        return None;
    }
    Some(format!(
        "{}|app {}|v{}|declip {}|ahr {}|isp {}|iir {}",
        source_id,
        env!("CARGO_PKG_VERSION"),
        VERSION,
        eng.lab.declip,
        eng.lab.adaptive_headroom,
        eng.lab.isp,
        eng.iir_dc_blocking
    ))
}

/// What a record holds: `SourceHead` and declip's samples, with the levels
/// and the clip rail found on the result.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub frames: usize,
    pub rate: u32,
    pub stats: Option<StatsRec>,
    pub tags: Vec<(String, String)>,
    pub chain: Vec<&'static str>,
    pub outcomes: Vec<(&'static str, &'static str, String)>,
    pub declip_l: Vec<(usize, usize)>,
    pub declip_r: Vec<(usize, usize)>,
    pub hot_l: Vec<(usize, usize)>,
    pub hot_r: Vec<(usize, usize)>,
    pub written_l: Vec<(usize, usize)>,
    pub written_r: Vec<(usize, usize)>,
    /// Declip's samples, the spans of `written_*` one after another.
    pub values_l: Vec<f64>,
    pub values_r: Vec<f64>,
    /// `chain::source_level` on the result: (ceiling dBTP, ISP) → (true
    /// peak, the limiter may hold locally).
    pub levels: Vec<(f64, bool, f64, bool)>,
    /// `declip::plateau_rail` on the result, once asked for.
    pub rail: Option<Option<f64>>,
    /// The static DC offsets (L, R) the head took away; None with the 2 Hz
    /// high-pass. A start that streams the track as it is decoded takes
    /// them away again from here.
    pub dc: Option<(f64, f64)>,
    /// The result's largest sample magnitude and how many of its samples
    /// are past full scale (the status panel's source peak), once asked for.
    pub src_peak: Option<(f64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatsRec {
    pub peak_lin: f64,
    pub peak_dbfs: f64,
    pub hist: Vec<u64>,
    pub grid16: bool,
    pub enob: Option<f64>,
}

impl Record {
    /// A record of `head`, run on `l`/`r` (which hold declip's samples).
    /// None when it holds a word this file format has no place for.
    pub fn new(head: &SourceHead, l: &[f64], r: &[f64], rate: u32) -> Option<Record> {
        let take = |ch: &[f64], spans: &[(usize, usize)]| -> Option<Vec<f64>> {
            let mut v = Vec::new();
            for &(a, b) in spans {
                v.extend_from_slice(ch.get(a..b)?);
            }
            Some(v)
        };
        let rec = Record {
            frames: l.len(),
            rate,
            stats: head.lab_stats.as_ref().map(|s| StatsRec {
                peak_lin: s.peak_lin,
                peak_dbfs: s.peak_dbfs,
                hist: s.hist.clone(),
                grid16: s.grid16,
                enob: s.enob,
            }),
            tags: head.lab_tags.clone(),
            chain: head.lab_chain.clone(),
            outcomes: head.lab_outcomes.iter().map(|o| (o.feature, o.level, o.text.clone())).collect(),
            declip_l: head.declip_spans_l.clone(),
            declip_r: head.declip_spans_r.clone(),
            hot_l: head.hot_spans_l.clone(),
            hot_r: head.hot_spans_r.clone(),
            written_l: head.declip_written_l.clone(),
            written_r: head.declip_written_r.clone(),
            values_l: take(l, &head.declip_written_l)?,
            values_r: take(r, &head.declip_written_r)?,
            levels: Vec::new(),
            rail: None,
            dc: head.dc,
            src_peak: None,
        };
        let words_known = rec.chain.iter().chain(rec.outcomes.iter().flat_map(|o| [&o.0, &o.1])).all(|w| known(w).is_some());
        words_known.then_some(rec)
    }

    /// The head as `prepare::source_head` returned it.
    pub fn head(&self) -> SourceHead {
        SourceHead {
            lab_stats: self.stats.as_ref().map(|s| SourceStats {
                peak_lin: s.peak_lin,
                peak_dbfs: s.peak_dbfs,
                hist: s.hist.clone(),
                grid16: s.grid16,
                enob: s.enob,
            }),
            lab_tags: self.tags.clone(),
            lab_chain: self.chain.clone(),
            lab_outcomes: self
                .outcomes
                .iter()
                .map(|(feature, level, text)| LabOutcome { feature, level, text: text.clone() })
                .collect(),
            declip_spans_l: self.declip_l.clone(),
            declip_spans_r: self.declip_r.clone(),
            hot_spans_l: self.hot_l.clone(),
            hot_spans_r: self.hot_r.clone(),
            declip_written_l: self.written_l.clone(),
            declip_written_r: self.written_r.clone(),
            dc: self.dc,
        }
    }

    /// Declip's samples, back where it wrote them.
    pub fn apply(&self, l: &mut [f64], r: &mut [f64]) {
        let put = |ch: &mut [f64], spans: &[(usize, usize)], values: &[f64]| {
            let mut at = 0;
            for &(a, b) in spans {
                ch[a..b].copy_from_slice(&values[at..at + (b - a)]);
                at += b - a;
            }
        };
        put(l, &self.written_l, &self.values_l);
        put(r, &self.written_r, &self.values_r);
    }

    /// Declip's samples among frames `at..at + l.len()` of the track, put
    /// back as `apply` puts them: for a track that comes in pieces, in order
    /// (`Patcher`).
    pub fn patcher(&self) -> Patcher<'_> {
        Patcher { rec: self, span: [0, 0], value: [0, 0] }
    }

    pub fn level(&self, target_db: f64, isp: bool) -> Option<(f64, bool)> {
        self.levels
            .iter()
            .find(|l| l.0.to_bits() == target_db.to_bits() && l.1 == isp)
            .map(|l| (l.2, l.3))
    }

    /// Every span inside the track and in order, the values as many as the
    /// spans hold: what `apply` and the head's readers rely on.
    fn sound(&self) -> bool {
        let fits = |spans: &[(usize, usize)]| {
            let mut end = 0;
            spans.iter().all(|&(a, b)| {
                let ok = a >= end && a <= b && b <= self.frames;
                end = b;
                ok
            })
        };
        let held = |spans: &[(usize, usize)]| spans.iter().map(|&(a, b)| b - a).sum::<usize>();
        [&self.declip_l, &self.declip_r, &self.hot_l, &self.hot_r, &self.written_l, &self.written_r]
            .iter()
            .all(|s| fits(s))
            && held(&self.written_l) == self.values_l.len()
            && held(&self.written_r) == self.values_r.len()
            && self.stats.as_ref().map_or(true, |s| s.hist.len() == 65536)
    }

    fn encode(&self, key: &str) -> Vec<u8> {
        let mut w = W(Vec::with_capacity(64 * 1024 + 8 * (self.values_l.len() + self.values_r.len())));
        w.0.extend_from_slice(MAGIC);
        w.var(VERSION as u64);
        w.str(key);
        w.var(self.frames as u64);
        w.var(self.rate as u64);
        match &self.stats {
            None => w.u8(0),
            Some(s) => {
                w.u8(1);
                w.f64(s.peak_lin);
                w.f64(s.peak_dbfs);
                w.hist(&s.hist);
                w.u8(s.grid16 as u8);
                match s.enob {
                    None => w.u8(0),
                    Some(e) => {
                        w.u8(1);
                        w.f64(e);
                    }
                }
            }
        }
        w.var(self.tags.len() as u64);
        for (k, v) in &self.tags {
            w.str(k);
            w.str(v);
        }
        w.var(self.chain.len() as u64);
        for c in &self.chain {
            w.str(c);
        }
        w.var(self.outcomes.len() as u64);
        for (f, l, t) in &self.outcomes {
            w.str(f);
            w.str(l);
            w.str(t);
        }
        for s in [&self.declip_l, &self.declip_r, &self.hot_l, &self.hot_r, &self.written_l, &self.written_r] {
            w.spans(s);
        }
        for v in [&self.values_l, &self.values_r] {
            w.var(v.len() as u64);
            v.iter().for_each(|&x| w.f64(x));
        }
        w.var(self.levels.len() as u64);
        for &(t, isp, tp, local) in &self.levels {
            w.f64(t);
            w.u8(isp as u8);
            w.f64(tp);
            w.u8(local as u8);
        }
        match self.rail {
            None => w.u8(0),
            Some(None) => w.u8(1),
            Some(Some(x)) => {
                w.u8(2);
                w.f64(x);
            }
        }
        match self.dc {
            None => w.u8(0),
            Some((l, r)) => {
                w.u8(1);
                w.f64(l);
                w.f64(r);
            }
        }
        match self.src_peak {
            None => w.u8(0),
            Some((peak, overs)) => {
                w.u8(1);
                w.f64(peak);
                w.var(overs);
            }
        }
        let sum = fnv64(&w.0);
        w.0.extend_from_slice(&sum.to_le_bytes());
        w.0
    }

    /// A record read back: None unless it is whole, of this format and
    /// version, made for `key` and sound.
    fn decode(b: &[u8], key: &str) -> Option<Record> {
        let (body, sum) = b.split_at(b.len().checked_sub(8)?);
        if u64::from_le_bytes(sum.try_into().ok()?) != fnv64(body) {
            return None;
        }
        let mut r = Rd { b: body, i: 0 };
        if r.take(8)? != MAGIC || r.var()? != VERSION as u64 || r.str()? != key {
            return None;
        }
        let frames = r.var()? as usize;
        let rate = u32::try_from(r.var()?).ok()?;
        let stats = match r.u8()? {
            0 => None,
            1 => Some(StatsRec {
                peak_lin: r.f64()?,
                peak_dbfs: r.f64()?,
                hist: r.hist()?,
                grid16: r.u8()? != 0,
                enob: match r.u8()? {
                    0 => None,
                    _ => Some(r.f64()?),
                },
            }),
            _ => return None,
        };
        let mut tags = Vec::new();
        for _ in 0..r.count()? {
            tags.push((r.str()?, r.str()?));
        }
        let mut chain = Vec::new();
        for _ in 0..r.count()? {
            chain.push(known(&r.str()?)?);
        }
        let mut outcomes = Vec::new();
        for _ in 0..r.count()? {
            outcomes.push((known(&r.str()?)?, known(&r.str()?)?, r.str()?));
        }
        let mut spans = Vec::new();
        for _ in 0..6 {
            spans.push(r.spans()?);
        }
        let mut values = Vec::new();
        for _ in 0..2 {
            let n = r.count()?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(r.f64()?);
            }
            values.push(v);
        }
        let mut levels = Vec::new();
        for _ in 0..r.count()? {
            levels.push((r.f64()?, r.u8()? != 0, r.f64()?, r.u8()? != 0));
        }
        let rail = match r.u8()? {
            0 => None,
            1 => Some(None),
            2 => Some(Some(r.f64()?)),
            _ => return None,
        };
        let dc = match r.u8()? {
            0 => None,
            1 => Some((r.f64()?, r.f64()?)),
            _ => return None,
        };
        let src_peak = match r.u8()? {
            0 => None,
            1 => Some((r.f64()?, r.var()?)),
            _ => return None,
        };
        if r.i != body.len() {
            return None;
        }
        let mut spans = spans.into_iter();
        let mut values = values.into_iter();
        let rec = Record {
            frames,
            rate,
            stats,
            tags,
            chain,
            outcomes,
            declip_l: spans.next()?,
            declip_r: spans.next()?,
            hot_l: spans.next()?,
            hot_r: spans.next()?,
            written_l: spans.next()?,
            written_r: spans.next()?,
            values_l: values.next()?,
            values_r: values.next()?,
            levels,
            rail,
            dc,
            src_peak,
        };
        rec.sound().then_some(rec)
    }
}

/// `Record::apply` on a track that comes in pieces, front to back.
pub struct Patcher<'a> {
    rec: &'a Record,
    /// Per channel: the first span not yet wholly behind, and where its
    /// values start.
    span: [usize; 2],
    value: [usize; 2],
}

impl Patcher<'_> {
    /// The piece of frames `at..at + l.len()`; pieces come in order.
    pub fn apply(&mut self, at: usize, l: &mut [f64], r: &mut [f64]) {
        for (c, ch) in [l, r].into_iter().enumerate() {
            let (spans, values) =
                if c == 0 { (&self.rec.written_l, &self.rec.values_l) } else { (&self.rec.written_r, &self.rec.values_r) };
            let end = at + ch.len();
            while let Some(&(a, b)) = spans.get(self.span[c]) {
                if a >= end {
                    break;
                }
                let (from, to) = (a.max(at), b.min(end));
                if from < to {
                    let v = self.value[c] + (from - a);
                    ch[from - at..to - at].copy_from_slice(&values[v..v + (to - from)]);
                }
                if b > end {
                    break;
                }
                self.value[c] += b - a;
                self.span[c] += 1;
            }
        }
    }
}

/// The words the head's chain and outcomes are made of, as the static
/// strings its types hold. A word not here is not stored: the next start
/// computes the track again.
fn known(w: &str) -> Option<&'static str> {
    ["DC", "DECLIP", "AHR", "ISP", "SUB", "none", "ok", "warn", "fail"].into_iter().find(|k| *k == w)
}

fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn var(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.var(s.len() as u64);
        self.0.extend_from_slice(s.as_bytes());
    }
    /// Spans in order: each start after the end before it, and its length.
    fn spans(&mut self, s: &[(usize, usize)]) {
        self.var(s.len() as u64);
        let mut end = 0usize;
        for &(a, b) in s {
            self.var(a.wrapping_sub(end) as u64);
            self.var(b.wrapping_sub(a) as u64);
            end = b;
        }
    }
    /// A histogram: runs of empty bins as a 0 and their count.
    fn hist(&mut self, h: &[u64]) {
        self.var(h.len() as u64);
        let mut i = 0;
        while i < h.len() {
            if h[i] == 0 {
                let run = h[i..].iter().take_while(|&&x| x == 0).count();
                self.var(0);
                self.var(run as u64);
                i += run;
            } else {
                self.var(h[i]);
                i += 1;
            }
        }
    }
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn var(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let x = self.u8()?;
            v |= ((x & 0x7f) as u64) << shift;
            if x & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    /// A count of items still to come: never more than bytes are left.
    fn count(&mut self) -> Option<usize> {
        let n = self.var()? as usize;
        (n <= self.b.len() - self.i).then_some(n)
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(u64::from_le_bytes(self.take(8)?.try_into().ok()?)))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.count()?;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn spans(&mut self) -> Option<Vec<(usize, usize)>> {
        let n = self.count()?;
        let mut v = Vec::with_capacity(n);
        let mut end = 0usize;
        for _ in 0..n {
            let a = end.checked_add(self.var()? as usize)?;
            let b = a.checked_add(self.var()? as usize)?;
            v.push((a, b));
            end = b;
        }
        Some(v)
    }
    fn hist(&mut self) -> Option<Vec<u64>> {
        let n = self.var()? as usize;
        if n > 1 << 20 {
            return None;
        }
        let mut h = Vec::with_capacity(n);
        while h.len() < n {
            match self.var()? {
                0 => {
                    let run = self.var()? as usize;
                    if run == 0 || h.len() + run > n {
                        return None;
                    }
                    h.resize(h.len() + run, 0);
                }
                x => h.push(x),
            }
        }
        Some(h)
    }
}

// ── The records on disk ─────────────────────────────────────────────────────

#[cfg_attr(test, allow(dead_code))]
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// One writer at a time: a level added to a record reads it, adds and
/// writes it back.
static WRITE: Mutex<()> = Mutex::new(());

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The running player's records: in `cache_dir\analysis`, trimmed on a
/// thread of its own. Unit tests never touch the user's folder.
#[cfg_attr(test, allow(dead_code))]
pub fn init(cache_dir: &Path) {
    let dir = cache_dir.join("analysis");
    let _ = std::fs::create_dir_all(&dir);
    if DIR.set(dir.clone()).is_ok() {
        let _ = std::thread::Builder::new().name("aura-analysis-prune".into()).spawn(move || {
            let (n, bytes) = prune(&dir, MAX_BYTES);
            if n > 0 {
                crate::aelog!(
                    "[PLAYER] cache: removed {} track analysis record(s), {:.1} MB (kept under {} MB)",
                    n,
                    bytes as f64 / (1024.0 * 1024.0),
                    MAX_BYTES >> 20
                );
            }
        });
    }
}

fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_DIR.with(|d| d.borrow().clone())
    }
    #[cfg(not(test))]
    {
        DIR.get().cloned()
    }
}

#[cfg(test)]
thread_local! {
    /// Heads this thread took from a record (tests).
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Heads this thread has taken from a record so far (tests).
#[cfg(test)]
pub fn hits() -> usize {
    HITS.with(|h| h.get())
}

/// Run `f` with the records in `d` on this thread (tests).
#[cfg(test)]
pub fn with_dir<R>(d: &Path, f: impl FnOnce() -> R) -> R {
    let _ = std::fs::create_dir_all(d);
    TEST_DIR.with(|t| *t.borrow_mut() = Some(d.to_path_buf()));
    let r = f();
    TEST_DIR.with(|t| *t.borrow_mut() = None);
    r
}

fn file_of(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{:016x}{}", fnv64(key.as_bytes()), SUFFIX))
}

/// The record for `key`, when one is on disk and sound. A hit marks it used.
pub fn load(key: &str) -> Option<Record> {
    let f = file_of(&dir()?, key);
    let b = std::fs::read(&f).ok()?;
    let rec = Record::decode(&b, key)?;
    if let Ok(h) = std::fs::File::options().write(true).open(&f) {
        let _ = h.set_modified(SystemTime::now());
    }
    Some(rec)
}

/// Keep `rec` as the record for `key` (written whole, then renamed in).
pub fn store(key: &str, rec: &Record) {
    let Some(d) = dir() else { return };
    let _w = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let f = file_of(&d, key);
    let tmp = f.with_extension("tmp");
    let ok = std::fs::write(&tmp, rec.encode(key)).is_ok() && std::fs::rename(&tmp, &f).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Change the record for `key`, if there is one.
fn update(key: &str, change: impl FnOnce(&mut Record)) {
    let Some(d) = dir() else { return };
    let _w = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let f = file_of(&d, key);
    let Some(mut rec) = std::fs::read(&f).ok().and_then(|b| Record::decode(&b, key)) else { return };
    change(&mut rec);
    let tmp = f.with_extension("tmp");
    if !(std::fs::write(&tmp, rec.encode(key)).is_ok() && std::fs::rename(&tmp, &f).is_ok()) {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Trim the records in `dir` to at most `max_bytes`, the least recently used
/// first. Only this cache's files are touched. Returns the files removed and
/// the bytes freed.
pub fn prune(dir: &Path, max_bytes: u64) -> (usize, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut files: Vec<(PathBuf, SystemTime, u64)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.ends_with(SUFFIX) || n.ends_with(".tmp")
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then_some(())?;
            Some((e.path(), m.modified().ok()?, m.len()))
        })
        .collect();
    files.sort_by_key(|f| f.1);
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    let (mut n, mut freed) = (0, 0);
    for (p, _, len) in &files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(p).is_ok() {
            total -= len;
            n += 1;
            freed += len;
        }
    }
    (n, freed)
}

// ── What a start asks for ───────────────────────────────────────────────────

/// `prepare::source_head` on a decoded track, `l`/`r` left as it leaves
/// them: from the record of an earlier start when there is one — DC removed
/// by the same code, declip's samples put back — else computed and kept.
/// With the record's key (None: no cache, or the file could not be looked
/// at), for the levels found later on the same track.
pub fn source_head(
    source_id: &str,
    l: &mut Vec<f64>,
    r: &mut Vec<f64>,
    rate: u32,
    lossy: Option<&str>,
    eng: &ConvertSettings,
    cancel: &AtomicBool,
) -> Result<(SourceHead, Option<String>), String> {
    let key = dir().and_then(|_| key(source_id, eng));
    if let Some(rec) = key.as_deref().and_then(load).filter(|rec| rec.frames == l.len() && rec.rate == rate) {
        prepare::remove_dc(l, r, rate, eng);
        rec.apply(l, r);
        #[cfg(test)]
        HITS.with(|h| h.set(h.get() + 1));
        crate::aelog!(
            "[PLAYER] track analysis from the cache: statistics, DC, declip ({} samples put back)",
            rec.values_l.len() + rec.values_r.len()
        );
        return Ok((rec.head(), key));
    }
    let head = prepare::source_head(l, r, rate, lossy, eng, cancel)?;
    if let Some(k) = key.as_deref() {
        if let Some(mut rec) = Record::new(&head, l, r, rate) {
            // What the status panel shows of it: a start that streams the
            // track as it is decoded has no track to scan.
            rec.src_peak = Some(peak_overs(l, r));
            store(k, &rec);
        }
    }
    Ok((head, key))
}

/// `chain::source_level` of a track (`compute`) at ceiling `target_db` with
/// ISP `isp`: from its record, else computed and added to it.
pub fn level(key: Option<&str>, target_db: f64, isp: bool, compute: impl FnOnce() -> (f64, bool)) -> (f64, bool) {
    let Some(key) = key else { return compute() };
    if let Some(v) = load(key).and_then(|rec| rec.level(target_db, isp)) {
        return v;
    }
    let v = compute();
    update(key, |rec| {
        if rec.level(target_db, isp).is_none() {
            rec.levels.push((target_db, isp, v.0, v.1));
        }
    });
    v
}

/// The largest sample magnitude of a track and how many of its samples are
/// past full scale — the status panel's source peak — in pieces side by
/// side: the same two numbers a scan front to back finds.
pub fn peak_overs(l: &[f64], r: &[f64]) -> (f64, u64) {
    use rayon::prelude::*;
    let scan = |x: &[f64]| {
        let (mut peak, mut overs) = (0.0f64, 0u64);
        for &v in x {
            let a = v.abs();
            if a > peak {
                peak = a;
            }
            if a > 1.0 {
                overs += 1;
            }
        }
        (peak, overs)
    };
    l.par_chunks(1 << 16)
        .chain(r.par_chunks(1 << 16))
        .map(scan)
        .reduce(|| (0.0, 0), |a, b| (if b.0 > a.0 { b.0 } else { a.0 }, a.1 + b.1))
}

/// `declip::plateau_rail` of a track (`compute`): from its record, else
/// computed and added to it.
pub fn rail(key: Option<&str>, compute: impl FnOnce() -> Option<f64>) -> Option<f64> {
    let Some(key) = key else { return compute() };
    if let Some(v) = load(key).and_then(|rec| rec.rail) {
        return v;
    }
    let v = compute();
    update(key, |rec| rec.rail = Some(v));
    v
}

// ── The worker ahead ────────────────────────────────────────────────────────

/// Starts going on now (a listener waits for each).
static STARTS: AtomicUsize = AtomicUsize::new(0);
/// When the last start ended (ms since `epoch()`).
static LAST_START_MS: AtomicU64 = AtomicU64::new(0);
/// Up while the worker's track should stop: a start began, or the rack
/// changed under it.
static STOP: AtomicBool = AtomicBool::new(false);
static WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());
/// The worker waits this long after a start before it goes on: the first
/// sound's pre-roll and the full variant's work are left the machine.
const QUIET: Duration = Duration::from_secs(3);
/// With nothing to do, the worker looks at the list again this often.
const IDLE: Duration = Duration::from_secs(10);

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// A start going on: the worker stops its track and stays out of the way
/// until `QUIET` after the last one ends.
pub struct StartGuard(());

pub fn start_begins() -> StartGuard {
    STARTS.fetch_add(1, Ordering::AcqRel);
    STOP.store(true, Ordering::Release);
    StartGuard(())
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        // 0 stands for no start yet.
        LAST_START_MS.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Release);
        STARTS.fetch_sub(1, Ordering::AcqRel);
        wake();
    }
}

/// No start goes on and none ended within `QUIET`; else how long to wait.
fn quiet_for() -> Option<Duration> {
    if STARTS.load(Ordering::Acquire) > 0 {
        return Some(QUIET);
    }
    let since = epoch().elapsed().saturating_sub(Duration::from_millis(LAST_START_MS.load(Ordering::Acquire).saturating_sub(1)));
    (LAST_START_MS.load(Ordering::Acquire) > 0 && since < QUIET).then(|| QUIET - since)
}

/// The list or the rack changed: the worker looks again now.
pub fn wake() {
    let (m, c) = &WAKE;
    *m.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    c.notify_all();
}

/// The rack changed: the track the worker is on stops (its key is the old
/// rack's), and the list is looked at again.
pub fn settings_changed() {
    STOP.store(true, Ordering::Release);
    wake();
}

fn sleep_until_woken(d: Duration) {
    let (m, c) = &WAKE;
    let g = m.lock().unwrap_or_else(|e| e.into_inner());
    let seen = *g;
    let _ = c.wait_timeout_while(g, d, |n| *n == seen);
}

/// What the worker did with one track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    /// Its record holds everything a start asks for.
    Known,
    /// Analysed now.
    Analysed,
    /// Not for the worker (too long, a rate the stream does not take…).
    Skipped,
}

/// The worker's job: `list` gives the tracks to look at in order with the
/// rack each plays with; `analyse` does one (`chain::analyse_ahead`).
pub type ListFn = fn() -> Vec<(PathBuf, crate::player::settings::PlayerSettings)>;
pub type AnalyseFn = fn(&Path, &crate::player::settings::PlayerSettings, &AtomicBool) -> Result<Ahead, String>;

/// The worker, started once: one track at a time below normal priority on
/// a pool of two threads of its own (declip's two channels), the next
/// tracks first. It stops a track when a start begins or the rack changes
/// and comes back to it later.
pub fn spawn_worker(list: ListFn, analyse: AnalyseFn) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) || dir().is_none() {
        return;
    }
    let _ = std::thread::Builder::new().name("aura-pre-analysis".into()).spawn(move || {
        crate::player::source_stages::set_thread_priority(-1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .thread_name(|i| format!("aura-pre-analysis-{}", i))
            .start_handler(|_| crate::player::source_stages::set_thread_priority(-1))
            .build();
        let Ok(pool) = pool else { return };
        // The tracks done under their key and rack, so an idle look at the
        // list costs a file's metadata per track.
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            if let Some(d) = quiet_for() {
                std::thread::sleep(d.min(QUIET));
                continue;
            }
            let t0 = Instant::now();
            let (mut analysed, mut busy) = (0usize, false);
            for (path, s) in list() {
                if quiet_for().is_some() {
                    busy = true;
                    break;
                }
                let tag = format!("{}|{}", crate::player::chain::source_id(&path), s.source_key());
                if done.contains(&tag) {
                    continue;
                }
                STOP.store(false, Ordering::Release);
                let t = Instant::now();
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                match pool.install(|| crate::audio::logging::quietly(|| analyse(&path, &s, &STOP))) {
                    Ok(Ahead::Analysed) => {
                        analysed += 1;
                        crate::aelog!("[PRE] analysed {} in {:.2} s", name, t.elapsed().as_secs_f64());
                        done.insert(tag);
                    }
                    Ok(Ahead::Skipped) => {
                        crate::aelog!("[PRE] skipped {} (a start prepares it itself)", name);
                        done.insert(tag);
                    }
                    Ok(Ahead::Known) => {
                        done.insert(tag);
                    }
                    // Stopped for a start or a new rack: looked at again.
                    Err(_) if STOP.load(Ordering::Acquire) => {
                        busy = true;
                        break;
                    }
                    // Unreadable: not tried again this session.
                    Err(e) => {
                        crate::aelog!("[PRE] skipped {}: {}", name, e);
                        done.insert(tag);
                    }
                }
            }
            if analysed > 0 {
                crate::aelog!(
                    "[PRE] {} track(s) analysed in {:.1} s{}",
                    analysed,
                    t0.elapsed().as_secs_f64(),
                    if busy { ", stopped for a start or a new rack" } else { "" }
                );
                let _ = dir().map(|d| prune(&d, MAX_BYTES));
            }
            if !busy {
                sleep_until_woken(IDLE);
            }
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::audio::converter::types::ConvertSettings;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aura-analysis-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Four seconds of tones with DC on both channels, clipped stretches
    /// (declip repairs them) and a few overs — or none of that.
    pub(crate) fn fixture(clipped: bool) -> (Vec<f64>, Vec<f64>) {
        let rate = 44_100.0;
        let n = 4 * 44_100;
        let tone = |i: usize, f: f64, a: f64| a * (2.0 * std::f64::consts::PI * f * i as f64 / rate).sin();
        let mut l: Vec<f64> = (0..n).map(|i| tone(i, 440.0, 0.3) + tone(i, 4.0, 0.05) + 0.01).collect();
        let mut r: Vec<f64> = (0..n).map(|i| tone(i, 330.0, 0.3) - 0.008).collect();
        if clipped {
            for (i, x) in l.iter_mut().enumerate().take(150_000).skip(120_000) {
                *x = tone(i, 220.0, 1.4).clamp(-0.85, 0.85);
            }
            for (i, x) in r.iter_mut().enumerate().take(40_000).skip(30_000) {
                *x = tone(i, 110.0, 1.2).clamp(-0.85, 0.85);
            }
            for k in 0..8 {
                l[60_000 + k] = if k % 2 == 0 { 0.68 } else { -0.68 };
            }
        }
        (l, r)
    }

    fn eng(declip: bool, isp: bool, ahr: bool, iir: bool) -> ConvertSettings {
        let mut e = crate::player::settings::PlayerSettings::default().to_engine();
        e.lab.declip = declip;
        e.lab.isp = isp;
        e.lab.adaptive_headroom = ahr;
        e.iir_dc_blocking = iir;
        e
    }

    fn bits(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    /// Everything a head holds, as a record (which compares whole).
    fn as_record(h: &SourceHead, l: &[f64], r: &[f64]) -> Record {
        Record::new(h, l, r, 44_100).expect("a record")
    }

    /// Pieces of a track, front to back, of sizes that cross every span.
    fn in_pieces(n: usize, mut f: impl FnMut(std::ops::Range<usize>)) {
        let sizes = [1usize, 7, 4_096, 333, 16_384, 65_536, 2];
        let (mut at, mut k) = (0, 0);
        while at < n {
            let e = (at + sizes[k % sizes.len()]).min(n);
            f(at..e);
            at = e;
            k += 1;
        }
    }

    /// DC taken away piece by piece is `remove_dc` on the whole, to the bit:
    /// the static offsets it returns, and the 2 Hz high-pass carried across.
    #[test]
    fn dc_taken_away_in_pieces_is_remove_dc() {
        for clipped in [true, false] {
            for iir in [false, true] {
                let (l0, r0) = fixture(clipped);
                let e = eng(true, true, true, iir);
                let (mut wl, mut wr) = (l0.clone(), r0.clone());
                let offsets = prepare::remove_dc(&mut wl, &mut wr, 44_100, &e);
                assert_eq!(offsets.is_none(), iir, "offsets with the static removal only");
                let mut dc = match offsets {
                    Some((a, b)) => prepare::DcBlock::offsets(a, b),
                    None => prepare::DcBlock::high_pass(44_100),
                };
                let (mut pl, mut pr) = (l0.clone(), r0.clone());
                in_pieces(pl.len(), |g| dc.apply(&mut pl[g.clone()], &mut pr[g]));
                assert!(bits(&pl, &wl) && bits(&pr, &wr), "clipped {} iir {}", clipped, iir);
            }
        }
    }

    /// Declip's samples put back piece by piece are `Record::apply`'s.
    #[test]
    fn declips_samples_put_back_in_pieces_are_applys() {
        let (mut l, mut r) = fixture(true);
        let e = eng(true, true, true, false);
        let head = prepare::source_head(&mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
        let rec = as_record(&head, &l, &r);
        assert!(rec.written_l.len() > 3 && rec.written_r.len() > 3, "spans to cross");
        let (z, zr) = (vec![0.0; l.len()], vec![0.0; r.len()]);
        let (mut wl, mut wr) = (z.clone(), zr.clone());
        rec.apply(&mut wl, &mut wr);
        let (mut pl, mut pr) = (z, zr);
        let mut p = rec.patcher();
        in_pieces(pl.len(), |g| p.apply(g.start, &mut pl[g.clone()], &mut pr[g]));
        assert!(bits(&pl, &wl) && bits(&pr, &wr));
        assert_eq!(head.dc, rec.dc);
        assert!(rec.dc.is_some());
    }

    /// A record reads back as it was written; a flipped byte, a cut file,
    /// another key or another version do not read at all.
    #[test]
    fn a_record_reads_back_whole_or_not_at_all() {
        let (mut l, mut r) = fixture(true);
        let e = eng(true, true, true, false);
        let head = prepare::source_head(&mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
        assert!(head.lab_chain.contains(&"DC") && !head.declip_written_l.is_empty() && !head.declip_written_r.is_empty());
        let mut rec = as_record(&head, &l, &r);
        rec.levels.push((-0.5, true, 1.25, false));
        rec.rail = Some(Some(0.85));
        rec.src_peak = Some((1.0625, 4_321));
        let b = rec.encode("k");
        assert_eq!(Record::decode(&b, "k").as_ref(), Some(&rec));
        assert!(Record::decode(&b, "other").is_none());
        assert!(Record::decode(&b[..b.len() - 1], "k").is_none());
        for at in [0, 9, b.len() / 2, b.len() - 9] {
            let mut bad = b.clone();
            bad[at] ^= 0x10;
            assert!(Record::decode(&bad, "k").is_none(), "byte {} flipped", at);
        }
        let mut old = b.clone();
        old[8] = (VERSION + 1) as u8;
        assert!(Record::decode(&old, "k").is_none(), "another version");
        // The histogram packs its empty bins.
        assert!(b.len() < 64 * 1024 + 8 * (rec.values_l.len() + rec.values_r.len()), "{} bytes", b.len());
    }

    /// The track from a record is the track computed: DC removed, declip's
    /// samples back — to the bit — and the head the same, on a clipped and a
    /// clean track under racks with each switch the head reads.
    #[test]
    fn a_remembered_head_is_the_computed_one() {
        let d = tmp("head");
        with_dir(&d, || {
            for clipped in [true, false] {
                for e in [eng(true, true, true, false), eng(true, false, false, true), eng(false, true, true, false)] {
                    let what = format!("clipped {}, {}", clipped, key("f|1|2", &e).unwrap());
                    let (mut l0, mut r0) = fixture(clipped);
                    let want = prepare::source_head(&mut l0, &mut r0, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
                    let sid = format!("fixture-{}|1|2", clipped);
                    let before = hits();
                    let (mut l1, mut r1) = fixture(clipped);
                    let (miss, k1) = source_head(&sid, &mut l1, &mut r1, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
                    assert_eq!(hits(), before, "{}: the first start computes", what);
                    let (mut l2, mut r2) = fixture(clipped);
                    let (hit, k2) = source_head(&sid, &mut l2, &mut r2, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
                    assert_eq!(hits(), before + 1, "{}: the second start takes the record", what);
                    assert_eq!(k1, k2);
                    for (l, r, h, how) in [(&l1, &r1, &miss, "computed"), (&l2, &r2, &hit, "remembered")] {
                        assert!(bits(l, &l0) && bits(r, &r0), "{}: the {} track differs", what, how);
                        assert_eq!(as_record(h, l, r), as_record(&want, &l0, &r0), "{}: the {} head differs", what, how);
                    }
                }
            }
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Another switch the head reads, another file state, no file id: a miss.
    #[test]
    fn what_the_head_depends_on_is_in_the_key() {
        let e = eng(true, true, true, false);
        let k = key("p|10|20", &e).unwrap();
        for other in [
            key("p|10|21", &e),
            key("p|11|20", &e),
            key("q|10|20", &e),
            key("p|10|20", &eng(false, true, true, false)),
            key("p|10|20", &eng(true, false, true, false)),
            key("p|10|20", &eng(true, true, false, false)),
            key("p|10|20", &eng(true, true, true, true)),
        ] {
            assert_ne!(other.as_deref(), Some(k.as_str()));
        }
        assert!(k.contains(&format!("v{}", VERSION)) && k.contains(env!("CARGO_PKG_VERSION")));
        assert!(key("", &e).is_none());
        // The output rate and the filter are not read by the head.
        let mut e2 = e.clone();
        e2.out_rate = 705_600;
        e2.taps = 30_000_000;
        assert_eq!(key("p|10|20", &e2).as_deref(), Some(k.as_str()));
    }

    /// A record that cannot be read is computed past and written again; a
    /// record of another length is not used.
    #[test]
    fn a_bad_record_is_computed_again() {
        let d = tmp("bad");
        with_dir(&d, || {
            let e = eng(true, true, true, false);
            let (mut l0, mut r0) = fixture(true);
            let want = prepare::source_head(&mut l0, &mut r0, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
            let k = key("bad|1|2", &e).unwrap();
            std::fs::write(file_of(&d, &k), b"not a record").unwrap();
            let before = hits();
            let (mut l, mut r) = fixture(true);
            let (h, _) = source_head("bad|1|2", &mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
            assert_eq!(hits(), before);
            assert!(bits(&l, &l0) && bits(&r, &r0));
            assert_eq!(as_record(&h, &l, &r), as_record(&want, &l0, &r0));
            assert!(load(&k).is_some(), "written again");
            // The same key over a shorter decode (a file cut, same metadata).
            let (mut l, mut r) = fixture(true);
            l.truncate(100_000);
            r.truncate(100_000);
            let (mut lc, mut rc) = (l.clone(), r.clone());
            let want = prepare::source_head(&mut lc, &mut rc, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
            let (h, _) = source_head("bad|1|2", &mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
            assert_eq!(hits(), before, "not taken");
            assert!(bits(&l, &lc) && bits(&r, &rc));
            assert_eq!(as_record(&h, &l, &r), as_record(&want, &lc, &rc));
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A level and the rail are computed once per record and read back after.
    #[test]
    fn levels_and_the_rail_are_kept() {
        let d = tmp("levels");
        with_dir(&d, || {
            let e = eng(true, true, true, false);
            let (mut l, mut r) = fixture(true);
            let (_, k) = source_head("lv|1|2", &mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
            let k = k.unwrap();
            let calls = std::cell::Cell::new(0);
            let lvl = |t: f64, isp: bool| {
                level(Some(&k), t, isp, || {
                    calls.set(calls.get() + 1);
                    (t.abs() + if isp { 1.0 } else { 0.0 }, isp)
                })
            };
            assert_eq!(lvl(-0.5, true), (1.5, true));
            assert_eq!(lvl(-0.5, true), (1.5, true));
            assert_eq!(lvl(-3.0, true), (4.0, true));
            assert_eq!(lvl(-0.5, false), (0.5, false));
            assert_eq!(lvl(-3.0, true), (4.0, true));
            assert_eq!(calls.get(), 3);
            let rails = std::cell::Cell::new(0);
            for _ in 0..3 {
                let got = rail(Some(&k), || {
                    rails.set(rails.get() + 1);
                    Some(0.85)
                });
                assert_eq!(got, Some(0.85));
            }
            assert_eq!(rails.get(), 1);
            // Without a key: computed every time.
            assert_eq!(level(None, -0.5, true, || (9.0, false)), (9.0, false));
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Over the cap, the least recently used records go; other files stay.
    #[test]
    fn prune_drops_the_least_recently_used() {
        let d = tmp("prune");
        let t0 = SystemTime::now() - Duration::from_secs(3600);
        for i in 0..5u64 {
            let f = d.join(format!("{:016x}{}", i, SUFFIX));
            std::fs::write(&f, vec![0u8; 1000]).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(t0 + Duration::from_secs(i * 60)).unwrap();
        }
        std::fs::write(d.join("other.json"), vec![0u8; 5000]).unwrap();
        assert_eq!(prune(&d, 2500), (3, 3000));
        let mut left: Vec<String> =
            std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.sort();
        assert_eq!(left, [format!("{:016x}{}", 3, SUFFIX), format!("{:016x}{}", 4, SUFFIX), "other.json".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A start stops the worker's track and keeps it away until `QUIET`
    /// after the last start ends.
    #[test]
    fn a_start_keeps_the_worker_away() {
        STOP.store(false, Ordering::Release);
        let g = start_begins();
        assert!(STOP.load(Ordering::Acquire), "the track the worker is on stops");
        assert!(quiet_for().is_some());
        drop(g);
        let left = quiet_for().expect("quiet for a while after");
        assert!(left <= QUIET && left > QUIET - Duration::from_secs(1));
    }

    /// The head on a fixed clipped track: when this moves, what a record
    /// holds has changed — bump `VERSION` and set the new value here.
    #[test]
    fn the_head_of_a_fixed_track_is_unchanged() {
        let (mut l, mut r) = fixture(true);
        let e = eng(true, true, true, false);
        let head = prepare::source_head(&mut l, &mut r, 44_100, None, &e, &AtomicBool::new(false)).unwrap();
        let mut rec = as_record(&head, &l, &r);
        rec.levels.push((
            -0.5,
            true,
            crate::audio::converter::dsp::true_peak::measure_true_peak(&l, &r),
            crate::audio::converter::dsp::lab::isp::output_limit_is_local(&l, &r, 10f64.powf(-0.5 / 20.0), 44_100),
        ));
        rec.rail = Some(crate::audio::converter::dsp::lab::declip::plateau_rail(&l, &r));
        let h = fnv64(&rec.encode("fixed"));
        assert_eq!(h, 0x99aaca66dfc79fd1, "the fixed track's analysis changed ({:#018x}): bump VERSION and set this value", h);
    }

    /// What each whole-track step before a live track's first sound costs on
    /// real files (`AURA_L2_FILES`, separated by `;`).
    #[test]
    #[ignore]
    fn head_pieces_cost() {
        use crate::audio::converter::dsp::lab;
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let files = std::env::var("AURA_L2_FILES").expect("AURA_L2_FILES");
        for f in files.split(';').filter(|f| !f.is_empty()) {
            for round in 0..3 {
                let cancel = AtomicBool::new(false);
                let t = Instant::now();
                let a = crate::audio::converter::decode::decode_file(Path::new(f)).unwrap();
                let dec = ms(t);
                let (mut l, mut r) = (a.samples_l, a.samples_r);
                let t = Instant::now();
                let stats = lab::stats::analyze(&l, &r, a.sample_rate, &cancel).unwrap();
                let st = ms(t);
                let t = Instant::now();
                let _hot = (lab::isp::hot_spans(&l, 0), lab::isp::hot_spans(&r, 0));
                let hot = ms(t);
                let t = Instant::now();
                prepare::remove_dc(&mut l, &mut r, a.sample_rate, &eng(true, true, true, false));
                let dc = ms(t);
                let t = Instant::now();
                let rep = lab::declip::run(&mut l, &mut r, &stats, a.sample_rate, &cancel);
                let dcl = ms(t);
                let t = Instant::now();
                let tp = crate::audio::converter::dsp::true_peak::measure_true_peak_parallel(&l, &r);
                let local = lab::isp::output_limit_is_local_parallel(&l, &r, 10f64.powf(-0.5 / 20.0), a.sample_rate);
                let lvl = ms(t);
                let t = Instant::now();
                let rail = lab::declip::plateau_rail(&l, &r);
                let rl = ms(t);
                println!(
                    "[head] {} #{}: decode {:.0} | stats {:.0} | hot {:.0} | dc {:.0} | declip {:.0} ({}) | level {:.0} (tp {:.3} local {}) | rail {:.0} ({:?})",
                    Path::new(f).file_name().unwrap().to_string_lossy(),
                    round,
                    dec,
                    st,
                    hot,
                    dc,
                    dcl,
                    rep.as_ref().map(|r| format!("{} regions", r.regions_total)).unwrap_or_else(|e| format!("{:?}", e)),
                    lvl,
                    tp,
                    local,
                    rl,
                    rail
                );
            }
        }
    }
}
