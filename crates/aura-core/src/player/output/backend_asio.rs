// SPDX-License-Identifier: GPL-3.0-only
//! ASIO as the output's backend: the DAC's own ASIO driver, PCM at the
//! stream's rate and DSD natively (the driver's DSD mode, one-bit samples
//! at the DSD rate) — or, for a driver without a DSD mode, DoP over its PCM
//! stream as on WASAPI.
//!
//! The interface is ASIO SDK 2.3.4's, written out in Rust as the input
//! driver has it (`desktop-app/asio/driver/src/sys.rs`); nothing of the SDK
//! is linked. The order of calls for native DSD is the one a Chord Mojo 2
//! played with (18.11.2026): `future(kAsioSetIoFormat, DSD)`, then the
//! sample rate in one-bit samples a second (2 822 400 for DSD64), the
//! buffer size (in one-bit samples: N / 8 bytes a channel), the buffers,
//! start. A driver's `canSampleRate` cannot be trusted (Chord's says yes
//! to everything): only `setSampleRate` and a running stream tell.
//!
//! The driver calls back on its own thread with no context, so the stream
//! it serves is a global (`HOST`): one ASIO stream at a time, as the
//! player never has two. The callback only copies a half-buffer out of the
//! ring (`asio_logic::Ring`); the audio thread fills the ring with the
//! periods the render loop makes (`Periods`), exactly as for WASAPI.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::asio_logic::*;
use super::*;

// ── The ASIO interface ───────────────────────────────────────────────────

type AsioBool = i32;
type AsioError = i32;

const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
const K_ASIO_ENGINE_VERSION: i32 = 2;
const K_ASIO_RESET_REQUEST: i32 = 3;
const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
const K_ASIO_RESYNC_REQUEST: i32 = 5;
const K_ASIO_LATENCIES_CHANGED: i32 = 6;
const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;

const K_ASIO_SET_IO_FORMAT: i32 = 0x2311_1961;
const K_ASIO_CAN_DO_IO_FORMAT: i32 = 0x2311_2004;
const K_ASIO_PCM_FORMAT: i32 = 0;
const K_ASIO_DSD_FORMAT: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Aura Player ASIO, the app's own input driver: never an output (the
/// player would play into itself).
const AURA_INPUT_CLSID: &str = "{B5A6F72C-5A06-4800-A43C-AE23BAC4DE65}";

#[repr(C)]
#[derive(Clone, Copy)]
struct AsioChannelInfo {
    channel: i32,
    is_input: AsioBool,
    is_active: AsioBool,
    channel_group: i32,
    sample_type: i32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AsioBufferInfo {
    is_input: AsioBool,
    channel_num: i32,
    buffers: [*mut c_void; 2],
}

#[repr(C)]
struct AsioIoFormat {
    format_type: i32,
    future: [u8; 512 - 4],
}

impl AsioIoFormat {
    fn new(t: i32) -> Self {
        AsioIoFormat { format_type: t, future: [0; 508] }
    }
}

#[repr(C)]
struct AsioCallbacks {
    buffer_switch: unsafe extern "C" fn(i32, AsioBool),
    sample_rate_did_change: unsafe extern "C" fn(f64),
    asio_message: unsafe extern "C" fn(i32, i32, *mut c_void, *mut f64) -> i32,
    buffer_switch_time_info: unsafe extern "C" fn(*mut c_void, i32, AsioBool) -> *mut c_void,
}

/// IASIO's vtable: IUnknown's three, then the driver's own in the SDK's
/// order — `thiscall` on x86, the platform's one convention elsewhere.
macro_rules! vtable {
    ($($name:ident: fn($($t:ty),*) $(-> $r:ty)?;)*) => {
        #[repr(C)]
        struct IAsioVtbl {
            _query_interface: unsafe extern "system" fn(),
            _add_ref: unsafe extern "system" fn(),
            release: unsafe extern "system" fn(*mut c_void) -> u32,
            $(
                #[cfg(target_arch = "x86")]
                $name: unsafe extern "thiscall" fn(*mut c_void $(, $t)*) $(-> $r)?,
                #[cfg(not(target_arch = "x86"))]
                $name: unsafe extern "system" fn(*mut c_void $(, $t)*) $(-> $r)?,
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
    can_sample_rate: fn(f64) -> AsioError;
    get_sample_rate: fn(*mut f64) -> AsioError;
    set_sample_rate: fn(f64) -> AsioError;
    get_clock_sources: fn(*mut c_void, *mut i32) -> AsioError;
    set_clock_source: fn(i32) -> AsioError;
    get_sample_position: fn(*mut [u32; 2], *mut [u32; 2]) -> AsioError;
    get_channel_info: fn(*mut AsioChannelInfo) -> AsioError;
    create_buffers: fn(*mut AsioBufferInfo, i32, i32, *mut AsioCallbacks) -> AsioError;
    dispose_buffers: fn() -> AsioError;
    control_panel: fn() -> AsioError;
    future: fn(i32, *mut c_void) -> AsioError;
    output_ready: fn() -> AsioError;
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(clsid: *const Guid, outer: *mut c_void, ctx: u32, iid: *const Guid, out: *mut *mut c_void) -> i32;
    fn CLSIDFromString(s: *const u16, out: *mut Guid) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(key: isize, sub: *const u16, opts: u32, sam: u32, out: *mut isize) -> i32;
    fn RegEnumKeyExW(
        key: isize,
        idx: u32,
        name: *mut u16,
        len: *mut u32,
        reserved: *mut u32,
        class: *mut u16,
        class_len: *mut u32,
        ft: *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(key: isize, name: *const u16, reserved: *mut u32, ty: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn GetDesktopWindow() -> *mut c_void;
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
const KEY_READ: u32 = 0x2_0019;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const CLSCTX_INPROC_SERVER: u32 = 0x1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

fn c_str(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

fn ok(e: AsioError) -> bool {
    e == ASE_OK || e == ASE_SUCCESS
}

// ── The drivers installed ────────────────────────────────────────────────

struct DriverEntry {
    /// The registry key's name: what the driver is listed and found by.
    key: String,
    clsid: String,
}

/// The ASIO drivers under HKLM\SOFTWARE\ASIO, the app's own input driver
/// left out. Nothing is loaded.
fn installed() -> Vec<DriverEntry> {
    let mut out = Vec::new();
    unsafe {
        let mut root = 0isize;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide("SOFTWARE\\ASIO").as_ptr(), 0, KEY_READ, &mut root) != 0 {
            return out;
        }
        let mut i = 0;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let r = RegEnumKeyExW(
                root,
                i,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != 0 {
                break;
            }
            i += 1;
            let key = from_wide(&name);
            let mut sub = 0isize;
            if RegOpenKeyExW(root, name.as_ptr(), 0, KEY_READ, &mut sub) != 0 {
                continue;
            }
            let mut buf = [0u16; 128];
            let mut blen = (buf.len() * 2) as u32;
            let mut ty = 0u32;
            let got = RegQueryValueExW(sub, wide("CLSID").as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut blen);
            RegCloseKey(sub);
            if got != 0 {
                continue;
            }
            let clsid = from_wide(&buf);
            if clsid.eq_ignore_ascii_case(AURA_INPUT_CLSID) {
                continue;
            }
            out.push(DriverEntry { key, clsid });
        }
        RegCloseKey(root);
    }
    out
}

/// The ASIO drivers as output devices: `asio:<key>`, "ASIO: <key>".
pub fn list_devices() -> Vec<DeviceInfo> {
    installed()
        .into_iter()
        .map(|d| DeviceInfo { id: format!("{ASIO_ID_PREFIX}{}", d.key), name: format!("ASIO: {}", d.key), is_default: false })
        .collect()
}

// ── The driver, loaded ───────────────────────────────────────────────────

/// A loaded driver (COM object), on the thread that loaded it.
struct Driver {
    p: *mut c_void,
    v: *const IAsioVtbl,
    name: String,
    /// COM was initialised here for it (to be undone after the release).
    com: bool,
}

impl Driver {
    fn vt(&self) -> &IAsioVtbl {
        unsafe { &*self.v }
    }

    fn load(key: &str) -> Result<Driver, String> {
        let entry = installed()
            .into_iter()
            .find(|d| d.key == key)
            .ok_or_else(|| format!("ASIO: no driver named \"{key}\" is installed (the DAC's driver removed?)"))?;
        unsafe {
            let hr = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
            let com = hr >= 0; // S_OK or S_FALSE: to be balanced
            let fail = |e: String| {
                if com {
                    CoUninitialize();
                }
                Err(e)
            };
            let mut clsid = Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] };
            if CLSIDFromString(wide(&entry.clsid).as_ptr(), &mut clsid) != 0 {
                return fail(format!("ASIO: {key}: its CLSID in the registry is not valid ({})", entry.clsid));
            }
            let mut p: *mut c_void = std::ptr::null_mut();
            // ASIO's quirk: the driver's CLSID is also the interface id.
            let hr = CoCreateInstance(&clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &clsid, &mut p);
            if hr != 0 || p.is_null() {
                return fail(format!("ASIO: {key}: the driver could not be loaded (HRESULT 0x{:08X})", hr as u32));
            }
            let v = *(p as *mut *const IAsioVtbl);
            let d = Driver { p, v, name: key.to_string(), com };
            if (d.vt().init)(p, GetDesktopWindow()) == 0 {
                let says = d.message();
                drop(d);
                return Err(start_error(key, "init", ASE_NOT_PRESENT, &says));
            }
            Ok(d)
        }
    }

    fn message(&self) -> String {
        let mut m = [0u8; 256];
        unsafe { (self.vt().get_error_message)(self.p, m.as_mut_ptr()) };
        c_str(&m)
    }

    fn io_format(&self, sel: i32, t: i32) -> AsioError {
        let mut f = AsioIoFormat::new(t);
        unsafe { (self.vt().future)(self.p, sel, &mut f as *mut _ as *mut c_void) }
    }

    fn rate(&self) -> f64 {
        let mut r = 0f64;
        unsafe { (self.vt().get_sample_rate)(self.p, &mut r) };
        r
    }

    /// The rate set (to `rate`, if it is not already), as the driver reads
    /// it back; the error in the listener's words.
    fn set_rate(&self, rate: f64, what: &str) -> Result<(), String> {
        if (self.rate() - rate).abs() > 0.5 {
            let e = unsafe { (self.vt().set_sample_rate)(self.p, rate) };
            if !ok(e) {
                return Err(format!(
                    "ASIO: {} does not take {what} ({}: \"{}\")",
                    self.name,
                    error_name(e),
                    self.message()
                ));
            }
        }
        let now = self.rate();
        if (now - rate).abs() > 0.5 {
            return Err(format!("ASIO: {} was set to {what} but runs at {now} Hz", self.name));
        }
        Ok(())
    }

    fn out_type(&self) -> i32 {
        let mut ci = AsioChannelInfo { channel: 0, is_input: 0, is_active: 0, channel_group: 0, sample_type: -1, name: [0; 32] };
        unsafe { (self.vt().get_channel_info)(self.p, &mut ci) };
        ci.sample_type
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        unsafe {
            (self.vt().release)(self.p);
            if self.com {
                CoUninitialize();
            }
        }
    }
}

// ── The callback's side ──────────────────────────────────────────────────

/// What the driver's callback serves: set once the buffers exist, taken
/// away (and waited out, `BUSY`) before they go.
struct Host {
    ring: Ring,
    layout: Layout,
    /// The driver's two halves, each channel's.
    bufs: [[*mut u8; 2]; 2],
    half: usize,
    /// The audio thread's period (`chunk_and_ring`): it is woken when the
    /// ring has room for one, not at each of the driver's halves.
    chunk: usize,
    /// DoP over PCM: the next marker of a dry ring's silence.
    phase: AtomicUsize,
    callbacks: AtomicU64,
    last_ns: AtomicU64,
    dry: AtomicU64,
    /// The render loop has started: a dry ring from now on is counted.
    armed: AtomicBool,
    reset: AtomicBool,
    audio_thread: std::thread::Thread,
    driver: (*mut c_void, *const IAsioVtbl),
    output_ready: bool,
}

static HOST: AtomicPtr<Host> = AtomicPtr::new(std::ptr::null_mut());
/// Callbacks inside `HOST` now.
static BUSY: AtomicUsize = AtomicUsize::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

unsafe fn fill(h: &Host, index: i32) {
    let half = (index & 1) as usize;
    let (l, r) = (h.bufs[0][half], h.bufs[1][half]);
    let unit = h.layout.unit();
    let n = h.ring.pop(h.half, l, r);
    let mut phase = h.phase.load(Ordering::Relaxed);
    if n > 0 {
        phase = h.layout.phase_after(std::slice::from_raw_parts(l, n * unit), phase);
    }
    if n < h.half {
        let rest = (h.half - n) * unit;
        let p = h.layout.idle(std::slice::from_raw_parts_mut(l.add(n * unit), rest), phase);
        h.layout.idle(std::slice::from_raw_parts_mut(r.add(n * unit), rest), phase);
        phase = p;
        if h.armed.load(Ordering::Relaxed) {
            h.dry.fetch_add(1, Ordering::Relaxed);
        }
    }
    h.phase.store(phase, Ordering::Relaxed);
}

unsafe extern "C" fn buffer_switch(index: i32, _direct: AsioBool) {
    BUSY.fetch_add(1, Ordering::AcqRel);
    let p = HOST.load(Ordering::Acquire);
    if let Some(h) = p.as_ref() {
        fill(h, index);
        if h.output_ready {
            let (d, v) = h.driver;
            ((*v).output_ready)(d);
        }
        h.callbacks.fetch_add(1, Ordering::Relaxed);
        h.last_ns.store(now_ns(), Ordering::Relaxed);
        // A driver at 64 samples calls ~3000 times a second: the audio
        // thread is woken only when it has a period to write.
        if h.ring.free() >= h.chunk {
            h.audio_thread.unpark();
        }
    }
    BUSY.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn buffer_switch_time_info(params: *mut c_void, index: i32, direct: AsioBool) -> *mut c_void {
    buffer_switch(index, direct);
    params
}

unsafe extern "C" fn sample_rate_did_change(_rate: f64) {
    // The driver's clock moved under the stream (a word clock, its panel):
    // opened again at the rate asked for.
    request_reset();
}

unsafe fn request_reset() {
    BUSY.fetch_add(1, Ordering::AcqRel);
    if let Some(h) = HOST.load(Ordering::Acquire).as_ref() {
        h.reset.store(true, Ordering::Release);
        h.audio_thread.unpark();
    }
    BUSY.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn asio_message(selector: i32, value: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => matches!(
            value,
            K_ASIO_ENGINE_VERSION
                | K_ASIO_RESET_REQUEST
                | K_ASIO_BUFFER_SIZE_CHANGE
                | K_ASIO_RESYNC_REQUEST
                | K_ASIO_LATENCIES_CHANGED
                | K_ASIO_SUPPORTS_TIME_INFO
        ) as i32,
        K_ASIO_ENGINE_VERSION => 2,
        K_ASIO_RESET_REQUEST | K_ASIO_BUFFER_SIZE_CHANGE => {
            request_reset();
            1
        }
        K_ASIO_RESYNC_REQUEST | K_ASIO_LATENCIES_CHANGED | K_ASIO_SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

// ── The backend ──────────────────────────────────────────────────────────

/// ASIO as the output's backend (`super::Backend`).
pub(super) struct AsioOut;

/// How long a started driver may take to ask for its first buffer, and
/// later go without asking, before the stream is given up.
const FIRST_CALL_WAIT: Duration = Duration::from_millis(1500);
const STALL: Duration = Duration::from_millis(2000);

impl super::Backend for AsioOut {
    type Stream = AsioStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        Ok(list_devices())
    }

    fn probe_formats(_device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        // A driver's canSampleRate cannot be trusted: nothing to say
        // without opening it.
        Ok(rates.iter().map(|&r| (r, "unknown (ASIO)".to_string())).collect())
    }

    fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        let key = asio_driver_of(device_id).ok_or("not an ASIO device")?;
        let d = Driver::load(key)?;
        let mut s = format!("ASIO driver {key}\n");
        let (mut ins, mut outs) = (0, 0);
        unsafe { (d.vt().get_channels)(d.p, &mut ins, &mut outs) };
        s += &format!("  channels: {ins} in, {outs} out; rate now {} Hz; output type {}\n", d.rate(), sample_type_name(d.out_type()));
        let dsd = d.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_DSD_FORMAT);
        s += &format!("  native DSD (kAsioCanDoIoFormat): {}\n", if ok(dsd) { "yes".into() } else { error_name(dsd) });
        for &r in rates {
            let e = unsafe { (d.vt().can_sample_rate)(d.p, r as f64) };
            s += &format!("  {r} Hz: {}\n", if ok(e) { "says yes (only a stream tells)".into() } else { error_name(e) });
        }
        Ok(s)
    }

    fn open(cfg: &OutputConfig) -> Result<Opened<AsioStream>, String> {
        let key = asio_driver_of(cfg.device_id.as_deref()).ok_or("not an ASIO device")?;
        let _one = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        OPENS.fetch_add(1, Ordering::SeqCst);
        let parked = PARKED.lock().unwrap_or_else(|e| e.into_inner()).take();
        match &parked {
            Some(p) => crate::aelog!(
                "[PLAYER] ASIO open {key} {} {}: a parked driver is there ({} {} {})",
                cfg.rate,
                mode_label(&cfg.mode),
                p.driver.name,
                p.rate,
                p.mode
            ),
            None => crate::aelog!("[PLAYER] ASIO open {key} {}: no parked driver", cfg.rate),
        }
        if let Some(p) = parked {
            if p.driver.name == key && p.rate == cfg.rate && p.mode == mode_label(&cfg.mode) {
                return take_up(p, cfg);
            }
            // Another rate, format or driver: this one stops; back in PCM
            // only when the next stream is another driver's.
            let other = p.driver.name != key;
            shut(p, other);
        }
        let driver = Driver::load(key)?;
        open_on(driver, cfg)
    }
}

/// A native DSD stream's driver left running between two streams, playing
/// DSD silence: the next stream of the same driver, rate and format takes
/// it up as it runs. Stopping the driver and starting it again clicked on
/// a Chord Mojo 2 at DSD256 between two tracks, the DAC staying in DSD
/// (Anton 18.11); a track taken up without a new stream never did.
struct Parked {
    driver: Driver,
    setup: Setup,
    rate: u32,
    mode: String,
}

// The driver and its buffers are only touched under OPEN_LOCK / PARKED.
unsafe impl Send for Parked {}

static PARKED: Mutex<Option<Parked>> = Mutex::new(None);
/// `p` parked: it plays DSD silence until the next stream takes it up, or
/// another stream (another rate, format or device) or the app's exit
/// (`release_parked`) stops it. Stopped after a few seconds, the next Play
/// started it again, from PCM, and the DAC clicked (Anton 19.11, Mojo 2 at
/// DSD256: after 5 min on Stop, and after a track waiting for its
/// Hybrid-Phase lists).
fn park(p: Parked) {
    crate::aelog!("[PLAYER] ASIO {}: parked, playing DSD silence ({} {})", p.driver.name, p.rate, p.mode);
    if let Some(old) = PARKED.lock().unwrap_or_else(|e| e.into_inner()).replace(p) {
        shut(old, true);
    }
}

/// A parked driver stopped and put back in PCM: before a stream on another
/// backend, and at the app's exit.
pub(super) fn release_parked() {
    let _one = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = PARKED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        crate::aelog!("[PLAYER] ASIO {}: the parked driver stopped and put back in PCM", p.driver.name);
        shut(p, true);
    }
}

/// A parked driver stopped, its host and buffers gone, released; `pcm`: put
/// back in PCM first (no stream of its own follows).
fn shut(p: Parked, pcm: bool) {
    let d = p.driver;
    unsafe {
        (d.vt().stop)(d.p);
        swap_host(std::ptr::null_mut());
        (d.vt().dispose_buffers)(d.p);
    }
    if pcm {
        d.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
    }
    drop(d);
}

/// The next stream on a parked driver: a fresh host on its running buffers.
fn take_up(p: Parked, cfg: &OutputConfig) -> Result<Opened<AsioStream>, String> {
    crate::aelog!("[PLAYER] ASIO {}: the new stream takes up the driver still running in DSD (no stop and start between tracks)", p.driver.name);
    let stream = AsioStream {
        driver: Some(p.driver),
        host: None,
        native: true,
        started: true,
        setup: None,
        rate: cfg.rate,
        mode: mode_label(&cfg.mode),
    };
    run(stream, p.setup, cfg)
}

/// Opens, and the driver put back in PCM after native DSD, one at a time.
static OPEN_LOCK: Mutex<()> = Mutex::new(());
/// Streams opened since the start: a PCM restore that sees a later open
/// leaves the driver to it.
static OPENS: AtomicU64 = AtomicU64::new(0);
/// How long after a native DSD stream the driver is put back in PCM when no
/// stream has taken it again.
const PCM_RESTORE_AFTER: Duration = Duration::from_secs(3);

/// The driver `key` back in PCM after a native DSD stream — later, and only
/// when no stream opened meanwhile. Putting it back at once flipped the DAC
/// from DSD to PCM and back between two tracks, and it clicked at the start
/// of the next one (Anton 18.11, Mojo 2 on DSD256). Its own thread, its own
/// load of the driver (COM there as on any thread that loads one).
fn restore_pcm_later(key: String) {
    let at = OPENS.load(Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("asio-pcm".into()).spawn(move || {
        std::thread::sleep(PCM_RESTORE_AFTER);
        let _one = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if OPENS.load(Ordering::SeqCst) != at {
            return;
        }
        match Driver::load(&key) {
            Ok(d) => {
                d.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
            }
            Err(e) => crate::aelog!("[PLAYER] ASIO {key}: not put back in PCM after DSD ({e})"),
        }
    });
    if let Err(e) = spawned {
        crate::aelog!("[PLAYER] ASIO: no thread to put the driver back in PCM ({e})");
    }
}

/// The stream's format on `d` for `cfg`: native DSD where the driver has
/// a DSD mode (`AURA_ASIO_DOP=1` asks for DoP over PCM instead), else
/// PCM — the DoP words for a DSD stream — at `cfg.rate`.
fn negotiate(d: &Driver, cfg: &OutputConfig) -> Result<(Layout, SampleFmt, String, bool), String> {
    let dop_only = std::env::var_os("AURA_ASIO_DOP").is_some_and(|v| v == "1");
    let dsd = match cfg.mode {
        OutputMode::Dsd { rate, .. } | OutputMode::DsdDirect { rate } => Some(rate),
        _ => None,
    };
    if let Some(dsd) = dsd.filter(|_| !dop_only) {
        let bits = dsd.bit_rate();
        let bpf = dsd_bytes_per_frame(bits, cfg.rate);
        let can = d.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_DSD_FORMAT);
        let set = if ok(can) { d.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_DSD_FORMAT) } else { can };
        match (bpf, ok(set)) {
            (Some(bpf), true) => {
                let t = d.out_type();
                let lsb_first = match t {
                    ASIOST_DSD_INT8_MSB1 => Some(false),
                    ASIOST_DSD_INT8_LSB1 => Some(true),
                    _ => None,
                };
                let name = crate::player::dsd_direct::label(dsd);
                let rate = d.set_rate(bits as f64, &format!("{name} ({bits} one-bit samples a second)"));
                match (lsb_first, rate) {
                    (Some(lsb_first), Ok(())) => {
                        let label = format!("DSD native ({}, {:.4} MHz)", sample_type_name(t), bits as f64 / 1e6);
                        return Ok((Layout::Dsd { lsb_first, bpf }, SampleFmt::Dsd, label, true));
                    }
                    (None, _) => crate::aelog!("[PLAYER] ASIO {}: DSD mode gives sample type {}", d.name, sample_type_name(t)),
                    (_, Err(e)) => crate::aelog!("[PLAYER] {e}"),
                }
            }
            (None, _) => crate::aelog!("[PLAYER] ASIO {}: a {} Hz stream carries no whole DSD bytes of {}", d.name, cfg.rate, dsd.multiple),
            (_, false) => crate::aelog!("[PLAYER] ASIO {}: no DSD mode ({})", d.name, error_name(set)),
        }
        // No native DSD: DoP over PCM, which carries DSD64-DSD256 only.
        if dsd.multiple > 256 || !cfg.mode.is_dop() || bpf != Some(2) {
            d.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
            return Err(format!(
                "{DOP_FORMAT_ERR}; ASIO: {} plays no native {} and DoP carries DSD64-DSD256 only",
                d.name,
                crate::player::dsd_direct::label(dsd)
            ));
        }
        crate::aelog!("[PLAYER] ASIO {}: DoP over PCM instead", d.name);
    }
    // PCM (a DSD stream: DoP words at its carrier).
    let pcm = d.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
    if !ok(pcm) && cfg.mode.is_dop() {
        crate::aelog!("[PLAYER] ASIO {}: kAsioSetIoFormat(PCM) {}", d.name, error_name(pcm));
    }
    let t = d.out_type();
    let (fmt, bytes) = match t {
        ASIOST_INT16_LSB => (SampleFmt::Int16, 2),
        ASIOST_INT24_LSB => (SampleFmt::Int24Packed, 3),
        ASIOST_INT32_LSB => (SampleFmt::Int32, 4),
        ASIOST_FLOAT32_LSB => (SampleFmt::Float32, 4),
        _ => return Err(format!("ASIO: {} plays sample type {}, which the player does not write yet", d.name, sample_type_name(t))),
    };
    let dop = cfg.mode.is_dop();
    if dop && !matches!(fmt, SampleFmt::Int24Packed | SampleFmt::Int32) {
        return Err(format!("{DOP_FORMAT_ERR} at {} Hz on ASIO: {} (it plays {})", cfg.rate, d.name, sample_type_name(t)));
    }
    if let Err(e) = d.set_rate(cfg.rate as f64, &format!("{} Hz", cfg.rate)) {
        return Err(if dop { format!("{DOP_FORMAT_ERR} at {} Hz ({e})", cfg.rate) } else { e });
    }
    let what = if dop { "DoP over PCM, " } else { "" };
    Ok((Layout::Pcm { bytes, dop }, fmt, format!("{what}{} (ASIO)", sample_type_name(t)), false))
}

/// The driver keeps the pointer to these for as long as its buffers live
/// (the SDK's own hosts hold them in a global) and calls through it from
/// its thread: a local in `open_on` left it calling into a dead stack frame.
/// Writable memory, as the SDK's pointer is not const.
static mut CALLBACKS: AsioCallbacks = AsioCallbacks { buffer_switch, sample_rate_did_change, asio_message, buffer_switch_time_info };

fn open_on(d: Driver, cfg: &OutputConfig) -> Result<Opened<AsioStream>, String> {
    let (mut ins, mut outs) = (0, 0);
    unsafe { (d.vt().get_channels)(d.p, &mut ins, &mut outs) };
    if outs < 2 {
        return Err(format!("ASIO: {} has {outs} output channels; the player needs two", d.name));
    }
    let (layout, fmt, label, native) = negotiate(&d, cfg)?;
    let stream = AsioStream { driver: Some(d), host: None, native, started: false, setup: None, rate: cfg.rate, mode: mode_label(&cfg.mode) };
    let d = stream.driver.as_ref().unwrap();

    let (mut mn, mut mx, mut pref, mut gran) = (0, 0, 0, 0);
    unsafe { (d.vt().get_buffer_size)(d.p, &mut mn, &mut mx, &mut pref, &mut gran) };
    // The driver's samples a second and a buffer's multiple: one-bit
    // samples for DSD, a whole number of frames.
    let (per_s, multiple) = match layout {
        Layout::Dsd { bpf, .. } => (cfg.rate as f64 * (8 * bpf) as f64, 8 * bpf),
        Layout::Pcm { .. } => (cfg.rate as f64, 1),
    };
    let preferred = pref.max(mn).max(1) as usize;
    let asked = pick_buffer(mn, mx, pref, gran, per_s, BUFFER_S, multiple);

    let mut infos = [
        AsioBufferInfo { is_input: 0, channel_num: 0, buffers: [std::ptr::null_mut(); 2] },
        AsioBufferInfo { is_input: 0, channel_num: 1, buffers: [std::ptr::null_mut(); 2] },
    ];
    let cbs = &raw mut CALLBACKS;
    let mut samples = asked;
    let mut e = unsafe { (d.vt().create_buffers)(d.p, infos.as_mut_ptr(), 2, samples as i32, cbs) };
    if !ok(e) && asked != preferred {
        // A driver that lists a size it then refuses: its own.
        crate::aelog!("[PLAYER] ASIO {}: createBuffers({asked}) refused ({}), its preferred {preferred} instead", d.name, error_name(e));
        samples = preferred;
        e = unsafe { (d.vt().create_buffers)(d.p, infos.as_mut_ptr(), 2, samples as i32, cbs) };
    }
    if !ok(e) {
        return Err(start_error(&d.name, &format!("createBuffers({samples})"), e, &d.message()));
    }
    let half = match layout {
        Layout::Dsd { bpf, .. } => match dsd_half_frames(samples, bpf) {
            Some(h) => h,
            None => {
                unsafe { (d.vt().dispose_buffers)(d.p) };
                return Err(format!("ASIO: {} offers a DSD buffer of {samples} samples, no whole number of frames", d.name));
            }
        },
        Layout::Pcm { .. } => samples,
    };
    let (chunk, ring_cap) = chunk_and_ring(half, cfg.rate);
    if infos.iter().any(|i| i.buffers.iter().any(|b| b.is_null())) {
        unsafe { (d.vt().dispose_buffers)(d.p) };
        return Err(format!("ASIO: {} gave no buffers", d.name));
    }
    let setup = Setup {
        layout,
        fmt,
        label,
        samples,
        half,
        chunk,
        ring_cap,
        bufs: [
            [infos[0].buffers[0] as *mut u8, infos[0].buffers[1] as *mut u8],
            [infos[1].buffers[0] as *mut u8, infos[1].buffers[1] as *mut u8],
        ],
        limits: (mn, mx, pref, gran),
        output_ready: ok(unsafe { (d.vt().output_ready)(d.p) }),
    };
    // Both halves start as silence of the stream's kind.
    unsafe {
        for half_i in 0..2 {
            for ch in 0..2 {
                layout.idle(std::slice::from_raw_parts_mut(setup.bufs[ch][half_i], half * layout.unit()), 0);
            }
        }
    }
    run(stream, setup, cfg)
}

/// A stream's buffers as the driver gave them (kept for a parked driver).
#[derive(Clone)]
struct Setup {
    layout: Layout,
    fmt: SampleFmt,
    label: String,
    samples: usize,
    half: usize,
    chunk: usize,
    ring_cap: usize,
    bufs: [[*mut u8; 2]; 2],
    /// The driver's buffer sizes: min, max, preferred, granularity.
    limits: (i32, i32, i32, i32),
    output_ready: bool,
}

/// A fresh host for the callback on `s`'s buffers: an empty ring.
fn new_host(s: &Setup, d: &Driver) -> *mut Host {
    Box::into_raw(Box::new(Host {
        ring: Ring::new(s.ring_cap, s.layout.unit()),
        layout: s.layout,
        bufs: s.bufs,
        half: s.half,
        chunk: s.chunk,
        phase: AtomicUsize::new(0),
        callbacks: AtomicU64::new(0),
        last_ns: AtomicU64::new(0),
        dry: AtomicU64::new(0),
        armed: AtomicBool::new(false),
        reset: AtomicBool::new(false),
        audio_thread: std::thread::current(),
        driver: (d.p, d.v),
        output_ready: s.output_ready,
    }))
}

/// The callback's host replaced by `next` (null: none); the old one freed
/// once no callback is inside it. Returns the old one's dry buffers.
unsafe fn swap_host(next: *mut Host) -> u64 {
    let old = HOST.swap(next, Ordering::AcqRel);
    if old.is_null() {
        return 0;
    }
    let t0 = Instant::now();
    while BUSY.load(Ordering::Acquire) > 0 && t0.elapsed() < Duration::from_millis(500) {
        std::thread::yield_now();
    }
    let dry = (*old).dry.load(Ordering::Relaxed);
    if BUSY.load(Ordering::Acquire) == 0 {
        drop(Box::from_raw(old));
    }
    dry
}

/// The stream on its buffers: the callback's host set, the driver started
/// (a parked one runs already), its first call waited for.
fn run(mut stream: AsioStream, s: Setup, cfg: &OutputConfig) -> Result<Opened<AsioStream>, String> {
    let d = stream.driver.as_ref().unwrap();
    let host_ptr = new_host(&s, d);
    unsafe { swap_host(host_ptr) };
    stream.host = Some(host_ptr);
    let host = unsafe { &*host_ptr };
    let taken_up = stream.started;

    // Started here, not at the gate: a DAC Windows holds refuses now, in
    // the open's words; until the music the driver plays the silence of a
    // dry ring.
    if !stream.started {
        let e = unsafe { (d.vt().start)(d.p) };
        if !ok(e) {
            return Err(start_error(&d.name, "start", e, &d.message()));
        }
        stream.started = true;
    }
    let t0 = Instant::now();
    while host.callbacks.load(Ordering::Relaxed) == 0 {
        if t0.elapsed() > FIRST_CALL_WAIT {
            return Err(format!(
                "ASIO: {} started but never asked for sound within {:.1} s - the DAC unplugged, or held by Windows \
                 or another program? Make another device the default output in Windows, then press Play again.",
                d.name,
                FIRST_CALL_WAIT.as_secs_f64()
            ));
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }

    let (mut in_lat, mut out_lat) = (0, 0);
    unsafe { (d.vt().get_latencies)(d.p, &mut in_lat, &mut out_lat) };
    let out_lat = match s.layout {
        Layout::Dsd { bpf, .. } => out_lat.max(0) as usize / (8 * bpf),
        Layout::Pcm { .. } => out_lat.max(0) as usize,
    };
    let half = s.half;
    let latency = (host.ring.cap() + 2 * half + out_lat) as u32;
    let device_name = format!("ASIO: {}", d.name);
    let (mn, mx, pref, gran) = s.limits;
    crate::aelog!(
        "[PLAYER] stream open (ASIO): driver={:?} rate={} format={} buffer={} samples ({half} frames a half; \
         driver min={mn} max={mx} preferred={pref} granularity={gran}) period={} frames \
         ring={} frames latency~{latency} frames outputReady={} mode={}{}",
        d.name,
        cfg.rate,
        s.label,
        s.samples,
        s.chunk,
        host.ring.cap(),
        host.output_ready,
        mode_label(&cfg.mode),
        if taken_up { " (taken up running)" } else { "" }
    );
    let info = StreamInfo {
        device_name,
        exclusive: true,
        format: s.label.clone(),
        rate: cfg.rate,
        period_frames: s.chunk as u32,
        buffer_frames: s.chunk as u32,
        latency_frames: latency,
    };
    let fmt = s.fmt;
    let blockalign = s.layout.period_frame_bytes();
    stream.setup = Some(s);
    Ok(Opened { stream, fmt, blockalign, info })
}

/// An open ASIO stream: the driver started, playing the silence of a dry
/// ring until the render loop's periods come.
pub(super) struct AsioStream {
    driver: Option<Driver>,
    host: Option<*mut Host>,
    native: bool,
    started: bool,
    /// Its buffers once it runs: what parking keeps.
    setup: Option<Setup>,
    rate: u32,
    mode: String,
}

impl AsioStream {
    fn host(&self) -> &Host {
        unsafe { &*self.host.expect("the ASIO host") }
    }

    fn scratch(&self, frames: usize) -> (Vec<u8>, Vec<u8>) {
        let n = frames * self.host().layout.unit();
        (vec![0; n], vec![0; n])
    }

    /// Stop the driver, take the callback's host away (waiting out a
    /// callback inside it), dispose of the buffers, release it; after DSD
    /// it is put back in PCM a while later (`restore_pcm_later`). A native
    /// DSD stream that runs well is parked instead (`park`): its driver goes
    /// on playing DSD silence for the next stream to take up. Idempotent.
    fn close(&mut self) {
        let Some(mut d) = self.driver.take() else { return };
        if self.native {
            let (reset, age_ms) = match self.host {
                Some(h) => unsafe {
                    ((*h).reset.load(Ordering::Acquire), now_ns().saturating_sub((*h).last_ns.load(Ordering::Relaxed)) / 1_000_000)
                },
                None => (false, u64::MAX),
            };
            crate::aelog!(
                "[PLAYER] ASIO {}: closing a DSD stream - started={} host={} setup={} reset={reset} last call {age_ms} ms ago",
                d.name,
                self.started,
                self.host.is_some(),
                self.setup.is_some()
            );
        }
        if self.native && self.started {
            if let (Some(h), Some(s)) = (self.host, self.setup.take()) {
                let healthy = unsafe {
                    !(*h).reset.load(Ordering::Acquire)
                        && now_ns().saturating_sub((*h).last_ns.load(Ordering::Relaxed)) < 500_000_000
                };
                if healthy {
                    // Silence on a fresh host in place of the rest of the track.
                    let dry = unsafe { swap_host(new_host(&s, &d)) };
                    if dry > 0 {
                        crate::aelog!("[PLAYER] ASIO {}: {dry} buffers ran dry (silence sent)", d.name);
                    }
                    self.host = None;
                    self.started = false;
                    // The thread that loaded it keeps its COM: the driver outlives it.
                    d.com = false;
                    park(Parked { driver: d, setup: s, rate: self.rate, mode: self.mode.clone() });
                    return;
                }
            }
        }
        unsafe {
            if self.started {
                (d.vt().stop)(d.p);
                self.started = false;
            }
            if self.host.take().is_some() {
                let dry = swap_host(std::ptr::null_mut());
                if dry > 0 {
                    crate::aelog!("[PLAYER] ASIO {}: {dry} buffers ran dry (silence sent)", d.name);
                }
                (d.vt().dispose_buffers)(d.p);
            }
            if self.native {
                restore_pcm_later(d.name.clone());
            }
        }
        drop(d);
    }
}

impl Drop for AsioStream {
    fn drop(&mut self) {
        self.close();
    }
}

impl super::Stream for AsioStream {
    type Dev<'a> = AsioDev<'a>;

    fn prefill(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let (mut l, mut r) = self.scratch(frames);
        let h = self.host();
        h.layout.split(frames, bytes, &mut l, &mut r);
        h.ring.push(frames.min(h.ring.free()), &l, &r);
        Ok(())
    }

    fn start<'a>(&'a mut self, _shared: &'a OutputShared) -> Result<AsioDev<'a>, String> {
        let chunk = self.host().chunk;
        let (l, r) = self.scratch(chunk);
        self.host().armed.store(true, Ordering::Relaxed);
        let name = self.driver.as_ref().map(|d| d.name.clone()).unwrap_or_default();
        Ok(AsioDev { host: self.host(), l, r, name, seen: (0, Instant::now()) })
    }

    fn stop(&mut self) {
        self.close();
    }
}

/// The ring as the render loop's device (`period::Device`): room for a
/// period is what it asks for; the driver's callback wakes it.
pub(super) struct AsioDev<'a> {
    host: &'a Host,
    l: Vec<u8>,
    r: Vec<u8>,
    name: String,
    /// The callbacks counted when the count last moved, and when.
    seen: (u64, Instant),
}

impl AsioDev<'_> {
    fn stalled(&mut self) -> bool {
        let n = self.host.callbacks.load(Ordering::Relaxed);
        if n != self.seen.0 {
            self.seen = (n, Instant::now());
            return false;
        }
        self.seen.1.elapsed() > STALL
    }

    fn ready(&mut self) -> bool {
        self.host.ring.free() >= self.host.chunk || self.host.reset.load(Ordering::Acquire) || self.stalled()
    }
}

impl super::Device for AsioDev<'_> {
    fn wait(&mut self) -> bool {
        if self.ready() {
            return true;
        }
        std::thread::park_timeout(Duration::from_millis(100));
        self.ready()
    }

    fn space(&mut self) -> Result<usize, String> {
        if self.host.reset.load(Ordering::Acquire) {
            return Err(format!("ASIO: {} asked to be opened again (its settings changed, or the DAC went away)", self.name));
        }
        if self.stalled() {
            return Err(format!(
                "ASIO: {} stopped asking for sound (the DAC unplugged, or taken by another program?)",
                self.name
            ));
        }
        Ok(self.host.ring.free().min(self.host.chunk))
    }

    fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let h = self.host;
        h.layout.split(frames, bytes, &mut self.l, &mut self.r);
        h.ring.push(frames, &self.l, &self.r);
        Ok(())
    }
}
