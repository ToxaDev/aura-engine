
import { state } from './state.js';
import { formatTaps, truncateMiddle } from './helpers.js';
import {
    setConverterControlsEnabled, updateConvSpecsLine, updateFsDisplay,
    refreshFilterAvailability
} from './ui.js';
import { TAP_PRESETS, FS_PRESETS } from './inventory.js';
import { renderFileQueue } from './dropzone.js';
import { saveSettings, applySettingsDependencies } from './settings.js';

const { invoke } = window.__TAURI__.tauri;

// FS Multiplier presets: index → FS value. Defined in inventory.js, which is
// also what decides whether a given position has a filter behind it; re-exported
// here so callers keep addressing the converter for converter settings.
export const fsPresets = FS_PRESETS;

export function getFsMultiplier() {
    const slider = document.getElementById('convFsSlider');
    if (!slider) return 8;
    const idx = parseInt(slider.value) || 0;
    return fsPresets[Math.min(idx, fsPresets.length - 1)];
}

export function convApplyFilterUI(name, taps) {
    const btn = document.getElementById('convLoadFilterBtn');
    btn.classList.add('active');
    btn.textContent = '✓ ' + name;
    document.getElementById('convClearFilterBtn').style.display = 'block';
    const info = document.getElementById('convFilterInfo');
    info.style.display = 'block';
    info.textContent = `Custom filter: ${formatTaps(taps)} taps — ${name}`;
    const tapSlider = document.getElementById('convTapSlider');
    // tapSlider.value is kept as index natively, we don't modify it on custom override 
    tapSlider.disabled = true;
    tapSlider.style.opacity = '0.4';
    document.getElementById('convTapDisplay').textContent = formatTaps(taps) + ' Taps';
    document.getElementById('convWindow').disabled = true;
    document.getElementById('convWindow').style.opacity = '0.4';
    updateConvSpecsLine();
}

/// Tap count in effect right now, honouring a loaded custom filter.
function currentTaps() {
    let idx = parseInt(document.getElementById('convTapSlider').value);
    if (idx >= tapPresets.length) idx = tapPresets.length - 1;
    return state.convCustomFilterPath ? state.convCustomFilterTaps : tapPresets[idx];
}

/// Every status message goes through here and lands in the status line under
/// the progress bar. `kind` is 'idle' | 'busy' | 'ok' | 'error' and colours it.
export function setStatus(text, kind = 'idle') {
    const el = document.getElementById('convStatus');
    if (!el) return;
    el.textContent = truncateMiddle(text);
    el.title = text;                       // full text on hover, untruncated
    el.className = 'conv-status' + (kind === 'idle' ? '' : ' is-' + kind);
}

/// Classify a backend status line so the bar colours itself without every
/// caller having to say what kind of message it is.
function statusKind(text) {
    if (/error|✗|failed/i.test(text)) return 'error';
    if (/all done|done|complete/i.test(text)) return 'ok';
    if (/cancel/i.test(text)) return 'idle';
    return 'busy';
}

/// A missing filter stops the whole batch, so it gets a real OS dialog rather
/// than only a line in a panel the user may not be looking at. The dialog also
/// offers the download directly, since that is the one action that fixes it.
async function showMissingFilterDialog(r) {
    const files = r.missing.map(m => m.file).join('\n   ');
    const plural = r.missing.length === 1 ? 'a filter file' : 'filter files';
    let msg = `The current settings need ${plural} that this build does not have:\n\n   ${files}\n\n`;
    if (r.dest) msg += `Extract into:\n   ${r.dest}\n\n`;
    msg += 'Nothing was converted — the engine never substitutes a different '
         + 'filter and never falls back to a plain resampler.';

    const pack = (r.packs || [])[0];
    try {
        const dlg = window.__TAURI__.dialog;
        if (pack) {
            const yes = await dlg.ask(`${msg}\n\nDownload ${pack.name} now?`, {
                title: 'Aura Engine — missing filter', type: 'error'
            });
            if (yes) window.__TAURI__.shell.open(pack.url);
        } else {
            await dlg.message(msg, { title: 'Aura Engine — missing filter', type: 'error' });
        }
    } catch (e) { /* dialog unavailable — the panel and status bar still carry it */ }
}

/// A source wider than stereo is converted as its front pair and the rest of
/// the channels are dropped. That is a real loss, so it is put to the user
/// before the batch runs rather than left to a line in the log — which is
/// where it used to live, and where nobody read it.
///
/// Answering no converts nothing at all, matching the missing-filter path: a
/// queue is accepted or it is not, and a partly-converted batch is harder to
/// reason about than one that never started.
async function confirmMultichannel(wide) {
    const shown = wide.slice(0, 8)
        .map(m => `   ${m.file} — ${m.channels} channels`).join('\n');
    const more = wide.length > 8 ? `\n   …and ${wide.length - 8} more` : '';
    const lead = wide.length === 1
        ? 'One file in this queue has more than two channels:'
        : `${wide.length} files in this queue have more than two channels:`;

    const msg = `${lead}\n\n${shown}${more}\n\n`
        + 'Only the front left and right pair is converted. The other channels '
        + 'are discarded — centre, surrounds and LFE are dropped, not folded '
        + 'into the stereo pair.\n\n'
        + 'The output will be stereo, and its filename will record the source '
        + 'width.\n\nConvert anyway?';

    try {
        return await window.__TAURI__.dialog.ask(msg, {
            title: 'Aura Engine — more than two channels', type: 'warning'
        });
    } catch (e) {
        // No dialog available: say it in the status bar and do not proceed on
        // the user's behalf.
        setStatus(`${wide.length} file(s) have more than two channels — not converted`, 'error');
        return false;
    }
}

/// Pre-flight before a batch: are all the filters these settings need present,
/// and is anything wider than stereo about to be narrowed? Returns true when
/// the conversion may proceed. A failure of the check itself never blocks —
/// the per-file error path still catches a genuinely missing filter during
/// conversion.
export async function checkFilterAvailability(filePaths) {
    if (!filePaths || filePaths.length === 0) return true;
    try {
        const json = await invoke('check_filters', {
            paths: filePaths,
            fsMultiplier: getFsMultiplier(),
            taps: currentTaps(),
            customFilterPath: state.convCustomFilterPath,
            hybridPhase: document.getElementById('convHybridPhase').checked
        });
        const r = JSON.parse(json);
        if (r.ok) return true;

        // A missing filter is fatal and comes first: there is no point asking
        // about channels for a batch that cannot run at all.
        const n = (r.missing || []).length;
        if (n > 0) {
            setStatus(n === 1
                ? `Missing filter: ${r.missing[0].file}`
                : `Missing ${n} filters for the selected settings`, 'error');
            await showMissingFilterDialog(r);
            return false;
        }

        const wide = r.multichannel || [];
        if (wide.length > 0) {
            setStatus(`${wide.length} file(s) wider than stereo — confirming`, 'busy');
            if (!await confirmMultichannel(wide)) {
                setStatus('Nothing converted', 'idle');
                return false;
            }
        }
        return true;
    } catch (e) {
        return true;
    }
}

/// Asked once per session, and only when the GPU box is ticked: does this
/// machine have the GPU it is promising?
///
/// The engine falls back to the CPU on its own, so nothing here is required
/// for correctness — the output is identical, it is the f64 reference path.
/// What is not identical is the wait. A 30M-tap batch that was going to run
/// on a GPU and is now running on a CPU is a different afternoon, and that is
/// the user's decision to make before it starts rather than a discovery they
/// make at the end.
let gpuFallbackAsked = false;

async function confirmGpuFallback() {
    if (gpuFallbackAsked) return true;
    if (!document.getElementById('convGpuCheck').checked) return true;

    let r;
    try {
        r = JSON.parse(await invoke('gpu_status'));
    } catch (e) {
        return true; // the probe itself failing is not a reason to block a batch
    }
    if (r.available) { gpuFallbackAsked = true; return true; }

    // Mark the control, so the answer stays visible after the dialog is gone.
    const badge = document.getElementById('convGpuBadge');
    const note = document.getElementById('convGpuNote');
    if (badge) badge.textContent = 'CPU FALLBACK';
    if (note) {
        note.textContent = `No usable GPU: ${r.reason}. Conversions run on the CPU.`;
        note.style.display = 'block';
    }

    const msg = 'Hardware GPU acceleration is switched on, but this machine '
        + `has no GPU the engine can use.\n\n${r.reason}\n\n`
        + 'The conversion will run on the CPU instead. The result is identical '
        + '— the CPU path is the f64 reference the GPU path is checked against '
        + '— but it will take considerably longer.\n\nStart anyway?';
    try {
        const yes = await window.__TAURI__.dialog.ask(msg, {
            title: 'Aura Engine — no usable GPU', type: 'warning'
        });
        gpuFallbackAsked = yes;
        return yes;
    } catch (e) {
        // No dialog available. The note above is already on screen and the
        // fallback is safe, so let the batch run rather than stopping it over
        // a message that could not be shown.
        gpuFallbackAsked = true;
        return true;
    }
}

export async function startConversion(filePaths) {
    // Say so before the wait, not after it.
    if (!await checkFilterAvailability(filePaths)) return;
    if (!await confirmGpuFallback()) return;
    setStatus('Preparing…', 'busy');
    state.convIsConverting = true;
    state.convQueueRev = 0; // full queue sync on the first poll of a new batch
    setConverterControlsEnabled(false);
    const isHp = document.getElementById('convHybridPhase').checked;
    const isAa = document.getElementById('convAdaptiveApodizer').checked;
    state.convFileQueue = filePaths.map(f => ({
        path: f,
        name: f.split('\\').pop().split('/').pop(),
        status: 'pending',
        hp: isHp,
        aa: isAa
    }));

    document.getElementById('convProgressWrap').style.display = 'block';
    document.getElementById('dzCancelBtn').style.display = 'flex';
    
    const fillEl = document.getElementById('convProgressFill');
    if (isHp) fillEl.classList.add('conv-hp-glow');
    else fillEl.classList.remove('conv-hp-glow');
    fillEl.style.width = '0%';

    renderFileQueue();

    let tapIndex = parseInt(document.getElementById('convTapSlider').value);
    if (tapIndex >= tapPresets.length) tapIndex = tapPresets.length - 1;
    const taps = state.convCustomFilterPath 
        ? state.convCustomFilterTaps 
        : tapPresets[tapIndex];
    try {
        await invoke('convert_files', {
            paths: filePaths,
            fsMultiplier: getFsMultiplier(),
            taps, precision: 64,
            winType: parseInt(document.getElementById('convWindow').value),
            customFilterPath: state.convCustomFilterPath,
            useGpu: document.getElementById('convGpuCheck').checked,
            useFirResampling: document.getElementById('convFirResampling')?.checked || false,
            apodizing: parseInt(document.getElementById('convApodizing')?.value || '0'),
            headroomDb: parseFloat(document.getElementById('convHeadroom')?.value || '0'),
            adaptiveApodizer: document.getElementById('convAdaptiveApodizer').checked,
            hybridPhase: document.getElementById('convHybridPhase').checked,
            iirDcBlocking: document.getElementById('convIirDc')?.checked || false
        });
        state.convPollTimer = setInterval(pollConversionProgress, 200);
    } catch(e) {
        setStatus('Error: ' + e, 'error');
        resetConverterUI();
    }
}

export async function pollConversionProgress() {
    try {
        const [progress, total, done, statusText, output, snappedRate] = await invoke('get_conversion_progress');
        state.convLastDone = done;
        state.convCurrentFilePct = progress / 10;

        let changedIdxs = null;
        try {
            // Delta poll: send the last merged queue revision; the backend
            // returns only files changed since then plus the active ones.
            // The old full-queue response (~150 KB at 685 files, 5×/sec)
            // went through the Tauri IPC eval bridge and made the WebView2
            // process grow by gigabytes over a long batch.
            const json = await invoke('get_queue_status', { knownRev: state.convQueueRev || 0 });
            const payload = JSON.parse(json);
            const fileStatuses = payload.files || [];
            state.convQueueRev = payload.rev || 0;
            changedIdxs = new Set();
            fileStatuses.forEach(({ idx, stage, pct, badge, aa, aa_note, error }) => {
                if (idx < state.convFileQueue.length && !state.convFileQueue[idx].dismissed) {
                    const f = state.convFileQueue[idx];
                    f.filePct = pct / 10;
                    // Store badge code from backend (0=none, 1=bad, 2=skip, 3=verified_fail)
                    if (badge !== undefined) f.badge = badge;
                    // AA verdict: 0 = unknown yet, 1 = treated, 2 = analyzed & left untouched
                    if (aa !== undefined) f.aaState = aa;
                    if (aa_note) f.aaNote = aa_note;
                    // Store error message as badge hint for SKIP
                    if (badge === 2 && error) f.badgeHint = error;
                    // Anything else that carries text is a real failure on this
                    // file — keep it so the row can show why, not just that.
                    else if (error) f.errorMsg = error;
                    if      (stage === 4) f.status = 'done';
                    else if (stage === 5) f.status = 'error';
                    else if (stage === 6) f.status = 'cancelled';
                    else if (stage >= 1 && stage <= 3) f.status = 'active';
                    changedIdxs.add(idx);
                }
            });

        } catch(e) {}

        let filePct = state.convCurrentFilePct;
        if (statusText.includes('Done') || statusText.includes('All done')) filePct = 0;
        let overallPct = total > 0 ? ((done * 100 + filePct) / total) : filePct;
        if (done >= total) overallPct = 100;
        
        document.getElementById('convProgressFill').style.width = overallPct.toFixed(1) + '%';
        document.getElementById('convProgressPct').textContent  = overallPct.toFixed(0) + '%';
        document.getElementById('convProgressFiles').textContent = `${done}/${total}`;
        setStatus(statusText, statusKind(statusText));

        // Re-render only rows the backend reported as changed — a no-op
        // poll (idle tick) touches no DOM at all.
        if (changedIdxs === null || changedIdxs.size > 0) renderFileQueue(changedIdxs);

        const isCancelled = statusText.includes('Cancelled');
        const isFinished  = (done >= total && progress >= 1000);
        const isError     = statusText.includes('Error') && progress >= 1000;

        if (isCancelled || isFinished || isError) {
            clearInterval(state.convPollTimer);
            state.convPollTimer = null;
            if (isFinished && !isCancelled) {
                state.convFileQueue.forEach(f => { if (f.status !== 'error' && !f.dismissed) f.status = 'done'; });
                renderFileQueue();
                resetConverterUI();
                return;
            }
            document.getElementById('convProgressFill').style.width = '0%';
            // Leave the queue visible so user can see errors/status, it will be cleared on next conversion
            renderFileQueue(); 
            resetConverterUI();
        }
    } catch(e) {}
}

export function resetConverterUI() {
    state.convIsConverting = false;
    setConverterControlsEnabled(true);
    if (state.convPollTimer) { clearInterval(state.convPollTimer); state.convPollTimer = null; }
    document.getElementById('dzCancelBtn').style.display = 'none';
    state.convCurrentFilePct = 0;

    if (state.convNextQueue.length > 0) {
        const nextPaths = state.convNextQueue.map(f => f.path);
        state.convNextQueue = [];
        setTimeout(() => startConversion(nextPaths), 350);
        return;
    }
    document.getElementById('convProgressWrap').style.display = 'none';
    if (window._dzSyncEmpty) window._dzSyncEmpty();
}

// Presets match the fir-optimizer output tags (1M/5M/10M/30M) exactly —
// the old 4M/16M presets silently resolved to the 5M/10M filter files while
// the UI and the output filename claimed 4M/16M.
export const tapPresets = TAP_PRESETS;

export function bindConverterControls() {
    document.getElementById('convTapSlider').addEventListener('input', (e) => {
        let index = parseInt(e.target.value);
        if (index >= tapPresets.length) index = tapPresets.length - 1;
        let val = tapPresets[index];
        document.getElementById('convTapDisplay').textContent = formatTaps(val) + ' Taps';
        updateConvSpecsLine();
        refreshFilterAvailability();
        saveSettings();
    });
    document.getElementById('convFsSlider').addEventListener('input', (e) => {
        updateFsDisplay();
        updateConvSpecsLine();
        refreshFilterAvailability();
        saveSettings();
    });
    ['convWindow', 'convApodizing', 'convHeadroom', 'convFirResampling'].forEach(id => {
        const el = document.getElementById(id);
        if (el) el.addEventListener('change', () => { updateConvSpecsLine(); saveSettings(); });
    });

    document.getElementById('convAdaptiveApodizer').addEventListener('change', (e) => {
        const apodSel = document.getElementById('convApodizing');
        if (e.target.checked) {
            apodSel.value = '0'; apodSel.disabled = true; apodSel.style.opacity = '0.4';
        } else {
            apodSel.disabled = false; apodSel.style.opacity = '1';
        }
        updateConvSpecsLine(); saveSettings();
    });

    document.getElementById('convHybridPhase').addEventListener('change', (e) => {
        const winSel = document.getElementById('convWindow');
        const loadBtn = document.getElementById('convLoadFilterBtn');
        const clearBtn = document.getElementById('convClearFilterBtn');
        if (e.target.checked) {
            // Lock window selector to Kaiser (required by min-phase filter)
            winSel.value = '4'; winSel.disabled = true; winSel.style.opacity = '0.4';
            // Clear and lock the custom filter button
            if (state.convCustomFilterPath) {
                state.convCustomFilterPath = null; state.convCustomFilterName = ''; state.convCustomFilterTaps = 0;
                if (clearBtn) clearBtn.style.display = 'none';
                const info = document.getElementById('convFilterInfo');
                if (info) info.style.display = 'none';
                document.getElementById('convTapSlider').disabled = false;
                document.getElementById('convTapSlider').style.opacity = '1';
            }
            if (loadBtn) {
                loadBtn.classList.remove('active');
                loadBtn.textContent = '📂 Custom Filter (.npy)';
                loadBtn.disabled = true;
                loadBtn.style.opacity = '0.35';
                loadBtn.style.cursor = 'not-allowed';
            }
        } else {
            winSel.disabled = false; winSel.style.opacity = '1';
            if (loadBtn) {
                loadBtn.disabled = false;
                loadBtn.style.opacity = '1';
                loadBtn.style.cursor = '';
            }
        }
        updateConvSpecsLine(); refreshFilterAvailability(); saveSettings();
    });

    document.getElementById('convGpuCheck').addEventListener('change', () => {
        applySettingsDependencies();
        saveSettings();
    });

    const loadBtn = document.getElementById('convLoadFilterBtn');
    if (loadBtn) {
        loadBtn.addEventListener('click', async () => {
            try {
                const { open } = window.__TAURI__.dialog;
                const { invoke } = window.__TAURI__.tauri;
                const selected = await open({ filters: [{ name: 'NumPy', extensions: ['npy'] }] });
                if (!selected) return;
                const taps = await invoke('set_custom_filter', { path: selected });
                state.convCustomFilterPath = selected;
                state.convCustomFilterName = selected.split('\\').pop().split('/').pop();
                state.convCustomFilterTaps = taps;
                convApplyFilterUI(state.convCustomFilterName, taps);
                refreshFilterAvailability();
                saveSettings();
            } catch(e) { alert('Error: ' + e); }
        });
    }

    const clearBtn = document.getElementById('convClearFilterBtn');
    if (clearBtn) {
        clearBtn.addEventListener('click', async () => {
            const { invoke } = window.__TAURI__.tauri;
            try { await invoke('clear_custom_filter'); } catch(e) {}
            state.convCustomFilterPath = null;
            state.convCustomFilterName = '';
            state.convCustomFilterTaps = 0;

            const tapSlider = document.getElementById('convTapSlider');
            if (tapSlider) {
                tapSlider.disabled = false;
                tapSlider.style.opacity = '1';
                const idx = Math.min(parseInt(tapSlider.value) || 0, tapPresets.length - 1);
                document.getElementById('convTapDisplay').textContent = formatTaps(tapPresets[idx]) + ' Taps';
            }
            if (loadBtn) {
                loadBtn.classList.remove('active');
                loadBtn.textContent = '📂 Custom Filter (.npy)';
            }
            clearBtn.style.display = 'none';
            const info = document.getElementById('convFilterInfo');
            if (info) info.style.display = 'none';

            const win = document.getElementById('convWindow');
            if (win && !document.getElementById('convHybridPhase').checked) {
                win.disabled = false;
                win.style.opacity = '1';
            }
            updateConvSpecsLine();
            refreshFilterAvailability();
            saveSettings();
        });
    }
}
