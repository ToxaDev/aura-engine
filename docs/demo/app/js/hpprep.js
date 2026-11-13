// Hybrid-Phase preparing: while the engine analyses a track before it plays
// (/player/hp_prepare), soft wave strands cross the Hybrid-Phase rack unit
// as if under glass, reaching across as far as the work is done, the percent
// in its middle — the view and the controls fade out meanwhile; at 100 % they flatten and fade and the
// sound starts (the engine starts it, not this file).
//
// The model is pure (time comes in, numbers go out) so the tests drive it
// without a page; the poller asks the engine ~10×/s only while a track is
// being prepared, or for a short while after a start until the first answer.

export const POLL_MS = 100;
/// After a start, how long to ask before giving up on hearing «preparing».
export const ARM_MS = 2500;
/// Shown progress chases the engine's with this time constant (s), and
/// faster once it is ready.
const TAU = 0.7, TAU_READY = 0.22;
/// While the engine's number stands still the fill still creeps, at most this
/// far ahead of it and never past CREEP_CAP, so a stall never looks stuck.
const CREEP = 0.06, CREEP_RATE = 0.015, CREEP_CAP = 0.96;
export const DISSOLVE_MS = 900, FADE_MS = 450;
/// The strands come in over this long once the engine says it prepares,
/// while the view and the controls go out (dsprack.css, `.hpu.prep`).
export const SHOW_MS = 350;
/// Answers missed in a row (no endpoint, errors) before a fill gives up.
const MISS_LIMIT = 30;

const clamp01 = v => (Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0);
const smooth = k => { const x = clamp01(k); return x * x * (3 - 2 * x); };

/// The state machine. Phases: off → armed (asked, nothing heard yet) →
/// filling → finishing (ready: fill to the top) → dissolving → off; any
/// shown phase → fading on failed/skipped/idle.
export function createPrepModel() {
    let phase = 'off', track = null;
    let target = 0, shown = 0, creep = 0, misses = 0;
    let armedUntil = 0, phaseAt = 0, lastT = null, since = 0;
    // armedFor: the track a start asks about (null: not known yet); done: the
    // track the engine said it has nothing to prepare for; lit: the track
    // Hybrid-Phase prepared, its sound not heard yet.
    let armedFor = null, done = null, lit = null;
    // via: what the fill shows — 'lists' (the start's, /player/hp_prepare)
    // or 'switch' (the pair's own work on the air, the status's arming);
    // split: the lists' share of a fill the pair's work follows (null: all);
    // base and f0: where the switch part starts, on the fill and on the
    // arming; follows: the arming says the pair's work comes after the lists.
    let via = 'lists', split = null, base = 0, f0 = null, follows = false, eta = null;
    const go = (p, t) => { phase = p; phaseAt = t; if (p === 'off') reset(); };
    const reset = () => { via = 'lists'; split = null; base = 0; f0 = null; follows = false; eta = null; };
    const showing = () => phase === 'filling' || phase === 'finishing';
    const arm = (t, id) => {
        armedUntil = t + ARM_MS;
        armedFor = id;
        if (phase === 'off') go('armed', t);
    };
    return {
        get phase() { return phase; },
        get shown() { return shown; },
        get track() { return track; },
        /// What the fill shows: 'lists' or 'switch'.
        get via() { return via; },
        /// A track starts (or may): ask the engine for a while about track
        /// `id` (null: not known yet). A fill on screen goes on; one fading
        /// out is asked about all the same.
        arm(t, id = null) { done = null; if (via === 'switch' || !showing()) reset(); arm(t, id); },
        /// The player still makes track `id` ready before its sound: go on
        /// asking — the engine's own preparing may come later than the first
        /// answers, and a start used to stop asking for good on one of them.
        /// True if it starts asking again.
        keep(t, id) {
            if (id == null || id === done || (phase !== 'off' && phase !== 'armed')) return false;
            const was = phase;
            arm(t, id);
            return was === 'off';
        },
        /// The engine's answer; null = no answer (error, no endpoint).
        answer(a, t) {
            // The pair's work on screen: the lists have nothing more to say.
            if (via === 'switch') return;
            if (!a || typeof a.state !== 'string') {
                if (showing() && ++misses >= MISS_LIMIT) go('fading', t);
                return;
            }
            misses = 0;
            const st = a.state, about = a.track ?? null;
            // While a start asks, an answer about another track (the one
            // before it, or idle: none yet) says nothing about this one.
            const own = armedFor == null || about === armedFor;
            if (st === 'preparing') {
                if (phase === 'armed' && !own) return;
                const fresh = !showing() || about !== track;
                if (fresh) {
                    // A fill already on screen goes on as it is; a new one comes in.
                    if (!showing()) since = t;
                    target = 0; shown = 0; creep = 0; track = about; lastT = t; go('filling', t);
                    split = null; follows = false;
                }
                if (Number.isFinite(a.eta_s)) eta = a.eta_s;
                const p = clamp01(a.progress) * (split ?? 1);
                if (p > target) { target = p; creep = 0; }
                return;
            }
            // Nothing to prepare: only an answer about the very track asked for
            // ends the asking (▶ before the track is known waits for it). Its
            // lists already made (kept on disk, or made between two answers),
            // Hybrid-Phase is lit until its sound is heard all the same: the
            // quick variant plays first without it, ~5 s at 1G (Anton 31.10).
            if (phase === 'armed') {
                if (armedFor != null && about === armedFor) {
                    done = about;
                    if (st === 'skipped' || st === 'ready') lit = about;
                    go('off', t);
                }
                return;
            }
            if (st === 'ready') {
                // The pair's own work comes next (a start at 1G: the quick
                // variant plays first without it): the same fill goes on.
                if (phase === 'filling' && follows) {
                    via = 'switch'; base = Math.max(target, split ?? 0); target = base; f0 = null; creep = 0;
                    return;
                }
                if (showing()) { lit = track; if (phase !== 'finishing') go('finishing', t); }
                return;
            }
            // skipped, idle, failed or anything new: nothing to wait for.
            if (showing()) { lit = null; go('fading', t); }
        },
        /// The arming of track `id` (the status's, arming.rs; null: none or
        /// not playing): while Hybrid-Phase's own work is on its way
        /// (`hpAhead`) the fill shows it — after a start's lists as the same
        /// fill, or on its own: a rack change plays the new chain without HP
        /// while its pair is made (Anton 1.11: 30M → 1G, the unit empty ~6 s).
        /// `fresh`: no Hybrid-Phase heard on the track yet. Its share comes
        /// from the arming's fraction, from where it was when the part began:
        /// never back, never full before the pair is heard.
        switching(t, id, a, fresh = false) {
            const ahead = id != null && !!a?.busy && !!a?.hpAhead;
            if (via === 'switch') {
                if (!showing()) return;
                if (ahead && id === track) {
                    const frac = clamp01(a.frac);
                    f0 ??= frac;
                    const q = f0 >= 1 ? 0 : clamp01((frac - f0) / (1 - f0));
                    const p = base + (1 - base) * q;
                    if (p > target) { target = p; creep = 0; }
                    return;
                }
                // Done: the pair is heard already, nothing left to light.
                const heard = id === track && !!a && !a.busy && a.step === 'done';
                lit = null;
                go(heard ? 'finishing' : 'fading', t);
                return;
            }
            if (phase === 'filling' && track === id) {
                // The lists' fill: the pair's work after it takes its share,
                // by the seconds each is expected to take.
                follows = ahead;
                if (ahead && split == null && Number.isFinite(eta) && Number.isFinite(a.leftS)) {
                    split = Math.min(0.9, Math.max(0.2, eta / Math.max(1e-3, eta + a.leftS)));
                }
                return;
            }
            if (phase !== 'off' || !ahead || !(fresh || String(a.step).startsWith('hp:'))) return;
            reset();
            via = 'switch'; since = t; target = 0; shown = 0; creep = 0; track = id; lastT = t;
            done = null; lit = null; go('filling', t);
            this.switching(t, id, a);
        },
        /// What the player hears: once Hybrid-Phase's own sound is on the
        /// air for the prepared track (or it is held off it, or another
        /// track or nothing plays, or the full chain plays without it — it
        /// is not coming with this sound), the prepare no longer lights it.
        heard(id, hp, stopped, settled = false) {
            if (lit != null && (stopped || hp || settled || id !== lit)) lit = null;
        },
        /// Hybrid-Phase works on track `id` or has done so for its coming
        /// sound: lit from its prepare until it is heard — the quick variant
        /// plays first without it, and its unit went dark between the end of
        /// the animation and its sound (Anton 31.10).
        litFor(id) {
            const working = phase === 'filling' || phase === 'finishing' || phase === 'dissolving';
            return (working && (track == null || track === id)) || (lit != null && lit === id);
        },
        /// Ask the engine now?
        wantPoll(t) {
            if (phase === 'armed' && t > armedUntil) go('off', t);
            if (via === 'switch') return false;
            return phase === 'armed' || phase === 'filling'
                || ((phase === 'dissolving' || phase === 'fading') && t <= armedUntil);
        },
        /// Hybrid-Phase at work on the track (or just done, its sound on the
        /// way): it is on, so its badge is lit, not switching in (Anton 30.10).
        working() { return phase === 'filling' || phase === 'finishing' || phase === 'dissolving'; },
        /// Anything left to draw (the loop runs while true).
        live() { return phase !== 'off' && phase !== 'armed'; },
        /// One frame: what to draw. fill 0..1, veil (backdrop) and alpha
        /// (strands) 0..1, rise 0..1 while dissolving; hide: the view and
        /// the controls go out while the work runs, and come back from the
        /// moment it is done.
        frame(t) {
            const dt = lastT == null ? 0 : Math.min(0.1, Math.max(0, (t - lastT) / 1000));
            lastT = t;
            if (phase === 'filling') {
                creep = Math.min(CREEP, creep + CREEP_RATE * dt);
                const goal = Math.max(target, Math.min(CREEP_CAP, target + creep));
                shown = Math.max(shown, shown + (goal - shown) * (1 - Math.exp(-dt / TAU)));
            } else if (phase === 'finishing') {
                shown = Math.max(shown, shown + (1 - shown) * (1 - Math.exp(-dt / TAU_READY)));
                if (shown >= 0.995) { shown = 1; go('dissolving', t); }
            }
            if (phase === 'filling' || phase === 'finishing') {
                const k = smooth((t - since) / SHOW_MS);
                return { draw: true, hide: true, fill: shown, veil: k, alpha: k, rise: 0 };
            }
            if (phase === 'dissolving') {
                const k = (t - phaseAt) / DISSOLVE_MS;
                if (k >= 1) { go('off', t); return { draw: false }; }
                const was = smooth((phaseAt - since) / SHOW_MS);
                return { draw: true, fill: 1, veil: was * (1 - smooth(k)), alpha: was * (1 - smooth(k * 1.15)), rise: smooth(k) };
            }
            if (phase === 'fading') {
                const k = (t - phaseAt) / FADE_MS;
                if (k >= 1) { go('off', t); return { draw: false }; }
                // From as far as it had come in: a fill that ends at once never flashes up.
                const was = smooth((phaseAt - since) / SHOW_MS);
                return { draw: true, fill: shown, veil: was * (1 - smooth(k)), alpha: was * (1 - smooth(k)), rise: 0 };
            }
            return { draw: false };
        },
    };
}

/// Arms the model when the player's status says a track starts: another
/// track, or a move into preparing/playing; keeps it asking while the track
/// is made ready; tells it what is heard. True when it should ask now.
export function startWatch(model) {
    let id = null, st = 'stopped', heardOn = null;
    return (s, t) => {
        const nid = s?.trackId ?? null, nst = s?.state || 'stopped';
        const starts = nid != null && nst !== 'stopped' && nst !== 'paused'
            && (nid !== id || (nst !== st && st !== 'playing' && st !== 'preparing'));
        id = nid; st = nst;
        // The chain on the air speaks for this track only once its jump has
        // reached the output: until then it may be the track before's.
        const aud = nst === 'playing' && !s?.jump?.pending ? s?.audible : null;
        const hp = !!aud?.hpHeld || !!aud?.stages?.some(x => x[0] === 'HP' || x[0] === 'αHP');
        const settled = !!aud && !aud.quick && !aud.hpDeferred;
        model.heard(nid, hp, nst === 'stopped', settled);
        if (hp) heardOn = nid;
        if (starts) model.arm(t, nid);
        const on = nst === 'playing' || nst === 'preparing';
        model.switching(t, nid, on ? s?.arming ?? null : null, nid !== heardOn);
        if (starts) return true;
        return nst === 'preparing' && model.keep(t, nid);
    };
}

/// Polls while the model wants it; never two requests at once.
export function createPoller(model, { fetchJson, now, setTimer, clearTimer, onAnswer }) {
    let timer = 0, busy = false;
    const schedule = () => { if (!timer && !busy && model.wantPoll(now())) timer = setTimer(tick, POLL_MS); };
    async function tick() {
        timer = 0;
        if (busy || !model.wantPoll(now())) return;
        busy = true;
        let a = null;
        try { a = await fetchJson(); } catch (_) { a = null; }
        busy = false;
        model.answer(a, now());
        onAnswer?.();
        schedule();
    }
    return {
        /// Ask at once (a start), then on schedule.
        kick() { if (timer) { clearTimer(timer); timer = 0; } if (!busy) tick(); },
        get busy() { return busy; },
        get pending() { return !!timer; },
    };
}

// ── drawing ───────────────────────────────────────────────────────────

/// The strands, in HP's violets: colour (rgb), height (of the unit, from its
/// middle), amplitude (of the height), waves across, speed (rad/s), phase,
/// opacity. Two run right, two left, so they cross and part.
const STRANDS = [
    { c: '167,139,250', dy: -0.06, amp: 0.26, k: 1.10, w: 0.70,  ph: 0.0, a: 0.30 },
    { c: '129,140,248', dy: 0.04,  amp: 0.22, k: 1.55, w: -0.55, ph: 1.9, a: 0.24 },
    { c: '192,132,252', dy: 0.10,  amp: 0.18, k: 0.80, w: 0.42,  ph: 3.3, a: 0.20 },
    { c: '125,211,252', dy: -0.12, amp: 0.14, k: 2.10, w: -0.90, ph: 4.6, a: 0.14 },
];

/// A strand's wave (k = 1) is this many unit heights long.
const WAVE_H = 10;

/// Paints one frame `f` (from model.frame) on the HP unit's canvas context,
/// `w`×`h` device pixels: the strands reach as far across as the work is
/// done (a soft front, faint ahead of it), under a glass sheen. `t` in
/// seconds moves them; `calm` (reduced motion) holds them still. While the
/// view dissolves the strands flatten as they fade.
export function paint(ctx, w, h, f, t, calm = false) {
    ctx.clearRect(0, 0, w, h);
    if (!f.draw) return;
    ctx.save();
    if (f.alpha > 0) {
        ctx.globalCompositeOperation = 'lighter';
        const flat = 1 - 0.7 * (f.rise || 0);
        const step = Math.max(2, w / 96);
        for (const k of STRANDS) {
            const y0 = h * (0.5 + k.dy), amp = k.amp * h * flat;
            const u0 = (calm ? 0 : t) * k.w + k.ph;
            ctx.beginPath();
            for (let x = 0; x <= w + step; x += step) {
                // Wavelengths in unit heights: the same waves on a narrow
                // rack and a wide one.
                const u = (x / (h * WAVE_H)) * Math.PI * 2 * k.k;
                const y = y0 + amp * Math.sin(u + u0) + 0.3 * amp * Math.sin(u * 1.8 - u0 * 0.6);
                if (x) ctx.lineTo(x, y); else ctx.moveTo(x, y);
            }
            // A wide dim body and a thin core: light seen through glass.
            ctx.strokeStyle = `rgba(${k.c},${k.a * 0.35 * f.alpha})`;
            ctx.lineWidth = h * 0.22;
            ctx.stroke();
            ctx.strokeStyle = `rgba(${k.c},${k.a * f.alpha})`;
            ctx.lineWidth = Math.max(1, h / 34);
            ctx.stroke();
        }
        // How far it has come: full up to the front, faint ahead of it.
        const x = clamp01(f.fill) * w, soft = w * 0.14;
        const g = ctx.createLinearGradient(0, 0, w, 0);
        g.addColorStop(0, 'rgba(0,0,0,1)');
        g.addColorStop(Math.max(0, (x - soft) / w), 'rgba(0,0,0,1)');
        g.addColorStop(x / w, 'rgba(0,0,0,0.16)');
        g.addColorStop(1, 'rgba(0,0,0,0.10)');
        ctx.globalCompositeOperation = 'destination-in';
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, w, h);
    }
    if (f.veil > 0) {
        // The glass: a faint sheen on its upper half, darker at the ends.
        ctx.globalCompositeOperation = 'source-over';
        const s = ctx.createLinearGradient(0, 0, 0, h);
        s.addColorStop(0, `rgba(237,233,254,${0.07 * f.veil})`);
        s.addColorStop(0.45, `rgba(237,233,254,${0.015 * f.veil})`);
        s.addColorStop(0.5, 'rgba(237,233,254,0)');
        s.addColorStop(1, `rgba(2,6,23,${0.18 * f.veil})`);
        ctx.fillStyle = s;
        ctx.fillRect(0, 0, w, h);
        const e = ctx.createLinearGradient(0, 0, w, 0);
        e.addColorStop(0, `rgba(2,6,23,${0.22 * f.veil})`);
        e.addColorStop(0.12, 'rgba(2,6,23,0)');
        e.addColorStop(0.88, 'rgba(2,6,23,0)');
        e.addColorStop(1, `rgba(2,6,23,${0.22 * f.veil})`);
        ctx.fillStyle = e;
        ctx.fillRect(0, 0, w, h);
    }
    ctx.restore();
}

/// The readout in the middle of the unit: whole percent, 0..100.
export const pctText = fill => String(Math.round(clamp01(fill) * 100));

// ── the page ──────────────────────────────────────────────────────────

/// A stand-in for the engine (`?hpprepMock=1` runs 0→1 in ~8 s with a
/// stall; a number such as `0.5` holds there; `fail` gives up half-way).
function mockSource(mode, now) {
    let t0 = null, released = false;
    const hold = Number(mode);
    const curve = s => (s < 1 ? 0.15 * s : s < 2.6 ? 0.15 : s < 6 ? 0.15 + 0.65 * Math.floor((s - 2.6) * 3) / 10.2 : 0.8 + 0.2 * (s - 6) / 2);
    return {
        start() { t0 = now(); released = false; },
        release() { released = true; },
        async fetchJson() {
            if (t0 == null) return { state: 'idle', progress: 0, stage: '', track: null, eta_s: null };
            const s = (now() - t0) / 1000;
            if (mode === 'fail' && s > 3) return { state: 'failed', progress: 0.4, stage: 'stems', track: 1, eta_s: null };
            if (mode !== '1' && mode !== 'fail' && Number.isFinite(hold) && !released) {
                return { state: 'preparing', progress: hold, stage: 'stems', track: 1, eta_s: 5 };
            }
            const p = released ? 1 : curve(s);
            if (p >= 1) return { state: 'ready', progress: 1, stage: '', track: 1, eta_s: 0 };
            return { state: 'preparing', progress: p, stage: p < 0.15 ? 'decode' : p < 0.8 ? 'stems' : 'kit', track: 1, eta_s: 8 - s };
        },
    };
}

let api = null;

/// Sets up the drawing on Hybrid-Phase's rack unit (dsprack.js: its
/// canvas.hpprep and span.pp, looked up each frame — the rack rebuilds
/// them). `url` is the endpoint's full address; `hidden()` is called once
/// the view has faded out under the strands (player.js drops the held
/// picture of the track before, so the new one is what fades back in).
export function initHpPrep(url, { hidden = () => {} } = {}) {
    if (api) return api;
    const now = () => performance.now();
    const model = createPrepModel();
    const mode = new URLSearchParams(location.search).get('hpprepMock');
    const mock = mode ? mockSource(mode, now) : null;
    const fetchJson = mock ? mock.fetchJson
        : async () => { const r = await fetch(url, { cache: 'no-store' }); return r.ok ? r.json() : null; };
    const calm = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
    let running = false, restTimer = 0, manual = false, gone = false;

    /// Draws `f` on the unit; true if it is on screen.
    const show = (f, ts) => {
        const unit = document.querySelector('.lab-badge.hpu');
        const cv = unit?.querySelector('canvas.hpprep'), pp = unit?.querySelector('.pp');
        if (!cv) return false;
        unit.classList.toggle('prep', !!f.hide);
        if (!f.hide) gone = false;
        else if (!gone && f.alpha >= 1) { gone = true; hidden(); }
        if (pp) {
            const pct = f.draw ? pctText(f.fill) : '';
            if (pp.dataset.v !== pct) { pp.dataset.v = pct; pp.innerHTML = pct ? `<b>${pct}</b><i>%</i>` : ''; }
            pp.style.opacity = f.draw ? String(Math.min(f.alpha, f.veil)) : '';
        }
        if (!f.draw) { cv.getContext('2d').clearRect(0, 0, cv.width, cv.height); return false; }
        if (unit.offsetParent === null) return false;
        const dpr = window.devicePixelRatio || 1;
        const w = Math.max(8, Math.round(cv.offsetWidth * dpr)), h = Math.max(8, Math.round(cv.offsetHeight * dpr));
        if (cv.width !== w || cv.height !== h) { cv.width = w; cv.height = h; }
        paint(cv.getContext('2d'), w, h, f, ts / 1000, calm);
        return true;
    };
    const loop = ts => {
        running = false;
        const f = model.frame(ts);
        const on = show(f, ts);
        if (!f.draw) return;
        if (!on) { restTimer = setTimeout(() => { restTimer = 0; wake(); }, 250); return; }
        running = true;
        requestAnimationFrame(auto);
    };
    const auto = ts => { if (!manual) loop(ts); };
    const wake = () => { if (!running && !restTimer && model.live()) { running = true; requestAnimationFrame(auto); } };
    const poller = createPoller(model, {
        fetchJson, now, setTimer: setTimeout, clearTimer: clearTimeout, onAnswer: wake,
    });
    const watch = startWatch(model);
    api = {
        /// A start from this window (▶, a row's ▶): ask at once.
        arm() { model.arm(now()); mock?.start(); poller.kick(); },
        /// Every player status: arms on a track start.
        status(s) { if (watch(s, now())) { mock?.start(); poller.kick(); } },
        /// Track `id` is being prepared by Hybrid-Phase, or was and its
        /// sound is not heard yet (model.litFor).
        preparing(id) { return model.litFor(id); },
    };
    // The mock's handle; step(ms) draws frames by hand from then on (a hidden
    // page gets none, and screenshots want a fixed moment).
    if (mock) window.__hpprep = { ...api, model, release: () => mock.release(), step: ms => { manual = true; loop(ms); } };
    return api;
}

export const hpPrepArm = () => api?.arm();
export const hpPrepStatus = s => api?.status(s);
export const hpPreparing = id => !!api?.preparing(id);
