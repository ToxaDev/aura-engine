// No `windows_subsystem = "windows"` here, on purpose: the app ships as a
// console application, and the console is the audit log (see
// `print_startup_banner`). Do not bring the attribute back.
// #![cfg_attr(
//   all(not(debug_assertions), target_os = "windows"),
//   windows_subsystem = "windows"
// )]

// The status JSON in player/controller.rs has a large number of fields;
// serde_json's json! macro uses 4 recursion levels per field.
#![recursion_limit = "512"]

mod audio;
mod player;
mod player_commands;
mod radio_catalog;
mod single_instance;
mod app_dir;
mod spatial;
mod startup;
mod updater;
mod vis_store;
mod webview_png;
mod window_state;

use tauri::Manager;

#[tauri::command]
fn convert_files(
    paths: Vec<String>,
    fs_multiplier: u32,
    taps: u32,
    precision: u32,
    custom_filter_path: Option<String>,
    use_gpu: bool,
    use_fir_resampling: bool,
    apodizing: u32,
    headroom_db: f64,
    adaptive_apodizer: bool,
    hybrid_phase: bool,
    iir_dc_blocking: bool,
    lab_features: Option<crate::audio::converter::types::LabFeatures>,
    subsonic_hz: u32,
    album_level: Option<bool>,
) -> Result<(), String> {
    if subsonic_hz != 0
        && !crate::audio::converter::apodize::SUBSONIC_CORNERS_HZ.contains(&subsonic_hz)
    {
        return Err(format!("Subsonic filter: unsupported corner {} Hz", subsonic_hz));
    }
    let mut settings = crate::audio::converter::ConvertSettings {
        out_rate: 0,      // Computed per-file in prepare.rs from src_rate x family_base x fs_multiplier
        fs_multiplier,    // FS slider value: 2, 4, 8, or 16
        taps: taps as usize,
        precision,
        custom_filter_path,
        use_gpu,
        use_fir_resampling,
        apodizing,
        headroom_db,
        adaptive_apodizer,
        hybrid_phase,
        iir_dc_blocking,
        lab: Default::default(),
        subsonic_hz,  // 0 = off; otherwise the corner in Hz (10, 15 or 20)
    };
    settings.lab = lab_features.unwrap_or_default();
    // Album level is on unless the page says otherwise (the rack's ALB stage).
    let album_level = album_level.unwrap_or(true);
    // Not while an update is being installed: the app is about to restart.
    crate::updater::start_unless_installing(|| {
        crate::audio::converter::convert_batch(paths, settings, album_level)
    })
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
    tfs_phase: bool,
) -> String {
    use crate::audio::converter::dsp::filter as flt;

    let taps = taps as usize;
    let mut missing: Vec<serde_json::Value> = Vec::new();
    let mut packs: Vec<serde_json::Value> = Vec::new();
    // The blobs asked for so far, by the rate they are designed at.
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

        // Every file is looked at, if only for wide sources: one header read
        // per file is worth it — missing one and silently discarding four
        // channels is the worse outcome by a wide margin.
        //
        // A non-standard source rate is a different error, surfaced per-file
        // as the BAD badge — not a missing filter.
        let Some(family) = crate::audio::converter::pipeline::prepare::detect_family(src_rate)
        else {
            continue;
        };
        let target = family * fs_multiplier;
        // At or above the output rate the file is skipped, not filtered.
        if src_rate >= target {
            continue;
        }
        // A hi-res source needs the blob of its own factor, not the output
        // rate's (`filter::design_rate`).
        let design = flt::design_rate(src_rate, target);
        if seen_rates.contains(&design) {
            continue;
        }
        seen_rates.push(design);

        // A custom filter is trusted as the linear-phase stage, so only the
        // minimum-phase blob still has to be found for it.
        let mut needed: Vec<&str> = Vec::new();
        if custom_filter_path.is_none() {
            needed.push("linear_phase");
        }
        // TFS derives its filter from the lin+min pair, so it wants the
        // minimum-phase blob exactly as Hybrid-Phase does. Asking for it here
        // matters more than it used to: TFS ships on, so an installation with
        // only the linear halves would otherwise pass this pre-flight and then
        // fail per-file inside `tfs::resolve_or_derive`, after the batch had
        // already started.
        if hybrid_phase || tfs_phase {
            needed.push("minimum_phase");
        }

        for phase in needed {
            if flt::find_precomputed_filter(taps, src_rate, target, phase).is_some() {
                continue;
            }
            missing.push(serde_json::json!({
                "file": flt::blob_name(taps, design, phase)
                    .unwrap_or_else(|| format!("{}-tap {} filter", taps, phase)),
                "rate": design,
                "phase": phase.replace('_', "-"),
            }));
            if let Some((name, url)) = flt::filter_pack_for(taps, design) {
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
    crate::aelog!("[FILTER] Custom filter validated: {} ({} taps)", path, coeffs.len());
    Ok(coeffs.len() as u32)
}

/// Acknowledge filter clear — the filter path lives in JS state, so this is a no-op on
/// the Rust side. Kept as a command so the frontend can await it without try/catch errors.
#[tauri::command]
fn clear_custom_filter() -> Result<(), String> {
    crate::aelog!("[FILTER] Custom filter cleared");
    Ok(())
}

/// The XTC listening-geometry window.
///
/// A window rather than a dialog inside the converter: four measurements, a plan
/// of the triangle and the span angle they imply do not fit in a panel that is
/// only as wide as the app, and cramming them there is what the first attempt
/// did — the labels wrapped one word to a line and the drawing had nowhere to go.
///
/// One window, reused, and it hands its result back over the event bus rather
/// than writing settings itself: the converter window stays the only writer of
/// `auraSettings`, so there is no second author to race with.
///
/// `async` rather than a plain command (wry#583): building a WebView2 window
/// from inside the first window's own IPC callback deadlocks on Windows — the
/// frame appears, the webview never initialises, and the event loop stops
/// answering. `async` hands the work to the runtime, so the event loop itself
/// creates the window.
#[tauri::command]
async fn xtc_geometry_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WindowUrl};

    if let Some(w) = app.get_window("xtc-geometry") {
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    tauri::WindowBuilder::new(
        &app,
        "xtc-geometry",
        WindowUrl::App("xtc-geometry.html".into()),
    )
    .title("XTC - listening geometry")
    .inner_size(760.0, 640.0)
    .min_inner_size(520.0, 520.0)
    .resizable(true)
    // Frameless like the main window: the page draws its own head bar.
    .decorations(false)
    .transparent(true)
    .build()
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Quit from the main window's close button: where the window was is kept,
/// the player stops, every other window goes with the process.
#[tauri::command]
fn app_quit(app: tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_window("main") {
        window_state::closing(&w);
    }
    player::controller::get().stop();
    crate::aelog!("[APP] quit from the main window");
    app.exit(0);
}

// analytics: subject lifecycle commands (§2.2)

/// Open an analyzer subject. Returns the allocated SID (u32).
/// `mode` = "live" or "file".
#[tauri::command]
fn an_subject_open(
    mode: String,
    entry_id: Option<u64>,
    path: Option<String>,
    conv: Option<String>,
    settings: Option<serde_json::Value>,
) -> u32 {
    // analytics: subject lifecycle
    player::analytics::hub::get().subject_open(
        &mode,
        entry_id,
        path.as_deref(),
        conv.as_deref(),
        settings,
    )
}

/// Close an analyzer subject and release its resources.
#[tauri::command]
fn an_subject_close(sid: u32) {
    // analytics: subject lifecycle
    player::analytics::hub::get().subject_close(sid);
}

/// The analyzer's Screenshot: the calling window's page as a PNG, in base64
/// (the page adds its line of what was measured and saves it).
#[tauri::command]
async fn an_window_png(window: tauri::Window) -> Result<String, String> {
    use base64::Engine as _;
    let png = webview_png::capture(&window).await?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

/// Open (or focus) an analyzer window.
/// `label` is the Tauri window label (unique per open window).
/// `url`   is a relative path such as `analytics.html?mode=live`.
/// Position/size are encoded in the URL query string by the caller.
#[tauri::command]
async fn analyzer_open(
    app: tauri::AppHandle,
    label: String,
    url: String,
) -> Result<(), String> {
    use tauri::{Manager, WindowUrl};
    if let Some(win) = app.get_window(&label) {
        win.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    // Analyzer windows already open: a new one goes a step down and right of
    // the remembered place, so it does not hide them.
    let others = app.windows().keys().filter(|l| l.starts_with("analyzer")).count();
    let w = tauri::WindowBuilder::new(
        &app,
        label,
        WindowUrl::App(url.into()),
    )
    .title("Aura Analyzer")
    // Wide enough for the numbers and the graphs side by side.
    .inner_size(1180.0, 720.0)
    .min_inner_size(480.0, 360.0)
    .resizable(true)
    // Frameless like the main window: the page draws its own title bar;
    // the edges still resize the window.
    .decorations(false)
    .transparent(true)
    // Placed where the analyzer was last time (size, maximized, whole
    // screen), then shown (window_state.rs).
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    window_state::restore_all(&w);
    if others > 0
        && !w.is_maximized().unwrap_or(false)
        && !w.is_fullscreen().unwrap_or(false)
    {
        if let Ok(p) = w.outer_position() {
            let step = 32 * others as i32;
            let _ = w.set_position(tauri::PhysicalPosition::new(p.x + step, p.y + step));
        }
    }
    let _ = w.show();
    let _ = w.set_focus();
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

// ── Cover cache ────────────────────────────────────────────────────────────
//
// The showcase mode of the player strip displays album art for the current
// (and potentially the next) track.  Loading a cover on every request would
// re-open and re-probe the file each time; this two-slot MRU keeps the most
// recently requested covers in memory so the strip's periodic repaint is free.
// The cache holds at most two entries so it never retains artwork for the
// whole queue.

use std::sync::{Mutex, OnceLock};

struct CoverCache {
    /// Entries in MRU order: index 0 is the most recently requested.
    slots: Vec<(u64, Vec<u8>, String)>,
}

impl CoverCache {
    fn new() -> Self {
        Self { slots: Vec::with_capacity(2) }
    }

    fn get(&self, id: u64) -> Option<(Vec<u8>, String)> {
        self.slots
            .iter()
            .find(|(i, _, _)| *i == id)
            .map(|(_, d, ct)| (d.clone(), ct.clone()))
    }

    fn put(&mut self, id: u64, data: Vec<u8>, ct: String) {
        self.slots.retain(|(i, _, _)| *i != id);
        self.slots.insert(0, (id, data, ct));
        self.slots.truncate(2);
    }
}

static COVER_CACHE: OnceLock<Mutex<CoverCache>> = OnceLock::new();

fn cover_cache() -> &'static Mutex<CoverCache> {
    COVER_CACHE.get_or_init(|| Mutex::new(CoverCache::new()))
}

fn main() {
    // Before anything else, so that a failure has a voice. Until this was
    // here, a machine that could not run the program said nothing at all:
    // the console this process owns closes with it, taking the panic message
    // along, and the user sees a black rectangle flash and nothing more.
    startup::install_handlers();

    // Rayon's global pool — the converter's jobs and the player's background
    // preparations (a track's source stages, Declip, the apodizer) — below
    // normal priority. Playback runs its own pools above it, but the video
    // card's driver works in threads of normal priority: with every core busy
    // at normal priority it waited, and a 30M chain on the card stalled for
    // about a second at a live start while the full variant was prepared
    // (Anton's log 27.09, 00:11:47–49). On an idle machine nothing changes.
    let _ = rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("aura-bg-{i}"))
        .start_handler(|_| player::analytics::track::set_below_normal_priority())
        .build_global();

    // `--help`, and the self tests that let someone whose machine will not run
    // this send back the reason. Before the log and the preflight, because it
    // has to work when those are exactly what is broken.
    if startup::handle_selftest_args() {
        return;
    }
    // The player's diagnostics, also windowless: --player-selftest <file>
    // (the whole playback path on the real device at -120 dB) and
    // --probe-device (what the output device accepts, in its own words).
    if player::selftest::handle_args() {
        return;
    }

    // After an in-app update the new exe starts with --after-update <old-pid>.
    // Wait for the old process to finish and clean up its .old file before
    // doing anything else — including taking the single-instance lock.
    updater::handle_after_update();

    // One copy per machine: a second one raises the first one's window and
    // leaves, before it opens a log or touches the output device.
    if !single_instance::acquire() {
        return;
    }

    // The console (the audit log) goes to the taskbar before the window
    // comes up; the banner in it still asks not to be closed.
    startup::minimize_own_console();

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
    // The converter's album level estimates each track's output peak with the
    // player's short render of the same chain; the engine alone has none.
    audio::converter::album::install_peak_probe(player::album_probe::converter_probe);
    // analytics: initialize hub (and live consumer thread) before the WebView2
    // protocol closure so routes are available from the first request.
    player::analytics::hub::init();
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
            filter_inventory,
            xtc_geometry_open,
            app_quit,
            vis_store::vis_studio_open,
            vis_store::vis_list,
            vis_store::vis_save,
            vis_store::vis_delete,
            vis_store::vis_dir_open,
            vis_store::vis_export,
            vis_store::vis_import,
            vis_store::vis_restore_app,
            window_state::window_keep_place,
            spatial::pack::spatial_status,
            spatial::pack::spatial_install,
            spatial::pack::spatial_install_file,
            spatial::pack::spatial_cancel,
            spatial::pack::spatial_set_wanted,
            spatial::pack::spatial_remove,
            spatial::spatial_track_status,
            player_commands::player_add,
            player_commands::player_remove,
            player_commands::player_reorder,
            player_commands::player_clear,
            player_commands::player_play,
            player_commands::player_pause,
            player_commands::player_stop,
            player_commands::player_next,
            player_commands::player_prev,
            player_commands::player_seek,
            player_commands::player_set_volume,
            player_commands::player_set_settings,
            player_commands::player_status,
            player_commands::player_radio,
            player_commands::radio_stream_wait,
            radio_catalog::radio_catalog_search,
            radio_catalog::radio_catalog_lists,
            radio_catalog::radio_station_url,
            radio_catalog::radio_icon,
            radio_catalog::radio_store_load,
            radio_catalog::radio_store_save,
            player_commands::player_devices,
            player_commands::player_set_device,
            player_commands::player_ui_log,
            player_commands::blind_test_trials,
            player_commands::ui_cursor_client,
            player_commands::player_paths_exist,
            player_commands::player_queue,
            player_commands::player_set_repeat,
            player_commands::player_set_vis_ahead,
            player_commands::player_set_track_settings,
            player_commands::player_clear_track_settings,
            updater::update_check,
            updater::update_install,
            // analytics: subject lifecycle (§2.2)
            an_subject_open,
            an_subject_close,
            an_window_png,
            analyzer_open
        ])
        // Custom protocol 'aura': high-frequency status and spectrum polling
        // goes here rather than through invoke, because Tauri 1's invoke path
        // retains every response in WebView2's ExecuteScript queue — frequent
        // large payloads measured at gigabytes per hour (see §0, rules).
        // On Windows, WebView2 maps 'aura' to https://aura.localhost/…
        .register_uri_scheme_protocol("aura", |_app, req| {
            let uri = req.uri();
            // On Windows, WebView2 maps the custom scheme aura:// to
            // https://aura.localhost/… before calling this handler.
            // Some wry versions hand it as aura://localhost/… instead.
            // Either way: strip everything up to and including the host.
            let after_host = uri
                .find("aura.localhost")
                .map(|p| &uri[p + "aura.localhost".len()..])
                .or_else(|| {
                    // Fallback: aura://localhost/path or aura:///path
                    uri.find("://")
                        .and_then(|p| uri[p + 3..].find('/').map(|q| &uri[p + 3 + q..]))
                })
                .unwrap_or("/not-found");
            let (path, query_str) = match after_host.find('?') {
                Some(q) => (&after_host[..q], &after_host[q + 1..]),
                None => (after_host, ""),
            };

            let ok = |body: Vec<u8>, ct: &str| {
                tauri::http::ResponseBuilder::new()
                    .header("Content-Type", ct)
                    .header("Cache-Control", "no-store")
                    .header("Access-Control-Allow-Origin", "*")
                    .status(200)
                    .body(body)
            };

            if path == "/player/status" {
                let v = player::controller::get().status();
                let json = serde_json::to_vec(&v).unwrap_or_default();
                return ok(json, "application/json");
            }

            if path == "/update/status" {
                return ok(updater::status_json(), "application/json");
            }

            if path == "/player/cover" {
                // Serve the album-art image for a queued track.
                // The id is a track id that must exist in the current queue;
                // arbitrary paths are never accepted.
                let id: Option<u64> = query_str
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("id="))
                    .and_then(|v| v.parse().ok());
                let Some(id) = id else {
                    return tauri::http::ResponseBuilder::new()
                        .header("Access-Control-Allow-Origin", "*")
                        .status(400)
                        .body(b"bad request".to_vec());
                };

                // Fast path: cover already cached.
                if let Some((data, ct)) = cover_cache().lock().unwrap().get(id) {
                    return ok(data, &ct);
                }

                // Look up the track path from the current queue.
                let track_path: Option<std::path::PathBuf> = player::controller::get()
                    .queue()
                    .into_iter()
                    .find(|t| t.id == id)
                    .map(|t| std::path::PathBuf::from(&t.path));
                let Some(track_path) = track_path else {
                    return tauri::http::ResponseBuilder::new()
                        .header("Access-Control-Allow-Origin", "*")
                        .status(404)
                        .body(b"not found".to_vec());
                };

                // Load the cover (no lock held while reading from disk).
                match player::probe::probe_cover(&track_path) {
                    Some((data, ct)) => {
                        cover_cache().lock().unwrap().put(id, data.clone(), ct.clone());
                        return ok(data, &ct);
                    }
                    None => {
                        return tauri::http::ResponseBuilder::new()
                            .header("Access-Control-Allow-Origin", "*")
                            .status(404)
                            .body(b"no cover".to_vec());
                    }
                }
            }

            if path == "/radio/icon" {
                // A station's icon, from the catalog's own folder only
                // (`radio_icon` fetched it): this never waits for the network.
                let key = query_str.split('&').find_map(|kv| kv.strip_prefix("k=")).unwrap_or("");
                return match radio_catalog::icon_file(key) {
                    Some((data, ct)) => tauri::http::ResponseBuilder::new()
                        .header("Content-Type", ct)
                        .header("X-Content-Type-Options", "nosniff")
                        .header("Cache-Control", "max-age=86400")
                        .header("Access-Control-Allow-Origin", "*")
                        .status(200)
                        .body(data),
                    None => tauri::http::ResponseBuilder::new()
                        .header("Access-Control-Allow-Origin", "*")
                        .status(404)
                        .body(b"no icon".to_vec()),
                };
            }

            if path == "/player/spectrum" {
                // Parse optional ?bands=N&ahead_ms=M query parameters.
                let bands: usize = query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix("bands="))
                    .and_then(|v: &str| v.parse::<usize>().ok())
                    .unwrap_or(48)
                    .min(128)
                    .max(1);
                let ahead_ms: f64 = query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix("ahead_ms="))
                    .and_then(|v: &str| v.parse::<f64>().ok())
                    .unwrap_or(35.0);
                let spectrum = player::controller::get().spectrum(bands, ahead_ms);
                return ok(spectrum, "application/octet-stream");
            }

            if path == "/player/spectrum_span" {
                // ?bands=N&from_ms=A&to_ms=B&step_ms=S — the big player's waves.
                let num = |key: &str, dflt: f64| query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix(key))
                    .and_then(|v: &str| v.parse::<f64>().ok())
                    .unwrap_or(dflt);
                let bands = (num("bands=", 48.0) as usize).clamp(1, 128);
                let span = player::controller::get().spectrum_span(
                    bands, num("from_ms=", 0.0), num("to_ms=", 2000.0), num("step_ms=", 40.0),
                    num("extra=", 0.0) > 0.0);
                return ok(span, "application/octet-stream");
            }

            if path == "/player/wave_span" {
                // ?from_ms=A&to_ms=B&rate=R — the visualization scenes' waveform.
                let num = |key: &str, dflt: f64| query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix(key))
                    .and_then(|v: &str| v.parse::<f64>().ok())
                    .unwrap_or(dflt);
                let span = player::controller::get().wave_span(
                    num("from_ms=", -250.0), num("to_ms=", 500.0), num("rate=", 24000.0));
                return ok(span, "application/octet-stream");
            }

            if path == "/player/spatial_span" {
                // ?from_ms=A&to_ms=B — the spatial scenes' sound objects.
                let num = |key: &str, dflt: f64| query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix(key))
                    .and_then(|v: &str| v.parse::<f64>().ok())
                    .unwrap_or(dflt);
                let span = spatial::span(num("from_ms=", -6000.0), num("to_ms=", 3500.0));
                return ok(span, "application/octet-stream");
            }

            if path == "/player/spatial_map" {
                // ?id=N — a track map's objects and notes (once per map; span's last header field)
                let id = query_str.split('&')
                    .find_map(|kv: &str| kv.strip_prefix("id="))
                    .and_then(|v: &str| v.parse::<u32>().ok())
                    .unwrap_or(0);
                if let Some(j) = spatial::map_json(id) {
                    return ok(j.into_bytes(), "application/json");
                }
            }

            // analytics: dispatch to analyzer routes (§2.2)
            if let Some(tail) = path.strip_prefix("/player/an/") {
                if let Some(bytes) = player::analytics::routes::handle(tail, query_str) {
                    return ok(bytes.to_vec(), "application/octet-stream");
                }
            }

            tauri::http::ResponseBuilder::new()
                .header("Access-Control-Allow-Origin", "*")
                .status(404)
                .body(b"not found".to_vec())
        })
        // The main window starts hidden (tauri.conf.json): it is put where it
        // was last time, then shown — never first in the default spot.
        .setup(|app| {
            if let Some(w) = app.get_window("main") {
                window_state::restore(&w);
                let _ = w.show();
                let _ = w.set_focus();
            }
            // The instruments pack: what earlier runs left is cleared before
            // anything loads from it, and a user who took the pack gets the
            // one this app pins (in the background, a while after start).
            spatial::pack::start(app.handle());
            Ok(())
        })
        // When the main window is closed, exit the whole process — the console
        // (log) window closes with it, and no XTC geometry window stays behind.
        // Where it was is kept for the next start (and while it moves).
        //
        // The page's own close button destroys the window without a
        // CloseRequested (Tauri 1: `appWindow.close()`), and with another
        // window open (the studio, the analyzer) the process lived on — the
        // music playing, the other window left behind. So the button asks for
        // `app_quit`, and a main window destroyed by any other way ends the
        // process too.
        .on_window_event(|event| {
            match event.event() {
                tauri::WindowEvent::CloseRequested { .. } if event.window().label() == "main" => {
                    window_state::closing(event.window());
                    player::controller::get().stop();
                    event.window().app_handle().exit(0);
                }
                tauri::WindowEvent::Destroyed if event.window().label() == "main" => {
                    player::controller::get().stop();
                    event.window().app_handle().exit(0);
                }
                tauri::WindowEvent::Moved(_) if event.window().label() == "main" => {
                    window_state::moved(event.window());
                }
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_)
                    if event.window().label() == "vis-studio"
                        || event.window().label().starts_with("analyzer") =>
                {
                    window_state::moved(event.window());
                }
                // Alt+F4 on an analyzer window (its own button keeps the
                // place first: window_keep_place).
                tauri::WindowEvent::CloseRequested { .. }
                    if event.window().label().starts_with("analyzer") =>
                {
                    window_state::closing(event.window());
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!());

    // `expect` here used to end the process with a message nobody could read.
    if let Err(e) = run {
        startup::tauri_failed(&e);
        std::process::exit(1);
    }
}
