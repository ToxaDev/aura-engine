// First: a press outside an open popup is stopped before anything else of
// the page hears it (popups.js).
import './popups.js';
import { state } from './state.js';
import { initUpdateChecker } from './update.js';
import { updateConvSpecsLine, updateFsDisplay, refreshFilterAvailability } from './ui.js';
import { loadSettings, applySettingsDependencies, saveSettings } from './settings.js';
import { bindConverterControls, convApplyFilterUI, tapPresets, setStatus } from './converter.js';
import { initRackStrip } from './rackstrip.js';
import { studioRowsFor, rowsForWindow } from './list-geom.js';
import { initDropZone, renderFileQueue } from './dropzone.js';
import { initDspRack, syncDspRack } from './dsprack.js';
import { initPlayer, playerSettingsReady } from './player.js';
import { initListening } from './listening.js';
import {
    loadFilterInventory, inventory, bestCombo, nearestAvailable,
    TAP_PRESETS, FS_PRESETS
} from './inventory.js';
import { formatTaps } from './helpers.js';
import { appVersionLabel } from './ui-pack.js';
import { initPerfBadge } from './perf-badge.js';
import './analytics.js';
import './pack-note.js';

const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { LogicalSize } = window.__TAURI__.dpi;
const appWindow = getCurrentWindow();

// ── Frameless-window drag (item F) ──────────────────────────────────────────
// Mousedown on any non-interactive spot starts a window drag so the user can
// drag the window by its heading, spec line, empty rack area, etc. — the usual
// title-bar role, distributed across the whole surface.
//
// isDragTarget walks up from the clicked element; the first interactive ancestor
// it meets returns false so the click is NOT a drag. Exported onto window for
// the unit tests the spec asks for: the real-window check is integration-only.
function isDragTarget(el) {
    for (let n = el; n && n !== document.documentElement; n = n.parentElement) {
        const tag = (n.tagName || '').toUpperCase();
        // Hard interactive tags
        if (tag === 'A' || tag === 'BUTTON' || tag === 'INPUT' ||
            tag === 'SELECT' || tag === 'TEXTAREA' || tag === 'CANVAS') return false;
        // Scrollable container that actually overflows
        if (tag === 'DIV' || tag === 'UL' || tag === 'OL') {
            const s = window.getComputedStyle(n);
            const oy = s.overflowY;
            if ((oy === 'auto' || oy === 'scroll') && n.scrollHeight > n.clientHeight + 2) return false;
        }
        // Row items and player controls
        if (n.classList.contains('file-item')) return false;
        if (n.classList.contains('lab-badge') || n.classList.contains('cb')) return false;
        if (n.classList.contains('pl-seek') || n.classList.contains('pl-transport')) return false;
        if (n.classList.contains('pl-dev') || n.classList.contains('pl-list-head')) return false;
        if (n.classList.contains('pl-list-body')) return false;
        // The radio in the list's place: its stations are pressed, not dragged.
        if (n.classList.contains('rd-view')) return false;
        // The input in the list's place: its switch's words are pressed too.
        if (n.classList.contains('in-view')) return false;
    }
    return true;
}
// Expose for page-side unit tests (no side effects).
window.__isDragTarget = isDragTarget;

// Windows lets the click through when a drag starts and goes nowhere. GTK and
// macOS hand the press to the window manager, so the click on a row, a tick
// or a tab is lost: there the drag waits until the mouse moves a few pixels.
const DRAG_ON_PRESS = /Windows/.test(navigator.userAgent);
const DRAG_SLOP_PX = 4;

function initDragAndWindowButtons() {
    // Drag: capture-phase mousedown on the left button anywhere non-interactive.
    let pressAt = null;
    document.addEventListener('mousedown', function (e) {
        pressAt = null;
        if (e.button !== 0) return;
        if (e.target.closest('#upModal, #fdModal')) return;
        if (e.target.closest('#fvView')) return;        // full screen: nothing to drag
        if (isDragTarget(e.target)) {
            if (!DRAG_ON_PRESS) { pressAt = { x: e.screenX, y: e.screenY }; return; }
            try { appWindow.startDragging(); } catch (_) {}
        }
    }, true);
    document.addEventListener('mousemove', function (e) {
        if (!pressAt) return;
        if (!(e.buttons & 1)) { pressAt = null; return; }
        if (Math.hypot(e.screenX - pressAt.x, e.screenY - pressAt.y) < DRAG_SLOP_PX) return;
        pressAt = null;
        try { appWindow.startDragging(); } catch (_) {}
    }, true);
    document.addEventListener('mouseup', () => { pressAt = null; }, true);

    // Window control buttons wired up after the converter component is loaded.
    const btnMin   = document.getElementById('winBtnMin');
    const btnClose = document.getElementById('winBtnClose');
    if (btnMin)   btnMin.addEventListener('click',   () => { try { appWindow.minimize(); } catch (_) {} });
    // Quit, not just close: the other windows (the studio, the analyzer) and
    // the music go with it (main.rs app_quit).
    if (btnClose) btnClose.addEventListener('click', () => {
        window.__TAURI__.core.invoke('app_quit').catch(() => { try { appWindow.close(); } catch (_) {} });
    });
}

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
let fitWanted = false;
/// Back from full screen: the fit that was asked for meanwhile, if any
/// (otherwise the window keeps exactly the size it came back with).
window.fitWindowAfterFull = () => {
    if (!fitWanted) return;
    fitWanted = false;
    fitWindowToContent();
};

/// One fit at a time: a fit asked for while one runs runs once after it.
let fitRunning = null, fitQueued = false;
async function fitWindowToContent() {
    if (fitRunning) { fitQueued = true; return fitRunning; }
    fitRunning = (async () => {
        do { fitQueued = false; await fitOnce(); } while (fitQueued);
    })();
    try { await fitRunning; } finally { fitRunning = null; }
}

/// Content that changes height after a fit (a bar, a notice, a line that
/// grows) fits the window again, so the list's border never runs past the
/// bottom edge between fits. Children added later are watched too.
function watchPanelHeight(panel) {
    if (typeof ResizeObserver !== 'function') return;
    let timer = null;
    const soon = () => {
        // The listening switch animates against its own numbers; its end fits.
        if (document.body.classList.contains('lm-anim')) return;
        clearTimeout(timer);
        timer = setTimeout(fitWindowToContent, 50);
    };
    const ro = new ResizeObserver(soon);
    // The window changed height by itself (a move to a display with another
    // scaling): the fit checks it against the content; its own resizes find
    // the height already right and ask for nothing.
    window.addEventListener('resize', soon);
    for (const el of panel.children) ro.observe(el);
    new MutationObserver(recs => recs.forEach(r => r.addedNodes.forEach(n => {
        if (n.nodeType === 1) ro.observe(n);
    }))).observe(panel, { childList: true });
}

/// Resolves on the next resize of the viewport, or after `ms` without one.
function nextResize(ms) {
    return new Promise(done => {
        const end = () => { clearTimeout(t); window.removeEventListener('resize', end); done(); };
        const t = setTimeout(end, ms);
        window.addEventListener('resize', end);
    });
}

async function fitOnce() {
    const panel = document.getElementById('converterPanel');
    if (!panel) return;
    // Full screen (fullview.js): the window is the screen until it comes
    // back to its own place and size; a fit asked for meanwhile runs then.
    if (document.body.classList.contains('fullview')) { fitWanted = true; return; }

    // Measure the natural content height without forcing the progress bar
    // visible: the window fits exactly the content as it is right now.
    // Callers that show or hide the progress bar (converter.js) call
    // window.fitWindowToContent() afterwards so the window follows.
    const prevHeight   = panel.style.height;
    const prevOverflow = panel.style.overflowY;
    // [listening mode] The window keeps the studio's height in both modes, so
    // the studio is what gets measured: listening.js's classes come off for
    // the measurement and go back before anything is painted.
    const lm = ['mode-listening', 'lm-keeprows'].filter(c => document.body.classList.contains(c));
    lm.forEach(c => document.body.classList.remove(c));
    // The big player writes its two-line technical line into the player's
    // sub line; the studio shows one line there.
    const sub = lm.length ? document.getElementById('plSub') : null;
    const prevSub = sub ? sub.style.maxHeight : '';
    if (sub) sub.style.maxHeight = '1lh';
    panel.style.height = 'auto';
    panel.style.overflowY = 'visible';

    // .slide-panel fills 100vh exactly (it is the visual window frame).
    // The natural content height IS the window height we need.
    // [studio rows] Measured with the stylesheet's rows (ten, list.css); on a
    // screen too low for that window, as many fewer as it takes (not fewer
    // than six), so the window needs no scrollbar.
    const root = document.documentElement;
    root.style.removeProperty('--pl-visible-rows');
    let needed = Math.ceil(panel.getBoundingClientRect().height);
    const css = getComputedStyle(root);
    const rowsCss = parseInt(css.getPropertyValue('--pl-visible-rows')) || 10;
    const step = (parseFloat(css.getPropertyValue('--pl-row-h')) || 43) + (parseFloat(css.getPropertyValue('--pl-row-gap')) || 4);
    // Never ask for a window taller than the screen can show.
    const avail = (window.screen && window.screen.availHeight) || needed;
    const room = Math.max(560, avail - 60);
    const rows = studioRowsFor(needed, room, rowsCss, step);
    if (rows < rowsCss) {
        root.style.setProperty('--pl-visible-rows', String(rows));
        needed = Math.ceil(panel.getBoundingClientRect().height);
    }

    panel.style.height = prevHeight;
    panel.style.overflowY = prevOverflow;
    if (sub) sub.style.maxHeight = prevSub;
    lm.forEach(c => document.body.classList.add(c));

    const target = Math.min(needed, room);
    // Only in that clamped case is a scrollbar the lesser evil.
    panel.style.overflowY = target < needed ? 'auto' : '';

    try {
        // Already that height (a watched change that left the height as it
        // was, the listening switch): nothing to ask the system for.
        if (Math.abs(target - window.innerHeight) > 1) {
            let resized = nextResize(500);
            await appWindow.setSize(new LogicalSize(440, target));
            await resized;
            // setSize addresses the outer box on some platforms and the inner
            // box on others, so correct by whatever the viewport actually
            // became instead of assuming a title-bar height.
            const delta = target - window.innerHeight;
            if (Math.abs(delta) > 2) {
                resized = nextResize(500);
                await appWindow.setSize(new LogicalSize(440, Math.min(target + delta, avail - 40)));
                await resized;
            }
        }
    } catch (e) {}

    // The window may still be lower than the content (the screen's edge, a
    // size the system did not give): fewer rows, so the list's whole dashed
    // border stays inside the window; a scrollbar only past that.
    const got = window.innerHeight;
    if (got > 0 && got < target - 2) {
        const r = rowsForWindow(needed, got, rows, step);
        if (r.rows < rows) root.style.setProperty('--pl-visible-rows', String(r.rows));
        if (r.scroll) panel.style.overflowY = 'auto';
    }
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
/// release cannot ship showing the number of the one before it. With an
/// interface update served it reads "1.6.2 · UI 1".
async function showAppVersion() {
    const el = document.getElementById('appVersion');
    if (!el) return;
    try {
        const v = await appVersionLabel();
        // Into the line's own text: the load badge (and an update badge)
        // sit beside it.
        if (v) {
            const t = el.firstChild && el.firstChild.nodeType === 3
                ? el.firstChild
                : el.insertBefore(document.createTextNode(''), el.firstChild);
            t.data = v;
        }
    } catch (_) {
        // No manifest to ask (a page opened outside the app): the line stays
        // empty and keeps its height.
    }
}

async function init() {
    // 1. Fetch components
    await loadComponent('converterPanel', 'components/converter.html');
    showAppVersion();
    initPerfBadge();

    // 2. Bind UI Modules
    bindConverterControls();
    // The rack's settings folded into one strip under the heading.
    initRackStrip();
    initDropZone();
    initDspRack();
    // The transport strip over the list; built before the window is sized.
    initPlayer();
    // [listening mode] Right after the strip exists and before the next
    // await: a remembered listening mode is on before the first paint.
    initListening();
    // analytics.js needs #plBar which initPlayer() just created.
    document.dispatchEvent(new Event('aura:player-ready'));
    // Drag zones and frameless window buttons (item F). Called after the
    // component is in the DOM so the button ids are present.
    initDragAndWindowButtons();

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

    // J5: the rack is now fully restored (settings + filter defaults applied).
    // Notify library.js so it re-evaluates the rack signature and updates the
    // "converted / needs conversion" state of every restored row immediately,
    // before the user sees the list. library.js exports rackRestored(); guard
    // with ?. because it is authored by a parallel stream and may not exist
    // yet if the streams land in a different order.
    try {
        const lib = await import('./library.js');
        lib.rackRestored?.();
    } catch (_) {}

    // The player gets the rack as restored, before anything is played.
    playerSettingsReady();

    // 5. Initial Displays
    updateFsDisplay();
    updateConvSpecsLine();
    let bootIndex = parseInt(document.getElementById('convTapSlider').value);
    if (bootIndex >= tapPresets.length) bootIndex = tapPresets.length - 1;
    document.getElementById('convTapDisplay').textContent =
        formatTaps(tapPresets[bootIndex]) + ' Taps';
    refreshFilterAvailability();

    // 6. Auto size — expose globally so dsprack.js and converter.js can
    //    call it after DOM changes that affect the panel's natural height.
    window.fitWindowToContent = fitWindowToContent;
    setTimeout(fitWindowToContent, 100);
    watchPanelHeight(document.getElementById('converterPanel'));
    window.__auraBooted = true;

    // 7. Check for an update ~2 s after boot (once per session, non-blocking).
    const updateChecked = new Promise(done => setTimeout(() => {
        initUpdateChecker().catch(() => {}).finally(done);
    }, 2000));

    // The ? under the version, and the tour's offer to anyone it has not
    // been offered to — after the update check, so the two never stand over
    // each other (tour.js). Loaded on its own: a tour that fails to load or
    // to start costs the tour, not the window.
    import('./tour.js')
        .then(m => m.initTour({ after: updateChecked }))
        .catch(e => window.__auraReport?.('Tour: ' + (e?.message || e)));

    // 8. The developer's blind test, only when the environment asks for it.
    window.__TAURI__.core.invoke('blind_test_trials')
        .then(n => n && import('./blindtest.js').then(m => m.initBlindTest(n)))
        .catch(e => window.__auraReport?.('Blind test: ' + (e?.message || e)));
}

document.addEventListener('DOMContentLoaded', () => {
    init().catch(e => window.__auraReport?.('Start-up failed: ' + (e?.message || e)));
});
