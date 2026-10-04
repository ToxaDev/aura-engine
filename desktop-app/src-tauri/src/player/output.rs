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
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
};

use serde::Serialize;

use crate::audio::converter::dsp::dither::DitherState;
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
    /// Full DSP chain: volume applied, 24-bit TPDF dither.
    Dsp,
    /// Bit-perfect pass-through at the native source bit depth.
    /// Volume control and dither are intentionally skipped; the signal is
    /// delivered to the DAC exactly as decoded. `source_bits` 0: the source
    /// has no integer depth (a lossy decoder's output) — it goes out in the
    /// widest integer the device takes.
    Direct { source_bits: u32 },
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
}

impl OutputShared {
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

    /// Signal the audio thread to stop and wait for it to exit.
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

    /// The state the audio thread keeps from one period to the next, and its
    /// scratch buffers, sized once to the hardware buffer: nothing is
    /// allocated per period.
    pub(super) struct Periods {
        fmt: SampleFmt,
        blockalign: usize,
        /// DSP mode's dither, which folds in the volume; Direct mode has none.
        dither: Option<DitherState>,
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
    }

    impl Periods {
        pub(super) fn new(rate: u32, fmt: SampleFmt, blockalign: usize, buf_frames: usize, mode: &OutputMode, mute: bool) -> Periods {
            let dither = match mode {
                OutputMode::Dsp => Some(DitherState::new(rate)),
                OutputMode::Direct { .. } => None,
            };
            Periods {
                fmt,
                blockalign,
                is_direct: dither.is_none(),
                dither,
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
            }
        }

        /// The next period of `avail` frames (at most the buffer's), packed
        /// for the device: its bytes, and the words a failed write of it is
        /// said with — " (hold)" held in silence, " (pause)" paused in
        /// silence, nothing for every other period.
        pub(super) fn next(&mut self, avail: usize, timeline: &Timeline, shared: &OutputShared) -> (&[u8], &'static str) {
            let nbytes = avail * self.blockalign;
            // Hold (track switch / seek): ramp to silence, wait for a jump
            // frame from the render thread, then teleport the read pointer and
            // ramp back up. The hold flag is set by the command thread before
            // any preparation starts, so no old audio escapes into the new one.
            let held = shared.hold.load(Ordering::Acquire);
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
            self.out_bytes[..nbytes].fill(0);
            self.guard(nbytes, avail, shared.volume_lin(), shared);
            &self.out_bytes[..nbytes]
        }

        /// The packed period measured, silenced by the safety guard if it
        /// must be, then by the test mute; and what is left — what the device
        /// is given — measured too (`sentPeakMaxDb`). Every period comes
        /// through here, silence too, so the peaks move on a regular cadence.
        fn guard(&mut self, nbytes: usize, avail: usize, volume_lin: f64, shared: &OutputShared) {
            let bytes = &mut self.out_bytes[..nbytes];
            let (peak, has_nan_inf) = unpack_peak(bytes, avail, self.fmt);
            // Post-volume peak of what was rendered for the device, before the
            // safety and test mutes below: devPeakDb over ~100 ms, and the
            // running maximum on every period, so a short tail is never lost.
            let measured = if has_nan_inf { 1.0 } else { peak };
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
            let sent = if sent.is_finite() { sent } else { f64::MAX };
            if sent > f64::from_bits(shared.sent_peak_max_bits.load(Ordering::Relaxed)) {
                shared.sent_peak_max_bits.store(sent.to_bits(), Ordering::Relaxed);
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
                    let mut p = Periods::new(rate, fmt, ba, avail, &mode, true);
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
            let mut p = Periods::new(rate, SampleFmt::Int16, 4, avail, &OutputMode::Direct { source_bits: 16 }, false);
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
        match wasapi_hresult(e) {
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
            _ => format!("{device}: exclusive mode failed at {rate} Hz — {}", describe(e)),
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
            };
            crate::aelog!(
                "[PLAYER] stream open: device={device_name:?} rate={rate} ch={ch} \
                 subfmt={subfmt_str} bits={store} valid={valid} blockalign={ba} \
                 packer={fmt:?} mode={mode_str}"
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
        // Pre-fill one period of silence before starting.
        let silence = vec![0u8; buf as usize * blockalign];
        render
            .write_to_device(buf as usize, &silence, None)
            .map_err(|e| e.to_string())?;
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
        let period = client.get_device_period().map(|(d, _)| d).unwrap_or(100_000);
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

            // Aim for slightly above the minimum period; 128-byte alignment for
            // Intel HDA and similar devices.
            let desired_period = 3 * min_period / 2;
            let aligned_period = match client_probe
                .calculate_aligned_period_near(desired_period, Some(128), &actual)
            {
                Ok(p) => p,
                Err(_) => def_period,
            };

            // is_supported_exclusive_with_quirks checked format support but did
            // not call Initialize; we need a fresh client for initialization.
            drop(client_probe);

            match init_exclusive(&device, &actual, aligned_period) {
                Ok((client, buf)) => {
                    return finish_exclusive(
                        client, buf, &actual, device_name, cfg.rate, &cfg.mode,
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
                let period = device
                    .get_iaudioclient()
                    .and_then(|c| c.get_device_period())
                    .map(|(def, _)| def);
                match period.and_then(|p| init_exclusive(&device, first, p)) {
                    Ok((client, buf)) => {
                        crate::aelog!(
                            "[PLAYER] {} refused every format in IsFormatSupported but accepted {} at Initialize",
                            device_name,
                            format_label(first)
                        );
                        return finish_exclusive(
                            client, buf, first, device_name, cfg.rate, &cfg.mode,
                        );
                    }
                    Err(e) => exclusive_err = Some(e),
                }
            }
        }

        // All exclusive attempts failed.
        if !cfg.allow_shared {
            let msg = exclusive_err
                .as_ref()
                .map(|e| exclusive_err_msg(e, cfg.rate, &device_name))
                .unwrap_or_else(|| format!("No format accepted by the device at {} Hz", cfg.rate));
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
            };
            crate::aelog!(
                "[PLAYER] stream open (shared): device={device_name:?} rate={} ch=2 \
                 subfmt=IEEE_FLOAT/WAVEFORMATEXTENSIBLE bits=32 valid=32 blockalign={} \
                 packer=Float32 mode={mode_str}",
                cfg.rate,
                blockalign,
            );
        }

        let silence = vec![0u8; buf as usize * blockalign];
        render
            .write_to_device(buf as usize, &silence, None)
            .map_err(|e| e.to_string())?;

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
        let sh = match open_stream(&cfg) {
            Ok(sh) => sh,
            Err(e) => {
                shared.alive.store(false, Ordering::SeqCst);
                init_tx.send(Err(e)).ok();
                return;
            }
        };

        let StreamHandle { client, render, event, fmt, blockalign, info } = sh;

        // What goes to the device, period by period (`Periods`): DSP mode
        // dithers (and applies the volume), Direct mode does neither.
        let mut periods = Periods::new(info.rate, fmt, blockalign, info.buffer_frames as usize, &cfg.mode, super::test_mute());
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

        // Measured output latency: frames handed to WASAPI minus the device
        // position (IAudioClock), every ~250 ms, smoothed. It replaces the
        // estimate above for the spectrum and the analyzer. What the driver
        // does not report (the DAC's own filter, the speakers) is left to the
        // page's per-device offset.
        let clock = client.get_audioclock().ok()
            .and_then(|c| c.get_frequency().ok().filter(|&f| f > 0).map(|f| (c, f)));
        let mut written: u64 = info.buffer_frames as u64; // the silence pre-fill
        let probe_every = ((info.rate as usize / 4) / buf_frames.max(1)).max(1);
        let mut since_probe = 0usize;
        let mut lat_ema = 0.0f64;

        // ── Render loop ──────────────────────────────────────────────────────
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            match event.wait_for_event(100) {
                Err(_) => {
                    // 100 ms timeout — check stop flag and try again.
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
                Ok(()) => {}
            }

            let avail = match client.get_available_space_in_frames() {
                Ok(n) => n as usize,
                Err(e) => {
                    *shared.last_error.lock().unwrap() =
                        Some(format!("get_available_space_in_frames: {e}"));
                    shared.device_lost.store(true, Ordering::SeqCst);
                    break;
                }
            };

            if avail == 0 {
                continue;
            }

            since_probe += 1;
            if since_probe >= probe_every {
                since_probe = 0;
                if let Some((clk, freq)) = &clock {
                    if let Ok((pos, _)) = clk.get_position() {
                        let played = (pos as u128 * info.rate as u128 / *freq as u128) as u64;
                        if written > played {
                            let lat = (written - played) as f64;
                            lat_ema = if lat_ema == 0.0 { lat } else { lat_ema * 0.8 + lat * 0.2 };
                            shared.device_latency_frames.store(lat_ema.round() as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            // Every period writes exactly `avail` frames (or stops).
            written += avail as u64;

            let (bytes, what) = periods.next(avail, &timeline, &shared);
            if let Err(e) = render.write_to_device(avail, bytes, None) {
                *shared.last_error.lock().unwrap() = Some(format!("write_to_device{what}: {e}"));
                shared.device_lost.store(true, Ordering::SeqCst);
                break;
            }
        }

        client.stop_stream().ok();
        // No stream, no recent peak (the running maximum stays).
        shared.dev_peak_bits.store(0f64.to_bits(), Ordering::Relaxed);
        shared.alive.store(false, Ordering::SeqCst);
        revert_mmcss(mmcss);
    }

    fn revert_mmcss(handle: winapi::um::winnt::HANDLE) {
        if !handle.is_null() {
            unsafe { winapi::um::avrt::AvRevertMmThreadCharacteristics(handle); }
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
