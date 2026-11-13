//! The HAL's property calls behind `plan::Hal`, the IOProc and the property
//! listeners (macOS).

use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use objc2_core_audio::{
    kAudioDevicePropertyAvailableNominalSampleRates, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange, kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyHogMode, kAudioDevicePropertyLatency, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertySafetyOffset, kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyAvailablePhysicalFormats, kAudioStreamPropertyLatency, kAudioStreamPropertyPhysicalFormat,
    kAudioStreamPropertyVirtualFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
    AudioObjectSetPropertyData, AudioStreamRangedDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioValueRange};
use objc2_core_foundation::{CFRetained, CFString};

use super::plan::{Asbd, Hal, Id, Ranged, Status};
use super::ring::Ring;

const SYSTEM: Id = kAudioObjectSystemObject as Id;

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain }
}

fn global(selector: u32) -> AudioObjectPropertyAddress {
    addr(selector, kAudioObjectPropertyScopeGlobal)
}

fn output(selector: u32) -> AudioObjectPropertyAddress {
    addr(selector, kAudioObjectPropertyScopeOutput)
}

fn check(st: Status) -> Result<(), Status> {
    if st == 0 {
        Ok(())
    } else {
        Err(st)
    }
}

/// A fixed-size property. `T` must be the property's own type.
fn get<T: Copy>(obj: Id, a: AudioObjectPropertyAddress) -> Result<T, Status> {
    let mut v = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    // SAFETY: `v` has room for `size` bytes; the HAL writes at most that.
    unsafe {
        check(AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(v.as_mut_ptr().cast()),
        ))?;
        Ok(v.assume_init())
    }
}

/// An array property of `T`s.
fn get_vec<T: Copy>(obj: Id, a: AudioObjectPropertyAddress) -> Result<Vec<T>, Status> {
    let mut size = 0u32;
    // SAFETY: plain out pointers.
    unsafe { check(AudioObjectGetPropertyDataSize(obj, NonNull::from(&a), 0, ptr::null(), NonNull::from(&mut size)))? };
    let n = size as usize / size_of::<T>();
    let mut v: Vec<T> = Vec::with_capacity(n.max(1));
    let mut size = (n * size_of::<T>()) as u32;
    // SAFETY: the vector has room for `n` elements; the HAL writes `size`
    // bytes at most and says how many it wrote.
    unsafe {
        check(AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(v.as_mut_ptr().cast()),
        ))?;
        v.set_len(size as usize / size_of::<T>());
    }
    Ok(v)
}

fn set<T>(obj: Id, a: AudioObjectPropertyAddress, v: &T) -> Result<(), Status> {
    // SAFETY: `v` is a valid `T` of the size passed.
    unsafe {
        check(AudioObjectSetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            ptr::null(),
            size_of::<T>() as u32,
            NonNull::from(v).cast(),
        ))
    }
}

/// A CFString property (the HAL hands over a retained one).
fn get_string(obj: Id, a: AudioObjectPropertyAddress) -> Result<String, Status> {
    let p: *const CFString = get(obj, a)?;
    let p = NonNull::new(p as *mut CFString).ok_or(0x7768_6174)?;
    // SAFETY: a +1 CFString from a "copy" property; dropping it releases it.
    let s = unsafe { CFRetained::from_raw(p) };
    Ok(s.to_string())
}

fn to_asbd(d: &AudioStreamBasicDescription) -> Asbd {
    Asbd {
        rate: d.mSampleRate,
        format_id: d.mFormatID,
        flags: d.mFormatFlags,
        bytes_per_packet: d.mBytesPerPacket,
        frames_per_packet: d.mFramesPerPacket,
        bytes_per_frame: d.mBytesPerFrame,
        channels: d.mChannelsPerFrame,
        bits: d.mBitsPerChannel,
    }
}

fn from_asbd(a: &Asbd) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: a.rate,
        mFormatID: a.format_id,
        mFormatFlags: a.flags,
        mBytesPerPacket: a.bytes_per_packet,
        mFramesPerPacket: a.frames_per_packet,
        mBytesPerFrame: a.bytes_per_frame,
        mChannelsPerFrame: a.channels,
        mBitsPerChannel: a.bits,
        mReserved: 0,
    }
}

/// The real HAL.
pub struct CoreAudio;

impl Hal for CoreAudio {
    fn default_output(&self) -> Result<Id, Status> {
        get(SYSTEM, global(kAudioHardwarePropertyDefaultOutputDevice))
    }

    fn devices(&self) -> Result<Vec<Id>, Status> {
        get_vec(SYSTEM, global(kAudioHardwarePropertyDevices))
    }

    fn uid(&self, dev: Id) -> Result<String, Status> {
        get_string(dev, global(kAudioDevicePropertyDeviceUID))
    }

    fn name(&self, dev: Id) -> Result<String, Status> {
        get_string(dev, global(kAudioObjectPropertyName))
    }

    fn output_channels(&self, dev: Id) -> Result<u32, Status> {
        // A variable-length AudioBufferList, read into u64s for its alignment.
        let a = output(kAudioDevicePropertyStreamConfiguration);
        let mut size = 0u32;
        // SAFETY: plain out pointers.
        unsafe { check(AudioObjectGetPropertyDataSize(dev, NonNull::from(&a), 0, ptr::null(), NonNull::from(&mut size)))? };
        if (size as usize) < size_of::<AudioBufferList>() {
            return Ok(0);
        }
        let mut words = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `words` holds `size` bytes, 8-aligned.
        unsafe {
            check(AudioObjectGetPropertyData(
                dev,
                NonNull::from(&a),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(words.as_mut_ptr().cast()),
            ))?;
            let list = &*(words.as_ptr() as *const AudioBufferList);
            let bufs = std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize);
            Ok(bufs.iter().map(|b| b.mNumberChannels).sum())
        }
    }

    fn hog_owner(&self, dev: Id) -> Result<i32, Status> {
        get::<libc::pid_t>(dev, global(kAudioDevicePropertyHogMode))
    }

    fn toggle_hog(&mut self, dev: Id) -> Result<i32, Status> {
        let a = global(kAudioDevicePropertyHogMode);
        set(dev, a, &self.pid())?;
        get::<libc::pid_t>(dev, a)
    }

    fn process_name(&self, pid: i32) -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: a buffer of the size passed.
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }

    fn pid(&self) -> i32 {
        std::process::id() as i32
    }

    fn nominal_rate(&self, dev: Id) -> Result<f64, Status> {
        get(dev, global(kAudioDevicePropertyNominalSampleRate))
    }

    fn nominal_rates(&self, dev: Id) -> Result<Vec<(f64, f64)>, Status> {
        let v: Vec<AudioValueRange> = get_vec(dev, global(kAudioDevicePropertyAvailableNominalSampleRates))?;
        Ok(v.iter().map(|r| (r.mMinimum, r.mMaximum)).collect())
    }

    fn set_nominal_rate(&mut self, dev: Id, rate: f64) -> Result<(), Status> {
        set(dev, global(kAudioDevicePropertyNominalSampleRate), &rate)?;
        // The change is asynchronous: the device reports it once it runs at
        // it (a DAC relocking takes up to a second or two).
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < until {
            if self.nominal_rate(dev).is_ok_and(|r| (r - rate).abs() < 0.5) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }

    fn output_stream(&self, dev: Id) -> Result<Id, Status> {
        let v: Vec<Id> = get_vec(dev, output(kAudioDevicePropertyStreams))?;
        v.first().copied().ok_or(0x2173_7472) // '!str'
    }

    fn physical_formats(&self, stream: Id) -> Result<Vec<Ranged>, Status> {
        let v: Vec<AudioStreamRangedDescription> = get_vec(stream, global(kAudioStreamPropertyAvailablePhysicalFormats))?;
        Ok(v.iter()
            .map(|r| Ranged {
                fmt: to_asbd(&r.mFormat),
                min_rate: r.mSampleRateRange.mMinimum,
                max_rate: r.mSampleRateRange.mMaximum,
            })
            .collect())
    }

    fn physical_format(&self, stream: Id) -> Result<Asbd, Status> {
        get::<AudioStreamBasicDescription>(stream, global(kAudioStreamPropertyPhysicalFormat)).map(|d| to_asbd(&d))
    }

    fn set_physical_format(&mut self, stream: Id, f: &Asbd) -> Result<(), Status> {
        set(stream, global(kAudioStreamPropertyPhysicalFormat), &from_asbd(f))?;
        // Asynchronous too; settled when it reads back.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < until {
            if self.physical_format(stream).is_ok_and(|g| g == *f) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }

    fn virtual_format(&self, stream: Id) -> Result<Asbd, Status> {
        get::<AudioStreamBasicDescription>(stream, global(kAudioStreamPropertyVirtualFormat)).map(|d| to_asbd(&d))
    }

    fn set_virtual_format(&mut self, stream: Id, f: &Asbd) -> Result<(), Status> {
        set(stream, global(kAudioStreamPropertyVirtualFormat), &from_asbd(f))
    }

    fn buffer_range(&self, dev: Id) -> Result<(f64, f64), Status> {
        get::<AudioValueRange>(dev, global(kAudioDevicePropertyBufferFrameSizeRange)).map(|r| (r.mMinimum, r.mMaximum))
    }

    fn buffer_frames(&self, dev: Id) -> Result<u32, Status> {
        get(dev, global(kAudioDevicePropertyBufferFrameSize))
    }

    fn set_buffer_frames(&mut self, dev: Id, n: u32) -> Result<(), Status> {
        set(dev, global(kAudioDevicePropertyBufferFrameSize), &n)
    }

    fn latency_frames(&self, dev: Id, stream: Id) -> u32 {
        let d: u32 = get(dev, output(kAudioDevicePropertyLatency)).unwrap_or(0);
        let s: u32 = get(dev, output(kAudioDevicePropertySafetyOffset)).unwrap_or(0);
        let st: u32 = get(stream, global(kAudioStreamPropertyLatency)).unwrap_or(0);
        d + s + st
    }
}

// ── Listeners ────────────────────────────────────────────────────────────────

/// Why the device stopped being ours to play on (set by the listeners).
pub const GONE: u8 = 1;
pub const DEFAULT_CHANGED: u8 = 2;

/// What the listeners report to the render loop.
pub struct Watch {
    pub lost: AtomicBool,
    pub why: AtomicU8,
    dev: Id,
    follows_default: bool,
}

unsafe extern "C-unwind" fn on_change(obj: Id, n: u32, addrs: NonNull<AudioObjectPropertyAddress>, data: *mut c_void) -> Status {
    // SAFETY: `data` is the Watch registered with this listener, alive until
    // the listener is removed (`Listening::drop`); `addrs` holds `n` items.
    let w = unsafe { &*(data as *const Watch) };
    let addrs = unsafe { std::slice::from_raw_parts(addrs.as_ptr(), n as usize) };
    for a in addrs {
        if a.mSelector == kAudioDevicePropertyDeviceIsAlive && obj == w.dev {
            if !get::<u32>(w.dev, global(kAudioDevicePropertyDeviceIsAlive)).is_ok_and(|v| v != 0) {
                w.why.store(GONE, Ordering::Relaxed);
                w.lost.store(true, Ordering::Release);
            }
        } else if a.mSelector == kAudioHardwarePropertyDefaultOutputDevice && w.follows_default {
            let now: Result<Id, _> = get(SYSTEM, global(kAudioHardwarePropertyDefaultOutputDevice));
            if now.is_ok_and(|d| d != w.dev) {
                w.why.store(DEFAULT_CHANGED, Ordering::Relaxed);
                w.lost.store(true, Ordering::Release);
            }
        }
    }
    0
}

/// The listeners on the device (removal) and, for a stream on the default
/// output, on the system (another default); removed on drop.
pub struct Listening {
    watch: Box<Watch>,
}

impl Listening {
    pub fn new(dev: Id, follows_default: bool) -> Listening {
        let watch = Box::new(Watch { lost: AtomicBool::new(false), why: AtomicU8::new(0), dev, follows_default });
        let data = &*watch as *const Watch as *mut c_void;
        // SAFETY: `data` outlives the registration (removed in drop).
        unsafe {
            AudioObjectAddPropertyListener(dev, NonNull::from(&global(kAudioDevicePropertyDeviceIsAlive)), Some(on_change), data);
            if follows_default {
                AudioObjectAddPropertyListener(
                    SYSTEM,
                    NonNull::from(&global(kAudioHardwarePropertyDefaultOutputDevice)),
                    Some(on_change),
                    data,
                );
            }
        }
        Listening { watch }
    }

    pub fn watch(&self) -> &Watch {
        &self.watch
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        let data = &*self.watch as *const Watch as *mut c_void;
        // SAFETY: the same registrations `new` made.
        unsafe {
            AudioObjectRemovePropertyListener(
                self.watch.dev,
                NonNull::from(&global(kAudioDevicePropertyDeviceIsAlive)),
                Some(on_change),
                data,
            );
            if self.watch.follows_default {
                AudioObjectRemovePropertyListener(
                    SYSTEM,
                    NonNull::from(&global(kAudioHardwarePropertyDefaultOutputDevice)),
                    Some(on_change),
                    data,
                );
            }
        }
    }
}

// ── The IOProc ───────────────────────────────────────────────────────────────

/// What the IOProc reads: the ring, and the device frame it spreads into.
pub struct IoCtx {
    pub ring: std::sync::Arc<Ring>,
    pub dst_frame: usize,
}

unsafe extern "C-unwind" fn io_proc(
    _dev: Id,
    _now: NonNull<AudioTimeStamp>,
    _in_data: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    out_data: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    data: *mut c_void,
) -> Status {
    // SAFETY: `data` is the IoCtx registered with this IOProc, alive until it
    // is destroyed (`Running::drop`); the buffer list is the HAL's, one buffer
    // per output stream, the first one ours.
    unsafe {
        let ctx = &*(data as *const IoCtx);
        let list = out_data.as_ptr();
        let n = (*list).mNumberBuffers as usize;
        let bufs = std::slice::from_raw_parts_mut((*list).mBuffers.as_mut_ptr(), n);
        for (i, b) in bufs.iter_mut().enumerate() {
            if b.mData.is_null() {
                continue;
            }
            let bytes = std::slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize);
            if i == 0 && ctx.dst_frame > 0 {
                let frames = bytes.len() / ctx.dst_frame;
                ctx.ring.pop_into(bytes, ctx.dst_frame, frames);
                bytes[frames * ctx.dst_frame..].fill(0);
            } else {
                bytes.fill(0);
            }
        }
    }
    0
}

/// A started IOProc; stopped and destroyed on drop.
pub struct Running {
    dev: Id,
    id: AudioDeviceIOProcID,
    _ctx: Box<IoCtx>,
}

impl Running {
    pub fn start(dev: Id, ctx: IoCtx) -> Result<Running, Status> {
        let ctx = Box::new(ctx);
        let mut id: AudioDeviceIOProcID = None;
        // SAFETY: `ctx` outlives the IOProc (destroyed in drop first).
        unsafe {
            check(AudioDeviceCreateIOProcID(
                dev,
                Some(io_proc),
                &*ctx as *const IoCtx as *mut c_void,
                NonNull::from(&mut id),
            ))?;
            if let Err(e) = check(AudioDeviceStart(dev, id)) {
                AudioDeviceDestroyIOProcID(dev, id);
                return Err(e);
            }
        }
        Ok(Running { dev, id, _ctx: ctx })
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: the IOProc `start` made; AudioDeviceStop returns once it is
        // no longer running, so the context is not read after this.
        unsafe {
            AudioDeviceStop(self.dev, self.id);
            AudioDeviceDestroyIOProcID(self.dev, self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan::*;

    #[test]
    fn the_format_constants_are_the_frameworks() {
        use objc2_core_audio_types as t;
        assert_eq!(LPCM, t::kAudioFormatLinearPCM);
        assert_eq!(FLAG_FLOAT, t::kAudioFormatFlagIsFloat);
        assert_eq!(FLAG_BIG_ENDIAN, t::kAudioFormatFlagIsBigEndian);
        assert_eq!(FLAG_SIGNED, t::kAudioFormatFlagIsSignedInteger);
        assert_eq!(FLAG_PACKED, t::kAudioFormatFlagIsPacked);
        assert_eq!(FLAG_ALIGNED_HIGH, t::kAudioFormatFlagIsAlignedHigh);
        assert_eq!(FLAG_NON_INTERLEAVED, t::kAudioFormatFlagIsNonInterleaved);
        assert_eq!(FLAG_NON_MIXABLE, t::kAudioFormatFlagIsNonMixable);
    }

    /// On the CI's Mac (often no output device at all): the list and the
    /// probe answer without failing.
    #[test]
    fn the_device_list_answers() {
        let h = super::CoreAudio;
        let l = list(&h).expect("device list");
        for d in &l {
            assert!(!d.uid.is_empty());
        }
    }
}
