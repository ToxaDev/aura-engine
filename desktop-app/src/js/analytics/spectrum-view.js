/**
 * spectrum-view.js — Log-frequency spectrum canvas view.
 *
 * Renders all spectrum layers (S PSD, B PSD, O PSD, |H|, Δ B-S, Δ O-B,
 * all-time max-hold, snapshots, ultra/infra tints) on a two-canvas stack:
 *   - mainCanvas:    redraws on data events (≈10 Hz or instantly on chain change)
 *   - overlayCanvas: redraws at 60 fps via cursor.drawOverlay()
 *
 * X-axis: log frequency, 20 Hz … output Nyquist (default; zoom adjusts).
 * Y-axis: dBFS, configurable floor (-60…-300).
 *
 * Two modes (the bar top right): WHOLE — the whole-track Welch spectra (and a
 * selection's, dotted); LIVE — S, B and O at the playhead, from the spectrogram's
 * tiles (512 log bands, the spectrogram's multi-resolution FFT): instant (the column
 * at the playhead), average (the last 1, 3 or 10 s) or peak hold (the highest since
 * the playhead last jumped, or Reset). Before the O pass has reached the playhead,
 * O is the newest live column (instant only).
 *
 * Exports:
 *   create(container, ctx)  — factory per FRONTEND-CONTRACT §4
 *   LAYER_REGISTRY          — consumed by layers.js for the layer strip
 *
 * Dependencies (other agents' files, code against contract):
 *   ./protocol.js  — PROV constants
 *   ./axes.js      — axis drawing helpers (this file)
 */

import { STREAM_NOTE } from './protocol.js';
import {
  logFreqFromNorm, normFromLogFreq, normFromDb,
  setupCanvas, drawLogFreqAxis, drawDbAxis,
  drawSpectrumGrid,
} from './axes.js';

// ---------------------------------------------------------------------------
// Layer registry (consumed by layers.js — §5 of FRONTEND-CONTRACT)
// ---------------------------------------------------------------------------

export const LAYER_REGISTRY = [
  {
    id:             's_psd',
    label:          'S',
    colorToken:     '--an-s',
    defaultOn:      true,
    shortcut:       '1',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'b_psd',
    label:          'B',
    colorToken:     '--an-b',
    defaultOn:      false,
    shortcut:       '2',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'o_psd',
    label:          'O',
    colorToken:     '--an-o',
    defaultOn:      true,
    shortcut:       '3',
    provenanceAware: true,
    axisRole:       'left',
  },
  {
    id:             'h_mag',
    label:          '|H|',
    colorToken:     '--an-h',
    defaultOn:      true,
    shortcut:       '4',
    provenanceAware: false,
    axisRole:       'right',
  },
  {
    id:             'delta_bs',
    label:          'Δ B-S',
    colorToken:     '--an-d2',
    defaultOn:      false,
    shortcut:       '5',
    provenanceAware: false,
    axisRole:       'right',
  },
  {
    id:             'delta_ob',
    label:          'Δ O-B',
    colorToken:     '--an-d',
    defaultOn:      false,
    shortcut:       '6',
    provenanceAware: false,
    axisRole:       'right',
  },
  {
    id:             'sel',
    label:          'Sel',
    colorToken:     '--an-s',
    defaultOn:      true,
    shortcut:       null,
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'max_hold',
    label:          'Max',
    colorToken:     '--an-s',
    defaultOn:      false,
    shortcut:       'M',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'ultra',
    label:          '>24k tint',
    colorToken:     '--an-ultra-tint',
    defaultOn:      true,
    shortcut:       'U',
    provenanceAware: false,
    axisRole:       null,
  },
  {
    id:             'infra',
    label:          '<20 Hz tint',
    colorToken:     '--an-infra-tint',
    defaultOn:      true,
    shortcut:       'I',
    provenanceAware: false,
    axisRole:       null,
  },
];

// ---------------------------------------------------------------------------
// Layer colors (resolved from CSS at first draw; fallback if getComputedStyle
// is not yet available during node --check)
// ---------------------------------------------------------------------------

const LAYER_COLORS = {
  s_psd:    { stroke: '#4ade80' },
  b_psd:    { stroke: '#94a3b8' },
  o_psd:    { stroke: '#38bdf8' },
  h_mag:    { stroke: '#f59e0b' },
  delta_bs: { stroke: '#a78bfa' },
  delta_ob: { stroke: '#fb923c' },
};

/** Right-axis (dBr) layers that use a separate Y-axis. */
const RIGHT_AXIS_LAYERS = new Set(['h_mag', 'delta_bs', 'delta_ob']);

// Stage marker colors matching analytics.css
const STAGE_MARK_COLORS = {
  0: '#f59e0b', // FIR — amber
  1: '#4ade80', // DC  — green
  2: '#a78bfa', // ISP — violet
  3: '#22d3ee', // SUB — cyan
  4: '#fb923c', // AHR — orange
  5: '#64748b', // AA  — slate
  6: '#2dd4bf', // XTC — teal
  7: '#a78bfa', // HP  — violet (same as ISP)
  8: '#f59e0b', // TFS — amber
  9: '#a78bfa', // ISP_OUT — violet
};

// ---------------------------------------------------------------------------
// Canvas layout constants (CSS pixels; scaled by DPR internally)
// ---------------------------------------------------------------------------

const LAYER_STRIP_W = 28;  // px — left layer icon strip (owned by layers.js, we leave space)
const AXIS_LEFT_W  = 44;   // px — left margin for dBFS labels
const AXIS_RIGHT_W = 52;   // px — right margin for dBr labels (shown when Δ/|H| active)
const AXIS_BOTTOM_H = 28;  // px — bottom margin for freq labels
const CAPTION_H    = 20;   // px — top caption row (window/FFT info)
const LEGEND_H     = 22;   // px — cursor-value legend strip at bottom

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget, cursor: object }} ctx
 * @returns {object}  View instance per FRONTEND-CONTRACT §4
 */
export function create(container, ctx) {
  const { store, bus, cursor, tileCache } = ctx;

  // ------------------------------------------------------------------
  // DOM setup: two stacked canvases inside container
  // ------------------------------------------------------------------

  container.style.position = 'relative';
  container.style.overflow = 'hidden';

  const mainCanvas = document.createElement('canvas');
  mainCanvas.style.cssText = 'position:absolute;top:0;left:0;width:100%;height:100%;';
  container.appendChild(mainCanvas);

  const overlayCanvas = document.createElement('canvas');
  overlayCanvas.style.cssText =
    'position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none;';
  container.appendChild(overlayCanvas);

  // The overlay canvas captures mouse events on behalf of the view
  // (it is stacked on top of mainCanvas but does NOT have pointer-events:none
  // for the interaction tracking canvas — we use a transparent interaction layer)
  const interactionEl = document.createElement('div');
  interactionEl.style.cssText =
    'position:absolute;top:0;left:0;width:100%;height:100%;cursor:crosshair;';
  container.appendChild(interactionEl);

  const mainCtx = mainCanvas.getContext('2d');
  const ovCtx   = overlayCanvas.getContext('2d');

  // ------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------

  // Visible frequency range (Hz)
  let viewF0 = 20;
  let viewF1 = 0; // 0 = "use Nyquist of output" — resolved at draw time

  // dBFS axis range
  let dbTop   = 0;
  let dbFloor = -300;
  // The range follows the curves (fitAxes) until the wheel sets it by hand;
  // a double click gives it back to the curves.
  let autoY = true;
  let looseSince = 0;      // when the fitted range first became much tighter than the shown one

  // dBr axis range (for right-axis layers |H|, Δ)
  let dbrTop   = 6;
  let dbrFloor = -120;

  // Last known output Nyquist (updated from store when track frame arrives)
  let outputNyquist = 22050;

  // Layer visibility (keyed by layer id)
  const layerOn = {};
  for (const entry of LAYER_REGISTRY) {
    layerOn[entry.id] = entry.defaultOn;
  }

  // All-time max-hold arrays — keyed by layerId, Float32Array of max values
  const maxHold = {};

  // Cached layout (physical pixels, updated in onResize)
  let layout = null; // { x, y, w, h, scale, cssW, cssH }
  let rightAxisActive = false;

  // Zoom/pan gesture state
  let panActive = false;
  let panStartX = 0;
  let panStartF0 = 20;
  let panStartF1 = 0;


  // ------------------------------------------------------------------
  // Resp range signaling to scheduler (window.js polls on f0/f1/n change)
  // ------------------------------------------------------------------

  function signalRespRange() {
    if (!layout) return;
    const n = Math.min(4096, Math.max(256, Math.floor(layout.w / 2)));
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;
    store.set('spectrumView', { f0, f1, n });
    // Dispatch a bus event so window.js can forward to sched.notifyZoomChange.
    // (window.js must listen for 'an:spec:rangeChange' and call
    //  sched.notifyZoomChange(f0, f1, n) — see FRONTEND-CONTRACT §6.)
    bus.dispatchEvent(new CustomEvent('an:spec:rangeChange', { detail: { f0, f1, n } }));
  }

  // ------------------------------------------------------------------
  // Layout helpers
  // ------------------------------------------------------------------

  // ------------------------------------------------------------------
  // LIVE mode: the spectrum at the playhead from the spectrogram's tiles
  // ------------------------------------------------------------------

  const PREFS_KEY = 'auraAnSpectrum';
  const F0 = 20, SRC_BANDS = 512, TILE_COLS = 64;
  const AVG_CHOICES = [1, 3, 10];
  const prefs = (() => { try { return JSON.parse(localStorage.getItem(PREFS_KEY) || '{}') || {}; } catch { return {}; } })();
  let mode = prefs.mode === 'live' ? 'live' : 'whole';
  let liveKind = ['instant', 'avg', 'peak'].includes(prefs.kind) ? prefs.kind : 'instant';
  let avgS = AVG_CHOICES.includes(prefs.avg) ? prefs.avg : 3;
  const savePrefs = () => { try { localStorage.setItem(PREFS_KEY, JSON.stringify({ mode, kind: liveKind, avg: avgS })); } catch { /* none */ } };

  // Each signal's tiles (from AAN2's tail) and its peak hold.
  const lt = { S: { gen: -1, total: 0, ready: 0 }, B: { gen: -1, total: 0, ready: 0 }, O: { gen: -1, total: 0, ready: 0 } };
  let specHop = 0;
  const holds = {};                  // src → { upTo, max: Float64Array, gen }
  let liveCol = null;                // the newest AAN1 column { bytes, bins, floorNeg }
  // A stream: the newest columns of S and O (its STRM tail) on S's grid, in
  // dB — instant, average or peak hold over them, as over a file's tiles.
  const STREAM_COLS_KEPT = 256;      // ≈ 12 s
  const streamCols = { S: [], O: [] };
  const streamHold = { S: null, O: null };
  let streamColS = 0, streamSrcRate = 0;

  function noteStream(t) {
    streamColS = t.sCols.hop / Math.max(1, t.srcRate);
    streamSrcRate = t.srcRate;
    for (const [src, strip] of [['S', t.sCols], ['O', t.oCols]]) {
      const ring = streamCols[src];
      for (const c of strip.cols) {
        const db = new Float32Array(c.length);
        for (let b = 0; b < c.length; b++) db[b] = -120 + (c[b] / 255) * 120;
        ring.push(db);
        const h = streamHold[src];
        if (!h || h.length !== db.length) streamHold[src] = Float32Array.from(db);
        else for (let b = 0; b < db.length; b++) if (db[b] > h[b]) h[b] = db[b];
      }
      if (ring.length > STREAM_COLS_KEPT) ring.splice(0, ring.length - STREAM_COLS_KEPT);
    }
  }

  function streamBands(src) {
    const ring = streamCols[src];
    if (!ring || !ring.length) return null;
    const newest = ring[ring.length - 1];
    if (liveKind === 'instant') return { db: newest, nBins: newest.length };
    if (liveKind === 'avg') {
      const n = Math.min(ring.length, Math.max(1, Math.round(avgS / Math.max(1e-3, streamColS))));
      const sum = new Float64Array(newest.length);
      let cnt = 0;
      for (let k = ring.length - n; k < ring.length; k++) {
        if (ring[k].length !== newest.length) continue;
        for (let b = 0; b < newest.length; b++) sum[b] += db2p(ring[k][b]);
        cnt++;
      }
      const out = new Float32Array(newest.length);
      for (let b = 0; b < newest.length; b++) out[b] = 10 * Math.log10(Math.max(1e-30, sum[b] / cnt));
      return { db: out, nBins: newest.length };
    }
    const h = streamHold[src];
    return h ? { db: h, nBins: h.length } : null;
  }
  let liveLayers = {};               // the curves drawn now: s_psd, b_psd, o_psd (+ o512 for Δ)

  const srcRateNow = () => (store.get && store.get('_srcRate')) || 44100;
  const outRateNow = () => (store.get && store.get('_outRate')) || srcRateNow();

  /** Column c of a signal's tiles (its band bytes), asking for the tile if it is not here. */
  function tileCol(src, c) {
    const T = lt[src];
    const ti = Math.floor(c / TILE_COLS);
    if (c < 0 || ti >= Math.min(T.ready, T.total)) return null;
    const key = `${src}-spec-0-${ti}`;
    const bytes = tileCache && tileCache.get(key);
    if (!bytes) { tileCache && tileCache.request(key, src, 'spec', 0, ti); return null; }
    if (bytes.byteLength < 24) return null;
    const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (T.gen >= 0 && dv.getUint8(7) !== (T.gen & 255)) { tileCache.drop?.(key); return null; }
    const nCols = dv.getUint16(20, true), nBins = dv.getUint16(22, true);
    const k = c - ti * TILE_COLS;
    if (k >= nCols) return null;
    return { bytes: bytes.subarray(24 + k * nBins, 24 + (k + 1) * nBins), nBins,
      floor: dv.getFloat32(12, true), range: dv.getFloat32(16, true) };
  }

  const db2p = (db) => Math.pow(10, db / 10);

  /** A signal's live spectrum (dB per band) for the chosen kind, or null. */
  function liveBands(src) {
    // A stream: S and O measured live (no B).
    if (store.get && store.get('_stream')) return src === 'B' ? null : streamBands(src);
    const posS = store.get && store.get('_posS');
    if (!(specHop > 0) || posS == null) return null;
    const colS = specHop / srcRateNow();
    const c = Math.floor(posS / colS);
    if (liveKind === 'instant') {
      const col = tileCol(src, c);
      if (!col) return null;
      const out = new Float32Array(col.nBins);
      for (let b = 0; b < col.nBins; b++) out[b] = col.floor + (col.bytes[b] / 255) * col.range;
      return { db: out, nBins: col.nBins };
    }
    if (liveKind === 'avg') {
      const n = Math.max(1, Math.round(avgS / colS));
      let sum = null, cnt = 0, nBins = 0;
      for (let k = c - n + 1; k <= c; k++) {
        const col = tileCol(src, k);
        if (!col) continue;
        if (!sum) { nBins = col.nBins; sum = new Float64Array(nBins); }
        if (col.nBins !== nBins) continue;
        for (let b = 0; b < nBins; b++) sum[b] += db2p(col.floor + (col.bytes[b] / 255) * col.range);
        cnt++;
      }
      if (!cnt) return null;
      const out = new Float32Array(nBins);
      for (let b = 0; b < nBins; b++) out[b] = 10 * Math.log10(Math.max(1e-30, sum[b] / cnt));
      return { db: out, nBins };
    }
    // Peak hold: the highest since the playhead last jumped (or Reset).
    let h = holds[src];
    const jumped = h && (c < h.upTo - 2 || c > h.upTo + 400);
    if (!h || jumped || h.gen !== lt[src].gen) { h = holds[src] = { upTo: c - 1, max: null, gen: lt[src].gen }; }
    for (let k = h.upTo + 1; k <= c; k++) {
      const col = tileCol(src, k);
      if (!col) { if (k === c) break; continue; }
      if (!h.max || h.max.length !== col.nBins) h.max = new Float32Array(col.nBins).fill(-Infinity);
      for (let b = 0; b < col.nBins; b++) {
        const v = col.floor + (col.bytes[b] / 255) * col.range;
        if (v > h.max[b]) h.max[b] = v;
      }
      h.upTo = k;
    }
    return h.max ? { db: h.max, nBins: h.max.length } : null;
  }

  /** Bands 20 Hz.. on the source's step as a layer (min = max), the top at `top` Hz. */
  function bandsLayer(db, nBins, top) {
    const step = Math.log(top / F0) / nBins;
    const data = new Float32Array(nBins * 2);
    for (let b = 0; b < nBins; b++) { data[2 * b] = db[b]; data[2 * b + 1] = db[b]; }
    return { data, minF: F0 * Math.exp(0.5 * step), maxF: F0 * Math.exp((nBins - 0.5) * step) };
  }

  /** The curves of the LIVE mode now. */
  function updateLive() {
    liveLayers = {};
    const stream = !!(store.get && store.get('_stream'));
    const srcNyq = (stream && streamSrcRate ? streamSrcRate : srcRateNow()) / 2;
    const srcStep = Math.log(srcNyq / F0) / SRC_BANDS;
    for (const [src, lid] of [['S', 's_psd'], ['B', 'b_psd'], ['O', 'o_psd']]) {
      const r = liveBands(src);
      if (!r) continue;
      liveLayers[lid] = bandsLayer(r.db, r.nBins, F0 * Math.exp(r.nBins * srcStep));
      if (src === 'O') liveLayers.o512 = bandsLayer(r.db.subarray(0, SRC_BANDS), SRC_BANDS, srcNyq);
    }
    // Before the O pass has reached the playhead: the newest live column (its own bands).
    liveLayers.oLive = false;
    if (!liveLayers.o_psd && liveKind === 'instant' && liveCol) {
      const db = new Float32Array(liveCol.bins);
      for (let b = 0; b < liveCol.bins; b++) db[b] = -liveCol.floorNeg + (liveCol.bytes[b] / 255) * liveCol.floorNeg;
      liveLayers.o_psd = bandsLayer(db, liveCol.bins, outRateNow() / 2);
      liveLayers.oLive = true;
    }
  }

  // The mode bar (top right).
  const modeBar = document.createElement('div');
  modeBar.className = 'an-specview-modes';
  const mkBtn = (label, tip, on) => {
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'an-spec-btn'; b.textContent = label;
    if (tip) b.setAttribute('data-tip', tip);
    b.addEventListener('click', on);
    return b;
  };
  const seg = (...btns) => { const s = document.createElement('div'); s.className = 'an-spec-seg'; s.append(...btns); return s; };
  const bWhole = mkBtn('Whole', 'The whole-track spectra (Welch, 65536 points), and a selection\'s', () => { mode = 'whole'; savePrefs(); syncModes(); drawMain(); });
  const bLive = mkBtn('Live', 'S, B and O at the playhead, in the spectrogram\'s 512 log bands (its multi-resolution FFT, 4096 points at 44.1/48 kHz in the middle): noise reads higher than in the whole-track 65536-point curves, tones the same', () => { mode = 'live'; savePrefs(); syncModes(); updateLive(); drawMain(); });
  // A live stream has no whole track: the live spectrum shows, and the
  // listener's own choice comes back with a file.
  const wholeTip = bWhole.getAttribute('data-tip');
  let streamFrom = null;
  store.on?.('_stream', on => {
    bWhole.disabled = !!on;
    bWhole.setAttribute('data-tip', on ? STREAM_NOTE : wholeTip);
    if (on && mode === 'whole') { streamFrom = mode; mode = 'live'; }
    else if (!on && streamFrom) { mode = streamFrom; streamFrom = null; }
    else return;
    syncModes(); updateLive(); drawMain();
  });
  const bInst = mkBtn('Instant', 'The column at the playhead', () => { liveKind = 'instant'; savePrefs(); syncModes(); updateLive(); drawMain(); });
  const bAvg = mkBtn('Avg', 'The mean power of the last seconds before the playhead', () => { liveKind = 'avg'; savePrefs(); syncModes(); updateLive(); drawMain(); });
  const bPeak = mkBtn('Peak', 'The highest level at each band since the playhead last jumped', () => { liveKind = 'peak'; savePrefs(); syncModes(); updateLive(); drawMain(); });
  const avgSel = document.createElement('select');
  avgSel.className = 'an-spec-sel';
  avgSel.setAttribute('data-tip', 'How many seconds the average takes');
  for (const s of AVG_CHOICES) { const o = document.createElement('option'); o.value = String(s); o.textContent = `${s} s`; avgSel.appendChild(o); }
  avgSel.addEventListener('change', () => { avgS = Number(avgSel.value); savePrefs(); updateLive(); drawMain(); });
  const bReset = mkBtn('Reset', 'Start the peak hold again from here', () => { for (const k of Object.keys(holds)) delete holds[k]; streamHold.S = streamHold.O = null; updateLive(); drawMain(); });
  const liveGroup = document.createElement('span');
  liveGroup.className = 'an-specview-live';
  liveGroup.append(seg(bInst, bAvg, bPeak), avgSel, bReset);
  // The snapshots taken (Snapshot button, key S): filled by renderSnapChips.
  const snapGroup = document.createElement('span');
  snapGroup.className = 'an-snap-chips';
  snapGroup.style.display = 'none';
  modeBar.append(snapGroup, seg(bWhole, bLive), liveGroup);
  container.appendChild(modeBar);

  function syncModes() {
    bWhole.classList.toggle('an-spec-btn--on', mode === 'whole');
    bLive.classList.toggle('an-spec-btn--on', mode === 'live');
    liveGroup.style.display = mode === 'live' ? '' : 'none';
    bInst.classList.toggle('an-spec-btn--on', liveKind === 'instant');
    bAvg.classList.toggle('an-spec-btn--on', liveKind === 'avg');
    bPeak.classList.toggle('an-spec-btn--on', liveKind === 'peak');
    avgSel.style.display = liveKind === 'avg' ? '' : 'none';
    avgSel.value = String(avgS);
    bReset.style.display = liveKind === 'peak' ? '' : 'none';
  }
  syncModes();

  function computeLayout(scale, cssW, cssH) {
    const rightActive = RIGHT_AXIS_LAYERS.has('h_mag') && layerOn.h_mag ||
                        RIGHT_AXIS_LAYERS.has('delta_bs') && layerOn.delta_bs ||
                        RIGHT_AXIS_LAYERS.has('delta_ob') && layerOn.delta_ob;
    rightAxisActive = rightActive;

    const leftMargin  = (LAYER_STRIP_W + AXIS_LEFT_W) * scale;
    const rightMargin = rightActive ? AXIS_RIGHT_W * scale : 8 * scale;
    const topMargin   = CAPTION_H * scale;
    const bottomMargin = (AXIS_BOTTOM_H + LEGEND_H) * scale;

    return {
      x: leftMargin,
      y: topMargin,
      w: cssW * scale - leftMargin - rightMargin,
      h: cssH * scale - topMargin - bottomMargin,
      scale,
      cssW,
      cssH,
    };
  }

  // ------------------------------------------------------------------
  // Coordinate conversion for cursor tracking
  // ------------------------------------------------------------------

  // The pointer hands over x as a share of the whole canvas (the cursor
  // module, the wheel, clicks); the plot sits between the axes. These map
  // between the two, so the hairline is under the pointer.
  function plotNorm(xNorm) {
    if (!layout) return xNorm;
    const lay = computeLayout(1, layout.cssW, layout.cssH);
    return Math.max(0, Math.min(1, (xNorm * layout.cssW - lay.x) / Math.max(1, lay.w)));
  }
  function plotNormAt(clientX) {
    const rect = interactionEl.getBoundingClientRect();
    return plotNorm((clientX - rect.left) / Math.max(1, rect.width));
  }
  function clientXAtPlotNorm(nx) {
    const rect = interactionEl.getBoundingClientRect();
    if (!layout) return rect.left + nx * rect.width;
    const lay = computeLayout(1, layout.cssW, layout.cssH);
    return rect.left + (lay.x + nx * lay.w) * rect.width / Math.max(1, layout.cssW);
  }

  function toFreqHz(xNorm) {
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;
    return logFreqFromNorm(plotNorm(xNorm), f0, f1);
  }

  // Register with cursor module
  if (cursor) {
    cursor.register(
      { drawOverlay },
      interactionEl,
      { toFreqHz, toTimeS: () => null, viewId: 'spectrum' }
    );
  }

  // ------------------------------------------------------------------
  // Drawing helpers
  // ------------------------------------------------------------------

  /**
   * Interpolate a layer's data at a given frequency (Hz).
   * data: Float32Array of [min0, max0, min1, max1, ...], n_pts pairs
   * Returns { min, max } in dB, or null.
   */
  function sampleLayerAt(layerData, freqHz) {
    if (!layerData || !layerData.data) return null;
    const { data, minF, maxF } = layerData;
    const n = data.length >> 1; // number of points
    if (n < 1) return null;
    const norm = normFromLogFreq(freqHz, minF, maxF);
    const fi = Math.max(0, Math.min(n - 1, norm * (n - 1)));
    const i = Math.floor(fi);
    const frac = fi - i;
    const i2 = Math.min(n - 1, i + 1);
    // min channel
    const minVal = data[2 * i] * (1 - frac) + data[2 * i2] * frac;
    // max channel
    const maxVal = data[2 * i + 1] * (1 - frac) + data[2 * i2 + 1] * frac;
    return { min: minVal, max: maxVal };
  }

  /**
   * Draw a min+max envelope for a layer's PSD data.
   * Fills the band between min and max at each frequency point.
   */
  function drawEnvelope(ctx2d, layerData, color, lay, alpha, dashed, style) {
    if (!layerData || !layerData.data) return;

    const { data, minF, maxF } = layerData;
    const n = data.length >> 1;
    if (n < 2) return;

    const { x, y, w, h, scale } = lay;
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;

    const isRightAxis = false; // all PSDs use left axis (dBFS)
    const top = dbTop;
    const floor = dbFloor;

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    if (style?.dash) ctx2d.setLineDash(style.dash.map(d => d * scale));
    else if (dashed) ctx2d.setLineDash([4 * scale, 4 * scale]);
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = (style?.width || 1.5) * scale;

    // Draw as a thin vertical stroke per point (honest envelope display)
    ctx2d.beginPath();
    let firstVisible = true;
    for (let i = 0; i < n; i++) {
      const frac = i / (n - 1);
      // Use data's own frequency range (minF..maxF) so the mapping is correct
      // even when the data covers a different range than the current view.
      const hz = Math.pow(maxF / minF, frac) * minF;
      const nx = normFromLogFreq(hz, f0, f1);
      if (nx < 0 || nx > 1) continue;

      const minDb = data[2 * i];
      const maxDb = data[2 * i + 1];

      if (!isFinite(minDb) || !isFinite(maxDb)) continue;

      const clampedMax = Math.max(floor, Math.min(top, maxDb));
      const clampedMin = Math.max(floor, Math.min(top, minDb));

      const px  = x + nx * w;
      const pyMax = y + normFromDb(clampedMax, top, floor) * h;
      const pyMin = y + normFromDb(clampedMin, top, floor) * h;

      if (firstVisible) {
        ctx2d.moveTo(px, pyMax);
        firstVisible = false;
      } else {
        ctx2d.lineTo(px, pyMax);
      }
    }
    ctx2d.stroke();

    // Envelope fill (semi-transparent) only when min !== max
    ctx2d.globalAlpha = alpha * 0.15;
    ctx2d.fillStyle = color;
    ctx2d.setLineDash([]);
    ctx2d.beginPath();
    let started = false;
    for (let i = 0; i < n; i++) {
      const frac = i / (n - 1);
      const hz = Math.pow(maxF / minF, frac) * minF;
      const nx = normFromLogFreq(hz, f0, f1);
      if (nx < 0 || nx > 1) continue;

      const minDb = data[2 * i];
      const maxDb = data[2 * i + 1];
      if (!isFinite(maxDb)) continue;

      const clampedMax = Math.max(floor, Math.min(top, maxDb));
      const px  = x + nx * w;
      const pyMax = y + normFromDb(clampedMax, top, floor) * h;

      if (!started) { ctx2d.moveTo(px, pyMax); started = true; }
      else ctx2d.lineTo(px, pyMax);
    }
    // Close along the bottom (min envelope) in reverse
    for (let i = n - 1; i >= 0; i--) {
      const frac = i / (n - 1);
      const hz = Math.pow(maxF / minF, frac) * minF;
      const nx = normFromLogFreq(hz, f0, f1);
      if (nx < 0 || nx > 1) continue;
      const minDb = data[2 * i];
      if (!isFinite(minDb)) continue;
      const clampedMin = Math.max(floor, Math.min(top, minDb));
      const px  = x + nx * w;
      const pyMin = y + normFromDb(clampedMin, top, floor) * h;
      ctx2d.lineTo(px, pyMin);
    }
    ctx2d.closePath();
    ctx2d.fill();
    ctx2d.globalAlpha = 1;

    ctx2d.restore();
  }

  /**
   * Draw a right-axis (dBr) layer like |H| or a delta layer.
   */
  function drawRightAxisLayer(ctx2d, layerData, color, lay, alpha, dashed) {
    if (!layerData || !layerData.data) return;

    const { data, minF, maxF } = layerData;
    const n = data.length >> 1;
    if (n < 2) return;

    const { x, y, w, h, scale } = lay;
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;
    const top = dbrTop;
    const floor = dbrFloor;

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 1.5 * scale;
    if (dashed) ctx2d.setLineDash([6 * scale, 3 * scale]);

    ctx2d.beginPath();
    let firstVisible = true;
    for (let i = 0; i < n; i++) {
      const frac = i / (n - 1);
      // Use data's own frequency range (minF..maxF)
      const hz = Math.pow(maxF / minF, frac) * minF;
      const nx = normFromLogFreq(hz, f0, f1);
      if (nx < 0 || nx > 1) continue;

      // For right-axis layers, use the midpoint (max channel) as the line
      const maxDb = data[2 * i + 1];
      if (!isFinite(maxDb)) continue;

      const clamped = Math.max(floor, Math.min(top, maxDb));
      const px  = x + nx * w;
      const py  = y + normFromDb(clamped, top, floor) * h;

      if (firstVisible) { ctx2d.moveTo(px, py); firstVisible = false; }
      else ctx2d.lineTo(px, py);
    }
    ctx2d.stroke();

    ctx2d.restore();
  }

  /**
   * Compute delta (difference) between two PSD layers at matching points.
   * Returns a synthetic layer object: { data, minF, maxF, chainRev }.
   */
  function computeDelta(layerA, layerB) {
    if (!layerA || !layerB || !layerA.data || !layerB.data) return null;
    const n = Math.min(layerA.data.length, layerB.data.length) >> 1;
    const out = new Float32Array(n * 2);
    for (let i = 0; i < n; i++) {
      // Use the max value from each layer for the primary line
      const a = layerA.data[2 * i + 1];
      const b = layerB.data[2 * i + 1];
      const delta = isFinite(a) && isFinite(b) ? a - b : NaN;
      out[2 * i] = delta;
      out[2 * i + 1] = delta;
    }
    return { data: out, minF: layerA.minF, maxF: layerA.maxF, chainRev: layerA.chainRev };
  }

  // ------------------------------------------------------------------
  // Main canvas redraw
  // ------------------------------------------------------------------

  function drawMain() {
    if (!mainCtx || !layout) return;
    // LIVE follows the playhead: its curves are read again for every frame,
    // not only when one of its buttons is pressed.
    if (mode === 'live') updateLive();
    const { cssW, cssH, scale } = layout;
    const W = cssW * scale;
    const H = cssH * scale;

    mainCtx.clearRect(0, 0, W, H);

    // Resolve Nyquist if not yet set
    if (store.durationS > 0 && store.trackId) {
      // Use stored value from last track frame
    }
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;

    const lay = computeLayout(scale, cssW, cssH);

    // -- Background --
    mainCtx.fillStyle = 'transparent';

    // -- Ultra/infra tints (before grid) --
    if (layerOn.ultra) {
      const nx0 = normFromLogFreq(Math.max(f0, 24000), f0, f1);
      const nx1 = 1;
      if (nx0 < 1) {
        mainCtx.fillStyle = 'rgba(239,68,68,0.12)';
        mainCtx.fillRect(
          lay.x + nx0 * lay.w, lay.y,
          (nx1 - nx0) * lay.w, lay.h
        );
      }
    }

    if (layerOn.infra) {
      const nx0 = 0;
      const nx1 = normFromLogFreq(Math.min(f1, 20), f0, f1);
      if (nx1 > 0) {
        mainCtx.fillStyle = 'rgba(245,158,11,0.10)';
        mainCtx.fillRect(
          lay.x + nx0 * lay.w, lay.y,
          (nx1 - nx0) * lay.w, lay.h
        );
      }
    }

    // -- Layers (read first: the axes fit them) --
    const live = mode === 'live';
    const layers = live ? { ...store.layers, s_psd: liveLayers.s_psd, b_psd: liveLayers.b_psd, o_psd: liveLayers.o_psd } : store.layers;
    // Snapshots in both modes: each is the curve that was shown (its chip says which).
    const snaps  = (store.snapshots || []).filter(s => s.on !== false);
    fitAxes(layers, snaps, live, lay, f0, f1);

    // -- Grid --
    drawSpectrumGrid(mainCtx, lay, f0, f1, dbFloor, dbTop);

    // -- Axes --
    drawLogFreqAxis(mainCtx, lay, f0, f1);
    drawDbAxis(mainCtx, lay, dbFloor, dbTop);
    if (rightAxisActive) {
      // Its top labels would sit under the mode bar (top right): not drawn there.
      const barBottom = modeBar.offsetHeight ? (modeBar.offsetTop + modeBar.offsetHeight + 6) * scale : 0;
      drawDbAxis(mainCtx, lay, dbrFloor, dbrTop, { rightAxis: true, labelSuffix: ' dBr', minY: barBottom });
    }

    // Snapshots (drawn first — below live layers)
    for (const snap of snaps) {
      if (snap.data && snap.data.o_psd) {
        drawEnvelope(mainCtx, snap.data.o_psd, snap.color, lay, 0.65, false);
      }
    }

    // S PSD
    if (layerOn.s_psd && layers.s_psd) {
      drawEnvelope(mainCtx, layers.s_psd, LAYER_COLORS.s_psd.stroke, lay, 0.9, false);
      updateMaxHold('s_psd', layers.s_psd);
    }

    // B PSD (dashed line)
    if (layerOn.b_psd && layers.b_psd) {
      drawEnvelope(mainCtx, layers.b_psd, LAYER_COLORS.b_psd.stroke, lay, 0.8, true);
    }

    // O PSD — dotted where analytic/forecast (prov from store), solid where measured
    if (layerOn.o_psd && layers.o_psd) {
      // Check O provenance for LUFS_I (ID 0) as a representative metric
      const oProv = store.metrics ? store.metrics.oProv : null;
      const lufsIProv = oProv ? oProv[0] : 0;
      // PROV.MEASURED = 3; if measured use solid, else use dotted
      // (LIVE: dotted while O is the newest live column, not the O pass's tiles)
      const dotted = live ? !!liveLayers.oLive : lufsIProv !== 3;
      drawEnvelope(mainCtx, layers.o_psd, LAYER_COLORS.o_psd.stroke, lay, 0.9, dotted);
      updateMaxHold('o_psd', layers.o_psd);
    }

    // The selected stretch's spectra: dotted, thicker, for each signal shown.
    if (layerOn.sel && !live) {
      for (const [key, lid] of [['sel_s', 's_psd'], ['sel_b', 'b_psd'], ['sel_o', 'o_psd']]) {
        if (layers[key] && layerOn[lid]) {
          drawEnvelope(mainCtx, layers[key], LAYER_COLORS[lid].stroke, lay, 1, false, { dash: [1.5, 2.5], width: 2 });
        }
      }
    }

    // |H| — right axis, dashed amber
    if (layerOn.h_mag && layers.h_mag) {
      drawRightAxisLayer(mainCtx, layers.h_mag, LAYER_COLORS.h_mag.stroke, lay, 0.85, true);
    }

    // Δ B-S
    if (layerOn.delta_bs && layers.s_psd && layers.b_psd) {
      const delta = computeDelta(layers.b_psd, layers.s_psd);
      drawRightAxisLayer(mainCtx, delta, LAYER_COLORS.delta_bs.stroke, lay, 0.8, false);
    }

    // Δ O-B (LIVE: on the source's bands, O's first 512)
    if (layerOn.delta_ob && layers.o_psd && layers.b_psd && (!live || liveLayers.o512)) {
      const delta = computeDelta(live ? liveLayers.o512 : layers.o_psd, layers.b_psd);
      drawRightAxisLayer(mainCtx, delta, LAYER_COLORS.delta_ob.stroke, lay, 0.8, false);
    }

    // All-time max-hold (drawn last for each visible layer; LIVE has Peak instead)
    if (layerOn.max_hold && !live) {
      for (const [lid, holdData] of Object.entries(maxHold)) {
        if (!layerOn[lid]) continue;
        const color = (LAYER_COLORS[lid] || {}).stroke || '#fff';
        drawEnvelope(mainCtx, holdData, color, lay, 0.35, false);
      }
    }

    // -- Top-left caption --
    drawCaption(mainCtx, lay, scale);
  }

  // ------------------------------------------------------------------
  // Auto-fit of the dB axes (Anton 26.09: by default the peaks ran under
  // the caption and the mode buttons). The caption and the mode bar sit
  // over the plot's top; the fitted range puts the highest point of every
  // curve shown below that band and the lowest above the floor. LIVE moves
  // at once only when a curve would be hidden, and tightens only after the
  // curves have stayed well inside for a while, so the axis does not jump
  // with every column. The wheel's range stays until a double click.
  // ------------------------------------------------------------------

  const FIT_STEP = 6;          // dB: the fitted ends land on this grid
  const FIT_MIN_RANGE = 36;
  const FIT_MAX_RANGE = 300;   // the wheel's limit too
  const FIT_LOOSE_MS = 1500;   // LIVE: how long the range may stay too loose

  /// The highest and lowest level of the curves over [f0, f1]; null when
  /// there is nothing to fit. Levels at or under −290 dB are the floor's
  /// stand-ins, not the signal.
  function curveExtent(list, f0, f1) {
    let hi = -Infinity, lo = Infinity;
    for (const L of list) {
      if (!L || !L.data || !(L.minF > 0) || !(L.maxF > 0)) continue;
      const { data, minF, maxF } = L;
      const n = data.length >> 1;
      for (let i = 0; i < n; i++) {
        const hz = n > 1 ? Math.pow(maxF / minF, i / (n - 1)) * minF : minF;
        if (hz < f0 || hz > f1) continue;
        const a = data[2 * i], b = data[2 * i + 1];
        if (isFinite(b) && b > hi) hi = b;
        if (isFinite(a) && a > -290 && a < lo) lo = a;
      }
    }
    if (hi === -Infinity) return null;
    return { hi, lo: lo === Infinity ? hi - FIT_MIN_RANGE : lo };
  }

  /// The height (CSS px) of the band the caption and the mode bar cover at
  /// the plot's top.
  function labelBandPx() {
    const caption = 3 + 13 + 5;
    const bar = modeBar.offsetHeight ? modeBar.offsetTop + modeBar.offsetHeight - CAPTION_H + 5 : 0;
    return Math.max(caption, bar);
  }

  /// top/floor for a curve extent: floor a step under the lowest point,
  /// top so that the highest point is clear of the label band.
  function fitRange(ext, plotPx, bandPx, floorFixed) {
    const k = Math.min(0.45, bandPx / Math.max(1, plotPx));
    let floor = floorFixed != null ? floorFixed : Math.floor((ext.lo - 3) / FIT_STEP) * FIT_STEP;
    if (ext.hi - floor < FIT_MIN_RANGE) floor = Math.floor((ext.hi - FIT_MIN_RANGE) / FIT_STEP) * FIT_STEP;
    // (top − hi) / (top − floor) ≥ k  ⇔  top − hi ≥ k (hi − floor) / (1 − k)
    const top = Math.ceil((ext.hi + k * (ext.hi - floor) / (1 - k) + 1) / FIT_STEP) * FIT_STEP;
    if (top - floor > FIT_MAX_RANGE) floor = top - FIT_MAX_RANGE;
    return { top, floor };
  }

  function fitAxes(layers, snaps, live, lay, f0, f1) {
    const plotPx = lay.h / lay.scale;
    const band = labelBandPx();

    if (autoY) {
      const list = [];
      if (layerOn.s_psd) list.push(layers.s_psd);
      if (layerOn.b_psd) list.push(layers.b_psd);
      if (layerOn.o_psd) list.push(layers.o_psd);
      if (layerOn.sel && !live) {
        if (layerOn.s_psd) list.push(layers.sel_s);
        if (layerOn.b_psd) list.push(layers.sel_b);
        if (layerOn.o_psd) list.push(layers.sel_o);
      }
      for (const s of snaps) if (s.data) list.push(s.data.o_psd);
      if (layerOn.max_hold && !live) for (const [lid, d] of Object.entries(maxHold)) if (layerOn[lid]) list.push(d);
      const ext = curveExtent(list, f0, f1);
      if (ext) {
        const want = fitRange(ext, plotPx, band, null);
        // Hidden: the top curve in the label band, or the bottom one under the floor.
        const range = dbTop - dbFloor;
        const hidden = (dbTop - ext.hi) / range * plotPx < band - 0.5 || ext.lo < dbFloor;
        const loose = want.top < dbTop - 2 * FIT_STEP || want.floor > dbFloor + 4 * FIT_STEP;
        const nowMs = performance.now();
        if (!loose) looseSince = 0;
        else if (!looseSince) looseSince = nowMs;
        if (hidden || !live || (loose && nowMs - looseSince >= FIT_LOOSE_MS)) {
          dbTop = want.top;
          dbFloor = want.floor;
          looseSince = 0;
        }
      }
    }

    // The right axis (|H|, Δ): its floor stays at −120 dBr, the top clears the band.
    const right = [];
    if (layerOn.h_mag) right.push(layers.h_mag);
    if (layerOn.delta_bs && layers.s_psd && layers.b_psd) right.push(computeDelta(layers.b_psd, layers.s_psd));
    if (layerOn.delta_ob && layers.o_psd && layers.b_psd && (!live || liveLayers.o512)) {
      right.push(computeDelta(live ? liveLayers.o512 : layers.o_psd, layers.b_psd));
    }
    const extR = curveExtent(right, f0, f1);
    if (extR) {
      const want = fitRange({ hi: Math.min(24, Math.max(extR.hi, 0)), lo: -120 }, plotPx, band, -120);
      dbrTop = want.top;
      dbrFloor = want.floor;
    }
    // For our checks: the ranges shown and who set them.
    const tag = `${dbTop} ${dbFloor} ${dbrTop} ${dbrFloor} ${autoY ? 'auto' : 'user'}`;
    if (container.dataset.db !== tag) container.dataset.db = tag;
  }

  /** Draw the FFT window/size caption top-left. */
  function drawCaption(ctx2d, lay, scale) {
    const font = `${11 * scale}px ui-monospace, "Cascadia Mono", Consolas, monospace`;
    ctx2d.save();
    ctx2d.font = font;
    ctx2d.fillStyle = 'rgba(148,163,184,0.7)'; // slate-400
    ctx2d.textBaseline = 'top';
    ctx2d.textAlign = 'left';
    // Caption text: window function · FFT size · sample rate. The curves are
    // the whole-track Welch PSDs (track.rs WELCH_N; the live spectrogram
    // columns in AAN1 are a different, shorter FFT).
    const fsHz = (store.get && store.get('_srcRate')) || 44100;
    if (mode === 'live') {
      const kind = liveKind === 'avg' ? `avg ${avgS} s` : liveKind === 'peak' ? 'peak hold' : 'instant';
      const posS = store.get && store.get('_posS');
      const at = posS != null ? ` · ${Math.floor(posS / 60)}:${(posS % 60).toFixed(1).padStart(4, '0')}` : '';
      const oNote = liveLayers.oLive ? ' · O: live column' : '';
      ctx2d.fillText(`Live · ${kind} · 512 bands${at}${oNote}`, lay.x + 4 * scale, lay.y + 3 * scale);
      ctx2d.restore();
      return;
    }
    let text = `Kaiser β28 · 65536 pts · ${fsHz} Hz`;
    const sel = store.get && store.get('selection');
    const sl = store.layers && (store.layers.sel_s || store.layers.sel_o || store.layers.sel_b);
    if (sel && sl && layerOn.sel) {
      const n = sl.welchN && sl.welchN !== 65536 ? ` · ${sl.welchN} pts` : '';
      text += `   ┊ selection ${(sel.t1 - sel.t0).toFixed(2)} s (dotted)${n}`;
    }
    ctx2d.fillText(text, lay.x + 4 * scale, lay.y + 3 * scale);
    ctx2d.restore();
  }

  // ------------------------------------------------------------------
  // Max-hold update
  // ------------------------------------------------------------------

  function updateMaxHold(lid, layerData) {
    if (!layerData || !layerData.data) return;
    const n = layerData.data.length;
    if (!maxHold[lid]) {
      maxHold[lid] = {
        data: Float32Array.from(layerData.data),
        minF: layerData.minF,
        maxF: layerData.maxF,
        chainRev: layerData.chainRev,
      };
      return;
    }
    const hold = maxHold[lid];
    if (hold.data.length !== n) {
      maxHold[lid] = {
        data: Float32Array.from(layerData.data),
        minF: layerData.minF,
        maxF: layerData.maxF,
        chainRev: layerData.chainRev,
      };
      return;
    }
    for (let i = 1; i < n; i += 2) { // max channel is odd indices
      if (layerData.data[i] > hold.data[i]) hold.data[i] = layerData.data[i];
    }
  }

  // ------------------------------------------------------------------
  // Overlay canvas (cursor hairline + value legend at 60 fps)
  // ------------------------------------------------------------------

  function drawOverlay(cursor) {
    if (!ovCtx || !layout) return;
    const { cssW, cssH, scale } = layout;
    const W = cssW * scale;
    const H = cssH * scale;

    ovCtx.clearRect(0, 0, W, H);

    const { freqHz } = cursor;
    if (freqHz == null) return;

    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;
    const nx = normFromLogFreq(freqHz, f0, f1);
    if (nx < 0 || nx > 1) return;

    const lay = computeLayout(scale, cssW, cssH);
    const px = lay.x + nx * lay.w;

    // Cursor hairline
    ovCtx.save();
    ovCtx.strokeStyle = 'rgba(255,255,255,0.6)';
    ovCtx.lineWidth = 1 * scale;
    ovCtx.setLineDash([4 * scale, 4 * scale]);
    ovCtx.beginPath();
    ovCtx.moveTo(px, lay.y);
    ovCtx.lineTo(px, lay.y + lay.h);
    ovCtx.stroke();
    ovCtx.restore();

    // Value legend strip below the plot
    drawCursorLegend(ovCtx, lay, scale, freqHz, cursor.values || {});
  }

  function drawCursorLegend(ctx2d, lay, scale, freqHz, values) {
    const layers = mode === 'live' ? { ...store.layers, ...liveLayers } : store.layers;
    const parts = [];

    const fmtFreq = freqHz >= 1000
      ? (freqHz / 1000).toFixed(1).replace(/\.0$/, '') + ' kHz'
      : Math.round(freqHz) + ' Hz';
    parts.push('cursor: ' + fmtFreq);

    const layerOrder = ['s_psd', 'b_psd', 'o_psd', 'h_mag'];
    const layerLabels = { s_psd: 'S', b_psd: 'B', o_psd: 'O', h_mag: '|H|' };
    const layerColors = { s_psd: '#4ade80', b_psd: '#94a3b8', o_psd: '#38bdf8', h_mag: '#f59e0b' };
    const suffix = { s_psd: ' dBFS', b_psd: ' dBFS', o_psd: ' dBFS', h_mag: ' dBr' };

    for (const lid of layerOrder) {
      if (!layerOn[lid]) continue;
      const sample = sampleLayerAt(layers[lid], freqHz);
      if (!sample) continue;
      const val = sample.max;
      if (!isFinite(val)) continue;
      parts.push({ label: layerLabels[lid], value: val.toFixed(1) + suffix[lid], color: layerColors[lid] });
    }

    // Render the legend
    const legendY = lay.y + lay.h + AXIS_BOTTOM_H * scale;
    const legendH = LEGEND_H * scale;
    const font = `${11 * scale}px ui-monospace, "Cascadia Mono", Consolas, monospace`;

    ctx2d.save();
    ctx2d.font = font;
    ctx2d.textBaseline = 'middle';
    const textY = legendY + legendH / 2;

    // Background pill
    ctx2d.fillStyle = 'rgba(11,25,44,0.85)';
    ctx2d.beginPath();
    const pillH = 16 * scale;
    const pillY = textY - pillH / 2;
    ctx2d.roundRect(lay.x, pillY, lay.w, pillH, 4 * scale);
    ctx2d.fill();

    let curX = lay.x + 8 * scale;
    for (const part of parts) {
      if (typeof part === 'string') {
        ctx2d.fillStyle = '#7dd3fc';
        ctx2d.textAlign = 'left';
        ctx2d.fillText(part, curX, textY);
        curX += ctx2d.measureText(part).width + 12 * scale;
      } else {
        // Separator
        ctx2d.fillStyle = 'rgba(255,255,255,0.25)';
        ctx2d.fillText('│', curX, textY);
        curX += ctx2d.measureText('│').width + 4 * scale;
        // Colored label
        ctx2d.fillStyle = part.color;
        ctx2d.fillText(part.label + ':', curX, textY);
        curX += ctx2d.measureText(part.label + ':').width + 4 * scale;
        ctx2d.fillStyle = '#e2e8f0';
        ctx2d.fillText(part.value, curX, textY);
        curX += ctx2d.measureText(part.value).width + 12 * scale;
      }
    }
    ctx2d.restore();
  }

  // ------------------------------------------------------------------
  // Mouse zoom / pan
  // ------------------------------------------------------------------

  function handleWheel(e) {
    e.preventDefault();
    const f0 = viewF0;
    const f1 = viewF1 > 0 ? viewF1 : outputNyquist;

    if (e.ctrlKey || e.metaKey) {
      // Zoom X (frequency) — log scale zoom centered on cursor
      const fCenter = logFreqFromNorm(plotNormAt(e.clientX), f0, f1);

      const factor = e.deltaY > 0 ? 1.25 : 0.8;
      const logF0 = Math.log(f0);
      const logF1 = Math.log(f1);
      const logFc = Math.log(fCenter);

      const newLogF0 = logFc + (logF0 - logFc) * factor;
      const newLogF1 = logFc + (logF1 - logFc) * factor;

      viewF0 = Math.max(1, Math.exp(newLogF0));
      viewF1 = Math.min(200000, Math.exp(newLogF1));
      if (viewF1 - viewF0 < 10) { viewF0 = f0; viewF1 = f1; } // guard
    } else {
      // Zoom Y (dB range)
      const factor = e.deltaY > 0 ? 1.1 : 0.9;
      const range = dbTop - dbFloor;
      const newRange = Math.min(300, Math.max(20, range * factor));
      dbFloor = dbTop - newRange;
      autoY = false;   // the range is the listener's now
    }

    signalRespRange();
    drawMain();
  }

  function handleMouseDown(e) {
    if (e.button !== 0) return;
    panActive = true;
    panStartX = e.clientX;
    panStartF0 = viewF0;
    panStartF1 = viewF1 > 0 ? viewF1 : outputNyquist;
    e.preventDefault();
  }

  function handleMouseMove(e) {
    if (!panActive) return;
    const rect = interactionEl.getBoundingClientRect();
    const dx = (e.clientX - panStartX) / rect.width; // normalized
    const logRange = Math.log(panStartF1 / panStartF0);
    const shift = -dx * logRange;
    viewF0 = Math.max(1, panStartF0 * Math.exp(shift));
    viewF1 = Math.min(200000, panStartF1 * Math.exp(shift));
    signalRespRange();
    drawMain();
  }

  function handleMouseUp() {
    panActive = false;
  }

  function handleDblClick() {
    // Reset zoom to defaults
    viewF0 = 20;
    viewF1 = 0;
    dbTop   = 0;
    dbFloor = -300;
    autoY = true;      // and the dB range follows the curves again
    looseSince = 0;
    signalRespRange();
    drawMain();
  }

  interactionEl.addEventListener('wheel', handleWheel, { passive: false });
  interactionEl.addEventListener('mousedown', handleMouseDown);
  window.addEventListener('mousemove', handleMouseMove, { passive: true });
  window.addEventListener('mouseup', handleMouseUp);
  interactionEl.addEventListener('dblclick', handleDblClick);

  // Right-click context menu hook (detail wired up by window.js)
  interactionEl.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    bus.dispatchEvent(new CustomEvent('an:canvas:contextmenu', {
      detail: { viewId: 'spectrum', clientX: e.clientX, clientY: e.clientY },
    }));
  });

  // The layers' keys are the layer strip's (layers.js): it toggles the pill
  // and sends an:layer:toggle, which onLayerToggle below takes. A second
  // handler here toggled every layer twice.

  // ------------------------------------------------------------------
  // Bus event subscriptions
  // ------------------------------------------------------------------

  function onResp(e) {
    // an:resp carries the newly decoded AAN3 frame; store already updated by window.js.
    // Update outputNyquist from the actual data range reported by the server so the
    // visible frequency axis expands to the true output Nyquist on the next draw.
    const layers = store.layers;
    const dataMaxF = Math.max(
      (layers && layers.h_mag && layers.h_mag.maxF) || 0,
      (layers && layers.o_psd && layers.o_psd.maxF) || 0,
      (layers && layers.s_psd && layers.s_psd.maxF) || 0,
    );
    if (dataMaxF > outputNyquist + 100) {
      outputNyquist = dataMaxF;
    }
    drawMain();
  }

  function onTrack(e) {
    // an:track carries the AAN2 frame — S/B series and metrics arrived
    // Extract Nyquist from track metadata if available
    if (e.detail && e.detail.frame && e.detail.frame.f_nyquist) {
      outputNyquist = e.detail.frame.f_nyquist;
    }
    // LIVE reads the spectrogram's tiles: which pass made them, how many are done.
    const f = e.detail && e.detail.frame;
    if (f && !f.stub) {
      if (f.spec_hop) specHop = f.spec_hop;
      const info = f.spec_info;
      const cnt = (f.spec_tile_counts_s || [])[0] || 0;
      if (info) {
        ['S', 'B', 'O'].forEach((k, i) => {
          if (lt[k].gen !== info[i].gen) { lt[k].gen = info[i].gen; delete holds[k]; tileCache?.dropPrefix?.(`${k}-spec-`); }
          lt[k].total = info[i].total; lt[k].ready = info[i].ready;
        });
        if (!info[0].total && cnt) { lt.S.total = cnt; lt.S.ready = cnt; }
      } else if (cnt) {
        lt.S.total = cnt; lt.S.ready = cnt;
      }
    }
    // Reset max-hold on track change (handled separately via an:track:change)
    drawMain();
  }

  function onLive(e) {
    // The newest live O column (LIVE's O before the O pass reaches the playhead).
    const f = e.detail && e.detail.frame;
    const cols = f && f.spec_cols;
    if (f && f.spec_hop_log2 && cols && cols.length) {
      liveCol = { bytes: cols[cols.length - 1], bins: f.n_fft_bins, floorNeg: f.spec_floor_neg || 120 };
    }
    if (f && f.strm) noteStream(f.strm);
    // O PSD data may have been updated in the live frame (analytic → measured transition)
    drawMain();
  }

  function onSnapshotAdd() {
    drawMain();
  }

  function onSnapshotRemove() {
    drawMain();
  }

  /** The snapshot button or the S key: freeze the O curve shown now. */
  function onSnapshotCapture(e) {
    const d = e.detail;
    if (!d || !ctx.snapshot) return;
    d.handled = true;
    let what = 'whole track';
    let src = store.layers;
    if (mode === 'live') {
      updateLive();
      src = { o_psd: liveLayers.o_psd, s_psd: liveLayers.s_psd, h_mag: store.layers.h_mag };
      const kind = liveKind === 'avg' ? `avg ${avgS} s` : liveKind === 'peak' ? 'peak hold' : 'instant';
      const posS = store.get && store.get('_posS');
      const at = posS != null ? ` · ${Math.floor(posS / 60)}:${(posS % 60).toFixed(1).padStart(4, '0')}` : '';
      what = `live · ${kind}${at}${liveLayers.oLive ? ' · live column' : ''}`;
    }
    const r = ctx.snapshot.capture(src, what);
    d.snap = r.snap || null;
    d.reason = r.reason || null;
  }

  // The snapshots' list in the mode bar: a chip each, in its colour.
  function renderSnapChips() {
    const list = store.snapshots || [];
    snapGroup.textContent = '';
    snapGroup.style.display = list.length ? '' : 'none';
    for (const s of list) {
      const chip = document.createElement('span');
      chip.className = 'an-snap-chip' + (s.on === false ? ' an-snap-chip--off' : '');
      chip.style.setProperty('--snap-color', s.color);
      const eye = document.createElement('button');
      eye.type = 'button';
      eye.className = 'an-snap-eye';
      eye.textContent = s.label;
      eye.setAttribute('data-tip', `Snapshot ${s.label}: O, ${s.what}${s.data.chainTokens ? ` · ${s.data.chainTokens}` : ''}. Click: show or hide.`);
      eye.addEventListener('click', () => ctx.snapshot.toggleById(s.id));
      const del = document.createElement('button');
      del.type = 'button';
      del.className = 'an-snap-del';
      del.textContent = '✕';
      del.setAttribute('aria-label', `Remove snapshot ${s.label}`);
      del.setAttribute('data-tip', 'Remove this snapshot (Ctrl+Z: the last one)');
      del.addEventListener('click', () => ctx.snapshot.removeById(s.id));
      chip.append(eye, del);
      snapGroup.appendChild(chip);
    }
  }
  const offSnapshots = store.on ? store.on('snapshots', () => { renderSnapChips(); drawMain(); }) : null;

  function onLayerToggle(e) {
    const { layerId, on } = e.detail || {};
    if (layerId in layerOn) {
      layerOn[layerId] = on;
      drawMain();
    }
  }

  function onTrackChange() {
    // Clear all max-hold data
    for (const key of Object.keys(maxHold)) delete maxHold[key];
    for (const key of Object.keys(holds)) delete holds[key];
    for (const k of ['S', 'B', 'O']) lt[k] = { gen: -1, total: 0, ready: 0 };
    specHop = 0; liveCol = null;
    streamCols.S = []; streamCols.O = []; streamHold.S = streamHold.O = null;
    outputNyquist = 22050;
    viewF0 = 20;
    viewF1 = 0;
    drawMain();
  }

  function onChainChange() {
    // |H| and analytic O update instantly on chain change
    drawMain();
  }

  bus.addEventListener('an:resp',          onResp);
  bus.addEventListener('an:track',         onTrack);
  bus.addEventListener('an:live',          onLive);
  bus.addEventListener('an:snapshot:add',  onSnapshotAdd);
  bus.addEventListener('an:snapshot:remove', onSnapshotRemove);
  bus.addEventListener('an:snapshot:capture', onSnapshotCapture);
  bus.addEventListener('an:layer:toggle',  onLayerToggle);
  bus.addEventListener('an:track:change',  onTrackChange);
  bus.addEventListener('an:chain:change',  onChainChange);

  // ------------------------------------------------------------------
  // View lifecycle API (FRONTEND-CONTRACT §4)
  // ------------------------------------------------------------------

  function setData(payload) {
    // Payload from window.js — may be a resp frame, track frame, or live frame
    if (payload && payload.f_nyquist) {
      outputNyquist = payload.f_nyquist;
    }
    drawMain();
  }

  function setLayer(key, on) {
    if (key in layerOn) {
      layerOn[key] = on;
      drawMain();
    }
  }

  function onResize(w, h) {
    if (!mainCanvas || !overlayCanvas) return;
    const scale = window.devicePixelRatio || 1;
    mainCanvas.width   = Math.round(w * scale);
    mainCanvas.height  = Math.round(h * scale);
    overlayCanvas.width  = Math.round(w * scale);
    overlayCanvas.height = Math.round(h * scale);
    layout = computeLayout(scale, w, h);
    signalRespRange();
    drawMain();
  }

  function destroy() {
    bus.removeEventListener('an:resp',            onResp);
    bus.removeEventListener('an:track',           onTrack);
    bus.removeEventListener('an:live',            onLive);
    bus.removeEventListener('an:snapshot:add',    onSnapshotAdd);
    bus.removeEventListener('an:snapshot:remove', onSnapshotRemove);
    bus.removeEventListener('an:snapshot:capture', onSnapshotCapture);
    bus.removeEventListener('an:layer:toggle',    onLayerToggle);
    offSnapshots?.();
    bus.removeEventListener('an:track:change',    onTrackChange);
    bus.removeEventListener('an:chain:change',    onChainChange);

    interactionEl.removeEventListener('wheel', handleWheel);
    interactionEl.removeEventListener('mousedown', handleMouseDown);
    window.removeEventListener('mousemove', handleMouseMove);
    window.removeEventListener('mouseup', handleMouseUp);
    interactionEl.removeEventListener('dblclick', handleDblClick);

    if (cursor) cursor.unregister(interactionEl);

    container.innerHTML = '';
  }

  // ------------------------------------------------------------------
  // Initial layout
  // ------------------------------------------------------------------
  {
    const scale = window.devicePixelRatio || 1;
    const w = container.clientWidth || 800;
    const h = container.clientHeight || 220;
    mainCanvas.width   = Math.round(w * scale);
    mainCanvas.height  = Math.round(h * scale);
    overlayCanvas.width  = Math.round(w * scale);
    overlayCanvas.height = Math.round(h * scale);
    layout = computeLayout(scale, w, h);
    signalRespRange();
    drawMain();
  }

  return { setData, setLayer, onResize, drawOverlay, destroy };
}
