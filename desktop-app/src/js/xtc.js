// ══════════════════════════════════════════════════════════════════════
// XTC geometry, converter-window side.
//
// Every other stage in the rack works on the file alone. This one works on
// the file *and the room it will be played in*, so it is the only badge that
// cannot be switched on cold: without a measured triangle the filter would be
// built for an invented listener and would reinforce the crosstalk it exists
// to remove. Hence the gate — the badge opens the geometry window the first
// time it is clicked, and only becomes switchable once a triangle is saved.
//
// The measuring itself happens in a WINDOW of its own (xtc-geometry.html),
// not in a dialog over the converter. Four measurements, a plan of the
// triangle and the angle they imply need more width than this app has; the
// first attempt put them in a modal and the labels wrapped one word to a line.
//
// This file therefore holds no interface at all any more. It holds the state,
// the gate, and the two wires to that window:
//   out — `xtc_geometry_open` opens (or focuses) it
//   in  — an `xtc:geometry` event carries the saved triangle back
//
// The window deliberately does not write settings itself. This window stays
// the only author of `auraSettings`, so there is no second writer to race.
//
// The arithmetic that decides what a valid triangle is lives in
// xtc-geom.js, imported by both windows so they can never disagree.
// ══════════════════════════════════════════════════════════════════════

import { LIMITS, FIELDS, isGeometryValid, spanAngleDeg, angleVerdict } from './xtc-geom.js';

// `mode` is carried but never used here: it is how the geometry was TYPED, kept
// only so the window can reopen showing the figures the owner entered rather
// than a translation of them. The filter reads the triangle, not the mode.
const DEFAULTS = { span: 0, distL: 0, distR: 0, headW: LIMITS.headW.def, mode: 'depth', saved: false };

let geom = { ...DEFAULTS };

export function getXtcGeometry() { return { ...geom }; }

export function setXtcGeometry(obj) {
    if (!obj || typeof obj !== 'object') return;
    for (const k of FIELDS) {
        const v = Number(obj[k]);
        if (Number.isFinite(v)) geom[k] = v;
    }
    if (obj.mode === 'depth' || obj.mode === 'direct') geom.mode = obj.mode;
    geom.saved = !!obj.saved && isGeometryValid(geom).ok;
}

/// Whether the badge is allowed to come up at all.
export function isXtcConfigured() { return geom.saved && isGeometryValid(geom).ok; }

/// What travels to Rust. Millimetres throughout — the backend converts once, so
/// there is exactly one place where a unit can be got wrong.
export function xtcGeometryPayload() {
    return {
        speakerSpanMm: geom.span,
        leftDistanceMm: geom.distL,
        rightDistanceMm: geom.distR,
        headWidthMm: geom.headW,
    };
}

/// One line for the badge tooltip, so the rack can say what geometry it is
/// holding without anything having to be opened.
export function xtcSummary() {
    if (!isXtcConfigured()) return 'no geometry measured yet — click the gear';
    const deg = spanAngleDeg(geom);
    return `${geom.span} mm span · ${Math.round((geom.distL + geom.distR) / 2)} mm out · `
         + `${deg.toFixed(1)}° — ${angleVerdict(deg).text}`;
}

// ── the two wires to the geometry window ──────────────────────────────

const tauri = () => window.__TAURI__;

/// Open (or focus) the geometry window.
///
/// `after` runs when a valid triangle comes back — which is what lets a click
/// on an unconfigured badge turn the stage on once the gate is satisfied. It
/// is held rather than awaited: the window is a separate window, the user may
/// take a minute with a tape measure, and nothing here should block on that.
let pending = null;

export function openXtcGeometry(after) {
    pending = after || null;
    try {
        tauri().tauri.invoke('xtc_geometry_open').catch((e) => {
            pending = null;
            console.error('[lab] geometry window would not open:', e);
        });
    } catch (e) {
        pending = null;
        console.error('[lab] geometry window would not open:', e);
    }
}

/// Listen for the saved triangle. Called once, from initDspRack.
///
/// `repaint` is handed in rather than imported to keep this file free of any
/// dependency on the panel — the panel owns its own drawing.
export function initXtcBridge(repaint) {
    let ev;
    try {
        ev = tauri().event;
    } catch (e) {
        return;                       // no Tauri (a plain browser): nothing to listen to
    }
    if (!ev?.listen) return;
    ev.listen('xtc:geometry', (msg) => {
        const g = msg?.payload;
        if (!g) return;
        setXtcGeometry({ ...g, saved: true });
        if (!isXtcConfigured()) return;
        import('./settings.js').then(m => m.saveSettings());
        const cb = pending;
        pending = null;
        repaint?.();
        cb?.();
    }).catch(() => {});
}
