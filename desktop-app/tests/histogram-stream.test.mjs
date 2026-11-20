// The loudness histogram on a live stream: S and O of the song playing or of
// the session, from the stream's totals (stream_totals.rs counts them in the
// files' bins); a switch picks which; a file's histogram as it was.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore, emptyTiles } from './fake-dom.mjs';

installFakeDom({ w: 600, h: 200 });
const { create, sharesOf } = await import('../src/js/analytics/histogram-view.js');

/** 256 counts with `n` values in the bin of `lufs` (−80…0 LUFS, 0.3125 LU a bin). */
const counts = (lufs, n = 100) => {
  const h = new Array(256).fill(0);
  h[Math.floor((lufs + 80) / 80 * 256)] = n;
  return h;
};

function setup() {
  const store = fakeStore({});
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus, tileCache: emptyTiles });
  const loudEl = container.children[0];
  const [canvas, , which] = loudEl.children;
  const ctx = canvas.getContext();
  const texts = () => ctx.texts.map(t => t.t);
  const fresh = () => { ctx.texts.length = 0; };
  return { store, bus, view, loudEl, which, texts, fresh };
}

test('counts become shares; none is no histogram', () => {
  const s = sharesOf([0, 1, 3, 0]);
  assert.deepEqual([...s], [0, 0.25, 0.75, 0]);
  assert.equal(sharesOf(new Array(256).fill(0)), null);
  assert.equal(sharesOf(undefined), null);
});

test("a stream's histogram: the song's, the session's at a click, measuring before the first values", () => {
  const h = setup();
  h.store.set('_stream', true);
  assert.equal(h.which.hidden, false, 'the switch shows on a stream');
  assert.match(h.loudEl.dataset.tip, /this song or of the session/);
  assert.ok(h.texts().includes('Measuring…'), 'no totals yet');
  h.fresh();
  h.store.set('_streamTotals', {
    song: { s: { hist: counts(-20) }, o: { hist: counts(-14) } },
    session: { s: { hist: counts(-30) }, o: { hist: counts(-24) } },
  });
  assert.ok(h.texts().some(t => t === 'P10 −20.0'), h.texts().join(' | '));
  const [song, session] = h.which.children;
  assert.ok(song.classList.contains('an-spec-btn--on'));
  h.fresh();
  session.fire('click');
  assert.ok(h.texts().some(t => t === 'P10 −30.0'), h.texts().join(' | '));
  assert.ok(session.classList.contains('an-spec-btn--on') && !song.classList.contains('an-spec-btn--on'));
  // A whole-track frame of the stream (no bins) leaves them be.
  h.fresh();
  h.bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame: { hist_s: new Float32Array(0), hist_b: new Float32Array(0) } } }));
  h.store.set('_streamTotals', { song: { s: { hist: counts(-20) } }, session: { s: { hist: counts(-32.5) } } });
  assert.ok(h.texts().some(t => t === 'P10 −32.5'), h.texts().join(' | '));
  h.view.destroy();
});

test("a file's histogram as before, no switch", () => {
  const h = setup();
  const hist = new Float32Array(256);
  hist[Math.floor((-17.5 + 80) / 80 * 256)] = 1;
  h.bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame: { hist_s: hist, hist_b: null } } }));
  assert.equal(h.which.hidden, true);
  assert.ok(h.texts().some(t => t === 'P10 −17.5'), h.texts().join(' | '));
  assert.match(h.loudEl.dataset.tip, /How much of the track/);
  h.view.destroy();
});
