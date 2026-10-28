//! Where a window was: kept while it moves and at close, and put back at the
//! next start (Anton 27.09) — inside a screen's work area, so a window left on
//! a monitor that is gone, or past the edge of a smaller resolution, comes
//! back whole.
//!
//! The main window keeps only its place (its size follows the page's layout).
//! The visualization studio keeps its size too, and whether it was maximized
//! or on the whole screen (Anton 27.09: it opened at the default size every
//! time). So does the analyzer (Anton 1.10, the same complaint): its windows —
//! the live one and one per file — share one place, size and state.
//!
//! A window starts hidden and is shown only after it has been placed, so it
//! never appears in the default spot first.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Writes while dragging, at most this often; the last move is always written
/// (a trailing write), and the close writes at once.
const EVERY: Duration = Duration::from_millis(400);

static LAST_WRITE: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
static TRAILING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// `%LOCALAPPDATA%\AuraEngine\window.json` for the main window,
/// `window-<label>.json` for the others. A run with a WebView2 profile of its
/// own (our test tool's windows) keeps its state in that profile, so it never
/// moves the user's windows.
fn file(label: &str) -> Option<PathBuf> {
    // Every analyzer window ("analyzer-live", "analyzer-file-<id>-<n>") keeps
    // the one place: a file's window is new each time it opens.
    let key = if label.starts_with("analyzer") { "analyzer" } else { label };
    let name = if key == "main" { "window.json".to_string() } else { format!("window-{key}.json") };
    if let Some(d) = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER") {
        return Some(PathBuf::from(d).join(name));
    }
    Some(crate::app_dir::root()?.join(name))
}

fn saved(label: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(file(label)?).ok()?;
    serde_json::from_str(&text).ok()
}

/// Put the main window where it was, fitted into the nearest screen's work area.
pub fn restore(w: &tauri::Window) {
    let Some(v) = saved(w.label()) else { return };
    let (Some(x), Some(y)) = (v["x"].as_i64(), v["y"].as_i64()) else { return };
    let cur = w.outer_size().ok();
    let width = v["w"].as_u64().map(|n| n as i32).or(cur.map(|s| s.width as i32)).unwrap_or(440);
    let height = v["h"].as_u64().map(|n| n as i32).or(cur.map(|s| s.height as i32)).unwrap_or(900);
    let (fx, fy, _, _) = fit(x as i32, y as i32, width, height);
    if (fx, fy) != (x as i32, y as i32) {
        crate::aelog!("[WINDOW] saved at {},{} — moved to {},{} to stay on the screen", x, y, fx, fy);
    }
    let _ = w.set_position(tauri::PhysicalPosition::new(fx, fy));
}

/// Put a window back with its size and state: its normal place and size
/// (fitted into the nearest screen), then maximized or on the whole screen if
/// it was left so. Call it while the window is still hidden.
pub fn restore_all(w: &tauri::Window) {
    let Some(v) = saved(w.label()) else { return };
    let (Some(x), Some(y), Some(width), Some(height)) =
        (v["x"].as_i64(), v["y"].as_i64(), v["w"].as_i64(), v["h"].as_i64())
    else {
        return;
    };
    let (fx, fy, fw, fh) = fit(x as i32, y as i32, width as i32, height as i32);
    let _ = w.set_size(tauri::PhysicalSize::new(fw.max(1) as u32, fh.max(1) as u32));
    let _ = w.set_position(tauri::PhysicalPosition::new(fx, fy));
    if v["full"].as_bool() == Some(true) {
        let _ = w.set_fullscreen(true);
    } else if v["max"].as_bool() == Some(true) {
        let _ = w.maximize();
    }
}

/// The window moved or changed size: remember it (throttled while dragging,
/// the last one always kept).
pub fn moved(w: &tauri::Window) {
    record(w, false);
}

/// The window closes: remember it now.
pub fn closing(w: &tauri::Window) {
    record(w, true);
}

/// For a page that closes its own window (Tauri 1's `close()` gives no
/// CloseRequested): keep the place first.
#[tauri::command]
pub fn window_keep_place(window: tauri::Window) {
    closing(&window);
}

fn record(w: &tauri::Window, force: bool) {
    let label = w.label().to_string();
    let (Ok(pos), Ok(size)) = (w.outer_position(), w.outer_size()) else { return };
    // Windows parks a minimized window far off the screen: not a place.
    if w.is_minimized().unwrap_or(false) || pos.x <= -30000 || pos.y <= -30000 {
        return;
    }
    if !force {
        let mut last = LAST_WRITE.lock().unwrap();
        let last = last.get_or_insert_with(HashMap::new);
        if last.get(&label).map_or(false, |t| t.elapsed() < EVERY) {
            // Too soon: write once more when the moving stops.
            let mut tr = TRAILING.lock().unwrap();
            if tr.get_or_insert_with(HashSet::new).insert(label.clone()) {
                let w = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(EVERY);
                    TRAILING.lock().unwrap().get_or_insert_with(HashSet::new).remove(w.label());
                    record(&w, true);
                });
            }
            return;
        }
    }
    LAST_WRITE.lock().unwrap().get_or_insert_with(HashMap::new).insert(label.clone(), Instant::now());
    let max = w.is_maximized().unwrap_or(false);
    let full = w.is_fullscreen().unwrap_or(false);
    // Maximized or on the whole screen, the window's rectangle is the
    // screen's: the place to keep is the one before it, and the window goes
    // back there.
    let (x, y, width, height) = if max || full {
        match saved(&label) {
            Some(v) => match (v["x"].as_i64(), v["y"].as_i64(), v["w"].as_i64(), v["h"].as_i64()) {
                (Some(x), Some(y), Some(ww), Some(hh)) => (x, y, ww, hh),
                _ if full => return,
                _ => (pos.x as i64, pos.y as i64, size.width as i64, size.height as i64),
            },
            None if full => return,
            None => (pos.x as i64, pos.y as i64, size.width as i64, size.height as i64),
        }
    } else {
        (pos.x as i64, pos.y as i64, size.width as i64, size.height as i64)
    };
    let Some(path) = file(&label) else { return };
    let json = format!("{{\"x\":{x},\"y\":{y},\"w\":{width},\"h\":{height},\"max\":{max},\"full\":{full}}}");
    let _ = std::fs::write(path, json);
}

/// A `w`×`h` window's top-left corner and size that keep it inside the work
/// area of the screen nearest to where it was (physical pixels; a window
/// larger than that area is made to fit).
#[cfg(windows)]
fn fit(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    use winapi::shared::windef::RECT;
    use winapi::um::winuser::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    unsafe {
        let r = RECT { left: x, top: y, right: x + w, bottom: y + h };
        let m = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if m.is_null() || GetMonitorInfoW(m, &mut mi) == 0 {
            return (x, y, w, h);
        }
        let wa = mi.rcWork;
        let (w, h) = (w.min(wa.right - wa.left), h.min(wa.bottom - wa.top));
        (x.min(wa.right - w).max(wa.left), y.min(wa.bottom - h).max(wa.top), w, h)
    }
}

#[cfg(not(windows))]
fn fit(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    (x, y, w, h)
}
