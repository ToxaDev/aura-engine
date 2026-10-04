//! The O pass: the whole track through the chain heard, measured without
//! listening to it.
//!
//! The hub queues an [`OWork`] for every chain the live thread hears. This
//! pass rebuilds that chain with `chain::build_chain` — an instance of its
//! own — and reads it from the first sample to the last, feeding streaming
//! meters; the output itself is never stored (4 min at 352.8 kHz would be
//! 1.35 GB). It is bit-exact with what is heard: the convolver's block grid is
//! anchored at the file's first sample, whatever the start.
//!
//! On the GPU when allowed (the chain heard is on it, or a file window's rack
//! has it on) as a *background* stream: it takes at most a quarter of the free
//! VRAM and its watchdog timeouts never count toward disabling the GPU for
//! playback. If it fails, the pass goes on from the same sample on the CPU.
//!
//! It runs on the subject's track thread after S and B, and waits while
//! playback is short of its look-ahead, so it never costs a dropout. A new
//! chain, a new track or the window closing ends it at the next block.
//!
//! The output's spectrogram is made on the way, on the S tiles' time grid
//! (a column per `spec_hop × L` output samples) and in the S bands, carried
//! on above the source's Nyquist at the same step: a thread of its own takes
//! the blocks as they come and stores each tile as soon as the samples it
//! needs are in, so O's tiles appear from the start while the pass runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::audio::converter::dsp::true_peak::TruePeakScan;
use crate::player::chain::{build_chain, hp_ready, Chain};
use crate::player::gpu::{poly_stream::set_background, GpuPolyCtx};
use crate::player::settings::Phase;

use super::dr::DrLive;
use super::loudness::LoudnessMeter;
use super::lra::lra_ebu;
use super::peaks::{EburTruePeak, lin_to_db};
use super::proto::{mid, prov, DrChannel, SpecInfo, TileSrc, METRIC_ARRAY_LEN};
use super::spectra::{
    spec_bands_on, spec_ln_step, SpecEngine, SpecScratch, SpecSource, WelchAccum, band_level,
    effective_bandwidth,
};
use super::proto::WaveSample;
use super::subject::{OPart, OWork, Subject};
use super::track::{
    block_stats, lod_count, spec_hop, spec_threads, store_mipmaps, store_spec_tile, wave_tile_counts,
    SPEC_TILE_COLS,
};

/// Frames read from the chain at a time (the render thread's step).
const BLOCK: usize = 8192;
/// How often the progress goes to the page.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// The live pass stops once the analyzer window has not asked for this long
/// (a window reopened at once finds it still running).
const UNWATCHED_STOP_MS: u64 = 10_000;

pub fn run(subject: &Subject, w: OWork) {
    let gen = w.generation;
    let stale = || subject.gen_o() != gen || subject.is_shutdown();
    let player = crate::player::controller::get();
    let res = player.resources();

    // A converted file from disk: O is the file.
    if let Some(file) = w.file.clone() {
        return run_file(subject, w, &file);
    }
    // Hybrid-Phase needs the track's envelope; without it the chain would
    // stand in linear phase. The player makes it for what it plays (wait for
    // it); a file window makes its own.
    if matches!(w.settings.phase, Phase::Hybrid | Phase::Alpha) {
        if w.own_envelope {
            if res.envelope(&w.track_key, &w.variant).is_err() {
                if !stale() { subject.publish_o_part(unavailable()); }
                return;
            }
        }
        while !hp_ready(res, &w.track_key, &w.variant) {
            if stale() { return; }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    // The GPU marks stay on this thread for the whole pass.
    struct Background;
    impl Drop for Background {
        fn drop(&mut self) { set_background(false); }
    }
    set_background(true);
    let _bg = Background;
    let gpu = if w.gpu {
        GpuPolyCtx::try_build()
            .filter(|c| !c.player_device_error.load(std::sync::atomic::Ordering::Acquire))
    } else {
        None
    };
    let build = |start: u64, gpu: Option<std::sync::Arc<GpuPolyCtx>>| -> Option<Chain> {
        build_chain(res, w.track_id, &w.track_key, &w.variant, &w.settings, start, gpu,
                    w.downgrade.clone(), w.downgrade_gen).ok()
    };
    let mut on_gpu = gpu.is_some();
    let chain = match build(0, gpu).or_else(|| { on_gpu = false; build(0, None) }) {
        Some(c) => c,
        None => {
            if !stale() { subject.publish_o_part(unavailable()); }
            return;
        }
    };

    let lf = w.variant.l.max(1);
    measure(subject, &w, chain, lf, on_gpu, Some(&build));
}

/// O of a converted file played from disk: the file itself, on the grid of
/// the track's source (`w.variant`, decoded as it is: L = the file's rate
/// over the source's).
fn run_file(subject: &Subject, w: OWork, file: &str) {
    let stale = || subject.gen_o() != w.generation || subject.is_shutdown();
    // The copy the player plays from, else the file decoded again.
    let src = match crate::player::controller::get().disk_source(file) {
        Some(s) => s,
        None => match crate::player::chain::disk_file(std::path::Path::new(file)) {
            Ok(s) => s,
            Err(e) => {
                crate::aelog!("[ANALYZER] O of {}: {}", file, e);
                if !stale() { subject.publish_o_part(unavailable()); }
                return;
            }
        },
    };
    let src_rate = w.variant.src.rate.max(1);
    if src.rate % src_rate != 0 {
        crate::aelog!("[ANALYZER] O of {}: {} Hz is not a multiple of the source's {} Hz", file, src.rate, src_rate);
        if !stale() { subject.publish_o_part(unavailable()); }
        return;
    }
    let lf = (src.rate / src_rate) as usize;
    let chain = crate::player::chain::build_file_chain(w.track_id, src, 0.0, std::path::Path::new(file));
    measure(subject, &w, chain, lf, false, None);
}

/// Read `chain` to the end through the O meters, spectrogram and waveform.
/// `rebuild` (a rendered chain) replaces a GPU stream that failed.
fn measure(
    subject: &Subject,
    w: &OWork,
    mut chain: Chain,
    lf: usize,
    mut on_gpu: bool,
    rebuild: Option<&dyn Fn(u64, Option<std::sync::Arc<GpuPolyCtx>>) -> Option<Chain>>,
) {
    let player = crate::player::controller::get();
    let gen = w.generation;
    let stale = || subject.gen_o() != gen || subject.is_shutdown();
    let started_on_gpu = on_gpu;
    let t0 = Instant::now();
    let n_src = w.variant.src.len();
    let total = n_src * lf;
    let rate = chain.out_rate;
    let mut m = Meters::new(rate, total, lf);
    let mut bl = vec![0.0f64; BLOCK];
    let mut br = vec![0.0f64; BLOCK];
    let mut done = 0usize;
    let mut last_pub = Instant::now();

    // The spectrogram's thread and its feed (a full feed holds the chain
    // back: the pass then takes as long as its spectrogram).
    let (tx, rx) = sync_channel::<(Vec<f64>, Vec<f64>)>(32);
    let spec = OSpec::new(w.variant.src.rate, rate, n_src, lf, gen);
    // O's waveform on the S grid: an entry per 256 × L output samples.
    let mut wave = WaveO::new(256 * lf, n_src.div_ceil(256));
    // O for the zoom, in f32, while a page looks and it is not too large.
    let mut keep = super::zoom::wants(subject, total * 8, TileSrc::O)
        .then(|| (Vec::<f32>::with_capacity(total), Vec::<f32>::with_capacity(total)));
    let fed_all = AtomicBool::new(false);
    let finished = std::thread::scope(|sc| {
        sc.spawn(|| spec.run(subject, rx, &fed_all));
        // Every way out of here drops the feed, so the spectrogram's thread
        // never waits for a block that will not come.
        let mut tx = Some(tx);
        while done < total {
            if stale() { return false; }
            // The live analyzer's window has been closed a while: stop, and
            // let it start again when the window opens.
            if subject.sid == super::hub::LIVE_SID && subject.idle_ms() > UNWATCHED_STOP_MS {
                if let Some(h) = super::hub::try_get() { h.o_put_off(); }
                crate::aelog!("[ANALYZER] O pass put off: the analyzer window is closed");
                return false;
            }
            // Playback first, and the stages being prepared for it.
            while super::hub::try_get().map(|h| h.playback_starved()).unwrap_or(false)
                || player.preparing()
            {
                std::thread::sleep(Duration::from_millis(200));
                if stale() { return false; }
            }
            let want = (total - done).min(BLOCK);
            let n = chain.stage.read(&mut bl[..want], &mut br[..want]).min(want);
            if on_gpu && chain.stage.is_gpu_failed() {
                // This block is lost with the GPU stream: the CPU goes on from
                // its first sample.
                on_gpu = false;
                match rebuild.and_then(|b| b(done as u64, None)) {
                    Some(c) => { chain = c; continue; }
                    None => {
                        if !stale() { subject.publish_o_part(unavailable()); }
                        return false;
                    }
                }
            }
            if n == 0 { break; }
            m.push(&bl[..n], &br[..n], done);
            if let Some((kl, kr)) = keep.as_mut() {
                kl.extend(bl[..n].iter().map(|&v| v as f32));
                kr.extend(br[..n].iter().map(|&v| v as f32));
            }
            wave.push(&bl[..n], &br[..n]);
            // A replaced pass's spectrogram has stopped listening.
            if let Some(t) = &tx {
                if t.send((bl[..n].to_vec(), br[..n].to_vec())).is_err() { tx = None; }
            }
            done += n;
            if last_pub.elapsed() >= PROGRESS_EVERY {
                subject.publish_o_part(OPart::progress(done as f64 * 100.0 / total as f64));
                last_pub = Instant::now();
            }
        }
        // The whole track went in: the spectrogram finishes its last tiles.
        fed_all.store(true, Ordering::Release);
        drop(tx);
        true
    });
    if !finished || stale() { return; }
    crate::aelog!(
        "[ANALYZER] O pass: {:.1} s of audio in {:.1} s, {}",
        total as f64 / rate as f64,
        t0.elapsed().as_secs_f64(),
        match (started_on_gpu, on_gpu, w.file.is_some()) {
            (_, _, true) => "the converted file",
            (true, true, _) => "GPU",
            (true, false, _) => "GPU, then the CPU after a GPU failure",
            _ => "CPU",
        }
    );
    let (part, psd) = m.finish(rate);
    subject.publish_o_part(part);
    subject.set_measured_o(psd, w.chain_rev);
    let samples = keep.map(|(l, r)| super::zoom::ZoomSamples::Own { rate, l, r });
    super::zoom::offer(subject, TileSrc::O, gen, samples, w.variant.src.rate, lf);
    let (wl, wr) = wave.finish();
    let n_lods = lod_count(n_src);
    store_mipmaps(subject, wl, wr, n_lods, TileSrc::O, gen);
    subject.set_wave_o(wave_tile_counts(n_src, 256, n_lods), gen);
}

/// The output's spectrogram, fed the pass's blocks.
struct OSpec {
    engine: SpecEngine,
    n_cols: usize,
    n_tiles: usize,
    gen: u64,
}

/// The samples the spectrogram still needs: `[base, base + l.len())`.
struct Held {
    l: Vec<f64>,
    r: Vec<f64>,
    base: usize,
}

impl SpecSource for Held {
    fn frame(&self, start: isize, l: &mut [f64], r: &mut [f64]) {
        l.fill(0.0);
        r.fill(0.0);
        let a = start.max(self.base as isize);
        let b = (start + l.len() as isize).min((self.base + self.l.len()) as isize);
        for i in a..b {
            let (d, s) = ((i - start) as usize, i as usize - self.base);
            l[d] = self.l[s];
            r[d] = self.r[s];
        }
    }
}

impl OSpec {
    fn new(src_rate: u32, out_rate: u32, n_src: usize, lf: usize, gen: u64) -> OSpec {
        let hop = spec_hop(n_src);
        let n_cols = n_src.div_ceil(hop);
        OSpec {
            engine: SpecEngine::new(out_rate, hop * lf, spec_bands_on(src_rate, out_rate), spec_ln_step(src_rate)),
            n_cols,
            n_tiles: n_cols.div_ceil(SPEC_TILE_COLS),
            gen,
        }
    }

    fn info(&self, ready: usize) -> SpecInfo {
        SpecInfo {
            gen: self.gen as u32,
            ready: ready as u32,
            total: self.n_tiles as u32,
            n_bins: self.engine.n_bands as u16,
        }
    }

    /// Each tile as soon as its samples are in (its columns split over up
    /// to `spec_threads` threads); what no later column needs is let go.
    fn run(&self, subject: &Subject, rx: Receiver<(Vec<f64>, Vec<f64>)>, fed_all: &AtomicBool) {
        super::track::set_below_normal_priority();
        let stale = || subject.pass_gen(TileSrc::O) != self.gen || subject.is_shutdown();
        let mut held = Held { l: Vec::new(), r: Vec::new(), base: 0 };
        let mut next = 0usize;
        let mut ended = false;
        let mut last_pub = Instant::now();
        let threads = spec_threads();
        let mut scratch: Vec<SpecScratch> = (0..threads).map(|_| SpecScratch::default()).collect();
        let t0 = Instant::now();
        while next < self.n_tiles {
            if stale() { return; }
            let c1 = ((next + 1) * SPEC_TILE_COLS).min(self.n_cols);
            if !ended && self.engine.needs_until(c1 - 1) > held.base + held.l.len() {
                // Wait for a block, then take whatever else has come.
                let mut take = |b: (Vec<f64>, Vec<f64>)| { held.l.extend_from_slice(&b.0); held.r.extend_from_slice(&b.1); };
                match rx.recv() {
                    Ok(b) => take(b),
                    Err(_) => ended = true,
                }
                while !ended {
                    match rx.try_recv() {
                        Ok(b) => take(b),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => ended = true,
                    }
                }
                // The pass stopped short (it failed, or a new chain came):
                // no tile of silence for the rest.
                if ended && !fed_all.load(Ordering::Acquire) { return; }
                continue;
            }
            let cols = self.tile(&held, next, &mut scratch);
            store_spec_tile(subject, TileSrc::O, self.gen, next as u32, &cols, self.engine.n_bands);
            next += 1;
            if next < self.n_tiles {
                let keep = self.engine.needs_from(next * SPEC_TILE_COLS).max(held.base);
                let drop = (keep - held.base).min(held.l.len());
                held.l.drain(..drop);
                held.r.drain(..drop);
                held.base += drop;
            }
            let reencode = last_pub.elapsed() >= PROGRESS_EVERY;
            if reencode { last_pub = Instant::now(); }
            subject.set_spec_info(TileSrc::O, self.info(next), reencode);
        }
        subject.set_spec_info(TileSrc::O, self.info(self.n_tiles), true);
        crate::aelog!(
            "[ANALYZER] O spectrogram: {} tiles of {} bands in {:.1} s",
            self.n_tiles, self.engine.n_bands, t0.elapsed().as_secs_f64()
        );
    }

    /// Tile `i`, its columns split over the threads (a column's value does
    /// not depend on the split).
    fn tile(&self, held: &Held, i: usize, scratch: &mut [SpecScratch]) -> Vec<Vec<u8>> {
        let c0 = i * SPEC_TILE_COLS;
        let c1 = (c0 + SPEC_TILE_COLS).min(self.n_cols);
        if scratch.len() == 1 {
            return self.engine.columns(held, c0, c1, &mut scratch[0]);
        }
        let per = (c1 - c0).div_ceil(scratch.len());
        std::thread::scope(|sc| {
            let parts: Vec<_> = scratch.iter_mut().enumerate().filter_map(|(k, s)| {
                let a = c0 + k * per;
                let b = (a + per).min(c1);
                (a < b).then(|| sc.spawn(move || {
                    super::track::set_below_normal_priority();
                    self.engine.columns(held, a, b, s)
                }))
            }).collect();
            parts.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        })
    }
}

/// O's waveform entries (min, max, RMS per channel), `per` output samples
/// each: the S grid's 256 source samples.
struct WaveO {
    per: usize,
    l: Vec<WaveSample>,
    r: Vec<WaveSample>,
    bl: Vec<f64>,
    br: Vec<f64>,
}

impl WaveO {
    fn new(per: usize, entries: usize) -> WaveO {
        WaveO { per, l: Vec::with_capacity(entries), r: Vec::with_capacity(entries), bl: Vec::with_capacity(per), br: Vec::with_capacity(per) }
    }

    fn push(&mut self, mut l: &[f64], mut r: &[f64]) {
        while !l.is_empty() {
            let take = (self.per - self.bl.len()).min(l.len());
            self.bl.extend_from_slice(&l[..take]);
            self.br.extend_from_slice(&r[..take]);
            l = &l[take..];
            r = &r[take..];
            if self.bl.len() == self.per {
                self.l.push(block_stats(&self.bl));
                self.r.push(block_stats(&self.br));
                self.bl.clear();
                self.br.clear();
            }
        }
    }

    fn finish(mut self) -> (Vec<WaveSample>, Vec<WaveSample>) {
        if !self.bl.is_empty() {
            self.l.push(block_stats(&self.bl));
            self.r.push(block_stats(&self.br));
        }
        (self.l, self.r)
    }
}

/// The chain could not be built: no O values (not a spinner forever).
fn unavailable() -> OPart {
    OPart {
        metrics: [f64::NAN; METRIC_ARRAY_LEN],
        prov: [prov::UNAVAIL; METRIC_ARRAY_LEN],
        dr: Vec::new(),
        lufs_s: Vec::new(),
        lufs_m: Vec::new(),
    }
}

/// The O pass's streaming meters (also the selection statistics, zoom.rs).
pub(crate) struct Meters {
    loud: LoudnessMeter,
    tp: EburTruePeak,
    tp_engine: TruePeakScan,
    dr: DrLive,
    welch: WelchAccum,
    sp: f64,
    sp_at: usize,
    clips: u64,
    ll: f64,
    rr: f64,
    lr: f64,
    n: usize,
}

impl Meters {
    fn new(rate: u32, total: usize, l: usize) -> Meters {
        // The Welch length of the response frame's grid (65536 at the
        // source rate), so the measured O replaces the predicted one bin for bin.
        Meters::with_welch(rate, total, 65536 * l)
    }

    /// Meters for `total` samples at `rate`, the spectrum a Welch of `n`.
    pub(crate) fn with_welch(rate: u32, total: usize, n: usize) -> Meters {
        Meters {
            loud: LoudnessMeter::new(rate as f64),
            tp: EburTruePeak::new(rate),
            tp_engine: TruePeakScan::new(total),
            dr: DrLive::new(rate),
            welch: WelchAccum::new(n, n / 2, 28.0),
            sp: 0.0,
            sp_at: 0,
            clips: 0,
            ll: 0.0,
            rr: 0.0,
            lr: 0.0,
            n: 0,
        }
    }

    pub(crate) fn push(&mut self, l: &[f64], r: &[f64], at: usize) {
        self.loud.push(l, r);
        self.tp.push(l, r);
        self.tp_engine.push(l, r);
        self.dr.push(l, r);
        self.welch.push(l, r);
        self.n += l.len();
        for (i, (&a, &b)) in l.iter().zip(r).enumerate() {
            let p = a.abs().max(b.abs());
            if p > self.sp { self.sp = p; self.sp_at = at + i; }
            if p > 1.0 { self.clips += 1; }
            self.ll += a * a;
            self.rr += b * b;
            self.lr += a * b;
        }
    }

    /// The O slots, and the output spectrum (dBFS-sine, L/R mean).
    pub(crate) fn finish(self, rate: u32) -> (OPart, Vec<f64>) {
        let mut p = unavailable();
        let mut set = |id: usize, v: f64| {
            if v.is_finite() {
                p.metrics[id] = v;
                p.prov[id] = prov::MEASURED;
            }
        };
        let lufs_i = self.loud.integrated();
        let st: Vec<f64> = self.loud.short_term_series().iter().map(|&v| v as f64).collect();
        let tp_db = lin_to_db(self.tp.peak_max());
        set(mid::LUFS_I, lufs_i);
        set(mid::LUFS_S_LIVE, self.loud.max_short_term());
        set(mid::LUFS_M_LIVE, self.loud.max_momentary());
        set(mid::LRA, lra_ebu(&st).map(|r| r.lra).unwrap_or(f64::NAN));
        set(mid::TP_EBUR, tp_db);
        set(mid::TP_ENGINE, lin_to_db(self.tp_engine.finish()));
        set(mid::SP, self.sp);
        set(mid::PEAK_AT, self.sp_at as f64 / rate as f64);
        set(mid::GAIN_18LUFS, -18.0 - lufs_i);
        set(mid::CLIPS_OUT_X1, self.clips as f64);
        set(mid::TP_OVER0_EVENTS, self.tp.tp_over_events as f64);
        set(mid::PLR, tp_db - lufs_i);
        if self.ll > 0.0 && self.rr > 0.0 {
            set(mid::STEREO_CORR, self.lr / (self.ll * self.rr).sqrt());
        }
        // RMS (whole): both channels over everything measured; RMS (top-20 %):
        // the loudest fifth of DR's 3-second blocks.
        if self.n > 0 {
            set(mid::RMS, 10.0 * ((self.ll + self.rr) / (2 * self.n) as f64).max(1e-30).log10());
        }
        let dr = self.dr.result();
        if let Some(d) = &dr {
            let rms = lin_to_db(((d.l.rms_top.powi(2) + d.r.rms_top.powi(2)) * 0.5).sqrt());
            set(mid::DR, d.dr_exact);
            set(mid::RMS_TOP20, rms);
        }

        let (psd_l, psd_r, _) = self.welch.finish();
        let psd: Vec<f64> = psd_l.iter().zip(&psd_r).map(|(&a, &b)| {
            10.0 * ((10.0f64.powf(a / 10.0) + 10.0f64.powf(b / 10.0)) * 0.5).max(1e-240).log10()
        }).collect();
        if psd.len() > 1 {
            let f = rate as f64;
            let (up, ur) = band_level(&psd, 24_000.0, f / 2.0, f);
            let (_, ir) = band_level(&psd, 0.0, 20.0, f);
            set(mid::ULTRA_PEAK, up);
            set(mid::ULTRA_RMS, ur);
            set(mid::INFRA_RMS, ir);
            set(mid::EFF_BW, effective_bandwidth(&psd, f, -60.0));
        }
        p.metrics[mid::COVERAGE_O] = 100.0;
        p.prov[mid::COVERAGE_O] = prov::MEASURED;
        // The loudness series as S's: one count, the short-term one padded at
        // the front (its window is 3 s, the momentary one's 0.4 s).
        let m_series = self.loud.momentary_series();
        let st_series = self.loud.short_term_series();
        let lead = m_series.len().saturating_sub(st_series.len());
        let mut st_aligned = vec![f32::NAN; lead];
        st_aligned.extend_from_slice(&st_series);
        p.lufs_s = st_aligned;
        p.lufs_m = m_series;
        if let Some(d) = dr {
            p.dr = [&d.l, &d.r].iter().map(|c| DrChannel {
                peak_dbfs: lin_to_db(c.peak2) as f32,
                rms_dbfs: lin_to_db(c.rms_top) as f32,
                dr: c.dr_exact as f32,
            }).collect();
        }
        (p, psd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::analytics::proto::{TileKey, TileKind};
    use crate::player::analytics::spectra::SliceSource;

    /// The streamed O tiles are the tiles of the whole signal, byte for
    /// byte, whatever the blocks.
    #[test]
    fn streamed_output_tiles_are_the_whole_signals() {
        use rand::{Rng, SeedableRng};
        let (src_rate, lf) = (44_100u32, 2usize);
        let n_src = 90_000;
        let total = n_src * lf;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(11);
        let l: Vec<f64> = (0..total).map(|_| rng.gen_range(-0.4..0.4)).collect();
        let r: Vec<f64> = (0..total).map(|_| rng.gen_range(-0.4..0.4)).collect();
        let subject = Subject::new_live(u32::MAX - 5);
        let spec = OSpec::new(src_rate, src_rate * lf as u32, n_src, lf, subject.gen_o());
        let fed_all = AtomicBool::new(false);
        let (tx, rx) = sync_channel::<(Vec<f64>, Vec<f64>)>(4);
        std::thread::scope(|sc| {
            sc.spawn(|| spec.run(&subject, rx, &fed_all));
            let mut at = 0;
            for (k, n) in [8192usize, 1000, 5000, 8192].iter().cycle().enumerate() {
                if at >= total { break; }
                let e = (at + n + k % 3).min(total);
                tx.send((l[at..e].to_vec(), r[at..e].to_vec())).unwrap();
                at = e;
            }
            fed_all.store(true, Ordering::Release);
            drop(tx);
        });
        let whole = spec.engine.columns(&SliceSource { l: &l, r: &r }, 0, spec.n_cols, &mut SpecScratch::default());
        assert!(spec.n_tiles >= 2);
        for i in 0..spec.n_tiles {
            let key = TileKey { src: TileSrc::O, kind: TileKind::Spec, lod: 0, idx: i as u32 };
            let tile = subject.get_tile(&key).unwrap_or_else(|| panic!("tile {i} missing"));
            let c0 = i * SPEC_TILE_COLS;
            let want: Vec<u8> = whole[c0..(c0 + SPEC_TILE_COLS).min(spec.n_cols)].concat();
            assert_eq!(&tile[24..], &want[..], "tile {i}");
        }
    }
}
