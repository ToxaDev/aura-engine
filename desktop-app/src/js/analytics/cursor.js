/**
 * cursor.js — Per-window shared cursor state, overlay canvas lifecycle, and 60 fps rAF loop.
 *
 * Responsibilities:
 *   - Track mousemove/mouseleave on registered canvas elements.
 *   - Convert canvas pixel position to { freqHz, timeS } via per-canvas converters.
 *   - Emit `an:cursor:move` on the event bus when cursor changes.
 *   - Update store.cursor and maintain per-layer values.
 *   - Run the single 60 fps requestAnimationFrame loop that calls
 *     view.drawOverlay(cursor) on every registered view.
 *
 * Usage (called by window.js once per analyzer window):
 *
 *   import { create } from './cursor.js';
 *   const cursor = create(ctx);           // ctx = { store, bus }
 *   cursor.register(spectrumView, spectrumCanvas, {
 *     toFreqHz: (xNorm) => ...,
 *     toTimeS:  (xNorm) => null,
 *   });
 *   // on window close:
 *   cursor.destroy();
 */

export function create(ctx) {
  const { store, bus } = ctx;

  let rafId = null;

  /** @type {Map<Element, { view: object, converters: object, cleanup: () => void }>} */
  const tracked = new Map();

  /** @type {Set<object>} All registered views (for drawOverlay loop). */
  const views = new Set();

  /** Current cursor state, mirrored from store.cursor for fast RAF access. */
  let cursorState = { freqHz: null, timeS: null, values: {}, sourceViewId: null };

  // ------------------------------------------------------------------
  // RAF loop — runs at 60 fps while any view is registered
  // ------------------------------------------------------------------

  function tick() {
    for (const view of views) {
      try {
        view.drawOverlay(cursorState);
      } catch (_) {
        // Never let a misbehaving view crash the loop
      }
    }
    rafId = requestAnimationFrame(tick);
  }

  // ------------------------------------------------------------------
  // Cursor update helpers
  // ------------------------------------------------------------------

  function emitCursorMove(freqHz, timeS, sourceViewId) {
    const changed =
      freqHz !== cursorState.freqHz ||
      timeS !== cursorState.timeS;

    if (!changed && sourceViewId === cursorState.sourceViewId) return;

    cursorState = { freqHz, timeS, values: cursorState.values, sourceViewId };
    store.set('cursor', { freqHz, timeS, values: cursorState.values });
    bus.dispatchEvent(
      new CustomEvent('an:cursor:move', {
        detail: { freqHz, timeS, sourceViewId },
      })
    );
  }

  function clearCursor(sourceViewId) {
    if (cursorState.freqHz === null && cursorState.timeS === null) return;
    cursorState = { freqHz: null, timeS: null, values: {}, sourceViewId: null };
    store.set('cursor', { freqHz: null, timeS: null, values: {} });
    bus.dispatchEvent(
      new CustomEvent('an:cursor:move', {
        detail: { freqHz: null, timeS: null, sourceViewId },
      })
    );
  }

  /**
   * Update the cursor `values` map with a new per-layer value for a given view.
   * Called by views when they compute cursor values (on mouse move or data update).
   * @param {string} layerId
   * @param {number | null} value  dBFS or dBr value at cursor position
   */
  function setCursorValue(layerId, value) {
    const updated = Object.assign({}, cursorState.values, { [layerId]: value });
    cursorState = { ...cursorState, values: updated };
    store.set('cursor', { ...store.cursor, values: updated });
  }

  // ------------------------------------------------------------------
  // Public API
  // ------------------------------------------------------------------

  /**
   * Register a canvas view for the 60 fps drawOverlay loop, and attach mouse
   * tracking to `canvas`.
   *
   * @param {object} view      View instance with drawOverlay(cursor) method.
   * @param {HTMLCanvasElement} canvas  The overlay canvas element for this view.
   * @param {{ toFreqHz?: (xNorm: number) => number | null,
   *            toTimeS?:  (xNorm: number) => number | null,
   *            viewId?:   string }} converters
   *   Functions that convert a normalized x in [0,1] to Hz or seconds.
   *   Either or both may be provided; missing converters return null.
   */
  function register(view, canvas, converters = {}) {
    views.add(view);

    const viewId = converters.viewId || String(views.size);
    const toFreqHz = converters.toFreqHz || (() => null);
    const toTimeS = converters.toTimeS || (() => null);

    function onMove(e) {
      const rect = canvas.getBoundingClientRect();
      const xNorm = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
      emitCursorMove(toFreqHz(xNorm), toTimeS(xNorm), viewId);
    }

    function onLeave() {
      clearCursor(viewId);
    }

    canvas.addEventListener('mousemove', onMove, { passive: true });
    canvas.addEventListener('mouseleave', onLeave);

    tracked.set(canvas, {
      view,
      converters: { toFreqHz, toTimeS, viewId },
      cleanup() {
        canvas.removeEventListener('mousemove', onMove);
        canvas.removeEventListener('mouseleave', onLeave);
      },
    });

    // Start RAF loop if this is the first view
    if (rafId === null) {
      rafId = requestAnimationFrame(tick);
    }
  }

  /**
   * Unregister a view and remove mouse tracking from its canvas.
   * @param {HTMLCanvasElement} canvas
   */
  function unregister(canvas) {
    const entry = tracked.get(canvas);
    if (!entry) return;
    entry.cleanup();
    views.delete(entry.view);
    tracked.delete(canvas);

    if (views.size === 0 && rafId !== null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
  }

  /**
   * Force-set cursor values (called by views after computing bin lookups).
   */
  function setValues(valuesMap) {
    cursorState = { ...cursorState, values: { ...cursorState.values, ...valuesMap } };
    store.set('cursor', { ...store.cursor, values: cursorState.values });
  }

  /**
   * Destroy the cursor module: cancel RAF, remove all listeners.
   */
  function destroy() {
    if (rafId !== null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
    for (const [, entry] of tracked) {
      entry.cleanup();
    }
    tracked.clear();
    views.clear();
  }

  // ------------------------------------------------------------------
  // Listen to an:cursor:move from other views (cross-view cursor sync
  // within the same window — spectrum cursor shows on loudness, etc.)
  // ------------------------------------------------------------------
  function onBusCursorMove(e) {
    const { freqHz, timeS, sourceViewId } = e.detail || {};
    // Merge time cursor into state — spectrum doesn't have timeS, loudness doesn't have freqHz
    if (freqHz !== undefined && cursorState.freqHz !== freqHz) {
      cursorState = { ...cursorState, freqHz };
    }
    if (timeS !== undefined && cursorState.timeS !== timeS) {
      cursorState = { ...cursorState, timeS };
    }
  }
  bus.addEventListener('an:cursor:move', onBusCursorMove);

  return {
    register,
    unregister,
    setCursorValue,
    setValues,
    destroy() {
      bus.removeEventListener('an:cursor:move', onBusCursorMove);
      destroy();
    },
  };
}
