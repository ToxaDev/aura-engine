// ══════════════════════════════════════════════════════════════════════
// The queue's rows the way the Library window draws them, in the app and
// in the streamer alike (Anton 22.11: "the streamer's rows, in the desktop
// too"): the title and the artist from the track's tags on two lines, the
// length and the format at the right, then ▶ and ×. The rows are the list's
// own (dropzone.js); this adds the two text boxes and keeps them up to
// date, the stylesheet (css/queue-rows.css) shows them and hides the file
// name in their place.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';

/// "3:41", "1:02:05"; '' for no length.
export const fmtTime = (s) => {
    if (!(s > 0)) return '';
    s = Math.round(s);
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${x}` : `${m}:${x}`;
};

/// A row's texts: the list's entry `e` and the player's track `t` (its
/// tags, `player_queue`), or none yet. { title, sub, time, tag }.
export function rowText(e, t) {
    const file = (e.path || e.name || '').split(/[\\/]/).pop();
    const folder = (e.path || '').split(/[\\/]/).slice(-2, -1)[0] || '';
    const title = (t && t.title) || file.replace(/\.[^.]+$/, '') || e.name || '';
    const sub = t ? [t.artist, t.album].filter(Boolean).join(' · ') || folder : folder;
    const i = e.info || {};
    const dur = (t && t.durationS) || i.durationS;
    const rate = (t && t.sampleRate) || i.sampleRate;
    const bits = (t && t.bits) || i.bits;
    const fmt = (t && t.format) || i.format || file.split('.').pop().toUpperCase();
    const res = rate ? (bits ? `${bits}/${+(rate / 1000).toFixed(1)}` : `${+(rate / 1000).toFixed(1)}k`) : '';
    return { title, sub, time: fmtTime(dur), tag: [res, fmt].filter(Boolean).join(' ') };
}

const invoke = (cmd, args) => {
    const f = globalThis.window?.__TAURI__?.core?.invoke;
    return f ? f(cmd, args) : Promise.reject('no bridge');
};

// The player's tracks (their tags), asked when the list changes and now and then.
let tracks = [], tracksAt = 0, asking = null;

/// The player's queue as last asked (`player_queue`): [{ id, title, artist, … }].
export const queueTracks = () => tracks;
/// When it was asked (performance.now()), 0 before.
export const queueTracksAt = () => tracksAt;

/// Asks the player's queue again (one ask at a time); the rows follow.
export function askQueue() {
    if (asking) return asking;
    asking = invoke('player_queue')
        .then((q) => { tracks = Array.isArray(q) ? q : []; tracksAt = performance.now(); decorateRows(); })
        .catch(() => {})
        .finally(() => { asking = null; });
    return asking;
}

const el = (tag, cls) => { const n = document.createElement(tag); if (cls) n.className = cls; return n; };

/// Every row of the list with its title, artist, length and format.
export function decorateRows() {
    const byId = new Map(tracks.map((t) => [t.id, t]));
    for (const row of document.querySelectorAll('#fileQueue .pl-item')) {
        const e = state.list.find((x) => String(x.id) === row.dataset.id);
        if (!e) continue;
        const name = row.querySelector('.file-item-name');
        if (!name) continue;
        let box = row.querySelector('.rm-qt');
        if (!box) {
            box = el('span', 'rm-qt');
            box.innerHTML = '<span class="rm-qt-title"></span><span class="rm-qt-sub"></span>';
            name.after(box);
            const meta = el('span', 'rm-qm');
            meta.innerHTML = '<span class="rm-qm-time"></span><span class="rm-qm-tag"></span>';
            box.after(meta);
        }
        const x = rowText(e, byId.get(e.trackId));
        const set = (sel, v) => { const n = row.querySelector(sel); if (n && n.textContent !== v) n.textContent = v; };
        set('.rm-qt-title', x.title);
        set('.rm-qt-sub', x.sub);
        set('.rm-qm-time', x.time);
        set('.rm-qm-tag', x.tag);
        row.classList.toggle('rm-qt-nosub', !x.sub);
    }
}

let watching = false;
/// Keeps the rows decorated from now on: a new list, a row drawn again, a
/// row brought up to date in place. Once.
export function watchQueueRows() {
    const list = document.getElementById('fileQueue');
    if (!list || watching) return;
    watching = true;
    let t = 0;
    const later = () => { if (!t) t = requestAnimationFrame(() => { t = 0; decorateRows(); }); };
    list.addEventListener('aura:list-rendered', () => { later(); askQueue(); });
    list.addEventListener('aura:row-rendered', later);
    new MutationObserver(later).observe(list, { childList: true, subtree: true, characterData: true });
    later();
    askQueue();
}
