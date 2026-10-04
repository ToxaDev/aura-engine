// ══════════════════════════════════════════════════════════════════════
// The newcomer's tour of the main window.
//
// Offered once to anyone it has not met: first the main window from the
// top down — the settings every file is rendered with (FS multiplier, the
// filter's resolution, GPU acceleration, Apodizing, Headroom), the Advanced
// DSP rack, the playlists and Convert all, where the files go and what a
// row's buttons do — then, last, the player over the list (the stages it
// plays, output, the analyzer, transport, its right-click menu and Instant
// start); then the player on the whole window and what is in it (the
// picture, the scenes and their studio), and last the player's top edge:
// the listener folds the big player back with it, and the tour is over
// (Done folds it too). Nothing else is asked of them: the tour switches
// into the listening mode itself, and Skip puts back the layout it found.
//
// The ? under the version brings it back any time; the tour, done or
// skipped, flies into it (tour-engine.js).
//
// The steps and their texts are tour-core.js; here is only what each one
// points at in this window, and what it does on its way in and out.
// ══════════════════════════════════════════════════════════════════════

import { createTour, shown } from './tour-engine.js';
import { TOUR_TEXTS, MAIN_STEPS, TOUR_KEY, shouldOffer, tourRecord, stepText } from './tour-core.js';
import { isListening, setListening } from './listening.js';
import { listView, setListView } from './listview.js';
import { holdPlayerControls, playerMenuPreview } from './player.js';
import { hide as hideTip } from './tooltip.js';
import { ICON_PLAY_SM, ICON_CONVERT, ICON_M, ICON_X_SM } from './icons.js';

const $ = id => document.getElementById(id);
const q = sel => document.querySelector(sel);
const frame = () => new Promise(r => requestAnimationFrame(() => r()));
const wait = ms => new Promise(r => setTimeout(r, ms));

// ── the list ──────────────────────────────────────────────────────────

const rowsOnList = () => !!q('#fileQueue .pl-item');

/// The first row wholly in sight (the list may be scrolled).
function firstVisibleRow() {
    const list = $('fileQueue');
    if (!list) return null;
    const lr = list.getBoundingClientRect();
    for (const r of list.querySelectorAll('.pl-item')) {
        const b = r.getBoundingClientRect();
        if (b.height > 0 && b.top >= lr.top - 1 && b.bottom <= lr.bottom + 1) return r;
    }
    return list.querySelector('.pl-item');
}

const KEY_ICONS = { play: ICON_PLAY_SM, convert: ICON_CONVERT, memory: ICON_M, remove: ICON_X_SM };

/// A row's buttons, each beside what it does.
function rowBody() {
    const t = TOUR_TEXTS.main.row;
    const wrap = document.createElement('div');
    const intro = document.createElement('div');
    intro.textContent = stepText(t, rowsOnList());
    const keys = document.createElement('div');
    keys.className = 'tour-keys';
    for (const [icon, text] of t.keys) {
        const k = document.createElement('span');
        k.className = `tour-key tour-key-${icon}`;
        k.innerHTML = KEY_ICONS[icon] || '';
        const d = document.createElement('span');
        d.textContent = text;
        keys.append(k, d);
    }
    wrap.append(intro, keys);
    return wrap;
}

// ── the right-click menu, shown in place ─────────────────────────────
// The player's own first page (player.js builds it as a right-click would
// now: the studio's and the big player's differ), laid over the player
// under the dim: seen, not pressed.

const MENU_STEPS = new Set(['menu', 'instant', 'studio']);
let replica = null;

function ensureReplica() {
    const mode = isListening() ? 'listening' : 'studio';
    if (replica?.isConnected && replica.dataset.tourMode === mode) return replica;
    dropReplica();
    let menu = null;
    try { menu = playerMenuPreview(); } catch (_) { return null; }
    if (!menu) return null;
    menu.classList.add('tour-replica');
    menu.setAttribute('aria-hidden', 'true');
    menu.dataset.tourMode = mode;
    // Its rows carry the real menu's ids; the copy keeps them as names
    // only, so nothing looking for the real menu finds this one.
    menu.querySelectorAll('[id]').forEach(e => { e.dataset.tourRow = e.id; e.removeAttribute('id'); });
    document.body.appendChild(menu);
    replica = menu;
    placeReplica();
    return menu;
}

/// Where a right-click in the middle of the player would open it.
function placeReplica() {
    const bar = $('plBar');
    if (!replica || !bar) return;
    const b = bar.getBoundingClientRect();
    const w = replica.offsetWidth, h = replica.offsetHeight;
    const x = Math.round(b.left + (b.width - w) / 2);
    const y = Math.round(b.top + Math.max(18, Math.min(b.height - h - 8, (b.height - h) / 2 - 24)));
    const left = x + 'px', top = y + 'px';
    if (replica.style.left !== left) replica.style.left = left;
    if (replica.style.top !== top) replica.style.top = top;
}

function dropReplica() {
    replica?.remove();
    replica = null;
}

const replicaRow = id => ensureReplica()?.querySelector(`[data-tour-row="${id}"]`) || null;

// ── what each step points at ─────────────────────────────────────────

/// A setting's cell in the rack's strip at the top of the window (rackstrip.js).
const cell = id => q(`#rkStrip .rk-cell[data-cell="${id}"]`);

const TARGETS = {
    settings: () => [$('rkStrip')],
    fs: () => [cell('fs')],
    // The note under the strip (a missing filter's download) with it, when shown.
    taps: () => [cell('taps'), $('convFilterAvail')],
    gpu: () => [cell('gpu')],
    apodizing: () => [cell('apod')],
    headroom: () => [cell('hr')],
    // F/R first in the list's head, and the radio it shows in the list's place.
    view: () => [$('plView')],
    radio: () => [q('#plListHead .rd-htabs'), $('dropZone')],
    rack: () => [$('labField')],
    playlist: () => [$('plPlaylistCtrl')],
    convertAll: () => [$('plConvertAll')],
    drop: () => [$('dropZone')],
    row: () => [firstVisibleRow() || $('dropZone')],
    player: () => [$('plBar')],
    badges: () => [$('plStatus')],
    output: () => [$('plDevPick'), $('plBitPerfect'), q('#plBar .pl-vol-wrap')],
    analyzer: () => [$('anBtn')],
    transport: () => [q('#plBar .pl-transport')],
    menu: () => [ensureReplica()],
    // The row lit, the menu round it uncovered (the tip keeps off all of it).
    instant: () => [replicaRow('plMenuInstant'), replica],
    listening: () => [$('plBar')],
    picture: () => [$('plFullBtn'), $('plWavesBtn'), $('plBarsBtn')],
    studio: () => [replicaRow('plVisMenuBtn'), replica],
    fold: () => [$('plModeStrip')],
};

/// Whether a step has anything to point at in this window. Asked once, as
/// the tour starts; the listening mode's parts are in the page (hidden)
/// in the studio, so they answer here too. The menu is built only when
/// shown.
const EXISTS = {
    view: () => !!$('plView'),
    radio: () => !!$('rdView'),
    row: () => !!$('dropZone'),
    menu: () => !!$('plBar') && typeof playerMenuPreview === 'function',
    instant: () => !!$('plBar') && typeof playerMenuPreview === 'function',
    picture: () => !!($('plFullBtn') || $('plWavesBtn') || $('plBarsBtn')),
    studio: () => !!$('plWavesBtn') && typeof playerMenuPreview === 'function',
    fold: () => !!$('plModeStrip'),
};

const hasListening = () => !!$('plModeStrip');

/// The steps that show the player's edge: it stands out as if the pointer
/// were near it.
const FLAP_STEPS = new Set(['listening', 'fold']);

/// Wait for the listening mode's switch to finish (listening.js keeps
/// lm-anim on the body while it runs, ~0.3 s). Not remembered: the app
/// closed in the middle of the tour opens in the listener's own layout.
async function switchTo(on) {
    setListening(on, true, false);
    const t0 = performance.now();
    await frame();
    while (document.body.classList.contains('lm-anim') && performance.now() - t0 < 1500) await wait(30);
    await wait(60);
}

function buildSteps() {
    return MAIN_STEPS.map(s => {
        const t = TOUR_TEXTS.main[s.id];
        const step = {
            id: s.id,
            side: s.side,
            pad: s.pad,
            rings: s.rings,
            live: !!s.live,
            title: t.title,
            body: () => stepText(t, rowsOnList()),
            targets: TARGETS[s.id],
            exists: EXISTS[s.id] || (() => TARGETS[s.id]().some(Boolean)),
        };
        if (s.id === 'row') step.body = rowBody;
        const on = s.mode === 'listening';
        if (on) {
            const base = step.exists;
            step.exists = () => hasListening() && base();
        }
        // The list's view the step is shown in (F/R): the radio's step shows
        // the radio, the files' steps the files; the others leave it be. Not
        // kept: the tour puts the listener's view back at its end.
        const view = s.view || null;
        const wrongView = () => !!view && listView() !== view;
        step.needsPrep = () => (hasListening() && isListening() !== on) || wrongView();
        step.prepare = async () => {
            if (wrongView()) { setListView(view, { remember: false }); await frame(); }
            if (hasListening() && isListening() !== on) await switchTo(on);
        };
        if (MENU_STEPS.has(s.id)) {
            step.enter = () => { ensureReplica(); };
            // Gone before a layout switch, which the tour shows undimmed.
            step.leave = (_, to) => { if (!to || !MENU_STEPS.has(to.id)) dropReplica(); };
        } else {
            step.enter = () => dropReplica();
        }
        if (FLAP_STEPS.has(s.id)) {
            step.enter = () => { dropReplica(); $('plBar')?.classList.add('tour-flap'); };
            step.leave = () => $('plBar')?.classList.remove('tour-flap');
        }
        if (s.id === 'fold') {
            // The listener's turn: the edge is theirs to press, and the L
            // key with it; once the big player has folded, the tour is done.
            step.keys = ['KeyL'];
            step.over = () => !isListening();
        }
        return step;
    });
}

// ── the ? under the version ───────────────────────────────────────────

function makeHelp(onClick) {
    const had = $('tourHelp');
    if (had) return had;
    const host = q('#converterPanel .app-head-bar') || $('converterPanel');
    if (!host) return null;
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'tourHelp';
    b.className = 'tour-help';
    b.textContent = '?';
    b.setAttribute('aria-label', TOUR_TEXTS.help.aria);
    b.dataset.tip = TOUR_TEXTS.help.tip;
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();   // Space stays the player's
        onClick();
    });
    host.appendChild(b);
    return b;
}

// ── the profile ───────────────────────────────────────────────────────

function readStored() {
    try { return localStorage.getItem(TOUR_KEY); } catch (_) { return null; }
}

function store(state) {
    try { localStorage.setItem(TOUR_KEY, tourRecord(state)); } catch (_) { /* a tour offered twice is no harm */ }
}

// ── init ──────────────────────────────────────────────────────────────

let tour = null;
let wasListening = false;
let wasView = 'files';     // the list's view the tour found
let scrollBefore = null;   // the panel's and the page's, put back after the tour

/// Called by main.js once the window is up. `after`: the update check — the
/// offer waits for it (up to 15 s), so it never comes up under the update
/// dialog.
export function initTour({ after = null } = {}) {
    if (tour) return;
    tour = createTour({
        name: 'main',
        texts: TOUR_TEXTS,
        steps: buildSteps(),
        help: () => $('tourHelp'),
        begin() {
            hideTip();
            wasListening = isListening();
            wasView = listView();
            scrollBefore = { panel: $('converterPanel')?.scrollTop || 0, x: window.scrollX, y: window.scrollY };
            try { holdPlayerControls(true); } catch (_) { /* */ }
        },
        end(kind) {
            dropReplica();
            $('plBar')?.classList.remove('tour-flap');
            try { holdPlayerControls(false); } catch (_) { /* */ }
            // The list in the view the listener had.
            if (listView() !== wasView) setListView(wasView, { remember: false });
            // The window as the tour found it: scrolled where it was (a step
            // may have brought its element into sight)...
            if (scrollBefore) {
                const panel = $('converterPanel');
                if (panel) panel.scrollTop = scrollBefore.panel;
                window.scrollTo(scrollBefore.x, scrollBefore.y);
                scrollBefore = null;
            }
            if (!hasListening()) return false;
            // ...and in the layout the listener had (Skip, Esc). Asked for
            // always, not only when it differs: with reduced motion the layout
            // changes at the end of a fade, and a tour left during that fade
            // would see the old layout and leave the new one to come.
            // Done: the tour ends on the big player's edge, and Done folds it
            // as a press of the edge does, kept as that press keeps it. The
            // listener's own press (or L) has folded it already: its
            // animation is left to run.
            const to = kind === 'done' ? false : wasListening;
            const moves = isListening() !== to || document.body.classList.contains('lm-anim');
            if (kind === 'done' && !isListening()) return moves;
            setListening(to, kind !== 'abort', kind === 'done');
            return moves;
        },
        record: store,
        frame: placeReplica,
    });
    makeHelp(() => { if (!tour.isOpen()) { hideTip(); tour.offer('again'); } });

    // For the test tool (rw): it takes the tour off the screen of a run.
    window.__auraTour = {
        abort: () => tour.abort(),
        offer: kind => tour.offer(kind || 'again'),
        start: () => tour.start(),
        next: () => tour.next(),
        back: () => tour.back(),
        skip: () => tour.skip(),
        state: () => tour.state(),
    };

    if (shouldOffer({ stored: readStored() }) === 'offer') {
        Promise.race([Promise.resolve(after), wait(15000)])
            .catch(() => {})
            .then(() => setTimeout(offerWhenFree, 400));
    }
}

/// The offer, once the window is free for it: not over the update dialog,
/// not in full screen.
function offerWhenFree() {
    if (shouldOffer({ stored: readStored() }) !== 'offer' || tour.isOpen()) return;
    const up = $('upModal');
    const busy = document.body.classList.contains('fullview')
        || (up && !up.classList.contains('up-hidden') && shown(up));
    if (busy) { setTimeout(offerWhenFree, 800); return; }
    tour.offer('first');
}
