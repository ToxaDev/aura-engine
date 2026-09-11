// Temporarily disabled for debugging — enables console output in release builds
// #![cfg_attr(
//   all(not(debug_assertions), target_os = "windows"),
//   windows_subsystem = "windows"
// )]

mod audio;
mod startup;

#[tauri::command]
fn convert_files(
    paths: Vec<String>,
    fs_multiplier: u32,
    taps: u32,
    precision: u32,
    win_type: i32,
    custom_filter_path: Option<String>,
    use_gpu: bool,
    use_fir_resampling: bool,
    apodizing: u32,
    headroom_db: f64,
    adaptive_apodizer: bool,
    hybrid_phase: bool,
    iir_dc_blocking: bool,
) -> Result<(), String> {
    let settings = crate::audio::converter::ConvertSettings {
        out_rate: 0,      // Computed per-file in prepare.rs from src_rate x family_base x fs_multiplier
        fs_multiplier,    // FS slider value: 2, 4, 8, or 16
        taps: taps as usize,
        precision,
        win_type,
        custom_filter_path,
        use_gpu,
        use_fir_resampling,
        apodizing,
        headroom_db,
        adaptive_apodizer,
        hybrid_phase,
        iir_dc_blocking,
    };
    crate::audio::converter::convert_files(paths, settings)
}

/// Pre-flight: does this queue, at these settings, have every filter it needs?
///
/// Called when files are added and whenever the filter settings change, so a
/// missing blob is reported before the user waits through a conversion that
/// cannot happen. Reads only the container header of each file (no decode).
///
/// Returns JSON: `ok`, the exact `missing` filenames, the release `packs` that
/// contain them with direct download links, and `dest` — where to extract.
#[tauri::command]
fn check_filters(
    paths: Vec<String>,
    fs_multiplier: u32,
    taps: u32,
    custom_filter_path: Option<String>,
    hybrid_phase: bool,
) -> String {
    use crate::audio::converter::dsp::filter as flt;

    let taps = taps as usize;
    let mut missing: Vec<serde_json::Value> = Vec::new();
    let mut packs: Vec<serde_json::Value> = Vec::new();
    let mut seen_rates: Vec<u32> = Vec::new();
    let mut seen_packs: Vec<String> = Vec::new();
    // Sources with more than two channels. The engine converts the front pair
    // and drops the rest, which is a decision the user has to make knowingly
    // rather than discover afterwards in a log line.
    let mut wide: Vec<serde_json::Value> = Vec::new();

    for path in &paths {
        let Some((_, src_rate, channels)) = crate::audio::converter::decode::probe_input_frames(
            std::path::Path::new(path),
        ) else {
            continue; // unreadable here; the conversion path reports it properly
        };

        if channels > 2 {
            wide.push(serde_json::json!({
                "file": std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone()),
                "channels": channels,
            }));
        }

        // Both source families known → every filter this batch needs has been
        // resolved, and the rest of the loop only still runs to find wide
        // sources. That costs one header read per file, which is worth it:
        // missing one and silently discarding four channels is the worse
        // outcome by a wide margin.
        if seen_rates.len() >= 2 {
            continue;
        }
        // A non-standard source rate is a different error, surfaced per-file
        // as the BAD badge — not a missing filter.
        let Some(family) = crate::audio::converter::pipeline::prepare::detect_family(src_rate)
        else {
            continue;
        };
        let target = family * fs_multiplier;
        if seen_rates.contains(&target) {
            continue;
        }
        seen_rates.push(target);

        // A custom filter is trusted as the linear-phase stage, so only the
        // minimum-phase blob still has to be found for it.
        let mut needed: Vec<&str> = Vec::new();
        if custom_filter_path.is_none() {
            needed.push("linear_phase");
        }
        if hybrid_phase {
            needed.push("minimum_phase");
        }

        for phase in needed {
            if flt::find_precomputed_filter(taps, target, phase).is_some() {
                continue;
            }
            missing.push(serde_json::json!({
                "file": flt::blob_name(taps, target, phase)
                    .unwrap_or_else(|| format!("{}-tap {} filter", taps, phase)),
                "rate": target,
                "phase": phase.replace('_', "-"),
            }));
            if let Some((name, url)) = flt::filter_pack_for(taps, target) {
                if !seen_packs.contains(&name) {
                    seen_packs.push(name.clone());
                    packs.push(serde_json::json!({ "name": name, "url": url }));
                }
            }
        }

    }

    serde_json::json!({
        "ok": missing.is_empty() && wide.is_empty(),
        "missing": missing,
        "multichannel": wide,
        "packs": packs,
        "dest": flt::portable_filter_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    })
    .to_string()
}

/// Does the GPU path this machine would take actually exist?
///
/// Deliberately not folded into `check_filters`. That one answers a question
/// with a yes-or-no consequence — a missing filter stops the batch. A missing
/// GPU stops nothing: the engine falls back to the CPU reference path on its
/// own. Sharing a pre-flight with the fatal check is how the harmless one
/// eventually ends up gating conversion by accident.
///
/// The frontend asks before a batch, not at startup: answering means standing
/// up a wgpu device, and there is no reason to pay for one on a launch that
/// converts nothing. The answer is cached for the life of the process.
///
/// Returns JSON: `available`, and `reason` when it is not.
#[tauri::command]
fn gpu_status() -> String {
    match crate::audio::gpu::context::probe() {
        Ok(()) => serde_json::json!({ "available": true }).to_string(),
        Err(reason) => serde_json::json!({
            "available": false,
            "reason": reason,
        })
        .to_string(),
    }
}

/// What this installation can actually convert with.
///
/// The interface has no way to know which filter packs a user extracted, and a
/// slider position with no blob behind it is a dead end they only discover
/// after picking files. So the frontend asks once at startup and shapes itself
/// around the answer: it starts on a combination that works, and marks the
/// ones that do not.
///
/// Returns JSON: `present` (the tap-count/output-rate cells on disk and which
/// phases each one has), `packs` (the download that would fill in a tap count,
/// keyed by tap count), `dest` (where a pack should be extracted), and
/// `searched` (every directory consulted).
#[tauri::command]
fn filter_inventory() -> String {
    use crate::audio::converter::dsp::filter as flt;

    let present: Vec<serde_json::Value> = flt::inventory()
        .iter()
        .map(|e| {
            serde_json::json!({
                "taps": e.taps,
                "rate": e.target_rate_hz,
                "linear": e.linear,
                "minimum": e.minimum,
            })
        })
        .collect();

    let mut packs = serde_json::Map::new();
    for taps in flt::TAP_LADDER {
        let list: Vec<serde_json::Value> = flt::filter_packs_for_taps(taps)
            .into_iter()
            .map(|(name, url)| serde_json::json!({ "name": name, "url": url }))
            .collect();
        packs.insert(taps.to_string(), serde_json::Value::Array(list));
    }

    serde_json::json!({
        "present": present,
        "packs": packs,
        "dest": flt::portable_filter_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "searched": flt::searched_dirs(),
    })
    .to_string()
}

#[tauri::command]
fn get_conversion_progress() -> (u32, u32, u32, String, String, u32) {
    crate::audio::converter::get_progress()
}

#[tauri::command]
fn cancel_conversion() -> Result<(), String> {
    crate::audio::converter::cancel();
    Ok(())
}

#[tauri::command]
fn cancel_file(idx: u32) -> Result<(), String> {
    crate::audio::converter::cancel_file(idx);
    Ok(())
}

#[tauri::command]
fn get_queue_status(known_rev: u32) -> String {
    crate::audio::converter::get_file_statuses(known_rev)
}

/// Validate a .npy filter file and return its tap count.
/// Called by the frontend when the user picks a custom FIR filter via the file dialog.
/// The actual path is stored in JS state and passed to convert_files on conversion start.
#[tauri::command]
fn set_custom_filter(path: String) -> Result<u32, String> {
    use std::path::Path;
    let p = Path::new(&path);
    if !p.exists() {
        return Err(format!("Filter file not found: {}", path));
    }
    let coeffs = crate::audio::dsp_core::load_npy_f64(&path)
        .map_err(|e| format!("Failed to load filter '{}': {}", path, e))?;
    if coeffs.is_empty() {
        return Err(format!("Filter file is empty or has no coefficients: {}", path));
    }
    println!("[FILTER] Custom filter validated: {} ({} taps)", path, coeffs.len());
    Ok(coeffs.len() as u32)
}

/// Acknowledge filter clear — the filter path lives in JS state, so this is a no-op on
/// the Rust side. Kept as a command so the frontend can await it without try/catch errors.
#[tauri::command]
fn clear_custom_filter() -> Result<(), String> {
    println!("[FILTER] Custom filter cleared");
    Ok(())
}

/// What the console says before anything happens.
///
/// The app ships as a console application on purpose — the window is the
/// audit log, and every claim the engine makes about a conversion is meant to
/// be readable there as it happens. But a bare window with one line in it
/// reads as a fault, and closing it ends the process, so the first thing it
/// prints says what it is and asks to be left alone.
///
/// Only facts that are true without a source checkout go in here: the version,
/// where this run is being logged, and which filters were actually found on
/// disk. The GPU is deliberately absent — naming the adapter means standing up
/// a wgpu instance, and instances created and dropped in quick succession have
/// already cost this project a driver-handle exhaustion bug. It is reported at
/// conversion time instead, where the instance exists anyway.
fn print_startup_banner(session_log: Option<&std::path::Path>) {
    const RULE: &str = "===============================================================";

    let mut out: Vec<String> = vec![
        RULE.to_string(),
        format!("  Aura Engine {}", env!("CARGO_PKG_VERSION")),
        RULE.to_string(),
        String::new(),
        "  This window is the engine's log: it shows what each conversion".to_string(),
        "  is doing, step by step. Keep it open — closing it closes Aura".to_string(),
        "  Engine. Minimise it if it is in the way.".to_string(),
        String::new(),
    ];

    for (i, line) in filter_summary().iter().enumerate() {
        out.push(format!(
            "  {:<12} {}",
            if i == 0 { "Filters" } else { "" },
            line
        ));
    }
    if let Some(p) = session_log {
        out.push(format!("  {:<12} {}", "Session log", p.display()));
    }

    out.push(String::new());
    out.push("  Drop audio files on the window to start.".to_string());
    out.push(RULE.to_string());
    out.push(String::new());

    audio::logging::banner(&out);
}

/// One line per tap count that has filters on disk, so the banner says what
/// this copy can actually do rather than what the app supports in general.
fn filter_summary() -> Vec<String> {
    use audio::converter::dsp::filter::{inventory, taps_label};

    let present = inventory();
    if present.is_empty() {
        return vec!["none found — see README.txt".to_string()];
    }

    let mut out = Vec::new();
    for taps in audio::converter::dsp::filter::TAP_LADDER {
        let rows: Vec<_> = present.iter().filter(|p| p.taps == taps).collect();
        if rows.is_empty() {
            continue;
        }
        let both = rows.iter().all(|r| r.linear && r.minimum);
        out.push(format!(
            "{} taps — {} output rate{} — {}",
            taps_label(taps).unwrap_or("?"),
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            if both {
                "linear + minimum"
            } else {
                "incomplete pair, Hybrid-Phase unavailable"
            }
        ));
    }
    out
}

fn main() {
    // Before anything else, so that a failure has a voice. Until this was
    // here, a machine that could not run the program said nothing at all:
    // the console this process owns closes with it, taking the panic message
    // along, and the user sees a black rectangle flash and nothing more.
    startup::install_handlers();

    // `--help`, and the self tests that let someone whose machine will not run
    // this send back the reason. Before the log and the preflight, because it
    // has to work when those are exactly what is broken.
    if startup::handle_selftest_args() {
        return;
    }

    // Session log file (%LOCALAPPDATA%\AuraEngine\logs\session-*.log):
    // every aelog line of this run is mirrored there for later analysis.
    let session_log = audio::logging::init_session_log();

    // Missing AVX2, missing WebView2. Both are ordinary on machines we do not
    // own, and both are worth a sentence the user can act on rather than an
    // error they cannot read.
    if !startup::preflight() {
        std::process::exit(1);
    }

    print_startup_banner(session_log.as_deref());
    let run = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            convert_files,
            get_conversion_progress,
            cancel_conversion,
            cancel_file,
            get_queue_status,
            set_custom_filter,
            clear_custom_filter,
            check_filters,
            gpu_status,
            filter_inventory
        ])
        .run(tauri::generate_context!());

    // `expect` here used to end the process with a message nobody could read.
    if let Err(e) = run {
        startup::tauri_failed(&e);
        std::process::exit(1);
    }
}
