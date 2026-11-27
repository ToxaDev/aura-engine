//! The link on Windows: named objects in the session's namespace (`Local\`),
//! so a player and Aura Player run by the same user find the same ones.
//!
//! * `<base>` — the memory: header and ring (a paging-file mapping);
//! * `<base>.data`, `<base>.room` — the two auto-reset events.
//!
//! The memory lives while either end holds it: a player still connected
//! keeps it when Aura Player closes, and Aura Player started again makes its
//! server over it (`Server::create` keeps the client's place).

use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenProcess, SetEvent, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use crate::{region_bytes, Client, LinkError, Region, Server, Signals};

/// The objects' base name.
pub const BASE: &str = "AuraEngine.AudioLink.v1";
/// Aura Player's ring: 2^21 samples (16 MiB) — 1.4 s of stereo at 768 kHz,
/// far more than the target the server keeps.
pub const RING_SAMPLES: usize = 1 << 21;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn local(base: &str, suffix: &str) -> Vec<u16> {
    wide(&format!("Local\\{base}{suffix}"))
}

/// A mapped view of the named memory.
struct Mapping {
    handle: HANDLE,
    view: *mut u8,
    len: usize,
}

// SAFETY: the view stays mapped until drop; both ends only touch it through
// atomics.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

// SAFETY: the view is page-aligned and `len` bytes long.
unsafe impl Region for Mapping {
    fn as_ptr(&self) -> *mut u8 {
        self.view
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl Mapping {
    /// Maps all of `handle` (closing it when that fails).
    unsafe fn map(handle: HANDLE) -> Result<Mapping, LinkError> {
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0).Value as *mut u8;
        if view.is_null() {
            let e = GetLastError();
            CloseHandle(handle);
            return Err(LinkError::Os(e));
        }
        let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        let got = VirtualQuery(view as _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>());
        let len = if got == 0 { 0 } else { info.RegionSize };
        Ok(Mapping { handle, view, len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped and opened by `map`, released once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ });
            CloseHandle(self.handle);
        }
    }
}

/// The two named events.
struct Events {
    data: HANDLE,
    room: HANDLE,
}

// SAFETY: event handles are usable from any thread.
unsafe impl Send for Events {}
unsafe impl Sync for Events {}

impl Events {
    fn open(base: &str) -> Result<Events, LinkError> {
        // SAFETY: plain calls; a handle that came is closed on drop.
        unsafe {
            let data = CreateEventW(std::ptr::null(), 0, 0, local(base, ".data").as_ptr());
            if data.is_null() {
                return Err(LinkError::Os(GetLastError()));
            }
            let room = CreateEventW(std::ptr::null(), 0, 0, local(base, ".room").as_ptr());
            if room.is_null() {
                let e = GetLastError();
                CloseHandle(data);
                return Err(LinkError::Os(e));
            }
            Ok(Events { data, room })
        }
    }
}

fn wait(h: HANDLE, timeout: Duration) -> bool {
    let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
    // SAFETY: a live event handle.
    unsafe { WaitForSingleObject(h, ms) == WAIT_OBJECT_0 }
}

impl Signals for Events {
    fn notify_data(&self) {
        // SAFETY: a live event handle.
        unsafe { SetEvent(self.data) };
    }
    fn wait_data(&self, timeout: Duration) -> bool {
        wait(self.data, timeout)
    }
    fn notify_room(&self) {
        // SAFETY: a live event handle.
        unsafe { SetEvent(self.room) };
    }
    fn wait_room(&self, timeout: Duration) -> bool {
        wait(self.room, timeout)
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        // SAFETY: opened by `open`, closed once.
        unsafe {
            CloseHandle(self.data);
            CloseHandle(self.room);
        }
    }
}

/// Whether process `pid` still runs: one that cannot be opened at all is
/// gone; one that can but is signalled has ended; one this process may not
/// look at (another user's, a protected one) is taken to be alive.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: plain calls; the handle is closed.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let ended = WaitForSingleObject(h, 0) == WAIT_OBJECT_0;
        CloseHandle(h);
        !ended
    }
}

/// Aura Player's end under `base` (normally `BASE`), with a ring of
/// `ring_samples`. Over memory a player still holds from a server before,
/// when it is big enough.
pub fn create_server(base: &str, ring_samples: usize) -> Result<Server, LinkError> {
    let bytes = region_bytes(ring_samples) as u64;
    // SAFETY: plain calls; handles are owned by the mapping.
    let mapping = unsafe {
        let h = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            (bytes >> 32) as u32,
            bytes as u32,
            local(base, "").as_ptr(),
        );
        if h.is_null() {
            return Err(LinkError::Os(GetLastError()));
        }
        Mapping::map(h)?
    };
    if mapping.len() < bytes as usize {
        return Err(LinkError::Version(0));
    }
    let events = Events::open(base)?;
    // SAFETY: plain call.
    let pid = unsafe { GetCurrentProcessId() };
    Ok(Server::create(Box::new(mapping), Box::new(events), ring_samples, pid))
}

/// A player's end: connects to the server under `base`. `NoServer` when
/// Aura Player has not made the link.
pub fn open_client(base: &str) -> Result<Client, LinkError> {
    // SAFETY: plain calls; handles are owned by the mapping.
    let mapping = unsafe {
        let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, local(base, "").as_ptr());
        if h.is_null() {
            return Err(LinkError::NoServer);
        }
        Mapping::map(h)?
    };
    let events = Events::open(base)?;
    // SAFETY: plain call.
    let pid = unsafe { GetCurrentProcessId() };
    Client::open(Box::new(mapping), Box::new(events), pid, process_alive)
}
