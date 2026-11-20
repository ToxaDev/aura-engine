// The player's status, read on a timer: one request at a time, and never an
// older answer over a newer one.
//
// The aura protocol answers on worker threads (two of them for the status),
// so two requests out together come back in any order. The status is slowest
// just when a chain swaps in — the player's state is busy then — and a poll
// every 250 ms had two out at once: the older answer, landing last, put a
// stage that had just come on the air back to "switching in", and the rack's
// lamps went dark and lit again while the sound went on (Anton 30.10).
//
// No DOM here: the page hands in how to ask and what to do with an answer.

/// A request still unanswered after this long no longer holds the next one
/// back (a lost answer must not stop the status for good); when it comes
/// after all, it is dropped if a newer one was taken meanwhile.
export const STATUS_STALE_MS = 5000;

/// A tick for the poll's timer: `ask()` (async) reads the status, null for
/// none; `take(s)` uses one. A tick while a request is out does nothing; an
/// answer is taken only if no later request's was taken before it. The tick
/// resolves true when it asked.
export function latestOnly(ask, take, { now, staleMs = STATUS_STALE_MS }) {
    let asked = 0, taken = 0, out = 0, outAt = 0;
    return async function tick() {
        if (out && now() - outAt < staleMs) return false;
        const n = ++asked;
        out = n; outAt = now();
        let s = null;
        try { s = await ask(); } catch (_) { s = null; }
        if (out === n) out = 0;
        if (s != null && n > taken) { taken = n; take(s); }
        return true;
    };
}
