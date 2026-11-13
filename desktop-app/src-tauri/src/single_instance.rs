//! One copy of the app per machine.
//!
//! Two copies used to run side by side — some listeners started a second one
//! to convert a library a little faster. With the player inside, that stops
//! being harmless: both copies want the output device, and in exclusive mode
//! only one of them can have it. So the second copy does not start. It brings
//! the first one's window forward instead, which is what the person
//! double-clicking the icon was after anyway.
//!
//! The lock is a named mutex in the global namespace, so it holds across
//! Windows sessions as well: the output device belongs to the machine, not
//! to a session. Windows releases it when the process ends, however it ends,
//! so a crash cannot leave the app locked out. Linux and macOS have their own
//! lock and their own way to ask for the window, below.
//!
//! The windowless diagnostics (`--player-selftest`, `--probe-device`, the
//! self tests) run before this check and are not affected by it.

/// The window title the first copy is found by (tauri.conf.json).
#[cfg(windows)]
const WINDOW_TITLE: &str = "Aura Engine";

/// Take the machine-wide lock, or hand over to the copy that has it. Returns
/// false when this process should exit now.
#[cfg(windows)]
pub fn acquire() -> bool {
    use std::ptr::null_mut;
    use winapi::shared::winerror::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::synchapi::CreateMutexW;

    let name = wide("Global\\AuraEngine.SingleInstance");
    let handle = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };
    let err = unsafe { GetLastError() };
    // A mutex another user's copy created can refuse us access; that is the
    // same answer — something already holds it.
    let taken = (!handle.is_null() && err == ERROR_ALREADY_EXISTS) || (handle.is_null() && err == ERROR_ACCESS_DENIED);
    if handle.is_null() && !taken {
        // The lock itself could not be made. Not a reason to refuse to start.
        crate::aelog!("[APP] single-instance lock unavailable (error {}), starting anyway", err);
        return true;
    }
    if !taken {
        // Held for the life of the process: never closed, released by Windows
        // on exit.
        return true;
    }
    println!("Aura Engine is already running — switching to its window.");
    if !bring_forward() {
        message("Aura Engine is already running.\n\nOnly one copy can run at a time: the player needs the output device to itself.");
    }
    false
}

/// Nothing to listen for on Windows: the second copy finds the first one's
/// window by its title.
#[cfg(windows)]
pub fn serve(_app: tauri::AppHandle) {}

// Linux and macOS: an flock on a file in the user's runtime directory is the
// lock (the kernel drops it when the process ends, however it ends), and a
// Unix socket beside it is how the second copy asks the first one to come
// forward. Per user, not per machine: there is no global namespace to share,
// and the runtime directory is the user's own.
#[cfg(not(windows))]
mod unix {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    const LOCK_FILE: &str = "aura-engine.lock";
    const SOCKET_FILE: &str = "aura-engine.sock";
    const SHOW: &[u8] = b"show\n";

    pub(super) enum Taken {
        /// This is the first copy. The file holds the lock for as long as it
        /// is open; the listener is None when the socket could not be made.
        First(File, Option<UnixListener>),
        /// Another copy holds the lock.
        Running,
        /// The lock itself could not be made. Not a reason to refuse to start.
        Unavailable(std::io::Error),
    }

    /// $XDG_RUNTIME_DIR, else the temp directory under a per-user name (on
    /// macOS the temp directory is per user already).
    pub(super) fn runtime_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            let dir = PathBuf::from(dir);
            if dir.is_dir() {
                return dir;
            }
        }
        std::env::temp_dir().join(format!("aura-engine-{}", unsafe { libc::getuid() }))
    }

    pub(super) fn take(dir: &Path) -> Taken {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Taken::Unavailable(e);
        }
        let file = match OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(dir.join(LOCK_FILE)) {
            Ok(f) => f,
            Err(e) => return Taken::Unavailable(e),
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EWOULDBLOCK) { Taken::Running } else { Taken::Unavailable(e) };
        }
        // Whatever socket file is there was left by a copy that is gone: the
        // lock says no other copy is running.
        let socket = dir.join(SOCKET_FILE);
        let _ = std::fs::remove_file(&socket);
        let listener = match UnixListener::bind(&socket) {
            Ok(l) => Some(l),
            Err(e) => {
                crate::aelog!("[APP] single-instance socket unavailable ({}), a second copy will not raise this window", e);
                None
            }
        };
        Taken::First(file, listener)
    }

    /// Ask the running copy to come forward. False when it cannot be reached.
    pub(super) fn signal(dir: &Path) -> bool {
        UnixStream::connect(dir.join(SOCKET_FILE)).and_then(|mut s| s.write_all(SHOW)).is_ok()
    }

    /// Wait for the next request on the listener; true when it is "show".
    pub(super) fn next_request(listener: &UnixListener) -> std::io::Result<bool> {
        let (mut stream, _) = listener.accept()?;
        let mut buf = [0u8; 16];
        let mut len = 0;
        // A request is one short line; the sender closes after writing it.
        while len < buf.len() {
            match stream.read(&mut buf[len..]) {
                Ok(0) => break,
                Ok(n) => len += n,
                Err(_) => break,
            }
            if buf[..len].contains(&b'\n') {
                break;
            }
        }
        Ok(buf[..len].starts_with(SHOW))
    }
}

/// The lock file stays open for the life of the process; the listener waits
/// here until the window exists to raise.
#[cfg(not(windows))]
static HELD: std::sync::Mutex<Option<(std::fs::File, Option<std::os::unix::net::UnixListener>)>> = std::sync::Mutex::new(None);

#[cfg(not(windows))]
pub fn acquire() -> bool {
    let dir = unix::runtime_dir();
    match unix::take(&dir) {
        unix::Taken::First(file, listener) => {
            *HELD.lock().unwrap_or_else(|p| p.into_inner()) = Some((file, listener));
            true
        }
        unix::Taken::Unavailable(e) => {
            crate::aelog!("[APP] single-instance lock unavailable ({}), starting anyway", e);
            true
        }
        unix::Taken::Running => {
            if unix::signal(&dir) {
                println!("Aura Engine is already running — switching to its window.");
            } else {
                println!("Aura Engine is already running.\nOnly one copy can run at a time: the player needs the output device to itself.");
            }
            false
        }
    }
}

/// Raise the main window whenever a second copy asks. Called once the app is
/// set up; a request that came in before then waits in the socket's queue.
#[cfg(not(windows))]
pub fn serve(app: tauri::AppHandle) {
    use tauri::Manager;
    let listener = HELD.lock().unwrap_or_else(|p| p.into_inner()).as_mut().and_then(|(_, l)| l.take());
    let Some(listener) = listener else { return };
    let _ = std::thread::Builder::new().name("aura-single-instance".into()).spawn(move || loop {
        match unix::next_request(&listener) {
            Ok(true) => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            Ok(false) => {}
            Err(e) => {
                crate::aelog!("[APP] single-instance socket closed ({})", e);
                return;
            }
        }
    });
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::unix::{next_request, signal, take, Taken};

    #[test]
    fn second_copy_is_refused_while_the_first_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = take(dir.path());
        assert!(matches!(first, Taken::First(_, Some(_))));
        assert!(matches!(take(dir.path()), Taken::Running));
        drop(first);
        // The lock goes with the first copy's file, as it does when the
        // process ends.
        assert!(matches!(take(dir.path()), Taken::First(_, Some(_))));
    }

    #[test]
    fn a_socket_left_by_a_crash_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        // A socket file nobody listens on, as a crashed copy leaves it.
        drop(std::os::unix::net::UnixListener::bind(dir.path().join("aura-engine.sock")).unwrap());
        assert!(!signal(dir.path()));
        let Taken::First(_lock, Some(listener)) = take(dir.path()) else { panic!("first copy expected") };
        assert!(signal(dir.path()));
        assert!(next_request(&listener).unwrap());
    }

    #[test]
    fn only_show_raises_the_window() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let Taken::First(_lock, Some(listener)) = take(dir.path()) else { panic!("first copy expected") };
        std::os::unix::net::UnixStream::connect(dir.path().join("aura-engine.sock")).unwrap().write_all(b"hello\n").unwrap();
        assert!(!next_request(&listener).unwrap());
    }
}

/// Restore and raise the running copy's window. False when it is not found
/// (it may still be starting up).
#[cfg(windows)]
fn bring_forward() -> bool {
    use std::ptr::null;
    use winapi::um::winuser::{FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW};

    let title = wide(WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
    if hwnd.is_null() {
        return false;
    }
    unsafe {
        ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(hwnd);
    }
    true
}

#[cfg(windows)]
fn message(text: &str) {
    use std::ptr::null_mut;
    use winapi::um::winuser::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let text = wide(text);
    let caption = wide(WINDOW_TITLE);
    unsafe {
        MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
