// eq/graph.js — EQ curve canvas with pointer editing.
//
// Performance contract:
//   Redraws only when dirty, coalesced through requestAnimationFrame.
//   Per-band curve cache (Float64Array): recomputed only when that band, the
//   canvas width or the frequency view changes.
//   Curves are sampled once per device pixel of the plot over the current view.
//   DPR-aware: canvas pixels scale with devicePixelRatio.
//   60 fps drag on 4K; spectrum drawn only while specMode is 'on'.
//
// Layers, back to front: grid → spectrum → band fills → curve → preview →
// nodes → readouts. The spectrum layer is one call (setSpectrumDrawer).

import { bandCurve, logGrid, F_MIN, F_MAX, hasGain, isCut, SLOPES } from './math.js';
import {
    F_LO, F_HI, G_MAX, Q_LO, Q_HI, typeAt, shelfAt, hitPoint, inRect, idRange, groupMove,
    expandRange, gridStep, zoomView, panView, slopeStep,
} from './controls.js';

/// The fixed grid other views (import preview) draw on.
export const NPT = 420;
export const FREQS = logGrid(NPT, F_MIN, F_MAX);
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));

export const fmtF = f => f >= 1000 ? (f / 1000).toFixed(f >= 10000 ? 1 : 2).replace(/\.?0+$/, '') + ' kHz' : (f < 100 ? f.toFixed(1).replace(/\.0$/, '') : Math.round(f)) + ' Hz';
export const fmtDb = g => (g > 0 ? '+' : g < 0 ? '−' : '') + Math.abs(g).toFixed(1) + ' dB';

// Padding inside the canvas (CSS px); the bottom strip is the frequency axis.
const P = { l: 6, r: 40, t: 16, b: 22 };
const MOVE_PX = 3;          // a press that moves less is a click
const MONO = 'ui-monospace,"Cascadia Mono",Consolas,monospace';

/**
 * Attach an EQ graph to `canvas`.
 *
 * Callbacks (all optional):
 *   onSelect(ids, primary)     — the selection changed (ids: array, primary: id|null)
 *   onEditStart()              — first move of a drag (push undo here)
 *   onEdit(bands, final)       — bands changed; final=true on release
 *   onWheelEdit(band, isNew)   — wheel changed Q/gain/slope; isNew=true after a 700 ms gap
 *   onAdd(type, f, gain)       — add a band; returns the new band object (or null)
 *   onToggle(bands)            — Alt-click: bypass these bands
 *   onOpenPanel(band)          — double-click on a node: type its values
 *   onTypeMenu(e, band|null, f, gain) — right-click
 *   onRange(r)                 — the dB scale grew while dragging
 *   onDrag(active)             — a node drag started / ended
 *   onDrawn()                  — after every frame (place overlays with nodePos)
 *
 * Returns an object with setters (all call markDirty) and a recalcBand helper.
 */
export function createGraph(canvas, cbs = {}) {
    // Internal state — band objects carry {id, type, f, gain, q, slope, on, color, curve}
    let bands = [];
    let sel = new Set();
    let primary = null;
    let range = 12;
    let eqOn = false;
    let specMode = 'off';
    let specData = null;    // Float64Array, log-spaced dB from /player/spectrum
    let specDrawer = null;  // fn(ctx, view) — replaces the built-in spectrum layer
    let drawFs = 96000;     // fs used for band curves (updated by eq:engine)
    let view = { lo: F_MIN, hi: F_MAX };

    // Sampling grid: one point per device pixel of the plot over the view
    let freqs = FREQS;
    let gridKey = '';
    let gridVer = 0;

    // Cached layout
    let G = { w: 0, h: 0 };
    let dpr = 1;

    // rAF coalescing
    let dirty = false;
    let rafId = 0;

    // Pointer state
    let drag = null;        // node drag
    let axis = null;        // frequency axis drag
    let rect = null;        // rubber band { a, z, add, active }
    let hover = null;       // { x, y } over the empty plot
    let preview = null;     // the band a click would add { type, f, gain, q, slope, curve }
    let lastWheel = 0;
    let wheelAcc = 0;
    let lastAdd = 0;

    // ── coordinate helpers ──────────────────────────────────────────────────
    const plot = () => ({ l: P.l, r: G.w - P.r, t: P.t, b: G.h - P.b });
    const LV = () => Math.log(view.hi / view.lo);
    const xOf = f => P.l + Math.log(f / view.lo) / LV() * (G.w - P.l - P.r);
    const fOf = x => view.lo * Math.exp((x - P.l) / (G.w - P.l - P.r) * LV());
    const yOf = d => P.t + (range - d) / (2 * range) * (G.h - P.t - P.b);
    const dOf = y => range - (y - P.t) / (G.h - P.t - P.b) * 2 * range;
    const nodeY = b => yOf(hasGain(b.type) ? clamp(b.gain, -range, range) : 0);
    const inView = f => f >= view.lo * 0.999 && f <= view.hi * 1.001;
    const points = () => bands.filter(b => inView(b.f)).map(b => ({ id: b.id, x: xOf(b.f), y: nodeY(b), b }));
    const byId = id => bands.find(b => b.id === id);

    // ── band curve cache ────────────────────────────────────────────────────
    function updateGrid() {
        const n = clamp(Math.round((G.w - P.l - P.r) * dpr), 64, 2048);
        const key = n + ':' + view.lo + ':' + view.hi;
        if (key === gridKey) return;
        gridKey = key;
        freqs = logGrid(n, view.lo, view.hi);
        gridVer++;
    }
    function recalcBand(b) {
        b.curve = bandCurve(b, freqs, drawFs);
        b._gv = gridVer;
    }
    const curveOf = b => { if (b._gv !== gridVer || !b.curve || b.curve.length !== freqs.length) recalcBand(b); return b.curve; };

    function total() {
        const t = new Float64Array(freqs.length);
        for (const b of bands) {
            if (!b.on) continue;
            const c = curveOf(b);
            for (let i = 0; i < t.length; i++) t[i] += c[i];
        }
        return t;
    }
    // The summed curve at x (for grabbing it)
    function totalAt(x) {
        const i = clamp(Math.round((x - P.l) / (G.w - P.l - P.r) * (freqs.length - 1)), 0, freqs.length - 1);
        let d = 0;
        for (const b of bands) if (b.on) d += curveOf(b)[i];
        return d;
    }

    // ── rAF loop ────────────────────────────────────────────────────────────
    function markDirty() {
        dirty = true;
        if (!rafId) rafId = requestAnimationFrame(frame);
    }
    function frame() {
        rafId = 0;
        if (dirty) { draw(); dirty = false; }
    }

    // ── canvas resize ───────────────────────────────────────────────────────
    function updateSize() {
        const r = canvas.getBoundingClientRect();
        dpr = window.devicePixelRatio || 1;
        const W = Math.max(1, Math.round(r.width * dpr));
        const H = Math.max(1, Math.round(r.height * dpr));
        if (canvas.width !== W || canvas.height !== H) { canvas.width = W; canvas.height = H; }
        G = { w: r.width, h: r.height };
        updateGrid();
    }

    // ── draw ────────────────────────────────────────────────────────────────
    function tracePath(ctx, c, lim) {
        for (let i = 0; i < freqs.length; i++) {
            const x = P.l + i / (freqs.length - 1) * (G.w - P.l - P.r);
            const y = yOf(clamp(c[i], -lim, lim));
            i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
        }
    }

    function drawGrid(ctx, w, h) {
        ctx.lineWidth = 1;
        const pxDec = (G.w - P.l - P.r) / (LV() / Math.LN10);
        const labels = [];
        for (let dec = 1; dec <= 10000; dec *= 10) {
            for (let m = 1; m < 10; m++) {
                const f = dec * m;
                if (!inView(f)) continue;
                const x = Math.round(xOf(f)) + 0.5;
                ctx.strokeStyle = m === 1 ? 'rgba(255,255,255,.12)' : 'rgba(255,255,255,.045)';
                ctx.beginPath(); ctx.moveTo(x, P.t); ctx.lineTo(x, h - P.b); ctx.stroke();
                if (m === 1 || m === 2 || m === 5 || pxDec > 420) labels.push(f);
            }
        }
        const step = gridStep(range);
        for (let d = -range; d <= range; d += step) {
            const y = Math.round(yOf(d)) + 0.5;
            ctx.strokeStyle = d === 0 ? 'rgba(255,255,255,.22)' : 'rgba(255,255,255,.06)';
            ctx.beginPath(); ctx.moveTo(P.l, y); ctx.lineTo(w - P.r, y); ctx.stroke();
        }
        // dB labels (right axis)
        ctx.font = '10px ' + MONO;
        ctx.fillStyle = 'rgba(148,163,184,.75)';
        ctx.textBaseline = 'middle';
        ctx.textAlign = 'left';
        for (let d = -range; d <= range; d += step) {
            ctx.fillText((d > 0 ? '+' : '') + d, w - P.r + 6, clamp(yOf(d), P.t + 5, h - P.b - 5));
        }
        // Frequency labels (bottom axis), thinned to fit
        ctx.textAlign = 'center';
        ctx.textBaseline = 'alphabetic';
        let lastX = -1e9;
        for (const f of labels) {
            const x = xOf(f);
            if (x - lastX < 30 || x < P.l + 8 || x > w - P.r - 4) continue;
            ctx.fillText(f >= 1000 ? f / 1000 + 'k' : String(f), x, h - 7);
            lastX = x;
        }
        if (view.lo > F_MIN * 1.001 || view.hi < F_MAX * 0.999) {
            // A zoomed view: a thin bar shows which part of 10 Hz–30 kHz is on screen
            const L = Math.log(F_MAX / F_MIN), x0 = P.l + Math.log(view.lo / F_MIN) / L * (w - P.l - P.r);
            const x1 = P.l + Math.log(view.hi / F_MIN) / L * (w - P.l - P.r);
            ctx.fillStyle = 'rgba(56,189,248,.35)';
            ctx.fillRect(x0, h - 2, Math.max(2, x1 - x0), 2);
        }
    }

    // Built-in spectrum layer (until a drawer is set)
    function drawSpectrum(ctx, v) {
        if (specDrawer) { specDrawer(ctx, v); return; }
        if (specMode !== 'on' || !specData || !specData.length) return;
        const n = specData.length, L = Math.log(F_MAX / F_MIN);
        const sY = d => P.t + clamp(-d / 100, 0, 1) * (G.h - P.t - P.b);
        ctx.beginPath();
        ctx.moveTo(xOf(F_MIN), G.h - P.b);
        for (let i = 0; i < n; i++) ctx.lineTo(xOf(F_MIN * Math.exp(L * i / (n - 1))), sY(specData[i]));
        ctx.lineTo(xOf(F_MAX), G.h - P.b);
        ctx.closePath();
        const sg = ctx.createLinearGradient(0, P.t, 0, G.h);
        sg.addColorStop(0, 'rgba(56,189,248,.30)');
        sg.addColorStop(1, 'rgba(56,189,248,.03)');
        ctx.fillStyle = sg;
        ctx.fill();
        ctx.strokeStyle = 'rgba(125,211,252,.35)';
        ctx.lineWidth = 1;
        ctx.stroke();
    }

    function viewInfo() {
        return { xOf, fOf, yOf, dOf, plot: plot(), w: G.w, h: G.h, lo: view.lo, hi: view.hi, range, dpr };
    }

    function drawBand(ctx, b, lim) {
        const isSel = sel.has(b.id), c = curveOf(b);
        if (!b.on && !isSel) return;
        const y0 = yOf(0);
        // Soft fill between the band and 0 dB: every band faint, the selected stronger
        ctx.beginPath();
        tracePath(ctx, c, lim);
        ctx.lineTo(G.w - P.r, y0);
        ctx.lineTo(P.l, y0);
        ctx.closePath();
        ctx.fillStyle = b.color + (isSel ? (b.id === primary ? '33' : '24') : (b.on ? '10' : '08'));
        ctx.fill();
        ctx.beginPath();
        tracePath(ctx, c, lim);
        ctx.strokeStyle = b.color + (isSel ? 'cc' : '50');
        ctx.lineWidth = isSel ? 1.4 : 1;
        ctx.setLineDash(b.on ? [] : [4, 3]);
        ctx.stroke();
        ctx.setLineDash([]);
    }

    function drawPreview(ctx, lim) {
        if (!preview) return;
        ctx.beginPath();
        tracePath(ctx, preview.curve, lim);
        ctx.strokeStyle = 'rgba(226,232,240,.32)';
        ctx.setLineDash([3, 4]);
        ctx.lineWidth = 1;
        ctx.stroke();
        ctx.setLineDash([]);
        const x = xOf(preview.f), y = yOf(hasGain(preview.type) ? clamp(preview.gain, -range, range) : 0);
        ctx.beginPath();
        ctx.arc(x, y, 6, 0, Math.PI * 2);
        ctx.strokeStyle = 'rgba(226,232,240,.45)';
        ctx.stroke();
    }

    function bandText(b) {
        return fmtF(b.f) + (hasGain(b.type) ? '  ' + fmtDb(b.gain) : '') + (isCut(b.type) ? '  ' + b.slope + ' dB/oct' : '  Q ' + b.q.toFixed(2));
    }

    function label(ctx, txt, x, y, color) {
        ctx.font = '10px ' + MONO;
        const tw = ctx.measureText(txt).width + 10;
        let bx = x + 14;
        if (bx + tw > G.w - P.r) bx = x - 14 - tw;
        const by = clamp(y - 9, 2, G.h - P.b - 20);
        ctx.fillStyle = 'rgba(4,10,20,.82)';
        ctx.strokeStyle = color;
        ctx.lineWidth = 1;
        ctx.beginPath();
        if (ctx.roundRect) ctx.roundRect(bx, by, tw, 18, 4); else ctx.rect(bx, by, tw, 18);
        ctx.fill(); ctx.stroke();
        ctx.fillStyle = '#e2e8f0';
        ctx.textAlign = 'left';
        ctx.textBaseline = 'middle';
        ctx.fillText(txt, bx + 5, by + 9.5);
    }

    function draw() {
        updateSize();
        const ctx = canvas.getContext('2d');
        const { w, h } = G;
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        ctx.clearRect(0, 0, w, h);

        // Background gradient
        const bg = ctx.createLinearGradient(0, 0, 0, h);
        bg.addColorStop(0, 'rgba(2,6,14,.2)');
        bg.addColorStop(1, 'rgba(2,6,14,.75)');
        ctx.fillStyle = bg;
        ctx.fillRect(0, 0, w, h);

        drawGrid(ctx, w, h);

        // Everything else stays inside the plot
        ctx.save();
        ctx.beginPath();
        ctx.rect(0, 0, w - P.r + 1, h - P.b);
        ctx.clip();

        drawSpectrum(ctx, viewInfo());

        const lim = range * 1.3;
        bands.forEach(b => { if (!sel.has(b.id)) drawBand(ctx, b, lim); });
        bands.forEach(b => { if (sel.has(b.id)) drawBand(ctx, b, lim); });

        // Total curve
        ctx.beginPath();
        tracePath(ctx, total(), lim);
        ctx.strokeStyle = eqOn ? '#e2e8f0' : '#64748b';
        ctx.lineWidth = 2.2;
        ctx.shadowColor = 'rgba(34,211,238,.8)';
        ctx.shadowBlur = eqOn ? 10 : 0;
        ctx.stroke();
        ctx.shadowBlur = 0;

        drawPreview(ctx, lim);

        // Hover crosshair
        if (hover && !drag && !rect?.active) {
            ctx.strokeStyle = 'rgba(148,163,184,.13)';
            ctx.lineWidth = 1;
            ctx.beginPath();
            ctx.moveTo(Math.round(hover.x) + 0.5, P.t); ctx.lineTo(Math.round(hover.x) + 0.5, h - P.b);
            ctx.moveTo(P.l, Math.round(hover.y) + 0.5); ctx.lineTo(w - P.r, Math.round(hover.y) + 0.5);
            ctx.stroke();
            ctx.font = '10px ' + MONO;
            ctx.fillStyle = 'rgba(148,163,184,.8)';
            ctx.textAlign = hover.x > w - P.r - 110 ? 'right' : 'left';
            ctx.textBaseline = 'bottom';
            ctx.fillText(fmtF(fOf(hover.x)) + ' · ' + fmtDb(dOf(hover.y)), hover.x + (ctx.textAlign === 'left' ? 8 : -8), hover.y - 5);
        }

        // Band nodes
        ctx.textAlign = 'center';
        ctx.textBaseline = 'middle';
        ctx.font = 'bold 9px ' + MONO;
        bands.forEach((b, i) => {
            if (!inView(b.f)) return;
            const x = xOf(b.f), y = nodeY(b), isSel = sel.has(b.id), rad = isSel ? 8.5 : 7;
            if (b.id === primary) {
                ctx.beginPath();
                ctx.arc(x, y, rad + 4, 0, Math.PI * 2);
                ctx.strokeStyle = b.color + '66';
                ctx.lineWidth = 2;
                ctx.stroke();
            }
            ctx.beginPath();
            ctx.arc(x, y, rad, 0, Math.PI * 2);
            ctx.fillStyle = b.on ? b.color : 'rgba(15,23,42,.9)';
            ctx.globalAlpha = b.on ? 0.92 : 1;
            ctx.fill();
            ctx.globalAlpha = 1;
            ctx.lineWidth = isSel ? 2 : 1.2;
            ctx.strokeStyle = isSel ? '#ffffff' : (b.on ? 'rgba(2,6,14,.9)' : b.color);
            ctx.stroke();
            ctx.fillStyle = b.on ? '#04101a' : b.color;
            ctx.fillText(String(i + 1), x, y + 0.5);
        });

        // Value readout beside the primary node
        const pb = byId(primary);
        if (pb && inView(pb.f)) label(ctx, bandText(pb), xOf(pb.f), nodeY(pb), pb.color + 'aa');

        // Rubber band
        if (rect?.active) {
            const x0 = Math.min(rect.a.x, rect.z.x), y0 = Math.min(rect.a.y, rect.z.y);
            ctx.fillStyle = 'rgba(56,189,248,.07)';
            ctx.strokeStyle = 'rgba(125,211,252,.5)';
            ctx.lineWidth = 1;
            ctx.fillRect(x0, y0, Math.abs(rect.z.x - rect.a.x), Math.abs(rect.z.y - rect.a.y));
            ctx.strokeRect(x0 + 0.5, y0 + 0.5, Math.abs(rect.z.x - rect.a.x), Math.abs(rect.z.y - rect.a.y));
        }
        ctx.restore();
        cbs.onDrawn?.();
    }

    // ── selection ───────────────────────────────────────────────────────────
    function select(ids, prim) {
        sel = new Set(ids);
        primary = prim ?? (ids.length ? ids[ids.length - 1] : null);
        if (primary !== null && !sel.has(primary)) sel.add(primary);
        cbs.onSelect?.([...sel], primary);
        markDirty();
    }

    // ── pointer helpers ─────────────────────────────────────────────────────
    function pos(e) {
        const r = canvas.getBoundingClientRect();
        if (r.width !== G.w || r.height !== G.h) { G = { w: r.width, h: r.height }; updateGrid(); }
        return { x: e.clientX - r.left, y: e.clientY - r.top };
    }
    const onAxis = p => p.y > G.h - P.b;
    const radius = e => (e.pointerType === 'touch' ? 20 : 12);
    const ctrl = e => e.ctrlKey || e.metaKey;

    function makePreview(p) {
        const pl = plot();
        const nearCurve = Math.abs(yOf(totalAt(p.x)) - p.y) < 6;
        const shelf = nearCurve ? shelfAt(p.x, pl) : null;
        if (!shelf && sel.size) return null;   // a click with a selection only deselects
        const type = shelf || typeAt(p.x, p.y, pl);
        const f = clamp(fOf(p.x), F_LO, F_HI);
        const gain = type === 'bell' ? clamp(dOf(p.y), -range, range) : 0;
        const pv = { type, f, gain, q: isCut(type) ? 0.71 : 1.0, slope: 12, shelf: !!shelf };
        pv.curve = bandCurve(pv, freqs, drawFs);
        return pv;
    }

    function startDrag(b, p, e, created) {
        const group = sel.has(b.id) && sel.size > 1;
        const members = group ? bands.filter(x => sel.has(x.id)) : [b];
        drag = {
            b, group, created,
            start: members.map(x => ({ id: x.id, f: x.f, gain: x.gain, q: x.q, gained: hasGain(x.type) })),
            last: p, sx: 0, sy: 0,
            lf: Math.log(b.f), g: b.gain, q: b.q,
            mode: ctrl(e) && !created ? 'q' : 'move',
            moved: !!created, mods: { alt: e.altKey, shift: e.shiftKey, ctrl: ctrl(e) },
        };
        canvas.setPointerCapture(e.pointerId);
        if (created) cbs.onDrag?.(true);
    }

    function applyDrag(p, e) {
        const d = drag, k = e.shiftKey ? 0.15 : 1;
        const dx = (p.x - d.last.x) * k, dy = (p.y - d.last.y) * k;
        d.last = p;
        d.sx += dx; d.sy += dy;
        const pw = G.w - P.l - P.r, ph = G.h - P.t - P.b;
        d.lf += dx / pw * LV();
        d.g -= dy / ph * 2 * range;
        d.q *= Math.exp(-dy / 80);
        const b = d.b;
        // Alt locks the dominant axis of the whole drag
        const lockX = e.altKey && Math.abs(d.sy) > Math.abs(d.sx);
        const lockY = e.altKey && !lockX;
        const s0 = d.start.find(s => s.id === b.id);
        if (d.mode === 'q') {
            b.q = clamp(d.q, Q_LO, Q_HI);
            recalcBand(b);
            return [b];
        }
        const f1 = lockX ? s0.f : clamp(Math.exp(d.lf), F_LO, F_HI);
        let g1 = lockY ? s0.gain : clamp(d.g, -G_MAX, G_MAX);
        if (!hasGain(b.type)) g1 = b.gain;
        let moved;
        if (d.group) {
            const m = groupMove(d.start, b.id, f1, g1);
            moved = bands.filter(x => m[x.id]);
            moved.forEach(x => { x.f = m[x.id].f; x.gain = m[x.id].gain; recalcBand(x); });
        } else {
            b.f = f1;
            if (hasGain(b.type)) b.gain = g1;
            else if (!lockY) b.q = clamp(s0.q * Math.exp(-d.sy / 80), Q_LO, Q_HI);
            recalcBand(b);
            moved = [b];
        }
        // The dB scale grows when a gain goes beyond it
        let peak = 0;
        moved.forEach(x => { if (hasGain(x.type)) peak = Math.max(peak, Math.abs(x.gain)); });
        const r = expandRange(range, peak);
        if (r !== range) { range = r; cbs.onRange?.(r); }
        return moved;
    }

    // ── pointer events ──────────────────────────────────────────────────────
    canvas.addEventListener('contextmenu', e => e.preventDefault());

    canvas.addEventListener('pointerdown', e => {
        const p = pos(e);
        canvas.focus({ preventScroll: true });
        const hp = hitPoint(points(), p, radius(e));
        const b = hp?.b || null;
        if (e.button === 2) {
            cbs.onTypeMenu?.(e, b, clamp(fOf(p.x), F_LO, F_HI), clamp(dOf(p.y), -range, range));
            return;
        }
        if (e.button !== 0) return;
        preview = null;
        if (b) {
            if (!sel.has(b.id) && !ctrl(e) && !e.shiftKey && !e.altKey) select([b.id], b.id);
            else if (sel.has(b.id) && primary !== b.id) { primary = b.id; cbs.onSelect?.([...sel], primary); }
            startDrag(b, p, e, false);
        } else if (onAxis(p)) {
            axis = { p0: p, view0: view, f0: fOf(p.x), moved: false };
            canvas.setPointerCapture(e.pointerId);
        } else {
            const pv = makePreview(p);
            if (pv?.shelf) {
                // Grabbing the summed curve near an end makes a shelf and drags it
                const nb = cbs.onAdd?.(pv.type, pv.f, 0);
                lastAdd = performance.now();
                if (nb) { select([nb.id], nb.id); startDrag(nb, p, e, true); }
            } else {
                rect = { a: p, z: p, add: ctrl(e), active: false, pv };
                canvas.setPointerCapture(e.pointerId);
            }
        }
        markDirty();
    });

    canvas.addEventListener('pointermove', e => {
        const p = pos(e);
        if (drag) {
            if (!drag.moved) {
                if (Math.hypot(p.x - drag.last.x, p.y - drag.last.y) < MOVE_PX) return;
                drag.moved = true;
                if (drag.b && !sel.has(drag.b.id)) select([drag.b.id], drag.b.id);
                cbs.onEditStart?.();
                cbs.onDrag?.(true);
            }
            const moved = applyDrag(p, e);
            cbs.onEdit?.(moved, false);
            markDirty();
            return;
        }
        if (axis) {
            const dx = p.x - axis.p0.x, dy = p.y - axis.p0.y;
            if (!axis.moved && Math.hypot(dx, dy) < MOVE_PX) return;
            axis.moved = true;
            // Up zooms in around the press point, sideways pans
            let v = zoomView(axis.view0, axis.f0, Math.exp(dy / 120));
            v = panView(v, -dx / (G.w - P.l - P.r) * Math.log(v.hi / v.lo));
            setView(v);
            return;
        }
        if (rect) {
            rect.z = p;
            if (!rect.active && Math.hypot(p.x - rect.a.x, p.y - rect.a.y) >= MOVE_PX) rect.active = true;
            markDirty();
            return;
        }
        const hp = hitPoint(points(), p, radius(e));
        if (hp) {
            canvas.style.cursor = 'grab';
            hover = null; preview = null;
        } else if (onAxis(p)) {
            canvas.style.cursor = 'ew-resize';
            hover = null; preview = null;
        } else {
            canvas.style.cursor = 'crosshair';
            hover = p;
            preview = makePreview(p);
        }
        markDirty();
    });

    canvas.addEventListener('pointerleave', () => {
        if (!drag && !rect) { hover = null; preview = null; markDirty(); }
    });

    function endPointer(e) {
        if (drag) {
            const d = drag;
            drag = null;
            if (d.moved) {
                bands.sort((a, z) => a.f - z.f);
                cbs.onDrag?.(false);
                cbs.onEdit?.(bands.filter(x => d.start.some(s => s.id === x.id)), true);
            } else if (d.mods.alt) {
                // Alt-click: bypass the band (the whole selection when it is in one)
                cbs.onToggle?.(sel.has(d.b.id) && sel.size > 1 ? bands.filter(x => sel.has(x.id)) : [d.b]);
            } else if (d.mods.ctrl) {
                const ids = new Set(sel);
                ids.has(d.b.id) ? ids.delete(d.b.id) : ids.add(d.b.id);
                select([...ids], ids.has(d.b.id) ? d.b.id : [...ids].pop() ?? null);
            } else if (d.mods.shift) {
                select(idRange(bands.map(x => x.id), primary ?? d.b.id, d.b.id), d.b.id);
            } else if (sel.size > 1) {
                select([d.b.id], d.b.id);
            }
        } else if (axis) {
            axis = null;
        } else if (rect) {
            const r = rect;
            rect = null;
            if (r.active) {
                const ids = inRect(points(), r.a, r.z);
                select(r.add ? [...new Set([...sel, ...ids])] : ids, ids.length ? ids[ids.length - 1] : (r.add ? primary : null));
            } else if (performance.now() - lastAdd < 500) {
                // the second click of a double-click that just added a band
            } else if (sel.size && !r.add) {
                select([], null);
            } else if (r.pv && !e?.altKey) {
                const nb = cbs.onAdd?.(r.pv.type, r.pv.f, r.pv.gain);
                lastAdd = performance.now();
                if (nb) select([nb.id], nb.id);
            }
            if (e) { const p = pos(e); hover = p; preview = makePreview(p); }
        }
        markDirty();
    }
    canvas.addEventListener('pointerup', endPointer);
    canvas.addEventListener('pointercancel', () => endPointer(null));

    canvas.addEventListener('dblclick', e => {
        const p = pos(e);
        const hp = hitPoint(points(), p, radius(e));
        if (hp) { cbs.onOpenPanel?.(hp.b); return; }
        if (onAxis(p)) { setView({ lo: F_MIN, hi: F_MAX }); return; }
        if (performance.now() - lastAdd < 500) return;
        const pv = makePreview(p);
        if (!pv) return;
        const nb = cbs.onAdd?.(pv.type, pv.f, pv.gain);
        lastAdd = performance.now();
        if (nb) select([nb.id], nb.id);
    });

    canvas.addEventListener('wheel', e => {
        const p = pos(e);
        if (onAxis(p) && !drag) {
            e.preventDefault();
            setView(zoomView(view, fOf(p.x), Math.exp(e.deltaY * 0.0015)));
            return;
        }
        const b = drag?.b || hitPoint(points(), p, radius(e))?.b;
        if (!b) return;
        e.preventDefault();
        const now = performance.now(), isNew = now - lastWheel > 700;
        lastWheel = now;
        const fine = e.shiftKey ? 0.2 : 1;
        if (ctrl(e) && hasGain(b.type)) {
            b.gain = clamp(Math.round((b.gain - Math.sign(e.deltaY) * 0.5 * fine) * 10) / 10, -G_MAX, G_MAX);
            const r = expandRange(range, b.gain);
            if (r !== range) { range = r; cbs.onRange?.(r); }
        } else if (isCut(b.type) && !ctrl(e)) {
            // Cuts step their slope; a trackpad's small deltas add up to a step
            wheelAcc = isNew ? e.deltaY : wheelAcc + e.deltaY;
            if (Math.abs(wheelAcc) < 40) return;
            b.slope = slopeStep(SLOPES, b.slope, -Math.sign(wheelAcc));
            wheelAcc = 0;
        } else {
            b.q = clamp(b.q * Math.exp(-e.deltaY * 0.0015 * fine), Q_LO, Q_HI);
        }
        recalcBand(b);
        if (!sel.has(b.id)) select([b.id], b.id);
        cbs.onWheelEdit?.(b, isNew);
        markDirty();
    }, { passive: false });

    function setView(v) {
        view = { lo: v.lo, hi: v.hi };
        updateGrid();
        markDirty();
    }

    // ── public API ──────────────────────────────────────────────────────────
    return {
        /** Replace all bands (must carry id and color; curves are made here). */
        setBands(newBands) { bands = newBands; markDirty(); },
        /** Select: primary id and, optionally, the whole set of ids. */
        setSel(id, ids) {
            sel = new Set(ids ?? (id == null ? [] : [id]));
            primary = id ?? null;
            if (primary !== null) sel.add(primary);
            for (const x of [...sel]) if (!byId(x)) sel.delete(x);
            markDirty();
        },
        setRange(r) { range = r; markDirty(); },
        setEqOn(on) { eqOn = on; markDirty(); },
        setSpecMode(mode) { specMode = mode; markDirty(); },
        /** Accept raw dB data from /player/spectrum (post-EQ, as-is from player). */
        setSpectrum(data) { specData = data; markDirty(); },
        /** The spectrum layer: fn(ctx, view) draws between the grid and the bands. */
        setSpectrumDrawer(fn) { specDrawer = fn || null; markDirty(); },
        /** Update the sample rate for future bandCurve calculations. */
        setDrawFs(fs) {
            if (fs !== drawFs) {
                drawFs = fs;
                gridVer++;
                markDirty();
            }
        },
        /** The frequency view { lo, hi }; reset() goes back to 10 Hz–30 kHz. */
        setView, getView: () => ({ ...view }),
        resetView() { setView({ lo: F_MIN, hi: F_MAX }); },
        /** Where a band's node is, in CSS px of the canvas (null when off view). */
        nodePos(id) {
            const b = byId(id);
            return b && inView(b.f) ? { x: xOf(b.f), y: nodeY(b), w: G.w, h: G.h, plotBottom: G.h - P.b } : null;
        },
        /** Recompute one band's curve (call after editing band fields). */
        recalcBand,
        /** Call from a ResizeObserver callback. */
        resize() { updateSize(); markDirty(); },
        /** Trigger an immediate synchronous redraw (e.g. after range change). */
        redraw() { updateSize(); draw(); dirty = false; rafId = 0; },
        markDirty,
    };
}
