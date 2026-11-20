// The streamer's library on the remote's left: search, the tabs Albums /
// Artists / Genres / Folders / Playlists, genre and HI-RES / DSD filters, the
// album grid with covers. The server reads the music folders' tags and covers
// (apps/streamer/src/library*.rs); playing goes into the app's own list, as a
// file dropped on it would.
//
// remote.js calls buildLibrary(root, { setSub }) once; everything inside `root`
// is this file's.

import { addFiles, listHooks, clearList } from '../js/dropzone.js';
import { state } from '../js/state.js';
import { getPlaylists, getActiveId, switchPlaylist } from '../js/library.js';
import { auraUrl } from '../js/aura-url.js';

const invoke = (...a) => window.__AURA_REMOTE__.invoke(...a);
const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
};
const keep = {
    get(k, d) { try { return localStorage.getItem('remote.lib.' + k) ?? d; } catch (_) { return d; } },
    set(k, v) { try { localStorage.setItem('remote.lib.' + k, v); } catch (_) {} },
};
const TABS = ['albums', 'artists', 'genres', 'folders', 'playlists'];
const SORTS = { added: 'Recently added', title: 'Title', artist: 'Artist', year: 'Year' };
const plural = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
const dirOf = (p) => p.replace(/[\\/][^\\/]*$/, '');
const time = (s) => {
    s = Math.round(s || 0);
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${x}` : `${m}:${x}`;
};

let ui = null;

export function buildLibrary(root, opts = {}) {
    if (!document.querySelector('link[href="remote/library.css"]')) {
        const css = el('link'); css.rel = 'stylesheet'; css.href = 'remote/library.css';
        document.head.append(css);
    }
    root.classList.add('lb');
    root.innerHTML = `
      <label class="lb-search"><svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/></svg>
        <input type="search" placeholder="Search artists, albums, tracks, composers" autocomplete="off" spellcheck="false"><kbd>/</kbd></label>
      <nav class="lb-tabs"></nav>
      <div class="lb-chips"></div>
      <div class="lb-bar"><span class="lb-sortlbl">Sort:</span><span class="lb-sort"><select></select></span>
        <span class="lb-back" hidden></span><span class="lb-views"><button type="button" data-l="grid" aria-label="Grid">▦</button><button type="button" data-l="list" aria-label="List">☰</button></span></div>
      <div class="lb-body"></div>`;
    ui = {
        root,
        setSub: opts.setSub || (() => {}),
        input: root.querySelector('.lb-search input'),
        tabs: root.querySelector('.lb-tabs'),
        chips: root.querySelector('.lb-chips'),
        bar: root.querySelector('.lb-bar'),
        sort: root.querySelector('.lb-sort select'),
        back: root.querySelector('.lb-back'),
        body: root.querySelector('.lb-body'),
        tab: TABS.includes(keep.get('tab')) ? keep.get('tab') : 'albums',
        layout: keep.get('layout', 'grid'),
        sortBy: keep.get('sort', 'added'),
        q: '',
        genre: null, hiRes: false, dsd: false,
        artist: null, album: null,
        genres: [],
        status: null,
        playing: null,
    };
    for (const t of TABS) {
        const b = el('button', null, t); b.type = 'button'; b.dataset.tab = t;
        b.addEventListener('click', () => { ui.artist = null; ui.album = null; top(); setTab(t); });
        ui.tabs.append(b);
    }
    for (const [k, v] of Object.entries(SORTS)) {
        const o = el('option', null, v); o.value = k; ui.sort.append(o);
    }
    ui.sort.value = ui.sortBy;
    ui.sort.addEventListener('change', () => { ui.sortBy = ui.sort.value; keep.set('sort', ui.sortBy); render(); });
    ui.bar.querySelectorAll('.lb-views button').forEach((b) => b.addEventListener('click', () => {
        ui.layout = b.dataset.l; keep.set('layout', ui.layout); render();
    }));
    let typing = 0;
    ui.input.addEventListener('input', () => {
        clearTimeout(typing);
        typing = setTimeout(() => { ui.q = ui.input.value.trim(); ui.album = null; top(); render(); }, 180);
    });
    ui.input.addEventListener('keydown', (e) => { if (e.key === 'Escape') { ui.input.value = ''; ui.q = ''; render(); ui.input.blur(); } });
    document.addEventListener('keydown', (e) => {
        if (e.key !== '/' || e.ctrlKey || e.metaKey || /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || '')) return;
        if (!root.offsetParent) return;
        e.preventDefault();
        ui.input.focus();
    });
    setTab(ui.tab);
    watchStatus();
    watchPlaying();
}

function setTab(t) {
    ui.tab = t;
    keep.set('tab', t);
    ui.tabs.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.tab === t));
    if (ui.q) { ui.q = ''; ui.input.value = ''; }
    render();
}

// ── the scan's state, in the header ─────────────────────────────────────
async function watchStatus() {
    let was = null;
    const tick = async () => {
        let s;
        try { s = await invoke('lib_status'); } catch (_) { setTimeout(tick, 5000); return; }
        ui.status = s;
        const where = (s.roots || []).join(', ') || 'no music folder yet';
        const what = s.scanning
            ? (s.total ? `reading tags ${s.done} of ${s.total}` : 'looking for music')
            : `${s.albums} albums, ${s.tracks} tracks · tags and covers`;
        ui.setSub(`${where} · ${what}`);
        // A scan that ends shows what it found.
        if (was && !s.scanning) { await loadGenres(); if (!ui.q && ui.tab !== 'folders' && ui.tab !== 'playlists') render(); }
        if (was === null) await loadGenres();
        // While it reads, the grid fills in as the albums come.
        if (s.scanning && was && ui.tab === 'albums' && !ui.album && !ui.q && s.albums !== ui.lastAlbums) render();
        ui.lastAlbums = s.albums;
        was = s.scanning;
        setTimeout(tick, s.scanning ? 1500 : 60000);
    };
    tick();
}

async function loadGenres() {
    try { ui.genres = await invoke('lib_genres'); } catch (_) { ui.genres = []; }
    paintChips();
}

// ── what plays, for the playing album's mark ────────────────────────────
function watchPlaying() {
    // The status comes with the page's own poll (shim.js passes it on); the
    // queue only when the track changes, not every 2 s.
    let seenKey = null;
    window.addEventListener('aura-remote-status', async (e) => {
        const s = e.detail || {};
        const id = s.trackId ?? s.track ?? s.current ?? null;
        const on = s.state === 'playing' || s.state === 'paused';
        const key = on ? String(id) : '';
        if (key === seenKey) return;
        seenKey = key;
        let path = null;
        if (on) {
            try { path = (await invoke('player_queue')).find((x) => x.id === id)?.path ?? null; } catch (_) { seenKey = null; return; }
        }
        if (path === ui.playing) return;
        ui.playing = path;
        markPlaying();
    });
}
function playsFrom(folder, path) {
    if (!path) return false;
    const d = dirOf(path);
    return d === folder || dirOf(d) === folder && /^(cd|dis[ck])[\s_.-]*\d+$/i.test(d.slice(folder.length + 1));
}
function markPlaying() {
    ui.body.querySelectorAll('[data-folder]').forEach((t) => t.classList.toggle('playing', playsFrom(t.dataset.folder, ui.playing)));
    ui.body.querySelectorAll('[data-path]').forEach((t) => t.classList.toggle('playing', t.dataset.path === ui.playing));
}

// ── the views ───────────────────────────────────────────────────────────
let seq = 0;
/// Another list: it starts at its top (the list scrolls in itself on a phone).
const top = () => { if (ui.body) ui.body.scrollTop = 0; };

async function render() {
    const my = ++seq;
    const body = ui.body;
    const albumsLike = !ui.q && (ui.tab === 'albums' || ui.artist) && !ui.album;
    ui.root.dataset.layout = ui.layout;
    ui.chips.hidden = !(ui.tab === 'albums' && !ui.q && !ui.album && !ui.artist);
    ui.bar.querySelector('.lb-sortlbl').hidden = ui.bar.querySelector('.lb-sort').hidden = !albumsLike;
    ui.bar.querySelector('.lb-views').hidden = !(albumsLike || ui.q);
    ui.bar.querySelectorAll('.lb-views button').forEach((b) => b.classList.toggle('on', b.dataset.l === ui.layout));
    ui.back.hidden = true;
    paintChips();
    let view;
    try {
        if (ui.q) view = await searchView();
        else if (ui.album) view = await albumView(ui.album);
        else if (ui.artist) view = await albumsView({ artist: ui.artist }, ui.artist);
        else if (ui.tab === 'albums') view = await albumsView({ genre: ui.genre, hiRes: ui.hiRes, dsd: ui.dsd });
        else if (ui.tab === 'artists') view = await artistsView();
        else if (ui.tab === 'genres') view = genresView();
        else if (ui.tab === 'folders') view = folderView();
        else view = playlistsView();
    } catch (e) {
        view = el('div', 'lb-empty', String(e));
    }
    if (my !== seq) return;
    body.replaceChildren(view);
    markPlaying();
}

function paintChips() {
    if (!ui) return;
    const c = ui.chips;
    c.replaceChildren();
    const chip = (text, on, cls, tap) => {
        const b = el('button', 'lb-chip ' + (cls || ''), text); b.type = 'button';
        b.classList.toggle('on', on);
        b.addEventListener('click', tap);
        c.append(b);
    };
    chip('All', !ui.genre && !ui.hiRes && !ui.dsd, '', () => { ui.genre = null; ui.hiRes = ui.dsd = false; render(); });
    for (const g of ui.genres.slice(0, 7)) chip(g.name, ui.genre === g.name, '', () => { ui.genre = ui.genre === g.name ? null : g.name; render(); });
    chip('Hi-Res', ui.hiRes, 'hires', () => { ui.hiRes = !ui.hiRes; render(); });
    chip('DSD', ui.dsd, 'dsd', () => { ui.dsd = !ui.dsd; render(); });
}

function backTo(text, tap) {
    ui.back.hidden = false;
    ui.back.replaceChildren();
    const b = el('button', null, '‹ ' + text); b.type = 'button';
    b.addEventListener('click', tap);
    ui.back.append(b);
}

function cover(url, title, cls = 'lb-cover') {
    const box = el('div', cls);
    const ph = () => { box.replaceChildren(el('span', 'lb-ph', title || '')); box.classList.add('none'); };
    if (!url) { ph(); return box; }
    const img = el('img'); img.alt = ''; img.loading = 'lazy'; img.decoding = 'async'; img.src = url;
    img.addEventListener('error', ph, { once: true });
    box.append(img);
    return box;
}

function fmtTag(tag, hi, dsd) {
    const s = el('span', 'lb-fmt', tag);
    if (dsd || /^DSD/.test(tag)) s.classList.add('dsd'); else if (hi) s.classList.add('hi');
    return s;
}

function tile(a) {
    const b = el('button', 'lb-tile'); b.type = 'button';
    b.dataset.folder = a.folder;
    b.append(cover(a.cover, a.title));
    const eq = el('i', 'lb-eq'); eq.innerHTML = '<b></b><b></b><b></b>';
    b.append(eq);
    const meta = el('div', 'lb-meta');
    meta.append(el('div', 'lb-t', a.title));
    const sub = el('div', 'lb-s');
    sub.append(el('span', 'lb-a', a.artist || '—'), fmtTag(a.tag, a.hiRes, a.dsd));
    meta.append(sub);
    b.append(meta);
    b.addEventListener('click', () => { ui.album = a.id; top(); render(); ui.root.scrollIntoView?.({ block: 'nearest' }); });
    return b;
}

function albumList(albums) {
    const box = el('div', 'lb-grid');
    for (const a of albums) box.append(tile(a));
    return box;
}

async function albumsView(filter, artistName) {
    const albums = await invoke('lib_albums', { ...filter, sort: ui.sortBy });
    if (artistName) backTo('Artists', () => { ui.artist = null; render(); });
    if (!albums.length) return emptyView(filter);
    const wrap = el('div');
    if (artistName) wrap.append(el('div', 'lb-h', artistName));
    wrap.append(albumList(albums));
    return wrap;
}

function emptyView(filter) {
    const s = ui.status || {};
    const box = el('div', 'lb-empty');
    if (s.scanning) {
        box.append(el('div', null, 'Reading your music…'), el('div', 'lb-dim', s.total ? `${s.done} of ${s.total} files` : ''));
    } else if (!(s.roots || []).length) {
        box.append(el('div', null, 'No music folder yet.'));
        const b = el('button', 'lb-btn', 'Add a music folder'); b.type = 'button';
        b.addEventListener('click', () => { pickMode = true; setTab('folders'); });
        box.append(b);
    } else if (filter && (filter.genre || filter.hiRes || filter.dsd)) {
        box.append(el('div', null, 'No albums with this filter.'));
    } else {
        box.append(el('div', null, 'No music found in the music folders.'));
    }
    return box;
}

async function albumView(id) {
    const a = await invoke('lib_album', { id });
    backTo(ui.artist || (ui.q ? 'Search' : ui.tab[0].toUpperCase() + ui.tab.slice(1)), () => { ui.album = null; top(); render(); });
    const box = el('div', 'lb-album');
    box.dataset.folder = a.folder;
    const head = el('div', 'lb-ahead');
    head.append(cover(a.cover, a.title, 'lb-cover big'));
    const meta = el('div', 'lb-ameta');
    meta.append(el('div', 'lb-at', a.title));
    const by = el('button', 'lb-aa', a.artist || ''); by.type = 'button';
    by.addEventListener('click', () => { if (!a.artist) return; ui.album = null; top(); ui.artist = a.artist; ui.q = ''; ui.input.value = ''; render(); });
    meta.append(by);
    meta.append(el('div', 'lb-dim', [a.year, plural(a.tracks, 'track'), time(a.durationS), a.genres.join(', ')].filter(Boolean).join(' · ')));
    const tags = el('div', 'lb-row'); tags.append(fmtTag(a.tag, a.hiRes, a.dsd)); meta.append(tags);
    const acts = el('div', 'lb-acts');
    const paths = a.list.map((t) => t.path);
    const play = el('button', 'lb-btn main', '▶ Play'); play.type = 'button';
    play.addEventListener('click', () => playNow(paths, 0));
    const add = el('button', 'lb-btn', '+ Add to queue'); add.type = 'button';
    add.addEventListener('click', () => { queueUp(paths); add.textContent = '✓ Added'; });
    acts.append(play, add);
    meta.append(acts);
    head.append(meta);
    box.append(head);
    const discs = new Set(a.list.map((t) => t.discNo)).size > 1;
    const list = el('div', 'lb-tracks');
    let disc = null;
    a.list.forEach((t, i) => {
        if (discs && t.discNo !== disc) { disc = t.discNo; list.append(el('div', 'lb-disc', `Disc ${disc || '?'}`)); }
        list.append(trackRow(t, String(t.trackNo || i + 1), t.artist !== a.artist ? t.artist : '', () => playNow(paths, i)));
    });
    box.append(list);
    return box;
}

function trackRow(t, num, sub, tap) {
    const r = el('button', 'lb-track'); r.type = 'button';
    r.dataset.path = t.path;
    r.append(el('span', 'lb-n', num));
    const m = el('span', 'lb-tm');
    m.append(el('span', 'lb-tt', t.title));
    if (sub) m.append(el('span', 'lb-ts', sub));
    r.append(m, fmtTag(t.tag, t.hiRes), el('span', 'lb-d', time(t.durationS)));
    r.addEventListener('click', tap);
    return r;
}

async function artistsView() {
    const list = await invoke('lib_artists', {});
    if (!list.length) return emptyView();
    const box = el('div', 'lb-people');
    for (const a of list) {
        const b = el('button', 'lb-person'); b.type = 'button';
        const initials = (a.name.match(/[\p{L}\p{N}]+/gu) || ['?']).slice(0, 2).map((w) => [...w][0]).join('').toUpperCase();
        b.append(cover(a.cover, initials, 'lb-cover round'));
        const m = el('div', 'lb-meta');
        m.append(el('div', 'lb-t', a.name), el('div', 'lb-s', `${plural(a.albums, 'album')} · ${plural(a.tracks, 'track')}`));
        b.append(m);
        b.addEventListener('click', () => { ui.artist = a.name; render(); });
        box.append(b);
    }
    return box;
}

function genresView() {
    if (!ui.genres.length) return emptyView();
    const box = el('div', 'lb-grid genres');
    for (const g of ui.genres) {
        const b = el('button', 'lb-tile genre'); b.type = 'button';
        b.append(cover(g.cover, g.name));
        const m = el('div', 'lb-meta');
        m.append(el('div', 'lb-t', g.name), el('div', 'lb-s', `${plural(g.albums, 'album')} · ${plural(g.tracks, 'track')}`));
        b.append(m);
        b.addEventListener('click', () => { ui.genre = g.name; ui.hiRes = ui.dsd = false; setTab('albums'); });
        box.append(b);
    }
    return box;
}

async function searchView() {
    const r = await invoke('lib_search', { q: ui.q });
    const box = el('div', 'lb-found');
    if (!r.albums.length && !r.artists.length && !r.tracks.length) {
        box.append(el('div', 'lb-empty', `Nothing found for “${ui.q}”.`));
        return box;
    }
    if (r.artists.length) {
        box.append(el('div', 'lb-h', 'Artists'));
        const row = el('div', 'lb-chips');
        for (const a of r.artists.slice(0, 12)) {
            const b = el('button', 'lb-chip', a.name); b.type = 'button';
            b.addEventListener('click', () => { ui.q = ''; ui.input.value = ''; ui.artist = a.name; render(); });
            row.append(b);
        }
        box.append(row);
    }
    if (r.albums.length) {
        box.append(el('div', 'lb-h', 'Albums'));
        box.append(albumList(r.albums));
    }
    if (r.tracks.length) {
        box.append(el('div', 'lb-h', 'Tracks'));
        const list = el('div', 'lb-tracks');
        r.tracks.forEach((t, i) => list.append(trackRow(t, String(i + 1), [t.artist, t.album].filter(Boolean).join(' · '),
            () => playNow([t.path], 0, true))));
        box.append(list);
    }
    return box;
}

// ── the folders (choosing them, and playing a folder as it is) ──────────
let pickMode = false;
function folderView() {
    const box = el('div', 'lb-folders');
    const crumb = el('div', 'lb-crumb');
    const rows = el('div', 'lb-rows');
    box.append(crumb, rows);
    async function open(path) {
        let v;
        try {
            v = await invoke(pickMode ? 'lib_pick' : 'lib_browse', { path: path ?? null });
        } catch (e) {
            rows.textContent = String(e);
            return;
        }
        rows.replaceChildren();
        crumb.textContent = pickMode ? 'Choose a music folder: ' + (v.path || 'this computer') : v.path || 'Music folders';
        const row = (cls, name, tag, tap) => {
            const b = el('button', 'lb-frow ' + cls); b.type = 'button';
            b.append(el('span', 'lb-fname', name));
            if (tag) b.append(el('span', 'lb-ftag', tag));
            b.addEventListener('click', tap);
            rows.append(b);
            return b;
        };
        if (path != null) row('up', '..', null, () => open(v.parent ?? null));
        if (pickMode && path != null) {
            row('act', 'Use this folder', null, async () => {
                await invoke('lib_add_root', { path });
                pickMode = false;
                open(null);
            });
        }
        for (const d of v.dirs) {
            const r = row('dir', d.name, null, () => open(d.path));
            if (path == null && !pickMode) {
                const x = el('span', 'lb-fx', 'Remove'); x.title = 'Remove this music folder from the library (nothing is deleted)';
                x.addEventListener('click', async (e) => {
                    e.stopPropagation();
                    if (!confirm(`Remove ${d.path} from the library? The files stay where they are.`)) return;
                    await invoke('lib_remove_root', { path: d.path });
                    open(null);
                });
                r.append(x);
            }
        }
        if (v.files.length) row('act', 'Play this folder', v.files.length + ' tracks', () => playNow(v.files.map((f) => f.path), 0));
        v.files.forEach((f, i) => row('file', f.name, null, () => playNow(v.files.map((x) => x.path), i)));
        if (path == null && !pickMode) {
            row('act', 'Add a music folder', null, () => { pickMode = true; open(null); });
            row('act', 'Read the folders again', null, async () => { await invoke('lib_rescan'); });
        }
        if (pickMode && path == null) row('act', 'Back', null, () => { pickMode = false; open(null); });
    }
    open(null);
    return box;
}

// ── the app's playlists ─────────────────────────────────────────────────
function playlistsView() {
    const box = el('div', 'lb-rows');
    const active = getActiveId();
    for (const p of getPlaylists()) {
        const n = p.id === active ? state.list.length : (p.entries || []).length;
        const b = el('button', 'lb-frow' + (p.id === active ? ' on' : '')); b.type = 'button';
        b.append(el('span', 'lb-fname', p.name), el('span', 'lb-ftag', `${plural(n, 'track')}${p.id === active ? ' · in Up next' : ''}`));
        const play = el('span', 'lb-fx', '▶'); play.title = 'Play';
        play.addEventListener('click', (e) => {
            e.stopPropagation();
            if (p.id !== active) switchPlaylist(p.id);
            setTimeout(() => listHooks.play?.(state.list[0] ?? null), 300);
        });
        b.append(play);
        b.addEventListener('click', () => { switchPlaylist(p.id); render(); });
        box.append(b);
    }
    if (!box.children.length) box.append(el('div', 'lb-empty', 'No playlists.'));
    return box;
}

// ── into the list ───────────────────────────────────────────────────────
/// The list becomes `paths` (`keepList`: they join it), then `at` plays.
async function playNow(paths, at, keepList = false) {
    if (!keepList) {
        const same = state.list.length === paths.length && state.list.every((e, i) => e.path === paths[i]);
        if (!same) clearList();
    }
    queueUp(paths);
    const want = paths[at];
    for (let i = 0; i < 100; i++) {
        const e = state.list.find((x) => x.path === want);
        if (e && e.trackId != null) { listHooks.play?.(e); return; }
        await new Promise((r) => setTimeout(r, 100));
    }
}
/// What of `paths` is not in the list yet goes to its end.
function queueUp(paths) {
    const inList = new Set(state.list.map((e) => e.path));
    const fresh = paths.filter((p) => !inList.has(p));
    if (fresh.length) addFiles(fresh);
}
