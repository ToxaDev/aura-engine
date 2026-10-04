
import { state } from './state.js';
import { startConversion } from './converter.js';
import { chainBadgesHtml, DSP_CHAIN } from './dsprack.js';
import { rackSig, entrySig, initLibrary, pushConvRecord, refreshDropdown, getSavedConvsForPath } from './library.js';
import { ICON_CONVERT, ICON_PLAY_SM, ICON_M } from './icons.js';
import {
    mKey, isMActive, getMSettings, toggleMEntry, setM, getM,
    snapToConvertOverride,
} from './memory.js';
import { batchReset, speedTag } from './batchstats.js';
import { convertAllView } from './convertall.js';
import { initListDrag } from './list-drag.js';
import { cameThrough, zoneClickBrowses } from './zone-click.js';
import { rowsAt, gapSlot, edgeAt, edgePacer, maxScroll, scrollAfterDrop, EDGE_BAND } from './list-geom.js';

const { invoke } = window.__TAURI__.tauri;

// ══════════════════════════════════════════════════════════════════════
// The list under the rack.
//
// One list for everything: a file dropped here is listened to, converted,
// or both, from its own row — ▶ plays it through the chain the rack shows,
// C writes it with that chain. Dropping only adds; nothing starts on its
// own, because a list that converts on drop cannot be used to listen.
//
// A row is an entry of `state.list`. Conversion keeps its own model — the
// batch the backend is running (`convFileQueue`, indexed like the backend's
// queue) and what waits for the next one (`convNextQueue`) — and each row is
// linked to the record that converts it, and stays linked after the batch
// ends, so the row keeps saying how it went.
//
// A finished conversion record lives on entry.convs. When a row's convs
// contain a record whose sig matches rackSig() (the current rack), the
// converted file is on disk: the row shows ✓ and the C button is gone —
// ▶ plays the file from disk with no processing.
//
// The player is not imported here: player.js fills in `listHooks`, so this
// file works (as a converter list) even if the player failed to start.
// ══════════════════════════════════════════════════════════════════════

const AUDIO_EXT = ['wav', 'flac', 'mp3', 'ogg', 'aac', 'm4a'];
/// The audio files among dropped paths (anything else the list does not take).
const audioPaths = paths => (paths || []).filter(f => AUDIO_EXT.includes(String(f).split('.').pop().toLowerCase()));

export const listHooks = {
    /** entries were added to the list */
    added: null,
    /** an entry left the list */
    removed: null,
    /** the list is in a new order (a row was dragged): the whole list */
    moved: null,
    /** play this entry (or the first one, when null) */
    play: null,
    /** { id, state } of what the player holds right now */
    nowPlaying: () => ({ id: null, state: 'stopped' }),
    /** Array<{id, path|null}> — rendered map for player_set_settings */
    rendered: null,
    /** called when the rendered map changes */
    renderedChanged: null,
    /** M button toggled on this entry */
    memoryToggle: null,
};

const baseName = p => p.split('\\').pop().split('/').pop();
const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

export function syncEmptyState() {
    document.getElementById('dzEmpty')?.classList.toggle('hidden', state.list.length > 0);
    updateListHead();
}
window._dzSyncEmpty = syncEmptyState;

// ── conversion records ───────────────────────────────────────────────

const inQueues = c => !!c && (state.convFileQueue.includes(c) || state.convNextQueue.includes(c));

/** Is this record still going to change — waiting, or being converted? */
export function convLive(c) {
    if (!inQueues(c) || c.dismissed) return false;
    return c.batch === 'next' || c.status === 'pending' || c.status === 'active';
}

/// Tie every conversion record that has no row yet to the row of its file.
/// A batch is started from paths, so its records arrive without a link; the
/// row they belong to is the one with that path whose own record is no
/// longer live (a record of the next queue is not live once that queue has
/// become the batch).
function linkConversions() {
    for (const f of [...state.convFileQueue, ...state.convNextQueue]) {
        if (f.listId != null) continue;
        const e = state.list.find(x => x.path === f.path && x.conv !== f && !convLive(x.conv));
        if (e) { e.conv = f; f.listId = e.id; }
        // Capture the rack and signature in the same tick startConversion read them.
        // For M batches, startConversion pre-computed f.mSig from the override so
        // the rack DOM (intentionally unchanged for M3) is not captured as the sig.
        if (f.batch !== 'next' && !f.rack) {
            f.rack = DSP_CHAIN.filter(id => !!document.getElementById(id)?.checked);
            f.sig = f.mSig ?? rackSig();
        }
    }
}

/// When a conversion record becomes done with an output path, push it into
/// the entry's convs list (via library.js) so it survives a restart.
/// _convPushed is set only after a successful push so that a record which
/// completes while the playlist is being switched can be retried on the next
/// render cycle (switchPlaylist calls _render before replacing state.list).
function processCompletedConversions() {
    for (const f of state.convFileQueue) {
        if (f.status === 'done' && f.outPath && f.listId != null && !f._convPushed) {
            const entry = state.list.find(e => e.id === f.listId);
            if (entry) {
                f._convPushed = true;
                pushConvRecord(entry, f);
            }
        }
    }
}

// ── adding, converting, removing ─────────────────────────────────────

/// Add audio files to the list. Only for user-initiated file additions (drops,
/// file-picker). The restore and playlist-switch paths go through library.js's
/// own _addFiles wrapper, not here.
export function addFiles(paths, at = null) {
    const files = audioPaths(paths);
    if (files.length === 0) return [];
    const entries = files.map(p => ({
        id: state.listSeq++, path: p, name: baseName(p),
        trackId: null, info: null, conv: null,
        // Carry over any saved conversion records for this path so the row
        // shows ✓ immediately when the file was already converted before.
        convs: getSavedConvsForPath(p),
        unreadable: null,
        fresh: true, // cleared after the highlight animation
    }));
    // Dropped over the list: where the pointer was (the player's queue takes
    // the list's order once the files are in it, player.js onAdded).
    if (at != null && at >= 0 && at < state.list.length) state.list.splice(at, 0, ...entries);
    else state.list.push(...entries);
    renderFileQueue();
    listHooks.added?.(entries);
    // Dropped where the pointer put them: they fill the gap they were dropped
    // into and the rows above it stay, so the view stays too (Anton 1.10: the
    // file is where it was dropped). Added any other way: at the end, shown.
    if (at != null) revolverHold(state.list.indexOf(entries[0]), entries.length);
    else scrollToNewEntries(entries);
    return entries;
}

/// Convert these rows with what the rack says now. While a batch runs they
/// wait for the next one, as files dropped mid-batch always have.
let starting = false;

export async function convertEntries(entries) {
    const todo = entries.filter(e => !convLive(e.conv));
    if (todo.length === 0) return;
    if (state.convIsConverting || starting) {
        for (const e of todo) {
            const f = { path: e.path, name: e.name, status: 'queued', batch: 'next', listId: e.id };
            state.convNextQueue.push(f);
            e.conv = f;
        }
        renderFileQueue();
        return;
    }
    batchReset();
    starting = true;
    try {
        await startConversion(todo.map(e => e.path));
    } finally {
        starting = false;
    }
    if (!state.convIsConverting && state.convNextQueue.length) state.convNextQueue = [];
    renderFileQueue();
}

/// Convert one entry using its M settings (or the current rack if no M
/// settings). Never touches the rack DOM (M3 rule).
async function convertEntryM(entry) {
    const snap = getMSettings(entry);
    const mOverride = snap ? snapToConvertOverride(snap) : null;
    const todo = [entry].filter(e => !convLive(e.conv));
    if (todo.length === 0) return;
    if (state.convIsConverting || starting) {
        // Already running — queue for next batch WITH this track's own M
        // settings (mOverride carried on the record so resetConverterUI can
        // group the next-queue by sig and pass the correct override).
        for (const e of todo) {
            const f = { path: e.path, name: e.name, status: 'queued', batch: 'next', listId: e.id, mOverride };
            state.convNextQueue.push(f);
            e.conv = f;
        }
        renderFileQueue();
        return;
    }
    batchReset();
    starting = true;
    try {
        await startConversion(todo.map(e => e.path), mOverride);
    } finally {
        starting = false;
    }
    if (!state.convIsConverting && state.convNextQueue.length) state.convNextQueue = [];
    renderFileQueue();
}

/// Convert all entries, grouping M tracks by their settings signature so each
/// group uses its own remembered settings (M3). The rack DOM never changes.
async function convertAllM(todo) {
    if (todo.length === 0) return;
    batchReset();

    // Partition into non-M entries and M groups (keyed by settings sig).
    const nonM = [];
    const mGroups = new Map();   // sig → { entries: [], snap: snapshot }

    for (const e of todo) {
        if (!isMActive(e)) {
            nonM.push(e);
            continue;
        }
        const snap = getMSettings(e);
        if (!snap) { nonM.push(e); continue; }
        const sig = JSON.stringify(snap);
        if (!mGroups.has(sig)) mGroups.set(sig, { entries: [], snap });
        mGroups.get(sig).entries.push(e);
    }

    // Build the execution list: non-M first, then each M group.
    const groups = [];
    if (nonM.length > 0) groups.push({ entries: nonM, mOverride: null });
    for (const { entries, snap } of mGroups.values()) {
        groups.push({ entries, mOverride: snapToConvertOverride(snap) });
    }

    if (groups.length === 0) return;

    // The first group starts immediately; remaining groups feed convMPendingGroups
    // so resetConverterUI runs them sequentially after the batch ends.
    const liveTodo = groups[0].entries.filter(e => !convLive(e.conv));
    if (liveTodo.length === 0 && groups.length === 1) return;

    // Queue remaining groups before starting the first so they are ready when
    // the batch's resetConverterUI fires.
    for (let i = 1; i < groups.length; i++) {
        const g = groups[i];
        const livePaths = g.entries.filter(e => !convLive(e.conv)).map(e => e.path);
        if (livePaths.length > 0) {
            state.convMPendingGroups.push({ paths: livePaths, mOverride: g.mOverride });
        }
    }

    if (liveTodo.length === 0) return;

    if (state.convIsConverting || starting) {
        // A batch is already running — queue the first group as next.
        // Store the group's mOverride on each record so resetConverterUI can
        // pass it to startConversion (Fix: was previously lost, causing M tracks
        // in the first group to convert with rack settings instead of M settings).
        for (const e of liveTodo) {
            const f = {
                path: e.path, name: e.name, status: 'queued', batch: 'next',
                listId: e.id, mOverride: groups[0].mOverride,
            };
            state.convNextQueue.push(f);
            e.conv = f;
        }
        renderFileQueue();
        return;
    }
    starting = true;
    try {
        await startConversion(liveTodo.map(e => e.path), groups[0].mOverride);
    } finally {
        starting = false;
    }
    if (!state.convIsConverting && state.convNextQueue.length) state.convNextQueue = [];
    renderFileQueue();
}

/// Take a row off the list. A conversion it is part of is cancelled.
export async function removeEntry(id) {
    const i = state.list.findIndex(e => e.id === id);
    if (i < 0) return;
    const e = state.list[i];
    const c = e.conv;
    if (convLive(c)) {
        if (c.batch === 'next') {
            state.convNextQueue.splice(state.convNextQueue.indexOf(c), 1);
        } else {
            c.dismissed = true;
            try { await invoke('cancel_file', { idx: state.convFileQueue.indexOf(c) }); } catch (_) {}
        }
    }
    state.list.splice(i, 1);
    listHooks.removed?.(e);
    renderFileQueue();
}

/// A row dragged to `toIndex`: the list in its new order, the player's queue
/// and the playlist after it.
function moveEntry(id, toIndex) {
    const from = state.list.findIndex(e => e.id === id);
    if (from < 0) return;
    const to = Math.max(0, Math.min(state.list.length - 1, toIndex));
    if (to === from) return;
    const [e] = state.list.splice(from, 1);
    state.list.splice(to, 0, e);
    renderFileQueue();
    listHooks.moved?.(state.list);
}

/// Empty the list, except rows whose conversion is still running or waiting.
export function clearList() {
    const gone = state.list.filter(e => !convLive(e.conv));
    state.list = state.list.filter(e => convLive(e.conv));
    gone.forEach(e => listHooks.removed?.(e));
    // Pending M groups refer to entries that are now gone; clear them so
    // resetConverterUI does not start a batch for a list the user already cleared.
    state.convMPendingGroups = [];
    renderFileQueue();
}

/// The drop gap's own slot at the list's end while files from outside hover
/// (dropGap): the list scrolls that one slot further.
let dropTail = 0;

/// The gap in the list for files dragged in from outside. While they hover,
/// the rows part in the slot under the pointer — asked of the backend once a
/// frame: the page sees no pointer events then — and at the list's top or
/// bottom edge the list scrolls (list-geom.js), with one slot more at its end
/// so the gap can open under the last row (Anton 1.10). `stop(true)` returns
/// the row index the drop goes to (null: not over the list) and leaves the
/// rows parted for the new rows to fill; `stop(false)` closes the gap;
/// `finish()` puts back whatever is left once the files are in.
function dropGap() {
    const list = () => document.getElementById('fileQueue');
    const ASK_EVERY_MS = 30;       // the pointer asked this often; the gap is placed every frame
    const POINTER_GRACE_MS = 400;  // a drop or a cancel on its way comes within this
    let on = false, raf = 0, asking = null, askedAt = 0, pt = null, at = null;
    let pace = null, geo = null, tail = null, graceTimer = 0;

    // The pointer in the page (__auraPinCursor: a point the rw checks pin —
    // they cannot drag from Explorer, nor move the real pointer). One question
    // at a time; the next one goes once the answer is in.
    const ask = () => {
        if (!asking) {
            asking = (async () => {
                try { pt = window.__auraPinCursor || await invoke('ui_cursor_client'); }
                catch (_) { pt = null; }   // an older backend: the drop goes to the end
            })().finally(() => { asking = null; });
        }
        return asking;
    };

    /// Over the list, or over its head just above it or the window's edge under
    /// it, where a hand going for the top or the end overshoots: there the gap
    /// keeps to the view's edge. Anywhere else the drop goes to the end — and
    /// so under whatever lies over the list (the full screen view, a dialog):
    /// what is under the pointer must be the list, its head or the bare panel.
    const over = ([x, y]) => {
        const zone = document.getElementById('dropZone');
        const head = document.getElementById('plListHead');
        const zr = zone?.getBoundingClientRect();
        if (!zr || x < zr.left || x > zr.right) return false;
        const top = Math.min(zr.top, head?.getBoundingClientRect().top ?? zr.top);
        if (y < top || y > Math.max(zr.bottom, window.innerHeight)) return false;
        const hit = document.elementFromPoint(x, Math.min(y, window.innerHeight - 1));
        return !!hit && (zone.contains(hit) || !!head?.contains(hit) || hit === zone.parentElement
            || hit === document.body || hit === document.documentElement);
    };

    /// The gap's slot for the pointer now (for the list scrolled to `top`);
    /// with `scroll`, a row further when the pointer waits at an edge.
    const target = (el, now, scroll, top = el.scrollTop) => {
        if (!pt || !over(pt)) { pace(now, 0); return null; }
        const h = el.clientHeight;
        const y = pt[1] - el.getBoundingClientRect().top - el.clientTop;
        if (scroll) {
            const edge = edgeAt(y, h, EDGE_BAND * geo.step);
            const dir = pace(now, edge.dir, edge.depth);
            if (dir) revolverStep(el, dir, dropTail);
        }
        // Over the head or under the list: the view's first or last slot.
        const inView = Math.max(geo.pad, Math.min(h - geo.pad - 1, y));
        return gapSlot(rowsAt(inView, top, geo), at, el.querySelectorAll('.pl-item').length);
    };

    /// The rows from the gap on a slot down.
    const paint = (el) => {
        el.querySelectorAll('.pl-item').forEach((row, i) => {
            row.classList.add('pl-drag-shift');
            const t = at != null && i >= at ? `translateY(${geo.step}px)` : '';
            if (row.style.transform !== t) row.style.transform = t;
        });
    };

    const frame = (now) => {
        if (!on) return;
        if (now - askedAt >= ASK_EVERY_MS) { askedAt = now; ask(); }
        const el = list();
        if (el) {
            const idx = target(el, now, true);
            if (idx !== at) { at = idx; paint(el); }
        }
        raf = requestAnimationFrame(frame);
    };

    const start = () => {
        const el = list();
        if (on || !el) return;
        on = true; at = null; pt = null;
        pace = edgePacer(); geo = revolverGeo();
        document.getElementById('dropZone')?.classList.add('drag-over');
        // Rows are not rebuilt under the files (renderFileQueue waits).
        state.listDragging = true;
        revolverSync(el);
        // No snapping while rows move: it would follow the first row down
        // and turn the list under the pointer (the jumping at the top).
        el.classList.add('pl-drag-active');
        tail?.remove();
        tail = document.createElement('div');
        tail.className = 'pl-drop-tail';
        el.appendChild(tail);
        dropTail = 1;
        raf = requestAnimationFrame(frame);
    };

    const stop = async (dropping) => {
        clearTimeout(graceTimer);
        graceTimer = 0;
        if (!on) return null;
        on = false;
        cancelAnimationFrame(raf);
        document.getElementById('dropZone')?.classList.remove('drag-over');
        state.listDragging = false;
        const el = list();
        if (el && dropping) {
            // The list stops on the row nearest to what is seen (a fast edge
            // scroll runs a row or two ahead of it) and the gap is placed for
            // that view: the files land where they were let go.
            const top = revolverStop(el);
            dropTail = 0;
            await ask();
            const idx = target(el, performance.now(), false, top);
            if (idx !== at) { at = idx; paint(el); }
            return at;
        }
        dropTail = 0;
        at = null;
        if (el) paint(el);
        return null;
    };

    const finish = () => {
        const el = list();
        at = null;
        if (!el) return;
        // Rows no render has replaced (nothing was added, or the drop was
        // cancelled) glide back.
        el.querySelectorAll('.pl-item').forEach(row => { if (row.style.transform) row.style.transform = ''; });
        setTimeout(() => el.querySelectorAll('.pl-item').forEach(row => row.classList.remove('pl-drag-shift')), 200);
        if (state.listRenderPending) { state.listRenderPending = false; renderFileQueue(); }
        // Back within the list's own end, then the extra slot goes and the
        // snapping comes back — on a whole row, so it has nothing to move.
        revolverClamp(el);
        revolverIdle().then(() => {
            // Files over the list again, or a row taken since: theirs now.
            if (on || state.listDragging) return;
            tail?.remove();
            tail = null;
            el.classList.remove('pl-drag-active');
        });
    };

    // The page hears no pointer while files hover, so a real one — a press,
    // or a move that moves (not the still one the browser makes up after a
    // scroll) — means the system drag is over. A drop or a cancel on its way
    // still comes first; if none does (a drag the backend never closed), the
    // gap closes and the list is itself again.
    const sawPointer = (e) => {
        if (!on || graceTimer) return;
        if (e.type === 'pointermove' && !e.movementX && !e.movementY) return;
        graceTimer = setTimeout(() => {
            graceTimer = 0;
            if (on) stop(false).then(finish);
        }, POINTER_GRACE_MS);
    };
    window.addEventListener('pointermove', sawPointer, true);
    window.addEventListener('pointerdown', sawPointer, true);

    return { start, stop, finish };
}

// ── rendering ─────────────────────────────────────────────────────────

function fmtTime(s) {
    if (!isFinite(s) || s <= 0) return '';
    const m = Math.floor(s / 60);
    return `${m}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
}

/// What a row shows, from its conversion record, disk records, and the player.
function rowProps(e, now) {
    const f = e.conv;
    let icon = '○', cls = '', title = 'Not converted', badgeHtml = '', pct = 0;
    let convertable = true;
    // How fast the conversion the ✓ stands for went, when it is known.
    let speedX = null;

    // Check if a conversion matching this row's settings exists on disk: the
    // track's own (M on) or the rack's.
    const mem = isMActive(e);
    const sig = entrySig(e);
    const diskMatch = (e.convs || []).find(c => c.sig === sig);
    const latestConv = (e.convs || []).length > 0 ? e.convs[e.convs.length - 1] : null;
    const isDisk = !!diskMatch;

    if (isDisk) {
        // Already converted with these settings — play from disk, no C button.
        icon = '✓'; cls = 'done';
        title = (mem ? 'Converted with this track’s own settings' : 'Converted with the current rack') + ' · plays from disk';
        // Pass status:'done' so chainBadgesHtml shows the VERIFIED badge as lit.
        badgeHtml = chainBadgesHtml({ ...diskMatch, status: 'done' });
        convertable = false;
        speedX = diskMatch.speedX;
    } else if (f) {
        if (f.dismissed) {
            icon = f.status === 'active' ? '⏳' : '–';
            cls = f.status === 'active' ? 'dismissing' : 'cancelled';
            title = 'Cancelled';
        } else if (f.batch === 'next' && inQueues(f)) {
            icon = '⏳'; cls = 'queued'; title = 'Waiting for the next batch'; convertable = false;
        } else if (f.status === 'done') {
            icon = '✓'; cls = 'done'; title = 'Converted'; speedX = f.speedX;
        } else if (f.status === 'cancelled') {
            icon = '–'; cls = 'cancelled'; title = 'Cancelled';
        } else if (f.status === 'active') {
            icon = '▶'; cls = 'active'; title = 'Converting'; convertable = false;
        } else if (f.status === 'error') {
            icon = '✗'; cls = 'error';
            title = f.errorMsg || 'Error during conversion';
        } else if (f.status === 'pending') {
            icon = '⏳'; cls = 'queued'; title = 'In this batch, waiting its turn'; convertable = false;
        }
        if (f.badge === 1) { icon = '✗'; cls = 'error'; }
        else if (f.badge === 2) { icon = '–'; cls = 'cancelled'; }
        else if (f.badge === 3) { icon = '⚠'; }

        badgeHtml = f.batch === 'next' && inQueues(f)
            ? '<span class="cb" data-lab-why="Queued for the next batch — nothing has run on it yet." '
              + 'data-lab-tok="NEXT" data-lab-name="Queued">&rarr; next</span>'
            : chainBadgesHtml(f);

        // If there are older disk records that don't match the rack, show them
        // dimmed with a tooltip explaining they used different settings.
        if (!isDisk && latestConv) {
            const dimBadges = chainBadgesHtml({ ...latestConv, status: 'done' });
            if (dimBadges && !badgeHtml) {
                badgeHtml = `<span class="pl-old-conv" title="Converted with different settings — re-convert to match the current rack">${dimBadges}</span>`;
            }
        }

        pct = f.filePct != null ? f.filePct
            : (cls === 'active' ? state.convCurrentFilePct : (cls === 'done' ? 100 : 0));
    } else if (latestConv) {
        // Has a disk record but not matching the current rack: show C, dim old badges.
        const dimBadges = chainBadgesHtml({ ...latestConv, status: 'done' });
        if (dimBadges) {
            badgeHtml = `<span class="pl-old-conv" title="Converted with different settings — C to re-convert with the current rack">${dimBadges}</span>`;
        }
    }

    const readable = !e.unreadable;
    if (!readable && !f) {
        icon = '✗'; cls = 'error'; title = e.unreadable; convertable = false;
    }
    // "×N" beside the ✓; it stays while the row plays (♪ in the ✓'s place).
    const speed = icon === '✓' ? speedTag(speedX) : null;
    const playing = now.id != null && now.id === e.trackId && now.state !== 'stopped';
    const preparing = playing && now.state === 'preparing';
    if (playing) {
        icon = now.state === 'paused' ? '❚❚' : '♪';
        title = (now.state === 'paused' ? 'Paused' : preparing ? 'Getting ready to play' : 'Playing') + (f ? ' · ' + title : '');
        if (preparing) cls += ' pl-prep';
    }
    const i = e.info;
    const meta = i ? [fmtTime(i.durationS), i.sampleRate ? (i.sampleRate / 1000) + (i.bits ? '/' + i.bits : '') : '']
        .filter(Boolean).join(' · ') : '';
    // The row's ▶ is ⏸ while it plays (or gets ready to), ▶ when paused.
    const pausable = playing && now.state !== 'paused';
    // A track pressed is getting ready: every row's ▶ waits for it.
    const locked = !!now.locked;
    return { icon, cls, title, badgeHtml, pct, playing, pausable, locked, convertable, readable, meta, isDisk, speed };
}

// Inline SVG play triangle, its box 0.4 right of the svg's centre (2.4–8.4 of
// 10) so it reads as centred: measured on the painted pixels, a full unit
// put the triangle ~0.9 px right of the button's centre. 10×9 inside the
// button's 18×15 leaves whole-pixel margins.
const playSvg = `<svg width="10" height="9" viewBox="0 0 10 9" aria-hidden="true"><polygon points="2.4,0.5 8.4,4.5 2.4,8.5" fill="currentColor"/></svg>`;
// The same box: two bars.
const pauseSvg = `<svg width="10" height="9" viewBox="0 0 10 9" aria-hidden="true"><rect x="2" y="0.5" width="2.3" height="8" rx="0.4" fill="currentColor"/><rect x="5.7" y="0.5" width="2.3" height="8" rx="0.4" fill="currentColor"/></svg>`;

function rowHtml(e, now) {
    const p = rowProps(e, now);
    // When a conversion matches the current rack, the C button is gone and ▶
    // moves into its slot (styled green like the convert button).
    const icon = p.pausable ? pauseSvg : playSvg;
    const playTitle = p.locked ? 'Wait — the track pressed is getting ready'
        : p.pausable ? 'Pause' : p.playing ? 'Resume'
        : p.isDisk ? 'Play the converted file from disk' : 'Listen';
    const wait = p.locked ? ' pl-wait' : '';
    const playBtn = p.readable
        ? (p.isDisk
            ? `<button class="pl-btn pl-disk-play${wait}" data-act="play" title="${playTitle}" tabindex="-1">${icon}</button>`
            : `<button class="pl-btn pl-play${wait}" data-act="play" title="${playTitle}" tabindex="-1">${icon}</button>`)
        : '';
    const convBtn = !p.isDisk && p.convertable && p.readable
        ? `<button class="pl-btn pl-conv" data-act="convert" title="Convert with the rack as it is now" tabindex="-1">${ICON_CONVERT}</button>`
        : '';
    // M belongs to the track, not to its conversion state: shown on every
    // readable row, converted or not, so it can always be switched off.
    const mOn = p.readable && isMActive(e);
    const memBtn = p.readable
        ? `<button class="pl-btn pl-mem${mOn ? ' pl-mem-on' : ''}" data-act="memory"` +
          ` title="Track memory: ${mOn ? 'on — rack changes go into this track' : 'off — click to remember DSP settings for this track'}"` +
          ` tabindex="-1">${ICON_M}</button>`
        : '';
    return `<div class="file-item pl-item ${p.cls}${p.playing ? ' pl-now' : ''}${e.fresh ? ' pl-fresh' : ''}" data-id="${e.id}">
        <div class="file-item-progress" style="width:${p.pct.toFixed(1)}%"></div>
        <div class="fi-head">
            <span class="file-item-icon" title="${esc(p.title)}">${p.icon}</span>
            ${p.speed ? `<span class="pl-speed" title="${esc(p.speed.title)}">${esc(p.speed.text)}</span>` : ''}
            <span class="file-item-name" title="${esc(e.path)}">${esc(e.name)}</span>
            <span class="pl-meta">${esc(p.meta)}</span>
            ${playBtn}
            ${convBtn}
            ${memBtn}
            <button class="file-item-close" data-act="remove" title="Remove${e.conv && convLive(e.conv) ? ' (cancels its conversion)' : ''}" tabindex="-1">
                <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>
            </button>
        </div>
        <div class="badge-container">${p.badgeHtml}</div>
    </div>`;
}

/// Redraw the list. `changedIdxs` (from the conversion poll) are indices into
/// the backend batch; only the rows linked to them are redrawn then, so a
/// long queue polled several times a second touches a handful of nodes.
export function renderFileQueue(changedIdxs = null) {
    if (state.listDragging) { state.listRenderPending = true; return; }
    linkConversions();
    processCompletedConversions();
    const container = document.getElementById('fileQueue');
    if (!container) return;
    const now = listHooks.nowPlaying();
    const rows = container.querySelectorAll('.pl-item');
    // The same rows in the same order: each is brought up to date where it
    // stands. Only an added, removed or moved row rebuilds the list — a
    // rebuild under a pressed button loses its click.
    const sameRows = rows.length === state.list.length && state.list.length > 0
        && state.list.every((e, i) => rows[i].dataset.id === String(e.id));
    if (changedIdxs && sameRows) {
        const ids = new Set();
        changedIdxs.forEach(i => { const f = state.convFileQueue[i]; if (f && f.listId != null) ids.add(f.listId); });
        refreshRows(ids);
    } else if (sameRows) {
        refreshRows(new Set(state.list.map(e => e.id)));
    } else {
        container.innerHTML = state.list.map(e => rowHtml(e, now)).join('');
        // Wire fresh-highlight removal: once the animation ends, strip the class.
        container.querySelectorAll('.pl-fresh').forEach(el => {
            el.addEventListener('animationend', () => {
                el.classList.remove('pl-fresh');
                const id = parseInt(el.dataset.id);
                const entry = state.list.find(x => x.id === id);
                if (entry) entry.fresh = false;
            }, { once: true });
        });
        // Rows are new nodes: whoever decorates them (the analyser's button)
        // hears about each one and about the whole list.
        const ids = [];
        container.querySelectorAll('.pl-item').forEach(el => { ids.push(parseInt(el.dataset.id)); announceRow(el); });
        container.dispatchEvent(new CustomEvent('aura:list-rendered', { detail: { ids } }));
    }
    syncEmptyState();
}

/// A row that is a new node in the list: `aura:row-rendered` on it, bubbling.
function announceRow(el) {
    el.dispatchEvent(new CustomEvent('aura:row-rendered', { bubbles: true, detail: { id: parseInt(el.dataset.id) } }));
}

// A title the tooltip system has already moved to data-tip (tooltip.js).
const tipOf = n => n.getAttribute('title') ?? n.dataset.tip ?? '';
function setTip(n, v) { if (tipOf(n) !== v) n.title = v; }

/// Bring a row up to date where it stands: classes, texts, tooltips, the
/// progress bar, the badges. False when its buttons are not the same set
/// any more, or its speed came or went — then it has to be replaced.
function patchRow(el, fresh) {
    const acts = n => [...n.querySelectorAll('[data-act]')];
    const a = acts(el), b = acts(fresh);
    if (a.length !== b.length || a.some((x, i) => x.dataset.act !== b[i].dataset.act)) return false;
    if (!el.querySelector('.pl-speed') !== !fresh.querySelector('.pl-speed')) return false;
    if (el.className !== fresh.className) el.className = fresh.className;
    a.forEach((x, i) => {
        if (x.className !== b[i].className) x.className = b[i].className;
        setTip(x, b[i].getAttribute('title') || '');
        if (x.innerHTML !== b[i].innerHTML) x.innerHTML = b[i].innerHTML;
    });
    const pair = sel => [el.querySelector(sel), fresh.querySelector(sel)];
    const [pg, pg2] = pair('.file-item-progress');
    if (pg && pg2 && pg.style.width !== pg2.style.width) pg.style.width = pg2.style.width;
    for (const sel of ['.file-item-icon', '.pl-speed', '.file-item-name', '.pl-meta']) {
        const [x, y] = pair(sel);
        if (!x || !y) continue;
        if (x.textContent !== y.textContent) x.textContent = y.textContent;
        if (y.hasAttribute('title')) setTip(x, y.getAttribute('title'));
    }
    const [bc, bc2] = pair('.badge-container');
    if (bc && bc2 && bc._key !== bc2.innerHTML) { bc._key = bc2.innerHTML; bc.innerHTML = bc2.innerHTML; }
    return true;
}

/** Redraw just these rows (by entry id). */
export function refreshRows(ids) {
    if (state.listDragging) { state.listRenderPending = true; return; }
    const container = document.getElementById('fileQueue');
    if (!container || ids.size === 0) return;
    const now = listHooks.nowPlaying();
    for (const id of ids) {
        const el = container.querySelector(`.pl-item[data-id="${id}"]`);
        const e = state.list.find(x => x.id === id);
        if (!el || !e) continue;
        const tmp = document.createElement('div');
        tmp.innerHTML = rowHtml(e, now);
        const newEl = tmp.firstElementChild;
        if (patchRow(el, newEl)) continue;
        el.replaceWith(newEl);
        announceRow(newEl);
        // Wire the fresh-highlight removal on the new element. The old element
        // is detached before animationend fires on it, so e.fresh would stay
        // true forever without this, restarting the animation every poll cycle.
        if (e.fresh) {
            newEl.addEventListener('animationend', () => {
                e.fresh = false;
                newEl.classList.remove('pl-fresh');
            }, { once: true });
        }
    }
    updateListHead();
}

// ── the list's own header: playlist, play, convert all, clear ─────────

/// How many entries need converting with the current rack?
function unconvertedCount() {
    const sig = rackSig();
    return state.list.filter(e =>
        !e.unreadable &&
        !convLive(e.conv) &&
        !(e.convs || []).some(c => c.sig === sig)
    ).length;
}

function updateListHead() {
    const ctrl = document.getElementById('plPlaylistCtrl');
    if (ctrl) refreshDropdown();

    const n = state.list.length;
    const uc = unconvertedCount();

    // Clear the post-batch result when the list has something new to convert
    // (a new file, or a rack change that made rows stale) or when it is empty.
    if (state.convAllResult && (uc > 0 || n === 0)) {
        state.convAllResult = null;
    }

    const btn = document.getElementById('plConvertAll');
    if (btn) {
        // Converting (progress, hold to cancel), a result (no click, its tip
        // shows), or idle — convertall.js.
        const v = convertAllView({ converting: state.convIsConverting, pct: state.convOverallPct,
            result: state.convAllResult, left: uc });
        const fill = document.getElementById('plCaProgress');
        const holdFill = document.getElementById('plCaHold');
        const text = document.getElementById('plCaText');
        btn.disabled = v.disabled;
        if (v.ariaDisabled) btn.setAttribute('aria-disabled', 'true');
        else btn.removeAttribute('aria-disabled');
        btn.classList.toggle('pl-ca-busy', v.busy);
        btn.classList.toggle('pl-ca-done', v.done);
        btn.title = v.title;
        if (fill) fill.style.width = v.fill.toFixed(1) + '%';
        if (!v.busy && holdFill) holdFill.style.width = '0%';
        if (text) text.textContent = v.text;
    }

    const play = document.getElementById('plPlayAll');
    if (play) play.disabled = n === 0;
    const clear = document.getElementById('plClear');
    if (clear) clear.disabled = n === 0;
}

function buildListHead(dropZone) {
    if (document.getElementById('plListHead')) return;
    const head = document.createElement('div');
    head.className = 'pl-list-head';
    head.id = 'plListHead';
    // The playlist control replaces the old "N files · M converted" count.
    // library.js fills it in once the playlists are restored.
    // #plConvertAll has inner fill + text so it can show a progress bar.
    head.innerHTML = `<span class="pl-playlist-ctrl" id="plPlaylistCtrl"><span class="pl-dd-trigger"><span class="pl-dd-name">Playlist 1</span><span class="pl-dd-count">0</span></span></span>
        <button type="button" class="pl-head-btn" id="plPlayAll" title="Play the list from the top">${ICON_PLAY_SM} Play</button>
        <button type="button" class="pl-head-btn" id="plConvertAll" title="Convert every file not converted yet, with the rack as it is now"><div class="pl-ca-progress" id="plCaProgress"></div><div class="pl-ca-hold" id="plCaHold"></div><span class="pl-ca-text" id="plCaText">Convert all</span></button>
        <button type="button" class="pl-head-btn pl-head-clear" id="plClear" title="Empty the list (running conversions stay)">Clear</button>`;
    dropZone.parentNode.insertBefore(head, dropZone);
    head.querySelector('#plPlayAll').addEventListener('click', () => listHooks.play?.(null));

    const caBtn = head.querySelector('#plConvertAll');
    caBtn.addEventListener('click', () => {
        // While a batch is running the button is a hold-to-cancel; clicks do nothing.
        if (state.convIsConverting) return;
        // While it shows a result it takes no click (aria-disabled, not
        // disabled, so the result's tip still shows).
        if (state.convAllResult) return;
        // Idle: convert every row not done with the current rack.
        const sig = rackSig();
        const todo = state.list.filter(e =>
            !e.unreadable &&
            !convLive(e.conv) &&
            !(e.convs || []).some(c => c.sig === sig)
        );
        convertAllM(todo);
    });

    // ── Hold to cancel the whole batch on the Convert all button ──────────
    // 2000 ms hold (same timing as the separate Hold to Cancel strip).
    let caHoldStart = null, caRafId = null;

    function caStartHold(e) {
        if (!state.convIsConverting) return;
        e.preventDefault(); e.stopPropagation();
        caHoldStart = performance.now();
        const holdFill = document.getElementById('plCaHold');
        if (holdFill) { holdFill.style.transition = 'none'; holdFill.style.width = '0%'; }
        caAnimTick();
    }
    function caAnimTick() {
        if (!caHoldStart) return;
        const pct = Math.min(100, (performance.now() - caHoldStart) / 20);
        const holdFill = document.getElementById('plCaHold');
        if (holdFill) holdFill.style.width = pct + '%';
        if (pct >= 100) { caCommitCancel(); return; }
        caRafId = requestAnimationFrame(caAnimTick);
    }
    function caStopHold(e) {
        if (e) { e.preventDefault(); e.stopPropagation(); }
        if (!caHoldStart) return;
        caHoldStart = null;
        if (caRafId) { cancelAnimationFrame(caRafId); caRafId = null; }
        const holdFill = document.getElementById('plCaHold');
        if (holdFill) { holdFill.style.transition = 'width 0.3s ease'; holdFill.style.width = '0%'; }
    }
    async function caCommitCancel() {
        caHoldStart = null;
        if (caRafId) { cancelAnimationFrame(caRafId); caRafId = null; }
        // Reset the red hold-fill immediately so it does not stay at 100%
        // on the now-idle button after the cancel commits.
        const _holdFill = document.getElementById('plCaHold');
        if (_holdFill) { _holdFill.style.transition = 'none'; _holdFill.style.width = '0%'; }
        state.dzClickSuppressed = true;
        setTimeout(() => { state.dzClickSuppressed = false; }, 600);
        state.convNextQueue = [];
        state.convFileQueue.forEach(f => {
            if (!f.dismissed && f.status !== 'done' && f.status !== 'error') {
                f.status = 'cancelled'; f.dismissed = true;
            }
        });
        renderFileQueue();
        document.getElementById('dzCancelBtn').style.display = 'none';
        try { await invoke('cancel_conversion'); } catch (_) {}
    }

    caBtn.addEventListener('mousedown', caStartHold);
    caBtn.addEventListener('mouseup', caStopHold);
    caBtn.addEventListener('mouseleave', caStopHold);

    head.querySelector('#plClear').addEventListener('click', clearList);
}

// ── revolver scroll ───────────────────────────────────────────────────

// The list shows exactly visibleRows() whole rows. The wheel moves one row
// at a time with a smooth ease-out animation; fast flicks accumulate.
//
// [listening mode] The count is the CSS variable the list's height is made of
// (--pl-visible-rows: 10 in the studio, as many whole rows as fit in listening
// mode), so the scroll limits and the height can never disagree.
function visibleRows() {
    const list = document.getElementById('fileQueue');
    const n = list && parseInt(getComputedStyle(list).getPropertyValue('--pl-visible-rows'));
    return n > 0 ? n : 6;
}

let _revolverTarget = 0;   // target scrollTop in pixels
let _revolverRaf = null;   // active animation frame id
let _revolverFrom = 0;     // scrollTop at animation start
let _revolverStart = 0;    // performance.now() at animation start
const ANIM_MS = 180;

/// The list's numbers (list.css): a row's height, the space between rows,
/// the list's padding, and the step — one row plus the space under it.
function revolverGeo() {
    const style = getComputedStyle(document.documentElement);
    const rowH = parseInt(style.getPropertyValue('--pl-row-h').trim()) || 43;
    const gap = parseInt(style.getPropertyValue('--pl-row-gap').trim()) || 0;
    const pad = parseInt(style.getPropertyValue('--pl-list-pad').trim()) || 0;
    return { rowH, gap, pad, step: rowH + gap };
}

function revolverRowH() {
    // The scroll step is one row height plus the gap between rows so the
    // revolver advances by exactly one row per wheel tick.
    return revolverGeo().step;
}

function revolverScrollTo(list, target) {
    _revolverFrom = list.scrollTop;
    _revolverTarget = target;
    _revolverStart = performance.now();
    if (!_revolverRaf) _revolverRaf = requestAnimationFrame(ts => revolverTick(list, ts));
}

function revolverTick(list, ts) {
    const elapsed = ts - _revolverStart;
    const t = Math.min(1, elapsed / ANIM_MS);
    // Ease-out cubic.
    const ease = 1 - Math.pow(1 - t, 3);
    list.scrollTop = _revolverFrom + (_revolverTarget - _revolverFrom) * ease;
    if (t < 1) {
        _revolverRaf = requestAnimationFrame(ts2 => revolverTick(list, ts2));
    } else {
        list.scrollTop = _revolverTarget;
        _revolverRaf = null;
    }
}

function initRevolver(list) {
    list.addEventListener('wheel', (e) => {
        e.preventDefault();
        e.stopPropagation();
        // Accumulate from the current animation target (not current scrollTop)
        // so fast flicks build up rows rather than re-starting from the same spot.
        revolverStep(list, e.deltaY > 0 ? 1 : -1, dropTail);
    }, { passive: false });
}

/// A row further up (-1) or down (1). `extra` slots past the last row: the
/// gap's own slot while files from outside are over the list.
function revolverStep(list, dir, extra = 0) {
    const rh = revolverRowH();
    const maxTop = maxScroll(state.list.length + extra, visibleRows(), rh);
    _revolverTarget = Math.max(0, Math.min(maxTop, _revolverTarget + dir * rh));
    revolverScrollTo(list, _revolverTarget);
}

/// Where the drum stands, when it is not turning: a drag scrolls on from
/// there, not from where the last wheel tick meant it to be.
function revolverSync(list) {
    if (_revolverRaf) return;
    const rh = revolverRowH();
    _revolverTarget = Math.round(list.scrollTop / rh) * rh;
}

/// Resolves once the drum has stopped turning.
function revolverIdle() {
    return new Promise(res => {
        const wait = () => (_revolverRaf ? requestAnimationFrame(wait) : res());
        wait();
    });
}

/// Back within the list's own end (after the drop gap's extra slot).
function revolverClamp(list) {
    const maxTop = maxScroll(state.list.length, visibleRows(), revolverRowH());
    if (_revolverTarget > maxTop) revolverScrollTo(list, maxTop);
}

/// `count` files dropped into the gap at row `at`: the view stays where it
/// shows now, on the nearest whole row (list-geom.js scrollAfterDrop).
function revolverHold(at, count) {
    const list = document.getElementById('fileQueue');
    if (!list) return;
    revolverScrollTo(list, scrollAfterDrop(list.scrollTop, state.list.length, visibleRows(), revolverRowH(), at, count));
}

/// A drag let go: the drum stops on the row nearest to what is seen — a fast
/// edge scroll has its target a row or two ahead. Returns where it stops.
function revolverStop(list) {
    revolverScrollTo(list, scrollAfterDrop(list.scrollTop, state.list.length + dropTail, visibleRows(), revolverRowH()));
    return _revolverTarget;
}

/// After adding entries, scroll fast to the last new row.
function scrollToNewEntries(newEntries) {
    const list = document.getElementById('fileQueue');
    if (!list) return;
    const rh = revolverRowH();
    const lastIdx = state.list.length - 1;
    const maxTop = Math.max(0, (state.list.length - visibleRows()) * rh);
    // Jump to a position that makes the new rows visible, fast (shorter duration).
    _revolverFrom = list.scrollTop;
    _revolverTarget = Math.min(maxTop, Math.max(0, (lastIdx - visibleRows() + 1) * rh));
    _revolverStart = performance.now() - (ANIM_MS * 0.5); // start mid-animation = faster
    if (!_revolverRaf) _revolverRaf = requestAnimationFrame(ts => revolverTick(list, ts));
}

/// [listening mode] After the number of visible rows changes: keep the first
/// visible row where it is when it still fits, otherwise the last whole page,
/// always on a row boundary.
export function revolverResync() {
    const list = document.getElementById('fileQueue');
    if (!list) return;
    if (_revolverRaf) { cancelAnimationFrame(_revolverRaf); _revolverRaf = null; }
    const rh = revolverRowH();
    const maxTop = Math.max(0, (state.list.length - visibleRows()) * rh);
    _revolverTarget = Math.min(maxTop, Math.round(list.scrollTop / rh) * rh);
    list.scrollTop = _revolverTarget;
}

// ── wiring ────────────────────────────────────────────────────────────

export function initDropZone() {
    const dropZone = document.getElementById('dropZone');
    buildListHead(dropZone);
    const hint = document.querySelector('#dzEmpty .dz-empty-hint');
    if (hint) hint.textContent = 'or click to browse · then ▶ to listen, C to convert';

    dropZone.addEventListener('click', async (e) => {
        // The radio's view lives in the zone too (radio.js): a click there
        // is the radio's, not a browse for files — though the station it
        // landed on is out of the page by now (zone-click.js).
        if (!zoneClickBrowses(e, state.dzClickSuppressed)) return;
        try {
            const { open } = window.__TAURI__.dialog;
            const selected = await open({
                multiple: true,
                title: 'Select Audio Files',
                filters: [{ name: 'Audio Files', extensions: AUDIO_EXT }]
            });
            if (!selected) return;
            addFiles(Array.isArray(selected) ? selected : [selected]);
        } catch (_) {}
    });

    if (window.__TAURI__.event) {
        const myLabel = window.__TAURI__.window?.appWindow?.label ?? 'main';
        const forMe = (event) => !event.windowLabel || event.windowLabel === myLabel;

        // While files from outside hover, the rows part where the pointer
        // is; the drop lands there (anywhere else: at the end, as before).
        const gap = dropGap();
        window.__TAURI__.event.listen('tauri://file-drop', async (event) => {
            if (!forMe(event)) return;
            const at = await gap.stop(true);
            // The new rows take the gap's place in the same redraw.
            try { addFiles(event.payload || [], at); }
            finally { gap.finish(); }
        });
        window.__TAURI__.event.listen('tauri://file-drop-hover', (event) => {
            if (!forMe(event)) return;
            // The backend announces whatever is dragged over the window — a
            // link or a picture from a browser comes with no paths — and says
            // nothing when such a drag leaves or is let go: only audio files,
            // which the list takes, open the gap.
            if (!audioPaths(event.payload).length) return;
            gap.start();
        });
        window.__TAURI__.event.listen('tauri://file-drop-cancelled', async (event) => {
            if (!forMe(event)) return;
            await gap.stop(false);
            gap.finish();
        });
    }

    // Row buttons, and a double-click on a row to play it.
    const list = document.getElementById('fileQueue');
    list.addEventListener('click', (e) => {
        const btn = e.target.closest('[data-act]');
        const row = e.target.closest('.pl-item');
        if (!btn || !row) return;
        e.stopPropagation();
        const id = parseInt(row.dataset.id);
        const entry = state.list.find(x => x.id === id);
        if (!entry) return;
        if (btn.dataset.act === 'play') listHooks.play?.(entry);
        else if (btn.dataset.act === 'convert') {
            // C on an M entry uses its remembered settings (M3); non-M uses rack.
            if (isMActive(entry) && getMSettings(entry)) convertEntryM(entry);
            else convertEntries([entry]);
        }
        else if (btn.dataset.act === 'remove') removeEntry(id);
        else if (btn.dataset.act === 'memory') {
            // Toggle M for this entry; capture the current rack as its initial
            // snapshot when turning on.
            listHooks.memoryToggle?.(entry);
        }
    });
    list.addEventListener('dblclick', (e) => {
        if (e.target.closest('[data-act]')) return;
        const row = e.target.closest('.pl-item');
        const entry = row && state.list.find(x => x.id === parseInt(row.dataset.id));
        if (entry) listHooks.play?.(entry);
    });

    // Revolver wheel behaviour.
    initRevolver(list);

    // Rows dragged into a new order; at an edge the drum turns a row at a time.
    initListDrag(list, {
        geo: revolverGeo,
        scrollRows: dir => revolverStep(list, dir),
        stopScroll: () => revolverStop(list),
        scrollIdle: revolverIdle,
        onStart: () => { state.listDragging = true; revolverSync(list); },
        onEnd: () => {
            state.listDragging = false;
            if (state.listRenderPending) { state.listRenderPending = false; renderFileQueue(); }
        },
        onDrop: moveEntry,
    });

    // Hold to Cancel all
    const btn = document.getElementById('dzCancelBtn');
    const fill = document.getElementById('dzCancelFill');
    if (!btn) return;
    let holdStart = null, rafId = null;

    function startHold(e) {
        e.preventDefault(); e.stopPropagation();
        holdStart = performance.now(); fill.style.transition = 'none'; tick();
    }
    function tick() {
        if (!holdStart) return;
        const pct = Math.min(100, (performance.now() - holdStart) / 20);
        fill.style.width = pct + '%';
        if (pct >= 100) commitCancel(); else rafId = requestAnimationFrame(tick);
    }
    function stopHold(e) {
        if (e) { e.preventDefault(); e.stopPropagation(); }
        if (!holdStart) return; holdStart = null;
        if (rafId) { cancelAnimationFrame(rafId); rafId = null; }
        fill.style.transition = 'width 0.3s ease'; fill.style.width = '0%';
    }
    async function commitCancel() {
        holdStart = null; if (rafId) cancelAnimationFrame(rafId);
        state.dzClickSuppressed = true; setTimeout(() => state.dzClickSuppressed = false, 600);
        state.convNextQueue = [];
        state.convFileQueue.forEach(f => { if (!f.dismissed && f.status !== 'done' && f.status !== 'error') { f.status = 'cancelled'; f.dismissed = true; } });
        renderFileQueue(); btn.style.display = 'none';
        try { await invoke('cancel_conversion'); } catch (_) {}
    }
    btn.addEventListener('mousedown', startHold);
    btn.addEventListener('mouseup', stopHold);
    btn.addEventListener('mouseleave', stopHold);
    btn.addEventListener('click', e => e.stopPropagation());

    // Re-render the list whenever the user changes a rack setting so each row's
    // C / disk-play button reflects the new sig against that entry's convs.
    const _panel = document.getElementById('converterPanel');
    if (_panel) {
        let _rackRenderTimer = null;
        _panel.addEventListener('change', (ev) => {
            // The rack decides what the rows show; the player's own controls
            // (volume, device) and the list's do not.
            if (cameThrough(ev, '#plBar, #plListHead, #fileQueue, #rdView')) return;
            clearTimeout(_rackRenderTimer);
            _rackRenderTimer = setTimeout(renderFileQueue, 60);
        });
    }

    // Schedule library init as a microtask. It fires when init() awaits
    // loadFilterInventory() — BEFORE loadSettings() runs. The rack-correct
    // render is deferred inside restore() with requestAnimationFrame, which
    // fires after init() resumes and calls loadSettings().
    Promise.resolve().then(() => initLibrary({
        listHooks,
        renderFileQueue,
        convLive,
        addFiles: (paths, existing) => {
            // When restoring, add the paths to player via listHooks.added.
            // We only want the player to get the paths; the entries already
            // exist in state.list (put there by library.js's restore()).
            listHooks.added?.(existing || []);
        },
    }));
}
