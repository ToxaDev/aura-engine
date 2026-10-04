
// ══════════════════════════════════════════════════════════════════════
// The big player's picture (listening mode, where a cover would lie).
//
// Every picture is a visualization scene — GLSL, from the user's list of
// scenes (vis/: the studio writes, tunes and saves them; the app's own
// ones are put in that list on the first start and can be removed like
// any other). Anton 27.09: Flight and Terrain are gone; only scenes.
//
// The look switch goes through the scenes, then "Album": the automatic
// look — a track's cover when it has one, else the scene last chosen.
// Choosing a scene keeps it on every track (covers are not shown then).
// "Off" (right-click menu): no picture, covers shown.
//
// A scene sees the sound up to ~3.5 s ahead: while one is drawn, the render
// is asked to keep that much ahead (controller.rs set_vis_ahead) — the
// adaptive buffer alone trims itself to well under a second.
// ══════════════════════════════════════════════════════════════════════

import { createMusic } from './vis/music.js';
import { createRenderer } from './vis/renderer.js';
import { loadCatalog, valuesFor, keptValues, listen as visListen } from './vis/catalog.js';
import { valuesOf } from './vis/scene.js';
import { instrumentsNote, notePlace } from './vis/instr-note.js';

const KEY = 'auraWavesLook';
const KEY_EFFECT = 'auraWavesEffect';
const ALBUM = { id: 'album', name: 'Album', why: "The track's cover when it has one, else the scene you chose last" };
const OFF = { id: 'off', name: 'Off', why: 'No visualization; covers are shown' };
const AHEAD_S = 3.8;

let sceneLooks = [];         // { id: 'vis:<scene id>', name, why }
let catalog = [];
const looks = () => [...sceneLooks, ALBUM];
const findLook = id => id === 'off' ? OFF : looks().find(l => l.id === id) || null;
const isScene = id => /^vis:/.test(id || '');

/// A look kept from before (Flight, Terrain, the app's scenes under their
/// old ids) as it is named now.
function migrate(id) {
    if (id === 'flight' || id === 'terrain') return null;
    const m = /^vis:app:(.+)$/.exec(id || '');
    return m ? 'vis:my:' + m[1] : id;
}

// What the right-click menu needs (player.js): the looks, the one chosen,
// choosing one. Set by initWaves.
let menuApi = null;
export const wavesMenu = () => menuApi;

/// wrap: the element the canvas fills (hidden by CSS where there is a cover,
/// or out of listening mode); canvas: the scenes' canvas; button: the look
/// switch; state(): the player's state; status(): its status; delayMs(): the
/// listener's delay for the device; on(): whether the big player shows.
export function initWaves({ wrap, canvas, button, label, state, status, delayMs, on }) {
    if (!wrap || !canvas || !button) return;
    const bar = wrap.closest('#plBar');
    let look = 'album';
    let lastEffect = null;
    try {
        const s = migrate(localStorage.getItem(KEY));
        if (s === 'album' || s === 'off' || isScene(s)) look = s;
        const le = migrate(localStorage.getItem(KEY_EFFECT));
        if (isScene(le)) lastEffect = le;
    } catch (_) {}
    const failed = new Set();       // scene ids that do not work here
    // A scene that works here, Tunnel first (the one that shows the sound coming).
    const working = () => {
        const ok = sceneLooks.filter(l => !failed.has(l.id.slice(4)));
        return (ok.find(l => l.id === 'vis:my:tunnel.aura-vis') || ok[0])?.id || null;
    };
    /// The scene drawn ('vis:…'), or null: the chosen one, under Album the
    /// last chosen; one that does not work here gives way to one that does.
    const drawn = () => {
        if (look === 'off') return null;
        let d = look === 'album' ? (lastEffect || working()) : look;
        if (d && (failed.has(d.slice(4)) || (catalog.length && !findLook(d)))) d = working();
        return d;
    };

    let labelTimer = null;
    const sync = announce => {
        const L = findLook(look) || { id: look, name: 'Scene', why: 'a visualization scene' };
        bar?.classList.toggle('lm-look-effect', isScene(L.id));
        bar?.classList.toggle('lm-look-off', L.id === 'off');
        wrap.dataset.look = drawn() || 'none';
        button.dataset.look = L.id;
        button.setAttribute('aria-label', `Look: ${L.name}`);
        const all = looks();
        const i = all.findIndex(x => x.id === L.id);
        const eff = findLook(lastEffect || working());
        const under = L.id === 'album' ? (eff ? ` (${eff.name} without a cover)` : ' (no scene: covers only)') : '';
        const bad = isScene(L.id) && failed.has(L.id.slice(4)) ? ' — it does not work here: open it in the studio (right-click)' : '';
        button.dataset.tip = `Look: ${L.name}${under} — ${L.why}${bad}. Click for the next${i >= 0 ? ` (${i + 1} of ${all.length})` : ''}; right-click the player for the list, Off and the studio.`;
        if (announce && label) {
            label.textContent = i >= 0 ? `${L.name} · ${i + 1}/${all.length}` : L.name;
            label.classList.add('show');
            clearTimeout(labelTimer);
            labelTimer = setTimeout(() => label.classList.remove('show'), 1600);
        }
    };
    const setLook = (id, announce) => {
        if (!findLook(id) && !isScene(id)) return;
        look = id;
        if (isScene(id)) lastEffect = id;
        try {
            localStorage.setItem(KEY, look);
            if (lastEffect) localStorage.setItem(KEY_EFFECT, lastEffect);
        } catch (_) {}
        sync(announce);
    };
    button.addEventListener('click', ev => {
        if (ev.detail > 0) button.blur();   // Space stays play/pause
        const all = looks();
        const i = all.findIndex(x => x.id === look);
        setLook(all[(i + 1) % all.length].id, true);
    });
    sync(false);
    menuApi = {
        looks: () => looks().map(l => ({ id: l.id, name: l.name, why: l.why, spatial: !!l.spatial })),
        current: () => look,
        set: id => setLook(id, true),
        failed: id => failed.has(String(id).replace(/^vis:/, '')),
    };

    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;

    // ── the scene on the card ──
    const music = createMusic({ status: status || (() => ({ state: state() })), delayMs });
    let current = null, wanted = null;
    const renderer = createRenderer(canvas, {
        onLost: ({ scene, soon }) => {
            console.warn('[vis] the video card dropped the picture', scene, soon ? '(right after the scene started)' : '');
            if (soon && scene) { failed.add(scene); sync(false); }
        },
        onRestored: () => { music.attach(renderer.sink); current = null; wanted = null; },
        onHeavy: ({ scene, why }) => { console.warn('[vis]', scene, why); if (scene) { failed.add(scene); current = null; sync(false); } },
    });
    if (!renderer) { canvas.style.display = 'none'; return; }
    music.attach(renderer.sink);

    // The note under a scene with instruments while they are being found
    // (instr-note.js): low in the middle of the picture, over the bars, clear
    // of what lies under it — the seek bar and the buttons in the player (the
    // picture reaches down to them with the bars off), the transport on the
    // whole screen, where it goes with the picture (fullview.js moves it) —
    // and of the text over it: on one line where two do not fit (notePlace).
    const note = document.createElement('div');
    note.className = 'vis-instr-note';
    note.innerHTML = '<span class="vis-instr-line"><span class="vis-instr-spin"></span><span class="vis-instr-text"></span></span>'
        + '<span class="vis-instr-sub"></span>';
    const noteText = note.querySelector('.vis-instr-text'), noteSub = note.querySelector('.vis-instr-sub');
    let noteShown = false, notePlacedAt = 0;
    const placeNote = () => {
        const fv = wrap.closest('.fv-view');
        const host = fv ? wrap.parentNode : bar;
        if (!host) return;
        if (note.parentNode !== host) host.appendChild(note);
        const wr = wrap.getBoundingClientRect(), hr = host.getBoundingClientRect();
        if (wr.height < 1) return;
        // In the player the seek bar's top, under the picture's foot or over it
        // (the bars off): the note stands level with the row of switches.
        let floor = wr.bottom;
        const under = (fv || bar)?.querySelector(fv ? '.fv-transport' : '.pl-seek')?.getBoundingClientRect();
        if (under && under.height > 0 && (!fv || under.top < floor)) floor = under.top;
        const text = fv ? null : bar.querySelector('.pl-now')?.getBoundingClientRect();
        const rem = parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
        const oneH = Math.ceil(0.66 * rem * 1.25);     // listening.css: the lines' sizes
        const p = notePlace({ floor, ceiling: text && text.height > 0 ? text.bottom : null,
            twoH: oneH + Math.ceil(0.58 * rem * 1.25) + 1, oneH });
        note.classList.toggle('one', p.one);
        note.style.left = `${wr.left + wr.width / 2 - hr.left}px`;
        note.style.bottom = `${Math.max(1, hr.bottom - floor + p.lift)}px`;
    };
    const showNote = (n, ts) => {
        if (n) {
            if (noteText.textContent !== n.text) noteText.textContent = n.text;
            if (noteSub.textContent !== n.sub) noteSub.textContent = n.sub;
            if (!noteShown || ts - notePlacedAt > 250) { notePlacedAt = ts; placeNote(); }
        }
        if (!!n !== noteShown) {
            noteShown = !!n;
            note.classList.toggle('show', noteShown);
        }
    };

    /// Show scene `id` ('my:…'): compiled once; the scene before goes on
    /// until the new one is ready. `text`: a live edit from the studio.
    const show = async (id, text, values) => {
        const e = catalog.find(c => c.id === id);
        if (!e && text == null) return;
        wanted = id;
        const r = await renderer.setScene(text ?? e.text, id, values ?? valuesFor(e));
        if (wanted !== id || r.superseded) return;
        wanted = null;
        if (r.ok) { current = id; failed.delete(id); }
        else if (text == null) {
            console.warn('[vis] scene does not compile:', id, r.errors);
            failed.add(id);
            current = null;
            sync(false);
        }
    };
    const readCatalog = async () => {
        catalog = await loadCatalog();
        sceneLooks = catalog.map(e => ({ id: 'vis:' + e.id, name: e.header.name, spatial: !!e.header.spatial,
            why: (e.header.about || 'a visualization scene') + (e.header.spatial ? ' (with instruments)' : '') }));
        // A look that is gone (a scene removed): the automatic one.
        if (isScene(look) && !findLook(look)) setLook('album', false);
        if (isScene(lastEffect) && !findLook(lastEffect)) lastEffect = null;
        sync(false);
    };
    readCatalog().then(() => { if (current) show(current); });

    // The studio: sliders live, code compiled there, back on close, a scene
    // chosen, the list changed.
    visListen('vis:params', ({ id, values }) => { if (current === id) renderer.setValues(values); });
    visListen('vis:code', ({ id, text, values }) => {
        if (drawn() !== 'vis:' + id) return;
        failed.delete(id);
        show(id, text, values || valuesOf(catalog.find(c => c.id === id)?.header || { params: [] }, keptValues(id)));
    });
    visListen('vis:revert', ({ id }) => { if (current === id) show(id); });
    visListen('vis:look', ({ id }) => {
        if (id) { failed.delete(id); readCatalog().then(() => setLook('vis:' + id, true)); }
        else if (isScene(look)) setLook('album', true);
    });
    visListen('vis:changed', () => readCatalog().then(() => { if (current) show(current); }));

    // How far ahead the render keeps the sound (sent when it changes).
    let aheadSent = -1, aheadAt = 0;
    const needAhead = d => {
        const st = state();
        const want = d && on() && !document.hidden && wrap.offsetParent && (st === 'playing' || st === 'paused') ? AHEAD_S : 0;
        const t = performance.now();
        if (Math.abs(want - aheadSent) < 0.05 || t - aheadAt < 500) return;
        aheadSent = want;
        aheadAt = t;
        window.__TAURI__?.tauri.invoke('player_set_vis_ahead', { source: 'player', seconds: want }).catch(() => {});
    };

    // The bars off (listening.js puts lm-bars-off on the player): the picture
    // takes the whole player. Its top keeps the band's quiet profile — the
    // band's share of the height is measured (the band ends where the seek
    // bar's row starts) — and under the seek bar and the buttons it is
    // dimmed, full again while they are away (the showcase).
    const LOWER_DIM = 0.55;
    let lower = 1;
    const wholeBandK = () => {
        const seek = bar?.querySelector('.pl-seek');
        const wr = wrap.getBoundingClientRect();
        if (!seek || wr.height < 1) return 1;
        const top = seek.getBoundingClientRect().top - (parseFloat(getComputedStyle(seek).marginTop) || 0);
        return Math.max(0.2, Math.min(1, (top - wr.top) / wr.height));
    };

    // Up to 60 frames a second, on the scene's own clock: it runs with the
    // music, slows to a stop on pause (the picture holds still), and runs
    // slower while nothing plays.
    let last = 0, clock = 0, pace = 1;
    const step = ts => {
        requestAnimationFrame(step);
        const d = drawn();
        needAhead(d);
        if (last && ts - last < 1000 / 62) return;
        const dt = last ? Math.min(0.1, (ts - last) / 1000) : 0.016;
        last = ts;
        if (!d) { if (canvas.style.display !== 'none') canvas.style.display = 'none'; showNote(null, ts); return; }
        if (document.hidden || !on() || !wrap.offsetParent) { showNote(null, ts); return; }
        if (canvas.style.display) canvas.style.display = '';
        const id = d.slice(4);
        if (current !== id && wanted !== id) show(id);
        const st = state();
        pace = st === 'paused' ? pace * 0.85 : pace + ((st === 'playing' ? 1 : 0.45) - pace) * 0.05;
        clock += dt * pace;
        const m = music.frame(dt, { spatial: renderer.spatial });
        const ps = status?.();
        showNote(instrumentsNote({ scene: renderer.spatial, state: st, progress: m.spatialProgress, busy: m.spatialBusy,
            source: m.spatial, count: m.objCount, stream: !!ps?.radio && !ps.radio.stopped }), ts);
        // On the whole screen (fullview.js): no text over it to keep clear.
        const full = wrap.dataset.full === '1';
        const whole = !full && !!bar?.classList.contains('lm-bars-off');
        const away = !!bar?.querySelector('.pl-top')?.classList.contains('pl-showcase');
        lower += ((away ? 1 : LOWER_DIM) - lower) * Math.min(1, dt * 4);
        renderer.frame(m, { clock, dt: dt * pace, fade: full ? 0 : whole ? 2 : 1,
            bandK: whole ? wholeBandK() : 1, lower, full, budgetMs: full ? 8 : 5 });
    };
    requestAnimationFrame(step);

    // For our checks.
    wrap.__setLook = id => setLook(migrate(id), false);
    wrap.__vis = () => ({ look, drawn: drawn(), failed: [...failed], scene: renderer.sceneId,
        info: renderer.info(), music: music.debug(), looks: looks().map(l => l.id),
        note: noteShown ? { text: noteText.textContent, sub: noteSub.textContent, at: note.getBoundingClientRect().toJSON() } : null });
}
