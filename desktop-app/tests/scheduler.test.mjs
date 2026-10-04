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
