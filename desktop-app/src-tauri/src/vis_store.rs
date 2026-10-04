//! The visualization studio's window and the user's own scenes (Anton 27.09).
//!
//! A scene is one text file (`*.aura-vis`): GLSL written against the app's
//! contract (src/vis/AURA-VIS-SPEC.md), its settings in `// @param` lines.
//! All scenes live in one list, the user's folder
//! `%LOCALAPPDATA%\AuraEngine\visualizations` — or, for a run with a
//! WebView2 profile of its own (our test tool's windows), in that profile,
//! so a test never touches the real ones. The app's own scenes are put there
//! once (Anton 27.09: one list; remove or add as you like); one removed does
//! not come back by itself — `vis_restore_app` brings them back. The pages
//! only name a file; every path is made here, inside that folder.

use std::path::PathBuf;

const EXT: &str = "aura-vis";
/// The app's own scenes, built in.
const APP_SCENES: &[(&str, &str)] = &[
    ("tunnel.aura-vis", include_str!("../../src/vis/tunnel.aura-vis")),
    ("nebula.aura-vis", include_str!("../../src/vis/nebula.aura-vis")),
    ("bars.aura-vis", include_str!("../../src/vis/bars.aura-vis")),
    ("stage.aura-vis", include_str!("../../src/vis/stage.aura-vis")),
    ("lineup.aura-vis", include_str!("../../src/vis/lineup.aura-vis")),
    ("aurora.aura-vis", include_str!("../../src/vis/aurora.aura-vis")),
    ("raindrops.aura-vis", include_str!("../../src/vis/raindrops.aura-vis")),
    ("horizon.aura-vis", include_str!("../../src/vis/horizon.aura-vis")),
    ("diamond-dust.aura-vis", include_str!("../../src/vis/diamond-dust.aura-vis")),
    ("diamond-dust-instruments.aura-vis", include_str!("../../src/vis/diamond-dust-instruments.aura-vis")),
    ("tourbillon.aura-vis", include_str!("../../src/vis/tourbillon.aura-vis")),
    ("speaker.aura-vis", include_str!("../../src/vis/speaker.aura-vis")),
    ("speaker-field.aura-vis", include_str!("../../src/vis/speaker-field.aura-vis")),
    ("better-rain.aura-vis", include_str!("../../src/vis/better-rain.aura-vis")),
    ("scope.aura-vis", include_str!("../../src/vis/scope.aura-vis")),
    ("neon-portal.aura-vis", include_str!("../../src/vis/neon-portal.aura-vis")),
    ("neonwave-sunset.aura-vis", include_str!("../../src/vis/neonwave-sunset.aura-vis")),
    ("phosphorescent-peaks.aura-vis", include_str!("../../src/vis/phosphorescent-peaks.aura-vis")),
    ("night-ridges.aura-vis", include_str!("../../src/vis/night-ridges.aura-vis")),
];
/// Which of the app's scenes were put in the folder already (one name a line).
const SEEDED: &str = ".app-scenes";
/// An app scene rewritten since it was put in folders: the FNV-1a 64 hash
/// of its old text. A copy that is still exactly that (the user has not
/// changed it) becomes the new one; a changed copy is left alone. Stage:
/// the spatial API's second round (27.09, parts that split off), then its
/// round-2 text (28.09: parts light up in place, no flying out), then that
/// one (28.09: instruments' wide layers as bands of haze), then that one
/// (28.09: the lineup test's toms and cymbals are drums). Diamond Dust: its
/// first text (29.09: rings coming in to the focus, a jolt on the kicks).
/// Speaker Field: its first text (29.09: three kinds of drivers, a wave and
/// rings of light rolling over the ground). Better Rain: its first text
/// (29.09: it turned the cover upright itself; the renderer does it now).
/// Neon Portal: its first text (29.09: the horizon in the player at the
/// height of the technical lines).
const UPGRADES: &[(&str, u64)] = &[
    ("stage.aura-vis", 0x32de_900b_1619_2772),
    ("stage.aura-vis", 0xbf53_5a77_e5d1_fe58),
    ("stage.aura-vis", 0xa9d0_93c9_a870_41b9),
    ("stage.aura-vis", 0x3ebe_2bdc_7b79_c6bc),
    ("diamond-dust.aura-vis", 0xb9dd_9d33_1cf3_d00b),
    ("speaker-field.aura-vis", 0x8cec_326b_bd9e_8c77),
    ("better-rain.aura-vis", 0xbbaa_91ae_2608_8e96),
    ("neon-portal.aura-vis", 0x4d7b_208e_f3e6_0459),
];

fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ *x as u64).wrapping_mul(0x0100_0000_01b3))
}

/// The app's scenes the user has not changed, brought to their new text.
fn upgrade(d: &std::path::Path) {
    for (file, old) in UPGRADES {
        let p = d.join(file);
        let Ok(have) = std::fs::read(&p) else { continue };
        if fnv1a(&have) != *old {
            continue;
        }
        if let Some((_, text)) = APP_SCENES.iter().find(|(f, _)| f == file) {
            if std::fs::write(&p, text).is_ok() {
                crate::aelog!("[VIS] the app's scene {} brought to its new version", file);
            }
        }
    }
}

/// Put the app's scenes the folder has never had into it.
fn seed(d: &std::path::Path) {
    upgrade(d);
    let marker = d.join(SEEDED);
    let done = std::fs::read_to_string(&marker).unwrap_or_default();
    let had: Vec<&str> = done.lines().map(str::trim).collect();
    let mut add = Vec::new();
    for (file, text) in APP_SCENES {
        if had.contains(file) {
            continue;
        }
        if std::fs::create_dir_all(d).is_err() {
            return;
        }
        if !d.join(file).exists() && std::fs::write(d.join(file), text).is_err() {
            continue;
        }
        add.push(*file);
    }
    if !add.is_empty() {
        let mut all = done.trim_end().to_string();
        for f in add {
            if !all.is_empty() {
                all.push('\n');
            }
            all.push_str(f);
        }
        all.push('\n');
        let _ = std::fs::write(&marker, all);
    }
}

/// Bring back the app's scenes that are not in the folder (removed ones
/// too). Returns how many came back.
#[tauri::command]
pub fn vis_restore_app() -> Result<usize, String> {
    let d = dir().ok_or("no place for scenes")?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let mut n = 0;
    for (file, text) in APP_SCENES {
        if !d.join(file).exists() {
            std::fs::write(d.join(file), text).map_err(|e| format!("{file}: {e}"))?;
            n += 1;
        }
    }
    seed(&d);
    Ok(n)
}
/// Larger than any scene anyone writes by hand; keeps a wrong file out.
const MAX_BYTES: u64 = 512 * 1024;

fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER") {
        return Some(PathBuf::from(d).join("visualizations"));
    }
    Some(crate::app_dir::root()?.join("visualizations"))
}

/// A file name the pages may use: a plain name ending in `.aura-vis`, no
/// folders, nothing Windows refuses.
fn checked(file: &str) -> Result<PathBuf, String> {
    let ok = file.len() <= 120
        && file.to_ascii_lowercase().ends_with(&format!(".{EXT}"))
        && !file.starts_with('.')
        && file.chars().all(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control());
    if !ok {
        return Err(format!("not a scene file name: {file}"));
    }
    Ok(dir().ok_or("no place for scenes")?.join(file))
}

/// A file name from a scene's name: letters, digits and dashes.
fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            s.extend(c.to_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').chars().take(60).collect::<String>();
    if s.is_empty() { "scene".into() } else { s }
}

/// Open (or bring up) the studio window. Which scene it shows first the page
/// reads from localStorage, where the window that opened it put it.
///
/// `async` for the same reason as the other windows (wry#583).
#[tauri::command]
pub async fn vis_studio_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WindowUrl};
    if let Some(w) = app.get_window("vis-studio") {
        let _ = w.unminimize();
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let w = tauri::WindowBuilder::new(&app, "vis-studio", WindowUrl::App("vis-studio.html".into()))
        .title("Aura Engine - visualization studio")
        .inner_size(1240.0, 780.0)
        .min_inner_size(900.0, 560.0)
        .resizable(true)
        // Frameless like the main window: the page draws its own head bar.
        .decorations(false)
        .transparent(true)
        // Placed where it was last time (size, maximized, whole screen), then shown.
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    crate::window_state::restore_all(&w);
    let _ = w.show();
    let _ = w.set_focus();
    Ok(())
}

/// The user's scenes: `[{ file, text }]`, by file name.
#[tauri::command]
pub fn vis_list() -> Vec<serde_json::Value> {
    let Some(d) = dir() else { return Vec::new() };
    seed(&d);
    let Ok(rd) = std::fs::read_dir(&d) else { return Vec::new() };
    let mut out: Vec<(String, String)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?.to_string();
            if !name.to_ascii_lowercase().ends_with(&format!(".{EXT}")) {
                return None;
            }
            if e.metadata().ok()?.len() > MAX_BYTES {
                return None;
            }
            Some((name, std::fs::read_to_string(&p).ok()?))
        })
        .collect();
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out.into_iter().map(|(file, text)| serde_json::json!({ "file": file, "text": text })).collect()
}

/// Save a scene. `file` given: that file is written over; none: a new file
/// named after `name` (a number added when the name is taken). Returns the
/// file name.
#[tauri::command]
pub fn vis_save(file: Option<String>, name: String, text: String) -> Result<String, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err("the scene is too long".into());
    }
    let d = dir().ok_or("no place for scenes")?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let file = match file {
        Some(f) => f,
        None => {
            let base = slug(&name);
            let mut f = format!("{base}.{EXT}");
            let mut n = 2;
            while d.join(&f).exists() {
                f = format!("{base}-{n}.{EXT}");
                n += 1;
            }
            f
        }
    };
    let path = checked(&file)?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::aelog!("[VIS] scene saved: {}", path.display());
    Ok(file)
}

/// Remove one of the user's scenes (the page asks first).
#[tauri::command]
pub fn vis_delete(file: String) -> Result<(), String> {
    let path = checked(&file)?;
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::aelog!("[VIS] scene removed: {}", path.display());
    Ok(())
}

/// Show the scenes' folder in Explorer (made if it is not there yet).
#[tauri::command]
pub fn vis_dir_open() -> Result<String, String> {
    let d = dir().ok_or("no place for scenes")?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    std::process::Command::new("explorer.exe")
        .arg(&d)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(d.display().to_string())
}

/// Write a scene where the user chose in the save dialog (to share it).
#[tauri::command]
pub fn vis_export(path: String, text: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let ext_ok = p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case(EXT)).unwrap_or(false);
    if !ext_ok {
        return Err(format!("a scene file ends in .{EXT}"));
    }
    std::fs::write(&p, text).map_err(|e| format!("{path}: {e}"))
}

/// Read a scene file the user chose in the open dialog.
#[tauri::command]
pub fn vis_import(path: String) -> Result<String, String> {
    let p = PathBuf::from(&path);
    let len = std::fs::metadata(&p).map_err(|e| format!("{path}: {e}"))?.len();
    if len > MAX_BYTES {
        return Err("too large for a scene".into());
    }
    std::fs::read_to_string(&p).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kept_inside_the_folder() {
        assert!(checked("rings.aura-vis").is_ok());
        assert!(checked("..\\x.aura-vis").is_err());
        assert!(checked("a/b.aura-vis").is_err());
        assert!(checked("c:x.aura-vis").is_err());
        assert!(checked("x.txt").is_err());
        assert!(checked(".aura-vis").is_err());
        assert_eq!(slug("  Neon Tunnel!  2 "), "neon-tunnel-2");
        assert_eq!(slug("***"), "scene");
        assert_eq!(slug("Волны"), "волны");
    }

    #[test]
    fn an_unchanged_old_app_scene_is_brought_up_to_date_a_changed_one_is_not() {
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        let d = std::env::temp_dir().join(format!("aura-vis-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // a copy the user changed: left alone
        std::fs::write(d.join("stage.aura-vis"), "// @name Mine\n").unwrap();
        upgrade(&d);
        assert_eq!(std::fs::read_to_string(d.join("stage.aura-vis")).unwrap(), "// @name Mine\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_apps_scenes_come_once() {
        let d = std::env::temp_dir().join(format!("aura-vis-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        seed(&d);
        assert!(d.join("tunnel.aura-vis").exists() && d.join("nebula.aura-vis").exists());
        std::fs::remove_file(d.join("tunnel.aura-vis")).unwrap();
        seed(&d);
        assert!(!d.join("tunnel.aura-vis").exists(), "a removed scene stays removed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
