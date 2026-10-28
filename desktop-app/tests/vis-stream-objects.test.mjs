// The instruments of a live stream as the scenes get them (vis/music.js,
// spatial::live): the frames come only as far as their values are known, the
// window is asked for again when frames given out before were made again
// (span's head[14], "the frames' make"), and the song's map grows — its id
// moves with each version, and the map is asked for again.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

let T = 1000;
Object.defineProperty(globalThis, 'performance', { value: { now: () => T }, configurable: true, writable: true });

const { createMusic } = await import('../src/js/vis/music.js');

const FPS = 44100 / 512, SLOTS = 32, VALS = 16, HEAD = 15;

/// The player's side: a stream heard from `startS`, its objects known up to
/// `known` frames ahead of the heard one, the map `mapId` (0: the mix
/// stands in) with `count` instruments, and the frames' make.
function fakeStream() {
    const p = { t0: T, startS: 40, ahead: 120, mapId: 0x80070000, count: 3, made: 1000, origin: 2, asks: [], maps: [] };
    p.heardS = () => p.startS + (T - p.t0) / 1000;
    p.span = u => {
        const from = +u.searchParams.get('from_ms'), to = +u.searchParams.get('to_ms');
        const at = p.heardS(), h = Math.floor(at * FPS);
        const f0 = Math.max(0, Math.floor((at + from / 1000) * FPS));
        const f1 = Math.min(Math.ceil((at + to / 1000) * FPS), h + p.ahead);
        const n = Math.max(0, f1 - f0);
        p.asks.push({ at: T, from, f0, n });
        const buf = new ArrayBuffer(HEAD * 8 + n * SLOTS * VALS * 2), dv = new DataView(buf);
        const head = [at, FPS, f0, n, h + 1000, 0, 1, p.origin, 0, 2 ** 50 + 7, SLOTS, VALS, p.origin === 2 ? p.mapId : 0, 0, p.made];
        head.forEach((x, i) => dv.setFloat64(i * 8, x, true));
        for (let j = 0; j < n; j++) for (let s = 0; s < p.count; s++) dv.setUint16(HEAD * 8 + (j * SLOTS + s) * VALS * 2, 65535, true);
        return buf;
    };
    globalThis.fetch = async url => {
        const u = new URL(url);
        if (u.pathname === '/player/spatial_span') {
            const buf = p.span(u);
            return { ok: true, status: 200, arrayBuffer: async () => buf };
        }
        if (u.pathname === '/player/spatial_map') {
            p.maps.push(+u.searchParams.get('id'));
            const objects = Array.from({ length: p.count }, (_, i) => ({ kind: 3 + i, name: `o${i}`, stem: 0, x: 0, width: 0.2, colour: [0.5, 0.5, 0.5] }));
            return { ok: true, status: 200, json: async () => ({ count: p.count, objects, notes: [], kit: 'bands' }) };
        }
        return { ok: false, status: 404 };
    };
    return p;
}

const sink = () => ({ clearSlices() {}, setSlice() {}, dropSlice() {}, setValue() {}, clearWave() {}, setWave() {},
    dropWave() {}, clearObjects() {}, setObjects() {}, clearNotes() {}, setNotes() {}, setCover() {} });
const flush = () => new Promise(r => setImmediate(r));

async function play(music, ms) {
    let m = null;
    for (const end = T + ms; T < end;) {
        T += 16;
        m = music.frame(0.016, { spatial: true });
        await flush();
    }
    return m;
}

function setup() {
    const p = fakeStream();
    const music = createMusic({ status: () => ({ state: 'playing', radio: { stopped: false } }), delayMs: () => 0 });
    music.attach(sink());
    return { p, music };
}

test('a stream: the objects come as far as they are known, then on from the first frame not had', async () => {
    const { p, music } = setup();
    await play(music, 1500);
    const d = music.debug().objects;
    assert.ok(d.frames > 0 && d.source === 2, 'the instruments are drawn');
    const later = p.asks.slice(1);
    assert.ok(later.length > 3 && later.every(a => a.from > -6000), 'no whole window asked for again while nothing was made again');
    assert.equal(music.debug().map.count, 3, 'the map came');
});

test("a stream's frames made again (the head's make moves): the whole window is asked for again", async () => {
    const { p, music } = setup();
    await play(music, 1500);
    const at = T;
    p.made = 4000;
    await play(music, 1500);
    const again = p.asks.filter(a => a.at > at && a.from === -6000);
    assert.equal(again.length, 1, 'asked for again once, from the window\'s start');
});

test("a stream's map grows: a new version is asked for, and the scenes get its instruments", async () => {
    const { p, music } = setup();
    await play(music, 1500);
    assert.deepEqual(p.maps, [p.mapId]);
    p.count = 5;
    p.mapId += 1;
    p.made = 7000;
    const m = await play(music, 1500);
    assert.deepEqual(p.maps.slice(-1), [p.mapId], 'the new version asked for');
    assert.equal(m.objCount, 5);
});

test('the mix stands in again (the map\'s id is none): the scenes have no instruments then', async () => {
    const { p, music } = setup();
    await play(music, 1500);
    p.origin = 1;
    p.made = 10000;
    const m = await play(music, 1500);
    assert.equal(m.objCount, 0, 'no map while the mix stands in');
    assert.equal(music.debug().objects.source, 1);
});
