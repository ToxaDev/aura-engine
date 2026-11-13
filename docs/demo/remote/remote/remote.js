// The web remote's own part: the app's page, laid out as the approved design
// (top strip; Library or Sound on the left; now playing and the queue on the
// right; on a phone, bottom tabs and a mini player). The rack, the player and the
// list stay the app's own code and elements: they are only moved here.

import { auraUrl } from '../js/aura-url.js';
import { loadSettings, applySettingsDependencies } from '../js/settings.js';
import { syncDspRack } from '../js/dsprack.js';
import { renderStrip } from '../js/rackstrip.js';
import { rackTakenElsewhere } from '../js/player.js';
import { state } from '../js/state.js';
import { isListening, setListening } from '../js/listening.js';

const { invoke, listen } = window.__AURA_REMOTE__;
const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
};
const keep = {
    get(k, d) { try { return localStorage.getItem('remote.' + k) ?? d; } catch (_) { return d; } },
    set(k, v) { try { localStorage.setItem('remote.' + k, v); } catch (_) {} },
};

// ── the layout ──────────────────────────────────────────────────────────
function layout() {
    const panel = $('converterPanel');
    const shell = el('div'); shell.id = 'rmShell';
    const top = el('div'); top.id = 'rmTop';
    const left = el('div'); left.id = 'rmLeft';
    const right = el('div'); right.id = 'rmRight';
    shell.append(top, left, right);

    const take = (parent, ...nodes) => nodes.forEach((n) => n && parent.append(n));
    // The strip's pane drops under the strip, inside the remote's width, on
    // any tab (in the Sound column it was hidden with the column on a phone,
    // and measured against the whole window on a big screen).
    take(top, panel.querySelector('.app-head-bar'), panel.querySelector('h2'), $('rkStrip'), $('rkPop'),
        el('div', 'rm-hint', 'Tap any cell to open Sound at that setting'), $('convFilterAvail'), $('convGpuNote'));

    const head = el('div', 'rm-head');
    const title = el('span', 'rm-title', 'Library');
    const sub = el('span', 'rm-sub');
    const seg = el('div', 'rm-seg');
    for (const v of ['library', 'sound']) {
        const b = el('button', null, v); b.type = 'button'; b.dataset.view = v;
        b.addEventListener('click', () => show(v));
        seg.append(b);
    }
    head.append(title, sub, seg);
    const lib = el('div'); lib.id = 'rmLib';
    const sound = el('div'); sound.id = 'rmSound';
    take(sound, $('labField'), $('labInv'));
    left.append(head, lib, sound);

    const now = el('div'); now.id = 'rmNow';
    now.innerHTML = `<div class="rm-title">Now playing</div>
      <div class="rm-now-row"><img id="rmArt" class="none" alt="" src="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="><div class="rm-now-meta">
        <div class="rm-track" id="rmTrack"></div><div class="rm-now-title" id="rmNowTitle">Nothing playing</div>
        <div class="rm-now-sub" id="rmNowSub"></div><div class="rm-tags" id="rmTags"></div></div></div>`;
    const queue = el('div'); queue.id = 'rmQueue';
    take(queue, el('div', 'rm-upnext', 'Up next'), $('plListHead'), $('dropZone'));
    take(right, now, $('plBar'), queue);
    panel.prepend(shell);

    const tabs = el('nav'); tabs.id = 'rmTabs';
    const scrolls = {};
    for (const v of ['library', 'player', 'sound']) {
        const b = el('button', null, v); b.type = 'button'; b.dataset.tab = v;
        b.addEventListener('click', () => {
            // A tab always leads back to its own page: an open window closes,
            // even under the tab that is already on.
            window.dispatchEvent(new Event('rm:closepage'));
            const from = document.body.dataset.rmTab;
            if (from !== v) scrolls[from] = panel.scrollTop;
            document.body.dataset.rmTab = v; keep.set('tab', v);
            if (v === 'library' || v === 'sound') show(v);
            paintTabs();
            // Each tab keeps where it was scrolled to; a new one starts at its top.
            requestAnimationFrame(() => { room(); if (from !== v) panel.scrollTop = scrolls[v] || 0; });
        });
        tabs.append(b);
    }
    const mini = el('div'); mini.id = 'rmMini';
    mini.innerHTML = `<img id="rmMiniArt" class="none" alt="" src="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="><div class="rm-mini-meta"><div id="rmMiniTitle"></div><div id="rmMiniSub"></div></div><button type="button" id="rmMiniPlay" aria-label="Play or pause"></button><i id="rmMiniBar"></i>`;
    mini.querySelector('#rmMiniPlay').addEventListener('click', () => $('plPlay')?.click());
    mini.addEventListener('click', (e) => { if (!e.target.closest('button')) tabs.querySelector('[data-tab="player"]').click(); });
    document.body.append(mini, tabs);

    // The player bar is the dock (remote.css): always at the bottom, the
    // page and the window pages end above it. Their room follows its height.
    const root = document.documentElement;
    const dockMq = matchMedia('(max-width: 1199px), (hover: none) and (pointer: coarse)');
    const room = () => {
        if (folding) return;
        const bar = $('plBar');
        // Out of the column (hidden on a phone's other tabs) into the page's foot, and back.
        if (bar) {
            if (dockMq.matches && bar.parentElement !== document.body) document.body.append(bar);
            else if (!dockMq.matches && bar.parentElement !== right) right.insertBefore(bar, queue);
        }
        const fixed = bar && getComputedStyle(bar).position === 'fixed';
        root.style.setProperty('--rm-dock', fixed ? bar.offsetHeight + 'px' : '0px');
        root.style.setProperty('--rm-tabs', getComputedStyle(tabs).display !== 'none' ? tabs.offsetHeight + 'px' : '0px');
        root.style.setProperty('--rm-head', top.offsetHeight + 'px');
        fitQueue();
        fitLib();
        // Once the list fits the room, the page under the still head has nothing to scroll: lock it, so a wheel or a
        // finger on the head, the tabs or the chips does not move the page (a second scroll) — only the list scrolls.
        const fit = phoneMq.matches && dockMq.matches && ['library', 'player'].includes(document.body.dataset.rmTab);
        document.body.classList.toggle('rm-nopanel', fit && panel.scrollHeight - panel.clientHeight < 1);
    };
    // On a phone the head stays put (Anton 10.11): the version, the title and
    // the strip; what is under it scrolls and fades out where it meets it.
    // The Library keeps its search, its tabs and its chips too and scrolls
    // only its albums (or whatever the list is), the Player only its queue.
    const libList = () => panel.querySelector('#rmLib .lb-body');
    let libTop = 0;
    const fitLib = () => {
        const body = libList();
        if (!body) return;
        const on = phoneMq.matches && dockMq.matches && document.body.dataset.rmTab === 'library';
        document.body.classList.toggle('rm-libfit', on);
        body.style.removeProperty('height');
        if (!on) return;
        // From the list's own height each time: a fit taken while it was still empty must not stay
        let h = body.offsetHeight;
        for (let i = 0; i < 3; i++) {
            const over = panel.scrollHeight - panel.clientHeight;
            if (Math.abs(over) < 1 && i) break;
            h = Math.max(120, h - over);
            body.style.setProperty('height', h + 'px', 'important');
        }
        if (body.scrollTop !== libTop) body.scrollTop = libTop;
        if (panel.scrollTop) panel.scrollTop = 0;
    };
    // A fade where the content goes under the head: only once it has.
    const under = (n, cls, on) => n.classList.toggle(cls, on);
    panel.addEventListener('scroll', () => under(document.body, 'rm-under', panel.scrollTop > 1), { passive: true });
    panel.addEventListener('scroll', (e) => {
        const t = e.target;
        if (t.classList?.contains('lb-body')) { libTop = t.scrollTop; under(t, 'rm-under', t.scrollTop > 1); }
        else if (t.id === 'fileQueue') under(t, 'rm-under', t.scrollTop > 1);
    }, { passive: true, capture: true });
    // On a phone's Player tab the list takes the room down to the player and
    // scrolls in itself: the page has nothing below it to run on into.
    const phoneMq = matchMedia('(max-width: 759px)');
    const fitQueue = () => {
        const list = $('fileQueue'), zone = $('dropZone');
        if (!list || !zone) return;
        const on = phoneMq.matches && dockMq.matches && document.body.dataset.rmTab === 'player';
        const keep = list.scrollTop;
        for (const n of [list, zone]) n.style.removeProperty('height');
        if (!on) return;
        const row = parseFloat(getComputedStyle(root).getPropertyValue('--pl-row-h')) || 58;
        // From the queue's own height each time, as the Library does
        let h = zone.offsetHeight;
        for (let i = 0; i < 3; i++) {
            const over = panel.scrollHeight - panel.clientHeight;
            if (Math.abs(over) < 1 && i) break;
            h = Math.max(row * 2, h - over);
            for (const n of [list, zone]) n.style.setProperty('height', h + 'px', 'important');
        }
        // A refit (a new row, a tick of the player) keeps the queue where the listener scrolled it
        if (list.scrollTop !== keep) list.scrollTop = keep;
        if (panel.scrollTop) panel.scrollTop = 0;
    };
    if ('ResizeObserver' in window) { const ro = new ResizeObserver(room); [$('plBar'), tabs].forEach((n) => n && ro.observe(n)); }
    window.addEventListener('resize', room);
    requestAnimationFrame(room);
    // The Library's list comes after the first fit (it waits for the server), and its head can change (a back link,
    // the chips): fit again whenever the panel's content changes, or the whole page would scroll under a still head.
    let refit = 0;
    new MutationObserver(() => {
        if (refit || !phoneMq.matches) return;
        refit = requestAnimationFrame(() => { refit = 0; room(); });
    }).observe(panel, { childList: true, subtree: true });

    // A cell of the strip opens the Sound view, its control at the top of it.
    $('rkStrip')?.addEventListener('click', () => show('sound'), true);

    let libSub = '';
    function show(v) {
        document.body.dataset.rmView = v;
        keep.set('view', v);
        title.textContent = v;
        sub.textContent = v === 'sound' ? 'changes apply live, the music keeps playing' : libSub;
        seg.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.view === v));
        if (document.body.dataset.rmTab === 'library' || document.body.dataset.rmTab === 'sound') {
            document.body.dataset.rmTab = v;
            paintTabs();
        }
    }
    function paintTabs() {
        tabs.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.tab === document.body.dataset.rmTab));
    }
    document.body.dataset.rmTab = keep.get('tab', 'library') === 'queue' ? 'player' : keep.get('tab', 'library');
    show(keep.get('view', 'library'));
    paintTabs();
    // The library is its own module (library.js).
    const setSub = (t) => { libSub = t || ''; if (document.body.dataset.rmView === 'library') sub.textContent = libSub; };
    import('./library.js')
        .then((m) => m.buildLibrary(lib, { setSub }))
        .catch((e) => { console.warn('[remote] library.js:', e); lib.textContent = 'The library did not load: ' + e; });
}

// ── now playing and the mini player ─────────────────────────────────────
let artId = null;
function paintNow(s, queue) {
    // A status can come before the page is laid out (remote.js builds it after the app's boot).
    if (!$('rmTrack')) return;
    const id = s.trackId ?? s.track ?? s.current ?? null;
    const t = queue.find((x) => x.id === id);
    const i = t ? queue.indexOf(t) : -1;
    const file = t ? t.path.split(/[\\/]/).pop() : '';
    const name = t ? (t.title || t.name || file) : 'Nothing playing';
    // No tags yet (the library comes next): the folder stands in for the album.
    const folder = t ? t.path.split(/[\\/]/).slice(-2, -1)[0] || '' : '';
    const sub = t ? ([t.artist, t.album, t.year].filter(Boolean).join(' · ') || folder) : '';
    $('rmTrack').textContent = t ? `Track ${i + 1} of ${queue.length}` : '';
    $('rmNowTitle').textContent = name;
    $('rmNowSub').textContent = sub;
    const tags = $('rmTags');
    tags.replaceChildren();
    let out = '';
    if (t) {
        const ext = (t.format || file.split('.').pop() || '').toUpperCase();
        const rate = t.sampleRate ? +(t.sampleRate / 1000).toFixed(1) : '';
        tags.append(el('span', 'rm-tag src', [t.bits && rate ? `${t.bits}/${rate}` : '', ext].filter(Boolean).join(' ')));
        if (s.state === 'playing' || s.state === 'paused') {
            out = s.dsd ? (typeof s.dsd === 'string' ? s.dsd : s.dsd.label || s.dsd.name || 'DSD')
                : s.outRate ? `${+(s.outRate / 1000).toFixed(1)}k` : '';
            if (out) tags.append(el('span', 'rm-tag out', '→ ' + out));
        }
    }
    $('rmMiniTitle').textContent = name;
    $('rmMiniSub').textContent = [t ? (t.artist || folder) : '', out && '→ ' + out].filter(Boolean).join(' · ');
    if (id !== artId) {
        artId = id;
        paintArt(id);
    }
    const pos = s.positionS ?? 0, dur = s.durationS ?? 0;
    $('rmMiniBar').style.width = dur > 0 ? (100 * pos / dur).toFixed(2) + '%' : '0';
    // Only on a change: any write to the body's class closes the strip's pane
    // in the big player (rackstrip.js takes it for the rack folding away).
    const playing = s.state === 'playing';
    if (document.body.classList.contains('rm-playing') !== playing) document.body.classList.toggle('rm-playing', playing);
}
/// The cover through fetch: a track without one is not an error on the page
/// (an <img> that fails is reported to the engine's log as one).
let artUrl = null;
async function paintArt(id) {
    const arts = [$('rmArt'), $('rmMiniArt')];
    let url = null;
    if (id != null) {
        try {
            const r = await fetch(auraUrl(`/player/cover?id=${id}`));
            if (r.ok) url = URL.createObjectURL(await r.blob());
        } catch (_) {}
    }
    if (id !== artId) { if (url) URL.revokeObjectURL(url); return; }
    if (artUrl) URL.revokeObjectURL(artUrl);
    artUrl = url;
    for (const a of arts) {
        // No cover: an empty picture, not a missing one (a missing one draws its broken frame).
        a.src = url || 'data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==';
        a.classList.toggle('none', !url);
    }
}
// The status comes with the page's own poll (shim.js passes it on): no
// second poll of it here. The queue only when the track is not in the one
// held, the list changed, or now and then.
let queue = [], queueAt = 0, queueAsk = null;
function askQueue() {
    if (queueAsk) return queueAsk;
    queueAsk = invoke('player_queue')
        .then((q) => { queue = Array.isArray(q) ? q : []; queueAt = performance.now(); decorateRows(); })
        .catch(() => {})
        .finally(() => { queueAsk = null; });
    return queueAsk;
}
let lastStatus = null;
// Stopped, HP's background under its unit goes, as nothing is heard to
// draw (a pause keeps it, its clock still, as in the app).
let hpView = null, wasState = null;
// The dock folding (dockFold): it sets the page's room itself, frame by frame.
let folding = false;
function clearHpOnStop(state) {
    if (state === wasState) return;
    wasState = state;
    if (state !== 'stopped') return;
    (hpView ||= import('../js/hpview.js')).then((m) => m.hpViewClear()).catch(() => {});
}
async function takeStatus(s) {
    if (!s) return;
    lastStatus = s;
    clearHpOnStop(s.state);
    const id = s.trackId ?? s.track ?? s.current ?? null;
    if ((id != null && !queue.some((t) => t.id === id)) || performance.now() - queueAt > 15000) await askQueue();
    paintNow(s, queue);
}
async function pollNow() {
    // Only while the page's own poll is quiet (a hidden tab, a page that has
    // not started it yet).
    if (lastStatus && performance.now() - statusAt < 2500) return;
    try { takeStatus(await fetch(auraUrl('/player/status')).then((r) => r.json())); } catch (_) {}
}
let statusAt = 0;
window.addEventListener('aura-remote-status', (e) => { statusAt = performance.now(); takeStatus(e.detail); });

// ── the list's rows: the title and the artist from the tags ─────────────
const fmtTime = (s) => {
    if (!(s > 0)) return '';
    s = Math.round(s);
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${x}` : `${m}:${x}`;
};
function rowText(e, t) {
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
function decorateRows() {
    const byId = new Map(queue.map((t) => [t.id, t]));
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
function watchRows() {
    const list = $('fileQueue');
    if (!list) return;
    let t = 0;
    const later = () => { if (!t) t = requestAnimationFrame(() => { t = 0; decorateRows(); }); };
    list.addEventListener('aura:list-rendered', () => { later(); askQueue(); });
    list.addEventListener('aura:row-rendered', later);
    // A row brought up to date in place puts its own texts back.
    new MutationObserver(later).observe(list, { childList: true, subtree: true, characterData: true });
    later();
}

// ── another device changed something ────────────────────────────────────
function followOthers() {
    let again = 0;
    listen('kv:changed', ({ payload }) => {
        if (payload.key !== 'auraSettings') {
            // A list changed: the app's reload path re-links its rows to the queue.
            if (/^aura(Library|Playlists)/.test(payload.key)) { clearTimeout(again); again = setTimeout(() => location.reload(), 500); }
            return;
        }
        // The rack as the other device left it, without sending it back.
        loadSettings();
        applySettingsDependencies?.();
        syncDspRack();
        renderStrip();
        rackTakenElsewhere();
    });
}

// ── the app's windows as pages ──────────────────────────────────────────
// What is a window in the app (the analyzer, CUE, XTC, the EQ, the load) is
// a page here (Anton 8.11): it takes the whole screen, with a bar on top that
// has Back and the window's name. Back, the browser's or the phone's back,
// Esc, or the window's own Close / Cancel / Save all come back to the remote.
const PAGES = [
    [/^analytics\.html/, 'Analyzer'],
    [/^cue\.html/, 'Cue Consistency'],
    [/^xtc-geometry\.html/, 'Crosstalk Cancel'],
    [/^eq\.html/, 'Equalizer'],
    [/^perf\.html/, 'Performance'],
];
const pageName = (page) => (PAGES.find(([re]) => re.test(page)) || [null, 'Aura'])[1];
function overlays() {
    let open = null;   // { box, page }
    const close = () => {
        if (!open) return;
        open.box.remove();
        open = null;
        document.body.classList.remove('rm-paged');
        $('rmPageBack')?.blur();
    };
    // Back goes through the browser's history, so its own back does the same.
    const back = () => {
        if (!open) return;
        if (history.state && history.state.rmPage) history.back();
        else close();
    };
    window.addEventListener('popstate', () => { if (open && !(history.state && history.state.rmPage)) close(); });
    listen('remote:open', ({ payload }) => {
        const page = String(payload.page || '');
        if (open) { open.box.remove(); open = null; }
        const box = el('div'); box.id = 'rmOverlay';
        box.dataset.page = page.replace(/[.?].*$/, '');
        const bar = el('header', 'rm-page-bar');
        const btn = el('button', 'rm-back'); btn.type = 'button'; btn.id = 'rmPageBack';
        btn.innerHTML = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 3L5 8l5 5"/></svg><span>Back</span>';
        btn.addEventListener('click', back);
        const name = el('div', 'rm-page-title', pageName(page));
        const now = el('div', 'rm-page-now', $('rmNowTitle')?.textContent && $('rmNowTitle').textContent !== 'Nothing playing' ? $('rmNowTitle').textContent : '');
        bar.append(btn, name, now);
        const f = el('iframe'); f.src = '/' + page; f.title = pageName(page);
        box.append(bar, f);
        document.body.append(box);
        document.body.classList.add('rm-paged');
        if (!(history.state && history.state.rmPage)) history.pushState({ rmPage: page }, '');
        else history.replaceState({ rmPage: page }, '');
        open = { box, page };
        f.addEventListener('load', () => { try { f.contentWindow.focus(); } catch (_) {} });
    });
    window.addEventListener('message', (m) => { if (m.data && m.data.remoteClose) back(); });
    window.addEventListener('rm:closepage', back);
    window.addEventListener('keydown', (e) => { if (open && e.key === 'Escape') { e.preventDefault(); back(); } });
}

/// The docked player folds down to one thin row (transport, title and the
/// line of the track) and back. A finger drawn over it moves it along with
/// the finger; let go, it settles open or folded, whichever is nearer. A tap
/// on its empty part or on the title does the same in one go.
/// Every frame of it sets everything that follows the bar's height at once
/// — the parts going, the line thinning, the page's room above (--rm-dock)
/// and the Player tab's list — so nothing waits a frame for a resize to be
/// seen (the page above used to jump after the bar, and again at the end).
function dockFold() {
    const bar = $('plBar');
    if (!bar) return;
    const body = document.body, root = document.documentElement;
    const seek = $('plSeek');
    const parts = () => [bar.querySelector('.pl-dev'), $('plStatus')].filter(Boolean);
    const folded = () => body.classList.contains('rm-dock-min');
    const fitted = () => [$('fileQueue'), $('dropZone')].filter((n) => n && n.style.getPropertyValue('height'));
    const clear = () => {
        for (const n of parts()) for (const k of ['maxHeight', 'opacity', 'marginTop', 'marginBottom', 'overflow']) n.style[k] = '';
        if (seek) for (const k of ['height', 'marginTop', 'marginBottom']) seek.style[k] = '';
    };
    const set = (on) => { clear(); body.classList.toggle('rm-dock-min', on); keep.set('dockMin', on ? '1' : '0'); };
    set(keep.get('dockMin', '0') === '1');
    // Both ends measured in one go (no frame drawn between): the parts'
    // heights, the line's height and margins, the bar's height.
    let m = null;
    const measure = () => {
        const was = folded();
        clear();
        body.classList.remove('rm-dock-min');
        const gap = parseFloat(getComputedStyle(bar).rowGap) || 0;
        const line = () => { const c = getComputedStyle(seek); return [seek.offsetHeight, parseFloat(c.marginTop) || 0, parseFloat(c.marginBottom) || 0]; };
        m = { gap, hs: parts().map((n) => { const c = getComputedStyle(n); return [n, n.offsetHeight, parseFloat(c.marginTop) || 0, parseFloat(c.marginBottom) || 0]; }), s0: seek ? line() : null, d0: bar.offsetHeight };
        body.classList.add('rm-dock-min');
        m.s1 = seek ? line() : null; m.d1 = bar.offsetHeight;
        body.classList.toggle('rm-dock-min', was);
        m.lists = fitted().map((n) => [n, parseFloat(n.style.getPropertyValue('height'))]);
        m.dock = was ? m.d1 : m.d0;
        // What the parts' own padding and borders keep at the end (max-height
        // leaves them): the last one's margin takes it back.
        m.fix = 0;
        body.classList.remove('rm-dock-min');
        paint(1);
        m.fix = bar.offsetHeight - m.d1;
        clear();
        body.classList.toggle('rm-dock-min', was);
    };
    const lerp = (a, b, p) => a + (b - a) * p;
    // p: 0 open .. 1 folded, drawn in the open layout with inline styles.
    let p = 0;
    const paint = (q) => {
        m.hs.forEach(([n, h, mt, mb], i) => {
            n.style.overflow = 'hidden';
            n.style.maxHeight = (1 - q) * h + 'px';
            n.style.opacity = String(Math.max(0, 1 - q * 1.6));
            // the bar's gap closes too
            n.style.marginTop = lerp(mt, -m.gap - (i === m.hs.length - 1 ? m.fix : 0), q) + 'px';
            n.style.marginBottom = lerp(mb, 0, q) + 'px';
        });
        if (m.s0) {
            seek.style.height = lerp(m.s0[0], m.s1[0], q) + 'px';
            seek.style.marginTop = lerp(m.s0[1], m.s1[1], q) + 'px';
            seek.style.marginBottom = lerp(m.s0[2], m.s1[2], q) + 'px';
        }
    };
    const show = (q) => {
        p = q;
        body.classList.remove('rm-dock-min');
        paint(q);
        const d = bar.offsetHeight;
        root.style.setProperty('--rm-dock', d + 'px');
        for (const [n, h] of m.lists) n.style.setProperty('height', h + (m.dock - d) + 'px', 'important');
    };
    const done = (on) => {
        set(on);
        folding = false;
        m = null;
        window.dispatchEvent(new Event('resize'));   // the room as the layout now has it
    };
    const ease = (k) => (k < 0.5 ? 4 * k * k * k : 1 - Math.pow(-2 * k + 2, 3) / 2);
    let anim = 0;
    const settle = (on) => {
        if (!m) { folding = true; measure(); p = folded() ? 1 : 0; }
        cancelAnimationFrame(anim);
        const from = p, to = on ? 1 : 0;
        const ms = 300 * Math.abs(to - from) + 60;
        let t0 = null;
        const step = (ts) => {
            t0 ??= ts;
            const k = Math.min(1, (ts - t0) / ms);
            show(lerp(from, to, ease(k)));
            if (k < 1) anim = requestAnimationFrame(step);
            else done(on);
        };
        anim = requestAnimationFrame(step);
    };
    const free = (t) => !t.closest('button, input, select, a, .pl-vol-wrap, .pl-devfmt') && !(t.closest('#plSeek') && !folded());
    let g = null, eat = 0;
    bar.addEventListener('pointerdown', (e) => {
        g = null;
        if (bar.parentElement !== body || e.button) return;
        if (e.target.closest('#plSeek') && !folded()) return;
        if (e.target.closest('input, select, .pl-vol-wrap')) return;
        g = { y: e.clientY, x: e.clientX, id: e.pointerId, moving: false };
    });
    bar.addEventListener('pointermove', (e) => {
        if (!g || e.pointerId !== g.id) return;
        const dx = e.clientX - g.x, dy = e.clientY - g.y;
        if (!g.moving) {
            if (Math.abs(dy) < 8 || Math.abs(dy) < Math.abs(dx) * 1.3) return;
            g.moving = true;
            cancelAnimationFrame(anim);
            if (!m) { folding = true; measure(); p = folded() ? 1 : 0; }
            g.p0 = p; g.y = e.clientY;
            g.span = Math.max(40, m.d0 - m.d1);
        }
        show(Math.min(1, Math.max(0, g.p0 + (e.clientY - g.y) / g.span)));
    });
    const end = (e) => {
        if (!g || e.pointerId !== g.id) return;
        const d = g; g = null;
        if (!d.moving) return;
        eat = performance.now() + 400;   // the lift that ends a pull is not a tap
        settle(p > 0.5);
    };
    bar.addEventListener('pointerup', end);
    bar.addEventListener('pointercancel', end);
    bar.addEventListener('click', (e) => {
        if (performance.now() < eat) { e.stopPropagation(); e.preventDefault(); return; }
        if (bar.parentElement !== body || !free(e.target)) return;
        settle(m ? p < 0.5 : !folded());
    }, true);
}

/// Full screen on a phone or a tablet: the browser's bar and the system's
/// buttons go, the page takes all the glass. A browser lets a page go full
/// screen only on a touch, so the first touch anywhere does it (unless it
/// was turned off here before); the button at the top right turns it off and
/// on, and is remembered. Where a page cannot go full screen (an iPhone),
/// no button.
function fullScreen() {
    const root = document.documentElement;
    const can = !!(root.requestFullscreen && document.fullscreenEnabled);
    const coarse = matchMedia('(hover: none) and (pointer: coarse)');
    if (!coarse.matches || matchMedia('(display-mode: fullscreen), (display-mode: standalone)').matches) return;
    // An iPhone's Safari has no full screen for a page: no button there.
    if (!can) return;
    const on = () => !!document.fullscreenElement;
    const go = () => root.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
    const btn = el('button', 'rm-fs');
    btn.type = 'button';
    btn.innerHTML = '<svg viewBox="0 0 28 28" aria-hidden="true"><path class="o" d="M3 9.5V3h6.5M18.5 3H25v6.5M25 18.5V25h-6.5M9.5 25H3v-6.5"/><path class="i" d="M9 3V9H3M25 9H19V3M19 25V19H25M3 19H9V25"/></svg>';
    const paint = () => {
        btn.classList.toggle('on', on());
        btn.setAttribute('aria-label', on() ? 'Exit full screen' : 'Full screen');
    };
    btn.addEventListener('click', (e) => {
        e.stopPropagation();
        if (on()) { keep.set('fullScreen', '0'); document.exitFullscreen().catch(() => {}); }
        else { keep.set('fullScreen', '1'); go(); }
    });
    $('rmTop')?.append(btn);
    // Chrome puts its own note over the page's foot on going full screen
    // ("swipe down to exit…") and keeps it there a while; no page can take it
    // off, so the player and the tabs step up over it meanwhile, and come back
    // down when it is gone (10 s: Anton's phone shows it about that long).
    let lift = 0;
    const lifted = (on) => { clearTimeout(lift); document.body.classList.toggle('rm-fs-lift', on); if (on) lift = setTimeout(() => lifted(false), 10000); };
    document.addEventListener('fullscreenchange', () => { paint(); lifted(on()); });
    paint();
    // The first touch, if full screen is wanted (the default).
    const first = () => {
        window.removeEventListener('pointerup', first, true);
        if (keep.get('fullScreen', '1') === '1' && !on()) go();
    };
    window.addEventListener('pointerup', first, true);
}

/// The controls never step aside: the app's player hides them a while after
/// the mouse leaves (the spectrum's showcase); on a remote there is no mouse to
/// bring them back, so the showcase is taken off before it is drawn.
function keepControls() {
    const top = document.querySelector('#plBar .pl-top');
    if (!top) return;
    const off = () => { if (top.classList.contains('pl-showcase')) top.classList.remove('pl-showcase'); };
    new MutationObserver(off).observe(top, { attributes: true, attributeFilter: ['class'] });
    off();
}

/// The strip's pane no wider than a pane needs, under the cell that opened it
/// (the app's window is 440 px: there it is the strip's width), its arrow
/// still on that cell.
function fitStripPane() {
    const pop = $('rkPop'), strip = $('rkStrip');
    if (!pop || !strip) return;
    const fit = () => {
        if (pop.hidden || !pop.dataset.cell) return;
        const cell = strip.querySelector(`.rk-cell[data-cell="${pop.dataset.cell}"]`);
        const box = pop.offsetParent;
        if (!cell || !box) return;
        const room = box.clientWidth;
        const w = Math.min(room, 640);
        const b = box.getBoundingClientRect(), c = cell.getBoundingClientRect();
        const mid = c.left - b.left + c.width / 2;
        const left = Math.round(Math.max(0, Math.min(room - w, mid - w / 2)));
        const set = (k, v) => { if (pop.style.getPropertyValue(k) !== v) pop.style.setProperty(k, v); };
        set('left', left + 'px');
        set('width', w + 'px');
        set('right', 'auto');
        set('--rk-caret', Math.round(mid - left) + 'px');
    };
    new MutationObserver(fit).observe(pop, { attributes: true, attributeFilter: ['hidden', 'data-cell', 'style'] });
    window.addEventListener('resize', fit);
}

/// No big player on the streamer (Anton 8.11): the app's listening mode,
/// its ^ strip and its L key are taken away; a mode remembered from before
/// is put back to the studio's layout at once.
function noListening() {
    const off = () => { if (isListening()) setListening(false, false, false); };
    off();
    new MutationObserver(off).observe(document.body, { attributes: true, attributeFilter: ['class'] });
    window.addEventListener('keydown', (e) => { if (e.code === 'KeyL' && !e.ctrlKey && !e.altKey && !e.metaKey) e.stopImmediatePropagation(); }, true);
}

function start() {
    document.body.classList.add('rm');
    // HP's background straight at the heard frame, no catching up (hpview.js).
    (hpView ||= import('../js/hpview.js')).then((m) => m.hpViewCatchUp?.(false)).catch(() => {});
    noListening();
    keepControls();
    fitStripPane();
    layout();
    dockFold();
    fullScreen();
    overlays();
    followOthers();
    watchRows();
    askQueue().then(pollNow);
    setInterval(pollNow, 1000);
}
const waitBoot = setInterval(() => {
    if (!window.__auraBooted) return;
    clearInterval(waitBoot);
    start();
}, 50);
