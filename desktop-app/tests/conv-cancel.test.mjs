// Cancelling a conversion (the Hold to Cancel strip, the Convert all button's
// hold, a row's ✕) and converting again: the list's batch state against a
// stand-in backend that behaves like manager.rs — a cancel is seen at the
// next check, so the file goes on for a while; the files end before the
// batch thread does; the player writes the same status line.
//
// Anton 20.10: cancel → the row kept a progress bar → play → C → "→ next"
// for good. The C came while the cancelled batch was still stopping; the
// end of that batch threw the next queue away and left the row as it was.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

// ── a page: every element asked for exists and keeps what is written ──
function el(id) {
    const classes = new Set();
    const attrs = {};
    return {
        id, style: {}, dataset: {}, checked: false, value: '', textContent: '', innerHTML: '', title: '',
        className: '', disabled: false, children: [],
        classList: {
            add: c => classes.add(c), remove: c => classes.delete(c), contains: c => classes.has(c),
            toggle: (c, on) => ((on ?? !classes.has(c)) ? classes.add(c) : classes.delete(c)),
        },
        setAttribute: (k, v) => { attrs[k] = String(v); }, getAttribute: k => attrs[k] ?? null,
        removeAttribute: k => { delete attrs[k]; }, hasAttribute: k => k in attrs,
        addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
        querySelector: sel => el(sel), querySelectorAll: () => [],
        appendChild(c) { this.children.push(c); return c; },
        insertBefore(c) { return c; }, replaceWith() {}, remove() {},
        parentNode: { insertBefore: c => c },
        getBoundingClientRect: () => ({ left: 0, top: 0, width: 0, height: 0, right: 0, bottom: 0 }),
    };
}
const els = new Map();
const byId = id => { if (!els.has(id)) els.set(id, el(id)); return els.get(id); };
globalThis.document = {
    getElementById: byId,
    querySelector: () => null, querySelectorAll: () => [],
    createElement: tag => el(tag), addEventListener() {}, removeEventListener() {},
    body: el('body'), documentElement: el('html'),
};
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.requestAnimationFrame = () => 0;
globalThis.cancelAnimationFrame = () => {};
globalThis.getComputedStyle = () => ({ getPropertyValue: () => '', paddingTop: '0', paddingBottom: '0', rowGap: '0', height: '0' });
if (!globalThis.CustomEvent) globalThis.CustomEvent = class { constructor(t, o) { this.type = t; this.detail = o?.detail; } };

// ── the backend (manager.rs as the page sees it) ──
const STAGE = { pending: 0, active: 2, done: 4, error: 5, cancelled: 6 };
const be = {
    running: false, files: [], status: '', cancel: false, rev: 0, starts: [], calls: [],
    workers: 1,         // files converting at once (the batch's GPU/CPU workers)
    tail: 0,            // polls the active file goes on after a cancel (stages that do not look)
    windDown: 0,        // polls between the last file ending and the thread ending
    playerStatus: null, // the player's own line in the same status
    hold: null,         // convert_files waits for this before it takes the batch
};
const TAIL = 3;
function bump(f, stage) { f.stage = stage; f.rev = ++be.rev; }
function backendTick() {
    if (!be.running) return;
    const active = be.files.filter(f => f.stage === STAGE.active);
    if (be.cancel) {
        if (active.length && be.tail-- > 0) { for (const f of active) f.pct = Math.min(990, f.pct + 100); return; }
        for (const f of be.files) if (f.stage < STAGE.done) bump(f, STAGE.cancelled);
    }
    // ✕ on a running file raises only its own flag: the files beside it go on.
    const dropped = active.filter(f => f.cancelled);
    if (dropped.length) { for (const f of dropped) bump(f, STAGE.cancelled); return; }
    if (be.files.every(f => f.stage >= STAGE.done)) {
        if (be.windDown-- > 0) return;
        const n = be.files.filter(f => f.stage === STAGE.done).length;
        be.status = be.cancel ? 'Cancelled by user' : `✓ All done! ${n} files converted`;
        be.running = false;
        return;
    }
    if (active.length < be.workers) {
        const next = be.files.find(f => f.stage === STAGE.pending);
        if (next) {
            if (next.cancelled) { bump(next, STAGE.cancelled); return; }
            bump(next, STAGE.active);
            be.status = `[${be.files.indexOf(next) + 1}/${be.files.length}] Processing...`;
            if (!active.length) return;
        }
    }
    for (const f of active) {
        f.pct = Math.min(1000, f.pct + 500);
        if (f.pct >= 1000) { f.output = f.path.replace(/\.\w+$/, ' [AE].flac'); bump(f, STAGE.done); }
    }
}
const invoke = async (cmd, a) => {
    switch (cmd) {
        case 'check_filters': return JSON.stringify({ ok: true });
        case 'gpu_status': return JSON.stringify({ available: true });
        case 'convert_files':
            if (be.hold) await be.hold;
            if (be.running) throw 'Conversion already in progress';
            // convert_batch clears the cancel flags as it takes the batch.
            be.running = true; be.cancel = false; be.rev = 0; be.tail = TAIL; be.windDown = 1;
            be.files = a.paths.map(p => ({ path: p, stage: STAGE.pending, pct: 0, rev: 0, cancelled: false }));
            be.status = 'Preparing…';
            be.starts.push([...a.paths]);
            return null;
        case 'cancel_conversion': be.calls.push(cmd); be.cancel = true; return null;
        case 'cancel_file': {
            be.calls.push(cmd);
            const f = be.files[a.idx];
            if (f) f.cancelled = true;
            return null;
        }
        case 'get_conversion_progress': {
            backendTick();
            const total = be.files.length;
            const done = be.files.filter(f => f.stage >= STAGE.done).length;
            const progress = total ? Math.floor(be.files.reduce((s, f) => s + (f.stage >= STAGE.done ? 1000 : f.pct), 0) / total) : 0;
            return [progress, total, done, be.playerStatus ?? be.status, '', 0, be.running, be.cancel];
        }
        case 'get_queue_status': {
            const files = be.files.map((f, idx) => ({ f, idx }))
                .filter(({ f }) => a.knownRev === 0 || f.rev > a.knownRev || f.stage === STAGE.active)
                .map(({ f, idx }) => ({ idx, stage: f.stage, pct: f.stage === STAGE.done ? 1000 : (f.stage === STAGE.active ? f.pct : 0), badge: 0, output: f.output }));
            return JSON.stringify({ rev: be.rev, files });
        }
        default: return null;
    }
};
globalThis.window = {
    __TAURI__: { core: { invoke }, event: { listen: async () => () => {} }, dialog: { ask: async () => true } },
    addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: false, addEventListener() {} }),
};

const { state } = await import('../src/js/state.js');
const conv = await import('../src/js/converter.js');
const dz = await import('../src/js/dropzone.js');

// ── driving it: the page's 200 ms poll, by hand ──
const settle = async () => { for (let i = 0; i < 5; i++) await new Promise(r => setImmediate(r)); };
async function poll(n = 1) {
    for (let i = 0; i < n; i++) { if (state.convPollTimer) await conv.pollConversionProgress(); await settle(); }
}
async function untilIdle(max = 60) {
    for (let i = 0; i < max && (state.convIsConverting || state.convPollTimer); i++) await poll();
    assert.equal(state.convIsConverting, false, 'the page is idle again');
    assert.equal(be.running, false, 'the backend is idle again');
}
function fresh(names) {
    state.list = [];
    state.convFileQueue = []; state.convNextQueue = []; state.convMPendingGroups = [];
    state.convAllResult = null;
    be.starts = []; be.calls = []; be.playerStatus = null; be.workers = 1;
    return dz.addFiles(names.map(n => `E:\\MUSIC\\${n}.mp3`));
}
const rowHtml = () => byId('fileQueue').innerHTML;
/** The row as drawn: its record's state and what the page shows for it. */
function row(e) {
    const html = rowHtml().split('data-id="').find(s => s.startsWith(`${e.id}"`)) || '';
    return {
        next: html.includes('&rarr; next'),
        hasC: html.includes('data-act="convert"'),
        pct: (html.match(/file-item-progress" style="width:([\d.]+)%/) || [])[1],
        live: dz.convLive(e.conv),
    };
}

test('a: one file — Hold to Cancel, then C on it again: it starts and finishes', async () => {
    const [e] = fresh(['Criminal']);
    await dz.convertEntries([e]);
    await poll(2);
    assert.equal(e.conv.status, 'active');
    assert.notEqual(row(e).pct, '0.0', 'converting: the row shows how far');
    await dz.cancelBatch();
    assert.equal(e.conv.status, 'cancelled');
    assert.equal(row(e).pct, '0.0', 'no progress left on the cancelled row');
    assert.equal(state.convIsConverting, true, 'converting until the backend has stopped');
    await untilIdle();
    assert.equal(e.conv.status, 'cancelled');
    assert.deepEqual(row(e), { next: false, hasC: true, pct: '0.0', live: false });

    await dz.convertEntries([e]);
    assert.equal(be.starts.length, 2, 'a new batch, not a wait for one');
    await untilIdle();
    assert.equal(e.conv.status, 'done');
    assert.equal(e.conv.outPath, 'E:\\MUSIC\\Criminal [AE].flac');
});

test("d: Anton's order — cancel, play the row, C while the cancelled batch still stops: it runs after", async () => {
    const [e] = fresh(['Criminal']);
    await dz.convertEntries([e]);
    await poll(2);
    await dz.cancelBatch();
    await poll(1);
    // The player plays the row and writes its own line into the status.
    dz.listHooks.nowPlaying = () => ({ id: e.trackId, state: 'playing' });
    be.playerStatus = 'Decoding: Criminal.mp3';
    await dz.convertEntries([e]);
    assert.equal(row(e).next, true, 'waits as next while the cancelled batch stops');
    await poll(TAIL + 2);
    // The cancelled batch is over: the C is its own batch now, not lost.
    assert.equal(be.starts.length, 2, 'the C pressed during the stop started a batch');
    assert.equal(row(e).next, false);
    await untilIdle();
    assert.equal(e.conv.status, 'done');
    dz.listHooks.nowPlaying = () => ({ id: null, state: 'stopped' });
});

test('c: three files — cancel during the 1st: all three cancelled; then one, then all again', async () => {
    const es = fresh(['A', 'B', 'C']);
    await dz.convertEntries(es);
    await poll(2);
    assert.equal(es[0].conv.status, 'active');
    await dz.cancelBatch();
    await untilIdle();
    assert.deepEqual(es.map(e => e.conv.status), ['cancelled', 'cancelled', 'cancelled']);
    assert.ok(es.every(e => !row(e).live && row(e).hasC && row(e).pct === '0.0'));

    await dz.convertEntries([es[1]]);
    await untilIdle();
    assert.equal(es[1].conv.status, 'done');
    await dz.convertEntries([es[0], es[2]]);
    await untilIdle();
    assert.deepEqual(es.map(e => e.conv.status), ['done', 'done', 'done']);
});

test('c: three files — cancel during the 2nd: the 1st stays done, the rest cancelled', async () => {
    const es = fresh(['A', 'B', 'C']);
    await dz.convertEntries(es);
    for (let i = 0; i < 10 && es[1].conv.status !== 'active'; i++) await poll();
    assert.equal(es[0].conv.status, 'done');
    assert.equal(es[1].conv.status, 'active');
    await dz.cancelBatch();
    await untilIdle();
    assert.deepEqual(es.map(e => e.conv.status), ['done', 'cancelled', 'cancelled']);
    assert.equal(state.convAllResult, null, 'no result for a cancelled batch');
});

test('e: cancel while the backend is still taking the batch: the cancel is not lost', async () => {
    const [e] = fresh(['Early']);
    let release;
    be.hold = new Promise(r => { release = r; });
    const started = dz.convertEntries([e]);
    await settle();
    await dz.cancelBatch();         // before convert_batch cleared the flag
    release(); be.hold = null;
    await started;
    await untilIdle();
    assert.equal(e.conv.status, 'cancelled');
    assert.equal(be.status, 'Cancelled by user');
});

test('e: cancel after the last file ended but before the batch thread did: the file stays done', async () => {
    const [e] = fresh(['Late']);
    await dz.convertEntries([e]);
    for (let i = 0; i < 10 && e.conv.status !== 'done'; i++) await poll();
    assert.equal(e.conv.status, 'done');
    assert.equal(state.convIsConverting, true, 'the thread still winds down');
    await dz.cancelBatch();
    await untilIdle();
    assert.equal(e.conv.status, 'done');
});

test('f: ✕ on the converting row of two: it goes, the other converts', async () => {
    const [a, b] = fresh(['A', 'B']);
    await dz.convertEntries([a, b]);
    await poll(2);
    assert.equal(a.conv.status, 'active');
    await dz.removeEntry(a.id);
    assert.ok(!state.list.includes(a));
    await untilIdle();
    assert.equal(b.conv.status, 'done');
});

test('f: ✕ on one of two files converting at once: only it stops, the other finishes ✓', async () => {
    const [a, b, c] = fresh(['A', 'B', 'C']);
    be.workers = 2;
    await dz.convertEntries([a, b, c]);
    await poll(2);
    assert.equal(a.conv.status, 'active');
    assert.equal(b.conv.status, 'active', 'both convert at once');
    await dz.removeEntry(a.id);
    assert.deepEqual(be.calls, ['cancel_file'], 'the ✕ cancels its own file, not the batch');
    await untilIdle();
    assert.equal(b.conv.status, 'done', 'the file beside it was not stopped');
    assert.equal(c.conv.status, 'done', 'the one after it still runs');
    assert.equal(be.files[0].stage, STAGE.cancelled);
    assert.match(be.status, /All done! 2 files converted/);
});

test('f: a file added and C pressed mid-batch waits as next; Hold to Cancel cancels it too', async () => {
    const [a] = fresh(['A']);
    await dz.convertEntries([a]);
    await poll(2);
    const [b] = dz.addFiles(['E:\\MUSIC\\B.mp3']);
    await dz.convertEntries([b]);
    assert.equal(row(b).next, true);
    await dz.cancelBatch();
    await untilIdle();
    assert.equal(be.starts.length, 1, 'what waited was cancelled with the batch');
    assert.deepEqual(row(b), { next: false, hasC: true, pct: '0.0', live: false });
    await dz.convertEntries([b]);
    await untilIdle();
    assert.equal(b.conv.status, 'done');
});

test('h: a finished batch ends on the backend thread, not on the status line the player overwrote', async () => {
    const [e] = fresh(['Played']);
    await dz.convertEntries([e]);
    be.playerStatus = 'Decoding: Played.mp3';
    await untilIdle();
    assert.equal(e.conv.status, 'done');
    assert.ok(state.convAllResult, 'the result is shown');
});

test('a throw while the batch ends still leaves the page idle', async () => {
    const [e] = fresh(['Throws']);
    await dz.convertEntries([e]);
    await poll(2);
    await dz.cancelBatch();
    const real = dz.listHooks.nowPlaying;
    dz.listHooks.nowPlaying = () => { throw new Error('a row hook failed'); };
    try {
        for (let i = 0; i < 20 && be.running; i++) await poll();
        await poll();
    } finally { dz.listHooks.nowPlaying = real; }
    assert.equal(state.convIsConverting, false);
    assert.equal(state.convPollTimer, null);
    await dz.convertEntries([e]);
    assert.equal(be.starts.length, 2, 'C starts a batch');
    await untilIdle();
});
