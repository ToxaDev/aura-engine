//! Whole-track S/B analysis background thread.
//!
//! One thread per active subject, running at below-normal priority. The
//! thread waits for a [`TrackWork`] from the subject's work queue (set by
//! `Subject::set_variant`), then runs five passes in fast-to-slow order:
//!
//! 1. **Stats pass** — SP, clips, DC offset, band levels (infra/ultra).
//! 2. **Loudness pass** — K-filter → LUFS-I/M/S, LRA, loudness histograms.
//! 3. **True-peak pass** — EBU BS.1770 TP and engine TP.
//! 4. **Mipmap pass** — min/max/rms blocks of 256 samples per channel.
//! 5. **Welch pass** — PSD using Kaiser β=28, N=65536; then the
//!    spectrogram tiles (S and B), on a few threads.
//!
//! Between each pass the thread checks the generation counter; on mismatch
//! it immediately drops the `Arc<Variant>` and waits for new work.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::proto::{
    DrBlock, DrChannel, SourcePsd, SpecInfo,
    TileSrc,
    TrackSummary, WaveSample, WaveTile, SpectTile,
    mid, prov, METRIC_ARRAY_LEN,
};
use super::subject::{Subject, TrackWork, Work};
use crate::player::analytics::{
    dr::compute_dr,
    lra::lra_ebu,
    loudness::LoudnessMeter,
    peaks::{EburTruePeak, clip_report, engine_true_peak, lin_to_db, sample_peak},
    spectra::{
        SliceSource, SpecEngine, SpecScratch, SpecSource, WelchAccum, band_level, effective_bandwidth,
        spec_ln_step, SPEC_DB_FLOOR, SPEC_DB_RANGE, SPEC_LOG_BINS,
    },
    stats::compute_stats,
};

// ─── Priority helper ──────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub(crate) fn set_below_normal_priority() {
    unsafe {
        winapi::um::processthreadsapi::SetThreadPriority(
            winapi::um::processthreadsapi::GetCurrentThread(),
            winapi::um::winbase::THREAD_PRIORITY_BELOW_NORMAL as i32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn set_below_normal_priority() {}

// ─── Spawn ────────────────────────────────────────────────────────────────────

/// Spawn a background analysis thread for `subject`.
pub fn spawn_for_subject(subject: Arc<Subject>) {
    let sid = subject.sid;
    std::thread::Builder::new()
        .name(format!("aura-analytics-track-{sid}"))
        .spawn(move || {
            set_below_normal_priority();
            track_loop(subject);
        })
        .expect("failed to spawn analytics track thread");
}

// ─── Thread main loop ─────────────────────────────────────────────────────────

fn track_loop(subject: Arc<Subject>) {
    loop {
        let work = match subject.wait_for_work() {
            Some(Work::Track(w)) => w,
            Some(Work::O(w)) => {
                if w.generation == subject.gen_o() {
                    subject.running_o_track.store(w.track_id, Ordering::Release);
                    super::opass::run(&subject, w);
                    subject.running_o_track.store(u64::MAX, Ordering::Release);
                }
                continue;
            }
            None => break,
        };
        if work.generation != subject.generation_for(work.is_b) {
            drop(work.variant);
            continue;
        }
        analyze_variant(&subject, work);
    }
}

// ─── Analysis driver ─────────────────────────────────────────────────────────

fn analyze_variant(subject: &Subject, work: TrackWork) {
    let gen = work.generation;
    let b = work.is_b;
    let variant = work.variant;
    // The B pass measures the variant heard (source stages applied) with
    // the same code; its results go to the B slots, its tiles nowhere.
    let stale = |subject: &Subject, gen: u64| subject.generation_for(b) != gen;
    let publish = |summary: &TrackSummary| {
        if b { subject.publish_b_part(summary) } else { subject.publish_track_frame(summary) }
    };

    let src = variant.src.clone();
    let rate = src.rate;
    let l = src.l.as_slice();
    let r = src.r.as_slice();

    // Build initial summary — all S slots COMPUTING; B/O slots UNAVAIL.
    let mut summary = TrackSummary {
        chain_str: variant.tokens.join("·"),
        s_prov: [prov::COMPUTING; METRIC_ARRAY_LEN],
        b_prov: [prov::UNAVAIL;  METRIC_ARRAY_LEN],
        o_prov: [prov::UNAVAIL;  METRIC_ARRAY_LEN],
        ..TrackSummary::default()
    };

    // ── Pass 1: Stats ─────────────────────────────────────────────────────

    let ss = compute_stats(l, r, None, None);
    if stale(subject, gen) { return; }

    let cr = clip_report(l, r, ss.bit_depth);
    let (sp_l, sp_r) = sample_peak(l, r);
    let sp_lin = sp_l.max(sp_r);
    let peak_at_s = peak_time(l, r, rate);
    let dc_pct = (ss.l.dc_offset.abs().max(ss.r.dc_offset.abs()) * 100.0).clamp(0.0, 100.0);
    let (ultra_peak, ultra_rms, infra_rms) = band_levels_quick(l, r, rate as f64);

    // analytics: E1 — all values computed from real S signal are MEASURED, not ANALYTIC
    set_s(&mut summary, mid::SP,          sp_lin,                 prov::MEASURED);
    set_s(&mut summary, mid::PEAK_AT,     peak_at_s,              prov::MEASURED);
    set_s(&mut summary, mid::CLIPS_GE2,   cr.rail_runs as f64,    prov::MEASURED);
    set_s(&mut summary, mid::CLIPS_GE17,  cr.runs_ge_17 as f64,   prov::MEASURED);
    set_s(&mut summary, mid::DC_OFFSET,   dc_pct,                 prov::MEASURED);
    set_s(&mut summary, mid::ULTRA_PEAK,  ultra_peak,             prov::MEASURED);
    set_s(&mut summary, mid::ULTRA_RMS,   ultra_rms,              prov::MEASURED);
    set_s(&mut summary, mid::INFRA_RMS,   infra_rms,              prov::MEASURED);
    set_s(&mut summary, mid::STEREO_CORR, ss.correlation,         prov::MEASURED);

    summary.clips_s = cr.first_positions.iter().take(4096)
        .map(|&(offset, run_len, _ch)| super::proto::ClipEvent {
            start_sample: offset.min(u32::MAX as u64) as u32,
            run_len: run_len.min(u16::MAX as u32) as u16,
        })
        .collect();

    summary.duration_s = if rate > 0 { l.len() as f32 / rate as f32 } else { 0.0 };
    summary.src_rate = rate;
    summary.spec_hop = spec_hop(l.len()) as u32;
    summary.rev = 1;
    publish(&summary);

    // ── Pass 2: Loudness + LRA ────────────────────────────────────────────

    if stale(subject, gen) { return; }

    let mut meter = LoudnessMeter::new(rate as f64);
    const CHUNK: usize = 44100 * 4;
    let n = l.len();
    let mut off = 0;
    while off < n {
        let end = (off + CHUNK).min(n);
        meter.push(&l[off..end], &r[off..end]);
        off = end;
        if stale(subject, gen) { return; }
    }

    let lufs_i     = meter.integrated();
    let lufs_s_max = meter.max_short_term();
    let lufs_m_max = meter.max_momentary();
    let st_series  = meter.short_term_series();
    let m_series   = meter.momentary_series();
    let gain_18    = if lufs_i.is_finite() { -18.0 - lufs_i } else { f64::NAN };

    // One count on the wire for both series, index i = the same instant: the
    // short-term series starts 2.6 s after the momentary one (3 s window vs
    // 0.4 s), so it is padded at the front.
    let lead = m_series.len().saturating_sub(st_series.len());
    let mut st_aligned = vec![f32::NAN; lead];
    st_aligned.extend_from_slice(&st_series);
    summary.lufs_s_series_s = st_aligned;
    summary.lufs_m_series_s = m_series;

    let lra = lra_ebu(&st_series.iter().map(|&v| v as f64).collect::<Vec<_>>())
        .map(|r| r.lra)
        .unwrap_or(f64::NAN);

    summary.hist_s = lufs_histogram(&st_series);

    // analytics: E1 — LUFS-I, LRA, GAIN computed from real S signal → MEASURED
    set_s(&mut summary, mid::LUFS_I,      lufs_i,     prov::MEASURED);
    set_s(&mut summary, mid::LUFS_S_LIVE, lufs_s_max, prov::MEASURED);
    set_s(&mut summary, mid::LUFS_M_LIVE, lufs_m_max, prov::MEASURED);
    set_s(&mut summary, mid::LRA,         lra,        prov::MEASURED);
    set_s(&mut summary, mid::GAIN_18LUFS, gain_18,    prov::MEASURED);

    // ── DR (TT) ───────────────────────────────────────────────────────────

    if stale(subject, gen) { return; }

    if let Some(dr) = compute_dr(l, r, rate) {
        let dr_val  = dr.dr_exact;
        // Overall RMS = RMS of the L/R rms_top values in linear.
        let rms_lin = ((dr.l.rms_top.powi(2) + dr.r.rms_top.powi(2)) * 0.5).sqrt();
        let rms_db  = lin_to_db(rms_lin);

        // analytics: E1 — DR/RMS from real S signal → MEASURED
        // RMS (whole): both channels over the whole track; RMS (top-20 %):
        // the loudest fifth of DR's 3-second blocks.
        let pw = |db: f64| 10f64.powf(db / 10.0);
        let rms_whole = 10.0 * ((pw(ss.l.rms_dbfs) + pw(ss.r.rms_dbfs)) * 0.5).max(1e-30).log10();
        set_s(&mut summary, mid::DR,        dr_val, prov::MEASURED);
        set_s(&mut summary, mid::RMS,       rms_whole, prov::MEASURED);
        set_s(&mut summary, mid::RMS_TOP20, rms_db, prov::MEASURED);

        summary.dr_s = vec![
            DrChannel {
                peak_dbfs: lin_to_db(dr.l.peak2) as f32,
                rms_dbfs:  lin_to_db(dr.l.rms_top) as f32,
                dr:        dr.l.dr_exact as f32,
            },
            DrChannel {
                peak_dbfs: lin_to_db(dr.r.peak2) as f32,
                rms_dbfs:  lin_to_db(dr.r.rms_top) as f32,
                dr:        dr.r.dr_exact as f32,
            },
        ];

        // Per-block histogram data (L-channel blocks).
        summary.dr_blocks_s = dr.blocks_l.iter().take(2048)
            .map(|b| DrBlock {
                rms_dbfs:  lin_to_db(b.rms) as f32,
                peak_dbfs: lin_to_db(b.peak) as f32,
            })
            .collect();
    }

    summary.rev = 2;
    publish(&summary);

    // ── Pass 3: True peak ─────────────────────────────────────────────────

    if stale(subject, gen) { return; }

    // In 100 ms blocks (the loudness series' hop): the page shows the
    // true peak of the last 3 s at the playhead from this series.
    let mut tp_meter = EburTruePeak::new(rate);
    {
        let sub = ((rate as f64 * 0.1).round() as usize).max(1);
        let mut series = Vec::with_capacity(n / sub + 1);
        let mut off2 = 0;
        while off2 < n {
            let end = (off2 + sub).min(n);
            tp_meter.push(&l[off2..end], &r[off2..end]);
            series.push(lin_to_db(tp_meter.take_block_peak()) as f32);
            off2 = end;
            if series.len() % 400 == 0 && stale(subject, gen) { return; }
        }
        summary.tp100_s = series;
    }
    let tp_ebur   = tp_meter.peak_l_dbtp().max(tp_meter.peak_r_dbtp());
    let tp_engine = lin_to_db(engine_true_peak(l, r));
    let plr       = if lufs_i.is_finite() && tp_ebur.is_finite() {
        tp_ebur - lufs_i
    } else { f64::NAN };

    // analytics: E1 — TP/PLR/EFF_BW from real S signal → MEASURED
    set_s(&mut summary, mid::TP_EBUR,   tp_ebur,   prov::MEASURED);
    set_s(&mut summary, mid::TP_ENGINE, tp_engine, prov::MEASURED);
    set_s(&mut summary, mid::PLR,       plr,       prov::MEASURED);

    let eff_bw = quick_eff_bw(l, r, rate);
    set_s(&mut summary, mid::EFF_BW, eff_bw, prov::MEASURED);

    summary.rev = 3;
    publish(&summary);

    // ── Pass 4: Mipmap build ──────────────────────────────────────────────

    if stale(subject, gen) { return; }

    if !b {
        build_and_store_mipmaps(subject, l, r, TileSrc::S, gen);
    }
    if stale(subject, gen) { return; }

    let n_lods     = lod_count(n);
    let tile_samp: u16 = 256;
    let wt_counts  = wave_tile_counts(n, tile_samp as usize, n_lods);

    summary.n_lods             = n_lods as u8;
    summary.n_wave_channels    = 2;
    summary.tile_samples       = tile_samp;
    summary.wave_tile_counts_s = wt_counts;
    summary.wave_tile_counts_o = vec![0u32; n_lods];
    summary.spec_tile_counts_s = vec![0u32; 1];

    summary.rev = 4;
    publish(&summary);

    // ── Pass 5: Welch PSD ─────────────────────────────────────────────────

    if stale(subject, gen) { return; }

    const WELCH_N: usize = 65536;
    let hop = WELCH_N / 2;
    let mut welch = WelchAccum::new(WELCH_N, hop, 28.0);

    // Push in large sequential chunks (WelchAccum handles the overlap internally).
    const W_CHUNK: usize = WELCH_N * 8;
    let mut woff = 0;
    while woff < n {
        let wend = (woff + W_CHUNK).min(n);
        welch.push(&l[woff..wend], &r[woff..wend]);
        woff = wend;
        if stale(subject, gen) { return; }
    }

    let (psd_l, psd_r, _cross) = welch.finish();
    let psd_avg: Vec<f64> = psd_l.iter().zip(&psd_r).map(|(&a, &b)| {
        let pa = 10.0f64.powf(a / 10.0);
        let pb = 10.0f64.powf(b / 10.0);
        10.0 * ((pa + pb) * 0.5).max(1e-240).log10()
    }).collect();

    if psd_avg.len() > 1 {
        let rate_f = rate as f64;
        let (up, ur) = band_level(&psd_avg, 24_000.0, rate_f / 2.0, rate_f);
        let (_, ir)  = band_level(&psd_avg, 0.0, 20.0, rate_f);
        let eff      = effective_bandwidth(&psd_avg, rate_f, -60.0);

        // analytics: E1 — Welch band levels from real S signal → MEASURED
        set_s(&mut summary, mid::ULTRA_PEAK, up,  prov::MEASURED);
        set_s(&mut summary, mid::ULTRA_RMS,  ur,  prov::MEASURED);
        set_s(&mut summary, mid::INFRA_RMS,  ir,  prov::MEASURED);
        set_s(&mut summary, mid::EFF_BW,     eff, prov::MEASURED);
    }

    // The source spectrum for the S and B curves; the chain's |H| and O come
    // from resp.rs on their own grid.
    let psd = Arc::new(SourcePsd { dbfs: psd_avg, f_max_hz: rate as f32 / 2.0 });
    if b {
        subject.publish_b_psd(psd);
        #[cfg(not(test))]
        if let Some(hub) = super::hub::try_get() {
            hub.on_b_psd(subject.sid);
        }
    } else {
        subject.publish_source_psd(psd);
    }

    // The spectrogram: S's own, and B's for the S | B and B − S views.
    let tile_src = if b { TileSrc::B } else { TileSrc::S };
    if !build_spec_tiles(subject, &SliceSource { l, r }, n, rate, tile_src, gen) {
        return;
    }
    let spec_counts = spec_tile_counts(n);
    let tiles = spec_counts[0];
    subject.set_spec_info(tile_src, SpecInfo {
        gen: gen as u32, ready: tiles, total: tiles, n_bins: SPEC_LOG_BINS as u16,
    }, false);
    // The zoom's signal: S as f32, B the variant's own buffer (the player
    // holds it anyway while it plays).
    use super::zoom::{offer, own_f32, wants, ZoomSamples};
    let keep = wants(subject, n * 8, tile_src);
    let samples = match (keep, b) {
        (false, _) => None,
        (true, false) => Some(own_f32(rate, l, r)),
        (true, true) => Some(ZoomSamples::Shared(src.clone())),
    };
    offer(subject, tile_src, gen, samples, rate, 1);

    summary.spec_tile_counts_s = spec_counts;
    for p in summary.s_prov.iter_mut() {
        if *p == prov::COMPUTING { *p = prov::UNAVAIL; }
    }
    summary.rev = 5;
    publish(&summary);

    // analytics: notify the live thread that the full S analysis is complete
    // so it can set b_ready = true and light the B chevron in AAN1 flags.
    // Use try_send (no panic) in case this is called before hub is initialized.
    if b {
        super::live::try_send(super::live::LiveMsg::BReady);
    }

    // Arc<Variant> and Arc<SourceBuf> drop here.
    drop(src);
    drop(variant);
}

// ─── Mipmap build ────────────────────────────────────────────────────────────

fn build_and_store_mipmaps(
    subject: &Subject,
    l: &[f64],
    r: &[f64],
    src: TileSrc,
    gen: u64,
) {
    const BLOCK: usize = 256;
    let n = l.len();

    let mut lod_l: Vec<WaveSample> = Vec::with_capacity(n / BLOCK + 1);
    let mut lod_r: Vec<WaveSample> = Vec::with_capacity(n / BLOCK + 1);
    let mut off = 0;
    while off < n {
        let end = (off + BLOCK).min(n);
        lod_l.push(block_stats(&l[off..end]));
        lod_r.push(block_stats(&r[off..end]));
        off += BLOCK;
    }
    store_mipmaps(subject, lod_l, lod_r, lod_count(n), src, gen);
}

/// The waveform's levels from its finest entries (an entry per 256 source
/// samples), each next level 4× coarser, stored while pass `gen` is current.
pub(crate) fn store_mipmaps(
    subject: &Subject,
    mut lod_l: Vec<WaveSample>,
    mut lod_r: Vec<WaveSample>,
    n_lods: usize,
    src: TileSrc,
    gen: u64,
) {
    const TILE_ENTRIES: usize = 256;
    for lod in 0..n_lods {
        if subject.pass_gen(src) != gen { return; }
        let n_entries = lod_l.len();
        let n_tiles = (n_entries + TILE_ENTRIES - 1) / TILE_ENTRIES;
        for tile_idx in 0..n_tiles {
            let start = tile_idx * TILE_ENTRIES;
            let end = (start + TILE_ENTRIES).min(n_entries);
            let count = (end - start) as u32;

            let mut samples = Vec::with_capacity(count as usize * 2);
            for i in start..end {
                samples.push(lod_l[i].clone());
                samples.push(lod_r[i].clone());
            }

            let tile = WaveTile {
                src_flag: src.flag(),
                lod: lod as u8,
                n_channels: 2,
                tile_idx: tile_idx as u32,
                n_samples: count,
                samples,
            };
            subject.store_wave_tile_of(&tile, gen);
        }

        if lod + 1 < n_lods {
            lod_l = downsample_lod(&lod_l);
            lod_r = downsample_lod(&lod_r);
        }
    }
}

pub(crate) fn block_stats(s: &[f64]) -> WaveSample {
    if s.is_empty() {
        return WaveSample { min: 0.0, max: 0.0, rms: 0.0 };
    }
    let mut min = s[0] as f32;
    let mut max = s[0] as f32;
    let mut sum_sq = 0.0f64;
    for &x in s {
        let xf = x as f32;
        if xf < min { min = xf; }
        if xf > max { max = xf; }
        sum_sq += x * x;
    }
    WaveSample { min, max, rms: (sum_sq / s.len() as f64).sqrt() as f32 }
}

fn downsample_lod(entries: &[WaveSample]) -> Vec<WaveSample> {
    const FACTOR: usize = 4;
    let n_out = (entries.len() + FACTOR - 1) / FACTOR;
    let mut out = Vec::with_capacity(n_out);
    for chunk in entries.chunks(FACTOR) {
        let min = chunk.iter().map(|e| e.min).fold(f32::INFINITY, f32::min);
        let max = chunk.iter().map(|e| e.max).fold(f32::NEG_INFINITY, f32::max);
        let rms = (chunk.iter().map(|e| (e.rms as f64).powi(2)).sum::<f64>()
                   / chunk.len() as f64).sqrt() as f32;
        out.push(WaveSample { min, max, rms });
    }
    out
}

pub(crate) fn lod_count(n: usize) -> usize {
    let mut count = 1usize;
    let mut blocks = n / 256;
    while blocks > 256 { blocks /= 4; count += 1; }
    count.max(1)
}

pub(crate) fn wave_tile_counts(n: usize, tile_entries: usize, n_lods: usize) -> Vec<u32> {
    let mut counts = Vec::with_capacity(n_lods);
    let mut entries = (n + 255) / 256;
    for _ in 0..n_lods {
        let tiles = (entries + tile_entries - 1) / tile_entries;
        counts.push(tiles as u32);
        entries = (entries + 3) / 4;
    }
    counts
}

// ─── Spectrogram tiles ────────────────────────────────────────────────────────

/// Columns in a spectrogram tile.
pub(crate) const SPEC_TILE_COLS: usize = 64;

/// Threads for a track's spectrogram tiles: a quarter of the cores, 1 to 4
/// (below normal priority, so playback never waits for them).
pub(crate) fn spec_threads() -> usize {
    (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 4).clamp(1, 4)
}

/// A signal's spectrogram tiles on its own time grid (a column per
/// `spec_hop` samples), stored while its pass is current. False when the
/// pass was replaced meanwhile.
fn build_spec_tiles(
    subject: &Subject,
    src: &dyn SpecSource,
    n: usize,
    rate: u32,
    tile_src: TileSrc,
    gen: u64,
) -> bool {
    let hop = spec_hop(n);
    let engine = SpecEngine::new(rate, hop, SPEC_LOG_BINS, spec_ln_step(rate));
    let n_cols = n.div_ceil(hop);
    let n_tiles = n_cols.div_ceil(SPEC_TILE_COLS);
    let stale = || subject.pass_gen(tile_src) != gen || subject.is_shutdown();
    let next = AtomicUsize::new(0);
    let t0 = std::time::Instant::now();
    let threads = spec_threads().min(n_tiles.max(1));
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                set_below_normal_priority();
                let mut scratch = SpecScratch::default();
                loop {
                    let ti = next.fetch_add(1, Ordering::Relaxed);
                    if ti >= n_tiles || stale() { break; }
                    let c0 = ti * SPEC_TILE_COLS;
                    let cols = engine.columns(src, c0, (c0 + SPEC_TILE_COLS).min(n_cols), &mut scratch);
                    store_spec_tile(subject, tile_src, gen, ti as u32, &cols, engine.n_bands);
                }
            });
        }
    });
    if stale() {
        return false;
    }
    crate::aelog!(
        "[ANALYZER] {:?} spectrogram: {} tiles, hop {}, in {:.2} s on {} threads",
        tile_src, n_tiles, hop, t0.elapsed().as_secs_f64(), threads
    );
    true
}

/// One tile of columns (`n_bins` bytes each), kept while pass `gen` is current.
pub(crate) fn store_spec_tile(subject: &Subject, src: TileSrc, gen: u64, tile_idx: u32, cols: &[Vec<u8>], n_bins: usize) {
    let tile = SpectTile {
        src_flag: src.flag(),
        lod: 0,
        gen8: gen as u8,
        tile_idx,
        db_floor: SPEC_DB_FLOOR as f32,
        db_range: SPEC_DB_RANGE as f32,
        n_cols: cols.len() as u16,
        n_bins: n_bins as u16,
        data: cols.concat(),
    };
    subject.store_spect_tile_of(&tile, gen);
}

/// The spectrogram's hop: 256 samples, longer for a long track so it stays
/// within SPEC_MAX_COLS columns (32 KB a 64-column tile; ≤ 8 MB a track).
const SPEC_MAX_COLS: usize = 16_384;

pub(crate) fn spec_hop(n: usize) -> usize {
    256usize.max(n.div_ceil(SPEC_MAX_COLS))
}

/// One column per hop, the last one partial.
pub(crate) fn spec_tile_counts(n: usize) -> Vec<u32> {
    vec![n.div_ceil(spec_hop(n)).div_ceil(SPEC_TILE_COLS) as u32]
}

// ─── Band-level helpers ───────────────────────────────────────────────────────

fn band_levels_quick(l: &[f64], r: &[f64], rate: f64) -> (f64, f64, f64) {
    const N: usize = 65536;
    let n = l.len().min(N * 8);
    if n < N { return (f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY); }

    let mut welch = WelchAccum::new(N, N / 2, 28.0);
    welch.push(&l[..n], &r[..n]);
    let (pl, pr, _) = welch.finish();

    let avg: Vec<f64> = pl.iter().zip(&pr).map(|(&a, &b)| {
        let pa = 10.0f64.powf(a / 10.0);
        let pb = 10.0f64.powf(b / 10.0);
        10.0 * ((pa + pb) * 0.5).max(1e-240).log10()
    }).collect();

    let (up, ur) = band_level(&avg, 24_000.0, rate / 2.0, rate);
    let (_, ir)  = band_level(&avg, 0.0, 20.0, rate);
    (up, ur, ir)
}

fn quick_eff_bw(l: &[f64], r: &[f64], rate: u32) -> f64 {
    const N: usize = 65536;
    let n = l.len().min(N * 4);
    if n < N { return f64::NAN; }
    let mut welch = WelchAccum::new(N, N / 2, 28.0);
    welch.push(&l[..n], &r[..n]);
    let (pl, pr, _) = welch.finish();
    let avg: Vec<f64> = pl.iter().zip(&pr).map(|(&a, &b)| {
        let pa = 10.0f64.powf(a / 10.0);
        let pb = 10.0f64.powf(b / 10.0);
        10.0 * ((pa + pb) * 0.5).max(1e-240).log10()
    }).collect();
    effective_bandwidth(&avg, rate as f64, -60.0)
}

// ─── Loudness histogram ───────────────────────────────────────────────────────

/// The loudness histogram's bins: 256 over −80…0 LUFS.
pub(super) const LUFS_HIST_BINS: usize = 256;

/// The bin of a short-term loudness value in the loudness histogram, None
/// outside −80…0 LUFS (a stream's totals count theirs the same way).
pub(super) fn lufs_hist_bin(v: f32) -> Option<usize> {
    const LO: f32 = -80.0;
    const HI: f32 = 0.0;
    if !(v.is_finite() && v >= LO && v <= HI) { return None; }
    Some((((v - LO) / (HI - LO) * LUFS_HIST_BINS as f32).floor() as usize).min(LUFS_HIST_BINS - 1))
}

pub(super) fn lufs_histogram(series: &[f32]) -> Vec<f32> {
    let mut bins = vec![0u32; LUFS_HIST_BINS];
    let mut count = 0u32;
    for &v in series {
        if let Some(b) = lufs_hist_bin(v) {
            bins[b] += 1;
            count += 1;
        }
    }
    if count == 0 { return vec![0.0; LUFS_HIST_BINS]; }
    bins.iter().map(|&c| c as f32 / count as f32).collect()
}

// ─── Utilities ────────────────────────────────────────────────────────────────

fn peak_time(l: &[f64], r: &[f64], rate: u32) -> f64 {
    let mut best = 0.0f64;
    let mut best_idx = 0usize;
    for (i, (&lv, &rv)) in l.iter().zip(r.iter()).enumerate() {
        let v = lv.abs().max(rv.abs());
        if v > best { best = v; best_idx = i; }
    }
    best_idx as f64 / rate as f64
}

#[inline]
fn set_s(summary: &mut TrackSummary, id: usize, val: f64, code: u8) {
    summary.s_metrics[id] = val;
    summary.s_prov[id]    = code;
}

#[cfg(test)]
#[inline]
fn stale(subject: &Subject, gen: u64) -> bool {
    subject.generation() != gen
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::analytics::proto::{TileKey, TileKind};

    // ── block_stats ────────────────────────────────────────────────────────

    #[test]
    fn block_stats_min_max_rms() {
        let s = vec![0.5f64, -0.5, 0.0, 1.0, -1.0];
        let bs = block_stats(&s);
        assert_eq!(bs.min, -1.0f32);
        assert_eq!(bs.max, 1.0f32);
        let expected = ((0.25 + 0.25 + 0.0 + 1.0 + 1.0) / 5.0_f64).sqrt() as f32;
        assert!((bs.rms - expected).abs() < 1e-4);
    }

    #[test]
    fn block_stats_empty() {
        let bs = block_stats(&[]);
        assert_eq!(bs.min, 0.0);
        assert_eq!(bs.max, 0.0);
        assert_eq!(bs.rms, 0.0);
    }

    // ── lod_count ──────────────────────────────────────────────────────────

    #[test]
    fn lod_count_small() { assert_eq!(lod_count(256), 1); }

    #[test]
    fn lod_count_medium() {
        assert!(lod_count(5 * 60 * 44100) >= 2);
    }

    // ── downsample_lod ─────────────────────────────────────────────────────

    #[test]
    fn downsample_preserves_extremes() {
        let entries = vec![
            WaveSample { min: -1.0, max: 0.5, rms: 0.3 },
            WaveSample { min: -0.2, max: 1.0, rms: 0.4 },
            WaveSample { min: -0.5, max: 0.8, rms: 0.2 },
            WaveSample { min: -0.3, max: 0.6, rms: 0.35 },
        ];
        let down = downsample_lod(&entries);
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].min, -1.0f32);
        assert_eq!(down[0].max, 1.0f32);
    }

    // ── lufs_histogram ─────────────────────────────────────────────────────

    #[test]
    fn histogram_sums_to_one() {
        let series: Vec<f32> = (0..256).map(|i| -80.0 + i as f32 * 80.0 / 256.0).collect();
        let hist = lufs_histogram(&series);
        let sum: f32 = hist.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4, "sum={sum}");
    }

    // ── spectrogram tiles ──────────────────────────────────────────────────

    #[test]
    fn spec_tiles_cover_every_hop() {
        for n in [1usize, 255, 256, 257, 44100 * 10, 16_384 * 700 + 1] {
            let cols = n.div_ceil(spec_hop(n));
            assert!(cols <= SPEC_MAX_COLS, "n={n}: {cols} columns");
            assert_eq!(spec_tile_counts(n)[0] as usize, cols.div_ceil(64), "n={n}");
        }
        assert_eq!(spec_tile_counts(0)[0], 0);
    }

    // ── generation stale check ─────────────────────────────────────────────

    #[test]
    fn stale_detects_changed_generation() {
        let s = Subject::new_live(u32::MAX - 1);
        assert!(!stale(&s, 0));
        s.generation.fetch_add(1, Ordering::SeqCst);
        assert!(stale(&s, 0));
        assert!(!stale(&s, 1));
    }

    // ── wave_tile_counts ───────────────────────────────────────────────────

    #[test]
    fn tile_counts_reasonable() {
        let n = 44100 * 10;
        let lods = lod_count(n);
        let counts = wave_tile_counts(n, 256, lods);
        assert_eq!(counts.len(), lods);
        assert!(counts[0] > 0);
        for i in 1..lods { assert!(counts[i] <= counts[i-1]); }
    }

    // ── mipmaps build + tile lookup ────────────────────────────────────────

    #[test]
    fn mipmaps_build_and_store() {
        let s = Subject::new_live(u32::MAX - 1);
        let n = 256 * 10;
        let l = vec![0.5f64; n];
        let r = vec![-0.5f64; n];
        build_and_store_mipmaps(&s, &l, &r, TileSrc::S, 0);

        let key = TileKey { src: TileSrc::S, kind: TileKind::Wave, lod: 0, idx: 0 };
        assert!(s.get_tile(&key).is_some(), "LOD-0 tile must exist after mipmaps");
    }

    // ── peak_time ──────────────────────────────────────────────────────────

    #[test]
    fn peak_time_finds_max() {
        let mut l = vec![0.0f64; 44100];
        let r = vec![0.0f64; 44100];
        l[1000] = 0.9;
        let t = peak_time(&l, &r, 44100);
        assert!((t - 1000.0 / 44100.0).abs() < 1e-6);
    }
}
