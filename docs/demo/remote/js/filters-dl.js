// The filter download on the page: the one-line bar above the rack (in place
// of the old yellow panel), the ⓘ popover with the packs to get by hand, and
// the dialog — Download (what to fetch, progress, pause, done) and Storage
// (what the filters take, the files this version does not use).
//
// The backend (src-tauri/src/filters_dl) does the work: filters_plan says
// which files a setting misses, filters_download fetches them in the
// background (progress at aura.localhost/filters/status), filters_storage and
// filters_trash serve the Storage tab. The decisions are in
// filters-dl-core.js.

import { loadFilterInventory, inventory } from './inventory.js';
import {
    fmtBytes, driveOf, fsText, phasesText, groupSizes, selection, viewOf, rateOf,
    etaText, percent, barModel, barLabel, storageModel, READY_BAR_MS
} from './filters-dl-core.js';
import { auraUrl } from './aura-url.js';

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

const POLL_MS = 500;
const PLAN_TTL_MS = 10 * 60 * 1000;

let ctx = null;            // the bar's setting: see renderFilterBar
let box = null;            // #convFilterAvail
let barShown = false;
let popOpen = false;
const plans = new Map();   // plan key -> { at, plan } | { at, error } | { pending: Promise }
let job = null;            // the download under way or just finished
let hidden = false;        // the dialog put away while the download runs
let pollTimer = null;
let samples = [];
let onChanged = () => {};  // re-reads the marks and the bar (ui.js)
let dlg = null;            // the dialog's own state while it is open
let readyTimer = null;     // takes "filters ready" off the bar

/// ui.js hands over how to redraw the marks once the files changed.
export function setFiltersChangedHook(fn) { onChanged = fn || (() => {}); }

const planKey = c => `${c.taps}|${c.fs}|${c.phases.join('+')}`;

/// Plan with every group, so the dialog can tick them apart; cached a while
/// (the backend caches the signed list for the same time).
function loadPlan(c, fresh = false) {
    const k = planKey(c);
    const hit = plans.get(k);
    if (hit?.pending) return hit.pending;
    if (hit && !fresh && Date.now() - hit.at < PLAN_TTL_MS) return Promise.resolve(hit);
    const pending = invoke('filters_plan', {
        taps: c.taps, fs: c.fs, phases: c.phases, families: ['44k', '48k'], hires: true,
    }).then(plan => ({ at: Date.now(), plan }), e => ({ at: Date.now(), error: String(e) }))
        .then(r => { plans.set(k, r); return r; });
    plans.set(k, { pending });
    return pending;
}

const DEFAULT_PICK = () => new Set(['44k', '48k']);

/// The files on disk changed: forget the plans and fetch the bar's again.
function replan() {
    plans.clear();
    if (ctx?.missing) loadPlan(ctx).then(() => drawBar());
}

// ── the bar ────────────────────────────────────────────────────────────────

/// Draw the bar for the setting chosen. `c` = { missing, empty, taps,
/// tapsLabel, fs, phases, packs }; called by ui.js markAvailability on every
/// change of the sliders, Hybrid-Phase or the inventory.
export function renderFilterBar(el, c) {
    box = el;
    wireOutside();
    ctx = c ? { ...c, key: `${c.taps}|${c.fs}`, label: barLabel(c.tapsLabel, c.fs, c.phases) } : null;
    // A finished download says so for a moment (endReady), less if the
    // setting moves on.
    if (job && job.view === 'done' && (!ctx || job.key !== ctx.key)) job = null;
    if (ctx?.missing && !plans.get(planKey(ctx))) loadPlan(ctx).then(() => drawBar());
    drawBar();
}

function drawBar() {
    if (!box) return;
    const hit = ctx?.missing ? plans.get(planKey(ctx)) : null;
    const plan = hit?.plan;
    const size = plan ? selection(plan, DEFAULT_PICK(), null).bytes : undefined;
    const notOffered = !!plan && plan.files.length === 0 && plan.not_offered.length > 0;
    const m = barModel({
        missing: !!ctx?.missing, empty: !!ctx?.empty, label: ctx?.label || '', key: ctx?.key,
        size, notOffered, job, hidden,
    });
    box.textContent = '';
    if (!m) {
        box.className = 'filter-avail';
        box.style.display = 'none';
        popOpen = false;
        shownChanged(false);
        return;
    }
    box.className = 'filter-avail fa-compact'
        + (m.kind === 'dl' ? ' is-dl' : m.kind === 'ok' ? ' is-ok' : m.empty ? ' is-empty' : '');
    const text = document.createElement('span');
    text.className = 'fa-text';
    text.textContent = m.text + (m.sub ? ' ' : '');
    if (m.sub) {
        const s = document.createElement('span');
        s.className = 'fa-size';
        s.textContent = m.sub;
        text.appendChild(s);
    }
    box.appendChild(text);
    if (m.kind === 'dl') {
        box.appendChild(button('fa-btn ghost', 'Show', () => { hidden = false; openDialog(); }));
        const line = document.createElement('span');
        line.className = 'fa-line';
        line.innerHTML = `<i style="width:${m.pct}%"></i>`;
        box.appendChild(line);
    } else if (m.kind === 'missing') {
        if (m.download) box.appendChild(button('fa-btn', 'Download', () => openDialog('dl')));
        const info = button('fa-info' + (popOpen ? ' open' : ''), 'i', e => { e.stopPropagation(); togglePop(); });
        info.title = 'Download by hand';
        box.appendChild(info);
        if (popOpen) box.appendChild(popover(plan));
    }
    box.style.display = 'flex';
    shownChanged(true);
}

/// "Filters ready" stays on the bar a moment once the dialog is out of view,
/// then the bar goes (and the window gives its line back). Started once: a
/// dialog opened again meanwhile does not bring the line back.
function endReady() {
    if (!job || job.view !== 'done' || dlg || job.readyUntil != null) return;
    const done = job;
    done.readyUntil = Date.now() + READY_BAR_MS;
    clearTimeout(readyTimer);
    readyTimer = setTimeout(() => {
        readyTimer = null;
        if (job === done) job = null;
        drawBar();
    }, READY_BAR_MS);
}

/// The window fits its content: the bar coming or going changes the height.
function shownChanged(shown) {
    if (shown === barShown) return;
    barShown = shown;
    window.fitWindowToContent?.();
}

function button(cls, label, onClick) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = cls;
    b.textContent = label;
    b.addEventListener('click', onClick);
    return b;
}

function togglePop(force) {
    popOpen = force ?? !popOpen;
    drawBar();
}

let outsideWired = false;

/// A click anywhere else puts the popover away (wired on first use: the
/// module is also loaded by tests without a page).
function wireOutside() {
    if (outsideWired) return;
    outsideWired = true;
    document.addEventListener('click', e => {
        if (popOpen && box && !box.contains(e.target)) togglePop(false);
    });
}

/// The packs to get in the browser, and the real folder to extract them into.
function popover(plan) {
    const pop = document.createElement('div');
    pop.className = 'fa-pop';
    pop.addEventListener('click', e => e.stopPropagation());
    const packs = ctx?.packs || [];
    pop.innerHTML = '<div class="p-title">Download by hand</div>'
        + (packs.length
            ? '<div class="p-note">Get a pack in the browser and extract it into the filters folder.</div>'
            : '<div class="p-note">No pack has this filter. Generate it with fir-optimizer/optimize.py --all-ratios.</div>');
    for (const p of packs) {
        const a = document.createElement('a');
        a.textContent = p.name;
        a.title = p.url;
        a.addEventListener('click', () => { try { window.__TAURI__.shell.open(p.url); } catch (_) {} });
        pop.appendChild(a);
    }
    const dest = plan?.dest || inventory.dest;
    if (dest) {
        const d = document.createElement('div');
        d.className = 'p-path';
        d.innerHTML = 'Folder: <code></code>';
        d.querySelector('code').textContent = dest;
        pop.appendChild(d);
    }
    return pop;
}

// ── the dialog ─────────────────────────────────────────────────────────────

function modal() {
    let m = document.getElementById('fdModal');
    if (!m) {
        m = document.createElement('div');
        m.id = 'fdModal';
        m.innerHTML = '<div id="fdDialog" role="dialog" aria-modal="true"></div>';
        m.addEventListener('click', e => { if (e.target === m && closable()) closeDialog(); });
        document.body.appendChild(m);
        document.addEventListener('keydown', e => {
            if (e.key === 'Escape' && m.style.display === 'flex' && closable()) closeDialog();
        });
    }
    return m;
}

const closable = () => !job || !['dl'].includes(job.view);

/// Open on `tab` ('dl' or 'st'); a download under way shows itself instead.
export function openDialog(tab = 'dl') {
    if (!ctx && !job && tab === 'dl') return;
    popOpen = false;
    hidden = false;
    dlg = { tab, picked: DEFAULT_PICK(), showFiles: true, storage: null, trashNote: '' };
    modal().style.display = 'flex';
    if (job && job.view !== 'done') render();
    else if (tab === 'st') showStorage();
    else showPlan();
    drawBar();
}

function closeDialog() {
    const m = document.getElementById('fdModal');
    if (m) m.style.display = 'none';
    dlg = null;
    if (job && job.view === 'done') job.closed = true;
    else if (job && job.view !== 'dl') job = null;
    endReady();
    drawBar();
}

function hideDialog() {
    hidden = true;
    const m = document.getElementById('fdModal');
    if (m) m.style.display = 'none';
    dlg = null;
    drawBar();
}

async function showPlan(fresh = false) {
    dlg.tab = 'dl';
    dlg.loading = true;
    render();
    const r = await loadPlan(ctx, fresh);
    if (!dlg) return;
    dlg.loading = false;
    dlg.plan = r.plan || null;
    dlg.planError = r.error || '';
    render();
}

async function showStorage() {
    dlg.tab = 'st';
    dlg.storage = null;
    render();
    let s;
    try {
        s = await invoke('filters_storage');
    } catch (e) {
        s = { error: String(e) };
    }
    if (!dlg) return;
    dlg.storage = s;
    render();
}

function subLine() {
    const c = job || ctx;
    return `<div class="up-sub">${esc(c.tapsLabel)} taps · ${esc(fsText(c.fs))} · ${esc(phasesText(c.phases))}</div>`;
}

const tabs = on => `<div class="fd-tabs"><span data-tab="dl" class="${on === 'dl' ? 'on' : ''}">Download</span>`
    + `<span data-tab="st" class="${on === 'st' ? 'on' : ''}">Storage</span></div>`;

const GROUPS = [
    ['44k', '44.1k family', 'CD and most music files'],
    ['48k', '48k family', 'video, DVD, some streams'],
    ['hires', 'Hi-res sources (88.2k/96k and up)', 'only if you play hi-res files'],
];

function render() {
    const d = document.getElementById('fdDialog');
    if (!d) return;
    if (job && job.view !== 'done' && job.view !== 'cancelled' && dlg?.tab !== 'st') {
        paint(d, jobView());
    } else if (job && job.view === 'done' && !job.closed && dlg?.tab !== 'st') {
        paint(d, doneView());
    } else if (dlg?.tab === 'st') {
        paint(d, storageView());
    } else {
        paint(d, readyView());
    }
    wire(d);
}

/// The poll repaints the dialog four times a second; a button replaced between
/// press and release loses its click. So when the buttons come out the same,
/// the old ones stay and only the rest is swapped.
function paint(d, html) {
    const t = document.createElement('div');
    t.innerHTML = html;
    const oldB = d.querySelector('.fd-btns');
    const newB = t.querySelector('.fd-btns');
    if (oldB && newB && oldB.outerHTML === newB.outerHTML) {
        newB.replaceWith(oldB);
        d.replaceChildren(...t.childNodes);
    } else {
        d.innerHTML = html;
    }
}

function readyView() {
    const head = '<p class="up-title">Download filters</p>' + subLine() + tabs('dl');
    if (dlg.loading) return head + '<div class="fd-msg"><span class="dim">Checking the filter release…</span></div>'
        + '<div class="fd-btns"><button class="sec" data-act="close">Cancel</button></div>';
    if (dlg.planError || !dlg.plan) {
        return '<p class="up-title">Download filters</p>' + subLine() + tabs('dl')
            + `<div class="fd-msg err">The filter release cannot be reached.<br><span class="dim">${esc(dlg.planError)}</span></div>`
            + '<div class="fd-btns"><button class="txt" data-act="hand">Download by hand</button><span class="sp"></span>'
            + '<button class="sec" data-act="close">Close</button><button class="pri" data-act="replan">Try again</button></div>';
    }
    const plan = dlg.plan;
    const sizes = groupSizes(plan);
    const sel = selection(plan, dlg.picked, plan.free);
    const short = !sel.enough || dlg.spaceError;
    const rows = GROUPS.map(([g, name, hint]) => {
        const has = plan.files.some(f => f.group === g);
        const on = dlg.picked.has(g) && has;
        return `<div class="fd-row${on ? '' : ' off'}${has ? ' pick' : ''}" data-group="${g}" role="checkbox" aria-checked="${on}">`
            + `<span class="fd-chk${on ? ' on' : ''}">${on ? '✓' : ''}</span>`
            + `<span class="l">${name}<span class="hint">${has ? hint : 'nothing missing'}</span></span>`
            + `<span class="r">${has ? fmtBytes(sizes[g]) : '—'}</span></div>`;
    }).join('');
    const total = `<div class="fd-row total"><span class="l">To download · ${sel.files.length} file${sel.files.length === 1 ? '' : 's'}</span>`
        + `<span class="r">${fmtBytes(sel.bytes)}</span></div>`;
    const drive = driveOf(plan.dest);
    const free = plan.free == null ? '' : `<div class="fd-free"><span>Free on ${drive}</span>`
        + `<b class="${short ? 'bad' : ''}">${fmtBytes(plan.free)}</b></div>`;
    let msg = '<div class="fd-msg"><span class="dim">From the app\'s GitHub release, checked against its signed list.</span></div>';
    if (!plan.writable) {
        msg = `<div class="fd-msg err">The filters folder cannot be written:<br><span class="dim">${esc(plan.dest)}</span></div>`;
    } else if (short) {
        const freeNow = plan.free == null ? '' : `, free ${fmtBytes(plan.free)}`;
        msg = `<div class="fd-msg err">Need ${fmtBytes(sel.need)} on ${drive}${freeNow}.<br>`
            + '<span class="dim">Free some space, or untick a family. Storage shows files this version does not use.</span></div>';
    } else if (plan.files.length === 0) {
        msg = plan.not_offered.length
            ? '<div class="fd-msg warn">The filter release has no files for this setting.</div>'
            : '<div class="fd-msg ok">Nothing to download — the files are in place.</div>';
    }
    if (plan.shadowed?.length) {
        msg += `<div class="fd-msg warn">A damaged copy is found first and would still be used:<br>`
            + `<span class="dim">${plan.shadowed.map(esc).join('<br>')}</span></div>`;
    }
    const can = plan.writable && sel.files.length > 0 && !short;
    const title = short ? 'Not enough space' : 'Download filters';
    return `<p class="up-title">${title}</p>` + subLine() + tabs('dl')
        + `<div class="fd-box"><div class="fd-cap">For sources</div>${rows}${total}</div>` + free + msg
        + '<div class="fd-btns">' + (short ? '<button class="sec" data-act="storage">Storage</button><span class="sp"></span>' : '')
        + `<button class="${short ? 'txt' : 'sec'}" data-act="close">Cancel</button>`
        + `<button class="pri" data-act="start"${can ? '' : ' disabled'}>Download</button></div>`;
}

function progressBlock(barCls, right, nums) {
    const pct = percent(job.done, job.total);
    return '<div class="fd-prog">'
        + `<div class="top"><span>File ${job.file || 1} of ${job.files || job.names.length}</span><span>${right ?? pct + '%'}</span></div>`
        + `<div class="file">${esc(job.name || job.names[0] || '')}</div>`
        + `<div class="fd-bar ${barCls}"><i style="width:${pct}%"></i></div>`
        + (nums ? `<div class="nums"><span>${Math.round(job.done / 1e6)} / ${Math.round(job.total / 1e6)} MB</span><span>${nums}</span></div>` : '')
        + '</div>';
}

function jobView() {
    if (job.view === 'paused') {
        return '<p class="up-title">Download paused</p>' + subLine() + progressBlock('paused', null, 'connection lost')
            + '<div class="fd-msg warn">Paused — resumes where it stopped.'
            + (job.error ? `<br><span class="dim">${esc(job.error)}</span>` : '') + '</div>'
            + '<div class="fd-btns"><button class="txt" data-act="drop">Cancel</button><span class="sp"></span>'
            + '<button class="sec" data-act="hide">Hide</button><button class="pri" data-act="resume">Resume</button></div>';
    }
    if (job.view === 'failed' || job.view === 'nospace') {
        const kept = Math.max(0, (job.file || 1) - 1);
        const checksum = /checksum|damaged|larger than the list/i.test(job.error || '');
        const what = checksum
            ? 'The file did not match the signed list. It was removed — nothing was used.'
            : esc(job.error || 'The download stopped.');
        const keptLine = kept ? `The ${kept} file${kept === 1 ? '' : 's'} already checked ${kept === 1 ? 'is' : 'are'} kept. ` : '';
        return `<p class="up-title">${job.view === 'nospace' ? 'Not enough space' : 'Download failed'}</p>` + subLine()
            + progressBlock('failed', 'stopped', '')
            + `<div class="fd-msg err">${what}<br><span class="dim">${keptLine}Try again, or download by hand.</span></div>`
            + '<div class="fd-btns"><button class="txt" data-act="hand">Download by hand</button><span class="sp"></span>'
            + (job.view === 'nospace' ? '<button class="sec" data-act="storage">Storage</button>' : '<button class="sec" data-act="drop">Close</button>')
            + '<button class="pri" data-act="resume">Try again</button></div>';
    }
    const rate = rateOf(samples, Date.now());
    const nums = rate > 0 ? `${Math.round(rate / 1e6)} MB/s${job.eta ? ' · ' + job.eta : ''}` : (job.view === 'dl' && job.phase === 'checking' ? 'checking…' : '');
    return '<p class="up-title">Downloading filters</p>' + subLine() + progressBlock('', null, nums)
        + '<div class="fd-msg"><span class="dim">Hide keeps it going — the bar above the rack shows the progress.</span></div>'
        + '<div class="fd-btns"><button class="txt" data-act="cancel">Cancel</button><span class="sp"></span>'
        + '<button class="sec" data-act="hide">Hide</button></div>';
}

function doneView() {
    const total = job.files_list.reduce((a, f) => a + f.size, 0);
    const list = job.files_list.map(f => `<div><span>✓ ${esc(f.name)}</span><span>${fmtBytes(f.size)}</span></div>`).join('');
    return '<p class="up-title">Filters ready</p>' + subLine()
        + '<div class="fd-bar done"><i style="width:100%"></i></div>'
        + '<div class="fd-msg ok">Ready — the setting works now.</div>'
        + `<div class="fd-box"><div class="fd-cap">${job.files_list.length} file${job.files_list.length === 1 ? '' : 's'} · ${fmtBytes(total)} · signature OK</div>`
        + `<div class="fd-list">${list}</div></div>`
        + '<div class="fd-btns"><button class="pri" data-act="close">Close</button></div>';
}

function storageView() {
    const s = dlg.storage;
    const head = '<p class="up-title">Filters</p>'
        + `<div class="up-sub">${s?.dest ? 'Folder ' + esc(s.dest) : '&nbsp;'}</div>` + (ctx ? tabs('st') : '');
    const foot = '<div class="fd-btns"><button class="txt" data-act="folder">Open folder</button><span class="sp"></span>'
        + '<button class="sec" data-act="close">Close</button></div>';
    if (!s) return head + '<div class="fd-msg"><span class="dim">Reading the folder…</span></div>' + foot;
    if (s.error) return head + `<div class="fd-msg err">${esc(s.error)}</div>` + foot;
    const m = storageModel(s.items);
    const all = m.used + m.unusedBytes || 1;
    const bar = m.rungs.map(r => `<i style="width:${r.bytes / all * 100}%;background:${r.color}"></i>`).join('')
        + (m.unusedBytes ? `<i class="fd-unused-fill" style="width:${m.unusedBytes / all * 100}%"></i>` : '');
    const rungs = m.rungs.map(r => `<div class="fd-row"><span class="fd-dot" style="background:${r.color}"></span>`
        + `<span class="l">${r.label}</span><span class="r fd-n">${r.count} file${r.count === 1 ? '' : 's'}</span>`
        + `<span class="r fd-sz">${fmtBytes(r.bytes)}</span></div>`).join('');
    const used = '<div class="fd-box">'
        + `<div class="fd-row total fd-head"><span class="l">Used by filters · ${m.usedCount} file${m.usedCount === 1 ? '' : 's'}</span>`
        + `<span class="r">${fmtBytes(m.used)}</span></div>`
        + `<div class="fd-usebar">${bar}</div>${rungs || '<div class="fd-row"><span class="l">No filter files yet.</span></div>'}</div>`;
    let unused = '';
    if (m.unused.length) {
        const busy = job && job.view === 'dl';
        const list = dlg.showFiles
            ? '<div class="fd-list">' + m.unused.map(it => `<div><span>${esc(it.name)}</span><span>${fmtBytes(it.size)}</span></div>`).join('')
              + '<div class="fd-why">sizes this version does not have · unfinished downloads</div></div>'
            : '';
        unused = '<div class="fd-box fd-unused">'
            + '<div class="fd-row"><span class="fd-dot fd-dot-unused"></span><span class="l fd-unused-title">Not used by this version'
            + `<span class="hint">${m.unused.length} file${m.unused.length === 1 ? '' : 's'} · ${fmtBytes(m.unusedBytes)}</span></span>`
            + `<button class="fa-btn fd-trash" data-act="trash"${busy ? ' disabled title="Filters are downloading"' : ''}>Move to Recycle Bin</button></div>`
            + `<div class="fd-exp" data-act="files">${dlg.showFiles ? '▾ Hide files' : '▸ Show files'}</div>${list}</div>`;
    }
    const note = dlg.trashNote
        ? `<div class="fd-msg ${dlg.trashErr ? 'err' : 'ok'}">${esc(dlg.trashNote)}</div>`
        : (m.unused.length
            ? '<div class="fd-msg"><span class="dim">Nothing is deleted until you press the button. Files go to the Recycle Bin — you can bring them back.</span></div>'
            : '');
    return head + used + unused + note + foot;
}

function wire(d) {
    d.querySelectorAll('[data-tab]').forEach(t => t.addEventListener('click', () => {
        if (t.dataset.tab === 'st') showStorage(); else showPlan();
    }));
    d.querySelectorAll('.fd-row.pick').forEach(r => r.addEventListener('click', () => {
        const g = r.dataset.group;
        if (dlg.picked.has(g)) dlg.picked.delete(g); else dlg.picked.add(g);
        dlg.spaceError = false;
        render();
    }));
    d.querySelectorAll('[data-act]').forEach(b => {
        if (b.wired) return; // kept across a repaint, already listening
        b.wired = true;
        b.addEventListener('click', () => act(b.dataset.act));
    });
}

function act(a) {
    switch (a) {
        case 'close': closeDialog(); break;
        case 'hide': hideDialog(); break;
        case 'start': start(selection(dlg.plan, dlg.picked, dlg.plan.free)); break;
        case 'replan': showPlan(true); break;
        case 'resume': restart(); break;
        case 'cancel': invoke('filters_cancel').catch(() => {}); break;
        case 'drop': job = null; closeDialog(); break;
        case 'storage': if (job && job.view !== 'dl') job = null; showStorage(); break;
        case 'hand': closeDialog(); popOpen = true; drawBar(); break;
        case 'files': dlg.showFiles = !dlg.showFiles; render(); break;
        case 'trash': trash(); break;
        case 'folder': invoke('filters_dir_open').catch(() => {}); break;
    }
}

// ── the download ───────────────────────────────────────────────────────────

function start(sel) {
    if (!sel.names.length) return;
    job = {
        key: ctx.key, taps: ctx.taps, tapsLabel: ctx.tapsLabel, fs: ctx.fs, phases: ctx.phases, label: ctx.label,
        names: sel.names, files_list: sel.files.map(f => ({ name: f.name, size: f.size })),
        view: 'dl', phase: 'checking', file: 0, files: sel.names.length, name: '', done: 0, total: sel.bytes, error: '',
    };
    restart();
}

async function restart() {
    if (!job) return;
    job.view = 'dl';
    job.phase = 'checking';
    job.error = '';
    samples = [];
    render();
    try {
        await invoke('filters_download', { names: job.names });
    } catch (e) {
        job.view = 'failed';
        job.error = String(e);
        render();
        return;
    }
    stopPoll();
    pollTimer = setInterval(poll, POLL_MS);
}

function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
}

async function poll() {
    let st;
    try {
        st = await (await fetch(auraUrl('/filters/status'))).json();
    } catch (_) { return; }
    if (!job) { stopPoll(); return; }
    const view = viewOf(st);
    job.phase = st.phase;
    if (st.total) { job.total = st.total; job.done = st.done; job.file = st.file; job.files = st.files; job.name = st.name; }
    const now = Date.now();
    samples.push({ t: now, done: job.done });
    samples = samples.filter(s => now - s.t <= 6000);
    job.eta = etaText(job.total - job.done, rateOf(samples, now));
    if (view === 'dl') {
        if (hidden) drawBar(); else render();
        return;
    }
    stopPoll();
    replan();
    if (view === 'cancelled') {
        job = null;
        if (hidden) { hidden = false; drawBar(); } else if (dlg) showPlan(true);
        return;
    }
    job.error = st.error || '';
    job.view = view === 'idle' ? 'failed' : view;
    if (view === 'done') {
        await loadFilterInventory();
        onChanged();
    }
    if (view !== 'done' && hidden) {
        // A stop the user has to answer comes back into view.
        hidden = false;
        modal().style.display = 'flex';
        dlg = { tab: 'dl', picked: DEFAULT_PICK(), showFiles: true };
    }
    hidden = false;
    // Finished while hidden: nobody is looking at the dialog.
    endReady();
    drawBar();
    if (dlg) render();
}

// ── storage ────────────────────────────────────────────────────────────────

async function trash() {
    const s = dlg?.storage;
    if (!s?.items) return;
    const names = storageModel(s.items).unused.map(it => it.name);
    if (!names.length) return;
    let res;
    try {
        res = await invoke('filters_trash', { names });
    } catch (e) {
        dlg.trashNote = String(e);
        dlg.trashErr = true;
        render();
        return;
    }
    const failed = res.filter(r => !r.ok);
    const moved = res.length - failed.length;
    const note = `Moved ${moved} file${moved === 1 ? '' : 's'} to the Recycle Bin.`
        + (failed.length ? ` ${failed.length} could not be moved: ${failed.map(r => r.name + (r.error ? ' (' + r.error + ')' : '')).join(', ')}` : '');
    await loadFilterInventory();
    onChanged();
    replan();
    await showStorage();
    if (dlg) { dlg.trashNote = note; dlg.trashErr = failed.length > 0; render(); }
}
