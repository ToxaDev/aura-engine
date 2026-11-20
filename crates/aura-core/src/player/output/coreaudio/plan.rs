//! CoreAudio's side of the output that needs no Mac: which rate and format a
//! device is asked for, how its IO buffer is sized, what its errors mean, and
//! in what order a device is taken and given back (`open`, `release`) — all
//! over the `Hal` trait, which the macOS backend implements with the HAL's
//! property calls and the tests with a made-up device.
//!
//! The way to the DAC unchanged: the device in hog mode (no other app, no
//! mixer), its nominal rate the stream's rate exactly (never resampled to
//! fit), its physical format an integer one of the depth the mode asks for.
//! The samples then reach CoreAudio either as those integers (a non-mixable
//! physical format: "integer mode") or as float32, which carries every 24-bit
//! value exactly and which CoreAudio turns into the device's integers with no
//! gain applied.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::player::output::{OutputMode, DOP_FORMAT_ERR};

// ── Formats ──────────────────────────────────────────────────────────────────

// CoreAudioBaseTypes.h, here so the logic runs on every system (the macOS
// backend's tests check them against the framework's own).
pub const LPCM: u32 = 0x6c70_636d; // 'lpcm'
pub const FLAG_FLOAT: u32 = 1 << 0;
pub const FLAG_BIG_ENDIAN: u32 = 1 << 1;
pub const FLAG_SIGNED: u32 = 1 << 2;
pub const FLAG_PACKED: u32 = 1 << 3;
pub const FLAG_ALIGNED_HIGH: u32 = 1 << 4;
pub const FLAG_NON_INTERLEAVED: u32 = 1 << 5;
pub const FLAG_NON_MIXABLE: u32 = 1 << 6;

/// An AudioStreamBasicDescription.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Asbd {
    pub rate: f64,
    pub format_id: u32,
    pub flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels: u32,
    pub bits: u32,
}

impl Asbd {
    /// Interleaved native-endian linear PCM of `kind`.
    pub fn pcm(kind: Kind, rate: f64, channels: u32) -> Asbd {
        let flags = match kind {
            Kind::Float32 => FLAG_FLOAT | FLAG_PACKED,
            Kind::Int24In32 => FLAG_SIGNED | FLAG_ALIGNED_HIGH,
            _ => FLAG_SIGNED | FLAG_PACKED,
        };
        let frame = kind.bytes() * channels;
        Asbd {
            rate,
            format_id: LPCM,
            flags,
            bytes_per_packet: frame,
            frames_per_packet: 1,
            bytes_per_frame: frame,
            channels,
            bits: kind.bits(),
        }
    }

    pub fn non_mixable(&self) -> bool {
        self.flags & FLAG_NON_MIXABLE != 0
    }
}

/// An AudioStreamRangedDescription: a format and the rates it runs at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranged {
    pub fmt: Asbd,
    pub min_rate: f64,
    pub max_rate: f64,
}

/// The sample layouts the output packs (`period::SampleFmt`), 24 in 32 apart:
/// the device's physical format says which it is, the packer writes both alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int16,
    Int24Packed,
    Int24In32,
    Int32,
    Float32,
}

impl Kind {
    pub fn bytes(self) -> u32 {
        match self {
            Kind::Int16 => 2,
            Kind::Int24Packed => 3,
            _ => 4,
        }
    }

    pub fn bits(self) -> u32 {
        match self {
            Kind::Int16 => 16,
            Kind::Int24Packed | Kind::Int24In32 => 24,
            _ => 32,
        }
    }

    pub fn is_int(self) -> bool {
        self != Kind::Float32
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Int16 => "int16",
            Kind::Int24Packed => "int24",
            Kind::Int24In32 => "int32 (24-in-32)",
            Kind::Int32 => "int32",
            Kind::Float32 => "float32",
        }
    }
}

/// What a format is, for the packer: None for anything it cannot write —
/// not linear PCM, big-endian, one buffer per channel, 24 bits low in 32,
/// unsigned or odd widths.
pub fn kind_of(a: &Asbd) -> Option<Kind> {
    if a.format_id != LPCM
        || a.flags & (FLAG_BIG_ENDIAN | FLAG_NON_INTERLEAVED) != 0
        || a.channels == 0
        || a.frames_per_packet != 1
        || a.bytes_per_frame == 0
        || a.bytes_per_frame % a.channels != 0
    {
        return None;
    }
    let width = a.bytes_per_frame / a.channels;
    let float = a.flags & FLAG_FLOAT != 0;
    let signed = a.flags & FLAG_SIGNED != 0;
    match (float, a.bits, width) {
        (true, 32, 4) => Some(Kind::Float32),
        (false, 16, 2) if signed => Some(Kind::Int16),
        (false, 24, 3) if signed => Some(Kind::Int24Packed),
        (false, 24, 4) if signed && a.flags & FLAG_ALIGNED_HIGH != 0 => Some(Kind::Int24In32),
        (false, 32, 4) if signed => Some(Kind::Int32),
        _ => None,
    }
}

/// What the stream needs of the device's physical format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The DSP chain's output, dithered on the 24-bit grid.
    Dsp,
    /// Bit-perfect at the source's depth (0: no integer depth, the widest).
    Direct(u32),
    /// DoP: a 24-bit integer word, never float.
    Dop,
}

impl Want {
    pub fn of(mode: &OutputMode) -> Want {
        match mode {
            OutputMode::Dsp => Want::Dsp,
            OutputMode::Direct { source_bits } => Want::Direct(*source_bits),
            OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. } => Want::Dop,
        }
    }

    /// The physical formats in the order they are asked for — WASAPI's
    /// order (`platform::*_candidates`), plus what a Mac's drivers offer
    /// more often (24 packed for DSP, float32 last for DSP only).
    pub fn priority(self) -> &'static [Kind] {
        use Kind::*;
        match self {
            Want::Dsp => &[Int24In32, Int32, Int24Packed, Float32, Int16],
            Want::Direct(0) => &[Int32, Int24In32, Int24Packed, Int16],
            Want::Direct(16) => &[Int16, Int24In32, Int24Packed, Int32],
            Want::Direct(24) => &[Int24In32, Int24Packed, Int32],
            Want::Direct(_) => &[Int32],
            Want::Dop => &[Int24In32, Int24Packed, Int32],
        }
    }
}

// ── Rates ────────────────────────────────────────────────────────────────────

/// Whether `rate` is in one of the device's nominal rate ranges (a single
/// rate is a range of one).
pub fn rate_ok(ranges: &[(f64, f64)], rate: u32) -> bool {
    let r = rate as f64;
    ranges.iter().any(|&(lo, hi)| r >= lo - 0.5 && r <= hi + 0.5)
}

fn khz(hz: f64) -> String {
    let k = hz / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{}", k.round() as u64)
    } else {
        format!("{}", (k * 10.0).round() / 10.0)
    }
}

/// "44.1, 48, 96 kHz" — a range as "8–192".
pub fn rates_text(ranges: &[(f64, f64)]) -> String {
    if ranges.is_empty() {
        return "no rates".into();
    }
    let parts: Vec<String> = ranges
        .iter()
        .map(|&(lo, hi)| if (hi - lo).abs() < 0.5 { khz(lo) } else { format!("{}–{}", khz(lo), khz(hi)) })
        .collect();
    format!("{} kHz", parts.join(", "))
}

pub fn rate_error(rate: u32, ranges: &[(f64, f64)]) -> String {
    format!(
        "The device does not run at {} kHz (it offers {}). The stream is never resampled to fit: choose a rate the device takes.",
        khz(rate as f64),
        rates_text(ranges)
    )
}

// ── Picking the physical format ──────────────────────────────────────────────

fn describe(r: &Ranged) -> String {
    let kind = kind_of(&r.fmt).map_or_else(|| format!("{} bit", r.fmt.bits), |k| k.label().to_string());
    let mix = if r.fmt.non_mixable() { ", non-mixable" } else { "" };
    format!("{kind} {}ch {}{mix}", r.fmt.channels, rates_text(&[(r.min_rate, r.max_rate)]))
}

/// The physical format to set for `want` at `rate`: the first kind in the
/// mode's order the device has at that rate in two channels or more; of
/// those, non-mixable first in hog mode (mixable first otherwise), then two
/// channels, then the fewest.
pub fn pick_physical(avail: &[Ranged], rate: u32, want: Want, hog: bool) -> Result<Asbd, String> {
    let r = rate as f64;
    for &kind in want.priority() {
        let best = avail
            .iter()
            .filter(|f| kind_of(&f.fmt) == Some(kind) && f.fmt.channels >= 2)
            .filter(|f| r >= f.min_rate - 0.5 && r <= f.max_rate + 0.5)
            .min_by_key(|f| (f.fmt.non_mixable() != hog, f.fmt.channels != 2, f.fmt.channels));
        if let Some(f) = best {
            return Ok(Asbd { rate: r, ..f.fmt });
        }
    }
    let offered: Vec<String> = avail.iter().map(describe).collect();
    let offered = if offered.is_empty() { "nothing".to_string() } else { offered.join("; ") };
    Err(match want {
        Want::Dop => format!("{DOP_FORMAT_ERR}; at {} kHz the device offers: {offered}", khz(r)),
        Want::Dsp => format!("No stereo PCM format at {} kHz; the device offers: {offered}", khz(r)),
        Want::Direct(bits) => format!(
            "No integer format for a {}-bit source at {} kHz; the device offers: {offered}",
            if bits == 0 { 32 } else { bits },
            khz(r)
        ),
    })
}

// ── Buffer ───────────────────────────────────────────────────────────────────

/// The IO buffer: ~10 ms (like WASAPI exclusive), within the device's range.
pub fn buffer_frames(rate: u32, range: (f64, f64)) -> u32 {
    let want = (rate as f64 / 100.0).ceil();
    let lo = range.0.ceil().max(1.0);
    let hi = range.1.floor().max(lo);
    want.clamp(lo, hi) as u32
}

/// The ring between the render loop and the IOProc: two IO buffers.
pub fn ring_frames(io_frames: u32) -> usize {
    2 * io_frames as usize
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// An OSStatus the HAL returned, named, with its four characters.
pub fn status_text(code: i32) -> String {
    let what = match code as u32 {
        0x7374_6f70 => "the device is not running",                       // 'stop'
        0x7768_6174 => "unspecified error",                               // 'what'
        0x7768_6f3f => "the device does not have this property",          // 'who?'
        0x2173_697a => "bad property size",                               // '!siz'
        0x6e6f_7065 => "the device refused the operation",                // 'nope'
        0x216f_626a => "the device is gone",                              // '!obj'
        0x2164_6576 => "the device is gone",                              // '!dev'
        0x2173_7472 => "the stream is gone",                              // '!str'
        0x756e_6f70 => "the device does not support this",                // 'unop'
        0x6e72_6479 => "the device is not ready",                         // 'nrdy'
        0x2164_6174 => "the device does not take this format",            // '!dat'
        0x2168_6f67 => "another app holds the device in exclusive mode",  // '!hog'
        _ => "",
    };
    let b = (code as u32).to_be_bytes();
    let tag = if b.iter().all(|c| (0x20..0x7f).contains(c)) {
        format!("'{}'", b.iter().map(|&c| c as char).collect::<String>())
    } else {
        format!("{code}")
    };
    if what.is_empty() {
        format!("OSStatus {tag}")
    } else {
        format!("{what} (OSStatus {tag})")
    }
}

pub fn busy_text(pid: i32, app: Option<&str>) -> String {
    let who = match app {
        Some(a) if !a.is_empty() => format!("{a}, pid {pid}"),
        _ => format!("pid {pid}"),
    };
    format!("The device is in exclusive use by another app ({who}). Stop its playback or quit it, then press Play again.")
}

/// Why a stream did not open: a device held by another app or a rate it does
/// not run at stay so in shared mode too; a format it lacks may not.
#[derive(Clone, Debug, PartialEq)]
pub enum OpenError {
    Busy(String),
    Rate(String),
    Format(String),
    Hal(String),
}

impl OpenError {
    pub fn text(self) -> String {
        match self {
            OpenError::Busy(s) | OpenError::Rate(s) | OpenError::Format(s) | OpenError::Hal(s) => s,
        }
    }
}

fn hal(what: &str, code: i32) -> OpenError {
    if code as u32 == 0x2168_6f67 {
        return OpenError::Busy(format!("{what}: {}", status_text(code)));
    }
    OpenError::Hal(format!("{what}: {}", status_text(code)))
}

// ── The device, taken and given back ─────────────────────────────────────────

pub type Id = u32;
pub type Status = i32;

/// The HAL calls the output makes (AudioObjectGet/SetPropertyData on the
/// properties named), for the macOS backend and a made-up device.
pub trait Hal {
    fn default_output(&self) -> Result<Id, Status>;
    fn devices(&self) -> Result<Vec<Id>, Status>;
    fn uid(&self, dev: Id) -> Result<String, Status>;
    fn name(&self, dev: Id) -> Result<String, Status>;
    /// Output channels over all its output streams (StreamConfiguration).
    fn output_channels(&self, dev: Id) -> Result<u32, Status>;
    /// HogMode: the pid holding the device, -1 for none.
    fn hog_owner(&self, dev: Id) -> Result<i32, Status>;
    /// Set HogMode — the HAL toggles it whatever is passed: taken when free,
    /// let go when ours, left when another's. The owner after it.
    fn toggle_hog(&mut self, dev: Id) -> Result<i32, Status>;
    fn process_name(&self, pid: i32) -> Option<String>;
    fn pid(&self) -> i32;
    fn nominal_rate(&self, dev: Id) -> Result<f64, Status>;
    fn nominal_rates(&self, dev: Id) -> Result<Vec<(f64, f64)>, Status>;
    /// Set NominalSampleRate and wait (bounded) until the device runs at it.
    fn set_nominal_rate(&mut self, dev: Id, rate: f64) -> Result<(), Status>;
    /// The first output stream.
    fn output_stream(&self, dev: Id) -> Result<Id, Status>;
    fn physical_formats(&self, stream: Id) -> Result<Vec<Ranged>, Status>;
    fn physical_format(&self, stream: Id) -> Result<Asbd, Status>;
    fn set_physical_format(&mut self, stream: Id, f: &Asbd) -> Result<(), Status>;
    fn virtual_format(&self, stream: Id) -> Result<Asbd, Status>;
    fn set_virtual_format(&mut self, stream: Id, f: &Asbd) -> Result<(), Status>;
    fn buffer_range(&self, dev: Id) -> Result<(f64, f64), Status>;
    fn buffer_frames(&self, dev: Id) -> Result<u32, Status>;
    fn set_buffer_frames(&mut self, dev: Id, n: u32) -> Result<(), Status>;
    /// Device and stream latency plus the safety offset, in frames.
    fn latency_frames(&self, dev: Id, stream: Id) -> u32;
}

/// A playable output device for the list: its UID (the id the settings keep),
/// its name, and whether it is the system's default output.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub uid: String,
    pub name: String,
    pub is_default: bool,
}

/// The devices with two output channels or more.
pub fn list(h: &impl Hal) -> Result<Vec<Listed>, String> {
    let default = h.default_output().unwrap_or(0);
    let devs = h.devices().map_err(|c| format!("device list: {}", status_text(c)))?;
    Ok(devs
        .into_iter()
        .filter(|&d| h.output_channels(d).is_ok_and(|n| n >= 2))
        .filter_map(|d| {
            let uid = h.uid(d).ok()?;
            let name = h.name(d).unwrap_or_else(|_| uid.clone());
            Some(Listed { uid, name, is_default: d == default })
        })
        .collect())
}

/// The device a stream is for: by UID, or the default output.
pub fn find(h: &impl Hal, uid: Option<&str>) -> Result<Id, String> {
    match uid {
        None => match h.default_output() {
            Ok(d) if d != 0 => Ok(d),
            _ => Err("There is no output device".into()),
        },
        Some(u) => {
            let devs = h.devices().map_err(|c| format!("device list: {}", status_text(c)))?;
            devs.into_iter()
                .find(|&d| h.uid(d).is_ok_and(|x| x == u))
                .ok_or_else(|| format!("The output device is not connected ({u})"))
        }
    }
}

/// What a stream changed on the device, with the values it had before:
/// given back in the reverse order (`release`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Taken {
    pub dev: Id,
    pub hog: bool,
    pub rate: Option<f64>,
    pub physical: Option<(Id, Asbd)>,
    pub virtual_fmt: Option<(Id, Asbd)>,
    pub buffer: Option<u32>,
}

/// An opened device, ready for its IOProc.
#[derive(Clone, Debug)]
pub struct Opened {
    pub dev: Id,
    pub stream: Id,
    pub name: String,
    /// What the IOProc is handed to fill.
    pub io: Asbd,
    pub io_kind: Kind,
    /// The DAC's side; None in shared mode (left as it was).
    pub physical: Option<Asbd>,
    pub exclusive: bool,
    pub buffer_frames: u32,
    pub latency_frames: u32,
    pub taken: Taken,
    /// Lines for the log: what was chosen and why.
    pub notes: Vec<String>,
}

impl Opened {
    /// The format line the UI shows: what the DAC gets, and how it travels.
    pub fn format_text(&self) -> String {
        match self.physical.and_then(|p| kind_of(&p)) {
            Some(p) if self.io_kind.is_int() => format!("{} (integer mode)", p.label()),
            Some(p) if p.is_int() => format!("{} (float32 to CoreAudio)", p.label()),
            Some(p) => p.label().to_string(),
            None => format!("{} (shared)", self.io_kind.label()),
        }
    }
}

/// Take the device for a stream at `rate`: hog mode (`exclusive`), the exact
/// nominal rate, the physical format `want` asks for (exclusive only), the
/// IO format, a ~10 ms buffer. Whatever was changed before a failure is given
/// back before it returns (`release`, the rate at once).
pub fn open(h: &mut impl Hal, dev: Id, rate: u32, want: Want, exclusive: bool) -> Result<Opened, OpenError> {
    let mut taken = Taken { dev, ..Taken::default() };
    match open_steps(h, dev, rate, want, exclusive, &mut taken) {
        Ok(o) => Ok(o),
        Err(e) => {
            if let Some(rate) = release(h, &taken) {
                h.set_nominal_rate(dev, rate).ok();
            }
            Err(e)
        }
    }
}

fn open_steps(h: &mut impl Hal, dev: Id, rate: u32, want: Want, exclusive: bool, taken: &mut Taken) -> Result<Opened, OpenError> {
    let mut notes = Vec::new();
    let name = h.name(dev).unwrap_or_else(|_| format!("device {dev}"));
    let me = h.pid();
    let owner = h.hog_owner(dev).unwrap_or(-1);
    if owner != -1 && owner != me {
        return Err(OpenError::Busy(busy_text(owner, h.process_name(owner).as_deref())));
    }
    if exclusive && owner != me {
        let now = h.toggle_hog(dev).map_err(|c| hal("hog mode", c))?;
        if now != me {
            if now != -1 {
                return Err(OpenError::Busy(busy_text(now, h.process_name(now).as_deref())));
            }
            return Err(OpenError::Hal("hog mode: the device did not give exclusive access".into()));
        }
        taken.hog = true;
    }

    // The exact rate, or a clear error — never resampled to fit.
    let ranges = h.nominal_rates(dev).map_err(|c| hal("sample rates", c))?;
    if !rate_ok(&ranges, rate) {
        return Err(OpenError::Rate(rate_error(rate, &ranges)));
    }
    let was = h.nominal_rate(dev).map_err(|c| hal("sample rate", c))?;
    if (was - rate as f64).abs() > 0.5 {
        taken.rate = Some(was);
        h.set_nominal_rate(dev, rate as f64).map_err(|c| hal(&format!("set {} kHz", khz(rate as f64)), c))?;
    }
    let now = h.nominal_rate(dev).map_err(|c| hal("sample rate", c))?;
    if (now - rate as f64).abs() > 0.5 {
        return Err(OpenError::Rate(format!(
            "The device stayed at {} kHz when asked for {} kHz",
            khz(now),
            khz(rate as f64)
        )));
    }

    let stream = h.output_stream(dev).map_err(|c| hal("output stream", c))?;

    // The DAC's side: the integer format of the depth asked for.
    let physical = if exclusive {
        let avail = h.physical_formats(stream).map_err(|c| hal("physical formats", c))?;
        let pick = pick_physical(&avail, rate, want, true).map_err(OpenError::Format)?;
        let before = h.physical_format(stream).map_err(|c| hal("physical format", c))?;
        if before != pick {
            taken.physical = Some((stream, before));
            h.set_physical_format(stream, &pick).map_err(|c| hal("set physical format", c))?;
        }
        let got = h.physical_format(stream).map_err(|c| hal("physical format", c))?;
        if kind_of(&got) != kind_of(&pick) || (got.rate - rate as f64).abs() > 0.5 {
            return Err(OpenError::Format(format!(
                "The device kept {} at {} kHz when asked for {} at {} kHz",
                kind_of(&got).map_or("another format", |k| k.label()),
                khz(got.rate),
                kind_of(&pick).map_or("?", |k| k.label()),
                khz(rate as f64)
            )));
        }
        notes.push(format!(
            "physical format {}{} {}ch",
            kind_of(&got).map_or("?", |k| k.label()),
            if got.non_mixable() { " non-mixable" } else { "" },
            got.channels
        ));
        Some(got)
    } else {
        None
    };

    // What the IOProc fills: the physical integers themselves when the
    // driver hands them through (a non-mixable format in hog mode), else
    // float32, which DoP cannot travel in.
    let virt = h.virtual_format(stream).map_err(|c| hal("IO format", c))?;
    let integer = physical.is_some_and(|p| p.non_mixable() && kind_of(&virt) == kind_of(&p) && kind_of(&p).is_some_and(Kind::is_int));
    let (io, io_kind) = if integer {
        notes.push("integer mode: the samples reach the driver as they are".into());
        (virt, kind_of(&virt).unwrap_or(Kind::Int32))
    } else {
        if want == Want::Dop {
            return Err(OpenError::Format(format!(
                "{DOP_FORMAT_ERR}; the driver takes only float32 samples (no non-mixable integer format)"
            )));
        }
        let channels = physical.map_or(virt.channels, |p| p.channels).max(2);
        let float = Asbd::pcm(Kind::Float32, rate as f64, channels);
        if kind_of(&virt) != Some(Kind::Float32) || (virt.rate - rate as f64).abs() > 0.5 || virt.channels != channels {
            taken.virtual_fmt = Some((stream, virt));
            h.set_virtual_format(stream, &float).map_err(|c| hal("set IO format float32", c))?;
        }
        let got = h.virtual_format(stream).map_err(|c| hal("IO format", c))?;
        if kind_of(&got) != Some(Kind::Float32) {
            return Err(OpenError::Format("The device would not take float32 samples".into()));
        }
        if physical.is_some() {
            notes.push(format!(
                "float32 to CoreAudio: the driver has no non-mixable integer format{}; float32 carries 24-bit samples exactly, \
                 CoreAudio converts them with no gain",
                if want == Want::Direct(32) || want == Want::Direct(0) { " (a 32-bit source loses its lowest 8 bits)" } else { "" }
            ));
        } else {
            notes.push("shared mode: float32 through the system mixer (other apps may play along)".into());
        }
        (got, Kind::Float32)
    };
    if io.channels < 2 {
        return Err(OpenError::Format(format!("The device has {} output channel(s); stereo needs 2", io.channels)));
    }

    let range = h.buffer_range(dev).map_err(|c| hal("buffer range", c))?;
    let frames = buffer_frames(rate, range);
    let before = h.buffer_frames(dev).map_err(|c| hal("buffer size", c))?;
    if before != frames {
        taken.buffer = Some(before);
        h.set_buffer_frames(dev, frames).map_err(|c| hal("set buffer size", c))?;
    }
    let buffer = h.buffer_frames(dev).map_err(|c| hal("buffer size", c))?.max(1);
    let latency = h.latency_frames(dev, stream) + buffer;

    Ok(Opened {
        dev,
        stream,
        name,
        io,
        io_kind,
        physical,
        exclusive,
        buffer_frames: buffer,
        latency_frames: latency,
        taken: taken.clone(),
        notes,
    })
}

/// Give the device back what a stream changed: the buffer, the IO and
/// physical formats, hog mode — in that order. The nominal rate is not set
/// back here: it is returned, for the caller to set back once no stream
/// follows on the same device (`Leftovers`), so the DAC does not relock
/// between two tracks of the same rate.
pub fn release(h: &mut impl Hal, t: &Taken) -> Option<f64> {
    if let Some(n) = t.buffer {
        h.set_buffer_frames(t.dev, n).ok();
    }
    if let Some((s, f)) = &t.virtual_fmt {
        h.set_virtual_format(*s, f).ok();
    }
    if let Some((s, f)) = &t.physical {
        h.set_physical_format(*s, f).ok();
    }
    if t.hog && h.hog_owner(t.dev).is_ok_and(|o| o == h.pid()) {
        h.toggle_hog(t.dev).ok();
    }
    t.rate
}

/// Nominal rates to set back once streams stop: a device's rate before the
/// first of a run of streams, kept while the next stream opens within
/// `grace`, set back when none did.
pub struct Leftovers {
    pending: Vec<(Id, f64, std::time::Instant)>,
    grace: std::time::Duration,
}

impl Leftovers {
    pub const fn new(grace: std::time::Duration) -> Leftovers {
        Leftovers { pending: Vec::new(), grace }
    }

    /// A stream stopped: `dev` goes back to `rate` after the grace, unless
    /// one already waits (that one is older: the rate before them all).
    pub fn put(&mut self, dev: Id, rate: f64, now: std::time::Instant) {
        if !self.pending.iter().any(|p| p.0 == dev) {
            self.pending.push((dev, rate, now + self.grace));
        }
    }

    /// A stream opens on `dev`: the rate to set back after it, if one waits.
    pub fn take(&mut self, dev: Id) -> Option<f64> {
        let i = self.pending.iter().position(|p| p.0 == dev)?;
        Some(self.pending.remove(i).1)
    }

    /// The rates due by `now`, taken out.
    pub fn due(&mut self, now: std::time::Instant) -> Vec<(Id, f64)> {
        let (due, keep): (Vec<_>, Vec<_>) = self.pending.drain(..).partition(|p| p.2 <= now);
        self.pending = keep;
        due.into_iter().map(|p| (p.0, p.1)).collect()
    }

    /// Everything, now (the app closing).
    pub fn all(&mut self) -> Vec<(Id, f64)> {
        self.pending.drain(..).map(|p| (p.0, p.1)).collect()
    }

    /// When the next one is due.
    pub fn next_due(&self) -> Option<std::time::Instant> {
        self.pending.iter().map(|p| p.2).min()
    }
}

// ── Interleaving into the device's frame ─────────────────────────────────────

/// Copy `frames` stereo frames of `src_frame` bytes each into a device buffer
/// of `dst_frame` bytes a frame: the two channels first, the rest silent.
pub fn spread(src: &[u8], src_frame: usize, dst: &mut [u8], dst_frame: usize, frames: usize) {
    if src_frame == dst_frame {
        let n = frames * src_frame;
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    for i in 0..frames {
        let d = &mut dst[i * dst_frame..(i + 1) * dst_frame];
        d[..src_frame].copy_from_slice(&src[i * src_frame..(i + 1) * src_frame]);
        d[src_frame..].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── A made-up device ─────────────────────────────────────────────────────

    const DEV: Id = 40;
    const STREAM: Id = 41;
    const ME: i32 = 500;

    struct Fake {
        rates: Vec<(f64, f64)>,
        rate: f64,
        formats: Vec<Ranged>,
        physical: Asbd,
        virt: Asbd,
        hog: i32,
        buffer: u32,
        /// The driver passes non-mixable integers to the IOProc.
        passes_integers: bool,
        /// The device stays at its rate whatever it is told.
        stuck_rate: bool,
        fail_set_physical: Option<Status>,
        calls: Vec<String>,
    }

    fn int24_2ch(min: f64, max: f64, non_mixable: bool) -> Ranged {
        let mut f = Asbd::pcm(Kind::Int24Packed, 0.0, 2);
        if non_mixable {
            f.flags |= FLAG_NON_MIXABLE;
        }
        Ranged { fmt: f, min_rate: min, max_rate: max }
    }

    fn ranged(kind: Kind, ch: u32, min: f64, max: f64, non_mixable: bool) -> Ranged {
        let mut f = Asbd::pcm(kind, 0.0, ch);
        if non_mixable {
            f.flags |= FLAG_NON_MIXABLE;
        }
        Ranged { fmt: f, min_rate: min, max_rate: max }
    }

    impl Fake {
        /// A USB DAC: 44.1–384 kHz, int16/int24-in-32/int32, mixable and
        /// non-mixable, its driver passing integers through.
        fn dac() -> Fake {
            let rates: Vec<(f64, f64)> =
                [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0, 352_800.0, 384_000.0].iter().map(|&r| (r, r)).collect();
            let mut formats = Vec::new();
            for nm in [false, true] {
                for k in [Kind::Int16, Kind::Int24In32, Kind::Int32] {
                    formats.push(ranged(k, 2, 44_100.0, 384_000.0, nm));
                }
            }
            Fake {
                rates,
                rate: 44_100.0,
                physical: Asbd { rate: 44_100.0, ..Asbd::pcm(Kind::Int24In32, 0.0, 2) },
                virt: Asbd::pcm(Kind::Float32, 44_100.0, 2),
                formats,
                hog: -1,
                buffer: 512,
                passes_integers: true,
                stuck_rate: false,
                fail_set_physical: None,
                calls: Vec::new(),
            }
        }

        fn log(&mut self, s: String) {
            self.calls.push(s);
        }
    }

    impl Hal for Fake {
        fn default_output(&self) -> Result<Id, Status> {
            Ok(DEV)
        }
        fn devices(&self) -> Result<Vec<Id>, Status> {
            Ok(vec![DEV, 77])
        }
        fn uid(&self, dev: Id) -> Result<String, Status> {
            Ok(format!("uid-{dev}"))
        }
        fn name(&self, dev: Id) -> Result<String, Status> {
            Ok(if dev == DEV { "Fake DAC".into() } else { "Mic".into() })
        }
        fn output_channels(&self, dev: Id) -> Result<u32, Status> {
            Ok(if dev == DEV { 2 } else { 0 })
        }
        fn hog_owner(&self, _: Id) -> Result<i32, Status> {
            Ok(self.hog)
        }
        fn toggle_hog(&mut self, _: Id) -> Result<i32, Status> {
            if self.hog == -1 {
                self.hog = ME;
                self.log("hog on".into());
            } else if self.hog == ME {
                self.hog = -1;
                self.log("hog off".into());
            }
            Ok(self.hog)
        }
        fn process_name(&self, pid: i32) -> Option<String> {
            (pid == 66).then(|| "Music".into())
        }
        fn pid(&self) -> i32 {
            ME
        }
        fn nominal_rate(&self, _: Id) -> Result<f64, Status> {
            Ok(self.rate)
        }
        fn nominal_rates(&self, _: Id) -> Result<Vec<(f64, f64)>, Status> {
            Ok(self.rates.clone())
        }
        fn set_nominal_rate(&mut self, _: Id, rate: f64) -> Result<(), Status> {
            self.log(format!("rate {rate}"));
            if !self.stuck_rate {
                self.rate = rate;
                self.physical.rate = rate;
                self.virt.rate = rate;
            }
            Ok(())
        }
        fn output_stream(&self, _: Id) -> Result<Id, Status> {
            Ok(STREAM)
        }
        fn physical_formats(&self, _: Id) -> Result<Vec<Ranged>, Status> {
            Ok(self.formats.clone())
        }
        fn physical_format(&self, _: Id) -> Result<Asbd, Status> {
            Ok(self.physical)
        }
        fn set_physical_format(&mut self, _: Id, f: &Asbd) -> Result<(), Status> {
            if let Some(c) = self.fail_set_physical {
                return Err(c);
            }
            self.log(format!("physical {}{}", kind_of(f).unwrap().label(), if f.non_mixable() { " nm" } else { "" }));
            self.physical = *f;
            // A non-mixable format in hog mode: the IO format follows it
            // when the driver passes integers through.
            self.virt = if f.non_mixable() && self.hog == ME && self.passes_integers {
                *f
            } else {
                Asbd::pcm(Kind::Float32, f.rate, f.channels)
            };
            Ok(())
        }
        fn virtual_format(&self, _: Id) -> Result<Asbd, Status> {
            Ok(self.virt)
        }
        fn set_virtual_format(&mut self, _: Id, f: &Asbd) -> Result<(), Status> {
            self.log(format!("virtual {}", kind_of(f).unwrap().label()));
            self.virt = *f;
            Ok(())
        }
        fn buffer_range(&self, _: Id) -> Result<(f64, f64), Status> {
            Ok((32.0, 4096.0))
        }
        fn buffer_frames(&self, _: Id) -> Result<u32, Status> {
            Ok(self.buffer)
        }
        fn set_buffer_frames(&mut self, _: Id, n: u32) -> Result<(), Status> {
            self.log(format!("buffer {n}"));
            self.buffer = n;
            Ok(())
        }
        fn latency_frames(&self, _: Id, _: Id) -> u32 {
            24
        }
    }

    // ── Formats ──────────────────────────────────────────────────────────────

    #[test]
    fn formats_are_told_apart_by_bits_width_and_flags() {
        for k in [Kind::Int16, Kind::Int24Packed, Kind::Int24In32, Kind::Int32, Kind::Float32] {
            assert_eq!(kind_of(&Asbd::pcm(k, 48_000.0, 2)), Some(k), "{k:?}");
            assert_eq!(kind_of(&Asbd::pcm(k, 48_000.0, 8)), Some(k), "{k:?} 8ch");
        }
        // 24 bits low in 32, big-endian, non-interleaved, unsigned, not PCM.
        let mut low = Asbd::pcm(Kind::Int24In32, 48_000.0, 2);
        low.flags &= !FLAG_ALIGNED_HIGH;
        assert_eq!(kind_of(&low), None);
        let mut be = Asbd::pcm(Kind::Int32, 48_000.0, 2);
        be.flags |= FLAG_BIG_ENDIAN;
        assert_eq!(kind_of(&be), None);
        let mut ni = Asbd::pcm(Kind::Float32, 48_000.0, 2);
        ni.flags |= FLAG_NON_INTERLEAVED;
        assert_eq!(kind_of(&ni), None);
        let mut unsigned = Asbd::pcm(Kind::Int16, 48_000.0, 2);
        unsigned.flags &= !FLAG_SIGNED;
        assert_eq!(kind_of(&unsigned), None);
        let ac3 = Asbd { format_id: 0x6163_2d33, ..Asbd::pcm(Kind::Int16, 48_000.0, 2) };
        assert_eq!(kind_of(&ac3), None);
    }

    #[test]
    fn each_mode_gets_its_depth_in_wasapis_order() {
        let dac = Fake::dac().formats;
        let k = |want| kind_of(&pick_physical(&dac, 96_000, want, true).unwrap()).unwrap();
        assert_eq!(k(Want::Dsp), Kind::Int24In32);
        assert_eq!(k(Want::Direct(16)), Kind::Int16);
        assert_eq!(k(Want::Direct(24)), Kind::Int24In32);
        assert_eq!(k(Want::Direct(32)), Kind::Int32);
        assert_eq!(k(Want::Direct(0)), Kind::Int32);
        assert_eq!(k(Want::Dop), Kind::Int24In32);
        // The rate goes into the format set.
        assert_eq!(pick_physical(&dac, 96_000, Want::Dsp, true).unwrap().rate, 96_000.0);
    }

    #[test]
    fn hog_mode_prefers_non_mixable_and_stereo() {
        let mut avail = vec![
            ranged(Kind::Int24In32, 8, 44_100.0, 192_000.0, true),
            ranged(Kind::Int24In32, 2, 44_100.0, 192_000.0, false),
            ranged(Kind::Int24In32, 2, 44_100.0, 192_000.0, true),
        ];
        let p = pick_physical(&avail, 48_000, Want::Dsp, true).unwrap();
        assert!(p.non_mixable() && p.channels == 2);
        let p = pick_physical(&avail, 48_000, Want::Dsp, false).unwrap();
        assert!(!p.non_mixable() && p.channels == 2);
        // Only an 8-channel interface: it is taken, the channels past two silent.
        avail.retain(|r| r.fmt.channels == 8);
        assert_eq!(pick_physical(&avail, 48_000, Want::Dsp, true).unwrap().channels, 8);
    }

    #[test]
    fn a_format_the_device_lacks_at_the_rate_is_a_clear_error() {
        // 24-bit only up to 192k: DoP at 352.8k has nothing to travel in.
        let avail = vec![int24_2ch(44_100.0, 192_000.0, false), ranged(Kind::Float32, 2, 44_100.0, 384_000.0, false)];
        let e = pick_physical(&avail, 352_800, Want::Dop, true).unwrap_err();
        assert!(crate::player::output::is_dop_format_error(&e), "{e}");
        assert!(e.contains("352.8 kHz") && e.contains("int24 2ch 44.1–192 kHz"), "{e}");
        // A 24-bit source never goes out as float.
        let e = pick_physical(&avail, 352_800, Want::Direct(24), true).unwrap_err();
        assert!(e.contains("24-bit source") && e.contains("float32 2ch"), "{e}");
        // DSP takes float last.
        assert_eq!(kind_of(&pick_physical(&avail, 352_800, Want::Dsp, true).unwrap()), Some(Kind::Float32));
        // Mono only: no stereo format.
        let mono = vec![ranged(Kind::Int16, 1, 44_100.0, 48_000.0, false)];
        assert!(pick_physical(&mono, 48_000, Want::Dsp, true).is_err());
    }

    #[test]
    fn rates_are_exact_and_said_plainly() {
        let r = Fake::dac().rates;
        assert!(rate_ok(&r, 352_800) && rate_ok(&r, 44_100));
        assert!(!rate_ok(&r, 705_600) && !rate_ok(&r, 50_000));
        assert!(rate_ok(&[(8_000.0, 192_000.0)], 50_000));
        let e = rate_error(705_600, &r);
        assert!(e.contains("705.6 kHz") && e.contains("44.1, 48, 88.2, 96, 176.4, 192, 352.8, 384 kHz") && e.contains("never resampled"), "{e}");
        assert_eq!(rates_text(&[(8_000.0, 192_000.0)]), "8–192 kHz");
    }

    #[test]
    fn the_buffer_is_ten_ms_within_the_range() {
        assert_eq!(buffer_frames(44_100, (14.0, 4096.0)), 441);
        assert_eq!(buffer_frames(705_600, (14.0, 4096.0)), 4096);
        assert_eq!(buffer_frames(48_000, (512.0, 4096.0)), 512);
        assert_eq!(buffer_frames(48_000, (0.0, 0.0)), 1);
        assert_eq!(ring_frames(441), 882);
    }

    #[test]
    fn statuses_are_named() {
        assert_eq!(status_text(0x2168_6f67), "another app holds the device in exclusive mode (OSStatus '!hog')");
        assert_eq!(status_text(0x216f_626a), "the device is gone (OSStatus '!obj')");
        assert_eq!(status_text(0x7a7a_7a7a), "OSStatus 'zzzz'");
        assert_eq!(status_text(-50), "OSStatus -50");
        assert!(busy_text(66, Some("Music")).contains("(Music, pid 66)"));
        assert!(busy_text(66, None).contains("(pid 66)"));
    }

    // ── Open and give back ───────────────────────────────────────────────────

    #[test]
    fn an_exclusive_open_takes_hog_then_rate_then_format_then_buffer() {
        let mut f = Fake::dac();
        let o = open(&mut f, DEV, 96_000, Want::Direct(24), true).unwrap();
        assert_eq!(f.calls, ["hog on", "rate 96000", "physical int32 (24-in-32) nm", "buffer 960"]);
        assert!(o.exclusive && o.io_kind == Kind::Int24In32 && o.io.non_mixable());
        assert_eq!(o.format_text(), "int32 (24-in-32) (integer mode)");
        assert_eq!((o.buffer_frames, o.latency_frames), (960, 984));
        assert_eq!(o.name, "Fake DAC");

        // Given back in reverse: buffer, physical, hog; the rate is returned.
        f.calls.clear();
        assert_eq!(release(&mut f, &o.taken), Some(44_100.0));
        assert_eq!(f.calls, ["buffer 512", "physical int32 (24-in-32)", "hog off"]);
        assert_eq!(f.hog, -1);
    }

    #[test]
    fn a_driver_that_will_not_pass_integers_gets_float32() {
        let mut f = Fake::dac();
        f.passes_integers = false;
        let o = open(&mut f, DEV, 48_000, Want::Direct(16), true).unwrap();
        assert_eq!(o.io_kind, Kind::Float32);
        assert_eq!(kind_of(&o.physical.unwrap()), Some(Kind::Int16));
        assert_eq!(o.format_text(), "int16 (float32 to CoreAudio)");
        assert!(o.notes.iter().any(|n| n.contains("float32 carries 24-bit samples exactly")));
        // DoP cannot: the controller plays PCM instead, and nothing is left taken.
        let mut f = Fake::dac();
        f.passes_integers = false;
        let e = open(&mut f, DEV, 176_400, Want::Dop, true).unwrap_err().text();
        assert!(crate::player::output::is_dop_format_error(&e), "{e}");
        assert_eq!(f.hog, -1);
        assert_eq!(f.rate, 44_100.0, "the rate set back at once");
        assert_eq!(kind_of(&f.physical), Some(Kind::Int24In32));
    }

    #[test]
    fn a_device_another_app_holds_is_refused_by_name() {
        let mut f = Fake::dac();
        f.hog = 66;
        let e = open(&mut f, DEV, 44_100, Want::Dsp, true).unwrap_err();
        assert!(matches!(&e, OpenError::Busy(s) if s.contains("Music, pid 66")), "{e:?}");
        // Shared mode cannot play through it either.
        assert!(matches!(open(&mut f, DEV, 44_100, Want::Dsp, false), Err(OpenError::Busy(_))));
        assert!(f.calls.is_empty(), "nothing touched: {:?}", f.calls);
    }

    #[test]
    fn a_rate_the_device_lacks_is_refused_and_nothing_stays_changed() {
        let mut f = Fake::dac();
        let e = open(&mut f, DEV, 705_600, Want::Dsp, true).unwrap_err();
        assert!(matches!(&e, OpenError::Rate(s) if s.contains("705.6")), "{e:?}");
        assert_eq!(f.calls, ["hog on", "hog off"]);
        // One that says yes and stays where it was.
        let mut f = Fake::dac();
        f.stuck_rate = true;
        let e = open(&mut f, DEV, 96_000, Want::Dsp, true).unwrap_err();
        assert!(matches!(&e, OpenError::Rate(s) if s.contains("stayed at 44.1")), "{e:?}");
        assert_eq!(f.hog, -1);
    }

    #[test]
    fn a_failure_midway_gives_everything_back() {
        let mut f = Fake::dac();
        f.fail_set_physical = Some(0x2164_6174);
        let e = open(&mut f, DEV, 96_000, Want::Dsp, true).unwrap_err();
        assert!(matches!(&e, OpenError::Hal(s) if s.contains("does not take this format")), "{e:?}");
        assert_eq!(f.calls, ["hog on", "rate 96000", "hog off", "rate 44100"]);
    }

    #[test]
    fn shared_mode_keeps_the_rate_exact_and_leaves_the_dac_format() {
        let mut f = Fake::dac();
        let o = open(&mut f, DEV, 88_200, Want::Dsp, false).unwrap();
        assert!(!o.exclusive && o.physical.is_none() && o.io_kind == Kind::Float32);
        assert_eq!(f.calls, ["rate 88200", "buffer 882"]);
        assert_eq!(o.format_text(), "float32 (shared)");
        f.calls.clear();
        assert_eq!(release(&mut f, &o.taken), Some(44_100.0));
        assert_eq!(f.calls, ["buffer 512"]);
    }

    #[test]
    fn the_same_rate_twice_changes_nothing() {
        let mut f = Fake::dac();
        f.rate = 96_000.0;
        f.buffer = 960;
        let o = open(&mut f, DEV, 96_000, Want::Direct(24), true).unwrap();
        assert_eq!(f.calls, ["hog on", "physical int32 (24-in-32) nm"]);
        assert_eq!(o.taken.rate, None);
    }

    #[test]
    fn the_list_has_playable_devices_and_find_goes_by_uid() {
        let f = Fake::dac();
        assert_eq!(list(&f).unwrap(), [Listed { uid: "uid-40".into(), name: "Fake DAC".into(), is_default: true }]);
        assert_eq!(find(&f, None), Ok(DEV));
        assert_eq!(find(&f, Some("uid-40")), Ok(DEV));
        assert!(find(&f, Some("uid-9")).unwrap_err().contains("not connected (uid-9)"));
    }

    #[test]
    fn a_rate_is_set_back_only_when_no_stream_follows() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut l = Leftovers::new(Duration::from_secs(3));
        l.put(DEV, 44_100.0, t0);
        // The next track opens within the grace: it carries the old rate on.
        assert_eq!(l.take(DEV), Some(44_100.0));
        assert_eq!(l.next_due(), None);
        l.put(DEV, 44_100.0, t0);
        l.put(DEV, 96_000.0, t0); // a later stream's "before" is not the original
        assert!(l.due(t0 + Duration::from_secs(2)).is_empty());
        assert_eq!(l.due(t0 + Duration::from_secs(3)), [(DEV, 44_100.0)]);
        l.put(7, 48_000.0, t0);
        assert_eq!(l.all(), [(7, 48_000.0)]);
    }

    #[test]
    fn stereo_spreads_over_more_channels_with_the_rest_silent() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8]; // two frames of int16 stereo
        let mut dst = [0xffu8; 16]; // two frames of int16 x4
        spread(&src, 4, &mut dst, 8, 2);
        assert_eq!(dst, [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
        let mut same = [0u8; 8];
        spread(&src, 4, &mut same, 4, 2);
        assert_eq!(same, src);
    }
}
