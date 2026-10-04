// analytics.js — Main-window side of the Analyzer feature.
//
// SPEC §2.3, §9.1-9.5, FRONTEND-CONTRACT §9.
// Injects:
//   • [≈] button + 3-number mini-strip into #plBar
//   • Right-click context menu on list rows (via event delegation)
//   • Ctrl+Shift+A global shortcut → LIVE window
//
// This file must be importable without side-effects (all DOM work inside
// DOMContentLoaded). It does NOT import from player.js, dsprack.js or any
// other existing module (except state.js and library.js per scout-integration.md).
//
// main.js adds exactly ONE line:  import './js/analytics.js';

import { state }    from './state.js';
import { entrySig } from './library.js';
import { addPopup } from './popups.js';

// ── Live store reference (set once the LIVE window reports back) ──────────
// analytics.js listens for a Tauri event from the live analyzer window that
// broadcasts its mini-strip metrics (LUFS-I O, LRA O, TP O) at 10 Hz.
// The mini-strip updates only while the live window is open.
const miniState = { lufsI: null, lra: null, tp: null, active: false };

// ── Window open / focus ────────────────────────────────────────────────────
// Where an analyzer window opens — place, size, maximized or on the whole
// screen — is the backend's: it puts the window where the analyzer was last
// (window_state.rs).
const seqMap = new Map(); // entryId -> seq counter
const { invoke } = window.__TAURI__.tauri;

async function openOrFocusLive() {
  const label  = 'analyzer-live';
  const params = new URLSearchParams({ mode: 'live' });
  await invoke('analyzer_open', { label, url: `analytics.html?${params}` });
}

// mode: 'source' (the file as it is) | 'output' (its converted copy)
async function openFileWindow(entryId, mode) {
  const entry = state.list.find(e => e.id === entryId);
  if (!entry) return;

  // Resolve the label
  const seq   = (seqMap.get(entryId) || 0) + 1;
  seqMap.set(entryId, seq);
  const label = `analyzer-file-${entryId}-${seq}`;

  // Resolve chain hex
  let chainHex = '00000000';
  try { chainHex = (entrySig(entry) >>> 0).toString(16).padStart(8, '0'); } catch {}

  // Resolve output path (for 'output' mode)
  const convPath = entry.convs?.find(c => c.sig === entrySig(entry))?.out || entry.convs?.at(-1)?.out || null;
  if (mode === 'output' && !convPath) return; // greyed item; should not reach here

  const params = new URLSearchParams({ mode: 'file', entry_id: String(entryId), seq: String(seq), chain: chainHex });
  // The file itself, and the player's id for it: while that track plays,
  // the window's bar follows and moves it.
  if (entry.path) params.set('path', entry.path);
  if (entry.trackId != null) params.set('track', String(entry.trackId));
  if (mode === 'output' && convPath) params.set('conv', encodeURIComponent(convPath));

  await invoke('analyzer_open', { label, url: `analytics.html?${params}` });
}

// ── Context menu (popover) ─────────────────────────────────────────────────
let ctxMenu = null;

function showContextMenu(x, y, entry, playing) {
  hideContextMenu();

  const convPath = entry.convs?.find(c => c.sig === entrySig(entry))?.out || entry.convs?.at(-1)?.out || null;
  const hasConv  = !!convPath;

  const menu = document.createElement('div');
  menu.className  = 'an-ctx-menu';
  menu.style.left = `${x}px`;
  menu.style.top  = `${y}px`;

  // The track playing opens the live analyzer (what is heard, through the
  // rack); any file can be measured as it is, and its converted copy.
  const items = [];
  if (playing) {
    items.push({ label: 'Live analyzer — what you hear now', act: 'live', disabled: false,
      tip: 'The source, the rack and the output of this track, as it plays.' });
  }
  items.push({ label: 'Analyze this file', act: 'source', disabled: false,
    tip: 'The file as it is, measured whole.' });
  items.push({ label: 'Analyze converted file', act: 'output', disabled: !hasConv,
    tip: hasConv ? 'The converted copy, measured whole.' : 'Convert first to measure output.' });

  for (const item of items) {
    const btn = document.createElement('button');
    btn.className = 'an-ctx-item' + (item.disabled ? ' an-ctx-item--disabled' : '');
    btn.textContent = item.label;
    if (item.tip) btn.setAttribute('data-tip', item.tip);
    if (item.disabled) {
      btn.disabled = true;
      btn.setAttribute('aria-disabled', 'true');
    } else {
      btn.addEventListener('click', async () => {
        hideContextMenu();
        if (item.act === 'live') await openOrFocusLive();
        else await openFileWindow(entry.id, item.act);
      });
    }
    menu.appendChild(btn);
  }

  document.body.appendChild(menu);
  ctxMenu = menu;

  // Keep it inside the window: measured now and again on the next frame,
  // once its sheet and fonts have settled.
  const place = () => {
    const rect = menu.getBoundingClientRect();
    const left = x + rect.width  > window.innerWidth  ? x - rect.width  : x;
    const top  = y + rect.height > window.innerHeight ? y - rect.height : y;
    menu.style.left = `${Math.max(4, Math.min(left, window.innerWidth  - rect.width  - 4))}px`;
    menu.style.top  = `${Math.max(4, Math.min(top,  window.innerHeight - rect.height - 4))}px`;
  };
  place();
  requestAnimationFrame(place);
  setTimeout(place, 50);
}

function hideContextMenu() {
  if (ctxMenu) { ctxMenu.remove(); ctxMenu = null; }
}

// ── Mini-strip formatting ──────────────────────────────────────────────────
function fmtMini(v) {
  if (v === null || !isFinite(v)) return '—'; // em-dash
  return v.toFixed(2);
}

// ── DOM injection ──────────────────────────────────────────────────────────
function injectPlayerBar() {
  // COORDINATION: append .an-open-btn + .an-mini at the END of .pl-dev as normal
  // flex items; no position:absolute; no assumption about single row in .pl-dev.
  const plBar = document.querySelector('#plBar');
  if (!plBar) return;

  // Already injected (idempotent guard)
  if (plBar.querySelector('.an-open-btn')) return;

  // [≈] button — waveform-analyzer SVG icon
  const btn = document.createElement('button');
  btn.id        = 'anBtn';
  btn.className = 'an-open-btn';  // COORDINATION: class is an-open-btn
  btn.setAttribute('data-tip', 'Open Analyzer');
  // Two traces, as the analyzer draws the source and the output.
  btn.innerHTML = `<svg viewBox="0 0 16 16" fill="none" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" width="16" height="16">
    <polyline class="an-open-t1" points="1,12 4,4 6,10 8,6 10,9 12,3 15,8"/>
    <polyline class="an-open-t2" points="1,13.5 4,7 6,12 8,9 10,11.5 12,6.5 15,10.5"/>
  </svg>`;
  btn.addEventListener('click', openOrFocusLive);

  // Mini-strip: 3 spans — LUFS-I O, LRA O, TP O
  const strip = document.createElement('span');
  strip.id        = 'anMini';
  // COORDINATION: class is an-mini. an-mini-wait keeps it out of the row until
  // the live analyzer first sends numbers (analyzer:mini): an invisible readout
  // took the slider's room and could wrap the device row.
  strip.className = 'an-mini an-mini-wait';
  strip.style.opacity = '0'; // hidden until live subject active, no layout shift

  const mkNum = (id) => {
    const s = document.createElement('span');
    s.className     = 'an-mini-num';
    s.dataset.anNum = id;
    s.textContent   = '—';
    return s;
  };

  strip.appendChild(mkNum('lufs'));
  strip.appendChild(mkNum('lra'));
  strip.appendChild(mkNum('tp'));

  // COORDINATION: append to END of .pl-dev as normal flex items (no position:absolute)
  const plDev = plBar.querySelector('.pl-dev');
  if (plDev) {
    plDev.appendChild(btn);
    plDev.appendChild(strip);
  } else {
    plBar.appendChild(btn);
    plBar.appendChild(strip);
  }

  // Register tooltip on injected elements (the existing initTip covers the DOM
  // at boot; injected elements need an explicit call).
  if (typeof window.initTip === 'function') {
    window.initTip(btn);
    window.initTip(strip);
  }
}

let _listDelegationDone = false;
function injectListDelegation() {
  if (_listDelegationDone) return;
  const fq = document.getElementById('fileQueue');
  if (!fq) return;
  _listDelegationDone = true;

  // ── Context menu (delegation — COORDINATION: closest('.pl-item[data-id]')) ──
  fq.addEventListener('contextmenu', ev => {
    ev.preventDefault();
    const row = ev.target.closest('.pl-item[data-id]');
    if (!row) return;
    const entryId = parseInt(row.dataset.id, 10);
    const entry   = state.list.find(e => e.id === entryId);
    if (!entry) return;
    showContextMenu(ev.clientX, ev.clientY, entry, row.classList.contains('pl-now'));
  });

  // No floating Analyze button on the rows: it sat over the row's own
  // buttons (the delete among them) and caught the right-click meant for
  // the row. The menu above and the A key cover it.

  // ── 'A' key: the row with the focus, else the row under the pointer ──────
  // (It answered only with the focus on one of a row's buttons, which a
  // row rarely has; and by the letter, so not in a Russian layout.)
  let hoverRow = null;
  fq.addEventListener('mouseover', ev => { hoverRow = ev.target.closest('.pl-item[data-id]'); });
  fq.addEventListener('mouseleave', () => { hoverRow = null; });
  document.addEventListener('keydown', ev => {
    if (ev.code !== 'KeyA' || ev.ctrlKey || ev.altKey || ev.metaKey || ev.shiftKey || ev.repeat) return;
    const t = ev.target;
    if (t && t.nodeType === 1) {
      // As the L key (listening.js): not while typing, a dialog or a menu is up.
      if (t.isContentEditable || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.tagName === 'OPTION') return;
      if (t.tagName === 'INPUT' && !/^(range|checkbox|radio|button|submit|reset|color)$/i.test(t.type || 'text')) return;
    }
    const up = document.getElementById('upModal');
    if (up && !up.classList.contains('up-hidden')) return;
    const menu = document.getElementById('labMenu');
    if (menu && menu.style.display === 'block') return;
    const row = (t && t.closest && t.closest('.pl-item[data-id]')) || (hoverRow && hoverRow.isConnected ? hoverRow : null);
    if (!row) return;
    const entryId = parseInt(row.dataset.id, 10);
    if (isNaN(entryId)) return;
    ev.preventDefault();
    openFileWindow(entryId, 'source');
  });
}

// ── Mini-strip live data subscription ────────────────────────────────────
// The LIVE analyzer window emits 'analyzer:mini' every 100 ms with {lufsI, lra, tp}.
function subscribeToLiveMetrics() {
  if (!window.__TAURI__?.event) return;
  window.__TAURI__.event.listen('analyzer:mini', ev => {
    // The first numbers bring the readout into the row; it keeps its place then.
    document.getElementById('anMini')?.classList.remove('an-mini-wait');
    const { lufsI, lra, tp, active } = ev.payload || {};
    miniState.lufsI  = lufsI ?? null;
    miniState.lra    = lra   ?? null;
    miniState.tp     = tp    ?? null;
    miniState.active = !!active;
    updateMiniStrip();
  });
  // Also update on live-window close
  window.__TAURI__.event.listen('analyzer:live-closed', () => {
    miniState.active = false;
    updateMiniStrip();
  });
}

function updateMiniStrip() {
  const strip = document.getElementById('anMini');
  if (!strip) return;
  strip.style.opacity = miniState.active ? '1' : '0';
  if (!miniState.active) return;

  const set = (id, v) => {
    const el = strip.querySelector(`[data-an-num="${id}"]`);
    if (el) el.textContent = fmtMini(v);
  };
  set('lufs', miniState.lufsI);
  set('lra',  miniState.lra);
  set('tp',   miniState.tp);
}

// ── Global keyboard shortcut ──────────────────────────────────────────────
function installKeyboardShortcut() {
  document.addEventListener('keydown', ev => {
    // Ctrl+Shift+A (global) → LIVE window (by the key's place: any layout)
    if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey && ev.code === 'KeyA') {
      ev.preventDefault();
      openOrFocusLive();
    }
  });
}

// ── Close menu on a press outside / ESC ───────────────────────────────────
// That press goes nowhere else — the right button's on another row too
// (popups.js).
addPopup({ isOpen: () => !!ctxMenu, inside: t => !!ctxMenu?.contains(t), close: hideContextMenu });

// ── Boot ───────────────────────────────────────────────────────────────────
// main.js loads the converter component async after DOMContentLoaded, so
// #plBar does not exist at DOMContentLoaded time.  We handle two paths:
//   1. boot() at DOMContentLoaded installs keyboard shortcuts, delegations and
//      the metrics subscription.  injectPlayerBar() is a no-op if #plBar isn't
//      there yet.
//   2. main.js dispatches 'aura:player-ready' after initPlayer() creates #plBar.
//      We then inject the bar (idempotent: if it already ran, anMini already
//      exists and injectPlayerBar() would skip the second injection).

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', boot);
} else {
  boot();
}
// Retry injections once the player components are in the DOM (async init).
// #fileQueue and #plBar both exist by the time aura:player-ready fires.
document.addEventListener('aura:player-ready', () => {
  if (!document.querySelector('.an-open-btn')) injectPlayerBar();
  injectListDelegation(); // idempotent; creates floatBtn if #fileQueue exists
}, { once: true });

function boot() {
  injectPlayerBar();
  injectListDelegation();
  installKeyboardShortcut();
  subscribeToLiveMetrics();
}

// ── Exports (for use by window.js in the analyzer window) ─────────────────
export { openOrFocusLive, openFileWindow };
