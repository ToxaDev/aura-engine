
import { state } from './state.js';
import { formatTaps, truncateMiddle } from './helpers.js';
import { setConverterControlsEnabled, updateConvSpecsLine, updateFsDisplay } from './ui.js';
import { renderFileQueue } from './dropzone.js';
import { saveSettings, applySettingsDependencies } from './settings.js';

const { invoke } = window.__TAURI__.tauri;

// FS Multiplier presets: index → FS value
export const fsPresets = [2, 4, 8, 16];

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

export async function startConversion(filePaths) {
    state.convIsConverting = true;
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
        document.getElementById('convStatus').textContent = '✗ Error: ' + e;
        resetConverterUI();
    }
}

export async function pollConversionProgress() {
    try {
        const [progress, total, done, statusText, output, snappedRate] = await invoke('get_conversion_progress');
        state.convLastDone = done;
        state.convCurrentFilePct = progress / 10;

        try {
            const json = await invoke('get_queue_status');
            const fileStatuses = JSON.parse(json);
            fileStatuses.forEach(({ idx, stage, pct, badge, error }) => {
                if (idx < state.convFileQueue.length && !state.convFileQueue[idx].dismissed) {
                    const f = state.convFileQueue[idx];
                    f.filePct = pct / 10;
                    // Store badge code from backend (0=none, 1=bad, 2=skip, 3=verified_fail)
                    if (badge !== undefined) f.badge = badge;
                    // Store error message as badge hint for SKIP
                    if (badge === 2 && error) f.badgeHint = error;
                    if      (stage === 4) f.status = 'done';
                    else if (stage === 5) f.status = 'error';
                    else if (stage === 6) f.status = 'cancelled';
                    else if (stage >= 1 && stage <= 3) f.status = 'active';
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
        document.getElementById('convStatus').textContent = truncateMiddle(statusText);

        renderFileQueue();

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
export const tapPresets = [1000000, 5000000, 10000000, 30000000];

export function bindConverterControls() {
    document.getElementById('convTapSlider').addEventListener('input', (e) => {
        let index = parseInt(e.target.value);
        if (index >= tapPresets.length) index = tapPresets.length - 1;
        let val = tapPresets[index];
        document.getElementById('convTapDisplay').textContent = formatTaps(val) + ' Taps';
        updateConvSpecsLine();
    });
    document.getElementById('convFsSlider').addEventListener('input', (e) => {
        updateFsDisplay();
        updateConvSpecsLine();
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
        updateConvSpecsLine(); saveSettings();
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
            saveSettings();
        });
    }
}
