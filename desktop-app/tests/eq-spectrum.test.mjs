// The EQ window's analyzer: settings, the engine's reply, ballistics, tilt,
// the level scale and the polling (src/js/eq/spectrum.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
    normalizeSettings, decodeFrame, gridFreqs, tiltDb, ballistic, stepCurve, levelY,
    createAnalyzer, plotEdges, DEFAULTS, SPEEDS, POINTS,
} from '../src/js/eq/spectrum.js';

function frame(n, pre, post, flags = 1) {
    const b = new ArrayBuffer(12 + 8 * n), dv = new DataView(b);
    dv.setUint8(0, 1); dv.setUint8(1, flags); dv.setUint16(2, n, true);
    dv.setUint32(4, 48000, true); dv.setUint32(8, 2048, true);
    for (let i = 0; i < n; i++) {
        dv.setFloat32(12 + 4 * i, pre(i), true);
        dv.setFloat32(12 + 4 * (n + i), post(i), true);
    }
    return b;
}

test('settings: defaults, and only known values kept', () => {
    assert.deepEqual(normalizeSettings(null), { ...DEFAULTS });
    assert.equal(DEFAULTS.mode, 'both');
    assert.equal(DEFAULTS.tilt, 4.5);
    const s = normalizeSettings({ on: true, mode: 'post', res: 'max', speed: 'fast', tilt: '3', range: 120, freeze: true });
    assert.deepEqual(s, { on: true, mode: 'post', res: 'max', speed: 'fast', tilt: 3, range: 120, freeze: true });
    const bad = normalizeSettings({ on: 1, mode: 'x', res: 7, speed: 'warp', tilt: 5, range: 100, freeze: 'yes' });
    assert.deepEqual(bad, { ...DEFAULTS });
});

test('the reply decodes to pre and post; empty is no frame', () => {
    const f = decodeFrame(frame(3, i => 0 - 10 * i, i => 0 - 5 * i));
    assert.equal(f.n, 3); assert.equal(f.rate, 48000); assert.equal(f.fft, 2048); assert.equal(f.tapped, true);
    assert.deepEqual([...f.pre], [0, -10, -20]);
    assert.deepEqual([...f.post], [0, -5, -10]);
    assert.equal(decodeFrame(frame(2, () => 0, () => 0, 0)).tapped, false);
    assert.equal(decodeFrame(new ArrayBuffer(0)), null);
    assert.equal(decodeFrame(frame(4, () => 0, () => 0).slice(0, 30)), null);
});

test('the grid matches the graph axis 10 Hz … 30 kHz, log-spaced', () => {
    const g = gridFreqs(POINTS);
    assert.ok(Math.abs(g[0] - 10) < 1e-9 && Math.abs(g[POINTS - 1] - 30000) < 1e-6);
    const r = g[1] / g[0];
    assert.ok(Math.abs(g[200] / g[199] - r) < 1e-9);
});

test('tilt pivots at 1 kHz', () => {
    assert.equal(tiltDb(1000, 4.5), 0);
    assert.ok(Math.abs(tiltDb(2000, 4.5) - 4.5) < 1e-12);
    assert.ok(Math.abs(tiltDb(250, 6) + 12) < 1e-12);
    assert.equal(tiltDb(20000, 0), 0);
});

test('ballistics: fast attack, release at its speed, frame-rate independent', () => {
    // Rise: 95 % within ~3 time constants.
    assert.ok(ballistic(-90, 0, 0.025, 30) > -4.5);
    // Fall: dB per second, not per frame.
    assert.ok(Math.abs(ballistic(0, -90, 0.1, 30) + 3) < 1e-12);
    let a = 0, b = 0;
    for (let i = 0; i < 60; i++) a = ballistic(a, -90, 1 / 60, SPEEDS.medium);
    for (let i = 0; i < 144; i++) b = ballistic(b, -90, 1 / 144, SPEEDS.medium);
    assert.ok(Math.abs(a - b) < 1e-9 && Math.abs(a + SPEEDS.medium) < 1e-9);
    // Attack too: two half steps equal one whole.
    const h = ballistic(ballistic(-60, 0, 0.01, 30), 0, 0.01, 30);
    assert.ok(Math.abs(h - ballistic(-60, 0, 0.02, 30)) < 1e-9);
    // Never past the target; frozen, nothing falls.
    assert.equal(ballistic(-10, -12, 1, 30), -12);
    assert.equal(ballistic(-10, -50, 1, 30, true), -10);
    assert.ok(SPEEDS.slow < SPEEDS.medium && SPEEDS.medium < SPEEDS.fast);
});

test('with nothing sounding the curve falls to the floor and stops', () => {
    const c = new Float64Array([0, -20, -40]);
    let live = true, steps = 0;
    while (live && steps < 1000) { live = stepCurve(c, null, 0.033, 30, -91); steps++; }
    assert.ok(!live);
    assert.deepEqual([...c], [-91, -91, -91]);
    // 91 dB at 30 dB/s in 33 ms frames: about 3 s.
    assert.ok(steps > 85 && steps < 100, String(steps));
});

test('the level scale: 0 dB at the top, −range at the bottom, clamped', () => {
    assert.equal(levelY(0, 90, 10, 100), 10);
    assert.equal(levelY(-90, 90, 10, 100), 100);
    assert.equal(levelY(-45, 90, 10, 100), 55);
    assert.equal(levelY(6, 90, 10, 100), 10);
    assert.equal(levelY(-200, 120, 10, 100), 100);
});

test('the analyzer polls only while on, persists its settings, falls on an empty reply', async () => {
    const store = new Map();
    const storage = { getItem: k => store.get(k) ?? null, setItem: (k, v) => store.set(k, v) };
    let asked = 0, dirty = 0;
    const reply = frame(POINTS, () => -20, () => -10);
    const an = createAnalyzer({ fetchFrame: async () => { asked++; return reply; }, markDirty: () => dirty++, storage });
    an.start();
    await new Promise(r => setTimeout(r, 20));
    assert.equal(asked, 0, 'off: no polling');
    an.set('tilt', 0);
    an.set('on', true);
    assert.equal(JSON.parse(store.get('auraEqSpectrum')).on, true);
    await new Promise(r => setTimeout(r, 10));
    assert.ok(asked >= 1);
    an.setVisible(false);
    const n = asked;
    await new Promise(r => setTimeout(r, 80));
    assert.equal(asked, n, 'hidden: no polling');
    // A drawn frame: the post line and the pre fill behind it.
    const ops = [];
    const ctx = new Proxy({}, { get: (_, k) => () => { ops.push(k); return { addColorStop() {} }; }, set: () => true });
    an.setVisible(true);
    await new Promise(r => setTimeout(r, 90));
    assert.ok(dirty > 0);
    an.draw(ctx, { xOf: f => Math.log10(f) * 100, plot: { x0: 100, x1: 448, y0: 0, y1: 300 } });
    assert.ok(ops.includes('fill') && ops.includes('stroke'));
    an.stop();
    const again = createAnalyzer({ fetchFrame: async () => null, storage });
    assert.equal(again.settings.on, true);
    assert.equal(again.settings.tilt, 0);
    again.stop();
});

test('the plot edges from the graph view, in its usual shapes', () => {
    const want = { x0: 6, x1: 406, y0: 16, y1: 216 };
    assert.deepEqual(plotEdges({ plot: { x0: 6, x1: 406, y0: 16, y1: 216 } }), want);
    assert.deepEqual(plotEdges({ plot: { x: 6, y: 16, w: 400, h: 200 } }), want);
    assert.deepEqual(plotEdges({ plot: { l: 6, r: 406, t: 16, b: 216 } }), want);
});
