// Writes eq-golden.json next to it: magnitudes, the gain modes' levels
// (safe, Auto Gain, a version-1 state migrated) and the eqActive/eqSig
// rule of the EQ, from the reference math of the EQ window
// (analog-matched sections of the RBJ cookbook's analog prototypes,
// bilinear Butterworth cuts), and the analog curve each band is matched to.
// The engine (dsp/eq.rs) and the window's JS are both tested against these
// numbers.
//   node tests/fixtures/eq-golden.gen.mjs
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SLOPES = [6, 12, 18, 24, 36, 48, 72, 96];
const GAIN_TYPES = ['bell', 'lowShelf', 'highShelf', 'tilt'];
const CUT_TYPES = ['lowCut', 'highCut'];
const PI = Math.PI;
const MATCH_CAP = 0.9 * PI;
const CUT_CAP = 0.45;
const sq = x => x * x;
const sin2 = x => sq(Math.sin(x));

/// Analog prototypes [n2, n1, n0, d2, d1, d0], s normalised to the centre.
function analog(kind, g, q) {
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
    throw new Error(kind);
}
function amag2(p, x) {
    const x2 = x * x, nr = p[2] - p[0] * x2, ni = p[1] * x, dr = p[5] - p[3] * x2, di = p[4] * x;
    return (nr * nr + ni * ni) / (dr * dr + di * di);
}
function aslope(p, x) {
    const x2 = x * x, nr = p[2] - p[0] * x2, ni = p[1] * x, dr = p[5] - p[3] * x2, di = p[4] * x;
    const N = nr * nr + ni * ni, D = dr * dr + di * di;
    return ((-4 * p[0] * x2 * nr + 2 * ni * ni) * D - N * (-4 * p[3] * x2 * dr + 2 * di * di)) / (D * D);
}
/// Poles of d2 s² + d1 s + d0 at w0 by impulse invariance.
function poles(d2, d1, d0, w0) {
    const wp = w0 * Math.sqrt(d0 / d2), z = d1 / (2 * Math.sqrt(d2 * d0));
    if (z < 1) {
        const th = Math.min(wp * Math.sqrt(1 - z * z), PI), r = Math.exp(-z * wp), m = -Math.expm1(-z * wp);
        return {
            a1: -2 * r * Math.cos(th), a2: r * r, d: z * wp,
            s0: m * m + 4 * r * sin2(th / 2), s1: m * m + 4 * r * sin2((PI - th) / 2),
            den: w => (m * m + 4 * r * sin2((w - th) / 2)) * (m * m + 4 * r * sin2((w + th) / 2)),
        };
    }
    const k = Math.sqrt(z * z - 1), sa = wp / (z + k), sb = wp * (z + k);
    const ra = Math.exp(-sa), rb = Math.exp(-sb), ma = -Math.expm1(-sa), mb = -Math.expm1(-sb);
    return {
        a1: -(ra + rb), a2: ra * rb, d: sa, s0: ma * mb, s1: (1 + ra) * (1 + rb),
        den: w => (ma * ma + 4 * ra * sin2(w / 2)) * (mb * mb + 4 * rb * sin2(w / 2)),
    };
}
function numerator(B0, B1, B2) {
    if (!(B0 >= 0) || !(B1 >= 0)) return null;
    const s0 = Math.sqrt(B0), s1 = Math.sqrt(B1), W = (s0 + s1) / 2, d = W * W + B2;
    if (!(d >= 0)) return null;
    const b0 = (W + Math.sqrt(d)) / 2;
    return [b0, (s0 - s1) / 2, b0 > 0 ? -B2 / (4 * b0) : 0];
}
function minPhase(b0, b1, b2, R) {
    const d = b1 * b1 - 4 * b0 * b2;
    let c;
    if (d < 0) {
        const r2 = b2 / b0;
        if (r2 <= R * R) return [b0, b1, b2];
        c = [b0, b1 * R / Math.sqrt(r2), R * R * b0];
    } else {
        const cl = z => Math.min(R, Math.max(-R, z)), sd = Math.sqrt(d);
        const z1 = (-b1 + (b1 <= 0 ? sd : -sd)) / (2 * b0), z2 = z1 !== 0 ? b2 / (b0 * z1) : 0;
        if (Math.abs(z1) <= R && Math.abs(z2) <= R) return [b0, b1, b2];
        c = [b0, -b0 * (cl(z1) + cl(z2)), b0 * cl(z1) * cl(z2)];
    }
    const s = c[0] + c[1] + c[2], k = s !== 0 ? (b0 + b1 + b2) / s : 1;
    return [c[0] * k, c[1] * k, c[2] * k];
}
function matched(kind, f, g, q, fs) {
    const p = analog(kind, g, q), shelf = kind === 'lowShelf' || kind === 'highShelf';
    if ((kind === 'bell' && g < 0) || (shelf && p[5] * p[0] > p[2] * p[3])) {
        const c = matched(kind, f, -g, q, fs), n = analog(kind, -g, q);
        const pz = poles(n[0], n[1], n[2], Math.min(2 * PI * f / fs, PI));
        const b = minPhase(c[0], c[1], c[2], Math.exp(-pz.d / 16));
        return [1, c[4], c[5], b[0], b[1], b[2]];
    }
    const w0 = 2 * PI * f / fs, P = poles(p[3], p[4], p[5], w0);
    if (kind === 'notch') {
        const wn = Math.min(w0, PI), k = P.s0 / (4 * sin2(wn / 2));
        return [k, -2 * Math.cos(wn) * k, k, 1, P.a1, P.a2];
    }
    const ze = shelf ? 0.5 * Math.min(1, Math.max(0, Math.log2(q))) : 0;
    const wm = Math.min(w0 * Math.pow(p[2] / p[0], ze), MATCH_CAP), xm = wm / w0;
    const f1 = sin2(wm / 2), c = Math.cos(wm);
    const A0 = P.s0 * P.s0, A1 = P.s1 * P.s1, A2 = -4 * P.a2, Am = P.den(wm);
    const H0 = amag2(p, 0), G = amag2(p, xm), S = aslope(p, xm);
    const B0 = H0 * A0;
    const B2 = (A0 * (G - H0) - S * Math.tan(wm / 2) / wm * Am) / (4 * f1 * f1) + G * A2;
    const R2 = G * (A1 - A0 + 4 * A2 * c) + 2 * S / (wm * Math.sin(wm)) * Am;
    let b = numerator(B0, R2 + B0 - 4 * B2 * c, B2);
    if (!b) {
        const B1 = amag2(p, PI / w0) * A1, B2n = (G * Am - B0 * (1 - f1) - B1 * f1) / (4 * (1 - f1) * f1);
        b = numerator(B0, B1, B2n) || numerator(B0, B1, Math.max(B2n, -sq((Math.sqrt(B0) + Math.sqrt(B1)) / 2)));
    }
    return [b[0], b[1], b[2], 1, P.a1, P.a2];
}
function cutPair(lp, f, q, fs) {
    const w = 2 * PI * Math.min(f, fs * CUT_CAP) / fs, al = Math.sin(w) / (2 * q), c = Math.cos(w);
    const lo = 2 * sin2(w / 2), hi = 2 - lo;
    return lp ? [lo / 2, lo, lo / 2, 1 + al, -2 * c, 1 - al] : [hi / 2, -hi, hi / 2, 1 + al, -2 * c, 1 - al];
}
function cutFirst(lp, f, fs) {
    const K = Math.tan(PI * Math.min(f, fs * CUT_CAP) / fs);
    return lp ? [K, K, 0, 1 + K, K - 1, 0] : [1, -1, 0, 1 + K, K - 1, 0];
}
/// The clamps of the contract (SPEC "EqState").
function clampBand(b) {
    const cl = (v, a, z) => Math.min(z, Math.max(a, v));
    const slope = SLOPES.reduce((best, s) => Math.abs(s - b.slope) < Math.abs(best - b.slope) ? s : best, SLOPES[0]);
    return { ...b, f: cl(b.f, 10, 24000), gain: cl(b.gain ?? 0, -30, 30), q: cl(b.q ?? 0.71, 0.1, 24), slope };
}
/// Whether a band changes the sound at all (a gain band at 0 dB does not).
function bandActive(b) {
    if (b.on === false) return false;
    return GAIN_TYPES.includes(b.type) ? Math.abs(b.gain) >= 0.005 : true;
}
function cutQs(n, q) {
    const pairs = Math.floor(n / 2), out = [];
    for (let k = 0; k < pairs; k++) {
        const th = n % 2 ? PI * (k + 1) / n : PI * (2 * k + 1) / (2 * n);
        out.push(1 / (2 * Math.cos(th)) * (k === pairs - 1 ? q / Math.SQRT1_2 : 1));
    }
    return out;
}
function sections(fs, b) {
    b = clampBand(b);
    if (b.type === 'tilt') return [matched('highShelf', b.f, b.gain / 2, 0.6, fs), matched('lowShelf', b.f, -b.gain / 2, 0.6, fs)];
    if (!CUT_TYPES.includes(b.type)) return [matched(b.type, b.f, b.gain, b.q, fs)];
    const lp = b.type === 'highCut', N = Math.round(b.slope / 6), out = [];
    if (N % 2) out.push(cutFirst(lp, b.f, fs));
    for (const q of cutQs(N, b.q)) out.push(cutPair(lp, b.f, q, fs));
    return out;
}
/// The analog curve the band is matched to, dB.
function analogDb(b, fr) {
    b = clampBand(b);
    const x = fr / b.f, db = (kind, g, q) => 10 * Math.log10(Math.max(1e-30, amag2(analog(kind, g, q), x)));
    if (b.type === 'tilt') return db('highShelf', b.gain / 2, 0.6) + db('lowShelf', -b.gain / 2, 0.6);
    if (!CUT_TYPES.includes(b.type)) return db(b.type, b.gain, b.q);
    const lp = b.type === 'highCut', n = Math.round(b.slope / 6);
    let d = n % 2 ? 10 * Math.log10(lp ? 1 / (1 + x * x) : x * x / (1 + x * x)) : 0;
    for (const q of cutQs(n, b.q)) d += db(lp ? 'lowPass' : 'highPass', 0, q);
    return d;
}
function magDb(fs, co, f) {
    const w = 2 * Math.PI * f / fs, c1 = Math.cos(w), s1 = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w);
    const nr = co[0] + co[1] * c1 + co[2] * c2, ni = -(co[1] * s1 + co[2] * s2), dr = co[3] + co[4] * c1 + co[5] * c2, di = -(co[4] * s1 + co[5] * s2);
    return 10 * Math.log10(Math.max(1e-30, (nr * nr + ni * ni) / (dr * dr + di * di)));
}
const bandDb = (fs, b, f) => sections(fs, b).reduce((a, co) => a + magDb(fs, co, f), 0);

/// The level grid: 480 points, log-spaced, 20 Hz to 20 kHz inclusive.
const GRID = Array.from({ length: 480 }, (_, i) => 20 * Math.pow(1000, i / 479));
function activeBands(state) { return state.bands.slice(0, 24).filter(bandActive).map(clampBand); }
function autoPreampDb(fs, state) {
    const bands = activeBands(state);
    let m = -Infinity;
    for (const f of GRID) m = Math.max(m, bands.reduce((a, b) => a + bandDb(fs, b, f), 0));
    return -Math.max(0, m);
}
const SAFE_MARGIN_DB = 0.5;
function safeDb(fs, state) {
    const p = autoPreampDb(fs, state);
    return p < 0 ? p - SAFE_MARGIN_DB : 0;
}
// BS.1770 K-weighting at 48 kHz: |K|² per grid point.
const K_W = GRID.map(f => 10 ** ((magDb(48000, [1.53512485958697, -2.69169618940638, 1.19839281085285, 1, -1.69065929318241, 0.73248077421585], f)
    + magDb(48000, [1, -2, 1, 1, -1.99004745483398, 0.99007225036621], f)) / 10));
function autoGainDb(fs, state) {
    const bands = activeBands(state);
    let p = 0, w = 0;
    GRID.forEach((f, i) => { p += K_W[i] * 10 ** (bands.reduce((a, b) => a + bandDb(fs, b, f), 0) / 10); w += K_W[i]; });
    return Math.max(-30, Math.min(30, -10 * Math.log10(p / w)));
}
const cl = (x, lo, hi) => Math.max(lo, Math.min(hi, x));
/// Gain mode and output: a valid `gain` wins; a version-1 manual preamp
/// goes into the output (manual); a version-1 auto preamp becomes safe.
function levelOf(s) {
    const out = typeof s.output === 'number' && isFinite(s.output) ? s.output : 0;
    if (['manual', 'auto', 'safe'].includes(s.gain)) return { gain: s.gain, output: cl(out, -30, 30) };
    if (typeof s.preamp === 'number' && isFinite(s.preamp)) return { gain: 'manual', output: cl(out + cl(s.preamp, -30, 0), -30, 30) };
    return { gain: s.v >= 2 ? 'manual' : 'safe', output: cl(out, -30, 30) };
}
function levelDb(fs, state) {
    const { gain } = levelOf(state);
    return gain === 'auto' ? autoGainDb(fs, state) : gain === 'safe' ? safeDb(fs, state) : 0;
}
/// The balance, dB: ±30, rounded to 0.01 dB (as Math.round rounds).
function balanceOf(s) {
    const b = cl(Math.round((typeof s.balance === 'number' && isFinite(s.balance) ? s.balance : 0) * 100) / 100, -30, 30);
    return { balance: b === 0 ? 0 : b, balanceMode: s.balanceMode === 'ms' ? 'ms' : 'lr' };
}
function eqActive(state) {
    if (!state.on) return false;
    return activeBands(state).length > 0 || levelOf(state).output !== 0 || balanceOf(state).balance !== 0;
}
function eqSig(state) {
    if (!eqActive(state)) return null;
    const { gain, output } = levelOf(state);
    const { balance, balanceMode } = balanceOf(state);
    const sig = [gain, output, activeBands(state).map(b => [b.type, b.f, b.gain, b.q, b.slope])];
    return JSON.stringify(balance !== 0 ? [...sig, balanceMode, balance] : sig);
}

const FREQS = [10, 20, 31.5, 50, 100, 200, 500, 1000, 2000, 3150, 5000, 8000, 10000, 12500, 16000, 18000, 20000];
const FS = [44100, 48000, 88200, 96000, 176400, 352800, 768000];
const band = (type, f, gain, q, slope = 12) => ({ type, f, gain, q, slope, on: true });
const BANDS = [
    band('bell', 1000, 6, 1.0),
    band('bell', 60, -9, 4),
    band('bell', 12000, 4.5, 0.5),
    band('bell', 16000, 6, 1.0),
    band('bell', 18000, -8, 2),
    band('bell', 22000, 9, 0.7),
    band('bell', 24000, -12, 0.4),
    band('bell', 20, 12, 3),
    band('lowShelf', 105, 6.5, 0.7),
    band('lowShelf', 300, -6, 3),
    band('highShelf', 8000, -4, 0.71),
    band('highShelf', 16000, 6, 0.71),
    band('highShelf', 20000, -9, 2),
    band('tilt', 1000, 3, 0.71),
    band('tilt', 3000, -8, 0.71),
    band('lowCut', 30, 0, 0.71, 6),
    band('lowCut', 30, 0, 0.71, 12),
    band('lowCut', 30, 0, 1.2, 18),
    band('lowCut', 80, 0, 0.71, 24),
    band('lowCut', 20, 0, 0.5, 96),
    band('lowCut', 10, 0, 0.71, 96),
    band('highCut', 15000, 0, 0.71, 48),
    band('highCut', 5000, 0, 0.71, 6),
    band('highCut', 30000, 0, 0.71, 12),
    band('notch', 3000, 0, 5),
    band('notch', 17000, 0, 2),
    band('bandPass', 500, 0, 2),
    band('bandPass', 16000, 0, 4),
];
const magnitudes = [];
for (const fs of FS) for (const b of BANDS) {
    magnitudes.push({ fs, band: b, freqs: FREQS, db: FREQS.map(f => bandDb(fs, b, f)), analogDb: FREQS.map(f => analogDb(b, f)) });
}

const st = (bands, extra = {}) => ({ v: 2, on: true, name: 'Test', bands, gain: 'manual', output: 0, ...extra });
const v1 = (bands, extra = {}) => ({ v: 1, on: true, name: 'Old', bands, preamp: null, output: 0, ...extra });
const B3 = [band('lowShelf', 105, 6.5, 0.7), band('bell', 3000, 4, 2), band('lowCut', 20, 0, 0.71, 24)];
const STATES = [
    st(B3, { gain: 'auto' }),
    st(B3, { gain: 'safe', output: 1.5 }),
    st([band('bell', 60, -9, 4), band('highShelf', 8000, -4, 0.71)], { gain: 'auto', output: -2 }),
    st([band('bell', 1000, 6, 1.0), band('bell', 100, -3, 0.7)], { gain: 'safe' }),
    st([band('bell', 1000, 30, 0.1), band('lowShelf', 200, 30, 0.3)], { gain: 'auto' }),
    st([], { gain: 'auto', output: 2 }),
    st([], { output: 40 }),
    st([band('bell', 1000, 6, 1.0)], { gain: 'x' }),
    { v: 2, on: true, name: 'NoGain', bands: [band('bell', 1000, 6, 1.0)], output: 0 },
    v1(B3),
    v1(B3, { preamp: -2, output: 1 }),
    v1([band('bell', 1000, 6, 1.0)], { preamp: 5, output: -3 }),
    v1(B3, { gain: 'auto' }),
    v1([band('bell', 1000, 6, 1.0)], { preamp: -50, output: -20 }),
    { on: true, name: 'NoVersion', bands: [band('tilt', 1000, 6, 0.71)] },
    v1([], { preamp: -2 }),
    v1([], { preamp: 0 }),
    st([band('bell', 1000, 6, 1.0)]),
    st([band('bell', 60, -9, 4), band('highShelf', 8000, -4, 0.71)]),
    st([band('lowShelf', 105, 6.5, 0.7), band('bell', 3000, 4, 2), band('lowCut', 20, 0, 0.71, 24)]),
    st([band('tilt', 1000, 6, 0.71)]),
    st([band('bell', 1000, 0, 1.0)]),
    st([band('bell', 1000, 6, 1.0)], { on: false }),
    st([{ ...band('bell', 1000, 6, 1.0), on: false }]),
    st([], { output: -3 }),
    st([], { preamp: -2 }),
    st([], { preamp: 0 }),
    st([band('notch', 3000, 0, 5)]),
    st([band('bell', 1000, 40, 1.0), band('lowCut', 5, 0, 30, 20)]),
    st([band('highShelf', 16000, 6, 0.71), band('bell', 19000, 4, 1)]),
    st([], { balance: 1.5 }),
    st([], { balance: -0.004 }),
    st([], { balance: 2.5, balanceMode: 'ms' }),
    st([], { balance: 0, balanceMode: 'ms' }),
    st(B3, { gain: 'safe', balance: -3.333 }),
    st([band('bell', 1000, 6, 1.0)], { on: false, balance: 4 }),
];
const levels = [];
for (const fs of FS) for (const s of STATES) {
    levels.push({ fs, state: s, ...levelOf(s), levelDb: levelDb(fs, s), autoPreampDb: autoPreampDb(fs, s), safeDb: safeDb(fs, s), autoGainDb: autoGainDb(fs, s) });
}
const active = STATES.map(s => ({ state: s, active: eqActive(s), sig: eqSig(s) }));

const out = {
    note: 'Generated by eq-golden.gen.mjs from the reference math. Magnitudes in dB, sum of the sections in dB; '
        + 'analogDb = the analog curve the sections are matched to. '
        + 'Level grid (safe = peak + ' + SAFE_MARGIN_DB + ' dB margin, auto = K-weighted power average): 480 log-spaced points 20 Hz..20 kHz inclusive. Clamps: f 10..24000 (a cut\'s corner then 0.45 fs), '
        + 'gain -30..30, q 0.1..24, slope to the nearest of ' + SLOPES.join('/') + '.',
    magnitudes, levels, active,
};
writeFileSync(fileURLToPath(new URL('./eq-golden.json', import.meta.url)), JSON.stringify(out, null, 1) + '\n');
console.log(`magnitudes ${magnitudes.length}, levels ${levels.length}, active ${active.length}`);
