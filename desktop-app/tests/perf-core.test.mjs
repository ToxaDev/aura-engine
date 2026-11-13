// The load badge and the load window: what the numbers read, which second the
// pointer is on, where the app was playing or converting.
import test from 'node:test';
import assert from 'node:assert/strict';

import {
    PERF_TIPS, loadText, loadText1, nearestIndex, stretches, scaleTop, clockText, pushCapped,
    ramText, ramScaleTop, gbAxisText,
} from '../src/js/perf-core.js';

test('the badge shows whole numbers, no sign', () => {
    assert.equal(loadText(12.3), '12');
    assert.equal(loadText(12.5), '13');
    assert.equal(loadText(0), '0');
    assert.equal(loadText(0.4), '0');
    assert.equal(loadText(100), '100');
    // Out of range is held to 0–100.
    assert.equal(loadText(140), '100');
    assert.equal(loadText(-3), '0');
    // Before the first second: a dash.
    assert.equal(loadText(undefined), '–');
    assert.equal(loadText(NaN), '–');
    assert.equal(loadText('7'), '–');
    assert.ok(!loadText(55).includes('%'));
});

test('the window reads to a tenth', () => {
    assert.equal(loadText1(3.14), '3.1');
    assert.equal(loadText1(0), '0.0');
    assert.equal(loadText1(101), '100.0');
    assert.equal(loadText1(null), '–');
});

test('the tips say what each part measures', () => {
    assert.match(PERF_TIPS.cpu, /all cores/);
    assert.match(PERF_TIPS.gpu, /video card/);
    assert.match(PERF_TIPS.gpu, /0 when the card is not used/);
});

const at = (...ts) => ts.map(t => ({ t, cpu: 0, gpu: 0 }));

test('the pointer finds the nearest second', () => {
    const s = at(1000, 2000, 3000, 5000);
    assert.equal(nearestIndex(s, 0), 0);
    assert.equal(nearestIndex(s, 1400), 0);
    assert.equal(nearestIndex(s, 1600), 1);
    assert.equal(nearestIndex(s, 2500), 1, 'a tie goes to the earlier');
    assert.equal(nearestIndex(s, 3900), 2);
    assert.equal(nearestIndex(s, 4100), 3);
    assert.equal(nearestIndex(s, 99999), 3);
    assert.equal(nearestIndex([], 1000), -1);
    assert.equal(nearestIndex(at(7), 1), 0);
});

test('the pointer lookup is a search, not a walk', () => {
    const s = Array.from({ length: 3600 }, (_, i) => ({ t: 1e12 + i * 1000 }));
    for (const i of [0, 1, 1799, 3598, 3599]) {
        assert.equal(nearestIndex(s, s[i].t + 300), i);
    }
});

test('playing and converting become stretches', () => {
    const s = [
        { t: 1000, play: false }, { t: 2000, play: true }, { t: 3000, play: true },
        { t: 4000, play: false }, { t: 5000, play: true },
    ];
    assert.deepEqual(stretches(s, 'play'), [{ t0: 1000, t1: 3000 }, { t0: 4000, t1: 5000 }]);
    assert.deepEqual(stretches(s, 'conv'), []);
    assert.deepEqual(stretches([], 'play'), []);
});

test('a gap in the seconds (the machine slept) ends a stretch', () => {
    const s = [{ t: 1000, play: true }, { t: 2000, play: true }, { t: 60000, play: true }, { t: 61000, play: true }];
    assert.deepEqual(stretches(s, 'play'), [{ t0: 0, t1: 2000 }, { t0: 59000, t1: 61000 }]);
});

test('the scale tops at a round number', () => {
    assert.equal(scaleTop(0), 5);
    assert.equal(scaleTop(4.9), 5);
    assert.equal(scaleTop(5.1), 10);
    assert.equal(scaleTop(18), 20);
    assert.equal(scaleTop(24), 25);
    assert.equal(scaleTop(26), 50);
    assert.equal(scaleTop(99), 100);
    assert.equal(scaleTop(250), 100);
    assert.equal(scaleTop(NaN), 5);
});

test('the clock reads HH:MM:SS', () => {
    const d = new Date(2026, 9, 19, 7, 5, 9);
    assert.equal(clockText(d.getTime()), '07:05:09');
});

test('the history keeps the newest', () => {
    const a = [];
    for (let i = 0; i < 10; i++) pushCapped(a, i, 4);
    assert.deepEqual(a, [6, 7, 8, 9]);
});

test('memory reads in MB under a gigabyte, in GB to a tenth above', () => {
    assert.equal(ramText(640), '640 MB');
    assert.equal(ramText(999.6), '1000 MB');
    assert.equal(ramText(1000), '1.0 GB');
    assert.equal(ramText(1229), '1.2 GB');
    assert.equal(ramText(16 * 1024), '16.0 GB');
    // Not read yet, or not readable: a dash.
    assert.equal(ramText(0), '–');
    assert.equal(ramText(undefined), '–');
    assert.equal(ramText(NaN), '–');
    assert.ok(PERF_TIPS.ram.includes('working set'));
});

test('the memory scale is a round number of GB, capped by the machine', () => {
    assert.equal(ramScaleTop(0), 512);
    assert.equal(ramScaleTop(300), 512);
    assert.equal(ramScaleTop(512), 512);
    assert.equal(ramScaleTop(700), 1024);
    assert.equal(ramScaleTop(5000), 8192);
    // An 8 GB machine (as the system reports it): the top is all of it, not 8192.
    assert.equal(ramScaleTop(5000, 7900), 7900);
    // A value above the reported total (should not happen) still fits.
    assert.equal(ramScaleTop(8000, 7900), 8000);
    // A big machine does not flatten a small line.
    assert.equal(ramScaleTop(900, 65536), 1024);
    assert.equal(ramScaleTop(NaN, 8192), 512);
});

test('the memory scale labels are GB', () => {
    assert.equal(gbAxisText(0), '0');
    assert.equal(gbAxisText(256), '0.25');
    assert.equal(gbAxisText(512), '0.5');
    assert.equal(gbAxisText(1024), '1');
    assert.equal(gbAxisText(3950), '3.9');
    assert.equal(gbAxisText(7900), '7.7');
    assert.equal(gbAxisText(16384), '16');
});
