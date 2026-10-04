/**
 * stream-view.js — The time view of a panel on a live stream (the radio).
 *
 * It follows the live edge — the last `span` seconds up to the newest moment
 * the panel keeps, the scale steady while less than that is kept — or stands
 * where the wheel, a drag or the minimap put it, within what the panel keeps
 * (`domain()`: { lo, hi }, seconds on the stream's clock), and comes back to
 * the edge (Follow) or shows all of it. Pure: the waveform's, the
 * spectrogram's and the loudness's on a stream.
 *
 * Also the level a spectrogram strip holds at a time and a frequency.
 */

/** The bands' lowest edge (Hz), as the spectrogram's. */
const F0 = 20;

/**
 * The view after a wheel step at time `t` (zoom in for deltaY < 0): `t`
 * stays where it is on screen; the span between `spanMin` and all of
 * [lo, hi] (or the span shown, when that is longer); the view inside
 * [lo, hi] — before `lo` only while the span is longer than what is kept.
 */
export function zoomAt(view, t, deltaY, lo, hi, spanMin = 0.25) {
  const k = deltaY > 0 ? 1.25 : 0.8;
  const was = view.t1 - view.t0;
  const span = Math.min(Math.max(hi - lo, was), Math.max(spanMin, was * k));
  const f = (t - view.t0) / Math.max(1e-9, was);
  const t0 = clampStart(t - f * span, span, lo, hi);
  return { t0, t1: t0 + span };
}

/** A view's start for `span` within [lo, hi] (its end at `hi` at the latest). */
function clampStart(t0, span, lo, hi) {
  return Math.max(Math.min(lo, hi - span), Math.min(hi - span, t0));
}

/**
 * @param {{ span0: number, spanMin?: number, domain: () => { lo: number, hi: number } }} o
 *   span0: the span at first (s); domain: what the panel keeps now.
 */
export function createStreamView({ span0, spanMin = 0.25, domain }) {
  // `whole`: all that is kept, as it grows (all()), until the view is moved by hand.
  let follow = true, span = span0, view = null, whole = false;
  const current = () => {
    const { lo, hi } = domain();
    if (whole) return { t0: Math.min(lo, hi - spanMin), t1: hi };
    if (follow || !view) return { t0: hi - span, t1: hi };
    return view;
  };
  /** A view placed by hand: at the live edge it follows again, with its span. */
  const place = (v) => {
    const { hi } = domain();
    whole = false;
    span = v.t1 - v.t0;
    follow = v.t1 >= hi - 1e-6;
    view = follow ? null : v;
  };
  return {
    get following() { return follow; },
    get span() { return span; },
    /** { t0, t1 } shown now. */
    view: current,
    /** A wheel step at time `t`. */
    wheel(t, deltaY) {
      const { lo, hi } = domain();
      place(zoomAt(current(), t, deltaY, lo, hi, spanMin));
    },
    /** Moved by `dt` seconds from the view `from` (a drag's start). */
    pan(from, dt) {
      const { lo, hi } = domain();
      const s = from.t1 - from.t0;
      const t0 = clampStart(from.t0 + dt, s, lo, hi);
      place({ t0, t1: t0 + s });
    },
    /** [t0, t1] asked for (the minimap), kept within what is kept. */
    setView(t0, t1) {
      const { lo, hi } = domain();
      const s = Math.min(Math.max(hi - lo, span0), Math.max(spanMin, t1 - t0));
      const a = clampStart(t0, s, lo, hi);
      place({ t0: a, t1: a + s });
    },
    /** All that is kept, following the live edge — all of it as it grows. (It
     *  kept the span it had then: seconds later the oldest went out of view.) */
    all() {
      const { lo, hi } = domain();
      span = Math.max(spanMin, hi - lo);
      follow = true; view = null; whole = true;
    },
    /** Back to the live edge, the span as it is (the Follow button). */
    followEdge() {
      const v = current();
      if (view || whole) span = v.t1 - v.t0;
      follow = true; view = null; whole = false;
    },
    /** Afresh: another stream, its clock from 0. */
    reset() { follow = true; span = span0; view = null; whole = false; },
  };
}

/**
 * The level (dB) a spectrogram strip holds at time `t` and frequency `hz`:
 * the strip `st` ({ bytes, bins, count, first }) has columns `colS` seconds
 * long (column k from k·colS, the first one numbered `first`) of `bins` log
 * bands from 20 Hz to `top`, bytes 0..255 over −floor..0 dB. Null where it
 * holds nothing.
 */
export function stripLevel(st, colS, t, hz, top, floor) {
  if (!st || !st.count || !(colS > 0) || !(hz >= F0) || !(hz <= top)) return null;
  const c = Math.floor(t / colS) - st.first;
  if (c < 0 || c >= st.count) return null;
  const b = Math.min(st.bins - 1, Math.floor(st.bins * Math.log(hz / F0) / Math.log(top / F0)));
  return -floor + (st.bytes[c * st.bins + b] / 255) * floor;
}
