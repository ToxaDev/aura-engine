// What the sliders open on, given what is on disk.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// This is the logic a first-time user meets before anything else. If it picks a
// combination the installation has no filter for, the very first thing the app
// does is fail — which is precisely the experience the ready-made bundles exist
// to remove. The cases below are the shapes real downloads produce.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    inventory, comboAvailable, tapsAvailable, bestCombo, nearestAvailable,
    TAP_PRESETS, FS_PRESETS
} from '../src/js/inventory.js';

const M1 = 1_000_000, M5 = 5_000_000, M10 = 10_000_000, M30 = 30_000_000;
const FS8_RATES = [352800, 384000];
const ALL_RATES = [88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000];

/// Load the module's inventory with a hand-built matrix, the way the backend
/// would after scanning a particular set of extracted packs.
function install(spec) {
    inventory.cells.clear();
    inventory.packs.clear();
    inventory.known = true;
    for (const [taps, rates, phases = 'both'] of spec) {
        const byRate = new Map();
        for (const rate of rates) {
            byRate.set(rate, {
                linear: phases !== 'minimum',
                minimum: phases !== 'linear'
            });
        }
        inventory.cells.set(taps, byRate);
    }
}

test('a fresh install opens on the filter its bundle shipped', (t) => {
    // Exactly what the Standard bundle unpacks to.
    install([[M10, FS8_RATES]]);
    assert.deepEqual(bestCombo(false), { taps: M10, fs: 8 });

    // ...and the Starter and Reference ones.
    install([[M1, ALL_RATES]]);
    assert.deepEqual(bestCombo(false), { taps: M1, fs: 8 });
    install([[M30, FS8_RATES]]);
    assert.deepEqual(bestCombo(false), { taps: M30, fs: 8 });
});

test('two bundles unzipped over each other offer the larger filter', () => {
    install([[M1, ALL_RATES], [M10, FS8_RATES]]);
    assert.deepEqual(bestCombo(false), { taps: M10, fs: 8 });
    assert.equal(tapsAvailable(M1, false), true);
    assert.equal(tapsAvailable(M10, false), true);
    assert.equal(tapsAvailable(M30, false), false);
});

test('a full matrix still opens on 30M at FS8, as it always did', () => {
    install([[M1, ALL_RATES], [M5, ALL_RATES], [M10, ALL_RATES], [M30, ALL_RATES]]);
    assert.deepEqual(bestCombo(false), { taps: M30, fs: 8 });
});

test('FS8 beats a larger filter that does not have it', () => {
    // A hand-picked set: 30M generated only at FS2, 10M at the design point.
    install([[M10, FS8_RATES], [M30, [88200, 96000]]]);
    assert.deepEqual(bestCombo(false), { taps: M10, fs: 8 });
});

test('with no FS8 anywhere, the largest filter and the nearest multiplier win', () => {
    install([[M10, [176400, 192000]], [M30, [176400, 192000]]]);
    assert.deepEqual(bestCombo(false), { taps: M30, fs: 4 });
});

test('nothing installed means no recommendation at all', () => {
    install([]);
    assert.equal(bestCombo(false), null);
    assert.equal(comboAvailable(M30, 8, false), false);
});

test('Hybrid-Phase needs the minimum-phase half of the pair', () => {
    install([[M10, FS8_RATES, 'linear']]);
    assert.equal(comboAvailable(M10, 8, false), true);
    assert.equal(comboAvailable(M10, 8, true), false);
    assert.equal(bestCombo(true), null);
});

test('a returning user keeps settings that still work', () => {
    install([[M1, ALL_RATES], [M30, FS8_RATES]]);
    assert.deepEqual(nearestAvailable(M1, 16, false), { taps: M1, fs: 16 });
    assert.deepEqual(nearestAvailable(M30, 8, false), { taps: M30, fs: 8 });
});

test('a stale multiplier moves, and the filter size is what is kept', () => {
    // Saved 30M/FS16 from a full matrix; now only the Reference bundle is here.
    install([[M30, FS8_RATES]]);
    assert.deepEqual(nearestAvailable(M30, 16, false), { taps: M30, fs: 8 });
});

test('a stale filter size moves, and the multiplier is what is kept', () => {
    // Saved 30M/FS8; the user now has the Standard bundle instead.
    install([[M10, FS8_RATES]]);
    assert.deepEqual(nearestAvailable(M30, 8, false), { taps: M10, fs: 8 });
});

test('when neither survives, the fallback is still a working pair', () => {
    install([[M1, [88200, 96000]]]);
    const got = nearestAvailable(M30, 16, false);
    assert.deepEqual(got, { taps: M1, fs: 2 });
    assert.equal(comboAvailable(got.taps, got.fs, false), true);
});

test('one rate family is enough to offer the multiplier', () => {
    // The 30M packs are split by family: a user may hold only the 44.1 half.
    install([[M30, [352800]]]);
    assert.equal(comboAvailable(M30, 8, false), true);
    // The per-file pre-flight is what catches a 48 kHz source against this.
});

test('an unanswered probe marks nothing and moves nothing', () => {
    install([]);
    inventory.known = false;
    // Every combination has to read as available, or the app would relocate a
    // user off settings whose filters are in fact right there.
    for (const taps of TAP_PRESETS) {
        for (const fs of FS_PRESETS) {
            assert.equal(comboAvailable(taps, fs, false), true);
            assert.equal(comboAvailable(taps, fs, true), true);
        }
    }
    assert.deepEqual(nearestAvailable(M30, 8, false), { taps: M30, fs: 8 });
});

test('every recommendation is one the sliders can actually express', () => {
    const shapes = [
        [[M1, ALL_RATES]],
        [[M10, FS8_RATES]],
        [[M30, FS8_RATES]],
        [[M5, [705600, 768000]]],
        [[M1, ALL_RATES], [M5, ALL_RATES], [M10, ALL_RATES], [M30, ALL_RATES]]
    ];
    for (const shape of shapes) {
        install(shape);
        const got = bestCombo(false);
        assert.ok(got, 'a non-empty install must yield a recommendation');
        assert.ok(TAP_PRESETS.includes(got.taps), `tap count off the ladder: ${got.taps}`);
        assert.ok(FS_PRESETS.includes(got.fs), `multiplier off the ladder: ${got.fs}`);
        assert.equal(comboAvailable(got.taps, got.fs, false), true);
    }
});
