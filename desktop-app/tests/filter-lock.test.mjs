// The custom filter button while Hybrid-Phase is on: held, never disabled,
// its tooltip says why, and the tip it had comes back when HP goes off —
// also when the player holds the button at the same time (player.js keeps
// the tip to put back on Stop in data-tip-idle).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { setFilterBtnHpLock, filterBtnHpLocked, HP_FILTER_TIP } from '../src/js/filter-lock.js';

function button(attrs = {}) {
    const a = { ...attrs };
    return {
        disabled: false, style: {}, dataset: {},
        getAttribute: k => (k in a ? a[k] : null),
        removeAttribute: k => { delete a[k]; },
    };
}

test('HP on: the button is held, not disabled, and says why', () => {
    const b = button({ title: 'Load .npy filter' });
    setFilterBtnHpLock(b, true);
    assert.equal(b.disabled, false);
    assert.equal(b.style.cursor, 'not-allowed');
    assert.equal(b.dataset.tip, HP_FILTER_TIP);
    assert.equal(b.getAttribute('title'), null);
    assert.ok(filterBtnHpLocked(b));
});

test('HP off: the button comes back with its own tip', () => {
    const b = button({ title: 'Load .npy filter' });
    setFilterBtnHpLock(b, true);
    setFilterBtnHpLock(b, true);
    setFilterBtnHpLock(b, false);
    assert.equal(b.dataset.tip, 'Load .npy filter');
    assert.equal(b.style.opacity, '1');
    assert.equal(b.style.cursor, '');
    assert.ok(!filterBtnHpLocked(b));
});

test('a disabled button (an old lock) is let go', () => {
    const b = button();
    b.disabled = true;
    setFilterBtnHpLock(b, false);
    assert.equal(b.disabled, false);
    assert.equal(b.dataset.tip, undefined);
});

test('while the player holds it, the tip put back on Stop is swapped', () => {
    const b = button();
    b.dataset.tip = 'Load .npy filter';
    // ▶: the player shows its own tip and keeps the button's.
    b.dataset.tipIdle = b.dataset.tip;
    b.dataset.tip = 'player: stop to change it';
    setFilterBtnHpLock(b, true);
    assert.equal(b.dataset.tip, 'player: stop to change it');
    assert.equal(b.dataset.tipIdle, HP_FILTER_TIP);
    // ■: the player puts its kept tip back — the HP one, while HP is on.
    b.dataset.tip = b.dataset.tipIdle;
    delete b.dataset.tipIdle;
    assert.equal(b.dataset.tip, HP_FILTER_TIP);
    setFilterBtnHpLock(b, false);
    assert.equal(b.dataset.tip, 'Load .npy filter');
});
