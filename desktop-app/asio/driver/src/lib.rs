// SPDX-License-Identifier: GPL-3.0-only
//! Aura Player ASIO — an ASIO driver with no hardware behind it: what a
//! player (foobar2000, JRiver, HQPlayer, Roon…) plays into it goes to Aura
//! Player, through the link (`aura_link`), and on through Aura Player's
//! chain to its DAC.
//!
//! The DLL is an in-process COM server, as every ASIO driver is: a player
//! finds it under HKLM\SOFTWARE\ASIO, creates it by its CLSID and talks to
//! it through IASIO (`sys`, `driver`). `regsvr32` registers it
//! (`DllRegisterServer`): the 64-bit one for 64-bit players, the 32-bit
//! one (from SysWOW64) for 32-bit players.
//!
//! Copyright (c) 2026 ToxaDev. This driver is free software under the GNU
//! General Public License, version 3 (LICENSE); it is a separate program from
//! Aura Player, which it speaks to only through the link.

#![cfg(windows)]

pub mod driver;
pub mod sys;

use std::ffi::c_void;

use sys::{Guid, IClassFactoryVtbl, IID_ICLASSFACTORY, IID_IUNKNOWN};

/// {B5A6F72C-5A06-4800-A43C-AE23BAC4DE65}: the driver's class, for good.
pub const CLSID: Guid = Guid {
    data1: 0xB5A6_F72C,
    data2: 0x5A06,
    data3: 0x4800,
    data4: [0xA4, 0x3C, 0xAE, 0x23, 0xBA, 0xC4, 0xDE, 0x65],
};

/// The driver's source id behind its marker (`source_id.rs`, baked by the
/// build script): Aura Player reads it from the registered file to know its
/// own driver when the bytes differ but the sources do not. Exported, so no
/// linker drops it.
#[no_mangle]
pub static AURA_PLAYER_ASIO_SOURCE: [u8; SOURCE.len()] = bytes(SOURCE);

const SOURCE: &str = env!("AURA_ASIO_SOURCE_MARKER");

const fn bytes<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = s.as_bytes()[i];
        i += 1;
    }
    out
}

const S_OK: i32 = 0;
const S_FALSE: i32 = 1;
const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;
const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110_u32 as i32;
const CLASS_E_CLASSNOTAVAILABLE: i32 = 0x8004_0111_u32 as i32;
const SELFREG_E_CLASS: i32 = 0x8004_0201_u32 as i32;

/// Live objects and server locks: the DLL may go when both are none.
pub mod objects {
    use std::sync::atomic::{AtomicIsize, Ordering::*};

    static OBJECTS: AtomicIsize = AtomicIsize::new(0);
    static LOCKS: AtomicIsize = AtomicIsize::new(0);

    pub fn add() {
        OBJECTS.fetch_add(1, AcqRel);
    }
    pub fn remove() {
        OBJECTS.fetch_sub(1, AcqRel);
    }
    pub fn lock(on: bool) {
        LOCKS.fetch_add(if on { 1 } else { -1 }, AcqRel);
    }
    pub fn idle() -> bool {
        OBJECTS.load(Acquire) <= 0 && LOCKS.load(Acquire) <= 0
    }
}

/// What the driver asks of the system.
pub mod os {
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows_sys::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The system clock in nanoseconds since boot, as ASIO's time stamps are.
    pub fn now_ns() -> u64 {
        let (mut c, mut f) = (0i64, 0i64);
        // SAFETY: plain calls into valid locals.
        unsafe {
            QueryPerformanceCounter(&mut c);
            QueryPerformanceFrequency(&mut f);
        }
        if f <= 0 {
            return 0;
        }
        (c as i128 * 1_000_000_000 / f as i128) as u64
    }

    /// The player's name: its program's file name, without `.exe`.
    pub fn host_name() -> String {
        let mut buf = [0u16; 1024];
        // SAFETY: the buffer's length is passed.
        let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let path = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or("");
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    }

    pub fn message_box(title: &str, text: &str) {
        // SAFETY: null-terminated strings that live through the call.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), MB_OK | MB_ICONINFORMATION);
        }
    }

    /// The thread as an audio thread (MMCSS "Pro Audio") while this lives.
    pub struct ProAudio(windows_sys::Win32::Foundation::HANDLE);

    pub fn pro_audio_thread() -> ProAudio {
        let mut task = 0u32;
        // SAFETY: a null-terminated name and a valid local.
        ProAudio(unsafe { AvSetMmThreadCharacteristicsW(wide("Pro Audio").as_ptr(), &mut task) })
    }

    impl Drop for ProAudio {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle MMCSS gave this thread.
                unsafe { AvRevertMmThreadCharacteristics(self.0) };
            }
        }
    }

    /// This DLL's own path.
    pub fn module_path() -> Option<String> {
        use windows_sys::Win32::System::LibraryLoader::{
            GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let mut module = std::ptr::null_mut();
        let mut buf = [0u16; 1024];
        // SAFETY: an address inside this DLL; the buffer's length is passed.
        unsafe {
            let ok = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                module_path as *const () as *const u16,
                &mut module,
            );
            if ok == 0 {
                return None;
            }
            let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
            (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
        }
    }
}

/// The registry entries a player looks for.
mod registry {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE,
        KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    use crate::driver::DRIVER_NAME;
    use crate::CLSID;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn set(root: HKEY, path: &str, values: &[(Option<&str>, &str)]) -> bool {
        let mut key = std::ptr::null_mut();
        // SAFETY: null-terminated strings, valid out-pointers; the key is closed.
        unsafe {
            if RegCreateKeyExW(
                root,
                wide(path).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            ) != 0
            {
                return false;
            }
            let mut ok = true;
            for (name, value) in values {
                let n = name.map(wide);
                let v = wide(value);
                ok &= RegSetValueExW(
                    key,
                    n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                    0,
                    REG_SZ,
                    v.as_ptr() as *const u8,
                    (v.len() * 2) as u32,
                ) == 0;
            }
            RegCloseKey(key);
            ok
        }
    }

    fn clsid_path() -> String {
        format!("CLSID\\{}", CLSID.braced())
    }

    fn asio_path() -> String {
        format!("SOFTWARE\\ASIO\\{DRIVER_NAME}")
    }

    pub fn register(dll: &str) -> bool {
        let clsid = CLSID.braced();
        set(HKEY_CLASSES_ROOT, &clsid_path(), &[(None, DRIVER_NAME)])
            && set(HKEY_CLASSES_ROOT, &format!("{}\\InprocServer32", clsid_path()), &[(None, dll), (Some("ThreadingModel"), "Apartment")])
            && set(HKEY_LOCAL_MACHINE, &asio_path(), &[(Some("CLSID"), &clsid), (Some("Description"), DRIVER_NAME)])
    }

    pub fn unregister() -> bool {
        // SAFETY: null-terminated strings; a key already gone is no error.
        unsafe {
            let a = RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(&asio_path()).as_ptr());
            let c = RegDeleteTreeW(HKEY_CLASSES_ROOT, wide(&clsid_path()).as_ptr());
            const NOT_FOUND: u32 = 2;
            (a == 0 || a == NOT_FOUND) && (c == 0 || c == NOT_FOUND)
        }
    }
}

/// The class factory: one, static, never freed.
#[repr(C)]
struct Factory {
    vtbl: &'static IClassFactoryVtbl,
}

static FACTORY_VTBL: IClassFactoryVtbl =
    IClassFactoryVtbl { query_interface: factory_qi, add_ref: factory_add_ref, release: factory_release, create_instance, lock_server };
static FACTORY: Factory = Factory { vtbl: &FACTORY_VTBL };

unsafe extern "system" fn factory_qi(this: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    if *iid == IID_IUNKNOWN || *iid == IID_ICLASSFACTORY {
        *out = this;
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn factory_add_ref(_this: *mut c_void) -> u32 {
    2
}

unsafe extern "system" fn factory_release(_this: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn create_instance(_this: *mut c_void, outer: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let d = driver::Driver::new();
    let vtbl = *(d as *const *const sys::IAsioVtbl);
    let hr = ((*vtbl).query_interface)(d, iid, out);
    // The object's own reference goes: the caller holds the one QI gave.
    ((*vtbl).release)(d);
    hr
}

unsafe extern "system" fn lock_server(_this: *mut c_void, lock: i32) -> i32 {
    objects::lock(lock != 0);
    S_OK
}

/// COM asks for the class factory.
///
/// # Safety
/// COM's contract: valid pointers in, an interface pointer out.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const Guid, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    if out.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if *clsid != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    factory_qi(&FACTORY as *const Factory as *mut c_void, iid, out)
}

/// Whether COM may unload the DLL.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    if objects::idle() {
        S_OK
    } else {
        S_FALSE
    }
}

/// `regsvr32`: the driver's class and its ASIO entry (needs an elevated
/// caller: both are under HKEY_LOCAL_MACHINE).
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> i32 {
    match os::module_path() {
        Some(p) if registry::register(&p) => S_OK,
        _ => SELFREG_E_CLASS,
    }
}

/// `regsvr32 /u`.
#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> i32 {
    if registry::unregister() {
        S_OK
    } else {
        SELFREG_E_CLASS
    }
}
