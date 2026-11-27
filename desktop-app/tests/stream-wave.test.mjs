// A stream's waveform on the analyzer: the ring of the blocks the live frames
// bring (STRM v2), its bins over a stretch of time, and the view that follows
// the live edge or is zoomed and moved by hand.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { createRing, createStreamWave, peakDb, labelAt, HISTORY_S } from '../src/js/analytics/stream-wave.js';

const block = (k, peak) => [-peak, peak, Math.round(peak / 2), -peak, peak, Math.round(peak / 2)];
const env = (k0, n, peak = 16384) => {
  const blocks = new Uint32Array(n), v = new Int16Array(n * 6);
  for (let i = 0; i < n; i++) { blocks[i] = k0 + i; v.set(block(k0 + i, peak), i * 6); }
  return { blocks, v };
};
// A live frame's STRM tail as protocol.js gives it (44.1 kHz: a block is 256/44100 s).
const strm = (k0, n, peak) => ({ srcRate: 44100, clockS: (k0 + n) * 256 / 44100, sEnv: env(k0, n, peak), oEnv: env(k0, n, peak) });

test('the ring holds the newest blocks; its bins are the least, the greatest and the RMS', () => {
  const r = createRing(100);
  const e = env(0, 150);
  r.add(e.blocks, e.v);
  assert.equal(r.newest, 149);
  assert.equal(r.oldest, 50);
  assert.equal(r.at(10), null, 'gone round the ring');
  assert.deepEqual([...r.at(120)], block(120, 16384));
  const b = r.bins(40, 60, 2);            // 40..49 not held, 50..59 held
  assert.ok(Number.isNaN(b[0]));
  assert.ok(Math.abs(b[3] + 0.5) < 1e-4 && Math.abs(b[4] - 0.5) < 1e-4 && Math.abs(b[5] - 0.25) < 1e-3);
  assert.ok(Math.abs(peakDb(r.at(120)) + 6.02) < 0.01);
  assert.ok(r.bins(0, 10, 2) === r.bins(0, 10, 2) || r.bins(0, 10, 2).buffer === r.bins(0, 10, 2).buffer, 'one buffer for the bins, not one per draw');
});

test('the view follows the live edge, zooms at the pointer, looks back, and comes back', () => {
  const sw = createStreamWave();
  sw.feed(strm(0, Math.round(44100 * 60 / 256), 8000)); // a minute
  const hi = 60;
  let v = sw.view();
  assert.ok(Math.abs(v.t1 - hi) < 0.01 && Math.abs(v.t1 - v.t0 - 30) < 0.01, 'the last 30 s at first');
  sw.wheel(50, -1);                                     // zoom in at 50 s
  v = sw.view();
  assert.ok(v.t1 - v.t0 < 30 && v.t0 <= 50 && v.t1 >= 50);
  assert.equal(sw.following, false, 'looked back: no longer following');
  sw.pan(v, 100);                                       // dragged forward past the edge
  assert.ok(Math.abs(sw.view().t1 - hi) < 0.01);
  assert.equal(sw.following, true, 'at the live edge again');
  sw.all();
  v = sw.view();
  assert.ok(v.t0 < 0.01 && Math.abs(v.t1 - hi) < 0.01, 'all of the history');
  assert.ok(HISTORY_S >= 1800);
});

test('another stream starts the waveform afresh', () => {
  const sw = createStreamWave();
  sw.feed(strm(10000, 50));
  sw.wheel(5, -1);
  sw.feed(strm(0, 50));                                 // a new session counts from 0
  assert.equal(sw.following, true);
  assert.ok(sw.view().t1 < 1, 'its clock, not the old one');
});

test('Follow brings the view back to the live edge with its span', () => {
  const sw = createStreamWave();
  sw.feed(strm(0, Math.round(44100 * 60 / 256)));
  sw.wheel(20, -1);
  const span = sw.view().t1 - sw.view().t0;
  assert.equal(sw.following, false);
  sw.followEdge();
  assert.equal(sw.following, true);
  const v = sw.view();
  assert.ok(Math.abs(v.t1 - 60) < 0.01 && Math.abs(v.t1 - v.t0 - span) < 1e-6);
});

// A 2D context that keeps what is written where (11 px monospace: 6.6 px a character).
const fakeCtx = () => {
  const texts = [];
  return {
    texts, fillStyle: '', font: '', globalAlpha: 1,
    fillRect() {}, measureText: (t) => ({ width: t.length * 6.6 }),
    fillText(t, x, y) { texts.push({ t, x, y }); },
  };
};
const overlaps = (a, b) => a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;

test("the pointer's value keeps clear of the Follow button at the right edge", () => {
  const w = 800, h = 200;
  const button = { x: w - 8 - 78, y: 6, w: 78, h: 17 };   // top: 6px; right: 8px (analytics.css)
  const sw = createStreamWave();
  sw.feed(strm(0, Math.round(44100 * 60 / 256), 8000));
  for (const at of [0.2, 0.9, 0.99, 1]) {
    const v = sw.view();
    sw.point(v.t0 + at * (v.t1 - v.t0));
    const c = fakeCtx();
    sw.draw(c, w, h, button);
    const label = c.texts.find(e => e.t.includes('dBFS'));
    assert.ok(label, 'the value is written');
    const box = { x: label.x, y: label.y - 11, w: label.t.length * 6.6, h: 14 };
    assert.ok(!overlaps(box, button), `at ${at}: ${JSON.stringify(box)} under the button`);
    assert.ok(box.x >= 0 && box.x + box.w <= w && box.y + box.h <= h - 16, 'inside the lanes');
  }
  // Away from the button it stays where it was; without one, too.
  assert.deepEqual(labelAt(100, 200, w, button), { x: 106, y: 26 });
  assert.deepEqual(labelAt(790, 200, w, null), { x: w - 204, y: 26 });
  assert.equal(labelAt(790, 200, w, button).y, 6 + 17 + 14);
});

test('frames that carry the blocks of the one before (STRM): a frame missed leaves no hole', () => {
  // Frame f brings blocks 17f..17f+16 and once more those of frame f − 1; every other one is missed.
  const sw = createStreamWave();
  const last = 60;
  for (let f = 0; f <= last; f += 2) {
    const k0 = Math.max(0, 17 * (f - 1));
    sw.feed(strm(k0, 17 * (f + 1) - k0, 9000));
  }
  const ring = sw.rings.s;
  const holes = [];
  for (let k = 0; k < 17 * (last + 1); k++) if (!ring.at(k)) holes.push(k);
  assert.deepEqual(holes, [], 'every block held');
  const b = ring.bins(0, 17 * (last + 1), 100);
  assert.ok(b.every(Number.isFinite), 'no empty bin across the history');
  assert.equal(sw.following, true, 'a block got twice is not another stream');
});
