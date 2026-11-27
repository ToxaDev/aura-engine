// A version on the air shows how it was made (version-view.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { viewOf, tapsText, fsOfRate, headroomKey, pinnedOnAir, setVersionView, versionViewNow, onVersionView } from '../src/js/version-view.js';

test('an FS8 FLAC made with 30M reads FS8 and 30M, its stages lit', () => {
    const v = viewOf({ path: 'a.flac', kind: 'pcm', rate: 352800, filter: 'Kaiser', taps: '30M', stages: ['ISP', 'SUB15', 'AA', 'PFR', 'HP', 'CUE', 'XTC'] });
    assert.equal(v.fs, 8);
    assert.equal(tapsText(v), '30M');
    assert.equal(v.subHz, 15);
    assert.equal(v.aa, true);
    assert.equal(v.out, null);
    for (const id of ['convLabIsp', 'convSubsonic', 'convAdaptiveApodizer', 'convFirResampling', 'convHybridPhase', 'convLabCue', 'convLabXtc']) assert.ok(v.on.has(id), id);
    for (const id of ['convLabDeclip', 'convLabHeadroom', 'convLabTfs', 'convEq', 'convLabAlpha']) assert.ok(!v.on.has(id), id);
});

test('a DSD256 version runs its chain at FS16 and names its modulator', () => {
    const v = viewOf({ path: 'a.dsf', kind: 'dsd', dsd: 256, modulator: 'SDM9 LA', filter: 'Kaiser', taps: '30M', stages: ['aHP'] });
    assert.equal(v.fs, 16);
    assert.deepEqual(v.out, { multiple: 256, label: 'SDM9 LA' });
    assert.ok(v.on.has('convHybridPhase') && v.on.has('convLabAlpha'));
    // DSD64 at the carrier ×4.
    assert.equal(viewOf({ kind: 'dsd', dsd: 64 }).fs, 4);
});

test('the recorded rack gives apodizing and headroom; the name wins on FS', () => {
    const sig = JSON.stringify({ fs: 16, taps: 30000000, apodizing: 2, headroomDb: -1, adaptiveApodizer: false });
    const v = viewOf({ kind: 'pcm', rate: 352800, taps: '30M', filter: 'Kaiser', stages: [] }, sig);
    assert.equal(v.fs, 8);
    assert.equal(v.apod, '2');
    assert.equal(v.hr, '-1.0');
    assert.equal(viewOf({ kind: 'pcm', rate: 352800 }, 'not json').hr, '0');
});

test('Auto sizes read as the name writes them', () => {
    assert.equal(tapsText(viewOf({ kind: 'pcm', rate: 352800, filter: 'AutoShift', taps: '212Mˢ' })), '212Mˢ');
    assert.equal(tapsText(viewOf({ kind: 'pcm', rate: 352800, filter: 'Auto', taps: '212M' })), 'Auto 212M');
    assert.equal(tapsText(viewOf({ kind: 'pcm', rate: 352800 })), '—');
});

test('rates, headroom keys', () => {
    assert.equal(fsOfRate(705600), 16);
    assert.equal(fsOfRate(384000), 8);
    assert.equal(fsOfRate(96001), 0);
    assert.equal(headroomKey(0), '0');
    assert.equal(headroomKey(-0.5), '-0.5');
    assert.equal(headroomKey(-3), '-3.0');
});

test('only the pinned file played from disk is a version on the air', () => {
    const e = { pin: { out: 'E:\\M\\a [AE].flac' } };
    const s = { state: 'playing' };
    assert.equal(pinnedOnAir(s, e, 'e:\\m\\A [AE].flac'), e.pin);
    assert.equal(pinnedOnAir(s, e, null), null);               // live
    assert.equal(pinnedOnAir(s, e, 'E:\\M\\b.flac'), null);    // another file
    assert.equal(pinnedOnAir({ state: 'stopped' }, e, e.pin.out), null);
    assert.equal(pinnedOnAir(s, {}, e.pin.out), null);         // no pin: the rack's own render
});

test('the view is told once per change', () => {
    let n = 0;
    const off = onVersionView(() => n++);
    const v = viewOf({ path: 'x', kind: 'pcm', rate: 352800, taps: '30M', stages: ['AA'] });
    setVersionView(v);
    setVersionView(viewOf({ path: 'x', kind: 'pcm', rate: 352800, taps: '30M', stages: ['AA'] }));
    assert.equal(versionViewNow(), v);
    setVersionView(null);
    setVersionView(null);
    off();
    assert.equal(n, 2);
});
