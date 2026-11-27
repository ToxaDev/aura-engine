// The seek line's amplitude bars (seek-wave.js barHeights).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { barHeights } from '../src/js/seek-wave.js';

const BINS = 1000;
const wave = (f) => ({ peak: Array.from({ length: BINS }, (_, i) => f(i)), rms: Array.from({ length: BINS }, (_, i) => f(i)), durationS: 100 });

test('no slices, no bars', () => {
    assert.equal(barHeights(null, 10), null);
    assert.equal(barHeights({ peak: [], rms: [] }, 10), null);
});

test('the loudest stretch reaches the top, silence keeps a dot', () => {
    // Silent first half, full scale second half.
    const h = barHeights(wave(i => (i < BINS / 2 ? 0 : 255)), 100);
    assert.equal(h.length, 100);
    assert.ok(Math.abs(h[99] - 1) < 1e-6);
    assert.ok(h[0] > 0 && h[0] < 0.1);
});

test('each bar takes the loudest slice under it', () => {
    // One loud slice in the middle of quiet ones.
    const h = barHeights(wave(i => (i === 503 ? 255 : 128)), 10);
    const top = h.indexOf(Math.max(...h));
    assert.equal(top, 5);
    assert.ok(h[0] < h[5]);
});

test('levels are against the track\'s own loudest', () => {
    // A quiet track (−30 dBFS at most) still fills the bar.
    const h = barHeights(wave(() => 128), 20);
    assert.ok(h.every(v => Math.abs(v - 1) < 1e-6));
});
