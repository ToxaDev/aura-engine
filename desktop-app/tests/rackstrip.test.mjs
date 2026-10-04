// The rack's strip (rackstrip.js): five cells that say the rack as it is,
// and panes whose choices write the same settings the controls always did.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
    STRIP_TEXTS, APOD_CHOICES, HR_CHOICES, CELLS, stripCells, shortTaps, pickChoice, stripOwns,
} from '../src/js/rackstrip.js';
import { studioRowsFor } from '../src/js/list-geom.js';
import { saveSettings } from '../src/js/settings.js';
import { state } from '../src/js/state.js';

const AS_SHIPPED = { fs: 8, taps: 30000000, custom: null, gpu: true, gpuNone: null, aa: true, apod: '0', hr: '-0.5', missing: false };
const cells = r => Object.fromEntries(stripCells({ ...AS_SHIPPED, ...r }).map(c => [c.id, c]));
const shown = c => (c.unit ? `${c.value} ${c.unit}` : c.value);

test('five cells in the strip\'s order, each named', () => {
    assert.deepEqual(CELLS, ['fs', 'taps', 'gpu', 'apod', 'hr']);
    assert.deepEqual(stripCells(AS_SHIPPED).map(c => c.id), CELLS);
    assert.deepEqual(stripCells(AS_SHIPPED).map(c => c.label), ['FS Multiplier', 'Resolution', 'GPU', 'Apodizing', 'Headroom']);
});

test('as shipped: FS8, 30M taps, GPU on, Apodizing by AA, −0.5 dB', () => {
    const c = cells({});
    assert.equal(shown(c.fs), 'FS8');
    assert.equal(c.fs.tip, 'FS Multiplier: the output rate, 352.8 / 384 kHz. Click to change.');
    assert.equal(shown(c.taps), '30M taps');
    assert.equal(c.taps.tip, 'Filter resolution: 30M taps. Click to change it or load a custom filter.');
    assert.equal(shown(c.gpu), 'On');
    assert.equal(c.gpu.cls, 'lit');
    assert.equal(c.gpu.tip, STRIP_TEXTS.gpuOn);
    assert.equal(shown(c.apod), 'AA');
    assert.equal(c.apod.tip, STRIP_TEXTS.apodAa);
    assert.equal(shown(c.hr), '−0.5 dB');
    assert.equal(c.hr.tip, 'Headroom: −0.5 dB. Click to change.');
});

test('the rates of each FS, the lengths of the ladder', () => {
    assert.match(cells({ fs: 2 }).fs.tip, /88\.2 \/ 96 kHz/);
    assert.match(cells({ fs: 4 }).fs.tip, /176\.4 \/ 192 kHz/);
    assert.match(cells({ fs: 16 }).fs.tip, /705\.6 \/ 768 kHz/);
    assert.deepEqual([5000, 1000000, 5000000, 10000000, 30000000].map(shortTaps), ['5k', '1M', '5M', '10M', '30M']);
    assert.equal(shortTaps(4200000), '4.2M');
});

test('a custom filter: Resolution says Custom and its length', () => {
    const c = cells({ custom: { taps: 4200000 } });
    assert.equal(shown(c.taps), 'Custom 4.2M');
    assert.equal(c.taps.cls, 'custom');
    assert.equal(c.taps.tip, 'Filter resolution: Custom 4.2M. Click to change it or load a custom filter.');
});

test('GPU: off dim, on lit, no usable GPU says CPU and why', () => {
    assert.deepEqual([shown(cells({ gpu: false }).gpu), cells({ gpu: false }).gpu.cls], ['Off', 'dim']);
    assert.equal(cells({ gpu: false }).gpu.tip, STRIP_TEXTS.gpuOff);
    const none = cells({ gpuNone: 'no Vulkan device' }).gpu;
    assert.deepEqual([shown(none), none.cls], ['CPU', 'bad']);
    assert.equal(none.tip, 'No usable GPU: no Vulkan device. Conversions run on the CPU.');
    assert.equal(shown(cells({ gpu: false, gpuNone: 'x' }).gpu), 'Off', 'switched off: the probe no longer matters');
});

test('Apodizing without AA: its own choice, with the frequency in the tip', () => {
    assert.equal(shown(cells({ aa: false, apod: '1' }).apod), 'Gentle');
    assert.equal(cells({ aa: false, apod: '1' }).apod.tip, 'Apodizing: Gentle (20 kHz). Click to change.');
    assert.equal(shown(cells({ aa: false, apod: '2' }).apod), 'Moderate');
    assert.equal(shown(cells({ aa: false, apod: '3' }).apod), 'Strong');
    assert.deepEqual([shown(cells({ aa: false, apod: '0' }).apod), cells({ aa: false, apod: '0' }).apod.cls], ['Off', 'dim']);
});

test('Headroom: Off is the shipped ceiling, the others in dB', () => {
    const off = cells({ hr: '0' }).hr;
    assert.deepEqual([shown(off), off.cls], ['Off', 'dim']);
    assert.equal(off.tip, 'Headroom: Off (−0.5 dBTP, as shipped). Click to change.');
    assert.equal(shown(cells({ hr: '-1.0' }).hr), '−1.0 dB');
    assert.equal(shown(cells({ hr: '-3.0' }).hr), '−3.0 dB');
});

test('no filter on disk for the FS and length: both cells warn', () => {
    const c = cells({ missing: true });
    assert.equal(c.fs.cls, 'warn');
    assert.equal(c.taps.cls, 'warn');
    assert.equal(cells({}).fs.cls, '');
});

test('every choice of the two selects has its name in the lists', () => {
    const html = readFileSync(new URL('../src/components/converter.html', import.meta.url), 'utf8');
    for (const [id, names] of [['convApodizing', APOD_CHOICES], ['convHeadroom', HR_CHOICES]]) {
        const sel = html.match(new RegExp(`<select id="${id}"[\\s\\S]*?</select>`));
        assert.ok(sel, `converter.html has #${id}`);
        const values = [...sel[0].matchAll(/<option value="([^"]*)"/g)].map(m => m[1]);
        assert.ok(values.length >= 4, id);
        for (const v of values) assert.ok(names[v], `${id}: ${v} has no name`);
    }
});

/// A select, as much of one as the pane's list and settings.js touch.
function select(value, disabled = false) {
    const events = [];
    return { value, disabled, events, dispatchEvent(e) { events.push([e.type, e.bubbles]); return true; } };
}

test('a pick in a pane writes the select as a pick in the select did, once, with its event', () => {
    const hr = select('-0.5');
    assert.equal(pickChoice(hr, '-3.0'), true);
    assert.equal(hr.value, '-3.0');
    assert.deepEqual(hr.events, [['change', true]]);
    assert.equal(pickChoice(hr, '-3.0'), false, 'the same choice again: no event');
    assert.equal(hr.events.length, 1);
    const apod = select('0', true);
    assert.equal(pickChoice(apod, '2'), false, 'locked while AA is on');
    assert.equal(apod.value, '0');
    assert.deepEqual(apod.events, []);
});

test('the settings saved after a pick are the ones the old selects saved', () => {
    const els = {
        convFsSlider: { value: '2' }, convTapSlider: { value: '4' },
        convApodizing: select('1'), convHeadroom: select('-0.5'),
        convGpuCheck: { checked: true }, convAdaptiveApodizer: { checked: false },
    };
    let kept = null;
    globalThis.document = { getElementById: id => els[id] ?? null };
    globalThis.localStorage = { setItem: (k, v) => { kept = JSON.parse(v); }, getItem: () => null };
    state.convCustomFilterPath = null;
    pickChoice(els.convHeadroom, '-1.0');
    pickChoice(els.convApodizing, '3');
    saveSettings();
    assert.equal(kept.convHeadroom, '-1.0');
    assert.equal(kept.convApodizing, '3');
    assert.equal(kept.convFs, '2');
    assert.equal(kept.convTapCount, 30000000);
    assert.equal(kept.convGpuCheck, true);
});

test('the studio\'s rows on a low screen: as many fewer as it takes, never under six', () => {
    assert.equal(studioRowsFor(1076, 1340, 10, 47), 10, 'room enough');
    assert.equal(studioRowsFor(1076, 1076, 10, 47), 10);
    assert.equal(studioRowsFor(1076, 1030, 10, 47), 9);
    assert.equal(studioRowsFor(1076, 980, 10, 47), 7);
    assert.equal(studioRowsFor(1076, 700, 10, 47), 6, 'not under six');
    assert.equal(studioRowsFor(NaN, 700, 10, 47), 10);
});

test('with a pane open, the pane and the cells that open one are its own; GPU and the rest only put it away', () => {
    // Nodes as the strip has them: contains, closest('.rk-cell'), the cell's data-cell.
    const node = (parent, cell) => {
        const n = { parent, dataset: cell ? { cell } : {} };
        n.contains = x => { for (let y = x; y; y = y.parent) if (y === n) return true; return false; };
        n.closest = () => { for (let y = n; y; y = y.parent) if (y.dataset.cell) return y; return null; };
        return n;
    };
    const body = node(null), strip = node(body), pop = node(body);
    const cells = Object.fromEntries(CELLS.map(id => [id, node(strip, id)]));
    const label = node(cells.taps);          // the name inside a cell
    const slider = node(pop);
    const play = node(body);
    const own = t => stripOwns(strip, pop, t);
    assert.equal(own(slider), true, 'inside the pane');
    for (const id of ['fs', 'taps', 'apod', 'hr']) assert.equal(own(cells[id]), true, id);
    assert.equal(own(label), true, 'a cell\'s own parts');
    assert.equal(own(cells.gpu), false, 'GPU is a switch, not an opener');
    assert.equal(own(strip), false, 'the gap between the cells');
    assert.equal(own(play), false);
});

test('the strip\'s words: English', () => {
    for (const v of Object.values(STRIP_TEXTS)) {
        const s = typeof v === 'function' ? v('x') : v;
        assert.ok(!/[Ѐ-ӿ]/.test(s), s);
    }
});
