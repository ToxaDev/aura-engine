// ══════════════════════════════════════════════════════════════════════
// The Visualizer page's reckoning (visualizer.js), apart from the page:
// which scene shows first, the gallery's tiles and their marks, the
// sliders as rows, a value as it is shown and as it is typed, the
// instruments pack's line, how far ahead the render is asked to keep.
// ══════════════════════════════════════════════════════════════════════

import { fmtNum, colourHex } from '../vis/scene.js';

/// The scene the page showed last (its own; the player's look is apart).
export const PAGE_SCENE = 'auraVisPageScene';
/// The scene the render keeps the sound ahead for (waves.js AHEAD_S).
export const AHEAD_S = 3.8;
/// The scene first shown where nothing was chosen (waves.js: it shows the
/// sound coming).
export const FIRST = 'my:tunnel.aura-vis';

/// A name's own colour (a tile without its picture): a hue 0…359.
export function nameHue(name) {
    let h = 2166136261;
    for (const ch of String(name || '')) {
        h ^= ch.codePointAt(0);
        h = Math.imul(h, 16777619) >>> 0;
    }
    return h % 360;
}

/// The scene the player draws, from its look (waves.js: 'vis:<id>', 'album',
/// 'off') and the scene chosen last: { id, always } — always: on every track;
/// false: under Album, where a track has no cover. id null: none.
export function playerScene(look, effect) {
    const sc = s => (/^vis:/.test(s || '') ? s.slice(4) : null);
    if (look === 'off') return { id: null, always: false };
    if (sc(look)) return { id: sc(look), always: true };
    return { id: sc(effect), always: false };
}

/// The scene the page shows first: the one it showed last, else the player's,
/// else the first one shown anywhere, else the first of the list.
export function firstScene(catalog, { kept, player } = {}) {
    const has = id => !!id && catalog.some(c => c.id === id);
    if (has(kept)) return kept;
    if (has(player)) return player;
    if (has(FIRST)) return FIRST;
    return catalog[0]?.id ?? null;
}

/// The gallery's tiles in the list's order: { id, name, about, spatial, hue,
/// current, mark ('' | 'IN PLAYER' | 'IN PLAYER · NO COVER'), failed }.
export function tilesOf(catalog, { current = null, player = { id: null, always: false }, failed = new Set() } = {}) {
    return catalog.map(e => ({
        id: e.id,
        name: e.header?.name || 'Untitled',
        about: e.header?.about || '',
        spatial: !!e.header?.spatial,
        hue: nameHue(e.header?.name || e.id),
        current: e.id === current,
        mark: player.id === e.id ? (player.always ? 'IN PLAYER' : 'IN PLAYER · NO COVER') : '',
        failed: failed.has(e.id),
    }));
}

/// A value as the slider's label shows it.
export function shownValue(p, v) {
    if (p.type === 'color') return colourHex(Array.isArray(v) ? v : p.def);
    if (p.type === 'bool') return v ? 'On' : 'Off';
    if (p.type === 'choice') return p.options?.[Math.round(v)] ?? String(v);
    return fmtNum(v, p) + (p.unit ? ' ' + p.unit : '');
}

/// A value from its control (a range's text, a checkbox, a colour's #rrggbb,
/// a choice's index): in range, of the param's kind; the default for
/// anything that is not one.
export function valueFrom(p, raw) {
    if (p.type === 'color') {
        const m = /^#?([0-9a-f]{6})$/i.exec(String(raw || ''));
        if (!m) return p.def.slice();
        return [0, 2, 4].map(i => parseInt(m[1].slice(i, i + 2), 16) / 255);
    }
    if (p.type === 'bool') return raw === true || raw === 1 || raw === '1' || raw === 'true' ? 1 : 0;
    const n = Number(raw);
    if (raw === '' || raw == null || !Number.isFinite(n)) return p.def;
    const c = Math.min(p.max, Math.max(p.min, n));
    return p.type === 'int' || p.type === 'choice' ? Math.round(c) : c;
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/// The sliders in their sections (the scene's // @section groups, in order;
/// a section without a name is the first, plain one): [{ name, why, rows:
/// [{ p, value, text, changed }] }] — changed: not its default.
export function sliderRows(header, values) {
    const byName = new Map((header?.params || []).map(p => [p.name, p]));
    const out = [];
    for (const s of header?.sections || []) {
        const rows = [];
        for (const n of s.params) {
            const p = byName.get(n);
            if (!p) continue;
            const value = values?.[n] ?? p.def;
            rows.push({ p, value, text: shownValue(p, value), changed: !same(value, p.def) });
        }
        if (rows.length) out.push({ name: s.name, why: s.why, rows });
    }
    return out;
}

/// Any slider off its default (Reset has something to do).
export const anyChanged = (header, values) => (header?.params || []).some(p => !same(values?.[p.name] ?? p.def, p.def));

/// The instruments pack's line under a scene with instruments, from
/// spatial_status ({ installed, busy, state, got, total, background }):
/// { text, get } — get: the line offers the download (in the studio).
/// Null: nothing to say (a plain scene, or the status unknown).
export function packText(st, spatial) {
    if (!spatial || !st) return null;
    if (st.busy && ['downloading', 'verifying', 'unpacking'].includes(st.state)) {
        const pct = st.total ? Math.min(100, Math.floor((100 * st.got) / st.total)) : 0;
        return { text: `${st.background ? 'Updating' : 'Downloading'} the instruments pack… ${pct} %`, get: false };
    }
    if (st.installed) return { text: 'With instruments · the instruments pack is in', get: false };
    return { text: 'The instruments pack is not installed — this scene shows only the rough layer.', get: true };
}

/// A new scene's name not yet in the list: "My scene", "My scene 2", …
export function freshName(names, base = 'My scene') {
    const taken = new Set((names || []).map(n => String(n).trim().toLowerCase()));
    if (!taken.has(base.toLowerCase())) return base;
    for (let i = 2; ; i++) if (!taken.has(`${base} ${i}`.toLowerCase())) return `${base} ${i}`;
}

/// The text with its @name line set to `name` (added when there is none) —
/// studio.js named().
export function named(text, name) {
    if (/^\s*\/\/\s*@name\b.*$/m.test(text)) return text.replace(/^(\s*\/\/\s*@name\s*).*$/m, (_, a) => a + name);
    return `// @name     ${name}\n` + text;
}

/// The look-ahead the page asks for: while it draws a scene on screen and
/// something plays or stands paused.
export const aheadWant = ({ drawing, visible, state }) =>
    drawing && visible && (state === 'playing' || state === 'paused') ? AHEAD_S : 0;
