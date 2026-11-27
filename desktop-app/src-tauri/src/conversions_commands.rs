//! The library's Conversions view: the files the converter wrote beside their
//! sources (`<stem> [AE · <tags>].flac|dsf`), found in the music folders and
//! any folders the view adds, with what their names say, their sizes and their
//! sources; deleted or shown in the file manager from there. What they are and
//! how they are found and deleted is the engine's
//! (`aura_core::library::conversions`), shared with the streamer.

use aura_core::library::conversions;
use serde_json::Value;
use std::path::PathBuf;

pub use conversions::Trashed;

/// The converted files in the music folders and in `folders`:
/// `{ files: [...], scanned: <folders read>, ms }`. Names only, no headers:
/// `conv_check` reads the stamps.
#[tauri::command]
pub async fn conv_scan(folders: Vec<String>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut roots = crate::library_commands::lib().roots();
        roots.extend(folders.into_iter().filter(|f| !f.is_empty()).map(PathBuf::from));
        conversions::scan_json(roots)
    })
    .await
    .map_err(|e| e.to_string())
}

/// The versions of `sources` made with the rack's settings `sig` (their
/// stamps carry its hash): `[{ source, out, kind, dsd, rate, mtimeMs }]`
/// (`conversions::matching`), so a track plays its version without a record.
#[tauri::command]
pub async fn conv_matching(sig: String, sources: Vec<String>) -> Result<Vec<Value>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sources: Vec<PathBuf> = sources.into_iter().filter(|s| !s.is_empty()).map(PathBuf::from).collect();
        conversions::matching(&sig, &sources)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Each file's stamp: `[{ exists, valid, tip }]`, as `player_render_checks`.
#[tauri::command]
pub async fn conv_check(paths: Vec<String>) -> Result<Vec<Value>, String> {
    tauri::async_runtime::spawn_blocking(move || aura_core::commands::player_render_checks(paths))
        .await
        .map_err(|e| e.to_string())
}

/// Delete converted files for good; anything else is refused
/// (`conversions::delete`).
#[tauri::command]
pub async fn conv_trash(paths: Vec<String>) -> Result<Vec<Trashed>, String> {
    tauri::async_runtime::spawn_blocking(move || conversions::delete(&paths))
        .await
        .map_err(|e| e.to_string())
}

/// Show `path` selected in the system's file manager (its folder, on Linux).
#[tauri::command]
pub fn conv_reveal(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("no such file".to_string());
    }
    #[cfg(target_os = "windows")]
    let r = {
        use std::os::windows::process::CommandExt;
        // explorer reads its switch and the quoted path as one argument.
        std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", p.display())).spawn()
    };
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(&p).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(&p)).spawn();
    r.map(|_| ()).map_err(|e| e.to_string())
}
