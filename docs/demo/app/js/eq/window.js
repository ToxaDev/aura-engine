// eq/window.js — EQ window glue: state, UI, Tauri bridge.

import { autoPreamp, safeLevel, autoGain, VIEW_FS, hasGain, isCut } from './math.js';
import {
    flatState, normalizeState, normalizeBand, normalizePresets,
    savePreset, renamePreset, deletePreset,
    History, eqActive, balanceOf, FLAT_NAME, MAX_BANDS, shortName, BUILTIN_PRESETS, isReservedName,
    delayOf, speakerFields, withSpeakers, speakersActive,
} from './state.js';
import { createGraph, FREQS, NPT, fmtF, fmtDb } from './graph.js';
import { parseFreq, parseNum, noteOf, nudge, idRange, F_LO, F_HI, G_MAX, Q_LO, Q_HI, fmtBalance, parseBalance, balanceFromPos, posFromBalance,
    fmtDelay, fmtDelayCm, parseDelay, delayFromPos, posFromDelay, speakerCalc, soundSpeed, UNIT_M } from './controls.js';
import {
    parseProfile, exportApo, describe,
    autoEqApi, searchAutoEq, autoEqCredit, AUTOEQ_SITE,
} from './profile-adapter.js';
import { auraUrl } from '../aura-url.js';
import { createAnalyzer, mountAnalyzerControls } from './spectrum.js';

// ── Tauri wrappers ──────────────────────────────────────────────────────────
const T = () => window.__TAURI__;
const invoke = (cmd, args) => T()?.core.invoke(cmd, args) ?? Promise.resolve(null);
const emitEv = (name, payload) => T()?.event.emit(name, payload)?.catch(() => {});
// Links open in the system browser; a plain <a href> would navigate the window itself.
document.addEventListener('click', e => {
    const a = e.target.closest?.('a[href^="http"]');
    if (a) { e.preventDefault(); openExternal(a.href); }
});
const listenEv = async (name, handler) => {
    try { return await T()?.event.listen(name, e => handler(e.payload)); } catch (_) {}
};
const openExternal = url => {
    try { T()?.shell.open(url); } catch (_) { window.open(url, '_blank', 'noopener'); }
};

// ── Constants ───────────────────────────────────────────────────────────────
const HUES = [
    '#22d3ee', '#a78bfa', '#facc15', '#34d399', '#fb923c',
    '#f472b6', '#60a5fa', '#a3e635', '#fb7185', '#2dd4bf',
    '#e879f9', '#cbd5e1',
];
const $ = id => document.getElementById(id);
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

// ── State ───────────────────────────────────────────────────────────────────
let _state = flatState();        // current EqState (normalised)
let _bands = [];                 // internal band objects: [...band fields, id, color, curve]
let _selId = null;               // the primary selected band
let _selIds = new Set();         // the whole selection (holds _selId)
let _nextId = 1;
let _presets = [];               // [{name, state}]
let _range = 12;
let _bitPerfect = false;
let _hist = new History(60);
let _abSlots = { A: null, B: null };
let _abSel = 'A';
let _engineGain = null;          // the gain mode's level in dB from eq:engine, or null
let _engineClipRisk = null;
let _clipTimer = 0;
let _clipBusy = false;            // one status request at a time
let _clipBase = null;            // eqClipped at the last reset (the count runs since the app started)
let _clipOverDb = 0;             // the highest over seen since the last reset
let _drawFs = VIEW_FS;
let _graph = null;
let _spec = null;                // the analyzer (eq/spectrum.js)
let _emitTimer = 0;
let _pendingEmit = false;
let _toastTimer = 0;
let _dragDepth = 0;
let _pasteTimer = 0;
let _saveNameMode = false;
let _autoEq = null;              // autoEqApi instance (created once)

// ── Helpers ─────────────────────────────────────────────────────────────────
const bandToState = b => ({ type: b.type, f: b.f, gain: b.gain, q: b.q, slope: b.slope, on: b.on });
const stateBands = bands => bands.map(bandToState);

function syncStateFromBands() {
    _state = { ..._state, bands: stateBands(_bands) };
}

function makeBand(type, f, gain, q, slope) {
    const id = _nextId++;
    const b = { id, color: HUES[(id - 1) % HUES.length], curve: null, type, f, gain: gain || 0, q: q || 0.71, slope: slope || 12, on: true };
    _graph?.recalcBand(b);
    return b;
}

// ── Snapshots (undo/redo, A/B) ───────────────────────────────────────────────
function snapShot() {
    return JSON.stringify({ state: _state, selId: _selId, nextId: _nextId, range: _range });
}

function restoreSnap(json) {
    const o = JSON.parse(json);
    _state = normalizeState(o.state);
    _nextId = o.nextId || 1;
    _range = o.range || 12;
    // Rebuild internal bands from the stored state (re-assign sequential ids/colors)
    _bands = _state.bands.map((sb, i) => {
        const b = { id: i + 1, color: HUES[i % HUES.length], curve: null, ...sb };
        _graph?.recalcBand(b);
        return b;
    });
    _nextId = _bands.length + 1;
    _selId = o.selId;
    if (_selId && !_bands.find(b => b.id === _selId)) _selId = _bands[0]?.id ?? null;
    _selIds.clear();
}

// ── Undo / Redo ──────────────────────────────────────────────────────────────
function histPush() { _hist.push(snapShot()); updHist(); }
function updHist() {
    const u = $('undoB'), r = $('redoB');
    if (u) u.disabled = !_hist.canUndo;
    if (r) r.disabled = !_hist.canRedo;
}
function doUndo() { const p = _hist.back(snapShot()); if (!p) return; restoreSnap(p); afterEdit(true); updHist(); }
function doRedo() { const p = _hist.forward(snapShot()); if (!p) return; restoreSnap(p); afterEdit(true); updHist(); }

// ── State emission (throttled ≤30/s, final on every discrete edit) ───────────
function emitState(final) {
    if (final) {
        clearTimeout(_emitTimer); _emitTimer = 0; _pendingEmit = false;
        doEmitState(true);
    } else {
        _pendingEmit = true;
        if (!_emitTimer) _emitTimer = setTimeout(() => { _emitTimer = 0; if (_pendingEmit) { _pendingEmit = false; doEmitState(false); } }, 33);
    }
}

function doEmitState(final) {
    syncStateFromBands();
    // The main window owns the engine link: it sends player_set_eq.
    emitEv('eq:state', { state: _state, final });
}

// ── A/B slots ────────────────────────────────────────────────────────────────
function switchAb(s) {
    if (s === _abSel) return;
    _abSlots[_abSel] = snapShot();
    _abSel = s;
    // The speakers are the room's, not a slot's: they stay as they are.
    const spk = speakerFields(_state);
    if (_abSlots[s]) restoreSnap(_abSlots[s]); else _abSlots[s] = snapShot();
    _state = withSpeakers(_state, spk);
    afterEdit(true);
    $('abA')?.classList.toggle('on', _abSel === 'A');
    $('abB')?.classList.toggle('on', _abSel === 'B');
}

// ── Presets ──────────────────────────────────────────────────────────────────
function loadFlatState(noHist) {
    if (!noHist) histPush();
    _state = withSpeakers(flatState(), _state); _bands = []; _selId = null; _selIds.clear(); _nextId = 1;
    afterEdit(true);
}

function applyPreset(name, noHist) {
    if (name === FLAT_NAME) { loadFlatState(noHist); return; }
    const b = BUILTIN_PRESETS.find(p => p.name === name);
    const p = b || _presets.find(p => p.name === name);
    if (!p) return;
    if (!noHist) histPush();
    // A built-in one keeps the EQ on or off as it is; a user's as it was saved.
    // The speakers stay: a preset brings a sound, not the room.
    _state = withSpeakers(b ? { ...p.state, on: _state.on } : { ...p.state }, _state);
    rebuildBandsFromState();
    afterEdit(true);
}

function rebuildBandsFromState() {
    _nextId = 1;
    _bands = _state.bands.map((sb, i) => {
        const b = { id: _nextId++, color: HUES[i % HUES.length], curve: null, ...sb };
        _graph?.recalcBand(b);
        return b;
    });
    _selId = null;
    _selIds.clear();
}

function buildPresetSelect() {
    const sel = $('presetSel');
    if (!sel) return;
    // Keep only Flat (first) and Custom (last); between them the built-in
    // presets, then the user's after a rule.
    while (sel.options.length > 2) sel.remove(1);
    const custom = sel.options[1];
    const add = (value, text, disabled) => {
        const opt = document.createElement('option');
        opt.value = value; opt.textContent = text; opt.disabled = !!disabled;
        sel.add(opt, custom);
    };
    BUILTIN_PRESETS.forEach(p => add(p.name, p.name));
    if (_presets.length) add('', '──────────', true);
    _presets.forEach(p => add(p.name, p.name));
    syncPresetSelect();
}

function syncPresetSelect() {
    const sel = $('presetSel');
    if (!sel) return;
    const name = _state.name;
    let found = false;
    for (const o of sel.options) { if (o.value === name && o.value !== 'custom') { found = true; break; } }
    if (name === FLAT_NAME && !_bands.length) found = true;
    const customOpt = sel.querySelector('[value="custom"]');
    if (customOpt) customOpt.hidden = found;
    sel.value = found ? name : 'custom';
    $('proOn')?.classList.toggle('on', !!_state.on);
}

function markCustom() {
    if (_state.name !== 'Custom') { _state = { ..._state, name: 'Custom' }; }
    syncPresetSelect();
}

// ── Band management ───────────────────────────────────────────────────────────
function addBand(type, f, gain) {
    if (_bands.length >= MAX_BANDS) { toast('<span class="warn">24 bands max</span>'); return null; }
    histPush();
    const b = makeBand(type, f, hasGain(type) ? (gain || 0) : 0, isCut(type) ? 0.71 : 1.0, 12);
    _bands.push(b); _bands.sort((a, z) => a.f - z.f); setSelection([b.id], b.id);
    markCustom(); afterEdit(true);
    return b;
}

/// Delete these bands (the selection when none are given).
function deleteBands(ids) {
    const set = new Set(ids ?? _selIds);
    const i = _bands.findIndex(b => set.has(b.id));
    if (i < 0) return;
    histPush();
    _bands = _bands.filter(b => !set.has(b.id));
    const next = _bands[Math.min(i, _bands.length - 1)]?.id ?? null;
    setSelection(next == null ? [] : [next], next);
    markCustom(); afterEdit(true);
}
const deleteBand = id => deleteBands(id == null ? null : [id]);

/// Bypass these bands, or turn them all on when they all are bypassed.
function toggleBands(list) {
    if (!list.length) return;
    histPush();
    const on = !list.some(b => b.on);
    list.forEach(b => { b.on = on; });
    markCustom(); afterEdit(true);
}
const selBands = () => _bands.filter(b => _selIds.has(b.id));

// ── Selection ─────────────────────────────────────────────────────────────────
function setSelection(ids, primary) {
    _selIds = new Set(ids);
    _selId = primary ?? null;
    if (_selId != null) _selIds.add(_selId);
}
function syncSel() {
    if (_selId != null && !_bands.some(b => b.id === _selId)) _selId = null;
    if (_selId != null) _selIds.add(_selId);
    for (const id of [..._selIds]) if (!_bands.some(b => b.id === id)) _selIds.delete(id);
    if (_selId == null && _selIds.size) _selId = [..._selIds].pop();
}
/// Show a selection change without touching the state.
function refreshSel() {
    syncSel();
    _graph?.setSel(_selId, [..._selIds]);
    buildPanel(); buildChips();
}

// ── Import ────────────────────────────────────────────────────────────────────
let _pending = null, _pendingLabel = '', _impMode = 'replace', _usePre = true;

const ERR_NONE = 'No EQ filters found. Use ParametricEQ.txt from AutoEq, config.txt from Equalizer APO or Peace, or a REW filter export.';

function applyImport(r, mode, withPre, label) {
    histPush();
    const nb = r.bands.slice(0, MAX_BANDS);
    // Rebuild as internal bands
    const newBands = nb.map((sb, i) => {
        const b = { id: i + 1, color: HUES[i % HUES.length], curve: null, ...sb };
        _graph?.recalcBand(b);
        return b;
    });
    if (mode === 'add') {
        _bands = [..._bands, ...newBands].slice(0, MAX_BANDS);
        _bands.forEach((b, i) => { b.id = i + 1; b.color = HUES[i % HUES.length]; });
    } else {
        _bands = newBands;
    }
    _nextId = _bands.length + 1;
    _bands.sort((a, z) => a.f - z.f);
    _selId = _bands[0]?.id ?? null;
    _selIds.clear();
    const pre = withPre && typeof r.preamp === 'number';
    _state = { ..._state, name: shortName(label) || 'Custom', bands: stateBands(_bands),
        ...(pre ? { gain: 'manual', output: clamp(r.preamp, -30, 30) } : {}) };
    delete _state.preamp;
    // Auto-range: expand if needed
    const mx = Math.max(6, ..._bands.map(b => Math.abs(b.gain)));
    if (mx > _range) { _range = mx > 12 ? 30 : 12; updateRange(_range); }
    afterEdit(true);
    toast('Imported' + (label ? ' <b>' + esc(label) + '</b>' : '') + ' · ' + describe(r), true);
}

function quickImport(txt, label) {
    const r = parseProfile(txt);
    if (r.kind === 'graphic') { toast('GraphicEQ is not supported yet. Download the ParametricEQ file from AutoEq instead.'); return; }
    if (!r.bands.length) { toast(esc(ERR_NONE)); return; }
    applyImport(r, 'replace', true, label);
}

// ── Export ────────────────────────────────────────────────────────────────────
function getExportText() {
    syncStateFromBands();
    const g = modeGainDb();
    return exportApo({ ..._state, preamp: g === null ? null : g + outDb() }, { autoPreamp: autoPreamp(_bands.filter(b => b.on)) + outDb() });
}

async function saveExportFile() {
    if (!T()) {
        const a = document.createElement('a');
        a.href = URL.createObjectURL(new Blob([getExportText()], { type: 'text/plain' }));
        a.download = (_state.name || 'aura-eq').replace(/[^\w-]/g, '_') + '.txt';
        a.click();
        return;
    }
    try {
        const path = await T().dialog.save({ defaultPath: (_state.name || 'aura-eq').replace(/[^\w-]/g, '_') + '.txt', filters: [{ name: 'Text', extensions: ['txt'] }] });
        if (!path) return;
        await T().fs.writeTextFile(path, getExportText());
    } catch (e) { toast('Could not save file: ' + esc(String(e))); }
}

// ── Spectrum ──────────────────────────────────────────────────────────────────
function setupSpectrum() {
    _spec = createAnalyzer({
        fetchFrame: async (n, res) => {
            const r = await fetch(auraUrl(`/player/eq_spectrum?n=${n}&res=${res}`));
            return r.ok ? r.arrayBuffer() : null;
        },
        markDirty: () => _graph?.markDirty(),
    });
    // The graph calls the drawer behind its curves, with its (zoomable) view.
    _graph?.setSpectrumDrawer?.((ctx, view) => _spec.draw(ctx, view));
    const sync = on => _graph?.setSpecMode(on ? 'on' : 'off');
    mountAnalyzerControls(_spec, { seg: $('spec'), btn: $('specOpts'), menu: $('specMenu'), onToggle: sync });
    sync(_spec.on);
    _spec.setVisible(!document.hidden);
}

// ── Status line ───────────────────────────────────────────────────────────────
// Gain modes (state.gain): manual = output only; safe = minus the peak boost;
// auto = equal loudness with the EQ on and off (the engine's figure).
const GAIN_MODES = ['manual', 'auto', 'safe'];
const gainMode = () => (GAIN_MODES.includes(_state.gain) ? _state.gain : 'manual');
const outDb = () => (typeof _state.output === 'number' ? _state.output : 0);

/// The mode's level: the engine's when it replied, else what the window can tell.
function modeGainDb() {
    if (_engineGain !== null) return _engineGain;
    const m = gainMode();
    if (m === 'manual') return 0;
    const on = _bands.filter(b => b.on);
    return m === 'safe' ? safeLevel(on, _drawFs) : autoGain(on, _drawFs);
}

function updateStatus() {
    const el = $('status');
    if (!el) return;
    syncStateFromBands();
    const m = gainMode(), g = modeGainDb(), out = outDb();
    let txt = 'Gain <b>' + (g === null ? 'auto' : fmtDb(g + out)) + '</b>'
        + (g !== null && m !== 'manual' ? ' ' + m : '');
    // Clipping risk: the peak boost plus the level above 0 dBFS
    let clipRisk = _engineClipRisk;
    if (clipRisk === null && g !== null) {
        const peak = -autoPreamp(_bands.filter(b => b.on));
        if (peak + g + out > 0.05) clipRisk = peak + g + out;
    }
    if (clipRisk !== null && clipRisk > 0) txt += ' <span class="warn">⚠ may clip by up to ' + clipRisk.toFixed(1) + ' dB</span>';
    const bal = balanceOf(_state), del = delayOf(_state);
    let spk = '';
    if (bal.balance !== 0) spk += ' · Balance <b>' + fmtBalance(bal.balance, bal.balanceMode) + '</b>';
    if (del !== 0) spk += ' · Delay <b>' + fmtDelay(del) + '</b>';
    if (!_state.on && speakersActive(_state)) txt = 'EQ off · speakers on' + spk;
    else {
        txt += spk;
        if (!eqActive(_state)) txt += ' <span class="warn">· bypassed</span>';
    }
    el.innerHTML = txt;
    el.title = el.textContent;
    $('gainMode')?.querySelectorAll('button').forEach(b => b.classList.toggle('on', b.dataset.v === m));
}

// ── Output gain (±30 dB): slider, field, wheel, drag on the label, double-click = 0 ──
function setOutput(v, final) {
    _state = { ..._state, output: clamp(Math.round(v * 10) / 10, -30, 30) };
    syncOutput();
    markCustom(); emitState(final); updateStatus();
}
function syncOutput() {
    const v = outDb();
    const og = $('outGain'); if (og) og.value = String(v);
    const f = $('outGainV');
    if (f && document.activeElement !== f) f.value = fmtDb(v).replace(/^[+−](0\.0 dB)$/, '$1');
    syncBalance();
    syncDelay();
}

// ── Balance (±30 dB, only lowers): slider, field, wheel, drag, double-click = C ──
function setBalance(v, final) {
    const { balance, balanceMode } = balanceOf({ balance: v, balanceMode: _state.balanceMode });
    const { balance: _b, ...rest } = _state;
    _state = { ...rest, ...(balance !== 0 ? { balance } : {}), ...(balanceMode === 'ms' ? { balanceMode } : {}) };
    syncBalance();
    emitState(final); updateStatus();
}
function setBalanceMode(mode) {
    const { balanceMode: _m, ...rest } = _state;
    _state = { ...rest, ...(mode === 'ms' ? { balanceMode: 'ms' } : {}) };
    syncBalance();
    emitState(true); updateStatus();
}
function syncBalance() {
    const { balance, balanceMode } = balanceOf(_state);
    const s = $('balance');
    // the slider's own position stays while it is dragged (its grid is finer than 0.01 dB near the centre)
    if (s && document.activeElement !== s) s.value = String(posFromBalance(balance));
    const f = $('balanceV');
    if (f && document.activeElement !== f) f.value = fmtBalance(balance, balanceMode);
    $('balMode')?.querySelectorAll('button').forEach(b => b.classList.toggle('on', b.dataset.v === balanceMode));
}

// ── Speaker delay (±30 ms, one channel late): slider, field, wheel, drag, double-click = 0 ──
function setDelay(v, final) {
    const delay = delayOf({ delay: v });
    const { delay: _d, ...rest } = _state;
    _state = { ...rest, ...(delay !== 0 ? { delay } : {}) };
    syncDelay();
    emitState(final); updateStatus();
}
function setSpeakersAlways(on) {
    const { speakersAlways: _a, ...rest } = _state;
    _state = { ...rest, ...(on ? { speakersAlways: true } : {}) };
    syncDelay();
    emitState(true); updateStatus();
}
function syncDelay() {
    const ms = delayOf(_state);
    const s = $('delay');
    if (s && document.activeElement !== s) s.value = String(posFromDelay(ms));
    const f = $('delayV');
    if (f && document.activeElement !== f) f.value = fmtDelay(ms);
    const cm = $('delayCm');
    if (cm) cm.textContent = ms ? '≈ ' + fmtDelayCm(ms) : '';
    const sw = $('spkAlways');
    if (sw) { const on = _state.speakersAlways === true; sw.classList.toggle('on', on); sw.setAttribute('aria-checked', String(on)); }
}

// ── CLIP light: samples over full scale after the EQ, from the player's status ──
async function pollClip() {
    clearTimeout(_clipTimer); _clipTimer = 0;
    if (document.hidden || _clipBusy) return;
    _clipBusy = true;
    try {
        const r = await fetch(auraUrl('/player/status'));
        if (r.ok) {
            const st = await r.json();
            const n = typeof st.eqClipped === 'number' ? st.eqClipped : null;
            if (n !== null) {
                if (_clipBase === null || n < _clipBase) _clipBase = n;
                if (typeof st.eqOverDb === 'number' && n > _clipBase) _clipOverDb = Math.max(_clipOverDb, st.eqOverDb);
                showClip(n - _clipBase);
            }
        }
    } catch (_) {}
    _clipBusy = false;
    _clipTimer = setTimeout(pollClip, 500);
}
function showClip(count) {
    const c = $('clipL');
    if (!c) return;
    c.classList.toggle('lit', count > 0);
    c.title = count > 0
        ? count + ' samples over full scale after the EQ' + (_clipOverDb > 0 ? ', up to +' + _clipOverDb.toFixed(1) + ' dB' : '') + '. Click to reset.'
        : 'No clipping after the EQ';
}
function resetClip() { _clipBase = null; _clipOverDb = 0; showClip(0); pollClip(); }

// ── Welcome screen ────────────────────────────────────────────────────────────
function updWelcome() {
    const w = $('welcome');
    if (w) w.hidden = !(_state.bands.length === 0 && _state.name === FLAT_NAME);
}

// ── After edit ────────────────────────────────────────────────────────────────
function afterEdit(final) {
    syncStateFromBands();
    _graph?.setBands(_bands);
    syncSel();
    _graph?.setSel(_selId, [..._selIds]);
    _graph?.setRange(_range);
    _graph?.setEqOn(eqActive(_state));
    buildPanel(); buildChips(); updateStatus(); updWelcome(); syncPresetSelect();
    syncOutput();
    if (final) emitState(true); else emitState(false);
}

// Version that doesn't emit (used when receiving eq:set)
function afterEditNoEmit() {
    syncStateFromBands();
    _graph?.setBands(_bands);
    syncSel();
    _graph?.setSel(_selId, [..._selIds]);
    _graph?.setRange(_range);
    _graph?.setEqOn(eqActive(_state));
    buildPanel(); buildChips(); updateStatus(); updWelcome(); syncPresetSelect();
    syncOutput();
}

// ── Range ─────────────────────────────────────────────────────────────────────
function updateRange(r) {
    _range = r;
    $('range')?.querySelectorAll('button').forEach(b => b.classList.toggle('on', +b.dataset.v === r));
    _graph?.setRange(r);
    _graph?.markDirty();
}

// ── Band panel (floats under the selected node) ──────────────────────────────
// Fields take typing ("2k", "A4", "C#2+13", "-3"), the wheel, and a vertical
// drag on their label. With several bands selected the fields edit the primary
// one; bypass and delete act on the whole selection.
let _panelKey = '';
let _finalTimer = 0;
let _lastNudge = 0;

const FIELDS = {
    f: { lo: F_LO, hi: F_HI, parse: parseFreq, fmt: b => fmtF(b.f), step: (v, d, fine) => v * Math.pow(2, d * (fine ? 1 / 96 : 1 / 24)) },
    gain: { lo: -G_MAX, hi: G_MAX, parse: parseNum, fmt: b => fmtDb(b.gain), step: (v, d, fine) => Math.round((v + d * (fine ? 0.1 : 0.5)) * 10) / 10 },
    q: { lo: Q_LO, hi: Q_HI, parse: parseNum, fmt: b => b.q.toFixed(2), step: (v, d, fine) => v * Math.pow(fine ? 1.02 : 1.1, d) },
};

/// After a run of small edits (wheel, keys) the engine gets one final state.
function scheduleFinal() {
    clearTimeout(_finalTimer);
    _finalTimer = setTimeout(() => emitState(true), 400);
}

function buildPanel() {
    const p = $('bandPop');
    if (!p) return;
    const b = _bands.find(x => x.id === _selId);
    if (!b) { p.hidden = true; _panelKey = ''; return; }
    const key = [b.id, b.type, b.on, b.slope, _selIds.size, _bands.indexOf(b), _bands.length].join(':');
    if (key !== _panelKey) { _panelKey = key; renderPanel(p, b); }
    syncPanelValues(b);
    p.hidden = false;
    placePanel();
}

function renderPanel(p, b) {
    const idx = _bands.indexOf(b) + 1, n = _selIds.size;
    const typeOpts = Object.entries(TYPE_LABELS)
        .map(([k, v]) => `<option value="${k}"${k === b.type ? ' selected' : ''}>${v}</option>`).join('');
    const slopeOpts = [6, 12, 18, 24, 36, 48, 72, 96].map(s => `<option value="${s}"${s === b.slope ? ' selected' : ''}>${s} dB/oct</option>`).join('');
    const fld = (k, name, dis) => `<span class="fld${dis ? ' dis' : ''}" data-k="${k}"><span class="fl" title="Drag up or down">${name}</span>` +
        `<input type="text" id="p_${k}" spellcheck="false" autocomplete="off"${dis ? ' disabled' : ''}></span>`;
    p.style.setProperty('--c', b.color);
    p.innerHTML =
        `<button type="button" class="pnav" id="pPrev" title="Previous band (Shift+Tab)" aria-label="Previous band">&lsaquo;</button>` +
        `<span class="pid"><i></i>${idx}${n > 1 ? `<small>+${n - 1}</small>` : ''}</span>` +
        `<button type="button" class="pnav" id="pNext" title="Next band (Tab)" aria-label="Next band">&rsaquo;</button>` +
        `<button type="button" class="sw${b.on ? ' on' : ''}" id="pOn" title="${n > 1 ? 'Bypass the selected bands' : 'Band on (Alt-click the point)'}" aria-label="Band on"></button>` +
        `<select id="pType" aria-label="Type">${typeOpts}</select>` +
        (isCut(b.type) ? `<select id="pSlope" aria-label="Slope">${slopeOpts}</select>` : '') +
        fld('f', 'Freq') + fld('gain', 'Gain', !hasGain(b.type)) + fld('q', 'Q') +
        `<button type="button" class="pdel" id="pDel" title="${n > 1 ? 'Delete the selected bands' : 'Delete band'} (Del)" aria-label="Delete">&times;</button>`;
    const go = d => {
        const i = _bands.indexOf(b), j = (i + d + _bands.length) % _bands.length;
        setSelection([_bands[j].id], _bands[j].id); refreshSel();
    };
    $('pPrev').onclick = () => go(-1);
    $('pNext').onclick = () => go(1);
    $('pOn').onclick = () => toggleBands(_selIds.size > 1 ? selBands() : [b]);
    $('pType').onchange = e => { histPush(); b.type = e.target.value; if (b.type === 'tilt' && !b.gain) b.gain = -3; _graph?.recalcBand(b); markCustom(); afterEdit(true); };
    $('pSlope')?.addEventListener('change', e => { histPush(); b.slope = +e.target.value; _graph?.recalcBand(b); markCustom(); afterEdit(true); });
    $('pDel').onclick = () => deleteBands(_selIds.size > 1 ? null : [b.id]);
    p.querySelectorAll('.fld').forEach(el => wireField(el, b));
}

function setField(b, k, v, final) {
    const F = FIELDS[k];
    b[k] = clamp(v, F.lo, F.hi);
    _graph?.recalcBand(b);
    if (k === 'gain') updateRange(Math.max(_range, [3, 6, 12, 30].find(r => r >= Math.abs(b.gain)) ?? 30));
    markCustom(); afterEdit(final);
}

function wireField(el, b) {
    const k = el.dataset.k, F = FIELDS[k], inp = el.querySelector('input');
    if (inp.disabled) return;
    const commit = () => {
        const v = F.parse(inp.value);
        if (v == null) { inp.value = F.fmt(b); return; }
        histPush(); setField(b, k, v, true);
        inp.value = F.fmt(b);
    };
    inp.addEventListener('focus', () => inp.select());
    inp.addEventListener('change', commit);
    inp.addEventListener('keydown', e => {
        if (e.key === 'Enter') { e.preventDefault(); commit(); inp.blur(); }
        else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); inp.value = F.fmt(b); inp.blur(); }
        else if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
            e.preventDefault();
            if (performance.now() - _lastNudge > 700) histPush();
            _lastNudge = performance.now();
            setField(b, k, F.step(b[k], e.key === 'ArrowUp' ? 1 : -1, e.shiftKey), false);
            inp.value = F.fmt(b); inp.select(); scheduleFinal();
        }
    });
    let lastWheel = 0;
    el.addEventListener('wheel', e => {
        e.preventDefault();
        if (performance.now() - lastWheel > 700) histPush();
        lastWheel = performance.now();
        setField(b, k, F.step(b[k], e.deltaY < 0 ? 1 : -1, e.shiftKey), false);
        inp.value = F.fmt(b); scheduleFinal();
    }, { passive: false });
    // Vertical drag on the label: up raises the value
    const lab = el.querySelector('.fl');
    lab.addEventListener('pointerdown', e => {
        if (e.button !== 0) return;
        e.preventDefault();
        lab.setPointerCapture(e.pointerId);
        let y = e.clientY, acc = 0, started = false;
        const move = ev => {
            acc += (y - ev.clientY) * (ev.shiftKey ? 0.2 : 1); y = ev.clientY;
            const steps = Math.trunc(acc / 3);
            if (!steps) return;
            acc -= steps * 3;
            if (!started) { started = true; histPush(); }
            setField(b, k, F.step(b[k], steps, true), false);
            inp.value = F.fmt(b);
        };
        const up = () => {
            lab.removeEventListener('pointermove', move);
            lab.removeEventListener('pointerup', up);
            lab.removeEventListener('pointercancel', up);
            if (started) emitState(true);
        };
        lab.addEventListener('pointermove', move);
        lab.addEventListener('pointerup', up);
        lab.addEventListener('pointercancel', up);
    });
}

function syncPanelValues(b) {
    for (const k of Object.keys(FIELDS)) {
        const inp = $('p_' + k);
        if (inp && document.activeElement !== inp) inp.value = FIELDS[k].fmt(b);
    }
    const f = $('p_f');
    if (f) { const n = noteOf(b.f); f.title = n.name + (n.cents ? ' ' + (n.cents > 0 ? '+' : '') + n.cents + ' ct' : ''); }
}

/// Keep the panel under the primary node (above it near the floor), inside the graph.
function placePanel() {
    const p = $('bandPop');
    if (!p || p.hidden) return;
    const at = _graph?.nodePos(_selId);
    if (!at) { p.style.visibility = 'hidden'; return; }
    p.style.visibility = '';
    const pw = p.offsetWidth, ph = p.offsetHeight;
    let top = at.y + 22;
    if (top + ph > at.plotBottom) top = at.y - 22 - ph;
    p.style.left = clamp(at.x - pw / 2, 6, Math.max(6, at.w - pw - 6)) + 'px';
    p.style.top = clamp(top, 4, Math.max(4, at.h - ph - 4)) + 'px';
}

function focusPanel(id) {
    buildPanel();
    const map = { pF: 'p_f', pG: 'p_gain', pQ: 'p_q' };
    const el = $(map[id] || id);
    if (el && !el.disabled) { el.focus(); el.select(); }
}

/// Arrow keys on the graph: nudge every selected band (Shift fine), one undo step per burst.
function nudgeSelection(key, fine) {
    const list = selBands();
    if (!list.length) return;
    if (performance.now() - _lastNudge > 700) histPush();
    _lastNudge = performance.now();
    list.forEach(b => { Object.assign(b, nudge(b, key, fine)); _graph?.recalcBand(b); });
    if (key === 'ArrowLeft' || key === 'ArrowRight') _bands.sort((a, z) => a.f - z.f);
    const peak = Math.max(0, ...list.filter(b => hasGain(b.type)).map(b => Math.abs(b.gain)));
    if (peak > _range) updateRange([3, 6, 12, 30].find(r => r >= peak) ?? 30);
    markCustom(); afterEdit(false); scheduleFinal();
}

// ── Chips ─────────────────────────────────────────────────────────────────────
function buildChips() {
    const c = $('chips');
    if (!c) return;
    c.innerHTML = _bands.length ? '' : '<span class="hint">Click the graph to add a band</span>';
    const typeLabels = { bell: 'Bell', lowShelf: 'Low shelf', highShelf: 'High shelf', tilt: 'Tilt', lowCut: 'Low cut', highCut: 'High cut', notch: 'Notch', bandPass: 'Band pass' };
    _bands.forEach((b, i) => {
        const el = document.createElement('button');
        el.type = 'button';
        el.className = 'chip' + (_selIds.has(b.id) ? ' sel' : '') + (b.on ? '' : ' off');
        el.style.setProperty('--c', b.color);
        const hasG = ['bell', 'lowShelf', 'highShelf', 'tilt'].includes(b.type);
        const isCut = b.type === 'lowCut' || b.type === 'highCut';
        el.innerHTML = `<i style="background:${b.color}"></i>${i + 1} ${typeLabels[b.type] || b.type} ${fmtF(b.f)}${hasG ? ' ' + fmtDb(b.gain) : ''}${isCut ? ' ' + b.slope + ' dB/oct' : ''}`;
        el.onclick = e => {
            if (e.ctrlKey || e.metaKey) {
                const ids = new Set(_selIds);
                ids.has(b.id) ? ids.delete(b.id) : ids.add(b.id);
                setSelection([...ids], ids.has(b.id) ? b.id : [...ids].pop());
            } else if (e.shiftKey && _selId != null) setSelection(idRange(_bands.map(x => x.id), _selId, b.id), b.id);
            else setSelection([b.id], b.id);
            refreshSel();
        };
        c.appendChild(el);
    });
}

// ── Toast ─────────────────────────────────────────────────────────────────────
function toast(html, undoable) {
    const t = $('toast');
    if (!t) return;
    t.innerHTML = html + (undoable ? ' · <button type="button" class="linkbtn" id="tUndo">Undo</button>' : '');
    t.hidden = false;
    if (undoable) $('tUndo').onclick = () => { doUndo(); t.hidden = true; };
    clearTimeout(_toastTimer);
    _toastTimer = setTimeout(() => { t.hidden = true; }, 6000);
}

// ── Import drawer ─────────────────────────────────────────────────────────────
function showDrawer(which, tab) {
    $('importDrawer').hidden = which !== 'import';
    $('exportDrawer').hidden = which !== 'export';
    if (which === 'export') { $('exportText').value = getExportText(); updateExportNote(); }
    if (which === 'import') setImportTab(tab || 'file');
}

function setImportTab(t) {
    $('impTabs')?.querySelectorAll('button').forEach(b => b.classList.toggle('on', b.dataset.v === t));
    document.querySelectorAll('.imp-pane').forEach(p => { p.hidden = p.dataset.p !== t; });
    clearPending();
    if (t === 'paste') { $('importText')?.focus(); const v = $('importText')?.value.trim(); if (v) previewText(v, 'Pasted text'); }
    if (t === 'find') { $('findQ')?.focus(); renderFound(); }
}

function clearPending() {
    _pending = null;
    const pp = $('impPrev'); if (pp) pp.hidden = true;
    const ie = $('importErr'); if (ie) ie.textContent = '';
}

function impError(m) {
    if ($('importDrawer')?.hidden) { toast(esc(m)); return; }
    const ie = $('importErr'); if (ie) ie.textContent = m;
    const pp = $('impPrev'); if (pp) pp.hidden = true;
    _pending = null;
}

function previewText(txt, label) {
    const r = parseProfile(txt);
    if (r.kind === 'graphic') {
        impError('GraphicEQ is not supported yet. Download the ParametricEQ file from AutoEq instead.');
        return;
    }
    if (!r.bands.length) { impError(ERR_NONE); return; }
    _pending = r; _pendingLabel = label;
    const ie = $('importErr'); if (ie) ie.textContent = '';
    const pp = $('impPrev'); if (pp) pp.hidden = false;
    const pvTitle = $('pvTitle'); if (pvTitle) pvTitle.innerHTML = 'Detected: <b>' + esc(describe(r)) + '</b>';
    const pvMsg = $('pvMsg');
    const skip = r.skipped.length ? 'Skipped ' + r.skipped.length + ' line' + (r.skipped.length > 1 ? 's' : '') + '.' : '';
    const trunc = r.truncated ? ' Only the first 24 bands are kept.' : '';
    if (pvMsg) pvMsg.textContent = (skip + trunc) || 'Looks good. Apply replaces the current bands, Undo brings them back.';
    $('usePre')?.classList.toggle('on', _usePre && r.preamp != null);
    drawPreview(r);
}

function drawPreview(r) {
    const cv = $('prevCv');
    if (!cv) return;
    import('./math.js').then(({ bandCurve: bc }) => {
        const dpr = window.devicePixelRatio || 1;
        const rect = cv.getBoundingClientRect();
        cv.width = Math.round(rect.width * dpr); cv.height = Math.round(rect.height * dpr);
        const ctx = cv.getContext('2d'), w = rect.width, h = rect.height;
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0); ctx.clearRect(0, 0, w, h);
        const newPts = new Float64Array(NPT);
        r.bands.slice(0, MAX_BANDS).filter(b => b.on).forEach(b => { const c = bc(b, FREQS); for (let i = 0; i < NPT; i++) newPts[i] += c[i]; });
        const curPts = _bands.length ? (() => { const t = new Float64Array(NPT); _bands.forEach(b => { if (b.on) { const c = bc(b, FREQS); for (let i = 0; i < NPT; i++) t[i] += c[i]; } }); return t; })() : null;
        const mx = Math.max(6, ...Array.from(newPts).map(Math.abs), ...(curPts ? Array.from(curPts).map(Math.abs) : []));
        const rr = mx > 12 ? Math.min(30, Math.ceil(mx / 6) * 6) : 12;
        const X = i => (i / (NPT - 1)) * w, Y = d => h / 2 - clamp(d, -rr, rr) / rr * (h / 2 - 6);
        ctx.strokeStyle = 'rgba(255,255,255,.15)'; ctx.lineWidth = 1;
        ctx.beginPath(); ctx.moveTo(0, h / 2 + 0.5); ctx.lineTo(w, h / 2 + 0.5); ctx.stroke();
        if (curPts) { ctx.beginPath(); curPts.forEach((d, i) => i ? ctx.lineTo(X(i), Y(d)) : ctx.moveTo(X(i), Y(d))); ctx.strokeStyle = 'rgba(148,163,184,.5)'; ctx.lineWidth = 1; ctx.stroke(); }
        ctx.beginPath(); newPts.forEach((d, i) => i ? ctx.lineTo(X(i), Y(d)) : ctx.moveTo(X(i), Y(d))); ctx.strokeStyle = '#22d3ee'; ctx.lineWidth = 1.8; ctx.stroke();
        ctx.fillStyle = 'rgba(148,163,184,.75)'; ctx.font = '9px ui-monospace,Consolas,monospace';
        ctx.textAlign = 'left'; ctx.textBaseline = 'top'; ctx.fillText('±' + rr + ' dB', 4, 2);
        if (curPts) { ctx.textAlign = 'right'; ctx.fillText('grey: current', w - 4, 2); }
    });
}

// ── Export drawer note ────────────────────────────────────────────────────────
function updateExportNote() {
    const note = $('exportNote');
    if (!note) return;
    const hasOddSlope = _bands.some(b => (b.type === 'lowCut' || b.type === 'highCut') && [6, 18].includes(b.slope));
    note.hidden = !hasOddSlope;
}

// ── Find headphones (AutoEq) ───────────────────────────────────────────────────
let _autoEqEntries = null;
let _autoEqFetched = null;
let _autoEqStale = false;   // the kept list: the server was not reached
let _autoEqLoading = false;

function getAutoEqApi() {
    if (!_autoEq && T()) _autoEq = autoEqApi(invoke);
    return _autoEq;
}

async function ensureAutoEqList() {
    if (_autoEqEntries || _autoEqLoading) return;
    _autoEqLoading = true;
    const box = $('found');
    if (box) box.innerHTML = '<div class="msg">Loading…</div>';
    const api = getAutoEqApi();
    if (!api) {
        if (box) box.innerHTML = `<div class="msg bad">AutoEq requires the Aura app. <a href="${AUTOEQ_SITE}" rel="noopener">Open autoeq.app</a> to download a profile manually.</div>`;
        _autoEqLoading = false; return;
    }
    try {
        const result = (await api.index()) || {};
        _autoEqEntries = Array.isArray(result.entries) ? result.entries : [];
        _autoEqFetched = result.fetched;
        _autoEqStale = !!result.stale;
        renderFound();
    } catch (e) {
        if (box) box.innerHTML = `<div class="msg bad">${esc(String(e.message || e))} <button type="button" class="linkbtn" id="autoEqSiteBtn">Open autoeq.app</button></div>`;
        const btn = $('autoEqSiteBtn'); if (btn) btn.onclick = () => openExternal(AUTOEQ_SITE);
    }
    _autoEqLoading = false;
}

function renderFound() {
    const box = $('found');
    if (!box) return;
    if (!_autoEqEntries) { ensureAutoEqList(); return; }
    const q = $('findQ')?.value.trim() || '';
    box.innerHTML = '';
    if (_autoEqStale && _autoEqFetched) {
        const ds = new Date(_autoEqFetched * 1000).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
        const warn = document.createElement('div'); warn.className = 'msg warn'; warn.textContent = 'Offline — list from ' + ds;
        box.appendChild(warn);
    }
    if (_autoEqEntries.length === 0) { box.insertAdjacentHTML('beforeend', '<div class="msg">No entries in the index.</div>'); return; }
    if (!q) { box.insertAdjacentHTML('beforeend', '<div class="msg">Type to search for a headphone or IEM model.</div>'); return; }
    const results = searchAutoEq(_autoEqEntries, q);
    if (!results.length) { box.insertAdjacentHTML('beforeend', '<div class="msg">No match.</div>'); return; }
    results.forEach(entry => {
        const btn = document.createElement('button');
        btn.type = 'button';
        const credit = autoEqCredit(entry);
        const othersLabel = entry.others > 0 ? ` <small>+${entry.others} other measurement${entry.others > 1 ? 's' : ''}</small>` : '';
        btn.innerHTML = `<span>${esc(entry.name)}</span><small>${esc(credit)}</small>${othersLabel}`;
        btn.onclick = async () => {
            box.querySelectorAll('button').forEach(x => x.classList.toggle('on', x === btn));
            const api = getAutoEqApi();
            try {
                const txt = await api.fetch(entry);
                if (txt) {
                    previewText(txt, entry.name);
                    const cm = $('prevCredit');
                    if (cm) { cm.innerHTML = esc(credit) + ' · <a href="' + AUTOEQ_SITE + '" class="linkbtn" rel="noopener">autoeq.app</a>'; cm.hidden = false; }
                } else impError('Empty response for ' + entry.name);
            } catch (e) { impError(String(e.message || e)); }
        };
        box.appendChild(btn);
    });
}

// ── Context menu ──────────────────────────────────────────────────────────────
const _menu = (() => {
    const el = document.createElement('div');
    el.className = 'menu'; el.hidden = true; document.body.appendChild(el);
    return el;
})();
function openMenu(x, y, items) {
    _menu.innerHTML = '';
    items.forEach(it => {
        if (it === '-') { const s = document.createElement('div'); s.className = 'sep'; _menu.appendChild(s); return; }
        const b = document.createElement('button'); b.type = 'button';
        b.innerHTML = '<span class="chk">' + (it.chk ? '✓' : '') + '</span>' + it.t;
        b.onclick = () => { hideMenu(); it.fn?.(); };
        _menu.appendChild(b);
    });
    _menu.hidden = false;
    const r = _menu.getBoundingClientRect();
    _menu.style.left = clamp(x, 4, innerWidth - r.width - 4) + 'px';
    _menu.style.top = clamp(y, 4, innerHeight - r.height - 4) + 'px';
    _menu.querySelector('button')?.focus();
}
function hideMenu() { _menu.hidden = true; }
// Capture phase: an outside press closes the old menu before the canvas opens a new one on the same press.
document.addEventListener('pointerdown', e => { if (!_menu.hidden && !_menu.contains(e.target)) hideMenu(); }, true);
// Escape with a menu open closes only the menu, not the window.
document.addEventListener('keydown', e => {
    if (e.key === 'Escape' && !_menu.hidden) { hideMenu(); e.stopPropagation(); }
}, true);
document.addEventListener('scroll', hideMenu, { passive: true });

const TYPE_LABELS = { bell: 'Bell', lowShelf: 'Low shelf', highShelf: 'High shelf', tilt: 'Tilt', lowCut: 'Low cut', highCut: 'High cut', notch: 'Notch', bandPass: 'Band pass' };

function openTypeMenu(e, band, f, gain) {
    if (band) {
        if (!_selIds.has(band.id)) setSelection([band.id], band.id); else _selId = band.id;
        refreshSel();
        const group = _selIds.size > 1 ? selBands() : null;
        openMenu(e.clientX, e.clientY, [
            ...Object.entries(TYPE_LABELS).map(([k, v]) => ({
                t: v, chk: band.type === k,
                fn: () => { histPush(); band.type = k; if (k === 'tilt' && !band.gain) band.gain = -3; _graph?.recalcBand(band); markCustom(); afterEdit(true); },
            })),
            '-',
            { t: band.on ? 'Bypass band' : 'Enable band', fn: () => { histPush(); band.on = !band.on; markCustom(); afterEdit(true); } },
            { t: 'Delete band', fn: () => deleteBand(band.id) },
            ...(group ? ['-',
                { t: 'Bypass ' + group.length + ' bands', fn: () => toggleBands(group) },
                { t: 'Delete ' + group.length + ' bands', fn: () => deleteBands(group.map(x => x.id)) }] : []),
        ]);
    } else {
        openMenu(e.clientX, e.clientY, Object.entries(TYPE_LABELS).map(([k, v]) => ({
            t: 'Add ' + v + ' · ' + fmtF(f), fn: () => addBand(k, f, gain),
        })));
    }
}

// ── Preset name field (save / rename) inline ─────────────────────────────────
// One field in the footer for both actions; window.prompt is not available in the app window.
function showNameField(initial, okText, onOk) {
    if (_saveNameMode) return;
    _saveNameMode = true;
    const footer = $('eqFooter');
    if (!footer) return;
    const wrap = document.createElement('span');
    wrap.id = 'saveWrap';
    wrap.style.cssText = 'display:flex;gap:6px;align-items:center';
    const input = document.createElement('input');
    input.type = 'text';
    input.placeholder = 'Preset name';
    input.value = initial;
    input.style.cssText = 'background:rgba(2,6,14,.85);color:#bae6fd;border:1px solid rgba(56,189,248,.35);border-radius:6px;font:.8rem var(--mono);padding:4px 8px;width:140px;user-select:text';
    const btnOk = document.createElement('button'); btnOk.type = 'button'; btnOk.className = 'act save sm'; btnOk.textContent = okText;
    const btnCancel = document.createElement('button'); btnCancel.type = 'button'; btnCancel.className = 'act ghost sm'; btnCancel.textContent = 'Cancel';
    wrap.append(input, btnOk, btnCancel);
    $('btnSave').before(wrap);
    $('btnSave').hidden = true;
    setTimeout(() => { input.focus(); input.select(); }, 0);
    const finish = () => { _saveNameMode = false; wrap.remove(); $('btnSave').hidden = false; };
    btnCancel.onclick = finish;
    btnOk.onclick = () => {
        const name = input.value.trim();
        if (name && isReservedName(name)) toast('<b>' + esc(name) + '</b> is a built-in preset. Choose another name.');
        if (!name || isReservedName(name) || onOk(name) === false) { input.focus(); input.select(); return; }
        finish();
    };
    input.onkeydown = e => {
        if (e.key === 'Enter') btnOk.click();
        if (e.key === 'Escape') { e.stopPropagation(); finish(); }
    };
}

function showSavePreset() {
    const initial = (!isReservedName(_state.name) && _state.name !== 'Custom') ? _state.name : '';
    showNameField(initial, 'Save', name => {
        syncStateFromBands();
        _presets = savePreset(_presets, name, _state);
        _state = { ..._state, name };
        buildPresetSelect();
        emitEv('eq:presets', { list: _presets });
        toast('Preset <b>' + esc(name) + '</b> saved.');
    });
}

function openPresetMenu() {
    const items = [];
    if (_presets.some(p => p.name === _state.name)) {
        items.push({ t: 'Rename “' + _state.name + '”…', fn: () => startRename(_state.name) });
        items.push({ t: 'Delete “' + _state.name + '”', fn: () => {
            _presets = deletePreset(_presets, _state.name);
            _state = { ..._state, name: 'Custom' };
            buildPresetSelect(); emitEv('eq:presets', { list: _presets });
        }});
        items.push('-');
    }
    items.push({ t: 'Save current as…', fn: showSavePreset });
    const btn = $('presetMenu');
    if (!btn) { showSavePreset(); return; }
    const r = btn.getBoundingClientRect();
    openMenu(r.left, r.bottom + 4, items);
}

function startRename(from) {
    showNameField(from, 'Rename', to => {
        if (to === from) return;
        if (_presets.some(p => p.name.toLowerCase() === to.toLowerCase() && p.name !== from)) {
            toast('A preset named <b>' + esc(to) + '</b> already exists.');
            return false;
        }
        _presets = renamePreset(_presets, from, to);
        if (_state.name === from) _state = { ..._state, name: to };
        buildPresetSelect(); emitEv('eq:presets', { list: _presets });
    });
}

// ── Init ──────────────────────────────────────────────────────────────────────
function init() {
    // Load persisted state
    try {
        const s = JSON.parse(localStorage.getItem('auraSettings') || '{}');
        _state = normalizeState(s.eq);
        _presets = normalizePresets(s.eqPresets);
    } catch (_) { _state = flatState(); _presets = []; }
    _bitPerfect = localStorage.getItem('auraPlayerBitPerfect') === '1';

    // Graph
    const cv = $('eqCanvas');
    if (cv) {
        _graph = createGraph(cv, {
            onSelect: (ids, prim) => { setSelection(ids, prim); refreshSel(); },
            onEditStart: () => histPush(),
            onEdit: (list, final) => { markCustom(); syncStateFromBands(); afterEdit(final); },
            onWheelEdit: (b, isNew) => { if (isNew) histPush(); markCustom(); syncStateFromBands(); afterEdit(false); scheduleFinal(); },
            onAdd: (type, f, gain) => addBand(type, f, gain),
            onToggle: list => toggleBands(list),
            onOpenPanel: b => { setSelection([b.id], b.id); refreshSel(); focusPanel('pF'); },
            onTypeMenu: openTypeMenu,
            onRange: r => updateRange(r),
            onDrag: on => $('bandPop')?.classList.toggle('dragging', on),
            onDrawn: placePanel,
        });
        new ResizeObserver(() => _graph?.resize()).observe(cv.parentElement || cv);
    }

    rebuildBandsFromState();
    _abSlots.A = snapShot();

    buildPresetSelect();
    afterEditNoEmit();
    updHist();
    updateBpBanner();

    setupToolbar();
    setupFooter();
    setupImportDrawer();
    setupExportDrawer();
    setupWelcome();
    setupDragDrop();
    setupKeyboard();
    setupChrome();
    setupTauriEvents();

    setupSpectrum();
    document.addEventListener('visibilitychange', () => _spec?.setVisible(!document.hidden));
}

function setupToolbar() {
    const seg = (id, fn) => $(id)?.addEventListener('click', e => {
        const b = e.target.closest('button'); if (!b) return;
        $(id).querySelectorAll('button').forEach(x => x.classList.toggle('on', x === b));
        fn(b.dataset.v ?? b.dataset.s);
    });
    seg('range', v => { updateRange(+v); afterEdit(false); });
    seg('ab', s => switchAb(s));
    $('undoB').onclick = doUndo;
    $('redoB').onclick = doRedo;
    $('addBand')?.addEventListener('click', () => addBand('bell', 1000, 0));
    $('btnImport')?.addEventListener('click', () => showDrawer($('importDrawer')?.hidden ? 'import' : null));
    $('btnExport')?.addEventListener('click', () => showDrawer($('exportDrawer')?.hidden ? 'export' : null));
}

/// The balance's controls; `push` remembers the state for Undo once per
/// burst of moves.
function setupBalance(push) {
    const bal = () => balanceOf(_state).balance;
    const s = $('balance');
    s?.addEventListener('input', () => { push(); setBalance(balanceFromPos(+s.value), false); });
    s?.addEventListener('change', () => { s.blur(); syncBalance(); emitState(true); });
    const wheel = e => {
        e.preventDefault(); push();
        const step = e.shiftKey ? 0.01 : 0.1;
        setBalance(Math.round((bal() + (e.deltaY < 0 ? 1 : -1) * step) / step) * step, false);
        scheduleFinal();
    };
    s?.addEventListener('wheel', wheel, { passive: false });
    $('balanceV')?.addEventListener('wheel', wheel, { passive: false });
    ['balance', 'balanceV', 'balLbl'].forEach(id => $(id)?.addEventListener('dblclick', () => { histPush(); setBalance(0, true); }));
    const f = $('balanceV');
    if (f) {
        f.addEventListener('focus', () => f.select());
        f.addEventListener('change', () => { const v = parseBalance(f.value); if (v !== null) { histPush(); f.blur(); setBalance(v, true); } syncBalance(); });
        f.addEventListener('keydown', e => {
            if (e.key === 'Enter') { e.preventDefault(); f.blur(); }
            else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); f.blur(); syncBalance(); }
        });
    }
    // Horizontal drag on the Balance label: right leans right (or to the side)
    const lbl = $('balLbl');
    lbl?.addEventListener('pointerdown', e => {
        if (e.button !== 0) return;
        e.preventDefault(); lbl.setPointerCapture(e.pointerId);
        let x = e.clientX, started = false;
        const move = ev => {
            const d = Math.trunc(ev.clientX - x) * (ev.shiftKey ? 0.01 : 0.05);
            if (!d) return;
            x = ev.clientX;
            if (!started) { started = true; histPush(); }
            setBalance(bal() + d, false);
        };
        const up = () => { lbl.removeEventListener('pointermove', move); lbl.removeEventListener('pointerup', up); lbl.removeEventListener('pointercancel', up); if (started) emitState(true); };
        lbl.addEventListener('pointermove', move); lbl.addEventListener('pointerup', up); lbl.addEventListener('pointercancel', up);
    });
    $('balMode')?.addEventListener('click', e => {
        const b = e.target.closest('button'); if (!b || b.dataset.v === balanceOf(_state).balanceMode) return;
        histPush(); setBalanceMode(b.dataset.v);
    });
    syncBalance();
}

/// The delay's controls, as the balance's.
function setupDelay(push) {
    const del = () => delayOf(_state);
    const s = $('delay');
    s?.addEventListener('input', () => { push(); setDelay(delayFromPos(+s.value), false); });
    s?.addEventListener('change', () => { s.blur(); syncDelay(); emitState(true); });
    const wheel = e => {
        e.preventDefault(); push();
        const step = e.shiftKey ? 0.01 : 0.1;
        setDelay(Math.round((del() + (e.deltaY < 0 ? 1 : -1) * step) / step) * step, false);
        scheduleFinal();
    };
    s?.addEventListener('wheel', wheel, { passive: false });
    $('delayV')?.addEventListener('wheel', wheel, { passive: false });
    ['delay', 'delayV', 'delLbl'].forEach(id => $(id)?.addEventListener('dblclick', () => { histPush(); setDelay(0, true); }));
    const f = $('delayV');
    if (f) {
        f.addEventListener('focus', () => f.select());
        f.addEventListener('change', () => { const v = parseDelay(f.value); if (v !== null) { histPush(); f.blur(); setDelay(v, true); } syncDelay(); });
        f.addEventListener('keydown', e => {
            if (e.key === 'Enter') { e.preventDefault(); f.blur(); }
            else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); f.blur(); syncDelay(); }
        });
    }
    // Horizontal drag on the Delay label: right makes the right channel later
    const lbl = $('delLbl');
    lbl?.addEventListener('pointerdown', e => {
        if (e.button !== 0) return;
        e.preventDefault(); lbl.setPointerCapture(e.pointerId);
        let x = e.clientX, started = false;
        const move = ev => {
            const d = Math.trunc(ev.clientX - x) * (ev.shiftKey ? 0.01 : 0.05);
            if (!d) return;
            x = ev.clientX;
            if (!started) { started = true; histPush(); }
            setDelay(del() + d, false);
        };
        const up = () => { lbl.removeEventListener('pointermove', move); lbl.removeEventListener('pointerup', up); lbl.removeEventListener('pointercancel', up); if (started) emitState(true); };
        lbl.addEventListener('pointermove', move); lbl.addEventListener('pointerup', up); lbl.addEventListener('pointercancel', up);
    });
    const keep = () => { histPush(); setSpeakersAlways(_state.speakersAlways !== true); };
    $('spkAlways')?.addEventListener('click', keep);
    $('spkAlwaysL')?.addEventListener('click', keep);
    syncDelay();
}

// ── Distances: the delay (and a balance hint) from the listener's distances to the speakers ──
let _spkUnit = 'cm';
function setupSpeakerCalc() {
    const pop = $('spkCalc'), btn = $('btnSpk');
    if (!pop || !btn) return;
    try {
        const saved = JSON.parse(localStorage.getItem('auraEqSpeakerCalc') || '{}');
        if (UNIT_M[saved.unit]) _spkUnit = saved.unit;
        if (saved.l) $('spkL').value = saved.l;
        if (saved.r) $('spkR').value = saved.r;
        if (isFinite(saved.t)) $('spkT').value = saved.t;
        $('spkBal').checked = saved.bal === true;
    } catch (_) {}
    const calc = () => speakerCalc($('spkL').value, $('spkR').value, _spkUnit, $('spkT').value === '' ? 20 : +$('spkT').value);
    const render = () => {
        $('spkUnit').querySelectorAll('button').forEach(b => b.classList.toggle('on', b.dataset.v === _spkUnit));
        const r = calc(), out = $('spkRes');
        $('spkApply').disabled = !r;
        try {
            localStorage.setItem('auraEqSpeakerCalc', JSON.stringify({ unit: _spkUnit, l: $('spkL').value, r: $('spkR').value, t: +$('spkT').value, bal: $('spkBal').checked }));
        } catch (_) {}
        if (!r) { out.textContent = 'Measure from your listening place to each speaker\'s tweeter.'; return; }
        const c = soundSpeed($('spkT').value === '' ? 20 : +$('spkT').value);
        let html;
        if (!r.delay) html = 'Same distance: <b>no delay</b>.';
        else html = 'Delay ' + (r.delay > 0 ? 'right' : 'left') + ' (the nearer) by <b>' + Math.abs(r.delay).toFixed(2) + ' ms</b>'
            + (r.clamped ? ' <span class="warn">(the most: 30 ms)</span>' : '') + '.';
        html += '<br>Balance hint: <b>' + fmtBalance(r.balance, 'lr') + '</b>' + ($('spkBal').checked ? '' : ' (not applied)');
        html += '<br><span title="Sound at ' + $('spkT').value + ' °C">' + c.toFixed(1) + ' m/s</span>';
        out.innerHTML = html;
    };
    const close = () => { pop.hidden = true; document.removeEventListener('pointerdown', outside, true); document.removeEventListener('keydown', esc, true); };
    const outside = e => { if (!pop.contains(e.target) && e.target !== btn) close(); };
    const esc = e => { if (e.key === 'Escape') { e.stopPropagation(); close(); } };
    btn.addEventListener('click', () => {
        if (!pop.hidden) { close(); return; }
        pop.hidden = false;
        const b = btn.getBoundingClientRect(), w = pop.offsetWidth, h = pop.offsetHeight;
        pop.style.left = Math.max(8, Math.min(window.innerWidth - w - 8, b.left)) + 'px';
        pop.style.top = Math.max(8, b.top - h - 6) + 'px';
        render();
        document.addEventListener('pointerdown', outside, true);
        document.addEventListener('keydown', esc, true);
        $('spkL').focus();
    });
    ['spkL', 'spkR', 'spkT', 'spkBal'].forEach(id => $(id).addEventListener('input', render));
    $('spkUnit').addEventListener('click', e => {
        const b = e.target.closest('button'); if (!b || b.dataset.v === _spkUnit) return;
        // The distances keep their length in the new unit.
        const k = UNIT_M[_spkUnit] / UNIT_M[b.dataset.v];
        for (const id of ['spkL', 'spkR']) {
            const v = parseFloat($(id).value);
            if (isFinite(v)) $(id).value = String(Math.round(v * k * 100) / 100);
        }
        _spkUnit = b.dataset.v; render();
    });
    $('spkApply').addEventListener('click', () => {
        const r = calc(); if (!r) return;
        histPush();
        const { delay: _d, ...rest } = _state;
        _state = { ...rest, ...(r.delay ? { delay: r.delay } : {}) };
        if ($('spkBal').checked) {
            const { balance: _b, balanceMode: _m, ...rest2 } = _state;
            _state = { ...rest2, ...(r.balance ? { balance: r.balance } : {}) };
        }
        syncBalance(); syncDelay(); emitState(true); updateStatus();
        toast('Speakers set: ' + (r.delay ? 'delay ' + fmtDelay(r.delay) : 'no delay')
            + ($('spkBal').checked ? ', balance ' + fmtBalance(r.balance, 'lr') : '') + (_state.on || _state.speakersAlways ? '.' : '. They play when the EQ is on.'));
        close();
    });
    // Clear here and the Speakers box's Reset: balance to the centre, no delay (the switch stays).
    const clearSpeakers = () => {
        histPush();
        const { delay: _d, balance: _b, balanceMode: _m, ...rest } = _state;
        _state = rest;
        syncBalance(); syncDelay(); emitState(true); updateStatus();
        render();
    };
    $('spkClear').addEventListener('click', clearSpeakers);
    $('btnSpkReset')?.addEventListener('click', clearSpeakers);
}

function setupFooter() {
    $('proOn')?.addEventListener('click', () => { histPush(); _state = { ..._state, on: !_state.on }; afterEdit(true); });
    let outHist = 0;
    const outPush = () => { if (performance.now() - outHist > 700) histPush(); outHist = performance.now(); };
    $('outGain')?.addEventListener('input', e => { outPush(); setOutput(+e.target.value, false); });
    $('outGain')?.addEventListener('change', () => emitState(true));
    const outWheel = e => { e.preventDefault(); outPush(); setOutput(outDb() + (e.deltaY < 0 ? 1 : -1) * (e.shiftKey ? 0.1 : 0.5), false); scheduleFinal(); };
    $('outGain')?.addEventListener('wheel', outWheel, { passive: false });
    $('outGainV')?.addEventListener('wheel', outWheel, { passive: false });
    ['outGain', 'outGainV', 'outLbl'].forEach(id => $(id)?.addEventListener('dblclick', () => { histPush(); setOutput(0, true); }));
    const ov = $('outGainV');
    if (ov) {
        ov.addEventListener('focus', () => ov.select());
        ov.addEventListener('change', () => { const v = parseNum(ov.value); if (v !== null) { histPush(); setOutput(v, true); } syncOutput(); });
        ov.addEventListener('keydown', e => {
            if (e.key === 'Enter') { e.preventDefault(); ov.blur(); }
            else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); syncOutput(); ov.blur(); }
        });
    }
    // Vertical drag on the Output label: up raises the level
    const ol = $('outLbl');
    ol?.addEventListener('pointerdown', e => {
        if (e.button !== 0) return;
        e.preventDefault(); ol.setPointerCapture(e.pointerId);
        let y = e.clientY, started = false;
        const move = ev => {
            const d = (y - ev.clientY) * (ev.shiftKey ? 0.02 : 0.1);
            if (Math.abs(d) < 0.1) return;
            y = ev.clientY;
            if (!started) { started = true; histPush(); }
            setOutput(outDb() + d, false);
        };
        const up = () => { ol.removeEventListener('pointermove', move); ol.removeEventListener('pointerup', up); ol.removeEventListener('pointercancel', up); if (started) emitState(true); };
        ol.addEventListener('pointermove', move); ol.addEventListener('pointerup', up); ol.addEventListener('pointercancel', up);
    });
    setupBalance(outPush);
    setupDelay(outPush);
    setupSpeakerCalc();
    $('gainMode')?.addEventListener('click', e => {
        const b = e.target.closest('button'); if (!b || b.dataset.v === gainMode()) return;
        histPush(); _state = { ..._state, gain: b.dataset.v }; _engineGain = null;
        markCustom(); afterEdit(true);
    });
    $('clipL')?.addEventListener('click', resetClip);
    pollClip();
    document.addEventListener('visibilitychange', () => { if (!document.hidden) pollClip(); });
    $('btnReset')?.addEventListener('click', () => loadFlatState());
    $('btnSave')?.addEventListener('click', showSavePreset);
    $('presetMenu')?.addEventListener('click', openPresetMenu);
    $('presetSel')?.addEventListener('change', e => {
        const v = e.target.value; if (v === 'custom') return;
        if (v === FLAT_NAME) loadFlatState(); else applyPreset(v);
    });
}

function setupImportDrawer() {
    $('impTabs')?.addEventListener('click', e => { const b = e.target.closest('button'); if (b) setImportTab(b.dataset.v); });
    $('importCancel')?.addEventListener('click', () => showDrawer(null));
    $('dropBox')?.addEventListener('click', () => $('importFile')?.click());
    $('importFile')?.addEventListener('change', e => {
        const f = e.target.files[0]; e.target.value = '';
        if (!f) return;
        readFile(f, txt => $('importDrawer')?.hidden ? quickImport(txt, f.name) : previewText(txt, f.name));
    });
    $('dropBox')?.addEventListener('dragover', e => { e.preventDefault(); $('dropBox').classList.add('over'); });
    $('dropBox')?.addEventListener('dragleave', () => $('dropBox').classList.remove('over'));
    $('dropBox')?.addEventListener('drop', e => { e.preventDefault(); e.stopPropagation(); $('dropBox').classList.remove('over'); const f = e.dataTransfer.files[0]; if (f) readFile(f, txt => previewText(txt, f.name)); });
    $('importText')?.addEventListener('input', e => { clearTimeout(_pasteTimer); const v = e.target.value; _pasteTimer = setTimeout(() => { v.trim() ? previewText(v, 'Pasted text') : clearPending(); }, 250); });
    $('impMode')?.addEventListener('click', e => { const b = e.target.closest('button'); if (b) { $('impMode').querySelectorAll('button').forEach(x => x.classList.toggle('on', x === b)); _impMode = b.dataset.v; } });
    $('usePre')?.addEventListener('click', e => { _usePre = !_usePre; e.currentTarget.classList.toggle('on', _usePre); });
    $('importGo')?.addEventListener('click', () => { if (!_pending) return; applyImport(_pending, _impMode, _usePre, _pendingLabel); showDrawer(null); });
    $('findQ')?.addEventListener('input', renderFound);
}

function setupExportDrawer() {
    $('exportClose')?.addEventListener('click', () => showDrawer(null));
    $('copyExport')?.addEventListener('click', e => {
        const ta = $('exportText'), btn = e.currentTarget;
        const done = ok => { btn.textContent = ok ? 'Copied' : 'Select and copy'; setTimeout(() => btn.textContent = 'Copy', 1400); };
        if (navigator.clipboard) navigator.clipboard.writeText(ta.value).then(() => done(true), () => { ta.select(); done(false); });
        else { ta.select(); done(false); }
    });
    $('saveExport')?.addEventListener('click', saveExportFile);
}

function setupWelcome() {
    $('wFile')?.addEventListener('click', () => { showDrawer('import', 'file'); $('importFile')?.click(); });
    $('wPaste')?.addEventListener('click', () => showDrawer('import', 'paste'));
    $('wFind')?.addEventListener('click', () => showDrawer('import', 'find'));
    $('wScratch')?.addEventListener('click', () => { _state = { ..._state, name: 'Custom' }; updWelcome(); toast('Click the graph to add a band'); });
}

function readFile(f, cb) {
    if (f.size > 2e6) { impError(f.name + ' is ' + (f.size / 1e6).toFixed(1) + ' MB, too large.'); return; }
    const r = new FileReader();
    r.onload = () => cb(String(r.result));
    r.onerror = () => impError('Could not read ' + f.name + '.');
    r.readAsText(f);
}

function setupDragDrop() {
    const win = document.body;
    const hasFiles = e => Array.from(e.dataTransfer?.types || []).includes('Files');
    win.addEventListener('dragenter', e => { if (!hasFiles(e)) return; e.preventDefault(); _dragDepth++; $('dropzone').hidden = false; });
    win.addEventListener('dragover', e => { if (hasFiles(e)) e.preventDefault(); });
    win.addEventListener('dragleave', () => { if (--_dragDepth <= 0) { _dragDepth = 0; $('dropzone').hidden = true; } });
    win.addEventListener('drop', e => { if (!hasFiles(e)) return; e.preventDefault(); _dragDepth = 0; $('dropzone').hidden = true; const f = e.dataTransfer.files[0]; if (f) readFile(f, txt => quickImport(txt, f.name)); });
}

function setupKeyboard() {
    const editing = t => t?.closest?.('input,textarea,select');
    document.addEventListener('paste', e => {
        if (editing(e.target)) return;
        const txt = e.clipboardData?.getData('text') || ''; if (!txt.trim()) return;
        e.preventDefault(); quickImport(txt, 'Clipboard');
    });
    document.addEventListener('keydown', e => {
        if (editing(e.target)) return;
        const mod = e.ctrlKey || e.metaKey;
        if (mod && e.key.toLowerCase() === 'z') { e.preventDefault(); e.shiftKey ? doRedo() : doUndo(); return; }
        if (mod && e.key.toLowerCase() === 'y') { e.preventDefault(); doRedo(); return; }
        if ((e.key === 'Delete' || e.key === 'Backspace') && _selIds.size) { e.preventDefault(); deleteBands(); return; }
        if (mod && e.key.toLowerCase() === 'a') { e.preventDefault(); setSelection(_bands.map(b => b.id), _selId ?? _bands[0]?.id); refreshSel(); return; }
        const free = e.target === document.body || e.target === $('eqCanvas') || e.target === document.documentElement;
        if (e.key === 'Tab' && free && _bands.length) {
            e.preventDefault();
            const i = _bands.findIndex(b => b.id === _selId);
            const n = _bands.length, j = i < 0 ? (e.shiftKey ? n - 1 : 0) : (i + (e.shiftKey ? n - 1 : 1)) % n;
            setSelection([_bands[j].id], _bands[j].id); refreshSel();
            return;
        }
        if (e.key.startsWith('Arrow') && !mod && _selIds.size && !e.target.closest?.('button')) {
            e.preventDefault();
            nudgeSelection(e.key, e.shiftKey);
        }
    });
}

function setupChrome() {
    $('winHead')?.addEventListener('mousedown', e => {
        if (e.button !== 0 || e.target.closest('button,select,input,label')) return;
        try { T()?.window.getCurrentWindow().startDragging(); } catch (_) {}
    });
    $('winMin')?.addEventListener('click', () => { try { T()?.window.getCurrentWindow().minimize(); } catch (_) {} });
    $('winClose')?.addEventListener('click', () => { try { T()?.window.getCurrentWindow().close(); } catch (_) { window.close(); } });
    document.addEventListener('keydown', e => {
        if (e.key === 'Escape' && (!$('importDrawer')?.hidden || !$('exportDrawer')?.hidden)) { showDrawer(null); return; }
        if (e.key === 'Escape') { try { T()?.window.getCurrentWindow().close(); } catch (_) {} }
    });
}

function updateBpBanner() { const b = $('bpBanner'); if (b) b.hidden = !_bitPerfect; }

async function setupTauriEvents() {
    // eq:set — state from outside; do NOT echo back
    await listenEv('eq:set', ({ state }) => {
        histPush();
        _state = normalizeState(state);
        rebuildBandsFromState();
        afterEditNoEmit();
    });
    await listenEv('eq:presets-set', ({ list }) => { _presets = normalizePresets(list); buildPresetSelect(); });
    await listenEv('eq:bitperfect', ({ on }) => { _bitPerfect = !!on; updateBpBanner(); updateStatus(); });
    await listenEv('eq:show', ({ tab }) => { window.focus(); if (tab === 'import') showDrawer('import'); });
    await listenEv('eq:engine', ({ active, rate, clipRiskDb, gainDb }) => {
        _engineGain = typeof gainDb === 'number' ? gainDb : null;
        _engineClipRisk = (typeof clipRiskDb === 'number' && clipRiskDb > 0) ? clipRiskDb : null;
        if (typeof rate === 'number' && rate > 0 && rate !== _drawFs) {
            _drawFs = rate;
            _graph?.setDrawFs(rate);
        }
        updateStatus();
    });
}

init();
