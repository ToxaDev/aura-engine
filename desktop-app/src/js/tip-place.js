// ══════════════════════════════════════════════════════════════════════
// tip-place.js — where a tooltip goes. Shared by every window that draws
// its own tooltip (the main window's tooltip.js, the analyzer, the
// visualization studio), so that none of them is cut by the window's edge.
//
// The caller measures the tooltip where nothing squeezes it (placeTip puts
// it at 0, 0 first: a box standing near the right edge wraps narrower and
// grows taller, so a size read where the last tooltip stood is not this
// one's), then fitTip places it by its anchor:
//   'above'   centred over the element, under it when there is no room above;
//   'below'   centred under the element, over it when there is no room under;
//   'pointer' right of and below the pointer, over it when there is no room
//             under.
// Whatever the side, the tooltip ends whole inside the window, at least
// TIP_MARGIN px from every edge. One larger than the window keeps its
// top-left corner in.
// ══════════════════════════════════════════════════════════════════════

export const TIP_MARGIN = 6;

function clamp(v, lo, hi) {
    return hi < lo ? lo : Math.max(lo, Math.min(hi, v));
}

/**
 * The top-left corner of a `w`×`h` tooltip in a `vw`×`vh` window.
 * @param {{left:number, top:number, right:number, bottom:number}} anchor
 *        the element explained, or the pointer (left = right, top = bottom)
 * @param {{side?: 'above'|'below'|'pointer', gap?: number, dx?: number, dy?: number, margin?: number}} [opts]
 *        gap: px between the element and the tooltip; dx, dy: the tooltip's
 *        offset from the pointer
 * @returns {{left:number, top:number, below:boolean}} below: it ended under the anchor
 */
export function fitTip(anchor, w, h, vw, vh, opts = {}) {
    const m = opts.margin ?? TIP_MARGIN;
    const side = opts.side || 'above';
    let left, top, below;
    if (side === 'pointer') {
        const dx = opts.dx ?? 14, dy = opts.dy ?? 16;
        left = anchor.left + dx;
        top = anchor.bottom + dy;
        below = true;
        if (top + h > vh - m && anchor.top - dy - h >= m) {
            top = anchor.top - dy - h;
            below = false;
        }
    } else {
        const gap = opts.gap ?? 8;
        left = (anchor.left + anchor.right) / 2 - w / 2;
        const over = anchor.top - gap - h;
        const under = anchor.bottom + gap;
        const fitsOver = over >= m;
        const fitsUnder = under + h <= vh - m;
        if (side === 'below') below = fitsUnder || !fitsOver;
        else below = !fitsOver && fitsUnder;
        // Room on neither side: the side with more of it.
        if (!fitsOver && !fitsUnder) below = vh - anchor.bottom > anchor.top;
        top = below ? under : over;
    }
    return {
        left: Math.round(clamp(left, m, vw - w - m)),
        top: Math.round(clamp(top, m, vh - h - m)),
        below,
    };
}

/**
 * Put the shown tooltip element `tip` in place by `anchor` (see fitTip),
 * measured at 0, 0 first. Returns what fitTip returned.
 */
export function placeTip(tip, anchor, opts) {
    tip.style.left = '0px';
    tip.style.top = '0px';
    const r = tip.getBoundingClientRect();
    const p = fitTip(anchor, r.width, r.height, window.innerWidth, window.innerHeight, opts);
    tip.style.left = `${p.left}px`;
    tip.style.top = `${p.top}px`;
    return p;
}
