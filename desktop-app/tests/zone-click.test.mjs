// A click on the drop zone opens the file picker, but not one that is the
// radio's, the rows' or the cancel button's (zone-click.js) — even when the
// element clicked is out of the page by the time the click reaches the
// zone: a station's click redraws the radio's list (Anton 4.10: any station
// in Featured opened "Select Audio Files").
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The page here: elements with parents, matches() and closest(), and a click
// that, as the DOM's, keeps the path it was sent along (composedPath) while
// the handlers on the way change the page.

import test from 'node:test';
import assert from 'node:assert/strict';

import { ZONE_OWN, cameThrough, zoneClickBrowses } from '../src/js/zone-click.js';

class El {
    constructor(tag, { id = '', cls = '' } = {}, parent = null) {
        this.tag = tag;
        this.id = id;
        this.classes = new Set(cls.split(' ').filter(Boolean));
        this.parent = null;
        this.children = [];
        this.listeners = [];
        if (parent) parent.append(this);
    }
    append(c) { c.parent = this; this.children.push(c); }
    replaceChildren(...cs) {
        for (const c of this.children) c.parent = null;
        this.children = [];
        for (const c of cs) this.append(c);
    }
    matches(sel) {
        return sel.split(',').map(s => s.trim()).some(s => (s.startsWith('#') ? this.id === s.slice(1)
            : s.startsWith('.') ? s.slice(1).split('.').every(c => this.classes.has(c)) : this.tag === s));
    }
    closest(sel) {
        for (let n = this; n && typeof n.matches === 'function'; n = n.parent) if (n.matches(sel)) return n;
        return null;
    }
    addEventListener(type, fn) { this.listeners.push({ type, fn }); }
}

/// A click bubbling from `target`; `path: false` — an engine without composedPath.
function click(target, { path = true } = {}) {
    const p = [];
    for (let n = target; n; n = n.parent) p.push(n);
    const ev = { type: 'click', target };
    if (path) ev.composedPath = () => p.slice();
    for (const n of p) for (const l of n.listeners) if (l.type === 'click') l.fn(ev);
    return ev;
}

/// The page: the window and the document (no matches(), as theirs), the
/// zone with the files' list and the radio's view in it.
function page() {
    const win = { listeners: [], parent: null };
    const doc = { listeners: [], parent: win };
    const body = new El('body');
    body.parent = doc;
    const zone = new El('div', { id: 'dropZone', cls: 'smart-drop-zone' }, body);
    const empty = new El('div', { id: 'dzEmpty', cls: 'dz-empty' }, zone);
    const emptyText = new El('div', { cls: 'dz-empty-text' }, empty);
    const queue = new El('div', { id: 'fileQueue', cls: 'dz-file-list' }, zone);
    const file = new El('div', { cls: 'file-item pl-item' }, queue);
    const cancel = new El('button', { id: 'dzCancelBtn', cls: 'dz-cancel-btn' }, zone);
    const view = new El('div', { id: 'rdView', cls: 'rd-view' }, zone);
    const list = new El('div', { id: 'rdList', cls: 'rd-list' }, view);
    const st = new El('div', { cls: 'rd-st' }, list);
    const name = new El('span', { cls: 'rd-nm' }, st);
    const more = new El('button', { cls: 'rd-btn rd-more' }, list);
    // As radio.js: a station's click plays it and redraws the list at once
    // (Featured has no catalog id to wait for); More redraws it busy.
    const redraw = () => list.replaceChildren(new El('div', { cls: 'rd-st' }), new El('button', { cls: 'rd-btn rd-more' }));
    st.addEventListener('click', redraw);
    more.addEventListener('click', redraw);
    // As dropzone.js: the zone's click, last on the way.
    const picker = [];
    zone.addEventListener('click', e => { if (zoneClickBrowses(e)) picker.push(e.target); });
    return { win, doc, body, zone, empty, emptyText, file, cancel, view, list, st, name, more, picker };
}

test('a station whose click redraws the radio\'s list opens no file picker', () => {
    const p = page();
    const ev = click(p.name);
    assert.equal(p.st.parent, null, 'the station is out of the page by the time the click reaches the zone');
    assert.deepEqual(p.picker, []);
    // What the zone asked before: the element clicked and its ancestors —
    // out of the page, it has none, and the picker opened.
    assert.equal(ev.target.closest('#rdView'), null);
    // More in a search: the same.
    click(p.more);
    assert.deepEqual(p.picker, []);
});

test('a click on the zone\'s empty part still opens the file picker, as before', () => {
    const p = page();
    click(p.emptyText);
    click(p.zone);
    assert.deepEqual(p.picker, [p.emptyText, p.zone]);
});

test('the rows, the cancel button and the radio\'s view are the zone\'s own', () => {
    const p = page();
    click(p.file);
    click(p.cancel);
    click(p.list);
    assert.deepEqual(p.picker, []);
    assert.equal(zoneClickBrowses(click(p.emptyText), true), false, 'right after a drop: none');
    for (const sel of ['.file-item', '.dz-cancel-btn', '#plPlaylistCtrl', '#rdView']) assert.ok(ZONE_OWN.includes(sel), sel);
});

test('without a path (an engine that has none) the element and its ancestors are asked', () => {
    const p = page();
    assert.equal(cameThrough(click(p.file, { path: false }), '.file-item'), true);
    assert.equal(cameThrough(click(p.emptyText, { path: false }), '#rdView'), false);
});

test('a control in the radio\'s view is the radio\'s, the rack\'s are not (the rows\' redraw on a rack change)', () => {
    const p = page();
    const sel = new El('select', { id: 'rdOrder' }, p.view);
    const rack = new El('select', { id: 'convHeadroom' }, p.body);
    const ev = s => ({ target: s, composedPath: () => { const a = []; for (let n = s; n; n = n.parent) a.push(n); return a; } });
    assert.equal(cameThrough(ev(sel), '#plBar, #plListHead, #fileQueue, #rdView'), true);
    assert.equal(cameThrough(ev(rack), '#plBar, #plListHead, #fileQueue, #rdView'), false);
});
