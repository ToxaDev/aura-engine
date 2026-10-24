// SPDX-License-Identifier: GPL-3.0-only
//! A player in a test: loads the built DLL by its path, as a player loads a
//! registered driver, creates the driver through its class factory and plays
//! into it — with Aura Player's end of the link in this process. Built for
//! x86 too (`--target i686-pc-windows-msvc`), where the driver's methods are
//! `thiscall`.
//!
//! The player's left channel counts frames and its right one is a scramble
//! of the count, so the server can tell any sample lost, doubled or changed.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aura_link::{Mode, Read, Server};
use aura_player_asio::sys::*;
use aura_player_asio::CLSID;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// The player's two channels' halves (addresses), and its frame count.
static HALVES: Mutex<[[usize; 2]; 2]> = Mutex::new([[0; 2]; 2]);
static SIZE: AtomicUsize = AtomicUsize::new(0);
static FRAME: AtomicU64 = AtomicU64::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);
static LAST_FLAGS: AtomicU64 = AtomicU64::new(0);

fn scramble(n: u64) -> i32 {
    (n as u32).wrapping_mul(0x9E37_79B1) as i32
}

fn fill(index: i32) {
    let h = *HALVES.lock().unwrap();
    let size = SIZE.load(Acquire);
    let n0 = FRAME.fetch_add(size as u64, AcqRel);
    for f in 0..size {
        let n = n0 + f as u64;
        // SAFETY: the halves the driver gave, `size` samples each.
        unsafe {
            *(h[0][index as usize] as *mut i32).add(f) = n as i32;
            *(h[1][index as usize] as *mut i32).add(f) = scramble(n);
        }
    }
    CALLS.fetch_add(1, AcqRel);
}

unsafe extern "C" fn buffer_switch(index: i32, _direct: AsioBool) {
    fill(index);
}

unsafe extern "C" fn buffer_switch_time_info(t: *mut AsioTime, index: i32, _direct: AsioBool) -> *mut AsioTime {
    let flags = (*t).time_info.flags;
    LAST_FLAGS.store(flags as u64, Release);
    fill(index);
    t
}

unsafe extern "C" fn sample_rate_did_change(_rate: f64) {}

unsafe extern "C" fn asio_message(selector: i32, value: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    match (selector, value) {
        (K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO) => 1,
        (K_ASIO_SUPPORTS_TIME_INFO, _) => 1,
        (K_ASIO_ENGINE_VERSION, _) => 2,
        _ => 0,
    }
}

fn dll() -> PathBuf {
    // A test's DLL is built beside it: target\[<triple>\]debug\deps.
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().join("aura_player_asio.dll")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn c_str(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// The left channel's count and the right's check, from interleaved samples.
fn check(samples: &[f64], next: &mut Option<u64>) {
    for fr in samples.chunks_exact(2) {
        let n = (fr[0] * 2_147_483_648.0) as i64 as u32 as u64;
        let r = (fr[1] * 2_147_483_648.0) as i64 as i32;
        assert_eq!(r, scramble(n), "the right channel at frame {n}");
        if let Some(want) = *next {
            assert_eq!(n as u32, want as u32, "a frame lost or doubled");
        }
        *next = Some(n + 1);
    }
}

/// Reads until `frames` have come (formats are handed to `on_format`).
fn read_frames(s: &mut Server, frames: usize, next: &mut Option<u64>, mut on_format: impl FnMut(aura_link::Segment)) {
    let mut buf = vec![0.0; 1024];
    let mut got = 0;
    let until = Instant::now() + Duration::from_secs(10);
    while got < frames {
        assert!(Instant::now() < until, "only {got} of {frames} frames came");
        match s.read(&mut buf) {
            Read::Samples(n) => {
                check(&buf[..n], next);
                got += n / 2;
            }
            Read::Format(f) => on_format(f),
            Read::Empty => {
                s.wait_data(Duration::from_millis(50));
            }
        }
    }
}

type GetClassObject = unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;
type CanUnloadNow = unsafe extern "system" fn() -> i32;

#[test]
fn a_player_plays_into_aura_player_through_the_driver() {
    let base = format!("AuraEngine.AudioLink.test.{}.host", std::process::id());
    std::env::set_var("AURA_LINK_BASE", &base);

    // SAFETY: the DLL this test built, called as COM and ASIO say.
    unsafe {
        let lib = LoadLibraryW(wide(dll().to_str().unwrap()).as_ptr());
        assert!(!lib.is_null(), "load {}", dll().display());
        let gco: GetClassObject = std::mem::transmute(GetProcAddress(lib, c"DllGetClassObject".as_ptr() as _).unwrap());
        let unload: CanUnloadNow = std::mem::transmute(GetProcAddress(lib, c"DllCanUnloadNow".as_ptr() as _).unwrap());
        for name in [c"DllRegisterServer", c"DllUnregisterServer"] {
            assert!(GetProcAddress(lib, name.as_ptr() as _).is_some(), "{name:?} exported");
        }

        let mut factory = std::ptr::null_mut();
        assert_eq!(gco(&CLSID, &IID_ICLASSFACTORY, &mut factory), 0);
        let fv = &**(factory as *const *const IClassFactoryVtbl);
        // As the SDK's host code does: the CLSID as the interface.
        let mut d = std::ptr::null_mut();
        assert_eq!((fv.create_instance)(factory, std::ptr::null_mut(), &CLSID, &mut d), 0);
        let v = &**(d as *const *const IAsioVtbl);
        assert_eq!(unload(), 1, "a live driver keeps the DLL");

        // No Aura Player yet: init fails and says why.
        assert_eq!((v.init)(d, std::ptr::null_mut()), ASIO_FALSE);
        let mut msg = [0u8; 124];
        (v.get_error_message)(d, msg.as_mut_ptr());
        assert!(c_str(&msg).starts_with("Aura Player isn't running"), "{}", c_str(&msg));

        let mut server = aura_link::win::create_server(&base, 1 << 16).unwrap();
        server.beat();
        assert_eq!((v.init)(d, std::ptr::null_mut()), ASIO_TRUE);
        let me = std::env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(server.client().unwrap().name, me);

        let mut name = [0u8; 32];
        (v.get_driver_name)(d, name.as_mut_ptr());
        assert_eq!(c_str(&name), "Aura Player ASIO");
        let (mut ins, mut outs) = (-1, -1);
        assert_eq!((v.get_channels)(d, &mut ins, &mut outs), ASE_OK);
        assert_eq!((ins, outs), (0, 2));
        assert_eq!((v.can_sample_rate)(d, 96_000.0), ASE_OK);
        assert_eq!((v.can_sample_rate)(d, 12_345.0), ASE_NO_CLOCK);
        assert_eq!((v.set_sample_rate)(d, 96_000.0), ASE_OK);
        let mut rate = 0.0;
        (v.get_sample_rate)(d, &mut rate);
        assert_eq!(rate, 96_000.0);
        let mut info = AsioChannelInfo { channel: 1, is_input: ASIO_FALSE, is_active: 9, channel_group: 9, sample_type: 0, name: [0; 32] };
        assert_eq!((v.get_channel_info)(d, &mut info), ASE_OK);
        assert_eq!((info.sample_type, info.is_active), (ASIOST_INT32_LSB, ASIO_FALSE));
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        (v.get_buffer_size)(d, &mut min, &mut max, &mut pref, &mut gran);
        assert!(min <= 256 && 256 <= max && pref > 0);

        // Buffers and callbacks, as a player makes them.
        const SIZE_FRAMES: i32 = 256;
        let mut infos = [
            AsioBufferInfo { is_input: ASIO_FALSE, channel_num: 0, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: ASIO_FALSE, channel_num: 1, buffers: [std::ptr::null_mut(); 2] },
        ];
        let mut cb = AsioCallbacks {
            buffer_switch: Some(buffer_switch),
            sample_rate_did_change: Some(sample_rate_did_change),
            asio_message: Some(asio_message),
            buffer_switch_time_info: Some(buffer_switch_time_info),
        };
        assert_eq!((v.create_buffers)(d, infos.as_mut_ptr(), 2, SIZE_FRAMES, &mut cb), ASE_OK);
        {
            let mut h = HALVES.lock().unwrap();
            for (c, i) in infos.iter().enumerate() {
                assert!(!i.buffers[0].is_null() && !i.buffers[1].is_null());
                h[c] = [i.buffers[0] as usize, i.buffers[1] as usize];
            }
        }
        SIZE.store(SIZE_FRAMES as usize, Release);
        (v.get_channel_info)(d, &mut info);
        assert_eq!(info.is_active, ASIO_TRUE);

        // Listening: every frame comes through, in order, bit for bit, in
        // the format the player set.
        let block = (SIZE_FRAMES * 2) as u64;
        server.set_target(block * 4);
        server.set_mode(Mode::Listening);
        assert_eq!((v.start)(d), ASE_OK);
        let mut next = None;
        let mut formats = Vec::new();
        read_frames(&mut server, 20 * SIZE_FRAMES as usize, &mut next, |f| formats.push((f.rate, f.channels, f.sample_type)));
        assert_eq!(formats, [(96_000.0, 2, ASIOST_INT32_LSB as u32)]);
        let flags = LAST_FLAGS.load(Acquire) as u32;
        assert_eq!(flags & (K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID), K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID);
        let (mut pos, mut stamp) = (Asio64::default(), Asio64::default());
        assert_eq!((v.get_sample_position)(d, &mut pos, &mut stamp), ASE_OK);
        assert!(u64::from(pos) >= 20 * SIZE_FRAMES as u64 && u64::from(stamp) > 0);

        // Held: the player stands still once the link is full; nothing lost.
        server.set_mode(Mode::Held);
        std::thread::sleep(Duration::from_millis(100));
        let c1 = CALLS.load(Acquire);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(CALLS.load(Acquire), c1, "the player was called while held");
        server.set_mode(Mode::Listening);
        read_frames(&mut server, 10 * SIZE_FRAMES as usize, &mut next, |_| panic!("no new format after a hold"));

        // Released: the player plays on by the system clock (≈375 calls a
        // second at 96 kHz and 256 frames), the samples dropped.
        server.set_mode(Mode::Released);
        std::thread::sleep(Duration::from_millis(50));
        let c1 = CALLS.load(Acquire);
        std::thread::sleep(Duration::from_millis(400));
        let calls = CALLS.load(Acquire) - c1;
        assert!((75..=300).contains(&calls), "{calls} calls in 400 ms by the system clock");
        let mut buf = [0.0; 64];
        assert_eq!(server.read(&mut buf), Read::Empty);
        // Listening again: from the newest sample, its format told again.
        server.set_mode(Mode::Listening);
        next = None;
        let mut told = 0;
        read_frames(&mut server, 5 * SIZE_FRAMES as usize, &mut next, |f| {
            told += 1;
            assert_eq!(f.rate, 96_000.0);
        });
        assert_eq!(told, 1);

        // A rate set while running starts a new segment between two blocks.
        assert_eq!((v.set_sample_rate)(d, 48_000.0), ASE_OK);
        let mut rates = Vec::new();
        read_frames(&mut server, 10 * SIZE_FRAMES as usize, &mut next, |f| rates.push(f.rate));
        assert_eq!(rates, [48_000.0]);

        // Stop, a new rate, start: the next segment in the new format, the
        // count going on.
        assert_eq!((v.stop)(d), ASE_OK);
        assert_eq!((v.set_sample_rate)(d, 44_100.0), ASE_OK);
        assert_eq!((v.start)(d), ASE_OK);
        let mut rates = Vec::new();
        read_frames(&mut server, 5 * SIZE_FRAMES as usize, &mut next, |f| rates.push(f.rate));
        assert_eq!(rates, [44_100.0]);

        // Aura Player goes away: the player plays on, as into an unplugged cable.
        drop(server);
        let c1 = CALLS.load(Acquire);
        std::thread::sleep(Duration::from_millis(300));
        assert!(CALLS.load(Acquire) > c1 + 20, "the player stopped when Aura Player went");

        assert_eq!((v.stop)(d), ASE_OK);
        assert_eq!((v.dispose_buffers)(d), ASE_OK);
        assert_eq!((v.release)(d), 0);
        assert_eq!(unload(), 0, "nothing keeps the DLL");
    }
}
