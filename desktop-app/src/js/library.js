
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
import { subsonicCornerHz } from './dsprack.js';
import { xtcGeometryPayload } from './xtc.js';
import { state } from './state.js';
import { isMActive, getMSettings, snapToConvertOverride } from './memory.js';
import { addPopup } from './popups.js';

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
        tfsPhase: chk('convLabTfs'),
        iirDcBlocking: chk('convIirDc'),
        subsonicHz: subsonicCornerHz(),
        declip: chk('convLabDeclip'),
        isp: chk('convLabIsp'),
        adaptiveHeadroom: chk('convLabHeadroom'),
        xtc: chk('convLabXtc'),
        xtcGeometry: chk('convLabXtc') ? xtcGeometryPayload() : null,
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
        tfsPhase:         lf.tfsPhase         || false,
        iirDcBlocking:    chk('convIirDc'),
        subsonicHz:       mo.subsonicHz,
        declip:           lf.declip           || false,
        isp:              lf.isp              || false,
        adaptiveHeadroom: lf.adaptiveHeadroom || false,
        xtc:              lf.xtc              || false,
        xtcGeometry:      lf.xtc ? (lf.xtcGeometry || null) : null,
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
        }
    }
    if (!entry.convs) entry.convs = [];
    entry.convs = entry.convs.filter(c => c.out !== rec.out);
    entry.convs.push(rec);
    scheduleSave();
    _hooks?.renderedChanged?.();
}

// ── rendered list (for player_set_settings) ───────────────────────────

/// For every entry in the active playlist that has a trackId: the output
/// path of the conversion record matching its entrySig(), else null.
/// Called by player.js through listHooks.rendered.
function rendered() {
    return state.list
        .filter(e => e.trackId != null)
        .map(e => {
            const sig = entrySig(e);
            return { id: e.trackId, path: (e.convs || []).find(c => c.sig === sig)?.out ?? null };
        });
}

// ── playlists ─────────────────────────────────────────────────────────

export function getPlaylists() { return _playlists; }
export function getActiveId() { return _activeId; }

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

    // The player keeps the track that is playing; everything else is removed.
    const nowPlaying = _hooks?.nowPlaying?.() ?? { id: null, state: 'stopped' };
    const keepEntry = nowPlaying.id != null
        ? state.list.find(e => e.trackId === nowPlaying.id) : null;

    for (const e of state.list) {
        if (e === keepEntry) continue;
        if (e.trackId != null) {
            const invoke = window.__TAURI__?.tauri?.invoke;
            if (invoke) invoke('player_remove', { id: e.trackId }).catch(() => {});
        }
    }

    _activeId = id;

    // Rebuild state.list from the playlist's entries, re-using the playing
    // entry's runtime fields when it moves to the new playlist.
    state.list = pl.entries.map(pe => {
        const existing = keepEntry && keepEntry.path === pe.path ? keepEntry : null;
        return existing || {
            id: state.listSeq++,
            path: pe.path,
            name: pe.name,
            trackId: null,
            info: null,
            conv: null,
            convs: (pe.convs || []).map(c => ({ ...c })),
            unreadable: null,
        };
    });

    // Add the new playlist's paths to the player (the playing one is already there).
    const toAdd = state.list.filter(e => e.trackId == null);
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

    // Drop conversion records whose output file no longer exists. We ask the
    // backend; if the command is missing (older backend), we keep the records.
    const allOuts = activePl.entries.flatMap(e => (e.convs || []).map(c => c.out));
    if (allOuts.length > 0) {
        try {
            const exists = await window.__TAURI__.tauri.invoke('player_paths_exist', { paths: allOuts });
            let i = 0;
            for (const e of activePl.entries) {
                e.convs = (e.convs || []).filter(c => {
                    const ok = exists[i] !== false;
                    i++;
                    return ok;
                });
            }
        } catch (_) {
            // player_paths_exist not available: keep the records.
        }
    }

    // Populate state.list from the active playlist.
    state.list = activePl.entries.map(e => ({
        id: state.listSeq++,
        path: e.path,
        name: e.name,
        trackId: null,
        info: null,
        conv: null,
        convs: (e.convs || []).map(c => ({ ...c })),
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
        queuedTracks = await window.__TAURI__.tauri.invoke('player_queue');
    } catch (_) {}

    if (queuedTracks.length > 0) {
        // Build path → [track, ...] queues (duplicates matched in order).
        const pathQueues = new Map();
        for (const t of queuedTracks) {
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
                window.__TAURI__?.tauri?.invoke?.('player_remove', { id: t.id }).catch(() => {});
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
            panel.appendChild(item);
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
            // Flush and persist the current playlist BEFORE changing _activeId so
            // flushActiveToPlaylist() writes into the old playlist, not the new one.
            flushActiveToPlaylist();
            save();
            const id = createPlaylist('Playlist ' + _plSeq);
            _activeId = id;
            _dropdownOpen = false;
            // The new playlist starts empty; remove the old tracks from the player.
            for (const e of state.list) {
                if (e.trackId != null) {
                    const invoke = window.__TAURI__?.tauri?.invoke;
                    if (invoke) invoke('player_remove', { id: e.trackId }).catch(() => {});
                }
            }
            state.list = [];
            _render?.();
            _hooks?.renderedChanged?.();
            scheduleSave();
            refreshDropdown();
        });
        panel.appendChild(newBtn);

        ctrl.appendChild(panel);
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
