// ══════════════════════════════════════════════════════════════════════
// The library's Duplicates page (Anton's design, local/tmp/library-dups/
// dups-mock.html, screens 2–3): the copies of the same track in the music
// folders, found by the backend (lib_dupes) — whole folder twins, exact
// copies, the same audio with other tags, and other versions for the eyes
// only. Where to look (the whole library, one music folder, a folder
// from Folders), which copy stays (the Keep rule, or a click on a row's
// round button), what goes (the ticks), and one button that moves the
// ticked files to the Recycle Bin after a confirmation with the list.
//
// The backend guards it all again (lib_trash): inside the music folders,
// music files only, the size and time unchanged, never the last copy of a
// track, nothing that plays or while a conversion runs.
// ══════════════════════════════════════════════════════════════════════

import { lib, statusNow, emit } from './api.js';
import { confirmAt } from '../confirm-pop.js';
import { getPlaylists } from '../library.js';
import { playingPath } from './play.js';
import { sizeText } from './conv-model.js';
import {
    rowsOf, kindCounts, chooseKeep, ticksFor, pathsFor, sizesOf, totals, selection, differPart, confirmLines,
    dateText, fmtWords, ruleText, RULES,
} from './dupes-model.js';

const RULE_KEY = 'auraDupesKeep';
/// Set by Folders ("Find duplicates here"): the page opens on that folder.
export const SCOPE_KEY = 'auraDupesScope';
const CHUNK = 80;
const CAN_REVEAL = !window.__AURA_REMOTE__;

const TEXTS = {
    title: 'DUPLICATES',
    lookIn: 'LOOK IN', keep: 'KEEP',
    whole: 'Whole library',
    again: '↻ Search again',
    loading: 'Looking for duplicates…',
    none: 'No duplicates here.',
    noneSub: 'Every track in this place is in the library once.',
    scanning: 'The library is being read: look again when it ends.',
    groups: 'groups', extra: 'extra files', free: 'can be freed',
    chips: { all: 'All', twin: 'Folder twins', exact: 'Exact copies', audio: 'Same audio, other tags', versions: 'Other versions' },
    kinds: { twin: 'Folder twin', exact: 'Exact copy', audio: 'Same audio, other tags', versions: 'Other versions' },
    twinMeta: (artist, same, all, size) => [artist, `${same} of ${all} tracks the same`, size].filter(Boolean).join(' · '),
    audioMeta: 'audio MD5 the same, the tags differ',
    versionsMeta: 'not duplicates, all stay in search · for your eyes only',
    tracksBtn: n => `${n} track${n === 1 ? '' : 's'} ▾`,
    wholeFolder: '(whole folder)',
    keepFlag: 'keep', plays: 'plays now', inPl: name => `in ${name}`,
    convs: n => `${n} conversion${n === 1 ? '' : 's'}`,
    keepTip: 'Keep this copy: the others of the group can go',
    boxTip: 'Move this copy to the Recycle Bin',
    keepBoxTip: 'The copy that stays',
    added: d => `added ${d}`,
    sel: (n, size) => `${n} file${n === 1 ? '' : 's'} · ${size}`,
    selTail: ' selected · one copy of every track stays',
    none0: 'Select none',
    trash: '🗑 Move to Recycle Bin…',
    ask: (n, size) => `Move ${n} file${n === 1 ? '' : 's'} (${size}) to the Recycle Bin?`,
    askSub: 'One copy of every track stays in the library. You can bring files back from the Recycle Bin.',
    askWhole: (f, n) => `Whole folder: ${f} (${n} file${n === 1 ? '' : 's'})`,
    askFiles: (n, d) => `${n} cop${n === 1 ? 'y' : 'ies'} in ${d} folder${d === 1 ? '' : 's'}`,
    askFolders: 'Also remove folders left with no music (cover.jpg, .cue, .log only)',
    yes: 'Move to Recycle Bin',
    moved: (n, size) => `Moved ${n} file${n === 1 ? '' : 's'} to the Recycle Bin (${size})`,
    dirs: n => ` · ${n} empty folder${n === 1 ? '' : 's'}`,
    skipped: (n, why) => ` · ${n} skipped (${why})`,
    more: n => `Show ${n} more`,
    reveal: 'Show in folder',
};

const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
}
const button = (cls, text, tip) => {
    const b = el('button', cls, text);
    b.type = 'button';
    if (tip) b.dataset.tip = tip;
    return b;
};
const store = {
    get: k => { try { return localStorage.getItem(k); } catch (_) { return null; } },
    set: (k, v) => { try { localStorage.setItem(k, v); } catch (_) {} },
};

/// The page's node (tools.js).
export function dupesPage(ctx) {
    const node = el('div', 'ml-dupp');
    const head = el('div', 'ml-dp-head');
    const sum = el('div', 'ml-dp-sum');
    const list = el('div', 'ml-dp-list');
    const foot = el('div', 'ml-dp-foot');
    node.append(head, sum, list, foot);

    let custom = null;
    try { custom = sessionStorage.getItem(SCOPE_KEY); sessionStorage.removeItem(SCOPE_KEY); } catch (_) {}
    const S = {
        data: null, rows: [], roots: [], scope: custom || '', custom: custom || null,
        rule: store.get(RULE_KEY) || '', chip: 'all',
        keeps: new Map(), ticks: new Map(), open: new Set(), shown: CHUNK,
        sizes: new Map(), inPl: new Map(), playing: null, busy: false, error: null,
    };

    // ── the head: where, which stays, again ─────────────────────────────
    const scopeSel = el('select', 'ml-dp-sel');
    const ruleSel = el('select', 'ml-dp-sel');
    const again = button('ml-btn ml-dp-again', TEXTS.again);
    const lab = (t, sel) => { const l = el('label', 'ml-dp-scope'); l.append(el('span', 'ml-dp-lbl', t), sel); return l; };
    head.append(el('span', 'ml-dp-title', TEXTS.title), lab(TEXTS.lookIn, scopeSel), lab(TEXTS.keep, ruleSel), el('span', 'ml-grow'), again);
    scopeSel.addEventListener('change', () => { S.scope = scopeSel.value; load(); });
    ruleSel.addEventListener('change', () => { S.rule = ruleSel.value; store.set(RULE_KEY, S.rule); S.keeps.clear(); retick(); paint(); });
    again.addEventListener('click', () => load());

    function paintHead() {
        const opts = [['', TEXTS.whole], ...S.roots.map(r => [r, r])];
        if (S.custom && !S.roots.includes(S.custom)) opts.push([S.custom, S.custom]);
        scopeSel.replaceChildren(...opts.map(([v, t]) => { const o = el('option', null, t); o.value = v; return o; }));
        scopeSel.value = S.scope;
        const rules = [...S.roots.map(r => 'folder:' + r), ...Object.keys(RULES)];
        if (!rules.includes(S.rule)) S.rule = rules[0] || 'quality';
        ruleSel.replaceChildren(...rules.map(r => { const o = el('option', null, ruleText(r)); o.value = r; return o; }));
        ruleSel.value = S.rule;
    }

    // ── what is known around: playlists, what plays ─────────────────────
    function readPlayer() {
        S.inPl = new Map();
        try {
            for (const p of getPlaylists() || []) for (const e of p.entries || []) if (e.path && !S.inPl.has(e.path)) S.inPl.set(e.path, p.name);
        } catch (_) {}
        try { S.playing = playingPath(ctx.status?.()); } catch (_) { S.playing = null; }
    }
    const inPlaylist = () => new Set(S.inPl.keys());

    // ── keeps and ticks ─────────────────────────────────────────────────
    const keepOf = r => (r.kind === 'versions' ? -1 : S.keeps.get(r.id) ?? chooseKeep(r.items, S.rule, { inPlaylist: inPlaylist() }));
    function retick() {
        S.ticks = new Map();
        for (const r of S.rows) S.ticks.set(r.id, ticksFor(r, keepOf(r)));
    }
    const chosen = () => selection(S.rows.flatMap(r => pathsFor(r, S.ticks.get(r.id) || new Set())), S.sizes);

    // ── loading ─────────────────────────────────────────────────────────
    async function load() {
        S.error = null;
        list.replaceChildren(el('div', 'ml-dp-note', TEXTS.loading));
        try {
            const d = await lib('lib_dupes', S.scope ? { scope: S.scope } : {});
            S.data = d;
            S.roots = (d.roots || []).map(String);
        } catch (e) {
            S.data = { groups: [], twins: [], versions: [] };
            S.error = String(e?.message || e);
            if (S.scope) { S.scope = ''; S.custom = null; }
        }
        S.rows = rowsOf(S.data);
        S.sizes = sizesOf(S.data);
        S.keeps.clear();
        S.shown = CHUNK;
        readPlayer();
        paintHead();
        retick();
        paint();
    }

    // ── painting ────────────────────────────────────────────────────────
    function paint() {
        paintSum();
        paintList();
        paintFoot();
    }

    function paintSum() {
        const t = totals(S.rows, keepOf);
        const c = kindCounts(S.rows);
        sum.replaceChildren();
        const kpi = (n, w, cls) => { const k = el('div', 'ml-dp-kpi' + (cls ? ' ' + cls : '')); k.append(el('b', null, n), el('span', null, w)); sum.append(k); };
        kpi(String(t.groups), TEXTS.groups);
        kpi(String(t.files), TEXTS.extra);
        kpi(sizeText(t.bytes), TEXTS.free, 'free');
        const chips = el('div', 'ml-dp-chips');
        for (const k of ['all', 'twin', 'exact', 'audio', 'versions']) {
            if (k !== 'all' && !c[k]) continue;
            const b = button('ml-chip' + (S.chip === k ? ' on' : ''), `${TEXTS.chips[k]} ${c[k]}`);
            b.addEventListener('click', () => { S.chip = k; S.shown = CHUNK; paint(); });
            chips.append(b);
        }
        sum.append(chips);
    }

    function paintList() {
        list.replaceChildren();
        if (S.error) list.append(el('div', 'ml-dp-note err', S.error));
        const rows = S.rows.filter(r => S.chip === 'all' || r.kind === S.chip);
        if (!rows.length) {
            const n = el('div', 'ml-dp-note');
            n.append(el('b', null, TEXTS.none), el('span', null, TEXTS.noneSub));
            list.append(n);
            return;
        }
        for (const r of rows.slice(0, S.shown)) list.append(rowNode(r));
        if (rows.length > S.shown) {
            const m = button('ml-btn ml-dp-more', TEXTS.more(Math.min(CHUNK, rows.length - S.shown)));
            m.addEventListener('click', () => { S.shown += CHUNK; paintList(); });
            list.append(m);
        }
    }

    function rowNode(r) {
        const g = el('div', 'ml-dp-grp k-' + r.kind);
        const gh = el('div', 'ml-dp-gh');
        gh.append(el('span', 'ml-dp-kind k-' + (r.kind === 'twin' ? 'twin' : r.kind), TEXTS.kinds[r.kind]), el('span', 'ml-dp-gt', r.title || '—'));
        let meta;
        if (r.kind === 'twin') {
            const keep = r.items[keepOf(r)];
            meta = TEXTS.twinMeta(r.artist, r.groups.length, r.tracks || keep?.tracks || r.groups.length, sizeText(keep?.size || 0));
        } else {
            const t = r.durationS ? `${Math.floor(r.durationS / 60)}:${String(Math.round(r.durationS % 60)).padStart(2, '0')}` : '';
            meta = [r.artist, r.album, t, r.kind === 'audio' ? TEXTS.audioMeta : r.kind === 'versions' ? TEXTS.versionsMeta : ''].filter(Boolean).join(' · ');
        }
        gh.append(el('span', 'ml-dp-ga', meta), el('span', 'ml-grow'));
        if (r.kind === 'twin') {
            const tb = button('ml-btn ml-dp-tb', TEXTS.tracksBtn(r.groups.length));
            tb.addEventListener('click', () => { if (S.open.has(r.id)) S.open.delete(r.id); else S.open.add(r.id); g.replaceWith(rowNode(r)); });
            gh.append(tb);
        }
        g.append(gh);
        const keep = keepOf(r);
        const ticks = S.ticks.get(r.id) || new Set();
        const parts = differPart(r.items.map(it => it.folder ?? it.path));
        r.items.forEach((it, i) => g.append(copyNode(r, it, i, keep, ticks, parts[i])));
        if (r.kind === 'twin' && S.open.has(r.id)) {
            const tl = el('div', 'ml-dp-tl');
            for (const x of r.groups) tl.append(el('div', null, [x.title, x.artist].filter(Boolean).join(' — ')));
            g.append(tl);
        }
        return g;
    }

    function copyNode(r, it, i, keep, ticks, part) {
        const info = r.kind === 'versions';
        const isKeep = i === keep;
        const on = ticks.has(i);
        const cp = el('div', 'ml-dp-cp' + (isKeep ? ' keep' : '') + (on ? ' del' : ''));
        const rad = button('ml-dp-rad' + (isKeep ? ' on' : ''), null, info ? null : TEXTS.keepTip);
        rad.disabled = info;
        rad.setAttribute('aria-pressed', String(isKeep));
        rad.addEventListener('click', () => {
            if (isKeep) return;
            S.keeps.set(r.id, i);
            const t = new Set(S.ticks.get(r.id) || []);
            t.delete(i);
            if (r.level === 'exact') r.items.forEach((_, j) => { if (j !== i) t.add(j); });
            S.ticks.set(r.id, t);
            paint();
        });
        const box = button('ml-dp-box' + (on ? ' on' : ''), on ? '✓' : '', info ? null : isKeep ? TEXTS.keepBoxTip : TEXTS.boxTip);
        box.disabled = info || isKeep;
        box.setAttribute('aria-pressed', String(on));
        box.addEventListener('click', () => {
            const t = new Set(S.ticks.get(r.id) || []);
            if (t.has(i)) t.delete(i); else t.add(i);
            S.ticks.set(r.id, t);
            paintSum();
            paintFoot();
            cp.classList.toggle('del', t.has(i));
            box.classList.toggle('on', t.has(i));
            box.textContent = t.has(i) ? '✓' : '';
            box.setAttribute('aria-pressed', String(t.has(i)));
        });
        const path = el('span', 'ml-dp-path');
        path.innerHTML = `${esc(part[0])}<em>${esc(part[1])}</em>${esc(part[2])}`;
        path.title = it.folder ?? it.path;
        if (r.kind === 'twin') path.append(el('span', 'ml-dp-dim', ' ' + TEXTS.wholeFolder));
        const flag = (t, cls) => path.append(el('span', 'ml-dp-flag ' + cls, t));
        if (isKeep && !info) flag(TEXTS.keepFlag, 'k');
        const paths = it.paths || [it.path];
        if (S.playing && paths.includes(S.playing)) flag(TEXTS.plays, 'p');
        const pl = paths.map(p => S.inPl.get(p)).find(Boolean);
        if (pl) flag(TEXTS.inPl(pl), 'pl');
        if (it.conversions > 0) flag(TEXTS.convs(it.conversions), 'c');
        cp.append(rad, box, path);
        for (const w of fmtWords(it)) cp.append(el('span', 'ml-dp-m', w));
        cp.append(el('span', 'ml-dp-m', sizeText(it.size || 0)), el('span', 'ml-dp-m ml-dp-date', TEXTS.added(dateText(it.added))));
        if (CAN_REVEAL) {
            const rv = button('ml-dp-rv', '📁', TEXTS.reveal);
            rv.addEventListener('click', () => {
                const p = it.path || (it.paths || [])[0];
                if (p) window.__TAURI__?.core?.invoke('conv_reveal', { path: p }).catch(e => ctx.toast(String(e?.message || e)));
            });
            cp.append(rv);
        }
        return cp;
    }

    function paintFoot() {
        const s = chosen();
        foot.replaceChildren();
        const t = el('span', 'ml-dp-s');
        t.append(el('b', null, TEXTS.sel(s.files, sizeText(s.bytes))), document.createTextNode(TEXTS.selTail));
        const none = button('ml-btn', TEXTS.none0);
        none.disabled = !s.files;
        none.addEventListener('click', () => { for (const r of S.rows) S.ticks.set(r.id, new Set()); paint(); });
        const go = button('ml-btn ml-dp-danger', TEXTS.trash);
        go.disabled = !s.files || S.busy;
        go.addEventListener('click', () => trash(go));
        foot.append(t, el('span', 'ml-grow'), none, go);
    }

    // ── to the Recycle Bin ──────────────────────────────────────────────
    async function trash(anchor) {
        const s = chosen();
        if (!s.files) return;
        const c = confirmLines(S.rows, S.ticks);
        const extra = el('div', 'ml-dp-ask');
        extra.append(el('div', 'ml-dp-asksub', TEXTS.askSub));
        const ul = el('ul');
        for (const w of c.whole) ul.append(el('li', null, TEXTS.askWhole(w.folder, w.files)));
        if (c.files) ul.append(el('li', null, TEXTS.askFiles(c.files, c.dirs)));
        extra.append(ul);
        const lbl = el('label', 'ml-dp-askchk');
        const chk = el('input');
        chk.type = 'checkbox';
        lbl.append(chk, el('span', null, TEXTS.askFolders));
        extra.append(lbl);
        const ok = await confirmAt(anchor, { text: TEXTS.ask(s.files, sizeText(s.bytes)), yes: TEXTS.yes, extra, big: ctx.layout !== 'wide' });
        if (!ok) return;
        S.busy = true;
        paintFoot();
        try {
            const r = await lib('lib_trash', { paths: s.paths, folders: chk.checked });
            const skipped = r.skipped || [];
            const whys = [...new Set(skipped.map(x => x.why))].slice(0, 3).join(', ');
            ctx.toast(TEXTS.moved(r.moved || 0, sizeText(r.bytes || 0)) + (r.folders ? TEXTS.dirs(r.folders) : '') + (skipped.length ? TEXTS.skipped(skipped.length, whys) : ''));
        } catch (e) {
            ctx.toast(String(e?.message || e));
        }
        S.busy = false;
        statusNow();
        emit({ type: 'dupes' });
        load();
    }

    node.__after = () => load();
    node.__refresh = () => { readPlayer(); paintList(); };
    return node;
}

