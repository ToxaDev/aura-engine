// ══════════════════════════════════════════════════════════════════════
// list-drag.js — a row of the list dragged to a new place.
//
// Press on a row (not on its buttons) and move: the row lifts and follows the
// pointer; each row it passes slides out of its way into the slot it left, one
// after another, like the tiles of a 15-puzzle. The row never leaves the view:
// at the list's top or bottom edge it waits there and the list scrolls under
// it, a row at a time and quicker the longer the pointer stays (list-geom.js).
// Let go: the row settles into its slot and the new order is handed on
// (`onDrop`); Esc, or the window losing the focus, puts every row back. A
// press that does not move is a click as before; a drag never ends in one.
//
// The rows are all one height (the revolver's), so a row's slot is its index
// times the step; the rows are only moved with transforms while dragging, and
// the list is rebuilt in its new order once the row has settled. Where the row
// is and which slot it takes are worked out once a frame, from the last
// pointer position and the list's scroll.
// ══════════════════════════════════════════════════════════════════════

import { settleSlot, dragTop, edgeAt, edgePacer, EDGE_BAND } from './list-geom.js';

const START_PX = 5;        // movement before a press becomes a drag
const SETTLE_MS = 170;     // the dragged row gliding into its slot

/**
 * @param {HTMLElement} list  the scrolling container of the `.pl-item` rows
 * @param {{ geo: () => { rowH: number, gap: number, pad: number, step: number },
 *           scrollRows: (dir: number) => void, stopScroll?: () => number,
 *           scrollIdle?: () => Promise<void>,
 *           onDrop: (id: number, toIndex: number) => void,
 *           onStart?: () => void, onEnd?: () => void }} opts
 */
export function initListDrag(list, { geo, scrollRows, stopScroll, scrollIdle, onDrop, onStart, onEnd }) {
    let d = null;
    let settling = false;   // the last row still gliding in: no new drag yet

    list.addEventListener('pointerdown', (e) => {
        if (e.button !== 0 || d || settling) return;
        if (e.target.closest('[data-act], button, a, input, select')) return;
        const el = e.target.closest('.pl-item');
        if (!el || !list.contains(el)) return;
        // Where on the row it was taken: that point stays under the pointer.
        const grab = e.clientY - el.getBoundingClientRect().top;
        d = { el, pointerId: e.pointerId, startY: e.clientY, lastY: e.clientY, grab, started: false };
    });

    list.addEventListener('pointermove', (e) => {
        if (!d || e.pointerId !== d.pointerId) return;
        d.lastY = e.clientY;
        if (!d.started) {
            // The button let go somewhere the list did not hear: no drag.
            if (!(e.buttons & 1)) { d = null; return; }
            if (Math.abs(e.clientY - d.startY) < START_PX) return;
            begin();
            if (!d) return;
        }
        e.preventDefault();
    });

    const end = (e) => {
        if (!d || (e && e.pointerId !== d.pointerId)) return;
        if (!d.started) { d = null; return; }
        finish(true);
    };
    list.addEventListener('pointerup', end);
    list.addEventListener('pointercancel', end);
    list.addEventListener('lostpointercapture', end);
    // A press let go before it became a drag — outside the list too, which
    // does not hear it then — is no drag: the row does not follow the pointer
    // later with no button held.
    const forget = (e) => { if (d && !d.started && e.pointerId === d.pointerId) d = null; };
    window.addEventListener('pointerup', forget, true);
    window.addEventListener('pointercancel', forget, true);
    // Esc, or the window losing the focus, puts everything back: no new order.
    window.addEventListener('keydown', (e) => {
        if (e.key !== 'Escape' || !d?.started) return;
        e.preventDefault();
        e.stopPropagation();
        finish(false);
    }, true);
    window.addEventListener('blur', () => {
        if (d?.started) finish(false);
        else d = null;
    });

    function begin() {
        const rows = [...list.querySelectorAll('.pl-item')];
        const from = rows.indexOf(d.el);
        if (from < 0) { d = null; return; }
        Object.assign(d, { started: true, rows, from, to: from, g: geo(), pace: edgePacer(), raf: 0 });
        try { list.setPointerCapture(d.pointerId); } catch { /* the pointer is gone */ }
        onStart?.();
        document.body.classList.add('pl-list-dragging');
        // No scroll snapping while rows move: the browser would follow the
        // snapped row as it moves and turn the list under the pointer.
        list.classList.add('pl-drag-active');
        d.el.classList.add('pl-drag');
        for (const r of rows) if (r !== d.el) r.classList.add('pl-drag-shift');
        frame(performance.now());
    }

    /** Once a frame while dragging: the edges, then the row and its slot. */
    function frame(now) {
        if (!d?.started) return;
        const y = d.lastY - (list.getBoundingClientRect().top + list.clientTop);
        const edge = edgeAt(y, list.clientHeight, EDGE_BAND * d.g.step);
        const dir = d.pace(now, edge.dir, edge.depth);
        if (dir) scrollRows(dir);
        const top = rowTop(d, list.scrollTop);
        d.el.style.transform = `translateY(${(top - d.from) * d.g.step}px) scale(1.015)`;
        place(d, settleSlot(top, d.to, d.rows.length - 1));
        d.raf = requestAnimationFrame(frame);
    }

    /** Where the pointer puts the dragged row's top, in rows, with the list
     *  scrolled to `scroll` — kept whole in the view. */
    function rowTop(c, scroll) {
        const y = c.lastY - (list.getBoundingClientRect().top + list.clientTop) - c.grab;
        return Math.max(0, Math.min(c.rows.length - 1, dragTop(y, scroll, { ...c.g, h: list.clientHeight })));
    }

    /** The dragged row takes slot `to`: the rows between move out of its way. */
    function place(c, to) {
        if (to === c.to) return;
        c.to = to;
        c.rows.forEach((r, i) => {
            if (r === c.el) return;
            const shift = c.from < i && i <= to ? -c.g.step : to <= i && i < c.from ? c.g.step : 0;
            r.style.transform = shift ? `translateY(${shift}px)` : '';
        });
    }

    /** The drag ends: `keep` — the row goes to its slot and the list takes
     *  the new order; otherwise every row glides back where it was. */
    function finish(keep) {
        const cur = d;
        d = null;
        cancelAnimationFrame(cur.raf);
        settling = true;
        try { list.releasePointerCapture(cur.pointerId); } catch { /* already let go */ }
        // The list stops on the row nearest to what is seen (a fast edge
        // scroll runs a row or two ahead) and the slot is taken for that view,
        // so the row settles where it was let go. It glides into its slot,
        // then the list is rebuilt in its new order — once the list is still
        // on a whole row, so snapping has nothing to move.
        const end = stopScroll ? stopScroll() : list.scrollTop;
        place(cur, keep ? settleSlot(rowTop(cur, end), cur.to, cur.rows.length - 1) : cur.from);
        const { el, rows, from, to, g } = cur;
        el.classList.add('pl-drag-settle');
        el.style.transform = `translateY(${(to - from) * g.step}px)`;
        // The click the release makes is not a click on the row.
        const swallow = (ev) => { ev.stopPropagation(); ev.preventDefault(); };
        list.addEventListener('click', swallow, { capture: true, once: true });
        setTimeout(() => list.removeEventListener('click', swallow, { capture: true }), 400);
        const settled = new Promise(res => setTimeout(res, SETTLE_MS));
        Promise.all([settled, scrollIdle?.()]).then(() => {
            for (const r of rows) {
                r.classList.remove('pl-drag', 'pl-drag-shift', 'pl-drag-settle');
                r.style.transform = '';
            }
            document.body.classList.remove('pl-list-dragging');
            list.classList.remove('pl-drag-active');
            settling = false;
            onEnd?.();
            const id = parseInt(el.dataset.id);
            if (keep && to !== from && Number.isFinite(id)) onDrop(id, to);
        });
    }
}
