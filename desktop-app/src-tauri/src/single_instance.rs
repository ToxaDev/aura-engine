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
//! so a crash cannot leave the app locked out.
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

#[cfg(not(windows))]
pub fn acquire() -> bool {
    true
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
