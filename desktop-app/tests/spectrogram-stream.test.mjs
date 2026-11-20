// The spectrogram on a live stream: the strips keep about three minutes of S
// and O; the view follows the live edge, the wheel, a drag and the Follow
// button move it over them; the readout gives S's and O's levels and O − S;
// late colours land where their columns are now. A file's view as it was.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore, emptyTiles } from './fake-dom.mjs';

const dom = installFakeDom({ w: 800, h: 300 });
const { create } = await import('../src/js/analytics/spectrogram-view.js');

const SR = 44100, HOP = 2048, BINS = 512, COL_S = HOP / SR;
const PW = 800 - 20 - 38;                       // the plot: less the colour bar and the ruler

/** A live frame's STRM tail: columns first..first+n of S (level s) and O (level o). */
function strm(first, n, s = 200, o = 200) {
  const col = (v) => Array.from({ length: n }, () => new Uint8Array(BINS).fill(v));
  return {
    srcRate: SR, outRate: SR, clockS: (first + n) * COL_S,
    sCols: { bins: BINS, hop: HOP, first, cols: col(s) }, oCols: { bins: BINS, hop: HOP, first, cols: col(o) },
  };
}

function setup() {
  const store = fakeStore({ _srcRate: SR, _outRate: SR });
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus, tileCache: emptyTiles });
  const [toolbar, plot] = container.children;
  const [main, overlay] = plot.children;
  const follow = toolbar.children.find(b => /^Follow/.test(b.textContent));
  const live = (t) => bus.dispatchEvent(new CustomEvent('an:live', { detail: { frame: { spec_floor_neg: 120, strm: t } } }));
  const time = () => container.dataset.time.split('..').map(Number);
  const draw = () => { dom.answerWorker(); dom.flushFrames(); };
  return { store, bus, container, view, toolbar, plot, main, overlay, follow, live, time, draw };
}

const near = (a, b, eps = 1e-3) => Math.abs(a - b) < eps;

test("a stream's spectrogram keeps its three minutes and follows the live edge", () => {
  const s = setup();
  s.store.set('_stream', true);
  assert.match(s.plot.dataset.tip, /The last 3 minutes of the stream are kept/);
  assert.equal(s.follow.style.display, '', 'Follow shows on a stream');
  for (let k = 0; k < 5000; k += 4) s.live(strm(k, 4));
  s.draw();
  const hi = 5000 * COL_S;
  let [t0, t1] = s.time();
  assert.ok(near(t1, hi) && near(t1 - t0, 20), `the last 20 s at the live edge: ${t0}..${t1}`);
  assert.equal(s.follow.textContent, 'Following');
  // All of it (double click): what the strips keep, 4096 columns.
  s.main.fire('dblclick');
  [t0, t1] = s.time();
  assert.ok(near(t0, (5000 - 4096) * COL_S) && near(t1, hi), `all of it: ${t0}..${t1}`);
  assert.ok(t1 - t0 > 180, 'about three minutes');
  s.view.destroy();
});

test('the wheel zooms at the pointer, a drag looks back, Follow comes back with the zoom', () => {
  const s = setup();
  s.store.set('_stream', true);
  for (let k = 0; k < 4000; k += 4) s.live(strm(k, 4));
  s.draw();
  const hi = 4000 * COL_S;
  s.main.fire('wheel', { clientX: PW / 2, clientY: 100, deltaY: -1 });
  s.draw();
  let [t0, t1] = s.time();
  assert.ok(near(t1 - t0, 16), 'zoomed in');
  assert.ok(near((t0 + t1) / 2, hi - 10), 'about the pointer');
  assert.equal(s.follow.textContent, 'Follow', 'no longer following');
  // A drag to the right: back in time by half the view.
  s.main.fire('pointerdown', { clientX: 400, clientY: 100 });
  s.main.fire('pointermove', { clientX: 400 + PW / 2, clientY: 100 });
  s.main.fire('pointerup', { clientX: 400 + PW / 2, clientY: 100 });
  s.draw();
  const [d0, d1] = s.time();
  assert.ok(near(d0, t0 - 8) && near(d1, t1 - 8), `moved back 8 s: ${d0}..${d1}`);
  // Shift+wheel: a tenth of the view on.
  s.main.fire('wheel', { clientX: 100, clientY: 100, deltaY: 1, shiftKey: true });
  s.draw();
  assert.ok(near(s.time()[0], d0 + 1.6));
  s.follow.fire('click');
  s.draw();
  [t0, t1] = s.time();
  assert.ok(near(t1, hi) && near(t1 - t0, 16), 'at the edge with its zoom');
  assert.equal(s.follow.textContent, 'Following');
  // The stream goes on: the view with it.
  s.live(strm(4000, 40));
  s.draw();
  assert.ok(near(s.time()[1], 4040 * COL_S));
  // Over the frequency ruler the wheel zooms in frequency, not in time.
  s.main.fire('wheel', { clientX: PW + 10, clientY: 100, deltaY: -1 });
  s.draw();
  assert.ok(near(s.time()[1] - s.time()[0], 16), 'the time as it was');
  s.view.destroy();
});

test('the readout gives S, O and O − S on a stream', () => {
  const s = setup();
  s.store.set('_stream', true);
  s.view.setOption('view', 'OS');
  for (let k = 0; k < 400; k += 4) s.live(strm(k, 4, 200, 220));
  s.draw();
  s.main.fire('pointermove', { clientX: PW / 2, clientY: 120 });
  s.view.drawOverlay();
  const line = s.overlay.getContext('2d').texts.at(-1).t;
  const db = (v) => (-120 + v / 255 * 120).toFixed(1);
  assert.ok(line.includes(`S ${db(200)} dB`) && line.includes(`O ${db(220)} dB`), line);
  assert.ok(line.includes(`Δ +${(20 / 255 * 120).toFixed(1)} dB`), line);
  assert.match(line, /\d:\d\d\.\d\d/, 'and the time');
  s.view.destroy();
});

test('colours that come late land where their columns are now; a new colour map recolours all of it', () => {
  const made = dom.offscreen.length;
  const s = setup();
  s.store.set('_stream', true);
  for (let k = 0; k < 5000; k += 4) s.live(strm(k, 4));
  dom.answerWorker();                        // all of it answered after the strips moved on
  const S = dom.offscreen.slice(made).find(c => c.width === 4096 && c.height === BINS);
  const puts = S.getContext().puts;
  const at = (x, sh) => puts.find(p => p.img.id.endsWith(`:S_${x}_${sh}`));
  assert.equal(at(4092, 904).x, 4092, 'the last columns at the end');
  assert.equal(at(4092, 4).x, 3192, 'columns 4096.. now at 4096 − 904');
  assert.ok(at(0, 0).x <= -900, 'the first ones gone off the left');
  puts.length = 0;
  const sel = s.toolbar.children.find(e => e.tagName === 'SELECT' && e.children.some(o => o.value === 'viridis'));
  sel.value = 'viridis';
  sel.fire('change');
  dom.answerWorker();
  const all = puts.find(p => /:S_0_904$/.test(p.img.id));
  assert.ok(all && all.x === 0 && all.img.nCols === 4096, 'every column it keeps, again');
  s.view.destroy();
});

test('a frame that comes twice or not at all does not start the strips over; a long gap does', () => {
  const made = dom.offscreen.length;
  const s = setup();
  s.store.set('_stream', true);
  for (let k = 0; k < 400; k += 4) {
    if (k === 200) continue;                   // a frame the page missed: columns 200..203
    s.live(strm(k, 4));
    if (k === 100) s.live(strm(k, 4));         // a frame got twice
  }
  s.main.fire('dblclick');
  s.draw();
  let [t0, t1] = s.time();
  assert.ok(near(t0, 0) && near(t1, 400 * COL_S), `all of it from column 0: ${t0}..${t1}`);
  const S = dom.offscreen.slice(made).find(c => c.width === 4096 && c.height === BINS);
  const sizes = S.getContext().puts.map(p => p.img.nCols);
  assert.ok(sizes.includes(8), 'the gap filled: its 4 columns and the next frame\'s 4 in one');
  assert.equal(sizes.reduce((a, b) => a + b, 0), 400, 'each column once: 396 heard and 4 filled');
  // Ten seconds missed: the strips start over there.
  s.live(strm(400 + 220, 40));
  s.main.fire('dblclick');
  s.draw();
  [t0, t1] = s.time();
  assert.ok(near(t0, 620 * COL_S) && near(t1, 660 * COL_S), `${t0}..${t1}`);
  s.view.destroy();
});

test('frames that carry the columns of the one before (STRM): a frame missed leaves no gap to fill', () => {
  const made = dom.offscreen.length;
  const s = setup();
  s.store.set('_stream', true);
  // Frame f brings columns 2f, 2f + 1 and once more those of frame f − 1; every other one is missed.
  const last = 300;
  for (let f = 0; f <= last; f += 2) {
    const k0 = Math.max(0, 2 * (f - 1));
    s.live(strm(k0, 2 * (f + 1) - k0));
  }
  dom.answerWorker();
  const S = dom.offscreen.slice(made).find(c => c.width === 4096 && c.height === BINS);
  const puts = S.getContext().puts.map(p => p.img.nCols);
  assert.equal(puts.reduce((a, b) => a + b, 0), 2 * (last + 1), 'each column once');
  assert.ok(puts.slice(1).every(n => n === 4), `only what it lacked, nothing filled in: ${puts.slice(0, 8)}`);
  s.main.fire('dblclick');
  s.draw();
  const [t0, t1] = s.time();
  assert.ok(near(t0, 0) && near(t1, 2 * (last + 1) * COL_S), `all of it: ${t0}..${t1}`);
  s.view.destroy();
});

test("a file's view zooms and moves as before", () => {
  const s = setup();
  s.bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame: { duration_s: 100, spec_tile_counts_s: [0], n_lods: 0 } } }));
  s.draw();
  assert.deepEqual(s.time(), [0, 100]);
  assert.equal(s.follow.style.display, 'none', 'no Follow for a file');
  assert.equal(s.plot.dataset.tip, undefined, 'no stream tip');
  s.main.fire('wheel', { clientX: PW / 2, clientY: 100, deltaY: -1 });
  s.draw();
  const [t0, t1] = s.time();
  assert.ok(near(t0, 10) && near(t1, 90), `${t0}..${t1}`);
  s.main.fire('dblclick');
  s.draw();
  assert.deepEqual(s.time(), [0, 100]);
  s.view.destroy();
});
