// ══════════════════════════════════════════════════════════════════════
// The media library's browser: search, Albums / Artists / Genres / Folders /
// Playlists, the genre and HI-RES / DSD chips, the sort, the album grid (or
// list) with covers, and an album's page — Play, Shuffle, + Queue,
// + Playlist, Convert to what is heard now, the tracks.
//
// Two layouts of the same browser: 'compact' in the list's place of the
// 480 px window (the M view: its search and grid / list switch in the list's
// head, the tabs over the albums), and 'wide' in the landscape window's
// centre (a title and the search over the chips, the sort, the tiles' size;
// the sections are the side's, side.js). Each keeps its own tab, sort and
// look for the next start.
// ══════════════════════════════════════════════════════════════════════

import { lib, emit, onMediaLibrary, watchStatus, libStatus, addFolder, removeFolder, rescan } from './api.js';
import {
    SECTIONS, TOOLS, sectionsOf, SORTS, albumQuery, sortAlbums, coverUrl, fmtKind, initials, albumLine, albumPaths, plural, time,
    addedText, playsFrom, scanLine, rateOfTag,
} from './model.js';
import { playPaths, queuePaths, addTarget, addToPlaylist, playlists, playlistTracks, playPlaylist, showPlaylist, playingPath, canRemovePlaylist, removePlaylist } from './play.js';
import { planFor, convertPaths, converterModulators, rackFs } from './convert.js';
import { formatMenu } from './convert-format.js';
import { createHistory, crumbsOf, sectionMemory, sectionTarget } from './nav.js';
import { openMenu, closeMenu } from './menu.js';
import { TOOL_PAGES } from './tools.js';
import { SCOPE_KEY } from './dupes.js';
import { createPickList } from '../pick-list.js';
import { confirmAt } from '../confirm-pop.js';
import { createPlaylist } from '../library.js';
import { playerStatus } from '../player.js';

// UI texts.
export const LIB_TEXTS = {
    search: 'Search artists, albums, tracks',
    tabs: { albums: 'Albums', artists: 'Artists', genres: 'Genres', folders: 'Folders', playlists: 'Playlists', conversions: 'Conversions', dupes: 'Duplicates', analyzer: 'Analyzer', visualizer: 'Visualizer' },
    all: 'All', hiRes: 'HI-RES', dsd: 'DSD',
    sort: 'Sort:',
    grid: 'Tiles', list: 'List', size: 'Tile size',
    play: 'Play', shuffle: 'Shuffle', queue: '+ Queue', playlist: '+ Playlist',
    playTip: 'Play the album: it becomes the "Library" playlist, in order.',
    shuffleTip: 'Play the album shuffled: it becomes the "Library" playlist.',
    queueTip: 'Add the album to the end of the playlist that plays (of the one in the queue when nothing plays); what plays goes on.',
    playlistTip: 'Add the album to one of your playlists.',
    formatTip: 'Convert to another format.',
    newPlaylist: name => `New playlist “${name}”`,
    added: (n, name) => (n ? `${plural(n, 'track')} added to “${name}”` : `Already in “${name}”`),
    converting: n => `Converting ${plural(n, 'track')} — the progress is in the queue`,
    convBusy: 'A conversion is running; this album can go when it ends.',
    convNoMod: 'The converter has no such modulator.',
    noFolder: 'No music folder yet.',
    addFolder: '+ Add a music folder',
    addFolderShort: '+ Add folder',
    pickTop: 'Choose a music folder on the streamer\'s computer',
    pickIn: 'Choose a music folder:',
    pickThis: '+ Use this folder',
    rescan: '↻ Read the folders again',
    remove: 'Remove',
    removeAsk: p => `Take ${p} out of the library? The files stay where they are.`,
    musicFolders: 'Music folders',
    playFolder: '▶ Play this folder',
    reading: 'Reading your music…',
    noMusic: 'No music found in the music folders.',
    noFilter: 'No albums with this filter.',
    nothing: q => `Nothing found for “${q}”.`,
    artists: 'Artists', albums: 'Albums', tracks: 'Tracks',
    showInFiles: 'Show in Files',
    showInQueue: 'Show in Queue',
    inFiles: 'in Files',
    inQueue: 'in Queue',
    back: 'Back (Alt+←, Backspace)',
    forward: 'Forward (Alt+→)',
    searchTitle: 'Search',
    more: 'More',
    playlistKind: 'Playlist',
    mPlay: 'Play',
    mPlayAlbum: 'Play the album',
    mShuffle: 'Shuffle',
    mQueue: 'Add to queue',
    mOpen: 'Open the album',
    mToPlaylist: 'Add to playlist',
    mNewPlaylist: 'New playlist…',
    mConvert: t => `Convert → ${t}`,
    manage: 'Manage folders',
    tools: 'TOOLS',
    folderHint: 'The library reads these folders and everything in them. Removing one takes its music out of the library; the files stay where they are.',
    noPlaylists: 'No playlists.',
    emptyPlaylist: 'This playlist is empty.',
    delPlaylist: 'Delete',
    delPlaylistTip: 'Delete this playlist. The music files stay where they are.',
    delPlaylistAsk: (name, n) => `Delete the playlist “${name}”${n ? ` with its ${n === 1 ? 'track' : n + ' tracks'}` : ''}? The music files stay on disk.`,
    delPlaylistYes: 'Delete playlist',
    delPlaylistLast: 'The only playlist stays.',
    delPlaylistBusy: 'This playlist is converting — wait for it or cancel the conversions first.',
    delPlaylistDone: name => `Playlist “${name}” deleted.`,
    album: 'Album',
    folders: 'Folders',
    playFailed: 'The player did not take it.',
    dupesHidden: n => `Dupes: hidden${n ? ' · ' + n : ''} ▾`,
    dupesShown: 'Dupes: shown ▾',
    dupesTip: 'Copies of the same track in other folders: hidden (one copy shows, with ×2) or shown as they are',
    copiesTip: n => `${n} more cop${n === 1 ? 'y' : 'ies'} of this in the library: click to see where`,
    copyAt: 'Copy in',
    copyOf: 'This is a copy; the main one:',
    inDupes: 'Show in Duplicates ›',
    findDupes: 'Find duplicates here',
    findDupesTip: 'Look for copies of the tracks in this folder (the Duplicates page)',
};

const STORE = 'auraMediaLib.';
const CHUNK = 120;          // tiles drawn at a time; more as the list scrolls
const ICON = {
    search: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/></svg>',
    grid: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1" y="1" width="6" height="6" rx="1"/><rect x="9" y="1" width="6" height="6" rx="1"/><rect x="1" y="9" width="6" height="6" rx="1"/><rect x="9" y="9" width="6" height="6" rx="1"/></svg>',
    list: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1" y="2" width="14" height="2" rx="1"/><rect x="1" y="7" width="14" height="2" rx="1"/><rect x="1" y="12" width="14" height="2" rx="1"/></svg>',
    play: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 2.5v11l9-5.5z"/></svg>',
    bin: '<svg class="ln" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>',
    more: '<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="3" cy="8" r="1.5"/><circle cx="8" cy="8" r="1.5"/><circle cx="13" cy="8" r="1.5"/></svg>',
    shuffle: '<svg viewBox="0 0 24 24" aria-hidden="true" class="ln"><path d="M3 7h3.5c5 0 5.5 10 10.500 10H21M3 17h3.5c1.600 0 2.700-1 3.600-2.400M13.400 9.400C14.300 8 15.400 7 17 7h4M18 4l3 3-3 3M18 14l3 3-3 3"/></svg>',
    plus: '<svg viewBox="0 0 16 16" aria-hidden="true" class="ln"><path d="M8 3v10M3 8h10"/></svg>',
    convert: '<svg viewBox="0 0 24 24" aria-hidden="true" class="ln"><path d="M4 8h14l-3.500-3.500M20 16H6l3.500 3.500"/></svg>',
    left: '<svg viewBox="0 0 16 16" aria-hidden="true" class="ln"><path d="M10 3L5 8l5 5"/></svg>',
    right: '<svg viewBox="0 0 16 16" aria-hidden="true" class="ln"><path d="M6 3l5 5-5 5"/></svg>',
    plist: '<svg viewBox="0 0 24 24" aria-hidden="true" class="ln"><path d="M4 6h12M4 11h12M4 16h7"/><path d="M17 14v6l4-3z"/></svg>',
    folder: '<svg viewBox="0 0 24 24" aria-hidden="true" class="ln"><path d="M3 6h7l2 2h9v11H3z"/></svg>',
    small: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="3" y="3" width="10" height="10" rx="2"/></svg>',
    big: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1" y="1" width="14" height="14" rx="2"/></svg>',
};

const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
};
const button = (cls, text, tip) => {
    const b = el('button', cls, text);
    b.type = 'button';
    if (tip) b.dataset.tip = tip;
    return b;
};
const keepGet = layout => { try { return JSON.parse(localStorage.getItem(STORE + layout) || '{}') || {}; } catch (_) { return {}; } };
const keepSet = (layout, v) => { try { localStorage.setItem(STORE + layout, JSON.stringify(v)); } catch (_) {} };

/// A cover, or the title on a dark ground when there is none (or it fails).
function cover(url, title, cls = 'ml-cv') {
    const box = el('div', cls);
    let img = null;
    // In the picture's place only: the tile's marks and Play stay.
    // Each title its own colour, the same every time.
    const ph = () => {
        img?.remove();
        let h = 0;
        for (const c of String(title || '')) h = (h * 31 + c.codePointAt(0)) % 360;
        box.style.setProperty('--ph', String(h));
        box.prepend(el('span', 'ml-ph', title || ''));
        box.classList.add('none');
    };
    const src = coverUrl(url);
    if (!src) { ph(); return box; }
    img = el('img');
    img.alt = '';
    img.loading = 'lazy';
    img.decoding = 'async';
    img.draggable = false;
    img.src = src;
    img.addEventListener('error', ph, { once: true });
    box.append(img);
    return box;
}

function fmtTag(tag, hiRes, dsd) {
    const s = el('span', 'ml-fmt', tag || '');
    const k = fmtKind(tag, hiRes, dsd);
    if (k) s.classList.add(k);
    return s;
}

/// Mounts the browser into `host`. `headHost` (compact): where the search
/// and the grid / list switch go (the list's head).
export function mountMediaLibrary(host, { layout = 'compact', headHost = null, tools = [] } = {}) {
    const kept = keepGet(layout);
    // `tools`: pages of TOOLS shown as tabs in the compact layout too (the
    // streamer's remote has no side column for them).
    const extra = layout === 'wide' ? [] : TOOLS.filter(t => tools.includes(t));
    const secs = [...sectionsOf(layout), ...extra];
    const S = {
        tab: secs.includes(kept.tab) ? kept.tab : 'albums',
        view: kept.view === 'list' ? 'list' : 'grid',
        sort: SORTS[kept.sort] ? kept.sort : 'added',
        tile: Math.min(260, Math.max(120, Number(kept.tile) || 168)),
        q: '', genre: null, flt: null, artist: null, album: null,
        folder: null, picking: false, playlist: null,
        genres: [], chosen: new Map(),
        albumTitle: '', upFolder: undefined,
        dupes: (() => { try { return localStorage.getItem(STORE + 'dupes') === 'shown'; } catch (_) { return false; } })(),
    };
    const hist = createHistory();
    const mem = sectionMemory(layout);
    /// Where the browser is, for Back / Forward and the head's path.
    const at = () => ({ tab: S.tab, artist: S.artist, genre: S.genre, flt: S.flt, album: S.album, folder: S.folder, playlist: S.playlist, q: S.q });
    const save = () => keepSet(layout, { tab: S.tab, view: S.view, sort: S.sort, tile: S.tile });
    const wide = layout === 'wide';

    const root = el('div', 'ml');
    root.dataset.layout = layout;
    root.style.setProperty('--tile', S.tile + 'px');
    root.innerHTML = `
      <div class="ml-head">
        <div class="ml-navb"><button type="button" data-nav="back" data-tip="${LIB_TEXTS.back}">${ICON.left}</button><button type="button" data-nav="fwd" data-tip="${LIB_TEXTS.forward}">${ICON.right}</button></div>
        <div class="ml-title"></div>
        <label class="ml-search">${ICON.search}<input type="search" spellcheck="false" autocomplete="off"><kbd>/</kbd></label>
        <div class="ml-vt"><button type="button" data-v="grid">${ICON.grid}</button><button type="button" data-v="list">${ICON.list}</button></div>
      </div>
      <nav class="ml-tabs" role="tablist"></nav>
      <div class="ml-bar">
        <div class="ml-chips"></div>
        <div class="ml-sortl"><span class="ml-art-x" hidden></span><span>${LIB_TEXTS.sort}</span><button type="button" class="ml-sortb"></button><button type="button" class="ml-dupt"></button><span class="ml-cnt"></span></div>
        <label class="ml-zoom" data-tip="${LIB_TEXTS.size}">${ICON.small}<input type="range" min="120" max="260" step="4">${ICON.big}</label>
      </div>
      <div class="ml-scroll"></div>
      <div class="ml-scan"><span class="ml-dot"></span><span class="ml-scantext"></span><button type="button" class="ml-lk" data-act="folders">⚙ ${LIB_TEXTS.folders}</button></div>
      <div class="ml-toast" hidden></div>`;
    const $ = s => root.querySelector(s);
    const head = $('.ml-head');
    const input = $('.ml-search input');
    const vt = $('.ml-vt');
    const tabs = $('.ml-tabs');
    const chips = $('.ml-chips');
    const sortl = $('.ml-sortl');
    const sortb = $('.ml-sortb');
    const dupt = $('.ml-dupt');
    const cnt = $('.ml-cnt');
    const artX = $('.ml-art-x');
    const zoom = $('.ml-zoom');
    const scroll = $('.ml-scroll');
    const scanText = $('.ml-scantext');
    const dot = $('.ml-dot');
    const toastBox = $('.ml-toast');
    input.placeholder = LIB_TEXTS.search;
    vt.querySelector('[data-v="grid"]').dataset.tip = LIB_TEXTS.grid;
    vt.querySelector('[data-v="list"]').dataset.tip = LIB_TEXTS.list;
    zoom.querySelector('input').value = S.tile;

    // Compact: the search and the switch in the list's head.
    let hbar = null;
    if (headHost) {
        hbar = el('div', 'ml-hbar');
        hbar.append($('.ml-search'), vt);
        headHost.append(hbar);
    }
    host.append(root);

    for (const t of [...SECTIONS, ...extra]) {
        const b = button('ml-tab', LIB_TEXTS.tabs[t]);
        b.dataset.tab = t;
        b.setAttribute('role', 'tab');
        b.addEventListener('click', () => setSection(t));
        tabs.append(b);
    }

    // ── what the browser shows ─────────────────────────────────────────
    let seq = 0;
    let destroyed = false;
    let io = null;
    let listScroll = 0;

    const albumsList = () => !S.album && !S.q && S.tab === 'albums';

    function paintChrome() {
        root.dataset.tab = S.tab;
        root.dataset.view = S.view;
        root.classList.toggle('ml-on-album', !!S.album);
        for (const b of tabs.children) b.classList.toggle('on', b.dataset.tab === S.tab);
        for (const b of vt.children) b.classList.toggle('on', b.dataset.v === S.view);
        const al = albumsList();
        // A tool's page has its own search and switches.
        const tool = TOOLS.includes(S.tab);
        root.classList.toggle('ml-tool', tool);
        $('.ml-search').hidden = tool;
        chips.hidden = !al;
        sortl.hidden = !al;
        zoom.hidden = !(wide && al && S.view === 'grid');
        vt.hidden = tool || !(al || (S.q && !S.album));
        $('.ml-bar').hidden = !al;
        tabs.hidden = wide;
        sortb.textContent = `${SORTS[S.sort]} ▾`;
        paintDupt();
        // Compact: the artist chosen stands by the sort (the wide head's
        // path names it).
        artX.hidden = wide || !S.artist;
        artX.textContent = S.artist ? `✕ ${S.artist}` : '';
        paintPath();
        paintChips();
    }

    /// The head's path (wide): the section, then what is open in it; the
    /// last step is the page's title, the ones before lead back.
    /// The sort line's switch: copies hidden (with how many) or shown.
    function paintDupt() {
        const n = libStatus()?.copies || 0;
        dupt.hidden = !S.dupes && !n;
        dupt.textContent = S.dupes ? LIB_TEXTS.dupesShown : LIB_TEXTS.dupesHidden(n);
        dupt.classList.toggle('on', S.dupes);
        dupt.dataset.tip = LIB_TEXTS.dupesTip;
    }
    dupt.addEventListener('click', e => {
        if (e.detail > 0) dupt.blur();
        S.dupes = !S.dupes;
        try { localStorage.setItem(STORE + 'dupes', S.dupes ? 'shown' : 'hidden'); } catch (_) {}
        paintDupt();
        render({ keepScroll: true });
    });

    /// The Duplicates page, looking in `folder` (the whole library without one).
    function toDupes(folder) {
        try { if (folder) sessionStorage.setItem(SCOPE_KEY, folder); else sessionStorage.removeItem(SCOPE_KEY); } catch (_) {}
        if (S.tab === 'dupes') { render(); return; }
        if (!secs.includes('dupes')) return;
        setSection('dupes');
    }
    const dirOf = p => String(p || '').replace(/[\\/][^\\/]*$/, '');

    /// ×N on a card, a row or a track: its copies elsewhere. `copies` the
    /// count (an album) or the paths (a track); `where` the copies' folders
    /// or paths, shown on a click.
    function copiesBadge(n, where, { inline = null, of = null } = {}) {
        const b = el('span', 'ml-x2', `×${n + 1}`);
        b.dataset.tip = LIB_TEXTS.copiesTip(n);
        b.setAttribute('role', 'button');
        b.addEventListener('click', e => {
            e.stopPropagation();
            e.preventDefault();
            if (inline) { inline(); return; }
            openMenu(b, [
                ...(of ? [{ group: LIB_TEXTS.copyOf }, { text: of, cls: 'dim', tap: () => {} }] : [{ group: LIB_TEXTS.copyAt }]),
                ...where.map(w => ({ text: w, cls: 'dim', tap: () => {} })),
                { sep: true },
                { text: LIB_TEXTS.inDupes, tap: () => toDupes(where.length === 1 ? (/\.[a-z0-9]{2,5}$/i.test(where[0]) ? dirOf(where[0]) : where[0]) : null) },
            ]);
        });
        return b;
    }

    /// A track's copies under its row, opened and closed by its ×N.
    function trackCopies(row, t) {
        const paths = Array.isArray(t.copies) ? t.copies : [];
        if (!paths.length) return null;
        return copiesBadge(paths.length, paths, {
            inline: () => {
                const next = row.nextElementSibling;
                if (next?.classList.contains('ml-copies')) { next.remove(); return; }
                const box = el('div', 'ml-copies');
                const line = (k, p) => { const d = el('div', null); d.append(el('span', 'ml-ck', k), el('span', 'ml-cp', p)); return d; };
                box.append(line(t.copyOf ? '○ this' : '● plays', t.path));
                for (const p of paths) box.append(line(p === t.copyOf ? '● main' : '○ copy', p));
                const go = button('ml-lk', LIB_TEXTS.inDupes);
                go.addEventListener('click', () => toDupes(dirOf(t.path)));
                box.append(go);
                row.after(box);
            },
        });
    }

    function paintPath() {
        const title = $('.ml-title');
        const pl = S.playlist != null ? playlists().find(p => p.id === S.playlist) : null;
        const steps = crumbsOf(at(), { tabs: LIB_TEXTS.tabs, search: LIB_TEXTS.searchTitle, album: S.albumTitle, playlist: pl?.name });
        title.replaceChildren();
        steps.forEach((s, i) => {
            if (i) title.append(el('span', 'ml-crs', '›'));
            if (s.to) {
                const b = button('ml-cr', s.text);
                b.addEventListener('click', () => go(s.to));
                title.append(b);
            } else title.append(el('span', 'ml-cr on', s.text));
        });
        const here = at();
        const nb = $('.ml-navb');
        nb.querySelector('[data-nav="back"]').disabled = !hist.canBack(here);
        nb.querySelector('[data-nav="fwd"]').disabled = !hist.canForward(here);
    }

    // ── Back, Forward, up ───────────────────────────────────────────────
    const mark = () => hist.mark(at(), scroll.scrollTop);

    /// Shows a place: one from the history, or a step of the head's path.
    function show(place, top = 0) {
        closeMenu();
        // The section left is kept as it is, to be found so again.
        const to = secs.includes(place.tab) ? place.tab : 'albums';
        if (to !== S.tab && !TOOLS.includes(S.tab)) {
            mem.keep(S.tab, { at: at(), top: scroll.scrollTop, list: listScroll, title: S.albumTitle, up: S.upFolder ?? null });
        }
        Object.assign(S, { artist: null, genre: null, flt: null, album: null, folder: null, playlist: null, q: '', picking: false, pickAt: null }, place);
        if (!secs.includes(S.tab)) S.tab = 'albums';
        if (TOOLS.includes(S.tab)) Object.assign(S, { album: null, q: '' });
        page = null;
        input.value = S.q;
        save();
        emit({ type: 'section', section: S.tab, layout });
        return render().then(() => reach(top));
    }
    /// Scrolls to `top`, drawing more of a long list first where it is not
    /// drawn that far yet.
    function reach(top) {
        scroll.scrollTop = top;
        const node = scroll.firstElementChild;
        for (let i = 0; i < 200 && top && scroll.scrollTop < top - 1 && node?.__more?.(CHUNK); i++) scroll.scrollTop = top;
        if (top && scroll.scrollTop < top - 1) requestAnimationFrame(() => { scroll.scrollTop = top; });
    }
    function go(place) { mark(); show(place); }
    function goBack() {
        const s = hist.back(at(), scroll.scrollTop);
        if (s) show(s.at, s.top);
        return !!s;
    }
    function goForward() {
        const s = hist.forward(at(), scroll.scrollTop);
        if (s) show(s.at, s.top);
        return !!s;
    }
    /// One step up from what is open: the album's list, the playlists, the
    /// folder above, all the albums. False at a section's own page.
    function up() {
        const a = at();
        let to = null;
        if (a.album) to = { ...a, album: null };
        else if (a.q) to = { ...a, q: '' };
        else if (a.tab === 'playlists' && a.playlist != null) to = { ...a, playlist: null };
        else if (a.tab === 'folders' && a.folder != null) to = { ...a, folder: S.upFolder ?? null };
        else if (a.artist) to = { ...a, artist: null };
        if (!to) return false;
        const top = a.album ? listScroll : 0;
        mark();
        show(to, top);
        return true;
    }

    function paintChips() {
        chips.replaceChildren();
        const chip = (text, on, cls, tap) => {
            const b = button('ml-chip ' + (cls || ''), text);
            b.classList.toggle('on', on);
            b.addEventListener('click', tap);
            chips.append(b);
        };
        chip(LIB_TEXTS.all, !S.genre && !S.flt, '', () => { S.genre = null; S.flt = null; render(); });
        chip(LIB_TEXTS.hiRes, S.flt === 'hi', 'hr', () => { S.flt = S.flt === 'hi' ? null : 'hi'; render(); });
        chip(LIB_TEXTS.dsd, S.flt === 'dsd', 'dsd', () => { S.flt = S.flt === 'dsd' ? null : 'dsd'; render(); });
        for (const g of S.genres.slice(0, wide ? 12 : 8)) {
            chip(g.name, S.genre === g.name, '', () => { S.genre = S.genre === g.name ? null : g.name; render(); });
        }
    }

    /// The tool's page shown, if one is: it is left (`__leave`) before
    /// another page takes its place, and a refresh of the same tool is its
    /// own (`__refresh`) — the page is not built again.
    let toolNode = null;
    const leaveTool = () => { try { toolNode?.__leave?.(); } catch (e) { console.warn(e); } toolNode = null; };
    const toolCtx = { toast: t => toast(t), menu: (at, items) => openMenu(at, items), status: () => playerStatus(), layout };

    async function render({ keepScroll = false } = {}) {
        const my = ++seq;
        paintChrome();
        const top = keepScroll ? scroll.scrollTop : 0;
        const tool = TOOLS.includes(S.tab) ? S.tab : null;
        if (tool && keepScroll && toolNode?.__tool === tool && toolNode.isConnected) { toolNode.__refresh?.(); return; }
        let node;
        try {
            if (tool) { node = await TOOL_PAGES[tool](toolCtx); node.__tool = tool; }
            else if (S.album) node = await albumPage(S.album);
            else if (S.q) node = await searchView();
            else if (S.tab === 'albums') node = await albumsView(keepScroll);
            else if (S.tab === 'artists') node = await artistsView();
            else if (S.tab === 'genres') node = genresView();
            else if (S.tab === 'folders') node = await folderView();
            else node = playlistsView();
        } catch (e) {
            node = el('div', 'ml-empty', String(e?.message || e));
        }
        if (my !== seq || destroyed) { if (node.__tool) { try { node.__leave?.(); } catch (_) {} } return; }
        io?.disconnect();
        io = null;
        leaveTool();
        if (node.__tool) toolNode = node;
        scroll.replaceChildren(node);
        node.__after?.();
        scroll.scrollTop = top;
        markPlaying();
    }

    // ── albums ──────────────────────────────────────────────────────────
    async function albumsView(keepScroll) {
        const q = albumQuery({ genre: S.genre, artist: S.artist, hiRes: S.flt === 'hi', dsd: S.flt === 'dsd', sort: S.sort, dupes: S.dupes });
        const albums = sortAlbums(await lib('lib_albums', q), S.sort);
        cnt.textContent = plural(albums.length, 'album');
        if (!albums.length) return emptyView(S.genre || S.flt || S.artist);
        return albumsBox(albums, keepScroll ? Math.max(CHUNK, scroll.querySelectorAll('[data-id]').length) : CHUNK);
    }

    /// Albums as tiles or a list, drawn `first` at a time and more as the
    /// list scrolls near its end.
    function albumsBox(albums, first = CHUNK) {
        let box;
        let add;
        if (S.view === 'grid') {
            box = el('div', 'ml-grid');
            add = a => box.append(tile(a));
        } else if (wide) {
            box = el('table', 'ml-table');
            box.innerHTML = '<thead><tr><th></th><th>Album</th><th>Artist</th><th>Year</th><th>Genre</th><th>Tracks</th><th>Length</th><th>Format</th><th>Added</th></tr></thead><tbody></tbody>';
            const body = box.querySelector('tbody');
            add = a => body.append(tableRow(a));
        } else {
            box = el('div', 'ml-rows');
            add = a => box.append(listRow(a));
        }
        let shown = 0;
        const more = n => {
            const end = Math.min(albums.length, shown + n);
            for (; shown < end; shown++) add(albums[shown]);
            return shown < albums.length;
        };
        more(first);
        if (shown < albums.length) {
            const wrap = el('div');
            const tail = el('div', 'ml-tail');
            wrap.append(box, tail);
            wrap.__more = n => {
                const left = more(n);
                if (!left) { io?.disconnect(); tail.remove(); }
                markPlaying();
                return left;
            };
            wrap.__after = () => {
                io = new IntersectionObserver(es => {
                    if (!es.some(e => e.isIntersecting)) return;
                    if (!more(CHUNK)) { io?.disconnect(); tail.remove(); }
                    markPlaying();
                }, { root: scroll, rootMargin: '600px' });
                io.observe(tail);
            };
            return wrap;
        }
        return box;
    }

    function tile(a) {
        const t = el('div', 'ml-alb');
        t.dataset.id = a.id;
        t.dataset.folder = a.folder || '';
        const c = cover(a.cover, a.title);
        const eq = el('i', 'ml-eq');
        eq.innerHTML = '<b></b><b></b><b></b>';
        const pl = button('ml-pl', null, LIB_TEXTS.play);
        pl.innerHTML = ICON.play;
        pl.dataset.playAlbum = a.id;
        const more = button('ml-tmore', null, LIB_TEXTS.more);
        more.innerHTML = ICON.more;
        more.dataset.moreAlbum = a.id;
        c.append(eq, more, pl);
        if (a.copies) c.append(copiesBadge(a.copies, a.copyFolders || []));
        const s = el('div', 'ml-as');
        s.append(el('span', 'ml-aa', a.artist || '—'), fmtTag(a.tag, a.hiRes, a.dsd));
        t.append(c, el('div', 'ml-at', a.title), s);
        t.title = `${a.title}${a.artist ? ' — ' + a.artist : ''}`;
        return t;
    }

    function listRow(a) {
        const r = el('div', 'ml-lrow');
        r.dataset.id = a.id;
        r.dataset.folder = a.folder || '';
        const mid = el('div', 'ml-mid');
        mid.append(el('div', 'ml-t1', a.title), el('div', 'ml-t2', [a.artist, a.year, plural(a.tracks, 'track')].filter(Boolean).join(' · ')));
        r.append(cover(a.cover, initials(a.title), 'ml-th'), mid, fmtTag(a.tag, a.hiRes, a.dsd));
        if (a.copies) mid.querySelector('.ml-t1').append(copiesBadge(a.copies, a.copyFolders || []));
        return r;
    }

    function tableRow(a) {
        const r = el('tr');
        r.dataset.id = a.id;
        r.dataset.folder = a.folder || '';
        const td = (text, cls) => { const d = el('td', cls, text); r.append(d); return d; };
        td(null, 'ml-tdc').append(cover(a.cover, initials(a.title), 'ml-th'));
        const nm = td(a.title, 'nm');
        if (a.copies) nm.append(copiesBadge(a.copies, a.copyFolders || []));
        td(a.artist || '');
        td(a.year || '', 'num');
        td((a.genres || []).join(', '));
        td(String(a.tracks ?? ''), 'num');
        td(time(a.durationS), 'num');
        td(null).append(fmtTag(a.tag, a.hiRes, a.dsd));
        td(addedText(a.added), 'num');
        return r;
    }

    function emptyView(filtered) {
        const s = libStatus() || {};
        const box = el('div', 'ml-empty');
        if (s.scanning) {
            box.append(el('div', null, LIB_TEXTS.reading), el('div', 'ml-dim', s.total ? `${s.done} of ${s.total}` : ''));
        } else if (!(s.roots || []).length) {
            box.append(el('div', null, LIB_TEXTS.noFolder));
            const b = button('ml-btn pri', LIB_TEXTS.addFolder);
            b.addEventListener('click', () => addFolderNow(b));
            box.append(b);
        } else {
            box.append(el('div', null, filtered ? LIB_TEXTS.noFilter : LIB_TEXTS.noMusic));
        }
        return box;
    }

    // ── an album's page ─────────────────────────────────────────────────
    let page = null;   // { a, paths, convBtn, mark, list } of the album shown

    async function albumPage(id) {
        const a = await lib('lib_album', { id });
        emit({ type: 'album', id, layout });
        const paths = albumPaths(a);
        S.albumTitle = a.title || '';
        paintPath();
        const box = el('div', 'ml-album');
        box.dataset.folder = a.folder || '';
        const back = button('ml-back', `‹ ${(S.q ? LIB_TEXTS.searchTitle : S.artist || LIB_TEXTS.tabs[S.tab]).toUpperCase()}`);
        back.addEventListener('click', up);
        const hero = el('div', 'ml-hero');
        const bg = el('div', 'ml-hbg');
        const src = coverUrl(a.cover);
        if (src) bg.style.backgroundImage = `url("${src}")`;
        const meta = el('div', 'ml-hmeta');
        meta.append(el('div', 'ml-hk', LIB_TEXTS.album.toUpperCase()), el('div', 'ml-ht', a.title));
        const by = button('ml-ha', a.artist || '');
        by.addEventListener('click', () => { if (a.artist) openArtist(a.artist); });
        meta.append(by);
        const m = el('div', 'ml-hm');
        m.append(el('span', null, albumLine(a)), fmtTag(a.tag, a.hiRes, a.dsd));
        meta.append(m);

        const acts = el('div', 'ml-acts');
        const play = button('ml-btn pri', null, LIB_TEXTS.playTip);
        play.innerHTML = `${ICON.play}<span>${LIB_TEXTS.play}</span>`;
        play.addEventListener('click', () => playAlbum(paths, {}));
        const shuf = button('ml-btn', null, LIB_TEXTS.shuffleTip);
        shuf.innerHTML = `${ICON.shuffle}<span>${LIB_TEXTS.shuffle}</span>`;
        shuf.addEventListener('click', () => playAlbum(paths, { shuffle: true }));
        const queue = button('ml-btn', null, LIB_TEXTS.queueTip);
        queue.innerHTML = `${ICON.plus}<span>Queue</span>`;
        queue.addEventListener('click', () => toast(LIB_TEXTS.added(queuePaths(paths), addTarget().name)));
        const toPl = button('ml-btn', null, LIB_TEXTS.playlistTip);
        toPl.innerHTML = `${ICON.plus}<span>Playlist</span>`;
        playlistMenu(toPl, a, paths);
        const cv = el('span', 'ml-cvt');
        const convBtn = button('ml-btn cv2 ml-cvgo');
        const fmtBtn = button('ml-btn cv2 ml-cvfmt', '▾', LIB_TEXTS.formatTip);
        cv.append(convBtn, fmtBtn);
        const mark = el('span', 'ml-notnow');
        const grow = el('span', 'ml-grow');
        acts.append(play, shuf, queue, toPl, grow, cv, mark);
        // The buttons are the hero's own row: under the cover and the words
        // in the compact window, beside the cover in the wide one.
        hero.append(bg, cover(a.cover, a.title, 'ml-hcv'), meta, acts);

        const list = el('div', 'ml-tracks');
        const discs = new Set(a.list.map(t => t.discNo)).size > 1;
        let disc = null;
        a.list.forEach((t, i) => {
            if (discs && t.discNo !== disc) { disc = t.discNo; list.append(el('div', 'ml-disc', `DISC ${disc || '?'}`)); }
            const r = el('div', 'ml-trk');
            r.dataset.path = t.path;
            const no = el('span', 'ml-no');
            no.append(el('span', 'ml-non', String(t.trackNo || i + 1)));
            no.insertAdjacentHTML('beforeend', `${ICON.play}<i class="ml-eq"><b></b><b></b><b></b></i>`);
            r.append(no);
            const n = el('span', 'ml-tn', t.title);
            if (t.artist && t.artist !== a.artist) n.append(el('span', 'ml-tsub', ` — ${t.artist}`));
            const x2 = trackCopies(r, t);
            if (x2) n.append(x2);
            r.append(n);
            if (wide) r.append(fmtTag(t.tag, t.hiRes));
            r.append(el('span', 'ml-td', time(t.durationS)));
            const menu = pt => trackMenu(pt, { path: t.path, tag: t.tag, play: () => playAlbum(paths, { at: i }) });
            r.append(moreButton(menu));
            r.__menu = menu;
            r.addEventListener('click', () => playAlbum(paths, { at: i }));
            list.append(r);
        });
        box.append(back, hero, list);

        page = { a, paths, convBtn, mark };
        paintConvert();
        convBtn.addEventListener('click', () => convertNow(convBtn));
        const pick = createPickList(fmtBtn, {
            className: 'pl-outfmt-list ml-pick',
            onPick: v => {
                const heard = planFor(a).heard;
                if (v === heard) S.chosen.delete(a.id); else S.chosen.set(a.id, v);
                paintConvert();
            },
            onToggle: open => {
                if (!open) return;
                // At once with what is known, then with the converter's
                // modulators (asked once).
                const fill = mods => {
                    const p = planFor(a, S.chosen.get(a.id) || null);
                    const items = formatMenu(mods, p.heard, { fs: rackFs(), srcRate: rateOfTag(a.tag) });
                    pick.setItems(items.map(i => (i.sep ? i : { value: i.value, text: i.text, cls: i.heard ? 'ml-heard' : '' })), p.value);
                };
                fill(null);
                converterModulators().then(fill, () => {});
            },
        });
        return box;
    }

    /// The Convert button as the player is now: what comes out, and the
    /// mark when that is not what plays.
    function paintConvert() {
        if (!page) return;
        const plan = planFor(page.a, S.chosen.get(page.a.id) || null);
        const text = `⇄ ${plan.text}`;
        if (page.convBtn.textContent !== text) page.convBtn.textContent = text;
        page.convBtn.dataset.tip = plan.tip;
        page.mark.textContent = plan.mark;
        page.mark.hidden = !plan.differs;
        page.mark.dataset.tip = plan.tip;
    }

    async function convertNow(b) {
        if (!page) return;
        const plan = planFor(page.a, S.chosen.get(page.a.id) || null);
        b.disabled = true;
        try {
            const r = await convertPaths(page.paths, plan.value);
            if (r.busy) toast(LIB_TEXTS.convBusy);
            else if (r.unsupported) toast(LIB_TEXTS.convNoMod);
            else if (r.started) toast(LIB_TEXTS.converting(r.count));
        } catch (e) {
            toast(String(e?.message || e));
        } finally {
            b.disabled = false;
        }
    }

    function playlistMenu(btn, a, paths) {
        const NEW = '__new';
        const pick = createPickList(btn, {
            className: 'pl-outfmt-list ml-pick',
            onPick: v => {
                let id = Number(v);
                let name;
                if (v === NEW) { id = createPlaylist(a.title); name = a.title; }
                else name = playlists().find(p => p.id === id)?.name || '';
                toast(LIB_TEXTS.added(addToPlaylist(id, paths), name));
            },
            onToggle: open => {
                if (!open) return;
                const items = playlists().map(p => ({ value: String(p.id), text: `${p.name} · ${p.count}` }));
                items.push({ sep: true }, { value: NEW, text: LIB_TEXTS.newPlaylist(a.title) });
                pick.setItems(items, null);
            },
        });
    }

    async function playAlbum(paths, opts) {
        const ok = await playPaths(paths, opts);
        if (!ok) toast(LIB_TEXTS.playFailed);
        markPlaying();
    }

    // ── the menus: an album's, a track's ───────────────────────────────
    /// The rows that add `paths` to a playlist (`name` for a new one).
    function playlistRows(paths, name) {
        const rows = [{ group: LIB_TEXTS.mToPlaylist }];
        for (const p of playlists()) {
            rows.push({ text: p.name, hint: String(p.count), tap: () => toast(LIB_TEXTS.added(addToPlaylist(p.id, paths), p.name)) });
        }
        rows.push({
            text: LIB_TEXTS.mNewPlaylist, cls: 'dim',
            tap: () => { const id = createPlaylist(name); toast(LIB_TEXTS.added(addToPlaylist(id, paths), name)); },
        });
        return rows;
    }

    async function convertSome(paths, tag) {
        const plan = planFor({ tag });
        try {
            const r = await convertPaths(paths, plan.value);
            if (r.busy) toast(LIB_TEXTS.convBusy);
            else if (r.unsupported) toast(LIB_TEXTS.convNoMod);
            else if (r.started) toast(LIB_TEXTS.converting(r.count));
        } catch (e) { toast(String(e?.message || e)); }
    }

    function trackMenu(pt, { path, tag, play, name }) {
        const title = name || path.split(/[\\/]/).pop().replace(/\.[a-z0-9]{2,5}$/i, '');
        openMenu(pt, [
            { text: LIB_TEXTS.mPlay, cls: 'pri', tap: play },
            { text: LIB_TEXTS.mQueue, tap: () => toast(LIB_TEXTS.added(queuePaths([path]), addTarget().name)) },
            { sep: true },
            ...playlistRows([path], title),
            { sep: true },
            { text: LIB_TEXTS.mConvert(planFor({ tag }).text.replace(/^Convert\s*→\s*/, '')), cls: 'cv', tap: () => convertSome([path], tag) },
        ]);
    }

    function albumMenu(pt, id) {
        const withAlbum = fn => async () => {
            try { const a = await lib('lib_album', { id }); await fn(a, albumPaths(a)); } catch (e) { toast(String(e?.message || e)); }
        };
        const base = [
            { text: LIB_TEXTS.mPlayAlbum, cls: 'pri', tap: withAlbum((a, paths) => playAlbum(paths, {})) },
            { text: LIB_TEXTS.mShuffle, tap: withAlbum((a, paths) => playAlbum(paths, { shuffle: true })) },
            { text: LIB_TEXTS.mQueue, tap: withAlbum((a, paths) => toast(LIB_TEXTS.added(queuePaths(paths), addTarget().name))) },
            { text: LIB_TEXTS.mOpen, tap: () => openAlbum(id) },
            { sep: true },
            { group: LIB_TEXTS.mToPlaylist },
            ...playlists().map(p => ({
                text: p.name, hint: String(p.count),
                tap: withAlbum((a, paths) => toast(LIB_TEXTS.added(addToPlaylist(p.id, paths), p.name))),
            })),
            {
                text: LIB_TEXTS.mNewPlaylist, cls: 'dim',
                tap: withAlbum((a, paths) => { const n = createPlaylist(a.title); toast(LIB_TEXTS.added(addToPlaylist(n, paths), a.title)); }),
            },
        ];
        openMenu(pt, base);
    }

    /// The row's "more" button: the row's menu under it.
    function moreButton(menu) {
        const b = button('ml-more', null, LIB_TEXTS.more);
        b.innerHTML = ICON.more;
        b.addEventListener('click', e => { e.stopPropagation(); menu(b); });
        return b;
    }

    function openAlbum(id) {
        if (!S.album) listScroll = scroll.scrollTop;
        mark();
        S.albumTitle = '';
        S.album = id;
        render();
    }

    function openArtist(name) {
        go({ tab: 'albums', artist: name });
    }

    // ── artists, genres ─────────────────────────────────────────────────
    async function artistsView() {
        const list = await lib('lib_artists', S.dupes ? { dupes: true } : {});
        if (!list.length) return emptyView();
        const box = el('div', 'ml-arts');
        for (const a of list) {
            const b = el('div', 'ml-art');
            b.append(cover(a.cover, initials(a.name), 'ml-ci'), el('div', 'ml-n', a.name), el('div', 'ml-c', plural(a.albums, 'album')));
            b.addEventListener('click', () => openArtist(a.name));
            box.append(b);
        }
        return box;
    }

    function genresView() {
        if (!S.genres.length) return emptyView();
        const box = el('div', 'ml-gens');
        for (const g of S.genres) {
            const b = el('div', 'ml-gen');
            const bg = el('div', 'ml-gbg');
            const src = coverUrl(g.cover);
            if (src) bg.style.backgroundImage = `url("${src}")`;
            const ov = el('div', 'ml-gov');
            ov.append(el('div', 'ml-gn', g.name), el('div', 'ml-gc', `${plural(g.albums, 'album')} · ${plural(g.tracks, 'track')}`));
            b.append(bg, ov);
            b.addEventListener('click', () => go({ tab: 'albums', genre: g.name }));
            box.append(b);
        }
        return box;
    }

    async function loadGenres() {
        try { S.genres = await lib('lib_genres', S.dupes ? { dupes: true } : {}); } catch (_) { S.genres = []; }
        if (albumsList()) paintChips();
    }

    // ── search ──────────────────────────────────────────────────────────
    async function searchView() {
        const r = await lib('lib_search', S.dupes ? { q: S.q, dupes: true } : { q: S.q });
        const box = el('div', 'ml-found');
        if (!r.albums.length && !r.artists.length && !r.tracks.length) {
            box.append(el('div', 'ml-empty', LIB_TEXTS.nothing(S.q)));
            return box;
        }
        if (r.artists.length) {
            box.append(el('div', 'ml-sec', `${LIB_TEXTS.artists.toUpperCase()} · ${r.artists.length}`));
            const row = el('div', 'ml-chips ml-wrap');
            for (const a of r.artists.slice(0, 16)) {
                const b = button('ml-chip', a.name);
                b.addEventListener('click', () => openArtist(a.name));
                row.append(b);
            }
            box.append(row);
        }
        if (r.albums.length) {
            box.append(el('div', 'ml-sec', `${LIB_TEXTS.albums.toUpperCase()} · ${r.albums.length}`));
            box.append(albumsBox(r.albums, 60));
        }
        if (r.tracks.length) {
            box.append(el('div', 'ml-sec', `${LIB_TEXTS.tracks.toUpperCase()} · ${r.tracks.length}`));
            const list = el('div', 'ml-tracks');
            for (const t of r.tracks) {
                const row = el('div', 'ml-trk');
                row.dataset.path = t.path;
                const n = el('span', 'ml-tn', t.title);
                const x2 = trackCopies(row, t);
                if (x2) n.append(x2);
                n.append(el('span', 'ml-tsub', ` — ${[t.artist, t.album].filter(Boolean).join(' · ')}`));
                const menu = pt => trackMenu(pt, { path: t.path, tag: t.tag, name: t.title, play: () => playAlbum([t.path], {}) });
                row.append(n, fmtTag(t.tag, t.hiRes), el('span', 'ml-td', time(t.durationS)), moreButton(menu));
                row.__menu = menu;
                row.addEventListener('click', () => playAlbum([t.path], {}));
                list.append(row);
            }
            box.append(list);
        }
        return box;
    }

    // ── folders ─────────────────────────────────────────────────────────
    async function folderView() {
        if (S.picking) return pickView();
        const v = await lib('lib_browse', { path: S.folder });
        S.upFolder = S.folder == null || v.atRoot ? null : (v.parent ?? null);
        const box = el('div', 'ml-folders');
        const crumb = el('div', 'ml-crumb');
        if (S.folder == null) crumb.append(el('span', null, LIB_TEXTS.musicFolders), el('span', 'ml-fhint', LIB_TEXTS.folderHint));
        else {
            const upb = button('ml-back', `‹ ${LIB_TEXTS.musicFolders.toUpperCase()}`);
            upb.addEventListener('click', () => go({ tab: 'folders' }));
            const fd = button('ml-lk ml-fdupes', LIB_TEXTS.findDupes, LIB_TEXTS.findDupesTip);
            fd.addEventListener('click', () => toDupes(v.path || S.folder));
            crumb.append(upb, el('span', 'ml-path', v.path || S.folder), fd);
        }
        box.append(crumb);
        const rows = el('div', 'ml-ftree');
        const row = (cls, name, aside, tap) => {
            const b = el('div', 'ml-fr ' + cls);
            if (cls === 'dir') b.insertAdjacentHTML('beforeend', ICON.folder);
            b.append(el('span', 'ml-fname', name));
            if (aside) b.append(el('span', 'ml-fm', aside));
            if (tap) b.addEventListener('click', tap);
            rows.append(b);
            return b;
        };
        if (S.folder != null) row('up', '..', null, up);
        for (const d of v.dirs || []) {
            const r = row('dir', S.folder == null ? d.path : d.name, null, () => go({ tab: 'folders', folder: d.path }));
            if (S.folder == null) {
                const x = button('ml-fx', LIB_TEXTS.remove);
                x.addEventListener('click', async e => {
                    e.stopPropagation();
                    if (!(await confirmAt(x, { text: LIB_TEXTS.removeAsk(d.path), yes: LIB_TEXTS.remove }))) return;
                    try { await removeFolder(d.path); } catch (err) { toast(String(err?.message || err)); }
                    render();
                });
                r.append(x);
            }
        }
        const files = v.files || [];
        if (files.length) {
            const paths = files.map(f => f.path);
            row('act', LIB_TEXTS.playFolder, plural(files.length, 'track'), () => playAlbum(paths, {}));
            files.forEach((f, i) => {
                const play = () => playAlbum(paths, { at: i });
                const r = row('file', f.name, null, play);
                r.dataset.path = f.path;
                if (f.copies) r.querySelector('.ml-fname').append(copiesBadge(f.copies, [], { inline: () => toDupes(S.folder) }));
                r.__menu = pt => trackMenu(pt, { path: f.path, tag: '', play });
                r.append(moreButton(r.__menu));
            });
        }
        box.append(rows);
        if (S.folder == null) {
            const acts = el('div', 'ml-acts');
            const add = button('ml-btn pri', LIB_TEXTS.addFolder);
            add.addEventListener('click', () => addFolderNow(add));
            const again = button('ml-btn', LIB_TEXTS.rescan);
            again.addEventListener('click', async () => { try { await rescan(); } catch (e) { toast(String(e?.message || e)); } });
            acts.append(add, again);
            box.append(acts);
        }
        return box;
    }

    /// The streamer's remote has no system picker: the computer's folders
    /// are walked here (names only, `lib_pick`) and one is taken.
    async function pickView() {
        const at = S.pickAt ?? null;
        const v = await lib('lib_pick', { path: at });
        const box = el('div', 'ml-folders');
        const crumb = el('div', 'ml-crumb');
        const back = button('ml-back', `‹ ${LIB_TEXTS.musicFolders.toUpperCase()}`);
        back.addEventListener('click', () => { S.picking = false; S.pickAt = null; render(); });
        crumb.append(back, el('span', 'ml-path', at ? `${LIB_TEXTS.pickIn} ${at}` : LIB_TEXTS.pickTop));
        box.append(crumb);
        const rows = el('div', 'ml-ftree');
        const row = (cls, name, tap) => {
            const b = el('div', 'ml-fr ' + cls);
            if (cls === 'dir') b.insertAdjacentHTML('beforeend', ICON.folder);
            b.append(el('span', 'ml-fname', name));
            b.addEventListener('click', tap);
            rows.append(b);
            return b;
        };
        const open = path => { S.pickAt = path; render().then(() => { scroll.scrollTop = 0; }); };
        if (at != null) {
            row('up', '..', () => open(v.parent ?? null));
            row('act', LIB_TEXTS.pickThis, async () => {
                try {
                    await lib('lib_add_root', { path: at });
                    emit({ type: 'roots' });
                    S.picking = false; S.pickAt = null;
                    render();
                } catch (e) { toast(String(e?.message || e)); }
            });
        }
        for (const d of v.dirs || []) row('dir', d.name || d.path, () => open(d.path));
        box.append(rows);
        return box;
    }

    async function addFolderNow(b) {
        if (!window.__TAURI__?.dialog?.open || window.__AURA_REMOTE__) {
            go({ tab: 'folders', picking: true });
            return;
        }
        if (b) b.disabled = true;
        try {
            if (await addFolder()) render();
        } catch (e) {
            toast(String(e?.message || e));
        } finally {
            if (b) b.disabled = false;
        }
    }

    // ── playlists (F's own) ─────────────────────────────────────────────
    /// Delete the playlist `p`, asked first on a large card under `anchor`
    /// (a finger finds its buttons: the remote will show this very page).
    async function askDeletePlaylist(p, anchor) {
        const can = canRemovePlaylist(p.id);
        if (can !== 'ok') { toast(can === 'busy' ? LIB_TEXTS.delPlaylistBusy : LIB_TEXTS.delPlaylistLast); return; }
        const ok = await confirmAt(anchor, {
            text: LIB_TEXTS.delPlaylistAsk(p.name, p.count), yes: LIB_TEXTS.delPlaylistYes, no: 'Cancel', big: true,
        });
        if (!ok) return;
        if (!removePlaylist(p.id)) { toast(LIB_TEXTS.delPlaylistLast); return; }
        toast(LIB_TEXTS.delPlaylistDone(p.name));
        show({ tab: 'playlists' });
    }
    function delButton(p, cls) {
        const b = button(cls, null, LIB_TEXTS.delPlaylistTip);
        b.innerHTML = cls.includes('ml-btn') ? `${ICON.bin}<span>${LIB_TEXTS.delPlaylist}</span>` : ICON.bin;
        b.addEventListener('click', e => { e.stopPropagation(); askDeletePlaylist(p, b); });
        return b;
    }
    function playlistsView() {
        const box = el('div', 'ml-pls');
        if (S.playlist != null) {
            const p = playlists().find(x => x.id === S.playlist);
            if (!p) { S.playlist = null; return playlistsView(); }
            const back = button('ml-back', `‹ ${LIB_TEXTS.tabs.playlists.toUpperCase()}`);
            back.addEventListener('click', up);
            // The playlist's head, as an album's: its mark, its name, Play.
            const hero = el('div', 'ml-hero ml-plhero');
            const mk = el('div', 'ml-hcv ml-plmark');
            mk.innerHTML = ICON.plist;
            const meta = el('div', 'ml-hmeta');
            meta.append(el('div', 'ml-hk', LIB_TEXTS.playlistKind.toUpperCase()), el('div', 'ml-ht', p.name), el('div', 'ml-hm', plural(p.count, 'track')));
            const acts = el('div', 'ml-acts');
            const play = button('ml-btn pri');
            play.innerHTML = `${ICON.play}<span>${LIB_TEXTS.play}</span>`;
            play.addEventListener('click', async () => { if (!(await playPlaylist(p.id))) toast(LIB_TEXTS.playFailed); });
            const show = button('ml-btn', wide ? LIB_TEXTS.showInQueue : LIB_TEXTS.showInFiles);
            show.addEventListener('click', () => { showPlaylist(p.id); emit({ type: 'files', playlist: p.id, layout }); });
            acts.append(play, show);
            if (playlists().length > 1) acts.append(delButton(p, 'ml-btn ml-danger'));
            hero.append(el('div', 'ml-hbg'), mk, meta, acts);
            box.append(back, hero);
            const list = el('div', 'ml-tracks');
            const tracks = playlistTracks(p.id);
            if (!tracks.length) list.append(el('div', 'ml-empty', LIB_TEXTS.emptyPlaylist));
            tracks.forEach((t, i) => {
                const r = el('div', 'ml-trk');
                r.dataset.path = t.path;
                const no = el('span', 'ml-no');
                no.append(el('span', 'ml-non', String(i + 1)));
                no.insertAdjacentHTML('beforeend', `${ICON.play}<i class="ml-eq"><b></b><b></b><b></b></i>`);
                r.append(no, el('span', 'ml-tn', String(t.name || t.path).replace(/\.[a-z0-9]{2,5}$/i, '')));
                r.addEventListener('click', async () => { if (!(await playPlaylist(p.id, i))) toast(LIB_TEXTS.playFailed); });
                list.append(r);
            });
            box.append(list);
            return box;
        }
        const rows = el('div', 'ml-ftree ml-plrows');
        for (const p of playlists()) {
            const r = el('div', 'ml-fr pl' + (p.active ? ' on' : ''));
            const mk = el('span', 'ml-plic');
            mk.innerHTML = ICON.plist;
            const pb = button('ml-rowplay', null, LIB_TEXTS.play);
            pb.innerHTML = ICON.play;
            pb.addEventListener('click', async e => { e.stopPropagation(); if (!(await playPlaylist(p.id))) toast(LIB_TEXTS.playFailed); });
            r.append(mk, el('span', 'ml-fname', p.name),
                el('span', 'ml-fm', `${plural(p.count, 'track')}${p.active ? ' · ' + (wide ? LIB_TEXTS.inQueue : LIB_TEXTS.inFiles) : ''}`), pb);
            r.addEventListener('click', () => go({ tab: 'playlists', playlist: p.id }));
            if (playlists().length > 1) r.append(delButton(p, 'ml-rowdel'));
            rows.append(r);
        }
        if (!rows.children.length) rows.append(el('div', 'ml-empty', LIB_TEXTS.noPlaylists));
        box.append(rows);
        return box;
    }

    // ── what plays ──────────────────────────────────────────────────────
    let playing = null;
    function markPlaying() {
        playing = playingPath(playerStatus());
        for (const t of scroll.querySelectorAll('[data-folder]')) t.classList.toggle('playing', playsFrom(t.dataset.folder, playing));
        for (const t of scroll.querySelectorAll('[data-path]')) t.classList.toggle('playing', t.dataset.path === playing);
    }
    const playTimer = setInterval(() => {
        if (destroyed || !root.isConnected || !root.offsetParent) return;
        const now = playingPath(playerStatus());
        if (now !== playing) markPlaying();
        paintConvert();
    }, 1500);

    // ── the scan ────────────────────────────────────────────────────────
    let lastAlbums = null;
    let refreshSoon = 0;
    function onStatus(s, was) {
        scanText.textContent = scanLine(s);
        scanText.title = (s.roots || []).join('\n');
        dot.classList.toggle('busy', !!s.scanning);
        paintDupt();
        const ended = was && was.scanning && !s.scanning;
        if (ended || !S.genres.length) loadGenres();
        const changed = lastAlbums !== null && s.albums !== lastAlbums;
        lastAlbums = s.albums;
        if ((ended || changed) && albumsList() && !refreshSoon) {
            // While it reads, the grid fills in as the albums come, not on
            // every status.
            refreshSoon = setTimeout(() => { refreshSoon = 0; if (albumsList()) render({ keepScroll: true }); }, ended ? 0 : 2500);
        }
    }

    // ── the controls ────────────────────────────────────────────────────
    /// A section, at its own first page: pressed where it already is (in an
    /// album, a playlist, a folder, an artist's albums), it goes back to its
    /// list. `folder`: straight into that music folder.
    /// A section pressed (its tab, or its item in the side): shown as it
    /// was left; the one shown stays as it is. `folder` opens that folder,
    /// `root` the section's own page.
    function setSection(name, { folder = null, root = false } = {}) {
        if (!secs.includes(name)) return;
        const t = sectionTarget(S.tab, name, mem.recall(name), { folder, root });
        if (!t) { emit({ type: 'section', section: S.tab, layout }); return; }
        mark();
        if (t.kept) {
            listScroll = t.kept.list || 0;
            S.albumTitle = t.kept.title || '';
            S.upFolder = t.kept.up ?? undefined;
        }
        show(t.at, t.top);
    }

    let typing = 0;
    input.addEventListener('input', () => {
        clearTimeout(typing);
        typing = setTimeout(() => {
            const q = input.value.trim();
            if (q === S.q && !S.album) return;
            // One step back from a search, however many letters it took.
            if (!S.q || S.album) mark();
            S.q = q;
            S.album = null;
            page = null;
            render();
        }, 200);
    });
    input.addEventListener('keydown', e => {
        if (e.key === 'Escape') {
            e.stopPropagation();
            clearTimeout(typing);
            input.value = '';
            if (S.q) { S.q = ''; render(); }
            input.blur();
        }
    });
    $('.ml-navb').addEventListener('click', e => {
        const b = e.target.closest('[data-nav]');
        if (!b) return;
        if (b.dataset.nav === 'back') goBack(); else goForward();
    });
    for (const b of vt.children) {
        b.addEventListener('click', e => {
            if (e.detail > 0) b.blur();
            if (S.view === b.dataset.v) return;
            S.view = b.dataset.v;
            save();
            render();
        });
    }
    zoom.querySelector('input').addEventListener('input', e => {
        S.tile = Number(e.target.value);
        root.style.setProperty('--tile', S.tile + 'px');
        save();
    });
    artX.addEventListener('click', () => go({ tab: 'albums', genre: S.genre, flt: S.flt }));
    const sortPick = createPickList(sortb, {
        className: 'pl-outfmt-list ml-pick',
        onPick: v => { S.sort = v; save(); render(); },
        onToggle: open => { if (open) sortPick.setItems(Object.entries(SORTS).map(([value, text]) => ({ value, text })), S.sort); },
    });
    $('.ml-lk').addEventListener('click', () => setSection('folders', { root: true }));

    // Album tiles, rows and their play buttons, by delegation.
    scroll.addEventListener('click', async e => {
        const more = e.target.closest('[data-more-album]');
        if (more) { e.stopPropagation(); albumMenu(more, more.dataset.moreAlbum); return; }
        const pl = e.target.closest('[data-play-album]');
        if (pl) {
            e.stopPropagation();
            try {
                const a = await lib('lib_album', { id: pl.dataset.playAlbum });
                playAlbum(albumPaths(a), {});
            } catch (err) { toast(String(err?.message || err)); }
            return;
        }
        const t = e.target.closest('[data-id]');
        if (t && scroll.contains(t)) openAlbum(t.dataset.id);
    });
    // A press of a button leaves no focus on it: Space stays the player's.
    const blurButtons = e => { if (e.detail > 0 && e.target.closest?.('button')) e.target.closest('button').blur(); };
    root.addEventListener('click', blurButtons);
    // The rack hears its own changes (dropzone.js re-renders the rows on
    // any change in the panel): the library's are not the rack's.
    root.addEventListener('change', e => e.stopPropagation());
    // The right button: the album's or the track's menu.
    scroll.addEventListener('contextmenu', e => {
        const row = e.target.closest('.ml-trk, .ml-fr.file');
        const alb = e.target.closest('[data-id]');
        if (row?.__menu) { e.preventDefault(); row.__menu({ x: e.clientX, y: e.clientY }); }
        else if (alb && scroll.contains(alb)) { e.preventDefault(); albumMenu({ x: e.clientX, y: e.clientY }, alb.dataset.id); }
    });
    const typingNow = () => /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || '');
    const onKey = e => {
        if (!root.offsetParent) return;
        // Back and Forward, as in a browser.
        if (e.altKey && !e.ctrlKey && !e.metaKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
            e.preventDefault();
            if (e.key === 'ArrowLeft') goBack(); else goForward();
            return;
        }
        if (typingNow()) return;
        if (e.key === 'Backspace' && !e.ctrlKey && !e.metaKey && !e.altKey) {
            if (goBack() || up()) e.preventDefault();
            return;
        }
        if (e.key !== '/' || e.ctrlKey || e.metaKey || e.altKey) return;
        e.preventDefault();
        input.focus();
    };
    document.addEventListener('keydown', onKey);
    // The mouse's own Back and Forward buttons.
    const onMouse = e => {
        if ((e.button !== 3 && e.button !== 4) || !root.offsetParent) return;
        e.preventDefault();
        if (e.button === 3) goBack(); else goForward();
    };
    document.addEventListener('mouseup', onMouse);

    let toastTimer = 0;
    function toast(text) {
        toastBox.textContent = text;
        toastBox.hidden = false;
        clearTimeout(toastTimer);
        toastTimer = setTimeout(() => { toastBox.hidden = true; }, 3200);
    }

    const offNews = onMediaLibrary(ev => {
        if (ev.type === 'status') onStatus(ev.status, ev.was);
        else if (ev.type === 'toast' && wide) toast(ev.text);
        else if (ev.type === 'roots') { loadGenres(); if (S.tab === 'folders' || albumsList()) render(); }
        else if (ev.type === 'select' && wide) setSection(ev.section, { folder: ev.folder ?? null, root: !!ev.root });
        else if (ev.type === 'pick') go({ tab: 'folders', picking: true });
    });
    const offStatus = watchStatus();

    // A browser mounted again (the window turned) finds its section as it
    // was left.
    const back0 = TOOLS.includes(S.tab) ? null : mem.recall(S.tab);
    if (back0?.at) {
        listScroll = back0.list || 0;
        S.albumTitle = back0.title || '';
        S.upFolder = back0.up ?? undefined;
        show({ ...back0.at, tab: S.tab }, back0.top || 0);
    } else render();

    function destroy() {
        if (!TOOLS.includes(S.tab)) mem.keep(S.tab, { at: at(), top: scroll.scrollTop, list: listScroll, title: S.albumTitle, up: S.upFolder ?? null });
        destroyed = true;
        leaveTool();
        clearInterval(playTimer);
        clearTimeout(refreshSoon);
        clearTimeout(toastTimer);
        io?.disconnect();
        offNews();
        offStatus();
        document.removeEventListener('keydown', onKey);
        document.removeEventListener('mouseup', onMouse);
        closeMenu();
        root.remove();
        hbar?.remove();
    }

    return {
        setSection,
        refresh: () => render({ keepScroll: true }),
        destroy,
        /// The section shown.
        section: () => S.tab,
    };
}
