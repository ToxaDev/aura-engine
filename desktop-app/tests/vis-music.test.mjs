// The music as the scenes see it (vis/music.js): the spectrum and the
// waveform keep coming while a file or a stream plays — through a change of
// scene, a scene not drawn for a while, and a new stream under it.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// Anton 3.10: "sometimes, switching the visualization — to Scope, say —
// even while a file plays, the scene takes no sound; I have to stop the
// file and start it again". The waveform (Scope's line) is asked for from
// the last sample it has: after a new stream (another rate family, BIT-
// PERFECT on or off, a stop and a start while the scene was not drawn) the
// player's frames begin again from nought, the old last sample lay minutes
// ahead of them, and the waveform was never asked for again — only a stop
// seen by the scene cleared it.

import test from 'node:test';
import assert from 'node:assert/strict';

// A clock of our own: the music engine paces its asking by performance.now().
let T = 1000;
Object.defineProperty(globalThis, 'performance', { value: { now: () => T }, configurable: true, writable: true });

const { createMusic } = await import('../src/js/vis/music.js');

const SLICE_EXTRA = 8;
const LEVEL = 16000;                  // every sample of the fake sound (≈ 0.49)

/// The player as the scenes' requests see it: one ring at a time, its
/// audible frame moving with the clock.
function fakePlayer() {
    const p = { ring: 11, rate: 384000, base: 0, t0: T, gen: 0, asks: [] };
    p.audible = () => Math.max(0, Math.round(p.base + (T - p.t0) * p.rate / 1000));
    /// A new stream: another ring (`ring`), its frames from `startS` on.
    p.newStream = ({ ring, rate, startS = 0.3 }) => {
        Object.assign(p, { ring, rate: rate ?? p.rate, gen: 0, t0: T });
        p.base = Math.round(startS * p.rate);
    };
    const head = (dv, v) => v.forEach((x, i) => dv.setFloat64(i * 8, x, true));
    p.spectrum = u => {
        const bands = +u.searchParams.get('bands'), from = +u.searchParams.get('from_ms'), to = +u.searchParams.get('to_ms');
        const a = p.audible(), step = Math.round(+u.searchParams.get('step_ms') / 1000 * p.rate);
        const lo = a + Math.min(from, to) / 1000 * p.rate, hi = a + Math.max(from, to) / 1000 * p.rate;
        const k0 = Math.ceil(Math.max(0, lo) / step), k1 = Math.floor(Math.max(0, hi) / step);
        const n = k1 < k0 ? 0 : Math.min(512, k1 - k0 + 1), per = 1 + bands + SLICE_EXTRA;
        const buf = new ArrayBuffer(80 + n * per), dv = new DataView(buf);
        head(dv, [a, p.rate, step, k0, n, p.ring, p.gen, -1, a + 4 * p.rate, 0]);
        for (let j = 0; j < n; j++) {
            const o = 80 + j * per;
            dv.setUint8(o, 1);
            for (let b = 0; b < bands + SLICE_EXTRA; b++) dv.setUint8(o + 1 + b, 150);
        }
        return buf;
    };
    p.wave = u => {
        const from = +u.searchParams.get('from_ms'), to = +u.searchParams.get('to_ms');
        const a = p.audible(), d = Math.max(1, Math.round(p.rate / +u.searchParams.get('rate')));
        const lo = Math.max(0, a + Math.min(from, to) / 1000 * p.rate), hi = Math.min(a + Math.max(from, to) / 1000 * p.rate, a + 4 * p.rate);
        const i0 = Math.ceil(lo / d), iEnd = Math.floor(hi / d), n = iEnd <= i0 ? 0 : iEnd - i0;
        const buf = new ArrayBuffer(80 + n * 4), dv = new DataView(buf);
        head(dv, [a, p.rate, d, i0, n, p.ring, p.gen, -1, a + 4 * p.rate, 0]);
        for (let j = 0; j < n * 2; j++) dv.setInt16(80 + j * 2, LEVEL, true);
        return buf;
    };
    globalThis.fetch = async url => {
        const u = new URL(url);
        p.asks.push({ at: T, path: u.pathname });
        const make = u.pathname === '/player/spectrum_span' ? p.spectrum : u.pathname === '/player/wave_span' ? p.wave : null;
        if (!make) return { ok: false, status: 404 };
        const buf = make(u);
        return { ok: true, status: 200, arrayBuffer: async () => buf };
    };
    return p;
}

/// A drawing that keeps nothing (renderer.js keeps the textures).
const sink = () => ({ clearSlices() {}, setSlice() {}, dropSlice() {}, setValue() {}, clearWave() {}, setWave() {},
    dropWave() {}, clearObjects() {}, setObjects() {}, clearNotes() {}, setNotes() {}, setCover() {} });

const flush = () => new Promise(r => setImmediate(r));

/// `ms` of frames at 60 a second (`draw`: false — the scene not drawn, as
/// waves.js does then: no frame() at all).
async function play(music, ms, { spatial = false, draw = true } = {}) {
    let m = null;
    for (const end = T + ms; T < end;) {
        T += 16;
        if (draw) m = music.frame(0.016, { spatial });
        await flush();
    }
    return m;
}

/// The waveform as the scene reads it now: the sample at the frame's
/// waveform index (both channels' middle), and the spectrum's level.
const heard = (music, m) => ({ wave: music.waveAt(m.wBase + m.wq, 2), live: m.live, bass: m.bass });

function setup() {
    const p = fakePlayer();
    let state = 'playing';
    const music = createMusic({ status: () => ({ state }), delayMs: () => 0 });
    music.attach(sink());
    p.base = Math.round(30 * p.rate);                // 30 s into the track
    return { p, music, setState: s => { state = s; } };
}

test('the waveform comes while a file plays (the ground every other case stands on)', async () => {
    const { music } = setup();
    const m = await play(music, 1500);
    const h = heard(music, m);
    assert.equal(h.live, 1, 'the spectrum comes');
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `the waveform comes (${h.wave})`);
});

test('a change of scene while a file plays: the sound keeps coming to the scene', async () => {
    const { p, music } = setup();
    await play(music, 1200, { spatial: true });     // Stage (with instruments)
    let m = await play(music, 1200);                // → Scope
    let h = heard(music, m);
    assert.equal(h.live, 1);
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `Scope gets the waveform (${h.wave})`);
    m = await play(music, 1200, { spatial: true }); // → Stage again
    m = await play(music, 1200);                    // → Scope again
    h = heard(music, m);
    assert.equal(h.live, 1);
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `Scope gets the waveform again (${h.wave})`);
    const late = p.asks.filter(a => a.at > T - 600);
    assert.ok(late.some(a => a.path === '/player/wave_span') && late.some(a => a.path === '/player/spectrum_span'),
        'both are still asked for');
});

test('a new stream under a playing scene (another rate family): the waveform is asked for again and comes', async () => {
    const { p, music } = setup();
    await play(music, 2000);
    // The next file is of the other family: a new stream, its frames from
    // nought — the status never says "stopped" in between.
    p.newStream({ ring: 12, rate: 352800 });
    const at = T;
    const m = await play(music, 2000);
    const h = heard(music, m);
    assert.equal(h.live, 1, 'the spectrum comes from the new stream');
    assert.ok(p.asks.some(a => a.at > at && a.path === '/player/wave_span'), 'the waveform is asked for again');
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `the waveform comes again (${h.wave})`);
});

test('a stop and a start while the scene was not drawn: the scene shown again gets the sound', async () => {
    const { p, music, setState } = setup();
    await play(music, 2000);
    // The big player closed: no frames; the file stopped and started again
    // meanwhile (the same rate: a new stream all the same).
    await play(music, 300, { draw: false });
    setState('stopped');
    await play(music, 600, { draw: false });
    setState('playing');
    p.newStream({ ring: 13 });
    await play(music, 1500, { draw: false });
    const m = await play(music, 1500);
    const h = heard(music, m);
    assert.equal(h.live, 1);
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `the waveform comes (${h.wave})`);
});

test("a new stream that took the old one's place in memory (the same identity, its frames begun again)", async () => {
    const { p, music } = setup();
    await play(music, 2000);
    p.newStream({ ring: 11 });
    const at = T;
    const m = await play(music, 2000);
    const h = heard(music, m);
    assert.equal(h.live, 1);
    assert.ok(p.asks.some(a => a.at > at && a.path === '/player/wave_span'), 'the waveform is asked for again');
    assert.ok(Math.abs(h.wave - LEVEL / 32767) < 1e-3, `the waveform comes again (${h.wave})`);
    // The old stream's slices (30 s on) are not kept as if they were the new one's.
    const d = music.debug();
    assert.ok(d.slices < 400, `only the new stream's slices are kept (${d.slices})`);
});
