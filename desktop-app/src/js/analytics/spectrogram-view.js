/**
 * spectrogram-view.js — the spectrogram panel: the whole track as S, B and O, their
 * comparison, and the output as it plays.
 *
 * Data (PROTOCOL.md §8.2): AAST tiles of 64 columns on one time grid for all three
 * signals (column c = source samples [c·hop, (c+1)·hop), hop from AAN2) in log bands
 * from 20 Hz: S (the file) and B (after the source stages) have 512 up to the
 * source's Nyquist, O (the output, from the O pass) the same 512 and more above at the
 * same step, up to its own Nyquist. AAN2's tail says which pass made each signal's
 * tiles and how many are done (O's appear while its pass runs). Live O columns from
 * AAN1 fill in what the O pass has not reached. Zoomed in past the whole-track
 * columns, finer S tiles (a hop of 2^lod samples) come for the stretch shown (the
 * `specz` route, AAZR) and are drawn over the whole-track ones as they arrive.
 *
 * Drawing: each signal's tiles are placed into one canvas for the whole track (a
 * column per pixel, a band per row), the live columns into a strip of their own; the
 * view draws them through `drawBands`, so any frequency scale and any zoom costs one
 * draw call (log) or one per visible band row (linear, Mel, Bark). The bytes are kept,
 * so a new colour map or level range recolours everything in one worker message.
 *
 * Views: S, B or O alone; S | O (one above the other, one time and frequency axis);
 * O − S and B − S (what the chain, or the source stages, changed: blue quieter, red
 * louder, black unchanged; levels below the colour map's floor in both are not
 * compared; above the source's Nyquist O − S shows O itself).
 *
 * Controls (the toolbar above the plot, as RX-class editors have them): the view,
 * frequency scale Log / Lin / Mel / Bark, level top and range or Auto, the Δ scale,
 * the waveform over the plot (its opacity; S's over S, O's over O), the colour map, Fit. Wheel: time zoom at the cursor; over the frequency ruler (or
 * with Alt): frequency zoom; Shift+wheel: move in time; drag: move; double click or
 * Fit: the whole track; Shift+drag: select a stretch (its statistics go to the table
 * and its spectrum to the spectrum panel, selection.js); a click or Esc clears it. The
 * cursor readout gives frequency (with the note), time and the levels of what is
 * shown there.
 *
 * A live stream (the radio) has no whole track: S's and O's columns as they play
 * (the frame's STRM tail) fill strips of their own, about three minutes of them;
 * the view follows the live edge or is zoomed and moved by hand over what they
 * keep (stream-view.js), Follow takes it back to the edge, and the readout gives
 * S's and O's levels there and O − S.
 *
 * Implements FRONTEND-CONTRACT §4 view lifecycle. LAYER_REGISTRY exported for layers.js.
 */

import { createStreamView, stripLevel } from './stream-view.js';

// ── Layer registry ────────────────────────────────────────────────────────────
// The signals are views (the toolbar), not layers: an image cannot be laid over another.

export const LAYER_REGISTRY = [
  { id: 'ultra_tint', label: '>24kHz',  colorToken: '--an-clip', defaultOn: true,  shortcut: 'u', provenanceAware: false, axisRole: null },
  { id: 'infra_tint', label: '<20Hz',   colorToken: '--an-over', defaultOn: true,  shortcut: 'i', provenanceAware: false, axisRole: null },
];

// ── Constants ─────────────────────────────────────────────────────────────────

const WORKER_URL = new URL('./spec-worker.js', import.meta.url).href;
const SPEC_TILE_COLS = 64;     // columns in a full AAST tile (the last one may hold fewer)
const SRC_BANDS      = 512;    // S and B bands; O's first 512 are the same
const LIVE_MAX_COLS  = 8192;   // ≈ 6 min at 46 ms a column
const F0             = 20;     // the bands' lowest edge (Hz)
const TOOLBAR_H      = 24;     // px
const COLORBAR_W     = 20;     // px
const RULER_W        = 38;     // px: the frequency ruler, between the plot and the colour bar
const TIME_H         = 16;     // px: the time ruler under the plot (the whole-track bar on top of it)
const OVERVIEW_H     = 3;      // px: the whole-track bar
const SPLIT_GAP      = 3;      // px between the halves of S | O
const C_RULER_BG = 'rgba(8,18,32,0.96)';
const COLOR_ULTRA = 'rgba(239,68,68,0.14)';
const COLOR_INFRA = 'rgba(245,158,11,0.12)';
const C_CURSOR   = 'rgba(255,255,255,0.6)';
const C_PLAYHEAD = '#38bdf8';
const C_AXIS     = '#7dd3fc';
const C_GRID     = 'rgba(255,255,255,0.07)';
const C_NYQ      = 'rgba(125,211,252,0.55)';
const PREFS_KEY  = 'auraAnSpectrogram';
const ZOOM_TILES_MAX = 192;    // zoomed tiles kept (64 × 512 each)
const ZOOM_POLL_MS   = 60;     // while the backend computes the stretch
const DELTA_RANGES   = [3, 6, 12, 24, 48];

// Colour bar stops of each map (the worker has the full tables).
const MAP_STOPS = {
  inferno:   ['#000004', '#57106e', '#bc3754', '#f98e09', '#fcffa4'],
  magma:     ['#000004', '#51127c', '#b73779', '#fc8961', '#fcfdbf'],
  viridis:   ['#440154', '#3b528b', '#21918c', '#5ec962', '#fde725'],
  grayscale: ['#000000', '#404040', '#808080', '#bfbfbf', '#ffffff'],
};
// The Δ map (spec-worker.js 'diverging'): quieter blue, unchanged black, louder red.
const DELTA_STOPS = ['#9ec5ff', '#2f6fd0', '#0a0a0a', '#d0452f', '#ffb49e'];

// The views: what each needs, and how the toolbar says it.
const VIEWS = {
  S:  { label: 'S',     needs: ['S'],      tip: 'S — the source file as it is' },
  B:  { label: 'B',     needs: ['B'],      tip: 'B — the source after its stages (DC, ISP, SUB…), before the filter' },
  O:  { label: 'O',     needs: ['O'],      tip: 'O — the output: the whole track through the chain heard' },
  SO: { label: 'S | O', needs: ['S', 'O'], tip: 'S above, O below, on one time and frequency axis' },
  OS: { label: 'O − S', needs: ['S', 'O'], tip: 'What the chain changed: blue quieter in O, red louder, black the same' },
  BS: { label: 'B − S', needs: ['S', 'B'], tip: 'What the source stages changed: blue quieter in B, red louder' },
};

// ── Frequency scales ──────────────────────────────────────────────────────────
// Each maps Hz to an axis unit that is linear on screen.

const SCALES = {
  log:  { label: 'Log',  fwd: f => Math.log(Math.max(f, 1e-3)),          inv: u => Math.exp(u) },
  lin:  { label: 'Lin',  fwd: f => f,                                     inv: u => u },
  mel:  { label: 'Mel',  fwd: f => 2595 * Math.log10(1 + f / 700),        inv: u => 700 * (Math.pow(10, u / 2595) - 1) },
  // Traunmüller's Bark.
  bark: { label: 'Bark', fwd: f => 26.81 * f / (1960 + f) - 0.53,        inv: u => 1960 * (u + 0.53) / (26.28 - u) },
};

// A stream's spectrogram: the plot's tip ({n}: the minutes its strips keep), Follow, Fit.
const STREAM_TIP = (n) => `The last ${n} minutes of the stream are kept. Wheel: zoom (over the frequency ruler or with Alt: frequency) · drag: move · double click: all of it · Follow: back to the live edge.`;
const STREAM_FOLLOW_TIP = 'Back to the live edge, the zoom as it is. Moving the view by hand stops following.';
const STREAM_FIT_TIP = 'All of it and every frequency, following the live edge again (double click)';
const FIT_TIP = 'The whole track and every frequency (double click)';

// Frequency ruler candidates; the drawn ones are at least 16 px apart.
const RULER_HZ = [20, 30, 50, 70, 100, 150, 200, 300, 500, 700, 1000, 1500, 2000, 3000, 4000, 5000,
  7000, 10000, 12000, 15000, 20000, 25000, 30000, 40000, 50000, 60000, 80000, 100000, 150000, 200000];

const NOTE_NAMES = ['C', 'C♯', 'D', 'D♯', 'E', 'F', 'F♯', 'G', 'G♯', 'A', 'A♯', 'B'];

function noteOf(hz) {
  if (!(hz > 16)) return '';
  const n = 69 + 12 * Math.log2(hz / 440);
  const k = Math.round(n);
  const cents = Math.round((n - k) * 100);
  const name = NOTE_NAMES[((k % 12) + 12) % 12] + (Math.floor(k / 12) - 1);
  return cents === 0 ? name : `${name}${cents > 0 ? '+' : '−'}${Math.abs(cents)}¢`;
}

function fmtHz(hz) {
  return hz >= 1000 ? `${(hz / 1000).toFixed(hz >= 10000 ? 1 : 2)} kHz` : `${hz.toFixed(0)} Hz`;
}

function fmtTime(s) {
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(2).padStart(5, '0')}`;
}

function fmtDb(v) {
  return `${v >= 0 ? '+' : '−'}${Math.abs(v).toFixed(1)}`;
}

function loadPrefs() {
  try { return JSON.parse(localStorage.getItem(PREFS_KEY) || '{}') || {}; } catch { return {}; }
}

function savePrefs(p) {
  try { localStorage.setItem(PREFS_KEY, JSON.stringify(p)); } catch { /* storage unavailable */ }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget, tileCache: object }} ctx
 */
export function create(container, ctx) {
  const { store, bus, tileCache } = ctx;

  // ── DOM ────────────────────────────────────────────────────────────────────
  container.style.cssText = 'position:relative;overflow:hidden;width:100%;height:100%;';
  const toolbar = document.createElement('div');
  toolbar.className = 'an-spec-toolbar';
  const plot = document.createElement('div');
  plot.style.cssText = `position:absolute;left:0;right:0;bottom:0;top:${TOOLBAR_H}px;`;
  const mainCanvas = document.createElement('canvas');
  mainCanvas.style.cssText = 'display:block;position:absolute;inset:0;width:100%;height:100%;cursor:crosshair;';
  const overlayCanvas = document.createElement('canvas');
  overlayCanvas.style.cssText = 'display:block;position:absolute;inset:0;pointer-events:none;width:100%;height:100%;';
  plot.append(mainCanvas, overlayCanvas);
  container.append(toolbar, plot);

  // ── Settings (kept per viewer) ─────────────────────────────────────────────
  const prefs = Object.assign({ view: 'S', scale: 'log', colormap: 'inferno', top: 0, range: 120, auto: false, delta: 12, match: true, wave: 0 }, loadPrefs());
  let view     = VIEWS[prefs.view] ? prefs.view : 'S';
  let scale    = SCALES[prefs.scale] ? prefs.scale : 'log';
  let colormap = MAP_STOPS[prefs.colormap] ? prefs.colormap : 'inferno';
  let topDb    = Number.isFinite(prefs.top) ? prefs.top : 0;
  let rangeDb  = Number.isFinite(prefs.range) ? prefs.range : 120;
  let autoTop  = !!prefs.auto;
  let deltaDb  = DELTA_RANGES.includes(prefs.delta) ? prefs.delta : 12;
  let matchLvl = prefs.match !== false;
  let waveAlpha = Number.isFinite(prefs.wave) ? Math.max(0, Math.min(100, prefs.wave)) : 0;
  const persist = () => savePrefs({ view, scale, colormap, top: topDb, range: rangeDb, auto: autoTop, delta: deltaDb, match: matchLvl, wave: waveAlpha });

  const layers = {};
  for (const def of LAYER_REGISTRY) layers[def.id] = def.defaultOn;

  // ── Track state ────────────────────────────────────────────────────────────
  let durationS = 0;
  let specHop   = 0;     // the tiles' hop in source samples (AAN2; 0 = not sent)
  let trackSig  = '';    // what makes S's canvas valid (a new one resets everything)
  let srcFloor  = -120, srcRange = 120;  // the bytes' dB scale (from the tiles)
  let cGen      = 0;     // bumps on a reset or a full recolour: late colours are dropped

  // Each signal's whole-track canvas: bytes (column-major, cols × nBins) and colours.
  const newTrack = (src) => ({
    src, gen: -1, tileCnt: 0, ready: 0, nBins: 0, cols: 0, realCols: 0,
    bytes: null, canvas: null, placed: new Set(),
  });
  const tr = { S: newTrack('S'), B: newTrack('B'), O: newTrack('O') };

  // The Δ view's canvas: bytes 128 + (a − b) per band (0..511), per tile computed.
  const delta = { key: '', bytes: null, canvas: null, placed: new Set() };

  // Live O: a strip of columns, the used part from x = 0, the newest last.
  let liveCanvas = null;
  let liveBytes  = null; // LIVE_MAX_COLS × liveBins, the same order as the canvas
  let liveCount  = 0;
  let liveBins   = 0;
  let liveHop    = 0;
  let liveGen    = 0;
  let livePosS   = null;
  let liveFloor  = 120;  // |floor| dB of the live columns (AAN1)

  // A stream (the radio): S's and O's live columns (the frame's STRM tail,
  // stream.rs) on S's grid — column k the same moment in both, band j the
  // same frequency — and their Δ, column for column. Strip: { canvas, bytes,
  // bins, count, first (column number), gen, shifted (columns gone off its
  // left since it began) }. The view follows the live edge (its last
  // STREAM_SPAN_S s at first) or is zoomed and moved by hand over them.
  const STREAM_SPAN_S = 20;
  const STREAM_MAX_COLS = 4096;   // ≈ 3 min at 46 ms a column
  const STREAM_GAP_MAX = 64;      // ≈ 3 s: frames the page missed, filled in; longer starts over
  const strips = { S: null, O: null, D: null };
  let stripGen = 0;
  let streamColS = 0;             // a column's length, s (S's hop over its rate)
  let streamClock = 0;            // the stream time after the newest frame measured
  const sview = createStreamView({ span0: STREAM_SPAN_S, domain: streamDomain });

  // Zoomed tiles: `${src}:${lod}:${i}` → { src, lod, i, nCols, nBins, bytes, canvas }, the
  // same grid for S, B and O; per signal the pass they belong to (AAZR) and whether
  // it has a zoom at all. The Δ views' zoomed tiles: `${lod}:${i}` → { …, bytes, canvas }.
  const zTiles = new Map();
  const zSrc = { S: { gen: null, unavailable: false }, B: { gen: null, unavailable: false }, O: { gen: null, unavailable: false } };
  const zDelta = new Map();
  let zDeltaKey = '';
  let zoomLod = 0;       // the hop (2^lod source samples) the view uses; 0 = the whole-track tiles
  let zBusy = false, zTimer = null;

  // The waveform over the plot: S's and O's tiles (AAWT, an entry per 256 source
  // samples at level 0, each next level 4× coarser; O's on the same grid).
  let waveLods = 0;
  const waveCounts = { S: [], O: [] };
  const waveDecoded = new Map();   // key → { lo: Float32Array, hi: Float32Array, n }

  // View.
  let viewStartS = 0, viewEndS = 0;
  let fLo = F0, fHi = 0; // fHi 0 = up to the axis' top
  let mainW = 0, mainH = 0;
  let destroyed = false;
  let rafHandle = null;
  // The cursor: this view's own (time and frequency) or another view's time.
  let cursor = null;
  // The selection (any view's, an:selection) and the one being dragged out.
  let selT0 = null, selT1 = null;
  let selecting = null;

  // ── Worker ─────────────────────────────────────────────────────────────────
  let worker = null;
  try { worker = new Worker(WORKER_URL); } catch { /* no worker: nothing is coloured */ }

  function colorize(id, data, nCols, bins, floor, range, transfer) {
    if (!worker) return;
    worker.postMessage({ type: 'colorize', id, data, nCols, nBins: bins, srcFloor: floor, srcRange: range,
      lo: topDb - rangeDb, hi: topDb, colormap }, transfer ? [data.buffer] : []);
  }

  /** Δ bytes (128 = 0 dB, a tile step each) on the Δ map, ±deltaDb. */
  function colorizeDelta(id, data, nCols) {
    if (!worker) return;
    const step = srcRange / 255;
    worker.postMessage({ type: 'colorize', id, data, nCols, nBins: SRC_BANDS, srcFloor: -128 * step, srcRange: 255 * step,
      lo: -deltaDb, hi: deltaDb, colormap: 'diverging' }, [data.buffer]);
  }

  if (worker) {
    worker.onmessage = (evt) => {
      const m = evt.data;
      if (!m || m.type !== 'colorized' || destroyed) return;
      const [kind, gen, at] = String(m.id).split(':');
      if (kind === 'live' || kind === 'liveall') {
        if (+gen !== liveGen || !liveCanvas) return;
        liveCanvas.getContext('2d').putImageData(m.imageData, kind === 'liveall' ? 0 : +at, 0);
        scheduleRedraw();
        return;
      }
      if (kind === 'strip') {
        const [key, x, sh] = at.split('_');
        const st = strips[key];
        if (!st || st.gen !== +gen) return;
        // Columns gone off the strip's left since these were asked for: they
        // land that much further left (a full strip moves on every frame).
        st.canvas.getContext('2d').putImageData(m.imageData, +x - (st.shifted - (+sh || 0)), 0);
        scheduleRedraw();
        return;
      }
      if (+gen !== cGen) return;
      if (kind === 'tile' || kind === 'all') {
        const [src, x] = at.split('_');
        const T = tr[src];
        if (T && T.canvas) T.canvas.getContext('2d').putImageData(m.imageData, kind === 'tile' ? +x : 0, 0);
      } else if (kind === 'z') {
        const t = zTiles.get(at.replaceAll('_', ':'));
        if (t) t.canvas.getContext('2d').putImageData(m.imageData, 0, 0);
      } else if (kind === 'zd') {
        const t = zDelta.get(at.replace('_', ':'));
        if (t) t.canvas.getContext('2d').putImageData(m.imageData, 0, 0);
      } else if (kind === 'dtile' || kind === 'dall') {
        if (delta.canvas) delta.canvas.getContext('2d').putImageData(m.imageData, kind === 'dtile' ? +at : 0, 0);
      }
      scheduleRedraw();
    };
  }

  /** Every colour again (a new map or range). */
  function recolorAll() {
    cGen++;
    for (const T of Object.values(tr)) {
      if (T.bytes && T.canvas) colorize(`all:${cGen}:${T.src}_0`, T.bytes.slice(0), T.cols, T.nBins, srcFloor, srcRange, true);
    }
    for (const t of zTiles.values()) {
      colorize(`z:${cGen}:${t.src}_${t.lod}_${t.i}`, t.bytes.slice(0), t.nCols, t.nBins, srcFloor, srcRange, true);
    }
    zDelta.clear();
    updateZoomDelta();
    // The Δ view compares only what the colour map shows: computed again.
    resetDelta();
    updateDelta();
    if (liveBytes && liveCanvas && liveCount > 0) {
      liveGen++;
      colorize(`liveall:${liveGen}`, liveBytes.slice(0, liveCount * liveBins), liveCount, liveBins, -liveFloor, liveFloor, true);
    }
    // A stream's strips, all they keep (it was only the columns to come).
    for (const key of ['S', 'O', 'D']) recolorStrip(key);
  }

  /** A stream's strip coloured again, every column it keeps. */
  function recolorStrip(key) {
    const st = strips[key];
    if (!st || !st.count) return;
    const id = `strip:${st.gen}:${key}_0_${st.shifted}`;
    const bytes = st.bytes.slice(0, st.count * st.bins);
    if (key === 'D') colorizeDelta(id, bytes, st.count);
    else colorize(id, bytes, st.count, st.bins, -liveFloor, liveFloor, true);
  }

  // ── Geometry ───────────────────────────────────────────────────────────────

  function srcNyq() { return (store.get('_srcRate') || 44100) / 2; }
  function outNyq() { return (store.get('_outRate') || store.get('_srcRate') || 44100) / 2; }
  /** The top of a signal's bands: S and B the source's Nyquist, O its own bands' top. */
  function topOf(T) {
    return T.nBins > SRC_BANDS ? F0 * Math.exp(T.nBins * Math.log(srcNyq() / F0) / SRC_BANDS) : srcNyq();
  }
  function axisNyq() { return Math.max(srcNyq(), outNyq(), tr.O.nBins ? topOf(tr.O) : 0); }
  function fTop() { return fHi > 0 ? Math.min(fHi, axisNyq()) : axisNyq(); }
  function plotW() { return Math.max(1, mainW - COLORBAR_W - RULER_W); }
  function plotH() { return Math.max(1, mainH - TIME_H); }

  /** The plot's panes: one, or S above O. */
  function panes() {
    const ph = plotH();
    if (view !== 'SO') return [{ y0: 0, h: ph, what: view }];
    const hTop = Math.max(1, Math.floor((ph - SPLIT_GAP) / 2));
    return [{ y0: 0, h: hTop, what: 'S' }, { y0: hTop + SPLIT_GAP, h: Math.max(1, ph - SPLIT_GAP - hTop), what: 'O' }];
  }

  function paneAt(y) {
    const ps = panes();
    return ps.find(p => y >= p.y0 && y < p.y0 + p.h) || null;
  }

  function freqToY(hz, h) {
    const s = SCALES[scale];
    const u0 = s.fwd(fLo), u1 = s.fwd(fTop());
    return h - ((s.fwd(hz) - u0) / (u1 - u0)) * h;
  }

  function yToFreq(y, h) {
    const s = SCALES[scale];
    const u0 = s.fwd(fLo), u1 = s.fwd(fTop());
    return s.inv(u0 + (1 - y / h) * (u1 - u0));
  }

  function timeToX(t) {
    return ((t - viewStartS) / Math.max(1e-6, viewEndS - viewStartS)) * plotW();
  }

  function xToTime(x) {
    return viewStartS + (x / plotW()) * (viewEndS - viewStartS);
  }

  /** A whole-track column's duration (s): the same for S, B and O. */
  function colS() {
    const rate = 2 * srcNyq();
    if (specHop > 0) return specHop / rate;
    const T = tr.S;
    return durationS / Math.max(1, T.realCols || T.cols);
  }

  function liveColS() { return liveHop / Math.max(1, 2 * outNyq()); }

  /** Fractional band of `hz` among `bins` bands 20 Hz..top. */
  function bandOf(hz, top, bins) {
    return bins * Math.log(hz / F0) / Math.log(top / F0);
  }

  /**
   * Columns [sx0, sx1) of `src` (bands 20 Hz..top, band 0 at the bottom) into
   * x [dx0, dx1) on the current scale and frequency range, only between fA and fB.
   * Log: one draw; the other scales: a draw per visible band row, rows thinner
   * than a pixel merged upwards.
   */
  function drawBands(c2, src, bins, sx0, sx1, dx0, dx1, top, h, fA = F0, fB = Infinity) {
    const f0 = Math.max(fLo, F0, fA), f1 = Math.min(fTop(), top, fB);
    if (!(f1 > f0) || !(sx1 > sx0) || !(dx1 > dx0)) return;
    if (scale === 'log') {
      const b0 = bandOf(f0, top, bins), b1 = bandOf(f1, top, bins);
      const y0 = freqToY(f1, h), y1 = freqToY(f0, h);
      c2.drawImage(src, sx0, bins - b1, sx1 - sx0, b1 - b0, dx0, y0, dx1 - dx0, y1 - y0);
      return;
    }
    const span = Math.log(top / F0);
    const bStart = Math.max(0, Math.floor(bandOf(f0, top, bins)));
    const bEnd = Math.min(bins, Math.ceil(bandOf(f1, top, bins)));
    let yLow = freqToY(F0 * Math.exp(span * bStart / bins), h);
    for (let b = bStart; b < bEnd; b++) {
      const yHigh = freqToY(F0 * Math.exp(span * (b + 1) / bins), h);
      if (yLow - yHigh < 1 && b < bEnd - 1) continue;
      c2.drawImage(src, sx0, bins - 1 - b, sx1 - sx0, 1, dx0, yHigh, dx1 - dx0, yLow - yHigh);
      yLow = yHigh;
    }
  }

  // ── Toolbar ────────────────────────────────────────────────────────────────

  function button(label, title, onClick) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'an-spec-btn';
    b.textContent = label;
    if (title) b.setAttribute('data-tip', title);
    b.addEventListener('click', onClick);
    return b;
  }

  function select(title, options, onChange) {
    const s = document.createElement('select');
    s.className = 'an-spec-sel';
    if (title) s.setAttribute('data-tip', title);
    for (const [value, text] of options) {
      const o = document.createElement('option');
      o.value = value; o.textContent = text;
      s.appendChild(o);
    }
    s.addEventListener('change', () => onChange(s.value));
    return s;
  }

  function slider(label, min, max, step, get, set, title, unit = 'dB') {
    const wrap = document.createElement('label');
    wrap.className = 'an-spec-range';
    if (title) wrap.setAttribute('data-tip', title);
    const name = document.createElement('span');
    name.textContent = label;
    const input = document.createElement('input');
    input.type = 'range';
    input.min = String(min); input.max = String(max); input.step = String(step);
    input.value = String(get());
    const val = document.createElement('span');
    val.className = 'an-spec-val';
    const show = () => { val.textContent = `${Math.round(get())} ${unit}`; };
    input.addEventListener('input', () => { set(Number(input.value)); show(); });
    show();
    wrap.append(name, input, val);
    return { wrap, input, show };
  }

  const viewSel = select(
    Object.values(VIEWS).map(v => `${v.label}: ${v.tip.replace(/^[^—]*— /, '')}`).join('\n'),
    Object.entries(VIEWS).map(([k, v]) => [k, v.label]),
    (v) => { view = v; persist(); syncToolbar(); requestTiles(); updateDelta(); redraw(); zoomSoon(); });
  // A stream has no B: its views go, and one shown goes to S | O. Its view
  // starts at the live edge; the plot, Follow and Fit say what they do there.
  function onStreamChange(on) {
    for (const o of viewSel.querySelectorAll?.('option') ?? []) {
      if (o.value === 'B' || o.value === 'BS') o.hidden = !!on;
    }
    if (on && (view === 'B' || view === 'BS')) view = 'SO';
    if (!on) resetStrips();
    sview.reset();
    fitBtn.setAttribute('data-tip', on ? STREAM_FIT_TIP : FIT_TIP);
    showStreamTip();
    syncToolbar();
    scheduleRedraw();
  }
  store.on?.('_stream', onStreamChange);

  const scaleBtns = {};
  const scaleGroup = document.createElement('div');
  scaleGroup.className = 'an-spec-seg';
  scaleGroup.setAttribute('data-tip', 'Frequency scale');
  for (const key of Object.keys(SCALES)) {
    scaleBtns[key] = button(SCALES[key].label, null, () => {
      scale = key; persist(); syncToolbar(); redraw();
    });
    scaleGroup.appendChild(scaleBtns[key]);
  }

  let recolorTimer = null;
  const recolorSoon = () => {
    clearTimeout(recolorTimer);
    recolorTimer = setTimeout(() => { recolorAll(); redraw(); }, 60);
  };
  const topCtl = slider('Top', -80, 0, 1, () => topDb, (v) => {
    topDb = v; autoTop = false; persist(); syncToolbar(); recolorSoon();
  }, 'The level at the top of the colour map');
  const rangeCtl = slider('Range', 40, 160, 5, () => rangeDb, (v) => {
    rangeDb = v; persist(); recolorSoon();
  }, 'Levels shown below the top: the rest is black (and, in a Δ view, not compared)');
  const autoBtn = button('Auto', 'Top follows the loudest band of what is shown', () => {
    autoTop = !autoTop; persist();
    if (autoTop) applyAutoTop(true);
    syncToolbar();
  });
  const deltaSel = select('The Δ colour scale: full blue or red at this many dB',
    DELTA_RANGES.map(r => [String(r), `Δ ±${r} dB`]),
    (v) => {
      deltaDb = Number(v); persist();
      // Only the Δ colours change (the worker answers in order: this one lands last).
      if (delta.bytes) colorizeDelta(`dall:${cGen}`, delta.bytes.slice(0), delta.bytes.length / SRC_BANDS);
      for (const t of zDelta.values()) colorizeDelta(`zd:${cGen}:${t.lod}_${t.i}`, t.bytes.slice(0), t.nCols);
      recolorStrip('D');
      redraw();
    });
  const waveCtl = slider('Wave', 0, 100, 5, () => waveAlpha, (v) => { waveAlpha = v; persist(); redraw(); },
    'The waveform over the spectrogram: how opaque (0 = off). S over S, O over O', '%');
  const matchBtn = button('Match', 'Take the whole-track loudness difference (LUFS-I) out: what is left is what changed besides the level', () => {
    matchLvl = !matchLvl; persist(); syncToolbar(); resetDelta(); updateDelta(); redraw();
  });
  const mapSel = select('Colour map', Object.keys(MAP_STOPS).map(k => [k, k[0].toUpperCase() + k.slice(1)]),
    (v) => { colormap = v; persist(); recolorAll(); redraw(); });
  const fitBtn = button('Fit', FIT_TIP, () => fitView());
  // A stream's: back to the live edge, the zoom as it is.
  const followBtn = button('Follow', STREAM_FOLLOW_TIP, () => { sview.followEdge(); redraw(); });
  toolbar.append(viewSel, scaleGroup, topCtl.wrap, rangeCtl.wrap, autoBtn, deltaSel, matchBtn, waveCtl.wrap, mapSel, fitBtn, followBtn);

  const isDelta = () => view === 'OS' || view === 'BS';
  const onStream = () => !!store.get?.('_stream');

  /** Follow says whether the view follows the live edge now. */
  let followShown = null;
  function syncFollow() {
    const f = sview.following;
    if (f === followShown) return;
    followShown = f;
    followBtn.textContent = f ? 'Following' : 'Follow';
    followBtn.classList.toggle('an-spec-btn--on', f);
  }

  /** The plot's tip on a stream: how many minutes its strips keep. */
  function showStreamTip() {
    if (!onStream()) { plot.removeAttribute('data-tip'); return; }
    const n = streamColS > 0 ? Math.max(1, Math.round(STREAM_MAX_COLS * streamColS / 60)) : 3;
    plot.setAttribute('data-tip', STREAM_TIP(n));
  }

  function syncToolbar() {
    const stream = onStream();
    viewSel.value = view;
    for (const [k, b] of Object.entries(scaleBtns)) b.classList.toggle('an-spec-btn--on', k === scale);
    autoBtn.classList.toggle('an-spec-btn--on', autoTop);
    deltaSel.style.display = isDelta() ? '' : 'none';
    deltaSel.value = String(deltaDb);
    // A stream's O − S takes no level out (no whole-track loudness to match),
    // and has no waveform tiles to lay over the plot.
    matchBtn.style.display = isDelta() && !stream ? '' : 'none';
    matchBtn.classList.toggle('an-spec-btn--on', matchLvl);
    followBtn.style.display = stream ? '' : 'none';
    followShown = null;
    syncFollow();
    waveCtl.wrap.style.display = isDelta() || stream ? 'none' : '';
    waveCtl.input.value = String(waveAlpha); waveCtl.show();
    mapSel.value = colormap;
    topCtl.input.value = String(topDb); topCtl.show();
    rangeCtl.input.value = String(rangeDb); rangeCtl.show();
    // The toolbar may take another row now: the plot moves under it.
    if (mainW > 0) { resizeAll(); scheduleRedraw(); }
  }
  syncToolbar();

  /** Auto: the top at the loudest band of what the view shows (5 dB steps). */
  function applyAutoTop(force) {
    if (!autoTop) return;
    let vmax = 0;
    for (const src of VIEWS[view].needs) {
      const b = tr[src].bytes;
      if (!b) continue;
      for (let i = 0; i < b.length; i++) if (b[i] > vmax) vmax = b[i];
    }
    if (vmax === 0) return;
    const db = srcFloor + (vmax / 255) * srcRange;
    const next = Math.min(0, Math.ceil(db / 5) * 5);
    if (force || next !== topDb) {
      topDb = next; syncToolbar(); recolorAll(); redraw();
    }
  }
  let autoTimer = null;
  const autoSoon = () => { clearTimeout(autoTimer); autoTimer = setTimeout(() => applyAutoTop(false), 400); };

  // ── Canvas sizing ──────────────────────────────────────────────────────────

  function sizeCanvas(canvas) {
    const dpr = window.devicePixelRatio || 1;
    const w = plot.clientWidth, h = plot.clientHeight;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    canvas.getContext('2d').setTransform(dpr, 0, 0, dpr, 0, 0);
    return { w, h };
  }

  function resizeAll() {
    plot.style.top = `${Math.max(TOOLBAR_H, toolbar.offsetHeight)}px`;
    const r = sizeCanvas(mainCanvas);
    mainW = r.w; mainH = r.h;
    sizeCanvas(overlayCanvas);
  }

  // ── Whole-track tiles (S, B, O) ────────────────────────────────────────────

  function resetTrack(T) {
    T.gen = -1; T.tileCnt = 0; T.ready = 0; T.nBins = 0; T.cols = 0; T.realCols = 0;
    T.bytes = null; T.canvas = null; T.placed.clear();
    tileCache.dropPrefix?.(`${T.src}-spec-`);
    resetDelta();
    dropZoom(T.src);
    zSrc[T.src] = { gen: null, unavailable: false };
  }

  function decodeSpecTile(bytes) {
    const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (bytes.byteLength < 24 || String.fromCharCode(v.getUint8(0), v.getUint8(1), v.getUint8(2), v.getUint8(3)) !== 'AAST') return null;
    const nCols = v.getUint16(20, true), bins = v.getUint16(22, true);
    if (bytes.byteLength < 24 + nCols * bins) return null;
    return {
      srcFlag: v.getUint8(5), lod: v.getUint8(6), gen8: v.getUint8(7), tileIdx: v.getUint32(8, true),
      floor: v.getFloat32(12, true), range: v.getFloat32(16, true),
      nCols, nBins: bins, data: new Uint8Array(bytes.buffer, bytes.byteOffset + 24, nCols * bins),
    };
  }

  /** A tile's bytes into its signal's track: kept, coloured, placed. */
  function placeTile(T, ti, bytes) {
    if (T.placed.has(ti) || T.tileCnt === 0 || ti >= T.tileCnt) return;
    const t = decodeSpecTile(bytes);
    if (!t || t.nBins === 0) return;
    // A tile of a pass that was replaced (a rack change): asked for again.
    if (T.gen >= 0 && t.gen8 !== (T.gen & 255)) { tileCache.drop?.(`${T.src}-spec-0-${ti}`); return; }
    if (!T.bytes) {
      T.nBins = t.nBins;
      T.cols = T.tileCnt * SPEC_TILE_COLS;
      T.bytes = new Uint8Array(T.cols * T.nBins);
      T.canvas = new OffscreenCanvas(T.cols, T.nBins);
      if (T.src === 'S') { srcFloor = t.floor; srcRange = t.range; }
    }
    if (t.nBins !== T.nBins) return;
    const x = ti * SPEC_TILE_COLS;
    T.bytes.set(t.data, x * T.nBins);
    T.placed.add(ti);
    if (ti === T.tileCnt - 1) T.realCols = x + t.nCols;
    colorize(`tile:${cGen}:${T.src}_${x}`, t.data.slice(0), t.nCols, T.nBins, srcFloor, srcRange, true);
    deltaSoon();
    if (autoTop) autoSoon();
  }

  /** Ask for the tiles the view needs that are done and not placed yet. */
  function requestTiles() {
    for (const src of new Set([...VIEWS[view].needs, 'S'])) {
      const T = tr[src];
      const n = Math.min(T.tileCnt, T.ready);
      for (let ti = 0; ti < n; ti++) {
        if (T.placed.has(ti)) continue;
        const key = `${src}-spec-0-${ti}`;
        const bytes = tileCache.get(key);
        if (bytes) placeTile(T, ti, bytes);
        else tileCache.request(key, src, 'spec', 0, ti);
      }
    }
  }

  // ── The Δ view ─────────────────────────────────────────────────────────────

  function resetDelta() {
    delta.key = ''; delta.bytes = null; delta.canvas = null; delta.placed.clear();
  }

  let deltaTimer = null;
  const deltaSoon = () => { if (!deltaTimer) deltaTimer = setTimeout(() => { deltaTimer = null; updateDelta(); }, 120); };

  /**
   * The level taken out of the Δ view (dB): the whole-track loudness difference of
   * the two signals when Match is on and both are measured, else 0.
   */
  function deltaGain() {
    if (!matchLvl || !isDelta()) return 0;
    const m = store.get('metrics');
    const a = view === 'OS' ? m?.o?.[0] : m?.b?.[0];
    const s = m?.s?.[0];
    return Number.isFinite(a) && Number.isFinite(s) ? a - s : 0;
  }

  /** Δ of every tile placed in both signals and not computed yet. */
  function updateDelta() {
    if (!isDelta()) return;
    const A = view === 'OS' ? tr.O : tr.B, S = tr.S;
    if (!A.bytes || !S.bytes) return;
    const g = deltaGain();
    const gSteps = g / (srcRange / 255);
    const key = `${view}|${A.gen}|${S.gen}|${S.cols}|${g.toFixed(3)}`;
    if (delta.key !== key) {
      delta.key = key;
      delta.bytes = new Uint8Array(S.cols * SRC_BANDS);
      delta.canvas = new OffscreenCanvas(S.cols, SRC_BANDS);
      delta.placed.clear();
    }
    // Levels below the colour map's floor in both are not compared.
    const floor = Math.round(((topDb - rangeDb) - srcFloor) / srcRange * 255);
    for (const ti of S.placed) {
      if (!A.placed.has(ti) || delta.placed.has(ti)) continue;
      const x = ti * SPEC_TILE_COLS;
      const n = ti === S.tileCnt - 1 && S.realCols ? S.realCols - x : SPEC_TILE_COLS;
      const out = new Uint8Array(n * SRC_BANDS);
      for (let c = 0; c < n; c++) {
        const a0 = (x + c) * A.nBins, s0 = (x + c) * S.nBins, o0 = c * SRC_BANDS;
        for (let b = 0; b < SRC_BANDS; b++) {
          const a = A.bytes[a0 + b], s = S.bytes[s0 + b];
          const d = a <= floor && s <= floor ? 0 : Math.round(a - s - gSteps);
          out[o0 + b] = d < -128 ? 0 : d > 127 ? 255 : d + 128;
        }
      }
      delta.bytes.set(out, x * SRC_BANDS);
      delta.placed.add(ti);
      colorizeDelta(`dtile:${cGen}:${x}`, out, n);
    }
  }

  // ── Zoomed tiles (S, B, O) ─────────────────────────────────────────────────

  /** The reference FFT at the source rate (spectra.rs spec_fft_len). */
  function refFft() {
    const rate = 2 * srcNyq();
    let k = 1;
    while (k < Math.ceil(rate / 48000)) k *= 2;
    return 4096 * k;
  }

  /**
   * The signals whose zoomed tiles the view can use now: those it shows whose
   * whole-track tiles are all here (B's and O's zoom comes from their finished
   * passes) and that have a zoom.
   */
  function zoomSources() {
    const needs = view === 'SO' ? ['S', 'O'] : VIEWS[view].needs;
    return needs.filter(src => {
      const T = tr[src];
      return T.bytes && T.tileCnt > 0 && T.ready >= T.tileCnt && !zSrc[src].unavailable;
    });
  }

  /**
   * The zoomed hop the view needs (2^lod source samples): about a column per
   * pixel, no finer than 1/32 of the reference FFT (finer would only
   * interpolate). 0 while the whole-track tiles are within twice of that.
   */
  function wantLod() {
    if (!(specHop > 0) || !(durationS > 0) || !zoomSources().length) return 0;
    const spp = (viewEndS - viewStartS) * 2 * srcNyq() / plotW();
    const lod = Math.max(Math.floor(Math.log2(Math.max(1, spp))), Math.log2(refFft() / 32));
    return (2 ** lod) * 2 > specHop ? 0 : lod;
  }

  /** The tiles at `lod` over the view and a quarter of it either side (≤ 64). */
  function zRange(lod) {
    const rate = 2 * srcNyq();
    const tileS = 64 * (2 ** lod) / rate;
    const pad = (viewEndS - viewStartS) * 0.25;
    const nTiles = Math.ceil(Math.ceil(durationS * rate / 2 ** lod) / 64);
    let i0 = Math.max(0, Math.floor((viewStartS - pad) / tileS));
    let i1 = Math.min(nTiles, Math.ceil((viewEndS + pad) / tileS));
    if (i1 - i0 > 64) {
      const mid = Math.floor((viewStartS + viewEndS) / 2 / tileS);
      i0 = Math.max(0, mid - 32); i1 = Math.min(nTiles, i0 + 64);
    }
    return [i0, i1];
  }

  function zoomSoon(ms = 80) {
    if (zTimer) clearTimeout(zTimer);
    zTimer = setTimeout(pollZoom, ms);
  }

  /** Ask for the zoomed tiles the view lacks, per signal; again while they are being computed. */
  async function pollZoom() {
    zTimer = null;
    const lod = wantLod();
    if (lod !== zoomLod) { zoomLod = lod; zDelta.clear(); redraw(); }
    if (!lod || destroyed) return;
    if (zBusy) { zoomSoon(ZOOM_POLL_MS); return; }
    const sid = store.get('sid');
    if (sid == null) return;
    const [i0, i1] = zRange(lod);
    let more = false;
    zBusy = true;
    for (const src of zoomSources()) {
      let got = 0n, all = true;
      for (let i = i0; i < i1; i++) {
        if (zTiles.has(`${src}:${lod}:${i}`)) got |= 1n << BigInt(i - i0);
        else all = false;
      }
      if (all) continue;
      let status = 0;
      try {
        const r = await fetch(`https://aura.localhost/player/an/specz?sid=${sid}&src=${src}&lod=${lod}&i0=${i0}&i1=${i1}&got=${got.toString(16)}`);
        if (r.ok) status = takeZoomReply(src, await r.arrayBuffer());
      } catch { /* the next poll asks again */ }
      if (destroyed) { zBusy = false; return; }
      if (status === 2) zSrc[src].unavailable = true;
      else more = true;
    }
    zBusy = false;
    updateZoomDelta();
    redraw();
    if (more) zoomSoon(ZOOM_POLL_MS);
  }

  /** An AAZR reply: its tiles kept, coloured and drawn. Returns its status. */
  function takeZoomReply(src, buf) {
    const v = new DataView(buf);
    if (buf.byteLength < 12 || String.fromCharCode(v.getUint8(0), v.getUint8(1), v.getUint8(2), v.getUint8(3)) !== 'AAZR') return 0;
    const status = v.getUint8(5), n = v.getUint16(6, true), gen = v.getUint32(8, true);
    const zs = zSrc[src];
    if (zs.gen !== gen) { dropZoom(src); zs.gen = gen; }
    let off = 12;
    for (let k = 0; k < n && off + 4 <= buf.byteLength; k++) {
      const len = v.getUint32(off, true);
      const t = decodeSpecTile(new Uint8Array(buf, off + 4, len));
      off += 4 + len;
      if (!t || t.nCols === 0 || (tr[src].nBins && t.nBins !== tr[src].nBins)) continue;
      const bytes = t.data.slice(0);
      zTiles.set(`${src}:${t.lod}:${t.tileIdx}`, { src, lod: t.lod, i: t.tileIdx, nCols: t.nCols, nBins: t.nBins, bytes, canvas: new OffscreenCanvas(t.nCols, t.nBins) });
      colorize(`z:${cGen}:${src}_${t.lod}_${t.tileIdx}`, bytes.slice(0), t.nCols, t.nBins, srcFloor, srcRange, true);
    }
    evictZoom();
    return status;
  }

  /** Keep the tiles of the current zoom nearest the view. */
  function evictZoom() {
    if (zTiles.size <= ZOOM_TILES_MAX) return;
    const rate = 2 * srcNyq();
    const mid = (viewStartS + viewEndS) / 2;
    const far = (t) => (t.lod !== zoomLod ? 1e9 : 0) + Math.abs((t.i + 0.5) * 64 * 2 ** t.lod / rate - mid);
    const order = [...zTiles.entries()].sort((a, b) => far(b[1]) - far(a[1]));
    for (const [key] of order.slice(0, zTiles.size - ZOOM_TILES_MAX)) zTiles.delete(key);
  }

  /** One signal's zoomed tiles go (a new pass of it). */
  function dropZoom(src) {
    for (const [k, t] of [...zTiles]) if (t.src === src) zTiles.delete(k);
    zDelta.clear();
  }

  function resetZoom() {
    zTiles.clear(); zDelta.clear(); zoomLod = 0;
    for (const k of Object.keys(zSrc)) zSrc[k] = { gen: null, unavailable: false };
  }

  /** The zoomed tile of `src` and its column under time `t`, if it is here. */
  function zoomAt(src, t) {
    if (!zoomLod) return null;
    const zc = 2 ** zoomLod / (2 * srcNyq());
    const c = Math.floor(t / zc);
    const z = zTiles.get(`${src}:${zoomLod}:${Math.floor(c / 64)}`);
    const k = c - Math.floor(c / 64) * 64;
    return z && k < z.nCols ? { z, k } : null;
  }

  /** The Δ of the zoomed tiles both signals have (the same grid, the same bands below 512). */
  function updateZoomDelta() {
    if (!isDelta() || !zoomLod) return;
    const a = view === 'OS' ? 'O' : 'B';
    const g = deltaGain();
    const key = `${view}|${zSrc[a].gen}|${zSrc.S.gen}|${g.toFixed(3)}|${topDb - rangeDb}`;
    if (zDeltaKey !== key) { zDelta.clear(); zDeltaKey = key; }
    const gSteps = g / (srcRange / 255);
    const floor = Math.round(((topDb - rangeDb) - srcFloor) / srcRange * 255);
    for (const t of zTiles.values()) {
      if (t.src !== 'S' || t.lod !== zoomLod || zDelta.has(`${t.lod}:${t.i}`)) continue;
      const o = zTiles.get(`${a}:${t.lod}:${t.i}`);
      if (!o || o.nCols !== t.nCols) continue;
      const out = new Uint8Array(t.nCols * SRC_BANDS);
      for (let c = 0; c < t.nCols; c++) {
        const a0 = c * o.nBins, s0 = c * t.nBins, o0 = c * SRC_BANDS;
        for (let b = 0; b < SRC_BANDS; b++) {
          const x = o.bytes[a0 + b], s = t.bytes[s0 + b];
          const d = x <= floor && s <= floor ? 0 : Math.round(x - s - gSteps);
          out[o0 + b] = d < -128 ? 0 : d > 127 ? 255 : d + 128;
        }
      }
      zDelta.set(`${t.lod}:${t.i}`, { lod: t.lod, i: t.i, nCols: t.nCols, bytes: out, canvas: new OffscreenCanvas(t.nCols, SRC_BANDS) });
      colorizeDelta(`zd:${cGen}:${t.lod}_${t.i}`, out.slice(0), t.nCols);
    }
  }

  // ── A stream's strips ──────────────────────────────────────────────────────

  function resetStrips() { strips.S = strips.O = strips.D = null; }

  /** What the strips keep: from S's or O's oldest column to the live edge (s). */
  function streamDomain() {
    const hi = streamClock;
    let lo = hi;
    for (const st of [strips.S, strips.O]) if (st && st.count && streamColS > 0) lo = Math.min(lo, st.first * streamColS);
    return { lo: Math.max(0, lo), hi };
  }

  /** The top of a strip's bands: S's the stream's Nyquist, O's on above it at the same step. */
  function stripTop(st) {
    return st.bins > SRC_BANDS ? F0 * Math.exp(st.bins * Math.log(srcNyq() / F0) / SRC_BANDS) : srcNyq();
  }

  /** Columns `cols` (`bins` bytes each) numbered from `first` into strip
   *  `key`, coloured by `paint(id, bytes, n, bins)`. The page asks for the
   *  newest frame (the route gives no other), so a frame can come twice or
   *  not at all: columns held already are left out, a short gap is filled
   *  with `blank` columns; a longer one, an earlier start (another stream)
   *  or another grid starts the strip over. (Every frame missed started it
   *  over: the strips kept seconds, not minutes — found in the window.) */
  function appendStrip(key, cols, bins, first, paint, blank = 0) {
    let n = cols.length;
    if (!n) return;
    let st = strips[key];
    if (st && st.bins === bins && first >= st.first && first < st.first + st.count) {
      const skip = Math.min(n, st.first + st.count - first);
      cols = cols.slice(skip); first += skip; n = cols.length;
      if (!n) return;
    }
    let gap = st && st.bins === bins ? first - (st.first + st.count) : -1;
    if (gap < 0 || gap > STREAM_GAP_MAX) {
      st = strips[key] = { canvas: new OffscreenCanvas(STREAM_MAX_COLS, bins),
        bytes: new Uint8Array(STREAM_MAX_COLS * bins), bins, count: 0, first, gen: ++stripGen, shifted: 0 };
      gap = 0;
    }
    const total = gap + n;
    const over = st.count + total - STREAM_MAX_COLS;
    if (over > 0) {
      st.bytes.copyWithin(0, over * bins, st.count * bins);
      const sc = st.canvas.getContext('2d');
      sc.globalCompositeOperation = 'copy';
      sc.drawImage(st.canvas, -over, 0);
      sc.globalCompositeOperation = 'source-over';
      st.count -= over;
      st.first += over;
      st.shifted += over;
    }
    const buf = new Uint8Array(total * bins);
    if (gap && blank) buf.fill(blank, 0, gap * bins);
    for (let c = 0; c < n; c++) buf.set(cols[c], (gap + c) * bins);
    st.bytes.set(buf, st.count * bins);
    paint(`strip:${st.gen}:${key}_${st.count}_${st.shifted}`, buf, total, bins);
    st.count += total;
  }

  /** A stream frame's columns: S's, O's and, where both have column k, O − S. */
  function appendStream(t) {
    // Another stream (its clock starts again): its view starts at the live edge.
    if (t.clockS < streamClock - 1) sview.reset();
    const colS = t.sCols.hop / Math.max(1, t.srcRate);
    if (colS !== streamColS) { streamColS = colS; showStreamTip(); }
    streamClock = t.clockS;
    const paint = (id, buf, n, bins) => colorize(id, buf, n, bins, -liveFloor, liveFloor, true);
    appendStrip('S', t.sCols.cols, t.sCols.bins, t.sCols.first, paint);
    appendStrip('O', t.oCols.cols, t.oCols.bins, t.oCols.first, paint);
    const k0 = Math.max(t.sCols.first, t.oCols.first);
    const k1 = Math.min(t.sCols.first + t.sCols.cols.length, t.oCols.first + t.oCols.cols.length);
    if (k1 <= k0) return;
    // Both under the colour floor: no change (as the whole-track Δ).
    const floor = Math.round(((topDb - rangeDb) + liveFloor) / liveFloor * 255);
    const dcols = [];
    for (let k = k0; k < k1; k++) {
      const s = t.sCols.cols[k - t.sCols.first], o = t.oCols.cols[k - t.oCols.first];
      const d = new Uint8Array(SRC_BANDS);
      for (let b = 0; b < SRC_BANDS; b++) {
        const v = o[b] <= floor && s[b] <= floor ? 0 : o[b] - s[b];
        d[b] = v < -128 ? 0 : v > 127 ? 255 : v + 128;
      }
      dcols.push(d);
    }
    // (A gap here is "no change": 128.)
    appendStrip('D', dcols, SRC_BANDS, k0, (id, buf, n) => colorizeDelta(id, buf, n), 128);
  }

  /** A stream's strip over the visible time (from `fA` up). */
  function drawStrip(c2, key, h, fA) {
    const st = strips[key];
    if (!st || !st.count || !(streamColS > 0)) return;
    const a = st.first * streamColS, b = a + st.count * streamColS;
    const t0 = Math.max(viewStartS, a), t1 = Math.min(viewEndS, b);
    if (t1 <= t0) return;
    drawBands(c2, st.canvas, st.bins, (t0 - a) / streamColS, (t1 - a) / streamColS, timeToX(t0), timeToX(t1), stripTop(st), h, fA);
  }

  // ── Live O strip ───────────────────────────────────────────────────────────

  function resetLive() {
    liveCanvas = null; liveBytes = null; liveCount = 0; liveGen++;
  }

  function appendLive(cols, bins) {
    const n = cols.length;
    if (!liveCanvas || liveBins !== bins) {
      liveBins = bins;
      liveCanvas = new OffscreenCanvas(LIVE_MAX_COLS, bins);
      liveBytes = new Uint8Array(LIVE_MAX_COLS * bins);
      liveCount = 0;
    }
    const over = liveCount + n - LIVE_MAX_COLS;
    if (over > 0) {
      // Full: keep the newest columns (bytes and colours alike).
      liveBytes.copyWithin(0, over * bins, liveCount * bins);
      const lctx = liveCanvas.getContext('2d');
      lctx.globalCompositeOperation = 'copy';
      lctx.drawImage(liveCanvas, -over, 0);
      lctx.globalCompositeOperation = 'source-over';
      liveCount -= over;
    }
    const buf = new Uint8Array(n * bins);
    for (let c = 0; c < n; c++) buf.set(cols[c], c * bins);
    liveBytes.set(buf, liveCount * bins);
    colorize(`live:${liveGen}:${liveCount}`, buf, n, bins, -liveFloor, liveFloor, true);
    liveCount += n;
  }

  // ── Drawing ────────────────────────────────────────────────────────────────

  let rafPending = false;
  function scheduleRedraw() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(() => { rafPending = false; redraw(); });
  }

  /** A signal's whole-track canvas over the visible time (fA..fB only). */
  function drawTrack(c2, T, h, fA, fB) {
    if (!T.canvas) return;
    const cs = colS();
    const cols = T.realCols || T.cols;
    const t0 = Math.max(viewStartS, 0), t1 = Math.min(viewEndS, cols * cs);
    if (t1 > t0) drawBands(c2, T.canvas, T.nBins, t0 / cs, t1 / cs, timeToX(t0), timeToX(t1), topOf(T), h, fA, fB);
  }

  /** Zoomed tiles (of a signal, or the Δ's) over the whole-track ones, as they come. */
  function drawZoomTiles(c2, tiles, h, fA, fB) {
    if (!zoomLod) return;
    const zc = 2 ** zoomLod / (2 * srcNyq());
    for (const z of tiles) {
      if (z.lod !== zoomLod) continue;
      const bins = z.nBins || SRC_BANDS;
      const top = bins > SRC_BANDS ? F0 * Math.exp(bins * Math.log(srcNyq() / F0) / SRC_BANDS) : srcNyq();
      const a = z.i * 64 * zc, b = a + z.nCols * zc;
      const z0 = Math.max(viewStartS, a), z1 = Math.min(viewEndS, b);
      if (z1 > z0) drawBands(c2, z.canvas, bins, (z0 - a) / zc, (z1 - a) / zc, timeToX(z0), timeToX(z1), top, h, fA, fB);
    }
  }

  const zoomOf = (src) => [...zTiles.values()].filter(t => t.src === src);

  /** S or B, with its zoomed tiles over it. */
  function drawSignal(c2, src, h) {
    drawTrack(c2, tr[src], h);
    drawZoomTiles(c2, zoomOf(src), h);
  }

  /** O: the live columns where the O pass has not been yet, its tiles over them. */
  function drawO(c2, h) {
    if (liveCanvas && liveCount > 0) {
      const lc = liveColS();
      const posS = store.get('_posS') ?? 0;
      const a = posS - liveCount * lc;
      const t0 = Math.max(viewStartS, a), t1 = Math.min(viewEndS, posS);
      if (t1 > t0) drawBands(c2, liveCanvas, liveBins, (t0 - a) / lc, (t1 - a) / lc, timeToX(t0), timeToX(t1), outNyq(), h);
    }
    drawTrack(c2, tr.O, h);
    drawZoomTiles(c2, zoomOf('O'), h);
  }

  function drawDelta(c2, h) {
    if (delta.canvas) {
      const S = tr.S, cs = colS();
      const cols = S.realCols || S.cols;
      const t0 = Math.max(viewStartS, 0), t1 = Math.min(viewEndS, cols * cs);
      if (t1 > t0) drawBands(c2, delta.canvas, SRC_BANDS, t0 / cs, t1 / cs, timeToX(t0), timeToX(t1), srcNyq(), h);
    }
    drawZoomTiles(c2, zDelta.values(), h);
    // Above the source's Nyquist S has nothing to compare: O itself.
    if (view === 'OS' && tr.O.nBins > SRC_BANDS) {
      drawTrack(c2, tr.O, h, srcNyq());
      drawZoomTiles(c2, zoomOf('O'), h, srcNyq());
    }
  }

  /** What a pane still waits for, if anything. */
  function paneStatus(what) {
    const needs = what === 'OS' || what === 'BS' ? VIEWS[what].needs : [what];
    const parts = [];
    for (const src of needs) {
      const T = tr[src];
      if (T.tileCnt === 0) parts.push(src === 'O' ? 'O: waiting for the O pass' : src === 'B' ? 'B: waiting for the B pass' : 'S: analysing');
      else if (T.ready < T.tileCnt) parts.push(`${src}: ${Math.floor(100 * T.ready / T.tileCnt)} %`);
    }
    return parts.join('   ');
  }

  /** A waveform tile's per-entry low and high (both channels), decoded once. */
  function waveTile(src, lod, ti) {
    const key = `${src}-wave-${lod}-${ti}`;
    let d = waveDecoded.get(key);
    if (d) return d;
    const bytes = tileCache.get(key);
    if (!bytes) { tileCache.request(key, src, 'wave', lod, ti); return null; }
    const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (bytes.byteLength < 16 || String.fromCharCode(v.getUint8(0), v.getUint8(1), v.getUint8(2), v.getUint8(3)) !== 'AAWT') return null;
    const ch = v.getUint8(7), n = v.getUint32(12, true);
    if (bytes.byteLength < 16 + n * ch * 12) return null;
    const lo = new Float32Array(n), hi = new Float32Array(n);
    for (let j = 0; j < n; j++) {
      let a = Infinity, b = -Infinity;
      for (let c = 0; c < ch; c++) {
        const off = 16 + (j * ch + c) * 12;
        a = Math.min(a, v.getFloat32(off, true));
        b = Math.max(b, v.getFloat32(off + 4, true));
      }
      lo[j] = a; hi[j] = b;
    }
    d = { lo, hi, n };
    waveDecoded.set(key, d);
    return d;
  }

  /** The waveform of `src` over a pane (±1 over its height), at `waveAlpha`. */
  function drawWave(c2, src, h) {
    const counts = waveCounts[src];
    if (!waveAlpha || !waveLods || !counts || !counts.length || !(counts[0] > 0)) return;
    const rate = 2 * srcNyq();
    const pw = plotW();
    const pxS = (viewEndS - viewStartS) / pw;
    // The coarsest level whose entries are no longer than a pixel.
    let lod = 0;
    for (let k = waveLods - 1; k >= 0; k--) {
      if ((256 * 4 ** k) / rate <= pxS) { lod = k; break; }
    }
    const entryS = (256 * 4 ** lod) / rate;
    const tileS = 256 * entryS;
    const n = counts[lod] || 0;
    const t0 = Math.max(0, viewStartS), t1 = Math.min(durationS, viewEndS);
    const i0 = Math.max(0, Math.floor(t0 / tileS)), i1 = Math.min(n - 1, Math.floor(t1 / tileS));
    const mid = h / 2, amp = h / 2 - 2;
    c2.save();
    c2.globalAlpha = waveAlpha / 100;
    c2.fillStyle = src === 'O' ? '#7dd3fc' : '#e2e8f0';
    c2.beginPath();
    let pts = [];
    const flush = () => {
      if (pts.length < 2) { pts = []; return; }
      c2.moveTo(pts[0][0], mid - pts[0][2] * amp);
      for (const [x, , hi] of pts) c2.lineTo(x, mid - hi * amp);
      for (let k = pts.length - 1; k >= 0; k--) c2.lineTo(pts[k][0], mid - pts[k][1] * amp);
      c2.closePath();
      pts = [];
    };
    for (let ti = i0; ti <= i1; ti++) {
      const d = waveTile(src, lod, ti);
      if (!d) { flush(); continue; }
      for (let j = 0; j < d.n; j++) {
        const t = (ti * 256 + j) * entryS;
        if (t + entryS < viewStartS || t > viewEndS) continue;
        pts.push([timeToX(t + entryS / 2), Math.max(-1, d.lo[j]), Math.min(1, d.hi[j])]);
      }
    }
    flush();
    c2.fill();
    c2.restore();
  }

  function drawPaneLabel(c2, text, y0) {
    c2.font = '10px ui-monospace, Consolas, monospace';
    const tw = c2.measureText(text).width;
    c2.fillStyle = 'rgba(11,25,44,0.8)';
    c2.fillRect(4, y0 + 4, tw + 8, 15);
    c2.fillStyle = C_AXIS;
    c2.fillText(text, 8, y0 + 15);
  }

  function redraw() {
    const c2 = mainCanvas.getContext('2d');
    const w = mainW, h = mainH;
    if (!c2 || !(w > 0 && h > 0)) return;   // a hidden tab has no size
    c2.clearRect(0, 0, w, h);
    c2.fillStyle = 'rgba(11,25,44,0.92)';
    c2.fillRect(0, 0, w, h);
    // A stream: its own view over what the strips keep (stream-view.js).
    const stream = onStream();
    if (stream) {
      const v = sview.view();
      viewStartS = v.t0; viewEndS = v.t1;
      syncFollow();
    }
    if (durationS <= 0 && !stream) {
      c2.fillStyle = C_AXIS;
      c2.font = '11px ui-monospace, Consolas, monospace';
      c2.textAlign = 'center';
      c2.fillText('Not available — no track loaded', w / 2, h / 2);
      c2.textAlign = 'start';
      return;
    }
    const pw = plotW(), ph = plotH();
    // For tests and bug reports: what the view draws from.
    container.dataset.view = view;
    container.dataset.zoomLod = String(zoomLod);
    container.dataset.zoomTiles = String(zTiles.size);
    container.dataset.tiles = ['S', 'B', 'O'].map(k => `${k}${tr[k].placed.size}/${tr[k].tileCnt}`).join(' ');
    container.dataset.time = `${viewStartS.toFixed(3)}..${viewEndS.toFixed(3)}`;
    for (const p of panes()) {
      c2.save();
      c2.translate(0, p.y0);
      c2.beginPath(); c2.rect(0, 0, pw, p.h); c2.clip();
      if (stream) {
        drawStrip(c2, p.what === 'OS' ? 'D' : p.what, p.h);
        // Above the stream's Nyquist S has nothing to compare: O itself.
        if (p.what === 'OS') drawStrip(c2, 'O', p.h, srcNyq());
      } else {
        if (p.what === 'S' || p.what === 'B') drawSignal(c2, p.what, p.h);
        else if (p.what === 'O') drawO(c2, p.h);
        else drawDelta(c2, p.h);
        if (p.what === 'S' || p.what === 'O') drawWave(c2, p.what, p.h);
      }
      if (layers.ultra_tint && fTop() > 24000 && !isDelta()) {
        c2.fillStyle = COLOR_ULTRA;
        c2.fillRect(0, 0, pw, Math.max(0, freqToY(24000, p.h)));
      }
      if (layers.infra_tint && fLo < 20) {
        const y = freqToY(20, p.h);
        c2.fillStyle = COLOR_INFRA;
        c2.fillRect(0, y, pw, Math.max(0, p.h - y));
      }
      // The source's Nyquist, where S and B end.
      const ny = freqToY(srcNyq(), p.h);
      if (fTop() > srcNyq() * 1.01 && ny > 0) {
        c2.strokeStyle = C_NYQ; c2.lineWidth = 1; c2.setLineDash([4, 4]);
        c2.beginPath(); c2.moveTo(0, ny); c2.lineTo(pw, ny); c2.stroke();
        c2.setLineDash([]);
      }
      drawGrid(c2, pw, p.h);
      const status = stream ? '' : paneStatus(p.what);
      const name = view === 'SO' ? p.what : '';
      const g = deltaGain();
      const matched = isDelta() && matchLvl ? (g !== 0 ? `level matched: ${fmtDb(g)} dB` : 'level not matched yet') : '';
      if (name || status || matched) drawPaneLabel(c2, [name, status, matched].filter(Boolean).join('   '), 0);
      c2.restore();
      drawFreqRuler(c2, pw, p.y0, p.h);
    }
    if (view === 'SO') {
      const p = panes()[1];
      c2.fillStyle = C_RULER_BG;
      c2.fillRect(0, p.y0 - SPLIT_GAP, pw + RULER_W, SPLIT_GAP);
    }
    if (isDelta()) drawDeltaBar(c2, w - COLORBAR_W, 0, COLORBAR_W - 2, ph);
    else drawColorbar(c2, w - COLORBAR_W, 0, COLORBAR_W - 2, ph);
    drawTimeRuler(c2, pw, ph, h);
  }

  /** The frequency marks the ruler shows: at least 16 px apart. */
  function rulerMarks(h) {
    const marks = [];
    let lastY = Infinity;
    for (const hz of RULER_HZ) {
      if (hz < fLo || hz > fTop()) continue;
      const y = freqToY(hz, h);
      if (lastY - y < 16) continue;
      lastY = y;
      marks.push({ hz, y });
    }
    return marks;
  }

  function drawGrid(c2, pw, h) {
    c2.strokeStyle = C_GRID;
    c2.lineWidth = 0.5;
    for (const { y } of rulerMarks(h)) {
      c2.beginPath(); c2.moveTo(0, y); c2.lineTo(pw, y); c2.stroke();
    }
  }

  /** The frequency ruler: its own dark strip right of the plot. */
  function drawFreqRuler(c2, pw, y0, h) {
    c2.fillStyle = C_RULER_BG;
    c2.fillRect(pw, y0, RULER_W, h);
    c2.strokeStyle = 'rgba(125,211,252,0.5)';
    c2.lineWidth = 1;
    c2.fillStyle = C_AXIS;
    c2.font = '9px ui-monospace, Consolas, monospace';
    c2.textAlign = 'left';
    c2.textBaseline = 'middle';
    for (const { hz, y } of rulerMarks(h)) {
      c2.beginPath(); c2.moveTo(pw, y0 + y); c2.lineTo(pw + 3, y0 + y); c2.stroke();
      c2.fillText(hz >= 1000 ? `${hz / 1000}k` : `${hz}`, pw + 5, y0 + Math.min(h - 5, Math.max(5, y)));
    }
    c2.textBaseline = 'alphabetic';
  }

  /** Under the plot: the whole track — on a stream, all its strips keep — the
   *  visible part lit, and time marks (a stream's on its own clock). */
  function drawTimeRuler(c2, pw, ph, h) {
    c2.fillStyle = C_RULER_BG;
    c2.fillRect(0, ph, pw + RULER_W, h - ph);
    let A = 0, B = durationS;
    if (onStream()) ({ lo: A, hi: B } = streamDomain());
    if (!(B > A)) return;
    const a = Math.max(0, (viewStartS - A) / (B - A)), b = Math.min(1, (viewEndS - A) / (B - A));
    c2.fillStyle = 'rgba(125,211,252,0.12)';
    c2.fillRect(0, ph, pw, OVERVIEW_H);
    c2.fillStyle = 'rgba(125,211,252,0.6)';
    c2.fillRect(a * pw, ph, Math.max(2, (b - a) * pw), OVERVIEW_H);
    const span = viewEndS - viewStartS;
    const steps = [0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600];
    const step = steps.find(s => (s / span) * pw >= 70) || 600;
    c2.fillStyle = C_AXIS;
    c2.strokeStyle = 'rgba(125,211,252,0.5)';
    c2.font = '9px ui-monospace, Consolas, monospace';
    c2.textAlign = 'center';
    // (None before 0: a stream's view starts there while less than it is kept.)
    for (let t = Math.ceil(Math.max(0, viewStartS) / step) * step; t <= viewEndS + 1e-9; t += step) {
      const x = timeToX(t);
      c2.beginPath(); c2.moveTo(x, ph + OVERVIEW_H); c2.lineTo(x, ph + OVERVIEW_H + 3); c2.stroke();
      const m = Math.floor(t / 60), sec = t - m * 60;
      const txt = step < 1 ? `${m}:${sec.toFixed(step < 0.1 ? 2 : 1).padStart(step < 0.1 ? 5 : 4, '0')}` : `${m}:${String(Math.round(sec)).padStart(2, '0')}`;
      c2.fillText(txt, Math.min(pw - 14, Math.max(14, x)), h - 3);
    }
    c2.textAlign = 'start';
  }

  function gradientBar(c2, x, y, w, h, stops, topText, bottomText, midText) {
    const g = c2.createLinearGradient(x, y + h, x, y);
    stops.forEach((c, i) => g.addColorStop(i / (stops.length - 1), c));
    c2.fillStyle = g;
    c2.fillRect(x, y, w, h);
    c2.strokeStyle = 'rgba(255,255,255,0.3)';
    c2.strokeRect(x, y, w, h);
    c2.save();
    c2.font = '8px ui-monospace, Consolas, monospace';
    c2.textAlign = 'center';
    c2.fillStyle = '#0b192c';
    c2.fillText(topText, x + w / 2, y + 9);
    c2.fillStyle = bottomText.dark ? '#0b192c' : '#e2e8f0';
    c2.fillText(bottomText.text, x + w / 2, y + h - 3);
    if (midText) { c2.fillStyle = '#e2e8f0'; c2.fillText(midText, x + w / 2, y + h / 2 + 3); }
    c2.restore();
  }

  function drawColorbar(c2, x, y, w, h) {
    gradientBar(c2, x, y, w, h, MAP_STOPS[colormap] || MAP_STOPS.inferno,
      `${Math.round(topDb)}`, { text: `${Math.round(topDb - rangeDb)}` });
  }

  function drawDeltaBar(c2, x, y, w, h) {
    gradientBar(c2, x, y, w, h, DELTA_STOPS, `+${deltaDb}`, { text: `−${deltaDb}`, dark: true }, '0');
  }

  // ── Overlay: playhead, cursor, readout ─────────────────────────────────────

  /** A signal's level (dB) at a time and frequency, if its tile is here. */
  function levelAt(T, t, hz) {
    if (!T.bytes || hz < F0 || hz > topOf(T)) return null;
    const zk = zoomAt(T.src, t);
    if (zk) {
      const b = Math.min(T.nBins - 1, Math.floor(bandOf(hz, topOf(T), T.nBins)));
      return srcFloor + (zk.z.bytes[zk.k * T.nBins + b] / 255) * srcRange;
    }
    const c = Math.floor(t / colS());
    if (c < 0 || c >= (T.realCols || T.cols) || !T.placed.has(Math.floor(c / SPEC_TILE_COLS))) return null;
    const b = Math.min(T.nBins - 1, Math.floor(bandOf(hz, topOf(T), T.nBins)));
    return srcFloor + (T.bytes[c * T.nBins + b] / 255) * srcRange;
  }

  /** The live O level (dB) at a time and frequency, if that column was heard. */
  function liveLevelAt(t, hz) {
    if (!liveBytes || liveCount === 0 || hz < F0 || hz > outNyq()) return null;
    const lc = liveColS();
    const posS = store.get('_posS') ?? 0;
    const c = Math.floor((t - (posS - liveCount * lc)) / lc);
    if (c < 0 || c >= liveCount) return null;
    const b = Math.min(liveBins - 1, Math.floor(bandOf(hz, outNyq(), liveBins)));
    return -liveFloor + (liveBytes[c * liveBins + b] / 255) * liveFloor;
  }

  /** A stream's readout: S's and O's levels from their strips, and O − S. */
  function streamReadout(t, hz) {
    const lv = (key) => (strips[key] ? stripLevel(strips[key], streamColS, t, hz, stripTop(strips[key]), liveFloor) : null);
    const vals = { S: lv('S'), O: lv('O') };
    const parts = [];
    for (const k of view === 'S' ? ['S'] : view === 'O' ? ['O'] : ['S', 'O']) {
      if (vals[k] != null) parts.push(`${k} ${vals[k].toFixed(1)} dB`);
    }
    if (view === 'OS' && hz <= srcNyq() && vals.S != null && vals.O != null) parts.push(`Δ ${fmtDb(vals.O - vals.S)} dB`);
    return parts;
  }

  /** The readout's levels for the view at a time and frequency. */
  function readout(t, hz) {
    if (onStream()) return streamReadout(t, hz);
    const parts = [];
    const lv = (src) => {
      let v = levelAt(tr[src], t, hz);
      if (v == null && src === 'O') v = liveLevelAt(t, hz);
      return v;
    };
    const shown = view === 'SO' ? ['S', 'O'] : VIEWS[view].needs;
    const vals = {};
    for (const src of shown) {
      vals[src] = lv(src);
      if (vals[src] != null) parts.push(`${src} ${vals[src].toFixed(1)} dB`);
    }
    if (isDelta() && hz <= srcNyq()) {
      const a = view === 'OS' ? vals.O : vals.B;
      const g = deltaGain();
      if (a != null && vals.S != null) parts.push(`Δ ${fmtDb(a - vals.S - g)} dB${g ? ' (matched)' : ''}`);
    }
    return parts;
  }

  function drawOverlay(cursor) {
    const c2 = overlayCanvas.getContext('2d');
    if (!c2) return;
    const w = mainW, h = mainH, pw = plotW(), ph = plotH();
    c2.clearRect(0, 0, w, h);
    if (durationS <= 0 && !onStream()) return;

    const posS = store.get('_posS');
    if (posS != null) {
      const x = timeToX(posS);
      if (x >= 0 && x <= pw) {
        c2.strokeStyle = C_PLAYHEAD; c2.lineWidth = 1.5;
        c2.beginPath(); c2.moveTo(x, 0); c2.lineTo(x, ph); c2.stroke();
      }
    }
    drawSelection(c2, pw, ph);
    if (!cursor || (cursor.timeS == null && cursor.freqHz == null)) return;
    const own = cursor.sourceViewId === 'spectrogram';
    c2.strokeStyle = C_CURSOR; c2.lineWidth = 1; c2.setLineDash([3, 3]);
    if (cursor.timeS != null) {
      const x = timeToX(cursor.timeS);
      if (x >= 0 && x <= pw) { c2.beginPath(); c2.moveTo(x, 0); c2.lineTo(x, ph); c2.stroke(); }
    }
    if (cursor.freqHz != null && own) {
      // The frequency line in every pane (S | O share the axis).
      for (const p of panes()) {
        const y = p.y0 + freqToY(cursor.freqHz, p.h);
        c2.beginPath(); c2.moveTo(0, y); c2.lineTo(pw, y); c2.stroke();
      }
    }
    c2.setLineDash([]);

    const parts = [];
    if (cursor.freqHz != null && own) parts.push(`${fmtHz(cursor.freqHz)} ${noteOf(cursor.freqHz)}`);
    // (A stream's view may start before 0: no time there.)
    if (cursor.timeS != null && cursor.timeS >= 0) parts.push(fmtTime(cursor.timeS));
    if (cursor.timeS != null && cursor.freqHz != null && own) parts.push(...readout(cursor.timeS, cursor.freqHz));
    if (!parts.length) return;
    const txt = parts.join('   ');
    c2.font = '10px ui-monospace, Consolas, monospace';
    const tw = c2.measureText(txt).width;
    c2.fillStyle = 'rgba(11,25,44,0.85)';
    c2.fillRect(4, ph - 18, tw + 8, 16);
    c2.fillStyle = C_AXIS;
    c2.fillText(txt, 8, ph - 6);
  }

  /** The selected stretch: lit, its edges, its length. */
  function drawSelection(c2, pw, ph) {
    if (selT0 == null || !(selT1 > selT0)) return;
    const x0 = Math.max(0, timeToX(selT0)), x1 = Math.min(pw, timeToX(selT1));
    if (!(x1 > x0 - 1)) return;
    c2.fillStyle = 'rgba(255,255,255,0.10)';
    c2.fillRect(x0, 0, Math.max(1, x1 - x0), ph);
    c2.strokeStyle = 'rgba(255,255,255,0.75)';
    c2.lineWidth = 1;
    for (const t of [selT0, selT1]) {
      const x = timeToX(t);
      if (x >= 0 && x <= pw) { c2.beginPath(); c2.moveTo(x, 0); c2.lineTo(x, ph); c2.stroke(); }
    }
    const len = selT1 - selT0;
    const txt = `${len.toFixed(len < 10 ? 2 : 1)} s`;
    c2.font = '10px ui-monospace, Consolas, monospace';
    const tw = c2.measureText(txt).width;
    const xm = Math.min(pw - tw - 6, Math.max(2, (x0 + x1) / 2 - tw / 2 - 3));
    c2.fillStyle = 'rgba(11,25,44,0.85)';
    c2.fillRect(xm, 2, tw + 6, 14);
    c2.fillStyle = '#e2e8f0';
    c2.fillText(txt, xm + 3, 13);
  }

  function rafLoop() {
    if (destroyed) return;
    drawOverlay(cursor);
    rafHandle = requestAnimationFrame(rafLoop);
  }

  // ── Navigation ─────────────────────────────────────────────────────────────

  const MIN_VIEW_S = 0.25;

  function clampTime() {
    const span = Math.min(Math.max(MIN_VIEW_S, viewEndS - viewStartS), durationS);
    let a = Math.max(0, Math.min(viewStartS, durationS - span));
    viewStartS = a; viewEndS = a + span;
  }

  function clampFreq() {
    const s = SCALES[scale];
    const uMin = s.fwd(F0), uMax = s.fwd(axisNyq());
    let u0 = s.fwd(Math.max(fLo, F0)), u1 = s.fwd(fTop());
    const minSpan = (uMax - uMin) / 200;
    if (u1 - u0 < minSpan) { const c = (u0 + u1) / 2; u0 = c - minSpan / 2; u1 = c + minSpan / 2; }
    if (u0 < uMin) { u1 += uMin - u0; u0 = uMin; }
    if (u1 > uMax) { u0 -= u1 - uMax; u1 = uMax; }
    fLo = s.inv(Math.max(uMin, u0));
    fHi = u1 >= uMax - 1e-9 ? 0 : s.inv(u1);
  }

  function fitView() {
    // A stream: all its strips keep, following the live edge again.
    if (onStream()) sview.all();
    else { viewStartS = 0; viewEndS = durationS; }
    fLo = F0; fHi = 0;
    redraw();
    zoomSoon();
  }

  mainCanvas.addEventListener('wheel', (e) => {
    const stream = onStream();
    if (durationS <= 0 && !stream) return;
    e.preventDefault();
    const rect = mainCanvas.getBoundingClientRect();
    const x = e.clientX - rect.left, y = e.clientY - rect.top;
    const k = e.deltaY > 0 ? 1.25 : 0.8;
    const pw = plotW(), ph = plotH();
    if (e.altKey || x > pw) {
      // Frequency zoom around the cursor's frequency, in the scale's own units.
      const p = paneAt(Math.min(ph - 1, Math.max(0, y))) || panes()[0];
      const s = SCALES[scale];
      const uc = s.fwd(yToFreq(Math.min(p.h, Math.max(0, y - p.y0)), p.h));
      const u0 = s.fwd(fLo), u1 = s.fwd(fTop());
      fLo = s.inv(uc - (uc - u0) * k);
      fHi = s.inv(uc + (u1 - uc) * k);
      clampFreq();
    } else if (stream) {
      // A stream: zoom at the pointer or move (Shift) over what the strips keep.
      const v = sview.view();
      if (e.shiftKey) sview.pan(v, (v.t1 - v.t0) * 0.1 * Math.sign(e.deltaY || e.deltaX));
      else sview.wheel(xToTime(Math.min(pw, Math.max(0, x))), e.deltaY);
    } else if (e.shiftKey) {
      const d = (viewEndS - viewStartS) * 0.1 * Math.sign(e.deltaY || e.deltaX);
      viewStartS += d; viewEndS += d;
      clampTime();
    } else {
      const tc = xToTime(Math.min(pw, Math.max(0, x)));
      viewStartS = tc - (tc - viewStartS) * k;
      viewEndS = tc + (viewEndS - tc) * k;
      clampTime();
    }
    redraw();
    zoomSoon();
  }, { passive: false });

  let drag = null;
  const tAt = (clientX) => {
    const rect = mainCanvas.getBoundingClientRect();
    return Math.max(0, Math.min(durationS, xToTime(Math.min(plotW(), Math.max(0, clientX - rect.left)))));
  };
  mainCanvas.addEventListener('pointerdown', (e) => {
    const stream = onStream();
    if (e.button !== 0 || (durationS <= 0 && !stream)) return;
    mainCanvas.setPointerCapture(e.pointerId);
    // (A stream has no stretch statistics: /selstat measures a whole track.)
    if (e.shiftKey && !stream) {
      // Shift+drag: a stretch for the statistics.
      selecting = { x: e.clientX, t0: tAt(e.clientX) };
      return;
    }
    const s = SCALES[scale];
    const rect = mainCanvas.getBoundingClientRect();
    const p = paneAt(e.clientY - rect.top) || panes()[0];
    drag = { x: e.clientX, y: e.clientY, a: viewStartS, b: viewEndS, u0: s.fwd(fLo), u1: s.fwd(fTop()), h: p.h, zoomedF: fLo > F0 + 1e-6 || fHi > 0, moved: false,
      sv: stream ? sview.view() : null };
  });
  mainCanvas.addEventListener('pointerup', (e) => {
    if (selecting) {
      const t = tAt(e.clientX);
      const wide = Math.abs(e.clientX - selecting.x) >= 3;
      const [a, b] = t < selecting.t0 ? [t, selecting.t0] : [selecting.t0, t];
      selecting = null;
      if (wide && b > a) bus.dispatchEvent(new CustomEvent('an:selection', { detail: { t0: a, t1: b } }));
    } else if (drag && !drag.moved && selT0 != null) {
      // A click (no drag) lets the selection go.
      bus.dispatchEvent(new CustomEvent('an:selection', { detail: null }));
    }
    drag = null;
    try { mainCanvas.releasePointerCapture(e.pointerId); } catch { /* not captured */ }
  });
  mainCanvas.addEventListener('dblclick', () => fitView());

  mainCanvas.addEventListener('pointermove', (e) => {
    const rect = mainCanvas.getBoundingClientRect();
    const x = e.clientX - rect.left, y = e.clientY - rect.top;
    if (selecting) {
      const t = tAt(e.clientX);
      [selT0, selT1] = t < selecting.t0 ? [t, selecting.t0] : [selecting.t0, t];
    } else if (drag) {
      if (Math.abs(e.clientX - drag.x) + Math.abs(e.clientY - drag.y) >= 3) drag.moved = true;
      const dt = ((e.clientX - drag.x) / plotW()) * (drag.b - drag.a);
      if (drag.sv) {
        sview.pan(drag.sv, -dt);
      } else {
        viewStartS = drag.a - dt; viewEndS = drag.b - dt;
        clampTime();
      }
      if (drag.zoomedF) {
        const s = SCALES[scale];
        const du = ((e.clientY - drag.y) / drag.h) * (drag.u1 - drag.u0);
        fLo = s.inv(drag.u0 + du); fHi = s.inv(drag.u1 + du);
        clampFreq();
      }
      redraw();
      zoomSoon();
    }
    const timeS = xToTime(Math.min(plotW(), Math.max(0, x)));
    const p = paneAt(y);
    const freqHz = p ? yToFreq(y - p.y0, p.h) : null;
    cursor = { timeS, freqHz, sourceViewId: 'spectrogram' };
    bus.dispatchEvent(Object.assign(new Event('an:cursor:move'), { detail: { timeS, freqHz, sourceViewId: 'spectrogram' } }));
  });
  mainCanvas.addEventListener('pointerleave', () => {
    cursor = null;
    bus.dispatchEvent(Object.assign(new Event('an:cursor:move'), { detail: { timeS: null, freqHz: null, sourceViewId: 'spectrogram' } }));
  });

  // ── Bus ────────────────────────────────────────────────────────────────────

  const onTrack = (e) => {
    const frame = e.detail?.frame;
    if (!frame || frame.stub) return;
    const info = frame.spec_info;        // [S, B, O] or null (older frames)
    const cnt = (frame.spec_tile_counts_s || [])[0] || 0;
    // The waveform's levels (O's appear when its pass is done).
    const oGen = info ? info[2].gen : '';
    const wsig = `${(frame.wave_tile_counts_s || []).join(',')}|${(frame.wave_tile_counts_o || []).join(',')}|${oGen}`;
    if (wsig !== onTrack.wsig) {
      // A new O pass: its waveform is new too (not the cached one).
      if (onTrack.oGen !== undefined && onTrack.oGen !== oGen) tileCache.dropPrefix?.('O-wave-');
      onTrack.oGen = oGen;
      onTrack.wsig = wsig;
      waveDecoded.clear();
    }
    waveLods = frame.n_lods || 0;
    waveCounts.S = frame.wave_tile_counts_s || [];
    waveCounts.O = frame.wave_tile_counts_o || [];
    const sig = `${frame.duration_s}|${cnt}|${frame.spec_hop || 0}|${frame.src_rate || 0}|${info ? info[0].gen : ''}`;
    durationS = frame.duration_s ?? durationS;
    if (sig !== trackSig) {
      // A new track (or its tiles are known now): every canvas starts over.
      trackSig = sig;
      specHop = frame.spec_hop || 0;
      cGen++;
      for (const T of Object.values(tr)) resetTrack(T);
      resetZoom();
      tr.S.tileCnt = cnt; tr.S.ready = cnt;
      tr.S.gen = info ? info[0].gen : -1;
      if (!(viewEndS > viewStartS) || viewEndS > durationS + 1e-6) { viewStartS = 0; viewEndS = durationS; }
    }
    if (info) {
      for (const [k, T] of [[1, tr.B], [2, tr.O]]) {
        const I = info[k];
        // A new B or O pass (a rack change): its canvas starts over.
        if (I.gen !== T.gen) { resetTrack(T); T.gen = I.gen; }
        // Its tiles span the S grid (O's count is S's).
        T.tileCnt = I.total; T.ready = I.ready;
      }
    }
    // The loudness the Δ view matches may have come (or changed).
    if (isDelta() && delta.key && !delta.key.endsWith(`|${deltaGain().toFixed(3)}`)) resetDelta();
    requestTiles();
    updateDelta();
    updateZoomDelta();
    redraw();
    zoomSoon();
  };

  const onLive = (e) => {
    const frame = e.detail?.frame;
    if (!frame) return;
    liveFloor = frame.spec_floor_neg || liveFloor;
    // A stream: S's and O's own strips (STRM), not the base O columns.
    if (frame.strm) { appendStream(frame.strm); scheduleRedraw(); return; }
    // A jump of the position (a seek) starts the strip over: it is drawn back
    // from the position, so older columns would land in the wrong place.
    const posS = store.get('_posS');
    if (posS != null && livePosS != null && (posS < livePosS - 0.5 || posS > livePosS + 3)) resetLive();
    if (posS != null) livePosS = posS;
    // Log bands, one column per 2^spec_hop_log2 output samples; older frames
    // (hop 0) carried linear bins and are not drawn.
    const cols = frame.spec_cols;
    if (frame.spec_hop_log2 && cols && cols.length > 0) {
      const hop = 1 << frame.spec_hop_log2;
      if (frame.n_fft_bins !== liveBins || hop !== liveHop) { resetLive(); liveHop = hop; }
      appendLive(cols, frame.n_fft_bins);
    }
    scheduleRedraw();
  };

  const onTile = (e) => {
    const key = e.detail?.key;
    if (key && /-wave-/.test(key)) { waveDecoded.delete(key); if (waveAlpha) scheduleRedraw(); return; }
    const m = key && /^([SBO])-spec-0-(\d+)$/.exec(key);
    if (!m) return;
    const bytes = tileCache.get(key) || e.detail.tile;
    if (bytes) placeTile(tr[m[1]], Number(m[2]), bytes);
  };

  const onTrackChange = () => {
    durationS = 0; specHop = 0; trackSig = '';
    viewStartS = 0; viewEndS = 0; fLo = F0; fHi = 0;
    cGen++;
    for (const T of Object.values(tr)) resetTrack(T);
    resetLive(); resetZoom(); resetStrips(); sview.reset(); livePosS = null;
    redraw();
  };

  // The selection (this view's or another's).
  const onSelection = (e) => {
    const d = e.detail;
    selT0 = d ? d.t0 : null;
    selT1 = d ? d.t1 : null;
  };
  const onKey = (e) => {
    if (e.key === 'Escape' && selT0 != null) bus.dispatchEvent(new CustomEvent('an:selection', { detail: null }));
  };
  document.addEventListener('keydown', onKey);

  // Another view's cursor: its time only.
  const onCursor = (e) => {
    const d = e.detail || {};
    if (d.sourceViewId === 'spectrogram') return;
    cursor = d.timeS != null ? { timeS: d.timeS, freqHz: null, sourceViewId: d.sourceViewId } : null;
  };

  bus.addEventListener('an:cursor:move', onCursor);
  bus.addEventListener('an:selection', onSelection);
  bus.addEventListener('an:track', onTrack);
  bus.addEventListener('an:live', onLive);
  bus.addEventListener('an:tile', onTile);
  bus.addEventListener('an:track:change', onTrackChange);

  // ── Init ───────────────────────────────────────────────────────────────────

  resizeAll();
  // Opened while a stream plays: as when it began.
  if (onStream()) onStreamChange(true);
  rafHandle = requestAnimationFrame(rafLoop);

  return {
    setData(payload) {
      if (payload && payload.nLods != null) onTrack({ detail: { frame: payload } });
    },

    setLayer(key, on) {
      layers[key] = on;
      redraw();
    },

    /** view ('S'|'B'|'O'|'SO'|'OS'|'BS'), colormap, scale ('log'|'lin'|'mel'|'bark'), logFreq (old name), top, range, delta */
    setOption(key, value) {
      if (key === 'view' && VIEWS[value]) { view = value; requestTiles(); updateDelta(); }
      else if (key === 'colormap' && MAP_STOPS[value]) { colormap = value; recolorAll(); }
      else if (key === 'scale' && SCALES[value]) { scale = value; }
      else if (key === 'logFreq') { scale = value ? 'log' : 'lin'; }
      else if (key === 'top') { topDb = Number(value); recolorAll(); }
      else if (key === 'range' || key === 'dbFloor') { rangeDb = Math.abs(Number(value)); recolorAll(); }
      else if (key === 'delta' && DELTA_RANGES.includes(Number(value))) { deltaDb = Number(value); recolorAll(); }
      else return;
      persist(); syncToolbar(); redraw(); zoomSoon();
    },

    onResize() {
      resizeAll();
      redraw();
      zoomSoon();
    },

    drawOverlay() {
      drawOverlay(cursor);
    },

    destroy() {
      destroyed = true;
      if (zTimer) clearTimeout(zTimer);
      if (deltaTimer) clearTimeout(deltaTimer);
      if (rafHandle) cancelAnimationFrame(rafHandle);
      if (worker) worker.terminate();
      bus.removeEventListener('an:cursor:move', onCursor);
      bus.removeEventListener('an:selection', onSelection);
      document.removeEventListener('keydown', onKey);
      bus.removeEventListener('an:track', onTrack);
      bus.removeEventListener('an:live', onLive);
      bus.removeEventListener('an:tile', onTile);
      bus.removeEventListener('an:track:change', onTrackChange);
      container.innerHTML = '';
    },
  };
}
