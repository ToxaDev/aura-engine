// The level beside the player's volume slider: one decimal always, a true
// minus, right-aligned in a box as wide as the longest level, so the last
// digits stay put as the level moves.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// player.js builds the whole bar and cannot be imported here, so the shipped
// formatter is read out of its source and evaluated.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const js = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
const css = readFileSync(new URL('../src/css/player.css', import.meta.url), 'utf8');

const src = js.match(/^const fmtVolText = (db => .*);$/m);
const fmtVolText = (0, eval)(src[1]);

test('the volume text has one decimal and a true minus', () => {
    assert.equal(fmtVolText(0), '0.0 dB');
    assert.equal(fmtVolText(-0), '0.0 dB');
    assert.equal(fmtVolText(-5.5), '−5.5 dB');
    assert.equal(fmtVolText(-23), '−23.0 dB');
    assert.equal(fmtVolText(-60), '−60.0 dB');
});

test('every place that writes the volume text uses it, and the box is right-aligned', () => {
    const writes = [...js.matchAll(/\$\('plVolText'\)\.textContent = ([^;]+);/g)].map(m => m[1]);
    assert.ok(writes.length >= 3);
    for (const w of writes) assert.match(w, /fmtVolText\(/);
    assert.match(js, /id="plVolText">0\.0 dB</);
    const box = css.slice(css.indexOf('.pl-vol-text {'), css.indexOf('}', css.indexOf('.pl-vol-text {')));
    assert.match(box, /width: 38px;/);
    assert.match(box, /text-align: right;/);
    assert.match(box, /margin-left: -2px;/);
});
