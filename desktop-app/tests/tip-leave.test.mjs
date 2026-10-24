// The pointer leaving the window, for a tooltip (tip-leave.js): where the
// backend says the pointer is, and what the watcher does with it.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { pointerInWindow, watchTipLeave, POLL_MS } from '../src/js/tip-leave.js';

const settle = async () => { for (let i = 0; i < 5; i++) await new Promise(r => setImmediate(r)); };

function fakeWindow(answer, size = {}) {
    const calls = [];
    return {
        calls,
        innerWidth: 440, innerHeight: 900, outerWidth: 440, outerHeight: 900, ...size,
        __TAURI__: { tauri: { invoke: async (cmd) => { calls.push(cmd); if (answer instanceof Error) throw answer; return answer; } } },
    };
}

test('in the window or out of it, by the window\'s client area', async () => {
    assert.equal(await pointerInWindow(fakeWindow([0, 0])), true);
    assert.equal(await pointerInWindow(fakeWindow([439, 899])), true);
    assert.equal(await pointerInWindow(fakeWindow([440, 10])), false);
    assert.equal(await pointerInWindow(fakeWindow([10, 900])), false);
    assert.equal(await pointerInWindow(fakeWindow([-1, 10])), false);
    assert.equal(await pointerInWindow(fakeWindow([10, -30])), false);
    const w = fakeWindow([5, 5]);
    await pointerInWindow(w);
    assert.deepEqual(w.calls, ['ui_cursor_client']);
});

test('the page zoomed in: its own right edge is still in the window', async () => {
    // At 125 % the page is 352 CSS pixels wide in a 440 wide window.
    assert.equal(await pointerInWindow(fakeWindow([400, 10], { innerWidth: 352, innerHeight: 720 })), true);
    assert.equal(await pointerInWindow(fakeWindow([441, 10], { innerWidth: 352, innerHeight: 720 })), false);
});

test('no answer: no backend, an older one, no point', async () => {
    assert.equal(pointerInWindow({ innerWidth: 440, innerHeight: 900 }), null);
    assert.equal(await pointerInWindow(fakeWindow(new Error('unknown command'))), null);
    assert.equal(await pointerInWindow(fakeWindow(null)), null);
});

test('a point the rw checks pin answers at once, without the backend', () => {
    const w = fakeWindow([-50, -50]);
    w.__auraPinCursor = [20, 30];
    assert.equal(pointerInWindow(w), true);
    w.__auraPinCursor = [600, 30];
    assert.equal(pointerInWindow(w), false);
    assert.deepEqual(w.calls, []);
});

function fakePage() {
    const h = { doc: {}, win: {} };
    const on = where => (type, fn) => { (h[where][type] ||= []).push(fn); };
    const doc = { visibilityState: 'visible', addEventListener: on('doc') };
    const win = { addEventListener: on('win') };
    const fire = (where, type, e = {}) => { for (const fn of h[where][type] || []) fn(e); };
    return { doc, win, fire };
}

test('the page left, the focus lost, the window hidden: leave()', () => {
    const { doc, win, fire } = fakePage();
    let left = 0;
    watchTipLeave(() => left++, { doc, win, inWindow: () => null });
    fire('doc', 'mouseout', { relatedTarget: {} });     // to another element of the page
    assert.equal(left, 0);
    fire('doc', 'mouseout', { relatedTarget: null });   // out of the page
    assert.equal(left, 1);
    fire('win', 'blur');
    assert.equal(left, 2);
    fire('doc', 'visibilitychange');                     // still visible
    assert.equal(left, 2);
    doc.visibilityState = 'hidden';
    fire('doc', 'visibilitychange');
    assert.equal(left, 3);
});

test('confirm: shows unless out; an answer that comes after cancel() shows nothing', async () => {
    const { doc, win } = fakePage();
    let where = true;
    const g = watchTipLeave(() => {}, { doc, win, inWindow: async () => where });
    let shows = 0;
    g.confirm(() => shows++);
    await settle();
    assert.equal(shows, 1);
    where = false;
    g.confirm(() => shows++);
    await settle();
    assert.equal(shows, 1);
    where = true;
    g.confirm(() => shows++);
    g.cancel();
    await settle();
    assert.equal(shows, 1);
    // No backend: at once, as before.
    const n = watchTipLeave(() => {}, { doc, win, inWindow: () => null });
    n.confirm(() => shows++);
    assert.equal(shows, 2);
});

test('up: asked every POLL_MS until down(); out of the window, leave()', async (t) => {
    t.mock.timers.enable({ apis: ['setInterval'] });
    const { doc, win } = fakePage();
    let where = true, asked = 0, left = 0;
    const g = watchTipLeave(() => left++, { doc, win, inWindow: async () => { asked++; return where; } });
    g.up();
    g.up();                                   // once is enough
    t.mock.timers.tick(POLL_MS);
    await settle();
    assert.equal(asked, 1);
    for (let i = 0; i < 3; i++) { t.mock.timers.tick(POLL_MS); await settle(); }
    assert.equal(asked, 4);
    assert.equal(left, 0);
    where = false;
    t.mock.timers.tick(POLL_MS);
    await settle();
    assert.equal(left, 1);
    g.down();
    t.mock.timers.tick(POLL_MS * 4);
    await settle();
    assert.equal(asked, 5);
});

test('one question at a time: ticks while the answer is on its way ask nothing', async (t) => {
    t.mock.timers.enable({ apis: ['setInterval'] });
    const { doc, win } = fakePage();
    let asked = 0, answer = null;
    const g = watchTipLeave(() => {}, { doc, win, inWindow: () => { asked++; return new Promise(r => { answer = r; }); } });
    g.up();
    t.mock.timers.tick(POLL_MS * 3);
    assert.equal(asked, 1);
    answer(true);
    await settle();
    t.mock.timers.tick(POLL_MS);
    assert.equal(asked, 2);
    g.down();
});

test('up with nothing to ask: it stops asking', (t) => {
    t.mock.timers.enable({ apis: ['setInterval'] });
    const { doc, win } = fakePage();
    let asked = 0;
    const g = watchTipLeave(() => {}, { doc, win, inWindow: () => { asked++; return null; } });
    g.up();
    t.mock.timers.tick(POLL_MS * 5);
    assert.equal(asked, 1);
});
