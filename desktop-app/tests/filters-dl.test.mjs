// The filter download's decisions (src/js/filters-dl-core.js): what the bar
// says, which files a ticked set of groups fetches, what a status means for the
// dialog, and which files the storage view may move to the Recycle Bin.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    fmtBytes, spaceNeeded, driveOf, fsText, phasesFor, phasesText, groupSizes, selection,
    classifyError, viewOf, rateOf, etaText, percent, barModel, barLabel, isUnused, storageModel
} from '../src/js/filters-dl-core.js';

const GB = 1e9, MB = 1e6;

const PLAN = {
    files: [
        { name: 'fir_1G_705600_linear_phase.b0.npy', size: 500 * MB, part: 0, group: '44k' },
        { name: 'fir_1G_705600_linear_phase.b0.ends.npy', size: 500 * MB, part: 100 * MB, group: '44k' },
        { name: 'fir_1G_768000_linear_phase.b0.npy', size: 1 * GB, part: 0, group: '48k' },
        { name: 'fir_1G_352800_linear_phase.b0.npy', size: 2 * GB, part: 0, group: 'hires' },
    ],
    not_offered: [],
    free: 412 * GB,
    dest: 'D:\\AuraEngine\\filters',
};

test('sizes read in decimal units like the drive', () => {
    assert.equal(fmtBytes(2 * GB), '2.0 GB');
    assert.equal(fmtBytes(760 * MB), '760 MB');
    assert.equal(fmtBytes(412.3 * GB), '412 GB');
    assert.equal(fmtBytes(0), '0 B');
    assert.equal(fsText(16), 'FS16 (705.6k / 768k)');
    assert.equal(driveOf('D:\\AuraEngine\\filters'), 'drive D:');
    assert.equal(driveOf('/home/a/filters'), 'disk');
});

test('the space margin matches the backend: 2 % but at least 256 MiB', () => {
    assert.equal(spaceNeeded(0), 256 * 1024 * 1024);
    assert.equal(spaceNeeded(100 * GB), 100 * GB + 2 * GB);
});

test('Hybrid-Phase asks for both phases, or the minimum one alone when the linear is there', () => {
    assert.deepEqual(phasesFor(false, false), ['linear']);
    assert.deepEqual(phasesFor(true, false), ['linear', 'minimum']);
    assert.deepEqual(phasesFor(true, true), ['minimum']);
    assert.equal(phasesText(['linear', 'minimum']), 'linear + minimum phase');
    assert.equal(barLabel('1G', 16, ['linear', 'minimum']), '1G · FS16 · 2 phases');
    assert.equal(barLabel('1G', 16, ['linear']), '1G · FS16');
    assert.equal(barLabel('30M', 8, ['minimum']), '30M · FS8 · minimum phase');
});

test('groups count what is still to come, an interrupted part taken off', () => {
    assert.deepEqual(groupSizes(PLAN), { '44k': 900 * MB, '48k': 1 * GB, hires: 2 * GB });
});

test('the ticked groups pick exactly their files and the free space decides', () => {
    const s = selection(PLAN, new Set(['44k', '48k']), PLAN.free);
    assert.equal(s.names.length, 3);
    assert.ok(!s.names.some(n => n.includes('352800')));
    assert.equal(s.bytes, 1.9 * GB);
    assert.equal(s.enough, true);
    assert.equal(selection(PLAN, new Set(['44k', '48k', 'hires']), 1.2 * GB).enough, false);
    // Unknown free space does not block; nothing ticked needs nothing.
    assert.equal(selection(PLAN, new Set(['hires']), null).enough, true);
    assert.equal(selection(PLAN, new Set(), 0).enough, true);
});

test('backend errors sort into checksum, space, and a connection that resumes', () => {
    assert.equal(classifyError('fir_x.npy arrived damaged (its checksum does not match)'), 'checksum');
    assert.equal(classifyError('Not enough free space in D:\\f: the filters need 2.3 GB, 1.2 GB is free.'), 'space');
    assert.equal(classifyError('fir_x.npy: error sending request'), 'network');
    assert.equal(classifyError('fir_x.npy: the server sent nothing for 60 s'), 'network');
    assert.equal(viewOf({ phase: 'error', error: 'fir_x.npy: HTTP 503' }), 'paused');
    assert.equal(viewOf({ phase: 'error', error: 'x arrived damaged (its checksum does not match)' }), 'failed');
    assert.equal(viewOf({ phase: 'error', error: 'Not enough free space in D:' }), 'nospace');
    assert.equal(viewOf({ phase: 'checking' }), 'dl');
    assert.equal(viewOf({ phase: 'downloading' }), 'dl');
    assert.equal(viewOf({ phase: 'done' }), 'done');
    assert.equal(viewOf({ phase: 'cancelled' }), 'cancelled');
    assert.equal(viewOf(null), 'idle');
});

test('speed comes from the last seconds, the time left from the speed', () => {
    const s = [{ t: 0, done: 0 }, { t: 1000, done: 10 * MB }, { t: 9000, done: 100 * MB }, { t: 10000, done: 142 * MB }];
    assert.equal(rateOf(s, 10000), 42 * MB);
    assert.equal(rateOf([{ t: 0, done: 0 }], 0), 0);
    assert.equal(etaText(1260 * MB, 42 * MB), '30 s left');
    assert.equal(etaText(42 * MB * 240, 42 * MB), '4 min left');
    assert.equal(etaText(42 * MB * 3900, 42 * MB), '1 h 5 min left');
    assert.equal(etaText(100, 0), '');
    assert.equal(percent(760, 2000), 38);
    assert.equal(percent(1, 0), 0);
});

test('the bar: missing with its size, downloading after Hide, ready until the setting moves', () => {
    const base = { missing: true, empty: false, label: '1G · FS16', key: '1000000000|16' };
    assert.equal(barModel({ ...base, missing: false }), null);
    let m = barModel({ ...base, size: GB });
    assert.equal(m.kind, 'missing');
    assert.equal(m.text, '⚠ 1G · FS16 — not downloaded');
    assert.equal(m.sub, '· 1.0 GB');
    assert.equal(m.download, true);
    // Size not known yet (offline): the bar still offers the download.
    assert.equal(barModel(base).sub, '');
    assert.equal(barModel({ ...base, notOffered: true }).download, false);
    assert.equal(barModel({ ...base, empty: true }).empty, true);

    const job = { view: 'dl', label: '1G · FS16', key: base.key, done: 380 * MB, total: 1000 * MB, eta: '1 min left' };
    m = barModel({ ...base, job, hidden: true });
    assert.equal(m.kind, 'dl');
    assert.equal(m.text, '↓ 1G · FS16 — downloading 38%');
    assert.equal(m.sub, '· 380 / 1000 MB · 1 min left');
    // With the dialog in view the bar keeps saying what is missing.
    assert.equal(barModel({ ...base, job, hidden: false }).kind, 'missing');

    const done = { ...job, view: 'done' };
    assert.equal(barModel({ ...base, missing: false, job: done }).kind, 'ok');
    assert.equal(barModel({ ...base, missing: false, key: '500000000|16', job: done }), null);
});

test('storage: in use by rung, and the unused list is exactly the files nothing reads', () => {
    const items = [
        { name: 'fir_1G_705600_linear_phase.b0.npy', size: 500 * MB, kind: 'filter', taps: 1e9, verified: true },
        { name: 'fir_300M_352800_linear_phase.npy', size: 600 * MB, kind: 'filter', taps: 3e8, verified: true },
        { name: 'fir_30M_352800_linear_phase.npy', size: 240 * MB, kind: 'filter', taps: 3e7, verified: null },
        { name: 'fir_30M_linear_phase.npy', size: 240 * MB, kind: 'filter', taps: 3e7, legacy: true, verified: null },
        { name: 'fir_500M_705600_linear_phase.b0.npy.part', size: 740 * MB, kind: 'partial', taps: 5e8 },
        { name: 'fir_200M_352800_linear_phase.npy', size: 800 * MB, kind: 'other', taps: null },
        { name: 'fir_100M_352800_linear_phase.npy', size: 7, kind: 'filter', taps: 1e8, verified: false },
    ];
    assert.equal(isUnused(items[0]), false);
    assert.equal(isUnused(items[3]), false);
    const m = storageModel(items);
    assert.deepEqual(m.rungs.map(r => [r.label, r.count]), [['1G', 1], ['300M', 1], ['30M and less', 2]]);
    assert.equal(m.usedCount, 4);
    assert.equal(m.used, 1580 * MB);
    assert.deepEqual(m.unused.map(i => i.name), [
        'fir_100M_352800_linear_phase.npy',
        'fir_200M_352800_linear_phase.npy',
        'fir_500M_705600_linear_phase.b0.npy.part',
    ]);
    assert.equal(m.unusedBytes, 1540 * MB + 7);
    assert.deepEqual(storageModel([]).rungs, []);
});
