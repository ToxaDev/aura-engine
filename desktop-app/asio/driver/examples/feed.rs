// SPDX-License-Identifier: GPL-3.0-only
//! A player for checking the input without one: loads the built DLL by its
//! path, as a player loads a registered driver, and plays a quiet sine into
//! a **running** Aura Player through it — the link's own name, or
//! `--base` / `AURA_LINK_BASE` for a test instance.
//!
//! ```text
//! cargo run --example feed -- [--dll PATH] [--rate HZ] [--seconds S]
//!                             [--switch-to HZ --switch-at S] [--level DB]
//!                             [--tone HZ] [--block FRAMES] [--base NAME]
//!                             [--fifo BLOCKS]
//! ```
//!
//! The level is −40 dBFS at most (`--level` lower only): a check, not a
//! listening test. It prints, every second, how much audio the driver has
//! asked for against the wall clock — listened to, the two go together (the
//! link holds the player to Aura Player's pace); not listened to (Released),
//! the driver keeps time by the system clock all the same — and the output
//! latency the driver reports (Aura Player's delay to the sound, measured,
//! and the driver's buffer). When the driver stops asking (paused in Aura
//! Player) it says so, with the audio asked for and when the last block was:
//! what the player gave after the pause is the link's share of the delay.
//!
//! And how evenly the driver called: the intervals between its calls that
//! second against the buffer's period — mean, spread, longest, how many over
//! 1.5 periods — and the rate they keep in Hz, over the second and over the
//! last 10 s, as a bridge shows it. With `--fifo N` the player plays as a
//! bridge from a system device does (VB-Audio's ASIO Bridge): its source
//! fills a FIFO of N blocks by the system clock and each call takes a block
//! out; calls that come bunched run it dry (a block of silence played), a
//! long wait runs it over (a block dropped) — it counts both. Over time they
//! come only as often as the two clocks differ.

#[cfg(not(windows))]
fn main() {
    eprintln!("feed: the driver is Windows'");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = feed::run() {
        eprintln!("feed: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod feed {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use aura_player_asio::sys::*;
    use aura_player_asio::CLSID;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    /// The loudest it plays: −40 dBFS.
    const MAX_LEVEL_DB: f64 = -40.0;

    static HALVES: Mutex<[[usize; 2]; 2]> = Mutex::new([[0; 2]; 2]);
    static SIZE: AtomicUsize = AtomicUsize::new(0);
    /// Frames played, the tone's phase step (f64 bits), its amplitude (f64 bits).
    static FRAME: AtomicU64 = AtomicU64::new(0);
    static STEP: AtomicU64 = AtomicU64::new(0);
    static AMP: AtomicU64 = AtomicU64::new(0);
    static PHASE: Mutex<f64> = Mutex::new(0.0);
    /// The rate played at (f64 bits).
    static RATE: AtomicU64 = AtomicU64::new(0);
    static CALLS: Mutex<Calls> = Mutex::new(Calls::new());

    /// The driver's calls since the last report: the intervals between them,
    /// and the bridge's FIFO, if the player plays as one; and the calls of
    /// the last 10 s, for the rate they keep.
    struct Calls {
        last: Option<Instant>,
        n: u32,
        sum: f64,
        sum2: f64,
        max: f64,
        over: u32,
        fifo: Option<Fifo>,
        recent: VecDeque<Instant>,
    }

    impl Calls {
        const fn new() -> Calls {
            Calls { last: None, n: 0, sum: 0.0, sum2: 0.0, max: 0.0, over: 0, fifo: None, recent: VecDeque::new() }
        }

        fn call(&mut self, now: Instant, period: f64) {
            self.recent.push_back(now);
            while self.recent.front().is_some_and(|&t| (now - t).as_secs_f64() > 10.0) {
                self.recent.pop_front();
            }
            if let Some(t) = self.last {
                let d = (now - t).as_secs_f64();
                self.n += 1;
                self.sum += d;
                self.sum2 += d * d;
                self.max = self.max.max(d);
                if d > 1.5 * period {
                    self.over += 1;
                }
            }
            self.last = Some(now);
            if let Some(f) = self.fifo.as_mut() {
                f.call(now, period);
            }
        }

        /// The second's line, and a new second: the calls' intervals, and the
        /// rate they keep, in Hz as a bridge shows it — over the second, and
        /// over the last 10 s.
        fn report(&mut self, period: f64) -> String {
            let n = self.n.max(1) as f64;
            let mean = self.sum / n;
            let sd = (self.sum2 / n - mean * mean).max(0.0).sqrt();
            let size = SIZE.load(Acquire) as f64;
            let hz = |frames: f64, s: f64| if s > 0.0 { frames / s } else { 0.0 };
            let long = match (self.recent.front(), self.recent.back()) {
                (Some(&a), Some(&b)) => hz((self.recent.len() - 1) as f64 * size, (b - a).as_secs_f64()),
                _ => 0.0,
            };
            let mut s = format!(
                "feed:   calls {:4} every {:5.1} ms (sd {:5.1}, max {:5.1}; {} over 1.5 periods) for a period of {:.1} ms; {:.0} Hz (10 s: {:.1} Hz)",
                self.n,
                mean * 1e3,
                sd * 1e3,
                self.max * 1e3,
                self.over,
                period * 1e3,
                hz(self.n as f64 * size, self.sum),
                long
            );
            if let Some(f) = self.fifo.as_mut() {
                s += &format!("; FIFO of {} blocks: silence {}, dropped {}", f.blocks, f.dry, f.over);
                (f.dry, f.over) = (0, 0);
            }
            (self.n, self.sum, self.sum2, self.max, self.over) = (0, 0.0, 0.0, 0.0, 0);
            s
        }
    }

    /// A bridge's FIFO: filled by the system clock at the stream's rate, a
    /// block taken out at each call — when it holds one; else the call
    /// plays a block of silence (`dry`). A block too many is dropped
    /// (`over`). Starts half full.
    struct Fifo {
        blocks: u32,
        level: f64,
        at: Option<Instant>,
        dry: u32,
        over: u32,
    }

    impl Fifo {
        /// `period`: a block's time, the FIFO counted in it.
        fn call(&mut self, now: Instant, period: f64) {
            let cap = self.blocks as f64;
            let at = *self.at.get_or_insert(now);
            self.level += (now - at).as_secs_f64() / period;
            self.at = Some(now);
            if self.level > cap {
                self.over += 1;
                self.level -= 1.0;
            }
            if self.level < 1.0 {
                self.dry += 1;
            } else {
                self.level -= 1.0;
            }
        }
    }

    fn period() -> f64 {
        SIZE.load(Acquire) as f64 / f64::from_bits(RATE.load(Acquire)).max(1.0)
    }

    fn fill(index: i32) {
        CALLS.lock().unwrap().call(Instant::now(), period());
        let h = *HALVES.lock().unwrap();
        let size = SIZE.load(Acquire);
        let step = f64::from_bits(STEP.load(Acquire));
        let amp = f64::from_bits(AMP.load(Acquire));
        let mut ph = PHASE.lock().unwrap();
        for f in 0..size {
            let v = (amp * ph.sin() * 2_147_483_647.0).round() as i32;
            *ph += step;
            if *ph > std::f64::consts::TAU {
                *ph -= std::f64::consts::TAU;
            }
            // SAFETY: the halves the driver gave, `size` samples each.
            unsafe {
                *(h[0][index as usize] as *mut i32).add(f) = v;
                *(h[1][index as usize] as *mut i32).add(f) = v;
            }
        }
        FRAME.fetch_add(size as u64, AcqRel);
    }

    unsafe extern "C" fn buffer_switch(index: i32, _direct: AsioBool) {
        fill(index);
    }

    unsafe extern "C" fn buffer_switch_time_info(t: *mut AsioTime, index: i32, _direct: AsioBool) -> *mut AsioTime {
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

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn c_str(b: &[u8]) -> String {
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..n]).into_owned()
    }

    struct Args {
        dll: PathBuf,
        rate: f64,
        seconds: f64,
        switch: Option<(f64, f64)>,
        level_db: f64,
        tone: f64,
        block: i32,
        fifo: Option<u32>,
    }

    fn args() -> Result<Args, String> {
        // Beside the example, where cargo builds the library: target\<profile>\.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut a = Args {
            dll: exe.parent().and_then(|d| d.parent()).map(|d| d.join("aura_player_asio.dll")).unwrap_or_default(),
            rate: 44_100.0,
            seconds: 10.0,
            switch: None,
            level_db: MAX_LEVEL_DB,
            tone: 440.0,
            block: 2048,
            fifo: None,
        };
        let (mut to, mut at) = (None, None);
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let mut v = || it.next().ok_or(format!("{k} wants a value"));
            let num = |s: String| s.parse::<f64>().map_err(|_| format!("{k}: not a number: {s}"));
            match k.as_str() {
                "--dll" => a.dll = PathBuf::from(v()?),
                "--rate" => a.rate = num(v()?)?,
                "--seconds" => a.seconds = num(v()?)?,
                "--switch-to" => to = Some(num(v()?)?),
                "--switch-at" => at = Some(num(v()?)?),
                "--level" => a.level_db = num(v()?)?,
                "--tone" => a.tone = num(v()?)?,
                "--block" => a.block = num(v()?)? as i32,
                "--fifo" => a.fifo = Some((num(v()?)? as u32).max(2)),
                "--base" => std::env::set_var("AURA_LINK_BASE", v()?),
                _ => return Err(format!("unknown argument {k}")),
            }
        }
        if let Some(to) = to {
            a.switch = Some((to, at.unwrap_or(a.seconds / 2.0)));
        }
        if a.level_db > MAX_LEVEL_DB {
            println!("feed: {:.1} dBFS asked, {MAX_LEVEL_DB} dBFS played (the most this tool plays)", a.level_db);
            a.level_db = MAX_LEVEL_DB;
        }
        Ok(a)
    }

    type GetClassObject = unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;

    pub fn run() -> Result<(), String> {
        let a = args()?;
        let set_tone = |rate: f64| {
            STEP.store((std::f64::consts::TAU * a.tone / rate).to_bits(), Release);
            RATE.store(rate.to_bits(), Release);
            // A bridge's FIFO starts half full at each rate.
            CALLS.lock().unwrap().fifo =
                a.fifo.map(|blocks| Fifo { blocks, level: blocks as f64 / 2.0, at: None, dry: 0, over: 0 });
        };
        AMP.store(10f64.powf(a.level_db / 20.0).to_bits(), Release);
        set_tone(a.rate);
        // SAFETY: the DLL named, called as COM and ASIO say, as tests/host.rs does.
        unsafe {
            let lib = LoadLibraryW(wide(&a.dll.to_string_lossy()).as_ptr());
            if lib.is_null() {
                return Err(format!("cannot load {}", a.dll.display()));
            }
            let gco: GetClassObject = std::mem::transmute(
                GetProcAddress(lib, c"DllGetClassObject".as_ptr() as _).ok_or("no DllGetClassObject: not the driver")?,
            );
            let mut factory = std::ptr::null_mut();
            if gco(&CLSID, &IID_ICLASSFACTORY, &mut factory) != 0 {
                return Err("the driver's class factory refused".into());
            }
            let fv = &**(factory as *const *const IClassFactoryVtbl);
            let mut d = std::ptr::null_mut();
            if (fv.create_instance)(factory, std::ptr::null_mut(), &CLSID, &mut d) != 0 {
                return Err("the driver was not made".into());
            }
            (fv.release)(factory);
            let v = &**(d as *const *const IAsioVtbl);
            if (v.init)(d, std::ptr::null_mut()) != ASIO_TRUE {
                let mut msg = [0u8; 124];
                (v.get_error_message)(d, msg.as_mut_ptr());
                (v.release)(d);
                return Err(c_str(&msg));
            }
            if (v.set_sample_rate)(d, a.rate) != ASE_OK {
                (v.release)(d);
                return Err(format!("{} Hz is not a rate the driver takes", a.rate));
            }
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
            if (v.create_buffers)(d, infos.as_mut_ptr(), 2, a.block, &mut cb) != ASE_OK {
                (v.release)(d);
                return Err(format!("buffers of {} frames refused", a.block));
            }
            {
                let mut h = HALVES.lock().unwrap();
                for (c, i) in infos.iter().enumerate() {
                    h[c] = [i.buffers[0] as usize, i.buffers[1] as usize];
                }
            }
            SIZE.store(a.block as usize, Release);
            let (mut lat_in, mut lat_out) = (0, 0);
            (v.get_latencies)(d, &mut lat_in, &mut lat_out);
            println!(
                "feed: {} Hz, {:.0} Hz tone at {:.1} dBFS, {} s, blocks of {} frames; output latency {} frames",
                a.rate, a.tone, a.level_db, a.seconds, a.block, lat_out
            );
            if (v.start)(d) != ASE_OK {
                (v.dispose_buffers)(d);
                (v.release)(d);
                return Err("start refused".into());
            }
            let t0 = Instant::now();
            let mut rate = a.rate;
            let mut switched = a.switch.is_none();
            let mut at_frames = 0u64;
            let mut audio_s = 0.0;
            let mut next = 1.0;
            // The last block asked for: frames then, and when; held — the
            // driver stopped asking.
            let (mut last_frames, mut last_at, mut held) = (0u64, 0.0f64, false);
            while t0.elapsed().as_secs_f64() < a.seconds {
                std::thread::sleep(Duration::from_millis(10));
                let frames = FRAME.load(Acquire);
                let t = t0.elapsed().as_secs_f64();
                if frames != last_frames {
                    if held {
                        held = false;
                        println!("feed: asked again at {:.3} s, after {:.3} s held", t, t - last_at);
                    }
                    last_frames = frames;
                    last_at = t;
                } else if !held && t - last_at >= 0.25 {
                    held = true;
                    let s = audio_s + (frames - at_frames) as f64 / rate;
                    println!("feed: held: {:.3} s of audio asked for, the last block at {:.3} s", s, last_at);
                }
                let now_s = audio_s + (frames - at_frames) as f64 / rate;
                if !switched && t0.elapsed().as_secs_f64() >= a.switch.unwrap().1 {
                    switched = true;
                    let to = a.switch.unwrap().0;
                    if (v.set_sample_rate)(d, to) != ASE_OK {
                        println!("feed: {to} Hz refused");
                    } else {
                        // The time played so far at the old rate, then on at the new one.
                        audio_s = now_s;
                        at_frames = frames;
                        rate = to;
                        set_tone(to);
                        println!("feed: now at {to} Hz");
                    }
                }
                if t0.elapsed().as_secs_f64() >= next {
                    next += 1.0;
                    let (mut lat_in, mut lat_out) = (0, 0);
                    (v.get_latencies)(d, &mut lat_in, &mut lat_out);
                    println!(
                        "feed: {:5.1} s of audio asked for in {:5.1} s; output latency {:.3} s",
                        now_s,
                        t0.elapsed().as_secs_f64(),
                        lat_out as f64 / rate
                    );
                    let line = CALLS.lock().unwrap().report(period());
                    println!("{line}");
                }
            }
            (v.stop)(d);
            (v.dispose_buffers)(d);
            (v.release)(d);
            println!("feed: stopped");
        }
        Ok(())
    }
}
