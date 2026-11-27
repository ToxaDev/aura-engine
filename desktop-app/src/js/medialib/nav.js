// ══════════════════════════════════════════════════════════════════════
// Where the library's browser has been: Back and Forward, as a browser's.
// A place is whatever the browser gives (its section, artist, album, folder,
// playlist, search), kept with how far its list was scrolled. Nothing here
// touches the page.
// ══════════════════════════════════════════════════════════════════════

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/// A history of places. `mark(at, top)` before a move keeps where the
/// browser is; `back(at, top)` / `forward(at, top)` give the place to show
/// ({ at, top }) or null, keeping the one left for the way the other way.
export function createHistory(limit = 80) {
    let back = [];
    let fwd = [];
    const take = (from, to, at, top) => {
        // A place kept twice in a row (a section pressed where it already
        // was) is one place.
        while (from.length && same(from[from.length - 1].at, at)) from.pop();
        if (!from.length) return null;
        const s = from.pop();
        to.push({ at, top });
        if (to.length > limit) to.shift();
        return s;
    };
    return {
        mark(at, top = 0) {
            const last = back[back.length - 1];
            if (last && same(last.at, at)) last.top = top;
            else back.push({ at, top });
            if (back.length > limit) back.shift();
            fwd = [];
        },
        back: (at, top = 0) => take(back, fwd, at, top),
        forward: (at, top = 0) => take(fwd, back, at, top),
        canBack: at => back.some(s => !same(s.at, at)),
        canForward: at => fwd.some(s => !same(s.at, at)),
    };
}

/// The path to a place, for the head: [{ text, to }] from the section down;
/// `to` is the place a press on that step shows (null for the last one).
/// `names` gives the words: tabs { albums: 'Albums', … }, search, and the
/// titles known only once loaded (album, playlist).
export function crumbsOf(at, names = {}) {
    const root = { tab: at.tab, artist: null, genre: null, flt: null, album: null, folder: null, playlist: null, q: '' };
    const out = [{ text: names.tabs?.[at.tab] || at.tab, to: root }];
    if (at.q) out.push({ text: `${names.search || 'Search'}: ${at.q}`, to: { ...root, q: at.q } });
    if (at.tab === 'albums' && at.genre && !at.q) out.push({ text: at.genre, to: { ...root, genre: at.genre, flt: at.flt || null } });
    if (at.tab === 'albums' && at.artist && !at.q) out.push({ text: at.artist, to: { ...root, genre: at.genre || null, artist: at.artist } });
    if (at.tab === 'folders' && at.folder != null) {
        out.push({ text: String(at.folder).replace(/[\\/]+$/, '').split(/[\\/]/).pop() || String(at.folder), to: { ...root, folder: at.folder } });
    }
    if (at.tab === 'playlists' && at.playlist != null) out.push({ text: names.playlist || '…', to: { ...root, playlist: at.playlist } });
    if (at.album) out.push({ text: names.album || '…', to: null });
    out[out.length - 1].to = null;
    return out;
}

/// Where each section was left, so that coming back to it (its item in the
/// side or its tab, pressed once or again) shows it as it was: the search,
/// the chips, the artist, the album or folder open, how far it was scrolled.
/// `keep(tab, state)` when a section is left; `recall(tab)` gives it back
/// (or null for one not visited yet). Kept for the app's run only, per
/// layout, so a browser mounted again finds it.
const memories = new Map();
export function sectionMemory(layout = 'wide') {
    if (!memories.has(layout)) memories.set(layout, new Map());
    const m = memories.get(layout);
    return {
        keep(tab, state) { if (tab) m.set(tab, JSON.parse(JSON.stringify(state))); },
        recall: tab => (m.has(tab) ? JSON.parse(JSON.stringify(m.get(tab))) : null),
        forget(tab) { if (tab == null) m.clear(); else m.delete(tab); },
    };
}

/// What a press on a section does: null when it is the section shown and
/// no folder is asked for (nothing changes, nothing is reset); else the
/// place to show and how far down: the remembered one, the folder asked
/// for, or the section's own page. `root` asks for the section's own page.
export function sectionTarget(current, name, kept, { folder = null, root = false } = {}) {
    if (folder != null) return { at: { tab: name, folder: name === 'folders' ? folder : null }, top: 0, kept: null };
    if (root) return { at: { tab: name }, top: 0, kept: null };
    if (current === name) return null;
    if (kept?.at) return { at: { ...kept.at, tab: name }, top: kept.top || 0, kept };
    return { at: { tab: name }, top: 0, kept: null };
}
