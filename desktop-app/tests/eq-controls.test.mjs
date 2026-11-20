// The EQ graph's editing arithmetic (js/eq/controls.js): typed values, the
// band a click adds, hit tests, the group move, the growing dB scale, the view.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    parseFreq, parseNum, noteOf, typeAt, shelfAt, hitPoint, inRect, idRange,
    groupMove, expandRange, gridStep, zoomView, panView, isFullView, nudge, slopeStep, F_HI,
} from '../src/js/eq/controls.js';

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg ?? ''} ${a} vs ${b}`);

test('typed frequencies: Hz, k, kHz, notes with cents', () => {
    assert.equal(parseFreq('1000'), 1000);
    assert.equal(parseFreq('2k'), 2000);
    assert.equal(parseFreq('1.5k'), 1500);
    assert.equal(parseFreq('1,5 kHz'), 1500);
    assert.equal(parseFreq('440hz'), 440);
    near(parseFreq('A4'), 440, 1e-9);
    near(parseFreq('a5'), 880, 1e-9);
    near(parseFreq('C4'), 261.6256, 1e-3);
    near(parseFreq('C#2+13'), 69.2957 * Math.pow(2, 13 / 1200), 1e-3);
    near(parseFreq('Bb3-20'), 233.0819 * Math.pow(2, -20 / 1200), 1e-3);
    for (const bad of ['', 'abc', 'H4', '-5', '2kk', null]) assert.equal(parseFreq(bad), null, bad);
});

test('typed numbers', () => {
    assert.equal(parseNum('+3'), 3);
    assert.equal(parseNum('-1.5 dB'), -1.5);
    assert.equal(parseNum('−2'), -2);
    assert.equal(parseNum('0,7'), 0.7);
    assert.equal(parseNum('x'), null);
});

test('note readout', () => {
    assert.deepEqual(noteOf(440), { name: 'A4', cents: 0 });
    assert.deepEqual(noteOf(440 * Math.pow(2, 3 / 1200)), { name: 'A4', cents: 3 });
    assert.equal(noteOf(261.63).name, 'C4');
});

test('the band a click adds depends on where', () => {
    const plot = { l: 0, r: 1000, t: 0, b: 400 };
    assert.equal(typeAt(5, 200, plot), 'lowCut');
    assert.equal(typeAt(995, 200, plot), 'highCut');
    assert.equal(typeAt(500, 395, plot), 'notch');
    assert.equal(typeAt(500, 200, plot), 'bell');
    assert.equal(shelfAt(50, plot), 'lowShelf');
    assert.equal(shelfAt(950, plot), 'highShelf');
    assert.equal(shelfAt(500, plot), null);
});

test('hit tests and selections', () => {
    const pts = [{ id: 1, x: 10, y: 10 }, { id: 2, x: 30, y: 10 }, { id: 3, x: 100, y: 100 }];
    assert.equal(hitPoint(pts, { x: 12, y: 11 }, 12).id, 1);
    assert.equal(hitPoint(pts, { x: 22, y: 10 }, 12).id, 2, 'the nearest wins');
    assert.equal(hitPoint(pts, { x: 60, y: 60 }, 12), null);
    assert.deepEqual(inRect(pts, { x: 40, y: 0 }, { x: 0, y: 20 }), [1, 2], 'corners in any order');
    assert.deepEqual(idRange([1, 2, 3, 4], 3, 1), [1, 2, 3]);
    assert.deepEqual(idRange([1, 2, 3], 9, 2), [2], 'no anchor: just the clicked one');
});

test('group move: log shift, relative gain, held inside the range', () => {
    const start = [
        { id: 1, f: 100, gain: 4, gained: true },
        { id: 2, f: 1000, gain: -2, gained: true },
        { id: 3, f: 50, gain: 0, gained: false },
    ];
    const m = groupMove(start, 1, 200, 8);
    near(m[1].f, 200, 1e-9); near(m[2].f, 2000, 1e-9); near(m[3].f, 100, 1e-9);
    assert.equal(m[1].gain, 8);
    near(m[2].gain, -4, 1e-9, 'gains scale by the primary ratio');
    assert.equal(m[3].gain, 0, 'a cut keeps its gain');
    const far = groupMove(start, 2, 1e6, -2);
    near(far[2].f, F_HI, 1e-6, 'the highest band stops at the top');
    near(far[1].f, 100 * F_HI / 1000, 1e-6, 'the others keep the shape');
    const flat = groupMove([{ id: 1, f: 100, gain: 0, gained: true }, { id: 2, f: 200, gain: 3, gained: true }], 1, 100, 2);
    near(flat[2].gain, 5, 1e-9, 'from 0 dB gains shift instead');
});

test('the dB scale grows while dragging beyond, never shrinks itself', () => {
    assert.equal(expandRange(6, 4), 6);
    assert.equal(expandRange(6, 7), 12);
    assert.equal(expandRange(3, -13), 30);
    assert.equal(expandRange(30, 2), 30);
    assert.equal(expandRange(12, 99), 30);
    assert.deepEqual([3, 6, 12, 30].map(gridStep), [1, 2, 3, 6]);
});

test('frequency view: zoom keeps the anchor, pan stays inside', () => {
    const full = { lo: 10, hi: 30000 };
    assert.ok(isFullView(full));
    const z = zoomView(full, 1000, 0.5);
    const t = v => Math.log(1000 / v.lo) / Math.log(v.hi / v.lo);
    near(t(z), t(full), 1e-9, 'anchor keeps its place');
    near(Math.log(z.hi / z.lo), Math.log(3000) / 2, 1e-9);
    const tiny = zoomView(full, 1000, 1e-6);
    near(tiny.hi / tiny.lo, 4, 1e-9, 'at least two octaves');
    const back = zoomView(z, 1000, 100);
    assert.ok(isFullView(back));
    const p = panView(z, 100);
    near(p.hi, 30000, 1e-6, 'pan stops at the top');
    near(p.hi / p.lo, z.hi / z.lo, 1e-6);
});

test('keyboard nudge and wheel slope steps', () => {
    const b = { type: 'bell', f: 1000, gain: 1, q: 1 };
    near(nudge(b, 'ArrowRight', false).f, 1000 * Math.pow(2, 1 / 24), 1e-9);
    assert.equal(nudge(b, 'ArrowUp', false).gain, 1.5);
    assert.equal(nudge(b, 'ArrowDown', true).gain, 0.9);
    const cut = { type: 'lowCut', f: 80, gain: 0, q: 0.71 };
    near(nudge(cut, 'ArrowUp', false).q, 0.781, 1e-9, 'a cut moves Q');
    const S = [6, 12, 18, 24];
    assert.equal(slopeStep(S, 12, 1), 18);
    assert.equal(slopeStep(S, 6, -1), 6);
    assert.equal(slopeStep(S, 24, 1), 24);
});

import { fmtBalance, parseBalance, balanceFromPos, posFromBalance, BALANCE_SPAN } from '../src/js/eq/controls.js';

test('balance field: shows C or the side and dB, reads what a listener types', () => {
    assert.equal(fmtBalance(0, 'lr'), 'C');
    assert.equal(fmtBalance(1.5, 'lr'), 'R 1.5 dB');
    assert.equal(fmtBalance(-0.25, 'lr'), 'L 0.25 dB');
    assert.equal(fmtBalance(-3, 'ms'), 'M 3.0 dB');
    assert.equal(fmtBalance(0.1, 'ms'), 'S 0.1 dB');
    for (const [t, v] of [['C', 0], ['0', 0], ['R1.5', 1.5], ['l 0.7 dB', -0.7], ['S2', 2], ['M 3', -3], ['-1.5', -1.5],
        ['+0.25', 0.25], ['1,5', 1.5], ['1.5R', 1.5], ['−2', -2], ['L-2', -2]]) assert.equal(parseBalance(t), v, t);
    for (const t of ['', 'x', 'RL1', 'L1R', '1..2']) assert.equal(parseBalance(t), null, t);
});

test('balance slider: cubed, finest in the middle, both ends at ±30 dB, round trip', () => {
    assert.equal(balanceFromPos(0), 0);
    assert.equal(balanceFromPos(1), BALANCE_SPAN);
    assert.equal(balanceFromPos(-1), -BALANCE_SPAN);
    assert.ok(balanceFromPos(0.1) <= 0.03, 'a tenth of the travel is hundredths of a dB');
    for (const db of [0.5, -1.25, 3, -12, 30]) near(balanceFromPos(posFromBalance(db)), db, 0.01, db);
    assert.equal(posFromBalance(99), 1);
});
