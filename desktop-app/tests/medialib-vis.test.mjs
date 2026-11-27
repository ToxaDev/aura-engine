// The library's Visualizer page: its reckoning apart from the page
// (js/medialib/vis-model.js) — the first scene, the tiles and their marks,
// the sliders, values shown and typed, the pack's line, the look-ahead.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { parseHeader, valuesOf } from '../src/js/vis/scene.js';
import {
    nameHue, playerScene, firstScene, tilesOf, shownValue, valueFrom, sliderRows, anyChanged,
    packText, aheadWant, freshName, named, AHEAD_S, FIRST,
} from '../src/js/medialib/vis-model.js';

const scene = f => readFileSync(new URL('../src/vis/' + f, import.meta.url), 'utf8');
const entry = (file, text) => ({ id: 'my:' + file, file, text, mine: true, header: parseHeader(text) });
const bars = entry('bars.aura-vis', scene('bars.aura-vis'));
const tunnel = entry('tunnel.aura-vis', scene('tunnel.aura-vis'));
const inst = entry('diamond-dust-instruments.aura-vis', scene('diamond-dust-instruments.aura-vis'));
const catalog = [bars, inst, tunnel];

test('a name has one hue, the same every time, in 0…359', () => {
    assert.equal(nameHue('Bars'), nameHue('Bars'));
    for (const n of ['', 'Bars', 'Tunnel', 'Ünïcode ✓']) {
        const h = nameHue(n);
        assert.ok(Number.isInteger(h) && h >= 0 && h < 360, n);
    }
    assert.notEqual(nameHue('Bars'), nameHue('Tunnel'));
});

test("the player's scene: a chosen scene always, under Album the last one where no cover, none when Off", () => {
    assert.deepEqual(playerScene('vis:my:bars.aura-vis', 'vis:my:tunnel.aura-vis'), { id: 'my:bars.aura-vis', always: true });
    assert.deepEqual(playerScene('album', 'vis:my:tunnel.aura-vis'), { id: 'my:tunnel.aura-vis', always: false });
    assert.deepEqual(playerScene('album', null), { id: null, always: false });
    assert.deepEqual(playerScene('off', 'vis:my:tunnel.aura-vis'), { id: null, always: false });
});

test('the first scene: the page\'s last, else the player\'s, else Tunnel, else the first', () => {
    assert.equal(firstScene(catalog, { kept: bars.id, player: tunnel.id }), bars.id);
    assert.equal(firstScene(catalog, { kept: 'my:gone.aura-vis', player: inst.id }), inst.id);
    assert.equal(firstScene(catalog, {}), FIRST);
    assert.equal(firstScene([bars, inst], {}), bars.id);
    assert.equal(firstScene([], { kept: bars.id }), null);
});

test('tiles keep the list\'s order and carry the marks', () => {
    const t = tilesOf(catalog, { current: tunnel.id, player: { id: bars.id, always: true }, failed: new Set([inst.id]) });
    assert.deepEqual(t.map(x => x.id), catalog.map(c => c.id));
    assert.equal(t[0].mark, 'IN PLAYER');
    assert.equal(t[2].current, true);
    assert.equal(t[0].current, false);
    assert.equal(t[1].failed, true);
    assert.equal(t[1].spatial, true);
    assert.equal(t[0].spatial, false);
    assert.equal(t[0].name, bars.header.name);
    const u = tilesOf(catalog, { player: { id: tunnel.id, always: false } });
    assert.equal(u[2].mark, 'IN PLAYER · NO COVER');
    assert.equal(u[0].mark, '');
});

test('values shown: numbers by their step with the unit, bool, choice, colour', () => {
    const h = bars.header;
    const p = n => h.params.find(x => x.name === n);
    assert.equal(shownValue(p('count'), 48), '48');
    assert.equal(shownValue(p('gap'), 0.3), '0.30');
    assert.equal(shownValue(p('curve'), 1.4), '1.40 x');
    assert.equal(shownValue(p('mirror'), 1), 'On');
    assert.equal(shownValue(p('mirror'), 0), 'Off');
    assert.equal(shownValue(p('colours'), 2), 'Custom');
    assert.equal(shownValue(p('low'), [0, 1, 0]), '#00ff00');
});

test('values typed: kept in range, of their kind, the default for nonsense', () => {
    const h = bars.header;
    const p = n => h.params.find(x => x.name === n);
    assert.equal(valueFrom(p('count'), '500'), 128);
    assert.equal(valueFrom(p('count'), '20.6'), 21);
    assert.equal(valueFrom(p('gap'), '-1'), 0);
    assert.equal(valueFrom(p('gap'), 'abc'), p('gap').def);
    assert.equal(valueFrom(p('gap'), ''), p('gap').def);
    assert.equal(valueFrom(p('mirror'), true), 1);
    assert.equal(valueFrom(p('mirror'), false), 0);
    assert.equal(valueFrom(p('colours'), '7'), 2);
    assert.deepEqual(valueFrom(p('low'), '#ff0000'), [1, 0, 0]);
    assert.deepEqual(valueFrom(p('low'), 'red'), p('low').def);
});

test('sliders come in the scene\'s sections, each marked when off its default', () => {
    const h = bars.header;
    const v = valuesOf(h, {});
    const s = sliderRows(h, v);
    assert.deepEqual(s.map(x => x.name), h.sections.map(x => x.name).filter((_, i) => h.sections[i].params.length));
    assert.equal(s.reduce((n, x) => n + x.rows.length, 0), h.params.length);
    assert.ok(s.every(x => x.rows.every(r => !r.changed)));
    assert.equal(anyChanged(h, v), false);
    const w = { ...v, count: 64 };
    const r = sliderRows(h, w).flatMap(x => x.rows).find(x => x.p.name === 'count');
    assert.equal(r.changed, true);
    assert.equal(r.text, '64');
    assert.equal(anyChanged(h, w), true);
    assert.equal(anyChanged(h, { ...v, low: v.low.slice() }), false);
});

test('a scene without settings has no rows; an empty header none either', () => {
    const h = parseHeader('// @name Plain\nvec4 scene(vec2 uv, vec2 p) { return vec4(0.0); }\n');
    assert.deepEqual(sliderRows(h, {}), []);
    assert.deepEqual(sliderRows(null, null), []);
    assert.equal(anyChanged(null, null), false);
});

test("the pack's line: only under a scene with instruments", () => {
    assert.equal(packText({ installed: false }, false), null);
    assert.equal(packText(null, true), null);
    assert.equal(packText({ installed: true }, true).get, false);
    const no = packText({ installed: false, state: 'idle' }, true);
    assert.equal(no.get, true);
    assert.match(no.text, /not installed/);
    const dl = packText({ installed: false, busy: true, state: 'downloading', got: 50, total: 200 }, true);
    assert.match(dl.text, /Downloading the instruments pack… 25 %/);
    assert.match(packText({ installed: true, busy: true, background: true, state: 'verifying', got: 0, total: 0 }, true).text, /^Updating/);
});

test('the look-ahead: only while a scene is drawn on screen and something plays or is paused', () => {
    assert.equal(aheadWant({ drawing: true, visible: true, state: 'playing' }), AHEAD_S);
    assert.equal(aheadWant({ drawing: true, visible: true, state: 'paused' }), AHEAD_S);
    assert.equal(aheadWant({ drawing: true, visible: true, state: 'stopped' }), 0);
    assert.equal(aheadWant({ drawing: true, visible: false, state: 'playing' }), 0);
    assert.equal(aheadWant({ drawing: false, visible: true, state: 'playing' }), 0);
});

test('a new scene\'s name is free; @name is set or added', () => {
    assert.equal(freshName([]), 'My scene');
    assert.equal(freshName(['my scene', 'My scene 2']), 'My scene 3');
    assert.equal(named('// @name  Old\nx', 'New'), '// @name  New\nx');
    assert.equal(named('x', 'New'), '// @name     New\nx');
    assert.equal(parseHeader(named(scene('bars.aura-vis'), 'Mine')).name, 'Mine');
});
