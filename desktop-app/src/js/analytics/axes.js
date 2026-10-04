/**
 * axes.js — Canvas drawing helpers for log-frequency, dB, and time axes.
 *
 * All draw* functions are pure side-effects on a CanvasRenderingContext2D.
 * They do not save/restore state themselves — callers should wrap in
 * ctx.save() / ctx.restore() when needed.
 *
 * Coordinate conversion helpers are self-contained here so views can work
 * without importing from utils.js during parallel development.
 */

// ---------------------------------------------------------------------------
// Coordinate conversion helpers
// ---------------------------------------------------------------------------

/**
 * Map a normalized position x in [0,1] to Hz on a log scale between f0 and f1.
 * @param {number} x  Normalized position [0, 1]
 * @param {number} f0 Low frequency bound (Hz)
 * @param {number} f1 High frequency bound (Hz)
 * @returns {number} Frequency in Hz
 */
export function logFreqFromNorm(x, f0, f1) {
  return f0 * Math.pow(f1 / f0, x);
}

/**
 * Map a frequency in Hz to a normalized position in [0,1] on a log scale.
 * @param {number} hz Frequency in Hz
 * @param {number} f0 Low frequency bound (Hz)
 * @param {number} f1 High frequency bound (Hz)
 * @returns {number} Normalized position [0, 1]
 */
export function normFromLogFreq(hz, f0, f1) {
  return Math.log(hz / f0) / Math.log(f1 / f0);
}

/**
 * Map dBFS value to normalized vertical position (0 = top/0dB, 1 = bottom/floor).
 * @param {number} db     dBFS value
 * @param {number} dbTop  Top of axis (usually 0 or slightly above)
 * @param {number} dbFloor Bottom of axis (e.g. -300)
 * @returns {number} Normalized position [0, 1]
 */
export function normFromDb(db, dbTop, dbFloor) {
  return (dbTop - db) / (dbTop - dbFloor);
}

/**
 * Map a time position in seconds to normalized horizontal position [0,1].
 * @param {number} t  Time in seconds
 * @param {number} t0 Start time (seconds)
 * @param {number} t1 End time (seconds)
 * @returns {number} Normalized position [0, 1]
 */
export function normFromTime(t, t0, t1) {
  return (t - t0) / (t1 - t0);
}

// ---------------------------------------------------------------------------
// Tick generation
// ---------------------------------------------------------------------------

/** Preferred decade subdivisions for log-freq axis labels. */
const LOG_FREQ_PREFERRED = [
  20, 30, 50, 100, 200, 300, 500,
  1000, 2000, 3000, 5000,
  10000, 20000, 30000, 50000,
  100000, 200000,
];

/**
 * Generate tick marks for a log-frequency axis.
 * @param {number} f0       Low frequency bound (Hz)
 * @param {number} f1       High frequency bound (Hz)
 * @param {number} maxTicks Maximum number of ticks to return (default 12)
 * @returns {Array<{hz: number, label: string, major: boolean}>}
 */
export function logFreqTicks(f0, f1, maxTicks = 12) {
  const ticks = [];
  for (const hz of LOG_FREQ_PREFERRED) {
    if (hz < f0 || hz > f1) continue;
    let label;
    if (hz >= 1000) {
      const k = hz / 1000;
      label = Number.isInteger(k) ? k + 'k' : k.toFixed(1).replace(/\.0$/, '') + 'k';
    } else {
      label = String(hz);
    }
    const major = [20, 100, 1000, 10000, 100000].includes(hz);
    ticks.push({ hz, label, major });
  }
  // Trim to maxTicks, keeping major ticks preferentially
  if (ticks.length > maxTicks) {
    const majors = ticks.filter(t => t.major);
    const minors = ticks.filter(t => !t.major);
    const keep = new Set(majors.map(t => t.hz));
    let i = 0;
    while (keep.size < maxTicks && i < minors.length) {
      keep.add(minors[i++].hz);
    }
    return ticks.filter(t => keep.has(t.hz));
  }
  return ticks;
}

/**
 * Generate evenly-spaced dB tick values for a linear dB axis.
 * @param {number} dbFloor  Bottom of axis (most negative, e.g. -300)
 * @param {number} dbTop    Top of axis (e.g. 0)
 * @param {number} maxTicks Maximum ticks (default 10)
 * @returns {Array<{db: number, label: string, major: boolean}>}
 */
export function dbAxisTicks(dbFloor, dbTop, maxTicks = 10) {
  const range = dbTop - dbFloor;
  // Choose a step size that gives roughly maxTicks ticks
  const rawStep = range / maxTicks;
  const steps = [1, 2, 3, 5, 6, 10, 12, 15, 20, 24, 25, 30, 40, 50, 60, 100, 120, 150, 200, 300];
  let step = steps[steps.length - 1];
  for (const s of steps) {
    if (s >= rawStep) { step = s; break; }
  }
  const ticks = [];
  const start = Math.ceil(dbFloor / step) * step;
  for (let db = start; db <= dbTop; db += step) {
    const major = db % (step * 2) === 0 || db === 0;
    ticks.push({ db, label: db === 0 ? '0' : String(db), major });
  }
  return ticks;
}

/**
 * Generate tick marks for a time axis.
 * @param {number} t0       Start time in seconds
 * @param {number} t1       End time in seconds
 * @param {number} maxTicks Maximum ticks
 * @returns {Array<{t: number, label: string, major: boolean}>}
 */
export function timeAxisTicks(t0, t1, maxTicks = 10) {
  const duration = t1 - t0;
  const rawStep = duration / maxTicks;
  // Time steps in seconds
  const steps = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];
  let step = steps[steps.length - 1];
  for (const s of steps) {
    if (s >= rawStep) { step = s; break; }
  }
  const ticks = [];
  // None before 0 (a stream's view starts there while less than it is kept).
  const start = Math.ceil(Math.max(0, t0) / step) * step;
  for (let t = start; t <= t1; t += step) {
    const m = Math.floor(t / 60);
    const s = Math.round(t % 60);
    const label = m + ':' + String(s).padStart(2, '0');
    ticks.push({ t, label, major: t % (step * 2) === 0 });
  }
  return ticks;
}

// ---------------------------------------------------------------------------
// Canvas DPR helper
// ---------------------------------------------------------------------------

/**
 * Set canvas physical pixels to CSS size × devicePixelRatio.
 * Call after every resize; returns the scale factor.
 * @param {HTMLCanvasElement} canvas
 * @returns {{ scale: number, cssW: number, cssH: number }}
 */
export function setupCanvas(canvas) {
  const scale = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth;
  const cssH = canvas.clientHeight;
  canvas.width = Math.round(cssW * scale);
  canvas.height = Math.round(cssH * scale);
  return { scale, cssW, cssH };
}

// ---------------------------------------------------------------------------
// Axis drawing
// ---------------------------------------------------------------------------

const AXIS_FONT = '11px ui-monospace, "Cascadia Mono", "Segoe UI Mono", Consolas, monospace';
const LABEL_COLOR = '#7dd3fc';
const GRID_COLOR = 'rgba(255,255,255,0.07)';
const TICK_COLOR = 'rgba(255,255,255,0.25)';

/**
 * Layout descriptor: canvas pixel coordinates for the plot area (scaled).
 * @typedef {{ x: number, y: number, w: number, h: number, scale: number }} Layout
 */

/**
 * Draw a log-frequency x-axis below the plot area.
 * @param {CanvasRenderingContext2D} ctx
 * @param {Layout} layout  Plot-area position in canvas pixels
 * @param {number} f0  Left frequency (Hz)
 * @param {number} f1  Right frequency (Hz)
 * @param {{ labelColor?: string, tickColor?: string, font?: string }} [opts]
 */
export function drawLogFreqAxis(ctx, layout, f0, f1, opts = {}) {
  const { x, y, w, h, scale } = layout;
  const bottom = y + h;
  const lc = opts.labelColor || LABEL_COLOR;
  const tc = opts.tickColor || TICK_COLOR;
  const font = opts.font || AXIS_FONT;

  ctx.save();
  ctx.font = font;
  ctx.fillStyle = lc;
  ctx.strokeStyle = tc;
  ctx.lineWidth = 1 * scale;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'top';

  const ticks = logFreqTicks(f0, f1, Math.max(6, Math.floor(w / (50 * scale))));
  for (const { hz, label, major } of ticks) {
    const nx = normFromLogFreq(hz, f0, f1);
    if (nx < 0 || nx > 1) continue;
    const px = x + nx * w;

    ctx.globalAlpha = major ? 1 : 0.6;
    ctx.beginPath();
    ctx.moveTo(px, bottom);
    ctx.lineTo(px, bottom + 4 * scale);
    ctx.stroke();

    ctx.fillText(label, px, bottom + 6 * scale);
  }

  ctx.globalAlpha = 1;
  ctx.restore();
}

/**
 * Draw a linear dB y-axis on the left side of the plot area.
 * @param {CanvasRenderingContext2D} ctx
 * @param {Layout} layout
 * @param {number} dbFloor  Bottom of axis (most negative, e.g. -300)
 * @param {number} dbTop    Top of axis (e.g. 0 or 6)
 * @param {{ rightAxis?: boolean, labelSuffix?: string } & object} [opts]
 */
export function drawDbAxis(ctx, layout, dbFloor, dbTop, opts = {}) {
  const { x, y, w, h, scale } = layout;
  const lc = opts.labelColor || LABEL_COLOR;
  const tc = opts.tickColor || TICK_COLOR;
  const font = opts.font || AXIS_FONT;
  const rightAxis = opts.rightAxis || false;
  const suffix = opts.labelSuffix || '';

  ctx.save();
  ctx.font = font;
  ctx.fillStyle = lc;
  ctx.strokeStyle = tc;
  ctx.lineWidth = 1 * scale;
  ctx.textBaseline = 'middle';
  ctx.textAlign = rightAxis ? 'left' : 'right';

  const ticks = dbAxisTicks(dbFloor, dbTop, Math.max(4, Math.floor(h / (35 * scale))));
  for (const { db, label, major } of ticks) {
    const ny = normFromDb(db, dbTop, dbFloor);
    if (ny < 0 || ny > 1) continue;
    const py = y + ny * h;
    if (opts.minY != null && py < opts.minY) continue;   // under a view's own labels
    const xLeft = rightAxis ? x + w : x;
    const tickDir = rightAxis ? 1 : -1;

    ctx.globalAlpha = major ? 1 : 0.6;
    ctx.beginPath();
    ctx.moveTo(xLeft, py);
    ctx.lineTo(xLeft + tickDir * 4 * scale, py);
    ctx.stroke();

    const labelX = rightAxis ? xLeft + 6 * scale : xLeft - 6 * scale;
    ctx.fillText(label + suffix, labelX, py);
  }

  ctx.globalAlpha = 1;
  ctx.restore();
}

/**
 * Draw a time x-axis below the plot area.
 * @param {CanvasRenderingContext2D} ctx
 * @param {Layout} layout
 * @param {number} t0  Start time (seconds)
 * @param {number} t1  End time (seconds)
 * @param {object} [opts]
 */
export function drawTimeAxis(ctx, layout, t0, t1, opts = {}) {
  const { x, y, w, h, scale } = layout;
  const bottom = y + h;
  const lc = opts.labelColor || LABEL_COLOR;
  const tc = opts.tickColor || TICK_COLOR;
  const font = opts.font || AXIS_FONT;

  ctx.save();
  ctx.font = font;
  ctx.fillStyle = lc;
  ctx.strokeStyle = tc;
  ctx.lineWidth = 1 * scale;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'top';

  const ticks = timeAxisTicks(t0, t1, Math.max(4, Math.floor(w / (60 * scale))));
  for (const { t, label, major } of ticks) {
    const nx = normFromTime(t, t0, t1);
    if (nx < 0 || nx > 1) continue;
    const px = x + nx * w;

    ctx.globalAlpha = major ? 1 : 0.6;
    ctx.beginPath();
    ctx.moveTo(px, bottom);
    ctx.lineTo(px, bottom + 4 * scale);
    ctx.stroke();

    ctx.fillText(label, px, bottom + 6 * scale);
  }

  ctx.globalAlpha = 1;
  ctx.restore();
}

/**
 * Draw a log-freq / dB grid over the plot area.
 * @param {CanvasRenderingContext2D} ctx
 * @param {Layout} layout
 * @param {number} f0
 * @param {number} f1
 * @param {number} dbFloor
 * @param {number} dbTop
 * @param {object} [opts]
 */
export function drawSpectrumGrid(ctx, layout, f0, f1, dbFloor, dbTop, opts = {}) {
  const { x, y, w, h, scale } = layout;
  const gc = opts.gridColor || GRID_COLOR;

  ctx.save();
  ctx.strokeStyle = gc;
  ctx.lineWidth = 1 * scale;

  // Vertical (freq) grid lines
  const fTicks = logFreqTicks(f0, f1, Math.max(6, Math.floor(w / (50 * scale))));
  for (const { hz, major } of fTicks) {
    const nx = normFromLogFreq(hz, f0, f1);
    if (nx < 0 || nx > 1) continue;
    const px = x + nx * w;
    ctx.globalAlpha = major ? 0.14 : 0.07;
    ctx.beginPath();
    ctx.moveTo(px, y);
    ctx.lineTo(px, y + h);
    ctx.stroke();
  }

  // Horizontal (dB) grid lines
  const dTicks = dbAxisTicks(dbFloor, dbTop, Math.max(4, Math.floor(h / (35 * scale))));
  for (const { db, major } of dTicks) {
    const ny = normFromDb(db, dbTop, dbFloor);
    if (ny < 0 || ny > 1) continue;
    const py = y + ny * h;
    ctx.globalAlpha = major ? 0.14 : 0.07;
    ctx.beginPath();
    ctx.moveTo(x, py);
    ctx.lineTo(x + w, py);
    ctx.stroke();
  }

  ctx.globalAlpha = 1;
  ctx.restore();
}

/**
 * Draw a time / dB grid for the loudness history canvas.
 * @param {CanvasRenderingContext2D} ctx
 * @param {Layout} layout
 * @param {number} t0
 * @param {number} t1
 * @param {number} dbFloor  e.g. -40
 * @param {number} dbTop    e.g. 0
 * @param {object} [opts]
 */
export function drawLoudnessGrid(ctx, layout, t0, t1, dbFloor, dbTop, opts = {}) {
  const { x, y, w, h, scale } = layout;
  const gc = opts.gridColor || GRID_COLOR;

  ctx.save();
  ctx.strokeStyle = gc;
  ctx.lineWidth = 1 * scale;

  // Vertical time lines
  const tTicks = timeAxisTicks(t0, t1, Math.max(4, Math.floor(w / (60 * scale))));
  for (const { t, major } of tTicks) {
    const nx = normFromTime(t, t0, t1);
    if (nx < 0 || nx > 1) continue;
    const px = x + nx * w;
    ctx.globalAlpha = major ? 0.14 : 0.07;
    ctx.beginPath();
    ctx.moveTo(px, y);
    ctx.lineTo(px, y + h);
    ctx.stroke();
  }

  // Horizontal dB lines
  const dTicks = dbAxisTicks(dbFloor, dbTop, Math.max(4, Math.floor(h / (30 * scale))));
  for (const { db, major } of dTicks) {
    const ny = normFromDb(db, dbTop, dbFloor);
    if (ny < 0 || ny > 1) continue;
    const py = y + ny * h;
    ctx.globalAlpha = major ? 0.14 : 0.07;
    ctx.beginPath();
    ctx.moveTo(x, py);
    ctx.lineTo(x + w, py);
    ctx.stroke();
  }

  ctx.globalAlpha = 1;
  ctx.restore();
}
