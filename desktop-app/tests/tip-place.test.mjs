// Where a tooltip goes: always whole inside the window.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The analyzer centred its tooltip over the element and never looked at the
// window's sides: a metric's name at the left edge got a tooltip cut in
// half. These cases put anchors against every edge and corner.

import test from 'node:test';
import assert from 'node:assert/strict';

import { fitTip, TIP_MARGIN } from '../src/js/tip-place.js';

const VW = 900, VH = 600;
const rect = (left, top, w = 60, h = 18) => ({ left, top, right: left + w, bottom: top + h });
const point = (x, y) => ({ left: x, top: y, right: x, bottom: y });

function inside(p, w, h, what) {
    assert.ok(p.left >= TIP_MARGIN, `${what}: left ${p.left}`);
    assert.ok(p.top >= TIP_MARGIN, `${what}: top ${p.top}`);
    assert.ok(p.left + w <= VW - TIP_MARGIN, `${what}: right ${p.left + w}`);
    assert.ok(p.top + h <= VH - TIP_MARGIN, `${what}: bottom ${p.top + h}`);
}

test('centred over an element in the middle', () => {
    const p = fitTip(rect(400, 300), 200, 40, VW, VH);
    assert.equal(p.left, 400 + 30 - 100);
    assert.equal(p.top, 300 - 8 - 40);
    assert.equal(p.below, false);
});

test('an element at the left edge: the tooltip moves right, not off the window', () => {
    // The analyzer's metric names sit at x = 10.
    const p = fitTip(rect(10, 300, 40), 280, 60, VW, VH);
    assert.equal(p.left, TIP_MARGIN);
    inside(p, 280, 60, 'left edge');
});

test('an element at the right edge: the tooltip moves left', () => {
    const p = fitTip(rect(VW - 30, 300, 24), 280, 60, VW, VH);
    assert.equal(p.left, VW - 280 - TIP_MARGIN);
    inside(p, 280, 60, 'right edge');
});

test('an element at the top: the tooltip goes under it', () => {
    const p = fitTip(rect(400, 4), 200, 40, VW, VH);
    assert.equal(p.below, true);
    assert.equal(p.top, 4 + 18 + 8);
    inside(p, 200, 40, 'top edge');
});

test('an element at the bottom with side below: over it', () => {
    const p = fitTip(rect(400, VH - 20), 200, 40, VW, VH, { side: 'below' });
    assert.equal(p.below, false);
    inside(p, 200, 40, 'bottom edge');
});

test('every corner, every side: whole inside', () => {
    const anchors = [rect(0, 0), rect(VW - 60, 0), rect(0, VH - 18), rect(VW - 60, VH - 18),
        point(1, 1), point(VW - 1, 1), point(1, VH - 1), point(VW - 1, VH - 1)];
    for (const a of anchors) {
        for (const side of ['above', 'below', 'pointer']) {
            for (const [w, h] of [[80, 20], [280, 120], [290, 300]]) {
                inside(fitTip(a, w, h, VW, VH, { side }), w, h, `${side} ${w}x${h} at ${a.left},${a.top}`);
            }
        }
    }
});

test('the pointer: right of it and below, over it when there is no room under', () => {
    const p = fitTip(point(300, 200), 150, 50, VW, VH, { side: 'pointer' });
    assert.deepEqual([p.left, p.top, p.below], [314, 216, true]);
    const q = fitTip(point(300, VH - 10), 150, 50, VW, VH, { side: 'pointer' });
    assert.equal(q.below, false);
    assert.equal(q.top, VH - 10 - 16 - 50);
});

test('a tooltip taller than the window keeps its top in', () => {
    const p = fitTip(rect(400, 300), 200, VH + 100, VW, VH);
    assert.equal(p.top, TIP_MARGIN);
});
