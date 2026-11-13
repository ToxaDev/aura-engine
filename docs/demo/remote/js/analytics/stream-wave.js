/**
 * stream-wave.js — A stream's waveform: the blocks of S and O the live frames
 * bring (STRM v2: per block of STRM_ENV_BLOCK of the stream's frames, the
 * least, the greatest and the RMS of each channel), kept for the last
 * HISTORY_S seconds, and drawn as two lanes over a view that follows the
 * live edge or is zoomed and moved by hand. The ring and its bins are pure;
 * the view draws on the waveform's canvas (waveform-view.js hands it over
 * while a stream plays).
 */
import { STRM_ENV_BLOCK } from './protocol.js';
import { formatTime, formatTimeInt } from './format.js';
import { createStreamView, zoomAt } from './stream-view.js';

export { zoomAt };

/** How long the page keeps a stream's waveform (s). */
export const HISTORY_S = 1800;
const SPAN0_S = 30;       // the view's span at first
const SPAN_MIN_S = 0.25;

/** A ring of `cap` blocks: their numbers and six i16 values each. */
export function createRing(cap) {
  // Fixed for the ring's life (no memory taken per frame); a slot no block
  // has filled holds 0xFFFFFFFF.
  const nums = new Uint32Array(cap).fill(0xFFFFFFFF);
  const vals = new Int16Array(cap * 6);
  let scratch = new Float32Array(0);
  let newest = -1;
  return {
    get newest() { return newest; },
    get oldest() { return newest < 0 ? -1 : Math.max(0, newest - cap + 1); },
    /** Blocks `blocks` (numbers) with their values `v` (6 per block). */
    add(blocks, v) {
      for (let i = 0; i < blocks.length; i++) {
        const k = blocks[i], j = k % cap;
        nums[j] = k;
        vals.set(v.subarray(i * 6, i * 6 + 6), j * 6);
        if (k > newest) newest = k;
      }
    },
    /** The block `k`'s six values, or null when not held. */
    at(k) {
      const j = k % cap;
      return k >= 0 && nums[j] === k ? vals.subarray(j * 6, j * 6 + 6) : null;
    },
    /**
     * `n` bins over blocks [k0, k1): per bin the least and the greatest of
     * the left channel and its greatest RMS, of full scale (NaN: nothing held).
     */
    bins(k0, k1, n) {
      // One buffer, grown only with the view's width (the draws do not each take one).
      if (scratch.length < n * 3) scratch = new Float32Array(n * 3);
      const out = scratch.subarray(0, n * 3).fill(NaN);
      const per = (k1 - k0) / n;
      for (let i = 0; i < n; i++) {
        const a = Math.floor(k0 + i * per), b = Math.max(a + 1, Math.floor(k0 + (i + 1) * per));
        let mn = Infinity, mx = -Infinity, rms = 0, any = false;
        for (let k = a; k < b; k++) {
          const j = k % cap;
          if (k < 0 || nums[j] !== k) continue;
          any = true;
          const o = j * 6;
          if (vals[o] < mn) mn = vals[o];
          if (vals[o + 1] > mx) mx = vals[o + 1];
          if (vals[o + 2] > rms) rms = vals[o + 2];
        }
        if (any) { out[i * 3] = mn / 32767; out[i * 3 + 1] = mx / 32767; out[i * 3 + 2] = rms / 32767; }
      }
      return out;
    },
  };
}

/** dBFS of a block's peak (the left channel), −∞ as NaN. */
export function peakDb(v) {
  const p = Math.max(Math.abs(v[0]), Math.abs(v[1])) / 32767;
  return p > 0 ? 20 * Math.log10(p) : NaN;
}

/**
 * Where the pointer's value goes: beside the line at `x`, within the width
 * `w`, its baseline at 26 px — or under `avoid` ({x, y, w, h}: the Follow
 * button in the top-right corner) when it would cover it there.
 */
export function labelAt(x, tw, w, avoid) {
  const lx = Math.min(w - tw - 4, Math.max(4, x + 6));
  let ly = 26;
  if (avoid && lx < avoid.x + avoid.w + 4 && lx + tw > avoid.x - 4 && ly - 11 < avoid.y + avoid.h + 2) {
    ly = avoid.y + avoid.h + 14;
  }
  return { x: lx, y: ly };
}

/** The stream's waveform view: feed it the live frames, let it draw. */
export function createStreamWave() {
  let rate = 0, rings = null, clock = 0, ptr = null;
  let overview = null;    // the minimap's bins: { n, at, lo, hi, s, o }
  const blockS = () => STRM_ENV_BLOCK / Math.max(1, rate);
  /** What the rings keep: from their oldest block to the live edge. */
  const domain = () => {
    const hi = clock, lo = rings ? Math.max(0, rings.s.oldest * blockS()) : hi;
    return { lo, hi };
  };
  // Following the live edge, or zoomed and moved by hand (stream-view.js).
  const sv = createStreamView({ span0: SPAN0_S, spanMin: SPAN_MIN_S, domain });
  return {
    /** A live frame's STRM tail (v2). */
    feed(strm) {
      if (!strm?.sEnv) return;
      const newest = Math.max(strm.sEnv.blocks.at(-1) ?? -1, strm.oEnv.blocks.at(-1) ?? -1);
      // Another stream (its count starts again) or another rate: afresh.
      if (!rings || strm.srcRate !== rate || (newest >= 0 && newest < rings.s.newest - 4)) {
        rate = strm.srcRate;
        const cap = Math.ceil(HISTORY_S * rate / STRM_ENV_BLOCK);
        rings = { s: createRing(cap), o: createRing(cap) };
        overview = null;
        sv.reset();
      }
      rings.s.add(strm.sEnv.blocks, strm.sEnv.v);
      rings.o.add(strm.oEnv.blocks, strm.oEnv.v);
      clock = strm.clockS;
    },
    reset() { rings = null; rate = 0; clock = 0; ptr = null; overview = null; sv.reset(); },
    get following() { return sv.following; },
    wheel: sv.wheel,
    /** Moved by hand by `dt` seconds from `from`: it stops following (at the edge it follows). */
    pan: sv.pan,
    /** [t0, t1] by hand (the minimap). */
    setView: sv.setView,
    /** All of the history, following again. */
    all: sv.all,
    /** Back to the live edge, the span as it is (the Follow button). */
    followEdge: sv.followEdge,
    view: sv.view,
    domain,
    /** The rings (S and O) and a block's length, s — the minimap's. */
    get rings() { return rings; },
    get blockS() { return blockS(); },
    point(t) { ptr = t; },
    /**
     * The minimap: all the rings keep, S and O over each other (their bins
     * taken again four times a second at most), and the view on it lit with
     * a grip on each edge, as a file's minimap.
     */
    drawOverview(c2, w, h) {
      if (!rings || !(w > 0)) return;
      const { lo, hi } = domain(), bs = blockS();
      const n = Math.max(1, Math.floor(w)), now = Date.now();
      if (!overview || overview.n !== n || now - overview.at > 250) {
        overview = { n, at: now, lo, hi,
          s: rings.s.bins(lo / bs, hi / bs, n).slice(), o: rings.o.bins(lo / bs, hi / bs, n).slice() };
      }
      const mid = h / 2, a = h / 2 - 1;
      for (const [b, color] of [[overview.s, '#4ade80'], [overview.o, '#38bdf8']]) {
        c2.fillStyle = color + '80';
        for (let i = 0; i < n; i++) {
          const mn = b[i * 3], mx = b[i * 3 + 1];
          if (Number.isFinite(mn)) c2.fillRect(i, mid - mx * a, 1, Math.max(1, (mx - mn) * a));
        }
      }
      const v = sv.view(), span = Math.max(1e-9, overview.hi - overview.lo);
      const x0 = Math.max(0, (v.t0 - overview.lo) / span * w);
      const x1 = Math.max(x0 + 2, Math.min(w, (v.t1 - overview.lo) / span * w));
      c2.fillStyle = 'rgba(56,189,248,0.10)';
      c2.fillRect(x0, 0, x1 - x0, h);
      c2.strokeStyle = 'rgba(56,189,248,0.75)';
      c2.lineWidth = 1;
      c2.strokeRect(x0 + 0.5, 0.5, x1 - x0 - 1, h - 1);
      c2.fillStyle = 'rgba(186,230,253,0.9)';
      for (const x of [x0, x1]) c2.fillRect(x - 1.5, h / 2 - 5, 3, 10);
    },
    /** The two lanes (S above, O below), a time scale, the value under the
     *  pointer — clear of `avoid`, the Follow button (labelAt). */
    draw(c2, w, h, avoid) {
      if (!rings || !(w > 0)) return;
      const v = sv.view(), bs = blockS();
      const k0 = v.t0 / bs, k1 = v.t1 / bs;
      const n = Math.max(1, Math.floor(w));
      const lane = (ring, y0, lh, color, label) => {
        const b = ring.bins(k0, k1, n);
        const mid = y0 + lh / 2, a = lh / 2 - 2;
        c2.fillStyle = 'rgba(148,163,184,0.25)';
        c2.fillRect(0, mid, w, 1);
        for (let i = 0; i < n; i++) {
          const mn = b[i * 3], mx = b[i * 3 + 1], r = b[i * 3 + 2];
          if (!Number.isFinite(mn)) continue;
          c2.globalAlpha = 0.45;
          c2.fillStyle = color;
          c2.fillRect(i, mid - mx * a, 1, Math.max(1, (mx - mn) * a));
          c2.globalAlpha = 0.9;
          c2.fillRect(i, mid - r * a, 1, Math.max(1, 2 * r * a));
        }
        c2.globalAlpha = 1;
        c2.fillStyle = '#94a3b8';
        c2.font = '10px ui-monospace, Consolas, monospace';
        c2.fillText(label, 6, y0 + 12);
      };
      const lh = (h - 16) / 2;
      lane(rings.s, 0, lh, '#4ade80', 'S');
      lane(rings.o, lh, lh, '#38bdf8', 'O');
      // The time scale: the stream's clock (none before it began: while less
      // than the span is kept, the view starts before 0).
      c2.fillStyle = '#64748b';
      c2.font = '10px ui-monospace, Consolas, monospace';
      for (let i = 0; i <= 4; i++) {
        const t = v.t0 + (v.t1 - v.t0) * i / 4;
        if (t < 0) continue;
        const x = Math.min(w - 40, Math.max(0, w * i / 4 - (i ? 20 : 0)));
        c2.fillText((v.t1 - v.t0 < 10 ? formatTime : formatTimeInt)(t), x, h - 3);
      }
      // The pointer: a line and the blocks' peaks there.
      if (ptr != null && ptr >= Math.max(0, v.t0) && ptr <= v.t1) {
        const x = (ptr - v.t0) / (v.t1 - v.t0) * w;
        c2.fillStyle = 'rgba(226,232,240,0.6)';
        c2.fillRect(Math.round(x), 0, 1, h - 16);
        const k = Math.floor(ptr / bs);
        const s = rings.s.at(k), o = rings.o.at(k);
        const db = (x) => (x && Number.isFinite(peakDb(x)) ? peakDb(x).toFixed(1) : '—');
        const text = `${formatTime(ptr)} · peak S ${db(s)} dBFS · O ${db(o)} dBFS`;
        c2.fillStyle = '#e2e8f0';
        c2.font = '11px ui-monospace, Consolas, monospace';
        const at = labelAt(x, c2.measureText(text).width, w, avoid);
        c2.fillText(text, at.x, at.y);
      }
    },
  };
}
