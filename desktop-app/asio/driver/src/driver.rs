// SPDX-License-Identifier: GPL-3.0-only
//! The driver object a player makes: two outputs, Int32 samples, every rate
//! Aura Player upsamples from — and no hardware. What the player gives in its
//! buffers goes into the link (`aura_link`) for Aura Player; the link's room
//! sets the pace, so the player plays at the rate Aura Player's DAC takes it.
//!
//! The worker thread is the driver's "hardware": while the server listens
//! with an even reader it calls the player's bufferSwitch by a clock of its
//! own (`aura_link::clock`: once a period, trimmed to what the reader takes,
//! a reserve in the link for the reader's hiccups); while it listens
//! otherwise, or holds, only when a block fits the link — and puts the block
//! in; while the server does not want the stream (Released, or no
//! heartbeat) it calls it in real time by the system clock and drops the
//! samples, so the player plays on as into an unplugged cable.
//!
//! It keeps an account of the calls in the link's header for the server's
//! log (`Account`): the late ones, and why.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aura_link::clock::{Clock, Next};
use aura_link::{Client, LateCall, LinkError, Mode};

use crate::sys::*;
use crate::{asio_method, objects, os, CLSID};

pub const DRIVER_NAME: &str = "Aura Player ASIO";
pub const DRIVER_VERSION: i32 = 1;
pub const CHANNELS: usize = 2;
/// The samples a player hands over: 32-bit integers, little-endian — every
/// 16- and 24-bit source whole, and the one format every ASIO host writes.
pub const SAMPLE_TYPE: AsioSampleType = ASIOST_INT32_LSB;
/// The rates Aura Player takes in: the 44.1 and 48 kHz families to ×16.
pub const RATES: [f64; 10] =
    [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0, 352_800.0, 384_000.0, 705_600.0, 768_000.0];
pub const BUFFER_MIN: i32 = 64;
pub const BUFFER_MAX: i32 = 16_384;
pub const BUFFER_PREFERRED: i32 = 2_048;
/// The server's beat must move within this, or it counts as gone.
const ALIVE: Duration = Duration::from_secs(1);
/// How long the worker sleeps at most before it looks again.
const POLL: Duration = Duration::from_millis(20);
/// A call this many periods after the one before is late (the account).
const LATE: f64 = 1.5;

const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;

/// An Int32 sample as the f64 the link carries: exact.
pub fn int32_to_f64(v: i32) -> f64 {
    v as f64 / 2_147_483_648.0
}

/// The link's error as a player shows it (a C string up to 124 bytes).
pub fn message(e: &LinkError) -> String {
    match e {
        LinkError::NoServer => "Aura Player isn't running. Start it, then choose this device again.".into(),
        LinkError::Busy => "Another player is using Aura Player ASIO.".into(),
        LinkError::Version(_) => "This driver doesn't match Aura Player. Reinstall it from Aura Player's settings.".into(),
        LinkError::Os(c) => format!("Couldn't connect to Aura Player (system error {c})."),
    }
}

/// The driver's COM object: its vtable first, as COM wants.
#[repr(C)]
pub struct Driver {
    vtbl: &'static IAsioVtbl,
    refs: AtomicU32,
    inner: Mutex<Inner>,
    shared: Arc<Shared>,
}

/// What the player's thread and the worker share.
struct Shared {
    /// Frames handed to the player since start, and when (ns, system clock).
    position: AtomicU64,
    stamp_ns: AtomicU64,
    rate_bits: AtomicU64,
    /// The server's delay to its sound, frames.
    latency: AtomicU32,
    stop: AtomicBool,
}

struct Inner {
    error: String,
    client: Option<Client>,
    buffers: Option<Buffers>,
    worker: Option<JoinHandle<Client>>,
}

/// The double buffers the player writes, per output channel.
struct Buffers {
    size: usize,
    halves: [Option<[Box<[i32]>; 2]>; CHANNELS],
    callbacks: AsioCallbacks,
    time_info: bool,
}

/// The halves' addresses, for the worker (the buffers outlive it: they are
/// only freed after it is joined).
#[derive(Clone, Copy)]
struct Halves([[*const i32; 2]; CHANNELS]);
// SAFETY: see above; the worker only reads them.
unsafe impl Send for Halves {}

impl Driver {
    pub fn new() -> *mut c_void {
        objects::add();
        let d = Box::new(Driver {
            vtbl: &VTBL,
            refs: AtomicU32::new(1),
            inner: Mutex::new(Inner { error: String::new(), client: None, buffers: None, worker: None }),
            shared: Arc::new(Shared {
                position: AtomicU64::new(0),
                stamp_ns: AtomicU64::new(0),
                rate_bits: AtomicU64::new(44_100f64.to_bits()),
                latency: AtomicU32::new(0),
                stop: AtomicBool::new(false),
            }),
        });
        Box::into_raw(d) as *mut c_void
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rate(&self) -> f64 {
        f64::from_bits(self.shared.rate_bits.load(Acquire))
    }

    fn stop_worker(&self, inner: &mut Inner) {
        if let Some(w) = inner.worker.take() {
            self.shared.stop.store(true, Release);
            if let Ok(mut client) = w.join() {
                client.end();
                inner.client = Some(client);
            }
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = inner.worker.take() {
            self.shared.stop.store(true, Release);
            let _ = w.join();
        }
        inner.buffers = None;
        inner.client = None;
        objects::remove();
    }
}

/// `this` as the driver.
///
/// # Safety
/// `this` is a live `Driver` made by `Driver::new`.
unsafe fn me<'a>(this: *mut c_void) -> &'a Driver {
    &*(this as *const Driver)
}

/// Runs `f`, or gives `fail` if it panicked: a panic must not unwind into
/// the player.
fn guard<T>(fail: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fail)
}

fn is_rate(r: f64) -> bool {
    RATES.iter().any(|&x| (x - r).abs() < 0.5)
}

unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // Hosts ask for the driver by its CLSID as the interface (the SDK's own
    // host code does).
    if *iid == IID_IUNKNOWN || *iid == CLSID {
        add_ref(this);
        *out = this;
        0
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    me(this).refs.fetch_add(1, AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let left = me(this).refs.fetch_sub(1, AcqRel) - 1;
    if left == 0 {
        drop(Box::from_raw(this as *mut Driver));
    }
    left
}

asio_method! {
    fn init(this: *mut c_void, _sys: *mut c_void) -> AsioBool {
        guard(ASIO_FALSE, || {
            let d = me(this);
            let mut inner = d.inner();
            if inner.client.is_some() || inner.worker.is_some() {
                return ASIO_TRUE;
            }
            let base = std::env::var("AURA_LINK_BASE").unwrap_or_else(|_| aura_link::win::BASE.to_string());
            match aura_link::win::open_client(&base) {
                Ok(c) => {
                    c.set_name(&os::host_name());
                    d.shared.latency.store(c.latency_frames(), Release);
                    inner.client = Some(c);
                    inner.error.clear();
                    ASIO_TRUE
                }
                Err(e) => {
                    inner.error = message(&e);
                    ASIO_FALSE
                }
            }
        })
    }
}

asio_method! {
    fn get_driver_name(_this: *mut c_void, name: *mut u8) {
        put_c_str(name, 32, DRIVER_NAME);
    }
}

asio_method! {
    fn get_driver_version(_this: *mut c_void) -> i32 {
        DRIVER_VERSION
    }
}

asio_method! {
    fn get_error_message(this: *mut c_void, s: *mut u8) {
        guard((), || put_c_str(s, 124, &me(this).inner().error));
    }
}

asio_method! {
    fn start(this: *mut c_void) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            let d = me(this);
            let mut inner = d.inner();
            if inner.worker.is_some() {
                return ASE_OK;
            }
            let Some(b) = inner.buffers.as_ref() else { return ASE_INVALID_MODE };
            let mut halves = Halves([[std::ptr::null(); 2]; CHANNELS]);
            for (c, h) in b.halves.iter().enumerate() {
                if let Some([h0, h1]) = h {
                    halves.0[c] = [h0.as_ptr(), h1.as_ptr()];
                }
            }
            let (size, cb, time_info) = (b.size, b.callbacks, b.time_info);
            let Some(mut client) = inner.client.take() else { return ASE_NOT_PRESENT };
            client.begin(d.rate(), CHANNELS as u32, SAMPLE_TYPE as u32);
            d.shared.stop.store(false, Release);
            d.shared.position.store(0, Release);
            d.shared.stamp_ns.store(os::now_ns(), Release);
            let shared = d.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("Aura Player ASIO".into())
                .spawn(move || run(client, halves, size, cb, time_info, &shared));
            match spawned {
                Ok(w) => {
                    inner.worker = Some(w);
                    ASE_OK
                }
                Err(_) => ASE_HW_MALFUNCTION,
            }
        })
    }
}

asio_method! {
    fn stop(this: *mut c_void) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            let d = me(this);
            let mut inner = d.inner();
            d.stop_worker(&mut inner);
            ASE_OK
        })
    }
}

asio_method! {
    fn get_channels(_this: *mut c_void, inputs: *mut i32, outputs: *mut i32) -> AsioError {
        if !inputs.is_null() {
            *inputs = 0;
        }
        if !outputs.is_null() {
            *outputs = CHANNELS as i32;
        }
        ASE_OK
    }
}

asio_method! {
    fn get_latencies(this: *mut c_void, input: *mut i32, output: *mut i32) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            let d = me(this);
            let inner = d.inner();
            let size = inner.buffers.as_ref().map_or(BUFFER_PREFERRED as usize, |b| b.size);
            let server = match inner.client.as_ref() {
                Some(c) => c.latency_frames(),
                None => d.shared.latency.load(Acquire),
            };
            if !input.is_null() {
                *input = 0;
            }
            if !output.is_null() {
                *output = (size as u64 + server as u64).min(i32::MAX as u64) as i32;
            }
            ASE_OK
        })
    }
}

asio_method! {
    fn get_buffer_size(_this: *mut c_void, min: *mut i32, max: *mut i32, preferred: *mut i32, granularity: *mut i32) -> AsioError {
        for (p, v) in [(min, BUFFER_MIN), (max, BUFFER_MAX), (preferred, BUFFER_PREFERRED), (granularity, -1)] {
            if !p.is_null() {
                *p = v;
            }
        }
        ASE_OK
    }
}

asio_method! {
    fn can_sample_rate(_this: *mut c_void, rate: AsioSampleRate) -> AsioError {
        if is_rate(rate) { ASE_OK } else { ASE_NO_CLOCK }
    }
}

asio_method! {
    fn get_sample_rate(this: *mut c_void, rate: *mut AsioSampleRate) -> AsioError {
        if rate.is_null() {
            return ASE_INVALID_PARAMETER;
        }
        *rate = me(this).rate();
        ASE_OK
    }
}

asio_method! {
    fn set_sample_rate(this: *mut c_void, rate: AsioSampleRate) -> AsioError {
        if !is_rate(rate) {
            return ASE_NO_CLOCK;
        }
        // A running worker starts a new segment at its next block.
        me(this).shared.rate_bits.store(rate.to_bits(), Release);
        ASE_OK
    }
}

asio_method! {
    fn get_clock_sources(_this: *mut c_void, clocks: *mut AsioClockSource, count: *mut i32) -> AsioError {
        if !clocks.is_null() && (count.is_null() || *count >= 1) {
            let mut name = [0u8; 32];
            put_c_str(name.as_mut_ptr(), 32, "Aura Player");
            *clocks = AsioClockSource { index: 0, associated_channel: -1, associated_group: -1, is_current_source: ASIO_TRUE, name };
        }
        if !count.is_null() {
            *count = 1;
        }
        ASE_OK
    }
}

asio_method! {
    fn set_clock_source(_this: *mut c_void, reference: i32) -> AsioError {
        if reference == 0 { ASE_OK } else { ASE_INVALID_MODE }
    }
}

asio_method! {
    fn get_sample_position(this: *mut c_void, pos: *mut AsioSamples, stamp: *mut AsioTimeStamp) -> AsioError {
        let s = &me(this).shared;
        if !pos.is_null() {
            *pos = s.position.load(Acquire).into();
        }
        if !stamp.is_null() {
            *stamp = s.stamp_ns.load(Acquire).into();
        }
        ASE_OK
    }
}

asio_method! {
    fn get_channel_info(this: *mut c_void, info: *mut AsioChannelInfo) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            if info.is_null() {
                return ASE_INVALID_PARAMETER;
            }
            let i = &mut *info;
            if i.is_input != ASIO_FALSE || i.channel < 0 || i.channel as usize >= CHANNELS {
                return ASE_INVALID_PARAMETER;
            }
            let c = i.channel as usize;
            let active = me(this).inner().buffers.as_ref().is_some_and(|b| b.halves[c].is_some());
            i.is_active = if active { ASIO_TRUE } else { ASIO_FALSE };
            i.channel_group = 0;
            i.sample_type = SAMPLE_TYPE;
            put_c_str(i.name.as_mut_ptr(), 32, if c == 0 { "Aura Player L" } else { "Aura Player R" });
            ASE_OK
        })
    }
}

asio_method! {
    fn create_buffers(this: *mut c_void, infos: *mut AsioBufferInfo, count: i32, size: i32, callbacks: *mut AsioCallbacks) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            let d = me(this);
            let mut inner = d.inner();
            if inner.buffers.is_some() {
                return ASE_INVALID_MODE;
            }
            if infos.is_null() || callbacks.is_null() || count <= 0 || !(BUFFER_MIN..=BUFFER_MAX).contains(&size) {
                return ASE_INVALID_PARAMETER;
            }
            let cb = *callbacks;
            if cb.buffer_switch.is_none() && cb.buffer_switch_time_info.is_none() {
                return ASE_INVALID_PARAMETER;
            }
            let infos = std::slice::from_raw_parts_mut(infos, count as usize);
            let mut halves: [Option<[Box<[i32]>; 2]>; CHANNELS] = Default::default();
            for i in infos.iter() {
                if i.is_input != ASIO_FALSE || i.channel_num < 0 || i.channel_num as usize >= CHANNELS {
                    return ASE_INVALID_PARAMETER;
                }
            }
            for i in infos.iter_mut() {
                let c = i.channel_num as usize;
                let h = halves[c].get_or_insert_with(|| {
                    [vec![0i32; size as usize].into_boxed_slice(), vec![0i32; size as usize].into_boxed_slice()]
                });
                i.buffers = [h[0].as_mut_ptr() as *mut c_void, h[1].as_mut_ptr() as *mut c_void];
            }
            let asks = |sel: i32, value: i32| cb.asio_message.map_or(0, |m| m(sel, value, std::ptr::null_mut(), std::ptr::null_mut()));
            let time_info = cb.buffer_switch_time_info.is_some()
                && asks(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO) == 1
                && asks(K_ASIO_SUPPORTS_TIME_INFO, 0) == 1;
            let time_info = time_info || cb.buffer_switch.is_none();
            inner.buffers = Some(Buffers { size: size as usize, halves, callbacks: cb, time_info });
            ASE_OK
        })
    }
}

asio_method! {
    fn dispose_buffers(this: *mut c_void) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            let d = me(this);
            let mut inner = d.inner();
            d.stop_worker(&mut inner);
            if inner.buffers.take().is_none() {
                return ASE_INVALID_MODE;
            }
            ASE_OK
        })
    }
}

asio_method! {
    fn control_panel(_this: *mut c_void) -> AsioError {
        guard(ASE_HW_MALFUNCTION, || {
            os::message_box(
                DRIVER_NAME,
                "Aura Player ASIO plays what this player sends through Aura Player.\n\n\
                 Choose the filter, the output and the volume in Aura Player.",
            );
            ASE_OK
        })
    }
}

asio_method! {
    fn future(_this: *mut c_void, selector: i32, opt: *mut c_void) -> AsioError {
        match selector {
            K_ASIO_CAN_TIME_INFO => ASE_SUCCESS,
            K_ASIO_GET_IO_FORMAT if !opt.is_null() => {
                (*(opt as *mut AsioIoFormat)).format_type = K_ASIO_PCM_FORMAT;
                ASE_SUCCESS
            }
            K_ASIO_SET_IO_FORMAT | K_ASIO_CAN_DO_IO_FORMAT
                if !opt.is_null() && (*(opt as *const AsioIoFormat)).format_type == K_ASIO_PCM_FORMAT =>
            {
                ASE_SUCCESS
            }
            _ => ASE_NOT_PRESENT,
        }
    }
}

asio_method! {
    fn output_ready(_this: *mut c_void) -> AsioError {
        ASE_NOT_PRESENT
    }
}

static VTBL: IAsioVtbl = IAsioVtbl {
    query_interface,
    add_ref,
    release,
    init,
    get_driver_name,
    get_driver_version,
    get_error_message,
    start,
    stop,
    get_channels,
    get_latencies,
    get_buffer_size,
    can_sample_rate,
    get_sample_rate,
    set_sample_rate,
    get_clock_sources,
    set_clock_source,
    get_sample_position,
    get_channel_info,
    create_buffers,
    dispose_buffers,
    control_panel,
    future,
    output_ready,
};

/// The account of the player's calls the driver keeps in the link's header
/// (`aura_link::CallStats`), from which the server's log tells what came
/// between the player and the link: a call late because the driver waited
/// for room (the server's reader stood), because the player's own call
/// before took long, or because the driver's thread itself was late.
struct Account {
    period: f64,
    /// The call before: when it began, how long it took.
    last: Option<Instant>,
    before_us: u32,
    /// Since it: the time waited for room while listening; and whether the
    /// server held or let go of the stream (a pause: no gap to judge).
    waited: Duration,
    paused: bool,
}

impl Account {
    fn new(client: &Client, size: usize, rate: f64) -> Account {
        let period = size as f64 / rate.max(1.0);
        client.set_period(Duration::from_secs_f64(period));
        Account { period, last: None, before_us: 0, waited: Duration::ZERO, paused: false }
    }

    /// Waits for room in the link: counted while `listening`, else a pause.
    fn wait(&mut self, client: &Client, listening: bool) {
        let t = Instant::now();
        client.wait_room(POLL);
        if listening {
            self.waited += t.elapsed();
        } else {
            self.paused = true;
        }
    }

    /// The server does not want the stream: the calls are its own.
    fn away(&mut self) {
        self.paused = true;
    }

    /// The player was called at `at` and took `took`, the link holding
    /// `used` samples before.
    fn called(&mut self, client: &Client, at: Instant, took: Duration, used: u64) {
        let us = |d: Duration| d.as_micros().min(u32::MAX as u128) as u32;
        let late = self
            .last
            .filter(|_| !self.paused)
            .map(|t| at.saturating_duration_since(t))
            .filter(|g| g.as_secs_f64() > LATE * self.period)
            .map(|g| LateCall { gap_us: us(g), waited_us: us(self.waited), before_us: self.before_us, fill: used.min(u32::MAX as u64) as u32 });
        client.count_call(self.waited.as_micros() as u64, us(took), late);
        self.last = Some(at);
        self.before_us = us(took);
        self.waited = Duration::ZERO;
        self.paused = false;
    }
}

/// The worker: the player's clock — its own while Aura Player listens with
/// an even reader, kept by the link's room while it listens otherwise (or
/// holds), the system clock while it does not. Gives the client back when
/// told to stop.
fn run(mut client: Client, halves: Halves, size: usize, cb: AsioCallbacks, time_info: bool, shared: &Shared) -> Client {
    let _mmcss = os::pro_audio_thread();
    let block = (size * CHANNELS) as u64;
    let mut index = 0usize;
    let mut rate = f64::from_bits(shared.rate_bits.load(Acquire));
    let mut rate_changed = false;
    // When the next block is due by the system clock (not listening).
    let mut due: Option<Instant> = None;
    // The driver's own clock, while the server's reader is even.
    let mut clock: Option<Clock> = None;
    let mut account = Account::new(&client, size, rate);
    while !shared.stop.load(Acquire) {
        let now_rate = f64::from_bits(shared.rate_bits.load(Acquire));
        if now_rate != rate {
            rate = now_rate;
            rate_changed = true;
            client.begin(rate, CHANNELS as u32, SAMPLE_TYPE as u32);
            clock = None;
            account = Account::new(&client, size, rate);
        }
        let mode = client.mode(ALIVE);
        shared.latency.store(client.latency_frames(), Release);
        let keep = mode != Mode::Released;
        let even = mode == Mode::Listening && client.even();
        let mut early = false;
        if even {
            let c = clock.get_or_insert_with(|| Clock::new(rate * CHANNELS as f64, block, client.target(), client.ring_samples()));
            match c.next(Instant::now(), client.used()) {
                Next::Sleep(d) => {
                    std::thread::sleep(d);
                    continue;
                }
                Next::Room => {
                    c.stood();
                    account.wait(&client, true);
                    continue;
                }
                Next::Call { early: e } => early = e,
            }
        } else if keep {
            if let Some(c) = clock.as_mut() {
                c.stood();
            }
            due = None;
            if !client.room(block) {
                account.wait(&client, mode == Mode::Listening);
                continue;
            }
        } else {
            if let Some(c) = clock.as_mut() {
                c.stood();
            }
            account.away();
            let now = Instant::now();
            let at = *due.get_or_insert(now);
            if now < at {
                std::thread::sleep((at - now).min(POLL));
                continue;
            }
            let step = Duration::from_secs_f64(size as f64 / rate);
            // Far behind (the machine slept, a debugger stopped us): from now.
            due = Some(if now - at > Duration::from_millis(500) { now + step } else { at + step });
        }
        let used = client.used();
        let at = Instant::now();
        let stamp = os::now_ns();
        let pos = shared.position.load(Acquire);
        shared.stamp_ns.store(stamp, Release);
        // SAFETY: the player's own callbacks, called the way ASIO calls them:
        // from the driver's thread, with the half it is to fill.
        unsafe {
            if time_info {
                let mut time = AsioTime::default();
                let mut flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID | K_SAMPLE_RATE_VALID | K_SPEED_VALID;
                if rate_changed {
                    flags |= K_SAMPLE_RATE_CHANGED;
                }
                time.time_info = AsioTimeInfo {
                    speed: 1.0,
                    system_time: stamp.into(),
                    sample_position: pos.into(),
                    sample_rate: rate,
                    flags,
                    reserved: [0; 12],
                };
                if let Some(f) = cb.buffer_switch_time_info {
                    f(&mut time, index as i32, ASIO_TRUE);
                }
            } else if let Some(f) = cb.buffer_switch {
                f(index as i32, ASIO_TRUE);
            }
        }
        rate_changed = false;
        let took = at.elapsed();
        match clock.as_mut().filter(|_| even) {
            Some(c) => {
                c.called(at, used, early);
                client.set_clock(true, c.ppm());
            }
            None => client.set_clock(false, 0),
        }
        if keep {
            account.called(&client, at, took, used);
            let h = halves.0;
            client.write_with(block as usize, |i| {
                let (frame, c) = (i / CHANNELS, i % CHANNELS);
                let p = h[c][index];
                // SAFETY: frame < size, the half the player just filled; a
                // channel it did not ask for is silence.
                if p.is_null() { 0.0 } else { int32_to_f64(unsafe { *p.add(frame) }) }
            });
        }
        shared.position.store(pos + size as u64, Release);
        index ^= 1;
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_int32_sample_is_whole_as_f64() {
        for v in [i32::MIN, i32::MIN + 1, -1, 0, 1, 0x7f_ffff << 8, i32::MAX] {
            let f = int32_to_f64(v);
            assert_eq!((f * 2_147_483_648.0) as i64, v as i64);
        }
        assert_eq!(int32_to_f64(i32::MIN), -1.0);
    }

    #[test]
    fn the_rates_are_the_families_to_x16() {
        assert!(is_rate(44_100.0) && is_rate(768_000.0) && is_rate(352_800.0));
        assert!(!is_rate(32_000.0) && !is_rate(0.0) && !is_rate(1_536_000.0));
    }

    #[test]
    fn every_link_error_reads_as_a_sentence_that_fits() {
        for e in [LinkError::NoServer, LinkError::Busy, LinkError::Version(2), LinkError::Os(5)] {
            let m = message(&e);
            assert!(m.len() < 124 && m.ends_with('.'), "{m}");
        }
    }
}
