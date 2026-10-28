// The main window's tooltip (tooltip.js) when the pointer leaves the window:
// it goes, and one on its way does not come (Anton 16.10: a tooltip came up,
// or stayed, with the pointer out of the window, over the app's controls).
// Inside the window it behaves as it did.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test, { mock } from 'node:test';
import assert from 'node:assert/strict';

mock.timers.enable({ apis: ['setTimeout', 'setInterval', 'Date'], now: 1e12 });

// ── a page small enough for tooltip.js ────────────────────────────────

const MATCH = {
    '.lab-badge': e => e.cls.has('lab-badge'),
    '[data-lab-why]': e => 'labWhy' in e.dataset,
    '[data-tip]': e => 'tip' in e.dataset,
    '[title]': e => 'title' in e.attrs,
    '[aria-describedby="labTip"]': e => e.attrs['aria-describedby'] === 'labTip',
    '#labTip': e => e.id === 'labTip',
    '#labMenu': e => e.id === 'labMenu',
    ':focus-visible': e => e.focusVisible,
};
const matches = (e, sel) => sel.split(',').some(s => MATCH[s.trim()]?.(e));

class El {
    constructor({ id = '', tip } = {}) {
        this.id = id;
        this.dataset = {};
        if (tip != null) this.dataset.tip = tip;
        this.attrs = {};
        this.cls = new Set();
        this.classList = {
            add: (...a) => a.forEach(c => this.cls.add(c)),
            remove: (...a) => a.forEach(c => this.cls.delete(c)),
            contains: c => this.cls.has(c),
            toggle: (c, on) => { if (on ?? !this.cls.has(c)) this.cls.add(c); else this.cls.delete(c); },
        };
        this.style = {};
        this.children = [];
        this.parentNode = null;
        this.focusVisible = false;
        this.innerHTML = '';
    }
    append(...cs) { for (const c of cs) { c.parentNode = this; this.children.push(c); } return this; }
    remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter(c => c !== this); this.parentNode = null; }
    get isConnected() { let e = this; while (e.parentNode) e = e.parentNode; return e === body; }
    closest(sel) { for (let e = this; e; e = e.parentNode) if (matches(e, sel)) return e; return null; }
    matches(sel) { return matches(this, sel); }
    contains(o) { for (let e = o; e; e = e.parentNode) if (e === this) return true; return false; }
    hasAttribute(k) { return k in this.attrs; }
    getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
    setAttribute(k, v) { this.attrs[k] = String(v); }
    removeAttribute(k) { delete this.attrs[k]; }
    querySelectorAll(sel) {
        const out = [];
        const walk = e => { for (const c of e.children) { if (matches(c, sel)) out.push(c); walk(c); } };
        walk(this);
        return out;
    }
    getBoundingClientRect() { return { left: 0, top: 0, right: 60, bottom: 20, width: 60, height: 20 }; }
}

const body = new El();
const tipEl = new El({ id: 'labTip' });
body.append(tipEl);

const handlers = { doc: {}, win: {} };
const on = (where) => (type, fn) => { (handlers[where][type] ||= []).push(fn); };
globalThis.document = {
    body,
    visibilityState: 'visible',
    addEventListener: on('doc'),
    removeEventListener() {},
    getElementById: id => (id === 'labTip' ? tipEl : null),
    querySelectorAll: sel => body.querySelectorAll(sel),
};
globalThis.window = { innerWidth: 440, innerHeight: 900, outerWidth: 440, outerHeight: 900, addEventListener: on('win'), removeEventListener() {} };
globalThis.MutationObserver = class { observe() {} disconnect() {} };

const fire = (where, type, e = {}) => { for (const fn of handlers[where][type] || []) fn({ relatedTarget: null, clientX: 10, clientY: 10, ...e }); };
const over = (target, from = null) => fire('doc', 'mouseover', { target, relatedTarget: from });
const out = (target, to = null) => fire('doc', 'mouseout', { target, relatedTarget: to });
const shown = () => tipEl.classList.contains('show');
/** Time passes; what the backend answered in the meantime arrives. */
async function wait(ms) {
    mock.timers.tick(ms);
    for (let i = 0; i < 5; i++) await new Promise(r => setImmediate(r));
}

// The backend that says where the pointer is (ui_cursor_client), or none.
let cursor = [10, 10];
const backend = { core: { invoke: async (cmd) => { assert.equal(cmd, 'ui_cursor_client'); return cursor; } } };

const { initTooltip, hide } = await import('../src/js/tooltip.js');
initTooltip();

// A panel with two buttons that have tooltips, one with a part inside, and
// a bare spot.
function page() {
    body.children = [tipEl];
    const panel = new El();
    const a = new El({ tip: 'Play' });
    const aIcon = new El();
    a.append(aIcon);
    const b = new El({ tip: 'Stop' });
    const bare = new El();
    panel.append(a, b, bare);
    body.append(panel);
    return { panel, a, aIcon, b, bare };
}

function reset() {
    hide(true);
    cursor = [10, 10];
    delete window.__TAURI__;
    document.visibilityState = 'visible';
}

// ── inside the window: as before ──────────────────────────────────────

test('inside the window: 1.5 s the first time, at once along a row, a part of the button keeps it', async () => {
    reset();
    const { a, aIcon, b, bare } = page();
    over(a);
    await wait(1499);
    assert.equal(shown(), false);
    await wait(1);
    assert.equal(shown(), true);
    out(a, aIcon); over(aIcon, a);              // into the button's own icon
    assert.equal(shown(), true);
    out(aIcon, b); over(b, aIcon);              // the next button: at once
    assert.equal(shown(), true);
    assert.equal(tipEl.innerHTML, 'Stop');
    out(b, bare); over(bare, b);                // off the buttons
    assert.equal(shown(), false);
});

test('inside the window with the backend asked: the same', async () => {
    reset();
    window.__TAURI__ = backend;
    const { a, b, bare } = page();
    over(a);
    await wait(1500);
    assert.equal(shown(), true);
    await wait(1000);                           // read at leisure: it stays
    assert.equal(shown(), true);
    out(a, b); over(b, a);
    assert.equal(shown(), true);
    out(b, bare); over(bare, b);
    assert.equal(shown(), false);
});

test('a key moves the focus: the tooltip of the button it reaches, under it, as before', async () => {
    reset();
    const { b } = page();
    b.focusVisible = true;
    fire('doc', 'keydown', { key: 'Tab', target: body });
    fire('doc', 'focusin', { target: b });
    await wait(1500);
    assert.equal(shown(), true);
    assert.equal(tipEl.innerHTML, 'Stop');
});

// ── leaving the window ────────────────────────────────────────────────

test('a tooltip on its way does not come when the pointer leaves the page from a spot without one', async () => {
    reset();
    const { panel, a } = page();
    over(a);
    // The button is drawn anew under the pointer; the page is left from the panel.
    a.remove();
    out(panel);
    await wait(1500);
    assert.equal(shown(), false);
});

test('a tooltip that is up goes when the pointer leaves the page, whatever it was over last', async () => {
    reset();
    const { panel, a } = page();
    over(a);
    await wait(1500);
    assert.equal(shown(), true);
    a.remove();                                 // drawn anew: the old one says nothing
    out(panel);
    assert.equal(shown(), false);
});

test('a tooltip whose button is gone does not stay over a bare spot', async () => {
    reset();
    const { a, bare } = page();
    over(a);
    await wait(1500);
    assert.equal(shown(), true);
    a.remove();
    over(bare);                                 // what is under the pointer now
    assert.equal(shown(), false);
});

test('the page told nothing, the backend says the pointer is out: none shows, and one that is up goes', async () => {
    reset();
    window.__TAURI__ = backend;
    const { a, b } = page();
    over(a);
    cursor = [-40, 300];                        // past the window's left edge, no event
    await wait(1500);
    assert.equal(shown(), false);

    cursor = [10, 10];
    over(b);
    await wait(1500);
    assert.equal(shown(), true);
    cursor = [470, 300];                        // past the right edge (the window is 440 wide)
    await wait(250);
    assert.equal(shown(), false);
});

test('the window loses focus or is minimized: the tooltip goes, and one on its way does not come', async () => {
    reset();
    const { a, b } = page();
    over(a);
    await wait(1500);
    assert.equal(shown(), true);
    document.visibilityState = 'hidden';
    fire('doc', 'visibilitychange');
    assert.equal(shown(), false);

    reset();
    over(b);
    fire('win', 'blur');
    await wait(1500);
    assert.equal(shown(), false);
});

test('coming back to the window brings up no tooltip of the button pressed last', async () => {
    reset();
    const { a, b } = page();
    // Pressed with the mouse: focused, no focus ring.
    fire('doc', 'focusin', { target: a });
    fire('doc', 'click', { target: a });
    fire('doc', 'focusout', { target: a });
    fire('win', 'blur');
    await wait(5000);
    // Back (Alt+Tab, the taskbar): the page focuses the window, then the button again.
    fire('win', 'focus');
    fire('doc', 'focusin', { target: a });
    await wait(1500);
    assert.equal(shown(), false);

    // Reached with a key before leaving: no tooltip on the way back either.
    b.focusVisible = true;
    fire('doc', 'focusout', { target: b });
    fire('win', 'blur');
    await wait(5000);
    fire('win', 'focus');
    fire('doc', 'focusin', { target: b });
    await wait(1500);
    assert.equal(shown(), false);
});
