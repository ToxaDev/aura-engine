// A file's waveform under the pointer: a line and the peaks of S and O there,
// read from the tiles the view draws (as a stream's waveform reads its blocks);
// it goes with the pointer.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { installFakeDom, fakeStore } from './fake-dom.mjs';

const W = 800;
installFakeDom({ w: W, h: 200 });
const { create } = await import('../src/js/analytics/waveform-view.js');

/** An AAWT tile: `n` entries of 2 channels, each entry's peak `peak(j)` (min −peak, max peak). */
function tile(n, peak) {
  const buf = new ArrayBuffer(16 + n * 2 * 12);
  const v = new DataView(buf);
  'AAWT'.split('').forEach((c, i) => v.setUint8(i, c.charCodeAt(0)));
  v.setUint8(7, 2);
  v.setUint32(12, n, true);
  let off = 16;
  for (let j = 0; j < n; j++) {
    for (let c = 0; c < 2; c++, off += 12) {
      const p = c === 0 ? peak(j) : 0.01;           // the left channel is the one drawn
      v.setFloat32(off, -p, true); v.setFloat32(off + 4, p, true); v.setFloat32(off + 8, p / 2, true);
    }
  }
  return new Uint8Array(buf);
}

test("a file's waveform reads the peaks of S and O under the pointer", () => {
  // 10 s in two tiles of 100 entries (50 ms each): S at −6 dBFS with 0 dBFS at 2.00–2.05 s, O at −12.
  const tiles = new Map([
    ['S-wave-0-0', tile(100, j => (j === 40 ? 1 : 0.5))], ['S-wave-0-1', tile(100, () => 0.5)],
    ['O-wave-0-0', tile(100, () => 0.25)], ['O-wave-0-1', tile(100, () => 0.25)],
  ]);
  const tileCache = { get: k => tiles.get(k) ?? null, request() {}, drop() {}, dropPrefix() {} };
  const store = fakeStore({});
  const bus = new EventTarget();
  const container = document.createElement('div');
  const view = create(container, { store, bus, tileCache });
  const detail = container.children[1];
  const [main, overlay] = detail.children;
  bus.dispatchEvent(new CustomEvent('an:track', { detail: { frame: {
    duration_s: 10, n_lods: 1, tile_samples: 256, n_wave_channels: 2, src_rate: 44100,
    wave_tile_counts_s: [2], wave_tile_counts_o: [2],
  } } }));
  const c = overlay.getContext();
  const read = () => { c.texts.length = 0; view.drawOverlay({ timeS: null }); return c.texts.map(t => t.t); };
  main.fire('pointermove', { clientX: 2.02 / 10 * W });
  assert.deepEqual(read(), ['0:02.0 · peak S 0.0 dBFS · O -12.0 dBFS']);
  main.fire('pointermove', { clientX: 7.5 / 10 * W });
  assert.deepEqual(read(), ['0:07.5 · peak S -6.0 dBFS · O -12.0 dBFS'], 'the second tile');
  main.fire('pointerleave');
  assert.deepEqual(read(), [], 'gone with the pointer');
  // Another view's time: a line only.
  bus.dispatchEvent(new CustomEvent('an:cursor:move', { detail: { timeS: 5, sourceViewId: 'spectrogram' } }));
  assert.deepEqual(read(), []);
  view.destroy();
});
