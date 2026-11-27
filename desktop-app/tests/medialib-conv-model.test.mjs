// The Conversions page's arithmetic (js/medialib/conv-model.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import {
    rateText, fsOf, typeOf, versionLine, groupFiles, versionTree, typesOf, usageOf, pick, sizeText, whenText, foldersOf,
} from '../src/js/medialib/conv-model.js';

const F = (name, o = {}) => ({
    path: `D:\\Music\\Album\\${name}`, source: 'D:\\Music\\Album\\01 Song.flac', sourceSize: 30e6,
    size: 100e6, mtimeMs: 1000, kind: 'pcm', rate: 352800, dsd: null, filter: 'AURA', taps: '10M', precision: '64', stages: ['DC', 'AA'],
    ...o,
});
const files = [
    F('01 Song [AE · 44.1k→352.8k · AURA 10M · f64 · DC·AA].flac', { mtimeMs: 3000 }),
    F('01 Song [AE · 44.1k→DSD256 SDM7 · AURA 10M · f64].dsf', { kind: 'dsd', rate: null, dsd: 256, modulator: 'SDM7', size: 400e6, mtimeMs: 2000 }),
    F('01 Song [AE · 44.1k→DSD256 SDM9 · AURA 10M · f64].dsf', { kind: 'dsd', rate: null, dsd: 256, modulator: 'SDM9', size: 400e6, mtimeMs: 5000 }),
    F('02 Other [AE · 705.6k · AURA 5k · f64].flac', { path: 'D:\\Music\\B\\02 Other [AE · 705.6k · AURA 5k · f64].flac', source: null, sourceSize: null, rate: 705600, size: 200e6, mtimeMs: 4000 }),
];

test('rates and kinds', () => {
    assert.equal(rateText(352800), '352.8k');
    assert.equal(rateText(384000), '384k');
    assert.equal(fsOf(352800), 8);
    assert.equal(fsOf(768000), 16);
    assert.equal(fsOf(50000), 0);
    assert.equal(typeOf(files[0]), 'FS8');
    assert.equal(typeOf(files[1]), 'DSD256');
    assert.equal(typeOf({ kind: 'pcm', rate: 50000 }), '50k');
    assert.equal(versionLine(files[0]), '352.8k · AURA 10M · f64');
    assert.equal(versionLine(files[1]), 'SDM7 · AURA 10M · f64');
});

test('files under their sources, the newest version first', () => {
    const g = groupFiles(files);
    assert.equal(g.length, 2);
    const song = g.find(x => x.source);
    assert.equal(song.name, '01 Song.flac');
    assert.deepEqual(song.versions.map(v => v.mtimeMs), [5000, 3000, 2000]);
    assert.equal(song.size, 900e6);
    assert.equal(song.last, 5000);
    const gone = g.find(x => !x.source);
    assert.equal(gone.name, '02 Other');
    assert.equal(gone.dir, 'D:\\Music\\B');
});

test('a source\'s tree: PCM by rate, then DSD, each with its versions', () => {
    const song = groupFiles(files).find(x => x.source);
    const tree = versionTree(song);
    assert.deepEqual(tree.map(b => b.type), ['FS8', 'DSD256']);
    assert.deepEqual(tree[1].versions.map(v => v.modulator), ['SDM9', 'SDM7']);
    assert.equal(tree[1].size, 800e6);
});

test('kinds and the space', () => {
    assert.deepEqual(typesOf(files).map(t => [t.type, t.count]), [['FS8', 1], ['FS16', 1], ['DSD256', 2]]);
    const u = usageOf(files);
    assert.equal(u.total, 1100e6);
    assert.equal(u.originals, 30e6);
    assert.equal(u.sources, 1);
    assert.equal(u.count, 4);
    assert.ok(Math.abs(u.types.reduce((s, t) => s + t.share, 0) - 1) < 1e-9);
});

test('the search, the kind and the sort', () => {
    const g = groupFiles(files);
    assert.deepEqual(pick(g).map(x => x.name), ['01 Song.flac', '02 Other']);
    assert.deepEqual(pick(g, { sort: 'name' }).map(x => x.name), ['01 Song.flac', '02 Other']);
    assert.deepEqual(pick(g, { q: 'other' }).map(x => x.name), ['02 Other']);
    assert.deepEqual(pick(g, { q: 'sdm9' }).map(x => x.name), ['01 Song.flac']);
    const dsd = pick(g, { type: 'DSD256' });
    assert.equal(dsd.length, 1);
    assert.equal(dsd[0].versions.length, 2);
    assert.equal(dsd[0].size, 800e6);
    assert.deepEqual(pick(g, { type: 'FS16', sort: 'size' }).map(x => x.name), ['02 Other']);
    assert.equal(g.find(x => x.source).versions.length, 3);
});

test('sizes and dates', () => {
    assert.equal(sizeText(512), '512 B');
    assert.equal(sizeText(1536), '1.50 KB');
    assert.equal(sizeText(37.2 * 1024 * 1024), '37.2 MB');
    assert.equal(sizeText(812 * 1024 * 1024), '812 MB');
    const now = new Date(2026, 10, 21, 15, 0).getTime();
    assert.equal(whenText(new Date(2026, 10, 21, 9, 0).getTime(), now), 'today');
    assert.equal(whenText(new Date(2026, 10, 20, 23, 0).getTime(), now), 'yesterday');
    assert.equal(whenText(new Date(2026, 10, 16, 12, 0).getTime(), now), '5 days ago');
    assert.equal(whenText(new Date(2026, 5, 3).getTime(), now), '2026-06-03');
});

test('the folders asked of the scan: each once, none inside another', () => {
    assert.deepEqual(
        foldersOf(['D:\\Music\\A\\1.flac', 'D:\\Music\\A\\2.flac', 'D:\\Music\\A\\CD1\\3.flac', 'E:\\X\\4.flac', 'D:\\Music\\AB\\5.flac']),
        ['E:\\X', 'D:\\Music\\A', 'D:\\Music\\AB'],
    );
});
