use crate::audio::converter::apodize::{analyze_source, pool_analyses, SourceAnalysis};
use crate::audio::converter::decode::{decode_file, set_status};
use crate::audio::converter::process::{prepare_audio_phase, process_one_prepared};
use crate::audio::converter::state::*;
use crate::audio::converter::types::{ConvertSettings, PreparedAudio};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

/// Folder key for album pooling: the file's parent directory.
fn folder_key(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Album pre-scan (v3.1): decode + analyze every sibling of `folder` and
/// pool the measurements into one album-level SourceAnalysis. Returns None
/// when the folder is heterogeneous (compilation) or too few tracks could
/// be analyzed — callers then keep per-track verdicts.
///
/// Cost: one extra decode per file (the analysis itself is ~0.2 s per
/// 4-minute track). The prep thread overlaps the previous files' GPU work,
/// so in batches only the very first folder's scan is on the critical path.
fn album_prescan(
    folder: &str,
    paths: &[String],
    settings: &ConvertSettings,
) -> Option<SourceAnalysis> {
    let siblings: Vec<&String> = paths
        .iter()
        .filter(|p| folder_key(p) == folder)
        .collect();
    if siblings.len() < 2 {
        return None;
    }
    let no_cancel = AtomicBool::new(false); // global cancel still honored inside
    let gain = if settings.headroom_db < 0.0 {
        10.0f64.powf(settings.headroom_db / 20.0)
    } else {
        1.0
    };
    let mut analyses: Vec<SourceAnalysis> = Vec::with_capacity(siblings.len());
    for (i, p) in siblings.iter().enumerate() {
        if CONV_CANCEL.load(Ordering::Relaxed) {
            break;
        }
        set_status(&format!(
            "Adaptive Apodizer: album analysis {}/{}...",
            i + 1,
            siblings.len()
        ));
        crate::audio::memory::await_free_ram(2500, "Album Analysis");
        let mut audio = match decode_file(std::path::Path::new(p.as_str())) {
            Ok(a) => a,
            Err(_) => continue, // the file will report its own error in prepare
        };
        if gain != 1.0 {
            // Same pre-DSP headroom the per-track path applies before its
            // analysis, so absolute thresholds (attack level, HF floor)
            // see identical material.
            for s in audio.samples_l.iter_mut() {
                *s *= gain;
            }
            for s in audio.samples_r.iter_mut() {
                *s *= gain;
            }
        }
        if let Some(a) =
            analyze_source(&audio.samples_l, &audio.samples_r, audio.sample_rate, &no_cancel)
        {
            analyses.push(a);
        }
    }
    let pooled = pool_analyses(&analyses);
    crate::aelog!(
        "[CONV] Adaptive Apodizer: album pool for '{}': {}/{} tracks analyzed → {}",
        folder,
        analyses.len(),
        siblings.len(),
        match &pooled {
            Some(p) => format!(
                "pooled verdict basis ({} attacks, {:.0}% ringing, ring freq {})",
                p.attacks_analyzed,
                p.ring_fraction * 100.0,
                p.ring_freq_hz
                    .map(|f| format!("{:.0} Hz", f))
                    .unwrap_or_else(|| "n/a".to_string())
            ),
            None => "heterogeneous or insufficient — per-track verdicts".to_string(),
        }
    );
    pooled
}

/// Estimate the peak RAM (MB) one file needs through its heavy stage
/// (convolution + trim + optional hybrid blend + streaming verify). Used by
/// the admission gate in `memory::reserve_ram` so a second worker doesn't
/// start a file whose peak won't fit next to the files already in flight.
///
/// Mirrors the allocation pattern of `process_one_prepared`: the dominant
/// terms are 1× (non-hybrid) or 2× (hybrid) full-length output-rate stereo
/// f64 buffers; the CPU convolver additionally holds ~96 bytes/tap of
/// FFT-domain filter + delay-line state across the parallel phases.
fn estimate_peak_ram_mb(prep: &PreparedAudio, settings: &ConvertSettings) -> u64 {
    const MB: u64 = 1024 * 1024;
    let n_in = prep.total_input_samples as u64;
    let in_mb = n_in * 16 / MB; // stereo f64 at source rate
    let ratio = if prep.sample_rate > 0 && settings.out_rate > prep.sample_rate {
        (settings.out_rate / prep.sample_rate) as u64
    } else {
        1
    };
    let integer_ratio =
        ratio > 1 && prep.sample_rate > 0 && settings.out_rate % prep.sample_rate == 0;
    let polyphase = settings.use_fir_resampling && integer_ratio && settings.taps > 0;
    let out_mb = n_in * ratio * 16 / MB; // stereo f64 at output rate
    let filter_mb = settings.taps as u64 * 8 / MB; // full f64 coefficient blob
    // CPU convolver state (h_blocks + 2 delay lines ≈ 96 B/tap summed over
    // all parallel phases); the GPU keeps this in VRAM instead.
    let conv_mb = if settings.use_gpu {
        0
    } else {
        settings.taps as u64 * 96 / MB
    };
    let peak = if polyphase {
        if crate::audio::converter::pipeline::segmented::is_giant_plan(
            prep.total_input_samples,
            ratio as u32,
            settings.hybrid_phase,
        ) {
            // Giant files take the SEGMENTED path: intermediates live in
            // temp files, RAM holds the source + segment buffers + filter.
            in_mb + filter_mb * 2 + conv_mb + 2048
        } else if settings.hybrid_phase {
            // linear out + min out + f32 envelope + source (until HPSS) + filter copies
            out_mb * 2 + out_mb / 4 + in_mb + filter_mb * 2 + conv_mb
        } else {
            out_mb + in_mb + filter_mb * 2 + conv_mb
        }
    } else {
        // Standard path (rubato + FIR post-filter): resampled input and
        // convolved output coexist; hybrid additionally keeps the saved
        // resampled copy and the min-phase output.
        if settings.hybrid_phase {
            out_mb * 4 + in_mb * 2 + filter_mb + conv_mb
        } else {
            out_mb * 2 + in_mb + filter_mb + conv_mb
        }
    };
    peak + peak / 5 + 256 // +20% + fixed margin for chunk/scratch buffers
}

/// Start converting a batch of files in a background thread
pub fn convert_files(paths: Vec<String>, mut settings: ConvertSettings) -> Result<(), String> {
    // Atomic check-and-set: two simultaneous IPC calls must not both pass a
    // separate load() gate and stomp the shared conversion state together.
    if CONV_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("Conversion already in progress".to_string());
    }
    CONV_CANCEL.store(false, Ordering::Relaxed);
    CONV_CANCEL_FILE.store(false, Ordering::Relaxed);
    crate::audio::cancel_flag::set(false); // sync shared cancel flag

    CONV_PROGRESS.store(0, Ordering::Relaxed);
    CONV_SNAPPED_RATE.store(0, Ordering::Relaxed);
    *CONV_OUTPUT.lock().unwrap() = String::new();

    // Whatever else was holding the card during the last batch may be gone,
    // so the ceiling a refusal taught the admission gate does not carry over.
    crate::audio::gpu::vram_admission::reset_oom_floor();

    // GPU asked for, GPU not available: run on the CPU rather than fail.
    //
    // Decided once, here, and written back into the settings the whole batch
    // is built from — so the worker count, the memory estimate and all five
    // places that construct a convolver see one answer instead of finding out
    // separately, per file, after allocating. Falling back costs time and
    // nothing else: the CPU path is the f64 reference the GPU path is checked
    // against, so the output is not the compromise here, the wall clock is.
    if settings.use_gpu {
        if let Err(why) = crate::audio::gpu::context::probe() {
            crate::aelog!("[GPU] Requested, but unavailable: {}", why);
            crate::aelog!(
                "[GPU] Running this batch on the CPU reference path instead. \
                 It is slower and bit-for-bit the path the engine is specified against."
            );
            settings.use_gpu = false;
        }
    }

    // Batch header in the session log: full settings + queue, so a run can
    // be reconstructed from the log file alone.
    crate::aelog!(
        "[CONV] ═══ Batch start: {} file(s) | FS×{} | taps={} | precision={} | GPU={} | FIR-resampling={} | static apod={} | AA={} | hybrid-phase={} | headroom={} dB | IIR DC={} ═══",
        paths.len(),
        settings.fs_multiplier,
        settings.taps,
        settings.precision,
        settings.use_gpu,
        settings.use_fir_resampling,
        settings.apodizing,
        settings.adaptive_apodizer,
        settings.hybrid_phase,
        settings.headroom_db,
        settings.iir_dc_blocking
    );
    for (i, p) in paths.iter().enumerate() {
        crate::aelog!("[CONV]   queue[{:02}]: {}", i + 1, p);
    }

    // Heartbeat: a self-overwriting status line at the bottom of stdout
    // showing current local time + how long since the last log line. Lets
    // you see at a glance whether a long-running step is still alive or
    // genuinely hung. Stopped at the bottom of the worker thread below.
    crate::audio::logging::start_heartbeat();
    crate::audio::memory::log_process_memory("batch start");
    // Drop spectra cached by the previous batch: a filter regenerated by
    // fir-optimizer between runs must not be served from a stale entry, and
    // an idle app should not sit on gigabytes of them.
    crate::audio::gpu::filter_cache::clear();

    // Initialise per-file state
    {
        let mut states = CONV_FILE_STATES.lock().unwrap();
        *states = (0..paths.len())
            .map(|_| Arc::new(FileConvState::new()))
            .collect();
    }
    let total = paths.len();

    thread::spawn(move || {
        let settings_clone = settings.clone(); // for prep thread

        // Bounded channel: prep thread can be at most `num_gpu_workers` files ahead.
        let num_gpu_workers = if settings_clone.use_gpu {
            // GPU mode: cap at 2 simultaneous files.
            //
            // VRAM safety is NOT decided here any more. It used to be, via
            // recommended_gpu_workers(settings.taps) — but on the polyphase
            // path a convolver is built from one sub-filter (taps / L), not
            // the whole filter, so that estimate overshot by a factor of L
            // and pinned every ×8 batch to a single worker (measured: 3136 MB
            // estimated vs 416 MB actually allocated). It could not be fixed
            // in place either: L depends on each file's source rate, which is
            // unknown until the file is decoded.
            //
            // The budget is now enforced where the real number is known — at
            // buffer allocation, by gpu::vram_admission. A second worker that
            // genuinely cannot fit simply waits there, so the worst case is
            // the old serialised behaviour rather than an out-of-memory.
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
            // Two was a conservative number, not a measured one. Measured now,
            // on 6 files at 30M taps with hybrid-phase, i9-14900K + RTX 4090:
            //
            //     1 worker  105.2 s
            //     2 workers  74.3 / 74.9 s
            //     3 workers  66.2 / 65.6 s   <- 12 % better than two
            //     4 workers  71.0 / 65.8 s   <- no gain, and the spread grows
            //
            // Three is raised only where there are cores to feed it. That
            // threshold is where the measurement was taken and nowhere else:
            // what a four-core laptop wants is not known, so it keeps two.
            // Neither number is a memory decision — `reserve_ram` and
            // `gpu::vram_admission` throttle a worker that does not fit, so a
            // machine short of RAM or VRAM simply makes the extra worker wait
            // rather than swapping or failing.
            let default_workers = if cores >= 16 { 3 } else { 2 }.clamp(1, cores.max(1));
            // Same affordance as AURA_NO_GPU: lets the number be measured on a
            // machine other than this one without a rebuild. Not documented
            // anywhere a user would read.
            std::env::var("AURA_GPU_WORKERS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n >= 1)
                .unwrap_or(default_workers)
        } else {
            // CPU mode: max 4 threads to prevent system hanging
            std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).clamp(1, 4)
        };
        let tx_prep_bound = num_gpu_workers.max(1);
        let (tx_prep, rx_prep) = std::sync::mpsc::sync_channel::<
            Result<(usize, PreparedAudio, u32), (usize, String)>,
        >(tx_prep_bound);

        let paths_clone = paths.clone();
        // ── Prep thread: decode + headroom + apodize (CPU) ──
        let prep_handle = thread::spawn(move || {
            // Album pool cache (v3.1): folder → pooled analysis (or None
            // when the folder refused to pool). Filled lazily when the
            // first file of a folder is reached.
            let mut album_pool: HashMap<String, Option<SourceAnalysis>> = HashMap::new();
            for (idx, path_str) in paths_clone.iter().enumerate() {
                // Ensure sufficient RAM (reserve ~2.5 GB buffer required to load decoded f64)
                crate::audio::memory::await_free_ram(2500, "Decoding Thread");

                // RAM admission for PREPARATION. A very long file (e.g. a
                // 38-minute 192 kHz album rip) needs tens of GB just to
                // decode + clone + apodize; without this ticket the prep
                // thread balloons IN PARALLEL with the worker's current
                // file (observed: 42.9 GB working set while converting a
                // 3-minute track). Probing the frame count costs
                // milliseconds; ~3.5× the decoded stereo f64 size covers
                // decode growth, the working clones and apodize buffers.
                let _prep_ticket = crate::audio::converter::decode::probe_input_frames(
                    std::path::Path::new(path_str.as_str()),
                )
                .map(|(frames, _rate, _channels)| {
                    let mb = frames.saturating_mul(56) / (1024 * 1024); // ≈3.5 × frames×16B
                    crate::audio::memory::reserve_ram(
                        mb.max(512),
                        &format!("prep [{}/{}]", idx + 1, paths_clone.len()),
                    )
                });

                // Stop on global cancel (not single-file cancel)
                if CONV_CANCEL.load(Ordering::Relaxed) && !CONV_CANCEL_FILE.load(Ordering::Relaxed)
                {
                    break;
                }

                // Skip files already cancelled before they were reached
                let pre_cancelled = {
                    let st = CONV_FILE_STATES.lock().unwrap();
                    st.get(idx).map(|s| s.is_cancelled()).unwrap_or(false)
                };
                if pre_cancelled {
                    if tx_prep.send(Err((idx, "Cancelled".into()))).is_err() {
                        break;
                    }
                    continue;
                }

                {
                    let st = CONV_FILE_STATES.lock().unwrap();
                    if let Some(s) = st.get(idx) {
                        s.set_stage(STAGE_PREPARING);
                    }
                }

                // Capture per-file cancel flag so apodizing loops can be interrupted by X button
                let file_state_arc: Option<Arc<FileConvState>> = {
                    let st = CONV_FILE_STATES.lock().unwrap();
                    st.get(idx).map(Arc::clone)
                };
                let no_cancel = AtomicBool::new(false);
                let file_cancel: &AtomicBool = match &file_state_arc {
                    Some(arc) => &arc.cancelled,
                    None => &no_cancel,
                };

                // Album pooling: scan this file's folder once, then hand
                // every sibling the same pooled evidence.
                let pooled_ref: Option<&SourceAnalysis> = if settings_clone.adaptive_apodizer {
                    let folder = folder_key(path_str);
                    if !album_pool.contains_key(&folder) {
                        let pooled = album_prescan(&folder, &paths_clone, &settings_clone);
                        album_pool.insert(folder.clone(), pooled);
                    }
                    album_pool.get(&folder).and_then(|o| o.as_ref())
                } else {
                    None
                };

                let src = std::path::Path::new(path_str.as_str());
                let mut file_settings = settings_clone.clone();
                let msg = match prepare_audio_phase(src, &mut file_settings, file_cancel, pooled_ref)
                {
                    Ok(p) => Ok((idx, p, file_settings.out_rate)),
                    Err(e) => Err((idx, e)),
                };
                // Surface the AA verdict to the queue UI as soon as prepare
                // finishes (the GPU stage does not change it).
                if let Ok((_, ref p, _)) = msg {
                    if let (Some(st), Some((treated, note))) =
                        (&file_state_arc, p.aa_ui.as_ref())
                    {
                        st.aa_state.store(
                            if *treated { AA_TREATED } else { AA_SKIPPED },
                            Ordering::Relaxed,
                        );
                        *st.aa_note.lock().unwrap() = note.clone();
                        st.touch(); // AA verdict has no set_stage — bump the queue rev explicitly
                    }
                }
                if tx_prep.send(msg).is_err() {
                    break;
                } // receiver gone → exit
            }
        });

        // ── GPU threads (worker pool): convolution + encoding ──
        let rx_prep = Arc::new(std::sync::Mutex::new(rx_prep));
        let mut worker_handles = Vec::with_capacity(num_gpu_workers);

        for _ in 0..num_gpu_workers {
            let rx = Arc::clone(&rx_prep);
            let paths = paths.clone();
            let settings = settings.clone();

            let h = thread::spawn(move || {
                loop {
                    let msg = {
                        match rx.lock().unwrap().recv() {
                            Ok(m) => m,
                            Err(_) => break, // tx dropped, queue empty
                        }
                    };

                    // Global cancel?
                    if CONV_CANCEL.load(Ordering::Relaxed)
                        && !CONV_CANCEL_FILE.load(Ordering::Relaxed)
                    {
                        break;
                    }

                    match msg {
                        Ok((idx, prep, resolved_out_rate)) => {
                            // File cancelled while being prepared?
                            let already = {
                                let st = CONV_FILE_STATES.lock().unwrap();
                                if let Some(s) = st.get(idx) {
                                    if s.is_cancelled() {
                                        s.set_stage(STAGE_CANCELLED);
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            if already {
                                continue;
                            }

                            let file_state = {
                                let st = CONV_FILE_STATES.lock().unwrap();
                                st.get(idx).map(Arc::clone)
                            };

                            if let Some(s) = &file_state {
                                s.set_stage(STAGE_GPU_CONV);
                                s.gpu_pct.store(0, Ordering::Relaxed);
                            }

                            set_status(&format!("[{}/{}] Processing...", idx + 1, total));

                            // Build a per-file settings copy with the resolved out_rate
                            let mut per_file_settings = settings.clone();
                            per_file_settings.out_rate = resolved_out_rate;
                            CONV_SNAPPED_RATE.store(resolved_out_rate, Ordering::Relaxed);

                            // RAM admission: hold a reservation for this
                            // file's estimated peak until it is fully done
                            // (encode + verify included). A second worker
                            // waits here instead of ballooning alongside.
                            let est_mb = estimate_peak_ram_mb(&prep, &per_file_settings);

                            // Hard cap: a file whose estimated peak cannot
                            // fit in physical RAM would push the system
                            // into swap-death (the solo-admission rule
                            // would still let it start). Refuse it with an
                            // actionable message instead.
                            let hard_cap_mb =
                                crate::audio::memory::total_ram_mb().saturating_sub(6144);
                            if est_mb > hard_cap_mb {
                                let msg = format!(
                                    "File too long for in-RAM conversion: needs ~{:.0} GB at FS×{}{} \
                                     (budget ~{:.0} GB). Lower the FS multiplier or disable \
                                     Hybrid-Phase for this file.",
                                    est_mb as f64 / 1024.0,
                                    per_file_settings.fs_multiplier,
                                    if per_file_settings.hybrid_phase {
                                        " with Hybrid-Phase"
                                    } else {
                                        ""
                                    },
                                    hard_cap_mb as f64 / 1024.0
                                );
                                crate::aelog!(
                                    "[CONV] ✗ [{}/{}] refused: estimated peak {} MB > RAM budget {} MB",
                                    idx + 1,
                                    total,
                                    est_mb,
                                    hard_cap_mb
                                );
                                {
                                    let st = CONV_FILE_STATES.lock().unwrap();
                                    if let Some(s) = st.get(idx) {
                                        s.set_stage(STAGE_ERROR);
                                        *s.error_msg.lock().unwrap() = msg.clone();
                                    }
                                }
                                set_status(&format!("\u{2717} Error [{}]: {}", idx + 1, msg));
                                continue;
                            }

                            let _ram_ticket = crate::audio::memory::reserve_ram(
                                est_mb,
                                &format!("[{}/{}]", idx + 1, total),
                            );

                            let src = std::path::Path::new(paths[idx].as_str());
                            let process_result = process_one_prepared(
                                src,
                                prep,
                                &per_file_settings,
                                file_state.unwrap_or_else(|| Arc::new(FileConvState::new())),
                            );
                            crate::audio::memory::log_process_memory(&format!(
                                "file {}/{} finished",
                                idx + 1,
                                total
                            ));
                            match process_result {
                                Ok(out) => {
                                    crate::aelog!(
                                        "[CONV] ✓ [{}/{}] done → {}",
                                        idx + 1,
                                        total,
                                        out
                                    );
                                    {
                                        let st = CONV_FILE_STATES.lock().unwrap();
                                        if let Some(s) = st.get(idx) {
                                            s.set_stage(STAGE_DONE);
                                            *s.output_path.lock().unwrap() = out.clone();
                                        }
                                    }
                                    *CONV_OUTPUT.lock().unwrap() = out;
                                }
                                Err(ref e) if e == "Cancelled" => {
                                    let was_single_file = CONV_CANCEL_FILE.load(Ordering::Relaxed);
                                    {
                                        let st = CONV_FILE_STATES.lock().unwrap();
                                        if let Some(s) = st.get(idx) {
                                            s.set_stage(STAGE_CANCELLED);
                                        }
                                    }
                                    if was_single_file {
                                        // Single-file cancel: clear flags so the NEXT file can start.
                                        CONV_CANCEL.store(false, Ordering::Relaxed);
                                        CONV_CANCEL_FILE.store(false, Ordering::Relaxed);
                                        crate::audio::cancel_flag::set(false); // allow next file's h_blocks
                                    }

                                    // If global cancel: leave CONV_CANCEL=true →
                                    // the while-loop's check at the top will break cleanly.
                                }

                                Err(e) => {
                                    {
                                        let st = CONV_FILE_STATES.lock().unwrap();
                                        if let Some(s) = st.get(idx) {
                                            s.set_stage(STAGE_ERROR);
                                            *s.error_msg.lock().unwrap() = e.clone();
                                        }
                                    }
                                    set_status(&format!("\u{2717} Error [{}]: {}", idx + 1, e));
                                }
                            }
                        }
                        Err((idx, e)) => {
                            let st = CONV_FILE_STATES.lock().unwrap();
                            if let Some(s) = st.get(idx) {
                                if e == "Cancelled" {
                                    s.set_stage(STAGE_CANCELLED);
                                } else if e.starts_with("BAD_RATE:") {
                                    // Non-standard sample rate — show BAD badge, count as done
                                    s.badge.store(BADGE_BAD, Ordering::Relaxed);
                                    s.set_stage(STAGE_DONE);
                                    s.gpu_pct.store(1000, Ordering::Relaxed);
                                    let hz = e.trim_start_matches("BAD_RATE:");
                                    *s.error_msg.lock().unwrap() = format!("Non-standard sample rate: {} Hz", hz);
                                } else if e.starts_with("SKIP_RATE:") {
                                    // Source >= target — show SKIP badge, count as done
                                    s.badge.store(BADGE_SKIP, Ordering::Relaxed);
                                    s.set_stage(STAGE_DONE);
                                    s.gpu_pct.store(1000, Ordering::Relaxed);
                                    let parts: Vec<&str> = e.trim_start_matches("SKIP_RATE:").split(':').collect();
                                    let src_hz = parts.get(0).unwrap_or(&"?");
                                    let tgt_hz = parts.get(1).unwrap_or(&"?");
                                    *s.error_msg.lock().unwrap() = format!(
                                        "Skipped: source {}Hz >= target {}Hz",
                                        src_hz, tgt_hz
                                    );
                                } else {
                                    s.set_stage(STAGE_ERROR);
                                    *s.error_msg.lock().unwrap() = e;
                                }
                            }
                        }
                    }
                } // loop
            });
            worker_handles.push(h);
        }

        // Wait for prep thread to finish
        prep_handle.join().ok();

        // GPU workers will break loop when tx_prep drops (prep thread joins).
        for h in worker_handles {
            h.join().ok();
        }

        // Final status — count real conversions separately from files that
        // were skipped (source already at/above target) or rejected (bad
        // rate), so "All done! 1 files converted" never appears when the
        // only file in the batch was in fact skipped.
        let (converted, skipped) = {
            let st = CONV_FILE_STATES.lock().unwrap();
            let converted = st
                .iter()
                .filter(|s| {
                    s.stage() == STAGE_DONE && s.badge.load(Ordering::Relaxed) == BADGE_NONE
                })
                .count();
            let skipped = st
                .iter()
                .filter(|s| {
                    let b = s.badge.load(Ordering::Relaxed);
                    b == BADGE_SKIP || b == BADGE_BAD
                })
                .count();
            (converted, skipped)
        };
        crate::audio::gpu::filter_cache::clear();
        crate::audio::memory::log_process_memory("batch end");
        if CONV_CANCEL.load(Ordering::Relaxed) {
            set_status("Cancelled by user");
        } else if skipped > 0 {
            set_status(&format!(
                "\u{2713} All done! {} converted, {} skipped",
                converted, skipped
            ));
        } else {
            set_status(&format!("\u{2713} All done! {} files converted", converted));
        }
        CONV_RUNNING.store(false, Ordering::Relaxed);
        // Stop the heartbeat thread and wipe its line so the next prompt
        // (or user input in the GUI's stdout pane) starts cleanly.
        crate::audio::logging::stop_heartbeat();
    });

    Ok(())
}
/// Get current conversion progress
pub fn get_progress() -> (u32, u32, u32, String, String, u32) {
    let (total, done) = {
        let st = CONV_FILE_STATES.lock().unwrap();
        let t = st.len() as u32;
        let d = st
            .iter()
            .filter(|s| matches!(s.stage(), STAGE_DONE | STAGE_ERROR | STAGE_CANCELLED))
            .count() as u32;
        (t, d)
    };
    let progress = {
        let st = CONV_FILE_STATES.lock().unwrap();
        let total_files = st.len() as u32;
        if total_files == 0 {
            0
        } else {
            let mut sum_pct = 0;
            for s in st.iter() {
                match s.stage() {
                    STAGE_DONE => sum_pct += 1000,
                    STAGE_ERROR | STAGE_CANCELLED => sum_pct += 1000, // effectively acts as done for overall progress
                    STAGE_GPU_CONV | STAGE_ENCODING => sum_pct += s.gpu_pct.load(Ordering::Relaxed),
                    _ => {}
                }
            }
            sum_pct / total_files
        }
    };
    let status = CONV_STATUS.lock().unwrap().clone();
    let output = CONV_OUTPUT.lock().unwrap().clone();
    let snapped_rate = CONV_SNAPPED_RATE.load(Ordering::Relaxed);
    (progress, total, done, status, output, snapped_rate)
}
pub fn cancel() {
    CONV_CANCEL_FILE.store(false, Ordering::Relaxed); // this is a global cancel
    CONV_CANCEL.store(true, Ordering::Relaxed);
    crate::audio::cancel_flag::set(true); // interrupt GPU h_blocks FFT / generate_fir
}
/// Cancel only the file currently being GPU-processed; remaining files continue.
pub fn cancel_file(idx: u32) {
    let st = CONV_FILE_STATES.lock().unwrap();
    if let Some(s) = st.get(idx as usize) {
        s.cancelled.store(true, Ordering::Relaxed);
        // Only trigger the low-level cancel signal when THIS file is on the GPU.
        // For pending/preparing files the main loop will skip them naturally.
        if s.stage() == STAGE_GPU_CONV {
            CONV_CANCEL_FILE.store(true, Ordering::Relaxed);
            CONV_CANCEL.store(true, Ordering::Relaxed);
            crate::audio::cancel_flag::set(true); // interrupt h_blocks if mid-creation
        }
    }
}
/// Per-file statuses as delta JSON: {"rev":N,"files":[{"idx":N,...}]}.
///
/// `known_rev` is the last queue revision the caller has already merged
/// (0 = full sync). The response contains only files whose discrete state
/// changed since then, plus every currently-active file (their progress
/// percentage moves continuously). With a 685-file queue polled at 5 Hz
/// this ships ~300 bytes instead of ~150 KB per poll — the full-queue JSON
/// was the dominant traffic through the Tauri IPC eval bridge and made the
/// WebView2 process balloon on multi-hour batches.
pub fn get_file_statuses(known_rev: u32) -> String {
    let rev = CONV_QUEUE_REV.load(Ordering::Acquire);
    let st = CONV_FILE_STATES.lock().unwrap();
    let mut parts = Vec::new();
    for (i, s) in st.iter().enumerate() {
        let stage = s.stage();
        let active = matches!(stage, STAGE_PREPARING | STAGE_GPU_CONV | STAGE_ENCODING);
        let changed = s.last_rev.load(Ordering::Acquire) > known_rev;
        if known_rev != 0 && !active && !changed {
            continue;
        }
        let pct = match stage {
            STAGE_GPU_CONV | STAGE_ENCODING => s.gpu_pct.load(Ordering::Relaxed),
            STAGE_DONE => 1000,
            _ => 0,
        };
        let badge = s.badge.load(Ordering::Relaxed);
        let err = s.error_msg.lock().unwrap().replace('"', "'");
        let aa = s.aa_state.load(Ordering::Relaxed);
        let aa_note = s
            .aa_note
            .lock()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "'");
        let out = s
            .output_path
            .lock()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        parts.push(format!(
            r#"{{"idx":{},"stage":{},"pct":{},"badge":{},"aa":{},"aa_note":"{}","error":"{}","output":"{}"}}"#,
            i, stage, pct, badge, aa, aa_note, err, out
        ));
    }
    format!(r#"{{"rev":{},"files":[{}]}}"#, rev, parts.join(","))
}
#[allow(dead_code)]
pub fn is_running() -> bool {
    CONV_RUNNING.load(Ordering::Relaxed)
}

#[cfg(test)]
mod soak_tests {
    use super::*;

    /// Memory soak: drive the REAL conversion pipeline (decode → AA →
    /// polyphase GPU/CPU → hybrid blend → encode → verify) over a batch of
    /// actual files and log the process-memory curve after every file.
    /// The [MEM] lines tell leak (monotonic growth) apart from a high but
    /// stable peak.
    ///
    /// Ignored by default — heavy and environment-dependent. Run with:
    ///   AE_SOAK_SRC=E:\MUSIC AE_SOAK_WORK=<dir> AURA_FILTER_DIR=<repo>\fir-optimizer\output \
    ///   cargo test --release mem_soak -- --ignored --nocapture
    /// Optional: AE_SOAK_FILES (default 8), AE_SOAK_GPU / AE_SOAK_HYBRID /
    /// AE_SOAK_AA (default 1), AE_SOAK_TAPS (default 30000000).
    #[test]
    #[ignore]
    fn mem_soak_convert_batch() {
        let flag = |name: &str, default: bool| -> bool {
            std::env::var(name)
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(default)
        };
        let src_dir = std::env::var("AE_SOAK_SRC").unwrap_or_else(|_| "E:\\MUSIC".into());
        let work_dir = std::env::var("AE_SOAK_WORK")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("ae_mem_soak"));
        let n_files: usize = std::env::var("AE_SOAK_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);

        std::fs::create_dir_all(&work_dir).expect("cannot create soak work dir");

        // Copy the N smallest mp3/flac sources into the work dir so outputs
        // and HPSS caches never touch the user's music folder.
        let mut sources: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&src_dir)
            .expect("cannot read AE_SOAK_SRC")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| matches!(x.to_ascii_lowercase().as_str(), "mp3" | "flac"))
                    .unwrap_or(false)
            })
            .filter_map(|e| e.metadata().ok().map(|m| (m.len(), e.path())))
            .collect();
        sources.sort_by_key(|(len, _)| *len);
        let mut paths: Vec<String> = Vec::new();
        for (_, src) in sources.iter().take(n_files) {
            let dst = work_dir.join(src.file_name().unwrap());
            std::fs::copy(src, &dst).expect("copy source file");
            paths.push(dst.to_string_lossy().to_string());
        }
        assert!(!paths.is_empty(), "no source files found in {}", src_dir);

        let settings = ConvertSettings {
            out_rate: 0, // resolved per file from fs_multiplier
            fs_multiplier: 8,
            taps: std::env::var("AE_SOAK_TAPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30_000_000),
            precision: 64,
            win_type: 4,
            custom_filter_path: None,
            use_gpu: flag("AE_SOAK_GPU", true),
            use_fir_resampling: true,
            apodizing: 0,
            headroom_db: -3.0,
            adaptive_apodizer: flag("AE_SOAK_AA", true),
            hybrid_phase: flag("AE_SOAK_HYBRID", true),
            iir_dc_blocking: false,
        };

        crate::aelog!(
            "[SOAK] {} files | GPU={} | hybrid={} | AA={} | taps={}",
            paths.len(),
            settings.use_gpu,
            settings.hybrid_phase,
            settings.adaptive_apodizer,
            settings.taps
        );
        crate::audio::memory::log_process_memory("soak start");

        convert_files(paths, settings).expect("convert_files failed to start");
        while is_running() {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }

        crate::audio::memory::log_process_memory("soak end (before drop of states)");
        let statuses = get_file_statuses(0);
        crate::aelog!("[SOAK] final statuses: {}", statuses);
    }
}
