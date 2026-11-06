// HP unit background trace (AURA_HP_TRACE=1): one line per event, a summary
// every TRACE_SUMMARY_MS, nothing for even motion.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { makeTrace, watchFrames, TRACE_SUMMARY_MS } from '../src/js/hptrace.js';
import { viewClock } from '../src/js/hpview.js';

/// Frames every 6 ms moving `v` px/ms from `t0` for `ms`, the picture at
/// `p0`; returns where it ended.
function run(tr, t0, ms, p0 = 0, v = 0.08) {
    let p = p0;
    for (let t = t0; t <= t0 + ms; t += 6) { tr.frame(t, p); p += v * 6; }
    return p;
}

test('even motion logs nothing but the summary', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    run(tr, 0, TRACE_SUMMARY_MS + 100);
    assert.equal(lines.length, 1, lines.join('\n'));
    assert.match(lines[0], /^10s: frames \d+ slow 0 gaps 0 .* jerks 0 /);
});

test('a step off the motion is one jerk line; a long frame is a gap', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    const p = run(tr, 0, 1000);            // last frame at 996 ms; p is where 1002 ms's goes
    tr.frame(1002, p + 4);                 // 4 px ahead of where it should be
    tr.frame(1056, p + 4 + 0.08 * 54);     // a 54 ms frame, moving evenly
    assert.equal(lines.filter(l => l.startsWith('jerk ')).length, 1, lines.join('\n'));
    assert.match(lines.find(l => l.startsWith('jerk ')), /^jerk 4px frame 6ms ans - ago$/);
    assert.equal(lines.filter(l => l.startsWith('gap 54ms')).length, 1, lines.join('\n'));
});

test('a frame of a few missed vsyncs is slow, not a gap; the summary counts it', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    const p = run(tr, 0, 1000);
    tr.frame(1022, p + 0.08 * 26);         // a 26 ms frame, moving evenly
    run(tr, 1028, TRACE_SUMMARY_MS, p + 0.08 * 32);
    assert.deepEqual(lines.filter(l => !l.startsWith('10s:')), ['slow 26ms']);
    assert.match(lines.find(l => l.startsWith('10s:')), /^10s: frames \d+ slow 1 gaps 0 /);
});

test('a snap of the clock is logged, and its jump is not a jerk', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    const p = run(tr, 0, 1000);
    tr.clock('snap', 'answer 3ms');
    tr.frame(1006, p - 40);
    run(tr, 1012, 300, p - 40 + 0.48);
    assert.deepEqual(lines, ['clock snap answer 3ms']);
});

test('late, empty and failed answers each say so; a quick good one says nothing', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    tr.frame(0, 0);
    tr.answer(2, 'ok', 2);
    tr.answer(3, 'kept', 5);
    tr.answer(400, 'ok', 410);
    tr.answer(5, 'failed', 420);
    assert.deepEqual(lines, ['answer kept 3ms', 'answer late 400ms ok', 'answer failed 5ms']);
});

test('the clock tells the trace when it holds, jumps and goes on', () => {
    const clk = { key: null }, f = (nowBin, frac = 0) => ({ nowBin, frac });
    assert.equal(viewClock(clk, f(100), 0, 1, 10), 'start');
    assert.equal(viewClock(clk, f(110), 100, 101, 10), undefined);
    assert.equal(viewClock(clk, f(110), 300, 301, 10), 'hold');
    assert.equal(viewClock(clk, f(110), 400, 401, 10), undefined);
    assert.equal(viewClock(clk, f(111), 500, 501, 10), 'on');
    assert.equal(viewClock(clk, f(500), 600, 601, 10), 'snap');
});

test('long frames and long tasks go to the log with their scripts', () => {
    const made = [];
    class PO {
        static supportedEntryTypes = ['long-animation-frame', 'longtask'];
        constructor(cb) { this.cb = cb; this.on = true; made.push(this); }
        observe({ type }) { this.type = type; }
        disconnect() { this.on = false; }
    }
    const lines = [];
    const stop = watchFrames((m) => lines.push(m), PO, () => 1000);
    const loaf = made.find((o) => o.type === 'long-animation-frame');
    const task = made.find((o) => o.type === 'longtask');
    loaf.cb({ getEntries: () => [
        { startTime: 900, duration: 12 },
        { startTime: 900, duration: 34.4, blockingDuration: 0, renderStart: 910, scripts: [
            { duration: 2, invoker: 'FrameRequestCallback', sourceURL: 'x/hpview.js', sourceFunctionName: 'draw' },
            { duration: 18.6, invoker: 'FrameRequestCallback', sourceURL: 'http://aura/js/player.js', sourceFunctionName: 'lampStep' },
        ] },
    ] });
    task.cb({ getEntries: () => [{ startTime: 800, duration: 61 }] });
    assert.deepEqual(lines, [
        'loaf 34ms at 900 block 0ms render 24ms seen 66ms after scripts 19ms FrameRequestCallback player.js:lampStep',
        'longtask 61ms at 800 seen 139ms after',
    ]);
    stop();
    assert.ok(made.every((o) => !o.on));
});

test('a page without long-frame observers says so once', () => {
    class PO {
        static supportedEntryTypes = ['longtask'];
        constructor() {}
        observe() {}
        disconnect() {}
    }
    const lines = [];
    watchFrames((m) => lines.push(m), PO, () => 0);
    assert.deepEqual(lines, ['no long-animation-frame']);
    const none = [];
    watchFrames((m) => none.push(m), null)();
    assert.deepEqual(none, ['no PerformanceObserver']);
});

test('jerks in a row at one new speed are learned once, not logged every frame', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    const p = run(tr, 0, 1000);
    run(tr, 1002, 2000, p, -0.46);
    assert.equal(lines.filter(l => l.startsWith('jerk ')).length, 5, lines.join('\n'));
    assert.deepEqual(lines.filter(l => l.startsWith('speed ')), ['speed 80 -> -460px/s']);
});

test("the summary counts the strip's steps in screen px", () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    // 1.25 px a frame put on whole px: 1, 1, 1, 2, ... — the mean stays 1.25.
    for (let i = 0, t = 0; t <= TRACE_SUMMARY_MS + 6; i++, t += 6) tr.frame(t, i * 1.25, -Math.round(i * 1.25));
    const sum = lines.find(l => l.startsWith('10s:'));
    const m = sum.match(/steps 0:(\d+) 1:(\d+) 2:(\d+) 3\+:(\d+) mean ([\d.]+)px changes (\d+)/);
    assert.ok(m, sum);
    const [z, one, two, more, mean] = m.slice(1, 6).map(Number);
    assert.equal(z, 0); assert.equal(more, 0);
    assert.ok(Math.abs(one / two - 3) < 0.05, sum);
    assert.ok(Math.abs(mean - 1.25) < 0.01, sum);
});

test('without the strip x the summary says nothing of steps', () => {
    const lines = [];
    const tr = makeTrace((m) => lines.push(m));
    run(tr, 0, TRACE_SUMMARY_MS + 100);
    assert.doesNotMatch(lines[0], /steps/);
});
