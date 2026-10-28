/**
 * snapshot.js — Per-window spectrum snapshot state (up to 8 captures).
 *
 * A snapshot freezes the O curve the spectrum shows (the whole-track one, or
 * the live one at the playhead) with S and |H| beside it, in a colour of the
 * 8-slot categorical palette defined in analytics.css. The spectrum view
 * draws them under its live layers and lists them (show, hide, remove).
 *
 * Keyboard shortcuts (handled globally within the analyzer window document;
 * by the key's place, so any keyboard layout):
 *   S          Capture a snapshot of the O curve shown
 *   Ctrl+Z     Remove the most recently added snapshot
 *
 * Bus events:
 *   an:snapshot:capture { handled, snap, reason } — asks the spectrum view for
 *                        its curves (it sets `handled` and calls capture());
 *                        without a spectrum view the whole-track curves are taken
 *   an:snapshot:add    { snapshot }  — new snapshot added
 *   an:snapshot:remove { id }        — snapshot removed
 *
 * Usage:
 *   import { create } from './snapshot.js';
 *   const snapshots = create(ctx);    // ctx = { store, bus }
 *   snapshots.request();              // the button: { snap } or { reason }
 *   snapshots.removeById(id);
 *   snapshots.destroy();
 */

/** Categorical palette matching --snap-1 … --snap-8 from analytics.css §11.3. */
export const SNAP_PALETTE = [
  '#f472b6', // snap-1 pink
  '#fb923c', // snap-2 orange
  '#facc15', // snap-3 yellow
  '#34d399', // snap-4 emerald
  '#22d3ee', // snap-5 cyan
  '#818cf8', // snap-6 indigo
  '#e879f9', // snap-7 fuchsia
  '#f87171', // snap-8 red
];

export const MAX_SNAPSHOTS = 8;

/**
 * @param {{ store: object, bus: EventTarget }} ctx
 */
export function create(ctx) {
  const { store, bus } = ctx;

  /** @type {Array<{id: number, label: string, what: string, color: string, on: boolean, capturedAt: number, data: object}>} */
  let snapshots = [];
  let nextId = 1;

  // ------------------------------------------------------------------
  // Internal helpers
  // ------------------------------------------------------------------

  function dispatch(eventName, detail) {
    bus.dispatchEvent(new CustomEvent(eventName, { detail }));
  }

  function commit() {
    store.set('snapshots', snapshots.slice()); // immutable replacement
  }

  /** The first palette colour no kept snapshot has (a removed one's comes back). */
  function freeColor() {
    const used = new Set(snapshots.map(s => s.color));
    return SNAP_PALETTE.find(c => !used.has(c)) || SNAP_PALETTE[snapshots.length % SNAP_PALETTE.length];
  }

  // ------------------------------------------------------------------
  // Snapshot capture
  // ------------------------------------------------------------------

  /**
   * Freeze `layers` ({ o_psd, s_psd?, h_mag? }: the curves shown). `what`
   * says where they came from ("whole track", "live · instant · 1:23.4").
   * @returns {{ snap?: object, reason?: 'full'|'empty' }}
   */
  function capture(layers, what) {
    if (snapshots.length >= MAX_SNAPSHOTS) return { reason: 'full' };
    // A curve with a level somewhere: before O is known (a file window, the
    // first seconds of the live one) its layer is all NaN, and "Captured"
    // with nothing drawn was a lie.
    if (!layers || !hasCurve(layers.o_psd)) return { reason: 'empty' };
    const chain = store.chain || {};
    const tokens = chain.tokens || '';
    const id = nextId++;
    const snap = {
      id,
      label: String(id),
      what: what || '',
      color: freeColor(),
      on: true,
      capturedAt: Date.now(),
      data: {
        // Copies, so the snapshot does not change as new data arrives
        o_psd:    copyLayerData(layers.o_psd),
        h_mag:    copyLayerData(layers.h_mag),
        s_psd:    copyLayerData(layers.s_psd),
        chainRev: chain.rev,
        chainTokens: tokens,
      },
    };
    snapshots = [...snapshots, snap];
    commit();
    dispatch('an:snapshot:add', { snapshot: snap });
    return { snap };
  }

  /** The whole-track curves (a window without a spectrum view). */
  function captureFromStore() {
    return capture(store.layers, 'whole track');
  }

  /**
   * The button and the S key: the curves the spectrum view shows, or the
   * whole-track ones without it.
   * @returns {{ snap?: object, reason?: 'full'|'empty' }}
   */
  function request() {
    const detail = { handled: false, snap: null, reason: null };
    dispatch('an:snapshot:capture', detail);
    if (!detail.handled) return captureFromStore();
    return detail.snap ? { snap: detail.snap } : { reason: detail.reason || 'empty' };
  }

  /**
   * Toggle a snapshot's visibility (its chip in the spectrum's list).
   * @param {number} id
   */
  function toggleById(id) {
    snapshots = snapshots.map(s =>
      s.id === id ? { ...s, on: !s.on } : s
    );
    commit();
  }

  /**
   * Remove a snapshot by ID.
   * @param {number} id
   */
  function removeById(id) {
    const before = snapshots.length;
    snapshots = snapshots.filter(s => s.id !== id);
    if (snapshots.length < before) {
      commit();
      dispatch('an:snapshot:remove', { id });
    }
  }

  /**
   * Remove the most recently added snapshot (Ctrl+Z).
   */
  function removeLast() {
    if (snapshots.length === 0) return;
    removeById(snapshots[snapshots.length - 1].id);
  }

  /**
   * Return current snapshot list (read-only copy).
   * @returns {Array<object>}
   */
  function getAll() {
    return snapshots;
  }

  /**
   * True when max 8 snapshots are held.
   * @returns {boolean}
   */
  function atCapacity() {
    return snapshots.length >= MAX_SNAPSHOTS;
  }

  // ------------------------------------------------------------------
  // Keyboard handler (document-level for the analyzer window)
  // ------------------------------------------------------------------

  // A key's result shows as the button's would: window.js listens.
  function onKeyDown(e) {
    // Skip if focus is in an input/textarea
    const tag = (e.target && e.target.tagName) ? e.target.tagName.toUpperCase() : '';
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;

    if (e.code === 'KeyS' && !e.ctrlKey && !e.metaKey && !e.altKey && !e.repeat) {
      e.preventDefault();
      const r = request();
      dispatch('an:snapshot:result', r);
      return;
    }

    if (e.code === 'KeyZ' && (e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey) {
      e.preventDefault();
      removeLast();
    }
  }

  document.addEventListener('keydown', onKeyDown);

  // ------------------------------------------------------------------
  // Listen to an:track:change — clear all snapshots when track changes
  // ------------------------------------------------------------------
  function onTrackChange() {
    const ids = snapshots.map(s => s.id);
    snapshots = [];
    commit();
    for (const id of ids) {
      dispatch('an:snapshot:remove', { id });
    }
    nextId = 1;
  }

  bus.addEventListener('an:track:change', onTrackChange);

  // ------------------------------------------------------------------
  // Cleanup
  // ------------------------------------------------------------------

  function destroy() {
    document.removeEventListener('keydown', onKeyDown);
    bus.removeEventListener('an:track:change', onTrackChange);
    snapshots = [];
  }

  return {
    capture,
    captureFromStore,
    request,
    toggleById,
    removeById,
    removeLast,
    getAll,
    atCapacity,
    destroy,
  };
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/** A layer with at least one finite level. */
export function hasCurve(layer) {
  return !!(layer && layer.data && Array.prototype.some.call(layer.data, Number.isFinite));
}

/**
 * Deep-copy a layer data object so the snapshot is frozen at capture time.
 * @param {{ data: Float32Array, minF: number, maxF: number, chainRev: number } | null} layer
 * @returns {object | null}
 */
function copyLayerData(layer) {
  if (!layer) return null;
  return {
    data: layer.data instanceof Float32Array
      ? layer.data.slice()
      : (layer.data ? Float32Array.from(layer.data) : null),
    minF: layer.minF,
    maxF: layer.maxF,
    chainRev: layer.chainRev,
  };
}
