// tile-cache.js: a tile the views ask for on every redraw is still fetched.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createTileCache } from '../src/js/analytics/tile-cache.js';

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

function setup() {
  const urls = [];
  globalThis.fetch = async (url) => {
    urls.push(url);
    return { ok: true, arrayBuffer: async () => new Uint8Array([1, 2, 3]).buffer };
  };
  const bus = new EventTarget();
  const store = { get: (k) => (k === 'sid' ? 7 : undefined) };
  return { urls, bus, cache: createTileCache({ store, bus }) };
}

test('a tile asked for every 20 ms (more often than the debounce) is fetched', async (t) => {
  const { urls, cache } = setup();
  t.after(() => cache.destroy());
  // LIVE redraws on every frame and asks again while the tile is not here:
  // restarting the 50 ms wait each time, it never went out.
  for (let i = 0; i < 25 && !cache.get('O-spec-0-5'); i++) {
    cache.request('O-spec-0-5', 'O', 'spec', 0, 5);
    await sleep(20);
  }
  assert.ok(cache.get('O-spec-0-5'), 'the tile arrived while still being asked for');
  assert.equal(urls.length, 1, 'fetched once');
  assert.match(urls[0], /\/player\/an\/spec\?sid=7&src=O&lod=0&i=5$/);
});

test('asked again while waiting: one fetch, and the views hear of it', async (t) => {
  const { urls, bus, cache } = setup();
  t.after(() => cache.destroy());
  const heard = [];
  bus.addEventListener('an:tile', (e) => heard.push(e.detail.key));
  cache.request('S-spec-0-0', 'S', 'spec', 0, 0);
  cache.request('S-spec-0-0', 'S', 'spec', 0, 0);
  await sleep(100);
  assert.equal(urls.length, 1);
  assert.deepEqual(heard, ['S-spec-0-0']);
});
