//! Internet radio through the player (an experiment): a stream that never
//! ends, played through the same convolver, rack and output as a file.
//!
//! * `net` — the HTTP(S) connection, the ICY metadata cut out, reconnects;
//! * `decode` — symphonia on the bytes as they come, into the live source;
//!   an HE-AAC stream (`adts` tells it) through `he_aac` (the FDK AAC
//!   decoder, `fdk`);
//! * `live` — the growing source the convolver reads (`convolver::Input::Live`);
//! * `chain` — the conversion side of the file chain on that source.
//!
//! A session is started by a hidden command (`player_radio`); the controller
//! plans the chain while the stream fills, builds it once the filter's
//! look-ahead and a margin are in, and puts it on the air with `play_chain`,
//! the same way to the device as a track: volume, test mute and guard.
//!
//! Switches for the experiment (environment): `AURA_RADIO_MARGIN_S` (stream
//! in hand beyond the first block before the build, default 8),
//! `AURA_RADIO_RESUME_S` (gathered after a starve, default 8),
//! `AURA_RADIO_DUMP=<folder>` (each connection's audio bytes to a file),
//! `AURA_RADIO_DROP_AT_S=60,300` (cut the connection at those session
//! seconds), `AURA_RADIO_STALL_AT_S=120:8` (stop reading for 8 s at 120 s),
//! `AURA_RADIO_LOG_S` (the buffer line in the log, default 10 s),
//! `AURA_RADIO_DRIFT_BAND_S` (the corridor the delay is kept in, default
//! 1 s: `drift`).

pub mod adts;
pub mod chain;
pub mod decode;
pub mod drift;
pub mod fdk;
pub mod he_aac;
pub mod hls;
pub mod join;
pub mod live;
pub mod mp4;
pub mod net;
pub mod ts;

#[cfg(test)]
mod bench;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use self::drift::{DriftPolicy, Place, Splice};
use self::live::LiveSource;
use crate::audio::converter::dsp::lab::stats::{self as lab_stats, SourceStats};
use super::convolver::BLOCK;
use super::settings::PlayerSettings;
use super::source_stages::{SourcePlan, SourceTally};
use super::stages::LimiterTally;
use super::timeline::Timeline;

/// History kept behind the reader: the longest filter's branch (30M taps at
/// ×2) and two blocks, so another rack can be primed at the current position.
pub const KEEP_FRAMES: usize = 15_000_000 + 2 * BLOCK;
/// A paused stream stops gathering this far ahead of the reader.
const MAX_AHEAD_S: f64 = 600.0;
/// How much of the stream's start the adaptive headroom looks at: the file
/// path decides on the whole source, a stream on its first seconds.
pub const FIRST_LOOK_S: f64 = 10.0;

/// The stream's first seconds as decoded, before the source stages (the
/// adaptive headroom reads the source, as the file path does).
#[derive(Default)]
struct FirstLook {
    l: Vec<f64>,
    r: Vec<f64>,
    /// Frames to gather (FIRST_LOOK_S at the stream's rate).
    want: usize,
    rate: u32,
    /// Taken: at FIRST_LOOK_S, or at the first chain built on what was in by
    /// then. One decision for the session: nothing is gathered after.
    taken: Option<(Option<SourceStats>, f64)>,
}

/// The delay's corridor (`drift`): the policy, and what the splices did.
pub struct DriftState {
    pub policy: DriftPolicy,
    pub splices: u32,
    pub cut_s: f64,
    pub repeated_s: f64,
    /// Each splice: session time, seconds (cut > 0), the place's level
    /// (dBFS) and how far under the loudest it was (dB).
    pub log: Vec<(f64, f64, f64, f64)>,
    /// Burst frames cut after gaps (their oldest), seconds.
    pub gap_cut_s: f64,
    /// The level now (seconds), for the status.
    pub level_s: Option<f64>,
}

impl FirstLook {
    fn take(&mut self) {
        if self.taken.is_some() {
            return;
        }
        let secs = self.l.len() as f64 / self.rate.max(1) as f64;
        let stats = lab_stats::analyze(&self.l, &self.r, self.rate, &AtomicBool::new(false));
        self.taken = Some((stats, secs));
        self.l = Vec::new();
        self.r = Vec::new();
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// What went wrong, as the page names it (its texts are the page's):
/// the station did not answer, or its format is not one the player decodes.
/// Anything else is shown as it is.
pub const ERR_UNREACHABLE: &str = "unreachable";
pub const ERR_FORMAT: &str = "format";
/// The stream decodes, at a sample rate the rack cannot upsample (an
/// AAC-LC stream at 22.05 kHz, say).
pub const ERR_RATE: &str = "rate";
/// The stream is encrypted (HLS with a key): it cannot be played at all —
/// not a format still to come.
pub const ERR_ENCRYPTED: &str = "encrypted";
pub const ERR_OTHER: &str = "other";
/// Not an error: the session was stopped (by the listener or a new one).
pub const STOPPED: &str = "stopped";

/// The kind of a failure from its words: an unsupported codec or container,
/// or an HLS address, is the stream's format; a rate the rack cannot take,
/// its rate.
pub fn kind_of(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("can be upsampled") || l.contains("highest output rate") || l.contains("integer factor") {
        ERR_RATE
    } else if l.starts_with("unsupported") || l.contains("hls") {
        ERR_FORMAT
    } else {
        ERR_OTHER
    }
}

/// What the stream says is playing, from the source frame it applies at.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Titled {
    /// The live source's frame from which it is heard.
    pub at: i64,
    pub text: String,
    /// "Album · Year", when a station's own list says so.
    pub album: Option<String>,
    pub from: String,
}

/// Titles kept for the place being heard to look up: the stream is heard
/// up to a minute (and a paused one ten) after it arrives.
const TITLES_KEPT: usize = 32;

/// Song starts kept (as many as titles, with room for starts no title came with).
const SONGS_KEPT: usize = 64;
/// How far past the stream's edge a song's end Radio Paradise's list
/// foretold may lie and still be where the next title begins, seconds.
const RP_SLACK_S: i64 = 5;

/// Where songs begin in the stream, as live source frames: a new title
/// heard from its frame, a chained Ogg stream's next logical stream. The
/// stream's chain lets its slow gain start afresh there.
#[derive(Default)]
pub struct SongStarts(Mutex<VecDeque<i64>>);

impl SongStarts {
    /// A song begins at `at`: two sources saying so within a second of
    /// each other count once.
    pub fn push(&self, at: i64, rate: u32) {
        let mut s = self.0.lock().unwrap();
        if s.iter().any(|&b| (at - b).abs() < rate.max(1) as i64) {
            return;
        }
        s.push_back(at);
        while s.len() > SONGS_KEPT {
            s.pop_front();
        }
    }

    /// The first song start at source frame `from` or after it.
    pub fn first_from(&self, from: i64) -> Option<i64> {
        self.0.lock().unwrap().iter().copied().filter(|&b| b >= from).min()
    }
}

/// What the session knows, for the status and the log.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioInfo {
    pub url: String,
    pub stream_url: String,
    pub content_type: String,
    pub icy_name: String,
    pub icy_br: String,
    pub audio_info: String,
    pub metaint: usize,
    pub connects: u32,
    /// How long each connection took to answer, ms.
    pub connect_ms: Vec<u64>,
    pub last_error: Option<String>,
    /// A failure that ended the session, and its kind (ERR_*).
    pub error: Option<String>,
    pub error_kind: Option<String>,
    /// A connection is up now (between two, the stream plays from what it
    /// holds while the next one is made).
    pub connected: bool,
    /// Audio bytes received (ICY metadata not counted).
    pub bytes: u64,
    pub codec: String,
    pub rate: u32,
    pub channels: u32,
    pub bits: Option<u32>,
    pub title: String,
    pub title_from: String,
    pub titles: u32,
    pub decode_errors: u64,
    /// New logical streams in a chained Ogg stream (song joins).
    pub resets: u32,
    /// New connections placed in the stream without a seam (their repeat
    /// cut off: the last one's length, s), and those met with a gap.
    pub joins: u32,
    pub repeated_s: f64,
    pub join_gaps: u32,
    /// Session milestones, ms from the start.
    pub t_connected_ms: Option<u64>,
    pub t_first_byte_ms: Option<u64>,
    pub t_first_pcm_ms: Option<u64>,
    pub t_planned_ms: Option<u64>,
    pub t_ready_ms: Option<u64>,
    pub t_built_ms: Option<u64>,
    pub t_on_air_ms: Option<u64>,
    /// Audio in hand against time after the first byte: (s, s of audio),
    /// every 250 ms for the first 15 s — the server's burst at connect.
    pub arrival: Vec<(f64, f64)>,
    /// The chain: rack, route, the filter's look-ahead and the stream it
    /// waited for.
    pub rack: String,
    pub route: String,
    pub delay_s: f64,
    pub need_s: f64,
    pub out_rate: u32,
    /// An HLS stream: what its client did (`hls`).
    pub hls: Option<hls::HlsInfo>,
}

/// Shared by the session's threads and the controller.
pub struct RadioShared {
    pub url: String,
    pub t0: Instant,
    pub stop: AtomicBool,
    pub info: Mutex<RadioInfo>,
    pub live: OnceLock<Arc<LiveSource>>,
    /// What is playing, oldest first, each from the frame it applies at.
    titles: Mutex<VecDeque<Titled>>,
    /// Where songs begin (the slow gain starts afresh there).
    pub songs: Arc<SongStarts>,
    /// Radio Paradise: the frame its list said the song playing would end
    /// at, when it was last asked.
    rp_next: Mutex<Option<i64>>,
    /// The rack the stream was tuned in with: its source stages run on the
    /// decoder thread, before the live source (`source_stages`).
    pub settings: PlayerSettings,
    pub source_tally: Arc<Mutex<SourceTally>>,
    /// The stream's first seconds, for the adaptive headroom.
    first: Mutex<FirstLook>,
    /// The delay's corridor: the monitor keeps the policy, the decoder's
    /// splicer makes what it asks for.
    pub drift: Mutex<DriftState>,
    /// The splice asked for, frames (> 0 cut out, < 0 played twice, 0 none).
    drift_ask: AtomicI64,
    /// Frames the splicer holds (they are past the joiner: `stream_frame`).
    splicer_held: AtomicI64,
    /// Output frames the chain's slow gain has read ahead (`buffer_s`).
    pub gain_queue: Arc<std::sync::atomic::AtomicU64>,
    pub margin_s: f64,
    resume_s: f64,
    dump_dir: Option<PathBuf>,
    drops: Mutex<Vec<f64>>,
    stall: Mutex<Option<(f64, f64)>>,
    timeline: Mutex<Option<Arc<Timeline>>>,
}

impl RadioShared {
    pub(crate) fn new(url: &str, settings: &PlayerSettings) -> RadioShared {
        let drops = std::env::var("AURA_RADIO_DROP_AT_S")
            .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_default();
        let stall = std::env::var("AURA_RADIO_STALL_AT_S").ok().and_then(|v| {
            let (a, b) = v.split_once(':')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        });
        RadioShared {
            url: url.to_string(),
            t0: Instant::now(),
            stop: AtomicBool::new(false),
            info: Mutex::new(RadioInfo { url: url.to_string(), ..RadioInfo::default() }),
            live: OnceLock::new(),
            titles: Mutex::new(VecDeque::new()),
            songs: Arc::new(SongStarts::default()),
            rp_next: Mutex::new(None),
            settings: settings.clone(),
            source_tally: Arc::new(Mutex::new(SourceTally::default())),
            first: Mutex::new(FirstLook::default()),
            drift: Mutex::new(DriftState {
                policy: DriftPolicy::new(env_f64("AURA_RADIO_DRIFT_BAND_S", drift::BAND_S)),
                splices: 0,
                cut_s: 0.0,
                repeated_s: 0.0,
                log: Vec::new(),
                gap_cut_s: 0.0,
                level_s: None,
            }),
            drift_ask: AtomicI64::new(0),
            splicer_held: AtomicI64::new(0),
            gain_queue: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            margin_s: env_f64("AURA_RADIO_MARGIN_S", 8.0).max(0.0),
            resume_s: env_f64("AURA_RADIO_RESUME_S", 8.0).max(0.1),
            dump_dir: std::env::var_os("AURA_RADIO_DUMP").map(PathBuf::from),
            drops: Mutex::new(drops),
            stall: Mutex::new(stall),
            timeline: Mutex::new(None),
        }
    }

    pub fn elapsed_s(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    /// The source stages the stream runs at `rate`.
    pub fn source_plan(&self, rate: u32) -> SourcePlan {
        SourcePlan::new(&self.settings, rate)
    }

    /// Frames as decoded (the decoder thread, before the source stages):
    /// the first FIRST_LOOK_S of them are kept for the adaptive headroom,
    /// and measured once they are in.
    pub fn first_look_push(&self, l: &[f64], r: &[f64], rate: u32) {
        let mut f = self.first.lock().unwrap();
        if f.taken.is_some() {
            return;
        }
        if f.want == 0 {
            f.rate = rate;
            f.want = ((FIRST_LOOK_S * rate as f64) as usize).max(1);
        }
        let n = (f.want - f.l.len()).min(l.len()).min(r.len());
        f.l.extend_from_slice(&l[..n]);
        f.r.extend_from_slice(&r[..n]);
        if f.l.len() >= f.want {
            f.take();
        }
    }

    /// The statistics of the stream's start (the source's peak, its ENOB)
    /// and the seconds they were taken on: when a chain is built before
    /// FIRST_LOOK_S are in, on what is in — and that stays the session's.
    pub fn first_look<R>(&self, f: impl FnOnce(Option<&SourceStats>, f64) -> R) -> R {
        let mut fl = self.first.lock().unwrap();
        fl.take();
        let (stats, secs) = fl.taken.as_ref().expect("taken above");
        f(stats.as_ref(), *secs)
    }

    /// The session cannot go on: say why and stop. The kind is read off the
    /// words (`kind_of`).
    pub fn fail(&self, e: String) {
        self.fail_kind(kind_of(&e), e);
    }

    /// The same, of a kind the caller knows (ERR_*).
    pub fn fail_kind(&self, kind: &'static str, e: String) {
        crate::aelog!("[RADIO] stopped ({}): {}", kind, e);
        {
            let mut i = self.info.lock().unwrap();
            i.error = Some(e);
            i.error_kind = Some(kind.to_string());
        }
        self.stop.store(true, Ordering::Release);
        if let Some(l) = self.live.get() {
            l.close();
        }
    }

    /// A new connection placed in the stream (join.rs): counted and logged.
    pub fn joined(&self, d: join::Joined, rate: u32) {
        let mut i = self.info.lock().unwrap();
        match d {
            join::Joined::Seamless { repeated } => {
                i.joins += 1;
                i.repeated_s = repeated as f64 / rate.max(1) as f64;
                crate::aelog!(
                    "[RADIO] the new connection repeats {:.2} s already received — skipped; the stream goes on without a seam",
                    i.repeated_s
                );
            }
            join::Joined::Gap { quiet } => {
                i.join_gaps += 1;
                crate::aelog!(
                    "[RADIO] the new connection is not in the last {:.0} s{} — the gap is faded",
                    join::TAIL_S,
                    if quiet { " (too quiet to place)" } else { "" }
                );
            }
            join::Joined::Known => {
                i.join_gaps += 1;
                crate::aelog!("[RADIO] the stream goes on after a break known to have lost audio — the gap is faded");
            }
        }
    }

    /// The live source frame the newest decoded frame goes to: what the
    /// network has brought in, the frames the source stages still hold
    /// counted in. Something the stream says now applies from here — a
    /// buffer and the filter's look-ahead ahead of the place being heard.
    pub fn stream_frame(&self) -> i64 {
        let held = {
            let t = self.source_tally.lock().unwrap();
            t.frames_in.saturating_sub(t.frames_out) as i64
        };
        self.live.get().map_or(0, |l| l.end()) + held + self.splicer_held.load(Ordering::Relaxed)
    }

    /// The stream in hand ahead of the chain, seconds: what the live source
    /// holds past the reader, and what the slow gain has read ahead of what
    /// it handed on (the filter's look-ahead not counted) — what covers a
    /// break in the network.
    pub fn buffer_s(&self) -> Option<f64> {
        let live = self.live.get()?;
        let out_rate = self.info.lock().unwrap().out_rate;
        let queued = if out_rate > 0 { self.gain_queue.load(Ordering::Relaxed) as f64 / out_rate as f64 } else { 0.0 };
        Some(live.ahead() as f64 / live.rate().max(1) as f64 + queued)
    }

    /// The splice the drift policy asks for, frames (> 0 cut out, < 0
    /// played twice, 0 none).
    pub fn drift_ask(&self) -> i64 {
        self.drift_ask.load(Ordering::Relaxed)
    }

    /// The frames the splicer holds now.
    pub fn set_splicer_held(&self, n: usize) {
        self.splicer_held.store(n as i64, Ordering::Relaxed);
    }

    /// Where the splicer, holding `held` frames, may splice: near a song's
    /// start (within 2 s of it, a less quiet place will do), not before one
    /// foretold within 2 minutes (Radio Paradise's list), else anywhere
    /// quiet.
    pub fn splice_place(&self, held: usize) -> Place {
        let Some(rate) = self.live.get().map(|l| l.rate() as i64) else { return Place::Anywhere };
        let next = self.stream_frame();
        let first = next - held as i64;
        let near = 2 * rate;
        let foretold = *self.rp_next.lock().unwrap();
        let song_near = self.songs.first_from(first - near).is_some_and(|s| s <= next + near)
            || foretold.is_some_and(|s| s >= first - near && s <= next + near);
        if song_near {
            Place::SongNear
        } else if foretold.is_some_and(|s| s > next && s <= next + 120 * rate) {
            Place::Wait
        } else {
            Place::Anywhere
        }
    }

    /// The splicer made a splice: the policy hears of it, the log says where.
    pub fn spliced(&self, s: &Splice, rate: u32) {
        self.drift_ask.store(0, Ordering::Relaxed);
        let t = self.elapsed_s();
        let secs = s.frames as f64 / rate.max(1) as f64;
        let mut d = self.drift.lock().unwrap();
        let dev = d.policy.deviation();
        d.policy.spliced(t, secs);
        d.splices += 1;
        if secs > 0.0 {
            d.cut_s += secs;
        } else {
            d.repeated_s -= secs;
        }
        d.log.push((t, secs, s.level_db, s.under_db));
        crate::aelog!(
            "[RADIO] drift: {} {:.1} ms at a {} place{} ({:.1} dBFS, {:.1} dB under the loudest{}); the delay was {:+.2} s off its level (corridor ±{:.2} s); splices {}",
            if secs > 0.0 { "cut out" } else { "played twice" },
            secs.abs() * 1000.0,
            if s.level_db <= -60.0 { "silent" } else { "quiet" },
            if s.at_song { " at a song's start" } else { "" },
            s.level_db,
            s.under_db,
            if s.corr.is_finite() { format!(", sides alike {:.2}", s.corr) } else { String::new() },
            dev.unwrap_or(0.0),
            d.policy.band_s(),
            d.splices
        );
    }

    /// `n` frames were taken out of the live source at frame `at`: what
    /// was placed after them comes that much earlier.
    pub fn shift_after(&self, at: i64, n: i64) {
        let moved = |x: i64| if x >= at + n { x - n } else if x > at { at } else { x };
        for t in self.titles.lock().unwrap().iter_mut() {
            t.at = moved(t.at);
        }
        for s in self.songs.0.lock().unwrap().iter_mut() {
            *s = moved(*s);
        }
        if let Some(r) = self.rp_next.lock().unwrap().as_mut() {
            *r = moved(*r);
        }
    }

    /// A song begins at the stream's newest frame (a chained Ogg stream's
    /// next logical stream).
    pub fn song_starts_here(&self) {
        let rate = self.live.get().map_or(1, |l| l.rate());
        self.songs.push(self.stream_frame(), rate);
    }

    pub fn set_title(&self, t: &str, from: &str) {
        self.set_title_with(t, None, from);
    }

    /// A new title, heard from the stream's newest frame on (`stream_frame`).
    pub fn set_title_with(&self, t: &str, album: Option<String>, from: &str) {
        self.set_title_at(t, album, from, None);
    }

    /// Radio Paradise's list says `t` plays, `time_s` seconds before it
    /// changes: the end of the song is placed in the stream from that
    /// (the list goes by the stream's live edge), and a new title the next
    /// answer brings is heard from the end the last one foretold — when it
    /// lies between the last title and a few seconds past the edge; else
    /// from the edge.
    pub fn rp_now_playing(&self, t: &str, album: Option<String>, time_s: f64) {
        let Some(rate) = self.live.get().map(|l| l.rate() as i64) else {
            return self.set_title_with(t, album, "rp");
        };
        let now = self.stream_frame();
        let foretold = self.rp_next.lock().unwrap().replace(now + (time_s.max(0.0) * rate as f64) as i64);
        let last_at = {
            let titles = self.titles.lock().unwrap();
            if titles.back().is_some_and(|x| x.text == t.trim() && x.album == album) {
                return;
            }
            titles.back().map(|x| x.at)
        };
        let at = foretold.filter(|&b| last_at.is_some_and(|a| b > a) && b <= now + RP_SLACK_S * rate);
        self.set_title_at(t, album, "rp", at);
    }

    /// A chained Ogg stream's next logical stream, with the title its tags
    /// give, if any: a song begins at the stream's newest frame — unless the
    /// tags name the song playing (a stream may chain anew to say more of
    /// the same song): then they only add to it.
    pub fn chained(&self, title: Option<&str>) {
        let same = title.is_some_and(|t| self.titles.lock().unwrap().back().is_some_and(|x| x.text == t.trim()));
        if !same {
            self.song_starts_here();
        }
        if let Some(t) = title {
            self.set_title(t, "tags");
        }
    }

    /// A new title from frame `at` (None: the newest, `stream_frame`). The
    /// first one stands for the stream from its start; each after it begins
    /// a song — but not the song playing said again: its words from any
    /// source (ICY, Radio Paradise's list, a chained Ogg stream's tags) only
    /// add what they bring, the album (Anton 3.10: the list's answer ~10 s
    /// after the ICY title cut the song in two, and the slow gain started
    /// afresh there). The same words after another title are a song again
    /// (a repeat).
    fn set_title_at(&self, t: &str, album: Option<String>, from: &str, at: Option<i64>) {
        let t = t.trim();
        if t.is_empty() {
            return;
        }
        let newest = at.unwrap_or_else(|| self.stream_frame());
        let (at, first) = {
            let mut titles = self.titles.lock().unwrap();
            if let Some(x) = titles.back_mut().filter(|x| x.text == t) {
                let adds = album.is_some() && x.album != album;
                if adds {
                    x.album = album.clone();
                }
                drop(titles);
                if adds {
                    crate::aelog!("[RADIO] the song playing, said again ({}), adds its album: {}", from, album.unwrap_or_default());
                }
                return;
            }
            let first = titles.is_empty();
            let at = if first { 0 } else { newest };
            titles.push_back(Titled { at, text: t.to_string(), album, from: from.to_string() });
            while titles.len() > TITLES_KEPT {
                titles.pop_front();
            }
            (at, first)
        };
        if !first {
            self.songs.push(at, self.live.get().map_or(1, |l| l.rate()));
        }
        {
            let mut i = self.info.lock().unwrap();
            i.title = t.to_string();
            i.title_from = from.to_string();
            i.titles += 1;
        }
        crate::aelog!("[RADIO] now playing ({}, from frame {}): {}", from, at, t);
    }

    /// The title heard at source frame `frame`: the newest one that applies
    /// there. None before anything is heard.
    pub fn title_at(&self, frame: Option<i64>) -> Option<Titled> {
        let f = frame?;
        self.titles.lock().unwrap().iter().rev().find(|x| x.at <= f).cloned()
    }

    /// The live source at the stream's rate: made at the first connection;
    /// a later one at another rate cannot join it.
    pub fn live_for(&self, rate: u32) -> Result<Arc<LiveSource>, String> {
        let l = self.live.get_or_init(|| Arc::new(LiveSource::new(rate, KEEP_FRAMES, self.resume_s, MAX_AHEAD_S)));
        if l.rate() != rate {
            return Err(format!("unsupported: the stream changed rate {} → {} Hz", l.rate(), rate));
        }
        Ok(l.clone())
    }

    /// A file for connection `n`'s audio bytes, when dumping is on.
    pub fn dump_file(&self, n: u32, content_type: &str) -> Option<std::fs::File> {
        let dir = self.dump_dir.as_ref()?;
        std::fs::create_dir_all(dir).ok()?;
        let ct = content_type.to_ascii_lowercase();
        let ext = if ct.contains("mpeg") || ct.contains("mp3") {
            "mp3"
        } else if ct.contains("aac") {
            "aac"
        } else if ct.contains("ogg") {
            "ogg"
        } else if ct.contains("flac") {
            "flac"
        } else {
            "bin"
        };
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = dir.join(format!("radio-{}-{}.{}", ms, n, ext));
        crate::aelog!("[RADIO] dumping connection #{} to {}", n, path.display());
        std::fs::File::create(path).ok()
    }

    /// The experiment's cut: true once for each AURA_RADIO_DROP_AT_S second
    /// passed.
    pub fn drop_due(&self) -> bool {
        let t = self.elapsed_s();
        let mut d = self.drops.lock().unwrap();
        if let Some(i) = d.iter().position(|&x| x <= t) {
            d.remove(i);
            crate::aelog!("[RADIO] cutting the connection at {:.1} s (AURA_RADIO_DROP_AT_S)", t);
            return true;
        }
        false
    }

    /// The experiment's stall: once, how long to stop reading now.
    pub fn stall_due(&self) -> Option<f64> {
        let t = self.elapsed_s();
        let mut s = self.stall.lock().unwrap();
        match *s {
            Some((at, len)) if at <= t => {
                *s = None;
                crate::aelog!("[RADIO] stalling the network for {:.1} s at {:.1} s (AURA_RADIO_STALL_AT_S)", len, t);
                Some(len)
            }
            _ => None,
        }
    }
}

/// After a gap, how long the new connection's burst is given to come in
/// before what it brought beyond the target is looked at, and the least
/// excess that is cut, seconds.
const GAP_LOOK_S: f64 = 2.0;
const GAP_CUT_MIN_S: f64 = 0.5;

/// The monitor's side of the delay's corridor: the level read against the
/// device's clock — the stream received less what the device has played —
/// the events after which the stream's level is its own again (on the air,
/// a pause, a gap, a starve, another device), and a gap's burst trimmed.
#[derive(Default)]
struct DriftWatch {
    /// The device's position at the last look, and its timeline.
    last_pos: Option<u64>,
    timeline: Option<usize>,
    playing: bool,
    gaps: u32,
    starves: u32,
    /// A gap was seen then: what its burst brought is looked at shortly.
    gap_at: Option<f64>,
}

impl DriftWatch {
    fn step(&mut self, sh: &RadioShared, live: &LiveSource, st: &live::LiveStats, t: f64) {
        let Some(tl) = sh.timeline.lock().unwrap().clone() else { return };
        let (pos, out_rate) = (tl.read_pos(), tl.rate().max(1) as f64);
        let rate = live.rate().max(1) as f64;
        // What the source stages and the splicer hold counted in: they hand
        // the stream on a block at a time.
        let level = (sh.stream_frame() as f64 - pos as f64 * rate / out_rate) / rate;
        // By its own number: a new timeline can take the old one's address.
        let id = tl.id() as usize;
        let moved = self.timeline == Some(id) && self.last_pos.is_some_and(|p| pos as f64 > p as f64 + 0.1 * out_rate);
        self.last_pos = Some(pos);
        self.timeline = Some(id);
        let events = st.gaps != self.gaps || st.starves != self.starves;
        if st.gaps != self.gaps {
            self.gap_at = Some(t);
        }
        self.gaps = st.gaps;
        self.starves = st.starves;
        let mut d = sh.drift.lock().unwrap();
        if !moved {
            // Paused, held, or another device: measured afresh when it plays.
            self.playing = false;
            d.level_s = None;
            return;
        }
        if !self.playing || events {
            self.playing = true;
            d.policy.restart(t);
            sh.drift_ask.store(0, Ordering::Relaxed);
        }
        // A gap: what the new connection's burst brought beyond the level
        // the stream kept goes — its oldest frames, not read yet.
        let mut level = level;
        if self.gap_at.is_some_and(|g| t >= g + GAP_LOOK_S) {
            self.gap_at = None;
            if let Some(excess) = d.policy.target().map(|tg| level - tg).filter(|&e| e > GAP_CUT_MIN_S) {
                if let Some((at, n)) = live.cut_after_gap((excess * rate) as usize) {
                    sh.shift_after(at, n as i64);
                    let s = n as f64 / rate;
                    d.gap_cut_s += s;
                    level -= s;
                    crate::aelog!(
                        "[RADIO] drift: the new connection brought {:.2} s more than the stream keeps — its oldest {:.2} s cut after the gap",
                        excess,
                        s
                    );
                }
            }
        }
        d.level_s = Some(level);
        let ask = d.policy.sample(t, level);
        if ask != 0.0 {
            sh.drift_ask.store((ask * rate).round() as i64, Ordering::Relaxed);
            crate::aelog!(
                "[RADIO] drift: the delay is {:+.2} s off its level (corridor ±{:.2} s): {} up to {:.0} ms at the next quiet place",
                d.policy.deviation().unwrap_or(0.0),
                d.policy.band_s(),
                if ask > 0.0 { "cutting out" } else { "playing twice" },
                ask.abs() * 1000.0
            );
        }
    }
}

/// One stream being played: its threads, its meter, its numbers.
pub struct Session {
    pub shared: Arc<RadioShared>,
    pub tally: Arc<Mutex<LimiterTally>>,
}

impl Session {
    /// Connect and decode `url` (the network and decoder threads, and the
    /// monitor that writes the buffer line); `settings` is the rack whose
    /// source stages the stream runs.
    pub fn start(url: &str, settings: &PlayerSettings) -> Session {
        let shared = Arc::new(RadioShared::new(url, settings));
        crate::aelog!(
            "[RADIO] session: {} (margin {:.1} s, resume {:.1} s)",
            url, shared.margin_s, shared.resume_s
        );
        let (tx, rx) = std::sync::mpsc::channel();
        net::spawn(shared.clone(), tx);
        decode::spawn(shared.clone(), rx);
        let s = Session { shared, tally: Arc::new(Mutex::new(LimiterTally::default())) };
        s.spawn_monitor();
        s
    }

    pub fn stop(&self) {
        if !self.shared.stop.swap(true, Ordering::AcqRel) {
            crate::aelog!("[RADIO] session stopped after {:.1} s", self.shared.elapsed_s());
        }
        if let Some(l) = self.shared.live.get() {
            l.close();
        }
    }

    pub fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    /// The depth BIT-PERFECT opens the device at: the stream's own when it
    /// has one (FLAC's), else 0 — a lossy decoder's output, which goes out
    /// in the widest integer the device takes (`OutputMode::Direct`).
    pub fn direct_bits(&self) -> u32 {
        self.shared.info.lock().unwrap().bits.unwrap_or(0)
    }

    /// The timeline the chain plays into: the device's clock for the log.
    pub fn set_timeline(&self, t: Option<Arc<Timeline>>) {
        *self.shared.timeline.lock().unwrap() = t;
    }

    pub fn mark(&self, f: impl FnOnce(&mut RadioInfo, u64)) {
        let ms = self.shared.t0.elapsed().as_millis() as u64;
        f(&mut self.shared.info.lock().unwrap(), ms);
    }

    /// The failure that ended the session, with its kind (ERR_*): every kind
    /// the page has words for is kept as it is (an encrypted stream, say);
    /// any other is OTHER, and the page shows its text.
    pub fn failure(&self) -> Option<(&'static str, String)> {
        let i = self.shared.info.lock().unwrap();
        let e = i.error.clone()?;
        let kind = match i.error_kind.as_deref() {
            Some(ERR_UNREACHABLE) => ERR_UNREACHABLE,
            Some(ERR_FORMAT) => ERR_FORMAT,
            Some(ERR_RATE) => ERR_RATE,
            Some(ERR_ENCRYPTED) => ERR_ENCRYPTED,
            _ => ERR_OTHER,
        };
        Some((kind, e))
    }

    /// The failure that ended the session, or STOPPED.
    fn ended(&self) -> Option<(&'static str, String)> {
        self.failure().or_else(|| self.stopped().then(|| (STOPPED, STOPPED.to_string())))
    }

    /// Wait for the stream's format (the live source is made with it).
    pub fn wait_live(&self, timeout: Duration) -> Result<Arc<LiveSource>, (&'static str, String)> {
        let until = Instant::now() + timeout;
        loop {
            if let Some(e) = self.ended() {
                return Err(e);
            }
            if let Some(l) = self.shared.live.get() {
                return Ok(l.clone());
            }
            if Instant::now() > until {
                return Err((ERR_UNREACHABLE, "no audio from the stream".into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the source holds `need` frames.
    pub fn wait_frames(&self, live: &LiveSource, need: i64) -> Result<(), (&'static str, String)> {
        loop {
            if let Some(e) = self.ended() {
                return Err(e);
            }
            if live.end() >= need {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Where the session is, for the page: "connecting" (no stream yet, or
    /// what the chain waits for is short), "waiting" (a long linear phase's
    /// look-ahead filling: the seconds left go with it), "playing",
    /// "reconnecting" (on the air, the connection lost and being made
    /// again), "failed".
    pub fn phase(&self) -> (&'static str, Option<f64>) {
        let i = self.shared.info.lock().unwrap();
        if i.error.is_some() {
            return ("failed", None);
        }
        if i.t_on_air_ms.is_none() {
            let wait = self
                .shared
                .live
                .get()
                .filter(|_| i.need_s > 0.0)
                .map(|l| (i.need_s - l.end() as f64 / l.rate().max(1) as f64).max(0.0));
            return match wait {
                Some(w) if i.delay_s >= 1.0 && w >= 1.0 => ("waiting", Some(w)),
                w => ("connecting", w),
            };
        }
        (if i.connected { "playing" } else { "reconnecting" }, None)
    }

    fn spawn_monitor(&self) {
        let sh = self.shared.clone();
        let tally = self.tally.clone();
        let period = env_f64("AURA_RADIO_LOG_S", 10.0).max(1.0);
        let _ = std::thread::Builder::new().name("aura-radio-monitor".into()).spawn(move || {
            let mut next_log = period;
            let mut fb: Option<Instant> = None;
            let mut watch = DriftWatch::default();
            while !sh.stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(250));
                let live = match sh.live.get() {
                    Some(l) => l.clone(),
                    None => continue,
                };
                let st = live.stats();
                let rate = live.rate() as f64;
                // The arrival curve: audio in hand after the first byte.
                {
                    let mut i = sh.info.lock().unwrap();
                    if fb.is_none() && i.t_first_byte_ms.is_some() {
                        fb = Some(Instant::now());
                    }
                    if let Some(f) = fb {
                        let t = f.elapsed().as_secs_f64();
                        if t <= 15.0 {
                            i.arrival.push((t, st.real_frames as f64 / rate));
                        }
                    }
                }
                let t = sh.elapsed_s();
                watch.step(&sh, &live, &st, t);
                if t >= next_log {
                    next_log += period;
                    let dev = sh.timeline.lock().unwrap().as_ref().map(|tl| (tl.read_pos(), tl.rate(), tl.underrun_frames()));
                    let lt = tally.lock().unwrap().clone();
                    let i = sh.info.lock().unwrap().clone();
                    let src = sh.source_tally.lock().unwrap().clone();
                    let drift = {
                        let d = sh.drift.lock().unwrap();
                        format!(
                            "level={} target={} ppm={} splices={} cut={:.3}s repeated={:.3}s gapcut={:.2}s",
                            d.level_s.map_or("-".into(), |v| format!("{:.3}s", v)),
                            d.policy.target().map_or("-".into(), |v| format!("{:.3}s", v)),
                            d.policy.drift_ppm().map_or("-".into(), |v| format!("{:+.1}", v)),
                            d.splices,
                            d.cut_s,
                            d.repeated_s,
                            d.gap_cut_s
                        )
                    };
                    crate::aelog!(
                        "[RADIO] t={:.1}s buffer={:.2}s in={:.2}s silent={:.2}s starves={} gaps={} maxwait={}ms dropped={} device={} underruns={} lim={:.4}%/{:.2}dB/{:.2}dB isp={}/{}/{} hot={} held={:.2}s bytes={} connects={} resets={} {} title={}",
                        t,
                        sh.buffer_s().unwrap_or(0.0),
                        st.real_frames as f64 / rate,
                        st.silent_frames as f64 / rate,
                        st.starves,
                        st.gaps,
                        st.max_wait_ms,
                        st.dropped_frames,
                        dev.map_or("-".to_string(), |(p, r, _)| format!("{:.3}s", p as f64 / r.max(1) as f64)),
                        dev.map_or(0, |d| d.2),
                        if lt.samples > 0 { 100.0 * lt.reduced as f64 / lt.samples as f64 } else { 0.0 },
                        if lt.reduced > 0 { lt.reduced_db_sum / lt.reduced as f64 } else { 0.0 },
                        lt.max_db,
                        src.isp_clusters,
                        src.isp_fixed,
                        src.isp_unfixed,
                        src.isp_hot,
                        src.frames_in.saturating_sub(src.frames_out) as f64 / rate,
                        i.bytes,
                        i.connects,
                        i.resets,
                        drift,
                        i.title
                    );
                }
            }
        });
    }

    /// The `radio` object of the player's status. `heard`: the source frame
    /// being heard, when the stream's chain is on the air.
    pub fn status_json(&self, heard: Option<i64>) -> Value {
        let (phase, wait_s) = self.phase();
        let now = self.shared.title_at(heard);
        let i = self.shared.info.lock().unwrap().clone();
        let (live, rate) = match self.shared.live.get() {
            Some(l) => (Some(l.stats()), l.rate() as f64),
            None => (None, 1.0),
        };
        let lt = self.tally.lock().unwrap().clone();
        let ahead = self.shared.live.get().map(|l| l.ahead() as f64 / rate);
        let src = self.shared.source_tally.lock().unwrap().clone();
        let drift = {
            let d = self.shared.drift.lock().unwrap();
            json!({
                "levelS": d.level_s,
                "targetS": d.policy.target(),
                "devS": d.policy.deviation(),
                "bandS": d.policy.band_s(),
                "ppm": d.policy.drift_ppm(),
                "splices": d.splices,
                "cutS": d.cut_s,
                "repeatedS": d.repeated_s,
                "gapCutS": d.gap_cut_s,
                // Each splice: [session s, seconds (cut > 0), dBFS, dB under the loudest].
                "places": d.log,
            })
        };
        json!({
            // The delay's corridor (`drift`).
            "drift": drift,
            "phase": phase,
            "waitS": wait_s,
            // A stream's own linear filter is being made (once): the stream
            // may be waiting for it (`stream_linear`).
            "preparing": crate::audio::converter::dsp::lab::stream_linear::preparing(),
            // What is heard now: {at, text, album, from}, null before anything is.
            "now": now,
            "heardFrame": heard,
            "info": i,
            "source": src,
            "stopped": self.stopped(),
            "elapsedS": self.shared.elapsed_s(),
            "bufferS": self.shared.buffer_s(),
            // What the live source alone holds past the reader (the slow gain's read-ahead not counted).
            "aheadS": ahead,
            // Gathered before a starved stream goes on (grows after each starve).
            "targetS": self.shared.live.get().map(|l| l.resume_s()),
            "starvedNow": self.shared.live.get().map(|l| l.starved()),
            "inS": live.as_ref().map(|s| s.real_frames as f64 / rate),
            "silentS": live.as_ref().map(|s| s.silent_frames as f64 / rate),
            "starves": live.as_ref().map(|s| s.starves),
            "gaps": live.as_ref().map(|s| s.gaps),
            "maxWaitMs": live.as_ref().map(|s| s.max_wait_ms),
            "droppedS": live.as_ref().map(|s| s.dropped_frames as f64 / rate),
            "limiter": {
                "samples": lt.samples,
                "reducedPct": if lt.samples > 0 { 100.0 * lt.reduced as f64 / lt.samples as f64 } else { 0.0 },
                "meanDb": if lt.reduced > 0 { lt.reduced_db_sum / lt.reduced as f64 } else { 0.0 },
                "maxDb": lt.max_db,
                "windowsLimited": lt.windows_limited,
            },
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_heard_from_the_frame_the_stream_had_reached() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        assert_eq!(sh.title_at(Some(0)), None, "nothing said yet");
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 500], &[0.0; 500]);
        // The first title stands for the stream from its start.
        sh.set_title("A - One", "icy");
        live.push(&[0.0; 700], &[0.0; 700]);
        // The next one from where the stream is now (1200).
        sh.set_title_with("B - Two", Some("Album · 2001".into()), "rp");
        sh.set_title("B - Two", "icy"); // the same song from another source: nothing new
        sh.set_title("B - Two", "icy"); // said again: nothing new
        assert_eq!(sh.titles.lock().unwrap().len(), 2);
        assert_eq!(sh.title_at(None), None, "nothing heard: no title");
        assert_eq!(sh.title_at(Some(0)).unwrap().text, "A - One");
        assert_eq!(sh.title_at(Some(1199)).unwrap().text, "A - One");
        let b = sh.title_at(Some(1200)).unwrap();
        assert_eq!((b.text.as_str(), b.at, b.album.as_deref()), ("B - Two", 1200, Some("Album · 2001")));
        assert_eq!(sh.info.lock().unwrap().title, "B - Two");
    }

    /// Every song start the stream knows, from the first.
    fn starts(sh: &RadioShared) -> Vec<i64> {
        let mut v = Vec::new();
        while let Some(b) = sh.songs.first_from(v.last().map_or(0, |&b| b + 1)) {
            v.push(b);
        }
        v
    }

    #[test]
    fn the_song_playing_said_again_with_its_album_is_not_a_new_song() {
        // Radio Paradise's MP3: the ICY title first, the list's answer ~10 s
        // later with the same words and the album (Anton 3.10: the song was
        // cut in two there, and its slow gain started afresh).
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.set_title("A - One", "icy");
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.set_title("B - Two", "icy");
        live.push(&vec![0.0; 10_000], &vec![0.0; 10_000]);
        sh.rp_now_playing("B - Two", Some("Album · 2001".into()), 200.0);
        assert_eq!(sh.titles.lock().unwrap().len(), 2, "one song");
        let b = sh.title_at(Some(12_000)).unwrap();
        assert_eq!((b.at, b.album.as_deref(), b.from.as_str()), (2000, Some("Album · 2001"), "icy"),
            "heard from its own start, with the album the list adds");
        assert_eq!(starts(&sh), vec![2000], "no song start where the list said it again");
        assert_eq!(sh.info.lock().unwrap().titles, 2);
    }

    #[test]
    fn the_same_words_from_the_list_first_and_icy_after_are_one_song() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.rp_now_playing("A - One", None, 300.0);
        live.push(&[0.0; 2000], &[0.0; 2000]);
        sh.rp_now_playing("B - Two", Some("Album · 2001".into()), 300.0);
        live.push(&[0.0; 3000], &[0.0; 3000]);
        // ICY says it too, no album: nothing new, the album kept.
        sh.set_title("B - Two", "icy");
        assert_eq!(sh.titles.lock().unwrap().len(), 2);
        assert_eq!(sh.title_at(Some(6000)).unwrap().album.as_deref(), Some("Album · 2001"));
        assert_eq!(starts(&sh), vec![3000]);
    }

    #[test]
    fn other_words_are_a_new_song_and_the_same_words_after_them_too() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.set_title("A - One", "icy");
        live.push(&[0.0; 2000], &[0.0; 2000]);
        sh.set_title("B - Two", "icy");
        live.push(&[0.0; 2000], &[0.0; 2000]);
        sh.set_title("A - One", "icy"); // played again after another: a repeat
        assert_eq!(sh.titles.lock().unwrap().len(), 3);
        assert_eq!(starts(&sh), vec![3000, 5000]);
        assert_eq!(sh.title_at(Some(5000)).unwrap().text, "A - One");
    }

    #[test]
    fn a_chained_ogg_stream_naming_the_song_playing_begins_no_song() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.chained(Some("A - One"));
        live.push(&[0.0; 2000], &[0.0; 2000]);
        // Chained anew with the same tags: the same song.
        sh.chained(Some("A - One"));
        assert_eq!(starts(&sh), vec![1000], "the first logical stream's start, as before; none for the same song");
        live.push(&[0.0; 2000], &[0.0; 2000]);
        sh.chained(Some("B - Two"));
        live.push(&[0.0; 2000], &[0.0; 2000]);
        // No tags at all: a song begins all the same.
        sh.chained(None);
        assert_eq!(starts(&sh), vec![1000, 5000, 7000]);
        assert_eq!(sh.titles.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_title_counts_in_what_the_source_stages_still_hold() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&[0.0; 1000], &[0.0; 1000]);
        sh.set_title("A - One", "icy");
        {
            let mut t = sh.source_tally.lock().unwrap();
            t.frames_in = 1500;
            t.frames_out = 1000;
        }
        // 500 frames are in the stages: the new title is heard from 1500.
        sh.set_title("B - Two", "icy");
        assert_eq!(sh.title_at(Some(1500)).unwrap().at, 1500);
        assert_eq!(sh.songs.first_from(0), Some(1500), "the second title begins a song; the first does not");
        // A chained Ogg stream's next song, said twice within a second: one start.
        live.push(&[0.0; 3000], &[0.0; 3000]);
        sh.song_starts_here();
        sh.songs.push(4600, 1000);
        assert_eq!(sh.songs.first_from(1501), Some(4500));
        assert_eq!(sh.songs.first_from(4501), None);
    }

    #[test]
    fn radio_paradise_titles_begin_where_its_list_said_the_song_would_end() {
        let sh = RadioShared::new("https://stream.radioparadise.com/flac", &PlayerSettings::default());
        let live = sh.live_for(1000).unwrap();
        live.push(&vec![0.0; 2000], &vec![0.0; 2000]);
        sh.rp_now_playing("A - One", None, 30.0);
        // Asked again before the change: nothing new, the end foretold anew
        // (12 000 + 20 s).
        live.push(&vec![0.0; 10_000], &vec![0.0; 10_000]);
        sh.rp_now_playing("A - One", None, 20.0);
        // Asked 2 s after it: the next title from the end foretold.
        live.push(&vec![0.0; 22_000], &vec![0.0; 22_000]);
        sh.rp_now_playing("B - Two", Some("Album · 2001".into()), 100.0);
        assert_eq!(sh.title_at(Some(31_999)).unwrap().text, "A - One");
        let b = sh.title_at(Some(32_000)).unwrap();
        assert_eq!((b.text.as_str(), b.at, b.from.as_str()), ("B - Two", 32_000, "rp"));
        assert_eq!(sh.songs.first_from(0), Some(32_000));
        // A change long before the end foretold (the list's answer moved on
        // early): heard from the stream's edge.
        live.push(&vec![0.0; 50_000], &vec![0.0; 50_000]);
        sh.rp_now_playing("C - Three", None, 100.0);
        assert_eq!(sh.title_at(Some(84_000)).unwrap().at, 84_000);
    }

    #[test]
    fn the_adaptive_headroom_looks_at_the_first_seconds_once() {
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        sh.first_look_push(&vec![0.5; 4000], &vec![0.25; 4000], 1000);
        sh.first_look_push(&vec![0.9; 8000], &vec![0.9; 8000], 1000);
        // 10 s at 1 kHz: the louder part from 4 s on is in them.
        assert_eq!(sh.first_look(|st, secs| (st.unwrap().peak_lin, secs)), (0.9, 10.0));
        sh.first_look_push(&vec![1.0; 100], &vec![1.0; 100], 1000);
        assert_eq!(sh.first_look(|st, _| st.unwrap().peak_lin), 0.9, "what comes after is not looked at");
        // A chain built before 10 s are in decides on what is in, and that
        // stays the session's.
        let early = RadioShared::new("http://x/stream", &PlayerSettings::default());
        early.first_look_push(&vec![0.5; 3000], &vec![0.5; 3000], 1000);
        assert_eq!(early.first_look(|st, secs| (st.unwrap().peak_lin, secs)), (0.5, 3.0));
        early.first_look_push(&vec![0.9; 3000], &vec![0.9; 3000], 1000);
        assert_eq!(early.first_look(|st, secs| (st.unwrap().peak_lin, secs)), (0.5, 3.0));
    }

    #[test]
    fn failures_are_named_by_kind() {
        assert_eq!(kind_of("unsupported codec (opus): x"), ERR_FORMAT);
        assert_eq!(kind_of("unsupported stream format (text/html): x"), ERR_FORMAT);
        assert_eq!(kind_of("22050 Hz: only the 44.1 kHz and 48 kHz families can be upsampled"), ERR_RATE);
        assert_eq!(kind_of("HLS streams are not supported yet"), ERR_FORMAT);
        assert_eq!(kind_of("the filter fir_1M is missing"), ERR_OTHER);
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        sh.fail_kind(ERR_UNREACHABLE, "HTTP 404".into());
        let i = sh.info.lock().unwrap().clone();
        assert_eq!((i.error.as_deref(), i.error_kind.as_deref()), (Some("HTTP 404"), Some(ERR_UNREACHABLE)));
        assert!(sh.stop.load(Ordering::Acquire));
    }

    /// What the page is told a session ended on (`failure`, the kind the
    /// status carries): each kind it has words for comes through as itself —
    /// an encrypted stream came as OTHER, and the page showed the backend's
    /// own text in its place.
    #[test]
    fn a_session_ends_on_the_kind_it_failed_with() {
        let session = |kind: &'static str, e: &str| {
            let s = Session {
                shared: Arc::new(RadioShared::new("http://x/stream", &PlayerSettings::default())),
                tally: Arc::new(Mutex::new(LimiterTally::default())),
            };
            s.shared.fail_kind(kind, e.to_string());
            s.failure()
        };
        for kind in [ERR_UNREACHABLE, ERR_FORMAT, ERR_RATE, ERR_ENCRYPTED] {
            assert_eq!(session(kind, "why"), Some((kind, "why".to_string())), "{kind}");
        }
        assert_eq!(session(ERR_OTHER, "the filter fir_1M is missing"),
            Some((ERR_OTHER, "the filter fir_1M is missing".to_string())));
        assert_eq!(session("a kind nobody named", "x"), Some((ERR_OTHER, "x".to_string())));
        // Off the words too, as the HLS reader and the decoder fail.
        let s = Session {
            shared: Arc::new(RadioShared::new("http://x/stream", &PlayerSettings::default())),
            tally: Arc::new(Mutex::new(LimiterTally::default())),
        };
        s.shared.fail("unsupported codec (ac-3): x".into());
        assert_eq!(s.failure().map(|f| f.0), Some(ERR_FORMAT));
        let none = Session {
            shared: Arc::new(RadioShared::new("http://x/stream", &PlayerSettings::default())),
            tally: Arc::new(Mutex::new(LimiterTally::default())),
        };
        assert_eq!(none.failure(), None, "a session that did not fail");
    }
}
