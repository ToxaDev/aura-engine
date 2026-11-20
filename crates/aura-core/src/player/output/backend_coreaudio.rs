//! CoreAudio (macOS) as the output's backend: the device taken in hog mode
//! at the stream's exact rate with an integer physical format
//! (`coreaudio::plan`), the periods handed to the IOProc through a ring
//! (`coreaudio::ring`), the device given back as it was when the stream
//! stops — or when it never started.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::coreaudio::hal::{self, CoreAudio, IoCtx, Listening, Running};
use super::coreaudio::plan::{self, OpenError, Want};
use super::coreaudio::ring::Ring;
use super::coreaudio::{self as ca};
use super::*;

/// CoreAudio as the output's backend (`super::Backend`).
pub(super) struct CoreAudioOut;

impl Backend for CoreAudioOut {
    type Stream = CaStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        ca::list_devices_impl()
    }

    fn probe_formats(device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        ca::probe_formats_impl(device_id, rates)
    }

    fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        ca::diagnose_impl(device_id, rates)
    }

    /// Hog mode, exact rate, physical format, IO buffer (`plan::open`); a
    /// device that refuses hog mode or the format plays shared when the
    /// stream allows it (never DoP: its words must reach the DAC as they are).
    fn open(cfg: &OutputConfig) -> Result<Opened<CaStream>, String> {
        let mut h = CoreAudio;
        let dev = plan::find(&h, cfg.device_id.as_deref())?;
        // A stream right after one on the same device: the rate to set back
        // is the one before them both.
        let older = ca::take_leftover(dev);
        let want = Want::of(&cfg.mode);
        let opened = match plan::open(&mut h, dev, cfg.rate, want, true) {
            Err(OpenError::Format(e)) | Err(OpenError::Hal(e)) if cfg.allow_shared && want != Want::Dop => {
                crate::aelog!("[OUTPUT] CoreAudio exclusive refused ({e}); shared mode");
                plan::open(&mut h, dev, cfg.rate, want, false)
            }
            r => r,
        };
        let o = match opened {
            Ok(o) => o,
            Err(e) => {
                if let Some(r) = older {
                    ca::set_back_later(dev, r);
                }
                return Err(e.text());
            }
        };
        for n in &o.notes {
            crate::aelog!("[OUTPUT] CoreAudio {}: {n}", o.name);
        }

        let fmt = ca::sample_fmt(o.io_kind);
        let blockalign = 2 * o.io_kind.bytes() as usize;
        // What the DAC is behind the timeline: its own latency and IO buffer,
        // and the ring, between one and two buffers full.
        let info = StreamInfo {
            device_name: o.name.clone(),
            exclusive: o.exclusive,
            format: o.format_text(),
            rate: cfg.rate,
            period_frames: o.buffer_frames,
            buffer_frames: o.buffer_frames,
            latency_frames: o.latency_frames + o.buffer_frames * 3 / 2,
        };
        crate::aelog!("[OUTPUT] CoreAudio {}: {} Hz, {}, buffer {} frames", o.name, cfg.rate, info.format, o.buffer_frames);
        let stream = CaStream {
            ring: Arc::new(Ring::new(plan::ring_frames(o.buffer_frames), blockalign)),
            set_back_to: older.or(o.taken.rate),
            follows_default: cfg.device_id.is_none(),
            opened: Some(o),
            listening: None,
            running: None,
        };
        Ok(Opened { stream, fmt, blockalign, info })
    }
}

/// A device taken for a stream, its IOProc not yet started (or stopped).
pub(super) struct CaStream {
    ring: Arc<Ring>,
    /// The nominal rate the device had before (None: left as it was).
    set_back_to: Option<f64>,
    follows_default: bool,
    /// None once given back.
    opened: Option<plan::Opened>,
    listening: Option<Listening>,
    running: Option<Running>,
}

impl CaStream {
    /// The IOProc stopped, the listeners removed, the device given back:
    /// format, buffer and hog at once, the rate after the grace.
    fn give_back(&mut self) {
        self.running = None;
        self.listening = None;
        let Some(o) = self.opened.take() else { return };
        let under = self.ring.underruns.load(Ordering::Relaxed);
        if under > 0 {
            crate::aelog!("[OUTPUT] CoreAudio {}: {under} IO buffer(s) went out short (silence)", o.name);
        }
        plan::release(&mut CoreAudio, &o.taken);
        if let Some(r) = self.set_back_to {
            ca::set_back_later(o.dev, r);
        }
    }
}

impl Stream for CaStream {
    type Dev<'a> = Ca<'a>;

    fn prefill(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let n = self.ring.push(bytes);
        if n < frames {
            return Err(format!("prefill: {n} of {frames} frames taken"));
        }
        Ok(())
    }

    fn start<'a>(&'a mut self, _shared: &'a OutputShared) -> Result<Ca<'a>, String> {
        let o = self.opened.as_ref().ok_or("the device was given back")?;
        let (dev, io, dst_frame) = (o.dev, o.buffer_frames as usize, o.io.bytes_per_frame as usize);
        self.listening = Some(Listening::new(dev, self.follows_default));
        self.ring.set_writer();
        match Running::start(dev, IoCtx { ring: self.ring.clone(), dst_frame }) {
            Ok(r) => self.running = Some(r),
            Err(c) => {
                self.listening = None;
                return Err(format!("AudioDeviceStart: {}", plan::status_text(c)));
            }
        }
        let me: &'a CaStream = self;
        Ok(Ca { ring: &me.ring, io, listening: me.listening.as_ref().expect("set above") })
    }

    fn stop(&mut self) {
        self.give_back();
    }
}

/// A stream that never started (stopped at the gate, or a failure after
/// the open) gives the device back too.
impl Drop for CaStream {
    fn drop(&mut self) {
        self.give_back();
    }
}

/// The ring as the render loop's device: a period is asked for once the
/// IOProc has made room for one; the listeners' news ends the stream.
pub(super) struct Ca<'a> {
    ring: &'a Ring,
    io: usize,
    listening: &'a Listening,
}

impl Device for Ca<'_> {
    fn wait(&mut self) -> bool {
        if self.ring.free_frames() >= self.io {
            return true;
        }
        std::thread::park_timeout(Duration::from_millis(20));
        self.ring.free_frames() >= self.io
    }

    fn space(&mut self) -> Result<usize, String> {
        let w = self.listening.watch();
        if w.lost.load(Ordering::Acquire) {
            return Err(match w.why.load(Ordering::Relaxed) {
                hal::DEFAULT_CHANGED => "the system's default output changed".into(),
                _ => "the output device was disconnected".into(),
            });
        }
        Ok(self.ring.free_frames().min(self.io))
    }

    fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let n = self.ring.push(bytes);
        if n < frames {
            return Err(format!("ring full: {n} of {frames} frames taken"));
        }
        Ok(())
    }
}
