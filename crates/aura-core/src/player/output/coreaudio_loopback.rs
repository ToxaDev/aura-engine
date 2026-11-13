//! Is what the CoreAudio backend is handed what the device gets, bit for
//! bit? A loopback device (BlackHole) plays it back on its input: the test
//! opens the backend on it (`CoreAudioOut::open`, the stream, the ring, the
//! IOProc), writes 24-bit noise through the render loop's device interface,
//! records the device's input in this same process (the stream holds the
//! device in hog mode, so no other process could) and compares the two
//! sample by sample, at every rate asked for, one stream after another (the
//! rate switch). BIT-PERFECT needs an integer device format: on a
//! float-only device its open must fail with the reason, and that is
//! checked too.
//!
//!     AURA_LOOPBACK_DEVICE=BlackHole        (part of the device's name)
//!     AURA_LOOPBACK_RATES=44100,192000      (Hz, played in this order)
//!     AURA_LOOPBACK_OUT=<folder>            (results.json, summary.md, captures)
//!     cargo test --profile fast --bins coreaudio_loopback -- --ignored --nocapture

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::mem::{size_of, MaybeUninit};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioDevicePropertyStreams, kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioStreamPropertyVirtualFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};

use super::backend_coreaudio::CoreAudioOut;
use super::coreaudio::hal::CoreAudio;
use super::coreaudio::plan::{self, Id, Status};
use super::period::{pack_frames, Device};
use super::*;

const FULL: f64 = 8_388_608.0; // 2^23

// ── The signal ───────────────────────────────────────────────────────────────

/// `secs` of stereo noise on the 24-bit grid at half scale, with both
/// full-scale ends now and then; deterministic.
fn signal(rate: u32, secs: f64) -> (Vec<f64>, Vec<f64>) {
    let n = (rate as f64 * secs) as usize;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ rate as u64;
    let mut next = || {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((x >> 41) as i64 - (1 << 22)) as f64 / FULL
    };
    let (mut l, mut r) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for i in 0..n {
        let (a, b) = (next(), next());
        match i % 10_007 {
            5_000 => {
                l.push(-1.0);
                r.push((FULL - 1.0) / FULL);
            }
            _ => {
                l.push(a);
                r.push(b);
            }
        }
    }
    (l, r)
}

// ── The recorder: an input IOProc on the same device ─────────────────────────

struct Capture {
    ptr: *mut f32,
    cap: usize,
    pos: AtomicUsize,
    channels: AtomicU32,
    calls: AtomicU32,
}

unsafe extern "C-unwind" fn in_proc(
    _dev: Id,
    _now: NonNull<AudioTimeStamp>,
    in_data: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    _out_data: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    data: *mut c_void,
) -> Status {
    // SAFETY: `data` is the Capture registered with this IOProc, alive until
    // the IOProc is destroyed (`Recorder::stop`); only this thread writes
    // past `pos`.
    unsafe {
        let c = &*(data as *const Capture);
        c.calls.fetch_add(1, Ordering::Relaxed);
        let list = in_data.as_ptr();
        if (*list).mNumberBuffers == 0 {
            return 0;
        }
        let b = &(*list).mBuffers[0];
        let ch = b.mNumberChannels.max(1) as usize;
        c.channels.store(ch as u32, Ordering::Relaxed);
        if b.mData.is_null() || ch < 2 {
            return 0;
        }
        let src = std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
        let frames = src.len() / ch;
        let at = c.pos.load(Ordering::Relaxed);
        let take = frames.min((c.cap - at) / 2);
        let dst = std::slice::from_raw_parts_mut(c.ptr.add(at), take * 2);
        for f in 0..take {
            dst[f * 2] = src[f * ch];
            dst[f * 2 + 1] = src[f * ch + 1];
        }
        c.pos.store(at + take * 2, Ordering::Release);
    }
    0
}

struct Recorder {
    dev: Id,
    id: AudioDeviceIOProcID,
    buf: Vec<f32>,
    cap: Box<Capture>,
}

impl Recorder {
    fn start(dev: Id, frames: usize) -> Result<Recorder, String> {
        let mut buf = vec![0f32; frames * 2];
        let cap = Box::new(Capture {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            pos: AtomicUsize::new(0),
            channels: AtomicU32::new(0),
            calls: AtomicU32::new(0),
        });
        let mut id: AudioDeviceIOProcID = None;
        // SAFETY: `cap` and `buf` outlive the IOProc (destroyed in `stop`).
        unsafe {
            let st = AudioDeviceCreateIOProcID(dev, Some(in_proc), &*cap as *const Capture as *mut c_void, NonNull::from(&mut id));
            if st != 0 {
                return Err(format!("input IOProc: {}", plan::status_text(st)));
            }
            let st = AudioDeviceStart(dev, id);
            if st != 0 {
                AudioDeviceDestroyIOProcID(dev, id);
                return Err(format!("input start: {}", plan::status_text(st)));
            }
        }
        Ok(Recorder { dev, id, buf, cap })
    }

    /// The frames recorded (interleaved L R), the input's channel count and
    /// how many times the IOProc ran.
    fn stop(mut self) -> (Vec<f32>, u32, u32) {
        // SAFETY: AudioDeviceStop returns once the IOProc no longer runs.
        unsafe {
            AudioDeviceStop(self.dev, self.id);
            AudioDeviceDestroyIOProcID(self.dev, self.id);
        }
        let n = self.cap.pos.load(Ordering::Acquire);
        self.buf.truncate(n);
        (std::mem::take(&mut self.buf), self.cap.channels.load(Ordering::Relaxed), self.cap.calls.load(Ordering::Relaxed))
    }
}

/// The device's input stream format, as text.
fn input_format(dev: Id) -> String {
    let a = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = 0u32;
    // SAFETY: plain out pointers; the vector has room for what the HAL says.
    let streams: Vec<Id> = unsafe {
        if AudioObjectGetPropertyDataSize(dev, NonNull::from(&a), 0, ptr::null(), NonNull::from(&mut size)) != 0 {
            return "no input streams".into();
        }
        let n = size as usize / size_of::<Id>();
        let mut v: Vec<Id> = Vec::with_capacity(n.max(1));
        if AudioObjectGetPropertyData(dev, NonNull::from(&a), 0, ptr::null(), NonNull::from(&mut size), NonNull::new_unchecked(v.as_mut_ptr().cast())) != 0 {
            return "no input streams".into();
        }
        v.set_len(size as usize / size_of::<Id>());
        v
    };
    let Some(&s) = streams.first() else { return "no input streams".into() };
    let a = AudioObjectPropertyAddress {
        mSelector: kAudioStreamPropertyVirtualFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut f = MaybeUninit::<AudioStreamBasicDescription>::zeroed();
    let mut size = size_of::<AudioStreamBasicDescription>() as u32;
    // SAFETY: `f` has room for the description.
    let f = unsafe {
        if AudioObjectGetPropertyData(s, NonNull::from(&a), 0, ptr::null(), NonNull::from(&mut size), NonNull::new_unchecked(f.as_mut_ptr().cast())) != 0 {
            return "input format unknown".into();
        }
        f.assume_init()
    };
    format!("{} Hz, {} bit, {} ch, flags 0x{:x}", f.mSampleRate, f.mBitsPerChannel, f.mChannelsPerFrame, f.mFormatFlags)
}

// ── Playing through the backend ──────────────────────────────────────────────

struct Played {
    info: StreamInfo,
    captured: Vec<f32>,
    in_channels: u32,
    in_calls: u32,
    in_format: String,
}

/// Open the backend on `uid` at `rate`, record the input, and write
/// silence, the signal, silence through the render loop's device.
fn play(uid: &str, rate: u32, l: &[f64], r: &[f64]) -> Result<Played, String> {
    let cfg = OutputConfig { device_id: Some(uid.into()), rate, mode: OutputMode::Dsp, allow_shared: false };
    let Opened { mut stream, fmt, blockalign, info } = <CoreAudioOut as Backend>::open(&cfg)?;
    if !matches!(fmt, SampleFmt::Float32 | SampleFmt::Int32) {
        return Err(format!("the device took {}: not a format 24-bit samples go through exactly in this test", info.format));
    }
    let dev = plan::find(&CoreAudio, Some(uid))?;
    let in_format = input_format(dev);
    let lead = rate as usize / 20; // 50 ms of silence before the signal
    let tail = rate as usize * 3 / 10; // and 300 ms after it
    let total = lead + l.len() + tail;
    let silence = vec![0u8; info.buffer_frames as usize * blockalign];
    stream.prefill(info.buffer_frames as usize, &silence)?;
    let rec = Recorder::start(dev, total + rate as usize * 2)?;
    let shared = OutputShared::new();
    let res = (|| {
        let mut d = stream.start(&shared)?;
        let mut bytes = Vec::new();
        let mut at = 0usize;
        let deadline = Instant::now() + Duration::from_secs_f64(total as f64 / rate as f64 + 5.0);
        while at < total {
            if Instant::now() > deadline {
                return Err(format!("the device stopped asking at frame {at} of {total}"));
            }
            if !d.wait() {
                continue;
            }
            let n = d.space()?.min(total - at);
            if n == 0 {
                continue;
            }
            bytes.resize(n * blockalign, 0);
            // The frames [at, at+n) of silence | signal | silence.
            let mut cl = Vec::with_capacity(n);
            let mut cr = Vec::with_capacity(n);
            for i in at..at + n {
                let k = i.wrapping_sub(lead);
                if i >= lead && k < l.len() {
                    cl.push(l[k]);
                    cr.push(r[k]);
                } else {
                    cl.push(0.0);
                    cr.push(0.0);
                }
            }
            pack_frames(&cl, &cr, n, fmt, &mut bytes);
            d.write(n, &bytes)?;
            at += n;
        }
        // What is in the ring and the device plays out.
        std::thread::sleep(Duration::from_millis(250));
        Ok(())
    })();
    stream.stop();
    std::thread::sleep(Duration::from_millis(100));
    let (captured, in_channels, in_calls) = rec.stop();
    res.map(|()| Played { info, captured, in_channels, in_calls, in_format })
}

// ── The comparison ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Compared {
    offset: Option<i64>,
    matched: usize,
    mismatched: usize,
    missing: usize,
    first_mismatch: Option<(usize, f64, f32)>,
    captured_frames: usize,
    nonzero_frames: usize,
}

/// Where the signal starts in the capture (a run of 32 frames found as
/// they are), then every signal frame against the frame recorded for it:
/// the 24-bit value as f32 must be the recorded f32 exactly.
fn compare(l: &[f64], r: &[f64], cap: &[f32]) -> Compared {
    let frames = cap.len() / 2;
    let mut c = Compared { captured_frames: frames, ..Default::default() };
    c.nonzero_frames = (0..frames).filter(|&j| cap[2 * j] != 0.0 || cap[2 * j + 1] != 0.0).count();
    let key = |a: f32, b: f32| ((a.to_bits() as u64) << 32) | b.to_bits() as u64;
    let mut at: HashMap<u64, usize> = HashMap::with_capacity(l.len());
    for i in 0..l.len() {
        at.entry(key(l[i] as f32, r[i] as f32)).or_insert(i);
    }
    let run = 32;
    let same = |i: usize, j: usize| (l[i] as f32).to_bits() == cap[2 * j].to_bits() && (r[i] as f32).to_bits() == cap[2 * j + 1].to_bits();
    for j in 0..frames.saturating_sub(run) {
        if cap[2 * j] == 0.0 && cap[2 * j + 1] == 0.0 {
            continue;
        }
        let Some(&i) = at.get(&key(cap[2 * j], cap[2 * j + 1])) else { continue };
        if i + run <= l.len() && (0..run).all(|k| same(i + k, j + k)) {
            c.offset = Some(j as i64 - i as i64);
            break;
        }
    }
    let Some(off) = c.offset else { return c };
    for i in 0..l.len() {
        let j = i as i64 + off;
        if j < 0 || j as usize >= frames {
            c.missing += 1;
        } else if same(i, j as usize) {
            c.matched += 1;
        } else {
            c.mismatched += 1;
            if c.first_mismatch.is_none() {
                let ju = j as usize;
                let (want, got) = if (l[i] as f32).to_bits() != cap[2 * ju].to_bits() { (l[i], cap[2 * ju]) } else { (r[i], cap[2 * ju + 1]) };
                c.first_mismatch = Some((i, want * FULL, got * FULL as f32));
            }
        }
    }
    c
}

// ── Files ────────────────────────────────────────────────────────────────────

fn write_wav_f32(path: &Path, rate: u32, inter: &[f32]) {
    let data = (inter.len() * 4) as u32;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 8).to_le_bytes());
    b.extend_from_slice(&8u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in inter {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b).ok();
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[test]
#[ignore]
fn coreaudio_loopback_is_bit_exact() {
    let want = std::env::var("AURA_LOOPBACK_DEVICE").unwrap_or_else(|_| "BlackHole".into());
    let rates: Vec<u32> = std::env::var("AURA_LOOPBACK_RATES")
        .unwrap_or_else(|_| "44100,192000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let out = PathBuf::from(std::env::var("AURA_LOOPBACK_OUT").unwrap_or_else(|_| "loopback-out".into()));
    std::fs::create_dir_all(&out).ok();

    let devices = list_devices().expect("device list");
    for d in &devices {
        println!("device: {}  [{}]{}", d.name, d.id, if d.is_default { "  (default)" } else { "" });
    }
    let dev = devices.iter().find(|d| d.name.contains(&want)).unwrap_or_else(|| panic!("no output device named like {want:?}"));
    println!("loopback device: {} [{}]", dev.name, dev.id);

    let mut md = String::new();
    let mut js = Vec::new();
    let mut all_ok = true;
    writeln!(md, "## CoreAudio loopback ({})\n", dev.name).ok();
    writeln!(md, "| rate | device format | exclusive | signal frames | matched | mismatched | missing | first mismatch | result |").ok();
    writeln!(md, "|---|---|---|---|---|---|---|---|---|").ok();
    for &rate in &rates {
        let (l, r) = signal(rate, 3.0);
        let row = match play(&dev.id, rate, &l, &r) {
            Err(e) => {
                all_ok = false;
                println!("{rate} Hz: FAIL — {e}");
                writeln!(md, "| {rate} | — | — | {} | — | — | — | {e} | **FAIL** |", l.len()).ok();
                format!("{{\"rate\":{rate},\"ok\":false,\"error\":{}}}", json_str(&e))
            }
            Ok(p) => {
                let c = compare(&l, &r, &p.captured);
                let ok = c.offset.is_some() && c.mismatched == 0 && c.missing == 0 && c.matched == l.len();
                all_ok &= ok;
                let first = c.first_mismatch.map_or("—".to_string(), |(i, w, g)| format!("frame {i}: sent {w}, got {g}"));
                println!(
                    "{rate} Hz: {} — {} ({}), excl {}, in {} [{} ch, {} calls], captured {} (nonzero {}), offset {:?}, matched {}/{}, mismatched {}, missing {}, first {first}",
                    if ok { "PASS" } else { "FAIL" },
                    p.info.format, p.info.device_name, p.info.exclusive, p.in_format, p.in_channels, p.in_calls,
                    c.captured_frames, c.nonzero_frames, c.offset, c.matched, l.len(), c.mismatched, c.missing
                );
                writeln!(
                    md,
                    "| {rate} | {} | {} | {} | {} | {} | {} | {first} | **{}** |",
                    p.info.format, p.info.exclusive, l.len(), c.matched, c.mismatched, c.missing, if ok { "PASS" } else { "FAIL" }
                )
                .ok();
                write_wav_f32(&out.join(format!("capture-{rate}.wav")), rate, &p.captured);
                format!(
                    "{{\"rate\":{rate},\"ok\":{ok},\"format\":{},\"exclusive\":{},\"input\":{},\"in_channels\":{},\"in_calls\":{},\"signal_frames\":{},\"captured_frames\":{},\"nonzero_frames\":{},\"offset\":{},\"matched\":{},\"mismatched\":{},\"missing\":{},\"first_mismatch\":{}}}",
                    json_str(&p.info.format), p.info.exclusive, json_str(&p.in_format), p.in_channels, p.in_calls, l.len(),
                    c.captured_frames, c.nonzero_frames, c.offset.map_or("null".into(), |o| o.to_string()),
                    c.matched, c.mismatched, c.missing, json_str(&first)
                )
            }
        };
        js.push(row);
    }

    // BIT-PERFECT on a device with no integer format: refused, with the
    // reason (the controller opens it this way: no shared fallback).
    let rate = rates.first().copied().unwrap_or(44_100);
    let cfg = OutputConfig { device_id: Some(dev.id.clone()), rate, mode: OutputMode::Direct { source_bits: 24 }, allow_shared: false };
    let direct = match <CoreAudioOut as Backend>::open(&cfg) {
        Ok(o) => format!("opened: {} (exclusive {})", o.info.format, o.info.exclusive),
        Err(e) => format!("refused: {e}"),
    };
    println!("BIT-PERFECT (Direct, 24-bit) at {rate} Hz: {direct}");
    writeln!(md, "\nBIT-PERFECT (Direct, 24-bit source) at {rate} Hz: {direct}").ok();
    writeln!(md, "\n**{}**", if all_ok { "PASS: every sample came back exactly" } else { "FAIL" }).ok();

    std::fs::write(out.join("summary.md"), &md).ok();
    std::fs::write(
        out.join("results.json"),
        format!("{{\"device\":{},\"ok\":{all_ok},\"direct\":{},\"rates\":[{}]}}\n", json_str(&dev.name), json_str(&direct), js.join(",")),
    )
    .ok();
    assert!(all_ok, "loopback not bit-exact:\n{md}");
}

#[test]
fn the_comparison_finds_the_signal_and_every_wrong_sample() {
    let (l, r) = signal(44_100, 0.2);
    let mut cap = vec![0f32; 2 * 1000];
    for i in 0..l.len() {
        cap.push(l[i] as f32);
        cap.push(r[i] as f32);
    }
    cap.extend(std::iter::repeat(0f32).take(400));
    let c = compare(&l, &r, &cap);
    assert_eq!((c.offset, c.matched, c.mismatched, c.missing), (Some(1000), l.len(), 0, 0));
    cap[2 * (1000 + 777) + 1] += 1.0 / FULL as f32;
    let c = compare(&l, &r, &cap);
    assert_eq!((c.mismatched, c.first_mismatch.map(|m| m.0)), (1, Some(777)));
    let c = compare(&l, &r, &cap[..2 * (1000 + 5000)]);
    assert_eq!(c.missing, l.len() - 5000);
}
