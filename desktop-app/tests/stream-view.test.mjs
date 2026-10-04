// The time view of a panel on a live stream (the waveform's, the spectrogram's,
// the loudness's): it follows the live edge or stands where the hand put it
// within what the panel keeps; and the level a spectrogram strip holds.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { createStreamView, stripLevel, zoomAt } from '../src/js/analytics/stream-view.js';

const near = (a, b, eps = 1e-6) => Math.abs(a - b) < eps;
// What a panel keeps: from `lo` to the live edge `hi` (both moved by the test).
const kept = { lo: 0, hi: 0 };
const domain = () => ({ ...kept });

test('it follows the live edge with a steady scale, even while less than the span is kept', () => {
  kept.lo = 0; kept.hi = 5;
  const sv = createStreamView({ span0: 20, domain });
  let v = sv.view();
  assert.ok(near(v.t0, -15) && near(v.t1, 5), 'the last 20 s, the first 15 of them not yet heard');
  kept.hi = 100; kept.lo = 0;
  v = sv.view();
  assert.ok(near(v.t0, 80) && near(v.t1, 100), 'the live edge moves on');
  assert.equal(sv.following, true);
});

test('the wheel zooms at the pointer and stops following; at the edge it follows again', () => {
  kept.lo = 0; kept.hi = 180;
  const sv = createStreamView({ span0: 20, domain });
  sv.wheel(170, -1);                                   // zoom in at 170 s (half the view)
  let v = sv.view();
  assert.ok(near(v.t1 - v.t0, 16), 'zoomed in');
  assert.ok(near(v.t0 + (v.t1 - v.t0) / 2, 170), 'the pointer stays where it was on screen');
  assert.equal(sv.following, false);
  kept.hi = 190;                                       // the stream goes on: the view stands
  assert.ok(near(sv.view().t0, v.t0));
  for (let i = 0; i < 40; i++) sv.wheel(150, 1);       // out as far as it goes
  v = sv.view();
  assert.ok(near(v.t0, 0) && near(v.t1, 190), 'all that is kept, no more');
  assert.equal(sv.following, true, 'its end is the live edge: following again');
});

test('a drag moves the view and Follow takes it back with its span; all of it follows', () => {
  kept.lo = 10; kept.hi = 190;
  const sv = createStreamView({ span0: 20, domain });
  const from = sv.view();
  sv.pan(from, -50);
  let v = sv.view();
  assert.ok(near(v.t0, 120) && near(v.t1, 140));
  assert.equal(sv.following, false);
  sv.pan(v, -1000);
  assert.ok(near(sv.view().t0, 10), 'no further back than what is kept');
  sv.pan(sv.view(), 1e6);
  assert.equal(sv.following, true, 'dragged to the edge: following');
  sv.wheel(100, -1);
  const span = sv.view().t1 - sv.view().t0;
  sv.followEdge();
  v = sv.view();
  assert.equal(sv.following, true);
  assert.ok(near(v.t1, 190) && near(v.t1 - v.t0, span), 'the span kept');
  sv.all();
  v = sv.view();
  assert.ok(near(v.t0, 10) && near(v.t1, 190));
  kept.hi = 200;
  assert.ok(near(sv.view().t1, 200), 'all of it follows the edge');
  assert.ok(near(sv.view().t0, 10), 'and still begins where what is kept begins (all of it, as it grows)');
  sv.followEdge();
  kept.hi = 210;
  assert.ok(near(sv.view().t0, 20) && near(sv.view().t1, 210), 'Follow: the span it had, at the edge');
});

test('the minimap places the view within what is kept; reset starts at the edge again', () => {
  kept.lo = 0; kept.hi = 1800;
  const sv = createStreamView({ span0: 30, domain });
  sv.setView(600, 660);
  assert.deepEqual(sv.view(), { t0: 600, t1: 660 });
  assert.equal(sv.following, false);
  sv.setView(1790, 1850);
  assert.ok(near(sv.view().t1, 1800) && sv.following, 'past the edge: at the edge, following');
  sv.setView(600, 600.01);
  assert.ok(near(sv.view().t1 - sv.view().t0, 0.25), 'no narrower than the least span');
  sv.reset();
  kept.lo = 0; kept.hi = 3;
  assert.ok(near(sv.view().t1, 3) && near(sv.view().t1 - sv.view().t0, 30), 'another stream: the edge, the first span');
  assert.ok(near(zoomAt({ t0: 0, t1: 10 }, 5, -1, 0, 100).t1 - zoomAt({ t0: 0, t1: 10 }, 5, -1, 0, 100).t0, 8));
});

test("a strip's level at a time and frequency", () => {
  // 4 columns of 512 bands from 20 Hz to 22050 Hz, numbered from 100; 46 ms a column.
  const bins = 512, colS = 2048 / 44100, top = 22050;
  const st = { bins, count: 4, first: 100, bytes: new Uint8Array(4 * bins) };
  const band = (hz) => Math.floor(bins * Math.log(hz / 20) / Math.log(top / 20));
  st.bytes[2 * bins + band(1000)] = 255;               // column 102 at 1 kHz: 0 dB
  st.bytes[3 * bins + band(1000)] = 128;               // column 103: about −60 dB
  const t = (k) => (k + 0.5) * colS;
  assert.equal(stripLevel(st, colS, t(102), 1000, top, 120), 0);
  assert.ok(near(stripLevel(st, colS, t(103), 1000, top, 120), -120 + 128 / 255 * 120));
  assert.equal(stripLevel(st, colS, t(101), 1000, top, 120), -120, 'nothing there: the floor');
  assert.equal(stripLevel(st, colS, t(99), 1000, top, 120), null, 'before the strip');
  assert.equal(stripLevel(st, colS, t(104), 1000, top, 120), null, 'after it');
  assert.equal(stripLevel(st, colS, t(102), 10, top, 120), null, 'under 20 Hz');
  assert.equal(stripLevel(st, colS, t(102), 30000, top, 120), null, 'over its top');
  assert.equal(stripLevel(null, colS, t(102), 1000, top, 120), null);
});
