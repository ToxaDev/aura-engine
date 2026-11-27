// ══════════════════════════════════════════════════════════════════════
// The library's Visualizer page: the scenes, the picture, its sliders, the
// studio and the full screen in one place.
//
// On top the scene chosen, large, drawn from the music that plays (the
// player's sound, the same numbers as its own picture: music.js); over it
// Full screen, Use in the player, Edit in Studio. Under it the scene's
// name, what it is, its sliders (kept at once, as the studio keeps them on
// Save — the player's picture follows: vis:params). Then every scene, a
// still picture of each (vis-thumbs.js), the one the player shows marked.
//
// One canvas draws live. The page asks the render to keep the sound ahead
// while it draws (waves.js does the same for the player: source 'library'),
// and leaving it ends everything: the frames, the timers, the listeners,
// the look-ahead, the video-card contexts.
// ══════════════════════════════════════════════════════════════════════

import { createMusic } from '../vis/music.js';
import { createRenderer } from '../vis/renderer.js';
import { loadCatalog, valuesFor, keepValues, forgetValues, emit, listen } from '../vis/catalog.js';
import { valuesOf, parseHeader, withDefaults } from '../vis/scene.js';
import { restingLoop } from '../frame-loop.js';
import { wavesMenu } from '../waves.js';
import { spectrumDelay } from '../player.js';
import { confirmAt } from '../confirm-pop.js';
import { createThumbs, thumbKey, thumbCached } from './vis-thumbs.js';
import {
    PAGE_SCENE, playerScene, firstScene, tilesOf, shownValue, valueFrom, sliderRows, anyChanged,
    packText, aheadWant, freshName, named,
} from './vis-model.js';

const T = () => window.__TAURI__;
const invoke = (c, a) => T().core.invoke(c, a);
const LOOK_KEY = 'auraWavesLook', EFFECT_KEY = 'auraWavesEffect';   // waves.js
const STUDIO_SCENE = 'auraVisStudioScene';                          // studio.js
const IDLE_MS = 2500;

const TEXTS = {
    full: 'Full screen',
    back: 'Back',
    fullTip: 'The picture on the whole screen. Esc or a double click: back.',
    use: 'Use in the player',
    useTip: "Show this scene as the player's picture (listening mode)",
    inPlayer: 'In the player',
    edit: 'Edit in Studio',
    editTip: 'Open this scene in the visualization studio, here in the page: its code, its settings, the AI prompt',
    editWin: 'Edit in a window',
    backScenes: '‹ Scenes',
    studioSlow: 'The studio is slow to open.',
    quiet: 'Nothing is playing',
    quietSub: 'the picture comes alive with the music',
    noGl: 'The picture needs WebGL 2, which this computer does not offer here.',
    lost: 'The video card dropped the picture. It comes back by itself; if it does not, choose the scene again.',
    heavy: why => `This scene is ${why}.`,
    fails: msg => `This scene does not compile here${msg ? ': ' + msg : ''}. Open it in the studio to see why.`,
    none: 'No scenes yet. Restore the app\'s own or make a new one.',
    settings: 'Settings',
    noSettings: 'This scene has no settings.',
    reset: 'Reset',
    resetTip: "Every setting back to the scene's own",
    exp: 'Export',
    expTip: 'Write this scene, with its settings, to a file to share',
    del: 'Delete',
    delTip: 'Delete this scene (its file is removed; the app\'s own come back with Restore built-in)',
    delAsk: n => `Delete "${n}"? Its file is removed.`,
    scenes: 'Scenes',
    nw: 'New',
    nwTip: 'A new scene from the example, opened in the studio',
    imp: 'Import',
    impTip: 'Add a scene from a file (.aura-vis, .glsl, .frag, .txt)',
    folder: 'Open folder',
    folderTip: 'The scenes\' folder in the file manager',
    restore: 'Restore built-in',
    restoreTip: "Put back the app's own scenes that were deleted",
    restored: n => (n ? `${n} of the app's scenes back in the list.` : "The app's scenes are all in the list."),
    used: n => `"${n}" is the player's picture now.`,
    deleted: n => `"${n}" deleted.`,
    added: 'Added to your scenes.',
    written: p => 'Written to ' + p,
    made: n => `"${n}" made — it opens in the studio.`,
    getPack: 'Get it in the studio',
    instruments: 'INSTRUMENTS',
    mShow: 'Show here',
    by: a => 'by ' + a,
};

const ICON = {
    full: '<svg class="ln" viewBox="0 0 24 24"><path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/></svg>',
    use: '<svg class="ln" viewBox="0 0 24 24"><rect x="3" y="5" width="18" height="12" rx="2"/><path d="M8 21h8M12 17v4"/></svg>',
    edit: '<svg class="ln" viewBox="0 0 24 24"><path d="M4 20h4L19 9l-4-4L4 16v4zM14 6l4 4"/></svg>',
};

const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const lsGet = k => { try { return localStorage.getItem(k); } catch (_) { return null; } };
const lsSet = (k, v) => { try { localStorage.setItem(k, v); } catch (_) {} };

/// The page's node (tools.js).
export function visualizerPage(ctx = {}) {
    const node = document.createElement('div');
    node.className = 'ml-vz';
    node.innerHTML = `
        <div class="ml-vz-stage">
            <canvas class="ml-vz-cv"></canvas>
            <div class="ml-vz-msg" hidden></div>
            <div class="ml-vz-quiet" hidden><b>${TEXTS.quiet}</b><span>${TEXTS.quietSub}</span></div>
            <div class="ml-vz-cap"></div>
            <div class="ml-vz-ctl">
                <button type="button" class="ml-vz-cb" data-vz-act="use" data-tip="${esc(TEXTS.useTip)}">${ICON.use}<span>${TEXTS.use}</span></button>
                <button type="button" class="ml-vz-cb" data-vz-act="edit" data-tip="${esc(TEXTS.editTip)}">${ICON.edit}<span>${TEXTS.edit}</span></button>
                <button type="button" class="ml-vz-cb" data-vz-act="full" data-tip="${esc(TEXTS.fullTip)}">${ICON.full}<span>${TEXTS.full}</span></button>
            </div>
        </div>
        <div class="ml-vz-info">
            <div class="ml-vz-about">
                <div class="ml-vz-name"><span class="ml-vz-nm"></span><span class="ml-vz-badge" hidden>${TEXTS.instruments}</span><span class="ml-vz-inpl" hidden></span></div>
                <div class="ml-vz-by"></div>
                <div class="ml-vz-txt"></div>
                <div class="ml-vz-pack" hidden></div>
                <div class="ml-vz-acts">
                    <button type="button" class="ml-btn" data-vz-act="export" data-tip="${esc(TEXTS.expTip)}">${TEXTS.exp}</button>
                    <button type="button" class="ml-btn" data-vz-act="delete" data-tip="${esc(TEXTS.delTip)}">${TEXTS.del}</button>
                </div>
            </div>
            <div class="ml-vz-set">
                <div class="ml-vz-sh"><span>${TEXTS.settings}</span><button type="button" class="ml-lk" data-vz-act="reset" data-tip="${esc(TEXTS.resetTip)}">${TEXTS.reset}</button></div>
                <div class="ml-vz-params"></div>
            </div>
        </div>
        <div class="ml-vz-gh">
            <span class="ml-vz-gt">${TEXTS.scenes}</span><span class="ml-vz-gn"></span>
            <span class="ml-vz-gacts">
                <button type="button" class="ml-btn pri" data-vz-act="new" data-tip="${esc(TEXTS.nwTip)}">${TEXTS.nw}</button>
                <button type="button" class="ml-btn" data-vz-act="import" data-tip="${esc(TEXTS.impTip)}">${TEXTS.imp}</button>
                <button type="button" class="ml-btn" data-vz-act="folder" data-tip="${esc(TEXTS.folderTip)}">${TEXTS.folder}</button>
                <button type="button" class="ml-btn" data-vz-act="restore" data-tip="${esc(TEXTS.restoreTip)}">${TEXTS.restore}</button>
            </span>
        </div>
        <div class="ml-vz-grid"></div>`;
    const q = s => node.querySelector(s);
    const stage = q('.ml-vz-stage'), canvas = q('.ml-vz-cv'), msg = q('.ml-vz-msg'), quiet = q('.ml-vz-quiet'), cap = q('.ml-vz-cap');
    const grid = q('.ml-vz-grid'), params = q('.ml-vz-params'), pack = q('.ml-vz-pack');
    const toast = t => ctx.toast?.(t);
    const status = () => ctx.status?.() || {};

    let catalog = [];
    let cur = null;           // the catalog's entry shown
    let values = {};          // its values as the sliders have them
    const failed = new Set(); // scenes that do not draw here
    let gone = false, started = false;
    let renderer = null, music = null, frames = null, io = null;
    let seen = true, wanted = null, lostNote = '';
    let thumbs = null;
    let packSt = null;
    const offs = [];          // the listeners' promises of their unlisten
    let keepTimer = 0, idleTimer = 0;

    // ── the scene shown ──────────────────────────────────────────────
    const showMsg = text => {
        msg.textContent = text || '';
        msg.hidden = !text;
        stage.classList.toggle('ml-vz-bad', !!text);
    };
    /// Draw `e` (a live text from the studio when `text` is given); the
    /// scene before goes on until this one is ready.
    async function show(e, text, vals) {
        // The card's context away: drawn again once it is back (step).
        if (!renderer || renderer.dead || !e) return;
        const id = e.id;
        wanted = id;
        const r = await renderer.setScene(text ?? e.text, id, vals ?? values);
        if (gone || wanted !== id || r.superseded) return;
        wanted = null;
        if (r.ok) { failed.delete(id); showMsg(lostNote); }
        else {
            console.warn('[vis] scene does not compile:', id, r.errors);
            failed.add(id);
            renderer.clearScene();
            showMsg(TEXTS.fails(r.errors?.[0]?.msg));
        }
        markTiles();
        frames?.wake();
    }

    function select(id, { draw = true } = {}) {
        const e = catalog.find(c => c.id === id) || null;
        cur = e;
        values = e ? valuesFor(e) : {};
        if (e) lsSet(PAGE_SCENE, e.id);
        failed.delete(id);
        lostNote = '';
        showMsg('');
        renderInfo();
        markTiles();
        if (draw && e && renderer) show(e);
        if (!e) renderer?.clearScene();
        frames?.wake();
    }

    // ── the player's look (waves.js) ─────────────────────────────────
    const playerLook = () => wavesMenu()?.current() || lsGet(LOOK_KEY) || 'album';
    const inPlayer = () => playerScene(playerLook(), lsGet(EFFECT_KEY));

    function useInPlayer() {
        if (!cur) return;
        const id = cur.id;
        const api = wavesMenu();
        if (api) api.set('vis:' + id);
        else lsSet(LOOK_KEY, 'vis:' + id);
        // The studio's list and a player of another window follow.
        emit('vis:look', { id });
        toast(TEXTS.used(cur.header.name));
        markPlayer();
        markTiles();
    }

    /// The studio in a window of its own, as before the page had it inside.
    function openStudioWindow(id) {
        if (id) lsSet(STUDIO_SCENE, id);
        invoke('vis_studio_open').catch(e => toast(String(e)));
        emit('vis:studio-select', { id: id || null });
    }

    // ── the studio inside the page ──
    // The studio's own page in a frame that takes the page's place: the
    // code, the errors, the settings and the AI prompt, on the scene chosen.
    // Its head's button (or the bar here, should the frame not come up)
    // brings the scenes back; the studio asks about changes not kept first.
    let studio = null;   // { wrap, frame, onMsg, ready, slow }
    function openStudio(id) {
        if (id) lsSet(STUDIO_SCENE, id);
        if (studio) { emit('vis:studio-select', { id: id || null }); return; }
        if (full) leaveFull(true);
        const wrap = document.createElement('div');
        wrap.className = 'ml-vz-studio';
        wrap.innerHTML = `<div class="ml-vz-sbar" hidden><button type="button" class="ml-btn">${esc(TEXTS.backScenes)}</button><span>${esc(TEXTS.studioSlow)}</span></div>`;
        const bar = wrap.firstElementChild;
        const frame = document.createElement('iframe');
        frame.className = 'ml-vz-frame';
        frame.src = 'vis-studio.html?embedded';
        wrap.append(frame);
        const onMsg = e => {
            if (!studio || e.source !== frame.contentWindow) return;
            const m = e.data?.auraStudio;
            if (m === 'ready') { studio.ready = true; clearTimeout(studio.slow); bar.hidden = true; }
            else if (m === 'close') closeStudio();
        };
        window.addEventListener('message', onMsg);
        bar.querySelector('button').addEventListener('click', closeStudio);
        studio = { wrap, frame, onMsg, ready: false, slow: setTimeout(() => { if (studio && !studio.ready) bar.hidden = false; }, 5000) };
        node.classList.add('editing');
        node.append(wrap);
    }
    function closeStudio() {
        if (!studio) return;
        const s = studio;
        studio = null;
        clearTimeout(s.slow);
        window.removeEventListener('message', s.onMsg);
        try { s.frame.contentWindow?.__auraOff?.(); } catch (_) {}
        s.wrap.remove();
        node.classList.remove('editing');
        invoke('player_set_vis_ahead', { source: 'studio', seconds: 0 }).catch(() => {});
        if (!gone) reload();
    }

    // ── the scene's words and sliders ────────────────────────────────
    function renderInfo() {
        const h = cur?.header;
        q('.ml-vz-nm').textContent = h ? h.name : '';
        cap.textContent = h ? h.name : '';
        q('.ml-vz-badge').hidden = !h?.spatial;
        q('.ml-vz-by').textContent = h?.author ? TEXTS.by(h.author) : '';
        q('.ml-vz-txt').textContent = h ? h.about : (catalog.length ? '' : TEXTS.none);
        for (const a of ['use', 'edit', 'full', 'export', 'delete']) byAct(a).disabled = !cur;
        markPlayer();
        renderParams();
        renderPack();
    }
    /// The page's buttons by their action (the stage's too, on the full screen).
    const byAct = a => stage.querySelector(`[data-vz-act="${a}"]`) || q(`[data-vz-act="${a}"]`);
    /// Whether the scene shown is the player's: its mark and Use's state.
    function markPlayer() {
        const pl = inPlayer();
        const mine = !!cur && pl.id === cur.id;
        const mark = q('.ml-vz-inpl');
        mark.hidden = !mine;
        mark.textContent = pl.always ? 'IN PLAYER' : 'IN PLAYER · NO COVER';
        const useB = byAct('use');
        useB.classList.toggle('on', mine && pl.always);
        useB.querySelector('span').textContent = mine && pl.always ? TEXTS.inPlayer : TEXTS.use;
    }

    function renderParams() {
        params.textContent = '';
        const h = cur?.header;
        const sections = h ? sliderRows(h, values) : [];
        q('.ml-vz-set').hidden = !cur;
        if (!sections.length) {
            const n = document.createElement('div');
            n.className = 'ml-vz-none';
            n.textContent = cur ? TEXTS.noSettings : '';
            params.appendChild(n);
        }
        for (const s of sections) {
            const box = document.createElement('div');
            box.className = 'ml-vz-sec';
            if (s.name) {
                const t = document.createElement('div');
                t.className = 'ml-vz-st';
                t.textContent = s.name;
                if (s.why) t.dataset.tip = s.why;
                box.appendChild(t);
            }
            for (const r of s.rows) box.appendChild(paramRow(r));
            params.appendChild(box);
        }
        resetState();
    }
    const resetState = () => { byAct('reset').disabled = !cur || !anyChanged(cur.header, values); };

    /// One setting: its label, its control, its value.
    function paramRow({ p, value, text, changed }) {
        const row = document.createElement('div');
        row.className = 'ml-vz-p' + (changed ? ' changed' : '');
        row.dataset.vzParam = p.name;
        const lab = document.createElement('span');
        lab.className = 'ml-vz-pl';
        lab.textContent = p.label;
        if (p.why) lab.dataset.tip = p.why;
        const val = document.createElement('span');
        val.className = 'ml-vz-pv';
        val.textContent = text;
        let ctl;
        const set = raw => {
            if (!cur) return;
            const v = valueFrom(p, raw);
            values = { ...values, [p.name]: v };
            renderer?.setValues(values);
            emit('vis:params', { id: cur.id, values, from: 'library' });
            val.textContent = shownValue(p, v);
            row.classList.toggle('changed', !same(v, p.def));
            if (p.type === 'choice') for (const b of ctl.children) b.classList.toggle('on', Number(b.dataset.vzOpt) === v);
            keepSoon();
            resetState();
        };
        if (p.type === 'choice') {
            ctl = document.createElement('div');
            ctl.className = 'ml-vz-opts';
            p.options.forEach((o, i) => {
                const b = document.createElement('button');
                b.type = 'button';
                b.className = 'ml-chip' + (i === Math.round(value) ? ' on' : '');
                b.dataset.vzOpt = String(i);
                b.textContent = o;
                b.addEventListener('click', () => set(i));
                ctl.appendChild(b);
            });
        } else if (p.type === 'bool') {
            ctl = document.createElement('input');
            ctl.type = 'checkbox';
            ctl.className = 'ml-vz-sw';
            ctl.checked = !!value;
            ctl.addEventListener('change', () => set(ctl.checked));
        } else if (p.type === 'color') {
            ctl = document.createElement('input');
            ctl.type = 'color';
            ctl.className = 'ml-vz-col';
            ctl.value = shownValue(p, value);
            ctl.addEventListener('input', () => set(ctl.value));
        } else {
            ctl = document.createElement('input');
            ctl.type = 'range';
            ctl.className = 'ml-vz-rg';
            ctl.min = String(p.min);
            ctl.max = String(p.max);
            ctl.step = String(p.step);
            ctl.value = String(value);
            ctl.addEventListener('input', () => set(ctl.value));
            // A double click on the slider: its own value back.
            ctl.addEventListener('dblclick', () => { ctl.value = String(p.def); set(p.def); });
        }
        ctl.setAttribute('aria-label', p.label);
        row.append(lab, ctl, val);
        return row;
    }

    /// The sliders kept a moment after the last move (catalog.js: the
    /// player's picture and the studio read them there).
    function keepSoon() {
        clearTimeout(keepTimer);
        const id = cur?.id, v = values;
        keepTimer = setTimeout(() => { keepTimer = 0; if (id) keepValues(id, v); thumbFor(cur); }, 300);
    }
    function keepNow() {
        if (!keepTimer) return;
        clearTimeout(keepTimer);
        keepTimer = 0;
        if (cur) keepValues(cur.id, values);
    }

    function resetValues() {
        if (!cur) return;
        clearTimeout(keepTimer);
        keepTimer = 0;
        values = valuesOf(cur.header, {});
        forgetValues(cur.id);
        renderer?.setValues(values);
        emit('vis:params', { id: cur.id, values, from: 'library' });
        renderParams();
        thumbFor(cur);
    }

    // ── the instruments pack (a scene with instruments) ──────────────
    function renderPack() {
        const t = packText(packSt, !!cur?.header?.spatial);
        pack.hidden = !t;
        pack.textContent = '';
        if (!t) return;
        pack.append(t.text + ' ');
        if (t.get) {
            const a = document.createElement('button');
            a.type = 'button';
            a.className = 'ml-lk';
            a.textContent = TEXTS.getPack;
            a.addEventListener('click', () => openStudio(cur?.id));
            pack.appendChild(a);
        }
    }
    const lookPack = async () => {
        try { packSt = await invoke('spatial_status'); } catch (_) { packSt = null; }
        if (!gone) renderPack();
    };

    // ── the gallery ──────────────────────────────────────────────────
    function renderGrid() {
        q('.ml-vz-gn').textContent = catalog.length ? String(catalog.length) : '';
        grid.textContent = '';
        if (!catalog.length) {
            const n = document.createElement('div');
            n.className = 'ml-vz-none';
            n.textContent = TEXTS.none;
            grid.appendChild(n);
            return;
        }
        for (const t of tilesOf(catalog, { current: cur?.id, player: inPlayer(), failed })) {
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'ml-vz-tile';
            b.dataset.vzScene = t.id;
            if (t.about) b.dataset.tip = t.about;
            b.innerHTML = `<span class="ml-vz-th ml-ph" style="--ph:${t.hue}"><span class="ml-vz-ini">${esc(t.name.slice(0, 1).toUpperCase())}</span><img alt="" hidden></span>`
                + `<span class="ml-vz-tn">${esc(t.name)}</span><span class="ml-vz-tm"></span>`
                + (t.spatial ? `<span class="ml-vz-ts">${TEXTS.instruments}</span>` : '');
            grid.appendChild(b);
        }
        markTiles();
        if (started) for (const e of catalog) thumbFor(e);
    }
    /// The marks on the tiles: the one shown, the player's, the ones that do not draw.
    function markTiles() {
        const tiles = tilesOf(catalog, { current: cur?.id, player: inPlayer(), failed });
        const byId = new Map(tiles.map(t => [t.id, t]));
        for (const b of grid.querySelectorAll('.ml-vz-tile')) {
            const t = byId.get(b.dataset.vzScene);
            if (!t) continue;
            b.classList.toggle('on', t.current);
            b.classList.toggle('bad', t.failed);
            const m = b.querySelector('.ml-vz-tm');
            m.textContent = t.mark;
            m.hidden = !t.mark;
        }
    }
    /// A tile's picture: from the memory, else drawn (once).
    function thumbFor(e) {
        if (!e || !thumbs) return;
        const v = e === cur ? values : valuesFor(e);
        const key = thumbKey(e, v);
        const put = url => {
            if (gone || !url) return;
            const img = [...grid.querySelectorAll('.ml-vz-tile')].find(b => b.dataset.vzScene === e.id)?.querySelector('img');
            if (!img) return;
            img.src = url;
            img.hidden = false;
        };
        const have = thumbCached(key);
        if (have !== undefined) { put(have); return; }
        thumbs.want(key, e, v).then(put);
    }

    // ── the list ─────────────────────────────────────────────────────
    async function reload(pick) {
        const list = await loadCatalog();
        if (gone) return;
        // What is shown is read now, not before the wait: a reload that
        // picked a scene meanwhile (a new one, one imported) keeps it.
        catalog = list;
        const before = cur;
        const id = pick || before?.id;
        const e = catalog.find(c => c.id === id);
        if (e) {
            const changed = !before || before.id !== e.id || before.text !== e.text;
            cur = e;
            if (changed) select(e.id);
            else renderInfo();
        } else {
            select(firstScene(catalog, { kept: lsGet(PAGE_SCENE), player: inPlayer().id }));
        }
        renderGrid();
    }

    // ── the actions ──────────────────────────────────────────────────
    async function del(anchor, e = cur) {
        if (!e) return;
        if (!(await confirmAt(anchor, { text: TEXTS.delAsk(e.header.name), yes: TEXTS.del }))) return;
        try {
            await invoke('vis_delete', { file: e.file });
            forgetValues(e.id);
            if (playerLook() === 'vis:' + e.id) emit('vis:look', { id: null });
            const i = catalog.findIndex(c => c.id === e.id);
            const next = catalog[i + 1] || catalog[i - 1] || null;
            emit('vis:changed', {});
            await reload(e === cur ? next?.id : cur?.id);
            toast(TEXTS.deleted(e.header.name));
        } catch (err) { toast(String(err)); }
    }
    async function exportScene() {
        if (!cur) return;
        try {
            const path = await T().dialog.save({ defaultPath: cur.file || cur.header.name.replace(/[^\w-]+/g, '-').toLowerCase() + '.aura-vis',
                filters: [{ name: 'Aura visualization', extensions: ['aura-vis'] }] });
            if (!path) return;
            keepNow();
            await invoke('vis_export', { path, text: withDefaults(cur.text, cur.header, values) });
            toast(TEXTS.written(path));
        } catch (e) { toast(String(e)); }
    }
    async function importScene() {
        try {
            const path = await T().dialog.open({ multiple: false, filters: [{ name: 'Aura visualization', extensions: ['aura-vis', 'glsl', 'frag', 'txt'] }] });
            if (!path || Array.isArray(path)) return;
            const text = await invoke('vis_import', { path });
            const h = parseHeader(text);
            const name = /@name/.test(text) ? h.name : String(path).split(/[\\/]/).pop().replace(/\.[^.]*$/, '');
            const file = await invoke('vis_save', { file: null, name, text: named(text, name) });
            emit('vis:changed', {});
            await reload('my:' + file);
            toast(TEXTS.added);
        } catch (e) { toast(String(e)); }
    }
    /// A new scene: the book's complete example (as the studio's New), under
    /// a name not taken, saved and opened in the studio.
    async function newScene() {
        try {
            let text = '';
            try {
                const spec = await (await fetch('./vis/AURA-VIS-SPEC.md', { cache: 'no-store' })).text();
                text = /#+[^\n]*complete example[^\n]*\n[\s\S]*?```glsl\n([\s\S]*?)```/i.exec(spec)?.[1] || '';
            } catch (_) {}
            if (!text) text = '// @name My scene\n\nvec4 scene(vec2 uv, vec2 p) {\n    return vec4(auraPalette(uv.x), uBass);\n}\n';
            const name = freshName(catalog.map(c => c.header.name));
            const file = await invoke('vis_save', { file: null, name, text: named(text, name) });
            emit('vis:changed', {});
            await reload('my:' + file);
            toast(TEXTS.made(name));
            openStudio('my:' + file);
        } catch (e) { toast(String(e)); }
    }
    async function restore() {
        try {
            const n = await invoke('vis_restore_app');
            emit('vis:changed', {});
            await reload();
            toast(TEXTS.restored(n));
        } catch (e) { toast(String(e)); }
    }

    node.addEventListener('click', ev => {
        const b = ev.target.closest('[data-vz-act]');
        if (b && node.contains(b)) {
            const a = b.dataset.vzAct;
            if (a === 'use') useInPlayer();
            else if (a === 'edit') openStudio(cur?.id);
            else if (a === 'full') full ? leaveFull() : enterFull();
            else if (a === 'reset') resetValues();
            else if (a === 'export') exportScene();
            else if (a === 'delete') del(b);
            else if (a === 'new') newScene();
            else if (a === 'import') importScene();
            else if (a === 'folder') invoke('vis_dir_open').catch(e => toast(String(e)));
            else if (a === 'restore') restore();
            return;
        }
        const t = ev.target.closest('.ml-vz-tile');
        if (t && grid.contains(t) && t.dataset.vzScene !== cur?.id) { keepNow(); select(t.dataset.vzScene); }
    });
    grid.addEventListener('contextmenu', ev => {
        const t = ev.target.closest('.ml-vz-tile');
        if (!t || !ctx.menu) return;
        ev.preventDefault();
        const e = catalog.find(c => c.id === t.dataset.vzScene);
        if (!e) return;
        const pick = fn => () => { if (cur?.id !== e.id) { keepNow(); select(e.id); } fn(); };
        ctx.menu({ x: ev.clientX, y: ev.clientY }, [
            { text: TEXTS.mShow, cls: 'pri', tap: pick(() => {}) },
            { text: TEXTS.use, tap: pick(useInPlayer) },
            { text: TEXTS.edit, tap: () => openStudio(e.id) },
            { text: TEXTS.editWin, tap: () => openStudioWindow(e.id) },
            { sep: true },
            { text: TEXTS.exp, tap: pick(exportScene) },
            { text: TEXTS.del, tap: () => del(t, e) },
        ]);
    });

    // ── the full screen ──────────────────────────────────────────────
    // The picture goes over everything (the page's own stage, moved to the
    // document's body), the window to the full screen; the buttons and the
    // name fade while the pointer rests. Esc or a double click: back.
    let full = false, home = null, fullBusy = false;
    const appWindow = () => { try { return T()?.window?.getCurrentWindow(); } catch (_) { return null; } };
    const awake = () => {
        stage.classList.add('awake');
        clearTimeout(idleTimer);
        idleTimer = setTimeout(() => stage.classList.remove('awake'), IDLE_MS);
    };
    const onFullKey = ev => {
        if (!full) return;
        if (ev.key === 'Escape') { ev.preventDefault(); ev.stopImmediatePropagation(); leaveFull(); return; }
        awake();
    };
    async function enterFull() {
        if (full || fullBusy || !cur) return;
        fullBusy = true;
        full = true;
        home = { parent: stage.parentNode, next: stage.nextSibling };
        document.body.appendChild(stage);
        stage.classList.add('full');
        stage.querySelector('[data-vz-act="full"] span').textContent = TEXTS.back;
        document.addEventListener('keydown', onFullKey, true);
        awake();
        frames?.wake();
        try { await appWindow()?.setFullscreen(true); } catch (_) {}
        fullBusy = false;
    }
    /// Back from the full screen (also when the page is left: `now`, the
    /// window's state not waited for).
    async function leaveFull(now = false) {
        if (!full || (fullBusy && !now)) return;
        fullBusy = true;
        full = false;
        document.removeEventListener('keydown', onFullKey, true);
        clearTimeout(idleTimer);
        stage.classList.remove('full', 'awake');
        stage.querySelector('[data-vz-act="full"] span').textContent = TEXTS.full;
        if (home?.parent?.isConnected && !gone) home.parent.insertBefore(stage, home.next && home.next.parentNode === home.parent ? home.next : home.parent.firstChild);
        else stage.remove();
        home = null;
        const w = appWindow()?.setFullscreen(false);
        if (!now) { try { await w; } catch (_) {} window.fitWindowAfterFull?.(); }
        else w?.catch?.(() => {});
        fullBusy = false;
        frames?.wake();
    }
    stage.addEventListener('dblclick', ev => {
        if (ev.target.closest('button')) return;
        if (full) leaveFull(); else enterFull();
    });
    // On the full screen the stage stands in the document's body, out of the
    // page's reach: its buttons answer here.
    stage.addEventListener('click', ev => {
        if (!full || node.contains(stage)) return;
        const a = ev.target.closest('[data-vz-act]')?.dataset.vzAct;
        if (a === 'full') leaveFull();
        else if (a === 'use') useInPlayer();
        else if (a === 'edit') leaveFull().then(() => openStudio(cur?.id));
    });
    stage.addEventListener('pointermove', awake);

    // ── the frames ───────────────────────────────────────────────────
    // As the player's picture (waves.js): up to 60 frames a second on the
    // scene's own clock, which runs with the music, slows to a stop on pause
    // and runs slower while nothing plays. Nothing to draw (the page out of
    // sight, the window hidden): the loop rests.
    let last = 0, clock = 0, pace = 1;
    let aheadSent = -1, aheadAt = 0;
    const setAhead = s => invoke('player_set_vis_ahead', { source: 'library', seconds: s }).catch(() => {});
    const needAhead = want => {
        const t = performance.now();
        if (Math.abs(want - aheadSent) < 0.05 || t - aheadAt < 500) return;
        aheadSent = want;
        aheadAt = t;
        setAhead(want);
    };
    let quietShown = null, lookSeen = '', lookAt = 0;
    const step = ts => {
        if (gone) return false;
        // The player's look switched there (its button sends no word): the marks follow.
        if (ts - lookAt > 1000) {
            lookAt = ts;
            const l = playerLook() + '|' + (lsGet(EFFECT_KEY) || '');
            if (l !== lookSeen) { if (lookSeen) { markPlayer(); markTiles(); } lookSeen = l; }
        }
        const st = status().state || 'stopped';
        const visible = !document.hidden && stage.isConnected && canvas.clientWidth > 0 && (seen || full);
        const drawing = !!cur && !!renderer && !renderer.dead && !failed.has(cur.id);
        needAhead(aheadWant({ drawing, visible, state: st }));
        const q2 = st !== 'playing' && st !== 'paused';
        if (q2 !== quietShown) { quietShown = q2; quiet.hidden = !q2; }
        if (!visible || !drawing) { last = 0; return false; }
        if (renderer.sceneId !== cur.id && wanted !== cur.id) show(cur);
        if (last && ts - last < 1000 / 62) return true;
        const dt = last ? Math.min(0.1, (ts - last) / 1000) : 0.016;
        last = ts;
        pace = st === 'paused' ? pace * 0.85 : pace + ((st === 'playing' ? 1 : 0.45) - pace) * 0.05;
        clock += dt * pace;
        const m = music.frame(dt, { spatial: renderer.spatial });
        renderer.frame(m, { clock, dt: dt * pace, fade: 0, full, budgetMs: full ? 8 : 6 });
        return true;
    };

    function start() {
        if (started || gone) return;
        started = true;
        music = createMusic({ status, delayMs: () => { try { return spectrumDelay() || 0; } catch (_) { return 0; } } });
        renderer = createRenderer(canvas, {
            onLost: ({ scene, soon }) => {
                if (gone) return;
                console.warn('[vis] the video card dropped the library\'s picture', scene, soon ? '(right after the scene started)' : '');
                if (soon && scene) failed.add(scene);
                lostNote = TEXTS.lost;
                showMsg(lostNote);
                markTiles();
            },
            onRestored: () => {
                if (gone) return;
                music.attach(renderer.sink);
                lostNote = '';
                if (cur && !failed.has(cur.id)) showMsg('');
                frames?.wake();
            },
            onHeavy: ({ scene, why }) => {
                if (gone) return;
                console.warn('[vis]', scene, why);
                if (scene) failed.add(scene);
                showMsg(TEXTS.heavy(why || 'too heavy for this video card'));
                markTiles();
            },
        });
        if (!renderer) {
            showMsg(TEXTS.noGl);
            stage.classList.add('ml-vz-nogl');
        } else {
            music.attach(renderer.sink);
            if (cur) show(cur);
        }
        thumbs = createThumbs();
        for (const e of catalog) thumbFor(e);
        frames = restingLoop(step);
        try {
            io = new IntersectionObserver(es => {
                for (const x of es) seen = x.isIntersecting;
                frames?.wake();
            });
            io.observe(stage);
        } catch (_) { io = null; }

        // The studio and the player's look: the sliders moved there, code
        // compiled there, back on close, the list changed, a look chosen.
        offs.push(listen('vis:params', ({ id, values: v, from }) => {
            if (from === 'library' || !cur || cur.id !== id || !v || same(v, values)) return;
            values = valuesOf(cur.header, v);
            renderer?.setValues(values);
            renderParams();
        }));
        offs.push(listen('vis:code', ({ id, text, values: v }) => {
            if (!cur || cur.id !== id || typeof text !== 'string') return;
            failed.delete(id);
            show(cur, text, v || values);
        }));
        offs.push(listen('vis:revert', ({ id }) => {
            if (!cur || cur.id !== id) return;
            values = valuesFor(cur);
            renderParams();
            show(cur);
        }));
        offs.push(listen('vis:changed', () => { if (!gone) reload(); }));
        offs.push(listen('vis:look', () => { if (!gone) { markPlayer(); markTiles(); } }));
        try {
            offs.push(T()?.event.listen('spatial:progress', m => {
                packSt = { ...(packSt || {}), ...(m.payload || {}) };
                if (!gone) renderPack();
            }).catch(() => {}));
        } catch (_) {}
    }

    node.__after = () => {
        if (gone) return;
        reload().then(() => { if (!gone) start(); });
        lookPack();
    };
    node.__refresh = () => {
        if (gone) return;
        reload();
        lookPack();
    };
    node.__leave = () => {
        if (gone) return;
        gone = true;
        closeStudio();
        keepNow();
        clearTimeout(keepTimer);
        clearTimeout(idleTimer);
        if (full) leaveFull(true);
        frames?.stop();
        frames = null;
        io?.disconnect();
        io = null;
        for (const p of offs.splice(0)) Promise.resolve(p).then(f => { try { f?.(); } catch (_) {} });
        thumbs?.stop();
        thumbs = null;
        setAhead(0);
        if (renderer) {
            try { renderer.clearScene(); } catch (_) {}
            // The page's context let go now, not when it is collected: the
            // player's picture keeps its own.
            try { canvas.getContext('webgl2')?.getExtension('WEBGL_lose_context')?.loseContext(); } catch (_) {}
            renderer = null;
        }
        music = null;
    };
    return node;
}
