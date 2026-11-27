// An original and its versions (js/medialib/versions-core.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import {
    renderStem, isRenderName, sourceCandidates, splitAdds, resolveAdds, writingNow, playBlock, pinOf, pinHolds,
    madePairs, orphansOf, sweepPlan, SWEEP_LIMIT,
} from '../src/js/medialib/versions-core.js';

const D = 'D:\\Music\\Album\\';
const PCM = `${D}01 Song [AE · 44.1k→352.8k · Kaiser 10M · f64 · AA].flac`;
const DSD = `${D}01 Song [AE · 44.1k→DSD256 · SDM5 · Kaiser 10M · f64 · AA].dsf`;
const SRC = `${D}01 Song.mp3`;
const low = s => s.toLowerCase();
const disk = (...paths) => { const s = new Set(paths.map(low)); return p => s.has(low(p)); };

test('a converted file is told by its name as the backend tells it', () => {
    assert.equal(renderStem(PCM), '01 Song');
    assert.equal(renderStem(DSD), '01 Song');
    assert.equal(renderStem('a [AE · 384k].FLAC'), 'a');
    assert.equal(renderStem('[AE · 384k · Kaiser 10M · f64].flac'), '');
    assert.equal(renderStem('song [AE · 384k · Kaiser 10M · f64].wav'), null);
    assert.equal(renderStem('song [AE · 384k · Kaiser 10M · f64] (copy).flac'), null);
    assert.equal(renderStem('song [AE · ].flac'), null);
    assert.equal(renderStem('song [live].flac'), null);
    assert.equal(renderStem('01 Song.flac'), null);
    assert.ok(isRenderName(PCM) && !isRenderName(SRC));
    // A stem that itself holds a block: the last block is the converter's.
    assert.equal(renderStem('x [AE · 96k] [AE · 384k].flac'), 'x [AE · 96k]');
});

test('the original of a version is looked for beside it, under its stem', () => {
    const c = sourceCandidates(PCM);
    assert.equal(c[0], `${D}01 Song.flac`);
    assert.ok(c.includes(SRC));
    assert.deepEqual(sourceCandidates(SRC), []);
    assert.deepEqual(sourceCandidates('[AE · 384k].flac'), []);
    assert.equal(sourceCandidates('/music/a [AE · 384k].flac')[0], '/music/a.flac');
});

test('versions among the paths to add get no row of their own', () => {
    const r = splitAdds([SRC, PCM, DSD, `${D}02 Other.flac`, `${D}03 Lone [AE · 384k].flac`]);
    assert.deepEqual(r.plain, [SRC, `${D}02 Other.flac`]);
    // The versions of an original that is being added too are left out.
    assert.deepEqual(r.renders, [`${D}03 Lone [AE · 384k].flac`]);
    assert.deepEqual(splitAdds(null), { plain: [], renders: [] });
});

test('a dropped version gives way to its original; one with no original stays the file it is', () => {
    const lone = `${D}03 Lone [AE · 384k].flac`;
    const r = resolveAdds([PCM, DSD, lone], disk(SRC));
    assert.deepEqual(r.add, [SRC, lone]);
    assert.equal(r.swapped, 2);
    assert.deepEqual(resolveAdds([], disk()), { add: [], swapped: 0 });
});

test('a version is being written while its original converts and it is not a finished record of another rack', () => {
    const v = { path: DSD, source: SRC };
    const none = () => null;
    assert.equal(writingNow(v, [], none), false);
    assert.equal(writingNow(v, [{ path: `${D}other.mp3`, sig: 'a' }], none), false);
    // The original converts and nobody recorded this file: it is the one in the making.
    assert.equal(writingNow(v, [{ path: SRC.toUpperCase(), sig: 'a' }], none), true);
    // A finished version of another rack stays playable meanwhile.
    assert.equal(writingNow(v, [{ path: SRC, sig: 'a' }], () => 'b'), false);
    // The same rack again: the file is being replaced.
    assert.equal(writingNow(v, [{ path: SRC, sig: 'a' }], () => 'a'), true);
    assert.equal(writingNow({ path: DSD, source: null }, [{ path: SRC, sig: 'a' }], none), false);
});

test('why a version cannot be played', () => {
    const pcm = { path: PCM, source: SRC, kind: 'pcm', rate: 352800 };
    const dsd = { path: DSD, source: SRC, kind: 'dsd', dsd: 256 };
    assert.equal(playBlock(pcm), null);
    assert.equal(playBlock(dsd), null);
    assert.equal(playBlock(pcm, { writing: true }), 'writing');
    assert.equal(playBlock({ ...pcm, source: null }), 'gone');
    assert.equal(playBlock({ ...pcm, source: null }, { writing: true }), 'writing');
    assert.equal(playBlock(pcm, { outdated: true }), 'outdated');
    assert.equal(playBlock(pcm, { cue: true }), 'cue');
    assert.equal(playBlock(pcm, { volumeFull: false }), null);
    assert.equal(playBlock(dsd, { volumeFull: false }), 'volume');
    assert.equal(playBlock({ ...dsd, dsd: 512 }), 'rate');
    assert.equal(playBlock({ ...dsd, dsd: null }), 'rate');
});

test('a pin holds while the output is the version\'s own kind', () => {
    const p = pinOf({ path: PCM, kind: 'pcm', rate: 352800 });
    const d = pinOf({ path: DSD, kind: 'dsd', dsd: 256 });
    assert.deepEqual({ out: p.out, dsd: p.dsd }, { out: PCM, dsd: 0 });
    assert.deepEqual({ out: d.out, dsd: d.dsd }, { out: DSD, dsd: 256 });
    // The name's settings travel with the pin (Sound shows them while it plays).
    assert.equal(p.v.rate, 352800);
    assert.equal(d.v.dsd, 256);
    assert.deepEqual(d.v.stages, []);
    assert.equal(pinHolds(p, 'flac'), true);
    assert.equal(pinHolds(p, '256:sdm5'), false);
    assert.equal(pinHolds(d, '256:sdm5'), true);
    assert.equal(pinHolds(d, '256:sdm7La'), true);
    assert.equal(pinHolds(d, '128:sdm5'), false);
    assert.equal(pinHolds(d, 'flac'), false);
    assert.equal(pinHolds(p, undefined), false);
    assert.equal(pinHolds(null, 'flac'), false);
});

test('what this app made: the rows\' records and the kept list, each file once', () => {
    const pairs = madePairs(
        [
            { path: SRC, convs: [{ out: PCM, sig: 'a' }], stale: [{ out: DSD }] },
            // A row that is itself a converted file names no original.
            { path: PCM, convs: [{ out: `${D}01 Song [AE · 96k] [AE · 384k].flac` }] },
            // A record that points at something that is no version is ignored.
            { path: `${D}02.flac`, convs: [{ out: `${D}02 copy.flac` }] },
        ],
        { [low(PCM)]: { out: PCM, source: `${D}other.mp3` }, x: { out: `${D}09 Z [AE · 384k].flac`, source: `${D}09 Z.wav` }, bad: null },
    );
    assert.deepEqual(pairs, [
        { out: PCM, source: SRC },
        { out: DSD, source: SRC },
        { out: `${D}09 Z [AE · 384k].flac`, source: `${D}09 Z.wav` },
    ]);
});

test('versions of an original that is gone: only where the folder still stands', () => {
    const pairs = [{ out: PCM, source: SRC }, { out: DSD, source: SRC }];
    const dir = D.slice(0, -1);
    // The original is there: nothing.
    assert.deepEqual(orphansOf(pairs, disk(dir, SRC, PCM, DSD)), []);
    // Gone (deleted or renamed), folder and versions there: both.
    assert.deepEqual(orphansOf(pairs, disk(dir, PCM, DSD, `${D}01 Song (remaster).mp3`)), pairs);
    // The drive is unplugged or the folder moved away: nothing is known, nothing goes.
    assert.deepEqual(orphansOf(pairs, disk()), []);
    assert.deepEqual(orphansOf(pairs, disk(PCM, DSD)), []);
    // The version is not there either.
    assert.deepEqual(orphansOf(pairs, disk(dir, DSD)), [pairs[1]]);
    // The original came back under another extension: its versions stay.
    assert.deepEqual(orphansOf(pairs, disk(dir, PCM, DSD, `${D}01 Song.flac`)), []);
    // Never the file that plays, nor one being written.
    assert.deepEqual(orphansOf(pairs, disk(dir, PCM, DSD), { playing: PCM.toUpperCase() }), [pairs[1]]);
    assert.deepEqual(orphansOf(pairs, disk(dir, PCM, DSD), { writing: o => o === DSD }), [pairs[0]]);
    // A network share has no Recycle Bin.
    const unc = [{ out: '\\\\nas\\m\\a [AE · 384k].flac', source: '\\\\nas\\m\\a.flac' }];
    assert.deepEqual(orphansOf(unc, disk('\\\\nas\\m', unc[0].out)), []);
    // The versions written to another folder than the original's.
    const far = [{ out: 'E:\\Out\\a [AE · 384k].flac', source: 'D:\\Music\\a.flac' }];
    assert.deepEqual(orphansOf(far, disk('D:\\Music', far[0].out)), far);
    assert.deepEqual(orphansOf(far, disk(far[0].out)), []);
});

test('a sweep takes what was gone twice, and asks past the limit', () => {
    const p = n => ({ out: `${D}${n} [AE · 384k].flac`, source: `${D}${n}.flac` });
    assert.deepEqual(sweepPlan([p(1), p(2)], [p(2), p(3)]), { trash: [p(2)], ask: 0 });
    assert.deepEqual(sweepPlan([p(1)], []), { trash: [], ask: 0 });
    const many = Array.from({ length: SWEEP_LIMIT + 1 }, (_, i) => p(i));
    assert.deepEqual(sweepPlan(many, many), { trash: [], ask: SWEEP_LIMIT + 1 });
    assert.equal(sweepPlan(many.slice(1), many).trash.length, SWEEP_LIMIT);
});

test('A/B: the original on the air goes on from its second; another track, or none, starts from the top', async () => {
    const { abStart, abFix } = await import('../src/js/medialib/versions-core.js');
    const src = 'E:/Music/A/01 Song.flac';
    assert.deepEqual(abStart({ trackId: 4, state: 'playing', positionS: 73.4 }, 'e:/music/a/01 song.flac', src), { trackId: 4, at: 73.4, paused: false });
    assert.deepEqual(abStart({ trackId: 4, state: 'paused', positionS: 12 }, src, src), { trackId: 4, at: 12, paused: true });
    assert.equal(abStart({ trackId: 4, state: 'stopped', positionS: 12 }, src, src), null);
    assert.equal(abStart({ trackId: 5, state: 'playing', positionS: 12 }, 'E:/Music/A/02 Other.flac', src), null);
    assert.equal(abStart(null, null, src), null);
    // Started over after the switch: the jump is made again; ahead is time gone by.
    assert.equal(abFix(73.4, { positionS: 0.4 }), 73.4);
    assert.equal(abFix(73.4, { positionS: 76 }), null);
    assert.equal(abFix(73.4, { positionS: 72 }), null);
    assert.equal(abFix(2, { positionS: 0 }), null);
    assert.equal(abFix(50, {}), null);
});
