// Which stages are switching in or out: one answer for the status badges, the
// rack's breathing and the bar under the status line (arming.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    normTok, stageStates, switchesLive, anyWaiting, preparingBeforeSound,
    NOTHING_HEARD, airOf, tapsLabel, tapsChip, waitsOnAir,
    barStep, barView, BAR_FILL_MS, BAR_FADE_MS, BAR_IDLE_MS, BIT_PERFECT_LINE, bitPerfectLine, radioBar,
} from '../src/js/arming.js';

test("a stream's bar: filled by what it waits for, running while a filter or a new chain is made", () => {
    const url = 'https://icecast.radiofrance.fr/fip-hifi.aac';
    const info = (extra = {}) => ({ url, rate: 48000, ...extra });
    assert.equal(radioBar(null), null);
    assert.equal(radioBar({ phase: 'playing', info: info() }), null, 'playing as asked: no bar');
    // Tuning in, the plan known: the stream in hand of what it waits for.
    let b = radioBar({ phase: 'connecting', inS: 2, info: info({ needS: 8, tPlannedMs: 900 }) });
    assert.deepEqual([b.frac, b.run, b.busy], [0.25, false, true]);
    assert.equal(radioBar({ phase: 'waiting', inS: 12, info: info({ needS: 8, tPlannedMs: 900 }) }).frac, 1);
    // Its own filter being made (once): no share — the bar runs.
    b = radioBar({ phase: 'connecting', preparing: true, inS: 3, info: info() });
    assert.deepEqual([b.frac, b.run], [0, true]);
    // A rack change on the air: a new chain being built, the stream plays on.
    const sw = radioBar({ phase: 'playing', switching: 4, info: info({ needS: 8, tPlannedMs: 900 }) });
    assert.deepEqual([sw.frac, sw.run, sw.busy], [0, true, true]);
    assert.notEqual(sw.seq, radioBar({ phase: 'playing', switching: 5, info: info({ needS: 8, tPlannedMs: 900 }) }).seq,
        'another rack change: a bar of its own');
    assert.equal(radioBar({ phase: 'playing', preparing: true, info: info() }).run, true);
    assert.equal(radioBar({ phase: 'playing', switching: null, info: info() }), null, 'built: the bar goes');
});

test('BIT-PERFECT on the air says a quiet line while it plays, and gives way to a warning', () => {
    const direct = { source: 'direct' };
    assert.equal(BIT_PERFECT_LINE, 'Bit-perfect: samples reach the DAC unaltered');
    assert.equal(bitPerfectLine({ state: 'playing' }, direct, false), BIT_PERFECT_LINE);
    assert.equal(bitPerfectLine({ state: 'paused' }, direct, false), '', 'nothing plays: nothing said');
    assert.equal(bitPerfectLine({ state: 'stopped' }, null, false), '');
    assert.equal(bitPerfectLine({ state: 'playing' }, null, false), '', 'not on the air yet');
    assert.equal(bitPerfectLine({ state: 'playing' }, direct, true), '', 'a warning says more');
    assert.equal(bitPerfectLine({ state: 'playing' }, { source: 'live' }, false), '', 'through the rack: its badges');
    assert.equal(bitPerfectLine({ state: 'playing' }, { source: 'file' }, false), '');
    assert.equal(bitPerfectLine(null, direct, false), '');
});

/// The player's chain in rack order, as the page hands it in.
const CHAIN = [
    { id: 'convLabDeclip', tok: 'DC' },
    { id: 'convLabIsp', tok: 'ISP' },
    { id: 'convSubsonic', tok: 'SUB' },
    { id: 'convAdaptiveApod', tok: 'AA' },
    { id: 'convHybridPhase', tok: 'HP' },
    { id: 'convAlphaHp', tok: 'αHP' },
];
const rack = (...on) => id => on.includes(id);
const live = (...stages) => ({ source: 'live', stages });
const states = xs => xs.map(x => [x.id, x.state]);

test('heard and on is on, on and unheard is pending, heard and off is leaving', () => {
    const aud = live(['DC', 1, 'declipped 12 runs'], ['ISP', 2, ''], ['HP', 1, '']);
    const got = stageStates(aud, CHAIN, rack('convLabDeclip', 'convSubsonic', 'convAdaptiveApod'));
    assert.deepEqual(states(got), [
        ['convLabDeclip', 'on'],
        ['convLabIsp', 'leaving'],
        ['convSubsonic', 'pending'],
        ['convAdaptiveApod', 'pending'],
        ['convHybridPhase', 'leaving'],
    ]);
    assert.deepEqual([got[0].st, got[0].why], [1, 'declipped 12 runs']);
    assert.equal(got[2].st, null, 'a pending stage has no state of its own yet');
    assert.ok(anyWaiting(got));
});

test('a stage neither on nor heard is left out; all settled waits for nothing', () => {
    const got = stageStates(live(['DC', 1, ''], ['ISP', 2, '']), CHAIN, rack('convLabDeclip', 'convLabIsp'));
    assert.deepEqual(states(got), [['convLabDeclip', 'on'], ['convLabIsp', 'on']]);
    assert.equal(anyWaiting(got), false);
});

test('the chain says SUB15 and αHP where the rack says SUB and aHP', () => {
    assert.equal(normTok('SUB15'), 'SUB');
    assert.equal(normTok('SUB10'), 'SUB');
    assert.equal(normTok('αHP'), 'aHP');
    const got = stageStates(live(['SUB15', 1, ''], ['αHP', 1, '']), CHAIN, rack('convSubsonic', 'convAlphaHp'));
    assert.deepEqual(states(got), [['convSubsonic', 'on'], ['convAlphaHp', 'on']]);
});

// The rack's DC was on when the file was converted, but declip found nothing
// and the name left DC out: the rack's DC badge breathed for the whole track.
test('a converted file switches nothing in, whatever its name leaves out', () => {
    const disk = { source: 'file', stages: [['ISP', 1, ''], ['SUB15', 1, '']] };
    assert.equal(switchesLive(disk), false);
    assert.deepEqual(stageStates(disk, CHAIN, rack('convLabDeclip', 'convLabIsp', 'convSubsonic')), []);
});

test('BIT-PERFECT and nothing on the air switch nothing in either', () => {
    const on = rack('convLabDeclip', 'convLabIsp');
    assert.deepEqual(stageStates({ source: 'direct', stages: [] }, CHAIN, on), []);
    assert.deepEqual(stageStates(null, CHAIN, on), []);
    assert.deepEqual(stageStates(undefined, CHAIN, on), []);
    assert.equal(anyWaiting([]), false);
});

test('a live chain without a stage list waits for every stage on in the rack', () => {
    const got = stageStates({ source: 'live' }, CHAIN, rack('convLabIsp'));
    assert.deepEqual(states(got), [['convLabIsp', 'pending']]);
});

// ── the bar under the badges ────────────────────────────────────────────

/// Polls at `ms` with the given inputs, from `state`; the last view.
function run(state, polls) {
    let s = state;
    const views = [];
    for (const [now, waiting, arming] of polls) {
        s = barStep(s, { now, waiting, arming });
        views.push(barView(s));
    }
    return { s, views };
}
const busy = (seq, frac) => ({ seq, frac, busy: true, step: 'prep:isp' });
const idle = (seq, frac) => ({ seq, frac, busy: false, step: 'done' });

test('the bar shows while a stage waits and work is on its way, and never goes back', () => {
    const { views } = run(null, [
        [0, false, busy(1, 0.1)],
        [250, true, busy(1, 0.2)],
        [500, true, busy(1, 0.15)],
        [750, true, busy(1, 0.5)],
    ]);
    assert.equal(views[0].present, false, 'nothing waits: no bar');
    assert.deepEqual(views.slice(1).map(v => [v.on, v.frac]), [[true, 0.2], [true, 0.2], [true, 0.5]]);
    assert.equal(views[1].reset, true, 'a new arming starts where it is');
    assert.equal(views[2].reset, false);
});

test('the last stage on: the bar runs to full, then fades and is gone', () => {
    let { s } = run(null, [[0, true, busy(1, 0.6)]]);
    ({ s } = run(s, [[250, false, idle(1, 1)]]));
    assert.deepEqual([barView(s).on, barView(s).frac], [true, 1]);
    ({ s } = run(s, [[250 + BAR_FILL_MS - 1, false, idle(1, 1)]]));
    assert.equal(s.phase, 'finishing');
    ({ s } = run(s, [[250 + BAR_FILL_MS, false, idle(1, 1)]]));
    assert.deepEqual([s.phase, barView(s).on, barView(s).present], ['fading', false, true]);
    ({ s } = run(s, [[250 + BAR_FILL_MS + BAR_FADE_MS, false, null]]));
    assert.equal(barView(s).present, false);
});

test('a stage still waiting with nothing on its way: the bar goes as it stands', () => {
    let { s } = run(null, [[0, true, busy(1, 0.4)], [250, true, idle(1, 1)]]);
    assert.deepEqual([s.phase, barView(s).frac], ['shown', 0.4], 'not filled: the stage is not on');
    ({ s } = run(s, [[250 + BAR_IDLE_MS - 1, true, idle(1, 1)]]));
    assert.equal(s.phase, 'shown');
    ({ s } = run(s, [[250 + BAR_IDLE_MS, true, idle(1, 1)]]));
    assert.deepEqual([s.phase, barView(s).frac], ['fading', 0.4]);
    ({ s } = run(s, [[2000, true, busy(1, 0.5)]]));
    assert.deepEqual([s.phase, barView(s).frac], ['shown', 0.5], 'work on its way again: back');
});

test('a new arming starts over; BIT-PERFECT or a disk file takes the bar away', () => {
    let { s } = run(null, [[0, true, busy(1, 0.8)], [250, true, busy(2, 0.05)]]);
    assert.deepEqual([s.seq, barView(s).frac, barView(s).reset], [2, 0.05, true]);
    ({ s } = run(s, [[500, true, null]]));
    assert.equal(s.phase, 'fading');
    const { views } = run(null, [[0, true, idle(1, 1)], [250, true, null]]);
    assert.ok(views.every(v => !v.present), 'nothing on its way: no bar at all');
});

test('a track made ready before its first sound counts as waiting', () => {
    assert.equal(preparingBeforeSound({ state: 'preparing' }), true);
    assert.equal(preparingBeforeSound({ state: 'playing', jump: { pending: true }, pending: { what: 'Preparing x' } }), true);
    assert.equal(preparingBeforeSound({ state: 'playing', jump: { pending: true }, pending: null }), false, 'a seek alone is not');
    assert.equal(preparingBeforeSound({ state: 'playing', jump: { pending: false } }), false);
    assert.equal(preparingBeforeSound(null), false);
});

test('a rack change not yet sent shows the bar empty; clicked back, it goes without filling', () => {
    const queued = { seq: 'rack@0', frac: 0, busy: true, queued: true };
    let { s, views } = run(null, [[0, true, queued], [250, true, queued]]);
    assert.deepEqual(views.map(v => [v.on, v.frac]), [[true, 0], [true, 0]]);
    // It went out: the player's arming takes over from nothing.
    ({ s } = run(s, [[1600, true, busy(5, 0.1)]]));
    assert.deepEqual([s.seq, s.q, barView(s).frac, barView(s).reset], [5, false, 0.1, true]);
    // Clicked back before it went: no stage waits, nothing was done.
    ({ s } = run(null, [[0, true, queued], [250, false, queued]]));
    assert.deepEqual([s.phase, barView(s).frac], ['fading', 0]);
});

// ── made ready before the first sound: the badges already there (1.10) ──

test('made ready before its first sound, a track through the rack has nothing heard yet', () => {
    const preparing = { state: 'preparing', audible: null };
    assert.equal(airOf(preparing, true), NOTHING_HEARD);
    assert.equal(airOf(preparing, false), null, 'BIT-PERFECT or a converted file: nothing switches in');
    const heard = live(['DC', 1, '']);
    assert.equal(airOf({ state: 'playing', audible: heard }, true), heard, 'what is heard, once something is');
    assert.equal(airOf({ state: 'playing', audible: null, jump: { pending: false } }, true), null);
    assert.equal(airOf(null, true), null);
});

test('with nothing heard yet every stage on in the rack is switching in, the bar under them', () => {
    const got = stageStates(NOTHING_HEARD, CHAIN, rack('convLabDeclip', 'convLabIsp', 'convHybridPhase'));
    assert.deepEqual(states(got), [
        ['convLabDeclip', 'pending'],
        ['convLabIsp', 'pending'],
        ['convHybridPhase', 'pending'],
    ]);
    assert.ok(waitsOnAir(got, tapsChip(NOTHING_HEARD, 30e6)));
    // No stage on: the taps alone are on their way, and the bar shows for them.
    assert.ok(waitsOnAir(stageStates(NOTHING_HEARD, CHAIN, rack()), tapsChip(NOTHING_HEARD, 30e6)));
    // The first sound with the whole chain: all on, nothing waits, the bar ends.
    const whole = { ...live(['DC', 1, ''], ['ISP', 2, ''], ['HP', 1, '']), taps: '30M' };
    const after = stageStates(whole, CHAIN, rack('convLabDeclip', 'convLabIsp', 'convHybridPhase'));
    assert.deepEqual(after.map(x => x.state), ['on', 'on', 'on']);
    assert.equal(waitsOnAir(after, tapsChip(whole, 30e6)), false);
});

test('taps are labelled as the player labels them', () => {
    assert.deepEqual([5000, 1e6, 5e6, 1e7, 3e7].map(tapsLabel), ['5k', '1M', '5M', '10M', '30M']);
    assert.equal(tapsLabel(1000), null);
    assert.equal(tapsLabel(undefined), null);
});

// ── the taps chip switching (1.10) ─────────────────────────────────────

test('the taps chip switches until the rack\'s size is heard, and shows the size on its way', () => {
    const at = taps => ({ ...live(['DC', 1, '']), taps });
    assert.deepEqual(tapsChip(at('5k'), 5000), { label: '5k', switching: false });
    assert.deepEqual(tapsChip(at('5k'), 30e6), { label: '30M', switching: true }, 'new taps not heard yet');
    assert.deepEqual(tapsChip(at('30M'), 30e6), { label: '30M', switching: false }, 'heard: the plain chip');
    assert.deepEqual(tapsChip(NOTHING_HEARD, 1e7), { label: '10M', switching: true }, 'nothing heard yet');
    // Nothing on its way any more (the new chain's build failed): the size
    // heard, plain — not a switch that never ends (review m1).
    assert.deepEqual(tapsChip(at('5k'), 30e6, false), { label: '5k', switching: false });
});

test('a step down the player made is not a switch; another size asked for is', () => {
    const stepped = { ...live(['DC', 1, '']), taps: '10M', downgrade: { from: '30M', to: '10M', reason: 'power', gen: 1 } };
    assert.deepEqual(tapsChip(stepped, 30e6), { label: '10M', switching: false }, 'the power or GPU step down blinks as before');
    assert.deepEqual(tapsChip(stepped, 5e6), { label: '5M', switching: true }, 'the rack asks for 5M: switching');
    // A second step (K3 after the build's own) comes down from the size
    // heard; the size asked for is still the rack's (review M1).
    const twice = { ...live(['DC', 1, '']), taps: '5M', downgrade: { from: '10M', to: '5M', reason: 'power', gen: 2, asked: '30M' } };
    assert.deepEqual(tapsChip(twice, 30e6), { label: '5M', switching: false }, 'stepped down twice: the blink, not switching');
    assert.deepEqual(tapsChip(twice, 10e6), { label: '10M', switching: true }, 'the rack asks for 10M: switching');
});

test('no taps chip for what does not follow the rack, or a chain of no known size', () => {
    assert.equal(tapsChip({ source: 'direct', stages: [], taps: null }, 30e6), null);
    assert.equal(tapsChip({ source: 'file', stages: [], taps: '30M' }, 5000), null);
    assert.equal(tapsChip(null, 30e6), null);
    assert.equal(tapsChip({ ...live(), taps: null }, 30e6), null);
});
