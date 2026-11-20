// SPDX-License-Identifier: GPL-3.0-only
//! The ASIO interface (ASIO SDK 2.3.4, asio.h and iasiodrv.h), as Rust: the
//! types a host and a driver pass each other, and the driver's COM object.
//!
//! On Windows the SDK's `long` is 32 bits on x86 and x64, a sample position
//! or a time stamp is two of them (hi, lo), a sample rate is a double. The
//! driver's own methods are plain C++ virtual functions — `thiscall` on x86
//! (the `this` in ECX), the one convention there is on x64 — while its
//! IUnknown methods are COM's `stdcall`. `asio_method!` gives a function the
//! convention its target needs. asio.h packs its structs to 4 bytes under
//! MSVC: only the time structs feel it (no padding after a time code).

use std::ffi::c_void;

pub type AsioBool = i32;
pub type AsioError = i32;
pub type AsioSampleRate = f64;
pub type AsioSampleType = i32;

pub const ASIO_FALSE: AsioBool = 0;
pub const ASIO_TRUE: AsioBool = 1;

pub const ASE_OK: AsioError = 0;
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_SP_NOT_ADVANCING: AsioError = -996;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

pub const ASIOST_INT16_LSB: AsioSampleType = 16;
pub const ASIOST_INT24_LSB: AsioSampleType = 17;
pub const ASIOST_INT32_LSB: AsioSampleType = 18;
pub const ASIOST_FLOAT32_LSB: AsioSampleType = 19;
pub const ASIOST_FLOAT64_LSB: AsioSampleType = 20;

// asioMessage selectors (host side).
pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
pub const K_ASIO_RESET_REQUEST: i32 = 3;
pub const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
pub const K_ASIO_RESYNC_REQUEST: i32 = 5;
pub const K_ASIO_LATENCIES_CHANGED: i32 = 6;
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;
pub const K_ASIO_SUPPORTS_TIME_CODE: i32 = 8;

// future() selectors (driver side).
pub const K_ASIO_CAN_TIME_INFO: i32 = 10;
pub const K_ASIO_CAN_TIME_CODE: i32 = 11;
pub const K_ASIO_SET_IO_FORMAT: i32 = 0x2311_1961;
pub const K_ASIO_GET_IO_FORMAT: i32 = 0x2311_1983;
pub const K_ASIO_CAN_DO_IO_FORMAT: i32 = 0x2311_2004;
/// ASIOIoFormat's type: PCM (DSD is 1, which this driver does not take).
pub const K_ASIO_PCM_FORMAT: i32 = 0;

// AsioTimeInfo flags.
pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 1 << 1;
pub const K_SAMPLE_RATE_VALID: u32 = 1 << 2;
pub const K_SPEED_VALID: u32 = 1 << 3;
pub const K_SAMPLE_RATE_CHANGED: u32 = 1 << 4;

/// A 64-bit count as the SDK has it on Windows: high, then low.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Asio64 {
    pub hi: u32,
    pub lo: u32,
}

impl From<u64> for Asio64 {
    fn from(v: u64) -> Self {
        Asio64 { hi: (v >> 32) as u32, lo: v as u32 }
    }
}

impl From<Asio64> for u64 {
    fn from(v: Asio64) -> u64 {
        (v.hi as u64) << 32 | v.lo as u64
    }
}

pub type AsioSamples = Asio64;
pub type AsioTimeStamp = Asio64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: AsioBool,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: i32,
    pub sample_type: AsioSampleType,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    pub is_input: AsioBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub system_time: AsioTimeStamp,
    pub sample_position: AsioSamples,
    pub sample_rate: AsioSampleRate,
    pub flags: u32,
    pub reserved: [u8; 12],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioSamples,
    pub flags: u32,
    pub future: [u8; 64],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

impl Default for AsioTime {
    fn default() -> Self {
        AsioTime {
            reserved: [0; 4],
            time_info: AsioTimeInfo::default(),
            time_code: AsioTimeCode { speed: 0.0, time_code_samples: Asio64::default(), flags: 0, future: [0; 64] },
        }
    }
}

/// The host's callbacks: plain C functions (any of which a careless host
/// may leave null).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    pub buffer_switch: Option<unsafe extern "C" fn(double_buffer_index: i32, direct_process: AsioBool)>,
    pub sample_rate_did_change: Option<unsafe extern "C" fn(rate: AsioSampleRate)>,
    pub asio_message: Option<unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32>,
    pub buffer_switch_time_info:
        Option<unsafe extern "C" fn(params: *mut AsioTime, double_buffer_index: i32, direct_process: AsioBool) -> *mut AsioTime>,
}

/// ASIOIoFormat: what kAsioGetIoFormat and its kin pass.
#[repr(C)]
pub struct AsioIoFormat {
    pub format_type: i32,
    pub future: [u8; 512 - 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}", as the registry has it.
    pub fn braced(&self) -> String {
        let d = &self.data4;
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

pub const IID_IUNKNOWN: Guid =
    Guid { data1: 0x0000_0000, data2: 0x0000, data3: 0x0000, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
pub const IID_ICLASSFACTORY: Guid =
    Guid { data1: 0x0000_0001, data2: 0x0000, data3: 0x0000, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

/// A method of the driver's own: `thiscall` on x86, the platform's one
/// convention elsewhere.
#[macro_export]
macro_rules! asio_method {
    ($(#[$m:meta])* $vis:vis fn $name:ident($this:ident: $tt:ty $(, $a:ident: $t:ty)* $(,)?) $(-> $r:ty)? $body:block) => {
        $(#[$m])*
        #[cfg(target_arch = "x86")]
        $vis unsafe extern "thiscall" fn $name($this: $tt $(, $a: $t)*) $(-> $r)? $body
        $(#[$m])*
        #[cfg(not(target_arch = "x86"))]
        $vis unsafe extern "system" fn $name($this: $tt $(, $a: $t)*) $(-> $r)? $body
    };
}

/// A field of the driver's vtable with the same conventions.
macro_rules! vtable {
    ($($name:ident: fn($($t:ty),*) $(-> $r:ty)?;)*) => {
        /// IASIO's vtable: IUnknown's three, then the driver's own, in the
        /// SDK's order.
        #[repr(C)]
        pub struct IAsioVtbl {
            pub query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
            pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
            pub release: unsafe extern "system" fn(*mut c_void) -> u32,
            $(
                #[cfg(target_arch = "x86")]
                pub $name: unsafe extern "thiscall" fn(*mut c_void $(, $t)*) $(-> $r)?,
                #[cfg(not(target_arch = "x86"))]
                pub $name: unsafe extern "system" fn(*mut c_void $(, $t)*) $(-> $r)?,
            )*
        }
    };
}

vtable! {
    init: fn(*mut c_void) -> AsioBool;
    get_driver_name: fn(*mut u8);
    get_driver_version: fn() -> i32;
    get_error_message: fn(*mut u8);
    start: fn() -> AsioError;
    stop: fn() -> AsioError;
    get_channels: fn(*mut i32, *mut i32) -> AsioError;
    get_latencies: fn(*mut i32, *mut i32) -> AsioError;
    get_buffer_size: fn(*mut i32, *mut i32, *mut i32, *mut i32) -> AsioError;
    can_sample_rate: fn(AsioSampleRate) -> AsioError;
    get_sample_rate: fn(*mut AsioSampleRate) -> AsioError;
    set_sample_rate: fn(AsioSampleRate) -> AsioError;
    get_clock_sources: fn(*mut AsioClockSource, *mut i32) -> AsioError;
    set_clock_source: fn(i32) -> AsioError;
    get_sample_position: fn(*mut AsioSamples, *mut AsioTimeStamp) -> AsioError;
    get_channel_info: fn(*mut AsioChannelInfo) -> AsioError;
    create_buffers: fn(*mut AsioBufferInfo, i32, i32, *mut AsioCallbacks) -> AsioError;
    dispose_buffers: fn() -> AsioError;
    control_panel: fn() -> AsioError;
    future: fn(i32, *mut c_void) -> AsioError;
    output_ready: fn() -> AsioError;
}

/// IClassFactory's vtable (all COM `stdcall`).
#[repr(C)]
pub struct IClassFactoryVtbl {
    pub query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub create_instance: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32,
    pub lock_server: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

/// Copies `s` into a C string buffer of `cap` bytes, cut to fit.
///
/// # Safety
/// `dst` points at `cap` writable bytes.
pub unsafe fn put_c_str(dst: *mut u8, cap: usize, s: &str) {
    if dst.is_null() || cap == 0 {
        return;
    }
    let n = s.len().min(cap - 1);
    std::ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
    *dst.add(n) = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn the_structs_have_the_sdk_s_sizes() {
        assert_eq!(size_of::<Asio64>(), 8);
        assert_eq!(size_of::<AsioClockSource>(), 48);
        assert_eq!(size_of::<AsioChannelInfo>(), 52);
        assert_eq!(size_of::<AsioBufferInfo>(), 8 + 2 * size_of::<usize>());
        assert_eq!(size_of::<AsioTimeInfo>(), 48);
        assert_eq!(size_of::<AsioTimeCode>(), 84);
        assert_eq!(size_of::<AsioTime>(), 148);
        assert_eq!(size_of::<AsioCallbacks>(), 4 * size_of::<usize>());
        assert_eq!(size_of::<IAsioVtbl>(), 24 * size_of::<usize>());
    }

    #[test]
    fn a_guid_reads_as_the_registry_writes_it() {
        assert_eq!(crate::CLSID.braced(), "{B5A6F72C-5A06-4800-A43C-AE23BAC4DE65}");
    }

    #[test]
    fn a_c_string_is_cut_to_its_buffer() {
        let mut b = [0xffu8; 6];
        unsafe { put_c_str(b.as_mut_ptr(), b.len(), "Aura Player") };
        assert_eq!(&b, b"Aura \0");
    }
}
