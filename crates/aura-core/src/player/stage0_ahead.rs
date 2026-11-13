//! Stage 0 ahead: a file track's long-filter blocks (`PolyStream`'s
//! multiply-accumulate, the expensive part of a CPU chain) computed ahead of
//! the playhead on a low-priority worker, into RAM. The render then only
//! copies a block out; one that is not there yet it sums itself, as always.
//!
//! Bit for bit: a block's output depends only on its index (the sum starts
//! from zero every block over the ring of input spectra, and the ring is the
//! input's), so the worker drives its own `PolyStream` over the same bank and
//! input, block after block, and keeps each finished block as it is.
//!
//! The worker serves the track read most recently; when that one is done, a
//! track attached but not read yet (the next track's chain, built ahead). At
//! most two inputs are kept (the current track and the next); the blocks of
//! both stay under a RAM cap (a quarter of the RAM, at most 2 GB), past it the
//! rest of the track streams as before. Off by default (a whole track
//! computed ahead loads the CPU for a song that may be skipped; streaming
//! spends it only as the music plays): on with `AURA_STAGE0_AHEAD=1`, and
//! never with `AURA_FIR_OPT=0` (the old work); a live stream, a head-and-tail
//! stream and the GPU never take part.

use super::convolver::{Bank, Input, PolyStream, BLOCK};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Banks shorter than this many partitions stream as always (the blocks
/// cost little; the same bar as the `[firperf]` lines).
const MIN_PARTITIONS: usize = 16;

/// Inputs kept: the current track and the next.
const KEEP_INPUTS: usize = 2;

/// One block's aligned output, as `PolyStream::compute_block` leaves it.
pub struct Block {
    pub l: Vec<f64>,
    pub r: Vec<f64>,
}

impl Block {
    fn bytes(&self) -> usize {
        (self.l.len() + self.r.len()) * std::mem::size_of::<f64>()
    }
}

/// The RAM the blocks of every track may hold together.
pub struct Budget {
    cap: usize,
    used: AtomicUsize,
}

impl Budget {
    pub fn new(cap: usize) -> Arc<Budget> {
        Arc::new(Budget { cap, used: AtomicUsize::new(0) })
    }

    fn take(&self, n: usize) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            if cur + n > self.cap {
                return false;
            }
            match self.used.compare_exchange_weak(cur, cur + n, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    fn give(&self, n: usize) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }
}

/// A quarter of the RAM, at most 2 GB.
fn ram_cap() -> usize {
    let quarter_mb = crate::audio::memory::total_ram_mb() / 4;
    (quarter_mb.min(2048) as usize) << 20
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static READ_TICK: AtomicU64 = AtomicU64::new(1);

/// One bank over one input: the blocks computed ahead and what the readers
/// took.
pub struct Track {
    id: u64,
    bank: Arc<Bank>,
    input: Input,
    /// Blocks the track plays, first to last.
    first: i64,
    last: i64,
    blocks: Mutex<HashMap<i64, Arc<Block>>>,
    budget: Arc<Budget>,
    bytes: AtomicUsize,
    /// The block a reader needs next, and when a reader last took one
    /// (`READ_TICK`; 0 = never).
    want: AtomicI64,
    read_at: AtomicU64,
    /// Nothing left to compute from `want` on (or the RAM cap reached).
    done: AtomicBool,
    capped: AtomicBool,
    computed: AtomicU64,
    compute_ns: AtomicU64,
    served: AtomicU64,
    inline: AtomicU64,
    started: AtomicBool,
    dropped: AtomicBool,
}

impl Track {
    /// A track over `input` (a file or a growing file, whose length is
    /// known); none for a live stream.
    pub fn new(bank: Arc<Bank>, input: Input, budget: Arc<Budget>) -> Option<Arc<Track>> {
        if matches!(input, Input::Live(_)) {
            return None;
        }
        let total = bank.stream_total(&input);
        if total == 0 || total == u64::MAX {
            return None;
        }
        let per = (BLOCK * bank.l) as u64;
        let d = bank.delay as u64;
        let first = (d / per) as i64;
        let last = ((total - 1 + d) / per) as i64;
        Some(Arc::new(Track {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            bank,
            input,
            first,
            last,
            blocks: Mutex::new(HashMap::new()),
            budget,
            bytes: AtomicUsize::new(0),
            want: AtomicI64::new(first),
            read_at: AtomicU64::new(0),
            done: AtomicBool::new(false),
            capped: AtomicBool::new(false),
            computed: AtomicU64::new(0),
            compute_ns: AtomicU64::new(0),
            served: AtomicU64::new(0),
            inline: AtomicU64::new(0),
            started: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        }))
    }

    /// Block `j` when it was computed ahead; a reader needs `j + 1` next.
    pub fn take(&self, j: i64) -> Option<Arc<Block>> {
        self.want.store(j + 1, Ordering::Relaxed);
        self.read_at.store(READ_TICK.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        let got = self.blocks.lock().unwrap_or_else(|e| e.into_inner()).get(&j).cloned();
        match &got {
            Some(_) => {
                self.served.fetch_add(1, Ordering::Relaxed);
                super::firperf::count_served();
            }
            None => {
                self.inline.fetch_add(1, Ordering::Relaxed);
                // A seek to blocks not computed: the worker goes on from there
                // (not for a block past the track, read at its end).
                if (self.first..=self.last).contains(&j)
                    && !self.capped.load(Ordering::Relaxed)
                    && self.done.swap(false, Ordering::AcqRel)
                {
                    wake();
                }
            }
        }
        got
    }

    /// The first block from the reader's on that is not computed yet.
    fn next_target(&self) -> Option<i64> {
        let from = self.want.load(Ordering::Relaxed).max(self.first);
        let g = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        (from..=self.last).find(|j| !g.contains_key(j))
    }

    /// Compute block `j` on the worker's stream `ws` (a new one when it is
    /// not at `j`) and keep it. False when the RAM cap stops the track.
    fn fill_one(&self, ws: &mut Option<(u64, PolyStream)>, j: i64) -> bool {
        if !self.started.swap(true, Ordering::Relaxed) {
            crate::aelog!(
                "[firperf] ahead start track={} filter={} L={} blocks={} from={} cap_mb={}",
                self.id,
                file_name(&self.bank.path),
                self.bank.l,
                self.last - self.first + 1,
                j - self.first,
                self.budget.cap >> 20
            );
        }
        let t0 = Instant::now();
        let fresh = !matches!(ws, Some((id, s)) if *id == self.id && s.next_block_index() == j);
        if fresh {
            *ws = Some((self.id, PolyStream::for_ahead(self.bank.clone(), self.input.clone(), j)));
        }
        let (_, s) = ws.as_mut().expect("the worker's stream");
        let (jb, l, r) = s.next_whole_block();
        debug_assert_eq!(jb, j);
        let b = Block { l, r };
        let n = b.bytes();
        self.compute_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if self.dropped.load(Ordering::Acquire) || !self.budget.take(n) {
            if !self.dropped.load(Ordering::Acquire) {
                self.capped.store(true, Ordering::Relaxed);
                self.finish("capped");
            }
            return false;
        }
        self.bytes.fetch_add(n, Ordering::Relaxed);
        self.computed.fetch_add(1, Ordering::Relaxed);
        self.blocks.lock().unwrap_or_else(|e| e.into_inner()).insert(j, Arc::new(b));
        true
    }

    /// Compute the blocks `from..=to` in order (the worker's loop, by hand:
    /// the tests). Stops at the RAM cap.
    #[cfg(test)]
    pub fn fill(&self, from: i64, to: i64) {
        let mut ws = None;
        for j in from.max(self.first)..=to.min(self.last) {
            if !self.fill_one(&mut ws, j) {
                break;
            }
        }
    }

    /// Blocks computed and kept.
    #[cfg(test)]
    pub fn kept(&self) -> usize {
        self.blocks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// (served, computed in line) so far.
    pub fn counts(&self) -> (u64, u64) {
        (self.served.load(Ordering::Relaxed), self.inline.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    pub fn range(&self) -> (i64, i64) {
        (self.first, self.last)
    }

    #[cfg(test)]
    pub fn is_capped(&self) -> bool {
        self.capped.load(Ordering::Relaxed)
    }

    fn finish(&self, why: &str) {
        if self.done.swap(true, Ordering::AcqRel) {
            return;
        }
        let blocks = self.computed.load(Ordering::Relaxed);
        let music = blocks as f64 * BLOCK as f64 / self.input.rate().max(1) as f64;
        let wall = self.compute_ns.load(Ordering::Relaxed) as f64 / 1e9;
        crate::aelog!(
            "[firperf] ahead end track={} {} blocks={} music_s={:.1} wall_s={:.2} x_rt={:.1} mb={}",
            self.id,
            why,
            blocks,
            music,
            wall,
            if wall > 0.0 { music / wall } else { 0.0 },
            self.bytes.load(Ordering::Relaxed) >> 20
        );
    }

    /// Let the blocks go (the track is neither current nor next any more).
    fn drop_blocks(&self) {
        self.dropped.store(true, Ordering::Release);
        self.done.store(true, Ordering::Release);
        let mut g = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        g.clear();
        self.budget.give(self.bytes.swap(0, Ordering::AcqRel));
    }

    fn same(&self, bank: &Arc<Bank>, input: &Input) -> bool {
        Arc::ptr_eq(&self.bank, bank) && same_input(&self.input, input)
    }
}

impl Track {
    /// The `[firperf] ahead use` line: blocks served and summed in line so
    /// far (a reader stream's end, and the track's).
    pub fn log_use(&self) {
        let (served, inline) = self.counts();
        if served + inline > 0 {
            crate::aelog!("[firperf] ahead use track={} served={} inline={}", self.id, served, inline);
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        self.log_use();
        self.budget.give(self.bytes.swap(0, Ordering::AcqRel));
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn same_input(a: &Input, b: &Input) -> bool {
    match (a, b) {
        (Input::File(x), Input::File(y)) => Arc::ptr_eq(x, y),
        (Input::Grow(x), Input::Grow(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

// ── The switch and the scope ────────────────────────────────────────────────

thread_local! {
    /// A chain for playback is being built on this thread (`scope`).
    static PLAYING: Cell<bool> = const { Cell::new(false) };
    /// Tests: ahead only where a test asks for it.
    #[cfg(test)]
    static TEST_ON: Cell<bool> = const { Cell::new(false) };
}

/// Only with `AURA_STAGE0_AHEAD=1`, and not with `AURA_FIR_OPT=0`.
pub fn enabled() -> bool {
    // Tests: the test's switch stands for the variable.
    #[cfg(test)]
    let asked = TEST_ON.with(|c| c.get());
    #[cfg(not(test))]
    let asked = std::env::var("AURA_STAGE0_AHEAD").is_ok_and(|v| v.trim() == "1");
    asked && crate::audio::dsp_core::fir_opt()
}

/// Ahead on this test's thread.
#[cfg(test)]
pub fn on_for_test(on: bool) {
    TEST_ON.with(|c| c.set(on));
}

/// While it lives, file streams built on this thread are for playback.
pub struct Scope(bool);

pub fn scope(on: bool) -> Scope {
    Scope(PLAYING.with(|c| c.replace(on)))
}

impl Drop for Scope {
    fn drop(&mut self) {
        PLAYING.with(|c| c.set(self.0));
    }
}

// ── The registry and the worker ─────────────────────────────────────────────

struct Reg {
    tracks: Vec<Arc<Track>>,
    budget: Arc<Budget>,
}

fn reg() -> &'static (Mutex<Reg>, Condvar) {
    static REG: OnceLock<(Mutex<Reg>, Condvar)> = OnceLock::new();
    REG.get_or_init(|| (Mutex::new(Reg { tracks: Vec::new(), budget: Budget::new(ram_cap()) }), Condvar::new()))
}

fn wake() {
    reg().1.notify_all();
}

/// The track of a stream about to play `bank` over `input`: the one kept, or
/// a new one the worker fills. None when ahead is off, outside a playback
/// build, for a live stream or a short bank.
pub fn attach(bank: &Arc<Bank>, input: &Input) -> Option<Arc<Track>> {
    if !PLAYING.with(|c| c.get()) || !enabled() || bank.max_partitions() < MIN_PARTITIONS {
        return None;
    }
    let (lock, _) = reg();
    let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
    let t = match g.tracks.iter().position(|t| t.same(bank, input)) {
        Some(i) => g.tracks.remove(i),
        None => Track::new(bank.clone(), input.clone(), g.budget.clone())?,
    };
    // Most recent last. Another bank over this input nobody reads (a rung
    // left behind) goes; so do inputs past the current and the next.
    g.tracks.retain(|o| {
        let keep = !(same_input(&o.input, input) && Arc::strong_count(o) == 1);
        if !keep {
            o.drop_blocks();
        }
        keep
    });
    g.tracks.push(t.clone());
    let mut inputs: Vec<Input> = Vec::new();
    for o in g.tracks.iter().rev() {
        if !inputs.iter().any(|i| same_input(i, &o.input)) {
            inputs.push(o.input.clone());
        }
    }
    if inputs.len() > KEEP_INPUTS {
        let keep: Vec<Input> = inputs.into_iter().take(KEEP_INPUTS).collect();
        g.tracks.retain(|o| {
            let k = keep.iter().any(|i| same_input(i, &o.input));
            if !k {
                o.drop_blocks();
            }
            k
        });
    }
    drop(g);
    start_worker();
    wake();
    Some(t)
}

fn start_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("aura-stage0-ahead".into()).spawn(worker);
    });
}

/// The track to work on: the one read most recently with blocks left; else
/// one attached and not read yet (the next track), oldest first. Only
/// tracks some stream still holds.
fn pick(tracks: &[Arc<Track>]) -> Option<Arc<Track>> {
    let live = tracks.iter().filter(|t| Arc::strong_count(t) > 1 && !t.done.load(Ordering::Acquire));
    let read = live.clone().filter(|t| t.read_at.load(Ordering::Relaxed) > 0).max_by_key(|t| t.read_at.load(Ordering::Relaxed));
    read.or_else(|| live.min_by_key(|t| t.id)).cloned()
}

fn worker() {
    super::source_stages::set_thread_priority(-1);
    let n = std::thread::available_parallelism().map_or(4, |x| x.get());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads((n / 2).clamp(2, 8))
        .thread_name(|i| format!("aura-stage0-ahead-{i}"))
        .start_handler(|_| super::source_stages::set_thread_priority(-1))
        .build()
        .ok();
    let mut ws: Option<(u64, PolyStream)> = None;
    loop {
        let job = {
            let (lock, cv) = reg();
            let g = lock.lock().unwrap_or_else(|e| e.into_inner());
            match pick(&g.tracks) {
                Some(t) => t,
                None => {
                    let _ = cv.wait_timeout(g, Duration::from_secs(1));
                    ws = None;
                    continue;
                }
            }
        };
        let Some(j) = job.next_target() else {
            job.finish("done");
            ws = None;
            continue;
        };
        match &pool {
            Some(p) => p.install(|| job.fill_one(&mut ws, j)),
            None => job.fill_one(&mut ws, j),
        };
    }
}
