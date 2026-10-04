// ══════════════════════════════════════════════════════════════════════
// One rule for every popup (Anton 4.10): while one is open — a pane of the
// rack's strip, a right-click menu, the playlists, a list that drops down —
// a press anywhere outside it only puts it away. Nothing under the pointer
// hears that press: no button is pressed, no focus moves, no slider jumps,
// no row starts to play, the window does not start to move. A press on the
// popup's own opener does what it did (closes it, or switches the pane);
// inside, everything works as before. Esc puts it away too. The wheel is
// left alone: it presses nothing.
//
// Each popup tells the guard once (addPopup) whether it is open, what is
// its own (itself and its opener) and how it closes; the guard is then the
// only one to listen for the press outside. A list that drops down (a
// select, select.css: its list is the page's) needs no telling: the
// browser puts it away by itself, the guard only keeps the press from
// going through.
//
// The press is caught at its very start — pointerdown, on the window, in
// the capture phase — and cancelled: the browser then sends no mousedown
// and no mouseup, moves no focus and starts no drag or selection. What
// still comes of that press (its pointerup, the click, a right button's
// auxclick and contextmenu) is stopped as it arrives, and so is the double
// click it begins.
// ══════════════════════════════════════════════════════════════════════

/// What a press still sends after its pointerdown.
const PRESS_REST = ['mousedown', 'pointerup', 'mouseup', 'click', 'auxclick', 'contextmenu'];
/// All of that comes with the release (the right button's menu, on some
/// systems, with the press): a stopped press's rest is looked for this long
/// after it and no longer, so a click that has neither a press nor a key
/// before it (one a screen reader makes) is never taken for it.
const REST_MS = 1000;

/// The guard of one window: its listeners, and the popups told to it.
/// Returns { add } — add(popup) as addPopup below.
export function createPopupGuard(win) {
    const doc = win.document;
    const popups = [];
    // The press under way began outside: the rest of it goes nowhere.
    let swallowing = false;
    // A button is down (between pointerdown and pointerup); when the last
    // one came up.
    let held = false, releasedAt = 0;
    // Whether the last press and the one before it were stopped: a double
    // click whose first press only put a popup away is no double click.
    let lastStopped = false, priorStopped = false;
    // A list the stopped press put away. The focus stays on its row, hidden
    // now, where Space or Enter could still pick that row (another output
    // device): it is let go when the press is.
    let listLeft = null;

    const stop = e => { e.preventDefault(); e.stopImmediatePropagation(); };
    const ask = (p, what, ...args) => {
        try { return p[what](...args); } catch (err) { console.error('[popups]', err); return false; }
    };

    /// A list that drops down, open now. An engine without :open throws on
    /// the selector; its lists are the system's, outside the page.
    const openList = () => {
        try { return doc.querySelector('select:open'); } catch (_) { return null; }
    };

    const openPopups = () => popups.filter(p => ask(p, 'isOpen'));

    win.addEventListener('pointerdown', e => {
        held = true;
        priorStopped = lastStopped;
        swallowing = false;
        listLeft = null;
        if (e.isTrusted) {
            const open = openPopups();
            const list = openList();
            if (list) open.push({ inside: t => list.contains(t), close() {} });
            if (open.length) {
                const own = open.filter(p => ask(p, 'inside', e.target));
                for (const p of open) if (!own.includes(p)) ask(p, 'close');
                swallowing = own.length === 0;
                if (swallowing && list) listLeft = list;
            }
        }
        lastStopped = swallowing;
        if (swallowing) stop(e);
    }, true);

    // Before the stop below: it would keep these from being heard.
    for (const type of ['pointerup', 'pointercancel']) {
        win.addEventListener(type, () => {
            held = false;
            releasedAt = Date.now();
            const a = doc.activeElement;
            if (listLeft && a && a !== listLeft && listLeft.contains(a)) a.blur();
            listLeft = null;
        }, true);
    }
    // A window left with the button down hears no release.
    win.addEventListener('blur', () => { held = false; });
    for (const type of PRESS_REST) {
        win.addEventListener(type, e => {
            if (swallowing && e.isTrusted && (held || Date.now() - releasedAt < REST_MS)) stop(e);
        }, true);
    }
    win.addEventListener('dblclick', e => { if (priorStopped && e.isTrusted) stop(e); }, true);

    win.addEventListener('keydown', e => {
        // A click a key makes (Enter, Space) is the key's — once the
        // stopped press is let go.
        if (!held) swallowing = false;
        // An open list is the browser's to put away; the popup goes with
        // the next Esc.
        if (e.key !== 'Escape' || openList()) return;
        const open = openPopups();
        if (!open.length) return;
        for (const p of open) ask(p, p.escape ? 'escape' : 'close');
        stop(e);
    }, true);

    return {
        add(p) {
            popups.push(p);
            return () => {
                const i = popups.indexOf(p);
                if (i >= 0) popups.splice(i, 1);
            };
        },
    };
}

// The page's guard, put up as soon as anything imports this file — before
// any other listener of the page's is, so the press it stops reaches none.
const pageGuard = typeof window !== 'undefined' && window.document && typeof window.addEventListener === 'function'
    ? createPopupGuard(window)
    : null;

/// Guard a popup of this page. popup: { isOpen(), inside(target), close(),
/// escape() } — inside: the popup itself and its opener, whose press is
/// its own; escape (optional): what Esc does when it is more than close (a
/// menu's page goes back to its first one). Returns what takes it off again.
export function addPopup(popup) {
    return pageGuard ? pageGuard.add(popup) : () => {};
}
