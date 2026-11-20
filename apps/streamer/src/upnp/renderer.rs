//! The renderer's one state, shared by its two faces (UPnP AV and OpenHome):
//! a list of tracks the control point gave by address, the one playing, and
//! what the listener asked for (play, pause, stop). A tick keeps the engine
//! in step: it fetches the current track and the next into a cache, queues
//! them in a list of their own in the engine (so the engine goes on to the
//! next one gaplessly), starts, follows and ends them.
//!
//! The engine plays files: a track sent by address is fetched whole before it
//! starts. A live stream (internet radio) goes to the engine's radio instead.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use aura_core::commands as engine;
use serde_json::Value;

use super::media::{self, Fetched};
use super::xml;

/// The engine's playlist id for the renderer's tracks: no page's id (a page's
/// ids are JavaScript numbers, below 2^53).
pub const UPNP_LIST: u64 = 0xA0A0_0000_0000_0001;

/// The engine's track ids that are the renderer's: a page reloading does not
/// take them off the queue (`dispatch`), as it does with tracks not in its list.
static OWNED: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();

fn owned() -> &'static Mutex<HashSet<u64>> {
    OWNED.get_or_init(Default::default)
}

/// Whether `id` is one of the renderer's tracks in the engine.
pub fn owns(id: u64) -> bool {
    owned().lock().unwrap().contains(&id)
}

/// What a control point said about a track (DIDL-Lite), as far as it is used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: String,
    pub class: String,
    pub mime: String,
    pub duration_s: Option<f64>,
}

impl Meta {
    pub fn from_didl(didl: &str) -> Meta {
        let Some(item) = xml::find(didl, "item") else { return Meta::default() };
        let text = |n: &str| xml::find(item.inner, n).map(|e| e.text().trim().to_string()).unwrap_or_default();
        let res = xml::find(item.inner, "res");
        let protocol = res.as_ref().and_then(|r| r.attr("protocolInfo")).unwrap_or_default();
        let artist = match text("artist") {
            a if a.is_empty() => text("creator"),
            a => a,
        };
        Meta {
            title: text("title"),
            artist,
            album: text("album"),
            art: text("albumArtURI"),
            class: text("class"),
            mime: protocol.split(':').nth(2).unwrap_or("").to_ascii_lowercase(),
            duration_s: res.and_then(|r| r.attr("duration")).and_then(|d| parse_time(&d)),
        }
    }

    pub fn is_broadcast(&self) -> bool {
        self.class.contains("audioBroadcast")
    }
}

/// One track of the renderer's list.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: u32,
    pub uri: String,
    /// DIDL-Lite as the control point sent it (given back as it is).
    pub didl: String,
    pub meta: Meta,
}

/// Where a track is on its way to the engine.
#[derive(Clone, Debug)]
enum Stage {
    Fetching,
    /// In the cache; `track`: its id in the engine once queued.
    Ready { path: PathBuf, track: Option<u64>, duration_s: f64 },
    /// A live stream: played by the engine's radio.
    Stream,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Stopped,
    Playing,
    Paused,
}

/// What a control point is told about the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    NoMedia,
    Stopped,
    Playing,
    Paused,
    /// Fetching, or the engine preparing.
    Transitioning,
}

impl Transport {
    pub fn av(self) -> &'static str {
        match self {
            Transport::NoMedia => "NO_MEDIA_PRESENT",
            Transport::Stopped => "STOPPED",
            Transport::Playing => "PLAYING",
            Transport::Paused => "PAUSED_PLAYBACK",
            Transport::Transitioning => "TRANSITIONING",
        }
    }

    pub fn oh(self) -> &'static str {
        match self {
            Transport::NoMedia | Transport::Stopped => "Stopped",
            Transport::Playing => "Playing",
            Transport::Paused => "Paused",
            Transport::Transitioning => "Buffering",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Off,
    One,
    All,
}

pub struct State {
    pub entries: Vec<Entry>,
    /// The entry playing or to play.
    pub current: Option<u32>,
    pub want: Want,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// The list's version: OpenHome's IdArray token.
    pub list_rev: u32,
    /// Only the AV face is in use: entries before the current one are
    /// forgotten as it moves on (AV knows the current and the next only).
    pub av_only: bool,
    next_id: u32,
    stages: Vec<(u32, Stage)>,
    /// The engine track asked to play, the engine's track at that moment,
    /// and whether it was seen playing since.
    asked: Option<Asked>,
    /// Where to go once the current track starts (a seek before it did).
    seek_after_start: Option<f64>,
    /// Muted: the volume to go back to.
    muted_from: Option<f64>,
    /// The engine's last word.
    pub seen: Seen,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct Asked {
    track: Option<u64>,
    /// The engine's track when the play was asked: a page playing another
    /// one after that has taken the player over.
    engine_before: Option<u64>,
    played: bool,
    at: Instant,
}

/// The engine's status, as far as the renderer reads it.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub state: String,
    pub track: Option<u64>,
    pub position_s: f64,
    pub volume_db: f64,
    pub radio: bool,
    pub radio_title: String,
    pub src_rate: u32,
    pub bits: u32,
}

impl Seen {
    pub fn read(s: &Value) -> Seen {
        let radio = !s["radio"].is_null();
        Seen {
            state: s["state"].as_str().unwrap_or("stopped").to_string(),
            track: s["trackId"].as_u64(),
            position_s: s["positionS"].as_f64().unwrap_or(0.0),
            volume_db: s["volumeDb"].as_f64().unwrap_or(0.0),
            radio,
            radio_title: s["radio"]["now"].as_str().or(s["radio"]["now"]["title"].as_str()).unwrap_or("").to_string(),
            src_rate: s["srcRate"].as_u64().unwrap_or(0) as u32,
            bits: s["bits"].as_u64().unwrap_or(0) as u32,
        }
    }
}

pub struct Renderer {
    pub st: Mutex<State>,
    cache: Option<PathBuf>,
    rt: tokio::runtime::Handle,
    client: reqwest::Client,
    /// Something a control point may want to hear about changed.
    pub changed: tokio::sync::Notify,
    /// The engine's calls, in order: the tick and the actions do not cross.
    engine_lock: Mutex<()>,
}

impl Renderer {
    pub fn new(cache: Option<PathBuf>, rt: tokio::runtime::Handle) -> Arc<Renderer> {
        if let Some(c) = &cache {
            // What a run before left.
            let _ = std::fs::remove_dir_all(c);
            let _ = std::fs::create_dir_all(c);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("AuraStreamer/", env!("CARGO_PKG_VERSION"), " UPnP/1.0 DLNADOC/1.50"))
            .build()
            .unwrap_or_default();
        Arc::new(Renderer {
            st: Mutex::new(State {
                entries: Vec::new(),
                current: None,
                want: Want::Stopped,
                repeat: Repeat::Off,
                shuffle: false,
                list_rev: 1,
                av_only: true,
                next_id: 1,
                stages: Vec::new(),
                asked: None,
                seek_after_start: None,
                muted_from: None,
                seen: Seen::default(),
                error: None,
            }),
            cache,
            rt,
            client,
            changed: tokio::sync::Notify::new(),
            engine_lock: Mutex::new(()),
        })
    }

    fn poke(&self) {
        self.changed.notify_waiters();
    }

    // ── the list ───────────────────────────────────────────────────────────

    /// Add a track after `after` (0: at the top); its new id.
    pub fn insert(&self, after: u32, uri: String, didl: String) -> Result<u32, &'static str> {
        let mut st = self.st.lock().unwrap();
        let at = if after == 0 {
            0
        } else {
            st.entries.iter().position(|e| e.id == after).ok_or("no such id")? + 1
        };
        let id = st.next_id;
        st.next_id += 1;
        let meta = Meta::from_didl(&didl);
        st.entries.insert(at, Entry { id, uri, didl, meta });
        st.list_rev = st.list_rev.wrapping_add(1);
        drop(st);
        self.poke();
        Ok(id)
    }

    pub fn delete(&self, id: u32) {
        let mut st = self.st.lock().unwrap();
        let Some(i) = st.entries.iter().position(|e| e.id == id) else { return };
        if st.current == Some(id) {
            // The one after it is current now; what plays goes on until told.
            st.current = st.entries.get(i + 1).map(|e| e.id);
            if st.current.is_none() {
                st.want = Want::Stopped;
            }
            st.asked = None;
        }
        st.entries.remove(i);
        st.list_rev = st.list_rev.wrapping_add(1);
        drop(st);
        self.poke();
    }

    pub fn delete_all(&self) {
        let mut st = self.st.lock().unwrap();
        st.entries.clear();
        st.current = None;
        st.want = Want::Stopped;
        st.asked = None;
        st.list_rev = st.list_rev.wrapping_add(1);
        drop(st);
        self.stop_engine_if_ours();
        self.poke();
    }

    /// AV's SetAVTransportURI: the list is this one track. Playing goes on
    /// with it; otherwise it waits for Play.
    pub fn set_uri(&self, uri: String, didl: String) {
        {
            let mut st = self.st.lock().unwrap();
            st.av_only = true;
            st.entries.clear();
            let id = st.next_id;
            st.next_id += 1;
            let meta = Meta::from_didl(&didl);
            st.entries.push(Entry { id, uri, didl, meta });
            st.current = Some(id);
            st.asked = None;
            st.error = None;
            st.seek_after_start = None;
            st.list_rev = st.list_rev.wrapping_add(1);
            if st.want == Want::Paused {
                st.want = Want::Stopped;
            }
        }
        self.poke();
    }

    /// AV's SetNextAVTransportURI: the track after the current one (an empty
    /// address takes it away).
    pub fn set_next_uri(&self, uri: String, didl: String) {
        {
            let mut st = self.st.lock().unwrap();
            let keep = match st.current.and_then(|c| st.entries.iter().position(|e| e.id == c)) {
                Some(i) => i + 1,
                None => st.entries.len(),
            };
            st.entries.truncate(keep);
            if !uri.is_empty() {
                let id = st.next_id;
                st.next_id += 1;
                let meta = Meta::from_didl(&didl);
                st.entries.push(Entry { id, uri, didl, meta });
            }
            st.list_rev = st.list_rev.wrapping_add(1);
        }
        self.poke();
    }

    /// OpenHome uses the list as a list: entries stay as tracks go by.
    pub fn use_as_playlist(&self) {
        self.st.lock().unwrap().av_only = false;
    }

    // ── the transport ──────────────────────────────────────────────────────

    pub fn play(&self) {
        let paused_ours = {
            let mut st = self.st.lock().unwrap();
            if st.current.is_none() {
                st.current = st.entries.first().map(|e| e.id);
            }
            if st.current.is_none() {
                return;
            }
            let was = st.want;
            st.want = Want::Playing;
            st.error = None;
            was == Want::Paused && st.asked.is_some_and(|a| a.played)
        };
        if paused_ours {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_play(None);
        }
        self.poke();
        self.tick();
    }

    pub fn pause(&self) {
        let ours = {
            let mut st = self.st.lock().unwrap();
            if st.want != Want::Playing {
                return;
            }
            st.want = Want::Paused;
            st.asked.is_some()
        };
        if ours && self.engine_on_ours() {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_pause();
        }
        self.poke();
    }

    pub fn stop(&self) {
        {
            let mut st = self.st.lock().unwrap();
            st.want = Want::Stopped;
            st.seek_after_start = None;
        }
        self.stop_engine_if_ours();
        self.st.lock().unwrap().asked = None;
        self.poke();
    }

    fn stop_engine_if_ours(&self) {
        if self.engine_on_ours() {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_stop();
        }
    }

    /// Whether what the engine plays (or holds paused) is the renderer's.
    fn engine_on_ours(&self) -> bool {
        let st = self.st.lock().unwrap();
        let seen = &st.seen;
        match st.asked {
            None => false,
            Some(a) if a.track.is_none() => seen.radio,
            Some(_) => seen.track.is_some_and(owns),
        }
    }

    /// Go to entry `id` (OpenHome's SeekId, AV's Next) and play it.
    pub fn jump(&self, id: u32) -> bool {
        {
            let mut st = self.st.lock().unwrap();
            if !st.entries.iter().any(|e| e.id == id) {
                return false;
            }
            st.current = Some(id);
            st.asked = None;
            st.want = Want::Playing;
            st.seek_after_start = None;
            st.error = None;
        }
        self.poke();
        self.tick();
        true
    }

    /// The entry `step` places from the current one (repeat all wraps).
    pub fn neighbour(&self, step: i64) -> Option<u32> {
        let st = self.st.lock().unwrap();
        let n = st.entries.len() as i64;
        let i = st.current.and_then(|c| st.entries.iter().position(|e| e.id == c))? as i64 + step;
        let i = if st.repeat == Repeat::All && n > 0 { i.rem_euclid(n) } else { i };
        (0..n).contains(&i).then(|| st.entries[i as usize].id)
    }

    pub fn seek(&self, seconds: f64) {
        let on = {
            let mut st = self.st.lock().unwrap();
            let started = st.asked.is_some_and(|a| a.played);
            if !started {
                st.seek_after_start = Some(seconds);
            }
            started
        };
        if on && self.engine_on_ours() {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_seek(seconds);
        }
        self.poke();
    }

    // ── the volume ─────────────────────────────────────────────────────────

    /// 0…100 as the AV and OpenHome faces count it: 0.6 dB a step, 0 is
    /// silence. The engine's volume is the page's too.
    pub fn volume(&self) -> u32 {
        let st = self.st.lock().unwrap();
        db_to_steps(st.muted_from.unwrap_or(st.seen.volume_db))
    }

    pub fn volume_db(&self) -> f64 {
        let st = self.st.lock().unwrap();
        st.muted_from.unwrap_or(st.seen.volume_db)
    }

    pub fn set_volume(&self, steps: u32) {
        self.set_volume_db(steps_to_db(steps));
    }

    pub fn set_volume_db(&self, db: f64) {
        let db = db.clamp(-120.0, 0.0);
        let muted = {
            let mut st = self.st.lock().unwrap();
            st.seen.volume_db = db;
            if st.muted_from.is_some() {
                st.muted_from = Some(db);
                true
            } else {
                false
            }
        };
        if !muted {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_set_volume(db);
        }
        self.poke();
    }

    pub fn muted(&self) -> bool {
        self.st.lock().unwrap().muted_from.is_some()
    }

    pub fn set_mute(&self, on: bool) {
        let to = {
            let mut st = self.st.lock().unwrap();
            match (on, st.muted_from) {
                (true, None) => {
                    st.muted_from = Some(st.seen.volume_db);
                    Some(-120.0)
                }
                (false, Some(db)) => {
                    st.muted_from = None;
                    Some(db)
                }
                _ => None,
            }
        };
        if let Some(db) = to {
            let _e = self.engine_lock.lock().unwrap();
            engine::player_set_volume(db);
        }
        self.poke();
    }

    // ── what is told ───────────────────────────────────────────────────────

    pub fn transport(&self) -> Transport {
        let st = self.st.lock().unwrap();
        transport_of(&st)
    }

    /// The current entry, and where it stands: (position, duration) in seconds.
    pub fn now(&self) -> Option<(Entry, f64, f64)> {
        let st = self.st.lock().unwrap();
        let e = st.current.and_then(|c| st.entries.iter().find(|e| e.id == c))?.clone();
        let stage = stage_of(&st, e.id);
        let ours_on_air = match stage {
            Some(Stage::Ready { track: Some(t), .. }) => st.seen.track == Some(*t),
            Some(Stage::Stream) => st.seen.radio,
            _ => false,
        };
        let known = match stage {
            Some(Stage::Ready { duration_s, .. }) if *duration_s > 0.0 => Some(*duration_s),
            _ => None,
        };
        let duration = known.or(e.meta.duration_s).unwrap_or(0.0);
        let position = if ours_on_air { st.seen.position_s } else { 0.0 };
        Some((e, position, duration))
    }

    /// The entry after the current one.
    pub fn next_entry(&self) -> Option<Entry> {
        let st = self.st.lock().unwrap();
        let i = st.current.and_then(|c| st.entries.iter().position(|e| e.id == c))?;
        st.entries.get(i + 1).cloned()
    }

    pub fn seen(&self) -> Seen {
        self.st.lock().unwrap().seen.clone()
    }

    // ── the tick ───────────────────────────────────────────────────────────

    /// Keep the engine in step. Called a few times a second, and after each
    /// action.
    pub fn tick(self: &Renderer) {
        let _e = self.engine_lock.lock().unwrap();
        let seen = Seen::read(&engine::player_status());
        let mut todo = Vec::new();
        let before;
        {
            let mut st = self.st.lock().unwrap();
            before = (transport_of(&st), st.current, st.seen.volume_db, st.list_rev);
            let prev = std::mem::replace(&mut st.seen, seen.clone());
            self.follow(&mut st, &prev, &seen);
            self.plan(&mut st, &seen, &mut todo);
        }
        for t in todo {
            self.run(t);
        }
        let st = self.st.lock().unwrap();
        let after = (transport_of(&st), st.current, st.seen.volume_db, st.list_rev);
        drop(st);
        if before != after {
            self.poke();
        }
    }

    /// Read what the engine did: went on to the next track, ended, or was
    /// taken over by a page.
    fn follow(&self, st: &mut State, prev: &Seen, seen: &Seen) {
        let Some(asked) = st.asked else { return };
        let playing_now = matches!(seen.state.as_str(), "playing" | "paused");
        // The engine went on to the next of the renderer's tracks.
        if let Some(t) = seen.track.filter(|&t| Some(t) != asked.track && owns(t)) {
            if let Some(id) = entry_of_track(st, t).filter(|id| st.entries.iter().any(|e| e.id == *id)) {
                st.current = Some(id);
                st.asked = Some(Asked { track: Some(t), engine_before: asked.engine_before, played: true, at: Instant::now() });
                if st.av_only {
                    if let Some(i) = st.entries.iter().position(|e| e.id == id) {
                        st.entries.drain(..i);
                        st.list_rev = st.list_rev.wrapping_add(1);
                    }
                }
                return;
            }
        }
        let on_ours = match asked.track {
            None => seen.radio,
            Some(t) => seen.track == Some(t),
        };
        if on_ours && playing_now {
            if !asked.played {
                st.asked = Some(Asked { played: true, ..asked });
                if let Some(at) = st.seek_after_start.take() {
                    engine::player_seek(at);
                }
            }
            return;
        }
        // A page played something else: the control point is told it stopped.
        let other = if asked.track.is_none() {
            !seen.radio && seen.track.is_some() && seen.track != asked.engine_before
        } else {
            seen.track.is_some_and(|t| !owns(t)) && seen.track != asked.engine_before && seen.state != "stopped"
        };
        if other {
            st.want = Want::Stopped;
            st.asked = None;
            return;
        }
        // It stopped after playing: the end of the track, or a Stop from a page.
        if asked.played && seen.state == "stopped" && st.want == Want::Playing {
            let last = prev.position_s;
            let dur = st.current.and_then(|c| match stage_of(st, c) {
                Some(Stage::Ready { duration_s, .. }) => Some(*duration_s),
                _ => None,
            });
            let ended = asked.track.is_some() && dur.is_some_and(|d| d <= 0.0 || last >= d - 3.0);
            st.asked = None;
            if !ended {
                st.want = Want::Stopped;
                return;
            }
            let i = st.current.and_then(|c| st.entries.iter().position(|e| e.id == c));
            let next = match (i, st.repeat) {
                (Some(i), Repeat::One) => st.entries.get(i).map(|e| e.id),
                (Some(i), Repeat::All) => st.entries.get(i + 1).or(st.entries.first()).map(|e| e.id),
                (Some(i), Repeat::Off) => st.entries.get(i + 1).map(|e| e.id),
                (None, _) => None,
            };
            match next {
                Some(n) => {
                    st.current = Some(n);
                    if st.av_only {
                        if let Some(j) = st.entries.iter().position(|e| e.id == n) {
                            st.entries.drain(..j);
                            st.list_rev = st.list_rev.wrapping_add(1);
                        }
                    }
                }
                None => st.want = Want::Stopped,
            }
        } else if !asked.played && asked.at.elapsed() > Duration::from_secs(120) && seen.state == "stopped" {
            // Asked long ago and nothing came: the engine said why, or not.
            st.asked = None;
            st.want = Want::Stopped;
            st.error = Some("The track did not start".into());
        }
    }

    /// What to do now: fetch, queue, start, let go.
    fn plan(&self, st: &mut State, seen: &Seen, todo: &mut Vec<Todo>) {
        // The tracks wanted in the engine: the current one and the next.
        let i = st.current.and_then(|c| st.entries.iter().position(|e| e.id == c));
        let mut wanted: Vec<u32> = Vec::new();
        if let Some(i) = i {
            wanted.push(st.entries[i].id);
            let next = match st.repeat {
                Repeat::One => None,
                Repeat::All if st.entries.len() > 1 => Some(st.entries.get(i + 1).unwrap_or(&st.entries[0]).id),
                _ => st.entries.get(i + 1).map(|e| e.id),
            };
            if let Some(n) = next.filter(|n| !wanted.contains(n)) {
                wanted.push(n);
            }
        }
        // What is not wanted goes (the track on the air stays until it ends).
        let on_air = seen.track;
        let mut gone = Vec::new();
        st.stages.retain(|(id, stage)| {
            if wanted.contains(id) {
                return true;
            }
            if let Stage::Ready { track: Some(t), .. } = stage {
                if on_air == Some(*t) && seen.state != "stopped" {
                    return true;
                }
            }
            gone.push(stage.clone());
            false
        });
        for g in gone {
            if let Stage::Ready { path, track, .. } = g {
                todo.push(Todo::Forget { path, track });
            }
        }
        // Fetch what is wanted and not on its way.
        for id in &wanted {
            if stage_of(st, *id).is_none() {
                let e = st.entries.iter().find(|e| e.id == *id).cloned();
                if let Some(e) = e {
                    st.stages.push((e.id, Stage::Fetching));
                    todo.push(Todo::Fetch(e));
                }
            }
        }
        // Queue in the engine, in order: the next one only after the current one.
        let unqueued = |st: &State, id: u32| match stage_of(st, id) {
            Some(Stage::Ready { track: None, path, .. }) => Some(path.clone()),
            _ => None,
        };
        if let Some(&cur) = wanted.first() {
            if let Some(path) = unqueued(st, cur) {
                todo.push(Todo::Queue { id: cur, path });
            } else if matches!(stage_of(st, cur), Some(Stage::Ready { track: Some(_), .. }) | Some(Stage::Stream)) {
                if let Some(&n) = wanted.get(1) {
                    if let Some(path) = unqueued(st, n) {
                        todo.push(Todo::Queue { id: n, path });
                    }
                }
            }
        }
        // Start the current one.
        if st.want == Want::Playing && st.asked.is_none() {
            if let Some(&id) = wanted.first() {
                match stage_of(st, id) {
                    Some(Stage::Ready { track: Some(t), .. }) => {
                        let t = *t;
                        st.asked = Some(Asked { track: Some(t), engine_before: seen.track, played: false, at: Instant::now() });
                        todo.push(Todo::Play(t));
                    }
                    Some(Stage::Stream) => {
                        let uri = st.entries.iter().find(|e| e.id == id).map(|e| e.uri.clone()).unwrap_or_default();
                        st.asked = Some(Asked { track: None, engine_before: seen.track, played: false, at: Instant::now() });
                        todo.push(Todo::Radio(uri));
                    }
                    Some(Stage::Failed(why)) => {
                        st.error = Some(why.clone());
                        st.want = Want::Stopped;
                    }
                    _ => {}
                }
            }
        }
    }

    fn run(&self, t: Todo) {
        match t {
            Todo::Fetch(e) => {
                let Some(dir) = self.cache.clone() else {
                    self.fetched(e.id, Err("No place to keep tracks".into()));
                    return;
                };
                let client = self.client.clone();
                let me = renderer();
                self.rt.spawn(async move {
                    let r = media::fetch(&client, &e.uri, &e.meta, &dir, e.id).await;
                    if let Some(me) = me {
                        tokio::task::spawn_blocking(move || {
                            me.fetched(e.id, r);
                            me.tick();
                        });
                    }
                });
            }
            Todo::Queue { id, path } => {
                let r = engine::player_add(vec![path.display().to_string()], Some(UPNP_LIST));
                let track = r["tracks"][0]["id"].as_u64();
                let dur = r["tracks"][0]["durationS"].as_f64().unwrap_or(0.0);
                let mut st = self.st.lock().unwrap();
                match track {
                    Some(t) => {
                        owned().lock().unwrap().insert(t);
                        set_stage(&mut st, id, Stage::Ready { path, track: Some(t), duration_s: dur });
                    }
                    None => {
                        let why = r["errors"][0].as_str().unwrap_or("The track cannot be read").to_string();
                        aura_core::aelog!("[UPNP] {why}");
                        let _ = std::fs::remove_file(&path);
                        set_stage(&mut st, id, Stage::Failed(why));
                    }
                }
            }
            Todo::Play(t) => {
                aura_core::aelog!("[UPNP] play track {t}");
                engine::player_play(Some(t));
            }
            Todo::Radio(uri) => {
                aura_core::aelog!("[UPNP] play a stream");
                if let Err(e) = engine::player_radio(uri) {
                    let mut st = self.st.lock().unwrap();
                    st.error = Some(e);
                    st.want = Want::Stopped;
                    st.asked = None;
                }
            }
            Todo::Forget { path, track } => {
                if let Some(t) = track {
                    engine::player_remove(t);
                    owned().lock().unwrap().remove(&t);
                }
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn fetched(&self, id: u32, r: Result<Fetched, String>) {
        let mut st = self.st.lock().unwrap();
        let still = st.stages.iter().any(|(i, s)| *i == id && matches!(s, Stage::Fetching));
        match r {
            Ok(Fetched::File(path)) if still => set_stage(&mut st, id, Stage::Ready { path, track: None, duration_s: 0.0 }),
            Ok(Fetched::File(path)) => {
                let _ = std::fs::remove_file(path);
            }
            Ok(Fetched::Stream) if still => set_stage(&mut st, id, Stage::Stream),
            Ok(Fetched::Stream) => {}
            Err(e) => {
                aura_core::aelog!("[UPNP] fetch: {e}");
                if still {
                    set_stage(&mut st, id, Stage::Failed(e));
                }
            }
        }
    }
}

enum Todo {
    Fetch(Entry),
    Queue { id: u32, path: PathBuf },
    Play(u64),
    Radio(String),
    Forget { path: PathBuf, track: Option<u64> },
}

fn stage_of(st: &State, id: u32) -> Option<&Stage> {
    st.stages.iter().find(|(i, _)| *i == id).map(|(_, s)| s)
}

fn set_stage(st: &mut State, id: u32, s: Stage) {
    if let Some(slot) = st.stages.iter_mut().find(|(i, _)| *i == id) {
        slot.1 = s;
    }
}

fn entry_of_track(st: &State, t: u64) -> Option<u32> {
    st.stages.iter().find_map(|(id, s)| match s {
        Stage::Ready { track: Some(x), .. } if *x == t => Some(*id),
        _ => None,
    })
}

fn transport_of(st: &State) -> Transport {
    if st.entries.is_empty() || st.current.is_none() {
        return Transport::NoMedia;
    }
    match st.want {
        Want::Stopped => Transport::Stopped,
        Want::Paused => Transport::Paused,
        Want::Playing => match st.asked {
            Some(a) if a.played && st.seen.state == "playing" => Transport::Playing,
            Some(a) if a.played && st.seen.state == "paused" => Transport::Paused,
            _ => Transport::Transitioning,
        },
    }
}

/// The renderer, once started.
static RENDERER: OnceLock<Arc<Renderer>> = OnceLock::new();

pub fn install(r: Arc<Renderer>) {
    let _ = RENDERER.set(r);
}

pub fn renderer() -> Option<Arc<Renderer>> {
    RENDERER.get().cloned()
}

// ── numbers as UPnP writes them ────────────────────────────────────────────

pub fn db_to_steps(db: f64) -> u32 {
    if db <= -60.0 {
        return 0;
    }
    (100.0 + db / 0.6).round().clamp(0.0, 100.0) as u32
}

pub fn steps_to_db(steps: u32) -> f64 {
    if steps == 0 {
        -120.0
    } else {
        (steps.min(100) as f64 - 100.0) * 0.6
    }
}

/// "H:MM:SS", "H:MM:SS.fff", "MM:SS", or seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    (total.is_finite() && total >= 0.0).then_some(total)
}

pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_read_and_written_as_upnp_does() {
        assert_eq!(parse_time("0:03:10.500"), Some(190.5));
        assert_eq!(parse_time("03:10"), Some(190.0));
        assert_eq!(parse_time("12"), Some(12.0));
        assert_eq!(parse_time("NOT_IMPLEMENTED"), None);
        assert_eq!(format_time(3725.9), "1:02:05");
    }

    #[test]
    fn the_volume_steps_go_both_ways() {
        assert_eq!(steps_to_db(0), -120.0);
        assert_eq!(steps_to_db(100), 0.0);
        for s in 0..=100 {
            assert_eq!(db_to_steps(steps_to_db(s)), s);
        }
        assert_eq!(db_to_steps(-120.0), 0);
    }

    #[test]
    fn didl_says_what_the_track_is() {
        let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<item id="1" parentID="0" restricted="1"><dc:title>Numb</dc:title><dc:creator>Linkin Park</dc:creator>
<upnp:album>Meteora</upnp:album><upnp:class>object.item.audioItem.musicTrack</upnp:class>
<upnp:albumArtURI>http://h/art.jpg</upnp:albumArtURI>
<res protocolInfo="http-get:*:audio/x-flac:DLNA.ORG_OP=01" duration="0:03:07.000">http://h/a.flac</res></item></DIDL-Lite>"#;
        let m = Meta::from_didl(didl);
        assert_eq!(m.title, "Numb");
        assert_eq!(m.artist, "Linkin Park");
        assert_eq!(m.album, "Meteora");
        assert_eq!(m.mime, "audio/x-flac");
        assert_eq!(m.duration_s, Some(187.0));
        assert!(!m.is_broadcast());
        assert_eq!(Meta::from_didl(""), Meta::default());
    }

    fn bare() -> Arc<Renderer> {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        Renderer::new(None, rt.handle().clone())
    }

    #[test]
    fn av_keeps_the_current_track_and_the_next() {
        let r = bare();
        r.set_uri("http://h/1.flac".into(), String::new());
        assert_eq!(r.transport(), Transport::Stopped);
        r.set_next_uri("http://h/2.flac".into(), String::new());
        r.set_next_uri("http://h/3.flac".into(), String::new());
        let st = r.st.lock().unwrap();
        let uris: Vec<_> = st.entries.iter().map(|e| e.uri.as_str()).collect();
        assert_eq!(uris, ["http://h/1.flac", "http://h/3.flac"], "the next one replaced");
        drop(st);
        assert_eq!(r.next_entry().unwrap().uri, "http://h/3.flac");
        r.set_next_uri(String::new(), String::new());
        assert!(r.next_entry().is_none());
    }

    #[test]
    fn a_playlist_goes_round_with_repeat_all() {
        let r = bare();
        r.use_as_playlist();
        let a = r.insert(0, "http://h/a".into(), String::new()).unwrap();
        let b = r.insert(a, "http://h/b".into(), String::new()).unwrap();
        let c = r.insert(0, "http://h/c".into(), String::new()).unwrap();
        let ids: Vec<_> = r.st.lock().unwrap().entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, [c, a, b]);
        assert!(r.insert(999, "x".into(), String::new()).is_err());
        r.st.lock().unwrap().current = Some(b);
        assert_eq!(r.neighbour(1), None);
        r.st.lock().unwrap().repeat = Repeat::All;
        assert_eq!(r.neighbour(1), Some(c));
        assert_eq!(r.neighbour(-1), Some(a));
        r.delete(b);
        assert_eq!(r.st.lock().unwrap().current, None, "nothing after it");
        assert_eq!(r.transport(), Transport::NoMedia);
    }
}
