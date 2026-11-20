// The live remote on the site's Aura Streamer slide: the streamer's own web
// remote (demo/remote/, the real pages) runs in frames of this page, and what
// the streamer would answer comes from here — the simulated player of the
// app's demo (sim.js), silent, and a small library of invented albums
// (demo/remote/demo-lib.json, generated covers). Every frame on the page is a
// device on the same streamer: they share the player, the lists and the rack.

import { createSim } from './sim.js?v=200-r1';

// What the pages keep (their settings and lists), for this visit only: in
// memory, shared by the frames, never in the browser's own storage (the app
// demo on the same page keeps its own there).
function memStore() {
  const data = Object.create(null);
  const api = {
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => { data[k] = String(v); },
    removeItem: (k) => { delete data[k]; },
    clear: () => { for (const k of Object.keys(data)) delete data[k]; },
    key: (i) => Object.keys(data)[i] ?? null,
  };
  return new Proxy(data, {
    get: (t, k) => (k === 'length' ? Object.keys(t).length : k in api ? api[k] : t[k]),
    set: (t, k, v) => { t[k] = String(v); return true; },
    deleteProperty: (t, k) => { delete t[k]; return true; },
  });
}

const words = (q) => String(q || '').toLowerCase().split(/\s+/).filter(Boolean);
const has = (hay, w) => w.every((x) => hay.includes(x));
const fmtTag = (t) => {
  if (t.dsd) return t.dsd;
  const k = t.sampleRate / 1000;
  return `${t.bits}/${Number.isInteger(k) ? k : k.toFixed(1)}`;
};
const hiRes = (t) => !!t.dsd || (t.bits > 16 && t.sampleRate >= 44100) || (t.bits >= 16 && t.sampleRate > 48000);

// The streamer's library index (library_index.rs), over the demo's tracks.
function library(tracks, base) {
  const byId = new Map();
  for (const t of tracks) {
    let a = byId.get(t.albumId);
    if (!a) byId.set(t.albumId, a = { id: t.albumId, list: [] });
    a.list.push(t);
  }
  const albums = [...byId.values()].map((a) => {
    a.list.sort((x, y) => x.discNo - y.discNo || x.trackNo - y.trackNo || x.path.localeCompare(y.path));
    const first = a.list[0];
    const best = a.list.reduce((m, t) => (t.sampleRate * 100 + t.bits > m.sampleRate * 100 + m.bits ? t : m));
    const genres = [];
    for (const t of a.list) for (const g of String(t.genre).split(/[;\0]/).map((s) => s.trim()).filter(Boolean)) {
      if (!genres.some((x) => x.toLowerCase() === g.toLowerCase())) genres.push(g);
    }
    const folder = first.path.slice(0, first.path.lastIndexOf('\\'));
    const out = {
      id: a.id, title: first.album, artist: first.albumArtist || first.artist, year: first.year, genres,
      tracks: a.list.length, durationS: a.list.reduce((s, t) => s + t.durationS, 0), tag: fmtTag(best), hiRes: hiRes(best),
      dsd: !!best.dsd, added: Math.max(...a.list.map((t) => t.added)), cover: `${base}covers/${a.id}.jpg`, folder,
    };
    a.json = out;
    a.hay = `${out.title} ${out.artist} ${out.year} ${genres.join(' ')} ${a.list.map((t) => `${t.title} ${t.artist} ${t.composer}`).join(' ')}`.toLowerCase();
    return a;
  });
  const trackJson = (t) => ({
    path: t.path, title: t.title, artist: t.artist, album: t.album, trackNo: t.trackNo, discNo: t.discNo,
    durationS: t.durationS, tag: fmtTag(t), hiRes: hiRes(t),
  });
  const key = (s) => String(s).toLowerCase();
  const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  function artists(q) {
    const w = words(q), by = new Map();
    for (const a of albums) {
      const n = a.json.artist;
      if (!n || !has(key(n), w)) continue;
      const e = by.get(key(n)) || { name: n, albums: 0, tracks: 0, cover: null };
      e.albums++; e.tracks += a.json.tracks; e.cover = e.cover || a.json.cover;
      by.set(key(n), e);
    }
    return [...by.entries()].sort((x, y) => cmp(x[0], y[0])).map((x) => x[1]);
  }
  const roots = [{ name: 'Music', path: 'M:\\Music' }];
  return {
    albumOf: (path) => albums.find((a) => a.list.some((t) => t.path === path)),
    invoke(cmd, args = {}) {
      switch (cmd) {
        case 'lib_status': return { scanning: false, done: tracks.length, total: tracks.length, scanned: true, error: null,
          tracks: tracks.length, albums: albums.length, roots: roots.map((r) => r.path) };
        case 'lib_albums': {
          const w = words(args.q);
          const list = albums.filter((a) => has(a.hay, w))
            .filter((a) => !args.genre || a.json.genres.some((g) => key(g) === key(args.genre)))
            .filter((a) => !args.artist || key(a.json.artist) === key(args.artist))
            .filter((a) => !args.hiRes || a.json.hiRes).filter((a) => !args.dsd || a.json.dsd)
            .map((a) => a.json);
          const s = args.sort || 'added';
          list.sort(s === 'title' ? (a, b) => cmp(key(a.title), key(b.title))
            : s === 'artist' ? (a, b) => cmp(key(a.artist), key(b.artist)) || cmp(a.year, b.year) || cmp(key(a.title), key(b.title))
            : s === 'year' ? (a, b) => cmp(b.year, a.year) || cmp(key(a.title), key(b.title))
            : (a, b) => b.added - a.added || cmp(key(a.title), key(b.title)));
          return list;
        }
        case 'lib_album': {
          const a = byId.get(args.id);
          if (!a) throw 'No such album';
          return { ...a.json, list: a.list.map(trackJson) };
        }
        case 'lib_artists': return artists(args.q);
        case 'lib_genres': {
          const by = new Map();
          for (const a of albums) for (const g of a.json.genres) {
            const e = by.get(key(g)) || { name: g, albums: 0, tracks: 0, cover: null };
            e.albums++; e.tracks += a.json.tracks; e.cover = e.cover || a.json.cover;
            by.set(key(g), e);
          }
          return [...by.values()].sort((x, y) => y.albums - x.albums || cmp(key(x.name), key(y.name)));
        }
        case 'lib_search': {
          const w = words(args.q);
          if (!w.length) return { albums: [], artists: [], tracks: [] };
          return {
            albums: albums.filter((a) => has(a.hay, w)).slice(0, 48).map((a) => a.json),
            artists: artists(args.q),
            tracks: tracks.filter((t) => has(`${t.title} ${t.artist} ${t.album} ${t.composer}`.toLowerCase(), w)).slice(0, 100).map(trackJson),
          };
        }
        // the folders: the music folder, its albums, their files
        case 'lib_browse': {
          const p = args.path;
          if (p == null) return { path: null, parent: null, dirs: roots, files: [] };
          if (p === roots[0].path) {
            return { path: p, parent: null, atRoot: true, files: [],
              dirs: albums.map((a) => ({ name: a.json.folder.split('\\').pop(), path: a.json.folder }))
                .sort((x, y) => cmp(key(x.name), key(y.name))) };
          }
          const a = albums.find((x) => x.json.folder === p);
          if (!a) throw 'No such folder';
          return { path: p, parent: roots[0].path, atRoot: false, dirs: [],
            files: a.list.map((t) => ({ name: t.path.split('\\').pop(), path: t.path, size: Math.round(t.durationS * 44100 * 2.1) })) };
        }
        case 'lib_pick': case 'lib_add_root': case 'lib_remove_root':
          throw 'Music folders are chosen on the streamer itself — not in this demo';
        case 'lib_rescan': return null;
        default: return undefined;
      }
    },
  };
}

let host = null;

/** Readies the shared streamer once (its library comes over the network). */
export function remoteHost() {
  if (host) return host;
  const base = new URL('demo/remote/', location.href).href;
  const store = memStore();
  host = {
    store, base, ready: null, sim: null, lib: null,
    // a word from the demo (a function it leaves to the full streamer)
    say: (text) => window.dispatchEvent(new CustomEvent('aura-remote-say', { detail: text })),
  };
  host.ready = fetch(base + 'remote/demo-lib.json').then((r) => r.json()).then((tracks) => {
    const lib = library(tracks, base + "remote/");
    // the list it starts with: one album, the newest
    const first = lib.invoke('lib_albums', {})[0];
    const start = lib.invoke('lib_album', { id: first.id }).list;
    const entries = start.map((t) => ({ path: t.path, name: t.path.split('\\').pop(), convs: [] }));
    const sim = createSim({ tracks, store, entries });
    host.sim = sim; host.lib = lib;
    host.invoke = async (cmd, args) => {
      if (/^lib_/.test(cmd)) return lib.invoke(cmd, args || {});
      if (cmd === 'update_check') return { enabled: true, available: false, current: '2.0.1', version: '', tag: '', notes: '', url: '', error: null };
      return sim.invoke(cmd, args || {});
    };
    // the cover of a track in the list (the player's own route)
    host.coverOf = (id) => {
      const t = sim.infos[id - 1], a = t && lib.albumOf(t.path);
      return a ? a.json.cover : null;
    };
    sim.host = { say: host.say, glow() {}, front() {}, broken() {} };
    return host;
  });
  window.__auraRemoteHost = host;
  return host;
}
