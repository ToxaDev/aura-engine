//! Build-time GPU/CPU routing policy.
//!
//! `decide()` is a pure function: given a snapshot of all relevant inputs it
//! returns one of three decisions.  No I/O; no global state.  The
//! `AURA_PLAYER_FORCE_GPU=1` environment variable is checked once at each call
//! (only for unit-testing; not called on a hot path).
//!
//! Policy rules (evaluated in order):
//!  1. `use_gpu` is false → CPU.
//!  2. No GPU context (no discrete GPU on this machine) → CPU.
//!  3. DXGI free VRAM × 0.75 < demand → CPU.
//!  4. Converter is currently running (VRAM competition) → CPU.
//!  5. `AURA_PLAYER_FORCE_GPU=1` not set AND predicted CPU RTF ≥ 3.0 → CPU.
//!  6. GPU.
//!
//! After reaching a CPU decision, if the predicted RTF at the requested taps
//! is below `CPU_MIN_THRESHOLD` (1.5), the function looks for the largest
//! ladder rung whose predicted RTF is ≥ 1.5 and returns `CpuDowngraded`.
//! Rungs without a measurement are predicted from the nearest measured rung
//! above them (`fill_ladder`); 5k is never a fallback.
//!
//! `DeficitDetector` is the mid-track side (K3): fed once per controller tick,
//! it says when a CPU chain has rendered below `CPU_DEFICIT_THRESHOLD` long
//! enough to be replaced. Pure, so the tests drive it with synthetic time.

use std::sync::Arc;

use crate::audio::converter::dsp::filter::TAP_LADDER;
use crate::player::gpu::ctx::GpuPolyCtx;

/// CPU RTF at or above this: the CPU has enough margin; skip the GPU.
pub const CPU_MARGIN_THRESHOLD: f64 = 3.0;

/// CPU RTF below this: automatic build-time tap downgrade.
pub const CPU_MIN_THRESHOLD: f64 = 1.5;

/// Mid-track rolling RTF deficit threshold: `DeficitDetector` (K3) fires on
/// a CPU chain that renders below it for `K3_HOLD_S` with the buffer short.
pub const CPU_DEFICIT_THRESHOLD: f64 = 1.2;

/// The smallest rung a downgrade may land on. 5k is never a fallback (K3):
/// it is chosen only when the rack asks for it.
pub const MIN_FALLBACK_TAPS: usize = 1_000_000;

/// The fixed part of a block's cost, in taps: cost ∝ taps + this (the 2 + 2L
/// FFTs and the scatter, which do not shrink with the filter). Set by the
/// bench (B2).
pub const COST_FIXED_TAPS: f64 = 2.0e6;

/// A Hybrid-Phase / alpha-HP pair renders both banks: its block costs at
/// least this many times its linear half's (bench B3, stored pair / linear:
/// 1.51-1.74; live at 4 E-cores 192 kHz: 1.65-1.78). A pair rung routes on
/// RTF ≤ linear / this, so a pair with no entry of its own, or with an entry
/// older than a deficit on its linear half, never routes above that bound.
pub const PAIR_MIN_COST_RATIO: f64 = 1.5;

/// Below this much buffered audio a contended pre-trial is skipped: its
/// slowdown of the playing chain could empty the buffer.
pub const CONTENDED_TRIAL_MIN_BUFFER_S: f64 = 1.0;

/// Fresh RTF windows needed after a cost change before one counts: the first
/// may straddle the old chain.
pub const K3_QUALIFY_WINDOWS: u32 = 2;

/// The deficit must hold this long (K3: "for ≥ 3 s").
pub const K3_HOLD_S: f64 = 3.0;

/// Window readings needed inside the hold.
pub const K3_MIN_READINGS: usize = 2;

/// After a generation bump (seek, settings), wait for the new chain or this
/// long: a user's Swap may still be in the channel.
pub const K3_GEN_WAIT_S: f64 = 10.0;

/// No escalation with less than this left in the track.
pub const K3_MIN_REMAINING_S: f64 = 5.0;

/// How far a CPU escalation goes down: one rung. `usize::MAX` = the model's
/// rung (`best_cpu_taps`).
pub const K3_MAX_RUNGS: usize = 1;

/// A tap ladder rung with its predicted CPU RTF.
#[derive(Clone, Debug)]
pub struct TapRtf {
    pub taps: usize,
    pub rtf:  f64,
}

/// Everything `decide()` needs.
pub struct PolicyInputs {
    /// The `useGpu` switch from the rack settings.
    pub use_gpu: bool,
    /// Result of `GpuPolyCtx::try_build()`.
    pub gpu_ctx: Option<Arc<GpuPolyCtx>>,
    /// DXGI free VRAM in bytes (0 when the query fails).
    pub free_vram_bytes: u64,
    /// VRAM required for this chain (bytes).  Use `vram_demand_hp` for
    /// Hybrid-Phase / alpha-HP pairs.
    pub vram_demand: u64,
    /// Whether the converter's GPU work is currently in progress.
    pub conversion_running: bool,
    /// The taps count the settings request.
    pub taps: usize,
    /// Predicted CPU RTF for each available ladder rung at this out_rate,
    /// from `CalibrationStore`.  Empty when no calibration exists.
    pub tap_rtfs: Vec<TapRtf>,
}

impl PolicyInputs {
    /// Predicted CPU RTF for the primary `self.taps` setting, if available.
    pub fn rtf_at_taps(&self) -> Option<f64> {
        self.tap_rtfs.iter().find(|r| r.taps == self.taps).map(|r| r.rtf)
    }
}

/// What `decide()` returns.
pub enum PolicyDecision {
    /// Use the CPU convolver at the requested taps.
    Cpu,
    /// Use the GPU convolver with the supplied context.
    Gpu { ctx: Arc<GpuPolyCtx> },
    /// Use the CPU convolver at a smaller taps count (auto-downgrade).
    CpuDowngraded { to_taps: usize, reason: &'static str },
}

/// Make the GPU/CPU routing decision for one chain build.
///
/// See module documentation for the ordered rule list.
pub fn decide(inputs: &PolicyInputs) -> PolicyDecision {
    // Rule 1: user disabled GPU.
    if !inputs.use_gpu {
        return cpu_or_downgrade(inputs, "power");
    }

    // Rule 2: no discrete GPU on this machine.
    let ctx = match inputs.gpu_ctx.clone() {
        Some(c) => c,
        None    => return cpu_or_downgrade(inputs, "power"),
    };

    // Rule 3: insufficient VRAM (DXGI × 0.75 < demand).
    let margin = (inputs.free_vram_bytes as f64 * 0.75) as u64;
    if margin < inputs.vram_demand {
        return cpu_or_downgrade(inputs, "power");
    }

    // Rule 4: converter is running.
    if inputs.conversion_running {
        return cpu_or_downgrade(inputs, "power");
    }

    // Rule 5 (skipped when AURA_PLAYER_FORCE_GPU=1).
    let force = std::env::var_os("AURA_PLAYER_FORCE_GPU").as_deref() == Some(std::ffi::OsStr::new("1"));
    if !force {
        // No calibration data at all → default to CPU (conservative).
        if inputs.tap_rtfs.is_empty() {
            return PolicyDecision::Cpu;
        }
        match inputs.rtf_at_taps() {
            Some(rtf) if rtf >= CPU_MARGIN_THRESHOLD => {
                return cpu_or_downgrade(inputs, "power");
            }
            None => {
                // Calibration exists for other tap levels but not for the
                // requested one.  Conservative default: CPU, so a live
                // measurement can be collected before the GPU path is taken.
                return PolicyDecision::Cpu;
            }
            _ => {}
        }
    }

    // Rule 6: GPU.
    PolicyDecision::Gpu { ctx }
}

/// Find the largest ladder rung at or below `from_taps` whose predicted RTF
/// meets `CPU_MIN_THRESHOLD`, over the measured rungs and the ones
/// `fill_ladder` predicts.  Rungs below `MIN_FALLBACK_TAPS` are candidates
/// only when they are `from_taps` itself.  Falls back to the smallest
/// candidate if nothing clears the bar.
///
/// Returns `from_taps` unchanged when `tap_rtfs` is empty.
pub fn best_cpu_taps(tap_rtfs: &[TapRtf], from_taps: usize) -> usize {
    let ladder = fill_ladder(tap_rtfs);
    let mut candidates: Vec<&TapRtf> = ladder.iter()
        .map(|(r, _)| r)
        .filter(|r| r.taps <= from_taps && (r.taps >= MIN_FALLBACK_TAPS || r.taps == from_taps))
        .collect();
    candidates.sort_by(|a, b| b.taps.cmp(&a.taps));
    for c in &candidates {
        if c.rtf >= CPU_MIN_THRESHOLD {
            return c.taps;
        }
    }
    candidates.last().map(|r| r.taps).unwrap_or(from_taps)
}

/// RTF at `taps` predicted from a measured rung: cost ∝ taps + `COST_FIXED_TAPS`.
pub fn predict_rtf(from: &TapRtf, taps: usize) -> f64 {
    from.rtf * (from.taps as f64 + COST_FIXED_TAPS) / (taps as f64 + COST_FIXED_TAPS)
}

/// The measured rungs (flag `true`) plus every `TAP_LADDER` rung from
/// `MIN_FALLBACK_TAPS` up that has no measurement, predicted from the
/// smallest measured rung above it (flag `false`), smallest taps first.
/// A rung with nothing measured above it stays absent: the model is only
/// trusted downwards.
pub fn fill_ladder(measured: &[TapRtf]) -> Vec<(TapRtf, bool)> {
    let mut out: Vec<(TapRtf, bool)> = Vec::with_capacity(TAP_LADDER.len());
    for m in measured {
        if !out.iter().any(|(r, _)| r.taps == m.taps) {
            out.push((m.clone(), true));
        }
    }
    for t in TAP_LADDER {
        if t < MIN_FALLBACK_TAPS || out.iter().any(|(r, _)| r.taps == t) {
            continue;
        }
        if let Some(anchor) = measured.iter().filter(|m| m.taps > t).min_by_key(|m| m.taps) {
            out.push((TapRtf { taps: t, rtf: predict_rtf(anchor, t) }, false));
        }
    }
    out.sort_by_key(|(r, _)| r.taps);
    out
}

/// The ladder rung one step below `taps`, never below `MIN_FALLBACK_TAPS`:
/// 30M → 10M → 5M → 1M → None.
pub fn rung_below(taps: usize) -> Option<usize> {
    TAP_LADDER.iter().rev().copied().find(|&t| t < taps && t >= MIN_FALLBACK_TAPS)
}

impl std::fmt::Debug for PolicyDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyDecision::Cpu => write!(f, "Cpu"),
            PolicyDecision::Gpu { .. } => write!(f, "Gpu"),
            PolicyDecision::CpuDowngraded { to_taps, reason } =>
                write!(f, "CpuDowngraded(to_taps={to_taps}, reason={reason})"),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn cpu_or_downgrade(inputs: &PolicyInputs, reason: &'static str) -> PolicyDecision {
    if let Some(rtf) = inputs.rtf_at_taps() {
        if rtf < CPU_MIN_THRESHOLD && !inputs.tap_rtfs.is_empty() {
            let to = best_cpu_taps(&inputs.tap_rtfs, inputs.taps);
            if to < inputs.taps {
                return PolicyDecision::CpuDowngraded { to_taps: to, reason };
            }
        }
    }
    PolicyDecision::Cpu
}

// ── Mid-track deficit (K3) ───────────────────────────────────────────────────

/// One controller tick, as the deficit detector sees it.
#[derive(Clone, Copy, Debug)]
pub struct DeficitSample {
    pub now_s: f64,
    pub track_id: u64,
    pub generation: u64,
    /// Identity of the chain being rendered (write side) and of the one being
    /// heard (read side): the address of their chain descriptors.
    pub write_chain: usize,
    pub read_chain: usize,
    /// Identity of the write side's cost (taps, HP, GPU, source). A swap
    /// that keeps it (quick → full) keeps the running deficit.
    pub cost_id: u64,
    /// `Shared::rtf_milli`: a new value means a render window closed.
    pub rtf_milli: u64,
    pub buffered_s: f64,
    pub target_s: f64,
    /// The write side is a live CPU chain with a ladder rung.
    pub cpu_live: bool,
    /// Something else owns the moment (paused, held, a seek or swap in
    /// flight, a GPU fallback, the end of the track, a pre-trial running…).
    pub blocked: bool,
}

/// What `DeficitDetector::observe` says about a tick.
#[derive(Clone, Debug, PartialEq)]
pub enum DeficitVerdict {
    /// Nothing to watch, or no deficit.
    Idle,
    /// Counting fresh windows after a (re)start.
    Arming { windows: u32 },
    /// In deficit since `held_s` ago, with `readings` window readings.
    Watching { held_s: f64, readings: usize },
    /// Escalate. `rtf` is the median of `readings`.
    Fire { rtf: f64, readings: Vec<f64> },
    /// Already fired for this track.
    Spent,
}

/// K3 detector: fires once per track when the write side's RTF stays below
/// `CPU_DEFICIT_THRESHOLD` with the buffer under its target for `K3_HOLD_S`,
/// counted only over fresh 2-second render windows of an unchanged cost.
pub struct DeficitDetector {
    track: Option<u64>,
    gen: u64,
    chain: usize,
    cost_id: u64,
    /// Set by a generation bump until the write side changes chain.
    await_since: Option<f64>,
    last_milli: u64,
    /// Fresh windows since the last (re)start.
    fresh: u32,
    /// Start of the running deficit span.
    since: Option<f64>,
    readings: Vec<f64>,
    fired: bool,
}

impl DeficitDetector {
    pub const fn new() -> Self {
        DeficitDetector {
            track: None,
            gen: 0,
            chain: 0,
            cost_id: 0,
            await_since: None,
            last_milli: 0,
            fresh: 0,
            since: None,
            readings: Vec::new(),
            fired: false,
        }
    }

    fn clear(&mut self) {
        self.fresh = 0;
        self.since = None;
        self.readings.clear();
    }

    pub fn observe(&mut self, s: &DeficitSample) -> DeficitVerdict {
        // A new track re-arms everything, `fired` included.
        if self.track != Some(s.track_id) {
            *self = DeficitDetector::new();
            self.track = Some(s.track_id);
            self.gen = s.generation;
            self.chain = s.write_chain;
            self.cost_id = s.cost_id;
            self.last_milli = s.rtf_milli;
        }
        // A seek or a settings change may have a Swap still in the channel.
        if s.generation != self.gen {
            self.gen = s.generation;
            self.await_since = Some(s.now_s);
            self.clear();
        }
        if s.write_chain != self.chain {
            self.chain = s.write_chain;
            self.await_since = None;
            if s.cost_id != self.cost_id {
                self.cost_id = s.cost_id;
                self.clear();
            }
        }
        // rtf_milli is stored when a 2 s window closes: a change is a new one.
        let new_window = s.rtf_milli > 0 && s.rtf_milli != self.last_milli;
        if new_window {
            self.last_milli = s.rtf_milli;
        }
        if self.fired {
            return DeficitVerdict::Spent;
        }
        if let Some(t) = self.await_since {
            if s.now_s - t < K3_GEN_WAIT_S {
                self.clear();
                return DeficitVerdict::Idle;
            }
            self.await_since = None;
        }
        // Blocked: the first window after it may hold the blocked time.
        if s.blocked || !s.cpu_live {
            self.clear();
            return DeficitVerdict::Idle;
        }
        if new_window {
            self.fresh += 1;
        }
        if self.fresh < K3_QUALIFY_WINDOWS {
            return DeficitVerdict::Arming { windows: self.fresh };
        }
        let rtf = self.last_milli as f64 / 1000.0;
        if !(rtf < CPU_DEFICIT_THRESHOLD && s.buffered_s < s.target_s) {
            self.since = None;
            self.readings.clear();
            return DeficitVerdict::Idle;
        }
        match self.since {
            None => {
                self.since = Some(s.now_s);
                self.readings.clear();
                self.readings.push(rtf);
            }
            Some(_) if new_window => self.readings.push(rtf),
            Some(_) => {}
        }
        let held_s = s.now_s - self.since.unwrap_or(s.now_s);
        if held_s >= K3_HOLD_S && self.readings.len() >= K3_MIN_READINGS && s.read_chain == s.write_chain {
            self.fired = true;
            return DeficitVerdict::Fire { rtf: median(&self.readings), readings: self.readings.clone() };
        }
        DeficitVerdict::Watching { held_s, readings: self.readings.len() }
    }
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    match n {
        0 => 0.0,
        _ if n % 2 == 1 => s[n / 2],
        _ => 0.5 * (s[n / 2 - 1] + s[n / 2]),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_inputs(use_gpu: bool, taps: usize, rtfs: &[(usize, f64)]) -> PolicyInputs {
        PolicyInputs {
            use_gpu,
            gpu_ctx: None,
            free_vram_bytes: 0,
            vram_demand: 0,
            conversion_running: false,
            taps,
            tap_rtfs: rtfs.iter().map(|&(t, r)| TapRtf { taps: t, rtf: r }).collect(),
        }
    }

    /// Rule 1: use_gpu = false always gives Cpu*.
    #[test]
    fn rule1_use_gpu_false_gives_cpu() {
        let inp = cpu_inputs(false, 1_000_000, &[(1_000_000, 1.0)]);
        assert!(matches!(decide(&inp), PolicyDecision::Cpu | PolicyDecision::CpuDowngraded { .. }));
    }

    /// Rule 2: use_gpu = true but no gpu_ctx → Cpu*.
    #[test]
    fn rule2_no_gpu_ctx() {
        let inp = cpu_inputs(true, 1_000_000, &[(1_000_000, 1.0)]);
        assert!(matches!(decide(&inp), PolicyDecision::Cpu | PolicyDecision::CpuDowngraded { .. }));
    }

    /// Rule 4: conversion running → Cpu* even with gpu_ctx=None (rule 2 fires
    /// first; either way the result is not Gpu).
    #[test]
    fn rule4_conversion_running() {
        let mut inp = cpu_inputs(true, 1_000_000, &[(1_000_000, 1.0)]);
        inp.conversion_running = true;
        assert!(matches!(decide(&inp), PolicyDecision::Cpu | PolicyDecision::CpuDowngraded { .. }));
    }

    /// Rule 5: RTF ≥ 3.0 → CPU has margin, rule 2 also gives Cpu*.
    #[test]
    fn rule5_cpu_has_margin() {
        let inp = cpu_inputs(true, 5_000, &[(5_000, 15.0)]);
        assert!(matches!(decide(&inp), PolicyDecision::Cpu));
    }

    /// No calibration data → Cpu (conservative).
    #[test]
    fn no_calib_no_downgrade() {
        let inp = cpu_inputs(false, 30_000_000, &[]);
        assert!(matches!(decide(&inp), PolicyDecision::Cpu));
    }

    /// Downgrade: RTF < 1.5 at requested taps, but higher at a smaller rung.
    #[test]
    fn downgrade_low_rtf() {
        let inp = cpu_inputs(false, 30_000_000, &[
            (30_000_000, 0.8),
            (10_000_000, 1.9),
            ( 5_000_000, 3.5),
        ]);
        match decide(&inp) {
            PolicyDecision::CpuDowngraded { to_taps, reason } => {
                assert_eq!(to_taps, 10_000_000, "should pick 10M (largest above threshold)");
                assert_eq!(reason, "power");
            }
            other => panic!("expected CpuDowngraded, got {other:?}"),
        }
    }

    /// best_cpu_taps returns the largest rung ≥ threshold.
    #[test]
    fn best_cpu_taps_largest_above_threshold() {
        let rtfs = vec![
            TapRtf { taps: 30_000_000, rtf: 0.8 },
            TapRtf { taps: 10_000_000, rtf: 1.9 },
            TapRtf { taps:  5_000_000, rtf: 3.5 },
            TapRtf { taps:  1_000_000, rtf: 8.0 },
        ];
        assert_eq!(best_cpu_taps(&rtfs, 30_000_000), 10_000_000);
        assert_eq!(best_cpu_taps(&rtfs, 10_000_000), 10_000_000);
        assert_eq!(best_cpu_taps(&rtfs,  5_000_000),  5_000_000);
    }

    /// best_cpu_taps falls back to smallest when nothing clears the bar.
    #[test]
    fn best_cpu_taps_fallback_to_smallest() {
        let rtfs = vec![
            TapRtf { taps: 30_000_000, rtf: 0.3 },
            TapRtf { taps: 10_000_000, rtf: 0.5 },
            TapRtf { taps:  1_000_000, rtf: 1.0 },
        ];
        assert_eq!(best_cpu_taps(&rtfs, 30_000_000), 1_000_000,
            "all below threshold → pick smallest");
    }

    fn rtf_of(ladder: &[(TapRtf, bool)], taps: usize) -> Option<(f64, bool)> {
        ladder.iter().find(|(r, _)| r.taps == taps).map(|(r, m)| (r.rtf, *m))
    }

    /// One measured rung fills the rungs below it (not 5k), never above.
    #[test]
    fn fill_ladder_down_only() {
        let ladder = fill_ladder(&[TapRtf { taps: 30_000_000, rtf: 0.64 }]);
        assert_eq!(rtf_of(&ladder, 30_000_000), Some((0.64, true)));
        // 0.64 · (30M + 2M) / (T + 2M)
        let (r10, m10) = rtf_of(&ladder, 10_000_000).unwrap();
        let (r5, m5) = rtf_of(&ladder, 5_000_000).unwrap();
        let (r1, m1) = rtf_of(&ladder, 1_000_000).unwrap();
        assert!(!m10 && !m5 && !m1, "predicted rungs flagged as measured");
        assert!((r10 - 0.64 * 32.0 / 12.0).abs() < 1e-9, "10M {r10}");
        assert!((r5 - 0.64 * 32.0 / 7.0).abs() < 1e-9, "5M {r5}");
        assert!((r1 - 0.64 * 32.0 / 3.0).abs() < 1e-9, "1M {r1}");
        assert!((r10 - 1.71).abs() < 0.01);
        assert!(rtf_of(&ladder, 5_000).is_none(), "5k must not be predicted");
        assert_eq!(ladder.len(), 4);

        // Nothing is extrapolated upwards.
        let ladder = fill_ladder(&[TapRtf { taps: 10_000_000, rtf: 2.0 }]);
        assert!(rtf_of(&ladder, 30_000_000).is_none());
        assert!(rtf_of(&ladder, 5_000_000).is_some());

        // The anchor is the smallest measured rung above.
        let ladder = fill_ladder(&[
            TapRtf { taps: 30_000_000, rtf: 0.5 },
            TapRtf { taps: 10_000_000, rtf: 2.0 },
        ]);
        let (r5, _) = rtf_of(&ladder, 5_000_000).unwrap();
        assert!((r5 - 2.0 * 12.0 / 7.0).abs() < 1e-9, "5M anchored at 10M: {r5}");
        assert!(fill_ladder(&[]).is_empty());
    }

    /// Only the requested rung measured, below 1.5: the model finds the rung.
    #[test]
    fn best_cpu_taps_extrapolates() {
        let one = |taps: usize, rtf: f64| vec![TapRtf { taps, rtf }];
        assert_eq!(best_cpu_taps(&one(30_000_000, 0.64), 30_000_000), 10_000_000);
        assert_eq!(best_cpu_taps(&one(30_000_000, 0.3), 30_000_000), 1_000_000);
        assert_eq!(best_cpu_taps(&one(10_000_000, 1.4), 10_000_000), 5_000_000);
        assert_eq!(best_cpu_taps(&[], 30_000_000), 30_000_000);
    }

    /// use_gpu off, only 30M measured at 0.64 → CpuDowngraded to 10M.
    #[test]
    fn decide_downgrades_with_only_requested_rung() {
        let inp = cpu_inputs(false, 30_000_000, &[(30_000_000, 0.64)]);
        match decide(&inp) {
            PolicyDecision::CpuDowngraded { to_taps, reason } => {
                assert_eq!(to_taps, 10_000_000);
                assert_eq!(reason, "power");
            }
            other => panic!("expected CpuDowngraded, got {other:?}"),
        }
    }

    /// A downgrade never lands on 5k, however fast it is.
    #[test]
    fn five_k_never_a_fallback() {
        let rtfs = vec![
            TapRtf { taps: 1_000_000, rtf: 0.5 },
            TapRtf { taps:     5_000, rtf: 50.0 },
        ];
        assert_eq!(best_cpu_taps(&rtfs, 1_000_000), 1_000_000);
        assert_eq!(best_cpu_taps(&rtfs, 30_000_000), 1_000_000);
        // Asked for 5k: 5k it is.
        assert_eq!(best_cpu_taps(&rtfs, 5_000), 5_000);
    }

    #[test]
    fn rung_below_ladder() {
        assert_eq!(rung_below(30_000_000), Some(10_000_000));
        assert_eq!(rung_below(10_000_000), Some(5_000_000));
        assert_eq!(rung_below(5_000_000), Some(1_000_000));
        assert_eq!(rung_below(1_000_000), None);
        assert_eq!(rung_below(5_000), None);
    }

    // ── DeficitDetector ─────────────────────────────────────────────────────

    const A: usize = 0xA0;
    const B: usize = 0xB0;

    /// A sustained deficit on one chain from t = 0: a window closes every 2 s
    /// and reads 0.90 (+ 0.001 per window, so consecutive windows differ).
    fn deficit(ms: u64) -> DeficitSample {
        DeficitSample {
            now_s: ms as f64 / 1000.0,
            track_id: 1,
            generation: 0,
            write_chain: A,
            read_chain: A,
            cost_id: 30,
            rtf_milli: if ms < 2000 { 0 } else { 900 + ms / 2000 },
            buffered_s: 0.8,
            target_s: 1.5,
            cpu_live: true,
            blocked: false,
        }
    }

    /// Ticks every 200 ms over `[from_ms, to_ms]`; the tick of the first Fire.
    fn first_fire(d: &mut DeficitDetector, from_ms: u64, to_ms: u64, at: impl Fn(u64) -> DeficitSample) -> Option<u64> {
        let mut ms = from_ms;
        while ms <= to_ms {
            if let DeficitVerdict::Fire { .. } = d.observe(&at(ms)) {
                return Some(ms);
            }
            ms += 200;
        }
        None
    }

    /// Two fresh windows arm it, then the deficit must hold 3 s with two
    /// readings: windows at 2, 4, 6 s → span from 4 s → fire at 7 s.
    #[test]
    fn fires_after_two_windows_and_hold() {
        let mut d = DeficitDetector::new();
        assert_eq!(d.observe(&deficit(0)), DeficitVerdict::Arming { windows: 0 });
        assert_eq!(d.observe(&deficit(2000)), DeficitVerdict::Arming { windows: 1 });
        assert_eq!(d.observe(&deficit(4000)), DeficitVerdict::Watching { held_s: 0.0, readings: 1 });
        assert_eq!(d.observe(&deficit(6000)), DeficitVerdict::Watching { held_s: 2.0, readings: 2 });
        match d.observe(&deficit(7000)) {
            DeficitVerdict::Fire { rtf, readings } => {
                assert_eq!(readings, vec![0.902, 0.903]);
                assert!((rtf - 0.9025).abs() < 1e-9, "median {rtf}");
            }
            other => panic!("expected Fire at 7 s, got {other:?}"),
        }
        assert_eq!(d.observe(&deficit(7200)), DeficitVerdict::Spent);

        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, deficit), Some(7000));
    }

    /// After a cost change the first window (which straddles both chains) does
    /// not count, even when it reads a deficit.
    #[test]
    fn first_window_after_cost_change_ignored() {
        let at = |ms: u64| {
            let mut s = deficit(ms);
            if ms < 11_000 {
                s.rtf_milli = if ms < 2000 { 0 } else { 2000 + ms / 2000 };   // healthy
            } else {
                s.write_chain = B;
                s.read_chain = B;
                s.cost_id = 10;
                s.rtf_milli = match ms {
                    _ if ms < 12_000 => 2005,
                    _ if ms < 14_000 => 1000,             // straddling window, in deficit
                    _ => 900 + ms / 2000,
                };
            }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 11_800, at), None);
        assert_eq!(d.observe(&at(12_000)), DeficitVerdict::Arming { windows: 1 });
        // Span from 14 s (not 12 s) → fire at 17 s.
        assert_eq!(first_fire(&mut d, 12_200, 60_000, at), Some(17_000));
    }

    /// A swap that keeps the cost (quick → full) keeps the running span (from
    /// 4 s, due at 7 s); the fire waits until the reader hears the new chain.
    #[test]
    fn same_cost_swap_keeps_span() {
        let at = |ms: u64| {
            let mut s = deficit(ms);
            if ms >= 5000 { s.write_chain = B; }
            if ms >= 7600 { s.read_chain = B; }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, at), Some(7600));

        // The same swap with another cost restarts: windows at 6, 8 → span
        // from 8 s, a second reading at 10 s → fire at 11 s.
        let other = |ms: u64| {
            let mut s = at(ms);
            if ms >= 5000 { s.cost_id = 10; }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, other), Some(11_000));
    }

    /// A generation bump holds the detector until the write side changes chain
    /// or K3_GEN_WAIT_S has passed.
    #[test]
    fn generation_waits_for_chain_or_10s() {
        // No new chain: idle from 5 s to 15 s; windows at 16, 18 → span from
        // 18 s, a second reading at 20 s → fire at 21 s.
        let bump = |ms: u64| {
            let mut s = deficit(ms);
            if ms >= 5000 { s.generation = 1; }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, bump), Some(21_000));

        // The new chain (another cost) arrives at 6.5 s: windows at 8, 10 →
        // span from 10 s, a second reading at 12 s → fire at 13 s.
        let chain = |ms: u64| {
            let mut s = bump(ms);
            if ms >= 6500 {
                s.write_chain = B;
                s.read_chain = B;
                s.cost_id = 10;
            }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, chain), Some(13_000));
    }

    /// Every blocked tick restarts the window count.
    #[test]
    fn blocked_restarts_window_count() {
        let at = |ms: u64| {
            let mut s = deficit(ms);
            s.blocked = (4200..=4600).contains(&ms);
            s
        };
        // Windows at 6, 8 → span from 8 s, 10 s → fire at 11 s (not 7 s).
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, at), Some(11_000));

        // Not a live CPU chain: the same.
        let gpu = |ms: u64| {
            let mut s = deficit(ms);
            s.cpu_live = !(4200..=4600).contains(&ms);
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, gpu), Some(11_000));
    }

    /// No fire with a full buffer, without windows, above the threshold, or
    /// off the CPU.
    #[test]
    fn full_buffer_or_rtf0_never_fires() {
        let cases: [fn(u64) -> DeficitSample; 4] = [
            |ms| DeficitSample { buffered_s: 1.5, ..deficit(ms) },
            |ms| DeficitSample { rtf_milli: 0, ..deficit(ms) },
            |ms| DeficitSample { rtf_milli: if ms < 2000 { 0 } else { 1300 + ms / 2000 }, ..deficit(ms) },
            |ms| DeficitSample { cpu_live: false, ..deficit(ms) },
        ];
        for (i, at) in cases.iter().enumerate() {
            let mut d = DeficitDetector::new();
            assert_eq!(first_fire(&mut d, 0, 120_000, at), None, "case {i}");
        }
    }

    /// Once per track, through seeks and swaps; a new track re-arms.
    #[test]
    fn once_per_track_new_track_rearms() {
        let at = |ms: u64| {
            let mut s = deficit(ms);
            if ms >= 12_000 {
                s.generation = 1;          // a seek
                s.write_chain = B;
                s.read_chain = B;
            }
            if ms >= 30_000 {
                s.track_id = 2;
            }
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 29_800, at), Some(7000));
        assert_eq!(first_fire(&mut d, 7200, 29_800, at), None, "fired twice in one track");
        assert_eq!(d.observe(&at(29_800)), DeficitVerdict::Spent);
        // Track 2 from 30 s: windows at 32, 34 → span, 36 → fire at 37 s.
        assert_eq!(first_fire(&mut d, 30_000, 60_000, at), Some(37_000));
    }

    /// The write side is in deficit, but the reader still hears the previous
    /// chain: the fire waits for it.
    #[test]
    fn read_ne_write_delays_fire() {
        let at = |ms: u64| {
            let mut s = deficit(ms);
            s.read_chain = if ms < 9000 { B } else { A };
            s
        };
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 8800, at), None);
        match d.observe(&at(8800)) {
            DeficitVerdict::Watching { held_s, readings } => {
                assert!((held_s - 4.8).abs() < 1e-9, "held {held_s}");
                assert_eq!(readings, 3);
            }
            other => panic!("expected Watching, got {other:?}"),
        }
        let mut d = DeficitDetector::new();
        assert_eq!(first_fire(&mut d, 0, 60_000, at), Some(9000));
    }
}
