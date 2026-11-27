
// ══════════════════════════════════════════════════════════════════════
// Playlists, persistence, conversion records, and the rack signature.
//
// A playlist is a named ordered list of entries. The active playlist's
// entries live in state.list; switching playlists swaps them out.
// Conversion records survive a restart: when a file was already converted
// with the exact settings the rack shows right now, the row says so and
// offers the disk file for playback instead of re-converting.
//
// Call initLibrary({ state, listHooks, renderFileQueue, convLive, addFiles })
// once from initDropZone() in dropzone.js (as a microtask). That microtask
// fires during main.js's await of loadFilterInventory() — BEFORE loadSettings()
// runs. restore() therefore defers its final render via requestAnimationFrame,
// which fires after init() resumes and calls loadSettings(). main.js also
// calls rackRestored() right after loadSettings() for an immediate re-render.
// ══════════════════════════════════════════════════════════════════════

import { TAP_PRESETS, FS_PRESETS } from './inventory.js';
import { subsonicCornerHz, hpSensStep, HP_SENS_MIN, HP_SENS_MAX } from './dsprack.js';
import { xtcGeometryPayload } from './xtc.js';
import { state } from './state.js';
import { eqSig } from './eq/state.js';
import { getEqState } from './eq-bridge.js';
import { isMActive, getMSettings, snapToConvertOverride } from './memory.js';
import { addPopup } from './popups.js';
import { getDsdOutput } from './dsdexport.js';
import { loadCueParams, CUE_KEY } from './cue.js';
import { pinOf, pinHolds, isRenderName, sourceCandidates, madePairs } from './medialib/versions-core.js';

const STORE_KEY = 'auraPlaylists';
const STORE_VER = 1;

// ── module-level references filled in by initLibrary ─────────────────

let _hooks = null;       // listHooks from dropzone.js
let _render = null;      // renderFileQueue
let _convLive = null;    // convLive
let _addFiles = null;    // addFiles (to add paths to the player when switching)

// Playlist data: array of { id, name, entries: [{path, name, convs}] }
// The active playlist's entries are also in state.list (with runtime fields).
let _playlists = [];
let _activeId = 0;
let _plSeq = 1;

// The playlist that plays while another one is shown: its rows as they
// were, with the player's ids. The player goes on through it — its next
// track, its end, its repeat — and never into the list shown, whose tracks
// are added under their own playlist (Anton 16.10). Shown again, it takes
// the same rows back; it is let go (its tracks leave the player) once the
// player plays a file of another playlist.
let _held = null;   // { plId, entries } | null

let _saveTimer = null;

// ── rack signature ────────────────────────────────────────────────────

const $el = id => document.getElementById(id);
const chk = id => !!$el(id)?.checked;
const int = (id, fallback) => parseInt($el(id)?.value ?? '') || fallback;
const flt = (id, fallback) => parseFloat($el(id)?.value ?? '') || fallback;
// The window list is gone (every built-in filter is Kaiser), but its value
// stays in the signature: records written before carry it, and a signature
// without it would no longer match a single one of them.
const KAISER = 4;

/// The output's part of a signature: nothing for FLAC, so a record made
/// before DSF export (1.7.0) still matches its rack; the DSD rate and the
/// modulator for DSF, so a change of either is a file not made yet.
function outputSig() {
    const out = getDsdOutput();
    return out ? { out: 'dsf', dsdRate: out.multiple, modulator: out.kind } : {};
}

/// Hybrid-Phase's sensitivity step: in a signature only with HP on and off
/// step 0, so a record made before the slider (or at 0) still matches.
function hpSensSig(hp, n) {
    return hp && Number.isInteger(n) && n !== 0 && n >= HP_SENS_MIN && n <= HP_SENS_MAX ? { hpSens: n } : {};
}

/// A stable JSON string of every setting that changes the converted file.
/// GPU and BIT-PERFECT are excluded (GPU produces the same result as CPU;
/// BIT-PERFECT bypasses the converter entirely). Used to match finished
/// conversion records against the current rack.
export function rackSig() {
    const tapIdx = Math.min(int('convTapSlider', 0), TAP_PRESETS.length - 1);
    const fsIdx = Math.min(int('convFsSlider', 0), FS_PRESETS.length - 1);
    const taps = state.convCustomFilterPath ? null : TAP_PRESETS[tapIdx];
    const fs = FS_PRESETS[fsIdx];
    return JSON.stringify({
        fs,
        taps,
        customFilterPath: state.convCustomFilterPath || null,
        customFilterTaps: state.convCustomFilterPath ? state.convCustomFilterTaps : null,
        winType: state.convCustomFilterPath ? null : KAISER,
        pfr: chk('convFirResampling'),
        apodizing: int('convApodizing', 0),
        adaptiveApodizer: chk('convAdaptiveApodizer'),
        headroomDb: flt('convHeadroom', 0),
        hybridPhase: chk('convHybridPhase'),
        continuousAlpha: chk('convLabAlpha'),
        ...hpSensSig(chk('convHybridPhase'), hpSensStep()),
        tfsPhase: chk('convLabTfs'),
        iirDcBlocking: chk('convIirDc'),
        subsonicHz: subsonicCornerHz(),
        declip: chk('convLabDeclip'),
        isp: chk('convLabIsp'),
        adaptiveHeadroom: chk('convLabHeadroom'),
        xtc: chk('convLabXtc'),
        xtcGeometry: chk('convLabXtc') ? xtcGeometryPayload() : null,
        ...cueSig(),
        // EQ: included only when active (null → omit) so sigs from before EQ
        // landed are byte-identical and still match their records.
        ...(() => { const s = eqSig({ ...getEqState(), speakersAlways: false }); return s !== null ? { eq: s } : {}; })(),
        ...outputSig(),
    });
}

/// Like rackSig(), but M-controlled fields come from the supplied mOverride
/// instead of the DOM. Non-M fields (FS, taps, PFR, iirDcBlocking)
/// still come from the DOM. Used when a batch was started with an M override
/// so the stored conversion sig matches what rackSig() returns once the rack
/// is set to those same M settings.
export function rackSigWithMOverride(mo) {
    const tapIdx = Math.min(int('convTapSlider', 0), TAP_PRESETS.length - 1);
    const fsIdx = Math.min(int('convFsSlider', 0), FS_PRESETS.length - 1);
    const taps = state.convCustomFilterPath ? null : TAP_PRESETS[tapIdx];
    const fs = FS_PRESETS[fsIdx];
    const lf = mo.labFeatures || {};
    return JSON.stringify({
        fs,
        taps,
        customFilterPath: state.convCustomFilterPath || null,
        customFilterTaps: state.convCustomFilterPath ? state.convCustomFilterTaps : null,
        winType: state.convCustomFilterPath ? null : KAISER,
        pfr: chk('convFirResampling'),
        apodizing:        mo.apodizing,
        adaptiveApodizer: mo.adaptiveApodizer,
        headroomDb:       mo.headroomDb,
        hybridPhase:      mo.hybridPhase,
        continuousAlpha:  lf.continuousAlpha  || false,
        ...hpSensSig(mo.hybridPhase, lf.hpSens),
        tfsPhase:         lf.tfsPhase         || false,
        iirDcBlocking:    chk('convIirDc'),
        subsonicHz:       mo.subsonicHz,
        declip:           lf.declip           || false,
        isp:              lf.isp              || false,
        adaptiveHeadroom: lf.adaptiveHeadroom || false,
        xtc:              lf.xtc              || false,
        xtcGeometry:      lf.xtc ? (lf.xtcGeometry || null) : null,
        // CUE is global, as EQ: the rack's row and the SET window's values.
        ...cueSig(),
        // EQ from M override: M memory never stores EQ (it is global), so
        // lf.eq is normally undefined → eqSig returns null → omitted.
        ...(() => { const s = eqSig(lf.eq); return s !== null ? { eq: s } : {}; })(),
        ...outputSig(),
    });
}

/// A record's signature as the rack gives it now. One written while the
/// window list was there (before 1.5.0) may name Hann, Blackman or Nuttall
/// (winType 1–3; Hamming was saved as 4 already): the file was Kaiser all
/// the same, the list only named it. The value is set to Kaiser in place,
/// the order of the keys kept, so the record matches the rack that made it.
export function normalizeSig(sig) {
    let o;
    try { o = JSON.parse(sig); } catch (_) { return sig; }
    if (!o || typeof o !== 'object' || typeof o.winType !== 'number' || o.winType === KAISER) return sig;
    o.winType = KAISER;
    return JSON.stringify(o);
}

/// The signature a row's converted file must carry to be what this row
/// plays: the track's own settings while its memory (M) is on, the rack's
/// otherwise. The ✓ on the row and the player's disk playback both use it.
export function entrySig(e) {
    const snap = isMActive(e) ? getMSettings(e) : null;
    return snap ? rackSigWithMOverride(snapToConvertOverride(snap)) : rackSig();
}

/// Cue Consistency's part of the signature: present only while its row is
/// on (sigs from before CUE stay byte-identical), with every value of the
/// SET window but the A/B. CUE is baked into the converted file (Anton
/// 22.11), so a file made with these values plays while CUE is on, and one
/// made with others or without it does not: the signature says which.
export function cueSig() {
    if (!chk('convLabCue')) return {};
    const { bypass, ...p } = loadCueParams();
    return { cue: p };
}

/// The parameters the converter bakes, null with CUE off.
export function cueParamsForConvert() {
    if (!chk('convLabCue')) return null;
    const { bypass, ...p } = loadCueParams();
    return { ...p, bypass: false };
}

/// Until 22.11 CUE was in no converted file and forced a row to play live;
/// it is baked now, and the signature decides (`cueSig`).
export const cueForcesLive = () => false;

/// The player's volume is at its top (0 dB), or there is no slider.
export function volumeFull() {
    const v = document.getElementById('plVol');
    return !v || !(Number(v.value) < Number(v.max || 0));
}

/// A DSF goes to the DAC as it is, its one-bit data untouched: the volume
/// cannot act on it. Below full volume its row plays live, the level set
/// before the modulator, and from the file again at full volume.
const dsfWaitsForVolume = out => /\.dsf$/i.test(out || '') && !volumeFull();

/// The file of a row converted with its settings (the record matching its
/// entrySig()), played or not; null when there is none.
const convertedFile = e => {
    const pin = pinnedFile(e);
    if (pin) return pin;
    const sig = entrySig(e);
    return (e.convs || []).find(c => c.sig === sig)?.out ?? stampedFile(e, sig);
};

// ── versions found by their stamps (no record needed) ─────────────────

/// A render carries the hash of the settings it was made with (AURA_SETTINGS),
/// so a version that fits the rack is found on the disk even where no row
/// recorded it: the streamer, another machine, a list made afresh. Asked of
/// the engine (`conv_matching`) a few rows at a time, kept per signature.
const _stamp = new Map();       // sig → Map(source lower → null (asked) | [{ out, dsd }])
const _stampAsk = new Map();    // sig → Set(source)
let _stampTimer = 0;
const STAMP_SIGS = 8;
const lowPath = p => String(p || '').toLowerCase();

function stampedFile(e, sig) {
    if (!e?.path || !sig) return null;
    const hit = _stamp.get(sig)?.get(lowPath(e.path));
    if (hit === undefined) { askStamp(sig, e.path); return null; }
    if (!hit?.length) return null;
    // Only a version of the output's own kind (as a pin): a DSF while the
    // player sends DSD of its rate, a FLAC while it sends PCM.
    const fmt = state.playerOutFmt;
    return hit.find(v => !fmt || pinHolds(v, fmt))?.out ?? null;
}

function askStamp(sig, path) {
    let s = _stampAsk.get(sig);
    if (!s) _stampAsk.set(sig, s = new Set());
    s.add(path);
    if (!_stampTimer) _stampTimer = setTimeout(flushStamp, 200);
}

async function flushStamp() {
    _stampTimer = 0;
    const asks = [..._stampAsk];
    _stampAsk.clear();
    const invoke = window.__TAURI__?.core?.invoke;
    if (!invoke) return;
    let any = false;
    for (const [sig, set] of asks) {
        let m = _stamp.get(sig);
        if (!m) {
            if (_stamp.size >= STAMP_SIGS) _stamp.delete(_stamp.keys().next().value);
            _stamp.set(sig, m = new Map());
        }
        const sources = [...set].filter(p => m.get(lowPath(p)) === undefined);
        if (!sources.length) continue;
        for (const p of sources) m.set(lowPath(p), null);
        let r = [];
        try { r = await invoke('conv_matching', { sig, sources }); } catch (_) { r = []; }
        if (_stamp.get(sig) !== m) continue;    // forgotten meanwhile: asked again
        for (const p of sources) m.set(lowPath(p), []);
        for (const v of r || []) {
            if (!v?.out || !v.source) continue;
            const k = lowPath(v.source);
            const list = m.get(k) || [];
            list.push({ out: v.out, dsd: v.kind === 'dsd' ? Number(v.dsd) || 0 : 0 });
            m.set(k, list);
            any = true;
        }
    }
    if (any) { _render?.(); _hooks?.renderedChanged?.(); }
}

/// What the stamps said is asked again (versions written or deleted).
export function forgetStampMatches() {
    if (!_stamp.size) return;
    _stamp.clear();
    _render?.();
    _hooks?.renderedChanged?.();
}

// ── a version chosen by hand (the Conversions page's Play) ─────────────

/// The version a row was told to play, while the player's output is still
/// that version's own kind (versions-core.js pinHolds); a pin that no longer
/// holds is dropped, and the row plays by its settings again.
function pinnedFile(e) {
    if (!e.pin) return null;
    if (pinHolds(e.pin, state.playerOutFmt)) return e.pin.out;
    delete e.pin;
    return null;
}

/// The row `entry` plays the version `v` (a file of the scan) from now on,
/// whatever the rack says; the player is told at once. Not kept over a
/// restart.
export function pinVersion(entry, v) {
    entry.pin = pinOf(v);
    _render?.();
    _hooks?.renderedChanged?.();
}

/// The row plays by its settings again (the original, or its own
/// record), the player told at once. True when it had a version.
export function unpinRow(entry) {
    if (!entry?.pin) return false;
    delete entry.pin;
    _render?.();
    _hooks?.renderedChanged?.();
    return true;
}

/// Every row plays by its settings again (■: the versions are done).
export function unpinAll() {
    let any = false;
    for (const e of [...state.list, ...(_held?.entries || [])]) if (e.pin) { delete e.pin; any = true; }
    if (any) { _render?.(); _hooks?.renderedChanged?.(); }
    return any;
}

/// Rows told to play one of these files (lower-case paths) forget it.
export function dropPins(outs) {
    let any = false;
    for (const e of [...state.list, ...(_held?.entries || [])]) {
        if (e.pin && outs.has(String(e.pin.out).toLowerCase())) { delete e.pin; any = true; }
    }
    if (any) { _render?.(); _hooks?.renderedChanged?.(); }
}

// ── what this app made (for the versions of originals that are gone) ───

const MADE_KEY = 'aura.convMade';
const MADE_MAX = 20000;

function readMade() {
    try { return JSON.parse(localStorage.getItem(MADE_KEY) || '{}') || {}; } catch (_) { return {}; }
}
function writeMade(m) {
    try { localStorage.setItem(MADE_KEY, JSON.stringify(m)); } catch (_) {}
}
function noteMade(out, source) {
    const m = readMade();
    const keys = Object.keys(m);
    if (keys.length >= MADE_MAX) delete m[keys[0]];
    m[String(out).toLowerCase()] = { out, source };
    writeMade(m);
}

/// Every version this app is known to have made, { out, source }: the rows'
/// records in all the playlists and the kept list (versions-core.js).
export function madeVersions() {
    const rows = [...state.list, ..._playlists.filter(p => p.id !== _activeId).flatMap(p => p.entries || [])];
    return madePairs(rows, readMade());
}

/// These files (lower-case paths) are no longer on disk: out of the kept
/// list, the rows' records and the pins.
export function forgetVersions(outs) {
    const m = readMade();
    for (const k of Object.keys(m)) if (outs.has(k)) delete m[k];
    writeMade(m);
    const lowOut = c => String(c.out).toLowerCase();
    for (const e of [...state.list, ..._playlists.flatMap(p => p.entries || [])]) {
        if (e.convs?.length) e.convs = e.convs.filter(c => !outs.has(lowOut(c)));
        if (e.stale?.length) { e.stale = e.stale.filter(c => !outs.has(lowOut(c))); if (!e.stale.length) delete e.stale; }
    }
    dropPins(outs);
    _stamp.clear();
    scheduleSave();
    _render?.();
    _hooks?.renderedChanged?.();
}

/// The signature the file `out` was recorded with by a row, or null.
export function recordedSig(out) {
    const k = String(out || '').toLowerCase();
    for (const e of [...state.list, ..._playlists.flatMap(p => p.entries || [])]) {
        for (const c of e.convs || []) if (String(c.out).toLowerCase() === k) return c.sig ?? null;
    }
    return null;
}

/// Why a row converted with its settings plays live all the same: 'cue'
/// (Cue Consistency is on), 'volume' (a DSF below full volume); null when
/// its file plays, or it has none.
export function liveReason(e) {
    const out = convertedFile(e);
    if (!out) return null;
    if (cueForcesLive()) return 'cue';
    return dsfWaitsForVolume(out) ? 'volume' : null;
}

/// The converted file a row plays now: the record matching its entrySig(),
/// else null (it plays live).
export function playedFile(e) {
    if (cueForcesLive()) return null;
    const out = convertedFile(e);
    return dsfWaitsForVolume(out) ? null : out;
}

// ── conversion records ────────────────────────────────────────────────

/// Called by dropzone.js when a conversion record reaches done with an output
/// path. Pushes the record into entry.convs, removes any older records with
/// the same out path (in any playlist, since output paths are unique), persists,
/// and notifies the player that the rendered map may have changed.
export function pushConvRecord(entry, f) {
    if (!f.outPath || !f.sig) return;
    const rec = {
        sig: f.sig,
        out: f.outPath,
        labChain: f.labChain ? [...f.labChain] : [],
        badge: f.badge ?? 0,
        rack: f.rack ? [...f.rack] : [],
        at: Date.now(),
        // How fast it was made (batchstats.js), shown beside the row's ✓.
        ...(f.speedX > 0 ? { speedX: f.speedX } : {}),
    };
    // Remove any earlier record with this exact output path from every
    // entry in every playlist — the file can only be one thing on disk.
    for (const pl of _playlists) {
        for (const pe of pl.entries) {
            const el = state.list.find(x => x.path === pe.path) || null;
            const target = el ?? pe;
            if (!target.convs) target.convs = [];
            target.convs = target.convs.filter(c => c.out !== rec.out);
            if (target.stale) target.stale = target.stale.filter(c => c.out !== rec.out);
        }
    }
    if (!entry.convs) entry.convs = [];
    entry.convs = entry.convs.filter(c => c.out !== rec.out);
    // A new render under an older one's name replaces it.
    if (entry.stale) entry.stale = entry.stale.filter(c => c.out !== rec.out);
    entry.convs.push(rec);
    noteMade(rec.out, entry.path);
    scheduleSave();
    _stamp.clear();
    _hooks?.renderedChanged?.();
}

/// Check the conversion records of `entries` against the disk, in one call:
/// a record whose file is gone is dropped; one whose render the backend calls
/// not current (made before the stamp, or by an older generation of the
/// sound or of a stage) moves to entry.stale with the reason as `tip`, so the
/// row reads as not converted, says why, and the player plays the source. The
/// file itself stays on disk; converting again replaces it. A stale record
/// whose file counts again (a newer one under the same name) comes back.
/// Without the backend's check (an older backend), existence alone decides.
export async function checkConvRecords(entries, invoke) {
    const recs = [];
    for (const e of entries) {
        for (const c of e.convs || []) recs.push([e, c]);
        for (const c of e.stale || []) recs.push([e, c]);
    }
    if (recs.length === 0) return;
    const paths = recs.map(([, c]) => c.out);
    let checks;
    try {
        checks = await invoke('player_render_checks', { paths });
    } catch (_) {
        try {
            const exists = await invoke('player_paths_exist', { paths });
            checks = exists.map(x => ({ exists: x !== false, valid: true }));
        } catch (_) {
            return; // neither available: keep the records as they are
        }
    }
    const touched = new Set(recs.map(([e]) => e));
    for (const e of touched) { e.convs = []; e.stale = []; }
    recs.forEach(([e, c], i) => {
        const k = checks?.[i] ?? { exists: true, valid: true };
        if (k.exists === false) return;
        const { tip, ...rec } = c;
        if (k.valid !== false) e.convs.push(rec);
        else e.stale.push({ ...rec, tip: k.tip || 'Made by an older version — convert again' });
    });
    for (const e of touched) if (e.stale.length === 0) delete e.stale;
}

/// The persisted form of an entry's stale records (none: no field).
const staleOf = e => (e.stale?.length ? { stale: e.stale.map(c => ({ ...c })) } : {});

// ── rendered list (for player_set_settings) ───────────────────────────

/// For every entry in the active playlist, and in the one that plays while
/// it is shown, that has a trackId: the file it plays (playedFile), else
/// null. Called by player.js through listHooks.rendered.
function rendered() {
    return [...state.list, ...(_held?.entries || [])]
        .filter(e => e.trackId != null)
        .map(e => ({ id: e.trackId, path: playedFile(e) }));
}

// ── playlists ─────────────────────────────────────────────────────────

export function getPlaylists() { return _playlists; }
export function getActiveId() { return _activeId; }

/// The row of a track of the playlist that plays while another one is
/// shown (null for any other track).
export function heldEntryOf(trackId) {
    return _held?.entries.find(e => e.trackId === trackId) ?? null;
}

/// Whether the player's track `trackId` is one of a playlist's rows: the
/// one shown or the one that plays while it is shown.
export function inPlaylists(trackId) {
    return state.list.some(e => e.trackId === trackId) || !!heldEntryOf(trackId);
}

/// The playlist the player's track `trackId` is a row of: { id, name, shown }
/// (shown: it is the one in the list now), or null for a track of no
/// playlist.
export function playlistOfTrack(trackId) {
    if (trackId == null) return null;
    const id = state.list.some(e => e.trackId === trackId) ? _activeId
        : heldEntryOf(trackId) ? _held.plId : null;
    const pl = id == null ? null : _playlists.find(p => p.id === id);
    return pl ? { id: pl.id, name: pl.name, shown: pl.id === _activeId } : null;
}

/// The playlist that plays (or is paused on), as playlistOfTrack; null when
/// the player is stopped.
export function playingPlaylist() {
    const now = _hooks?.nowPlaying?.();
    if (!now || now.id == null || !now.state || now.state === 'stopped') return null;
    return playlistOfTrack(now.id);
}

/// The player is on `cur` ({ id, state }, from its status): a file of
/// another playlist than the one held (played from the list shown) lets
/// that one go.
export function playerMoved(cur) {
    if (!_held || cur?.id == null || cur.state === 'stopped') return;
    if (!heldEntryOf(cur.id)) releaseHeld();
}

/// The tracks of these rows leave the player.
function removeFromPlayer(entries) {
    const invoke = window.__TAURI__?.core?.invoke;
    if (!invoke) return;
    for (const e of entries) {
        if (e.trackId != null) invoke('player_remove', { id: e.trackId }).catch(() => {});
    }
}

function releaseHeld() {
    if (!_held) return;
    removeFromPlayer(_held.entries);
    _held = null;
}

export function getActivePl() {
    return _playlists.find(p => p.id === _activeId) || _playlists[0] || null;
}

/// Serialize the runtime state.list back to the active playlist's entries
/// (the persisted form: path, name, convs only — no runtime fields).
function flushActiveToPlaylist() {
    const pl = getActivePl();
    if (!pl) return;
    pl.entries = state.list.map(e => ({
        path: e.path,
        name: e.name,
        convs: (e.convs || []).map(c => ({ ...c })),
        ...staleOf(e),
    }));
}

export function createPlaylist(name) {
    const id = _plSeq++;
    _playlists.push({ id, name: name || ('Playlist ' + id), entries: [] });
    scheduleSave();
    refreshDropdown();
    return id;
}

export function renamePlaylist(id, name) {
    const pl = _playlists.find(p => p.id === id);
    if (pl) { pl.name = name; scheduleSave(); refreshDropdown(); }
}

export function deletePlaylist(id) {
    if (_playlists.length <= 1) return false;
    // A playlist with running conversions cannot be deleted: the records
    // are linked to its entries and would become orphans.
    const pl = _playlists.find(p => p.id === id);
    if (!pl) return false;
    if (id === _activeId) {
        const other = _playlists.find(p => p.id !== id);
        if (other) switchPlaylist(other.id);
    }
    // Deleted while it plays: the track on the air stops and its tracks
    // leave the player.
    if (_held?.plId === id) { _hooks?.stopIfOn?.(_held.entries); releaseHeld(); }
    _playlists = _playlists.filter(p => p.id !== id);
    scheduleSave();
    refreshDropdown();
    return true;
}

export function switchPlaylist(id) {
    if (id === _activeId) { refreshDropdown(); return; }
    // Process any done-but-not-pushed conversion records while state.list still
    // maps the old playlist's entry ids. After flushActiveToPlaylist() the ids
    // change, so records that finish after the switch would find no matching entry.
    _render?.();
    flushActiveToPlaylist();

    const pl = _playlists.find(p => p.id === id);
    if (!pl) return;

    // The playlist shown plays (or is paused on, or gets a track ready): it
    // goes on in the player as it is — what follows its track stays in it —
    // and its rows are held here. One that does not play leaves the player.
    // Before 16.10 the track on the air stayed and the new playlist's tracks
    // were queued after it: at its end the player went on into them.
    const nowPlaying = _hooks?.nowPlaying?.() ?? { id: null, state: 'stopped' };
    const plays = nowPlaying.id != null && nowPlaying.state && nowPlaying.state !== 'stopped'
        && state.list.some(e => e.trackId === nowPlaying.id);
    if (plays) {
        releaseHeld();
        _held = { plId: _activeId, entries: state.list };
    } else {
        removeFromPlayer(state.list);
    }

    _activeId = id;

    if (_held?.plId === id) {
        // The playlist that plays, shown again: its own rows, with the
        // records its files got meanwhile.
        state.list = _held.entries;
        _held = null;
        state.list.forEach((e, i) => {
            const pe = pl.entries[i];
            if (pe?.path === e.path) {
                e.convs = (pe.convs || []).map(c => ({ ...c }));
                if (pe.stale?.length) e.stale = pe.stale.map(c => ({ ...c })); else delete e.stale;
            }
        });
    } else {
        state.list = pl.entries.map(pe => ({
            id: state.listSeq++,
            path: pe.path,
            name: pe.name,
            trackId: null,
            info: null,
            conv: null,
            convs: (pe.convs || []).map(c => ({ ...c })),
            ...staleOf(pe),
            unreadable: null,
        }));
    }

    // Add the new playlist's paths to the player (a held one's are there).
    const toAdd = state.list.filter(e => e.trackId == null && !e.unreadable);
    if (toAdd.length > 0 && _addFiles) {
        _addFiles(toAdd.map(e => e.path), toAdd);
    }

    _render?.();
    _hooks?.renderedChanged?.();
    scheduleSave();
    refreshDropdown();
}

// ── persistence ───────────────────────────────────────────────────────

function scheduleSave() {
    clearTimeout(_saveTimer);
    _saveTimer = setTimeout(save, 300);
}

function save() {
    flushActiveToPlaylist();
    const data = {
        version: STORE_VER,
        active: _activeId,
        plSeq: _plSeq,
        playlists: _playlists.map(pl => ({
            id: pl.id,
            name: pl.name,
            entries: pl.entries.map(e => ({
                path: e.path,
                name: e.name,
                convs: (e.convs || []),
                ...staleOf(e),
            })),
        })),
    };
    try { localStorage.setItem(STORE_KEY, JSON.stringify(data)); } catch (_) {}
}

async function restore() {
    let data = null;
    try {
        const raw = localStorage.getItem(STORE_KEY);
        if (raw) data = JSON.parse(raw);
    } catch (_) {}

    if (!data || data.version !== STORE_VER || !Array.isArray(data.playlists) || data.playlists.length === 0) {
        // No saved state: create the first playlist and it starts empty.
        _playlists = [{ id: 1, name: 'Playlist 1', entries: [] }];
        _activeId = 1;
        _plSeq = 2;
        refreshDropdown();
        return;
    }

    _plSeq = data.plSeq || 2;
    _playlists = data.playlists;
    _activeId = data.active || _playlists[0].id;
    // Records made while the window list was there match the rack again.
    for (const pl of _playlists) {
        for (const e of pl.entries || []) {
            for (const c of e.convs || []) if (c.sig) c.sig = normalizeSig(c.sig);
        }
    }

    const activePl = getActivePl();
    if (!activePl) { refreshDropdown(); return; }

    await originalsForVersions(_playlists, (cmd, args) => window.__TAURI__.core.invoke(cmd, args));

    // Every playlist's records, once: gone files dropped, older renders set aside.
    await checkConvRecords(_playlists.flatMap(pl => pl.entries || []),
        (cmd, args) => window.__TAURI__.core.invoke(cmd, args));

    // Populate state.list from the active playlist.
    state.list = activePl.entries.map(e => ({
        id: state.listSeq++,
        path: e.path,
        name: e.name,
        trackId: null,
        info: null,
        conv: null,
        convs: (e.convs || []).map(c => ({ ...c })),
        ...staleOf(e),
        unreadable: null,
    }));

    // D2: Re-link entries to the backend's existing queue by path rather than
    // re-adding every track. After a page reload the Rust backend still holds
    // the tracks from the previous session; player_queue() returns them.
    // Entries that match by path get their trackId set; unmatched ones are
    // added via _addFiles as usual. Backend tracks not in the list are removed.
    // Note: a conversion running in the backend keeps running; this page does
    // not re-attach its progress after a reload.
    let queuedTracks = [];
    try {
        queuedTracks = await window.__TAURI__.core.invoke('player_queue');
    } catch (_) {}

    if (queuedTracks.length > 0) {
        // Build path → [track, ...] queues (duplicates matched in order).
        // Only this playlist's tracks: another one's (it played while this
        // one was shown) would take its next track from that one.
        const pathQueues = new Map();
        for (const t of queuedTracks) {
            if (t.list != null && t.list !== _activeId) continue;
            if (!pathQueues.has(t.path)) pathQueues.set(t.path, []);
            pathQueues.get(t.path).push(t);
        }
        for (const e of state.list) {
            const q = pathQueues.get(e.path);
            if (q && q.length > 0) e.trackId = q.shift().id;
        }
        // Remove backend tracks that are not in the restored list.
        const matchedIds = new Set(state.list.map(e => e.trackId).filter(id => id != null));
        for (const t of queuedTracks) {
            if (!matchedIds.has(t.id)) {
                window.__TAURI__?.core?.invoke?.('player_remove', { id: t.id }).catch(() => {});
            }
        }
    }

    // Add entries that did not match any backend track.
    const toAdd = state.list.filter(e => e.trackId == null);
    if (toAdd.length > 0 && _addFiles) {
        _addFiles(toAdd.map(e => e.path), toAdd);
    }

    // Re-send the rendered map so any restored conversion records take effect.
    _hooks?.renderedChanged?.();

    // Defer the render until after main.js's loadSettings() has restored the
    // rack. This microtask fires during init()'s await of loadFilterInventory()
    // — BEFORE loadSettings() — so the rack-correct render is deferred here.
    // main.js also calls rackRestored() right after loadSettings() for a faster
    // update.
    requestAnimationFrame(() => {
        _render?.();
        _hooks?.renderedChanged?.();
        refreshDropdown();
    });
}

/// The lists hold originals: a saved row that is a converted file (an older
/// build put one there to play it as it is) becomes its original's row,
/// where the original is on disk — or goes, when the playlist has that row
/// already. A version with no original stays as the file it is.
export async function originalsForVersions(playlists, invoke) {
    const rows = playlists.flatMap(pl => (pl.entries || []).filter(e => isRenderName(e.path)));
    if (!rows.length) return;
    const cands = rows.map(e => sourceCandidates(e.path));
    let exists;
    try { exists = await invoke('player_paths_exist', { paths: cands.flat() }); } catch (_) { return; }
    const src = new Map();
    let k = 0;
    rows.forEach((e, i) => {
        const hit = cands[i].find((_, j) => exists?.[k + j] !== false && exists?.[k + j] != null);
        k += cands[i].length;
        if (hit) src.set(e, hit);
    });
    if (!src.size) return;
    const base = p => String(p).split(/[\\/]/).pop();
    for (const pl of playlists) {
        const have = new Set((pl.entries || []).filter(e => !src.has(e)).map(e => String(e.path).toLowerCase()));
        pl.entries = (pl.entries || []).flatMap(e => {
            const s = src.get(e);
            if (!s) return [e];
            if (have.has(s.toLowerCase())) return [];
            have.add(s.toLowerCase());
            return [{ path: s, name: base(s), convs: [] }];
        });
    }
}

// ── dropdown UI ───────────────────────────────────────────────────────

let _dropdownOpen = false;
let _renamingId = null;

/// Rebuild the playlist control inside #plPlaylistCtrl (created by
/// dropzone.js's buildListHead as a placeholder).
export function refreshDropdown() {
    const ctrl = document.getElementById('plPlaylistCtrl');
    if (!ctrl) return;

    const activePl = getActivePl();
    const count = state.list.length;
    const name = activePl?.name ?? 'Playlist 1';

    // This runs on every list update (several times a second while a batch
    // converts), so the trigger is made once and only its text changes after
    // that: a trigger rebuilt under a pressed pointer lost the click.
    let trigger = document.getElementById('plDdTrigger');
    if (!trigger || trigger.parentNode !== ctrl) {
        ctrl.innerHTML = '';
        trigger = document.createElement('button');
        trigger.type = 'button';
        trigger.className = 'pl-dd-trigger';
        trigger.id = 'plDdTrigger';
        trigger.title = 'Playlists';
        trigger.innerHTML = `<span class="pl-dd-name"></span>`
            + `<span class="pl-dd-count"></span>`
            + `<svg class="pl-dd-arrow" width="8" height="5" viewBox="0 0 8 5"><path d="M0 0l4 5 4-5z" fill="currentColor"/></svg>`;
        ctrl.appendChild(trigger);
        // Click on trigger toggles the dropdown.
        trigger.addEventListener('click', ev => {
            ev.stopPropagation();
            _dropdownOpen = !_dropdownOpen;
            _renamingId = null;
            refreshDropdown();
        });
    }
    const nameEl = trigger.querySelector('.pl-dd-name');
    const countEl = trigger.querySelector('.pl-dd-count');
    if (nameEl.textContent !== name) nameEl.textContent = name;
    if (countEl.textContent !== String(count)) countEl.textContent = String(count);

    // The panel: rebuilt only when what it shows changes (open or closed,
    // the playlists, the active one, the one being renamed) — a rebuild would
    // also take the focus out of the rename field.
    const oldPanel = document.getElementById('plDdPanel');
    const panelKey = _dropdownOpen
        ? JSON.stringify([_playlists.map(p => [p.id, p.name]), _activeId, _renamingId])
        : null;
    if (!_dropdownOpen) {
        oldPanel?.remove();
        ctrl._panelKey = null;
        return;
    }
    if (oldPanel && ctrl._panelKey === panelKey) return;
    ctrl._panelKey = panelKey;
    oldPanel?.remove();

    // The dropdown panel (shown when open).
    {
        const panel = document.createElement('div');
        panel.className = 'pl-dd-panel';
        panel.id = 'plDdPanel';
        // The playlists scroll in their own box, whole rows only (fitDropdown);
        // New playlist stays under it, always in view.
        const rows = document.createElement('div');
        rows.className = 'pl-dd-rows';
        panel.appendChild(rows);

        for (const pl of _playlists) {
            const item = document.createElement('div');
            item.className = 'pl-dd-item' + (pl.id === _activeId ? ' active' : '');
            item.dataset.plId = pl.id;

            if (_renamingId === pl.id) {
                // Inline rename field.
                const input = document.createElement('input');
                input.type = 'text';
                input.className = 'pl-dd-rename';
                input.value = pl.name;
                input.maxLength = 64;
                // Focus after the next paint so the input is actually in the DOM.
                requestAnimationFrame(() => { input.focus(); input.select(); });
                // Enter keeps the name; Esc drops it with the playlists
                // (the popup's escape below), and the field taken away then
                // keeps nothing.
                input.addEventListener('keydown', ev => {
                    if (ev.key === 'Enter') {
                        commitRename(pl.id, input.value.trim());
                        ev.preventDefault();
                    }
                });
                input.addEventListener('blur', () => {
                    if (_renamingId === pl.id) commitRename(pl.id, input.value.trim());
                });
                item.appendChild(input);
            } else {
                const nameSpan = document.createElement('span');
                nameSpan.className = 'pl-dd-item-name';
                nameSpan.textContent = pl.name;
                item.appendChild(nameSpan);

                const actions = document.createElement('span');
                actions.className = 'pl-dd-actions';

                const renameBtn = document.createElement('button');
                renameBtn.type = 'button';
                renameBtn.className = 'pl-dd-act-btn';
                renameBtn.textContent = 'Rename';
                renameBtn.addEventListener('click', ev => {
                    ev.stopPropagation();
                    _renamingId = pl.id;
                    refreshDropdown();
                });
                actions.appendChild(renameBtn);

                if (_playlists.length > 1) {
                    const delBtn = document.createElement('button');
                    delBtn.type = 'button';
                    delBtn.className = 'pl-dd-act-btn pl-dd-del';
                    delBtn.textContent = 'Delete';
                    delBtn.addEventListener('click', ev => {
                        ev.stopPropagation();
                        const hasPending = state.list.some(e => _convLive?.(e.conv));
                        if (hasPending) {
                            delBtn.textContent = 'Has active conversions';
                            setTimeout(() => { delBtn.textContent = 'Delete'; }, 2000);
                            return;
                        }
                        deletePlaylist(pl.id);
                    });
                    actions.appendChild(delBtn);
                }
                item.appendChild(actions);
            }
            rows.appendChild(item);
        }

        // Divider + New playlist.
        const divider = document.createElement('div');
        divider.className = 'pl-dd-divider';
        panel.appendChild(divider);

        const newBtn = document.createElement('div');
        newBtn.className = 'pl-dd-new';
        newBtn.textContent = '+ New playlist';
        newBtn.addEventListener('click', ev => {
            ev.stopPropagation();
            _dropdownOpen = false;
            // The new playlist starts empty and is shown as any other: the
            // one that plays goes on (switchPlaylist).
            switchPlaylist(createPlaylist('Playlist ' + _plSeq));
        });
        panel.appendChild(newBtn);

        ctrl.appendChild(panel);
        fitDropdown(panel);
    }

    // Non-renaming item click: switch playlist.
    ctrl.querySelectorAll('.pl-dd-item:not(.active)').forEach(item => {
        item.addEventListener('click', ev => {
            if (ev.target.closest('.pl-dd-act-btn') || ev.target.closest('.pl-dd-rename')) return;
            const id = parseInt(item.dataset.plId);
            if (id && id !== _activeId) {
                _dropdownOpen = false;
                switchPlaylist(id);
            }
        });
    });
}

/// The open list reaches down to the window's bottom edge, a small margin
/// short of it, and never past it (Anton 21.11: with many playlists it ran
/// out of the window and was cut). Its height is a whole number of rows, so
/// no row is ever left half-shown at the bottom; the rest scrolls, a row at
/// a time, and the active playlist is brought into view.
const DD_MARGIN = 10;
const DD_MIN_ROWS = 3;
let _fitOnResize = false;
function fitDropdown(panel) {
    // Kept fitted while open, should the window's height change.
    if (!_fitOnResize) {
        _fitOnResize = true;
        window.addEventListener('resize', () => {
            const open = document.getElementById('plDdPanel');
            if (open) fitDropdown(open);
        });
    }
    const rows = panel.querySelector('.pl-dd-rows');
    const first = rows?.firstElementChild;
    if (!first) return;
    rows.style.maxHeight = '';
    const rowH = first.getBoundingClientRect().height;
    if (!rowH) return;
    const top = rows.getBoundingClientRect().top;
    const below = panel.getBoundingClientRect().bottom - rows.getBoundingClientRect().bottom;
    const room = window.innerHeight - DD_MARGIN - below - top;
    const fit = Math.max(DD_MIN_ROWS, Math.floor(room / rowH));
    if (fit < rows.children.length) rows.style.maxHeight = (fit * rowH) + 'px';
    const active = rows.querySelector('.pl-dd-item.active');
    if (active) rows.scrollTop = Math.max(0, active.offsetTop - rows.offsetTop - rowH * Math.floor((fit - 1) / 2));
}

function commitRename(id, name) {
    _renamingId = null;
    if (name) renamePlaylist(id, name);
    _dropdownOpen = false;
    refreshDropdown();
}

/// The playlists put away by a press outside them (popups.js: that press
/// goes nowhere else, so the name field does not lose the focus to it). A
/// name being typed is kept, as when the field loses the focus.
function putAwayDropdown() {
    const field = document.querySelector('#plDdPanel .pl-dd-rename');
    if (_renamingId != null && field) { commitRename(_renamingId, field.value.trim()); return; }
    _dropdownOpen = false;
    _renamingId = null;
    refreshDropdown();
}

/// Esc: put away, and a name being typed is dropped.
function dropDropdown() {
    _renamingId = null;
    _dropdownOpen = false;
    refreshDropdown();
}

/// The playlists as a popup: a press outside them and their button only
/// puts them away; the button toggles them as before. Open means seen: with
/// the list's head out of sight (the radio in the list's place) a press is
/// not taken for them.
addPopup({
    isOpen: () => _dropdownOpen && !!document.getElementById('plDdPanel')?.getClientRects().length,
    inside: t => !!document.getElementById('plPlaylistCtrl')?.contains(t),
    close: putAwayDropdown,
    escape: dropDropdown,
});

const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// ── init ──────────────────────────────────────────────────────────────

/// Called by main.js right after loadSettings() restores the rack. Triggers
/// an immediate re-render so rows reflect the now-correct rackSig() without
/// waiting for the next requestAnimationFrame cycle from restore().
export function rackRestored() {
    _render?.();
    _hooks?.renderedChanged?.();
}

/// Return saved conversion records for a path from any playlist's stored
/// data. Called by dropzone.js when creating a new entry for a file that
/// has been converted before, so the row shows ✓ immediately.
export function getSavedConvsForPath(path) {
    for (const pl of _playlists) {
        for (const pe of pl.entries) {
            if (pe.path === path && Array.isArray(pe.convs) && pe.convs.length > 0) {
                return pe.convs.map(c => ({ ...c }));
            }
        }
    }
    return [];
}

/// The saved older renders of a path (see checkConvRecords), for a new row
/// of a file that was converted by an older version.
export function getSavedStaleForPath(path) {
    for (const pl of _playlists) {
        for (const pe of pl.entries) {
            if (pe.path === path && pe.stale?.length) return pe.stale.map(c => ({ ...c }));
        }
    }
    return [];
}

/// Called from dropzone.js's initDropZone via a microtask, so it fires during
/// main.js's await of loadFilterInventory() — BEFORE loadSettings() runs. The
/// rack-correct render is deferred inside restore() with requestAnimationFrame.
export async function initLibrary({ listHooks, renderFileQueue, convLive, addFiles: af }) {
    _hooks = listHooks;
    _render = renderFileQueue;
    _convLive = convLive;
    _addFiles = af;

    // Wire the rendered hook so player.js can call it.
    listHooks.rendered = rendered;

    // The SET window writes CUE's values: they are in the signature, so
    // what the rows play changes with them.
    window.addEventListener?.('storage', (ev) => {
        if (ev.key === CUE_KEY && chk('convLabCue')) { _render?.(); listHooks.renderedChanged?.(); }
    });

    // Save on every change that goes through the list.
    const origAdded = listHooks.added;
    listHooks.added = (entries) => {
        origAdded?.(entries);
        scheduleSave();
        refreshDropdown();
    };
    const origRemoved = listHooks.removed;
    listHooks.removed = (entry) => {
        origRemoved?.(entry);
        scheduleSave();
        refreshDropdown();
    };
    const origMoved = listHooks.moved;
    listHooks.moved = (list) => {
        origMoved?.(list);
        scheduleSave();
    };

    await restore();
}
