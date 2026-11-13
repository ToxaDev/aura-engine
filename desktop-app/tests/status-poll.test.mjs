// The player's status poll (status-poll.js): one request at a time, and an
// older answer never taken over a newer one — the rack's lamps went dark and
// lit again mid-track when two status requests crossed (Anton 30.10).

import test from 'node:test';
import assert from 'node:assert/strict';
import { latestOnly, STATUS_STALE_MS } from '../src/js/status-poll.js';
import { stageStates } from '../src/js/arming.js';

/// Requests the test answers by hand, in any order.
function fakeAsk() {
    const out = [];
    const ask = () => new Promise(res => out.push(res));
    return { ask, out };
}
const flush = () => new Promise(r => setImmediate(r));

// Two snapshots of a track whose HP comes on the air: before, HP is on in the
// rack and not heard yet; after, it is heard.
const STAGES = [{ id: 'convSubsonic', tok: 'SUB' }, { id: 'convHybridPhase', tok: 'HP' }];
const isOn = () => true;
const before = { n: 1, state: 'playing', audible: { source: 'live', stages: [['SUB15', 1, '']] } };
const after = { n: 2, state: 'playing', audible: { source: 'live', stages: [['SUB15', 1, ''], ['HP', 1, '']] } };
const hpState = s => stageStates(s.audible, STAGES, isOn).find(x => x.id === 'convHybridPhase').state;

test('the bare poll this replaces: two requests crossing put HP back to switching in', async () => {
    // What setInterval(poll, 250) did with an async poll and no guard.
    const { ask, out } = fakeAsk();
    const seen = [];
    const naive = async () => { seen.push(hpState(await ask())); };
    const a = naive(), b = naive();
    out[1](after); await flush();      // the newer answer lands first…
    out[0](before); await flush();     // …the older one last
    await Promise.all([a, b]);
    assert.deepEqual(seen, ['on', 'pending'], 'lit, then dark again: the flicker');
});

test('a tick while a request is out asks nothing', async () => {
    const { ask, out } = fakeAsk();
    const taken = [];
    let t = 0;
    const tick = latestOnly(ask, s => taken.push(s.n), { now: () => t });
    const first = tick();
    t = 250; assert.equal(await tick(), false);
    t = 500; assert.equal(await tick(), false);
    assert.equal(out.length, 1, 'one request out');
    out[0](before);
    assert.equal(await first, true);
    const second = tick();
    assert.equal(out.length, 2);
    out[1](after); await second;
    assert.deepEqual(taken, [1, 2]);
    assert.deepEqual([before, after].map(hpState), ['pending', 'on']);
});

test('nothing answered (null, an error) is not taken and lets the next tick ask', async () => {
    let t = 0, n = 0;
    const taken = [];
    const answers = [null, new Error('down'), before];
    const tick = latestOnly(async () => {
        const a = answers[n++];
        if (a instanceof Error) throw a;
        return a;
    }, s => taken.push(s.n), { now: () => t });
    assert.equal(await tick(), true);
    assert.equal(await tick(), true);
    assert.equal(await tick(), true);
    assert.deepEqual(taken, [1]);
});

test('a lost answer holds the poll back only so long; when it comes late it is dropped', async () => {
    const { ask, out } = fakeAsk();
    const taken = [];
    let t = 0;
    const tick = latestOnly(ask, s => taken.push(s.n), { now: () => t });
    const lost = tick();
    t = STATUS_STALE_MS - 1; assert.equal(await tick(), false);
    t = STATUS_STALE_MS; const fresh = tick();
    assert.equal(out.length, 2, 'asked again after the stale time');
    out[1](after); await fresh;
    out[0](before); await lost;        // the lost one turns up after all
    assert.deepEqual(taken, [2], 'the older answer is not taken over the newer');
    // The poll goes on after it.
    const next = tick();
    assert.equal(out.length, 3);
    out[2](after); await next;
    assert.deepEqual(taken, [2, 2]);
});

test('a stale request answered before the newer one is still taken, in order', async () => {
    const { ask, out } = fakeAsk();
    const taken = [];
    let t = 0;
    const tick = latestOnly(ask, s => taken.push(s.n), { now: () => t });
    const slow = tick();
    t = STATUS_STALE_MS; const fresh = tick();
    out[0](before); await slow;
    out[1](after); await fresh;
    assert.deepEqual(taken, [1, 2]);
});
