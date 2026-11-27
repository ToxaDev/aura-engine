// ══════════════════════════════════════════════════════════════════════
// The library's Conversions page: every converted file the library knows —
// the space they take, the files with their versions as a tree, each version
// played as it is or deleted.
//
// On top the space: one bar, a piece a kind (FS8, DSD256 …), and the kinds
// as chips that filter. Under it the search and the order. Left the source
// files that have versions; right the chosen file's versions as a tree —
// the kind, then each version with what it was made with, its size and age,
// and Play, Show in folder, Delete (for good, asked first). A
// version made by an older generation of the sound is marked outdated (the
// backend's stamp check).
//
// A version is never a track of its own: Play puts its ORIGINAL in the
// queue and has that row play this file (play.js playVersion). A version
// still being written is marked and cannot be played or deleted. Versions of
// an original that is gone are deleted by themselves (orphans.js);
// those the app has no record of making wait for a click here.
// ══════════════════════════════════════════════════════════════════════

import { state } from '../state.js';
import { renderFileQueue } from '../dropzone.js';
import { getPlaylists, checkConvRecords, rackSig, cueForcesLive, volumeFull, forgetVersions, forgetStampMatches } from '../library.js';
import { confirmAt } from '../confirm-pop.js';
import { playVersion, playingVersion, toggleVersion } from './play.js';
import { playBlock } from './versions-core.js';
import { isWriting, liveConversions, sweepOrphans, takeNote } from './orphans.js';
import {
    groupFiles, versionTree, usageOf, pick, typeOf, versionLine, stagesLine, sizeText, whenText, foldersOf,
} from './conv-model.js';
import { markShiftHtml } from '../ideal-taps.js';

const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
// The streamer's web remote: no file manager to show a file in.
const CAN_REVEAL = !window.__AURA_REMOTE__;
const SORT_KEY = 'auraConvSort';
const CHECK_CHUNK = 200;
/// Sources asked of the stamps at once (conv_matching).
const STAMP_CHUNK = 300;

const TEXTS = {
    loading: 'Looking for converted files…',
    none: 'No converted files yet.',
    noneSub: 'Convert an album or a track (the C button) and its versions appear here.',
    nothing: 'Nothing matches.',
    sum: (n, size, src) => `${n} converted file${n === 1 ? '' : 's'} · ${size}${src ? ` · their sources ${src}` : ''}`,
    rescan: 'Rescan',
    rescanTip: 'Look through the folders again',
    all: 'All',
    search: 'Search files',
    sort: { recent: 'Recent', name: 'Name', size: 'Size' },
    versions: n => `${n} version${n === 1 ? '' : 's'}`,
    pick: 'Choose a file on the left to see its versions.',
    source: 'Source',
    gone: 'the source file is not there any more',
    play: 'Play',
    playTip: 'Play this version under its original: the original goes to the queue, the output switches to this format and the file plays as it is',
    block: {
        writing: 'Being converted: it can be played once it is finished',
        gone: 'Its original is not there any more: there is nothing to play it under',
        outdated: 'Made by an older version of the sound: the player does not take it — convert again',
        cue: 'Cue Consistency is on: every track plays live through it. Turn it off to play a converted file',
        rate: 'A DSD file above DSD256 does not go out as it is yet',
        volume: 'A DSD file takes no volume: set the volume to its top to play it as it is',
        output: 'This output has no such format: the device or this PC does not take it',
        row: 'The player could not take the original of this file.',
        live: 'The player did not take this file as it is: the original plays live',
    },
    writing: 'CONVERTING',
    writingTip: 'This file is being written now',
    delGone: (n, size) => `Delete versions without an original (${n} · ${size})`,
    delGoneAsk: (n, size) => `Delete ${n} file${n === 1 ? '' : 's'} whose original is gone (${size}) for good?`,
    reveal: 'Show in folder',
    del: 'Delete',
    delTip: 'Deleted for good, not to the Recycle Bin',
    delAsk: 'Delete this version for good?',
    delKind: (t, n, size) => `Delete all ${t} (${n} · ${size})`,
    delKindAsk: (t, n, size) => `Delete ${n} ${t} file${n === 1 ? '' : 's'} (${size}) for good?`,
    delOld: (n, size) => `Delete outdated (${n} · ${size})`,
    delOldAsk: (n, size) => `Delete ${n} outdated file${n === 1 ? '' : 's'} (${size}) for good?`,
    outdated: 'OUTDATED',
    outdatedTip: 'Made by an older version of the sound — convert again',
    current: 'CURRENT RACK',
    currentTip: 'Made with the settings the rack has now',
    nowPlaying: 'PLAYING',
    pause: 'Pause',
    resume: 'Play',
    pauseTip: 'Pause this version',
    resumeTip: 'Go on playing this version from where it stopped',
    deleted: (n, size) => `${n} file${n === 1 ? '' : 's'} deleted, ${size} free.`,
    failed: (n, why) => `${n} could not be deleted${why ? ': ' + why : ''}.`,
    busy: 'The file that plays stays: stop it first.',
    busyWriting: 'A file that is being converted stays.',
};

const ICON = {
    play: '<svg viewBox="0 0 24 24"><path d="M7 4.5v15l12-7.5z"/></svg>',
    pause: '<svg viewBox="0 0 24 24"><path d="M6 4.5h4v15H6zM14 4.5h4v15h-4z"/></svg>',
    folder: '<svg class="ln" viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>',
    bin: '<svg class="ln" viewBox="0 0 24 24"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>',
    again: '<svg class="ln" viewBox="0 0 24 24"><path d="M20 12a8 8 0 1 1-2.3-5.6M20 4v5h-5"/></svg>',
    find: '<svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/></svg>',
};

// A kind's colour on the bar and its chip: PCM blue to green, DSD violet.
const PCM_HUES = ['#38bdf8', '#22d3ee', '#2dd4bf', '#4ade80', '#a3e635', '#facc15'];
const DSD_HUES = ['#a78bfa', '#c084fc', '#e879f9', '#f472b6', '#818cf8'];
function hues(types) {
    const out = new Map();
    let p = 0, d = 0;
    for (const t of types) out.set(t.type, t.kind === 'dsd' ? DSD_HUES[d++ % DSD_HUES.length] : PCM_HUES[p++ % PCM_HUES.length]);
    return out;
}

const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
function el(tag, cls, html) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (html != null) n.innerHTML = html;
    return n;
}
const low = p => String(p || '').toLowerCase();

/// The page's node (tools.js).
export function conversionsPage(ctx) {
    const node = el('div', 'ml-convp');
    const top = el('div', 'ml-cv-top');
    const tools = el('div', 'ml-cv-tools');
    const panes = el('div', 'ml-cv-panes');
    const list = el('div', 'ml-cv-list');
    const tree = el('div', 'ml-cv-tree');
    panes.append(list, tree);
    node.append(top, tools, panes);

    const S = {
        files: null, groups: [], q: '', type: null, sel: null,
        sort: (() => { try { return localStorage.getItem(SORT_KEY) || 'recent'; } catch (_) { return 'recent'; } })(),
        checks: new Map(),      // path (lower) → { valid, tip }
        current: new Set(),     // paths (lower) made with the rack as it is
        playing: null,
        paused: false,          // the version on the air is paused
        live: '',               // the conversions that run, as last seen
    };
    let seq = 0, timer = 0, left = false;
    const liveKey = () => liveConversions().map(c => c.path).join('|');
    const playingNow = () => { try { return playingVersion(ctx.status()); } catch (_) { return null; } };
    const pausedNow = () => { try { return ctx.status()?.state === 'paused'; } catch (_) { return false; } };

    // ── data ────────────────────────────────────────────────────────────
    function knownPaths() {
        const out = state.list.map(e => e.path);
        for (const pl of getPlaylists()) for (const e of pl.entries || []) out.push(e.path);
        return out;
    }
    function markCurrent() {
        S.current = new Set();
        let sig = null;
        try { sig = rackSig(); } catch (_) { return; }
        const take = e => { for (const c of e.convs || []) if (c.sig === sig && c.out) S.current.add(low(c.out)); };
        state.list.forEach(take);
        for (const pl of getPlaylists()) (pl.entries || []).forEach(take);
    }
    /// The versions whose stamps say they were made with the rack as it is,
    /// recorded by a row or not (the streamer has no records): CURRENT RACK
    /// on them too.
    async function markStamped(my) {
        let sig = null;
        try { sig = rackSig(); } catch (_) { return; }
        const sources = [...new Set(S.files.map(f => f.source).filter(Boolean))];
        let any = false;
        for (let i = 0; i < sources.length; i += STAMP_CHUNK) {
            let r;
            try { r = await invoke('conv_matching', { sig, sources: sources.slice(i, i + STAMP_CHUNK) }); } catch (_) { return; }
            if (my !== seq || left) return;
            for (const v of r || []) if (v?.out && !S.current.has(low(v.out))) { S.current.add(low(v.out)); any = true; }
        }
        if (any) paintTree();
    }
    async function load() {
        const my = ++seq;
        if (!S.files) paintTop();
        let r;
        try { r = await invoke('conv_scan', { folders: foldersOf(knownPaths()) }); }
        catch (e) { if (my === seq && !left) { S.files = []; S.groups = []; paint(); ctx.toast(String(e?.message || e)); } return; }
        if (my !== seq || left) return;
        S.files = r.files || [];
        S.groups = groupFiles(S.files);
        forgetStampMatches();
        markCurrent();
        if (S.type && !S.files.some(f => typeOf(f) === S.type)) S.type = null;
        paint();
        check(my);
        markStamped(my);
        sweep(my);
    }
    /// The versions of originals that are gone (orphans.js), behind the
    /// page: what was deleted leaves the page, and the listener
    /// is told.
    async function sweep(my) {
        const kept = takeNote();
        if (kept) ctx.toast(kept);
        const r = await sweepOrphans({ invoke, playing: playingNow, tell: t => { if (!left) ctx.toast(t); } });
        if (r.trashed && my === seq && !left) load();
    }
    /// The stamps, read in pieces behind the page: a version made by an older
    /// generation of the sound gets its mark, and the count for the bulk
    /// delete grows.
    async function check(my) {
        const todo = S.files.map(f => f.path).filter(p => !S.checks.has(low(p)));
        for (let i = 0; i < todo.length; i += CHECK_CHUNK) {
            const part = todo.slice(i, i + CHECK_CHUNK);
            let res;
            try { res = await invoke('conv_check', { paths: part }); } catch (_) { return; }
            if (my !== seq || left) return;
            part.forEach((p, k) => S.checks.set(low(p), { valid: res?.[k]?.valid !== false, tip: res?.[k]?.tip || '' }));
            paintTools();
            paintTree();
        }
    }
    const outdated = v => S.checks.get(low(v.path))?.valid === false;
    const shown = () => pick(S.groups, { q: S.q, type: S.type, sort: S.sort });

    // ── deleting ────────────────────────────────────────────────────────
    async function trash(versions, anchor, text) {
        if (!versions.length) return;
        if (!(await confirmAt(anchor, { text, yes: TEXTS.del, danger: true }))) return;
        const playing = low(playingNow());
        const idle = versions.filter(v => !isWriting(v));
        if (idle.length < versions.length) ctx.toast(TEXTS.busyWriting);
        const paths = idle.map(v => v.path).filter(p => low(p) !== playing);
        if (paths.length < idle.length) ctx.toast(TEXTS.busy);
        if (!paths.length) return;
        let res;
        try { res = await invoke('conv_trash', { paths }); }
        catch (e) { ctx.toast(String(e?.message || e)); return; }
        const gone = new Set(res.filter(r => r.ok).map(r => low(r.path)));
        const bad = res.filter(r => !r.ok);
        const freed = versions.filter(v => gone.has(low(v.path))).reduce((s, v) => s + (Number(v.size) || 0), 0);
        if (gone.size) ctx.toast(TEXTS.deleted(gone.size, sizeText(freed)));
        if (bad.length) ctx.toast(TEXTS.failed(bad.length, bad[0].error));
        // The rows of the list that pointed at these files read as not
        // converted again; the rows that were the files themselves stay
        // (the player says a file is gone when it is asked to play it).
        S.files = S.files.filter(f => !gone.has(low(f.path)));
        S.groups = groupFiles(S.files);
        if (S.type && !S.files.some(f => typeOf(f) === S.type)) S.type = null;
        paint();
        if (gone.size) forgetVersions(gone);
        try { await checkConvRecords(state.list, invoke); renderFileQueue(); } catch (_) {}
    }

    // ── the top: the space ──────────────────────────────────────────────
    function paintTop() {
        top.replaceChildren();
        if (!S.files) { top.append(el('div', 'ml-cv-note', esc(TEXTS.loading))); return; }
        if (!S.files.length) {
            top.append(el('div', 'ml-cv-note', `<b>${esc(TEXTS.none)}</b><span>${esc(TEXTS.noneSub)}</span>`));
            return;
        }
        const u = usageOf(S.files);
        const hue = hues(u.types);
        const head = el('div', 'ml-cv-sum');
        head.append(el('span', '', esc(TEXTS.sum(u.count, sizeText(u.total), u.originals ? sizeText(u.originals) : ''))));
        const again = el('button', 'ml-cv-ib', ICON.again);
        again.type = 'button';
        again.title = TEXTS.rescanTip;
        again.setAttribute('aria-label', TEXTS.rescan);
        again.addEventListener('click', () => { S.checks.clear(); load(); });
        head.append(again);
        const bar = el('div', 'ml-cv-bar');
        for (const t of u.types) {
            const seg = el('i');
            seg.style.flexGrow = String(Math.max(t.share, 0.004));
            seg.style.background = hue.get(t.type);
            seg.title = `${t.type} · ${sizeText(t.size)} · ${t.count}`;
            if (S.type && S.type !== t.type) seg.classList.add('dim');
            seg.addEventListener('click', () => setType(S.type === t.type ? null : t.type));
            bar.append(seg);
        }
        const chips = el('div', 'ml-chips ml-wrap ml-cv-kinds');
        const chip = (label, on, sub, dot, tap) => {
            const b = el('button', 'ml-chip' + (on ? ' on' : ''), `${dot ? `<i style="background:${dot}"></i>` : ''}${esc(label)}${sub ? `<small>${esc(sub)}</small>` : ''}`);
            b.type = 'button';
            b.addEventListener('click', tap);
            chips.append(b);
        };
        chip(TEXTS.all, !S.type, '', '', () => setType(null));
        for (const t of u.types) chip(t.type, S.type === t.type, sizeText(t.size), hue.get(t.type), () => setType(S.type === t.type ? null : t.type));
        top.append(head, bar, chips);
    }
    function setType(t) { S.type = t; paint(); }

    // ── the search, the order, the bulk deletes ─────────────────────────
    let input = null;
    function paintTools() {
        if (!S.files?.length) { tools.replaceChildren(); input = null; return; }
        if (!input) {
            const box = el('label', 'ml-search ml-cv-search', ICON.find);
            input = el('input');
            input.type = 'search';
            input.placeholder = TEXTS.search;
            input.spellcheck = false;
            input.value = S.q;
            input.addEventListener('input', () => { S.q = input.value.trim(); paintList(); paintTree(); paintBulk(); });
            box.append(input);
            const sort = el('div', 'ml-cv-sort');
            for (const k of Object.keys(TEXTS.sort)) {
                const b = el('button', 'ml-chip' + (S.sort === k ? ' on' : ''), esc(TEXTS.sort[k]));
                b.type = 'button';
                b.addEventListener('click', () => {
                    S.sort = k;
                    try { localStorage.setItem(SORT_KEY, k); } catch (_) {}
                    for (const x of sort.children) x.classList.toggle('on', x === b);
                    paintList();
                });
                sort.append(b);
            }
            tools.replaceChildren(box, sort, el('div', 'ml-cv-bulk'));
        }
        paintBulk();
    }
    function paintBulk() {
        const bulk = tools.querySelector('.ml-cv-bulk');
        if (!bulk) return;
        bulk.replaceChildren();
        const vis = shown().flatMap(g => g.versions);
        if (S.type && vis.length) {
            const size = sizeText(vis.reduce((s, v) => s + (Number(v.size) || 0), 0));
            const b = el('button', 'ml-btn ml-cv-danger', ICON.bin + `<span>${esc(TEXTS.delKind(S.type, vis.length, size))}</span>`);
            b.type = 'button';
            b.addEventListener('click', () => trash(vis, b, TEXTS.delKindAsk(S.type, vis.length, size)));
            b.title = b.textContent;
            bulk.append(b);
        }
        const old = vis.filter(outdated);
        if (old.length) {
            const size = sizeText(old.reduce((s, v) => s + (Number(v.size) || 0), 0));
            const b = el('button', 'ml-btn ml-cv-danger', ICON.bin + `<span>${esc(TEXTS.delOld(old.length, size))}</span>`);
            b.type = 'button';
            b.addEventListener('click', () => trash(old, b, TEXTS.delOldAsk(old.length, size)));
            b.title = b.textContent;
            bulk.append(b);
        }
        const lone = vis.filter(v => !v.source);
        if (lone.length) {
            const size = sizeText(lone.reduce((s, v) => s + (Number(v.size) || 0), 0));
            const b = el('button', 'ml-btn ml-cv-danger', ICON.bin + `<span>${esc(TEXTS.delGone(lone.length, size))}</span>`);
            b.type = 'button';
            b.addEventListener('click', () => trash(lone, b, TEXTS.delGoneAsk(lone.length, size)));
            b.title = b.textContent;
            bulk.append(b);
        }
    }

    // ── left: the files ─────────────────────────────────────────────────
    function paintList() {
        list.replaceChildren();
        if (!S.files?.length) return;
        const groups = shown();
        if (!groups.length) { list.append(el('div', 'ml-cv-hint', esc(TEXTS.nothing))); S.sel = null; return; }
        if (!groups.some(g => g.key === S.sel)) S.sel = groups[0].key;
        const frag = document.createDocumentFragment();
        for (const g of groups) {
            const row = el('button', 'ml-cv-file' + (g.key === S.sel ? ' on' : ''),
                `<b>${esc(g.name)}</b><span>${esc(g.dir)}</span><em>${esc(TEXTS.versions(g.versions.length))} · ${esc(sizeText(g.size))}</em>`);
            row.type = 'button';
            row.title = g.source || g.dir;
            row.addEventListener('click', () => {
                S.sel = g.key;
                for (const x of list.children) x.classList.toggle('on', x === row);
                paintTree();
                tree.scrollTop = 0;
            });
            frag.append(row);
        }
        list.append(frag);
    }

    // ── right: the chosen file's versions ───────────────────────────────
    function paintTree() {
        const keep = tree.scrollTop;
        tree.replaceChildren();
        if (!S.files?.length) return;
        const g = shown().find(x => x.key === S.sel);
        if (!g) { tree.append(el('div', 'ml-cv-hint', esc(TEXTS.pick))); return; }
        const head = el('div', 'ml-cv-src');
        head.append(
            el('b', '', esc(g.name)),
            el('span', '', esc(g.dir)),
            el('em', g.source ? '' : 'gone', esc(g.source ? `${TEXTS.source} · ${g.sourceSize != null ? sizeText(g.sourceSize) : ''}` : TEXTS.gone)),
        );
        if (g.source && CAN_REVEAL) {
            const b = iconBtn(ICON.folder, TEXTS.reveal, () => reveal(g.source));
            head.append(b);
        }
        tree.append(head);
        const playing = low(S.playing);
        for (const br of versionTree(g)) {
            const sec = el('div', 'ml-cv-branch ' + br.kind);
            const bh = el('div', 'ml-cv-bh', `<b>${esc(br.type)}</b><span>${esc(TEXTS.versions(br.versions.length))} · ${esc(sizeText(br.size))}</span>`);
            sec.append(bh);
            for (const v of br.versions) {
                const writing = isWriting(v);
                const row = el('div', 'ml-cv-ver' + (low(v.path) === playing ? ' playing' : '') + (outdated(v) ? ' old' : '') + (writing ? ' writing' : ''));
                row.dataset.path = v.path;
                const text = el('div', 'ml-cv-vt');
                const marks = [];
                if (low(v.path) === playing) marks.push(`<i class="pl">${TEXTS.nowPlaying}</i>`);
                if (writing) marks.push(`<i class="wr" title="${esc(TEXTS.writingTip)}">${TEXTS.writing}</i>`);
                else if (outdated(v)) marks.push(`<i class="old" title="${esc(S.checks.get(low(v.path))?.tip || TEXTS.outdatedTip)}">${TEXTS.outdated}</i>`);
                else if (S.current.has(low(v.path))) marks.push(`<i class="cur" title="${esc(TEXTS.currentTip)}">${TEXTS.current}</i>`);
                text.innerHTML = `<b>${markShiftHtml(esc(versionLine(v) || br.type))}${marks.join('')}</b><span>${esc(stagesLine(v))}</span>`;
                text.title = v.path;
                const meta = el('div', 'ml-cv-vm', `<b>${esc(sizeText(v.size))}</b><span>${esc(whenText(v.mtimeMs))}</span>`);
                const acts = el('div', 'ml-cv-va');
                // The version on the air: its button pauses and goes on.
                const onAir = low(v.path) === playing;
                const play = el('button', 'ml-btn pri ml-cv-play' + (onAir ? ' on' : ''),
                    onAir && !S.paused ? ICON.pause + `<span>${esc(TEXTS.pause)}</span>` : ICON.play + `<span>${esc(onAir ? TEXTS.resume : TEXTS.play)}</span>`);
                play.type = 'button';
                // What does not change while the page stands shuts the
                // button; what the listener can change (Cue, the volume) is
                // said on the click.
                const fixed = playBlock(v, { writing, outdated: outdated(v) });
                play.disabled = !!fixed && !onAir;
                play.title = fixed ? TEXTS.block[fixed] : onAir ? (S.paused ? TEXTS.resumeTip : TEXTS.pauseTip) : TEXTS.playTip;
                play.addEventListener('click', async () => {
                    if (onAir && low(playingNow()) === low(v.path)) {
                        toggleVersion(g.source);
                        setTimeout(watch, 250);
                        return;
                    }
                    const why = playBlock(v, { writing: isWriting(v), outdated: outdated(v), cue: cueForcesLive(), volumeFull: volumeFull() });
                    if (why) { ctx.toast(TEXTS.block[why]); return; }
                    play.disabled = true;
                    try {
                        const failed = await playVersion(v, g.source);
                        if (failed) ctx.toast(TEXTS.block[failed]);
                    } catch (e) { ctx.toast(String(e?.message || e)); }
                    play.disabled = false;
                    watch();
                });
                const del = iconBtn(ICON.bin, `${TEXTS.del} — ${TEXTS.delTip}`, () => trash([v], del, TEXTS.delAsk));
                del.classList.add('ml-cv-danger');
                del.disabled = writing;
                acts.append(play, ...(CAN_REVEAL ? [iconBtn(ICON.folder, TEXTS.reveal, () => reveal(v.path))] : []), del);
                row.append(text, meta, acts);
                sec.append(row);
            }
            tree.append(sec);
        }
        tree.scrollTop = keep;
    }
    function iconBtn(icon, tip, tap) {
        const b = el('button', 'ml-cv-ib', icon);
        b.type = 'button';
        b.title = tip;
        b.setAttribute('aria-label', tip);
        b.addEventListener('click', tap);
        return b;
    }
    const reveal = p => invoke('conv_reveal', { path: p }).catch(e => ctx.toast(String(e?.message || e)));

    function paint() { paintTop(); paintTools(); paintList(); paintTree(); }

    /// What plays, looked at once a second: the version that is the playing
    /// row gets its mark.
    function watch() {
        // A conversion that began or ended: its file is new on the page.
        const live = liveKey();
        if (live !== S.live) {
            const ended = S.live.length > live.length || !live;
            S.live = live;
            if (ended) { load(); return; }
            paintTree();
        }
        const now = playingNow();
        const paused = !!now && pausedNow();
        if (low(now) === low(S.playing) && paused === S.paused) return;
        S.playing = now;
        S.paused = paused;
        paintTree();
    }

    node.__after = () => { load(); timer = setInterval(watch, 1000); };
    node.__refresh = () => load();
    node.__leave = () => { left = true; clearInterval(timer); };
    return node;
}
