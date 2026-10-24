// ══════════════════════════════════════════════════════════════════════
// The list's three views: the files (the playlists), the radio, and the
// input (what another player plays into Aura Player ASIO, input.js).
//
// One place in the window shows one of them, and F / R / I first in the
// list's head switches them (Anton 2.10, the input 16.10): the playlist's
// controls and rows, the radio's tabs and stations, or the input's card.
// F / R / I is one narrow button that turns round the three (Anton 21.10:
// the three-letter capsule took the playlist's name its room): it shows the
// letter of the view and three pips under it, the view's pip lit, so the
// three are still seen; a click is the next one, a right-click or
// Shift+click the one before. A view is only what is shown — nothing is
// rebuilt or stopped, the playlist keeps its rows and its scroll, what plays
// goes on playing; only choosing something new to play changes the sound.
// The view is kept for the next start.
//
// What plays and is not in view says so: its pip breathes (list.css).
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';

const KEY = 'auraListView';
/// The views, in the order a click goes round them, and the body's class
/// for each but the files.
export const VIEWS = ['files', 'radio', 'input'];
const CLASS = { radio: 'lv-radio', input: 'lv-input' };
const LETTER = { files: 'F', radio: 'R', input: 'I' };
const NAME = { files: 'Files', radio: 'Radio', input: 'Input' };

// UI texts.
export const VIEW_TEXTS = {
    aria: 'Files, radio or input',
    files: 'Files: your playlists — play a file through the rack, or convert it.',
    radio: 'Radio: featured stations, a catalog to search, or any stream address.',
    input: 'Input: what another player plays into Aura Player ASIO — foobar2000, JRiver, HQPlayer, Roon.',
};

/// The button's tip: the view shown, then where a click and a right-click
/// go, the three named in their order.
export const switchTip = view =>
    `${VIEW_TEXTS[view]}\nClick for ${NAME[nextView(view)]}, right-click for ${NAME[prevView(view)]} (Files → Radio → Input).`;

/// The view a stored value asks for: the radio or the input, or else the files.
export const viewFromSaved = v => (v === 'radio' || v === 'input' ? v : 'files');

/// What plays, from the player's status: another player's stream through
/// the input, a radio stream, a file, or nothing.
export function playingOf(s) {
    if (s?.input?.listening) return 'input';
    if (s?.radio && !s.radio.stopped) return 'radio';
    if (s && s.trackId != null && s.state && s.state !== 'stopped') return 'file';
    return null;
}

/// The view each source plays in.
const VIEW_OF = { file: 'files', radio: 'radio', input: 'input' };

/// The letter that breathes: the view of what plays, when it is not the one
/// shown.
export function liveMark(view, playing) {
    const of = VIEW_OF[playing];
    return of && of !== view ? of : null;
}

/// What the switch shows: the view whose letter and pip are lit, the view
/// whose pip breathes.
export const switchState = (view, playing) => ({ on: view, live: liveMark(view, playing) });

/// The view an arrow key asks for, from `view`: ← the one before, → the one
/// after, round the three.
export function viewForKey(key, view = 'files') {
    const step = key === 'ArrowLeft' ? -1 : key === 'ArrowRight' ? 1 : 0;
    if (!step) return null;
    const i = Math.max(0, VIEWS.indexOf(view));
    return VIEWS[(i + step + VIEWS.length) % VIEWS.length];
}

/// The view after `view`, round the three: what a click shows.
export const nextView = view => viewForKey('ArrowRight', view);
/// The view before `view`: what a right-click or Shift+click shows.
export const prevView = view => viewForKey('ArrowLeft', view);

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

/// The next view, round the three.
export const toggleListView = () => setListView(nextView(view));

/// What plays now (playingOf): the breathing pip follows it.
export function setListPlaying(p) {
    if (p === playing) return;
    playing = p;
    paint();
}

function paint() {
    if (typeof document === 'undefined') return;
    for (const [v, cls] of Object.entries(CLASS)) document.body.classList.toggle(cls, view === v);
    if (!trig) return;
    const sw = switchState(view, playing);
    for (const p of trig.querySelectorAll('.lv-pip')) {
        const v = p.dataset.view;
        p.classList.toggle('on', v === sw.on);
        p.classList.toggle('live', v === sw.live);
    }
    const cur = trig.querySelector('.lv-cur');
    if (cur && cur.textContent !== LETTER[view]) {
        cur.textContent = LETTER[view];
        // The new letter comes in (list.css lvIn); the class is taken off and
        // put back so it plays again on each turn.
        cur.classList.remove('lv-in');
        void cur.offsetWidth;
        cur.classList.add('lv-in');
    }
    trig.dataset.view = view;
    trig.dataset.tip = switchTip(view);
    trig.setAttribute('aria-label', `${VIEW_TEXTS.aria}: ${view}`);
}

/// F / R / I and the rule after it, first in the list's head; the view the
/// last start was in. A click shows the next view, a right-click or
/// Shift+click the one before; Space and Enter press it (the next one), ←
/// and → step round. Files brought to the window go on the playlist, so it
/// comes into view as they arrive.
export function mountListView() {
    const head = document.getElementById('plListHead');
    if (!head || document.getElementById('plView')) return;
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'plView';
    b.className = 'lv-trig';
    b.setAttribute('aria-label', VIEW_TEXTS.aria);
    b.innerHTML = '<span class="lv-cur" aria-hidden="true"></span><span class="lv-pips" aria-hidden="true">'
        + VIEWS.map(v => `<i class="lv-pip" data-view="${v}"></i>`).join('') + '</span>';
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();   // Space stays the player's
        setListView(e.shiftKey ? prevView(view) : nextView(view));
    });
    b.addEventListener('contextmenu', e => {
        e.preventDefault();
        b.blur();
        setListView(prevView(view));
    });
    b.addEventListener('keydown', e => {
        const v = viewForKey(e.key, view);
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
    const toFiles = e => { if ((!e?.windowLabel || e.windowLabel === me) && view !== 'files') setListView('files'); };
    for (const name of ['tauri://file-drop-hover', 'tauri://file-drop']) {
        Promise.resolve(ev.listen(name, toFiles)).catch(() => {});
    }
}
