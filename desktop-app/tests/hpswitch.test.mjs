// Hybrid-Phase's unit at a track switch: out as the old sound stops, back with
// the new track's picture once it is heard (hpswitch.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { createSwitchFade, IN_MS, GAP_WAIT_MS } from '../src/js/hpswitch.js';

const st = (trackId, state = 'playing', { pending = false, pos = 30, dur = 200 } = {}) =>
    ({ trackId, state, jump: { pending }, positionS: pos, durationS: dur });

/// A model already showing track 1 at t = 0.
function playing() {
    const m = createSwitchFade();
    m.status(st(1), 0);
    m.fresh(10, 20);
    m.tick(20 + IN_MS);
    assert.equal(m.phase, 'on');
    return m;
}

test('next with a ready pair: out at the jump, back with the first picture asked for once heard', () => {
    const m = playing();
    m.status(st(2, 'playing', { pending: true }), 1000);
    assert.equal(m.out, true);
    // Answers meanwhile — the old frame held, or the new one not yet on the air.
    m.fresh(1100, 1150);
    assert.equal(m.out, true);
    m.status(st(2), 1250);
    m.fresh(1200, 1300);          // asked before it was heard
    assert.equal(m.out, true);
    m.fresh(1260, 1320);
    assert.equal(m.phase, 'in');
    m.tick(1320 + IN_MS);
    assert.equal(m.phase, 'on');
});

test('the jump landed between two statuses: heard at the first one of the new track', () => {
    const m = playing();
    m.status(st(2), 1000);
    assert.equal(m.out, true);
    m.fresh(1000, 1040);
    assert.equal(m.phase, 'in');
});

test('a seek within the track never fades', () => {
    const m = playing();
    m.status(st(1, 'playing', { pending: true, pos: 120 }), 1000);
    m.status(st(1, 'playing', { pos: 120 }), 1250);
    assert.equal(m.phase, 'on');
});

test('the gapless hand-over stays shown when the new picture comes in time', () => {
    const m = playing();
    m.status(st(1, 'playing', { pos: 199.2 }), 1000);
    m.status(st(2, 'playing', { pos: 0.1 }), 1250);
    assert.equal(m.phase, 'wait');
    assert.equal(m.out, false);
    m.fresh(1300, 1340);
    assert.equal(m.phase, 'on');
});

test('the gapless hand-over goes out rather than stand frozen, and back with the picture', () => {
    const m = playing();
    m.status(st(1, 'playing', { pos: 199.2 }), 1000);
    m.status(st(2, 'playing', { pos: 0.1 }), 1250);
    m.status(st(2, 'playing', { pos: 0.4 }), 1250 + GAP_WAIT_MS);
    assert.equal(m.out, true);
    m.fresh(1250 + GAP_WAIT_MS + 10, 1250 + GAP_WAIT_MS + 60);
    assert.equal(m.phase, 'in');
});

test('next pressed mid-track while playing is not taken for gapless', () => {
    const m = playing();
    m.status(st(2, 'playing', { pos: 0 }), 1000);
    assert.equal(m.out, true);
});

test('another track clicked while paused: out, back once it plays and its picture comes', () => {
    const m = playing();
    m.status(st(1, 'paused'), 1000);
    m.status(st(2, 'paused', { pos: 0 }), 2000);
    assert.equal(m.out, true);
    m.fresh(2100, 2150);
    assert.equal(m.out, true);
    m.status(st(2, 'preparing', { pos: 0 }), 2250);
    m.status(st(2, 'playing', { pos: 0 }), 4000);
    m.fresh(4010, 4060);
    assert.equal(m.phase, 'in');
});

test('started from stopped: the first picture fades in', () => {
    const m = createSwitchFade();
    m.status(st(null, 'stopped'), 0);
    assert.equal(m.phase, 'on');
    m.status(st(3, 'preparing', { pos: 0 }), 100);
    assert.equal(m.out, true);
    m.status(st(3, 'playing', { pos: 0 }), 1500);
    m.fresh(1550, 1600);
    assert.equal(m.phase, 'in');
});
