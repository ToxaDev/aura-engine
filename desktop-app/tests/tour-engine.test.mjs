// The tour's engine (tour-engine.js) on a page made of the least it needs:
// what happens when the listener moves faster than the layout switches.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

// ── a page, as small as the engine allows ─────────────────────────────

class ClassList {
    constructor() { this.s = new Set(); }
    add(...c) { c.forEach(x => this.s.add(x)); }
    remove(...c) { c.forEach(x => this.s.delete(x)); }
    contains(c) { return this.s.has(c); }
    toggle(c, on) { if (on ?? !this.s.has(c)) this.s.add(c); else this.s.delete(c); }
}

class El {
    constructor(tag) {
        this.tagName = tag.toUpperCase();
        this.childNodes = [];
        this.parentNode = null;
        this.classList = new ClassList();
        this.style = {};
        this.dataset = {};
        this.attrs = {};
        this.listeners = {};
        this.hidden = false;
        this._text = '';
    }
    set className(v) { this.classList = new ClassList(); String(v).split(/\s+/).filter(Boolean).forEach(c => this.classList.add(c)); }
    get className() { return [...this.classList.s].join(' '); }
    set textContent(v) { this._text = String(v); this.childNodes = []; }
    get textContent() { return this._text + this.childNodes.map(c => c.textContent).join(''); }
    setAttribute(k, v) { this.attrs[k] = String(v); }
    getAttribute(k) { return this.attrs[k] ?? null; }
    appendChild(c) { c.remove?.(); c.parentNode = this; this.childNodes.push(c); return c; }
    append(...cs) { cs.forEach(c => this.appendChild(c)); }
    replaceChildren(...cs) { this.childNodes.forEach(c => { c.parentNode = null; }); this.childNodes = []; this._text = ''; this.append(...cs); }
    remove() { if (this.parentNode) { const p = this.parentNode; p.childNodes = p.childNodes.filter(c => c !== this); this.parentNode = null; } }
    get lastChild() { return this.childNodes.at(-1) || null; }
    get isConnected() { let n = this; while (n.parentNode) n = n.parentNode; return n === document.documentElement; }
    contains(o) { for (let n = o; n; n = n.parentNode) if (n === this) return true; return false; }
    addEventListener(t, f) { (this.listeners[t] ||= []).push(f); }
    removeEventListener(t, f) { this.listeners[t] = (this.listeners[t] || []).filter(x => x !== f); }
    click() { (this.listeners.click || []).forEach(f => f({ target: this })); }
    focus() { document.activeElement = this; }
    blur() { if (document.activeElement === this) document.activeElement = document.body; }
    animate() { return { finished: Promise.resolve(), cancel() {} }; }
    getAnimations() { return []; }
    getBoundingClientRect() { const r = this.rect || { x: 0, y: 0, w: this.isConnected ? 100 : 0, h: this.isConnected ? 20 : 0 }; return { left: r.x, top: r.y, width: r.w, height: r.h, right: r.x + r.w, bottom: r.y + r.h }; }
    get offsetWidth() { return 286; }
    get offsetHeight() { return 120; }
    scrollIntoView() {}
}

function installPage() {
    const win = {};
    globalThis.document = {
        createElement: t => new El(t),
        createElementNS: (_, t) => new El(t),
        createTextNode: t => { const n = new El('#text'); n.textContent = t; return n; },
        getElementById: () => null,
        activeElement: null,
    };
    document.documentElement = new El('html');
    document.body = new El('body');
    document.documentElement.appendChild(document.body);
    document.activeElement = document.body;
    win.listeners = {};
    globalThis.window = Object.assign(globalThis, {
        innerWidth: 440, innerHeight: 900,
        addEventListener: (t, f) => { (win.listeners[t] ||= []).push(f); },
        removeEventListener: (t, f) => { win.listeners[t] = (win.listeners[t] || []).filter(x => x !== f); },
        matchMedia: () => ({ matches: false }),
    });
    let frames = [];
    globalThis.requestAnimationFrame = f => { frames.push(f); return frames.length; };
    globalThis.cancelAnimationFrame = () => {};
    globalThis.getComputedStyle = () => ({ borderTopLeftRadius: '6px' });
    /// Run the frames asked for so far (the engine asks for the next one in each).
    win.tick = (n = 1) => { for (let k = 0; k < n; k++) { const f = frames; frames = []; f.forEach(fn => fn(performance.now())); } };
    win.key = key => (win.listeners.keydown || []).forEach(f => f({ key, repeat: false, target: document.activeElement, preventDefault() {}, stopImmediatePropagation() {} }));
    return win;
}

/// Two steps, the second the listener's to do (as the main tour's last:
/// the big player's edge, pressed to fold it). `folded` is the window's
/// answer to "was it pressed". `prepMs`: the second lives in another
/// layout, which takes that long to come; `over`: the step's own question.
function makeLiveTour(createTour, { prepMs = 0, over = null } = {}) {
    const els = [0, 1].map(k => { const e = new El('div'); e.rect = { x: 10, y: 100 + 200 * k, w: 200, h: 14 }; document.body.appendChild(e); return e; });
    const help = new El('button');
    help.rect = { x: 9, y: 24, w: 15, h: 15 };
    document.body.appendChild(help);
    const page = { folded: false, records: [], ends: [], layout: 'studio' };
    const prep = prepMs > 0 ? {
        needsPrep: () => page.layout !== 'listening',
        prepare: async () => { page.layout = 'listening'; await sleep(prepMs); },
    } : {};
    const tour = createTour({
        name: 'live',
        texts: {
            offer: { title: 'o', body: 'o', start: 'Start', skip: 'Skip' },
            again: { title: 'a', body: 'a', start: 'Start', cancel: 'Not now' },
            nav: { next: 'Next', back: 'Back', done: 'Done', skip: 'Skip tour', count: (i, n) => `${i}/${n}` },
        },
        steps: [
            { id: 'one', title: 'one', body: () => 'the {?} button brings it back', side: ['bottom'], pad: 4, targets: () => [els[0]] },
            { id: 'fold', title: 'fold', body: () => 'press it', side: ['bottom'], pad: 3, targets: () => [els[1]],
              live: true, keys: ['KeyL'], over: over || (() => page.folded), ...prep },
        ],
        help: () => help,
        begin() {},
        end(kind) { page.ends.push(kind); return false; },
        record: state => page.records.push(state),
    });
    return { tour, page, els };
}

const layerOf = () => document.body.childNodes.find(n => n.classList.contains('tour-layer'));
const sheetOf = layer => layer.childNodes.find(n => n.classList.contains('tour-block'));

const sleep = ms => new Promise(r => setTimeout(r, ms));

/// Three steps: the second lives in another layout, which takes `prepMs`
/// to come (the listening mode's switch).
function makeTour(createTour, { prepMs = 120 } = {}) {
    const els = [0, 1, 2].map(k => { const e = new El('div'); e.rect = { x: 10, y: 100 + 200 * k, w: 200, h: 40 }; document.body.appendChild(e); return e; });
    let layout = 'studio';
    const help = new El('button');
    help.rect = { x: 9, y: 24, w: 15, h: 15 };
    document.body.appendChild(help);
    const step = (id, k, mode) => ({
        id, title: id, body: () => id, side: ['bottom', 'top'], pad: 4,
        targets: () => [els[k]],
        needsPrep: () => layout !== mode,
        // As listening.js does it: the layout's class at once, then the
        // animation runs; a step asked about meanwhile is already there.
        prepare: async () => { layout = mode; await sleep(prepMs); },
    });
    const tour = createTour({
        name: 'test',
        texts: {
            offer: { title: 'o', body: 'o', start: 'Start', skip: 'Skip' },
            again: { title: 'a', body: 'a', start: 'Start', cancel: 'Not now' },
            nav: { next: 'Next', back: 'Back', done: 'Done', skip: 'Skip tour', count: (i, n) => `${i}/${n}` },
        },
        steps: [step('one', 0, 'studio'), step('two', 1, 'listening'), step('three', 2, 'listening')],
        help: () => help,
        record: () => {},
    });
    return { tour, layoutNow: () => layout };
}

test('two quick Nexts over a layout switch: the tour does not stay dark and dead', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=${Date.now()}`);
    const { tour } = makeTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    page.tick(2);
    assert.equal(tour.state().step, 'one');
    tour.next();               // into the other layout: the dim lifts while it switches
    await sleep(220);          // the layout has switched, its animation still runs
    tour.next();               // again, before the switch is over
    await sleep(400);
    page.tick(3);
    const st = tour.state();
    const layer = document.body.childNodes.find(n => n.classList.contains('tour-layer'));
    assert.ok(layer, 'the tour is still up');
    assert.equal(st.switching, false, 'the switch is over');
    assert.ok(!layer.classList.contains('tour-switching'), 'the dim and the tip are back');
    assert.ok(['two', 'three'].includes(st.step), `on a step: ${st.step}`);
    tour.abort();
});

test('Back in the middle of a switch is passed over, the switch still ends', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=back${Date.now()}`);
    const { tour } = makeTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    tour.next();
    await sleep(30);
    tour.back();
    await sleep(400);
    page.tick(3);
    const layer = document.body.childNodes.find(n => n.classList.contains('tour-layer'));
    assert.equal(tour.state().switching, false);
    assert.ok(!layer.classList.contains('tour-switching'));
    assert.equal(tour.state().step, 'two', 'the move under way finished');
    tour.abort();
});

test('the update dialog over the tour keeps the keys', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=dialog${Date.now()}`);
    const { tour } = makeTour(createTour);
    const up = new El('div');
    document.body.appendChild(up);                       // shown: no up-hidden
    document.getElementById = id => (id === 'upModal' ? up : null);
    tour.offer('again');
    page.key('Enter');
    page.key('Escape');
    assert.equal(tour.state().phase, 'offer', 'Enter and Esc went to the dialog, not to the tour');
    up.classList.add('up-hidden');
    page.key('Enter');
    await sleep(10);
    assert.equal(tour.state().phase, 'steps', 'the dialog gone, Enter is the tour\'s again');
    tour.abort();
});

test('flying into the ?, the tour lets the keys go, all but the release of the one that closed it', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=closing${Date.now()}`);
    const { tour } = makeTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    page.tick(2);
    const ev = key => ({ key, repeat: false, target: document.body, stopped: false,
        preventDefault() {}, stopImmediatePropagation() { this.stopped = true; } });
    const down = k => { const e = ev(k); (page.listeners.keydown || []).forEach(f => f(e)); return e; };
    const up = k => { const e = ev(k); (page.listeners.keyup || []).forEach(f => f(e)); return e; };
    assert.equal(down('l').stopped, true, 'during the tour the window\'s keys wait');
    down('Escape');                       // skip: the tour flies into the ?
    assert.equal(tour.state().phase, 'closing');
    assert.equal(down(' ').stopped, false, 'Space goes to the window during the flight');
    assert.equal(up('Escape').stopped, true, 'the release of Esc is the tour\'s');
    assert.equal(up(' ').stopped, false);
    tour.abort();
});

test('a live step opens the sheet over what it points at, and only there', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=live${Date.now()}`);
    const { tour, els } = makeLiveTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    page.tick(2);
    const sheet = sheetOf(layerOf());
    assert.equal(tour.state().step, 'one');
    assert.ok(!sheet.style.clipPath, 'an ordinary step: the sheet takes every click');
    tour.next();
    await sleep(320);              // the hole glides 280 ms
    page.tick(2);
    assert.equal(tour.state().step, 'fold');
    assert.equal(tour.state().live, true);
    // Over the target itself, not over the room its hole keeps round it.
    const r = els[1].rect;
    const hole = `M${r.x.toFixed(1)} ${r.y.toFixed(1)}h${r.w.toFixed(1)}v${r.h.toFixed(1)}h${(-r.w).toFixed(1)}Z`;
    assert.ok(String(sheet.style.clipPath).includes(hole), `open over the target: ${sheet.style.clipPath}`);
    assert.match(String(sheet.style.clipPath), /^path\(evenodd,/, 'the rest of the sheet stays');
    tour.back();
    await sleep(320);
    page.tick(2);
    assert.ok(!sheet.style.clipPath, 'Back: whole again');
    tour.abort();
});

test('the listener does what the live step asks: the tour is done', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=over${Date.now()}`);
    const { tour, page: win } = makeLiveTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    tour.next();
    await sleep(10);
    page.tick(2);
    assert.equal(tour.state().phase, 'steps', 'not pressed yet: the step waits');
    const sheet = sheetOf(layerOf());
    assert.ok(sheet.style.clipPath, 'the sheet open over the target');
    win.folded = true;             // the window: pressed
    page.tick(1);
    assert.equal(tour.state().phase, 'closing', 'the tour goes');
    assert.ok(!sheet.style.clipPath, 'flying away, the sheet is whole: a second click lands on it');
    assert.deepEqual(win.records, ['done']);
    assert.deepEqual(win.ends, ['done']);
    await sleep(10);
    assert.ok(!layerOf(), 'and it is gone');
});

test('Done on the live step ends the tour as done', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=done${Date.now()}`);
    const { tour, page: win } = makeLiveTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    tour.next();
    await sleep(10);
    page.tick(2);
    const next = layerOf().childNodes.find(n => n.classList.contains('tour-tip'))
        .childNodes.find(n => n.classList.contains('tour-f')).childNodes.find(n => n.classList.contains('tour-next'));
    assert.equal(next.textContent, 'Done');
    next.click();
    assert.deepEqual(win.records, ['done']);
    assert.deepEqual(win.ends, ['done'], 'the window folds it (tour.js end)');
    await sleep(10);
    assert.ok(!layerOf());
});

test('a key a step hands to the window goes on to it; on other steps it waits', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=pass${Date.now()}`);
    const { tour } = makeLiveTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    const down = (key, code) => {
        const e = { key, code, repeat: false, target: document.body, stopped: false,
            preventDefault() {}, stopImmediatePropagation() { this.stopped = true; } };
        (page.listeners.keydown || []).forEach(f => f(e));
        return e;
    };
    assert.equal(down('l', 'KeyL').stopped, true, 'step one: L waits');
    tour.next();
    await sleep(10);
    assert.equal(tour.state().step, 'fold');
    assert.equal(down('l', 'KeyL').stopped, false, 'the live step: L goes to the window');
    assert.equal(down('d', 'KeyD').stopped, true, 'other keys still wait');
    tour.abort();
});

test('a button named by its sign is drawn as a key, the words around it as text', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=chip${Date.now()}`);
    const { tour } = makeLiveTour(createTour);
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    const body = layerOf().childNodes.find(n => n.classList.contains('tour-tip')).childNodes.find(n => n.classList.contains('tour-b'));
    const chips = body.childNodes.filter(n => n.classList.contains('tour-chip'));
    assert.equal(chips.length, 1);
    assert.equal(chips[0].textContent, '?');
    assert.equal(body.textContent, 'the ? button brings it back', 'reads as one line');
    tour.next();
    await sleep(10);
    assert.equal(body.textContent, 'press it');
    assert.equal(body.childNodes.length, 0, 'a plain text is plain text');
    tour.abort();
});

test('a step\'s question that throws is asked again, and none is asked once the tour is going', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=overthrow${Date.now()}`);
    let calls = 0, answer = false;
    const { tour, page: win } = makeLiveTour(createTour, {
        over: () => { calls++; if (calls === 1) throw new Error('not yet'); return answer; },
    });
    tour.offer('again');
    page.tick(2);
    assert.equal(calls, 0, 'not asked on the offer');
    tour.start();
    await sleep(10);
    tour.next();
    await sleep(10);
    page.tick(1);
    assert.equal(calls, 1);
    assert.equal(tour.state().phase, 'steps', 'a throw is no answer');
    page.tick(1);
    assert.equal(calls, 2, 'asked again on the next frame');
    answer = true;
    page.tick(1);
    assert.equal(tour.state().phase, 'closing');
    const after = calls;
    page.tick(3);
    assert.equal(calls, after, 'not asked while the tour flies away');
    assert.deepEqual(win.records, ['done']);
    await sleep(10);
});

test('a key the live step hands over waits while the layout is still switching to it', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=passswitch${Date.now()}`);
    const { tour } = makeLiveTour(createTour, { prepMs: 150 });
    tour.offer('again');
    page.tick(2);
    tour.start();
    await sleep(10);
    tour.next();                   // into the other layout
    await sleep(20);
    assert.equal(tour.state().switching, true);
    const e = { key: 'l', code: 'KeyL', repeat: false, target: document.body, stopped: false,
        preventDefault() {}, stopImmediatePropagation() { this.stopped = true; } };
    (page.listeners.keydown || []).forEach(f => f(e));
    assert.equal(e.stopped, true, 'held while the layout switches');
    await sleep(400);
    assert.equal(tour.state().step, 'fold');
    assert.equal(tour.state().switching, false);
    tour.abort();
});

test('the window\'s own tooltips wait while a tour is up', async () => {
    installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=tips${Date.now()}`);
    const { tour } = makeLiveTour(createTour);
    assert.ok(!document.body.classList.contains('tour-up'));
    tour.offer('again');
    assert.ok(document.body.classList.contains('tour-up'), 'up: tour.css hides #labTip');
    tour.abort();
    assert.ok(!document.body.classList.contains('tour-up'), 'gone: tooltips again');
});

test('the keys come off with the tour', async () => {
    const page = installPage();
    const { createTour } = await import(`../src/js/tour-engine.js?case=keys${Date.now()}`);
    const { tour } = makeTour(createTour);
    tour.offer('again');
    assert.equal((page.listeners.keydown || []).length, 1);
    tour.abort();
    assert.equal((page.listeners.keydown || []).length, 0);
    assert.equal((page.listeners.keyup || []).length, 0);
});
