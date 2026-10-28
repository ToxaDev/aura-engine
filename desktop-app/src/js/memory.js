
// ══════════════════════════════════════════════════════════════════════
// Track memory — per-title DSP snapshots.
//
// A track with M active remembers the rack's Advanced DSP stages,
// Headroom, Apodizing and XTC (incl. geometry). The memory key is the
// filename without extension, case-insensitive (M1 rule). Two files
// with the same title share one record.
//
// Persisted to localStorage under 'auraTrackMemory'. The base rack
// (the settings in effect before the first M track took over) lives
// under 'auraPlayerBase' and is held only until a non-M track plays.
// ══════════════════════════════════════════════════════════════════════

import { setVal, setCheck } from './settings.js';
import { getXtcGeometry, setXtcGeometry } from './xtc.js';
import { saveSettings } from './settings.js';

const KEY_MEM  = 'auraTrackMemory';
const KEY_BASE = 'auraPlayerBase';

let _mem = {};         // { [key]: { on: bool, settings: snapshot } }
let _saveTimer = null;

/// Key for a track: filename without extension, lowercase.
export function mKey(pathOrName) {
    const base = String(pathOrName).split(/[/\\]/).pop();
    return base.replace(/\.[^.]+$/, '').toLowerCase();
}

export function loadMemory() {
    try {
        const s = localStorage.getItem(KEY_MEM);
        _mem = s ? JSON.parse(s) : {};
    } catch (_) { _mem = {}; }
}

function saveMemory() {
    if (_saveTimer) clearTimeout(_saveTimer);
    _saveTimer = setTimeout(() => {
        try { localStorage.setItem(KEY_MEM, JSON.stringify(_mem)); } catch (_) {}
    }, 300);
}

/// Return the record for this key (or null).
export function getM(key) { return _mem[key] || null; }

/// Write a record and schedule a debounced save.
export function setM(key, data) { _mem[key] = data; saveMemory(); }

/// Remove a record.
export function clearM(key) { delete _mem[key]; saveMemory(); }

/// Whether M is active for this entry.
export function isMActive(entry) {
    return !!_mem[mKey(entry.name)]?.on;
}

/// Return the saved settings snapshot for this entry, or null.
export function getMSettings(entry) {
    return _mem[mKey(entry.name)]?.settings || null;
}

/// Toggle M on/off for an entry. Returns the new on-state.
export function toggleMEntry(entry) {
    const k = mKey(entry.name);
    const rec = _mem[k] || { on: false, settings: null };
    rec.on = !rec.on;
    _mem[k] = rec;
    saveMemory();
    return rec.on;
}

// ── snapshot read / apply ─────────────────────────────────────────────

/// Read M-relevant fields from the rack DOM. Does NOT touch FS/taps/GPU.
export function readMSnapshot() {
    const get = id => document.getElementById(id);
    return {
        convApodizing:        get('convApodizing')?.value        || '0',
        convHeadroom:         get('convHeadroom')?.value         || '0',
        convAdaptiveApodizer: !!get('convAdaptiveApodizer')?.checked,
        convHybridPhase:      !!get('convHybridPhase')?.checked,
        convSubsonic:         !!get('convSubsonic')?.checked,
        convSubsonicHz:       get('convSubsonicHz')?.value       || '15',
        labFeatures: {
            declip:           !!get('convLabDeclip')?.checked,
            isp:              !!get('convLabIsp')?.checked,
            tfsPhase:         !!get('convLabTfs')?.checked,
            continuousAlpha:  !!get('convLabAlpha')?.checked,
            adaptiveHeadroom: !!get('convLabHeadroom')?.checked,
            xtc:              !!get('convLabXtc')?.checked,
        },
        xtcGeometry: getXtcGeometry(),
    };
}

/// Apply a snapshot to the rack DOM without firing change events.
/// Calls __dspSyncRack to reconcile badges, then saveSettings to persist.
export function applyMSnapshot(snap) {
    if (!snap) return;
    setVal('convApodizing',    snap.convApodizing);
    setVal('convHeadroom',     snap.convHeadroom);
    setCheck('convAdaptiveApodizer', snap.convAdaptiveApodizer);
    setCheck('convHybridPhase',      snap.convHybridPhase);
    setCheck('convSubsonic',         snap.convSubsonic);
    setVal('convSubsonicHz',   snap.convSubsonicHz);
    if (snap.labFeatures) {
        setCheck('convLabDeclip',    snap.labFeatures.declip           || false);
        setCheck('convLabIsp',       snap.labFeatures.isp              || false);
        setCheck('convLabTfs',       snap.labFeatures.tfsPhase         || false);
        setCheck('convLabAlpha',     snap.labFeatures.continuousAlpha  || false);
        setCheck('convLabHeadroom',  snap.labFeatures.adaptiveHeadroom || false);
        setCheck('convLabXtc',       snap.labFeatures.xtc              || false);
    }
    if (snap.xtcGeometry) setXtcGeometry(snap.xtcGeometry);
    window.__dspSyncRack?.();
    saveSettings();
}

/// Convert an M snapshot to the mOverride shape expected by startConversion.
/// Non-M fields (FS, taps, GPU, PFR) must come from the rack DOM.
export function snapToConvertOverride(snap) {
    if (!snap) return null;
    const sub = snap.convSubsonic
        ? (parseFloat(snap.convSubsonicHz) || 15)
        : 0;
    return {
        apodizing:        parseInt(snap.convApodizing || '0'),
        headroomDb:       parseFloat(snap.convHeadroom || '0'),
        adaptiveApodizer: snap.convAdaptiveApodizer,
        hybridPhase:      snap.convHybridPhase,
        subsonicHz:       sub,
        labFeatures: {
            declip:           snap.labFeatures?.declip           || false,
            isp:              snap.labFeatures?.isp              || false,
            // HP and TFS are mutually exclusive; HP wins (same precedence as the
            // rack reconciler and collectPlayerSettings).
            tfsPhase:         snap.convHybridPhase
                                  ? false
                                  : (snap.labFeatures?.tfsPhase || false),
            continuousAlpha:  snap.labFeatures?.continuousAlpha  || false,
            adaptiveHeadroom: snap.labFeatures?.adaptiveHeadroom || false,
            xtc:              snap.labFeatures?.xtc              || false,
            xtcGeometry:      snap.xtcGeometry || null,
        },
    };
}

// ── base rack ─────────────────────────────────────────────────────────

let _baseRack = null;

export function loadBaseRack() {
    try {
        const s = localStorage.getItem(KEY_BASE);
        _baseRack = s ? JSON.parse(s) : null;
    } catch (_) { _baseRack = null; }
}

export function getBaseRack() { return _baseRack; }

/// Save the current rack as base only if no base is held yet.
export function saveBaseRackIfAbsent() {
    if (_baseRack) return;
    _baseRack = readMSnapshot();
    try { localStorage.setItem(KEY_BASE, JSON.stringify(_baseRack)); } catch (_) {}
}

/// Restore the base rack to the DOM (applyMSnapshot) and clear it.
export function restoreAndClearBaseRack() {
    if (!_baseRack) return;
    const snap = _baseRack;
    _baseRack = null;
    try { localStorage.removeItem(KEY_BASE); } catch (_) {}
    applyMSnapshot(snap);
}
