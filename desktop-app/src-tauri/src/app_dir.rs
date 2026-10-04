//! Where the app keeps its own files: logs, the player's calibration and
//! learned timings, its caches, the windows' places, scenes and packs.
//!
//! `%LOCALAPPDATA%\AuraEngine`, or the folder `AURA_DATA_DIR` names: a build
//! that must not share them with the installed app (a test build kept apart
//! from the release on the same machine) keeps them there. Its WebView2
//! profile is apart already, by its bundle identifier.

use std::ffi::OsString;
use std::path::PathBuf;

/// The folder the app's own files go in (made by whoever writes there).
pub fn root() -> Option<PathBuf> {
    root_from(std::env::var_os("AURA_DATA_DIR"), std::env::var_os("LOCALAPPDATA"))
}

fn root_from(data_dir: Option<OsString>, local: Option<OsString>) -> Option<PathBuf> {
    match data_dir.filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => local.map(|b| PathBuf::from(b).join("AuraEngine")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aura_data_dir_takes_the_app_s_files_elsewhere() {
        let local = Some(OsString::from(r"C:\Users\x\AppData\Local"));
        assert_eq!(root_from(None, local.clone()), Some(PathBuf::from(r"C:\Users\x\AppData\Local\AuraEngine")));
        assert_eq!(root_from(Some(OsString::from(r"D:\Radio\data")), local.clone()), Some(PathBuf::from(r"D:\Radio\data")));
        assert_eq!(root_from(Some(OsString::new()), local), Some(PathBuf::from(r"C:\Users\x\AppData\Local\AuraEngine")));
        assert_eq!(root_from(None, None), None);
    }
}
