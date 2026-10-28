// The analyzer's track-frame poll: after the views are cleared (a new track)
// the whole frame comes again, even with a poll in flight.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The reply to a poll made before refetchTrack() was a stub of the old rev;
// the window took its rev, and asked "anything newer than that" from then
// on: the table stood empty until the analysis moved on.

import test from 'node:test';
import assert from 'node:assert/strict';

import { createScheduler } from '../src/js/analytics/scheduler.js';

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

test('a stub asked for before refetchTrack() does not take the rev back', async () => {
    const asks = [];          // { url, reply(bytes) }
    globalThis.fetch = (url) => new Promise((resolve) => {
        asks.push({ url, reply: (n) => resolve({ ok: true, arrayBuffer: async () => new ArrayBuffer(n) }) });
    });
    const trackAsks = () => asks.filter(a => a.url.includes('/track?'));
    let sched;
    const seen = [];
    sched = createScheduler(1, {
        onLive() {},
        onResp() {},
        onError() {},
        // As window.js: a stub or a frame carries the rev (7 here) it tells the scheduler.
        onTrack(buf) { seen.push(buf.byteLength); sched.notifyTrackRev(7); },
    });
    sched.start();
    await sleep(20);
    trackAsks()[0].reply(100);                 // the first whole frame (rev 7)
    await sleep(560);                          // the next poll: "newer than 7?"
    assert.match(trackAsks()[1].url, /rev=7/);
    sched.refetchTrack();                      // a new track: the views are empty
    trackAsks()[1].reply(8);                   // ...and the old poll's stub comes back
    await sleep(30);
    const next = trackAsks()[2];
    assert.ok(next, 'asked again at once');
    assert.match(next.url, /rev=0/, 'for the whole frame, not for newer than 7');
    assert.deepEqual(seen, [100], 'the stale stub was not handed on');
    sched.stop();
});

// The spectrum view's zoom reaches the scheduler: the Whole curves are asked
// for over the range shown (below 20 Hz too), once the wheel has rested.
test('an:spec:rangeChange reaches notifyZoomChange, debounced', async () => {
    const { wireSpectrumRange } = await import('../src/js/analytics/scheduler.js');
    const bus = new EventTarget();
    const calls = [];
    const off = wireSpectrumRange(bus, { notifyZoomChange: (f0, f1, n) => calls.push([f0, f1, n]) }, 30);
    const send = (f0, f1, n) => bus.dispatchEvent(new CustomEvent('an:spec:rangeChange', { detail: { f0, f1, n } }));
    send(5, 88200, 512);
    send(3, 60000, 512);
    send(1.5, 40000, 512);
    await sleep(10);
    assert.deepEqual(calls, [], 'not while the wheel turns');
    await sleep(50);
    assert.deepEqual(calls, [[1.5, 40000, 512]], 'the last range, once');
    send(0, 100, 512);                         // no range: ignored
    await sleep(50);
    assert.equal(calls.length, 1);
    off();
    send(5, 22050, 512);
    await sleep(50);
    assert.equal(calls.length, 1, 'unwired');
});

test('the resp request follows the zoom: f0 below 20 Hz, f1 above 22.05 kHz', async () => {
    const urls = [];
    globalThis.fetch = (url) => { urls.push(url); return Promise.resolve({ ok: true, arrayBuffer: async () => new ArrayBuffer(0) }); };
    const sched = createScheduler(1, { onLive() {}, onTrack() {}, onResp() {}, onError() {} });
    sched.start();
    sched.notifyZoomChange(5, 88200, 600);
    await sleep(120);
    const resp = urls.filter(u => u.includes('/resp?'));
    assert.ok(resp.length > 0, 'asked');
    assert.match(resp[resp.length - 1], /f0=5&f1=88200&n=600/);
    sched.stop();
});
