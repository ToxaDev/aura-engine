// The landscape window's arithmetic and its remembered state, with no DOM
// and no window behind them (landscape.js does the moving; the tests read
// this file alone).

/// The compact window's width (tauri.conf.json, window_state.rs).
export const COMPACT_W = 480;
/// The landscape window's least size and the size it first opens at.
export const MIN_W = 1100;
export const MIN_H = 680;
export const FIRST_W = 1440;
export const FIRST_H = 900;

export const KEY = 'auraWindowMode';
/// The Library intro: absent on a fresh profile and on one from a version
/// before the window opened in Library. The start that finds it absent
/// opens in Library and writes it; from then on the look the listener
/// left is the look that opens.
export const INTRO_KEY = 'auraLibraryIntro';
const TABS = ['queue', 'sound'];

const num = v => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const pt = v => (v && num(v.x) != null && num(v.y) != null ? { x: Math.round(v.x), y: Math.round(v.y) } : null);

/// What was kept, read back. Anything unreadable is the compact window with
/// nothing remembered.
export function parseSaved(text) {
    const out = { mode: 'compact', size: null, pos: null, max: false, tab: 'queue', compactPos: null };
    let v = null;
    try { v = JSON.parse(text || 'null'); } catch (_) { return out; }
    if (!v || typeof v !== 'object') return out;
    if (v.mode === 'landscape') out.mode = 'landscape';
    const w = num(v.size?.w), h = num(v.size?.h);
    if (w != null && h != null && w >= 200 && h >= 200) out.size = { w: Math.round(w), h: Math.round(h) };
    out.pos = pt(v.pos);
    out.compactPos = pt(v.compactPos);
    out.max = v.max === true;
    if (TABS.includes(v.tab)) out.tab = v.tab;
    return out;
}

export const stringifySaved = s => JSON.stringify({
    mode: s.mode === 'landscape' ? 'landscape' : 'compact',
    size: s.size || null, pos: s.pos || null, max: !!s.max,
    tab: TABS.includes(s.tab) ? s.tab : 'queue', compactPos: s.compactPos || null,
});

/// Whether this start is the intro's (nothing kept under INTRO_KEY).
export const introDue = stored => stored == null;

/// The intro's record, kept under INTRO_KEY.
export const introRecord = (at = new Date().toISOString()) => JSON.stringify({ v: 1, at });

/// Where and how large the landscape window opens, in logical pixels: the
/// size kept (or the first size), made to fit the screen's work area `area`
/// ({x, y, w, h}); at the place kept if the whole window fits there,
/// otherwise from where the compact window stands (`at`), moved in just as
/// far as it takes to be whole on the screen.
export function fitLandscape(saved, at, area) {
    const a = area && area.w > 0 && area.h > 0 ? area : null;
    let w = saved?.size?.w || FIRST_W;
    let h = saved?.size?.h || FIRST_H;
    w = Math.max(w, MIN_W); h = Math.max(h, MIN_H);
    if (!a) {
        const p = saved?.pos || at || { x: 0, y: 0 };
        return { w, h, x: p.x, y: p.y };
    }
    // A screen smaller than the least size gets the whole of it.
    w = Math.min(w, a.w); h = Math.min(h, a.h);
    const inside = p => p && p.x >= a.x && p.y >= a.y && p.x + w <= a.x + a.w && p.y + h <= a.y + a.h;
    if (inside(saved?.pos)) return { w, h, x: saved.pos.x, y: saved.pos.y };
    const from = at || { x: a.x + (a.w - w) / 2, y: a.y + (a.h - h) / 2 };
    const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);
    return {
        w, h,
        x: Math.round(clamp(from.x, a.x, a.x + a.w - w)),
        y: Math.round(clamp(from.y, a.y, a.y + a.h - h)),
    };
}

/// The compact window coming back: where it stood before (if that is still
/// on the screen), otherwise where the landscape window's corner is, moved
/// in to fit `w` wide.
export function compactPlace(kept, at, area, w = COMPACT_W) {
    const a = area && area.w > 0 && area.h > 0 ? area : null;
    if (!a) return kept || at || null;
    const ok = p => p && p.x >= a.x && p.y >= a.y && p.x + w <= a.x + a.w && p.y < a.y + a.h - 60;
    if (ok(kept)) return kept;
    const p = at || { x: a.x, y: a.y };
    return {
        x: Math.round(Math.min(Math.max(p.x, a.x), a.x + a.w - w)),
        y: Math.round(Math.min(Math.max(p.y, a.y), a.y + a.h - 200)),
    };
}

/// A screen's work area in logical pixels, from a Tauri monitor (physical
/// pixels; workArea where the engine reports one, else the whole screen).
export function logicalArea(mon) {
    if (!mon) return null;
    const s = mon.scaleFactor || 1;
    const box = mon.workArea || { position: mon.position, size: mon.size };
    if (!box?.position || !box?.size) return null;
    return {
        x: Math.round(box.position.x / s), y: Math.round(box.position.y / s),
        w: Math.floor(box.size.width / s), h: Math.floor(box.size.height / s),
    };
}

/// A field that types text: a key pressed there is the field's.
export function isTextTarget(t) {
    if (!t || t.nodeType !== 1) return false;
    const tag = (t.tagName || '').toUpperCase();
    if (t.isContentEditable || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'OPTION') return true;
    return tag === 'INPUT' && !/^(range|checkbox|radio|button|submit|reset|color)$/i.test(t.type || 'text');
}

/// Ctrl+L (Cmd+L on a Mac) switches the window, by the physical key so the
/// Russian layout's Д works too; never inside a text field.
export function isSwitchKey(e) {
    if (!e || e.repeat || e.altKey || e.shiftKey) return false;
    if (!(e.ctrlKey || e.metaKey)) return false;
    if (e.code !== 'KeyL' && String(e.key).toLowerCase() !== 'l') return false;
    return !isTextTarget(e.target);
}

/// The plain L of listening mode, which the landscape window does not have.
export function isListeningKey(e) {
    if (!e || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return false;
    return e.code === 'KeyL' && !isTextTarget(e.target);
}

/// Whole rows of the list that fit `avail` pixels of height: the list's
/// own frame (`extra`: the zone's border and anything not rows) taken off,
/// then rows `rowH` high `gap` apart inside `pad` above and below. Never
/// fewer than `least`.
export function rowsFor(avail, { rowH = 43, gap = 4, pad = 4, extra = 0, least = 3 } = {}) {
    const room = avail - extra - 2 * pad + gap;
    const n = Math.floor(room / (rowH + gap));
    return Math.max(least, Number.isFinite(n) ? n : least);
}
