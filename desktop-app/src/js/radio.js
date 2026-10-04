// Internet radio in the player: the stations, the view that finds and plays
// them in the list's place (F / R, listview.js), and how the strip and the
// big player say what a stream plays.
//
// The player's status says where a stream is in codes — radio.phase
// ("connecting", "waiting", "playing", "reconnecting", "failed"), radio.waitS,
// radio.preparing (the stream's own filter being made, once), radio.now
// (what is heard there), radioError (a failure's kind) — and the words for
// them are this file's. A stream plays the rack, its length too. player.js
// hands in each status and plays what the view asks for; the catalog (Radio
// Browser), the stations' icons and the view's records come from the
// backend through the hooks player.js gives (`call`); through them too the
// view has the stream's filter of the rack made ahead while it is shown.

import { listView, onListView, mountListView, setListPlaying, playingOf } from './listview.js';

const $ = id => document.getElementById(id);

/// Featured: the stations the radio was made on, the lossless ones first.
/// They play through the rack as any pasted address does; for Radio Paradise
/// the backend also asks its own list what plays (its FLAC streams carry no
/// titles).
export const STATIONS = [
    { name: 'Radio Paradise — Main Mix', url: 'https://stream.radioparadise.com/flac', tech: 'FLAC 44.1 kHz' },
    { name: 'Radio Paradise — Mellow Mix', url: 'https://stream.radioparadise.com/mellow-flac', tech: 'FLAC 44.1 kHz' },
    { name: 'Radio Paradise — Rock Mix', url: 'https://stream.radioparadise.com/rock-flac', tech: 'FLAC 44.1 kHz' },
    { name: 'Radio Paradise — Global Mix', url: 'https://stream.radioparadise.com/global-flac', tech: 'FLAC 44.1 kHz' },
    { name: 'Rondo Classic — Klasu', url: 'https://iradio.fi/klasu.flac', tech: 'FLAC 44.1 kHz' },
    { name: 'Rondo Classic — Klasu Pro', url: 'https://iradio.fi/klasupro.flac', tech: 'FLAC 44.1 kHz' },
    { name: 'FIP', url: 'https://icecast.radiofrance.fr/fip-hifi.aac', tech: 'AAC 192k 48 kHz' },
    { name: 'FIP Jazz', url: 'https://icecast.radiofrance.fr/fipjazz-hifi.aac', tech: 'AAC 192k 48 kHz' },
    { name: 'FIP Rock', url: 'https://icecast.radiofrance.fr/fiprock-hifi.aac', tech: 'AAC 192k 48 kHz' },
    { name: 'FIP Groove', url: 'https://icecast.radiofrance.fr/fipgroove-hifi.aac', tech: 'AAC 192k 48 kHz' },
].map(st => /radioparadise\.com/.test(st.url) ? { ...st, favicon: 'https://radioparadise.com/apple-touch-icon.png' } : st);

// UI texts.
export const RADIO_TEXTS = {
    title: 'Radio',
    tabs: { featured: 'Featured', favorites: 'Favorites', recent: 'Recent', search: 'Search' },
    searchName: 'Search stations by name',
    anyGenre: 'Any genre',
    anyCountry: 'Any country',
    anyLanguage: 'Any language',
    anyFormat: 'Any format',
    anyQuality: 'Any quality',
    lossless: 'Lossless',
    kbpsUp: k => `${k} kbps and up`,
    orderName: 'Name',
    orderVotes: 'Most voted',
    orderClicks: 'Most played',
    orderQuality: 'Best quality',
    more: 'More',
    loading: 'Loading…',
    none: 'No stations found',
    catalogDown: "Couldn't reach the catalog: try again later",
    notYet: 'not supported yet',
    favAdd: 'Add to favorites',
    favRemove: 'Remove from favorites',
    favEmpty: 'Star a station to keep it here',
    recentEmpty: 'Stations you play show up here',
    credit: 'Catalog: radio-browser.info',
    paste: 'Paste a stream address',
    play: 'Play',
    tuning: 'Tuning in…',
    filterPreparing: 'Preparing this filter for streams (once)…',
    waiting: n => `Waiting for the filter: ${n} s`,
    reconnecting: 'Reconnecting…',
    address: 'A stream address starts with http:// or https://',
    notLive: 'Not on a live stream: it needs the whole track.',
    // A scene with instruments on a live stream, when they cannot be found
    // there (the status' `radio.instruments`): the menu's hint.
    instrumentsOff: {
        gpu: 'On a live stream the instruments need a graphics card: this scene shows the mix.',
        memory: 'On a live stream the instruments need more free graphics memory: this scene shows the mix.',
    },
    err: {
        unreachable: "Couldn't reach the station",
        format: "This stream's format isn't supported yet",
        rate: "This stream's sample rate can't be upsampled",
        encrypted: "This stream is encrypted: it can't be played.",
    },
};

/// The formats a search can be narrowed to (Radio Browser's names).
const FORMATS = ['MP3', 'AAC', 'AAC+', 'OGG', 'OPUS', 'FLAC'];

/// The qualities a search can be narrowed to: lossless (FLAC, or FLAC in
/// Ogg), or a bitrate at least — a lossless station passes any, one of
/// unknown bitrate none (the catalog's side does it, radio_catalog_search).
export const QUALITIES = ['lossless', '320', '256', '192', '128'];
export const qualityText = q => (q === 'lossless' ? RADIO_TEXTS.lossless : RADIO_TEXTS.kbpsUp(q));

const KEY_URL = 'auraRadioUrl';     // the address last pasted in the view
const KEY_LAST = 'auraRadioLast';   // the stream last played (▶ after Stop, ⏮/⏭)
const KEY_TAB = 'auraRadioTab';     // the view's tab
const KEY_QUERY = 'auraRadioQuery'; // the search last asked
const KEY_SORT = 'auraRadioSort';   // its order (a "votes" kept with the search before is not looked at)
const KEY_STORE = 'auraRadioStore'; // a copy of the records kept in the app's data
const load = k => { try { return localStorage.getItem(k) || ''; } catch (_) { return ''; } };
const store = (k, v) => { try { localStorage.setItem(k, String(v)); } catch (_) {} };

const norm = u => String(u || '').trim().replace(/\/+$/, '').toLowerCase();

/// The station of the list a stream address is, or null.
export const stationOf = url => STATIONS.find(st => norm(st.url) === norm(url)) || null;

/// A stream address the player can try: http:// or https://.
export const isStreamAddress = url => /^https?:\/\/\S+$/i.test(String(url || '').trim());

// ── the radio's records ────────────────────────────────────────────────
// Favorites, recent stations and the stations that failed on their format,
// kept by the backend in the app's data folder (radio_store_*), a copy in
// localStorage. Each function here returns a new record and leaves the one
// it was given as it was.

export const RECENT_KEEP = 20;
const FAVORITES_KEEP = 500;

/// What a station is known by: its catalog uuid, else its address.
export const stationKey = st => (st?.uuid ? st.uuid : 'url:' + norm(st?.url));

/// What a record keeps of a station.
function snapshot(st) {
    const out = {};
    for (const k of ['name', 'url', 'uuid', 'favicon', 'homepage', 'tags', 'country', 'countrycode', 'codec',
        'bitrate', 'hls', 'plays', 'tech', 'rate', 'bits']) {
        if (st[k] !== undefined && st[k] !== '' && st[k] !== null) out[k] = st[k];
    }
    return out;
}

const okStation = st => !!st && typeof st === 'object' && typeof st.name === 'string' && isStreamAddress(st.url);

/// A record as kept (or none, or damaged) made whole.
export function normStore(x) {
    const list = (a, n) => (Array.isArray(a) ? a.filter(okStation).slice(0, n) : []);
    const failed = {};
    if (x?.failed && typeof x.failed === 'object') {
        for (const [k, v] of Object.entries(x.failed)) if (Number.isFinite(v)) failed[k] = v;
    }
    return { v: 1, favorites: list(x?.favorites, FAVORITES_KEEP), recent: list(x?.recent, RECENT_KEEP), failed };
}

export const isFavorite = (rec, st) => rec.favorites.some(f => stationKey(f) === stationKey(st));

/// Starred, or unstarred when it was: a new star goes to the end.
export function toggleFavorite(rec, st) {
    const k = stationKey(st);
    const favorites = isFavorite(rec, st)
        ? rec.favorites.filter(f => stationKey(f) !== k)
        : [...rec.favorites, snapshot(st)].slice(-FAVORITES_KEEP);
    return { ...rec, favorites };
}

/// Played now: first in the recent list, once.
export function addRecent(rec, st) {
    const k = stationKey(st);
    const old = rec.recent.find(r => stationKey(r) === k);
    const recent = [{ ...old, ...snapshot(st) }, ...rec.recent.filter(r => stationKey(r) !== k)].slice(0, RECENT_KEEP);
    return { ...rec, recent };
}

/// What was heard of the stream at `url` (its codec, rate and depth, and the
/// name its server gives a pasted stream) written into its records — the
/// records at that address, or of the station `key` (a catalog station
/// plays at the address the catalog gave when it started). Returns the same
/// record when nothing new was heard.
export function noteHeard(rec, url, info, key = null) {
    if (!info?.rate) return rec;
    let changed = false;
    const fix = st => {
        if (norm(st.url) !== norm(url) && !(key && stationKey(st) === key)) return st;
        const next = { ...st, rate: info.rate };
        if (info.bits) next.bits = info.bits;
        if (!next.codec && info.codec) next.codec = codecName(info.codec);
        const icy = String(info.icyName || '').trim();
        if (icy && !st.uuid && !stationOf(st.url) && st.name === hostOf(st.url)) next.name = icy;
        if (next.rate !== st.rate || next.bits !== st.bits || next.codec !== st.codec || next.name !== st.name) changed = true;
        return next;
    };
    const out = { ...rec, favorites: rec.favorites.map(fix), recent: rec.recent.map(fix) };
    return changed ? out : rec;
}

/// A catalog station that turned out to be in a format the player does not
/// play (Radio Browser lists Opus as OGG), under the formats table's
/// revision: a newer table tries it again.
export const markFailed = (rec, uuid, rev) => (uuid ? { ...rec, failed: { ...rec.failed, [uuid]: rev } } : rec);

/// Whether a station plays here: what the catalog said of its format, less
/// what it turned out to be.
export const playsHere = (rec, st, rev) => st.plays !== false && !(st.uuid && rec.failed[st.uuid] === rev);

// ── the lines a row shows ──────────────────────────────────────────────

const fmtRate = hz => { const k = hz / 1000; return (Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1)) + ' kHz'; };
const CODECS = { flac: 'FLAC', aac: 'AAC', 'he-aac': 'HE-AAC', 'he-aac v2': 'HE-AAC v2', mp3: 'MP3', vorbis: 'Vorbis', opus: 'Opus' };

/// A codec as the lines write it: its own name when it has one (HE-AAC v2,
/// Opus), else in capitals.
export const codecName = c => { const s = String(c || ''); return CODECS[s.toLowerCase()] || s.toUpperCase(); };

function hostOf(url) {
    try { return new URL(url).host; } catch (_) { return String(url || ''); }
}

/// A station's format: its own words for a featured one, else the catalog's
/// codec and bitrate, and the rate once heard.
export function techLine(st) {
    if (st.tech) return st.tech;
    const parts = [];
    if (st.codec) parts.push(st.bitrate ? `${st.codec} ${st.bitrate}k` : st.codec);
    else if (st.bitrate) parts.push(`${st.bitrate}k`);
    if (st.rate) parts.push(fmtRate(st.rate));
    return parts.join(' ');
}

/// The line under a station's name: genres, country, format — and that it
/// does not play here yet.
export function rowSub(st, plays = true) {
    const parts = [];
    const tags = (st.tags || []).slice(0, 2).join(', ');
    if (tags) parts.push(tags);
    if (st.country) parts.push(st.country);
    const tech = techLine(st);
    if (tech) parts.push(tech);
    if (!plays) parts.push(RADIO_TEXTS.notYet);
    return parts.join(' · ');
}

/// A station's initials, for the tile shown until (or instead of) its icon.
export function initials(name) {
    const words = String(name || '').replace(/[^\p{L}\p{N}\s]/gu, ' ').split(/\s+/).filter(Boolean);
    return words.slice(0, 2).map(w => w[0].toUpperCase()).join('') || '?';
}

/// A hue for a station's tile, the same each time.
export function tileHue(name) {
    let h = 0;
    for (const c of String(name || '')) h = (h * 31 + c.codePointAt(0)) % 360;
    return h;
}

// ── the lists' order ───────────────────────────────────────────────────
// Names A to Z with case not looked at and numbers by their value (Radio 2
// before Radio 10); a search by name puts the closest names first. Featured
// keeps its own order, Recent the newest first.

const COLLATOR = new Intl.Collator('en', { numeric: true, sensitivity: 'base' });
const WORD_CHAR = /[\p{L}\p{N}]/u;

/// Two names, A to Z.
export const byName = (a, b) => COLLATOR.compare(String(a ?? '').trim(), String(b ?? '').trim());

/// Stations A to Z by name (a new list; equal names keep their order).
export const alphabetical = stations => [...(stations || [])].sort((a, b) => byName(a?.name, b?.name));

/// How close a station's name is to what was searched (both trimmed, case
/// not looked at): 0 the name is it, 1 the name starts with it, 2 a word of
/// the name starts with it, 3 it is elsewhere in the name, 4 the catalog
/// matched it otherwise (an accent, say).
export function nameRank(name, asked) {
    const n = String(name || '').trim().toLowerCase();
    const q = String(asked || '').trim().toLowerCase();
    if (n === q) return 0;
    if (!q || n.startsWith(q)) return 1;
    for (let i = n.indexOf(q, 1); i > 0; i = n.indexOf(q, i + 1)) {
        if (!WORD_CHAR.test(n[i - 1])) return 2;
    }
    return n.includes(q) ? 3 : 4;
}

/// Lossless as the catalog lists it: FLAC, or FLAC in Ogg — listed as Ogg,
/// told from lossy Ogg (Vorbis, Opus: under 510 kbps) by its bitrate.
export const OGG_LOSSLESS_KBPS = 600;
export function isLossless(st) {
    const c = String(st?.codec || '').toUpperCase();
    return c === 'FLAC' || (c === 'OGG' && (st?.bitrate || 0) >= OGG_LOSSLESS_KBPS);
}

/// The orders a search can be listed in (the view's last picker), the
/// first the default.
export const SORTS = ['name', 'votes', 'clickcount', 'quality'];

/// The order a saved choice asks for: one of `SORTS`, else by name.
export const sortFromSaved = v => (SORTS.includes(v) ? v : 'name');

/// What a search found, in the order it is listed (`sort`, `SORTS`):
/// "name" — by `name` asked the closest names first (`nameRank`), A to Z
/// within each, without one A to Z; "votes" and "clickcount" — the most
/// voted or played first; "quality" — lossless first (`isLossless`), then
/// the highest bitrate. Ties A to Z, then the catalog's order.
export function searchOrder(stations, name, sort = 'name') {
    const q = String(name || '').trim();
    const key = {
        name: st => [q ? nameRank(st?.name, q) : 0],
        votes: st => [-(st?.votes || 0)],
        clickcount: st => [-(st?.clickcount || 0)],
        quality: st => [isLossless(st) ? 0 : 1, -(st?.bitrate || 0)],
    }[sortFromSaved(sort)];
    const cmp = (a, b) => {
        for (let k = 0; k < a.k.length; k++) if (a.k[k] !== b.k[k]) return a.k[k] - b.k[k];
        return byName(a.st?.name, b.st?.name) || a.i - b.i;
    };
    return (stations || [])
        .map((st, i) => ({ st, i, k: key(st) }))
        .sort(cmp)
        .map(x => x.st);
}

/// The genres and countries to choose from, A to Z, and the languages, the
/// most spoken first as the catalog gives them — each a [value, text]; the
/// view puts its "Any …" before them.
export function pickerItems(lists) {
    const az = items => items.sort((a, b) => byName(a[1], b[1]));
    const named = (l, capital) => (l || []).filter(x => x?.name)
        .map(x => [x.name, capital ? x.name.replace(/^\p{L}/u, c => c.toUpperCase()) : x.name]);
    return {
        tags: az(named(lists?.tags, false)),
        countries: az((lists?.countries || []).filter(c => c?.code && c?.name).map(c => [c.code, c.name])),
        languages: named(lists?.languages, true),
    };
}

/// The station a stream is: its name in the lists, else what its server
/// calls it, else its host.
export function stationName(r) {
    const url = r?.info?.url || '';
    const st = stationOf(url) || known(url);
    if (st) return st.name;
    const icy = String(r?.info?.icyName || '').trim();
    if (icy) return icy;
    return hostOf(url);
}

/// The stream's codec and rate, and the rate the rack plays it at; going out
/// bit-perfect, as a file's line says it — "bit-perfect · 44.1 kHz/16 ·
/// exclusive int16" — the codec first.
export function radioTech(s) {
    const i = s?.radio?.info;
    if (!i?.rate) return '';
    const c = String(i.codec || '');
    const depth = `${fmtRate(i.rate)}${i.bits ? '/' + i.bits : ''}`;
    const src = `${codecName(c)} ${depth}`.trim();
    if (!(s.outRate > 0 && s.state !== 'stopped')) return src;
    if (s.streamDirect === true) {
        return `${codecName(c)} · bit-perfect · ${depth} · ${s.exclusive ? 'exclusive' : 'shared'} ${s.format || ''}`.trim();
    }
    return `${src} → ${fmtRate(s.outRate)}`;
}

/// What the strip and the big player show of a stream: the title (what is
/// heard, else the station), the station, the album line a station's own
/// list gives, the technical line.
export function radioLines(s) {
    const r = s?.radio;
    const station = stationName(r);
    const now = r?.now;
    return { station, title: now?.text || station, album: now?.album || '', tech: radioTech(s) };
}

/// Where the stream is, in the page's words — { text, kind: 'busy' | 'error' }
/// — or null when it simply plays (or there is none). While the stream's own
/// filter is being made (once for a rack and a rate) that is said over the
/// phase, a failure excepted.
export function radioStateText(s) {
    if (s?.radioError) return { text: RADIO_TEXTS.err[s.radioError] || s.error || '', kind: 'error' };
    const r = s?.radio;
    if (!r) return null;
    if (r.preparing && r.phase !== 'failed') return { text: RADIO_TEXTS.filterPreparing, kind: 'busy' };
    switch (r.phase) {
    case 'connecting': return { text: RADIO_TEXTS.tuning, kind: 'busy' };
    case 'waiting': return { text: RADIO_TEXTS.waiting(Math.max(1, Math.ceil(r.waitS || 0))), kind: 'busy' };
    case 'reconnecting': return { text: RADIO_TEXTS.reconnecting, kind: 'busy' };
    case 'failed': {
        const t = RADIO_TEXTS.err[r.info?.errorKind];
        return { text: t || `Radio: ${r.info?.error || ''}`, kind: 'error' };
    }
    default: return null;
    }
}

/// Whether the status says a stream failed on its format.
const formatFailed = s => s?.radioError === 'format' || (s?.radio?.phase === 'failed' && s.radio.info?.errorKind === 'format');

/// The station `d` steps from the one at `url` (⏮ −1, ⏭ +1) in `list`, round
/// its ends, past the ones `skip` says do not play; from a stream not in it,
/// the first (or the last).
export function stepStation(url, d, list = STATIONS, skip = () => false) {
    const n = list.length;
    if (!n) return null;
    const i = list.findIndex(st => norm(st.url) === norm(url) || (playing && stationKey(st) === stationKey(playing) && norm(playing.url) === norm(url)));
    let at = i >= 0 ? i : (d > 0 ? -1 : n);
    for (let k = 0; k < n; k++) {
        at = (at + d + n * 2) % n;
        if (!skip(list[at])) return list[at];
    }
    return null;
}

/// The stream last played.
export const lastStream = () => load(KEY_LAST);
export const rememberStream = url => store(KEY_LAST, url);

// ── the session ────────────────────────────────────────────────────────

let hooks = null;     // { play(url) → null | words why not, call(cmd, args) → Promise,
                      //   settings() → the rack as it would be sent }
let rec = normStore(null);
let formatsRev = 1;
let recLoaded = false;
let saveTimer = null;
let playing = null;   // the station last started from the view (or ⏮/⏭)
let playList = null;  // the list it was started from: ⏮/⏭ step through it
let last = null;      // the status last rendered

/// The view's station (in its records) a stream address is.
function known(url) {
    if (!url) return null;
    if (playing && norm(playing.url) === norm(url)) return playing;
    return rec.recent.find(st => norm(st.url) === norm(url)) || rec.favorites.find(st => norm(st.url) === norm(url)) || null;
}

const call = (cmd, args) => (hooks?.call ? hooks.call(cmd, args) : Promise.reject(new Error('no backend')));

async function loadRecords() {
    let kept = null;
    try {
        const r = JSON.parse(await call('radio_store_load'));
        kept = r.store;
        if (Number.isFinite(r.formatsRev)) formatsRev = r.formatsRev;
    } catch (_) {}
    if (!kept) { try { kept = JSON.parse(load(KEY_STORE) || 'null'); } catch (_) {} }
    rec = normStore(kept);
    recLoaded = true;
    renderList();
}

function setRec(next) {
    if (next === rec) return;
    rec = next;
    const json = JSON.stringify(rec);
    store(KEY_STORE, json);
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => { saveTimer = null; call('radio_store_save', { data: json }).catch(() => {}); }, 400);
}

/// F / R in the list's head, the radio's view behind it, and its records.
export function initRadio(h) {
    hooks = h;
    mountListView();
    mountView();
    loadRecords();
}

/// Play a station from one of the view's lists (`list`: ⏮/⏭ step
/// through it). A catalog station's address is asked of the catalog at each
/// start (it counts listeners so); without an answer, the one it listed.
async function playStation(st, list) {
    if (!playsHere(rec, st, formatsRev)) { say(RADIO_TEXTS.err.format); return; }
    playing = st;
    playList = list && list.length ? list.slice() : null;
    setRec(addRecent(rec, st));
    let url = st.url;
    if (st.uuid) {
        try { url = (await call('radio_station_url', { uuid: st.uuid })) || st.url; } catch (_) {}
        if (playing !== st) return;   // another station was chosen meanwhile
        if (norm(url) !== norm(st.url)) playing = { ...st, url };
    }
    const why = hooks?.play(url);
    say(why || null);
    renderList();
}

/// ⏮/⏭ on a stream: the station before or after it in the list it was
/// started from (Featured for a pasted stream).
export function stepRadio(d) {
    const url = last?.radio?.info?.url || playing?.url || lastStream();
    const list = playList && playing && norm(playing.url) === norm(url) ? playList : STATIONS;
    const next = stepStation(url, d, list, st => !playsHere(rec, st, formatsRev));
    if (next) playStation(next, list);
}

// ── the view ───────────────────────────────────────────────────────────
// The radio in the list's place (F / R, listview.js): its tabs in the
// list's head, its stations where the rows are, and under them a stream's
// address to paste. What plays and where it is are
// the player's to say, right over the list; the view's own word (an address
// that is not one, a format that does not play) stands over the address
// for a while.

const TABS = ['featured', 'favorites', 'recent', 'search'];

let view = null;      // #rdView, once built
let tabsBox = null;   // its tabs, in the list's head
let note = null;      // the view's own word, for a while
let noteTimer = null;
let tab = TABS.includes(load(KEY_TAB)) ? load(KEY_TAB) : 'featured';
let query = readQuery();
let lists = null;     // { tags, countries, languages } once the catalog gave them
let listsAsked = false;
let found = { stations: [], next: 0, more: false, busy: false, error: false, key: null };
let searchSeq = 0;
let searchTimer = null;
let warmKey = '';     // the rack and rate the stream's filter was last made ahead for
const icons = new Map();   // favicon address → Promise of its key ('' for none)
let iconWatch = null;

function readQuery() {
    try {
        const q = JSON.parse(load(KEY_QUERY) || '{}');
        return { name: '', tag: '', country: '', language: '', codec: '', quality: '', ...q, order: sortFromSaved(load(KEY_SORT)), offset: 0 };
    } catch (_) {
        return { name: '', tag: '', country: '', language: '', codec: '', quality: '', order: sortFromSaved(load(KEY_SORT)), offset: 0 };
    }
}

/// What a search asks, as a key: the same key is the same search.
export const searchKey = q => JSON.stringify([q.name.trim().toLowerCase(), q.tag, q.country, q.language, q.codec, q.quality || '', q.order]);

/// Whether the radio is the list's view now.
const shown = () => !!view && listView() === 'radio';

function say(text) {
    note = text || null;
    if (noteTimer) clearTimeout(noteTimer);
    noteTimer = note ? setTimeout(() => { note = null; renderNote(); }, 6000) : null;
    renderNote();
}

function renderNote() {
    const el = view?.querySelector('#rdNote');
    if (!el) return;
    if (el.textContent !== (note || '')) el.textContent = note || '';
    el.hidden = !note;
}

function option(value, text) {
    const o = document.createElement('option');
    o.value = value;
    o.textContent = text;
    return o;
}

/// The tabs after F / R and the view in the list's zone: built once, shown
/// and hidden with the view (list.css, radio.css).
function mountView() {
    const head = $('plListHead');
    const zone = $('dropZone');
    if (!head || !zone || $('rdView')) return;

    const tabs = document.createElement('div');
    tabs.className = 'rd-htabs';
    tabs.setAttribute('role', 'tablist');
    tabs.setAttribute('aria-label', RADIO_TEXTS.title);
    for (const id of TABS) {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'rd-htab';
        b.dataset.tab = id;
        b.setAttribute('role', 'tab');
        b.textContent = RADIO_TEXTS.tabs[id];
        b.addEventListener('click', e => {
            if (e.detail > 0) b.blur();   // Space stays the player's
            setTab(id);
        });
        tabs.appendChild(b);
    }
    head.appendChild(tabs);
    tabsBox = tabs;

    const v = document.createElement('div');
    v.className = 'rd-view';
    v.id = 'rdView';
    v.innerHTML = `
      <div class="rd-find" id="rdFind" hidden>
        <input type="search" id="rdName" spellcheck="false" autocomplete="off">
        <div class="rd-pick">
          <select id="rdTag"></select><select id="rdCountry"></select><select id="rdQuality"></select>
          <select id="rdLang"></select><select id="rdCodec"></select><select id="rdOrder"></select>
        </div>
      </div>
      <div class="rd-list" id="rdList"></div>
      <div class="rd-tools">
        <div class="rd-note" id="rdNote" hidden></div>
        <div class="rd-url"><input type="text" id="rdUrl" spellcheck="false" autocomplete="off">
          <button type="button" class="rd-btn" id="rdPlay"></button></div>
      </div>`;
    v.querySelector('#rdPlay').textContent = RADIO_TEXTS.play;

    const name = v.querySelector('#rdName');
    name.placeholder = RADIO_TEXTS.searchName;
    name.value = query.name;
    name.addEventListener('input', () => { query.name = name.value; askSoon(350); });
    name.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); askSoon(0); } });
    const pick = (id, key) => v.querySelector(id).addEventListener('change', e => { query[key] = e.target.value; askSoon(0); });
    pick('#rdTag', 'tag'); pick('#rdCountry', 'country'); pick('#rdLang', 'language');
    pick('#rdCodec', 'codec'); pick('#rdOrder', 'order'); pick('#rdQuality', 'quality');

    const field = v.querySelector('#rdUrl');
    field.placeholder = RADIO_TEXTS.paste;
    field.value = load(KEY_URL);
    const playField = () => {
        const url = field.value.trim();
        store(KEY_URL, url);
        if (!isStreamAddress(url)) { say(RADIO_TEXTS.address); return; }
        const st = known(url) || stationOf(url) || { name: hostOf(url), url };
        playStation(st, null);
    };
    v.querySelector('#rdPlay').addEventListener('click', playField);
    field.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); playField(); } });
    field.addEventListener('input', () => store(KEY_URL, field.value.trim()));
    // The right-click menu of the player is the player's; here the browser's
    // own (paste into the field) stays.
    v.addEventListener('contextmenu', e => e.stopPropagation());
    zone.appendChild(v);
    view = v;
    if (typeof IntersectionObserver === 'function') {
        iconWatch = new IntersectionObserver(seen => {
            for (const e of seen) if (e.isIntersecting) { iconWatch.unobserve(e.target); showIcon(e.target); }
        }, { root: v.querySelector('#rdList') });
    }
    fillPickers();
    setTab(tab, false);
    onListView(on => { if (on === 'radio') opened(); });
    if (shown()) opened();
}

/// The view has come up: the stream's filter made ahead for the rack as it
/// is now, the search where it was left, the station playing ticked.
function opened() {
    warmKey = '';
    warm();
    if (tab === 'search') {
        askLists();
        if (found.key !== searchKey(query) && !found.busy) askSoon(0);
    }
    markPlaying();
}

function setTab(id, focus = true) {
    tab = id;
    store(KEY_TAB, id);
    if (!view) return;
    for (const b of tabsBox.querySelectorAll('.rd-htab')) {
        const on = b.dataset.tab === id;
        b.classList.toggle('on', on);
        b.setAttribute('aria-selected', on ? 'true' : 'false');
    }
    view.querySelector('#rdFind').hidden = id !== 'search';
    if (id === 'search' && shown()) {
        askLists();
        if (found.key !== searchKey(query) && !found.busy) askSoon(0);
        if (focus) view.querySelector('#rdName').focus();
    }
    renderList();
}

function fillPickers() {
    if (!view) return;
    const fill = (id, any, items, value) => {
        const sel = view.querySelector(id);
        sel.replaceChildren(option('', any), ...items.map(([v, t]) => option(v, t)));
        sel.value = items.some(([v]) => v === value) ? value : '';
    };
    const items = pickerItems(lists);
    fill('#rdTag', RADIO_TEXTS.anyGenre, items.tags, query.tag);
    fill('#rdCountry', RADIO_TEXTS.anyCountry, items.countries, query.country);
    fill('#rdLang', RADIO_TEXTS.anyLanguage, items.languages, query.language);
    fill('#rdCodec', RADIO_TEXTS.anyFormat, FORMATS.map(f => [f, f]), query.codec);
    fill('#rdQuality', RADIO_TEXTS.anyQuality, QUALITIES.map(q => [q, qualityText(q)]), query.quality);
    const order = view.querySelector('#rdOrder');
    const sortText = { name: RADIO_TEXTS.orderName, votes: RADIO_TEXTS.orderVotes, clickcount: RADIO_TEXTS.orderClicks,
        quality: RADIO_TEXTS.orderQuality };
    order.replaceChildren(...SORTS.map(s => option(s, sortText[s])));
    order.value = sortFromSaved(query.order);
}

async function askLists() {
    if (lists || listsAsked) return;
    listsAsked = true;
    try {
        lists = JSON.parse(await call('radio_catalog_lists'));
        fillPickers();
    } catch (_) {
        listsAsked = false;   // asked again the next time the tab opens
    }
}

/// Search once the field has been still for `ms` (at once for a list's
/// choice and Enter); the same search twice is asked once.
function askSoon(ms) {
    store(KEY_QUERY, JSON.stringify({ ...query, offset: 0 }));
    store(KEY_SORT, query.order);
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(() => { searchTimer = null; search(false); }, ms);
}

async function search(more) {
    const key = searchKey(query);
    if (!more && key === found.key && !found.error) return;
    const seq = ++searchSeq;
    const offset = more ? found.next : 0;
    const asked = query.name, sort = query.order;
    found = more ? { ...found, busy: true } : { stations: [], next: 0, more: false, busy: true, error: false, key };
    renderList();
    try {
        const r = JSON.parse(await call('radio_catalog_search', { query: { ...query, offset } }));
        if (seq !== searchSeq) return;   // a newer search is under way
        const got = r.stations || [];
        if (Number.isFinite(r.formatsRev)) formatsRev = r.formatsRev;
        const seen = new Set(found.stations.map(stationKey));
        found = {
            // In the order chosen (a next page too): what the picker says.
            stations: searchOrder([...found.stations, ...got.filter(st => !seen.has(stationKey(st)))], asked, sort),
            // The catalog's count: what this view does not list counts too.
            next: offset + (r.count ?? got.length),
            more: !!r.more, busy: false, error: false, key,
        };
    } catch (_) {
        if (seq !== searchSeq) return;
        found = { ...found, busy: false, error: true, key };
    }
    renderList();
}

/// The stations of the tab shown: Featured in its own order, Favorites A to
/// Z, Recent the newest first, a search as `searchOrder` put it.
function tabStations() {
    switch (tab) {
    case 'favorites': return alphabetical(rec.favorites);
    case 'recent': return rec.recent;
    case 'search': return found.stations;
    default: return STATIONS;
    }
}

function emptyText() {
    if (tab === 'favorites') return RADIO_TEXTS.favEmpty;
    if (tab === 'recent') return RADIO_TEXTS.recentEmpty;
    if (tab === 'search') {
        if (found.error) return RADIO_TEXTS.catalogDown;
        if (found.busy) return RADIO_TEXTS.loading;
        return found.key ? RADIO_TEXTS.none : '';
    }
    return '';
}

function iconKey(url) {
    if (!icons.has(url)) icons.set(url, call('radio_icon', { url }).then(k => k || '', () => ''));
    return icons.get(url);
}

function showIcon(tile) {
    const url = tile.dataset.icon;
    if (!url) return;
    iconKey(url).then(k => {
        if (!k || !tile.isConnected) return;
        const img = document.createElement('img');
        img.alt = '';
        img.decoding = 'async';
        img.addEventListener('load', () => tile.classList.add('has-img'));
        img.addEventListener('error', () => img.remove());
        img.src = `https://aura.localhost/radio/icon?k=${k}`;
        tile.appendChild(img);
    });
}

function row(st, list) {
    const plays = playsHere(rec, st, formatsRev);
    const b = document.createElement('div');
    b.className = 'rd-st' + (plays ? '' : ' dim');
    b.setAttribute('role', 'button');
    b.tabIndex = 0;
    b.dataset.url = st.url;
    b.dataset.key = stationKey(st);
    b.innerHTML = `<span class="rd-ico"><span class="rd-ini"></span></span>
        <span class="rd-txt"><span class="rd-nm"></span><span class="rd-sub"></span></span>
        <button type="button" class="rd-fav"></button>`;
    const tile = b.querySelector('.rd-ico');
    tile.style.setProperty('--rd-hue', tileHue(st.name));
    b.querySelector('.rd-ini').textContent = initials(st.name);
    if (st.favicon) tile.dataset.icon = st.favicon;
    b.querySelector('.rd-nm').textContent = st.name;
    const sub = rowSub(st, plays);
    b.querySelector('.rd-sub').textContent = sub;
    b.title = [st.name, sub, st.homepage].filter(Boolean).join('\n');
    const fav = b.querySelector('.rd-fav');
    const starred = isFavorite(rec, st);
    fav.textContent = starred ? '★' : '☆';
    fav.classList.toggle('on', starred);
    fav.title = starred ? RADIO_TEXTS.favRemove : RADIO_TEXTS.favAdd;
    fav.setAttribute('aria-label', fav.title);
    fav.addEventListener('click', e => {
        e.stopPropagation();
        setRec(toggleFavorite(rec, st));
        renderList();
    });
    const go = () => playStation(st, list);
    b.addEventListener('click', go);
    b.addEventListener('keydown', e => { if (e.key === 'Enter' && e.target === b) { e.preventDefault(); go(); } });
    return b;
}

function renderList() {
    if (!view) return;
    const box = view.querySelector('#rdList');
    const top = box.scrollTop;
    const list = tabStations();
    const rows = list.map(st => row(st, list));
    const empty = emptyText();
    if (!rows.length && empty) {
        const e = document.createElement('div');
        e.className = 'rd-empty' + (tab === 'search' && found.error ? ' is-error' : '');
        e.textContent = empty;
        rows.push(e);
    }
    if (tab === 'search' && found.stations.length && (found.more || found.busy)) {
        const m = document.createElement('button');
        m.type = 'button';
        m.className = 'rd-btn rd-more';
        m.textContent = found.busy ? RADIO_TEXTS.loading : RADIO_TEXTS.more;
        m.disabled = found.busy;
        m.addEventListener('click', () => search(true));
        rows.push(m);
    }
    if (tab === 'search') {
        const c = document.createElement('div');
        c.className = 'rd-credit';
        c.textContent = RADIO_TEXTS.credit;
        rows.push(c);
    }
    iconWatch?.disconnect();
    box.replaceChildren(...rows);
    box.scrollTop = top;
    markPlaying();
    // Icons as their rows come into view.
    for (const t of box.querySelectorAll('.rd-ico[data-icon]')) {
        if (iconWatch) iconWatch.observe(t); else showIcon(t);
    }
}

/// The row of the station playing, ticked.
function markPlaying() {
    if (!view) return;
    const r = last?.radio;
    const on = r && !r.stopped ? norm(r.info?.url) : '';
    const onKey = on && playing && norm(playing.url) === on ? stationKey(playing) : null;
    for (const b of view.querySelectorAll('.rd-st')) {
        const is = !!on && (norm(b.dataset.url) === on || b.dataset.key === onKey);
        if (b.classList.contains('on') !== is) b.classList.toggle('on', is);
    }
}

/// What the stream's filter is made ahead for, as a key: the rack's mode,
/// FS, length and phase and the stream's rate (0 before one plays: the
/// 44.1 kHz family) — the same key is the same filter.
export const warmKeyOf = (settings, rate) =>
    JSON.stringify([settings?.mode, settings?.fsMultiplier, settings?.taps, settings?.phase, rate || 0]);

/// The rate the stream's filter is made ahead for, from the status's
/// `radio`: the stream's own; with none (or one that failed before saying
/// it) 0, the 44.1 kHz family; null while a stream tunes in and has not said
/// it yet — it makes its own filter then, and one made ahead for another
/// family would only be stopped again.
export function warmRateOf(r) {
    const rate = r?.info?.rate || 0;
    if (rate > 0 || !r || r.stopped || r.phase === 'failed') return rate;
    return null;
}

/// The stream's own filter of the rack as it is, for the stream's rate,
/// made ahead by the backend so a stream started now has it ready
/// (radio_stream_wait): asked again when either changes, while the view is
/// shown. Its answer is not shown here — the status says while a filter is
/// being made (radio.preparing).
function warm() {
    if (!shown() || !hooks?.settings) return;
    const rate = warmRateOf(last?.radio);
    if (rate == null) return;
    const settings = hooks.settings();
    const key = warmKeyOf(settings, rate);
    if (key === warmKey) return;
    warmKey = key;
    call('radio_stream_wait', { settings, srcRate: rate || null }).catch(() => {});
}

/// After a status: what plays marked on F / R, the station playing ticked,
/// what was heard of it kept, a format it turned out not to play remembered.
export function radioRender(s) {
    last = s;
    setListPlaying(playingOf(s));
    const r = s?.radio;
    const url = r?.info?.url;
    if (url && recLoaded) {
        const key = playing && norm(playing.url) === norm(url) ? stationKey(playing) : null;
        if (r.phase === 'playing') setRec(noteHeard(rec, url, r.info, key));
        if (formatFailed(s) && playing?.uuid && norm(playing.url) === norm(url) && rec.failed[playing.uuid] !== formatsRev) {
            setRec(markFailed(rec, playing.uuid, formatsRev));
            renderList();
        }
    }
    if (!view) return;
    markPlaying();
    warm();
}
