//! Reports the user sent that could not go: no network, the receiver asleep.
//! They wait here, sealed, and go by themselves later.
//!
//! Only a send puts a file here, so each has a press of Send or the "always
//! send" box behind it, and the name says which. One the box sent waits while
//! the box is unticked. After 14 days it is given up on, either way.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const GIVE_UP_AFTER: Duration = Duration::from_secs(14 * 24 * 3600);
const EXTENSION: &str = "aerep";
const BY_THE_BOX: &str = "-auto";

pub fn dir() -> Option<PathBuf> {
    crate::app_dir::root().map(|d| d.join("outbox"))
}

/// The file is here whole or not at all: a power cut mid-write must not leave
/// half a report to be sent.
pub fn keep_at(dir: &Path, code: &str, by_the_box: bool, file: &[u8]) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let name = format!("AuraEngine-report-{code}{}", if by_the_box { BY_THE_BOX } else { "" });
    let part = dir.join(format!("{name}.part"));
    let path = dir.join(format!("{name}.{EXTENSION}"));
    crate::watchdog::write_synced(&part, file).ok()?;
    std::fs::rename(&part, &path).ok()?;
    Some(path)
}

fn is_by_the_box(path: &Path) -> bool {
    path.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(BY_THE_BOX))
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub send: Vec<PathBuf>,
    pub give_up: Vec<PathBuf>,
}

/// What to do with these files (with the time each was written), oldest first.
fn plan(mut files: Vec<(PathBuf, SystemTime)>, now: SystemTime, box_ticked: bool) -> Plan {
    files.sort_by_key(|(_, written)| *written);
    let mut out = Plan::default();
    for (path, written) in files {
        // A clock set back counts as no time gone.
        if now.duration_since(written).unwrap_or_default() > GIVE_UP_AFTER {
            out.give_up.push(path);
        } else if box_ticked || !is_by_the_box(&path) {
            out.send.push(path);
        }
    }
    out
}

fn waiting(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == EXTENSION))
        .filter_map(|p| {
            let written = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((p, written))
        })
        .collect()
}

/// One round: what is too old goes, then what may leave is sent, each removed
/// once it has gone. The first refusal ends the round: the next file would
/// only meet the same closed door. Answers how many went.
pub async fn round<F, Fut>(dir: &Path, now: SystemTime, box_ticked: bool, mut send: F) -> usize
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let plan = plan(waiting(dir), now, box_ticked);
    for old in &plan.give_up {
        let _ = std::fs::remove_file(old);
        crate::aelog!("[REPORT] outbox: given up after 14 days");
    }
    let mut sent = 0;
    for path in &plan.send {
        let Ok(file) = std::fs::read(path) else { continue };
        let bytes = file.len();
        match send(file).await {
            Ok(code) => {
                let _ = std::fs::remove_file(path);
                crate::aelog!("[REPORT] sent from the outbox, {} bytes, code {}", bytes, code);
                sent += 1;
            }
            Err(e) => {
                crate::aelog!("[REPORT] outbox: still not sent: {}", e);
                break;
            }
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    fn at_days(now: SystemTime, name: &str, days: u64) -> (PathBuf, SystemTime) {
        (PathBuf::from(name), now - DAY * days as u32)
    }

    #[test]
    fn what_was_sent_by_hand_goes_oldest_first_and_too_old_is_given_up() {
        let now = SystemTime::now();
        let p = plan(vec![at_days(now, "b.aerep", 1), at_days(now, "a.aerep", 13), at_days(now, "old.aerep", 15)], now, false);
        assert_eq!(names(&p.send), ["a.aerep", "b.aerep"]);
        assert_eq!(names(&p.give_up), ["old.aerep"]);
    }

    #[test]
    fn what_the_box_sent_waits_while_the_box_is_unticked() {
        let now = SystemTime::now();
        let files = || vec![at_days(now, "x-auto.aerep", 1), at_days(now, "y-auto.aerep", 20)];
        let off = plan(files(), now, false);
        assert!(off.send.is_empty());
        assert_eq!(names(&off.give_up), ["y-auto.aerep"], "too old, ticked or not");
        assert_eq!(names(&plan(files(), now, true).send), ["x-auto.aerep"]);
    }

    #[test]
    fn a_clock_set_back_gives_up_on_nothing() {
        let now = SystemTime::now();
        let p = plan(vec![(PathBuf::from("a.aerep"), now + DAY * 30)], now, false);
        assert_eq!(names(&p.send), ["a.aerep"]);
    }

    #[test]
    fn a_round_removes_what_went_and_stops_at_the_first_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let first = keep_at(dir.path(), "AE-1", false, b"one").unwrap();
        let second = keep_at(dir.path(), "AE-2", false, b"two").unwrap();
        let third = keep_at(dir.path(), "AE-3", false, b"three").unwrap();
        let old = SystemTime::now() - DAY;
        for (i, p) in [&first, &second, &third].iter().enumerate() {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(old + Duration::from_secs(i as u64)).unwrap();
        }
        let tried = RefCell::new(Vec::new());
        let sent = tauri::async_runtime::block_on(round(dir.path(), SystemTime::now(), false, |file| {
            let n = tried.borrow().len();
            tried.borrow_mut().push(file);
            async move {
                if n == 0 { Ok("AE-OK".to_string()) } else { Err("offline".to_string()) }
            }
        }));
        assert_eq!(sent, 1);
        assert_eq!(*tried.borrow(), vec![b"one".to_vec(), b"two".to_vec()], "the third is not tried");
        assert!(!first.exists(), "gone once sent");
        assert!(second.exists() && third.exists(), "the refused one and the untried one stay");
    }

    #[test]
    fn half_a_file_is_never_sent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AuraEngine-report-AE-9.part"), b"half").unwrap();
        assert!(waiting(dir.path()).is_empty());
        let kept = keep_at(dir.path(), "AE-1", true, b"whole").unwrap();
        assert!(is_by_the_box(&kept));
        assert_eq!(waiting(dir.path()).len(), 1);
        assert_eq!(std::fs::read(kept).unwrap(), b"whole");
    }

    #[test]
    fn a_folder_that_is_not_there_is_an_empty_round() {
        let dir = tempfile::tempdir().unwrap();
        let sent = tauri::async_runtime::block_on(round(&dir.path().join("none"), SystemTime::now(), true, |_| async {
            Ok::<String, String>(String::new())
        }));
        assert_eq!(sent, 0);
    }
}
