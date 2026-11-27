// ══════════════════════════════════════════════════════════════════════
// The album's Convert, driven through the app's converter: the album joins
// the library's playlist (F's rows are what the converter converts), the
// converter's Output is set to the format asked for while the batch starts,
// and the listener's own Output is put back once it has started (the batch
// keeps the settings it started with).
// ══════════════════════════════════════════════════════════════════════

import { state } from '../state.js';
import { convertEntries } from '../dropzone.js';
import { setConvOutput } from '../converter.js';
import { convFmtValue } from '../dsdexport.js';
import { FS_PRESETS } from '../inventory.js';
import { playerStatus } from '../player.js';
import { convertPlan } from './convert-format.js';
import { queuePaths } from './play.js';
import { rateOfTag } from './model.js';

const $ = id => document.getElementById(id);
const load = k => { try { return localStorage.getItem(k); } catch (_) { return null; } };

/// The rack's FS now (the slider the player and the converter share).
export function rackFs() {
    const i = parseInt($('convFsSlider')?.value) || 0;
    return FS_PRESETS[Math.min(Math.max(0, i), FS_PRESETS.length - 1)];
}

/// The converter's Output as it is now ('flac' or '<multiple>:<kind>').
export const convOutputNow = () => convFmtValue($('convOutFormat')?.value, $('convDsdRate')?.value, $('convDsdModulator')?.value);

/// The Convert of the album `a` (its tag gives the rate family) as the
/// player is now; `chosen` is a format picked from the menu, or null.
export function planFor(a, chosen = null) {
    return convertPlan({
        status: playerStatus(),
        outFmt: load('auraPlayerOutputFormat') || 'pcm',
        bitPerfect: load('auraPlayerBitPerfect') === '1',
        fs: rackFs(),
        srcRate: rateOfTag(a?.tag),
        chosen,
    });
}

/// The engine's modulators for each DSD rate, asked once.
let mods = null;
export async function converterModulators() {
    if (mods) return mods;
    const invoke = window.__TAURI__?.core?.invoke;
    const rates = [64, 128, 256, 512, 1024];
    const lists = await Promise.all(rates.map(m => invoke?.('dsd_export_modulators', { multiple: m }).catch(() => []) ?? []));
    mods = Object.fromEntries(rates.map((m, i) => [m, lists[i] || []]));
    return mods;
}

/// Converts `paths` to `value`. Resolves to { started, busy, count,
/// unsupported }: busy when a batch already runs (nothing is changed then),
/// unsupported when the converter has no such modulator.
export async function convertPaths(paths, value) {
    if (state.convIsConverting) return { started: false, busy: true, count: 0 };
    queuePaths(paths);
    const want = new Set(paths);
    const entries = state.list.filter(e => want.has(e.path));
    if (!entries.length) return { started: false, busy: false, count: 0 };
    const before = convOutputNow();
    const swap = value && value !== before;
    try {
        if (swap) {
            await setConvOutput(value);
            // A modulator the converter does not have would make it FLAC.
            if (convOutputNow() !== value) return { started: false, busy: false, count: 0, unsupported: true };
        }
        await convertEntries(entries);
    } finally {
        if (swap) await setConvOutput(before);
    }
    return { started: true, busy: false, count: entries.length };
}
