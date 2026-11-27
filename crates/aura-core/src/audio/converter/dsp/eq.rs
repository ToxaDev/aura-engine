//! Aura EQ: the listener's parametric equalizer.
//!
//! An `EqState` (the window's JSON) becomes an `EqDesign` — a cascade of f64
//! biquads (matched to the RBJ cookbook's analog prototypes, so a treble
//! band keeps its shape near Nyquist; cuts are bilinear Butterworth
//! cascades) and the gain, the balance and the speaker delay after
//! it — for one sample rate. `EqRun` holds a cascade's memory per channel.
//! The same design and the same run are what the player's output thread
//! and the converter's tail apply, at the output rate, after the true-peak
//! gain and before the dither: a file converted with an EQ sounds as the
//! player played it.
//!
//! The math is the window's (`tests/fixtures/eq-golden.gen.mjs` holds the
//! reference and writes the numbers both sides are tested against).
//!
//! Off, or on with nothing that changes the sound (bands bypassed or at
//! 0 dB, no output gain, balance or delay), the EQ is not active and nobody runs
//! it: the samples are left as they were, bit for bit.

use serde::{Deserialize, Serialize};

/// The stage's version in the render stamp (token `EQ`): 2 = analog-matched
/// sections (1 was the bilinear RBJ cookbook).
pub const STAGE_VERSION: u32 = 2;
/// Bands the EQ takes; the rest of a longer list is left out.
pub const MAX_BANDS: usize = 24;
/// A cut's slopes, dB/octave (order = slope / 6).
pub const SLOPES: [u32; 8] = [6, 12, 18, 24, 36, 48, 72, 96];
/// Sections a design holds at most: every band a 96 dB/oct cut.
pub const MAX_SECTIONS: usize = MAX_BANDS * 8;
/// A gain band closer to 0 dB than this changes nothing.
const FLAT_DB: f64 = 0.005;
/// The level grid (safe and auto): log-spaced, 20 Hz to 20 kHz inclusive.
const GRID_POINTS: usize = 480;
/// What the safe level keeps below the peak boost, dB: a loud master's
/// sample peaks grow a little more than the curve's peak (measured: up to
/// +0.2 dB with hard-clipped pink noise through 6–12 dB boosts).
pub const SAFE_MARGIN_DB: f64 = 0.5;
/// The output gain's range, dB.
const OUTPUT_DB: f64 = 30.0;
/// The speaker delay's range, ms (either channel): 30 ms is 10.3 m of a
/// speaker's distance.
pub const DELAY_MS: f64 = 30.0;
/// Memory this small is flushed to zero before it turns denormal.
const TINY: f64 = 1e-250;

/// The EQ as the window keeps it (SPEC "EqState"). Every field has a
/// default, so a partial or older object still reads.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct EqState {
    #[serde(default = "version_1")]
    pub v: u32,
    pub on: bool,
    pub name: String,
    pub bands: Vec<EqBand>,
    /// How the level after the cascade is set: "manual", "auto" or "safe"
    /// (`GainMode`); empty or anything else — a version-1 state — see
    /// `level`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gain: String,
    /// Version 1's preamp (None = auto, a number = manual dB): what `level`
    /// migrates; kept as read, so an old state goes back out unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preamp: Option<f64>,
    /// Output gain, dB.
    pub output: f64,
    /// Balance, dB: how much quieter the other side is (+ toward the right,
    /// or the side in mid/side; − toward the left, or the mid). 0 = centre.
    #[serde(skip_serializing_if = "is_zero")]
    pub balance: f64,
    /// What the balance weighs: "lr" (left/right, the default) or "ms"
    /// (mid/side: the width).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub balance_mode: String,
    /// Speaker delay, ms: how late one channel plays, for a speaker nearer
    /// the listener than the other (+ the right is late, − the left). 0 =
    /// none.
    #[serde(skip_serializing_if = "is_zero")]
    pub delay: f64,
    /// The balance and the delay play with the EQ off too (the listener's
    /// switch, off by default): off, only the bands and the gain stop.
    #[serde(skip_serializing_if = "is_false")]
    pub speakers_always: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// What the balance weighs (`EqState::balance_mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BalanceMode {
    LeftRight,
    MidSide,
}

/// A state without `v` is of version 1.
fn version_1() -> u32 {
    1
}

impl Default for EqState {
    fn default() -> Self {
        EqState {
            v: 2,
            on: false,
            name: "Flat".into(),
            bands: Vec::new(),
            gain: "manual".into(),
            preamp: None,
            output: 0.0,
            balance: 0.0,
            balance_mode: String::new(),
            delay: 0.0,
            speakers_always: false,
        }
    }
}

/// How the level after the EQ is set (`EqState::gain`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainMode {
    /// The output gain alone (the default): overs are the listener's.
    Manual,
    /// As loud as with the EQ off (`auto_gain_db`), plus the output gain.
    Auto,
    /// Minus the peak boost and `SAFE_MARGIN_DB`, plus the output gain.
    Safe,
}

impl GainMode {
    /// The state's name for it (log lines and the render tag).
    pub fn name(self) -> &'static str {
        match self {
            GainMode::Manual => "manual",
            GainMode::Auto => "auto",
            GainMode::Safe => "safe",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct EqBand {
    /// bell | lowShelf | highShelf | tilt | lowCut | highCut | notch |
    /// bandPass; any other is left out.
    #[serde(rename = "type")]
    pub kind: String,
    pub f: f64,
    pub gain: f64,
    pub q: f64,
    /// Cuts only, dB/octave.
    pub slope: f64,
    pub on: bool,
}

impl Default for EqBand {
    fn default() -> Self {
        EqBand { kind: "bell".into(), f: 1000.0, gain: 0.0, q: 0.71, slope: 12.0, on: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bell,
    LowShelf,
    HighShelf,
    Tilt,
    LowCut,
    HighCut,
    Notch,
    BandPass,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "bell" => Kind::Bell,
            "lowShelf" => Kind::LowShelf,
            "highShelf" => Kind::HighShelf,
            "tilt" => Kind::Tilt,
            "lowCut" => Kind::LowCut,
            "highCut" => Kind::HighCut,
            "notch" => Kind::Notch,
            "bandPass" => Kind::BandPass,
            _ => return None,
        })
    }

    fn has_gain(self) -> bool {
        matches!(self, Kind::Bell | Kind::LowShelf | Kind::HighShelf | Kind::Tilt)
    }
}

/// A band as the EQ runs it: its type known, every value in its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    /// Its place in the state's list (a design's sections remember it, so
    /// a moved band keeps its memory).
    pub index: usize,
    pub kind: Kind,
    pub f: f64,
    pub gain: f64,
    pub q: f64,
    pub slope: u32,
}

fn finite_or(v: f64, d: f64) -> f64 {
    if v.is_finite() { v } else { d }
}

impl EqState {
    /// The bands that change the sound, clamped (f 10..24000 Hz, gain ±30 dB,
    /// Q 0.1..24, slope to the nearest of `SLOPES`): of the first
    /// `MAX_BANDS` in the list, those with a known type, on, and — for a
    /// gain type — off 0 dB.
    pub fn active_bands(&self) -> Vec<Band> {
        self.bands
            .iter()
            .take(MAX_BANDS)
            .enumerate()
            .filter_map(|(index, b)| {
                let kind = Kind::parse(&b.kind)?;
                let slope_in = finite_or(b.slope, 12.0);
                let slope = SLOPES
                    .iter()
                    .copied()
                    .fold(SLOPES[0], |best, s| if (s as f64 - slope_in).abs() < (best as f64 - slope_in).abs() { s } else { best });
                let band = Band {
                    index,
                    kind,
                    f: finite_or(b.f, 1000.0).clamp(10.0, 24000.0),
                    gain: finite_or(b.gain, 0.0).clamp(-30.0, 30.0),
                    q: finite_or(b.q, 0.71).clamp(0.1, 24.0),
                    slope,
                };
                let flat = kind.has_gain() && band.gain.abs() < FLAT_DB;
                (b.on && !flat).then_some(band)
            })
            .collect()
    }

    /// The gain mode and the output gain (dB, ±30), as the window's
    /// `levelOf` reads them: a valid `gain` wins; else a version-1 manual
    /// preamp becomes manual with the preamp (clamped to -30..0) added to
    /// the output, and a version-1 auto preamp becomes safe.
    pub fn level(&self) -> (GainMode, f64) {
        let out = finite_or(self.output, 0.0);
        let mode = match self.gain.as_str() {
            "manual" => Some(GainMode::Manual),
            "auto" => Some(GainMode::Auto),
            "safe" => Some(GainMode::Safe),
            _ => None,
        };
        if let Some(m) = mode {
            return (m, out.clamp(-OUTPUT_DB, OUTPUT_DB));
        }
        if let Some(p) = self.preamp.filter(|p| p.is_finite()) {
            return (GainMode::Manual, (out + p.clamp(-30.0, 0.0)).clamp(-OUTPUT_DB, OUTPUT_DB));
        }
        (if self.v >= 2 { GainMode::Manual } else { GainMode::Safe }, out.clamp(-OUTPUT_DB, OUTPUT_DB))
    }

    pub fn output_db(&self) -> f64 {
        self.level().1
    }

    /// The balance's mode and its dB (±30 in 0.01 dB steps, + toward the
    /// right or the side), as the window's `balanceOf` reads them (its
    /// Math.round: halves up): anything but "ms" is left/right.
    pub fn balance(&self) -> (BalanceMode, f64) {
        let mode = if self.balance_mode == "ms" { BalanceMode::MidSide } else { BalanceMode::LeftRight };
        let db = ((finite_or(self.balance, 0.0) * 100.0 + 0.5).floor() / 100.0).clamp(-OUTPUT_DB, OUTPUT_DB);
        (mode, if db == 0.0 { 0.0 } else { db })
    }

    /// The speaker delay, ms (±`DELAY_MS` in 0.01 ms steps, + the right
    /// late), as the window's `delayOf` reads it.
    pub fn delay_ms(&self) -> f64 {
        let ms = ((finite_or(self.delay, 0.0) * 100.0 + 0.5).floor() / 100.0).clamp(-DELAY_MS, DELAY_MS);
        if ms == 0.0 { 0.0 } else { ms }
    }

    /// What plays: the state as it is when on; off with `speakers_always`,
    /// its speakers alone (the balance and the delay, no band, no gain);
    /// else nothing.
    pub fn effective(&self) -> Option<EqState> {
        if self.on {
            return Some(self.clone());
        }
        self.speakers_always.then(|| EqState {
            on: true,
            balance: self.balance,
            balance_mode: self.balance_mode.clone(),
            delay: self.delay,
            ..Default::default()
        })
    }

    /// Whether the EQ changes the sound: what plays (`effective`) has a
    /// band, an output gain, a balance or a delay that does something (with
    /// no band every mode's level is 0). Not active, it is not run at all.
    pub fn active(&self) -> bool {
        self.effective().is_some_and(|s| {
            !s.active_bands().is_empty() || s.output_db() != 0.0 || s.balance().1 != 0.0 || s.delay_ms() != 0.0
        })
    }
}

/// One biquad's coefficients, a0 = 1: b0, b1, b2, a1, a2.
pub type Sos = [f64; 5];

const PI: f64 = std::f64::consts::PI;
/// The highest digital frequency a matched section is fitted at (a band
/// centred above it keeps its analog skirt below Nyquist).
const MATCH_CAP: f64 = 0.9 * PI;
/// A cut's corner is held below this fraction of the rate (bilinear).
const CUT_CAP: f64 = 0.45;

fn sin2(x: f64) -> f64 {
    let s = x.sin();
    s * s
}

/// The analog prototype, s normalised to the centre, as the window's
/// `analog`: [n2, n1, n0, d2, d1, d0] of (n2 s² + n1 s + n0) / (d2 s² + d1 s + d0).
/// A low cut's pair is a high pass, a high cut's a low pass.
fn analog(kind: Kind, g: f64, q: f64) -> [f64; 6] {
    let a = 10f64.powf(g / 40.0);
    let sa = a.sqrt();
    match kind {
        Kind::Bell => [1.0, a / q, 1.0, 1.0, 1.0 / (a * q), 1.0],
        Kind::LowShelf => [a, a * sa / q, a * a, a, sa / q, 1.0],
        Kind::HighShelf => [a * a, a * sa / q, a, 1.0, sa / q, a],
        Kind::Notch => [1.0, 0.0, 1.0, 1.0, 1.0 / q, 1.0],
        Kind::BandPass => [0.0, 1.0 / q, 0.0, 1.0, 1.0 / q, 1.0],
        Kind::HighCut => [0.0, 0.0, 1.0, 1.0, 1.0 / q, 1.0],
        Kind::LowCut => [1.0, 0.0, 0.0, 1.0, 1.0 / q, 1.0],
        Kind::Tilt => unreachable!("tilt is two shelves"),
    }
}

/// |H(jx)|² of a prototype at x = F / f.
fn amag2(p: &[f64; 6], x: f64) -> f64 {
    let x2 = x * x;
    let (nr, ni, dr, di) = (p[2] - p[0] * x2, p[1] * x, p[5] - p[3] * x2, p[4] * x);
    (nr * nr + ni * ni) / (dr * dr + di * di)
}

/// x·d|H(jx)|²/dx: the slope per natural-log frequency.
fn aslope(p: &[f64; 6], x: f64) -> f64 {
    let x2 = x * x;
    let (nr, ni, dr, di) = (p[2] - p[0] * x2, p[1] * x, p[5] - p[3] * x2, p[4] * x);
    let (n, d) = (nr * nr + ni * ni, dr * dr + di * di);
    let xdn = -4.0 * p[0] * x2 * nr + 2.0 * ni * ni;
    let xdd = -4.0 * p[3] * x2 * dr + 2.0 * di * di;
    (xdn * d - n * xdd) / (d * d)
}

/// The poles of a denominator d2 s² + d1 s + d0 at digital centre `w0`, by
/// impulse invariance, as the window's `poles`.
struct Poles {
    a1: f64,
    a2: f64,
    /// The damping distance: how far the nearer pole sits from the circle.
    d: f64,
    /// |A(1)|, |A(-1)|.
    s0: f64,
    s1: f64,
    shape: PoleShape,
}

enum PoleShape {
    /// A complex pair: 1 - r (by expm1), r, the angle.
    Pair { m: f64, r: f64, th: f64 },
    /// Two real poles.
    Real { ma: f64, ra: f64, mb: f64, rb: f64 },
}

impl Poles {
    /// |A(e^jw)|², nothing cancelling at low frequencies.
    fn den(&self, w: f64) -> f64 {
        match self.shape {
            PoleShape::Pair { m, r, th } => {
                (m * m + 4.0 * r * sin2((w - th) / 2.0)) * (m * m + 4.0 * r * sin2((w + th) / 2.0))
            }
            PoleShape::Real { ma, ra, mb, rb } => {
                (ma * ma + 4.0 * ra * sin2(w / 2.0)) * (mb * mb + 4.0 * rb * sin2(w / 2.0))
            }
        }
    }
}

fn poles(d2: f64, d1: f64, d0: f64, w0: f64) -> Poles {
    let wp = w0 * (d0 / d2).sqrt();
    let z = d1 / (2.0 * (d2 * d0).sqrt());
    if z < 1.0 {
        let th = (wp * (1.0 - z * z).sqrt()).min(PI);
        let r = (-z * wp).exp();
        let m = -(-z * wp).exp_m1();
        return Poles {
            a1: -2.0 * r * th.cos(),
            a2: r * r,
            d: z * wp,
            s0: m * m + 4.0 * r * sin2(th / 2.0),
            s1: m * m + 4.0 * r * sin2((PI - th) / 2.0),
            shape: PoleShape::Pair { m, r, th },
        };
    }
    // sa = wp (z - k), written without the cancellation.
    let k = (z * z - 1.0).sqrt();
    let (sa, sb) = (wp / (z + k), wp * (z + k));
    let (ra, rb) = ((-sa).exp(), (-sb).exp());
    let (ma, mb) = (-(-sa).exp_m1(), -(-sb).exp_m1());
    Poles { a1: -(ra + rb), a2: ra * rb, d: sa, s0: ma * mb, s1: (1.0 + ra) * (1.0 + rb), shape: PoleShape::Real { ma, ra, mb, rb } }
}

/// A minimum-phase numerator from |B(e^jw)|² = B0 cos²(w/2) + B1 sin²(w/2) +
/// B2 sin²w, or None when no real numerator has it.
fn numerator(big0: f64, big1: f64, big2: f64) -> Option<[f64; 3]> {
    if !(big0 >= 0.0) || !(big1 >= 0.0) {
        return None;
    }
    let (s0, s1) = (big0.sqrt(), big1.sqrt());
    let w = (s0 + s1) / 2.0;
    let d = w * w + big2;
    if !(d >= 0.0) {
        return None;
    }
    let b0 = (w + d.sqrt()) / 2.0;
    Some([b0, (s0 - s1) / 2.0, if b0 > 0.0 { -big2 / (4.0 * b0) } else { 0.0 }])
}

/// The numerator with its zeros held at radius ≤ `r` and its DC gain kept:
/// an inverted section's poles stay inside the circle.
fn min_phase(b: [f64; 3], r: f64) -> [f64; 3] {
    let [b0, b1, b2] = b;
    let d = b1 * b1 - 4.0 * b0 * b2;
    let c = if d < 0.0 {
        let r2 = b2 / b0;
        if r2 <= r * r {
            return b;
        }
        [b0, b1 * r / r2.sqrt(), r * r * b0]
    } else {
        let cl = |z: f64| z.clamp(-r, r);
        let sd = d.sqrt();
        let z1 = (-b1 + if b1 <= 0.0 { sd } else { -sd }) / (2.0 * b0);
        let z2 = if z1 != 0.0 { b2 / (b0 * z1) } else { 0.0 };
        if z1.abs() <= r && z2.abs() <= r {
            return b;
        }
        [b0, -b0 * (cl(z1) + cl(z2)), b0 * cl(z1) * cl(z2)]
    };
    let s = c[0] + c[1] + c[2];
    let k = if s != 0.0 { (b0 + b1 + b2) / s } else { 1.0 };
    [c[0] * k, c[1] * k, c[2] * k]
}

/// One analog-matched second-order section, as the window's `matched`:
/// [b0, b1, b2, 1, a1, a2] of a bell, shelf, notch or band pass at `f` Hz,
/// gain `g` dB, quality `q`. Poles by impulse invariance, zeros from |B|²
/// fitted to the analog magnitude at DC and its value and slope at a match
/// point (Vicanek, "Matched Second Order Digital Filters", 2016).
fn matched(fs: f64, kind: Kind, f: f64, g: f64, q: f64) -> [f64; 6] {
    let p = analog(kind, g, q);
    // A cut bell, and a shelf whose poles lie above its zeros (low-shelf
    // cut, high-shelf boost), are the exact inverse of their opposite gain:
    // that design's poles map without aliasing and become these zeros. The
    // zeros come no closer to the circle than 1/16 of the distance of the
    // analog zeros mapped by impulse invariance.
    let shelf = matches!(kind, Kind::LowShelf | Kind::HighShelf);
    if (kind == Kind::Bell && g < 0.0) || (shelf && p[5] * p[0] > p[2] * p[3]) {
        let c = matched(fs, kind, f, -g, q);
        let n = analog(kind, -g, q);
        let pz = poles(n[0], n[1], n[2], (2.0 * PI * f / fs).min(PI));
        let b = min_phase([c[0], c[1], c[2]], (-pz.d / 16.0).exp());
        return [1.0, c[4], c[5], b[0], b[1], b[2]];
    }
    let w0 = 2.0 * PI * f / fs;
    let pp = poles(p[3], p[4], p[5], w0);
    if kind == Kind::Notch {
        // Zeros on the circle at the centre, DC gain 1.
        let wn = w0.min(PI);
        let k = pp.s0 / (4.0 * sin2(wn / 2.0));
        return [k, -2.0 * wn.cos() * k, k, 1.0, pp.a1, pp.a2];
    }
    // A gentle shelf is fitted at its centre, a resonant one (q ≥ 2) at its
    // zeros' frequency, where its peak or dip sits; between, the point moves
    // smoothly with log q. A bell's zeros are at its centre.
    let ze = if shelf { 0.5 * q.log2().clamp(0.0, 1.0) } else { 0.0 };
    let wm = (w0 * (p[2] / p[0]).powf(ze)).min(MATCH_CAP);
    let xm = wm / w0;
    let (f1, c) = (sin2(wm / 2.0), wm.cos());
    let (a0, a1, a2, am) = (pp.s0 * pp.s0, pp.s1 * pp.s1, -4.0 * pp.a2, pp.den(wm));
    let (h0, gm, sm) = (amag2(&p, 0.0), amag2(&p, xm), aslope(&p, xm));
    // |B|² = |H|²·|A|² at DC, and its value and slope (in sin²(w/2)) at wm.
    // The closed form keeps the small terms apart: B2 has no 1/f1² of a
    // difference.
    let big0 = h0 * a0;
    let big2 = (a0 * (gm - h0) - sm * (wm / 2.0).tan() / wm * am) / (4.0 * f1 * f1) + gm * a2;
    let r2 = gm * (a1 - a0 + 4.0 * a2 * c) + 2.0 * sm / (wm * wm.sin()) * am;
    let b = numerator(big0, r2 + big0 - 4.0 * big2 * c, big2).unwrap_or_else(|| {
        // The slope cannot be met: match the analog value at Nyquist instead.
        let big1 = amag2(&p, PI / w0) * a1;
        let b2n = (gm * am - big0 * (1.0 - f1) - big1 * f1) / (4.0 * (1.0 - f1) * f1);
        let w = (big0.sqrt() + big1.sqrt()) / 2.0;
        numerator(big0, big1, b2n).or_else(|| numerator(big0, big1, b2n.max(-w * w))).unwrap_or([1.0, 0.0, 0.0])
    });
    [b[0], b[1], b[2], 1.0, pp.a1, pp.a2]
}

/// A cut's bilinear second-order section (a Butterworth pair of quality
/// `q`), prewarped at `f`, as the window's `cutPair`.
fn cut_pair(fs: f64, kind: Kind, f: f64, q: f64) -> [f64; 6] {
    let w = 2.0 * PI * f.min(fs * CUT_CAP) / fs;
    let al = w.sin() / (2.0 * q);
    let c = w.cos();
    // 1 - cos and 1 + cos, the first without cancellation.
    let lo = 2.0 * sin2(w / 2.0);
    let hi = 2.0 - lo;
    if kind == Kind::HighCut {
        [lo / 2.0, lo, lo / 2.0, 1.0 + al, -2.0 * c, 1.0 - al]
    } else {
        [hi / 2.0, -hi, hi / 2.0, 1.0 + al, -2.0 * c, 1.0 - al]
    }
}

/// A cut's first-order bilinear section, as the window's `cutFirst`.
fn cut_first(fs: f64, kind: Kind, f: f64) -> [f64; 6] {
    let k = (PI * f.min(fs * CUT_CAP) / fs).tan();
    if kind == Kind::HighCut { [k, k, 0.0, 1.0 + k, k - 1.0, 0.0] } else { [1.0, -1.0, 0.0, 1.0 + k, k - 1.0, 0.0] }
}

/// A band's sections, as the window's `sections`.
fn band_sections(fs: f64, b: &Band, out: &mut Vec<[f64; 6]>) {
    match b.kind {
        Kind::Tilt => {
            out.push(matched(fs, Kind::HighShelf, b.f, b.gain / 2.0, 0.6));
            out.push(matched(fs, Kind::LowShelf, b.f, -b.gain / 2.0, 0.6));
        }
        Kind::LowCut | Kind::HighCut => {
            let n = (b.slope / 6) as usize;
            if n % 2 == 1 {
                out.push(cut_first(fs, b.kind, b.f));
            }
            let pairs = n / 2;
            for k in 0..pairs {
                let th = if n % 2 == 1 { PI * (k + 1) as f64 / n as f64 } else { PI * (2 * k + 1) as f64 / (2 * n) as f64 };
                let mut q = 1.0 / (2.0 * th.cos());
                if k == pairs - 1 {
                    q *= b.q / std::f64::consts::FRAC_1_SQRT_2;
                }
                out.push(cut_pair(fs, b.kind, b.f, q));
            }
        }
        _ => out.push(matched(fs, b.kind, b.f, b.gain, b.q)),
    }
}

/// |H(e^jw)|² of a section, dB.
fn section_db(s: &Sos, w: f64) -> f64 {
    let (s1, c1) = w.sin_cos();
    let (s2, c2) = (2.0 * w).sin_cos();
    let nr = s[0] + s[1] * c1 + s[2] * c2;
    let ni = -(s[1] * s1 + s[2] * s2);
    let dr = 1.0 + s[3] * c1 + s[4] * c2;
    let di = -(s[3] * s1 + s[4] * s2);
    10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).max(1e-30).log10()
}

/// An EQ for one sample rate: its sections and the gain after them.
#[derive(Clone)]
pub struct EqDesign {
    pub fs: u32,
    n: usize,
    sos: [Sos; MAX_SECTIONS],
    /// Per section: (band index, section number in the band, sections in the
    /// band, type) packed — what makes two designs' sections the same.
    layout: [u32; MAX_SECTIONS],
    /// What follows the cascade, linear: l' = m[0]·l + m[1]·r,
    /// r' = m[2]·l + m[3]·r — the level and output gain on the diagonal,
    /// the balance's weights in it (and off it, for mid/side).
    mix: [f64; 4],
    pub active: bool,
    pub mode: GainMode,
    /// The gain mode's level, dB (0 in manual).
    pub level_db: f64,
    pub output_db: f64,
    pub balance_mode: BalanceMode,
    /// The balance, dB (+ toward the right or the side).
    pub balance_db: f64,
    /// The speaker delay, ms (+ the right late, − the left).
    pub delay_ms: f64,
    /// The cascade's peak over 20 Hz–20 kHz, dB (before the gain).
    pub peak_db: f64,
}

impl EqDesign {
    /// The EQ that changes nothing (not active).
    pub fn identity(fs: u32) -> Box<EqDesign> {
        Box::new(EqDesign {
            fs,
            n: 0,
            sos: [[0.0; 5]; MAX_SECTIONS],
            layout: [0; MAX_SECTIONS],
            mix: [1.0, 0.0, 0.0, 1.0],
            active: false,
            mode: GainMode::Manual,
            level_db: 0.0,
            output_db: 0.0,
            balance_mode: BalanceMode::LeftRight,
            balance_db: 0.0,
            delay_ms: 0.0,
            peak_db: 0.0,
        })
    }

    pub fn new(state: &EqState, fs: u32) -> Box<EqDesign> {
        let mut d = Self::identity(fs);
        let Some(state) = state.effective().filter(|s| s.active()) else {
            return d;
        };
        let state = &state;
        let fsf = fs as f64;
        let mut secs = Vec::with_capacity(16);
        for b in state.active_bands() {
            let from = secs.len();
            band_sections(fsf, &b, &mut secs);
            let count = secs.len() - from;
            for k in 0..count {
                let i = from + k;
                if i < MAX_SECTIONS {
                    d.layout[i] = (b.index as u32) << 16 | (k as u32) << 11 | (count as u32) << 4 | b.kind as u32;
                }
            }
        }
        d.n = secs.len().min(MAX_SECTIONS);
        for (i, c) in secs.iter().take(d.n).enumerate() {
            d.sos[i] = [c[0] / c[3], c[1] / c[3], c[2] / c[3], c[4] / c[3], c[5] / c[3]];
        }
        d.peak_db = if d.n == 0 {
            0.0
        } else {
            (0..GRID_POINTS)
                .map(|i| d.cascade_db(20.0 * 1000f64.powf(i as f64 / (GRID_POINTS - 1) as f64)))
                .fold(f64::NEG_INFINITY, f64::max)
        };
        let (mode, output_db) = state.level();
        d.level_db = match mode {
            GainMode::Manual => 0.0,
            GainMode::Safe if d.peak_db > 0.0 => -d.peak_db - SAFE_MARGIN_DB,
            GainMode::Safe => 0.0,
            GainMode::Auto => d.auto_gain_db(),
        };
        d.mode = mode;
        d.output_db = output_db;
        (d.balance_mode, d.balance_db) = state.balance();
        d.mix = balance_mix(10f64.powf((d.level_db + d.output_db) / 20.0), d.balance_mode, d.balance_db);
        d.delay_ms = state.delay_ms();
        d.active = true;
        d
    }

    /// The cascade's magnitude at `f` Hz, dB (without the gain after it).
    pub fn cascade_db(&self, f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f / self.fs as f64;
        self.sos[..self.n].iter().map(|s| section_db(s, w)).sum()
    }

    /// Auto Gain's level: minus the cascade's power average over the level
    /// grid, each point weighted by BS.1770's K-weighting (|K|², its 48 kHz
    /// sections) — the log grid weighs every octave alike, as pink noise
    /// (music's long-term spectrum) does — so the EQ on sounds as loud as
    /// off. Clamped to ±30 dB.
    fn auto_gain_db(&self) -> f64 {
        const K_SHELF: Sos = [1.53512485958697, -2.69169618940638, 1.19839281085285, -1.69065929318241, 0.73248077421585];
        const K_HP: Sos = [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621];
        let (mut p, mut sum) = (0.0, 0.0);
        for i in 0..GRID_POINTS {
            let f = 20.0 * 1000f64.powf(i as f64 / (GRID_POINTS - 1) as f64);
            let wk = 2.0 * std::f64::consts::PI * f / 48_000.0;
            let w = 10f64.powf((section_db(&K_SHELF, wk) + section_db(&K_HP, wk)) / 10.0);
            p += w * 10f64.powf(self.cascade_db(f) / 10.0);
            sum += w;
        }
        (-10.0 * (p / sum).log10()).clamp(-30.0, 30.0)
    }

    /// The whole EQ's magnitude at `f` Hz, dB: the cascade and its gain.
    #[cfg(test)]
    pub fn response_db(&self, f: f64) -> f64 {
        if self.active { self.cascade_db(f) + self.level_db + self.output_db } else { 0.0 }
    }

    /// How far a full-scale tone may go over 0 dBFS with this EQ, dB (0 in
    /// safe with no output gain): the window's hint.
    pub fn clip_risk_db(&self) -> f64 {
        if self.active { (self.peak_db + self.level_db + self.output_db).max(0.0) } else { 0.0 }
    }

    #[cfg(test)]
    pub fn sections(&self) -> usize {
        self.n
    }

    /// How late each channel plays, frames `[l, r]`: the delay at this
    /// rate in whole frames (one is 7.8 mm at 44.1 kHz), on its one side.
    pub fn delay_frames(&self) -> [usize; 2] {
        let f = delay_frames(self.delay_ms, self.fs);
        if !self.active || f == 0 {
            [0, 0]
        } else if self.delay_ms < 0.0 {
            [f, 0]
        } else {
            [0, f]
        }
    }
}

fn delay_frames(ms: f64, fs: u32) -> usize {
    (ms.abs() * fs as f64 / 1000.0).round() as usize
}

/// The matrix after the cascade (`EqDesign::mix`) for a linear gain `g` and
/// a balance. The balance only ever lowers: the side it moves away from is
/// `|db|` quieter and the other keeps `g`, so it adds no over and at 0 the
/// matrix is the gain alone. Mid/side weighs m = (l+r)/2 and s = (l−r)/2
/// the same way (+ = the mid lower, wider; − = the side lower, narrower);
/// |m[0]| + |m[1]| is still `g`, so neither channel can grow.
fn balance_mix(g: f64, mode: BalanceMode, db: f64) -> [f64; 4] {
    let cut = 10f64.powf(-db.abs() / 20.0);
    let (lo, hi) = if db > 0.0 { (cut, 1.0) } else if db < 0.0 { (1.0, cut) } else { (1.0, 1.0) };
    match mode {
        // lo weighs the left, hi the right
        BalanceMode::LeftRight => [g * lo, 0.0, 0.0, g * hi],
        // lo weighs the mid, hi the side
        BalanceMode::MidSide => {
            let (a, b) = (g * (lo + hi) / 2.0, g * (lo - hi) / 2.0);
            [a, b, b, a]
        }
    }
}

/// A cascade's memory, both channels (transposed direct form II).
#[derive(Clone)]
pub struct EqRun {
    z: [[[f64; 2]; MAX_SECTIONS]; 2],
}

impl EqRun {
    pub fn new() -> Box<EqRun> {
        Box::new(EqRun { z: [[[0.0; 2]; MAX_SECTIONS]; 2] })
    }

    pub fn reset(&mut self) {
        self.z = [[[0.0; 2]; MAX_SECTIONS]; 2];
    }

    /// The EQ over a stereo block, in place. A block cut anywhere gives the
    /// same samples as the whole: what a period of the player gets is what
    /// the converter's file gets. Not active, the block is left as it was.
    pub fn process(&mut self, d: &EqDesign, l: &mut [f64], r: &mut [f64]) {
        if !d.active {
            return;
        }
        for (z, buf) in self.z.iter_mut().zip([&mut *l, &mut *r]) {
            for (s, zs) in d.sos[..d.n].iter().zip(z.iter_mut()) {
                let [b0, b1, b2, a1, a2] = *s;
                let [mut z1, mut z2] = *zs;
                for x in buf.iter_mut() {
                    let xi = *x;
                    let y = b0 * xi + z1;
                    z1 = b1 * xi - a1 * y + z2;
                    z2 = b2 * xi - a2 * y;
                    *x = y;
                }
                if z1.abs() < TINY {
                    z1 = 0.0;
                }
                if z2.abs() < TINY {
                    z2 = 0.0;
                }
                *zs = [z1, z2];
            }
        }
        let [ll, lr, rl, rr] = d.mix;
        if lr == 0.0 && rl == 0.0 {
            for (buf, g) in [(l, ll), (r, rr)] {
                if g != 1.0 {
                    for x in buf.iter_mut() {
                        *x *= g;
                    }
                }
            }
        } else {
            for (x, y) in l.iter_mut().zip(r.iter_mut()) {
                let (a, b) = (*x, *y);
                *x = ll * a + lr * b;
                *y = rl * a + rr * b;
            }
        }
    }

    /// Memory for `new` taken from this run of `old`: a section the two
    /// designs share (same band, type, place) keeps its memory, any other
    /// starts from silence. A moved band then glides rather than restarts.
    pub fn carry(&self, old: &EqDesign, new: &EqDesign, into: &mut EqRun) {
        into.reset();
        for i in 0..new.n {
            let key = new.layout[i];
            if let Some(j) = (0..old.n).find(|&j| old.layout[j] == key) {
                for ch in 0..2 {
                    into.z[ch][i] = self.z[ch][j];
                }
            }
        }
    }
}

/// The speaker delay after the EQ: each channel read from its own ring
/// `[l, r]` frames late. A changed delay crossfades from the old tap to the
/// new one over `XFADE_S` (one asked meanwhile waits for the end), so it
/// never clicks. With no channel late and no fade the samples are left as
/// they are and the rings are not kept: a delay started from there reads
/// silence for what it has not seen. Allocated once.
pub struct Delay {
    ring: [Vec<f64>; 2],
    w: usize,
    now: [usize; 2],
    to: [usize; 2],
    xf_len: usize,
    xf_pos: usize,
    fading: bool,
}

impl Delay {
    pub fn new(fs: u32) -> Delay {
        let cap = delay_frames(DELAY_MS, fs) + 1;
        Delay {
            ring: [vec![0.0; cap], vec![0.0; cap]],
            w: 0,
            now: [0, 0],
            to: [0, 0],
            xf_len: ((XFADE_S * fs as f64).round() as usize).max(1),
            xf_pos: 0,
            fading: false,
        }
    }

    fn clamp(&self, want: [usize; 2]) -> [usize; 2] {
        want.map(|d| d.min(self.ring[0].len() - 1))
    }

    /// `want` from now on, at once, from silence (a stream's start or a
    /// stop).
    pub fn set(&mut self, want: [usize; 2]) {
        for r in &mut self.ring {
            r.fill(0.0);
        }
        self.w = 0;
        self.now = self.clamp(want);
        self.to = self.now;
        self.fading = false;
    }

    /// Something to do: a channel late, or a fade under way.
    pub fn busy(&self) -> bool {
        self.fading || self.now != [0, 0]
    }

    /// The next block, in place, toward `want`.
    pub fn process(&mut self, l: &mut [f64], r: &mut [f64], want: [usize; 2]) {
        let want = self.clamp(want);
        if !self.fading && want != self.now {
            if self.now == [0, 0] {
                self.set([0, 0]);
            }
            self.to = want;
            self.xf_pos = 0;
            self.fading = true;
        }
        if !self.busy() {
            return;
        }
        let cap = self.ring[0].len();
        let len = self.xf_len as f64;
        for (x, y) in l.iter_mut().zip(r.iter_mut()) {
            let w = self.w;
            let t = (self.xf_pos + 1) as f64 / len;
            for (c, v) in [x, y].into_iter().enumerate() {
                self.ring[c][w] = *v;
                let ring = &self.ring[c];
                let old = ring[(w + cap - self.now[c]) % cap];
                *v = if self.fading { old + (ring[(w + cap - self.to[c]) % cap] - old) * t } else { old };
            }
            self.w = (w + 1) % cap;
            if self.fading {
                self.xf_pos += 1;
                if self.xf_pos >= self.xf_len {
                    self.now = self.to;
                    self.fading = false;
                }
            }
        }
    }
}

/// What the EQ left over full scale, clipped to ±1.0 in place; the count of
/// samples clipped and the highest |sample| before the clip. Run right after
/// an active EQ, by the player and the converter alike: an over is a clipped
/// sample, never a muted period. NaN is left as it is for the guards behind.
pub fn clip(l: &mut [f64], r: &mut [f64]) -> (u64, f64) {
    let (mut over, mut peak) = (0u64, 0f64);
    for x in l.iter_mut().chain(r.iter_mut()) {
        let a = x.abs();
        peak = peak.max(a);
        if a > 1.0 {
            *x = x.clamp(-1.0, 1.0);
            over += 1;
        }
    }
    (over, peak)
}

/// The converter's EQ over a file's output, whole or block by block: the
/// gain the chain had already applied, then the cascade and the clip as the
/// player's output thread runs them. The cascade is linear, so the ceiling
/// is measured on what it gives (`cascade`, or a fresh `EqRun` over a
/// streamed render) and its gain put in after (`level`): the EQ's boosts
/// and output gain are held under the true-peak target like the rest.
pub struct EqTail {
    design: Box<EqDesign>,
    run: Box<EqRun>,
    delay: Delay,
    /// Samples clipped at full scale so far.
    pub clipped: u64,
    /// The highest |sample| the cascade gave, before the clip.
    pub peak: f64,
}

impl EqTail {
    /// None when the state changes nothing: the tail is left as it was.
    pub fn new(state: Option<&EqState>, fs: u32) -> Option<EqTail> {
        let state = state.filter(|s| s.active())?;
        let design = EqDesign::new(state, fs);
        if !design.active {
            return None;
        }
        let mut delay = Delay::new(fs);
        delay.set(design.delay_frames());
        Some(EqTail { design, run: EqRun::new(), delay, clipped: 0, peak: 0.0 })
    }

    pub fn design(&self) -> &EqDesign {
        &self.design
    }

    /// The next block, in place: `gain`, the cascade, the clip, the delay.
    pub fn process(&mut self, l: &mut [f64], r: &mut [f64], gain: f64) {
        if gain != 1.0 {
            for x in l.iter_mut().chain(r.iter_mut()) {
                *x *= gain;
            }
        }
        self.run.process(&self.design, l, r);
        self.count_clip(l, r);
        self.delay.process(l, r, self.design.delay_frames());
    }

    /// The cascade alone over the next block, in place: no gain, no clip.
    /// What the ceiling measures on a render in RAM; `level` follows.
    pub fn cascade(&mut self, l: &mut [f64], r: &mut [f64]) {
        self.run.process(&self.design, l, r);
    }

    /// `gain`, the clip and the delay on what `cascade` gave: with
    /// `cascade` the same samples as `process` (the cascade is linear), but
    /// the gain is known only once the cascade's output is measured. The
    /// late channel keeps the file's length: its last frames are left out.
    pub fn level(&mut self, l: &mut [f64], r: &mut [f64], gain: f64) {
        if gain != 1.0 {
            for x in l.iter_mut().chain(r.iter_mut()) {
                *x *= gain;
            }
        }
        self.count_clip(l, r);
        self.delay.process(l, r, self.design.delay_frames());
    }

    fn count_clip(&mut self, l: &mut [f64], r: &mut [f64]) {
        let (over, peak) = clip(l, r);
        self.clipped += over;
        self.peak = self.peak.max(peak);
    }
}

/// `LiveEq`'s crossfade between two designs, seconds.
pub const XFADE_S: f64 = 0.03;

/// The player's EQ, changed while it plays: a new design is offered from
/// the outside, taken at the start of the next block and crossfaded in
/// over `XFADE_S` (the old cascade and the new one both run, mixed
/// linearly), so a moved band never clicks; the speaker delay after it
/// (`Delay`) fades to the new design's alongside. Everything is allocated
/// once:
/// `offer` copies into a box that is already there, `process` takes no
/// heap. With no design or an identity one the samples are left as they
/// are — bit for bit.
pub struct LiveEq {
    a: Box<EqDesign>,
    ra: Box<EqRun>,
    b: Box<EqDesign>,
    rb: Box<EqRun>,
    pending: Box<EqDesign>,
    has_pending: bool,
    xf_len: usize,
    xf_pos: usize,
    running: bool,
    /// Both runs hold silence: `reset` has nothing to do.
    quiet: bool,
    scratch_l: Vec<f64>,
    scratch_r: Vec<f64>,
    delay: Delay,
}

impl LiveEq {
    /// `block`: the scratch the crossfade works in, frames (any length
    /// works, longer is fewer passes).
    pub fn new(fs: u32, block: usize) -> LiveEq {
        LiveEq {
            a: EqDesign::identity(fs),
            ra: EqRun::new(),
            b: EqDesign::identity(fs),
            rb: EqRun::new(),
            pending: EqDesign::identity(fs),
            has_pending: false,
            xf_len: ((XFADE_S * fs as f64).round() as usize).max(1),
            xf_pos: 0,
            running: false,
            quiet: true,
            scratch_l: vec![0.0; block.max(1)],
            scratch_r: vec![0.0; block.max(1)],
            delay: Delay::new(fs),
        }
    }

    /// The design played from now on, at once, no crossfade (a stream's
    /// start, before any sound).
    pub fn set(&mut self, d: &EqDesign) {
        *self.a = d.clone();
        self.ra.reset();
        self.running = false;
        self.has_pending = false;
        self.quiet = true;
        self.delay.set(d.delay_frames());
    }

    /// The design to crossfade to, taken at the next block (a later offer
    /// before then replaces it).
    pub fn offer(&mut self, d: &EqDesign) {
        (*self.pending).clone_from(d);
        self.has_pending = true;
    }

    /// `offer`, or `set` when nothing has played since the last `set` or
    /// `reset`: from silence there is nothing to glide from.
    pub fn change(&mut self, d: &EqDesign) {
        if self.quiet && !self.running {
            self.set(d);
        } else {
            self.offer(d);
        }
    }

    /// Something to do: a design that changes the samples, or a change
    /// under way.
    pub fn active(&self) -> bool {
        self.a.active || self.running || self.has_pending || self.delay.busy()
    }

    /// The music stopped (silence sent instead): the memories cleared, a
    /// change under way or offered is simply in force.
    pub fn reset(&mut self) {
        if self.running {
            std::mem::swap(&mut self.a, &mut self.b);
            self.running = false;
        }
        if self.has_pending {
            std::mem::swap(&mut self.a, &mut self.pending);
            self.has_pending = false;
        }
        if !self.quiet {
            self.ra.reset();
            self.rb.reset();
            self.quiet = true;
        }
        self.delay.set(self.a.delay_frames());
    }

    pub fn process(&mut self, l: &mut [f64], r: &mut [f64]) {
        self.cascade(l, r);
        let want = if self.running { self.b.delay_frames() } else { self.a.delay_frames() };
        self.delay.process(l, r, want);
    }

    fn cascade(&mut self, l: &mut [f64], r: &mut [f64]) {
        if !self.running && self.has_pending {
            std::mem::swap(&mut self.b, &mut self.pending);
            self.has_pending = false;
            self.ra.carry(&self.a, &self.b, &mut self.rb);
            self.xf_pos = 0;
            self.running = true;
        }
        if !self.running {
            if self.a.active {
                self.quiet = false;
                self.ra.process(&self.a, l, r);
            }
            return;
        }
        self.quiet = false;
        let n = l.len().min(r.len());
        let mut at = 0;
        while at < n {
            if !self.running {
                self.ra.process(&self.a, &mut l[at..n], &mut r[at..n]);
                return;
            }
            let k = (n - at).min(self.scratch_l.len()).min(self.xf_len - self.xf_pos);
            let (sl, sr) = (&mut self.scratch_l[..k], &mut self.scratch_r[..k]);
            let (bl, br) = (&mut l[at..at + k], &mut r[at..at + k]);
            sl.copy_from_slice(bl);
            sr.copy_from_slice(br);
            self.ra.process(&self.a, bl, br);
            self.rb.process(&self.b, sl, sr);
            let len = self.xf_len as f64;
            for i in 0..k {
                let t = (self.xf_pos + i + 1) as f64 / len;
                bl[i] += (sl[i] - bl[i]) * t;
                br[i] += (sr[i] - br[i]) * t;
            }
            self.xf_pos += k;
            at += k;
            if self.xf_pos >= self.xf_len {
                std::mem::swap(&mut self.a, &mut self.b);
                std::mem::swap(&mut self.ra, &mut self.rb);
                self.running = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../../../desktop-app/tests/fixtures/eq-golden.json")).unwrap()
    }

    fn one_band(b: &serde_json::Value) -> EqState {
        EqState { on: true, bands: vec![serde_json::from_value(b.clone()).unwrap()], ..Default::default() }
    }

    #[test]
    fn every_type_has_the_reference_magnitudes() {
        let g = golden();
        let cases = g["magnitudes"].as_array().unwrap();
        assert!(cases.len() >= 48);
        for c in cases {
            let fs = c["fs"].as_u64().unwrap() as u32;
            let d = EqDesign::new(&one_band(&c["band"]), fs);
            for (f, want) in c["freqs"].as_array().unwrap().iter().zip(c["db"].as_array().unwrap()) {
                let (f, want) = (f.as_f64().unwrap(), want.as_f64().unwrap());
                let got = d.cascade_db(f);
                // Far under the cutoff at 768 kHz the two evaluations part in
                // the 7th decimal of a dB (a0 divided in or not).
                assert!((got - want).abs() < 1e-5, "{} at {fs}: {f} Hz {got} vs {want}", c["band"]);
            }
            let b = one_band(&c["band"]).active_bands()[0];
            for (f, want) in c["freqs"].as_array().unwrap().iter().zip(c["analogDb"].as_array().unwrap()) {
                let (f, want) = (f.as_f64().unwrap(), want.as_f64().unwrap());
                assert!((analog_db(&b, f) - want).abs() < 1e-9, "analog {} at {f} Hz", c["band"]);
            }
        }
    }

    /// The analog curve a band's sections are matched to, dB.
    fn analog_db(b: &Band, fr: f64) -> f64 {
        let x = fr / b.f;
        let db = |kind, g, q| 10.0 * amag2(&analog(kind, g, q), x).max(1e-30).log10();
        match b.kind {
            Kind::Tilt => db(Kind::HighShelf, b.gain / 2.0, 0.6) + db(Kind::LowShelf, -b.gain / 2.0, 0.6),
            Kind::LowCut | Kind::HighCut => {
                let n = (b.slope / 6) as usize;
                let lp = b.kind == Kind::HighCut;
                let top = if lp { 1.0 } else { x * x };
                let mut d = if n % 2 == 1 { 10.0 * (top / (1.0 + x * x)).log10() } else { 0.0 };
                let pairs = n / 2;
                for k in 0..pairs {
                    let th = if n % 2 == 1 { PI * (k + 1) as f64 / n as f64 } else { PI * (2 * k + 1) as f64 / (2 * n) as f64 };
                    let mut q = 1.0 / (2.0 * th.cos());
                    if k == pairs - 1 {
                        q *= b.q / std::f64::consts::FRAC_1_SQRT_2;
                    }
                    d += db(b.kind, 0.0, q);
                }
                d
            }
            _ => db(b.kind, b.gain, b.q),
        }
    }

    /// How far a matched band may part from its analog curve (dB, where the
    /// analog curve is above -30 dB, up to 20 kHz), by rate and by where the
    /// frequency sits: up to 0.25 / 0.5 / 0.8 / 1 of Nyquist. Measured over
    /// |gain| ≤ 18 dB, q 0.3..10, 20 Hz..24 kHz, plus a margin. Near Nyquist at
    /// 44.1/48 kHz a single second-order section cannot follow a high, wide
    /// band's low skirt exactly; from 88.2 kHz on the curves are the analog.
    fn matched_tolerance(fs: u32, kind: Kind, zone: usize) -> f64 {
        if fs >= 176_400 {
            return match kind {
                Kind::Notch | Kind::BandPass => 0.25,
                _ => 0.05,
            };
        }
        if fs >= 88_200 {
            return match kind {
                Kind::Bell | Kind::Tilt => 0.17,
                Kind::BandPass => 0.25,
                _ => 0.8,
            };
        }
        let t: [f64; 4] = match kind {
            Kind::Bell => [1.8, 2.0, 2.0, 1.6],
            Kind::LowShelf | Kind::HighShelf => [0.9, 1.4, 4.5, 4.5],
            Kind::Tilt => [1.0, 2.0, 2.0, 0.8],
            // A notch near Nyquist: its skirt above the centre is cut short.
            Kind::Notch => [0.7, 2.4, 11.0, 21.0],
            _ => [3.2, 3.2, 2.6, 1.8],
        };
        t[zone]
    }

    #[test]
    fn matched_bands_follow_their_analog_curve_at_every_rate() {
        let kinds = [Kind::Bell, Kind::LowShelf, Kind::HighShelf, Kind::Tilt, Kind::Notch, Kind::BandPass];
        let freqs: Vec<f64> = (0..48).map(|i| 20.0 * 1000f64.powf(i as f64 / 47.0)).collect();
        let mut worst = Vec::new();
        for fs in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000] {
            for kind in kinds {
                let mut w = [0.0f64; 4];
                for f in [20.0, 60.0, 200.0, 1000.0, 3000.0, 8000.0, 12000.0, 16000.0, 18000.0, 20000.0, 22000.0, 24000.0] {
                    for q in [0.3, 0.71, 2.0, 5.0, 10.0] {
                        for g in [-18.0, -6.0, 3.0, 12.0, 18.0] {
                            let b = Band { index: 0, kind, f, gain: g, q, slope: 12 };
                            let mut secs = Vec::new();
                            band_sections(fs as f64, &b, &mut secs);
                            for &fr in &freqs {
                                let a = analog_db(&b, fr);
                                if a < -30.0 {
                                    continue;
                                }
                                let w0 = 2.0 * PI * fr / fs as f64;
                                let got: f64 = secs.iter().map(|c| section_db(&[c[0] / c[3], c[1] / c[3], c[2] / c[3], c[4] / c[3], c[5] / c[3]], w0)).sum();
                                let r = fr / (fs as f64 / 2.0);
                                let zone = if r <= 0.25 { 0 } else if r <= 0.5 { 1 } else if r <= 0.8 { 2 } else { 3 };
                                w[zone] = w[zone].max((got - a).abs());
                            }
                        }
                    }
                }
                worst.push((fs, kind, w));
            }
        }
        let over: Vec<String> = worst
            .iter()
            .filter(|(fs, kind, w)| w.iter().enumerate().any(|(z, e)| *e > matched_tolerance(*fs, *kind, z)))
            .map(|(fs, kind, w)| format!("{kind:?} at {fs}: {w:.3?}"))
            .collect();
        assert!(over.is_empty(), "over the tolerance:
{}", over.join("
"));
    }

    /// Far under the rate (10 Hz at 768 kHz) the sections still hold the
    /// analog curve: no round-off in 1 - cos or 1 - r.
    #[test]
    fn low_bands_at_768k_are_exact() {
        let fs = 768_000;
        for b in [
            Band { index: 0, kind: Kind::LowCut, f: 10.0, gain: 0.0, q: 0.71, slope: 96 },
            Band { index: 0, kind: Kind::Bell, f: 20.0, gain: 9.0, q: 3.0, slope: 12 },
            Band { index: 0, kind: Kind::Bell, f: 20.0, gain: -9.0, q: 3.0, slope: 12 },
            Band { index: 0, kind: Kind::LowShelf, f: 10.0, gain: 6.0, q: 0.71, slope: 12 },
            Band { index: 0, kind: Kind::Notch, f: 20.0, gain: 0.0, q: 2.0, slope: 12 },
        ] {
            let mut secs = Vec::new();
            band_sections(fs as f64, &b, &mut secs);
            for fr in [5.0, 10.0, 14.0, 20.0, 28.0, 40.0, 100.0] {
                let a = analog_db(&b, fr);
                if a < -100.0 {
                    continue;
                }
                let w0 = 2.0 * PI * fr / fs as f64;
                let got: f64 = secs.iter().map(|c| section_db(&[c[0] / c[3], c[1] / c[3], c[2] / c[3], c[4] / c[3], c[5] / c[3]], w0)).sum();
                assert!((got - a).abs() < 1e-3, "{b:?} at {fr} Hz: {got} vs {a}");
            }
        }
        // A 20 Hz tone through a 20 Hz bell at 768 kHz has the drawn level.
        let st = EqState {
            on: true,
            bands: vec![EqBand { f: 20.0, gain: 9.0, q: 3.0, ..Default::default() }],
            ..Default::default()
        };
        let d = EqDesign::new(&st, fs);
        let n = fs as usize * 2;
        let x = sine(fs, 20.0, 0.25, n);
        let (mut l, mut r) = (x.clone(), x);
        EqRun::new().process(&d, &mut l, &mut r);
        let got = 20.0 * (peak(&l[n / 2..]) / 0.25).log10();
        assert!((got - 9.0).abs() < 0.01 && (d.response_db(20.0) - 9.0).abs() < 1e-4, "{got} {}", d.response_db(20.0));
    }

    /// A band moved while it plays (frequency, gain and q at once) glides in
    /// over the crossfade without broadband energy: what a 96 dB/oct low cut
    /// three octaves above the tone leaves of the change is far below hearing.
    #[test]
    fn a_moved_band_glides_without_a_spectral_click() {
        let fs = 48_000;
        let n = 24_000;
        let x = sine(fs, 500.0, 0.5, n);
        let st = |f: f64, gain: f64, q: f64| EqState {
            on: true,
            bands: vec![EqBand { f, gain, q, ..Default::default() }],
            ..Default::default()
        };
        let probe = EqDesign::new(&EqState { on: true, bands: vec![EqBand { kind: "lowCut".into(), f: 4000.0, slope: 96.0, ..Default::default() }], ..Default::default() }, fs);
        let residue = |change: bool| {
            let mut eq = LiveEq::new(fs, 256);
            let (mut l, _) = live(&mut eq, &x, 480, |i, eq| {
                if i == 0 {
                    eq.set(&EqDesign::new(&st(500.0, 6.0, 1.0), fs));
                }
                if change && i == 9600 {
                    eq.offer(&EqDesign::new(&st(700.0, 12.0, 2.0), fs));
                }
            });
            let mut r = l.clone();
            EqRun::new().process(&probe, &mut l, &mut r);
            peak(&l[4800..])
        };
        let (steady, moved) = (residue(false), residue(true));
        assert!(moved < 1e-4 && steady < 1e-6, "steady {steady}, moved {moved}");
    }

    #[test]
    fn gain_levels_and_active_are_the_references() {
        let g = golden();
        for c in g["levels"].as_array().unwrap() {
            let fs = c["fs"].as_u64().unwrap() as u32;
            let st: EqState = serde_json::from_value(c["state"].clone()).unwrap();
            let (mode, output) = st.level();
            assert_eq!((mode.name(), output), (c["gain"].as_str().unwrap(), c["output"].as_f64().unwrap()), "{}", c["state"]);
            let d = EqDesign::new(&st, fs);
            let want = c["levelDb"].as_f64().unwrap();
            if d.active {
                assert!((d.level_db - want).abs() < 1e-7, "{} at {fs}: {} vs {want}", c["state"], d.level_db);
                assert_eq!(d.output_db, output);
            }
        }
        for c in g["active"].as_array().unwrap() {
            let st: EqState = serde_json::from_value(c["state"].clone()).unwrap();
            assert_eq!(st.active(), c["active"].as_bool().unwrap(), "{}", c["state"]);
            assert_eq!(EqDesign::new(&st, 48000).active, st.active());
        }
    }

    #[test]
    fn state_reads_with_defaults_clamps_and_drops_what_it_cannot_run() {
        let st: EqState = serde_json::from_str("{}").unwrap();
        assert_eq!(st, EqState { v: 1, gain: String::new(), ..Default::default() }, "no version = version 1");
        assert!(!st.active());
        let st: EqState = serde_json::from_str(
            r#"{"v":1,"on":true,"name":"Flat","bands":[{"type":"bell","f":1000,"gain":0,"q":0.71,"slope":12,"on":true}],"preamp":null,"output":0}"#,
        )
        .unwrap();
        assert!(!st.active(), "a 0 dB bell changes nothing");
        let st: EqState = serde_json::from_str(
            r#"{"on":true,"bands":[{"type":"wobble","gain":3},{"type":"bell","f":99999,"gain":45,"q":50},{"type":"lowCut","f":1,"slope":100}],"preamp":-50,"output":20}"#,
        )
        .unwrap();
        let b = st.active_bands();
        assert_eq!(b.len(), 2, "the unknown type is left out");
        assert_eq!((b[0].index, b[0].f, b[0].gain, b[0].q), (1, 24000.0, 30.0, 24.0));
        assert_eq!((b[1].f, b[1].slope), (10.0, 96));
        assert_eq!(st.level(), (GainMode::Manual, -10.0), "a version-1 preamp (clamped to -30) goes into the output");
        let many = EqState {
            on: true,
            bands: (0..30).map(|i| EqBand { gain: 1.0, f: 100.0 + i as f64, ..Default::default() }).collect(),
            ..Default::default()
        };
        assert_eq!(many.active_bands().len(), MAX_BANDS);
        let back: EqState = serde_json::from_str(&serde_json::to_string(&many).unwrap()).unwrap();
        assert_eq!(back, many);
    }

    /// Version-1 states keep their sound: auto preamp -> safe, a manual one
    /// into the output; a `gain` field wins; written back, a state reads the
    /// same.
    #[test]
    fn a_version_1_state_migrates_and_reads_the_same_written_back() {
        let read = |j: &str| serde_json::from_str::<EqState>(j).unwrap();
        let bell = r#""on":true,"bands":[{"type":"bell","f":1000,"gain":6,"q":1}]"#;
        assert_eq!(read(&format!(r#"{{"v":1,{bell},"preamp":null,"output":1}}"#)).level(), (GainMode::Safe, 1.0));
        assert_eq!(read(&format!(r#"{{"v":1,{bell},"preamp":-2.5,"output":1}}"#)).level(), (GainMode::Manual, -1.5));
        assert_eq!(read(&format!(r#"{{"v":1,{bell},"preamp":4,"output":1}}"#)).level(), (GainMode::Manual, 1.0));
        assert_eq!(read(&format!(r#"{{"v":1,{bell},"gain":"auto","preamp":-3}}"#)).level(), (GainMode::Auto, 0.0));
        assert_eq!(read(&format!(r#"{{"v":2,{bell},"gain":"x","output":44}}"#)).level(), (GainMode::Manual, 30.0));
        let old = read(&format!(r#"{{"v":1,{bell},"preamp":null}}"#));
        let d = EqDesign::new(&old, 48_000);
        assert!((d.level_db + d.peak_db + SAFE_MARGIN_DB).abs() < 1e-12 && d.peak_db > 5.9);
        for j in [r#"{"v":1,"preamp":null}"#, r#"{"v":1,"preamp":-2,"output":1}"#, r#"{"preamp":null}"#, r#"{"v":2,"gain":"auto"}"#] {
            let st = read(j);
            let text = serde_json::to_string(&st).unwrap();
            assert_eq!(read(&text).level(), st.level(), "{j} -> {text}");
        }
        assert!(!serde_json::to_string(&EqState::default()).unwrap().contains("preamp"));
        // Only an output gain, no band: active in every mode, the level 0.
        for gain in ["manual", "auto", "safe"] {
            let st = EqState { on: true, gain: gain.into(), output: -3.0, ..Default::default() };
            let d = EqDesign::new(&st, 48_000);
            assert!(d.active && d.level_db == 0.0 && (d.response_db(1000.0) + 3.0).abs() < 1e-12, "{gain}");
        }
    }

    /// Auto Gain keeps the loudness: K-weighted pink noise through the EQ at
    /// its level has the K-weighted power it had without the EQ.
    #[test]
    fn auto_gain_keeps_the_loudness_of_pink_noise() {
        let fs = 48_000;
        // Pink noise (Kellet's filter on a fixed-seed white noise), 8 s.
        let mut seed = 1u64;
        let (mut b, n) = ([0f64; 7], fs as usize * 8);
        let x: Vec<f64> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let w = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
                b[0] = 0.99886 * b[0] + w * 0.0555179;
                b[1] = 0.99332 * b[1] + w * 0.0750759;
                b[2] = 0.969 * b[2] + w * 0.153852;
                b[3] = 0.8665 * b[3] + w * 0.3104856;
                b[4] = 0.55 * b[4] + w * 0.5329522;
                b[5] = -0.7616 * b[5] - w * 0.016898;
                let o = b.iter().sum::<f64>() + w * 0.5362;
                b[6] = w * 0.115926;
                o * 0.05
            })
            .collect();
        // BS.1770's K-weighting as a design of its own.
        let mut k = EqDesign::identity(fs);
        k.sos[0] = [1.53512485958697, -2.69169618940638, 1.19839281085285, -1.69065929318241, 0.73248077421585];
        k.sos[1] = [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621];
        k.n = 2;
        k.active = true;
        let loud = |y: &[f64]| {
            let (mut l, mut r) = (y.to_vec(), y.to_vec());
            EqRun::new().process(&k, &mut l, &mut r);
            10.0 * (l[fs as usize..].iter().map(|v| v * v).sum::<f64>()).log10()
        };
        let before = loud(&x);
        for bands in [
            vec![EqBand { kind: "lowShelf".into(), f: 120.0, gain: 6.0, q: 0.7, ..Default::default() }],
            vec![EqBand { f: 3000.0, gain: -8.0, q: 1.0, ..Default::default() }, EqBand { kind: "highShelf".into(), f: 8000.0, gain: 4.0, ..Default::default() }],
            vec![EqBand { kind: "tilt".into(), f: 1000.0, gain: 6.0, ..Default::default() }],
        ] {
            let d = EqDesign::new(&EqState { on: true, gain: "auto".into(), bands, ..Default::default() }, fs);
            let (mut l, mut r) = (x.clone(), x.clone());
            EqRun::new().process(&d, &mut l, &mut r);
            let after = loud(&l);
            assert!((after - before).abs() < 0.35, "level {:+.2}: {:+.2} dB", d.level_db, after - before);
        }
    }

    fn sine(fs: u32, f: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / fs as f64).sin()).collect()
    }

    fn peak(x: &[f64]) -> f64 {
        x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
    }

    #[test]
    fn off_or_flat_leaves_every_bit() {
        let x = sine(48000, 997.0, 0.7, 4800);
        for st in [
            EqState::default(),
            EqState { on: true, ..Default::default() },
            EqState { on: true, bands: vec![EqBand { gain: 0.001, ..Default::default() }], ..Default::default() },
            EqState { on: false, bands: vec![EqBand { gain: 6.0, ..Default::default() }], output: 3.0, ..Default::default() },
        ] {
            let d = EqDesign::new(&st, 48000);
            assert!(!d.active && d.sections() == 0);
            let (mut l, mut r) = (x.clone(), x.clone());
            EqRun::new().process(&d, &mut l, &mut r);
            assert!(l.iter().chain(&r).zip(x.iter().chain(&x)).all(|(a, b)| a.to_bits() == b.to_bits()));
        }
    }

    #[test]
    fn safe_keeps_a_full_scale_tone_at_the_boost_under_0_dbfs_by_the_margin() {
        for (band, f) in [
            (EqBand { gain: 6.0, f: 1000.0, q: 1.0, ..Default::default() }, 1000.0),
            (EqBand { kind: "lowShelf".into(), gain: 9.0, f: 120.0, q: 0.7, ..Default::default() }, 40.0),
        ] {
            let st = EqState { on: true, bands: vec![band], gain: "safe".into(), ..Default::default() };
            let fs = 48000;
            let d = EqDesign::new(&st, fs);
            assert!(d.level_db < -6.4 && d.clip_risk_db() == 0.0);
            let x = sine(fs, f, 1.0, fs as usize);
            let (mut l, mut r) = (x.clone(), x);
            EqRun::new().process(&d, &mut l, &mut r);
            let p = peak(&l[fs as usize / 4..]);
            // The grid's step (1.5 %) may miss the very top by a few
            // ten-thousandths of a dB.
            let m = 10f64.powf(-SAFE_MARGIN_DB / 20.0);
            assert!(p <= 1.0012 * m && p > 0.98 * m, "{f} Hz: {p}");
        }
        let st = EqState { on: true, bands: vec![EqBand { gain: 6.0, ..Default::default() }], output: -2.0, ..Default::default() };
        assert!((EqDesign::new(&st, 48000).clip_risk_db() - 4.0).abs() < 0.01);
    }

    /// The cascade as it runs gives the magnitude it is drawn with.
    #[test]
    fn a_tone_through_the_cascade_has_the_drawn_level() {
        let fs = 96000;
        let st = EqState {
            on: true,
            bands: vec![
                EqBand { gain: 6.0, f: 1000.0, q: 1.0, ..Default::default() },
                EqBand { kind: "lowCut".into(), f: 200.0, slope: 96.0, ..Default::default() },
                EqBand { kind: "tilt".into(), f: 2000.0, gain: -4.0, ..Default::default() },
            ],
            ..Default::default()
        };
        let d = EqDesign::new(&st, fs);
        for f in [150.0, 400.0, 1000.0, 5000.0] {
            let x = sine(fs, f, 0.5, fs as usize);
            let (mut l, mut r) = (x.clone(), x);
            EqRun::new().process(&d, &mut l, &mut r);
            let got = 20.0 * (peak(&l[fs as usize / 2..]) / 0.5).log10();
            assert!((got - d.response_db(f)).abs() < 0.02, "{f} Hz: {got} vs {}", d.response_db(f));
        }
    }

    #[test]
    fn a_block_cut_anywhere_gives_the_same_samples() {
        let fs = 48000;
        let st = EqState {
            on: true,
            bands: vec![
                EqBand { gain: -7.0, f: 60.0, q: 4.0, ..Default::default() },
                EqBand { kind: "highCut".into(), f: 9000.0, slope: 18.0, ..Default::default() },
            ],
            output: 1.5,
            ..Default::default()
        };
        let d = EqDesign::new(&st, fs);
        let x: Vec<f64> = sine(fs, 440.0, 0.3, 20000).iter().zip(sine(fs, 55.0, 0.4, 20000)).map(|(a, b)| a + b).collect();
        let (mut l, mut r) = (x.clone(), x.clone());
        EqRun::new().process(&d, &mut l, &mut r);
        let (mut l2, mut r2) = (x.clone(), x);
        let mut run = EqRun::new();
        let mut at = 0;
        for n in [441, 1, 4096, 7, 480].iter().cycle() {
            let to = (at + n).min(l2.len());
            run.process(&d, &mut l2[at..to], &mut r2[at..to]);
            at = to;
            if at == l2.len() {
                break;
            }
        }
        assert!(l.iter().zip(&l2).chain(r.iter().zip(&r2)).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    fn bell(gain: f64) -> EqState {
        EqState { on: true, bands: vec![EqBand { gain, ..Default::default() }], ..Default::default() }
    }

    /// `x` through `eq` in blocks of `block` frames, `at(i, eq)` called
    /// before the block starting at frame i.
    fn live(eq: &mut LiveEq, x: &[f64], block: usize, mut at: impl FnMut(usize, &mut LiveEq)) -> (Vec<f64>, Vec<f64>) {
        let (mut l, mut r) = (x.to_vec(), x.iter().map(|v| -v).collect::<Vec<f64>>());
        let mut i = 0;
        while i < x.len() {
            let e = (i + block).min(x.len());
            at(i, eq);
            eq.process(&mut l[i..e], &mut r[i..e]);
            i = e;
        }
        (l, r)
    }

    #[test]
    fn a_change_while_playing_glides_in_without_a_step() {
        let fs = 48_000;
        let x = sine(fs, 1000.0, 0.25, 9600);
        let to = EqDesign::new(&bell(12.0), fs);
        assert!(to.active);
        for (block, scratch) in [(480, 256), (97, 4096), (1024, 1)] {
            let mut eq = LiveEq::new(fs, scratch);
            let (l, r) = live(&mut eq, &x, block, |i, eq| {
                if i == 0 {
                    eq.set(&EqDesign::new(&bell(-6.0), fs));
                }
                if i >= 2400 && i < 2400 + block {
                    eq.offer(&to);
                }
            });
            // The loudest the tone gets (at +12 dB) moves this much a sample
            // at most; a jump of the gain or the memories would be more.
            let natural = 0.25 * 10f64.powf(12.0 / 20.0) * 2.0 * std::f64::consts::PI * 1000.0 / fs as f64;
            for ch in [&l, &r] {
                let step = ch.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
                assert!(step <= natural * 1.05, "block {block}: step {step} vs {natural}");
            }
            // Over within the crossfade: the last 100 ms at +12 dB.
            let tail = peak(&l[x.len() - 4800..]);
            assert!((tail / 0.25 - 10f64.powf(12.0 / 20.0)).abs() < 0.01, "block {block}: {tail}");
        }
    }

    #[test]
    fn on_then_off_again_comes_back_to_every_bit() {
        let fs = 96_000;
        let x: Vec<f64> = sine(fs, 440.0, 0.3, 19_200).iter().zip(sine(fs, 7000.0, 0.2, 19_200)).map(|(a, b)| a + b).collect();
        let on = EqDesign::new(&bell(9.0), fs);
        let off = EqDesign::new(&EqState::default(), fs);
        let mut eq = LiveEq::new(fs, 512);
        assert!(!eq.active());
        let (l, r) = live(&mut eq, &x, 300, |i, eq| match i {
            4800 => eq.offer(&on),
            9600 => eq.offer(&off),
            _ => {}
        });
        let bits = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(a, b)| a.to_bits() == b.to_bits());
        let neg: Vec<f64> = x.iter().map(|v| -v).collect();
        assert!(bits(&l[..4800], &x[..4800]) && bits(&r[..4800], &neg[..4800]));
        assert!(!bits(&l[4800..9600], &x[4800..9600]));
        // Off again: from the end of its crossfade on, bit for bit.
        let back = 9600 + (XFADE_S * fs as f64) as usize;
        assert!(bits(&l[back..], &x[back..]) && bits(&r[back..], &neg[back..]));
        assert!(!eq.active());
    }

    #[test]
    fn set_plays_the_cascade_alone_and_a_reset_lands_on_the_change() {
        let fs = 44_100;
        let x = sine(fs, 3000.0, 0.4, 8000);
        let d = EqDesign::new(&bell(5.0), fs);
        let reference = |d: &EqDesign, x: &[f64]| {
            let (mut l, mut r) = (x.to_vec(), x.iter().map(|v| -v).collect::<Vec<f64>>());
            EqRun::new().process(d, &mut l, &mut r);
            (l, r)
        };
        let mut eq = LiveEq::new(fs, 64);
        let got = live(&mut eq, &x, 333, |i, eq| if i == 0 { eq.set(&d) });
        assert_eq!(got, reference(&d, &x));
        // A change under way when the music stops: the next music plays
        // it from silence, no crossfade left.
        let to = EqDesign::new(&bell(-4.0), fs);
        eq.offer(&to);
        let (mut l, mut r) = (x[..100].to_vec(), x[..100].to_vec());
        eq.process(&mut l, &mut r);
        eq.reset();
        assert_eq!(live(&mut eq, &x, 512, |_, _| {}), reference(&to, &x));
    }

    #[test]
    fn the_converter_s_tail_is_the_player_s_output_bit_for_bit() {
        // +12 dB in manual (no level): overs clipped on both sides.
        let fs = 96_000;
        let gain = 0.8912509381337456;
        let x: Vec<f64> = sine(fs, 1000.0, 0.6, 40_000).iter().zip(sine(fs, 97.0, 0.3, 40_000)).map(|(a, b)| a + b).collect();
        let state = bell(12.0);
        // The player: the chain's gain already in the samples, the output
        // thread's EQ and clip period by period.
        let mut eq = LiveEq::new(fs, 480);
        eq.set(&EqDesign::new(&state, fs));
        let (mut pl, mut pr) = (x.iter().map(|v| v * gain).collect::<Vec<f64>>(), x.iter().map(|v| -v * gain).collect::<Vec<f64>>());
        let mut over = 0;
        for (a, b) in pl.chunks_mut(480).zip(pr.chunks_mut(480)) {
            eq.process(a, b);
            over += clip(a, b).0;
        }
        assert!(over > 100);
        // The converter: whole (in RAM) and in streamed blocks.
        for block in [x.len(), 4096] {
            let mut t = EqTail::new(Some(&state), fs).unwrap();
            let (mut l, mut r) = (x.clone(), x.iter().map(|v| -v).collect::<Vec<f64>>());
            for (a, b) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
                t.process(a, b, gain);
            }
            assert!(l == pl && r == pr, "block {block}");
            assert_eq!(t.clipped, over);
            assert!(t.peak > 1.5);
        }
        // Off or flat: no tail at all.
        assert!(EqTail::new(None, fs).is_none());
        assert!(EqTail::new(Some(&EqState { on: false, ..state.clone() }), fs).is_none());
        assert!(EqTail::new(Some(&bell(0.0)), fs).is_none());
    }

    fn balanced(db: f64, mode: &str) -> EqState {
        EqState { on: true, balance: db, balance_mode: mode.into(), ..Default::default() }
    }

    /// Left/right: the side the balance moves away from is |db| quieter, the
    /// other keeps every bit; alone it makes the EQ active.
    #[test]
    fn balance_lowers_one_side_by_its_db_and_leaves_the_other() {
        let x = sine(48_000, 997.0, 0.7, 4800);
        for db in [0.1, 1.5, -0.73, 12.0, -30.0] {
            let st = balanced(db, "");
            assert!(st.active());
            let d = EqDesign::new(&st, 48_000);
            assert!(d.active && d.sections() == 0 && d.balance_mode == BalanceMode::LeftRight);
            let (mut l, mut r) = (x.clone(), x.clone());
            EqRun::new().process(&d, &mut l, &mut r);
            let (cut, kept) = if db > 0.0 { (&l, &r) } else { (&r, &l) };
            assert!(kept.iter().zip(&x).all(|(a, b)| a.to_bits() == b.to_bits()), "{db}: the kept side");
            let got = 20.0 * (peak(cut) / peak(&x)).log10();
            assert!((got + db.abs()).abs() < 1e-9, "{db}: {got}");
        }
        // Centred, or off: nothing changes.
        assert!(!balanced(0.0, "ms").active());
        assert!(!EqState { on: false, ..balanced(3.0, "") }.active());
        // Clamped to ±30 dB; a NaN is the centre.
        assert_eq!(balanced(55.0, "").balance(), (BalanceMode::LeftRight, 30.0));
        assert_eq!(balanced(f64::NAN, "ms").balance(), (BalanceMode::MidSide, 0.0));
    }

    /// Mid/side: + lowers the mid (a mono tone, l = r), − the side (an
    /// out-of-phase one, l = −r); the other stays. No channel ever grows.
    #[test]
    fn mid_side_balance_lowers_the_mid_or_the_side() {
        let x = sine(48_000, 997.0, 0.7, 4800);
        let neg: Vec<f64> = x.iter().map(|v| -v).collect();
        for db in [2.0, -2.0, 30.0, -30.0] {
            let d = EqDesign::new(&balanced(db, "ms"), 48_000);
            for (right, is_mid) in [(&x, true), (&neg, false)] {
                let (mut l, mut r) = (x.clone(), right.clone());
                EqRun::new().process(&d, &mut l, &mut r);
                let lowered = (db > 0.0) == is_mid;
                let want = if lowered { -db.abs() } else { 0.0 };
                for ch in [&l, &r] {
                    let got = 20.0 * (peak(ch) / peak(&x)).log10();
                    assert!((got - want).abs() < 1e-9, "{db} mid={is_mid}: {got} vs {want}");
                }
            }
            // A full-scale left with silence on the right stays at or under full scale.
            let (mut l, mut r) = (vec![1.0; 8], vec![0.0; 8]);
            EqRun::new().process(&d, &mut l, &mut r);
            assert!(l.iter().chain(&r).all(|v| v.abs() <= 1.0 + 1e-15), "{db}");
        }
    }

    /// The balance rides on the gain after the cascade: the band's curve and
    /// the levels are as without it, the state reads back the same, and a
    /// state without it is written as before.
    #[test]
    fn balance_keeps_the_curve_the_levels_and_old_states() {
        for gain in ["manual", "auto", "safe"] {
            let plain = EqState { gain: gain.into(), output: -2.0, ..bell(6.0) };
            let with = EqState { balance: -4.0, ..plain.clone() };
            let (a, b) = (EqDesign::new(&plain, 48_000), EqDesign::new(&with, 48_000));
            assert_eq!((a.level_db, a.peak_db, a.clip_risk_db()), (b.level_db, b.peak_db, b.clip_risk_db()), "{gain}");
            let x = sine(48_000, 1000.0, 0.25, 4800);
            let (mut al, mut ar, mut bl, mut br) = (x.clone(), x.clone(), x.clone(), x.clone());
            EqRun::new().process(&a, &mut al, &mut ar);
            EqRun::new().process(&b, &mut bl, &mut br);
            assert!(al.iter().zip(&bl).all(|(p, q)| p.to_bits() == q.to_bits()), "{gain}: left kept");
            assert!(ar.iter().zip(&br).all(|(p, q)| (q - p * 10f64.powf(-0.2)).abs() < 1e-15), "{gain}: right −4 dB");
        }
        let st = EqState { balance: 1.25, balance_mode: "ms".into(), ..bell(3.0) };
        let text = serde_json::to_string(&st).unwrap();
        assert!(text.contains(r#""balance":1.25"#) && text.contains(r#""balanceMode":"ms""#), "{text}");
        assert_eq!(serde_json::from_str::<EqState>(&text).unwrap(), st);
        let old = serde_json::to_string(&bell(3.0)).unwrap();
        assert!(!old.contains("balance"), "{old}");
    }

    /// A balance moved while playing glides over the crossfade like a band.
    #[test]
    fn a_moved_balance_glides_without_a_step() {
        let fs = 48_000;
        let x = sine(fs, 1000.0, 0.5, 9600);
        let mut eq = LiveEq::new(fs, 480);
        let (l, r) = live(&mut eq, &x, 480, |i, eq| {
            if i == 0 {
                eq.set(&EqDesign::new(&balanced(0.0, ""), fs));
            }
            if i == 2400 {
                eq.offer(&EqDesign::new(&balanced(-24.0, ""), fs));
            }
        });
        let natural = 0.5 * 2.0 * std::f64::consts::PI * 1000.0 / fs as f64;
        for ch in [&l, &r] {
            let step = ch.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
            assert!(step <= natural * 1.05, "step {step} vs {natural}");
        }
        assert!((peak(&l[x.len() - 2400..]) - 0.5).abs() < 1e-3);
        assert!((peak(&r[x.len() - 2400..]) / 0.5 - 10f64.powf(-24.0 / 20.0)).abs() < 1e-3);
    }

    fn delayed(ms: f64) -> EqState {
        EqState { on: true, delay: ms, ..Default::default() }
    }

    /// The delay reads in 0.01 ms steps within ±30 ms, is off with the EQ,
    /// and lands on one channel in whole frames at the rate.
    #[test]
    fn delay_reads_clamps_and_lands_on_one_channel() {
        assert_eq!(delayed(2.044).delay_ms(), 2.04);
        assert_eq!(delayed(-45.0).delay_ms(), -30.0);
        assert_eq!(delayed(f64::NAN).delay_ms(), 0.0);
        assert!(delayed(0.5).active() && !delayed(0.004).active());
        assert!(!EqState { on: false, ..delayed(2.0) }.active());
        let d = EqDesign::new(&delayed(2.04), 48_000);
        assert!(d.active && d.sections() == 0);
        assert_eq!(d.delay_frames(), [0, 98]);
        assert_eq!(EqDesign::new(&delayed(-30.0), 768_000).delay_frames(), [23040, 0]);
        assert_eq!(EqDesign::new(&EqState { on: false, ..delayed(3.0) }, 48_000).delay_frames(), [0, 0]);
        // Off, the speakers play alone when asked to: no band, no gain.
        let kept = EqState { on: false, speakers_always: true, output: 6.0, balance: 2.0, ..bell(9.0) };
        assert!(kept.active() && !EqState { balance: 0.0, ..kept.clone() }.active());
        let d = EqDesign::new(&EqState { delay: 1.0, ..kept.clone() }, 48_000);
        assert_eq!((d.sections(), d.output_db, d.balance_db, d.delay_frames()), (0, 0.0, 2.0, [0, 48]));
        let on = EqDesign::new(&EqState { on: true, ..kept }, 48_000);
        assert!(on.sections() > 0 && on.output_db == 6.0);
        let text = serde_json::to_string(&EqState { delay: -1.5, ..bell(3.0) }).unwrap();
        assert!(text.contains(r#""delay":-1.5"#), "{text}");
        let plain = serde_json::to_string(&bell(3.0)).unwrap();
        assert!(!plain.contains("delay") && !plain.contains("speakersAlways"), "{plain}");
    }

    /// The converter's tail: the late channel is the input shifted by whole
    /// frames (silence before), the other is left bit for bit, and a block
    /// cut anywhere gives the same samples.
    #[test]
    fn the_tail_delays_one_channel_and_leaves_the_other() {
        let fs = 48_000;
        let x = sine(fs, 997.0, 0.5, 4800);
        let run = |block: usize| {
            let mut t = EqTail::new(Some(&delayed(-1.0)), fs).unwrap();
            let (mut l, mut r) = (x.clone(), x.clone());
            for (bl, br) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
                t.process(bl, br, 1.0);
            }
            (l, r)
        };
        let (l, r) = run(4800);
        assert_eq!(r, x);
        assert!(l[..48].iter().all(|&v| v == 0.0));
        assert_eq!(&l[48..], &x[..x.len() - 48]);
        assert_eq!(run(37), (l, r));
    }

    /// A delay moved while playing fades from the old tap to the new one
    /// without a step, then plays exactly late; back to none, every bit as
    /// it came.
    #[test]
    fn a_moved_delay_glides_and_settles_exactly() {
        let fs = 48_000;
        let x = sine(fs, 1000.0, 0.5, 14_400);
        let mut eq = LiveEq::new(fs, 480);
        let (l, r) = live(&mut eq, &x, 480, |i, eq| {
            if i == 0 {
                eq.set(&EqDesign::new(&delayed(0.5), fs));
            }
            if i == 4800 {
                eq.offer(&EqDesign::new(&delayed(-2.0), fs));
            }
            if i == 9600 {
                eq.offer(&EqDesign::new(&EqState { on: true, ..Default::default() }, fs));
            }
        });
        let natural = 0.5 * 2.0 * std::f64::consts::PI * 1000.0 / fs as f64;
        for ch in [&l, &r] {
            let step = ch[24..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
            assert!(step <= natural * 1.05, "step {step} vs {natural}");
        }
        for i in 7200..9600 {
            assert_eq!(l[i], x[i - 96]);
            assert_eq!(r[i], -x[i]);
        }
        for i in 12_000..x.len() {
            assert_eq!((l[i], r[i]), (x[i], -x[i]));
        }
        assert!(!eq.active());
    }
}
