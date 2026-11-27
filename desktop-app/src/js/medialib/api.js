// ══════════════════════════════════════════════════════════════════════
// The media library's backend and its news: one command (`lib_call`) takes
// the library's own command names, as the streamer's remote sends them; the
// scan's status is asked here once for every view on the page, and what one
// view changes (the section, the music folders) is told to the others, so
// the landscape's side and centre stay in step.
// ══════════════════════════════════════════════════════════════════════

/// A library command: `lib_albums`, `lib_album {id}`, `lib_browse {path}`…
export function lib(cmd, args = {}) {
    const invoke = window.__TAURI__?.core?.invoke;
    if (!invoke) return Promise.reject(new Error('The library needs the app.'));
    return invoke('lib_call', { cmd, args });
}

const subs = new Set();

/// Fires `fn(event)` on the library's news: { type: 'section', section,
/// layout } when a view changes its section, { type: 'status', status } on
/// each scan status, { type: 'roots' } when the music folders change,
/// { type: 'album', id } when an album page opens. Returns the unsubscribe.
export function onMediaLibrary(fn) {
    subs.add(fn);
    return () => subs.delete(fn);
}

/// Tells every listener.
export function emit(ev) {
    for (const fn of [...subs]) {
        try { fn(ev); } catch (e) { console.warn('media library listener', e); }
    }
}

// ── the scan's status, asked for all ────────────────────────────────────
let status = null;
let watchers = 0;
let timer = 0;
let asking = false;

export const libStatus = () => status;

async function tick() {
    clearTimeout(timer);
    timer = 0;
    if (!watchers || asking) return;
    asking = true;
    let next = 60000;
    try {
        const was = status;
        status = await lib('lib_status');
        emit({ type: 'status', status, was });
        if (status.scanning) next = 1500;
    } catch (_) {
        next = 5000;
    } finally {
        asking = false;
    }
    if (watchers && !timer) timer = setTimeout(tick, next);
}

/// Keeps the status coming while a view is on the page; the returned
/// function lets it go.
export function watchStatus() {
    watchers++;
    if (watchers === 1) tick();
    else if (status) queueMicrotask(() => emit({ type: 'status', status, was: status }));
    return () => {
        watchers = Math.max(0, watchers - 1);
        if (!watchers) { clearTimeout(timer); timer = 0; }
    };
}

/// The status asked again now: after a folder is added or removed, a rescan.
export function statusNow() {
    if (watchers) tick();
}

// ── the music folders ───────────────────────────────────────────────────

/// The system's folder picker, then the folder joins the library (its scan
/// starts). Resolves to the path, or null when nothing was chosen.
export async function addFolder() {
    // The streamer's remote: no system picker; the browser walks the
    // streamer's folders by name instead (view.js pickView).
    if (window.__AURA_REMOTE__) { emit({ type: 'pick' }); return null; }
    const open = window.__TAURI__?.dialog?.open;
    if (!open) throw new Error('No folder picker in this build.');
    const picked = await open({ directory: true, multiple: false, title: 'Add a music folder' });
    const path = Array.isArray(picked) ? picked[0] : picked;
    if (!path) return null;
    await lib('lib_add_root', { path });
    emit({ type: 'roots' });
    statusNow();
    return path;
}

/// A music folder leaves the library (its files stay where they are).
export async function removeFolder(path) {
    await lib('lib_remove_root', { path });
    emit({ type: 'roots' });
    statusNow();
}

/// The music folders read again.
export async function rescan() {
    await lib('lib_rescan');
    statusNow();
}
