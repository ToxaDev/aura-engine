// ══════════════════════════════════════════════════════════════════════
// tip-leave.js — the pointer leaving the window takes the tooltip with it,
// and one on its way does not come. Shared by the main window
// (tooltip.js), the analyzer (analytics/window.js) and the scene studio
// (vis/studio.js).
//
// An element's own mouseout is not enough. The pointer can leave the page
// from a spot with no tooltip while one waits or is up — the element it
// belongs to drawn anew under the pointer says nothing when the pointer
// goes — and WebView2 does not always tell the page that the pointer went
// past the window's edge at all. So, besides the element's mouseout:
//   - a mouseout to nowhere (the pointer left the page), the window losing
//     focus, the window hidden (minimized) — the tooltip goes;
//   - a tooltip that waited for the pointer asks the backend first where
//     the pointer is, and one that is up for the pointer asks every POLL_MS:
//     out of the window, it goes. No answer (no backend, an older one): as
//     before.
// ══════════════════════════════════════════════════════════════════════

export const POLL_MS = 250;

/**
 * Whether the pointer is in the window, by the backend (ui_cursor_client:
 * the pointer in the window's client area, in CSS pixels): true / false, or
 * null when it cannot say. A promise when it asks; a point the rw checks pin
 * (__auraPinCursor, as dropzone.js) answers at once.
 */
export function pointerInWindow(win = globalThis.window) {
    // The window's width in the same pixels: page zoom shrinks innerWidth,
    // not outerWidth; the larger of the two never takes the window's own
    // edge for outside.
    const inside = (pt) => {
        if (!Array.isArray(pt)) return null;
        const w = Math.max(win.innerWidth || 0, win.outerWidth || 0);
        const h = Math.max(win.innerHeight || 0, win.outerHeight || 0);
        return pt[0] >= 0 && pt[1] >= 0 && pt[0] < w && pt[1] < h;
    };
    if (win?.__auraPinCursor) return inside(win.__auraPinCursor);
    const invoke = win?.__TAURI__?.tauri?.invoke;
    if (!invoke) return null;
    return Promise.resolve().then(() => invoke('ui_cursor_client')).then(inside, () => null);
}

/**
 * Watches the pointer leaving the window for one tooltip. `leave()` hides
 * the tooltip and drops one on its way. Returns:
 *   confirm(show) — a tooltip that waited for the pointer: show() unless
 *                   the pointer is out of the window;
 *   cancel()      — a confirm still waiting for its answer is dropped;
 *   up()          — a tooltip for the pointer is showing: asked every pollMs;
 *   down()        — none is: no more asking.
 */
export function watchTipLeave(leave, {
    doc = globalThis.document,
    win = globalThis.window,
    inWindow = () => pointerInWindow(win),
    pollMs = POLL_MS,
} = {}) {
    doc.addEventListener('mouseout', e => { if (!e.relatedTarget) leave(); });
    win.addEventListener('blur', () => leave());
    doc.addEventListener('visibilitychange', () => { if (doc.visibilityState === 'hidden') leave(); });

    let asked = 0;         // the confirm whose answer counts
    let poll = null;
    let asking = false;

    const isPromise = r => !!r && typeof r.then === 'function';

    function confirm(show) {
        const mine = ++asked;
        const r = inWindow();
        if (!isPromise(r)) {
            if (r !== false) show();
            return;
        }
        r.then(inside => { if (mine === asked && inside !== false) show(); }, () => {});
    }

    function cancel() { asked++; }

    function down() {
        clearInterval(poll);
        poll = null;
    }

    function up() {
        if (poll) return;
        poll = setInterval(() => {
            if (asking) return;
            const r = inWindow();
            if (!isPromise(r)) {
                if (r === false) leave();
                else if (r === null) down();   // nothing to ask
                return;
            }
            asking = true;
            r.then(inside => {
                asking = false;
                if (inside === false && poll) leave();
            }, () => { asking = false; });
        }, pollMs);
    }

    return { confirm, cancel, up, down };
}
