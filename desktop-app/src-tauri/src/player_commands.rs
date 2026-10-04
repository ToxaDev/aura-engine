//! The player's Tauri commands (the converter's stay in main.rs). Thin:
//! argument plumbing into `player::controller`.
//!
//! A plain `#[tauri::command]` runs on the window's own thread (Tauri 1), so
//! anything that touches the disk or COM — probing files, listing devices,
//! scanning the filter matrix — is `async` and hands the work to the
//! blocking pool: a frozen window is not an acceptable price for a slow USB
//! stick. The quick ones stay synchronous, which keeps them in the order the
//! page sent them (device, then play).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::player::controller::{self, RepeatMode};
use crate::player::settings::PlayerSettings;

/// `{ tracks: [...], errors: ["Cannot open …", …] }` — the readable files
/// are queued even when some are not.
#[tauri::command]
pub async fn player_add(paths: Vec<String>) -> Result<Value, String> {
    let asked = paths.len();
    let (added, errors) = blocking(move || controller::get().add(paths)).await?;
    crate::aelog!("[UI] add: {} of {} file(s) readable", added.len(), asked);
    for e in &errors {
        crate::aelog!("[UI] add: {}", e);
    }
    Ok(serde_json::json!({ "tracks": added, "errors": errors }))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn player_remove(id: u64) -> Result<(), String> {
    controller::get().remove(id);
    Ok(())
}

/// The list's new order (a row dragged): the track ids from the top.
#[tauri::command]
pub fn player_reorder(ids: Vec<u64>) -> Result<(), String> {
    controller::get().reorder(ids);
    Ok(())
}

#[tauri::command]
pub fn player_clear() -> Result<(), String> {
    controller::get().clear();
    Ok(())
}

#[tauri::command]
pub fn player_play(id: Option<u64>) -> Result<(), String> {
    crate::aelog!("[UI] play {:?}", id);
    controller::get().play(id);
    Ok(())
}

#[tauri::command]
pub fn player_pause() -> Result<(), String> {
    controller::get().pause();
    Ok(())
}

#[tauri::command]
pub fn player_stop() -> Result<(), String> {
    controller::get().stop();
    Ok(())
}

#[tauri::command]
pub fn player_next() -> Result<(), String> {
    controller::get().next();
    Ok(())
}

#[tauri::command]
pub fn player_prev() -> Result<(), String> {
    controller::get().prev();
    Ok(())
}

#[tauri::command]
pub fn player_seek(seconds: f64) -> Result<(), String> {
    controller::get().seek(seconds);
    Ok(())
}

#[tauri::command]
pub fn player_set_volume(db: f64) -> Result<(), String> {
    crate::aelog!("[UI] volume: {db} dB");
    controller::get().set_volume(db);
    Ok(())
}

/// One entry in the `rendered` map: track id → path of its converted file
/// (null means play live for that track).
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedItem {
    pub id: u64,
    pub path: Option<String>,
}

/// `instant_start`: the player menu's Instant start tick, which the page sends
/// with the rack. A setting of the player's own, not the rack's: it makes no
/// variant and no Apply, so it is not in `PlayerSettings`.
/// A stream plays the rack, its length too (Anton 2.10): its own linear
/// filter no longer keeps it waiting (`stream_linear`), so streams have no
/// length of their own to send.
#[tauri::command]
pub fn player_set_settings(
    settings: PlayerSettings,
    rendered: Option<Vec<RenderedItem>>,
    instant_start: Option<bool>,
) -> Result<(), String> {
    if let Some(on) = instant_start {
        controller::get().set_instant_start(on);
    }
    if !crate::audio::converter::dsp::filter::TAP_LADDER.contains(&settings.taps) {
        return Err(format!("Unsupported tap count {}", settings.taps));
    }
    if ![2, 4, 8, 16].contains(&settings.fs_multiplier) {
        return Err(format!("Unsupported FS multiplier {}", settings.fs_multiplier));
    }
    if settings.subsonic_hz != 0
        && !crate::audio::converter::apodize::SUBSONIC_CORNERS_HZ.contains(&settings.subsonic_hz)
    {
        return Err(format!("Subsonic filter: unsupported corner {} Hz", settings.subsonic_hz));
    }
    crate::aelog!("[UI] settings: {:?}", settings);
    // The `rendered` map replaces the whole track→file mapping when present.
    // Each entry says which converted file to play for a given track id; absent
    // tracks (or null paths) play live. The controller applies it alongside the
    // DSP settings — one Apply, not two.
    //
    // When only the rendered map changes (settings identical) set_settings
    // returns false without sending Job::Apply. We call apply() explicitly
    // when the playing track's file changed, so a finished conversion is
    // picked up even when the rack was not touched — and only then: the page
    // resends the map as rows come and go, and an Apply rebuilds (from disk:
    // restarts) what plays.
    if let Some(r) = rendered {
        let map: Vec<(u64, Option<String>)> = r.into_iter().map(|e| (e.id, e.path)).collect();
        let on_air = controller::get().set_rendered(map);
        let changed = controller::get().set_settings(settings);
        if !changed && on_air {
            controller::get().apply();
        }
    } else {
        controller::get().set_settings(settings);
    }
    Ok(())
}

/// The length a stream plays with rack `settings` on a stream at `src_rate`
/// (the rack's, or the nearest installed one), and the stream's own linear
/// filter of it made now, ahead of the stream (`radio::chain::prepare_ahead`:
/// once, then kept; what was being made ahead for another rack stops).
fn stream_length_ahead(settings: &PlayerSettings, rate: u32) -> Option<usize> {
    use crate::player::radio::chain as live_chain;
    let plays = live_chain::installed_length(settings, rate);
    match plays {
        Some(taps) => live_chain::prepare_ahead(&PlayerSettings { taps, ..settings.clone() }, rate),
        None => crate::audio::converter::dsp::lab::stream_linear::prepare_ahead(None),
    }
    plays
}

/// What a stream waits for before its first sound with rack `settings`, on a
/// stream at `src_rate` (the 44.1 kHz family when none plays) — reckoned by
/// the radio's own plan (`radio::chain::filter_wait`) for the length that
/// plays (`plays`: the rack's, or the nearest installed; null: none is). The
/// page asks it as the radio is shown and as the rack changes: the stream's
/// own linear filter of that length is made then, ahead of the stream, and
/// `preparing` says while one is being made (the status's `radio.preparing`
/// too).
#[tauri::command]
pub fn radio_stream_wait(settings: PlayerSettings, src_rate: Option<u32>) -> Result<String, String> {
    use crate::player::radio::chain as live_chain;
    let rate = src_rate.filter(|&r| r > 0).unwrap_or(44_100);
    let plays = stream_length_ahead(&settings, rate);
    let wait = match plays {
        Some(taps) => Some(live_chain::filter_wait(&PlayerSettings { taps, ..settings.clone() }, rate)?),
        None => None,
    };
    Ok(serde_json::json!({
        "rate": rate,
        "plays": plays,
        "label": plays.and_then(crate::audio::converter::dsp::filter::taps_label),
        "needS": wait.map(|w| w.need_s),
        "lookAheadS": wait.map(|w| w.look_ahead_s),
        "preparing": crate::audio::converter::dsp::lab::stream_linear::preparing(),
    })
    .to_string())
}

#[tauri::command]
pub fn player_status() -> Result<Value, String> {
    Ok(controller::get().status())
}

/// The radio experiment: play an internet radio stream (an http(s) address,
/// a .pls or .m3u playlist) through the rack. No button calls it yet.
#[tauri::command]
pub fn player_radio(url: String) -> Result<(), String> {
    let url = url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("A stream address starts with http:// or https://".into());
    }
    controller::get().radio(url);
    Ok(())
}

/// Check whether a list of file paths exist on disk (for restoring playlists:
/// conversion records whose output file is gone are dropped on restore).
/// Runs on the blocking pool to avoid stalling the UI thread.
#[tauri::command]
pub async fn player_paths_exist(paths: Vec<String>) -> Result<Vec<bool>, String> {
    blocking(move || {
        paths.iter().map(|p| std::path::Path::new(p).exists()).collect()
    })
    .await
}

/// Enumerated off the window's thread: that thread is a COM STA, where the
/// MTA the device enumerator is written for cannot be entered.
#[tauri::command]
pub async fn player_devices() -> Result<Value, String> {
    let d = blocking(crate::player::output::list_devices).await??;
    serde_json::to_value(d).map_err(|e| e.to_string())
}

/// `None` (or an empty id) follows the system default device.
#[tauri::command]
pub fn player_set_device(id: Option<String>) -> Result<(), String> {
    let id = id.filter(|s| !s.is_empty());
    crate::aelog!("[UI] device: {}", id.as_deref().unwrap_or("system default"));
    controller::get().set_device(id);
    Ok(())
}

/// Return the current backend queue so a reloaded page can re-link its
/// playlist rows to the existing track ids without re-adding files (D2).
#[tauri::command]
pub fn player_queue() -> Result<Value, String> {
    let tracks = controller::get().queue();
    serde_json::to_value(tracks).map_err(|e| e.to_string())
}

/// Set the repeat mode ("off", "all", "one"). Persisted by the page.
#[tauri::command]
pub fn player_set_repeat(mode: RepeatMode) -> Result<(), String> {
    controller::get().set_repeat(mode);
    Ok(())
}

/// How much sound the big player's visualization wants rendered ahead
/// (seconds; 0: none). Sent when the picture shown changes, not per frame.
#[tauri::command]
pub fn player_set_vis_ahead(source: String, seconds: f64) -> Result<(), String> {
    controller::get().set_vis_ahead(&source, seconds);
    Ok(())
}

/// Register a per-track settings override. The next time this track id is
/// started (direct play, prefetch, or gapless hand-over) the supplied
/// settings are used instead of the global rack. Idempotent: calling again
/// replaces the previous override.
#[tauri::command]
pub fn player_set_track_settings(id: u64, settings: PlayerSettings) -> Result<(), String> {
    controller::get().set_track_settings(id, settings);
    Ok(())
}

/// Remove a per-track settings override. Subsequent operations on this
/// track id revert to whatever the global rack holds at the time.
#[tauri::command]
pub fn player_clear_track_settings(id: u64) -> Result<(), String> {
    controller::get().clear_track_settings(id);
    Ok(())
}

/// What the page reports about itself — a rejected command, an exception, a
/// module that would not load — goes into the same log as the engine's, so a
/// broken window leaves a trace in the console.
/// The pointer in the window's client area, in CSS pixels. Files dragged in
/// from Explorer give the page no pointer events while they hover (the
/// webview's drop target has them), so the list asks here where to open a
/// gap for them.
#[tauri::command]
pub fn ui_cursor_client(window: tauri::Window) -> Option<(f64, f64)> {
    #[cfg(windows)]
    {
        use winapi::shared::windef::{HWND, POINT};
        use winapi::um::winuser::{GetCursorPos, ScreenToClient};
        let hwnd = window.hwnd().ok()?.0 as HWND;
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: plain Win32 calls on a live window handle and a local POINT.
        unsafe {
            if GetCursorPos(&mut p) == 0 || ScreenToClient(hwnd, &mut p) == 0 {
                return None;
            }
        }
        let scale = window.scale_factor().ok()?.max(0.1);
        Some((p.x as f64 / scale, p.y as f64 / scale))
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

#[tauri::command]
pub fn player_ui_log(level: String, message: String) {
    crate::aelog!("[UI] {}: {}", level, message);
}

/// The developer's blind test (js/blindtest.js): `AURA_BLIND_TEST=<trials>`
/// (1–40) in the environment opens the app behind its curtain. None: the
/// app as usual.
#[tauri::command]
pub fn blind_test_trials() -> Option<u32> {
    std::env::var("AURA_BLIND_TEST").ok()?.trim().parse().ok().filter(|n| (1..=40).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::settings::Phase;

    /// What a stream waits for is reckoned for the rack's length, the
    /// stream's own linear filter in it (50 ms); with none installed, no
    /// length plays.
    #[test]
    fn a_streams_wait_is_reckoned_for_the_racks_length() {
        let dir = std::env::current_exe().unwrap().parent().unwrap().join("fir-optimizer").join("output");
        std::fs::create_dir_all(&dir).unwrap();
        // 44.1 kHz ×16 at 5M: a cell no other test reckons lengths on; empty
        // files stand for the pair (the filter made ahead of the stream fails
        // on them, in the background).
        let pair = ["fir_5M_705600_linear_phase.npy", "fir_5M_705600_minimum_phase.npy"].map(|n| dir.join(n));
        for p in &pair {
            std::fs::write(p, b"").unwrap();
        }
        let rack = PlayerSettings { taps: 5_000_000, phase: Phase::Linear, fs_multiplier: 16, ..PlayerSettings::default() };
        let got: Value = serde_json::from_str(&radio_stream_wait(rack.clone(), Some(44_100)).unwrap()).unwrap();
        for p in &pair {
            std::fs::remove_file(p).ok();
        }
        assert_eq!((got["rate"].as_u64(), got["plays"].as_u64(), got["label"].as_str()), (Some(44_100), Some(5_000_000), Some("5M")));
        assert!((got["lookAheadS"].as_f64().unwrap() - 0.050).abs() < 1e-12, "{got}");
        assert!(got["needS"].as_f64().unwrap() >= 0.050 && got["preparing"].is_boolean(), "{got}");
        let none: Value = serde_json::from_str(&radio_stream_wait(rack, Some(44_100)).unwrap()).unwrap();
        assert!(none["plays"].is_null() && none["needS"].is_null(), "{none}");
    }
}
