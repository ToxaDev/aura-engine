//! How much of the minimum-phase branch a Hybrid-Phase chain plays, along
//! its output: the rack's lamp on HP (and on αHP) lights when it is heard.
//!
//! The blend stage notes its weight on the minimum-phase branch for every
//! output sample it emits (HP: 0 or 1 and the short fade between, αHP: α),
//! and the meter keeps the largest weight of each ~2 ms slot in a ring of
//! atomics — about a minute of output, far more than the render runs ahead.
//! It only reads what the stage computed: the samples are not touched. The
//! controller asks it at the output index heard (`Mark::index`), so the lamp
//! goes with the sound as the bars do.
//!
//! A converted file blends nothing as it plays: the blend is in its samples.
//! Its meter (`for_file`) holds the whole file's slots instead, worked out
//! from the source the way the converter planned the blend (`lamp_plan`), so
//! the lamp lights there too; the file's frame is the render's frame, as the
//! converter's plan counts them.

use std::sync::atomic::{AtomicI8, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::blend::{Envelope, Plan};
use crate::audio::hybrid_phase::{catmull_env_at, gain_smoothed, smooth_analysis_envelope};

/// Slots in the ring: 2^15 of ~2 ms each, ~65 s.
const SLOTS: usize = 1 << 15;
const SLOT_S: f64 = 0.002;
const FULL: f64 = u16::MAX as f64;

/// Which blend a meter watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendKind {
    /// Hybrid-Phase: switched to minimum phase across an attack.
    Hybrid,
    /// Continuous alpha: a per-sample crossfade on the envelope.
    Alpha,
}

/// Where a meter's slots come from, as the lamp route sends it (its third
/// byte): a live blend noting them, or a converted file's timeline and why
/// it is not there yet.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanState {
    /// A live blend notes its weights (or there is no meter).
    Live = 0,
    /// The file's timeline is there.
    Ready = 1,
    /// Being worked out from the source.
    Pending = 2,
    /// The source is not there to work it out from.
    NoSource = 3,
    /// Not enough free memory to decode the source now.
    LowMemory = 4,
    /// A conversion is running: the source is not decoded beside it.
    Converting = 5,
    /// The source could not be decoded or analysed.
    Failed = 6,
}

impl PlanState {
    fn from_u8(v: u8) -> PlanState {
        match v {
            0 => PlanState::Live,
            1 => PlanState::Ready,
            2 => PlanState::Pending,
            3 => PlanState::NoSource,
            4 => PlanState::LowMemory,
            5 => PlanState::Converting,
            _ => PlanState::Failed,
        }
    }
}

pub struct PhaseMeter {
    pub kind: BlendKind,
    rate: f64,
    frames_per_slot: u64,
    /// `(slot + 1) << 16 | weight`; 0: nothing noted there. Empty on a
    /// converted file's meter.
    slots: Box<[AtomicU64]>,
    /// A converted file's weights, one per slot from its first frame.
    plan: OnceLock<Arc<[u16]>>,
    /// `PlanState` as a byte.
    state: AtomicU8,
    /// The onset envelope the blend follows, when it is known (`view`).
    env: OnceLock<Arc<Envelope>>,
    /// The envelope a live blend's plan has made so far (`EnvWriter`): what
    /// `view` draws while the whole one is not there.
    live_env: Mutex<Envelope>,
    /// The Hybrid-Phase sensitivity step the blend runs at, −3..+3 (`view`).
    sens: AtomicI8,
}

impl std::fmt::Debug for PhaseMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseMeter").field("kind", &self.kind).finish_non_exhaustive()
    }
}

impl PhaseMeter {
    pub fn new(kind: BlendKind, rate: f64) -> Arc<PhaseMeter> {
        Arc::new(PhaseMeter {
            kind,
            rate,
            frames_per_slot: frames_per_slot(rate),
            slots: (0..SLOTS).map(|_| AtomicU64::new(0)).collect(),
            plan: OnceLock::new(),
            state: AtomicU8::new(PlanState::Live as u8),
            env: OnceLock::new(),
            live_env: Mutex::new(Envelope { analysis: Vec::new(), analysis_sr: 0.0 }),
            sens: AtomicI8::new(0),
        })
    }

    /// A converted file's meter: dark until its timeline is set (`set_plan`).
    pub fn for_file(kind: BlendKind, rate: f64) -> Arc<PhaseMeter> {
        Arc::new(PhaseMeter {
            kind,
            rate,
            frames_per_slot: frames_per_slot(rate),
            slots: Box::new([]),
            plan: OnceLock::new(),
            state: AtomicU8::new(PlanState::Pending as u8),
            env: OnceLock::new(),
            live_env: Mutex::new(Envelope { analysis: Vec::new(), analysis_sr: 0.0 }),
            sens: AtomicI8::new(0),
        })
    }

    pub fn state(&self) -> PlanState {
        PlanState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Why a file's timeline is not there; once it is, it stays `Ready`.
    pub fn set_state(&self, s: PlanState) {
        if self.plan.get().is_none() {
            self.state.store(s as u8, Ordering::Release);
        }
    }

    /// The whole file's weights, one per slot (`file_timeline`): the lamp
    /// reads them from now on.
    pub fn set_plan(&self, timeline: Arc<[u16]>) {
        let _ = self.plan.set(timeline);
        self.state.store(PlanState::Ready as u8, Ordering::Release);
    }

    /// The onset envelope behind the blend: the rack's background draws it.
    pub fn set_envelope(&self, env: Arc<Envelope>) {
        let _ = self.env.set(env);
    }

    /// The sensitivity step the blend runs at (`hybrid_phase::switch_threshold`):
    /// the rack's background draws the threshold it uses.
    pub fn set_sens(&self, step: i8) {
        self.sens.store(step.clamp(-3, 3), Ordering::Relaxed);
    }

    pub fn sens(&self) -> i8 {
        self.sens.load(Ordering::Relaxed)
    }

    pub fn writer(self: &Arc<Self>) -> MeterWriter {
        MeterWriter { meter: self.clone(), slot: None, max: 0 }
    }

    /// The live plan's end of the meter (`PlanFeed::metered`): the envelope
    /// it makes, from envelope frame `from` on (before it, 0), room made now
    /// for `frames` of it (a file's whole length) so it is not grown as it plays.
    pub fn env_writer(self: &Arc<Self>, from: usize, frames: usize) -> EnvWriter {
        self.live_env.lock().unwrap_or_else(|p| p.into_inner()).analysis.reserve(frames);
        EnvWriter { meter: self.clone(), from }
    }

    /// The lamp route's three bytes at output index `index` (`level` over
    /// `back` frames, 0..255): HP's lamp, αHP's lamp, and the state
    /// (`PlanState`).
    pub fn lamp(&self, index: u64, back: u64) -> [u8; 3] {
        let v = (self.level(index, back) * 255.0).round() as u8;
        let state = self.state() as u8;
        match self.kind {
            BlendKind::Hybrid => [v, 0, state],
            BlendKind::Alpha => [0, v, state],
        }
    }

    /// The largest weight noted from `back` frames before `index` up to it,
    /// 0..1; 0 where nothing was noted (not rendered yet, or lapped).
    pub fn level(&self, index: u64, back: u64) -> f64 {
        let s1 = index / self.frames_per_slot;
        let s0 = index.saturating_sub(back) / self.frames_per_slot;
        self.peak(s0, s1)
    }

    /// The largest weight in slots `s0..=s1`, 0..1.
    fn peak(&self, s0: u64, s1: u64) -> f64 {
        if let Some(plan) = self.plan.get() {
            let end = (s1 as usize).saturating_add(1).min(plan.len());
            let best = plan.get(s0 as usize..end).map_or(0, |w| w.iter().copied().max().unwrap_or(0));
            return best as f64 / FULL;
        }
        if self.slots.is_empty() {
            return 0.0;
        }
        let mut best = 0u64;
        for s in s0..=s1 {
            let v = self.slots[(s % SLOTS as u64) as usize].load(Ordering::Relaxed);
            if v >> 16 == s + 1 {
                best = best.max(v & 0xFFFF);
            }
        }
        best as f64 / FULL
    }

    /// The rack's background (`/player/phase_view`) around output index
    /// `index`: `n` bins of `step` frames on a grid fixed in the output's
    /// frames (so a bin is the same bin in every answer), the first holding
    /// `index + from`. Little endian: the kind (1 HP, 2 αHP), the state
    /// (`PlanState`), 1 when the envelope is there, where `index` falls in
    /// its bin (0..255), `n` (u16), the bin `index` falls in (u16); then per
    /// bin the largest weight (0..255), then per bin the envelope's peak
    /// (0..255 over 0..1, 0 without one); last, one byte: the sensitivity
    /// step the blend runs at (i8, −3..+3) — after the bins, so the fields
    /// before it keep their places and an answer without it still reads;
    /// then the grid bin `index` falls in (u32), so the rack keeps the bins
    /// already heard as it drew them while the step changes.
    /// The envelope is the whole track's when it is there, else what a live
    /// plan has made so far (0 past it): the same values where both are.
    pub fn view(&self, index: u64, from: i64, step: u64, n: usize) -> Vec<u8> {
        let step = step.max(1);
        let n = n.min(u16::MAX as usize);
        let g0 = (index as i64 + from).div_euclid(step as i64);
        let now = (index / step) as i64;
        let frac = ((index % step) * 256 / step) as u8;
        let live;
        let env: Option<&Envelope> = match self.env.get() {
            Some(e) => Some(e),
            None => {
                live = self.live_env.lock().unwrap_or_else(|p| p.into_inner());
                (!live.analysis.is_empty()).then_some(&*live)
            }
        };
        let kind = match self.kind { BlendKind::Hybrid => 1, BlendKind::Alpha => 2 };
        let mut out = Vec::with_capacity(8 + 2 * n + 5);
        out.extend_from_slice(&[kind, self.state() as u8, env.is_some() as u8, frac]);
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&((now - g0).clamp(0, u16::MAX as i64) as u16).to_le_bytes());
        let bins = || (0..n as i64).map(|k| g0 + k).map(|g| (g >= 0).then(|| (g as u64 * step, g as u64 * step + step - 1)));
        let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let fps = self.frames_per_slot;
        out.extend(bins().map(|b| b.map_or(0, |(a, z)| byte(self.peak(a / fps, z / fps)))));
        out.extend(bins().map(|b| match (b, env) {
            (Some((a, z)), Some(e)) => {
                let f2o = self.rate / e.analysis_sr.max(1.0);
                let at = |i: u64| catmull_env_at(&e.analysis, f2o, i as usize) as f64;
                // Every envelope point inside the bin, and its edges.
                let (i0, i1) = ((a as f64 / f2o).ceil() as usize, (z as f64 / f2o).floor() as usize);
                let inner = e.analysis.get(i0..=i1.min(e.analysis.len().saturating_sub(1))).unwrap_or(&[]);
                byte(inner.iter().copied().fold(at(a).max(at(z)), f64::max))
            }
            _ => 0,
        }));
        out.push(self.sens() as u8);
        out.extend_from_slice(&(now.clamp(0, u32::MAX as i64) as u32).to_le_bytes());
        out
    }

    fn store(&self, slot: u64, w: u16) {
        if self.slots.is_empty() {
            return;
        }
        self.slots[(slot % SLOTS as u64) as usize].store((slot + 1) << 16 | w as u64, Ordering::Relaxed);
    }
}

fn frames_per_slot(rate: f64) -> u64 {
    ((rate * SLOT_S) as u64).max(1)
}

/// Slots of `frames_per_slot` covering `total` frames.
fn slot_count(total: usize, frames_per_slot: u64) -> usize {
    (total as u64).div_ceil(frames_per_slot.max(1)) as usize
}

/// HP's timeline: full in every slot a minimum-phase span of `plan` touches
/// (from a boundary to the next, or from the first frame with `use_min_0`),
/// dark elsewhere. The short fades at the edges are not told apart: the
/// lamp shows the switch, as the live meter does to a slot.
pub fn hybrid_timeline(plan: &Plan, total: usize, frames_per_slot: u64) -> Vec<u16> {
    let fps = frames_per_slot.max(1);
    let mut out = vec![0u16; slot_count(total, fps)];
    let mut edges = Vec::with_capacity(plan.boundaries.len() + 1);
    if plan.use_min_0 {
        edges.push(0);
    }
    edges.extend(plan.boundaries.iter().copied());
    for span in edges.chunks(2) {
        let a = span[0].min(total);
        let b = span.get(1).copied().unwrap_or(total).min(total);
        if b <= a {
            continue;
        }
        let (s0, s1) = ((a as u64 / fps) as usize, ((b - 1) as u64 / fps) as usize);
        out[s0..=s1].fill(u16::MAX);
    }
    out
}

/// αHP's timeline: the largest α in each slot, α as the blend stage takes it
/// (`catmull_env_at` on the smoothed envelope), looked at every `step`
/// frames and at each slot's last frame — an α peak is far wider than a
/// step of ~0.25 ms.
pub fn alpha_timeline(smoothed: &[f64], frames_to_output: f64, total: usize, frames_per_slot: u64, step: usize) -> Vec<u16> {
    let fps = frames_per_slot.max(1) as usize;
    let step = step.max(1);
    let q = |i: usize| (catmull_env_at(smoothed, frames_to_output, i).clamp(0.0, 1.0) as f64 * FULL).round() as u16;
    (0..slot_count(total, fps as u64))
        .map(|s| {
            let a = s * fps;
            let last = (a + fps).min(total) - 1;
            (a..last).step_by(step).map(q).fold(q(last), u16::max)
        })
        .collect()
}

/// A converted file's timeline: the blend `kind` the converter ran over it,
/// planned from the source's onset envelope at the file's `rate` over its
/// `total` frames (`blend::scan_plan_sens` for HP, the smoothed α for αHP),
/// at the sensitivity `step` it was converted with, in the slots of a meter
/// at that rate. Step 0 is the timeline it always was.
pub fn file_timeline(kind: BlendKind, env: &Envelope, rate: f64, total: usize, step: i8) -> Vec<u16> {
    let fps = frames_per_slot(rate);
    match kind {
        BlendKind::Hybrid => hybrid_timeline(&super::blend::scan_plan_sens(env, rate, total, step), total, fps),
        BlendKind::Alpha => {
            let mut smoothed = smooth_analysis_envelope(&env.analysis, env.analysis_sr);
            gain_smoothed(&mut smoothed, step);
            let step = ((rate * 0.000_25) as usize).max(1);
            alpha_timeline(&smoothed, rate / env.analysis_sr.max(1.0), total, fps, step)
        }
    }
}

/// A live plan's end of a meter (`PlanFeed::metered`): the onset envelope
/// it makes, frame f at index f, copied as it grows.
pub struct EnvWriter {
    meter: Arc<PhaseMeter>,
    /// Envelope frames before this one are a warm-up, not the track's: 0.
    from: usize,
}

impl EnvWriter {
    /// The envelope `env` (at `analysis_sr`) made so far. It never waits: while
    /// the page reads the meter, the frames are taken on the next call.
    pub fn publish(&mut self, env: &[f64], analysis_sr: f64) {
        let Ok(mut e) = self.meter.live_env.try_lock() else { return };
        let have = e.analysis.len();
        if have >= env.len() {
            return;
        }
        e.analysis_sr = analysis_sr;
        let from = self.from;
        e.analysis.extend(env[have..].iter().enumerate().map(|(k, &v)| if have + k < from { 0.0 } else { v }));
    }
}

/// The blend stage's end of a meter: one writer, the slot it is filling.
pub struct MeterWriter {
    meter: Arc<PhaseMeter>,
    slot: Option<u64>,
    max: u16,
}

impl MeterWriter {
    /// The weight on the minimum-phase branch at output index `index`.
    #[inline]
    pub fn note(&mut self, index: u64, w: f64) {
        let s = index / self.meter.frames_per_slot;
        if self.slot != Some(s) {
            self.flush();
            self.slot = Some(s);
            self.max = 0;
        }
        let q = (w.clamp(0.0, 1.0) * FULL).round() as u16;
        self.max = self.max.max(q);
    }

    /// Publish the slot being filled (at the end of each read).
    pub fn flush(&mut self) {
        if let Some(s) = self.slot {
            self.meter.store(s, self.max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flash_shorter_than_a_frame_lights_a_frame_and_no_more() {
        let m = PhaseMeter::new(BlendKind::Hybrid, 48_000.0);
        let mut w = m.writer();
        // 1 s linear, 1 ms minimum phase, 1 s linear.
        for i in 0..48_000u64 { w.note(i, 0.0); }
        for i in 48_000..48_048u64 { w.note(i, 1.0); }
        for i in 48_048..96_048u64 { w.note(i, 0.0); }
        w.flush();
        // The page looks every 16 ms, each time over the 16 ms since the last.
        let frame = 768;
        let lit: Vec<u64> = (1..120u64).map(|k| k * frame)
            .filter(|&at| m.level(at, frame) > 0.0).collect();
        assert!(!lit.is_empty(), "the flash is not lost between frames");
        assert!(lit.len() <= 2, "a 1 ms flash lights at most the frames its 2 ms slot touches: {lit:?}");
        assert!(lit.iter().all(|&at| at >= 48_000 && at < 48_000 + 2 * frame + 96));
        assert_eq!(m.level(40_000, frame), 0.0, "steady linear is dark");
    }

    #[test]
    fn a_decaying_alpha_reads_its_own_peak_over_the_window() {
        let m = PhaseMeter::new(BlendKind::Alpha, 48_000.0);
        let mut w = m.writer();
        // α jumps to 1 and decays with τ = 30 ms, as the engine's envelope.
        let a = |i: u64| (-(i as f64) / (0.030 * 48_000.0)).exp();
        for i in 0..48_000u64 { w.note(i, a(i)); }
        w.flush();
        for at in [1_000u64, 3_000, 10_000] {
            let got = m.level(at, 768);
            // The peak of the window is α at its start (within a 2 ms slot).
            assert!(got <= a(at.saturating_sub(768 + 96)) + 1e-4 && got >= a(at.saturating_sub(768)) - 1e-4,
                "at {at}: {got}");
        }
        assert!(m.level(30_000, 768) < 0.001, "dark once α has decayed");
    }

    fn plan(boundaries: &[usize], use_min_0: bool) -> Plan {
        Plan { boundaries: boundaries.to_vec(), use_min_0 }
    }

    #[test]
    fn hp_timeline_is_full_on_the_minimum_phase_spans_only() {
        // 96 frames a slot at 48 kHz.
        let t = hybrid_timeline(&plan(&[960, 1_920, 4_800], false), 9_600, 96);
        assert_eq!(t.len(), 100);
        let lit: Vec<usize> = (0..t.len()).filter(|&s| t[s] > 0).collect();
        // 960..1920 → slots 10..=19; 4800..end → slots 50..=99.
        let want: Vec<usize> = (10..20).chain(50..100).collect();
        assert_eq!(lit, want);
        assert!(t.iter().all(|&w| w == 0 || w == u16::MAX));
        // From the first frame, and a span ending mid-slot lights that slot.
        let t = hybrid_timeline(&plan(&[100], true), 1_000, 96);
        assert_eq!(&t[..3], &[u16::MAX, u16::MAX, 0]);
        assert_eq!(t.len(), 11, "a part slot at the end counts");
        assert!(hybrid_timeline(&plan(&[], false), 1_000, 96).iter().all(|&w| w == 0));
    }

    #[test]
    fn alpha_timeline_peaks_at_the_smoothed_alpha() {
        // An envelope at 100 Hz with one attack, at 48 kHz output.
        let mut analysis = vec![0.0; 200];
        analysis[50] = 1.0;
        let env = Envelope { analysis, analysis_sr: 100.0 };
        let total = 96_000;
        let t = file_timeline(BlendKind::Alpha, &env, 48_000.0, total, 0);
        let smoothed = smooth_analysis_envelope(&env.analysis, env.analysis_sr);
        let ratio = 48_000.0 / 100.0;
        let peak = (0..total).map(|i| catmull_env_at(&smoothed, ratio, i) as f64).fold(0.0, f64::max);
        let got = *t.iter().max().unwrap() as f64 / FULL;
        assert!(peak > 0.1, "the attack shows: {peak}");
        assert!((got - peak).abs() < 0.01, "timeline peak {got} vs α peak {peak}");
        // Each slot holds at least α at its first frame (it is a maximum).
        for s in (0..t.len()).step_by(37) {
            let a = catmull_env_at(&smoothed, ratio, s * 96) as f64;
            assert!(t[s] as f64 / FULL >= a - 1e-4, "slot {s}");
        }
    }

    /// A file converted at a sensitivity step: step 0 is the timeline it
    /// always was, to the bit; a more sensitive step lights at least as much.
    #[test]
    fn a_files_timeline_follows_its_sensitivity_step() {
        // Attacks of several heights at 100 Hz, at 48 kHz output.
        let mut analysis = vec![0.0; 400];
        for (i, h) in [(50, 0.25), (120, 0.33), (200, 0.5), (300, 0.9)] {
            analysis[i] = h;
        }
        let env = Envelope { analysis, analysis_sr: 100.0 };
        let total = 4 * 48_000;
        let fps = frames_per_slot(48_000.0);
        let hp0 = file_timeline(BlendKind::Hybrid, &env, 48_000.0, total, 0);
        assert_eq!(hp0, hybrid_timeline(&super::super::blend::scan_plan(&env, 48_000.0, total), total, fps), "step 0: as before");
        let smoothed = smooth_analysis_envelope(&env.analysis, env.analysis_sr);
        let a0 = file_timeline(BlendKind::Alpha, &env, 48_000.0, total, 0);
        let step = ((48_000.0 * 0.000_25) as usize).max(1);
        assert_eq!(a0, alpha_timeline(&smoothed, 480.0, total, fps, step), "αHP step 0: as before");
        let lit = |t: &[u16]| t.iter().filter(|&&w| w > 0).count();
        let sum = |t: &[u16]| t.iter().map(|&w| w as u64).sum::<u64>();
        let hp2 = file_timeline(BlendKind::Hybrid, &env, 48_000.0, total, 2);
        assert!(lit(&hp2) >= lit(&hp0) && lit(&hp0) > 0, "HP+2 lights {} slots, step 0 {}", lit(&hp2), lit(&hp0));
        assert!(hp0.iter().zip(&hp2).all(|(a, b)| b >= a), "every slot lit at 0 is lit at +2");
        let hpm = file_timeline(BlendKind::Hybrid, &env, 48_000.0, total, -3);
        assert!(lit(&hpm) <= lit(&hp0));
        let a2 = file_timeline(BlendKind::Alpha, &env, 48_000.0, total, 2);
        assert!(sum(&a2) > sum(&a0), "αHP+2 blends more");
        assert!(a0.iter().zip(&a2).all(|(a, b)| b >= a));
    }

    #[test]
    fn a_file_meter_reads_its_timeline_over_the_window() {
        let m = PhaseMeter::for_file(BlendKind::Hybrid, 48_000.0);
        assert_eq!(m.state(), PlanState::Pending);
        assert_eq!(m.level(5_000, 768), 0.0, "dark until the timeline is there");
        m.set_state(PlanState::LowMemory);
        assert_eq!(m.state(), PlanState::LowMemory);
        m.set_plan(hybrid_timeline(&plan(&[4_800, 4_896], false), 48_000, m.frames_per_slot).into());
        assert_eq!(m.state(), PlanState::Ready);
        m.set_state(PlanState::Pending);
        assert_eq!(m.state(), PlanState::Ready, "a timeline stays");
        // The window ending at 5_500 looks back over 4_732.. — the span is in it.
        assert_eq!(m.level(5_500, 768), 1.0);
        assert_eq!(m.level(4_700, 0), 0.0);
        assert_eq!(m.level(4_850, 0), 1.0);
        assert_eq!(m.level(6_000, 768), 0.0, "past the span");
        assert_eq!(m.level(10_000_000, 768), 0.0, "past the end");
        assert_eq!(m.lamp(5_500, 768), [255, 0, PlanState::Ready as u8]);
        let a = PhaseMeter::for_file(BlendKind::Alpha, 48_000.0);
        a.set_state(PlanState::Converting);
        assert_eq!(a.lamp(5_500, 768), [0, 0, 5], "dark, and why");
        // A live meter is live.
        assert_eq!(PhaseMeter::new(BlendKind::Alpha, 48_000.0).state(), PlanState::Live);
        let mut w = m.writer();
        w.note(1, 1.0);
        w.flush();
    }

    #[test]
    fn the_view_is_bins_of_the_weight_and_the_envelope_on_a_fixed_grid() {
        // A file at 48 kHz: one minimum-phase span 48_000..52_800 (100 ms),
        // an envelope at 100 Hz with 0.8 at 1 s.
        let m = PhaseMeter::for_file(BlendKind::Hybrid, 48_000.0);
        m.set_plan(hybrid_timeline(&plan(&[48_000, 52_800], false), 96_000, m.frames_per_slot).into());
        let v = m.view(50_000, -24_000, 480, 100);
        assert_eq!(&v[..3], &[1, PlanState::Ready as u8, 0], "HP, ready, no envelope yet");
        assert_eq!(u16::from_le_bytes([v[4], v[5]]), 100);
        let now = u16::from_le_bytes([v[6], v[7]]) as usize;
        assert_eq!(now, 50, "index 50_000 is bin 104, the first is (50_000 − 24_000) / 480 = 54");
        assert_eq!(v[3], ((50_000 % 480) * 256 / 480) as u8);
        assert_eq!(v.len(), 8 + 200 + 1 + 4);
        assert_eq!(v[208], 0, "step 0");
        assert_eq!(u32::from_le_bytes([v[209], v[210], v[211], v[212]]), 104, "last, the grid bin of index");
        let w = &v[8..108];
        // Grid bins 100..=109 hold 48_000..52_799.
        let lit: Vec<usize> = (0..100).filter(|&k| w[k] > 0).collect();
        assert_eq!(lit, (46..56).collect::<Vec<_>>());
        assert!(v[108..208].iter().all(|&e| e == 0), "no envelope, no line");
        // With its envelope the second half follows it.
        let mut analysis = vec![0.0; 200];
        analysis[100] = 0.8;
        m.set_envelope(Arc::new(Envelope { analysis, analysis_sr: 100.0 }));
        let v = m.view(50_000, -24_000, 480, 100);
        assert_eq!(v[2], 1);
        let e = &v[108..208];
        assert_eq!(*e.iter().max().unwrap(), (0.8f64 * 255.0).round() as u8);
        assert_eq!(e[46], 204, "the bin holding 1 s (grid 100) peaks");
        assert_eq!(e[0], 0);
        // Before the first frame: dark bins, not a panic.
        let v = m.view(100, -24_000, 480, 100);
        assert!(v[8..108].iter().all(|&w| w == 0));
        assert_eq!(u16::from_le_bytes([v[6], v[7]]), 50);
        // Last, after the bins: the sensitivity step, as an i8.
        m.set_sens(-2);
        let v = m.view(50_000, -24_000, 480, 100);
        assert_eq!((v.len(), v[208] as i8), (213, -2));
        m.set_sens(9);
        assert_eq!(m.view(50_000, -24_000, 480, 100)[208] as i8, 3, "clamped");
    }

    /// A live plan's envelope draws as the whole one does: taken as it grows,
    /// never waiting on a page that reads, 0 before its first frame, and the
    /// whole envelope drawn instead once it is there.
    #[test]
    fn a_live_envelope_draws_until_the_whole_one_is_there() {
        let m = PhaseMeter::new(BlendKind::Hybrid, 48_000.0);
        let mut analysis = vec![0.0; 200];
        analysis[100] = 0.8;
        analysis[5] = 0.5;
        let v = m.view(50_000, -24_000, 480, 100);
        assert_eq!(v[2], 0, "nothing made yet");
        assert!(v[108..208].iter().all(|&e| e == 0));
        // Made up to frame 110, from frame 10 on; the page is reading.
        let mut w = m.env_writer(10, 200);
        {
            let _page = m.live_env.lock().unwrap();
            w.publish(&analysis[..110], 100.0);
        }
        assert_eq!(m.view(50_000, -24_000, 480, 100)[2], 0, "kept, not waited for");
        w.publish(&analysis[..110], 100.0);
        let v = m.view(50_000, -24_000, 480, 100);
        assert_eq!(v[2], 1);
        assert_eq!(v[108 + 46], 204, "the bin holding 1 s peaks, as with the whole envelope");
        assert_eq!(m.live_env.lock().unwrap().analysis[5], 0.0, "before its first frame: 0");
        w.publish(&analysis, 100.0);
        assert_eq!(m.live_env.lock().unwrap().analysis.len(), 200, "the rest taken");
        // The same bytes as the whole envelope over the frames made.
        let whole = PhaseMeter::new(BlendKind::Hybrid, 48_000.0);
        let mut cut = analysis.clone();
        cut[..10].fill(0.0);
        whole.set_envelope(Arc::new(Envelope { analysis: cut, analysis_sr: 100.0 }));
        assert_eq!(m.view(50_000, -24_000, 480, 100), whole.view(50_000, -24_000, 480, 100));
        // The whole envelope wins once it is set.
        m.set_envelope(Arc::new(Envelope { analysis: vec![0.0; 200], analysis_sr: 100.0 }));
        let v = m.view(50_000, -24_000, 480, 100);
        assert!(v[108..208].iter().all(|&e| e == 0), "the whole envelope is drawn");
    }

    #[test]
    fn nothing_noted_reads_dark() {
        let m = PhaseMeter::new(BlendKind::Alpha, 44_100.0);
        assert_eq!(m.level(10_000, 3_000), 0.0);
        let mut w = m.writer();
        for i in 0..1_000u64 { w.note(i, 0.4); }
        w.flush();
        assert!((m.level(500, 0) - 0.4).abs() < 1e-3);
        // A slot a lap later is not mistaken for this one.
        assert_eq!(m.level(500 + SLOTS as u64 * m.frames_per_slot, 0), 0.0);
    }
}
