// The Auto / Auto Shift grid picture: the wave and the output points (dots)
// the same in both, only the source samples (rings) move: on every 4th dot
// for Auto, half a dot earlier for Auto Shift. No text inside the picture.
import test from 'node:test';
import assert from 'node:assert/strict';

import { gridTipSvg, gridTipHtml, gridGraphSvg, gridOutX, gridSrcX, GRID_CAPTION, GRID_EXPLAIN } from '../src/js/grid-tip.js';

const rings = (svg) => [...svg.matchAll(/<circle cx="([\d.]+)" cy="[\d.]+" r="5(?:\.5)?" fill="none"/g)].map((m) => +m[1]);
const dots = (svg) => [...svg.matchAll(/<circle cx="([\d.]+)" cy="([\d.]+)" r="2\.2"/g)].map((m) => `${m[1]},${m[2]}`);
const wave = (svg) => svg.match(/<path d="([^"]+)"/)[1];

for (const [name, draw] of [['tooltip', gridTipSvg], ['read-out', gridGraphSvg]]) {
    test(`${name}: the same wave and dots in both modes, only the rings move`, () => {
        const a = draw('auto');
        const s = draw('shift');
        assert.equal(wave(a), wave(s));
        assert.deepEqual(dots(a), dots(s));
        const ax = dots(a).map((d) => +d.split(',')[0]);
        // Auto: every ring on a dot, four output steps apart.
        for (const x of rings(a)) assert.ok(ax.includes(x), `auto: ring at ${x} on a dot`);
        // Auto Shift: every ring half a step from the nearest dot.
        for (const x of rings(s)) assert.equal(Math.min(...ax.map((d) => Math.abs(d - x))), 10, `shift: ${x} between dots`);
        assert.equal(rings(a)[1] - rings(a)[0], 80);
        assert.equal(rings(a)[0] - rings(s)[0], 10);
        assert.equal(/<text/.test(a) || /<text/.test(s), false);
    });
}

test('the grids, the sizes, the captions', () => {
    assert.deepEqual(gridOutX(60), [0, 20, 40, 60]);
    assert.deepEqual(gridSrcX('auto', 200), [20, 100, 180]);
    assert.deepEqual(gridSrcX('shift', 200), [10, 90, 170]);
    assert.match(gridTipSvg('auto'), /^<svg class="grid-tip-pic" width="160" height="60" viewBox="0 0 160 60"/);
    assert.match(gridGraphSvg('shift'), /^<svg class="tap-graph-pic" width="200" height="46" viewBox="0 0 200 46"/);
    // The read-out's glow filters do not clash between the two modes.
    assert.ok(gridGraphSvg('auto').includes('id="tapGraphGlow-auto"'));
    assert.ok(gridGraphSvg('shift').includes('id="tapGraphGlow-shift"'));
    for (const mode of ['auto', 'shift']) {
        assert.ok(gridTipHtml(mode).includes(`<div class="grid-tip-cap">${GRID_CAPTION[mode]}</div>`));
        assert.match(GRID_EXPLAIN[mode], /amber rings/);
    }
    assert.equal(GRID_CAPTION.auto, 'Source samples on the output points');
    assert.equal(GRID_CAPTION.shift, 'Source samples between the output points');
});
