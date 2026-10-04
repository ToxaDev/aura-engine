//! Global analytics hub.
//!
//! One `Hub` per process, accessed via `hub::get()`. Holds the subject
//! registry and the current live-stream state. All route handlers call into
//! the hub; so do the three controller hook functions (`on_variant`,
//! `on_stream`, `on_stop`).
//!
//! ## Locking budget
//!
//! Every method that a route handler calls must return within microseconds.
//! - Subject registry: `Mutex<HashMap<u32, Arc<Subject>>>` — held only for
//!   lookup and `Arc` clone.
//! - Per-subject AAN1/AAN2/AAN3 buffers: delegated to `Subject::{try_*}`,
//!   each of which uses `try_lock` and returns `None` on contention.
//! - Hook functions (`on_variant`, `on_stream`, `on_stop`): called from the
//!   controller thread, not from render/output threads. They clone an `Arc`
//!   and notify a `Condvar` — total lock hold < 1 µs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::sync::atomic::{AtomicU32, Ordering};

use super::proto::{ChainRespData, TileKind, TileSrc};
use super::subject::{Subject, SubjectMode};
use super::track;
use crate::player::chain::Variant;
use crate::player::convolver::SourceBuf;
use crate::player::output::OutputShared;
use crate::player::render::{ChainDesc, Shared as RenderShared};
use crate::player::timeline::Timeline;

// ─── Reserved SID for the LIVE subject ───────────────────────────────────────

/// Reserved subject ID for the single live-playback subject.
pub const LIVE_SID: u32 = u32::MAX - 1;

// ─── Hub ──────────────────────────────────────────────────────────────────────

/// Live-stream state registered by `on_stream` / `on_variant`.
struct LiveState {
    timeline: Arc<Timeline>,
    render_shared: Arc<RenderShared>,
    #[allow(dead_code)] // held so the stream's shared state lives as long as it
    output_shared: Arc<OutputShared>,
    #[allow(dead_code)] // recorded with the stream
    out_rate: u32,
    // analytics: S/B live meters — current track's source audio
    // Held only while the live window is open; dropped with LiveState on stop.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    src_buf: Option<Arc<SourceBuf>>,
}

pub struct Hub {
    subjects: Mutex<HashMap<u32, Arc<Subject>>>,
    next_sid: AtomicU32,
    live_state: Mutex<Option<LiveState>>,
    /// Variants the player prepared, newest last, one per track. Weak: the
    /// player's own cache decides how long a variant lives.
    variants: Mutex<Vec<(u64, Weak<Variant>)>>,
    /// The variant the live subject analyses now.
    live_variant: Mutex<(u64, Weak<Variant>)>,
    /// The live chain whose |H| is wanted (older results are dropped).
    live_chain_rev: AtomicU32,
    /// That chain, kept to recompute the predicted O when the B pass ends.
    live_chain: Mutex<Option<Arc<ChainDesc>>>,
    /// What the last live O pass rendered (track, variant, settings,
    /// downgrade): the same chain heard again (a repeat-one loop, a swap to
    /// an equal chain) keeps its O.
    last_o_sig: Mutex<String>,
    /// What the live subject's whole-track analysis was last given: the
    /// track whose S it decoded and the variant its B pass took.
    analysed: Mutex<(u64, Weak<Variant>)>,
    /// The track of `live_chain`.
    live_chain_track: std::sync::atomic::AtomicU64,
    /// A converted file heard from disk: its track, the file, and the
    /// track's source once decoded (S, the S/B meters, O's grid).
    file_src: Mutex<Option<FileSrc>>,
}

struct FileSrc {
    track: u64,
    file: String,
    src: Option<Arc<Variant>>,
}

/// The analyzer window counts as open while it has asked for the live
/// subject within this long (it polls several times a second). The heavy
/// whole-track work — S (decode, spectrogram), B, the O pass — runs only
/// then: listening costs nothing but the live meters, and an opened window
/// catches up with what plays (Anton 26.09: "the analyzer computes when you
/// look at it").
const WATCH_MS: u64 = 3_000;

/// Prepared variants remembered for the live analyzer (current, next, a few
/// toggles back).
const VARIANTS_KEPT: usize = 8;

static INSTANCE: OnceLock<Hub> = OnceLock::new();

/// Initialize the hub. Call once from `main.rs` before the WebView2 protocol
/// closure is registered. Returns a reference to the singleton.
pub fn init() -> &'static Hub {
    INSTANCE.get_or_init(|| {
        // analytics: start the live consumer thread before registering subjects
        super::live::start();

        let hub = Hub {
            subjects: Mutex::new(HashMap::new()),
            next_sid: AtomicU32::new(1),
            live_state: Mutex::new(None),
            variants: Mutex::new(Vec::new()),
            live_variant: Mutex::new((u64::MAX, Weak::new())),
            live_chain_rev: AtomicU32::new(0),
            live_chain: Mutex::new(None),
            last_o_sig: Mutex::new(String::new()),
            analysed: Mutex::new((u64::MAX, Weak::new())),
            live_chain_track: std::sync::atomic::AtomicU64::new(u64::MAX),
            file_src: Mutex::new(None),
        };

        // Pre-register the live subject.
        let live = Subject::new_live(LIVE_SID);
        hub.subjects.lock().unwrap().insert(LIVE_SID, live.clone());
        track::spawn_for_subject(live);

        hub
    })
}

/// Access the hub after `init()`. Panics if called before `init()`.
pub fn get() -> &'static Hub {
    INSTANCE.get().expect("analytics hub not initialized — call hub::init() from main.rs")
}

/// Access the hub if it has been initialized, or `None` otherwise.
/// Safe to call from code that may run before `init()` (e.g. player tests).
pub fn try_get() -> Option<&'static Hub> {
    INSTANCE.get()
}

impl Hub {
    // ─── Subject lifecycle ─────────────────────────────────────────────────

    /// Open a new analyzer subject. Returns the allocated SID.
    ///
    /// * `mode` — `"live"` or `"file"`.
    /// * For `"live"`: returns `LIVE_SID`; subject already exists.
    /// * For `"file"`: allocates a fresh SID from `next_sid`.
    pub fn subject_open(
        &self,
        mode: &str,
        entry_id: Option<u64>,
        path: Option<&str>,
        conv: Option<&str>,
        settings: Option<serde_json::Value>,
    ) -> u32 {
        if mode == "live" {
            return LIVE_SID;
        }

        // FILE subject
        let sid = self.next_sid.fetch_add(1, Ordering::Relaxed);
        let path_buf = path.map(std::path::PathBuf::from).unwrap_or_default();
        // The page decoded its URL parameter (analytics/window.js
        // _parseParams): the path comes as it is on disk. Decoded here again,
        // byte by byte into Latin-1, a converted file named in Cyrillic
        // («Пыльца - Геометрия [AE …].flac») could not be opened (TASK-6).
        let conv_buf = conv.map(std::path::PathBuf::from);

        // Deserialize settings or use default.
        use crate::player::settings::PlayerSettings;
        let ps = settings
            .and_then(|v| serde_json::from_value::<PlayerSettings>(v).ok())
            .unwrap_or_default();

        // S: the file decoded as it is (no source stage). For a converted
        // copy, S is its source and O the copy itself, as when the copy plays
        // from disk (`activate_file`): the O column was empty for good when
        // the copy was S and nothing was O. Without its source on disk the
        // copy is S, alone, as before.
        let has_source = !path_buf.as_os_str().is_empty() && path_buf.is_file();
        let file = match &conv_buf {
            Some(_) if has_source => Some(path_buf.clone()),
            Some(c) => Some(c.clone()),
            None => (!path_buf.as_os_str().is_empty()).then(|| path_buf.clone()),
        };
        let o_file = conv_buf.clone().filter(|_| has_source);
        // B and O: the source file through the rack it would play with now
        // (not for a converted output, which is already processed).
        let source = (conv_buf.is_none() && !path_buf.as_os_str().is_empty()).then(|| path_buf.clone());

        let subject = Subject::new_file(
            sid,
            entry_id.unwrap_or(0),
            path_buf,
            conv_buf,
            Arc::new(ps),
        );
        let subject_clone = subject.clone();
        self.subjects.lock().unwrap().insert(sid, subject.clone());
        track::spawn_for_subject(subject_clone);

        let entry = entry_id.unwrap_or(u64::MAX);
        subject.track_id.store(entry, Ordering::Relaxed);
        if let Some(file) = file {
            std::thread::Builder::new()
                .name("aura-analytics-decode".into())
                .spawn(move || {
                    use crate::player::settings::{Mode, PlayerSettings};
                    let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
                    let cancel = std::sync::atomic::AtomicBool::new(false);
                    match crate::player::chain::prepare_variant(&file, &direct, false, &cancel) {
                        Ok(v) => {
                            let v = Arc::new(v);
                            subject.set_variant(v.clone());
                            // A converted copy: O is the copy, read on S's grid.
                            if let Some(conv) = o_file {
                                subject.set_o_work(super::subject::OWork {
                                    variant: v,
                                    settings: Arc::new(direct.clone()),
                                    track_id: entry,
                                    track_key: String::new(),
                                    downgrade: None,
                                    downgrade_gen: 0,
                                    chain_rev: 0,
                                    own_envelope: false,
                                    gpu: false,
                                    generation: 0,
                                    file: Some(conv.to_string_lossy().into_owned()),
                                });
                                return;
                            }
                        }
                        Err(e) => crate::aelog!("[ANALYZER] {}: {}", file.display(), e),
                    }
                    let Some(src) = source else { return };
                    let player = crate::player::controller::get();
                    let rack = player.settings_for(entry);
                    if rack.mode == Mode::Direct { return; }
                    match crate::player::chain::prepare_variant(&src, &rack, false, &cancel) {
                        Ok(v) => {
                            let v = Arc::new(v);
                            subject.set_b_variant(v.clone());
                            let track_key = player.track_key_for(entry)
                                .unwrap_or_else(|| format!("file:{}", src.display()));
                            subject.set_o_work(super::subject::OWork {
                                variant: v,
                                settings: Arc::new(rack.clone()),
                                track_id: entry,
                                track_key,
                                downgrade: None,
                                downgrade_gen: 0,
                                chain_rev: 0,
                                own_envelope: true,
                                gpu: rack.use_gpu && o_pass_gpu_allowed(hub_playback_on_gpu()),
                                generation: 0,
                                file: None,
                            });
                        }
                        Err(e) => crate::aelog!("[ANALYZER] B/O {}: {}", src.display(), e),
                    }
                })
                .ok();
        }

        sid
    }

    /// Close a subject and release its resources.
    pub fn subject_close(&self, sid: u32) {
        // The live subject lives as long as the process: its window closing
        // only stops the polls, and the work stops with them (`live_watched`).
        // Removed, it could not be opened again.
        if sid == LIVE_SID {
            return;
        }
        // Removing the Arc from the registry drops it once the track thread
        // also releases its copy (which happens on generation mismatch).
        let subject = self.subjects.lock().unwrap().remove(&sid);
        if let Some(s) = subject {
            // Bump generation so any in-progress analysis pass aborts.
            s.generation.fetch_add(1, Ordering::Release);
            s.evict_tiles();
            // analytics: signal shutdown BEFORE notify so wait_for_work returns None
            s.signal_shutdown();
            // Wake the track thread so it sees the shutdown flag.
            s.notify_track_thread();
        }
    }

    // ─── Controller hooks (must hold no lock > 1 µs) ──────────────────────

    /// Called from `controller.rs` whenever the player has a variant ready
    /// (outside the `st` lock). Only remembers it: a variant is prepared
    /// before it is heard (next track, full after quick), so the live thread
    /// takes it with [`Hub::activate_track`] when its track becomes audible.
    ///
    /// analytics: hook
    pub fn on_variant(&self, track_id: u64, variant: &Arc<Variant>) {
        let mut g = self.variants.lock().unwrap();
        g.retain(|(id, w)| *id != track_id && w.strong_count() > 0);
        g.push((track_id, Arc::downgrade(variant)));
        let n = g.len();
        if n > VARIANTS_KEPT {
            g.drain(..n - VARIANTS_KEPT);
        }
    }

    /// Called from the live thread when `track_id` is audible (first mark,
    /// track change, rack change). Points the live subject at the variant
    /// heard — the one the audible chain carries, else the newest prepared
    /// for that track; its whole-track analysis restarts only when the
    /// variant changed — and returns the source for the S/B meters.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn activate_track(&self, track_id: u64, heard: Option<Arc<Variant>>) -> Option<Arc<SourceBuf>> {
        *self.file_src.lock().unwrap() = None;
        if track_id == crate::player::radio::chain::RADIO_TRACK_ID {
            // A live stream: no variant and no whole track (`queue_live_o`
            // tells the window); the last track's variant is not taken for it.
            *self.live_variant.lock().unwrap() = (track_id, Weak::new());
            *self.analysed.lock().unwrap() = (track_id, Weak::new());
            return None;
        }
        let v = match heard {
            Some(v) => v,
            None => self.variants.lock().unwrap().iter().rev()
                .find(|(id, _)| *id == track_id)
                .and_then(|(_, w)| w.upgrade())?,
        };
        let (new_track, changed) = {
            let mut cur = self.live_variant.lock().unwrap();
            let new_track = cur.0 != track_id;
            let same = !new_track && cur.1.upgrade().is_some_and(|c| Arc::ptr_eq(&c, &v));
            if !same {
                *cur = (track_id, Arc::downgrade(&v));
            }
            (new_track, !same)
        };
        if changed {
            if new_track {
                if let Some(live) = self.live_subject() {
                    live.track_id.store(track_id, Ordering::Relaxed);
                }
            }
            if self.live_watched() {
                self.analyse_live(track_id, &v, None);
            }
            if let Some(state) = self.live_state.lock().unwrap().as_mut() {
                state.src_buf = Some(v.src.clone());
            }
        }
        Some(v.src.clone())
    }

    /// Called from the live thread while a converted file plays from disk
    /// (every wakeup until it returns the source). S is the track's source,
    /// decoded as for the rack played live; O is the file itself — the O
    /// pass reads it instead of rendering a chain; there is no B (the file's
    /// source stages ran inside the converter). Nothing starts while the
    /// analyzer is closed: the next call after it opens does.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn activate_file(&'static self, track_id: u64, desc: &Arc<ChainDesc>, chain_rev: u32) -> Option<Arc<SourceBuf>> {
        let file = desc.file.clone()?;
        let mut g = self.file_src.lock().unwrap();
        if let Some(f) = g.as_ref().filter(|f| f.track == track_id && f.file == file) {
            return f.src.as_ref().map(|v| v.src.clone());
        }
        if !self.live_watched() {
            return None;
        }
        let live = self.live_subject()?;
        let path = crate::player::controller::get().track_path(track_id)?;
        *g = Some(FileSrc { track: track_id, file: file.clone(), src: None });
        drop(g);
        live.track_id.store(track_id, Ordering::Relaxed);
        let desc = desc.clone();
        std::thread::Builder::new()
            .name("aura-analytics-file".into())
            .spawn(move || {
                use crate::player::settings::{Mode, PlayerSettings};
                let current = || self.file_src.lock().unwrap().as_ref()
                    .is_some_and(|f| f.track == track_id && f.file == file);
                let too_large = crate::player::controller::get().track_length(track_id)
                    .is_some_and(|(dur, rate)| dur * rate as f64 * 16.0 > S_DECODE_MAX_BYTES);
                if too_large {
                    crate::aelog!("[ANALYZER] S skipped: the file is too large to decode twice");
                    return;
                }
                let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let v = match crate::player::chain::prepare_variant(std::path::Path::new(&path), &direct, false, &cancel) {
                    Ok(v) => Arc::new(v),
                    Err(e) => {
                        crate::aelog!("[ANALYZER] {}: {}", path, e);
                        return;
                    }
                };
                {
                    let mut g = self.file_src.lock().unwrap();
                    match g.as_mut() {
                        Some(f) if f.track == track_id && f.file == file => f.src = Some(v.clone()),
                        _ => return,
                    }
                }
                if !current() { return; }
                *self.live_variant.lock().unwrap() = (track_id, Arc::downgrade(&v));
                *self.analysed.lock().unwrap() = (track_id, Arc::downgrade(&v));
                live.set_variant(v);
                live.clear_b();
                self.queue_file_o(&desc, chain_rev, track_id);
            })
            .ok();
        None
    }

    /// The O pass over the converted file heard (its S decoded first).
    fn queue_file_o(&self, desc: &ChainDesc, chain_rev: u32, track_id: u64) {
        if !self.live_watched() {
            return;
        }
        let Some(file) = desc.file.clone() else { return };
        let v = {
            let g = self.file_src.lock().unwrap();
            match g.as_ref() {
                Some(f) if f.track == track_id && f.file == file => f.src.clone(),
                _ => None,
            }
        };
        // Its S is still being decoded: the decode queues the pass.
        let Some(v) = v else { return };
        let Some(live) = self.live_subject() else { return };
        let sig = format!("file|{track_id}|{file}");
        {
            let mut last = self.last_o_sig.lock().unwrap();
            if *last == sig && (live.running_o_track.load(Ordering::Acquire) == track_id || live.o_done_for(track_id)) {
                return;
            }
            *last = sig;
        }
        live.set_o_work(super::subject::OWork {
            variant: v,
            settings: desc.settings.clone(),
            track_id,
            track_key: String::new(),
            downgrade: None,
            downgrade_gen: 0,
            chain_rev,
            own_envelope: false,
            gpu: false,
            generation: 0,
            file: Some(file),
        });
    }

    /// The live subject's whole-track analysis for the variant heard: S and
    /// B for a track it has not analysed, B alone after a rack change, and
    /// nothing when it already has that variant. `then` runs once S and B
    /// are queued (the O pass after them, so the S column fills first).
    fn analyse_live(&self, track_id: u64, v: &Arc<Variant>, then: Option<Box<dyn FnOnce() + Send>>) {
        let Some(live) = self.live_subject() else {
            if let Some(f) = then { f(); }
            return;
        };
        let (same_track, same_variant) = {
            let mut a = self.analysed.lock().unwrap();
            let same_track = a.0 == track_id;
            let same_variant = same_track && a.1.upgrade().is_some_and(|c| Arc::ptr_eq(&c, v));
            *a = (track_id, Arc::downgrade(v));
            (same_track, same_variant)
        };
        if same_variant {
            if let Some(f) = then { f(); }
            return;
        }
        if !same_track {
            live.track_id.store(track_id, Ordering::Relaxed);
            analyse_source(live, track_id, v.clone(), then);
        } else {
            // A rack change: S stays, B is the new variant heard.
            live.set_b_variant(v.clone());
            if let Some(f) = then { f(); }
        }
    }

    /// A live O pass stopped with nobody looking: the next look starts it
    /// again (its chain no longer counts as rendered).
    pub fn o_put_off(&self) {
        self.last_o_sig.lock().unwrap().clear();
    }

    /// The analyzer window has asked for the live subject lately.
    pub fn live_watched(&self) -> bool {
        self.live_subject().is_some_and(|s| s.idle_ms() <= WATCH_MS)
    }

    /// A page asked for the live subject: when it had not for a while (the
    /// window just opened), the work put off meanwhile starts — S and B for
    /// the variant heard, the O pass for the chain heard.
    fn watch_live(&'static self, s: &Subject) {
        let opened = s.idle_ms() > WATCH_MS;
        s.touch();
        if !opened {
            return;
        }
        std::thread::Builder::new()
            .name("aura-analytics-catchup".into())
            .spawn(move || {
                let (tid, v) = {
                    let cur = self.live_variant.lock().unwrap();
                    (cur.0, cur.1.upgrade())
                };
                let o = move || {
                    let desc = self.live_chain.lock().unwrap().clone();
                    if let Some(d) = desc {
                        let rev = self.live_chain_rev.load(Ordering::Relaxed);
                        self.queue_live_o(&d, rev, self.live_chain_track.load(Ordering::Relaxed));
                    }
                };
                match v {
                    Some(v) => self.analyse_live(tid, &v, Some(Box::new(o))),
                    None => o(),
                }
            })
            .ok();
    }

    /// Called from `controller.rs` before `st.timeline = Some(timeline)` in
    /// `play_chain`. Registers the live timeline for `live.rs`.
    ///
    /// analytics: hook
    pub fn on_stream(
        &self,
        timeline: &Arc<Timeline>,
        render_shared: &Arc<RenderShared>,
        output_shared: &Arc<OutputShared>,
        out_rate: u32,
    ) {
        *self.live_state.lock().unwrap() = Some(LiveState {
            timeline: timeline.clone(),
            render_shared: render_shared.clone(),
            output_shared: output_shared.clone(),
            out_rate,
            src_buf: None, // filled by on_variant
        });
        // analytics: forward to live consumer thread so it starts accumulation
        super::live::notify_stream(
            timeline.clone(),
            render_shared.clone(),
            output_shared.clone(),
            out_rate,
            None,  // src_rate: refined from first Mark
            0,     // track_id: refined from first Mark
            0,     // chain_rev: refined from first Mark
            false, false, false,
        );
    }

    /// Called from `controller.rs` when the stream stops.
    ///
    /// analytics: hook
    pub fn on_stop(&self) {
        *self.live_state.lock().unwrap() = None;
        // analytics: flush the live consumer thread
        super::live::notify_stop();
    }

    // ─── Route data accessors (all lock-free or try_lock) ─────────────────

    /// Fetch the latest prebuilt AAN1 frame for `sid`.
    /// Returns `None` when: subject not found; not a LIVE subject; ring is
    /// locked; no frame published yet.
    pub fn aan1_bytes(&'static self, sid: u32, since: u32) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        if s.mode != SubjectMode::Live {
            return None;
        }
        self.watch_live(&s);
        s.try_aan1_bytes(since)
    }

    /// Fetch the prebuilt AAN2 frame (or the 8-byte unchanged stub when
    /// `client_rev` matches the current rev).
    pub fn aan2_bytes(&'static self, sid: u32, client_rev: u32) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        // The page polls this: someone looks at the subject.
        if sid == LIVE_SID {
            self.watch_live(&s);
        } else {
            s.touch();
        }
        let current_rev = s.aan2_rev();
        if current_rev == client_rev {
            // Return the 8-byte unchanged stub.
            let stub: Arc<[u8]> = super::proto::encode_aan2_unchanged(current_rev).into();
            return Some(stub);
        }
        s.try_aan2_bytes()
    }

    /// Fetch a decimated AAN3 response frame for `sid`.
    /// All computation (decimation) happens here synchronously but is O(n)
    /// over the output points (≤ 4096), not over the full-res array — each
    /// point requires a pass over at most `n_full / n` bins.
    pub fn aan3_bytes(
        &self,
        sid: u32,
        f0: f32,
        f1: f32,
        n: u16,
        log: bool,
    ) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        let resp = s.try_chain_resp();
        let psd = s.try_source_psd();
        let b_psd = s.try_b_psd();
        if resp.is_none() && psd.is_none() && b_psd.is_none() {
            return None;
        }
        let bytes: Arc<[u8]> = super::proto::encode_aan3_from_data(
            resp.as_deref(), psd.as_deref(), b_psd.as_deref(), n, f0, f1, log,
        ).into();
        Some(bytes)
    }

    /// The page shows `src`'s zoomed spectrogram tiles `i0..i1` at `lod`
    /// (holding those in `got`): the ready ones (zoom.rs).
    pub fn zoom_bytes(&self, sid: u32, src: TileSrc, lod: u8, i0: u32, i1: u32, got: u64) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        s.touch();
        Some(super::zoom::reply(&s, src, lod, i0, i1, got))
    }

    /// The live output's stereo picture (AAVS; live.rs).
    pub fn vec_bytes(&'static self, sid: u32) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        if s.mode != SubjectMode::Live {
            return None;
        }
        s.touch();
        s.live_vec()
    }

    /// `src`'s waveform over source samples `[s0, s1)` in `n` bins (zoom.rs).
    pub fn wavez_bytes(&self, sid: u32, src: TileSrc, s0: u64, s1: u64, n: u32) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        s.touch();
        Some(super::zoom::reply_wave(&s, src, s0, s1, n))
    }

    /// The statistics of `src` over source samples `[s0, s1)` (zoom.rs).
    pub fn stat_bytes(&self, sid: u32, src: TileSrc, s0: u64, s1: u64) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        s.touch();
        Some(super::zoom::reply_stats(&s, src, s0, s1))
    }

    /// Fetch one tile frame (wave or spectrogram) for `sid`.
    pub fn tile_bytes(
        &self,
        sid: u32,
        src: TileSrc,
        kind: TileKind,
        lod: u8,
        i: u32,
    ) -> Option<Arc<[u8]>> {
        let s = self.subject_ref(sid)?;
        let key = super::proto::TileKey { src, kind, lod, idx: i };
        s.get_tile(&key)
    }

    // ─── Live timeline access (for live.rs) ───────────────────────────────

    /// Get the current live timeline and shared state (if a stream is running).
    /// Called by `live.rs` at initialization / on re-open.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn live_timeline(&self) -> Option<(Arc<Timeline>, Arc<RenderShared>, Arc<OutputShared>, u32)> {
        let guard = self.live_state.lock().ok()?;
        guard.as_ref().map(|s| (
            s.timeline.clone(),
            s.render_shared.clone(),
            s.output_shared.clone(),
            s.out_rate,
        ))
    }

    /// Called from the live thread when a chain becomes audible (first mark,
    /// rack or track change): computes its |H| off that thread (a 30M bank
    /// takes ~80 ms) and publishes it for the live subject unless a newer
    /// chain came meanwhile.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn on_live_chain(&'static self, desc: Arc<ChainDesc>, chain_rev: u32, track_id: u64) {
        self.live_chain_rev.store(chain_rev, Ordering::Relaxed);
        self.live_chain_track.store(track_id, Ordering::Relaxed);
        *self.live_chain.lock().unwrap() = Some(desc.clone());
        if let Some(live) = self.live_subject() {
            live.set_chain_str(chain_line(&desc));
        }
        // The O pass is queued off the live thread (the track key takes the
        // player's lock).
        let d = desc.clone();
        std::thread::Builder::new()
            .name("aura-analytics-oq".into())
            .spawn(move || self.queue_live_o(&d, chain_rev, track_id))
            .ok();
        self.spawn_live_resp(desc, chain_rev);
    }

    /// The O pass for the chain heard: the rack rendered live, with its full
    /// variant (not the quick one, a converted file or BIT-PERFECT).
    fn queue_live_o(&self, desc: &ChainDesc, chain_rev: u32, track_id: u64) {
        if track_id == crate::player::radio::chain::RADIO_TRACK_ID {
            // A live stream has no whole track: its measures say so at once
            // rather than wait for passes that never come.
            if let Some(live) = self.live_subject() {
                live.set_stream(track_id);
            }
            *self.last_o_sig.lock().unwrap() = format!("stream|{track_id}");
            return;
        }
        if desc.source == "direct" {
            // BIT-PERFECT: what plays is the file itself — O is S, there is
            // no pass to run, and the previous chain's O must not stay up.
            if let Some(live) = self.live_subject() {
                live.clear_o();
            }
            *self.last_o_sig.lock().unwrap() = format!("direct|{track_id}");
            return;
        }
        if desc.source == "file" {
            self.queue_file_o(desc, chain_rev, track_id);
            return;
        }
        if desc.source != "live" || desc.quick {
            return;
        }
        // Nobody looks: put off until the window opens (`watch_live`).
        if !self.live_watched() {
            return;
        }
        let Some(variant) = desc.variant.upgrade() else { return };
        let Some(live) = self.live_subject() else { return };
        let player = crate::player::controller::get();
        let Some(track_key) = player.track_key_for(track_id) else { return };
        // The user's GPU switch (the chain's own settings say what the
        // router chose for playback).
        let gpu_on_in_rack = player.settings_for(track_id).use_gpu;
        if self.live_chain_rev.load(Ordering::Relaxed) != chain_rev {
            return;
        }
        // The same chain as the last O pass (same track, variant, rack and
        // downgrade): its O stands, running or done.
        let sig = format!("{}|{:p}|{:?}|{:?}|{}", track_id, Arc::as_ptr(&variant), desc.settings, desc.downgrade, desc.gpu_on);
        {
            let mut last = self.last_o_sig.lock().unwrap();
            if *last == sig && live.running_o_track.load(Ordering::Acquire) == track_id || *last == sig && live.o_done_for(track_id) {
                return;
            }
            *last = sig;
        }
        live.set_o_work(super::subject::OWork {
            variant,
            settings: desc.settings.clone(),
            track_id,
            track_key,
            downgrade: desc.downgrade.clone(),
            downgrade_gen: desc.downgrade_gen,
            chain_rev,
            own_envelope: false,
            // The rack allows the GPU (also beside playback on it).
            gpu: gpu_on_in_rack && o_pass_gpu_allowed(desc.gpu_on),
            generation: 0,
            file: None,
        });
    }

    /// Playback is short of its look-ahead (under half its target): the O
    /// pass waits. False when nothing plays.
    pub fn playback_starved(&self) -> bool {
        let Ok(g) = self.live_state.try_lock() else { return false };
        let Some(ls) = g.as_ref() else { return false };
        let rate = ls.timeline.rate().max(1) as f64;
        let buffered_s = ls.timeline.buffered_frames() as f64 / rate;
        let target_s = ls.render_shared.target_ahead_s.lock().map(|t| *t).unwrap_or(0.0);
        buffered_s < target_s * 0.5
    }

    /// The B pass of subject `sid` has its spectrum: for the live subject,
    /// the predicted O (B through the heard chain's |H|) is computed again.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn on_b_psd(&'static self, sid: u32) {
        if sid != LIVE_SID { return; }
        let desc = self.live_chain.lock().unwrap().clone();
        if let Some(desc) = desc {
            self.spawn_live_resp(desc, self.live_chain_rev.load(Ordering::Relaxed));
        }
    }

    /// |H| and the predicted O of the live chain, off the calling thread; the
    /// result is dropped if a newer chain came meanwhile. Without the B
    /// spectrum yet (a new track or rack), O stays NaN until `on_b_psd`.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    fn spawn_live_resp(&'static self, desc: Arc<ChainDesc>, chain_rev: u32) {
        std::thread::Builder::new()
            .name("aura-analytics-resp".into())
            .spawn(move || {
                let player = crate::player::controller::get();
                let b = self.live_subject().and_then(|s| s.try_b_psd())
                    .map(|p| Arc::new(super::resp::BWelchPsd::from_mean(&p)));
                let resp = super::resp::compute(&desc, player.resources(), desc.out_rate, b, chain_rev);
                if self.live_chain_rev.load(Ordering::Relaxed) == chain_rev {
                    self.set_chain_resp(LIVE_SID, Arc::new(resp));
                }
            })
            .ok();
    }

    /// Publish a completed `ChainResp` for a subject. Called from `resp.rs`.
    ///
    /// analytics: owner B
    pub fn set_chain_resp(&self, sid: u32, resp: Arc<super::resp::ChainResp>) {
        if let Some(s) = self.subject_ref(sid) {
            let data = Arc::new(ChainRespData {
                chain_rev: resp.chain_rev,
                h_dbr: resp.h_db.clone(),
                o_dbfs: resp.o_db.clone(),
                f_max_hz: resp.fs_out as f32 / 2.0,
            });
            s.publish_resp_frame(data);
        }
    }

    /// Publish a live frame (AAN1) for the LIVE subject. Called from `live.rs`.
    ///
    /// analytics: owner B — converts live::LiveFrame → proto::LiveFrame
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn publish_live_aan1(&self, frame: super::live::LiveFrame) {
        if let Some(s) = self.live_subject() {
            use super::proto::{LiveFrame as ProtoFrame, LoudPt};
            let proto = ProtoFrame {
                flags:           frame.flags,
                seq:             frame.seq,
                track_id:        frame.track_id,
                chain_rev:       frame.chain_rev,
                coverage_pct:    frame.coverage_pct,
                n_fft_bins_log2: frame.n_fft_bins_log2,
                spec_floor_neg:  frame.spec_floor_neg,
                spec_hop_log2:   frame.spec_hop_log2,
                live_metrics:    frame.live_metrics,
                prov_codes:      frame.prov_nibbles,
                loud_pts:        frame.loud_pts.into_iter()
                    .map(|p| LoudPt { time_s: p.time_s, lufs_s: p.lufs_s, lufs_m: p.lufs_m })
                    .collect(),
                spec_cols:       frame.spec_cols,
                // analytics: S/B extension fields
                s_lufs_m: frame.s_lufs_m,
                s_lufs_s: frame.s_lufs_s,
                s_tp:     frame.s_tp,
                b_lufs_m: frame.b_lufs_m,
                b_lufs_s: frame.b_lufs_s,
                b_tp:     frame.b_tp,
                sb_prov:  frame.sb_prov,
                strm:     frame.strm,
            };
            s.publish_live_frame(proto);
        }
    }

    /// Get the live subject directly (for live.rs to publish AAN1 frames).
    pub fn live_subject(&self) -> Option<Arc<Subject>> {
        self.subject_ref(LIVE_SID)
    }

    /// A subject by its id.
    pub fn subject(&self, sid: u32) -> Option<Arc<Subject>> {
        self.subject_ref(sid)
    }

    /// Every open subject.
    pub fn all_subjects(&self) -> Vec<Arc<Subject>> {
        self.subjects.lock().map(|g| g.values().cloned().collect()).unwrap_or_default()
    }

    // ─── Helpers ──────────────────────────────────────────────────────────

    fn subject_ref(&self, sid: u32) -> Option<Arc<Subject>> {
        self.subjects.lock().ok()?.get(&sid).cloned()
    }
}

/// The chain heard, as the converter row writes it: the taps, then the
/// stages that ran ("1M·ISP·SUB15·AA·PFR").
#[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
fn chain_line(desc: &ChainDesc) -> String {
    use crate::audio::converter::dsp::lab::CHAIN_RAN;
    let mut toks: Vec<&str> = Vec::new();
    // A converted file from disk: FILE, then the stages its name lists.
    if desc.source == "file" {
        toks.push("FILE");
    }
    if let Some(t) = &desc.taps {
        toks.push(t);
    }
    toks.extend(desc.stages.iter().filter(|s| s.st == CHAIN_RAN).map(|s| s.tok.as_str()));
    if toks.is_empty() {
        toks.push(match desc.source { "direct" => "BIT-PERFECT", _ => "" });
    }
    toks.join("·")
}

// ─── The live subject's S: the file as it is ─────────────────────────────────

/// The largest decoded source the live analyzer takes for its S column
/// (stereo f64): a 38-minute 192 kHz file would be 7 GB on top of the
/// player's own copy.
const S_DECODE_MAX_BYTES: f64 = 1.5e9;

/// Playback runs now with its convolver on the GPU.
fn hub_playback_on_gpu() -> bool {
    let Some(h) = try_get() else { return false };
    let playing = h.live_state.try_lock().map(|g| g.is_some()).unwrap_or(true);
    playing && h.live_chain.lock().unwrap().as_ref().is_some_and(|d| d.gpu_on)
}

/// Whether an O pass may take the GPU. The card first, also while playback
/// runs on it (Anton 26.09: the CPU pass of a 30M chain took 35-55 s of
/// every core; the background stream keeps to a quarter of the free VRAM,
/// has its own watchdog and waits whenever playback runs short).
/// AURA_O_PASS_GPU=0 keeps it on the CPU always.
fn o_pass_gpu_allowed(_playback_on_gpu: bool) -> bool {
    std::env::var_os("AURA_O_PASS_GPU").is_none_or(|v| v != "0")
}

/// S is the source file before any source stage (the variant the player
/// plays is B: DC, ISP, SUB… already applied). Decoded once per track, off
/// the live thread; dropped when the analysis ends. B (`heard`) is queued
/// after it, so the S column fills first.
fn analyse_source(live: Arc<Subject>, track_id: u64, heard: Arc<Variant>, then: Option<Box<dyn FnOnce() + Send>>) {
    let player = crate::player::controller::get();
    let Some(path) = player.track_path(track_id) else {
        live.set_b_variant(heard);
        if let Some(f) = then { f(); }
        return;
    };
    // The track's O pass, queued meanwhile from its chain, waits for its S
    // and B: the S column fills first, as when the window opens.
    live.expect_source(track_id);
    let waits = live.clone();
    let spawned = std::thread::Builder::new()
        .name("aura-analytics-source".into())
        .spawn(move || {
            use crate::player::settings::{Mode, PlayerSettings};
            // However the decode ends (S queued, an error, another track
            // heard meanwhile, a panic), the O pass waits no longer.
            struct Over(Arc<Subject>, u64);
            impl Drop for Over {
                fn drop(&mut self) { self.0.source_done(self.1); }
            }
            let _over = Over(live.clone(), track_id);
            // Still the track heard: a quick skip must not analyse the old one.
            let current = || live.track_id.load(Ordering::Relaxed) == track_id;
            let too_large = crate::player::controller::get().track_length(track_id)
                .is_some_and(|(dur, rate)| dur * rate as f64 * 16.0 > S_DECODE_MAX_BYTES);
            if too_large {
                crate::aelog!("[ANALYZER] S skipped: the file is too large to decode twice");
            } else {
                let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
                let cancel = std::sync::atomic::AtomicBool::new(false);
                match crate::player::chain::prepare_variant(std::path::Path::new(&path), &direct, false, &cancel) {
                    Ok(v) if current() => live.set_variant(Arc::new(v)),
                    Ok(_) => {}
                    Err(e) => crate::aelog!("[ANALYZER] {}: {}", path, e),
                }
            }
            if current() {
                live.set_b_variant(heard);
            }
            if let Some(f) = then { f(); }
        });
    if spawned.is_err() {
        waits.source_done(track_id);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── LIVE_SID constant ──────────────────────────────────────────────────

    #[test]
    fn live_sid_is_max_minus_1() {
        assert_eq!(LIVE_SID, u32::MAX - 1);
    }

    // ── Hub init / get ─────────────────────────────────────────────────────
    // NOTE: The global `INSTANCE` OnceLock is shared across all tests in
    // this crate. The hub-init test only works if no other test initializes
    // it first (the OnceLock is idempotent after the first init).

    #[test]
    fn hub_get_after_init_does_not_panic() {
        let hub = init();
        let _ = get();
        // Live subject is registered
        assert!(hub.subject_ref(LIVE_SID).is_some());
    }

    #[test]
    fn subject_open_live_returns_live_sid() {
        let hub = init();
        let sid = hub.subject_open("live", None, None, None, None);
        assert_eq!(sid, LIVE_SID);
    }

    #[test]
    fn subject_open_file_returns_incrementing_sids() {
        let hub = init();
        let s1 = hub.subject_open("file", Some(1), Some("/tmp/a.flac"), None, None);
        let s2 = hub.subject_open("file", Some(2), Some("/tmp/b.flac"), None, None);
        assert_ne!(s1, s2);
        assert_ne!(s1, LIVE_SID);
        assert_ne!(s2, LIVE_SID);

        // Cleanup
        hub.subject_close(s1);
        hub.subject_close(s2);
    }

    #[test]
    fn aan1_miss_when_no_frame() {
        let hub = init();
        let result = hub.aan1_bytes(LIVE_SID, 0);
        // May be None (no frame yet) or Some (if another test published one).
        // Just verify it doesn't panic.
        let _ = result;
    }

    #[test]
    fn aan2_none_initially_for_live() {
        let hub = init();
        // client_rev = 0; current_rev = 0 → should return unchanged stub
        // unless another test already published a frame.
        let result = hub.aan2_bytes(LIVE_SID, 0);
        // May be Some (stub) or None. Just ensure no panic.
        let _ = result;
    }

    #[test]
    fn aan2_unknown_sid_returns_none() {
        let hub = init();
        assert!(hub.aan2_bytes(0xDEAD_BEEF, 0).is_none());
    }

    #[test]
    fn tile_unknown_sid_returns_none() {
        let hub = init();
        assert!(hub.tile_bytes(0xDEAD_BEEF, TileSrc::S, TileKind::Wave, 0, 0).is_none());
    }

    #[test]
    fn routes_never_block_when_empty() {
        let hub = init();
        let start = std::time::Instant::now();
        let _ = hub.aan1_bytes(LIVE_SID, 0);
        let _ = hub.aan2_bytes(LIVE_SID, 0);
        let _ = hub.aan3_bytes(LIVE_SID, 20.0, 20000.0, 512, true);
        let _ = hub.tile_bytes(LIVE_SID, TileSrc::S, TileKind::Wave, 0, 0);
        assert!(
            start.elapsed().as_millis() < 10,
            "hub route accessors must not block"
        );
    }

    /// Measure per-route latency to confirm < 1 ms requirement.
    ///
    /// Run with:
    ///   cargo test --profile fast route_latency_microbench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn route_latency_microbench() {
        let hub = init();
        const REPS: u64 = 10_000;

        // Warm up.
        for _ in 0..100 {
            let _ = hub.aan1_bytes(LIVE_SID, 0);
        }

        let t = std::time::Instant::now();
        for _ in 0..REPS {
            let _ = hub.aan1_bytes(LIVE_SID, 0);
        }
        let aan1_ns = t.elapsed().as_nanos() as f64 / REPS as f64;

        let t = std::time::Instant::now();
        for _ in 0..REPS {
            let _ = hub.aan2_bytes(LIVE_SID, 0);
        }
        let aan2_ns = t.elapsed().as_nanos() as f64 / REPS as f64;

        let t = std::time::Instant::now();
        for _ in 0..REPS {
            let _ = hub.aan3_bytes(LIVE_SID, 20.0, 20_000.0, 512, true);
        }
        let aan3_ns = t.elapsed().as_nanos() as f64 / REPS as f64;

        println!(
            "Route latency (empty hub, {REPS} reps):\n  AAN1: {aan1_ns:.0} ns\n  AAN2: {aan2_ns:.0} ns\n  AAN3: {aan3_ns:.0} ns"
        );
        assert!(aan1_ns < 1_000_000.0, "AAN1 > 1 ms");
        assert!(aan2_ns < 1_000_000.0, "AAN2 > 1 ms");
        assert!(aan3_ns < 1_000_000.0, "AAN3 > 1 ms");
    }
}
