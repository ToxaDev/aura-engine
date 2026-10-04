/**
 * scheduler.js — Polling scheduler for the analyzer window.
 *
 * Manages three independent polling loops (live/track/resp) with exactly
 * one in-flight request per route and exponential backoff on errors.
 *
 * Per FRONTEND-CONTRACT.md §6:
 *   - live:  10 Hz, always while subject active
 *   - track: on rev change (chain_rev changes)
 *   - resp:  on chain_rev change or zoom change (>2% f0/f1/n change)
 *
 * Never falls back to invoke for data routes.
 *
 * Public API:
 *   createScheduler(sid, opts)
 *     sid   u32 — subject id from an_subject_open
 *     opts  { onLive, onTrack, onResp, onError }
 *   Returns:
 *     { start(), stop(), notifyChainChange(rev), notifyZoomChange(f0,f1,n) }
 */

const BASE_URL = 'https://aura.localhost/player/an';

/**
 * @param {number} sid
 * @param {{
 *   onLive:  (buf: ArrayBuffer) => void,
 *   onTrack: (buf: ArrayBuffer) => void,
 *   onResp:  (buf: ArrayBuffer) => void,
 *   onError: (route: string, err: Error) => void,
 * }} opts
 */
export function createScheduler(sid, opts) {
  const { onLive, onTrack, onResp, onError } = opts;

  let _running = false;
  let _liveTimer   = null;
  let _trackTimer  = null;
  let _respTimer   = null;

  // Per-route in-flight guard
  let _liveInFlight  = false;
  let _trackInFlight = false;
  let _respInFlight  = false;

  // live loop state
  let _lastSeq       = 0;
  let _liveBackoff   = 100;  // ms

  // track loop state
  // The track frame is polled all the time: the whole-track analysis fills it
  // in several steps and a new track can begin without a chain change. An
  // unchanged frame costs an 8-byte stub.
  let _lastTrackRev  = 0;
  let _trackBackoff  = 500;
  let _trackEpoch    = 0;    // bumped by refetchTrack(): older replies are dropped

  // resp loop state
  let _lastChainRev  = null;  // null: the first live frame always fetches
  let _respF0        = 20;
  let _respF1        = 22050;
  let _respN         = 1024;
  let _respBackoff   = 200;
  let _respPending   = false;

  // ── Live ───────────────────────────────────────────────────────────────────

  async function _pollLive() {
    if (!_running || _liveInFlight) return;
    _liveInFlight = true;
    try {
      const url = `${BASE_URL}/live?sid=${sid}&since=${_lastSeq}`;
      const res = await fetch(url, { cache: 'no-store' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const buf = await res.arrayBuffer();
      if (buf.byteLength > 0) {
        onLive(buf);
      }
      _liveBackoff = 100; // reset on success
    } catch (err) {
      if (typeof onError === 'function') onError('live', err);
      _liveBackoff = Math.min(_liveBackoff * 2, 800);
    } finally {
      _liveInFlight = false;
    }
    if (_running) {
      _liveTimer = setTimeout(_pollLive, _liveBackoff);
    }
  }

  // ── Track ─────────────────────────────────────────────────────────────────

  async function _pollTrack() {
    if (!_running || _trackInFlight) return;
    _trackInFlight = true;
    // A reply to a request made before refetchTrack() is stale: its stub
    // would set the rev back to the old one and the whole frame asked for
    // would never come (the table stood empty until the analysis moved on).
    const epoch = _trackEpoch;
    let stale = false;
    try {
      const url = `${BASE_URL}/track?sid=${sid}&rev=${_lastTrackRev}`;
      const res = await fetch(url, { cache: 'no-store' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const buf = await res.arrayBuffer();
      stale = epoch !== _trackEpoch;
      if (!stale && buf.byteLength > 0) {
        onTrack(buf);
      }
      // A full frame (not the 8-byte stub): the analysis moved on, and with
      // it the source spectrum in the response frame.
      if (!stale && buf.byteLength > 8) _scheduleResp();
      _trackBackoff = 500;
    } catch (err) {
      if (typeof onError === 'function') onError('track', err);
      _trackBackoff = Math.min(_trackBackoff * 2, 2000);
    } finally {
      _trackInFlight = false;
    }
    if (_running) {
      clearTimeout(_trackTimer);
      _trackTimer = setTimeout(_pollTrack, stale ? 0 : _trackBackoff);
    }
  }

  // ── Resp ──────────────────────────────────────────────────────────────────

  async function _pollResp() {
    if (!_running || _respInFlight) return;
    _respInFlight = true;
    _respPending  = false;
    const { f0, f1, n } = { f0: _respF0, f1: _respF1, n: _respN };
    try {
      const url = `${BASE_URL}/resp?sid=${sid}&f0=${f0}&f1=${f1}&n=${n}&scale=log`;
      const res = await fetch(url, { cache: 'no-store' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const buf = await res.arrayBuffer();
      if (buf.byteLength > 0) {
        onResp(buf);
      }
      _respBackoff = 200;
    } catch (err) {
      if (typeof onError === 'function') onError('resp', err);
      _respBackoff = Math.min(_respBackoff * 2, 800);
    } finally {
      _respInFlight = false;
    }
    // If a change came in while we were in-flight, re-fetch
    if (_running && _respPending) {
      _respTimer = setTimeout(_pollResp, _respBackoff);
    }
  }

  function _scheduleResp() {
    _respPending = true;
    if (!_respInFlight && _running) {
      clearTimeout(_respTimer);
      _respTimer = setTimeout(_pollResp, 50); // small debounce
    }
  }

  // ── Public API ────────────────────────────────────────────────────────────

  return {
    start() {
      if (_running) return;
      _running = true;
      _liveTimer = setTimeout(_pollLive, 0);
      // A file window gets no live frames: its track frame starts here.
      _trackTimer = setTimeout(_pollTrack, 0);
    },

    stop() {
      _running = false;
      clearTimeout(_liveTimer);
      clearTimeout(_trackTimer);
      clearTimeout(_respTimer);
    },

    /**
     * Called by window.js when a new chain_rev is observed from AAN1.
     * Triggers track and resp re-fetches.
     * @param {number} newChainRev
     */
    notifyChainChange(newChainRev) {
      if (newChainRev === _lastChainRev) return;
      _lastChainRev    = newChainRev;
      // Schedule resp immediately
      _scheduleResp();
      // Schedule track fetch
      if (!_trackInFlight) {
        clearTimeout(_trackTimer);
        _trackTimer = setTimeout(_pollTrack, 0);
      }
    },

    /**
     * Called by window.js when a new track_rev (AAN2 rev) is received.
     * Updates the rev so future track polls use the new value.
     * @param {number} rev
     */
    notifyTrackRev(rev) {
      _lastTrackRev = rev;
    },

    /**
     * The views were cleared (a new track): ask for the whole track frame
     * now, not "anything newer than" the one they had. A frame that came
     * before the live frames told of the new track would otherwise not come
     * again until the analysis moved on, and the table stood empty.
     */
    refetchTrack() {
      _trackEpoch++;
      _lastTrackRev = 0;
      if (_running && !_trackInFlight) {
        clearTimeout(_trackTimer);
        _trackTimer = setTimeout(_pollTrack, 0);
      }
    },

    /**
     * Called when the spectrum view zoom changes (f0, f1, or n changes > 2%).
     * @param {number} f0
     * @param {number} f1
     * @param {number} n
     */
    notifyZoomChange(f0, f1, n) {
      const changed =
        Math.abs(f0 - _respF0) / _respF0 > 0.02 ||
        Math.abs(f1 - _respF1) / _respF1 > 0.02 ||
        n !== _respN;
      if (!changed) return;
      _respF0 = f0;
      _respF1 = f1;
      _respN  = n;
      _scheduleResp();
    },

    /**
     * Update the seq after a successful live decode so future polls continue from it.
     * @param {number} seq
     */
    updateLiveSeq(seq) {
      if (seq > _lastSeq || seq === 0) _lastSeq = seq;
    },
  };
}
