// ══════════════════════════════════════════════════════════════════════
// The landscape window: the same window turned wide and resizable, with
// the media library in the middle (Ctrl+L, or LIBRARY by the window's
// buttons; COMPACT or Ctrl+L again goes back).
//
// One DOM, two layouts, as listening mode does it. Nothing is built twice:
// the rack, the player and the list are the compact window's own elements,
// moved into the landscape shell on the way in and put back where they
// were on the way out, so every control, listener and setting goes on
// working. The converter panel moves into the shell whole and lays its
// children out in the shell's grid (display: contents), so everything that
// listens on the panel (the player's rack changes, the list's re-render)
// still hears them; the library's columns stand beside the panel, outside
// it, so the library's own fields are never taken for rack changes.
//
//   head:   the heading and the window's buttons
//   side:   the library's sections and music folders (medialib)
//   centre: the library (medialib, wide)
//   right:  Queue (the list) | Sound (the rack)
//   foot:   the player
//
// The window's size and place, whether it was maximized, and which mode it
// was left in are kept (localStorage) and put back at the next start. The
// first start of all, and the first after the update that brought this,
// opens in Library (the intro, INTRO_KEY); tour-switch.js then shows the
// switch.
// ══════════════════════════════════════════════════════════════════════

import {
    KEY, INTRO_KEY, introDue, introRecord, MIN_W, MIN_H, COMPACT_W, parseSaved, stringifySaved, fitLandscape, compactPlace,
    logicalArea, isSwitchKey, isListeningKey, rowsFor,
} from './landscape-core.js';
import { isListening, setListening } from './listening.js';
import { revolverResync } from './dropzone.js';
import { listView, setListView } from './listview.js';
import { wireCentrePages, closeCentrePage } from './centre-page.js';
import { watchQueueRows } from './queue-rows.js';

const CLS = 'win-landscape';
const $ = id => document.getElementById(id);
const TW = window.__TAURI__?.window;
const DPI = window.__TAURI__?.dpi;
const appWindow = (() => { try { return TW?.getCurrentWindow(); } catch (_) { return null; } })();

// UI texts, all in one place.
const TEXTS = {
    toWide: 'LIBRARY',
    toWideTip: 'Library view: the window turns landscape and resizable, with the media library (Ctrl+L)',
    toCompact: 'COMPACT',
    toCompactTip: 'Back to the compact window (Ctrl+L)',
    queue: 'Queue',
    sound: 'Sound',
    libMissing: 'The media library is not in this build yet.',
    grip: 'Drag to resize',
};

let saved = load();
let on = false;
let busy = false;           // the window is being moved by us: not the listener's size
let chain = Promise.resolve();
let fitCompact = () => {};
let lib = null, libState = 'none';   // none | loading | ok
let forTour = false;        // left for the tour; the landscape comes back after it
let marks = null;           // where the moved elements came from

export const isLandscape = () => document.body.classList.contains(CLS);

function load() {
    try { return parseSaved(localStorage.getItem(KEY)); } catch (_) { return parseSaved(null); }
}
function store() {
    try { localStorage.setItem(KEY, stringifySaved(saved)); } catch (_) {}
}

/// One switch at a time, in the order asked.
function run(fn) {
    chain = chain.then(fn, fn).catch(e => window.__auraReport?.('Landscape: ' + (e?.message || e)));
    return chain;
}

// ── the shell ─────────────────────────────────────────────────────────

function shell() {
    let s = $('lsShell');
    if (s) return s;
    s = document.createElement('div');
    s.id = 'lsShell';
    s.className = 'ls-shell';
    s.innerHTML = `
      <aside class="ls-side" id="lsSide"></aside>
      <main class="ls-centre" id="lsCentre"></main>
      <div class="ls-grip" id="lsGrip" title="${TEXTS.grip}" aria-hidden="true"></div>`;
    document.body.appendChild(s);
    $('lsGrip').addEventListener('mousedown', e => {
        if (e.button !== 0) return;
        e.preventDefault();
        appWindow?.startResizeDragging?.('SouthEast')?.catch?.(() => {});
    });
    return s;
}

function right() {
    let r = $('lsRight');
    if (r) return r;
    r = document.createElement('div');
    r.id = 'lsRight';
    r.className = 'ls-right';
    r.innerHTML = `
      <div class="ls-tabs" role="tablist">
        <button type="button" class="ls-tab" data-tab="queue" role="tab">${TEXTS.queue}</button>
        <button type="button" class="ls-tab" data-tab="sound" role="tab">${TEXTS.sound}</button>
      </div>
      <div class="ls-pane ls-queue" id="lsQueue" data-pane="queue"></div>
      <div class="ls-pane ls-sound" id="lsSound" data-pane="sound"></div>`;
    r.querySelector('.ls-tabs').addEventListener('click', e => {
        const b = e.target.closest('.ls-tab');
        if (!b) return;
        if (e.detail > 0) b.blur();   // Space stays play/pause
        setTab(b.dataset.tab);
    });
    try { new ResizeObserver(() => requestAnimationFrame(sizeQueue)).observe(r.querySelector('#lsQueue')); } catch (_) {}
    return r;
}

function setTab(tab) {
    saved.tab = tab === 'sound' ? 'sound' : 'queue';
    store();
    const r = $('lsRight');
    if (!r) return;
    r.dataset.tab = saved.tab;
    r.querySelectorAll('.ls-tab').forEach(b => {
        const sel = b.dataset.tab === saved.tab;
        b.classList.toggle('on', sel);
        b.setAttribute('aria-selected', String(sel));
    });
    requestAnimationFrame(sizeQueue);
}

/// The Queue tab shown (the foot's playlist name leads to it).
export function showQueueTab() { if (on) setTab('queue'); }

/// The compact window's blocks into the shell: the heading stays in the
/// head, the rack goes to Sound, the player to the foot, the list (and
/// whatever stands in its place: radio, input, the library's compact view)
/// to Queue. Comment marks keep their places for the way back.
function moveIn() {
    const panel = $('converterPanel');
    const bar = $('plBar');
    if (!panel || !bar || bar.parentNode !== panel) return false;
    const kids = [...panel.children];
    const at = kids.indexOf(bar);
    const head = el => el.matches('.app-head-bar, h2');
    const rack = kids.slice(0, at).filter(el => !head(el));
    const list = kids.slice(at + 1);
    const r = right();
    const mark = name => document.createComment('ls:' + name);
    marks = { panel: mark('panel'), rack: mark('rack'), bar: mark('bar'), list: mark('list') };
    (rack[0] || bar).before(marks.rack);
    bar.before(marks.bar);
    bar.after(marks.list);
    r.querySelector('#lsSound').append(...rack);
    r.querySelector('#lsQueue').append(...list);
    const s = shell();
    panel.before(marks.panel);
    s.prepend(panel);
    panel.append(r);
    panel.append(bar);   // the foot; its place in the grid is the stylesheet's
    return true;
}

function moveOut() {
    if (!marks) return;
    const panel = $('converterPanel');
    const bar = $('plBar');
    const sound = $('lsSound'), queue = $('lsQueue');
    if (sound) marks.rack.before(...sound.childNodes);
    if (bar) marks.bar.before(bar);
    if (queue) marks.list.before(...queue.childNodes);
    $('lsRight')?.remove();
    if (panel) marks.panel.before(panel);
    for (const m of Object.values(marks)) m.remove();
    marks = null;
}

/// Whole rows of the list in the Queue pane's height.
function sizeQueue() {
    const host = $('lsQueue'), zone = $('dropZone'), list = $('fileQueue');
    if (!on || !host || !zone || !list || !host.contains(zone)) return;
    if (!host.clientHeight) return;   // Sound is showing
    const px = v => parseFloat(v) || 0;
    const ls = getComputedStyle(list);
    const rowH = px(ls.getPropertyValue('--pl-row-h')) || 43;
    const gap = px(ls.getPropertyValue('--pl-row-gap')) || 4;
    const pad = px(ls.getPropertyValue('--pl-list-pad')) || 4;
    let others = 0;
    for (const el of host.children) {
        if (el === zone || !el.getClientRects().length) continue;
        const cs = getComputedStyle(el);
        if (cs.position === 'absolute' || cs.position === 'fixed') continue;
        others += el.getBoundingClientRect().height + px(cs.marginTop) + px(cs.marginBottom);
    }
    const zs = getComputedStyle(zone);
    const extra = px(zs.borderTopWidth) + px(zs.borderBottomWidth) + px(zs.paddingTop)
        + px(zs.paddingBottom) + px(zs.marginTop) + px(zs.marginBottom);
    const hs = getComputedStyle(host);
    const avail = host.clientHeight - px(hs.paddingTop) - px(hs.paddingBottom) - others;
    const rows = rowsFor(avail, { rowH, gap, pad, extra });
    if (host.style.getPropertyValue('--pl-visible-rows') !== String(rows)) {
        host.style.setProperty('--pl-visible-rows', String(rows));
        revolverResync();
    }
}

// ── the library ───────────────────────────────────────────────────────

function mountLibrary() {
    if (libState === 'ok') { try { lib?.refresh?.(); } catch (_) {} return; }
    if (libState === 'loading') return;
    libState = 'loading';
    const side = $('lsSide'), centre = $('lsCentre');
    import('./medialib/index.js').then(m => {
        side.textContent = ''; centre.textContent = '';
        m.mountLibrarySide?.(side);
        lib = m.mountMediaLibrary?.(centre, { layout: 'wide' }) || null;
        // "Show in Files" from the library's playlists: the queue shows it.
        m.onMediaLibrary?.(ev => { if (ev?.type === 'files' && ev.layout === 'wide' && on) setTab('queue'); });
        libState = 'ok';
    }).catch(e => {
        libState = 'none';   // tried again the next time the window turns
        console.warn('[landscape] media library:', e?.message || e);
        side.innerHTML = '<div class="ls-ph-side">LIBRARY</div>';
        centre.innerHTML = `<div class="ls-ph">${TEXTS.libMissing}</div>`;
    });
}

// ── the window ────────────────────────────────────────────────────────

async function scale() { try { return await appWindow.scaleFactor(); } catch (_) { return 1; } }
async function where() {
    try {
        const p = await appWindow.outerPosition();
        const s = await scale();
        return { x: Math.round(p.x / s), y: Math.round(p.y / s) };
    } catch (_) { return null; }
}
async function area() {
    try { return logicalArea(await TW.currentMonitor()); } catch (_) { return null; }
}
const tryDo = async f => { try { await f(); } catch (_) {} };

async function placeWide() {
    if (!appWindow || !DPI) return;
    const a = await area();
    const r = fitLandscape(saved, saved.compactPos, a);
    await tryDo(() => appWindow.setResizable(true));
    await tryDo(() => appWindow.setMinSize(new DPI.LogicalSize(Math.min(MIN_W, r.w), Math.min(MIN_H, r.h))));
    await tryDo(() => appWindow.setPosition(new DPI.LogicalPosition(r.x, r.y)));
    await tryDo(() => appWindow.setSize(new DPI.LogicalSize(r.w, r.h)));
    if (saved.max) await tryDo(() => appWindow.maximize());
    syncMax();
}

async function placeCompact() {
    if (!appWindow || !DPI) return;
    await tryDo(async () => { if (await appWindow.isFullscreen()) await appWindow.setFullscreen(false); });
    await tryDo(async () => { if (await appWindow.isMaximized()) await appWindow.unmaximize(); });
    const at = await where();
    const a = await area();
    await tryDo(() => appWindow.setMinSize(null));
    await tryDo(() => appWindow.setSize(new DPI.LogicalSize(COMPACT_W, Math.min(window.innerHeight, (a?.h || 900) - 20))));
    await tryDo(() => appWindow.setResizable(false));
    const p = compactPlace(saved.compactPos, at, a);
    if (p) await tryDo(() => appWindow.setPosition(new DPI.LogicalPosition(p.x, p.y)));
}

/// Keeps the landscape window's size, place and maximized state as the
/// listener leaves them (a moment after the last change).
let keepTimer = null;
function keepSoon() {
    if (!on || busy) return;
    clearTimeout(keepTimer);
    keepTimer = setTimeout(keep, 400);
}
async function keep() {
    if (!on || busy || !appWindow) return;
    let max = false, full = false;
    try { max = await appWindow.isMaximized(); } catch (_) {}
    try { full = await appWindow.isFullscreen(); } catch (_) {}
    if (!on || busy) return;
    if (!full) saved.max = max;
    if (!max && !full) {
        saved.size = { w: Math.round(window.innerWidth), h: Math.round(window.innerHeight) };
        const p = await where();
        if (p) saved.pos = p;
    }
    store();
    syncMax(max || full);
}

async function syncMax(max) {
    if (max === undefined) { try { max = await appWindow.isMaximized(); } catch (_) { max = false; } }
    document.body.classList.toggle('ls-max', !!max);
}

async function toggleMax() {
    if (!on || !appWindow) return;
    await tryDo(() => appWindow.toggleMaximize());
    keepSoon();
}

async function toggleFull() {
    if (!on || !appWindow) return;
    await tryDo(async () => appWindow.setFullscreen(!(await appWindow.isFullscreen())));
    keepSoon();
}

// ── the switch ────────────────────────────────────────────────────────

function syncButton() {
    const b = $('winBtnOrient');
    if (!b) return;
    // Both looks are named in it (COMPACT | LIBRARY), the one shown lit by
    // the stylesheet (landscape.css): only the tip follows.
    b.title = on ? TEXTS.toCompactTip : TEXTS.toWideTip;
}

/// The list showed the library (M) before the window went wide.
let mBefore = false;

async function goWide(remember = true) {
    if (on) return;
    // The rack lives in Sound here: listening mode (the rack folded away) is
    // the compact window's. Not kept: it comes back with the compact window.
    if (isListening()) setListening(false, false, false);
    // The library has the middle here: the queue shows the files, not a
    // second library. Not kept: M comes back with the compact window.
    mBefore = listView() === 'library';
    if (mBefore) setListView('files', { remember: false });
    if (!moveIn()) return;
    busy = true;
    on = true;
    if (remember) { saved.mode = 'landscape'; }
    const p = await where();
    if (p) saved.compactPos = p;
    store();
    document.body.classList.add(CLS);
    wireCentrePages();
    watchQueueRows();
    syncButton();
    setTab(saved.tab);
    mountLibrary();
    try { await placeWide(); } finally { busy = false; }
    requestAnimationFrame(sizeQueue);
}

async function goCompact(remember = true) {
    if (!on) return;
    clearTimeout(keepTimer);
    await keep();
    busy = true;
    on = false;
    if (remember) saved.mode = 'compact';
    store();
    clearTimeout(keepTimer);
    try {
        closeCentrePage();
        moveOut();
        document.body.classList.remove(CLS, 'ls-max');
        syncButton();
        await placeCompact();
    } finally { busy = false; }
    // Listening mode as the listener left it in the compact window.
    let wasListening = false;
    try { wasListening = localStorage.getItem('auraListeningMode') === '1'; } catch (_) {}
    if (wasListening && !isListening()) setListening(true, false, false);
    if (mBefore && listView() === 'files') setListView('library', { remember: false });
    mBefore = false;
    revolverResync();
    fitCompact();
}

export const setLandscape = (wide, remember = true) => run(() => (wide ? goWide(remember) : goCompact(remember)));
export const toggleLandscape = () => setLandscape(!on);

/// Nothing else is on top that the key would go behind.
function keyBlocked() {
    const b = document.body.classList;
    if (b.contains('fullview') || b.contains('tour-up') || b.contains('lm-anim')) return true;
    const up = $('upModal');
    return !!(up && !up.classList.contains('up-hidden'));
}

export function initLandscape({ fit } = {}) {
    if (typeof fit === 'function') fitCompact = fit;
    const btn = $('winBtnOrient');
    btn?.addEventListener('click', e => {
        if (e.detail > 0) btn.blur();
        // A press on the half already lit: the window is that look already.
        const seg = e.target.closest?.('.wo-seg');
        if (seg && seg.classList.contains(on ? 'wo-l' : 'wo-c')) return;
        toggleLandscape();
    });
    $('winBtnMax')?.addEventListener('click', () => toggleMax());
    syncButton();

    window.addEventListener('keydown', e => {
        if (isSwitchKey(e)) {
            e.preventDefault();
            if (!keyBlocked()) toggleLandscape();
            return;
        }
        // The plain L folds the rack in the compact window; here the rack
        // is a tab, and the key is nobody's.
        if (on && isListeningKey(e)) { e.stopImmediatePropagation(); return; }
        if (on && e.key === 'F11') { e.preventDefault(); toggleFull(); }
    }, true);

    // The head: a double press maximizes, as a title bar's does.
    document.addEventListener('dblclick', e => {
        if (!on || e.target.closest('button, input, select, a')) return;
        if (e.target.closest('#converterPanel > .app-head-bar, #converterPanel > h2')) toggleMax();
    });

    window.addEventListener('resize', () => { keepSoon(); if (on) requestAnimationFrame(sizeQueue); });
    try { appWindow?.onMoved?.(() => keepSoon())?.catch?.(() => {}); } catch (_) {}

    // The tour shows the compact window (its steps are the compact window's
    // places, and one of them is listening mode): the landscape steps aside
    // and comes back after it. Listening mode turned on any other way does
    // the same, without coming back.
    // The switch's own tour is about this look: it is shown in it.
    const tourHere = () => $('tourLayer')?.dataset.tour === 'switch';
    new MutationObserver(() => {
        const b = document.body.classList;
        if (on && !busy && ((b.contains('tour-up') && !tourHere()) || b.contains('mode-listening'))) {
            forTour = b.contains('tour-up');
            setLandscape(false, false);
        } else if (!on && forTour && !b.contains('tour-up')) {
            forTour = false;
            setLandscape(true, false);
        }
    }).observe(document.body, { attributes: true, attributeFilter: ['class'] });

    // The intro: a fresh profile, or one from before the window opened in
    // Library, opens in Library once; kept as the listener's own choice.
    let intro = false;
    try { intro = introDue(localStorage.getItem(INTRO_KEY)); } catch (_) {}
    if (intro) {
        saved.mode = 'landscape';
        store();
        try { localStorage.setItem(INTRO_KEY, introRecord()); } catch (_) {}
    }
    if (saved.mode === 'landscape') setLandscape(true);
}
