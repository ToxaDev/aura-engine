/**
 * loudness-view.js — Loudness history canvas view.
 *
 * Shows the time-series loudness data for a track:
 *   - LUFS-S and LUFS-M lines for S, B, and O taps
 *   - Integrated LUFS horizontal line
 *   - LRA P10–P95 shaded bands (S and O)
 *   - Chain-change markers (vertical lines, color-coded by stage)
 *     with hover labels and per-segment LUFS-I values (Ctrl+click two markers)
 *   - Gate event ticks (layer 9, default off)
 *   - Playhead on overlayCanvas at 60 fps
 *
 * Two-canvas stack:
 *   mainCanvas    — redraws at 10 Hz (new O loudness points) or on track/chain events
 *   overlayCanvas — redraws at 60 fps: playhead + chain-marker hover labels
 *
 * Exports:
 *   create(container, ctx)  factory per FRONTEND-CONTRACT §4
 *   LAYER_REGISTRY          consumed by layers.js
 *
 * Dependencies:
 *   ./axes.js — axis drawing helpers
 */

import {
  normFromDb, normFromTime, setupCanvas,
  drawDbAxis, drawTimeAxis, drawLoudnessGrid,
} from './axes.js';
import { formatTime } from './format.js';
import { createStreamView } from './stream-view.js';

// A stream: the last five minutes at the live edge at first, over the 30 the
// page keeps (window.js); the plot's tip says how to move about them.
const STREAM_SPAN0_S = 300;
const STREAM_TIP = 'The last 30 minutes of the stream are kept. Ctrl+wheel: zoom in time · wheel: the dB range · drag: move · double click: the last 5 minutes at the live edge.';

/**
 * A LUFS series' value at time `t` (s): an array on the 10 Hz grid (index i
 * the window ending at (i + 4)·0.1 s, as drawn), or points {time_s, value}
 * (the nearest within 0.2 s). NaN where it has none.
 */
export function lufsAt(series, t) {
  if (!series || !series.length || !Number.isFinite(t)) return NaN;
  if (typeof series[0] === 'number') {
    const i = Math.round(t / 0.1 - 4);
    return i >= 0 && i < series.length && Number.isFinite(series[i]) ? series[i] : NaN;
  }
  let lo = 0, hi = series.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (series[mid].time_s < t) lo = mid + 1; else hi = mid;
  }
  if (lo > 0 && Math.abs(series[lo - 1].time_s - t) <= Math.abs(series[lo].time_s - t)) lo--;
  const p = series[lo];
  return Math.abs(p.time_s - t) <= 0.2 && Number.isFinite(p.value) ? p.value : NaN;
}

// ---------------------------------------------------------------------------
// Layer registry
// ---------------------------------------------------------------------------

export const LAYER_REGISTRY = [
  {
    id:             'lufs_s_o',
    label:          'LUFS-S O',
    colorToken:     '--an-o',
    defaultOn:      true,
    shortcut:       '1',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'lufs_s_s',
    label:          'LUFS-S S',
    colorToken:     '--an-s',
    defaultOn:      true,
    shortcut:       '2',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'lufs_m_o',
    label:          'LUFS-M O',
    colorToken:     '--an-o',
    defaultOn:      false,
    shortcut:       '3',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'lufs_m_s',
    label:          'LUFS-M S',
    colorToken:     '--an-s',
    defaultOn:      false,
    shortcut:       '4',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'lufs_i_line',
    label:          'LUFS-I',
    colorToken:     '--an-s',
    defaultOn:      true,
    shortcut:       '5',
    provenanceAware: false,
    axisRole:       'left',
  },
  {
    id:             'lra_band_s',
    label:          'LRA S',
    colorToken:     '--an-s',
    defaultOn:      true,
    shortcut:       '6',
    provenanceAware: false,
    axisRole:       null,
  },
  {
    id:             'lra_band_o',
    label:          'LRA O',
    colorToken:     '--an-o',
    defaultOn:      true,
    shortcut:       '7',
    provenanceAware: false,
    axisRole:       null,
  },
  {
    id:             'gate_ticks',
    label:          'Gate events',
    colorToken:     '--an-h',
    defaultOn:      false,
    shortcut:       '9',
    provenanceAware: false,
    axisRole:       null,
  },
];

// ---------------------------------------------------------------------------
// Stage colors for chain-change markers (matching §11.3 --an-mark-* tokens)
// ---------------------------------------------------------------------------
const STAGE_MARK_COLORS = [
  '#f59e0b', // 0 FIR  amber
  '#4ade80', // 1 DC   green
  '#a78bfa', // 2 ISP  violet
  '#22d3ee', // 3 SUB  cyan
  '#fb923c', // 4 AHR  orange
  '#64748b', // 5 AA   slate
  '#2dd4bf', // 6 XTC  teal
  '#a78bfa', // 7 HP   violet
  '#f59e0b', // 8 TFS  amber
  '#a78bfa', // 9 ISP_OUT violet
];

// ---------------------------------------------------------------------------
// Canvas layout constants (CSS pixels)
// ---------------------------------------------------------------------------
const AXIS_LEFT_W   = 44;   // dB/LUFS labels
const AXIS_RIGHT_W  = 8;
const AXIS_BOTTOM_H = 28;   // time labels
const AXIS_TOP_H    = 8;
const MARKER_HIT_R  = 8;    // pixels around a marker line for hover detection

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget, cursor: object }} ctx
 */
export function create(container, ctx) {
  const { store, bus, cursor } = ctx;

  // ------------------------------------------------------------------
  // DOM
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

  const interactionEl = document.createElement('div');
  interactionEl.style.cssText =
    'position:absolute;top:0;left:0;width:100%;height:100%;cursor:crosshair;';
  container.appendChild(interactionEl);

  const mainCtx = mainCanvas.getContext('2d');
  const ovCtx   = overlayCanvas.getContext('2d');

  // ------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------

  // View time range (seconds). 0 = full track (t0=0, t1=durationS).
  let viewT0 = 0;
  let viewT1 = 0; // 0 = auto (uses durationS)

  // dBLUFS axis range
  let dbTop   = 0;
  let dbFloor = -36;

  // Layer visibility
  const layerOn = {};
  for (const entry of LAYER_REGISTRY) {
    layerOn[entry.id] = entry.defaultOn;
  }

  // Chain-change marker selection (for Ctrl+click → per-segment LUFS-I)
  let selectedMarkers = []; // up to 2 indices into store.chain.marks

  // Hover state: index of hovered marker, or -1
  let hoveredMarkerIdx = -1;

  // Cached layout (physical pixels)
  let layout = null;

  // Playhead position (seconds, received via cursor)
  let playheadTime = null;

  // LRA band bounds (set on an:track): S from hist_s P10/P95; O from the O
  // pass's own short-term series, so the band sits on the O line. (It was
  // B's histogram in O's colour: after the output gain B is several dB above
  // O, and a second blue band appeared above the O line once B was done.)
  let lraS_p10 = NaN;
  let lraS_p95 = NaN;
  let lraO_p10 = NaN;
  let lraO_p95 = NaN;

  // Cached LUFS-I values (store.metrics.s/o[0] may stay NaN; read from frames)
  let lufsISCached = NaN;  // S integrated LUFS (from track frame)
  let lufsIOCached = NaN;  // O integrated LUFS (from live frame)

  // A stream's view: following the live edge, or zoomed and moved by hand
  // over what the page keeps (stream-view.js).
  const sview = createStreamView({ span0: STREAM_SPAN0_S, spanMin: 10, domain: streamDomain });
  let sDrag = null;

  // The pointer's time here (the readout's), and another view's (a line only).
  let hoverT = null;
  let otherT = null;

  // ------------------------------------------------------------------
  // Layout helper
  // ------------------------------------------------------------------

  function computeLayout(scale, cssW, cssH) {
    return {
      x: AXIS_LEFT_W * scale,
      y: AXIS_TOP_H * scale,
      w: cssW * scale - (AXIS_LEFT_W + AXIS_RIGHT_W) * scale,
      h: cssH * scale - (AXIS_TOP_H + AXIS_BOTTOM_H) * scale,
      scale,
      cssW,
      cssH,
    };
  }

  // ------------------------------------------------------------------
  // Coordinate helpers
  // ------------------------------------------------------------------

  /** What the page keeps of a stream's loudness: its oldest point to the live edge. */
  function streamDomain() {
    const s = store.series || {};
    const first = (p) => (p && p.length ? p[0].time_s : Infinity);
    const last = (p) => (p && p.length ? p[p.length - 1].time_s : 0);
    const hi = Math.max(store._posS ?? 0, last(s.lufs_s_o), last(s.lufs_s_s_live));
    return { lo: Math.max(0, Math.min(hi, first(s.lufs_s_o), first(s.lufs_s_s_live))), hi };
  }

  function getTimeRange() {
    // A stream: its own view, the last five minutes at the live edge at first.
    if (store._stream) {
      let v = sview.view();
      // Another stream (its curves start again): a view placed by hand there is gone.
      const { lo, hi } = streamDomain();
      if (!sview.following && (v.t0 > hi || v.t1 < lo)) { sview.reset(); v = sview.view(); }
      return { t0: v.t0, t1: v.t1 };
    }
    const t0 = viewT0;
    const t1 = viewT1 > 0 ? viewT1 : (store.durationS || 300);
    return { t0, t1 };
  }

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

  function toTimeS(xNorm) {
    const { t0, t1 } = getTimeRange();
    return t0 + plotNorm(xNorm) * (t1 - t0);
  }

  // ------------------------------------------------------------------
  // Drawing
  // ------------------------------------------------------------------

  function drawMain() {
    if (!mainCtx || !layout) return;
    const { cssW, cssH, scale } = layout;
    const W = cssW * scale;
    const H = cssH * scale;

    mainCtx.clearRect(0, 0, W, H);

    const lay = computeLayout(scale, cssW, cssH);
    const { t0, t1 } = getTimeRange();
    const series = store.series || {};
    const metrics = store.metrics || {};
    // For tests and bug reports: the time shown.
    container.dataset.time = `${t0.toFixed(3)}..${t1.toFixed(3)}`;

    // Grid
    drawLoudnessGrid(mainCtx, lay, t0, t1, dbFloor, dbTop);

    // Axes
    drawDbAxis(mainCtx, lay, dbFloor, dbTop, { labelSuffix: '' });
    drawTimeAxis(mainCtx, lay, t0, t1);

    // -- LRA bands (drawn under the loudness lines) --

    // S LRA band: P10–P95 computed from hist_s on an:track
    if (layerOn.lra_band_s) {
      drawLraBand(mainCtx, lay, t0, t1,
        lraS_p10, lraS_p95,
        '#4ade80', 0.20);
    }

    // O LRA band: only once the O pass has the whole track.
    if (layerOn.lra_band_o) {
      drawLraBand(mainCtx, lay, t0, t1,
        lraO_p10, lraO_p95,
        '#38bdf8', 0.15);
    }

    // -- LUFS-I horizontal line(s) --
    if (layerOn.lufs_i_line) {
      // Use locally cached values (store.metrics.s/o may stay NaN if window.js
      // did not write S metrics from the track frame; cache from frame directly).
      if (isFinite(lufsISCached)) {
        drawHorizLine(mainCtx, lay, lufsISCached, '#4ade80', 0.55, [4, 4], scale);
      }
      if (isFinite(lufsIOCached)) {
        drawHorizLine(mainCtx, lay, lufsIOCached, '#38bdf8', 0.55, [4, 4], scale);
      }
    }

    // -- LUFS-S and LUFS-M lines --
    if (layerOn.lufs_s_s && series.lufs_s_s) {
      drawLufsLine(mainCtx, lay, series.lufs_s_s, t0, t1, '#4ade80', 0.90, false, scale);
    }
    if (layerOn.lufs_m_s && series.lufs_m_s) {
      drawLufsLine(mainCtx, lay, series.lufs_m_s, t0, t1, '#4ade80', 0.55, true, scale);
    }
    // A stream: S measured live beside O (its STRM tail).
    if (store._stream && layerOn.lufs_s_s && series.lufs_s_s_live) {
      drawLufsLinePoints(mainCtx, lay, series.lufs_s_s_live, t0, t1, '#4ade80', 0.90, false, scale);
    }
    if (store._stream && layerOn.lufs_m_s && series.lufs_m_s_live) {
      drawLufsLinePoints(mainCtx, lay, series.lufs_m_s_live, t0, t1, '#4ade80', 0.55, true, scale);
    }
    if (layerOn.lufs_s_b && series.lufs_s_b) {
      drawLufsLine(mainCtx, lay, series.lufs_s_b, t0, t1, '#94a3b8', 0.70, false, scale);
    }
    // O: the whole track from the O pass once it is done, the points heard until then.
    if (layerOn.lufs_s_o && series.lufs_s_o_whole) {
      drawLufsLine(mainCtx, lay, series.lufs_s_o_whole, t0, t1, '#38bdf8', 0.90, false, scale);
    } else if (layerOn.lufs_s_o && series.lufs_s_o) {
      drawLufsLinePoints(mainCtx, lay, series.lufs_s_o, t0, t1, '#38bdf8', 0.90, false, scale);
    }
    if (layerOn.lufs_m_o && series.lufs_m_o_whole) {
      drawLufsLine(mainCtx, lay, series.lufs_m_o_whole, t0, t1, '#38bdf8', 0.55, true, scale);
    } else if (layerOn.lufs_m_o && series.lufs_m_o) {
      drawLufsLinePoints(mainCtx, lay, series.lufs_m_o, t0, t1, '#38bdf8', 0.55, true, scale);
    }

    // -- Gate ticks --
    if (layerOn.gate_ticks && series.gateTicks) {
      drawGateTicks(mainCtx, lay, series.gateTicks, t0, t1, scale);
    }

    // -- Chain-change markers --
    drawChainMarkers(mainCtx, lay, t0, t1, scale);

    // -- Per-segment LUFS-I annotation (when two markers selected) --
    if (selectedMarkers.length === 2) {
      drawSegmentAnnotation(mainCtx, lay, t0, t1, scale);
    }
  }

  // ------------------------------------------------------------------
  // Drawing helpers
  // ------------------------------------------------------------------

  /**
   * Draw a Float32Array loudness series sampled at 10 Hz. Index i is the window
   * ending at (i + 4) × 0.1 s (four 100 ms blocks make the momentary one): drawn
   * where a meter shows it, as the live points are.
   */
  function drawLufsLine(ctx2d, lay, arr, t0, t1, color, alpha, dashed, scale) {
    if (!arr || arr.length < 2) return;
    const { x, y, w, h } = lay;

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 1.5 * scale;
    if (dashed) ctx2d.setLineDash([6 * scale, 3 * scale]);

    ctx2d.beginPath();
    let started = false;
    const n = arr.length;
    for (let i = 0; i < n; i++) {
      const t = (i + 4) * 0.1; // 10 Hz, the window's end
      if (t < t0 || t > t1) continue;
      const v = arr[i];
      if (!isFinite(v)) { started = false; continue; }
      const nx = normFromTime(t, t0, t1);
      const ny = normFromDb(v, dbTop, dbFloor);
      const px = x + nx * w;
      const py = y + Math.max(0, Math.min(1, ny)) * h;
      if (!started) { ctx2d.moveTo(px, py); started = true; }
      else ctx2d.lineTo(px, py);
    }
    ctx2d.stroke();
    ctx2d.restore();
  }

  /** Draw a [{time_s, value}] point series (live O points from AAN1). */
  function drawLufsLinePoints(ctx2d, lay, pts, t0, t1, color, alpha, dashed, scale) {
    if (!pts || pts.length < 2) return;
    const { x, y, w, h } = lay;

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 1.5 * scale;
    if (dashed) ctx2d.setLineDash([6 * scale, 3 * scale]);

    ctx2d.beginPath();
    let started = false;
    for (const pt of pts) {
      const { time_s, lufs_s, value } = pt;
      const t = time_s != null ? time_s : (pt.t != null ? pt.t : null);
      const v = lufs_s != null ? lufs_s : value;
      if (t === null || !isFinite(v)) { started = false; continue; }
      if (t < t0 || t > t1) continue;
      const nx = normFromTime(t, t0, t1);
      const ny = normFromDb(v, dbTop, dbFloor);
      const px = x + nx * w;
      const py = y + Math.max(0, Math.min(1, ny)) * h;
      if (!started) { ctx2d.moveTo(px, py); started = true; }
      else ctx2d.lineTo(px, py);
    }
    ctx2d.stroke();
    ctx2d.restore();
  }

  /** Draw an LRA P10–P95 shaded horizontal band. */
  function drawLraBand(ctx2d, lay, t0, t1, p10, p95, color, alpha) {
    if (!isFinite(p10) || !isFinite(p95)) return;
    const { x, y, w, h } = lay;
    const py10 = y + normFromDb(p10, dbTop, dbFloor) * h;
    const py95 = y + normFromDb(p95, dbTop, dbFloor) * h;
    const bandY = Math.min(py10, py95);
    const bandH = Math.abs(py10 - py95);

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    ctx2d.fillStyle = color;
    ctx2d.fillRect(x, bandY, w, bandH);
    ctx2d.restore();
  }

  /** Draw a horizontal dashed/dotted LUFS-I line. */
  function drawHorizLine(ctx2d, lay, db, color, alpha, dash, scale) {
    if (!isFinite(db)) return;
    const { x, y, w, h } = lay;
    const ny = normFromDb(db, dbTop, dbFloor);
    if (ny < 0 || ny > 1) return;
    const py = y + ny * h;

    ctx2d.save();
    ctx2d.globalAlpha = alpha;
    ctx2d.strokeStyle = color;
    ctx2d.lineWidth = 1 * scale;
    ctx2d.setLineDash(dash.map(v => v * scale));
    ctx2d.beginPath();
    ctx2d.moveTo(x, py);
    ctx2d.lineTo(x + w, py);
    ctx2d.stroke();
    ctx2d.restore();
  }

  /** Draw gate-event ticks at bottom of plot. */
  function drawGateTicks(ctx2d, lay, ticks, t0, t1, scale) {
    const { x, y, w, h } = lay;
    const tickH = 5 * scale;
    ctx2d.save();
    ctx2d.strokeStyle = '#f59e0b';
    ctx2d.lineWidth = 1.5 * scale;
    ctx2d.globalAlpha = 0.7;
    for (const t of ticks) {
      if (t < t0 || t > t1) continue;
      const nx = normFromTime(t, t0, t1);
      const px = x + nx * w;
      ctx2d.beginPath();
      ctx2d.moveTo(px, y + h - tickH);
      ctx2d.lineTo(px, y + h);
      ctx2d.stroke();
    }
    ctx2d.restore();
  }

  /** Draw chain-change marker lines on mainCanvas. */
  function drawChainMarkers(ctx2d, lay, t0, t1, scale) {
    const marks = (store.chain && store.chain.marks) ? store.chain.marks : [];
    if (marks.length === 0) return;
    const { x, y, w, h } = lay;

    ctx2d.save();
    ctx2d.lineWidth = 1.5 * scale;
    ctx2d.setLineDash([3 * scale, 3 * scale]);

    for (let i = 0; i < marks.length; i++) {
      const mark = marks[i];
      const { time_s, stageId } = mark;
      if (time_s < t0 || time_s > t1) continue;

      const nx = normFromTime(time_s, t0, t1);
      const px = x + nx * w;
      const color = STAGE_MARK_COLORS[stageId] || '#64748b';

      const isSelected = selectedMarkers.includes(i);
      const isHovered  = hoveredMarkerIdx === i;

      ctx2d.globalAlpha = isSelected || isHovered ? 1.0 : 0.65;
      ctx2d.strokeStyle = color;
      ctx2d.lineWidth = isSelected ? 2 * scale : 1.5 * scale;

      ctx2d.beginPath();
      ctx2d.moveTo(px, y);
      ctx2d.lineTo(px, y + h);
      ctx2d.stroke();
    }

    ctx2d.restore();
  }

  /** Draw per-segment LUFS-I annotation row between two selected markers. */
  function drawSegmentAnnotation(ctx2d, lay, t0, t1, scale) {
    if (selectedMarkers.length < 2) return;
    const marks = (store.chain && store.chain.marks) ? store.chain.marks : [];
    const [i1, i2] = selectedMarkers.sort((a, b) => a - b);
    const m1 = marks[i1];
    const m2 = marks[i2];
    if (!m1 || !m2) return;

    const { x, y, w } = lay;
    const font = `${11 * scale}px ui-monospace, "Cascadia Mono", Consolas, monospace`;

    // Mid-point between markers
    const nx1 = normFromTime(m1.time_s, t0, t1);
    const nx2 = normFromTime(m2.time_s, t0, t1);
    const midX = x + ((nx1 + nx2) / 2) * w;

    const lufsI = (store.segmentLufsI && store.segmentLufsI[`${i1}-${i2}`]) || null;
    const text = lufsI != null ? `I: ${lufsI.toFixed(2)} LUFS` : 'Computing...';

    ctx2d.save();
    ctx2d.font = font;
    ctx2d.textAlign = 'center';
    ctx2d.textBaseline = 'top';

    const metrics2d = ctx2d.measureText(text);
    const pillW = metrics2d.width + 12 * scale;
    const pillH = 16 * scale;
    const pillX = midX - pillW / 2;
    const pillY = y + 4 * scale;

    ctx2d.fillStyle = 'rgba(11,25,44,0.90)';
    ctx2d.beginPath();
    ctx2d.roundRect(pillX, pillY, pillW, pillH, 4 * scale);
    ctx2d.fill();

    ctx2d.fillStyle = '#e2e8f0';
    ctx2d.fillText(text, midX, pillY + 3 * scale);
    ctx2d.restore();
  }

  // ------------------------------------------------------------------
  // Overlay canvas — playhead + marker hover labels
  // ------------------------------------------------------------------

  function drawOverlay(cursorArg) {
    if (!ovCtx || !layout) return;
    const { cssW, cssH, scale } = layout;
    const W = cssW * scale;
    const H = cssH * scale;

    ovCtx.clearRect(0, 0, W, H);

    const lay = computeLayout(scale, cssW, cssH);
    const { t0, t1 } = getTimeRange();

    // Playhead: where the sound is (the player's position, as the
    // spectrogram draws it); the last live loudness point only without one.
    const pos = store.get ? store.get('_posS') : null;
    const pt = pos != null ? pos : playheadTime;
    if (pt != null && pt >= t0 && pt <= t1) {
      const nx = normFromTime(pt, t0, t1);
      const px = lay.x + nx * lay.w;

      ovCtx.save();
      ovCtx.strokeStyle = 'rgba(255,255,255,0.8)';
      ovCtx.lineWidth = 2 * scale;
      ovCtx.beginPath();
      ovCtx.moveTo(px, lay.y);
      ovCtx.lineTo(px, lay.y + lay.h);
      ovCtx.stroke();
      ovCtx.restore();
    }

    // The time cursor: the pointer here, or another view's time. (It read
    // store.cursor, which nothing sets: the line never showed.)
    const timeS = hoverT ?? otherT ?? (cursorArg && cursorArg.timeS);
    if (timeS != null && timeS >= t0 && timeS <= t1) {
      const nx = normFromTime(timeS, t0, t1);
      const px = lay.x + nx * lay.w;

      ovCtx.save();
      ovCtx.strokeStyle = 'rgba(255,255,255,0.4)';
      ovCtx.lineWidth = 1 * scale;
      ovCtx.setLineDash([4 * scale, 4 * scale]);
      ovCtx.beginPath();
      ovCtx.moveTo(px, lay.y);
      ovCtx.lineTo(px, lay.y + lay.h);
      ovCtx.stroke();
      ovCtx.restore();
      if (hoverT != null && timeS >= 0) drawReadout(ovCtx, lay, px, timeS, scale);
    }

    // Hovered chain-change marker tooltip
    if (hoveredMarkerIdx >= 0) {
      drawMarkerTooltip(ovCtx, lay, t0, t1, scale);
    }
  }

  /** The pointer's time and S's and O's LUFS-S there, at the plot's foot. */
  function drawReadout(ctx2d, lay, px, t, scale) {
    const s = store.series || {};
    const sSeries = store._stream ? s.lufs_s_s_live : s.lufs_s_s;
    const oSeries = s.lufs_s_o_whole || s.lufs_s_o;
    const val = (v) => (Number.isFinite(v) ? v.toFixed(1).replace('-', '−') : '—');
    const text = `${formatTime(t)} · S ${val(lufsAt(sSeries, t))} LUFS · O ${val(lufsAt(oSeries, t))} LUFS`;
    ctx2d.save();
    ctx2d.font = `${10 * scale}px ui-monospace, "Cascadia Mono", Consolas, monospace`;
    const tw = ctx2d.measureText(text).width;
    const pillW = tw + 10 * scale, pillH = 16 * scale;
    const pillX = Math.max(lay.x, Math.min(lay.x + lay.w - pillW, px + 6 * scale));
    const pillY = lay.y + lay.h - pillH - 4 * scale;
    ctx2d.fillStyle = 'rgba(11,25,44,0.88)';
    ctx2d.fillRect(pillX, pillY, pillW, pillH);
    ctx2d.fillStyle = '#e2e8f0';
    ctx2d.textBaseline = 'middle';
    ctx2d.textAlign = 'left';
    ctx2d.fillText(text, pillX + 5 * scale, pillY + pillH / 2);
    ctx2d.restore();
  }

  function drawMarkerTooltip(ctx2d, lay, t0, t1, scale) {
    const marks = (store.chain && store.chain.marks) ? store.chain.marks : [];
    const mark = marks[hoveredMarkerIdx];
    if (!mark) return;

    const { time_s, fromToken, toToken, stageId } = mark;
    const nx = normFromTime(time_s, t0, t1);
    const px = lay.x + nx * lay.w;

    const m = mark.fromToken || '';
    const s = mark.toToken || '';
    const stageName = ['FIR', 'DC', 'ISP', 'SUB', 'AHR', 'AA', 'XTC', 'HP', 'TFS', 'ISP_OUT'][stageId] || 'Stage';

    const mm = Math.floor(time_s / 60);
    const ss = (time_s % 60).toFixed(1);
    const timeStr = mm + ':' + String(ss).padStart(4, '0');
    const text = stageName + ': ' + m + ' → ' + s + ' at ' + timeStr;

    const font = `${11 * scale}px ui-monospace, "Cascadia Mono", Consolas, monospace`;
    ctx2d.save();
    ctx2d.font = font;
    const textW = ctx2d.measureText(text).width;
    const pillW = textW + 12 * scale;
    const pillH = 18 * scale;

    // Position tooltip above the marker line
    let pillX = px - pillW / 2;
    if (pillX < lay.x) pillX = lay.x;
    if (pillX + pillW > lay.x + lay.w) pillX = lay.x + lay.w - pillW;
    const pillY = lay.y + 4 * scale;

    ctx2d.fillStyle = 'rgba(11,25,44,0.92)';
    ctx2d.strokeStyle = STAGE_MARK_COLORS[stageId] || '#64748b';
    ctx2d.lineWidth = 1 * scale;
    ctx2d.beginPath();
    ctx2d.roundRect(pillX, pillY, pillW, pillH, 4 * scale);
    ctx2d.fill();
    ctx2d.stroke();

    ctx2d.fillStyle = '#e2e8f0';
    ctx2d.textBaseline = 'middle';
    ctx2d.textAlign = 'left';
    ctx2d.fillText(text, pillX + 6 * scale, pillY + pillH / 2);
    ctx2d.restore();
  }

  // ------------------------------------------------------------------
  // Mouse interaction — hover and Ctrl+click for chain markers
  // ------------------------------------------------------------------

  function getMarkerIdxAt(clientX) {
    if (!layout) return -1;
    const rect = interactionEl.getBoundingClientRect();
    const xNorm = plotNormAt(clientX);
    const { t0, t1 } = getTimeRange();
    const t = t0 + xNorm * (t1 - t0);
    const scale = layout.scale;
    const hitR = MARKER_HIT_R;

    const marks = (store.chain && store.chain.marks) ? store.chain.marks : [];
    let best = -1;
    let bestD = Infinity;

    for (let i = 0; i < marks.length; i++) {
      const nx = normFromTime(marks[i].time_s, t0, t1);
      const markerClientX = clientXAtPlotNorm(nx);
      const d = Math.abs(clientX - markerClientX);
      if (d < hitR && d < bestD) { bestD = d; best = i; }
    }
    return best;
  }

  function onMouseMove(e) {
    const idx = getMarkerIdxAt(e.clientX);
    if (idx !== hoveredMarkerIdx) {
      hoveredMarkerIdx = idx;
      // Redraw main to update marker highlight
      drawMain();
    }
    // Emit time cursor
    const rect = interactionEl.getBoundingClientRect();
    const xNorm = plotNormAt(e.clientX);
    const { t0, t1 } = getTimeRange();
    const t = t0 + xNorm * (t1 - t0);
    hoverT = t;
    // (Object.assign cannot set a CustomEvent's detail: it threw on every move.)
    bus.dispatchEvent(new CustomEvent('an:cursor:move', {
      detail: { freqHz: null, timeS: t, sourceViewId: 'loudness' },
    }));
  }

  function onMouseLeave() {
    hoverT = null;
    // The other views' line goes with the pointer.
    bus.dispatchEvent(new CustomEvent('an:cursor:move', {
      detail: { freqHz: null, timeS: null, sourceViewId: 'loudness' },
    }));
    if (hoveredMarkerIdx !== -1) {
      hoveredMarkerIdx = -1;
      drawMain();
    }
  }

  /** Another view's pointer: its time, as a line here. */
  function onCursor(e) {
    const d = e.detail || {};
    if (d.sourceViewId !== 'loudness') otherT = d.timeS ?? null;
  }
  bus.addEventListener('an:cursor:move', onCursor);

  // A stream: a drag moves the view over what is kept (it stops following).
  interactionEl.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || !store._stream || e.ctrlKey || e.metaKey) return;
    sDrag = { x: e.clientX, from: sview.view() };
    interactionEl.setPointerCapture?.(e.pointerId);
  });
  interactionEl.addEventListener('pointermove', (e) => {
    if (!sDrag || !layout) return;
    const lay = computeLayout(1, layout.cssW, layout.cssH);
    const rect = interactionEl.getBoundingClientRect();
    const px = (e.clientX - sDrag.x) * layout.cssW / Math.max(1, rect.width);
    sview.pan(sDrag.from, -px / Math.max(1, lay.w) * (sDrag.from.t1 - sDrag.from.t0));
    drawMain();
  });
  const sDragEnd = (e) => {
    if (!sDrag) return;
    sDrag = null;
    try { interactionEl.releasePointerCapture?.(e.pointerId); } catch { /* not captured */ }
  };
  interactionEl.addEventListener('pointerup', sDragEnd);
  interactionEl.addEventListener('pointercancel', sDragEnd);

  // A stream: its view starts at the live edge; the plot says how to move about it.
  store.on?.('_stream', (on) => {
    sview.reset();
    if (on) interactionEl.setAttribute('data-tip', STREAM_TIP);
    else interactionEl.removeAttribute('data-tip');
    drawMain();
  });
  if (store._stream) interactionEl.setAttribute('data-tip', STREAM_TIP);

  function onClick(e) {
    if (!e.ctrlKey && !e.metaKey) return;
    const idx = getMarkerIdxAt(e.clientX);
    if (idx < 0) return;

    if (selectedMarkers.includes(idx)) {
      selectedMarkers = selectedMarkers.filter(i => i !== idx);
    } else {
      selectedMarkers = [...selectedMarkers, idx].slice(-2);
    }
    drawMain();
  }

  interactionEl.addEventListener('mousemove', onMouseMove, { passive: true });
  interactionEl.addEventListener('mouseleave', onMouseLeave);
  interactionEl.addEventListener('click', onClick);

  // Zoom/pan via scroll
  interactionEl.addEventListener('wheel', (e) => {
    e.preventDefault();
    const { t0, t1 } = getTimeRange();
    const range = t1 - t0;
    const factor = e.deltaY > 0 ? 1.2 : 0.83;
    const rect = interactionEl.getBoundingClientRect();
    const xNorm = plotNormAt(e.clientX);
    const tCenter = t0 + xNorm * range;

    if (store._stream && (e.ctrlKey || e.metaKey || e.shiftKey)) {
      // A stream: zoom in time at the pointer (Ctrl), or move (Shift), over what is kept.
      if (e.shiftKey) sview.pan({ t0, t1 }, range * 0.1 * Math.sign(e.deltaY || e.deltaX));
      else sview.wheel(tCenter, e.deltaY);
    } else if (e.ctrlKey || e.metaKey) {
      // Zoom time axis
      const newRange = Math.max(10, Math.min(store.durationS || 600, range * factor));
      viewT0 = Math.max(0, tCenter - xNorm * newRange);
      viewT1 = Math.min(store.durationS || 600, viewT0 + newRange);
    } else {
      // Zoom Y (dB range)
      const dRange = dbTop - dbFloor;
      const newDRange = Math.max(6, Math.min(60, dRange * factor));
      const mid = (dbTop + dbFloor) / 2;
      dbTop   = mid + newDRange / 2;
      dbFloor = mid - newDRange / 2;
    }
    drawMain();
  }, { passive: false });

  interactionEl.addEventListener('dblclick', () => {
    viewT0 = 0;
    viewT1 = 0;
    sview.reset();   // a stream: the last five minutes at the live edge
    dbTop   = 0;
    dbFloor = -36;
    selectedMarkers = [];
    drawMain();
  });

  // ------------------------------------------------------------------
  // Helpers
  // ------------------------------------------------------------------

  /**
   * Compute nearest-rank percentile from a probability-density histogram.
   * @param {Float32Array} pdf - probability density array
   * @param {number} p - percentile in [0,1]
   * @param {number} lufsMin - LUFS value at bin 0
   * @param {number} lufsMax - LUFS value at bin nBins
   */
  /**
   * P10 and P95 of a short-term loudness series with the EBU R128 LRA gates
   * (absolute −70 LUFS, relative −20 LU under the gated mean): the edges of
   * the loudness range. [NaN, NaN] without a series.
   */
  function _lraFromShortTerm(series) {
    if (!series || series.length === 0) return [NaN, NaN];
    const abs = [];
    for (const v of series) if (Number.isFinite(v) && v > -70) abs.push(v);
    if (abs.length === 0) return [NaN, NaN];
    const mean = 10 * Math.log10(abs.reduce((a, v) => a + Math.pow(10, v / 10), 0) / abs.length);
    const kept = abs.filter(v => v > mean - 20).sort((a, b) => a - b);
    if (kept.length === 0) return [NaN, NaN];
    const at = p => kept[Math.min(kept.length - 1, Math.max(0, Math.round(p * (kept.length - 1))))];
    return [at(0.10), at(0.95)];
  }

  function _percentileFromPdf(pdf, p, lufsMin, lufsMax) {
    if (!pdf || pdf.length === 0) return NaN;
    const n = pdf.length;
    let total = 0;
    for (let i = 0; i < n; i++) total += pdf[i];
    if (total <= 0) return lufsMin;
    const target = p * total;
    let cumulative = 0;
    for (let i = 0; i < n; i++) {
      cumulative += pdf[i];
      if (cumulative >= target) {
        return lufsMin + (i / n) * (lufsMax - lufsMin);
      }
    }
    return lufsMax;
  }

  // ------------------------------------------------------------------
  // Bus subscriptions
  // ------------------------------------------------------------------

  function onLive(e) {
    // New O loudness points appended — redraw to extend the O line.
    // Also cache LUFS-I from live_metrics[0] (metric ID 0 = LUFS-I).
    const frame = e.detail && e.detail.frame;
    if (frame && frame.live_metrics) {
      const v = frame.live_metrics[0];
      if (isFinite(v)) lufsIOCached = v;
    }
    drawMain();
  }

  function onTrack(e) {
    // S/B series arrived; compute LRA band P10/P95 from histograms.
    // Also cache LUFS-I S from s_metrics[0] (metric ID 0 = LUFS-I).
    const frame = e.detail && e.detail.frame;
    if (frame) {
      if (frame.s_metrics) {
        const v = frame.s_metrics[0];
        if (isFinite(v)) lufsISCached = v;
      }
      const LUFS_MIN = -80;
      const LUFS_MAX = 0;
      if (frame.hist_s && frame.hist_s.length > 0) {
        lraS_p10 = _percentileFromPdf(frame.hist_s, 0.10, LUFS_MIN, LUFS_MAX);
        lraS_p95 = _percentileFromPdf(frame.hist_s, 0.95, LUFS_MIN, LUFS_MAX);
      }
      [lraO_p10, lraO_p95] = _lraFromShortTerm(frame.lufs_s_series_o);
    }
    selectedMarkers = [];
    drawMain();
  }

  function onChainChange(e) {
    // New chain marker — redraw immediately
    drawMain();
  }

  function onTrackChange(e) {
    // Clear state for new track
    viewT0 = 0;
    viewT1 = 0;
    selectedMarkers = [];
    hoveredMarkerIdx = -1;
    drawMain();
  }

  function onLayerToggle(e) {
    const { layerId, on } = e.detail || {};
    if (layerId in layerOn) {
      layerOn[layerId] = on;
      drawMain();
    }
  }

  // Playhead position arrives via an:live frame's last O loudness point
  function updatePlayhead(e) {
    const frame = e.detail && e.detail.frame;
    if (!frame) return;
    // The last O loudness point timestamp is the latest played position
    if (frame.loud_pts && frame.loud_pts.length > 0) {
      playheadTime = frame.loud_pts[frame.loud_pts.length - 1].time_s;
    }
  }

  bus.addEventListener('an:live',         onLive);
  bus.addEventListener('an:live',         updatePlayhead);
  bus.addEventListener('an:track',        onTrack);
  bus.addEventListener('an:chain:change', onChainChange);
  bus.addEventListener('an:track:change', onTrackChange);
  bus.addEventListener('an:layer:toggle', onLayerToggle);

  // ------------------------------------------------------------------
  // View lifecycle API (FRONTEND-CONTRACT §4)
  // ------------------------------------------------------------------

  function setData(payload) {
    drawMain();
  }

  function setLayer(key, on) {
    if (key in layerOn) {
      layerOn[key] = on;
      drawMain();
    }
  }

  function onResize(w, h) {
    const scale = window.devicePixelRatio || 1;
    mainCanvas.width   = Math.round(w * scale);
    mainCanvas.height  = Math.round(h * scale);
    overlayCanvas.width  = Math.round(w * scale);
    overlayCanvas.height = Math.round(h * scale);
    layout = computeLayout(scale, w, h);
    drawMain();
  }

  // The overlay (the playhead and the pointer's hairline) is drawn every
  // frame, as the waveform's is: nothing else called drawOverlay, so the
  // playhead never showed and the graph stood still while the track played.
  let overlayRaf = null;
  const overlayLoop = () => {
    // Only while it is on screen (another tab hides it).
    if (container.offsetParent !== null) drawOverlay(store.cursor || null);
    overlayRaf = requestAnimationFrame(overlayLoop);
  };
  overlayRaf = requestAnimationFrame(overlayLoop);

  function destroy() {
    cancelAnimationFrame(overlayRaf);
    overlayRaf = null;
    bus.removeEventListener('an:live',          onLive);
    bus.removeEventListener('an:live',          updatePlayhead);
    bus.removeEventListener('an:track',         onTrack);
    bus.removeEventListener('an:chain:change',  onChainChange);
    bus.removeEventListener('an:track:change',  onTrackChange);
    bus.removeEventListener('an:layer:toggle',  onLayerToggle);
    bus.removeEventListener('an:cursor:move',   onCursor);

    interactionEl.removeEventListener('mousemove', onMouseMove);
    interactionEl.removeEventListener('mouseleave', onMouseLeave);
    interactionEl.removeEventListener('click', onClick);

    container.innerHTML = '';
  }

  // ------------------------------------------------------------------
  // Initial layout
  // ------------------------------------------------------------------
  {
    const scale = window.devicePixelRatio || 1;
    const w = container.clientWidth || 800;
    const h = container.clientHeight || 140;
    mainCanvas.width   = Math.round(w * scale);
    mainCanvas.height  = Math.round(h * scale);
    overlayCanvas.width  = Math.round(w * scale);
    overlayCanvas.height = Math.round(h * scale);
    layout = computeLayout(scale, w, h);
    drawMain();
  }

  return { setData, setLayer, onResize, drawOverlay, destroy };
}
