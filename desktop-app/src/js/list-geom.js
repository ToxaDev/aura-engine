// ══════════════════════════════════════════════════════════════════════
// list-geom.js — the numbers of a drag over the list: the slot the dragged
// row settles in (or the gap that files dragged in from outside open), when
// the list scrolls at its edges, and how fast. No DOM here, so the node
// tests hold it to its rules (tests/list-geom.test.mjs).
//
// Positions are in rows, counted from the list's first slot: a slot is one
// row step (a row and the space under it) and 3.5 is the middle of the
// fourth slot.
// ══════════════════════════════════════════════════════════════════════

/// How far past the half-way line a row must go before its slot changes, in
/// rows (about 9 px at the list's 47 px step): a hand trembling on the line
/// does not throw the rows back and forth.
export const SETTLE_HYST = 0.2;

/// The edge band, in rows: the pointer this close to the list's top or bottom
/// edge, or past it, scrolls the list.
export const EDGE_BAND = 0.5;

/// The pointer waits this long in the band before the first row goes by, so
/// crossing the band on the way in scrolls nothing.
export const EDGE_WAIT_MS = 250;
const EDGE_FIRST_MS = 190;    // the rows after the wait go by this far apart,
const EDGE_SPEEDUP = 0.85;    // each one sooner than the last,
const EDGE_FASTEST_MS = 40;   // down to this.

const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));

/**
 * The slot a row settles in. `top` is where the row's top edge is, in rows;
 * `cur` the slot it settles in now (null: none yet); `last` the last slot it
 * may take. The slot moves on once the row is half a row and `hyst` more
 * away from it.
 */
export function settleSlot(top, cur, last, hyst = SETTLE_HYST) {
    if (cur != null && Math.abs(top - cur) <= 0.5 + hyst) return clamp(cur, 0, last);
    return clamp(Math.round(top), 0, last);
}

/**
 * Where a point of the list's view is, in rows: `y` in px from the view's top
 * edge, `scrollTop` the list's, `geo` = { pad, gap, step } (the list's
 * padding, the space between rows, row + space). A row's slot runs from half
 * a space above the row to half a space below it, so its middle is k + 0.5.
 */
export function rowsAt(y, scrollTop, { pad, gap, step }) {
    return (y + scrollTop - pad + gap / 2) / step;
}

/**
 * The gap for files dragged in from outside: the slot under the pointer
 * (`at`, in rows), the rows from it on moved a slot down. `n` rows: the gap
 * may be under the last one (n). `cur` the gap now (null: closed).
 */
export function gapSlot(at, cur, n, hyst = SETTLE_HYST) {
    return settleSlot(at - 0.5, cur, n, hyst);
}

/**
 * The dragged row's top, in rows, for the pointer putting it `rowTop` px from
 * the view's top edge. The row stays whole inside the view (at an edge it
 * waits there while the list scrolls under it); `geo` = { pad, step, rowH,
 * h } (the view's height).
 */
export function dragTop(rowTop, scrollTop, { pad, step, rowH, h }) {
    const t = clamp(rowTop, pad, Math.max(pad, h - pad - rowH));
    return (t + scrollTop - pad) / step;
}

/**
 * The edge the pointer is at: dir -1 the top, 1 the bottom, 0 neither, and
 * `depth` from 0 on the band's inner side to 1 at the edge and past it.
 * `y` in px from the view's top edge, `h` the view's height, `band` in px.
 */
export function edgeAt(y, h, band) {
    if (y < band) return { dir: -1, depth: Math.min(1, (band - y) / band) };
    if (y > h - band) return { dir: 1, depth: Math.min(1, (y - (h - band)) / band) };
    return { dir: 0, depth: 0 };
}

/**
 * How long until the next row goes by, for the k-th row of one stay at an
 * edge (k = 0: the wait before the first). Deeper in the band, sooner.
 */
export function edgeStepMs(k, depth = 1) {
    if (k <= 0) return EDGE_WAIT_MS;
    const base = Math.max(EDGE_FASTEST_MS, EDGE_FIRST_MS * Math.pow(EDGE_SPEEDUP, k - 1));
    return Math.round(base * (1.4 - 0.4 * clamp(depth, 0, 1)));
}

/**
 * The pace of one drag at the edges: pace(now, dir, depth) says, once a
 * frame, whether a row goes by now (-1 up, 1 down) or not (0). Leaving the
 * band, or moving to the other edge, starts the count again.
 */
export function edgePacer() {
    let dir = 0, k = 0, due = 0;
    return (now, d, depth = 1) => {
        if (d !== dir) { dir = d; k = 0; due = now + edgeStepMs(0, depth); return 0; }
        if (!d || now < due) return 0;
        k++;
        due = now + edgeStepMs(k, depth);
        return d;
    };
}

/// The furthest the list scrolls, in px: `slots` rows (and the gap's own
/// slot while files from outside hover), `visible` rows shown at a time.
export const maxScroll = (slots, visible, step) => Math.max(0, (slots - visible) * step);

/**
 * Where the list's top goes after files are dropped into it (`slots` rows
 * after the drop). The files land in the slot the gap held and the rows above
 * them stay, so the view stays where it shows now — on the nearest whole row:
 * a fast edge scroll has its target a row or two ahead of what is seen. `count`
 * files dropped in at row `at`: when they run past the view's bottom, it moves
 * down to show as many as fit, the first of them kept in view.
 */
export function scrollAfterDrop(scrollTop, slots, visible, step, at = null, count = 1) {
    let top = Math.round(scrollTop / step);
    if (at != null && count > 1) top = Math.max(top, Math.min(at, at + count - visible));
    return clamp(top * step, 0, maxScroll(slots, visible, step));
}

/// The studio's rows on a screen too low for the window they make (main.js
/// fitWindowToContent): `needed` px for `rows` rows a `step` apart, `room` px
/// the screen gives — as many fewer as it takes, not fewer than `min`.
export function studioRowsFor(needed, room, rows, step, min = 6) {
    if (!(needed > room) || !(step > 0)) return rows;
    return Math.max(min, rows - Math.ceil((needed - room) / step));
}
