// The list's two views (listview.js): F / R first in the list's head shows
// the files or the radio in the same place, and nothing else changes.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    VIEW_TEXTS, viewFromSaved, playingOf, liveMark, listView, setListView, toggleListView, onListView, setListPlaying,
    switchState, viewForKey,
} from '../src/js/listview.js';
import { state } from '../src/js/state.js';

/// Just enough of a page: a body with classes, a store, a backend that
/// counts what it is asked.
function page() {
    const classes = new Set();
    const kept = new Map();
    const asked = [];
    globalThis.document = {
        body: { classList: { toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)), contains: c => classes.has(c) } },
        getElementById: () => null,
    };
    globalThis.localStorage = { getItem: k => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, String(v)) };
    globalThis.window = { __TAURI__: { tauri: { invoke: (cmd, a) => { asked.push(cmd); return Promise.resolve(null); } } } };
    return { classes, kept, asked };
}

test('a kept view: the radio when it was the radio, the files otherwise', () => {
    assert.equal(viewFromSaved('radio'), 'radio');
    for (const v of ['files', null, undefined, '', 'Radio', 'stations']) assert.equal(viewFromSaved(v), 'files', String(v));
});

test('what plays: a stream, a file, or nothing', () => {
    assert.equal(playingOf(null), null);
    assert.equal(playingOf({ state: 'stopped' }), null);
    assert.equal(playingOf({ state: 'playing', trackId: 3 }), 'file');
    assert.equal(playingOf({ state: 'paused', trackId: 3 }), 'file');
    assert.equal(playingOf({ state: 'stopped', trackId: 3 }), null);
    assert.equal(playingOf({ state: 'playing', radio: { phase: 'playing' } }), 'radio');
    assert.equal(playingOf({ state: 'playing', radio: { phase: 'waiting' }, trackId: 3 }), 'radio', 'a stream is the source');
    assert.equal(playingOf({ state: 'stopped', radio: { stopped: true } }), null);
});

test('the dot: on the letter of what plays when that view is not shown', () => {
    assert.equal(liveMark('files', 'radio'), 'radio');
    assert.equal(liveMark('radio', 'file'), 'files');
    assert.equal(liveMark('radio', 'radio'), null, 'in view: no dot');
    assert.equal(liveMark('files', 'file'), null);
    assert.equal(liveMark('files', null), null);
    assert.equal(liveMark('radio', null), null);
});

test('switching the view: kept for the next start, said once, the radio class on the body', () => {
    const { classes, kept } = page();
    const seen = [];
    const off = onListView(v => seen.push(v));
    setListView('files');
    assert.equal(listView(), 'files');
    setListView('radio');
    assert.equal(listView(), 'radio');
    assert.equal(kept.get('auraListView'), 'radio');
    assert.ok(classes.has('lv-radio'));
    setListView('radio');
    assert.deepEqual(seen, ['radio'], 'the same view twice is one change');
    toggleListView();
    assert.equal(listView(), 'files');
    assert.equal(kept.get('auraListView'), 'files');
    assert.ok(!classes.has('lv-radio'));
    // A switch that is not the listener's (the tour) is not kept.
    setListView('radio', { remember: false });
    assert.equal(listView(), 'radio');
    assert.equal(kept.get('auraListView'), 'files');
    setListView('files', { remember: false });
    off();
    assert.deepEqual(seen, ['radio', 'files', 'radio', 'files']);
});

test('the view is only what is shown: the playlist and the player are not touched', () => {
    const { asked } = page();
    state.list = [{ id: 1, name: 'a.flac', trackId: 7 }, { id: 2, name: 'b.flac', trackId: 8 }];
    const before = JSON.stringify(state.list);
    for (let i = 0; i < 4; i++) toggleListView();
    setListPlaying('radio');
    setListPlaying('file');
    setListPlaying(null);
    assert.equal(JSON.stringify(state.list), before, 'the rows are the same rows');
    assert.deepEqual(asked, [], 'nothing asked of the backend: what plays goes on playing');
    state.list = [];
});

test('the switch: on for the radio, the lens under the view shown, the dot on the other side', () => {
    assert.deepEqual(switchState('files', null), { checked: false, on: 'files', live: null });
    assert.deepEqual(switchState('radio', null), { checked: true, on: 'radio', live: null });
    assert.deepEqual(switchState('files', 'radio'), { checked: false, on: 'files', live: 'radio' });
    assert.deepEqual(switchState('radio', 'file'), { checked: true, on: 'radio', live: 'files' });
});

test('the arrows pick a side, other keys leave it to the button', () => {
    assert.equal(viewForKey('ArrowLeft'), 'files');
    assert.equal(viewForKey('ArrowRight'), 'radio');
    for (const k of [' ', 'Enter', 'ArrowUp', 'ArrowDown', 'Tab', 'l']) assert.equal(viewForKey(k), null, k);
});

test('a switch ends the glow of rows just added: it does not play again when the files come back', () => {
    page();
    state.list = [{ id: 1, name: 'a.flac', fresh: true }, { id: 2, name: 'b.flac' }];
    toggleListView();
    assert.deepEqual(state.list.map(e => !!e.fresh), [false, false]);
    assert.equal('fresh' in state.list[1], false, 'a row that was not fresh is left as it was');
    toggleListView();
    state.list = [];
});

test('the words: English, the group named, each letter said', () => {
    assert.equal(VIEW_TEXTS.aria, 'Files or radio');
    assert.match(VIEW_TEXTS.files, /^Files: /);
    assert.match(VIEW_TEXTS.radio, /^Radio: /);
    for (const s of Object.values(VIEW_TEXTS)) assert.ok(!/[Ѐ-ӿ]/.test(s), s);
});
