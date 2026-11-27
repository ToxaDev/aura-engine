//! A fault, written down for the next start.
//!
//! A panic or a hardware fault happens where no window can be counted on, so
//! the box only asks; the report is made at the next start, from
//! `pending-report.json` in the app's data folder. Written from inside the
//! panic hook and the fault handler: it reads atomics and writes a file, and
//! it logs nothing.
//!
//! `asked` is the user's Yes in that box. Without it the file only lends its
//! detail to a report the user chooses to send after an unclean exit, and a
//! clean exit removes it. It never leaves this machine by itself.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Most of a backtrace worth keeping.
const BACKTRACE_MAX: usize = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// `panic` or `hw_fault`.
    pub kind: String,
    pub thread: String,
    pub message: String,
    pub backtrace: String,
    pub markers: String,
    pub stage: String,
    pub progress: u64,
    pub asked: bool,
}

fn path() -> Option<PathBuf> {
    crate::app_dir::root().map(|d| d.join("pending-report.json"))
}

fn write_at(path: &Path, p: &Pending) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(p) {
        let _ = crate::watchdog::write_synced(path, text.as_bytes());
    }
}

fn read_at(path: &Path) -> Option<Pending> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// What a fault leaves behind (`asked` still false).
pub fn record(kind: &str, message: &str) {
    let Some(path) = path() else { return };
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    let (stage, progress) = crate::watchdog::busiest()
        .map_or((String::new(), 0), |(m, s, p)| (format!("{m}: {s}"), p));
    write_at(
        &path,
        &Pending {
            kind: kind.to_string(),
            thread: std::thread::current().name().unwrap_or("unnamed").to_string(),
            message: first_chars(message, 4000).to_string(),
            backtrace: first_chars(&backtrace, BACKTRACE_MAX).to_string(),
            markers: crate::watchdog::markers_line(),
            stage,
            progress,
            asked: false,
        },
    );
}

/// The user said Yes in the box.
pub fn mark_asked() {
    let Some(path) = path() else { return };
    if let Some(mut p) = read_at(&path) {
        p.asked = true;
        write_at(&path, &p);
    }
}

pub fn read() -> Option<Pending> {
    read_at(&path()?)
}

/// Gone: the report was made, or the user declined it.
pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

/// A clean exit: what nobody asked to send goes.
pub fn discard_unasked() {
    if read().is_some_and(|p| !p.asked) {
        clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_survives_the_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("aura-pending-{}", std::process::id()));
        let at = dir.join("pending-report.json");
        let p = Pending { kind: "panic".into(), message: "it broke".into(), asked: true, ..Default::default() };
        write_at(&at, &p);
        assert_eq!(read_at(&at), Some(p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_text_is_cut_on_a_character() {
        assert_eq!(first_chars("абв", 2), "аб");
        assert_eq!(first_chars("аб", 5), "аб");
    }
}
