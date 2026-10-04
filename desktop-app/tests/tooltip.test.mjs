// A plain data-tip as the tip's HTML (tooltip.js plainTip).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { plainTip } from '../src/js/tooltip.js';

test('a plain tip keeps its lines and lets no markup through', () => {
    assert.equal(plainTip('one\ntwo\r\nthree'), 'one<br>two<br>three');
    assert.equal(plainTip('<b>"x" & y</b>\n<img src=z onerror=1>'), '&lt;b&gt;&quot;x&quot; &amp; y&lt;/b&gt;<br>&lt;img src=z onerror=1&gt;');
    assert.equal(plainTip('no breaks'), 'no breaks');
});
