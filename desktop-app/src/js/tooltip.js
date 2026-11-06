// ══════════════════════════════════════════════════════════════════════
// Unified tooltip system — one floating #labTip for every tooltip in
// the app.  Replaces the instant show/hide in dsprack.js with a full
// state machine: when a tooltip shows is tip-timing.js (1.5 s cold, at once
// while one is up and for 1 s after), the pointer leaving the window is
// tip-leave.js, keyboard focus support, and a MutationObserver that intercepts
// every title= write and moves the text to data-tip so the OS never
// shows a native tooltip.
//
// Lifecycle
//   initTooltip(badgeTipFn?) — call once after #labTip is in the DOM.
//     badgeTipFn(badgeEl) → HTML string or null — badge tooltip builder
//     injected by dsprack.js to keep FEAT_BY_ID out of this module.
//   hide()    — hide immediately and cancel any pending show.
//   armShow(el, x, y) — arm a delayed show for el at (x, y).
// ══════════════════════════════════════════════════════════════════════

import { createTipTiming } from './tip-timing.js';
import { placeTip } from './tip-place.js';
import { watchTipLeave } from './tip-leave.js';

let _tip          = null; // #labTip element
let _badgeTipFn   = null; // injected by dsprack.js

const _timing     = createTipTiming();
let _showTimer    = null;
let _leave        = null; // the pointer leaving the window (tip-leave.js)
let _tipFor       = null; // the element whose tooltip is up
let _byPointer    = false; // ...shown for the pointer (not for the focus)
let _activating   = false; // the window is getting the focus back

// ── MutationObserver: intercept title= writes ─────────────────────────

function _moveTitles(roots) {
    for (const el of roots) {
        el.querySelectorAll('[title]').forEach(_absorbTitle);
    }
}

function _absorbTitle(el) {
    const v = el.getAttribute('title');
    // v === null means the attribute was just removed (our own removeAttribute re-fired
    // the MO). Skip to avoid wiping the data-tip we set in the first fire.
    if (v === null) return;
    if (v) {
        el.dataset.tip = v;
    } else {
        delete el.dataset.tip;
    }
    el.removeAttribute('title');
}

function _startObserver() {
    const mo = new MutationObserver(recs => {
        for (const r of recs) {
            if (r.type === 'attributes' && r.attributeName === 'title') {
                _absorbTitle(r.target);
            } else if (r.type === 'childList') {
                r.addedNodes.forEach(n => {
                    if (n.nodeType === 1) {
                        if (n.hasAttribute('title')) _absorbTitle(n);
                        _moveTitles([n]);
                    }
                });
            }
        }
    });
    mo.observe(document.body, {
        attributes: true,
        attributeFilter: ['title'],
        childList: true,
        subtree: true,
    });
}

// ── content resolver ──────────────────────────────────────────────────

function _esc(s) {
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}

/// A plain data-tip as the tip's HTML: the text escaped (no markup from the
/// data gets through), its line breaks kept — a tip of several lines, the
/// batch's figures file by file, read as one paragraph.
export function plainTip(s) {
    return _esc(s).replace(/\r?\n/g, '<br>');
}

function tipContent(el) {
    // rack badge — delegate to dsprack's builder
    const b = el.closest('.lab-badge');
    if (b && _badgeTipFn) return _badgeTipFn(b);

    // inline chip with a full why explanation
    const c = el.closest('[data-lab-why]');
    if (c) {
        return `<b style="color:${c.dataset.labHue || '#7dd3fc'}">`
            + `${c.dataset.labTok} · ${c.dataset.labName}</b>${c.dataset.labWhy}`;
    }

    // plain data-tip — escape as text, its lines kept
    const d = el.closest('[data-tip]');
    if (d && d.dataset.tip) return plainTip(d.dataset.tip);

    return null;
}

// ── show / hide ───────────────────────────────────────────────────────

function _doShow(el, x, y) {
    if (!_tip) return;
    const content = tipContent(el);
    if (!content) return;
    _timing.shown();
    _tip.innerHTML = content;
    _tip.classList.add('show');
    // By the pointer; from the keyboard (focus), under the element.
    if (x == null) placeTip(_tip, el.getBoundingClientRect(), { side: 'below', gap: 6 });
    else placeTip(_tip, { left: x, top: y, right: x, bottom: y }, { side: 'pointer' });
    el.setAttribute('aria-describedby', 'labTip');
    _tipFor = el;
    _byPointer = x != null;
    // Up for the pointer: it goes once the pointer is out of the window.
    if (_byPointer) _leave?.up();
    else _leave?.down();
}

export function armShow(el, x, y) {
    _disarm();
    const delay = _timing.delay();
    if (delay === 0) _doShow(el, x, y);
    else _showTimer = setTimeout(() => {
        // For the pointer: only if it is still in the window (tip-leave.js).
        if (x != null && _leave) _leave.confirm(() => _doShow(el, x, y));
        else _doShow(el, x, y);
    }, delay);
}

/// Show el's tooltip at once, without the delay: a press on a control that
/// does nothing says why straight away (player.js, a control held while a
/// track is on). No position: under the element.
export function showNow(el, x = null, y = null) {
    _disarm();
    _doShow(el, x, y);
}

function _disarm() {
    clearTimeout(_showTimer);
    _leave?.cancel();
}

/// `cold`: the tooltip went away because of a click or a key, and the next
/// one waits the full delay again (tip-timing.js).
export function hide(cold = false) {
    _disarm();
    _leave?.down();
    _tipFor = null;
    if (!_tip) return;
    if (_tip.classList.contains('show') || cold) _timing.gone(cold);
    _tip.classList.remove('show');
    document.querySelectorAll('[aria-describedby="labTip"]')
        .forEach(el => el.removeAttribute('aria-describedby'));
}

// ── global event wiring ───────────────────────────────────────────────

function _wire() {
    document.addEventListener('mouseover', e => {
        if (e.target.closest('#labTip') || e.target.closest('#labMenu')) return;
        const tip = e.target.closest('.lab-badge, [data-lab-why], [data-tip]');
        if (tip) {
            // Into a child of the element whose tooltip is up: it stays.
            if (tip.getAttribute('aria-describedby') === 'labTip' && _tip?.classList.contains('show')) return;
            armShow(tip, e.clientX, e.clientY);
        } else if (_tip?.classList.contains('show') && (_byPointer || !_tipFor?.isConnected)) {
            // Over a spot with none while one is up for the pointer, or for an
            // element gone: that element was drawn anew under the pointer and
            // never said the pointer left it. It goes.
            hide();
        } else {
            _disarm();
        }
    });

    document.addEventListener('mouseout', e => {
        const tip = e.target.closest('.lab-badge, [data-lab-why], [data-tip]');
        if (!tip) return;
        // Moving between the element's own parts is not leaving it.
        if (e.relatedTarget && tip.contains(e.relatedTarget)) return;
        hide();
    });

    // The focus brings a tooltip when a key moved it (the focus ring shows).
    // Not a press of the mouse — the pointer has its own — and not the
    // window getting the focus back (Alt+Tab, the taskbar): the page then
    // focuses the button pressed last again, wherever the pointer is.
    document.addEventListener('focusin', e => {
        const tip = e.target.closest('.lab-badge, [data-lab-why], [data-tip]');
        if (tip && !_activating && _byKeyboard(e.target)) armShow(tip, null, null);
    });
    window.addEventListener('focus', () => {
        _activating = true;
        setTimeout(() => { _activating = false; }, 0);
    });

    document.addEventListener('focusout', () => hide());

    // hide on click, but do not suppress Tab (focusout already hides)
    document.addEventListener('click', () => hide(true));
    document.addEventListener('keydown', e => { if (e.key !== 'Tab') hide(true); });
    document.addEventListener('scroll', () => hide(), true);
    // The pointer out of the window, the window's focus lost, the window
    // minimized: it goes.
    _leave = watchTipLeave(() => hide());
}

function _byKeyboard(el) {
    try { return el.matches(':focus-visible'); } catch (_) { return true; }
}

// ── injectHtml helper ─────────────────────────────────────────────────
// Sets el.innerHTML and immediately walks the inserted subtree to move
// any title= attributes to data-tip, so the MO does not have to race.

export function injectHtml(el, html) {
    el.innerHTML = html;
    _moveTitles([el]);
}

// ── public init ───────────────────────────────────────────────────────

export function initTooltip(badgeTipFn) {
    _badgeTipFn = badgeTipFn || null;
    _tip = document.getElementById('labTip');

    // Walk existing title= attributes before the observer is active
    _moveTitles([document.body]);

    _startObserver();
    _wire();
}
