import { state } from './state.js';
import { formatTaps } from './helpers.js';
import { saveSettings } from './settings.js';

// FS presets matching converter.js (2, 4, 8, 16)
const FS_PRESETS = [2, 4, 8, 16];
// Reference base rates for display
const BASE_441 = 44100;
const BASE_480 = 48000;

/** Format Hz for display: 352800 → "352.8k", 768000 → "768k" */
function fmtKhz(hz) {
    const k = hz / 1000;
    return (k === Math.floor(k) ? k.toFixed(0) : k.toFixed(1)) + 'k';
}

/** Update the FS slider display label (FSN) and the reference Hz row */
export function updateFsDisplay() {
    const slider = document.getElementById('convFsSlider');
    if (!slider) return;
    const idx = parseInt(slider.value) || 0;
    const fs = FS_PRESETS[Math.min(idx, FS_PRESETS.length - 1)];

    const displayEl = document.getElementById('convFsDisplay');
    if (displayEl) displayEl.textContent = 'FS' + fs;

    const hzEl = document.getElementById('convFsHz');
    if (hzEl) {
        const hz441 = BASE_441 * fs;
        const hz480 = BASE_480 * fs;
        hzEl.textContent = fmtKhz(hz441) + ' / ' + fmtKhz(hz480) + ' kHz';
    }
}

export function updateConvSpecsLine() {
    const tapSlider = document.getElementById('convTapSlider');
    if (!tapSlider) return;
    const presets = [1_000_000, 5_000_000, 10_000_000, 30_000_000];
    let index = parseInt(tapSlider.value) || 0;
    if (index >= presets.length) index = presets.length - 1;
    let actualTaps = state.convCustomFilterTaps || presets[index];
    const taps = formatTaps(actualTaps);

    // FS display for specs line
    const fsSlider = document.getElementById('convFsSlider');
    let fsText = 'FS8';
    if (fsSlider) {
        const idx = parseInt(fsSlider.value) || 0;
        const fs = FS_PRESETS[Math.min(idx, FS_PRESETS.length - 1)];
        const hz441 = BASE_441 * fs;
        const hz480 = BASE_480 * fs;
        fsText = `FS${fs} (${fmtKhz(hz441)}/${fmtKhz(hz480)})`;
    }

    const win = document.getElementById('convWindow');
    const winName = win ? win.options[win.selectedIndex].text : '';
    
    const el = document.getElementById('convSpecsLine');
    if (el) el.innerHTML = (() => {
        const apodSel = document.getElementById('convApodizing');
        const headroomSel = document.getElementById('convHeadroom');
        let extras = '';
        if (apodSel && apodSel.value !== '0') extras += ` &bull; Apod:${apodSel.options[apodSel.selectedIndex].text}`;
        if (headroomSel && headroomSel.value !== '0') extras += ` &bull; HR:${headroomSel.options[headroomSel.selectedIndex].text}`;
        if (document.getElementById('convAdaptiveApodizer')?.checked) extras += ' &bull; <span style="color:#34d399">AA</span>';
        if (document.getElementById('convHybridPhase')?.checked) extras += ' &bull; <span style="color:#a78bfa">HP</span>';
        return `FIR [${winName}] 64-bit &bull; ${taps} Taps &bull; ${fsText} &bull; FLAC${extras}`;
    })();
}

export function setConverterControlsEnabled(enabled) {
    const ids = [
        'convFsSlider', 'convTapSlider', 'convWindow', 'convApodizing',
        'convHeadroom', 'convAdaptiveApodizer', 'convHybridPhase',
        'convFirResampling', 'convGpuCheck', 'convLoadFilterBtn', 'convClearFilterBtn'
    ];
    for (const id of ids) {
        const el = document.getElementById(id);
        if (el) {
            el.disabled = !enabled;
            el.style.opacity = enabled ? '1' : '0.35';
        }
    }
    if (enabled) {
        import('./settings.js').then(m => m.applySettingsDependencies());
    }
}
