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

// The engine; the rest of this crate reaches it as it always did.
pub use aura_core::aelog;
pub use aura_core::{app_dir, audio, gpu_sched, perf, player, spatial, watchdog};

mod crash_report;
mod perf_commands;
mod player_commands;
mod radio_catalog;
mod self_install;
mod single_instance;
mod spatial_commands;
mod aura_protocol;
mod filters_dl;
mod eq_autoeq;
mod motion_test;
mod wv2_extra;
mod startup;
mod ui_pack;
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
    upsampler: Option<crate::player::settings::Upsampler>,
    dsd_output: Option<crate::audio::converter::dsd_out::DsdExport>,
    rack_sig: Option<String>,
    // On Auto: the shifted grid (the default when the page sends none).
    auto_shift: Option<bool>,
) -> Result<(), String> {
    // 100M–1G from a page before 2.0.1: Auto took their place.
    let taps = if custom_filter_path.is_none() {
        crate::audio::converter::dsp::filter::normalize_taps(taps as usize) as u32
    } else {
        taps
    };
    // Only a length the filter set is built for (a custom filter brings its
    // own): anything else would be rendered by a neighbour and still named
    // after the asked length.
    if custom_filter_path.is_none() {
        crate::audio::converter::dsp::filter::check_taps(taps as usize)?;
    }
    // DSF output: only a modulator built for that rate.
    if let Some(d) = dsd_output {
        if !crate::audio::converter::dsd_out::modulators(d.multiple).iter().any(|e| e.kind == d.kind) {
            return Err(format!("The {} modulator is not available for DSD{}", d.kind.label(), d.multiple));
        }
    }
    if subsonic_hz != 0
        && !crate::audio::converter::apodize::SUBSONIC_CORNERS_HZ.contains(&subsonic_hz)
    {
        return Err(format!("Subsonic filter: unsupported corner {} Hz", subsonic_hz));
    }
    let mut settings = crate::audio::converter::ConvertSettings {
        out_rate: 0,      // Computed per-file in prepare.rs from src_rate x family_base x fs_multiplier
        fs_multiplier,    // FS slider value: 2, 4, 8, 16 or 32
        taps: taps as usize,
        auto_shift: auto_shift.unwrap_or(true),
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
        // Multistage unless the page's hidden switch (or AURA_MULTISTAGE=0) says single.
        upsampler: crate::player::settings::Upsampler::resolve(upsampler),
        dsd: dsd_output, // None = FLAC
        rack_sig,
    };
    settings.lab = lab_features.unwrap_or_default();
    // Album level is on unless the page says otherwise (the rack's ALB stage).
    let album_level = album_level.unwrap_or(true);
    // Not while an update is being installed: the app is about to restart.
    crate::updater::start_unless_installing(|| {
        crate::audio::converter::convert_batch(paths, settings, album_level)
    })
}

/// Why TFS cannot be had at `taps` on this machine's RAM, or None: the rack
/// reads its unit dim with this as the reason (`dsprack.js` tfsWhyNow).
#[tauri::command(async)]
fn tfs_unavailable(taps: usize) -> Option<String> {
    crate::audio::converter::dsp::lab::tfs::unavailable_here(taps)
}

/// The modulators the converter offers for DSF at DSD`multiple`: built
/// ones only, best first (the catalog's order).
#[tauri::command(async)]
fn dsd_export_modulators(multiple: u32) -> Vec<crate::audio::converter::dsd_out::ModulatorEntry> {
    crate::audio::converter::dsd_out::modulators(multiple)
}

#[tauri::command(async)]
fn check_filters(
    paths: Vec<String>,
    fs_multiplier: u32,
    taps: u32,
    custom_filter_path: Option<String>,
    hybrid_phase: bool,
    tfs_phase: bool,
) -> String {
    aura_core::commands::check_filters(paths, fs_multiplier, taps, custom_filter_path, hybrid_phase, tfs_phase)
}

#[tauri::command(async)]
fn gpu_status() -> String {
    aura_core::commands::gpu_status()
}

#[tauri::command(async)]
fn gpu_info() -> String {
    aura_core::commands::gpu_info()
}

#[tauri::command(async)]
fn filter_inventory() -> String {
    aura_core::commands::filter_inventory()
}

#[tauri::command(async)]
fn get_conversion_progress() -> (u32, u32, u32, String, String, u32, bool, bool) {
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

#[tauri::command(async)]
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
    use tauri::{Manager, WebviewUrl};

    if let Some(w) = app.get_webview_window("xtc-geometry") {
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let w = tauri::WebviewWindowBuilder::new(
        &app,
        "xtc-geometry",
        WebviewUrl::App("xtc-geometry.html".into()),
    )
    .title("XTC - listening geometry")
    // The smallest that shows the whole page without a scroll bar (both
    // ways of measuring, measured 1.11); then where it was last time.
    .inner_size(730.0, 880.0)
    .center()
    .min_inner_size(520.0, 520.0)
    .resizable(true)
    // Frameless like the main window: the page draws its own head bar.
    .decorations(false)
    .transparent(true)
    // No Tauri 2 shadow (its 1px border and DWM frame): as on Tauri 1.
    .shadow(false)
    // The pages' origin stays https://tauri.localhost (what they saved).
    .use_https_scheme(true)
    // Placed where it was last time, then shown (window_state.rs).
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    window_state::restore_all(&w.as_ref().window());
    let _ = w.show();
    let _ = w.set_focus();

    Ok(())
}

/// Open (or focus) the Cue Consistency window; `async` as above.
#[tauri::command]
async fn cue_window_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl};

    if let Some(w) = app.get_webview_window("cue") {
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let w = tauri::WebviewWindowBuilder::new(&app, "cue", WebviewUrl::App("cue.html".into()))
        .title("Cue Consistency")
        .inner_size(720.0, 800.0)
        .center()
        .min_inner_size(640.0, 560.0)
        .resizable(true)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .use_https_scheme(true)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    window_state::restore_all(&w.as_ref().window());
    let _ = w.show();
    let _ = w.set_focus();
    Ok(())
}

/// Open (or focus) the Aura EQ window.
///
/// `tab`: null = open on the graph, "import" = open on the import panel.
/// When the window is already open, focus it and emit `eq:show` so it
/// can switch to the requested tab without rebuilding.
///
/// `async` for the same reason as `xtc_geometry_open` (wry#583 on Windows).
#[tauri::command]
async fn eq_window_open(app: tauri::AppHandle, tab: Option<String>) -> Result<(), String> {
    use tauri::{Emitter, Manager, WebviewUrl};

    if let Some(w) = app.get_webview_window("eq") {
        w.set_focus().map_err(|e| e.to_string())?;
        let _ = w.emit("eq:show", serde_json::json!({ "tab": tab }));
        return Ok(());
    }

    let url = if tab.as_deref() == Some("import") { "eq.html#import" } else { "eq.html" };

    let w = tauri::WebviewWindowBuilder::new(
        &app,
        "eq",
        WebviewUrl::App(url.into()),
    )
    .title("Aura EQ")
    // The smallest that shows everything without a scroll bar, the import
    // panel open over the welcome included (measured 1.11); then where it
    // was last time.
    .inner_size(820.0, 700.0)
    .center()
    .min_inner_size(760.0, 520.0)
    .resizable(true)
    // Frameless like the main window: the page draws its own head bar.
    .decorations(false)
    .transparent(true)
    // No Tauri 2 shadow (its 1px border and DWM frame): as on Tauri 1.
    .shadow(false)
    // The page's origin stays https://tauri.localhost (what it saved).
    .use_https_scheme(true)
    // Placed where it was last time, then shown (window_state.rs).
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    window_state::restore_all(&w.as_ref().window());
    let _ = w.show();
    let _ = w.set_focus();

    Ok(())
}

/// How long the quit waits for the player's stop: its fade and the device's
/// silence go out before the process ends (`stop_and_wait`), 0.3 s at most;
/// a worker busy elsewhere is not waited for past this.
const QUIT_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Quit from the main window's close button: where the window was is kept,
/// the player stops, every other window goes with the process.
#[tauri::command]
fn app_quit(app: tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        window_state::closing(&w.as_ref().window());
    }
    player::controller::get().stop_and_wait(QUIT_STOP_WAIT);
    player::input::shutdown();
    player::output::restore_devices();
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
async fn an_window_png(window: tauri::WebviewWindow) -> Result<String, String> {
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
    use tauri::{Manager, WebviewUrl};
    if let Some(win) = app.get_webview_window(&label) {
        win.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    // Analyzer windows already open: a new one goes a step down and right of
    // the remembered place, so it does not hide them.
    let others = app.webview_windows().keys().filter(|l| l.starts_with("analyzer")).count();
    let w = tauri::WebviewWindowBuilder::new(
        &app,
        label,
        WebviewUrl::App(url.into()),
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
    // No Tauri 2 shadow (its 1px border and DWM frame): as on Tauri 1.
    .shadow(false)
    // Placed where the analyzer was last time (size, maximized, whole
    // screen), then shown (window_state.rs).
    .visible(false)
    .use_https_scheme(true)
    .build()
    .map_err(|e| e.to_string())?;
    window_state::restore_all(&w.as_ref().window());
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
        format!("  Aura Engine {}", ui_pack::version_label()),
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
    if player::selftest::handle_args() || player::render_bench::handle_args() {
        return;
    }

    // After an in-app update the new exe starts with --after-update <old-pid>.
    // Wait for the old process to finish and clean up its .old file before
    // doing anything else — including taking the single-instance lock.
    updater::handle_after_update();
    // The engine's filter download takes the app's key, release flag and update lock.
    filters_dl::hooks();

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
    // What this session is doing, in the log as it goes; and whether the last
    // one ended without a clean exit.
    watchdog::session_begin(session_log.as_deref());
    watchdog::watch_console_close();
    watchdog::start();

    // Missing AVX2, missing WebView2. Both are ordinary on machines we do not
    // own, and both are worth a sentence the user can act on rather than an
    // error they cannot read.
    if !startup::preflight() {
        std::process::exit(1);
    }

    // The interface pack beside the exe, checked before the version is first
    // shown (the banner names it).
    ui_pack::init();
    print_startup_banner(session_log.as_deref());
    // AURA_WV2_EXTRA: extra WebView2 arguments for A/B tests; unset = none.
    wv2_extra::apply();
    // The converter's album level estimates each track's output peak with the
    // player's short render of the same chain; the engine alone has none.
    audio::converter::album::install_peak_probe(player::album_probe::converter_probe);
    // analytics: initialize hub (and live consumer thread) before the WebView2
    // protocol closure so routes are available from the first request.
    player::analytics::hub::init();
    let run = tauri::Builder::default()
        // The pages' dialogs, file writes (the analyzer's exports), links
        // opened in the browser, and the studio's clipboard.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            convert_files,
            dsd_export_modulators,
            get_conversion_progress,
            cancel_conversion,
            cancel_file,
            get_queue_status,
            set_custom_filter,
            clear_custom_filter,
            check_filters,
            gpu_status,
            gpu_info,
            filter_inventory,
            filters_dl::filters_plan,
            eq_autoeq::eq_autoeq_index,
            eq_autoeq::eq_autoeq_fetch,
            filters_dl::filters_download,
            filters_dl::filters_cancel,
            filters_dl::filters_storage,
            filters_dl::filters_trash,
            filters_dl::filters_dir_open,
            xtc_geometry_open,
            eq_window_open,
            tfs_unavailable,
            app_quit,
            crash_report::crash_report_state,
            crash_report::crash_report_build,
            crash_report::crash_report_send,
            crash_report::crash_report_save,
            crash_report::crash_report_done,
            crash_report::crash_report_retry,
            perf_commands::perf_history,
            perf_commands::perf_open,
            vis_store::vis_studio_open,
            vis_store::vis_list,
            vis_store::vis_save,
            vis_store::vis_delete,
            vis_store::vis_dir_open,
            vis_store::vis_export,
            vis_store::vis_import,
            vis_store::vis_restore_app,
            window_state::window_keep_place,
            spatial_commands::spatial_status,
            spatial_commands::spatial_install,
            spatial_commands::spatial_install_file,
            spatial_commands::spatial_cancel,
            spatial_commands::spatial_set_wanted,
            spatial_commands::spatial_remove,
            spatial_commands::spatial_track_status,
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
            player_commands::player_set_eq,
            player_commands::player_set_cue,
            player_commands::player_cue_view,
            cue_window_open,
            player_commands::player_status,
            player_commands::player_dsd_capabilities,
            player_commands::player_radio,
            player_commands::radio_stream_wait,
            player_commands::player_input_install,
            player_commands::player_input_remove,
            player_commands::player_input_supported,
            player_commands::player_input_listen,
            player_commands::player_input_auto,
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
            player_commands::player_render_checks,
            player_commands::player_queue,
            player_commands::player_set_repeat,
            player_commands::player_set_vis_ahead,
            player_commands::player_set_track_settings,
            player_commands::player_clear_track_settings,
            updater::update_check,
            updater::update_install,
            updater::ui_pack_install,
            ui_pack::ui_pack_info,
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
        // Answered on the protocol's own threads, not on the one that runs
        // the windows: a request that waits for the player's state (a rack
        // being rebuilt, a machine under full load) no longer holds up
        // painting and input (aura_protocol.rs).
        .register_asynchronous_uri_scheme_protocol("aura", |_ctx, req, responder| {
            aura_protocol::serve(req.uri().to_string(), move |r| responder.respond(r));
        })
        // The main window starts hidden (tauri.conf.json): it is put where it
        // was last time, then shown — never first in the default spot.
        .setup(|app| {
            if let Some(w) = app.get_webview_window("main") {
                window_state::restore(&w.as_ref().window());
                let _ = w.show();
                let _ = w.set_focus();
            }
            // The instruments pack: what earlier runs left is cleared before
            // anything loads from it, and a user who took the pack gets the
            // one this app pins (in the background, a while after start).
            spatial_commands::start(app.handle().clone());
            // The input: the link players play into through Aura Player ASIO.
            player::controller::start_input();
            // This app's load, once a second, for the badge and the load window.
            perf_commands::start(app.handle().clone());
            // Linux and macOS: a second copy asks this one to come forward.
            single_instance::serve(app.handle().clone());
            // A steady-motion test window, only with AURA_MOTION_TEST set.
            motion_test::start(app.handle());
            // GPU priority classes, only with AURA_WV2_GPU_PRIO / AURA_SELF_GPU_PRIO set.
            gpu_sched::start();
            Ok(())
        })
        // When the main window is closed, exit the whole process — the console
        // (log) window closes with it, and no XTC geometry window stays behind.
        // Where it was is kept for the next start (and while it moves).
        //
        // The page's own close button destroys the window without a
        // CloseRequested (`appWindow.close()` in Tauri 1), and with another
        // window open (the studio, the analyzer) the process lived on — the
        // music playing, the other window left behind. So the button asks for
        // `app_quit`, and a main window destroyed by any other way ends the
        // process too.
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { .. } if window.label() == "main" => {
                    window_state::closing(window);
                    player::controller::get().stop_and_wait(QUIT_STOP_WAIT);
                    player::input::shutdown();
                    player::output::restore_devices();
                    window.app_handle().exit(0);
                }
                tauri::WindowEvent::Destroyed if window.label() == "main" => {
                    player::controller::get().stop_and_wait(QUIT_STOP_WAIT);
                    player::input::shutdown();
                    player::output::restore_devices();
                    window.app_handle().exit(0);
                }
                tauri::WindowEvent::Moved(_) if window.label() == "main" => {
                    window_state::moved(window);
                }
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_)
                    if window.label() == "vis-studio"
                        || window.label() == "perf"
                        || window.label() == "eq"
                        || window.label() == "xtc-geometry"
                        || window.label() == "cue"
                        || window.label().starts_with("analyzer") =>
                {
                    window_state::moved(window);
                }
                // Alt+F4 on an analyzer window or the load window (their own
                // buttons keep the place first: window_keep_place).
                tauri::WindowEvent::CloseRequested { .. }
                    if window.label().starts_with("analyzer")
                        || window.label() == "perf"
                        || window.label() == "eq"
                        || window.label() == "cue"
                        || window.label() == "xtc-geometry" =>
                {
                    window_state::closing(window);
                }
                _ => {}
            }
        })
        .build(ui_pack::context(tauri::generate_context!()))
        .map(|app| {
            app.run(|_, event| {
                if let tauri::RunEvent::Exit = event {
                    watchdog::session_end();
                }
            })
        });

    // `expect` here used to end the process with a message nobody could read.
    if let Err(e) = run {
        startup::tauri_failed(&e);
        std::process::exit(1);
    }
}
