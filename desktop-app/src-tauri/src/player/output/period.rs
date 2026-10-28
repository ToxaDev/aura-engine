//! What the audio thread hands the device, period by period: the timeline
//! read (or not, while held or paused), the ramps, the volume and dither (DSP
//! mode) or none of them (Direct), the packing, the guard and the test mute.
//! No device in here: every way a period is written runs in the tests too,
//! and every system's backend shares it.

use super::*;
use super::fade::{short_frames, Envelope};
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
    /// The fade (`fade::Envelope`): out on a hold, a pause, the close; in
    /// after a jump, a resume, the render running dry; 1 between.
    env: Envelope,
    /// `fade::FADE_MS` in frames.
    long: usize,
    /// The music's next start has its fade-in to come: a new stream, a
    /// jump. Mid-track (`OutputShared::fade_in_long`) it is a whole fade,
    /// from a track's beginning the short ramp.
    start_pending: bool,
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
            // Silent until the music's start: a stream used to begin at
            // full level, a thump.
            env: Envelope::silent(),
            long: fade_frames(rate),
            start_pending: true,
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
        // Hold (track switch / seek / rack change): fade out over the
        // music still on its way, wait for a jump frame from the render
        // thread, then teleport the read pointer there and fade back in.
        // The hold flag is set by the command thread before any
        // preparation starts; the render places the jump past the fade's
        // reach (`render::jump_at`), so no new audio leaks into it.
        let held = shared.hold.load(Ordering::Acquire);
        if held && self.raw() && !self.env.is_silent() {
            // DSD as it is: no fade — a period of DSD silence in its
            // place, the period read and dropped as a fade's would be.
            shared.ramp_frames_read.store(0, Ordering::Relaxed);
            let n_read = self.read(timeline, avail, avail);
            shared.ramp_frames_read.fetch_add(n_read as u64, Ordering::Relaxed);
            self.env.cut();
            return (self.silence(nbytes, avail, shared), "");
        }
        if held {
            if !self.env.is_silent() {
                if !self.env.fading_out() {
                    shared.ramp_frames_read.store(0, Ordering::Relaxed);
                    let rf = shared.hold_ramp_frames.load(Ordering::Acquire);
                    self.env.fade_out(if rf == 0 { self.long } else { rf as usize });
                }
                // The jump already posted: the fade reads none of the new
                // music after it — it is over by then.
                let jf = shared.jump_frame.load(Ordering::Acquire);
                let mut n = avail;
                if jf != u64::MAX {
                    let left = jf.saturating_sub(timeline.read_pos()) as usize;
                    self.env.end_within(left);
                    n = n.min(left);
                }
                let n_read = self.read(timeline, n, avail);
                shared.ramp_frames_read.fetch_add(n_read as u64, Ordering::Relaxed);
                // The old music ran out: nothing more of it to fade (and
                // what the render writes next is the new).
                self.fade_period(avail, n_read);
                // Heard like any other period: at the listener's volume.
                self.deliver(nbytes, avail, shared);
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
            self.start_pending = true;
            // Fall through to the normal read path this period.
        }

        if shared.paused.load(Ordering::Relaxed) {
            if self.raw() {
                // DSD as it is: DSD silence at once, nothing read.
                self.env.cut();
                return (self.silence(nbytes, avail, shared), " (pause)");
            }
            // A fade over the music on its way, then silence without
            // advancing the timeline.
            if !self.env.is_silent() {
                if !self.env.fading_out() {
                    self.env.fade_out(self.long);
                }
                let n_read = self.read(timeline, avail, avail);
                self.fade_period(avail, n_read);
                self.deliver(nbytes, avail, shared);
                return (&self.out_bytes[..nbytes], "");
            }
            return (self.silence(nbytes, avail, shared), " (pause)");
        }

        // Pre-dither f64 frames from the timeline; what it does not hold
        // yet (the render thread behind) is silence, and counted.
        let n_read = self.read(timeline, avail, avail);
        if n_read < avail {
            timeline.note_underrun(avail - n_read);
        }
        for i in 0..avail {
            self.ch_l[i] = self.interleaved[2 * i];
            self.ch_r[i] = self.interleaved[2 * i + 1];
        }
        if self.raw() {
            // DSD as it is: the words go out whole — no fade, no level
            // (they are not samples), no tap.
            self.env.set_full();
            self.start_pending = false;
            self.deliver(nbytes, avail, shared);
            return (&self.out_bytes[..nbytes], "");
        }
        // The fade-in: the music's start (short from a track's beginning,
        // whole mid-track), or its coming back after a pause, a fade
        // turned round, the render running dry. Nothing read, nothing
        // starts: the start waits for the music.
        if n_read > 0 {
            let starting = self.start_pending;
            if starting {
                let long = shared.fade_in_long.load(Ordering::Acquire);
                self.env.fade_in(if long { self.long } else { short_frames(self.rate, avail) });
                self.start_pending = false;
            } else if !self.env.is_full() && !self.env.fading_in() {
                self.env.fade_in(self.long);
            }
            self.env.apply(&mut self.ch_l[..n_read], &mut self.ch_r[..n_read]);
            if n_read < avail {
                // Ran dry (the rest is silence already): it comes back
                // with a fade-in — the start's own, if it was the start.
                self.env.cut();
                self.start_pending = starting;
            }
        } else if !self.env.is_silent() {
            self.env.cut();
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

    /// A period of `avail` frames into `interleaved`, `n` of them at most
    /// from the timeline; the frames it does not hold read as silence.
    /// Returns how many it held.
    fn read(&mut self, timeline: &Timeline, n: usize, avail: usize) -> usize {
        self.read_from = timeline.read_pos();
        let n_read = timeline.read_into(&mut self.interleaved[..n * 2], n);
        for s in &mut self.interleaved[n_read * 2..avail * 2] {
            *s = 0.0;
        }
        n_read
    }

    /// The period read (`n_read` frames of music, silence after) into
    /// `ch_l`/`ch_r` under the fade; where the music ran out the fade is
    /// over — silent.
    fn fade_period(&mut self, avail: usize, n_read: usize) {
        for i in 0..avail {
            self.ch_l[i] = self.interleaved[2 * i];
            self.ch_r[i] = self.interleaved[2 * i + 1];
        }
        self.env.apply(&mut self.ch_l[..n_read], &mut self.ch_r[..n_read]);
        if n_read < avail {
            self.env.cut();
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
    /// where the fade stands, over `fade::FADE_MS`, and heard like any
    /// other — at the listener's volume, dithered, guarded; then silence,
    /// nothing read. Held or paused, it is silence already.
    fn closing(&mut self, avail: usize, timeline: &Timeline, shared: &OutputShared) -> &[u8] {
        let nbytes = avail * self.blockalign;
        // Stopped in DoP's lead-in: no music went out yet, none goes now.
        let lead_in = self.dop.as_ref().is_some_and(|d| d.lead_in_left > 0);
        // A DSD file as it is stops on DSD silence: no fade.
        if self.env.is_silent() || lead_in || self.raw() {
            return self.silence(nbytes, avail, shared);
        }
        if !self.env.fading_out() {
            self.env.fade_out(self.long);
        }
        let n_read = self.read(timeline, avail, avail);
        self.fade_period(avail, n_read);
        self.deliver(nbytes, avail, shared);
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
/// client (`backend_wasapi`), a stand-in in the tests.
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
/// asking does not hold the stop up past the fade's and the tail's time
/// and a quarter second; a device gone meanwhile ends it — nothing is left
/// to hear.
fn close(dev: &mut impl Device, periods: &mut Periods, timeline: &Timeline, shared: &OutputShared) {
    let tail = periods.close_tail(shared.device_latency_frames.load(Ordering::Relaxed));
    let until = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(tail as f64 / periods.rate.max(1) as f64 + FADE_MS / 1000.0)
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
        let (l, r) = loud(120 * avail);
        let tl = timeline(rate, &l, &r);
        let sh = OutputShared::new();
        let mut p = Periods::new(rate, SampleFmt::Int16, 4, avail, &OutputMode::Direct { source_bits: 16 }, false).unwrap();
        let sample = |bytes: &[u8], i: usize| {
            let (a, b) = (i16::from_le_bytes([bytes[4 * i], bytes[4 * i + 1]]), i16::from_le_bytes([bytes[4 * i + 2], bytes[4 * i + 3]]));
            (a as f64 / 32_768.0, b as f64 / 32_768.0)
        };
        // The stream's start: the short ramp (10 ms, this period), from
        // near silence up to the music itself.
        let (bytes, what) = p.next(avail, &tl, &sh);
        assert_eq!(what, "");
        assert!(sample(bytes, 0).0.abs() < 1e-3);
        assert_eq!(sample(bytes, avail - 1), (l[avail - 1], r[avail - 1]));
        // Then the music to the bit.
        let (bytes, _) = p.next(avail, &tl, &sh);
        for i in 0..avail {
            assert_eq!(sample(bytes, i), (l[avail + i], r[avail + i]), "frame {}", avail + i);
        }
        assert!(sh.sent_peak_max_db().is_some_and(|db| db > -0.01), "{:?}", sh.sent_peak_max_db());
        // Held: a fade of FADE_MS (25 periods), then silence that reads
        // nothing.
        sh.hold.store(true, Ordering::Release);
        let mut fade = 0;
        let at = loop {
            let at = tl.read_pos();
            let (bytes, what) = p.next(avail, &tl, &sh);
            if what == " (hold)" {
                assert!(bytes.iter().all(|&b| b == 0));
                break at;
            }
            fade += 1;
        };
        assert_eq!((fade, tl.read_pos()), (25, at));
        assert_eq!(at, 27 * avail as u64);
        // The jump: from its frame on, a track's start (the short ramp).
        sh.jump_frame.store(50 * avail as u64, Ordering::Release);
        p.next(avail, &tl, &sh);
        assert_eq!(tl.read_pos(), 51 * avail as u64);
        assert!(!sh.hold.load(Ordering::Acquire));
        let (bytes, _) = p.next(avail, &tl, &sh);
        assert_eq!(sample(bytes, 0), (l[51 * avail], r[51 * avail]));
        // Paused: a fade of FADE_MS, then silence that reads nothing.
        sh.paused.store(true, Ordering::Relaxed);
        for _ in 0..25 {
            assert_eq!(p.next(avail, &tl, &sh).1, "");
        }
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

    // ── The fades at a jump, a run dry and the close ─────────────────────

    /// Direct mode to a float device: no volume, no dither — over a steady
    /// 0.5 a frame's level is the envelope's gain itself.
    fn direct_float(rate: u32, avail: usize) -> Periods {
        Periods::new(rate, SampleFmt::Float32, 8, avail, &OutputMode::Direct { source_bits: 24 }, false).unwrap()
    }

    fn steady(frames: usize) -> (Vec<f64>, Vec<f64>) {
        (vec![0.5; frames], vec![0.5; frames])
    }

    /// The left channel's gain over a steady 0.5, frame by frame.
    fn gains(bytes: &[u8]) -> Vec<f64> {
        bytes.chunks(8).map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]) as f64 / 0.5).collect()
    }

    /// `n` periods one after another, their gains.
    fn play(p: &mut Periods, tl: &Timeline, sh: &OutputShared, avail: usize, n: usize) -> Vec<f64> {
        (0..n).flat_map(|_| gains(p.next(avail, tl, sh).0)).collect()
    }

    /// A fade-in over `frames`: from near silence, rising, halfway at half
    /// of them, full from their end on.
    fn faded_in_over(g: &[f64], frames: usize, what: &str) {
        assert!(g[0].abs() < 0.01, "{what}: starts at {}", g[0]);
        assert!(g.windows(2).all(|w| w[1] >= w[0]), "{what}: not rising");
        let half = g.iter().position(|&x| x >= 0.5).unwrap();
        assert!(half.abs_diff(frames / 2) <= 2, "{what}: halfway at {half}, want {}", frames / 2);
        assert!(g[frames..].iter().all(|&x| x == 1.0), "{what}: not full after {frames} frames");
    }

    /// After a jump the music fades in whole mid-track (a seek, a rack
    /// change: `fade_in_long`), and on the short ramp from a track's start.
    #[test]
    fn after_a_jump_the_music_fades_in_whole_mid_track_and_on_the_short_ramp_from_a_start() {
        let (rate, avail) = (44_100, 441);
        let (l, r) = steady(120 * avail);
        for long in [true, false] {
            let tl = timeline(rate, &l, &r);
            let sh = OutputShared::new();
            let mut p = direct_float(rate, avail);
            play(&mut p, &tl, &sh, avail, 2);
            sh.hold.store(true, Ordering::Release);
            let mut fade = 0;
            while p.next(avail, &tl, &sh).1 != " (hold)" {
                fade += 1;
                assert!(fade < 100, "the hold never went silent");
            }
            sh.fade_in_long.store(long, Ordering::Release);
            sh.jump_frame.store(60 * avail as u64, Ordering::Release);
            let g = play(&mut p, &tl, &sh, avail, 30);
            assert_eq!(tl.read_pos(), 90 * avail as u64);
            let n = if long { fade_frames(rate) } else { short_frames(rate, avail) };
            faded_in_over(&g, n, if long { "mid-track" } else { "a track's start" });
        }
    }

    /// The render running dry mid-period: what was read plays at its level,
    /// the rest is silence (counted), no fade-out over music that is not
    /// there; when the music is back it fades in whole.
    #[test]
    fn music_run_dry_is_cut_and_comes_back_with_a_whole_fade_in() {
        let (rate, avail) = (44_100, 441);
        let (l, r) = steady(2 * avail + 220);
        let tl = timeline(rate, &l, &r);
        let sh = OutputShared::new();
        let mut p = direct_float(rate, avail);
        let g = play(&mut p, &tl, &sh, avail, 3);
        assert!(g[avail..2 * avail + 220].iter().all(|&x| x == 1.0), "the music before it ran dry");
        assert!(g[2 * avail + 220..].iter().all(|&x| x == 0.0), "silence after it");
        assert_eq!(tl.underrun_frames(), 221);
        let (l, r) = steady(40 * avail);
        tl.append(&l, &r);
        let g = play(&mut p, &tl, &sh, avail, 30);
        faded_in_over(&g, fade_frames(rate), "back after running dry");
    }

    /// A stream's start with nothing to read yet waits for the music, and
    /// keeps its short ramp; a start that runs dry halfway starts again on
    /// it when the music is back.
    #[test]
    fn a_start_with_nothing_to_read_yet_waits_and_keeps_its_short_ramp() {
        let (rate, avail) = (44_100, 441);
        let tl = Timeline::new(rate, 4.0);
        let sh = OutputShared::new();
        let mut p = direct_float(rate, avail);
        let g = play(&mut p, &tl, &sh, avail, 2);
        assert!(g.iter().all(|&x| x == 0.0));
        assert_eq!(tl.underrun_frames(), 2 * avail as u64);
        let (l, r) = steady(100);
        tl.append(&l, &r);
        let g = play(&mut p, &tl, &sh, avail, 1);
        assert!(g[99] > 0.0 && g[99] < 0.5, "the ramp begun: {}", g[99]);
        assert!(g[100..].iter().all(|&x| x == 0.0));
        let (l, r) = steady(40 * avail);
        tl.append(&l, &r);
        let g = play(&mut p, &tl, &sh, avail, 3);
        faded_in_over(&g, short_frames(rate, avail), "the start again");
    }

    /// The close fades the music out over a whole fade (25 periods at
    /// 44.1 kHz), smoothly, before the silence that ends the stream.
    #[test]
    fn the_close_fades_out_over_a_whole_fade() {
        let (rate, avail) = (44_100, 441);
        let (l, r) = steady(60 * avail);
        let tl = timeline(rate, &l, &r);
        let sh = OutputShared::new();
        let mut p = direct_float(rate, avail);
        let mut dac = Dac::new(avail, 8, 10);
        let stop = dac.stop.clone();
        run(&mut dac, &mut p, &tl, &sh, &stop).unwrap();
        let g = gains(&dac.sent);
        let after = &g[10 * avail..];
        let n = fade_frames(rate);
        assert_eq!(n, 25 * avail);
        assert!(after[0] > 0.99, "the fade starts at full: {}", after[0]);
        assert!(after.windows(2).all(|w| w[1] <= w[0]), "not falling");
        let half = after.iter().position(|&x| x <= 0.5).unwrap();
        assert!(half.abs_diff(n / 2) <= 2, "halfway at {half}, want {}", n / 2);
        let last = after.iter().rposition(|&x| x > 0.0).unwrap();
        assert!(last + 3 >= n && last < n, "the fade's last sound at {last}, want {n}");
    }

    /// A hold whose jump is already posted reads none of the new music
    /// past the jump frame: its fade is made steeper to be over there.
    #[test]
    fn a_hold_with_its_jump_posted_fades_out_before_the_jump_frame() {
        let (rate, avail) = (44_100, 441);
        // The old music up to the jump frame, the new (negative) after it.
        let jf = 5 * avail + 100;
        let mut l = vec![0.5; jf];
        l.extend(vec![-0.5; 40 * avail]);
        let tl = timeline(rate, &l, &l);
        let sh = OutputShared::new();
        let mut p = direct_float(rate, avail);
        play(&mut p, &tl, &sh, avail, 2);
        sh.fade_in_long.store(true, Ordering::Release);
        sh.jump_frame.store(jf as u64, Ordering::Release);
        sh.hold.store(true, Ordering::Release);
        let mut g = Vec::new();
        while tl.read_pos() < jf as u64 {
            g.extend(gains(p.next(avail, &tl, &sh).0));
            assert!(g.len() < 10 * avail, "the hold read on");
        }
        assert_eq!(tl.read_pos(), jf as u64, "read past the jump frame");
        let faded = &g[..jf - 2 * avail];
        assert!(faded[0] > 0.99);
        assert!(faded.iter().all(|&x| x >= 0.0), "the new music leaked into the fade");
        assert!(faded.windows(2).all(|w| w[1] <= w[0]), "not falling");
        assert!(*faded.last().unwrap() < 1e-3, "not over by the jump frame: {}", faded.last().unwrap());
        assert!(g[faded.len()..].iter().all(|&x| x == 0.0));
        // The jump: the hold over, the new music faded in whole from there.
        let g = play(&mut p, &tl, &sh, avail, 30);
        assert!(!sh.hold.load(Ordering::Acquire));
        assert_eq!(tl.read_pos(), (jf + 30 * avail) as u64);
        let up: Vec<f64> = g.iter().map(|x| -x).collect();
        faded_in_over(&up, fade_frames(rate), "the new music");
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
            // Then the music, read from the timeline's start, after the
            // start's short ramp.
            let vol = sh.volume_lin();
            let n = 20 * avail;
            let (mut pl, mut pr) = (l[..n].to_vec(), r[..n].to_vec());
            let mut ramp = Envelope::silent();
            ramp.fade_in(short_frames(rate, avail));
            ramp.apply(&mut pl, &mut pr);
            let (pl, pr): (Vec<f64>, Vec<f64>) = (pl.iter().map(|x| x * vol).collect(), pr.iter().map(|x| x * vol).collect());
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
        let (l, r) = music(rate, 90 * avail);
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
            let fade = fade_frames(rate);
            let read_at_pause = {
                let mut v = Vec::new();
                for _ in 0..fade / avail + 20 {
                    let (b, _) = p.next(avail, &tl, &sh);
                    v.push(b.to_vec());
                }
                v
            };
            let read = tl.read_pos() as usize;
            // The fade's rounding may take a period more.
            assert!(read == 30 * avail + fade || read == 31 * avail + fade, "{c:?}: 30 periods and the pause's fade, {read} frames");
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
            let (mut pl, mut pr) = (l[..n].to_vec(), r[..n].to_vec());
            let mut ramp = Envelope::silent();
            ramp.fade_in(short_frames(rate, avail));
            ramp.apply(&mut pl, &mut pr);
            let pl: Vec<f64> = pl.iter().map(|x| x * vol).collect();
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
