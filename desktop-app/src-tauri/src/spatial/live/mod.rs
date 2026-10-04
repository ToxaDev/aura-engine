//! The instruments of a live stream (TASK-27, stage 1): what the scenes with
//! instruments (Stage, Lineup, Diamond Dust Stage) get on the radio.
//!
//! A file is analysed once, whole (`job`); a stream never ends and is heard
//! about eight seconds after it arrives (the margin a station is tuned in
//! with) — one segment of the separation network. So the stream is taken
//! apart as it comes, ahead of what is heard (`feed` → `sep` → `feats`), and
//! each song's map grows as it plays (`song`); until the instruments are there
//! for what is heard — the first seconds of a station, after a break, a
//! segment skipped behind a busy card — the scenes get the stand-in from the
//! mix (`rough`, F5), as a file's do before its map.
//!
//! The work is a thread of its own at a low priority, started by a scene
//! asking for the stream's objects and resting ten seconds after the last
//! ask. The network runs on the graphics card only: on the processor it would
//! sit beside the engine's own convolution for as long as the stream plays.
//! Its session takes about 2.4 GB of the card: it is opened only with enough
//! free for it and the convolution's own (`set_conv_vram`), let go when that
//! runs short, and always before the radio builds a chain (`GpuHold`: the
//! sound first).
//!
//! Stage 2: beside it, when the card has room for both, the drum network
//! takes the drums source apart into the kit's pieces (`drums`) — the toms
//! and the cymbals become instruments of their own, kick, snare and hats
//! sharper; its session is the first to go when memory runs short, and the
//! kit is the drums' bands again without it.
//!
//! Stage 2b: the bass's and the voice's notes (`notes`) — the note network
//! over those sources as they come, on the processor and the work's own
//! thread, after each segment; each note on a place of its source, as a
//! file's (`Song::set_notes`). Stage 3: the guitars', the keys' and the
//! others' notes too, on their places the same way until their source has
//! notes enough to be split into its instruments.

mod drums;
mod feats;
mod feed;
mod inst;
mod notes;
mod pct;
mod rough;
mod sep;
mod song;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::analysis::{self, FPS, HOP};
use super::core::{self, SEG};
use super::{map_frames, objects, pack, SLOTS, VALS};
use crate::player::radio::SongStarts;
use feed::{Feed, Shadow, SR};
use song::Song;

/// Nobody asked for this long: the work rests and lets the card go.
const IDLE: Duration = Duration::from_secs(10);
/// The feed starts this far before the place being heard.
const LEAD_IN_S: f64 = 1.5;
/// The frames' values held (for a place found late, a song's start found late).
const FEATS_S: f64 = 40.0;
/// A song: at most this long — a stream with no song starts, or a set that
/// runs on — then a new map, at the quietest place this near the mark.
const SONG_MAX_S: f64 = 600.0;
const SONG_CUT_NEAR_S: f64 = 15.0;
/// Silence this long between two sounds is a new song (no song starts in the
/// stream): its mean power under `SILENT_DB`.
const SILENCE_S: f64 = 2.0;
const SILENT_DB: f32 = -60.0;
/// The songs kept: the one heard, the one analysed ahead, one before.
const SONGS_KEPT: usize = 3;
/// The stand-in's frames this near the end of what is analysed are not given
/// out (their smoothing still waits for what follows, as `objects` finishes
/// them over 1.5 s each way).
const ROUGH_EDGE: usize = (FPS * 1.5) as usize + 1;
/// Free video memory the work keeps watching for while its session is open.
const WATCH: Duration = Duration::from_secs(5);
const VRAM_LOW: u64 = 512 << 20;
/// Free video memory the drum network's session wants beside the
/// separation's (and the convolution's own): its ~1.1 GB and a margin
/// (measured 3.10 beside the separation's session:
/// `the_drum_network_beside_the_separation_on_the_card`).
const DRUMS_NEEDS: u64 = 1_536 << 20;
/// The kit waits for the drum network's whole frames while they are at most
/// this far behind the sources' (frames: ten seconds).
const DRUMS_NEAR: usize = (10.0 * FPS) as usize;

struct Want {
    gen: u64,
    at_s: f64,
    when: Instant,
}

/// Why the stream's instruments are not made, for the menu: no card for the
/// network, or not enough free memory on it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WhyNot {
    Gpu,
    Memory,
}

impl WhyNot {
    pub fn word(self) -> &'static str {
        match self {
            WhyNot::Gpu => "gpu",
            WhyNot::Memory => "memory",
        }
    }
}

struct Shared {
    live: Mutex<Option<Live>>,
    want: Mutex<Option<Want>>,
    wake: Condvar,
}

/// The work and what it holds: started by the first ask for a stream's objects.
fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| {
        std::thread::Builder::new()
            .name("spatial-live".into())
            .spawn(worker)
            .expect("the live spatial worker");
        Shared { live: Mutex::new(None), want: Mutex::new(None), wake: Condvar::new() }
    })
}

/// The network's session on the card, as the radio's chain builds and the
/// status see it — without starting the work.
struct Card {
    hold: Mutex<u32>,
    hold_cv: Condvar,
    open: AtomicBool,
    why: Mutex<Option<(WhyNot, Instant)>>,
    /// The drum network's session is open beside it.
    drums: AtomicBool,
}

fn card() -> &'static Card {
    static C: Card =
        Card { hold: Mutex::new(0), hold_cv: Condvar::new(), open: AtomicBool::new(false), why: Mutex::new(None), drums: AtomicBool::new(false) };
    &C
}

/// The convolution's own need of the card for the stream's rack (bytes):
/// what has to stay free beside the network's session.
static CONV_VRAM: AtomicU64 = AtomicU64::new(0);

pub fn set_conv_vram(bytes: u64) {
    CONV_VRAM.store(bytes, Ordering::Relaxed);
}

/// While one is held, the network's session is closed and not opened: the
/// radio builds its chain with the card to itself (the sound first). Taking
/// one waits (a little) for the session to close.
pub struct GpuHold(());

impl GpuHold {
    pub fn take() -> GpuHold {
        let c = card();
        *c.hold.lock().unwrap() += 1;
        let t0 = Instant::now();
        let mut g = c.hold.lock().unwrap();
        while c.open.load(Ordering::Acquire) && t0.elapsed() < Duration::from_millis(1500) {
            g = c.hold_cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
        }
        drop(g);
        if c.open.load(Ordering::Acquire) {
            crate::aelog!("[SPATIAL] live: the separation's session did not close in time for the radio's chain");
        }
        GpuHold(())
    }
}

impl Drop for GpuHold {
    fn drop(&mut self) {
        let mut g = card().hold.lock().unwrap();
        *g = g.saturating_sub(1);
    }
}

fn held() -> bool {
    *card().hold.lock().unwrap() > 0
}

/// The radio session `gen` of a heard path (`radio:<gen>:<url>`).
pub fn stream_of(path: &str) -> Option<u64> {
    path.strip_prefix("radio:")?.split(':').next()?.parse().ok()
}

/// The heard path of radio session `gen` (`heard`, for the scenes).
pub fn path_of(gen: u64, url: &str) -> String {
    format!("radio:{gen}:{url}")
}

/// A stream's track id for the scenes: one per session (a new station is a
/// new track to them), exact as an f64.
pub fn track_id(gen: u64) -> u64 {
    (1u64 << 50) + gen
}

fn want(gen: u64, at_s: f64) {
    let s = shared();
    *s.want.lock().unwrap() = Some(Want { gen, at_s, when: Instant::now() });
    s.wake.notify_all();
}

/// A song's map's id on the wire: the session, the song, its version.
fn map_id(gen: u64, n: u32, version: u32) -> u32 {
    0x8000_0000 | ((gen as u32 & 0x7f) << 24) | ((n & 0xff) << 16) | (version & 0xffff)
}

/// What the work holds for one radio session.
struct Live {
    gen: u64,
    source: Arc<dyn Shadow + Send + Sync>,
    /// Where the stream says songs begin (its frames).
    told: Arc<SongStarts>,
    feed: Feed,
    stft: analysis::Stft,
    /// The next session frame of the stand-in.
    rough_next: usize,
    songs: VecDeque<Song>,
    song_n: u32,
    /// Song starts the stream told of, as session frames, not reached yet.
    starts: VecDeque<usize>,
    /// The last song start taken from the stream (its frame).
    starts_seen: i64,
    /// Frames of silence running at the stand-in's end.
    silent: usize,
    sep: sep::Sep,
    feats: feats::Feats,
    /// Session frames below: their values are whole.
    feat_next: usize,
    /// Runs of the songs' maps (the frames written again since: the wire's
    /// head[14] moves with it).
    builds: u64,
    last_ms: f64,
    skipped_said: u64,
    /// The second heard at the last ask (the notes' window of `map_json`).
    heard_s: f64,
    /// The drum network over the drums source, and what is measured of its
    /// pieces frame by frame (session frames below `dfeat_next`: whole).
    drums: drums::DrumRun,
    dfeats: drums::DrumFeats,
    dfeat_next: usize,
    /// Its session is open (the kit's final values wait for its frames).
    drum_on: bool,
    drum_ms: f64,
    drum_skipped_said: u64,
    /// The sources' notes (`notes::SOURCES`): their work (out while a round
    /// runs), the notes settled with the place each went to, the stream's own
    /// for now; the note network is there.
    notes: Option<notes::Notes>,
    placed: [Vec<notes::Placed>; 5],
    own: [Vec<notes::LNote>; 5],
    /// The sound of a note instruments' source's own (as `own`), and the last
    /// id given to a note.
    own_sound: [Vec<notes::Sound>; 5],
    note_id: u64,
    /// The note instruments' sources are split into their instruments
    /// (`inst::SPLIT`; else their notes stay on their places, as the bass's).
    split: bool,
    /// The session sample the work began from: every place heard from here on
    /// is its own (what it made for it lies in the songs' maps), however far
    /// ahead it has gone and let go of its feed's history behind it.
    began: usize,
    notes_on: bool,
    notes_ms: f64,
}

impl Live {
    fn new(gen: u64, source: Arc<dyn Shadow + Send + Sync>, told: Arc<SongStarts>, at_s: f64) -> Live {
        let rate = source.rate();
        let from = ((at_s - LEAD_IN_S) * rate as f64) as i64;
        let from = from.max(source.first()).max(0);
        let feed = Feed::new(rate, from, source.edits());
        let start = feed.start();
        let f0 = (start + 1024).div_ceil(HOP);
        crate::aelog!("[SPATIAL] live: the stream's instruments from {:.1} s of the session ({} Hz)", start as f64 / SR as f64, rate);
        let first = Song::new(0, f0, format!("radio:{gen}#0"));
        Live {
            gen,
            source,
            told,
            feed,
            stft: analysis::Stft::new(),
            rough_next: f0,
            songs: VecDeque::from([first]),
            song_n: 0,
            starts: VecDeque::new(),
            starts_seen: -1,
            silent: 0,
            sep: sep::Sep::new(start),
            feats: feats::Feats::new(FEATS_S),
            feat_next: f0,
            builds: 0,
            last_ms: 400.0,
            skipped_said: 0,
            heard_s: at_s,
            drums: drums::DrumRun::new(start),
            dfeats: drums::DrumFeats::new(FEATS_S),
            dfeat_next: f0,
            drum_on: false,
            drum_ms: 200.0,
            drum_skipped_said: 0,
            notes: Some(notes::Notes::new(start)),
            placed: Default::default(),
            own: Default::default(),
            own_sound: Default::default(),
            note_id: 0,
            split: inst::SPLIT,
            began: start,
            notes_on: false,
            notes_ms: 0.0,
        }
    }

    fn song_mut(&mut self) -> &mut Song {
        self.songs.back_mut().expect("a song")
    }

    /// A new song from session frame `at` (its frames past `at` of the one
    /// before become its own) — not within a second of the last start.
    fn new_song(&mut self, at: usize, why: &str) {
        if at > self.songs.back().expect("a song").f0 + (FPS as usize) {
            self.begin_song(at, why);
        }
    }

    fn begin_song(&mut self, at: usize, why: &str) {
        if at <= self.songs.back().expect("a song").f0 {
            // nothing of the song before is left
            self.songs.pop_back();
            self.song_n += 1;
            self.songs.push_back(Song::new(self.song_n, at, format!("radio:{}#{}", self.gen, self.song_n)));
            return;
        }
        self.song_n += 1;
        let n = self.song_n;
        let path = format!("radio:{}#{n}", self.gen);
        let cur = self.song_mut();
        let rough = cur.rough.split_off(at - cur.f0, path);
        cur.end_at(at);
        let mut s = Song::new(n, at, String::new());
        s.rough = rough;
        crate::aelog!("[SPATIAL] live: song {n} from {:.1} s ({why})", at as f64 / FPS);
        self.songs.push_back(s);
        while self.songs.len() > SONGS_KEPT {
            self.songs.pop_front();
        }
    }

    /// The stream changed under the analysis from session sample `s` on (or
    /// went on past where the feed could follow): all of it past there is
    /// made again — a new song, a new grid of segments.
    fn restart_at(&mut self, s: usize) {
        let j = (s.saturating_sub(1024) / HOP).max(self.songs.back().expect("a song").f0);
        // what was made of the stream past there no longer holds
        let cur = self.song_mut();
        let k = j - cur.f0;
        cur.rough.truncate(k);
        self.begin_song(j, "the stream changed under it");
        self.rough_next = j;
        self.began = self.began.min(s);
        self.sep = sep::Sep::new(s);
        self.feats.truncate(j);
        self.feat_next = j;
        self.drums = drums::DrumRun::new(s);
        self.dfeats.truncate(j);
        self.dfeat_next = j;
        self.notes = Some(notes::Notes::new(s));
        self.own = Default::default();
        self.own_sound = Default::default();
        self.starts.retain(|&x| x > j);
        self.silent = 0;
    }

    /// The songs the stream says begin, as session frames.
    fn take_starts(&mut self) {
        while let Some(f) = self.told.first_from(self.starts_seen + 1) {
            self.starts_seen = f;
            let at = (self.feed.session_of(f as f64) / HOP as f64).round() as usize;
            if at >= self.rough_next {
                self.starts.push_back(at);
            } else if at > self.songs.back().unwrap().f0 && at >= self.feats.start() {
                // told after its frames came: they become the new song's
                self.new_song(at, "the stream's song start, told late");
            }
        }
    }

    /// The feed and the stand-in brought up to what the stream has.
    fn pull(&mut self) {
        let p = self.feed.pull(&*self.source);
        if let Some(s) = p.restart {
            self.restart_at(s);
        }
        self.take_starts();
        let end = self.feed.end();
        if end < 1024 + HOP {
            return;
        }
        let j1 = (end - 1024) / HOP;
        while self.rough_next < j1 {
            let a = self.rough_next;
            let b = j1.min(a + 512);
            let from = (a.saturating_sub(40) * HOP) as isize - 1024;
            let n = (b - a.saturating_sub(40)) * HOP + 2048;
            let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
            // f32: the analysis' boundary (§4.1)
            self.feed.get_f32(0, from, &mut l);
            self.feed.get_f32(1, from, &mut r);
            let (ml, mr) = self.feed.slice(0, usize::MAX);
            let rows = rough::rows_of(&self.stft, &l, &r, from, a..b, (ml, mr, self.feed.start()));
            let mut i = a;
            for j in a..b {
                let start = self.starts.front().is_some_and(|&s| s <= j);
                let lv = rows.lv[j - a];
                let quiet_end = lv >= SILENT_DB && self.silent as f64 >= SILENCE_S * FPS;
                if lv < SILENT_DB {
                    self.silent += 1;
                } else {
                    self.silent = 0;
                }
                if start || quiet_end {
                    self.song_mut().rough.push(rows.rows(i - a, j - a));
                    i = j;
                    if start {
                        self.starts.pop_front();
                        self.new_song(j, "the stream's song start");
                    } else {
                        self.new_song(j, "after a silence");
                    }
                }
            }
            self.song_mut().rough.push(rows.rows(i - a, b - a));
            self.rough_next = b;
            self.long_song();
        }
        // the history kept: a segment and a stride back — an older segment would be heard before it
        // could be ready (`Sep::pick` skips it) — and the stand-in's warm-up
        let keep = self.feed.end().saturating_sub(SEG + super::STRIDE + 2 * SR);
        self.feed.trim(keep);
    }

    /// A song that runs past `SONG_MAX_S` ends at its quietest place near
    /// the mark (half a second's mean power, ±15 s), found once the stand-in
    /// is 15 s past it.
    fn long_song(&mut self) {
        let s = self.songs.back().unwrap();
        let mark = (SONG_MAX_S * FPS) as usize;
        let near = (SONG_CUT_NEAR_S * FPS) as usize;
        if s.rough.frames() < mark + near {
            return;
        }
        let lv = &s.rough.lv;
        let w = (FPS / 2.0) as usize;
        let mut best = (f32::INFINITY, mark);
        for j in mark - near..mark + near {
            let a = j.saturating_sub(w / 2);
            let b = (j + w / 2).min(lv.len());
            let m = lv[a..b].iter().map(|d| 10f32.powf(d / 10.0)).sum::<f32>() / (b - a).max(1) as f32;
            if m < best.0 {
                best = (m, j);
            }
        }
        let at = s.f0 + best.1;
        self.new_song(at, "ten minutes");
    }

    /// Segment `k`'s mix (f32: the network's boundary, §4.1).
    fn segment(&self, k: usize) -> (Vec<f32>, Vec<f32>) {
        let a = self.sep.start(k) as isize;
        let (mut l, mut r) = (vec![0f32; SEG], vec![0f32; SEG]);
        self.feed.get_f32(0, a, &mut l);
        self.feed.get_f32(1, a, &mut r);
        (l, r)
    }

    /// The drums the last segment brought, to the drum network: its last
    /// quarter too, as that segment alone made it (read once — the chunks'
    /// own lag would put them behind the place heard otherwise).
    fn feed_drums(&mut self) {
        let run0 = self.sep.run_start().max(self.sep.held_from());
        if run0 > self.drums.end() {
            // segments skipped (or the drum network off meanwhile): no drums between; the chunks
            // start afresh where they come again
            self.drums.skip_to(run0);
            self.drums.restart(run0);
        }
        let (a, b) = (self.drums.end(), self.sep.prov_end());
        if b > a {
            let (mut l, mut r) = (vec![0f32; b - a], vec![0f32; b - a]);
            self.sep.read(super::stems::DRUMS * 2, a as isize, &mut l);
            self.sep.read(super::stems::DRUMS * 2 + 1, a as isize, &mut r);
            self.drums.push(&l, &r);
        }
    }

    /// The bass and the voice the last segment brought, to their notes: what
    /// is whole now once, its last quarter as that segment alone made it.
    fn feed_notes(&mut self) {
        let Some(nt) = self.notes.as_mut() else { return };
        let run0 = self.sep.run_start().max(self.sep.held_from());
        if run0 > nt.end() {
            // segments skipped (or the notes off meanwhile): they start afresh where the sources come again
            *nt = notes::Notes::new(run0);
            self.own = Default::default();
            self.own_sound = Default::default();
        }
        nt.push(&self.sep, self.sep.final_end(), self.sep.prov_end());
    }

    /// A round of the notes, when the sources came on since the last: their
    /// work taken out of the lock, with the song's mix's loud level.
    fn notes_take(&mut self) -> Option<notes::Job> {
        if !self.notes.as_ref()?.fresh() {
            return None;
        }
        let mix_ref = self.songs.back().map_or(-20.0, |s| s.mix_ref());
        Some(notes::Job { notes: self.notes.take()?, mix_ref })
    }

    /// The round's work back, and what it brought to the songs. A note of
    /// the stream's own that has started where it is heard and is gone from
    /// what came now stays, ending there — its flash dies away, not cut off.
    /// A note instruments' source's notes keep what they are like (an id
    /// each, the stream's).
    fn notes_put(&mut self, job: notes::Job, out: Result<[notes::Round; 5], String>) -> Result<(), String> {
        self.notes = Some(job.notes);
        let rounds = out?;
        let heard = self.heard_s;
        for (p, r) in rounds.into_iter().enumerate() {
            let notes::Round { settled, own, like, sound } = r;
            let was = std::mem::take(&mut self.own_sound[p]);
            for (k, o) in std::mem::take(&mut self.own[p]).into_iter().enumerate() {
                let again = own.iter().chain(&settled).any(|n| n.key.round() == o.key.round() && (n.on - o.on).abs() <= 0.05);
                if o.on <= heard + 0.2 && !again {
                    let n = notes::LNote { off: (o.on + 0.12).max(o.off.min(heard)), ..o };
                    let like = was.get(k).map(|s| {
                        self.note_id += 1;
                        Box::new(notes::Like { id: self.note_id, row: None, lvl: n.lvl as f64, parent: None, sound: s.cut(n.off) })
                    });
                    self.placed[p].push(notes::Placed { n, to: None, like });
                }
            }
            let base = self.note_id + 1;
            for (k, n) in settled.into_iter().enumerate() {
                let like = like.get(k).map(|&(row, lvl, parent)| {
                    Box::new(notes::Like { id: base + k as u64, row: Some(row), lvl, parent: parent.map(|q| base + q as u64), sound: sound[k].clone() })
                });
                self.placed[p].push(notes::Placed { n, to: None, like });
            }
            self.note_id += like.len() as u64;
            self.own_sound[p] = sound.get(like.len()..).map(|v| v.to_vec()).unwrap_or_default();
            self.own[p] = own;
        }
        for s in self.songs.iter_mut() {
            s.set_notes(&mut self.placed, &self.own, &self.own_sound, &self.feats, heard, self.split);
        }
        // the places' flashes changed for frames given out: they are asked for again
        self.builds += 1;
        // the settled notes of the songs kept; one with no place yet waits half a minute — a note
        // instruments' source's stay with their song (it is split again over them)
        let first = self.songs.front().map_or(0.0, |s| s.f0 as f64 / FPS);
        for (p, v) in self.placed.iter_mut().enumerate() {
            v.retain(|q| q.n.on >= first && (p >= notes::INST || q.to.is_some() || q.n.on >= heard - 30.0));
        }
        Ok(())
    }

    /// After a segment or the drum network's chunks: the frames' values
    /// whole and ahead, then the songs' maps.
    fn after_segment(&mut self) {
        let edge = |s: usize| s.saturating_sub(1024) / HOP;
        let fin = edge(self.sep.final_end()).min(self.rough_next);
        let prov = edge(self.sep.prov_end()).min(self.rough_next).max(fin);
        // frames of segments skipped have no sources: their values are not made
        self.feat_next = self.feat_next.max((self.sep.run_start() + 1024).div_ceil(HOP));
        let (ml, mr) = self.feed.slice(0, usize::MAX);
        let mix = (ml, mr, self.feed.start());
        if fin > self.feat_next {
            self.feats.make(&self.sep, mix, self.feat_next, fin, true);
            self.feat_next = fin;
        }
        if prov > fin {
            self.feats.make(&self.sep, mix, fin, prov, false);
        }
        // the drum network's frames: whole ones once, the last chunk's own each time
        let dfin = edge(self.drums.fin()).min(self.rough_next);
        let dprov = edge(self.drums.prov()).min(self.rough_next).max(dfin);
        self.dfeat_next = self.dfeat_next.max((self.drums.run_start() + 1024).div_ceil(HOP));
        if dfin > self.dfeat_next {
            self.dfeats.make(&self.drums, self.dfeat_next, dfin);
            self.dfeat_next = dfin;
        }
        if dprov > self.dfeat_next {
            self.dfeats.make(&self.drums, self.dfeat_next, dprov);
        }
        // the kit waits for the drum network's whole frames while it runs and keeps up
        let on = self.drum_on && dfin + DRUMS_NEAR >= fin;
        let dr = song::Drums { feats: &self.dfeats, fin: self.dfeat_next, on };
        for s in self.songs.iter_mut() {
            s.run(&self.feats, fin, prov, dr);
        }
        self.builds += 1;
        let keep = self.feed.end().saturating_sub(SEG * 2);
        self.sep.trim(keep.min(self.feat_next.saturating_sub(40) * HOP));
        self.drums.trim(keep.min(self.dfeat_next.saturating_sub(drums::WARM + 4) * HOP));
    }
}

/// The work: one thread for the whole app, at work while a scene asks.
fn worker() {
    crate::player::analytics::track::set_below_normal_priority();
    let s = shared();
    // the network's own threads (its inverse transforms): two, low
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .thread_name(|i| format!("spatial-live-{i}"))
        .start_handler(|_| crate::player::analytics::track::set_below_normal_priority())
        .build()
        .ok();
    let mut net: Option<core::Core> = None;
    // the drum network's session: beside the separation's, never alone
    let mut kit: Option<super::kit::Kit> = None;
    // the note network: on the processor, on this thread
    let mut bp: Option<super::notes::bp::Model> = None;
    let mut next_try = Instant::now();
    let mut kit_next_try = Instant::now();
    let mut bp_next_try = Instant::now();
    let mut watched = Instant::now();
    let close = |net: &mut Option<core::Core>, kit: &mut Option<super::kit::Kit>, why: &str| {
        if kit.take().is_some() {
            crate::aelog!("[SPATIAL] live: the drum network's session closed ({why})");
        }
        if net.take().is_some() {
            crate::aelog!("[SPATIAL] live: the separation's session closed ({why})");
        }
        card().drums.store(false, Ordering::Release);
        card().open.store(false, Ordering::Release);
        card().hold_cv.notify_all();
    };
    loop {
        let w = {
            let g = s.want.lock().unwrap();
            let g = s.wake.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
            g.as_ref().filter(|w| w.when.elapsed() < IDLE).map(|w| (w.gen, w.at_s))
        };
        if held() {
            close(&mut net, &mut kit, "the radio builds a chain");
            next_try = Instant::now() + Duration::from_secs(2);
            kit_next_try = next_try;
        }
        let Some((gen, at_s)) = w else {
            if s.live.lock().unwrap().take().is_some() {
                crate::aelog!("[SPATIAL] live: nobody asked for the stream's instruments for 10 s — resting");
            }
            close(&mut net, &mut kit, "nobody asks");
            bp = None;
            continue;
        };
        let session = crate::player::controller::get().radio_shared(gen);
        let Some((told, source)) = session.and_then(|sh| sh.live.get().cloned().map(|l| (sh.songs.clone(), l))) else {
            s.live.lock().unwrap().take();
            continue;
        };
        {
            let mut g = s.live.lock().unwrap();
            match fit(g.as_ref(), gen, at_s, &*source) {
                Fit::Wait => continue,
                Fit::Start => {
                    if g.as_ref().is_some_and(|l| l.gen == gen) {
                        crate::aelog!("[SPATIAL] live: the place heard ({at_s:.1} s) is before what the feed holds — afresh from there");
                    }
                    *g = Some(Live::new(gen, source, told, at_s));
                }
                Fit::Keep => {}
            }
            g.as_mut().unwrap().pull();
        }
        let Some(dir) = pack::installed() else { continue };
        // the network's session: opened when the card has room, watched while open
        if net.is_none() && !held() && Instant::now() >= next_try {
            match open(&dir) {
                Ok(c) => {
                    net = Some(c);
                    card().open.store(true, Ordering::Release);
                    *card().why.lock().unwrap() = None;
                }
                Err((why, wait)) => {
                    *card().why.lock().unwrap() = Some((why, Instant::now()));
                    next_try = Instant::now() + wait;
                }
            }
        }
        if net.is_some() && watched.elapsed() >= WATCH {
            watched = Instant::now();
            let reserve = CONV_VRAM.load(Ordering::Relaxed).max(VRAM_LOW);
            if vram_free().is_some_and(|f| f < reserve) {
                if kit.take().is_some() {
                    // the drum network goes first: the kit from the drums' bands again
                    crate::aelog!("[SPATIAL] live: the drum network's session closed (the card runs short of memory)");
                    card().drums.store(false, Ordering::Release);
                    kit_next_try = Instant::now() + Duration::from_secs(60);
                } else {
                    close(&mut net, &mut kit, "the card runs short of memory");
                    *card().why.lock().unwrap() = Some((WhyNot::Memory, Instant::now()));
                    next_try = Instant::now() + Duration::from_secs(30);
                }
            }
        }
        let Some(c) = net.as_mut() else { continue };
        // the drum network's session: beside the separation's, when the card has room for both
        if kit.is_none() && !held() && Instant::now() >= kit_next_try {
            match open_kit(&dir) {
                Ok(k) => {
                    kit = Some(k);
                    card().drums.store(true, Ordering::Release);
                }
                Err(wait) => kit_next_try = Instant::now() + wait,
            }
        }
        // the note network beside it (the bass's and the voice's notes)
        if bp.is_none() && Instant::now() >= bp_next_try {
            match open_notes(&dir) {
                Ok(m) => bp = Some(m),
                Err(wait) => bp_next_try = Instant::now() + wait,
            }
        }
        // a segment, if one is due
        let job = {
            let mut g = s.live.lock().unwrap();
            let Some(l) = g.as_mut().filter(|l| l.gen == gen) else { continue };
            l.drum_on = kit.is_some();
            l.notes_on = bp.is_some();
            let heard = (at_s * SR as f64) as usize;
            let budget = ((l.last_ms * 1.5 + 300.0) / 1000.0 * SR as f64) as usize;
            let k = l.sep.pick(l.feed.end(), heard, budget);
            if l.sep.skipped > l.skipped_said {
                crate::aelog!("[SPATIAL] live: behind the stream — {} segment(s) skipped so far (their frames keep the mix)", l.sep.skipped);
                l.skipped_said = l.sep.skipped;
            }
            k.map(|k| (k, l.segment(k)))
        };
        let mut made = false;
        let mut seg = false;
        if let Some((k, (sl, sr))) = job {
            let t0 = Instant::now();
            let out = match &pool {
                Some(p) => p.install(|| c.separate(&sl, &sr)),
                None => c.separate(&sl, &sr),
            };
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            match out {
                Ok(out) => {
                    let mut g = s.live.lock().unwrap();
                    if let Some(l) = g.as_mut().filter(|l| l.gen == gen) {
                        l.last_ms = ms;
                        l.sep.add(k, &out);
                        if l.drum_on {
                            l.feed_drums();
                        }
                        if l.notes_on {
                            l.feed_notes();
                        }
                        made = true;
                        seg = true;
                    }
                }
                Err(e) => {
                    crate::aelog!("[SPATIAL] live: the separation failed ({e})");
                    close(&mut net, &mut kit, "it failed");
                    *card().why.lock().unwrap() = Some((WhyNot::Gpu, Instant::now()));
                    next_try = Instant::now() + Duration::from_secs(60);
                    continue;
                }
            }
        }
        // the drum network's chunks that are due, ahead of what is heard
        loop {
            let Some(dk) = kit.as_mut() else { break };
            let job = {
                let mut g = s.live.lock().unwrap();
                let Some(l) = g.as_mut().filter(|l| l.gen == gen) else { break };
                let heard = (at_s * SR as f64) as usize;
                let budget = ((l.drum_ms * 1.5 + 300.0) / 1000.0 * SR as f64) as usize;
                let j = l.drums.pick(heard, budget);
                if l.drums.skipped > l.drum_skipped_said {
                    crate::aelog!("[SPATIAL] live: the drum network behind the stream — it started afresh ahead {} time(s) so far", l.drums.skipped);
                    l.drum_skipped_said = l.drums.skipped;
                }
                j
            };
            let Some((i, cl, cr)) = job else { break };
            let t0 = Instant::now();
            let out = match &pool {
                Some(p) => p.install(|| dk.chunk(&cl, &cr)),
                None => dk.chunk(&cl, &cr),
            };
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut g = s.live.lock().unwrap();
            let Some(l) = g.as_mut().filter(|l| l.gen == gen) else { break };
            match out.and_then(|y| l.drums.add(i, &y)) {
                Ok(()) => {
                    l.drum_ms = ms;
                    made = true;
                }
                Err(e) => {
                    crate::aelog!("[SPATIAL] live: the drum network failed ({e}) — the kit from the drums' bands");
                    kit = None;
                    card().drums.store(false, Ordering::Release);
                    kit_next_try = Instant::now() + Duration::from_secs(60);
                    break;
                }
            }
        }
        if made {
            let mut g = s.live.lock().unwrap();
            if let Some(l) = g.as_mut().filter(|l| l.gen == gen) {
                l.drum_on = kit.is_some();
                match &pool {
                    Some(p) => p.install(|| l.after_segment()),
                    None => l.after_segment(),
                }
            }
        }
        // the notes of what the segment brought: the network and the measuring out of the lock
        if let (true, Some(m)) = (seg, bp.as_mut()) {
            let job = s.live.lock().unwrap().as_mut().filter(|l| l.gen == gen).and_then(|l| l.notes_take());
            if let Some(mut job) = job {
                let t0 = Instant::now();
                let out = job.run(m);
                let failed = out.as_ref().err().cloned();
                if let Some(l) = s.live.lock().unwrap().as_mut().filter(|l| l.gen == gen) {
                    let _ = l.notes_put(job, out);
                    l.notes_ms = t0.elapsed().as_secs_f64() * 1000.0;
                }
                if let Some(e) = failed {
                    crate::aelog!("[SPATIAL] live: the note network failed ({e}) — no notes for a while");
                    bp = None;
                    bp_next_try = Instant::now() + Duration::from_secs(300);
                }
            }
        }
    }
}

/// The note network on the processor, one thread (this one's: low, beside
/// the sound): else how long until it is tried again (no notes meanwhile).
fn open_notes(dir: &std::path::Path) -> Result<super::notes::bp::Model, Duration> {
    let model = std::env::var_os("AURA_NOTES_MODEL").map(std::path::PathBuf::from).unwrap_or_else(|| dir.join(pack::PITCH));
    if !model.is_file() {
        crate::aelog!("[SPATIAL] live: no note network at {} — the bass and the voice without notes", model.display());
        return Err(Duration::from_secs(600));
    }
    match super::notes::bp::Model::open_threads(dir, &model, 1) {
        Ok(m) => {
            crate::aelog!("[SPATIAL] live: the note network opened (on the processor, one thread)");
            Ok(m)
        }
        Err(e) => {
            crate::aelog!("[SPATIAL] live: the note network cannot run here: {e} — the bass and the voice without notes");
            Err(Duration::from_secs(300))
        }
    }
}

/// The drum network's session on the card beside the separation's, when it
/// has room for it and the convolution's own: else how long until it is
/// tried again (the kit comes from the drums' bands meanwhile).
fn open_kit(dir: &std::path::Path) -> Result<super::kit::Kit, Duration> {
    let model = std::env::var_os("AURA_KIT_MODEL").map(std::path::PathBuf::from).unwrap_or_else(|| dir.join(pack::DRUMS));
    if !model.is_file() {
        crate::aelog!("[SPATIAL] live: no drum network at {} — the kit from the drums' bands", model.display());
        return Err(Duration::from_secs(600));
    }
    let need = DRUMS_NEEDS + CONV_VRAM.load(Ordering::Relaxed);
    let free = vram_free().unwrap_or(0);
    if free < need {
        crate::aelog!(
            "[SPATIAL] live: {} MB of video memory free, the drum network needs {} MB beside the separation and the convolution — the kit from the drums' bands",
            free >> 20,
            need >> 20
        );
        return Err(Duration::from_secs(30));
    }
    match super::kit::Kit::open_gpu(dir, &model, need) {
        Ok(k) => {
            crate::aelog!("[SPATIAL] live: the drum network opened on the GPU ({} MB free, {} MB asked for)", free >> 20, need >> 20);
            Ok(k)
        }
        Err(e) => {
            crate::aelog!("[SPATIAL] live: the drum network cannot run here: {e} — the kit from the drums' bands");
            Err(Duration::from_secs(300))
        }
    }
}

/// What the work does with an ask for session `gen` heard at `at_s`.
#[derive(Debug, PartialEq)]
enum Fit {
    Wait,
    Start,
    Keep,
}

/// A place heard outside the session's stream is not its own — the station
/// before still playing out its end under the new one's name while the new
/// chain is built: wait for one inside. One the feed can no longer give
/// (before where it starts: it was started at such a place, or a pause let
/// the stream run on) starts the work afresh there.
fn fit(l: Option<&Live>, gen: u64, at_s: f64, source: &dyn Shadow) -> Fit {
    let rate = source.rate() as f64;
    let frame = at_s * rate;
    if !(0.0..=source.end() as f64 + rate).contains(&frame) {
        return Fit::Wait;
    }
    match l {
        // a place heard the work began before is its own, though the work has run on ahead and let go of
        // its feed's history there (a station's burst; anew there, its instruments went every 1.4 s: TASK-34)
        Some(l) if l.gen == gen && (at_s * SR as f64) as usize >= l.began => Fit::Keep,
        // a place the stream no longer holds: no work can start there — the one under way goes on and the
        // place heard comes to it (afresh at every ask, the scene's instruments would never come: TASK-34)
        Some(l) if l.gen == gen && frame < source.first() as f64 => Fit::Keep,
        _ => Fit::Start,
    }
}

/// Free video memory on the card the network would run on; None: no budget
/// to read (no such card, or not on Windows) — then the stream goes without.
fn vram_free() -> Option<u64> {
    let (v, d) = crate::audio::gpu::dxgi_memory::largest_adapter()?;
    crate::audio::gpu::dxgi_memory::query(v, d).ok().map(|m| m.free())
}

/// What the network's session needs free: its own and the convolution's.
fn vram_need() -> u64 {
    core::GPU_NEEDS.max(core::SESSION_VRAM + CONV_VRAM.load(Ordering::Relaxed))
}

/// The network's session on the card, when it has room: else why not, and
/// how long until it is tried again.
fn open(dir: &std::path::Path) -> Result<core::Core, (WhyNot, Duration)> {
    let need = vram_need();
    let Some(free) = vram_free() else {
        crate::aelog!("[SPATIAL] live: no graphics card's memory to read — the stream's instruments stay off");
        return Err((WhyNot::Gpu, Duration::from_secs(300)));
    };
    if free < need {
        crate::aelog!("[SPATIAL] live: {} MB of video memory free, the separation needs {} MB beside the convolution — off", free >> 20, need >> 20);
        return Err((WhyNot::Memory, Duration::from_secs(15)));
    }
    match core::Core::open_gpu(dir, need) {
        Ok(c) => {
            crate::aelog!("[SPATIAL] live: the separation opened on the GPU ({} MB free, {} MB asked for)", free >> 20, need >> 20);
            Ok(c)
        }
        Err(e) => {
            crate::aelog!("[SPATIAL] live: the separation cannot run here: {e}");
            Err((WhyNot::Gpu, Duration::from_secs(300)))
        }
    }
}

/// Why the stream's instruments are off now (the pack there): for the menu.
/// Without a try yet, the card's budget says.
pub fn why_not() -> Option<WhyNot> {
    pack::installed()?;
    if card().open.load(Ordering::Acquire) {
        return None;
    }
    if let Some((w, at)) = *card().why.lock().unwrap() {
        if at.elapsed() < Duration::from_secs(600) {
            return Some(w);
        }
    }
    static GUESS: Mutex<Option<(Option<WhyNot>, Instant)>> = Mutex::new(None);
    let mut g = GUESS.lock().unwrap();
    if let Some((w, at)) = *g {
        if at.elapsed() < Duration::from_secs(5) {
            return w;
        }
    }
    let w = match vram_free() {
        None => Some(WhyNot::Gpu),
        Some(f) if f < vram_need() => Some(WhyNot::Memory),
        Some(_) => None,
    };
    *g = Some((w, Instant::now()));
    w
}

/// The stream's objects around what is heard (`span` on a stream: the same
/// answer, see there). The window stays within the song heard and stops where
/// its values are known; head[14] moves when frames given out before were
/// made again (a segment's last quarter made whole, the instruments come for
/// frames of the stand-in, another song heard).
pub fn span(gen: u64, track: u64, at_s: f64, state: u8, from_ms: f64, to_ms: f64) -> Vec<u8> {
    want(gen, at_s);
    let s = shared();
    let pack = pack::installed().is_some();
    let on = pack && !held() && why_not().is_none();
    let open = card().open.load(Ordering::Acquire);
    let mut g = s.live.lock().unwrap();
    let l = g.as_mut().filter(|l| l.gen == gen);
    answer(l, Ask { track, at_s, state, from_ms, to_ms, pack, on, open })
}

/// What `span` is asked, and what the work is like now.
struct Ask {
    track: u64,
    at_s: f64,
    state: u8,
    from_ms: f64,
    to_ms: f64,
    /// the pack is there; the separation runs (or is about to); its session is open
    pack: bool,
    on: bool,
    open: bool,
}

fn answer(l: Option<&mut Live>, a: Ask) -> Vec<u8> {
    let mut head = [a.at_s, FPS, 0.0, 0.0, 0.0, if a.pack { 0.0 } else { -1.0 }, -1.0, 0.0, a.state as f64, a.track as f64, SLOTS as f64, VALS as f64, 0.0, 0.0, 0.0];
    if a.on {
        head[13] = 1.0;
    }
    let put = |head: &[f64; 15], out: &mut Vec<u8>| head.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
    let mut out = Vec::new();
    let Some(l) = l else {
        put(&head, &mut out);
        return out;
    };
    head[6] = if a.open { 1.0 } else { -1.0 };
    l.heard_s = a.at_s;
    let h = (a.at_s * FPS).floor().max(0.0) as usize;
    let (gen, builds) = (l.gen, l.builds);
    let Some(song) = l.songs.iter_mut().rev().find(|s| s.f0 <= h) else {
        put(&head, &mut out);
        return out;
    };
    let k = h - song.f0;
    let origin = if song.has(k) {
        2
    } else if k < song.rough.frames() {
        1
    } else {
        0
    };
    head[4] = (song.f0 + song.rough.frames()) as f64;
    head[7] = origin as f64;
    head[14] = ((builds * 3 + origin as u64) * 1000 + (song.n % 1000) as u64) as f64;
    if origin == 2 {
        head[12] = map_id(gen, song.n, song.version) as f64;
        head[13] = 0.0;
        song.shown = true;
    }
    if origin == 0 {
        put(&head, &mut out);
        return out;
    }
    let f0 = ((a.at_s + a.from_ms.min(a.to_ms) / 1000.0) * FPS).floor().max(0.0) as usize;
    let mut f1 = ((a.at_s + a.from_ms.max(a.to_ms) / 1000.0) * FPS).ceil().max(0.0) as usize;
    let known = if origin == 2 { song.ready() } else { song.rough.frames().saturating_sub(ROUGH_EDGE) };
    f1 = f1.min(song.f0 + known.max(k + 1));
    if let Some(e) = song.end {
        f1 = f1.min(e);
    }
    if f1 <= f0 {
        put(&head, &mut out);
        return out;
    }
    // frames before the song's start: nothing (another song's slots)
    let mut vals = vec![0f32; (f1 - f0) * SLOTS * VALS];
    let from = f0.max(song.f0);
    if f1 > from {
        let v = if origin == 2 {
            map_frames(&song.map, from - song.f0, f1 - song.f0)
        } else {
            objects(&song.rough.d, from - song.f0, f1 - song.f0)
        };
        let o = (from - f0) * SLOTS * VALS;
        vals[o..o + v.len()].copy_from_slice(&v);
    }
    head[2] = f0 as f64;
    head[3] = (f1 - f0) as f64;
    put(&head, &mut out);
    out.reserve(vals.len() * 2);
    for (i, v) in vals.iter().enumerate() {
        let k = i % VALS;
        let q = if k == 4 || k == 12 { (v + 1.0) * 0.5 } else { *v };
        out.extend_from_slice(&((q.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes());
    }
    out
}

/// A stream song's map for the scenes (`spatial_map?id=`): its instruments
/// as they are now (an id of an older version gets the newest), and its notes
/// from ten seconds before the place heard on (all that are known: they run a
/// little ahead of the frames), in the session's seconds (the scenes' frames
/// are the session's).
pub fn map_json(id: u32) -> Option<String> {
    if id & 0x8000_0000 == 0 {
        return None;
    }
    let g = shared().live.lock().unwrap();
    map_json_of(g.as_ref()?, id)
}

fn map_json_of(l: &Live, id: u32) -> Option<String> {
    let song = l.songs.iter().find(|s| map_id(l.gen, s.n, s.version) & 0xffff_0000 == id & 0xffff_0000)?;
    let at = song.f0 as f64 / FPS;
    let w0 = l.heard_s - at - 10.0;
    let notes: Vec<serde_json::Value> = song
        .map
        .notes
        .iter()
        .filter(|n| n.off as f64 >= w0)
        .map(|n| serde_json::json!([n.obj, n.key, n.on as f64 + at, n.off as f64 + at, n.vel]))
        .collect();
    let objects: Vec<serde_json::Value> = song
        .map
        .objects
        .iter()
        .map(|o| serde_json::json!({"kind": o.kind, "name": o.name, "stem": o.stem, "x": o.x, "width": o.width, "colour": o.colour}))
        .collect();
    Some(serde_json::json!({"count": objects.len(), "objects": objects, "notes": notes, "kit": if song.map.kit_bands { "bands" } else { "drumsep" }}).to_string())
}

/// For the page (`spatial_track_status` on a stream).
pub fn status(gen: u64) -> serde_json::Value {
    let g = shared().live.lock().unwrap();
    let l = g.as_ref().filter(|l| l.gen == gen);
    serde_json::json!({
        "pack": pack::installed().is_some(),
        "stream": true,
        "gpu": card().open.load(Ordering::Acquire),
        "why": why_not().map(|w| w.word()),
        "songs": l.map(|l| l.songs.iter().map(|s| serde_json::json!({
            "n": s.n, "from": s.f0 as f64 / FPS, "instruments": s.map.objects.iter().map(|o| o.name.clone()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>()),
        "segments": l.map(|l| l.sep.done), "skipped": l.map(|l| l.sep.skipped),
        "drums": card().drums.load(Ordering::Acquire),
        "drum_chunks": l.map(|l| l.drums.done), "drum_restarts": l.map(|l| l.drums.skipped),
        "notes": l.map(|l| l.placed.iter().map(|v| v.len()).collect::<Vec<_>>()), "notes_ms": l.map(|l| l.notes_ms.round()),
    })
}

#[cfg(test)]
mod tests;
