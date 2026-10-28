// SPDX-License-Identifier: GPL-3.0-only
//! The driver's own clock, through the built DLL: a player at 48 kHz with
//! a bridge's buffer (2048 frames), and Aura Player's end of the link in
//! this process read as Aura Player reads it — evenly, by a tick of 2 ms, a
//! late tick caught up — standing still once for 150 ms. With the reader
//! even (the server says so) the player's calls go on once a period through
//! the stand; without, the driver calls when room is made and the stand
//! reaches the player — and its account tells the server why.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aura_link::{LateCall, Mode, Read, Server};
use aura_player_asio::sys::*;
use aura_player_asio::CLSID;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const RATE: f64 = 48_000.0;
const SIZE_FRAMES: i32 = 2_048;

static CALLS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
static HALVES: Mutex<[[usize; 2]; 2]> = Mutex::new([[0; 2]; 2]);
static FRAME: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn buffer_switch(index: i32, _direct: AsioBool) {
    CALLS.lock().unwrap().push(Instant::now());
    let h = *HALVES.lock().unwrap();
    let n0 = FRAME.fetch_add(SIZE_FRAMES as usize, AcqRel);
    for f in 0..SIZE_FRAMES as usize {
        // SAFETY: the halves the driver gave, SIZE_FRAMES samples each.
        unsafe {
            *(h[0][index as usize] as *mut i32).add(f) = (n0 + f) as i32;
            *(h[1][index as usize] as *mut i32).add(f) = (n0 + f) as i32;
        }
    }
}

unsafe extern "C" fn sample_rate_did_change(_rate: f64) {}

unsafe extern "C" fn asio_message(_selector: i32, _value: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    0
}

fn dll() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().join("aura_player_asio.dll")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

type GetClassObject = unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;

/// Reads the link for `secs` as Aura Player's reader does: every 2 ms what
/// the time since the last tick is worth at the stream's rate (up to 0.25 s
/// owed), standing still from `stand.0` s for `stand.1` s. Every frame in
/// order, or it says which went missing.
fn read_evenly(server: &mut Server, secs: f64, stand: (f64, f64), next: &mut usize) {
    let t0 = Instant::now();
    let mut last = t0;
    let mut owed = 0.0f64;
    let mut buf = vec![0.0; 1 << 16];
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_millis(2));
        // The heartbeat goes on while the reader stands (Aura Player's does).
        server.beat();
        let now = Instant::now();
        let t = now.duration_since(t0).as_secs_f64();
        owed = (owed + now.duration_since(last).as_secs_f64() * RATE * 2.0).min(0.25 * RATE * 2.0);
        last = now;
        if t >= stand.0 && t < stand.0 + stand.1 {
            continue;
        }
        while owed >= 2.0 {
            let n = ((owed as usize) / 2 * 2).min(buf.len());
            match server.read(&mut buf[..n]) {
                Read::Samples(k) => {
                    for fr in buf[..k].chunks_exact(2) {
                        let v = (fr[0] * 2_147_483_648.0) as i64 as usize;
                        assert_eq!(v, *next, "a frame lost or doubled");
                        *next += 1;
                    }
                    owed -= k as f64;
                }
                Read::Format(_) => {}
                Read::Empty => break,
            }
        }
    }
}

/// The longest gap between the player's calls from `from` on, s.
fn longest_gap(from: Instant) -> f64 {
    let c = CALLS.lock().unwrap();
    c.windows(2).filter(|w| w[0] >= from).map(|w| w[1].duration_since(w[0]).as_secs_f64()).fold(0.0, f64::max)
}

#[test]
fn the_player_is_called_evenly_while_the_reader_stands() {
    let base = format!("AuraEngine.AudioLink.test.{}.even", std::process::id());
    std::env::set_var("AURA_LINK_BASE", &base);
    let period = SIZE_FRAMES as f64 / RATE;
    // SAFETY: the DLL this test built, called as COM and ASIO say.
    unsafe {
        let lib = LoadLibraryW(wide(dll().to_str().unwrap()).as_ptr());
        assert!(!lib.is_null(), "load {}", dll().display());
        let gco: GetClassObject = std::mem::transmute(GetProcAddress(lib, c"DllGetClassObject".as_ptr() as _).unwrap());
        let mut factory = std::ptr::null_mut();
        assert_eq!(gco(&CLSID, &IID_ICLASSFACTORY, &mut factory), 0);
        let fv = &**(factory as *const *const IClassFactoryVtbl);
        let mut d = std::ptr::null_mut();
        assert_eq!((fv.create_instance)(factory, std::ptr::null_mut(), &CLSID, &mut d), 0);
        let v = &**(d as *const *const IAsioVtbl);

        let mut server = aura_link::win::create_server(&base, aura_link::win::RING_SAMPLES).unwrap();
        server.beat();
        assert_eq!((v.init)(d, std::ptr::null_mut()), ASIO_TRUE);
        assert_eq!((v.set_sample_rate)(d, RATE), ASE_OK);
        let mut infos = [
            AsioBufferInfo { is_input: ASIO_FALSE, channel_num: 0, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: ASIO_FALSE, channel_num: 1, buffers: [std::ptr::null_mut(); 2] },
        ];
        let mut cb = AsioCallbacks {
            buffer_switch: Some(buffer_switch),
            sample_rate_did_change: Some(sample_rate_did_change),
            asio_message: Some(asio_message),
            buffer_switch_time_info: None,
        };
        assert_eq!((v.create_buffers)(d, infos.as_mut_ptr(), 2, SIZE_FRAMES, &mut cb), ASE_OK);
        *HALVES.lock().unwrap() = [
            [infos[0].buffers[0] as usize, infos[0].buffers[1] as usize],
            [infos[1].buffers[0] as usize, infos[1].buffers[1] as usize],
        ];
        // Aura Player's target: 0.05 s ahead of its reader.
        server.set_target((0.05 * RATE) as u64 * 2);
        server.set_mode(Mode::Listening);
        server.set_even(true);
        assert_eq!((v.start)(d), ASE_OK);
        let mut next = 0usize;

        // Even: the stand is the reserve's, not the player's.
        let t0 = Instant::now();
        read_evenly(&mut server, 1.0, (9.0, 0.0), &mut next);
        let warm = Instant::now();
        read_evenly(&mut server, 2.0, (0.8, 0.15), &mut next);
        let even_gap = longest_gap(warm);
        let st = server.call_stats();
        let (mut seen, mut late) = (0u64, Vec::<LateCall>::new());
        server.late_calls(&mut seen, &mut late);
        println!(
            "even: the longest gap {:.1} ms for a period of {:.1} ms; {} calls, {} late, the clock {:+} ppm, waited {} µs",
            even_gap * 1e3,
            period * 1e3,
            st.calls,
            st.late,
            st.clock_ppm,
            st.waited_us
        );
        assert!(st.clocked, "the driver keeps its own clock");
        assert!(even_gap < 1.5 * period, "the player heard the stand: a gap of {:.1} ms", even_gap * 1e3);
        assert!(late.is_empty(), "late calls: {late:?}");
        assert!(t0.elapsed() > Duration::from_secs(2));

        // Not even (an older Aura Player, or AURA_INPUT_DRIVER_CLOCK=0): the
        // driver calls when room is made — the stand reaches the player, and
        // the account says it waited for room.
        server.set_even(false);
        read_evenly(&mut server, 0.5, (9.0, 0.0), &mut next);
        let from = Instant::now();
        read_evenly(&mut server, 1.0, (0.4, 0.15), &mut next);
        let room_gap = longest_gap(from);
        late.clear();
        server.late_calls(&mut seen, &mut late);
        println!("by room: the longest gap {:.1} ms; late {late:?}", room_gap * 1e3);
        assert!(!server.call_stats().clocked);
        assert!(room_gap > 0.12, "the stand reached the player: {:.1} ms", room_gap * 1e3);
        let worst = late.iter().max_by_key(|c| c.gap_us).expect("the late call counted");
        assert!(worst.gap_us > 120_000 && worst.waited_us > 100_000, "it waited for room: {worst:?}");

        assert_eq!((v.stop)(d), ASE_OK);
        assert_eq!((v.dispose_buffers)(d), ASE_OK);
        assert_eq!((v.release)(d), 0);
    }
}
