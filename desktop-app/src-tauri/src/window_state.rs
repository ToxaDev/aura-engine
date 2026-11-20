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
/// (fitted into the screen it is most on), then maximized or on the whole
/// screen if it was left so. A place on no screen any more (a monitor gone)
/// opens the window in the middle of its screen at the saved size; nothing
/// saved yet, it opens as built, made to fit the screen if larger
/// (Anton 1.11). Call it while the window is still hidden.
pub fn restore_all(w: &tauri::Window) {
    let areas: Vec<Rect> = w
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let a = m.work_area();
            Rect { x: a.position.x, y: a.position.y, w: a.size.width as i32, h: a.size.height as i32 }
        })
        .collect();
    let here = w.current_monitor().ok().flatten().map(|m| {
        let a = m.work_area();
        Rect { x: a.position.x, y: a.position.y, w: a.size.width as i32, h: a.size.height as i32 }
    });
    let saved_rect = saved(w.label()).and_then(|v| {
        let r = Rect { x: v["x"].as_i64()? as i32, y: v["y"].as_i64()? as i32, w: v["w"].as_i64()? as i32, h: v["h"].as_i64()? as i32 };
        Some((r, v))
    });
    let Some((r, v)) = saved_rect else {
        // First open: the built size, unless the screen is smaller.
        if let (Some(a), Ok(s)) = (here, w.outer_size()) {
            let c = centred(Rect { x: a.x, y: a.y, w: s.width as i32, h: s.height as i32 }, a);
            if (c.w, c.h) != (s.width as i32, s.height as i32) {
                let _ = w.set_size(tauri::PhysicalSize::new(c.w.max(1) as u32, c.h.max(1) as u32));
                let _ = w.set_position(tauri::PhysicalPosition::new(c.x, c.y));
            }
        }
        return;
    };
    let p = match place(r, &areas) {
        Some(p) => p,
        None => match here.or(areas.first().copied()) {
            Some(a) => {
                crate::aelog!("[WINDOW] {} saved at {},{} is on no screen — opens in the middle", w.label(), r.x, r.y);
                centred(r, a)
            }
            None => r,
        },
    };
    let _ = w.set_size(tauri::PhysicalSize::new(p.w.max(1) as u32, p.h.max(1) as u32));
    let _ = w.set_position(tauri::PhysicalPosition::new(p.x, p.y));
    if v["full"].as_bool() == Some(true) {
        let _ = w.set_fullscreen(true);
    } else if v["max"].as_bool() == Some(true) {
        let _ = w.maximize();
    }
}

/// A rectangle in physical pixels: a window's outer place and size, or a
/// screen's work area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// How much of a window must be on a screen for its place to count: enough
/// of its head to take it by the mouse.
const MIN_SEEN: (i32, i32) = (64, 32);

fn overlap(a: Rect, b: Rect) -> (i32, i32) {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    (w.max(0), h.max(0))
}

/// Where a window saved at `r` goes back among the screens' work areas
/// `areas`: on the one it is most on, made to fit it, moved inside it. None
/// when no screen shows enough of it (`MIN_SEEN`) — the monitor it was on
/// is gone, or the place is past every edge.
pub(crate) fn place(r: Rect, areas: &[Rect]) -> Option<Rect> {
    let best = areas
        .iter()
        .map(|&a| (a, overlap(r, a)))
        .filter(|&(_, (ow, oh))| ow >= MIN_SEEN.0.min(r.w) && oh >= MIN_SEEN.1.min(r.h))
        .max_by_key(|&(_, (ow, oh))| ow as i64 * oh as i64)?
        .0;
    let (w, h) = (r.w.min(best.w), r.h.min(best.h));
    Some(Rect { x: r.x.min(best.x + best.w - w).max(best.x), y: r.y.min(best.y + best.h - h).max(best.y), w, h })
}

/// `r`'s size made to fit `a`, in the middle of it.
pub(crate) fn centred(r: Rect, a: Rect) -> Rect {
    let (w, h) = (r.w.min(a.w), r.h.min(a.h));
    Rect { x: a.x + (a.w - w) / 2, y: a.y + (a.h - h) / 2, w, h }
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: Rect = Rect { x: 0, y: 0, w: 1920, h: 1040 };
    const B: Rect = Rect { x: 1920, y: 0, w: 2560, h: 1400 };

    #[test]
    fn a_place_on_a_screen_comes_back_as_it_was() {
        let r = Rect { x: 100, y: 80, w: 820, h: 700 };
        assert_eq!(place(r, &[A, B]), Some(r));
        let r = Rect { x: 2200, y: 300, w: 730, h: 820 };
        assert_eq!(place(r, &[A, B]), Some(r));
    }

    #[test]
    fn a_place_past_an_edge_is_pulled_in_on_the_screen_it_is_most_on() {
        // Mostly on B, hanging off its bottom right.
        let r = Rect { x: 4200, y: 1100, w: 820, h: 700 };
        assert_eq!(place(r, &[A, B]), Some(Rect { x: 4480 - 820, y: 1400 - 700, w: 820, h: 700 }));
        // Larger than its screen: made to fit.
        let r = Rect { x: -50, y: -10, w: 2400, h: 1300 };
        assert_eq!(place(r, &[A]), Some(Rect { x: 0, y: 0, w: 1920, h: 1040 }));
    }

    #[test]
    fn a_place_on_no_screen_is_none_and_opens_in_the_middle() {
        // B unplugged: the window was all on it.
        let r = Rect { x: 2200, y: 300, w: 730, h: 820 };
        assert_eq!(place(r, &[A]), None);
        // Only a sliver on A: not enough to take it by the head.
        assert_eq!(place(Rect { x: 1900, y: 300, w: 730, h: 820 }, &[A]), None);
        assert_eq!(place(r, &[]), None);
        assert_eq!(centred(r, A), Rect { x: (1920 - 730) / 2, y: (1040 - 820) / 2, w: 730, h: 820 });
        assert_eq!(centred(Rect { x: 0, y: 0, w: 900, h: 1200 }, Rect { x: 0, y: 0, w: 1280, h: 680 }),
            Rect { x: 190, y: 0, w: 900, h: 680 });
    }
}
