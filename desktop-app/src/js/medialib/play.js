// ══════════════════════════════════════════════════════════════════════
// The library plays through the app's own list and player, as a playlist's
// row does: its queue is a playlist of its own, "Library", made on its first
// use and kept. Play and Shuffle make that playlist the album (in order, or
// shuffled) and play it; + Queue adds the album to its end without stopping
// what plays; + Playlist adds it to another playlist. The rows are F's: the
// same lists in all the views.
// ══════════════════════════════════════════════════════════════════════

import { addFiles, clearList, listHooks, convLive } from '../dropzone.js';
import { state } from '../state.js';
import { getPlaylists, getActiveId, getActivePl, switchPlaylist, createPlaylist, renamePlaylist, deletePlaylist, heldEntryOf, playingPlaylist, pinVersion, playedFile } from '../library.js';
import { outputForVersion, fileOnAir, sendNow, playerStatus, seekPlayer } from '../player.js';
import { queueOrder, missingPaths } from './model.js';
import { abStart, abFix, pinOf } from './versions-core.js';

/// The library's own playlist.
export const QUEUE_NAME = 'Library';

const baseName = p => String(p).split('\\').pop().split('/').pop();
const samePath = (a, b) => String(a || '').toLowerCase() === String(b || '').toLowerCase();

/// The library's playlist's id: the one named so, else a new one.
export function queuePlaylistId() {
    const pl = getPlaylists().find(p => p.name === QUEUE_NAME);
    return pl ? pl.id : createPlaylist(QUEUE_NAME);
}

/// The library's playlist shown in F (what plays elsewhere goes on playing).
function showQueue() {
    const id = queuePlaylistId();
    if (getActiveId() !== id) switchPlaylist(id);
    return id;
}

/// Where a track added while something plays goes: the playlist that plays
/// (the listener hears it and finds the track after what plays), else the
/// one shown in the list. { id, name }.
export function addTarget() {
    const pl = playingPlaylist() || getActivePl();
    return pl ? { id: pl.id, name: pl.name } : { id: queuePlaylistId(), name: QUEUE_NAME };
}

/// That playlist shown in F (one that plays behind another comes up as it
/// is; nothing stops).
function showTarget() {
    const t = addTarget();
    if (getActiveId() !== t.id) switchPlaylist(t.id);
    return t;
}

/// Plays the row of `path` once the player holds it (a row just added gets
/// its track a moment later). False after `ms` without it.
async function playWhenReady(path, ms = 10000) {
    const end = Date.now() + ms;
    while (Date.now() < end) {
        const e = state.list.find(x => x.path === path);
        if (e?.unreadable) return false;
        if (e && e.trackId != null) { listHooks.play?.(e); return true; }
        await new Promise(r => setTimeout(r, 80));
    }
    return false;
}

/// The queue becomes `paths` (shuffled for Shuffle) and plays from the track
/// `at` of them (in the album's order; Shuffle starts at the shuffle's
/// first). A queue that is already these tracks in this order is kept as
/// it is.
export async function playPaths(paths, { shuffle = false, at = 0 } = {}) {
    if (!paths.length) return false;
    let order = queueOrder(paths, { shuffle });
    if (shuffle && at > 0) order = [paths[at], ...order.filter(p => p !== paths[at])];
    showQueue();
    const same = !shuffle && state.list.length === order.length && state.list.every((e, i) => e.path === order[i]);
    if (!same) {
        clearList();
        addFiles(missingPaths(order, state.list.map(e => e.path)));
    }
    return playWhenReady(shuffle ? order[0] : paths[at]);
}

/// A version played under its original (the Conversions page's Play): the
/// lists hold originals only, so the original `source` gets its row in the
/// playlist that plays (addTarget) — the row it has there already, else a
/// new one at the end — the output goes to the version's own kind (PCM, or
/// its DSD rate), the row is told to play this file (library.js pinVersion)
/// and is played. The file itself never gets a row and is never processed
/// again. Returns null when it plays, else why not: 'output' (this PC or
/// device has no such output), 'row' (the player did not take the
/// original), 'live' (the player chose to play the original live).
///
/// The original already on the air (another of its versions, or itself)
/// goes on from the second it is at: the A/B of two formats is one click.
export async function playVersion(v, source, ms = 10000) {
    // Where the original is, taken before the output changes.
    const s0 = playerStatus();
    const ab = abStart(s0, playingPath(s0), source);
    // The row already there takes the file before the output changes: the
    // output's change goes out with it, and the player never builds the
    // original live for the moment between them (a DSF pinned while the
    // output turned PCM played its source, 22.11).
    const had = state.list.find(x => samePath(x.path, source));
    const before = had ? had.pin : undefined;
    if (had && had.trackId != null && !had.unreadable) had.pin = pinOf(v);
    if (!outputForVersion(v.kind === 'dsd' ? Number(v.dsd) || 0 : 0)) {
        if (had) { if (before === undefined) delete had.pin; else had.pin = before; }
        return 'output';
    }
    queuePaths([source]);
    const end = Date.now() + ms;
    let e = null;
    while (Date.now() < end) {
        e = state.list.find(x => samePath(x.path, source));
        if (e?.unreadable) return 'row';
        if (e && e.trackId != null) break;
        await new Promise(r => setTimeout(r, 80));
    }
    if (!e || e.trackId == null) return 'row';
    pinVersion(e, v);
    await sendNow();
    // The same row on the air: a jump to the second it was at makes the
    // player take the other file from there (a row's ▶ would pause it).
    // Paused, it goes on playing.
    let jumped = false;
    if (ab && ab.trackId === e.trackId) {
        await seekPlayer(ab.at).catch(() => {});
        jumped = true;
        if (ab.paused) listHooks.play?.(e, { version: true });
    } else listHooks.play?.(e, { version: true });
    // The player's own word on what is on the air: the file, or — a version
    // it does not take as it is — the original, live.
    const until = Date.now() + 6000;
    let started = Date.now();
    let replayed = false;
    while (Date.now() < until) {
        await new Promise(r => setTimeout(r, 150));
        const s = playerStatus();
        const on = fileOnAir();
        // The jump found nothing on the air (the output's change stopped
        // it): the row plays, and the jump is made once it does.
        if (jumped && !replayed && Date.now() - started > 1500 && (!s || s.trackId !== e.trackId || s.state === 'stopped')) {
            replayed = true;
            jumped = false;
            listHooks.play?.(e, { version: true });
            started = Date.now();
            continue;
        }
        if (on && samePath(on, v.path)) {
            if (ab) {
                const fix = abFix(ab.at, s);
                if (fix != null) await seekPlayer(fix).catch(() => {});
            }
            return null;
        }
    }
    return 'live';
}

/// The row's ▶ for the version on the air: pause, or go on — never again
/// from the start.
export function toggleVersion(source) {
    const s = playerStatus();
    const e = (s && state.list.find(x => x.trackId === s.trackId)) || heldEntryOf(s?.trackId) || state.list.find(x => samePath(x.path, source));
    if (e) listHooks.play?.(e, { version: true });
}

/// The album at the end of the playlist that plays (addTarget); nothing that
/// plays stops. Returns how many tracks were added.
export function queuePaths(paths) {
    showTarget();
    const fresh = missingPaths(paths, state.list.map(e => e.path));
    if (fresh.length) addFiles(fresh);
    return fresh.length;
}

/// The album at the end of the playlist `id`. The playlist shown takes the
/// rows at once; another one keeps them in its saved list, so nothing in the
/// player changes. Returns how many tracks were added.
export function addToPlaylist(id, paths) {
    if (id === getActiveId()) {
        const fresh = missingPaths(paths, state.list.map(e => e.path));
        if (fresh.length) addFiles(fresh);
        return fresh.length;
    }
    const pl = getPlaylists().find(p => p.id === id);
    if (!pl) return 0;
    const fresh = missingPaths(paths, (pl.entries || []).map(e => e.path));
    for (const p of fresh) pl.entries.push({ path: p, name: baseName(p), convs: [] });
    // Kept as a rename is (the playlists' store and the dropdown's counts).
    if (fresh.length) renamePlaylist(pl.id, pl.name);
    return fresh.length;
}

/// The playlists, for the menu and the Playlists tab: id, name, how many
/// tracks, whether it is the one shown in F.
export function playlists() {
    const active = getActiveId();
    return getPlaylists().map(p => ({
        id: p.id,
        name: p.name,
        count: p.id === active ? state.list.length : (p.entries || []).length,
        active: p.id === active,
    }));
}

/// Whether the playlist `id` can be deleted now: 'ok', 'last' (the only one
/// stays), 'busy' (a conversion of its rows runs or waits).
export function canRemovePlaylist(id) {
    const all = getPlaylists();
    if (all.length <= 1) return 'last';
    if (id === getActiveId() && state.list.some(e => convLive(e.conv))) return 'busy';
    return all.some(p => p.id === id) ? 'ok' : 'last';
}

/// The playlist `id` deleted (its files stay on disk; what plays from it
/// stops). False when it could not be.
export function removePlaylist(id) {
    return canRemovePlaylist(id) === 'ok' && deletePlaylist(id);
}

/// A playlist's tracks: the rows shown when it is F's, its saved list
/// otherwise. Each { path, name }.
export function playlistTracks(id) {
    if (id === getActiveId()) return state.list.map(e => ({ path: e.path, name: e.name }));
    const p = getPlaylists().find(x => x.id === id);
    return (p?.entries || []).map(e => ({ path: e.path, name: e.name }));
}

/// The playlist `id` shown in F, then its track `index` (or its first)
/// played.
export async function playPlaylist(id, index = 0) {
    if (getActiveId() !== id) switchPlaylist(id);
    const e = state.list[index] || state.list[0];
    if (!e) return false;
    return playWhenReady(e.path);
}

/// The playlist `id` shown in F.
export function showPlaylist(id) {
    if (getActiveId() !== id) switchPlaylist(id);
}

/// The converted file that is heard now (the version the playing row plays
/// from disk), or null: nothing plays, or the row plays live.
export function playingVersion(s) {
    if (!s || s.trackId == null || !s.state || s.state === 'stopped') return null;
    const e = state.list.find(x => x.trackId === s.trackId) || heldEntryOf(s.trackId);
    return e ? (fileOnAir() || playedFile(e)) : null;
}

/// The path of what plays, from the player's status, or null.
export function playingPath(s) {
    if (!s || s.trackId == null || !s.state || s.state === 'stopped') return null;
    const e = state.list.find(x => x.trackId === s.trackId) || heldEntryOf(s.trackId);
    return e?.path ?? null;
}
