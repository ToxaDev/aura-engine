
// ══════════════════════════════════════════════════════════════════════
// The visualization studio (vis-studio.html; Anton 27.09).
//
// Left: the scenes there are (the app's, the user's). Middle: the preview
// on the music playing now, the AI bar (copy the spec with an idea into
// any AI chat, paste its answer back), the code — compiled half a second
// after typing stops; a scene that does not compile leaves the last good
// one running and says where it is wrong. Right: the scene's sliders (its
// @param lines), Save, Show in player, Export.
//
// Values of the sliders live in localStorage per scene (catalog.js), shared
// with the player's window; moving one is sent to the player at once
// (vis:params), a compiled change of the code too (vis:code) — the player
// takes them if it shows that scene. Closing without saving takes the
// player back (vis:revert).
// ══════════════════════════════════════════════════════════════════════

// First: a press outside an open list is stopped before anything else of
// the studio hears it (../popups.js).
import '../popups.js';
import { buildScene, parseHeader, valuesOf, withDefaults, extractCode, colourHex, fmtNum, setSpec, SPEC_MAJOR, fromShadertoy } from './scene.js';
import { createMusic } from './music.js';
import { createRenderer } from './renderer.js';
import { loadCatalog, keptValues, keepValues, forgetValues, emit, listen } from './catalog.js';
import { createTipTiming } from '../tip-timing.js';
import { placeTip } from '../tip-place.js';
import { ensureInstruments, packLine } from './pack.js';

const $ = id => document.getElementById(id);
const T = window.__TAURI__;
const invoke = (c, a) => T.tauri.invoke(c, a);
const win = T.window.appWindow;

// The player's picture is about this shape (its band in listening mode).
const PLAYER_AR = 418 / 262;
const LOOK_KEY = 'auraWavesLook';
const STUDIO_SCENE = 'auraVisStudioScene';
const COMPILE_MS = 450;

let catalog = [];
let cur = null;          // { id, file, mine, isNew, savedText, text, header, values, savedValues }
let aspect = 'player';
let specText = null;

// ── the player's status (this window polls it; the player's window has its own) ──
let status = null;
async function pollStatus() {
    try {
        const r = await fetch('https://aura.localhost/player/status', { cache: 'no-store' });
        if (r.ok) status = await r.json();
    } catch (_) {}
}
setInterval(pollStatus, 300);
pollStatus();
const delayMs = () => {
    try { return Number(localStorage.getItem('auraSpecDelay:' + (status?.device || ''))) || 0; } catch (_) { return 0; }
};

// ── the preview ──
const canvas = $('cv');
const music = createMusic({ status: () => status, delayMs });
let lostNote = '';
const renderer = createRenderer(canvas, {
    onLost: ({ soon }) => { lostNote = soon ? 'The video card dropped the picture right after this scene started — check its loops.' : 'The video card dropped the picture; waiting for it to come back.'; },
    onRestored: () => { lostNote = ''; music.attach(renderer.sink); },
    onHeavy: ({ why }) => showErrors([{ line: 0, msg: 'Stopped: ' + why + '. Make it lighter, or add // @quality 0.5.' }]),
});
if (renderer) music.attach(renderer.sink);

let pvOn = false;          // the preview on the whole screen
function fitPreview() {
    const box = $('preview');
    if (pvOn) {
        canvas.style.width = box.clientWidth + 'px';
        canvas.style.height = box.clientHeight + 'px';
        return;
    }
    const bw = box.clientWidth - 16, bh = box.clientHeight - 16;
    const ar = aspect === 'player' ? PLAYER_AR : (screen.width / screen.height || 16 / 9);
    let w = Math.min(bw, bh * ar), h = w / ar;
    canvas.style.width = Math.max(40, Math.floor(w)) + 'px';
    canvas.style.height = Math.max(24, Math.floor(h)) + 'px';
}
new ResizeObserver(fitPreview).observe($('preview'));
// Player | Screen: kept for the next time the studio opens.
const ASPECT_KEY = 'auraVisStudioAspect';
function setAspect(a, keep) {
    aspect = a === 'screen' ? 'screen' : 'player';
    for (const x of $('aspect').querySelectorAll('button')) x.classList.toggle('on', x.dataset.a === aspect);
    if (keep) try { localStorage.setItem(ASPECT_KEY, aspect); } catch (_) {}
    fitPreview();
}
for (const b of $('aspect').querySelectorAll('button')) b.addEventListener('click', () => setAspect(b.dataset.a, true));
try { setAspect(localStorage.getItem(ASPECT_KEY), false); } catch (_) {}

// The bar between the preview and the code: dragged (or its arrow keys), it shares the room between them; the
// share of the column is kept for the next time. Double-click: the usual share. The code keeps at least its
// 120 px, the preview its 150 px (the CSS minimums).
const SPLIT_KEY = 'auraVisStudioSplit', SPLIT_USUAL = .44;
const split = $('split'), pvBox = $('previewBox');
const columnHeight = () => pvBox.parentElement.clientHeight - 20;     // .center's padding: 10 px above and below
function splitRange() {
    const h = pvBox.getBoundingClientRect().height, ed = $('editor').getBoundingClientRect().height;
    return [150, Math.max(150, h + ed - 120)];
}
function setSplit(frac, keep) {
    if (!(frac > 0 && frac < 1)) return;
    pvBox.style.flexBasis = (frac * 100).toFixed(2) + '%';
    if (keep) try { localStorage.setItem(SPLIT_KEY, frac.toFixed(4)); } catch (_) {}
}
function splitTo(px, keep) {
    const [lo, hi] = splitRange();
    setSplit(Math.min(hi, Math.max(lo, px)) / columnHeight(), keep);
}
split.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    e.preventDefault();
    split.setPointerCapture(e.pointerId);
    split.classList.add('drag');
    document.body.classList.add('split-drag');
    const y0 = e.clientY, h0 = pvBox.getBoundingClientRect().height;
    const [lo, hi] = splitRange();
    const move = ev => { pvBox.style.flexBasis = Math.min(hi, Math.max(lo, h0 + ev.clientY - y0)) + 'px'; };
    const up = () => {
        split.removeEventListener('pointermove', move);
        split.removeEventListener('pointerup', up);
        split.removeEventListener('pointercancel', up);
        split.classList.remove('drag');
        document.body.classList.remove('split-drag');
        setSplit(pvBox.getBoundingClientRect().height / columnHeight(), true);
    };
    split.addEventListener('pointermove', move);
    split.addEventListener('pointerup', up);
    split.addEventListener('pointercancel', up);
});
split.addEventListener('dblclick', () => setSplit(SPLIT_USUAL, true));
split.addEventListener('keydown', e => {
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    e.preventDefault();
    splitTo(pvBox.getBoundingClientRect().height + (e.key === 'ArrowUp' ? -24 : 24), true);
});
try { setSplit(Number(localStorage.getItem(SPLIT_KEY)), false); } catch (_) {}

let clock = 0, pace = 1, lastTs = 0, frames = 0, fpsAt = 0, fps = 0;
function loop(ts) {
    requestAnimationFrame(loop);
    const dt = lastTs ? Math.min(0.1, (ts - lastTs) / 1000) : 0.016;
    if (lastTs && ts - lastTs < 1000 / 62) return;     // 60 a second at most
    lastTs = ts;
    const st = status?.state || 'stopped';
    pace = st === 'paused' ? pace * 0.85 : pace + ((st === 'playing' ? 1 : 0.45) - pace) * 0.05;
    clock += dt * pace;
    const m = music.frame(dt, { spatial: renderer?.spatial });
    if (renderer && !renderer.dead) renderer.frame(m, { clock, dt: dt * pace, fade: aspect === 'player' && !pvOn, full: aspect === 'screen' || pvOn, budgetMs: 8 });
    frames++;
    if (ts - fpsAt > 700) {
        fps = frames * 1000 / (ts - fpsAt);
        frames = 0;
        fpsAt = ts;
        stat(m);
    }
}
requestAnimationFrame(loop);

function stat(m) {
    const note = $('pvNote');
    let text = '', warn = false;
    if (!renderer) { text = 'This computer\'s browser engine has no WebGL2: scenes cannot be drawn.'; warn = true; }
    else if (lostNote) { text = lostNote; warn = true; }
    else if (renderer.heavy) { text = 'Stopped: ' + renderer.heavy; warn = true; }
    else if (!status || status.state === 'stopped') text = 'Nothing is playing: this is the scene at rest. Play a track to see it on the music.';
    else if (status.state === 'paused') text = 'Paused: the picture holds still.';
    note.textContent = text;
    note.classList.toggle('warn', warn);
    if (!renderer) return;
    const i = renderer.info();
    const parts = [];
    if (m.live) {
        parts.push(`ahead ${m.ahead.toFixed(1)} s`);
        if (m.bpm) parts.push(`${Math.round(m.bpm)} bpm${m.beatConf < 0.35 ? '?' : ''}`);
    }
    if (renderer.spatial && status && status.state !== 'stopped') parts.push(objectsState(m));
    parts.push(`${Math.round(fps)} fps`);
    if (i.timed && i.gpuMs) parts.push(`card ${i.gpuMs < 1 ? i.gpuMs.toFixed(2) : i.gpuMs.toFixed(1)} ms`);
    parts.push(`${Math.round((i.scale || 1) * 100)} %`);
    if (i.feedback) parts.push('feedback');
    $('pvStat').textContent = parts.join(' · ');
}

/// Where the objects come from now, for the preview's line.
function objectsState(m) {
    const p = m.spatialProgress;
    const sep = p >= 0 && p < 1 ? ` (separating ${Math.round(p * 100)} %)` : '';
    if (m.spatial === 2) return 'objects: instruments' + sep;
    if (m.spatial === 1) return (p < 0 ? 'objects: from the mix (no pack)' : 'objects: from the mix') + sep;
    return 'objects: listening…';
}

// The scene's book: Standard (1, the whole mix) or With instruments (2) —
// its `// @spec` line. With instruments needs the pack: switched only once
// it is there. Beside the switch, the pack's line (pack.js).
const specOf = () => (cur ? parseHeader(cur.text).spec : 1);
const packLook = packLine($('spState'), specOf);
let packKey = '';
function showSpec() {
    const h = cur ? parseHeader(cur.text) : null;
    const s = h ? h.spec : 1;
    for (const b of $('tier').querySelectorAll('button')) {
        b.classList.toggle('on', Number(b.dataset.t) === Math.min(s, SPEC_MAJOR));
        b.disabled = !cur;
    }
    $('specNote').textContent = h?.specNewer
        ? `Written for a newer Aura Engine (spec ${h.spec}${h.specMinor ? '.' + h.specMinor : ''}): parts of it may not work until the app is updated.`
        : '';
}
for (const b of $('tier').querySelectorAll('button')) {
    b.addEventListener('click', async () => {
        if (!cur) return;
        const want = Number(b.dataset.t);
        if (want === Math.min(specOf(), SPEC_MAJOR)) return;
        if (want >= 2 && !(await ensureInstruments())) return;
        code.value = setSpec(cur.text, want);
        code.dispatchEvent(new Event('input'));
        showSpec();
        packLook();
    });
}

// ── tooltips (the app's timing) ──
const tipT = createTipTiming();
let tipTimer = null, tipEl = null;
document.addEventListener('mouseover', e => {
    const el = e.target.closest?.('[data-tip]');
    if (el === tipEl) return;
    clearTimeout(tipTimer);
    if (tipEl) { $('tip').classList.remove('show'); tipT.gone(); }
    tipEl = el;
    if (!el) return;
    tipTimer = setTimeout(() => {
        const tip = $('tip');
        tip.textContent = el.dataset.tip;
        tip.classList.add('show');
        placeTip(tip, el.getBoundingClientRect(), { side: 'below', gap: 6 });
        tipT.shown();
    }, tipT.delay());
});
document.addEventListener('mousedown', () => { clearTimeout(tipTimer); $('tip').classList.remove('show'); tipT.gone(true); tipEl = null; }, true);

// ── the list: every scene, one list (remove or add as you like) ──
function playerLook() { try { return localStorage.getItem(LOOK_KEY) || ''; } catch (_) { return ''; } }

function renderList() {
    const box = $('listScroll');
    box.innerHTML = '';
    const inPlayer = playerLook();
    const items = catalog.slice();
    if (cur?.isNew) items.push({ id: cur.id, header: cur.header, isNew: true });
    if (!items.length) {
        const e = document.createElement('div');
        e.className = 'empty';
        e.textContent = 'No scenes. New, Import…, an AI\'s answer (Paste) — or bring back the app\'s scenes below.';
        box.appendChild(e);
    }
    for (const it of items) {
        const b = document.createElement('div');
        b.className = 'item';
        b.setAttribute('role', 'button');
        b.classList.toggle('on', cur?.id === it.id);
        b.classList.toggle('in-player', inPlayer === 'vis:' + it.id);
        b.innerHTML = '<span class="dot"></span><span class="nm"></span>';
        b.querySelector('.nm').textContent = it.header.name;
        if (it.header.spatial) {
            const s = document.createElement('span');
            s.className = 'inst';
            s.textContent = 'instruments';
            b.appendChild(s);
        }
        b.dataset.tip = (it.header.about || it.header.name) + (it.header.spatial ? ' — draws the instruments (needs the instruments pack)' : '')
            + (inPlayer === 'vis:' + it.id ? ' — shown in the player now' : '');
        b.addEventListener('click', () => select(it.id));
        if (it.isNew) {
            const s = document.createElement('span');
            s.className = 'new';
            s.textContent = 'not saved';
            b.appendChild(s);
        } else {
            const del = document.createElement('span');
            del.className = 'del';
            del.textContent = '✕';
            del.dataset.tip = 'Remove this scene';
            del.addEventListener('click', ev => { ev.stopPropagation(); removeScene(it.id); });
            b.appendChild(del);
        }
        box.appendChild(b);
    }
}

async function removeScene(id) {
    const e = catalog.find(c => c.id === id);
    if (!e) return;
    const yes = await T.dialog.ask(`Remove "${e.header.name}"? Its file is deleted.`, { title: 'Visualization studio', type: 'warning' });
    if (!yes) return;
    try {
        await invoke('vis_delete', { file: e.file });
        forgetValues(id);
        if (playerLook() === 'vis:' + id) emit('vis:look', { id: null });
        const wasCur = cur?.id === id;
        if (wasCur) cur = null;
        await reloadCatalog();
        emit('vis:changed', {});
        if (wasCur) {
            if (catalog.length) await select(catalog[0].id, true);
            else { renderParams(); header(); setCode(''); }
        }
        note(`"${e.header.name}" removed.`);
    } catch (err) { note('Not removed: ' + err, true); }
}

async function reloadCatalog() {
    catalog = await loadCatalog();
    renderList();
}

// ── the current scene ──
const dirtyCode = () => !!cur && cur.text !== cur.savedText;
const dirtyValues = () => !!cur && JSON.stringify(cur.values) !== JSON.stringify(cur.savedValues);
const dirty = () => dirtyCode() || dirtyValues() || !!cur?.isNew;

function header() {
    const hn = $('hdrName');
    hn.textContent = cur ? '— ' + cur.header.name : '';
    if (cur && dirty()) { const s = document.createElement('span'); s.className = 'dirty'; s.textContent = ' •'; hn.appendChild(s); }
    $('infoName').textContent = cur?.header.name || '';
    $('infoBy').textContent = cur ? [cur.header.author && 'by ' + cur.header.author, cur.header.license && 'licence: ' + cur.header.license,
        cur.isNew && 'new, not saved'].filter(Boolean).join(' · ') : '';
    $('infoAbout').textContent = cur?.header.about || '';
    for (const id of ['bUse', 'bSaveAs', 'bExport', 'bSave', 'bRevert', 'bDefaults', 'bFold']) $(id).disabled = !cur;
    const key = (cur?.id || '') + ':' + specOf();
    if (key !== packKey) { packKey = key; packLook(); }
    showSpec();
    const use = $('bUse');
    const shown = !!cur && playerLook() === 'vis:' + cur.id;
    use.classList.toggle('shown', shown);
    use.textContent = shown ? '● In the player' : 'Show in player';
    const st = $('st');
    if (!cur) { st.textContent = ''; return; }
    if (cur.isNew) st.textContent = 'Not saved yet: Save puts it in the list.';
    else if (dirty()) st.textContent = 'Not saved: the preview and the player show it; Save keeps it.';
    else st.textContent = 'Saved.';
    st.classList.toggle('dirty', dirty());
}

async function confirmLeave() {
    if (!dirty()) return true;
    return await T.dialog.ask(`"${cur.header.name}" has changes that are not saved. Leave them?`, { title: 'Visualization studio', type: 'warning' });
}

async function select(id, force = false) {
    if (cur?.id === id && !force) return;
    if (!force && !(await confirmLeave())) return;
    if (cur && dirty()) emit('vis:revert', { id: cur.id });
    const e = catalog.find(c => c.id === id);
    if (!e) return;
    const values = valuesOf(e.header, keptValues(e.id));
    cur = { id: e.id, file: e.file, mine: e.mine, isNew: false, savedText: e.text, text: e.text, header: e.header,
        values, savedValues: JSON.parse(JSON.stringify(values)) };
    try { localStorage.setItem(STUDIO_SCENE, id); } catch (_) {}
    setCode(e.text);
    renderList();
    renderParams();
    header();
    compileNow();
}

function newScene(text, name) {
    const h = parseHeader(text);
    cur = { id: 'new:' + Date.now(), file: null, mine: false, isNew: true, savedText: '', text, header: h,
        values: valuesOf(h, {}), savedValues: {} };
    setCode(text);
    renderList();
    renderParams();
    header();
    compileNow();
}

// ── the editor ──
const code = $('code');
function setCode(t) {
    code.value = t;
    code.scrollTop = 0;
    gutter([]);
}
function gutter(errs) {
    const n = code.value.split('\n').length;
    const bad = new Set(errs.filter(e => e.line > 0).map(e => e.line));
    let s = '';
    for (let i = 1; i <= n; i++) s += bad.has(i) ? `<span class="e">${i}</span>\n` : i + '\n';
    $('gutter').innerHTML = s;
    const lh = parseFloat(getComputedStyle(code).lineHeight) || 18.75;
    const hl = $('hl');
    hl.innerHTML = '';
    for (const l of bad) {
        const d = document.createElement('div');
        d.style.top = (8 + (l - 1) * lh) + 'px';
        d.style.height = lh + 'px';
        hl.appendChild(d);
    }
    syncScroll();
}
function syncScroll() {
    $('gutter').scrollTop = code.scrollTop;
    $('hl').style.transform = `translateY(${-code.scrollTop}px)`;
}
code.addEventListener('scroll', syncScroll);
// A shader's JSON from Shadertoy pasted into the editor: the whole scene, its passes and channels.
code.addEventListener('paste', e => {
    const t = e.clipboardData?.getData('text') || '';
    const scene = fromShadertoy(t);
    if (!scene) return;
    e.preventDefault();
    code.select();
    document.execCommand('insertText', false, scene);
    const passes = (scene.match(/^\s*\/\/\s*@pass\b/gm) || []).length;
    note(`A shader from Shadertoy${passes ? `, ${passes} passes` : ''}: its author, link and licence are in the header. Save keeps it.`);
});
let typeTimer = null;
code.addEventListener('input', () => {
    if (!cur) return;
    cur.text = code.value;
    const h = parseHeader(cur.text);
    header();
    clearTimeout(typeTimer);
    typeTimer = setTimeout(compileNow, COMPILE_MS);
    if (h.params.length !== cur.header.params.length) gutter(lastErrors);
});
code.addEventListener('keydown', e => {
    if (e.key === 'Tab') {
        e.preventDefault();
        const s = code.selectionStart, en = code.selectionEnd;
        // Typed through execCommand, so Ctrl+Z undoes it like typing.
        if (s === en && !e.shiftKey) {
            document.execCommand('insertText', false, '    ');
        } else {
            // Indent or unindent the lines of the selection.
            const v = code.value;
            const ls = v.lastIndexOf('\n', s - 1) + 1;
            let le = v.indexOf('\n', en - (en > s && v[en - 1] === '\n' ? 1 : 0));
            if (le < 0) le = v.length;
            const out = v.slice(ls, le).split('\n').map(l => e.shiftKey ? l.replace(/^ {1,4}|^\t/, '') : '    ' + l).join('\n');
            code.setSelectionRange(ls, le);
            document.execCommand('insertText', false, out);
            code.setSelectionRange(ls, ls + out.length);
        }
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        clearTimeout(typeTimer);
        compileNow();
    } else if (e.key === 'Enter' && !e.shiftKey && !e.altKey) {
        // Keep the line's indent.
        e.preventDefault();
        const s = code.selectionStart;
        const ls = code.value.lastIndexOf('\n', s - 1) + 1;
        const ind = /^[ \t]*/.exec(code.value.slice(ls, s))[0];
        const extra = /\{\s*$/.test(code.value.slice(ls, s)) ? '    ' : '';
        document.execCommand('insertText', false, '\n' + ind + extra);
    }
});

// ── compiling ──
let lastErrors = [];
let compiling = 0;
async function compileNow() {
    if (!cur || !renderer) return;
    const my = ++compiling;
    const text = cur.text;
    const r = await renderer.setScene(text, cur.id, cur.values);
    if (my !== compiling || r.superseded) return;
    if (r.ok) {
        const oldNames = cur.header.params.map(p => p.name + p.type + p.min + p.max).join();
        cur.header = r.header;
        cur.values = valuesOf(cur.header, cur.values);
        renderer.setValues(cur.values);
        if (cur.header.params.map(p => p.name + p.type + p.min + p.max).join() !== oldNames || !$('params').children.length) renderParams();
        lastErrors = [];
        showErrors([], r.compileMs);
        emit('vis:code', { id: cur.id, text, values: cur.values });
        header();
        renderList();
    } else {
        lastErrors = r.errors;
        showErrors(r.errors);
    }
    gutter(lastErrors);
}

function showErrors(errs, ms) {
    const box = $('errors');
    box.innerHTML = '';
    if (!errs.length) {
        if (ms != null) {
            const d = document.createElement('div');
            d.className = 'ok';
            d.textContent = `Compiled in ${Math.round(ms)} ms.`;
            box.appendChild(d);
        }
        return;
    }
    for (const e of errs.slice(0, 12)) {
        const d = document.createElement('div');
        d.className = 'er';
        d.innerHTML = e.line ? `<b>line ${e.line}</b> ` : '';
        d.appendChild(document.createTextNode(e.msg));
        if (e.line) d.addEventListener('click', () => goLine(e.line));
        box.appendChild(d);
    }
    if (errs.length && renderer?.sceneId) {
        const d = document.createElement('div');
        d.style.color = 'var(--mute)';
        d.textContent = 'The last scene that compiled is still shown.';
        box.appendChild(d);
    }
}
function goLine(n) {
    const lines = code.value.split('\n');
    let s = 0;
    for (let i = 0; i < n - 1 && i < lines.length; i++) s += lines[i].length + 1;
    code.focus();
    code.setSelectionRange(s, s + (lines[n - 1] || '').length);
    const lh = parseFloat(getComputedStyle(code).lineHeight) || 18.75;
    code.scrollTop = Math.max(0, (n - 4) * lh);
}

// ── the settings: the scene's @section groups, each folding (open by default) ──
const FOLD_KEY = 'auraVis:folded:';
function foldedOf(id) { try { return new Set(JSON.parse(localStorage.getItem(FOLD_KEY + id) || '[]')); } catch (_) { return new Set(); } }
function keepFolded(id, set) { try { localStorage.setItem(FOLD_KEY + id, JSON.stringify([...set])); } catch (_) {} }

function renderParams() {
    const box = $('params');
    const scroll = box.scrollTop;
    box.innerHTML = '';
    if (!cur) return;
    const ps = cur.header.params;
    if (!ps.length) {
        const d = document.createElement('div');
        d.className = 'none';
        d.textContent = 'This scene has no settings. Lines like  // @section "Rings"  and  // @param float speed 1 0 3 0.05 "Speed"  in its header make them.';
        box.appendChild(d);
        return;
    }
    const folded = foldedOf(cur.id);
    const byName = new Map(ps.map(p => [p.name, p]));
    const secs = cur.header.sections.filter(s => s.params.some(n => byName.has(n)));
    for (const s of secs) {
        const sec = document.createElement('div');
        sec.className = 'sec';
        const key = s.name || '(settings)';
        sec.classList.toggle('folded', folded.has(key));
        const members = s.params.map(n => byName.get(n)).filter(Boolean);
        // A scene without sections: its settings with no head of their own.
        if (secs.length > 1 || s.name) {
            const head = document.createElement('button');
            head.type = 'button';
            head.className = 'sec-head';
            head.innerHTML = '<span class="chev">▼</span><span class="sec-name"></span><span class="sec-n"></span>';
            head.querySelector('.sec-name').textContent = s.name || 'Settings';
            head.querySelector('.sec-n').textContent = String(members.length);
            head.addEventListener('click', () => {
                const f = foldedOf(cur.id);
                if (f.has(key)) f.delete(key); else f.add(key);
                keepFolded(cur.id, f);
                sec.classList.toggle('folded', f.has(key));
            });
            sec.appendChild(head);
            if (s.why) {
                const w = document.createElement('div');
                w.className = 'sec-why';
                w.textContent = s.why;
                sec.appendChild(w);
            }
        }
        const body = document.createElement('div');
        body.className = 'sec-body';
        if (!(secs.length > 1 || s.name)) body.style.paddingLeft = '0';
        for (const p of members) body.appendChild(paramRow(p));
        sec.appendChild(body);
        box.appendChild(sec);
    }
    box.scrollTop = scroll;
}

function paramRow(p) {
    const row = document.createElement('div');
    row.className = 'p';
    row.innerHTML = '<div class="top"><span class="nm"></span><span class="v"></span></div><div class="ctl"></div><div class="why"></div>';
    const nm = row.querySelector('.nm'), v = row.querySelector('.v'), ctl = row.querySelector('.ctl');
    nm.textContent = p.label;
    const defText = p.type === 'color' ? colourHex(p.def) : p.type === 'choice' ? p.options[p.def] : p.type === 'bool' ? (p.def ? 'on' : 'off') : fmtNum(p.def, p);
    nm.dataset.tip = `Double-click: back to ${defText} (the scene's default) · in the code: ${p.name}`;
    row.querySelector('.why').textContent = p.why || '';
    let input;
    if (p.type === 'color') {
        input = document.createElement('input');
        input.type = 'color';
    } else if (p.type === 'bool') {
        input = document.createElement('input');
        input.type = 'checkbox';
    } else if (p.type === 'choice') {
        input = document.createElement('select');
        p.options.forEach((o, i) => { const opt = document.createElement('option'); opt.value = String(i); opt.textContent = o; input.appendChild(opt); });
    } else {
        input = document.createElement('input');
        input.type = 'range';
        Object.assign(input, { min: p.min, max: p.max, step: p.step });
    }
    input.dataset.name = p.name;
    // A switch sits at the right of its name; the others under it.
    if (p.type === 'bool') { v.appendChild(input); ctl.remove(); } else ctl.appendChild(input);
    const show = () => {
        const val = cur.values[p.name];
        if (p.type === 'color') { input.value = colourHex(val); v.textContent = colourHex(val); }
        else if (p.type === 'bool') { input.checked = !!val; }
        else if (p.type === 'choice') { input.value = String(val); v.textContent = ''; }
        else {
            input.value = String(val);
            v.textContent = fmtNum(val, p);
            if (p.unit) { const u = document.createElement('small'); u.textContent = p.unit; v.appendChild(u); }
        }
        const saved = cur.savedValues[p.name] ?? p.def;
        nm.classList.toggle('changed', JSON.stringify(val) !== JSON.stringify(saved));
    };
    const set = x => {
        cur.values[p.name] = x;
        renderer?.setValues(cur.values);
        emit('vis:params', { id: cur.id, values: cur.values });
        show();
        header();
    };
    input.addEventListener(p.type === 'choice' ? 'change' : 'input', () => {
        if (p.type === 'color') set(hexToRgb(input.value));
        else if (p.type === 'bool') set(input.checked ? 1 : 0);
        else if (p.type === 'choice') set(Number(input.value));
        else set(p.type === 'int' ? Math.round(Number(input.value)) : Number(input.value));
    });
    nm.addEventListener('dblclick', () => set(Array.isArray(p.def) ? p.def.slice() : p.def));
    // A number: click it to type one.
    if (p.type === 'float' || p.type === 'int') {
        v.dataset.tip = 'Click to type a value';
        v.addEventListener('click', () => {
            if (v.querySelector('input')) return;
            const box = document.createElement('input');
            box.type = 'text';
            box.value = fmtNum(cur.values[p.name], p);
            v.textContent = '';
            v.appendChild(box);
            box.focus();
            box.select();
            const done = ok => {
                const x = Number(box.value.replace(',', '.'));
                if (ok && Number.isFinite(x)) set(Math.min(p.max, Math.max(p.min, p.type === 'int' ? Math.round(x) : x)));
                else show();
            };
            box.addEventListener('keydown', e => { if (e.key === 'Enter') done(true); else if (e.key === 'Escape') done(false); e.stopPropagation(); });
            box.addEventListener('blur', () => done(true));
        });
    }
    show();
    return row;
}
$('bFold').addEventListener('click', () => {
    if (!cur) return;
    const keys = cur.header.sections.map(s => s.name || '(settings)');
    const f = foldedOf(cur.id);
    const all = keys.every(k => f.has(k));
    keepFolded(cur.id, all ? new Set() : new Set(keys));
    $('bFold').textContent = all ? 'Fold all' : 'Open all';
    renderParams();
});
const hexToRgb = h => [1, 3, 5].map(i => parseInt(h.slice(i, i + 2), 16) / 255);

// ── saving ──
function ask(title, value) {
    return new Promise(resolve => {
        const m = $('modal'), inp = $('modalInput');
        $('modalTitle').textContent = title;
        inp.value = value || '';
        m.hidden = false;
        inp.focus();
        inp.select();
        const done = v => { m.hidden = true; $('modalYes').onclick = $('modalNo').onclick = inp.onkeydown = null; resolve(v); };
        $('modalYes').onclick = () => done(inp.value.trim() || null);
        $('modalNo').onclick = () => done(null);
        inp.onkeydown = e => { if (e.key === 'Enter') done(inp.value.trim() || null); else if (e.key === 'Escape') done(null); };
    });
}
/// The text with its @name line set to `name` (added when there is none).
function named(text, name) {
    if (/^\s*\/\/\s*@name\b.*$/m.test(text)) return text.replace(/^(\s*\/\/\s*@name\s*).*$/m, (_, a) => a + name);
    return `// @name     ${name}\n` + text;
}

async function saveAsMine(name) {
    const text = named(cur.text, name);
    const file = await invoke('vis_save', { file: null, name, text });
    const id = 'my:' + file;
    keepValues(id, cur.values);
    await reloadCatalog();
    emit('vis:changed', {});
    const values = cur.values;
    cur = null;
    await select(id, true);
    cur.values = values;
    return id;
}

async function save() {
    if (!cur) return;
    try {
        if (cur.isNew) {
            const name = await ask('Name of the new scene', cur.header.name);
            if (!name) return;
            await saveAsMine(name);
            note('Saved to the list.');
            return;
        }
        if (dirtyCode()) {
            await invoke('vis_save', { file: cur.file, name: cur.header.name, text: cur.text });
            cur.savedText = cur.text;
            const e = catalog.find(c => c.id === cur.id);
            if (e) { e.text = cur.text; e.header = cur.header; }
            emit('vis:changed', {});
        }
        keepValues(cur.id, cur.values);
        cur.savedValues = JSON.parse(JSON.stringify(cur.values));
        emit('vis:params', { id: cur.id, values: cur.values });
        renderParams();
        header();
        renderList();
        note('Saved.');
    } catch (e) { note('Not saved: ' + e, true); }
}
function note(text, bad) {
    const st = $('st');
    st.textContent = text;
    st.classList.toggle('dirty', !!bad);
}

$('bSave').addEventListener('click', save);
$('bSaveAs').addEventListener('click', async () => {
    if (!cur) return;
    const name = await ask('Save a copy of this scene as', cur.header.name + ' (copy)');
    if (!name) return;
    try { await saveAsMine(name); note('Saved as a scene of yours.'); } catch (e) { note('Not saved: ' + e, true); }
});
$('bRevert').addEventListener('click', () => {
    if (!cur || cur.isNew) return;
    cur.text = cur.savedText;
    cur.values = JSON.parse(JSON.stringify(cur.savedValues));
    setCode(cur.text);
    cur.header = parseHeader(cur.text);
    renderParams();
    header();
    compileNow();
    emit('vis:revert', { id: cur.id });
});
$('bDefaults').addEventListener('click', () => {
    if (!cur) return;
    cur.values = valuesOf(cur.header, {});
    renderer?.setValues(cur.values);
    emit('vis:params', { id: cur.id, values: cur.values });
    renderParams();
    header();
});
$('bUse').addEventListener('click', async () => {
    if (!cur) return;
    if (cur.isNew) {
        const name = await ask('To show it in the player, save it first — name', cur.header.name);
        if (!name) return;
        try { await saveAsMine(name); } catch (e) { note('Not saved: ' + e, true); return; }
    }
    try { localStorage.setItem(LOOK_KEY, 'vis:' + cur.id); } catch (_) {}
    emit('vis:look', { id: cur.id });
    if (dirtyCode()) emit('vis:code', { id: cur.id, text: cur.text, values: cur.values });
    emit('vis:params', { id: cur.id, values: cur.values });
    renderList();
    header();
    note('Shown in the player (listening mode, the picture behind the title).');
});
$('bExport').addEventListener('click', async () => {
    if (!cur) return;
    try {
        const path = await T.dialog.save({ defaultPath: (cur.file || cur.header.name.replace(/[^\w-]+/g, '-').toLowerCase() + '.aura-vis'),
            filters: [{ name: 'Aura visualization', extensions: ['aura-vis'] }] });
        if (!path) return;
        await invoke('vis_export', { path, text: withDefaults(cur.text, cur.header, cur.values) });
        note('Written to ' + path);
    } catch (e) { note('Not written: ' + e, true); }
});
$('bImport').addEventListener('click', async () => {
    try {
        const path = await T.dialog.open({ multiple: false, filters: [{ name: 'Aura visualization', extensions: ['aura-vis', 'glsl', 'frag', 'txt'] }] });
        if (!path || Array.isArray(path)) return;
        if (!(await confirmLeave())) return;
        const text = await invoke('vis_import', { path });
        const h = parseHeader(text);
        const name = /@name/.test(text) ? h.name : String(path).split(/[\\/]/).pop().replace(/\.[^.]*$/, '');
        const file = await invoke('vis_save', { file: null, name, text: named(text, name) });
        await reloadCatalog();
        emit('vis:changed', {});
        await select('my:' + file, true);
        note('Added to your scenes.');
    } catch (e) { note('Not added: ' + e, true); }
});
$('bFolder').addEventListener('click', () => invoke('vis_dir_open').catch(e => note(String(e), true)));
$('bRestore').addEventListener('click', async () => {
    try {
        const n = await invoke('vis_restore_app');
        await reloadCatalog();
        emit('vis:changed', {});
        note(n ? `${n} of the app's scenes back in the list.` : 'The app\'s scenes are all in the list.');
        if (!cur && catalog.length) await select(catalog[0].id, true);
    } catch (e) { note(String(e), true); }
});
$('bNew').addEventListener('click', async () => {
    if (!(await confirmLeave())) return;
    const spec = await getSpec();
    const m = /#+[^\n]*complete example[^\n]*\n[\s\S]*?```glsl\n([\s\S]*?)```/i.exec(spec);
    newScene(m ? m[1].replace(/^(\/\/\s*@name\s+).*$/m, '$1My scene') : '// @name My scene\n\nvec4 scene(vec2 uv, vec2 p) {\n    return vec4(auraPalette(uv.x), uBass);\n}\n');
});

// ── the AI ──
/// The book for a scene written for `major`: the book itself (Spec 1, the
/// whole mix), and for 2 its instruments part after it.
async function getSpec(major = 1) {
    const read = async f => { try { return await (await fetch('./vis/' + f, { cache: 'no-store' })).text(); } catch (_) { return ''; } };
    if (specText == null) specText = await read('AURA-VIS-SPEC.md');
    if (major < 2) return specText;
    if (specInst == null) specInst = await read('AURA-VIS-SPEC-INSTRUMENTS.md');
    return specText + '\n\n---\n\n' + specInst;
}
let specInst = null;
$('bCopyAi').addEventListener('click', async () => {
    const withInst = specOf() >= 2;
    const spec = await getSpec(withInst ? 2 : 1);
    const idea = $('idea').value.trim();
    let prompt = spec + '\n\n---\n\n';
    if ($('withCode').checked && cur) {
        prompt += 'Here is the current scene:\n\n```glsl\n' + cur.text.replace(/\s+$/, '') + '\n```\n\n';
        prompt += idea ? `Change it: ${idea}\n` : 'Make it better: more beautiful, and use the sound ahead more.\n';
    } else {
        prompt += idea ? `Write a scene: ${idea}\n` : 'Write a scene of your own: beautiful, and showing the music before it is heard.\n';
    }
    if (withInst) prompt += '\nIt is a scene with instruments: keep `// @spec 2` in its header and draw the song\'s instruments as objects in space, as Part F of the book says.';
    prompt += '\nDesign its settings as Part A3 of the book says: sections per element, in that order, the handles a person would reach for, safe ranges, great defaults, the colour choice. Check them against the list at the end of A3.';
    prompt += '\nAnswer with the whole scene file in one ```glsl code block.\n';
    try {
        await T.clipboard.writeText(prompt);
        note(`The prompt is in the clipboard (${Math.round(prompt.length / 1000)}k characters): paste it into an AI chat, then copy its answer and press Paste answer & run.`);
    } catch (e) { note('Clipboard: ' + e, true); }
});
$('bPaste').addEventListener('click', async () => {
    let answer = '';
    try { answer = await T.clipboard.readText(); } catch (e) { note('Clipboard: ' + e, true); return; }
    const fromSt = fromShadertoy(answer);
    if (!fromSt && (!answer || !/scene\s*\(|mainImage/.test(answer))) { note('The clipboard holds no scene (no scene() or mainImage in it). Copy the AI\'s whole answer.', true); return; }
    let text = fromSt || extractCode(answer);
    if (!/@name/.test(text)) text = '// @name     From an AI\n' + text;
    if (cur?.isNew) {
        cur.text = text;
        setCode(text);
        cur.header = parseHeader(text);
        cur.values = valuesOf(cur.header, {});
        renderParams();
        header();
        renderList();
        compileNow();
    } else {
        if (!(await confirmLeave())) return;
        newScene(text);
    }
    note('The answer is in the editor and running. Save keeps it.');
});

// ── the window ──
// The preview shows the sound to come: while this window is open the player
// keeps that much rendered ahead (controller.rs set_vis_ahead).
const setAhead = s => invoke('player_set_vis_ahead', { source: 'studio', seconds: s }).catch(() => {});
setAhead(3.8);
window.addEventListener('beforeunload', () => setAhead(0));
async function closeWin() {
    if (!(await confirmLeave())) return;
    if (cur && dirty()) await emit('vis:revert', { id: cur.id });
    await setAhead(0);
    // Its size, place and state come back next time (window_state.rs).
    await invoke('window_keep_place').catch(() => {});
    win.close();
}
$('bClose').addEventListener('click', closeWin);
$('bMin').addEventListener('click', () => win.minimize());
// Maximize / back; the rounded frame goes square while it fills the screen.
const syncMax = async () => {
    const m = await win.isMaximized().catch(() => false);
    document.body.classList.toggle('maxed', !!m);
    $('bMax').dataset.tip = m ? 'Back to the window (double-click the head bar too)' : 'Maximize (double-click the head bar too)';
};
$('bMax').addEventListener('click', () => win.toggleMaximize().then(syncMax));
$('head').addEventListener('dblclick', e => { if (!e.target.closest('button')) win.toggleMaximize().then(syncMax); });
window.addEventListener('resize', () => { clearTimeout(syncMax.t); syncMax.t = setTimeout(syncMax, 120); });
syncMax();

// The preview on the whole screen, as the player shows it there.
const SVG_FULL = '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M7 1h4v4M11 1 7.2 4.8M5 11H1V7M1 11l3.8-3.8"/></svg>';
const SVG_BACK = '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M11 5H7V1M7 5l3.8-3.8M1 7h4v4M5 7 1.2 10.8"/></svg>';
$('pvFull').innerHTML = SVG_FULL;
$('pvBack').innerHTML = SVG_BACK;
let restTimer = null;
const wake = () => {
    document.body.classList.remove('pv-rest');
    clearTimeout(restTimer);
    if (pvOn) restTimer = setTimeout(() => document.body.classList.add('pv-rest'), 2500);
};
async function pvFull(on) {
    if (on === pvOn) return;
    pvOn = on;
    document.body.classList.toggle('pv-on', on);
    try { await win.setFullscreen(on); } catch (_) {}
    wake();
    setTimeout(fitPreview, 60);
}
// Left on the whole screen last time: the window comes back so
// (window_state.rs), and the page with it.
win.isFullscreen().then(f => { if (f) pvFull(true); }).catch(() => {});
$('pvFull').addEventListener('click', () => pvFull(true));
$('pvBack').addEventListener('click', () => pvFull(false));
document.addEventListener('mousemove', wake);
window.addEventListener('keydown', e => {
    if (e.key === 'Escape' && pvOn) { e.preventDefault(); pvFull(false); }
});

window.addEventListener('keydown', e => {
    if ((e.ctrlKey || e.metaKey) && (e.key === 's' || e.key === 'S')) { e.preventDefault(); save(); }
    if (e.key === 'F5' || ((e.ctrlKey || e.metaKey) && (e.key === 'r' || e.key === 'R'))) e.preventDefault();
});
document.addEventListener('contextmenu', e => { if (!e.target.closest('textarea, input')) e.preventDefault(); });

listen('vis:studio-select', ({ id }) => { if (id && catalog.some(c => c.id === id)) select(id); });
listen('vis:changed', () => { reloadCatalog(); });
listen('vis:look', () => { renderList(); header(); });

(async () => {
    await reloadCatalog();
    let want = null;
    try { want = localStorage.getItem(STUDIO_SCENE); } catch (_) {}
    if (want?.startsWith('app:')) want = 'my:' + want.slice(4);      // the app's scenes live in the list now
    const look = playerLook().replace(/^vis:app:/, 'vis:my:');
    if (!catalog.some(c => c.id === want) && look.startsWith('vis:')) want = look.slice(4);
    if (!catalog.some(c => c.id === want)) want = catalog[0]?.id;
    if (want) await select(want, true);
    fitPreview();
})();

// For our checks.
window.__studio = {
    cur: () => cur && { id: cur.id, name: cur.header.name, params: cur.header.params.map(p => p.name), values: cur.values, dirty: dirty() },
    errors: () => lastErrors,
    info: () => renderer?.info(),
    music: () => music.debug(),
    setCode: t => { code.value = t; code.dispatchEvent(new Event('input')); },
    compile: () => compileNow(),
    catalog: () => catalog.map(c => c.id),
    build: t => { const b = buildScene(t); return { errors: b.errors, entry: b.entry, feedback: b.feedback }; },
};
