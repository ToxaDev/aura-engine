//! The windows' side of `aura_core::spatial`: the commands, and its events
//! going out to every window.

use crate::spatial::pack;
use aura_core::events::Events;
use tauri::Emitter;

fn events(app: tauri::AppHandle) -> Events {
    Events::new(move |name, payload| {
        let _ = app.emit(name, payload);
    })
}

/// At start: what earlier runs left is cleared; for a user who took the pack,
/// the one this app pins is fetched in the background if it is not there.
pub fn start(app: tauri::AppHandle) {
    if pack::start() {
        tauri::async_runtime::spawn(pack::keep_up_to_date(events(app)));
    }
}

/// Is a pack there, which, is the user keeping it, and what is an install doing.
#[tauri::command(async)]
pub fn spatial_status() -> serde_json::Value {
    pack::spatial_status()
}

/// Stop the install that is running (what came so far is kept to go on from).
#[tauri::command]
pub fn spatial_cancel() {
    pack::spatial_cancel()
}

/// The user takes the pack: download, check, unpack. Resolves when it is in
/// (from then on it is kept up to date), or with the reason it is not;
/// `spatial:progress` events on the way.
#[tauri::command]
pub async fn spatial_install(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    pack::spatial_install(events(app)).await
}

/// The same from a zip at hand (our checks, offline): the same pinned hash.
#[tauri::command]
pub async fn spatial_install_file(app: tauri::AppHandle, path: String) -> Result<serde_json::Value, String> {
    pack::spatial_install_file(events(app), path).await
}

/// Keep the pack up to date with the app (on), or not (off: an update under
/// way stops, nothing more is downloaded).
#[tauri::command]
pub fn spatial_set_wanted(on: bool) -> Result<serde_json::Value, String> {
    pack::spatial_set_wanted(on)
}

/// The user removes the pack: every pack and download deleted, no more
/// updates. Resolves with the bytes freed and whether some wait for the
/// next start (in use now).
#[tauri::command]
pub async fn spatial_remove(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    pack::spatial_remove(events(app)).await
}

/// For the page: is the pack there, how far the heard track's analysis is.
#[tauri::command(async)]
pub fn spatial_track_status() -> serde_json::Value {
    crate::spatial::spatial_track_status()
}
