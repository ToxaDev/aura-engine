// ══════════════════════════════════════════════════════════════════════
// The rack's settings at a glance: one strip of five cells under the
// heading — FS Multiplier, Resolution, GPU, Apodizing, Headroom — each with
// its name and its value (Anton 2.10: the big sliders took a quarter of the
// window, and they are not changed every day). A cell opens its control
// under the strip, over Advanced DSP; a press elsewhere (and nothing more:
// popups.js), Esc or the same cell puts it away. GPU switches on a click.
//
// The controls are the same elements with the same ids as ever, moved into
// the strip's pane (components/converter.html): settings.js saves them, the
// converter and the player read them, the player's hold reaches them. This
// file shows them, lists the selects' choices, and keeps nothing of its own.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';
import { TAP_PRESETS, FS_PRESETS } from './inventory.js';
import { addPopup } from './popups.js';

// UI texts.
export const STRIP_TEXTS = {
    fs: 'FS Multiplier', taps: 'Resolution', gpu: 'GPU', apod: 'Apodizing', hr: 'Headroom',
    tapsUnit: 'taps', custom: 'Custom',
    on: 'On', off: 'Off', cpu: 'CPU', aa: 'AA',
    fsTip: rates => `FS Multiplier: the output rate, ${rates}. Click to change.`,
    tapsTip: len => `Filter resolution: ${len}. Click to change it or load a custom filter.`,
    gpuOn: 'GPU acceleration (Vulkan compute) is on. Click to switch it off.',
    gpuOff: 'GPU acceleration (Vulkan compute) is off. Click to switch it on.',
    gpuNone: why => `No usable GPU: ${why}. Conversions run on the CPU.`,
    apodTip: what => `Apodizing: ${what}. Click to change.`,
    hrTip: what => `Headroom: ${what}. Click to change.`,
    apodAa: 'Adaptive Apodizer (AA) is on: it measures each file and sets the cutoff itself. Switch AA off in Advanced DSP to choose one here.',
};

/// The selects' choices as the strip and its lists say them: [name, aside].
export const APOD_CHOICES = { '0': ['Off', ''], '1': ['Gentle', '20 kHz'], '2': ['Moderate', '19 kHz'], '3': ['Strong', '18 kHz'] };
export const HR_CHOICES = { '0': ['Off', '−0.5 dBTP, as shipped'], '-0.5': ['−0.5 dB', ''], '-1.0': ['−1.0 dB', ''], '-3.0': ['−3.0 dB', ''] };

export const CELLS = ['fs', 'taps', 'gpu', 'apod', 'hr'];

/// A tap count the way the strip writes it: 5k, 1M, 4.2M, 30M.
export function shortTaps(n) {
    const v = Number(n) || 0;
    if (v >= 1e6) { const m = v / 1e6; return (Number.isInteger(m) ? m.toFixed(0) : m.toFixed(1)) + 'M'; }
    if (v >= 1e3) return Math.round(v / 1e3) + 'k';
    return String(v);
}

const kHz = hz => { const k = hz / 1000; return Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1); };

/// The five cells for the rack as it is. `r`: { fs, taps, custom: { taps } |
/// null, gpu, gpuNone: why | null, aa, apod, hr, missing } — `missing`: no
/// filter on disk for this FS and length. Each cell: { id, label, value,
/// unit, cls, tip }.
export function stripCells(r) {
    const fs = Number(r.fs) || 8;
    const rates = `${kHz(44100 * fs)} / ${kHz(48000 * fs)} kHz`;
    const warn = r.missing ? 'warn' : '';
    const custom = r.custom && r.custom.taps > 0;
    const len = custom ? `${STRIP_TEXTS.custom} ${shortTaps(r.custom.taps)}` : `${shortTaps(r.taps)} ${STRIP_TEXTS.tapsUnit}`;
    const apod = APOD_CHOICES[r.apod] || APOD_CHOICES['0'];
    const hr = HR_CHOICES[r.hr] || HR_CHOICES['0'];
    const gpu = !r.gpu
        ? { value: STRIP_TEXTS.off, cls: 'dim', tip: STRIP_TEXTS.gpuOff }
        : r.gpuNone
            ? { value: STRIP_TEXTS.cpu, cls: 'bad', tip: STRIP_TEXTS.gpuNone(r.gpuNone) }
            : { value: STRIP_TEXTS.on, cls: 'lit', tip: STRIP_TEXTS.gpuOn };
    return [
        { id: 'fs', label: STRIP_TEXTS.fs, value: 'FS' + fs, unit: '', cls: warn, tip: STRIP_TEXTS.fsTip(rates) },
        { id: 'taps', label: STRIP_TEXTS.taps, value: custom ? len : shortTaps(r.taps), unit: custom ? '' : STRIP_TEXTS.tapsUnit,
            cls: warn || (custom ? 'custom' : ''), tip: STRIP_TEXTS.tapsTip(len) },
        { id: 'gpu', label: STRIP_TEXTS.gpu, value: gpu.value, unit: '', cls: gpu.cls, tip: gpu.tip },
        r.aa
            ? { id: 'apod', label: STRIP_TEXTS.apod, value: STRIP_TEXTS.aa, unit: '', cls: 'aa', tip: STRIP_TEXTS.apodAa }
            : { id: 'apod', label: STRIP_TEXTS.apod, value: apod[0], unit: '', cls: r.apod === '0' ? 'dim' : '',
                tip: STRIP_TEXTS.apodTip(apod[1] ? `${apod[0]} (${apod[1]})` : apod[0]) },
        hr[1] || r.hr === '0'
            ? { id: 'hr', label: STRIP_TEXTS.hr, value: hr[0], unit: '', cls: 'dim', tip: STRIP_TEXTS.hrTip(hr[1] ? `${hr[0]} (${hr[1]})` : hr[0]) }
            : { id: 'hr', label: STRIP_TEXTS.hr, value: hr[0].replace(/ dB$/, ''), unit: 'dB', cls: '', tip: STRIP_TEXTS.hrTip(hr[0]) },
    ];
}

// ── the page ──────────────────────────────────────────────────────────

const $ = id => document.getElementById(id);
const idx = (id, list) => Math.min(Math.max(0, parseInt($(id)?.value) || 0), list.length - 1);

/// The rack as the strip reads it.
function readRack() {
    return {
        fs: FS_PRESETS[idx('convFsSlider', FS_PRESETS)],
        taps: TAP_PRESETS[idx('convTapSlider', TAP_PRESETS)],
        custom: state.convCustomFilterPath ? { taps: state.convCustomFilterTaps } : null,
        gpu: !!$('convGpuCheck')?.checked,
        gpuNone: state.gpuNone || null,
        aa: !!$('convAdaptiveApodizer')?.checked,
        apod: $('convApodizing')?.value ?? '0',
        hr: $('convHeadroom')?.value ?? '0',
        missing: !!state.filterMissing,
    };
}

let strip = null;
let pop = null;
let open = null;      // the cell whose control shows

const setText = (el, t) => { if (el && el.textContent !== t) el.textContent = t; };

/// The strip after any change of the rack: the values, the cell that is
/// open, the choices ticked.
export function renderStrip() {
    if (!strip) return;
    for (const c of stripCells(readRack())) {
        const b = strip.querySelector(`.rk-cell[data-cell="${c.id}"]`);
        if (!b) continue;
        setText(b.querySelector('.rk-lbl'), c.label);
        setText(b.querySelector('.rk-v'), c.value);
        setText(b.querySelector('.rk-u'), c.unit);
        const cls = 'rk-cell' + (c.cls ? ' ' + c.cls : '') + (open === c.id ? ' open' : '');
        if (b.className !== cls) b.className = cls;
        if (b.dataset.tip !== c.tip) b.dataset.tip = c.tip;
        b.setAttribute('aria-label', `${c.label}: ${c.value}${c.unit ? ' ' + c.unit : ''}`);
        if (c.id !== 'gpu') b.setAttribute('aria-expanded', open === c.id ? 'true' : 'false');
    }
    for (const list of pop.querySelectorAll('.rk-opts')) {
        const sel = $(list.dataset.for);
        if (!sel) continue;
        list.classList.toggle('off', sel.disabled);
        for (const o of list.querySelectorAll('.rk-opt')) o.classList.toggle('on', o.dataset.value === sel.value);
    }
    const aa = pop.querySelector('.rk-why-aa');
    if (aa) {
        aa.hidden = !$('convAdaptiveApodizer')?.checked;
        setText(aa, STRIP_TEXTS.apodAa);
    }
}

/// A choice from a pane's list: the select set as a pick in it would set it,
/// with its change event, so every listener of the select hears it (the
/// specs, the saved settings, Adaptive Headroom's dependency). Nothing when
/// the select is locked (Apodizing while AA is on) or already says it.
export function pickChoice(sel, value) {
    if (!sel || sel.disabled || sel.value === value) return false;
    sel.value = value;
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
}

/// A select's choices listed in its pane; a pick sets the select and puts
/// the pane away.
function listChoices(list, names) {
    const sel = $(list.dataset.for);
    if (!sel) return;
    for (const opt of sel.options) {
        const [name, aside] = names[opt.value] || [opt.textContent, ''];
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'rk-opt';
        b.dataset.value = opt.value;
        b.innerHTML = '<span class="rk-tk">✓</span><span class="rk-nm"></span><span class="rk-as"></span>';
        b.querySelector('.rk-nm').textContent = name;
        b.querySelector('.rk-as').textContent = aside;
        b.addEventListener('click', () => {
            if (sel.disabled) return;
            pickChoice(sel, opt.value);
            close();
        });
        list.appendChild(b);
    }
}

function press(id, byKey) {
    if (id === 'gpu') {
        const cb = $('convGpuCheck');
        if (!cb) return;
        cb.checked = !cb.checked;
        cb.dispatchEvent(new Event('change', { bubbles: true }));
        close();
        renderStrip();
        return;
    }
    if (open === id) { close(); return; }
    show(id, byKey);
}

function show(id, byKey) {
    open = id;
    pop.dataset.cell = id;
    pop.hidden = false;
    place();
    renderStrip();
    // From the keyboard, into the pane: its first control.
    if (byKey) pop.querySelector(`.rk-pane[data-cell="${id}"]`)?.querySelector('input:not([type="hidden"]), button')?.focus();
}

export function closeStrip() { close(); }

function close() {
    if (!open) return;
    open = null;
    pop.hidden = true;
    delete pop.dataset.cell;
    renderStrip();
}

/// What a press is the open pane's own (popups.js): the pane itself and the
/// cells that open one — the same cell puts it away, another switches it.
/// GPU is a switch, not an opener: a press on it, as anywhere else, only
/// puts the pane away.
export function stripOwns(strip, pop, target) {
    if (pop.contains(target)) return true;
    const cell = target?.closest?.('.rk-cell');
    return !!cell && strip.contains(cell) && cell.dataset.cell !== 'gpu';
}

/// Under the strip, its arrow under the cell that opened it.
function place() {
    if (!open) return;
    pop.style.top = (strip.offsetTop + strip.offsetHeight + 7) + 'px';
    const cell = strip.querySelector(`.rk-cell[data-cell="${open}"]`);
    if (cell) pop.style.setProperty('--rk-caret', (cell.offsetLeft + cell.offsetWidth / 2) + 'px');
}

/// Build the cells and wire them. Does nothing where the page has no strip.
export function initRackStrip() {
    strip = $('rkStrip');
    pop = $('rkPop');
    if (!strip || !pop) return;
    for (const id of CELLS) {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'rk-cell';
        b.dataset.cell = id;
        b.innerHTML = '<span class="rk-lbl"></span><span class="rk-val"><span class="rk-v"></span><small class="rk-u"></small></span>';
        b.addEventListener('click', e => {
            if (e.detail > 0) b.blur();   // Space stays the player's
            press(id, e.detail === 0);
        });
        strip.appendChild(b);
    }
    for (const list of pop.querySelectorAll('.rk-opts')) {
        listChoices(list, list.dataset.for === 'convApodizing' ? APOD_CHOICES : HR_CHOICES);
    }
    // Away on a press anywhere else and on Esc — that press goes nowhere
    // else (popups.js) — and when the rack folds away.
    addPopup({ isOpen: () => !!open, inside: t => stripOwns(strip, pop, t), close });
    window.addEventListener('resize', place);
    new MutationObserver(() => { if (open && document.body.classList.contains('mode-listening')) close(); })
        .observe(document.body, { attributes: true, attributeFilter: ['class'] });
    // A control moved in its pane: the cell says so at once.
    for (const t of ['input', 'change']) {
        pop.addEventListener(t, renderStrip);
    }
    renderStrip();
}
