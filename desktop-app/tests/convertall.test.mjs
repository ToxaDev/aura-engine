// The Convert all button's faces (convertall.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { convertAllView } from '../src/js/convertall.js';

test('a batch\'s result takes no click, but is not a disabled button: its tip shows', () => {
    const v = convertAllView({ converting: false, result: { text: '✓ 0:20 · ×33.1', title: 'figures' }, left: 0 });
    assert.equal(v.disabled, false, 'a disabled button gets no pointer, so no tip');
    assert.equal(v.ariaDisabled, true);
    assert.equal(v.done, true);
    assert.equal(v.busy, false);
    assert.equal(v.text, '✓ 0:20 · ×33.1');
    assert.equal(v.title, 'figures');
});

test('a running batch is held to cancel; idle, the button is off only with nothing left', () => {
    const busy = convertAllView({ converting: true, pct: 41.6, left: 3 });
    assert.deepEqual([busy.disabled, busy.ariaDisabled, busy.busy, busy.done, busy.text, busy.fill], [false, false, true, false, '42%', 41.6]);
    assert.equal(busy.title, 'Hold to cancel the batch');
    const some = convertAllView({ converting: false, left: 1 });
    assert.deepEqual([some.disabled, some.ariaDisabled, some.text], [false, false, 'Convert all · 1']);
    assert.equal(some.title, 'Convert 1 file not yet converted with the current rack');
    const none = convertAllView({ converting: false, left: 0 });
    assert.deepEqual([none.disabled, none.ariaDisabled, none.text], [true, false, 'Convert all']);
});
