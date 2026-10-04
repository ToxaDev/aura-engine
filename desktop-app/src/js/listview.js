// ══════════════════════════════════════════════════════════════════════
// The list's two views: the files (the playlists) and the radio.
//
// One place in the window shows either, and F / R first in the list's head
// switches them (Anton 2.10): the playlist's controls and rows, or the
// radio's tabs and stations. F / R is a switch: a capsule of two letters
// with a lit lens that slides under the one shown (Anton chose it of three). A view is only what is shown — nothing is
// rebuilt or stopped, the playlist keeps its rows and its scroll, what plays
// goes on playing; only choosing something new to play changes the sound.
// The view is kept for the next start.
//
// What plays and is not in view says so: the other letter breathes (list.css).
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';

const KEY = 'auraListView';
const CLS = 'lv-radio';

// UI texts.
export const VIEW_TEXTS = {
    aria: 'Files or radio',
    files: 'Files: your playlists — play a file through the rack, or convert it.',
    radio: 'Radio: featured stations, a catalog to search, or any stream address',
};

/// The view a stored value asks for: the radio, or else the files.
export const viewFromSaved = v => (v === 'radio' ? 'radio' : 'files');

/// What plays, from the player's status: a stream, a file, or nothing.
export function playingOf(s) {
    if (s?.radio && !s.radio.stopped) return 'radio';
    if (s && s.trackId != null && s.state && s.state !== 'stopped') return 'file';
    return null;
}

/// The letter that carries the dot: the view of what plays, when it is not
/// the one shown.
export function liveMark(view, playing) {
    if (playing === 'radio' && view !== 'radio') return 'radio';
    if (playing === 'file' && view !== 'files') return 'files';
    return null;
}

/// What the switch shows: on (aria-checked) for the radio, the letter lit
/// under the lens, the letter with the dot.
export const switchState = (view, playing) => ({ checked: view === 'radio', on: view, live: liveMark(view, playing) });

/// The side an arrow key asks for: ← the files, → the radio.
export const viewForKey = key => (key === 'ArrowLeft' ? 'files' : key === 'ArrowRight' ? 'radio' : null);

const load = () => { try { return localStorage.getItem(KEY); } catch (_) { return null; } };
const store = v => { try { localStorage.setItem(KEY, v); } catch (_) {} };

let view = 'files';
let playing = null;
let trig = null;
const subs = new Set();

export const listView = () => view;

/// Called with the view each time it changes.
export function onListView(fn) {
    subs.add(fn);
    return () => subs.delete(fn);
}

/// Show `v`. `remember` false: a switch that is not the listener's (the tour
/// shows the radio and puts the listener's view back) is not kept.
export function setListView(v, { remember = true } = {}) {
    const next = viewFromSaved(v);
    if (remember) store(next);
    if (next === view) return;
    view = next;
    settleFresh();
    paint();
    for (const fn of subs) fn(view);
}

/// Rows just added glow as they come in (list.css pl-row-appear) and lose
/// the glow when it has played. A view switched meanwhile hides them before
/// it ends, and a hidden row's glow never ends: it would play again each time
/// the files came back. A switch ends it where it is.
function settleFresh() {
    if (typeof document === 'undefined') return;
    for (const el of document.querySelectorAll?.('#fileQueue .pl-fresh') || []) el.classList.remove('pl-fresh');
    for (const e of state.list) if (e.fresh) e.fresh = false;
}

export const toggleListView = () => setListView(view === 'radio' ? 'files' : 'radio');

/// What plays now (playingOf): the dot follows it.
export function setListPlaying(p) {
    if (p === playing) return;
    playing = p;
    paint();
}

function paint() {
    if (typeof document === 'undefined') return;
    document.body.classList.toggle(CLS, view === 'radio');
    if (!trig) return;
    const sw = switchState(view, playing);
    for (const l of trig.querySelectorAll('.lv-l')) {
        const v = l.dataset.view;
        l.classList.toggle('on', v === sw.on);
        l.classList.toggle('live', v === sw.live);
    }
    // The lens follows (list.css).
    trig.dataset.view = view;
    trig.setAttribute('aria-checked', sw.checked ? 'true' : 'false');
}

/// F / R and the rule after it, first in the list's head; the view the last
/// start was in. A click anywhere on it switches; Space and Enter press it,
/// ← and → pick a side. Files brought to the window go on the playlist, so it
/// comes into view as they arrive.
export function mountListView() {
    const head = document.getElementById('plListHead');
    if (!head || document.getElementById('plView')) return;
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'plView';
    b.className = 'lv-trig';
    b.setAttribute('role', 'switch');
    b.setAttribute('aria-label', VIEW_TEXTS.aria);
    b.innerHTML = '<span class="lv-lens" aria-hidden="true"></span>'
        + '<span class="lv-l" data-view="files">F<i class="lv-dot"></i></span>'
        + '<span class="lv-l" data-view="radio">R<i class="lv-dot"></i></span>';
    b.querySelector('[data-view="files"]').dataset.tip = VIEW_TEXTS.files;
    b.querySelector('[data-view="radio"]').dataset.tip = VIEW_TEXTS.radio;
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();   // Space stays the player's
        toggleListView();
    });
    b.addEventListener('keydown', e => {
        const v = viewForKey(e.key);
        if (!v) return;
        e.preventDefault();
        setListView(v);
    });
    const rule = document.createElement('span');
    rule.className = 'lv-rule';
    rule.setAttribute('aria-hidden', 'true');
    head.prepend(b, rule);
    trig = b;
    view = viewFromSaved(load());
    paint();

    const ev = window.__TAURI__?.event;
    if (!ev) return;
    const me = window.__TAURI__.window?.appWindow?.label ?? 'main';
    const toFiles = e => { if ((!e?.windowLabel || e.windowLabel === me) && view === 'radio') setListView('files'); };
    for (const name of ['tauri://file-drop-hover', 'tauri://file-drop']) {
        Promise.resolve(ev.listen(name, toFiles)).catch(() => {});
    }
}
