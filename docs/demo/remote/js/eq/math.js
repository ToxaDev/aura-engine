// eq/math.js — the EQ's magnitude arithmetic, the same the engine runs.
//
// Every band type is an analog prototype (the RBJ cookbook's s-domain
// filters; cuts are Butterworth). The digital sections match that analog
// curve, not the bilinear transform's cramped copy of it, so a treble band
// keeps its shape near Nyquist at 44.1/48 kHz (Vicanek, "Matched Second Order
// Digital Filters", 2016): poles by impulse invariance, zeros from |B|² fixed
// by the analog magnitude at DC and its value and slope at the centre. Cuts
// stay bilinear Butterworth, prewarped at the corner (exact -3 dB there, a
// true zero at Nyquist). Evaluated in f64. Pure: no DOM, so the node tests
// hold it against the engine's golden values, and the window draws exactly
// the curve the player and the converter apply.
//
// A section is [b0, b1, b2, a0, a1, a2] (magnitude is a ratio, a0 cancels).
// Band types are the EqState names (SPEC).

/// The rate the window draws at before the engine names its own.
export const VIEW_FS = 96000;
export const F_MIN = 10;
export const F_MAX = 30000;

export const TYPES = {
    bell: 'Bell', lowShelf: 'Low shelf', highShelf: 'High shelf', tilt: 'Tilt',
    lowCut: 'Low cut', highCut: 'High cut', notch: 'Notch', bandPass: 'Band pass',
};
export const SLOPES = [6, 12, 18, 24, 36, 48, 72, 96];

export const hasGain = t => t === 'bell' || t === 'lowShelf' || t === 'highShelf' || t === 'tilt';
export const isCut = t => t === 'lowCut' || t === 'highCut';

const PI = Math.PI;
/// The highest digital frequency a matched section is fitted at (a band
/// centred above it keeps its analog skirt below Nyquist).
const MATCH_CAP = 0.9 * PI;
/// A cut's corner is held below this fraction of the rate (bilinear).
const CUT_CAP = 0.45;

/// `n` frequencies, log-spaced from `lo` to `hi`.
export function logGrid(n, lo = F_MIN, hi = F_MAX) {
    const L = Math.log(hi / lo), out = new Float64Array(n);
    for (let i = 0; i < n; i++) out[i] = lo * Math.exp(L * i / (n - 1));
    return out;
}

/// The analog prototype, s normalised to the centre: [n2, n1, n0, d2, d1, d0]
/// of (n2 s² + n1 s + n0) / (d2 s² + d1 s + d0).
/// `kind`: bell | lowShelf | highShelf | notch | bandPass | lowPass | highPass.
export function analog(kind, g, q) {
    const A = Math.pow(10, g / 40), sA = Math.sqrt(A);
    switch (kind) {
        case 'bell': return [1, A / q, 1, 1, 1 / (A * q), 1];
        case 'lowShelf': return [A, A * sA / q, A * A, A, sA / q, 1];
        case 'highShelf': return [A * A, A * sA / q, A, 1, sA / q, A];
        case 'notch': return [1, 0, 1, 1, 1 / q, 1];
        case 'bandPass': return [0, 1 / q, 0, 1, 1 / q, 1];
        case 'lowPass': return [0, 0, 1, 1, 1 / q, 1];
        case 'highPass': return [1, 0, 0, 1, 1 / q, 1];
    }
    return [0, 0, 1, 0, 0, 1];
}

/// |H(jx)|² of a prototype at x = F / f, and x·d|H|²/dx (the slope per
/// natural-log frequency).
function amag2(p, x) {
    const x2 = x * x, nr = p[2] - p[0] * x2, ni = p[1] * x, dr = p[5] - p[3] * x2, di = p[4] * x;
    return (nr * nr + ni * ni) / (dr * dr + di * di);
}
function aslope(p, x) {
    const x2 = x * x, nr = p[2] - p[0] * x2, ni = p[1] * x, dr = p[5] - p[3] * x2, di = p[4] * x;
    const N = nr * nr + ni * ni, D = dr * dr + di * di;
    const xdN = -4 * p[0] * x2 * nr + 2 * ni * ni, xdD = -4 * p[3] * x2 * dr + 2 * di * di;
    return (xdN * D - N * xdD) / (D * D);
}

const sq = x => x * x;
const sin2 = x => sq(Math.sin(x));

/// The poles of prototype `p` at digital centre `w0`, by impulse invariance:
/// a1, a2, |A(1)|, |A(-1)| and |A(e^jw)|², written so nothing cancels at low
/// frequencies (1 - r by expm1, 1 - cos by sin²).
function poles(p, w0) {
    const wp = w0 * Math.sqrt(p[5] / p[3]), z = p[4] / (2 * Math.sqrt(p[3] * p[5]));
    if (z < 1) {
        const th = Math.min(wp * Math.sqrt(1 - z * z), PI), r = Math.exp(-z * wp), m = -Math.expm1(-z * wp);
        return {
            a1: -2 * r * Math.cos(th), a2: r * r, d: z * wp,
            s0: m * m + 4 * r * sin2(th / 2), s1: m * m + 4 * r * sin2((PI - th) / 2),
            den: w => (m * m + 4 * r * sin2((w - th) / 2)) * (m * m + 4 * r * sin2((w + th) / 2)),
        };
    }
    const k = Math.sqrt(z * z - 1), sa = wp / (z + k), sb = wp * (z + k);   // sa = wp (z - k), no cancellation
    const ra = Math.exp(-sa), rb = Math.exp(-sb), ma = -Math.expm1(-sa), mb = -Math.expm1(-sb);
    return {
        a1: -(ra + rb), a2: ra * rb, d: sa,
        s0: ma * mb, s1: (1 + ra) * (1 + rb),
        den: w => (ma * ma + 4 * ra * sin2(w / 2)) * (mb * mb + 4 * rb * sin2(w / 2)),
    };
}

/// A minimum-phase numerator from |B(e^jw)|² = B0 cos²(w/2) + B1 sin²(w/2) + B2 sin²w,
/// or null when no real numerator has it.
function numerator(B0, B1, B2) {
    if (!(B0 >= 0) || !(B1 >= 0)) return null;
    const s0 = Math.sqrt(B0), s1 = Math.sqrt(B1), W = (s0 + s1) / 2, d = W * W + B2;
    if (!(d >= 0)) return null;
    const b0 = (W + Math.sqrt(d)) / 2;
    return [b0, (s0 - s1) / 2, b0 > 0 ? -B2 / (4 * b0) : 0];
}

/// The numerator with its zeros held at radius ≤ R and its DC gain kept:
/// an inverted section's poles stay inside the circle.
function minPhase(b0, b1, b2, R) {
    const d = b1 * b1 - 4 * b0 * b2;
    let c;
    if (d < 0) {
        const r2 = b2 / b0;
        if (r2 <= R * R) return [b0, b1, b2];
        const k = R / Math.sqrt(r2);
        c = [b0, b1 * k, R * R * b0];
    } else {
        const cl = z => Math.min(R, Math.max(-R, z)), sd = Math.sqrt(d);
        const z1 = (-b1 + (b1 <= 0 ? sd : -sd)) / (2 * b0), z2 = z1 !== 0 ? b2 / (b0 * z1) : 0;
        if (Math.abs(z1) <= R && Math.abs(z2) <= R) return [b0, b1, b2];
        c = [b0, -b0 * (cl(z1) + cl(z2)), b0 * cl(z1) * cl(z2)];
    }
    const s = c[0] + c[1] + c[2], k = s !== 0 ? (b0 + b1 + b2) / s : 1;
    return [c[0] * k, c[1] * k, c[2] * k];
}

/// One analog-matched second-order section: `kind` at `f` Hz, gain `g` dB,
/// quality `q`, rate `fs`. bell | lowShelf | highShelf | notch | bandPass.
export function matched(kind, f, g, q, fs) {
    const p = analog(kind, g, q);
    // Bells and shelves are the exact inverse of their opposite gain. A cut
    // bell is designed as the boost and inverted: the boost's resonant poles
    // map exactly and become the cut's zeros, where its dip is. So is a shelf
    // whose poles lie above its zeros (low-shelf cut, high-shelf boost): its
    // poles would sit too high to map without aliasing.
    if ((kind === 'bell' && g < 0) || ((kind === 'lowShelf' || kind === 'highShelf') && p[5] * p[0] > p[2] * p[3])) {
        // Its zeros may come no closer to the circle than 1/16 of the distance of
        // the analog zeros mapped by impulse invariance.
        const c = matched(kind, f, -g, q, fs), pz = poles([0, 0, 0, ...analog(kind, -g, q).slice(0, 3)], Math.min(2 * PI * f / fs, PI));
        const b = minPhase(c[0], c[1], c[2], Math.exp(-pz.d / 16));
        return [1, c[4], c[5], b[0], b[1], b[2]];
    }
    const w0 = 2 * PI * f / fs, P = poles(p, w0);
    if (kind === 'notch') {     // zeros on the circle at the centre, DC gain 1
        const wn = Math.min(w0, PI), k = P.s0 / (4 * sin2(wn / 2));
        return [k, -2 * Math.cos(wn) * k, k, 1, P.a1, P.a2];
    }
    // A gentle shelf is fitted at its centre, a resonant one (q ≥ 2) at its
    // zeros' frequency, where its peak or dip sits; between, the point moves
    // smoothly with log q. A bell's zeros are at its centre.
    const ze = kind === 'bell' || kind === 'bandPass' ? 0 : 0.5 * Math.min(1, Math.max(0, Math.log2(q)));
    const wm = Math.min(w0 * Math.pow(p[2] / p[0], ze), MATCH_CAP), xm = wm / w0;
    const f1 = sin2(wm / 2), c = Math.cos(wm);
    const A0 = P.s0 * P.s0, A1 = P.s1 * P.s1, A2 = -4 * P.a2, Am = P.den(wm);
    const H0 = amag2(p, 0), G = amag2(p, xm), S = aslope(p, xm);
    // |B|² = |H|²·|A|² at DC, and its value and slope (in sin²(w/2)) at wm.
    // The closed form keeps the small terms apart: B2 has no 1/f1² of a difference.
    const B0 = H0 * A0;
    const B2 = (A0 * (G - H0) - S * Math.tan(wm / 2) / wm * Am) / (4 * f1 * f1) + G * A2;
    const R2 = G * (A1 - A0 + 4 * A2 * c) + 2 * S / (wm * Math.sin(wm)) * Am;
    let b = numerator(B0, R2 + B0 - 4 * B2 * c, B2);
    if (!b) {                   // the slope cannot be met: match the analog value at Nyquist instead
        const B1 = amag2(p, PI / w0) * A1, B2n = (G * Am - B0 * (1 - f1) - B1 * f1) / (4 * (1 - f1) * f1);
        b = numerator(B0, B1, B2n) || numerator(B0, B1, Math.max(B2n, -sq((Math.sqrt(B0) + Math.sqrt(B1)) / 2)));
    }
    return [b[0], b[1], b[2], 1, P.a1, P.a2];
}

/// A cut's bilinear second-order section (Butterworth pair of quality `q`),
/// prewarped at `f`: `lp` true = low pass (high cut).
function cutPair(lp, f, q, fs) {
    const w = 2 * PI * Math.min(f, fs * CUT_CAP) / fs, s = Math.sin(w), al = s / (2 * q);
    const c = Math.cos(w), lo = 2 * sin2(w / 2), hi = 2 - lo;    // 1 - cos, 1 + cos
    return lp ? [lo / 2, lo, lo / 2, 1 + al, -2 * c, 1 - al] : [hi / 2, -hi, hi / 2, 1 + al, -2 * c, 1 - al];
}

/// A cut's first-order bilinear section (the odd stage of an odd order).
function cutFirst(lp, f, fs) {
    const K = Math.tan(PI * Math.min(f, fs * CUT_CAP) / fs);
    return lp ? [K, K, 0, 1 + K, K - 1, 0] : [1, -1, 0, 1 + K, K - 1, 0];
}

/// A band held to the contract's ranges, as the engine holds it: f 10..24000,
/// gain ±30, q 0.1..24, slope the nearest step.
export function clampBand(b) {
    const cl = (v, a, z) => Math.min(z, Math.max(a, v));
    const s = typeof b.slope === 'number' ? b.slope : 12;
    const slope = SLOPES.reduce((best, x) => (Math.abs(x - s) < Math.abs(best - s) ? x : best), SLOPES[0]);
    return { ...b, f: cl(b.f, 10, 24000), gain: cl(b.gain ?? 0, -30, 30), q: cl(b.q ?? 0.71, 0.1, 24), slope };
}

/// The Butterworth pairs of a cut of order `n`: their qualities, the band's
/// q reshaping the last pair (q/0.7071 times).
function cutQs(n, q) {
    const pairs = Math.floor(n / 2), out = [];
    for (let k = 0; k < pairs; k++) {
        const th = n % 2 ? PI * (k + 1) / n : PI * (2 * k + 1) / (2 * n);
        out.push(1 / (2 * Math.cos(th)) * (k === pairs - 1 ? q / Math.SQRT1_2 : 1));
    }
    return out;
}

/// The biquad sections of one band at rate `fs`. A cut is a Butterworth
/// cascade of order slope/6; tilt is two shelves of half the gain.
export function sections(b, fs = VIEW_FS) {
    b = clampBand(b);
    if (b.type === 'tilt') return [matched('highShelf', b.f, b.gain / 2, 0.6, fs), matched('lowShelf', b.f, -b.gain / 2, 0.6, fs)];
    if (!isCut(b.type)) return [matched(b.type, b.f, b.gain, b.q, fs)];
    const lp = b.type === 'highCut', n = Math.round(b.slope / 6), out = [];
    if (n % 2) out.push(cutFirst(lp, b.f, fs));
    for (const q of cutQs(n, b.q)) out.push(cutPair(lp, b.f, q, fs));
    return out;
}

/// The band's analog curve at `fr` Hz, in dB: the reference its sections
/// are tested against (the sections are what the engine runs).
export function analogDb(b, fr) {
    b = clampBand(b);
    const x = fr / b.f, db = (kind, g, q) => 10 * Math.log10(Math.max(1e-30, amag2(analog(kind, g, q), x)));
    if (b.type === 'tilt') return db('highShelf', b.gain / 2, 0.6) + db('lowShelf', -b.gain / 2, 0.6);
    if (!isCut(b.type)) return db(b.type, b.gain, b.q);
    const lp = b.type === 'highCut', n = Math.round(b.slope / 6);
    let d = n % 2 ? 10 * Math.log10(lp ? 1 / (1 + x * x) : x * x / (1 + x * x)) : 0;
    for (const q of cutQs(n, b.q)) d += db(lp ? 'lowPass' : 'highPass', 0, q);
    return d;
}

/// |H(f)| of one section, in dB.
export function magDb(co, f, fs = VIEW_FS) {
    const w = 2 * PI * f / fs, c1 = Math.cos(w), s1 = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w);
    const nr = co[0] + co[1] * c1 + co[2] * c2, ni = -(co[1] * s1 + co[2] * s2);
    const dr = co[3] + co[4] * c1 + co[5] * c2, di = -(co[4] * s1 + co[5] * s2);
    return 10 * Math.log10(Math.max(1e-30, (nr * nr + ni * ni) / (dr * dr + di * di)));
}

/// One band's curve over `freqs`, in dB.
export function bandCurve(b, freqs, fs = VIEW_FS) {
    const secs = sections(b, fs), out = new Float64Array(freqs.length);
    for (let i = 0; i < freqs.length; i++) {
        let d = 0;
        for (const co of secs) d += magDb(co, freqs[i], fs);
        out[i] = d;
    }
    return out;
}

/// The summed curve of the bands that are on (no output gain).
export function totalCurve(bands, freqs, fs = VIEW_FS) {
    const t = new Float64Array(freqs.length);
    for (const b of bands) {
        if (!b.on) continue;
        const c = bandCurve(b, freqs, fs);
        for (let i = 0; i < t.length; i++) t[i] += c[i];
    }
    return t;
}

const AUTO_GRID = logGrid(480, 20, 20000);

/// Safe level: minus the peak of the summed curve over 20 Hz–20 kHz on a
/// dense log grid, never a boost.
export function autoPreamp(bands, fs = VIEW_FS) {
    let m = -Infinity;
    const t = totalCurve(bands, AUTO_GRID, fs);
    for (let i = 0; i < t.length; i++) m = Math.max(m, t[i]);
    return m > 0 ? -m : 0;
}

/// What the safe level keeps below the peak boost, dB: a loud master's
/// sample peaks grow a little more than the curve's peak (measured: up to
/// +0.2 dB with hard-clipped pink noise through 6–12 dB boosts).
export const SAFE_MARGIN_DB = 0.5;

/// Safe level: minus the peak boost and the margin; 0 with no boost (a
/// rounding residue of a 0 dB band is none).
export function safeLevel(bands, fs = VIEW_FS) {
    const p = autoPreamp(bands, fs);
    return p < -1e-9 ? p - SAFE_MARGIN_DB : 0;
}

// BS.1770 K-weighting (its 48 kHz sections): the head's high shelf and the
// low cut. Weight per grid point = |K|²; the log grid itself weighs every
// octave alike, as pink noise (the long-term spectrum of music) does.
const K_SECS = [
    [1.53512485958697, -2.69169618940638, 1.19839281085285, 1, -1.69065929318241, 0.73248077421585],
    [1, -2, 1, 1, -1.99004745483398, 0.99007225036621],
];
const K_W = AUTO_GRID.map(f => 10 ** ((magDb(K_SECS[0], f, 48000) + magDb(K_SECS[1], f, 48000)) / 10));
const K_SUM = K_W.reduce((a, w) => a + w, 0);

/// Auto Gain: the level at which the EQ sounds as loud as without it —
/// minus the K-weighted power average of the curve over 20 Hz–20 kHz,
/// clamped to ±30 dB.
export function autoGain(bands, fs = VIEW_FS) {
    const t = totalCurve(bands, AUTO_GRID, fs);
    let p = 0;
    for (let i = 0; i < t.length; i++) p += K_W[i] * 10 ** (t[i] / 10);
    return Math.max(-30, Math.min(30, -10 * Math.log10(p / K_SUM))) || 0;
}

/// The level the state's gain mode sets before its output gain, dB:
/// manual 0, auto `autoGain`, safe `safeLevel`. The state as `state.js`
/// normalizes it (a version-1 preamp already migrated).
export function modeLevel(state, fs = VIEW_FS) {
    const bands = (state.bands || []).filter(b => b.on);
    if (state.gain === 'auto') return autoGain(bands, fs);
    if (state.gain === 'safe') return safeLevel(bands, fs);
    return 0;
}
