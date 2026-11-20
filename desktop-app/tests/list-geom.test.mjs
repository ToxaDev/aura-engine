// A drag over the list: the slot the row settles in, the gap for files from
// outside, the edges that scroll the list and how fast (list-geom.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    settleSlot, rowsAt, gapSlot, dragTop, edgeAt, edgeStepMs, edgePacer, maxScroll, scrollAfterDrop,
    SETTLE_HYST, EDGE_WAIT_MS,
} from '../src/js/list-geom.js';

// The list's own numbers (list.css): rows 43 px, 4 px apart, 4 px padding.
const GEO = { rowH: 43, gap: 4, pad: 4, step: 47 };
const VISIBLE = 6;
const H = VISIBLE * GEO.rowH + (VISIBLE - 1) * GEO.gap + 2 * GEO.pad;   // 286, the view

test('a row keeps its slot until it is half a row and the margin away from it', () => {
    assert.equal(settleSlot(3.0, 3, 9), 3);
    assert.equal(settleSlot(3.5 + SETTLE_HYST - 0.01, 3, 9), 3, 'inside the margin: stays');
    assert.equal(settleSlot(3.5 + SETTLE_HYST + 0.01, 3, 9), 4, 'past it: the next slot');
    assert.equal(settleSlot(2.5 - SETTLE_HYST + 0.01, 3, 9), 3);
    assert.equal(settleSlot(2.5 - SETTLE_HYST - 0.01, 3, 9), 2);
    assert.equal(settleSlot(6.2, 3, 9), 6, 'a fast hand: straight to the slot under it');
    assert.equal(settleSlot(2.4, null, 9), 2, 'no slot yet: the nearest');
    assert.equal(settleSlot(-3, 1, 9), 0);
    assert.equal(settleSlot(14, 8, 9), 9);
});

test('a hand trembling on the line between two slots does not move the rows', () => {
    // The row has just moved on to slot 4 at 3.71; the hand wavers ±5 px around there.
    let slot = 4;
    const moves = [];
    for (const px of [0, -5, 4, -3, 5, -5, 2]) {
        const next = settleSlot(3.71 + px / GEO.step, slot, 9);
        if (next !== slot) moves.push(next);
        slot = next;
    }
    assert.deepEqual(moves, []);
});

test('a point of the view in rows: a slot runs half a space above its row to half below', () => {
    assert.equal(rowsAt(GEO.pad - GEO.gap / 2, 0, GEO), 0, 'half a space above the first row');
    assert.equal(rowsAt(GEO.pad + GEO.rowH / 2, 0, GEO), 0.5, 'the middle of the first row');
    assert.equal(rowsAt(GEO.pad + GEO.rowH / 2, 10 * GEO.step, GEO), 10.5, 'scrolled ten rows');
});

test('the gap opens in the slot under the pointer and follows it with the margin', () => {
    const n = 40;
    const at = rows => rowsAt(GEO.pad + rows * GEO.step, 0, GEO);   // a point `rows` down the view
    assert.equal(gapSlot(at(2.3), null, n), 2, 'over the third row: the gap before it');
    assert.equal(gapSlot(at(2.9), null, n), 2);
    // With the gap in slot 2, the row that was there shows in slot 3: the gap moves on only
    // when the pointer is the margin into that slot — not on the line between them.
    assert.equal(gapSlot(3 + SETTLE_HYST - 0.05, 2, n), 2);
    assert.equal(gapSlot(3 + SETTLE_HYST + 0.05, 2, n), 3);
    assert.equal(gapSlot(2 - SETTLE_HYST + 0.05, 2, n), 2);
    assert.equal(gapSlot(2 - SETTLE_HYST - 0.05, 2, n), 1);
    assert.equal(gapSlot(at(45), 3, n), n, 'past the last row: the gap under it');
});

test('the gap under the last row: at the list end, one slot more than the rows', () => {
    const n = 40;
    // Scrolled as far as the gap's own slot lets: rows 35..39 and the gap at the bottom.
    const top = maxScroll(n + 1, VISIBLE, GEO.step);
    assert.equal(top, 35 * GEO.step);
    const bottom = rowsAt(H - GEO.pad - 1, top, GEO);   // the pointer held at the view's bottom
    assert.equal(gapSlot(bottom, null, n), n);
    assert.equal(gapSlot(bottom, n - 1, n), n, 'the list scrolled the last row up: the gap goes under it');
    // At the list's own end (no extra slot) the last row sits at the bottom: the gap opens
    // before it, and the edge scrolls the list on.
    assert.equal(gapSlot(rowsAt(H - GEO.pad - 1, maxScroll(n, VISIBLE, GEO.step), GEO), null, n), n - 1);
});

test('the gap before the first row: scrolled to the top, the pointer at the top edge', () => {
    assert.equal(gapSlot(rowsAt(GEO.pad, 0, GEO), null, 40), 0);
    assert.equal(gapSlot(rowsAt(GEO.pad, 0, GEO), 1, 40), 0);
    // A step of the edge scroll up moves the gap with the first slot of the view.
    assert.equal(gapSlot(rowsAt(GEO.pad, 5 * GEO.step, GEO), 6, 40), 5);
});

test('the dragged row stays whole inside the view', () => {
    const g = { ...GEO, h: H };
    assert.equal(dragTop(GEO.pad + 2 * GEO.step, 0, g), 2);
    assert.equal(dragTop(-80, 10 * GEO.step, g), 10, 'past the top edge: the first slot shown');
    assert.equal(dragTop(900, 10 * GEO.step, g), 10 + VISIBLE - 1, 'past the bottom: the last slot shown');
});

test('the edge band: the pointer near an edge or past it', () => {
    const band = 0.5 * GEO.step;
    assert.deepEqual(edgeAt(100, H, band), { dir: 0, depth: 0 });
    assert.equal(edgeAt(band - 1, H, band).dir, -1);
    assert.equal(edgeAt(-40, H, band).depth, 1, 'past the edge: as deep as it goes');
    assert.equal(edgeAt(H - 2, H, band).dir, 1);
    assert.ok(edgeAt(H - band + 2, H, band).depth < 0.1, 'just inside the band: slow');
});

test('the edge waits before the first row, then speeds up, deeper sooner', () => {
    assert.equal(edgeStepMs(0), EDGE_WAIT_MS);
    const deep = [1, 2, 3, 6, 12, 40].map(k => edgeStepMs(k, 1));
    for (let i = 1; i < deep.length; i++) assert.ok(deep[i] <= deep[i - 1], `k-th row sooner: ${deep}`);
    assert.ok(deep.at(-1) >= 40, 'never faster than the floor');
    assert.ok(edgeStepMs(3, 0) > edgeStepMs(3, 1), 'at the band\'s inner side slower than at the edge');
    // Forty rows at the edge go by in a few seconds, not in half a minute.
    let total = 0;
    for (let k = 0; k < 40; k++) total += edgeStepMs(k, 1);
    assert.ok(total < 4000, `40 rows in ${total} ms`);
});

test('the pace: nothing on the way in, then rows while the pointer stays', () => {
    const pace = edgePacer();
    const got = [];
    for (let t = 0; t <= 1000; t += 16) got.push([t, pace(t, 1, 1)]);
    const rows = got.filter(([, d]) => d);
    assert.ok(rows.length >= 3, `rows in the first second: ${rows.length}`);
    assert.ok(rows[0][0] >= EDGE_WAIT_MS, 'the first row after the wait');
    assert.equal(pace(1016, 0, 0), 0, 'out of the band: nothing');
    assert.equal(pace(1032, -1, 1), 0, 'the other edge: the wait again');
    assert.equal(pace(1032 + EDGE_WAIT_MS - 16, -1, 1), 0);
    assert.equal(pace(1032 + EDGE_WAIT_MS, -1, 1), -1);
});

test('after the drop the view stays where it shows, on the nearest whole row', () => {
    const step = GEO.step;
    // Forty rows, the gap at the end, scrolled into the gap's own slot; one file lands.
    assert.equal(scrollAfterDrop(35 * step, 41, VISIBLE, step, 40, 1), 35 * step);
    // Part-way through a step of the edge scroll: the nearer row — not the drum's target,
    // which a fast edge scroll keeps a row or two ahead of what is seen.
    assert.equal(scrollAfterDrop(20 * step + 20, 43, VISIBLE, step), 20 * step);
    assert.equal(scrollAfterDrop(20 * step + 30, 43, VISIBLE, step), 21 * step);
    // Never past the end of the list.
    assert.equal(scrollAfterDrop(90 * step, 43, VISIBLE, step), 37 * step);
    assert.equal(scrollAfterDrop(-5, 3, VISIBLE, step), 0);
});

test('several files dropped at the bottom come into view, the first of them kept in view', () => {
    const step = GEO.step;
    const view = 10 * step;                 // slots 10..15 shown
    assert.equal(scrollAfterDrop(view, 50, VISIBLE, step, 15, 3), 12 * step, 'three into the bottom slot: all three shown');
    assert.equal(scrollAfterDrop(view, 50, VISIBLE, step, 15, 8), 15 * step, 'more than the view holds: the first at the top');
    assert.equal(scrollAfterDrop(view, 50, VISIBLE, step, 10, 3), view, 'into the top slot: all shown already');
    assert.equal(scrollAfterDrop(view, 50, VISIBLE, step, 11, 2), view);
    assert.equal(scrollAfterDrop(view, 50, VISIBLE, step, 13, 5), 12 * step, 'past the bottom from the middle');
});

test('a list of no rows or one row', () => {
    assert.equal(settleSlot(3.2, 0, 0), 0);
    assert.equal(settleSlot(-2, null, 0), 0);
    assert.equal(gapSlot(0.3, null, 0), 0, 'an empty list: the gap is its only slot');
    assert.equal(gapSlot(0.9, null, 1), 0, 'one row: over it, the gap before it');
    assert.equal(gapSlot(1.6, null, 1), 1, 'under it: after it');
    assert.equal(maxScroll(0, VISIBLE, GEO.step), 0);
    assert.equal(maxScroll(1 + 1, VISIBLE, GEO.step), 0, 'one row and the gap\'s slot: nothing to scroll');
    assert.equal(scrollAfterDrop(0, 1, VISIBLE, GEO.step, 0, 1), 0);
});
