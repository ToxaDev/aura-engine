//! The zoomed spectrogram: finer tiles for the stretch the page shows.
//!
//! The whole-track tiles (track.rs, opass.rs) have a column per `spec_hop`
//! source samples, ~15 ms on a four-minute track. Zoomed in closer, the page
//! asks for tiles at a hop of 2^lod source samples over the stretch it shows
//! (the `specz` route), for S, B or O: the same grid for all three (O's hop is
//! 2^lod × L output samples), computed with the same multi-resolution engine,
//! so they only add detail and the page can take one from another (the Δ
//! views).
//!
//! The signals come from the passes: S as f32 (exact for 16/24-bit and float
//! files; decoded again when it was not kept), B as the variant the player
//! holds, O as f32 captured on its way through the O pass. They are kept
//! only while a page looks at the subject, and within a memory cap.
//!
//! The route only records what the page asks for and returns what is ready;
//! one worker thread computes, the middle of the stretch first, and starts
//! over as soon as the page asks for another stretch.
//!
//! The same signals give a selection's statistics (the `selstat` route): the
//! whole-track metrics measured over a stretch with the O pass's own meters,
//! and its spectrum, on a thread of their own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::proto::{
    decimate_pairs, encode_aasl, encode_aast, encode_aazr, mid, prov, SelStats, SpectTile, TileKey, TileKind,
    TileSrc, METRIC_ARRAY_LEN,
};
use super::spectra::{
    spec_bands_on, spec_fft_len, spec_ln_step, SliceSource, SpecEngine, SpecScratch, SpecSource,
    SPEC_DB_FLOOR, SPEC_DB_RANGE,
};
use super::subject::{Subject, SubjectMode, TileStore};
use crate::player::convolver::SourceBuf;

/// Columns in a tile (as the whole-track tiles).
pub const TILE_COLS: usize = 64;
/// Zoomed tiles kept per subject (a tile is 32–43 KB).
const TILE_BUDGET: usize = 32 << 20;
/// The largest S and O kept for the zoom (f32 stereo): 4 min of S at
/// 192 kHz is 370 MB, of O at 352.8 kHz 680 MB.
pub const S_MAX_BYTES: usize = 512 << 20;
pub const O_MAX_BYTES: usize = 768 << 20;
/// Samples and tiles go when no page has asked for this long.
pub const IDLE_DROP_MS: u64 = 60_000;
/// At most this many tiles in one ask, and in one reply.
const MAX_TILES_PER_ASK: u32 = 64;
const MAX_TILES_PER_REPLY: usize = 16;
/// The coarsest zoom (hop 2^lod source samples).
const MAX_LOD: u8 = 14;

/// Milliseconds since the first call (never 0).
pub fn now_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// The finest zoom for a source at `rate`: a column per 1/32 of the
/// reference FFT (128 samples at 44.1/48 kHz, 2.9 ms), where the short FFT's
/// frames are an eighth of their length apart; finer would only interpolate.
pub fn min_lod(rate: u32) -> u8 {
    ((spec_fft_len(rate) / 32).max(1)).trailing_zeros() as u8
}

/// A signal the zoom computes from.
pub enum ZoomSamples {
    /// A copy in f32 (S, O).
    Own { rate: u32, l: Vec<f32>, r: Vec<f32> },
    /// The variant's own buffer (B).
    Shared(Arc<SourceBuf>),
}

impl ZoomSamples {
    fn rate(&self) -> u32 {
        match self { ZoomSamples::Own { rate, .. } => *rate, ZoomSamples::Shared(b) => b.rate }
    }

    fn len(&self) -> usize {
        match self { ZoomSamples::Own { l, .. } => l.len(), ZoomSamples::Shared(b) => b.l.len() }
    }

    fn with_source<R>(&self, f: impl FnOnce(&dyn SpecSource) -> R) -> R {
        match self {
            ZoomSamples::Own { l, r, .. } => f(&SliceSource { l, r }),
            ZoomSamples::Shared(b) => f(&SliceSource { l: &b.l, r: &b.r }),
        }
    }

    /// Samples `[a, b)` as f64.
    fn copy_f64(&self, a: usize, b: usize, l: &mut Vec<f64>, r: &mut Vec<f64>) {
        l.clear();
        r.clear();
        match self {
            ZoomSamples::Own { l: sl, r: sr, .. } => {
                l.extend(sl[a..b].iter().map(|&v| v as f64));
                r.extend(sr[a..b].iter().map(|&v| v as f64));
            }
            ZoomSamples::Shared(s) => {
                l.extend_from_slice(&s.l[a..b]);
                r.extend_from_slice(&s.r[a..b]);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZoomAsk {
    lod: u8,
    i0: u32,
    i1: u32,
}

/// One signal's zoom.
struct ZoomSrc {
    /// The pass its tiles belong to.
    gen: u64,
    samples: Option<Arc<ZoomSamples>>,
    /// The source rate (the grid) and the signal's samples per source
    /// sample (O's L; 1 for S and B).
    src_rate: u32,
    lf: usize,
    /// No zoom for this pass's signal.
    unavailable: bool,
    ask: Option<ZoomAsk>,
    /// The selection asked for (source samples), its statistics when done
    /// (AASL bytes), and whether a thread computes them now.
    stat_ask: Option<(u64, u64)>,
    stat: Option<(u64, u64, Arc<[u8]>)>,
    stat_running: bool,
    /// S is being decoded again for the zoomed waveform.
    wave_decoding: bool,
}

impl ZoomSrc {
    fn new() -> ZoomSrc {
        ZoomSrc {
            gen: 0, samples: None, src_rate: 0, lf: 1, unavailable: false, ask: None,
            stat_ask: None, stat: None, stat_running: false, wave_decoding: false,
        }
    }
}

/// A subject's zoom (in `Subject::zoom`).
pub struct ZoomState {
    srcs: [ZoomSrc; 3],
    tiles: TileStore,
}

impl ZoomState {
    pub fn new() -> ZoomState {
        ZoomState { srcs: [ZoomSrc::new(), ZoomSrc::new(), ZoomSrc::new()], tiles: TileStore::with_cap(TILE_BUDGET) }
    }

    /// A new pass of `src` (a new track for S): nothing of the old one stays.
    pub fn reset(&mut self, src: TileSrc, gen: u64) {
        let z = &mut self.srcs[src.flag() as usize];
        *z = ZoomSrc::new();
        z.gen = gen;
        self.tiles.remove_where(|k| k.src == src);
    }

    fn drop_all(&mut self) {
        for z in self.srcs.iter_mut() {
            z.samples = None;
            z.ask = None;
            z.stat_ask = None;
        }
        self.tiles.clear();
    }
}

/// A pass is done: its signal for the zoom (S and O as f32 copies, B as its
/// buffer; None when it was not kept: then B and O have no zoom). For O,
/// `lf` output samples stand for one source sample.
pub fn offer(subject: &Subject, src: TileSrc, gen: u64, samples: Option<ZoomSamples>, src_rate: u32, lf: usize) {
    let mut z = subject.zoom.lock().unwrap();
    let s = &mut z.srcs[src.flag() as usize];
    if s.gen != gen {
        return;
    }
    s.src_rate = src_rate;
    s.lf = lf.max(1);
    s.samples = samples.map(Arc::new);
}

/// Whether a pass of `src` should keep its signal for the zoom now.
pub fn wants(subject: &Subject, bytes: usize, src: TileSrc) -> bool {
    let cap = if src == TileSrc::O { O_MAX_BYTES } else { S_MAX_BYTES };
    subject.idle_ms() <= IDLE_DROP_MS && bytes <= cap
}

/// f32 copies of a pass's buffers.
pub fn own_f32(rate: u32, l: &[f64], r: &[f64]) -> ZoomSamples {
    ZoomSamples::Own { rate, l: l.iter().map(|&v| v as f32).collect(), r: r.iter().map(|&v| v as f32).collect() }
}

/// The `specz` route: record that the page shows `src`'s tiles `i0..i1` at
/// `lod` (it holds those whose bit is set in `got`, bit k = tile i0 + k) and
/// return the others that are ready.
pub fn reply(subject: &Subject, src: TileSrc, lod: u8, i0: u32, i1: u32, got: u64) -> Arc<[u8]> {
    let i1 = i1.min(i0.saturating_add(MAX_TILES_PER_ASK));
    let mut z = subject.zoom.lock().unwrap();
    let zs = &z.srcs[src.flag() as usize];
    let gen32 = zs.gen as u32;
    if zs.unavailable || lod > MAX_LOD || i1 <= i0 {
        return encode_aazr(if zs.unavailable { 2 } else { 1 }, gen32, &[]).into();
    }
    // Only S decodes again: B and O exist only while their pass keeps them.
    if zs.samples.is_none() && src != TileSrc::S && zs.src_rate != 0 {
        return encode_aazr(2, gen32, &[]).into();
    }
    if zs.src_rate != 0 && lod < min_lod(zs.src_rate) {
        return encode_aazr(2, gen32, &[]).into();
    }
    // Past the track's end there is nothing to wait for.
    let n_tiles = zs.samples.as_ref()
        .map(|s| (s.len() / zs.lf).div_ceil(1usize << lod).div_ceil(TILE_COLS) as u32)
        .unwrap_or(u32::MAX);
    let mut out = Vec::new();
    let mut missing = false;
    for i in i0..i1.min(n_tiles) {
        if (i - i0) < 64 && got & (1u64 << (i - i0)) != 0 {
            continue;
        }
        let key = TileKey { src, kind: TileKind::Spec, lod, idx: i };
        match z.tiles.get(&key) {
            Some(b) if out.len() < MAX_TILES_PER_REPLY => out.push(b),
            _ => missing = true,
        }
    }
    z.srcs[src.flag() as usize].ask = Some(ZoomAsk { lod, i0, i1 });
    drop(z);
    if missing {
        wake(subject.sid, src);
    }
    encode_aazr(if missing { 0 } else { 1 }, gen32, &out).into()
}

// ─── The zoomed waveform ─────────────────────────────────────────────────────

/// The most bins one `wavez` reply carries (a wide window, twice over).
const WAVE_MAX_BINS: usize = 8192;

/// The `wavez` route: `src` over source samples `[s0, s1)` as `n` bins of
/// (min L, max L, min R, max R) — the waveform closer than its tiles (an
/// entry per 256 samples) can show, down to the samples themselves: with
/// fewer samples than `n` a bin is one sample (`raw` = 1, min = max).
/// Status 0: not ready (S being decoded again), 1: done, 2: unavailable
/// (B and O are kept only while their pass's page looks).
///
/// Layout (little-endian): "AAWZ", status u8, src u8, raw u8, 0 u8, gen u32,
/// a u64, b u64 (the signal's samples the bins cover: `[s0, s1)` times `lf`,
/// cut at the end), count u32, lf u32, then count × 4 f32.
pub fn reply_wave(subject: &Arc<Subject>, src: TileSrc, s0: u64, s1: u64, n: u32) -> Arc<[u8]> {
    let k = src.flag() as usize;
    let (gen, samples, lf, unavailable) = {
        let z = subject.zoom.lock().unwrap();
        let zs = &z.srcs[k];
        (zs.gen, zs.samples.clone(), zs.lf.max(1), zs.unavailable)
    };
    let head = |status: u8, raw: u8, count: u32, a: u64, b: u64| -> Vec<u8> {
        let mut v = Vec::with_capacity(36 + count as usize * 16);
        v.extend_from_slice(b"AAWZ");
        v.extend_from_slice(&[status, src.flag(), raw, 0]);
        v.extend_from_slice(&(gen as u32).to_le_bytes());
        v.extend_from_slice(&a.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&(lf as u32).to_le_bytes());
        v
    };
    if unavailable || s1 <= s0 || n == 0 {
        return head(2, 0, 0, 0, 0).into();
    }
    let Some(samples) = samples else {
        if src != TileSrc::S {
            return head(2, 0, 0, 0, 0).into();
        }
        // S was not kept: decode it again, once, off the route.
        let start = {
            let mut z = subject.zoom.lock().unwrap();
            let zs = &mut z.srcs[k];
            let start = !zs.wave_decoding;
            zs.wave_decoding = true;
            start
        };
        if start {
            let subject = subject.clone();
            std::thread::Builder::new()
                .name("aura-analytics-wavez".into())
                .spawn(move || {
                    super::track::set_below_normal_priority();
                    let _ = decode(&subject, gen);
                    subject.zoom.lock().unwrap().srcs[k].wave_decoding = false;
                })
                .ok();
        }
        return head(0, 0, 0, 0, 0).into();
    };
    let a = (s0 as usize).saturating_mul(lf).min(samples.len());
    let b = (s1 as usize).saturating_mul(lf).min(samples.len());
    if b <= a {
        return head(2, 0, 0, 0, 0).into();
    }
    let m = b - a;
    let bins = (n as usize).min(WAVE_MAX_BINS).min(m).max(1);
    let raw = (bins == m) as u8;
    let mut v = head(1, raw, bins as u32, a as u64, b as u64);
    let mut push = |get: &dyn Fn(usize) -> (f32, f32)| {
        for i in 0..bins {
            let i0 = a + m * i / bins;
            let i1 = (a + m * (i + 1) / bins).max(i0 + 1);
            let (mut lo_l, mut hi_l, mut lo_r, mut hi_r) = (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
            for j in i0..i1 {
                let (x, y) = get(j);
                lo_l = lo_l.min(x);
                hi_l = hi_l.max(x);
                lo_r = lo_r.min(y);
                hi_r = hi_r.max(y);
            }
            for x in [lo_l, hi_l, lo_r, hi_r] {
                v.extend_from_slice(&x.to_le_bytes());
            }
        }
    };
    match &*samples {
        ZoomSamples::Own { l, r, .. } => push(&|j| (l[j], r[j])),
        ZoomSamples::Shared(s) => push(&|j| (s.l[j] as f32, s.r[j] as f32)),
    }
    v.into()
}

// ─── Selection statistics ────────────────────────────────────────────────────

/// The `selstat` route: the statistics of `src` over source samples
/// `[s0, s1)` if they are done; otherwise they are asked for.
pub fn reply_stats(subject: &Arc<Subject>, src: TileSrc, s0: u64, s1: u64) -> Arc<[u8]> {
    let mut z = subject.zoom.lock().unwrap();
    let zs = &mut z.srcs[src.flag() as usize];
    let head = |status: u8, gen: u64| -> Arc<[u8]> {
        encode_aasl(&SelStats {
            status, src, f32_kept: false, gen: gen as u32, s0, s1,
            metrics: [f64::NAN; METRIC_ARRAY_LEN], prov: [prov::UNAVAIL; METRIC_ARRAY_LEN],
            welch_n: 0, f0: 0.0, f1: 0.0, pairs: Vec::new(),
        }).into()
    };
    if let Some((a, b, bytes)) = &zs.stat {
        if (*a, *b) == (s0, s1) {
            return bytes.clone();
        }
    }
    if zs.unavailable || s1 <= s0 {
        return head(2, zs.gen);
    }
    // B and O come only from their passes: not kept (2), or the pass runs (0).
    if src != TileSrc::S && zs.samples.is_none() {
        return head(if zs.src_rate != 0 { 2 } else { 0 }, zs.gen);
    }
    zs.stat_ask = Some((s0, s1));
    let start = !zs.stat_running;
    zs.stat_running = true;
    let gen = zs.gen;
    drop(z);
    if start {
        let subject = subject.clone();
        std::thread::Builder::new()
            .name("aura-analytics-selstat".into())
            .spawn(move || {
                super::track::set_below_normal_priority();
                serve_stats(&subject, src);
            })
            .ok();
    }
    head(0, gen)
}

/// Compute the asked selection until the ask stays the same.
fn serve_stats(subject: &Subject, src: TileSrc) {
    let k = src.flag() as usize;
    loop {
        let (gen, ask, samples, lf, src_rate) = {
            let mut z = subject.zoom.lock().unwrap();
            let zs = &mut z.srcs[k];
            let Some(ask) = zs.stat_ask else { zs.stat_running = false; return };
            if zs.stat.as_ref().is_some_and(|(a, b, _)| (*a, *b) == ask) {
                zs.stat_running = false;
                return;
            }
            (zs.gen, ask, zs.samples.clone(), zs.lf, zs.src_rate)
        };
        let samples = match samples {
            Some(s) => Some(s),
            None if src == TileSrc::S => decode(subject, gen),
            None => None,
        };
        let Some(samples) = samples else {
            let mut z = subject.zoom.lock().unwrap();
            z.srcs[k].stat_running = false;
            return;
        };
        let src_rate = if src_rate != 0 { src_rate } else { samples.rate() };
        let mut st = range_stats(&samples, lf, src_rate, ask.0, ask.1);
        st.src = src;
        st.gen = gen as u32;
        // S is exact in f32 (16/24-bit and float files), O is rounded to it.
        st.f32_kept = src == TileSrc::O && matches!(*samples, ZoomSamples::Own { .. });
        let bytes: Arc<[u8]> = encode_aasl(&st).into();
        let mut z = subject.zoom.lock().unwrap();
        let zs = &mut z.srcs[k];
        if zs.gen != gen {
            zs.stat_running = false;
            return;
        }
        zs.stat = Some((ask.0, ask.1, bytes));
    }
}

/// The whole-track metrics over source samples `[s0, s1)` of a signal with
/// `lf` samples per source sample, measured by the O pass's meters, and the
/// stretch's spectrum.
fn range_stats(samples: &ZoomSamples, lf: usize, src_rate: u32, s0: u64, s1: u64) -> SelStats {
    const BLOCK: usize = 8192;
    let rate = samples.rate();
    let n = samples.len();
    let a = (s0 as usize).saturating_mul(lf).min(n);
    let b = (s1 as usize).saturating_mul(lf).min(n);
    let len = b - a;
    // The whole track's Welch length (65536 per source sample) when the
    // stretch holds two of its frames, else the longest that fits twice.
    let full = 65536 * lf;
    let welch_n = if len >= 2 * full { full } else { (len / 2).max(2).next_power_of_two() / 2 }.max(256 * lf).min(full);
    let mut m = super::opass::Meters::with_welch(rate, len, welch_n);
    let (mut l, mut r) = (Vec::with_capacity(BLOCK), Vec::with_capacity(BLOCK));
    let (mut sum_l, mut sum_r) = (0.0f64, 0.0f64);
    let mut at = a;
    while at < b {
        let e = (at + BLOCK).min(b);
        samples.copy_f64(at, e, &mut l, &mut r);
        sum_l += l.iter().sum::<f64>();
        sum_r += r.iter().sum::<f64>();
        m.push(&l, &r, at);
        at = e;
    }
    let (part, psd) = m.finish(rate);
    let mut metrics = part.metrics;
    let mut pv = part.prov;
    metrics[mid::COVERAGE_O] = f64::NAN;
    pv[mid::COVERAGE_O] = prov::UNAVAIL;
    if len > 0 {
        let dc = (sum_l / len as f64).abs().max((sum_r / len as f64).abs()) * 100.0;
        metrics[mid::DC_OFFSET] = dc;
        pv[mid::DC_OFFSET] = prov::MEASURED;
    }
    // DR needs at least one whole 3-second block.
    if len < 3 * rate as usize {
        for id in [mid::DR, mid::RMS_TOP20] {
            metrics[id] = f64::NAN;
            pv[id] = prov::UNAVAIL;
        }
    }
    let nyq = rate as f64 / 2.0;
    let pairs = decimate_pairs(&psd, nyq, 20.0, nyq, 1024, true);
    let _ = src_rate;
    SelStats {
        status: 1, src: TileSrc::S, f32_kept: false, gen: 0, s0, s1, metrics, prov: pv,
        welch_n: welch_n as u32, f0: 20.0, f1: nyq as f32, pairs,
    }
}

// ─── The worker ──────────────────────────────────────────────────────────────

struct Queue {
    jobs: Mutex<Vec<(u32, TileSrc)>>,
    cond: Condvar,
}

fn queue() -> &'static Queue {
    static Q: OnceLock<Queue> = OnceLock::new();
    Q.get_or_init(|| {
        std::thread::Builder::new()
            .name("aura-analytics-zoom".into())
            .spawn(worker)
            .expect("failed to spawn the zoom thread");
        Queue { jobs: Mutex::new(Vec::new()), cond: Condvar::new() }
    })
}

fn wake(sid: u32, src: TileSrc) {
    let q = queue();
    let mut g = q.jobs.lock().unwrap();
    if !g.contains(&(sid, src)) {
        g.push((sid, src));
    }
    drop(g);
    q.cond.notify_one();
}

type Engines = HashMap<(u32, usize, usize, usize), Arc<SpecEngine>>;

fn worker() {
    super::track::set_below_normal_priority();
    let mut engines: Engines = HashMap::new();
    let mut scratch = SpecScratch::default();
    let q = queue();
    loop {
        let (sid, src) = {
            let mut g = q.jobs.lock().unwrap();
            loop {
                if !g.is_empty() {
                    break g.remove(0);
                }
                let (ng, to) = q.cond.wait_timeout(g, Duration::from_secs(10)).unwrap();
                g = ng;
                if to.timed_out() {
                    drop(g);
                    gc();
                    g = q.jobs.lock().unwrap();
                }
            }
        };
        let Some(hub) = super::hub::try_get() else { continue };
        if let Some(subject) = hub.subject(sid) {
            serve(&subject, src, &mut engines, &mut scratch);
        }
    }
}

/// Nobody looks at a subject any more: its samples and tiles go.
fn gc() {
    let Some(hub) = super::hub::try_get() else { return };
    for s in hub.all_subjects() {
        if s.idle_ms() > IDLE_DROP_MS {
            s.zoom.lock().unwrap().drop_all();
        }
    }
}

fn serve(subject: &Subject, src: TileSrc, engines: &mut Engines, scratch: &mut SpecScratch) {
    let k = src.flag() as usize;
    loop {
        let (gen, ask, samples, lf, src_rate) = {
            let z = subject.zoom.lock().unwrap();
            let zs = &z.srcs[k];
            if zs.unavailable { return; }
            let Some(ask) = zs.ask else { return };
            (zs.gen, ask, zs.samples.clone(), zs.lf, zs.src_rate)
        };
        let samples = match samples {
            Some(s) => s,
            None if src == TileSrc::S => match decode(subject, gen) {
                Some(s) => s,
                None => return,
            },
            None => return,
        };
        let src_rate = if src_rate != 0 { src_rate } else { samples.rate() };
        if ask.lod < min_lod(src_rate) {
            return;
        }
        let rate = samples.rate();
        let hop = (1usize << ask.lod) * lf;
        let n_bands = spec_bands_on(src_rate, rate);
        let eng = engines.entry((rate, hop, n_bands, src_rate as usize))
            .or_insert_with(|| Arc::new(SpecEngine::new(rate, hop, n_bands, spec_ln_step(src_rate))))
            .clone();
        let n_cols = (samples.len() / lf).div_ceil(1usize << ask.lod);
        let n_tiles = n_cols.div_ceil(TILE_COLS);
        let (i0, i1) = (ask.i0 as usize, (ask.i1 as usize).min(n_tiles));
        // The middle of the stretch first.
        let mid = (i0 + i1) as f64 / 2.0 - 0.5;
        let mut order: Vec<usize> = (i0..i1).collect();
        order.sort_by(|&a, &b| (a as f64 - mid).abs().total_cmp(&(b as f64 - mid).abs()));
        let mut moved = false;
        for i in order {
            let key = TileKey { src, kind: TileKind::Spec, lod: ask.lod, idx: i as u32 };
            {
                let z = subject.zoom.lock().unwrap();
                let zs = &z.srcs[k];
                if zs.gen != gen || zs.ask != Some(ask) {
                    moved = true;
                    break;
                }
                if z.tiles.contains(&key) {
                    continue;
                }
            }
            let c0 = i * TILE_COLS;
            let c1 = (c0 + TILE_COLS).min(n_cols);
            let cols = samples.with_source(|s| eng.columns(s, c0, c1, scratch));
            let bytes: Arc<[u8]> = encode_aast(&SpectTile {
                src_flag: src.flag(),
                lod: ask.lod,
                gen8: gen as u8,
                tile_idx: i as u32,
                db_floor: SPEC_DB_FLOOR as f32,
                db_range: SPEC_DB_RANGE as f32,
                n_cols: (c1 - c0) as u16,
                n_bins: n_bands as u16,
                data: cols.concat(),
            }).into();
            let mut z = subject.zoom.lock().unwrap();
            if z.srcs[k].gen == gen {
                z.tiles.insert(key, bytes);
            }
        }
        if !moved {
            return;
        }
    }
}

/// The file whose S the subject shows: a file window's (the converted
/// output, else the file), the live subject's track.
fn s_file(subject: &Subject) -> Option<PathBuf> {
    match subject.mode {
        SubjectMode::Live => {
            let tid = subject.track_id.load(std::sync::atomic::Ordering::Relaxed);
            crate::player::controller::get().track_path(tid).map(PathBuf::from)
        }
        _ => subject.conv.clone().or_else(|| subject.path.clone().filter(|p| !p.as_os_str().is_empty())),
    }
}

/// Decode the S signal again (it was not kept, or was dropped while nobody
/// looked).
fn decode(subject: &Subject, gen: u64) -> Option<Arc<ZoomSamples>> {
    use crate::player::settings::{Mode, PlayerSettings};
    let k = TileSrc::S.flag() as usize;
    let fail = || {
        let mut z = subject.zoom.lock().unwrap();
        if z.srcs[k].gen == gen { z.srcs[k].unavailable = true; }
    };
    let Some(path) = s_file(subject) else { fail(); return None };
    let direct = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let t0 = Instant::now();
    let v = match crate::player::chain::prepare_variant(&path, &direct, false, &cancel) {
        Ok(v) => v,
        Err(e) => {
            crate::aelog!("[ANALYZER] zoom: {}: {}", path.display(), e);
            fail();
            return None;
        }
    };
    let s = Arc::new(own_f32(v.src.rate, &v.src.l, &v.src.r));
    crate::aelog!("[ANALYZER] zoom: decoded {} in {:.1} s", path.display(), t0.elapsed().as_secs_f64());
    let mut z = subject.zoom.lock().unwrap();
    let zs = &mut z.srcs[k];
    if zs.gen != gen {
        return None;
    }
    zs.src_rate = v.src.rate;
    // Kept for the next stretches unless it is too large to hold.
    if s.len() * 8 <= S_MAX_BYTES {
        zs.samples = Some(s.clone());
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_lod_is_a_32nd_of_the_reference_fft() {
        assert_eq!(min_lod(44_100), 7);
        assert_eq!(min_lod(48_000), 7);
        assert_eq!(min_lod(96_000), 8);
        assert_eq!(min_lod(192_000), 9);
    }

    fn parse(b: &[u8]) -> (u8, u32, Vec<Vec<u8>>) {
        assert_eq!(&b[..4], b"AAZR");
        let n = u16::from_le_bytes([b[6], b[7]]) as usize;
        let gen = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let mut off = 12;
        let mut tiles = Vec::new();
        for _ in 0..n {
            let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            tiles.push(b[off + 4..off + 4 + len].to_vec());
            off += 4 + len;
        }
        assert_eq!(off, b.len());
        (b[5], gen, tiles)
    }

    /// The reply sends what is ready (not what the page holds) and says
    /// whether more is coming.
    #[test]
    fn reply_sends_ready_tiles_and_the_status() {
        let s = Subject::new_live(u32::MAX - 3);
        s.touch();
        {
            let mut z = s.zoom.lock().unwrap();
            z.reset(TileSrc::S, 5);
            for i in 0..3u32 {
                let key = TileKey { src: TileSrc::S, kind: TileKind::Spec, lod: 7, idx: i };
                z.tiles.insert(key, vec![i as u8; 10].into());
            }
        }
        let n = 128 * 64 * 4;
        offer(&s, TileSrc::S, 5, Some(own_f32(44_100, &vec![0.0; n], &vec![0.0; n])), 44_100, 1);
        // Tiles 0..3 of 4 are ready; the page holds tile 1.
        let (status, gen, tiles) = parse(&reply(&s, TileSrc::S, 7, 0, 4, 0b10));
        assert_eq!(gen, 5);
        assert_eq!(tiles, vec![vec![0u8; 10], vec![2u8; 10]]);
        assert_eq!(status, 0, "tile 3 is still missing");
        // Asking past the end: only the track's 4 tiles count.
        {
            let mut z = s.zoom.lock().unwrap();
            let key = TileKey { src: TileSrc::S, kind: TileKind::Spec, lod: 7, idx: 3 };
            z.tiles.insert(key, vec![3u8; 10].into());
        }
        let (status, _, tiles) = parse(&reply(&s, TileSrc::S, 7, 2, 40, 0b11));
        assert_eq!((status, tiles.len()), (1, 0));
        // Finer than the finest zoom: none.
        let (status, _, _) = parse(&reply(&s, TileSrc::S, 6, 0, 4, 0));
        assert_eq!(status, 2);
    }

    /// A selection's statistics are the meters' over that stretch only.
    #[test]
    fn range_stats_measure_only_the_stretch() {
        let rate = 48_000u32;
        let n = rate as usize * 4;
        // Silence, then one second of a 1 kHz sine at half scale with 1 % DC, then silence.
        let mut l = vec![0.0f64; n];
        for i in rate as usize..2 * rate as usize {
            l[i] = 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64).sin() + 0.01;
        }
        let s = own_f32(rate, &l, &l);
        let st = range_stats(&s, 1, rate, rate as u64, 2 * rate as u64);
        assert_eq!(st.status, 1);
        assert!((st.metrics[mid::SP] - 0.51).abs() < 1e-3, "SP {}", st.metrics[mid::SP]);
        assert!((st.metrics[mid::DC_OFFSET] - 1.0).abs() < 0.05, "DC {}", st.metrics[mid::DC_OFFSET]);
        let lufs = st.metrics[mid::LUFS_I];
        assert!(lufs > -8.0 && lufs < -5.0, "LUFS-I {lufs}");
        assert!((st.metrics[mid::PEAK_AT] - 1.0).abs() < 1.0, "peak at {}", st.metrics[mid::PEAK_AT]);
        // The silence around it is not in the stretch: the whole signal reads lower.
        let all = range_stats(&s, 1, rate, 0, n as u64);
        assert!(all.metrics[mid::LUFS_I] < lufs + 0.01);
        assert_eq!(st.pairs.len(), 1024);
    }

    /// O's zoom counts its tiles on the source grid (its samples are L per
    /// source sample), and a new O pass drops the old one's tiles.
    #[test]
    fn output_zoom_is_on_the_source_grid() {
        let s = Subject::new_live(u32::MAX - 4);
        s.touch();
        s.zoom.lock().unwrap().reset(TileSrc::O, 9);
        let n = 128 * 64 * 2 * 2;   // 2 tiles at lod 7 of the source, L = 2
        offer(&s, TileSrc::O, 9, Some(own_f32(88_200, &vec![0.0; n], &vec![0.0; n])), 44_100, 2);
        {
            let mut z = s.zoom.lock().unwrap();
            for i in 0..2u32 {
                z.tiles.insert(TileKey { src: TileSrc::O, kind: TileKind::Spec, lod: 7, idx: i }, vec![7u8; 4].into());
            }
        }
        let (status, gen, tiles) = parse(&reply(&s, TileSrc::O, 7, 0, 10, 0));
        assert_eq!((status, gen, tiles.len()), (1, 9, 2));
        s.zoom.lock().unwrap().reset(TileSrc::O, 10);
        let (status, _, tiles) = parse(&reply(&s, TileSrc::O, 7, 0, 10, 0));
        assert_eq!((status, tiles.len()), (0, 0), "the new pass is running: its zoom is to come");
        // It ends without keeping its signal: no zoom for O.
        offer(&s, TileSrc::O, 10, None, 44_100, 2);
        let (status, _, _) = parse(&reply(&s, TileSrc::O, 7, 0, 10, 0));
        assert_eq!(status, 2);
    }
}
