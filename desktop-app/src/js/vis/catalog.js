
// ══════════════════════════════════════════════════════════════════════
// The visualization scenes there are — one list, the user's folder
// (vis_store.rs: %LOCALAPPDATA%\AuraEngine\visualizations; the app's own
// scenes are put there on the first start) — and each scene's slider
// values (localStorage — shared by the player's window and the studio,
// both pages of the app).
//
// A scene's id: "my:<file>". As a look of the player it is
// "vis:<id>" (waves.js).
//
// Between the windows (Tauri events):
//   vis:changed          a scene saved, added or removed — read the list again
//   vis:params {id, values}   sliders moved in the studio — draw with these
//   vis:revert {id}      the studio closed without saving — back to the kept values
//   vis:code {id, text}  the studio's current code (compiled there) — draw it
//   vis:look {id}        show this scene in the player
// ══════════════════════════════════════════════════════════════════════

import { parseHeader, valuesOf } from './scene.js';

const T = () => window.__TAURI__;
const VALUES = 'auraVis:values:';

/// Every scene: [{ id, file, text, mine, header }] — one list, the user's
/// folder (the app's own scenes are put there on the first start; any of
/// them can be removed), by name.
export async function loadCatalog() {
    const out = [];
    try {
        const mine = await T().tauri.invoke('vis_list');
        for (const m of mine || []) out.push({ id: 'my:' + m.file, file: m.file, text: m.text, mine: true, header: parseHeader(m.text) });
    } catch (_) {}
    out.sort((a, b) => a.header.name.localeCompare(b.header.name));
    return out;
}

export function keptValues(id) {
    try { return JSON.parse(localStorage.getItem(VALUES + id) || 'null') || {}; } catch (_) { return {}; }
}
export function keepValues(id, values) {
    try { localStorage.setItem(VALUES + id, JSON.stringify(values)); } catch (_) {}
}
export function forgetValues(id) {
    try { localStorage.removeItem(VALUES + id); } catch (_) {}
}
/// A scene's values as they are kept (its defaults where none were).
export const valuesFor = (entry) => valuesOf(entry.header, keptValues(entry.id));

export const emit = (name, payload) => T()?.event.emit(name, payload).catch(() => {});
export const listen = (name, fn) => T()?.event.listen(name, m => fn(m.payload || {})).catch(() => {});
