// The waveform's minimap on a live stream: all the stream keeps, the view lit;
// a click elsewhere, a drag of the window and a double click move the view as
// they do a file's.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore, emptyTiles } from './fake-dom.mjs';

const W = 800;
const dom = installFakeDom({ w: W, h: 200 });
const { create } = await import('../src/js/analytics/waveform-view.js');

const SR = 44100, BLOCK_S = 256 / SR;
/** A live frame's STRM tail with the blocks k0..k0+n of S and O. */
function strm(k0, n) {
  const env = () => {
    const blocks = new Uint32Array(n), v = new Int16Array(n * 6);
    for (let i = 0; i < n; i++) { blocks[i] = k0 + i; v.set([-8000, 8000, 4000, -8000, 8000, 4000], i * 6); }
    return { blocks, v };
  };
  return { srcRate: SR, clockS: (k0 + n) * BLOCK_S, sEnv: env(), oEnv: env() };
}

const near = (a, b, eps = 0.5) => Math.abs(a - b) < eps;

test("a stream's minimap shows all it keeps and moves the view", () => {
  const store = fakeStore({});
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus, tileCache: emptyTiles });
  const [mini] = container.children;
  const ctx = mini.children[0].getContext();
  store.set('_stream', true);
  assert.match(mini.dataset.tip, /All of the stream kept/);
  const blocks = Math.round(120 / BLOCK_S);             // two minutes
  for (let k = 0; k < blocks; k += 1000) bus.dispatchEvent(new CustomEvent('an:live', { detail: { frame: { strm: strm(k, Math.min(1000, blocks - k)) } } }));
  const lit = () => {
    ctx.rects.length = 0;
    dom.flushFrames();
    const r = ctx.rects.filter(x => x.style === 'rgba(56,189,248,0.10)').at(-1);
    return r && [r.x, r.x + r.w];
  };
  const hi = blocks * BLOCK_S;
  let [x0, x1] = lit();
  assert.ok(near(x0, (hi - 30) / hi * W) && near(x1, W), `the last 30 s lit at the edge: ${x0}..${x1}`);
  assert.ok(ctx.rects.some(x => x.style === '#4ade8080'), 'S drawn');
  // A click at a quarter: the view goes there, its span kept.
  mini.fire('pointerdown', { clientX: W / 4 });
  mini.fire('pointerup', { clientX: W / 4 });
  [x0, x1] = lit();
  assert.ok(near((x0 + x1) / 2, W / 4) && near(x1 - x0, 30 / hi * W), `${x0}..${x1}`);
  // The window dragged on by a tenth of the width: 12 s later.
  mini.fire('pointerdown', { clientX: W / 4 });
  mini.fire('pointermove', { clientX: W / 4 + W / 10 });
  mini.fire('pointerup', { clientX: W / 4 + W / 10 });
  [x0, x1] = lit();
  assert.ok(near((x0 + x1) / 2, W / 4 + W / 10), `${x0}..${x1}`);
  // Its right edge dragged out: wider.
  mini.fire('pointerdown', { clientX: x1 });
  mini.fire('pointermove', { clientX: x1 + 100 });
  mini.fire('pointerup', { clientX: x1 + 100 });
  const [y0, y1] = lit();
  assert.ok(near(y0, x0) && near(y1, x1 + 100), `${y0}..${y1}`);
  // Double click: all of it, at the edge.
  mini.fire('dblclick');
  [x0, x1] = lit();
  assert.ok(near(x0, 0) && near(x1, W), `${x0}..${x1}`);
  // The detail view goes with it (its follow mark is the file's; the stream's view is drawn).
  store.set('_stream', false);
  assert.match(mini.dataset.tip, /The whole track/);
  view.destroy();
});
