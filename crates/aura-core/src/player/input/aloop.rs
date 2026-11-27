//! The input on Linux: what another program plays into the ALSA Loopback
//! (`snd-aloop`), played through the chain as Aura Player ASIO's is on
//! Windows.
//!
//! A program — HQPlayer, Roon Bridge, mpd, squeezelite, or any through
//! PipeWire ("Loopback Analog Stereo") — plays into `hw:Loopback,0,0`; this
//! client captures the cable's other end (`hw:Loopback,1,0`) and puts it in
//! the input's link (`link`, over memory in this process), the same link the
//! ASIO driver writes on Windows: the reader, the pace, the session and the
//! Input view are the very same.
//!
//! * The clock: the Loopback keeps its own (a system timer), the reader the
//!   DAC's (`super::pace`). The ASIO driver makes the player wait for room
//!   in the link; a capture cannot wait. So the Loopback is sped up or
//!   slowed down (its "PCM Rate Shift 100000" control, which moves both of
//!   the cable's ends: the player is called as much faster or slower), by a
//!   slow loop on what waits for the reader here (`Trim`) — no resampling,
//!   nothing cut or repeated, as CamillaDSP does over the same cable.
//! * The format is the player's (the cable's "PCM Slave …" controls): the
//!   capture is opened at its rate and sample format, stereo. A new format
//!   ("PCM Notify" stops the capture) is a new segment.
//! * What the link has no room for waits here (`Backlog`), up to a second:
//!   the reader takes in steps until the chain is built. Released (not
//!   listened to), the capture goes on and is dropped, as a player plays on
//!   into an unplugged cable; Held, it is dropped past the second too — a
//!   capture cannot hold the player still.
//!
//! The Loopback is the kernel's: `sudo modprobe snd-aloop` once (and a line
//! in /etc/modules-load.d to keep it), which the Input view says.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::VecDeque;

/// The Loopback card's id (`/proc/asound/Loopback`).
pub const CARD: &str = "Loopback";
/// Players play into this device of the card; the capture is the other.
pub const PLAY_DEV: u32 = 0;
pub const CAPTURE_DEV: u32 = 1;
/// The cable (substream) used: the first.
pub const SUB: u32 = 0;

/// What a player chooses, in its words.
pub const PLAY_INTO: &str = "hw:Loopback,0,0";

// ── Formats ──────────────────────────────────────────────────────────────────

/// `snd_pcm_format_t` values the cable reports ("PCM Slave Format").
pub mod alsa_format {
    pub const S16_LE: i32 = 2;
    pub const S24_LE: i32 = 6;
    pub const S32_LE: i32 = 10;
    pub const FLOAT_LE: i32 = 14;
    pub const S24_3LE: i32 = 32;
}

/// A sample format the capture takes, as the player plays it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapFmt {
    S16,
    /// 24 bits in the low three bytes of four (ALSA's S24_LE).
    S24In32,
    /// 24 bits in three bytes.
    S24Packed,
    S32,
    Float,
}

impl CapFmt {
    pub fn from_alsa(v: i32) -> Option<CapFmt> {
        use alsa_format::*;
        Some(match v {
            S16_LE => CapFmt::S16,
            S24_LE => CapFmt::S24In32,
            S24_3LE => CapFmt::S24Packed,
            S32_LE => CapFmt::S32,
            FLOAT_LE => CapFmt::Float,
            _ => return None,
        })
    }

    /// ALSA's value of it.
    pub fn alsa(self) -> i32 {
        use alsa_format::*;
        match self {
            CapFmt::S16 => S16_LE,
            CapFmt::S24In32 => S24_LE,
            CapFmt::S24Packed => S24_3LE,
            CapFmt::S32 => S32_LE,
            CapFmt::Float => FLOAT_LE,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CapFmt::S16 => "S16_LE",
            CapFmt::S24In32 => "S24_LE",
            CapFmt::S24Packed => "S24_3LE",
            CapFmt::S32 => "S32_LE",
            CapFmt::Float => "FLOAT_LE",
        }
    }

    /// Bytes a sample.
    pub fn bytes(self) -> usize {
        match self {
            CapFmt::S16 => 2,
            CapFmt::S24Packed => 3,
            _ => 4,
        }
    }

    /// The ASIO sample type the link and the Input view name it by
    /// (Int16LSB, Int24LSB, Int32LSB, Float32LSB).
    pub fn sample_type(self) -> u32 {
        match self {
            CapFmt::S16 => 16,
            CapFmt::S24In32 | CapFmt::S24Packed => 17,
            CapFmt::S32 => 18,
            CapFmt::Float => 19,
        }
    }

    /// A sample (`b`, `bytes()` long) as the f64 the link carries: full
    /// scale ±1, exact for every integer format.
    pub fn to_f64(self, b: &[u8]) -> f64 {
        match self {
            CapFmt::S16 => i16::from_le_bytes([b[0], b[1]]) as f64 / 32_768.0,
            CapFmt::S24Packed => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f64 / 8_388_608.0,
            CapFmt::S24In32 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f64 / 8_388_608.0,
            CapFmt::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 / 2_147_483_648.0,
            CapFmt::Float => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        }
    }
}

/// What the cable says of the player's end ("PCM Slave …").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slave {
    pub active: bool,
    pub format: i32,
    pub rate: u32,
    pub channels: u32,
}

impl Slave {
    /// The format the capture opens in, or why it cannot.
    pub fn capture(&self) -> Result<CapFmt, String> {
        if self.channels != 2 {
            return Err(format!("the player plays {} channels into the Loopback; the input takes stereo", self.channels));
        }
        if !(8_000..=1_536_000).contains(&self.rate) {
            return Err(format!("the player plays {} Hz into the Loopback", self.rate));
        }
        CapFmt::from_alsa(self.format).ok_or_else(|| {
            format!("the player plays sample format {} into the Loopback; the input takes 16, 24 or 32-bit and float", self.format)
        })
    }
}

/// `owner_pid` from `/proc/asound/cardN/pcmDp/subS/status` (the player's
/// end of the cable, open).
pub fn owner_pid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "owner_pid").then(|| v.trim().parse().ok()).flatten()
    })
}

/// The name the Input view shows for the program on the cable (`comm`).
pub fn player_name(comm: Option<&str>) -> String {
    match comm.map(str::trim) {
        Some("pipewire" | "pipewire-pulse" | "wireplumber") => "System sound (PipeWire)".to_string(),
        Some("pulseaudio") => "System sound (PulseAudio)".to_string(),
        Some(c) if !c.is_empty() => c.to_string(),
        _ => "Loopback".to_string(),
    }
}

// ── What waits for the link ──────────────────────────────────────────────────

/// Samples captured that the link had no room for yet, interleaved; past
/// `cap` the oldest go.
pub struct Backlog {
    q: VecDeque<f64>,
    cap: usize,
    /// Samples dropped for want of room, since the segment began.
    pub dropped: u64,
}

impl Backlog {
    pub fn new(cap: usize) -> Backlog {
        Backlog { q: VecDeque::with_capacity(cap), cap, dropped: 0 }
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn clear(&mut self) {
        self.q.clear();
    }

    /// Captured samples in, the oldest dropped past the cap.
    pub fn push(&mut self, x: impl IntoIterator<Item = f64>) {
        for v in x {
            if self.q.len() == self.cap {
                self.q.pop_front();
                self.dropped += 1;
            }
            self.q.push_back(v);
        }
    }

    /// The oldest `n` (whole frames: the caller asks so), out.
    pub fn take(&mut self, n: usize) -> impl Iterator<Item = f64> + '_ {
        let n = n.min(self.q.len());
        self.q.drain(..n)
    }
}

/// Of `n` samples waiting, the whole stereo frames the link takes now (its
/// room rule, `aura_link::Client::room`): up to its target past what it
/// holds (`used`), or a whole ring when empty.
pub fn room_for(n: usize, used: u64, target: u64, ring: u64) -> usize {
    let room = if used == 0 { ring.max(target) } else { target.saturating_sub(used) };
    (n.min(room.min(usize::MAX as u64) as usize)) / 2 * 2
}

// ── The clock ────────────────────────────────────────────────────────────────

/// "PCM Rate Shift 100000": the Loopback's pace, 100000 its own clock;
/// more is slower (the cable moves `100000 / shift` as many bytes a tick).
pub const NO_SHIFT: i32 = 100_000;

/// The Loopback's pace for a trim of `ppm` (positive: slower).
pub fn shift_for(ppm: f64) -> i32 {
    (NO_SHIFT as f64 * (1.0 + ppm * 1e-6)).round() as i32
}

/// The loop that keeps the Loopback at the reader's pace: what waits for
/// the reader — in the link and here — held at `aim` seconds. Too much
/// waiting: the Loopback runs fast against the DAC; it is slowed by as many
/// parts per million as the level is over, plus the sum over time (the
/// drift itself, once found). Both slow: the level is smoothed over a
/// second, and the loop settles over a few minutes (critically damped, a
/// time constant of 50 s), so the player is never called in jerks. Learns only while the reader takes at
/// its even pace.
pub struct Trim {
    pub aim_s: f64,
    /// The level, smoothed (s).
    level: Option<f64>,
    /// The drift found (ppm).
    integral: f64,
    pub ppm: f64,
}

/// ppm a second of level over the aim (the proportional part).
const KP: f64 = 40_000.0;
/// ppm a second, a second of level over the aim (the integral part):
/// critically damped against `KP`.
const KI: f64 = KP * KP * 1e-6 / 4.0;
/// The trim never goes past this: a Loopback whose clock is off by more is
/// broken, and a player called that far off its rate would mind.
pub const MAX_PPM: f64 = 2_000.0;
/// The level's smoothing, s.
const SMOOTH_S: f64 = 1.0;

impl Trim {
    pub fn new(aim_s: f64) -> Trim {
        Trim { aim_s, level: None, integral: 0.0, ppm: 0.0 }
    }

    /// The level now (`level_s`, what waits), `dt` after the last look;
    /// `learn` while the reader takes evenly. The trim, ppm.
    pub fn update(&mut self, level_s: f64, dt: f64, learn: bool) -> f64 {
        let a = (dt / SMOOTH_S).clamp(0.0, 1.0);
        let l = match self.level {
            Some(l) => l + a * (level_s - l),
            None => level_s,
        };
        self.level = Some(l);
        if !learn {
            return self.ppm;
        }
        let e = l - self.aim_s;
        let p = KP * e;
        // No winding up past the edge.
        let i = (self.integral + KI * e * dt).clamp(-MAX_PPM, MAX_PPM);
        self.integral = i;
        self.ppm = (p + i).clamp(-MAX_PPM, MAX_PPM);
        self.ppm
    }

    /// The smoothed level (s), once looked at.
    pub fn level(&self) -> Option<f64> {
        self.level
    }
}

// ── The capture (Linux) ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub use sys::{loaded, spawn};

#[cfg(target_os = "linux")]
mod sys {
    use super::*;
    use std::ffi::CString;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::{Direction, ValueOr};
    use aura_link::{Client, HeapRegion, LocalSignals, Mode};

    /// The server's beat must move within this, or it counts as gone.
    const ALIVE: Duration = Duration::from_secs(1);
    /// A capture period, s: small, so what waits here moves evenly.
    const PERIOD_S: f64 = 0.005;
    /// The capture's buffer, s: the thread can be late by this much.
    const BUFFER_S: f64 = 0.2;
    /// What waits here at most, s (`Backlog`).
    const BACKLOG_S: f64 = 1.0;
    /// The trim is looked at this often.
    const TRIM_EVERY: Duration = Duration::from_millis(50);
    /// The cable's controls are read this often while a player plays.
    const LOOK_EVERY: Duration = Duration::from_millis(250);

    /// Whether the Loopback is there (`snd-aloop` loaded).
    pub fn loaded() -> bool {
        std::path::Path::new("/proc/asound").join(CARD).exists()
    }

    /// The Loopback's capture, putting what a player plays in the link
    /// over `region` (the server's, in this process), on a thread of its own.
    pub fn spawn(region: Arc<HeapRegion>, signals: LocalSignals) {
        let r = std::thread::Builder::new().name("aura-loopback".into()).spawn(move || run(region, signals));
        if let Err(e) = r {
            crate::aelog!("[INPUT] loopback: the capture's thread could not start: {e}");
        }
    }

    fn run(region: Arc<HeapRegion>, signals: LocalSignals) {
        let mut said = String::new();
        let mut say = |s: String| {
            if s != said {
                crate::aelog!("[INPUT] loopback: {s}");
                said = s;
            }
        };
        loop {
            let Some(ctl) = open_ctl() else {
                say("not loaded (sudo modprobe snd-aloop)".into());
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let cable = Cable(ctl);
            let slave = match cable.slave() {
                Ok(s) => s,
                Err(e) => {
                    say(format!("its controls cannot be read: {e}"));
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            if !slave.active {
                say("waiting for a player".into());
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let fmt = match slave.capture() {
                Ok(f) => f,
                Err(e) => {
                    say(e);
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };
            say(format!("a player plays {} Hz {} in", slave.rate, fmt.name()));
            if let Err(e) = play_through(&cable, slave, fmt, &region, &signals) {
                say(format!("the capture ended: {e}"));
                std::thread::sleep(Duration::from_millis(200));
            }
            cable.set_shift(NO_SHIFT);
        }
    }

    fn open_ctl() -> Option<Ctl> {
        if !loaded() {
            return None;
        }
        Ctl::new(&format!("hw:{CARD}"), false).ok()
    }

    /// The cable's controls (device `CAPTURE_DEV`, substream `SUB`: the
    /// cable a player on `PLAY_DEV` plays into).
    struct Cable(Ctl);

    impl Cable {
        fn id(name: &str) -> ElemId {
            let mut id = ElemId::new(ElemIface::PCM);
            id.set_device(CAPTURE_DEV);
            id.set_subdevice(SUB);
            id.set_name(&CString::new(name).expect("no NUL"));
            id
        }

        fn read(&self, name: &str, ty: ElemType) -> alsa::Result<ElemValue> {
            let mut v = ElemValue::new(ty)?;
            v.set_id(&Self::id(name));
            self.0.elem_read(&mut v)?;
            Ok(v)
        }

        fn int(&self, name: &str) -> alsa::Result<i32> {
            Ok(self.read(name, ElemType::Integer)?.get_integer(0).unwrap_or(0))
        }

        fn slave(&self) -> alsa::Result<Slave> {
            Ok(Slave {
                active: self.read("PCM Slave Active", ElemType::Boolean)?.get_boolean(0).unwrap_or(false),
                format: self.int("PCM Slave Format")?,
                rate: self.int("PCM Slave Rate")?.max(0) as u32,
                channels: self.int("PCM Slave Channels")?.max(0) as u32,
            })
        }

        fn write(&self, name: &str, ty: ElemType, set: impl FnOnce(&mut ElemValue)) -> alsa::Result<()> {
            let mut v = ElemValue::new(ty)?;
            v.set_id(&Self::id(name));
            set(&mut v);
            self.0.elem_write(&v)
        }

        fn set_shift(&self, shift: i32) {
            let _ = self.write("PCM Rate Shift 100000", ElemType::Integer, |v| {
                v.set_integer(0, shift.clamp(80_000, 120_000));
            });
        }

        /// The capture is stopped when the player's format changes.
        fn set_notify(&self) {
            let _ = self.write("PCM Notify", ElemType::Boolean, |v| {
                v.set_boolean(0, true);
            });
        }
    }

    fn format_of(f: CapFmt) -> Format {
        match f {
            CapFmt::S16 => Format::S16LE,
            CapFmt::S24In32 => Format::S24LE,
            CapFmt::S24Packed => Format::S243LE,
            CapFmt::S32 => Format::S32LE,
            CapFmt::Float => Format::FloatLE,
        }
    }

    /// The program on the cable's other end, from `/proc/asound`.
    fn player() -> String {
        let status = std::fs::read_to_string(format!("/proc/asound/{CARD}/pcm{PLAY_DEV}p/sub{SUB}/status")).ok();
        let comm = status
            .as_deref()
            .and_then(owner_pid)
            .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok());
        player_name(comm.as_deref())
    }

    /// One run of a player into the cable, in one format: captured into the
    /// link until the player stops or changes its format.
    fn play_through(cable: &Cable, slave: Slave, fmt: CapFmt, region: &Arc<HeapRegion>, signals: &LocalSignals) -> Result<(), String> {
        let rate = slave.rate;
        let dev = format!("hw:CARD={CARD},DEV={CAPTURE_DEV},SUBDEV={SUB}");
        let pcm = PCM::new(&dev, Direction::Capture, true).map_err(|e| format!("{dev}: {e}"))?;
        {
            let hwp = HwParams::any(&pcm).map_err(|e| e.to_string())?;
            let fail = |e: alsa::Error| format!("{dev}: {} {rate} Hz refused ({e})", fmt.name());
            hwp.set_access(Access::RWInterleaved).map_err(fail)?;
            hwp.set_channels(2).map_err(fail)?;
            hwp.set_format(format_of(fmt)).map_err(fail)?;
            hwp.set_rate(rate, ValueOr::Nearest).map_err(fail)?;
            let period = ((PERIOD_S * rate as f64) as alsa::pcm::Frames).max(32);
            hwp.set_period_size_near(period, ValueOr::Nearest).map_err(fail)?;
            hwp.set_buffer_size_near(((BUFFER_S * rate as f64) as alsa::pcm::Frames).max(4 * period)).map_err(fail)?;
            pcm.hw_params(&hwp).map_err(fail)?;
        }
        cable.set_notify();
        pcm.start().map_err(|e| format!("{dev}: start ({e})"))?;

        let mut client = Client::open(Box::new(region.clone()), Box::new(signals.clone()), std::process::id(), |_| false)
            .map_err(|e| format!("the link: {e}"))?;
        let name = player();
        client.set_name(&name);
        client.begin(rate as f64, 2, fmt.sample_type());
        crate::aelog!("[INPUT] loopback: {name} plays {rate} Hz {} into {PLAY_INTO}", fmt.name());

        let frame_bytes = 2 * fmt.bytes();
        let mut bytes = vec![0u8; frame_bytes * ((BUFFER_S * rate as f64) as usize).max(1024)];
        let mut backlog = Backlog::new(2 * (BACKLOG_S * rate as f64) as usize);
        let mut trim = Trim::new(client.target() as f64 / 2.0 / 2.0 / rate as f64);
        let (mut trim_at, mut look_at) = (Instant::now(), Instant::now());
        let mut shift = NO_SHIFT;
        let mut xruns = 0u32;
        let io = pcm.io_bytes();
        loop {
            match pcm.wait(Some(100)) {
                Ok(_) => {}
                Err(e) => return Err(format!("wait ({e})")),
            }
            match io.readi(&mut bytes) {
                Ok(n) => {
                    let got = &bytes[..n * frame_bytes];
                    backlog.push(got.chunks_exact(fmt.bytes()).map(|s| fmt.to_f64(s)));
                }
                Err(e) if e.errno() == libc::EAGAIN => {}
                Err(e) if e.errno() == libc::EPIPE => {
                    // The thread was late past the capture's buffer.
                    xruns += 1;
                    crate::aelog!("[INPUT] loopback: capture overrun #{xruns}");
                    pcm.prepare().map_err(|e| format!("prepare ({e})"))?;
                    pcm.start().map_err(|e| format!("start ({e})"))?;
                }
                // The player stopped or changed its format (`PCM Notify`).
                Err(e) => return Err(format!("{name} stopped ({e})")),
            }
            let mode = client.mode(ALIVE);
            match mode {
                Mode::Released => backlog.clear(),
                Mode::Listening | Mode::Held => {
                    // Whole frames into the link, as far as it has room.
                    let fit = room_for(backlog.len(), client.used(), client.target(), client.ring_samples());
                    if fit > 0 {
                        debug_assert!(client.room(fit as u64));
                        let mut it = backlog.take(fit);
                        client.write_with(fit, |_| it.next().unwrap_or(0.0));
                    }
                }
            }
            let dt = trim_at.elapsed();
            if dt >= TRIM_EVERY {
                trim_at = Instant::now();
                let level_s = (client.used() as f64 + backlog.len() as f64) / 2.0 / rate as f64;
                let ppm = match mode {
                    Mode::Listening => trim.update(level_s, dt.as_secs_f64(), client.even()),
                    _ => trim.ppm,
                };
                let s = shift_for(ppm);
                if s != shift {
                    shift = s;
                    cable.set_shift(s);
                }
            }
            if look_at.elapsed() >= LOOK_EVERY {
                look_at = Instant::now();
                match cable.slave() {
                    Ok(s) if s == slave => {}
                    Ok(s) if !s.active => return Ok(()),
                    Ok(_) => return Ok(()), // another format: a new segment
                    Err(e) => return Err(format!("the cable's controls ({e})")),
                }
            }
        }
    }
}

/// With `snd-aloop` loaded (`sudo modprobe snd-aloop`; CI's port-check
/// loads it): a player playing into `hw:Loopback,0,0` comes out of the
/// link bit for bit, one segment in its format, named.
///
///     cargo test --profile fast -p aura-core --lib aloop::loopback_tests -- --ignored --nocapture
#[cfg(all(test, target_os = "linux"))]
mod loopback_tests {
    use super::*;
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::{Direction, ValueOr};
    use aura_link::{region_bytes, HeapRegion, LocalSignals, Mode, Read, Server};
    use std::time::{Duration, Instant};

    /// The test signal: frame `i`'s left and right, distinct and never
    /// silence.
    fn sample(i: usize, ch: usize) -> i32 {
        ((i as i32).wrapping_mul(65_536) | 0x100 | ch as i32) ^ 0x5a5a_0000
    }

    #[test]
    #[ignore]
    fn a_player_into_the_loopback_comes_out_of_the_link() {
        if !loaded() {
            eprintln!("no ALSA Loopback here (sudo modprobe snd-aloop): skipped");
            return;
        }
        const RATE: u32 = 48_000;
        const SECONDS: usize = 3;
        let ring = 1 << 18;
        let region = HeapRegion::new(region_bytes(ring));
        let signals = LocalSignals::default();
        let mut server = Server::create(Box::new(region.clone()), Box::new(signals.clone()), ring, std::process::id());
        server.set_target((0.05 * RATE as f64) as u64 * 2);
        server.set_mode(Mode::Listening);
        server.set_even(true);
        spawn(region, signals);

        let player = std::thread::spawn(|| {
            let pcm = PCM::new(PLAY_INTO, Direction::Playback, false).expect("open hw:Loopback,0,0");
            {
                let hwp = HwParams::any(&pcm).unwrap();
                hwp.set_access(Access::RWInterleaved).unwrap();
                hwp.set_channels(2).unwrap();
                hwp.set_format(Format::S32LE).unwrap();
                hwp.set_rate(RATE, ValueOr::Nearest).unwrap();
                hwp.set_period_size_near(480, ValueOr::Nearest).unwrap();
                hwp.set_buffer_size_near(4_800).unwrap();
                pcm.hw_params(&hwp).unwrap();
            }
            let io = pcm.io_i32().unwrap();
            let n = RATE as usize * SECONDS;
            let all: Vec<i32> = (0..n).flat_map(|i| [sample(i, 0), sample(i, 1)]).collect();
            for chunk in all.chunks(960) {
                let mut off = 0;
                while off < chunk.len() {
                    match io.writei(&chunk[off..]) {
                        Ok(f) => off += 2 * f,
                        Err(_) => pcm.prepare().unwrap(),
                    }
                }
            }
            pcm.drain().ok();
        });

        let mut got: Vec<f64> = Vec::new();
        let mut seg = None;
        let mut buf = vec![0.0; 8_192];
        let until = Instant::now() + Duration::from_secs(SECONDS as u64 + 4);
        while Instant::now() < until {
            server.beat();
            match server.read(&mut buf) {
                Read::Format(s) => seg = Some(s),
                Read::Samples(n) => got.extend_from_slice(&buf[..n]),
                Read::Empty => {
                    server.wait_data(Duration::from_millis(5));
                }
            }
        }
        player.join().unwrap();
        let seg = seg.expect("a segment");
        assert_eq!((seg.rate, seg.channels, seg.sample_type), (RATE as f64, 2, CapFmt::S32.sample_type()));
        let name = server.client().map(|c| c.name).unwrap_or_default();
        eprintln!("player: {name:?}, {} samples", got.len());
        // The capture may begin a little into the stream: from the first
        // frame of the signal on, every sample as it was played.
        let want = |i: usize, ch: usize| sample(i, ch) as f64 / 2_147_483_648.0;
        // A frame of the signal says its index (the low 16 bits of it).
        let index = |x: f64| {
            let u = ((x * 2_147_483_648.0) as i32 ^ 0x5a5a_0000) as u32;
            (u & 0xffff == 0x100).then_some((u >> 16) as usize)
        };
        let first = got.chunks_exact(2).position(|f| index(f[0]).is_some()).expect("the signal");
        let start = index(got[2 * first]).unwrap();
        let frames = got.len() / 2 - first;
        let mut exact = 0;
        for k in 0..frames.min(RATE as usize * SECONDS - start) {
            let (l, r) = (got[2 * (first + k)], got[2 * (first + k) + 1]);
            if l != want(start + k, 0) || r != want(start + k, 1) {
                break;
            }
            exact += 1;
        }
        eprintln!("from frame {start}: {exact} frames bit for bit");
        assert!(exact >= RATE as usize * (SECONDS - 1), "only {exact} frames as played");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every format the input takes comes to the link at full scale ±1,
    /// exactly; others are refused in words.
    #[test]
    fn samples_come_in_at_full_scale() {
        assert_eq!(CapFmt::S16.to_f64(&(-32_768i16).to_le_bytes()), -1.0);
        assert_eq!(CapFmt::S16.to_f64(&16_384i16.to_le_bytes()), 0.5);
        assert_eq!(CapFmt::S24Packed.to_f64(&[0x00, 0x00, 0x80]), -1.0);
        assert_eq!(CapFmt::S24Packed.to_f64(&[0xff, 0xff, 0x3f]), 4_194_303.0 / 8_388_608.0);
        assert_eq!(CapFmt::S24In32.to_f64(&[0x00, 0x00, 0x40, 0x00]), 0.5);
        assert_eq!(CapFmt::S32.to_f64(&i32::MIN.to_le_bytes()), -1.0);
        assert_eq!(CapFmt::Float.to_f64(&0.25f32.to_le_bytes()), 0.25);
        for f in [CapFmt::S16, CapFmt::S24In32, CapFmt::S24Packed, CapFmt::S32, CapFmt::Float] {
            assert_eq!(CapFmt::from_alsa(f.alsa()), Some(f));
        }
        assert_eq!(CapFmt::from_alsa(0), None, "S8");
        let s = Slave { active: true, format: alsa_format::S32_LE, rate: 96_000, channels: 2 };
        assert_eq!(s.capture(), Ok(CapFmt::S32));
        assert!(Slave { channels: 6, ..s }.capture().unwrap_err().contains("6 channels"));
        assert!(Slave { format: 0, ..s }.capture().unwrap_err().contains("sample format 0"));
    }

    #[test]
    fn the_player_is_named_by_its_process() {
        assert_eq!(owner_pid("state: RUNNING\nowner_pid   : 4242\ntrigger_time: 1"), Some(4242));
        assert_eq!(owner_pid("closed"), None);
        assert_eq!(player_name(Some("hqplayer5desktop\n")), "hqplayer5desktop");
        assert_eq!(player_name(Some("pipewire")), "System sound (PipeWire)");
        assert_eq!(player_name(None), "Loopback");
    }

    /// Past its cap the oldest samples go, counted; whole frames come out
    /// in order.
    #[test]
    fn the_backlog_drops_the_oldest() {
        let mut b = Backlog::new(4);
        b.push([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!((b.len(), b.dropped), (4, 2));
        assert_eq!(b.take(2).collect::<Vec<_>>(), [3.0, 4.0]);
        assert_eq!(b.take(9).collect::<Vec<_>>(), [5.0, 6.0]);
    }

    /// The link takes whole frames up to its target, a whole ring empty.
    #[test]
    fn whole_frames_go_in_as_far_as_the_link_has_room() {
        assert_eq!(room_for(1_000, 0, 9_600, 1 << 18), 1_000);
        assert_eq!(room_for(1_000, 9_000, 9_600, 1 << 18), 600);
        assert_eq!(room_for(1_000, 9_001, 9_600, 1 << 18), 598);
        assert_eq!(room_for(1_001, 0, 9_600, 1 << 18), 1_000);
        assert_eq!(room_for(1_000, 9_700, 9_600, 1 << 18), 0);
    }

    /// More is slower: a Loopback running 100 ppm fast is slowed by 100 ppm.
    #[test]
    fn the_shift_is_the_loopbacks_pace() {
        assert_eq!(shift_for(0.0), NO_SHIFT);
        assert_eq!(shift_for(100.0), 100_010);
        assert_eq!(shift_for(-2_000.0), 99_800);
    }

    /// The Loopback's clock off the DAC's by `drift` ppm (positive: fast),
    /// and what waits starting off the aim: the trim finds the drift and
    /// holds the level at the aim, never past the edge, with no overshoot
    /// past a few ms — at the shift's own steps (10 ppm).
    #[test]
    fn the_trim_holds_the_level_against_the_drift() {
        for (drift, start) in [(150.0, 0.025), (-300.0, 0.025), (0.0, 0.040), (80.0, 0.010)] {
            let aim = 0.025;
            let mut t = Trim::new(aim);
            let dt = 0.05;
            let mut level: f64 = start;
            let mut worst_late: f64 = 0.0;
            for i in 0..(900.0 / dt) as usize {
                let ppm = t.update(level, dt, true);
                // The Loopback runs at (1 + drift) / shift of its pace.
                let applied = (shift_for(ppm) - NO_SHIFT) as f64 * 10.0;
                level += (drift - applied) * 1e-6 * dt;
                assert!(ppm.abs() <= MAX_PPM);
                if i as f64 * dt > 400.0 {
                    worst_late = worst_late.max((level - aim).abs());
                }
            }
            assert!((t.ppm - drift).abs() < 15.0, "drift {drift}: trim {}", t.ppm);
            assert!(worst_late < 0.001, "drift {drift}: off the aim by {worst_late} s after 400 s");
            assert!(level > 0.0);
        }
    }

    /// Not learning (the reader not even yet, or not listening): the trim
    /// stays where it was, the level is still followed.
    #[test]
    fn the_trim_learns_only_while_the_reader_is_even() {
        let mut t = Trim::new(0.025);
        assert_eq!(t.update(0.5, 0.05, false), 0.0);
        assert_eq!(t.level(), Some(0.5));
        assert!(t.update(0.5, 0.05, true) > 0.0);
    }
}
