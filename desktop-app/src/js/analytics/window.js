/**
 * window.js — Per-window bootstrap for the AuraEngine Analyzer.
 *
 * Runs inside analytics.html.  Owns:
 *   - an_subject_open / an_subject_close lifecycle
 *   - The three polling loops (live / track / resp) via scheduler.js
 *   - The store (reactive state per FRONTEND-CONTRACT.md §2)
 *   - The event bus (EventTarget; all views in this window share it)
 *   - Title bar wiring (drag / minimize / close)
 *   - Chain strip rendering
 *   - Toolbar wiring
 *   - Panel collapse/expand
 *   - Status bar
 *   - Metrics table instantiation
 *   - HP banner
 *
 * Exported: bus (EventTarget), store (object)
 *
 * Note: view modules (spectrum-view, loudness-view, etc.) are imported
 * by this file; they are written by other agents but called via the
 * view lifecycle contract (create/setData/setLayer/onResize/drawOverlay/destroy).
 */

// First: a press outside an open popup or list is stopped before anything
// else of the window hears it (../popups.js).
import '../popups.js';
import { decodeAAN1, decodeAAN2, decodeAAN3, PROV, N_METRICS, STREAM_TEXTS, bandwidthText, streamTechText } from './protocol.js';
import { formatTimeInt, formatHz } from './format.js';
import { createScheduler } from './scheduler.js';
import { createMetricsTable } from './metrics-table.js';
import { createSongsView } from './songs-view.js';
import { create as createCursor } from './cursor.js';
import { createTileCache } from './tile-cache.js';
import { createSelection } from './selection.js';
import { create as createSnapshot } from './snapshot.js';
import { createLayerStrip } from './layers.js';
import { createExportPopover, flashEl } from './export.js';
import { create as createSpectrumView, LAYER_REGISTRY as SPECTRUM_LAYERS } from './spectrum-view.js';
import { create as createLoudnessView, LAYER_REGISTRY as LOUDNESS_LAYERS } from './loudness-view.js';
import { create as createSpectrogramView, LAYER_REGISTRY as SPECTROGRAM_LAYERS } from './spectrogram-view.js';
import { create as createWaveformView, LAYER_REGISTRY as WAVEFORM_LAYERS } from './waveform-view.js';
import { create as createHistogramView, LAYER_REGISTRY as HISTOGRAM_LAYERS } from './histogram-view.js';
import { create as createStereoView, LAYER_REGISTRY as STEREO_LAYERS } from './stereo-view.js';
import { createTipTiming } from '../tip-timing.js';
import { placeTip } from '../tip-place.js';
import { analysisStages, overallProgress, percentText, placeBadge, progressTip, statusText } from './progress.js';
import { windowTitle, titleParts, titleMarkup, fileName } from './title.js';

// ── Event bus (shared across all modules in this window) ─────────────────────

export const bus = new EventTarget();

// ── Tooltip system ────────────────────────────────────────────────────────────

// When a tooltip shows is the main window's rule (tip-timing.js): 1.5 s
// cold, at once while one is up and for 1 s after it went away.
let _tip        = null;
let _tipTimer   = null;
let _tipFor     = null;   // the element whose tooltip is up
const _timing   = createTipTiming();

function _initTooltip() {
  _tip = document.getElementById('labTip');
  if (!_tip) return;

  const show = (el) => {
    const hint = _tip.querySelector('.hint') || _tip;
    hint.textContent = el.dataset.tip;
    _placeTip(el);
    _tip.classList.add('visible');
    _tipFor = el;
    _timing.shown();
  };

  document.addEventListener('mouseover', (ev) => {
    const el = ev.target.closest('[data-tip]');
    if (!el) { clearTimeout(_tipTimer); return; }
    if (el === _tipFor) return;   // a child of the element whose tooltip is up
    clearTimeout(_tipTimer);
    const delay = _timing.delay();
    if (delay === 0) show(el);
    else _tipTimer = setTimeout(() => show(el), delay);
  });

  document.addEventListener('mouseout', (ev) => {
    const el = ev.target.closest('[data-tip]');
    if (!el) return;
    if (ev.relatedTarget && el.contains(ev.relatedTarget)) return;
    _hideTip();
  });

  // A click or a key is an action: the tooltip goes, and the next waits again.
  document.addEventListener('click', () => _hideTip(true));
  document.addEventListener('keydown', () => _hideTip(true));
  window.addEventListener('blur', () => _hideTip());
}

/** The tooltip over `el`, or under it when there is no room above (an
 *  element near the top, a tip of several lines); whole inside the window
 *  either way (a metric's name at the left edge used to get half a tip). */
function _placeTip(el) {
  const p = placeTip(_tip, el.getBoundingClientRect(), { side: 'above' });
  _tip.classList.toggle('below', p.below);
}

/** The tooltip of `el` is up: show its text again (it changed). */
function _refreshTip(el) {
  if (_tip && _tipFor === el) {
    (_tip.querySelector('.hint') || _tip).textContent = el.dataset.tip;
    _placeTip(el);
  }
}

function _hideTip(cold = false) {
  clearTimeout(_tipTimer);
  if (_tipFor || cold) _timing.gone(cold);
  _tipFor = null;
  if (_tip) _tip.classList.remove('visible');
}

// ── Store ─────────────────────────────────────────────────────────────────────

/**
 * Create a minimal reactive store.
 * @param {object} initial
 */
function createStore(initial) {
  const _listeners = new Map(); // key → Set<fn>
  const _state     = { ...initial };

  const _api = {
    get(key)     { return _state[key]; },
    getAll()     { return { ..._state }; },

    set(partialOrKey, value) {
      if (typeof partialOrKey === 'string') {
        const old = _state[partialOrKey];
        _state[partialOrKey] = value;
        const fns = _listeners.get(partialOrKey);
        if (fns) for (const fn of fns) fn(value, old);
      } else {
        for (const [k, v] of Object.entries(partialOrKey)) {
          const old = _state[k];
          _state[k] = v;
          const fns = _listeners.get(k);
          if (fns) for (const fn of fns) fn(v, old);
        }
      }
    },

    setDeep(path, value) {
      const parts = path.split('.');
      let obj = _state;
      for (let i = 0; i < parts.length - 1; i++) {
        if (obj[parts[i]] === undefined) obj[parts[i]] = {};
        obj = obj[parts[i]];
      }
      const key = parts[parts.length - 1];
      obj[key] = value;
    },

    on(key, fn) {
      if (!_listeners.has(key)) _listeners.set(key, new Set());
      _listeners.get(key).add(fn);
      return () => _listeners.get(key).delete(fn);
    },
  };

  // Allow view modules to access state via store.key (direct property access)
  // in addition to the canonical store.get('key') API.
  return new Proxy(_api, {
    get(target, prop, receiver) {
      if (prop in target) return Reflect.get(target, prop, receiver);
      return _state[prop];
    },
    set(target, prop, value) {
      if (prop in target) return Reflect.set(target, prop, value);
      // Treat direct assignment as store.set(key, value)
      const old = _state[prop];
      _state[prop] = value;
      const fns = _listeners.get(prop);
      if (fns) for (const fn of fns) fn(value, old);
      return true;
    },
  });
}

export const store = createStore({
  sid:       null,
  mode:      null,
  entryId:   null,
  seq:       null,
  chainHex:  null,

  chain: {
    rev:    0,
    tokens: '',
    flags:  0,
    marks:  [],
  },

  metrics: {
    s:          new Float64Array(26).fill(NaN),
    b:          new Float64Array(26).fill(NaN),
    o:          new Float64Array(26).fill(NaN),
    sProv:      new Uint8Array(26),
    bProv:      new Uint8Array(26),
    oProv:      new Uint8Array(26),
    coveragePct: 0,
    bReady:     false,
  },

  series: {
    lufs_s_s: null,
    lufs_m_s: null,
    lufs_s_b: null,
    lufs_m_b: null,
    lufs_s_o: [],
    lufs_m_o: [],
  },

  layers: {
    s_psd:    null,
    b_psd:    null,
    o_psd:    null,
    h_mag:    null,
    delta_bs: null,
    delta_ob: null,
  },

  snapshots: [],

  cursor: {
    freqHz: null,
    timeS:  null,
    values: {},
  },

  trackId:          0n,
  trackName:        '',
  durationS:        0,
  trackRev:         0,
  hpBannerVisible:  false,
});

// ── Parse URL params ──────────────────────────────────────────────────────────

function _parseParams() {
  const p = new URLSearchParams(location.search);
  return {
    mode:    p.get('mode') || 'live',
    entryId: p.get('entry_id') || null,
    seq:     p.get('seq')      || null,
    chain:   p.get('chain')    || null,
    conv:    p.has('conv') ? decodeURIComponent(p.get('conv')) : null,
    path:    p.get('path')     || null,
    track:   p.has('track') ? Number(p.get('track')) : null,
  };
}

// ── Title bar ─────────────────────────────────────────────────────────────────

function _initTitleBar() {
  const bar = document.getElementById('anTitleBar');
  if (!bar) return;

  // Maximize / restore: the button, or a double click on the bar.
  const toggleMax = () => {
    try { window.__TAURI__.window.appWindow.toggleMaximize(); } catch (_) { }
  };

  // Drag region. The second press of a double click maximizes instead:
  // once dragging starts, the page never sees a dblclick.
  bar.addEventListener('mousedown', (ev) => {
    if (ev.button !== 0 || ev.target.closest('button')) return;
    if (ev.detail >= 2) { toggleMax(); return; }
    try { window.__TAURI__.window.appWindow.startDragging(); } catch (_) { /* non-Tauri */ }
  });

  // Minimize button
  const minBtn = document.getElementById('anMinBtn');
  if (minBtn) {
    minBtn.addEventListener('click', () => {
      try { window.__TAURI__.window.appWindow.minimize(); } catch (_) { }
    });
  }

  document.getElementById('anMaxBtn')?.addEventListener('click', toggleMax);

  // Close button
  const closeBtn = document.getElementById('anCloseBtn');
  if (closeBtn) {
    closeBtn.addEventListener('click', async () => {
      // Its place, size and state come back next time (window_state.rs);
      // Tauri 1's close() gives the backend no CloseRequested, so ask first.
      try { await window.__TAURI__.tauri.invoke('window_keep_place'); } catch (_) { }
      try { window.__TAURI__.window.appWindow.close(); } catch (_) { }
    });
  }
}

// The live window's seek bar: where the track heard is, and a drag moves
// it — the player's own seek, as the main window's bar. A file window has
// no player to move and keeps the plain readout.
// ── A stream (the radio) ─────────────────────────────────────────────────────
// The station and the song in the window's title, the stream's own line
// (codec, rate) and its true bandwidth above the numbers, and the session's
// and the songs' totals (stream_totals.rs) once a second. radio.js names the
// station as the main window does; without it, the stream's own name.
let _radioNames = null;
import('../radio.js').then(m => { _radioNames = m; }).catch(() => {});
let _radioKey = null;
let _totalsTick = 0;
let _totalsBusy = false;

function _hostOf(url) {
  try { return new URL(url).host; } catch (_) { return String(url || ''); }
}

/** The stream's totals (`reset`: ↺ first, the session afresh). */
async function _fetchTotals(reset) {
  const sid = store.get('sid');
  if (sid == null || (_totalsBusy && !reset)) return;
  _totalsBusy = true;
  try {
    const r = await fetch(`https://aura.localhost/player/an/stream?sid=${sid}${reset ? '&reset=1' : ''}`, { cache: 'no-store' });
    if (r.ok) store.set('_streamTotals', await r.json());
  } catch (_) { /* the next second asks again */ }
  _totalsBusy = false;
  _renderStreamLine();
}

/** Each status poll: the stream heard (its status), or null when none is. */
function _noteStream(s) {
  if (!s) {
    if (_radioKey !== null) {
      _radioKey = null;
      store.set('_radioNow', null);
      // The stream's live curves go: what plays next starts its own (its
      // points would land among the station's, on the same seconds).
      for (const k of ['lufs_s_o', 'lufs_m_o', 'lufs_s_s_live', 'lufs_m_s_live']) store.setDeep(`series.${k}`, []);
      _renderStreamLine();
    }
    return;
  }
  const r = s.radio, info = r.info || {};
  const station = _radioNames?.stationName?.(r) || String(info.icyName || '').trim() || _hostOf(info.url);
  const title = r.now?.text || '';
  const tech = streamTechText(info, _radioNames?.codecName);
  const key = `${station}\n${title}\n${tech}`;
  if (key !== _radioKey) {
    // No whole file to analyse: the progress line has nothing to wait for.
    if (_radioKey === null) document.getElementById('anProgress')?.classList.add('an-progress--idle');
    // Another stream (its clock starts again): the live curves start afresh.
    if (_radioKey === null || !_radioKey.startsWith(`${station}\n`)) {
      for (const k of ['lufs_s_o', 'lufs_m_o', 'lufs_s_s_live', 'lufs_m_s_live']) store.setDeep(`series.${k}`, []);
    }
    _radioKey = key;
    store.set('_radioNow', { station, title, tech });
    _renderStreamLine();
  }
  if (_totalsTick++ % 4 === 0) _fetchTotals(false);
}

/** The live window's title, from each status poll: what plays — a stream's
 *  station and song, else the file's name (`s.trackFile`) — set the moment it
 *  changes, so with several windows open it is clear whose this one is. */
let _liveTitle = null;
function _noteLiveTitle(s) {
  const now = store.get('_radioNow');
  const w = {
    mode: 'live',
    stream: now ? { station: now.station, song: now.title } : null,
    file: s?.trackFile || null,
  };
  const t = windowTitle(w);
  if (t !== _liveTitle) {
    _liveTitle = t;
    _setWindowTitle(w);
    // The exports name what was heard too (they said "Unknown Track").
    store.set('trackName', now ? [now.station, now.title].filter(Boolean).join(' · ') : (s?.trackFile || ''));
  }
}

/** The stream's line above the numbers: what it says it is, and how far its
 *  spectrum really reaches. Hidden off a stream. */
function _renderStreamLine() {
  const sec = document.getElementById('anMetricsSection');
  if (!sec) return;
  let el = document.getElementById('anStreamLine');
  const now = store.get('_radioNow');
  if (!now) { if (el) el.hidden = true; return; }
  if (!el) {
    el = document.createElement('div');
    el.id = 'anStreamLine';
    el.className = 'an-stream-line';
    // Each part keeps to one line (analytics.css); the space between them,
    // the line's own, is where a narrow column breaks it.
    el.innerHTML = '<span class="an-stream-tech"></span> <span class="an-stream-bw"></span>';
    sec.insertBefore(el, document.getElementById('anMetricsTable'));
  }
  el.hidden = false;
  el.querySelector('.an-stream-tech').textContent = now.tech ? `${now.tech} ·` : '';
  const bw = el.querySelector('.an-stream-bw');
  bw.textContent = bandwidthText(store.get('_streamTotals')?.session?.bw);
  bw.setAttribute('data-tip', STREAM_TEXTS.bwTip);
}

function _initLiveScrub(follows, trackId) {
  const wrap = document.getElementById('anPlayheadWrap');
  const text = document.getElementById('anDurationText');
  if (!wrap || !text) return;
  _liveScrub = true;
  const playBtn = _initPlayButton(wrap, trackId);
  wrap.classList.add('an-scrub');
  wrap.setAttribute('data-tip', 'Drag or click to move the track');
  const thumb = document.createElement('span');
  thumb.className = 'an-scrub-thumb';
  wrap.appendChild(thumb);
  // After a seek, until the jump is heard: a breathing band from where the
  // sound is to the target, a dot at the target (as in the main player).
  const band = document.createElement('span');
  band.className = 'an-scrub-band';
  wrap.appendChild(band);
  let pending = null;                  // { target, t0 } after a seek

  const showBand = (from, to) => {
    if (!(dur > 0)) { band.classList.remove('active'); return; }
    const a = Math.max(0, Math.min(1, from / dur)), b = Math.max(0, Math.min(1, to / dur));
    band.style.left = `${Math.min(a, b) * 100}%`;
    band.style.width = `${Math.max(0.4, Math.abs(b - a) * 100)}%`;
    band.classList.toggle('an-scrub-band--back', b < a);
    band.classList.add('active');
  };

  let pos = 0, dur = 0, drag = null;   // drag: 0..1 while the pointer holds it
  let stream = false;                  // a live stream: no length, only the time played
  const draw = () => {
    const p = drag != null ? drag : (dur > 0 ? Math.min(1, pos / dur) : 0);
    wrap.style.setProperty('--an-scrub', `${p * 100}%`);
    const t = drag != null ? drag * dur : pos;
    text.textContent = stream ? formatTimeInt(t) : `${formatTimeInt(t)} / ${formatTimeInt(dur)}`;
  };
  const poll = async () => {
    try {
      const r = await fetch('https://aura.localhost/player/status', { cache: 'no-store' });
      if (r.ok) {
        const s = await r.json();
        const on = follows(s) && s.state !== 'stopped';
        stream = on && !!s.radio && !s.radio.stopped;
        if (store.get('_stream') !== stream) store.set('_stream', stream);
        _noteStream(stream ? s : null);
        if (store.get('mode') === 'live') _noteLiveTitle(s);
        playBtn.show(follows(s) && s.state === 'playing');
        if (on) {
          // What the graphs need of the player: where the sound is, the rates.
          _posClock.poll(s.positionS || 0, s.state === 'playing');
          if (s.outRate) store.set('_outRate', s.outRate);
          if (s.srcRate) store.set('_srcRate', s.srcRate);
        } else {
          _posClock.stop();
        }
        pos = on ? (s.positionS || 0) : 0;
        dur = on ? (s.durationS || 0) : 0;
        wrap.classList.toggle('an-scrub--idle', !(dur > 0));
        if (on) draw();
        else if (!drag) text.textContent = `--:-- / ${formatTimeInt(store.get('durationS') || 0)}`;
        if (pending) {
          if (on && s.jump?.pending) {
            showBand(pos, s.jump.targetS ?? pending.target);
          } else if (Date.now() - pending.t0 > 700) {
            // The jump is heard (or never came): the band goes.
            pending = null;
            band.classList.remove('active');
          }
        }
      }
    } catch (_) { /* the next poll tries again */ }
    setTimeout(poll, 250);
  };
  poll();

  const at = (ev) => {
    const r = wrap.getBoundingClientRect();
    return Math.max(0, Math.min(1, (ev.clientX - r.left) / Math.max(1, r.width)));
  };
  wrap.addEventListener('pointerdown', (ev) => {
    if (ev.button !== 0 || !(dur > 0)) return;
    ev.preventDefault();
    wrap.setPointerCapture(ev.pointerId);
    wrap.classList.add('an-scrub--drag');
    drag = at(ev);
    draw();
  });
  wrap.addEventListener('pointermove', (ev) => {
    if (drag == null) return;
    drag = at(ev);
    draw();
  });
  const release = (ev, commit) => {
    if (drag == null) return;
    const target = drag * dur;
    drag = null;
    wrap.classList.remove('an-scrub--drag');
    if (commit) {
      pending = { target, t0: Date.now() };
      showBand(pos, target);
      // Through the main window, which sends a rack change still settling
      // first (player.js): the seek is made with the rack it shows. Sent
      // here, the rack went out while the seek was built, and the seek was
      // dropped as stale.
      const direct = () => { try { window.__TAURI__.tauri.invoke('player_seek', { seconds: target }); } catch (_) { } };
      try { window.__TAURI__.event.emit('player-transport', { action: 'seek', seconds: target }).catch(direct); } catch (_) { direct(); }
    }
    draw();
  };
  wrap.addEventListener('pointerup', (ev) => release(ev, true));
  wrap.addEventListener('pointercancel', (ev) => release(ev, false));
}

// Play/pause next to the seek bar (Anton 26.09), and Space does the same
// here. The main window does it as its own ▶/⏸ would (player.js listens
// for 'player-transport'): the live window toggles the player; a file
// window toggles its own track, and starts it when another one plays.
const SVG_AN_PLAY = '<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2.6 1.4v7.2L8.8 5z" fill="currentColor"/></svg>';
const SVG_AN_PAUSE = '<svg viewBox="0 0 10 10" aria-hidden="true"><rect x="2" y="1.5" width="2.3" height="7" rx="0.6" fill="currentColor"/>'
  + '<rect x="5.7" y="1.5" width="2.3" height="7" rx="0.6" fill="currentColor"/></svg>';

function _initPlayButton(wrap, trackId) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.id = 'anPlayBtn';
  wrap.after(btn);
  let shown = null;
  const show = (playing) => {
    if (playing === shown) return;
    shown = playing;
    btn.innerHTML = playing ? SVG_AN_PAUSE : SVG_AN_PLAY;
    btn.dataset.state = playing ? 'playing' : 'idle';
    btn.setAttribute('aria-label', playing ? 'Pause' : 'Play');
    btn.dataset.tip = playing ? 'Pause (Space)' : 'Play (Space)';
    _refreshTip(btn);
  };
  const toggle = () => {
    try { window.__TAURI__.event.emit('player-transport', { action: 'toggle', trackId }); } catch (_) { }
  };
  btn.addEventListener('click', (ev) => {
    // After a mouse click the focus goes back, so Space is not a second press.
    if (ev.detail > 0) btn.blur();
    toggle();
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key !== ' ' || ev.repeat || ev.ctrlKey || ev.altKey || ev.metaKey) return;
    if (ev.target.tagName === 'INPUT' || ev.target.tagName === 'TEXTAREA' || ev.target.tagName === 'SELECT') return;
    ev.preventDefault();
    toggle();
  });
  show(false);
  return { show };
}

// The splitter between the numbers and the graphs: drag to set the left
// column's width, remembered for the next window.
const LEFT_W_KEY = 'analyzer-left-w';

function _initSplitter() {
  const split = document.getElementById('anSplit');
  const body  = document.getElementById('anBody');
  if (!split || !body) return;
  const setW = (w) => body.style.setProperty('--an-left-w', `${Math.round(w)}px`);
  try {
    const saved = Number(localStorage.getItem(LEFT_W_KEY));
    if (saved > 0) setW(saved);
  } catch (_) { /* storage off: the default width */ }

  split.addEventListener('pointerdown', (ev) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    split.setPointerCapture(ev.pointerId);
    split.classList.add('an-split--drag');
    document.body.classList.add('an-resizing');
    const left0 = body.getBoundingClientRect().left;
    let w = 0;
    const move = (e) => {
      // Keep both sides usable: at least 280 px of numbers, 320 px of graphs.
      const max = body.getBoundingClientRect().width - 320;
      w = Math.max(280, Math.min(max, e.clientX - left0));
      setW(w);
    };
    const up = () => {
      split.removeEventListener('pointermove', move);
      split.removeEventListener('pointerup', up);
      split.removeEventListener('pointercancel', up);
      split.classList.remove('an-split--drag');
      document.body.classList.remove('an-resizing');
      if (w > 0) { try { localStorage.setItem(LEFT_W_KEY, String(Math.round(w))); } catch (_) { } }
    };
    split.addEventListener('pointermove', move);
    split.addEventListener('pointerup', up);
    split.addEventListener('pointercancel', up);
  });
}

// ── Chain strip ───────────────────────────────────────────────────────────────

const STAGE_TOKENS_ORDER = ['PFR', 'LIN', 'MIN', 'HP', 'AHP', 'TFS', 'DC', 'ISP', 'SUB', 'AHR', 'AA', 'XTC'];
const STAGE_COLORS = {
  PFR: 'var(--lab-pfr)',
  LIN: 'var(--lab-pfr)',
  MIN: 'var(--lab-pfr)',
  HP:  'var(--lab-hp)',
  AHP: 'var(--lab-ahp)',
  TFS: 'var(--lab-tfs)',
  DC:  'var(--lab-dc)',
  ISP: 'var(--lab-isp)',
  SUB: 'var(--lab-sub)',
  AHR: 'var(--lab-ahr)',
  AA:  'var(--lab-aa)',
  XTC: 'var(--lab-xtc)',
};

function _updateChainStrip(tokens, flags) {
  const strip = document.getElementById('anChainStrip');
  if (!strip) return;

  const activeSet = new Set(
    tokens.split(/[·\-_\s]/)
          .map(t => t.replace(/\d+$/, '').toUpperCase())
  );

  // Parse the raw token string to get the visible chip labels
  const rawTokens = tokens ? tokens.split(/[·]/) : [];
  strip.innerHTML = '';

  for (const raw of rawTokens) {
    const key = raw.replace(/\d+$/, '').toUpperCase();
    const chip = document.createElement('span');
    chip.className = 'chain-chip';
    chip.textContent = raw;
    chip.style.setProperty('--chip-color', STAGE_COLORS[key] || '#7dd3fc');
    chip.classList.add('chain-chip--on');
    strip.appendChild(chip);
  }
}

// ── HP banner ─────────────────────────────────────────────────────────────────

function _updateHpBanner(hpActive, oCoverageComplete) {
  const banner = document.getElementById('anHpBanner');
  if (!banner) return;
  if (hpActive && !oCoverageComplete) {
    banner.classList.remove('an-hidden');
  } else {
    banner.classList.add('an-hidden');
  }
}

// ── Status bar ─────────────────────────────────────────────────────────────────

// The live window's seek bar owns the time readout (see _initLiveScrub).
let _liveScrub = false;

// The O pass (the whole track through the chain heard, in the background):
// its progress 0..99, 100 when done, null when there is none.
let _oPassPct = null;

// ── The playhead clock ────────────────────────────────────────────────────────

/**
 * `_posS`, where the sound is, for every view that follows the playhead (LIVE
 * spectrum, playheads, the table's LIVE rows). The player's status comes four
 * times a second; set straight from it, the views moved in 250 ms steps.
 * Between polls the position runs on with the frame clock; a poll close to it
 * pulls it in gently (never backwards by a visible step), one far from it (a
 * seek, a stall) is taken at once.
 */
const _posClock = (() => {
  let base = 0, at = 0, playing = false, raf = 0;
  const now = () => (playing ? base + (performance.now() - at) / 1000 : base);
  const tick = () => {
    raf = 0;
    store.set('_posS', now());
    if (playing) raf = requestAnimationFrame(tick);
  };
  return {
    poll(posS, isPlaying) {
      const cur = now();
      const off = posS - cur;
      base = (playing && isPlaying && Math.abs(off) < 0.3) ? cur + off * 0.25 : posS;
      at = performance.now();
      playing = isPlaying;
      if (!raf) tick();
    },
    /** Nothing of this window's plays (stopped, another track): no playhead.
     *  The clock ran on from its last "playing" poll, and the waveform
     *  followed a ghost to the end of the track. */
    stop() {
      if (raf) { cancelAnimationFrame(raf); raf = 0; }
      if (playing || store.get('_posS') != null) {
        playing = false;
        store.set('_posS', null);
      }
    },
  };
})();

// ── Analysis progress (under the chain strip) ────────────────────────────────

// The O pass's pace for its ETA: [O generation, first ms, first %].
let _oPace = null;
// The whole analysis done (0..1, the line's fill) and the status bar's word on it.
let _progressFrac = 0;
let _statusWord = '';

/**
 * The thin gold line under the chain: how much of the whole-track analysis
 * is done (S and B spectrograms, the O pass; progress.js weighs them), its
 * percentage on a tag riding at the fill's end, both gone when all of it is.
 * The tooltip says what is still being computed, so a half-filled view is
 * not taken for the result.
 */
function _renderProgress(frame) {
  const el = document.getElementById('anProgress');
  const fill = document.getElementById('anProgressFill');
  if (!el || !fill) return;
  const info = frame.spec_info;   // [S, B, O] or null
  let eta = '';
  if (_oPassPct !== null && _oPassPct < 100) {
    const pct = _oPassPct;
    const gen = info && info[2] ? info[2].gen : 0;
    const now = performance.now();
    if (!_oPace || _oPace[0] !== gen || pct < _oPace[2]) _oPace = [gen, now, pct];
    const dt = (now - _oPace[1]) / 1000, dp = pct - _oPace[2];
    if (dt > 1 && dp > 0) eta = `, about ${Math.max(1, Math.round((100 - pct) * dt / dp))} s left`;
  }
  // (A stream has no whole file to decode: nothing waits there.)
  const decoding = new URLSearchParams(location.search).get('mode') === 'live' && !store.get('_stream');
  const stages = analysisStages(info, _oPassPct, { decoding, eta });
  const { frac, open } = overallProgress(stages);
  _progressFrac = frac;
  fill.style.width = `${Math.round(frac * 1000) / 10}%`;
  const badge = document.getElementById('anProgressPct');
  if (badge) badge.textContent = percentText(frac);
  _placeProgressBadge();
  el.classList.toggle('an-progress--idle', open === 0);
  el.dataset.tip = progressTip(stages, open);
  _refreshTip(el);
  _statusWord = statusText(stages, open);
}

/** The percentage on the end of the fill, whole inside the line, risen over
 *  a toolbar button under it (again when the window's width changes). The
 *  rise is a transform: the tag's own top stays where the sheet puts it on
 *  the line, and is measured from there. */
function _placeProgressBadge() {
  const el = document.getElementById('anProgress');
  const badge = document.getElementById('anProgressPct');
  if (!el || !badge) return;
  const lr = el.getBoundingClientRect();
  const spacer = document.querySelector('#anToolbar > .an-spacer');
  const controls = [...document.querySelectorAll('#anToolbar > *')]
    .filter(c => c !== spacer)
    .map(c => c.getBoundingClientRect())
    .filter(r => r.width > 0)
    .map(r => ({ left: r.left, right: r.right, top: r.top }));
  const bottom = lr.top + badge.offsetTop + badge.offsetHeight;
  const { left, lift } = placeBadge(_progressFrac, lr.left, lr.width, badge.offsetWidth, bottom, controls);
  badge.style.left = `${left}px`;
  badge.style.transform = lift ? `translateY(${-lift}px)` : '';
}

function _updateStatusBar(_unused, timeS, durationS) {
  const cov  = document.getElementById('anCoverageText');
  const dur  = document.getElementById('anDurationText');
  const fill = document.getElementById('anPlayheadFill');

  if (cov) cov.textContent = _statusWord;
  if (_liveScrub) return;
  if (dur)  dur.textContent  = `${formatTimeInt(timeS)} / ${formatTimeInt(durationS)}`;
  if (fill && durationS > 0) {
    fill.style.left = `${(timeS / durationS) * 100}%`;
  }
}

// ── Live frame handling ───────────────────────────────────────────────────────

let _prevTrackId  = null;
let _prevChainRev = null;

function _handleLive(buf, sched) {
  let frame;
  try { frame = decodeAAN1(buf); } catch (_) { return; }

  // Update seq for next poll
  sched.updateLiveSeq(frame.seq);

  // Detect track change — always set trackId; dispatch on first frame too
  const tidStr = `${frame.track_id_lo}-${frame.track_id_hi}`;
  store.set('trackId', BigInt(frame.track_id_lo));
  if (tidStr !== _prevTrackId) {
    bus.dispatchEvent(new CustomEvent('an:track:change', {
      detail: { trackId: BigInt(frame.track_id_lo), trackName: '', durationS: 0 },
    }));
    // The views are empty now: the whole track frame again, whatever its rev.
    // (The live window's title follows the player's status: _noteLiveTitle.)
    sched.refetchTrack();
  }
  _prevTrackId = tidStr;

  // Detect chain change; on the first live frame (_prevChainRev===null) always
  // notify so the scheduler fetches the initial track and resp frames.
  if (frame.chain_rev !== _prevChainRev) {
    if (_prevChainRev !== null) {
      // Real mid-session chain change
      bus.dispatchEvent(new CustomEvent('an:chain:change', {
        detail: { rev: frame.chain_rev, tokens: store.get('chain').tokens, time_s: 0 },
      }));
    }
    sched.notifyChainChange(frame.chain_rev);
  }
  _prevChainRev = frame.chain_rev;

  // Store updates
  store.setDeep('chain.rev',   frame.chain_rev);
  store.setDeep('chain.flags', frame.flags);
  store.setDeep('metrics.coveragePct', frame.coverage_pct);

  // HP banner: no longer needed — whole-track O is measured by rendering
  // the track through the chain heard (Hybrid-Phase included), not by
  // listening to it.
  _updateHpBanner(false, true);
  store.set('hpBannerVisible', false);

  // The live frame's values are live only (the metrics table shows them in
  // its LIVE section); store.metrics.o holds the whole-track O (AAN2).

  // A stream whose clock began again (the same station tuned anew keeps its
  // name, so _noteStream left them): the points kept are the session before's.
  if (frame.strm) {
    const s = store.get('series');
    const last = Math.max(s.lufs_s_s_live?.at?.(-1)?.time_s ?? -Infinity, s.lufs_s_o?.at?.(-1)?.time_s ?? -Infinity);
    if (last > frame.strm.clockS + 1) {
      for (const k of ['lufs_s_o', 'lufs_m_o', 'lufs_s_s_live', 'lufs_m_s_live']) store.setDeep(`series.${k}`, []);
    }
  }

  // Accumulate O loudness history points
  if (frame.n_loud_pts > 0) {
    const series = store.get('series');
    const newO_s = [...series.lufs_s_o];
    const newO_m = [...series.lufs_m_o];
    for (const pt of frame.loud_pts) {
      newO_s.push({ time_s: pt.time_s, value: pt.lufs_s });
      newO_m.push({ time_s: pt.time_s, value: pt.lufs_m });
    }
    store.setDeep('series.lufs_s_o', newO_s);
    store.setDeep('series.lufs_m_o', newO_m);
  }
  // A stream: S's points beside O's (its STRM tail), both kept 30 minutes.
  const strm = frame.strm;
  if (strm) {
    const series = store.get('series');
    const keep = p => p.time_s >= strm.clockS - 1800;
    const add = (old, k) => [...(old || []).filter(keep), ...strm.sPts.map(p => ({ time_s: p.time_s, value: p[k] }))];
    store.setDeep('series.lufs_s_s_live', add(series.lufs_s_s_live, 'lufs_s'));
    store.setDeep('series.lufs_m_s_live', add(series.lufs_m_s_live, 'lufs_m'));
    if (series.lufs_s_o.length && !keep(series.lufs_s_o[0])) {
      store.setDeep('series.lufs_s_o', series.lufs_s_o.filter(keep));
      store.setDeep('series.lufs_m_o', series.lufs_m_o.filter(keep));
    }
  }


  // Dispatch event
  bus.dispatchEvent(new CustomEvent('an:live', { detail: { frame } }));

  // Status bar
  _updateStatusBar(frame.coverage_pct, 0, store.get('durationS'));
}

// ── Track frame handling ──────────────────────────────────────────────────────

function _handleTrack(buf, sched) {
  let frame;
  try { frame = decodeAAN2(buf); } catch (_) { return; }
  if (frame.stub) {
    // Server says no change; update our known rev
    sched.notifyTrackRev(frame.rev);
    return;
  }

  sched.notifyTrackRev(frame.rev);

  store.setDeep('trackRev', frame.rev);
  store.setDeep('chain.tokens', frame.chain_str);
  store.setDeep('metrics.bReady', true);
  if (frame.duration_s > 0) store.set('durationS', frame.duration_s);

  // Populate store metrics from AAN2 so export.js can read them.
  const metrics = store.get('metrics');
  if (frame.s_metrics && frame.s_metrics.length) {
    for (let i = 0; i < Math.min(frame.s_metrics.length, metrics.s.length); i++)
      metrics.s[i] = frame.s_metrics[i];
  }
  if (frame.b_metrics && frame.b_metrics.length) {
    for (let i = 0; i < Math.min(frame.b_metrics.length, metrics.b.length); i++)
      metrics.b[i] = frame.b_metrics[i];
  }
  if (frame.o_metrics && frame.o_metrics.length) {
    for (let i = 0; i < Math.min(frame.o_metrics.length, metrics.o.length); i++)
      metrics.o[i] = frame.o_metrics[i];
  }
  if (frame.s_prov && frame.s_prov.length) {
    for (let i = 0; i < Math.min(frame.s_prov.length, metrics.sProv.length); i++)
      metrics.sProv[i] = frame.s_prov[i];
  }
  if (frame.b_prov && frame.b_prov.length) {
    for (let i = 0; i < Math.min(frame.b_prov.length, metrics.bProv.length); i++)
      metrics.bProv[i] = frame.b_prov[i];
  }
  if (frame.o_prov && frame.o_prov.length) {
    for (let i = 0; i < Math.min(frame.o_prov.length, metrics.oProv.length); i++)
      metrics.oProv[i] = frame.o_prov[i];
  }
  // Update track metadata for title bar and export headers
  if (frame.track_name)  store.set('trackName',  frame.track_name);
  if (frame.chain_str)   store.setDeep('metrics.chainStr', frame.chain_str);
  store.set('_srcRate', frame.src_rate || store.get('_srcRate') || 44100);
  // A file window has no output: its rate is the file's. The live window
  // takes the output rate from the player's status (_initLiveScrub).
  store.set('_outRate', frame.out_rate || store.get('_outRate') || store.get('_srcRate'));
  store.set('_bits',    frame.bit_depth || store.get('_bits') || 24);

  // Update chain strip
  _updateChainStrip(frame.chain_str, store.get('chain').flags);

  // Update chain marks
  const existing = store.get('chain').marks;
  const newMarks = frame.chain_marks.map(m => ({
    time_s: m.time_s, fromToken: m.from_token, toToken: m.to_token, stageId: m.stage_id,
  }));
  store.setDeep('chain.marks', [...existing, ...newMarks]);

  // S/B/O series (O's whole track from the O pass; its live points stay apart)
  store.setDeep('series.lufs_s_s', frame.lufs_s_series_s);
  store.setDeep('series.lufs_m_s', frame.lufs_m_series_s);
  store.setDeep('series.lufs_s_b', frame.lufs_s_series_b);
  store.setDeep('series.lufs_m_b', frame.lufs_m_series_b);
  const oWhole = frame.lufs_s_series_o && frame.lufs_s_series_o.length > 0;
  store.setDeep('series.lufs_s_o_whole', oWhole ? frame.lufs_s_series_o : null);
  store.setDeep('series.lufs_m_o_whole', oWhole ? frame.lufs_m_series_o : null);

  // The O pass's progress (COVERAGE_O while COMPUTING).
  const oP = frame.o_prov ? frame.o_prov[N_METRICS - 1] : PROV.UNAVAIL;
  const oV = frame.o_metrics ? frame.o_metrics[N_METRICS - 1] : NaN;
  _oPassPct = oP === PROV.COMPUTING ? Math.floor(oV || 0) : oP === PROV.MEASURED ? 100 : null;
  _renderProgress(frame);
  _updateStatusBar(0, 0, store.get('durationS'));

  bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame } }));
}

// ── Resp frame handling ───────────────────────────────────────────────────────

function _pairsToFlat(pairs) {
  const arr = new Float32Array(pairs.length * 2);
  for (let i = 0; i < pairs.length; i++) {
    arr[2 * i]     = pairs[i].min;
    arr[2 * i + 1] = pairs[i].max;
  }
  return arr;
}

function _handleResp(buf) {
  let frame;
  try { frame = decodeAAN3(buf); } catch (_) { return; }

  // Populate layer store with flat Float32Arrays
  store.setDeep('layers.h_mag', {
    data:     _pairsToFlat(frame.h_pairs),
    minF:     frame.f0_actual,
    maxF:     frame.f1_actual,
    chainRev: frame.chain_rev,
  });
  store.setDeep('layers.o_psd', {
    data:     _pairsToFlat(frame.o_pairs),
    minF:     frame.f0_actual,
    maxF:     frame.f1_actual,
    chainRev: frame.chain_rev,
  });
  if (frame.s_pairs && frame.s_pairs.length > 0) {
    store.setDeep('layers.s_psd', {
      data:     _pairsToFlat(frame.s_pairs),
      minF:     frame.f0_actual,
      maxF:     frame.f1_actual,
      chainRev: frame.chain_rev,
    });
  }
  if (frame.b_pairs && frame.b_pairs.length > 0) {
    store.setDeep('layers.b_psd', {
      data:     _pairsToFlat(frame.b_pairs),
      minF:     frame.f0_actual,
      maxF:     frame.f1_actual,
      chainRev: frame.chain_rev,
    });
  }

  bus.dispatchEvent(new CustomEvent('an:resp', { detail: { frame } }));
}

// ── Panel collapse/expand ─────────────────────────────────────────────────────

// The graphs are tabs: all of them named in one row, the chosen one takes
// the whole column (no scrolling). The choice is remembered.
const TAB_KEY = 'analyzer-tab';

// Bring a graph's tab up (a snapshot shows on the spectrum). Set by _initPanels.
let _selectTab = () => {};

function _initPanels() {
  const host = document.getElementById('anPanels');
  const panels = [...document.querySelectorAll('#anPanels > .an-panel-collapsible')];
  if (!host || !panels.length) return;
  host.classList.add('an-tabbed');
  const bar = document.createElement('div');
  bar.id = 'anTabs';
  bar.setAttribute('role', 'tablist');
  host.insertBefore(bar, host.firstChild);

  const tabs = panels.map((panel) => {
    const title = panel.querySelector('.an-section-title')?.textContent.trim() || panel.id;
    const tab = document.createElement('button');
    tab.type = 'button';
    tab.className = 'an-tab';
    tab.setAttribute('role', 'tab');
    tab.textContent = title;
    tab.addEventListener('click', () => select(panel.id));
    bar.appendChild(tab);
    return { panel, tab };
  });

  function select(id) {
    for (const { panel, tab } of tabs) {
      const on = panel.id === id;
      panel.classList.toggle('an-tab-active', on);
      panel.classList.toggle('an-panel--expanded', on);
      tab.classList.toggle('an-tab--on', on);
      tab.setAttribute('aria-selected', on ? 'true' : 'false');
    }
    try { localStorage.setItem(TAB_KEY, id); } catch (_) { }
  }

  let first = tabs[0].panel.id;
  try {
    const saved = localStorage.getItem(TAB_KEY);
    if (saved && tabs.some(t => t.panel.id === saved)) first = saved;
  } catch (_) { }
  select(first);
  _selectTab = select;
  // A stream's songs: their tab only while one plays.
  const songs = tabs.find(t => t.panel.id === 'anSongsSection');
  if (songs) {
    const show = (on) => {
      songs.tab.hidden = !on;
      if (!on && songs.panel.classList.contains('an-tab-active')) select(tabs[0].panel.id);
    };
    show(!!store.get('_stream'));
    store.on('_stream', show);
  }
}

/** The station a song in the list was heard on, when it is not this one. */
function _stationOfSong(e) {
  const name = _radioNames?.stationName?.({ info: { url: e.url, icyName: e.icy } })
    || String(e.icy || '').trim() || _hostOf(e.url);
  return name === store.get('_radioNow')?.station ? '' : name;
}

/** The graph shown (the active tab's panel), for "Save PNG — the graph". */
function _graphRect() {
  const p = document.querySelector('#anPanels > .an-tab-active');
  return p ? p.getBoundingClientRect() : null;
}

// ── Toolbar ───────────────────────────────────────────────────────────────────

function _initToolbar(mode) {
  // (The Snapshot button is wired in _main, once the snapshots exist: it
  // used to send the spectrum an empty "snapshot" and nothing showed.)

  // Keyboard: B → the toolbar's B (by the key's place: any layout)
  document.addEventListener('keydown', (ev) => {
    if (ev.target.tagName === 'INPUT' || ev.target.tagName === 'TEXTAREA' || ev.target.tagName === 'SELECT') return;
    if (ev.ctrlKey || ev.altKey || ev.metaKey) return;
    if (ev.code === 'KeyB') {
      const bToggle = document.getElementById('anBToggle');
      if (bToggle) { ev.preventDefault(); bToggle.click(); }
    }
  });

  // Keyboard: ←/→ in LIVE window forward to player (Space: _initPlayButton)
  if (mode === 'live') {
    document.addEventListener('keydown', (ev) => {
      if (ev.target.tagName === 'INPUT' || ev.target.tagName === 'TEXTAREA') return;
      if (ev.key === 'ArrowLeft' || ev.key === 'ArrowRight') {
        ev.preventDefault();
        try {
          window.__TAURI__.event.emit('player-key', { key: ev.key });
        } catch (_) { }
      }
    });
  }
}

// ── Main bootstrap ────────────────────────────────────────────────────────────

// ── Title bar helper ──────────────────────────────────────────────────────────

/** The title (title.js): the system's as one line, the bar's in its parts,
 *  each in its colour. */
function _setWindowTitle(w) {
  const parts = titleParts(w);
  document.title = parts.map(p => p.text).join('');
  const el = document.getElementById('anTitleText');
  if (el) el.innerHTML = titleMarkup(parts);
}

async function _main() {
  const params = _parseParams();
  store.set({ mode: params.mode, entryId: params.entryId, seq: params.seq, chainHex: params.chain });

  // The window's title: a file window's file; the live window's follows
  // what plays (_noteLiveTitle).
  _setWindowTitle({ mode: params.mode, path: params.path, conv: params.conv, entryId: params.entryId });
  if (params.mode !== 'live') store.set('trackName', fileName(params.conv || params.path));

  // Init subsystems
  _initTitleBar();
  _initSplitter();
  window.addEventListener('resize', _placeProgressBadge);
  // The live window always follows the player; a file window while its
  // track is the one playing.
  if (params.mode === 'live') _initLiveScrub(() => true, null);
  else if (params.track != null) _initLiveScrub(s => s.trackId === params.track, params.track);
  _initTooltip();
  _initPanels();
  _initToolbar(params.mode);

  // Open subject via invoke (the only allowed invoke path)
  let sid;
  try {
    if (window.__TAURI__) {
      const invokeArgs = { mode: params.mode };
      if (params.entryId) invokeArgs.entryId = parseInt(params.entryId, 10);
      if (params.conv)    invokeArgs.conv     = params.conv;
      if (params.path)    invokeArgs.path     = params.path;
      sid = await window.__TAURI__.tauri.invoke('an_subject_open', invokeArgs);
    } else {
      // Non-Tauri dev preview: use a mock sid
      sid = 1;
    }
  } catch (err) {
    console.error('an_subject_open failed:', err);
    return;
  }
  store.set('sid', sid);

  // Create scheduler
  const sched = createScheduler(sid, {
    onLive:  (buf) => _handleLive(buf, sched),
    onTrack: (buf) => _handleTrack(buf, sched),
    onResp:  (buf) => _handleResp(buf),
    onError: (route, err) => console.warn(`[scheduler/${route}]`, err),
  });

  // Create metrics table
  const metricsEl = document.getElementById('anMetricsTable');
  if (metricsEl) {
    createMetricsTable(metricsEl, { store, bus });
  }
  // A stream's songs (its own tab).
  const songsEl = document.getElementById('anSongs');
  if (songsEl) createSongsView(songsEl, store, _stationOfSong);

  // ── Shared subsystems ────────────────────────────────────────────────────
  const selection = createSelection({ store, bus });
  const tileCache = createTileCache({ store, bus });
  const cursor    = createCursor({ store, bus });
  const snapshot  = createSnapshot({ store, bus });
  const viewCtx   = { store, bus, tileCache, cursor, snapshot };

  // ── Canvas views ─────────────────────────────────────────────────────────
  const views = [];
  const strips = {};   // viewId → its layer strip (setOn keeps a pill in step)

  // The spectrum and loudness views take their layers' toggles from the bus
  // themselves; the others only have setLayer, which nothing called: their
  // layer pills went dim and the graph did not change.
  const TAKES_TOGGLES = new Set(['spectrum', 'loudness']);

  function _mountView(createFn, layers, wrapId, stripContainerId, viewId) {
    const wrap = document.getElementById(wrapId);
    if (!wrap) return null;
    let view;
    try { view = createFn(wrap, viewCtx); } catch (err) {
      console.error('[window] view init failed:', viewId, err); return null;
    }
    // Layer strip — prepend to the panel's canvas-panel-wrap
    const stripContainer = document.getElementById(stripContainerId);
    if (stripContainer && layers && layers.length) {
      try { strips[viewId] = createLayerStrip(stripContainer, layers, bus, viewId); } catch (err) {
        console.warn('[window] layer strip failed:', viewId, err);
      }
    }
    if (!TAKES_TOGGLES.has(viewId) && typeof view.setLayer === 'function') {
      bus.addEventListener('an:layer:toggle', (e) => {
        const d = e.detail || {};
        if (d.viewId === viewId && !d.delta && typeof d.on === 'boolean') view.setLayer(d.layerId, d.on);
      });
    }
    return view;
  }

  const spectrumView    = _mountView(createSpectrumView,    SPECTRUM_LAYERS,    'anSpectrumCanvasWrap',    'anSpectrumPanel',    'spectrum');
  const loudnessView    = _mountView(createLoudnessView,    LOUDNESS_LAYERS,    'anLoudnessCanvasWrap',    'anLoudnessPanel',    'loudness');
  const spectrogramView = _mountView(createSpectrogramView, SPECTROGRAM_LAYERS, 'anSpectrogramCanvasWrap', 'anSpectrogramPanel', 'spectrogram');
  const waveformView    = _mountView(createWaveformView,    WAVEFORM_LAYERS,    'anWaveformCanvasWrap',    'anWaveformPanel',    'waveform');
  const histogramView   = _mountView(createHistogramView,   HISTOGRAM_LAYERS,   'anHistogramCanvasWrap',   'anHistogramPanel',   'histogram');
  const stereoView      = _mountView(createStereoView,      STEREO_LAYERS,      'anStereoCanvasWrap',      'anStereoPanel',      'stereo');

  if (spectrumView)    views.push(spectrumView);
  if (loudnessView)    views.push(loudnessView);
  if (spectrogramView) views.push(spectrogramView);
  if (waveformView)    views.push(waveformView);
  if (histogramView)   views.push(histogramView);
  if (stereoView)      views.push(stereoView);

  // ── Toolbar B: the spectrum's B layer, in step with its pill ─────────────
  const bBtn = document.getElementById('anBToggle');
  if (bBtn) {
    bBtn.addEventListener('click', () => {
      const on = !bBtn.classList.contains('active');
      strips.spectrum?.setOn('b_psd', on);
      bus.dispatchEvent(new CustomEvent('an:layer:toggle', {
        detail: { layerId: 'b_psd', on, viewId: 'spectrum' },
      }));
    });
    bus.addEventListener('an:layer:toggle', (e) => {
      const d = e.detail || {};
      if (d.viewId === 'spectrum' && d.layerId === 'b_psd' && typeof d.on === 'boolean') {
        bBtn.classList.toggle('active', d.on);
      }
    });
  }

  // ── Snapshot: the O curve the spectrum shows, frozen ─────────────────────
  const snapBtn = document.getElementById('anSnapBtn');
  const snapResult = (r) => {
    if (r.snap) {
      _selectTab('anSpectrumSection');   // where it shows
      flashEl(snapBtn, `Captured ${r.snap.label}`);
    } else {
      flashEl(snapBtn, r.reason === 'full' ? 'Eight kept' : 'No curve yet');
    }
  };
  snapBtn?.addEventListener('click', () => snapResult(snapshot.request()));
  bus.addEventListener('an:snapshot:result', (e) => snapResult(e.detail || {}));
  // ↺ on a stream's session (metrics-table.js).
  bus.addEventListener('an:stream:reset', () => _fetchTotals(true));

  // ── Export: the Export ▾ menu, the copy cards, the screenshot buttons ────
  const exportBtn = document.getElementById('anExportMenuBtn');
  if (exportBtn) {
    try {
      const ep = createExportPopover(exportBtn, { ...viewCtx, getGraphRect: _graphRect });
      const copyCard = (id, block) => {
        const b = document.getElementById(id);
        b?.addEventListener('click', async () => {
          flashEl(b, (await ep.copyBlock(block)) ? 'Copied' : 'Failed');
        });
      };
      copyCard('anCardFooTp', 'truepeak');
      copyCard('anCardFooDr', 'dr');
      copyCard('anCardLab', 'lab');
      copyCard('anCopyAll', 'all');
      // The picture of the window: PNG under the metrics, the camera in the status bar.
      for (const id of ['anSavePng', 'anExportBtn']) {
        const b = document.getElementById(id);
        b?.addEventListener('click', async () => {
          try {
            const p = await ep.screenshot(null);
            if (p) flashEl(b, 'Saved');
          } catch (err) {
            console.error('[export png]', err);
            flashEl(b, 'Failed');
          }
        });
      }
    } catch (err) {
      console.warn('[window] export popover init failed:', err);
    }
  }

  // Map each canvas-wrap element id to its view so ResizeObserver
  // only notifies the view whose own container changed size.
  const _wrapViewMap = new Map([
    ['anSpectrumCanvasWrap',    spectrumView],
    ['anLoudnessCanvasWrap',    loudnessView],
    ['anSpectrogramCanvasWrap', spectrogramView],
    ['anWaveformCanvasWrap',    waveformView],
    ['anHistogramCanvasWrap',   histogramView],
    ['anStereoCanvasWrap',      stereoView],
  ]);

  const ro = new ResizeObserver((entries) => {
    bus.dispatchEvent(new CustomEvent('an:resize'));
    for (const entry of entries) {
      const el = entry.target;
      const view = _wrapViewMap.get(el.id);
      if (view) {
        try { view.onResize?.(el.clientWidth, el.clientHeight); } catch (_) {}
      }
    }
  });
  const panelsEl = document.getElementById('anPanels');
  if (panelsEl) ro.observe(panelsEl);
  // Observe each canvas wrap individually for per-view resize events
  for (const id of _wrapViewMap.keys()) {
    const el = document.getElementById(id);
    if (el) ro.observe(el);
  }

  // Start polling — the first live frame will trigger notifyChainChange which
  // kicks off the initial track + resp fetches (see _handleLive).
  sched.start();

  // On close: clean up
  window.addEventListener('beforeunload', () => {
    sched.stop();
    ro.disconnect();
    cursor.destroy();
    selection.destroy();
    tileCache.destroy();
    for (const v of views) { try { v.destroy?.(); } catch (_) {} }
    try {
      if (window.__TAURI__) window.__TAURI__.tauri.invoke('an_subject_close', { sid });
    } catch (_) { }
  });
}

// Run when DOM is ready
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', _main);
} else {
  _main();
}
