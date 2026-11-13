// The meters over the network, in blocks (Anton 8.11).
//
// In the app's window the bars, the Hybrid-Phase lamp and the background
// under HP's unit each ask the engine once a screen frame: cheap over IPC,
// 40-60 requests a second over Wi-Fi. Here the remote takes what is coming
// in blocks instead, as far ahead as the engine has rendered it (its ring
// holds a few seconds past what the DAC plays):
//
//   /player/spectrum_span  the bands on the ring's 20 ms grid, from just
//                          behind the heard frame to the write head;
//   /player/phase_view     HP's switch and the detector's envelope, 5 ms
//                          bins, the same stretch.
//
// Each answer says which frame the DAC plays at the moment it was made (the
// engine has already taken off the device's latency and the output buffer).
// That moment is put at the middle of the request (half the round trip each
// way), and from it this side keeps its own clock of the heard frame, nudged
// by every later answer. The app's own per-frame requests are then answered
// here, from the block, at the clock's frame plus the ahead_ms the page
// asked for (its listener delay included): nothing goes over the network on
// a frame. A new block goes when what is left of the last one runs short,
// and at once after a seek, a track change, play or pause, or a rack change
// (the engine rewrites its ring then: the splice count in the answer).

/// The ring's grid for the bars, ms.
export const SPEC_STEP_MS = 20;
/// HP's bins, ms (the meter notes every 2 ms).
export const HP_BIN_MS = 5;
/// How far back the first block of a run reaches, ms: the background under
/// HP's unit shows the last two seconds.
export const FIRST_BACK_MS = 2100;
/// Later HP blocks start this far behind the heard frame, ms.
export const OVERLAP_MS = 250;
/// How far ahead a block is asked for, ms (the engine sends what it has).
export const AHEAD_MS = 4500;
/// While it plays, HP's own short block this often, ms: the engine notes
/// the switch only a little ahead of the heard frame, so a bars' block
/// (seconds apart) left the last few hundred ms before NOW empty.
export const HP_EVERY_MS = 200;
/// How far ahead that short block reaches, ms.
export const HP_AHEAD_MS = 1200;
/// A block at least this often (the clock follows the engine), ms.
export const MAX_GAP_MS = 3000;
/// Never more often than this, ms (but for a kick).
export const MIN_GAP_MS = 350;
/// A meter the page has not asked for in this long is not fetched, ms.
export const IDLE_MS = 1500;
/// After a failed request, ms.
export const RETRY_MS = 1500;
/// The clock: how much of an answer's difference it takes, and past what it
/// jumps (a seek, the next track), in ms of sound.
export const CLOCK_GAIN = 0.35;
export const CLOCK_SNAP_MS = 150;

const qnum = (q, key, d) => {
    const m = new RegExp('(?:^|&)' + key + '=([^&]*)').exec(q);
    const v = m ? parseFloat(m[1]) : NaN;
    return Number.isFinite(v) ? v : d;
};

/// A clock of the heard position: `pos` (any unit) at page time `at`, going
/// `perMs` units a millisecond while it runs.
export function makeClock() {
    return { pos: 0, at: 0, perMs: 0, running: false, synced: false };
}
/// Where the clock is at page time `t`.
export const clockAt = (c, t) => c.pos + (c.running ? (t - c.at) * c.perMs : 0);
/// An answer: at page time `t` the position was `pos`. Returns 'start',
/// 'snap', 'follow' or 'still'.
export function clockTake(c, pos, t, perMs, running, snapUnits) {
    const was = c.synced;
    if (!running) {
        Object.assign(c, { pos, at: t, perMs, running: false, synced: true });
        return 'still';
    }
    const guess = clockAt(c, t);
    const err = pos - guess;
    if (was && c.running && c.perMs === perMs && Math.abs(err) < snapUnits) {
        Object.assign(c, { pos: guess + err * CLOCK_GAIN, at: t });
        return 'follow';
    }
    Object.assign(c, { pos, at: t, perMs, running: true, synced: true });
    return was ? 'snap' : 'start';
}

// ── the server's clock ─────────────────────────────────────────────────
// Each engine answer of the streamer says, on the server's own clock, when
// the engine took the request up (a meter reads the heard frame first) and
// when the answer left: `x-aura-clock: <read>;<send>`. With the page's own
// times it went out and came back, the offset θ (server − page) is bounded
// both ways: θ ≤ read − sent (the request cannot arrive before it left)
// and θ ≥ send − got (nor the answer). Over many answers the tightest
// bounds come from different ones — the quickest way up and the quickest
// way down — so a way slow in one direction (a phone's Wi-Fi asleep between
// packets holds what comes to it) no longer puts the heard frame late by
// half its round trip: the meters on a phone ran tens of ms behind the PC
// (Anton 11.11). The middle of the two bounds is taken.

/// The answers the bounds are taken from, ms of page time (the two clocks
/// drift apart by a few ms an hour at most).
export const CLOCK_WINDOW_MS = 30000;

/// `x-aura-clock` read: [read, send], or null.
export function parseClockHeader(h) {
    if (!h) return null;
    const [a, b] = String(h).split(';').map(Number);
    return Number.isFinite(a) && Number.isFinite(b) ? [a, b] : null;
}

/// The offset (server − page, ms) from the samples [{ sent, got, read,
/// send }], or null with none.
export function clockOffset(samples) {
    let lo = -Infinity, hi = Infinity;
    for (const s of samples) {
        lo = Math.max(lo, s.send - s.got);
        hi = Math.min(hi, s.read - s.sent);
    }
    if (!Number.isFinite(lo) || !Number.isFinite(hi)) return null;
    return (lo + hi) / 2;
}

// ── the bars: /player/spectrum_span ─────────────────────────────────────

/// The span's head (controller.rs spectrum_span): ten f64.
export function parseSpan(buf, bands) {
    if (!buf || buf.byteLength < 80) return null;
    const dv = new DataView(buf);
    const f = (i) => dv.getFloat64(i * 8, true);
    const head = {
        audible: f(0), rate: f(1), step: f(2), k0: f(3), n: f(4), ring: f(5), gen: f(6),
        spliceAt: f(7), write: f(8), state: ['playing', 'paused', 'held'][f(9)] || 'none',
    };
    const slices = [];
    const per = 1 + bands;
    for (let j = 0, o = 80; j < head.n && o + per <= buf.byteLength; j++, o += per) {
        if (dv.getUint8(o) === 1) slices.push([head.k0 + j, new Uint8Array(buf.slice(o + 1, o + per))]);
    }
    return { head, slices };
}

/// The bands at fractional grid index `q`: between the two slices around
/// it; null where neither is held.
export function bandsAt(slices, q, bands) {
    const k = Math.floor(q), fr = q - k;
    const a = slices.get(k), b = slices.get(k + 1);
    if (!a && !b) return null;
    if (!a || !b) return (a || b).slice(0, bands);
    const out = new Uint8Array(bands);
    for (let i = 0; i < bands; i++) out[i] = Math.round(a[i] + (b[i] - a[i]) * fr);
    return out;
}

// ── HP: /player/phase_view ──────────────────────────────────────────────

/// The view's answer (phase_meter.rs view): null when nothing blends.
export function parseViewBlock(buf) {
    const b = new Uint8Array(buf || new ArrayBuffer(0));
    if (b.length < 8) return null;
    const n = b[4] | b[5] << 8;
    if (!n || b.length < 8 + 2 * n) return null;
    const t = 8 + 2 * n;
    return {
        kind: b[0], state: b[1], hasEnv: b[2], frac: b[3] / 256, n, nowK: b[6] | b[7] << 8,
        sens: b.length > t ? b[t] : 0,
        nowBin: b.length >= t + 5 ? (b[t + 1] | b[t + 2] << 8 | b[t + 3] << 16 | b[t + 4] << 24) >>> 0 : null,
        w: b.slice(8, 8 + n), env: b.slice(8 + n, t),
    };
}

/// The view the page asked for, made from the bins held (`bins`: grid bin of
/// `binMs` → [w, env]) with the heard frame at `posMs` (in the bins' own
/// time, ms): the bytes the engine would send (phase_meter.rs view), on a
/// grid of (to - from) / n.
export function synthView(hp, posMs, fromMs, toMs, n) {
    n = Math.max(1, Math.min(1024, Math.round(n)));
    const span = Math.min(10000, Math.max(100, toMs - fromMs));
    const binA = span / n;
    const nowA = posMs / binA;
    const nowBin = Math.floor(nowA);
    const g0 = Math.floor((posMs + fromMs) / binA);
    const out = new Uint8Array(8 + 2 * n + 5);
    out[0] = hp.kind; out[1] = hp.state; out[2] = hp.hasEnv;
    out[3] = Math.min(255, Math.floor((nowA - nowBin) * 256));
    out[4] = n & 255; out[5] = n >> 8;
    const nowK = Math.max(0, Math.min(65535, nowBin - g0));
    out[6] = nowK & 255; out[7] = nowK >> 8;
    const f = hp.binMs;
    for (let k = 0; k < n; k++) {
        const a = (g0 + k) * binA, z = a + binA;
        if (a + binA <= 0) continue;
        let w = 0, e = 0;
        const i0 = Math.floor(Math.max(0, a) / f), i1 = Math.max(i0, Math.ceil(z / f) - 1);
        for (let i = i0; i <= i1; i++) {
            const v = hp.bins.get(i);
            if (!v) continue;
            if (v[0] > w) w = v[0];
            if (v[1] > e) e = v[1];
        }
        out[8 + k] = w;
        out[8 + n + k] = e;
    }
    const t = 8 + 2 * n;
    out[t] = hp.sens;
    const nb = Math.max(0, nowBin) >>> 0;
    out[t + 1] = nb & 255; out[t + 2] = nb >>> 8 & 255; out[t + 3] = nb >>> 16 & 255; out[t + 4] = nb >>> 24 & 255;
    return out;
}

/// The lamp (controller.rs phase_lamp): the peak of the switch over the
/// `backMs` before `posMs`, as [HP, αHP, state].
export function synthLamp(hp, posMs, backMs) {
    const f = hp.binMs;
    let v = 0;
    const i1 = Math.floor(posMs / f), i0 = Math.floor((posMs - backMs) / f);
    for (let i = i0; i <= i1; i++) { const b = hp.bins.get(i); if (b && b[0] > v) v = b[0]; }
    return hp.kind === 2 ? new Uint8Array([0, v, hp.state]) : new Uint8Array([v, 0, hp.state]);
}

// ── the feed ────────────────────────────────────────────────────────────

const bytes = (u8) => new Response(u8, { status: 200, headers: { 'Content-Type': 'application/octet-stream' } });
const ROUTE = /\/aura\/player\/(spectrum|phase_lamp|phase_view)(?:\?(.*))?$/;

/// opts: { fetch (the network's), now () => page ms, base ('/aura') }.
export function createFeed(opts) {
    const net = opts.fetch;
    const now = opts.now || (() => performance.now());
    const base = opts.base || '/aura';
    const spec = {
        bands: 0, rate: 0, step: 0, ring: null, gen: null, state: 'none', write: 0, leadMs: 0,
        slices: new Map(), clock: makeClock(), wanted: -1e9,
    };
    const hp = {
        on: false, kind: 0, state: 0, hasEnv: 0, sens: 0, binMs: HP_BIN_MS,
        bins: new Map(), clock: makeClock(), wanted: -1e9, last: -1,
    };
    let pending = false, lastAsk = -1e9, retryAt = 0, kicked = false, kickTimers = [];
    let seen = null;   // the last status seen (onStatus)
    const stats = { blocks: 0, served: 0 };

    // ── a block ─────────────────────────────────────────────────────────
    function due(t) {
        if (pending || t < retryAt) return false;
        const want = t - spec.wanted < IDLE_MS || t - hp.wanted < IDLE_MS;
        if (!want) return false;
        if (kicked) return true;
        // At rest (stopped, paused, held) nothing new comes from the engine:
        // no asking until the status says it plays again (that kicks).
        if (lastAsk > -1e8 && spec.state !== 'playing' && seen && seen.state !== 'playing') return false;
        if (t - lastAsk < MIN_GAP_MS) return false;
        if (t - lastAsk >= MAX_GAP_MS || !spec.clock.synced) return true;
        if (spec.state !== 'playing') return t - lastAsk >= 1000;
        // What is left ahead of the heard frame, ms; a new block once it is
        // under 40 % of what the engine holds ahead (at least 400 ms).
        const left = leftMs(t);
        return left < Math.max(400, Math.min(1500, spec.leadMs * 0.4));
    }
    /// What is left of the bars ahead of the heard frame, ms. HP's bins
    /// past the write head are not noted yet (zeros): a block's worth of
    /// them is good only as far as the ring's, so the ring's lead decides.
    function leftMs(t) {
        if (!spec.slices.size || !spec.step) return 0;
        let last = -Infinity;
        for (const k of spec.slices.keys()) if (k > last) last = k;
        return (last * spec.step - clockAt(spec.clock, t)) / spec.rate * 1000;
    }

    async function ask() {
        const t = now();
        pending = true;
        kicked = false;
        lastAsk = t;
        const wantHp = t - hp.wanted < IDLE_MS;
        // The bars: from just behind the newest slice held (or behind the
        // heard frame) to as far as the engine has.
        let from = -300;
        if (spec.clock.synced && spec.step && spec.slices.size && spec.state === 'playing') {
            const frame = clockAt(spec.clock, t);
            let last = -Infinity;
            for (const k of spec.slices.keys()) if (k > last) last = k;
            from = Math.max(-300, Math.min(AHEAD_MS - 200, ((last - 1) * spec.step - frame) / spec.rate * 1000));
        }
        // The span always: its head is the clock and the state, for HP too.
        const reqs = [];
        const bands = spec.bands || 48;
        reqs.push(timed(`${base}/player/spectrum_span?bands=${bands}&from_ms=${from.toFixed(1)}&to_ms=${AHEAD_MS}&step_ms=${SPEC_STEP_MS}`)
            .then(([buf, mid]) => takeSpan(buf, mid, bands)));
        if (wantHp) {
            // The bins: exactly HP_BIN_MS each at the ring's rate, so the
            // grid is the same in every answer.
            const rate = spec.rate || 0;
            const binMs = rate ? Math.max(1, Math.round(HP_BIN_MS * rate / 1000)) / rate * 1000 : HP_BIN_MS;
            // From a little behind the heard frame every time: the bins past
            // the write head came as zeros last time (not rendered yet) and
            // are asked again; the first block also brings the last two
            // seconds for the background.
            const hFrom = hp.on && hp.clock.synced && heldBack(clockAt(hp.clock, t)) ? -OVERLAP_MS : -FIRST_BACK_MS;
            const n = Math.min(1020, Math.round((AHEAD_MS - hFrom) / binMs));
            const hTo = hFrom + n * binMs;
            reqs.push(timed(`${base}/player/phase_view?ahead_ms=0&from_ms=${hFrom.toFixed(3)}&to_ms=${hTo.toFixed(3)}&n=${n}`)
                .then(([buf, mid]) => takeView(buf, mid, binMs)));
        }
        try {
            await Promise.all(reqs);
            stats.blocks++;
        } catch (_) {
            retryAt = now() + RETRY_MS;
        } finally {
            pending = false;
        }
        if (kicked) setTimeout(tick, 0);
    }
    // When the engine read the heard frame: it does that first, then works
    // out the block (tens of ms for a fresh span), so the answer's middle is
    // late by half that work. Half the network's own round trip is taken
    // instead: the quickest of the page's recent small requests (its status
    // poll), never past the middle.
    const rtts = [];
    function noteRtt(ms) {
        if (!(ms >= 0)) return;
        rtts.push(ms);
        if (rtts.length > 24) rtts.shift();
    }
    // The server's clock (above): every engine answer's stamp, the status
    // poll's too (shim.js).
    const clocks = [];
    function noteClock(sent, got, header) {
        const c = parseClockHeader(header);
        if (!c || !(got >= sent)) return null;
        clocks.push({ sent, got, read: c[0], send: c[1] });
        while (clocks.length > 1 && (clocks[0].got < got - CLOCK_WINDOW_MS || clocks.length > 400)) clocks.shift();
        return c;
    }
    async function timed(url) {
        const sent = now();
        const r = await net(url, { cache: 'no-store' });
        // The answer's head is in: what comes after is its body, not the way.
        const head = now();
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const c = noteClock(sent, head, r.headers && r.headers.get && r.headers.get('x-aura-clock'));
        const buf = await r.arrayBuffer();
        const got = now();
        // The server's own time the heard frame was read at, on this page's clock.
        const theta = c ? clockOffset(clocks) : null;
        if (theta != null) return [buf, c[0] - theta];
        const mid = (sent + got) / 2;
        const fast = rtts.length ? Math.min(...rtts) : null;
        return [buf, fast == null ? mid : Math.min(mid, sent + fast / 2)];
    }

    function takeSpan(buf, mid, bands) {
        const p = parseSpan(buf, bands);
        if (!p) { spec.state = 'none'; spec.slices.clear(); spec.clock.synced = false; return; }
        const h = p.head;
        const anew = h.ring === spec.ring && spec.clock.synced && h.audible < clockAt(spec.clock, mid) - h.rate;
        if (h.ring !== spec.ring || h.rate !== spec.rate || h.step !== spec.step || bands !== spec.bands || anew) {
            spec.slices.clear();
            spec.clock.synced = false;
        } else if (h.gen !== spec.gen) {
            // Rewritten from the splice on (a rack change, a seek): those slices are gone.
            const first = h.spliceAt < 0 ? 0 : Math.floor((h.spliceAt - 0.05 * h.rate) / h.step);
            for (const k of [...spec.slices.keys()]) if (k >= first) spec.slices.delete(k);
        }
        Object.assign(spec, { ring: h.ring, rate: h.rate, step: h.step, gen: h.gen, bands, state: h.state, write: h.write });
        spec.leadMs = Math.max(0, (h.write - h.audible) / h.rate * 1000);
        clockTake(spec.clock, h.audible, mid, h.rate / 1000, h.state === 'playing', CLOCK_SNAP_MS * h.rate / 1000);
        for (const [k, v] of p.slices) spec.slices.set(k, v);
        // Kept: a second behind the heard frame.
        const old = Math.floor((h.audible - h.rate) / h.step);
        for (const k of [...spec.slices.keys()]) if (k < old) spec.slices.delete(k);
    }

    function takeView(buf, mid, binMs) {
        const v = parseViewBlock(buf);
        if (!v || v.nowBin == null) {
            hp.on = false; hp.bins.clear(); hp.last = -1; hp.clock.synced = false;
            hp.state = v ? v.state : 0;
            return;
        }
        // The grid's own position of the heard frame, ms.
        const pos = (v.nowBin + v.frac) * binMs;
        const g0 = v.nowBin - v.nowK;
        if (hp.binMs !== binMs || hp.kind !== v.kind || (hp.clock.synced && Math.abs(pos - clockAt(hp.clock, mid)) > 1000)) {
            // Another grid, another blend, a jump (a seek, the next track: a
            // segment's frames start again): none of the old bins hold.
            hp.bins.clear();
            hp.last = -1;
            hp.clock.synced = false;
        }
        Object.assign(hp, { on: true, kind: v.kind, state: v.state, hasEnv: v.hasEnv, sens: v.sens, binMs });
        // HP's frames run while the engine says it plays (the span knows).
        const running = spec.state === 'playing' || !spec.clock.synced;
        clockTake(hp.clock, pos, mid, 1, running, CLOCK_SNAP_MS);
        for (let k = 0; k < v.n; k++) hp.bins.set(g0 + k, [v.w[k], v.env[k]]);
        hp.last = Math.max(hp.last, g0 + v.n - 1);
        const old = Math.floor((pos - 4000) / binMs);
        for (const k of [...hp.bins.keys()]) if (k < old) hp.bins.delete(k);
    }

    /// Whether the bins held reach back from the heard frame (`posMs`, the
    /// grid's ms) as far as the background shows (the track's start nearer).
    function heldBack(posMs) {
        const need = Math.max(0, Math.floor((posMs - FIRST_BACK_MS + 2 * OVERLAP_MS) / hp.binMs));
        return hp.bins.has(need) && hp.bins.has(Math.floor(posMs / hp.binMs));
    }

    let hpPending = false, hpAsk = -1e9;
    async function askHp() {
        const t = now();
        hpPending = true; hpAsk = t;
        const rate = spec.rate || 0;
        const binMs = rate ? Math.max(1, Math.round(HP_BIN_MS * rate / 1000)) / rate * 1000 : HP_BIN_MS;
        // Behind too, when the bins held do not reach back the background's
        // width (dropped on a jump: what came since is all there is).
        const back = heldBack(clockAt(hp.clock, t)) ? OVERLAP_MS : FIRST_BACK_MS;
        const n = Math.min(1020, Math.round((HP_AHEAD_MS + back) / binMs));
        const hFrom = -back, hTo = hFrom + n * binMs;
        try {
            const [buf, mid] = await timed(`${base}/player/phase_view?ahead_ms=0&from_ms=${hFrom.toFixed(3)}&to_ms=${hTo.toFixed(3)}&n=${n}`);
            if (!pending) takeView(buf, mid, binMs);
        } catch (_) {} finally { hpPending = false; }
    }
    function tick() {
        const t = now();
        if (due(t)) ask();
        else if (!pending && !hpPending && hp.on && hp.clock.synced && spec.state === 'playing'
            && t - hp.wanted < IDLE_MS && t - hpAsk >= HP_EVERY_MS && t >= retryAt) askHp();
    }

    // ── the page's requests, answered here ──────────────────────────────
    function handle(url) {
        const m = ROUTE.exec(url.replace(/^https?:\/\/[^/]+/, ''));
        if (!m) return null;
        const t = now();
        const q = m[2] || '';
        let out;
        if (m[1] === 'spectrum') {
            spec.wanted = t;
            const bands = Math.max(1, Math.min(128, qnum(q, 'bands', 48)));
            if (bands !== spec.bands) { spec.bands = bands; kicked = true; }
            const ahead = qnum(q, 'ahead_ms', 35);
            out = new Uint8Array(0);
            if (spec.clock.synced && spec.state === 'playing' && spec.step) {
                const frame = clockAt(spec.clock, t) + ahead * spec.rate / 1000;
                out = bandsAt(spec.slices, frame / spec.step, bands) || out;
            }
        } else {
            // Not asked for a while (its unit out of sight): what is held is
            // old and has holes; start again with the last two seconds.
            if (t - hp.wanted > IDLE_MS && hp.wanted > -1e8) {
                hp.bins.clear(); hp.last = -1; hp.clock.synced = false; hp.on = false;
                kicked = true;
            }
            hp.wanted = t;
            const ahead = qnum(q, 'ahead_ms', 35);
            const ready = hp.on && hp.clock.synced;
            const pos = ready ? clockAt(hp.clock, t) + ahead : 0;
            if (m[1] === 'phase_lamp') {
                const playing = spec.state === 'playing';
                out = ready && playing ? synthLamp(hp, pos, Math.min(50, Math.max(8, qnum(q, 'back_ms', 16))))
                    : new Uint8Array([0, 0, ready ? hp.state : 0]);
            } else {
                // Not with a hole behind: the page keeps the past as first
                // seen, and a hole would stay there as silence.
                out = ready && heldBack(pos) ? synthView(hp, pos, qnum(q, 'from_ms', -1600), qnum(q, 'to_ms', 400), qnum(q, 'n', 200))
                    : new Uint8Array(0);
            }
        }
        stats.served++;
        tick();
        return Promise.resolve(bytes(out));
    }

    /// Something moved the sound (a seek, a track, play or pause, the rack):
    /// a block now, and again once the engine has rebuilt its ring.
    function kick() {
        kicked = true;
        kickTimers.forEach(clearTimeout);
        kickTimers = [0, 300, 900].map((ms) => setTimeout(() => { kicked = true; tick(); }, ms));
    }

    /// The page's own status poll, seen on its way: another track, another
    /// state, or a position that jumped (a seek from another device).
    function onStatus(s) {
        if (!s) return;
        const t = now();
        const key = `${s.trackId ?? s.track ?? ''}|${s.state}`;
        const pos = s.positionS ?? 0;
        if (seen && (seen.key !== key || (s.state === 'playing' && Math.abs(pos - (seen.pos + (t - seen.t) / 1000)) > 1.2))) kick();
        seen = { key, pos, t, state: s.state };
    }

    return { handle, kick, onStatus, noteRtt, noteClock, offset: () => clockOffset(clocks), stats, spec, hp, tick };
}
