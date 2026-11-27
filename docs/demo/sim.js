// The player behind the live demo. Silent: no audio is decoded or played.
// Each track carries its own measured envelopes (demo/data/*.json, made from
// the real files: peak and RMS per channel and 32 log bands 20 Hz–20 kHz at
// 20 frames a second), and this file answers the app pages' requests from
// them — status, spectrum, the Hybrid-Phase lamp and live strip, the HP
// preparation — the way the engine would, at the track's own clock.

import { createAnalyzer } from './an-sim.js?v=201-3';
// the EQ's own math and state rules, as the EQ window runs them
import { sections, magDb, modeLevel } from './app/js/eq/math.js?v=201-3';
import { normalizeState, eqActive, effectiveState, balanceOf } from './app/js/eq/state.js?v=201-3';

const TRACKS = [
  { slug: 'youre-my-heart', title: "You're My Heart, You're My Soul", track: 1 },
  { slug: 'megamix', title: '30th Anniversary Megamix', track: 2 },
  { slug: 'lonely-tears', title: 'Lonely Tears In Chinatown', track: 3 },
  { slug: 'princess', title: 'Princess Of The Night', track: 4 },
  { slug: 'diamonds', title: 'Diamonds Never Made A Lady', track: 5 },
];
const DURATIONS = { 'youre-my-heart': 198.867, megamix: 2106.9, 'lonely-tears': 211.2, princess: 234.4, diamonds: 244.9 };
const ALBUM = 'Special Hit Edition (2015)', ARTIST = 'Modern Talking';
const SRC_RATE = 44100;
const FS0 = 16, TAPS0 = 2000000001; // the rack the page opens with: FS ×16, Auto S (the shifted grid)
const OUT_RATE = SRC_RATE * FS0;   // the EQ's curve is drawn at the default chain's rate
const PREP_S = 5.6;                // how long Hybrid-Phase prepares a track the first time
const FULL_APP = 'This function works in the full app';
const HP_SENS = [0.45, 0.40, 0.35, 0.30, 0.26, 0.23, 0.20];

// A headphone profile, already chosen: an invented one, "My headphones",
// as an AutoEq import would give it.
// Imported with its preamp, as the EQ window takes one: manual gain, the
// preamp as the output.
const HP_EQ = {
  v: 2, on: true, name: 'My headphones', gain: 'manual', output: -6.3, bands: [
    ['lowShelf', 105, 6.5, 0.70], ['bell', 125, -2.7, 0.55], ['bell', 8445, 3.3, 1.61], ['bell', 522, 0.7, 1.02],
    ['bell', 1298, -1.2, 2.14], ['highShelf', 10000, -3.1, 0.70], ['bell', 3158, -1.8, 3.67], ['bell', 2166, 0.9, 3.32],
    ['bell', 6639, 2.2, 5.82], ['bell', 5433, -1.2, 5.70],
  ].map(([type, f, gain, q]) => ({ type, f, gain, q, slope: 12, on: true })),
};

const pathOf = t => `Modern Talking/Special Hit Edition/${String(t.track).padStart(2, '0')} - ${t.title}.flac`;

// The settings the app pages read at start: every unit of the rack on,
// Hybrid-Phase on with αHP, TFS Phase off, the EQ on with the profile above, XTC with
// a measured triangle.
function seedStorage(store = localStorage, entries = null) {
  const eq = HP_EQ;
  const settings = {
    settingsRev: 2, convFs: '3', convTapCount: TAPS0, convApodizing: '0', convHeadroom: '0',
    convAdaptiveApodizer: true, convHybridPhase: true, convGpuCheck: false, convFirResampling: true,
    convSubsonic: true, convSubsonicHz: '10', convHpSens: 0, convAlbumLevel: true,
    convOutFormat: 'flac', convDsdRate: '256', convDsdModulator: '',
    labFeatures: { declip: true, isp: true, tfsPhase: false, continuousAlpha: true, adaptiveHeadroom: true, cue: true, xtc: true, eq },
    xtcGeometry: { span: 2000, distL: 2200, distR: 2200, headW: 180, mode: 'depth', saved: true },
    eq, eqPresets: [{ name: eq.name, state: eq }],
  };
  const playlists = {
    version: 1, active: 1, plSeq: 2,
    playlists: [{ id: 1, name: 'Playlist 1', entries: entries || TRACKS.map(t => ({ path: pathOf(t), name: `${t.track} - ${t.title}.flac`, convs: [] })) }],
  };
  try {
    const localStorage = store;
    for (const k of Object.keys(localStorage)) if (k.startsWith('aura')) localStorage.removeItem(k);
    localStorage.setItem('auraSettings', JSON.stringify(settings));
    localStorage.setItem('auraPlaylists', JSON.stringify(playlists));
    localStorage.setItem('auraTour', JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' }));
    localStorage.setItem('auraAnSpectrum', JSON.stringify({ mode: 'live', kind: 'instant', avg: 1 }));
    localStorage.setItem('auraTourAnalyzer', JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' }));
    localStorage.setItem('auraTourEq', JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' }));
    localStorage.setItem('auraTourCue', JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' }));
    localStorage.setItem('auraTourSwitch', JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' }));
    localStorage.setItem('auraLibraryIntro', JSON.stringify({ v: 1, at: '2026-11-01T00:00:00Z' }));
  } catch (_) { /* storage blocked: the pages start from their defaults */ }
  return settings;
}

const b64 = s => Uint8Array.from(atob(s), c => c.charCodeAt(0));

// A filter size as the player labels it (filter.rs taps_label).
function tapsLabel(n) {
  if (!(n >= 2500)) return null;
  if (n >= 2e9) return 'Auto';   // the player names its own size from the track
  return n >= 400e6 ? '500M' : n >= 240e6 ? '300M' : n >= 140e6 ? '180M'
    : n >= 65e6 ? '100M' : n >= 25e6 ? '30M' : n >= 7.5e6 ? '10M' : n >= 2.5e6 ? '5M' : n >= 5e5 ? '1M' : '5k';
}
// The rack as the player has it (player_set_settings), from the seeded one
// until the page sends its own.
function rackOf(s) {
  const lf = s.labFeatures;
  return {
    mode: 'aura', fsMultiplier: FS0, taps: s.convTapCount,
    phase: lf.continuousAlpha ? 'alpha' : s.convHybridPhase ? 'hybrid' : lf.tfsPhase ? 'tfs' : 'linear',
    hpSens: 0, adaptiveApodizer: s.convAdaptiveApodizer, subsonicHz: s.convSubsonic ? +s.convSubsonicHz : 0,
    declip: lf.declip, isp: lf.isp, adaptiveHeadroom: lf.adaptiveHeadroom, cue: !!lf.cue, xtc: lf.xtc, eq: s.eq,
  };
}
// The chain's stages in the engine's order (lab/mod.rs), as the status's
// `audible.stages` names them. The EQ is no stage: the output plays it.
function stagesOf(r) {
  const st = [];
  if (r.declip) st.push('DC');
  if (r.isp) st.push('ISP');
  if (r.subsonicHz > 0) st.push('SUB' + Math.round(r.subsonicHz));
  if (r.adaptiveHeadroom) st.push('AHR');
  if (r.adaptiveApodizer) st.push('AA');
  st.push('PFR');
  // αHP runs on HP's pair: the chain names both (chain.rs), so both lamps light
  if (r.phase === 'hybrid') st.push('HP'); else if (r.phase === 'alpha') st.push('HP', 'αHP'); else if (r.phase === 'tfs') st.push('TFS');
  if (r.cue) st.push('CUE');
  if (r.xtc) st.push('XTC');
  return st;
}
const fmtK = hz => (hz / 1000).toFixed(1).replace(/\.0$/, '') + 'k';

// `tracks` (the streamer's demo): a library of its own instead of the album,
// each track drawn with the measured envelopes of a demo track at least as
// long; `store` holds the pages' settings and lists (localStorage by
// default), `entries` the list it starts with.
// The media library over invented tracks (the streamer slide's
// demo/remote/remote/demo-lib.json and its generated covers), answering the
// library's own commands as the app's library index does (lib_call).
const words = q => String(q || '').toLowerCase().split(/\s+/).filter(Boolean);
const hasAll = (hay, w) => w.every(x => hay.includes(x));
const fmtTag = t => {
  if (t.dsd) return t.dsd;
  const k = t.sampleRate / 1000;
  return `${t.bits}/${Number.isInteger(k) ? k : k.toFixed(1)}`;
};
const hiRes = t => !!t.dsd || (t.bits > 16 && t.sampleRate >= 44100) || (t.bits >= 16 && t.sampleRate > 48000);
export function createLibrary(tracks, coverBase) {
  const byId = new Map();
  for (const t of tracks) {
    let a = byId.get(t.albumId);
    if (!a) byId.set(t.albumId, a = { id: t.albumId, list: [] });
    a.list.push(t);
  }
  const key = s => String(s).toLowerCase();
  const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  const albums = [...byId.values()].map(a => {
    a.list.sort((x, y) => x.discNo - y.discNo || x.trackNo - y.trackNo || x.path.localeCompare(y.path));
    const first = a.list[0];
    const best = a.list.reduce((m, t) => (t.sampleRate * 100 + t.bits > m.sampleRate * 100 + m.bits ? t : m));
    const genres = [];
    for (const t of a.list) for (const g of String(t.genre).split(/[;\0]/).map(s => s.trim()).filter(Boolean)) {
      if (!genres.some(x => key(x) === key(g))) genres.push(g);
    }
    a.json = {
      id: a.id, title: first.album, artist: first.albumArtist || first.artist, year: first.year, genres,
      tracks: a.list.length, durationS: a.list.reduce((s, t) => s + t.durationS, 0), tag: fmtTag(best), hiRes: hiRes(best),
      dsd: !!best.dsd, added: Math.max(...a.list.map(t => t.added)), cover: `${coverBase}${a.id}.jpg`,
      folder: first.path.slice(0, first.path.lastIndexOf('\\')),
    };
    a.hay = `${a.json.title} ${a.json.artist} ${a.json.year} ${genres.join(' ')} ${a.list.map(t => `${t.title} ${t.artist} ${t.composer}`).join(' ')}`.toLowerCase();
    return a;
  });
  const trackJson = t => ({ path: t.path, title: t.title, artist: t.artist, album: t.album, trackNo: t.trackNo, discNo: t.discNo,
    durationS: t.durationS, tag: fmtTag(t), hiRes: hiRes(t) });
  const group = (name, of) => {
    const by = new Map();
    for (const a of albums) for (const n of of(a)) {
      const e = by.get(key(n)) || { name: n, albums: 0, tracks: 0, cover: null };
      e.albums++; e.tracks += a.json.tracks; e.cover = e.cover || a.json.cover;
      by.set(key(n), e);
    }
    return [...by.values()];
  };
  const artists = q => group('artist', a => (a.json.artist && hasAll(key(a.json.artist), words(q)) ? [a.json.artist] : []))
    .sort((x, y) => cmp(key(x.name), key(y.name)));
  const root = 'M:\\Music';
  return {
    albumOf: path => albums.find(a => a.list.some(t => t.path === path)),
    invoke(cmd, args = {}) {
      switch (cmd) {
        case 'lib_status': return { scanning: false, done: tracks.length, total: tracks.length, scanned: true, error: null,
          tracks: tracks.length, albums: albums.length, roots: [root] };
        case 'lib_albums': {
          const w = words(args.q), s = args.sort || 'added';
          const list = albums.filter(a => hasAll(a.hay, w))
            .filter(a => !args.genre || a.json.genres.some(g => key(g) === key(args.genre)))
            .filter(a => !args.artist || key(a.json.artist) === key(args.artist))
            .filter(a => !args.hiRes || a.json.hiRes).filter(a => !args.dsd || a.json.dsd).map(a => a.json);
          list.sort(s === 'title' ? (a, b) => cmp(key(a.title), key(b.title))
            : s === 'artist' ? (a, b) => cmp(key(a.artist), key(b.artist)) || cmp(a.year, b.year)
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
        case 'lib_genres': return group('genre', a => a.json.genres).sort((x, y) => y.albums - x.albums || cmp(key(x.name), key(y.name)));
        case 'lib_search': {
          const w = words(args.q);
          if (!w.length) return { albums: [], artists: [], tracks: [] };
          return { albums: albums.filter(a => hasAll(a.hay, w)).slice(0, 48).map(a => a.json), artists: artists(args.q),
            tracks: tracks.filter(t => hasAll(`${t.title} ${t.artist} ${t.album} ${t.composer}`.toLowerCase(), w)).slice(0, 100).map(trackJson) };
        }
        case 'lib_browse': {
          const p = args.path;
          if (p == null) return { path: null, parent: null, dirs: [{ name: 'Music', path: root }], files: [] };
          if (p === root) return { path: p, parent: null, atRoot: true, files: [],
            dirs: albums.map(a => ({ name: a.json.folder.split('\\').pop(), path: a.json.folder })).sort((x, y) => cmp(key(x.name), key(y.name))) };
          const a = albums.find(x => x.json.folder === p);
          if (!a) throw 'No such folder';
          return { path: p, parent: root, atRoot: false, dirs: [],
            files: a.list.map(t => ({ name: t.path.split('\\').pop(), path: t.path, size: Math.round(t.durationS * 44100 * 2.1) })) };
        }
        case 'lib_pick': case 'lib_add_root': case 'lib_remove_root':
          throw 'Music folders are chosen in the full app — not in this demo';
        case 'lib_rescan': return null;
        // The demo library has no copies: the Duplicates page says so.
        case 'lib_dupes': return { groups: [], twins: [], versions: [], roots: [root] };
        case 'lib_trash': return { moved: 0, failed: [] };
        default: return undefined;
      }
    },
  };
}

// The visualization scenes the demo's Visualizer offers (demo/app/vis/):
// the app's own, but those that need the separated instruments.
const VIS_FILES = ['tunnel', 'aurora', 'nebula', 'horizon', 'neon-portal', 'neonwave-sunset', 'night-ridges',
  'phosphorescent-peaks', 'raindrops', 'better-rain', 'bars', 'scope', 'tourbillon'].map(n => n + '.aura-vis');
let visTexts = null;
const visList = () => (visTexts = visTexts || Promise.all(VIS_FILES.map(f => fetch('demo/app/vis/' + f)
  .then(r => (r.ok ? r.text() : null)).then(text => (text ? { file: f, text } : null), () => null)))
  .then(l => l.filter(Boolean)));

export function createSim({ reducedMotion = false, tracks = null, store = localStorage, entries = null, lib = null } = {}) {
  const settings = seedStorage(store, entries);
  const SLUGS = TRACKS.map(t => t.slug);
  const infos = tracks ? tracks.map((t, i) => {
    const fit = SLUGS.filter(sl => DURATIONS[sl] >= t.durationS);
    return { id: i + 1, path: t.path, title: t.title, artist: t.artist, album: t.album, year: t.year || '',
      durationS: t.durationS, sampleRate: t.sampleRate || 44100, bits: t.bits || 16, channels: 2, format: '', list: 1,
      slug: fit[i % fit.length] };
  }) : TRACKS.map((t, i) => ({
    id: i + 1, path: pathOf(t), title: t.title, artist: ARTIST, album: ALBUM, year: '2015',
    durationS: DURATIONS[t.slug], sampleRate: 44100, bits: 16, channels: 2, format: '', list: 1, slug: t.slug,
  }));
  // the play order: the active list as the pages keep it, else every track
  function order() {
    try {
      const pl = JSON.parse(store.getItem('auraPlaylists'));
      const list = pl.playlists.find(x => x.id === pl.active) || pl.playlists[0];
      const ids = list.entries.map(e => infos.find(t => t.path === e.path)).filter(Boolean).map(t => t.id);
      if (ids.length) return ids;
    } catch (_) { /* no list kept: the tracks as they are */ }
    return infos.map(t => t.id);
  }
  const data = new Map();            // slug -> decoded envelopes (or a promise)
  const prepared = new Set();
  let eqState = normalizeState(settings.eq), eqRev = 0;
  let rack = rackOf(settings);
  const trackRacks = new Map();      // track id -> its own rack (M), player_set_track_settings
  // The rack the playing track goes through: BIT-PERFECT plays every track
  // as it is; else a track's own (M), else the rack (controller.rs
  // effective_settings).
  const eff = () => (rack.mode === 'direct' ? rack : trackRacks.get(p.id) || rack);
  const direct = () => eff().mode === 'direct';
  const hpOn = () => !direct() && (eff().phase === 'hybrid' || eff().phase === 'alpha');
  const isAlpha = () => eff().phase === 'alpha';             // αHP: the blend is α, not a switch
  const sensOf = () => Math.max(-3, Math.min(3, eff().hpSens | 0));
  // what plays: the state when on; off with "Keep with EQ off", the speakers alone
  const eqEff = () => effectiveState(eqState);
  const eqLive = () => !direct() && !!eqEff() && (eff().eq ? eff().eq.on !== false : true);
  const p = {
    state: 'stopped', id: null, pos: 0, t0: 0, prepT0: 0, prepId: null, volumeDb: -6, repeat: 'off',
  };
  const listeners = new Map();       // event -> Set(fn)
  const sim = {
    host: null, infos, reducedMotion, visible: true,
    setVisible(v) {
      const was = this.visible; this.visible = v;
      if (v && !was) for (const fn of wake) { try { fn(); } catch (_) { /* a page that has gone */ } }
    },
    onVisible(fn) { wake.add(fn); },
  };
  const wake = new Set();
  // the library (lib_call) and a track's cover: its album's
  sim.lib = lib;
  sim.coverOf = id => {
    const t = infos[id - 1], a = t && lib && lib.albumOf(t.path);
    return a ? a.json.cover : null;
  };
  // the app's windows the pages open: a tool of the Library window where
  // the page has one (demo.js), else the window itself
  const openTool = id => { const h = sim.host; if (h?.tool) h.tool(id); else h?.glow?.(id); return null; };

  // ── track data ─────────────────────────────────────────────────────────
  function load(slug) {
    if (data.has(slug)) return data.get(slug);
    // the engine's own detector, recorded from the track (<slug>.hp.json),
    // when there is one; else an envelope made from the bands
    // a request the server dropped is asked again: a track whose data never
    // came stayed blank in every view (the analyzer drew nothing) until the
    // page was loaded anew
    const json = (url, miss, n = 4) => fetch(url)
      .then(r => (r.ok ? r.json() : r.status === 404 ? miss : Promise.reject(new Error(r.status))))
      .catch(e => (n > 1 ? new Promise(ok => setTimeout(ok, 500)).then(() => json(url, miss, n - 1)) : Promise.reject(e)));
    const hp = json(`demo/data/${slug}.hp.json`, null).catch(() => null);
    const pr = Promise.all([json(`demo/data/${slug}.json`), hp]).then(([j, h]) => {
      const d = { fps: j.fps, nb: j.nbands, frames: j.frames, pk: b64(j.pk), rms: b64(j.rms), bands: b64(j.bands), edges: j.band_edges_hz };
      if (h && h.det) { d.det = b64(h.det); d.detFps = h.fps || d.fps; } else d.onset = onsets(d);
      data.set(slug, d);
      return d;
    }, () => { data.delete(slug); return null; });   // asked for again by the next ready()
    data.set(slug, pr);
    return pr;
  }
  const ready = slug => {
    const d = data.get(slug);
    if (!d) { load(slug); return null; }
    return d instanceof Promise ? null : d;
  };
  // The recorded detector at its own rate (86 Hz), 0..255 a frame, read the
  // engine's way: Catmull-Rom between frames (catmull_env_at)…
  function catmull(a, x) {
    const i = Math.floor(x), t = x - i, g = k => (k < 0 || k >= a.length ? 0 : a[k] / 255);
    const p0 = g(i - 1), p1 = g(i), p2 = g(i + 1), p3 = g(i + 2);
    const v = 0.5 * (2 * p1 + (-p0 + p2) * t + (2 * p0 - 5 * p1 + 4 * p2 - p3) * t * t + (-p0 + 3 * p1 - 3 * p2 + p3) * t * t * t);
    return Math.min(1, Math.max(0, v));
  }
  // …and one strip bin = the max of the curve at its ends and every frame
  // inside it, as PhaseMeter::view sends it
  function detBin(d, ms0, ms1) {
    const a = ms0 / 1000 * d.detFps, z = ms1 / 1000 * d.detFps;
    let v = Math.max(catmull(d.det, a), catmull(d.det, z));
    for (let i = Math.max(0, Math.ceil(a)); i <= Math.floor(z) && i < d.det.length; i++) v = Math.max(v, d.det[i] / 255);
    return v;
  }
  // The onset envelope: rise of the low and mid bands frame to frame, the
  // way an attack detector sees a hit, scaled 0..1.
  function onsets(d) {
    const n = d.frames, nb = d.nb, out = new Float32Array(n);
    let mx = 1e-6;
    for (let f = 1; f < n; f++) {
      let s = 0;
      for (let b = 2; b < 22; b++) {
        const up = d.bands[f * nb + b] - d.bands[(f - 1) * nb + b];
        if (up > 0) s += up * (b < 8 ? 1.6 : 1);
      }
      out[f] = s; if (s > mx) mx = s;
    }
    const sorted = Array.from(out).sort((a, b) => a - b);
    const ref = sorted[Math.floor(n * 0.985)] || mx;
    for (let f = 0; f < n; f++) out[f] = Math.min(1, out[f] / ref * 0.62);
    // a little smoothing: the detector reads a smoothed envelope
    for (let f = n - 1; f > 0; f--) out[f] = Math.max(out[f], out[f - 1] * 0.55);
    return out;
  }

  // ── clock ──────────────────────────────────────────────────────────────
  const now = () => performance.now() / 1000;
  function position() {
    if (p.state === 'playing') return Math.min(p.pos + (now() - p.t0), dur());
    return p.pos;
  }
  const dur = () => (p.id ? infos[p.id - 1].durationS : 0);
  const srcRate = () => (info() ? info().sampleRate : SRC_RATE);
  // FS counts from the family's base rate: ×16 is 705.6k for 44.1k and 88.2k alike
  const baseRate = () => (srcRate() % 48000 === 0 ? 48000 : 44100);
  const info = () => (p.id ? infos[p.id - 1] : null);
  function tick() {
    if (p.state === 'preparing' && now() - p.prepT0 >= PREP_S && ready(info().slug)) {
      prepared.add(p.id); p.state = 'playing'; p.t0 = now();
    }
    if (p.state === 'playing') {
      const pos = position();
      // the next track's lists are made during the music
      const o = order(), k = o.indexOf(p.id);
      if (pos > 15 && k >= 0 && k < o.length - 1) prepared.add(o[k + 1]);
      if (pos >= dur() - 0.05) next(true);
    }
  }
  setInterval(tick, 100);

  function start(id) {
    p.id = id; p.pos = 0;
    load(infos[id - 1].slug);
    // only Hybrid-Phase prepares a track before its sound
    if (prepared.has(id) || !hpOn()) { p.state = 'playing'; p.t0 = now(); }
    else { p.state = 'preparing'; p.prepT0 = now(); p.prepId = id; }
    emit('player-transport', { state: p.state, trackId: id });
  }
  function next(auto) {
    if (!p.id) return;
    const o = order(), k = o.indexOf(p.id);
    if (p.repeat === 'one' && auto) start(p.id);
    else if (k >= 0 && k < o.length - 1) start(o[k + 1]);
    else if (k < 0 && o.length) start(o[0]);
    else if (p.repeat === 'all') start(o[0]);
    else { p.state = 'stopped'; p.pos = 0; if (!auto) p.id = null; }
  }

  // ── commands (core.invoke) ─────────────────────────────────────────────
  const strip = t => { const { slug, ...r } = t; return r; };
  // The track's amplitude for the seek bar (the engine's player_waveform):
  // 1000 slices of peak and RMS, 0..255 over -60..0 dBFS. Made up here, the
  // same each time for a track: intro, verses, choruses, a bridge, an outro.
  const waves = new Map();
  function waveform(id) {
    const t = infos[id - 1];
    if (!t) throw new Error('No such track');
    if (waves.has(id)) return waves.get(id);
    let s = (id * 2654435761) ^ [...(t.path || '')].reduce((h, c) => (h * 31 + c.charCodeAt(0)) | 0, 7);
    const rnd = () => { s = (s + 0x6D2B79F5) | 0; let x = Math.imul(s ^ (s >>> 15), 1 | s); x ^= x + Math.imul(x ^ (x >>> 7), 61 | x); return ((x ^ (x >>> 14)) >>> 0) / 4294967296; };
    const N = 1000;
    // sections: [share of the track, level dBFS]
    const plan = [[0.07, -17], [0.15, -11], [0.12, -5], [0.15, -10], [0.13, -4], [0.1, -14], [0.16, -3.5], [0.12, -9]];
    const tot = plan.reduce((x, p) => x + p[0] * (0.8 + 0.4 * rnd()), 0);
    const ends = []; let acc = 0;
    for (const p of plan) { acc += p[0] * (0.8 + 0.4 * rnd()); ends.push([Math.round((acc / tot) * N), p[1] + (rnd() - 0.5) * 2]); }
    const beat = 6 + Math.floor(rnd() * 5);
    const peak = new Array(N), rms = new Array(N);
    let k = 0;
    for (let i = 0; i < N; i++) {
      while (k < ends.length - 1 && i >= ends[k][0]) k++;
      let db = ends[k][1];
      const prev = k ? ends[k - 1][1] : -40, from = k ? ends[k - 1][0] : 0;
      if (i - from < 8) db = prev + (db - prev) * ((i - from) / 8);       // a short swell into the section
      if (i < 15) db -= (15 - i) * 2.2;                                    // the first notes
      if (i > N - 60) db -= ((i - (N - 60)) / 60) ** 1.6 * 38;             // the outro fades
      db += (i % beat === 0 ? 1.8 : 0) + (rnd() - 0.5) * 3;                 // beats and jitter
      const pb = Math.max(0, Math.min(255, Math.round(((Math.min(db, -0.1) + 60) / 60) * 255)));
      peak[i] = pb;
      rms[i] = Math.max(0, pb - 20 - Math.round(rnd() * 20));
    }
    const w = { peak, rms, durationS: t.durationS };
    waves.set(id, w);
    return w;
  }
  sim.invoke = async (cmd, a = {}) => {
    switch (cmd) {
      case 'player_status': return status();
      case 'player_waveform': await new Promise(ok => setTimeout(ok, 300)); return waveform(+a.id);
      case 'player_queue': return order().map(id => strip(infos[id - 1]));
      case 'player_add': {
        const tracks = (a.paths || []).map(pa => infos.find(t => t.path === pa)).filter(Boolean).map(strip);
        return { tracks, errors: [] };
      }
      case 'player_play':
        if (a && a.id) start(a.id);
        else if (p.state === 'paused') { p.state = 'playing'; p.t0 = now(); }
        else if (!p.id) start(order()[0]);
        else if (p.state === 'stopped') start(p.id);
        return null;
      case 'player_pause': if (p.state === 'playing') { p.pos = position(); p.state = 'paused'; } return null;
      case 'player_stop': p.state = 'stopped'; p.pos = 0; return null;
      case 'player_next': next(false); return null;
      case 'player_prev':
        { const o = order(), k = o.indexOf(p.id);
          if (p.id && position() < 3 && k > 0) start(o[k - 1]); else if (p.id) { p.pos = 0; p.t0 = now(); } }
        return null;
      case 'player_seek':
        p.pos = Math.max(0, Math.min(dur() - 0.2, +a.seconds || 0)); if (p.state === 'playing') p.t0 = now();
        return null;
      case 'player_set_volume': p.volumeDb = +a.db || 0; return null;
      case 'player_set_repeat': p.repeat = a.mode; return null;
      case 'player_set_settings':
        if (Array.isArray(a.rendered)) { rendered.clear(); for (const r of a.rendered) if (r && r.id != null && r.path) rendered.set(r.id, r.path); }
        if (a.settings) rack = { ...rack, ...a.settings };
        // the rack carries the whole EQ state, its on the rack's switch
        if (a.settings?.eq) { eqState = normalizeState(a.settings.eq); eqRev++; }
        return null;
      case 'player_set_track_settings': if (a.id) trackRacks.set(a.id, a.settings); return null;
      case 'player_clear_track_settings': trackRacks.delete(a.id); return null;
      case 'player_set_eq': {
        if (a.eq) eqState = normalizeState(a.eq);
        eqRev++;
        return { active: eqActive(eqState), rate: outRate(), clipRiskDb: 0, gainDb: modeLevel(eqState, FS), eqOverDb: 0 };
      }
      // ── conversion (the converter page's batch and its polls) ──
      case 'convert_files': return conv.start(a);
      case 'get_conversion_progress': return conv.progress();
      case 'get_queue_status': return JSON.stringify(conv.queue(a.knownRev | 0));
      case 'cancel_conversion': conv.cancel(); return null;
      case 'cancel_file': conv.cancelFile(a.idx); return null;
      case 'player_devices': return [{ id: 'demo', name: 'Speakers', isDefault: true }];
      case 'player_paths_exist': return (a.paths || []).map(() => true);
      case 'player_render_checks': return (a.paths || []).map(pa => ({ exists: conv.made.has(pa) || !!versionOf(pa), valid: true, tip: null }));
      // the modulators an ordinary PC runs, DoP up to DSD256 (the Conversions
      // page's Play of a DSD256 version puts the output there)
      case 'player_dsd_capabilities': return { kinds: [['sdm7', 'SDM7', 1, [0.22, 0.12, 0.07]], ['sdm5', 'SDM5', 2, [0.14, 0.08, 0.05]]]
        .map(([kind, label, rank, loads]) => ({ kind, label, rank, rates: [256, 128, 64].flatMap((multiple, i) =>
          [44100, 48000].map(base => ({ multiple, base, state: 'ok', reason: '', load: loads[i] }))) })) };
      case 'dsd_export_modulators': return [];
      case 'check_filters': return JSON.stringify({ ok: true });
      case 'filter_inventory': throw new Error('demo');     // unknown inventory: every filter reads as available
      case 'gpu_status': throw new Error('demo');
      case 'update_check': return { enabled: false, available: false, current: '2.1.1', version: '', tag: '', notes: '', url: '', error: null };
      case 'blind_test_trials': return 0;
      case 'perf_history': return { samples: [] };
      case 'spatial_status': return { busy: false };
      case 'eq_window_open': return openTool('eq');
      case 'xtc_geometry_open': return openTool('xtc');
      case 'cue_window_open': return openTool('cue');
      case 'player_cue_view': return cueView();
      case 'player_set_cue': return null;
      // the media library (the Library window, the compact window's M view)
      case 'lib_call':
        if (!sim.lib || !/^lib_/.test(a.cmd || '')) throw new Error('No library in this demo');
        return sim.lib.invoke(a.cmd, a.args || {});
      // the Conversions page: the demo's music folder holds no converted versions
      case 'conv_scan': return { files: VERS.map(v => ({ ...v, stages: [...v.stages], tags: [] })), scanned: VERS.length, ms: 14 };
      case 'conv_matching': return [];
      case 'conv_check': return (a.paths || []).map(pa => (versionOf(pa) ? { exists: true, valid: true, tip: null } : { exists: false, valid: false, tip: null }));
      case 'conv_trash': return (a.paths || []).map(pa => ({ path: pa, ok: false, error: 'Not in this demo' }));
      case 'conv_reveal': case 'vis_dir_open': case 'vis_save': case 'vis_delete': case 'vis_import': case 'vis_export':
      case 'vis_restore_app':
        sim.host?.say(FULL_APP);
        return null;
      case 'vis_list': return visList();
      case 'crash_report_state': return 'none';
      case 'radio_store_load': return '{}';
      case 'an_window_png': throw new Error(FULL_APP);
      case 'eq_autoeq_index': return {
        entries: [{ name: 'My headphones', source: 'demo', path: 'demo/over-ear/My headphones', rec: true }],
        fetched: Math.floor(Date.now() / 1000), stale: false };
      case 'eq_autoeq_fetch': return 'Preamp: -6.3 dB\n' + HP_EQ.bands.map((b, i) =>
        `Filter ${i + 1}: ON ${b.type === 'lowShelf' ? 'LSC' : b.type === 'highShelf' ? 'HSC' : 'PK'} Fc ${b.f} Hz Gain ${b.gain} dB Q ${b.q.toFixed(2)}`).join('\n');
      case 'an_subject_open': return an.sid;
      case 'analyzer_open': return openTool('analyzer');
      case 'player_input_supported': return true;
      case 'perf_open': case 'vis_studio_open': case 'filters_download':
      case 'update_install': case 'spatial_install': case 'player_input_listen': case 'player_radio':
      case 'player_input_install': case 'radio_station_url': case 'radio_stream_wait':
      case 'radio_catalog_lists': case 'radio_catalog_search':
        sim.host?.say(FULL_APP);
        return null;
      default: return null;
    }
  };

  // ── conversion ─────────────────────────────────────────────────────────
  // A batch runs its files side by side, each at its own pace (a longer
  // track takes longer), and answers the converter page's two polls the way
  // the backend does: get_conversion_progress and get_queue_status.
  const conv = {
    made: new Set(),                 // the files the demo has "written"
    b: null, rev: 0,
    start(a) {
      if (this.b && this.b.running) throw new Error('A batch is running');
      const t0 = now(), fs = a.fsMultiplier || FS0, lf = a.labFeatures || {};
      const tags = stagesOf({
        declip: lf.declip, isp: lf.isp, subsonicHz: a.subsonicHz, adaptiveHeadroom: lf.adaptiveHeadroom,
        adaptiveApodizer: a.adaptiveApodizer, xtc: lf.xtc,
        phase: lf.continuousAlpha ? 'alpha' : a.hybridPhase ? 'hybrid' : lf.tfsPhase ? 'tfs' : 'linear',
      }).filter(t => t !== 'PFR').map(t => t.replace('α', 'a'));
      const label = `AE · ${fmtK(SRC_RATE)}→${fmtK(SRC_RATE * fs)} · Kaiser ${tapsLabel(a.taps) || ''} · f64${tags.length ? ' · ' + tags.join('·') : ''}`;
      this.b = {
        t0, running: true, cancelled: false, lab: tags.map(t => [t, 1, '']),
        files: (a.paths || []).map((pa, idx) => {
          const t = infos.find(x => x.path === pa), durS = t ? t.durationS : 200;
          return { idx, path: pa, out: pa.replace(/\.[^./]+$/, '') + ` [${label}].flac`,
            t0: t0 + 0.3 + idx * 0.25, len: Math.min(24, 5 + durS / 32), stage: 0, pct: 0, rev: 0 };
        }),
      };
      return null;
    },
    step() {
      const b = this.b; if (!b || !b.running) return;
      const t = now();
      for (const f of b.files) {
        if (f.stage >= 4) continue;
        const x = (t - f.t0) / f.len;
        if (x < 0) continue;
        const stage = x >= 1 ? 4 : x < 0.12 ? 1 : x < 0.9 ? 2 : 3, pct = Math.round(Math.min(1, x) * 1000);
        if (stage !== f.stage || pct !== f.pct) { f.stage = stage; f.pct = pct; f.rev = ++this.rev; }
        if (stage === 4) this.made.add(f.out);
      }
      if (b.files.every(f => f.stage >= 4)) b.running = false;
    },
    progress() {
      this.step();
      const b = this.b;
      if (!b) return [0, 0, 0, '', '', 0, false, false];
      const done = b.files.filter(f => f.stage >= 4).length, act = b.files.filter(f => f.stage > 0 && f.stage < 4);
      const cur = act.length ? act.reduce((s, f) => s + f.pct, 0) / act.length : 1000;
      const text = b.cancelled ? 'Cancelled' : b.running ? `Converting ${act.length} file${act.length === 1 ? '' : 's'}…` : 'All done';
      return [Math.round(cur), b.files.length, done, text, '', SRC_RATE * FS0, b.running, b.cancelled];
    },
    queue(known) {
      this.step();
      const b = this.b; if (!b) return { rev: this.rev, files: [] };
      const files = b.files.filter(f => f.rev > known || (f.stage > 0 && f.stage < 4)).map(f => ({
        idx: f.idx, stage: f.stage, pct: f.pct, badge: 0, aa: f.stage >= 4 ? 1 : 0,
        lab: f.stage >= 2 ? b.lab : [], output: f.stage === 4 ? f.out : undefined, error: f.stage === 6 ? '' : undefined,
      }));
      return { rev: this.rev, files };
    },
    cancel() {
      const b = this.b; if (!b || !b.running) return;
      for (const f of b.files) if (f.stage < 4) { f.stage = 6; f.rev = ++this.rev; }
      b.running = false; b.cancelled = true;
    },
    cancelFile(idx) {
      const f = this.b && this.b.files[idx];
      if (f && f.stage < 4) { f.stage = 6; f.rev = ++this.rev; }
      if (this.b && this.b.files.every(x => x.stage >= 4)) this.b.running = false;
    },
  };

  // ── the Conversions page: versions made earlier ────────────────────────
  // The newest album's first tracks were "converted" before: each as DSD256
  // (a DSF), FS8 352.8k and FS16 705.6k (FLAC 24-bit), named and stamped the
  // converter's way. The page lists them under their originals; Play puts
  // one on the air the app's way (the original's row is told to play that
  // file), and the status says the file plays as it is, in its own format.
  const VERS = (() => {
    const src = (entries || []).map(e => e.path).filter(pa => infos.some(t => t.path === pa));
    const list = (src.length ? src : infos.map(t => t.path)).slice(0, 4);
    const tags = stagesOf({ phase: 'hybrid', adaptiveApodizer: true, subsonicHz: 10 }).filter(t => t !== 'PFR').map(t => t.replace('α', 'a'));
    const at0 = Date.UTC(2026, 10, 20, 21, 4);
    const out = [];
    list.forEach((pa, i) => {
      const t = infos.find(x => x.path === pa), sr0 = t.sampleRate || SRC_RATE, d = t.durationS || 240;
      // FS and DSD count from the family's base rate (88.2k ×8 is 352.8k)
      const sr = sr0 % 48000 === 0 ? 48000 : 44100;
      const stem = pa.replace(/\.[^.\\/]+$/, '');
      const base = { source: pa, sourceSize: Math.round(d * sr0 * 6 * 0.58), srcRate: sr0, filter: 'Kaiser', precision: 'f64', stages: tags, tags: [] };
      const pcm = (fs, taps, k) => ({ ...base, kind: 'pcm', rate: sr * fs, dsd: null, modulator: null, taps,
        path: `${stem} [AE · ${fmtK(sr0)}→${fmtK(sr * fs)} · Kaiser ${taps} · f64 · ${tags.join('·')}].flac`,
        size: Math.round(d * sr * fs * 6 * 0.52), mtimeMs: at0 + (i * 3 + k) * 61000 });
      out.push({ ...base, kind: 'dsd', rate: sr * 256, dsd: 256, modulator: 'SDM7', taps: '4M',
        path: `${stem} [AE · ${fmtK(sr0)}→DSD256 · SDM7 · Kaiser 4M · ${tags.join('·')}].dsf`,
        size: Math.round(d * sr * 256 * 2 / 8), mtimeMs: at0 + i * 3 * 61000 });
      out.push(pcm(8, '4M', 1), pcm(16, '16M', 2));
    });
    return out;
  })();
  const lowP = s => String(s || '').toLowerCase();
  const versionOf = pa => (pa ? VERS.find(v => lowP(v.path) === lowP(pa)) : null) || null;
  // the file each row plays (player_set_settings' `rendered`): id → path
  const rendered = new Map();
  // the version on the air, when the row that plays is told to play one
  const onAir = () => (p.id ? versionOf(rendered.get(p.id)) : null);

  // ── events (one bus for the three windows) ─────────────────────────────
  sim.listen = (name, fn) => {
    if (!listeners.has(name)) listeners.set(name, new Set());
    listeners.get(name).add(fn);
    return () => listeners.get(name)?.delete(fn);
  };
  function emit(name, payload) {
    for (const fn of listeners.get(name) || []) { try { fn({ event: name, payload, id: 0 }); } catch (e) { console.error(e); } }
  }
  sim.emit = (name, payload) => {
    if (name === 'eq:state' && payload?.state) { eqState = normalizeState(payload.state); eqRev++; }
    setTimeout(() => emit(name, payload), 0);
  };

  // ── status ─────────────────────────────────────────────────────────────
  const outRate = () => (direct() ? srcRate() : baseRate() * (eff().fsMultiplier || FS0));
  function status() {
    tick();
    const t = info(), playing = p.state === 'playing', pos = position();
    const live = p.state === 'playing' || p.state === 'paused';
    const r = eff(), dir = direct(), v = live ? onAir() : null, stages = dir || v ? [] : stagesOf(r);
    // a version on the air goes out as it is: its own rate, or its DSD
    const vRate = v ? (v.kind === 'dsd' ? 0 : v.rate) : 0;
    const vDsd = v && v.kind === 'dsd' ? { label: `DSD${v.dsd}`, rateHz: v.rate, modulator: null, running: null, overloads: 0, asIs: false } : null;
    return {
      state: p.state, trackId: p.id, trackFile: t ? t.path.split('/').pop() : null, renderNote: null,
      positionS: pos, durationS: dur(), srcRate: t ? srcRate() : 0, bits: t ? t.bits : 0, srcFormat: '',
      outRate: t && p.state !== 'stopped' ? (v ? vRate : outRate()) : 0, chainRate: v ? vRate : outRate(),
      device: 'Speakers', exclusive: true, format: v ? (v.kind === 'dsd' ? 'DoP' : '24-bit') : dir ? t && t.bits > 16 ? '24-bit' : '16-bit' : '32-bit float', devFormat: '32-bit float',
      chain: stages, notes: [], quickVariant: false,
      pending: p.state === 'preparing' ? { what: 'Preparing Hybrid-Phase…', sinceMs: Math.round((now() - p.prepT0) * 1000) } : null,
      rebuilding: false, bufferS: live ? 2 : 0, bufferTargetS: 2, visAheadS: 0, bufferMode: 'auto', underrunFrames: 0,
      renderRtf: playing && !dir ? 0.12 : 0, tp: { db: -0.4, kind: 'predicted' }, gainDb: 0, lastSwitchMs: 0, switches: 0,
      volumeDb: p.volumeDb, error: null,
      audible: live ? {
        source: v ? 'file' : dir ? 'direct' : 'live', file: v ? v.path : null, quick: false, taps: dir || v ? null : tapsLabel(r.taps),
        stages: stages.map(tok => [tok, 1, '']),
        gainDb: 0, tpDb: -0.4, ceilingDb: -0.1, gpu: null, downgrade: null, hpDeferred: false, upsampler: null, hpHeld: false,
      } : null,
      jump: { pending: false, targetS: null }, srcPeakDb: live ? -0.1 : null, srcOverPct: live ? 0 : null,
      outLevelDb: null, devPeakDb: null, devPeakMaxDb: null, safetyMutes: 0, eqClipped: 0, testMute: false, hpTrace: false,
      sentPeakMaxDb: null, streamDirect: false, dsd: vDsd, dsdNote: null, rampFramesRead: 0, trackSettingsAppliedSeq: 0,
      latencyMs: 30, arming: null, radio: null, radioError: null, input: null,
    };
  }

  // ── frames ─────────────────────────────────────────────────────────────
  // The 32 measured bands at a time, interpolated between frames, in dB.
  function bandsAt(d, t) {
    const x = Math.max(0, Math.min(d.frames - 1.001, t * d.fps)), f = Math.floor(x), k = x - f;
    const out = new Float32Array(d.nb);
    for (let b = 0; b < d.nb; b++) {
      const a = d.bands[f * d.nb + b], c = d.bands[(f + 1) * d.nb + b];
      out[b] = (a + (c - a) * k) / 2 - 127.5;
    }
    return out;
  }
  function onsetAt(d, t) {
    if (d.det) return catmull(d.det, Math.max(0, t) * d.detFps);
    const x = Math.max(0, Math.min(d.frames - 1.001, t * d.fps)), f = Math.floor(x), k = x - f;
    return d.onset[f] + (d.onset[f + 1] - d.onset[f]) * k;
  }
  // N log-spaced bands 20 Hz–20 kHz, bytes on the protocol's scale
  // (0 = −96 dBFS, 255 = 0 dBFS), with the EQ's curve on them.
  // The EQ window (no ahead_ms) gets it as a meter; the main window has its own.
  function spectrum(n, meter) {
    const t = info(), d = t && ready(t.slug);
    if (!d || p.state !== 'playing') return new Uint8Array(0);
    const src = bandsAt(d, position()), out = new Uint8Array(n), raw = new Float32Array(n);
    const lo = Math.log(20), hi = Math.log(20000), eqOn = eqLive();
    const pre = eqOn ? levelDb(eqEff()) : 0;
    for (let i = 0; i < n; i++) {
      const fHz = Math.exp(lo + (hi - lo) * (i + 0.5) / n);
      const xb = (i + 0.5) / n * d.nb - 0.5, b0 = Math.max(0, Math.min(d.nb - 1, Math.floor(xb))), b1 = Math.min(d.nb - 1, b0 + 1);
      const k = Math.max(0, Math.min(1, xb - b0));
      let db = src[b0] + (src[b1] - src[b0]) * k;
      if (eqOn) db += eqGainDb(eqEff(), fHz) + pre;
      db += p.volumeDb * 0;                  // the analyzer reads the signal before the volume
      raw[i] = db;
    }
    // the meter's look: lightly smoothed across frequency, then a fast rise
    // and a slow, smooth fall frame to frame
    if (!meter) {
      for (let i = 0; i < n; i++) out[i] = Math.max(0, Math.min(255, Math.round((raw[i] + 96) * 255 / 96)));
      return out;
    }
    for (let pass = 0; pass < 2; pass++) {
      let prev = raw[0];
      for (let i = 0; i < n; i++) {
        const c = raw[i], nx = raw[Math.min(n - 1, i + 1)];
        raw[i] = 0.25 * prev + 0.5 * c + 0.25 * nx; prev = c;
      }
    }
    let sp = spec.get(n);
    if (!sp) spec.set(n, sp = { v: Float32Array.from(raw), t: now() });
    const tm = now(), dt = Math.max(0, Math.min(0.25, tm - sp.t));
    const up = 1 - Math.exp(-dt / 0.012), down = 1 - Math.exp(-dt / 0.35);
    for (let i = 0; i < n; i++) {
      const v = sp.v[i], x = raw[i];
      sp.v[i] = v + (x - v) * (x > v ? up : down);
      out[i] = Math.max(0, Math.min(255, Math.round((sp.v[i] + 96) * 255 / 96)));
    }
    sp.t = tm;
    return out;
  }
  const spec = new Map();          // the meter's state per band count
  // The EQ window's analyzer (`/player/eq_spectrum`): n points 10 Hz–30 kHz
  // on a log grid, the level per FFT bin before and after the EQ, in dB, as
  // the engine's frame (version, tapped, n, rate, fft size, then pre and
  // post as f32). The window tilts and smooths them itself. Nothing playing:
  // an empty reply, and its curves fall to the floor.
  const EQ_FFT = 16384;
  function eqSpectrum(n) {
    const t = info(), d = t && ready(t.slug);
    if (!d || p.state !== 'playing') return new Uint8Array(0);
    const src = bandsAt(d, position()), eqOn = eqLive(), lvl = eqOn ? levelDb(eqEff()) : 0;
    const buf = new ArrayBuffer(12 + 8 * n), dv = new DataView(buf);
    dv.setUint8(0, 1); dv.setUint8(1, 1); dv.setUint16(2, n, true);
    dv.setUint32(4, outRate(), true); dv.setUint32(8, EQ_FFT, true);
    const pre = new Float32Array(buf, 12, n), post = new Float32Array(buf, 12 + 4 * n, n);
    const span = Math.log(3000), lo = Math.log(20), hi = Math.log(20000);
    const frame = Math.floor(position() * 30);
    for (let i = 0; i < n; i++) {
      const f = 10 * Math.exp(span * i / (n - 1));
      // the band level there (the envelopes span 20 Hz–20 kHz), falling off
      // outside it as a master does
      const xb = (Math.log(Math.max(20, Math.min(20000, f))) - lo) / (hi - lo) * d.nb - 0.5;
      const b0 = Math.max(0, Math.min(d.nb - 1, Math.floor(xb))), b1 = Math.min(d.nb - 1, b0 + 1);
      const k = Math.max(0, Math.min(1, xb - b0));
      let db = src[b0] + (src[b1] - src[b0]) * k;
      if (f < 20) db -= 18 * Math.log2(20 / f);
      if (f > 20000) db -= 30 * Math.log2(f / 20000);
      // a band's power spread over its FFT bins: about −3 dB an octave on
      // the per-bin scale (the window's tilt lifts it back), plus a little
      // of an FFT's grain, new every frame
      db -= 3 * Math.log2(f / 1000) + 12;
      db += 1.5 * (grain(i, frame) - 0.5);
      pre[i] = db;
      post[i] = eqOn ? db + eqGainDb(eqEff(), f) + lvl : db;
    }
    return new Uint8Array(buf);
  }
  const grain = (i, fr) => { const x = Math.sin(i * 12.9898 + fr * 78.233) * 43758.5453; return x - Math.floor(x); };
  function phaseLamp() {
    const t = info(), d = t && ready(t.slug);
    if (!d || p.state !== 'playing' || !hpOn()) return new Uint8Array([0, 0, 0]);
    const e = onsetAt(d, position());
    // the engine's lamp bytes: HP's switch, or αHP's α (the other one dark)
    if (isAlpha()) return new Uint8Array([0, Math.round(alphaOf(e) * 255), 0]);
    return new Uint8Array([e >= HP_SENS[sensOf() + 3] ? 255 : 0, 0, 0]);
  }
  // αHP's α: the smoothed envelope times the step's gain (the switch
  // threshold at step 0 over the step's own), capped at 1
  const alphaOf = e => Math.min(1, e * HP_SENS[3] / HP_SENS[sensOf() + 3]);
  function phaseView(q) {
    const t = info(), d = t && ready(t.slug);
    if (!d || (p.state !== 'playing' && p.state !== 'paused') || !hpOn()) return new Uint8Array(0);
    const alpha = isAlpha(), hpSens = sensOf();
    const n = Math.max(1, Math.min(1024, q.n | 0)), from = +q.from_ms, to = +q.to_ms, binMs = (to - from) / n;
    if (!(binMs > 0)) return new Uint8Array(0);
    const posMs = position() * 1000, g = posMs / binMs, nowBin = Math.floor(g), nowK = Math.round(-from / binMs);
    const thr = HP_SENS[hpSens + 3];
    const b = new Uint8Array(8 + 2 * n + 5);
    b[0] = alpha ? 2 : 1; b[1] = 0; b[2] = 1; b[3] = Math.floor((g - nowBin) * 256);
    b[4] = n & 255; b[5] = n >> 8; b[6] = nowK & 255; b[7] = nowK >> 8;
    let hold = 0;
    for (let k = 0; k < n; k++) {
      const ms = (nowBin - nowK + k) * binMs;
      const e = ms < 0 ? 0 : d.det ? detBin(d, ms, ms + binMs) : onsetAt(d, ms / 1000);
      let w;
      if (alpha) w = Math.round(alphaOf(e) * 255);
      else {
        if (e >= thr) hold = Math.ceil(120 / binMs);     // the minimum-phase branch holds through the attack
        w = hold > 0 ? 255 : 0; if (hold > 0) hold--;
      }
      b[8 + k] = w; b[8 + n + k] = Math.round(e * 255);
    }
    const tail = 8 + 2 * n;
    b[tail] = hpSens & 255;
    b[tail + 1] = nowBin & 255; b[tail + 2] = (nowBin >> 8) & 255; b[tail + 3] = (nowBin >> 16) & 255; b[tail + 4] = (nowBin >>> 24) & 255;
    return b;
  }
  function hpPrepare() {
    if (p.state === 'preparing') {
      const x = Math.min(1, (now() - p.prepT0) / PREP_S);
      return { state: 'preparing', progress: x < 0.2 ? x * 1.5 : 0.3 + (x - 0.2) * 0.86, stage: x < 0.25 ? 'decode' : x < 0.7 ? 'stems' : 'kit',
        track: p.id, eta_s: Math.max(0, PREP_S - (now() - p.prepT0)) };
    }
    if (p.id && prepared.has(p.id)) return { state: 'ready', progress: 1, stage: '', track: p.id, eta_s: null };
    return { state: 'idle', progress: 0, stage: '', track: null, eta_s: null };
  }

  // ── the visualizer's spans (vis/music.js) ──────────────────────────────
  // The frames are counted at VIS_RATE: the spectrum's slices every step_ms,
  // the waveform at half that rate. A track is a ring of its own.
  const VIS_RATE = 48000;
  const visRing = () => (p.id || 0) * 7 + 1;
  const visState = () => (p.state === 'playing' ? 0 : p.state === 'paused' ? 1 : 3);
  const db255 = db => Math.max(0, Math.min(255, Math.round((db + 96) * 255 / 96)));
  function levelsAt(d, t) {
    const x = Math.max(0, Math.min(d.frames - 1, Math.round(t * d.fps)));
    const db = v => v / 2 - 127.5;
    return { pkL: db(d.pk[2 * x]), pkR: db(d.pk[2 * x + 1]), rL: db(d.rms[2 * x]), rR: db(d.rms[2 * x + 1]) };
  }
  function spanHead(dv, now, step, k0, n) {
    [now, VIS_RATE, step, k0, n, visRing(), 1, -1, now + 4 * VIS_RATE, visState()].forEach((v, i) => dv.setFloat64(i * 8, v, true));
  }
  function spectrumSpan(q) {
    const nb = Math.max(1, Math.min(128, q.bands | 0 || 48)), step = Math.round(Math.max(5, +q.step_ms || 20) / 1000 * VIS_RATE);
    const t = info(), d = t && ready(t.slug), live = d && (p.state === 'playing' || p.state === 'paused');
    const now = live ? position() * VIS_RATE : 0;
    let k0 = 0, n = 0;
    if (live) {
      k0 = Math.ceil((now + (+q.from_ms || 0) / 1000 * VIS_RATE) / step);
      n = Math.max(0, Math.min(400, Math.floor((now + (+q.to_ms || 0) / 1000 * VIS_RATE) / step) - k0 + 1));
    }
    const per = 1 + nb + 8, buf = new ArrayBuffer(80 + n * per), dv = new DataView(buf), u8 = new Uint8Array(buf);
    spanHead(dv, now, step, k0, n);
    const lo = Math.log(20), hi = Math.log(20000), eqOn = eqLive(), pre = eqOn ? levelDb(eqEff()) : 0;
    for (let j = 0; j < n; j++) {
      const ts = (k0 + j) * step / VIS_RATE, o = 80 + j * per;
      if (ts < 0 || ts > dur()) continue;
      u8[o] = 1;
      const src = bandsAt(d, ts);
      for (let i = 0; i < nb; i++) {
        const xb = (i + 0.5) / nb * d.nb - 0.5, b0 = Math.max(0, Math.min(d.nb - 1, Math.floor(xb))), b1 = Math.min(d.nb - 1, b0 + 1);
        const k = Math.max(0, Math.min(1, xb - b0));
        let db = src[b0] + (src[b1] - src[b0]) * k;
        if (eqOn) db += eqGainDb(eqEff(), Math.exp(lo + (hi - lo) * (i + 0.5) / nb)) + pre;
        u8[o + 1 + i] = db255(db);
      }
      const l = levelsAt(d, ts), mid = 10 * Math.log10((Math.pow(10, l.rL / 10) + Math.pow(10, l.rR / 10)) / 2 + 1e-12);
      const e = o + 1 + nb;
      u8[e] = db255(Math.max(l.pkL, l.pkR)); u8[e + 1] = db255(l.rL); u8[e + 2] = db255(l.rR);
      u8[e + 3] = db255(mid); u8[e + 4] = db255(mid - 11 + 3 * Math.sin(ts * 0.7)); u8[e + 5] = Math.round((0.72 + 1) * 127.5);
    }
    return u8;
  }
  // The waveform: a few partials under the track's own level, with grain
  function waveSpan(q) {
    const rate = Math.max(4000, Math.min(48000, +q.rate || 24000)), dd = VIS_RATE / rate;
    const t = info(), d = t && ready(t.slug), live = d && (p.state === 'playing' || p.state === 'paused');
    const now = live ? position() * VIS_RATE : 0;
    let i0 = 0, n = 0;
    if (live) {
      i0 = Math.max(0, Math.ceil((now + (+q.from_ms || 0) / 1000 * VIS_RATE) / dd));
      n = Math.max(0, Math.min(rate, Math.floor((now + (+q.to_ms || 0) / 1000 * VIS_RATE) / dd) - i0 + 1));
    }
    const buf = new ArrayBuffer(80 + n * 4), dv = new DataView(buf);
    [now, VIS_RATE, dd, i0, n, visRing(), 1, -1, 0, 0].forEach((v, i) => dv.setFloat64(i * 8, v, true));
    let lv = null, lvT = -1;
    for (let j = 0; j < n; j++) {
      const i = i0 + j, ts = i / rate;
      const ft = Math.floor(ts * 20);
      if (ft !== lvT) { lvT = ft; lv = levelsAt(d, ts); }
      const aL = Math.pow(10, lv.rL / 20) * 1.3, aR = Math.pow(10, lv.rR / 20) * 1.3;
      const w = 0.55 * Math.sin(2 * Math.PI * 82 * ts) + 0.3 * Math.sin(2 * Math.PI * 247 * ts + 1.3)
        + 0.18 * Math.sin(2 * Math.PI * 1310 * ts) + 0.22 * (grain(i, 3) - 0.5);
      const w2 = 0.55 * Math.sin(2 * Math.PI * 82 * ts + 0.4) + 0.3 * Math.sin(2 * Math.PI * 247 * ts + 0.2)
        + 0.18 * Math.sin(2 * Math.PI * 1310 * ts + 2) + 0.22 * (grain(i, 5) - 0.5);
      dv.setInt16(80 + j * 4, Math.max(-32767, Math.min(32767, Math.round(w * aL * 32767))), true);
      dv.setInt16(82 + j * 4, Math.max(-32767, Math.min(32767, Math.round(w2 * aR * 32767))), true);
    }
    return new Uint8Array(buf);
  }

  // ── Cue Consistency's view (player_cue_view) ───────────────────────────
  // Five places across the stage, each with a share of the track's bands and
  // a wave of its own under the track's level; they drift a little.
  const CUE_HZ = 100, CUE_HOPS = 80;
  const CUE_SRC = [
    { deg: -24, lo: 0, hi: 12, w: 1, dir: 0.82 }, { deg: 4, lo: 8, hi: 22, w: 0.9, dir: 0.9 },
    { deg: 31, lo: 14, hi: 28, w: 0.7, dir: 0.66 }, { deg: -41, lo: 18, hi: 32, w: 0.55, dir: 0.5 },
    { deg: 17, lo: 4, hi: 16, w: 0.6, dir: 0.74 },
  ];
  function cueView() {
    const t = info(), d = t && ready(t.slug), r = eff();
    const running = !!d && p.state === 'playing' && !direct() && !!r.cue;
    if (!running) return { running: false, rate: 0, xtc: !!r.xtc, confidence: 0, open: false, mono: false, wet: 0, dots: [], bands: [], atS: 0, seq: 0, sliceHz: CUE_HZ };
    const pos = position(), src = bandsAt(d, pos), seq = Math.floor(pos * CUE_HZ);
    const nbs = 16, bands = [];
    for (let i = 0; i < nbs; i++) {
      const hz = 20 * Math.pow(1000, (i + 0.5) / nbs);
      const itd = hz < 1500 ? 420 - hz * 0.12 : 120;
      bands.push({ hz, itdUs: itd, ildInDb: 0.4 + 2.6 * i / nbs, ildOutDb: 1.2 + 5.4 * i / nbs });
    }
    const dots = CUE_SRC.map((s, k) => {
      const specDb = [];
      for (let i = 0; i < nbs; i++) {
        const b = Math.round(i / (nbs - 1) * (d.nb - 1)), inside = b >= s.lo && b <= s.hi;
        specDb.push(src[b] - (inside ? 4 : 22) - k * 1.5);
      }
      const wave = [];
      for (let h = seq - CUE_HOPS + 1; h <= seq; h++) {
        const ts = Math.max(0, h / CUE_HZ), l = levelsAt(d, ts);
        const a = Math.min(1, Math.pow(10, (k % 2 ? l.rR : l.rL) / 20) * 1.6 * s.w * (0.7 + 0.3 * Math.sin(ts * (2 + k))));
        wave.push(-a * (0.6 + 0.4 * grain(h, k)), a * (0.6 + 0.4 * grain(h, k + 9)));
      }
      const deg = s.deg + 3 * Math.sin(pos * 0.21 + k * 1.7);
      return { id: (p.id || 0) * 10 + k + 1, deg, rhoDb: deg / 6, itdMs: 0, weight: s.w, specDb, direct: s.dir, wave };
    });
    return { running: true, rate: outRate(), xtc: !!r.xtc, confidence: 0.84 + 0.06 * Math.sin(pos * 0.3), open: true, mono: false,
      wet: 1, dots, bands, atS: pos, seq, sliceHz: CUE_HZ };
  }

  // ── the aura protocol (fetch) ──────────────────────────────────────────
  sim.route = (url) => {
    const u = new URL(url), q = Object.fromEntries(u.searchParams);
    if (u.pathname.startsWith('/player/an/')) {
      const out = an.route(u.pathname, q);
      return out ? bin(out) : new Response('', { status: 404 });
    }
    switch (u.pathname) {
      case '/player/status': return json(status());
      case '/player/eq_spectrum': return bin(eqSpectrum(Math.max(2, Math.min(2048, q.n | 0 || 480))));
      case '/player/spectrum': return bin(spectrum(Math.max(1, Math.min(128, +q.bands || 48)), q.ahead_ms == null));
      case '/player/phase_lamp': return bin(phaseLamp());
      case '/player/phase_view': return bin(phaseView(q));
      case '/player/hp_prepare': return json(hpPrepare());
      case '/player/spectrum_span': return bin(spectrumSpan(q));
      case '/player/wave_span': return bin(waveSpan(q));
      case '/filters/status': return json({ phase: 'idle' });
      case '/update/status': return json({ phase: 'idle', done: 0, total: 0, error: null });
      default: return new Response('', { status: 404 });
    }
  };
  const json = o => new Response(JSON.stringify(o), { headers: { 'Content-Type': 'application/json' } });
  const bin = a => new Response(a, { headers: { 'Content-Type': 'application/octet-stream' } });

  // the analyzer window's routes (demo/an-sim.js)
  const an = createAnalyzer({
    track: info, ready, position, state: () => p.state, get outRate() { return outRate(); },
    eq: () => (eqLive() ? {
      gainDb: f => eqGainDb(eqEff(), f), rev: eqRev,
      preDb: levelDb(eqEff()),
    } : null),
  });

  load(infos[0].slug);
  return sim;
}

// ── the EQ's curve: its biquad sections at the output rate (eq/math.js) ──
const FS = OUT_RATE;
const secsOf = new WeakMap();      // state -> its bands' sections
function eqGainDb(s, f) {
  let secs = secsOf.get(s);
  if (!secs) secsOf.set(s, secs = (s.bands || []).filter(b => b.on).flatMap(b => sections(b, FS)));
  let db = 0;
  for (const co of secs) db += magDb(co, f, FS);
  return db;
}
// the level after the EQ: the gain mode's, plus the output gain, plus the
// balance's share of the two channels together (it only lowers one side)
const levelDb = s => modeLevel(s, FS) + (s.output || 0) + balanceLossDb(s);
function balanceLossDb(s) {
  const b = Math.abs(balanceOf(s).balance);
  return b === 0 ? 0 : 10 * Math.log10((1 + Math.pow(10, -b / 10)) / 2);
}
