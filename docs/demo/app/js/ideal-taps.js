// The Auto position of the tap slider: the ideal filter's size for the track
// that plays.
//
// Auto plays a filter the engine makes by formula for each track (Rust:
// audio/ideal.rs): a pure sinc with no window. A track of length T only ever
// meets the taps at lags within ±T, so the filter that is exactly enough for
// it has 2 · T · rate taps at the output rate; more taps change no sample.
// The player's status names the size of the chain heard ("423M"); this
// module gives the same number where the rack shows its own setting, before
// the chain is heard.

import { formatTaps } from './helpers.js';
import { gridGraphSvg, GRID_EXPLAIN } from './grid-tip.js';

/// The slider positions that play the ideal filter. The engine knows one tap
/// count, Auto (ideal.rs AUTO_TAPS), and a grid switch beside it (the rack's
/// `autoShift`); the slider shows the two grids as two positions:
///   Auto       the odd length: every L-th output sample on a source sample;
///   Auto Shift the even length: the output grid half an output sample later,
///              no output sample on a source sample (as the stored rungs).
/// IDEAL_SHIFT_SLOT is a UI value only: `engineTaps` turns it into the pair.
export const IDEAL_SLOT = 2_000_000_000;
export const IDEAL_SHIFT_SLOT = 2_000_000_001;
/// The label the engine gives a chain at Auto before a track sizes it (filter.rs taps_label).
export const IDEAL_SLOT_LABEL = 'Auto';
/// Whether the rack's taps ask for a track's own filter.
export const isIdealSlot = (taps) => Number(taps) === IDEAL_SLOT || Number(taps) === IDEAL_SHIFT_SLOT;
/// UI text: the positions' names.
export const IDEAL_NAME = 'Auto';
export const IDEAL_SHIFT_NAME = 'Auto S';
/// The slider value's name (Auto / Auto Shift), null for a stored rung.
export const idealName = (taps) => (Number(taps) === IDEAL_SHIFT_SLOT ? IDEAL_SHIFT_NAME
    : Number(taps) === IDEAL_SLOT ? IDEAL_NAME : null);

/// What the engine takes for a slider value: { taps, autoShift }. Every
/// rung but Auto sends the grid's default (it acts on Auto only).
export function engineTaps(v) {
    const n = Number(v);
    if (n === IDEAL_SHIFT_SLOT) return { taps: IDEAL_SLOT, autoShift: true };
    if (n === IDEAL_SLOT) return { taps: IDEAL_SLOT, autoShift: false };
    return { taps: n, autoShift: true };
}

/// The stored rungs' labels (filter.rs taps_label): a chain's size that is
/// none of them is a track's own.
/// (100M..2G: rungs of older installs, still a stored file's size.)
const RUNG_LABELS = new Set(['5k', '1M', '5M', '10M', '30M', '100M', '180M', '300M', '500M', '1G', '2G']);
/// Whether a chain's taps label is a track's own size, not a stored rung's.
export const isOwnSize = (label) => typeof label === 'string' && label !== '' && label !== IDEAL_SLOT_LABEL && !RUNG_LABELS.has(label);

/// The ideal filter's rate: the output rate, `fs` × the source's family base.
export const idealRate = (srcRate, fs = 16) => (srcRate % 11025 === 0 ? 44100 : 48000) * fs;

/// The taps the ideal filter has for a track `durationS` long at `srcRate`,
/// played at ×`fs` on the grid `shifted` (Auto Shift: the even length, one
/// tap fewer; ideal.rs linear_taps): { taps, rate, durationS, shifted },
/// null when length or rate is unknown.
export function idealTapsFor(durationS, srcRate, fs = 16, shifted = false) {
    if (!(durationS > 0) || !(srcRate > 0)) return null;
    const rate = idealRate(srcRate, fs);
    return { taps: 2 * Math.round(durationS * rate) + (shifted ? 0 : 1), rate, durationS, shifted: !!shifted };
}

/// A tap count the way the chip writes it: 86M, 423M, 1.27G.
export function idealShort(n) {
    if (n >= 999.5e6) return (n / 1e9).toFixed(2) + 'G';
    if (n >= 1e6) return Math.round(n / 1e6) + 'M';
    return Math.max(1, Math.round(n / 1e3)) + 'k';
}

let track = null;
const listeners = new Set();

/// The track that plays (null, or its idealTapsFor).
export const idealTrack = () => track;

/// Tell the module what plays: a file's length and rate, or nothing (0, 0),
/// and the rack's FS and grid. Listeners hear of it when the number shown changes.
export function setIdealTrack(durationS, srcRate, fs = 16, shifted = false) {
    const next = idealTapsFor(durationS, srcRate, fs, shifted);
    if ((next?.taps ?? null) === (track?.taps ?? null) && (next?.rate ?? null) === (track?.rate ?? null)) return;
    track = next;
    for (const fn of listeners) { try { fn(track); } catch { /* a listener's own trouble */ } }
}

export function onIdealChange(fn) { listeners.add(fn); return () => listeners.delete(fn); }

/// The superscript after a size on the shifted grid ("423Mˢ"): Auto Shift
/// at a glance; plain Auto writes the size alone.
export const SHIFT_MARK = 'ˢ';

/// The playing track's size as shown: with the shift mark on Auto Shift.
const trackShort = (t) => idealShort(t.taps) + (t.shifted ? SHIFT_MARK : '');

/// The shift mark on screen: a real superscript "s", raised to the top of the
/// digits and large enough to read as an s (.shift-s), not the small
/// modifier letter that sits mid-height. `html`: text already escaped.
export const markShiftHtml = (html) => String(html).split(SHIFT_MARK).join('<sup class="shift-s">s</sup>');

/// `text` into `el` with the shift mark drawn as markShiftHtml does
/// (unchanged text: nothing is touched). Its aria-label keeps the plain text.
export function setMarkedText(el, text) {
    if (!el) return;
    text = String(text ?? '');
    if (el.dataset.markedText === text) return;
    el.dataset.markedText = text;
    if (!text.includes(SHIFT_MARK)) { el.textContent = text; return; }
    el.textContent = '';
    const parts = text.split(SHIFT_MARK);
    parts.forEach((p, i) => {
        if (p) el.appendChild(document.createTextNode(p));
        if (i < parts.length - 1) {
            const s = document.createElement('sup');
            s.className = 'shift-s';
            s.textContent = 's';
            el.appendChild(s);
        }
    });
}

/// The slot's short label: the playing track's size, or the position's name.
/// (`taps`: the rack's value, to name Auto Shift as such.)
export const idealSlotLabel = (taps) => (track ? trackShort(track) : (idealName(taps) ?? IDEAL_NAME));

/// A chain's taps label as shown: the slot's "Auto" becomes the track's size.
/// The engine's own size of the playing track ("281M") carries the shift mark
/// on Auto Shift, as the read-out and the strip do.
export const shownTapsLabel = (label) => (label === IDEAL_SLOT_LABEL ? idealSlotLabel()
    : (track?.shifted && isOwnSize(label) && label === idealShort(track.taps)) ? label + SHIFT_MARK : label);

/// Whether a chain labelled `built` (from `source`) is what a rack on Auto
/// asks for: a track's own size, or a stream or a live input (no length to
/// size a filter for: the longest stored rung plays).
export const autoBuilt = (built, source) => built === IDEAL_SLOT_LABEL || isOwnSize(built) || (source != null && source !== 'file');

/// The slider's read-out for a rung of `taps`: "30.0M Taps"; on Auto the
/// position's name with nothing playing ("Auto S"), the track's size
/// while it plays ("423Mˢ Taps · Auto S"; on plain Auto "423M Taps · Auto").
export function tapsReadout(taps) {
    const name = idealName(taps);
    if (!name) return formatTaps(taps) + ' Taps';
    return track ? `${trackShort(track)} Taps · ${name}` : name;
}

/// A plain text read-out ("30.0M Taps"): no picture, no tooltip.
export function paintTapsText(el, text) {
    if (!el) return;
    el.classList.remove('tap-graph');
    delete el.dataset.markedText;
    el.textContent = text;
    delete el.dataset.tip;
    delete el.dataset.tipGrid;
}

/// Write the read-out for `taps` into `el`. A fixed size is its text; on
/// Auto / Auto Shift the grid's picture (grid-tip.js) with the read-out as a
/// small caption under it, and the two grids explained in its tooltip.
export function paintTapsReadout(el, taps) {
    if (!el) return;
    const name = idealName(taps);
    if (!name) return paintTapsText(el, tapsReadout(taps));
    const mode = name === IDEAL_SHIFT_NAME ? 'shift' : 'auto';
    el.classList.add('tap-graph');
    el.innerHTML = gridGraphSvg(mode) + '<span class="tap-graph-cap"></span>';
    setMarkedText(el.lastChild, tapsReadout(taps));
    el.dataset.tip = GRID_EXPLAIN[mode];
    delete el.dataset.tipGrid;
}

const fmtLen = (s) => {
    const t = Math.round(s);
    return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, '0')}`;
};

/// The tooltip of the ideal size (null when no track plays).
export function idealTip() {
    if (!track) return null;
    const n = track.taps.toLocaleString('en-US');
    const rate = `${track.rate / 1000} kHz`;
    return `Ideal filter for this track: ${n} taps = 2 × ${fmtLen(track.durationS)} × ${rate}, made by formula. `
        + 'A pure sinc with no window; more taps would not change a single sample of this track.';
}
