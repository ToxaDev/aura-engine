//! The ALSA backend's choices and its device loop, with no ALSA in them:
//! which format a stream asks a `hw:` device for, what the listener is told
//! when the device refuses the rate or is held by another program, the
//! period sizes, and the render loop's device (`period::Device`) over any
//! PCM (`PcmIo`) — so a fake device runs it in the tests on every system.
//! `backend_alsa` (Linux) puts the real PCM under it.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ── Formats ──────────────────────────────────────────────────────────────────

/// A `hw:` device's sample format: little-endian, interleaved stereo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AlsaFmt {
    S16,
    /// 24 bits in 3 bytes.
    S24_3,
    /// 32 bits; a 24-bit DAC plays the top 24 (the "24 in 32" of WASAPI —
    /// ALSA's S24_LE, the low 24 of 32, is never asked for).
    S32,
    Float,
    /// Native DSD (`native`): a frame is one, two or four bytes of each
    /// channel's one-bit samples, the oldest in the top bit of the first
    /// byte (big-endian) — or, little-endian, of the word's last byte.
    DsdU8,
    DsdU16Le,
    DsdU16Be,
    DsdU32Le,
    DsdU32Be,
}

impl AlsaFmt {
    /// ALSA's name for it.
    pub(super) fn name(self) -> &'static str {
        match self {
            AlsaFmt::S16 => "S16_LE",
            AlsaFmt::S24_3 => "S24_3LE",
            AlsaFmt::S32 => "S32_LE",
            AlsaFmt::Float => "FLOAT_LE",
            AlsaFmt::DsdU8 => "DSD_U8",
            AlsaFmt::DsdU16Le => "DSD_U16_LE",
            AlsaFmt::DsdU16Be => "DSD_U16_BE",
            AlsaFmt::DsdU32Le => "DSD_U32_LE",
            AlsaFmt::DsdU32Be => "DSD_U32_BE",
        }
    }

    /// Bytes per stereo frame.
    pub(super) fn frame_bytes(self) -> usize {
        match self {
            AlsaFmt::S16 => 4,
            AlsaFmt::S24_3 => 6,
            AlsaFmt::S32 | AlsaFmt::Float => 8,
            f => 2 * f.dsd().map_or(0, |(w, _)| w),
        }
    }

    /// A DSD format's bytes a channel a frame, and whether its words are
    /// little-endian; None for PCM.
    pub(super) fn dsd(self) -> Option<(usize, bool)> {
        match self {
            AlsaFmt::DsdU8 => Some((1, false)),
            AlsaFmt::DsdU16Le => Some((2, true)),
            AlsaFmt::DsdU16Be => Some((2, false)),
            AlsaFmt::DsdU32Le => Some((4, true)),
            AlsaFmt::DsdU32Be => Some((4, false)),
            _ => None,
        }
    }
}

/// The ALSA format a stream format goes out in: 24 in 32 and 32/32 are
/// both S32_LE (the same bytes), packed 24 is S24_3LE.
pub(super) fn alsa_fmt(f: FmtSpec) -> AlsaFmt {
    match (f.float, f.store) {
        (true, _) => AlsaFmt::Float,
        (false, 16) => AlsaFmt::S16,
        (false, 24) => AlsaFmt::S24_3,
        _ => AlsaFmt::S32,
    }
}

/// The rates looked at when a device refuses one (and in `diagnose`).
pub(super) const COMMON_RATES: [u32; 12] =
    [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000, 1_411_200, 1_536_000];

/// What a device takes: stereo in `fmt` at exactly `rate`, nothing resampled.
pub(super) trait HwCaps {
    fn accepts(&self, fmt: AlsaFmt, rate: u32) -> bool;
}

/// The rates of `COMMON_RATES` the device plays in any of `fmts`.
pub(super) fn rates_taken(caps: &impl HwCaps, fmts: &[AlsaFmt]) -> Vec<u32> {
    COMMON_RATES.iter().copied().filter(|&r| fmts.iter().any(|&f| caps.accepts(f, r))).collect()
}

/// "44.1, 48, 96 kHz".
pub(super) fn khz_list(rates: &[u32]) -> String {
    let khz: Vec<String> = rates.iter().map(|&r| format!("{}", r as f64 / 1000.0)).collect();
    format!("{} kHz", khz.join(", "))
}

/// The format a stream in `mode` at `rate` opens `device` with: the first
/// of `formats_for(mode)` the device takes at exactly that rate. Refused:
/// a DoP stream with `DOP_FORMAT_ERR` first (the controller then plays
/// PCM), any other with the rates the device does play — it is never
/// resampled.
pub(super) fn choose(mode: &OutputMode, rate: u32, caps: &impl HwCaps, device: &str) -> Result<FmtSpec, String> {
    let wanted = formats_for(mode);
    if let Some(f) = wanted.iter().copied().find(|&f| caps.accepts(alsa_fmt(f), rate)) {
        return Ok(f);
    }
    let all = [AlsaFmt::S32, AlsaFmt::S24_3, AlsaFmt::S16, AlsaFmt::Float];
    let at_rate: Vec<&str> = all.iter().filter(|&&f| caps.accepts(f, rate)).map(|f| f.name()).collect();
    if mode.is_dop() {
        let has = if at_rate.is_empty() { "does not play this rate".to_string() } else { format!("takes {}", at_rate.join(", ")) };
        return Err(format!("{DOP_FORMAT_ERR} at {rate} Hz on {device} (it {has})"));
    }
    if at_rate.is_empty() {
        let plays = rates_taken(caps, &all);
        let plays = if plays.is_empty() { "no common rate in stereo".to_string() } else { khz_list(&plays) };
        return Err(format!(
            "{device} does not play {rate} Hz bit-perfect (it plays {plays}); \
             choose an upsampling target the device plays"
        ));
    }
    let mut names: Vec<&str> = wanted.iter().map(|&f| alsa_fmt(f).name()).collect();
    names.dedup();
    Err(format!(
        "{device} takes no {} at {rate} Hz (it takes {})",
        names.join("/"),
        at_rate.join(", ")
    ))
}

// ── Native DSD ───────────────────────────────────────────────────────────────

/// The DSD formats ALSA has, widest first. A USB DAC's driver offers the
/// one its firmware takes (most: DSD_U32_BE), where the kernel knows the
/// DAC plays DSD natively; else none, and DSD goes as DoP.
pub(super) const DSD_FMTS: [AlsaFmt; 5] =
    [AlsaFmt::DsdU32Be, AlsaFmt::DsdU32Le, AlsaFmt::DsdU16Be, AlsaFmt::DsdU16Le, AlsaFmt::DsdU8];

/// Native DSD: the DSD bytes of each channel a frame of the render loop
/// carries (`period::Wire::Native`, left then right) for a stream in `mode`
/// at `rate` frames a second — two at a DoP carrier, four for DSD512 and
/// eight for DSD1024 on the ×16 chain; a DSD file as it is keeps its 16
/// one-bit samples a frame. None for PCM, or when a frame is no whole bytes.
pub(super) fn dsd_bpf(mode: &OutputMode, rate: u32) -> Option<usize> {
    let (dsd, as_is) = match *mode {
        OutputMode::Dsd { rate, .. } => (rate, false),
        OutputMode::DsdDirect { rate } => (rate, true),
        _ => return None,
    };
    let bits = dsd.bit_rate();
    if rate == 0 || bits % rate != 0 {
        return None;
    }
    let r = (bits / rate) as usize;
    (r % 8 == 0 && r >= 16 && (!as_is || r == 16)).then_some(r / 8)
}

/// How the render loop's native DSD frames (`bpf` bytes of each channel,
/// left then right) become a DSD format's (`w` bytes of each channel):
/// the same bytes of each channel in the same order, regrouped — a DSD1024
/// frame of the ×16 chain is two DSD_U32 frames, two DSD64 frames at the
/// DoP carrier are one — each word reversed for a little-endian format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DsdPack {
    pub(super) w: usize,
    pub(super) le: bool,
    pub(super) bpf: usize,
}

impl DsdPack {
    /// The render loop's frames in `alsa` of the device's (whole ones).
    pub(super) fn to_loop(self, alsa: usize) -> usize {
        alsa * self.w / self.bpf
    }

    /// The device's frames in `frames` of the render loop's (a whole
    /// number of them: `to_loop` hands out only such counts).
    pub(super) fn to_alsa(self, frames: usize) -> usize {
        frames * self.bpf / self.w
    }

    /// `frames` of the render loop's bytes (`src`) as the device takes them.
    pub(super) fn pack(self, frames: usize, src: &[u8], out: &mut Vec<u8>) {
        let (w, bpf) = (self.w, self.bpf);
        let n = self.to_alsa(frames);
        out.clear();
        out.resize(2 * w * n, 0);
        for j in 0..n {
            let o = 2 * w * j;
            for k in 0..w {
                // A channel's byte `b`: in frame b / bpf, at b % bpf.
                let b = w * j + k;
                let s = 2 * bpf * (b / bpf) + b % bpf;
                let d = if self.le { w - 1 - k } else { k };
                out[o + d] = src[s];
                out[o + w + d] = src[s + bpf];
            }
        }
    }
}

/// Native DSD for a stream in `mode` at `rate` (the render loop's): the
/// first DSD format the device takes at the rate the stream comes to in
/// it, with how the bytes are regrouped and that rate. Formats whose
/// words split the loop's frames evenly first, the widest of them first.
/// None: PCM, or the device takes no DSD format (DoP then, up to DSD256).
pub(super) fn choose_native(mode: &OutputMode, rate: u32, caps: &impl HwCaps) -> Option<(AlsaFmt, DsdPack, u32)> {
    let bpf = dsd_bpf(mode, rate)?;
    let mut order = DSD_FMTS.to_vec();
    order.sort_by_key(|f| f.dsd().is_some_and(|(w, _)| bpf % w != 0));
    order.into_iter().find_map(|f| {
        let (w, le) = f.dsd()?;
        let bytes = u64::from(rate) * bpf as u64;
        let r = u32::try_from(bytes / w as u64).ok().filter(|_| bytes % w as u64 == 0)?;
        caps.accepts(f, r).then_some((f, DsdPack { w, le, bpf }, r))
    })
}

/// A DSD stream the device takes no DSD format for, past DoP's DSD256:
/// refused as a device with no DoP format (the controller plays PCM).
pub(super) fn native_refused(mode: &OutputMode, caps: &impl HwCaps, device: &str) -> Option<String> {
    let dsd = match *mode {
        OutputMode::Dsd { rate, .. } | OutputMode::DsdDirect { rate } => rate,
        _ => return None,
    };
    if dsd.multiple <= 256 {
        return None;
    }
    let took: Vec<&str> = DSD_FMTS.iter().filter(|&&f| any_rate(caps, f)).map(|f| f.name()).collect();
    let has = if took.is_empty() { "no DSD format".to_string() } else { format!("{} at other rates", took.join(", ")) };
    Some(format!(
        "{DOP_FORMAT_ERR}; {device} plays no native DSD{} at {} Hz ({has}) and DoP carries DSD64-DSD256 only",
        dsd.multiple,
        dsd.bit_rate()
    ))
}

/// Whether the device takes `f` at any DSD rate of either family (the
/// rates of DSD_U8 to DSD_U32 for DSD64-DSD1024).
fn any_rate(caps: &impl HwCaps, f: AlsaFmt) -> bool {
    let Some((w, _)) = f.dsd() else { return false };
    [44_100u32, 48_000].iter().any(|&base| [64u32, 128, 256, 512, 1024].iter().any(|&m| caps.accepts(f, base * m / (8 * w as u32))))
}

/// Whether DoP is asked for over a native DSD format (`AURA_ALSA_DOP=1`).
pub(super) fn dop_only() -> bool {
    std::env::var_os("AURA_ALSA_DOP").is_some_and(|v| v == "1")
}

// ── Periods ──────────────────────────────────────────────────────────────────

/// A period of ~10 ms, as WASAPI exclusive runs.
pub(super) fn period_frames(rate: u32) -> usize {
    (rate as usize / 100).max(64)
}

/// Periods the device buffer holds.
pub(super) const PERIODS: usize = 3;

/// Software parameters for a buffer of `buffer` frames in periods of
/// `period`: the device wakes the loop once a period is free, and starts
/// by itself once two periods are in after an xrun (the first start is
/// the stream's own).
pub(super) fn thresholds(period: usize, buffer: usize) -> (usize, usize) {
    let avail_min = period;
    let start = buffer.saturating_sub(period).max(period);
    (avail_min, start)
}

// ── Devices ──────────────────────────────────────────────────────────────────

/// The id of a card's PCM device: by the card's id, which keeps across
/// reboots and plugging in order (its number does not).
pub(super) fn hw_id(card_id: &str, dev: u32) -> String {
    format!("hw:CARD={card_id},DEV={dev}")
}

/// The name the UI shows: the card's, and the PCM's when the card has more
/// than one ("HDA Intel PCH — ALC1220 Analog").
pub(super) fn device_name(card: &str, pcm: &str, pcms_on_card: usize) -> String {
    if pcms_on_card <= 1 || pcm.is_empty() || pcm == card {
        card.to_string()
    } else {
        format!("{card} — {pcm}")
    }
}

/// `owner_pid` from `/proc/asound/cardN/pcmDp/subS/status` (an open PCM).
pub(super) fn owner_pid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "owner_pid").then(|| v.trim().parse().ok()).flatten()
    })
}

/// Who holds a busy device, in the listener's words, and how to free it.
pub(super) fn busy_message(device: &str, holder: Option<(u32, &str)>) -> String {
    let Some((pid, comm)) = holder else {
        return format!(
            "{device} is busy: another program has it open. Close the programs that play sound \
             (a browser, a media player) and try again"
        );
    };
    match comm.trim() {
        "pipewire" | "pipewire-pulse" | "wireplumber" => format!(
            "{device} is busy: PipeWire, the system's sound server (pid {pid}), holds it. \
             For bit-perfect play free the card: stop other sound and wait a few seconds, or set \
             the card's profile to Off (pavucontrol, Configuration), or stop PipeWire for this \
             session: systemctl --user stop pipewire.socket pipewire-pulse.socket pipewire \
             pipewire-pulse wireplumber"
        ),
        "pulseaudio" => format!(
            "{device} is busy: PulseAudio, the system's sound server (pid {pid}), holds it. \
             For bit-perfect play free the card: set its profile to Off (pavucontrol, \
             Configuration), or stop PulseAudio for this session: systemctl --user stop \
             pulseaudio.socket pulseaudio"
        ),
        "jackd" | "jackdbus" => format!("{device} is busy: JACK (pid {pid}) holds it. Stop JACK and try again"),
        other => format!("{device} is busy: {other} (pid {pid}) holds it. Close it and try again"),
    }
}

// ── The device loop ──────────────────────────────────────────────────────────

/// Linux errno values the loop tells apart (libc's, here so the tests run
/// on every system).
pub(super) mod errno {
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const EAGAIN: i32 = 11;
    pub const EBUSY: i32 = 16;
    pub const ENODEV: i32 = 19;
    pub const EPIPE: i32 = 32;
    pub const EBADFD: i32 = 77;
    pub const ESTRPIPE: i32 = 86;
}

/// What a PCM call's error means to the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    /// The device ran out of data: prepare and go on.
    Xrun,
    /// The system was suspended: resume (or prepare) and go on.
    Suspended,
    /// The device is gone — unplugged, or its driver let it go.
    Lost,
    Other,
}

pub(super) fn fault(errno: i32) -> Fault {
    match errno {
        errno::EPIPE => Fault::Xrun,
        errno::ESTRPIPE => Fault::Suspended,
        errno::ENODEV | errno::EBADFD | errno::ENXIO | errno::EIO => Fault::Lost,
        _ => Fault::Other,
    }
}

/// An open PCM, as the loop uses it; errors are errno values.
pub(super) trait PcmIo {
    /// Wait up to `ms` for a period to be free: false when none came.
    fn wait(&mut self, ms: u32) -> Result<bool, i32>;
    /// Frames writable now.
    fn avail(&mut self) -> Result<usize, i32>;
    /// Write whole frames; the frames written (EAGAIN: no room now).
    fn writei(&mut self, bytes: &[u8]) -> Result<usize, i32>;
    fn prepare(&mut self) -> Result<(), i32>;
    fn resume(&mut self) -> Result<(), i32>;
    fn start(&mut self) -> Result<(), i32>;
}

/// "the device is gone (unplugged?) (ENODEV)" and the like.
pub(super) fn errno_text(e: i32) -> String {
    let name = match e {
        errno::EIO => "EIO",
        errno::ENXIO => "ENXIO",
        errno::EAGAIN => "EAGAIN",
        errno::EBUSY => "EBUSY",
        errno::ENODEV => "ENODEV",
        errno::EPIPE => "EPIPE",
        errno::EBADFD => "EBADFD",
        errno::ESTRPIPE => "ESTRPIPE",
        _ => return format!("errno {e}"),
    };
    name.to_string()
}

/// The PCMs being closed, by device id, and since when. A close can hang in
/// the kernel for good: the USB audio driver waits for transfers the host
/// controller never hands back (VMware's virtual USB 2, 26.10), the card's
/// driver stays locked, and an open of the same card would hang in it too.
/// So an open waits a little for a close of its device to end, then is
/// refused in words (`stuck_message`).
pub(super) struct Closing(Mutex<Vec<(String, Instant)>>);

impl Closing {
    pub(super) const fn new() -> Self {
        Closing(Mutex::new(Vec::new()))
    }

    pub(super) fn begin(&self, id: &str) {
        self.0.lock().unwrap().push((id.to_string(), Instant::now()));
    }

    pub(super) fn end(&self, id: &str) {
        let mut all = self.0.lock().unwrap();
        if let Some(i) = all.iter().position(|(d, _)| d == id) {
            all.remove(i);
        }
    }

    /// Wait up to `max` for `id` to be closed; how long it has been
    /// closing when it still is.
    pub(super) fn wait_clear(&self, id: &str, max: Duration) -> Result<(), Duration> {
        let until = Instant::now() + max;
        loop {
            let since = self.0.lock().unwrap().iter().find(|(d, _)| d == id).map(|(_, t)| t.elapsed());
            match since {
                None => return Ok(()),
                Some(s) if Instant::now() >= until => return Err(s),
                Some(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

/// How long an open waits for its device's close (`Closing`).
pub(super) const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// A device whose last stream has not closed for `since`.
pub(super) fn stuck_message(name: &str, since: Duration) -> String {
    format!(
        "{name} does not answer: its last stream has not closed for {} s - the system's USB audio driver is stuck \
         on it. Unplug the DAC and plug it in again (in a virtual machine: disconnect and reconnect it), or restart \
         the computer.",
        since.as_secs()
    )
}

/// The render loop's device over an ALSA PCM: `space` and `write` recover
/// an xrun or a suspend on the spot (the device starts again by itself
/// once two periods are in), a device gone is an error — the controller
/// is told it is lost. So is a device that stops taking data with no
/// error at all (`STALL`).
pub(super) struct AlsaDev<P: PcmIo> {
    pub(super) pcm: P,
    /// The most frames one `space` offers (the stream's chunk).
    pub(super) chunk: usize,
    pub(super) frame_bytes: usize,
    /// An error `wait` met, for `space` to answer.
    pending: Option<i32>,
    pub(super) xruns: u64,
    /// When the device last had room.
    last_room: Instant,
    /// No room for this long: it has stalled (`STALL`).
    pub(super) stall_after: Duration,
    /// Native DSD: the render loop's frames regrouped into the device's
    /// (`DsdPack`); `space` and `write` count the loop's frames, `chunk`
    /// and `frame_bytes` are the device's.
    dsd: Option<DsdPack>,
    /// The device's bytes of a native DSD period.
    packed: Vec<u8>,
}

/// Recoveries one call tries before it gives up.
const RECOVERIES: usize = 3;

/// A running device with no room for this long has stopped playing: its
/// position stands still and no call fails. A USB stream whose transfers
/// the host controller never hands back does this (VMware's virtual USB 2
/// with Mojo 2, 26.10: about 10 ms played, then nothing — the meters
/// froze and the loop waited for room for ever).
pub(super) const STALL: Duration = Duration::from_secs(2);

impl<P: PcmIo> AlsaDev<P> {
    pub(super) fn new(pcm: P, chunk: usize, frame_bytes: usize) -> Self {
        AlsaDev {
            pcm,
            chunk,
            frame_bytes,
            pending: None,
            xruns: 0,
            last_room: Instant::now(),
            stall_after: STALL,
            dsd: None,
            packed: Vec::new(),
        }
    }

    /// Native DSD in `pack`'s format.
    pub(super) fn with_dsd(mut self, pack: Option<DsdPack>) -> Self {
        self.dsd = pack;
        if let Some(p) = pack {
            self.packed.reserve(2 * p.w * self.chunk);
        }
        self
    }

    /// Whole frames of the device's: written until none are left.
    fn write_device(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let total = frames * self.frame_bytes;
        let mut off = 0;
        let mut recoveries = 0;
        let mut waits = 0;
        while off < total {
            match self.pcm.writei(&bytes[off..total]) {
                Ok(n) => off += n * self.frame_bytes,
                // Non-blocking and full: wait for room (a device that
                // takes nothing for a second is gone).
                Err(errno::EAGAIN) if waits < 10 => {
                    waits += 1;
                    if let Err(e) = self.pcm.wait(100) {
                        self.recover(e)?;
                    }
                }
                Err(e) if recoveries < RECOVERIES => {
                    recoveries += 1;
                    self.recover(e)?;
                }
                Err(e) => return Err(self.gone(e)),
            }
        }
        Ok(())
    }

    fn stalled(&self) -> bool {
        self.last_room.elapsed() >= self.stall_after
    }

    /// Recover from `e`, or the error the loop ends with.
    fn recover(&mut self, e: i32) -> Result<(), String> {
        match fault(e) {
            Fault::Xrun => {
                self.xruns += 1;
                crate::aelog!("[PLAYER] alsa xrun #{} - the device ran dry, restarted", self.xruns);
                self.pcm.prepare().map_err(|e| self.gone(e))
            }
            Fault::Suspended => {
                crate::aelog!("[PLAYER] alsa: the system was suspended - resuming the stream");
                let mut r = self.pcm.resume();
                for _ in 0..100 {
                    if r != Err(errno::EAGAIN) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    r = self.pcm.resume();
                }
                if r.is_err() {
                    self.pcm.prepare().map_err(|e| self.gone(e))?;
                }
                Ok(())
            }
            _ => Err(self.gone(e)),
        }
    }

    fn gone(&self, e: i32) -> String {
        match fault(e) {
            Fault::Lost => format!("the device is gone (unplugged?) ({})", errno_text(e)),
            _ => format!("the device failed ({})", errno_text(e)),
        }
    }
}

impl<P: PcmIo> Device for AlsaDev<P> {
    fn wait(&mut self) -> bool {
        match self.pcm.wait(100) {
            // Stalled: `space` says so.
            Ok(ready) => ready || self.stalled(),
            Err(e) => {
                self.pending = Some(e);
                true
            }
        }
    }

    fn space(&mut self) -> Result<usize, String> {
        if let Some(e) = self.pending.take() {
            self.recover(e)?;
        }
        for _ in 0..RECOVERIES {
            match self.pcm.avail() {
                Ok(0) if self.stalled() => {
                    return Err(format!(
                        "the device stopped taking data - nothing played for {} s (its USB stream stalled?)",
                        self.stall_after.as_secs()
                    ))
                }
                Ok(n) => {
                    if n > 0 {
                        self.last_room = Instant::now();
                    }
                    let n = n.min(self.chunk);
                    return Ok(self.dsd.map_or(n, |p| p.to_loop(n)));
                }
                Err(e) => self.recover(e)?,
            }
        }
        Err("the device keeps failing (xruns)".to_string())
    }

    fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let Some(pack) = self.dsd else { return self.write_device(frames, bytes) };
        let mut packed = std::mem::take(&mut self.packed);
        pack.pack(frames, bytes, &mut packed);
        let r = self.write_device(pack.to_alsa(frames), &packed);
        self.packed = packed;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsd::{DsdRate, ModulatorKind};
    use std::collections::VecDeque;

    /// A device that takes `fmts` at `rates`.
    struct Caps {
        fmts: Vec<AlsaFmt>,
        rates: Vec<u32>,
    }

    impl HwCaps for Caps {
        fn accepts(&self, fmt: AlsaFmt, rate: u32) -> bool {
            self.fmts.contains(&fmt) && self.rates.contains(&rate)
        }
    }

    fn usb_dac() -> Caps {
        Caps { fmts: vec![AlsaFmt::S32, AlsaFmt::S24_3, AlsaFmt::S16], rates: vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000] }
    }

    fn dop64() -> OutputMode {
        OutputMode::DsdDirect { rate: DsdRate::DSD64 }
    }

    /// Every mode gets the first of its formats the device takes, at the
    /// rate asked for: DSP and 24-bit sources S32, a 16-bit source S16,
    /// DoP S32 (its 24-bit word on top) — or packed 24 where only that is.
    #[test]
    fn the_first_format_the_device_takes_is_chosen() {
        let dac = usb_dac();
        let pick = |mode: &OutputMode, caps: &Caps| alsa_fmt(choose(mode, 96_000, caps, "DAC").unwrap());
        assert_eq!(pick(&OutputMode::Dsp, &dac), AlsaFmt::S32);
        assert_eq!(pick(&OutputMode::Direct { source_bits: 16 }, &dac), AlsaFmt::S16);
        assert_eq!(pick(&OutputMode::Direct { source_bits: 24 }, &dac), AlsaFmt::S32);
        assert_eq!(pick(&OutputMode::Direct { source_bits: 0 }, &dac), AlsaFmt::S32);
        let packed = Caps { fmts: vec![AlsaFmt::S24_3, AlsaFmt::S16], rates: dac.rates.clone() };
        assert_eq!(pick(&OutputMode::Direct { source_bits: 24 }, &packed), AlsaFmt::S24_3);
        assert_eq!(pick(&dop64(), &packed), AlsaFmt::S24_3);
        assert_eq!(choose(&OutputMode::Direct { source_bits: 24 }, 96_000, &packed, "DAC").unwrap().packer(), SampleFmt::Int24Packed);
        assert_eq!(alsa_fmt(choose(&dop64(), 176_400, &dac, "DAC").unwrap()), AlsaFmt::S32);
    }

    /// A rate the device does not play is an error naming the rates it
    /// does — never a resampled stream.
    #[test]
    fn a_rate_the_device_does_not_play_is_refused_with_its_rates() {
        let e = choose(&OutputMode::Dsp, 705_600, &usb_dac(), "Mojo 2").unwrap_err();
        assert!(e.starts_with("Mojo 2 does not play 705600 Hz bit-perfect"), "{e}");
        assert!(e.contains("44.1, 48, 88.2, 96, 176.4, 192, 352.8, 384 kHz"), "{e}");
        assert!(!is_dop_format_error(&e));
    }

    /// DSP needs 24 in 32, 32 or float: a 16-bit-only device says so; a
    /// 24-bit source on a 16-bit device is refused (no truncation).
    #[test]
    fn a_device_with_no_wanted_format_says_what_it_takes() {
        let cheap = Caps { fmts: vec![AlsaFmt::S16], rates: vec![44_100, 48_000] };
        let e = choose(&OutputMode::Dsp, 48_000, &cheap, "USB").unwrap_err();
        assert_eq!(e, "USB takes no S32_LE/FLOAT_LE at 48000 Hz (it takes S16_LE)");
        assert!(choose(&OutputMode::Direct { source_bits: 24 }, 48_000, &cheap, "USB").is_err());
        assert!(choose(&OutputMode::Direct { source_bits: 16 }, 48_000, &cheap, "USB").is_ok());
    }

    /// DoP refused — no 24-bit integer format, or not the carrier rate —
    /// fails with `DOP_FORMAT_ERR`, so the controller plays PCM instead.
    #[test]
    fn dop_refused_is_the_dop_format_error() {
        let cheap = Caps { fmts: vec![AlsaFmt::S16], rates: vec![176_400] };
        assert!(is_dop_format_error(&choose(&dop64(), 176_400, &cheap, "USB").unwrap_err()));
        let e = choose(&dop64(), 705_600, &usb_dac(), "DAC").unwrap_err();
        assert!(is_dop_format_error(&e) && e.contains("does not play this rate"), "{e}");
    }

    #[test]
    fn periods_are_ten_ms_and_restart_at_two() {
        assert_eq!(period_frames(44_100), 441);
        assert_eq!(period_frames(768_000), 7_680);
        let p = period_frames(96_000);
        assert_eq!(thresholds(p, p * PERIODS), (p, 2 * p));
        assert_eq!(thresholds(p, p), (p, p), "a one-period buffer starts at one");
        assert_eq!(AlsaFmt::S24_3.frame_bytes(), 6);
    }

    #[test]
    fn ids_and_names() {
        assert_eq!(hw_id("Mojo2", 0), "hw:CARD=Mojo2,DEV=0");
        assert_eq!(device_name("Mojo 2", "USB Audio", 1), "Mojo 2");
        assert_eq!(device_name("HDA Intel PCH", "ALC1220 Analog", 3), "HDA Intel PCH — ALC1220 Analog");
    }

    /// The busy error names the sound server holding the card and how to
    /// free it.
    #[test]
    fn a_busy_card_names_its_holder() {
        let status = "state: RUNNING\nowner_pid   : 1234\ntrigger_time: 1.0\n";
        assert_eq!(owner_pid(status), Some(1234));
        assert_eq!(owner_pid("closed\n"), None);
        let e = busy_message("Mojo 2", Some((1234, "pipewire\n")));
        assert!(e.contains("PipeWire") && e.contains("pid 1234") && e.contains("systemctl --user stop"), "{e}");
        assert!(busy_message("Mojo 2", Some((9, "wireplumber"))).contains("PipeWire"));
        assert!(busy_message("Mojo 2", Some((9, "pulseaudio"))).contains("PulseAudio"));
        assert!(busy_message("Mojo 2", Some((9, "vlc"))).contains("vlc (pid 9)"));
        assert!(busy_message("Mojo 2", None).contains("another program"));
    }

    /// A PCM that answers from a script; what is written is kept.
    #[derive(Default)]
    struct FakePcm {
        avail: VecDeque<Result<usize, i32>>,
        writes: VecDeque<Result<usize, i32>>,
        resumes: VecDeque<Result<(), i32>>,
        waits: VecDeque<Result<bool, i32>>,
        prepared: usize,
        written: Vec<u8>,
    }

    impl PcmIo for FakePcm {
        fn wait(&mut self, _ms: u32) -> Result<bool, i32> {
            self.waits.pop_front().unwrap_or(Ok(true))
        }
        fn avail(&mut self) -> Result<usize, i32> {
            self.avail.pop_front().unwrap_or(Ok(1_000))
        }
        fn writei(&mut self, bytes: &[u8]) -> Result<usize, i32> {
            let n = self.writes.pop_front().unwrap_or(Ok(bytes.len() / 8))?;
            self.written.extend_from_slice(&bytes[..n * 8]);
            Ok(n)
        }
        fn prepare(&mut self) -> Result<(), i32> {
            self.prepared += 1;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), i32> {
            self.resumes.pop_front().unwrap_or(Ok(()))
        }
        fn start(&mut self) -> Result<(), i32> {
            Ok(())
        }
    }

    fn frames(n: usize) -> Vec<u8> {
        (0..n * 8).map(|i| i as u8).collect()
    }

    /// An xrun in `space` or mid-`write` is recovered: the PCM prepared,
    /// the rest of the period written, every byte once, in order.
    #[test]
    fn an_xrun_is_recovered_and_nothing_is_lost() {
        let mut fake = FakePcm::default();
        fake.avail.push_back(Err(errno::EPIPE));
        fake.writes.extend([Ok(3), Err(errno::EPIPE)]);
        let mut dev = AlsaDev::new(fake, 10, 8);
        assert_eq!(dev.space(), Ok(10), "capped to the chunk");
        let bytes = frames(10);
        dev.write(10, &bytes).unwrap();
        assert_eq!(dev.pcm.written, bytes);
        assert_eq!((dev.xruns, dev.pcm.prepared), (2, 2));
    }

    /// A full device (non-blocking) is waited for, then written.
    #[test]
    fn a_full_device_is_waited_for() {
        let mut fake = FakePcm::default();
        fake.writes.extend([Err(errno::EAGAIN), Ok(1), Err(errno::EAGAIN)]);
        let mut dev = AlsaDev::new(fake, 10, 8);
        dev.write(2, &frames(2)).unwrap();
        assert_eq!(dev.pcm.written, frames(2));
        assert_eq!(dev.xruns, 0);
        dev.pcm.writes.extend([Err(errno::EAGAIN); 11]);
        assert!(dev.write(1, &frames(1)).is_err(), "never any room: gone");
    }

    /// A suspend is resumed (after EAGAIN), or prepared when resume fails.
    #[test]
    fn a_suspend_is_resumed() {
        let mut fake = FakePcm::default();
        fake.avail.extend([Err(errno::ESTRPIPE), Ok(4)]);
        fake.resumes.extend([Err(errno::EAGAIN), Ok(())]);
        let mut dev = AlsaDev::new(fake, 10, 8);
        assert_eq!(dev.space(), Ok(4));
        assert_eq!(dev.pcm.prepared, 0);
        dev.pcm.avail.push_back(Err(errno::ESTRPIPE));
        dev.pcm.resumes.push_back(Err(38)); // ENOSYS
        assert_eq!(dev.space(), Ok(10));
        assert_eq!(dev.pcm.prepared, 1);
    }

    /// Unplugged: the loop ends with an error that says so, from any call.
    #[test]
    fn an_unplugged_device_is_lost() {
        let mut fake = FakePcm::default();
        fake.avail.push_back(Err(errno::ENODEV));
        let mut dev = AlsaDev::new(fake, 10, 8);
        assert!(dev.space().unwrap_err().contains("gone (unplugged?) (ENODEV)"));
        dev.pcm.writes.push_back(Err(errno::EBADFD));
        assert!(dev.write(2, &frames(2)).unwrap_err().contains("gone"));
        struct Failing;
        impl PcmIo for Failing {
            fn wait(&mut self, _: u32) -> Result<bool, i32> { Err(errno::ENODEV) }
            fn avail(&mut self) -> Result<usize, i32> { Ok(1) }
            fn writei(&mut self, _: &[u8]) -> Result<usize, i32> { Ok(1) }
            fn prepare(&mut self) -> Result<(), i32> { Ok(()) }
            fn resume(&mut self) -> Result<(), i32> { Ok(()) }
            fn start(&mut self) -> Result<(), i32> { Ok(()) }
        }
        let mut dev = AlsaDev::new(Failing, 10, 8);
        assert!(dev.wait(), "the error is answered by space");
        assert!(dev.space().unwrap_err().contains("ENODEV"));
    }

    /// Xruns that never end give up rather than spin.
    #[test]
    fn endless_xruns_give_up() {
        let mut fake = FakePcm::default();
        fake.avail.extend([Err(errno::EPIPE), Err(errno::EPIPE), Err(errno::EPIPE)]);
        let mut dev = AlsaDev::new(fake, 10, 8);
        assert!(dev.space().is_err());
        dev.pcm.writes.extend([Err(errno::EPIPE); 4]);
        assert!(dev.write(1, &frames(1)).is_err());
    }

    /// A device that stops taking data with no error (its position stands
    /// still) ends the loop with an error that says so: not a wait for
    /// ever with the meters frozen. Room in time keeps it going.
    #[test]
    fn a_stalled_device_is_an_error() {
        let mut fake = FakePcm::default();
        fake.avail.extend([Ok(0), Ok(5), Ok(0)]);
        let mut dev = AlsaDev::new(fake, 10, 8);
        assert_eq!(dev.space(), Ok(0), "full for a moment: not a stall");
        dev.pcm.waits.push_back(Ok(false));
        assert!(!dev.wait(), "no period yet");
        dev.stall_after = Duration::from_millis(30);
        assert_eq!(dev.space(), Ok(5));
        std::thread::sleep(Duration::from_millis(40));
        dev.pcm.waits.push_back(Ok(false));
        assert!(dev.wait(), "stalled: space is asked");
        let e = dev.space().unwrap_err();
        assert!(e.contains("stopped taking data"), "{e}");
    }

    /// A device that frees room only now and then, but within `stall_after`
    /// each time, never trips it: the bar is time with no room at all, not
    /// how often room comes. (A pause still plays — silence — and a new
    /// rate is a new stream with its own clock; 2 s is 200 of the 10 ms
    /// periods the backend asks for.)
    #[test]
    fn a_slow_device_in_time_is_not_a_stall() {
        let mut dev = AlsaDev::new(FakePcm::default(), 10, 8);
        // Each gap well inside the bar (a slow test machine's sleep runs
        // long), all of them together well past it.
        dev.stall_after = Duration::from_millis(300);
        assert!(STALL >= Duration::from_secs(1) && period_frames(705_600) * 1000 / 705_600 <= 20);
        for _ in 0..10 {
            dev.pcm.avail.extend([Ok(0), Ok(0)]);
            dev.pcm.waits.push_back(Ok(false));
            assert!(!dev.wait());
            assert_eq!(dev.space(), Ok(0));
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(dev.space(), Ok(0), "still in time");
            assert_eq!(dev.space(), Ok(10), "room");
        }
    }

    /// An open waits for its device's close; a close that does not end is
    /// how long it has been stuck; other devices are not held up.
    #[test]
    fn a_stuck_close_holds_only_its_device() {
        let closing = Closing::new();
        closing.begin("hw:CARD=Mojo2,DEV=0");
        assert_eq!(closing.wait_clear("hw:CARD=PCH,DEV=0", Duration::ZERO), Ok(()));
        assert!(closing.wait_clear("hw:CARD=Mojo2,DEV=0", Duration::from_millis(20)).is_err());
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                closing.end("hw:CARD=Mojo2,DEV=0");
            });
            assert_eq!(closing.wait_clear("hw:CARD=Mojo2,DEV=0", Duration::from_secs(2)), Ok(()), "closed meanwhile");
        });
        let e = stuck_message("Mojo2", Duration::from_secs(7));
        assert!(e.contains("Mojo2 does not answer") && e.contains("7 s") && e.contains("Unplug"), "{e}");
    }

    // ── Native DSD ──

    fn dsd(multiple: u32) -> OutputMode {
        OutputMode::Dsd { rate: DsdRate::new(44_100, multiple).unwrap(), modulator: ModulatorKind::Sdm7 }
    }

    /// An XMOS-style DAC: DSD_U32_BE at DSD64-DSD1024 (88.2 to 1411.2 kHz)
    /// beside its PCM.
    fn dsd_dac() -> Caps {
        let mut c = usb_dac();
        c.fmts.push(AlsaFmt::DsdU32Be);
        c.rates.extend([705_600, 1_411_200]);
        c
    }

    /// The render loop's DSD bytes a channel a frame: two at a DoP
    /// carrier (and a DSD file as it is), four and eight for DSD512 and
    /// DSD1024 on the ×16 chain; none for PCM or a rate with no whole bytes.
    #[test]
    fn the_loops_dsd_bytes_a_frame() {
        assert_eq!(dsd_bpf(&dsd(64), 176_400), Some(2));
        assert_eq!(dsd_bpf(&dsd(256), 705_600), Some(2));
        assert_eq!(dsd_bpf(&dsd(512), 705_600), Some(4));
        assert_eq!(dsd_bpf(&dsd(1024), 705_600), Some(8));
        assert_eq!(dsd_bpf(&dop64(), 176_400), Some(2));
        // A DSD file as it is keeps its 16 one-bit samples a frame.
        assert_eq!(dsd_bpf(&dop64(), 88_200), None);
        assert_eq!(dsd_bpf(&dsd(64), 44_101), None);
        assert_eq!(dsd_bpf(&OutputMode::Dsp, 176_400), None);
    }

    /// Each channel's bytes go out in the same order, regrouped into the
    /// format's words, left then right — a little-endian word reversed.
    #[test]
    fn the_loops_bytes_are_regrouped_into_the_formats_words() {
        // Two loop frames of 4 bytes a channel: L = 0..8, R = 100..108.
        let src4: Vec<u8> = [0u8, 1, 2, 3, 100, 101, 102, 103, 4, 5, 6, 7, 104, 105, 106, 107].to_vec();
        let mut out = Vec::new();
        let be = DsdPack { w: 4, le: false, bpf: 4 };
        be.pack(2, &src4, &mut out);
        assert_eq!(out, src4, "DSD_U32_BE at four bytes a frame: as it is");
        DsdPack { w: 4, le: true, bpf: 4 }.pack(2, &src4, &mut out);
        assert_eq!(out, [3, 2, 1, 0, 103, 102, 101, 100, 7, 6, 5, 4, 107, 106, 105, 104]);
        // DSD1024: one loop frame of 8 bytes a channel is two DSD_U32 frames.
        let src8: Vec<u8> = (0..8).chain(100..108).collect();
        let p8 = DsdPack { w: 4, le: false, bpf: 8 };
        assert_eq!((p8.to_alsa(1), p8.to_loop(2), p8.to_loop(3)), (2, 1, 1));
        p8.pack(1, &src8, &mut out);
        assert_eq!(out, [0, 1, 2, 3, 100, 101, 102, 103, 4, 5, 6, 7, 104, 105, 106, 107]);
        // DSD64 at the DoP carrier: two loop frames of 2 bytes are one DSD_U32 frame.
        let src2 = [0u8, 1, 100, 101, 2, 3, 102, 103];
        let p2 = DsdPack { w: 4, le: false, bpf: 2 };
        assert_eq!((p2.to_alsa(2), p2.to_loop(1)), (1, 2));
        p2.pack(2, &src2, &mut out);
        assert_eq!(out, [0, 1, 2, 3, 100, 101, 102, 103]);
        // DSD_U8: a byte of each channel a frame.
        DsdPack { w: 1, le: false, bpf: 2 }.pack(1, &[7, 8, 9, 10], &mut out);
        assert_eq!(out, [7, 9, 8, 10]);
    }

    /// A DAC with a DSD format plays every DSD rate natively, at the rate
    /// the format counts it in; PCM, a DAC with none, or DoP asked for get
    /// none (DoP then).
    #[test]
    fn native_dsd_is_chosen_where_the_device_has_a_dsd_format() {
        let dac = dsd_dac();
        let u32be = |bpf| DsdPack { w: 4, le: false, bpf };
        assert_eq!(choose_native(&dsd(64), 176_400, &dac), Some((AlsaFmt::DsdU32Be, u32be(2), 88_200)));
        assert_eq!(choose_native(&dop64(), 176_400, &dac), Some((AlsaFmt::DsdU32Be, u32be(2), 88_200)));
        assert_eq!(choose_native(&dsd(256), 705_600, &dac), Some((AlsaFmt::DsdU32Be, u32be(2), 352_800)));
        assert_eq!(choose_native(&dsd(512), 705_600, &dac), Some((AlsaFmt::DsdU32Be, u32be(4), 705_600)));
        assert_eq!(choose_native(&dsd(1024), 705_600, &dac), Some((AlsaFmt::DsdU32Be, u32be(8), 1_411_200)));
        assert_eq!(choose_native(&OutputMode::Dsp, 176_400, &dac), None);
        assert_eq!(choose_native(&dsd(64), 176_400, &usb_dac()), None);
        // A format whose words split the loop's frames evenly first.
        let both = Caps { fmts: vec![AlsaFmt::DsdU32Be, AlsaFmt::DsdU16Le], rates: vec![88_200, 176_400] };
        assert_eq!(choose_native(&dsd(64), 176_400, &both), Some((AlsaFmt::DsdU16Le, DsdPack { w: 2, le: true, bpf: 2 }, 176_400)));
    }

    /// Past DSD256 a device with no DSD format is refused as one with no
    /// DoP format (the controller plays PCM and says why); DSD64-DSD256
    /// go on as DoP.
    #[test]
    fn dsd512_with_no_dsd_format_is_the_dop_format_error() {
        let e = native_refused(&dsd(512), &usb_dac(), "DAC").unwrap();
        assert!(is_dop_format_error(&e) && e.contains("no native DSD512") && e.contains("no DSD format"), "{e}");
        // A DSD format at other rates only is named.
        let low = Caps { fmts: vec![AlsaFmt::DsdU32Be], rates: vec![88_200, 176_400, 352_800] };
        let e = native_refused(&dsd(1024), &low, "DAC").unwrap();
        assert!(e.contains("DSD_U32_BE at other rates"), "{e}");
        assert_eq!(native_refused(&dsd(256), &usb_dac(), "DAC"), None);
        assert_eq!(native_refused(&OutputMode::Dsp, &usb_dac(), "DAC"), None);
    }

    /// The loop's device over a DSD_U32_BE PCM: room and writes counted in
    /// the loop's frames, the device handed its own, every byte once.
    #[test]
    fn native_dsd_room_and_writes_are_the_loops_frames() {
        // DSD1024: 8 bytes a channel a loop frame, two device frames each.
        let mut fake = FakePcm::default();
        fake.avail.extend([Ok(7), Ok(1_000)]);
        fake.writes.extend([Ok(3)]);
        let pack = DsdPack { w: 4, le: false, bpf: 8 };
        let mut dev = AlsaDev::new(fake, 10, 8).with_dsd(Some(pack));
        assert_eq!(dev.space(), Ok(3), "7 device frames hold 3 whole loop frames");
        assert_eq!(dev.space(), Ok(5), "capped to the chunk: 10 device frames");
        let src: Vec<u8> = (0..5 * 16).map(|i| i as u8).collect();
        dev.write(5, &src).unwrap();
        let mut want = Vec::new();
        pack.pack(5, &src, &mut want);
        assert_eq!(dev.pcm.written, want, "a short write goes on where it stopped");
        assert_eq!(want.len(), 10 * 8);
    }
}
