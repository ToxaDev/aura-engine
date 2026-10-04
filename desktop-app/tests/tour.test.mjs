// The newcomer's tour: its steps and texts, when it offers itself, the walk
// through the steps, and where the tip goes (tour-core.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
    TOUR_KEY, TOUR_KEY_ANALYZER, TOUR_TEXTS, MAIN_STEPS, ANALYZER_STEPS, STEP_MODES, stepText, textParts,
    shouldOffer, tourRecord,
    nextIndex, stepCount,
    rect, padRect, unionRect, clipRect, lerpRect, easeOutCubic,
    placeTip, flyTransform, flyDuration,
} from '../src/js/tour-core.js';

// ── the steps and their texts ─────────────────────────────────────────

/// Every string a tour can put on the screen.
function allTexts() {
    const out = [];
    const walk = v => {
        if (typeof v === 'string') out.push(v);
        else if (typeof v === 'function') out.push(v(3, 12));
        else if (Array.isArray(v)) v.forEach(walk);
        else if (v && typeof v === 'object') Object.values(v).forEach(walk);
    };
    walk(TOUR_TEXTS);
    return out;
}

test('the test tool marks the profile under these keys: they do not move', () => {
    // rw (Session._tourOff) writes both on every run without --tour, so
    // neither the main window nor an analyzer window offers its tour.
    assert.equal(TOUR_KEY, 'auraTour');
    assert.equal(TOUR_KEY_ANALYZER, 'auraTourAnalyzer');
});

test('every step of the main tour has something to point at in tour.js, and nothing else does', () => {
    // A step whose target was renamed away would be passed over without a word.
    const src = fs.readFileSync(path.join(SRC, 'js', 'tour.js'), 'utf8');
    const block = src.slice(src.indexOf('const TARGETS = {'), src.indexOf('};', src.indexOf('const TARGETS = {')));
    const keys = [...block.matchAll(/^\s{4}([A-Za-z]+): \(\) =>/gm)].map(m => m[1]);
    assert.deepEqual([...keys].sort(), MAIN_STEPS.map(s => s.id).sort());
});

test('every step has a title and a text, ids are unique, modes are known', () => {
    const ids = MAIN_STEPS.map(s => s.id);
    assert.equal(new Set(ids).size, ids.length, 'ids unique');
    for (const s of MAIN_STEPS) {
        const t = TOUR_TEXTS.main[s.id];
        assert.ok(t, `texts for ${s.id}`);
        assert.ok(t.title && t.title.length <= 24, `${s.id}: a short title`);
        assert.ok(stepText(t, false).length > 0 && stepText(t, true).length > 0, `${s.id}: a text either way`);
        assert.ok(STEP_MODES.includes(s.mode), `${s.id}: mode ${s.mode}`);
        assert.ok(Array.isArray(s.side) && s.side.length > 0, `${s.id}: sides`);
    }
});

test('texts are English, short, one or two sentences', () => {
    for (const s of allTexts()) {
        assert.ok(!/[Ѐ-ӿ]/.test(s), `no Cyrillic: ${s}`);
        assert.match(s, /^[\x20-\x7E—…]*$/, `plain text: ${s}`);
        assert.ok(s.length <= 180, `short: ${s.length} chars — ${s}`);
        // A sentence ends before a capital or at the end.
        const sentences = s.split(/[.!?](?=\s+[A-Z]|$)/).filter(x => x.trim().length > 0);
        assert.ok(sentences.length <= 2, `one or two sentences: ${s}`);
    }
});

test('no sign stands alone in a text: a button is named in words, or as a key', () => {
    // Anton 1.10, the analyzer step: "Inside, its own ? shows what each tab
    // is for" read like a slip of the pen. A sign is drawn as a key only in
    // braces ("the {?} button"); out of them, every word has a letter or a
    // digit (a dash between words aside). The step count ("3 / 12") is no
    // text.
    for (const s of allTexts().filter(x => x !== TOUR_TEXTS.nav.count(3, 12))) {
        const words = textParts(s).filter(p => p.text != null).map(p => p.text).join(' ').split(/\s+/).filter(Boolean);
        const lone = words.filter(w => w !== '—' && !/[A-Za-z0-9]/.test(w));
        assert.deepEqual(lone, [], `a sign alone in: ${s}`);
        assert.ok(!/[{}]/.test(textParts(s).map(p => p.text || '').join('')), `braces left over: ${s}`);
    }
    // The keys the texts draw are the ones on the screen.
    const keys = allTexts().flatMap(s => textParts(s).filter(p => p.key != null).map(p => p.key));
    assert.ok(keys.length >= 1, 'the ? is named as a key');
    assert.deepEqual([...new Set(keys)], ['?']);
});

test('a text is cut into its words and the keys it names', () => {
    assert.deepEqual(textParts('the {?} button brings it back'),
        [{ text: 'the ' }, { key: '?' }, { text: ' button brings it back' }]);
    assert.deepEqual(textParts('{?}'), [{ key: '?' }]);
    assert.deepEqual(textParts('no keys here'), [{ text: 'no keys here' }]);
    assert.deepEqual(textParts('{ not a key }'), [{ text: '{ not a key }' }], 'braces round words are words');
    assert.deepEqual(textParts(null), []);
});

test('zone by zone, the player last, then the big player, and its edge pressed by the listener', () => {
    // Anton 1.10: no going back and forth between the layouts, nor the eye
    // between the zones of the window: the main window from the top down —
    // the settings, the rack, the list's head, the list — then, one short
    // step up, the player last, from its lowest row; it grows to the whole
    // window, and the listener folds it back.
    const modes = MAIN_STEPS.map(s => s.mode);
    assert.equal(modes[0], 'studio', 'the tour opens where a first start opens');
    const firstListening = modes.indexOf('listening');
    assert.ok(firstListening > 0, 'the big player comes after the main window');
    assert.ok(modes.slice(0, firstListening).every(m => m === 'studio'), 'all of the main window first');
    assert.ok(modes.slice(firstListening).every(m => m === 'listening'), 'then only the big player');
    const ids = MAIN_STEPS.map(s => s.id);
    const ZONES = [
        ['settings', 'fs', 'taps', 'gpu', 'apodizing', 'headroom'],        // the top
        ['rack'],
        ['view', 'radio', 'playlist', 'convertAll'],                         // the list's head: F/R first
        ['drop', 'row'],                                                     // the list
        ['player', 'badges', 'output', 'analyzer', 'transport', 'menu', 'instant'],
        ['listening', 'picture', 'studio', 'fold'],                          // the big player
    ];
    assert.deepEqual(ids, ZONES.flat(), 'each zone once, in this order');
    for (const id of ZONES[0].slice(1)) assert.ok(TOUR_TEXTS.main[id].title.length > 0, `${id}: named`);
    // The player is the last of the main window, and says so.
    assert.equal(ids[firstListening - 1], 'instant');
    assert.match(TOUR_TEXTS.main.player.text, /^Last, the player/);
    // The edge that folds it is the last step, and the only one the
    // listener presses.
    const last = MAIN_STEPS.at(-1);
    assert.equal(last.id, 'fold');
    assert.equal(last.mode, 'listening');
    assert.equal(last.live, true);
    assert.deepEqual(MAIN_STEPS.filter(s => s.live).map(s => s.id), ['fold']);
    // A menu's row is shown with its menu just before it, in the same layout.
    assert.equal(ids.indexOf('instant'), ids.indexOf('menu') + 1);
});

test('the analyzer\'s own tour offers itself the first time, as the main one does', () => {
    const o = TOUR_TEXTS.analyzer.offer;
    for (const k of ['title', 'body', 'start', 'skip']) assert.ok(o && o[k], `analyzer.offer.${k}`);
    assert.match(TOUR_TEXTS.main.analyzer.text, /first time it opens, it offers a short tour/);
});

test('the list step reads differently with nothing on the list', () => {
    const t = TOUR_TEXTS.main.drop;
    assert.notEqual(stepText(t, false), stepText(t, true));
    assert.equal(stepText({ text: 'x' }, true), 'x');
    assert.equal(stepText({ empty: 'e' }, true), 'e', 'a missing half falls back to the other');
    assert.equal(stepText(null, true), '');
});

test('a row lists its four buttons', () => {
    const keys = TOUR_TEXTS.main.row.keys.map(k => k[0]);
    assert.deepEqual(keys, ['play', 'convert', 'memory', 'remove']);
});

test('the analyzer: a text for every step, a tab for every graph, its ? at the end', () => {
    const ids = ANALYZER_STEPS.map(s => s.id);
    assert.equal(new Set(ids).size, ids.length, 'ids unique');
    for (const s of ANALYZER_STEPS) {
        const t = TOUR_TEXTS.analyzer[s.id];
        assert.ok(t && t.title && t.title.length <= 24 && t.text, `texts for ${s.id}`);
    }
    assert.deepEqual(ANALYZER_STEPS.filter(s => s.tab).map(s => s.tab), [
        'anSpectrumSection', 'anLoudnessSection', 'anSpectrogramSection',
        'anWaveformSection', 'anHistogramSection', 'anStereoSection',
    ]);
    assert.equal(ANALYZER_STEPS.at(-1).id, 'end');
    assert.notEqual(TOUR_KEY_ANALYZER, TOUR_KEY);
});

// ── what the tours point at is still there ────────────────────────────
// The tour points at elements other parts of the window own; a rename
// there would leave a step pointing at nothing (it is then passed over).
// Every id a tour looks up must still be made somewhere else in src.

const SRC = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'src');

function sources() {
    const out = [];
    const walk = d => {
        for (const e of fs.readdirSync(d, { withFileTypes: true })) {
            const p = path.join(d, e.name);
            if (e.isDirectory()) walk(p);
            else if (/\.(js|html)$/.test(e.name) && !/^tour/.test(e.name)) out.push(fs.readFileSync(p, 'utf8'));
        }
    };
    walk(SRC);
    return out.join('\n');
}

test('every name the tours import is still exported where they take it from', () => {
    const files = [path.join(SRC, 'js', 'tour.js'), path.join(SRC, 'js', 'analytics', 'tour-analyzer.js')];
    const missing = [];
    for (const f of files) {
        const src = fs.readFileSync(f, 'utf8');
        for (const m of src.matchAll(/import\s*\{([^}]+)\}\s*from\s*'([^']+)'/g)) {
            const from = fs.readFileSync(path.resolve(path.dirname(f), m[2]), 'utf8');
            for (const part of m[1].split(',')) {
                const name = part.trim().split(/\s+as\s+/)[0].trim();
                if (!name) continue;
                const re = new RegExp(`export\\s+(async\\s+)?(function|const|let|class)\\s+${name}\\b|export\\s*\\{[^}]*\\b${name}\\b`);
                if (!re.test(from)) missing.push(`${path.basename(f)}: ${name} from ${m[2]}`);
            }
        }
    }
    assert.deepEqual(missing, [], 'imports with nothing behind them');
});

test('every element the tours point at is still made by its owner', () => {
    const all = sources();
    const tourJs = fs.readFileSync(path.join(SRC, 'js', 'tour.js'), 'utf8')
        + fs.readFileSync(path.join(SRC, 'js', 'analytics', 'tour-analyzer.js'), 'utf8');
    const own = new Set(['tourHelp', 'anTourHelp']);
    const ids = [...new Set([...tourJs.matchAll(/(?:\$|block)\('([A-Za-z]+)'\)/g)].map(m => m[1]))].filter(id => !own.has(id));
    ids.push(...ANALYZER_STEPS.filter(s => s.tab).map(s => s.tab));
    assert.ok(ids.length >= 20, `ids found: ${ids.length}`);
    const missing = ids.filter(id => !new RegExp(`id\\s*=\\s*["']${id}["']|\\.id\\s*=\\s*['"]${id}['"]|id="${id}"`).test(all));
    assert.deepEqual(missing, [], 'ids no longer made anywhere');
    // Classes looked up by selector.
    for (const cls of ['pl-transport', 'pl-vol-wrap', 'pl-item', 'app-head-bar', 'an-tab', 'an-panel-collapsible',
        'prop-block', 'tap-slider-container']) {
        assert.ok(all.includes(cls), `class ${cls}`);
    }
    // The player's menu rows the copy of the menu is pointed at.
    for (const row of ['plVisMenuBtn', 'plMenuInstant']) assert.ok(all.includes(`'${row}'`), `menu row ${row}`);
});

// ── when it offers itself ─────────────────────────────────────────────

test('anyone the tour was not offered to gets it, an upgrader too; a met one nothing', () => {
    // A fresh install and an update from 1.3.4 hold the same: nothing under the key.
    assert.equal(shouldOffer({ stored: null }), 'offer');
    // An earlier build only lit the ? for an upgrader: that was no offer.
    assert.equal(shouldOffer({ stored: tourRecord('hinted') }), 'offer');
    for (const state of ['offered', 'done', 'skipped']) {
        assert.equal(shouldOffer({ stored: tourRecord(state) }), null, state);
    }
    assert.equal(shouldOffer({ stored: '{"state":"done","by":"rw"}' }), null, 'the test tool\'s mark');
    assert.equal(shouldOffer({ stored: 'anything' }), null);
});

test('the record says what happened and when', () => {
    assert.deepEqual(JSON.parse(tourRecord('done', '2026-10-01T00:00:00.000Z')),
        { v: 1, state: 'done', at: '2026-10-01T00:00:00.000Z' });
});

// ── the walk ──────────────────────────────────────────────────────────

test('Next and Back pass over the steps this window has nothing for', () => {
    const ok = k => ![2, 3].includes(k);
    assert.equal(nextIndex(6, 1, +1, ok), 4);
    assert.equal(nextIndex(6, 4, -1, ok), 1);
    assert.equal(nextIndex(6, 5, +1, ok), 6, 'past the end');
    assert.equal(nextIndex(6, 0, -1, ok), -1, 'before the start');
    assert.equal(nextIndex(6, -1, +1, () => false), 6, 'nothing to show');
});

test('the count is among the steps that will be shown', () => {
    const ok = k => k !== 1;
    assert.deepEqual(stepCount(5, 0, ok), { at: 1, total: 4 });
    assert.deepEqual(stepCount(5, 2, ok), { at: 2, total: 4 });
    assert.deepEqual(stepCount(5, 4, ok), { at: 4, total: 4 });
});

// ── rectangles ────────────────────────────────────────────────────────

test('rectangles: pad, union, clip, lerp', () => {
    assert.deepEqual(padRect(rect(10, 10, 20, 5), 3), rect(7, 7, 26, 11));
    assert.deepEqual(unionRect([rect(0, 0, 10, 10), rect(20, 5, 5, 20), null, rect(3, 3, 0, 0)]), rect(0, 0, 25, 25));
    assert.equal(unionRect([]), null);
    assert.deepEqual(clipRect(rect(-5, 390, 20, 30), { w: 440, h: 400 }), rect(0, 390, 15, 10));
    assert.deepEqual(lerpRect(rect(0, 0, 10, 10), rect(10, 20, 30, 40), 0.5), rect(5, 10, 20, 25));
    assert.equal(easeOutCubic(0), 0);
    assert.equal(easeOutCubic(1), 1);
    assert.ok(easeOutCubic(0.5) > 0.5, 'fast out, slow in');
});

// ── the tip ───────────────────────────────────────────────────────────

const VIEW = { w: 440, h: 900 };
const TIP = { w: 286, h: 120 };

test('under the target when there is room, the arrow on its middle', () => {
    const t = rect(100, 200, 80, 30);
    const p = placeTip(VIEW, t, TIP, ['bottom', 'top']);
    assert.equal(p.side, 'bottom');
    assert.equal(p.y, 200 + 30 + 12);
    assert.equal(p.x + p.ax, 140, 'the arrow points at the middle of the target');
    assert.equal(p.ay, 0, 'on the tip\'s top edge');
});

test('over the target when there is no room under it', () => {
    const t = rect(100, 820, 80, 40);
    const p = placeTip(VIEW, t, TIP, ['bottom', 'top']);
    assert.equal(p.side, 'top');
    assert.equal(p.y + TIP.h + 12, 820);
    assert.equal(p.ay, TIP.h, 'on the tip\'s bottom edge');
});

test('several things shown: the tip keeps off all of them, the arrow points at the first', () => {
    const row = rect(120, 150, 200, 22), menu = rect(110, 140, 220, 140);
    const p = placeTip(VIEW, unionRect([row, menu]), TIP, ['bottom', 'top'], { aim: row });
    assert.equal(p.side, 'bottom');
    assert.equal(p.y, 140 + 140 + 12, 'under the whole menu, not over its rows');
    assert.equal(p.x + p.ax, 220, 'the arrow under the row\'s middle');
    const q = placeTip(VIEW, unionRect([rect(20, 300, 18, 18), rect(400, 300, 18, 18)]), TIP, ['bottom'], { aim: rect(20, 300, 18, 18) });
    assert.equal(q.x, 8, 'the tip slides toward the first, held by the margin');
    assert.equal(q.x + q.ax, 29, 'the arrow right at the first');
});

test('the preferred side wins when both have room', () => {
    const t = rect(100, 400, 80, 30);
    assert.equal(placeTip(VIEW, t, TIP, ['top', 'bottom']).side, 'top');
    assert.equal(placeTip(VIEW, t, TIP, ['bottom', 'top']).side, 'bottom');
});

test('a target at the edge: the tip stays in the window, the arrow stays on the tip', () => {
    const t = rect(2, 100, 16, 16);   // the ? in the top left corner
    const p = placeTip(VIEW, t, TIP, ['bottom', 'right']);
    assert.equal(p.side, 'bottom');
    assert.equal(p.x, 8, 'held off the edge by the margin');
    assert.ok(p.ax >= 16 && p.ax <= TIP.w - 16, 'the arrow keeps off the corners');
    const r = placeTip(VIEW, rect(424, 100, 16, 16), TIP, ['bottom']);
    assert.equal(r.x + TIP.w, VIEW.w - 8);
});

test('a target as tall as the window: the tip lies over its lower part', () => {
    const t = rect(9, 60, 422, 820);
    const p = placeTip(VIEW, t, TIP, ['top', 'bottom']);
    assert.equal(p.side, 'inside');
    assert.equal(p.ax, null);
    assert.ok(p.y >= 8 && p.y + TIP.h <= VIEW.h - 8);
});

test('no target: the tip in the middle, a little above it', () => {
    const p = placeTip(VIEW, null, TIP);
    assert.equal(p.side, 'center');
    assert.equal(p.x, Math.round((VIEW.w - TIP.w) / 2));
    assert.ok(p.y < (VIEW.h - TIP.h) / 2);
});

test('wherever the target is, the tip is inside the window', () => {
    let seed = 7;
    const rnd = () => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed / 2147483648; };
    for (let k = 0; k < 2000; k++) {
        const view = { w: 300 + Math.round(rnd() * 900), h: 300 + Math.round(rnd() * 900) };
        const tip = { w: Math.min(286, view.w - 16), h: 60 + Math.round(rnd() * 120) };
        const t = rect(rnd() * view.w - 40, rnd() * view.h - 40, 4 + rnd() * view.w * 0.6, 4 + rnd() * view.h * 0.6);
        const sides = [['bottom', 'top'], ['top', 'bottom'], ['right', 'left'], ['left']][k % 4];
        const p = placeTip(view, t, tip, sides);
        assert.ok(p.x >= 8 && p.x + tip.w <= view.w - 8 + 0.5, `x in the window: ${JSON.stringify({ view, t, tip, p })}`);
        assert.ok(p.y >= 8 && p.y + tip.h <= view.h - 8 + 0.5, `y in the window: ${JSON.stringify({ view, t, tip, p })}`);
        if (p.ax != null) {
            assert.ok(p.ax >= 0 && p.ax <= tip.w && p.ay >= 0 && p.ay <= tip.h, 'the arrow on the tip\'s edge');
        }
    }
});

// ── into the ? ────────────────────────────────────────────────────────

test('the tip flies centre to centre, shrinking without squashing', () => {
    const from = rect(77, 400, 286, 120);
    const to = rect(9, 24, 15, 15);
    const f = flyTransform(from, to);
    assert.equal(f.dx, (9 + 7.5) - (77 + 143));
    assert.equal(f.dy, (24 + 7.5) - (400 + 60));
    assert.ok(Math.abs(f.s - 15 / 286) < 1e-9);
    assert.ok(f.dx < 0 && f.dy < 0, 'left and up');
    assert.equal(flyTransform(rect(0, 0, 0, 0), to).s, 1, 'no size: no scale');
});

test('the flight lasts 400 to 700 ms', () => {
    for (const from of [rect(10, 30, 286, 100), rect(77, 400, 286, 120), rect(1500, 1500, 286, 120)]) {
        const d = flyDuration(from, rect(9, 24, 15, 15));
        assert.ok(d >= 400 && d <= 700, `${d} ms`);
    }
});
