// SPDX-License-Identifier: GPL-3.0-only
//! The ASIO output's part without a driver (`backend_asio`): how a period
//! the render loop packed (`Periods`, interleaved stereo) becomes the
//! driver's two channel buffers, the ring between the audio thread and the
//! driver's callback, and what the callback sends when the ring has run dry.
//!
//! The audio thread writes periods into the ring whenever it has room; the
//! driver's `bufferSwitch` takes one half-buffer out of it each time it
//! asks, never waiting for anything. Native DSD rides the same periods as
//! DoP does on WASAPI (the DSD output and a DSD file as it is pack DoP
//! words, `Periods`): the backend takes the two DSD bytes out of each word
//! and drops the marker, so the modulators, the guard, the silences and the
//! counts are the very same code for both.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::audio::dsd::dop::MARKERS;
use crate::audio::dsd::DSD_SILENCE;

/// ASIO sample types the output takes (asio.h `ASIOSampleType`).
pub(super) const ASIOST_INT16_LSB: i32 = 16;
pub(super) const ASIOST_INT24_LSB: i32 = 17;
pub(super) const ASIOST_INT32_LSB: i32 = 18;
pub(super) const ASIOST_FLOAT32_LSB: i32 = 19;
pub(super) const ASIOST_DSD_INT8_LSB1: i32 = 32;
pub(super) const ASIOST_DSD_INT8_MSB1: i32 = 33;

/// The SDK's name of a sample type, for the log and the errors.
pub(super) fn sample_type_name(t: i32) -> String {
    let n = match t {
        0 => "Int16MSB",
        1 => "Int24MSB",
        2 => "Int32MSB",
        3 => "Float32MSB",
        4 => "Float64MSB",
        8..=11 => "Int32MSB (padded)",
        16 => "Int16LSB",
        17 => "Int24LSB",
        18 => "Int32LSB",
        19 => "Float32LSB",
        20 => "Float64LSB",
        24..=27 => "Int32LSB (padded)",
        32 => "DSDInt8LSB1",
        33 => "DSDInt8MSB1",
        40 => "DSDInt8NER8",
        _ => "unknown",
    };
    format!("{n} ({t})")
}

/// How the periods' bytes map onto the driver's channel buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// PCM (and DoP over a PCM stream): each interleaved sample of `bytes`
    /// goes to its channel as it is — the period is packed in the driver's
    /// own little-endian type. `dop`: the samples are DoP words (24 in 32
    /// when `bytes` is 4), so a dry ring sends DSD silence with markers.
    Pcm { bytes: usize, dop: bool },
    /// Native DSD: a period frame is `bpf` DSD bytes of the left channel,
    /// then of the right (`SampleFmt::Dsd`), eight one-bit samples a byte,
    /// the oldest in the top bit (DSDInt8MSB1) — or, for a driver that
    /// takes DSDInt8LSB1, in the bottom bit.
    Dsd { lsb_first: bool, bpf: usize },
}

impl Layout {
    /// Bytes a frame takes in a channel's buffer.
    pub(super) fn unit(&self) -> usize {
        match *self {
            Layout::Pcm { bytes, .. } => bytes,
            Layout::Dsd { bpf, .. } => bpf,
        }
    }

    /// Bytes an interleaved stereo frame of a period takes.
    pub(super) fn period_frame_bytes(&self) -> usize {
        match *self {
            Layout::Pcm { bytes, .. } => 2 * bytes,
            Layout::Dsd { bpf, .. } => 2 * bpf,
        }
    }

    /// `frames` of a period (`src`, interleaved) into the two channels.
    pub(super) fn split(&self, frames: usize, src: &[u8], l: &mut [u8], r: &mut [u8]) {
        match *self {
            Layout::Pcm { bytes: b, .. } => {
                for i in 0..frames {
                    let s = 2 * b * i;
                    l[b * i..b * (i + 1)].copy_from_slice(&src[s..s + b]);
                    r[b * i..b * (i + 1)].copy_from_slice(&src[s + b..s + 2 * b]);
                }
            }
            Layout::Dsd { lsb_first, bpf: b } => {
                for i in 0..frames {
                    let s = 2 * b * i;
                    l[b * i..b * (i + 1)].copy_from_slice(&src[s..s + b]);
                    r[b * i..b * (i + 1)].copy_from_slice(&src[s + b..s + 2 * b]);
                }
                if lsb_first {
                    for x in l[..b * frames].iter_mut().chain(r[..b * frames].iter_mut()) {
                        *x = x.reverse_bits();
                    }
                }
            }
        }
    }

    /// Silence for a channel's `out` (whole frames) when the ring has run
    /// dry: zeros for PCM, DSD silence for native DSD (zero bytes would be
    /// a full-scale DC step), DoP silence with markers for DoP over PCM,
    /// starting from marker `phase` (an index into `MARKERS`), which comes
    /// back advanced past them.
    pub(super) fn idle(&self, out: &mut [u8], phase: usize) -> usize {
        match *self {
            Layout::Pcm { dop: false, .. } => {
                out.fill(0);
                phase
            }
            Layout::Dsd { .. } => {
                out.fill(DSD_SILENCE);
                phase
            }
            Layout::Pcm { bytes, dop: true } => {
                let mut p = phase;
                for w in out.chunks_exact_mut(bytes) {
                    let n = w.len();
                    w.fill(0);
                    w[n - 1] = MARKERS[p];
                    w[n - 2] = DSD_SILENCE;
                    w[n - 3] = DSD_SILENCE;
                    p ^= 1;
                }
                p
            }
        }
    }

    /// DoP over PCM: the marker phase after a channel's `data` (whole
    /// frames) — the other marker than its last word's. Otherwise `phase`.
    pub(super) fn phase_after(&self, data: &[u8], phase: usize) -> usize {
        match *self {
            Layout::Pcm { bytes, dop: true } if data.len() >= bytes => {
                let m = data[data.len() - 1];
                if m == MARKERS[0] {
                    1
                } else if m == MARKERS[1] {
                    0
                } else {
                    phase
                }
            }
            _ => phase,
        }
    }
}

/// A single-producer, single-consumer ring of the two channels' bytes,
/// in frames: the audio thread pushes, the driver's callback pops. Neither
/// side waits for the other or allocates.
pub(super) struct Ring {
    cap: usize,
    unit: usize,
    l: Box<[UnsafeCell<u8>]>,
    r: Box<[UnsafeCell<u8>]>,
    /// Frames pushed, and popped, since the start.
    written: AtomicU64,
    read: AtomicU64,
}

// The producer writes only the frames between `written` and `read + cap`,
// the consumer reads only those between `read` and `written`; each side
// publishes its count with Release after its copy and reads the other's
// with Acquire before it.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub(super) fn new(cap_frames: usize, unit: usize) -> Ring {
        let cells = |n| (0..n).map(|_| UnsafeCell::new(0u8)).collect::<Vec<_>>().into_boxed_slice();
        Ring { cap: cap_frames, unit, l: cells(cap_frames * unit), r: cells(cap_frames * unit), written: AtomicU64::new(0), read: AtomicU64::new(0) }
    }

    pub(super) fn cap(&self) -> usize {
        self.cap
    }

    /// Frames waiting to be popped.
    pub(super) fn filled(&self) -> usize {
        (self.written.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)) as usize
    }

    /// Frames there is room for.
    pub(super) fn free(&self) -> usize {
        self.cap - self.filled()
    }

    fn ptr(cells: &[UnsafeCell<u8>]) -> *mut u8 {
        // UnsafeCell<u8> has u8's layout.
        cells.as_ptr() as *mut u8
    }

    /// Producer: `frames` of each channel (at most `free`).
    pub(super) fn push(&self, frames: usize, l: &[u8], r: &[u8]) {
        let w = self.written.load(Ordering::Relaxed);
        let room = self.cap - (w - self.read.load(Ordering::Acquire)) as usize;
        let frames = frames.min(room);
        let start = (w % self.cap as u64) as usize;
        let first = frames.min(self.cap - start);
        let u = self.unit;
        for (src, cells) in [(l, &self.l), (r, &self.r)] {
            let dst = Self::ptr(cells);
            // SAFETY: the frames [start, start + frames) mod cap are the
            // producer's until `written` moves past them.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(start * u), first * u);
                std::ptr::copy_nonoverlapping(src.as_ptr().add(first * u), dst, (frames - first) * u);
            }
        }
        self.written.store(w + frames as u64, Ordering::Release);
    }

    /// Consumer: up to `frames` of each channel into `l` and `r`; returns
    /// how many there were.
    ///
    /// # Safety
    /// `l` and `r` point at `frames * unit` writable bytes each.
    pub(super) unsafe fn pop(&self, frames: usize, l: *mut u8, r: *mut u8) -> usize {
        let rd = self.read.load(Ordering::Relaxed);
        let have = (self.written.load(Ordering::Acquire) - rd) as usize;
        let n = frames.min(have);
        let start = (rd % self.cap as u64) as usize;
        let first = n.min(self.cap - start);
        let u = self.unit;
        for (dst, cells) in [(l, &self.l), (r, &self.r)] {
            let src = Self::ptr(cells);
            std::ptr::copy_nonoverlapping(src.add(start * u), dst, first * u);
            std::ptr::copy_nonoverlapping(src, dst.add(first * u), (n - first) * u);
        }
        self.read.store(rd + n as u64, Ordering::Release);
        n
    }
}

/// How long the driver's buffer (one half) is asked to be, at least: as
/// WASAPI's exclusive period (`period::exclusive_period_hns`). The player
/// renders seconds ahead and needs no small latency; a driver set for one
/// (an RME at 32-128 samples) crackled.
pub(super) const BUFFER_S: f64 = 0.010;
/// The audio thread's period on ASIO: what it hands the ring in one go,
/// whatever the driver's half — never the driver's few samples a time.
pub(super) const CHUNK_S: f64 = 0.010;
/// The ring between the audio thread and the callback, at least.
pub(super) const RING_S: f64 = 0.060;

/// The buffer size (in the driver's samples: one-bit samples for DSD) to
/// ask `createBuffers` for, from what `getBufferSize` said: the driver's
/// preferred size when it lasts `target_s` at `per_s` samples a second,
/// else the smallest size the driver takes that does (`gran` -1: powers of
/// two from `min` to `max`; above 0: `min` in steps of `gran`; 0: one size
/// only), else the largest it takes. Only a multiple of `multiple` (a DSD
/// buffer of whole frames) is taken.
pub(super) fn pick_buffer(min: i32, max: i32, pref: i32, gran: i32, per_s: f64, target_s: f64, multiple: usize) -> usize {
    let multiple = multiple.max(1);
    let pref = pref.max(min).max(1) as usize;
    let want = (target_s * per_s).ceil() as usize;
    if pref >= want || max <= 0 || min <= 0 || max < min {
        return pref;
    }
    let (min, max) = (min as usize, max as usize);
    let sizes: Vec<usize> = match gran {
        -1 => (0..63).map(|k| 1usize << k).skip_while(|&s| s < min).take_while(|&s| s <= max).collect(),
        g if g > 0 => (min..=max).step_by(g as usize).collect(),
        _ => Vec::new(),
    };
    let ok = |s: &&usize| **s > pref && **s % multiple == 0;
    sizes.iter().filter(ok).find(|&&s| s >= want).or_else(|| sizes.iter().filter(ok).last()).copied().unwrap_or(pref)
}

/// Whole halves of `half` frames lasting `s` at `rate`, at least `least`
/// of them.
pub(super) fn whole_halves(half: usize, rate: u32, s: f64, least: usize) -> usize {
    let want = (s * rate as f64).ceil() as usize;
    want.div_ceil(half.max(1)).max(least) * half.max(1)
}

/// The audio thread's period and the ring's size on ASIO, in frames, for
/// a driver half of `half` frames: whole halves, the period `CHUNK_S` at
/// least, the ring `RING_S` and three periods at least.
pub(super) fn chunk_and_ring(half: usize, rate: u32) -> (usize, usize) {
    let chunk = whole_halves(half, rate, CHUNK_S, 1);
    let ring = whole_halves(half, rate, RING_S, 2).max(3 * chunk);
    (chunk, ring)
}

/// Native DSD on a stream of `rate` frames a second at `bit_rate` one-bit
/// samples a second: the DSD bytes a channel a frame, None when a frame is
/// no whole number of bytes (at least two: 16 one-bit samples, a DoP
/// word's worth).
pub(super) fn dsd_bytes_per_frame(bit_rate: u32, rate: u32) -> Option<usize> {
    (rate > 0 && bit_rate % rate == 0).then(|| (bit_rate / rate) as usize).filter(|r| r % 8 == 0 && *r >= 16).map(|r| r / 8)
}

/// Frames in a DSD buffer of `samples` one-bit samples (asio.h counts DSD
/// buffers in samples: N / 8 bytes a channel), `bpf` bytes a frame; None
/// when it is no whole number of frames.
pub(super) fn dsd_half_frames(samples: usize, bpf: usize) -> Option<usize> {
    let bits = 8 * bpf;
    (samples > 0 && bpf > 0 && samples % bits == 0).then_some(samples / bits)
}

/// The words a failed open or start is said with. A driver that finds no
/// way to the DAC (`ASE_NotPresent`, "No pins available for rendering" in
/// Chord's words) is almost always shut out by Windows: the DAC is the
/// system's default playback device, or another program plays through it.
pub(super) fn start_error(driver: &str, what: &str, code: i32, driver_says: &str) -> String {
    let said = if driver_says.trim().is_empty() { String::new() } else { format!(": \"{}\"", driver_says.trim()) };
    if code == ASE_NOT_PRESENT || driver_says.to_ascii_lowercase().contains("no pins") {
        format!(
            "ASIO: {driver} cannot reach the DAC ({what} refused{said}). Windows is holding it - it is the default \
             playback device, or another program plays through it. Make another device the default output in Windows \
             (Settings > System > Sound > Output), close other players, then press Play again."
        )
    } else {
        format!("ASIO: {driver}: {what} refused ({}){said}", error_name(code))
    }
}

pub(super) const ASE_OK: i32 = 0;
pub(super) const ASE_SUCCESS: i32 = 0x3f48_47a0;
pub(super) const ASE_NOT_PRESENT: i32 = -1000;

/// An ASIOError by its SDK name.
pub(super) fn error_name(e: i32) -> String {
    match e {
        ASE_OK => "OK".into(),
        ASE_SUCCESS => "SUCCESS".into(),
        ASE_NOT_PRESENT => "NotPresent".into(),
        -999 => "HWMalfunction".into(),
        -998 => "InvalidParameter".into(),
        -997 => "InvalidMode".into(),
        -996 => "SPNotAdvancing".into(),
        -995 => "NoClock".into(),
        -994 => "NoMemory".into(),
        x => format!("code {x}"),
    }
}

/// The device id an ASIO driver is listed under (`DeviceInfo::id`): its
/// registry key's name behind a prefix no system device id has.
pub const ASIO_ID_PREFIX: &str = "asio:";

/// The driver's registry key name when `id` is an ASIO device's.
pub fn asio_driver_of(id: Option<&str>) -> Option<&str> {
    id.and_then(|s| s.strip_prefix(ASIO_ID_PREFIX)).filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native DSD: each frame's left bytes, then right, to their channels
    /// in order, whatever the bytes a frame; LSB1 drivers get each byte's
    /// bits the other way round.
    #[test]
    fn native_dsd_bytes_go_to_their_channels_in_order() {
        for bpf in [2usize, 4, 8] {
            let frames = 3;
            let src: Vec<u8> = (0..2 * bpf * frames).map(|i| i as u8).collect();
            let lay = Layout::Dsd { lsb_first: false, bpf };
            assert_eq!((lay.unit(), lay.period_frame_bytes()), (bpf, 2 * bpf));
            let (mut l, mut r) = (vec![0u8; bpf * frames], vec![0u8; bpf * frames]);
            lay.split(frames, &src, &mut l, &mut r);
            for f in 0..frames {
                assert_eq!(&l[bpf * f..bpf * (f + 1)], &src[2 * bpf * f..2 * bpf * f + bpf], "bpf {bpf} frame {f} L");
                assert_eq!(&r[bpf * f..bpf * (f + 1)], &src[2 * bpf * f + bpf..2 * bpf * (f + 1)], "bpf {bpf} frame {f} R");
            }
            let (mut l2, mut r2) = (vec![0u8; bpf * frames], vec![0u8; bpf * frames]);
            Layout::Dsd { lsb_first: true, bpf }.split(frames, &src, &mut l2, &mut r2);
            assert!(l2.iter().zip(&l).all(|(a, b)| *a == b.reverse_bits()));
            assert!(r2.iter().zip(&r).all(|(a, b)| *a == b.reverse_bits()));
        }
        // DSD silence stays DSD silence either way (0x69 reversed is 0x96:
        // the same pattern a bit later).
        assert_eq!(DSD_SILENCE.reverse_bits(), 0x96);
    }

    /// PCM: each interleaved sample to its channel, as it is.
    #[test]
    fn pcm_samples_go_to_their_channels_as_they_are() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let lay = Layout::Pcm { bytes: 3, dop: false };
        assert_eq!((lay.unit(), lay.period_frame_bytes()), (3, 6));
        let (mut l, mut r) = ([0u8; 6], [0u8; 6]);
        lay.split(2, &src, &mut l, &mut r);
        assert_eq!(l, [1, 2, 3, 7, 8, 9]);
        assert_eq!(r, [4, 5, 6, 10, 11, 12]);
    }

    /// A dry ring sends silence the DAC stays in its mode with: zeros for
    /// PCM, 0x69 for native DSD, DoP silence whose markers go on from the
    /// last word sent.
    #[test]
    fn a_dry_ring_sends_the_silence_of_the_stream_s_kind() {
        let mut b = [7u8; 8];
        Layout::Pcm { bytes: 4, dop: false }.idle(&mut b, 0);
        assert_eq!(b, [0; 8]);
        Layout::Dsd { lsb_first: false, bpf: 2 }.idle(&mut b, 0);
        assert_eq!(b, [DSD_SILENCE; 8]);
        let dop = Layout::Pcm { bytes: 4, dop: true };
        let last = [0u8, 0x11, 0x22, MARKERS[0]];
        let p = dop.phase_after(&last, 0);
        assert_eq!(p, 1);
        let mut w = [0u8; 12];
        let after = dop.idle(&mut w, p);
        assert_eq!(w, [0, 0x69, 0x69, MARKERS[1], 0, 0x69, 0x69, MARKERS[0], 0, 0x69, 0x69, MARKERS[1]]);
        assert_eq!(after, 0);
        let dop24 = Layout::Pcm { bytes: 3, dop: true };
        let mut w = [9u8; 6];
        dop24.idle(&mut w, 0);
        assert_eq!(w, [0x69, 0x69, MARKERS[0], 0x69, 0x69, MARKERS[1]]);
    }

    /// The ring hands out what went in, in order, across its end; never
    /// more than it holds, never more than it has room for.
    #[test]
    fn the_ring_hands_out_what_went_in_across_its_end() {
        let ring = Ring::new(5, 2);
        let frame = |k: u8| ([k, k + 100], [k + 50, k + 150]);
        let mut next_in = 0u8;
        let mut next_out = 0u8;
        for round in 0..20 {
            let n = (round % 4 + 1).min(ring.free());
            let (mut l, mut r) = (vec![], vec![]);
            for _ in 0..n {
                let (a, b) = frame(next_in);
                l.extend(a);
                r.extend(b);
                next_in += 1;
            }
            ring.push(n, &l, &r);
            let want = round % 3 + 1;
            let (mut ol, mut or) = (vec![0u8; 2 * want], vec![0u8; 2 * want]);
            let got = unsafe { ring.pop(want, ol.as_mut_ptr(), or.as_mut_ptr()) };
            assert!(got <= want);
            for i in 0..got {
                let (a, b) = frame(next_out);
                assert_eq!((&ol[2 * i..2 * i + 2], &or[2 * i..2 * i + 2]), (&a[..], &b[..]));
                next_out += 1;
            }
            assert_eq!(ring.filled() + ring.free(), 5);
        }
        assert!(next_out > 10);
        // Full: a push beyond the room is cut to it.
        let ring = Ring::new(2, 1);
        ring.push(3, &[1, 2, 3], &[4, 5, 6]);
        assert_eq!(ring.filled(), 2);
    }

    #[test]
    fn sizes_and_ids() {
        assert_eq!(dsd_half_frames(32_768, 2), Some(2048));
        assert_eq!(dsd_half_frames(32_768, 8), Some(512));
        assert_eq!(dsd_half_frames(100, 2), None);
        // DSD64 at its DoP carrier; DSD512 and DSD1024 on a ×16 chain.
        assert_eq!(dsd_bytes_per_frame(2_822_400, 176_400), Some(2));
        assert_eq!(dsd_bytes_per_frame(22_579_200, 705_600), Some(4));
        assert_eq!(dsd_bytes_per_frame(45_158_400, 705_600), Some(8));
        assert_eq!(dsd_bytes_per_frame(2_822_400, 352_800), None);
        assert_eq!(dsd_bytes_per_frame(2_822_400, 44_101), None);
        assert_eq!(asio_driver_of(Some("asio:ASIO Chord 44.1Khz 768Khz")), Some("ASIO Chord 44.1Khz 768Khz"));
        assert_eq!(asio_driver_of(Some("{0.0.0.00000000}.{abc}")), None);
        assert_eq!(asio_driver_of(Some("asio:")), None);
        assert_eq!(asio_driver_of(None), None);
    }

    /// A driver set for a small latency (an RME at 64 samples) is asked for
    /// 10 ms where it takes it; a preferred size of 10 ms or more stays;
    /// a driver of one size gets that size.
    #[test]
    fn the_driver_is_asked_for_ten_ms_at_least_where_it_can() {
        // Powers of two 32..8192, preferred 64, at 192k: 2048 (10.7 ms).
        assert_eq!(pick_buffer(32, 8192, 64, -1, 192_000.0, BUFFER_S, 1), 2048);
        // At 44.1k: 512 (11.6 ms).
        assert_eq!(pick_buffer(32, 8192, 64, -1, 44_100.0, BUFFER_S, 1), 512);
        // Max below 10 ms: the max.
        assert_eq!(pick_buffer(32, 1024, 64, -1, 192_000.0, BUFFER_S, 1), 1024);
        // Steps of 48 from 48: the first step past 1920.
        assert_eq!(pick_buffer(48, 4800, 96, 48, 192_000.0, BUFFER_S, 1), 1920);
        // One size only (RME's panel, min = max = pref): that size.
        assert_eq!(pick_buffer(64, 64, 64, 0, 192_000.0, BUFFER_S, 1), 64);
        // A preferred size already long enough stays as it is.
        assert_eq!(pick_buffer(32, 8192, 4096, -1, 192_000.0, BUFFER_S, 1), 4096);
        // DSD64 (2.8224 MHz one-bit), 2 bytes a frame: 32768 one-bit samples.
        assert_eq!(pick_buffer(512, 65536, 2048, -1, 2_822_400.0, BUFFER_S, 16), 32768);
        // Nonsense from the driver: its preferred size.
        assert_eq!(pick_buffer(0, 0, 256, 0, 192_000.0, BUFFER_S, 1), 256);
        assert_eq!(pick_buffer(512, 64, 256, -1, 192_000.0, BUFFER_S, 1), 512);
    }

    /// The audio thread hands the ring 10 ms at least in whole halves,
    /// whatever the driver's half; the ring holds 60 ms and three periods.
    #[test]
    fn the_audio_thread_writes_ten_ms_at_a_time_on_a_small_half() {
        // RME at 64 samples, 176.4k: 28 halves (1792 frames, 10.2 ms).
        let (chunk, ring) = chunk_and_ring(64, 176_400);
        assert_eq!(chunk, 28 * 64);
        assert_eq!(ring, 166 * 64);
        assert!(ring as f64 / 176_400.0 >= RING_S && ring >= 3 * chunk);
        // A 10 ms half: one half a period, the ring of whole halves.
        let (chunk, ring) = chunk_and_ring(2048, 192_000);
        assert_eq!((chunk, ring), (2048, 6 * 2048));
        // A long half (8192 at 44.1k): one half, three of them in the ring.
        assert_eq!(chunk_and_ring(8192, 44_100), (8192, 3 * 8192));
    }

    /// Shut out by Windows, the error says so and what to do.
    #[test]
    fn a_dac_held_by_windows_is_said_in_words_with_the_way_out() {
        let e = start_error("ASIO Chord", "start", ASE_NOT_PRESENT, "No pins available for rendering");
        assert!(e.contains("Windows is holding it") && e.contains("default output") && e.contains("No pins"), "{e}");
        let e = start_error("X", "start", 0, "No pins available for rendering");
        assert!(e.contains("Windows is holding it"), "{e}");
        let e = start_error("X", "createBuffers", -994, "");
        assert!(e.contains("NoMemory") && !e.contains("Windows"), "{e}");
    }
}
