// HP's unit background: the route's answer, the request, and where a bin lands.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The answer's layout is PhaseMeter::view's (phase_meter.rs): kind, state,
// has_env, frac, n (u16 LE), now_k (u16 LE), n weights, n envelope bytes.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { parseView, viewQuery, binX, viewThr, thrStops, bakePast, drawView, paintStrip, drawStrip, stripX, nowX, crossSegs, catchUp, VIEW_SPAN_MS, VIEW_AHEAD_MS } from '../src/js/hpview.js';

function answer({ kind = 1, state = 0, env = 1, frac = 64, now = 3, w = [0, 0, 255, 255, 0], e = null, step = null, bin = null }) {
    const n = w.length;
    e ??= [10, 20, 30, 40, 50, 60].slice(0, n).concat(new Array(Math.max(0, n - 6)).fill(0));
    const tail = step == null && bin == null ? [] : [(step ?? 0) & 255];
    if (bin != null) tail.push(bin & 255, bin >>> 8 & 255, bin >>> 16 & 255, bin >>> 24 & 255);
    return new Uint8Array([kind, state, env, frac, n & 255, n >> 8, now & 255, now >> 8, ...w, ...e, ...tail]).buffer;
}

test('an answer reads back as bins of weight and envelope', () => {
    const v = parseView(answer({}));
    assert.equal(v.alpha, false);
    assert.equal(v.hasEnv, true);
    assert.equal(v.frac, 0.25);
    assert.equal(v.n, 5);
    assert.equal(v.nowK, 3);
    assert.deepEqual([...v.w], [0, 0, 255, 255, 0]);
    assert.deepEqual([...v.env], [10, 20, 30, 40, 50]);
});

test('the byte after the bins is the step in effect, signed', () => {
    assert.equal(parseView(answer({})).step, null, 'an answer without it');
    assert.equal(parseView(answer({ step: 0 })).step, 0);
    assert.equal(parseView(answer({ step: 2 })).step, 2);
    const v = parseView(answer({ step: -3 }));
    assert.equal(v.step, -3);
    assert.deepEqual([...v.env], [10, 20, 30, 40, 50], 'the bins keep their places');
    assert.equal(v.nowBin, null, 'an answer without the grid bin');
});

test('last, the grid bin of the heard frame, u32 LE', () => {
    const v = parseView(answer({ step: -1, bin: 0x01020304 }));
    assert.equal(v.step, -1);
    assert.equal(v.nowBin, 0x01020304);
    assert.equal(parseView(answer({ step: 0, bin: 0xfffffffe })).nowBin, 0xfffffffe, 'unsigned');
});

test('baked past: the bins heard stay as drawn when the step changes, the rest follow it', () => {
    const c = { key: null, now: null, map: new Map() };
    // NOW in bin 3 (grid 103): bins 0..3 heard, 4.. to come.
    const a = bakePast(c, parseView(answer({ now: 3, bin: 103, w: [10, 20, 30, 40, 50, 60] })), '4');
    assert.deepEqual([...a.w], [10, 20, 30, 40, 50, 60], 'the first answer as it is');
    // A new chain at another step redraws the whole track: one bin later, all different.
    const b = bakePast(c, parseView(answer({ now: 3, bin: 104, w: [1, 2, 3, 4, 5, 6] })), '4');
    assert.deepEqual([...b.w], [20, 30, 40, 4, 5, 6], 'grid 101..103 as drawn, 104 (now) and later the new step');
    const d = bakePast(c, parseView(answer({ now: 3, bin: 105, w: [9, 9, 9, 9, 9, 9] })), '4');
    assert.deepEqual([...d.w], [30, 40, 4, 9, 9, 9], 'bin 104 was baked at its first sight');
    assert.ok(!c.map.has(101), 'scrolled off: forgotten');
    // A seek back (or the next track) starts over.
    const e = bakePast(c, parseView(answer({ now: 3, bin: 50, w: [7, 7, 7, 7, 7, 7] })), '4');
    assert.deepEqual([...e.w], [7, 7, 7, 7, 7, 7]);
    // A seek ahead past the window starts over too.
    const f = bakePast(c, parseView(answer({ now: 3, bin: 500, w: [8, 8, 8, 8, 8, 8] })), '4');
    assert.deepEqual([...f.w], [8, 8, 8, 8, 8, 8]);
    // Another bin size (the unit resized) or the other kind: another grid.
    const g = bakePast(c, parseView(answer({ now: 3, bin: 501, w: [6, 6, 6, 6, 6, 6] })), '5');
    assert.deepEqual([...g.w], [6, 6, 6, 6, 6, 6]);
    const h = bakePast(c, parseView(answer({ kind: 2, now: 3, bin: 502, w: [5, 5, 5, 5, 5, 5] })), '5');
    assert.deepEqual([...h.w], [5, 5, 5, 5, 5, 5]);
    // The answer itself is left alone; an answer without the bin passes through.
    const raw = parseView(answer({ now: 3, bin: 503, w: [1, 1, 1, 1, 1, 1] }));
    bakePast(c, raw, '5');
    assert.deepEqual([...raw.w], [1, 1, 1, 1, 1, 1]);
    const old = parseView(answer({ step: 0 }));
    assert.equal(bakePast(c, old, '5'), old);
    assert.equal(bakePast(c, null, '5'), null);
});

test('the player bakes every answer and forgets on clear', () => {
    const src = readFileSync(new URL('../src/js/hpview.js', import.meta.url), 'utf8');
    assert.match(src, /const v = bakePast\(baked, parseView\(buf\), /);
    assert.match(src, /export function hpViewClear\(\) \{[^}]*baked\.key = null;/);
});

test('kind 2 is αHP; no envelope yet is said so', () => {
    const v = parseView(answer({ kind: 2, env: 0 }));
    assert.equal(v.alpha, true);
    assert.equal(v.hasEnv, false);
});

test('n and now_k are little-endian u16', () => {
    const w = new Array(300).fill(7), e = new Array(300).fill(9);
    const v = parseView(answer({ now: 258, w, e }));
    assert.equal(v.n, 300);
    assert.equal(v.nowK, 258);
    assert.equal(v.env[299], 9);
});

test('nothing blends: an empty or cut answer is no picture', () => {
    assert.equal(parseView(new ArrayBuffer(0)), null);
    assert.equal(parseView(null), null);
    assert.equal(parseView(answer({}).slice(0, 12)), null);
    assert.equal(parseView(answer({ w: [], e: [] })), null);
});

test('a converted file without its timeline draws nothing', () => {
    assert.equal(parseView(answer({ state: 1 })).state, 1);
    for (const state of [2, 3, 4, 5, 6]) assert.equal(parseView(answer({ state })), null, `state ${state}`);
});

test('the threshold is the slider\'s at once, whatever step the answer still carries', () => {
    let slider = { value: '2' };
    globalThis.document = { getElementById: (id) => (id === 'convHpSens' ? slider : null) };
    try {
        assert.equal(viewThr(), 0.23);
        slider = null;
        assert.equal(viewThr(), 0.30);
    } finally { delete globalThis.document; }
    // the engine's table (hybrid_phase.rs SENS_THRESHOLDS), step −3..+3
    assert.deepEqual([-3, -2, -1, 0, 1, 2, 3].map(s => viewThr(s)), [0.45, 0.40, 0.35, 0.30, 0.26, 0.23, 0.20]);
    assert.equal(viewThr(9), 0.30, 'nonsense is step 0');
});

test('the threshold line fades in from the controls: 0, .08 at 35 %, .40 at 75 %, .65 by NOW, .30 at the edge', () => {
    assert.deepEqual(thrStops(100, 200, 230), [[100, 0], [135, 0.08], [175, 0.40], [198, 0.65], [230, 0.30]]);
});

/// A canvas that records what is drawn on it.
function recCanvas(W = 230, H = 40) {
    const ops = [];
    const grad = (kind, args) => { const g = { kind, args, stops: [] }; g.addColorStop = (o, c) => g.stops.push([o, c]); return g; };
    const ctx = new Proxy({ ops }, {
        get(t, k) {
            if (k === 'ops') return ops;
            if (k === 'createLinearGradient') return (...a) => grad('lin', a);
            if (k in t) return t[k];
            return (...a) => ops.push({ op: k, a, dash: t.dash, stroke: t.strokeStyle, fill: t.fillStyle });
        },
        set(t, k, v) { t[k] = v; return true; },
    });
    ctx.setLineDash = (d) => { ctx.dash = d; };
    ctx.save = () => {}; ctx.restore = () => { ctx.dash = undefined; };
    return { offsetWidth: W, offsetHeight: H, width: 0, height: 0, getContext: () => ctx, ops };
}

for (const kind of [1, 2]) test(`${kind === 1 ? 'HP' : 'αHP'}: the threshold dashed as NOW, no zone fill, nothing scaled by the slider`, () => {
    const cv = recCanvas();
    const v = parseView(answer({ kind, now: 3, w: [0, 128, 255, 255, 64, 0], step: 3 }));
    drawView(cv, v, 200, 0, 100, 0.30);
    const strokes = cv.ops.filter(o => o.op === 'stroke');
    const thr = strokes.find(o => o.stroke?.args?.[3] === 0 && o.stroke.args[2] === 230);
    assert.ok(thr, 'a stroke with a horizontal gradient over the unit');
    assert.deepEqual(thr.dash, [3, 2], 'dashed as NOW');
    assert.deepEqual(thr.stroke.stops.map(([o, c]) => [Math.round(o * 230), c]),
        [[100, 'rgba(226,232,240,0)'], [135, 'rgba(226,232,240,0.08)'], [175, 'rgba(226,232,240,0.4)'], [198, 'rgba(226,232,240,0.65)'], [230, 'rgba(226,232,240,0.3)']]);
    const yT = Math.round(3 + 34 * 0.7) - 0.5;
    const at = (op, x, y) => cv.ops.some(o => o.op === op && o.a[0] === x && o.a[1] === y);
    assert.ok(at('moveTo', 198, yT) && at('lineTo', 100, yT), 'out from NOW to the controls\' edge at the threshold');
    assert.ok(at('moveTo', 203, yT) && at('lineTo', 230, yT), 'and out to the right edge');
    assert.ok(at('moveTo', 200.5, yT - 2.5) && at('moveTo', 200.5, yT + 2.5), 'NOW in two halves from the crossing');
    const sq = cv.ops.findLast(o => o.op === 'fillRect');
    assert.deepEqual([sq.a, sq.fill], [[200, yT - 0.5, 1, 1], 'rgba(148,163,184,.85)'], 'a grey pixel where they cross, drawn last');
    // drawn after the vignette, before NOW
    const vig = cv.ops.findLastIndex(o => o.op === 'fillRect' && o.fill?.args?.[2] === 0);
    const iThr = cv.ops.indexOf(thr);
    assert.ok(vig >= 0 && vig < iThr, 'after the vignette');
    assert.ok(strokes.indexOf(thr) === strokes.length - 2, 'NOW is the last stroke');
    assert.ok(!cv.ops.some(o => o.op === 'fillRect' && /56,189,248/.test(o.fill)), 'no cyan zone');
    if (kind === 2) {
        const cols = cv.ops.filter(o => o.op === 'fillRect' && /^rgba\(232,121,249,0\.(?!05)/.test(o.fill));
        assert.deepEqual(cols.map(o => o.fill), ['rgba(232,121,249,0.311)', 'rgba(232,121,249,0.620)', 'rgba(232,121,249,0.620)', 'rgba(232,121,249,0.156)'],
            'α as the engine made it, whatever the slider says');
    }
});

test('the slider moving repaints the picture on screen (a pause too)', () => {
    const src = readFileSync(new URL('../src/js/hpview.js', import.meta.url), 'utf8');
    assert.match(src, /addEventListener\?\.\('change'[^\n]*convHpSens[^\n]*hpViewRepaint\(\)/);
    const rack = readFileSync(new URL('../src/js/dsprack.js', import.meta.url), 'utf8');
    assert.match(rack, /el\.dispatchEvent\(new Event\('change', \{ bubbles: true \}\)\)/, 'setHpSens says so');
});

test('the request: one bin per pixel, from 2 px left of the edge to three seconds past NOW', () => {
    const q = viewQuery(186, 163);
    assert.equal(q.binMs, VIEW_SPAN_MS / 186);
    assert.equal(q.n, 165 + 279);
    assert.equal(q.fromMs, -165 * q.binMs);
    assert.ok(Math.abs((q.toMs - q.fromMs) - q.n * q.binMs) < 1e-9);
    assert.ok(Math.abs(q.toMs - VIEW_AHEAD_MS) < 1e-9, 'a late answer still has the next three seconds');
    // The route takes 1..1024 bins over at most 10 s.
    assert.equal(viewQuery(5000, 100).n, 1024);
    assert.equal(viewQuery(0, 0).n, 4);
    const wide = viewQuery(1000, 900);
    assert.ok(wide.n === 1024 && wide.toMs > 0, 'fewer ahead on a wide unit, never fewer than the window');
});

test('the heard frame sits on NOW; the picture slides left with time', () => {
    // Bin now_k starts frac of a pixel left of NOW: the heard frame is on it.
    assert.equal(binX(3, 3, 0.25, 163), 162.75);
    assert.equal(binX(3, 3, 0.25, 163) + 0.25, 163);
    assert.equal(binX(0, 3, 0, 163), 160);
    assert.equal(binX(10, 3, 0, 163), 170);
    assert.equal(binX(3, 3, 0, 163, 4.5), 158.5);
});

test('the route the player asks is the one the server answers', () => {
    const main = readFileSync(new URL('../../crates/aura-core/src/protocol.rs', import.meta.url), 'utf8');
    assert.match(main, /path == "\/player\/phase_view"/);
    for (const k of ['ahead_ms=', 'from_ms=', 'to_ms=', 'n=']) assert.ok(main.includes(`num("${k}"`), k);
    const player = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(player, /\/player\/phase_view\?ahead_ms=.*&from_ms=.*&to_ms=.*&n=/s);
});

test('NOW 32 px left of the lamp\'s right edge (v17)', () => {
    const box = (left, width) => ({ getBoundingClientRect: () => ({ left, right: left + width, width }) });
    assert.equal(nowX(box(10, 200), box(189, 11)), 200 - 32 - 10);
});

test('the crossing: four halves going out from 2.5 px off it, a 1×1 square in the middle', () => {
    const c = crossSegs(150.5, 20.5, 40, 230, 40);
    assert.deepEqual(c.h, [[148, 20.5, 40, 20.5], [153, 20.5, 230, 20.5]]);
    assert.deepEqual(c.v, [[150.5, 18, 150.5, 1], [150.5, 23, 150.5, 39]]);
    assert.deepEqual(c.sq, [150, 20, 1, 1]);
    // each half starts 2 px clear of the square: dash 3 · gap 2 · square 1 · gap 2 · dash 3
    for (const [x0, y0] of [...c.h, ...c.v]) assert.equal(Math.abs(x0 - 150.5) + Math.abs(y0 - 20.5), 2.5);
    for (let y = 3.5; y < 37; y += 1) assert.ok(crossSegs(150.5, y, 40, 230, 40), `any threshold step: y ${y}`);
    assert.equal(crossSegs(150.5, 41.5, 40, 230, 40), null, 'threshold off the canvas: whole lines');
    assert.equal(crossSegs(150.5, 20.5, 160, 230, 40), null, 'threshold starting right of NOW: whole lines');
    assert.deepEqual(crossSegs(150.5, 20.5, 149, 230, 40).h, [[153, 20.5, 230, 20.5]], 'no left half when the controls reach the gap');
});

/// The page's frames at 60 Hz against an engine whose heard frame steps a
/// device period `P` ms at a time; one request every 100 ms (never two at
/// a time), on its way `lat` ms, the frame read anywhere inside the trip.
/// The x of one grid bin per frame.
function replay({ P, lat, frames = 600, binMs = 2000 / 360, seekAt = -1, holdFrom = -1, holdTo = -1, wobble = 0, every = [100, 100] }) {
    const { viewClock, clockPos } = hpviewMod;
    const clk = { key: null }, xs = [];
    let seed = 7, pending = null, jump = 0, sentAt = -Infinity, gap = every[0];
    const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
    const t0 = 1000 + holdFrom * 1000 / 60, t1 = 1000 + holdTo * 1000 / 60;
    // the engine's own time: it stands still from t0 to t1
    const heard = (t) => Math.floor((t - Math.max(0, Math.min(t, t1) - t0)) / P) * P / binMs + jump;
    for (let f = 0; f < frames; f++) {
        const ts = 1000 + f * 1000 / 60;
        if (f === seekAt) jump += 500;
        if (pending && pending.arrive <= ts) {
            const p = pending.pos, nowBin = Math.floor(p);
            viewClock(clk, { nowBin, frac: p - nowBin }, pending.sent, pending.arrive, binMs);
            pending = null;
        }
        if (!pending && ts - sentAt >= gap) {
            const l = lat[0] + rnd() * (lat[1] - lat[0]);
            // `wobble` bins either way: the device latency re-measured
            pending = { sent: ts, pos: heard(ts + rnd() * l) + (rnd() * 2 - 1) * wobble, arrive: ts + l };
            sentAt = ts;
            gap = every[0] + rnd() * (every[1] - every[0]);
        }
        if (clk.key) xs.push(100 - clockPos(clk, ts));
    }
    return xs;
}
const steps = (xs) => xs.slice(1).map((x, i) => xs[i] - x);
const sd = (a) => { const m = a.reduce((s, d) => s + d, 0) / a.length; return Math.sqrt(a.reduce((s, d) => s + (d - m) ** 2, 0) / a.length); };
let hpviewMod;
test.before(async () => { hpviewMod = await import('../src/js/hpview.js'); });

for (const P of [10, 21, 46]) for (const lat of [[1, 5], [10, 290]]) test(`even motion: device period ${P} ms, answers ${lat[0]}-${lat[1]} ms on their way`, () => {
    // 360 px over 2 s: 3 px a frame. Anchored to each answer the steps were
    // 0.6..4.2 px (P 10) up to −3..5.3 px with frames going back (P 46).
    const d = steps(replay({ P, lat })).slice(90);
    assert.ok(d.every(s => s > 2.85 && s < 3.15), `steps ${Math.min(...d).toFixed(2)}..${Math.max(...d).toFixed(2)}`);
    assert.ok(sd(d) < 0.08, `sd ${sd(d).toFixed(3)}`);
});

test('a seek jumps the clock at once; a hold stops it, no drift, then it goes on', () => {
    const d = steps(replay({ P: 10, lat: [2, 15], seekAt: 200 })).slice(60);
    assert.equal(d.filter(s => Math.abs(s) > 6).length, 1, 'one jump at the seek');
    assert.ok(d.slice(150).every(s => s > 2.85 && s < 3.15), 'even again right after');
    const h = steps(replay({ P: 10, lat: [2, 15], holdFrom: 200, holdTo: 300 }));
    assert.ok(h.slice(230, 295).every(s => s === 0), 'standing still while held');
    assert.ok(h.every(s => s >= 0 && s < 3.6), 'never back, never a jump: on again it waits where it stopped');
    assert.ok(h.slice(340).every(s => s > 2.4), 'and moves evenly once the sound gets there');
});

test('an engine standing while it plays (a track\'s end, the next one not ready): the picture stops, never runs on and snaps back', () => {
    // As seen under load (HL28): the frame stood at one bin, its fraction
    // wobbling ±4/256 of a bin, answers 250–900 ms apart. Taken for moves,
    // the picture ran on and went back 40–290 px about once a second.
    const binMs = 2000 / 188;
    const h = steps(replay({ P: 10, lat: [10, 30], binMs, frames: 1500, holdFrom: 300, holdTo: 1200, wobble: 4 / 256, every: [250, 900] }));
    assert.ok(h.every(s => s >= 0), `never back: ${Math.min(...h).toFixed(1)} px`);
    assert.ok(h.slice(400, 1190).every(s => s === 0), 'standing still while the engine stands');
    assert.ok(h.slice(1300).every(s => s > 1 && s < 2.2), 'moving evenly once the sound goes on');
});

test('a pause holds the clock where the picture is; playing again goes on from there', () => {
    const { viewClock, clockPos, clockHold } = hpviewMod;
    const binMs = 5, clk = { key: null };
    viewClock(clk, { nowBin: 100, frac: 0 }, 0, 10, binMs);
    viewClock(clk, { nowBin: 120, frac: 0 }, 100, 110, binMs);
    const at = clockPos(clk, 200);
    clockHold(clk, 200);
    assert.equal(clockPos(clk, 5000), at, 'no frames for seconds: stands');
    // Playing again a little later in the sound: no jump back, then on.
    viewClock(clk, { nowBin: Math.ceil(at) + 2, frac: 0 }, 5000, 5010, binMs);
    assert.ok(clockPos(clk, 5011) >= at, 'never back');
    assert.ok(clockPos(clk, 5200) > at + 30, 'moving again');
});

test('the player asks ten times a second, slides by the clock and starts it over on clear', () => {
    const src = readFileSync(new URL('../src/js/hpview.js', import.meta.url), 'utf8');
    assert.match(src, /t - sentAt >= VIEW_EVERY_MS/);
    assert.match(src, /got = now\(\);/);
    assert.match(src, /viewClock\(clock, v, t, got, q\.binMs\)/);
    assert.match(src, /if \(gen !== asked\) return;/, 'an answer to an older request is let go');
    assert.match(src, /export function hpViewClear\(\) \{[^}]*clock\.key = null;/);
    assert.match(src, /export function hpViewHold\(\) \{\s*clockHold\(clock, lastTs\);/);
    const player = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(player, /hpViewClear\(\);\s*else hpViewHold\(\);/, 'a pause or a hidden window holds it');
});

/// The unit on a fake page for hpViewStep: `frames` screen frames 1000/60 ms
/// apart, `answer(i, q)` the route's bytes for request i (null: it fails),
/// `ask(f)` false: the route is not asked on frame f. Gives the strip's x
/// per frame (null: nothing drawn, a blank unit).
async function stepUnit({ frames, answer, ask = () => true }) {
    const cv = recCanvas(188, 38);
    cv.classList = { contains: (c) => c === 'hpbg' };
    cv.getBoundingClientRect = () => ({ left: 0, right: 188 });
    cv.ownerDocument = { createElement: () => recCanvas(188, 38) };
    const lamp = { getBoundingClientRect: () => ({ left: 160, right: 182 }) };
    const u = { querySelector: (s) => (s === 'canvas.hpbg' ? cv : s === '.lamp' ? lamp : null) };
    globalThis.document = { querySelector: () => u, getElementById: () => null };
    const { hpViewStep, hpViewClear } = hpviewMod;
    let ts = 0, asked = 0;
    const now = () => ts, xs = [];
    try {
        hpViewClear();
        for (let f = 0; f < frames; f++) {
            ts = 1000 + f * 1000 / 60;
            const n = cv.ops.length;
            hpViewStep(ts, ask(f) ? (q) => {
                const b = answer(asked++, q, ts);
                return b ? Promise.resolve(b) : Promise.reject(new Error('HTTP 500'));
            } : null, now);
            const d = cv.ops.slice(n).find(o => o.op === 'drawImage');
            xs.push(d ? d.a[1] : null);
            await new Promise(r => setImmediate(r));   // the answer comes before the next frame
        }
        return { xs };
    } finally { delete globalThis.document; }
}
/// A good answer for request q at page time ts: the sound at 1 ms per page ms,
/// every answer's bin 0 grid bin 0, so the strip's x alone tells the motion.
const goodAt = (q, ts) => {
    const pos = ts / q.binMs, nowBin = Math.floor(pos);
    return answer({ now: nowBin, w: new Array(nowBin + 300).fill(128), bin: nowBin, frac: Math.round((pos - nowBin) * 256) % 256 });
};

test('a stray empty answer while it plays: the picture stays and moves on, no blink', async () => {
    const empty = answer({ now: 0, w: [] });
    const { xs } = await stepUnit({ frames: 240, answer: (i, q, ts) => (i === 12 || i === 13 ? empty : goodAt(q, ts)) });
    const d = xs.slice(30);
    assert.ok(d.every(x => x != null), 'drawn every frame');
    assert.ok(steps(d).every(s => Math.abs(s) <= 2), `no jump: ${Math.max(...steps(d).map(Math.abs))} px`);

});

test('a lasting empty answer (nothing blends any more) clears the picture', async () => {
    const empty = answer({ now: 0, w: [] });
    const { xs } = await stepUnit({ frames: 240, answer: (i, q, ts) => (i >= 10 ? empty : goodAt(q, ts)) });
    assert.ok(xs.slice(20, 50).every(x => x != null), 'drawn while good');
    assert.ok(xs.slice(-30).every(x => x == null), 'gone after VIEW_KEEP_MS');
});

test('the pair on its way (a rack change: the chain heard blends nothing for a while): the picture stays, no blank (HL52)', async () => {
    const empty = answer({ now: 0, w: [] });
    hpviewMod.hpViewOnItsWay(true);
    try {
        const { xs } = await stepUnit({ frames: 240, answer: (i, q, ts) => (i >= 10 ? empty : goodAt(q, ts)) });
        assert.ok(xs.slice(20).every(x => x != null), 'drawn on through the empty answers');
    } finally { hpviewMod.hpViewOnItsWay(false); }
    const player = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(player, /hpViewOnItsWay\(!!status\.arming\?\.busy && !!status\.arming\?\.hpAhead\)/);
});

test('failed answers and frames that ask nothing (the protocol pausing) slide on evenly', async () => {
    const { xs } = await stepUnit({ frames: 300, ask: (f) => f < 90 || f > 150,
        answer: (i, q, ts) => (i % 3 === 1 ? null : goodAt(q, ts)) });
    const d = xs.slice(30);
    assert.ok(d.every(x => x != null), 'drawn every frame');
    assert.ok(steps(d).every(s => s >= 0 && s <= 2), `even: ${Math.min(...steps(d))}..${Math.max(...steps(d))} px`);

});

test('player: the spectrum protocol pausing does not stop the HP background', () => {
    const player = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(player, /const on = playing && protoDue\(proto\.spectrum\);/);
    assert.match(player, /if \(playing && status\.audible\?\.source !== 'direct'\) \{ hpViewStep\(ts, null\); return true; \}/);
});

/// The ops that draw the bins: up to the vignette (its fill over the unit
/// from the left edge) if there is one, past the canvas's own
/// setTransform/clearRect.
const binOps = (ops, W) => {
    const i = ops.findIndex(o => o.op === 'fillRect' && o.fill?.args?.join() === `0,0,${W},0`);
    return ops.slice(0, i < 0 ? ops.length : i).filter(o => o.op !== 'setTransform' && o.op !== 'clearRect');
};
const overlayOps = (ops) => ops.slice(ops.findIndex(o => o.op === 'fillRect' && o.fill?.args?.join() === '0,0,230,0')).map(o => [o.op, o.a]);

test('HP: the strip holds the very bins drawView draws, put in place on a whole pixel', () => {
    const v = parseView(answer({ now: 3, w: [0, 0, 255, 255, 0, 255], bin: 50 }));   // frac .25
    const xN = 200, shift = 0.75, x0 = stripX(v, xN, shift, 1);
    assert.equal(x0, binX(0, v.nowK, v.frac, xN, shift));
    assert.equal(x0, 196);
    const ref = recCanvas(), st = recCanvas(0, 0), cv = recCanvas();
    drawView(ref, v, xN, shift, 100, 0.30);
    paintStrip(st, v, cv);
    assert.deepEqual([st.width, st.height], [v.n + 2, 40]);
    const a = binOps(ref.ops, 230), b = binOps(st.ops, v.n + 2);
    assert.ok(a.length > 10);
    assert.deepEqual(b.map(o => o.op), a.map(o => o.op));
    const moved = (o) => (['moveTo', 'lineTo'].includes(o.op) ? [o.a[0] + x0, o.a[1]] : o.a);
    assert.deepEqual(b.map(moved), a.map(o => o.a), 'every point the same, x0 to the right');
    // Each frame: the strip once, on x0, then the vignette and the lines.
    drawStrip(cv, st, v, xN, shift, 100, 0.30);
    const imgs = cv.ops.filter(o => o.op === 'drawImage');
    assert.equal(imgs.length, 1);
    assert.deepEqual(imgs[0].a.slice(1), [196, 0]);
    const iImg = cv.ops.indexOf(imgs[0]);
    assert.ok(!cv.ops.slice(0, iImg).some(o => ['moveTo', 'lineTo', 'fillRect'].includes(o.op)), 'nothing drawn under it');
    assert.deepEqual(overlayOps(cv.ops), overlayOps(ref.ops), 'the same overlay as drawView');
});

test('HP: moving, the strip goes on the fraction of a pixel, smoothed; whole pixels without it', () => {
    const v = parseView(answer({ now: 3, w: [0, 0, 255, 255, 0, 255], bin: 50 }));   // frac .25
    const xN = 200.4, shift = 0.75;
    assert.equal(stripX(v, xN, shift, 1), 196);
    assert.ok(Math.abs(stripX(v, xN, shift, 1, true) - 196.4) < 1e-9);
    assert.ok(Math.abs(stripX(v, xN, shift, 1.25, true) - 196.4 * 1.25) < 1e-9, 'scaled, not rounded');
    const st = recCanvas(0, 0), whole = recCanvas(), fine = recCanvas();
    paintStrip(st, v, whole);
    assert.equal(drawStrip(whole, st, v, xN, shift, 100, 0.30), 196);
    assert.equal(whole.getContext().imageSmoothingEnabled, false);
    const at = drawStrip(fine, st, v, xN, shift, 100, 0.30, true);
    assert.ok(Math.abs(at - 196.4) < 1e-9);
    assert.equal(fine.getContext().imageSmoothingEnabled, true);
    assert.equal(fine.ops.find(o => o.op === 'drawImage').a[1], at);
    assert.deepEqual(overlayOps(fine.ops), overlayOps(whole.ops), 'the same overlay');
});

test('αHP: the strip\'s columns are drawView\'s, x0 to the left', () => {
    const v = parseView(answer({ kind: 2, now: 3, w: [0, 128, 255, 255, 64, 0], bin: 50 }));
    const x0 = stripX(v, 200, 0.75, 1);
    const ref = recCanvas(), st = recCanvas(0, 0), cv = recCanvas();
    drawView(ref, v, 200, 0.75, 100, 0.30);
    paintStrip(st, v, cv);
    const cols = (ops, dx) => ops.filter(o => o.op === 'fillRect' && /^rgba\(232,121,249,0\.(?!05)/.test(o.fill)).map(o => [o.a[0] + dx, o.fill]);
    assert.equal(cols(ref.ops, 0).length, 4);
    assert.deepEqual(cols(st.ops, x0), cols(ref.ops, 0));
});

// HL61: after missed frames the strip catches up over a few frames, not in one jump.
const P = 1000 / 173, SPEED = 0.537 / P;   // Anton's screen: 173 fps, 0.537 px a frame
const frames = (c, ts, p, n) => { const out = []; for (let i = 0; i < n; i++) { ts += P; p += SPEED * P; out.push([ts, p, catchUp(c, ts, p)]); } return { ts, p, out }; };

test('regular frames are drawn exactly on the clock (HL61)', () => {
    const c = {};
    catchUp(c, 0, 10);
    const { out } = frames(c, 0, 10, 300);
    for (const [, p, q] of out) assert.equal(q, p);
});

test('a gap of missed frames is taken back over a second, eased, at most 1 % faster, then exact again (bench 3.11)', () => {
    const c = {};
    catchUp(c, 0, 10);
    let { ts, p, out } = frames(c, 0, 10, 60);
    // 11 frames missed: the clock moved ~6 px in one step.
    const prev = out.at(-1)[2];
    ts += 11 * P; p += SPEED * 11 * P;
    const g = catchUp(c, ts, p);
    assert.ok(Math.abs(g - prev - SPEED * P) < 1e-9, `the gap frame moves as a regular one: ${g - prev}`);
    const lag = p - g, t0 = ts;
    let last = g, maxExtra = 0, back = null;
    while (ts - t0 < 1100) {
        ts += P; p += SPEED * P;
        const q = catchUp(c, ts, p);
        assert.ok(q <= p + 1e-9 && q >= last, `behind the clock, never back: ${q} ${p}`);
        maxExtra = Math.max(maxExtra, (q - last) / (SPEED * P) - 1);
        if (q === p && back == null) back = ts - t0;
        last = q;
    }
    // A short lag (no speed limit would need longer) takes the longest: 1000 ms.
    assert.ok(back >= 1000 - P && back <= 1000 + P, `on the clock again after a second: ${back}`);
    assert.ok(maxExtra < 1.5 * lag / 1000 / SPEED + 1e-9 && maxExtra > 0, `eased, the peak of smoothstep: +${(maxExtra * 100).toFixed(1)} %`);
});

test('a short gap is taken back sooner: 1.5 · lag / (1 % of the speed) (bench 3.11)', () => {
    const c = {};
    catchUp(c, 0, 10);
    let { ts, p } = frames(c, 0, 10, 60);
    // One frame missed: a frame's move behind, 1.5 · P / 1 % ≈ 867 ms at 173 fps.
    ts += 2 * P; p += 2 * SPEED * P;
    const t0 = ts;
    assert.ok(Math.abs(p - catchUp(c, ts, p) - SPEED * P) < 1e-9);
    let back = null;
    while (back == null && ts - t0 < 2000) { ts += P; p += SPEED * P; if (catchUp(c, ts, p) === p) back = ts - t0; }
    assert.ok(Math.abs(back - 150 * P) <= P, `${back} vs ${150 * P}`);
});

test('a move back, a big jump or a fresh start goes at once; a long freeze jumps only past 400 px (bench 3.11)', () => {
    const c = {};
    catchUp(c, 0, 10);
    let { ts, p } = frames(c, 0, 10, 60);
    ts += 5 * P;
    assert.equal(catchUp(c, ts, p - 40), p - 40, 'a seek back');
    ts += 5 * P;
    assert.equal(catchUp(c, ts, p + 2000), p + 2000, 'the next track, far on');
    assert.equal(catchUp({}, 5, 77), 77, 'a new state');
    // Frozen long enough for 600 px: 200 jump now, 400 are caught up.
    const d = {};
    ({ ts, p } = frames(d, 0, 10, 60));
    const q = catchUp(d, ts + 600 / SPEED, p + 600);
    assert.ok(Math.abs(p + 600 - q - 400) < 1e-9, `lag held to 400: ${p + 600 - q}`);
});

test('gaps in a row never leave the strip drifting behind (bench 3.11)', () => {
    const c = {};
    catchUp(c, 0, 10);
    let ts = 0, p = 10;
    for (let i = 0; i < 400; i++) {
        const n = i % 7 === 3 ? 6 : 1;
        ts += n * P; p += SPEED * n * P;
        const q = catchUp(c, ts, p);
        assert.ok(p - q <= 400 && p - q >= 0, `bounded: ${p - q}`);
    }
    // A second of regular frames after the last gap: on the clock again.
    ({ ts, p } = frames(c, ts, p, Math.ceil(1000 / P) + 1));
    assert.equal(catchUp(c, ts + P, p + SPEED * P), p + SPEED * P);
});

test('the unit\'s picture is drawn once off screen as soon as the unit is on screen, before anything plays', async () => {
    const { hpViewWarm } = hpviewMod;
    const cv = recCanvas(0, 0), made = [], bitmaps = [];
    cv.ownerDocument = { createElement: () => { const c = recCanvas(); made.push(c); return c; } };
    const u = { querySelector: (s) => (s === 'canvas.hpbg' ? cv : null) };
    globalThis.document = { querySelector: () => u, getElementById: () => null };
    globalThis.createImageBitmap = (c) => { bitmaps.push(c); return Promise.resolve({ close() {} }); };
    try {
        assert.equal(hpViewWarm(), false, 'not laid out yet: nothing drawn');
        cv.offsetWidth = 188; cv.offsetHeight = 38;
        assert.equal(hpViewWarm(), true);
        assert.equal(hpViewWarm(), false, 'once');
        const [out, ...strips] = made;
        assert.equal(strips.length, 2, 'both kinds\' bins');
        assert.ok(strips.every(s => s.ops.some(o => o.op === 'fillRect' || o.op === 'fill')), 'the bins painted');
        assert.equal(out.ops.filter(o => o.op === 'drawImage').length, 4, 'each strip smoothed and on whole pixels');
        assert.ok(out.ops.some(o => o.op === 'stroke' && o.dash), 'the dashed lines');
        assert.ok(out.ops.some(o => o.op === 'fillRect' && o.fill?.kind === 'lin'), 'the vignette');
        assert.deepEqual(bitmaps, [out], 'taken as a bitmap: drawn on the card now');
        assert.equal(cv.ops.length, 0, 'the unit on screen untouched');
    } finally { delete globalThis.document; delete globalThis.createImageBitmap; }
    const player = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(player, /function takeStatus[\s\S]*?hpViewWarm\(\);/, 'every status looks, the first one with the unit on screen draws');
});
