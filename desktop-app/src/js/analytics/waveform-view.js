/**
 * waveform-view.js — Waveform mipmap panel with deep-zoom sinc reconstruction.
 *
 * Features:
 *  - 24 px minimap strip (full track, always visible): drag the window to
 *    move, drag its edges to zoom in time, click elsewhere to jump there,
 *    double click for the whole track
 *  - Detail view: wheel = time zoom at the pointer, Shift+wheel = move, drag =
 *    move, wheel over the dB scale (or Alt+wheel, or drag on the scale) =
 *    amplitude zoom, double click = whole track and full scale (on the scale:
 *    full scale only)
 *  - Follow (on by default, while there is a playhead): zoomed in, the view
 *    keeps the playhead in its middle and scrolls with it, the wheel zooming
 *    about it; moving the view by hand (drag, Shift+wheel, the minimap)
 *    pauses it; the Follow button or a double click takes it up again
 *  - Detail view with tile LODs: min/max/rms per sample block; closer than
 *    the tiles reach (an entry per 256 samples), the `wavez` route gives the
 *    samples' own min/max per pixel, down to the samples themselves
 *  - Deep zoom (>= 4 px/sample): draws individual sample dots + band-limited
 *    reconstruction curve via 16x windowed-sinc interpolation
 *  - Inter-sample overs above 0 dBFS highlighted in red
 *  - Clip markers (AAN2 clip events)
 *  - DR top-20% block spans
 *  - Loudness gate spans
 *  - S and O tracks
 *  - Cursor/playhead overlay at 60 fps
 *
 * Implements the standard view lifecycle (FRONTEND-CONTRACT §4):
 *   create(container, ctx) → { setData, setLayer, onResize, drawOverlay, destroy }
 *
 * LAYER_REGISTRY exported for layers.js (FRONTEND-CONTRACT §5).
 */

import { createStreamWave, labelAt } from './stream-wave.js';
import { formatTime } from './format.js';

// A stream's waveform (stream-wave.js draws it while a stream plays).
const STREAM_TIP = "The stream's waveform, S and O, as it played: the last 30 minutes are kept. Wheel: zoom · drag: move · double click: all of it · Follow: back to the live edge.";
const STREAM_MINI_TIP = 'All of the stream kept, up to 30 minutes. Drag the lit window to move, drag its edges to zoom; a click elsewhere moves it there; double click: all of it, following the live edge again.';

// ── Layer registry ────────────────────────────────────────────────────────────

export const LAYER_REGISTRY = [
  { id: 's_mip',    label: 'S',       colorToken: '--an-s',   defaultOn: true,  shortcut: '1', provenanceAware: false, axisRole: null },
  { id: 's_rms',    label: 'S rms',   colorToken: '--an-s',   defaultOn: false, shortcut: 'r', provenanceAware: false, axisRole: null },
  { id: 'o_mip',    label: 'O',       colorToken: '--an-o',   defaultOn: true,  shortcut: '3', provenanceAware: false, axisRole: null },
  { id: 'clips',    label: 'Clips',   colorToken: '--an-clip', defaultOn: true,  shortcut: 'c', provenanceAware: false, axisRole: null },
  { id: 'overs',    label: 'Overs',   colorToken: '--an-over', defaultOn: true,  shortcut: 'p', provenanceAware: false, axisRole: null },
  { id: 'dr_blocks',label: 'DR 20%',  colorToken: '--an-dr',  defaultOn: false, shortcut: 'd', provenanceAware: false, axisRole: null },
  { id: 'gate_spans',label: 'Gate',   colorToken: '--an-gate', defaultOn: false, shortcut: 'g', provenanceAware: false, axisRole: null },
];

// ── Sinc kernel (16×, 8-tap Lanczos) ─────────────────────────────────────────

/** Lanczos window: sinc(x) * sinc(x/a), |x| < a (here a=4 taps each side). */
function lanczos(x, a) {
  if (x === 0) return 1;
  if (Math.abs(x) >= a) return 0;
  const px = Math.PI * x;
  return (Math.sin(px) / px) * (Math.sin(px / a) / (px / a));
}

/**
 * Precompute 16 sub-sample sinc kernel weights for 8-tap Lanczos (4 taps each side).
 * sincKernels[s][t]  s in [0,15], t in [0,7] (4 samples before + 4 after the sub-pixel)
 */
const SINC_SUBDIVISIONS = 16;
const SINC_TAPS = 8; // 4 before + 4 after
const sincKernels = new Array(SINC_SUBDIVISIONS);
for (let s = 0; s < SINC_SUBDIVISIONS; s++) {
  const phase = s / SINC_SUBDIVISIONS; // 0..1 (fractional sample position)
  const w = new Float32Array(SINC_TAPS);
  for (let t = 0; t < SINC_TAPS; t++) {
    const x = (t - (SINC_TAPS / 2 - 1)) - phase; // offset from center (positive = forward)
    w[t] = lanczos(x, SINC_TAPS / 2);
  }
  // Normalize to avoid DC gain error
  let sum = 0; for (let t = 0; t < SINC_TAPS; t++) sum += w[t];
  if (sum !== 0) for (let t = 0; t < SINC_TAPS; t++) w[t] /= sum;
  sincKernels[s] = w;
}

/**
 * Compute 16 interpolated values between samples[i] and samples[i+1]
 * using Lanczos sinc with surrounding context.
 * @param {Float32Array} samples  raw sample values
 * @param {number} i              base index (0-based)
 * @returns {Float32Array}        16 interpolated values
 */
function sincInterpolate(samples, i) {
  const n = samples.length;
  const result = new Float32Array(SINC_SUBDIVISIONS);
  for (let s = 0; s < SINC_SUBDIVISIONS; s++) {
    const w = sincKernels[s];
    let val = 0;
    for (let t = 0; t < SINC_TAPS; t++) {
      const idx = i - (SINC_TAPS / 2 - 1) + t;
      const clampedIdx = Math.max(0, Math.min(n - 1, idx));
      val += w[t] * samples[clampedIdx];
    }
    result[s] = val;
  }
  return result;
}

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

// ── CSS color tokens (resolved at runtime) ────────────────────────────────────

const C = {
  s:      '#4ade80', // --an-s   green-400
  sRms:   '#16a34a', // darker green
  o:      '#38bdf8', // --an-o   sky-400
  clips:  '#ef4444', // --an-clip red
  overs:  '#fb923c', // --an-over orange
  dr:     'rgba(163,230,53,0.18)',  // --an-dr
  gate:   'rgba(100,116,139,0.18)', // --an-gate
  grid:   'rgba(255,255,255,0.07)',
  axisLabel: '#7dd3fc',
  bg:     'transparent',
  over0:  '#ef4444', // inter-sample overs >0 dBFS
  cursor: 'rgba(255,255,255,0.6)',
  playhead: '#38bdf8',
};

const MINIMAP_H = 24; // px
const RULER_W = 44;   // px: the dB scale over the left edge of the detail view
const EDGE_PX = 6;    // px: the minimap window's edges take a drag this close
const AMP_MAX = 1024; // the deepest amplitude zoom (+60 dB)
const MIN_VIEW_SAMPLES = 32;
const BASE_URL = 'https://aura.localhost/player/an';

// ── Factory ───────────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget, tileCache: object, cursor: object }} ctx
 */
export function create(container, ctx) {
  const { store, bus, tileCache } = ctx;

  // ── DOM structure ──────────────────────────────────────────────────────────
  container.style.cssText = 'position:relative;overflow:hidden;width:100%;height:100%;';

  // Minimap strip
  const minimapEl = document.createElement('div');
  minimapEl.style.cssText = `position:relative;width:100%;height:${MINIMAP_H}px;flex-shrink:0;`;
  const minimapCanvas = document.createElement('canvas');
  minimapCanvas.style.cssText = 'display:block;width:100%;height:100%;';
  const minimapOverlay = document.createElement('canvas');
  minimapOverlay.style.cssText = 'position:absolute;inset:0;pointer-events:none;width:100%;height:100%;';
  minimapEl.appendChild(minimapCanvas);
  minimapEl.appendChild(minimapOverlay);

  // Detail view
  const detailEl = document.createElement('div');
  detailEl.style.cssText = 'position:relative;width:100%;flex:1;min-height:80px;';
  const mainCanvas = document.createElement('canvas');
  // A stream's waveform, fed by the live frames (stream-wave.js).
  const streamWave = createStreamWave();
  mainCanvas.style.cssText = 'display:block;width:100%;height:100%;';
  const overlayCanvas = document.createElement('canvas');
  overlayCanvas.style.cssText = 'position:absolute;inset:0;pointer-events:none;width:100%;height:100%;';
  detailEl.appendChild(mainCanvas);
  detailEl.appendChild(overlayCanvas);

  // The Follow switch, in the detail view's top-right corner (shown while
  // there is a playhead: the track is the one playing).
  const followBtn = document.createElement('button');
  followBtn.type = 'button';
  followBtn.className = 'an-wave-follow';
  followBtn.hidden = true;
  followBtn.innerHTML = '<svg width="9" height="10" viewBox="0 0 9 10" fill="none" stroke="currentColor"'
    + ' stroke-width="1.4" stroke-linecap="round" aria-hidden="true"><line x1="4.5" y1="1" x2="4.5" y2="9"/>'
    + '<polyline points="1.5,3.5 0.5,5 1.5,6.5"/><polyline points="7.5,3.5 8.5,5 7.5,6.5"/></svg><span></span>';
  detailEl.appendChild(followBtn);

  container.appendChild(minimapEl);
  container.appendChild(detailEl);
  container.style.display = 'flex';
  container.style.flexDirection = 'column';

  // ── State ──────────────────────────────────────────────────────────────────

  const layers = {};
  for (const def of LAYER_REGISTRY) layers[def.id] = def.defaultOn;

  // From AAN2 (track frame)
  let durationS   = 0;
  let nLods       = 0;
  let tileSamples = 256;
  let nChannels   = 1;
  /** @type {number[]} tile counts per LOD for S */
  let waveCountsS = [];
  /** @type {number[]} tile counts per LOD for O */
  let waveCountsO = [];
  /** @type {Array<{startSample:number, runLen:number}>} */
  let clipEvents  = [];
  /** @type {Array<{rms:number, peak:number}>} */
  let drBlocks    = [];
  /** @type {number|null} sample rate of source (Hz) */
  let sampleRate  = null;

  // Viewport: which samples to display in the detail view
  let viewStartS  = 0;  // start time in seconds
  let viewEndS    = 0;  // end time in seconds

  // Amplitude zoom of the detail view (1 = full scale fills the lane) and the
  // zoom ampToY draws with now (the minimap is always at full scale).
  let ampZoom  = 1;
  let drawZoom = 1;

  // The waveform from the samples (`wavez`), per signal: the last reply and
  // the request in flight.
  const wz    = { S: null, O: null };
  const wzAsk = { S: null, O: null };
  // A signal whose samples its pass does not keep (status 2), and when to ask
  // again after "being decoded" (status 0); both until the next pass or track.
  const wzNone    = { S: false, O: false };
  const wzRetryAt = { S: 0, O: 0 };
  let wzTimer = null;

  // Cached tile decoded data: key → { mins: Float32Array, maxs: Float32Array, rms: Float32Array }
  const decodedTiles = new Map();

  // Cached raw sample tiles (for deep zoom)
  const rawTiles = new Map();

  // Live O loudness series for gate spans (from store.series)
  // Playhead position (seconds) updated from live frame
  let playheadS = null;

  // A file's pointer here (its line and the peaks under it) and another
  // view's time (a line only).
  let filePtr = null;
  let otherT = null;

  // Following the playhead (Anton 1.10): while zoomed in, the view keeps the
  // playhead in its middle and moves with it. Moving the view by hand (drag,
  // Shift+wheel, the minimap) pauses it; the Follow button or a double click
  // takes it up again.
  let follow = true;

  // Canvas metrics (set in setup)
  let mainW = 0, mainH = 0;
  let minimapW = 0, minimapH_px = 0;

  // RAF handle for overlay
  let rafHandle = null;
  let destroyed = false;

  // ── Canvas sizing ──────────────────────────────────────────────────────────

  function resizeAll() {
    const r1 = setupCanvas(minimapCanvas, minimapEl);
    minimapW   = r1.w;
    minimapH_px = r1.h;
    setupCanvas(minimapOverlay, minimapEl);

    const r2 = setupCanvas(mainCanvas, detailEl);
    mainW = r2.w;
    mainH = r2.h;
    setupCanvas(overlayCanvas, detailEl);
  }

  // ── Tile decoding ──────────────────────────────────────────────────────────

  /**
   * Parse an AAWT wave tile.
   * Returns { mins, maxs, rms } arrays (length = nSamples × nChannels, interleaved).
   */
  function decodeWaveTile(bytes) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const magic = String.fromCharCode(view.getUint8(0), view.getUint8(1), view.getUint8(2), view.getUint8(3));
    if (magic !== 'AAWT') throw new TypeError('Expected AAWT tile, got: ' + magic);
    const ch   = view.getUint8(7);
    const nSmp = view.getUint32(12, true);
    const count = nSmp * ch;
    const mins = new Float32Array(count);
    const maxs = new Float32Array(count);
    const rms  = new Float32Array(count);
    let off = 16;
    for (let k = 0; k < count; k++) {
      mins[k] = view.getFloat32(off,     true); off += 4;
      maxs[k] = view.getFloat32(off,     true); off += 4;
      rms[k]  = view.getFloat32(off,     true); off += 4;
    }
    return { mins, maxs, rms, nSamples: nSmp, nChannels: ch };
  }

  // ── LOD selection ──────────────────────────────────────────────────────────

  /**
   * Pick the best LOD for the current pixel width and time range.
   * LOD 0 = most zoomed out (fewest tiles), LOD (nLods-1) = finest.
   */
  function selectLod(pixelWidth, startS, endS) {
    if (nLods === 0 || durationS === 0 || sampleRate == null) return 0;
    const totalSamples = durationS * sampleRate;
    const visibleSamples = (endS - startS) * sampleRate;
    // We want roughly 2–4 mipmap samples per pixel
    const targetSamplesPerPixel = visibleSamples / pixelWidth;

    // The backend's LOD 0 is the finest (an entry per 256 samples), each
    // next one 4x coarser. Take the coarsest that still gives at least two
    // entries per pixel: a whole track is one or two tiles, not a hundred.
    // samplesPerMipSample at LOD k = totalSamples / (waveCountsS[k] * tileSamples)
    let bestLod = 0;
    for (let lod = nLods - 1; lod >= 0; lod--) {
      const cnt = waveCountsS[lod] || 1;
      const spm = totalSamples / (cnt * tileSamples);
      if (spm <= targetSamplesPerPixel / 2) {
        bestLod = lod;
        break;
      }
    }
    return bestLod;
  }

  // ── Draw helpers ──────────────────────────────────────────────────────────

  function drawGrid(ctx2d, w, h) {
    ctx2d.strokeStyle = C.grid;
    ctx2d.lineWidth = 0.5;
    // Horizontal lines at 0, -3, -6, -12, -24 dB below the lane's top
    for (const amp of gridAmps()) {
      const y0 = ampToY(amp, h);
      const y1 = ampToY(-amp, h);
      ctx2d.beginPath();
      ctx2d.moveTo(0, y0); ctx2d.lineTo(w, y0);
      ctx2d.moveTo(0, y1); ctx2d.lineTo(w, y1);
      ctx2d.stroke();
    }
    // Center line
    ctx2d.strokeStyle = 'rgba(255,255,255,0.15)';
    ctx2d.beginPath();
    ctx2d.moveTo(0, h / 2);
    ctx2d.lineTo(w, h / 2);
    ctx2d.stroke();
    // 0 dBFS rails (off the lane while the amplitude is zoomed in)
    const yr = ampToY(1, h);
    if (yr >= 0) {
      ctx2d.strokeStyle = 'rgba(239,68,68,0.25)';
      ctx2d.setLineDash([4, 4]);
      ctx2d.beginPath();
      ctx2d.moveTo(0, yr);     ctx2d.lineTo(w, yr);
      ctx2d.moveTo(0, h - yr); ctx2d.lineTo(w, h - yr);
      ctx2d.stroke();
      ctx2d.setLineDash([]);
    }
  }

  /** The dB scale over the left edge of the detail view. */
  function drawRuler(ctx2d, h) {
    ctx2d.fillStyle = 'rgba(11,18,32,0.78)';
    ctx2d.fillRect(0, 0, RULER_W, h);
    ctx2d.strokeStyle = 'rgba(125,211,252,0.22)';
    ctx2d.lineWidth = 1;
    ctx2d.beginPath(); ctx2d.moveTo(RULER_W + 0.5, 0); ctx2d.lineTo(RULER_W + 0.5, h); ctx2d.stroke();
    ctx2d.fillStyle = C.axisLabel;
    ctx2d.font = '9px ui-monospace, Consolas, monospace';
    ctx2d.textAlign = 'right';
    ctx2d.textBaseline = 'middle';
    let lastY = -1e9;
    for (const amp of gridAmps()) {
      const y = Math.max(20, ampToY(amp, h));   // the top line's label sits under "dBFS"
      if (y - lastY < 11) continue;
      const db = 20 * Math.log10(amp);
      const txt = db > -0.05 ? '0' : (Math.abs(db) < 10 ? db.toFixed(1) : db.toFixed(0)).replace('-', '\u2212');
      ctx2d.fillText(txt, RULER_W - 5, y);
      lastY = y;
    }
    ctx2d.textAlign = 'left';
    ctx2d.textBaseline = 'top';
    ctx2d.fillText('dBFS', 4, 3);
    if (drawZoom > 1.001) {
      ctx2d.textBaseline = 'bottom';
      ctx2d.fillText('+' + (20 * Math.log10(drawZoom)).toFixed(1), 4, h - 14);
    }
    ctx2d.textBaseline = 'alphabetic';
  }

  /**
   * Map a sample value (−1..+1) to canvas y coordinate.
   * @param {number} v sample value
   * @param {number} h canvas height in CSS px
   * @returns {number}
   */
  function ampToY(v, h) {
    return h / 2 - v * drawZoom * (h / 2 - 3);
  }

  /** Amplitudes of the grid lines and the scale's labels: 0, −3, −6, −12 and
   *  −24 dB below the top of the lane. */
  function gridAmps() {
    const top = 1 / drawZoom;
    return [1, 0.7079, 0.5012, 0.2512, 0.0631].map(f => top * f);
  }

  /**
   * Draw min/max/rms waveform for one channel from tile data.
   * @param {CanvasRenderingContext2D} ctx2d
   * @param {number} xLeft px
   * @param {number} xRight px
   * @param {object} decoded { mins, maxs, rms, nSamples }
   * @param {number} chan channel index
   * @param {string} color
   * @param {boolean} drawRms
   * @param {number} h canvas height
   */
  function drawMipmapRange(ctx2d, xLeft, xRight, decoded, chan, color, drawRms, h) {
    const { mins, maxs, rms: rmsArr, nSamples, nChannels: ch } = decoded;
    const pxPerSample = (xRight - xLeft) / nSamples;

    ctx2d.fillStyle = color + '60'; // semi-transparent fill
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 0.8;

    // Fill min/max band
    ctx2d.beginPath();
    for (let s = 0; s < nSamples; s++) {
      const k = s * ch + chan;
      const x = xLeft + s * pxPerSample;
      const yMax = ampToY(maxs[k], h);
      const yMin = ampToY(mins[k], h);
      if (s === 0) ctx2d.moveTo(x, yMax);
      else ctx2d.lineTo(x, yMax);
    }
    for (let s = nSamples - 1; s >= 0; s--) {
      const k = s * ch + chan;
      const x = xLeft + s * pxPerSample;
      const yMin = ampToY(mins[k], h);
      ctx2d.lineTo(x, yMin);
    }
    ctx2d.closePath();
    ctx2d.fill();

    // RMS envelope if enabled
    if (drawRms) {
      ctx2d.strokeStyle = color;
      ctx2d.lineWidth = 1;
      ctx2d.beginPath();
      for (let s = 0; s < nSamples; s++) {
        const k = s * ch + chan;
        const x = xLeft + s * pxPerSample;
        const y = ampToY(rmsArr[k], h);
        if (s === 0) ctx2d.moveTo(x, y); else ctx2d.lineTo(x, y);
      }
      ctx2d.stroke();
    }

    // Over-0 dBFS highlights (inter-sample overs in red)
    if (layers['overs']) {
      for (let s = 0; s < nSamples; s++) {
        const k = s * ch + chan;
        if (maxs[k] > 1.0) {
          const x = xLeft + s * pxPerSample;
          const y = ampToY(maxs[k], h);
          ctx2d.fillStyle = C.over0;
          ctx2d.fillRect(x, 0, Math.max(1, pxPerSample), Math.max(1, h / 2 - y));
        }
      }
    }
  }

  /**
   * Draw sinc-interpolated reconstruction curve from raw samples.
   * Used at deep zoom (>= 4 px/sample).
   */
  function drawSincCurve(ctx2d, xLeft, xRight, rawSamples, nSamples, chan, nCh, color, h) {
    if (nSamples < SINC_TAPS) return;
    const pxPerSample = (xRight - xLeft) / nSamples;
    const totalPoints = nSamples * SINC_SUBDIVISIONS;
    const pxPerPoint = pxPerSample / SINC_SUBDIVISIONS;

    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 1.5;
    ctx2d.beginPath();
    let first = true;
    for (let i = 0; i < nSamples - 1; i++) {
      // Extract mono channel view
      const windowLen = Math.min(SINC_TAPS, nSamples);
      const start = Math.max(0, i - SINC_TAPS / 2 + 1);
      const end   = Math.min(nSamples, start + SINC_TAPS + 2);
      const window = new Float32Array(nSamples);
      for (let k = 0; k < nSamples; k++) window[k] = rawSamples[k * nCh + chan];

      const interp = sincInterpolate(window, i);
      for (let s = 0; s < SINC_SUBDIVISIONS; s++) {
        const x = xLeft + (i + s / SINC_SUBDIVISIONS) * pxPerSample;
        const y = ampToY(interp[s], h);
        if (first) { ctx2d.moveTo(x, y); first = false; }
        else ctx2d.lineTo(x, y);
      }

      // Sample dot
      const x = xLeft + i * pxPerSample;
      const y = ampToY(window[i], h);
      ctx2d.fillStyle = color;
      ctx2d.fillRect(x - 2, y - 2, 4, 4);

      // Inter-sample over if max(interp) > 1
      if (layers['overs']) {
        const maxInterp = Math.max(...interp);
        if (maxInterp > 1.0) {
          ctx2d.fillStyle = C.over0;
          ctx2d.fillRect(x, 0, Math.max(1, pxPerSample), 4);
        }
      }
    }
    ctx2d.stroke();
  }

  /**
   * Draw clip markers (red ticks) from clip events.
   * @param {CanvasRenderingContext2D} ctx2d
   * @param {number} startSample
   * @param {number} endSample
   * @param {number} xLeft
   * @param {number} xRight
   * @param {number} h
   */
  function drawClipMarkers(ctx2d, startSample, endSample, xLeft, xRight, h) {
    if (!layers['clips']) return;
    const rangeLen = endSample - startSample;
    ctx2d.fillStyle = C.clips;
    for (const ev of clipEvents) {
      if (ev.startSample + ev.runLen < startSample) continue;
      if (ev.startSample > endSample) continue;
      const relStart = (ev.startSample - startSample) / rangeLen;
      const relEnd   = (ev.startSample + ev.runLen - startSample) / rangeLen;
      const x0 = xLeft + relStart * (xRight - xLeft);
      const x1 = xLeft + relEnd   * (xRight - xLeft);
      ctx2d.fillRect(x0, 0, Math.max(1, x1 - x0), 5);
    }
  }

  /**
   * Draw DR top-20% block spans and gate spans as background fills.
   */
  function drawSpans(ctx2d, startS, endS, w, h) {
    const rangeS = endS - startS;
    if (rangeS <= 0) return;

    // DR block spans
    if (layers['dr_blocks'] && drBlocks.length > 0) {
      // Sort by RMS descending; top 20%
      const sorted = [...drBlocks].sort((a, b) => b.rms - a.rms);
      const topN = Math.max(1, Math.round(sorted.length * 0.20));
      const blockDur = 3; // 3-second blocks
      ctx2d.fillStyle = C.dr;
      for (let i = 0; i < topN; i++) {
        const idx = drBlocks.indexOf(sorted[i]);
        const blockStart = idx * blockDur;
        const blockEnd   = blockStart + blockDur;
        if (blockEnd < startS || blockStart > endS) continue;
        const x0 = ((blockStart - startS) / rangeS) * w;
        const x1 = ((blockEnd   - startS) / rangeS) * w;
        ctx2d.fillRect(x0, 0, x1 - x0, h);
      }
    }

    // Gate spans: intervals where LUFS-S > gate threshold
    if (layers['gate_spans']) {
      const gateThreshold = -70;
      const series = store.series?.lufs_s_s;
      if (series && series.length > 0) {
        ctx2d.fillStyle = C.gate;
        const step = durationS / series.length;
        let gateStart = null;
        for (let i = 0; i < series.length; i++) {
          const t = i * step;
          const val = series[i];
          if (!isNaN(val) && val > gateThreshold) {
            if (gateStart === null) gateStart = t;
          } else {
            if (gateStart !== null) {
              const x0 = ((gateStart - startS) / rangeS) * w;
              const x1 = ((t - startS) / rangeS) * w;
              if (x1 > 0 && x0 < w) ctx2d.fillRect(Math.max(0, x0), 0, Math.max(0, Math.min(w, x1) - Math.max(0, x0)), h);
              gateStart = null;
            }
          }
        }
      }
    }
  }

  // ── Minimap draw ──────────────────────────────────────────────────────────

  function drawMinimap() {
    const ctx2d = minimapCanvas.getContext('2d');
    if (!ctx2d) return;
    drawZoom = 1;
    const w = minimapW, h = minimapH_px;
    ctx2d.clearRect(0, 0, w, h);
    ctx2d.fillStyle = 'rgba(15,23,42,0.8)';
    ctx2d.fillRect(0, 0, w, h);
    // A live stream: all it keeps, the view lit (stream-wave.js).
    if (store.get && store.get('_stream')) { streamWave.drawOverview(ctx2d, w, h); return; }
    if (durationS <= 0 || nLods === 0) return;

    // The coarsest LOD (the last) for the minimap
    const lod = nLods - 1;
    const tileCnt = waveCountsS[lod] || 0;
    if (tileCnt === 0) return;

    const drawTileAtX = (tileIdx, src, color) => {
      const key = `${src}-wave-${lod}-${tileIdx}`;
      const bytes = tileCache.get(key);
      if (!bytes) {
        tileCache.request(key, src, 'wave', lod, tileIdx);
        return;
      }
      let decoded = decodedTiles.get(key);
      if (!decoded) {
        try { decoded = decodeWaveTile(bytes); decodedTiles.set(key, decoded); }
        catch { return; }
      }
      const xLeft  = (tileIdx / tileCnt) * w;
      const xRight = ((tileIdx + 1) / tileCnt) * w;
      ctx2d.save();
      ctx2d.fillStyle = color + '80';
      ctx2d.strokeStyle = color;
      ctx2d.lineWidth = 0.5;
      drawMipmapRange(ctx2d, xLeft, xRight, decoded, 0, color, false, h);
      ctx2d.restore();
    };

    for (let i = 0; i < tileCnt; i++) {
      if (layers['s_mip']) drawTileAtX(i, 'S', C.s);
      if (layers['o_mip'] && (waveCountsO[lod] || 0) > 0) drawTileAtX(i, 'O', C.o);
    }

    // Viewport indicator, with a grip on each edge (drag = time zoom)
    if (durationS > 0 && viewEndS > viewStartS) {
      const x0 = (viewStartS / durationS) * w;
      const x1 = Math.max(x0 + 2, (viewEndS / durationS) * w);
      ctx2d.fillStyle = 'rgba(56,189,248,0.10)';
      ctx2d.fillRect(x0, 0, x1 - x0, h);
      ctx2d.strokeStyle = 'rgba(56,189,248,0.75)';
      ctx2d.lineWidth = 1;
      ctx2d.strokeRect(x0 + 0.5, 0.5, x1 - x0 - 1, h - 1);
      ctx2d.fillStyle = 'rgba(186,230,253,0.9)';
      for (const x of [x0, x1]) ctx2d.fillRect(x - 1.5, h / 2 - 5, 3, 10);
    }
  }

  // ── Detail draw ───────────────────────────────────────────────────────────

  function drawDetail() {
    const ctx2d = mainCanvas.getContext('2d');
    if (!ctx2d) return;
    const w = mainW, h = mainH;
    ctx2d.clearRect(0, 0, w, h);
    ctx2d.fillStyle = 'rgba(15,23,42,0.6)';
    ctx2d.fillRect(0, 0, w, h);
    // A live stream: its waveform as it played (stream-wave.js).
    if (store.get && store.get('_stream')) {
      // The pointer's value keeps clear of the Follow button (it stood under
      // "Following" at the right edge).
      streamWave.draw(ctx2d, w, h, followRect());
      return;
    }
    if (durationS <= 0 || nLods === 0 || viewEndS <= viewStartS) return;

    drawZoom = ampZoom;
    drawGrid(ctx2d, w, h);
    drawSpans(ctx2d, viewStartS, viewEndS, w, h);

    const startS = viewStartS, endS = viewEndS;
    const startSample = Math.floor(startS * (sampleRate || 44100));
    const endSample   = Math.ceil(endS   * (sampleRate || 44100));
    const totalSamples = durationS * (sampleRate || 44100);

    // Closer than the finest tiles give two entries a pixel: the samples.
    const useWz = (endSample - startSample) < 512 * w;
    const wzDrawn = { S: false, O: false };
    if (useWz) {
      if (layers['s_mip']) wzDrawn.S = drawWz(ctx2d, 'S', C.s, startS, endS, w, h);
      if (layers['o_mip'] && (waveCountsO[0] || 0) > 0) wzDrawn.O = drawWz(ctx2d, 'O', C.o, startS, endS, w, h);
      askWz();
    }

    const lod = selectLod(w, startS, endS);
    const tileCnt = waveCountsS[lod] || 0;
    if (tileCnt === 0) { finishDetail(ctx2d, startSample, endSample, startS, endS, w, h); return; }

    // Pixels per sample at this zoom
    const visibleSamples = endSample - startSample;
    const pxPerSample = w / Math.max(1, visibleSamples);
    const useDeepZoom = pxPerSample >= 4 && lod >= nLods - 1;

    // Draw tiles that overlap the viewport
    const tileStartIdx = Math.max(0, Math.floor((startS / durationS) * tileCnt));
    const tileEndIdx   = Math.min(tileCnt - 1, Math.ceil((endS / durationS) * tileCnt));

    for (let ti = tileStartIdx; ti <= tileEndIdx; ti++) {
      const drawForSrc = (src, color) => {
        const key = `${src}-wave-${lod}-${ti}`;
        let bytes = tileCache.get(key);
        if (!bytes) {
          tileCache.request(key, src, 'wave', lod, ti);
          return;
        }
        let decoded = decodedTiles.get(key);
        if (!decoded) {
          try { decoded = decodeWaveTile(bytes); decodedTiles.set(key, decoded); }
          catch { return; }
        }
        // Map tile to pixel x range
        const tileStartS = (ti / tileCnt) * durationS;
        const tileEndS   = ((ti + 1) / tileCnt) * durationS;
        const xLeft  = ((tileStartS - startS) / (endS - startS)) * w;
        const xRight = ((tileEndS   - startS) / (endS - startS)) * w;

        if (useDeepZoom) {
          // Try raw tile for sinc reconstruction
          const rawKey = `${src}-raw-${lod}-${ti}`;
          const rawBytes = tileCache.get(rawKey);
          if (rawBytes) {
            const rawView = new DataView(rawBytes.buffer, rawBytes.byteOffset, rawBytes.byteLength);
            const nSmp = rawView.getUint32(12, true);
            const ch   = rawView.getUint8(7);
            const raw  = new Float32Array(rawBytes.buffer, rawBytes.byteOffset + 16, nSmp * ch);
            drawSincCurve(ctx2d, xLeft, xRight, raw, nSmp, 0, ch, color, h);
          } else {
            // Fall back to mipmap, and request raw tile
            tileCache.request(rawKey, src, 'raw', lod, ti);
            drawMipmapRange(ctx2d, xLeft, xRight, decoded, 0, color, layers['s_rms'] && src === 'S', h);
          }
        } else {
          drawMipmapRange(ctx2d, xLeft, xRight, decoded, 0, color, layers['s_rms'] && src === 'S', h);
        }
      };

      if (layers['s_mip'] && !wzDrawn.S) drawForSrc('S', C.s);
      // O tiles exist only when the backend built them (counts > 0).
      if (layers['o_mip'] && (waveCountsO[lod] || 0) > 0 && !wzDrawn.O) drawForSrc('O', C.o);
    }
    finishDetail(ctx2d, startSample, endSample, startS, endS, w, h);
  }

  /** The Follow button's box in the detail view (null when hidden): values keep clear of it. */
  function followRect() {
    return followBtn.hidden ? null
      : { x: followBtn.offsetLeft, y: followBtn.offsetTop, w: followBtn.offsetWidth, h: followBtn.offsetHeight };
  }

  /**
   * A file's peak (dBFS) at time `t`: the entry under it in the tiles the
   * view draws now (their level, the left channel, as drawn); NaN when that
   * tile is not here.
   */
  function filePeakDb(src, t) {
    if (durationS <= 0 || nLods === 0 || !(t >= 0 && t <= durationS)) return NaN;
    const lod = selectLod(Math.max(1, mainW), viewStartS, viewEndS);
    const tileCnt = waveCountsS[lod] || 0;
    if (!tileCnt || (src === 'O' && !((waveCountsO[lod] || 0) > 0))) return NaN;
    const ti = Math.min(tileCnt - 1, Math.floor(t / durationS * tileCnt));
    const d = decodedTiles.get(`${src}-wave-${lod}-${ti}`);
    if (!d || !d.nSamples) return NaN;
    const tileS = durationS / tileCnt;
    const j = Math.min(d.nSamples - 1, Math.max(0, Math.floor((t - ti * tileS) / tileS * d.nSamples)));
    const k = j * d.nChannels;
    const p = Math.max(Math.abs(d.mins[k]), Math.abs(d.maxs[k]));
    return p > 0 ? 20 * Math.log10(p) : NaN;
  }

  /** Clip markers, the time axis and the dB scale over the waveform. */
  function finishDetail(ctx2d, startSample, endSample, startS, endS, w, h) {
    if (layers['clips']) {
      drawClipMarkers(ctx2d, startSample, endSample, 0, w, h);
    }
    // Time axis labels (milliseconds once the view is shorter than 10 s)
    ctx2d.fillStyle = C.axisLabel;
    ctx2d.font = '10px ui-monospace, Consolas, monospace';
    ctx2d.textBaseline = 'bottom';
    const span = endS - startS;
    const nTicks = Math.min(8, Math.floor((w - RULER_W) / 76));
    for (let i = 0; i <= nTicks; i++) {
      const x = RULER_W + (i / Math.max(1, nTicks)) * (w - RULER_W - 60);
      const t = startS + (x / w) * span;
      const m = Math.floor(t / 60);
      const s = span < 10 ? (t % 60).toFixed(3).padStart(6, '0') : (t % 60).toFixed(1).padStart(4, '0');
      ctx2d.fillText(`${m}:${s}`, x + 2, h - 2);
    }
    drawRuler(ctx2d, h);
  }

  // ── The waveform from the samples (wavez) ─────────────────────────────────

  // AAWZ: "AAWZ", status u8, src u8, raw u8, 0, gen u32, a u64, b u64 (the
  // signal's samples covered), count u32, lf u32 (signal samples per source
  // sample), then count x (min L, max L, min R, max R) f32.
  function decodeWz(buf) {
    const dv = new DataView(buf);
    const count = dv.getUint32(28, true);
    return {
      status: dv.getUint8(4), raw: dv.getUint8(6) === 1, gen: dv.getUint32(8, true),
      a: Number(dv.getBigUint64(12, true)), b: Number(dv.getBigUint64(20, true)),
      count, lf: Math.max(1, dv.getUint32(32, true)),
      data: new Float32Array(buf.slice(36, 36 + count * 16)),
    };
  }

  /** Draw `src` from its last `wavez` reply when it covers the view; false
   *  when it does not (the tiles are drawn meanwhile). */
  function drawWz(ctx2d, src, color, startS, endS, w, h) {
    const r = wz[src];
    const sr = sampleRate || 44100;
    if (!r || r.count < 2) return false;
    if (r.a / r.lf > Math.floor(startS * sr) || r.b / r.lf < Math.min(Math.ceil(endS * sr), Math.floor(durationS * sr) - 1)) return false;
    const span = endS - startS;
    const toX = (pos) => ((pos / sr) - startS) / span * w;           // pos: source samples
    const posOf = (i) => (r.a + (r.b - r.a) * i / r.count) / r.lf;
    const d = r.data;
    if (r.raw) {
      const pxPer = toX(posOf(1)) - toX(posOf(0));
      const vals = new Float32Array(r.count);
      for (let i = 0; i < r.count; i++) vals[i] = d[i * 4];
      const at = (sec) => (sec * sr * r.lf - r.a) / (r.b - r.a) * r.count;
      const i0 = Math.max(0, Math.floor(at(startS)) - 4);
      const i1 = Math.min(r.count, Math.ceil(at(endS)) + 4);
      ctx2d.strokeStyle = color;
      ctx2d.lineWidth = 1.3;
      ctx2d.beginPath();
      let first = true;
      let over = false;
      for (let i = i0; i < i1; i++) {
        if (pxPer >= 3 && i < r.count - 1) {
          // The band-limited curve between the samples (16 points each)
          const interp = sincInterpolate(vals, i);
          for (let s = 0; s < SINC_SUBDIVISIONS; s++) {
            const x = toX(posOf(i + s / SINC_SUBDIVISIONS));
            const y = ampToY(interp[s], h);
            if (Math.abs(interp[s]) > 1) over = true;
            if (first) { ctx2d.moveTo(x, y); first = false; } else ctx2d.lineTo(x, y);
          }
        } else {
          const x = toX(posOf(i)), y = ampToY(vals[i], h);
          if (first) { ctx2d.moveTo(x, y); first = false; } else ctx2d.lineTo(x, y);
        }
      }
      ctx2d.stroke();
      if (pxPer >= 6) {
        ctx2d.fillStyle = color;
        for (let i = i0; i < i1; i++) {
          const x = toX(posOf(i)), y = ampToY(vals[i], h);
          ctx2d.fillRect(x - 1.5, y - 1.5, 3, 3);
        }
      }
      if (over && layers['overs']) {
        ctx2d.fillStyle = C.over0;
        ctx2d.fillRect(0, 0, w, 2);
      }
      return true;
    }
    // Min/max of each bin (L), as a filled band
    ctx2d.fillStyle = color + '60';
    ctx2d.beginPath();
    for (let i = 0; i < r.count; i++) {
      const x = toX(posOf(i + 0.5));
      const y = ampToY(d[i * 4 + 1], h);
      if (i === 0) ctx2d.moveTo(x, y); else ctx2d.lineTo(x, y);
    }
    for (let i = r.count - 1; i >= 0; i--) ctx2d.lineTo(toX(posOf(i + 0.5)), ampToY(d[i * 4], h));
    ctx2d.closePath();
    ctx2d.fill();
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 0.8;
    ctx2d.stroke();
    if (layers['overs']) {
      ctx2d.fillStyle = C.over0;
      for (let i = 0; i < r.count; i++) {
        if (d[i * 4 + 1] > 1 || d[i * 4] < -1) ctx2d.fillRect(toX(posOf(i)), 0, Math.max(1, w / r.count), 3);
      }
    }
    return true;
  }

  /** Ask for the samples of the view (and a quarter past each side), once
   *  the view has stood still for 40 ms. While it follows the playhead it
   *  never stands still: the samples ahead are asked for at once, whenever
   *  the last answer no longer reaches half a view past the view's end. */
  function askWz() {
    clearTimeout(wzTimer);
    if (followingNow()) { askWzNow(true); return; }
    wzTimer = setTimeout(() => askWzNow(false), 40);
  }

  function askWzNow(ahead) {
    const sid = store.get ? store.get('sid') : null;
    if (sid == null || durationS <= 0) return;
    const sr = sampleRate || 44100;
    const span = viewEndS - viewStartS;
    if (!(span > 0)) return;
    const after = ahead ? Math.max(span * 3, 0.25) : span * 0.25;
    const s0 = Math.max(0, Math.floor((viewStartS - span * 0.25) * sr));
    const s1 = Math.min(Math.ceil(durationS * sr), Math.ceil((viewEndS + after) * sr));
    // A bin a pixel.
    const n = Math.min(8192, Math.max(64, Math.round(mainW * (s1 - s0) / (span * sr))));
    const want = [];
    if (layers['s_mip']) want.push('S');
    if (layers['o_mip'] && (waveCountsO[0] || 0) > 0) want.push('O');
    for (const src of want) {
      // Following asks every frame: not for samples this pass does not keep,
      // nor while S is being decoded again (up to 60 requests a second, each
      // on the app's main thread, all through the playback otherwise).
      if (wzNone[src] || performance.now() < wzRetryAt[src]) continue;
      if (ahead) {
        if (wzAsk[src]) continue;   // one in flight
        const r = wz[src];
        const need = Math.min(Math.ceil((viewEndS + span * 0.5) * sr), Math.floor(durationS * sr) - 1);
        if (r && r.count >= 2 && r.a / r.lf <= Math.floor(viewStartS * sr) && r.b / r.lf >= need) continue;
      }
      const key = `${s0}|${s1}|${n}`;
      if (wz[src]?.key === key || wzAsk[src] === key) continue;
      wzAsk[src] = key;
      fetch(`${BASE_URL}/wavez?sid=${sid}&src=${src}&s0=${s0}&s1=${s1}&n=${n}`, { cache: 'no-store' })
        .then(res => (res.ok ? res.arrayBuffer() : null))
        .then(buf => {
          if (wzAsk[src] === key) wzAsk[src] = null;
          if (!buf || buf.byteLength < 36 || destroyed) return;
          const r = decodeWz(buf);
          if (r.status === 0) {   // S is being decoded: again in half a second, not every frame
            wzRetryAt[src] = performance.now() + 500;
            setTimeout(askWz, 500);
            return;
          }
          if (r.status !== 1) { wzNone[src] = true; return; }   // not kept for this pass: the tiles draw it
          r.key = key;
          wz[src] = r;
          scheduleDraw();
        })
        .catch(() => { if (wzAsk[src] === key) wzAsk[src] = null; });
    }
  }

  function drawNow() {
    drawDetail();
    drawMinimap();
    // What the view shows, for a look from outside (the rw checks).
    detailEl.dataset.view = `${viewStartS.toFixed(4)}|${viewEndS.toFixed(4)}|${ampZoom.toFixed(3)}`;
    detailEl.dataset.follow = followingNow() ? '1' : '0';
  }

  let drawPending = false;
  function scheduleDraw() {
    if (drawPending) return;
    drawPending = true;
    requestAnimationFrame(() => {
      drawPending = false;
      if (destroyed) return;
      drawNow();
    });
  }

  // ── Following the playhead ────────────────────────────────────────────────

  /** The view moves with the playhead now: following, a playhead, zoomed in. */
  function followingNow() {
    return follow && playheadS != null && durationS > 0 && viewEndS - viewStartS < durationS - 1e-9;
  }

  let followShown = null;
  function showFollow() {
    // A stream: following its live edge (stream-wave.js); the panel's tip says it.
    const stream = !!(store.get && store.get('_stream'));
    const st = stream ? (streamWave.following ? 'on' : 'off')
      : playheadS == null || durationS <= 0 ? 'hidden' : follow ? 'on' : 'off';
    if (st + stream === followShown) return;
    followShown = st + stream;
    followBtn.hidden = st === 'hidden';
    followBtn.classList.toggle('an-wave-follow--on', st === 'on');
    followBtn.setAttribute('aria-pressed', st === 'on' ? 'true' : 'false');
    followBtn.querySelector('span').textContent = st === 'on' ? 'Following' : 'Follow';
    if (stream) { delete followBtn.dataset.tip; return; }
    followBtn.dataset.tip = st === 'on'
      ? 'Following the playhead: zoomed in, the waveform keeps it in the middle. Moving the view by hand pauses this.'
      : 'Follow the playhead again: zoomed in, the waveform keeps it in the middle. A double click does it too (with the whole track).';
  }

  function setFollow(on) {
    follow = on;
    showFollow();
    detailEl.dataset.follow = followingNow() ? '1' : '0';
  }

  /** Every frame: the playhead in the middle of the view while following. */
  function followTick() {
    showFollow();
    if (!followingNow()) return;
    const span = viewEndS - viewStartS;
    const s = Math.max(0, Math.min(durationS - span, playheadS - span / 2));
    if (Math.abs(s - viewStartS) * Math.max(1, mainW) / span < 0.1) return;   // under a tenth of a pixel
    viewStartS = s;
    viewEndS = s + span;
    // Drawn in this frame, with the playhead line: the wave never trails it.
    // Not while another tab is up (its size comes back with the tab).
    if (detailEl.offsetParent !== null) drawNow();
  }

  followBtn.addEventListener('click', () => {
    if (store.get && store.get('_stream')) { streamWave.followEdge(); showFollow(); scheduleDraw(); return; }
    setFollow(!follow);
    if (follow) followTick();
  });

  // ── Overlay (60 fps) ──────────────────────────────────────────────────────

  function drawOverlay(cursor) {
    const ctx2d = overlayCanvas.getContext('2d');
    if (!ctx2d) return;
    const w = mainW, h = mainH;
    ctx2d.clearRect(0, 0, w, h);
    if (durationS <= 0 || viewEndS <= viewStartS) return;
    const rangeS = viewEndS - viewStartS;

    // Playhead
    if (playheadS !== null && playheadS >= viewStartS && playheadS <= viewEndS) {
      const x = ((playheadS - viewStartS) / rangeS) * w;
      ctx2d.strokeStyle = C.playhead;
      ctx2d.lineWidth = 1.5;
      ctx2d.beginPath();
      ctx2d.moveTo(x, 0);
      ctx2d.lineTo(x, h);
      ctx2d.stroke();
    }

    // The pointer here: its line and the peaks of S and O under it, as a
    // stream's (it had neither: the line below read a cursor nothing set).
    if (filePtr != null && filePtr >= viewStartS && filePtr <= viewEndS) {
      const x = ((filePtr - viewStartS) / rangeS) * w;
      ctx2d.fillStyle = 'rgba(226,232,240,0.6)';
      ctx2d.fillRect(Math.round(x), 0, 1, h);
      const db = (v) => (Number.isFinite(v) ? v.toFixed(1) : '—');
      const text = `${formatTime(filePtr)} · peak S ${db(filePeakDb('S', filePtr))} dBFS · O ${db(filePeakDb('O', filePtr))} dBFS`;
      ctx2d.font = '11px ui-monospace, Consolas, monospace';
      const at = labelAt(x, ctx2d.measureText(text).width, w, followRect());
      ctx2d.fillStyle = '#e2e8f0';
      ctx2d.fillText(text, at.x, at.y);
    }

    // Another view's time
    const ct = otherT ?? (cursor && cursor.timeS);
    if (filePtr == null && ct != null) {
      const x = ((ct - viewStartS) / rangeS) * w;
      if (x >= 0 && x <= w) {
        ctx2d.strokeStyle = C.cursor;
        ctx2d.lineWidth = 1;
        ctx2d.setLineDash([3, 3]);
        ctx2d.beginPath();
        ctx2d.moveTo(x, 0);
        ctx2d.lineTo(x, h);
        ctx2d.stroke();
        ctx2d.setLineDash([]);
      }
    }

    // Minimap overlay
    const mm2d = minimapOverlay.getContext('2d');
    if (!mm2d) return;
    const mw = minimapW, mh = minimapH_px;
    mm2d.clearRect(0, 0, mw, mh);
    if (playheadS !== null && durationS > 0) {
      const x = (playheadS / durationS) * mw;
      mm2d.strokeStyle = C.playhead;
      mm2d.lineWidth = 1;
      mm2d.beginPath();
      mm2d.moveTo(x, 0);
      mm2d.lineTo(x, mh);
      mm2d.stroke();
    }
  }

  // ── RAF loop ──────────────────────────────────────────────────────────────

  function rafLoop() {
    if (destroyed) return;
    // The playhead is where the sound is (the player's position, as the
    // spectrogram draws it), every frame.
    // None when nothing of this window plays: no line, nothing to follow.
    const pos = store.get ? store.get('_posS') : null;
    playheadS = pos != null ? pos : null;
    followTick();
    const cursor = store.cursor || {};
    drawOverlay(cursor);
    rafHandle = requestAnimationFrame(rafLoop);
  }

  // ── Event handlers ────────────────────────────────────────────────────────

  // Track frames come again for every B/O update (the O pass sends its
  // progress twice a second): the tiles are dropped only when their layout
  // changes.
  let tileSig = '';
  const onTrack = (e) => {
    const frame = e.detail?.frame;
    if (!frame) return;
    durationS   = frame.duration_s       ?? durationS;
    nLods       = frame.n_lods           ?? 0;
    tileSamples = frame.tile_samples     ?? 256;
    nChannels   = frame.n_wave_channels  ?? 1;
    waveCountsS = frame.wave_tile_counts_s ?? [];
    waveCountsO = frame.wave_tile_counts_o ?? [];
    if (frame.src_rate > 0) sampleRate = frame.src_rate;
    clipEvents  = (frame.clips_s ?? []).map(c => ({ startSample: c.start_sample, runLen: c.run_len }));
    drBlocks    = frame.dr_blocks_s      ?? [];
    if (durationS > 0 && viewEndS <= viewStartS) {
      viewStartS = 0;
      viewEndS = durationS;
    }
    // O's tiles are its pass's: a new O pass (a rack change) brings new ones.
    const oGen = frame.spec_info ? frame.spec_info[2].gen : '';
    const sig = `${durationS}|${nLods}|${tileSamples}|${waveCountsS.join(',')}|${waveCountsO.join(',')}|${oGen}`;
    if (sig !== tileSig) {
      if (tileSig && !tileSig.endsWith(`|${oGen}`)) tileCache.dropPrefix?.('O-wave-');
      tileSig = sig;
      decodedTiles.clear();
      rawTiles.clear();
      wz.S = wz.O = null;
      wzNone.S = wzNone.O = false;   // a new pass may keep its samples
    }
    drawDetail();
    drawMinimap();
  };

  const onLive = (e) => {
    const frame = e.detail?.frame;
    if (!frame) return;
    // The playhead follows the player's position (rafLoop). This used to take
    // the pointer's time, so the line moved only when the mouse did.
    // A stream's waveform comes with its live frames.
    if (frame.strm) {
      streamWave.feed(frame.strm);
      if (store.get('_stream')) scheduleDraw();
    }
  };

  let _waveRafPending = false;
  const onTile = (e) => {
    const { key } = e.detail || {};
    if (!key) return;
    // Invalidate decoded cache so it gets re-decoded on next draw
    decodedTiles.delete(key);
    if (!_waveRafPending) {
      _waveRafPending = true;
      requestAnimationFrame(() => {
        _waveRafPending = false;
        drawDetail();
        drawMinimap();
      });
    }
  };

  const onTrackChange = () => {
    durationS = 0; nLods = 0; waveCountsS = []; waveCountsO = [];
    clipEvents = []; drBlocks = [];
    viewStartS = 0; viewEndS = 0;
    playheadS = null;
    follow = true;   // a pause by hand was for the track before
    decodedTiles.clear(); rawTiles.clear();
    wz.S = wz.O = null; wzAsk.S = wzAsk.O = null;
    wzNone.S = wzNone.O = false; wzRetryAt.S = wzRetryAt.O = 0;
    const ctx2d = mainCanvas.getContext('2d');
    if (ctx2d) ctx2d.clearRect(0, 0, mainW, mainH);
    const mm2d = minimapCanvas.getContext('2d');
    if (mm2d) mm2d.clearRect(0, 0, minimapW, minimapH_px);
  };

  bus.addEventListener('an:track', onTrack);
  bus.addEventListener('an:live',  onLive);
  bus.addEventListener('an:tile',  onTile);
  bus.addEventListener('an:track:change', onTrackChange);

  // ── View changes ──────────────────────────────────────────────────────────

  const minSpan = () => Math.min(durationS, MIN_VIEW_SAMPLES / (sampleRate || 44100));

  /** Show [a, b): at least MIN_VIEW_SAMPLES wide, at most the track, inside it. */
  function setView(a, b) {
    if (durationS <= 0) return;
    const span = Math.min(durationS, Math.max(minSpan(), b - a));
    let s = b - a < minSpan() ? (a + b) / 2 - span / 2 : a;
    s = Math.max(0, Math.min(durationS - span, s));
    viewStartS = s;
    viewEndS = s + span;
    scheduleDraw();
  }

  // ── Minimap: drag the window, drag its edges, click to jump ──────────────

  minimapEl.dataset.tip = 'The whole track. Drag the lit window to move through it, drag its edges to zoom in time; a click elsewhere moves it there; double click: the whole track.';
  // The minimap's whole and the view on it: a file's track, or all a stream keeps.
  const mmStream = () => !!(store.get && store.get('_stream'));
  const mmWhole = () => (mmStream() ? streamWave.domain() : { lo: 0, hi: durationS });
  const mmView = () => (mmStream() ? streamWave.view() : { t0: viewStartS, t1: viewEndS });
  const mmSet = (a, b) => { if (mmStream()) { streamWave.setView(a, b); scheduleDraw(); } else setView(a, b); };
  const mmMinSpan = () => (mmStream() ? 0.25 : minSpan());
  const mmReady = () => (mmStream() ? !!streamWave.rings : durationS > 0);
  const mmTime = (e) => {
    const r = minimapEl.getBoundingClientRect(), { lo, hi } = mmWhole();
    return lo + Math.max(0, Math.min(1, (e.clientX - r.left) / Math.max(1, r.width))) * (hi - lo);
  };
  function mmHit(e) {
    const r = minimapEl.getBoundingClientRect(), { lo, hi } = mmWhole(), v = mmView();
    const x = e.clientX - r.left;
    const x0 = (v.t0 - lo) / Math.max(1e-9, hi - lo) * r.width;
    const x1 = (v.t1 - lo) / Math.max(1e-9, hi - lo) * r.width;
    const dl = Math.abs(x - x0), dr = Math.abs(x - x1);
    if (Math.min(dl, dr) <= EDGE_PX) return dl < dr ? 'left' : 'right';
    return x > x0 && x < x1 ? 'move' : 'outside';
  }
  const mmCursor = (hit) => (hit === 'left' || hit === 'right' ? 'ew-resize' : hit === 'move' ? 'grab' : 'pointer');
  let mmDrag = null;
  minimapEl.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || !mmReady()) return;
    if (!mmStream()) setFollow(false);   // the view placed by hand (a stream's: at its edge it follows)
    let hit = mmHit(e);
    const t = mmTime(e);
    if (hit === 'outside') {
      const v = mmView(), half = (v.t1 - v.t0) / 2;
      mmSet(t - half, t + half);
      hit = 'move';
    }
    const v = mmView();
    mmDrag = { hit, t0: t, a: v.t0, b: v.t1 };
    minimapEl.setPointerCapture(e.pointerId);
    minimapEl.style.cursor = hit === 'move' ? 'grabbing' : 'ew-resize';
    e.preventDefault();
  });
  minimapEl.addEventListener('pointermove', (e) => {
    if (!mmReady()) return;
    if (!mmDrag) { minimapEl.style.cursor = mmCursor(mmHit(e)); return; }
    const t = mmTime(e);
    const d = t - mmDrag.t0;
    if (mmDrag.hit === 'move') mmSet(mmDrag.a + d, mmDrag.b + d);
    else if (mmDrag.hit === 'left') mmSet(Math.min(t, mmDrag.b - mmMinSpan()), mmDrag.b);
    else mmSet(mmDrag.a, Math.max(t, mmDrag.a + mmMinSpan()));
  });
  const mmEnd = (e) => {
    if (!mmDrag) return;
    mmDrag = null;
    try { minimapEl.releasePointerCapture(e.pointerId); } catch { /* not captured */ }
    minimapEl.style.cursor = mmCursor(mmHit(e));
  };
  minimapEl.addEventListener('pointerup', mmEnd);
  minimapEl.addEventListener('pointercancel', mmEnd);
  minimapEl.addEventListener('dblclick', () => {
    if (mmStream()) { streamWave.all(); scheduleDraw(); return; }
    setFollow(true); setView(0, durationS);
  });

  // ── Detail view: wheel, drag, the dB scale ───────────────────────────────

  detailEl.dataset.tip = 'Wheel: zoom in time (about the playhead while following it, else at the pointer) · Shift+wheel or drag: move (stops following) · wheel or drag on the dB scale (or Alt+wheel): amplitude · double click: the whole track at full scale, following again (on the scale: full scale).';
  const plotTime = (clientX) => {
    const r = mainCanvas.getBoundingClientRect();
    return viewStartS + (clientX - r.left) / Math.max(1, r.width) * (viewEndS - viewStartS);
  };
  const onRuler = (e) => e.clientX - mainCanvas.getBoundingClientRect().left < RULER_W;
  const setAmp = (z) => { ampZoom = Math.max(1, Math.min(AMP_MAX, z)); scheduleDraw(); };
  // A stream: its own view (stream-wave.js) takes the wheel, the drag and the pointer.
  const fileTip = detailEl.dataset.tip;
  const fileMiniTip = minimapEl.dataset.tip;
  const onStream = () => !!(store.get && store.get('_stream'));
  const streamTime = (clientX) => {
    const r = mainCanvas.getBoundingClientRect(), v = streamWave.view();
    return v.t0 + (clientX - r.left) / Math.max(1, r.width) * (v.t1 - v.t0);
  };
  let sDrag = null;
  store.on?.('_stream', (on) => {
    if (!on) { streamWave.reset(); sDrag = null; }
    detailEl.dataset.tip = on ? STREAM_TIP : fileTip;
    minimapEl.dataset.tip = on ? STREAM_MINI_TIP : fileMiniTip;
    scheduleDraw();
  });
  mainCanvas.addEventListener('pointerleave', () => {
    if (onStream()) { streamWave.point(null); scheduleDraw(); } else filePtr = null;
  });
  // Another view's time (the spectrogram's, the loudness's): a line here.
  const onCursor = (e) => { const d = e.detail || {}; if (d.sourceViewId !== 'waveform') otherT = d.timeS ?? null; };
  bus.addEventListener('an:cursor:move', onCursor);
  mainCanvas.addEventListener('wheel', (e) => {
    if (onStream()) {
      e.preventDefault();
      streamWave.wheel(streamTime(e.clientX), e.deltaY);
      scheduleDraw();
      return;
    }
    if (durationS <= 0) return;
    e.preventDefault();
    if (e.altKey || onRuler(e)) { setAmp(ampZoom * (e.deltaY > 0 ? 0.8 : 1.25)); return; }
    if (e.shiftKey) {
      setFollow(false);
      const d = (viewEndS - viewStartS) * 0.1 * Math.sign(e.deltaY || e.deltaX);
      setView(viewStartS + d, viewEndS + d);
      return;
    }
    const k = e.deltaY > 0 ? 1.25 : 0.8;
    // Following: the zoom is about the playhead, which stays in the middle.
    const tc = follow && playheadS != null ? playheadS : plotTime(e.clientX);
    setView(tc - (tc - viewStartS) * k, tc + (viewEndS - tc) * k);
  }, { passive: false });
  let dDrag = null;
  mainCanvas.addEventListener('pointerdown', (e) => {
    if (e.button === 0 && onStream()) {
      sDrag = { x: e.clientX, from: streamWave.view(), w: mainCanvas.getBoundingClientRect().width };
      mainCanvas.setPointerCapture(e.pointerId);
      mainCanvas.style.cursor = 'grabbing';
      return;
    }
    if (e.button !== 0 || durationS <= 0) return;
    dDrag = { x: e.clientX, y: e.clientY, a: viewStartS, b: viewEndS, z: ampZoom, ruler: onRuler(e),
              w: mainCanvas.getBoundingClientRect().width };
    mainCanvas.setPointerCapture(e.pointerId);
    mainCanvas.style.cursor = dDrag.ruler ? 'ns-resize' : 'grabbing';
  });
  mainCanvas.addEventListener('pointermove', (e) => {
    if (onStream()) {
      if (sDrag) {
        const s = sDrag.from.t1 - sDrag.from.t0;
        streamWave.pan(sDrag.from, -(e.clientX - sDrag.x) / Math.max(1, sDrag.w) * s);
      } else {
        mainCanvas.style.cursor = 'grab';
      }
      streamWave.point(streamTime(e.clientX));
      scheduleDraw();
      return;
    }
    // The pointer's time: its line and the peaks under it (drawOverlay).
    filePtr = durationS > 0 ? plotTime(e.clientX) : null;
    if (!dDrag) { mainCanvas.style.cursor = durationS > 0 ? (onRuler(e) ? 'ns-resize' : 'grab') : ''; return; }
    if (dDrag.ruler) {
      // Up: closer (twice the amplitude per 40 px).
      setAmp(dDrag.z * Math.pow(2, (dDrag.y - e.clientY) / 40));
      return;
    }
    // A drag, not a click: the view moved by hand stops following (from
    // where it stands now, so it does not jump back first).
    if (follow && Math.abs(e.clientX - dDrag.x) > 3) {
      setFollow(false);
      dDrag.a = viewStartS; dDrag.b = viewEndS; dDrag.x = e.clientX;
    }
    const dt = -(e.clientX - dDrag.x) / Math.max(1, dDrag.w) * (dDrag.b - dDrag.a);
    setView(dDrag.a + dt, dDrag.b + dt);
  });
  const dEnd = (e) => {
    if (sDrag) {
      sDrag = null;
      try { mainCanvas.releasePointerCapture(e.pointerId); } catch { /* not captured */ }
      mainCanvas.style.cursor = 'grab';
      return;
    }
    if (!dDrag) return;
    dDrag = null;
    try { mainCanvas.releasePointerCapture(e.pointerId); } catch { /* not captured */ }
    mainCanvas.style.cursor = onRuler(e) ? 'ns-resize' : 'grab';
  };
  mainCanvas.addEventListener('pointerup', dEnd);
  mainCanvas.addEventListener('pointercancel', dEnd);
  mainCanvas.addEventListener('dblclick', (e) => {
    if (onStream()) { streamWave.all(); scheduleDraw(); return; }
    if (onRuler(e)) { setAmp(1); return; }
    ampZoom = 1;
    setFollow(true);
    setView(0, durationS);
  });

  // ── Init ──────────────────────────────────────────────────────────────────

  resizeAll();
  rafHandle = requestAnimationFrame(rafLoop);

  // ── Public interface ──────────────────────────────────────────────────────

  return {
    setData(payload) {
      // Called by window.js with decoded frame data
      if (payload && payload.nLods != null) {
        onTrack({ detail: { frame: payload } });
      }
    },

    setLayer(key, on) {
      layers[key] = on;
      drawDetail();
      drawMinimap();
    },

    onResize(w, h) {
      resizeAll();
      drawDetail();
      drawMinimap();
    },

    drawOverlay(cursor) {
      drawOverlay(cursor);
    },

    destroy() {
      destroyed = true;
      if (rafHandle) cancelAnimationFrame(rafHandle);
      bus.removeEventListener('an:track', onTrack);
      bus.removeEventListener('an:live',  onLive);
      bus.removeEventListener('an:tile',  onTile);
      bus.removeEventListener('an:track:change', onTrackChange);
      bus.removeEventListener('an:cursor:move', onCursor);
      container.innerHTML = '';
    },
  };
}
