
import { state } from './state.js';
import { updateConvSpecsLine, updateFsDisplay, refreshFilterAvailability } from './ui.js';
import { loadSettings, applySettingsDependencies, saveSettings } from './settings.js';
import { bindConverterControls, convApplyFilterUI, tapPresets, setStatus } from './converter.js';
import { initDropZone, renderFileQueue } from './dropzone.js';
import { initDspRack, syncDspRack } from './dsprack.js';
import {
    loadFilterInventory, inventory, bestCombo, nearestAvailable,
    TAP_PRESETS, FS_PRESETS
} from './inventory.js';
import { formatTaps } from './helpers.js';

const { invoke } = window.__TAURI__.tauri;
const { appWindow, LogicalSize } = window.__TAURI__.window;

async function loadComponent(id, url) {
    const res = await fetch(url);
    const html = await res.text();
    document.getElementById(id).innerHTML = html;
}

/// Size the window to exactly fit the panel, so nothing is clipped and no
/// scrollbar ever appears.
///
/// The height is measured, not hardcoded. A fixed number is a guess about the
/// display's text scaling, and the wrong guess is what used to push the status
/// line off the bottom edge where it looked like it had disappeared.
async function fitWindowToContent() {
    const panel = document.getElementById('converterPanel');
    if (!panel) return;
    const progress = document.getElementById('convProgressWrap');

    // Measure the tallest state the panel ever reaches: mid-conversion, with
    // the progress bar and the status line both present. Sizing for that once
    // means the window never has to resize while a batch is running.
    const prevDisplay  = progress ? progress.style.display : null;
    if (progress) progress.style.display = 'block';
    const prevHeight   = panel.style.height;
    const prevOverflow = panel.style.overflowY;
    panel.style.height = 'auto';
    panel.style.overflowY = 'visible';

    // `.slide-panel` is laid out as calc(100vh - 20px); give those 20 back.
    const needed = Math.ceil(panel.getBoundingClientRect().height) + 20;

    panel.style.height = prevHeight;
    panel.style.overflowY = prevOverflow;
    if (progress) progress.style.display = prevDisplay;

    // Never ask for a window taller than the screen can show.
    const avail = (window.screen && window.screen.availHeight) || needed;
    const target = Math.min(needed, Math.max(560, avail - 60));
    // Only in that clamped case is a scrollbar the lesser evil.
    panel.style.overflowY = target < needed ? 'auto' : '';

    try {
        await appWindow.setSize(new LogicalSize(440, target));
        // setSize addresses the outer box on some platforms and the inner box
        // on others, so correct by whatever the viewport actually became
        // instead of assuming a title-bar height.
        await new Promise(r => setTimeout(r, 60));
        const delta = target - window.innerHeight;
        if (Math.abs(delta) > 2) {
            await appWindow.setSize(new LogicalSize(440, Math.min(target + delta, avail - 40)));
        }
    } catch (e) {}
}

/// Put the sliders on a combination this installation actually has filters for.
///
/// The filter blobs are a separate multi-gigabyte download, so the app cannot
/// assume the full matrix is present. A first run opens on the largest filter
/// on disk at FS8 — which is what the ready-made bundles are built around, so
/// the copy a newcomer downloads opens already set to the filter that came
/// with it, and the first thing they do is drop a file rather than read an
/// error.
///
/// A returning user keeps their own settings. They are only moved when the
/// filters behind them are not there any more, and then they are told.
function applyInstalledFilterDefaults(hadSavedSettings) {
    // Nothing known, or nothing installed: leave every control alone. The
    // notice under the sliders covers the empty case.
    if (!inventory.known || inventory.cells.size === 0) return;
    // A custom .npy replaces the built-in matrix, so the tap slider is not
    // ours to move.
    if (state.convCustomFilterPath) return;

    const tapSlider = document.getElementById('convTapSlider');
    const fsSlider = document.getElementById('convFsSlider');
    if (!tapSlider || !fsSlider) return;

    // Both Hybrid-Phase and TFS render against the minimum-phase half of the
    // pair, so either one makes half a pair too little to open on. TFS ships
    // on, so unlike before this genuinely constrains a fresh install.
    const needMin = !!document.getElementById('convHybridPhase')?.checked
                 || !!document.getElementById('convLabTfs')?.checked;

    const taps = TAP_PRESETS[Math.min(parseInt(tapSlider.value) || 0, TAP_PRESETS.length - 1)];
    const fs = FS_PRESETS[Math.min(parseInt(fsSlider.value) || 0, FS_PRESETS.length - 1)];

    const want = hadSavedSettings ? nearestAvailable(taps, fs, needMin) : bestCombo(needMin);
    if (!want) return;
    if (want.taps === taps && want.fs === fs) return;

    tapSlider.value = String(TAP_PRESETS.indexOf(want.taps));
    fsSlider.value = String(FS_PRESETS.indexOf(want.fs));
    // Persist the correction, so this is a one-time move rather than something
    // that happens again at every launch.
    saveSettings();

    if (hadSavedSettings) {
        setStatus(
            `No filter for ${formatTaps(taps)} · FS${fs} — switched to `
            + `${formatTaps(want.taps)} · FS${want.fs}`);
    }
}

/// The version over the heading, read from the app manifest (tauri.conf.json,
/// kept in step with Cargo.toml) rather than written into the page, so a
/// release cannot ship showing the number of the one before it.
async function showAppVersion() {
    const el = document.getElementById('appVersion');
    if (!el) return;
    try {
        const v = await window.__TAURI__?.app?.getVersion?.();
        if (v) el.textContent = v;
    } catch (_) {
        // No manifest to ask (a page opened outside the app): the line stays
        // empty and keeps its height.
    }
}

async function init() {
    // 1. Fetch components
    await loadComponent('converterPanel', 'components/converter.html');
    showAppVersion();

    // 2. Bind UI Modules
    bindConverterControls();
    initDropZone();
    initDspRack();

    // 3. Ask the backend which filter blobs are on disk, before anything reads
    //    a slider: the answer decides what the sliders are allowed to open on.
    await loadFilterInventory();

    // 4. Restore Settings
    const hadSavedSettings = loadSettings();
    applySettingsDependencies();
    syncDspRack();
    if (state.convCustomFilterPath && state.convCustomFilterTaps > 0) {
        convApplyFilterUI(state.convCustomFilterName, state.convCustomFilterTaps);
    }
    applyInstalledFilterDefaults(hadSavedSettings);

    // 5. Initial Displays
    updateFsDisplay();
    updateConvSpecsLine();
    let bootIndex = parseInt(document.getElementById('convTapSlider').value);
    if (bootIndex >= tapPresets.length) bootIndex = tapPresets.length - 1;
    document.getElementById('convTapDisplay').textContent =
        formatTaps(tapPresets[bootIndex]) + ' Taps';
    refreshFilterAvailability();

    // 6. Auto size
    setTimeout(fitWindowToContent, 100);
}

document.addEventListener('DOMContentLoaded', init);
