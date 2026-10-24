/**
 * stereo-view.js — Vectorscope, correlation and width of the output.
 *
 *  - Vectorscope (goniometer): the live output's last ~50 ms of L/R pairs,
 *    turned 45° (mid up, side across), with a short afterglow and a slow
 *    automatic gain (shown as ×N) so a quiet passage still fills it. Polled
 *    from the `vec` route (AAVS) while the panel is visible.
 *  - Correlation meter: the last 0.3 s of the output, −1 … +1, and its width
 *    as side over mid in dB (AAVS; the live frame's STEREO_CORR otherwise).
 *  - History: both over the last minute.
 *  - Whole track: correlation of S, B and O (AAN2 STEREO_CORR) and the width
 *    that correlation means for balanced channels, side/mid = (1 − r)/(1 + r).
 *    A live stream has none: the song playing and the session instead, S and
 *    O (the stream's totals, stream_totals.rs).
 *
 * LAYER_REGISTRY exported for layers.js.
 * Implements FRONTEND-CONTRACT §4 view lifecycle.
 */

// ── Layer registry ────────────────────────────────────────────────────────────

export const LAYER_REGISTRY = [
  { id: 'vectorscope', label: 'Vectorscope', colorToken: '--an-o',   defaultOn: true,  shortcut: 'v', provenanceAware: false, axisRole: null },
  { id: 'corr_meter',  label: 'Correlation', colorToken: '--an-s',   defaultOn: true,  shortcut: 'c', provenanceAware: false, axisRole: null },
];

// ── Constants ─────────────────────────────────────────────────────────────────

import { STREAM_TEXTS } from './protocol.js';

const METRIC_STEREO_CORR = 23; // PROTOCOL.md §3

// A stream's: the panel's tip (the song and the session below, not the whole track).
const STREAM_TIP = 'The output now: the vectorscope (mid up, side across; ×N = its automatic gain), the correlation and width of the last 0.3 s and their last minute; below, this song and the session for S and O.';
const BASE_URL = 'https://aura.localhost/player/an';

const C_BG       = 'rgba(11,25,44,0.92)';
const C_AXIS     = '#7dd3fc';
const C_DIM      = 'rgba(125,211,252,0.45)';
const C_GRID     = 'rgba(255,255,255,0.08)';
const C_DOT      = '#38bdf8';
const C_S        = '#4ade80';
const C_O        = '#38bdf8';
const C_B        = '#94a3b8';
const C_CORR_POS = '#4ade80'; // correlation > 0.5
const C_CORR_NEU = '#38bdf8'; // 0 … 0.5
const C_CORR_AMB = '#fbbf24'; // −0.5 … 0
const C_CORR_NEG = '#ef4444'; // < −0.5 (anti-phase)

const HISTORY_S  = 60;
const POLL_MS    = 33;
const FONT       = 'ui-monospace, Consolas, monospace';

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

const widthDb = (r) => (Number.isFinite(r) ? 10 * Math.log10(Math.max(1e-6, 1 - r) / Math.max(1e-6, 1 + r)) : NaN);
const fmt = (v, d = 2) => (Number.isFinite(v) ? v.toFixed(d).replace('-', '−') : '—');

function corrColor(c) {
  if (!Number.isFinite(c)) return C_AXIS;
  if (c >= 0.5)  return C_CORR_POS;
  if (c >= 0.0)  return C_CORR_NEU;
  if (c >= -0.5) return C_CORR_AMB;
  return C_CORR_NEG;
}

// ── Factory ───────────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget }} ctx
 */
export function create(container, ctx) {
  const { store, bus } = ctx;

  container.style.cssText = 'position:relative;overflow:hidden;width:100%;height:100%;';
  const mainCanvas = document.createElement('canvas');
  mainCanvas.style.cssText = 'display:block;width:100%;height:100%;';
  container.appendChild(mainCanvas);
  container.dataset.tip = 'The output now: the vectorscope (mid up, side across; ×N = its automatic gain), the correlation and width of the last 0.3 s and their last minute; below, the whole track for S, B and O.';

  // The scope's afterglow lives in a canvas of its own.
  const scope = document.createElement('canvas');

  const layers = {};
  for (const def of LAYER_REGISTRY) layers[def.id] = def.defaultOn;

  let mainW = 0, mainH = 0;
  let destroyed = false;
  let pollTimer = null;
  let inFlight = false;

  // Live
  let corr = NaN, sm = NaN;                 // correlation, side/mid dB
  let pts = null;                           // Float32Array L,R,L,R…
  let gain = 1;                             // the scope's automatic gain
  const history = [];                       // { t, corr, sm }
  let lastVecAt = 0;
  // Whole track
  let whole = { s: NaN, b: NaN, o: NaN };

  function resizeAll() {
    const r = setupCanvas(mainCanvas, container);
    mainW = r.w; mainH = r.h;
  }

  // ── Layout ────────────────────────────────────────────────────────────────

  function layout() {
    const w = mainW, h = mainH;
    const both = layers.vectorscope && layers.corr_meter;
    if (!both) {
      const size = Math.max(0, Math.min(w, h) - 16);
      return layers.vectorscope
        ? { scope: { x: (w - size) / 2, y: 8, s: size }, side: null }
        : { scope: null, side: { x: 12, y: 8, w: w - 24, h: h - 16 } };
    }
    // Side by side while the column keeps 280 px (the scope gives way first);
    // stacked in a narrow panel.
    const sideW = Math.max(280, Math.round(w * 0.38));
    if (w >= sideW + 200) {
      const size = Math.max(0, Math.min(h - 16, w - sideW - 36));
      return { scope: { x: 8, y: 8 + (h - 16 - size) / 2, s: size }, side: { x: size + 28, y: 8, w: w - size - 40, h: h - 16 } };
    }
    const size = Math.max(0, Math.min(w - 16, h * 0.5));
    return { scope: { x: (w - size) / 2, y: 8, s: size }, side: { x: 12, y: size + 20, w: w - 24, h: h - size - 28 } };
  }

  // ── Vectorscope ───────────────────────────────────────────────────────────

  function stepScope(size) {
    if (size <= 8) return;
    const dpr = window.devicePixelRatio || 1;
    if (scope.width !== Math.round(size * dpr)) {
      scope.width = scope.height = Math.round(size * dpr);
    }
    const c = scope.getContext('2d');
    c.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Afterglow: the old points fade over a few frames.
    c.globalCompositeOperation = 'destination-out';
    c.fillStyle = 'rgba(0,0,0,0.42)';
    c.fillRect(0, 0, size, size);
    c.globalCompositeOperation = 'source-over';
    if (!pts || pts.length < 4) return;
    // Automatic gain: the loudest point at ~85 % of the radius, rising at
    // once, falling slowly; ×1 … ×32.
    let peak = 0;
    for (let i = 0; i < pts.length; i += 2) {
      const m = Math.abs(pts[i] + pts[i + 1]), s = Math.abs(pts[i] - pts[i + 1]);
      peak = Math.max(peak, m, s);
    }
    peak /= Math.SQRT2;
    const want = peak > 1e-5 ? Math.min(32, Math.max(1, 0.85 / peak)) : gain;
    gain = want < gain ? want : gain + (want - gain) * 0.02;
    const r = size / 2 - 2, cx = size / 2, cy = size / 2;
    const k = r * gain / Math.SQRT2;
    c.fillStyle = C_DOT;
    c.globalAlpha = 0.55;
    for (let i = 0; i < pts.length; i += 2) {
      const l = pts[i], rr = pts[i + 1];
      const x = cx + (l - rr) * k;       // side across (L to the left)
      const y = cy - (l + rr) * k;       // mid up
      c.fillRect(x - 0.7, y - 0.7, 1.4, 1.4);
    }
    c.globalAlpha = 1;
  }

  function drawScope(g, box) {
    const { x, y, s } = box;
    if (s <= 8) return;
    const r = s / 2 - 2, cx = x + s / 2, cy = y + s / 2;
    g.fillStyle = 'rgba(8,16,30,0.9)';
    g.beginPath(); g.arc(cx, cy, r, 0, Math.PI * 2); g.fill();
    g.strokeStyle = C_GRID;
    g.lineWidth = 1;
    g.beginPath(); g.arc(cx, cy, r, 0, Math.PI * 2); g.stroke();
    g.beginPath(); g.arc(cx, cy, r / 2, 0, Math.PI * 2); g.stroke();
    // M (vertical), S (horizontal), L and R (the diagonals)
    g.beginPath();
    g.moveTo(cx, cy - r); g.lineTo(cx, cy + r);
    g.moveTo(cx - r, cy); g.lineTo(cx + r, cy);
    const d = r / Math.SQRT2;
    g.moveTo(cx - d, cy - d); g.lineTo(cx + d, cy + d);
    g.moveTo(cx + d, cy - d); g.lineTo(cx - d, cy + d);
    g.stroke();
    g.drawImage(scope, x, y, s, s);
    g.fillStyle = C_DIM;
    g.font = `9px ${FONT}`;
    g.textAlign = 'center';
    g.fillText('M', cx, cy - r + 11);
    g.fillText('L', cx - d + 8, cy - d + 12);
    g.fillText('R', cx + d - 8, cy - d + 12);
    g.textAlign = 'left';
    g.fillText('−S', cx - r + 3, cy - 3);
    g.textAlign = 'right';
    g.fillText('+S', cx + r - 3, cy - 3);
    g.fillText('×' + (gain < 9.95 ? gain.toFixed(1) : gain.toFixed(0)), x + s - 2, y + s - 3);
    g.textAlign = 'start';
    if (!pts) {
      g.fillStyle = C_DIM;
      g.textAlign = 'center';
      g.fillText('Plays nothing yet', cx, cy + r / 2 + 4);
      g.textAlign = 'start';
    }
  }

  // ── Correlation, width, history, whole track ─────────────────────────────

  function drawSide(g, box) {
    const { x, y, w, h } = box;
    if (w < 60 || h < 40) return;
    let yy = y;
    // Meter
    const mh = 22;
    g.fillStyle = 'rgba(30,41,59,0.8)';
    g.beginPath(); g.roundRect(x, yy, w, mh, 4); g.fill();
    const cx = x + w / 2;
    if (Number.isFinite(corr)) {
      const fw = Math.abs(corr) * (w / 2);
      g.fillStyle = corrColor(corr);
      g.beginPath(); g.roundRect(corr >= 0 ? cx : cx - fw, yy + 3, fw, mh - 6, 3); g.fill();
    }
    g.strokeStyle = 'rgba(255,255,255,0.3)';
    g.beginPath(); g.moveTo(cx, yy + 2); g.lineTo(cx, yy + mh - 2); g.stroke();
    g.fillStyle = '#fff';
    g.font = `bold 11px ${FONT}`;
    g.textAlign = 'center';
    g.fillText(Number.isFinite(corr) ? fmt(corr, 3) : 'no signal', cx, yy + mh / 2 + 4);
    g.font = `9px ${FONT}`;
    g.fillStyle = C_AXIS;
    for (const v of [-1, -0.5, 0, 0.5, 1]) g.fillText(v === 0 ? '0' : fmt(v, 1), x + ((v + 1) / 2) * w, yy + mh + 11);
    g.textAlign = 'left';
    g.fillStyle = C_DIM;
    g.fillText('anti-phase', x + 4, yy + 10);
    g.textAlign = 'right';
    g.fillText('in phase', x + w - 4, yy + 10);
    yy += mh + 18;
    // Width now
    g.textAlign = 'left';
    g.fillStyle = C_AXIS;
    g.font = `10px ${FONT}`;
    g.fillText(`Correlation ${fmt(corr, 2)}   Width ${Number.isFinite(sm) ? fmt(sm, 1) + ' dB' : '—'}`, x, yy);
    g.fillStyle = C_DIM;
    g.font = `9px ${FONT}`;
    g.fillText('width = side over mid, the last 0.3 s', x, yy + 12);
    yy += 22;
    // History: correlation (line, −1 … +1) and width (dim), last minute — over
    // the room the numbers below need (a stream's song and session take more).
    const hh = Math.max(40, h - (yy - y) - (store.get?.('_stream') ? 108 : 64));
    g.fillStyle = 'rgba(8,16,30,0.9)';
    g.fillRect(x, yy, w, hh);
    g.strokeStyle = C_GRID;
    for (const v of [-1, -0.5, 0, 0.5, 1]) {
      const ly = yy + (1 - (v + 1) / 2) * hh;
      g.beginPath(); g.moveTo(x, ly); g.lineTo(x + w, ly); g.stroke();
    }
    const now = performance.now() / 1000;
    const tx = (t) => x + w - (now - t) / HISTORY_S * w;
    if (history.length > 1) {
      g.strokeStyle = C_DIM;
      g.lineWidth = 1;
      g.beginPath();
      let first = true;
      for (const p of history) {
        if (!Number.isFinite(p.sm)) { first = true; continue; }
        const v = Math.max(-1, Math.min(1, p.sm / 30));   // ±30 dB over the height
        const px = tx(p.t), py = yy + (1 - (v + 1) / 2) * hh;
        if (first) { g.moveTo(px, py); first = false; } else g.lineTo(px, py);
      }
      g.stroke();
      g.strokeStyle = C_O;
      g.lineWidth = 1.4;
      g.beginPath();
      first = true;
      for (const p of history) {
        if (!Number.isFinite(p.corr)) { first = true; continue; }
        const px = tx(p.t), py = yy + (1 - (p.corr + 1) / 2) * hh;
        if (first) { g.moveTo(px, py); first = false; } else g.lineTo(px, py);
      }
      g.stroke();
    }
    g.fillStyle = C_DIM;
    g.font = `9px ${FONT}`;
    g.textAlign = 'left';
    g.fillText('+1', x + 3, yy + 10);
    g.fillText('−1', x + 3, yy + hh - 3);
    g.textAlign = 'right';
    g.fillText('corr (blue) · width ±30 dB · 1 min', x + w - 4, yy + 10);
    yy += hh + 16;
    // Whole track — a stream's song and session.
    g.textAlign = 'left';
    g.font = `10px ${FONT}`;
    const row = (label, color, r) => {
      g.fillStyle = color;
      g.fillText(label, x, yy);
      g.fillStyle = C_AXIS;
      g.fillText(`correlation ${fmt(r, 3)}   width ≈ ${Number.isFinite(r) ? fmt(widthDb(r), 1) + ' dB' : '—'}`, x + 22, yy);
      yy += 14;
    };
    const block = (title, s, o, b) => {
      g.fillStyle = C_AXIS;
      g.fillText(title, x, yy);
      yy += 15;
      row('S', C_S, s);
      if (Number.isFinite(b)) row('B', C_B, b);
      row('O', C_O, o);
    };
    if (store.get?.('_stream')) {
      const t = store.get('_streamTotals');
      block(STREAM_TEXTS.song, t?.song?.s?.corr, t?.song?.o?.corr);
      yy += 4;
      block(STREAM_TEXTS.session, t?.session?.s?.corr, t?.session?.o?.corr);
    } else {
      block('Whole track', whole.s, whole.o, whole.b);
    }
    g.textAlign = 'start';
  }

  function redraw() {
    const g = mainCanvas.getContext('2d');
    if (!g) return;
    const w = mainW, h = mainH;
    if (w <= 0 || h <= 0) return;
    g.clearRect(0, 0, w, h);
    g.fillStyle = C_BG;
    g.fillRect(0, 0, w, h);
    const L = layout();
    if (L.scope) drawScope(g, L.scope);
    if (L.side) drawSide(g, L.side);
  }

  // ── The vec poll (only while the panel shows) ─────────────────────────────

  const visible = () => mainW > 0 && mainH > 0 && container.offsetParent !== null && document.visibilityState === 'visible';

  async function poll() {
    if (destroyed) return;
    pollTimer = setTimeout(poll, POLL_MS);
    if (!visible() || inFlight) return;
    const sid = store.get ? store.get('sid') : null;
    if (sid == null) return;
    inFlight = true;
    try {
      const res = await fetch(`${BASE_URL}/vec?sid=${sid}`, { cache: 'no-store' });
      const buf = res.ok ? await res.arrayBuffer() : null;
      if (buf && buf.byteLength >= 24) {
        const dv = new DataView(buf);
        const n = dv.getUint32(12, true);
        corr = dv.getFloat32(4, true);
        sm = dv.getFloat32(8, true);
        pts = new Float32Array(buf.slice(24, 24 + n * 8));
        const t = performance.now() / 1000;
        if (t - lastVecAt >= 0.1) {
          history.push({ t, corr, sm });
          lastVecAt = t;
          while (history.length && history[0].t < t - HISTORY_S) history.shift();
        }
        const L = layout();
        if (L.scope) stepScope(L.scope.s);
        redraw();
      }
    } catch { /* the next poll tries again */ }
    inFlight = false;
  }

  // ── Bus ───────────────────────────────────────────────────────────────────

  const onLive = (e) => {
    const m = e.detail?.frame?.live_metrics;
    if (!m) return;
    // Without the vec route (an older backend) the live frame's value.
    if (performance.now() / 1000 - lastVecAt > 1) {
      const c = m[METRIC_STEREO_CORR];
      corr = Number.isFinite(c) ? c : NaN;
      redraw();
    }
  };
  const onTrack = (e) => {
    const f = e.detail?.frame;
    if (!f || f.stub) return;
    whole = { s: f.s_metrics?.[METRIC_STEREO_CORR], b: f.b_metrics?.[METRIC_STEREO_CORR], o: f.o_metrics?.[METRIC_STEREO_CORR] };
    redraw();
  };
  const onTrackChange = () => {
    whole = { s: NaN, b: NaN, o: NaN };
    history.length = 0;
    redraw();
  };

  bus.addEventListener('an:live', onLive);
  bus.addEventListener('an:track', onTrack);
  bus.addEventListener('an:track:change', onTrackChange);

  // A stream: its song's and session's numbers come with its totals.
  const fileTip = container.dataset.tip;
  const onStream = (on) => { container.dataset.tip = on ? STREAM_TIP : fileTip; redraw(); };
  store.on?.('_stream', onStream);
  store.on?.('_streamTotals', () => { if (store.get?.('_stream')) redraw(); });
  if (store.get?.('_stream')) container.dataset.tip = STREAM_TIP;

  resizeAll();
  redraw();
  poll();

  return {
    setData() { /* driven by the bus and the vec poll */ },
    setLayer(key, on) { layers[key] = on; redraw(); },
    onResize() { resizeAll(); redraw(); },
    drawOverlay() { /* nothing over the panel */ },
    destroy() {
      destroyed = true;
      clearTimeout(pollTimer);
      bus.removeEventListener('an:live', onLive);
      bus.removeEventListener('an:track', onTrack);
      bus.removeEventListener('an:track:change', onTrackChange);
      container.innerHTML = '';
    },
  };
}
