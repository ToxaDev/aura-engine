// The note under a scene with instruments while they are being found
// (vis/instr-note.js, placed by waves.js): shown while the scene draws the
// mix's stand-in and the instruments are on their way, gone as they come for
// what is heard — not at the end of the work, and back where the work has
// not reached; never without the pack.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { instrumentsNote, notePlace, NOTE_TEXT, NOTE_SUB } from '../src/js/vis/instr-note.js';

const base = { scene: true, state: 'playing', progress: 0.42, busy: 1, source: 1, count: 0, stream: false };
const note = o => instrumentsNote({ ...base, ...o });

test('the analysis under way, the mix drawn: the share done and what is shown meanwhile', () => {
    assert.deepEqual(note({}), { text: 'Separating instruments… 42%', sub: 'Showing the mix for now' });
    assert.equal(NOTE_TEXT, 'Separating instruments…');
    assert.equal(NOTE_SUB, 'Showing the mix for now');
    assert.deepEqual(note({ state: 'paused' }), note({}), 'paused: the same');
    // the share is never rounded up to a whole that is not there
    assert.equal(note({ progress: 0.999 }).text, 'Separating instruments… 99%');
    assert.equal(note({ progress: 0 }).text, 'Separating instruments… 0%');
});

test('no pack: nothing is on its way, no note', () => {
    assert.equal(note({ progress: -1 }), null);
    assert.equal(note({ progress: -1, busy: 0 }), null);
});

test('the instruments came for what is heard: the note goes, whatever the share', () => {
    assert.equal(note({ source: 2, count: 13 }), null);
    assert.equal(note({ source: 2, count: 13, progress: 0.3 }), null, 'before the end of the work');
    // the map's frames are there but the scene has not its instruments yet (count 0): still the stand-in
    assert.deepEqual(note({ source: 2, count: 0 }), { text: 'Separating instruments… 42%', sub: '' });
});

test('a place the work has not reached (a seek ahead on a stream): the note again', () => {
    assert.deepEqual(note({ source: 1, count: 13, stream: true, progress: 0 }), { text: 'Separating instruments…', sub: 'Showing the mix for now' });
});

test('a live stream: no share of a whole', () => {
    assert.equal(note({ stream: true }).text, 'Separating instruments…');
});

test('nothing yet to draw: the first line only', () => {
    assert.deepEqual(note({ source: 0 }), { text: 'Separating instruments… 42%', sub: '' });
});

test('no note for a scene without instruments, a stopped player, or work that will bring none', () => {
    assert.equal(note({ scene: false }), null);
    assert.equal(note({ state: 'stopped' }), null);
    assert.equal(note({ busy: 0 }), null, 'made, failed, or stopped for good');
});

test('the note stands level with the row of switches, on one line where two would reach the text', () => {
    const h = { twoH: 27, oneH: 14 };
    // room to spare (the band over the seek bar), and the whole screen (no text over it)
    assert.deepEqual(notePlace({ floor: 400, ceiling: 300, ...h }), { lift: 6, one: false });
    assert.deepEqual(notePlace({ floor: 900, ceiling: null, ...h }), { lift: 6, one: false });
    // a low big player, a title on two lines and a long device: 24 px between the text and the seek bar
    const low = notePlace({ floor: 400, ceiling: 376, ...h });
    assert.deepEqual(low, { lift: 6, one: true });
    assert.ok(400 - low.lift - h.oneH >= 376, 'its top stays under the text');
    // two lines just fit / just do not
    assert.equal(notePlace({ floor: 400, ceiling: 400 - 36, ...h }).one, false);
    assert.equal(notePlace({ floor: 400, ceiling: 400 - 35, ...h }).one, true);
    // squeezed past that: one line, as low as it goes, still under the text
    const tight = notePlace({ floor: 400, ceiling: 382, ...h });
    assert.equal(tight.one, true);
    assert.ok(tight.lift >= 1 && 400 - tight.lift - h.oneH >= 382, JSON.stringify(tight));
});

// ── from the span's head to the note (music.js) ──────────────────────

let T = 1000;
Object.defineProperty(globalThis, 'performance', { value: { now: () => T }, configurable: true, writable: true });
const { createMusic } = await import('../src/js/vis/music.js');

const FPS = 44100 / 512, SLOTS = 32, VALS = 16;

/// The spatial span as spatial/mod.rs answers it: fourteen head fields (the
/// last the instruments on their way), then frames around what is heard.
function spatialPlayer() {
    const p = { atS: 30, t0: T, progress: 0.2, source: 1, mapId: 0, busy: 1, count: 3, asks: [] };
    const atS = () => p.atS + (T - p.t0) / 1000;
    globalThis.fetch = async url => {
        const u = new URL(url);
        p.asks.push(u.pathname);
        if (u.pathname === '/player/spatial_span') {
            const k0 = Math.floor((atS() - 1) * FPS), n = 2 * Math.ceil(FPS);
            const buf = new ArrayBuffer(14 * 8 + n * SLOTS * VALS * 2), dv = new DataView(buf);
            [atS(), FPS, k0, n, 30000, p.progress, 1, p.source, 0, 7, SLOTS, VALS, p.mapId, p.busy]
                .forEach((v, i) => dv.setFloat64(i * 8, v, true));
            return { ok: true, arrayBuffer: async () => buf };
        }
        if (u.pathname === '/player/spatial_map') {
            const objects = Array.from({ length: p.count }, (_, i) => ({ kind: 1, colour: [0.5, 0.5, 0.5], x: i / 4 }));
            return { ok: true, json: async () => ({ count: p.count, objects, notes: [] }) };
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
const noteOf = m => instrumentsNote({ scene: true, state: 'playing', progress: m.spatialProgress, busy: m.spatialBusy,
    source: m.spatial, count: m.objCount, stream: false });

test('the span says the instruments are on their way, then that they came: the note shows, then goes', async () => {
    const p = spatialPlayer();
    const music = createMusic({ status: () => ({ state: 'playing' }), delayMs: () => 0 });
    music.attach(sink());
    let m = await play(music, 1200);
    assert.equal(m.spatialBusy, 1);
    assert.equal(m.spatial, 1, 'the heard objects from the mix');
    assert.deepEqual(noteOf(m), { text: 'Separating instruments… 20%', sub: 'Showing the mix for now' });
    // the track's map made: its id in the head, the frames the instruments', the work over
    Object.assign(p, { progress: 1, source: 2, mapId: 77, busy: 0 });
    m = await play(music, 1200);
    assert.ok(p.asks.includes('/player/spatial_map'), 'the map is asked for');
    assert.equal(m.objCount, 3);
    assert.equal(noteOf(m), null);
    // the work ended without a map: no note left turning
    Object.assign(p, { progress: 0.45, source: 1, mapId: 0, busy: 0 });
    m = await play(music, 1200);
    assert.equal(noteOf(m), null);
});

test('a span from before the field: the note while the share is short of the whole', async () => {
    const p = spatialPlayer();
    const real = globalThis.fetch;
    globalThis.fetch = async url => {
        const r = await real(url);
        if (!new URL(url).pathname.endsWith('spatial_span')) return r;
        const b = await r.arrayBuffer();
        // the same answer with a thirteen-field head (no "on their way")
        const n = (b.byteLength - 14 * 8) / (SLOTS * VALS * 2), old = new ArrayBuffer(b.byteLength - 8);
        new Uint8Array(old).set(new Uint8Array(b, 0, 13 * 8));
        new Uint8Array(old).set(new Uint8Array(b, 14 * 8), 13 * 8);
        assert.equal((old.byteLength - 13 * 8) / (SLOTS * VALS * 2), n);
        return { ok: true, arrayBuffer: async () => old };
    };
    const music = createMusic({ status: () => ({ state: 'playing' }), delayMs: () => 0 });
    music.attach(sink());
    let m = await play(music, 1200);
    assert.equal(m.spatialBusy, 1);
    p.progress = 1;
    m = await play(music, 1200);
    assert.equal(m.spatialBusy, 0);
});
