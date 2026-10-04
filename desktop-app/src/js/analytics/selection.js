/**
 * selection.js — a stretch of the track and its statistics (RX's "waveform statistics").
 *
 * A view sets the selection with `an:selection` { t0, t1 } (seconds; null clears it).
 * For S, B and O this asks the backend (`/player/an/selstat`, AASL) for the whole-track
 * metrics measured over that stretch and its spectrum, polling while they are being
 * computed, and hands each result on:
 *   - `an:selstats` { sel, src, stats }  (stats: decodeAASL's frame; status 2 = none)
 *   - store.layers.sel_s / sel_b / sel_o { data, minF, maxF } for the spectrum view.
 * The selection itself is kept in store.selection.
 */

import { decodeAASL } from './protocol.js';

const POLL_MS = 150;
const SRCS = ['S', 'B', 'O'];

/** @param {{ store: object, bus: EventTarget }} ctx */
export function createSelection({ store, bus }) {
  let sel = null;       // { t0, t1, s0, s1, id }
  let timer = null;
  let nextId = 1;
  const done = {};      // src → true once its answer (1 or 2) came for this selection

  function clearLayers() {
    for (const s of SRCS) store.setDeep(`layers.sel_${s.toLowerCase()}`, null);
  }

  async function poll() {
    timer = null;
    if (!sel) return;
    const cur = sel;
    const sid = store.get('sid');
    if (sid == null) return;
    let pending = false;
    for (const src of SRCS) {
      if (done[src]) continue;
      let f = null;
      try {
        const r = await fetch(`https://aura.localhost/player/an/selstat?sid=${sid}&src=${src}&s0=${cur.s0}&s1=${cur.s1}`);
        if (r.ok) f = decodeAASL(await r.arrayBuffer());
      } catch { /* asked again below */ }
      if (sel !== cur) return;           // a new selection meanwhile
      if (!f || f.status === 0 || f.s0 !== cur.s0 || f.s1 !== cur.s1) { pending = true; continue; }
      done[src] = true;
      if (f.status === 1) {
        store.setDeep(`layers.sel_${src.toLowerCase()}`, { data: f.pairs, minF: f.f0, maxF: f.f1, welchN: f.welch_n });
      }
      bus.dispatchEvent(new CustomEvent('an:selstats', { detail: { sel: cur, src, stats: f } }));
    }
    if (pending) timer = setTimeout(poll, POLL_MS);
  }

  function onSelection(e) {
    const d = e.detail;
    if (timer) { clearTimeout(timer); timer = null; }
    for (const s of SRCS) delete done[s];
    clearLayers();
    const rate = store.get('_srcRate') || 44100;
    if (!d || !(d.t1 > d.t0)) {
      sel = null;
      store.set('selection', null);
      bus.dispatchEvent(new CustomEvent('an:selstats', { detail: { sel: null } }));
      return;
    }
    sel = { t0: d.t0, t1: d.t1, s0: Math.max(0, Math.round(d.t0 * rate)), s1: Math.round(d.t1 * rate), id: nextId++ };
    store.set('selection', sel);
    bus.dispatchEvent(new CustomEvent('an:selstats', { detail: { sel, src: null } }));
    poll();
  }

  // A new track, or a new B or O pass (a rack change): the stretch is measured again.
  let passSig = '';
  function onTrack(e) {
    const f = e.detail?.frame;
    if (!f || f.stub || !sel) return;
    const sig = (f.spec_info || []).map(i => `${i.gen}:${i.ready >= i.total}`).join('|');
    if (sig === passSig) return;
    passSig = sig;
    for (const s of SRCS) if (done[s]) { delete done[s]; }
    if (!timer) timer = setTimeout(poll, POLL_MS);
  }

  function onTrackChange() {
    bus.dispatchEvent(new CustomEvent('an:selection', { detail: null }));
  }

  bus.addEventListener('an:selection', onSelection);
  bus.addEventListener('an:track', onTrack);
  bus.addEventListener('an:track:change', onTrackChange);

  return {
    destroy() {
      if (timer) clearTimeout(timer);
      bus.removeEventListener('an:selection', onSelection);
      bus.removeEventListener('an:track', onTrack);
      bus.removeEventListener('an:track:change', onTrackChange);
    },
  };
}
