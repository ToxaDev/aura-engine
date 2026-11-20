// ══════════════════════════════════════════════════════════════════════
// confirm-pop.js — "are you sure?" right where the button is (Anton 8.11:
// Clear asks before the list goes). A small card drops under the button
// that was pressed, its arrow on it: a line of text, the action and Cancel.
// A press outside it, Esc or Cancel keep everything as it was; the action
// goes on. Cancel has the focus, so a stray Enter or Space empties nothing. The popup rule of the app holds (popups.js): the press that
// only puts it away presses nothing under it.
// ══════════════════════════════════════════════════════════════════════

import { addPopup } from './popups.js';
import { placeTip } from './tip-place.js';

let open = null;   // { pop, anchor, done }

/// Put away the card that is up (its answer: no). True when one was up.
export function closeConfirm() {
    if (!open) return false;
    open.done(false);
    return true;
}

/**
 * Ask under `anchor`. Resolves true for the action, false for anything else.
 * A second press on the same button while its card is up puts it away.
 * @param {HTMLElement} anchor
 * @param {{text?: string, yes?: string, no?: string, danger?: boolean}} [o]
 * @returns {Promise<boolean>}
 */
export function confirmAt(anchor, o = {}) {
    if (open) {
        const same = open.anchor === anchor;
        open.done(false);
        if (same) return Promise.resolve(false);
    }
    const doc = anchor.ownerDocument || document;
    const pop = doc.createElement('div');
    pop.className = 'cpop' + (o.danger === false ? '' : ' cpop-danger');
    pop.setAttribute('role', 'alertdialog');
    const text = doc.createElement('div');
    text.className = 'cpop-text';
    text.textContent = o.text || 'Are you sure?';
    pop.id = 'cpop' + Math.random().toString(36).slice(2, 8);
    text.id = pop.id + 't';
    pop.setAttribute('aria-labelledby', text.id);
    const row = doc.createElement('div');
    row.className = 'cpop-row';
    const no = doc.createElement('button');
    no.type = 'button';
    no.className = 'cpop-no';
    no.textContent = o.no || 'Cancel';
    const yes = doc.createElement('button');
    yes.type = 'button';
    yes.className = 'cpop-yes';
    yes.textContent = o.yes || 'Yes';
    row.append(no, yes);
    const caret = doc.createElement('i');
    caret.className = 'cpop-caret';
    pop.append(caret, text, row);
    doc.body.append(pop);

    return new Promise((resolve) => {
        let off = () => {};
        const done = (v) => {
            if (!open || open.pop !== pop) return;
            open = null;
            off();
            window.removeEventListener('resize', put);
            window.removeEventListener('scroll', put, true);
            try { if (pop.matches(':popover-open')) pop.hidePopover(); } catch (_) {}
            pop.remove();
            anchor.setAttribute('aria-expanded', 'false');
            resolve(v);
        };
        const put = () => {
            const a = anchor.getBoundingClientRect();
            if (!a.width && !a.height) { done(false); return; }
            const p = placeTip(pop, a, { side: 'below', gap: 9 });
            pop.classList.toggle('cpop-up', !p.below);
            const mid = (a.left + a.right) / 2 - p.left;
            pop.style.setProperty('--cpop-caret', Math.round(Math.max(12, Math.min(pop.offsetWidth - 12, mid))) + 'px');
        };
        open = { pop, anchor, done };
        off = addPopup({
            isOpen: () => open?.pop === pop,
            inside: (t) => pop.contains(t) || anchor.contains(t),
            close: () => done(false),
        });
        no.addEventListener('click', () => done(false));
        yes.addEventListener('click', () => done(true));
        window.addEventListener('resize', put);
        window.addEventListener('scroll', put, true);
        anchor.setAttribute('aria-expanded', 'true');
        put();
        // Cancel takes the focus: a stray Space or a second Enter on the
        // button just pressed must not empty anything.
        try { no.focus({ preventScroll: true }); } catch (_) {}
    });
}
