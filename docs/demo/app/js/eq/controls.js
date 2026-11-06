// eq/controls.js — the editing arithmetic of the EQ graph, without the DOM.
//
// Pure functions the graph and the band panel share and the node tests hold:
// typed values ("2k", "A4", "C#2+13"), the band a click would add, hit tests,
// the group move, the dB range that grows while dragging, the frequency view.

import { F_MIN, F_MAX, isCut } from './math.js';

/// The editable ranges (the contract's, as state.js holds them).
export const F_LO = 10, F_HI = 24000, G_MAX = 30, Q_LO = 0.1, Q_HI = 24;
/// The dB scales the graph offers, smallest first.
export const RANGES = [3, 6, 12, 30];

const clamp = (v, a, b) => Math.min(b, Math.max(a, v));

const NOTE_PC = { c: 0, d: 2, e: 4, f: 5, g: 7, a: 9, b: 11 };
const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];

/// A typed frequency in Hz, or null: "1000", "2k", "1.5k", "1,5 kHz", "440hz",
/// or a note with an optional cents offset: "A4" = 440, "C#2+13", "Bb3-20".
export function parseFreq(text) {
    const s = String(text ?? '').trim().toLowerCase().replace(',', '.').replace(/\s+/g, '');
    if (!s) return null;
    const note = /^([a-g])([#b]?)(-?\d)([+-]\d+(?:\.\d+)?)?(?:c|ct|cents?)?$/.exec(s);
    if (note) {
        const pc = NOTE_PC[note[1]] + (note[2] === '#' ? 1 : note[2] === 'b' ? -1 : 0);
        const midi = 12 * (+note[3] + 1) + pc;
        const cents = note[4] ? +note[4] : 0;
        return 440 * Math.pow(2, (midi - 69 + cents / 100) / 12);
    }
    const hz = /^(\d+(?:\.\d*)?|\.\d+)(k|khz|hz)?$/.exec(s);
    if (!hz) return null;
    const v = +hz[1] * (hz[2] === 'k' || hz[2] === 'khz' ? 1000 : 1);
    return isFinite(v) && v > 0 ? v : null;
}

/// A typed number ("+3", "-1.5 dB", "0,7"), or null.
export function parseNum(text) {
    const s = String(text ?? '').trim().toLowerCase().replace('−', '-').replace(',', '.').replace(/(db|q)$/, '').trim();
    if (!/^[+-]?(\d+(\.\d*)?|\.\d+)$/.test(s)) return null;
    return +s;
}

/// The balance as the field shows it: "C" at the centre, else the side it
/// leans to and its dB — "R 1.5 dB", "L 0.25 dB" (M/S for mid/side).
export function fmtBalance(db, mode) {
    if (!db) return 'C';
    const side = mode === 'ms' ? (db > 0 ? 'S' : 'M') : (db > 0 ? 'R' : 'L');
    const a = Math.abs(db);
    return side + ' ' + (Math.abs(a * 10 - Math.round(a * 10)) < 1e-9 ? a.toFixed(1) : a.toFixed(2)) + ' dB';
}

/// A typed balance in dB (+ = right or side), or null: "C", "0", "R1.5",
/// "l 0.7 dB", "S2", "M 3", "-1.5", "+0.25".
export function parseBalance(text) {
    const s = String(text ?? '').trim().toLowerCase().replace('−', '-').replace(',', '.').replace(/db$/, '').replace(/\s+/g, '');
    if (s === 'c' || s === 'center' || s === 'centre') return 0;
    const m = /^([lrms])?([+-]?(?:\d+(?:\.\d*)?|\.\d+))([lrms])?$/.exec(s);
    if (!m || (m[1] && m[3])) return null;
    const side = m[1] || m[3];
    const v = Math.abs(+m[2]);
    if (!side) return +m[2];
    return side === 'l' || side === 'm' ? -v : v;
}

/// The balance slider: a position −1..1 to dB and back. Cubed, so the
/// middle — where a balance is set by tenths of a dB — is the finest.
export const BALANCE_SPAN = 30;
export const balanceFromPos = p => Math.round(Math.sign(p) * BALANCE_SPAN * Math.abs(p) ** 3 * 100) / 100;
export const posFromBalance = db => Math.sign(db) * Math.cbrt(Math.min(1, Math.abs(db) / BALANCE_SPAN));

/// Centimetres of a speaker's distance one millisecond of delay makes up
/// (sound at 343 m/s, 20 °C).
export const CM_PER_MS = 34.3;

/// The delay as the field shows it: "0 ms", else the late channel and its
/// ms — "L 2.04 ms", "R 0.5 ms".
export function fmtDelay(ms) {
    if (!ms) return '0 ms';
    const a = Math.abs(ms);
    return (ms > 0 ? 'R ' : 'L ') + (Math.abs(a * 10 - Math.round(a * 10)) < 1e-9 ? a.toFixed(1) : a.toFixed(2)) + ' ms';
}

/// The distance a delay makes up, as the hint beside it: "70 cm", "1.2 cm".
export function fmtDelayCm(ms) {
    const cm = Math.abs(ms) * CM_PER_MS;
    return (cm < 10 ? cm.toFixed(1) : String(Math.round(cm))) + ' cm';
}

/// A typed delay in ms (+ = the right late), or null: "0", "off", "L2.04",
/// "r 1.5 ms", "-2", and distances — "L70cm", "R 0.7 m" — in centimetres
/// of sound (`CM_PER_MS`).
export function parseDelay(text) {
    const s = String(text ?? '').trim().toLowerCase().replace('−', '-').replace(',', '.').replace(/\s+/g, '');
    if (s === 'off' || s === 'none') return 0;
    const m = /^([lr])?([+-]?(?:\d+(?:\.\d*)?|\.\d+))(ms|cm|m)?([lr])?$/.exec(s);
    if (!m || (m[1] && m[4])) return null;
    const unit = m[3] || 'ms';
    let v = +m[2];
    if (unit === 'cm') v /= CM_PER_MS;
    else if (unit === 'm') v = v * 100 / CM_PER_MS;
    const side = m[1] || m[4];
    if (!side) return v;
    return side === 'l' ? -Math.abs(v) : Math.abs(v);
}

/// The delay slider: a position −1..1 to ms and back, cubed like the
/// balance's, so the small delays a room needs are the finest.
export const DELAY_SPAN = 30;
export const delayFromPos = p => Math.round(Math.sign(p) * DELAY_SPAN * Math.abs(p) ** 3 * 100) / 100;
export const posFromDelay = ms => Math.sign(ms) * Math.cbrt(Math.min(1, Math.abs(ms) / DELAY_SPAN));

/// Sound's speed in air at `tempC` °C, m/s.
export const soundSpeed = tempC => 331.3 * Math.sqrt(1 + tempC / 273.15);

/// Metres in one of the calculator's units.
export const UNIT_M = { cm: 0.01, m: 1, in: 0.0254, ft: 0.3048 };

/// The speakers from the listener's distances to the left and the right
/// one (in `unit`) at `tempC` °C: the delay that makes the nearer one's
/// sound arrive with the farther one's (ms, + = the right late, 0.01 ms
/// steps, within ±30 — `clamped` when it was more), and the balance that
/// lowers the nearer one by what the distance adds in a free field,
/// 20·log10(far/near) (dB, + = toward the right). Null for a distance that
/// is not above 0.
export function speakerCalc(left, right, unit = 'cm', tempC = 20) {
    const k = UNIT_M[unit] ?? 0.01;
    const l = +left * k, r = +right * k;
    if (!(l > 0) || !(r > 0) || !isFinite(l) || !isFinite(r)) return null;
    const t = isFinite(+tempC) ? Math.min(45, Math.max(-10, +tempC)) : 20;
    const raw = (l - r) / soundSpeed(t) * 1000;
    const delay = Math.min(30, Math.max(-30, Math.round(raw * 100) / 100));
    const balance = Math.round(20 * Math.log10(r / l) * 100) / 100;
    return { delay: delay === 0 ? 0 : delay, balance: balance === 0 ? 0 : balance, clamped: Math.abs(raw) > 30.005 };
}

/// The nearest note of `f` and its offset in cents: { name: 'A4', cents: 3 }.
export function noteOf(f) {
    const m = 69 + 12 * Math.log2(f / 440), n = Math.round(m);
    const pc = ((n % 12) + 12) % 12;
    return { name: NOTE_NAMES[pc] + (Math.floor(n / 12) - 1), cents: Math.round((m - n) * 100) };
}

/// The band type a click at (x, y) adds, by position in the plot
/// { l, r, t, b } (pixels): near the left or right edge a cut, near the
/// floor a notch, else a bell.
export function typeAt(x, y, plot) {
    const w = plot.r - plot.l, h = plot.b - plot.t;
    const edge = Math.max(14, w * 0.035);
    if (x - plot.l < edge) return 'lowCut';
    if (plot.r - x < edge) return 'highCut';
    if (plot.b - y < Math.max(10, h * 0.09)) return 'notch';
    return 'bell';
}

/// The shelf a drag of the summed curve at x makes, or null away from the ends.
export function shelfAt(x, plot) {
    const w = plot.r - plot.l;
    if (x - plot.l < w * 0.12) return 'lowShelf';
    if (plot.r - x < w * 0.12) return 'highShelf';
    return null;
}

/// The nearest point { id, x, y } within `radius` of p, or null.
export function hitPoint(points, p, radius) {
    let best = null, bd = radius;
    for (const q of points) {
        const d = Math.hypot(q.x - p.x, q.y - p.y);
        if (d <= bd) { bd = d; best = q; }
    }
    return best;
}

/// The ids of the points inside the rectangle spanned by two corners.
export function inRect(points, a, z) {
    const x0 = Math.min(a.x, z.x), x1 = Math.max(a.x, z.x), y0 = Math.min(a.y, z.y), y1 = Math.max(a.y, z.y);
    return points.filter(q => q.x >= x0 && q.x <= x1 && q.y >= y0 && q.y <= y1).map(q => q.id);
}

/// The ids from one band to another in list order (a Shift-click range).
export function idRange(ids, from, to) {
    const i = ids.indexOf(from), j = ids.indexOf(to);
    if (i < 0 || j < 0) return j < 0 ? [] : [to];
    return ids.slice(Math.min(i, j), Math.max(i, j) + 1);
}

/// A group move. `start` holds each selected band at the drag start
/// { id, f, gain, gained }; the primary band moved from its start to
/// (f1, g1). Frequencies shift together in log space, held so that no band
/// leaves the range (the group keeps its shape); gains scale by the
/// primary's ratio, or shift by its difference while it starts near 0 dB.
/// Returns { id → { f, gain } }.
export function groupMove(start, primaryId, f1, g1) {
    const p = start.find(s => s.id === primaryId);
    const out = {};
    if (!p) return out;
    let ratio = f1 / p.f;
    for (const s of start) ratio = clamp(ratio, F_LO / s.f, F_HI / s.f);
    const scale = p.gained && Math.abs(p.gain) >= 0.5;
    const k = scale ? g1 / p.gain : 1, dg = scale ? 0 : g1 - p.gain;
    for (const s of start) {
        const gain = !s.gained ? s.gain : s.id === primaryId ? clamp(g1, -G_MAX, G_MAX) : clamp(s.gain * k + dg, -G_MAX, G_MAX);
        out[s.id] = { f: s.f * ratio, gain };
    }
    return out;
}

/// The dB scale that shows |gain|: the current one, or the next larger that
/// holds it (the graph grows while a drag goes beyond; it never shrinks itself).
export function expandRange(range, gain) {
    const a = Math.abs(gain);
    if (a <= range) return range;
    return RANGES.find(r => r >= a) ?? RANGES[RANGES.length - 1];
}

/// The step of the dB grid for a scale.
export const gridStep = range => (range <= 3 ? 1 : range <= 6 ? 2 : range <= 12 ? 3 : 6);

/// The frequency view { lo, hi } zoomed by `factor` (< 1 narrows) around
/// `anchor` Hz, which keeps its place. At most F_MIN..F_MAX, at least two octaves.
export function zoomView(v, anchor, factor) {
    const L0 = Math.log(v.lo), L1 = Math.log(v.hi), La = clamp(Math.log(anchor), L0, L1);
    const full = Math.log(F_MAX / F_MIN);
    const span = clamp((L1 - L0) * factor, Math.log(4), full);
    const t = (La - L0) / (L1 - L0 || 1);
    return panView({ lo: Math.exp(La - t * span), hi: Math.exp(La + (1 - t) * span) }, 0);
}

/// The view moved by `dl` in natural-log units (> 0 to higher frequencies),
/// held inside F_MIN..F_MAX.
export function panView(v, dl) {
    const span = Math.log(v.hi / v.lo);
    let lo = Math.log(v.lo) + dl;
    lo = clamp(lo, Math.log(F_MIN), Math.log(F_MAX) - span);
    return { lo: Math.exp(lo), hi: Math.exp(lo + span) };
}

export const isFullView = v => v.lo <= F_MIN * 1.0001 && v.hi >= F_MAX * 0.9999;

/// One keyboard nudge of a band: arrows move f (a quarter tone; Shift a
/// sixteenth) and gain (0.5 dB; Shift 0.1 dB); a cut's up/down moves Q.
export function nudge(b, key, fine) {
    const out = { f: b.f, gain: b.gain, q: b.q };
    const fs = Math.pow(2, (fine ? 1 / 4 : 1) / 24);
    if (key === 'ArrowRight') out.f = clamp(b.f * fs, F_LO, F_HI);
    else if (key === 'ArrowLeft') out.f = clamp(b.f / fs, F_LO, F_HI);
    else {
        const dir = key === 'ArrowUp' ? 1 : key === 'ArrowDown' ? -1 : 0;
        if (!dir) return out;
        if (isCut(b.type) || b.type === 'notch' || b.type === 'bandPass') out.q = clamp(b.q * Math.pow(fine ? 1.02 : 1.1, dir), Q_LO, Q_HI);
        else out.gain = clamp(Math.round((b.gain + dir * (fine ? 0.1 : 0.5)) * 10) / 10, -G_MAX, G_MAX);
    }
    return out;
}

/// The cut slope one wheel step from `slope` (dir > 0 steeper).
export function slopeStep(slopes, slope, dir) {
    const i = slopes.indexOf(slope);
    return slopes[clamp((i < 0 ? 1 : i) + (dir > 0 ? 1 : -1), 0, slopes.length - 1)];
}
