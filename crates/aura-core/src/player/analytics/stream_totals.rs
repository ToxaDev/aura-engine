//! A stream's totals for the analyzer: the listening session (since tuned
//! in or ↺), the song playing and the songs heard, and how far the stream's
//! spectrum really reaches — its bandwidth.
//!
//! Fed by the live thread's stream tap (stream.rs) on the frames it
//! measures — S at the stream's rate, O at the output's — and kept here,
//! apart from the live session: a reopened device (BIT-PERFECT on or off, a
//! rack at another rate) starts a new live session, not a new listening
//! session; a new station does. The songs list goes on across stations: a
//! station's last song is cut short, its first one joined.
//!
//! The numbers are the files' own: integrated loudness and LRA from the same
//! 100 ms blocks and functions as the whole-track analysis
//! (`loudness::integrated_of`, `short_term_at`, `lra::lra_ebu`), the true
//! peak as `EburTruePeak`, DR as `DrLive` — per song only: a session of many
//! songs has no DR comparable to a track's. The short-term loudness
//! histogram counts the same values in the files' bins (`track::lufs_hist_bin`),
//! the stereo correlation sums L·R as the O pass does.
//!
//! Songs are cut where the session says songs begin (`RadioShared::songs`: a
//! new title, a chained Ogg stream's next logical stream) — S at frame b, O
//! at b·L. The edges are as good as the stream's metadata: approximate.
//!
//! The page reads it all as JSON (`/player/an/stream`), rebuilt once a second.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::{json, Value};

use crate::player::radio::RadioShared;
use super::dr::DrLive;
use super::loudness::{integrated_of, short_term_at, LoudnessMeter};
use super::lra::lra_ebu;
use super::peaks::{lin_to_db, EburTruePeak};
use super::track::{lufs_hist_bin, LUFS_HIST_BINS};

/// Songs kept in the list, newest first.
const SONGS_KEPT: usize = 30;

/// A column counts for the bandwidth when its mean power in 1–8 kHz is at
/// least this (dBFS-sine per bin: −90 dB) — silence and fades do not.
const LTAS_GATE: f64 = 1e-9;

/// Seconds of louder sound before the bandwidth is told.
const LTAS_MIN_S: f64 = 10.0;

/// A cut is where the spectrum stands this many dB above everything from
/// half a kilohertz above it to the top (a codec's lowpass is a cliff; the
/// music's own fall is gentle) …
const CLIFF_DB: f64 = 30.0;
/// … and its edge where it falls this far under the level just below it.
const EDGE_DB: f64 = 10.0;

/// The frames one wakeup of the live thread measured on a stream.
pub struct Frames<'a> {
    pub src_rate: u32,
    /// S's spectrogram frame and hop (the bandwidth's columns).
    pub n_fft: usize,
    pub hop: usize,
    pub out_rate: u32,
    /// The chain's factor: output index = source frame × l.
    pub l: u64,
    /// O's frames, chain output index `idx0` on.
    pub idx0: u64,
    pub o_l: &'a [f64],
    pub o_r: &'a [f64],
    /// S's frames, source frame `s_from` on.
    pub s_from: u64,
    pub s_l: &'a [f64],
    pub s_r: &'a [f64],
}

/// Loudness over a stretch whose rate may change: the meters' 100 ms
/// blocks in one row, the short-term values as they complete, and how many
/// of those fell in each bin of the files' loudness histogram.
struct Loud {
    meter: LoudnessMeter,
    rate: u32,
    seen: usize,
    subs: Vec<f64>,
    st: Vec<f64>,
    hist: Vec<u32>,
}

impl Loud {
    fn new(rate: u32) -> Loud {
        Loud {
            meter: LoudnessMeter::new(rate as f64),
            rate,
            seen: 0,
            subs: Vec::new(),
            st: Vec::new(),
            hist: vec![0; LUFS_HIST_BINS],
        }
    }

    fn push(&mut self, rate: u32, l: &[f64], r: &[f64]) {
        if rate != self.rate {
            self.meter = LoudnessMeter::new(rate as f64);
            self.rate = rate;
            self.seen = 0;
        }
        self.meter.push(l, r);
        for &p in &self.meter.subs()[self.seen..] {
            self.subs.push(p);
            if self.subs.len() >= 30 {
                let v = short_term_at(&self.subs, self.subs.len());
                self.st.push(v as f64);
                if let Some(b) = lufs_hist_bin(v) {
                    self.hist[b] += 1;
                }
            }
        }
        self.seen = self.meter.subs().len();
    }

    fn lufs_i(&self) -> f64 {
        integrated_of(&self.subs).0
    }

    fn lra(&self) -> f64 {
        lra_ebu(&self.st).map_or(f64::NAN, |r| r.lra)
    }
}

/// The highest true peak over a stretch whose rate may change.
struct Peak {
    tp: EburTruePeak,
    rate: u32,
    before: f64,
}

impl Peak {
    fn new(rate: u32) -> Peak {
        Peak { tp: EburTruePeak::new(rate), rate, before: 0.0 }
    }

    fn push(&mut self, rate: u32, l: &[f64], r: &[f64]) {
        if rate != self.rate {
            self.before = self.before.max(self.tp.peak_max());
            self.tp = EburTruePeak::new(rate);
            self.rate = rate;
        }
        self.tp.push(l, r);
    }

    fn db(&self) -> f64 {
        let p = self.before.max(self.tp.peak_max());
        if p > 0.0 { lin_to_db(p) } else { f64::NAN }
    }
}

/// One signal's totals (S or O).
struct Meters {
    loud: Loud,
    peak: Peak,
    dr: Option<DrLive>,
    /// Σ L², Σ R², Σ L·R: the stereo correlation, as the O pass sums it.
    ll: f64,
    rr: f64,
    lr: f64,
}

impl Meters {
    fn new(rate: u32, with_dr: bool) -> Meters {
        Meters {
            loud: Loud::new(rate),
            peak: Peak::new(rate),
            dr: with_dr.then(|| DrLive::new(rate)),
            ll: 0.0,
            rr: 0.0,
            lr: 0.0,
        }
    }

    fn push(&mut self, rate: u32, l: &[f64], r: &[f64]) {
        if l.is_empty() {
            return;
        }
        self.loud.push(rate, l, r);
        self.peak.push(rate, l, r);
        if let Some(d) = &mut self.dr {
            d.rate_changed(rate);
            d.push(l, r);
        }
        for (&a, &b) in l.iter().zip(r) {
            self.ll += a * a;
            self.rr += b * b;
            self.lr += a * b;
        }
    }

    /// L against R over all of it, −1…+1 (opass.rs's STEREO_CORR); NaN in silence.
    fn corr(&self) -> f64 {
        if self.ll > 0.0 && self.rr > 0.0 { self.lr / (self.ll * self.rr).sqrt() } else { f64::NAN }
    }

    /// The numbers, and the short-term loudness histogram as counts per bin
    /// (`hist`: the files' bins; the page makes them shares).
    fn value(&self) -> Value {
        let mut v = json!({
            "lufsI": self.loud.lufs_i(),
            "lra": self.loud.lra(),
            "tp": self.peak.db(),
            "corr": self.corr(),
            "hist": &self.loud.hist,
        });
        if let Some(d) = &self.dr {
            v["dr"] = json!(d.result().map_or(f64::NAN, |r| r.dr_exact));
        }
        v
    }
}

/// The stream's long-term spectrum over its louder columns (S's
/// spectrogram frames), for its bandwidth.
pub struct Ltas {
    sum: Vec<f64>,
    cols: u64,
    rate: u32,
    hop: usize,
    lo: usize,
    hi: usize,
}

impl Ltas {
    /// Columns of `n`-point frames at `rate`, `hop` apart.
    pub fn new(n: usize, hop: usize, rate: u32) -> Ltas {
        let df = rate as f64 / n as f64;
        let bins = n / 2 + 1;
        let lo = ((1000.0 / df) as usize).min(bins - 1);
        let hi = ((8000.0 / df) as usize).clamp(lo + 1, bins);
        Ltas { sum: vec![0.0; bins], cols: 0, rate, hop, lo, hi }
    }

    /// One column's power spectrum (`n/2 + 1` bins), counted when its
    /// 1–8 kHz level says it is not silence or a fade.
    pub fn add(&mut self, power: &[f64]) {
        if power.len() != self.sum.len() {
            return;
        }
        let mean = power[self.lo..self.hi].iter().sum::<f64>() / (self.hi - self.lo) as f64;
        if mean < LTAS_GATE {
            return;
        }
        for (s, &p) in self.sum.iter_mut().zip(power) {
            *s += p;
        }
        self.cols += 1;
    }

    /// Seconds of louder sound counted.
    pub fn seconds(&self) -> f64 {
        self.cols as f64 * self.hop as f64 / self.rate.max(1) as f64
    }

    /// The bandwidth: `Some(Some(hz))` a cut below the top, `Some(None)`
    /// none (full), `None` not enough sound yet.
    pub fn estimate(&self) -> Option<Option<f64>> {
        if self.seconds() < LTAS_MIN_S {
            return None;
        }
        let mean: Vec<f64> = self.sum.iter().map(|s| s / self.cols as f64).collect();
        Some(cutoff(&mean, self.rate))
    }

    fn value(&self) -> Value {
        let nyq = self.rate as f64 / 2.0;
        match self.estimate() {
            None => json!({ "state": "measuring", "hz": null, "full": false, "nyq": nyq }),
            Some(None) => json!({ "state": "ok", "hz": nyq, "full": true, "nyq": nyq }),
            Some(Some(hz)) => json!({ "state": "ok", "hz": hz, "full": false, "nyq": nyq }),
        }
    }
}

/// Where a mean power spectrum (`n/2 + 1` bins at `rate`) really ends: the
/// highest frequency the spectrum stands `CLIFF_DB` above everything from
/// 500 Hz above it to the top (smoothed over ±100 Hz), taken to the edge —
/// the last frequency within `EDGE_DB` of the level 0.5–2 kHz below the
/// cliff. None: no such cliff, or one in the top 5 % (full band).
pub fn cutoff(mean: &[f64], rate: u32) -> Option<f64> {
    let bins = mean.len();
    if bins < 16 {
        return None;
    }
    let df = rate as f64 / ((bins - 1) * 2) as f64;
    let w = ((100.0 / df).round() as usize).max(1);
    let mut pre = vec![0.0; bins + 1];
    for (i, &p) in mean.iter().enumerate() {
        pre[i + 1] = pre[i] + p;
    }
    let d: Vec<f64> = (0..bins)
        .map(|k| {
            let (a, b) = (k.saturating_sub(w), (k + w + 1).min(bins));
            10.0 * ((pre[b] - pre[a]) / (b - a) as f64).max(1e-30).log10()
        })
        .collect();
    let mut top = vec![f64::NEG_INFINITY; bins + 1];
    for k in (0..bins).rev() {
        top[k] = top[k + 1].max(d[k]);
    }
    let g = ((500.0 / df).round() as usize).max(1);
    let k_lo = ((1000.0 / df) as usize).max(1);
    let k = (k_lo..bins.saturating_sub(g)).rev().find(|&k| d[k] >= top[k + g] + CLIFF_DB)?;
    let a = k.saturating_sub((2000.0 / df) as usize).max(k_lo / 2);
    let b = k.saturating_sub(g).max(a + 1);
    let mut band = d[a..b].to_vec();
    band.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let level = band[band.len() / 2];
    let mut edge = a;
    for (j, &v) in d.iter().enumerate().take((k + g).min(bins)).skip(a) {
        if v >= level - EDGE_DB {
            edge = j;
        }
    }
    let hz = (edge as f64 + 0.5) * df;
    (hz < 0.95 * rate as f64 / 2.0).then_some(hz)
}

/// The song playing.
struct Song {
    /// The source frame it began at.
    start: u64,
    /// Tuned in after it began.
    joined: bool,
    s: Meters,
    o: Meters,
    ltas: Ltas,
    /// S frames measured.
    frames: u64,
}

impl Song {
    fn new(start: u64, joined: bool, f: &Frames) -> Song {
        Song {
            start,
            joined,
            s: Meters::new(f.src_rate, true),
            o: Meters::new(f.out_rate, true),
            ltas: Ltas::new(f.n_fft, f.hop, f.src_rate),
            frames: 0,
        }
    }

    fn push(&mut self, f: &Frames, o: std::ops::Range<usize>, s: std::ops::Range<usize>) {
        self.o.push(f.out_rate, &f.o_l[o.clone()], &f.o_r[o]);
        self.frames += s.len() as u64;
        self.s.push(f.src_rate, &f.s_l[s.clone()], &f.s_r[s]);
    }
}

/// One station's listening session.
struct Station {
    radio: Weak<RadioShared>,
    url: String,
    icy: String,
    src_rate: u32,
    /// Since tuned in or ↺.
    s: Meters,
    o: Meters,
    /// How far the source reaches is the station's: since tuned in, ↺ leaves
    /// it (it said "measuring…" again for 10 s of a source that had not
    /// changed); another station measures afresh.
    ltas: Ltas,
    frames: u64,
    song: Song,
    /// The session's values, and how many S blocks they were made of.
    cache: Option<(usize, Value)>,
}

impl Station {
    fn new(radio: &Weak<RadioShared>, f: &Frames) -> Station {
        let shared = radio.upgrade();
        let url = shared.as_ref().map(|s| s.url.clone()).unwrap_or_default();
        let icy = shared.as_ref().map(|s| s.info.lock().unwrap().icy_name.clone()).unwrap_or_default();
        Station {
            radio: radio.clone(),
            url,
            icy,
            src_rate: f.src_rate,
            s: Meters::new(f.src_rate, false),
            o: Meters::new(f.out_rate, false),
            ltas: Ltas::new(f.n_fft, f.hop, f.src_rate),
            frames: 0,
            song: Song::new(f.s_from, true, f),
            cache: None,
        }
    }

    /// The song's entry: its title (the one heard at its start), when the
    /// stream said one.
    fn song_value(&self, song: &Song, cut: bool) -> Option<Value> {
        let title = self.radio.upgrade().and_then(|r| r.title_at(Some(song.start as i64)))?;
        let rate = self.src_rate.max(1) as f64;
        Some(json!({
            "title": title.text,
            "album": title.album,
            "url": self.url,
            "icy": self.icy,
            "startS": song.start as f64 / rate,
            "playedS": song.frames as f64 / rate,
            "joined": song.joined,
            "cut": cut,
            "s": song.s.value(),
            "o": song.o.value(),
            "bw": song.ltas.value(),
        }))
    }

    fn session_value(&mut self) -> Value {
        let n = self.s.loud.subs.len();
        let fresh = self.cache.as_ref().is_some_and(|(m, _)| n < m + (n / 600).max(1));
        if !fresh {
            let v = json!({
                "playedS": self.frames as f64 / self.src_rate.max(1) as f64,
                "s": self.s.value(),
                "o": self.o.value(),
                "bw": self.ltas.value(),
            });
            self.cache = Some((n, v));
        }
        let mut v = self.cache.as_ref().map(|(_, v)| v.clone()).unwrap_or(Value::Null);
        v["playedS"] = json!(self.frames as f64 / self.src_rate.max(1) as f64);
        v
    }
}

/// Everything the analyzer keeps of streams (see the module doc).
pub struct Totals {
    cur: Option<Station>,
    songs: VecDeque<Value>,
}

impl Totals {
    pub const fn new() -> Totals {
        Totals { cur: None, songs: VecDeque::new() }
    }

    /// A song that ended goes in the list — without its histograms (the list
    /// shows its numbers; 30 songs' bins would weigh on every second's JSON).
    fn keep(&mut self, song: Option<Value>) {
        if let Some(mut v) = song {
            for k in ["s", "o"] {
                if let Some(m) = v.get_mut(k).and_then(Value::as_object_mut) {
                    m.remove("hist");
                }
            }
            self.songs.push_front(v);
            self.songs.truncate(SONGS_KEPT);
        }
    }

    /// One wakeup's frames of the stream `radio` plays.
    pub fn feed(&mut self, radio: &Weak<RadioShared>, f: &Frames) {
        let same = self.cur.as_ref().is_some_and(|c| Weak::ptr_eq(&c.radio, radio) && c.src_rate == f.src_rate);
        if !same {
            // Another station: the last one's song was cut short.
            let old = self.cur.take().and_then(|c| if c.song.frames > 0 { c.song_value(&c.song, true) } else { None });
            self.keep(old);
            self.cur = Some(Station::new(radio, f));
        }
        let Some(st) = self.cur.as_mut() else { return };
        let (on, sn) = (f.o_l.len().min(f.o_r.len()), f.s_l.len().min(f.s_r.len()));
        let s_end = f.s_from + sn as u64;
        // A song beginning in these frames, or one said too late (its place
        // already heard: cut here).
        let next = st
            .radio
            .upgrade()
            .and_then(|r| r.songs.first_from(st.song.start as i64 + 1))
            .map(|b| b.max(0) as u64)
            .filter(|&b| b <= s_end);
        match next {
            None => st.song.push(f, 0..on, 0..sn),
            Some(b) => {
                let b = b.max(f.s_from);
                let ss = (b - f.s_from) as usize;
                let os = ((b * f.l).saturating_sub(f.idx0) as usize).min(on);
                st.song.push(f, 0..os, 0..ss);
                let done = std::mem::replace(&mut st.song, Song::new(b, false, f));
                let v = st.song_value(&done, false);
                st.song.push(f, os..on, ss..sn);
                self.keep(v);
            }
        }
        if let Some(st) = self.cur.as_mut() {
            st.o.push(f.out_rate, &f.o_l[..on], &f.o_r[..on]);
            st.s.push(f.src_rate, &f.s_l[..sn], &f.s_r[..sn]);
            st.frames += sn as u64;
        }
    }

    /// One of S's spectrogram columns (its power spectrum): the bandwidth's.
    pub fn column(&mut self, power: &[f64]) {
        if let Some(st) = self.cur.as_mut() {
            st.ltas.add(power);
            st.song.ltas.add(power);
        }
    }

    /// ↺: the session's totals afresh; the source's bandwidth (the
    /// station's), the song and the list go on.
    pub fn reset_session(&mut self) {
        if let Some(st) = self.cur.as_mut() {
            st.s = Meters::new(st.src_rate, false);
            st.o = Meters::new(st.o.peak.rate, false);
            st.frames = 0;
            st.cache = None;
        }
    }

    /// The page's JSON: `direct` — BIT-PERFECT now (O is S).
    pub fn value(&mut self, direct: bool) -> Value {
        let songs: Vec<Value> = self.songs.iter().cloned().collect();
        let Some(st) = self.cur.as_mut() else {
            return json!({ "v": 1, "bp": direct, "titles": false, "session": null, "song": null, "songs": songs });
        };
        let titles = st.radio.upgrade().is_some_and(|r| r.title_at(Some(i64::MAX)).is_some());
        let session = st.session_value();
        let song = st.song_value(&st.song, false).unwrap_or(Value::Null);
        json!({
            "v": 1,
            "bp": direct,
            "rate": st.src_rate,
            "titles": titles,
            "session": session,
            "song": song,
            "songs": songs,
        })
    }
}

/// The live thread's totals (its stream tap feeds them).
pub static TOTALS: Mutex<Totals> = Mutex::new(Totals::new());

/// ↺ asked (the route sets it; the tap applies it on its next frames).
static RESET: AtomicBool = AtomicBool::new(false);

/// The page's JSON as last built.
static JSON: Mutex<Option<Arc<[u8]>>> = Mutex::new(None);

pub fn ask_reset() {
    RESET.store(true, Ordering::Release);
}

/// Whether ↺ was asked since the last call.
pub fn take_reset() -> bool {
    RESET.swap(false, Ordering::AcqRel)
}

/// Build the page's JSON (once a second, from the tap).
pub fn publish(direct: bool) {
    let v = TOTALS.lock().unwrap_or_else(|e| e.into_inner()).value(direct);
    if let Ok(bytes) = serde_json::to_vec(&v) {
        *JSON.lock().unwrap_or_else(|e| e.into_inner()) = Some(bytes.into());
    }
}

/// The JSON for the route (never waits).
pub fn json_bytes() -> Option<Arc<[u8]>> {
    JSON.try_lock().ok()?.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::radio::live::LiveSource;
    use crate::player::settings::PlayerSettings;
    use super::super::dr::compute_dr;
    use super::super::spectra::{kaiser, spec_fft_len, Stft, SPEC_BETA};

    /// Deterministic noise in [−1, 1).
    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
            })
            .collect()
    }

    /// Music-like: noise and a tone whose level moves (an LRA), in sections.
    fn music(rate: u32, secs: f64, seed: u64) -> Vec<f64> {
        let n = (rate as f64 * secs) as usize;
        let z = noise(n, seed);
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let level = 0.03 + 0.25 * (0.5 + 0.5 * (t * 0.37).sin()).powi(2);
                level * (0.6 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.4 * z[i])
            })
            .collect()
    }

    fn frames<'a>(rate: u32, l: &'a [f64], s_from: u64) -> Frames<'a> {
        let n = spec_fft_len(rate);
        Frames {
            src_rate: rate,
            n_fft: n,
            hop: n / 2,
            out_rate: rate,
            l: 1,
            idx0: s_from,
            o_l: l,
            o_r: l,
            s_from,
            s_l: l,
            s_r: l,
        }
    }

    fn radio(rate: u32) -> Arc<RadioShared> {
        let shared = Arc::new(RadioShared::new("test://stream", &PlayerSettings::default()));
        assert!(shared.live.set(Arc::new(LiveSource::new(rate, 1 << 30, 0.5, 600.0))).is_ok());
        shared
    }

    /// Feed `x` from source frame `at` in uneven chunks (S = O, L = 1).
    fn feed_all(t: &mut Totals, r: &Arc<RadioShared>, rate: u32, x: &[f64], at: u64) {
        let w = Arc::downgrade(r);
        let mut i = 0;
        let mut k = 0usize;
        while i < x.len() {
            let n = [441, 882, 1333, 97, 2048][k % 5].min(x.len() - i);
            t.feed(&w, &frames(rate, &x[i..i + n], at + i as u64));
            i += n;
            k += 1;
        }
    }

    /// The whole track fed as a stream: its session and its song (one, from
    /// the first title) say what the files' analysis says of it — the same
    /// integrated loudness, LRA, true peak and DR.
    #[test]
    fn a_whole_track_through_the_totals_is_the_files_analysis() {
        let rate = 44_100;
        let x = music(rate, 40.0, 7);
        let r = radio(rate);
        r.set_title("A - One", "icy");
        let mut t = Totals::new();
        feed_all(&mut t, &r, rate, &x, 0);
        let v = t.value(false);
        let mut m = LoudnessMeter::new(rate as f64);
        m.push(&x, &x);
        let st: Vec<f64> = m.short_term_series().iter().map(|&v| v as f64).collect();
        let lra = lra_ebu(&st).unwrap().lra;
        let mut tp = EburTruePeak::new(rate);
        tp.push(&x, &x);
        let dr = compute_dr(&x, &x, rate).unwrap().dr_exact;
        for (what, got) in [("session", &v["session"]["s"]), ("session", &v["session"]["o"]), ("song", &v["song"]["s"]), ("song", &v["song"]["o"])] {
            assert!((got["lufsI"].as_f64().unwrap() - m.integrated()).abs() < 1e-9, "{what} {got}");
            assert!((got["lra"].as_f64().unwrap() - lra).abs() < 1e-9, "{what} {got}");
            assert!((got["tp"].as_f64().unwrap() - lin_to_db(tp.peak_max())).abs() < 1e-9, "{what} {got}");
        }
        assert!(v["session"]["s"].get("dr").is_none(), "no DR on the session");
        assert!((v["song"]["s"]["dr"].as_f64().unwrap() - dr).abs() < 1e-9, "{}", v["song"]);
        assert_eq!(v["song"]["title"], "A - One");
        assert_eq!(v["song"]["joined"], true);
        assert!((v["session"]["playedS"].as_f64().unwrap() - 40.0).abs() < 1e-6);
        assert_eq!(v["titles"], true);
    }

    /// Songs are cut where the stream says they begin — S at the frame, O at
    /// the frame × L — a start said late is cut where it is heard, and the
    /// list keeps the newest first.
    #[test]
    fn songs_are_cut_where_the_stream_says_they_begin() {
        let rate = 44_100u32;
        let l = 4u64;
        let x = music(rate, 30.0, 3);
        let r = radio(rate);
        let live = r.live.get().unwrap().clone();
        // "A" stands from the start; "B" begins at 10 s (the stream's edge then).
        let b1 = 10 * rate as usize;
        live.push_raw(&x[..b1], &x[..b1]);
        live.push(&x[..b1], &x[..b1]);
        r.set_title("A - One", "icy");
        r.set_title("B - Two", "icy");
        assert_eq!(r.songs.first_from(1), Some(b1 as i64));
        let w = Arc::downgrade(&r);
        let o: Vec<f64> = x.iter().flat_map(|&v| std::iter::repeat(v).take(l as usize)).collect();
        let n = spec_fft_len(rate);
        let mut t = Totals::new();
        // From 2 s on (tuned in mid-song), 20 ms at a time, O L times S.
        let step = rate as usize / 50;
        let mut s = 2 * rate as usize;
        while s < 20 * rate as usize {
            let e = s + step;
            t.feed(&w, &Frames {
                src_rate: rate, n_fft: n, hop: n / 2, out_rate: rate * l as u32, l,
                idx0: s as u64 * l,
                o_l: &o[s * l as usize..e * l as usize], o_r: &o[s * l as usize..e * l as usize],
                s_from: s as u64, s_l: &x[s..e], s_r: &x[s..e],
            });
            s = e;
        }
        let v = t.value(false);
        let songs = v["songs"].as_array().unwrap();
        assert_eq!(songs.len(), 1, "{v}");
        let a = &songs[0];
        assert!(a["s"].get("hist").is_none() && a["s"]["corr"].is_f64(), "the list: numbers, no bins: {a}");
        assert_eq!(a["title"], "A - One");
        assert_eq!(a["joined"], true);
        assert_eq!(a["cut"], false);
        assert!((a["playedS"].as_f64().unwrap() - 8.0).abs() < 1e-9, "{a}");
        // A's S is exactly 2..10 s; its O exactly those frames × L.
        let mut m = LoudnessMeter::new(rate as f64);
        m.push(&x[2 * rate as usize..b1], &x[2 * rate as usize..b1]);
        assert!((a["s"]["lufsI"].as_f64().unwrap() - m.integrated()).abs() < 1e-9);
        let mut mo = LoudnessMeter::new((rate * l as u32) as f64);
        let (o0, o1) = (2 * rate as usize * l as usize, b1 * l as usize);
        mo.push(&o[o0..o1], &o[o0..o1]);
        assert!((a["o"]["lufsI"].as_f64().unwrap() - mo.integrated()).abs() < 1e-9);
        assert_eq!(v["song"]["title"], "B - Two");
        assert_eq!(v["song"]["joined"], false);
        assert!((v["song"]["startS"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        assert!((v["song"]["playedS"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        // A start said late (at 15 s, heard already): B ends here, at 20 s.
        r.songs.push(15 * rate as i64, rate);
        t.feed(&w, &Frames {
            src_rate: rate, n_fft: n, hop: n / 2, out_rate: rate * l as u32, l,
            idx0: s as u64 * l,
            o_l: &o[s * l as usize..(s + step) * l as usize], o_r: &o[s * l as usize..(s + step) * l as usize],
            s_from: s as u64, s_l: &x[s..s + step], s_r: &x[s..s + step],
        });
        let v = t.value(false);
        assert_eq!(v["songs"].as_array().unwrap().len(), 2);
        assert_eq!(v["songs"][0]["title"], "B - Two");
        assert!((v["songs"][0]["playedS"].as_f64().unwrap() - 10.0).abs() < 1e-9, "{}", v["songs"][0]);
        assert!((v["song"]["startS"].as_f64().unwrap() - 20.0).abs() < 1e-9);
    }

    /// Another station: the last one's song goes in cut short, the session
    /// begins anew; ↺ starts the session afresh and leaves the song and the
    /// list; a stream with no titles has no songs.
    #[test]
    fn a_new_station_cuts_the_song_short_and_reset_keeps_the_songs() {
        let rate = 48_000u32;
        let x = music(rate, 12.0, 5);
        let (a, b) = (radio(rate), radio(rate));
        a.set_title("A - One", "icy");
        let mut t = Totals::new();
        feed_all(&mut t, &a, rate, &x[..6 * rate as usize], 0);
        feed_all(&mut t, &b, rate, &x[6 * rate as usize..], 100);
        let v = t.value(false);
        assert_eq!(v["songs"].as_array().unwrap().len(), 1);
        assert_eq!(v["songs"][0]["cut"], true);
        assert_eq!(v["songs"][0]["title"], "A - One");
        assert_eq!(v["titles"], false, "the new station said no title");
        assert_eq!(v["song"], Value::Null);
        assert!((v["session"]["playedS"].as_f64().unwrap() - 6.0).abs() < 1e-6);
        // ↺: the session from here; the list stays.
        t.reset_session();
        feed_all(&mut t, &b, rate, &x[..rate as usize], 100 + 6 * rate as u64);
        let v = t.value(true);
        assert!((v["session"]["playedS"].as_f64().unwrap() - 1.0).abs() < 1e-6, "{}", v["session"]);
        assert_eq!(v["songs"].as_array().unwrap().len(), 1);
        assert_eq!(v["bp"], true);
    }

    /// The song's and the session's short-term loudness histograms count the
    /// files' values in the files' bins (as the whole-track `hist_s`); ↺
    /// empties the session's and leaves the song's; the correlation is the O
    /// pass's sum: +1 for channels alike, −1 for opposite ones.
    #[test]
    fn the_histograms_and_the_correlation_are_the_files() {
        let rate = 44_100;
        let x = music(rate, 20.0, 11);
        let r = radio(rate);
        r.set_title("A - One", "icy");
        let mut t = Totals::new();
        feed_all(&mut t, &r, rate, &x, 0);
        let v = t.value(false);
        let mut m = LoudnessMeter::new(rate as f64);
        m.push(&x, &x);
        let series = m.short_term_series();
        let want = super::super::track::lufs_histogram(&series);
        let counts = |got: &Value| -> Vec<u32> {
            got["hist"].as_array().unwrap().iter().map(|c| c.as_u64().unwrap() as u32).collect()
        };
        for (what, got) in [("session S", &v["session"]["s"]), ("session O", &v["session"]["o"]),
                            ("song S", &v["song"]["s"]), ("song O", &v["song"]["o"])] {
            let h = counts(got);
            assert_eq!(h.len(), LUFS_HIST_BINS, "{what}");
            let n: u32 = h.iter().sum();
            assert_eq!(n as usize, series.iter().filter(|&&s| lufs_hist_bin(s).is_some()).count(), "{what}");
            assert!(n > 100, "{what}: {n} values");
            for (i, (&c, &w)) in h.iter().zip(&want).enumerate() {
                assert_eq!(c as f32 / n as f32, w, "{what}: bin {i}");
            }
            assert!((got["corr"].as_f64().unwrap() - 1.0).abs() < 1e-12, "{what}: L = R");
        }
        // ↺: the session's afresh; the song's goes on.
        t.reset_session();
        let v = t.value(false);
        assert_eq!(counts(&v["session"]["s"]).iter().sum::<u32>(), 0);
        assert!(v["session"]["s"]["corr"].is_null(), "nothing heard since ↺: {}", v["session"]["s"]["corr"]);
        assert!(counts(&v["song"]["s"]).iter().sum::<u32>() > 100);
        // Opposite channels.
        let neg: Vec<f64> = x.iter().map(|v| -v).collect();
        let n = spec_fft_len(rate);
        let mut t2 = Totals::new();
        t2.feed(&Arc::downgrade(&r), &Frames {
            src_rate: rate, n_fft: n, hop: n / 2, out_rate: rate, l: 1, idx0: 0,
            o_l: &x, o_r: &neg, s_from: 0, s_l: &x, s_r: &neg,
        });
        let v = t2.value(false);
        for got in [&v["session"]["s"], &v["session"]["o"], &v["song"]["o"]] {
            assert!((got["corr"].as_f64().unwrap() + 1.0).abs() < 1e-12, "{got}");
        }
    }

    /// How far the source reaches is the station's: ↺ leaves it (it said
    /// "measuring…" again for 10 s), another station measures afresh.
    #[test]
    fn the_bandwidth_is_the_stations_and_reset_leaves_it() {
        let rate = 44_100u32;
        let (a, b) = (radio(rate), radio(rate));
        let mut t = Totals::new();
        feed_all(&mut t, &a, rate, &music(rate, 1.0, 3), 0);
        // 12 s of S's columns.
        let n = spec_fft_len(rate);
        let stft = Stft::new(n, n / 2, SPEC_BETA);
        let x = noise(12 * rate as usize, 5);
        let mut p = Vec::new();
        let mut i = 0;
        while i + n <= x.len() {
            stft.frame_power_lr(&x[i..i + n], &x[i..i + n], &mut p);
            t.column(&p);
            i += n / 2;
        }
        let bw = t.value(false)["session"]["bw"].clone();
        assert_eq!(bw["state"], "ok", "{bw}");
        t.reset_session();
        let v = t.value(false);
        assert_eq!(v["session"]["bw"], bw, "↺: the session afresh, the bandwidth as it was");
        assert!(v["session"]["playedS"].as_f64().unwrap() < 1e-9);
        feed_all(&mut t, &b, rate, &music(rate, 1.0, 4), 0);
        assert_eq!(t.value(false)["session"]["bw"]["state"], "measuring", "another station");
    }

    /// Noise of falling level through a steep lowpass (Kaiser-windowed
    /// sinc), the cut a codec makes; S's columns as the tap makes them.
    fn ltas_of(rate: u32, cut_hz: Option<f64>, secs: f64, quiet: bool) -> Ltas {
        let n_sig = (rate as f64 * secs) as usize;
        let z = noise(n_sig + 1024, 11);
        // A gentle tilt (a one-pole lowpass at 3 kHz), as music's highs fall.
        let a = (-2.0 * std::f64::consts::PI * 3000.0 / rate as f64).exp();
        let mut y = 0.0;
        let tilted: Vec<f64> = z.iter().map(|&v| { y = a * y + (1.0 - a) * v; y }).collect();
        let x: Vec<f64> = match cut_hz {
            None => tilted[..n_sig].to_vec(),
            Some(fc) => {
                let taps = 1023;
                let win = kaiser(taps, 12.0);
                let h: Vec<f64> = (0..taps)
                    .map(|i| {
                        let m = i as f64 - (taps / 2) as f64;
                        let wc = 2.0 * fc / rate as f64;
                        let s = if m == 0.0 { wc } else { (std::f64::consts::PI * wc * m).sin() / (std::f64::consts::PI * m) };
                        s * win[i]
                    })
                    .collect();
                (0..n_sig).map(|i| (0..taps).map(|j| h[j] * tilted[i + j]).sum()).collect()
            }
        };
        // Quiet stretches (−70 dB) and silence between loud ones.
        let x: Vec<f64> = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let sec = i / rate as usize;
                if quiet && sec % 3 == 1 { v * 3e-4 } else if quiet && sec % 3 == 2 && sec % 2 == 0 { 0.0 } else { v * 0.5 }
            })
            .collect();
        let n = spec_fft_len(rate);
        let stft = Stft::new(n, n / 2, SPEC_BETA);
        let mut ltas = Ltas::new(n, n / 2, rate);
        let mut p = Vec::new();
        let mut i = 0;
        while i + n <= x.len() {
            stft.frame_power_lr(&x[i..i + n], &x[i..i + n], &mut p);
            ltas.add(&p);
            i += n / 2;
        }
        ltas
    }

    /// The bandwidth finds a codec's cut, to a quarter kilohertz, with quiet
    /// passages and silence left out; a stream that goes to the top is full;
    /// too little sound is still measuring.
    #[test]
    fn the_bandwidth_finds_the_cut_in_the_spectrum() {
        for (rate, cut, quiet) in [
            (44_100u32, Some(16_000.0), false),
            (44_100, Some(19_000.0), true),
            (48_000, Some(22_000.0), false),
            (44_100, None, true),
        ] {
            let ltas = ltas_of(rate, cut, 24.0, quiet);
            let got = ltas.estimate().expect("enough sound");
            match cut {
                Some(fc) => {
                    let hz = got.unwrap_or_else(|| panic!("{rate} {fc}: full"));
                    assert!((hz - fc).abs() < 250.0, "{rate} {fc}: {hz}");
                }
                None => assert_eq!(got, None, "{rate}: {got:?}"),
            }
        }
        assert!(ltas_of(44_100, Some(16_000.0), 6.0, false).estimate().is_none(), "6 s: measuring");
    }
}
