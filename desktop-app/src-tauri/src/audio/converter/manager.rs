use crate::audio::converter::decode::set_status;
use crate::audio::converter::process::{prepare_audio_phase, process_one_prepared};
use crate::audio::converter::state::*;
use crate::audio::converter::types::{ConvertSettings, PreparedAudio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

/// Start converting a batch of files in a background thread
pub fn convert_files(paths: Vec<String>, settings: ConvertSettings) -> Result<(), String> {
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

    // Heartbeat: a self-overwriting status line at the bottom of stdout
    // showing current local time + how long since the last log line. Lets
    // you see at a glance whether a long-running step is still alive or
    // genuinely hung. Stopped at the bottom of the worker thread below.
    crate::audio::logging::start_heartbeat();

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
            // GPU mode: cap at 2 simultaneous heavy convolutions, but only
            // if VRAM can actually fit two of them. recommended_gpu_workers()
            // looks at adapter limits and downgrades to 1 if h_freq + delay
            // buffers would exceed the safe budget.
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
            let vram_safe = crate::audio::gpu::context::recommended_gpu_workers(settings_clone.taps);
            cores.clamp(1, 2).min(vram_safe)
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
            for (idx, path_str) in paths_clone.iter().enumerate() {
                // Ensure sufficient RAM (reserve ~2.5 GB buffer required to load decoded f64)
                crate::audio::memory::await_free_ram(2500, "Decoding Thread");

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

                let src = std::path::Path::new(path_str.as_str());
                let mut file_settings = settings_clone.clone();
                let msg = match prepare_audio_phase(src, &mut file_settings, file_cancel) {
                    Ok(p) => Ok((idx, p, file_settings.out_rate)),
                    Err(e) => Err((idx, e)),
                };
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

                            let src = std::path::Path::new(paths[idx].as_str());
                            match process_one_prepared(
                                src,
                                prep,
                                &per_file_settings,
                                file_state.unwrap_or_else(|| Arc::new(FileConvState::new())),
                            ) {
                                Ok(out) => {
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
/// Per-file statuses as a JSON array: [{"idx":N,"stage":N,"pct":N,"error":"","output":""}].
pub fn get_file_statuses() -> String {
    let st = CONV_FILE_STATES.lock().unwrap();
    let mut parts = Vec::with_capacity(st.len());
    for (i, s) in st.iter().enumerate() {
        let stage = s.stage();
        let pct = match stage {
            STAGE_GPU_CONV | STAGE_ENCODING => s.gpu_pct.load(Ordering::Relaxed),
            STAGE_DONE => 1000,
            _ => 0,
        };
        let badge = s.badge.load(Ordering::Relaxed);
        let err = s.error_msg.lock().unwrap().replace('"', "'");
        let out = s
            .output_path
            .lock()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        parts.push(format!(
            r#"{{"idx":{},"stage":{},"pct":{},"badge":{},"error":"{}","output":"{}"}}"#,
            i, stage, pct, badge, err, out
        ));
    }
    format!("[{}]", parts.join(","))
}
#[allow(dead_code)]
pub fn is_running() -> bool {
    CONV_RUNNING.load(Ordering::Relaxed)
}
