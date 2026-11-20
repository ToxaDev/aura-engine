// ══════════════════════════════════════════════════════════════════════
// eq-bridge.js — live link between the EQ state and the rest of the app.
//
// Pattern mirrors xtc.js: the main window is the only author of settings.
// The EQ window emits eq:state / eq:presets; this module listens, stores,
// propagates to the engine live (player_set_eq, ≤ 30/s, final always sent)
// and schedules a settings save.
//
// Rack toggle: dsprack.js apply() sets the convEq checkbox and dispatches
// `change`. player.js's panel capture listener skips convEq (see there),
// so only this module's listener drives the engine for that toggle.
// ══════════════════════════════════════════════════════════════════════

import { flatState, normalizeState, normalizePresets, eqSig } from './eq/state.js';

// ── in-memory state ──────────────────────────────────────────────────
let eqState = flatState();
let eqPresets = [];

export function getEqState()  { return eqState; }
export function setEqState(s) { eqState = normalizeState(s); }

// ── the rack signature's EQ part ─────────────────────────────────────
let lastSig;

/// After a settled EQ change (final, a toggle, a preset): when the EQ part of
/// rackSig moved, tell the list (rows, rendered map) — never per drag tick.
export function noteEqSig() {
    const sig = eqSig({ ...eqState, speakersAlways: false });
    if (sig === lastSig) return;
    lastSig = sig;
    document.dispatchEvent(new CustomEvent('aura:eq-sig'));
}

export function getEqPresets()      { return eqPresets; }
export function setEqPresets(list)  { eqPresets = normalizePresets(list); }

// ── send to engine (throttled ≤ 30/s, final value always sent) ───────
const THROTTLE_MS = Math.ceil(1000 / 30);
let throttleTimer = null;
let queuedEq = null;

function scheduleEqSend(eq, final) {
    if (final) {
        if (throttleTimer) { clearTimeout(throttleTimer); throttleTimer = null; }
        doSend(eq, true);
    } else {
        queuedEq = eq;
        if (!throttleTimer) {
            throttleTimer = setTimeout(() => {
                throttleTimer = null;
                doSend(queuedEq, false);
            }, THROTTLE_MS);
        }
    }
}

function doSend(eq, final) {
    const t = window.__TAURI__;
    if (!t) return;
    t.core.invoke('player_set_eq', { eq }).then(result => {
        // Forward the engine's reply to the EQ window (gain level, clip risk…).
        if (final && result && t.event) {
            const { active, rate, clipRiskDb, gainDb, eqOverDb } = result;
            t.event.emit('eq:engine', { active, rate, clipRiskDb, gainDb, eqOverDb }).catch(() => {});
        }
    }).catch(() => {});    // command may not exist yet (EQ1 still landing)
}

// ── save settings (debounced 300 ms, immediate on final) ─────────────
let saveTimer = null;
function scheduleSave(immediate) {
    if (immediate) {
        if (saveTimer) { clearTimeout(saveTimer); saveTimer = null; }
        import('./settings.js').then(m => m.saveSettings());
    } else if (!saveTimer) {
        saveTimer = setTimeout(() => {
            saveTimer = null;
            import('./settings.js').then(m => m.saveSettings());
        }, 300);
    }
}

// ── init ──────────────────────────────────────────────────────────────
/// Called once from initDspRack (dsprack.js). `repaint` is the rack's
/// paint function.
export function initEqBridge(repaint) {
    const t = window.__TAURI__;
    if (!t?.event?.listen) return;
    const ev = t.event;

    // EQ window streaming its edits (≤ 30/s while dragging, final on release).
    ev.listen('eq:state', (msg) => {
        const { state, final } = msg?.payload || {};
        if (!state) return;
        setEqState(state);
        // Sync the checkbox without dispatching change (would loop back here).
        const cb = document.getElementById('convEq');
        if (cb && cb.checked !== eqState.on) cb.checked = eqState.on;
        repaint?.();
        scheduleSave(!!final);
        scheduleEqSend(eqState, !!final);
        if (final) noteEqSig();
    }).catch(() => {});

    // Preset list changed in the window.
    ev.listen('eq:presets', (msg) => {
        const list = msg?.payload?.list;
        if (!Array.isArray(list)) return;
        setEqPresets(list);
        scheduleSave(true);
    }).catch(() => {});

    // convEq checkbox toggled by the rack (dsprack.js → apply() dispatches
    // `change`). player.js returns early for this element so only this
    // handler drives the engine for the toggle.
    document.getElementById('convEq')?.addEventListener('change', (e) => {
        eqState = { ...eqState, on: !!e.target.checked };
        // repaint + saveSettings are already called by dsprack.js apply();
        // only send live and notify the window.
        scheduleEqSend(eqState, true);
        noteEqSig();
        ev.emit('eq:set', { state: eqState }).catch(() => {});
    });

    lastSig = eqSig({ ...eqState, speakersAlways: false });
    // Send the restored state to the engine right after init so it starts
    // live even before the first user edit.
    scheduleEqSend(eqState, true);
}

/// Emit eq:bitperfect to the EQ window when the player's bit-perfect mode
/// changes (called from player.js).
export function notifyEqBitPerfect(on) {
    window.__TAURI__?.event?.emit('eq:bitperfect', { on }).catch(() => {});
}
