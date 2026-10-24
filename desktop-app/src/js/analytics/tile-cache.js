/**
 * tile-cache.js — LRU-64 tile cache for waveform and spectrogram tiles.
 *
 * Tile key format: "{src}-{kind}-{lod}-{i}"
 *   src:  "S" | "B" | "O"
 *   kind: "wave" | "spec" | "raw"
 *   lod:  0-based LOD index (0 = most zoomed out)
 *   i:    tile index within lod
 *
 * Public API:
 *   createTileCache(ctx)  → TileCache
 *
 * TileCache methods:
 *   request(key, src, kind, lod, i) — schedules a debounced fetch; fires an:tile when ready
 *   get(key)                        — returns Uint8Array | null (cache hit only)
 *   drop(key), dropPrefix(prefix)   — forget tiles (a pass that was replaced)
 *   purge()                         — clears all cached tiles (call on track change)
 *   destroy()                       — removes bus listeners
 */

// FRONTEND-CONTRACT §6: LRU-64, ≤4 in-flight, 50 ms debounce, retry 3× after 150 ms on hub miss.

// A whole track: ~200 waveform tiles (6 KB) and up to 256 spectrogram
// tiles (16 KB) — about 5 MB. At 64 the spectrogram's 256 tiles evicted each
// other (and the waveform's) on every redraw, and only a quarter showed.
const LRU_CAP      = 1024;
const MAX_IN_FLIGHT = 4;   // analytics: at most 4 concurrent tile fetches
const DEBOUNCE_MS  = 50;
const RETRY_DELAY  = 150;
const MAX_RETRIES  = 3;

/** @param {{ store: object, bus: EventTarget }} ctx */
export function createTileCache(ctx) {
  const { store, bus } = ctx;

  /** @type {Map<string, Uint8Array>} key → tile bytes */
  const cache = new Map();
  /** @type {Map<string, number>}     key → LRU counter */
  const lruAt = new Map();
  let lruClock = 0;

  /** @type {Map<string, ReturnType<typeof setTimeout>>} debounce timers */
  const timers = new Map();
  /** @type {Set<string>} currently in-flight fetch keys */
  const inFlight = new Set();
  /** @type {Array<{key:string, src:string, kind:string, lod:number, i:number}>} waiting queue */
  const queue = [];

  // On track change: purge everything (FRONTEND-CONTRACT §6)
  const onTrackChange = () => purge();
  bus.addEventListener('an:track:change', onTrackChange);

  // ── LRU helpers ──────────────────────────────────────────────────────────

  function lruTouch(key) {
    lruAt.set(key, ++lruClock);
  }

  function lruEvict() {
    if (cache.size < LRU_CAP) return;
    // Find the key with the lowest LRU counter
    let oldest = Infinity;
    let evictKey = null;
    for (const [k, ts] of lruAt) {
      if (ts < oldest) { oldest = ts; evictKey = k; }
    }
    if (evictKey !== null) {
      cache.delete(evictKey);
      lruAt.delete(evictKey);
    }
  }

  // ── Fetch ─────────────────────────────────────────────────────────────────

  /** Drain the queue: start fetches for queued keys while under the in-flight cap. */
  function drainQueue() {
    while (queue.length > 0 && inFlight.size < MAX_IN_FLIGHT) {
      const { key, src, kind, lod, i } = queue.shift();
      // Skip if already cached, in-flight, or stale (cancelled by purge)
      if (cache.has(key) || inFlight.has(key)) continue;
      inFlight.add(key);
      fetchTile(key, src, kind, lod, i, 0);
    }
  }

  async function fetchTile(key, src, kind, lod, i, retries) {
    // The store is read with get(); the backend serves /wave and /spec.
    const sid = typeof store.get === 'function' ? store.get('sid') : store.sid;
    if (sid == null) { inFlight.delete(key); drainQueue(); return; }

    const route = kind === 'spec' ? 'spec' : 'wave';
    const url = `https://aura.localhost/player/an/${route}?sid=${sid}&src=${encodeURIComponent(src)}&lod=${lod}&i=${i}`;

    let resp;
    try {
      resp = await fetch(url);
    } catch (err) {
      inFlight.delete(key);
      scheduleRetry(key, src, kind, lod, i, retries);
      drainQueue();
      return;
    }

    if (!resp.ok) {
      inFlight.delete(key);
      scheduleRetry(key, src, kind, lod, i, retries);
      drainQueue();
      return;
    }

    const buf = await resp.arrayBuffer();
    inFlight.delete(key);

    if (buf.byteLength === 0) {
      // Hub miss — retry with delay
      scheduleRetry(key, src, kind, lod, i, retries);
      drainQueue();
      return;
    }

    // Store in cache
    lruEvict();
    const bytes = new Uint8Array(buf);
    cache.set(key, bytes);
    lruTouch(key);

    // Notify views
    bus.dispatchEvent(Object.assign(new Event('an:tile'), { detail: { key, tile: bytes } }));
    drainQueue();
  }

  // A tile that failed MAX_RETRIES times is left alone for a while: the
  // views ask again on every redraw (10 a second while live), and a tile
  // that is not there turned that into thousands of requests.
  const failedAt = new Map();
  const FAILED_HOLD_MS = 5000;

  function scheduleRetry(key, src, kind, lod, i, retries) {
    if (retries >= MAX_RETRIES) { failedAt.set(key, Date.now()); return; }
    const t = setTimeout(() => {
      timers.delete(key);
      if (!inFlight.has(key)) {
        inFlight.add(key);
        fetchTile(key, src, kind, lod, i, retries + 1);
      }
    }, RETRY_DELAY);
    timers.set(key, t);
  }

  // ── Public API ────────────────────────────────────────────────────────────

  /**
   * Schedule a fetch for this tile (debounced).
   * If the tile is already cached, fires an:tile synchronously.
   * If a fetch is already in flight, does nothing.
   */
  function request(key, src, kind, lod, i) {
    const failed = failedAt.get(key);
    if (failed != null) {
      if (Date.now() - failed < FAILED_HOLD_MS) return;
      failedAt.delete(key);
    }
    // Cache hit: fire immediately
    if (cache.has(key)) {
      lruTouch(key);
      const bytes = cache.get(key);
      bus.dispatchEvent(Object.assign(new Event('an:tile'), { detail: { key, tile: bytes } }));
      return;
    }

    // Already pending or in-flight
    if (inFlight.has(key)) return;

    // Clear existing debounce timer
    if (timers.has(key)) {
      clearTimeout(timers.get(key));
    }

    // Debounce
    const t = setTimeout(() => {
      timers.delete(key);
      if (cache.has(key)) return; // arrived while waiting
      if (inFlight.has(key)) return;
      if (inFlight.size >= MAX_IN_FLIGHT) {
        // analytics: in-flight cap reached — enqueue for later dispatch
        if (!queue.some(q => q.key === key)) {
          queue.push({ key, src, kind, lod, i });
        }
        return;
      }
      inFlight.add(key);
      fetchTile(key, src, kind, lod, i, 0);
    }, DEBOUNCE_MS);
    timers.set(key, t);
  }

  /** Returns cached bytes or null (no side effects). */
  function get(key) {
    return cache.get(key) ?? null;
  }

  /**
   * Forget the tiles whose key starts with `prefix` (or is `key`): cached,
   * waiting, queued or held after failures. A fetch already on its way still
   * lands; the views check what they place.
   */
  function dropPrefix(prefix, exact = false) {
    const hit = (k) => (exact ? k === prefix : k.startsWith(prefix));
    for (const k of [...cache.keys()]) if (hit(k)) { cache.delete(k); lruAt.delete(k); }
    for (const [k, t] of [...timers]) if (hit(k)) { clearTimeout(t); timers.delete(k); }
    for (const k of [...failedAt.keys()]) if (hit(k)) failedAt.delete(k);
    for (let i = queue.length - 1; i >= 0; i--) if (hit(queue[i].key)) queue.splice(i, 1);
  }

  const drop = (key) => dropPrefix(key, true);

  /** Clears all cached tiles and cancels pending timers. */
  function purge() {
    for (const t of timers.values()) clearTimeout(t);
    timers.clear();
    inFlight.clear();
    queue.length = 0; // analytics: cancel stale queued requests
    cache.clear();
    lruAt.clear();
  }

  /** Removes bus listeners and cancels all pending activity. */
  function destroy() {
    purge();
    bus.removeEventListener('an:track:change', onTrackChange);
  }

  return { request, get, drop, dropPrefix, purge, destroy };
}
