// Off Windows: WASAPI is the only output yet; elsewhere the stream is a stub (stage C).
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]
//! Output device: exclusive (bit-perfect) render through the system's sound
//! API — WASAPI exclusive event-driven with shared-mode fallback on Windows.
//!
//! What depends on the system is one `Backend` (`backend_*.rs`): the device
//! list, the format negotiation, the device's stream. Everything else is
//! shared: the formats asked for (`formats_for`), the periods (`period`) and
//! the audio thread (`audio_thread`). The audio thread owns every device
//! handle. It opens the stream and reports the result back through a one-shot
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
    Active::list_devices()
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
    /// The hold's fade-out in frames (0 = the default, `fade::FADE_MS`).
    /// Reset to 0 by stop_stream.
    pub hold_ramp_frames: AtomicU64,
    /// How the next start of the music fades in — after a jump, or a new
    /// stream's first sound: true, mid-track (a seek, a rack change, a
    /// switch), over `fade::FADE_MS`; false, a track from its beginning,
    /// a short ramp (`fade::SHORT_MS`) that keeps its attack. Written by the
    /// render thread before it posts the jump (or starts the chain).
    pub fade_in_long: AtomicBool,
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
            fade_in_long: AtomicBool::new(false),
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

/// Linux: how long a stop waits for the audio thread, and an open for the
/// device to be initialised. A USB audio driver can hang in the kernel for
/// good — closing a stream whose transfers the host controller never hands
/// back (VMware's virtual USB 2 with Mojo 2, 26.10) — and the thread with
/// it: the stop waited for it for ever, and the controller with it — no
/// Play, Stop or Seek did anything after. Now the thread is left behind and
/// the player goes on (the card itself is refused until it is free:
/// `alsa_logic::Closing`). A normal stop is over in well under a second
/// (`period::close`), an open in tens of milliseconds.
#[cfg(any(target_os = "linux", test))]
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(target_os = "linux")]
const OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Join `h` if it ends within `max`; false: it is left running.
#[cfg(any(target_os = "linux", test))]
fn join_within(h: JoinHandle<()>, max: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + max;
    while !h.is_finished() {
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.join().ok();
    true
}

/// The audio thread's report on the device, waited for up to `max`; in time
/// or not, in the listener's words.
#[cfg(any(target_os = "linux", test))]
fn init_within(rx: &mpsc::Receiver<Result<StreamInfo, String>>, max: std::time::Duration) -> Result<StreamInfo, String> {
    match rx.recv_timeout(max) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "The device did not answer within {} s while opening - the system's USB audio driver is stuck on it? \
             Unplug the DAC and plug it in again, or restart the computer.",
            max.as_secs()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("Audio thread exited before reporting init result".to_string()),
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
                audio_thread::<Active>(cfg, timeline, shared_clone, stop_clone, go_clone, init_tx);
            })
            .map_err(|e| format!("Failed to spawn audio thread: {e}"))?;

        #[cfg(not(target_os = "linux"))]
        let info = init_rx
            .recv()
            .map_err(|_| "Audio thread exited before reporting init result".to_string())??;
        #[cfg(target_os = "linux")]
        let info = init_within(&init_rx, OPEN_WAIT).inspect_err(|e| {
            // Never started if it ever gets out (`wait_to_start`).
            stop_flag.store(true, Ordering::SeqCst);
            crate::aelog!("[PLAYER] stream open failed: {e}");
        })?;

        Ok(OutputStream { handle: Some(handle), stop_flag, go, info })
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
            #[cfg(not(target_os = "linux"))]
            h.join().ok();
            #[cfg(target_os = "linux")]
            if !join_within(h, STOP_WAIT) {
                crate::aelog!(
                    "[PLAYER] the audio thread did not end within {} s: the device's close hangs in the system \
                     (its USB audio driver) - left behind, the player goes on",
                    STOP_WAIT.as_secs()
                );
            }
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

    /// A stop does not wait for an audio thread stuck in the kernel past its
    /// time: the thread is left behind. One that ends in time is joined.
    #[test]
    fn a_stuck_audio_thread_is_left_behind() {
        use super::{join_within, STOP_WAIT};
        use std::time::{Duration, Instant};
        assert!(STOP_WAIT >= Duration::from_secs(1), "a normal close fits");
        let quick = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(20)));
        assert!(join_within(quick, Duration::from_secs(2)));
        let stuck = Arc::new(AtomicBool::new(true));
        let s = stuck.clone();
        let h = std::thread::spawn(move || {
            while s.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let t = Instant::now();
        assert!(!join_within(h, Duration::from_millis(50)), "left behind");
        assert!(t.elapsed() < Duration::from_secs(1));
        stuck.store(false, Ordering::Release);
    }

    /// An open whose device never answers is refused in words, in time.
    #[test]
    fn an_open_that_never_answers_is_refused() {
        use super::{init_within, StreamInfo};
        use std::time::Duration;
        let (tx, rx) = std::sync::mpsc::channel::<Result<StreamInfo, String>>();
        let e = init_within(&rx, Duration::from_millis(20)).unwrap_err();
        assert!(e.contains("did not answer") && e.contains("Unplug"), "{e}");
        tx.send(Err("refused".into())).unwrap();
        assert_eq!(init_within(&rx, Duration::from_millis(20)).unwrap_err(), "refused");
        drop(tx);
        assert!(init_within(&rx, Duration::from_millis(20)).unwrap_err().contains("exited"));
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
    Active::probe_formats(device_id, rates)
}

/// A plain-text report of what the device accepts in exclusive mode and why
/// it refuses what it refuses (`aura-player --probe-device`).
pub fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
    Active::diagnose(device_id, rates)
}


// ── One device period, without a device ──────────────────────────────────────

pub mod fade;
pub use fade::{fade_frames, FADE_MS};

#[cfg_attr(not(windows), allow(dead_code))]
mod period;
use period::*;
// CoreAudio (macOS): the HAL side of `backend_coreaudio`.
mod coreaudio;

/// Give the devices back as they were before the app took them (the app
/// closing): CoreAudio's nominal rates still waiting for their grace.
/// Nothing to do on the other systems.
pub fn restore_devices() {
    #[cfg(target_os = "macos")]
    coreaudio::restore_now();
}

// ── The formats a stream asks for, on every system ───────────────────────────

/// A device sample format as the stream asks for it: `store` bits a sample
/// takes, `valid` of them carry the signal (24 in 32: the top 24 bits), an
/// integer or a float. Interleaved stereo, no padding between samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmtSpec {
    pub store: u16,
    pub valid: u16,
    pub float: bool,
}

const fn int(store: u16, valid: u16) -> FmtSpec {
    FmtSpec { store, valid, float: false }
}

const I24_IN_32: FmtSpec = int(32, 24);
const I32: FmtSpec = int(32, 32);
const I24: FmtSpec = int(24, 24);
const I16: FmtSpec = int(16, 16);
const FLOAT32: FmtSpec = FmtSpec { store: 32, valid: 32, float: true };

// The backends that pick the format themselves (ALSA, CoreAudio) use these.
#[cfg_attr(windows, allow(dead_code))]
impl FmtSpec {
    /// The packer that writes this format (`pack_frames`).
    fn packer(&self) -> SampleFmt {
        match (self.float, self.store) {
            (true, _) => SampleFmt::Float32,
            (false, 16) => SampleFmt::Int16,
            (false, 24) => SampleFmt::Int24Packed,
            _ => SampleFmt::Int32,
        }
    }

    /// Its name in the player's words: "int32 (24-in-32)", "float32", …
    pub fn label(&self) -> String {
        match (self.float, self.store, self.valid) {
            (true, _, _) => "float32".into(),
            (false, 16, 16) => "int16".into(),
            (false, 24, 24) => "int24 (packed)".into(),
            (false, 32, 24) => "int32 (24-in-32)".into(),
            (false, 32, 32) => "int32".into(),
            (false, s, v) => format!("int{v} (in {s})"),
        }
    }
}

/// DSP-mode format priority: 24-bit-in-32 → 32-bit int → float32.
pub fn dsp_formats() -> &'static [FmtSpec] {
    &[I24_IN_32, I32, FLOAT32]
}

/// DoP's format priority: a 24-bit integer word, 24 in 32 → 24 packed →
/// 32/32 (the DoP word in its top 24 bits). Never float, never 16-bit.
pub fn dop_formats() -> &'static [FmtSpec] {
    &[I24_IN_32, I24, I32]
}

/// Direct-mode format priority by source bit depth (bit-perfect intent).
/// 0, a source of no integer depth (a lossy decoder's output, f32 in the
/// decoder): the widest integer the device takes — 32, 24 in 32, 24, 16.
pub fn direct_formats(source_bits: u32) -> &'static [FmtSpec] {
    match source_bits {
        0 => &[I32, I24_IN_32, I24, I16],
        16 => &[I16, I24_IN_32, I32],
        24 => &[I24_IN_32, I24, I32],
        _ /* 32 */ => &[I32],
    }
}

/// The formats a stream in `mode` asks for, in priority order.
pub fn formats_for(mode: &OutputMode) -> &'static [FmtSpec] {
    match mode {
        OutputMode::Dsp => dsp_formats(),
        OutputMode::Direct { source_bits } => direct_formats(*source_bits),
        OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. } => dop_formats(),
    }
}

/// The mode in the stream-open log line.
pub fn mode_label(mode: &OutputMode) -> String {
    match mode {
        OutputMode::Dsp => "DSP".to_string(),
        OutputMode::Direct { source_bits: 0 } => "Direct(no integer depth: the widest)".to_string(),
        OutputMode::Direct { source_bits } => {
            format!("Direct({}bit)", source_bits)
        }
        OutputMode::Dsd { rate, modulator } => {
            format!("DSD{}/{} Hz DoP, {modulator:?}", rate.multiple, rate.base)
        }
        OutputMode::DsdDirect { rate } => format!("DSD{}/{} Hz DoP, the file as it is", rate.multiple, rate.base),
    }
}

// ── The backend: one system's sound API ──────────────────────────────────────

/// One system's output API (WASAPI; ALSA and CoreAudio to come). Everything
/// else — the periods, volume and dither, DoP, the gate, the close, the
/// audio thread's order of things (`audio_thread`) — is the same on every
/// system. All calls but the listings run on the audio thread.
trait Backend {
    type Stream: Stream;
    /// The render endpoints the UI offers.
    fn list_devices() -> Result<Vec<DeviceInfo>, String>;
    /// Per rate, the first DSP-mode format the device accepts (or
    /// "unsupported"); nothing is opened.
    fn probe_formats(device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String>;
    /// `--probe-device`: what the device accepts and why it refuses the rest.
    fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String>;
    /// Open and initialise the device for `cfg` (`formats_for` its mode,
    /// in order), not started. The error is what the listener is told; a
    /// DoP stream the device has no 24-bit integer format for fails with
    /// `DOP_FORMAT_ERR` first (the controller then plays PCM).
    fn open(cfg: &OutputConfig) -> Result<Opened<Self::Stream>, String>;
}

/// A device stream `Backend::open` opened: the packer, bytes per frame and
/// what the controller is told of it.
struct Opened<S> {
    stream: S,
    fmt: SampleFmt,
    blockalign: usize,
    info: StreamInfo,
}

/// An opened device stream, on the audio thread.
trait Stream {
    /// The render loop's device (`period::Device`), once started.
    type Dev<'a>: Device
    where
        Self: 'a;
    /// Hand it `frames` of silence before the start.
    fn prefill(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String>;
    /// Start it; the error is the stream's last error.
    fn start<'a>(&'a mut self, shared: &'a OutputShared) -> Result<Self::Dev<'a>, String>;
    /// Stop it, after the render loop.
    fn stop(&mut self);
}

#[cfg(windows)]
mod backend_wasapi;
#[cfg(windows)]
type Active = backend_wasapi::WasapiOut;

#[cfg(any(target_os = "linux", test))]
mod alsa_logic;
#[cfg(target_os = "linux")]
mod backend_alsa;
#[cfg(target_os = "linux")]
type Active = backend_alsa::AlsaOut;

#[cfg(target_os = "macos")]
mod backend_coreaudio;
#[cfg(target_os = "macos")]
type Active = backend_coreaudio::CoreAudioOut;
#[cfg(all(test, target_os = "macos"))]
mod coreaudio_loopback;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod backend_none;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
type Active = backend_none::NoOutput;

/// The audio thread: opens the stream, reports the result, then the render
/// loop — in the same order on every system.
fn audio_thread<B: Backend>(
    cfg: OutputConfig,
    timeline: Arc<Timeline>,
    shared: Arc<OutputShared>,
    stop: Arc<AtomicBool>,
    go: Arc<AtomicBool>,
    init_tx: mpsc::Sender<Result<StreamInfo, String>>,
) {
    use crate::audio::os_thread::{enter_pro_audio, leave_pro_audio};

    let fail = |e: String| {
        shared.alive.store(false, Ordering::SeqCst);
        init_tx.send(Err(e)).ok();
    };
    let Opened { mut stream, fmt, blockalign, info } = match B::open(&cfg) {
        Ok(o) => o,
        Err(e) => return fail(e),
    };

    // What goes to the device, period by period (`Periods`): DSP mode
    // dithers (and applies the volume), Direct mode does neither, DSD
    // output modulates and packs DoP.
    let mut periods = match Periods::new(info.rate, fmt, blockalign, info.buffer_frames as usize, &cfg.mode, test_mute()) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    // One period of silence before the start (DoP: DSD silence, markers
    // running on into the first period).
    let prefill = periods.prefill(info.buffer_frames as usize);
    if let Err(e) = stream.prefill(info.buffer_frames as usize, &prefill) {
        return fail(e);
    }
    shared.dsd_overloads.store(0, Ordering::Relaxed);
    shared.dsd_guard_tripped.store(false, Ordering::Relaxed);
    shared.dsd_opened(periods.dsd_delay(), periods.dsd_kind());
    if test_mute() {
        crate::aelog!("[PLAYER] test mute: AURA_TEST_MUTE=1 - the device gets silence, levels are measured before it");
    }

    // Report success — start() on the spawning side unblocks here.
    init_tx.send(Ok(info.clone())).ok();

    // ── Real-time scheduling (MMCSS "Pro Audio" on Windows) ──────────────────
    let mmcss = enter_pro_audio();

    // Export device latency so the spectrum analyser can sync its window.
    shared.device_latency_frames.store(info.latency_frames as u64, Ordering::Relaxed);

    // A gated start (`OutputStream::start_gated`): the device is open and
    // initialised; the stream starts when the controller lets it, once
    // its pre-roll is rendered — or never, stopped first. Until then
    // nothing is read from the timeline and nothing written, so the
    // first period after it is the first a stream opened then played.
    if !wait_to_start(&go, &stop) {
        shared.alive.store(false, Ordering::SeqCst);
        leave_pro_audio(mmcss);
        return;
    }
    crate::player::output_tap::open(info.rate);

    let mut dev = match stream.start(&shared) {
        Ok(d) => d,
        Err(msg) => {
            *shared.last_error.lock().unwrap() = Some(msg);
            shared.alive.store(false, Ordering::SeqCst);
            leave_pro_audio(mmcss);
            return;
        }
    };
    if let Err(e) = run(&mut dev, &mut periods, &timeline, &shared, &stop) {
        *shared.last_error.lock().unwrap() = Some(e);
        shared.device_lost.store(true, Ordering::SeqCst);
    }
    drop(dev);

    stream.stop();
    // No stream, no recent peak (the running maximum stays).
    shared.dev_peak_bits.store(0f64.to_bits(), Ordering::Relaxed);
    shared.alive.store(false, Ordering::SeqCst);
    leave_pro_audio(mmcss);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── Non-ignored: run without audio hardware ───────────────────────────────

    /// Every format a stream asks for has its packer and its name.
    #[test]
    fn the_formats_asked_for_pack_and_read_as_they_are() {
        let all = [dsp_formats(), dop_formats(), direct_formats(0), direct_formats(16), direct_formats(24), direct_formats(32)];
        for spec in all.iter().flat_map(|l| l.iter()) {
            let (packer, label) = (spec.packer(), spec.label());
            let want = match (spec.float, spec.store, spec.valid) {
                (true, 32, 32) => (SampleFmt::Float32, "float32"),
                (false, 32, 24) => (SampleFmt::Int32, "int32 (24-in-32)"),
                (false, 32, 32) => (SampleFmt::Int32, "int32"),
                (false, 24, 24) => (SampleFmt::Int24Packed, "int24 (packed)"),
                (false, 16, 16) => (SampleFmt::Int16, "int16"),
                _ => panic!("unexpected {spec:?}"),
            };
            assert_eq!((packer, label.as_str()), want);
        }
        // DoP: 24-bit integer words only.
        assert!(dop_formats().iter().all(|s| !s.float && s.valid >= 24));
        let dop = OutputMode::DsdDirect { rate: DsdRate::new(44_100, 64).unwrap() };
        assert_eq!(formats_for(&dop), dop_formats());
        assert_eq!(formats_for(&OutputMode::Direct { source_bits: 16 }), direct_formats(16));
    }

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
