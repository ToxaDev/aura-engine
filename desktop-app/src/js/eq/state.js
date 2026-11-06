// eq/state.js — the EqState contract (SPEC) and what is built on it: the
// clamps, the stable signature, undo history and the presets list.
//
// Pure, no DOM: the main window (settings, rack, rackSig) and the EQ window
// both import it, so a state read back from settings is the state the
// window shows and the engine gets.

import { TYPES, hasGain, clampBand } from './math.js';

export const MAX_BANDS = 24;
export const FLAT_NAME = 'Flat';

const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const num = (v, d) => (typeof v === 'number' && isFinite(v) ? v : d);

/// How the level after the EQ is set (state.gain): manual = the output gain
/// alone (the default); auto = the level that sounds as loud as the EQ off,
/// plus the output gain; safe = lowered by the biggest boost, plus the output.
export const GAIN_MODES = ['manual', 'auto', 'safe'];

/// A fresh state: off, no bands, manual gain at 0 dB.
export function flatState() {
    return { v: 2, on: false, name: FLAT_NAME, bands: [], gain: 'manual', output: 0 };
}

/// The gain mode and output (dB, ±30) of any state, a version-1 one too:
/// its auto preamp (null) becomes `safe`, a manual preamp goes into the
/// output. The engine reads old states the same way.
function levelOf(s) {
    const out = num(s.output, 0);
    if (GAIN_MODES.includes(s.gain)) return { gain: s.gain, output: clamp(out, -30, 30) };
    if (typeof s.preamp === 'number' && isFinite(s.preamp)) return { gain: 'manual', output: clamp(out + clamp(s.preamp, -30, 0), -30, 30) };
    return { gain: s.v >= 2 ? 'manual' : 'safe', output: clamp(out, -30, 30) };
}

/// The balance after the EQ (state.balance, dB, ±30, 0.01 dB steps): how
/// much quieter the other side is, + toward the right (or the side, when
/// state.balanceMode is 'ms'), − toward the left (or the mid). It only
/// lowers, so it never adds an over. The engine reads it the same way.
export const BALANCE_MAX = 30;
export function balanceOf(s) {
    const db = clamp(Math.round(num(s && s.balance, 0) * 100) / 100, -BALANCE_MAX, BALANCE_MAX);
    return { balance: db === 0 ? 0 : db, balanceMode: s && s.balanceMode === 'ms' ? 'ms' : 'lr' };
}

/// The balance fields a normalized state carries: only what differs from
/// the centre and left/right, so a state without a balance stays as it was.
function balanceFields(s) {
    const { balance, balanceMode } = balanceOf(s);
    return { ...(balance !== 0 ? { balance } : {}), ...(balanceMode === 'ms' ? { balanceMode } : {}) };
}

/// The speaker delay after the EQ (state.delay, ms, ±30, 0.01 ms steps):
/// how late one channel plays, for a speaker nearer the listener than the
/// other — + the right is late, − the left. The engine reads it the same way.
export const DELAY_MAX = 30;
export function delayOf(s) {
    const ms = clamp(Math.round(num(s && s.delay, 0) * 100) / 100, -DELAY_MAX, DELAY_MAX);
    return ms === 0 ? 0 : ms;
}

/// The speakers' fields of a state (balance, its mode, delay), only those
/// off their default. They belong to the room, not to a sound: a preset
/// neither keeps nor brings them, and Reset leaves them.
export function speakerFields(s) {
    const delay = delayOf(s);
    return { ...balanceFields(s), ...(delay !== 0 ? { delay } : {}), ...(s && s.speakersAlways === true ? { speakersAlways: true } : {}) };
}

/// `s` without its speakers' fields.
export function withoutSpeakers(s) {
    const { balance: _b, balanceMode: _m, delay: _d, speakersAlways: _a, ...rest } = s || {};
    return rest;
}

/// `s` with the speakers of `from` (a preset loaded, the EQ reset).
export const withSpeakers = (s, from) => normalizeState({ ...withoutSpeakers(s), ...speakerFields(from) });

/// The presets that come with the EQ, after Flat (Anton 1.11): gentle,
/// wide moves of the kind a mastering engineer reaches for — no band above
/// +5 dB, Q 0.7–2; they come with the manual gain at 0 dB. Read-only:
/// a user preset may not take their names.
const band = (type, f, gain, q) => ({ type, f, gain, q, slope: 12, on: true });
const BUILTIN = [
    ['Warm', [band('lowShelf', 150, 2, 0.71), band('highShelf', 7000, -1.5, 0.71)]],
    ['Bright', [band('highShelf', 5000, 2, 0.71), band('bell', 250, -1, 0.8)]],
    ['Bass boost', [band('lowShelf', 105, 4, 0.71)]],
    ['Loudness', [band('lowShelf', 80, 5, 0.71), band('highShelf', 10000, 2, 0.71)]],
    ['Vocal', [band('bell', 3000, 2, 1.0), band('bell', 250, -1, 1.0)]],
    ['De-harsh', [band('bell', 3200, -2.5, 1.4), band('bell', 6500, -2, 2.0)]],
    ['Acoustic', [band('lowShelf', 90, 1.5, 0.71), band('bell', 2500, -1, 0.9), band('highShelf', 10000, 1, 0.71)]],
];
export const BUILTIN_PRESETS = Object.freeze(BUILTIN.map(([name, bands]) => Object.freeze({
    name, state: Object.freeze({ v: 2, on: true, name, bands: Object.freeze(bands.map(Object.freeze)), gain: 'manual', output: 0 }),
})));

/// Flat or a built-in preset's name (any case): not a name a user preset may have.
export const isReservedName = n => {
    const k = String(n || '').trim().toLowerCase();
    return k === FLAT_NAME.toLowerCase() || BUILTIN_PRESETS.some(p => p.name.toLowerCase() === k);
};

/// Every preset a list offers, in its order: Flat, the built-in ones, the user's.
export function allPresets(user) {
    return [{ name: FLAT_NAME, state: flatState() }, ...BUILTIN_PRESETS, ...normalizePresets(user)];
}

/// One band clamped to the contract; unknown types become a bell.
export function normalizeBand(b) {
    b = b || {};
    const type = Object.prototype.hasOwnProperty.call(TYPES, b.type) ? b.type : 'bell';
    const c = clampBand({ f: num(b.f, 1000), gain: num(b.gain, 0), q: num(b.q, 0.71), slope: num(b.slope, 12) });
    return { type, f: c.f, gain: c.gain, q: c.q, slope: c.slope, on: b.on !== false };
}

/// Any value (settings may be old, hand-edited or missing) as a valid
/// EqState of version 2 (a version-1 preamp migrated, see `levelOf`).
export function normalizeState(s) {
    if (!s || typeof s !== 'object') return flatState();
    const bands = Array.isArray(s.bands) ? s.bands.slice(0, MAX_BANDS).map(normalizeBand) : [];
    return {
        v: 2,
        on: s.on === true,
        name: typeof s.name === 'string' && s.name.trim() ? s.name.trim().slice(0, 60) : FLAT_NAME,
        bands,
        ...levelOf(s),
        ...speakerFields(s),
    };
}

/// A band that changes the sound: on, and a type that acts without gain or
/// a gain of at least 0.005 dB. The engine applies the same rule.
const bandActive = b => b.on !== false && (!hasGain(b.type) || Math.abs(b.gain) >= 0.005);
const activeBands = s => (s.bands || []).slice(0, MAX_BANDS).filter(bandActive).map(clampBand);

/// Whether the EQ changes the sound at all (the engine's eqActive): on, and
/// an active band, an output gain, a balance or a delay (with no band every
/// mode's level is 0). Off, the speakers are off with the rest.
export function eqActive(s) {
    const e = effectiveState(s);
    if (!e) return false;
    return activeBands(e).length > 0 || levelOf(e).output !== 0 || balanceOf(e).balance !== 0 || delayOf(e) !== 0;
}

/// What plays (the engine's `effective`): the state when on; off with
/// speakersAlways, its speakers alone (no band, no gain); else null.
export function effectiveState(s) {
    if (!s) return null;
    if (s.on) return s;
    if (s.speakersAlways !== true) return null;
    return { v: 2, on: true, name: s.name, bands: [], gain: 'manual', output: 0, ...speakerFields(s) };
}

/// Whether a balance or a delay plays now (the main window's SPK mark).
export function speakersActive(s) {
    const e = effectiveState(s);
    return !!e && (balanceOf(e).balance !== 0 || delayOf(e) !== 0);
}

/// True when the state changes nothing.
export const isBypass = s => !eqActive(s);

/// The signature rackSig carries, the engine's format exactly: null when
/// inactive, else JSON of [gain mode, output, [[type, f, gain, q, slope]…]]
/// over the active bands in order, [balance mode, balance] after them when
/// the balance is off centre or a delay is set, and the delay after those
/// when set (without them the signature is as it was, so earlier renders
/// still match). Name and version are not part of it: renaming a preset is
/// not a new render.
export function eqSig(s) {
    if (!eqActive(s)) return null;
    s = effectiveState(s);
    const bands = activeBands(s).map(b => [b.type, b.f, b.gain, b.q, b.slope]);
    const { gain, output } = levelOf(s);
    const { balance, balanceMode } = balanceOf(s);
    const delay = delayOf(s);
    const sig = [gain, output, bands];
    if (balance !== 0 || delay !== 0) sig.push(balanceMode, balance);
    if (delay !== 0) sig.push(delay);
    return JSON.stringify(sig);
}

/// The badge text: the short name, or FLAT when nothing is loaded.
export function badgeLabel(s) {
    if (!s || !s.bands || !s.bands.length) return 'FLAT';
    return shortName(s.name);
}

/// A profile file name as a short label ("HD 600 ParametricEQ.txt" → "HD 600").
export function shortName(name) {
    let s = String(name || 'Custom').replace(/\.(txt|cfg)$/i, '').replace(/\s*(ParametricEQ|GraphicEQ|FixedBandEQ)\s*$/i, '').trim();
    if (!s) s = 'Custom';
    return s.length > 14 ? s.slice(0, 13) + '…' : s;
}

// ── undo history ───────────────────────────────────────────────────────

/// Undo/redo of snapshots (any JSON-able value), at most `limit` deep.
export class History {
    constructor(limit = 60) { this.limit = limit; this.undo = []; this.redo = []; }
    /// Remember `snap` as the state before an edit; a new edit drops redo.
    push(snap) {
        this.undo.push(snap);
        if (this.undo.length > this.limit) this.undo.shift();
        this.redo.length = 0;
    }
    /// The state to go back to, given the current one; null when none.
    back(current) {
        if (!this.undo.length) return null;
        this.redo.push(current);
        return this.undo.pop();
    }
    forward(current) {
        if (!this.redo.length) return null;
        this.undo.push(current);
        return this.redo.pop();
    }
    get canUndo() { return this.undo.length > 0; }
    get canRedo() { return this.redo.length > 0; }
    clear() { this.undo.length = 0; this.redo.length = 0; }
}

// ── presets ────────────────────────────────────────────────────────────

/// The user's presets, cleaned: [{name, state}], unique names, flat excluded.
export function normalizePresets(list) {
    const out = [], seen = new Set();
    (Array.isArray(list) ? list : []).forEach(p => {
        if (!p || typeof p.name !== 'string') return;
        const name = p.name.trim().slice(0, 60);
        if (!name || isReservedName(name) || seen.has(name.toLowerCase())) return;
        seen.add(name.toLowerCase());
        const st = withoutSpeakers(normalizeState(p.state));
        st.name = name;
        out.push({ name, state: st });
    });
    return out.slice(0, 100);
}

/// Save `state` as `name` (replacing a preset of that name), without its
/// speakers (`speakerFields`).
export function savePreset(list, name, state) {
    name = String(name || '').trim().slice(0, 60);
    if (!name || isReservedName(name)) return normalizePresets(list);
    const st = { ...withoutSpeakers(normalizeState(state)), name };
    const rest = normalizePresets(list).filter(p => p.name.toLowerCase() !== name.toLowerCase());
    return normalizePresets([...rest, { name, state: st }]);
}

export function renamePreset(list, from, to) {
    to = String(to || '').trim();
    const ps = normalizePresets(list);
    if (!to || isReservedName(to)) return ps;
    if (ps.some(p => p.name.toLowerCase() === to.toLowerCase() && p.name !== from)) return ps;
    return normalizePresets(ps.map(p => (p.name === from ? { name: to, state: { ...p.state, name: to } } : p)));
}

export function deletePreset(list, name) {
    return normalizePresets(list).filter(p => p.name !== name);
}
