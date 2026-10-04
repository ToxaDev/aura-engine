// The one rule for every popup (popups.js): while one is open, a press
// outside it only puts it away, and nothing under the pointer hears it.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The page here is a small tree of nodes with the DOM's event path: the
// capture phase from the window down, the target, the bubbling back up;
// preventDefault, stopPropagation and stopImmediatePropagation as the DOM
// has them. A press is sent as the browser sends it (seen in Chromium 152):
// pointerdown; mousedown and mouseup only when pointerdown was not
// cancelled; pointerup; then click — or, for the right button, auxclick
// and contextmenu.

import test from 'node:test';
import assert from 'node:assert/strict';

import { createPopupGuard } from '../src/js/popups.js';

class Node {
    constructor(name, parent = null) {
        this.name = name;
        this.parent = parent;
        this.listeners = [];
    }
    contains(n) {
        for (let x = n; x; x = x.parent) if (x === this) return true;
        return false;
    }
    addEventListener(type, fn, opt) {
        this.listeners.push({ type, fn, capture: opt === true || !!(opt && opt.capture) });
    }
}

function dispatch(target, type, init = {}) {
    const path = [];
    for (let n = target; n; n = n.parent) path.unshift(n);
    let stopped = false, now = false;
    const ev = {
        type, target, isTrusted: true, button: 0, detail: 0, ...init,
        defaultPrevented: false,
        preventDefault() { this.defaultPrevented = true; },
        stopPropagation() { stopped = true; },
        stopImmediatePropagation() { stopped = true; now = true; },
    };
    const run = (node, capture) => {
        for (const l of [...node.listeners]) {
            if (l.type !== type || l.capture !== capture) continue;
            l.fn.call(node, ev);
            if (now) return;
        }
    };
    for (const n of path.slice(0, -1)) { if (stopped) break; run(n, true); }
    if (!stopped) run(target, true);
    if (!stopped) run(target, false);
    for (const n of path.slice(0, -1).reverse()) { if (stopped) break; run(n, false); }
    return ev;
}

function press(target, { button = 0, detail = 1, up = target } = {}) {
    const down = dispatch(target, 'pointerdown', { button });
    if (!down.defaultPrevented) dispatch(target, 'mousedown', { button, detail });
    dispatch(up, 'pointerup', { button });
    if (!down.defaultPrevented) dispatch(up, 'mouseup', { button, detail });
    const at = up === target ? target : target.parent;
    if (button === 0) dispatch(at, 'click', { button, detail });
    else {
        dispatch(at, 'auxclick', { button, detail });
        if (button === 2) dispatch(at, 'contextmenu', { button });
    }
    return down;
}

const key = (target, k) => dispatch(target, 'keydown', { key: k });

const HEARD = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'auxclick', 'contextmenu', 'dblclick'];

/// The page: a window, its document and body, a popup with a button in it,
/// its opener, a Play button and a list row; every press the elements hear
/// written down in `heard`.
function page() {
    const win = new Node('window');
    let list = null;
    win.document = new Node('document', win);
    win.document.querySelector = sel => (sel === 'select:open' ? list : null);
    const body = new Node('body', win.document);
    const pop = new Node('pop', body);
    const inPop = new Node('inPop', pop);
    const opener = new Node('opener', body);
    const play = new Node('play', body);
    const row = new Node('row', body);
    const heard = [];
    for (const n of [body, inPop, opener, play, row]) {
        for (const t of HEARD) n.addEventListener(t, e => { if (e.target === n) heard.push(`${n.name}:${t}`); });
    }
    // As main.js: a press on the window's empty parts starts moving it.
    const drags = [];
    win.document.addEventListener('mousedown', e => drags.push(e.target.name), true);
    const guard = createPopupGuard(win);
    let open = false;
    const closes = [];
    guard.add({
        isOpen: () => open,
        inside: t => pop.contains(t) || opener.contains(t),
        close: () => { closes.push('close'); open = false; },
    });
    // The opener toggles on its click, as the strip's cells and the
    // playlists' button do.
    opener.addEventListener('click', () => { open = !open; });
    return {
        win, body, pop, inPop, opener, play, row, heard, drags, guard, closes,
        get open() { return open; }, set open(v) { open = v; },
        setList(el) { list = el; },
    };
}

test('a press outside an open popup only puts it away: nothing under the pointer hears it', () => {
    const p = page();
    p.open = true;
    const down = press(p.play);
    assert.equal(p.open, false);
    assert.deepEqual(p.closes, ['close']);
    assert.deepEqual(p.heard, []);
    assert.equal(down.defaultPrevented, true, 'cancelled: no mousedown, no focus, no slider moved');
    assert.deepEqual(p.drags, [], 'the window does not start to move');
    // Closed now, the next press is Play's.
    press(p.play);
    assert.deepEqual(p.heard, ['play:pointerdown', 'play:mousedown', 'play:pointerup', 'play:mouseup', 'play:click']);
});

test('the right button outside only puts it away too: no auxclick, no context menu', () => {
    const p = page();
    p.open = true;
    let menus = 0;
    p.win.addEventListener('contextmenu', () => { menus++; });
    press(p.play, { button: 2 });
    assert.equal(p.open, false);
    assert.deepEqual(p.heard, []);
    assert.equal(menus, 0);
});

test('inside the popup everything works as before, and it stays open', () => {
    const p = page();
    p.open = true;
    const down = press(p.inPop);
    assert.equal(p.open, true);
    assert.equal(down.defaultPrevented, false);
    assert.deepEqual(p.heard, ['inPop:pointerdown', 'inPop:mousedown', 'inPop:pointerup', 'inPop:mouseup', 'inPop:click']);
});

test('the opener does what it did: its click puts the popup away, the guard does not', () => {
    const p = page();
    p.open = true;
    press(p.opener);
    assert.equal(p.open, false);
    assert.deepEqual(p.closes, []);
    assert.ok(p.heard.includes('opener:click'));
    press(p.opener);
    assert.equal(p.open, true);
});

test('with nothing open every press goes through untouched', () => {
    const p = page();
    const down = press(p.play);
    assert.equal(down.defaultPrevented, false);
    assert.deepEqual(p.heard, ['play:pointerdown', 'play:mousedown', 'play:pointerup', 'play:mouseup', 'play:click']);
    assert.deepEqual(p.drags, ['play']);
});

test('a press that begins outside and is let go inside is stopped whole', () => {
    const p = page();
    p.open = true;
    press(p.play, { up: p.inPop });
    assert.equal(p.open, false);
    assert.deepEqual(p.heard, []);
});

test('Esc puts the popup away and goes no further', () => {
    const p = page();
    let later = 0;
    // As the full screen's Esc (fullview.js), heard after the guard.
    p.win.addEventListener('keydown', () => { later++; }, true);
    p.open = true;
    const ev = key(p.body, 'Escape');
    assert.equal(p.open, false);
    assert.equal(later, 0);
    assert.equal(ev.defaultPrevented, true);
    // Nothing open: Esc is everyone else's.
    key(p.body, 'Escape');
    assert.equal(later, 1);
    // Other keys pass with a popup open.
    p.open = true;
    key(p.body, 'a');
    assert.equal(later, 2);
    assert.equal(p.open, true);
});

test('Esc does what the popup asks when it is more than close: a menu\'s page goes back first', () => {
    const win = new Node('window');
    win.document = new Node('document', win);
    win.document.querySelector = () => null;
    const body = new Node('body', win.document);
    const guard = createPopupGuard(win);
    let page = 'delay', open = true;
    guard.add({
        isOpen: () => open,
        inside: () => false,
        close: () => { open = false; },
        escape: () => { if (page !== 'main') page = 'main'; else open = false; },
    });
    key(body, 'Escape');
    assert.deepEqual([page, open], ['main', true]);
    key(body, 'Escape');
    assert.equal(open, false);
});

test('an open list (select:open): the press outside is stopped, the browser puts the list away', () => {
    const p = page();
    const sel = new Node('select', p.body);
    const option = new Node('option', sel);
    for (const t of HEARD) option.addEventListener(t, e => p.heard.push(`option:${t}`));
    p.setList(sel);
    press(p.play);
    assert.deepEqual(p.heard, []);
    // Its own rows are pressed as ever.
    press(option);
    assert.ok(p.heard.includes('option:click'));
});

test('a list put away by the press does not keep the focus on its hidden row', () => {
    const p = page();
    const sel = new Node('select', p.body);
    const row = new Node('option', sel);
    const doc = p.win.document;
    doc.body = p.body;
    const focus = n => { doc.activeElement = n; n.blur = () => { if (doc.activeElement === n) doc.activeElement = doc.body; }; };
    // Open, its row focused (where Space or Enter would pick it).
    focus(row);
    p.setList(sel);
    dispatch(p.play, 'pointerdown');
    assert.equal(doc.activeElement, row, 'not while the press is held');
    p.setList(null);               // the browser put the list away
    dispatch(p.play, 'pointerup');
    assert.equal(doc.activeElement, p.body);
    // The list itself keeping the focus is left so (as after Esc), and a
    // press inside the list moves nothing.
    focus(sel);
    p.setList(sel);
    press(p.play);
    assert.equal(doc.activeElement, sel);
    focus(row);
    p.setList(sel);
    press(row);
    assert.equal(doc.activeElement, row);
});

test('Esc with a list open is the browser\'s: the popup goes with the next one', () => {
    const p = page();
    const sel = new Node('select', p.body);
    p.open = true;
    p.setList(sel);
    const ev = key(p.body, 'Escape');
    assert.equal(p.open, true);
    assert.equal(ev.defaultPrevented, false);
    p.setList(null);
    key(p.body, 'Escape');
    assert.equal(p.open, false);
});

test('a double click whose first press only put a popup away is no double click', () => {
    const p = page();
    p.open = true;
    press(p.row, { detail: 1 });
    press(p.row, { detail: 2 });
    dispatch(p.row, 'dblclick', { detail: 2 });
    assert.equal(p.open, false);
    // The second press is a press like any other; the double click (a row
    // plays on it) is not heard.
    assert.ok(p.heard.includes('row:click'));
    assert.ok(!p.heard.includes('row:dblclick'));
    // Two presses with nothing open are a double click again.
    p.heard.length = 0;
    press(p.row, { detail: 1 });
    press(p.row, { detail: 2 });
    dispatch(p.row, 'dblclick', { detail: 2 });
    assert.ok(p.heard.includes('row:dblclick'));
});

test('a key\'s click after a stopped press is the key\'s', () => {
    const p = page();
    p.open = true;
    press(p.play);
    assert.deepEqual(p.heard, []);
    key(p.play, ' ');
    dispatch(p.play, 'click', { detail: 0 });
    assert.deepEqual(p.heard, ['play:click']);
});

test('a key pressed while the stopped press is held does not let its click through', () => {
    const p = page();
    p.open = true;
    dispatch(p.play, 'pointerdown');
    key(p.play, 'Shift');
    dispatch(p.play, 'pointerup');
    dispatch(p.play, 'click', { detail: 1 });
    assert.deepEqual(p.heard, []);
});

test('the rest of a stopped press is looked for a second after it, no longer', () => {
    const p = page();
    const now = Date.now;
    let t = 1000000;
    Date.now = () => t;
    try {
        p.open = true;
        dispatch(p.play, 'pointerdown');
        t += 5000;                       // held as long as the hand likes
        dispatch(p.play, 'pointerup');
        dispatch(p.play, 'click', { detail: 1 });
        assert.deepEqual(p.heard, [], 'its own click, with the release');
        // A click with neither a press nor a key before it (a screen
        // reader's), long after: not the stopped press's.
        t += 1500;
        dispatch(p.play, 'click', { detail: 1 });
        assert.deepEqual(p.heard, ['play:click']);
    } finally {
        Date.now = now;
    }
});

test('a window left with the button down: a key\'s click afterwards is the key\'s', () => {
    const p = page();
    p.open = true;
    dispatch(p.play, 'pointerdown');
    dispatch(p.win, 'blur');
    key(p.play, 'Enter');
    dispatch(p.play, 'click', { detail: 0 });
    assert.deepEqual(p.heard, ['play:click']);
});

test('a click the page makes itself is never stopped', () => {
    const p = page();
    p.open = true;
    press(p.play);
    dispatch(p.play, 'click', { isTrusted: false });
    assert.deepEqual(p.heard, ['play:click']);
});

test('with two open, a press inside one puts the other away and goes through', () => {
    const p = page();
    const other = new Node('other', p.body);
    let otherOpen = true;
    p.guard.add({ isOpen: () => otherOpen, inside: t => other.contains(t), close: () => { otherOpen = false; } });
    p.open = true;
    press(p.inPop);
    assert.equal(p.open, true);
    assert.equal(otherOpen, false);
    assert.ok(p.heard.includes('inPop:click'));
});

test('a popup taken off is not guarded any more', () => {
    const win = new Node('window');
    win.document = new Node('document', win);
    win.document.querySelector = () => null;
    const body = new Node('body', win.document);
    const play = new Node('play', body);
    let clicks = 0;
    play.addEventListener('click', () => { clicks++; });
    const guard = createPopupGuard(win);
    const off = guard.add({ isOpen: () => true, inside: () => false, close() {} });
    press(play);
    assert.equal(clicks, 0);
    off();
    press(play);
    assert.equal(clicks, 1);
});

test('a popup whose close throws does not take the guard down', () => {
    const p = page();
    const err = console.error;
    console.error = () => {};
    try {
        p.guard.add({ isOpen: () => true, inside: () => false, close: () => { throw new Error('boom'); } });
        p.open = true;
        press(p.play);
    } finally {
        console.error = err;
    }
    assert.equal(p.open, false);
    assert.deepEqual(p.heard, []);
});
