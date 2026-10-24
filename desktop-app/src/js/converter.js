
import { state } from './state.js';
import { formatTaps, truncateMiddle } from './helpers.js';
import {
    setConverterControlsEnabled, updateConvSpecsLine, updateFsDisplay,
    refreshFilterAvailability
} from './ui.js';
import { TAP_PRESETS, FS_PRESETS, fsForTaps, tapsForFs } from './inventory.js';
import { renderFileQueue } from './dropzone.js';
import { saveSettings, applySettingsDependencies } from './settings.js';
import { subsonicCornerHz } from './dsprack.js';
import { xtcGeometryPayload } from './xtc.js';
import { batchStarted, batchPolled, batchSummary } from './batchstats.js';
import { rackSig, rackSigWithMOverride } from './library.js';
import { getDsdOutput, refreshModulators, showDsdControls, effectiveFs, parseConvFmt, flacFsRefusal } from './dsdexport.js';
import { relistChoices } from './rackstrip.js';
import { renderStrip } from './rackstrip.js';
import { setFilterBtnHpLock, filterBtnHpLocked } from './filter-lock.js';
import { showNow } from './tooltip.js';

const { invoke } = window.__TAURI__.tauri;

// FS Multiplier presets: index → FS value. Defined in inventory.js, which is
// also what decides whether a given position has a filter behind it; re-exported
// here so callers keep addressing the converter for converter settings.
export const fsPresets = FS_PRESETS;

export function getFsMultiplier() {
    const slider = document.getElementById('convFsSlider');
    if (!slider) return 8;
    const idx = parseInt(slider.value) || 0;
    const fs = fsPresets[Math.min(idx, fsPresets.length - 1)];
    // DSF: the FS the chain runs at for that DSD rate (DSD64: FS8 at most).
    const dsd = getDsdOutput();
    return dsd ? effectiveFs(fs, dsd.multiple) : fs;
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
            hybridPhase: document.getElementById('convHybridPhase').checked,
            // TFS derives from the lin+min pair, so it needs the minimum-phase
            // blob too — and it is on by default, unlike Hybrid-Phase.
            tfsPhase: document.getElementById('convLabTfs')?.checked || false
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

    // Mark the control, so the answer stays visible after the dialog is gone:
    // the strip's GPU cell says CPU, the note under it says why.
    state.gpuNone = r.reason || 'unknown';
    renderStrip();
    const note = document.getElementById('convGpuNote');
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

/// Start a conversion batch.
///
/// `mOverride` (optional) supplies M-relevant settings directly — see
/// snapToConvertOverride in memory.js for the shape. When provided, those
/// fields override the DOM values; non-M fields (FS, taps, GPU, PFR)
/// always come from the rack DOM. The rack UI never changes.
export async function startConversion(filePaths, mOverride = null) {
    // Busy from the first moment: a C pressed during the checks below waits
    // for the next batch instead of starting one beside this.
    state.convIsConverting = true;
    state.convCancelRequested = false;
    // FLAC cannot hold a ×32 rate: nothing starts (Convert and Convert all alike).
    const fsIdx = parseInt(document.getElementById('convFsSlider')?.value) || 0;
    const fsWhy = flacFsRefusal(fsPresets[Math.min(fsIdx, fsPresets.length - 1)], getDsdOutput());
    if (fsWhy) {
        resetConverterUI();
        setStatus(fsWhy, 'error');
        return;
    }
    // Say so before the wait, not after it.
    const go = await checkFilterAvailability(filePaths) && await confirmGpuFallback();
    if (!go) {
        // Nothing started; what was asked for meanwhile runs now.
        resetConverterUI();
        return;
    }
    setStatus('Preparing…', 'busy');
    state.convQueueRev = 0; // full queue sync on the first poll of a new batch
    // The rack stays live during a batch: the batch runs with the settings it
    // was started with (they travel in convert_files), and the same rack is
    // what the player is playing through right now.
    const mo = mOverride;
    const isHp = mo != null ? !!mo.hybridPhase    : document.getElementById('convHybridPhase').checked;
    const isAa = mo != null ? !!mo.adaptiveApodizer : document.getElementById('convAdaptiveApodizer').checked;
    const subHz = mo != null ? (mo.subsonicHz ?? subsonicCornerHz()) : subsonicCornerHz();
    // Pre-compute the effective sig now, while the rack DOM reflects the batch
    // settings. For M batches the rack DOM is intentionally unchanged (M3 rule),
    // so rackSig() would capture the rack's current state (different settings).
    // rackSigWithMOverride mixes non-M DOM fields with the M override values so
    // the stored sig matches what rackSig() returns once the rack is set to those
    // same M settings.
    const batchSig = mo != null ? rackSigWithMOverride(mo) : null;

    state.convFileQueue = filePaths.map(f => ({
        path: f,
        name: f.split('\\').pop().split('/').pop(),
        status: 'pending',
        hp: isHp,
        aa: isAa,
        sub: subHz,
        // batchSig is non-null only for M batches; linkConversions will use it
        // instead of rackSig() when writing f.sig.
        mSig: batchSig,
        // Carry the override so linkConversions can distinguish M batches even
        // after batchSig is consumed.
        mOverride: mo,
    }));
    batchStarted(state.convFileQueue);

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
            customFilterPath: state.convCustomFilterPath,
            useGpu: document.getElementById('convGpuCheck').checked,
            useFirResampling: document.getElementById('convFirResampling')?.checked || false,
            apodizing: mo != null ? mo.apodizing
                : parseInt(document.getElementById('convApodizing')?.value || '0'),
            headroomDb: mo != null ? mo.headroomDb
                : parseFloat(document.getElementById('convHeadroom')?.value || '0'),
            adaptiveApodizer: isAa,
            hybridPhase: isHp,
            iirDcBlocking: document.getElementById('convIirDc')?.checked || false,
            subsonicHz: subHz,
            // FLAC (null) or DSF at a DSD rate through a modulator.
            dsdOutput: getDsdOutput(),
            // The settings the rows' records will carry, hashed into the render's stamp.
            rackSig: batchSig ?? rackSig(),
            // A batch decision, from the rack whatever a track's memory says.
            albumLevel: document.getElementById('convAlbumLevel')?.checked ?? true,
            // The hidden upsampler switch, sent only when set.
            ...(state.upsampler ? { upsampler: state.upsampler } : {}),
            labFeatures: mo != null ? mo.labFeatures : {
                declip: document.getElementById('convLabDeclip')?.checked || false,
                isp: document.getElementById('convLabIsp')?.checked || false,
                tfsPhase: document.getElementById('convLabTfs')?.checked || false,
                continuousAlpha: document.getElementById('convLabAlpha')?.checked || false,
                adaptiveHeadroom: document.getElementById('convLabHeadroom')?.checked || false,
                xtc: document.getElementById('convLabXtc')?.checked || false,
                xtcGeometry: xtcGeometryPayload(),
            }
        });
        state.convPollTimer = setInterval(pollConversionProgress, 200);
        // Cancelled while the backend was taking the batch: a cancel that
        // came before the batch reset its flag is said again, now.
        if (state.convCancelRequested) invoke('cancel_conversion').catch(() => {});
    } catch(e) {
        setStatus('Error: ' + e, 'error');
        // The backend never took this batch: its rows say so instead of waiting.
        // Clear pending M groups — the batch failed before it started.
        state.convFileQueue.forEach(f => { f.status = 'error'; f.errorMsg = String(e); });
        renderFileQueue();
        resetConverterUI(true);
    }
}

export async function pollConversionProgress() {
    const timer = state.convPollTimer;
    try {
        const [progress, total, done, statusText, output, snappedRate, running, cancelled] = await invoke('get_conversion_progress');
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
            fileStatuses.forEach(({ idx, stage, pct, badge, aa, aa_note, error, lab, output }) => {
                if (idx < state.convFileQueue.length && !state.convFileQueue[idx].dismissed) {
                    const f = state.convFileQueue[idx];
                    f.filePct = pct / 10;
                    // Store badge code from backend (0=none, 1=bad, 2=skip, 3=verified_fail)
                    if (badge !== undefined) f.badge = badge;
                    // AA verdict: 0 = unknown yet, 1 = treated, 2 = analyzed & left untouched
                    if (aa !== undefined) f.aaState = aa;
                    if (aa_note) f.aaNote = aa_note;
                    // The chain as the backend has it so far: [tok, state, why]
                    // per stage that has been reached. Stages that are on but
                    // absent here simply have not run yet.
                    if (Array.isArray(lab)) f.labChain = lab;
                    // Store error message as badge hint for SKIP
                    if (badge === 2 && error) f.badgeHint = error;
                    // Anything else that carries text is a real failure on this
                    // file — keep it so the row can show why, not just that.
                    else if (error) f.errorMsg = error;
                    // Keep the output path when the backend reports it done,
                    // so library.js can push it into the entry's convs record.
                    if (stage === 4 && output) f.outPath = output;
                    if      (stage === 4) f.status = 'done';
                    else if (stage === 5) f.status = 'error';
                    else if (stage === 6) f.status = 'cancelled';
                    else if (stage >= 1 && stage <= 3) f.status = 'active';
                    changedIdxs.add(idx);
                }
            });
            batchPolled();

        } catch(e) {}

        let filePct = state.convCurrentFilePct;
        if (statusText.includes('Done') || statusText.includes('All done')) filePct = 0;
        let overallPct = total > 0 ? ((done * 100 + filePct) / total) : filePct;
        if (done >= total) overallPct = 100;
        
        state.convOverallPct = overallPct;
        document.getElementById('convProgressFill').style.width = overallPct.toFixed(1) + '%';
        document.getElementById('convProgressPct').textContent  = overallPct.toFixed(0) + '%';
        document.getElementById('convProgressFiles').textContent = `${done}/${total}`;
        // Update the Convert all button's progress fill directly so it advances
        // even on no-op poll ticks when renderFileQueue is skipped.
        const _caFill = document.getElementById('plCaProgress');
        const _caText = document.getElementById('plCaText');
        if (_caFill) _caFill.style.width = overallPct.toFixed(1) + '%';
        if (_caText) _caText.textContent = overallPct.toFixed(0) + '%';
        // A cancelled batch finishes the stage it is in before it stops.
        if (state.convCancelRequested && running !== false) setStatus('Cancelling…', 'busy');
        else setStatus(statusText, statusKind(statusText));

        // Re-render only rows the backend reported as changed — a no-op
        // poll (idle tick) touches no DOM at all.
        if (changedIdxs === null || changedIdxs.size > 0) renderFileQueue(changedIdxs);

        // The batch is over when the backend's batch thread is (`running`):
        // not when the status line says so (the player writes that line
        // too), and not when every file has ended (the thread still winds
        // down then, and a batch started that moment would be refused).
        const ended = typeof running === 'boolean'
            ? !running
            : (done >= total && progress >= 1000) || statusText.includes('Cancelled')
              || (statusText.includes('Error') && progress >= 1000);
        // Only the poll of the batch still polled ends it: a late tick of a
        // batch already ended must not end the one started after it.
        if (ended && timer === state.convPollTimer) endBatch(cancelled === true || statusText.includes('Cancelled'));
    } catch(e) {}
}

/// The backend's batch is over: the poll stops, and the page is back to
/// idle — or runs what was asked for while it ran — whatever else fails on
/// the way. Before, a throw here came after the poll had stopped and left
/// the page converting for good: every C after it waited for a next batch
/// that never came.
function endBatch(isCancelled) {
    clearInterval(state.convPollTimer);
    state.convPollTimer = null;
    let clean = true;
    try {
        clean = finishBatch(isCancelled);
    } finally {
        state.convCancelRequested = false;
        resetConverterUI(!clean);
        renderFileQueue();
    }
}

/// Bring the rows to the batch's end and say how it went. False when the
/// batch did not end well (cancelled, or failed as a whole).
function finishBatch(isCancelled) {
    // A file the backend still had going is not going any more.
    state.convFileQueue.forEach(f => {
        if (f.status === 'active' || f.status === 'pending') {
            f.status = isCancelled ? 'cancelled' : 'done';
            if (isCancelled) f.filePct = 0;
        }
    });
    if (isCancelled) {
        // Leave the rows as they ended (cancelled, errors) until the next conversion.
        document.getElementById('convProgressFill').style.width = '0%';
        setStatus('Cancelled', 'idle');
        return false;
    }
    batchPolled();
    // How fast it went, in place of the backend's "All done".
    const stats = batchSummary();
    if (stats) {
        setStatus(stats.text, 'ok');
        document.getElementById('convStatus').title = stats.title;
        // Short result for the Convert all button ("✓ ×25.3").
        state.convAllResult = { text: stats.short, title: stats.title };
        // And in the session's log, line by line.
        for (const message of stats.log) {
            window.__TAURI__.tauri.invoke('player_ui_log', { level: 'convert', message }).catch(() => {});
        }
    }
    return true;
}

// clearPending: pass true when the batch ended abnormally (cancelled, error)
// so the groups a Convert all left for later do not start. What was asked
// for after the batch began (the next queue) still runs: a cancel empties
// that queue when it is given, so whatever is in it now came after the
// cancel — a C pressed while the cancelled batch was still stopping.
export function resetConverterUI(clearPending = false) {
    state.convIsConverting = false;
    state.convOverallPct = 0;
    setConverterControlsEnabled(true);
    if (state.convPollTimer) { clearInterval(state.convPollTimer); state.convPollTimer = null; }
    document.getElementById('dzCancelBtn').style.display = 'none';
    state.convCurrentFilePct = 0;

    if (clearPending) {
        state.convMPendingGroups = [];
        state.convAllResult = null;
    }

    if (state.convNextQueue.length > 0) {
        const queue = [...state.convNextQueue];
        state.convNextQueue = [];

        // Group by mOverride so a mixed next-queue (M and non-M rows, or M
        // rows with different settings) runs each group with the right sig.
        // Non-M entries go first; each distinct M override is its own group.
        const nonM = queue.filter(f => !f.mOverride);
        const mMap = new Map();
        for (const f of queue.filter(f => f.mOverride)) {
            const sig = JSON.stringify(f.mOverride);
            if (!mMap.has(sig)) mMap.set(sig, { mOverride: f.mOverride, paths: [] });
            mMap.get(sig).paths.push(f.path);
        }
        const groups = [];
        if (nonM.length > 0) groups.push({ paths: nonM.map(f => f.path), mOverride: null });
        for (const { mOverride, paths } of mMap.values()) groups.push({ paths, mOverride });

        if (groups.length > 0) {
            // Queue remaining groups so the next resetConverterUI picks them up.
            for (let i = 1; i < groups.length; i++) state.convMPendingGroups.push(groups[i]);
            const first = groups[0];
            // Started now, not after a pause: the backend's batch thread has
            // ended (the poll waits for that), and in a pause the queued rows
            // would look idle and take a C of their own. Render afterwards
            // whatever happened.
            startConversion(first.paths, first.mOverride).finally(renderFileQueue);
            return;
        }
    }
    if (state.convMPendingGroups && state.convMPendingGroups.length > 0) {
        const { paths, mOverride } = state.convMPendingGroups.shift();
        startConversion(paths, mOverride).finally(renderFileQueue);
        return;
    }
    document.getElementById('convProgressWrap').style.display = 'none';
    if (window._dzSyncEmpty) window._dzSyncEmpty();
}

// Presets match the fir-optimizer output tags (5k/1M/5M/10M/30M) exactly —
// the old 4M/16M presets silently resolved to the 5M/10M filter files while
// the UI and the output filename claimed 4M/16M.
export const tapPresets = TAP_PRESETS;

/// The Convert all format part's choice (`convFmtValue`): the Output pane's
/// controls set to it and a change sent, as if it was picked there — saved,
/// the specs line and the strip brought up, the rows checked again.
export async function setConvOutput(v) {
    const fmt = document.getElementById('convOutFormat');
    if (!fmt) return;
    const p = parseConvFmt(v);
    fmt.value = p ? 'dsf' : 'flac';
    if (p) {
        const rate = document.getElementById('convDsdRate');
        if (rate && rate.value !== String(p.multiple)) {
            rate.value = String(p.multiple);
            await refreshModulators(invoke, relistChoices, p.kind);
        }
        const mod = document.getElementById('convDsdModulator');
        if (mod) mod.value = p.kind;
    }
    fmt.dispatchEvent(new Event('change', { bubbles: true }));
}

export function bindConverterControls() {
    // The output format: FLAC, or DSF with its rate and modulator.
    const dsdChanged = () => { showDsdControls(); updateConvSpecsLine(); refreshFilterAvailability(); saveSettings(); };
    document.getElementById('convOutFormat')?.addEventListener('change', dsdChanged);
    document.getElementById('convDsdRate')?.addEventListener('change', async () => {
        await refreshModulators(invoke, relistChoices, document.getElementById('convDsdModulator')?.value || state.convDsdModulatorSaved);
        dsdChanged();
    });
    document.getElementById('convDsdModulator')?.addEventListener('change', dsdChanged);
    // At start and once the saved settings are back (settings.js): the list
    // for the restored rate, the restored modulator in it — nothing saved.
    const dsdRestore = () => {
        showDsdControls();
        refreshModulators(invoke, relistChoices, state.convDsdModulatorSaved).then(() => updateConvSpecsLine());
    };
    window.addEventListener('aura-dsd-restored', dsdRestore);
    dsdRestore();

    document.getElementById('convTapSlider').addEventListener('input', (e) => {
        let index = parseInt(e.target.value);
        if (index >= tapPresets.length) index = tapPresets.length - 1;
        let val = tapPresets[index];
        document.getElementById('convTapDisplay').textContent = formatTaps(val) + ' Taps';
        // A long rung the FS does not offer takes the FS up to its lowest one.
        const fsSlider = document.getElementById('convFsSlider');
        const fs = fsPresets[Math.min(parseInt(fsSlider.value) || 0, fsPresets.length - 1)];
        const fsTo = fsForTaps(val, fs);
        if (fsTo !== fs) { fsSlider.value = String(fsPresets.indexOf(fsTo)); updateFsDisplay(); }
        updateConvSpecsLine();
        refreshFilterAvailability();
        saveSettings();
    });
    document.getElementById('convFsSlider').addEventListener('input', (e) => {
        // An FS below the rung's lowest takes the taps down to the longest rung offered there.
        const tapSlider = document.getElementById('convTapSlider');
        if (!state.convCustomFilterPath) {
            const taps = tapPresets[Math.min(parseInt(tapSlider.value) || 0, tapPresets.length - 1)];
            const fs = fsPresets[Math.min(parseInt(e.target.value) || 0, fsPresets.length - 1)];
            const tapsTo = tapsForFs(taps, fs);
            if (tapsTo !== taps) {
                tapSlider.value = String(tapPresets.indexOf(tapsTo));
                document.getElementById('convTapDisplay').textContent = formatTaps(tapsTo) + ' Taps';
            }
        }
        updateFsDisplay();
        updateConvSpecsLine();
        refreshFilterAvailability();
        saveSettings();
    });
    ['convApodizing', 'convHeadroom', 'convFirResampling',
     'convSubsonic', 'convSubsonicHz'].forEach(id => {
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
        const loadBtn = document.getElementById('convLoadFilterBtn');
        const clearBtn = document.getElementById('convClearFilterBtn');
        if (e.target.checked) {
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
            }
        }
        setFilterBtnHpLock(loadBtn, e.target.checked);
        updateConvSpecsLine(); refreshFilterAvailability(); saveSettings();
    });

    document.getElementById('convGpuCheck').addEventListener('change', () => {
        applySettingsDependencies();
        saveSettings();
    });

    const loadBtn = document.getElementById('convLoadFilterBtn');
    if (loadBtn) {
        loadBtn.addEventListener('click', async (ev) => {
            // Held for Hybrid-Phase: the press says why (filter-lock.js),
            // kept from the document's click that hides tooltips.
            if (filterBtnHpLocked(loadBtn)) {
                ev.stopPropagation();
                if (ev.detail > 0) showNow(loadBtn, ev.clientX, ev.clientY);
                else showNow(loadBtn);
                return;
            }
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

            updateConvSpecsLine();
            refreshFilterAvailability();
            saveSettings();
        });
    }
}
