//! Why the app did not start.
//!
//! Until now a failure before the window came up was indistinguishable from
//! nothing happening. The program is a console application, Windows closes
//! that console the moment the process exits, and the panic message goes with
//! it — so a user double-clicks the exe, sees a black rectangle flash, and
//! deletes the folder. That is a real report from a real user on Windows 10,
//! not a hypothetical.
//!
//! Every way this program can die now ends in two things: a message box the
//! user can read, and a file they can send back. Both name the actual cause
//! where we can work it out.
//!
//! Where the cause has a known cure — a runtime that is not installed, a CPU
//! these binaries are not built for — the box also offers to open the page
//! that fixes it, and the report carries the same address in writing. An
//! address nobody can click, in a window nobody can copy text out of, is not
//! an answer.
//!
//! The three causes worth naming by hand, in the order they are likely:
//!
//!   * **No WebView2 runtime.** Windows 11 ships it; Windows 10 does not
//!     always have it, and the LTSC and N editions never do. Tauri cannot
//!     make a window without it and fails at `run()`. Checked before we get
//!     that far, because the message "install this" is worth more than a
//!     Rust error string.
//!
//!   * **A CPU without AVX2.** The release binaries are built for
//!     `x86-64-v3`, which is Haswell and newer. On anything older the process
//!     dies on an illegal instruction with no message at all. Checked first
//!     thing in `main`, and caught by a vectored handler if something got
//!     there before the check.
//!
//!   * **Anything else** — a panic on any thread, or `run()` returning an
//!     error. Both are caught rather than left to `expect`.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

const APP: &str = "Aura Engine";

/// A UTF-16, NUL-terminated copy, which is what every W-suffixed Win32 call
/// wants and what Rust strings are not.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Put it in front of the user. `MB_SETFOREGROUND | MB_TOPMOST` because the
/// app has no window of its own yet, and a box behind the file manager is the
/// same as no box.
pub fn message_box(title: &str, body: &str) {
    #[cfg(windows)]
    unsafe {
        use winapi::um::winuser::{
            MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
        };
        MessageBoxW(
            std::ptr::null_mut(),
            wide(body).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
    #[cfg(not(windows))]
    {
        eprintln!("{}: {}", title, body);
    }
}

/// The same box with a way out of it.
///
/// `MB_OK` leaves a user who cannot start the program holding an address they
/// have to retype into a browser by hand — from a window they cannot select
/// text in. Yes/No makes the fix one click. Returns true when they took it.
fn message_box_offer(title: &str, body: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use winapi::um::winuser::{
            MessageBoxW, IDYES, MB_ICONERROR, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
        };
        return MessageBoxW(
            std::ptr::null_mut(),
            wide(body).as_ptr(),
            wide(title).as_ptr(),
            MB_YESNO | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        ) == IDYES;
    }
    #[cfg(not(windows))]
    {
        eprintln!("{}: {}", title, body);
        false
    }
}

/// Hand a page to whatever the user browses with.
///
/// False when the shell refuses — no browser association, a policy that
/// blocks it — so the caller can put the address on screen as text rather
/// than leave a Yes that did nothing.
fn open_url(url: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use winapi::um::shellapi::ShellExecuteW;
        use winapi::um::winuser::SW_SHOWNORMAL;
        // ShellExecute signals success with a fake HINSTANCE above 32. It is
        // the documented test and it is as odd as it looks.
        return ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        ) as usize
            > 32;
    }
    #[cfg(not(windows))]
    {
        eprintln!("open: {}", url);
        false
    }
}

/// What would fix this failure, and where.
///
/// Two phrasings of one thing, kept together so they cannot drift apart: the
/// box asks a question because it has a Yes button, the report gives an
/// instruction because it is read later by someone who cannot press anything.
/// The address is written once.
struct Remedy {
    /// Last line of the box, immediately above the buttons.
    ask: &'static str,
    /// The `next step` line of the report.
    step: &'static str,
    url: &'static str,
}

/// Microsoft's permanent address for the Evergreen bootstrapper —
/// `MicrosoftEdgeWebview2Setup.exe`, 1.8 MB, which then fetches the runtime
/// itself. Deliberately not the product page: that page offers five downloads
/// and asks the visitor to choose, and choosing wrong is how somebody who has
/// already failed to start this app gives up for the second time.
const WEBVIEW2_SETUP: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// Where a machine we cannot run on should be reported. Named here rather
/// than written into each message, so there is one line to change.
const ISSUES: &str = "https://github.com/ToxaDev/aura-engine/issues";

/// The offer that goes with a missing runtime. Written once and used by the
/// pre-flight check, by the last-ditch handler after `run()` has already
/// failed, and by `--selftest=webview2` — so the self test rehearses the
/// wording a stranger will actually see.
const WEBVIEW2_REMEDY: Remedy = Remedy {
    ask: "Download it from Microsoft now?",
    step: "install the Microsoft Edge WebView2 runtime from",
    url: WEBVIEW2_SETUP,
};

/// Where a crash report can be written.
///
/// Next to the executable first: these are handed out as portable zips, and a
/// file in the folder the user already has open is a file they can actually
/// find and attach. Falls back to the log directory when the folder is not
/// writable — Program Files, a read-only share, a zip opened in place.
fn crash_report_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("AuraEngine-crash.txt");
            if std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .is_ok()
            {
                return Some(p);
            }
        }
    }
    let dir = crate::audio::logging::session_log_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        return Some(dir.join("AuraEngine-crash.txt"));
    }
    None
}

/// What this binary needs from a CPU, read off the compiler's own view rather
/// than a build variable someone has to remember to set. The release workflow
/// builds with `-C target-cpu=x86-64-v3`; a baseline build of the same source
/// answers differently here, and the check below then lets it run.
fn required_features() -> Vec<&'static str> {
    let mut v = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if cfg!(target_feature = "avx") { v.push("avx"); }
        if cfg!(target_feature = "avx2") { v.push("avx2"); }
        if cfg!(target_feature = "fma") { v.push("fma"); }
        if cfg!(target_feature = "bmi2") { v.push("bmi2"); }
    }
    v
}

fn built_for() -> String {
    let need = required_features();
    if need.is_empty() {
        "x86-64 baseline".to_string()
    } else {
        format!("x86-64 + {}", need.join(", "))
    }
}

/// What the machine is, so a report answers the first three questions we
/// would otherwise have to ask.
fn environment() -> String {
    let mut s = String::new();
    let _ = writeln!(s, "app          {} {}", APP, env!("CARGO_PKG_VERSION"));
    let _ = writeln!(s, "built for    {}", built_for());
    #[cfg(target_arch = "x86_64")]
    {
        let _ = writeln!(
            s,
            "cpu features avx={} avx2={} fma={} bmi2={}",
            is_x86_feature_detected!("avx"),
            is_x86_feature_detected!("avx2"),
            is_x86_feature_detected!("fma"),
            is_x86_feature_detected!("bmi2"),
        );
    }
    let _ = writeln!(
        s,
        "webview2     {}",
        webview2_version().unwrap_or_else(|| "NOT INSTALLED".to_string())
    );
    if let Ok(exe) = std::env::current_exe() {
        let _ = writeln!(s, "exe          {}", exe.display());
    }
    s
}

/// Append a report and tell the user where it went. Returns the path so the
/// caller can name it in the box.
pub fn write_report(what: &str, detail: &str) -> Option<PathBuf> {
    use std::io::Write;
    let path = crash_report_path()?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let _ = writeln!(
        f,
        "\n═══ {} ═══\nwhen         {}\n{}\n{}\n",
        what,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        environment(),
        detail
    );
    Some(path)
}

/// Report and show, in one call, with the report path appended to the body.
pub fn fail(title: &str, body: &str, detail: &str) {
    fail_inner(title, body, detail, true, None)
}

/// The same, for the failures we know the cure for: the box offers to open
/// the page, and the report carries the address whether or not anyone
/// pressed anything.
fn fail_with_remedy(title: &str, body: &str, detail: &str, remedy: Remedy) {
    fail_inner(title, body, detail, true, Some(remedy))
}

/// One box, ever.
///
/// Every report still reaches the file. The box does not: a failure that
/// repeats — a worker thread panicking once per file in a batch — would
/// otherwise stack modal windows over the app until the user could not reach
/// the close button.
static BOX_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `log` is false from inside the panic hook. `aelog!` takes the log mutex,
/// and a thread that panicked while already holding it would deadlock on
/// itself — a hang instead of a crash, which is worse than what we started
/// with. The report file is written either way.
fn fail_inner(title: &str, body: &str, detail: &str, log: bool, remedy: Option<Remedy>) {
    // The cure goes into the file as well as onto the screen. A box gets
    // dismissed unread, and the person we end up helping by mail is often not
    // the person who dismissed it.
    let detail = match &remedy {
        Some(r) => format!("{}\nnext step    {}\n             {}", detail, r.step, r.url),
        None => detail.to_string(),
    };
    let path = write_report(title, &detail);
    if log {
        crate::aelog!("[STARTUP] {}: {}", title, detail.replace('\n', " | "));
    }
    if BOX_SHOWN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // Our own automation drives the failure paths and cannot dismiss a modal
    // window. Not documented anywhere a user would read.
    if std::env::var_os("AURA_SELFTEST_NO_BOX").is_some() {
        return;
    }
    let mut msg = body.to_string();
    if let Some(p) = path {
        msg.push_str(&format!(
            "\n\nA report was written to:\n{}\n\nSending that file is enough to \
             tell us what happened.",
            p.display()
        ));
    }
    let title = format!("{} — {}", APP, title);
    let Some(r) = remedy else {
        message_box(&title, &msg);
        return;
    };
    msg.push_str(&format!("\n\n{}", r.ask));
    if message_box_offer(&title, &msg) && !open_url(r.url) {
        // Yes, and nothing opened. Saying so beats a button that quietly did
        // nothing — and the address has to be readable somewhere.
        message_box(
            &title,
            &format!(
                "This machine has no browser to open it with. The address is:\n\n{}",
                r.url
            ),
        );
    }
}

// ── WebView2 ──────────────────────────────────────────────────────────────

/// The version string the Evergreen runtime registers, or None when it is not
/// installed. These four locations are the documented ones; a machine-wide
/// install on 64-bit Windows lands in the WOW6432Node branch even though the
/// runtime itself is 64-bit.
#[cfg(windows)]
pub fn webview2_version() -> Option<String> {
    const CLIENT: &str =
        "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let roots: [(winapi::shared::minwindef::HKEY, String); 3] = [
        (
            winapi::um::winreg::HKEY_LOCAL_MACHINE,
            format!("SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{}", CLIENT),
        ),
        (
            winapi::um::winreg::HKEY_LOCAL_MACHINE,
            format!("SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{}", CLIENT),
        ),
        (
            winapi::um::winreg::HKEY_CURRENT_USER,
            format!("Software\\Microsoft\\EdgeUpdate\\Clients\\{}", CLIENT),
        ),
    ];
    for (root, path) in roots {
        if let Some(v) = read_reg_sz(root, &path, "pv") {
            // A stub install leaves 0.0.0.0 behind; that is not a runtime.
            if !v.is_empty() && v != "0.0.0.0" {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(not(windows))]
pub fn webview2_version() -> Option<String> {
    Some("n/a".to_string())
}

#[cfg(windows)]
fn read_reg_sz(
    root: winapi::shared::minwindef::HKEY,
    path: &str,
    name: &str,
) -> Option<String> {
    use winapi::shared::winerror::ERROR_SUCCESS;
    use winapi::um::winreg::RegGetValueW;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    // KEY_WOW64_64KEY has no effect on RegGetValueW's flags; the two explicit
    // paths above cover the redirection instead.
    let mut len: u32 = 0;
    unsafe {
        let sub = wide(path);
        let val = wide(name);
        let rc = RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        );
        if rc != ERROR_SUCCESS as i32 || len == 0 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize / 2) + 1];
        let mut len2 = len;
        let rc = RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut len2,
        );
        if rc != ERROR_SUCCESS as i32 {
            return None;
        }
        let s: Vec<u16> = buf.into_iter().take_while(|&c| c != 0).collect();
        Some(String::from_utf16_lossy(&s))
    }
}

// ── the checks themselves ─────────────────────────────────────────────────

/// Run before anything else in `main`. Returns false when the program cannot
/// usefully continue; the user has already been told why.
pub fn preflight() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        // The release binaries are built for x86-64-v3. On an older CPU the
        // process dies on an illegal instruction, historically with no
        // message at all. This runs early enough to say so instead — and if
        // the fault happens before it, the vectored handler below says it.
        //
        // Only what THIS binary was compiled to need is checked: a baseline
        // build of the same source has to keep running on the same old CPU
        // this refuses to start on.
        let missing: Vec<&str> = required_features()
            .into_iter()
            .filter(|f| match *f {
                "avx" => !is_x86_feature_detected!("avx"),
                "avx2" => !is_x86_feature_detected!("avx2"),
                "fma" => !is_x86_feature_detected!("fma"),
                _ => !is_x86_feature_detected!("bmi2"),
            })
            .collect();
        if !missing.is_empty() {
            fail_with_remedy(
                "This CPU is too old for these binaries",
                &format!(
                    "The released builds are compiled for x86-64-v3, which needs \
                     AVX2 — Intel Haswell (2013) or AMD Excavator (2015) and newer.\n\n\
                     This CPU is missing: {}.\n\n\
                     The program cannot run here as it is. Ask and we will \
                     publish a baseline build that can.",
                    missing.join(", ")
                ),
                &format!("missing CPU features: {}", missing.join(", ")),
                Remedy {
                    ask: "Open the issue tracker now?",
                    step: "ask for a baseline build at",
                    url: ISSUES,
                },
            );
            return false;
        }
    }

    if webview2_version().is_none() {
        fail_with_remedy(
            "Microsoft Edge WebView2 is missing",
            "Aura Engine draws its window with the Edge WebView2 runtime. \
             Windows 11 has it already; Windows 10 often does not, and the \
             LTSC and N editions never do.\n\n\
             Microsoft gives it away: the installer is 1.8 MB and asks for no \
             restart. Once it is in, start Aura Engine again — nothing else \
             needs changing.",
            "WebView2 runtime not found in the registry",
            WEBVIEW2_REMEDY,
        );
        return false;
    }

    true
}

/// Catch what `preflight` cannot: a panic on any thread, and a hardware fault
/// anywhere. Both used to end the process without a word.
pub fn install_handlers() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let detail = format!(
            "thread       {}\npanic        {}",
            thread.name().unwrap_or("unnamed"),
            info
        );
        fail_inner(
            "Aura Engine stopped unexpectedly",
            "Something failed that the program could not recover from.",
            &detail,
            false,
            None,
        );
        previous(info);
    }));

    #[cfg(windows)]
    unsafe {
        winapi::um::errhandlingapi::AddVectoredExceptionHandler(1, Some(on_hardware_fault));
    }
}

/// First in the vectored chain, so it sees the fault before anything else and
/// before the process is torn down. It reports and steps aside: returning
/// CONTINUE_SEARCH lets the normal machinery run, which will end the process
/// as it always did — only now the user knows why.
#[cfg(windows)]
unsafe extern "system" fn on_hardware_fault(
    info: *mut winapi::um::winnt::EXCEPTION_POINTERS,
) -> i32 {
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
    const PRIVILEGED_INSTRUCTION: u32 = 0xC000_0096;

    if info.is_null() || (*info).ExceptionRecord.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let code = (*(*info).ExceptionRecord).ExceptionCode;
    // Only the ones worth interrupting for. Anything else (a breakpoint under
    // a debugger, a C++ exception passing through) is not ours.
    if code == ILLEGAL_INSTRUCTION || code == PRIVILEGED_INSTRUCTION {
        let what = if code == ILLEGAL_INSTRUCTION {
            "illegal instruction"
        } else {
            "privileged instruction"
        };
        // The address goes in as text, not as a button. Everything else that
        // knows a cure offers to open it; this path will not, because it runs
        // inside a vectored exception handler and `ShellExecuteW` starts COM
        // and a process from there. The process is already dying, and a hang
        // is worse than a crash.
        fail(
            "This CPU cannot run these binaries",
            &format!(
                "The program hit an instruction this processor does not have. \
                 The released builds need AVX2 — Intel Haswell (2013) or AMD \
                 Excavator (2015) and newer.\n\n\
                 Ask for a baseline build and we will publish one:\n{}",
                ISSUES
            ),
            &format!("hardware fault: {} (0x{:08X})", what, code),
        );
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// The last line of defence: `run()` came back with an error, so there is no
/// window and never will be.
pub fn tauri_failed(err: &dyn std::fmt::Display) {
    let detail = format!("tauri run error: {}", err);
    // A stub entry in the registry passes the pre-flight check and still
    // cannot make a window, so the runtime is worth asking about again here
    // rather than assuming the earlier answer held.
    if webview2_version().is_none() {
        fail_with_remedy(
            "Aura Engine could not open its window",
            "The interface failed to start. The Edge WebView2 runtime is not \
             installed on this machine, which is almost certainly the reason.\n\n\
             Microsoft gives it away: the installer is 1.8 MB and asks for no \
             restart.",
            &detail,
            WEBVIEW2_REMEDY,
        );
        return;
    }
    fail(
        "Aura Engine could not open its window",
        "The interface failed to start.",
        &detail,
    );
}

// ── the self test ─────────────────────────────────────────────────────────

/// `--selftest` and friends.
///
/// The point is being able to say to someone whose machine will not run this:
/// "start it with `--selftest` and send me what it writes". That path prints
/// the same block a crash report carries and exits, without a window, without
/// a WebView2, without needing the program to work at all.
///
/// The named variants take the real failure branches — including `crash`,
/// which executes an illegal instruction, so the vectored handler is tested
/// by an actual hardware fault rather than by a function that pretends to be
/// one.
///
/// Returns true when it handled the arguments and `main` should stop.
pub fn handle_selftest_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h" || a == "/?") {
        // One line per line, rather than one long literal with continuations:
        // the indentation of the source is not the indentation of the output,
        // and a help text that comes out ragged is its own small bug report.
        println!();
        println!("{} {}", APP, env!("CARGO_PKG_VERSION"));
        println!();
        println!("Drop audio files onto the window to convert them. The window is the whole");
        println!("interface; there are no other options on the command line.");
        println!();
        println!("Diagnostics:");
        println!("  --selftest             what this build needs, what this machine has,");
        println!("                         and where a report was written");
        println!("  --selftest=cpu         take the \"CPU too old\" path");
        println!("  --selftest=webview2    take the \"WebView2 missing\" path");
        println!("  --selftest=panic       take the panic path");
        println!("  --selftest=crash       execute an illegal instruction");
        println!();
        println!("If the program will not start on your machine, run it with --selftest and");
        println!("send the file it names. That is enough to tell us why.");
        println!();
        return true;
    }

    let Some(arg) = args.iter().find(|a| a.starts_with("--selftest")) else {
        return false;
    };
    let what = arg.split_once('=').map(|(_, v)| v).unwrap_or("");

    match what {
        "" => {
            let path = write_report("self test", "requested with --selftest; nothing failed");
            print!("
{}", environment());
            match path {
                Some(p) => println!("report       {}", p.display()),
                None => println!("report       could not be written anywhere"),
            }
            println!();
        }
        "cpu" => fail_with_remedy(
            "This CPU is too old for these binaries",
            "Self test: this is what a machine without AVX2 would show.",
            "self test: cpu",
            Remedy {
                ask: "Open the issue tracker now?",
                step: "ask for a baseline build at",
                url: ISSUES,
            },
        ),
        "webview2" => fail_with_remedy(
            "Microsoft Edge WebView2 is missing",
            "Self test: this is what a machine without WebView2 would show.",
            "self test: webview2",
            WEBVIEW2_REMEDY,
        ),
        "panic" => {
            // Straight through the hook installed in main.
            panic!("self test: deliberate panic");
        }
        "crash" => {
            println!("[STARTUP] self test: executing an illegal instruction");
            #[cfg(target_arch = "x86_64")]
            unsafe {
                std::arch::asm!("ud2", options(noreturn));
            }
            #[cfg(not(target_arch = "x86_64"))]
            unreachable!();
        }
        other => {
            println!("unknown self test: {} (try --help)", other);
        }
    }
    true
}
