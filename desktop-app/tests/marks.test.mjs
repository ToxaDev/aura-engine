// The marks under the Resolution and FS sliders sit under the thumb.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// A range input's thumb centre runs from half a thumb in from the track's left
// end to half a thumb in from its right end, evenly by value. Marks spread with
// `justify-content: space-between` put the edges of the labels at the ends
// instead, so labels of different widths drifted away from the thumb. Here the
// markup and the stylesheet are read as shipped and the positions computed.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { TAP_PRESETS, FS_PRESETS } from '../src/js/inventory.js';

const html = readFileSync(new URL('../src/components/converter.html', import.meta.url), 'utf8');
const css = readFileSync(new URL('../src/css/style.css', import.meta.url), 'utf8');

function slider(id) {
    const m = html.match(new RegExp(`<input type="range" id="${id}" min="(\\d+)" max="(\\d+)"`));
    assert.ok(m, id);
    return { min: +m[1], max: +m[2] };
}

function marks(id, attr) {
    const box = html.match(new RegExp(`<div class="tap-marks" id="${id}" style="--n: (\\d+)">([\\s\\S]*?)</div>`));
    assert.ok(box, id);
    const spans = [...box[2].matchAll(new RegExp(`<span ${attr}="(\\d+)"(?: data-tip-grid="\\w+")? style="--i: (\\d+)">([^<]*)</span>`, 'g'))];
    return { n: +box[1], spans: spans.map(s => ({ value: +s[1], i: +s[2], text: s[3] })) };
}

function rule(selector) {
    const at = css.indexOf(selector + ' {');
    assert.ok(at >= 0, selector);
    return css.slice(at, css.indexOf('}', at));
}

const px = (block, prop) => {
    const m = block.match(new RegExp(`(?:^|[\\s;{])${prop}:\\s*([^;]+);`));
    assert.ok(m, prop);
    return m[1].trim();
};

test('every mark is centred under the thumb at its own value', () => {
    const thumb = parseFloat(px(rule('.tap-slider-container input[type="range"]::-webkit-slider-thumb'), 'width'));
    assert.equal(px(rule('.tap-slider-container input[type="range"]'), 'margin'), '0');
    const [, side] = px(rule('.tap-marks'), 'margin').split(/\s+/);
    const inset = parseFloat(side);
    const span = rule('.tap-marks span');
    assert.equal(px(span, 'left'), 'calc(100% * var(--i) / (var(--n) - 1))');
    assert.equal(px(span, 'transform'), 'translateX(-50%)');

    for (const [sid, mid, attr, presets] of [
        ['convTapSlider', 'convTapMarks', 'data-taps', TAP_PRESETS],
        ['convFsSlider', 'convFsMarks', 'data-fs', FS_PRESETS],
    ]) {
        const s = slider(sid);
        const m = marks(mid, attr);
        assert.equal(s.min, 0, sid);
        assert.equal(s.max, presets.length - 1, `${sid}: one position per preset`);
        assert.equal(m.n, presets.length, mid);
        assert.deepEqual(m.spans.map(x => x.value), presets, `${mid}: the presets in slider order`);
        for (const width of [180, 260, 400]) {
            for (const { i } of m.spans) {
                // The thumb at value i (Chromium: the thumb box moves over track - thumb).
                const thumbCentre = thumb / 2 + (i - s.min) / (s.max - s.min) * (width - thumb);
                // The mark's centre: its left within the inset row, then shifted back half its width.
                const markCentre = inset + (width - 2 * inset) * i / (m.n - 1);
                assert.ok(Math.abs(markCentre - thumbCentre) < 1e-9, `${mid} mark ${i} at ${width}px`);
            }
        }
    }
    assert.equal(marks('convTapMarks', 'data-taps').spans.at(-2).text, 'Auto');
    assert.equal(marks('convTapMarks', 'data-taps').spans.at(-1).text, 'Auto S');
    // One filter on two grids: a bracket under both, from Auto's mark one step on.
    assert.ok(html.includes('<span class="tap-pair" style="--i: ' + TAP_PRESETS.indexOf(2000000000) + '" aria-hidden="true"></span>'));
    const pair = rule('.tap-marks .tap-pair');
    assert.equal(px(pair, 'transform'), 'none');
    assert.equal(px(pair, 'width'), 'calc(100% / (var(--n) - 1) + 30px)');
    assert.equal(parseFloat(px(pair, 'margin-left')), -15, 'centred: as far left of Auto as right of Auto S');
    assert.equal(px(pair, 'border-top'), 'none');
    // Their tooltip is the grid's picture (grid-tip.js).
    assert.ok(html.includes('<span data-taps="2000000000" data-tip-grid="auto"'));
    assert.ok(html.includes('<span data-taps="2000000001" data-tip-grid="shift"'));
});

test('no "optimal" mark; the input is painted over the marks so the thumb gets every click', () => {
    assert.equal(html.includes('tap-optimal'), false);
    assert.equal(css.includes('tap-optimal'), false);
    const lifted = css.match(/\.tap-slider-container input\[type="range"\] \{ position: relative; z-index: (\d+); \}/);
    assert.ok(lifted && +lifted[1] > 0, 'input above the line');
});

test('every stop is a socket and a groove joins them: one recess centred on the track', () => {
    // Same measured geometry as the "optimal" cell: the input's 6px track
    // starts 14px above the marks row, which is inset by half a thumb.
    const TRACK_TOP = 14;
    const thumb = parseFloat(px(rule('.tap-slider-container input[type="range"]::-webkit-slider-thumb'), 'width'));
    const track = parseFloat(px(rule('input[type="range"]'), 'height'));
    const [, side] = px(rule('.tap-marks'), 'margin').split(/\s+/);
    const inset = parseFloat(side);
    assert.equal(inset, thumb / 2);

    // The input's own track is cleared and drawn in the recess instead.
    assert.ok(css.includes('.tap-slider-container input[type="range"] { background: transparent; }'), 'input track cleared');

    // The SVG covers the row's width, its box 22px tall ending at the row's
    // top, so its middle (y = half its height) is the track's middle.
    const box = rule('.tap-marks .tap-groove');
    const h = parseFloat(px(box, 'height'));
    assert.equal(px(box, 'position'), 'absolute');
    assert.equal(parseFloat(px(box, 'left')), 0);
    assert.equal(px(box, 'width'), '100%');
    assert.equal(parseFloat(px(box, 'top')), -h);
    assert.equal(px(box, 'overflow'), 'visible');
    assert.equal(px(box, 'pointer-events'), 'none');
    const mid = h / 2;
    assert.equal(-h + mid, -(TRACK_TOP - track / 2), 'centred on the track');

    for (const [id, attr, fx] of [['convFsMarks', 'data-fs', 'convFsGrooveFx'], ['convTapMarks', 'data-taps', 'convTapGrooveFx']]) {
        const row = html.match(new RegExp(`<div class="tap-marks" id="${id}"[^>]*>\\s*(<svg class="tap-groove"[\\s\\S]*?</svg>)\\s*<span`));
        assert.ok(row, `${id}: the recess first in the row`);
        const svg = row[1];
        const { n, spans } = marks(id, attr);
        // One filtered group holds the bed (sockets + groove) and the line,
        // so the shade falls on both and the recess has no seam.
        assert.ok(svg.includes(`<filter id="${fx}"`) && svg.includes(`<g filter="url(#${fx})">`), `${id}: filter`);
        const bed = svg.match(/<g class="tap-groove-bed">([\s\S]*?)<\/g>/);
        assert.ok(bed, `${id}: bed`);
        // A socket on every mark (cx at i / (n - 1) of the row, like the
        // marks: under the thumb), 2px round the thumb, on the track's middle.
        const circles = [...bed[1].matchAll(/<circle cx="([\d.]+)%" cy="([\d.]+)" r="([\d.]+)"\/>/g)];
        assert.equal(circles.length, n, `${id}: a socket per stop`);
        assert.equal(spans.length, n);
        circles.forEach((c, i) => {
            assert.ok(Math.abs(+c[1] - 100 * i / (n - 1)) < 0.001, `${id}: socket ${i} under its mark`);
            assert.equal(+c[2], mid);
            assert.equal(+c[3] * 2, thumb + 4, '2px clear of the thumb all round');
        });
        // The groove spans the stops, centred on the track, inside the sockets.
        const g = bed[1].match(/<rect x="0" y="([\d.]+)" width="100%" height="([\d.]+)"\/>/);
        assert.ok(g, `${id}: groove`);
        assert.equal(+g[1] + +g[2] / 2, mid);
        assert.ok(+g[2] > track && +g[2] < thumb + 4);
        // The line: on the track's middle, 4px (thinner than the input's 6px
        // track, the thumb still centred on it), from the first socket's
        // centre to the last's (not out to the ends of the row's insets).
        const line = svg.match(/<g class="tap-groove-line"[^>]*>(<rect x="([\d.]+)" y="([\d.]+)" width="([\d.]+)%" height="([\d.]+)" rx="[\d.]+"\/>)<\/g>/);
        assert.ok(line, `${id}: line`);
        const [, , x, y, w, lh] = line.map(Number);
        assert.equal(x, 0); assert.equal(w, 100);
        assert.equal(lh, track - 2, 'a thinner line than the input track'); assert.equal(y + lh / 2, mid);
    }
    // A disabled slider dims the recess with it.
    assert.ok(css.includes('.tap-slider-container input[type="range"]:disabled + .tap-marks .tap-groove { opacity: 0.4; }'));
});

test('the labels sit 6px under the row top on both sliders; no thumb grows on hover', () => {
    const sp = rule('.tap-marks span');
    assert.equal(parseFloat(px(sp, 'top')), 6);
    assert.equal(px(rule('.tap-marks'), 'height'), 'calc(1.1em + 6px)');
    assert.ok(!/slider-thumb:hover[^{]*\{[^}]*scale/.test(css), 'no hover scale on a thumb');
    assert.ok(!/slider-thumb\s*\{[^}]*transform/.test(css));
});
