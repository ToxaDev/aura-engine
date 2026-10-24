// ══════════════════════════════════════════════════════════════════════
// The format part of Convert all: the right side of the split button on
// the list's top bar, as the player's output format is beside its device.
// Closed it names the output ("FLAC", "DSD256"); open it lists FLAC and
// every DSD rate with its modulators. It is the rack's Output setting
// itself — a pick here sets the Output pane, a change there shows here.
// ══════════════════════════════════════════════════════════════════════

import { createPickList } from './pick-list.js';
import { DSD_RATES, convFmtValue, convFmtClosedLabel, convFmtItems, convFmtTip } from './dsdexport.js';
import { setConvOutput } from './converter.js';

const $ = id => document.getElementById(id);

let list = null;
// The engine's modulators for each DSD rate: they do not change while the
// app runs, so they are asked once.
let mods = null;
let asking = null;

function askMods() {
    if (mods) return Promise.resolve(mods);
    if (asking) return asking;
    const { invoke } = window.__TAURI__.tauri;
    asking = Promise.all(DSD_RATES.map(m =>
        invoke('dsd_export_modulators', { multiple: m }).catch(e => {
            console.warn('dsd_export_modulators failed', e);
            return [];
        }),
    )).then(lists => {
        mods = Object.fromEntries(DSD_RATES.map((m, i) => [m, lists[i]]));
        asking = null;
        renderConvFmt();
        return mods;
    });
    return asking;
}

function current() {
    return convFmtValue($('convOutFormat')?.value, $('convDsdRate')?.value, $('convDsdModulator')?.value);
}

/// The part as the Output pane is now: its text, its tip, lit for DSF.
export function renderConvFmt() {
    const btn = $('plCaFmt');
    if (!btn) return;
    const dsf = $('convOutFormat')?.value === 'dsf';
    const v = current();
    // The rate shows even before the modulators are back.
    const label = dsf ? `DSD${$('convDsdRate')?.value || 256}` : convFmtClosedLabel(v);
    const text = $('plCaFmtText');
    if (text && text.textContent !== label) text.textContent = label;
    btn.classList.toggle('on', dsf);
    const mod = $('convDsdModulator');
    btn.dataset.tip = convFmtTip(v, mod?.selectedOptions?.[0]?.textContent || '');
    if (list && mods) list.setItems(convFmtItems(mods), v);
}

/// Wires the part once the top bar is built.
export function initConvFmt() {
    const btn = $('plCaFmt');
    if (!btn || list) return;
    list = createPickList(btn, {
        className: 'pl-outfmt-list pl-cafmt-list',
        // Not while a batch runs: its files are already set.
        canOpen: () => !btn.classList.contains('busy'),
        onPick: v => { setConvOutput(v).then(renderConvFmt); },
        onToggle: open => { if (open) askMods(); },
    });
    // A change in the Output pane (or the restored settings) shows here.
    $('converterPanel')?.addEventListener('change', () => setTimeout(renderConvFmt, 0));
    window.addEventListener('aura-dsd-restored', () => setTimeout(renderConvFmt, 0));
    askMods();
    renderConvFmt();
}
