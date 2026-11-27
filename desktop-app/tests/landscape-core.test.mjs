// The landscape window's arithmetic and kept state (landscape-core.js):
// the size and place it opens at, the compact window's way back, the keys,
// and the list's whole rows in the Queue pane.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    parseSaved, stringifySaved, fitLandscape, compactPlace, logicalArea,
    isSwitchKey, isListeningKey, isTextTarget, rowsFor,
    MIN_W, MIN_H, FIRST_W, FIRST_H, COMPACT_W, KEY, INTRO_KEY, introDue, introRecord,
} from '../src/js/landscape-core.js';

const FHD = { x: 0, y: 0, w: 1920, h: 1040 };   // 1080p less the taskbar

test('nothing kept, or something unreadable, is the compact window', () => {
    for (const t of [null, '', 'nope', '[]', '42', '{"mode":"sideways"}']) {
        const s = parseSaved(t);
        assert.equal(s.mode, 'compact');
        assert.equal(s.size, null);
        assert.equal(s.max, false);
        assert.equal(s.tab, 'queue');
    }
});

test('the kept state reads back as it was written', () => {
    const s = { mode: 'landscape', size: { w: 1500, h: 950 }, pos: { x: 40, y: 30 }, max: true, tab: 'sound', compactPos: { x: 900, y: 10 } };
    assert.deepEqual(parseSaved(stringifySaved(s)), s);
});

test('a size too small to be a window or a tab of no name is dropped', () => {
    const s = parseSaved('{"mode":"landscape","size":{"w":10,"h":900},"tab":"signal","pos":{"x":"a","y":1}}');
    assert.equal(s.mode, 'landscape');
    assert.equal(s.size, null);
    assert.equal(s.pos, null);
    assert.equal(s.tab, 'queue');
});

test('the first time the window opens 1440 x 900, from where the compact one stands', () => {
    const r = fitLandscape(parseSaved(null), { x: 100, y: 50 }, FHD);
    assert.deepEqual(r, { w: FIRST_W, h: FIRST_H, x: 100, y: 50 });
});

test('a compact window near the right edge: the wide one moves in to stay whole', () => {
    const r = fitLandscape(parseSaved(null), { x: 1400, y: 300 }, FHD);
    assert.equal(r.x, 1920 - FIRST_W);
    assert.equal(r.y, 1040 - FIRST_H);
});

test('the kept place and size come back when they fit', () => {
    const saved = { size: { w: 1200, h: 800 }, pos: { x: 300, y: 120 } };
    assert.deepEqual(fitLandscape(saved, { x: 0, y: 0 }, FHD), { w: 1200, h: 800, x: 300, y: 120 });
});

test('a kept place on a screen that is gone: from the compact window instead', () => {
    const saved = { size: { w: 1200, h: 800 }, pos: { x: 2500, y: 120 } };
    assert.deepEqual(fitLandscape(saved, { x: 10, y: 20 }, FHD), { w: 1200, h: 800, x: 10, y: 20 });
});

test('the least size holds, and a small screen gets all of itself', () => {
    const small = { x: 0, y: 0, w: 1024, h: 728 };
    const r = fitLandscape({ size: { w: 600, h: 400 } }, { x: 50, y: 50 }, small);
    assert.deepEqual(r, { w: 1024, h: MIN_H, x: 0, y: 48 });
    const big = fitLandscape({ size: { w: 600, h: 400 } }, { x: 0, y: 0 }, FHD);
    assert.equal(big.w, MIN_W);
    assert.equal(big.h, MIN_H);
});

test('a second screen to the left (negative coordinates) is a screen too', () => {
    const left = { x: -1920, y: 0, w: 1920, h: 1040 };
    const r = fitLandscape(parseSaved(null), { x: -500, y: 100 }, left);
    assert.equal(r.x, -FIRST_W);
    assert.equal(r.y, 100);
});

test('no screen known: the kept size at the kept place', () => {
    const r = fitLandscape({ size: { w: 1300, h: 820 }, pos: { x: 5, y: 6 } }, { x: 1, y: 2 }, null);
    assert.deepEqual(r, { w: 1300, h: 820, x: 5, y: 6 });
});

test('the compact window comes back where it stood, or inside the screen', () => {
    assert.deepEqual(compactPlace({ x: 900, y: 40 }, { x: 0, y: 0 }, FHD), { x: 900, y: 40 });
    // Its old place past the right edge: from the wide window's corner.
    assert.deepEqual(compactPlace({ x: 1700, y: 40 }, { x: 300, y: 100 }, FHD), { x: 300, y: 100 });
    // The wide window's corner too far right for 480 px.
    assert.deepEqual(compactPlace(null, { x: 1800, y: 100 }, FHD), { x: 1920 - COMPACT_W, y: 100 });
});

test('a monitor reads as logical pixels, its work area first', () => {
    const mon = {
        scaleFactor: 1.5,
        position: { x: 0, y: 0 }, size: { width: 3840, height: 2160 },
        workArea: { position: { x: 0, y: 0 }, size: { width: 3840, height: 2088 } },
    };
    assert.deepEqual(logicalArea(mon), { x: 0, y: 0, w: 2560, h: 1392 });
    delete mon.workArea;
    assert.deepEqual(logicalArea(mon), { x: 0, y: 0, w: 2560, h: 1440 });
    assert.equal(logicalArea(null), null);
});

const el = (tagName, extra = {}) => ({ nodeType: 1, tagName, ...extra });
const key = (over = {}) => ({ code: 'KeyL', key: 'l', ctrlKey: true, target: el('DIV'), ...over });

test('Ctrl+L switches; Cmd+L too; the Russian layout by its key', () => {
    assert.equal(isSwitchKey(key()), true);
    assert.equal(isSwitchKey(key({ ctrlKey: false, metaKey: true })), true);
    assert.equal(isSwitchKey(key({ key: 'д' })), true);
});

test('no switch without Ctrl, with Shift or Alt, on a held key, or in a text field', () => {
    assert.equal(isSwitchKey(key({ ctrlKey: false })), false);
    assert.equal(isSwitchKey(key({ shiftKey: true })), false);
    assert.equal(isSwitchKey(key({ altKey: true })), false);
    assert.equal(isSwitchKey(key({ repeat: true })), false);
    assert.equal(isSwitchKey(key({ code: 'KeyK', key: 'k' })), false);
    assert.equal(isSwitchKey(key({ target: el('INPUT', { type: 'search' }) })), false);
    assert.equal(isSwitchKey(key({ target: el('TEXTAREA') })), false);
    assert.equal(isSwitchKey(key({ target: el('DIV', { isContentEditable: true }) })), false);
    // A slider or a tick is not a text field.
    assert.equal(isSwitchKey(key({ target: el('INPUT', { type: 'range' }) })), true);
});

test('the plain L is listening mode\'s key, never a text field\'s', () => {
    assert.equal(isListeningKey(key({ ctrlKey: false })), true);
    assert.equal(isListeningKey(key()), false);
    assert.equal(isListeningKey(key({ ctrlKey: false, target: el('INPUT') })), false);
    assert.equal(isTextTarget(el('SELECT')), true);
    assert.equal(isTextTarget(null), false);
});

test('whole rows in the Queue pane, never fewer than three', () => {
    // 10 rows = 10*43 + 9*4 + 2*4 = 474 px of list.
    assert.equal(rowsFor(474), 10);
    assert.equal(rowsFor(473), 9);
    assert.equal(rowsFor(474 + 2, { extra: 2 }), 10);
    assert.equal(rowsFor(100), 3);
    assert.equal(rowsFor(0, { least: 4 }), 4);
});

test('the intro: a fresh profile or one from before it opens in Library once, then the look left is kept', () => {
    // Its key and the window's own never move: an updated profile is read by them.
    assert.equal(KEY, 'auraWindowMode');
    assert.equal(INTRO_KEY, 'auraLibraryIntro');
    // Nothing kept: a first install, or an update from a version without it.
    assert.equal(introDue(null), true);
    // Kept, whatever it says: done, and the listener's look is the window's.
    assert.equal(introDue(introRecord()), false);
    assert.equal(introDue(''), false);
    const r = JSON.parse(introRecord('2026-11-23T20:00:00.000Z'));
    assert.deepEqual(r, { v: 1, at: '2026-11-23T20:00:00.000Z' });
    // The look the intro keeps reads back as Library; a compact one left after it as compact.
    assert.equal(parseSaved(stringifySaved({ ...parseSaved(null), mode: 'landscape' })).mode, 'landscape');
    assert.equal(parseSaved(stringifySaved({ ...parseSaved(null), mode: 'compact' })).mode, 'compact');
});
