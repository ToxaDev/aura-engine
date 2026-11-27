// ══════════════════════════════════════════════════════════════════════
// A version on the air shows how it was made (Anton 22.11). A converted
// file is the sound as the rack made it then: its FS, its filter and
// size, the stages that ran, its output. While one plays from disk as the
// version picked on the Conversions page, Sound says those — the strip's
// cells and the stages' rows as they were — under a line that says so,
// and the rack can not be changed meanwhile (the file is what it is). The
// listener's own rack is not touched: it is back the moment the version
// stops or another track plays.
//
// What the version was made with comes from its name (the scan:
// FS or DSD rate, filter, size, stages, modulator) and, where this app
// recorded it, the rack's own signature of the moment (apodizing,
// headroom, the subsonic corner).
// ══════════════════════════════════════════════════════════════════════

// UI texts.
export const VERSION_TEXTS = {
    head: 'Version on the air',
    note: 'Sound shows how this file was converted. Your rack comes back when it stops.',
    tip: 'A converted file plays as it was made: its settings are shown here and cannot be changed while it plays. Stop it, or play the original, to use your rack.',
};

/// A stage's token in a version's name → the rack's row(s) it lights.
export const TOKEN_ROWS = {
    DC: ['convLabDeclip'],
    ISP: ['convLabIsp'],
    SUB10: ['convSubsonic'], SUB15: ['convSubsonic'], SUB20: ['convSubsonic'],
    AHR: ['convLabHeadroom'],
    AA: ['convAdaptiveApodizer'],
    PFR: ['convFirResampling'],
    TFS: ['convLabTfs'],
    HP: ['convHybridPhase'],
    aHP: ['convHybridPhase', 'convLabAlpha'],
    CUE: ['convLabCue'],
    XTC: ['convLabXtc'],
    EQ: ['convEq'],
};

const FAMILY = [44100, 48000];

/// How many times its family's base a PCM rate is (352800 → 8), 0 for none.
export function fsOfRate(hz) {
    const n = Number(hz) || 0;
    for (const b of FAMILY) if (n >= b && n % b === 0) return n / b;
    return 0;
}

/// The Headroom select's value for a dB figure: '0', '-0.5', '-1.0', '-3.0'.
export function headroomKey(db) {
    const n = Number(db);
    if (!Number.isFinite(n) || n === 0) return '0';
    return n.toFixed(1);
}

/// What the rack shows for the version `v` (a file of the scan: kind,
/// rate, dsd, srcRate, modulator, filter, taps, stages) recorded with the
/// signature `sig` (the rack's JSON at the time, or null). { fs, taps
/// (the size as the name spells it), filter, on: Set of rows, subHz, aa,
/// apod, hr, out: { multiple, label } | null, path }.
export function viewOf(v, sig = null) {
    let s = null;
    if (sig) { try { s = typeof sig === 'string' ? JSON.parse(sig) : sig; } catch (_) { s = null; } }
    const stages = Array.isArray(v?.stages) ? v.stages : [];
    const on = new Set();
    let subHz = null;
    for (const t of stages) {
        for (const id of TOKEN_ROWS[t] || []) on.add(id);
        const m = /^SUB(\d+)$/.exec(t);
        if (m) subHz = Number(m[1]);
    }
    const dsd = v?.kind === 'dsd' ? Number(v.dsd) || 0 : 0;
    // A DSF's chain ran at its carrier, a sixteenth of the DSD rate (×16 at
    // most, ideal-taps.js chainFs); the rack's own FS when it was recorded.
    const fs = dsd ? (Number(s?.fs) || Math.max(1, Math.min(Math.floor(dsd / 16), 16))) : (fsOfRate(v?.rate) || Number(s?.fs) || 0);
    const aa = on.has('convAdaptiveApodizer');
    return {
        path: v?.path || null,
        fs,
        taps: v?.taps || null,
        filter: v?.filter || null,
        on,
        subHz,
        aa,
        apod: s && s.apodizing != null ? String(s.apodizing) : '0',
        hr: s && s.headroomDb != null ? headroomKey(s.headroomDb) : '0',
        out: dsd ? { multiple: dsd, label: v?.modulator || '' } : null,
    };
}

/// The strip's taps cell for a version: "30M", "Auto 212M", "212Mˢ".
export function tapsText(view) {
    if (!view?.taps) return view?.filter || '—';
    return /^Auto/i.test(view.filter || '') && !/ˢ$/.test(view.taps) ? `Auto ${view.taps}` : view.taps;
}

// ── the one on the air ────────────────────────────────────────────────

let current = null;
const hooks = new Set();

/// The version whose settings Sound shows now, or null.
export const versionViewNow = () => current;

/// Called on every change of it (the strip, the stages' rows, the line).
export function onVersionView(fn) { hooks.add(fn); return () => hooks.delete(fn); }

/// Shows `view` (viewOf) in Sound, or the rack again with null.
export function setVersionView(view) {
    const key = v => (v ? `${v.path}|${v.fs}|${v.taps}|${[...v.on].join(',')}|${v.apod}|${v.hr}|${v.out?.multiple || 0}` : '');
    if (key(view) === key(current)) return;
    current = view;
    for (const f of hooks) { try { f(current); } catch (e) { console.warn('[version-view]', e); } }
}

/// The version a status plays, or null: the row `e` on the air was told to
/// play the file `e.pin` (the Conversions page's Play) and the player plays
/// that very file from disk.
export function pinnedOnAir(s, e, onAir) {
    if (!s || !['playing', 'paused'].includes(s.state) || !e?.pin?.out || !onAir) return null;
    return String(onAir).toLowerCase() === String(e.pin.out).toLowerCase() ? e.pin : null;
}
