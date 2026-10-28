// A list the page draws itself (pick-list.js): which row the keys reach,
// where the list drops; and the tooltips lifted above every list
// (tip-place.js raiseTip).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { dropPlace, stepIndex } from '../src/js/pick-list.js';
import { raiseTip } from '../src/js/tip-place.js';

const ITEMS = [
    { value: 'pcm', text: 'PCM' },
    { sep: true },
    { value: '256:a', text: 'A', disabled: true },
    { value: '256:b', text: 'B' },
    { sep: true },
    { value: '128:a', text: 'C' },
    { value: '128:b', text: 'D', disabled: true },
];

test('the keys step over separators and rows that cannot be chosen, and stop at the ends', () => {
    assert.equal(stepIndex(ITEMS, 0, 1), 3, 'past the rule and the grey row');
    assert.equal(stepIndex(ITEMS, 3, 1), 5);
    assert.equal(stepIndex(ITEMS, 5, 1), 5, 'the last that can be chosen: it stays');
    assert.equal(stepIndex(ITEMS, 5, -1), 3);
    assert.equal(stepIndex(ITEMS, 3, -1), 0);
    assert.equal(stepIndex(ITEMS, 0, -1), 0);
    assert.equal(stepIndex(ITEMS, -1, 1), 0, 'Home: the first');
    assert.equal(stepIndex(ITEMS, ITEMS.length, -1), 5, 'End: the last that can be chosen');
    assert.equal(stepIndex([{ sep: true }, { value: 'x', disabled: true }], -1, 1), -1, 'none at all');
});

test('the list drops under the button, over it when only there it fits', () => {
    const r = (top, h = 22) => ({ top, bottom: top + h, left: 10, right: 80 });
    assert.deepEqual(dropPlace(r(40), 300, 800), { top: 65, up: false });
    // Near the bottom: over the button, whole in the window.
    assert.deepEqual(dropPlace(r(700), 300, 800), { top: 397, up: true });
    // Fits neither way: the side with more room, kept in the window.
    assert.deepEqual(dropPlace(r(100), 500, 400), { top: 125, up: false });
    assert.deepEqual(dropPlace(r(300), 500, 400), { top: 6, up: true });
});

// A tooltip element as raiseTip sees it, in a page whose top layer may hold
// something else (`busy`).
function fakeTip(busy = []) {
    const log = [];
    const attrs = {};
    const classes = new Set();
    let open = false;
    const doc = {
        querySelectorAll: (q) => {
            if (q === 'select:open') throw new Error('not supported');
            return q === ':popover-open' ? [...(open ? [tip] : []), ...busy] : [];
        },
    };
    const tip = {
        ownerDocument: doc,
        classList: { add: (c) => classes.add(c) },
        hasAttribute: (a) => a in attrs,
        setAttribute: (a, v) => { attrs[a] = v; },
        matches: (q) => q === ':popover-open' && open,
        showPopover: () => { open = true; log.push('show'); },
        hidePopover: () => { open = false; log.push('hide'); },
    };
    return { tip, log, attrs, classes };
}

test('a tooltip goes into the top layer once, and is lifted again only over something else there', () => {
    const t = fakeTip();
    raiseTip(t.tip);
    assert.equal(t.attrs.popover, 'manual');
    assert.ok(t.classes.has('tip-layer'), 'select.css takes the popover\'s own box away');
    assert.deepEqual(t.log, ['show']);
    raiseTip(t.tip);
    assert.deepEqual(t.log, ['show'], 'alone up there: left as it is (its fade-in kept)');

    const list = {};
    const u = fakeTip([list]);
    raiseTip(u.tip);
    raiseTip(u.tip);
    assert.deepEqual(u.log, ['show', 'hide', 'show'], 'a list opened after it: shown anew, the topmost');

    // No popover API (an old WebView, node): nothing happens, nothing breaks.
    assert.doesNotThrow(() => raiseTip({}));
    assert.doesNotThrow(() => raiseTip(null));
});

test('every window\'s tooltip is placed through placeTip, so every one is lifted', () => {
    const src = (f) => readFileSync(new URL(`../src/js/${f}`, import.meta.url), 'utf8');
    assert.match(src('tip-place.js'), /export function placeTip\(tip, anchor, opts\) \{\s*raiseTip\(tip\);/);
    for (const f of ['tooltip.js', 'analytics/window.js', 'vis/studio.js']) assert.match(src(f), /placeTip\(/, f);
    const css = readFileSync(new URL('../src/css/select.css', import.meta.url), 'utf8');
    assert.match(css, /:where\(\.tip-layer\[popover\]\) \{[^}]*border: 0;[^}]*background: transparent;/);
    // select.css is on every page that has a tooltip.
    for (const page of ['index.html', 'analytics.html', 'vis-studio.html']) {
        assert.match(readFileSync(new URL(`../src/${page}`, import.meta.url), 'utf8'), /href="css\/select\.css"/, page);
    }
});
