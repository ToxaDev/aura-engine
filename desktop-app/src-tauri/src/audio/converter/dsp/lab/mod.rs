pub mod declip;
pub mod headroom;
pub mod isp;
pub mod stats;
pub mod tfs;
pub mod xtc;

/// What the output ceiling cost this file, as a suffix for the line the
/// interface shows when a conversion finishes. Empty when it cost nothing.
///
/// The ceiling only ever attenuates. A file whose output peak lands above it
/// is handed back quieter by exactly the amount it was over — and that is the
/// whole of what "the lab version sounds quieter" means: declip rebuilds the
/// peaks the master had clipped off, TFS re-times the ones the linear-phase
/// filter had smeared, both are real, and the ceiling takes the difference off
/// the entire file. The number has always been in the log. Nobody reads the
/// log while listening, so it belongs on the row too.
pub fn ceiling_note(peak_gain: f64, target_dbtp: f64) -> String {
    if !(peak_gain < 1.0) {
        return String::new();
    }
    format!(
        ", {:.2} dB to hold {:.1} dBTP",
        20.0 * peak_gain.log10(),
        target_dbtp
    )
}

/// Outcome record for a single lab feature pass on one file.
pub struct LabOutcome {
    /// Short feature tag, e.g. "DECLIP", "ISP", "XTC".
    pub feature: &'static str,
    /// Severity level: "ok" (green), "none" (gray), "warn" (amber), "fail" (red).
    pub level: &'static str,
    /// Human-readable detail printed in the console report.
    pub text: String,
}

/// The chain badge in a queue row has three states, and every one of them
/// has to be true: it ran, it was reached and declined, or it has not been
/// reached yet. The console report already knows the first two per feature —
/// this turns that knowledge into something the row can draw.
///
/// AA, PFR and HP are not lab features and carry their verdicts elsewhere, so
/// the caller passes them in alongside. Everything is emitted in pipeline
/// order by the caller, which is the order the row draws.
pub const CHAIN_RAN: u8 = 1;
pub const CHAIN_DECLINED: u8 = 2;
pub const CHAIN_FAILED: u8 = 3;

/// The chain in the order the pipeline runs it.
///
/// Tokens are pushed as each stage fires, from two files and in whatever
/// order the code happens to reach them — TFS is decided before the FIR runs,
/// PFR while it runs. The order they are SHOWN in, in the filename and in
/// AURA_CHAIN, is fixed here instead of left to the order they were pushed.
/// The panel's LAB_CHAIN in labpanel.js is the same list.
///
/// The subsonic filter (upstream 1.2.8) carries its corner in the token, so
/// it has three names and one place: after ISP, before AHR.
pub const CHAIN_ORDER: [&str; 12] = [
    "DC", "ISP", "SUB20", "SUB15", "SUB10", "AHR", "AA", "PFR", "HP", "aHP", "TFS", "XTC",
];

pub fn in_chain_order(tokens: &[&'static str]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for want in CHAIN_ORDER {
        if let Some(t) = tokens.iter().find(|t| **t == want) {
            out.push(*t);
        }
    }
    // A token nobody listed above still belongs in the name; it goes last
    // rather than disappearing because someone forgot to extend the array.
    for t in tokens {
        if !CHAIN_ORDER.contains(t) {
            out.push(*t);
        }
    }
    out
}

pub fn chain_state(level: &str) -> u8 {
    match level {
        "none" => CHAIN_DECLINED,
        "fail" => CHAIN_FAILED,
        _ => CHAIN_RAN,   // "ok" and "warn" both mean it did something
    }
}

/// `[["DC",1,"fixed 11750 short + 5213 plateaus"], ...]`, hand-rolled to keep
/// serde out of the dependency list for one line of output.
pub fn chain_json(entries: &[(&str, u8, String)]) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "'").replace('\n', " ");
    let body: Vec<String> = entries
        .iter()
        .map(|(tok, st, why)| format!(r#"["{}",{},"{}"]"#, esc(tok), st, esc(why)))
        .collect();
    format!("[{}]", body.join(","))
}

// ── Windows VT console mode ───────────────────────────────────────────────────────────────────
//
// Enable ENABLE_VIRTUAL_TERMINAL_PROCESSING so ANSI escape codes render in
// the Windows console.  Called at most once, via a Once guard.
// If any Win32 call fails the flag stays false and plain text is emitted.
//
// The kernel32 declarations cover only the three functions we need and carry
// no external crate dependencies: kernel32.dll is unconditionally linked by
// the Rust Windows toolchain.
#[cfg(windows)]
mod ansi_init {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Once;

    pub static ANSI_OK: AtomicBool = AtomicBool::new(false);
    static INIT: Once = Once::new();

    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
    }

    pub fn ensure() {
        INIT.call_once(|| unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if h.is_null() || h as isize == -1 {
                return;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return;
            }
            if SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 {
                ANSI_OK.store(true, Ordering::Relaxed);
            }
        });
    }
}

#[cfg(not(windows))]
mod ansi_init {
    use std::sync::atomic::{AtomicBool, Ordering};
    // On non-Windows targets ANSI escape codes are unconditionally supported.
    pub static ANSI_OK: AtomicBool = AtomicBool::new(true);
    pub fn ensure() {}
    // Suppress unused-import warning on the Ordering import used only on Windows.
    #[allow(dead_code)]
    fn _use_ordering() { let _ = Ordering::Relaxed; }
}

/// Print a colored lab report block to the console and mirror plain text to
/// the session log file.  The output goes through the same `lock_out()` /
/// `file_line()` / `touch()` discipline as `aelog!` so it never interleaves
/// with the heartbeat ticker.
///
/// Does nothing when `outcomes` is empty (no lab features were enabled).
pub fn print_lab_report(output_name: &str, outcomes: &[LabOutcome]) {
    if outcomes.is_empty() {
        return;
    }

    ansi_init::ensure();
    let use_color = ansi_init::ANSI_OK.load(std::sync::atomic::Ordering::Relaxed);

    let ts = crate::audio::logging::ts();
    let rule = format!("─── LAB REPORT · {} ───", output_name);
    let header = format!("[{}] {}", ts, rule);
    let footer = format!("[{}] {}", ts, "─".repeat(rule.chars().count()));

    // Build (console_line, plain_line) pairs for each outcome.
    let body: Vec<(String, String)> = outcomes
        .iter()
        .map(|o| plain_body_line_pair(use_color, &ts, o))
        .collect();

    // One locked write for the entire block — no heartbeat can interleave.
    let _g = crate::audio::logging::lock_out();
    println!("{}", header);
    for (console, _) in &body {
        println!("{}", console);
    }
    println!("{}", footer);
    // Mirror plain text to the session log file (no escape codes in the file).
    crate::audio::logging::file_line(&header);
    for (_, plain) in &body {
        crate::audio::logging::file_line(plain);
    }
    crate::audio::logging::file_line(&footer);
    crate::audio::logging::touch();
}

/// Build a (console_line, plain_line) pair for one `LabOutcome`.
/// When `use_color` is false both members are identical plain text.
fn plain_body_line_pair(
    use_color: bool,
    ts: &str,
    o: &LabOutcome,
) -> (String, String) {
    const FEAT_W: usize = 8;
    let (dot, esc): (char, &str) = match o.level {
        "ok"   => ('●', "\x1b[92m"),
        "warn" => ('●', "\x1b[93m"),
        "fail" => ('●', "\x1b[91m"),
        _      => ('○', "\x1b[90m"),
    };
    let feat = format!("{:<width$}", o.feature, width = FEAT_W);
    let plain = format!("[{}]   {} {} {}", ts, dot, feat, o.text);
    let console = if use_color {
        format!("[{}]   {}{}\x1b[0m {} {}", ts, esc, dot, feat, o.text)
    } else {
        plain.clone()
    };
    (console, plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_line_ok_uses_filled_dot() {
        let o = LabOutcome { feature: "DECLIP", level: "ok", text: "fixed 3 runs".to_string() };
        let (console, plain) = plain_body_line_pair(false, "12:00:00.000", &o);
        assert_eq!(console, plain, "no-color: console and plain must be identical");
        assert!(plain.contains('●'), "ok level must use filled dot");
        assert!(plain.contains("DECLIP  "), "DECLIP padded to 8 chars");
        assert!(plain.contains("fixed 3 runs"));
    }

    #[test]
    fn plain_line_none_uses_open_dot() {
        let o = LabOutcome { feature: "ISP", level: "none", text: "no exceedance found".to_string() };
        let (_, plain) = plain_body_line_pair(false, "00:00:00.000", &o);
        assert!(plain.contains('○'), "none level must use open dot");
        assert!(!plain.contains('●'), "none level must not use filled dot");
    }

    #[test]
    fn plain_line_short_name_is_padded() {
        // DECLIP is 6 chars; padded to 8 it must carry exactly two trailing spaces.
        let o = LabOutcome { feature: "DECLIP", level: "ok", text: "37 runs".to_string() };
        let (_, plain) = plain_body_line_pair(false, "00:00:00.000", &o);
        assert!(plain.contains("DECLIP  "), "DECLIP must be padded to 8 chars");
    }

    #[test]
    fn color_line_contains_ansi_reset() {
        let o = LabOutcome { feature: "XTC", level: "ok", text: "cancelled".to_string() };
        let (console, plain) = plain_body_line_pair(true, "00:00:00.000", &o);
        assert!(console.contains("\x1b[92m"), "ok color code must appear");
        assert!(console.contains("\x1b[0m"), "reset code must appear");
        assert!(!plain.contains('\x1b'), "plain must contain no escape codes");
    }

    #[test]
    fn ceiling_note_is_silent_when_nothing_was_taken_off() {
        assert_eq!(ceiling_note(1.0, -0.5), "");
        assert_eq!(ceiling_note(f64::NAN, -0.5), "");
    }

    #[test]
    fn ceiling_note_states_the_cost_and_the_ceiling() {
        // gain 0.604239 is -4.38 dB — the number from a real TFS conversion.
        let s = ceiling_note(0.604_239, -0.5);
        assert!(s.contains("-4.38 dB"), "got {s}");
        assert!(s.contains("-0.5 dBTP"), "got {s}");
    }

    #[test]
    fn print_lab_report_noop_on_empty() {
        // Must not panic or print when no lab features fired.
        print_lab_report("dummy.flac", &[]);
    }
}
