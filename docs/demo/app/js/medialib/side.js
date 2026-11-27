// ══════════════════════════════════════════════════════════════════════
// The landscape window's left column for the library: the sections (Albums,
// Artists, Genres, Folders, Playlists, with their counts) and the music
// folders — each with the scan's state, Add folder, read them again. A
// section chosen here is told to the wide browser (api.js 'select'); the
// browser's own changes light the section here ('section').
// ══════════════════════════════════════════════════════════════════════

import { lib, emit, onMediaLibrary, watchStatus, addFolder, rescan } from './api.js';
import { SECTIONS, TOOLS, plural } from './model.js';
import { playlists } from './play.js';
import { LIB_TEXTS } from './view.js';

const ICONS = {
    albums: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="2.5"/></svg>',
    artists: '<svg viewBox="0 0 24 24"><circle cx="12" cy="8" r="4"/><path d="M4 21c1-4.5 4.5-7 8-7s7 2.5 8 7"/></svg>',
    genres: '<svg viewBox="0 0 24 24"><path d="M9 18V5l11-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="17" cy="16" r="3"/></svg>',
    folders: '<svg viewBox="0 0 24 24"><path d="M3 6h7l2 2h9v11H3z"/></svg>',
    playlists: '<svg viewBox="0 0 24 24"><path d="M4 6h12M4 11h12M4 16h7"/><path d="M17 14v6l4-3z"/></svg>',
    conversions: '<svg viewBox="0 0 24 24"><path d="M4 8h14l-3.5-3.5M20 16H6l3.5 3.5"/></svg>',
    dupes: '<svg viewBox="0 0 24 24"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"/></svg>',
    analyzer: '<svg viewBox="0 0 24 24"><path d="M3 17l4-9 3 6 3-4 3 5 2-8 3 6"/><path d="M3 21h18"/></svg>',
    visualizer: '<svg viewBox="0 0 24 24"><path d="M3 12c2.500-7 4.500-7 6 0s3.500 7 6 0 3.500-7 6 0"/></svg>',
};

const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
};

/// "311 albums · 978 tracks · 37 copies hidden": the unique ones counted.
function totalLine(s) {
    const c = s.copies || 0;
    const albums = (s.albums || 0) - (s.albumCopies || 0);
    return `${plural(albums, 'album')} · ${plural((s.tracks || 0) - c, 'track')}${c ? ` · ${c} ${c === 1 ? 'copy' : 'copies'} hidden` : ''}`;
}

/// Mounts the side into `host`. Returns { setSection(name), destroy() }.
export function mountLibrarySide(host) {
    const root = el('div', 'ml-side');
    root.innerHTML = `<div class="ml-sh">LIBRARY</div><div class="ml-sitems"></div>
      <div class="ml-sh">MUSIC FOLDERS<button type="button" class="ml-sadd" data-tip="${LIB_TEXTS.addFolderShort}">+</button></div>
      <div class="ml-sfolders"></div>
      <div class="ml-sacts"><button type="button" class="ml-lk ml-srescan">${LIB_TEXTS.rescan}</button><button type="button" class="ml-lk ml-smanage">⚙ ${LIB_TEXTS.manage}</button></div>`;
    const items = root.querySelector('.ml-sitems');
    const folders = root.querySelector('.ml-sfolders');
    const counts = {};
    let current = 'albums';
    try { current = JSON.parse(localStorage.getItem('auraMediaLib.wide') || '{}').tab || 'albums'; } catch (_) {}

    for (const s of [...SECTIONS, ...TOOLS]) {
        if (s === TOOLS[0]) items.append(el('div', 'ml-sh ml-sh-in', LIB_TEXTS.tools));
        const b = el('button', 'ml-si');
        b.type = 'button';
        b.dataset.sec = s;
        b.innerHTML = ICONS[s];
        b.append(el('span', 'ml-sl', LIB_TEXTS.tabs[s]), el('span', 'ml-sn'));
        b.addEventListener('click', e => {
            if (e.detail > 0) b.blur();
            setSection(s);
            emit({ type: 'select', section: s });
        });
        items.append(b);
        counts[s] = b.querySelector('.ml-sn');
    }

    function setSection(name) {
        current = name;
        for (const b of items.children) b.classList.toggle('on', b.dataset.sec === name);
    }

    async function loadCounts(s) {
        counts.albums.textContent = s?.albums ?? '';
        counts.playlists.textContent = playlists().length;
        counts.dupes.textContent = s?.copies || '';
        try { counts.artists.textContent = (await lib('lib_artists', {})).length; } catch (_) {}
        try { counts.genres.textContent = (await lib('lib_genres')).length; } catch (_) {}
    }

    function paintFolders(s) {
        folders.replaceChildren();
        const roots = s?.roots || [];
        if (!roots.length) {
            folders.append(el('div', 'ml-fold-none', LIB_TEXTS.noFolder));
        }
        for (const r of roots) {
            const f = el('div', 'ml-fold');
            f.title = r;
            // The folder's own name first; where it is, under it.
            const name = String(r).replace(/[\\/]+$/, '').split(/[\\/]/).pop() || r;
            f.append(el('div', 'ml-fp', name));
            if (name !== r) f.append(el('div', 'ml-fpath', r));
            const m = s.scanning
                ? (s.total ? `reading ${s.done} of ${s.total}` : 'looking for music')
                : totalLine(s);
            if (s.scanning || roots.length === 1) f.append(el('div', 'ml-fmeta', m));
            if (s.scanning && s.total) {
                const bar = el('div', 'ml-fbar');
                const i = el('i');
                i.style.width = `${Math.round((100 * (s.done || 0)) / s.total)}%`;
                bar.append(i);
                f.append(bar);
            }
            f.addEventListener('click', () => { setSection('folders'); emit({ type: 'select', section: 'folders', folder: r }); });
            folders.append(f);
        }
        if (roots.length > 1 && !s.scanning) {
            folders.append(el('div', 'ml-fmeta ml-ftot', totalLine(s)));
        }
    }

    root.querySelector('.ml-sadd').addEventListener('click', async e => {
        if (e.detail > 0) e.currentTarget.blur();
        try { await addFolder(); } catch (err) { console.warn(err); }
    });
    root.querySelector('.ml-srescan').addEventListener('click', () => rescan().catch(() => {}));
    root.querySelector('.ml-smanage').addEventListener('click', () => { setSection('folders'); emit({ type: 'select', section: 'folders', root: true }); });

    let last = null;
    const off = onMediaLibrary(ev => {
        if (ev.type === 'status') {
            const s = ev.status;
            paintFolders(s);
            if (!last || last.albums !== s.albums || last.copies !== s.copies || (ev.was?.scanning && !s.scanning)) loadCounts(s);
            last = s;
        } else if (ev.type === 'section' && ev.layout === 'wide') {
            setSection(ev.section);
            counts.playlists.textContent = playlists().length;   // one made or deleted
        }
    });
    const offStatus = watchStatus();
    host.append(root);
    setSection(current);

    return {
        setSection,
        destroy() { off(); offStatus(); root.remove(); },
    };
}
