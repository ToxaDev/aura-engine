//! Crash reports: anonymised, encrypted, and sent only by the user's hand.
//!
//! A report is built on request, shown as it is, and only then sealed. Nothing
//! leaves without a click in the report window or the "always send" checkbox
//! the user ticked there (the page decides that, `js/crash-report.js`). What a
//! fault wrote down stays on this machine until then (`pending.rs`). What was
//! sent and could not go waits, and goes later by itself (`outbox.rs`).

pub use aura_core::crash_report::known;
pub mod outbox;
pub use aura_core::crash_report::pending;
#[cfg(test)]
mod privacy_guard;
pub mod redact;
pub mod report;
pub mod seal;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;
use tauri::Manager;

/// A program that is stuck has its state behind the lock that is stuck, and
/// that is the moment a report is wanted: each question gets this long.
const ASK_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the receiver gets.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The report as the user last saw it. Sealed as it is, never rebuilt.
static BUILT: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// The last session's report has been dealt with in this run.
static LAST_DEALT_WITH: AtomicBool = AtomicBool::new(false);

/// The file a failed send of the window's report left waiting: the next press
/// of Send takes its place, or the same report would arrive twice.
static WAITING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// A round of the outbox is under way.
static IN_ROUND: AtomicBool = AtomicBool::new(false);

/// `f` on a thread of its own, for no longer than `ASK_TIMEOUT`.
fn ask<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aura-report-ask".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(ASK_TIMEOUT).ok()
}

fn gather() -> report::Parts {
    let player = || crate::player::controller::get();
    report::Parts {
        status: ask(move || player().status()),
        queue: ask(move || player().queue()).unwrap_or_default(),
        settings: ask(move || player().settings_now()),
        hp_prepare: ask(move || player().hp_prepare()),
        devices: ask(crate::player::output::list_devices).and_then(Result::ok).unwrap_or_default(),
        gpu: ask(|| serde_json::from_str(&crate::gpu_info()).ok()).flatten(),
    }
}

/// The last run ended without a clean exit, and that has not been dealt with.
fn last_session() -> Option<PathBuf> {
    if LAST_DEALT_WITH.load(Ordering::Relaxed) {
        return None;
    }
    crate::watchdog::crashed_session()
}

/// What a start has to say about the session before it.
#[derive(Debug, PartialEq)]
struct Offer {
    /// `stall`, `unclean_exit`, or the fault's own (`panic`, `hw_fault`).
    kind: String,
    /// The session's log, when it had one.
    log: Option<PathBuf>,
}

/// `crashed`: the log the `running` file named (empty when the session had
/// none), `None` after a clean end. A fault with a Yes behind it is offered
/// even so; one without has nothing to offer once the session ended cleanly.
fn offer_of(crashed: Option<&Path>, pending: Option<&pending::Pending>) -> Option<Offer> {
    if crashed.is_none() && !pending.is_some_and(|p| p.asked) {
        return None;
    }
    let log = crashed.filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
    let kind = match pending {
        Some(p) => p.kind.clone(),
        None => report::ended_as(&log.as_deref().map(report::read_log_tail).unwrap_or_default()).to_string(),
    };
    Some(Offer { kind, log })
}

/// What the page should offer at start.
#[tauri::command(async)]
pub fn crash_report_state() -> Value {
    let crashed = last_session();
    let pending = pending::read();
    // A fault in a run that went on to a clean exit, or a stale file, with no
    // Yes behind it: there is nothing to offer.
    if crashed.is_none() && pending.as_ref().is_some_and(|p| !p.asked) {
        pending::clear();
    }
    let offer = offer_of(crashed.as_deref(), pending.as_ref());
    if let Some(o) = &offer {
        let log = o.log.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
        crate::aelog!("[REPORT] the last session ended as {}, log {}", o.kind, log.as_deref().unwrap_or("none"));
    }
    json!({
        "crashed": crashed.is_some(),
        "asked": pending.is_some_and(|p| p.asked),
        "kind": offer.map(|o| o.kind),
    })
}

/// Build the report: `about` is "last" (the session that ended badly) or
/// "current". Answers the exact text that will be sealed.
#[tauri::command(async)]
pub fn crash_report_build(about: String, note: String) -> Result<Value, String> {
    let about_last = about == "last";
    let log_path = if about_last {
        last_session().or_else(crate::watchdog::this_session)
    } else {
        crate::watchdog::this_session()
    };
    let log_path = log_path.filter(|p| !p.as_os_str().is_empty());
    let parts = gather();
    let mut met = known::current();
    if let Some(p) = log_path.as_ref().filter(|_| about_last) {
        met.extend(known::of_session(p));
    }
    let redactor = report::build_redactor(&parts.queue, &parts.devices, &met);
    let mut log = log_path.as_deref().map(report::read_log_tail).unwrap_or_default();
    let pending = pending::read();
    let moment = report::Moment::now();
    let text = loop {
        let v = report::build(&report::Input {
            parts: &parts,
            about_last,
            pending: pending.as_ref(),
            log: &log,
            note: &note,
            redactor: &redactor,
            moment: &moment,
        });
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        // Sealing adds a few dozen bytes; the receiver's limit is firm.
        let small = seal::deflate(text.as_bytes())?.len() < seal::MAX_FILE - 1024;
        if small || log.is_empty() {
            break text;
        }
        log.drain(..log.len().div_ceil(2));
    };
    *BUILT.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.clone().into_bytes());
    Ok(json!({ "json": text }))
}

fn built_and_sealed() -> Result<Vec<u8>, String> {
    let built = BUILT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    seal::seal(&built.ok_or("no report has been built")?)
}

/// `AE-7K3F-Q9`: letters that cannot be misread, from a code of its own when
/// the report is only saved (the receiver gives one when it is sent).
fn local_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut r = rand::thread_rng();
    let mut pick = |n: usize| -> String { (0..n).map(|_| ALPHABET[r.gen_range(0..ALPHABET.len())] as char).collect() };
    format!("AE-{}-{}", pick(4), pick(2))
}

fn open_folder(dir: &std::path::Path) {
    let opener = if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(dir).spawn();
}

fn parse_code(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let code = v.get("code")?.as_str()?;
    (code.len() <= 32 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| code.to_string())
}

/// A test build can send elsewhere; a released one sends only to the receiver.
fn report_url() -> String {
    let test_url = option_env!("AURA_RELEASE_BUILD").is_none().then(|| std::env::var("AURA_REPORT_URL").ok()).flatten();
    test_url.unwrap_or_else(|| crate::updater::REPORT_URL.to_string())
}

/// What the user reads: the system's own text is long and in the system's
/// language, so a report that found no way out says so plainly.
fn plain_reason(timed_out: bool, no_connection: bool, raw: String) -> String {
    if timed_out {
        "the server did not answer in time".to_string()
    } else if no_connection {
        "no connection to the server".to_string()
    } else {
        raw
    }
}

fn reason_of(e: reqwest::Error) -> String {
    let (timed_out, no_connection) = (e.is_timeout(), e.is_connect());
    let raw = e.without_url().to_string();
    crate::aelog!("[REPORT] send error: {}", raw);
    plain_reason(timed_out, no_connection, raw)
}

async fn post(file: Vec<u8>) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("AuraEngine/", env!("CARGO_PKG_VERSION")))
        .timeout(SEND_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(report_url())
        .header("content-type", "application/octet-stream")
        .body(file)
        .send()
        .await
        .map_err(reason_of)?;
    let status = resp.status();
    let body = resp.text().await.map_err(reason_of)?;
    if !status.is_success() {
        return Err(format!("the server answered {}", status.as_u16()));
    }
    parse_code(&body).ok_or_else(|| "the server gave no code".to_string())
}

/// Send what was last built. On failure the file waits in the outbox, and the
/// answer says where. `auto`: the ticked box sent it, not a press of Send.
#[tauri::command]
pub async fn crash_report_send(auto: bool) -> Result<Value, Value> {
    let file = built_and_sealed().map_err(|e| json!({ "error": e }))?;
    let result = post(file.clone()).await;
    let earlier = WAITING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(p) = earlier {
        let _ = std::fs::remove_file(p);
    }
    match result {
        Ok(code) => {
            crate::aelog!("[REPORT] sent, {} bytes, code {}", file.len(), code);
            Ok(json!({ "code": code }))
        }
        Err(e) => {
            crate::aelog!("[REPORT] not sent: {}", e);
            let kept = outbox::dir().and_then(|d| outbox::keep_at(&d, &local_code(), auto, &file));
            *WAITING.lock().unwrap_or_else(|e| e.into_inner()) = kept.clone();
            Err(json!({ "error": e, "kept": kept.map(|p| p.display().to_string()) }))
        }
    }
}

/// A quiet round of the outbox: at start and once an hour (the page's timer,
/// which also knows the box). `auto`: the box is ticked now.
#[tauri::command]
pub async fn crash_report_retry(auto: bool) -> usize {
    let Some(dir) = outbox::dir() else { return 0 };
    if IN_ROUND.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let sent = outbox::round(&dir, std::time::SystemTime::now(), auto, post).await;
    IN_ROUND.store(false, Ordering::Release);
    sent
}

/// Save what was last built on the desktop, and show it.
#[tauri::command(async)]
pub fn crash_report_save(app: tauri::AppHandle) -> Result<Value, String> {
    let file = built_and_sealed()?;
    let dir = app.path().desktop_dir().or_else(|_| app.path().home_dir()).map_err(|e| e.to_string())?;
    let code = local_code();
    let path = dir.join(format!("AuraEngine-report-{code}.aerep"));
    std::fs::write(&path, file).map_err(|e| e.to_string())?;
    open_folder(&dir);
    crate::aelog!("[REPORT] saved, code {}", code);
    Ok(json!({ "code": code, "path": path.display().to_string() }))
}

/// The last session's report was made, or declined: not offered again.
#[tauri::command(async)]
pub fn crash_report_done() {
    LAST_DEALT_WITH.store(true, Ordering::Relaxed);
    pending::clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_that_found_no_way_out_says_so_plainly() {
        let raw = || "tcp connect error: Подключение не установлено (os error 10061)".to_string();
        assert_eq!(plain_reason(false, true, raw()), "no connection to the server");
        assert_eq!(plain_reason(true, true, raw()), "the server did not answer in time");
        assert_eq!(plain_reason(false, false, raw()), raw(), "anything else stays as it came");
    }

    #[test]
    fn each_report_gets_a_code_of_its_own() {
        let codes: std::collections::HashSet<String> = (0..64).map(|_| local_code()).collect();
        assert!(codes.len() > 60, "{codes:?}");
    }

    #[test]
    fn a_local_code_looks_like_the_receivers() {
        let c = local_code();
        let parts: Vec<&str> = c.split('-').collect();
        assert_eq!(parts[0], "AE");
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 2);
    }

    #[test]
    fn only_a_plain_code_is_taken_from_the_server() {
        assert_eq!(parse_code(r#"{"code":"AE-7K3F-Q9"}"#).as_deref(), Some("AE-7K3F-Q9"));
        assert_eq!(parse_code(r#"{"code":"<script>"}"#), None);
        assert_eq!(parse_code("not json"), None);
    }

    /// A session that froze the machine: its `running` file and its log are what
    /// the disk still holds, with a stall in the log.
    fn left_behind(dir: &Path, log_text: &str) -> (PathBuf, PathBuf) {
        let marker = dir.join("running");
        let log = dir.join("session-frozen.log");
        std::fs::write(&log, log_text).unwrap();
        crate::watchdog::write_synced(&marker, log.to_string_lossy().as_bytes()).unwrap();
        (marker, log)
    }

    #[test]
    fn a_start_after_a_freeze_offers_the_stall_with_the_log_of_that_session() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[10:00:00.000] [UI] started\n[10:00:31.000] [WATCHDOG] stall prep: no change for 31 s\n";
        let (marker, log) = left_behind(dir.path(), text);
        let before = crate::watchdog::begin_at(&marker, Some(&dir.path().join("session-next.log")));
        assert_eq!(before.as_deref(), Some(log.as_path()));
        let offer = offer_of(before.as_deref(), None).unwrap();
        assert_eq!(offer, Offer { kind: "stall".into(), log: Some(log) });
    }

    #[test]
    fn zeros_a_power_cut_left_at_the_end_of_a_log_do_not_hide_the_stall() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cut.log");
        let mut bytes = b"[10:00:31.000] [WATCHDOG] stall prep: no change for 31 s\n".to_vec();
        bytes.extend([0u8; 4096]);
        std::fs::write(&log, bytes).unwrap();
        let lines = report::read_log_tail(&log);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(report::ended_as(&lines), "stall");
    }

    #[test]
    fn a_start_after_a_kill_without_a_stall_is_an_unclean_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (marker, _) = left_behind(dir.path(), "[10:00:00.000] [UI] started\n");
        let before = crate::watchdog::begin_at(&marker, None);
        assert_eq!(offer_of(before.as_deref(), None).unwrap().kind, "unclean_exit");
    }

    #[test]
    fn a_session_with_no_log_is_offered_all_the_same() {
        assert_eq!(offer_of(Some(Path::new("")), None), Some(Offer { kind: "unclean_exit".into(), log: None }));
    }

    #[test]
    fn a_fault_gives_the_offer_its_kind_and_only_a_yes_outlives_a_clean_end() {
        let said = |asked| pending::Pending { kind: "panic".into(), asked, ..Default::default() };
        assert_eq!(offer_of(Some(Path::new("a.log")), Some(&said(false))).unwrap().kind, "panic");
        assert_eq!(offer_of(None, Some(&said(true))).unwrap().kind, "panic");
        assert_eq!(offer_of(None, Some(&said(false))), None);
        assert_eq!(offer_of(None, None), None);
    }

    #[test]
    fn a_crashed_session_is_offered_once_however_many_starts_follow() {
        let dir = tempfile::tempdir().unwrap();
        let (marker, _) = left_behind(dir.path(), "[10:00:31.000] [WATCHDOG] stall prep\n");
        let next = dir.path().join("session-next.log");
        let first = crate::watchdog::begin_at(&marker, Some(&next));
        assert!(offer_of(first.as_deref(), None).is_some());
        // Not now, then this run leaves on its own terms: its `session_end` takes the file away.
        std::fs::remove_file(&marker).unwrap();
        let second = crate::watchdog::begin_at(&marker, Some(&dir.path().join("session-third.log")));
        assert_eq!(offer_of(second.as_deref(), None), None);
    }

    #[test]
    fn a_question_gets_its_answer() {
        assert_eq!(ask(|| 5), Some(5));
    }
}
