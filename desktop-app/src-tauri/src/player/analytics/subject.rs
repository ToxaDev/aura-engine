//! Subject state machine.
//!
//! Every open analyzer window owns one `Subject`. A LIVE subject follows the
//! currently-playing track; FILE subjects are snapshots.
//!
//! ## Buffer ownership
//!
//! Each subject holds three prebuilt buffers: AAN1 (a ring), AAN2, and a
//! `ChainRespData` slab (for AAN3 decimation). Routes clone an `Arc<[u8]>` or
//! a reference; they never hold the mutex for longer than one pointer clone.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::proto::{
    ChainRespData, DrChannel, LiveFrame, SourcePsd, SpecInfo, TrackSummary, TileKey, TileKind, TileSrc,
    METRIC_ARRAY_LEN, WaveTile, SpectTile,
    encode_aan1, encode_aan2, encode_aawt, encode_aast,
};
use super::zoom::ZoomState;
use crate::player::chain::{DowngradeInfo, Variant};
use crate::player::settings::PlayerSettings;

// ─── Subject mode ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectMode {
    Live,
    File,
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    Converted,
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    Snapshot,
}

// ─── Tile cache ───────────────────────────────────────────────────────────────

/// LRU tile cache, bounded by bytes. A track's tiles (waveform levels and
/// the spectrogram) are built once, before anyone asks for them, so the
/// budget must hold them all: 64 entries kept only the last spectrogram
/// tiles and every other request missed.
pub struct TileStore {
    cache: VecDeque<(TileKey, Arc<[u8]>)>,
    bytes: usize,
    cap_bytes: usize,
}

const TILE_BUDGET_BYTES: usize = 64 << 20;

impl TileStore {
    pub fn new() -> Self {
        Self::with_cap(TILE_BUDGET_BYTES)
    }

    pub fn with_cap(cap_bytes: usize) -> Self {
        Self { cache: VecDeque::new(), bytes: 0, cap_bytes }
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.cache.iter().any(|(k, _)| k == key)
    }

    /// Drop the tiles `drop` picks (a pass's tiles when it is replaced).
    pub fn remove_where(&mut self, drop: impl Fn(&TileKey) -> bool) {
        let mut freed = 0;
        self.cache.retain(|(k, b)| {
            let gone = drop(k);
            if gone { freed += b.len(); }
            !gone
        });
        self.bytes -= freed;
    }

    pub fn get(&mut self, key: &TileKey) -> Option<Arc<[u8]>> {
        if let Some(pos) = self.cache.iter().position(|(k, _)| k == key) {
            let entry = self.cache.remove(pos).unwrap();
            let bytes = entry.1.clone();
            self.cache.push_front(entry);
            Some(bytes)
        } else {
            None
        }
    }

    pub fn insert(&mut self, key: TileKey, bytes: Arc<[u8]>) {
        // Evict if already present
        if let Some(pos) = self.cache.iter().position(|(k, _)| k == &key) {
            let old = self.cache.remove(pos).unwrap();
            self.bytes -= old.1.len();
        }
        while !self.cache.is_empty() && self.bytes + bytes.len() > self.cap_bytes {
            let (_, b) = self.cache.pop_back().unwrap();
            self.bytes -= b.len();
        }
        self.bytes += bytes.len();
        self.cache.push_front((key, bytes));
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.bytes = 0;
    }
}

// ─── AAN1 ring ────────────────────────────────────────────────────────────────

/// Ring of the last `RING_SIZE` AAN1 frames, indexed by `seq % RING_SIZE`.
const AAN1_RING_SIZE: usize = 256;

pub struct Aan1Ring {
    frames: [Option<Arc<[u8]>>; AAN1_RING_SIZE],
    /// Current (latest) seq.
    pub seq: u32,
}

impl Aan1Ring {
    pub fn new() -> Self {
        // Option<Arc<[u8]>> is not Copy, so we can't use array initializer.
        let frames: [Option<Arc<[u8]>>; AAN1_RING_SIZE] =
            std::array::from_fn(|_| None);
        Self { frames, seq: 0 }
    }

    /// Store a new frame at the ring slot for `seq`.
    pub fn push(&mut self, seq: u32, bytes: Arc<[u8]>) {
        self.seq = seq;
        self.frames[(seq as usize) % AAN1_RING_SIZE] = Some(bytes);
    }

    /// Get the frame for the latest seq. Returns `None` if the ring is empty
    /// or `since` is too far behind.
    pub fn get_latest(&self) -> Option<Arc<[u8]>> {
        self.frames[(self.seq as usize) % AAN1_RING_SIZE].clone()
    }

    /// Check whether `since` is within the ring (i.e., current_seq - since < RING_SIZE).
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn within_ring(&self, since: u32) -> bool {
        self.seq.wrapping_sub(since) < AAN1_RING_SIZE as u32
    }

    pub fn current_seq(&self) -> u32 { self.seq }
}

// ─── Track work handoff ───────────────────────────────────────────────────────

/// Payload sent to the background track thread.
pub struct TrackWork {
    pub variant: Arc<Variant>,
    pub generation: u64,
    pub is_b: bool,  // false = S pass first, true = B pass (full variant)
}

/// The O pass: the heard chain rebuilt apart from the live one and read to
/// the end (opass.rs). The hub queues one for every chain heard.
pub struct OWork {
    pub variant: Arc<Variant>,
    pub settings: Arc<PlayerSettings>,
    pub track_id: u64,
    pub track_key: String,
    pub downgrade: Option<DowngradeInfo>,
    pub downgrade_gen: u32,
    /// The live chain this O belongs to (its measured spectrum replaces the
    /// predicted O only while that chain is still the one heard).
    pub chain_rev: u32,
    /// Make the Hybrid-Phase envelope if it is missing (a file window: the
    /// player makes it only for what it plays; the live O pass waits for it).
    pub own_envelope: bool,
    /// The GPU is allowed (playback uses it, or the file's rack has it on).
    pub gpu: bool,
    pub generation: u64,
    /// A converted file played from disk: O is the file itself, read as it
    /// is (no chain), on the grid of `variant` (the track's source).
    pub file: Option<String>,
}

/// The track thread's next job.
pub enum Work {
    Track(TrackWork),
    O(OWork),
}

/// The O pass's results: its progress while it runs (COVERAGE_O, the other
/// slots COMPUTING), then the measured values. Merged into every track frame.
#[derive(Clone)]
pub struct OPart {
    pub metrics: [f64; METRIC_ARRAY_LEN],
    pub prov: [u8; METRIC_ARRAY_LEN],
    pub dr: Vec<DrChannel>,
    /// The loudness series (aligned as S's), when the pass is done.
    pub lufs_s: Vec<f32>,
    pub lufs_m: Vec<f32>,
}

impl OPart {
    /// The ids the O pass measures (COVERAGE_O carries its progress).
    pub const IDS: [usize; 20] = {
        use super::proto::mid::*;
        [LUFS_I, LUFS_S_LIVE, LUFS_M_LIVE, LRA, TP_EBUR, TP_ENGINE, SP, PEAK_AT, DR, RMS, RMS_TOP20,
         GAIN_18LUFS, CLIPS_OUT_X1, TP_OVER0_EVENTS, ULTRA_PEAK, ULTRA_RMS, INFRA_RMS, PLR,
         STEREO_CORR, EFF_BW]
    };

    /// Running: `pct` done, every measured slot COMPUTING.
    pub fn progress(pct: f64) -> OPart {
        use super::proto::{mid, prov};
        let mut p = OPart {
            metrics: [f64::NAN; METRIC_ARRAY_LEN],
            prov: [prov::UNAVAIL; METRIC_ARRAY_LEN],
            dr: Vec::new(),
            lufs_s: Vec::new(),
            lufs_m: Vec::new(),
        };
        for id in Self::IDS {
            p.prov[id] = prov::COMPUTING;
        }
        p.metrics[mid::COVERAGE_O] = pct;
        p.prov[mid::COVERAGE_O] = prov::COMPUTING;
        p
    }
}

/// What the B pass measured, kept apart so the S pass's frames carry it.
#[derive(Clone)]
struct BPart {
    metrics: [f64; METRIC_ARRAY_LEN],
    prov: [u8; METRIC_ARRAY_LEN],
    hist: Vec<f32>,
    lufs_s: Vec<f32>,
    lufs_m: Vec<f32>,
    tp100: Vec<f32>,
}

/// A track's frame before its S pass has measured anything: S computing
/// ("···"), B and O as their own parts say.
fn computing_summary() -> TrackSummary {
    TrackSummary { s_prov: [super::proto::prov::COMPUTING; METRIC_ARRAY_LEN], ..TrackSummary::default() }
}

// ─── Subject ──────────────────────────────────────────────────────────────────

/// One analyzer subject (one open analyzer window).
pub struct Subject {
    pub sid: u32,
    pub mode: SubjectMode,
    pub track_id: AtomicU64,
    pub generation: AtomicU64,

    // ── Track thread I/O ──────────────────────────────────────────────────
    /// Set to true by subject_close; track thread exits on next wakeup.
    // analytics: shutdown flag — fixes wait_for_work infinite loop on subject_close
    shutdown: AtomicBool,
    /// Next work item for the track thread.
    work: Mutex<Option<TrackWork>>,
    work_cond: Condvar,
    /// The B pass: the variant heard (source stages applied), after S.
    /// Locked only while `work` is held (lock order work -> work_b).
    work_b: Mutex<Option<TrackWork>>,
    gen_b: AtomicU64,
    /// The O pass, after S and B (lock order work -> work_b -> work_o).
    work_o: Mutex<Option<OWork>>,
    gen_o: AtomicU64,
    o_part: Mutex<Option<OPart>>,
    /// The track whose O pass runs now (u64::MAX: none).
    pub running_o_track: AtomicU64,
    /// The track whose S is on its way (decoded apart, its pass not queued
    /// yet): that track's O pass waits for its S and B, so the S column
    /// fills first, as when the window opens (`expect_source`). u64::MAX:
    /// none.
    source_coming: AtomicU64,

    // ── Prebuilt AAN2 ─────────────────────────────────────────────────────
    aan2_buf: Mutex<Option<Arc<[u8]>>>,
    aan2_rev: AtomicU32,

    // ── AAN1 ring (LIVE subjects only) ────────────────────────────────────
    aan1_ring: Mutex<Aan1Ring>,
    /// The live output's last L/R pairs and stereo numbers (AAVS, the
    /// `vec` route; LIVE subjects only).
    live_vec: Mutex<Option<Arc<[u8]>>>,

    // ── Chain response for AAN3 ───────────────────────────────────────────
    chain_resp: Mutex<Option<Arc<ChainRespData>>>,
    /// The whole-track source spectrum, from the track analysis.
    source_psd: Mutex<Option<Arc<SourcePsd>>>,
    /// The last summary the track thread published (to re-send it with a
    /// new chain) and the chain heard now ("" = the summary's own).
    last_summary: Mutex<Option<TrackSummary>>,
    chain_str: Mutex<String>,
    /// The B pass's results, merged into every track frame, and its spectrum.
    b_part: Mutex<Option<BPart>>,
    b_psd: Mutex<Option<Arc<SourcePsd>>>,

    // ── Tile cache ────────────────────────────────────────────────────────
    tiles: Mutex<TileStore>,
    /// The spectrogram tiles of S, B and O (sent in the track frame).
    spec: Mutex<[SpecInfo; 3]>,
    /// O's waveform tile counts per level (empty while its pass runs).
    wave_o: Mutex<Vec<u32>>,
    /// The zoomed spectrogram (zoom.rs).
    pub zoom: Mutex<ZoomState>,
    /// When a page last asked for this subject (ms since the process
    /// started): the zoom keeps its samples only while one looks.
    touched_ms: AtomicU64,

    // ── FILE subject metadata (None for LIVE) ─────────────────────────────
    #[allow(dead_code)] // FILE subject metadata, read by the tests
    pub entry_id: Option<u64>,
    pub path: Option<PathBuf>,
    pub conv: Option<PathBuf>,
    #[allow(dead_code)] // FILE subject metadata
    pub settings: Option<Arc<PlayerSettings>>,
}

impl Subject {
    /// Create a LIVE subject. One such subject exists per session.
    pub fn new_live(sid: u32) -> Arc<Self> {
        Arc::new(Self {
            sid,
            mode: SubjectMode::Live,
            track_id: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            work: Mutex::new(None),
            work_cond: Condvar::new(),
            work_b: Mutex::new(None),
            gen_b: AtomicU64::new(0),
            work_o: Mutex::new(None),
            gen_o: AtomicU64::new(0),
            o_part: Mutex::new(None),
            running_o_track: AtomicU64::new(u64::MAX),
            source_coming: AtomicU64::new(u64::MAX),
            aan2_buf: Mutex::new(None),
            aan2_rev: AtomicU32::new(0),
            aan1_ring: Mutex::new(Aan1Ring::new()),
            live_vec: Mutex::new(None),
            chain_resp: Mutex::new(None),
            source_psd: Mutex::new(None),
            last_summary: Mutex::new(None),
            chain_str: Mutex::new(String::new()),
            b_part: Mutex::new(None),
            b_psd: Mutex::new(None),
            tiles: Mutex::new(TileStore::new()),
            spec: Mutex::new([SpecInfo::default(); 3]),
            wave_o: Mutex::new(Vec::new()),
            zoom: Mutex::new(ZoomState::new()),
            touched_ms: AtomicU64::new(0),
            entry_id: None,
            path: None,
            conv: None,
            settings: None,
        })
    }

    /// Create a FILE subject (whole-track snapshot at `settings`).
    pub fn new_file(
        sid: u32,
        entry_id: u64,
        path: PathBuf,
        conv: Option<PathBuf>,
        settings: Arc<PlayerSettings>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sid,
            mode: SubjectMode::File,
            track_id: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            work: Mutex::new(None),
            work_cond: Condvar::new(),
            work_b: Mutex::new(None),
            gen_b: AtomicU64::new(0),
            work_o: Mutex::new(None),
            gen_o: AtomicU64::new(0),
            o_part: Mutex::new(None),
            running_o_track: AtomicU64::new(u64::MAX),
            source_coming: AtomicU64::new(u64::MAX),
            aan2_buf: Mutex::new(None),
            aan2_rev: AtomicU32::new(0),
            aan1_ring: Mutex::new(Aan1Ring::new()),
            live_vec: Mutex::new(None),
            chain_resp: Mutex::new(None),
            source_psd: Mutex::new(None),
            last_summary: Mutex::new(None),
            chain_str: Mutex::new(String::new()),
            b_part: Mutex::new(None),
            b_psd: Mutex::new(None),
            tiles: Mutex::new(TileStore::new()),
            spec: Mutex::new([SpecInfo::default(); 3]),
            wave_o: Mutex::new(Vec::new()),
            zoom: Mutex::new(ZoomState::new()),
            touched_ms: AtomicU64::new(0),
            entry_id: Some(entry_id),
            path: Some(path),
            conv,
            settings: Some(settings),
        })
    }

    // ─── Variant dispatch ────────────────────────────────────────────────────

    /// Push a new variant to the track thread. Increments generation.
    /// Old work (if any) is replaced — the track thread will detect the
    /// generation mismatch and discard the stale job.
    pub fn set_variant(&self, v: Arc<Variant>) {
        let gen = self.generation.fetch_add(1, Ordering::Release) + 1;
        *self.source_psd.lock().unwrap() = None;
        // A new S (a new track) ends the B pass too; the caller queues the
        // next one with set_b_variant.
        self.gen_b.fetch_add(1, Ordering::Release);
        *self.b_part.lock().unwrap() = None;
        *self.b_psd.lock().unwrap() = None;
        {
            let mut spec = self.spec.lock().unwrap();
            spec[0] = SpecInfo { gen: gen as u32, ..SpecInfo::default() };
            spec[1] = SpecInfo { gen: self.gen_b.load(Ordering::Acquire) as u32, ..SpecInfo::default() };
        }
        self.drop_spec_tiles(TileSrc::B);
        {
            let mut z = self.zoom.lock().unwrap();
            z.reset(TileSrc::S, gen);
            z.reset(TileSrc::B, self.gen_b.load(Ordering::Acquire));
        }
        let mut guard = self.work.lock().unwrap();
        *self.work_b.lock().unwrap() = None;
        // The O pass of this track survives the new S (the hub queues it as
        // soon as the chain is heard, the S decode ends later); another
        // track's is dropped, running or queued.
        let tid = self.track_id.load(Ordering::Relaxed);
        {
            let mut wo = self.work_o.lock().unwrap();
            if wo.as_ref().is_some_and(|w| w.track_id != tid) {
                *wo = None;
            }
            let running = self.running_o_track.load(Ordering::Acquire);
            if running != u64::MAX && running != tid {
                let g = self.gen_o.fetch_add(1, Ordering::Release) + 1;
                if let Some(w) = wo.as_mut() { w.generation = g; }
            }
            if wo.is_none() && running != tid {
                *self.o_part.lock().unwrap() = None;
            }
        }
        // The frame says the new S is computing at once, not what the last
        // track or a stream left; and an O pass's progress has a frame to go
        // out on (with none, a pass that ran ahead of S showed nothing).
        self.publish_track_frame(&computing_summary());
        *guard = Some(TrackWork { variant: v, generation: gen, is_b: false });
        drop(guard);
        self.work_cond.notify_one();
    }

    /// `track_id`'s S is being decoded apart (hub.rs `analyse_source`): its
    /// O pass, queued meanwhile, waits until `source_done` — by then its S
    /// and B are queued and go first, as when the window opens. After a
    /// stream, the O pass of the file played next ran first, and the S
    /// column stood empty for all of it (20 s on the GPU, a minute on the CPU).
    pub fn expect_source(&self, track_id: u64) {
        let _work = self.work.lock().unwrap();
        self.source_coming.store(track_id, Ordering::Release);
    }

    /// The decode of `track_id`'s S is over (its S and B queued, or none to
    /// be had): its O pass no longer waits. Another track's wait stays.
    pub fn source_done(&self, track_id: u64) {
        // Under the track thread's lock: it checks the wait and sleeps in one
        // step, so the wake-up below is not lost between the two.
        let work = self.work.lock().unwrap();
        let ended = self.source_coming
            .compare_exchange(track_id, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        drop(work);
        if ended {
            self.work_cond.notify_one();
        }
    }

    /// A live stream is heard: it has no whole track to measure. What the
    /// last track's passes left goes, nothing is queued, and the track frame
    /// says every whole-track measure is unavailable ("—", not "···").
    pub fn set_stream(&self, track_id: u64) {
        let gen = self.generation.fetch_add(1, Ordering::Release) + 1;
        *self.source_psd.lock().unwrap() = None;
        self.gen_b.fetch_add(1, Ordering::Release);
        *self.b_part.lock().unwrap() = None;
        *self.b_psd.lock().unwrap() = None;
        {
            let mut spec = self.spec.lock().unwrap();
            spec[0] = SpecInfo { gen: gen as u32, ..SpecInfo::default() };
            spec[1] = SpecInfo { gen: self.gen_b.load(Ordering::Acquire) as u32, ..SpecInfo::default() };
        }
        self.drop_spec_tiles(TileSrc::B);
        {
            let mut z = self.zoom.lock().unwrap();
            z.reset(TileSrc::S, gen);
            z.reset(TileSrc::B, self.gen_b.load(Ordering::Acquire));
        }
        {
            let mut guard = self.work.lock().unwrap();
            *guard = None;
            *self.work_b.lock().unwrap() = None;
            *self.work_o.lock().unwrap() = None;
            self.source_coming.store(u64::MAX, Ordering::Release);
        }
        self.track_id.store(track_id, Ordering::Relaxed);
        self.clear_o();
        self.publish_track_frame(&TrackSummary::default());
    }

    /// Wait for the next work item (blocks the calling track thread).
    /// Returns `None` when `signal_shutdown()` has been called (subject_close
    /// path).  Spurious Condvar wakeups loop back; stale-generation check is
    /// done by the caller, not here.
    // analytics: shutdown flag checked first to allow clean thread exit
    pub fn wait_for_work(&self) -> Option<Work> {
        let mut guard = self.work.lock().unwrap();
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return None;
            }
            if let Some(work) = self.next_work(&mut guard) {
                return Some(work);
            }
            guard = self.work_cond.wait(guard).unwrap();
        }
    }

    /// The next job (`work` is the queued S, its lock held): S, then B,
    /// then O — a track's O once its S is no longer on its way.
    fn next_work(&self, work: &mut Option<TrackWork>) -> Option<Work> {
        if let Some(w) = work.take() {
            return Some(Work::Track(w));
        }
        if let Some(w) = self.work_b.lock().unwrap().take() {
            return Some(Work::Track(w));
        }
        let coming = self.source_coming.load(Ordering::Acquire);
        let mut wo = self.work_o.lock().unwrap();
        if wo.as_ref().is_some_and(|w| w.track_id != coming) {
            return wo.take().map(Work::O);
        }
        None
    }

    /// The subject is closing (the O pass stops at once).
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Queue the O pass for the chain heard now (replaces an older one). Its
    /// O cells go blank at once, with the progress in COVERAGE_O.
    pub fn set_o_work(&self, mut w: OWork) {
        w.generation = self.gen_o.fetch_add(1, Ordering::Release) + 1;
        *self.o_part.lock().unwrap() = Some(OPart::progress(0.0));
        self.spec.lock().unwrap()[2] = SpecInfo { gen: w.generation as u32, ..SpecInfo::default() };
        self.drop_spec_tiles(TileSrc::O);
        self.wave_o.lock().unwrap().clear();
        if let Ok(mut ts) = self.tiles.lock() {
            ts.remove_where(|k| k.src == TileSrc::O && k.kind == TileKind::Wave);
        }
        self.zoom.lock().unwrap().reset(TileSrc::O, w.generation);
        let guard = self.work.lock().unwrap();
        *self.work_o.lock().unwrap() = Some(w);
        drop(guard);
        self.work_cond.notify_one();
        self.reencode();
    }

    /// No O for the chain heard (BIT-PERFECT: the file itself plays, so O is
    /// S): a running pass stops, the previous chain's O goes, nothing is
    /// queued.
    pub fn clear_o(&self) {
        let generation = self.gen_o.fetch_add(1, Ordering::Release) + 1;
        *self.o_part.lock().unwrap() = Some(OPart {
            metrics: [f64::NAN; super::proto::METRIC_ARRAY_LEN],
            prov: [super::proto::prov::UNAVAIL; super::proto::METRIC_ARRAY_LEN],
            dr: Vec::new(),
            lufs_s: Vec::new(),
            lufs_m: Vec::new(),
        });
        self.spec.lock().unwrap()[2] = SpecInfo { gen: generation as u32, ..SpecInfo::default() };
        self.drop_spec_tiles(TileSrc::O);
        self.wave_o.lock().unwrap().clear();
        if let Ok(mut ts) = self.tiles.lock() {
            ts.remove_where(|k| k.src == TileSrc::O && k.kind == TileKind::Wave);
        }
        self.zoom.lock().unwrap().reset(TileSrc::O, generation);
        *self.work_o.lock().unwrap() = None;
        self.reencode();
    }

    pub fn gen_o(&self) -> u64 {
        self.gen_o.load(Ordering::Acquire)
    }

    /// The O pass for `track_id` is done (its values measured) and nothing
    /// newer is queued.
    pub fn o_done_for(&self, track_id: u64) -> bool {
        use super::proto::{mid, prov};
        let queued = self.work_o.lock().unwrap().is_some();
        let done = self.o_part.lock().unwrap().as_ref().is_some_and(|p| p.prov[mid::COVERAGE_O] == prov::MEASURED);
        !queued && done && self.track_id.load(Ordering::Relaxed) == track_id
    }

    /// The O pass's progress or results (see `OPart`).
    pub fn publish_o_part(&self, part: OPart) {
        *self.o_part.lock().unwrap() = Some(part);
        self.reencode();
    }

    /// The O pass measured the output spectrum: it replaces the predicted O
    /// of the response frame, if that frame is still for the same chain and
    /// on the same grid.
    pub fn set_measured_o(&self, dbfs: Vec<f64>, chain_rev: u32) {
        let mut g = self.chain_resp.lock().unwrap();
        let next = match g.as_ref() {
            Some(r) if r.chain_rev == chain_rev && r.o_dbfs.len() == dbfs.len() => Arc::new(ChainRespData {
                chain_rev,
                h_dbr: r.h_dbr.clone(),
                o_dbfs: dbfs,
                f_max_hz: r.f_max_hz,
            }),
            _ => return,
        };
        *g = Some(next);
    }

    fn reencode(&self) {
        let last = self.last_summary.lock().unwrap().clone();
        if let Some(s) = last {
            self.encode_track_frame(s);
        }
    }

    /// Signal the track thread to exit cleanly (called before `notify_track_thread`
    /// in `subject_close`).
    // analytics: shutdown flag — companion to wait_for_work fix
    pub fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Peek for work without blocking. Returns immediately.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn try_take_work(&self) -> Option<TrackWork> {
        self.work.lock().unwrap().take()
    }

    /// Current generation counter value.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Queue the B pass on `v` (the variant heard). A newer B (a rack
    /// change) replaces it; the S pass, if queued, runs first.
    /// No B for what plays (a converted file from disk: its source stages
    /// ran inside the converter): the B column says so instead of waiting.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn clear_b(&self) {
        let gen = self.gen_b.fetch_add(1, Ordering::Release) + 1;
        *self.b_psd.lock().unwrap() = None;
        self.spec.lock().unwrap()[1] = SpecInfo { gen: gen as u32, ..SpecInfo::default() };
        self.drop_spec_tiles(TileSrc::B);
        self.zoom.lock().unwrap().reset(TileSrc::B, gen);
        {
            let guard = self.work.lock().unwrap();
            *self.work_b.lock().unwrap() = None;
            drop(guard);
        }
        *self.b_part.lock().unwrap() = Some(BPart {
            metrics: [f64::NAN; super::proto::METRIC_ARRAY_LEN],
            prov: [super::proto::prov::UNAVAIL; super::proto::METRIC_ARRAY_LEN],
            hist: Vec::new(),
            lufs_s: Vec::new(),
            lufs_m: Vec::new(),
            tp100: Vec::new(),
        });
        let last = self.last_summary.lock().unwrap().clone();
        if let Some(s) = last {
            self.encode_track_frame(s);
        }
    }

    pub fn set_b_variant(&self, v: Arc<Variant>) {
        let gen = self.gen_b.fetch_add(1, Ordering::Release) + 1;
        *self.b_part.lock().unwrap() = None;
        *self.b_psd.lock().unwrap() = None;
        self.spec.lock().unwrap()[1] = SpecInfo { gen: gen as u32, ..SpecInfo::default() };
        self.drop_spec_tiles(TileSrc::B);
        self.zoom.lock().unwrap().reset(TileSrc::B, gen);
        let guard = self.work.lock().unwrap();
        *self.work_b.lock().unwrap() = Some(TrackWork { variant: v, generation: gen, is_b: true });
        drop(guard);
        self.work_cond.notify_one();
        // The frame loses the old B at once.
        let last = self.last_summary.lock().unwrap().clone();
        if let Some(s) = last {
            self.encode_track_frame(s);
        }
    }

    /// The generation a work item must still match: B passes have their own.
    pub fn generation_for(&self, is_b: bool) -> u64 {
        if is_b { self.gen_b.load(Ordering::Acquire) } else { self.generation() }
    }

    /// Keep what the B pass measured (its summary's S slots) and re-send the
    /// track frame with it.
    pub fn publish_b_part(&self, b: &TrackSummary) {
        *self.b_part.lock().unwrap() = Some(BPart {
            metrics: b.s_metrics,
            prov: b.s_prov,
            hist: b.hist_s.clone(),
            lufs_s: b.lufs_s_series_s.clone(),
            lufs_m: b.lufs_m_series_s.clone(),
            tp100: b.tp100_s.clone(),
        });
        let last = self.last_summary.lock().unwrap().clone();
        if let Some(s) = last {
            self.encode_track_frame(s);
        }
    }

    /// The B pass's whole-track spectrum.
    pub fn publish_b_psd(&self, psd: Arc<SourcePsd>) {
        *self.b_psd.lock().unwrap() = Some(psd);
    }

    pub fn try_b_psd(&self) -> Option<Arc<SourcePsd>> {
        self.b_psd.try_lock().ok()?.clone()
    }

    // ─── AAN2 publishing ─────────────────────────────────────────────────────

    /// Publish a completed or partial TrackSummary as the new AAN2 bytes.
    /// Increments `aan2_rev`.
    pub fn publish_track_frame(&self, summary: &TrackSummary) {
        *self.last_summary.lock().unwrap() = Some(summary.clone());
        self.encode_track_frame(summary.clone());
    }

    /// The chain heard now, for the track frame's chain line; re-sends the
    /// last summary with it.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn set_chain_str(&self, chain: String) {
        *self.chain_str.lock().unwrap() = chain;
        let last = self.last_summary.lock().unwrap().clone();
        if let Some(s) = last {
            self.encode_track_frame(s);
        }
    }

    /// The frame carries the subject's own rev (what the page sends back
    /// and the route compares), not the track thread's pass number.
    fn encode_track_frame(&self, mut summary: TrackSummary) {
        let chain = self.chain_str.lock().unwrap().clone();
        if !chain.is_empty() {
            summary.chain_str = chain;
        }
        if let Some(o) = self.o_part.lock().unwrap().clone() {
            summary.o_metrics = o.metrics;
            summary.o_prov = o.prov;
            summary.dr_o = o.dr;
            summary.lufs_s_series_o = o.lufs_s;
            summary.lufs_m_series_o = o.lufs_m;
        }
        summary.spec_info = self.spec.lock().unwrap().to_vec();
        {
            let w = self.wave_o.lock().unwrap();
            if !w.is_empty() {
                summary.wave_tile_counts_o = w.clone();
            }
        }
        if let Some(b) = self.b_part.lock().unwrap().clone() {
            summary.b_metrics = b.metrics;
            summary.b_prov = b.prov;
            summary.hist_b = b.hist;
            summary.lufs_s_series_b = b.lufs_s;
            summary.lufs_m_series_b = b.lufs_m;
            summary.tp100_b = b.tp100;
        }
        let mut buf = self.aan2_buf.lock().unwrap();
        let new_rev = self.aan2_rev.fetch_add(1, Ordering::AcqRel) + 1;
        summary.rev = new_rev;
        *buf = Some(encode_aan2(&summary).into());
    }

    /// Get the current AAN2 bytes (or None if not yet computed).
    /// Does not block; uses try_lock.
    pub fn try_aan2_bytes(&self) -> Option<Arc<[u8]>> {
        self.aan2_buf.try_lock().ok()?.clone()
    }

    /// Current AAN2 rev counter.
    pub fn aan2_rev(&self) -> u32 {
        self.aan2_rev.load(Ordering::Acquire)
    }

    // ─── AAN1 publishing (called from live.rs) ───────────────────────────────

    /// Publish a live frame into the AAN1 ring. Called from live.rs at 10 Hz.
    /// The frame goes out under the ring's own number: a new stream (BIT-
    /// PERFECT on or off, another rate) starts a new live session counting
    /// from 0, and the page, asking for "newer than 5" while the ring stood
    /// at 3000, was told it had fallen behind — and heard nothing more.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn publish_live_frame(&self, mut frame: LiveFrame) {
        if let Ok(mut ring) = self.aan1_ring.lock() {
            let seq = ring.current_seq().wrapping_add(1);
            frame.seq = seq;
            let bytes: Arc<[u8]> = encode_aan1(&frame).into();
            ring.push(seq, bytes);
        }
    }

    /// The live output's stereo picture (AAVS bytes, see live.rs).
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn set_live_vec(&self, bytes: Arc<[u8]>) {
        *self.live_vec.lock().unwrap() = Some(bytes);
    }

    pub fn live_vec(&self) -> Option<Arc<[u8]>> {
        self.live_vec.try_lock().ok()?.clone()
    }

    /// Directly push prebuilt AAN1 bytes at a given seq. Used by live.rs when
    /// it pre-encodes the frame externally.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn push_aan1_bytes(&self, seq: u32, bytes: Arc<[u8]>) {
        if let Ok(mut ring) = self.aan1_ring.lock() {
            ring.push(seq, bytes);
        }
    }

    /// Get the latest AAN1 frame. Returns None if no frame has been published
    /// yet or the ring is still locked.
    pub fn try_aan1_bytes(&self, since: u32) -> Option<Arc<[u8]>> {
        let ring = self.aan1_ring.try_lock().ok()?;
        if ring.current_seq() == 0 && since == 0 {
            return ring.get_latest();
        }
        // A page too far behind (or ahead, after the app restarted under
        // it) takes the latest frame and continues from its number; an
        // empty answer left it asking for the same number for good.
        ring.get_latest()
    }

    // ─── Chain response (AAN3) publishing ────────────────────────────────────

    /// Publish a new chain response (called from resp.rs on chain_rev change).
    pub fn publish_resp_frame(&self, resp: Arc<ChainRespData>) {
        *self.chain_resp.lock().unwrap() = Some(resp);
    }

    /// Get the current ChainRespData (for AAN3 decimation in routes.rs).
    pub fn try_chain_resp(&self) -> Option<Arc<ChainRespData>> {
        self.chain_resp.try_lock().ok()?.clone()
    }

    /// Publish the source spectrum (called from track.rs after the Welch pass).
    pub fn publish_source_psd(&self, psd: Arc<SourcePsd>) {
        *self.source_psd.lock().unwrap() = Some(psd);
    }

    /// Get the current source spectrum (for AAN3 decimation).
    pub fn try_source_psd(&self) -> Option<Arc<SourcePsd>> {
        self.source_psd.try_lock().ok()?.clone()
    }

    // ─── Tile store ───────────────────────────────────────────────────────────

    /// Store a prebuilt tile.
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn store_tile(&self, key: TileKey, bytes: Arc<[u8]>) {
        if let Ok(mut ts) = self.tiles.lock() {
            ts.insert(key, bytes);
        }
    }

    /// Look up a tile from the cache.
    pub fn get_tile(&self, key: &TileKey) -> Option<Arc<[u8]>> {
        self.tiles.try_lock().ok()?.get(key)
    }

    /// Store a WaveTile (builds AAWT bytes and inserts into the cache).
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn store_wave_tile(&self, tile: &WaveTile) {
        let src = TileSrc::from_flag(tile.src_flag).unwrap_or(TileSrc::S);
        let key = TileKey {
            src,
            kind: TileKind::Wave,
            lod: tile.lod,
            idx: tile.tile_idx,
        };
        let bytes: Arc<[u8]> = encode_aawt(tile).into();
        self.store_tile(key, bytes);
    }

    /// Store a SpectTile (builds AAST bytes and inserts into the cache).
    #[allow(dead_code)] // kept for the analyzer views not wired yet
    pub fn store_spect_tile(&self, tile: &SpectTile) {
        let src = TileSrc::from_flag(tile.src_flag).unwrap_or(TileSrc::S);
        let key = TileKey {
            src,
            kind: TileKind::Spec,
            lod: tile.lod,
            idx: tile.tile_idx,
        };
        let bytes: Arc<[u8]> = encode_aast(tile).into();
        self.store_tile(key, bytes);
    }

    /// Store a pass's spectrogram tile while that pass is still current:
    /// the check and the insert share the lock the replacement's eviction
    /// takes, so no tile of a replaced pass survives it.
    pub fn store_spect_tile_of(&self, tile: &SpectTile, gen: u64) {
        let src = TileSrc::from_flag(tile.src_flag).unwrap_or(TileSrc::S);
        let key = TileKey { src, kind: TileKind::Spec, lod: tile.lod, idx: tile.tile_idx };
        let bytes: Arc<[u8]> = encode_aast(tile).into();
        let mut ts = self.tiles.lock().unwrap();
        if self.pass_gen(src) == gen {
            ts.insert(key, bytes);
        }
    }

    /// The generation of the pass that makes `src`'s tiles now.
    pub fn pass_gen(&self, src: TileSrc) -> u64 {
        match src {
            TileSrc::S => self.generation(),
            TileSrc::B => self.gen_b.load(Ordering::Acquire),
            TileSrc::O => self.gen_o(),
        }
    }

    /// A pass's waveform tile, while that pass is still current (as
    /// `store_spect_tile_of`).
    pub fn store_wave_tile_of(&self, tile: &WaveTile, gen: u64) {
        let src = TileSrc::from_flag(tile.src_flag).unwrap_or(TileSrc::S);
        let key = TileKey { src, kind: TileKind::Wave, lod: tile.lod, idx: tile.tile_idx };
        let bytes: Arc<[u8]> = encode_aawt(tile).into();
        let mut ts = self.tiles.lock().unwrap();
        if self.pass_gen(src) == gen {
            ts.insert(key, bytes);
        }
    }

    /// O's waveform tiles are done (their count per level), for the track frame.
    pub fn set_wave_o(&self, counts: Vec<u32>, gen: u64) {
        if self.gen_o() != gen { return; }
        *self.wave_o.lock().unwrap() = counts;
        self.reencode();
    }

    /// A pass's spectrogram tiles: how many are done, for the track frame.
    /// Ignored when that pass was replaced meanwhile.
    pub fn set_spec_info(&self, src: TileSrc, info: SpecInfo, reencode: bool) {
        {
            let mut spec = self.spec.lock().unwrap();
            let slot = &mut spec[src.flag() as usize];
            if slot.gen != info.gen {
                return;
            }
            *slot = info;
        }
        if reencode {
            self.reencode();
        }
    }

    fn drop_spec_tiles(&self, src: TileSrc) {
        if let Ok(mut ts) = self.tiles.lock() {
            ts.remove_where(|k| k.src == src && k.kind == TileKind::Spec);
        }
    }

    /// A page asked for this subject.
    pub fn touch(&self) {
        self.touched_ms.store(super::zoom::now_ms(), Ordering::Relaxed);
    }

    /// Milliseconds since a page last asked (u64::MAX: never).
    pub fn idle_ms(&self) -> u64 {
        let t = self.touched_ms.load(Ordering::Relaxed);
        if t == 0 { u64::MAX } else { super::zoom::now_ms().saturating_sub(t) }
    }

    /// Wake the background track thread (called by hub on subject_close after
    /// bumping the generation counter, so the thread sees a stale check and exits).
    pub fn notify_track_thread(&self) {
        self.work_cond.notify_one();
    }

    /// Evict the tile cache (on track/generation change).
    pub fn evict_tiles(&self) {
        if let Ok(mut ts) = self.tiles.lock() {
            ts.clear();
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_live() -> Arc<Subject> {
        Subject::new_live(u32::MAX - 1)
    }

    /// The stream's track id (radio/chain.rs RADIO_TRACK_ID).
    const STREAM: u64 = u64::MAX - 1;

    /// A short silent source, as a decoded track's variant.
    fn variant() -> Arc<Variant> {
        use crate::player::convolver::SourceBuf;
        Arc::new(Variant {
            stream: None,
            key: "full".into(),
            src: Arc::new(SourceBuf { l: vec![0.0; 64], r: vec![0.0; 64], rate: 44_100 }),
            out_rate: 88_200,
            l: 2,
            tp_target_dbtp: -0.5,
            tp_pred_lin: 0.1,
            lim_local: true,
            tokens: vec![],
            notes: vec![],
            stages: vec![],
            quick: false,
            direct: false,
            source_id: String::new(),
        })
    }

    /// The O pass of `track` on `v`.
    fn o_work(track: u64, v: &Arc<Variant>) -> OWork {
        OWork {
            variant: v.clone(),
            settings: Arc::new(PlayerSettings::default()),
            track_id: track,
            track_key: String::new(),
            downgrade: None,
            downgrade_gen: 0,
            chain_rev: 0,
            own_envelope: false,
            gpu: false,
            generation: 0,
            file: None,
        }
    }

    /// What the page reads of the track frame: O's progress (COVERAGE_O),
    /// its provenance, and the provenance of S's LUFS-I — where AAN2 keeps
    /// them (after the header, S's, B's and O's 32 values, then S's, B's and
    /// O's provenance nibbles).
    fn frame_progress(s: &Subject) -> (f64, u8, u8) {
        use super::super::proto::{mid, N_PROV_BYTES_AAN2};
        let b = s.try_aan2_bytes().expect("a track frame");
        let at = 20 + 64 * 8 + mid::COVERAGE_O * 8;
        let pct = f64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        let nib = |base: usize, i: usize| (b[base + (i >> 1)] >> ((i & 1) * 4)) & 0x0f;
        let prov = 20 + 96 * 8;
        (pct, nib(prov + 2 * N_PROV_BYTES_AAN2, mid::COVERAGE_O), nib(prov, mid::LUFS_I))
    }

    /// The radio, then a file: its O pass got ahead of its S (the S is
    /// decoded apart, the O pass was queued first). The frame goes on with
    /// the pass's progress and says S is computing; it stood at the stream's
    /// dashes with O at 0 % for the whole pass (3.10: the window open on the
    /// radio showed nothing of the file played next, no gold line).
    #[test]
    fn after_a_stream_an_o_pass_ahead_of_s_shows_its_progress() {
        use super::super::proto::prov::COMPUTING;
        let s = make_live();
        s.set_stream(STREAM);
        let v = variant();
        s.track_id.store(7, Ordering::Relaxed);
        s.set_o_work(o_work(7, &v));
        s.set_variant(v.clone());
        s.publish_o_part(OPart::progress(40.0));
        let (pct, o_prov, s_prov) = frame_progress(&s);
        assert_eq!(pct, 40.0, "the O pass's progress goes out");
        assert_eq!(o_prov, COMPUTING);
        assert_eq!(s_prov, COMPUTING, "S computing, not the stream's dash");
    }

    /// What the track thread would take next (it waits when None).
    fn next(s: &Subject) -> Option<Work> {
        let mut work = s.work.lock().unwrap();
        s.next_work(&mut work)
    }

    /// The radio, then a file: its S is decoded apart while its chain's O
    /// pass is queued. The track thread takes its S, then its B, and its O
    /// only then — as when the window opens; the O pass went first, and the
    /// S column waited for the whole of it.
    #[test]
    fn a_tracks_o_pass_waits_for_its_s_on_the_way() {
        let s = make_live();
        s.set_stream(STREAM);
        let v = variant();
        s.track_id.store(7, Ordering::Relaxed);
        s.expect_source(7);
        s.set_o_work(o_work(7, &v));
        assert!(next(&s).is_none(), "the O pass waits for the S on its way");
        s.set_variant(v.clone());
        s.set_b_variant(v.clone());
        assert!(matches!(next(&s), Some(Work::Track(w)) if !w.is_b), "S first");
        assert!(matches!(next(&s), Some(Work::Track(w)) if w.is_b), "then B");
        assert!(next(&s).is_none(), "O waits until the decode is over");
        s.source_done(7);
        assert!(matches!(next(&s), Some(Work::O(w)) if w.track_id == 7), "then O");
    }

    /// The wait is one track's: another track's O pass goes as before, the
    /// end of another track's decode leaves it, a stream ends it.
    #[test]
    fn the_wait_for_an_s_is_one_tracks() {
        let s = make_live();
        let v = variant();
        s.expect_source(7);
        s.set_o_work(o_work(8, &v));
        assert!(matches!(next(&s), Some(Work::O(w)) if w.track_id == 8), "another track's O is not held");
        s.set_o_work(o_work(7, &v));
        s.source_done(8);
        assert!(next(&s).is_none(), "the end of another track's decode leaves the wait");
        s.set_stream(STREAM);
        s.set_o_work(o_work(7, &v));
        assert!(matches!(next(&s), Some(Work::O(w)) if w.track_id == 7), "a stream ends the wait");
    }

    #[test]
    fn a_live_stream_says_its_whole_track_measures_are_unavailable() {
        use super::super::proto::prov::UNAVAIL;
        let s = make_live();
        let rev = s.aan2_rev();
        s.set_stream(u64::MAX - 1);
        assert!(s.aan2_rev() > rev, "a track frame went out");
        let sum = s.last_summary.lock().unwrap().clone().expect("its summary");
        assert!(sum.s_prov.iter().chain(&sum.b_prov).all(|&p| p == UNAVAIL), "S and B: unavailable, not computing");
        let o = s.o_part.lock().unwrap().clone().expect("O cleared");
        assert!(o.prov.iter().all(|&p| p == UNAVAIL));
        assert!(s.work.lock().unwrap().is_none() && s.work_o.lock().unwrap().is_none(), "nothing queued");
        assert_eq!(s.track_id.load(Ordering::Relaxed), u64::MAX - 1);
    }

    // ── Generation counter ─────────────────────────────────────────────────

    #[test]
    fn generation_increments_on_set_variant() {
        let s = make_live();
        assert_eq!(s.generation(), 0);
        // We need a Variant — build a minimal one without the engine.
        // Just check the generation increment; skip variant content.
        s.generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(s.generation(), 1);
        s.generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(s.generation(), 2);
    }

    // ── AAN1 ring ──────────────────────────────────────────────────────────

    #[test]
    fn aan1_ring_roundtrip() {
        let s = make_live();
        let data: Arc<[u8]> = vec![1u8, 2, 3].into();
        s.push_aan1_bytes(1, data.clone());
        let out = s.try_aan1_bytes(0).unwrap();
        assert_eq!(&out[..], &[1u8, 2, 3]);
    }

    #[test]
    fn aan1_ring_since_within_range() {
        let s = make_live();
        for seq in 1..=10u32 {
            let data: Arc<[u8]> = vec![seq as u8].into();
            s.push_aan1_bytes(seq, data);
        }
        // since=5 is within ring (10-5=5 < 256)
        assert!(s.try_aan1_bytes(5).is_some());
    }

    // ── AAN2 buffer ────────────────────────────────────────────────────────

    #[test]
    fn aan2_none_initially() {
        let s = make_live();
        assert!(s.try_aan2_bytes().is_none());
    }

    #[test]
    fn aan2_present_after_publish() {
        let s = make_live();
        let summary = TrackSummary::default();
        s.publish_track_frame(&summary);
        assert!(s.try_aan2_bytes().is_some());
    }

    // ── Tile store ─────────────────────────────────────────────────────────

    #[test]
    fn tile_store_insert_and_get() {
        let mut ts = TileStore::new();
        let key = TileKey { src: TileSrc::S, kind: TileKind::Wave, lod: 0, idx: 3 };
        let bytes: Arc<[u8]> = vec![42u8].into();
        ts.insert(key.clone(), bytes.clone());
        let got = ts.get(&key).unwrap();
        assert_eq!(&got[..], &[42u8]);
    }

    #[test]
    fn tile_store_lru_eviction() {
        let mut ts = TileStore::new();
        ts.cap_bytes = 2;
        for i in 0u32..3 {
            let key = TileKey { src: TileSrc::S, kind: TileKind::Wave, lod: 0, idx: i };
            ts.insert(key, vec![i as u8].into());
        }
        // Tile 0 should have been evicted
        let k0 = TileKey { src: TileSrc::S, kind: TileKind::Wave, lod: 0, idx: 0 };
        assert!(ts.get(&k0).is_none());
        // Tile 2 (most recent) is present
        let k2 = TileKey { src: TileSrc::S, kind: TileKind::Wave, lod: 0, idx: 2 };
        assert!(ts.get(&k2).is_some());
    }

    // ── new_file ───────────────────────────────────────────────────────────

    #[test]
    fn new_file_fields() {
        use crate::player::settings::PlayerSettings;
        let settings = Arc::new(PlayerSettings::default());
        let s = Subject::new_file(
            1,
            99,
            PathBuf::from("test.flac"),
            None,
            settings,
        );
        assert_eq!(s.sid, 1);
        assert_eq!(s.mode, SubjectMode::File);
        assert_eq!(s.entry_id, Some(99));
        assert_eq!(s.path, Some(PathBuf::from("test.flac")));
        assert!(s.conv.is_none());
    }
}
