// Snapshots of the analyzer's spectrum: a curve is frozen only when it has one.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// A file window gets no measured O on the spectrum: its layer is all NaN.
// Snapshot answered "Captured 1" and put up a chip over nothing.

import test from 'node:test';
import assert from 'node:assert/strict';

// snapshot.js listens for its keys on the document: a stand-in is enough.
globalThis.document ??= { addEventListener() {}, removeEventListener() {} };

const { create, hasCurve, MAX_SNAPSHOTS } = await import('../src/js/analytics/snapshot.js');

function setup(oData) {
    const state = {
        chain: { rev: 1, tokens: '1M·ISP' },
        layers: { o_psd: oData ? { data: Float32Array.from(oData), minF: 20, maxF: 22050, chainRev: 1 } : null },
        snapshots: [],
    };
    const store = { ...state, set(k, v) { store[k] = v; } };
    return { store, snap: create({ store, bus: new EventTarget() }) };
}

test('a curve of NaN only is not a curve', () => {
    assert.equal(hasCurve({ data: new Float32Array([NaN, NaN, NaN]) }), false);
    assert.equal(hasCurve({ data: new Float32Array([NaN, -60, NaN]) }), true);
    assert.equal(hasCurve(null), false);
    assert.equal(hasCurve({ data: null }), false);
});

test('a snapshot of an all-NaN O: "No curve yet", no chip', () => {
    const { store, snap } = setup([NaN, NaN, NaN, NaN]);
    const r = snap.request();   // no spectrum view here: the whole-track layers
    assert.equal(r.snap, undefined);
    assert.equal(r.reason, 'empty');
    assert.equal(store.snapshots.length, 0);
    snap.destroy();
});

test('a snapshot of a measured O is kept, frozen', () => {
    const { store, snap } = setup([-80, -60, NaN, -70]);
    const r = snap.request();
    assert.ok(r.snap, 'captured');
    assert.equal(store.snapshots.length, 1);
    store.layers.o_psd.data[0] = 0;   // new data arrives
    assert.equal(r.snap.data.o_psd.data[0], -80, 'the snapshot keeps its own copy');
    snap.destroy();
});

test('eight at most, and a removed one gives its colour back', () => {
    const { store, snap } = setup([-60, -60]);
    const colours = [];
    for (let i = 0; i < MAX_SNAPSHOTS; i++) colours.push(snap.request().snap.color);
    assert.equal(new Set(colours).size, MAX_SNAPSHOTS);
    assert.equal(snap.request().reason, 'full');
    snap.removeById(store.snapshots[2].id);
    assert.equal(snap.request().snap.color, colours[2]);
    snap.destroy();
});

test('the spectrum view answers the request with the curve it shows', () => {
    const { store, snap } = setup([NaN]);
    const bus = new EventTarget();
    const s2 = create({ store, bus });
    bus.addEventListener('an:snapshot:capture', (e) => {
        e.detail.handled = true;
        const r = s2.capture({ o_psd: { data: new Float32Array([-50, -55]) } }, 'live · instant');
        e.detail.snap = r.snap || null;
        e.detail.reason = r.reason || null;
    });
    const r = s2.request();
    assert.ok(r.snap);
    assert.equal(r.snap.what, 'live · instant');
    snap.destroy();
    s2.destroy();
});
