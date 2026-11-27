//! The filter download's commands: the work is in the engine
//! (`aura_core::filters_dl`), shared with the streamer; the app lends it the
//! update key, its release flag and the updater's lock (`hooks`).

pub use aura_core::filters_dl::{in_flight, status_json};
use aura_core::filters_dl as dl;

/// Hands the engine's download what only the app knows. Called once at start.
pub fn hooks() {
    dl::set_hooks(dl::Hooks {
        pubkey: crate::updater::active_pubkey,
        release_build: crate::updater::is_release_build,
        unless_installing: |refusal, start| crate::updater::unless_installing(refusal, || start()),
    });
}

/// Plan the download for a setting: `taps` at `fs` in `phases` (`"linear"`,
/// `"minimum"`; Hybrid-Phase asks for both) for the source `families`
/// (`"44k"`, `"48k"`; empty = both).
#[tauri::command]
pub async fn filters_plan(
    taps: usize,
    fs: u32,
    phases: Vec<String>,
    families: Vec<String>,
    hires: Option<bool>,
) -> Result<serde_json::Value, String> {
    dl::plan(taps, fs, phases, families, hires).await
}

/// Download exactly `names` (from a plan) in the background; progress at
/// `/filters/status`.
#[tauri::command]
pub fn filters_download(names: Vec<String>) -> Result<(), String> {
    tauri::async_runtime::spawn(dl::start_download(names)?);
    Ok(())
}

/// Stop the download after the chunk in hand.
#[tauri::command]
pub fn filters_cancel() {
    dl::cancel()
}

/// The filter files in the download folder, for the storage view.
#[tauri::command]
pub async fn filters_storage() -> Result<serde_json::Value, String> {
    dl::storage().await
}

/// Move exactly `names` from the download folder to the Recycle Bin.
#[tauri::command]
pub async fn filters_trash(names: Vec<String>) -> Result<Vec<dl::storage::Removed>, String> {
    dl::trash(names).await
}

/// Show the download folder in the file manager (made if it is not there yet).
#[tauri::command]
pub fn filters_dir_open() -> Result<String, String> {
    let d = dl::dest_dir()?;
    std::fs::create_dir_all(&d).map_err(|_| dl::not_writable(&d))?;
    let opener = if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(&d).spawn().map_err(|e| e.to_string())?;
    Ok(d.display().to_string())
}
