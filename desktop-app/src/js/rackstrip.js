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
import { isIdealSlot, idealSlotLabel, setMarkedText, chainFs } from './ideal-taps.js';
import { TAP_PRESETS, FS_PRESETS } from './inventory.js';
import { addPopup } from './popups.js';
import { DSD_TEXTS, rateAside } from './dsdexport.js';
import { dsdPlaying, dsdNative } from './outfmt.js';
import { versionViewNow, onVersionView, tapsText, VERSION_TEXTS } from './version-view.js';

// UI texts.
export const STRIP_TEXTS = {
    fs: 'FS Multiplier', taps: 'Resolution', gpu: 'GPU', apod: 'Apodizing', hr: 'Headroom',
    tapsUnit: 'taps', custom: 'Custom',
    on: 'On', off: 'Off', cpu: 'CPU', aa: 'AA',
    fsTip: rates => `FS Multiplier: the output rate, ${rates}. Click to change.`,
    tapsTip: len => `Filter resolution: ${len}. Click to change it or load a custom filter.`,
    // The player plays fewer taps than the rack's: `asked` → `to`.
    tapsLowered: (asked, to, why) =>
        `The player plays ${to} taps, not the ${asked} chosen: ${why === 'gpu-failed'
            ? 'the graphics card failed and this CPU cannot run'
            : 'this computer cannot run'} ${asked} in real time right now. ` +
        `Untick "Lower taps when the CPU can't keep up" in this pane to hold ${asked}. Click to change.`,
    lowerTaps: "Lower taps when the CPU can't keep up",
    lowerTapsTip: 'On: when this computer cannot render the chosen filter in real time (another program ' +
        'busy, a slow CPU, the graphics card failing), the player switches to a shorter one and keeps playing ' +
        'without a gap; the Resolution cell then shows both, e.g. 30M → 10M. Off: the player holds the chosen ' +
        'taps whatever the load — on a PC that cannot keep up the sound may stutter or drop out instead. ' +
        'Conversions are not affected.',
    auto: 'Auto',
    // GPU is Auto: no switch. What the card is, and when it is used.
    gpuAuto: info => `GPU: Auto. ${gpuFound(info)} The long filter runs on the CPU while it keeps up; ` +
        'the graphics card takes over by itself when it cannot. Click to look for the card again.',
    gpuNone: why => `GPU: Auto. No usable graphics card: ${why}. Everything runs on the CPU.`,
    apodTip: what => `Apodizing: ${what}. Click to change.`,
    hrTip: what => `Headroom: ${what}. Click to change.`,
    // In listening mode the FS and Output cells say what the player plays.
    setByDsd: 'set by DSD', native: 'native', dop: 'DoP', pcm: 'PCM',
    fsDsdTip: (fs, rate, dsd, rack) => `FS Multiplier: the chain runs at ×${fs} (${rate}) for ${dsd}: ` +
        `a DSD output sets its own FS${fs === rack ? '' : `, the rack's FS${rack} is for PCM`}. Click to change the rack's.`,
    fsPcmTip: (fs, rate) => `FS Multiplier: the player plays at ×${fs}, ${rate}. Click to change.`,
    outDsdTip: (label, native, mhz) => `Output: the player sends ${label} (${mhz} MHz) ${native
        ? "natively through the DAC's ASIO driver" : 'as DoP'}. The converter's format is on the Convert button. Click to change.`,
    outPcmTip: rate => `Output: the player sends PCM at ${rate}. The converter's format is on the Convert button. Click to change.`,
    apodAa: 'Adaptive Apodizer (AA) is on: it measures each file and sets the cutoff itself. Switch AA off in Advanced DSP to choose one here.',
};

/// The selects' choices as the strip and its lists say them: [name, aside].
export const APOD_CHOICES = { '0': ['Off', ''], '1': ['Gentle', '20 kHz'], '2': ['Moderate', '19 kHz'], '3': ['Strong', '18 kHz'] };
export const HR_CHOICES = { '0': ['Off', '−0.5 dBTP, as shipped'], '-0.5': ['−0.5 dB', ''], '-1.0': ['−1.0 dB', ''], '-3.0': ['−3.0 dB', ''] };

export const CELLS = ['fs', 'taps', 'gpu', 'apod', 'hr', 'out'];

/// The output choices: FLAC or DSF, and the DSD rates with both families.
export const OUT_CHOICES = { flac: [DSD_TEXTS.flac, ''], dsf: [DSD_TEXTS.dsf, ''] };
export const DSD_RATE_CHOICES = Object.fromEntries([64, 128, 256, 512, 1024].map(m => [String(m), ['DSD' + m, rateAside(m)]]));

/// A tap count the way the strip writes it: 5k, 1M, 4.2M, 30M, 500M.
export function shortTaps(n) {
    const v = Number(n) || 0;
    if (v >= 1e9) { const g = v / 1e9; return (Number.isInteger(g) ? g.toFixed(0) : g.toFixed(1)) + 'G'; }
    if (v >= 1e6) { const m = v / 1e6; return (Number.isInteger(m) ? m.toFixed(0) : m.toFixed(1)) + 'M'; }
    if (v >= 1e3) return Math.round(v / 1e3) + 'k';
    return String(v);
}

/// The rack's rung as the strip writes it: the ideal position shows the
/// playing track's size (ideal-taps.js).
const rungTaps = (n) => (isIdealSlot(n) ? idealSlotLabel(n) : shortTaps(n));

/// The card `gpu_info` found, in a sentence (unknown before the first ask).
export function gpuFound(info) {
    if (!info) return 'Point here to see which graphics card was found.';
    if (!info.found) return `No graphics card the engine can use${info.reason ? ` (${info.reason})` : ''}.`;
    const gb = mb => (mb / 1024).toFixed(mb >= 10240 ? 0 : 1);
    const mem = info.vramMb ? `, ${gb(info.vramMb)} GB${info.freeMb != null ? ` (${gb(info.freeMb)} GB free)` : ''}` : '';
    const ok = !info.supported ? `not supported: ${info.reason || 'unknown'}`
        : info.player ? 'supported' : 'supported by the converter only (the player needs a discrete card)';
    return `Found: ${info.name}${mem}; ${ok}.`;
}

const kHz = hz => { const k = hz / 1000; return Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1); };

/// The five cells for the rack as it is. `r`: { fs, taps, custom: { taps } |
/// null, gpu, gpuNone: why | null, aa, apod, hr, missing, out: { multiple,
/// label } | null (FLAC), lowered: { asked, to, why } | null } — `missing`:
/// no filter on disk for this FS and length; `lowered`: the player plays
/// fewer taps than asked (`setStripLowered`). Each cell: { id, label, value,
/// unit, cls, tip }.
export function stripCells(r) {
    const fs = Number(r.fs) || 8;
    const rates = `${kHz(44100 * fs)} / ${kHz(48000 * fs)} kHz`;
    const warn = r.missing ? 'warn' : '';
    const custom = r.custom && r.custom.taps > 0;
    // The cell shows the size alone; the word "taps" stays in its tooltip.
    const len = custom ? `${STRIP_TEXTS.custom} ${shortTaps(r.custom.taps)}`
        : isIdealSlot(r.taps) ? `${rungTaps(r.taps)} (${STRIP_TEXTS.tapsUnit} sized to each track)`
        : `${rungTaps(r.taps)} ${STRIP_TEXTS.tapsUnit}`;
    const apod = APOD_CHOICES[r.apod] || APOD_CHOICES['0'];
    const hr = HR_CHOICES[r.hr] || HR_CHOICES['0'];
    const gpu = r.gpuNone
        ? { value: STRIP_TEXTS.cpu, cls: 'dim', tip: STRIP_TEXTS.gpuNone(r.gpuNone) }
        : { value: STRIP_TEXTS.auto, cls: 'lit', tip: STRIP_TEXTS.gpuAuto(r.gpuInfo) };
    const p = r.playing;
    const cells = [
        { id: 'fs', label: STRIP_TEXTS.fs, value: 'FS' + fs, unit: '', cls: warn, tip: STRIP_TEXTS.fsTip(rates) },
        r.lowered && !custom
            ? { id: 'taps', label: STRIP_TEXTS.taps, value: `${r.lowered.asked} → ${r.lowered.to}`, unit: '',
                cls: warn || 'lowered', tip: STRIP_TEXTS.tapsLowered(r.lowered.asked, r.lowered.to, r.lowered.why) }
            : { id: 'taps', label: STRIP_TEXTS.taps, value: custom ? len : rungTaps(r.taps), unit: '',
                cls: warn || (custom ? 'custom' : ''), tip: STRIP_TEXTS.tapsTip(len) },
        { id: 'gpu', label: STRIP_TEXTS.gpu, value: gpu.value, unit: '', cls: gpu.cls, tip: gpu.tip },
        r.aa
            ? { id: 'apod', label: STRIP_TEXTS.apod, value: STRIP_TEXTS.aa, unit: '', cls: 'aa', tip: STRIP_TEXTS.apodAa }
            : { id: 'apod', label: STRIP_TEXTS.apod, value: apod[0], unit: '', cls: r.apod === '0' ? 'dim' : '',
                tip: STRIP_TEXTS.apodTip(apod[1] ? `${apod[0]} (${apod[1]})` : apod[0]) },
        hr[1] || r.hr === '0'
            ? { id: 'hr', label: STRIP_TEXTS.hr, value: hr[0], unit: '', cls: 'dim', tip: STRIP_TEXTS.hrTip(hr[1] ? `${hr[0]} (${hr[1]})` : hr[0]) }
            : { id: 'hr', label: STRIP_TEXTS.hr, value: hr[0].replace(/ dB$/, ''), unit: 'dB', cls: '', tip: STRIP_TEXTS.hrTip(hr[0]) },
        r.out
            ? { id: 'out', label: DSD_TEXTS.output, value: 'DSD' + r.out.multiple, unit: '', cls: 'lit',
                tip: DSD_TEXTS.outTip(`DSF, DSD${r.out.multiple}${r.out.label ? ', ' + r.out.label : ''}`) }
            : { id: 'out', label: DSD_TEXTS.output, value: DSD_TEXTS.flac, unit: '', cls: '', tip: DSD_TEXTS.outTip(DSD_TEXTS.flac) },
    ];
    // A version on the air (version-view.js): its size as its name spells
    // it, and every cell says whose values these are.
    if (r.tapsLabel) {
        cells[1] = { id: 'taps', label: STRIP_TEXTS.taps, value: r.tapsLabel, unit: '', cls: '', tip: VERSION_TEXTS.tip };
        for (const c of cells) c.tip = `${VERSION_TEXTS.head}: ${c.label} ${c.value}${c.unit ? ' ' + c.unit : ''}. ${VERSION_TEXTS.tip}`;
    }
    if (!p) return cells;
    // Listening: what plays (Anton 20.11, variant C). The FS the chain runs
    // at, marked when a DSD output set it apart from the rack's; the
    // player's output — DSD and how it goes out, or PCM and its rate.
    const rate = kHz(p.rate) + ' kHz';
    cells[0] = p.dsd
        ? { id: 'fs', label: STRIP_TEXTS.fs, value: 'FS' + p.fs, unit: '', sub: p.fs !== fs ? STRIP_TEXTS.setByDsd : '',
            cls: warn || (p.fs !== fs ? 'set' : ''), tip: STRIP_TEXTS.fsDsdTip(p.fs, rate, p.dsd.label, fs) }
        : { id: 'fs', label: STRIP_TEXTS.fs, value: 'FS' + p.fs, unit: '', cls: warn, tip: STRIP_TEXTS.fsPcmTip(p.fs, rate) };
    cells[5] = p.dsd
        ? { id: 'out', label: DSD_TEXTS.output, value: 'DSD' + p.dsd.multiple, unit: '', sub: p.dsd.native ? STRIP_TEXTS.native : STRIP_TEXTS.dop,
            cls: 'dsd', tip: STRIP_TEXTS.outDsdTip(p.dsd.label, p.dsd.native, (p.dsd.multiple * p.dsd.family / 1e6).toFixed(2)) }
        : { id: 'out', label: DSD_TEXTS.output, value: STRIP_TEXTS.pcm, unit: '', sub: kHz(p.rate) + 'k',
            cls: 'lit', tip: STRIP_TEXTS.outPcmTip(rate) };
    return cells;
}

/// What the player plays, as the strip and the heading say it in listening
/// mode, from a status (`player_status`): { fs, rate (the chain's, Hz), dsd:
/// { label, multiple, family, native } | null }; null when nothing is on the
/// air, under BIT-PERFECT (no chain) or outside listening mode — the rack
/// then says what a conversion writes. `rackFs`: the rack's FS; `deviceId`:
/// the output device ("asio:…" sends DSD natively).
export function stripPlaying(status, { listening, bitPerfect, rackFs, deviceId }) {
    if (!listening || bitPerfect || !status || !['playing', 'paused'].includes(status.state)) return null;
    const dsd = dsdPlaying(status.dsd);
    const fam = hz => (hz % 11025 === 0 ? 44100 : 48000);
    if (dsd) {
        // The chain runs on the track's family; a 48 kHz family track steps
        // down to the 44.1 kHz family's DSD only before the modulator.
        const fs = chainFs(rackFs, dsd.multiple);
        const src = Number(status.srcRate) || dsd.family;
        return { fs, rate: fam(src) * fs, dsd: { ...dsd, native: dsdNative(deviceId, status.format) } };
    }
    const out = Number(status.outRate) || 0;
    if (!out) return null;
    return { fs: Math.max(1, Math.round(out / fam(out))), rate: out, dsd: null };
}

// ── the page ──────────────────────────────────────────────────────────

const $ = id => document.getElementById(id);
const idx = (id, list) => Math.min(Math.max(0, parseInt($(id)?.value) || 0), list.length - 1);

/// The rack as the strip reads it; a version on the air instead, as it
/// was made (version-view.js).
function readRack() {
    const r = readOwnRack();
    const v = versionViewNow();
    if (!v) return r;
    return {
        ...r, fs: v.fs || r.fs, taps: null, tapsLabel: tapsText(v), custom: null,
        aa: v.aa, apod: v.apod, hr: v.hr, missing: false, lowered: null, playing: null, out: v.out,
    };
}

function readOwnRack() {
    return {
        fs: FS_PRESETS[idx('convFsSlider', FS_PRESETS)],
        taps: TAP_PRESETS[idx('convTapSlider', TAP_PRESETS)],
        custom: state.convCustomFilterPath ? { taps: state.convCustomFilterTaps } : null,
        gpuNone: state.gpuNone || null,
        gpuInfo: state.gpuInfo || null,
        aa: !!$('convAdaptiveApodizer')?.checked,
        apod: $('convApodizing')?.value ?? '0',
        hr: $('convHeadroom')?.value ?? '0',
        missing: !!state.filterMissing,
        lowered,
        playing,
        out: $('convOutFormat')?.value === 'dsf'
            ? { multiple: Number($('convDsdRate')?.value) || 256, label: $('convDsdModulator')?.selectedOptions?.[0]?.textContent || '' }
            : null,
    };
}

let strip = null;
let pop = null;
let open = null;      // the cell whose control shows
let lowered = null;   // the player's stepped-down taps: { asked, to, why } | null
let playing = null;   // what the player plays in listening mode (stripPlaying) | null
let playingHook = null;

/// The player's stepped-down taps in a status (`player_status`), or null:
/// a track or stream on the air (playing or paused) whose audible chain
/// carries a downgrade — what was asked, what plays, why.
export function loweredTaps(status) {
    const dg = status?.audible?.downgrade;
    if (!dg || !['playing', 'paused'].includes(status.state)) return null;
    const to = status.audible.taps || dg.to;
    const asked = dg.asked || dg.from;
    if (!to || !asked || to === asked) return null;
    return { asked, to, why: dg.reason || 'power' };
}

/// What the player plays when it is fewer taps than asked (player.js, from
/// each status: the audible chain's downgrade), or null: the Resolution
/// cell says it with the rack's size, so the cell and the taps chip agree.
export function setStripLowered(next) {
    const key = v => (v ? `${v.asked}|${v.to}|${v.why}` : '');
    if (key(next) === key(lowered)) return;
    lowered = next ? { asked: next.asked, to: next.to, why: next.why } : null;
    renderStrip();
}

/// What the player plays in listening mode (player.js, from each status:
/// `stripPlaying`): the FS and Output cells and the heading say it. The
/// heading (ui.js) hears of a change through `onStripPlaying`.
export function setStripPlaying(next) {
    const key = v => JSON.stringify(v);
    if (key(next) === key(playing)) return;
    playing = next ? { ...next, dsd: next.dsd ? { ...next.dsd } : null } : null;
    if (playingHook) playingHook(); else renderStrip();
}

/// What the strip says plays (null: the rack as a conversion writes it).
export const stripPlayingNow = () => playing;

/// The heading's redraw on a change of what plays (it redraws the strip too).
export function onStripPlaying(fn) { playingHook = fn; }

const setText = (el, t) => { if (el && el.textContent !== t) el.textContent = t; };

/// The strip after any change of the rack: the values, the cell that is
/// open, the choices ticked.
let gpuAsk = null;

/// Ask the engine which card the GPU's Auto would use (`gpu_info`), once,
/// or again when `again`; the strip's tip shows it. Standing up the device
/// takes a moment, so it is asked on the first look at the cell, not at
/// launch.
export function askGpuInfo(again = false) {
    if (gpuAsk && !again) return gpuAsk;
    const invoke = globalThis.window?.__TAURI__?.core?.invoke;
    if (!invoke) return Promise.resolve(null);
    gpuAsk = invoke('gpu_info').then(j => {
        const info = JSON.parse(j);
        state.gpuInfo = info;
        state.gpuNone = info.found && info.supported ? null : (info.reason || 'unknown');
        renderStrip();
        return info;
    }).catch(() => null);
    return gpuAsk;
}

export function renderStrip() {
    if (!strip) return;
    strip.classList.toggle('rk-version', !!versionViewNow());
    for (const c of stripCells(readRack())) {
        const b = strip.querySelector(`.rk-cell[data-cell="${c.id}"]`);
        if (!b) continue;
        setText(b.querySelector('.rk-lbl'), c.label);
        setMarkedText(b.querySelector('.rk-v'), c.value);
        setText(b.querySelector('.rk-u'), c.unit);
        setText(b.querySelector('.rk-sub'), c.sub || '');
        const cls = 'rk-cell' + (c.cls ? ' ' + c.cls : '') + (c.sub ? ' has-sub' : '') + (open === c.id ? ' open' : '');
        if (b.className !== cls) b.className = cls;
        if (b.dataset.tip !== c.tip) b.dataset.tip = c.tip;
        b.setAttribute('aria-label', `${c.label}: ${c.value}${c.unit ? ' ' + c.unit : ''}${c.sub ? ', ' + c.sub : ''}`);
        if (c.id !== 'gpu') b.setAttribute('aria-expanded', open === c.id ? 'true' : 'false');
    }
    for (const list of pop.querySelectorAll('.rk-opts')) {
        const sel = $(list.dataset.for);
        if (!sel) continue;
        list.classList.toggle('off', sel.disabled);
        for (const o of list.querySelectorAll('.rk-opt')) o.classList.toggle('on', o.dataset.value === sel.value);
    }
    // A pane's own ticks (`.rk-flag`): a checkbox shown as the lists' rows.
    for (const f of pop.querySelectorAll('.rk-flag')) {
        const on = !!$(f.dataset.for)?.checked;
        f.classList.toggle('on', on);
        f.setAttribute('aria-checked', on ? 'true' : 'false');
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
    list.replaceChildren();
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
    // A version on the air shows its own values: nothing to change there.
    if (versionViewNow()) return;
    if (id === 'gpu') {
        // Auto: nothing to switch. Ask for the card again (a driver may
        // have come back), the tip says what was found.
        close();
        askGpuInfo(true);
        return;
    }
    // Listening, the Output cell says the player's output: its list opens.
    if (id === 'out' && playing) {
        close();
        $('plOutFmt')?.click();
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

/// A select whose options changed (the modulators of a new DSD rate): its
/// pane's list again, with `names` for its choices.
export function relistChoices(selId, names) {
    const list = pop?.querySelector(`.rk-opts[data-for="${selId}"]`);
    if (list) listChoices(list, names);
    renderStrip();
}

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

/// Under the strip and as wide as the strip, its arrow under the cell that
/// opened it. Measured on the screen, not by offsets: in the Library window
/// the panel is only a frame for its parts (landscape.css `display:
/// contents`), the pane's box is the whole window, and a pane set by its box
/// ran across all of it (Anton 23.11) — it is the strip's width there too,
/// as in the compact window, over the Sound column however far it is scrolled.
function place() {
    if (!open) return;
    const box = pop.offsetParent;
    const s = strip.getBoundingClientRect();
    const b = box ? box.getBoundingClientRect() : { left: 0, top: 0 };
    const x0 = b.left + (box ? box.clientLeft - box.scrollLeft : 0);
    const y0 = b.top + (box ? box.clientTop - box.scrollTop : 0);
    pop.style.top = Math.round(s.bottom - y0 + 7) + 'px';
    pop.style.left = Math.round(s.left - x0) + 'px';
    pop.style.width = Math.round(s.width) + 'px';
    pop.style.right = 'auto';
    const cell = strip.querySelector(`.rk-cell[data-cell="${open}"]`);
    if (cell) {
        const c = cell.getBoundingClientRect();
        pop.style.setProperty('--rk-caret', Math.round(c.left - s.left + c.width / 2) + 'px');
    }
}

/// Build the cells and wire them. Does nothing where the page has no strip.
export function initRackStrip() {
    strip = $('rkStrip');
    onVersionView(() => { if (open) close(); renderStrip(); });
    pop = $('rkPop');
    if (!strip || !pop) return;
    for (const id of CELLS) {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'rk-cell';
        b.dataset.cell = id;
        b.innerHTML = '<span class="rk-lbl"></span><span class="rk-val"><span class="rk-v"></span><small class="rk-u"></small></span><span class="rk-sub"></span>';
        b.addEventListener('click', e => {
            if (e.detail > 0) b.blur();   // Space stays the player's
            press(id, e.detail === 0);
        });
        if (id === 'gpu') {
            b.addEventListener('pointerenter', () => askGpuInfo());
            b.addEventListener('focus', () => askGpuInfo());
        }
        strip.appendChild(b);
    }
    for (const list of pop.querySelectorAll('.rk-opts')) {
        const names = { convApodizing: APOD_CHOICES, convOutFormat: OUT_CHOICES, convDsdRate: DSD_RATE_CHOICES }[list.dataset.for];
        listChoices(list, names || HR_CHOICES);
    }
    for (const f of pop.querySelectorAll('.rk-flag')) {
        f.querySelector('.rk-nm').textContent = STRIP_TEXTS[f.dataset.text] || '';
        f.dataset.tip = STRIP_TEXTS[f.dataset.text + 'Tip'] || '';
        // The tick flips its checkbox, with its change event; the pane stays.
        f.addEventListener('click', () => {
            const cb = $(f.dataset.for);
            if (!cb) return;
            cb.checked = !cb.checked;
            cb.dispatchEvent(new Event('change', { bubbles: true }));
        });
    }
    // Away on a press anywhere else and on Esc — that press goes nowhere
    // else (popups.js) — and when the rack folds away.
    addPopup({ isOpen: () => !!open, inside: t => stripOwns(strip, pop, t), close });
    window.addEventListener('resize', place);
    // The Sound column scrolls under an open pane: the pane goes with the strip.
    document.addEventListener('scroll', place, true);
    // Only on the way in: a window fit takes the class off and puts it back
    // (main.js), and any other class on the body changes it too.
    let listening = document.body.classList.contains('mode-listening');
    new MutationObserver(() => {
        const now = document.body.classList.contains('mode-listening');
        if (open && now && !listening) close();
        listening = now;
    }).observe(document.body, { attributes: true, attributeFilter: ['class'] });
    // A control moved in its pane: the cell says so at once.
    for (const t of ['input', 'change']) {
        pop.addEventListener(t, renderStrip);
    }
    renderStrip();
}
