// ══════════════════════════════════════════════════════════════════════
// HP's unit background: the last two seconds of Hybrid-Phase, drawn under
// the unit's text. The player asks /player/phase_view for bins of the blend
// (the switch, or α) and of the detector's envelope around the heard frame;
// the heard frame sits on a dashed NOW line just left of the lamp, history
// scrolls away to the left, the next few hundred ms (as far as the player
// knows them) are to its right.
//
//   HP  — the switch as a lilac square wave, the envelope as a yellow line;
//   αHP — α as brightness: a full-height column per bin, a pink wash, the
//         envelope in yellow.
//
// The threshold is a dashed line like NOW's, fading in from the controls'
// right edge; it follows the sensitivity slider at once. Lilac is what the
// engine did: the bins already heard stay as they were drawn when the step
// changes (bakePast), only the ones to come follow the new step.
//
// One bin per CSS pixel. The player is asked ten times a second for the
// window and three seconds past NOW, so a late answer (a busy machine) still
// finds the bins to come in the last one. The bins are drawn once per answer
// on a strip of their own; each frame only puts the strip in place, on whole
// screen pixels, and draws the lines over it. The picture slides left on the
// page's own clock, eased toward each answer (viewClock), so it moves every
// frame and evenly. Only drawing: nothing here reaches the sound.
// ══════════════════════════════════════════════════════════════════════

import { HP_SENS_THRESHOLDS, HP_SENS_MIN, hpSensStep } from './dsprack.js';
import { hpSwitchFresh } from './hpswitch.js';

/// The window the background shows, ms.
export const VIEW_SPAN_MS = 2000;
/// How far past NOW each request reaches, ms (as far as the route's 1024
/// bins over at most 10 s allow on a wide unit).
export const VIEW_AHEAD_MS = 3000;
const VIEW_MAX_N = 1024, VIEW_MAX_SPAN_MS = 9500;
/// How often the player is asked, ms.
export const VIEW_EVERY_MS = 100;
/// A request not answered in this long no longer holds the next one back, ms.
const VIEW_GIVE_UP_MS = 2000;
/// An empty answer this soon after a good one leaves the picture as it is,
/// ms: one stray answer blanked the unit till the next (a blink); only an
/// empty one past this clears it.
export const VIEW_KEEP_MS = 600;

/// The route's answer (see PhaseMeter::view): null while nothing blends,
/// and while a converted file's timeline is not there (being worked out, no
/// source, …: the state byte, as on the lamp) — no stale picture then.
/// `step`: the sensitivity step in effect — the byte after the bins (i8,
/// −3..+3); `nowBin`: the grid bin of the heard frame (u32 after it); each
/// null in an answer without it.
export function parseView(buf) {
    const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf || new ArrayBuffer(0));
    if (b.length < 8 || b[1] >= 2) return null;
    const n = b[4] | b[5] << 8;
    if (!n || b.length < 8 + 2 * n) return null;
    const t = 8 + 2 * n;
    return {
        alpha: b[0] === 2, state: b[1], hasEnv: b[2] === 1, frac: b[3] / 256,
        n, nowK: b[6] | b[7] << 8,
        step: b.length > t ? b[t] << 24 >> 24 : null,
        nowBin: b.length >= t + 5 ? (b[t + 1] | b[t + 2] << 8 | b[t + 3] << 16 | b[t + 4] << 24) >>> 0 : null,
        w: b.subarray(8, 8 + n), env: b.subarray(8 + n, t),
    };
}

/// The bins already heard, as they were drawn: `cache` ({ key, now, map }
/// of grid bin → weight) keeps every bin up to NOW the first time it is
/// seen; a later answer — the slider moved, the chain was built anew and
/// drew the whole track at the new step — gets those back, and only the
/// bins to come are its own. Starts over on another `key` (kind, bin size),
/// on a jump back (seek, the next track) or past the window (seek ahead).
export function bakePast(cache, v, key) {
    if (!v || v.nowBin == null) return v;
    key = `${v.alpha ? 'a' : 'h'}|${key}`;
    if (cache.key !== key || cache.now == null || v.nowBin < cache.now || v.nowBin - cache.now > v.n) {
        cache.key = key; cache.map = new Map();
    }
    cache.now = v.nowBin;
    const g0 = v.nowBin - v.nowK, w = Uint8Array.from(v.w);
    for (const g of cache.map.keys()) if (g < g0) cache.map.delete(g);
    for (let k = 0; k <= v.nowK && k < v.n; k++) {
        const old = cache.map.get(g0 + k);
        if (old === undefined) cache.map.set(g0 + k, w[k]); else w[k] = old;
    }
    return { ...v, w };
}

/// What to ask for a unit `w` px wide with NOW at `xN`: one bin per pixel
/// over VIEW_SPAN_MS, the first bin 2 px left of the left edge, the last
/// `aheadMs` past NOW (fewer when the route's limits come first).
export function viewQuery(w, xN, aheadMs = VIEW_AHEAD_MS) {
    const width = Math.max(1, Math.round(w));
    const binMs = VIEW_SPAN_MS / width;
    const back = Math.max(0, Math.ceil(xN)) + 2;
    const n = Math.max(1, Math.min(VIEW_MAX_N, Math.floor(VIEW_MAX_SPAN_MS / binMs), back + Math.ceil(aheadMs / binMs)));
    const fromMs = -back * binMs;
    return { n, binMs, fromMs, toMs: fromMs + n * binMs };
}

/// The left edge of bin `k`, px: the heard frame (at `frac` into bin
/// `nowK`) on `xN`, moved left by `shift` px since the answer.
export const binX = (k, nowK, frac, xN, shift = 0) => xN + (k - nowK) - frac - shift;

/// How much of the way to each answer the clock's aim goes — less for an
/// answer long on its way (CLOCK_TRIP_MS: half a round trip that halves
/// it); how much faster or slower than the sound the picture may go to get
/// there; past CLOCK_SNAP_MS off (beyond the answer's own uncertainty) it
/// jumps (a seek, the next track, back from a hidden window); an answer
/// standing still for CLOCK_STILL_MS (a hold) stops it. Standing still is
/// within CLOCK_SAME_MS of the frame last seen moving: a standing engine's
/// frame wobbles by a fraction of a millisecond as its device latency is
/// re-measured (0.17 ms seen at a track's end waiting for the next), while
/// two answers a request apart are a device period or more apart.
const CLOCK_GAIN = 0.15;
const CLOCK_TRIP_MS = 5;
const CLOCK_SLEW = 0.03;
const CLOCK_SNAP_MS = 300;
const CLOCK_STILL_MS = 120;
const CLOCK_SAME_MS = 2;

/// The heard frame on the page's clock, so the picture moves evenly. The
/// engine's frame steps a device period at a time (10 ms on WASAPI, up to
/// ~50 ms on ASIO) and is read somewhere between the request going (`sent`)
/// and its answer coming (`got`), both page time: the middle is taken, and
/// an answer that was long on its way counts for less. `clk.aim` (grid bins
/// less page time / binMs) eases toward each answer; `clk.off`, what the
/// picture shows, follows the aim at most CLOCK_SLEW of the speed, so a
/// correction never shows as a step; on a new grid it starts over. Returns
/// 'start', 'hold', 'snap' or 'on' when it started over, stopped, jumped
/// or went on again (the trace's word), else nothing.
export function viewClock(clk, v, sent, got, binMs) {
    if (v?.nowBin == null) { clk.key = null; return; }
    const key = `${binMs}`, p = v.nowBin + v.frac;
    const mid = (sent + got) / 2, half = Math.max(0, got - sent) / 2;
    const o = p - mid / binMs;
    if (clk.key !== key || clk.off == null) {
        clk.key = key; clk.binMs = binMs; clk.p = p;
        clk.off = clk.aim = o; clk.t = got; clk.held = clk.floor = null; clk.movedAt = got;
        return 'start';
    }
    // The same frame again: the engine is between two periods, or held —
    // held only when it was read CLOCK_STILL_MS after the last answer that
    // moved came at the least. A wobble is the same frame: taken for a move
    // it let the picture run on over a standing engine and snap back.
    if (Math.abs(p - clk.p) * binMs < CLOCK_SAME_MS) {
        if (sent - clk.movedAt > CLOCK_STILL_MS && clk.held == null) { clk.held = clockPos(clk, got); return 'hold'; }
        return;
    }
    clk.p = p; clk.movedAt = got;
    const off = (o + got / clk.binMs - clockPos(clk, got)) * binMs;
    if (Math.abs(off) > CLOCK_SNAP_MS + half) {
        clk.off = clk.aim = o; clk.t = got; clk.held = clk.floor = null;
        return 'snap';
    } else if (clk.held != null) {
        // On again: the picture waits where it stopped till the frame heard
        // gets there, rather than going back.
        clk.off = clk.aim = o; clk.t = got; clk.floor = clk.held; clk.held = null;
        return 'on';
    } else clk.aim += CLOCK_GAIN / (1 + half / CLOCK_TRIP_MS) * (o - clk.aim);
}

/// The heard frame at page time `ts`, grid bins (viewClock's): the shown
/// offset first moves toward the aim as far as the time since allows.
export function clockPos(clk, ts) {
    if (clk.held != null) return clk.held;
    const dt = ts - clk.t;
    if (dt > 0) {
        const most = CLOCK_SLEW * dt / clk.binMs;
        clk.off += Math.max(-most, Math.min(most, clk.aim - clk.off));
        clk.t = ts;
    }
    return Math.max(clk.floor ?? -Infinity, ts / clk.binMs + clk.off);
}

/// Stop the clock where the picture is (the player paused or the window
/// was hidden: no frames come); the next answer that moves goes on from
/// there, or jumps if it is far.
export function clockHold(clk, ts) {
    if (clk.key && clk.off != null) clk.held ??= clockPos(clk, ts);
}

/// Frames missed (a step over CATCH_GAP screen periods): the strip does not
/// jump the whole way at once — the frame after the gap moves it as a
/// regular frame would, and the rest (the lag) is taken back over time, eased
/// in and out, the picture going at most CATCH_BOOST faster than the sound:
/// the catch-up lasts 1.5·lag / (CATCH_BOOST·speed), within CATCH_MIN_MS ..
/// CATCH_MAX_MS (Anton's pick on the bench, 3.11: 150 ms, 1000 ms, 1 % — so
/// in practice a second). A lag over CATCH_MAX bins jumps the excess at once
/// and catches up the rest (it used to jump all of it, and the 4-frame catch-up
/// before this ran at up to +170 % of the speed). A move back or over
/// CATCH_SEEK bins (a seek, the next track) goes at once.
const CATCH_MIN_MS = 150;
const CATCH_MAX_MS = 1000;
const CATCH_BOOST = 0.01;
const CATCH_GAP = 1.5;
const CATCH_MAX = 400;
const CATCH_SEEK = 1000;
const CATCH_PERIOD_MAX_MS = 100;

const smoothstep = (k) => { const x = Math.min(1, Math.max(0, k)); return x * x * (3 - 2 * x); };
const catchLeft = (c, ts) => (c.lag0 > 0 ? c.lag0 * (1 - smoothstep((ts - c.t0) / c.dur)) : 0);

/// Where to draw the heard frame `p` (grid bins, clockPos's) at page time
/// `ts`: `p` itself while frames come regularly, a lag behind it that eases
/// to nothing after a gap. `c` is the state ({} to begin, or to start over);
/// the screen's period and the picture's speed are learned from the regular
/// frames. After a gap `c.began` holds { lag, ms } (the trace takes it).
export function catchUp(c, ts, p) {
    if (c.t == null) { Object.assign(c, { t: ts, p, period: null, speed: null, lag0: 0, t0: ts, dur: 1 }); return p; }
    const dt = ts - c.t, d = p - c.p;
    if (!(dt > 0)) return p - catchLeft(c, ts);
    c.t = ts; c.p = p;
    if (d < 0 || d > CATCH_SEEK) { c.lag0 = 0; return p; }
    if (c.period != null && dt > CATCH_GAP * c.period) {
        // The part a regular frame would have moved now, the rest eased.
        const lag = Math.min(CATCH_MAX, catchLeft(c, ts) + d * (1 - c.period / dt));
        const speed = c.speed > 0 ? c.speed : d / dt;
        c.lag0 = lag; c.t0 = ts;
        c.dur = Math.min(CATCH_MAX_MS, Math.max(CATCH_MIN_MS, 1.5 * lag / (CATCH_BOOST * speed)));
        c.began = { lag, ms: c.dur, gap: dt };
        return p - lag;
    }
    if (dt < CATCH_PERIOD_MAX_MS) {
        c.period = c.period == null ? dt : c.period + 0.1 * (dt - c.period);
        c.speed = c.speed == null ? d / dt : c.speed + 0.1 * (d / dt - c.speed);
    }
    if (c.lag0 > 0 && ts - c.t0 >= c.dur) { c.lag0 = 0; return p; }
    return p - catchLeft(c, ts);
}

const sensThr = (step) => HP_SENS_THRESHOLDS[step - HP_SENS_MIN];
const isStep = (n) => Number.isInteger(n) && n >= HP_SENS_MIN && n <= HP_SENS_MIN + 6;

/// The slider's detector threshold, drawn at once (the new chain takes
/// seconds, a pause forever); the background itself never follows it.
export const viewThr = (step = hpSensStep()) => sensThr(isStep(step) ? step : 0);

/// The threshold line's alpha along the unit, as [x px, alpha]: nothing at
/// `xs` (just right of the controls), up through 35 % and 75 % of the way
/// to NOW, brightest just before it, half again to the right edge `W`.
export function thrStops(xs, xN, W) {
    const d = xN - xs;
    return [[xs, 0], [xs + 0.35 * d, 0.08], [xs + 0.75 * d, 0.40], [xN - 2, 0.65], [W, 0.30]];
}

/// NOW's place on the unit's canvas: 32 px left of the lamp's right edge
/// (v17: 16 px further from the lamp than beside the old 6 px round one).
export function nowX(canvas, lamp) {
    const c = canvas.getBoundingClientRect();
    return (lamp.getBoundingClientRect().right - c.left) / zoomOf(canvas, c) - 32;
}

/// The canvas's CSS zoom (the remote enlarges the rack on a phone): the
/// screen rectangles are zoomed, the canvas's own pixels are not. 1 in the app.
function zoomOf(canvas, c = canvas.getBoundingClientRect()) {
    const w = canvas.offsetWidth;
    return w > 0 && c.width > 0 ? c.width / w : 1;
}

/// Where NOW (x = `xl`) crosses the threshold (y = `yt`), both on half
/// pixels: no cross, each line as two halves going outward from 2.5 px off
/// the crossing (each half starts its own dash), and a 1×1 square in the
/// middle — dash 3 · gap 2 · square · gap 2 · dash 3 both ways. `h` and
/// `v` as [x0, y0, x1, y1] from the crossing out; null (whole lines) when
/// the threshold is off the canvas or starts right of NOW.
export function crossSegs(xl, yt, xs, W, H) {
    if (!(yt > 0 && yt < H) || !(xs < xl) || !(xl > 0 && xl < W)) return null;
    const g = 2.5;
    const h = [[xl - g, yt, xs, yt], [xl + g, yt, W, yt]].filter(([a, , b]) => a !== b && (a > b) === (a < xl));
    const v = [[xl, yt - g, xl, 1], [xl, yt + g, xl, H - 1]].filter(([, a, , b]) => a !== b && (a > b) === (a < yt));
    return { h, v, sq: [xl - 0.5, yt - 0.5, 1, 1] };
}

function vignette(x, W, H) {
    x.globalCompositeOperation = 'destination-in';
    const g = x.createLinearGradient(0, 0, W, 0);
    g.addColorStop(0, 'rgba(0,0,0,0)'); g.addColorStop(0.1, 'rgba(0,0,0,.55)');
    g.addColorStop(0.22, 'rgba(0,0,0,.8)'); g.addColorStop(0.5, 'rgba(0,0,0,.92)');
    g.addColorStop(1, 'rgba(0,0,0,1)');
    x.fillStyle = g; x.fillRect(0, 0, W, H);
    const v = x.createLinearGradient(0, 0, 0, H);
    v.addColorStop(0, 'rgba(0,0,0,.6)'); v.addColorStop(0.2, 'rgba(0,0,0,1)');
    v.addColorStop(0.85, 'rgba(0,0,0,1)'); v.addColorStop(1, 'rgba(0,0,0,.6)');
    x.fillStyle = v; x.fillRect(0, 0, W, H);
    x.globalCompositeOperation = 'source-over';
}

function envLine(x, v, X, Y, alpha) {
    if (!v.hasEnv) return;
    x.beginPath();
    for (let k = 0; k < v.n; k++) {
        const px = X(k) + 0.5, py = Y(v.env[k] / 255);
        k ? x.lineTo(px, py) : x.moveTo(px, py);
    }
    x.strokeStyle = `rgba(250,204,21,${alpha})`; x.lineWidth = 1; x.stroke();
}

/// Dashed segments [x0, y0, x1, y1] in one stroke, each its own subpath.
function dashed(x, style, segs) {
    x.save(); x.setLineDash([3, 2]); x.strokeStyle = style; x.lineWidth = 1;
    x.beginPath();
    for (const [x0, y0, x1, y1] of segs) { x.moveTo(x0, y0); x.lineTo(x1, y1); }
    x.stroke(); x.restore();
}

function nowLine(x, xl, H, segs = [[xl, 1, xl, H - 1]]) {
    const g = x.createLinearGradient(0, 0, 0, H);
    g.addColorStop(0, 'rgba(226,232,240,0)'); g.addColorStop(0.25, 'rgba(226,232,240,.65)');
    g.addColorStop(0.75, 'rgba(226,232,240,.65)'); g.addColorStop(1, 'rgba(226,232,240,0)');
    dashed(x, g, segs);
}

/// The threshold from the controls to the right edge, dashed as NOW.
function thrLine(x, y, xs, xN, W, segs = [[xs, y, W, y]]) {
    if (!(W > xs)) return;
    const g = x.createLinearGradient(0, 0, W, 0);
    for (const [px, a] of thrStops(xs, xN, W)) g.addColorStop(Math.min(1, Math.max(0, px / W)), `rgba(226,232,240,${a})`);
    dashed(x, g, segs);
}

/// Size the canvas to its box at the screen's pixel ratio; its 2D context
/// in CSS px, or null while the unit is not on screen.
function context(canvas) {
    const W = canvas.offsetWidth, H = canvas.offsetHeight;
    if (!W || !H) return null;
    const r = globalThis.devicePixelRatio || 1;
    const cw = Math.round(W * r), ch = Math.round(H * r);
    if (canvas.width !== cw || canvas.height !== ch) { canvas.width = cw; canvas.height = ch; }
    const x = canvas.getContext('2d');
    x.setTransform(cw / W, 0, 0, ch / H, 0, 0);
    x.clearRect(0, 0, W, H);
    return { x, W, H, r: cw / W };
}

/// The bins of `v` on `x` (W×H CSS px, `r` screen px to one), bin k's left
/// edge at X(k): what moves; the vignette and the lines go over it (overlay).
function paintBins(x, v, X, W, H, r) {
    const top = 3, hh = H - 6;
    const Y = (a) => top + hh * (1 - Math.min(1, a));
    if (v.alpha) {
        // α as brightness: soft-edged squares where α holds up.
        for (let k = 0; k < v.n; k++) {
            const a = v.w[k] / 255;
            if (a <= 0) continue;
            x.fillStyle = `rgba(232,121,249,${(0.62 * a).toFixed(3)})`;
            x.fillRect(X(k), top, 1.02, hh);
        }
        x.globalCompositeOperation = 'source-atop';
        const vg = x.createLinearGradient(0, top, 0, top + hh);
        vg.addColorStop(0, 'rgba(250,232,255,.55)'); vg.addColorStop(0.22, 'rgba(232,121,249,0)');
        vg.addColorStop(1, 'rgba(124,58,237,.4)');
        x.fillStyle = vg; x.fillRect(0, 0, W, H);
        x.globalCompositeOperation = 'source-over';
        x.fillStyle = 'rgba(232,121,249,.05)'; x.fillRect(0, 0, W, H);
        envLine(x, v, X, Y, 0.6);
    } else {
        // Edges on whole screen pixels (finer steps than CSS px on a
        // high-DPI screen), the 1 px line centred on them.
        const px = (xx) => Math.round(xx * r) / r + 0.5;
        const yH = top + 0.5, yL = H - top - 0.5;
        x.beginPath();
        let prev = null;
        for (let k = 0; k < v.n; k++) {
            const s = v.w[k] >= 128;
            const xi = px(X(k)), y = s ? yH : yL;
            if (prev === null) x.moveTo(xi, y);
            else if (s !== prev) { x.lineTo(xi, prev ? yH : yL); x.lineTo(xi, y); }
            else x.lineTo(xi, y);
            prev = s;
        }
        const xEnd = px(X(v.n));
        x.lineTo(xEnd, prev ? yH : yL);
        x.strokeStyle = 'rgba(167,139,250,.95)'; x.lineWidth = 1; x.stroke();
        x.lineTo(xEnd, yL); x.lineTo(px(X(0)), yL); x.closePath();
        x.fillStyle = 'rgba(167,139,250,.38)'; x.fill();
        envLine(x, v, X, Y, 0.65);
    }
}

/// What stands still over the bins: the vignette, then where the envelope
/// (yellow) trips the switch, or takes α to the full; NOW across it, a
/// grey pixel where they meet.
function overlay(x, W, H, xN, xs, thr) {
    vignette(x, W, H);
    const yt = Math.round(3 + (H - 6) * (1 - Math.min(1, thr))) - 0.5, xl = Math.round(xN) + 0.5;
    const cross = crossSegs(xl, yt, xs, W, H);
    thrLine(x, yt, xs, xN, W, cross?.h);
    nowLine(x, xl, H, cross?.v);
    if (cross) { x.fillStyle = 'rgba(148,163,184,.85)'; x.fillRect(...cross.sq); }
}

/// One picture of `v` (parseView) on `canvas`, NOW at `xN`, slid left by
/// `shift` px; the threshold `thr` (viewThr's) from `xs` (the controls'
/// right edge + 6 px). Without `v` the canvas is cleared. Everything drawn
/// afresh: the player puts a strip in place instead (drawStrip), the same
/// picture on whole screen pixels.
export function drawView(canvas, v, xN, shift = 0, xs = 0, thr = viewThr()) {
    const c = context(canvas);
    if (!c || !v) return;
    const { x, W, H, r } = c;
    paintBins(x, v, (k) => binX(k, v.nowK, v.frac, xN, shift), W, H, r);
    overlay(x, W, H, xN, xs, thr);
}

/// The canvas's size in CSS px and its scale to screen px, as context()
/// sets it; null while the unit is not on screen.
function scaleOf(canvas) {
    const W = canvas.offsetWidth, H = canvas.offsetHeight;
    if (!W || !H) return null;
    const d = globalThis.devicePixelRatio || 1;
    return { W, H, sx: Math.round(W * d) / W, sy: Math.round(H * d) / H };
}

/// The bins of `v` once, on `strip` at the screen's pixels of a canvas
/// `canvas` (scaleOf's), bin k's left edge k CSS px from the strip's.
export function paintStrip(strip, v, canvas) {
    const s = scaleOf(canvas);
    if (!s) return;
    const w = v.n + 2;
    const cw = Math.ceil(w * s.sx), ch = Math.round(s.H * s.sy);
    if (strip.width !== cw || strip.height !== ch) { strip.width = cw; strip.height = ch; }
    const x = strip.getContext('2d');
    x.setTransform(s.sx, 0, 0, s.sy, 0, 0);
    x.clearRect(0, 0, w, s.H);
    paintBins(x, v, (k) => k, w, s.H, s.sx);
}

/// Where drawStrip puts the strip of `v`, screen px: bin 0's left edge
/// (binX) on the nearest whole one, or just where it is with `fine`.
export const stripX = (v, xN, shift, sx, fine = false) => {
    const x = binX(0, v.nowK, v.frac, xN, shift) * sx;
    return fine ? x : Math.round(x);
};

/// drawView's picture from the strip of `v` (paintStrip): the strip put in
/// place on a whole screen pixel (`fine`: on a fraction of one, smoothed),
/// the vignette and the lines over it. Returns where the strip went, screen px.
export function drawStrip(canvas, strip, v, xN, shift = 0, xs = 0, thr = viewThr(), fine = false) {
    const c = context(canvas);
    if (!c || !v) return;
    const { x, W, H, r } = c;
    x.save();
    x.setTransform(1, 0, 0, 1, 0, 0);
    x.imageSmoothingEnabled = fine;
    if (fine) x.imageSmoothingQuality = 'high';
    const at = stripX(v, xN, shift, r, fine);
    x.drawImage(strip, at, 0);
    x.restore();
    overlay(x, W, H, xN, xs, thr);
    return at;
}

// ── The player's side: ask ten times a second, slide every frame ──────
let last = null;       // { v, binMs, xN } of the last answer
let pending = 0;       // when the request in flight went, page time; 0: none
let sentAt = -Infinity;
let asked = 0;         // requests sent: an answer to an earlier one is let go
let lastTs = 0;
let goodAt = 0;        // when the last answer with a picture came, page time
let drawn = false;
let shown = null;      // the picture on screen: { v, xN, shift }
let strip = null;      // { cv, v, sx, sy, H }: the bins of shown.v, drawn once
const baked = { key: null, now: null, map: new Map() };   // bakePast's
const clock = { key: null };                              // viewClock's
let caught = {};       // catchUp's
let trace = null;      // hptrace.js's tracer while AURA_HP_TRACE=1, else null
let holdTold = false;  // the trace was told of the player's hold
let onItsWay = false;  // Hybrid-Phase's pair is being made for the chain on the air

/// The trace on (a makeTrace tracer) or off (null).
export function hpViewTrace(t) { trace = t; }

/// Whether missed frames are caught up with over time (catchUp) or the
/// picture goes straight to the heard frame. The remote turns it off: a
/// phone misses a vsync often (each one began a second-long catch-up that
/// the next added to, the picture lagging and lurching), and a turn of the
/// screen holds it ~0.7 s, the catch-up after it a rush of the past.
let catching = true;
export function hpViewCatchUp(on) { catching = !!on; caught = {}; }

const unit = () => document.querySelector('.hpu');

/// Where the threshold line starts on the unit's canvas: 6 px right of the
/// controls group.
export function thrStartX(canvas, ctl) {
    if (!ctl) return 0;
    const c = canvas.getBoundingClientRect();
    return (ctl.getBoundingClientRect().right - c.left) / zoomOf(canvas, c) + 6;
}

/// Draw the picture on screen again with the slider's threshold — the slider
/// moved (on a pause too: the line needs no engine, the lamp loop is asleep).
export function hpViewRepaint() {
    const u = unit(), cv = u?.querySelector('canvas.hpbg');
    if (cv && shown) paint(cv, u);
}

/// The picture on screen from its strip, the strip drawn first when the
/// answer or the canvas's size is new.
function paint(cv, u) {
    const { v, xN, shift } = shown, s = scaleOf(cv);
    if (!s) return null;
    if (!strip || strip.v !== v || strip.sx !== s.sx || strip.sy !== s.sy || strip.H !== s.H) {
        strip = { cv: strip?.cv ?? cv.ownerDocument.createElement('canvas'), v, sx: s.sx, sy: s.sy, H: s.H };
        paintStrip(strip.cv, v, cv);
    }
    // Moving, the strip slides on fractions of a pixel: at ~0.5 px a frame whole
    // pixels step 0/1 unevenly and the picture jerks; held, it stays sharp.
    return drawStrip(cv, strip.cv, v, xN, shift, thrStartX(cv, u.querySelector('.ctl')), viewThr(), clock.held == null);
}
globalThis.document?.addEventListener?.('change', (e) => { if (e.target?.id === 'convHpSens') hpViewRepaint(); });

/// Once, as soon as the unit is on screen: its whole picture — both kinds'
/// bins, the strip put in place smoothed and on whole pixels, the overlay —
/// drawn off screen and taken as a bitmap, so the card has what these draws
/// need before anything plays. Drawn first right after a start was heard,
/// they held every window of the app ~120 ms. Returns whether it drew.
let warmed = false;
export function hpViewWarm() {
    if (warmed) return false;
    const cv = unit()?.querySelector('canvas.hpbg'), s = cv && scaleOf(cv);
    if (!s) return false;
    warmed = true;
    const doc = cv.ownerDocument, n = 64;
    const w = new Uint8Array(n).map((_, k) => (k % 7 < 3 ? 255 : k % 5 ? 140 : 0));
    const env = new Uint8Array(n).map((_, k) => (k * 37) & 255);
    const out = doc.createElement('canvas');
    out.width = Math.round(s.W * s.sx); out.height = Math.round(s.H * s.sy);
    const x = out.getContext('2d');
    for (const alpha of [false, true]) {
        const st = doc.createElement('canvas');
        paintStrip(st, { alpha, state: 0, hasEnv: true, frac: 0.5, n, nowK: n >> 1, nowBin: null, w, env }, cv);
        for (const fine of [true, false]) {
            x.save();
            x.imageSmoothingEnabled = fine;
            if (fine) x.imageSmoothingQuality = 'high';
            x.drawImage(st, fine ? 0.4 : 0, 0);
            x.restore();
        }
    }
    x.setTransform(s.sx, 0, 0, s.sy, 0, 0);
    overlay(x, s.W, s.H, s.W * 0.6, s.W * 0.2, 0.5);
    globalThis.createImageBitmap?.(out).then(b => b.close(), () => {});
    trace?.note('view warmed');
    return true;
}

/// Clear the background once (HP off, BIT-PERFECT, nothing blends).
export function hpViewClear() {
    last = null; shown = null; strip = null; pending = 0; sentAt = -Infinity; asked++;
    baked.key = null; clock.key = null; caught = {};
    if (!drawn) return;
    drawn = false;
    const cv = unit()?.querySelector('canvas.hpbg');
    if (cv) context(cv);
}

/// The player paused or the window went: no frames come till it plays
/// again, the picture stays, its clock stops where it was.
export function hpViewHold() {
    clockHold(clock, lastTs);
    if (trace && clock.held != null && !holdTold) { holdTold = true; trace.clock('hold', 'player'); }
}

/// The player's status says Hybrid-Phase's own work is on its way (the
/// arming's hpAhead): the chain heard meanwhile blends nothing, and its
/// empty answers leave the picture there is as it goes, rather than blank
/// the unit (Anton 1.11: 30M → 1G, ~6 s) — a long wait's fill covers it,
/// and the pair's own picture follows.
export function hpViewOnItsWay(on) { onItsWay = !!on; }

/// One screen frame while HP is on and something plays (player.js lamp
/// loop). `ask(query)` fetches the route and gives its bytes; null: the
/// route is not to be asked now (the protocol pausing after a failure) —
/// the picture goes on on its clock. Every VIEW_EVERY_MS one request (never
/// two at a time but for one given up on); every frame the last answer's
/// strip, slid to the heard frame on viewClock's clock. A late, failed or
/// stray empty answer (VIEW_KEEP_MS) leaves the picture as it goes: the
/// last one holds the bins up to VIEW_AHEAD_MS past NOW.
export function hpViewStep(ts, ask, now = () => performance.now()) {
    const u = unit();
    const cv = u?.querySelector('canvas.hpbg'), lamp = u?.querySelector('.lamp');
    // Out of sight (its page or tab hidden): no frames; back in sight the
    // picture stands where the sound is, not caught up with over a second.
    if (!cv || !lamp || !cv.offsetWidth) { caught = {}; return; }
    lastTs = ts;
    const xN = nowX(cv, lamp), t = now();
    if (ask && (!pending || t - pending > VIEW_GIVE_UP_MS) && t - sentAt >= VIEW_EVERY_MS) {
        const q = viewQuery(cv.offsetWidth, xN), gen = ++asked;
        pending = sentAt = t;
        ask(q).then(buf => {
            if (gen !== asked) return;
            pending = 0;
            const v = bakePast(baked, parseView(buf), `${q.binMs}`), got = now();
            const kept = !v && last && (onItsWay || got - goodAt < VIEW_KEEP_MS);
            trace?.answer(got - t, v ? 'ok' : kept ? 'kept' : 'empty', got);
            if (kept) return;
            if (v) { goodAt = got; hpSwitchFresh(t); }
            last = v ? { v, binMs: q.binMs, xN } : null;
            const k = viewClock(clock, v, t, got, q.binMs);
            // A new grid or a jump: drawn where the clock is at once.
            if (k === 'start' || k === 'snap') caught = {};
            if (k && trace) trace.clock(k, `answer ${Math.round(got - t)}ms`);
        }).catch(() => { if (gen === asked) { pending = 0; trace?.answer(now() - t, 'failed', now()); } });
    }
    if (!last || !clock.key) { shown = null; if (drawn) { drawn = false; context(cv); } return; }
    // Slid to where the clock says the heard frame is now (after missed
    // frames, catching up with it over a few).
    const { v } = last;
    const pos = catching ? catchUp(caught, ts, clockPos(clock, ts)) : clockPos(clock, ts);
    if (caught.began) { trace?.note(`catch-up ${caught.began.lag.toFixed(1)}px over ${Math.round(caught.began.ms)}ms after a ${Math.round(caught.began.gap)}ms frame`); caught.began = null; }
    shown = { v, xN: last.xN, shift: pos - (v.nowBin + v.frac) };
    const at = paint(cv, u);
    drawn = true;
    // The trace follows the picture, not the strip: a new answer's strip starts at another bin.
    if (trace) { trace.frame(ts, pos, at == null ? null : at - (v.nowBin - v.nowK) * (strip?.sx ?? 1)); holdTold = clock.held != null; }
}
