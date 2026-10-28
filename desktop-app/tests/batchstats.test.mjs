// How fast a conversion went (batchstats.js), for the session's log.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { batchReset, batchStarted, batchPolled, batchSummary, speedTag } from '../src/js/batchstats.js';
import { pushConvRecord } from '../src/js/library.js';
import { state } from '../src/js/state.js';

test('a finished batch says itself in the log: the batch, then each file with its own time', () => {
    batchReset();
    const recs = [{ name: 'a.flac', path: 'x/a.flac', status: 'done' }, { name: 'b.flac', path: 'x/b.flac', status: 'done' }];
    batchStarted(recs);
    const t0 = performance.now() - 10000;
    Object.assign(recs[0], { durS: 120, tActive: t0, tDone: t0 + 10000 });
    Object.assign(recs[1], { durS: 60, tActive: t0, tDone: t0 + 5000 });
    const s = batchSummary();
    assert.match(s.log[0], /^Conversion done: 2 files · .+ of audio in .+ · ×[\d.]+ real time$/);
    assert.equal(s.log[0], 'Conversion done: ' + s.text.replace(/^✓ /, ''), 'the status line\'s words');
    assert.equal(s.log.length, 3, 'one line per file');
    assert.ok(s.log[1].startsWith('  b.flac — ') && s.log[1].endsWith('×12.0'), s.log[1]);
    assert.ok(s.log[2].startsWith('  a.flac — ') && s.log[2].endsWith('×12.0'), s.log[2]);
    batchReset();
    assert.equal(batchSummary(), null, 'nothing finished: nothing said');
});

test('a finished file says its speed beside its check mark: ×N, and in words on hover', () => {
    assert.deepEqual(speedTag(25.31), { text: '×25.3', title: '25.3× faster than real time' });
    assert.deepEqual(speedTag(123.4), { text: '×123', title: '123× faster than real time' });
    assert.deepEqual(speedTag(0.5), { text: '×0.5', title: '0.5× faster than real time' });
    for (const x of [null, undefined, 0, -3, NaN, Infinity]) assert.equal(speedTag(x), null, String(x));
});

test('a file knows its own speed the moment it is seen done, the same figure as its line in the log', () => {
    batchReset();
    state.list = [{ path: 'x/a.flac', info: { durationS: 120 } }, { path: 'x/b.flac', info: null }];
    const recs = [{ name: 'a.flac', path: 'x/a.flac', status: 'active' }, { name: 'b.flac', path: 'x/b.flac', status: 'active' }];
    batchStarted(recs);
    batchPolled();
    assert.equal(recs[0].speedX, undefined, 'converting: no speed yet');
    recs[0].tActive = recs[1].tActive = performance.now() - 10000;
    recs[0].status = recs[1].status = 'done';
    batchPolled();
    assert.ok(Math.abs(recs[0].speedX - 12) < 0.05, `120 s in 10 s: ${recs[0].speedX}`);
    assert.equal(speedTag(recs[0].speedX).text, '×12.0');
    assert.equal(recs[1].speedX, null, 'no duration known: no speed');
    const s = batchSummary();
    const line = s.log.find(l => l.startsWith('  a.flac — '));
    assert.ok(line.endsWith(speedTag(recs[0].speedX).text), line);
    batchReset();
    state.list = [];
});

test('the record of a converted file keeps how fast it was made, when that is known', () => {
    const e = { path: 'x/a.flac', convs: [] };
    pushConvRecord(e, { outPath: 'x/a [1M].flac', sig: 's1', speedX: 12.5 });
    assert.equal(e.convs[0].speedX, 12.5);
    pushConvRecord(e, { outPath: 'x/a [5M].flac', sig: 's2', speedX: null });
    assert.ok(!('speedX' in e.convs[1]), 'no speed: no field');
});

test('the result\'s tip names the first twelve files and says how many more; the log has them all', () => {
    batchReset();
    const recs = Array.from({ length: 15 }, (_, i) => ({ name: `f${i}.flac`, path: `x/f${i}.flac`, status: 'done' }));
    batchStarted(recs);
    const t0 = performance.now() - 10000;
    recs.forEach((f, i) => Object.assign(f, { durS: 60, tActive: t0, tDone: t0 + 1000 + i }));
    const s = batchSummary();
    const tipFiles = s.title.split('\n').filter(l => /^f\d+\.flac — /.test(l));
    assert.equal(tipFiles.length, 12);
    assert.ok(s.title.endsWith('\n…and 3 more'), s.title.slice(-40));
    assert.equal(s.log.length, 16, 'the batch\'s line and all fifteen files');
    batchReset();
});
