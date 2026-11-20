// The stereo panel on a live stream: under the live correlation, the song's
// and the session's of S and O (the stream's totals) instead of the whole
// track's; a file's whole track as it was.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore } from './fake-dom.mjs';

installFakeDom({ w: 700, h: 360 });
const { create } = await import('../src/js/analytics/stereo-view.js');

function setup() {
  const store = fakeStore({});
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus });
  const ctx = container.children[0].getContext();
  const texts = () => ctx.texts.map(t => t.t);
  return { store, bus, container, view, ctx, texts };
}

test("a stream's song and session, S and O, under the live numbers", (tc) => {
  const h = setup();
  tc.after(() => h.view.destroy());   // its poll timer goes even when an assertion fails
  h.store.set('_stream', true);
  assert.match(h.container.dataset.tip, /this song and the session for S and O/);
  h.ctx.texts.length = 0;
  h.store.set('_streamTotals', {
    song: { s: { corr: 0.8125 }, o: { corr: 0.75 } },
    session: { s: { corr: 0.5 }, o: { corr: null } },
  });
  const t = h.texts();
  assert.ok(!t.includes('Whole track'), t.join(' | '));
  const at = (title) => t.indexOf(title);
  assert.ok(at('This song') >= 0 && at('Session') > at('This song'), t.join(' | '));
  const after = (title) => t.slice(at(title) + 1, at(title) + 5).join(' | ');
  assert.match(after('This song'), /correlation 0\.813 {3}width ≈ −9\.9 dB.*correlation 0\.750/);
  assert.match(after('Session'), /correlation 0\.500 {3}width ≈ −4\.8 dB.*correlation — {3}width ≈ —/);
});

test("a file's whole track as before", (tc) => {
  const h = setup();
  tc.after(() => h.view.destroy());
  const m = (r) => { const a = new Array(26).fill(NaN); a[23] = r; return a; };
  h.ctx.texts.length = 0;
  h.bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame: { s_metrics: m(0.9), b_metrics: m(NaN), o_metrics: m(0.8) } } }));
  const t = h.texts();
  assert.ok(t.includes('Whole track') && !t.includes('This song'), t.join(' | '));
  assert.ok(t.some(x => x.startsWith('correlation 0.900')), t.join(' | '));
  assert.match(h.container.dataset.tip, /the whole track for S, B and O/);
});
