/**
 * histogram-view.js — Loudness distribution and sample amplitude histogram panels.
 *
 * Sub-panels:
 *  1. Short-term loudness distribution (from AAN2 hist_s/hist_b; O binned
 *     here the same way from the O pass's LUFS-S series):
 *     share of the track's 3 s windows per loudness, in % per LU (the
 *     vertical scale), −80…0 LUFS in 256 bins; fitted to where the data is
 *     (wheel: zoom, drag: move, double click: fit again).
 *     P10 and P95 vertical lines (green, labeled with dBFS values).
 *     Loudness gate threshold at −70 LUFS shown as a dashed amber line.
 *     S and B histogram overlaid (B semi-transparent).
 *
 *  2. Sample amplitude histogram (toggle; OFF by default):
 *     Distribution of sample amplitudes from waveform tiles.
 *     S in green, O in sky; ±1.0 clipping rail highlighted in red.
 *     Note: sample histogram built from available mipmap min/max values
 *     (approximation; full sample-level histogram requires raw tiles per SPEC §4.6).
 *
 * A live stream (the radio): S and O of the song playing or of the session
 *  (a switch at the top right), counted by the backend in the same bins
 *  (stream_totals.rs, the /stream JSON's `hist`); ↺ starts the session's
 *  afresh.
 *
 * LAYER_REGISTRY exported for layers.js.
 * Implements FRONTEND-CONTRACT §4 view lifecycle.
 */

// TEXTS-FOR-APPROVAL:
//  "Short-term loudness distribution (LUFS-S, 3s windows)" — panel title tooltip
//  "Sample amplitude distribution" — sub-panel title
//  "P10: −XX.X LUFS" — percentile line label
//  "P95: −XX.X LUFS" — percentile line label
//  "Gate: −70 LUFS" — gate threshold label
//  "Not available — no analysis data yet" — empty state

import { STREAM_TEXTS } from './protocol.js';

// A stream's histogram: the panel's tip, and what it says before the first values.
const STREAM_TIP = 'How much of this song or of the session (its 3 s windows) sits at each loudness, in % per LU: S and O. Wheel: zoom · drag: move · double click: fit to the data again.';
const STREAM_WAIT = 'Measuring…';

/** A stream's histogram counts (the /stream JSON's `hist`) as shares per bin; null when none. */
export function sharesOf(counts) {
  if (!Array.isArray(counts) || !counts.length) return null;
  let n = 0;
  for (const c of counts) n += c > 0 ? c : 0;
  if (!(n > 0)) return null;
  const out = new Float32Array(counts.length);
  for (let i = 0; i < counts.length; i++) out[i] = counts[i] > 0 ? counts[i] / n : 0;
  return out;
}

// ── Layer registry ────────────────────────────────────────────────────────────

export const LAYER_REGISTRY = [
  { id: 'loud_s',   label: 'S loud', colorToken: '--an-s',   defaultOn: true,  shortcut: '1', provenanceAware: false, axisRole: null },
  { id: 'loud_b',   label: 'B loud', colorToken: '--an-b',   defaultOn: false, shortcut: '2', provenanceAware: false, axisRole: null },
  { id: 'loud_o',   label: 'O loud', colorToken: '--an-o',   defaultOn: true,  shortcut: '3', provenanceAware: false, axisRole: null },
  { id: 'amp_hist', label: 'Ampl',   colorToken: '--an-s',   defaultOn: false, shortcut: 'a', provenanceAware: false, axisRole: null },
];

// ── Constants ─────────────────────────────────────────────────────────────────

const LUFS_MIN     = -80;  // bin range start
const LUFS_MAX     =   0;  // bin range end
const N_BINS       = 256;
const LUFS_GATE    = -70;  // absolute gate
const C_S          = '#4ade80';
const C_B          = 'rgba(148,163,184,0.55)';
const C_O          = '#38bdf8';
const BIN_LU       = (LUFS_MAX - LUFS_MIN) / N_BINS;   // 0.3125 LU
const Y_AXIS_W     = 40;   // px: the % scale on the left
const MIN_SPAN_LU  = 6;
const C_P10        = '#6ee7b7';
const C_P95        = '#6ee7b7';
const C_GATE       = '#fbbf24';
const C_AXIS       = '#7dd3fc';
const C_GRID       = 'rgba(255,255,255,0.07)';
const C_CLIP_RAIL  = '#ef4444';
const C_AMP_S      = '#4ade80';
const C_AMP_O      = '#38bdf8';
const PANEL_GAP    = 4;

// ── DPR canvas setup ──────────────────────────────────────────────────────────

function setupCanvas(canvas, container) {
  const dpr = window.devicePixelRatio || 1;
  const w = container.clientWidth;
  const h = container.clientHeight;
  canvas.width  = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  canvas.style.width  = w + 'px';
  canvas.style.height = h + 'px';
  const ctx2d = canvas.getContext('2d');
  ctx2d.scale(dpr, dpr);
  return { ctx2d, w, h, dpr };
}

// ── Percentile helpers ────────────────────────────────────────────────────────

/**
 * Compute nearest-rank percentile from a probability-density histogram.
 * @param {Float32Array} pdf   probability density array, length nBins
 * @param {number} p           percentile in [0,1]
 * @param {number} lufsMin     LUFS value at bin 0
 * @param {number} lufsMax     LUFS value at bin nBins-1
 * @returns {number} LUFS value
 */
function percentileFromPdf(pdf, p, lufsMin, lufsMax) {
  const n = pdf.length;
  let cumulative = 0;
  // Find total mass (may not sum exactly to 1 due to float precision)
  let total = 0;
  for (let i = 0; i < n; i++) total += pdf[i];
  if (total <= 0) return lufsMin;
  const target = p * total;
  for (let i = 0; i < n; i++) {
    cumulative += pdf[i];
    if (cumulative >= target) {
      return lufsMin + (i / n) * (lufsMax - lufsMin);
    }
  }
  return lufsMax;
}

// ── Factory ───────────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget, tileCache: object, cursor: object }} ctx
 */
export function create(container, ctx) {
  const { store, bus, tileCache } = ctx;

  // ── DOM ────────────────────────────────────────────────────────────────────
  container.style.cssText = 'position:relative;overflow:hidden;width:100%;height:100%;display:flex;flex-direction:column;';

  // Loudness histogram panel (top, always visible)
  const loudEl = document.createElement('div');
  loudEl.style.cssText = 'position:relative;width:100%;flex:1;min-height:80px;';
  const loudCanvas    = document.createElement('canvas');
  loudCanvas.style.cssText = 'display:block;width:100%;height:100%;';
  const loudOverlay   = document.createElement('canvas');
  loudOverlay.style.cssText = 'position:absolute;inset:0;pointer-events:none;width:100%;height:100%;';
  loudEl.appendChild(loudCanvas);
  loudEl.appendChild(loudOverlay);

  // A stream: this song or the session (stream_totals.rs counts both).
  let streamWhich = 'song';
  const which = document.createElement('div');
  which.className = 'an-hist-which an-spec-seg';
  which.hidden = true;
  const whichBtns = {};
  for (const [key, label, tip] of [['song', STREAM_TEXTS.song, STREAM_TEXTS.songTip],
                                   ['session', STREAM_TEXTS.session, STREAM_TEXTS.sessionTip]]) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'an-spec-btn';
    b.textContent = label;
    b.setAttribute('data-tip', tip);
    b.addEventListener('click', () => { streamWhich = key; userView = false; showStream(); });
    whichBtns[key] = b;
    which.appendChild(b);
  }
  loudEl.appendChild(which);

  // Amplitude histogram panel (bottom, toggle)
  const ampEl = document.createElement('div');
  ampEl.style.cssText = `position:relative;width:100%;height:0px;overflow:hidden;transition:height 0.2s ease;`;
  const ampCanvas  = document.createElement('canvas');
  ampCanvas.style.cssText = 'display:block;width:100%;height:100%;';
  ampEl.appendChild(ampCanvas);
  container.appendChild(loudEl);
  container.appendChild(ampEl);

  // ── State ──────────────────────────────────────────────────────────────────
  const layers = {};
  for (const def of LAYER_REGISTRY) layers[def.id] = def.defaultOn;

  /** @type {Float32Array | null} */
  let histS = null;
  /** @type {Float32Array | null} */
  let histB = null;
  /** O, binned here from its LUFS-S series (fractions per bin, as hist_s). */
  let histO = null;

  // The LUFS range shown; fitted to the data until the listener zooms.
  let viewLo = LUFS_MIN, viewHi = LUFS_MAX;
  let userView = false;

  /** Sample amplitude histogram: counts per bucket (256 buckets, -1..+1) */
  const AMP_BUCKETS = 256;
  const ampCountS   = new Float32Array(AMP_BUCKETS);
  const ampCountO   = new Float32Array(AMP_BUCKETS);
  let   ampDirty    = false;

  let loudW = 0, loudH = 0;
  let ampW  = 0, ampH  = 0;
  let destroyed = false;
  let rafHandle = null;

  // Cursor state
  let hoverLufs = null;

  // ── Canvas sizing ──────────────────────────────────────────────────────────

  function resizeAll() {
    const r1 = setupCanvas(loudCanvas,   loudEl);
    loudW = r1.w; loudH = r1.h;
    setupCanvas(loudOverlay, loudEl);
    if (layers['amp_hist'] && ampEl.clientHeight > 0) {
      const r2 = setupCanvas(ampCanvas, ampEl);
      ampW = r2.w; ampH = r2.h;
    }
  }

  // ── LUFS coordinate helpers ───────────────────────────────────────────────

  /** Map LUFS value → x pixel position in loudness histogram */
  function lufsToX(lufs, w) {
    return Y_AXIS_W + ((lufs - viewLo) / (viewHi - viewLo)) * (w - Y_AXIS_W);
  }

  function xToLufs(x, w) {
    return viewLo + ((x - Y_AXIS_W) / Math.max(1, w - Y_AXIS_W)) * (viewHi - viewLo);
  }

  /** O's histogram from its LUFS-S series (the backend's binning). */
  function binSeries(series) {
    if (!series || !series.length) return null;
    const bins = new Float32Array(N_BINS);
    let n = 0;
    for (const v of series) {
      if (Number.isFinite(v) && v >= LUFS_MIN && v <= LUFS_MAX) {
        bins[Math.min(N_BINS - 1, Math.floor((v - LUFS_MIN) / (LUFS_MAX - LUFS_MIN) * N_BINS))]++;
        n++;
      }
    }
    if (n === 0) return null;
    for (let i = 0; i < N_BINS; i++) bins[i] /= n;
    return bins;
  }

  /** The histograms shown now. */
  const shown = () => [layers['loud_s'] && histS, layers['loud_b'] && histB, layers['loud_o'] && histO].filter(Boolean);

  /** Fit the range to where the shown data is: from 8 LU below each
   *  histogram's P5 to 3 LU above its P99.8 (a fade-in's quiet tail stays
   *  out of the picture), 12 LU at least. */
  function fitView() {
    let a = Infinity, b = -Infinity;
    for (const hst of shown()) {
      let total = 0;
      for (let i = 0; i < N_BINS; i++) total += hst[i];
      if (total <= 0) continue;
      a = Math.min(a, percentileFromPdf(hst, 0.05, LUFS_MIN, LUFS_MAX) - 8);
      b = Math.max(b, percentileFromPdf(hst, 0.998, LUFS_MIN, LUFS_MAX) + BIN_LU + 3);
    }
    if (!Number.isFinite(a)) { viewLo = LUFS_MIN; viewHi = LUFS_MAX; return; }
    if (b - a < 12) { const c = (a + b) / 2; a = c - 6; b = c + 6; }
    viewLo = Math.max(LUFS_MIN, Math.floor(a));
    viewHi = Math.min(LUFS_MAX, Math.ceil(b));
  }

  function setView(a, b) {
    const span = Math.min(LUFS_MAX - LUFS_MIN, Math.max(MIN_SPAN_LU, b - a));
    let lo = b - a < MIN_SPAN_LU ? (a + b) / 2 - span / 2 : a;
    lo = Math.max(LUFS_MIN, Math.min(LUFS_MAX - span, lo));
    viewLo = lo; viewHi = lo + span;
    userView = true;
    drawLoudness();
  }

  /** A step for about five labels on the % axis. */
  function niceStep(max) {
    const raw = max / 5;
    const p = Math.pow(10, Math.floor(Math.log10(raw)));
    for (const m of [1, 2, 2.5, 5, 10]) if (raw <= m * p) return m * p;
    return 10 * p;
  }

  // ── Loudness draw ─────────────────────────────────────────────────────────

  function drawLoudness() {
    const ctx2d = loudCanvas.getContext('2d');
    if (!ctx2d) return;
    const w = loudW, h = loudH;
    ctx2d.clearRect(0, 0, w, h);
    ctx2d.fillStyle = 'rgba(15,23,42,0.7)';
    ctx2d.fillRect(0, 0, w, h);

    const AXIS_H = 18; // px for LUFS axis labels at bottom
    const plotH  = h - AXIS_H;

    // Grid: a line every 10 LU, every 5 or 2 when zoomed in
    const span = viewHi - viewLo;
    const xStep = span > 40 ? 10 : span > 16 ? 5 : span > 8 ? 2 : 1;
    ctx2d.strokeStyle = C_GRID;
    ctx2d.lineWidth = 0.5;
    for (let lufs = Math.ceil(viewLo / xStep) * xStep; lufs <= viewHi; lufs += xStep) {
      const x = lufsToX(lufs, w);
      ctx2d.beginPath(); ctx2d.moveTo(x, 0); ctx2d.lineTo(x, plotH); ctx2d.stroke();
    }

    // (A stream's frame sends no bins: an empty array is none, not a histogram
    // — the plot stood empty with the gate lines and no word.)
    const has = (x) => !!x && x.length > 0;
    if (!has(histS) && !has(histB) && !has(histO)) {
      ctx2d.fillStyle = C_AXIS;
      ctx2d.font = '11px ui-monospace, Consolas, monospace';
      ctx2d.textAlign = 'center';
      // A stream's song or session counts once its first 3 s are heard.
      ctx2d.fillText(store.get && store.get('_stream') ? STREAM_WAIT : 'Not available — no analysis data yet', w / 2, h / 2);
      ctx2d.textAlign = 'start';
      return;
    }

    // The vertical scale: % of the windows per LU; its top a step above the
    // highest bar shown.
    let maxDensity = 0;
    for (const hst of shown()) for (let i = 0; i < N_BINS; i++) { if (hst[i] > maxDensity) maxDensity = hst[i]; }
    if (maxDensity <= 0) maxDensity = 1;
    const pctPerLu = (f) => f / BIN_LU * 100;
    const yStep = niceStep(pctPerLu(maxDensity));
    const yTop = Math.ceil(pctPerLu(maxDensity) / yStep) * yStep;
    maxDensity = yTop / 100 * BIN_LU;
    ctx2d.font = '9px ui-monospace, Consolas, monospace';
    ctx2d.textAlign = 'right';
    ctx2d.textBaseline = 'middle';
    for (let v = 0; v <= yTop + 1e-9; v += yStep) {
      const y = plotH - (v / yTop) * (plotH - 4);
      ctx2d.strokeStyle = C_GRID;
      ctx2d.beginPath(); ctx2d.moveTo(Y_AXIS_W, y); ctx2d.lineTo(w, y); ctx2d.stroke();
      ctx2d.fillStyle = C_AXIS;
      ctx2d.fillText(yStep < 1 ? v.toFixed(1) : v.toFixed(0), Y_AXIS_W - 5, Math.max(6, y));
    }
    ctx2d.save();
    ctx2d.translate(9, plotH / 2);
    ctx2d.rotate(-Math.PI / 2);
    ctx2d.textAlign = 'center';
    ctx2d.fillText('% per LU', 0, 0);
    ctx2d.restore();
    ctx2d.textAlign = 'start';
    ctx2d.textBaseline = 'alphabetic';
    ctx2d.save();
    ctx2d.beginPath();
    ctx2d.rect(Y_AXIS_W, 0, w - Y_AXIS_W, h);
    ctx2d.clip();

    const binW = (w - Y_AXIS_W) / ((viewHi - viewLo) / BIN_LU);
    const binX = (i) => lufsToX(LUFS_MIN + i * BIN_LU, w);

    // Draw B histogram (behind S)
    if (layers['loud_b'] && histB) {
      ctx2d.fillStyle = C_B;
      for (let i = 0; i < N_BINS; i++) {
        const barH = (histB[i] / maxDensity) * (plotH - 4);
        ctx2d.fillRect(binX(i), plotH - barH, Math.max(1, binW), barH);
      }
    }

    // O: an outline over S (both at once stay readable)
    if (layers['loud_o'] && histO) {
      ctx2d.fillStyle = C_O + '38';
      for (let i = 0; i < N_BINS; i++) {
        const barH = (histO[i] / maxDensity) * (plotH - 4);
        ctx2d.fillRect(binX(i), plotH - barH, Math.max(1, binW), barH);
      }
    }

    // Draw S histogram
    if (layers['loud_s'] && histS) {
      ctx2d.fillStyle = C_S + (layers['loud_o'] && histO ? '70' : 'b0');
      for (let i = 0; i < N_BINS; i++) {
        const barH = (histS[i] / maxDensity) * (plotH - 4);
        ctx2d.fillRect(binX(i), plotH - barH, Math.max(1, binW), barH);
      }
      // Outline
      ctx2d.strokeStyle = C_S;
      ctx2d.lineWidth = 1;
      ctx2d.beginPath();
      for (let i = 0; i < N_BINS; i++) {
        const barH = (histS[i] / maxDensity) * (plotH - 4);
        const x = binX(i);
        if (i === 0) ctx2d.moveTo(x, plotH - barH);
        else ctx2d.lineTo(x, plotH - barH);
      }
      ctx2d.stroke();

      // P10 and P95 lines
      const p10 = percentileFromPdf(histS, 0.10, LUFS_MIN, LUFS_MAX);
      const p95 = percentileFromPdf(histS, 0.95, LUFS_MIN, LUFS_MAX);

      ctx2d.strokeStyle = C_P10;
      ctx2d.lineWidth = 1.5;
      ctx2d.setLineDash([4, 3]);
      // (Under the stream's switch when it shows.)
      const yOff = which.hidden ? 0 : 20;
      for (const [lufs, label, ly] of [[p10, `P10 ${p10.toFixed(1)}`, 14 + yOff], [p95, `P95 ${p95.toFixed(1)}`, 26 + yOff]]) {
        const x = lufsToX(lufs, w);
        ctx2d.beginPath();
        ctx2d.moveTo(x, 0); ctx2d.lineTo(x, plotH);
        ctx2d.stroke();
        ctx2d.fillStyle = C_P10;
        ctx2d.font = '9px ui-monospace, Consolas, monospace';
        // Flip label to the left of the line when near the right edge
        const textW = ctx2d.measureText(label).width;
        const labelX = (x + 4 + textW > w - 2) ? x - textW - 4 : x + 2;
        ctx2d.fillText(label.replace('-', '−'), labelX, ly);
      }
      ctx2d.setLineDash([]);
    }
    if (layers['loud_o'] && histO) {
      ctx2d.strokeStyle = C_O;
      ctx2d.lineWidth = 1.2;
      ctx2d.beginPath();
      for (let i = 0; i < N_BINS; i++) {
        const y = plotH - (histO[i] / maxDensity) * (plotH - 4);
        if (i === 0) ctx2d.moveTo(binX(i), y); else ctx2d.lineTo(binX(i), y);
      }
      ctx2d.stroke();
    }

    // Gate threshold line
    {
      const x = lufsToX(LUFS_GATE, w);
      ctx2d.strokeStyle = C_GATE;
      ctx2d.lineWidth = 1;
      ctx2d.setLineDash([3, 3]);
      ctx2d.beginPath();
      ctx2d.moveTo(x, 0); ctx2d.lineTo(x, plotH);
      ctx2d.stroke();
      ctx2d.setLineDash([]);
      ctx2d.fillStyle = C_GATE;
      ctx2d.font = '9px ui-monospace, Consolas, monospace';
      ctx2d.fillText('Gate', x + 2, plotH - 4);
    }
    ctx2d.restore();

    // LUFS axis labels (the unit at the right end, clear of the numbers)
    ctx2d.fillStyle = C_AXIS;
    ctx2d.font = '9px ui-monospace, Consolas, monospace';
    ctx2d.textAlign = 'center';
    for (let lufs = Math.ceil(viewLo / xStep) * xStep; lufs <= viewHi; lufs += xStep) {
      const x = lufsToX(lufs, w);
      if (x > w - 40) continue;
      ctx2d.fillText(`${lufs}`.replace('-', '\u2212'), x, h - 3);
    }
    ctx2d.textAlign = 'right';
    ctx2d.fillText('LUFS-S', w - 3, h - 3);
    ctx2d.textAlign = 'start';
  }

  // ── Amplitude draw ────────────────────────────────────────────────────────

  function drawAmplitude() {
    if (!layers['amp_hist']) return;
    const ctx2d = ampCanvas.getContext('2d');
    if (!ctx2d) return;
    const w = ampW, h = ampH;
    if (w === 0 || h === 0) return;
    ctx2d.clearRect(0, 0, w, h);
    ctx2d.fillStyle = 'rgba(15,23,42,0.7)';
    ctx2d.fillRect(0, 0, w, h);

    const AXIS_H = 16;
    const plotH  = h - AXIS_H;
    let maxCount = 0;
    for (let i = 0; i < AMP_BUCKETS; i++) {
      if (ampCountS[i] > maxCount) maxCount = ampCountS[i];
      if (ampCountO[i] > maxCount) maxCount = ampCountO[i];
    }
    if (maxCount <= 0) {
      ctx2d.fillStyle = C_AXIS;
      ctx2d.font = '10px ui-monospace, Consolas, monospace';
      ctx2d.textAlign = 'center';
      ctx2d.fillText('No amplitude data yet', w / 2, h / 2);
      ctx2d.textAlign = 'start';
      return;
    }

    const binW = w / AMP_BUCKETS;

    // S (green)
    ctx2d.fillStyle = C_AMP_S + 'a0';
    for (let i = 0; i < AMP_BUCKETS; i++) {
      const barH = (ampCountS[i] / maxCount) * plotH;
      ctx2d.fillRect(i * binW, plotH - barH, Math.max(1, binW), barH);
    }
    // O (sky)
    ctx2d.fillStyle = C_AMP_O + '80';
    for (let i = 0; i < AMP_BUCKETS; i++) {
      const barH = (ampCountO[i] / maxCount) * plotH;
      ctx2d.fillRect(i * binW, plotH - barH, Math.max(1, binW), barH);
    }

    // ±1.0 clipping rails in red
    const clipX0 = 0;
    const clipX1 = w;
    ctx2d.strokeStyle = C_CLIP_RAIL;
    ctx2d.lineWidth = 1.5;
    // Bucket for +1.0 and -1.0 (approximately)
    const clipHi = Math.round((1.0 + 1.0) / 2.0 * AMP_BUCKETS) - 1; // index for +1.0
    const clipLo = 0;
    for (const bIdx of [clipLo, clipHi]) {
      const x = bIdx * binW;
      ctx2d.beginPath();
      ctx2d.moveTo(x, 0); ctx2d.lineTo(x, plotH);
      ctx2d.stroke();
    }

    // Axis labels
    ctx2d.fillStyle = C_AXIS;
    ctx2d.font = '9px ui-monospace, Consolas, monospace';
    ctx2d.textAlign = 'center';
    for (const [amp, label] of [[-1.0, '-1.0'], [-0.5, '-0.5'], [0, '0'], [0.5, '+0.5'], [1.0, '+1.0']]) {
      const i = Math.round((amp + 1.0) / 2.0 * AMP_BUCKETS);
      const x = i * binW;
      ctx2d.fillText(label, x, h - 2);
    }
    ctx2d.textAlign = 'start';
  }

  // ── Overlay ────────────────────────────────────────────────────────────────

  function drawOverlay(cursor) {
    const ctx2d = loudOverlay.getContext('2d');
    if (!ctx2d) return;
    const w = loudW, h = loudH;
    ctx2d.clearRect(0, 0, w, h);
    if (hoverLufs !== null) {
      const x = lufsToX(hoverLufs, w);
      ctx2d.strokeStyle = 'rgba(255,255,255,0.6)';
      ctx2d.lineWidth = 1;
      ctx2d.setLineDash([3, 3]);
      ctx2d.beginPath();
      ctx2d.moveTo(x, 0); ctx2d.lineTo(x, h - 18);
      ctx2d.stroke();
      ctx2d.setLineDash([]);
      ctx2d.fillStyle = 'rgba(11,25,44,0.85)';
      ctx2d.font = '10px ui-monospace, Consolas, monospace';
      const bin = Math.floor((hoverLufs - LUFS_MIN) / BIN_LU);
      const pct = (hst) => (hst && bin >= 0 && bin < N_BINS ? (hst[bin] / BIN_LU * 100).toFixed(1) + '%/LU' : null);
      const parts = [`${hoverLufs.toFixed(1)} LUFS`];
      if (layers['loud_s'] && pct(histS)) parts.push('S ' + pct(histS));
      if (layers['loud_o'] && pct(histO)) parts.push('O ' + pct(histO));
      if (layers['loud_b'] && pct(histB)) parts.push('B ' + pct(histB));
      const label = parts.join(' · ');
      const tw = ctx2d.measureText(label).width;
      const lx = x + 4 + tw + 8 > w ? x - tw - 12 : x + 4;
      const ly = which.hidden ? 4 : 24;   // under the stream's switch
      ctx2d.fillRect(lx, ly, tw + 8, 16);
      ctx2d.fillStyle = C_AXIS;
      ctx2d.fillText(label, lx + 4, ly + 12);
    }
  }

  // ── RAF ───────────────────────────────────────────────────────────────────

  function rafLoop() {
    if (destroyed) return;
    drawOverlay(store.cursor || {});
    rafHandle = requestAnimationFrame(rafLoop);
  }

  // ── Event handlers ────────────────────────────────────────────────────────

  const onTrack = (e) => {
    const frame = e.detail?.frame;
    // (A stream's whole-track frames have no bins: its own come with its totals.)
    if (!frame || store.get?.('_stream')) return;
    histS = frame.hist_s ?? null;
    histB = frame.hist_b ?? null;
    histO = binSeries(frame.lufs_s_series_o);
    if (!userView) fitView();
    // Build amplitude histogram from waveform tile cache (approximation from mipmap)
    ampCountS.fill(0);
    ampCountO.fill(0);
    ampDirty = false;
    drawLoudness();
    drawAmplitude();
  };

  const onTile = (e) => {
    const { key, tile } = e.detail || {};
    if (!key || !tile) return;
    // Accumulate amplitude counts from wave tiles
    if (!key.includes('-wave-')) return;
    const src = key.startsWith('S') ? 'S' : key.startsWith('O') ? 'O' : null;
    if (!src) return;
    const view = new DataView(tile.buffer, tile.byteOffset, tile.byteLength);
    if (tile.length < 16) return;
    const magic = String.fromCharCode(view.getUint8(0), view.getUint8(1), view.getUint8(2), view.getUint8(3));
    if (magic !== 'AAWT') return;
    const ch   = view.getUint8(7);
    const nSmp = view.getUint32(12, true);
    const target = src === 'S' ? ampCountS : ampCountO;
    let off = 16;
    for (let k = 0; k < nSmp * ch; k++) {
      const minV = view.getFloat32(off, true); off += 4;
      const maxV = view.getFloat32(off, true); off += 4;
      off += 4; // skip rms
      const useV = (Math.abs(minV) > Math.abs(maxV)) ? minV : maxV;
      const bucketIdx = Math.max(0, Math.min(AMP_BUCKETS - 1, Math.round((useV + 1.0) / 2.0 * (AMP_BUCKETS - 1))));
      target[bucketIdx]++;
    }
    ampDirty = true;
    if (layers['amp_hist']) drawAmplitude();
  };

  const onTrackChange = () => {
    histS = null; histB = null; histO = null;
    userView = false; viewLo = LUFS_MIN; viewHi = LUFS_MAX;
    ampCountS.fill(0); ampCountO.fill(0);
    const ctx2d = loudCanvas.getContext('2d');
    if (ctx2d) ctx2d.clearRect(0, 0, loudW, loudH);
    const actx = ampCanvas.getContext('2d');
    if (actx) actx.clearRect(0, 0, ampW, ampH);
  };

  bus.addEventListener('an:track', onTrack);
  bus.addEventListener('an:tile',  onTile);
  bus.addEventListener('an:track:change', onTrackChange);

  // ── Interaction ───────────────────────────────────────────────────────────

  loudEl.dataset.tip = 'How much of the track (its 3 s windows) sits at each loudness, in % per LU: S, O and B. Wheel: zoom · drag: move · double click: fit to the data again.';
  let lDrag = null;
  loudCanvas.addEventListener('mousemove', (e) => {
    const rect = loudCanvas.getBoundingClientRect();
    const x = e.clientX - rect.left;
    hoverLufs = x >= Y_AXIS_W ? xToLufs(x, loudW) : null;
    loudCanvas.style.cursor = lDrag ? 'grabbing' : 'grab';
  });
  loudCanvas.addEventListener('mouseleave', () => { hoverLufs = null; });
  loudCanvas.addEventListener('wheel', (e) => {
    e.preventDefault();
    const rect = loudCanvas.getBoundingClientRect();
    const c = xToLufs(Math.max(Y_AXIS_W, e.clientX - rect.left), loudW);
    const k = e.deltaY > 0 ? 1.25 : 0.8;
    setView(c - (c - viewLo) * k, c + (viewHi - c) * k);
  }, { passive: false });
  loudCanvas.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    lDrag = { x: e.clientX, lo: viewLo, hi: viewHi };
    loudCanvas.setPointerCapture(e.pointerId);
  });
  loudCanvas.addEventListener('pointermove', (e) => {
    if (!lDrag) return;
    const d = -(e.clientX - lDrag.x) / Math.max(1, loudW - Y_AXIS_W) * (lDrag.hi - lDrag.lo);
    setView(lDrag.lo + d, lDrag.hi + d);
  });
  const lEnd = (e) => { lDrag = null; try { loudCanvas.releasePointerCapture(e.pointerId); } catch { /* not captured */ } };
  loudCanvas.addEventListener('pointerup', lEnd);
  loudCanvas.addEventListener('pointercancel', lEnd);
  loudCanvas.addEventListener('dblclick', () => { userView = false; fitView(); drawLoudness(); });

  // ── A stream: the song's or the session's (the /stream JSON) ──────────────

  /** The stream's histograms from its last totals: the song's or the session's. */
  function showStream() {
    for (const [k, b] of Object.entries(whichBtns)) b.classList.toggle('an-spec-btn--on', k === streamWhich);
    const d = store.get?.('_streamTotals')?.[streamWhich];
    histS = sharesOf(d?.s?.hist);
    histO = sharesOf(d?.o?.hist);
    histB = null;
    if (!userView) fitView();
    drawLoudness();
  }

  const FILE_TIP = loudEl.dataset.tip;
  function onStreamChange(on) {
    which.hidden = !on;
    loudEl.dataset.tip = on ? STREAM_TIP : FILE_TIP;
    // A file's bins come with its next whole-track frame.
    histS = histB = histO = null;
    userView = false; viewLo = LUFS_MIN; viewHi = LUFS_MAX;
    if (on) showStream(); else drawLoudness();
  }
  store.on?.('_stream', onStreamChange);
  store.on?.('_streamTotals', () => { if (store.get?.('_stream')) showStream(); });
  if (store.get?.('_stream')) onStreamChange(true);

  // ── Init ──────────────────────────────────────────────────────────────────

  resizeAll();
  rafHandle = requestAnimationFrame(rafLoop);

  // ── Public interface ──────────────────────────────────────────────────────

  return {
    setData(payload) {
      if (payload && (payload.histS || payload.histB)) {
        onTrack({ detail: { frame: payload } });
      }
    },

    setLayer(key, on) {
      layers[key] = on;
      if (!userView) fitView();
      if (key === 'amp_hist') {
        if (on) {
          ampEl.style.height = '120px';
          resizeAll();
          drawAmplitude();
        } else {
          ampEl.style.height = '0px';
        }
      }
      drawLoudness();
    },

    onResize(w, h) {
      resizeAll();
      drawLoudness();
      if (layers['amp_hist']) drawAmplitude();
    },

    drawOverlay(cursor) {
      drawOverlay(cursor);
    },

    destroy() {
      destroyed = true;
      if (rafHandle) cancelAnimationFrame(rafHandle);
      bus.removeEventListener('an:track', onTrack);
      bus.removeEventListener('an:tile',  onTile);
      bus.removeEventListener('an:track:change', onTrackChange);
      container.innerHTML = '';
    },
  };
}
