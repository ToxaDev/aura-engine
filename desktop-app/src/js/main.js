
import { state } from './state.js';
import { updateConvSpecsLine, updateFsDisplay } from './ui.js';
import { loadSettings, applySettingsDependencies, saveSettings } from './settings.js';
import { bindConverterControls, convApplyFilterUI, tapPresets } from './converter.js';
import { initDropZone, renderFileQueue } from './dropzone.js';

const { invoke } = window.__TAURI__.tauri;
const { appWindow, LogicalSize } = window.__TAURI__.window;

async function loadComponent(id, url) {
    const res = await fetch(url);
    const html = await res.text();
    document.getElementById(id).innerHTML = html;
}

async function init() {
    // 1. Fetch components
    await loadComponent('converterPanel', 'components/converter.html');

    // 2. Bind UI Modules
    bindConverterControls();
    initDropZone();





    // 6. Restore Settings
    loadSettings();
    applySettingsDependencies();
    if (state.convCustomFilterPath && state.convCustomFilterTaps > 0) {
        convApplyFilterUI(state.convCustomFilterName, state.convCustomFilterTaps);
    }
    
    // 7. Initial Displays
    updateFsDisplay();
    updateConvSpecsLine();
    let bootIndex = parseInt(document.getElementById('convTapSlider').value);
    if (bootIndex >= tapPresets.length) bootIndex = tapPresets.length - 1;
    let bootTaps = tapPresets[bootIndex];
    document.getElementById('convTapDisplay').textContent = (bootTaps >= 1000000 ? (bootTaps / 1000000).toFixed(1) + 'M' : (bootTaps / 1000) + 'K') + ' Taps';

    // 8. Auto size
    setTimeout(async () => { try { await appWindow.setSize(new LogicalSize(440, 960)); } catch(e){} }, 100);
}

document.addEventListener('DOMContentLoaded', init);
