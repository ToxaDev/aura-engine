// eq/spectrum.js — the EQ window's analyzer: the spectrum before and after the
// EQ (`/player/eq_spectrum`), with ballistics, tilt, its own level range and a
// freeze (max-hold), drawn behind the EQ curve.
//
// The math (settings, decode, ballistics, tilt, the level scale) is pure and
// exported for the tests; createAnalyzer() holds the polling and the drawing.
// Polls only while on and visible; with nothing sounding the curves fall to
// the floor at the release speed, then the polling slows down.

export const MODES = ['pre', 'post', 'both'];
export const RES = ['low', 'medium', 'high', 'max'];
export const SPEEDS = { slow: 12, medium: 30, fast: 80 };   // release, dB/s
export const TILTS = [0, 3, 4.5, 6];                        // dB/oct, pivot 1 kHz
export const RANGES = [60, 90, 120];                        // dB below 0 dBFS
export const ATTACK_S = 0.008;                              // rise time constant
export const POINTS = 480;                                  // grid points, 10 Hz … 30 kHz
export const DEFAULTS = Object.freeze({ on: false, mode: 'both', res: 'medium', speed: 'medium', tilt: 4.5, range: 90, freeze: false });
const KEY = 'auraEqSpectrum';
const HEADER = 12;

/** Settings from whatever was stored: unknown or missing fields → defaults. */
export function normalizeSettings(o) {
    const s = { ...DEFAULTS };
    if (!o || typeof o !== 'object') return s;
    if (typeof o.on === 'boolean') s.on = o.on;
    if (MODES.includes(o.mode)) s.mode = o.mode;
    if (RES.includes(o.res)) s.res = o.res;
    if (o.speed in SPEEDS) s.speed = o.speed;
    if (TILTS.includes(+o.tilt)) s.tilt = +o.tilt;
    if (RANGES.includes(+o.range)) s.range = +o.range;
    if (typeof o.freeze === 'boolean') s.freeze = o.freeze;
    return s;
}

/** The reply's frame: null when empty or malformed (nothing sounding). */
export function decodeFrame(buf) {
    if (!buf || buf.byteLength < HEADER) return null;
    const dv = new DataView(buf);
    if (dv.getUint8(0) !== 1) return null;
    const n = dv.getUint16(2, true);
    if (buf.byteLength < HEADER + 8 * n) return null;
    const pre = new Float32Array(n), post = new Float32Array(n);
    for (let i = 0; i < n; i++) {
        pre[i] = dv.getFloat32(HEADER + 4 * i, true);
        post[i] = dv.getFloat32(HEADER + 4 * (n + i), true);
    }
    return { tapped: (dv.getUint8(1) & 1) === 1, n, rate: dv.getUint32(4, true), fft: dv.getUint32(8, true), pre, post };
}

/** The grid's band centres, as the engine's (`player::eq_spectrum::grid`). */
export function gridFreqs(n, fMin = 10, fMax = 30000) {
    const span = Math.log(fMax / fMin);
    return Float64Array.from({ length: n }, (_, i) => fMin * Math.exp(span * i / Math.max(1, n - 1)));
}

/** The tilt at `f`, dB: `tilt` dB/oct around 1 kHz. */
export const tiltDb = (f, tilt) => tilt * Math.log2(f / 1000);

/**
 * One step of the ballistics in dB, `dt` seconds after the last: a rise
 * follows at once (time constant ATTACK_S), a fall at `release` dB/s at
 * most; frozen, nothing falls. Frame-rate independent.
 */
export function ballistic(prev, target, dt, release, freeze = false) {
    if (target >= prev) return prev + (target - prev) * (1 - Math.exp(-dt / ATTACK_S));
    if (freeze) return prev;
    return Math.max(target, prev - release * dt);
}

/** `cur` (dB, in place) towards `target` (null: the floor); true while any is above it. */
export function stepCurve(cur, target, dt, release, floor, freeze = false) {
    let live = false;
    for (let i = 0; i < cur.length; i++) {
        const t = target ? Math.max(floor, target[i]) : floor;
        cur[i] = ballistic(cur[i], t, dt, release, freeze);
        if (cur[i] > floor + 0.01) live = true;
    }
    return live;
}

/** A level's place on the analyzer's own scale: 0 dB at `top`, −range at `bottom`. */
export function levelY(db, range, top, bottom) {
    const k = Math.min(1, Math.max(0, -db / range));
    return top + k * (bottom - top);
}

/** The plot's edges in CSS px from the graph's view: { x0, x1, y0, y1 }. */
export function plotEdges(v) {
    const p = v.plot || v;
    if ('x0' in p) return { x0: p.x0, x1: p.x1, y0: p.y0, y1: p.y1 };
    if ('w' in p || 'width' in p) {
        const x = p.x ?? p.l ?? p.left ?? 0, y = p.y ?? p.t ?? p.top ?? 0;
        return { x0: x, x1: x + (p.w ?? p.width), y0: y, y1: y + (p.h ?? p.height) };
    }
    return { x0: p.l ?? p.left, x1: p.r ?? p.right, y0: p.t ?? p.top, y1: p.b ?? p.bottom };
}

/**
 * The analyzer of one graph. `opts`: fetchFrame() → Promise<ArrayBuffer|null>
 * for (n, res); markDirty() asks the graph for a frame; now() in ms.
 */
export function createAnalyzer({ fetchFrame, markDirty, now = () => performance.now(), storage = globalThis.localStorage } = {}) {
    let s = DEFAULTS;
    try { s = normalizeSettings(JSON.parse(storage?.getItem(KEY) || 'null')); } catch (_) { /* defaults */ }
    const freqs = gridFreqs(POINTS);
    let tilt = new Float64Array(POINTS);
    const pre = new Float64Array(POINTS), post = new Float64Array(POINTS);
    let target = null;      // { pre, post } of the last frame, or null
    let last = 0;           // ms of the last ballistics step
    let live = false;       // something above the floor
    let visible = true, timer = 0, busy = false, gen = 0;

    const floor = () => -s.range - 1;
    function retilt() { tilt = Float64Array.from(freqs, f => tiltDb(f, s.tilt)); }
    function resetCurves() { pre.fill(floor()); post.fill(floor()); target = null; live = false; }
    retilt(); resetCurves();

    function save() { try { storage?.setItem(KEY, JSON.stringify(s)); } catch (_) { /* private mode */ } }

    function step() {
        const t = now();
        const dt = last ? Math.min(0.25, (t - last) / 1000) : 0;
        last = t;
        const rel = SPEEDS[s.speed], fl = floor();
        const tp = target && Float64Array.from(target.pre, (v, i) => v + tilt[i]);
        const tq = target && Float64Array.from(target.post, (v, i) => v + tilt[i]);
        const a = stepCurve(pre, tp, dt, rel, fl, s.freeze);
        const b = stepCurve(post, tq, dt, rel, fl, s.freeze);
        live = a || b;
    }

    async function poll() {
        timer = 0;
        if (!s.on || !visible || busy) return;
        busy = true;
        const g = gen;
        let fr = null;
        try { fr = decodeFrame(await fetchFrame(POINTS, RES.indexOf(s.res))); } catch (_) { fr = null; }
        busy = false;
        if (g !== gen || !s.on || !visible) return;
        target = fr && fr.n === POINTS ? fr : null;
        step();
        markDirty?.();
        // Nothing sounding and all fallen: ask less often.
        timer = setTimeout(poll, target || live ? 33 : 250);
    }

    function start() { if (!timer && s.on && visible) { last = 0; timer = setTimeout(poll, 0); } }
    function stop() { clearTimeout(timer); timer = 0; gen++; busy = false; }

    return {
        get settings() { return { ...s }; },
        /** Change one setting (persisted); the curves start afresh where needed. */
        set(key, value) {
            const was = s;
            s = normalizeSettings({ ...s, [key]: value });
            save();
            if (s.tilt !== was.tilt) retilt();
            if (s.range !== was.range || (was.freeze && !s.freeze) || s.res !== was.res) resetCurves();
            if (s.on) start(); else { stop(); resetCurves(); }
            markDirty?.();
        },
        /** The window shown or hidden: no polling while hidden. */
        setVisible(v) { visible = v; v ? start() : stop(); },
        start, stop,
        get on() { return s.on; },
        /**
         * Draw behind the curve (graph's setSpectrumDrawer): `view` has xOf
         * (frequency → x, follows the graph's zoom) and the plot's edges.
         */
        draw(ctx, view) {
            if (!s.on) return;
            const { xOf } = view, { x0, x1, y0, y1 } = plotEdges(view);
            const yOf = d => levelY(d, s.range, y0, y1);
            // Own scale: gray labels inside the right edge.
            ctx.save();
            ctx.font = '9px ui-monospace,"Cascadia Mono",Consolas,monospace';
            ctx.fillStyle = 'rgba(100,116,139,.55)';
            ctx.textAlign = 'right';
            ctx.textBaseline = 'middle';
            for (let d = -30; d > -s.range; d -= 30) ctx.fillText(String(d), x1 - 4, yOf(d));
            if (!live) { ctx.restore(); return; }
            const path = c => {
                ctx.beginPath();
                for (let i = 0; i < POINTS; i++) {
                    const x = xOf(freqs[i]);
                    if (x < x0 - 2 || x > x1 + 2) continue;
                    const y = yOf(c[i]);
                    ctx.lineTo(x, y);
                }
            };
            if (s.mode !== 'post') {
                path(pre);
                ctx.lineTo(x1, y1); ctx.lineTo(x0, y1); ctx.closePath();
                const fg = ctx.createLinearGradient(0, y0, 0, y1);
                fg.addColorStop(0, 'rgba(56,189,248,.20)');
                fg.addColorStop(1, 'rgba(56,189,248,.02)');
                ctx.fillStyle = fg;
                ctx.fill();
            }
            if (s.mode !== 'pre') {
                path(post);
                ctx.strokeStyle = s.freeze ? 'rgba(251,191,36,.75)' : 'rgba(186,230,253,.80)';
                ctx.lineWidth = 1.25;
                ctx.lineJoin = 'round';
                ctx.stroke();
            }
            ctx.restore();
        },
    };
}

/**
 * The toolbar's analyzer controls: the Off/On segment `seg`, the settings
 * button `btn` and its menu `menu` (rows of `.modes[data-k]`).
 */
export function mountAnalyzerControls(an, { seg, btn, menu, onToggle } = {}) {
    const mark = () => {
        const s = an.settings;
        seg?.querySelectorAll('button').forEach(b => b.classList.toggle('on', b.dataset.v === (s.on ? 'on' : 'off')));
        menu?.querySelectorAll('.modes[data-k]').forEach(m => m.querySelectorAll('button')
            .forEach(b => b.classList.toggle('on', b.dataset.v === String(s[m.dataset.k]))));
    };
    seg?.addEventListener('click', e => {
        const b = e.target.closest('button'); if (!b) return;
        an.set('on', b.dataset.v === 'on'); mark(); onToggle?.(an.on);
    });
    menu?.addEventListener('click', e => {
        const b = e.target.closest('button'), m = b?.closest('.modes[data-k]'); if (!m) return;
        const k = m.dataset.k, v = b.dataset.v;
        an.set(k, k === 'freeze' ? v === 'true' : k === 'tilt' || k === 'range' ? +v : v);
        if (!an.on) { an.set('on', true); onToggle?.(true); }
        mark();
    });
    const close = () => { if (menu) menu.hidden = true; btn?.setAttribute('aria-expanded', 'false'); };
    btn?.addEventListener('click', e => {
        e.stopPropagation();
        if (!menu.hidden) return close();
        const r = btn.getBoundingClientRect();
        menu.hidden = false;
        const w = menu.offsetWidth;
        menu.style.left = Math.max(8, Math.min(innerWidth - w - 8, r.left)) + 'px';
        menu.style.top = (r.bottom + 6) + 'px';
        btn.setAttribute('aria-expanded', 'true');
    });
    document.addEventListener('pointerdown', e => { if (menu && !menu.hidden && !menu.contains(e.target) && e.target !== btn && !btn?.contains(e.target)) close(); });
    document.addEventListener('keydown', e => { if (e.key === 'Escape') close(); });
    mark();
}
