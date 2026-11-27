// The Duplicates page's logic (js/medialib/dupes-model.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import {
    under, chooseKeep, rowsOf, kindCounts, ticksFor, pathsFor, sizesOf, totals, selection, differPart, confirmLines, ruleText, fmtWords,
} from '../src/js/medialib/dupes-model.js';

const M = (path, o = {}) => ({ path, size: 100, added: 10, rate: 44100, bits: 16, conversions: 0, ext: 'flac', tag: '16/44.1', ...o });
const data = {
    roots: ['E:\\MUSIC', 'C:\\Music'],
    twins: [{
        kind: 'exact', title: 'Meteora', artist: 'Linkin Park', tracks: 3,
        folders: [
            { folder: 'E:\\MUSIC\\LP\\Meteora', size: 300, tracks: 3, added: 5, conversions: 2, paths: ['E:\\MUSIC\\LP\\Meteora\\1.flac', 'E:\\MUSIC\\LP\\Meteora\\2.flac', 'E:\\MUSIC\\LP\\Meteora\\3.flac'] },
            { folder: 'C:\\Music\\Meteora', size: 200, tracks: 2, added: 9, conversions: 0, paths: ['C:\\Music\\Meteora\\1.flac', 'C:\\Music\\Meteora\\2.flac'] },
        ],
    }],
    groups: [
        { kind: 'exact', twin: 0, title: 'One', members: [M('E:\\MUSIC\\LP\\Meteora\\1.flac'), M('C:\\Music\\Meteora\\1.flac')] },
        { kind: 'exact', twin: 0, title: 'Two', members: [M('E:\\MUSIC\\LP\\Meteora\\2.flac'), M('C:\\Music\\Meteora\\2.flac')] },
        { kind: 'exact', twin: null, title: 'Shape', members: [M('E:\\MUSIC\\Ed\\Divide\\04.mp3', { size: 9 }), M('E:\\MUSIC\\Single\\04.mp3', { size: 9 }), M('C:\\Music\\04.mp3', { size: 9, added: 1 })] },
        { kind: 'audio', twin: null, title: 'Lose', members: [M('E:\\MUSIC\\Em\\8 Mile\\01.flac', { size: 38 }), M('E:\\MUSIC\\Em\\Curtain\\05.flac', { size: 37 })] },
    ],
    versions: [{ title: 'In the End', members: [M('E:\\MUSIC\\Hi\\08.flac', { rate: 96000, bits: 24 }), M('E:\\MUSIC\\mp3\\08.mp3', { ext: 'mp3' })] }],
};

test('a path under a folder, either slash, no case', () => {
    assert.ok(under('E:\\MUSIC\\a\\b.flac', 'e:/music'));
    assert.ok(under('E:\\MUSIC', 'E:\\MUSIC\\'));
    assert.ok(!under('E:\\MUSIC2\\a.flac', 'E:\\MUSIC'));
    assert.ok(!under('x', ''));
});

test('the rows: twins first, folded groups inside them, versions last', () => {
    const rows = rowsOf(data);
    assert.deepEqual(rows.map(r => r.kind), ['twin', 'exact', 'audio', 'versions']);
    assert.equal(rows[0].groups.length, 2);
    assert.deepEqual(kindCounts(rows), { all: 4, twin: 1, exact: 1, audio: 1, versions: 1 });
});

test('which copy stays: conversions, then a playlist, then the rule', () => {
    const items = [M('C:\\Music\\a.flac'), M('E:\\MUSIC\\a.flac', { added: 3 }), M('E:\\MUSIC\\deep\\er\\a.flac', { rate: 96000, bits: 24 })];
    assert.equal(chooseKeep(items, 'folder:E:\\MUSIC'), 1, 'the first of the folder; the backend order breaks the tie');
    assert.equal(chooseKeep(items, 'folder:C:\\Music'), 0);
    assert.equal(chooseKeep(items, 'quality'), 2);
    assert.equal(chooseKeep(items, 'oldest'), 1);
    assert.equal(chooseKeep(items, 'shortest'), 0);
    assert.equal(chooseKeep(items, 'quality', { inPlaylist: new Set(['C:\\Music\\a.flac']) }), 0, 'in a playlist wins over the rule');
    items[1].conversions = 1;
    assert.equal(chooseKeep(items, 'quality', { inPlaylist: new Set(['C:\\Music\\a.flac']) }), 1, 'conversions win over all');
    const rows = rowsOf(data);
    assert.equal(chooseKeep(rows[0].items, 'folder:C:\\Music'), 0, 'the twin folder with conversions stays');
});

test('ticks: exact all but the keep, same audio none, versions never', () => {
    const rows = rowsOf(data);
    assert.deepEqual([...ticksFor(rows[1], 0)], [1, 2]);
    assert.deepEqual([...ticksFor(rows[0], 0)], [1]);
    assert.equal(ticksFor(rows[2], 0).size, 0);
    assert.equal(ticksFor(rows[3], -1).size, 0);
    assert.deepEqual(pathsFor(rows[3], new Set([0, 1])), []);
});

test('a twin folder sends only its tracks that are copies', () => {
    const rows = rowsOf(data);
    assert.deepEqual(pathsFor(rows[0], new Set([1])), ['C:\\Music\\Meteora\\1.flac', 'C:\\Music\\Meteora\\2.flac']);
    // Kept the other way round, the main folder's third track (no copy) stays.
    assert.deepEqual(pathsFor(rows[0], new Set([0])), ['E:\\MUSIC\\LP\\Meteora\\1.flac', 'E:\\MUSIC\\LP\\Meteora\\2.flac']);
});

test('totals count a folded group once and the selection each file once', () => {
    const rows = rowsOf(data);
    const t = totals(rows, r => (r.kind === 'versions' ? -1 : 0));
    assert.deepEqual(t, { groups: 3, files: 2 + 2 + 1, bytes: 200 + 18 + 37 });
    const sizes = sizesOf(data);
    const s = selection(['C:\\Music\\04.mp3', 'C:\\Music\\04.mp3', 'E:\\MUSIC\\Single\\04.mp3'], sizes);
    assert.deepEqual(s, { files: 2, bytes: 18, paths: ['C:\\Music\\04.mp3', 'E:\\MUSIC\\Single\\04.mp3'] });
});

test('the part of a path that differs', () => {
    const [a, b] = differPart(['E:\\MUSIC\\Ed Sheeran\\Divide\\04 Shape.mp3', 'E:\\MUSIC\\Shape - Single\\04 Shape.mp3']);
    assert.deepEqual(a, ['E:\\MUSIC\\', 'Ed Sheeran\\Divide\\', '04 Shape.mp3']);
    assert.deepEqual(b, ['E:\\MUSIC\\', 'Shape - Single\\', '04 Shape.mp3']);
    const [c, d] = differPart(['E:\\MUSIC\\LP\\Meteora', 'C:\\Music\\Meteora']);
    assert.equal(c.join(''), 'E:\\MUSIC\\LP\\Meteora');
    assert.equal(d[1] + d[2], 'C:\\Music\\Meteora');
    assert.deepEqual(differPart(['x\\y']), [['x\\y', '', '']]);
});

test('the confirmation lines and the words', () => {
    const rows = rowsOf(data);
    const ticks = new Map([[rows[0].id, new Set([1])], [rows[1].id, new Set([1, 2])]]);
    const c = confirmLines(rows, ticks);
    assert.deepEqual(c, { whole: [{ folder: 'C:\\Music\\Meteora', files: 2 }], files: 2, dirs: 2 });
    assert.equal(ruleText('folder:E:\\MUSIC'), 'Copy in E:\\MUSIC');
    assert.equal(ruleText('oldest'), 'Added first');
    assert.deepEqual(fmtWords({ path: 'a.flac', ext: 'flac', tag: '16/44.1' }), ['FLAC', '16/44.1']);
    assert.deepEqual(fmtWords({ path: 'a.mp3', ext: 'mp3', tag: 'MP3' }), ['MP3']);
});
