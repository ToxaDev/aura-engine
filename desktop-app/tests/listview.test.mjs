// The list's three views (listview.js): one F / R / I button first in the
// list's head turns round the files, the radio and the input in the same
// place, and nothing else changes.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    VIEW_TEXTS, VIEWS, viewFromSaved, playingOf, liveMark, listView, setListView, toggleListView, onListView, setListPlaying,
    switchState, viewForKey, nextView, prevView, switchTip, dropView,
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
    globalThis.window = { __TAURI__: { core: { invoke: (cmd, a) => { asked.push(cmd); return Promise.resolve(null); } } } };
    return { classes, kept, asked };
}

test('a kept view: the radio or the input when it was one, the files otherwise', () => {
    assert.equal(viewFromSaved('radio'), 'radio');
    assert.equal(viewFromSaved('input'), 'input');
    for (const v of ['files', null, undefined, '', 'Radio', 'stations', 'Input', 'asio']) assert.equal(viewFromSaved(v), 'files', String(v));
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
    assert.equal(playingOf({ state: 'playing', input: { listening: true } }), 'input');
    assert.equal(playingOf({ state: 'playing', input: { listening: true }, radio: { stopped: true }, trackId: 3 }), 'input', 'the input is the source');
    assert.equal(playingOf({ state: 'playing', input: { listening: false }, trackId: 3 }), 'file', 'an input not listened to is not what plays');
});

test('the breathing pip: on the view of what plays when that view is not shown', () => {
    assert.equal(liveMark('files', 'radio'), 'radio');
    assert.equal(liveMark('radio', 'file'), 'files');
    assert.equal(liveMark('radio', 'radio'), null, 'in view: nothing breathes');
    assert.equal(liveMark('files', 'file'), null);
    assert.equal(liveMark('files', null), null);
    assert.equal(liveMark('radio', null), null);
    assert.equal(liveMark('files', 'input'), 'input');
    assert.equal(liveMark('radio', 'input'), 'input');
    assert.equal(liveMark('input', 'input'), null);
    assert.equal(liveMark('input', 'file'), 'files');
    assert.equal(liveMark('input', 'radio'), 'radio');
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
    assert.equal(listView(), 'input', 'the next one round the three');
    assert.equal(kept.get('auraListView'), 'input');
    assert.ok(classes.has('lv-input') && !classes.has('lv-radio'));
    toggleListView();
    assert.equal(listView(), 'files');
    assert.equal(kept.get('auraListView'), 'files');
    assert.ok(!classes.has('lv-radio') && !classes.has('lv-input'));
    // A switch that is not the listener's (the tour) is not kept.
    setListView('radio', { remember: false });
    assert.equal(listView(), 'radio');
    assert.equal(kept.get('auraListView'), 'files');
    setListView('files', { remember: false });
    off();
    assert.deepEqual(seen, ['radio', 'input', 'files', 'radio', 'files']);
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

test('the switch: the view shown lit, the pip of what plays elsewhere breathing', () => {
    assert.deepEqual(switchState('files', null), { on: 'files', live: null });
    assert.deepEqual(switchState('radio', null), { on: 'radio', live: null });
    assert.deepEqual(switchState('files', 'radio'), { on: 'files', live: 'radio' });
    assert.deepEqual(switchState('radio', 'file'), { on: 'radio', live: 'files' });
    assert.deepEqual(switchState('files', 'input'), { on: 'files', live: 'input' });
    assert.deepEqual(switchState('input', 'input'), { on: 'input', live: null });
});

test('a click turns Files, Radio, Input and back to Files; a right-click the other way', () => {
    assert.deepEqual([nextView('files'), nextView('radio'), nextView('input')], ['radio', 'input', 'files']);
    assert.deepEqual([prevView('files'), prevView('input'), prevView('radio')], ['input', 'radio', 'files']);
    let v = 'files';
    const seen = [];
    for (let i = 0; i < 6; i++) { v = nextView(v); seen.push(v); }
    assert.deepEqual(seen, ['radio', 'input', 'files', 'radio', 'input', 'files']);
});

test('the tip: the view shown, where a click and a right-click go, the three in order', () => {
    assert.equal(switchTip('files'), `${VIEW_TEXTS.files}\nClick for Radio, right-click for Input (Files → Radio → Input).`);
    assert.match(switchTip('radio'), /^Radio: .*\nClick for Input, right-click for Files /s);
    assert.match(switchTip('input'), /^Input: .*\nClick for Files, right-click for Radio /s);
});

test('the arrows step round the three, other keys leave it to the button', () => {
    assert.deepEqual(VIEWS, ['files', 'radio', 'input']);
    assert.equal(viewForKey('ArrowRight', 'files'), 'radio');
    assert.equal(viewForKey('ArrowRight', 'radio'), 'input');
    assert.equal(viewForKey('ArrowRight', 'input'), 'files');
    assert.equal(viewForKey('ArrowLeft', 'files'), 'input');
    assert.equal(viewForKey('ArrowLeft', 'input'), 'radio');
    assert.equal(viewForKey('ArrowLeft', 'radio'), 'files');
    assert.equal(nextView('radio'), 'input');
    for (const k of [' ', 'Enter', 'ArrowUp', 'ArrowDown', 'Tab', 'l']) assert.equal(viewForKey(k, 'radio'), null, k);
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

test('the words: English, the group named, each view said', () => {
    assert.equal(VIEW_TEXTS.aria, 'Files, radio or input');
    assert.match(VIEW_TEXTS.files, /^Files: /);
    assert.match(VIEW_TEXTS.radio, /^Radio: /);
    assert.match(VIEW_TEXTS.input, /^Input: /);
    for (const s of Object.values(VIEW_TEXTS)) assert.ok(!/[Ѐ-ӿ]/.test(s), s);
});

// Last: it changes the round for the rest of the file.
test('a system without the input has two views: files and radio', () => {
    setListView('input', { remember: false });
    dropView('input');
    assert.deepEqual(VIEWS, ['files', 'radio']);
    assert.equal(listView(), 'files');
    assert.equal(nextView('radio'), 'files');
    assert.equal(viewFromSaved('input'), 'files');
    assert.match(switchTip('files'), /\(Files → Radio\)\.$/);
    dropView('files');
    assert.deepEqual(VIEWS, ['files', 'radio']);
});
