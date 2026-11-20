// ══════════════════════════════════════════════════════════════════════
// The converter's output format: FLAC, or DSF at a DSD rate through a
// modulator. The modulators are the engine's (`dsd_export_modulators`):
// built ones only, best first, in the order the engine lists them — a
// modulator that lands later appears here without a change to this file.
//
// The DSD rate follows the source's family (44.1 or 48 kHz times the
// multiple); the chain never crosses families. DSD64 runs the chain at FS8
// at most, so the modulator gets at least eight bits per PCM sample.
// ══════════════════════════════════════════════════════════════════════

// UI texts.
export const DSD_TEXTS = {
    output: 'Output', flac: 'FLAC', dsf: 'DSF (DSD)', rate: 'DSD rate', modulator: 'Modulator',
    offline: 'offline',
    offlineTip: 'Trellis modulators are far slower than real time: a track may take minutes.',
    none: 'No modulator is built for this rate yet.',
    outTip: what => `Output: ${what}. Click to change.`,
};

export const DSD_RATES = [64, 128, 256, 512, 1024];

const mhz = hz => (hz / 1e6).toFixed(4).replace(/0+$/, '').replace(/\.$/, '');

/// "2.8224 / 3.072 MHz": the rate in both families.
export function rateAside(multiple) {
    return `${mhz(44100 * multiple)} / ${mhz(48000 * multiple)} MHz`;
}

/// The FS the chain runs at for DSD`multiple` (DSD64: FS8 at most).
export function effectiveFs(fs, multiple) {
    return Math.max(1, Math.min(fs, Math.floor(multiple / 8)));
}

/// FLAC keeps the rate in 20 bits (up to 1 048 575 Hz): FS16 is its highest FS.
/// ×32 is for DSF and the player, which reads the same FS slider.
export const FLAC_MAX_FS = 16;
export const FLAC_FS_HINT = 'FLAC stores up to 768 kHz; ×32 is for DSD and the player';

/// Why the converter cannot write this FS to `out` (null = FLAC), or null.
export function flacFsRefusal(fs, out) {
    return !out && fs > FLAC_MAX_FS ? FLAC_FS_HINT : null;
}

/// The modulator choices for the engine's list, in its order: [value, name,
/// aside]. Converter-only (not real-time) kinds say "offline".
export function modulatorChoices(list) {
    return (list || []).map(m => [m.kind, m.label, m.realtime ? '' : DSD_TEXTS.offline]);
}

/// The modulator to select: the saved one while the rate has it, else the
/// best built one (the list's first). Null for an empty list.
export function pickModulator(list, saved) {
    if (!list || list.length === 0) return null;
    return list.some(m => m.kind === saved) ? saved : list[0].kind;
}

/// What `convert_files` takes as `dsdOutput`: null for FLAC.
export function dsdOutput(format, multiple, kind) {
    if (format !== 'dsf' || !kind) return null;
    const m = Number(multiple);
    return DSD_RATES.includes(m) ? { multiple: m, kind } : null;
}

/// The specs line's output part: "FLAC" or "DSF DSD256 &bull; SDM5".
export function specsOutput(out, label) {
    return out ? `DSF DSD${out.multiple} &bull; ${label || out.kind}` : 'FLAC';
}

// ── the format part of Convert all (the list's top bar) ──────────────
// The same choice as the Output pane, as one value: 'flac', or
// '<multiple>:<kind>' for DSF ('256:sdm7La').

/// The value for the Output pane's controls; 'flac' for FLAC or no modulator.
export function convFmtValue(format, multiple, kind) {
    const out = dsdOutput(format, multiple, kind);
    return out ? `${out.multiple}:${out.kind}` : 'flac';
}

/// A value's DSD rate and modulator; null for FLAC.
export function parseConvFmt(v) {
    const m = String(v || '').match(/^(\d+):(.+)$/);
    if (!m || !DSD_RATES.includes(Number(m[1]))) return null;
    return { multiple: Number(m[1]), kind: m[2] };
}

/// The closed part's text: "FLAC", or the rate alone ("DSD256"); the
/// modulator is in the open list and the tip.
export function convFmtClosedLabel(v) {
    const p = parseConvFmt(v);
    return p ? `DSD${p.multiple}` : DSD_TEXTS.flac;
}

/// The open list: FLAC in its own section, then DSD1024 … DSD64 - the
/// highest rate on top, as the player's format list puts the best first -
/// each rate's modulators as the engine lists them (best first), a
/// separator between the rates. `mods` maps the multiple to the engine's
/// list; a rate with none is left out. A modulator far slower than real
/// time is marked and says why in its tip. Items as pick-list.js takes them.
export function convFmtItems(mods) {
    const items = [{ value: 'flac', text: `${DSD_TEXTS.flac} 24-bit` }];
    for (const multiple of [...DSD_RATES].sort((a, b) => b - a)) {
        const list = mods?.[multiple] || [];
        if (list.length === 0) continue;
        items.push({ sep: true });
        for (const m of list) {
            const slow = !m.realtime;
            items.push({
                value: `${multiple}:${m.kind}`,
                text: `DSD${multiple} (${m.label})${slow ? ` · ${DSD_TEXTS.offline}` : ''}`,
                cls: slow ? 'heavy' : '',
                tip: slow ? DSD_TEXTS.offlineTip : '',
            });
        }
    }
    return items;
}

/// The part's tip: "Output: FLAC 24-bit. Click to change." or the DSF rate
/// and the modulator's name.
export function convFmtTip(v, label) {
    const p = parseConvFmt(v);
    return DSD_TEXTS.outTip(p ? `DSF DSD${p.multiple} · ${label || p.kind}` : `${DSD_TEXTS.flac} 24-bit`);
}

// ── the page ──────────────────────────────────────────────────────────

const $ = id => document.getElementById(id);

/// The current list from the engine, for the selected rate.
let modulators = [];

/// The output the converter's controls ask for, or null for FLAC.
export function getDsdOutput() {
    return dsdOutput($('convOutFormat')?.value, $('convDsdRate')?.value, $('convDsdModulator')?.value);
}

/// The label of the selected modulator.
export function selectedModulatorLabel() {
    const kind = $('convDsdModulator')?.value;
    return modulators.find(m => m.kind === kind)?.label || '';
}

/// Fills the modulator select (and its list in the pane, through `relist`)
/// from the engine for the selected rate; keeps `saved` when it is there.
export async function refreshModulators(invoke, relist, saved) {
    const sel = $('convDsdModulator');
    const rate = Number($('convDsdRate')?.value) || 256;
    if (!sel) return;
    try {
        modulators = await invoke('dsd_export_modulators', { multiple: rate });
    } catch (e) {
        console.warn('dsd_export_modulators failed', e);
        modulators = [];
    }
    const keep = pickModulator(modulators, saved ?? sel.value);
    sel.innerHTML = '';
    for (const [value, name] of modulatorChoices(modulators)) {
        const o = document.createElement('option');
        o.value = value;
        o.textContent = name;
        sel.appendChild(o);
    }
    sel.value = keep || '';
    const none = $('convDsdNone');
    if (none) {
        none.textContent = DSD_TEXTS.none;
        none.hidden = modulators.length > 0;
    }
    const offline = $('convDsdOffline');
    if (offline) {
        offline.textContent = DSD_TEXTS.offlineTip;
        offline.hidden = !modulators.some(m => !m.realtime);
    }
    relist?.(sel.id, Object.fromEntries(modulatorChoices(modulators).map(([v, n, a]) => [v, [n, a]])));
}

/// The DSD controls show only for DSF.
export function showDsdControls() {
    const dsf = $('convOutFormat')?.value === 'dsf';
    for (const el of document.querySelectorAll('.dsd-only')) el.hidden = !dsf;
}
