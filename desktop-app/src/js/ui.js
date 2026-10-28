import { state } from './state.js';
import { renderFilterBar, setFiltersChangedHook } from './filters-dl.js';
import { phasesFor, needsMinimum } from './filters-dl-core.js';
import { saveSettings } from './settings.js';
import { setDspRackEnabled } from './dsprack.js';
import { renderStrip } from './rackstrip.js';
import { getDsdOutput, effectiveFs, specsOutput, selectedModulatorLabel, flacFsRefusal } from './dsdexport.js';
import {
    TAP_PRESETS, FS_PRESETS, inventory,
    comboAvailable, tapsAvailable, packsFor
} from './inventory.js';

// Reference base rates for display
const BASE_441 = 44100;
const BASE_480 = 48000;

/** Format Hz for display: 352800 → "352.8k", 768000 → "768k" */
function fmtKhz(hz) {
    const k = hz / 1000;
    return (k === Math.floor(k) ? k.toFixed(0) : k.toFixed(1)) + 'k';
}

/** The tap count the slider currently points at (ignoring a custom filter). */
function selectedTaps() {
    const el = document.getElementById('convTapSlider');
    let i = parseInt(el ? el.value : 0) || 0;
    if (i >= TAP_PRESETS.length) i = TAP_PRESETS.length - 1;
    return TAP_PRESETS[i];
}

/** The FS multiplier the slider currently points at. */
function selectedFs() {
    const el = document.getElementById('convFsSlider');
    let i = parseInt(el ? el.value : 0) || 0;
    if (i >= FS_PRESETS.length) i = FS_PRESETS.length - 1;
    return FS_PRESETS[i];
}

/** Update the FS slider display label (FSN) and the reference Hz row */
export function updateFsDisplay() {
    const slider = document.getElementById('convFsSlider');
    if (!slider) return;
    const fs = selectedFs();

    const displayEl = document.getElementById('convFsDisplay');
    if (displayEl) displayEl.textContent = 'FS' + fs;

    const hzEl = document.getElementById('convFsHz');
    if (hzEl) {
        const hz441 = BASE_441 * fs;
        const hz480 = BASE_480 * fs;
        hzEl.textContent = fmtKhz(hz441) + ' / ' + fmtKhz(hz480) + ' kHz';
    }
}

/** Tap count the way the output filename writes it: 30M, 12.5M, 5k, 1G. */
function compactTaps(value) {
    const v = parseInt(value) || 0;
    if (v >= 1e9) {
        const g = v / 1e9;
        return (Number.isInteger(g) ? g.toFixed(0) : g.toFixed(1)) + 'G';
    }
    if (v >= 1_000_000) {
        const m = v / 1_000_000;
        return (Number.isInteger(m) ? m.toFixed(0) : m.toFixed(1)) + 'M';
    }
    if (v >= 1000) return Math.floor(v / 1000) + 'k';
    return String(v);
}

/// The two lines under the heading, always the same two:
///
///   FIR [KAISER] 64-BIT • 30M TAPS
///   FS8 (352.8K/384K) • FLAC
///
/// The filter on top, the output underneath; only the values change. It used
/// to be one run of text with every switched-on option appended (Apod, HR,
/// SUB, AA, HP), which wrapped wherever it happened to and pushed onto a third
/// line — the panel jumped each time a stage was toggled. The options are lit
/// on their own rows now, and the break is fixed rather than left to the width.
export function updateConvSpecsLine() {
    const tapSlider = document.getElementById('convTapSlider');
    if (!tapSlider) return;
    const taps = compactTaps(state.convCustomFilterTaps || selectedTaps());

    // DSF: the FS the chain runs at for that DSD rate.
    const dsd = getDsdOutput();
    const fs = dsd ? effectiveFs(selectedFs(), dsd.multiple) : selectedFs();
    const fsText = `FS${fs} (${fmtKhz(BASE_441 * fs)}/${fmtKhz(BASE_480 * fs)})`;

    // FLAC at ×32: the slider keeps it for the player, the converter says why it will not write it.
    const hint = document.getElementById('convFsFlacHint');
    if (hint) {
        const why = flacFsRefusal(selectedFs(), dsd);
        hint.textContent = why || '';
        hint.style.display = why ? 'block' : 'none';
    }

    // Every built-in filter is Kaiser; a loaded .npy is whatever it is.
    const filter = state.convCustomFilterPath ? 'Custom' : 'Kaiser';

    const el = document.getElementById('convSpecsLine');
    if (!el) return;
    el.innerHTML =
        `<span class="specs-line">FIR [${filter}] 64-bit &bull; ${taps} Taps</span>`
        + `<span class="specs-line">${fsText} &bull; ${specsOutput(dsd, selectedModulatorLabel())}</span>`;
    renderStrip();
}

/// Show which slider positions this installation actually has filters for, and
/// name the download for the one that is selected when it is missing.
///
/// The marks are advisory, not a lock: the slider still reaches an unavailable
/// position on purpose, because that is how the user learns which pack they
/// want. Nothing here decides whether a conversion may run — the pre-flight
/// check in the backend does, from the real source rates.
export function refreshFilterAvailability() {
    setFiltersChangedHook(refreshFilterAvailability);
    state.filterMissing = markAvailability();
    // The strip's FS and length cells turn amber with it (rackstrip.js).
    renderStrip();
}

/// The marks and the bar above the rack; whether the filter chosen is missing.
function markAvailability() {
    const box = document.getElementById('convFilterAvail');
    const tapMarks = document.querySelectorAll('#convTapMarks span[data-taps]');
    const fsMarks = document.querySelectorAll('#convFsMarks span[data-fs]');

    // Without an answer from the backend we know nothing, and a custom filter
    // replaces the built-in matrix outright (Hybrid-Phase is force-cleared
    // alongside it), so in both cases there is nothing to mark.
    if (!inventory.known || state.convCustomFilterPath) {
        tapMarks.forEach(el => el.classList.remove('mark-off'));
        fsMarks.forEach(el => el.classList.remove('mark-off'));
        if (box) renderFilterBar(box, null);
        return false;
    }

    const needMin = needsMinimum(
        document.getElementById('convHybridPhase')?.checked,
        document.getElementById('convLabTfs')?.checked);
    const taps = selectedTaps();
    const fs = selectedFs();

    tapMarks.forEach(el => el.classList.toggle(
        'mark-off', !tapsAvailable(parseInt(el.dataset.taps, 10), needMin)));
    fsMarks.forEach(el => el.classList.toggle(
        'mark-off', !comboAvailable(taps, parseInt(el.dataset.fs, 10), needMin)));

    const missing = !comboAvailable(taps, fs, needMin);
    if (!box) return missing;
    // The minimum-phase file missing on its own: the setting works, Hybrid-Phase
    // is what it cannot do, and only that file is fetched.
    const onlyMinMissing = missing && needMin && comboAvailable(taps, fs, false);
    const phases = phasesFor(needMin, onlyMinMissing);
    renderFilterBar(box, {
        missing,
        empty: inventory.cells.size === 0,
        taps,
        tapsLabel: compactTaps(taps),
        fs,
        phases,
        packs: missing
            ? packsFor(taps, fs, { linear: phases.includes('linear'), minimum: phases.includes('minimum') })
            : [],
    });
    return missing;
}

export function setConverterControlsEnabled(enabled) {
    const ids = [
        'convFsSlider', 'convTapSlider', 'convApodizing',
        'convHeadroom', 'convAdaptiveApodizer', 'convHybridPhase',
        'convFirResampling', 'convSubsonic', 'convSubsonicHz',
        'convGpuCheck', 'convLoadFilterBtn', 'convClearFilterBtn',
        'convLabDeclip', 'convLabIsp', 'convLabHeadroom',
        'convLabAlpha', 'convLabTfs', 'convLabXtc'
    ];
    for (const id of ids) {
        const el = document.getElementById(id);
        if (el) {
            el.disabled = !enabled;
            el.style.opacity = enabled ? '1' : '0.35';
        }
    }
    setDspRackEnabled(enabled);
    if (enabled) {
        import('./settings.js').then(m => m.applySettingsDependencies());
    }
}
