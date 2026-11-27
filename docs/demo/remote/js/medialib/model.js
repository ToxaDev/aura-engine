// ══════════════════════════════════════════════════════════════════════
// The media library's plain logic, with no page and no backend: the sorts,
// the album query the backend takes, a cover's address, the words of a
// line, the order a queue gets. The views (view.js) and the tests use it.
// ══════════════════════════════════════════════════════════════════════

import { auraUrl } from '../aura-url.js';

/// The sections, in the tabs' order.
/// Folders stays the last of them, whatever is added (Anton 21.11).
export const SECTIONS = ['albums', 'artists', 'genres', 'playlists', 'folders'];

/// The landscape window's own sections under the library's: pages of their
/// own (tools.js), not lists of albums. The compact view has no room for them.
// The Visualizer stays last: whatever tool is added goes above it (Anton 21.11).
export const TOOLS = ['conversions', 'dupes', 'analyzer', 'visualizer'];

/// The sections a layout shows.
export const sectionsOf = layout => (layout === 'wide' ? [...SECTIONS, ...TOOLS] : SECTIONS);

/// The sorts, with their words; the backend sorts by all but the resolution,
/// which is sorted here.
export const SORTS = {
    added: 'Recently added', artist: 'Artist', title: 'Album', year: 'Year', res: 'Resolution',
};

export const plural = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;

/// "3:07", "1:02:45".
export function time(s) {
    s = Math.round(Number(s) || 0);
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${x}` : `${m}:${x}`;
}

/// When an album came: "today", "3 days ago", or its month ("Oct 2026").
/// `added` and `now` in seconds.
export function addedText(added, now = Date.now() / 1000) {
    if (!added) return '';
    const days = Math.floor((now - added) / 86400);
    if (days < 1) return 'today';
    if (days < 2) return 'yesterday';
    if (days < 30) return `${days} days ago`;
    const d = new Date(added * 1000);
    return `${d.toLocaleString('en', { month: 'short' })} ${d.getFullYear()}`;
}

/// The album query the backend takes (`lib_albums`): the filters set, and
/// its own sort (the resolution is sorted here, after the newest first).
/// `dupes`: the copies of a track or an album shown too (they are hidden).
export function albumQuery({ q = '', genre = null, artist = null, hiRes = false, dsd = false, sort = 'added', dupes = false } = {}) {
    const a = { sort: sort === 'res' || !SORTS[sort] ? 'added' : sort };
    if (q && q.trim()) a.q = q.trim();
    if (genre) a.genre = genre;
    if (artist) a.artist = artist;
    if (hiRes) a.hiRes = true;
    if (dsd) a.dsd = true;
    if (dupes) a.dupes = true;
    return a;
}

/// A format tag's rate in Hz: "24/96" → 96000, "16/44.1" → 44100, "DSD64"
/// → 2822400; 0 when the tag has none ("FLAC", "MP3").
export function rateOfTag(tag) {
    const s = String(tag || '');
    const d = s.match(/^DSD(\d+)/i);
    if (d) return 44100 * Number(d[1]);
    const p = s.match(/^\d+\/(\d+(?:\.\d+)?)$/);
    return p ? Math.round(Number(p[1]) * 1000) : 0;
}

/// How high a tag stands among the resolutions: DSD over PCM, then the
/// rate, then the bits; a tag with no rate lowest.
export function resRank(tag) {
    const s = String(tag || '');
    const d = s.match(/^DSD(\d+)/i);
    if (d) return 1e9 + Number(d[1]);
    const p = s.match(/^(\d+)\/(\d+(?:\.\d+)?)$/);
    if (!p) return 0;
    return Math.round(Number(p[2]) * 1000) * 100 + Number(p[1]);
}

/// The albums as the sort asks: the backend's order kept, the resolution
/// sorted here (the highest first, then by title).
export function sortAlbums(list, sort) {
    if (sort !== 'res') return list;
    const key = a => String(a.title || '').toLowerCase();
    return [...list].sort((a, b) => resRank(b.tag) - resRank(a.tag) || key(a).localeCompare(key(b)));
}

/// A cover's address in this window: the backend gives the streamer's
/// ("/api/lib/cover/<id>?v=<hash>"), the window's own protocol serves it
/// without "/api". Null without one.
export function coverUrl(cover) {
    if (!cover) return null;
    const s = String(cover);
    if (/^(https?|aura|data):/.test(s)) return s;
    // The streamer serves covers itself (server.rs /api/lib/cover), not
    // through the engine's /aura routes.
    if (typeof window !== 'undefined' && window.__AURA_REMOTE__ && s.startsWith('/api/')) return s;
    const path = s.replace(/^\/api(?=\/)/, '');
    return auraUrl(path.startsWith('/') ? path : '/' + path);
}

/// The kind of a tag for its colour: 'dsd', 'hi' or ''.
export function fmtKind(tag, hiRes, dsd) {
    if (dsd || /^DSD/i.test(String(tag || ''))) return 'dsd';
    return hiRes ? 'hi' : '';
}

/// One or two letters for a name without a picture.
export function initials(name) {
    const w = String(name || '').match(/[\p{L}\p{N}]+/gu) || ['?'];
    return w.slice(0, 2).map(x => [...x][0]).join('').toUpperCase();
}

/// The album's line under its title: year, tracks, length, genres.
export function albumLine(a) {
    return [a.year, plural(a.tracks ?? (a.list || []).length, 'track'), time(a.durationS), (a.genres || []).join(', ')]
        .filter(Boolean).join(' · ');
}

/// The tracks of an album page in the order they play, as paths.
export const albumPaths = a => (a?.list || []).map(t => t.path).filter(Boolean);

/// The order a queue gets: `paths` as they are, or shuffled (`rng` returns
/// [0, 1), Math.random by default). The paths are not changed.
export function queueOrder(paths, { shuffle = false, rng = Math.random } = {}) {
    const out = [...paths];
    if (!shuffle) return out;
    for (let i = out.length - 1; i > 0; i--) {
        const j = Math.floor(rng() * (i + 1));
        [out[i], out[j]] = [out[j], out[i]];
    }
    return out;
}

/// What of `paths` is not in `have` (paths already in a list), in order,
/// each once.
export function missingPaths(paths, have) {
    const seen = new Set(have);
    const out = [];
    for (const p of paths) {
        if (seen.has(p)) continue;
        seen.add(p);
        out.push(p);
    }
    return out;
}

const dirOf = p => String(p).replace(/[\\/][^\\/]*$/, '');

/// Whether the file `path` plays from the album folder `folder` (or a "CD1"
/// under it).
export function playsFrom(folder, path) {
    if (!path || !folder) return false;
    const d = dirOf(path);
    return d === folder || (dirOf(d) === folder && /^(cd|dis[ck])[\s_.-]*\d+$/i.test(d.slice(folder.length + 1)));
}

/// The scan's line: where, then what it does or found.
export function scanLine(s) {
    if (!s) return 'Reading the library…';
    const roots = s.roots || [];
    if (!roots.length) return 'No music folder yet';
    const where = roots.length === 1 ? roots[0] : `${roots.length} folders`;
    if (s.scanning) return `${where} · ${s.total ? `reading ${s.done} of ${s.total}` : 'looking for music'}`;
    if (s.error) return `${where} · ${s.error}`;
    return `${where} · ${plural(s.albums || 0, 'album')} · ${plural(s.tracks || 0, 'track')}`;
}
