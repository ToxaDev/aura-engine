//! Output device: WASAPI exclusive event-driven render with shared-mode fallback.
//!
//! The audio thread owns every WASAPI handle. It initialises COM, negotiates
//! the format, opens the stream, and reports the result back through a one-shot
//! channel so that `OutputStream::start` can block until the device is ready
//! (or fail immediately when it cannot be opened).
//!
//! Frames on the Timeline are pre-volume, pre-dither f64. The output thread
//! applies volume and dither in DSP mode, or converts bit-perfectly in Direct
//! mode, before packing into device bytes. No heap allocation happens inside
//! the render loop.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
};

use serde::Serialize;

use crate::audio::converter::dsp::dither::DitherState;
use crate::audio::dsd::{DsdRate, ModulatorKind, CATALOG};
use crate::player::timeline::Timeline;

// ── Public types ─────────────────────────────────────────────────────────────

/// An audio endpoint visible to the UI layer (camelCase for the Tauri command).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Enumerate all active render endpoints.
pub fn list_devices() -> Result<Vec<DeviceInfo>, String> {
    #[cfg(windows)]
    return platform::list_devices_impl();
    #[cfg(not(windows))]
    return Ok(Vec::new());
}

/// Playback processing mode.
#[derive(Clone, Debug)]
pub enum OutputMode {
    /// Full DSP chain: volume applied, TPDF dither with noise shaping on the
    /// device's grid (16-bit for a 16-bit device, 24-bit otherwise).
    Dsp,
    /// Bit-perfect pass-through at the native source bit depth.
    /// Volume control and dither are intentionally skipped; the signal is
    /// delivered to the DAC exactly as decoded. `source_bits` 0: the source
    /// has no integer depth (a lossy decoder's output) — it goes out in the
    /// widest integer the device takes.
    Direct { source_bits: u32 },
    /// The DSP chain's output as one-bit DSD, sent as DoP: the stream runs at
    /// the DoP carrier (`DsdRate::dop_carrier`), the volume is applied in PCM
    /// before the modulator, no dither. Needs a 24-bit integer container.
    Dsd { rate: DsdRate, modulator: ModulatorKind },
    /// A DSD file's own data, as it is, sent as DoP at `rate`'s carrier
    /// (`player::dsd_direct`): the timeline holds DoP words, nothing is
    /// applied — no volume, no fade; every transition is DSD silence, and a
    /// guard (`DsdGuard`) sends silence for the rest of the track once the
    /// data stops looking like DSD.
    DsdDirect { rate: DsdRate },
}

impl OutputMode {
    /// Whether the stream is DoP (DSD output or a DSD file as it is).
    pub fn is_dop(&self) -> bool {
        matches!(self, OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. })
    }
}

/// The start of the error a DoP stream fails with when the device offers no
/// 24-bit integer format at the carrier rate (`is_dop_format_error`).
pub const DOP_FORMAT_ERR: &str = "DoP needs a 24-bit integer device format";

/// Whether a stream's open failed because the device has no container DoP
/// can travel in (the controller then plays PCM instead).
pub fn is_dop_format_error(e: &str) -> bool {
    e.starts_with(DOP_FORMAT_ERR)
}

/// Parameters for opening a stream.
#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub device_id: Option<String>,
    pub rate: u32,
    pub mode: OutputMode,
    /// Fall back to shared (Windows-mixer) mode when exclusive is unavailable.
    pub allow_shared: bool,
}

/// State shared between the controller and the output thread.
/// All fields are lock-free or behind a tiny mutex used only on error.
pub struct OutputShared {
    /// f64 linear gain encoded as its bit pattern. Default 1.0 = unity.
    pub volume_bits: AtomicU64,
    /// True → write silence; do NOT advance the timeline read position.
    pub paused: AtomicBool,
    /// Jump protocol (item 5): the output ramps to silence, then waits here.
    /// Set by the command thread immediately before any track switch or seek.
    /// Cleared by the output thread once the jump frame is consumed.
    pub hold: AtomicBool,
    /// Set by the render thread when the new chain's first frames (with
    /// fade-in) have been written to the timeline. u64::MAX = no pending jump.
    /// The output thread reads this while in hold, teleports the read pointer
    /// to this frame, then clears hold. The render thread writes this AFTER
    /// writing the fade-in frames — happens-before is guaranteed by the
    /// Release/Acquire pair on the AtomicU64.
    pub jump_frame: AtomicU64,
    /// Number of frames the device buffer holds at the current rate, used to
    /// synchronise the spectrum analyser's look-ahead. Set once after stream
    /// init; 0 before the stream is opened.
    pub device_latency_frames: AtomicU64,
    /// Peak level of the pre-volume, pre-dither signal sent to the device
    /// over the last ~100 ms, stored as f64 bits. Used for the outLevelDb
    /// status field (item A3: proves audio actually flows to the device).
    /// f64::NEG_INFINITY bits = silent/not yet measured.
    pub out_level_peak_bits: AtomicU64,
    /// Frames of old audio read during the current ramp-down after a hold.
    /// Reset to 0 at the start of each ramp-down; used by J3 to prove that
    /// the old track bleeds no more than one output period.
    pub ramp_frames_read: AtomicU64,
    /// Extended ramp length in frames (0 = one-period default).
    /// Set before hold=true for a BIT-PERFECT/FS fade so the listener hears
    /// a ~120 ms fade-out rather than an instant cut. Reset to 0 by
    /// stop_stream so any subsequent hold uses the one-period default.
    pub hold_ramp_frames: AtomicU64,
    /// False once the thread exits.
    pub alive: AtomicBool,
    /// Set when the device is lost; the controller should restart the stream.
    pub device_lost: AtomicBool,
    /// Last error message; written at thread exit, read by the controller.
    pub last_error: Mutex<Option<String>>,
    /// Peak level of the post-volume, post-dither signal in the last ~100 ms,
    /// stored as f64 bits. f64::NEG_INFINITY bits = silent/not yet measured.
    pub dev_peak_bits: AtomicU64,
    /// Maximum post-volume peak observed since the application started,
    /// stored as f64 bits. Never reset. f64::NEG_INFINITY bits = never measured.
    pub dev_peak_max_bits: AtomicU64,
    /// Count of output periods silenced by the safety guard since the application started.
    pub safety_mutes: AtomicU64,
    /// Maximum peak of what was handed to the device — after the safety
    /// and test mutes — since the application started, as f64 bits. Never
    /// reset. f64::NEG_INFINITY bits = nothing sent yet; 0 = only silence.
    pub sent_peak_max_bits: AtomicU64,
    /// DSD output: the modulators' overloads (both channels) since the
    /// stream opened (`DsdEncoder::overloads`). 0 for PCM.
    pub dsd_overloads: AtomicU64,
    /// A DSD file as it is: the guard found data that is not DSD and the
    /// stream sends silence until the next track starts or it closes.
    /// Cleared then.
    pub dsd_guard_tripped: AtomicBool,
    /// The timeline frame the latest track change of the stream starts at
    /// (the render thread's marks): the guard starts afresh there, so a
    /// track begins clean whatever the one before did. u64::MAX for none.
    pub dsd_guard_from: AtomicU64,
    /// DSD output: frames from a read of the timeline to the music heard,
    /// beyond the device's latency — the modulators' lead and latency
    /// (`dsd_worker::Pipe::delay`). The position and the analyzer take it
    /// off with the device's latency. 0 for PCM and a DSD file as it is.
    pub dsd_delay_frames: AtomicU64,
    /// DSD output: steps down to a lighter modulator since the stream opened
    /// (a modulator thread fell behind).
    pub dsd_step_downs: AtomicU64,
    /// DSD output: gaps of DSD silence sent for a modulator thread behind
    /// (steps down and resyncs at the lightest).
    pub dsd_gaps: AtomicU64,
    /// DSD output: frames due before the modulators finished them, sent as
    /// DSD silence.
    pub dsd_late_frames: AtomicU64,
    /// DSD output: the modulator running now, as its place in `CATALOG`
    /// (u8::MAX: none).
    dsd_running: AtomicU8,
}

impl OutputShared {
    /// A stream opened: its DSD output's delay and modulator (none for PCM),
    /// the counts from zero.
    pub fn dsd_opened(&self, delay: usize, kind: Option<ModulatorKind>) {
        self.dsd_delay_frames.store(delay as u64, Ordering::Relaxed);
        self.dsd_step_downs.store(0, Ordering::Relaxed);
        self.dsd_gaps.store(0, Ordering::Relaxed);
        self.dsd_late_frames.store(0, Ordering::Relaxed);
        self.set_dsd_running(kind);
    }

    pub fn set_dsd_running(&self, kind: Option<ModulatorKind>) {
        let i = kind.and_then(|k| CATALOG.iter().position(|i| i.kind == k)).map_or(u8::MAX, |i| i as u8);
        self.dsd_running.store(i, Ordering::Relaxed);
    }

    /// The modulator the DSD output runs now (a lighter one after a step down).
    pub fn dsd_running(&self) -> Option<ModulatorKind> {
        CATALOG.get(self.dsd_running.load(Ordering::Relaxed) as usize).map(|i| i.kind)
    }

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            volume_bits: AtomicU64::new(f64::to_bits(1.0)),
            paused: AtomicBool::new(false),
            hold: AtomicBool::new(false),
            jump_frame: AtomicU64::new(u64::MAX),
            device_latency_frames: AtomicU64::new(0),
            out_level_peak_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
            ramp_frames_read: AtomicU64::new(0),
            hold_ramp_frames: AtomicU64::new(0),
            alive: AtomicBool::new(false),
            device_lost: AtomicBool::new(false),
            last_error: Mutex::new(None),
            dev_peak_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
            dev_peak_max_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
            safety_mutes: AtomicU64::new(0),
            sent_peak_max_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
            dsd_overloads: AtomicU64::new(0),
            dsd_guard_tripped: AtomicBool::new(false),
            dsd_guard_from: AtomicU64::new(u64::MAX),
            dsd_delay_frames: AtomicU64::new(0),
            dsd_step_downs: AtomicU64::new(0),
            dsd_gaps: AtomicU64::new(0),
            dsd_late_frames: AtomicU64::new(0),
            dsd_running: AtomicU8::new(u8::MAX),
        })
    }

    /// Peak dBFS of the pre-volume signal sent to the device in the last ~100 ms.
    /// Returns None when silent or not yet measured.
    pub fn out_level_db(&self) -> Option<f64> {
        let bits = self.out_level_peak_bits.load(Ordering::Relaxed);
        let peak = f64::from_bits(bits);
        if peak <= 0.0 { None } else { Some(20.0 * peak.log10()) }
    }

    /// Peak dBFS of the post-volume, post-dither signal in the last ~100 ms.
    /// Returns None when silent or not yet measured.
    pub fn dev_peak_db(&self) -> Option<f64> {
        let bits = self.dev_peak_bits.load(Ordering::Relaxed);
        let peak = f64::from_bits(bits);
        if peak <= 0.0 { None } else { Some(20.0 * peak.log10()) }
    }

    /// Maximum post-volume peak dBFS since the application started.
    /// Returns None when no audio has been played yet.
    pub fn dev_peak_max_db(&self) -> Option<f64> {
        let bits = self.dev_peak_max_bits.load(Ordering::Relaxed);
        let peak = f64::from_bits(bits);
        if peak <= 0.0 { None } else { Some(20.0 * peak.log10()) }
    }

    /// Maximum peak dBFS of what the device was given since the application
    /// started (after the mutes). None while only silence has gone out: in
    /// a test run (`test_mute`) it is None for good.
    pub fn sent_peak_max_db(&self) -> Option<f64> {
        let peak = f64::from_bits(self.sent_peak_max_bits.load(Ordering::Relaxed));
        if peak <= 0.0 { None } else { Some(20.0 * peak.log10()) }
    }

    /// Set the playback volume from decibels (≤ 0 dBFS).
    pub fn set_volume_db(&self, db: f64) {
        let lin = 10.0_f64.powf(db / 20.0);
        self.volume_bits.store(lin.to_bits(), Ordering::Relaxed);
    }

    /// Read the linear gain (thread-safe, lock-free).
    pub fn volume_lin(&self) -> f64 {
        f64::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }
}

/// Test runs only (the window harness sets AURA_TEST_MUTE=1): everything is
/// rendered, measured and guarded as usual, but the device is sent digital
/// silence, so a test run is never heard, even if the device keeps looping
/// its last buffer after the process is gone.
pub fn test_mute() -> bool {
    static MUTE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MUTE.get_or_init(|| std::env::var_os("AURA_TEST_MUTE").is_some_and(|v| v == "1"))
}

/// Information about the opened stream, reported to the controller.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub device_name: String,
    /// True for exclusive; false when the Windows mixer is in the path.
    pub exclusive: bool,
    /// Human-readable format: "int32 (24-in-32)", "float32", "int16", …
    pub format: String,
    pub rate: u32,
    #[allow(dead_code)]
    pub period_frames: u32,
    pub buffer_frames: u32,
    /// Hardware buffer + stream latency in frames, for spectrum sync.
    pub latency_frames: u32,
}

/// A live output stream. Dropping it stops the audio thread cleanly.
pub struct OutputStream {
    handle: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    /// Up once the stream may start (`start_gated`, `release`).
    go: Arc<AtomicBool>,
    info: StreamInfo,
}

/// A gated stream's wait (`OutputStream::start_gated`): true once it is let
/// go, false when it is stopped first. Nothing is read or written meanwhile.
fn wait_to_start(go: &AtomicBool, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        if go.load(Ordering::Acquire) {
            return true;
        }
        std::thread::park_timeout(std::time::Duration::from_millis(5));
    }
}

impl OutputStream {
    /// Open the device and the audio thread, the stream held at a gate until
    /// `release`. Blocks until the hardware is initialised (or until an error
    /// is reported). At the gate the audio thread waits right before the
    /// stream starts — nothing read from the timeline, nothing written, no
    /// ramp begun — so a new stream can open while its pre-roll renders and
    /// then start as one opened only then starts. Stopped at the gate, it
    /// closes without a sample sent.
    pub fn start_gated(
        cfg: OutputConfig,
        timeline: Arc<Timeline>,
        shared: Arc<OutputShared>,
    ) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_clone = stop_flag.clone();
            let go = Arc::new(AtomicBool::new(false));
            let go_clone = go.clone();
            let shared_clone = shared.clone();
            let (init_tx, init_rx) = mpsc::channel::<Result<StreamInfo, String>>();

            shared.alive.store(true, Ordering::SeqCst);

            let handle = std::thread::Builder::new()
                .name("aura-output".into())
                .spawn(move || {
                    platform::audio_thread(cfg, timeline, shared_clone, stop_clone, go_clone, init_tx);
                })
                .map_err(|e| format!("Failed to spawn audio thread: {e}"))?;

            let info = init_rx
                .recv()
                .map_err(|_| "Audio thread exited before reporting init result".to_string())??;

            Ok(OutputStream { handle: Some(handle), stop_flag, go, info })
        }
        #[cfg(not(windows))]
        {
            let _ = (cfg, timeline, shared);
            Err("WASAPI output is only available on Windows".to_string())
        }
    }

    /// Let a gated stream start (`start_gated`).
    pub fn release(&self) {
        self.go.store(true, Ordering::Release);
        if let Some(h) = &self.handle {
            h.thread().unpark();
        }
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Signal the audio thread to stop and wait for it to exit: the music
    /// fades out and the device is handed silence for its latency first
    /// (`period::close`, 0.3 s at most), then the stream stops.
    /// Idempotent: safe to call even if already stopped or dropped.
    pub fn stop(mut self) {
        // Drop runs do_stop again; the handle is gone by then, so it only
        // re-sets the flag.
        self.do_stop();
    }

    fn do_stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            h.join().ok();
        }
    }
}

impl Drop for OutputStream {
    fn drop(&mut self) {
        self.do_stop();
    }
}

#[cfg(test)]
impl OutputStream {
    /// A stream with no device, for the controller's tests. Its thread
    /// stands in for the audio thread and notes, when it sees the stop
    /// flag, whether the output was held then: what the real thread's last
    /// period played (a hold already let go: the old track, ramping up,
    /// cut off by the stop).
    pub fn test_stream(shared: Arc<OutputShared>, info: StreamInfo) -> (OutputStream, Arc<Mutex<Option<bool>>>) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let held_at_stop = Arc::new(Mutex::new(None));
        let (stop, seen) = (stop_flag.clone(), held_at_stop.clone());
        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            *seen.lock().unwrap() = Some(shared.hold.load(Ordering::Acquire));
        });
        let go = Arc::new(AtomicBool::new(true));
        (OutputStream { handle: Some(handle), stop_flag, go, info }, held_at_stop)
    }

    /// A gated stream with no device (`start_gated`'s stand-in): its thread
    /// waits at the gate as the audio thread does (`wait_to_start`) and notes
    /// what came first — the release, with what the timeline held then, or
    /// the stop.
    pub fn test_gated(timeline: Arc<Timeline>, info: StreamInfo) -> (OutputStream, Arc<Mutex<Option<GateSeen>>>) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let go = Arc::new(AtomicBool::new(false));
        let seen_at = Arc::new(Mutex::new(None));
        let (stop, let_go, seen) = (stop_flag.clone(), go.clone(), seen_at.clone());
        let handle = std::thread::spawn(move || {
            let started = wait_to_start(&let_go, &stop);
            *seen.lock().unwrap() = Some(if started {
                GateSeen::Released { buffered: timeline.buffered_frames() as usize }
            } else {
                GateSeen::Stopped
            });
            while !stop.load(Ordering::SeqCst) {
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
        });
        (OutputStream { handle: Some(handle), stop_flag, go, info }, seen_at)
    }
}

/// What a gated stand-in stream saw first (`OutputStream::test_gated`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateSeen {
    /// Let go, the timeline holding this many frames.
    Released { buffered: usize },
    /// Stopped at the gate: never started, nothing sent.
    Stopped,
}

#[cfg(test)]
mod gate_tests {
    use super::wait_to_start;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A gated stream starts when it is let go — from another thread, while
    /// it waits — and never when it is stopped first, let go or not.
    #[test]
    fn a_gated_stream_starts_when_let_go_and_never_when_stopped_first() {
        assert!(!wait_to_start(&AtomicBool::new(false), &AtomicBool::new(true)), "stopped");
        assert!(!wait_to_start(&AtomicBool::new(true), &AtomicBool::new(true)), "stopped, then let go");
        assert!(wait_to_start(&AtomicBool::new(true), &AtomicBool::new(false)), "let go");
        let (go, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (g, s) = (go.clone(), stop.clone());
        let waits = std::thread::spawn(move || wait_to_start(&g, &s));
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(!waits.is_finished(), "it waits");
        go.store(true, Ordering::Release);
        waits.thread().unpark();
        assert!(waits.join().unwrap(), "let go while it waits");
    }
}

/// For each requested sample rate, return the best exclusive format this device
/// accepts, or `"unsupported"` if none of the Dsp-mode candidates are accepted.
/// The probe does not open a stream; it only queries format support.
// Intentional stub: reserved for the future DAC capabilities panel.
#[allow(dead_code)]
pub fn probe_formats(
    device_id: Option<&str>,
    rates: &[u32],
) -> Result<Vec<(u32, String)>, String> {
    #[cfg(windows)]
    return platform::probe_formats_impl(device_id, rates);
    #[cfg(not(windows))]
    {
        let _ = (device_id, rates);
        Ok(Vec::new())
    }
}

/// A plain-text report of what the device accepts in exclusive mode and why
/// it refuses what it refuses (`aura-player --probe-device`).
pub fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
    #[cfg(windows)]
    return platform::diagnose_impl(device_id, rates);
    #[cfg(not(windows))]
    {
        let _ = (device_id, rates);
        Ok(String::new())
    }
}

// ── One device period, without a device ──────────────────────────────────────

/// What the audio thread hands the device, period by period: the timeline
/// read (or not, while held or paused), the ramps, the volume and dither (DSP
/// mode) or none of them (Direct), the packing, the guard and the test mute.
/// No device in here: every way a period is written runs in the tests too.
#[cfg_attr(not(windows), allow(dead_code))]
mod period {
    use super::*;
    use crate::audio::dsd::dop::{self, Container, DopPacker};
    use crate::audio::dsd::encoder::new_encoder;
    use crate::audio::dsd::{encoder_ratio, DSD_SILENCE};
    use crate::player::dsd_direct::{self, DsdGuard};
    use crate::player::dsd_worker::{Lag, Pipe, Tag};

    /// Compact tag for the sample packing loop.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) enum SampleFmt {
        Int16,
        Int24Packed,
        /// Covers both 24-in-32 and 32/32; packed as i32 scaled to 2^31.
        Int32,
        Float32,
    }

    // ── Sample packing ───────────────────────────────────────────────────────

    /// Pack `available` stereo frames from ch_l / ch_r into `out` as raw
    /// device bytes. The format is fixed for the lifetime of the stream; the
    /// dispatch on `fmt` compiles to a single branch outside the per-sample
    /// loop.
    ///
    /// Conversion rules:
    ///   Int32 (24-in-32 and 32/32): x * 2^31 → i32 LE (24-bit grid values map
    ///     exactly — the lower 8 bits are zero after dithering).
    ///   Int24 packed: x * 2^23 → 3-byte LE.
    ///   Int16: x * 2^15 → i16 LE.
    ///   Float32: x as f32 → 4-byte LE.
    #[inline]
    pub(super) fn pack_frames(ch_l: &[f64], ch_r: &[f64], avail: usize, fmt: SampleFmt, out: &mut [u8]) {
        match fmt {
            SampleFmt::Int32 => {
                for i in 0..avail {
                    let l = (ch_l[i] * 2_147_483_648.0)
                        .round()
                        .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                    let r = (ch_r[i] * 2_147_483_648.0)
                        .round()
                        .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                    let b = i * 8;
                    out[b..b + 4].copy_from_slice(&l.to_le_bytes());
                    out[b + 4..b + 8].copy_from_slice(&r.to_le_bytes());
                }
            }
            SampleFmt::Float32 => {
                for i in 0..avail {
                    let b = i * 8;
                    out[b..b + 4].copy_from_slice(&(ch_l[i] as f32).to_le_bytes());
                    out[b + 4..b + 8].copy_from_slice(&(ch_r[i] as f32).to_le_bytes());
                }
            }
            SampleFmt::Int24Packed => {
                for i in 0..avail {
                    let l = (ch_l[i] * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    let r = (ch_r[i] * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    let b = i * 6;
                    out[b..b + 3].copy_from_slice(&l.to_le_bytes()[..3]);
                    out[b + 3..b + 6].copy_from_slice(&r.to_le_bytes()[..3]);
                }
            }
            SampleFmt::Int16 => {
                for i in 0..avail {
                    let l = (ch_l[i] * 32_768.0)
                        .round()
                        .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    let r = (ch_r[i] * 32_768.0)
                        .round()
                        .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    let b = i * 4;
                    out[b..b + 2].copy_from_slice(&l.to_le_bytes());
                    out[b + 2..b + 4].copy_from_slice(&r.to_le_bytes());
                }
            }
        }
    }

    // ── The period asked of the device ───────────────────────────────────────

    /// The exclusive period asked of the device, 100-ns units, from its
    /// default and minimum periods (before the alignment to its buffer): its
    /// default, 10 ms at least and 50 ms at most, never under its minimum.
    /// The player needs no small latency — it renders seconds ahead — and a
    /// short period is what a driver set for a small ASIO latency crackles at
    /// (1.5 × the minimum was ~2 ms on an RME). 50 ms keeps what the device
    /// reads in one go far inside the timeline's rewrite guard.
    pub(super) fn exclusive_period_hns(def_period: i64, min_period: i64) -> i64 {
        const MS: i64 = 10_000;
        def_period.max(10 * MS).min(50 * MS).max(min_period)
    }

    // ── Post-pack measurement and safety guard ───────────────────────────────

    /// Unpack the max |x| peak and NaN/Inf presence from a period that has
    /// already been packed into device bytes by `pack_frames`. Decoding the
    /// same bytes that were written measures what the device is given; the
    /// format itself is only as right as resolve_sample_fmt.
    pub(super) fn unpack_peak(bytes: &[u8], avail: usize, fmt: SampleFmt) -> (f64, bool) {
        let mut peak = 0.0f64;
        let mut has_nan_inf = false;
        match fmt {
            SampleFmt::Float32 => {
                for i in 0..avail {
                    let b = i * 8;
                    let l = f32::from_le_bytes(bytes[b..b + 4].try_into().unwrap()) as f64;
                    let r = f32::from_le_bytes(bytes[b + 4..b + 8].try_into().unwrap()) as f64;
                    if l.is_nan() || l.is_infinite() || r.is_nan() || r.is_infinite() {
                        has_nan_inf = true;
                        peak = f64::INFINITY;
                    } else {
                        let s = l.abs().max(r.abs());
                        if s > peak { peak = s; }
                    }
                }
            }
            SampleFmt::Int32 => {
                for i in 0..avail {
                    let b = i * 8;
                    let l = i32::from_le_bytes(bytes[b..b + 4].try_into().unwrap()) as f64
                        / 2_147_483_648.0;
                    let r = i32::from_le_bytes(bytes[b + 4..b + 8].try_into().unwrap()) as f64
                        / 2_147_483_648.0;
                    let s = l.abs().max(r.abs());
                    if s > peak { peak = s; }
                }
            }
            SampleFmt::Int24Packed => {
                for i in 0..avail {
                    let b = i * 6;
                    // Read 3 bytes per sample, sign-extend from 24 bits via arithmetic shift.
                    let l_raw = i32::from_le_bytes([bytes[b], bytes[b + 1], bytes[b + 2], 0]);
                    let r_raw = i32::from_le_bytes([bytes[b + 3], bytes[b + 4], bytes[b + 5], 0]);
                    let l = ((l_raw << 8) >> 8) as f64 / 8_388_608.0;
                    let r = ((r_raw << 8) >> 8) as f64 / 8_388_608.0;
                    let s = l.abs().max(r.abs());
                    if s > peak { peak = s; }
                }
            }
            SampleFmt::Int16 => {
                for i in 0..avail {
                    let b = i * 4;
                    let l = i16::from_le_bytes(bytes[b..b + 2].try_into().unwrap()) as f64
                        / 32_768.0;
                    let r = i16::from_le_bytes(bytes[b + 2..b + 4].try_into().unwrap()) as f64
                        / 32_768.0;
                    let s = l.abs().max(r.abs());
                    if s > peak { peak = s; }
                }
            }
        }
        (peak, has_nan_inf)
    }

    /// Returns true when the period must be replaced with silence to protect
    /// the listener. Pure function — no side-effects, testable without hardware.
    ///
    /// DSP mode: mute if peak exceeds the set volume by more than 12 dB, OR if
    /// a float stream contains NaN/Inf or any |x| > 1.
    /// Direct mode: only float streams can carry NaN/Inf; mute on those only.
    pub(super) fn safety_mute_needed(
        peak: f64,
        has_nan_inf: bool,
        fmt: SampleFmt,
        volume_lin: f64,
        is_direct: bool,
    ) -> bool {
        let float_bad =
            matches!(fmt, SampleFmt::Float32) && (has_nan_inf || peak > 1.0);
        if is_direct {
            float_bad
        } else {
            float_bad || peak > (volume_lin * 4.0).max(1e-5)
        }
    }

    /// DSD output's part of `Periods`: the two channels' modulators on their
    /// threads (`dsd_worker::Pipe`), the DoP packer (its marker phase runs on
    /// across every period of the stream) and their scratch bytes.
    pub(super) struct Dop {
        container: Container,
        packer: DopPacker,
        /// The two channels' modulators, `lead` frames ahead of the device;
        /// None for a DSD file as it is, whose words are unpacked
        /// (`dsd_direct::bytes`) and watched by `guard` instead. Every period
        /// of the stream pushes to it and pops from it the same frames, DSD
        /// silence included, so what it holds in flight (the fade before a
        /// pause, the music before a stop) goes out before the silence. DSD
        /// silence pushed in place of music (lead-in, pause, hold, guard)
        /// starts the modulators from silence again before they next run;
        /// PCM zeros inside the music never do — they go through them like
        /// any other samples.
        pipe: Option<Pipe>,
        guard: Option<DsdGuard>,
        /// The last track start (`OutputShared::dsd_guard_from`) the guard
        /// was reset at.
        guard_from_seen: u64,
        /// Two DSD bytes per frame (16 one-bit samples), each channel.
        bytes_l: Vec<u8>,
        bytes_r: Vec<u8>,
        /// Frames of DSD silence still to send before the first music: a
        /// DAC locks to DSD on the markers before anything is heard. The
        /// pipe's lead of DSD silence counts in it.
        lead_in_left: usize,
    }

    /// The lead-in of DSD silence at the start of a DoP stream.
    pub(super) const DOP_LEAD_IN_S: f64 = 0.1;

    impl Dop {
        fn new(rate: u32, dsd: DsdRate, modulator: Option<ModulatorKind>, fmt: SampleFmt, buf_frames: usize) -> Result<Dop, String> {
            let container = match fmt {
                SampleFmt::Int24Packed => Container::Int24,
                SampleFmt::Int32 => Container::Int32,
                other => return Err(format!("{DOP_FORMAT_ERR}; the device took {other:?} at {rate} Hz")),
            };
            match encoder_ratio(rate, dsd) {
                Ok(r) if r == dop::BITS_PER_FRAME => {}
                _ => {
                    return Err(format!(
                        "DoP: a {rate} Hz stream cannot carry DSD{} at {} Hz (its carrier is {} Hz)",
                        dsd.multiple,
                        dsd.bit_rate(),
                        dsd.dop_carrier()
                    ))
                }
            }
            let pipe = match modulator {
                Some(m) => Some(Pipe::new(m, rate, dsd, buf_frames, &|k| new_encoder(k, rate, dsd))?),
                None => None,
            };
            let lead = pipe.as_ref().map_or(0, |p| p.lead());
            Ok(Dop {
                container,
                packer: DopPacker::new(),
                guard: pipe.is_none().then(|| DsdGuard::new(dsd)),
                guard_from_seen: u64::MAX,
                pipe,
                bytes_l: vec![0; 2 * buf_frames],
                bytes_r: vec![0; 2 * buf_frames],
                lead_in_left: ((DOP_LEAD_IN_S * rate as f64).ceil() as usize).saturating_sub(lead),
            })
        }
    }

    /// The state the audio thread keeps from one period to the next, and its
    /// scratch buffers, sized once to the hardware buffer: nothing is
    /// allocated per period.
    pub(super) struct Periods {
        rate: u32,
        fmt: SampleFmt,
        blockalign: usize,
        /// DSP mode's dither, which folds in the volume; Direct and DSD
        /// output have none.
        dither: Option<DitherState>,
        /// DSD output (DoP); None for PCM.
        dop: Option<Dop>,
        is_direct: bool,
        /// A test run (`test_mute`): the device is sent silence.
        mute: bool,
        interleaved: Vec<f64>,
        ch_l: Vec<f64>,
        ch_r: Vec<f64>,
        out_bytes: Vec<u8>,
        /// The ramp envelope for the hold/pause declic and the jump ramp-up:
        /// the gain runs 0 → 1 on a fade-in, 1 → 0 on a fade-out, 1 between.
        /// For a hold ramp `ramp_step` is the per-sample decrement; 0 means
        /// the default (one period), recomputed from `avail` each period.
        ramp_gain: f64,
        ramp_step: f64,
        ramping_down: bool,
        ramping_up: bool,
        /// Output level measurement (A3): the pre-volume peak over ~100 ms.
        level_window: usize,
        level_frames: usize,
        level_peak: f64,
        /// The post-volume device peak, over the same window.
        dev_level_peak: f64,
        dev_level_frames: usize,
        /// Frames of silence the device was handed in a row, up to the last
        /// period: whatever sound it still holds is older than them.
        silent_run: u64,
        /// The timeline frame the last read started at.
        read_from: u64,
    }

    impl Periods {
        /// Fails only for DSD output: a device format DoP cannot travel in
        /// (`DOP_FORMAT_ERR`), or a rate that is not the DSD rate's carrier.
        pub(super) fn new(rate: u32, fmt: SampleFmt, blockalign: usize, buf_frames: usize, mode: &OutputMode, mute: bool) -> Result<Periods, String> {
            let (dither, dop) = match mode {
                // A 16-bit device gets its dither on the 16-bit grid; every
                // wider one the 24-bit result.
                OutputMode::Dsp => (Some(DitherState::for_bits(rate, if fmt == SampleFmt::Int16 { 16 } else { 24 })), None),
                OutputMode::Direct { .. } => (None, None),
                OutputMode::Dsd { rate: dsd, modulator } => (None, Some(Dop::new(rate, *dsd, Some(*modulator), fmt, buf_frames)?)),
                OutputMode::DsdDirect { rate: dsd } => (None, Some(Dop::new(rate, *dsd, None, fmt, buf_frames)?)),
            };
            Ok(Periods {
                rate,
                fmt,
                blockalign,
                is_direct: matches!(mode, OutputMode::Direct { .. }),
                dither,
                dop,
                mute,
                interleaved: vec![0.0; buf_frames * 2],
                ch_l: vec![0.0; buf_frames],
                ch_r: vec![0.0; buf_frames],
                out_bytes: vec![0u8; buf_frames * blockalign],
                ramp_gain: 1.0,
                ramp_step: 0.0,
                ramping_down: false,
                ramping_up: false,
                level_window: ((0.1 * rate as f64) as usize).max(1),
                level_frames: 0,
                level_peak: 0.0,
                dev_level_peak: 0.0,
                dev_level_frames: 0,
                silent_run: 0,
                read_from: 0,
            })
        }

        /// What the device is handed before the stream starts, `frames` of
        /// it: silence — zeros for PCM, DSD silence with markers for DoP
        /// (the markers it starts with run on into the first period).
        pub(super) fn prefill(&mut self, frames: usize) -> Vec<u8> {
            let mut b = vec![0u8; frames * self.blockalign];
            if let Some(d) = self.dop.as_mut() {
                d.packer.silence(frames, d.container, &mut b);
            }
            b
        }

        /// DSD output: the modulators' overloads so far.
        #[cfg(test)]
        pub(super) fn dop_overloads(&self) -> Option<u64> {
            self.dop.as_ref().and_then(|d| d.pipe.as_ref()).map(|p| p.overloads())
        }

        /// DSD output: the frames from a read of the timeline to the music
        /// heard, beyond the device's latency (`Pipe::delay`). 0 otherwise.
        pub(super) fn dsd_delay(&self) -> usize {
            self.dop.as_ref().and_then(|d| d.pipe.as_ref()).map_or(0, |p| p.delay())
        }

        /// DSD output: the modulator the pipe started with.
        pub(super) fn dsd_kind(&self) -> Option<ModulatorKind> {
            self.dop.as_ref().and_then(|d| d.pipe.as_ref()).map(|p| p.kind())
        }

        /// A DSD file as it is: the timeline holds DoP words, which no
        /// ramp may scale.
        fn raw(&self) -> bool {
            self.dop.as_ref().is_some_and(|d| d.pipe.is_none())
        }

        /// The next period of `avail` frames (at most the buffer's), packed
        /// for the device: its bytes, and the words a failed write of it is
        /// said with — " (hold)" held in silence, " (pause)" paused in
        /// silence, nothing for every other period.
        pub(super) fn next(&mut self, avail: usize, timeline: &Timeline, shared: &OutputShared) -> (&[u8], &'static str) {
            let nbytes = avail * self.blockalign;
            // DoP: DSD silence first, nothing read, so the DAC has locked to
            // DSD before the music. The position stays right: what the
            // device was handed beyond the timeline's reads is in the
            // measured latency (frames written − frames played), which the
            // position and the analyzer take off the read position.
            if let Some(d) = self.dop.as_mut() {
                if d.lead_in_left > 0 {
                    d.lead_in_left = d.lead_in_left.saturating_sub(avail);
                    return (self.silence(nbytes, avail, shared), " (lead-in)");
                }
            }
            // Hold (track switch / seek): ramp to silence, wait for a jump
            // frame from the render thread, then teleport the read pointer and
            // ramp back up. The hold flag is set by the command thread before
            // any preparation starts, so no old audio escapes into the new one.
            let held = shared.hold.load(Ordering::Acquire);
            if held && self.raw() && self.ramp_gain > 0.0 {
                // DSD as it is: no fade — a period of DSD silence in its
                // place, the period read and dropped as a fade's would be.
                shared.ramp_frames_read.store(0, Ordering::Relaxed);
                let n_read = self.read(timeline, avail);
                shared.ramp_frames_read.fetch_add(n_read as u64, Ordering::Relaxed);
                self.ramp_gain = 0.0;
                self.ramping_down = false;
                self.ramp_step = 0.0;
                return (self.silence(nbytes, avail, shared), "");
            }
            if held || self.ramping_down {
                if !self.ramping_down && self.ramp_gain > 0.0 {
                    // Just entered hold: start the ramp-down and capture the
                    // ramp length from hold_ramp_frames (0 = one-period default).
                    self.ramping_down = true;
                    shared.ramp_frames_read.store(0, Ordering::Relaxed);
                    let rf = shared.hold_ramp_frames.load(Ordering::Acquire);
                    self.ramp_step = if rf == 0 { 0.0 } else { 1.0 / rf as f64 };
                }
                if self.ramping_down && self.ramp_gain > 0.0 {
                    // Read and immediately discard one period from the timeline
                    // while fading out, so the timeline read position advances
                    // past the stale audio (the render thread will rewrite
                    // ahead of here once the hold is engaged).
                    let n_read = self.read(timeline, avail);
                    shared.ramp_frames_read.fetch_add(n_read as u64, Ordering::Relaxed);
                    // Per-sample step: either the captured ramp_step (extended
                    // fade) or the one-period default (completes this period).
                    let step = if self.ramp_step > 0.0 { self.ramp_step } else { 1.0 / avail as f64 };
                    self.fade_down(avail, step);
                    // The fade is heard like any other period: at the
                    // listener's volume (it went out at 0 dB before).
                    self.deliver(nbytes, avail, shared);
                    if self.ramp_gain == 0.0 {
                        self.ramping_down = false;
                        self.ramp_step = 0.0; // reset for the next ramp
                    }
                    return (&self.out_bytes[..nbytes], "");
                }
                // Silence phase: check for a pending jump from the render thread.
                let jf = shared.jump_frame.load(Ordering::Acquire);
                if jf == u64::MAX {
                    // Still waiting: write silence without advancing the reader.
                    return (self.silence(nbytes, avail, shared), " (hold)");
                }
                // The render thread has placed new audio starting at `jf`.
                // Teleport the read pointer there and clear the hold.
                timeline.jump_to(jf);
                shared.jump_frame.store(u64::MAX, Ordering::Release);
                shared.hold.store(false, Ordering::Release);
                self.ramping_up = true;
                self.ramp_gain = 0.0;
                // Fall through to the normal read path this period.
            }

            if shared.paused.load(Ordering::Relaxed) {
                if self.raw() {
                    // DSD as it is: DSD silence at once, nothing read.
                    self.ramp_gain = 0.0;
                    self.ramping_down = false;
                    return (self.silence(nbytes, avail, shared), " (pause)");
                }
                // Silence without advancing the timeline, after a fade.
                if !self.ramping_down && self.ramp_gain > 0.0 {
                    self.ramping_down = true;
                }
                if self.ramping_down && self.ramp_gain > 0.0 {
                    self.read(timeline, avail);
                    self.fade_down(avail, 1.0 / avail as f64);
                    self.deliver(nbytes, avail, shared);
                    if self.ramp_gain == 0.0 {
                        self.ramping_down = false;
                    }
                    return (&self.out_bytes[..nbytes], "");
                }
                return (self.silence(nbytes, avail, shared), " (pause)");
            }
            // Resumed from pause: restore the ramp gain.
            if self.ramp_gain == 0.0 && !self.ramping_up {
                self.ramping_up = true;
            }

            // Pre-dither f64 frames from the timeline; what it does not hold
            // yet (the render thread behind) is silence, and counted.
            let n_read = self.read(timeline, avail);
            if n_read < avail {
                timeline.note_underrun(avail - n_read);
            }
            for i in 0..avail {
                self.ch_l[i] = self.interleaved[2 * i];
                self.ch_r[i] = self.interleaved[2 * i + 1];
            }
            if self.raw() {
                // DSD as it is: the words go out whole — no ramp, no level
                // (they are not samples), no tap.
                self.ramp_gain = 1.0;
                self.ramping_up = false;
                self.deliver(nbytes, avail, shared);
                return (&self.out_bytes[..nbytes], "");
            }
            // The ramp-up envelope after a jump or a pause → play transition.
            if self.ramping_up {
                let step = 1.0 / avail as f64;
                for i in 0..avail {
                    self.ramp_gain = (self.ramp_gain + step).min(1.0);
                    self.ch_l[i] *= self.ramp_gain;
                    self.ch_r[i] *= self.ramp_gain;
                }
                if self.ramp_gain >= 1.0 {
                    self.ramping_up = false;
                    self.ramp_gain = 1.0;
                }
            }
            crate::player::output_tap::frames(&self.ch_l[..avail], &self.ch_r[..avail]);

            // Output level measurement (A3): the pre-volume peak over a
            // ~100 ms window, published so the status can prove audio is
            // flowing to the device.
            for i in 0..avail {
                let s = self.ch_l[i].abs().max(self.ch_r[i].abs());
                if s > self.level_peak {
                    self.level_peak = s;
                }
            }
            self.level_frames += avail;
            if self.level_frames >= self.level_window {
                shared.out_level_peak_bits.store(self.level_peak.to_bits(), Ordering::Relaxed);
                self.level_peak = 0.0;
                self.level_frames = 0;
            }
            self.deliver(nbytes, avail, shared);
            (&self.out_bytes[..nbytes], "")
        }

        /// One period from the timeline into `interleaved`; the frames it
        /// does not hold read as silence. Returns how many it held.
        fn read(&mut self, timeline: &Timeline, avail: usize) -> usize {
            self.read_from = timeline.read_pos();
            let n_read = timeline.read_into(&mut self.interleaved[..avail * 2], avail);
            if n_read < avail {
                for s in &mut self.interleaved[n_read * 2..avail * 2] {
                    *s = 0.0;
                }
            }
            n_read
        }

        /// The period read, faded down from the ramp's gain by `step` a sample.
        fn fade_down(&mut self, avail: usize, step: f64) {
            for i in 0..avail {
                self.ch_l[i] = self.interleaved[2 * i] * self.ramp_gain;
                self.ch_r[i] = self.interleaved[2 * i + 1] * self.ramp_gain;
                self.ramp_gain = (self.ramp_gain - step).max(0.0);
            }
        }

        /// The period in `ch_l`/`ch_r` to the device's bytes. DSP mode: the
        /// volume and the dither, one read of the volume (the guard judges
        /// the period by the gain it was rendered with). Direct mode:
        /// bit-perfect — no dither, no volume. Packed, then guarded.
        fn deliver(&mut self, nbytes: usize, avail: usize, shared: &OutputShared) {
            let vol = shared.volume_lin();
            if self.raw() {
                return self.deliver_raw(nbytes, avail, shared);
            }
            if self.dop.is_some() {
                return self.deliver_dop(nbytes, avail, vol, shared);
            }
            if let Some(d) = self.dither.as_mut() {
                d.process(&mut self.ch_l[..avail], &mut self.ch_r[..avail], vol);
            }
            pack_frames(&self.ch_l[..avail], &self.ch_r[..avail], avail, self.fmt, &mut self.out_bytes);
            self.guard(nbytes, avail, vol, shared);
        }

        /// A period of silence, nothing read: the bytes cleared (they still
        /// hold the last period played — a pause looped those few
        /// milliseconds as a buzz) and guarded like any other.
        fn silence(&mut self, nbytes: usize, avail: usize, shared: &OutputShared) -> &[u8] {
            if let Some(d) = self.dop.as_mut() {
                // DoP: DSD silence with its markers — zero words would take
                // the DAC out of DSD mode. The modulators' pipe takes the
                // silence in and gives out what it still holds first.
                let sent = match d.pipe.as_mut() {
                    Some(pipe) => {
                        pipe.push_silence(avail);
                        let music = Self::pop_dop(d, avail, &mut self.out_bytes[..nbytes], shared);
                        if music { f64::MIN_POSITIVE } else { 0.0 }
                    }
                    None => {
                        d.packer.silence(avail, d.container, &mut self.out_bytes[..nbytes]);
                        0.0
                    }
                };
                self.measure(0.0, avail, shared);
                self.note_sent(sent, avail, shared);
                return &self.out_bytes[..nbytes];
            }
            self.out_bytes[..nbytes].fill(0);
            self.guard(nbytes, avail, shared.volume_lin(), shared);
            &self.out_bytes[..nbytes]
        }

        /// DoP's `deliver`: the volume applied in PCM (no dither — the
        /// modulator's own noise shaping takes its place), the period judged
        /// on that PCM as `guard` judges PCM, then modulated and packed. The
        /// guard's mute and the test mute send DSD silence with markers; the
        /// guard's mute also keeps the period from the modulators (a NaN would
        /// stay in their state), the test mute does not — they run as usual,
        /// unheard.
        fn deliver_dop(&mut self, nbytes: usize, avail: usize, vol: f64, shared: &OutputShared) {
            let (mut peak, mut nan) = (0.0f64, false);
            for i in 0..avail {
                self.ch_l[i] *= vol;
                self.ch_r[i] *= vol;
                let (l, r) = (self.ch_l[i], self.ch_r[i]);
                if !l.is_finite() || !r.is_finite() {
                    nan = true;
                } else {
                    peak = peak.max(l.abs()).max(r.abs());
                }
            }
            self.measure(if nan { 1.0 } else { peak }, avail, shared);
            let guarded = nan || safety_mute_needed(peak, false, self.fmt, vol, false);
            if guarded {
                shared.safety_mutes.fetch_add(1, Ordering::Relaxed);
            }
            let Periods { dop, ch_l, ch_r, out_bytes, mute, .. } = self;
            let d = dop.as_mut().expect("DoP output");
            let pipe = d.pipe.as_mut().expect("DSD output's modulators");
            if guarded {
                // Kept from the modulators: they start again from silence.
                pipe.push_silence(avail);
            } else {
                pipe.push(&ch_l[..avail], &ch_r[..avail], if *mute { Tag::Muted } else { Tag::Music });
            }
            let music = Self::pop_dop(d, avail, &mut out_bytes[..nbytes], shared);
            // What goes out is the music pushed `lead` frames ago; its peak
            // is not kept, this period's stands for it (a running maximum).
            let sent = if music { peak.max(f64::MIN_POSITIVE) } else { 0.0 };
            self.note_sent(sent, avail, shared);
        }

        /// The modulators' bytes of the next `avail` frames, packed with their
        /// markers into `out` (DSD silence for what they did not finish, and
        /// for muted frames); the status's counts kept. True if music went out.
        fn pop_dop(d: &mut Dop, avail: usize, out: &mut [u8], shared: &OutputShared) -> bool {
            let pipe = d.pipe.as_mut().expect("DSD output's modulators");
            let (got, lag) = pipe.pop(avail, &mut d.bytes_l[..2 * avail], &mut d.bytes_r[..2 * avail]);
            shared.dsd_overloads.store(pipe.overloads(), Ordering::Relaxed);
            if got.missed > 0 {
                shared.dsd_late_frames.fetch_add(got.missed as u64, Ordering::Relaxed);
            }
            if let Some(lag) = lag {
                if lag == Lag::SteppedDown {
                    shared.dsd_step_downs.store(pipe.step_downs(), Ordering::Relaxed);
                }
                shared.dsd_gaps.store(pipe.gaps(), Ordering::Relaxed);
                shared.set_dsd_running(Some(pipe.kind()));
                shared.dsd_delay_frames.store(pipe.delay() as u64, Ordering::Relaxed);
            }
            d.packer.pack(&d.bytes_l, &d.bytes_r, avail, d.container, out);
            got.music
        }

        /// A DSD file's period as it is: the words back to their bytes, packed
        /// with their markers — nothing applied. The guard watches the bytes;
        /// once tripped, DSD silence goes out in their place until the next
        /// track starts (`OutputShared::dsd_guard_from`), where a fresh guard
        /// takes over from that very frame. A test run sends DSD silence.
        /// No PCM level exists here: the peaks are left alone, only the run
        /// of silence (`close`) is kept.
        fn deliver_raw(&mut self, nbytes: usize, avail: usize, shared: &OutputShared) {
            let Periods { dop, ch_l, ch_r, out_bytes, mute, silent_run, read_from, .. } = self;
            let d = dop.as_mut().expect("DoP output");
            for i in 0..avail {
                let [a, b] = dsd_direct::bytes(ch_l[i]);
                d.bytes_l[2 * i] = a;
                d.bytes_l[2 * i + 1] = b;
                let [a, b] = dsd_direct::bytes(ch_r[i]);
                d.bytes_r[2 * i] = a;
                d.bytes_r[2 * i + 1] = b;
            }
            // A track starting in this period, or passed unread (a jump):
            // the frames before it are the old track's, judged as before.
            let from = shared.dsd_guard_from.load(Ordering::Acquire);
            let start = (from != u64::MAX && from != d.guard_from_seen && from < *read_from + avail as u64).then(|| {
                d.guard_from_seen = from;
                (from.saturating_sub(*read_from) as usize).min(avail)
            });
            let guard = d.guard.as_mut().expect("the DSD guard");
            let parts = match start {
                Some(k) => [(0, k), (k, avail)],
                None => [(0, avail), (avail, avail)],
            };
            for (n, (a, b)) in parts.into_iter().enumerate() {
                if n == 1 && start.is_some() {
                    guard.reset();
                    shared.dsd_guard_tripped.store(false, Ordering::Relaxed);
                }
                if a == b {
                    continue;
                }
                let before = guard.tripped();
                if guard.check(&d.bytes_l[2 * a..2 * b], &d.bytes_r[2 * a..2 * b]) {
                    if !before {
                        shared.safety_mutes.fetch_add(1, Ordering::Relaxed);
                        shared.dsd_guard_tripped.store(true, Ordering::Relaxed);
                    }
                    d.bytes_l[2 * a..2 * b].fill(DSD_SILENCE);
                    d.bytes_r[2 * a..2 * b].fill(DSD_SILENCE);
                }
            }
            let (l, r) = (&d.bytes_l[..2 * avail], &d.bytes_r[..2 * avail]);
            let silent = *mute || l.iter().chain(r).all(|&b| b == DSD_SILENCE);
            let out = &mut out_bytes[..nbytes];
            if *mute {
                d.packer.silence(avail, d.container, out);
            } else {
                d.packer.pack(&d.bytes_l, &d.bytes_r, avail, d.container, out);
            }
            *silent_run = if silent { *silent_run + avail as u64 } else { 0 };
        }

        /// The packed period measured, silenced by the safety guard if it
        /// must be, then by the test mute; and what is left — what the device
        /// is given — measured too (`sentPeakMaxDb`). Every period comes
        /// through here, silence too, so the peaks move on a regular cadence.
        fn guard(&mut self, nbytes: usize, avail: usize, volume_lin: f64, shared: &OutputShared) {
            let (peak, has_nan_inf) = unpack_peak(&self.out_bytes[..nbytes], avail, self.fmt);
            self.measure(if has_nan_inf { 1.0 } else { peak }, avail, shared);
            let bytes = &mut self.out_bytes[..nbytes];
            // Safety mute: silence instead of this period. Nothing is logged
            // here (a real-time thread); the controller logs when the count
            // changes.
            if safety_mute_needed(peak, has_nan_inf, self.fmt, volume_lin, self.is_direct) {
                bytes.fill(0);
                shared.safety_mutes.fetch_add(1, Ordering::Relaxed);
            }
            // Test runs: measured and guarded above, then silence to the device.
            if self.mute {
                bytes.fill(0);
            }
            // What the device is given after both: in a test run zeros and
            // nothing else, ever — sentPeakMaxDb stays null. The window
            // harness's guard checks it where BIT-PERFECT has no volume to
            // keep a loud stream quiet.
            let (sent, _) = unpack_peak(bytes, avail, self.fmt);
            self.note_sent(sent, avail, shared);
        }

        /// The post-volume peak of what was rendered for the device, before
        /// the safety and test mutes: devPeakDb over ~100 ms, and the running
        /// maximum on every period, so a short tail is never lost.
        fn measure(&mut self, measured: f64, avail: usize, shared: &OutputShared) {
            if measured > self.dev_level_peak {
                self.dev_level_peak = measured;
            }
            // Running max: only the audio thread writes, so no CAS needed.
            let old_max = f64::from_bits(shared.dev_peak_max_bits.load(Ordering::Relaxed));
            if measured > old_max {
                shared.dev_peak_max_bits.store(measured.to_bits(), Ordering::Relaxed);
            }
            self.dev_level_frames += avail;
            if self.dev_level_frames >= self.level_window {
                shared.dev_peak_bits.store(self.dev_level_peak.to_bits(), Ordering::Relaxed);
                self.dev_level_peak = 0.0;
                self.dev_level_frames = 0;
            }
        }

        /// What the device was given after the mutes, its peak `sent`:
        /// `sentPeakMaxDb`, and the run of silence it holds (`close`).
        fn note_sent(&mut self, sent: f64, avail: usize, shared: &OutputShared) {
            let sent = if sent.is_finite() { sent } else { f64::MAX };
            if sent > f64::from_bits(shared.sent_peak_max_bits.load(Ordering::Relaxed)) {
                shared.sent_peak_max_bits.store(sent.to_bits(), Ordering::Relaxed);
            }
            self.silent_run = if sent == 0.0 { self.silent_run + avail as u64 } else { 0 };
        }

        /// A period of the stream's close (`close`): the music faded out from
        /// where the ramp's gain stands, within the period, and heard like any
        /// other — at the listener's volume, dithered, guarded; then silence,
        /// nothing read. Held or paused, it is silence already.
        fn closing(&mut self, avail: usize, timeline: &Timeline, shared: &OutputShared) -> &[u8] {
            let nbytes = avail * self.blockalign;
            // Stopped in DoP's lead-in: no music went out yet, none goes now.
            let lead_in = self.dop.as_ref().is_some_and(|d| d.lead_in_left > 0);
            // A DSD file as it is stops on DSD silence: no fade.
            if self.ramp_gain == 0.0 || lead_in || self.raw() {
                return self.silence(nbytes, avail, shared);
            }
            self.read(timeline, avail);
            self.fade_down(avail, 1.0 / avail as f64);
            self.deliver(nbytes, avail, shared);
            self.ramping_down = false;
            self.ramping_up = false;
            &self.out_bytes[..nbytes]
        }

        /// The silence the device is handed before the stream stops: what it
        /// holds in flight (`latency`, the measured `device_latency_frames`)
        /// and two periods more, 300 ms at most.
        fn close_tail(&self, latency: u64) -> u64 {
            // DSD output: the modulators' pipe holds its delay of music more.
            let pipe = self.dsd_delay() as u64;
            (latency + 2 * self.ch_l.len() as u64).min((0.3 * self.rate as f64) as u64) + pipe
        }
    }

    // ── The device, period by period ─────────────────────────────────────────

    /// What the audio thread asks of the device: WASAPI's event-driven render
    /// client (`platform`), a stand-in in the tests.
    pub(super) trait Device {
        /// Wait for the device to ask for a period: false when it did not
        /// within the wait (the stop flag is looked at again).
        fn wait(&mut self) -> bool;
        /// The frames the device takes now; its error when it is gone.
        fn space(&mut self) -> Result<usize, String>;
        /// Hand it a period of `frames`; its error when it is gone.
        fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String>;
    }

    /// Everything the audio thread hands the device between the stream's
    /// start and its stop: a period each time the device asks, until the
    /// stop flag — then the close (`close`) — (Ok) or the device fails (Err,
    /// in the words it is told with).
    pub(super) fn run(
        dev: &mut impl Device,
        periods: &mut Periods,
        timeline: &Timeline,
        shared: &OutputShared,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        loop {
            if stop.load(Ordering::Relaxed) {
                close(dev, periods, timeline, shared);
                return Ok(());
            }
            // Not asked within the wait: the stop flag is looked at again.
            if !dev.wait() {
                continue;
            }
            let avail = dev.space().map_err(|e| format!("get_available_space_in_frames: {e}"))?;
            if avail == 0 {
                continue;
            }
            let (bytes, what) = periods.next(avail, timeline, shared);
            dev.write(avail, bytes).map_err(|e| format!("write_to_device{what}: {e}"))?;
        }
    }

    /// The stream's close, the stop flag up: the music faded out, then
    /// silence until the device has been handed `close_tail` of it in a row
    /// — all it holds in flight is silence by then. It stopped right after
    /// the last period of music, and a USB DAC's driver went on playing the
    /// piece it held, round and round, until a new stream: the squeak after
    /// Stop (Anton 16.10; Mojo 2 at ×4 holds 46–90 ms). Already silent
    /// (paused, held, a test run) it is over at once. A device that stops
    /// asking does not hold the stop up past the tail's time and a quarter
    /// second; a device gone meanwhile ends it — nothing is left to hear.
    fn close(dev: &mut impl Device, periods: &mut Periods, timeline: &Timeline, shared: &OutputShared) {
        let tail = periods.close_tail(shared.device_latency_frames.load(Ordering::Relaxed));
        let until = std::time::Instant::now()
            + std::time::Duration::from_secs_f64(tail as f64 / periods.rate.max(1) as f64)
            + std::time::Duration::from_millis(250);
        while periods.silent_run < tail && std::time::Instant::now() < until {
            if !dev.wait() {
                continue;
            }
            let avail = match dev.space() {
                Ok(n) => n,
                Err(_) => return,
            };
            if avail == 0 {
                continue;
            }
            let bytes = periods.closing(avail, timeline, shared);
            if dev.write(avail, bytes).is_err() {
                return;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The packers' formats with their frame sizes in bytes.
        const FORMATS: [(SampleFmt, usize); 4] =
            [(SampleFmt::Int16, 4), (SampleFmt::Int24Packed, 6), (SampleFmt::Int32, 8), (SampleFmt::Float32, 8)];

        /// `frames` of a full-scale square wave, each side its own: what a
        /// loud stream gives in BIT-PERFECT, where no volume brings it down.
        fn loud(frames: usize) -> (Vec<f64>, Vec<f64>) {
            let l: Vec<f64> = (0..frames).map(|i| if (i / 37) % 2 == 0 { 32_767.0 / 32_768.0 } else { -1.0 }).collect();
            let r: Vec<f64> = (0..frames).map(|i| if (i / 53) % 2 == 0 { -1.0 } else { 32_767.0 / 32_768.0 }).collect();
            (l, r)
        }

        fn timeline(rate: u32, l: &[f64], r: &[f64]) -> Timeline {
            let tl = Timeline::new(rate, 4.0);
            assert_eq!(tl.append(l, r), l.len());
            tl
        }

        /// DSP mode quantizes through the converter's `DitherState`: above
        /// 48 kHz it shapes the noise out of 0..20 kHz (unless `AURA_NS_HIGH=0`),
        /// at 44.1/48 kHz it keeps the old shaper; Direct output has no dither.
        #[test]
        fn dsp_periods_shape_the_noise_above_48k() {
            let on = std::env::var("AURA_NS_HIGH").map_or(true, |v| v.trim() != "0");
            let dither = |rate: u32, mode: &OutputMode| {
                let p = Periods::new(rate, SampleFmt::Int32, 8, 64, mode, true).unwrap();
                p.dither.as_ref().map(|d| d.shapes_high())
            };
            for rate in [88_200, 96_000, 352_800, 705_600, 768_000, 1_411_200, 1_536_000] {
                assert_eq!(dither(rate, &OutputMode::Dsp), Some(on), "rate {rate}");
            }
            for rate in [44_100, 48_000] {
                assert_eq!(dither(rate, &OutputMode::Dsp), Some(false), "rate {rate}");
            }
            assert_eq!(dither(705_600, &OutputMode::Direct { source_bits: 24 }), None);
            // A 16-bit device: the dither lands on its grid (packing is exact);
            // a 24-bit one keeps the finer grid.
            for (fmt, ba, step) in [(SampleFmt::Int16, 4, 32_768.0), (SampleFmt::Int24Packed, 6, 8_388_608.0)] {
                for rate in [44_100, 705_600] {
                    let mut p = Periods::new(rate, fmt, ba, 64, &OutputMode::Dsp, true).unwrap();
                    let mut l: Vec<f64> = (0..4096).map(|i| 0.3 * (i as f64 * 0.01).sin() + 1e-7).collect();
                    let mut r = l.clone();
                    p.dither.as_mut().unwrap().process(&mut l, &mut r, 0.7);
                    assert!(l.iter().all(|v| (v * step).fract() == 0.0), "{fmt:?} at {rate}: off its grid");
                    if fmt == SampleFmt::Int24Packed {
                        assert!(l.iter().any(|v| (v * 32_768.0).fract() != 0.0), "24-bit output must not sit on the 16-bit grid");
                    }
                }
            }
        }

        /// The exclusive period is the device's default, 10 ms at least and
        /// 50 ms at most — never its minimum: an RME set for a small ASIO
        /// latency (minimum 1.33 ms) crackled at 1.5 × it (ASR #142).
        #[test]
        fn exclusive_period_is_the_default_ten_ms_at_least() {
            const MS: i64 = 10_000;
            // RME: default 10 ms, minimum 1.33 ms.
            assert_eq!(exclusive_period_hns(10 * MS, 13_333), 10 * MS);
            // A default shorter than 10 ms: 10 ms.
            assert_eq!(exclusive_period_hns(3 * MS, 2 * MS), 10 * MS);
            // A longer default is taken as it is.
            assert_eq!(exclusive_period_hns(20 * MS, 3 * MS), 20 * MS);
            // 50 ms at most: the timeline's rewrite guard (250 ms) stays far
            // past what the device reads in one go.
            assert_eq!(exclusive_period_hns(100 * MS, 3 * MS), 50 * MS);
            assert!(50 * MS * 5 <= crate::player::timeline::GUARD_FRAMES_MS as i64 * MS);
            // Never under the device's minimum.
            assert_eq!(exclusive_period_hns(10 * MS, 60 * MS), 60 * MS);
        }

        /// Every way a period is written, in turn — playing, the hold's fade,
        /// the hold's silence, the jump that ends it, the pause's fade and its
        /// silence, playing on, a BIT-PERFECT switch's long fade and the hold
        /// after it: each period's name, the words of its write error and its
        /// bytes.
        fn every_path(p: &mut Periods, tl: &Timeline, sh: &OutputShared, avail: usize) -> Vec<(&'static str, &'static str, Vec<u8>)> {
            let mut out = Vec::new();
            let mut period = |p: &mut Periods, name: &'static str| {
                let (b, what) = p.next(avail, tl, sh);
                out.push((name, what, b.to_vec()));
            };
            period(p, "playing");
            sh.hold.store(true, Ordering::Release);
            period(p, "hold: fade");
            period(p, "hold: silence");
            sh.jump_frame.store(tl.read_pos() + 17, Ordering::Release);
            period(p, "hold: the jump, playing again");
            period(p, "playing");
            sh.paused.store(true, Ordering::Relaxed);
            period(p, "pause: fade");
            period(p, "pause: silence");
            sh.paused.store(false, Ordering::Relaxed);
            period(p, "resumed");
            sh.hold_ramp_frames.store(3 * avail as u64 - 5, Ordering::Release);
            sh.hold.store(true, Ordering::Release);
            for _ in 0..3 {
                period(p, "a BIT-PERFECT switch's long fade");
            }
            period(p, "hold: silence");
            out
        }

        /// A test run sends the device zeros and nothing else — in Direct
        /// mode too, where no volume keeps a loud stream down — on every way
        /// a period is written: playing, the hold's fade and silence, the
        /// jump, the pause's fade and silence, a long fade; in every format
        /// the device may take. Measured before the mute it is loud (the
        /// meters see it); what is sent stays nothing at all.
        #[test]
        fn a_test_run_sends_the_device_only_zeros_in_every_path_and_format() {
            let (rate, avail) = (44_100, 441);
            let (l, r) = loud(20 * avail);
            for mode in [OutputMode::Direct { source_bits: 16 }, OutputMode::Direct { source_bits: 24 }, OutputMode::Dsp] {
                for (fmt, ba) in FORMATS {
                    let tl = timeline(rate, &l, &r);
                    let sh = OutputShared::new();
                    // The tests' volume: it brings DSP mode down, not Direct.
                    sh.set_volume_db(-60.0);
                    let mut p = Periods::new(rate, fmt, ba, avail, &mode, true).unwrap();
                    let periods = every_path(&mut p, &tl, &sh, avail);
                    for (name, _, bytes) in &periods {
                        assert_eq!(bytes.len(), avail * ba, "{mode:?} {fmt:?} {name}");
                        assert!(bytes.iter().all(|&b| b == 0), "{mode:?} {fmt:?} {name}: a non-zero byte went to the device");
                    }
                    assert_eq!(sh.sent_peak_max_db(), None, "{mode:?} {fmt:?}: only silence was sent");
                    if matches!(mode, OutputMode::Direct { .. }) {
                        // Measured before the mute: the stream at full scale.
                        let dev = sh.dev_peak_max_db().expect("measured");
                        assert!(dev > -0.01, "{fmt:?}: {dev:.2} dBFS before the mute");
                    }
                    assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 0, "{mode:?} {fmt:?}");
                }
            }
        }

        /// Without the mute the same periods reach the device as packed: in
        /// Direct mode a 16-bit stream's samples are the integers it carries,
        /// to the bit, and what was sent is measured at their level — the
        /// test above sees a real signal go silent, not nothing. The silent
        /// periods (held, paused) leave the reader where it was, and are said
        /// with their own words; the jump moves it to its frame.
        #[test]
        fn without_the_mute_direct_sends_the_streams_integers_and_held_or_paused_reads_nothing() {
            let (rate, avail) = (44_100, 441);
            let (l, r) = loud(20 * avail);
            let tl = timeline(rate, &l, &r);
            let sh = OutputShared::new();
            let mut p = Periods::new(rate, SampleFmt::Int16, 4, avail, &OutputMode::Direct { source_bits: 16 }, false).unwrap();
            let (bytes, what) = p.next(avail, &tl, &sh);
            assert_eq!(what, "");
            for i in 0..avail {
                let (a, b) = (i16::from_le_bytes([bytes[4 * i], bytes[4 * i + 1]]), i16::from_le_bytes([bytes[4 * i + 2], bytes[4 * i + 3]]));
                assert_eq!((a as f64 / 32_768.0, b as f64 / 32_768.0), (l[i], r[i]), "frame {i}");
            }
            assert!(sh.sent_peak_max_db().is_some_and(|db| db > -0.01), "{:?}", sh.sent_peak_max_db());
            // Held: one fade, then silence that reads nothing.
            sh.hold.store(true, Ordering::Release);
            let (_, what) = p.next(avail, &tl, &sh);
            assert_eq!(what, "");
            let at = tl.read_pos();
            let (bytes, what) = p.next(avail, &tl, &sh);
            assert_eq!((what, tl.read_pos()), (" (hold)", at));
            assert!(bytes.iter().all(|&b| b == 0));
            // The jump: from its frame on, ramping up.
            sh.jump_frame.store(5 * avail as u64, Ordering::Release);
            p.next(avail, &tl, &sh);
            assert_eq!(tl.read_pos(), 6 * avail as u64);
            assert!(!sh.hold.load(Ordering::Acquire));
            // Paused: one fade, then silence that reads nothing.
            sh.paused.store(true, Ordering::Relaxed);
            p.next(avail, &tl, &sh);
            let at = tl.read_pos();
            let (bytes, what) = p.next(avail, &tl, &sh);
            assert_eq!((what, tl.read_pos()), (" (pause)", at));
            assert!(bytes.iter().all(|&b| b == 0));
        }

        /// A DAC behind its driver, as a stop leaves it: what it was handed
        /// last is still in flight when the stream stops (a Chord Mojo 2 at
        /// ×4 holds 46–90 ms), and a USB driver may go on playing that piece
        /// round and round — the squeak after Stop (Anton 16.10).
        struct Dac {
            period: usize,
            ba: usize,
            /// Every byte handed to it.
            sent: Vec<u8>,
            /// The listener's Stop: the stop flag goes up after this many periods.
            stop_after: usize,
            stop: Arc<AtomicBool>,
            /// It stops asking for periods once the stop flag is up.
            mute_after_stop: bool,
            writes: usize,
        }

        impl Dac {
            fn new(period: usize, ba: usize, stop_after: usize) -> Dac {
                Dac { period, ba, sent: Vec::new(), stop_after, stop: Arc::new(AtomicBool::new(false)), mute_after_stop: false, writes: 0 }
            }

            /// The peak of each frame handed to it.
            fn frames(&self, fmt: SampleFmt) -> Vec<f64> {
                self.sent.chunks(self.ba).map(|f| unpack_peak(f, 1, fmt).0).collect()
            }
        }

        impl Device for Dac {
            fn wait(&mut self) -> bool {
                if self.mute_after_stop && self.stop.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    return false;
                }
                true
            }

            fn space(&mut self) -> Result<usize, String> {
                Ok(self.period)
            }

            fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
                assert_eq!(bytes.len(), frames * self.ba);
                self.sent.extend_from_slice(bytes);
                self.writes += 1;
                if self.writes == self.stop_after {
                    self.stop.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
        }

        /// A sine at half scale, `frames` long.
        fn music(rate: u32, frames: usize) -> (Vec<f64>, Vec<f64>) {
            let l: Vec<f64> = (0..frames).map(|i| 0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin()).collect();
            let r = l.iter().map(|x| -x).collect();
            (l, r)
        }

        /// #1 (T49). Stopped in the middle of the music, in every mode and
        /// format: the music fades out, and the device is then handed
        /// silence until everything it holds in flight is silence — longer
        /// than its measured latency — before the stream stops. It stopped
        /// right after the last period of music: the DAC was left holding
        /// it, and the driver looped it.
        #[test]
        fn stopped_mid_music_the_device_is_left_holding_only_silence_after_a_fade() {
            let (rate, avail) = (44_100, 441);
            let in_flight = 4 * avail; // 40 ms
            let (l, r) = music(rate, 40 * avail);
            for mode in [OutputMode::Dsp, OutputMode::Direct { source_bits: 24 }] {
                for (fmt, ba) in FORMATS {
                    let tl = timeline(rate, &l, &r);
                    let sh = OutputShared::new();
                    sh.set_volume_db(-20.0);
                    sh.device_latency_frames.store(in_flight as u64, Ordering::Relaxed);
                    let mut p = Periods::new(rate, fmt, ba, avail, &mode, false).unwrap();
                    let mut dac = Dac::new(avail, ba, 10);
                    let stop = dac.stop.clone();
                    assert_eq!(run(&mut dac, &mut p, &tl, &sh, &stop), Ok(()), "{mode:?} {fmt:?}");
                    let f = dac.frames(fmt);
                    let level = f[..10 * avail].iter().cloned().fold(0.0, f64::max);
                    assert!(level > 0.04, "{mode:?} {fmt:?}: the music went out ({level})");
                    // What the device holds when it stops: silence, every bit.
                    assert!(f.len() >= 10 * avail + in_flight, "{mode:?} {fmt:?}: {} frames, nothing after the stop", f.len());
                    let held = &dac.sent[dac.sent.len() - (in_flight + 2 * avail) * ba..];
                    assert!(held.iter().all(|&b| b == 0), "{mode:?} {fmt:?}: the device was left holding sound");
                    // And the music did not end mid-waveform: a fade, its
                    // last 20 frames near silence (a fifth of the sine's
                    // cycle: cut off, they hold a third of its level at least).
                    let last = f.iter().rposition(|&x| x > 1e-6).unwrap();
                    let tail = f[last + 1 - 20..=last].iter().cloned().fold(0.0, f64::max);
                    assert!(tail < 0.06 * level, "{mode:?} {fmt:?}: cut at {:.1} dB under the music", 20.0 * (tail / level).log10());
                }
            }
        }

        /// #2 (T49). Already silent when the stop comes — paused, or a test
        /// run sending nothing but zeros: the device holds nothing to hear,
        /// and the stream stops at once.
        #[test]
        fn already_silent_the_stream_stops_at_once() {
            let (rate, avail) = (44_100, 441);
            let (l, r) = music(rate, 40 * avail);
            for (paused, mute) in [(true, false), (false, true)] {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                sh.device_latency_frames.store(4 * avail as u64, Ordering::Relaxed);
                sh.paused.store(paused, Ordering::Relaxed);
                let mut p = Periods::new(rate, SampleFmt::Int32, 8, avail, &OutputMode::Dsp, mute).unwrap();
                // Paused at its first period: one fade, then silence.
                let mut dac = Dac::new(avail, 8, 10);
                let stop = dac.stop.clone();
                run(&mut dac, &mut p, &tl, &sh, &stop).unwrap();
                assert_eq!(dac.writes, 10, "paused {paused}, test mute {mute}: nothing more after the stop");
            }
        }

        /// #3 (T49). A device with a long way to go (a second in flight):
        /// the silence after the fade is held to 300 ms; and one that stops
        /// asking for periods does not hold the stop up past its bound.
        #[test]
        fn the_silence_before_the_stop_is_bounded() {
            let (rate, avail) = (44_100, 441);
            let (l, r) = music(rate, 100 * avail);
            let tl = timeline(rate, &l, &r);
            let sh = OutputShared::new();
            sh.device_latency_frames.store(rate as u64, Ordering::Relaxed);
            let mut p = Periods::new(rate, SampleFmt::Int32, 8, avail, &OutputMode::Dsp, false).unwrap();
            let mut dac = Dac::new(avail, 8, 10);
            let stop = dac.stop.clone();
            run(&mut dac, &mut p, &tl, &sh, &stop).unwrap();
            let f = dac.frames(SampleFmt::Int32);
            let zeros = f.len() - 1 - f.iter().rposition(|&x| x > 0.0).unwrap();
            let max = (0.3 * rate as f64) as usize;
            assert!(zeros >= max && zeros < max + avail, "{zeros} frames of silence, {max} the bound");

            let tl = timeline(rate, &l, &r);
            let mut p = Periods::new(rate, SampleFmt::Int32, 8, avail, &OutputMode::Dsp, false).unwrap();
            let mut dac = Dac::new(avail, 8, 10);
            dac.mute_after_stop = true;
            let stop = dac.stop.clone();
            let t0 = std::time::Instant::now();
            run(&mut dac, &mut p, &tl, &sh, &stop).unwrap();
            assert!(t0.elapsed() < std::time::Duration::from_secs(1), "{:?}", t0.elapsed());
        }

        // ── DSD output (DoP) ─────────────────────────────────────────────────

        use crate::audio::dsd::DSD_SILENCE;

        /// DoP's containers, with the packer's format and frame size.
        const DOP_FORMATS: [(SampleFmt, Container, usize); 2] =
            [(SampleFmt::Int24Packed, Container::Int24, 6), (SampleFmt::Int32, Container::Int32, 8)];

        /// DSD64 over DoP: the stream at its 176.4 kHz carrier, 10 ms periods.
        const DOP_RATE: u32 = 176_400;
        const DOP_AVAIL: usize = 1_764;

        fn dsd64() -> OutputMode {
            OutputMode::Dsd { rate: DsdRate::DSD64, modulator: ModulatorKind::Sdm5 }
        }

        /// What the device was handed, frame by frame, as a DAC reads DoP.
        fn dop_frames(sent: &[u8], c: Container) -> Vec<(u8, [u8; 2], [u8; 2])> {
            dop::unpack(sent, c).expect("whole DoP frames, the low byte zero, one marker a frame")
        }

        fn is_silence(f: &(u8, [u8; 2], [u8; 2])) -> bool {
            f.1 == [DSD_SILENCE; 2] && f.2 == [DSD_SILENCE; 2]
        }

        /// One modulator fed `pcm` from silence, in one go: what the stream's
        /// payload must be, bit for bit.
        fn reference(pcm: &[f64]) -> Vec<u8> {
            let mut enc = new_encoder(ModulatorKind::Sdm5, DOP_RATE, DsdRate::DSD64).unwrap();
            let mut out = vec![0u8; pcm.len() * 2];
            enc.process(pcm, &mut out);
            out
        }

        /// A DoP stream from its start to its stop, in both containers: first
        /// 100 ms of DSD silence (nothing read), then the music — its payload
        /// exactly the modulators' bytes for the PCM at the listener's
        /// volume, digital silence inside it going through them like any
        /// other samples — then, after the stop, the fade and DSD silence
        /// until all the device holds is silence. The markers alternate over
        /// the whole stream, from the prefill on; no zero word ever.
        #[test]
        fn a_dop_stream_carries_the_modulators_bytes_between_dsd_silence() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let in_flight = 4 * avail;
            let (mut l, mut r) = music(rate, 40 * avail);
            for i in 5 * avail..8 * avail {
                (l[i], r[i]) = (0.0, 0.0);
            }
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                sh.set_volume_db(-6.0);
                sh.device_latency_frames.store(in_flight as u64, Ordering::Relaxed);
                let mut p = Periods::new(rate, fmt, ba, avail, &dsd64(), false).unwrap();
                let mut dac = Dac::new(avail, ba, 30);
                dac.sent = p.prefill(avail);
                let stop = dac.stop.clone();
                assert_eq!(run(&mut dac, &mut p, &tl, &sh, &stop), Ok(()), "{c:?}");
                let f = dop_frames(&dac.sent, c);
                assert!(dop::markers_alternate(&f, Some(0x05)), "{c:?}: the markers broke");
                // The prefill and the lead-in: DSD silence, 100 ms at least.
                let lead = (DOP_LEAD_IN_S * rate as f64).ceil() as usize;
                let start = avail + lead;
                assert!(f[..start].iter().all(is_silence), "{c:?}: the lead-in");
                // Then the music, read from the timeline's start.
                let vol = sh.volume_lin();
                let n = 20 * avail;
                let (pl, pr): (Vec<f64>, Vec<f64>) = (l[..n].iter().map(|x| x * vol).collect(), r[..n].iter().map(|x| x * vol).collect());
                let got_l: Vec<u8> = f[start..start + n].iter().flat_map(|x| x.1).collect();
                let got_r: Vec<u8> = f[start..start + n].iter().flat_map(|x| x.2).collect();
                assert!(got_l == reference(&pl), "{c:?}: the left payload is not the modulator's");
                assert!(got_r == reference(&pr), "{c:?}: the right payload is not the modulator's");
                // After the stop: the fade, then DSD silence the device is left holding.
                assert!(f.len() >= start + n + in_flight, "{c:?}: {} frames", f.len());
                assert!(f[f.len() - in_flight - 2 * avail..].iter().all(is_silence), "{c:?}: the tail");
                assert!(sh.sent_peak_max_db().is_some(), "{c:?}: the music was sent");
                assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 0);
                assert_eq!(p.dop_overloads(), Some(0));
            }
        }

        /// A test run of DoP sends DSD silence with its markers on every way a
        /// period is written — never a zero word, which would take the DAC out
        /// of DSD — and nothing else; the meters still see the music. Without
        /// the mute, the hold's and the pause's silences are DSD silence too,
        /// and the markers run on through them.
        #[test]
        fn dop_silences_and_the_test_mute_send_dsd_silence_with_markers() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (l, r) = music(rate, 60 * avail);
            for mute in [true, false] {
                for (fmt, c, ba) in DOP_FORMATS {
                    let tl = timeline(rate, &l, &r);
                    let sh = OutputShared::new();
                    let mut p = Periods::new(rate, fmt, ba, avail, &dsd64(), mute).unwrap();
                    let mut all = Vec::new();
                    for _ in 0..10 {
                        all.extend_from_slice(p.next(avail, &tl, &sh).0); // the lead-in
                    }
                    let periods = every_path(&mut p, &tl, &sh, avail);
                    for (name, what, bytes) in &periods {
                        let f = dop_frames(bytes, c);
                        let silent = f.iter().all(is_silence);
                        if mute {
                            assert!(silent, "{c:?} {name}: the test mute let something through");
                        } else if !what.is_empty() {
                            assert!(silent, "{c:?} {name}: not DSD silence");
                        } else if *name == "playing" {
                            assert!(!silent, "{c:?} {name}: no music");
                        }
                        all.extend_from_slice(bytes);
                    }
                    // Still held: the fade's rounding may take a period more,
                    // then the hold's silence — DSD silence, markers running on.
                    let mut held = 0;
                    for _ in 0..3 {
                        let (bytes, what) = p.next(avail, &tl, &sh);
                        if what == " (hold)" {
                            held += 1;
                            assert!(dop_frames(bytes, c).iter().all(is_silence), "{c:?}: the hold's silence");
                        }
                        all.extend_from_slice(bytes);
                    }
                    assert!(held >= 1, "{c:?}: never held in silence");
                    assert!(dop::markers_alternate(&dop_frames(&all, c), Some(0x05)), "mute {mute} {c:?}");
                    assert_eq!(sh.sent_peak_max_db().is_none(), mute, "mute {mute} {c:?}");
                    assert!(sh.dev_peak_max_db().is_some(), "measured before the mute");
                    assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 0);
                }
            }
        }

        /// The guard judges the PCM before the modulators: a period far over
        /// the volume, or a NaN, goes out as DSD silence and is counted, and
        /// never reaches them — the music after it is modulated from silence
        /// again, exactly as a fresh modulator would.
        #[test]
        fn the_dop_guard_sends_dsd_silence_and_keeps_bad_samples_from_the_modulators() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (mut l, mut r) = music(rate, 30 * avail);
            for i in 0..avail {
                l[i] = 5.0;
                r[i] = if i == 3 { f64::NAN } else { 0.0 };
            }
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                let mut p = Periods::new(rate, fmt, ba, avail, &dsd64(), false).unwrap();
                for _ in 0..10 {
                    p.next(avail, &tl, &sh); // the lead-in
                }
                let bad = dop_frames(p.next(avail, &tl, &sh).0, c);
                assert!(bad.iter().all(is_silence), "{c:?}: the guard let it through");
                assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 1);
                let good = dop_frames(p.next(avail, &tl, &sh).0, c);
                let got: Vec<u8> = good.iter().flat_map(|x| x.2).collect();
                assert!(got == reference(&r[avail..2 * avail]), "{c:?}: the right channel after the guard");
                assert_eq!(p.dop_overloads(), Some(0));
            }
        }

        /// DoP needs a 24-bit integer word: a 16-bit or float device format is
        /// refused with the error the controller falls back to PCM on; a rate
        /// that is not the DSD rate's carrier is refused too.
        #[test]
        fn dop_refuses_16_bit_and_float_and_a_wrong_carrier() {
            for (fmt, ba) in [(SampleFmt::Int16, 4), (SampleFmt::Float32, 8)] {
                let e = Periods::new(DOP_RATE, fmt, ba, DOP_AVAIL, &dsd64(), false).err().expect("refused");
                assert!(super::super::is_dop_format_error(&e), "{e}");
            }
            let e = Periods::new(2 * DOP_RATE, SampleFmt::Int32, 8, DOP_AVAIL, &dsd64(), false).err().expect("refused");
            assert!(!super::super::is_dop_format_error(&e), "{e}");
            assert!(Periods::new(DOP_RATE, SampleFmt::Int32, 8, DOP_AVAIL, &dsd64(), false).is_ok());
        }

        /// Stopped within DoP's lead-in: nothing but DSD silence ever went
        /// out, markers alternating, and the stream stops at once.
        #[test]
        fn stopped_in_the_dop_lead_in_only_dsd_silence_goes_out() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (l, r) = music(rate, 40 * avail);
            let tl = timeline(rate, &l, &r);
            let sh = OutputShared::new();
            sh.device_latency_frames.store(4 * avail as u64, Ordering::Relaxed);
            let mut p = Periods::new(rate, SampleFmt::Int32, 8, avail, &dsd64(), false).unwrap();
            let mut dac = Dac::new(avail, 8, 3);
            let stop = dac.stop.clone();
            run(&mut dac, &mut p, &tl, &sh, &stop).unwrap();
            let f = dop_frames(&dac.sent, Container::Int32);
            assert!(dop::markers_alternate(&f, Some(0x05)));
            assert!(f.iter().all(is_silence), "music went out in the lead-in");
            assert!(dac.writes <= 7, "{} periods", dac.writes);
            assert_eq!(sh.sent_peak_max_db(), None);
        }

        /// The modulators run `lead` frames ahead (here 150 ms, longer than
        /// the lead-in): the music starts after the lead of DSD silence, not
        /// after the lead-in and the lead; its bytes are the modulator's; on
        /// a pause and on a stop what the pipe holds goes out first — every
        /// frame read is heard, the fade too — then DSD silence; the delay is
        /// said for the position (`dsd_delay`).
        #[test]
        fn the_modulators_ahead_start_after_their_lead_and_drain_on_pause_and_stop() {
            use crate::player::dsd_worker::TEST_PIPE;
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let lead = 15 * avail;
            let in_flight = 4 * avail;
            let (l, r) = music(rate, 60 * avail);
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                sh.device_latency_frames.store(in_flight as u64, Ordering::Relaxed);
                let old = TEST_PIPE.with(|t| t.replace((Some(lead), true)));
                let mut p = Periods::new(rate, fmt, ba, avail, &dsd64(), false).unwrap();
                TEST_PIPE.with(|t| t.set(old));
                assert_eq!(p.dsd_delay(), lead);
                assert_eq!(p.close_tail(in_flight as u64), in_flight as u64 + 2 * avail as u64 + lead as u64);
                let silent = |b: &[u8]| dop_frames(b, c).iter().all(is_silence);
                // Playing, then a pause: the music pushed before it still
                // goes out for the lead, the fade last, then silence.
                let mut sent = Vec::new();
                for _ in 0..30 {
                    sent.extend_from_slice(p.next(avail, &tl, &sh).0);
                }
                sh.paused.store(true, Ordering::Relaxed);
                let read_at_pause = {
                    let mut v = Vec::new();
                    for _ in 0..20 {
                        let (b, _) = p.next(avail, &tl, &sh);
                        v.push(b.to_vec());
                    }
                    v
                };
                let read = tl.read_pos() as usize;
                // The fade's rounding may take a period more.
                assert!(read == 31 * avail || read == 32 * avail, "{c:?}: 30 periods and the pause's fade, {read} frames");
                for b in &read_at_pause {
                    sent.extend_from_slice(b);
                }
                let f = dop_frames(&sent, c);
                assert!(f[..lead].iter().all(is_silence), "{c:?}: the lead");
                assert!(!is_silence(&f[lead]), "{c:?}: the music starts right after the lead");
                let last = f.iter().rposition(|x| !is_silence(x)).unwrap();
                assert_eq!(last + 1, lead + read, "{c:?}: every frame read went out, the fade last");
                let vol = sh.volume_lin();
                let n = 20 * avail;
                let pl: Vec<f64> = l[..n].iter().map(|x| x * vol).collect();
                let got_l: Vec<u8> = f[lead..lead + n].iter().flat_map(|x| x.1).collect();
                assert!(got_l == reference(&pl), "{c:?}: the payload is the modulator's");
                assert!(read_at_pause[read_at_pause.len() - 3..].iter().all(|b| silent(b)), "{c:?}: silence after the drain");
                // Resumed and stopped: the fade and the music ahead of it go
                // out before the stop's silence.
                sh.paused.store(false, Ordering::Relaxed);
                let mut dac = Dac::new(avail, ba, 10);
                let stop = dac.stop.clone();
                assert_eq!(run(&mut dac, &mut p, &tl, &sh, &stop), Ok(()), "{c:?}");
                let g = dop_frames(&dac.sent, c);
                let read2 = tl.read_pos() as usize - read;
                let first = g.iter().position(|x| !is_silence(x)).unwrap();
                let last = g.iter().rposition(|x| !is_silence(x)).unwrap();
                assert_eq!(last + 1 - first, read2, "{c:?}: every frame read after the pause went out");
                assert!(g[g.len() - in_flight - 2 * avail..].iter().all(is_silence), "{c:?}: the tail");
                assert!(dop::markers_alternate(&dop_frames(&[sent, dac.sent].concat(), c), Some(0x05)), "{c:?}: the markers broke");
            }
        }

        // ── A DSD file as it is (DoP words) ──────────────────────────────────

        fn as_is64() -> OutputMode {
            OutputMode::DsdDirect { rate: DsdRate::DSD64 }
        }

        /// A quiet DSD64 tone's bytes, `frames` DoP frames per channel (the
        /// right channel in antiphase), and the timeline's words for them.
        fn dsd_tone(frames: usize) -> ((Vec<u8>, Vec<u8>), (Vec<f64>, Vec<f64>)) {
            use crate::audio::dsd::decimate::test_signal::modulate;
            let fs = DsdRate::DSD64.bit_rate() as f64;
            let tone = |s: f64| modulate(move |t| s * 0.01 * (2.0 * std::f64::consts::PI * 1_234.5 * t).sin(), fs, 16 * frames);
            let (bl, br) = (tone(1.0), tone(-1.0));
            let words = |b: &[u8]| b.chunks(2).map(|p| dsd_direct::word(p[0], p[1])).collect::<Vec<f64>>();
            let (wl, wr) = (words(&bl), words(&br));
            ((bl, br), (wl, wr))
        }

        /// Each period of DoP frames: DSD silence, or the file's bytes as
        /// they are from some frame on — never anything in between (a scaled
        /// or faded word). The frame it starts at, None for silence.
        fn as_is_period(f: &[(u8, [u8; 2], [u8; 2])], src: &(Vec<u8>, Vec<u8>)) -> Option<usize> {
            if f.iter().all(is_silence) {
                return None;
            }
            let l: Vec<u8> = f.iter().flat_map(|x| x.1).collect();
            let r: Vec<u8> = f.iter().flat_map(|x| x.2).collect();
            let at = (0..src.0.len() / 2)
                .find(|&k| src.0[2 * k..].starts_with(&l[..16.min(l.len())]) && src.1[2 * k..].starts_with(&r[..16.min(r.len())]))
                .expect("a period that is neither DSD silence nor the file's bytes");
            assert!(src.0[2 * at..].starts_with(&l) && src.1[2 * at..].starts_with(&r), "a period with altered bytes");
            Some(at)
        }

        /// A DSD file as it is, from the stream's start to its stop, in both
        /// containers: the lead-in's DSD silence, then the file's bytes
        /// exactly — the volume set low is not applied — from its first
        /// frame on, and after the stop DSD silence at once, no fade; the
        /// markers alternate over the whole stream. No PCM level is
        /// claimed for it, and the guard passes it.
        #[test]
        fn a_dsd_file_as_it_is_goes_out_bit_exact_between_dsd_silence() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let in_flight = 4 * avail;
            let (src, (l, r)) = dsd_tone(40 * avail);
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                sh.set_volume_db(-40.0);
                sh.device_latency_frames.store(in_flight as u64, Ordering::Relaxed);
                let mut p = Periods::new(rate, fmt, ba, avail, &as_is64(), false).unwrap();
                let mut dac = Dac::new(avail, ba, 30);
                dac.sent = p.prefill(avail);
                let stop = dac.stop.clone();
                assert_eq!(run(&mut dac, &mut p, &tl, &sh, &stop), Ok(()), "{c:?}");
                let f = dop_frames(&dac.sent, c);
                assert!(dop::markers_alternate(&f, Some(0x05)), "{c:?}: the markers broke");
                let lead = (DOP_LEAD_IN_S * rate as f64).ceil() as usize;
                let start = avail + lead.div_ceil(avail) * avail;
                assert!(f[..start].iter().all(is_silence), "{c:?}: the lead-in");
                let played = 31 * avail - start; // 30 periods written: 10 of lead-in, 20 of the file
                let got_l: Vec<u8> = f[start..start + played].iter().flat_map(|x| x.1).collect();
                let got_r: Vec<u8> = f[start..start + played].iter().flat_map(|x| x.2).collect();
                assert!(got_l == src.0[..2 * played] && got_r == src.1[..2 * played], "{c:?}: the bytes are not the file's");
                assert!(f[start + played..].iter().all(is_silence), "{c:?}: no DSD silence right after the stop");
                assert!(f.len() >= start + played + in_flight, "{c:?}: {} frames", f.len());
                assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 0);
                assert!(!sh.dsd_guard_tripped.load(Ordering::Relaxed));
                assert_eq!((sh.dev_peak_max_db(), sh.out_level_db()), (None, None), "no PCM level for DoP words");
            }
        }

        /// Every way a period is written, for a DSD file as it is: each
        /// period is DSD silence or the file's bytes untouched — the hold's
        /// and the pause's "fades" are DSD silence, the jump lands on the
        /// file's bytes at its frame whole, no ramp — and the markers run on
        /// through all of it. A test run sends DSD silence only.
        #[test]
        fn a_dsd_file_as_it_is_holds_pauses_and_jumps_on_dsd_silence() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (src, (l, r)) = dsd_tone(60 * avail);
            for mute in [false, true] {
                for (fmt, c, ba) in DOP_FORMATS {
                    let tl = timeline(rate, &l, &r);
                    let sh = OutputShared::new();
                    let mut p = Periods::new(rate, fmt, ba, avail, &as_is64(), mute).unwrap();
                    let mut all = Vec::new();
                    for _ in 0..10 {
                        all.extend_from_slice(p.next(avail, &tl, &sh).0); // the lead-in
                    }
                    let jump = tl.read_pos() + 3 * avail as u64 + 17;
                    let mut periods = Vec::new();
                    let mut period = |p: &mut Periods, name: &'static str| {
                        let (b, _) = p.next(avail, &tl, &sh);
                        periods.push((name, b.to_vec()));
                    };
                    period(&mut p, "playing");
                    sh.hold.store(true, Ordering::Release);
                    period(&mut p, "hold: in place of a fade");
                    period(&mut p, "hold: silence");
                    sh.jump_frame.store(jump, Ordering::Release);
                    period(&mut p, "the jump");
                    sh.paused.store(true, Ordering::Relaxed);
                    period(&mut p, "pause");
                    period(&mut p, "pause: silence");
                    sh.paused.store(false, Ordering::Relaxed);
                    period(&mut p, "resumed");
                    for (name, bytes) in &periods {
                        let f = dop_frames(bytes, c);
                        let at = as_is_period(&f, &src);
                        let want = match (mute, *name) {
                            (true, _) => None,
                            (false, "playing") => Some(0),
                            (false, "the jump") => Some(jump as usize),
                            (false, "resumed") => Some(jump as usize + avail),
                            _ => None,
                        };
                        assert_eq!(at, want, "mute {mute} {c:?} {name}");
                        all.extend_from_slice(bytes);
                    }
                    assert!(dop::markers_alternate(&dop_frames(&all, c), Some(0x05)), "mute {mute} {c:?}");
                    assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 0);
                }
            }
        }

        /// The guard: a stream of a DSD file whose data turns into something
        /// that is not DSD (here stuck at 0xFF) goes out as DSD silence from
        /// that period on — for the rest of the stream, the good data after
        /// it too — counted once, and said (`dsd_guard_tripped`).
        #[test]
        fn the_dsd_guard_silences_the_rest_of_a_stream_that_stops_looking_like_dsd() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (src, (mut l, r)) = dsd_tone(30 * avail);
            for x in &mut l[5 * avail..6 * avail] {
                *x = dsd_direct::word(0xFF, 0xFF);
            }
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                let mut p = Periods::new(rate, fmt, ba, avail, &as_is64(), false).unwrap();
                for _ in 0..10 {
                    p.next(avail, &tl, &sh); // the lead-in
                }
                for k in 0..12 {
                    let f = dop_frames(p.next(avail, &tl, &sh).0, c);
                    let want = (k < 5).then_some(k * avail);
                    assert_eq!(as_is_period(&f, &src), want, "{c:?}: period {k}");
                }
                assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 1);
                assert!(sh.dsd_guard_tripped.load(Ordering::Relaxed));
            }
        }

        /// A tripped guard holds to the end of its track only: where the
        /// next track starts (`dsd_guard_from`, here inside a period) a fresh
        /// guard takes over from that very frame — the new track goes out as
        /// it is, the note is cleared — and a fault in the new track trips it
        /// again.
        #[test]
        fn the_dsd_guard_starts_afresh_where_the_next_track_starts() {
            let (rate, avail) = (DOP_RATE, DOP_AVAIL);
            let (src, (mut l, r)) = dsd_tone(30 * avail);
            for k in [5, 14] {
                for x in &mut l[k * avail..(k + 1) * avail] {
                    *x = dsd_direct::word(0xFF, 0xFF);
                }
            }
            let (next, half) = (8 * avail + avail / 2, avail / 2);
            for (fmt, c, ba) in DOP_FORMATS {
                let tl = timeline(rate, &l, &r);
                let sh = OutputShared::new();
                sh.dsd_guard_from.store(next as u64, Ordering::Release);
                let mut p = Periods::new(rate, fmt, ba, avail, &as_is64(), false).unwrap();
                for _ in 0..10 {
                    p.next(avail, &tl, &sh); // the lead-in
                }
                // The file's bytes from `at` on, as they are (a quiet tone
                // repeats: its place is checked, not searched for).
                let file_from = |f: &[(u8, [u8; 2], [u8; 2])], at: usize| {
                    let l: Vec<u8> = f.iter().flat_map(|x| x.1).collect();
                    let r: Vec<u8> = f.iter().flat_map(|x| x.2).collect();
                    src.0[2 * at..].starts_with(&l) && src.1[2 * at..].starts_with(&r)
                };
                for k in 0..20 {
                    let f = dop_frames(p.next(avail, &tl, &sh).0, c);
                    if k == 8 {
                        assert!(f[..half].iter().all(is_silence), "{c:?}: the old track's end stays silent");
                        assert!(file_from(&f[half..], next), "{c:?}: the next track from its first frame");
                        assert!(!sh.dsd_guard_tripped.load(Ordering::Relaxed), "{c:?}: the note goes with the old track");
                    } else if k < 5 || (9..14).contains(&k) {
                        assert!(file_from(&f, k * avail), "{c:?}: period {k} as it is");
                    } else {
                        assert!(f.iter().all(is_silence), "{c:?}: period {k} silent");
                    }
                }
                assert_eq!(sh.safety_mutes.load(Ordering::Relaxed), 2, "{c:?}: the new track's fault trips it again");
                assert!(sh.dsd_guard_tripped.load(Ordering::Relaxed));
            }
        }

        /// A DSD file as it is needs DoP's 24-bit word like DSD output does:
        /// a 16-bit or float device format is refused with the error the
        /// controller plays its PCM on.
        #[test]
        fn a_dsd_file_as_it_is_refuses_16_bit_and_float() {
            for (fmt, ba) in [(SampleFmt::Int16, 4), (SampleFmt::Float32, 8)] {
                let e = Periods::new(DOP_RATE, fmt, ba, DOP_AVAIL, &as_is64(), false).err().expect("refused");
                assert!(super::super::is_dop_format_error(&e), "{e}");
            }
            let p = Periods::new(DOP_RATE, SampleFmt::Int24Packed, 6, DOP_AVAIL, &as_is64(), false).unwrap();
            assert_eq!(p.dop_overloads(), None, "no modulator");
        }
    }
}
use period::*;

// ── Windows implementation ────────────────────────────────────────────────────

#[cfg(windows)]
mod platform {
    use super::*;
    use std::sync::mpsc::Sender;
    use wasapi::{
        calculate_period_100ns, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
        WasapiError,
    };

    // AUDCLNT error HRESULTs (from Windows SDK audioclient.h / mmreg.h).
    const HRESULT_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x88890019u32 as i32;
    const HRESULT_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;
    const HRESULT_EXCLUSIVE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;

    /// The AUDCLNT codes a render endpoint actually answers with, by name.
    fn hresult_name(code: i32) -> Option<&'static str> {
        Some(match code as u32 {
            0x88890001 => "AUDCLNT_E_NOT_INITIALIZED",
            0x88890002 => "AUDCLNT_E_ALREADY_INITIALIZED",
            0x88890004 => "AUDCLNT_E_DEVICE_INVALIDATED",
            0x88890008 => "AUDCLNT_E_UNSUPPORTED_FORMAT",
            0x8889000A => "AUDCLNT_E_DEVICE_IN_USE",
            0x8889000E => "AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED",
            0x8889000F => "AUDCLNT_E_ENDPOINT_CREATE_FAILED",
            0x88890010 => "AUDCLNT_E_SERVICE_NOT_RUNNING",
            0x88890016 => "AUDCLNT_E_BUFFER_SIZE_ERROR",
            0x88890017 => "AUDCLNT_E_CPUUSAGE_EXCEEDED",
            0x88890019 => "AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED",
            0x88890020 => "AUDCLNT_E_INVALID_DEVICE_PERIOD",
            0x80070005 => "E_ACCESSDENIED",
            0x8007000E => "E_OUTOFMEMORY",
            0x80070057 => "E_INVALIDARG",
            _ => return None,
        })
    }

    /// An error as the endpoint said it: the AUDCLNT name when there is one,
    /// the hex code otherwise, never just the crate's paraphrase.
    fn describe(e: &WasapiError) -> String {
        match wasapi_hresult(e) {
            Some(c) => match hresult_name(c) {
                Some(n) => n.to_string(),
                None => format!("0x{:08X}", c as u32),
            },
            None => e.to_string(),
        }
    }

    // ── Format helpers ───────────────────────────────────────────────────────

    /// Resolve the sample format from raw format fields.
    ///
    /// `subformat_type` is Some for WAVEFORMATEXTENSIBLE (SubFormat is a known
    /// GUID), None when SubFormat is absent or unrecognised (WAVEFORMATEX).
    /// `format_tag` is the WAVEFORMATEX wFormatTag: 1=PCM, 3=IEEE_FLOAT — only
    /// consulted when `subformat_type` is None.
    /// `valid_bits` == 0 means "same as store_bits" (driver convention).
    fn resolve_sample_fmt(
        subformat_type: Option<SampleType>,
        format_tag: u16,
        store_bits: u16,
        valid_bits: u16,
        blockalign: u32,
        channels: u16,
    ) -> Result<SampleFmt, String> {
        // 0 valid bits is driver shorthand for "every stored bit".
        let _valid = if valid_bits == 0 { store_bits } else { valid_bits };
        // The packers write interleaved stereo, store_bits per sample, no padding.
        if channels != 2 || blockalign != channels as u32 * store_bits as u32 / 8 {
            return Err(format!(
                "unsupported device format: {channels} channels, {store_bits} bits, \
                 blockalign={blockalign} (packed stereo expected)"
            ));
        }

        let is_float = match subformat_type {
            Some(SampleType::Float) => true,
            Some(SampleType::Int) => false,
            None => {
                // WAVEFORMATEX: type is in wFormatTag only.
                match format_tag as u32 {
                    1 => false, // WAVE_FORMAT_PCM
                    3 => true,  // WAVE_FORMAT_IEEE_FLOAT
                    _ => return Err(format!(
                        "unsupported device format: unknown wFormatTag={format_tag} \
                         bits={store_bits}"
                    )),
                }
            }
        };

        if is_float {
            return if store_bits == 32 {
                Ok(SampleFmt::Float32)
            } else {
                Err(format!("unsupported device format: float{store_bits}"))
            };
        }

        match store_bits {
            16 => Ok(SampleFmt::Int16),
            24 => {
                let expected_ba = 3 * channels as u32;
                if blockalign == expected_ba {
                    Ok(SampleFmt::Int24Packed)
                } else {
                    Err(format!(
                        "unsupported device format: PCM 24-bit blockalign={blockalign} \
                         for {channels} channels (expected {expected_ba})"
                    ))
                }
            }
            32 => Ok(SampleFmt::Int32),
            _ => Err(format!(
                "unsupported device format: int{store_bits}"
            )),
        }
    }

    impl SampleFmt {
        /// Determine the sample format from a WaveFormat returned by the driver.
        ///
        /// For WAVEFORMATEXTENSIBLE the SubFormat GUID names the type.
        /// For WAVEFORMATEX (returned via the quirks path: SubFormat zeroed,
        /// wValidBitsPerSample = 0) the type is in wFormatTag instead.
        fn from_wave(fmt: &WaveFormat) -> Result<Self, String> {
            resolve_sample_fmt(
                fmt.get_subformat().ok(),
                fmt.as_waveformatex_ref().wFormatTag,
                fmt.get_bitspersample(),
                fmt.get_validbitspersample(),
                fmt.get_blockalign(),
                fmt.get_nchannels(),
            )
        }
    }

    fn format_label(fmt: &WaveFormat) -> String {
        let store = fmt.get_bitspersample();
        // Some drivers (Realtek among them) leave wValidBitsPerSample at 0,
        // which means "every stored bit": 0 of 32 is plain int32.
        let valid = if fmt.get_validbitspersample() == 0 {
            store
        } else {
            fmt.get_validbitspersample()
        };
        match fmt.get_subformat() {
            Ok(SampleType::Float) => "float32".into(),
            Ok(SampleType::Int) => match (store, valid) {
                (16, 16) => "int16".into(),
                (24, 24) => "int24 (packed)".into(),
                (32, 24) => "int32 (24-in-32)".into(),
                (32, 32) => "int32".into(),
                (s, v) => format!("int{v} (in {s})"),
            },
            Err(_) => {
                // WAVEFORMATEX: SubFormat is absent; type lives in wFormatTag.
                let tag = fmt.as_waveformatex_ref().wFormatTag;
                match tag as u32 {
                    1 /* PCM */ => match (store, valid) {
                        (16, 16) => "int16".into(),
                        (24, 24) => "int24 (packed)".into(),
                        (32, 32) => "int32".into(),
                        (s, v) => format!("int{v} (in {s})"),
                    },
                    3 /* IEEE_FLOAT */ => "float32".into(),
                    _ => format!("unknown (tag={tag} bits={store})"),
                }
            }
        }
    }

    /// DSP-mode format priority: 24-bit-in-32 → 32-bit int → float32.
    fn dsp_candidates(rate: u32) -> Vec<WaveFormat> {
        vec![
            WaveFormat::new(32, 24, &SampleType::Int, rate as usize, 2, None),
            WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
            WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None),
        ]
    }

    /// DoP's format priority: a 24-bit integer word, 24 in 32 → 24 packed →
    /// 32/32 (the DoP word in its top 24 bits). Never float, never 16-bit.
    fn dop_candidates(rate: u32) -> Vec<WaveFormat> {
        vec![
            WaveFormat::new(32, 24, &SampleType::Int, rate as usize, 2, None),
            WaveFormat::new(24, 24, &SampleType::Int, rate as usize, 2, None),
            WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
        ]
    }

    /// Direct-mode format priority by source bit depth (bit-perfect intent).
    /// 0, a source of no integer depth (a lossy decoder's output, f32 in the
    /// decoder): the widest integer the device takes — 32, 24 in 32, 24, 16.
    fn direct_candidates(rate: u32, source_bits: u32) -> Vec<WaveFormat> {
        match source_bits {
            0 => vec![
                WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(32, 24, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(24, 24, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(16, 16, &SampleType::Int, rate as usize, 2, None),
            ],
            16 => vec![
                WaveFormat::new(16, 16, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(32, 24, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
            ],
            24 => vec![
                WaveFormat::new(32, 24, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(24, 24, &SampleType::Int, rate as usize, 2, None),
                WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
            ],
            _ /* 32 */ => vec![
                WaveFormat::new(32, 32, &SampleType::Int, rate as usize, 2, None),
            ],
        }
    }

    fn wasapi_hresult(e: &WasapiError) -> Option<i32> {
        if let WasapiError::Windows(ref we) = e {
            Some(we.code().0)
        } else {
            None
        }
    }

    fn exclusive_err_msg(e: &WasapiError, rate: u32, device: &str) -> String {
        exclusive_err_text(wasapi_hresult(e), rate, device, || describe(e))
    }

    /// What the listener is told when exclusive mode fails: the HRESULT, if
    /// the driver gave one, in plain words; `described` for any other.
    fn exclusive_err_text(code: Option<i32>, rate: u32, device: &str, described: impl FnOnce() -> String) -> String {
        match code {
            Some(c) if c == HRESULT_DEVICE_IN_USE => format!(
                "{device} is busy: another application is playing through it \
                 (a browser, a messenger, another player), and exclusive mode \
                 needs the device to itself. Stop the sound there and press Play. \
                 [AUDCLNT_E_DEVICE_IN_USE at {rate} Hz]"
            ),
            Some(c) if c == HRESULT_EXCLUSIVE_NOT_ALLOWED => format!(
                "{device} does not allow exclusive mode. Windows Sound settings → \
                 {device} → Properties → Advanced → tick \"Allow applications to \
                 take exclusive control of this device\". \
                 [AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED at {rate} Hz]"
            ),
            Some(c) if c as u32 == 0x88890008 => format!(
                "{device} does not accept {rate} Hz in exclusive mode \
                 (tried int24-in-32, int32, float32). Pick a lower FS multiplier. \
                 [AUDCLNT_E_UNSUPPORTED_FORMAT]"
            ),
            Some(c) if c as u32 == 0x8889000F => format!(
                "{device} would not open at {rate} Hz: another program holds it \
                 (an ASIO host), or the rate or the clock is locked in its \
                 driver's settings. Free it and press Play. \
                 [AUDCLNT_E_ENDPOINT_CREATE_FAILED at {rate} Hz]"
            ),
            _ => format!("{device}: exclusive mode failed at {rate} Hz — {}", described()),
        }
    }

    // ── Device enumeration ───────────────────────────────────────────────────

    pub fn list_devices_impl() -> Result<Vec<DeviceInfo>, String> {
        let _ = wasapi::initialize_mta();

        let enumerator = DeviceEnumerator::new().map_err(|e| format!("{e}"))?;
        let default_id = enumerator
            .get_default_device(&Direction::Render)
            .and_then(|d| d.get_id())
            .unwrap_or_default();

        let collection = enumerator
            .get_device_collection(&Direction::Render)
            .map_err(|e| format!("get_device_collection: {e}"))?;

        let mut out = Vec::new();
        for device_res in &collection {
            let device = match device_res {
                Ok(d) => d,
                Err(_) => continue,
            };
            let id = device.get_id().unwrap_or_default();
            let name = device.get_friendlyname().unwrap_or_else(|_| id.clone());
            let is_default = id == default_id;
            out.push(DeviceInfo { id, name, is_default });
        }
        Ok(out)
    }

    // ── Format negotiation ───────────────────────────────────────────────────

    /// Try each candidate in order, returning the first the driver accepts in
    /// exclusive mode (via `is_supported_exclusive_with_quirks`).
    // Used only by probe_formats_impl; kept for the DAC capabilities panel.
    #[allow(dead_code)]
    fn negotiate_exclusive(
        client: &wasapi::AudioClient,
        candidates: &[WaveFormat],
    ) -> Option<WaveFormat> {
        for fmt in candidates {
            if let Ok(actual) = client.is_supported_exclusive_with_quirks(fmt) {
                return Some(actual);
            }
        }
        None
    }

    /// Initialise an AudioClient for exclusive event-driven render, handling
    /// the AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED retry loop recommended by MSDN.
    fn init_exclusive(
        device: &wasapi::Device,
        fmt: &WaveFormat,
        period_hns: i64,
    ) -> Result<(wasapi::AudioClient, u32 /* buffer_frames */), WasapiError> {
        let mode = StreamMode::EventsExclusive { period_hns };
        let mut client = device.get_iaudioclient()?;

        match client.initialize_client(fmt, &Direction::Render, &mode) {
            Ok(()) => {
                let buf = client.get_buffer_size()?;
                Ok((client, buf))
            }
            Err(ref e) if wasapi_hresult(e) == Some(HRESULT_BUFFER_SIZE_NOT_ALIGNED) => {
                // The driver wants a different buffer size. Read what it actually
                // wants, compute the matching period in 100-ns units, get a fresh
                // AudioClient (the spec requires a new one after a failed Init),
                // and retry.
                let buf = client.get_buffer_size()?;
                let aligned_period =
                    calculate_period_100ns(buf as i64, fmt.get_samplespersec() as i64);
                let mode2 = StreamMode::EventsExclusive { period_hns: aligned_period };
                let mut client2 = device.get_iaudioclient()?;
                client2.initialize_client(fmt, &Direction::Render, &mode2)?;
                let buf2 = client2.get_buffer_size()?;
                Ok((client2, buf2))
            }
            Err(e) => Err(e),
        }
    }

    // Used only by probe_formats; kept for the DAC capabilities panel.
    #[allow(dead_code)]
    pub fn probe_formats_impl(
        device_id: Option<&str>,
        rates: &[u32],
    ) -> Result<Vec<(u32, String)>, String> {
        let _ = wasapi::initialize_mta();

        let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
        let device = match device_id {
            Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
            None => enumerator
                .get_default_device(&Direction::Render)
                .map_err(|e| e.to_string())?,
        };

        // One AudioClient is enough for format queries; IsFormatSupported does
        // not change the client's initialisation state.
        let client = device.get_iaudioclient().map_err(|e| e.to_string())?;

        let mut results = Vec::with_capacity(rates.len());
        for &rate in rates {
            match negotiate_exclusive(&client, &dsp_candidates(rate)) {
                Some(fmt) => results.push((rate, format_label(&fmt))),
                None => results.push((rate, "unsupported".to_string())),
            }
        }
        Ok(results)
    }

    // ── Audio thread ─────────────────────────────────────────────────────────

    /// Everything the audio thread needs after successful initialisation.
    struct StreamHandle {
        client: wasapi::AudioClient,
        render: wasapi::AudioRenderClient,
        event: wasapi::Handle,
        fmt: SampleFmt,
        blockalign: usize,
        info: StreamInfo,
    }

    /// Event handle, render client and one period of silence for an
    /// initialised exclusive client.
    fn finish_exclusive(
        client: wasapi::AudioClient,
        buf: u32,
        actual: &WaveFormat,
        device_name: String,
        rate: u32,
        mode: &OutputMode,
        (def_period, min_period): (i64, i64),
    ) -> Result<StreamHandle, String> {
        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
        let fmt = SampleFmt::from_wave(actual)?;
        let blockalign = actual.get_blockalign() as usize;
        let label = format_label(actual);
        // Log everything the driver told us at this stream open.
        {
            let store = actual.get_bitspersample();
            let valid_raw = actual.get_validbitspersample();
            let valid = if valid_raw == 0 { store } else { valid_raw };
            let ba = actual.get_blockalign();
            let ch = actual.get_nchannels();
            let tag = actual.as_waveformatex_ref().wFormatTag;
            let subfmt_str = match actual.get_subformat().ok() {
                Some(SampleType::Float) => "IEEE_FLOAT/EXTENSIBLE".to_string(),
                Some(SampleType::Int) => "PCM/EXTENSIBLE".to_string(),
                None => format!("WAVEFORMATEX(tag={tag})"),
            };
            let mode_str = match mode {
                OutputMode::Dsp => "DSP".to_string(),
                OutputMode::Direct { source_bits: 0 } => "Direct(no integer depth: the widest)".to_string(),
                OutputMode::Direct { source_bits } => {
                    format!("Direct({}bit)", source_bits)
                }
                OutputMode::Dsd { rate, modulator } => {
                    format!("DSD{}/{} Hz DoP, {modulator:?}", rate.multiple, rate.base)
                }
                OutputMode::DsdDirect { rate } => format!("DSD{}/{} Hz DoP, the file as it is", rate.multiple, rate.base),
            };
            let ms = |hns: i64| hns as f64 / 10_000.0;
            crate::aelog!(
                "[PLAYER] stream open: device={device_name:?} rate={rate} ch={ch} \
                 subfmt={subfmt_str} bits={store} valid={valid} blockalign={ba} \
                 packer={fmt:?} mode={mode_str} period={buf} frames ({:.2} ms; \
                 device default {:.2} ms, min {:.2} ms)",
                buf as f64 * 1000.0 / rate.max(1) as f64,
                ms(def_period),
                ms(min_period),
            );
        }
        let info = StreamInfo {
            device_name,
            exclusive: true,
            format: label,
            rate,
            period_frames: buf,
            buffer_frames: buf,
            // Exclusive mode: two hardware periods is a conservative latency
            // estimate (one in the hardware FIFO, one being filled).
            latency_frames: buf * 2,
        };
        Ok(StreamHandle { client, render, event, fmt, blockalign, info })
    }

    /// What the endpoint says about itself, in its own words: the shared-mode
    /// mix format, the raw answer of IsFormatSupported(EXCLUSIVE) for every
    /// DSP candidate at every rate, and one real exclusive Initialize per rate
    /// — the only call that reports a device another application is using.
    /// Nothing is started, so nothing is heard.
    pub fn diagnose_impl(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        use std::fmt::Write;
        let _ = wasapi::initialize_mta();
        let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
        let device = match device_id {
            Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
            None => enumerator
                .get_default_device(&Direction::Render)
                .map_err(|e| e.to_string())?,
        };
        let name = device.get_friendlyname().unwrap_or_else(|_| "Unknown".into());
        let mut r = String::new();
        let _ = writeln!(r, "device: {name}");
        let client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        match client.get_mixformat() {
            Ok(m) => {
                let _ = writeln!(
                    r,
                    "shared mix format: {} Hz, {}, {} ch",
                    m.get_samplespersec(),
                    format_label(&m),
                    m.get_nchannels()
                );
            }
            Err(e) => {
                let _ = writeln!(r, "shared mix format: {}", describe(&e));
            }
        }
        // The period the player asks for (`exclusive_period_hns`).
        let period = client
            .get_device_period()
            .map(|(d, m)| exclusive_period_hns(d, m))
            .unwrap_or(100_000);
        for &rate in rates {
            let mut line = format!("{rate:>6} Hz  support:");
            for fmt in dsp_candidates(rate) {
                let ans = match client.is_supported(&fmt, &wasapi::ShareMode::Exclusive) {
                    Ok(_) => "yes".to_string(),
                    Err(e) => describe(&e),
                };
                let _ = write!(line, " {}={}", format_label(&fmt), ans);
            }
            let first = &dsp_candidates(rate)[0];
            let init = match init_exclusive(&device, first, period) {
                Ok((_c, buf)) => format!("ok ({buf} frames)"),
                Err(e) => describe(&e),
            };
            let _ = writeln!(r, "{line}  |  Initialize {}: {init}", format_label(first));
        }
        Ok(r)
    }

    fn open_stream(cfg: &OutputConfig) -> Result<StreamHandle, String> {
        let _ = wasapi::initialize_mta();

        let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;

        let device = match &cfg.device_id {
            Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
            None => enumerator
                .get_default_device(&Direction::Render)
                .map_err(|e| e.to_string())?,
        };

        let device_name = device.get_friendlyname().unwrap_or_else(|_| "Unknown".into());

        // Build the format candidate list for the requested mode.
        let candidates: Vec<WaveFormat> = match &cfg.mode {
            OutputMode::Dsp => dsp_candidates(cfg.rate),
            OutputMode::Direct { source_bits } => direct_candidates(cfg.rate, *source_bits),
            OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. } => dop_candidates(cfg.rate),
        };

        // Try exclusive mode with each candidate in priority order.
        let mut exclusive_err: Option<WasapiError> = None;
        for desired in &candidates {
            let client_probe = match device.get_iaudioclient() {
                Ok(c) => c,
                Err(e) => {
                    exclusive_err = Some(e);
                    continue;
                }
            };

            let actual = match client_probe.is_supported_exclusive_with_quirks(desired) {
                Ok(f) => f,
                Err(e) => {
                    exclusive_err = Some(e);
                    continue;
                }
            };

            let (def_period, min_period) = match client_probe.get_device_period() {
                Ok(p) => p,
                Err(e) => {
                    exclusive_err = Some(e);
                    continue;
                }
            };

            // The device's default period, 10 ms at least
            // (`exclusive_period_hns`); 128-byte alignment for Intel HDA and
            // similar devices.
            let desired_period = exclusive_period_hns(def_period, min_period);
            let aligned_period = match client_probe
                .calculate_aligned_period_near(desired_period, Some(128), &actual)
            {
                Ok(p) => p,
                Err(_) => desired_period,
            };

            // is_supported_exclusive_with_quirks checked format support but did
            // not call Initialize; we need a fresh client for initialization.
            drop(client_probe);

            match init_exclusive(&device, &actual, aligned_period) {
                Ok((client, buf)) => {
                    return finish_exclusive(
                        client, buf, &actual, device_name, cfg.rate, &cfg.mode, (def_period, min_period),
                    );
                }
                Err(e) => {
                    exclusive_err = Some(e);
                }
            }
        }

        // IsFormatSupported said no to every candidate, and it says no without
        // saying why: a device another application is playing through answers
        // exactly like a device that cannot do the rate. Initialize is the
        // call that knows the difference — and the one that decides — so it
        // gets the last word, on the first candidate.
        let ambiguous = |e: &WasapiError| match wasapi_hresult(e) {
            None => true,                              // the crate's own "no compatible format"
            Some(c) => c as u32 == 0x88890008,         // AUDCLNT_E_UNSUPPORTED_FORMAT
        };
        if exclusive_err.as_ref().map_or(true, ambiguous) {
            if let Some(first) = candidates.first() {
                let periods = device.get_iaudioclient().and_then(|c| c.get_device_period());
                match periods.and_then(|(def, min)| {
                    init_exclusive(&device, first, exclusive_period_hns(def, min)).map(|(c, buf)| (c, buf, (def, min)))
                }) {
                    Ok((client, buf, periods)) => {
                        crate::aelog!(
                            "[PLAYER] {} refused every format in IsFormatSupported but accepted {} at Initialize",
                            device_name,
                            format_label(first)
                        );
                        return finish_exclusive(
                            client, buf, first, device_name, cfg.rate, &cfg.mode, periods,
                        );
                    }
                    Err(e) => exclusive_err = Some(e),
                }
            }
        }

        // All exclusive attempts failed. DoP never goes through the Windows
        // mixer: it would resample and mix the DoP words into noise.
        let dsd = cfg.mode.is_dop();
        if !cfg.allow_shared || dsd {
            let msg = exclusive_err
                .as_ref()
                .map(|e| exclusive_err_msg(e, cfg.rate, &device_name))
                .unwrap_or_else(|| format!("No format accepted by the device at {} Hz", cfg.rate));
            // A device that takes the carrier rate in no 24-bit integer
            // format: the controller plays PCM instead.
            let unsupported = exclusive_err.as_ref().map_or(true, |e| match wasapi_hresult(e) {
                None => true,
                Some(c) => c as u32 == 0x88890008, // AUDCLNT_E_UNSUPPORTED_FORMAT
            });
            if dsd && unsupported {
                return Err(format!("{DOP_FORMAT_ERR} at {} Hz on {device_name} ({msg})", cfg.rate));
            }
            return Err(msg);
        }

        // ── Shared-mode fallback ─────────────────────────────────────────────
        // Windows mixer resamples and converts; this path is not bit-perfect.
        let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        let shared_fmt =
            WaveFormat::new(32, 32, &SampleType::Float, cfg.rate as usize, 2, None);
        let (def_period, _) = client.get_device_period().map_err(|e| e.to_string())?;
        let shared_stream_mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: def_period * 4,
        };
        client
            .initialize_client(&shared_fmt, &Direction::Render, &shared_stream_mode)
            .map_err(|e| format!("Shared fallback failed: {e}"))?;

        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
        let buf = client.get_buffer_size().map_err(|e| e.to_string())?;
        let fmt = SampleFmt::Float32;
        let blockalign = shared_fmt.get_blockalign() as usize;
        let label = format_label(&shared_fmt);

        {
            let mode_str = match &cfg.mode {
                OutputMode::Dsp => "DSP (shared)".to_string(),
                OutputMode::Direct { source_bits: 0 } => "Direct(no integer depth)/shared".to_string(),
                OutputMode::Direct { source_bits } => {
                    format!("Direct({}bit)/shared", source_bits)
                }
                OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. } => unreachable!("DoP never opens in shared mode"),
            };
            crate::aelog!(
                "[PLAYER] stream open (shared): device={device_name:?} rate={} ch=2 \
                 subfmt=IEEE_FLOAT/WAVEFORMATEXTENSIBLE bits=32 valid=32 blockalign={} \
                 packer=Float32 mode={mode_str}",
                cfg.rate,
                blockalign,
            );
        }

        let info = StreamInfo {
            device_name,
            exclusive: false,
            format: label,
            rate: cfg.rate,
            period_frames: buf,
            buffer_frames: buf,
            // Shared mode: use the hardware buffer size as a latency estimate
            // (the Windows mixer adds at least one buffer period).
            latency_frames: buf,
        };
        Ok(StreamHandle { client, render, event, fmt, blockalign, info })
    }

    /// The audio thread entry point. Initialises COM, opens the stream, reports
    /// the result, then enters the render loop.
    pub fn audio_thread(
        cfg: OutputConfig,
        timeline: Arc<Timeline>,
        shared: Arc<OutputShared>,
        stop: Arc<AtomicBool>,
        go: Arc<AtomicBool>,
        init_tx: Sender<Result<StreamInfo, String>>,
    ) {
        let fail = |e: String| {
            shared.alive.store(false, Ordering::SeqCst);
            init_tx.send(Err(e)).ok();
        };
        let sh = match open_stream(&cfg) {
            Ok(sh) => sh,
            Err(e) => return fail(e),
        };

        let StreamHandle { client, render, event, fmt, blockalign, info } = sh;

        // What goes to the device, period by period (`Periods`): DSP mode
        // dithers (and applies the volume), Direct mode does neither, DSD
        // output modulates and packs DoP.
        let mut periods = match Periods::new(info.rate, fmt, blockalign, info.buffer_frames as usize, &cfg.mode, super::test_mute()) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        // One period of silence before the start (DoP: DSD silence, markers
        // running on into the first period).
        let prefill = periods.prefill(info.buffer_frames as usize);
        if let Err(e) = render.write_to_device(info.buffer_frames as usize, &prefill, None) {
            return fail(e.to_string());
        }
        shared.dsd_overloads.store(0, Ordering::Relaxed);
        shared.dsd_guard_tripped.store(false, Ordering::Relaxed);
        shared.dsd_opened(periods.dsd_delay(), periods.dsd_kind());
        if super::test_mute() {
            crate::aelog!("[PLAYER] test mute: AURA_TEST_MUTE=1 - the device gets silence, levels are measured before it");
        }

        // Report success — start() on the spawning side unblocks here.
        init_tx.send(Ok(info.clone())).ok();

        // ── MMCSS "Pro Audio" scheduling ─────────────────────────────────────
        let name_w: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index: u32 = 0;
        let mmcss = unsafe {
            winapi::um::avrt::AvSetMmThreadCharacteristicsW(name_w.as_ptr(), &mut task_index)
        };

        // The scratch buffers are sized to the hardware period (`Periods`):
        // no allocation happens inside the loop.
        let buf_frames = info.buffer_frames as usize;

        // Export device latency so the spectrum analyser can sync its window.
        shared.device_latency_frames.store(info.latency_frames as u64, Ordering::Relaxed);

        // A gated start (`OutputStream::start_gated`): the device is open and
        // initialised; the stream starts when the controller lets it, once
        // its pre-roll is rendered — or never, stopped first. Until then
        // nothing is read from the timeline and nothing written, so the
        // first period after it is the first a stream opened then played.
        if !super::wait_to_start(&go, &stop) {
            shared.alive.store(false, Ordering::SeqCst);
            revert_mmcss(mmcss);
            return;
        }
        crate::player::output_tap::open(info.rate);

        if let Err(e) = client.start_stream() {
            let msg = format!("start_stream: {e}");
            *shared.last_error.lock().unwrap() = Some(msg);
            shared.alive.store(false, Ordering::SeqCst);
            revert_mmcss(mmcss);
            return;
        }

        let mut dev = Wasapi {
            client: &client,
            render: &render,
            event: &event,
            shared: &shared,
            clock: client.get_audioclock().ok()
                .and_then(|c| c.get_frequency().ok().filter(|&f| f > 0).map(|f| (c, f))),
            rate: info.rate,
            written: info.buffer_frames as u64, // the silence pre-fill
            probe_every: ((info.rate as usize / 4) / buf_frames.max(1)).max(1),
            since_probe: 0,
            lat_ema: 0.0,
        };
        if let Err(e) = super::run(&mut dev, &mut periods, &timeline, &shared, &stop) {
            *shared.last_error.lock().unwrap() = Some(e);
            shared.device_lost.store(true, Ordering::SeqCst);
        }

        client.stop_stream().ok();
        // No stream, no recent peak (the running maximum stays).
        shared.dev_peak_bits.store(0f64.to_bits(), Ordering::Relaxed);
        shared.alive.store(false, Ordering::SeqCst);
        revert_mmcss(mmcss);
    }

    /// The WASAPI render client as the render loop's device (`run`), the
    /// output latency measured as it goes: frames handed to WASAPI minus the
    /// device position (IAudioClock), every ~250 ms, smoothed. It replaces
    /// the estimate for the spectrum and the analyzer. What the driver does
    /// not report (the DAC's own filter, the speakers) is left to the page's
    /// per-device offset.
    struct Wasapi<'a> {
        client: &'a wasapi::AudioClient,
        render: &'a wasapi::AudioRenderClient,
        event: &'a wasapi::Handle,
        shared: &'a OutputShared,
        clock: Option<(wasapi::AudioClock, u64)>,
        rate: u32,
        /// Frames handed to WASAPI.
        written: u64,
        probe_every: usize,
        since_probe: usize,
        lat_ema: f64,
    }

    impl super::Device for Wasapi<'_> {
        fn wait(&mut self) -> bool {
            self.event.wait_for_event(100).is_ok()
        }

        fn space(&mut self) -> Result<usize, String> {
            self.client.get_available_space_in_frames().map(|n| n as usize).map_err(|e| e.to_string())
        }

        fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
            self.since_probe += 1;
            if self.since_probe >= self.probe_every {
                self.since_probe = 0;
                if let Some((clk, freq)) = &self.clock {
                    if let Ok((pos, _)) = clk.get_position() {
                        let played = (pos as u128 * self.rate as u128 / *freq as u128) as u64;
                        if self.written > played {
                            let lat = (self.written - played) as f64;
                            self.lat_ema = if self.lat_ema == 0.0 { lat } else { self.lat_ema * 0.8 + lat * 0.2 };
                            self.shared.device_latency_frames.store(self.lat_ema.round() as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            // Every period writes exactly `frames` (or stops).
            self.written += frames as u64;
            self.render.write_to_device(frames, bytes, None).map_err(|e| e.to_string())
        }
    }

    fn revert_mmcss(handle: winapi::um::winnt::HANDLE) {
        if !handle.is_null() {
            unsafe { winapi::um::avrt::AvRevertMmThreadCharacteristics(handle); }
        }
    }

    #[cfg(test)]
    mod err_text_tests {
        use super::exclusive_err_text;

        /// ENDPOINT_CREATE_FAILED says what holds the device: an RME kept
        /// by its ASIO host at 44.1 kHz refused ×4 with the bare code (ASR #139).
        #[test]
        fn endpoint_create_failed_says_what_holds_the_device() {
            let m = exclusive_err_text(Some(0x8889000Fu32 as i32), 176_400, "ADI-2/4 Pro SE", || unreachable!());
            assert!(m.starts_with("ADI-2/4 Pro SE would not open at 176400 Hz"), "{m}");
            assert!(m.contains("ASIO host") && m.contains("driver's settings") && m.contains("press Play"), "{m}");
            assert!(m.ends_with("[AUDCLNT_E_ENDPOINT_CREATE_FAILED at 176400 Hz]"), "{m}");
            // Any other code: the driver's words.
            let o = exclusive_err_text(Some(0x88890001u32 as i32), 44_100, "DAC", || "AUDCLNT_E_NOT_INITIALIZED".into());
            assert_eq!(o, "DAC: exclusive mode failed at 44100 Hz — AUDCLNT_E_NOT_INITIALIZED");
        }
    }

    #[cfg(test)]
    mod format_label_tests {
        use super::format_label;
        use wasapi::{SampleType, WaveFormat};

        fn int(store: usize, valid: usize) -> WaveFormat {
            WaveFormat::new(store, valid, &SampleType::Int, 44_100, 2, None)
        }

        #[test]
        fn labels_every_format_the_player_opens() {
            assert_eq!(format_label(&int(16, 16)), "int16");
            assert_eq!(format_label(&int(24, 24)), "int24 (packed)");
            assert_eq!(format_label(&int(32, 24)), "int32 (24-in-32)");
            assert_eq!(format_label(&int(32, 32)), "int32");
            let float = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 2, None);
            assert_eq!(format_label(&float), "float32");
        }

        /// wValidBitsPerSample = 0 (Realtek's answer) means every stored bit,
        /// not "int0 (in 32)".
        #[test]
        fn zero_valid_bits_means_all_of_them() {
            assert_eq!(format_label(&int(32, 0)), "int32");
            assert_eq!(format_label(&int(16, 0)), "int16");
            assert_eq!(format_label(&int(24, 0)), "int24 (packed)");
            let float = WaveFormat::new(32, 0, &SampleType::Float, 44_100, 2, None);
            assert_eq!(format_label(&float), "float32");
        }
    }

    // ── from_wave / resolve_sample_fmt tests ─────────────────────────────────

    #[cfg(test)]
    mod from_wave_tests {
        use super::{dsp_candidates, direct_candidates, resolve_sample_fmt, SampleFmt};
        use SampleFmt::*;
        use wasapi::{SampleType, WaveFormat};

        fn wave_from(fmt: &WaveFormat) -> Result<SampleFmt, String> {
            resolve_sample_fmt(
                fmt.get_subformat().ok(),
                fmt.as_waveformatex_ref().wFormatTag,
                fmt.get_bitspersample(),
                fmt.get_validbitspersample(),
                fmt.get_blockalign(),
                fmt.get_nchannels(),
            )
        }

        /// DSP float32 candidate converted to WAVEFORMATEX must resolve to Float32.
        #[test]
        fn waveformatex_float32_dsp_candidate() {
            let float_dsp = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
            let wfex = float_dsp.to_waveformatex().unwrap();
            assert_eq!(wave_from(&wfex), Ok(Float32));
        }

        /// WAVEFORMATEX PCM formats map to the correct packers.
        #[test]
        fn waveformatex_pcm_formats() {
            let pcm16 = WaveFormat::new(16, 16, &SampleType::Int, 44_100, 2, None);
            assert_eq!(wave_from(&pcm16.to_waveformatex().unwrap()), Ok(Int16));

            let pcm24 = WaveFormat::new(24, 24, &SampleType::Int, 44_100, 2, None);
            assert_eq!(wave_from(&pcm24.to_waveformatex().unwrap()), Ok(Int24Packed));

            let pcm32 = WaveFormat::new(32, 32, &SampleType::Int, 44_100, 2, None);
            assert_eq!(wave_from(&pcm32.to_waveformatex().unwrap()), Ok(Int32));
        }

        /// An unknown wFormatTag (neither 1=PCM nor 3=IEEE_FLOAT) must be an error.
        #[test]
        fn unknown_tag_is_error() {
            let result = resolve_sample_fmt(None, 7, 8, 8, 2, 2);
            assert!(result.is_err(), "expected Err for unknown tag, got Ok");
            let msg = result.unwrap_err();
            assert!(msg.contains("unknown wFormatTag"), "message: {msg}");
        }

        /// Every DSP candidate must resolve to a packer whose bytes_per_frame
        /// matches the WaveFormat's blockalign.
        #[test]
        fn dsp_candidates_packer_blockalign_consistent() {
            for fmt in dsp_candidates(48_000) {
                let sf = wave_from(&fmt).expect("DSP candidate must be supported");
                let expected_ba: u32 = match sf {
                    Int16 => 4,
                    Int24Packed => 6,
                    Int32 | Float32 => 8,
                };
                assert_eq!(
                    fmt.get_blockalign(), expected_ba,
                    "DSP candidate {sf:?}: blockalign {} != expected {}",
                    fmt.get_blockalign(), expected_ba
                );
            }
        }

        /// Every Direct-mode candidate must also resolve consistently.
        #[test]
        fn direct_candidates_packer_blockalign_consistent() {
            for source_bits in [0u32, 16, 24, 32] {
                for fmt in direct_candidates(48_000, source_bits) {
                    let sf = wave_from(&fmt).expect("Direct candidate must be supported");
                    let expected_ba: u32 = match sf {
                        Int16 => 4,
                        Int24Packed => 6,
                        Int32 | Float32 => 8,
                    };
                    assert_eq!(
                        fmt.get_blockalign(), expected_ba,
                        "Direct({}bit) candidate {sf:?}: blockalign {} != expected {}",
                        source_bits, fmt.get_blockalign(), expected_ba
                    );
                }
            }
        }
    }

    // ── safety_mute_needed / unpack_peak tests ───────────────────────────────

    #[cfg(test)]
    mod safety_tests {
        use super::{pack_frames, unpack_peak, safety_mute_needed, SampleFmt};
        use SampleFmt::*;

        /// Round-trip pack → unpack must reproduce the original peak for all formats.
        #[test]
        fn unpack_peak_round_trips_pack() {
            let ch_l = vec![0.5f64; 16];
            let ch_r = vec![-0.7f64; 16];
            for fmt in [Float32, Int32, Int24Packed, Int16] {
                let bpf: usize = match fmt {
                    Int16 => 4,
                    Int24Packed => 6,
                    Int32 | Float32 => 8,
                };
                let mut buf = vec![0u8; 16 * bpf];
                pack_frames(&ch_l, &ch_r, 16, fmt, &mut buf);
                let (peak, has_nan_inf) = unpack_peak(&buf, 16, fmt);
                assert!(!has_nan_inf, "{fmt:?}: unexpected NaN/Inf");
                // Int16 has limited precision; allow 1 LSB tolerance (~3e-5).
                let tol = if matches!(fmt, Int16) { 1.0 / 32_768.0 } else { 1e-6 };
                assert!(
                    (peak - 0.7).abs() < tol,
                    "{fmt:?}: peak {peak} is not close to 0.7 (tol={tol})"
                );
            }
        }

        /// NaN in a Float32 stream is detected and reported.
        #[test]
        fn float32_nan_detected() {
            let mut buf = [0u8; 8];
            buf[0..4].copy_from_slice(&f32::NAN.to_le_bytes());
            let (peak, has) = unpack_peak(&buf, 1, Float32);
            assert!(has, "NaN must set has_nan_inf");
            assert!(peak.is_infinite(), "peak must be Infinity when NaN present");
        }

        /// Inf in a Float32 stream is detected.
        #[test]
        fn float32_inf_detected() {
            let mut buf = [0u8; 8];
            buf[0..4].copy_from_slice(&f32::INFINITY.to_le_bytes());
            let (_, has) = unpack_peak(&buf, 1, Float32);
            assert!(has, "Inf must set has_nan_inf");
        }

        /// In DSP mode, a peak more than 12 dB above the volume triggers mute.
        #[test]
        fn dsp_peak_above_threshold_mutes() {
            // volume = 0.25 (-12 dB), threshold = 1.0; peak = 1.01 > 1.0 → mute
            assert!(safety_mute_needed(1.01, false, Int32, 0.25, false));
            // -60 dB: an overshoot up to +12 dB passes, a bypass of the volume does not
            assert!(!safety_mute_needed(0.0039, false, Int32, 0.001, false));
            assert!(safety_mute_needed(0.2, false, Int32, 0.001, false));
        }

        /// In DSP mode, normal audio below threshold does not mute.
        #[test]
        fn dsp_normal_audio_does_not_mute() {
            // volume = 1.0 (0 dBFS), threshold = 4.0; peak = 0.999 → no mute
            assert!(!safety_mute_needed(0.999, false, Int32, 1.0, false));
            // -120 dB (the blind test's mute): dither alone never trips it
            assert!(!safety_mute_needed(5e-6, false, Int32, 1e-6, false));
        }

        /// In DSP mode, NaN/Inf in a float stream always mutes.
        #[test]
        fn dsp_float_nan_mutes() {
            assert!(safety_mute_needed(f64::INFINITY, true, Float32, 0.1, false));
        }

        /// In Direct mode, int formats below 1.0 do not mute
        /// (int streams cannot carry NaN/Inf; near-full-scale is normal).
        #[test]
        fn direct_int_peak_does_not_mute() {
            assert!(!safety_mute_needed(0.9999, false, Int32, 1.0, true));
        }

        /// In Direct mode, float NaN/Inf mutes.
        #[test]
        fn direct_float_nan_mutes() {
            assert!(safety_mute_needed(f64::INFINITY, true, Float32, 1.0, true));
        }

        /// In Direct mode, float |x| > 1 mutes.
        #[test]
        fn direct_float_over_one_mutes() {
            assert!(safety_mute_needed(1.01, false, Float32, 1.0, true));
        }

        /// In Direct mode, float |x| ≤ 1 does not mute.
        #[test]
        fn direct_float_normal_does_not_mute() {
            assert!(!safety_mute_needed(0.99, false, Float32, 1.0, true));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── Non-ignored: run without audio hardware ───────────────────────────────

    /// list_devices() must return Ok on any machine (even with no audio device).
    #[test]
    fn list_devices_runs() {
        let result = list_devices();
        assert!(result.is_ok(), "list_devices failed: {:?}", result.err());
    }

    /// Int16 conversion: check the integer values at key points.
    #[test]
    fn int16_conversion_exact() {
        let cases: &[(f64, i16)] = &[
            (0.0, 0),
            (1.0, i16::MAX),             // clamped from 32768 → 32767
            (-1.0, i16::MIN),            // -32768
            (0.5, 16384),                // 0.5 * 32768
            (-0.5, -16384),
            (1.0 / 32768.0, 1),          // one LSB up
            (-1.0 / 32768.0, -1),        // one LSB down
        ];
        for &(x, want) in cases {
            let got = (x * 32_768.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            assert_eq!(got, want, "int16 for x = {x}");
        }
    }

    /// Int32 conversion of 24-bit-grid values must be multiples of 256 with the
    /// upper 24 bits matching the original integer — "24-bit grid values map
    /// exactly" as the spec states.
    #[test]
    fn int32_24bit_grid_exact() {
        let q_step = 1.0f64 / 8_388_608.0; // 1 / 2^23
        for k in &[-8_388_608i32, -1, 0, 1, 8_388_607] {
            let x = *k as f64 * q_step;
            let packed =
                (x * 2_147_483_648.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            assert_eq!(packed % 256, 0, "k={k}: i32={packed} is not a multiple of 256");
            assert_eq!(packed >> 8, *k, "k={k}: upper 24 bits differ");
        }
    }

    /// Int24-packed conversion: the three bytes must carry the correct signed
    /// 24-bit value in little-endian order.
    #[test]
    fn int24_packed_bytes() {
        // -1.0 → -8388608 (0xFF800000 as i32) → bytes [0x00, 0x00, 0x80]
        let neg1 = (-1.0f64 * 8_388_608.0)
            .round()
            .clamp(-8_388_608.0, 8_388_607.0) as i32;
        assert_eq!(neg1, -8_388_608);
        assert_eq!(&neg1.to_le_bytes()[..3], &[0x00, 0x00, 0x80]);

        // +1 LSB → 1 → bytes [0x01, 0x00, 0x00]
        let q = 1.0f64 / 8_388_608.0;
        let one_lsb = (q * 8_388_608.0).round() as i32;
        assert_eq!(one_lsb, 1);
        assert_eq!(&one_lsb.to_le_bytes()[..3], &[0x01, 0x00, 0x00]);
    }

    /// DitherState must leave every output sample on the 24-bit quantisation
    /// grid (i.e. a multiple of Q_STEP = 1/2^23).
    #[test]
    fn dither_output_on_24bit_grid() {
        use crate::audio::converter::dsp::dither::DitherState;
        let mut d = DitherState::new(48_000);
        let n = 512;
        let mut l: Vec<f64> = (0..n).map(|i| 0.3 * (i as f64 * 0.1).sin()).collect();
        let mut r: Vec<f64> = (0..n).map(|i| -0.7 * (i as f64 * 0.07).cos()).collect();
        d.process(&mut l, &mut r, 1.0);

        let q = 1.0 / 8_388_608.0;
        for (ch, name) in [(&l, "L"), (&r, "R")] {
            for &v in ch {
                let deviation = (v / q) - (v / q).round();
                assert!(
                    deviation.abs() < 1e-9,
                    "{name} sample {v} is not on the 24-bit grid (deviation {deviation})"
                );
            }
        }
    }

    /// The ramp-down in the hold path completes within one period: after
    /// `avail` samples the gain must reach exactly 0. (rt-1 regression guard.)
    #[test]
    fn ramp_down_completes_in_one_period() {
        let avail = 256usize;
        let mut gain = 1.0f64;
        let step = 1.0 / avail as f64;
        for _ in 0..avail {
            gain = (gain - step).max(0.0);
        }
        assert!(gain == 0.0, "gain after one period: {gain} (expected 0)");
    }

    /// The ramp-up in the jump path completes within one period.
    #[test]
    fn ramp_up_completes_in_one_period() {
        let avail = 256usize;
        let mut gain = 0.0f64;
        let step = 1.0 / avail as f64;
        for _ in 0..avail {
            gain = (gain + step).min(1.0);
        }
        assert!(gain == 1.0, "gain after one period: {gain} (expected 1.0)");
    }

    // ── Ignored: requires a live audio device ────────────────────────────────

    /// Open the default device in exclusive mode at 48 000 Hz, write silence
    /// for ~1 s, then stop. Must not produce audible sound (timeline is all
    /// zeros). Reports StreamInfo and underrun count.
    #[test]
    #[ignore]
    fn exclusive_silence_smoke() {
        use crate::player::timeline::Timeline;

        // Fill the timeline with 2 s of silence so the output thread always has
        // data; underruns indicate the pre-fill wasn't enough.
        let tl = Arc::new(Timeline::new(48_000, 4.0));
        {
            let n = 48_000 * 2;
            let zeros = vec![0.0f64; n];
            tl.append(&zeros, &zeros);
        }

        let shared = OutputShared::new();
        let cfg = OutputConfig {
            device_id: None,
            rate: 48_000,
            mode: OutputMode::Dsp,
            allow_shared: true,
        };

        let stream = match OutputStream::start_gated(cfg, tl.clone(), shared.clone()) {
            Ok(s) => {
                s.release();
                s
            }
            Err(e) => {
                println!("Could not open audio device: {e}");
                println!("(skipping smoke test — no audio device available)");
                return;
            }
        };

        let si = stream.info();
        println!("device:        {}", si.device_name);
        println!("exclusive:     {}", si.exclusive);
        println!("format:        {}", si.format);
        println!("rate:          {} Hz", si.rate);
        println!("buffer_frames: {}", si.buffer_frames);
        println!("period_frames: {}", si.period_frames);

        std::thread::sleep(std::time::Duration::from_millis(1100));

        let underruns = tl.underrun_frames();
        println!("underrun_frames after ~1 s: {underruns}");

        stream.stop();
        println!("stream stopped cleanly");
    }
}
