//! Where the app keeps its own files: logs, the player's calibration and
//! learned timings, its caches, the windows' places, scenes and packs.
//!
//! The folder `AURA_DATA_DIR` names, if set: a build that must not share them
//! with the installed app (a test build kept apart from the release on the
//! same machine) keeps them there. Its WebView2 profile is apart already, by
//! its bundle identifier. Otherwise the system's place for an app's own data:
//!
//! * Windows — `%LOCALAPPDATA%\AuraEngine`;
//! * macOS — `~/Library/Application Support/AuraEngine`;
//! * Linux and the rest — `$XDG_DATA_HOME/AuraEngine`, or
//!   `~/.local/share/AuraEngine` when that is unset or not absolute.

use std::ffi::OsString;
use std::path::PathBuf;

/// The folder the app's own files go in (made by whoever writes there).
pub fn root() -> Option<PathBuf> {
    root_from(std::env::var_os("AURA_DATA_DIR"), system_base())
}

/// The system's folder for an app's own data, `AuraEngine` included.
#[cfg(windows)]
fn system_base() -> Option<PathBuf> {
    windows_base(std::env::var_os("LOCALAPPDATA"))
}

#[cfg(target_os = "macos")]
fn system_base() -> Option<PathBuf> {
    macos_base(std::env::var_os("HOME"))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn system_base() -> Option<PathBuf> {
    xdg_base(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn root_from(data_dir: Option<OsString>, base: Option<PathBuf>) -> Option<PathBuf> {
    match data_dir.filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => base,
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_base(local: Option<OsString>) -> Option<PathBuf> {
    local.map(|b| PathBuf::from(b).join("AuraEngine"))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_base(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("Library").join("Application Support").join("AuraEngine"))
}

/// XDG Base Directory: a relative `XDG_DATA_HOME` is invalid and ignored.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn xdg_base(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let xdg = xdg_data_home.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = xdg.or_else(|| {
        home.filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".local").join("share"))
    })?;
    Some(base.join("AuraEngine"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aura_data_dir_takes_the_app_s_files_elsewhere() {
        let local = windows_base(Some(OsString::from(r"C:\Users\x\AppData\Local")));
        assert_eq!(root_from(None, local.clone()), Some(PathBuf::from(r"C:\Users\x\AppData\Local").join("AuraEngine")));
        assert_eq!(root_from(Some(OsString::from(r"D:\Radio\data")), local.clone()), Some(PathBuf::from(r"D:\Radio\data")));
        assert_eq!(root_from(Some(OsString::new()), local), Some(PathBuf::from(r"C:\Users\x\AppData\Local").join("AuraEngine")));
        assert_eq!(root_from(None, windows_base(None)), None);
    }

    #[test]
    fn macos_keeps_them_in_application_support() {
        assert_eq!(
            macos_base(Some(OsString::from("/Users/x"))),
            Some(PathBuf::from("/Users/x").join("Library").join("Application Support").join("AuraEngine"))
        );
        assert_eq!(macos_base(None), None);
        assert_eq!(macos_base(Some(OsString::new())), None);
    }

    #[test]
    fn linux_follows_xdg_data_home_then_local_share() {
        // An absolute path on the system the test runs on.
        let abs = std::env::temp_dir().join("xdg");
        assert_eq!(xdg_base(Some(abs.clone().into()), Some("/home/x".into())), Some(abs.join("AuraEngine")));
        let home_share = Some(PathBuf::from("/home/x").join(".local").join("share").join("AuraEngine"));
        assert_eq!(xdg_base(None, Some("/home/x".into())), home_share);
        assert_eq!(xdg_base(Some("relative/dir".into()), Some("/home/x".into())), home_share);
        assert_eq!(xdg_base(None, None), None);
    }
}
