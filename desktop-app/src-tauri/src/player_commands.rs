//! The player's Tauri commands (the converter's stay in main.rs). Thin:
//! argument plumbing into `aura_core::commands`.
//!
//! A plain `#[tauri::command]` runs on the window's own thread (Tauri 1), so
//! anything that touches the disk or COM — probing files, listing devices,
//! scanning the filter matrix — is `async` and hands the work to the
//! blocking pool: a frozen window is not an acceptable price for a slow USB
//! stick. The quick ones stay synchronous, which keeps them in the order the
//! page sent them (device, then play). The reads that only wait for the
//! player's lock (status, queue) are `command(async)`: they change nothing,
//! so their order does not matter, and a busy machine keeps the wait off the
//! window.

use aura_core::commands as engine;
use serde_json::Value;

use crate::player::controller::RepeatMode;
use crate::player::settings::PlayerSettings;

#[tauri::command]
pub async fn player_add(paths: Vec<String>, list: Option<u64>) -> Result<Value, String> {
    blocking(move || engine::player_add(paths, list)).await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn player_remove(id: u64) -> Result<(), String> {
    engine::player_remove(id);
    Ok(())
}

#[tauri::command]
pub fn player_reorder(ids: Vec<u64>) -> Result<(), String> {
    engine::player_reorder(ids);
    Ok(())
}

#[tauri::command]
pub fn player_clear() -> Result<(), String> {
    engine::player_clear();
    Ok(())
}

#[tauri::command]
pub fn player_play(id: Option<u64>) -> Result<(), String> {
    engine::player_play(id);
    Ok(())
}

#[tauri::command]
pub fn player_pause() -> Result<(), String> {
    engine::player_pause();
    Ok(())
}

#[tauri::command]
pub fn player_stop() -> Result<(), String> {
    engine::player_stop();
    Ok(())
}

#[tauri::command]
pub fn player_next() -> Result<(), String> {
    engine::player_next();
    Ok(())
}

#[tauri::command]
pub fn player_prev() -> Result<(), String> {
    engine::player_prev();
    Ok(())
}

#[tauri::command]
pub fn player_seek(seconds: f64) -> Result<(), String> {
    engine::player_seek(seconds);
    Ok(())
}

#[tauri::command]
pub fn player_set_volume(db: f64) -> Result<(), String> {
    engine::player_set_volume(db);
    Ok(())
}

#[tauri::command]
pub fn player_set_cue(params: crate::player::cue::CueParams) {
    engine::player_set_cue(params);
}

#[tauri::command(async)]
pub fn player_cue_view() -> crate::player::cue::CueView {
    engine::player_cue_view()
}

#[tauri::command]
pub fn player_set_eq(eq: crate::audio::converter::dsp::eq::EqState) -> Result<crate::player::output::EqReply, String> {
    Ok(engine::player_set_eq(eq))
}

#[tauri::command]
pub fn player_set_settings(
    settings: PlayerSettings,
    rendered: Option<Vec<engine::RenderedItem>>,
    instant_start: Option<bool>,
    lower_taps: Option<bool>,
    prepare_ahead: Option<bool>,
) -> Result<(), String> {
    engine::player_set_settings(settings, rendered, instant_start, lower_taps, prepare_ahead)
}

#[tauri::command]
pub fn radio_stream_wait(settings: PlayerSettings, src_rate: Option<u32>) -> Result<String, String> {
    engine::radio_stream_wait(settings, src_rate)
}

#[tauri::command(async)]
pub fn player_status() -> Result<Value, String> {
    Ok(engine::player_status())
}

#[tauri::command]
pub async fn player_dsd_capabilities(remeasure: Option<bool>) -> Result<Value, String> {
    blocking(move || engine::player_dsd_capabilities(remeasure)).await
}

#[tauri::command]
pub fn player_radio(url: String) -> Result<(), String> {
    engine::player_radio(url)
}

/// Fetches this build's driver from its release into `driver::fetch_dirs()`.
async fn fetch_driver() -> Result<(), crate::updater::AsioFetchError> {
    use crate::player::input::driver;
    use crate::updater::AsioFetchError;
    let Some(want) = driver::expected() else {
        return Err(AsioFetchError::Mismatch("this build has no driver of its own".into()));
    };
    let url = crate::updater::asio_asset_url(env!("CARGO_PKG_VERSION"));
    crate::updater::fetch_asio_driver(&url, want, &driver::fetch_dirs()).await.map(|_| ())
}

/// The input: install (or update) Aura Player ASIO — Windows asks for
/// permission once. A release build without its driver at hand fetches it
/// from its release first. 'ok', 'cancelled' (permission not given),
/// 'network' (not downloaded), 'mismatch' (downloaded, not this build's) or
/// 'failed'.
#[tauri::command]
pub async fn player_input_install() -> Result<String, String> {
    use crate::player::input::{driver, set_installing};
    use crate::updater::AsioFetchError;
    crate::aelog!("[UI] input: install the driver");
    if blocking(driver::needs_fetch).await? {
        set_installing(Some("download"));
        let r = fetch_driver().await;
        if let Err(e) = r {
            set_installing(None);
            crate::aelog!("[INPUT] the driver was not fetched: {:?}", e);
            return Ok(match e {
                AsioFetchError::Network(_) => "network",
                AsioFetchError::Mismatch(_) => "mismatch",
                AsioFetchError::Write(_) => "failed",
            }
            .into());
        }
    }
    set_installing(Some("install"));
    let r = blocking(driver::install).await;
    set_installing(None);
    let r = r?;
    crate::player::input::driver_changed();
    crate::aelog!("[INPUT] driver install: {:?}", r);
    outcome_word(r)
}

/// The input: remove Aura Player ASIO (unregistered, its files deleted).
#[tauri::command]
pub async fn player_input_remove() -> Result<String, String> {
    crate::aelog!("[UI] input: remove the driver");
    let r = blocking(crate::player::input::driver::remove).await?;
    crate::player::input::driver_changed();
    crate::aelog!("[INPUT] driver removal: {:?}", r);
    outcome_word(r)
}

fn outcome_word(r: crate::player::input::driver::Outcome) -> Result<String, String> {
    match serde_json::to_value(r).map_err(|e| e.to_string())? {
        Value::String(w) => Ok(w),
        v => Ok(v.to_string()),
    }
}

/// The input is there: Aura Player ASIO is a Windows driver, so on other
/// systems the list has no Input view.
#[tauri::command]
pub fn player_input_supported() -> bool {
    cfg!(windows)
}

#[tauri::command]
pub fn player_input_listen() -> Result<(), String> {
    engine::player_input_listen();
    Ok(())
}

#[tauri::command]
pub fn player_input_auto(on: bool) -> Result<(), String> {
    engine::player_input_auto(on);
    Ok(())
}

#[tauri::command]
pub async fn player_paths_exist(paths: Vec<String>) -> Result<Vec<bool>, String> {
    blocking(move || engine::player_paths_exist(paths)).await
}

#[tauri::command]
pub async fn player_render_checks(paths: Vec<String>) -> Result<Vec<Value>, String> {
    blocking(move || engine::player_render_checks(paths)).await
}

/// Enumerated off the window's thread: that thread is a COM STA, where the
/// MTA the device enumerator is written for cannot be entered.
#[tauri::command]
pub async fn player_devices() -> Result<Value, String> {
    blocking(engine::player_devices).await?
}

#[tauri::command]
pub fn player_set_device(id: Option<String>) -> Result<(), String> {
    engine::player_set_device(id);
    Ok(())
}

#[tauri::command(async)]
pub fn player_queue() -> Result<Value, String> {
    engine::player_queue()
}

#[tauri::command]
pub fn player_set_repeat(mode: RepeatMode) -> Result<(), String> {
    engine::player_set_repeat(mode);
    Ok(())
}

#[tauri::command]
pub fn player_set_vis_ahead(source: String, seconds: f64) -> Result<(), String> {
    engine::player_set_vis_ahead(source, seconds);
    Ok(())
}

#[tauri::command]
pub fn player_set_track_settings(id: u64, settings: PlayerSettings) -> Result<(), String> {
    engine::player_set_track_settings(id, settings);
    Ok(())
}

#[tauri::command]
pub fn player_clear_track_settings(id: u64) -> Result<(), String> {
    engine::player_clear_track_settings(id);
    Ok(())
}

/// The pointer in the window's client area, in CSS pixels. Files dragged in
/// from Explorer give the page no pointer events while they hover (the
/// webview's drop target has them), so the list asks here where to open a
/// gap for them.
#[tauri::command]
pub fn ui_cursor_client(window: tauri::Window) -> Option<(f64, f64)> {
    // The pointer on the screen less where the client area starts, in
    // physical pixels, then in CSS pixels.
    let p = window.cursor_position().ok()?;
    let at = window.inner_position().ok()?;
    let scale = window.scale_factor().ok()?.max(0.1);
    Some(((p.x - at.x as f64) / scale, (p.y - at.y as f64) / scale))
}

#[tauri::command]
pub fn player_ui_log(level: String, message: String) {
    engine::player_ui_log(level, message);
}

/// The developer's blind test (js/blindtest.js): `AURA_BLIND_TEST=<trials>`
/// (1–40) in the environment opens the app behind its curtain. None: the
/// app as usual.
#[tauri::command]
pub fn blind_test_trials() -> Option<u32> {
    std::env::var("AURA_BLIND_TEST").ok()?.trim().parse().ok().filter(|n| (1..=40).contains(n))
}
