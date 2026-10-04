// The loudness on a live stream: the last five minutes at the live edge at
// first, zoomed (Ctrl+wheel) and moved (drag, Shift+wheel) over the 30 the
// page keeps, back to the edge on a double click; the pointer's line and the
// LUFS-S of S and O there (a file's too). A file's zoom as it was.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore } from './fake-dom.mjs';

installFakeDom({ w: 852, h: 236 });
const { create, lufsAt } = await import('../src/js/analytics/loudness-view.js');

// The plot: 852 less the axes (44 left, 8 right) = 800 px from x = 44.
const X0 = 44, PW = 800;
const near = (a, b, eps = 1e-3) => Math.abs(a - b) < eps;

/** Points every 0.1 s over [a, b): S at −20, O at −14 LUFS-S. */
function points(a, b, v) {
  const out = [];
  for (let t = a; t < b - 1e-9; t += 0.1) out.push({ time_s: Math.round(t * 10) / 10, value: v });
  return out;
}

function setup(state) {
  const store = fakeStore(state);
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus, cursor: null });
  const [, overlay, ui] = container.children;
  const time = () => container.dataset.time.split('..').map(Number);
  return { store, bus, container, view, overlay, ui, time };
}

test('a value at a time: the 10 Hz grid, or the nearest point', () => {
  const grid = [NaN, -30, -20];                // index i: the window ending at (i + 4)·0.1 s
  assert.equal(lufsAt(grid, 0.5), -30);
  assert.ok(Number.isNaN(lufsAt(grid, 0.4)));
  assert.ok(Number.isNaN(lufsAt(grid, 9)));
  const pts = [{ time_s: 10, value: -1 }, { time_s: 10.1, value: -2 }, { time_s: 11, value: -3 }];
  assert.equal(lufsAt(pts, 10.04), -1);
  assert.equal(lufsAt(pts, 10.07), -2);
  assert.ok(Number.isNaN(lufsAt(pts, 10.5)), 'nothing within 0.2 s');
  assert.ok(Number.isNaN(lufsAt([], 1)));
});

test("a stream's loudness: the edge, a zoom, a drag, back on a double click, all of the 30 minutes", () => {
  const series = { lufs_s_o: points(600, 2400, -14), lufs_m_o: [], lufs_s_s_live: points(600, 2400, -20), lufs_m_s_live: [] };
  const h = setup({ _stream: true, _posS: 2400, series });
  h.store.set('_stream', true);
  assert.match(h.ui.getAttribute('data-tip'), /The last 30 minutes of the stream are kept/);
  h.view.setData();
  let [t0, t1] = h.time();
  assert.ok(near(t0, 2100) && near(t1, 2400), `the last five minutes: ${t0}..${t1}`);
  h.ui.fire('wheel', { clientX: X0 + PW / 2, deltaY: -1, ctrlKey: true });
  [t0, t1] = h.time();
  assert.ok(near(t1 - t0, 240) && near((t0 + t1) / 2, 2250), `zoomed at the pointer: ${t0}..${t1}`);
  // The stream goes on: the view placed by hand stands.
  series.lufs_s_o.push(...points(2400, 2410, -14));
  h.store.set('_posS', 2410);
  h.view.setData();
  assert.ok(near(h.time()[0], t0));
  // A drag to the right: back by half the view.
  h.ui.fire('pointerdown', { clientX: 300 });
  h.ui.fire('pointermove', { clientX: 300 + PW / 2 });
  h.ui.fire('pointerup', { clientX: 300 + PW / 2 });
  assert.ok(near(h.time()[0], t0 - 120), `${h.time()}`);
  // Shift+wheel: a tenth of the view on.
  h.ui.fire('wheel', { clientX: X0 + 10, deltaY: 1, shiftKey: true });
  assert.ok(near(h.time()[0], t0 - 120 + 24));
  h.ui.fire('dblclick');
  [t0, t1] = h.time();
  assert.ok(near(t0, 2110) && near(t1, 2410), `back at the edge: ${t0}..${t1}`);
  for (let i = 0; i < 30; i++) h.ui.fire('wheel', { clientX: X0 + PW / 2, deltaY: 1, ctrlKey: true });
  [t0, t1] = h.time();
  assert.ok(near(t0, 600) && near(t1, 2410), `all that is kept: ${t0}..${t1}`);
  // The wheel alone is the dB range, as for a file.
  h.ui.fire('wheel', { clientX: X0 + PW / 2, deltaY: -1 });
  assert.ok(near(h.time()[0], 600));
  h.view.destroy();
});

test('the pointer reads S and O there; another view gives a line only', () => {
  const series = { lufs_s_o: points(0, 120, -14), lufs_m_o: [], lufs_s_s_live: points(0, 120, -20), lufs_m_s_live: [] };
  const h = setup({ _stream: true, _posS: 120, series });
  h.view.setData();
  h.ui.fire('mousemove', { clientX: X0 + 0.7 * PW });  // the view: −180..120, so 30 s here
  const c = h.overlay.getContext();
  c.texts.length = 0;
  h.view.drawOverlay(null);
  const line = c.texts.map(t => t.t).join(' | ');
  assert.ok(line.includes('0:30.0 · S −20.0 LUFS · O −14.0 LUFS'), line);
  const seen = [];
  h.bus.addEventListener('an:cursor:move', (e) => seen.push(e.detail.timeS));
  h.ui.fire('mouseleave');
  assert.equal(seen.at(-1), null, 'the other views let it go');
  c.texts.length = 0;
  h.bus.dispatchEvent(new CustomEvent('an:cursor:move', { detail: { timeS: 30, sourceViewId: 'spectrogram' } }));
  h.view.drawOverlay(null);
  assert.equal(c.texts.length, 0, 'no readout for another view');
  h.view.destroy();
});

test("a file's loudness zooms as before and reads its series", () => {
  const sGrid = new Array(2000).fill(-23);         // 200 s on the 10 Hz grid
  const h = setup({ durationS: 200, series: { lufs_s_s: sGrid, lufs_s_o: [], lufs_m_o: [] } });
  h.view.setData();
  assert.deepEqual(h.time(), [0, 200]);
  h.ui.fire('wheel', { clientX: X0 + PW / 2, deltaY: -1, ctrlKey: true });
  const [t0, t1] = h.time();
  assert.ok(near(t1 - t0, 166) && t0 > 0, `${t0}..${t1}`);
  h.ui.fire('mousemove', { clientX: X0 + PW / 2 });
  const c = h.overlay.getContext();
  c.texts.length = 0;
  h.view.drawOverlay(null);
  assert.ok(c.texts.some(t => /S −23\.0 LUFS · O — LUFS/.test(t.t)), c.texts.map(t => t.t).join(' | '));
  h.ui.fire('dblclick');
  assert.deepEqual(h.time(), [0, 200]);
  assert.equal(h.ui.getAttribute('data-tip'), null, 'no stream tip');
  h.view.destroy();
});
