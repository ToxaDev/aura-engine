// ══════════════════════════════════════════════════════════════════════
// An album's Convert in the library makes what the listener hears now
// (Anton's decision): the player's output as it plays. PCM out of the rack
// is a FLAC at the rack's FS; DSD out of it (DSD256 by a modulator) is a
// DSD256 .dsf by the same modulator. The button names what comes out
// ("Convert → FLAC 705.6k", "Convert → DSD256"); another format picked from
// its menu is said to be not what plays now.
//
// The values are the converter's own (dsdexport.js): 'flac', or
// '<multiple>:<kind>' for a DSF ('256:sdm7La').
// ══════════════════════════════════════════════════════════════════════

import { parseOutFmt, rateBase, kindLabel } from '../outfmt.js';
import { parseConvFmt, FLAC_MAX_FS, FLAC_FS_HINT } from '../dsdexport.js';

// UI texts.
export const CONVERT_TEXTS = {
    convert: 'Convert',
    notNow: '≠ now playing',
    heardTip: 'Converts the album to what you hear now: the player\'s output through the rack.',
    otherTip: 'Not what plays now: the player\'s output is {heard}. The menu has it first.',
    bitPerfect: 'Bit-perfect plays the files as they are; this is the rack\'s FLAC.',
    flacMax: FLAC_FS_HINT,
};

/// "705.6k", "768k", "1411.2k".
export function kHz(hz) {
    const k = hz / 1000;
    return `${Number.isInteger(k) ? k : k.toFixed(1).replace(/\.0$/, '')}k`;
}

/// The DSD rate of a playing DSD output's label ("DSD256", "DSD256 (48k)"),
/// or 0.
export function multipleOfLabel(label) {
    const m = String(label || '').match(/DSD(\d+)/i);
    return m ? Number(m[1]) : 0;
}

/// What the player sends out now, as the converter's value. `status` is the
/// player's status (its `dsd` while DSD plays), `outFmt` the stored output
/// choice ('pcm' or '256:sdm7La', what plays when nothing plays yet),
/// `bitPerfect` the player's bit-perfect switch. A DSD file played as it is
/// is not the rack's: that is FLAC too.
export function heardValue({ status = null, outFmt = 'pcm', bitPerfect = false } = {}) {
    if (bitPerfect) return 'flac';
    const d = status?.dsd;
    const playing = status && status.state && status.state !== 'stopped';
    if (playing && d && !d.asIs) {
        const m = multipleOfLabel(d.label);
        const kind = d.modulator || d.running;
        if (m && kind) return `${m}:${kind}`;
    }
    if (playing && status.outRate > 0) return 'flac';
    const p = parseOutFmt(outFmt);
    return p ? `${p.multiple}:${p.kind}` : 'flac';
}

/// The FS a FLAC is made at: the rack's, at most what FLAC holds.
export const flacFs = fs => Math.min(Math.max(1, Number(fs) || 1), FLAC_MAX_FS);

/// A value's short name for the button: "FLAC 705.6k" (the album's family
/// times the FS), "DSD256".
export function targetLabel(value, { fs = 16, srcRate = 0 } = {}) {
    const p = parseConvFmt(value);
    if (p) return `DSD${p.multiple}`;
    return `FLAC ${kHz(rateBase(srcRate) * flacFs(fs))}`;
}

/// A value's long name for the menu and the tip: "FLAC 24-bit · 705.6k",
/// "DSD256 · SDM7 LA".
export function targetLong(value, opts = {}) {
    const p = parseConvFmt(value);
    if (p) return `DSD${p.multiple} · ${kindLabel(p.kind)}`;
    return `FLAC 24-bit · ${targetLabel(value, opts).slice(5)}`;
}

/// The album's Convert as the player is now: `chosen` is the format picked
/// from the menu (null: what is heard). Returns the value to convert to, the
/// button's text, whether it differs from what is heard, and the tip.
export function convertPlan({ status = null, outFmt = 'pcm', bitPerfect = false, fs = 16, srcRate = 0, chosen = null } = {}) {
    const heard = heardValue({ status, outFmt, bitPerfect });
    const value = chosen || heard;
    const opts = { fs, srcRate };
    const label = targetLabel(value, opts);
    const flacTooHigh = !parseConvFmt(value) && Number(fs) > FLAC_MAX_FS;
    const differs = value !== heard || bitPerfect || flacTooHigh;
    const notes = [];
    if (value !== heard) notes.push(CONVERT_TEXTS.otherTip.replace('{heard}', targetLong(heard, opts)));
    else notes.push(CONVERT_TEXTS.heardTip);
    if (bitPerfect) notes.push(CONVERT_TEXTS.bitPerfect);
    if (flacTooHigh) notes.push(CONVERT_TEXTS.flacMax);
    return {
        value,
        heard,
        label,
        text: `${CONVERT_TEXTS.convert} → ${label}`,
        differs,
        mark: differs ? CONVERT_TEXTS.notNow : '',
        tip: `${targetLong(value, opts)}\n${notes.join('\n')}`,
    };
}

/// The menu's items: what is heard first, then FLAC and each DSD rate with
/// its modulators (`mods`: the converter's lists by multiple, best first).
/// Each { value, text, heard } or { sep: true }.
export function formatMenu(mods, heard, opts = {}) {
    const groups = [];
    if (heard !== 'flac') groups.push([{ value: 'flac', text: targetLong('flac', opts) }]);
    for (const m of [1024, 512, 256, 128, 64]) {
        const list = (mods?.[m] || []).filter(k => `${m}:${k.kind}` !== heard);
        if (list.length) groups.push(list.map(k => ({
            value: `${m}:${k.kind}`,
            text: `DSD${m} · ${k.label || kindLabel(k.kind)}${k.realtime === false ? ' · offline' : ''}`,
        })));
    }
    const items = [{ value: heard, text: `${targetLong(heard, opts)} — now playing`, heard: true }];
    for (const g of groups) items.push({ sep: true }, ...g);
    return items;
}
