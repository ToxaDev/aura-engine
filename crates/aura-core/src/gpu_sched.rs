//! Diagnostics for lost window frames while the card is shared (HL57): the
//! GPU scheduling priority class of the WebView2 processes the windows draw
//! in (`AURA_WV2_GPU_PRIO`) and of this process (`AURA_SELF_GPU_PRIO`), from
//! 0 (idle) to 4 (high). Unset, nothing happens.

/// Applies the classes asked for: this process's at once, the WebView2
/// processes' as they come up (looked for every 5 s for a minute).
pub fn start() {
    #[cfg(windows)]
    {
        let v = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<i32>().ok()).filter(|c| (0..=4).contains(c));
        let (wv2, own) = (v("AURA_WV2_GPU_PRIO"), v("AURA_SELF_GPU_PRIO"));
        if wv2.is_none() && own.is_none() {
            return;
        }
        let _ = std::thread::Builder::new().name("gpu-sched".into()).spawn(move || {
            use winapi::um::processthreadsapi::{GetCurrentProcess, GetCurrentProcessId};
            if let Some(c) = own {
                // SAFETY: no arguments; the pseudo handle of this process.
                let r = win::set(unsafe { GetCurrentProcess() }, c);
                crate::aelog!("[GPU-SCHED] this process: class {c} -> {r}");
            }
            let Some(c) = wv2 else { return };
            let mut done = std::collections::HashSet::new();
            for _ in 0..12 {
                // SAFETY: no arguments.
                let me = unsafe { GetCurrentProcessId() };
                for (pid, name) in win::descendants(me) {
                    if done.insert(pid) {
                        crate::aelog!("[GPU-SCHED] {name} {pid}: class {c} -> {}", win::set_pid(pid, c));
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
    }
}

#[cfg(windows)]
mod win {
    use winapi::shared::minwindef::DWORD;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryA};
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::tlhelp32::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    use winapi::um::winnt::{HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION};

    type SetFn = unsafe extern "system" fn(HANDLE, i32) -> i32;
    type GetFn = unsafe extern "system" fn(HANDLE, *mut i32) -> i32;

    fn fns() -> Option<(SetFn, GetFn)> {
        // SAFETY: gdi32 stays loaded for the life of the process; the two
        // exports have these signatures (d3dkmthk.h).
        unsafe {
            let m = LoadLibraryA(c"gdi32.dll".as_ptr());
            if m.is_null() {
                return None;
            }
            let s = GetProcAddress(m, c"D3DKMTSetProcessSchedulingPriorityClass".as_ptr());
            let g = GetProcAddress(m, c"D3DKMTGetProcessSchedulingPriorityClass".as_ptr());
            if s.is_null() || g.is_null() {
                return None;
            }
            Some((std::mem::transmute::<_, SetFn>(s), std::mem::transmute::<_, GetFn>(g)))
        }
    }

    /// Sets the class of process `h`; answers the status and the class read back.
    pub fn set(h: HANDLE, class: i32) -> String {
        let Some((s, g)) = fns() else { return "no D3DKMT exports".into() };
        let mut now = -1;
        // SAFETY: `h` is a process handle the caller holds open.
        let (st, gs) = unsafe { (s(h, class), g(h, &mut now)) };
        format!("status {st:#x}, now {now} (get {gs:#x})")
    }

    pub fn set_pid(pid: DWORD, class: i32) -> String {
        // SAFETY: the handle is checked and closed here.
        unsafe {
            let h = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_INFORMATION, 0, pid);
            if h.is_null() {
                return "OpenProcess failed".into();
            }
            let r = set(h, class);
            CloseHandle(h);
            r
        }
    }

    /// Every process below `root` (children, theirs, …), with its exe name.
    pub fn descendants(root: DWORD) -> Vec<(DWORD, String)> {
        let mut all: Vec<(DWORD, DWORD, String)> = Vec::new();
        // SAFETY: a snapshot walked with a sized entry, then closed.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return Vec::new();
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as DWORD;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                all.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        let mut out = Vec::new();
        let mut front = vec![root];
        while let Some(p) = front.pop() {
            for (pid, parent, name) in &all {
                if *parent == p && *pid != root && !out.iter().any(|(q, _)| q == pid) {
                    out.push((*pid, name.clone()));
                    front.push(*pid);
                }
            }
        }
        out
    }
}
