//! The load window's side of `aura_core::perf`.

use tauri::Emitter;

/// The sampler; the main and load windows hear each second as `perf:tick`.
pub fn start(app: tauri::AppHandle) {
    crate::perf::start(move |s| {
        // Only the main window (its load badge) and the load window listen;
        // the others are not woken every second for nothing.
        for label in ["main", "perf"] {
            let _ = app.emit_to(label, "perf:tick", s);
        }
    });
}

/// The kept seconds, oldest first, for the load window.
#[tauri::command]
pub fn perf_history() -> crate::perf::History {
    crate::perf::perf_history()
}

/// Open (or bring up) the load window: frameless like the others, where it
/// was last time (window_state.rs). `async` for the same reason as the
/// analyzer's (wry#583).
#[tauri::command]
pub async fn perf_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl};
    if let Some(w) = app.get_webview_window("perf") {
        let _ = w.unminimize();
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let w = tauri::WebviewWindowBuilder::new(&app, "perf", WebviewUrl::App("perf.html".into()))
        .title("Aura Engine - Load")
        .inner_size(680.0, 560.0)
        .min_inner_size(380.0, 360.0)
        .resizable(true)
        .decorations(false)
        .transparent(true)
        // No Tauri 2 shadow (its 1px border and DWM frame): as on Tauri 1.
        .shadow(false)
        .visible(false)
        .use_https_scheme(true)
        .build()
        .map_err(|e| e.to_string())?;
    crate::window_state::restore_all(&w.as_ref().window());
    let _ = w.show();
    let _ = w.set_focus();
    Ok(())
}
