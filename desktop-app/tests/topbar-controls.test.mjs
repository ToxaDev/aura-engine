// The player's device row: the device/format housing, BIT-PERFECT and the
// analyzer button share one box (one height, outline and corners), and the
// badge line sits the strip's own row gap under them.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const read = (f) => readFileSync(new URL(`../src/css/${f}`, import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const player = read('player.css');
const analyzer = read('analyzer-entry.css');
const listening = read('listening.css');

const block = (css, sel) => {
    const i = css.indexOf(`\n${sel} {\n`);
    assert.ok(i >= 0, `no "${sel}" rule`);
    return css.slice(i, css.indexOf('\n}', i + 1));
};
const prop = (b, p) => b.match(new RegExp(`\\n\\s*${p}: ([^;]+);`))?.[1];

test('one height for the device row\'s three buttons, from one variable', () => {
    assert.equal(prop(block(player, '.pl-bar'), '--pl-ctl-h'), '22px');
    const shared = block(player, '.pl-dev .pl-devfmt,\n.pl-dev .pl-bp,\n.pl-dev .an-open-btn');
    assert.equal(prop(shared, 'height'), 'var(--pl-ctl-h)');
    assert.equal(prop(shared, 'box-sizing'), 'border-box');
    assert.equal(prop(shared, 'border-width'), '1px');
    assert.equal(prop(shared, 'border-style'), 'solid');
    assert.equal(prop(shared, 'border-radius'), '5px');
});

test('no button keeps a height, outline or corners of its own', () => {
    for (const [css, sel] of [[player, '.pl-devfmt'], [player, '.pl-bp'], [analyzer, '.an-open-btn']]) {
        const b = block(css, sel);
        for (const p of ['height', 'border', 'border-radius', 'box-sizing']) assert.equal(prop(b, p), undefined, `${sel} ${p}`);
        assert.ok(prop(b, 'border-color'), `${sel} keeps its hue`);
    }
    // No other height for them anywhere in the sheets.
    for (const css of [player, analyzer, listening]) {
        assert.doesNotMatch(css, /\.(pl-devfmt|pl-bp|an-open-btn)(?![\w-])(?:[:.][\w-]+)*\s*\{[^}]*[^-]height: (?!var\(--pl-ctl-h\))/);
    }
});

test('BIT-PERFECT centres its letters in the shared height', () => {
    const bp = block(player, '.pl-bp');
    assert.match(bp, /display: inline-flex; align-items: center;/);
    assert.equal(prop(bp, 'padding'), '0 7px');
});

test('the badge line sits the strip\'s row gap under the buttons; the big player keeps its own', () => {
    assert.equal(prop(block(player, '.pl-bar'), 'gap'), '6px');
    assert.equal(prop(block(player, '.pl-status'), 'margin-top'), '0');
    assert.match(listening, /body\.mode-listening #plBar \.pl-status \{[^}]*margin-top: 5px;/);
});

test('BIT-PERFECT\'s letters are trimmed to their capitals, so they sit on its centre', () => {
    const js = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(js, /id="plBitPerfect" class="pl-bp"[^>]*><span class="pl-bp-t">BIT-PERFECT<\/span><\/button>/);
    const t = block(player, '.pl-bp-t');
    assert.equal(prop(t, 'text-box'), 'trim-both cap alphabetic');
    assert.equal(prop(t, 'line-height'), '1');
});
