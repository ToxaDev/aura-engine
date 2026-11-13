//! Cue Consistency: the stage in front of XTC.
//!
//! Pan-pot stereo carries one cue, a level difference, the same at every
//! frequency, with no time difference. A real source at an angle gives three
//! that agree: a time difference below ~1.5 kHz, a head shadow rising with
//! frequency above, and almost no level difference at the bottom. Delivered
//! ear by ear (XTC, headphones) the brain hears the conflict. This stage
//! completes the missing cues for the angle the mix already encodes.
//!
//! It runs on the output-rate stream, in the chain, BEFORE XTC: XTC then
//! delivers the completed binaural signal, and the room's filter stays what
//! it was measured for. Being in the chain, a change is heard when the render
//! it reaches is played: after the player's buffer ("buf" in the player).
//!
//! How, on an STFT of about 46 ms (2048 at 44.1 kHz, about the same length in
//! time at any output rate), hop a quarter of it:
//!
//! * Analysis per ERB band, not per bin: a 2×2 covariance of L and R, smoothed
//!   over `tau_s_ms`, reset at an onset. Its principal axis, read from the size
//!   of the cross term, is the pan: exact for a coherent source whatever the
//!   phase between the channels, unbiased by what is around it, where a power
//!   ratio is not. How far the axis stands out from the rest of the band says
//!   how much of the band is one source: a diffuse band is left alone, and so
//!   is one whose channels are not in phase. In the bass a band is read with
//!   its neighbours: one note's harmonics are one source, not one place each.
//! * Levels by rotating (L, R): a rotation keeps the power of every bin, so
//!   stage on and stage bypassed play at one level, and it can give a hard-left
//!   bass to the right ear, which no gain on the right channel could.
//! * Time by a true delay of the lagging channel at every frequency: a phase
//!   that follows the frequency moves the envelope by exactly the delay, where
//!   anything periodic in frequency would leave the attack where it was and
//!   add echoes. The head is the low-frequency sphere (Kuhn) below `f1` and the
//!   ray (Woodworth) above `f2`.
//! * In front of XTC nothing happens below XTC's own band: there each ear hears
//!   both speakers, which give their cues by themselves, and two channels that
//!   differ only by a delay would lose their time difference at the ears.
//!
//! Dry and wet stay aligned sample for sample (the stage reads ahead, it adds
//! no delay), so the bypass and the "already natural" gate crossfade to the
//! dry samples and then are the upstream's samples, bit for bit.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use super::stages::Stage;
use crate::audio::converter::dsp::lab::xtc;

type C = Complex<f64>;

/// Bins from here up are left as they are.
const TOP_HZ: f64 = 20_000.0;
/// Bins below this are analysed with the lowest band's angle.
const BOTTOM_HZ: f64 = 30.0;
/// The bypass and the gate's crossfade, seconds (as `LiveEq`'s).
const FADE_S: f64 = 0.030;
/// The width a band in the bass is read over, Hz, with its neighbours.
const POOL_HZ: f64 = 250.0;
/// The head: radius, m, and the speed of sound, m/s.
const HEAD_RADIUS_M: f64 = 0.0875;
const SOUND_M_S: f64 = 343.0;
/// The widest level difference the angle and the blend are read to, dB.
const RHO_MAX_DB: f64 = 60.0;
/// The most of a pan's own level difference the head shadow is blended with, dB.
const RHO_BLEND_DB: f64 = 24.0;
/// A channel this far under the other, dB and on: its phase says nothing.
const LEAN_FROM_DB: f64 = 12.0;
const LEAN_FULL_DB: f64 = 24.0;
/// The gate's time to know a track, seconds, and its memory.
const GATE_FIRST_S: f64 = 0.30;
const GATE_TAU_S: f64 = 2.0;
/// Confidence that opens the gate, and that closes it again.
const GATE_OPEN: f64 = 0.50;
const GATE_CLOSE: f64 = 0.30;
/// How long the gate must want a change before it makes it, seconds.
const GATE_HOLD_S: f64 = 5.0;
/// The most a chain reads ahead at its first read to know the gate, seconds.
const LOOK_AHEAD_S: f64 = 0.5;
/// Side to mid power under which a track counts as mono.
const MONO_SIDE: f64 = 1e-7;
/// Frames between two views of the stage (and two looks of the gate).
const VIEW_FRAMES: u32 = 4;

/// The pan law the mix is read with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanLaw {
    /// −3 dB sin/cos.
    #[default]
    Sin3,
    /// −4.5 dB, halfway.
    Mid45,
    /// −6 dB, linear.
    Lin6,
}

/// What the SET window sends: all of it live, nothing rebuilt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CueParams {
    /// A/B: the dry samples, from the end of the crossfade bit for bit.
    pub bypass: bool,
    /// 0–100 %: how far toward the target, in the cues themselves (a dry/wet
    /// mix of a delayed channel would comb).
    pub strength: f64,
    pub f1_hz: f64,
    pub f2_hz: f64,
    /// 0–100 %: the level difference taken off below `f1`.
    pub lf_ild: f64,
    /// 50–150 %: the time differences (a head's size).
    pub itd_scale: f64,
    /// 30–90°: the angle a hard pan is given; the rest of the pans in
    /// proportion. Time and head shadow both follow it, so they stay one place.
    pub stage_width: f64,
    pub head_shadow: bool,
    /// 0–100 %: head shadow against the pan's own level difference above `f2`.
    pub shadow_blend: f64,
    /// The delay goes on above `f2` (off: it fades out from `f1` to `f2`).
    pub env_itd: bool,
    pub pan_law: PanLaw,
    /// The places a calibration found, as level differences, dB; a band
    /// within 3 dB of one is read as being at it. Empty: not calibrated.
    pub snap_db: Vec<f64>,
    pub tau_s_ms: f64,
    pub onset_reset: bool,
}

impl Default for CueParams {
    fn default() -> Self {
        CueParams {
            bypass: false,
            strength: 100.0,
            f1_hz: 700.0,
            f2_hz: 1500.0,
            lf_ild: 100.0,
            itd_scale: 100.0,
            stage_width: 90.0,
            head_shadow: true,
            shadow_blend: 50.0,
            env_itd: true,
            pan_law: PanLaw::Sin3,
            snap_db: Vec::new(),
            tau_s_ms: 50.0,
            onset_reset: true,
        }
    }
}

impl CueParams {
    /// Whatever the page sends comes into the ranges the stage works in.
    fn sane(mut self) -> CueParams {
        self.strength = self.strength.clamp(0.0, 100.0);
        self.lf_ild = self.lf_ild.clamp(0.0, 100.0);
        self.itd_scale = self.itd_scale.clamp(50.0, 150.0);
        self.stage_width = self.stage_width.clamp(30.0, 90.0);
        self.shadow_blend = self.shadow_blend.clamp(0.0, 100.0);
        self.f1_hz = self.f1_hz.clamp(200.0, 1400.0);
        self.f2_hz = self.f2_hz.clamp(self.f1_hz + 100.0, 3000.0);
        self.tau_s_ms = self.tau_s_ms.clamp(5.0, 500.0);
        self
    }
}

/// One place the stage reads a source at.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueDot {
    /// The source followed from view to view: a place keeps its number while
    /// it moves, a new source gets a new one (unique across tracks).
    pub id: u32,
    /// Azimuth, degrees, left positive: where the mix puts it, a pan's level
    /// difference and a delay between the channels together.
    pub deg: f64,
    /// Its level difference, left over right, dB: what a calibration holds.
    pub rho_db: f64,
    /// The delay between the channels it carries, ms, the right one later
    /// positive: none for a pan pot, a source panned by time shows it.
    pub itd_ms: f64,
    /// Against the strongest place, 0–1.
    pub weight: f64,
    /// What sounds from this place, dB against full scale, band by band in
    /// the order of `CueView::bands`: its image, over the last seconds.
    pub spec_db: Vec<f64>,
    /// How much of what sounds from here is one coherent source, 0–1: a dry
    /// source reads 1, reverberation lowers it. Not a distance in metres.
    pub direct: f64,
    /// Its own sound as a wave, separated from the rest by where it sits:
    /// the lowest and highest sample of each hop in turn, oldest first, the
    /// last one the view's `seq`. Full scale is ±1.
    pub wave: Vec<f32>,
}

/// One band as the stage plays it now.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueBand {
    pub hz: f64,
    pub itd_us: f64,
    pub ild_in_db: f64,
    pub ild_out_db: f64,
}

/// What the SET window shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueView {
    pub running: bool,
    pub rate: u32,
    /// XTC follows the stage (off: direct delivery, headphones).
    pub xtc: bool,
    pub confidence: f64,
    /// The gate: false on a mono or an already natural recording.
    pub open: bool,
    pub mono: bool,
    /// The wet share playing now, 0–1.
    pub wet: f64,
    pub dots: Vec<CueDot>,
    pub bands: Vec<CueBand>,
    /// Where in the track this view was read, seconds: the stage runs ahead
    /// of what is heard by the player's buffer, the window asks for this.
    pub at_s: f64,
    /// The number of the newest hop in the dots' waves, and hops a second.
    pub seq: u64,
    pub slice_hz: f64,
    /// The stage that read it: a chain's end marks only its own view stopped.
    #[serde(skip)]
    pub chain: u64,
}

/// The views kept to find the one heard now: the render runs up to a few
/// seconds ahead of the ear.
const VIEWS_KEPT: usize = 160;
/// Seconds of a place's wave a view hands the window.
const WAVE_S: f64 = 0.8;
/// Seconds the window counts as open after its last look: the window's
/// reading of the stage costs nothing while nobody looks.
const WATCH_S: f64 = 3.0;

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// The wire between the window and the stage: parameters taken at the next
/// frame by a generation counter (the EQ's way), the views back.
#[derive(Default)]
pub struct CueControl {
    gen: AtomicU64,
    params: Mutex<CueParams>,
    views: Mutex<VecDeque<CueView>>,
    /// When the window last looked, `now_ms`; 0 never.
    looked_ms: AtomicU64,
    /// The places as the last chain read them, for the next chain of the same
    /// track to go on from.
    carry: Mutex<Option<Carry>>,
}

/// A track's places handed from one chain to the next: the player starts a
/// track on a quick chain and moves to the full one a few seconds in (and a
/// rack change or a near seek builds another); the window goes on with the
/// same places instead of finding them all over again.
struct Carry {
    chain: u64,
    rate: u32,
    total: u64,
    at_s: f64,
    ms: u64,
    look: LookState,
}

/// A chain takes the places over when it is the same track (rate and length)
/// read within this many seconds of where the last one was, this fresh, ms.
const CARRY_S: f64 = 8.0;
const CARRY_MS: u64 = 10_000;
/// The places are handed on every this many views.
const CARRY_VIEWS: u64 = 4;

impl CueControl {
    pub fn set(&self, p: CueParams) {
        *self.params.lock().unwrap() = p.sane();
        self.gen.fetch_add(1, Ordering::AcqRel);
    }

    /// The newest view (the streamer's remote asks for it this way).
    #[allow(dead_code)]
    pub fn view(&self) -> CueView {
        self.view_heard(None)
    }

    /// The view read at `heard_s`, the second of the track heard now (None:
    /// the newest), each place with the last `WAVE_S` of its wave.
    pub fn view_heard(&self, heard_s: Option<f64>) -> CueView {
        self.looked_ms.store(now_ms(), Ordering::Relaxed);
        let q = self.views.lock().unwrap();
        let Some(newest) = q.back() else { return CueView::default() };
        // Ahead of the ear, behind it by more than a seek's distance: another chain.
        let pick = heard_s
            .and_then(|h| q.iter().rposition(|v| v.at_s <= h + 0.02 && v.at_s >= h - 2.0))
            .unwrap_or(q.len() - 1);
        let mut v = q[pick].clone();
        v.running = newest.running;
        for d in v.dots.iter_mut() {
            let mut parts: Vec<&[f32]> = Vec::new();
            for w in q.range(..pick).rev() {
                if w.at_s > v.at_s || w.at_s < v.at_s - WAVE_S {
                    break;
                }
                match w.dots.iter().find(|e| e.id == d.id) {
                    Some(e) => parts.push(&e.wave),
                    None => break,
                }
            }
            let mut wave: Vec<f32> = parts.iter().rev().flat_map(|p| p.iter().copied()).collect();
            wave.extend_from_slice(&d.wave);
            d.wave = wave;
        }
        v
    }

    fn watched(&self) -> bool {
        let at = self.looked_ms.load(Ordering::Relaxed);
        at != 0 && now_ms().saturating_sub(at) < (WATCH_S * 1e3) as u64
    }
}

/// The player's one control.
pub fn control() -> Arc<CueControl> {
    static C: OnceLock<Arc<CueControl>> = OnceLock::new();
    C.get_or_init(Default::default).clone()
}

// ─────────────────────────── the head ───────────────────────────

/// Pressure on a rigid sphere for a plane wave from `gamma` radians off the
/// point (Rayleigh's series, as Duda & Martens 1998), relative to the free
/// field, at `mu` = k·a.
fn sphere_pressure(mu: f64, gamma: f64) -> f64 {
    if mu < 1e-3 {
        return 1.0;
    }
    let x = gamma.cos();
    // h⁽¹⁾_m(mu), upward (stable: y_m dominates).
    let e = C::new(mu.cos(), mu.sin());
    let mut h_prev = C::new(0.0, -1.0) * e / mu;
    let mut h = -e * C::new(mu, 1.0) / (mu * mu);
    let (mut p_prev, mut p) = (1.0, x);
    // m = 0: h'₀ = −h₁.
    let mut sum = C::new(0.0, 1.0) / (-h);
    // (−j)^m: `gamma` counts from where the wave comes from.
    let mut i_m = C::new(1.0, 0.0);
    let last = (mu as usize) + 30;
    for m in 1..=last {
        i_m *= C::new(0.0, -1.0);
        let dh = h_prev - h * ((m + 1) as f64 / mu);
        if !dh.norm().is_finite() || dh.norm() > 1e120 {
            break;
        }
        sum += C::new(0.0, 1.0) * i_m * ((2 * m + 1) as f64) * p / dh;
        let h_next = h * ((2 * m + 1) as f64 / mu) - h_prev;
        h_prev = h;
        h = h_next;
        let p_next = ((2 * m + 1) as f64 * x * p - m as f64 * p_prev) / (m + 1) as f64;
        p_prev = p;
        p = p_next;
    }
    (sum / (mu * mu)).norm()
}

/// Head shadow, dB, left over right, for a source `deg` degrees to the left
/// (the ears at ±100°, as Duda & Martens have them).
fn shadow_exact_db(deg: f64, hz: f64) -> f64 {
    let mu = 2.0 * PI * hz / SOUND_M_S * HEAD_RADIUS_M;
    let off = |ear: f64| {
        let a = (deg - ear).abs();
        (if a > 180.0 { 360.0 - a } else { a }).to_radians()
    };
    20.0 * (sphere_pressure(mu, off(100.0)) / sphere_pressure(mu, off(-100.0))).log10()
}

const SHADOW_DEG_STEP: f64 = 5.0;
const SHADOW_DEGS: usize = 19;
const SHADOW_FS: usize = 48;

/// The head's table, on log frequency: its series once, its lookups at frame
/// rate. A shadow is odd in the angle, so the table holds 0–90°. A perfect
/// sphere's bright spot would have a source lose its shadow as it moves further
/// out, which no real head does: the table only ever grows with the angle.
fn shadow_table() -> &'static Vec<[f64; SHADOW_DEGS]> {
    static T: OnceLock<Vec<[f64; SHADOW_DEGS]>> = OnceLock::new();
    T.get_or_init(|| {
        (0..SHADOW_FS)
            .map(|i| {
                let hz = shadow_hz(i);
                let mut row = [0.0; SHADOW_DEGS];
                let mut most = 0f64;
                for (j, v) in row.iter_mut().enumerate() {
                    most = most.max(shadow_exact_db(j as f64 * SHADOW_DEG_STEP, hz));
                    *v = most;
                }
                row
            })
            .collect()
    })
}

fn shadow_hz(i: usize) -> f64 {
    100.0 * (TOP_HZ / 100.0).powf(i as f64 / (SHADOW_FS - 1) as f64)
}

/// `shadow_exact_db`, from the table.
fn shadow_db(deg: f64, hz: f64) -> f64 {
    let t = shadow_table();
    let fi = ((hz.max(100.0) / 100.0).ln() / (TOP_HZ / 100.0).ln() * (SHADOW_FS - 1) as f64).clamp(0.0, (SHADOW_FS - 1) as f64);
    let di = (deg.abs() / SHADOW_DEG_STEP).clamp(0.0, (SHADOW_DEGS - 1) as f64);
    let (f0, d0) = ((fi as usize).min(SHADOW_FS - 2), (di as usize).min(SHADOW_DEGS - 2));
    let (ff, fd) = (fi - f0 as f64, di - d0 as f64);
    let at = |f: usize, d: usize| t[f][d];
    let v = (1.0 - ff) * ((1.0 - fd) * at(f0, d0) + fd * at(f0, d0 + 1)) + ff * ((1.0 - fd) * at(f0 + 1, d0) + fd * at(f0 + 1, d0 + 1));
    v * deg.signum()
}

/// Woodworth: the ray's time difference for an angle, seconds, left positive.
/// What a head gives high up, where sound goes around it.
fn woodworth_s(deg: f64) -> f64 {
    let th = deg.abs().to_radians();
    deg.signum() * HEAD_RADIUS_M / SOUND_M_S * (th + th.sin())
}

/// Kuhn: the low-frequency time difference, up to half as much again as the
/// ray's, the one the sphere above gives from its phase below ~1 kHz.
fn kuhn_s(deg: f64) -> f64 {
    3.0 * HEAD_RADIUS_M / SOUND_M_S * deg.to_radians().sin()
}

/// The time difference of one head at `t`, 0 low, 1 high.
fn head_itd_s(deg: f64, t: f64) -> f64 {
    (1.0 - t) * kuhn_s(deg) + t * woodworth_s(deg)
}

// ─────────────────────────── the pan law ───────────────────────────

/// The level difference, left over right, a pan at `deg` gives.
fn law_rho_db(law: PanLaw, deg: f64) -> f64 {
    let x = ((90.0 - deg) / 180.0).clamp(0.0, 1.0);
    let a = x * PI / 2.0;
    let (l3, r3) = (a.cos(), a.sin());
    let (l6, r6) = (1.0 - x, x);
    let (l, r) = match law {
        PanLaw::Sin3 => (l3, r3),
        PanLaw::Lin6 => (l6, r6),
        PanLaw::Mid45 => ((l3 * l6).sqrt(), (r3 * r6).sqrt()),
    };
    (20.0 * (l.max(1e-9) / r.max(1e-9)).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB)
}

/// The law, inverted on a table of every degree.
struct LawTable {
    law: PanLaw,
    rho: Vec<f64>,
}

impl LawTable {
    fn new(law: PanLaw) -> LawTable {
        LawTable { law, rho: (0..=180).map(|i| law_rho_db(law, i as f64 - 90.0)).collect() }
    }

    fn deg(&self, rho: f64) -> f64 {
        let i = self.rho.partition_point(|&v| v < rho);
        if i == 0 {
            return -90.0;
        }
        if i >= self.rho.len() {
            return 90.0;
        }
        let (a, b) = (self.rho[i - 1], self.rho[i]);
        let f = if b > a { (rho - a) / (b - a) } else { 0.0 };
        i as f64 - 1.0 + f - 90.0
    }
}

// ─────────────────────────── the bands ───────────────────────────

fn erb_rate(hz: f64) -> f64 {
    21.4 * (1.0 + 0.00437 * hz).log10()
}

fn erb_rate_inv(e: f64) -> f64 {
    (10f64.powf(e / 21.4) - 1.0) / 0.00437
}

/// One ERB each, of two bins at least: first bin of each, then one past the last.
fn band_edges(bin_hz: f64, top: usize) -> Vec<usize> {
    let first = ((BOTTOM_HZ / bin_hz).round() as usize).max(1);
    let mut edges = vec![first];
    let (mut e, top_e) = (erb_rate(BOTTOM_HZ), erb_rate(top as f64 * bin_hz));
    while e < top_e {
        e += 1.0;
        let k = ((erb_rate_inv(e) / bin_hz).round() as usize).min(top + 1);
        if k >= edges[edges.len() - 1] + 2 {
            edges.push(k);
        }
    }
    let last = edges.len() - 1;
    edges[last] = top + 1;
    edges
}

/// A band's covariance of L and R, per bin, against a full-scale window.
#[derive(Clone, Copy, Default)]
struct Cov {
    ll: f64,
    rr: f64,
    re: f64,
    im: f64,
}

impl Cov {
    fn go(&mut self, to: &Cov, a: f64) {
        self.ll += a * (to.ll - self.ll);
        self.rr += a * (to.rr - self.rr);
        self.re += a * (to.re - self.re);
        self.im += a * (to.im - self.im);
    }

    fn add(&mut self, c: &Cov, w: f64) {
        self.ll += w * c.ll;
        self.rr += w * c.rr;
        self.re += w * c.re;
        self.im += w * c.im;
    }

    /// The principal axis as an angle, radians (L = cos α, R = sin α), and
    /// how far it stands out: 1 one coherent source, 0 a diffuse band. The size
    /// of the cross term, not its real part: a phase between the channels does
    /// not move a source.
    fn axis(&self) -> (f64, f64) {
        let sum = self.ll + self.rr;
        if sum <= 0.0 {
            return (PI / 4.0, 0.0);
        }
        let d = self.ll - self.rr;
        let mag = (self.re * self.re + self.im * self.im).sqrt();
        (0.5 * (2.0 * mag).atan2(d), ((d * d + 4.0 * mag * mag).sqrt() / sum).min(1.0))
    }

    /// How far the channels are in phase: 1 up to 60° apart, 0 from 90° on, so
    /// a widener's or a chorus's band fades out rather than flips from frame
    /// to frame. A band with no cross term has no phase to ask about.
    fn in_phase(&self) -> f64 {
        let mag = (self.re * self.re + self.im * self.im).sqrt();
        if mag <= 0.0 {
            return 1.0;
        }
        smoothstep(0.0, 0.5, self.re / mag)
    }
}

/// How a bin reads the bands' values: between two band centres, and what its
/// frequency makes of the time difference.
#[derive(Clone, Copy)]
struct BinMap {
    a: u16,
    frac: f32,
    /// Hz the phase is delay × 2π of.
    phase_hz: f64,
    /// Fades the stage out toward the top of the band.
    edge: f64,
}

/// One frame's values per band.
#[derive(Clone, Copy, Default)]
struct BandOut {
    delta: f64,
    tau_s: f64,
    rho_in: f64,
    rho_out: f64,
}

fn raised(x: f64) -> f64 {
    let t = x.clamp(0.0, 1.0);
    0.5 - 0.5 * (PI * t).cos()
}

fn smoothstep(a: f64, b: f64, x: f64) -> f64 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The stage's arithmetic for one windowed frame, with no idea of a stream.
struct CueCore {
    bin_hz: f64,
    hop_s: f64,
    top: usize,
    edges: Vec<usize>,
    centres: Vec<f64>,
    cov: Vec<Cov>,
    energy: Vec<f64>,
    out: Vec<BandOut>,
    map: Vec<BinMap>,
    l: Vec<C>,
    r: Vec<C>,
    p: CueParams,
    law: LawTable,
    /// XTC follows: nothing below its band.
    xtc: bool,
    /// The bands each band is read with, and how many bins each band has.
    pool: Vec<(usize, usize)>,
    bins: Vec<f64>,
    alpha_a: f64,
    /// A window of hop N/4's frame power in one bin at full scale.
    norm: f64,
    // The gate.
    gcc: Vec<C>,
    gcc_fft: Arc<dyn Fft<f64>>,
    gcc_buf: Vec<C>,
    gcc_seen: f64,
    side: f64,
    mid: f64,
    conf: f64,
    open: bool,
    mono: bool,
    known: bool,
    /// How long, seconds, the gate has wanted what it has not done.
    gate_wants: f64,
    /// What the window shows: nothing of it is heard.
    look: Look,
}

// ─────────────────────────── the window's look ───────────────────────────
//
// The window's own reading of the stage, apart from what the stage plays (the
// bands above): bin by bin, so two sources sharing a band are two places; with
// a short memory, so a source that comes in or moves shows within a fraction
// of a second; down to the bass, so a low instrument has a place too; by level
// and by time, so a source a delay puts to one side stands there; each band
// by its loudness, so a quiet instrument beside a loud one shows. And each
// place's own sound, the bins that read at it, as a wave.

/// The places' memory, seconds.
const LOOK_TAU_S: f64 = 1.5;
/// A bin's reading is smoothed over this, seconds.
const BIN_TAU_S: f64 = 0.040;
/// A bin smoothed over less than this many of its time constants still
/// reads as one source whatever it holds: a look starts after them.
const BIN_WARM: f64 = 3.0;
/// The places' histogram: a cell a degree, hard right to hard left.
const DEG_N: usize = 181;
/// The most places a view lists.
const MOST_PLACES: usize = 8;
/// The histogram the places are fitted to remembers this long, seconds: a
/// source that comes in shows within a fraction of a second.
const HIST_TAU_S: f64 = 0.35;
/// How fast a place follows what it is fitted to, seconds: its centre, its
/// spread, its share (fast, so a source that stops shows going).
const MOVE_TAU_S: f64 = 0.35;
const SPREAD_TAU_S: f64 = 0.5;
const SHARE_TAU_S: f64 = 0.12;
/// A place's spread, degrees: no narrower, no wider.
const SPREAD_MIN: f64 = 2.5;
const SPREAD_MAX: f64 = 12.0;
/// What no place explains, standing out from the ground around it by this
/// share of the histogram within `BIRTH_REACH` degrees, is a new place; not
/// nearer than `BIRTH_GAP` degrees to one followed.
const BIRTH_SHARE: f64 = 0.04;
const BIRTH_REACH: usize = 8;
const BIRTH_GAP: f64 = 7.0;
/// A place whose peak has not come back for this long, seconds, is gone,
/// fading out over the last `HOLD_S`: an instrument heard on its own every
/// few seconds stays a place all the while.
const UNSEEN_S: f64 = 3.0;
const HOLD_S: f64 = 0.8;
/// The short histogram's memory, seconds: places are born on its peaks and
/// seen by them.
const BIRTH_TAU_S: f64 = 0.15;
/// A peak of the short histogram's bumps (each cell over the mean of
/// `BUMP_W` cells either side) sees a place when the bump is this share of
/// the cell at least, the cell this share of the highest one at least, and
/// it lies within this many times the place's reach. Bumps, not plain peaks:
/// an instrument on the slope of a dense centre's hump has no peak of its
/// own there, but a bump, and stays a place while the centre plays.
const BUMP_W: usize = 12;
const SEEN_BUMP: f64 = 0.05;
const SEEN_MIN: f64 = 0.02;
const SEEN_REACH: f64 = 1.5;
/// A place is shown once it has stood this long, seconds: no flicker.
const SHOW_S: f64 = 0.09;
/// Two places nearer than this, degrees, are one.
const MERGE_DEG: f64 = 6.0;
/// A new place is a peak this much over the histogram 6–10° either side.
const PEAK_CONTRAST: f64 = 1.1;
/// The delay that puts a source all the way to the side that leads, ms, the
/// level and the time trading against each other on the way (summing
/// localisation between two speakers: about 1.1 ms, as about 15 dB).
const TIME_FULL_MS: f64 = 1.1;
/// A delay under this, ms, is none; over the second, read as none too (a
/// longer one is an echo, or phases agreeing by chance).
const TIME_FLOOR_MS: f64 = 0.03;
const TIME_MOST_MS: f64 = 1.6;
/// Bins either side a delay is read over.
const TIME_REACH: usize = 3;
/// A bin counts by its power to this, loudness about: a quiet instrument
/// where it is the one heard counts nearly as much as a loud one.
const LOUD_POW: f64 = 0.3;
/// What a bin held before a sound began is read over this, seconds.
const ONSET_SLOW_S: f64 = 0.2;
/// A start's vote against a held sound's, loudness for loudness.
const ONSET_GAIN: f64 = 1.0;
/// The longest transform the waves are read back with: up to 20 kHz needs no more.
const WAVE_N: usize = 2048;
/// A band is read as the strongest place and a second source when the second
/// has this share of its power, from the first to the second figure, and the
/// band stands this far, the second pair, from the strongest place's own
/// point (closer, it is that place and a little diffuse sound).
const SPLIT_SHARE: (f64, f64) = (0.10, 0.30);
const SPLIT_GAP: (f64, f64) = (0.05, 0.15);
/// A band's point is read over this, seconds: the chord is drawn from far
/// enough out that a frame's chance does not swing it.
const SPLIT_TAU_S: f64 = 0.25;

/// A histogram smoothed over ±2 cells, a triangle.
fn smooth5(v: &[f64]) -> Vec<f64> {
    let n = v.len() as i64;
    (0..n)
        .map(|i| {
            let (mut s, mut w) = (0.0, 0.0);
            for d in -2i64..=2 {
                let j = i + d;
                if (0..n).contains(&j) {
                    let g = 3.0 - d.abs() as f64;
                    s += g * v[j as usize];
                    w += g;
                }
            }
            s / w
        })
        .collect()
}

/// Each cell over the histogram's own trend around it, the mean over
/// `w` cells either side: a peak shows even on the slope of a broad hump.
fn bumps(v: &[f64], w: usize) -> Vec<f64> {
    let n = v.len();
    (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(w), (i + w).min(n - 1));
            v[i] - v[a..=b].iter().sum::<f64>() / (b - a + 1) as f64
        })
        .collect()
}

/// How far cell `i` stands over the histogram 6–10 cells either side.
fn contrast_at(v: &[f64], i: usize) -> f64 {
    let mut g = 0.0f64;
    for d in 6..=10usize {
        for k in [i as i64 - d as i64, (i + d) as i64] {
            if (0..v.len() as i64).contains(&k) {
                g = g.max(v[k as usize]);
            }
        }
    }
    if g <= 0.0 { f64::INFINITY } else { v[i] / g }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// A source the window follows.
#[derive(Clone)]
struct Track {
    id: u32,
    /// Its centre and spread, degrees, and its share of the histogram.
    deg: f64,
    sigma: f64,
    share: f64,
    rho: f64,
    itd_ms: f64,
    spec_db: Vec<f64>,
    direct: f64,
    /// Seconds it has been fading out (see `UNSEEN_S`), and since it began.
    gone_s: f64,
    age_s: f64,
    /// Seconds since a peak of the short histogram was last seen at it.
    unseen_s: f64,
    /// Its wave since the last view.
    wave: Vec<f32>,
}

/// A bin as read: its place, its weight for the histogram (0: it has no
/// place), level difference, delay, power, how far it stands out, and its
/// covariance as a point of the disc of `split`.
#[derive(Clone, Copy, Default)]
struct Reading {
    deg: f64,
    h: f64,
    rho: f64,
    ms: f64,
    e: f64,
    sure: f64,
    x: f64,
    y: f64,
}

/// A band's sums over its bins, and, when it reads as two sources, the two
/// and how the band shares between them.
#[derive(Clone, Copy, Default)]
struct BandSum {
    e: f64,
    x: f64,
    y: f64,
    h: f64,
    h_ms: f64,
    /// Its bins' powers to `LOUD_POW`, summed.
    p: f64,
    /// How far the band is read as two (0: each bin at its own place).
    k: f64,
    /// The place followed's share of the band's power.
    t: f64,
    q: (f64, f64, f64),
    s: (f64, f64, f64),
    q_at: (f64, f64),
    s_at: (f64, f64),
}

struct Look {
    on: bool,
    /// The first bin read, the band of each bin.
    first: usize,
    band_of: Vec<u16>,
    bins: Vec<Cov>,
    a: f64,
    keep: f64,
    /// The histogram's own memory, shorter: the places are fitted to it.
    keep_h: f64,
    /// The share of the histogram that is no place: the floor of the mixture.
    floor: f64,
    /// Each histogram cell's place (index into `tracks`), `u8::MAX` none.
    own: Vec<u8>,
    /// Each bin over a longer time, what was there before a sound began, and
    /// how fast it follows; scratch for what came in this frame.
    slow: Vec<Cov>,
    a_slow: f64,
    rise: Vec<Cov>,
    /// Frames still to read before the bins say anything, and how many that is.
    warm: u32,
    warm_frames: u32,
    bands: usize,
    /// Scratch: a bin's place, NaN when it has none; the place it sounds from.
    deg_k: Vec<f64>,
    owner: Vec<u8>,
    coh: Vec<f64>,
    /// Scratch: each bin's reading, each band's sums and how it is read.
    reads: Vec<Reading>,
    sums: Vec<BandSum>,
    /// Each band's power, and its point times it, over `SPLIT_TAU_S`.
    band_at: Vec<[f64; 3]>,
    a_band: f64,
    // The histogram: weight, and weight × level difference, × delay.
    hist: Vec<f64>,
    hist_rho: Vec<f64>,
    hist_ms: Vec<f64>,
    /// The same histogram over a shorter memory: places are born on its peaks.
    hist_b: Vec<f64>,
    keep_b: f64,
    // Band by place, what makes a place's image: energy that stands out, and all of it.
    cell_sure: Vec<f64>,
    cell_all: Vec<f64>,
    tracks: Vec<Track>,
    /// The waves' transform, its buffer, 1 / the window over the middle hop.
    m: usize,
    ifft: Arc<dyn Fft<f64>>,
    buf: Vec<C>,
    unwin: Vec<f64>,
    /// The newest hop's number, counted from the track's start: every chain
    /// of a track numbers its hops alike.
    seq: u64,
}

/// What a look has learnt, to go on from in another chain.
struct LookState {
    bins: Vec<Cov>,
    slow: Vec<Cov>,
    warm: u32,
    band_at: Vec<[f64; 3]>,
    floor: f64,
    own: Vec<u8>,
    hist: Vec<f64>,
    hist_rho: Vec<f64>,
    hist_ms: Vec<f64>,
    hist_b: Vec<f64>,
    cell_sure: Vec<f64>,
    cell_all: Vec<f64>,
    tracks: Vec<Track>,
}

impl Look {
    fn state(&self) -> LookState {
        LookState {
            bins: self.bins.clone(),
            slow: self.slow.clone(),
            warm: self.warm,
            band_at: self.band_at.clone(),
            floor: self.floor,
            own: self.own.clone(),
            hist: self.hist.clone(),
            hist_rho: self.hist_rho.clone(),
            hist_ms: self.hist_ms.clone(),
            hist_b: self.hist_b.clone(),
            cell_sure: self.cell_sure.clone(),
            cell_all: self.cell_all.clone(),
            tracks: self.tracks.iter().map(|t| Track { wave: Vec::new(), ..t.clone() }).collect(),
        }
    }

    /// Goes on from `s`, read by a look of the same shape.
    fn adopt(&mut self, s: &LookState) {
        if s.bins.len() != self.bins.len() || s.cell_all.len() != self.cell_all.len() {
            return;
        }
        self.bins.clone_from(&s.bins);
        self.slow.clone_from(&s.slow);
        self.warm = s.warm;
        self.band_at.clone_from(&s.band_at);
        self.floor = s.floor;
        self.own.clone_from(&s.own);
        self.hist.clone_from(&s.hist);
        self.hist_rho.clone_from(&s.hist_rho);
        self.hist_ms.clone_from(&s.hist_ms);
        self.hist_b.clone_from(&s.hist_b);
        self.cell_sure.clone_from(&s.cell_sure);
        self.cell_all.clone_from(&s.cell_all);
        self.tracks = s.tracks.clone();
    }

    fn new(n: usize, hop_s: f64, top: usize, edges: &[usize]) -> Look {
        let bands = edges.len() - 1;
        let mut band_of = vec![0u16; top + 1];
        for b in 0..bands {
            for k in edges[b]..edges[b + 1] {
                band_of[k] = b as u16;
            }
        }
        let m = n.min(WAVE_N);
        let unwin = (m / 2 - m / 8..m / 2 + m / 8).map(|j| 1.0 / (PI * j as f64 / m as f64).sin()).collect();
        Look {
            on: false,
            first: edges[0],
            band_of,
            bins: vec![Cov::default(); top + 1],
            slow: vec![Cov::default(); top + 1],
            a_slow: 1.0 - (-hop_s / ONSET_SLOW_S).exp(),
            rise: vec![Cov::default(); top + 1],
            a: 1.0 - (-hop_s / BIN_TAU_S).exp(),
            keep: (-hop_s / LOOK_TAU_S).exp(),
            keep_h: (-hop_s / HIST_TAU_S).exp(),
            floor: 1.0,
            own: vec![u8::MAX; DEG_N],
            warm: (BIN_WARM * BIN_TAU_S / hop_s).ceil() as u32,
            warm_frames: (BIN_WARM * BIN_TAU_S / hop_s).ceil() as u32,
            a_band: 1.0 - (-hop_s / SPLIT_TAU_S).exp(),
            bands,
            deg_k: vec![f64::NAN; top + 1],
            owner: vec![u8::MAX; top + 1],
            coh: vec![0.0; top + 1],
            reads: vec![Reading::default(); top + 1],
            sums: vec![BandSum::default(); bands],
            band_at: vec![[0.0; 3]; bands],
            hist: vec![0.0; DEG_N],
            hist_rho: vec![0.0; DEG_N],
            hist_ms: vec![0.0; DEG_N],
            hist_b: vec![0.0; DEG_N],
            keep_b: (-hop_s / BIRTH_TAU_S).exp(),
            cell_sure: vec![0.0; bands * DEG_N],
            cell_all: vec![0.0; bands * DEG_N],
            tracks: Vec::new(),
            m,
            ifft: FftPlanner::<f64>::new().plan_fft_inverse(m),
            buf: vec![C::new(0.0, 0.0); m],
            unwin,
            seq: 0,
        }
    }

    /// Nobody looks: what was read goes, a look starts afresh.
    fn off(&mut self) {
        if !self.on {
            return;
        }
        self.on = false;
        for v in [&mut self.hist, &mut self.hist_rho, &mut self.hist_ms, &mut self.hist_b, &mut self.cell_sure, &mut self.cell_all] {
            v.fill(0.0);
        }
        self.bins.fill(Cov::default());
        self.slow.fill(Cov::default());
        self.warm = self.warm_frames;
        self.band_at.fill([0.0; 3]);
        self.tracks.clear();
        self.floor = 1.0;
        self.own.fill(u8::MAX);
    }

    /// One frame's spectra of L and R (windowed, `norm` to full scale), the
    /// `n` they were read over.
    fn frame(&mut self, l: &[C], r: &[C], n: usize, norm: f64, law: &LawTable, bin_hz: f64) {
        self.read(l, r, norm, law, bin_hz);
        self.waves(l, r, n);
        self.seq += 1;
    }

    fn read(&mut self, l: &[C], r: &[C], norm: f64, law: &LawTable, bin_hz: f64) {
        let (k0, top) = (self.first, l.len() - 1);
        for k in k0..=top {
            let x = l[k] * r[k].conj();
            let c = Cov { ll: l[k].norm_sqr() * norm, rr: r[k].norm_sqr() * norm, re: x.re * norm, im: x.im * norm };
            self.bins[k].go(&c, self.a);
        }
        // A frame or two in, every bin is one source: its coherence 1, the
        // phases of a mix wandering from bin to bin as a delay would.
        if self.warm > 0 {
            self.warm -= 1;
            return;
        }
        self.read_bins(l, r, norm, law, bin_hz);
    }

    fn read_bins(&mut self, l: &[C], r: &[C], norm: f64, law: &LawTable, bin_hz: f64) {
        let (k0, top) = (self.first, l.len() - 1);
        let cross = |c: &Cov| C::new(c.re, c.im);
        // How much of a bin is one source: its coherence, 1 for one source
        // however panned or delayed, under 1 where two at different places
        // meet. A channel far under the other has no say: one source.
        for k in k0..=top {
            let c = &self.bins[k];
            let (lo, hi) = (c.ll.min(c.rr), c.ll.max(c.rr));
            self.coh[k] = if hi <= 0.0 {
                0.0
            } else if lo < 1e-6 * hi {
                1.0
            } else {
                ((c.re * c.re + c.im * c.im) / (c.ll * c.rr)).min(1.0)
            };
        }
        // A step of the phase from bin to bin, radians, as ms of delay.
        let ms_per_rad = 1e3 / (2.0 * PI * bin_hz);
        let bin_pow = LOUD_POW;
        self.sums.fill(BandSum::default());
        for k in k0..=top {
            self.deg_k[k] = f64::NAN;
            self.reads[k] = Reading::default();
            let c = self.bins[k];
            let (e, b) = (c.ll + c.rr, self.band_of[k] as usize);
            if e < 1e-14 {
                continue;
            }
            // The bin's covariance as a point of the disc (see `split`).
            let (px, py) = ((c.ll - c.rr) / e, 2.0 * cross(&c).norm() / e);
            let s = &mut self.sums[b];
            s.e += e;
            s.p += e.powf(bin_pow);
            s.x += px * e;
            s.y += py * e;
            let (alpha, sure) = c.axis();
            let a = alpha.clamp(1e-3, PI / 2.0 - 1e-3);
            let rho = (20.0 * (a.cos() / a.sin()).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
            // A delay between the channels turns the phase in step with the
            // frequency: read over the neighbours that are one source each, a
            // delay is that step the same in all of them; where sources meet,
            // or in reverberation, the phases agree on none. A delay of a
            // fraction of a radian a bin is ms, so only an agreement of nearly
            // all of them is one.
            let (j0, j1) = (k.saturating_sub(TIME_REACH).max(k0), (k + TIME_REACH).min(top - 1));
            let (mut p, mut q) = (C::new(0.0, 0.0), 0.0);
            for j in j0..=j1 {
                let g = smoothstep(0.8, 0.95, self.coh[j].min(self.coh[j + 1]));
                let (x0, x1) = (cross(&self.bins[j]), cross(&self.bins[j + 1]));
                let s = x1.norm() * x0.norm();
                if g > 0.0 && s > 0.0 {
                    p += x1 * x0.conj() * (g / s);
                    q += g;
                }
            }
            let mut ms = if q >= 2.0 { p.arg() * ms_per_rad * smoothstep(0.9, 0.98, p.norm() / q) } else { 0.0 };
            if ms.abs() < TIME_FLOOR_MS || ms.abs() > TIME_MOST_MS {
                ms = 0.0;
            }
            // In phase once the delay is taken off, as the stage's bands ask.
            let x = cross(&c);
            let in_phase = if x.norm() > 0.0 { smoothstep(0.0, 0.5, (x.arg() - 2.0 * PI * k as f64 * bin_hz * ms * 1e-3).cos()) } else { 1.0 };
            let lean = smoothstep(LEAN_FROM_DB, LEAN_FULL_DB, rho.abs());
            let w = smoothstep(0.5, 0.85, self.coh[k]) * (lean + (1.0 - lean) * in_phase);
            self.reads[k] = Reading { deg: f64::NAN, h: 0.0, rho, ms, e, sure, x: px, y: py };
            if w <= 0.0 {
                continue;
            }
            let deg = 90.0 * (law.deg(rho) / 90.0 + ms / TIME_FULL_MS).clamp(-1.0, 1.0);
            let h = e.powf(bin_pow) * w;
            self.reads[k].deg = deg;
            self.reads[k].h = h;
            let s = &mut self.sums[b];
            s.h += h;
            s.h_ms += h * ms;
        }
        // A band counts by its loudness, its power to `LOUD_POW`, not by how
        // many bins it has: shared among its bins by theirs to the same power,
        // so a quiet instrument where it is the one heard still has its say,
        // while the top octaves' hundreds of bins weigh no more than the
        // bass's few.
        for k in k0..=top {
            let s = &self.sums[self.band_of[k] as usize];
            if self.reads[k].h > 0.0 {
                self.reads[k].h *= s.e.powf(LOUD_POW) / s.p;
            }
        }
        for s in self.sums.iter_mut() {
            if s.p > 0.0 {
                let g = s.e.powf(LOUD_POW) / s.p;
                s.h *= g;
                s.h_ms *= g;
            }
        }
        let a = self.a_band;
        for (at, s) in self.band_at.iter_mut().zip(&self.sums) {
            for (v, to) in at.iter_mut().zip([s.e, s.x, s.y]) {
                *v += a * (to - *v);
            }
        }
        self.split(law);
        for k in k0..=top {
            let rd = self.reads[k];
            let b = self.band_of[k] as usize;
            let s = self.sums[b];
            if rd.h > 0.0 {
                self.deg_k[k] = rd.deg;
                self.add(b, rd.deg, (1.0 - s.k) * rd.h, rd.rho, rd.ms, (1.0 - s.k) * rd.e * rd.sure, (1.0 - s.k) * rd.e);
            }
            // A bin of a band read as two sounds from the one it is nearer.
            if s.k >= 0.5 && rd.e > 0.0 {
                let near = |p: (f64, f64)| (rd.x - p.0).powi(2) + (rd.y - p.1).powi(2);
                self.deg_k[k] = if near(s.q_at) <= near(s.s_at) { s.q.0 } else { s.s.0 };
            }
        }
        for b in 0..self.bands {
            let s = self.sums[b];
            if s.k <= 0.0 {
                continue;
            }
            for (share, (deg, rho, ms)) in [(s.t, s.q), (1.0 - s.t, s.s)] {
                // What a source with that share of the band's bins would have
                // weighed, each bin by its power to `LOUD_POW`; heard as direct.
                let e = s.k * share * s.e;
                self.add(b, deg, s.k * share.powf(LOUD_POW) * s.h, rho, ms, e, e);
            }
        }
        self.onsets(l, r, norm, law, bin_hz);
        let (keep, keep_h) = (self.keep, self.keep_h);
        for v in [&mut self.hist, &mut self.hist_rho, &mut self.hist_ms] {
            for h in v.iter_mut() {
                *h *= keep_h;
            }
        }
        let keep_b = self.keep_b;
        for h in self.hist_b.iter_mut() {
            *h *= keep_b;
        }
        for v in [&mut self.cell_sure, &mut self.cell_all] {
            for h in v.iter_mut() {
                *h *= keep;
            }
        }
    }

    /// What came in this frame, bin by bin: the frame less what the bin held
    /// before. A note's start over a held sound is the note alone, at its own
    /// place, however much louder the held one is; and the ear places a
    /// sound by its start. Each start votes where it is, by its loudness.
    fn onsets(&mut self, l: &[C], r: &[C], norm: f64, law: &LawTable, bin_hz: f64) {
        let gain = ONSET_GAIN;
        let (k0, top) = (self.first, l.len() - 1);
        let mut band = vec![(0.0f64, 0.0f64); self.bands];
        for k in k0..=top {
            let x = l[k] * r[k].conj();
            let c = Cov { ll: l[k].norm_sqr() * norm, rr: r[k].norm_sqr() * norm, re: x.re * norm, im: x.im * norm };
            let o = self.slow[k];
            let d = Cov { ll: (c.ll - o.ll).max(0.0), rr: (c.rr - o.rr).max(0.0), re: c.re - o.re, im: c.im - o.im };
            let de = d.ll + d.rr;
            // Only what is mostly new: a held sound's frame-to-frame wobble is not a start.
            let new = if de > 0.0 { smoothstep(0.3, 0.7, de / (c.ll + c.rr)) } else { 0.0 };
            self.rise[k] = if new > 0.0 && de > 1e-14 { d } else { Cov::default() };
            self.coh[k] = new;
            if new > 0.0 && de > 1e-14 {
                let b = &mut band[self.band_of[k] as usize];
                b.0 += de;
                b.1 += de.powf(LOUD_POW);
            }
            self.slow[k].go(&c, self.a_slow);
        }
        if gain <= 0.0 || self.warm > 0 {
            return;
        }
        let ms_per_rad = 1e3 / (2.0 * PI * bin_hz);
        let cross = |c: &Cov| C::new(c.re, c.im);
        let one = |c: &Cov| {
            let (lo, hi) = (c.ll.min(c.rr), c.ll.max(c.rr));
            if hi <= 0.0 {
                0.0
            } else if lo < 1e-3 * hi {
                1.0
            } else {
                ((c.re * c.re + c.im * c.im) / (c.ll * c.rr)).min(1.0)
            }
        };
        for k in k0..=top {
            let d = self.rise[k];
            let de = d.ll + d.rr;
            if de <= 1e-14 {
                continue;
            }
            let coh = one(&d);
            let w = smoothstep(0.5, 0.85, coh) * self.coh[k];
            if w <= 0.0 {
                continue;
            }
            let (alpha, _) = d.axis();
            let a = alpha.clamp(1e-3, PI / 2.0 - 1e-3);
            let rho = (20.0 * (a.cos() / a.sin()).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
            let (j0, j1) = (k.saturating_sub(TIME_REACH).max(k0), (k + TIME_REACH).min(top - 1));
            let (mut p, mut q) = (C::new(0.0, 0.0), 0.0);
            for j in j0..=j1 {
                let (a0, a1) = (&self.rise[j], &self.rise[j + 1]);
                let g = smoothstep(0.8, 0.95, one(a0).min(one(a1)));
                let (x0, x1) = (cross(a0), cross(a1));
                let s = x1.norm() * x0.norm();
                if g > 0.0 && s > 0.0 {
                    p += x1 * x0.conj() * (g / s);
                    q += g;
                }
            }
            let mut ms = if q >= 2.0 { p.arg() * ms_per_rad * smoothstep(0.9, 0.98, p.norm() / q) } else { 0.0 };
            if ms.abs() < TIME_FLOOR_MS || ms.abs() > TIME_MOST_MS {
                ms = 0.0;
            }
            let x = cross(&d);
            let in_phase = if x.norm() > 0.0 { smoothstep(0.0, 0.5, (x.arg() - 2.0 * PI * k as f64 * bin_hz * ms * 1e-3).cos()) } else { 1.0 };
            let lean = smoothstep(LEAN_FROM_DB, LEAN_FULL_DB, rho.abs());
            let w = w * (lean + (1.0 - lean) * in_phase);
            let (be, bp) = band[self.band_of[k] as usize];
            let h = gain * de.powf(LOUD_POW) * w * be.powf(LOUD_POW) / bp;
            let deg = 90.0 * (law.deg(rho) / 90.0 + ms / TIME_FULL_MS).clamp(-1.0, 1.0);
            let i = (deg + 90.0).round().clamp(0.0, (DEG_N - 1) as f64) as usize;
            self.hist[i] += h;
            self.hist_rho[i] += h * rho;
            self.hist_ms[i] += h * ms;
            self.hist_b[i] += h;
        }
    }

    /// Weight `h` at `deg` with its level difference and delay, and band `b`'s
    /// energy there, `sure` of it standing out.
    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, b: usize, deg: f64, h: f64, rho: f64, ms: f64, sure: f64, all: f64) {
        let i = (deg + 90.0).round().clamp(0.0, (DEG_N - 1) as f64) as usize;
        self.hist[i] += h;
        self.hist_rho[i] += h * rho;
        self.hist_ms[i] += h * ms;
        self.hist_b[i] += h;
        self.cell_sure[b * DEG_N + i] += sure;
        self.cell_all[b * DEG_N + i] += all;
    }

    /// Each band read, where it can be, as two sources: the strongest place
    /// followed and a second one.
    ///
    /// A 2×2 covariance over its trace is a point of a disc, (L² − R²,
    /// 2·|L·R*|) over (L² + R²): a single source, however panned, on its rim at
    /// twice its pan angle, and a mix of two on the chord between theirs, as
    /// far along it as their powers share. One bin of two sources reads, as
    /// its principal axis, a place between them, and so does the band; but
    /// once one of the two is known, the chord from its point through the
    /// band's goes on to the other's. A quiet instrument above a loud one is
    /// found that way where no bin of it stands alone, and two sources sharing
    /// bins stay two places, not one between them. Diffuse sound puts the band
    /// towards the middle of the disc, the chord from the place through it
    /// leaves on the far side, out of phase: no second source, the bins are
    /// read each at its own place.
    ///
    /// Only the strongest place: a band explained against any place might be
    /// explained by places that are not there.
    fn split(&mut self, law: &LawTable) {
        let Some(q) = self.tracks.iter().filter(|t| t.gone_s == 0.0).max_by(|a, b| a.share.total_cmp(&b.share)) else {
            return;
        };
        let (q_deg, q_rho, q_ms) = (q.deg, q.rho, q.itd_ms);
        let a = (10f64.powf(-q_rho / 20.0)).atan();
        let (qx, qy) = ((2.0 * a).cos(), (2.0 * a).sin());
        for (s, at) in self.sums.iter_mut().zip(&self.band_at) {
            if s.e <= 0.0 || s.h <= 0.0 || at[0] <= 0.0 {
                continue;
            }
            let (dx, dy) = (at[1] / at[0] - qx, at[2] / at[0] - qy);
            let dd = dx * dx + dy * dy;
            if dd < 1e-12 {
                continue;
            }
            // Q + f·D on the rim, beyond the band's point (f ≥ 1).
            let f = (-2.0 * (qx * dx + qy * dy) / dd).max(1.0);
            let (sx, sy) = (qx + f * dx, qy + f * dy);
            if sy <= 0.0 {
                continue;
            }
            let t = 1.0 - 1.0 / f;
            s.k = smoothstep(SPLIT_SHARE.0, SPLIT_SHARE.1, 1.0 - t) * smoothstep(SPLIT_GAP.0, SPLIT_GAP.1, dd.sqrt());
            if s.k <= 0.0 {
                continue;
            }
            let a = (0.5 * sy.atan2(sx)).clamp(1e-3, PI / 2.0 - 1e-3);
            let rho = (20.0 * (a.cos() / a.sin()).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
            let ms = s.h_ms / s.h;
            let deg = 90.0 * (law.deg(rho) / 90.0 + ms / TIME_FULL_MS).clamp(-1.0, 1.0);
            s.t = t;
            s.q = (q_deg, q_rho, q_ms);
            s.s = (deg, rho, ms);
            s.q_at = (qx, qy);
            s.s_at = (sx, sy);
        }
    }

    /// Each place's sound this hop: the bins that read at it, taken along
    /// its own pan (the channels as it was mixed into them), back to time.
    /// The middle hop of the frame, over its window, is the hop's wave.
    fn waves(&mut self, l: &[C], r: &[C], n: usize) {
        if self.tracks.is_empty() {
            return;
        }
        let (k0, top, m) = (self.first, l.len() - 1, self.m);
        for k in k0..=top {
            let d = self.deg_k[k];
            self.owner[k] = u8::MAX;
            if d.is_nan() {
                continue;
            }
            self.owner[k] = self.own[(d + 90.0).round().clamp(0.0, (DEG_N - 1) as f64) as usize];
        }
        let j = C::new(0.0, 1.0);
        for pair in (0..self.tracks.len()).step_by(2) {
            self.buf.fill(C::new(0.0, 0.0));
            for (s, t) in [(C::new(1.0, 0.0), pair), (j, pair + 1)] {
                let Some(tr) = self.tracks.get(t) else { continue };
                let a = (10f64.powf(-tr.rho / 20.0)).atan();
                let (ca, sa) = (a.cos(), a.sin());
                for k in k0..=top {
                    if self.owner[k] as usize == t {
                        let v = l[k] * ca + r[k] * sa;
                        self.buf[k] += s * v;
                        self.buf[m - k] += s * v.conj();
                    }
                }
            }
            self.ifft.process(&mut self.buf);
            let mut lo = [f64::INFINITY; 2];
            let mut hi = [f64::NEG_INFINITY; 2];
            let u = 1.0 / n as f64;
            for (i, w) in self.unwin.iter().enumerate() {
                let z = self.buf[m / 2 - m / 8 + i] * (w * u);
                for (h, v) in [z.re, z.im].into_iter().enumerate() {
                    lo[h] = lo[h].min(v);
                    hi[h] = hi[h].max(v);
                }
            }
            for h in 0..2 {
                if let Some(tr) = self.tracks.get_mut(pair + h) {
                    tr.wave.push(lo[h] as f32);
                    tr.wave.push(hi[h] as f32);
                }
            }
        }
    }

    /// Each cell's share for each place and for the floor: a bell for each
    /// place, its share spread over its spread, and the floor flat.
    fn shares(&self) -> (Vec<f64>, Vec<f64>) {
        let n = self.tracks.len();
        let mut r = vec![0.0; n * DEG_N];
        let mut bg = vec![0.0; DEG_N];
        let flat = self.floor.max(0.02) / DEG_N as f64;
        for i in 0..DEG_N {
            let x = i as f64 - 90.0;
            let mut sum = flat;
            for (t, tr) in self.tracks.iter().enumerate() {
                let z = (x - tr.deg) / tr.sigma;
                let p = tr.share.max(1e-4) * (-0.5 * z * z).exp() / (tr.sigma * (2.0 * PI).sqrt());
                r[t * DEG_N + i] = p;
                sum += p;
            }
            for t in 0..n {
                r[t * DEG_N + i] /= sum;
            }
            bg[i] = flat / sum;
        }
        (r, bg)
    }

    /// The places. A place is born where a peak of the short histogram
    /// stands out from what the places so far explain, and lives as long as
    /// such a peak comes back near it now and then: an instrument that is
    /// heard on its own for a moment, every few seconds, is a place all the
    /// while, even where the whole mix reads as one broad hump. Its centre
    /// follows its peaks slowly and stays put between them, so it does not
    /// shake or slide into a louder neighbour. Each place is also a bell of
    /// the longer histogram's mixture (spread, share, over a flat floor for
    /// what has no place): its share is how bright it is, and which cells,
    /// and so which bins, are its own. `dt` seconds since the last time.
    fn places(&mut self, dt: f64) {
        let total: f64 = self.hist.iter().sum();
        let total_b: f64 = self.hist_b.iter().sum();
        for t in self.tracks.iter_mut() {
            t.age_s += dt;
        }
        let a = |tau: f64| 1.0 - (-dt / tau).exp();
        let (a_mu, a_sig, a_pi) = (a(MOVE_TAU_S), a(SPREAD_TAU_S), a(SHARE_TAU_S));
        if total > 1e-30 && total_b > 1e-30 {
            let h: Vec<f64> = self.hist.iter().map(|v| v / total).collect();
            let hb: Vec<f64> = self.hist_b.iter().map(|v| v / total_b).collect();
            let sh = smooth5(&hb);
            // Each place seen where the short histogram has a bump near it
            // (a peak over its local trend, so on a dense centre's slope too),
            // the nearest place taking it.
            let top = sh.iter().cloned().fold(0.0, f64::max);
            let bump = bumps(&sh, BUMP_W);
            let mut seen: Vec<Option<(f64, f64)>> = vec![None; self.tracks.len()];
            for i in 1..DEG_N - 1 {
                let (l, c, r) = (bump[i - 1], bump[i], bump[i + 1]);
                if !(c >= l && c > r && c > SEEN_BUMP * sh[i] && sh[i] >= SEEN_MIN * top) {
                    continue;
                }
                let den = l - 2.0 * c + r;
                let at = i as f64 - 90.0 + if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
                let near = self
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(t, tr)| (t, (tr.deg - at).abs(), SEEN_REACH * BIRTH_GAP.max(tr.sigma)))
                    .filter(|&(_, d, reach)| d < reach)
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((t, _, _)) = near {
                    if seen[t].is_none_or(|(_, v)| v < c) {
                        seen[t] = Some((at, c));
                    }
                }
            }
            // One step of the mixture's fit, each place taking its part of
            // every cell; the centre from the peaks.
            let (r, bg) = self.shares();
            for (t, tr) in self.tracks.iter_mut().enumerate() {
                let row = &r[t * DEG_N..(t + 1) * DEG_N];
                let m: f64 = h.iter().zip(row).map(|(h, r)| h * r).sum();
                if m > 1e-9 {
                    let mu = h.iter().zip(row).enumerate().map(|(i, (h, r))| h * r * (i as f64 - 90.0)).sum::<f64>() / m;
                    let var = h.iter().zip(row).enumerate().map(|(i, (h, r))| h * r * (i as f64 - 90.0 - mu).powi(2)).sum::<f64>() / m;
                    tr.sigma += a_sig * (var.sqrt().clamp(SPREAD_MIN, SPREAD_MAX) - tr.sigma);
                }
                tr.share += a_pi * (m - tr.share);
                match seen[t] {
                    Some((at, _)) => {
                        tr.deg += a_mu * (at - tr.deg);
                        tr.unseen_s = 0.0;
                    }
                    None => tr.unseen_s += dt,
                }
            }
            let m_bg: f64 = h.iter().zip(&bg).map(|(h, r)| h * r).sum();
            self.floor += a_pi * (m_bg - self.floor);
            // Two places on one: the older stays, with both shares.
            let mut t = 0;
            while t < self.tracks.len() {
                let near = (0..t).find(|&o| (self.tracks[o].deg - self.tracks[t].deg).abs() < MERGE_DEG);
                match near {
                    Some(o) => {
                        let q = self.tracks.remove(t);
                        let p = &mut self.tracks[o];
                        let w = p.share + q.share;
                        if w > 0.0 {
                            p.deg = (p.deg * p.share + q.deg * q.share) / w;
                        }
                        p.share = w;
                        p.age_s = p.age_s.max(q.age_s);
                        p.unseen_s = p.unseen_s.min(q.unseen_s);
                    }
                    None => t += 1,
                }
            }
            // A new place where what no place explains stands out, at a peak
            // of the short histogram (not on the slope of a place's bell that
            // the bell misses).
            let (_, bg) = self.shares();
            let res: Vec<f64> = (0..DEG_N).map(|i| hb[i] * bg[i]).collect();
            let sm: Vec<f64> = (0..DEG_N)
                .map(|i| {
                    let (mut s, mut w) = (0.0, 0.0);
                    for d in -3i64..=3 {
                        let j = i as i64 + d;
                        if (0..DEG_N as i64).contains(&j) {
                            let g = 4.0 - d.abs() as f64;
                            s += g * res[j as usize];
                            w += g;
                        }
                    }
                    s / w
                })
                .collect();
            let is_peak = |j: usize| {
                (j.saturating_sub(3)..=(j + 3).min(DEG_N - 1)).any(|i| {
                    let (l, r) = (if i > 0 { sh[i - 1] } else { 0.0 }, if i + 1 < DEG_N { sh[i + 1] } else { 0.0 });
                    sh[i] >= l && sh[i] >= r && contrast_at(&sh, i) >= PEAK_CONTRAST
                })
            };
            if self.tracks.len() < MOST_PLACES {
                let mut order: Vec<usize> = (0..DEG_N).collect();
                order.sort_by(|&x, &y| sm[y].total_cmp(&sm[x]));
                for &j in order.iter().take(3) {
                    let x = j as f64 - 90.0;
                    if self.tracks.iter().any(|t| (t.deg - x).abs() < BIRTH_GAP.max(t.sigma)) || !is_peak(j) {
                        continue;
                    }
                    let (lo, hi) = (j.saturating_sub(BIRTH_REACH), (j + BIRTH_REACH).min(DEG_N - 1));
                    let mass: f64 = res[lo..=hi].iter().sum();
                    // The ground: what lies just beyond, on either side there is.
                    let (mut g, mut gn) = (0.0, 0.0);
                    for d in BIRTH_REACH + 4..=BIRTH_REACH + 12 {
                        for k in [j as i64 - d as i64, (j + d) as i64] {
                            if (0..DEG_N as i64).contains(&k) {
                                g += res[k as usize];
                                gn += 1.0;
                            }
                        }
                    }
                    let ground = if gn > 0.0 { g / gn } else { 0.0 };
                    let excess = mass - ground * (hi - lo + 1) as f64;
                    if excess < BIRTH_SHARE {
                        continue;
                    }
                    let mu = (lo..=hi).map(|i| res[i] * (i as f64 - 90.0)).sum::<f64>() / mass;
                    self.tracks.push(Track {
                        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                        deg: mu,
                        sigma: 4.0,
                        share: excess,
                        rho: 0.0,
                        itd_ms: 0.0,
                        spec_db: Vec::new(),
                        direct: 0.0,
                        gone_s: 0.0,
                        age_s: 0.0,
                        unseen_s: 0.0,
                        wave: Vec::new(),
                    });
                    self.floor = (self.floor - excess).max(0.02);
                    break;
                }
            }
        } else {
            for t in self.tracks.iter_mut() {
                t.unseen_s += dt;
            }
        }
        // Unseen for `UNSEEN_S`, a place is gone, fading out over the last `HOLD_S`.
        for t in self.tracks.iter_mut() {
            t.gone_s = (t.unseen_s - (UNSEEN_S - HOLD_S)).max(0.0);
        }
        self.tracks.retain(|t| t.gone_s <= HOLD_S);
        self.measure();
    }

    /// Each place's readings, from the cells it takes, and the cells it owns.
    fn measure(&mut self) {
        let (r, bg) = self.shares();
        for i in 0..DEG_N {
            let best = (0..self.tracks.len()).max_by(|&x, &y| r[x * DEG_N + i].total_cmp(&r[y * DEG_N + i]));
            self.own[i] = match best {
                Some(t) if r[t * DEG_N + i] > bg[i] && r[t * DEG_N + i] > 0.3 => t as u8,
                _ => u8::MAX,
            };
        }
        for t in 0..self.tracks.len() {
            let row = &r[t * DEG_N..(t + 1) * DEG_N];
            let h: f64 = self.hist.iter().zip(row).map(|(h, r)| h * r).sum();
            let (mut sure, mut all) = (0.0, 0.0);
            let spec_db = (0..self.bands)
                .map(|b| {
                    let c = b * DEG_N;
                    let s: f64 = self.cell_sure[c..c + DEG_N].iter().zip(row).map(|(v, r)| v * r).sum();
                    sure += s;
                    all += self.cell_all[c..c + DEG_N].iter().zip(row).map(|(v, r)| v * r).sum::<f64>();
                    (100.0 * s.max(1e-12).log10()).round() / 10.0
                })
                .collect();
            let tr = &mut self.tracks[t];
            if h > 0.0 {
                tr.rho = self.hist_rho.iter().zip(row).map(|(v, r)| v * r).sum::<f64>() / h;
                tr.itd_ms = self.hist_ms.iter().zip(row).map(|(v, r)| v * r).sum::<f64>() / h;
            }
            tr.spec_db = spec_db;
            tr.direct = if all > 0.0 { (sure / all).min(1.0) } else { 0.0 };
        }
    }

    /// The places as the view lists them, the strongest first, each with its
    /// wave since the last view (handed over: the next view starts afresh).
    /// A place only just begun is not listed yet.
    fn dots(&mut self) -> Vec<CueDot> {
        let top = self.tracks.iter().map(|t| t.share).fold(0.0, f64::max).max(1e-12);
        let mut v: Vec<CueDot> = Vec::new();
        for t in self.tracks.iter_mut() {
            let wave = std::mem::take(&mut t.wave);
            if t.age_s < SHOW_S {
                continue;
            }
            v.push(CueDot {
                id: t.id,
                deg: (t.deg * 10.0).round() / 10.0,
                rho_db: t.rho,
                itd_ms: (t.itd_ms * 1000.0).round() / 1000.0,
                // A place going fades out over its hold.
                weight: (t.share / top).min(1.0) * (1.0 - t.gone_s / HOLD_S).max(0.0),
                spec_db: t.spec_db.clone(),
                direct: t.direct,
                wave,
            });
        }
        v.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        v
    }
}

impl CueCore {
    fn new(n: usize, rate: u32, hop: usize, xtc: bool) -> CueCore {
        let bin_hz = rate as f64 / n as f64;
        let top = ((TOP_HZ / bin_hz).floor() as usize).min(n / 2 - 1);
        let edges = band_edges(bin_hz, top);
        let edges_look = edges.clone();
        let bands = edges.len() - 1;
        let centres: Vec<f64> = (0..bands).map(|b| 0.5 * (edges[b] + edges[b + 1] - 1) as f64 * bin_hz).collect();
        let bins: Vec<f64> = (0..bands).map(|b| (edges[b + 1] - edges[b]) as f64).collect();
        // A band narrower than `POOL_HZ` is read with those whose centres are
        // within half of it; a wider one is on its own.
        let pool = (0..bands)
            .map(|b| {
                let a = centres.partition_point(|&c| c < centres[b] - POOL_HZ / 2.0);
                let e = centres.partition_point(|&c| c <= centres[b] + POOL_HZ / 2.0);
                (a.min(b), e.max(b + 1))
            })
            .collect();
        // GCC-PHAT's own short transform, up to ~5.5 kHz.
        let gcc_n = 512usize;
        let mut planner = FftPlanner::<f64>::new();
        let gcc_fft = planner.plan_fft_inverse(gcc_n);
        let mut c = CueCore {
            bin_hz,
            hop_s: hop as f64 / rate as f64,
            top,
            edges,
            centres,
            cov: vec![Cov::default(); bands],
            energy: vec![0.0; bands],
            out: vec![BandOut::default(); bands],
            map: Vec::new(),
            l: vec![C::new(0.0, 0.0); top + 1],
            r: vec![C::new(0.0, 0.0); top + 1],
            p: CueParams::default(),
            law: LawTable::new(PanLaw::Sin3),
            xtc,
            pool,
            bins,
            alpha_a: 0.0,
            norm: 2.0 / n as f64,
            gcc: vec![C::new(0.0, 0.0); gcc_n / 2 + 1],
            gcc_fft,
            gcc_buf: vec![C::new(0.0, 0.0); gcc_n],
            gcc_seen: 0.0,
            side: 0.0,
            mid: 0.0,
            conf: 0.0,
            open: false,
            mono: false,
            known: false,
            gate_wants: 0.0,
            look: Look::new(n, hop as f64 / rate as f64, top, &edges_look),
        };
        c.set_params(CueParams::default());
        c
    }

    fn set_params(&mut self, p: CueParams) {
        if p.pan_law != self.law.law {
            self.law = LawTable::new(p.pan_law);
        }
        self.alpha_a = 1.0 - (-self.hop_s / (p.tau_s_ms * 1e-3)).exp();
        let (f1, f2) = (p.f1_hz, p.f2_hz);
        let centres = &self.centres;
        let bands = centres.len();
        self.map = (0..=self.top)
            .map(|k| {
                let hz = k as f64 * self.bin_hz;
                let a = centres.partition_point(|&c| c <= hz);
                let (a, frac) = if a == 0 {
                    (0, 0.0)
                } else if a >= bands {
                    (bands - 2, 1.0)
                } else {
                    (a - 1, (hz - centres[a - 1]) / (centres[a] - centres[a - 1]))
                };
                let phase_hz = if p.env_itd { hz } else { (1.0 - raised((hz - f1) / (f2 - f1))) * hz };
                let edge = self.low_fade(hz) * (1.0 - raised((hz - 0.9 * TOP_HZ) / (0.1 * TOP_HZ)));
                BinMap { a: a as u16, frac: frac as f32, phase_hz, edge }
            })
            .collect();
        self.p = p;
    }

    /// How much of the stage plays at `hz` for what follows it: all of it with
    /// no XTC, where each ear has its own channel. With XTC, nothing below its band.
    fn low_fade(&self, hz: f64) -> f64 {
        if self.xtc {
            xtc::low_edge(hz)
        } else {
            1.0
        }
    }

    /// A band's covariance as it is read: with its neighbours in the bass,
    /// each by its bins, so a loud band speaks louder.
    fn pooled(&self, b: usize) -> Cov {
        let (a, e) = self.pool[b];
        let (mut c, mut bins) = (Cov::default(), 0.0);
        for i in a..e {
            c.add(&self.cov[i], self.bins[i]);
            bins += self.bins[i];
        }
        let u = 1.0 / bins;
        Cov { ll: c.ll * u, rr: c.rr * u, re: c.re * u, im: c.im * u }
    }

    /// `z` = the forward transform of a windowed frame of L + jR, to be made
    /// the transform of the frame to play.
    fn frame(&mut self, z: &mut [C]) {
        let n = z.len();
        let half_j = C::new(0.0, -0.5);
        for k in 1..=self.top {
            let (a, b) = (z[k], z[n - k].conj());
            self.l[k] = (a + b) * 0.5;
            self.r[k] = (a - b) * half_j;
        }
        self.analyse();
        if self.look.on {
            self.look.frame(&self.l, &self.r, n, self.norm, &self.law, self.bin_hz);
        }
        self.gate_feed();
        self.decide();
        for k in 1..=self.top {
            let m = self.map[k];
            let (a, f) = (m.a as usize, m.frac as f64);
            let (b0, b1) = (self.out[a], self.out[a + 1]);
            let delta = (b0.delta + f * (b1.delta - b0.delta)) * m.edge;
            let tau = (b0.tau_s + f * (b1.tau_s - b0.tau_s)) * m.edge;
            let (s, c) = delta.sin_cos();
            let (l, r) = (self.l[k], self.r[k]);
            let (mut lo, mut ro) = (l * c - r * s, l * s + r * c);
            if tau != 0.0 {
                let (ps, pc) = (2.0 * PI * m.phase_hz * tau.abs()).sin_cos();
                let rot = C::new(pc, -ps);
                if tau > 0.0 {
                    ro *= rot;
                } else {
                    lo *= rot;
                }
            }
            let j = C::new(0.0, 1.0);
            z[k] = lo + j * ro;
            z[n - k] = lo.conj() + j * ro.conj();
        }
    }

    /// The bands' covariances, each reset by an onset of its own.
    fn analyse(&mut self) {
        let norm = self.norm;
        for b in 0..self.cov.len() {
            let (mut c, mut bins) = (Cov::default(), 0.0);
            for k in self.edges[b]..self.edges[b + 1] {
                let (l, r) = (self.l[k], self.r[k]);
                let x = l * r.conj();
                c.ll += l.norm_sqr();
                c.rr += r.norm_sqr();
                c.re += x.re;
                c.im += x.im;
                bins += 1.0;
            }
            let u = norm / bins;
            c = Cov { ll: c.ll * u, rr: c.rr * u, re: c.re * u, im: c.im * u };
            let e = c.ll + c.rr;
            let a = if self.p.onset_reset && e > 8.0 * self.energy[b] && e > 1e-12 { 1.0 } else { self.alpha_a };
            self.energy[b] += self.alpha_a * (e - self.energy[b]);
            self.cov[b].go(&c, a);
        }
    }

    /// Angle, targets and what they make of the band.
    fn decide(&mut self) {
        let s = self.p.strength / 100.0;
        let lf = self.p.lf_ild / 100.0;
        let itd = self.p.itd_scale / 100.0;
        let width = self.p.stage_width / 90.0;
        let blend = if self.p.head_shadow { self.p.shadow_blend / 100.0 } else { 0.0 };
        let (f1, f2) = (self.p.f1_hz, self.p.f2_hz);
        for b in 0..self.cov.len() {
            let cov = self.pooled(b);
            let (alpha, sure) = cov.axis();
            let hz = self.centres[b];
            let a = alpha.clamp(1e-3, PI / 2.0 - 1e-3);
            let mut rho = (20.0 * (a.cos() / a.sin()).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
            // Out of phase, or nothing that stands out: this band is not a
            // panned source. A band too quiet to matter is not worth a turn.
            // A channel far under the other has no say in the first of these.
            let lean = smoothstep(LEAN_FROM_DB, LEAN_FULL_DB, rho.abs());
            let quiet = cov.ll + cov.rr < 1e-12;
            let weight = if quiet { 0.0 } else { smoothstep(0.25, 0.6, sure) * (lean + (1.0 - lean) * cov.in_phase()) };
            if weight == 0.0 {
                self.out[b] = BandOut::default();
                continue;
            }
            if let Some(&near) = self.p.snap_db.iter().min_by(|x, y| (rho - **x).abs().total_cmp(&(rho - **y).abs())) {
                if (rho - near).abs() <= 3.0 {
                    rho = near;
                }
            }
            let deg = self.law.deg(rho) * width;
            let low = rho * (1.0 - lf * s);
            let high = if blend > 0.0 {
                let bl = blend * s;
                // A hard pan's 60 dB would outweigh any head: its share in the
                // blend is capped, in step with the blend, so no strength
                // and no blend still mean the pan as it was.
                let capped = rho + (2.0 * bl).min(1.0) * (rho.clamp(-RHO_BLEND_DB, RHO_BLEND_DB) - rho);
                (1.0 - bl) * capped + bl * shadow_db(deg, hz)
            } else {
                rho
            };
            let t = raised((hz.max(1.0).ln() - f1.ln()) / (f2.ln() - f1.ln()));
            let target = (1.0 - t) * low + t * high;
            let to = (10f64.powf(-target / 20.0)).atan();
            self.out[b] = BandOut {
                delta: (to - a) * weight,
                tau_s: head_itd_s(deg, t) * itd * s * weight,
                rho_in: rho,
                rho_out: (1.0 - weight) * rho + weight * target,
            };
        }
    }

    /// The running average of the phase-only cross spectrum, and of side and mid.
    fn gate_feed(&mut self) {
        let frame_e: f64 = (1..=self.top).map(|k| self.l[k].norm_sqr() + self.r[k].norm_sqr()).sum();
        if frame_e * self.norm < 1e-10 {
            return; // Silence says nothing of the recording.
        }
        self.gcc_seen += self.hop_s;
        let a = (self.hop_s / self.gcc_seen).max(self.hop_s / GATE_TAU_S);
        let (mut side, mut mid) = (0.0, 0.0);
        for k in 1..=self.top {
            let (l, r) = (self.l[k], self.r[k]);
            side += (l - r).norm_sqr();
            mid += (l + r).norm_sqr();
        }
        self.side += a * (side - self.side);
        self.mid += a * (mid - self.mid);
        let m = self.gcc.len() - 1;
        for k in 1..=m.min(self.top) {
            let x = self.l[k] * self.r[k].conj();
            let g = x.norm();
            let v = if g > 1e-30 { x / g } else { C::new(0.0, 0.0) };
            let was = self.gcc[k];
            self.gcc[k] = was + (v - was) * a;
        }
    }

    /// The peak of the averaged PHAT at no lag against its peak anywhere else,
    /// out to a little past a head's width. A pan-pot mix has it at no lag;
    /// spaced microphones and a binaural head elsewhere.
    fn confidence(&mut self) -> f64 {
        let n = self.gcc_buf.len();
        let (lo, hi) = (150.0, 4000.0);
        for c in self.gcc_buf.iter_mut() {
            *c = C::new(0.0, 0.0);
        }
        for k in 1..n / 2 {
            let hz = k as f64 * self.bin_hz;
            if hz >= lo && hz <= hi && k < self.gcc.len() {
                self.gcc_buf[k] = self.gcc[k];
                self.gcc_buf[n - k] = self.gcc[k].conj();
            }
        }
        self.gcc_fft.process(&mut self.gcc_buf);
        let lag_s = 1.0 / (n as f64 * self.bin_hz);
        let far = ((0.0016 / lag_s).round() as usize).clamp(3, n / 2 - 1);
        let at = |i: i64| self.gcc_buf[i.rem_euclid(n as i64) as usize].re;
        let p0 = at(0).max(at(1)).max(at(-1));
        let mut off = 0f64;
        for i in 3..=far as i64 {
            off = off.max(at(i)).max(at(-i));
        }
        if p0 <= 0.0 {
            return 0.0;
        }
        ((p0 - 1.2 * off) / (0.6 * p0)).clamp(0.0, 1.0)
    }

    /// The gate's state from what it has heard.
    fn gate(&mut self) {
        if self.gcc_seen < GATE_FIRST_S {
            return;
        }
        let was_mono = self.mono;
        self.mono = self.side < MONO_SIDE * self.mid;
        self.conf = if self.mono { 0.0 } else { self.confidence() };
        let want = if self.mono {
            false
        } else if self.open {
            self.conf > GATE_CLOSE
        } else {
            self.conf >= GATE_OPEN
        };
        // The first look decides, and a mono stretch beginning or ending is
        // plain. Past that a recording does not change its nature in a passage:
        // the gate has to want it for a while.
        let sure = !self.known || was_mono != self.mono;
        self.known = true;
        if want == self.open {
            self.gate_wants = 0.0;
        } else if sure {
            self.open = want;
            self.gate_wants = 0.0;
        } else {
            self.gate_wants += VIEW_FRAMES as f64 * self.hop_s;
            if self.gate_wants >= GATE_HOLD_S {
                self.open = want;
                self.gate_wants = 0.0;
            }
        }
    }

    fn bands(&self) -> Vec<CueBand> {
        self.out
            .iter()
            .zip(&self.centres)
            .map(|(o, &hz)| {
                let f = self.low_fade(hz);
                CueBand { hz, itd_us: o.tau_s * f * 1e6, ild_in_db: o.rho_in, ild_out_db: o.rho_in + f * (o.rho_out - o.rho_in) }
            })
            .collect()
    }
}

// ─────────────────────────── the stage ───────────────────────────

/// The frame's length at this rate: 2048 at 44.1 kHz, within a few percent of
/// the same in time. A multiple of 4 for the hop, of no prime over 5 for the
/// transform's speed; the nearest such length, not the next power of two, which
/// on the 48 kHz family would be 85 ms.
fn frame_len(rate: u32) -> usize {
    let want = ((2048.0 * rate as f64 / 44_100.0).round() as usize).max(256);
    let fits = |n: usize| {
        let mut m = n / 4;
        for p in [2, 3, 5] {
            while m % p == 0 {
                m /= p;
            }
        }
        n % 4 == 0 && m == 1
    };
    (0..want).find_map(|d| [want - d, want + d].into_iter().find(|&n| fits(n))).unwrap_or(want.next_power_of_two())
}

pub struct CueStage {
    up: Box<dyn Stage>,
    total: u64,
    rate: u32,
    xtc: bool,
    n: usize,
    hop: usize,
    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,
    win: Vec<f64>,
    buf: Vec<C>,
    core: CueCore,
    ctl: Arc<CueControl>,
    seen_gen: u64,
    /// The dry input and the overlap-added wet, over absolute indices `base ..`.
    base: i64,
    dry_l: Vec<f64>,
    dry_r: Vec<f64>,
    acc_l: Vec<f64>,
    acc_r: Vec<f64>,
    /// Next input index read, and where the first read started.
    filled: i64,
    /// Start of the next frame: indices below it are final.
    next_frame: i64,
    pos: u64,
    wet: f64,
    frames: u32,
    /// The first read has read ahead to know the gate.
    looked: bool,
    /// This chain's number, for its views.
    id: u64,
}

static NEXT_CHAIN: AtomicU64 = AtomicU64::new(1);

impl CueStage {
    /// Input the stage needs before output index `start`.
    pub fn history(rate: u32, start: u64) -> u64 {
        let n = frame_len(rate);
        start.min((n - n / 4) as u64)
    }

    /// The player's stage, on its one control. `up` must be positioned at
    /// `start − history(rate, start)`.
    pub fn new(up: Box<dyn Stage>, rate: u32, start: u64, xtc: bool) -> CueStage {
        Self::with_control(up, rate, start, xtc, control())
    }

    pub fn with_control(up: Box<dyn Stage>, rate: u32, start: u64, xtc: bool, ctl: Arc<CueControl>) -> CueStage {
        let n = frame_len(rate);
        let hop = n / 4;
        debug_assert_eq!(up.position(), start - Self::history(rate, start));
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        // √Hann both ways: the two together sum to 2 at a quarter hop.
        let win: Vec<f64> = (0..n).map(|i| (PI * i as f64 / n as f64).sin()).collect();
        let total = up.total();
        let at = up.position() as i64;
        let mut stage = CueStage {
            up,
            total,
            rate,
            xtc,
            n,
            hop,
            fft,
            ifft,
            win,
            buf: vec![C::new(0.0, 0.0); n],
            core: CueCore::new(n, rate, hop, xtc),
            ctl,
            seen_gen: u64::MAX,
            base: at,
            dry_l: Vec::new(),
            dry_r: Vec::new(),
            acc_l: Vec::new(),
            acc_r: Vec::new(),
            filled: at,
            next_frame: start as i64 - (n - hop) as i64,
            pos: start,
            wet: 0.0,
            frames: 0,
            looked: false,
            id: NEXT_CHAIN.fetch_add(1, Ordering::Relaxed),
        };
        stage.poll();
        stage
    }

    /// Frames ahead of what plays, until the gate can say: a start or a seek
    /// then plays the stage from its first sample, not dry for a moment and
    /// then opening out. Silence says nothing, so this stops at `LOOK_AHEAD_S`.
    fn look_ahead(&mut self) {
        self.looked = true;
        let end = (self.pos as i64 + (LOOK_AHEAD_S * self.rate as f64) as i64).min(self.total as i64);
        while self.core.gcc_seen < GATE_FIRST_S && self.next_frame < end {
            self.do_frame();
        }
        self.core.gate();
        if self.core.open && !self.core.p.bypass {
            self.wet = 1.0;
        }
    }

    /// The window's latest, at a frame's edge.
    fn poll(&mut self) {
        let g = self.ctl.gen.load(Ordering::Acquire);
        if g != self.seen_gen {
            self.seen_gen = g;
            let p = self.ctl.params.lock().unwrap().clone();
            self.core.set_params(p);
        }
    }

    /// Read input up to absolute index `to`.
    fn fill(&mut self, to: i64) {
        if self.filled >= to {
            return;
        }
        let need = (to - self.filled) as usize;
        let at = (self.filled - self.base) as usize;
        self.dry_l.resize(at + need, 0.0);
        self.dry_r.resize(at + need, 0.0);
        self.up.read(&mut self.dry_l[at..at + need], &mut self.dry_r[at..at + need]);
        self.filled = to;
    }

    fn do_frame(&mut self) {
        self.poll();
        let (n, start) = (self.n, self.next_frame);
        self.fill(start + n as i64);
        for i in 0..n {
            let t = start + i as i64 - self.base;
            let (l, r) = if t >= 0 { (self.dry_l[t as usize], self.dry_r[t as usize]) } else { (0.0, 0.0) };
            let w = self.win[i];
            self.buf[i] = C::new(l * w, r * w);
        }
        self.fft.process(&mut self.buf);
        // The hop's number from the track's start (the frame's middle hop):
        // each chain of a track counts alike, and `Look::frame` adds this one.
        self.core.look.seq = ((start + (n - self.hop) as i64).max(0) as u64) / self.hop as u64;
        self.core.frame(&mut self.buf);
        self.ifft.process(&mut self.buf);
        let need = (start + n as i64 - self.base).max(0) as usize;
        if self.acc_l.len() < need {
            self.acc_l.resize(need, 0.0);
            self.acc_r.resize(need, 0.0);
        }
        let norm = 0.5 / n as f64;
        for i in 0..n {
            let t = start + i as i64 - self.base;
            if t >= 0 {
                let w = self.win[i] * norm;
                self.acc_l[t as usize] += self.buf[i].re * w;
                self.acc_r[t as usize] += self.buf[i].im * w;
            }
        }
        self.next_frame += self.hop as i64;
        self.frames = self.frames.wrapping_add(1);
        if self.frames % VIEW_FRAMES == 0 {
            self.core.gate();
            self.publish(start + (n / 2) as i64);
        }
    }

    /// A view of the stage as read at index `at`, queued for the window to
    /// take when that is heard. Only while the window looks is it read at all.
    fn publish(&mut self, at: i64) {
        let look = &mut self.core.look;
        let at_s = at.max(0) as f64 / self.rate as f64;
        if !self.ctl.watched() {
            look.off();
        } else if !look.on {
            look.on = true;
            // Another chain of this track was read just here: go on from it.
            if let Some(c) = self.ctl.carry.lock().unwrap().as_ref() {
                if c.chain != self.id
                    && c.rate == self.rate
                    && c.total == self.total
                    && (c.at_s - at_s).abs() < CARRY_S
                    && now_ms().saturating_sub(c.ms) < CARRY_MS
                {
                    look.adopt(&c.look);
                }
            }
        } else {
            look.places(VIEW_FRAMES as f64 * self.core.hop_s);
            if (self.frames / VIEW_FRAMES) as u64 % CARRY_VIEWS == 0 {
                let carry = Carry { chain: self.id, rate: self.rate, total: self.total, at_s, ms: now_ms(), look: look.state() };
                if let Ok(mut c) = self.ctl.carry.try_lock() {
                    *c = Some(carry);
                }
            }
        }
        // Never wait on the window: a view it holds is skipped, and the waves
        // go with the next.
        let Ok(mut q) = self.ctl.views.try_lock() else { return };
        let bands = self.core.bands();
        let look = &mut self.core.look;
        let view = CueView {
            running: true,
            rate: self.rate,
            xtc: self.xtc,
            confidence: self.core.conf,
            open: self.core.open,
            mono: self.core.mono,
            wet: self.wet,
            dots: look.dots(),
            bands,
            at_s,
            seq: look.seq,
            slice_hz: self.rate as f64 / self.hop as f64,
            chain: self.id,
        };
        if q.len() >= VIEWS_KEPT {
            q.pop_front();
        }
        q.push_back(view);
    }
}

impl Drop for CueStage {
    fn drop(&mut self) {
        // Its own last view only: the next chain of the track may be on already.
        if let Ok(mut q) = self.ctl.views.lock() {
            if let Some(v) = q.back_mut().filter(|v| v.chain == self.id) {
                v.running = false;
            }
        }
    }
}

impl Stage for CueStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }

    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        if !self.looked {
            self.look_ahead();
        }
        let n = out_l.len();
        let step = 1.0 / (FADE_S * self.rate as f64);
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if self.pos >= self.total {
                out_l[done..].fill(0.0);
                out_r[done..].fill(0.0);
                break;
            }
            let at = self.pos as i64;
            if at >= self.next_frame {
                self.do_frame();
                continue;
            }
            let take = ((self.next_frame - at) as usize).min(n - done).min((self.total - self.pos).min(usize::MAX as u64) as usize);
            let target = if !self.core.p.bypass && self.core.open { 1.0 } else { 0.0 };
            let q = (at - self.base) as usize;
            for i in 0..take {
                if self.wet != target {
                    self.wet = if target > self.wet { (self.wet + step).min(1.0) } else { (self.wet - step).max(0.0) };
                }
                let (d_l, d_r) = (self.dry_l[q + i], self.dry_r[q + i]);
                if self.wet == 0.0 {
                    out_l[done + i] = d_l;
                    out_r[done + i] = d_r;
                } else {
                    out_l[done + i] = d_l + self.wet * (self.acc_l[q + i] - d_l);
                    out_r[done + i] = d_r + self.wet * (self.acc_r[q + i] - d_r);
                }
            }
            done += take;
            inside += take;
            self.pos += take as u64;
            // What nothing reads again: below the next frame and the output.
            let keep = (self.pos as i64).min(self.next_frame);
            if keep - self.base > 1 << 18 {
                let drop = (keep - self.base) as usize;
                self.dry_l.drain(..drop);
                self.dry_r.drain(..drop);
                self.acc_l.drain(..drop.min(self.acc_l.len()));
                self.acc_r.drain(..drop.min(self.acc_r.len()));
                self.base += drop as i64;
            }
        }
        inside
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 176_400;

    /// A track in memory.
    struct Mem {
        l: Vec<f64>,
        r: Vec<f64>,
        pos: u64,
    }

    impl Stage for Mem {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for i in 0..out_l.len() {
                let p = self.pos as usize;
                if p < self.l.len() {
                    out_l[i] = self.l[p];
                    out_r[i] = self.r[p];
                    inside += 1;
                } else {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                self.pos += 1;
            }
            inside
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.l.len() as u64
        }
    }

    /// Noise that repeats from run to run.
    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed.wrapping_mul(2).wrapping_add(1);
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1234_5678_9ABC_DEF1);
                let v = (s >> 11) as f64 / (1u64 << 53) as f64;
                0.25 * (v - 0.5)
            })
            .collect()
    }

    /// A one-pole low-pass, keeping the test's noise off the top octaves'
    /// huge empty range at this rate.
    fn lowpass(x: &[f64], hz: f64) -> Vec<f64> {
        let a = 1.0 - (-2.0 * PI * hz / RATE as f64).exp();
        let mut y = 0.0;
        let mut b = 0.0;
        x.iter()
            .map(|&v| {
                y += a * (v - y);
                b += a * (y - b);
                b
            })
            .collect()
    }

    fn pan(s: &[f64], deg: f64, law: PanLaw) -> (Vec<f64>, Vec<f64>) {
        let rho = law_rho_db(law, deg);
        let a = (10f64.powf(-rho / 20.0)).atan();
        (s.iter().map(|v| v * a.cos()).collect(), s.iter().map(|v| v * a.sin()).collect())
    }

    /// With nothing after it: each ear has its own channel, every band plays.
    fn run(l: Vec<f64>, r: Vec<f64>, p: CueParams, start: u64, chunk: usize) -> (Vec<f64>, Vec<f64>, CueView) {
        run_at(RATE, false, l, r, p, start, chunk)
    }

    fn run_at(rate: u32, xtc: bool, l: Vec<f64>, r: Vec<f64>, p: CueParams, start: u64, chunk: usize) -> (Vec<f64>, Vec<f64>, CueView) {
        run_on(rate, xtc, l, r, p, start, chunk, true)
    }

    /// `watch`: the window looks all along.
    #[allow(clippy::too_many_arguments)]
    fn run_on(rate: u32, xtc: bool, l: Vec<f64>, r: Vec<f64>, p: CueParams, start: u64, chunk: usize, watch: bool) -> (Vec<f64>, Vec<f64>, CueView) {
        let ctl = Arc::new(CueControl::default());
        ctl.set(p);
        if watch {
            ctl.view();
        }
        let n = l.len();
        let up_at = start - CueStage::history(rate, start);
        let mem = Mem { l, r, pos: up_at };
        let mut st = CueStage::with_control(Box::new(mem), rate, start, xtc, ctl.clone());
        let (mut ol, mut or) = (vec![0.0; n - start as usize], vec![0.0; n - start as usize]);
        let mut at = 0;
        while at < ol.len() {
            let e = (at + chunk).min(ol.len());
            let (a, b) = (&mut ol[at..e], &mut or[at..e]);
            st.read(a, b);
            at = e;
            if watch {
                ctl.looked_ms.store(now_ms(), Ordering::Relaxed);
            }
        }
        let view = ctl.view();
        (ol, or, view)
    }

    fn band_of(x: &[C], k0: usize, k1: usize) -> f64 {
        (k0..k1).map(|k| x[k].norm_sqr()).sum()
    }

    /// Cross spectrum of two channels over a long stretch, Welch's way, and
    /// from it a band's level difference, dB, and time difference, s (left
    /// first = positive).
    fn cues(l: &[f64], r: &[f64], lo: f64, hi: f64) -> (f64, f64) {
        let n = 16384;
        let mut p = FftPlanner::<f64>::new();
        let f = p.plan_fft_forward(n);
        let (k0, k1) = ((lo * n as f64 / RATE as f64) as usize, (hi * n as f64 / RATE as f64) as usize);
        let mut sum = vec![C::new(0.0, 0.0); k1];
        let (mut el, mut er) = (0.0, 0.0);
        let mut at = 0;
        while at + n <= l.len() {
            let w = |i: usize| (PI * i as f64 / n as f64).sin().powi(2);
            let mut xl: Vec<C> = (0..n).map(|i| C::new(l[at + i] * w(i), 0.0)).collect();
            let mut xr: Vec<C> = (0..n).map(|i| C::new(r[at + i] * w(i), 0.0)).collect();
            f.process(&mut xl);
            f.process(&mut xr);
            el += band_of(&xl, k0, k1);
            er += band_of(&xr, k0, k1);
            for k in k0..k1 {
                sum[k] += xr[k] * xl[k].conj();
            }
            at += n / 2;
        }
        // The phase slope, on neighbouring bins: −2π f τ.
        let mut turn = C::new(0.0, 0.0);
        for k in k0 + 1..k1 {
            turn += sum[k] * sum[k - 1].conj();
        }
        let tau = -turn.arg() / (2.0 * PI * RATE as f64 / n as f64);
        (10.0 * (el / er).log10(), tau)
    }

    fn power(x: &[f64]) -> f64 {
        x.iter().map(|v| v * v).sum()
    }

    /// The transform and its inverse with nothing changed give the input back.
    #[test]
    fn strength_zero_gives_the_input_back_to_the_noise_floor_of_f64() {
        let s = lowpass(&noise(RATE as usize, 1), 8000.0);
        let (l, r) = pan(&s, 30.0, PanLaw::Sin3);
        let p = CueParams { strength: 0.0, head_shadow: false, ..Default::default() };
        let (ol, or, v) = run(l.clone(), r.clone(), p, 0, 4096);
        assert!(v.open, "a panned source opens the gate ({:?})", v.confidence);
        let e: f64 = ol.iter().zip(&l).chain(or.iter().zip(&r)).map(|(a, b)| (a - b).powi(2)).sum();
        let db = 10.0 * (e / (power(&l) + power(&r))).log10();
        assert!(db < -180.0, "the round trip is {:.1} dB", db);
    }

    #[test]
    fn bypass_is_the_input_bit_for_bit() {
        let s = lowpass(&noise(RATE as usize / 2, 2), 8000.0);
        let (l, r) = pan(&s, 45.0, PanLaw::Sin3);
        let p = CueParams { bypass: true, ..Default::default() };
        let (ol, or, _) = run(l.clone(), r.clone(), p, 0, 1000);
        assert_eq!(ol, l);
        assert_eq!(or, r);
    }

    #[test]
    fn a_later_start_is_the_input_from_that_sample() {
        let s = lowpass(&noise(RATE as usize / 2, 9), 8000.0);
        let (l, r) = pan(&s, 45.0, PanLaw::Sin3);
        let p = CueParams { bypass: true, ..Default::default() };
        let start = 50_000u64;
        let (ol, or, _) = run(l.clone(), r.clone(), p, start, 777);
        assert_eq!(&ol[..], &l[start as usize..]);
        assert_eq!(&or[..], &r[start as usize..]);
    }

    /// Mono: the gate stays shut, and what plays is the input.
    #[test]
    fn mono_passes_bit_for_bit() {
        let s = lowpass(&noise(RATE as usize, 3), 8000.0);
        let (ol, or, v) = run(s.clone(), s.clone(), CueParams::default(), 0, 4096);
        assert!(v.mono && !v.open, "{:?}", v.confidence);
        assert_eq!(ol, s);
        assert_eq!(or, s);
    }

    /// Spaced microphones, a binaural head: the time differences are there
    /// already, the gate stays shut, the input plays.
    #[test]
    fn a_recording_with_real_time_differences_passes_bit_for_bit() {
        let n = RATE as usize * 2;
        for (name, delays) in [("one source, 0.5 ms", vec![0.0005]), ("three sources", vec![0.0006, -0.0004, 0.0009])] {
            let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
            for (i, d) in delays.iter().enumerate() {
                let src = lowpass(&noise(n, 10 + i as u64), 8000.0);
                let lag = (d * RATE as f64) as i64;
                for t in 0..n as i64 {
                    let q = t - lag.max(0);
                    let w = t + lag.min(0);
                    if q >= 0 {
                        l[t as usize] += src[q as usize];
                    }
                    if w >= 0 {
                        r[t as usize] += src[w as usize];
                    }
                }
            }
            let (ol, or, v) = run(l.clone(), r.clone(), CueParams::default(), 0, 4096);
            assert!(!v.open, "{}: conf {:.2}", name, v.confidence);
            assert_eq!(ol, l, "{}", name);
            assert_eq!(or, r, "{}", name);
        }
    }

    /// The stage's cues against the formulas, for a known pan.
    #[test]
    fn a_panned_source_gets_the_time_and_level_of_its_angle() {
        let s = noise(RATE as usize * 3, 4);
        println!("deg    ITD µs (want)      LF ILD dB   HF ILD dB (want)");
        for deg in [15.0, 30.0, 45.0, 60.0, -30.0] {
            let (l, r) = pan(&s, deg, PanLaw::Sin3);
            let (ol, or, v) = run(l, r, CueParams::default(), 0, 4096);
            assert!(v.open, "{}°: conf {:.2}", deg, v.confidence);
            let skip = RATE as usize / 2;
            let (ild_low, itd) = cues(&ol[skip..], &or[skip..], 150.0, 600.0);
            let (ild_high, _) = cues(&ol[skip..], &or[skip..], 5000.0, 7000.0);
            let want_itd = kuhn_s(deg);
            let rho = law_rho_db(PanLaw::Sin3, deg);
            let want_high = 0.5 * rho + 0.5 * shadow_db(deg, 6000.0);
            println!(
                "{:>4}  {:>7.1} ({:>7.1})  {:>7.2}    {:>6.2} ({:>6.2})",
                deg,
                itd * 1e6,
                want_itd * 1e6,
                ild_low,
                ild_high,
                want_high
            );
            assert!((itd - want_itd).abs() < 0.08 * want_itd.abs() + 12e-6, "{}°: ITD {:.1} µs, want {:.1}", deg, itd * 1e6, want_itd * 1e6);
            assert!(ild_low.abs() < 1.0, "{}°: LF ILD {:.2} dB", deg, ild_low);
            assert!((ild_high - want_high).abs() < 1.5, "{}°: HF ILD {:.2} dB, want {:.2}", deg, ild_high, want_high);
        }
    }

    /// A rotation and a phase keep the power: on and bypassed play at one level.
    #[test]
    fn the_stage_keeps_the_level() {
        let s = lowpass(&noise(RATE as usize * 2, 5), 8000.0);
        for deg in [0.0, 30.0, 70.0, 90.0] {
            let (l, r) = pan(&s, deg, PanLaw::Sin3);
            let (ol, or, _) = run(l.clone(), r.clone(), CueParams::default(), 0, 4096);
            let skip = RATE as usize / 2;
            let db = 10.0 * ((power(&ol[skip..]) + power(&or[skip..])) / (power(&l[skip..]) + power(&r[skip..]))).log10();
            assert!(db.abs() < 0.05, "{}°: {:+.3} dB", deg, db);
        }
    }

    /// A hard-left bass reaches the right ear: no gain could do that.
    #[test]
    fn a_hard_panned_bass_is_brought_to_both_ears() {
        let s = lowpass(&noise(RATE as usize * 2, 6), 150.0);
        let r0 = vec![0.0; s.len()];
        let (ol, or, v) = run(s.clone(), r0, CueParams::default(), 0, 4096);
        assert!(v.open, "conf {:.2}", v.confidence);
        let skip = RATE as usize / 2;
        let (ild, itd) = cues(&ol[skip..], &or[skip..], 60.0, 250.0);
        assert!(ild.abs() < 1.5, "ILD {:.2} dB", ild);
        assert!((itd - kuhn_s(90.0)).abs() < 40e-6, "ITD {:.1} µs", itd * 1e6);
    }

    /// Stage width moves the angle itself: a hard pan at 30° gets the time and
    /// the shadow of 30°, not of 90° scaled down.
    #[test]
    fn stage_width_gives_a_hard_pan_the_cues_of_its_angle() {
        let s = noise(RATE as usize * 3, 7);
        let r0 = vec![0.0; s.len()];
        let p = CueParams { stage_width: 30.0, shadow_blend: 100.0, ..Default::default() };
        let (ol, or, v) = run(s, r0, p, 0, 4096);
        assert!(v.open, "conf {:.2}", v.confidence);
        let skip = RATE as usize / 2;
        let (_, itd) = cues(&ol[skip..], &or[skip..], 150.0, 600.0);
        let (ild_high, _) = cues(&ol[skip..], &or[skip..], 5000.0, 7000.0);
        println!("ITD {:.1} µs, HF ILD {:.2} dB", itd * 1e6, ild_high);
        assert!((woodworth_s(30.0) - 261e-6).abs() < 2e-6);
        assert!((itd - kuhn_s(30.0)).abs() < 30e-6, "ITD {:.1} µs", itd * 1e6);
        let want = shadow_db(30.0, 6000.0);
        assert!((ild_high - want).abs() < 1.5, "HF ILD {:.2} dB, want {:.2}", ild_high, want);
    }

    /// Above f2 a true delay: the envelope moves by the ray's time difference.
    /// Off, nothing there is delayed at all.
    #[test]
    fn above_f2_the_delay_is_a_delay() {
        let s = noise(RATE as usize * 3, 8);
        let (l, r) = pan(&s, 45.0, PanLaw::Sin3);
        let (ol, or, _) = run(l, r, CueParams::default(), 0, 4096);
        let skip = RATE as usize / 2;
        let (_, itd) = cues(&ol[skip..], &or[skip..], 5000.0, 7000.0);
        assert!((itd - woodworth_s(45.0)).abs() < 15e-6, "ITD {:.1} µs, want {:.1}", itd * 1e6, woodworth_s(45.0) * 1e6);
        let mut core = CueCore::new(16384, RATE, 4096, false);
        for k in [500usize, 1000, 1500] {
            assert_eq!(core.map[k].phase_hz, k as f64 * core.bin_hz);
        }
        core.set_params(CueParams { env_itd: false, ..Default::default() });
        assert_eq!(core.map[1000].phase_hz, 0.0);
    }

    /// In front of XTC the bass plays as it came: each ear hears both speakers.
    #[test]
    fn in_front_of_xtc_nothing_happens_below_its_band() {
        let s = lowpass(&noise(RATE as usize * 2, 6), 150.0);
        let r0 = vec![0.0; s.len()];
        let (ol, or, v) = run_at(RATE, true, s.clone(), r0, CueParams::default(), 0, 4096);
        assert!(v.open, "conf {:.2}", v.confidence);
        let skip = RATE as usize / 2;
        let (ild, _) = cues(&ol[skip..], &or[skip..], 40.0, 120.0);
        assert!(ild > 40.0, "ILD {:.1} dB", ild);
        let core = CueCore::new(8192, RATE, 2048, true);
        assert_eq!(core.low_fade(xtc::F_LO), 0.0);
        assert_eq!(core.low_fade(xtc::F_LO_FULL), 1.0);
        assert_eq!(CueCore::new(8192, RATE, 2048, false).low_fade(100.0), 1.0);
    }

    /// One source with a phase between its channels stands where its levels
    /// say; at a right angle it is not a pan at all. No cross term: a hard pan.
    #[test]
    fn a_phase_between_the_channels_does_not_move_a_source() {
        let (ll, rr) = (1.0, 0.25);
        let cov = |deg: f64| Cov { ll, rr, re: 0.5 * deg.to_radians().cos(), im: 0.5 * deg.to_radians().sin() };
        for deg in [0.0, 60.0] {
            let (alpha, sure) = cov(deg).axis();
            let rho = 20.0 * (alpha.cos() / alpha.sin()).log10();
            assert!((rho - 6.02).abs() < 0.01, "{} deg: {:.2} dB", deg, rho);
            assert!((sure - 1.0).abs() < 1e-9);
            assert!((cov(deg).in_phase() - 1.0).abs() < 1e-9);
        }
        assert!(cov(90.0).in_phase() < 1e-9);
        assert_eq!(cov(180.0).in_phase(), 0.0);
        assert_eq!(Cov { ll: 1.0, rr: 0.0, re: 0.0, im: 0.0 }.in_phase(), 1.0);
    }

    /// One note's harmonics are read together in the bass; high up each band
    /// is its own.
    #[test]
    fn a_band_in_the_bass_is_read_with_its_neighbours() {
        let core = CueCore::new(8192, RATE, 2048, false);
        let (a, e) = core.pool[core.centres.partition_point(|&c| c < 300.0)];
        assert!(e - a >= 3, "{} bands", e - a);
        let top = core.centres.len() - 1;
        assert_eq!(core.pool[top], (top, top + 1));
    }

    /// One length in time at every rate, and the speed of a smooth transform.
    #[test]
    fn a_frame_is_about_46_ms_at_every_rate() {
        assert_eq!(frame_len(176_400), 8192);
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000] {
            let n = frame_len(rate);
            let ms = n as f64 / rate as f64 * 1e3;
            assert!(n % 4 == 0 && (ms - 46.44).abs() < 0.04 * 46.44, "{} Hz: {} = {:.1} ms", rate, n, ms);
        }
    }

    /// The 48 kHz family's gate sees as finely as the 44.1 kHz family's.
    #[test]
    fn at_48_khz_a_delay_a_head_could_give_keeps_the_gate_shut() {
        let rate = 48_000u32;
        let s = lowpass(&noise(rate as usize * 3, 16), 8000.0 * RATE as f64 / rate as f64);
        for lag in [10usize, 14] {
            let r: Vec<f64> = (0..s.len()).map(|i| if i >= lag { s[i - lag] } else { 0.0 }).collect();
            let (ol, _, v) = run_at(rate, true, s.clone(), r, CueParams::default(), 0, 4096);
            assert!(!v.open, "{} samples: conf {:.2}", lag, v.confidence);
            assert_eq!(ol, s);
        }
    }

    /// A start plays the stage from its first sample, not dry for a moment.
    #[test]
    fn a_start_has_the_stage_at_once() {
        let s = lowpass(&noise(RATE as usize * 2, 17), 8000.0);
        let (l, r) = pan(&s, 45.0, PanLaw::Sin3);
        let start = RATE as u64 / 2;
        let (_, or, v) = run(l, r.clone(), CueParams::default(), start, 1024);
        assert!(v.open, "conf {:.2}", v.confidence);
        let early = RATE as usize / 100;
        let (a, b) = (&or[..early], &r[start as usize..start as usize + early]);
        let diff: f64 = a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum();
        assert!(diff > 0.05 * power(b), "the first 10 ms differ by {:.4} of their power", diff / power(b));
    }

    /// Past the first look the gate wants a change for a while before it
    /// makes it: a passage does not change what a recording is.
    #[test]
    fn the_gate_holds_through_a_passage() {
        let mut core = CueCore::new(8192, RATE, 2048, false);
        core.gcc_seen = 1.0;
        core.known = true;
        core.open = true;
        core.side = 1.0;
        core.mid = 1.0;
        let looks = (GATE_HOLD_S / (VIEW_FRAMES as f64 * core.hop_s)).ceil() as usize;
        for _ in 0..looks - 1 {
            core.gate();
            assert!(core.open);
        }
        core.gate();
        assert!(!core.open);
    }

    /// A hard pan's level difference does not outweigh the head high up.
    #[test]
    fn a_hard_pan_high_up_is_a_head_not_60_db() {
        let s = noise(RATE as usize * 2, 18);
        let r0 = vec![0.0; s.len()];
        let (ol, or, _) = run(s, r0, CueParams::default(), 0, 4096);
        let skip = RATE as usize / 2;
        let (ild, _) = cues(&ol[skip..], &or[skip..], 5000.0, 7000.0);
        let want = 0.5 * RHO_BLEND_DB + 0.5 * shadow_db(90.0, 6000.0);
        assert!((ild - want).abs() < 1.5, "ILD {:.2} dB, want {:.2}", ild, want);
    }

    /// A parameter sent while playing is in within a frame, with no jump.
    #[test]
    fn parameters_change_live_without_a_jump() {
        let s = lowpass(&noise(RATE as usize * 2, 7), 6000.0);
        let (l, r) = pan(&s, 45.0, PanLaw::Sin3);
        let ctl = Arc::new(CueControl::default());
        ctl.set(CueParams::default());
        let mem = Mem { l: l.clone(), r: r.clone(), pos: 0 };
        let mut st = CueStage::with_control(Box::new(mem), RATE, 0, true, ctl.clone());
        let mut ol = vec![0.0; l.len()];
        let mut or = vec![0.0; l.len()];
        let half = RATE as usize;
        st.read(&mut ol[..half], &mut or[..half]);
        ctl.set(CueParams { bypass: true, ..Default::default() });
        st.read(&mut ol[half..], &mut or[half..]);
        // After a frame and the crossfade, the dry samples exactly.
        let settled = half + 8192 * 2 + (FADE_S * RATE as f64) as usize + 2;
        assert_eq!(&ol[settled..], &l[settled..]);
        assert_eq!(&or[settled..], &r[settled..]);
        let jump = (settled - 20000..settled).map(|i| (ol[i] - ol[i - 1]).abs()).fold(0.0, f64::max);
        let usual = (1000..half).map(|i| (ol[i] - ol[i - 1]).abs()).fold(0.0, f64::max);
        assert!(jump < 1.5 * usual, "jump {:.5} against {:.5}", jump, usual);
    }

    #[test]
    fn the_head_is_the_sphere_of_the_textbooks() {
        // No shadow at the bottom, on-axis pressure doubling at the top.
        assert!((sphere_pressure(0.01, 0.0) - 1.0).abs() < 0.02);
        assert!((sphere_pressure(30.0, 0.0) - 2.0).abs() < 0.2, "{}", sphere_pressure(30.0, 0.0));
        // Nothing in the middle, and the shadow grows with frequency.
        assert!(shadow_exact_db(0.0, 3000.0).abs() < 1e-9);
        let (a, b) = (shadow_exact_db(90.0, 500.0), shadow_exact_db(90.0, 4000.0));
        assert!(a > 0.5 && a < 8.0 && b > 8.0 && b < 25.0, "{} {}", a, b);
        // The table agrees with the series.
        assert!((shadow_db(30.0, 3000.0) - shadow_exact_db(30.0, 3000.0)).abs() < 0.6);
        // Past its bright spot a source keeps its shadow.
        for hz in [1000.0, 4000.0, 8000.0] {
            for deg in (5..=90).step_by(5) {
                assert!(shadow_db(deg as f64, hz) >= shadow_db(deg as f64 - 5.0, hz), "{} Hz, {} deg", hz, deg);
            }
        }
        assert_eq!(shadow_db(-60.0, 3000.0), -shadow_db(60.0, 3000.0));
    }

    #[test]
    fn woodworth_and_the_laws() {
        assert!((woodworth_s(90.0) - 656e-6).abs() < 2e-6);
        assert_eq!(woodworth_s(-30.0), -woodworth_s(30.0));
        assert!((kuhn_s(90.0) - 765e-6).abs() < 2e-6);
        assert_eq!(kuhn_s(-30.0), -kuhn_s(30.0));
        assert_eq!(head_itd_s(30.0, 1.0), woodworth_s(30.0));
        for law in [PanLaw::Sin3, PanLaw::Mid45, PanLaw::Lin6] {
            let t = LawTable::new(law);
            for deg in [-80.0, -45.0, 0.0, 12.0, 60.0] {
                assert!((t.deg(law_rho_db(law, deg)) - deg).abs() < 0.05, "{:?} {}", law, deg);
            }
        }
    }

    /// A calibration's place holds the bands near it.
    #[test]
    fn the_stage_finds_the_places_and_a_calibration_holds_them() {
        let n = RATE as usize * 2;
        let (a, b) = (lowpass(&noise(n, 20), 16000.0), lowpass(&noise(n, 21), 16000.0));
        let (la, ra) = pan(&a, 40.0, PanLaw::Sin3);
        let (lb, rb) = pan(&b, 40.0, PanLaw::Sin3);
        let l: Vec<f64> = la.iter().zip(&lb).map(|(x, y)| x + y).collect();
        let r: Vec<f64> = ra.iter().zip(&rb).map(|(x, y)| x + y).collect();
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        let want = law_rho_db(PanLaw::Sin3, 40.0);
        let top = v.dots.first().expect("a place");
        assert!((top.rho_db - want).abs() < 1.5, "{} against {}", top.rho_db, want);
        assert!((top.deg - 40.0).abs() < 5.0, "{}", top.deg);
    }

    /// The level of a place's image at the band nearest `hz`.
    fn image_at(v: &CueView, dot: &CueDot, hz: f64) -> f64 {
        let b = (0..v.bands.len()).min_by(|&x, &y| (v.bands[x].hz - hz).abs().total_cmp(&(v.bands[y].hz - hz).abs())).unwrap();
        dot.spec_db[b]
    }

    #[test]
    fn each_place_shows_what_sounds_from_it() {
        let n = RATE as usize * 3;
        let low = lowpass(&noise(n, 40), 3000.0);
        let raw = noise(n, 41);
        let cut = lowpass(&raw, 4000.0);
        let high: Vec<f64> = raw.iter().zip(&cut).map(|(x, y)| 0.3 * (x - y)).collect();
        let (la, ra) = pan(&low, 40.0, PanLaw::Sin3);
        let (lb, rb) = pan(&high, -40.0, PanLaw::Sin3);
        let l: Vec<f64> = la.iter().zip(&lb).map(|(x, y)| x + y).collect();
        let r: Vec<f64> = ra.iter().zip(&rb).map(|(x, y)| x + y).collect();
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        let left = v.dots.iter().max_by(|a, b| a.deg.total_cmp(&b.deg)).unwrap();
        let right = v.dots.iter().min_by(|a, b| a.deg.total_cmp(&b.deg)).unwrap();
        assert!(left.deg > 20.0 && right.deg < -20.0, "{} {}", left.deg, right.deg);
        assert_eq!(left.spec_db.len(), v.bands.len());
        let tilt = |d: &CueDot| image_at(&v, d, 500.0) - image_at(&v, d, 10000.0);
        assert!(tilt(left) > tilt(right) + 20.0, "{} against {}", tilt(left), tilt(right));
    }

    #[test]
    fn reverberation_lowers_what_reads_as_direct() {
        let n = RATE as usize * 3;
        let a = lowpass(&noise(n, 50), 16000.0);
        let (l, r) = pan(&a, 30.0, PanLaw::Sin3);
        let (_, _, dry) = run(l.clone(), r.clone(), CueParams::default(), 0, 4096);
        let (d0, d1) = (lowpass(&noise(n, 51), 16000.0), lowpass(&noise(n, 52), 16000.0));
        let wl: Vec<f64> = l.iter().zip(&d0).map(|(x, y)| x + 0.5 * y).collect();
        let wr: Vec<f64> = r.iter().zip(&d1).map(|(x, y)| x + 0.5 * y).collect();
        let (_, _, wet) = run(wl, wr, CueParams::default(), 0, 4096);
        let (dd, wd) = (dry.dots[0].direct, wet.dots[0].direct);
        assert!(dd > 0.95, "{dd}");
        assert!(dd - wd > 0.1, "{dd} against {wd}");
    }

    #[test]
    fn a_hard_pan_is_a_place() {
        let n = RATE as usize * 2;
        let a = lowpass(&noise(n, 60), 16000.0);
        let (_, _, v) = run(a.clone(), vec![0.0; n], CueParams::default(), 0, 4096);
        assert!(v.dots.first().expect("a place").deg > 75.0, "{:?}", v.dots.first().map(|d| d.deg));
    }

    fn mix(parts: &[(Vec<f64>, Vec<f64>)]) -> (Vec<f64>, Vec<f64>) {
        let n = parts[0].0.len();
        let l = (0..n).map(|i| parts.iter().map(|p| p.0[i]).sum()).collect();
        let r = (0..n).map(|i| parts.iter().map(|p| p.1[i]).sum()).collect();
        (l, r)
    }

    fn tone(n: usize, hz: f64, amp: f64) -> Vec<f64> {
        (0..n).map(|i| amp * (2.0 * PI * hz * i as f64 / RATE as f64).sin()).collect()
    }

    fn has_place(v: &CueView, lo: f64, hi: f64) -> bool {
        v.dots.iter().any(|d| d.deg >= lo && d.deg <= hi)
    }

    fn degs(v: &CueView) -> Vec<f64> {
        v.dots.iter().map(|d| d.deg).collect()
    }

    /// Two sources a few bins apart in one band: two places, not one between them.
    #[test]
    fn two_sources_in_one_band_are_two_places() {
        let n = RATE as usize * 2;
        let a = pan(&tone(n, 3000.0, 0.2), 40.0, PanLaw::Sin3);
        let b = pan(&tone(n, 3150.0, 0.2), -40.0, PanLaw::Sin3);
        let (l, r) = mix(&[a, b]);
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        assert!(has_place(&v, 35.0, 45.0) && has_place(&v, -45.0, -35.0), "{:?}", degs(&v));
        assert!(!has_place(&v, -20.0, 20.0), "{:?}", degs(&v));
    }

    /// A source coming in half a second ago is on the stage already.
    #[test]
    fn a_source_coming_in_shows_at_once() {
        let n = RATE as usize * 5 / 2;
        let centre = pan(&lowpass(&noise(n, 70), 2000.0), 0.0, PanLaw::Sin3);
        let raw = noise(n, 71);
        let cut = lowpass(&raw, 4000.0);
        let late: Vec<f64> = (0..n).map(|i| if i >= RATE as usize * 2 { 0.5 * (raw[i] - cut[i]) } else { 0.0 }).collect();
        let (l, r) = mix(&[centre, pan(&late, 60.0, PanLaw::Sin3)]);
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        assert!(has_place(&v, 50.0, 70.0), "{:?}", degs(&v));
    }

    /// A quiet instrument where it is the one heard shows beside a loud one.
    #[test]
    fn a_quiet_source_shows_beside_a_loud_one() {
        let n = RATE as usize * 2;
        let loud = lowpass(&noise(n, 80), 3000.0);
        let raw = noise(n, 81);
        let cut = lowpass(&raw, 5000.0);
        let quiet: Vec<f64> = raw.iter().zip(&cut).map(|(x, y)| 0.05 * (x - y)).collect();
        let (l, r) = mix(&[pan(&loud, 0.0, PanLaw::Sin3), pan(&quiet, -50.0, PanLaw::Sin3)]);
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        assert!(has_place(&v, -60.0, -40.0), "{:?}", degs(&v));
    }

    /// The bass has places too.
    #[test]
    fn a_bass_instrument_has_a_place() {
        let n = RATE as usize * 2;
        let (l, r) = pan(&lowpass(&noise(n, 90), 300.0), -50.0, PanLaw::Sin3);
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        assert!(has_place(&v, -60.0, -40.0), "{:?}", degs(&v));
    }

    /// The same in both channels, the right one later: it stands on the left,
    /// and says by how much.
    #[test]
    fn a_source_panned_by_time_stands_to_its_side() {
        let n = RATE as usize * 2;
        let s = lowpass(&noise(n, 100), 12000.0);
        let lag = (0.0005 * RATE as f64).round() as usize;
        let r: Vec<f64> = (0..n).map(|i| if i >= lag { s[i - lag] } else { 0.0 }).collect();
        let (_, _, v) = run(s, r, CueParams::default(), 0, 4096);
        let top = v.dots.first().expect("a place");
        assert!(top.deg > 25.0, "{:?}", degs(&v));
        assert!((top.itd_ms - 0.5).abs() < 0.1, "{}", top.itd_ms);
        assert!(top.rho_db.abs() < 1.0, "{}", top.rho_db);
    }

    /// Each place's wave is its own sound: one that stops goes quiet while
    /// the other goes on, at its own level.
    ///
    /// The two take turns, as instruments' notes do, before the right one
    /// goes on alone. Two noises of one spectrum playing steadily together
    /// would be no test: their covariance in every bin is that of one source
    /// in the middle and some diffuse sound, and nothing could tell the two.
    #[test]
    fn a_place_shows_its_own_wave() {
        let n = RATE as usize * 3;
        let stop = n - RATE as usize / 2;
        let turn = |i: usize| (2.0 * PI * i as f64 / RATE as f64).sin().powi(2);
        let a: Vec<f64> = lowpass(&noise(n, 110), 12000.0).iter().enumerate().map(|(i, v)| if i < stop { v * turn(i) } else { 0.0 }).collect();
        let b: Vec<f64> =
            lowpass(&noise(n, 111), 12000.0).iter().enumerate().map(|(i, v)| if i + RATE as usize / 10 < stop { v * (1.0 - turn(i)) } else { *v }).collect();
        let (l, r) = mix(&[pan(&a, 60.0, PanLaw::Sin3), pan(&b, -60.0, PanLaw::Sin3)]);
        let (_, _, v) = run(l, r, CueParams::default(), 0, 4096);
        let left = v.dots.iter().find(|d| d.deg > 40.0).expect("the left place");
        let right = v.dots.iter().find(|d| d.deg < -40.0).expect("the right place");
        let hops = (WAVE_S * v.slice_hz) as usize;
        assert!(right.wave.len() >= 2 * hops - 2 * VIEW_FRAMES as usize, "{}", right.wave.len());
        let peak = |w: &[f32]| w.iter().fold(0f32, |m, x| m.max(x.abs()));
        let (lw, rw) = (&left.wave, &right.wave);
        // The last 0.3 s: the left silent, the right as it is.
        let tail = 2 * (0.3 * v.slice_hz) as usize;
        let before = &lw[..lw.len() - tail - 20];
        assert!(peak(&lw[lw.len() - tail..]) < 0.1 * peak(before), "{} {}", peak(&lw[lw.len() - tail..]), peak(before));
        let truth = b[n - RATE as usize / 4..].iter().fold(0f64, |m, x| m.max(x.abs())) as f32;
        let got = peak(&rw[rw.len() - tail / 2..]);
        assert!((got / truth - 1.0).abs() < 0.35, "{got} against {truth}");
    }

    /// The window's reading changes nothing heard.
    #[test]
    fn the_window_looking_changes_nothing_heard() {
        let n = RATE as usize;
        let (l, r) = mix(&[
            pan(&lowpass(&noise(n, 120), 16000.0), 50.0, PanLaw::Sin3),
            pan(&lowpass(&noise(n, 121), 4000.0), -20.0, PanLaw::Sin3),
        ]);
        let (al, ar, _) = run_on(RATE, false, l.clone(), r.clone(), CueParams::default(), 0, 4096, true);
        let (bl, br, bv) = run_on(RATE, false, l, r, CueParams::default(), 0, 4096, false);
        assert!(al == bl && ar == br);
        assert!(bv.dots.is_empty());
    }

    /// The player moves a track from its quick first chain to the full one a
    /// few seconds in: the next chain goes on with the same places and the
    /// same hop numbers, and the first one's end does not stop the window.
    #[test]
    fn the_next_chain_of_a_track_keeps_its_places() {
        let n = RATE as usize * 6;
        let raw = noise(n, 131);
        let cut = lowpass(&raw, 4000.0);
        let high: Vec<f64> = raw.iter().zip(&cut).map(|(x, y)| 0.5 * (x - y)).collect();
        let (l, r) = mix(&[pan(&lowpass(&noise(n, 130), 1500.0), 50.0, PanLaw::Sin3), pan(&high, -40.0, PanLaw::Sin3)]);
        let ctl = Arc::new(CueControl::default());
        ctl.set(CueParams::default());
        ctl.view();
        let chain = |start: u64, secs: f64| {
            let up_at = start - CueStage::history(RATE, start);
            let mut st = CueStage::with_control(Box::new(Mem { l: l.clone(), r: r.clone(), pos: up_at }), RATE, start, false, ctl.clone());
            let len = (secs * RATE as f64) as usize;
            let (mut a, mut b) = (vec![0.0; 4096], vec![0.0; 4096]);
            let mut at = 0;
            while at < len {
                st.read(&mut a, &mut b);
                at += 4096;
                ctl.looked_ms.store(now_ms(), Ordering::Relaxed);
            }
            st
        };
        let first = chain(0, 4.0);
        let a = ctl.view();
        let ids: Vec<u32> = a.dots.iter().map(|d| d.id).collect();
        assert!(ids.len() >= 2, "{:?}", degs(&a));
        let second = chain(RATE as u64 * 3, 0.3);
        drop(first);
        let q = ctl.views.lock().unwrap();
        let b = q.iter().rev().find(|v| v.chain != a.chain && !v.dots.is_empty()).expect("a view of the next chain").clone();
        drop(q);
        assert!(b.dots.iter().all(|d| ids.contains(&d.id)), "{ids:?} then {:?}", b.dots.iter().map(|d| d.id).collect::<Vec<_>>());
        // The same hop number for the same place in the track.
        let hop_s = 1.0 / b.slice_hz;
        assert!((b.seq as f64 * hop_s - b.at_s).abs() < 0.1, "{} {}", b.seq as f64 * hop_s, b.at_s);
        assert!((a.seq as f64 * hop_s - a.at_s).abs() < 0.1, "{} {}", a.seq as f64 * hop_s, a.at_s);
        assert!(ctl.view().running);
        drop(second);
    }

    /// The window gets the view read where the track is heard, and each place
    /// its wave over the views before it.
    #[test]
    fn the_view_heard_is_the_one_shown() {
        let ctl = CueControl::default();
        {
            let mut q = ctl.views.lock().unwrap();
            for i in 0..40u64 {
                let dot = CueDot { id: 7, wave: vec![-(i as f32), i as f32], ..Default::default() };
                q.push_back(CueView { running: true, at_s: i as f64 * 0.05, seq: i, dots: vec![dot], ..Default::default() });
            }
        }
        let v = ctl.view_heard(Some(1.0));
        assert_eq!(v.seq, 20);
        let w = &v.dots[0].wave;
        assert_eq!(*w.last().unwrap(), 20.0);
        assert_eq!(w.len(), 2 * 17, "{w:?}");
        // Nothing heard there: a chain the ear has not reached, the newest.
        assert_eq!(ctl.view_heard(Some(30.0)).seq, 39);
        assert_eq!(ctl.view_heard(None).seq, 39);
    }
}

#[cfg(test)]
#[path = "cue_stand.rs"]
mod stand;
