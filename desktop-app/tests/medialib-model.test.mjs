// The media library's plain logic (medialib/model.js): the backend's query,
// the sorts, a cover's address, the queue's order, the lines' words.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    SECTIONS, SORTS, albumQuery, rateOfTag, resRank, sortAlbums, coverUrl, fmtKind, initials, albumLine,
    albumPaths, queueOrder, missingPaths, playsFrom, scanLine, time, addedText, plural,
} from '../src/js/medialib/model.js';

test('the sections and the sorts', () => {
    assert.deepEqual(SECTIONS, ['albums', 'artists', 'genres', 'playlists', 'folders']);
    assert.deepEqual(Object.keys(SORTS), ['added', 'artist', 'title', 'year', 'res']);
});

test('the query: only what is set, the backend sorting all but the resolution', () => {
    assert.deepEqual(albumQuery(), { sort: 'added' });
    assert.deepEqual(albumQuery({ q: '  pink  ', genre: 'Rock', hiRes: true, sort: 'year' }), { sort: 'year', q: 'pink', genre: 'Rock', hiRes: true });
    assert.deepEqual(albumQuery({ artist: 'Adele', dsd: true, sort: 'res' }), { sort: 'added', artist: 'Adele', dsd: true });
    assert.deepEqual(albumQuery({ sort: 'nonsense' }), { sort: 'added' });
    assert.deepEqual(albumQuery({ dupes: true }), { sort: 'added', dupes: true });
});

test('the resolution: DSD over PCM, then the rate, then the bits', () => {
    assert.equal(rateOfTag('24/96'), 96000);
    assert.equal(rateOfTag('16/44.1'), 44100);
    assert.equal(rateOfTag('DSD64'), 2822400);
    assert.equal(rateOfTag('FLAC'), 0);
    assert.ok(resRank('DSD64') > resRank('32/384'));
    assert.ok(resRank('DSD128') > resRank('DSD64'));
    assert.ok(resRank('24/96') > resRank('24/48'));
    assert.ok(resRank('24/44.1') > resRank('16/44.1'));
    assert.ok(resRank('16/44.1') > resRank('MP3'));
    const list = [
        { title: 'b', tag: '16/44.1' }, { title: 'a', tag: 'MP3' }, { title: 'c', tag: 'DSD64' },
        { title: 'd', tag: '24/96' }, { title: 'A', tag: '24/96' },
    ];
    assert.deepEqual(sortAlbums(list, 'res').map(a => a.title), ['c', 'A', 'd', 'b', 'a']);
    assert.equal(sortAlbums(list, 'added'), list, 'the backend\'s order kept');
});

test('a cover: the streamer\'s address served by the window\'s protocol without /api', () => {
    assert.equal(coverUrl('/api/lib/cover/abc?v=12'), 'https://aura.localhost/lib/cover/abc?v=12');
    assert.equal(coverUrl('/lib/cover/abc'), 'https://aura.localhost/lib/cover/abc');
    assert.equal(coverUrl(null), null);
    assert.equal(coverUrl(''), null);
    assert.equal(coverUrl('https://x/y.jpg'), 'https://x/y.jpg');
});

test('the words of a line', () => {
    assert.equal(fmtKind('DSD64'), 'dsd');
    assert.equal(fmtKind('24/96', true), 'hi');
    assert.equal(fmtKind('16/44.1', false), '');
    assert.equal(initials('Pink Floyd'), 'PF');
    assert.equal(initials('björk'), 'B');
    assert.equal(initials(''), '?');
    assert.equal(time(187), '3:07');
    assert.equal(time(3765), '1:02:45');
    assert.equal(plural(1, 'album'), '1 album');
    assert.equal(plural(2, 'album'), '2 albums');
    assert.equal(albumLine({ year: '1973', tracks: 10, durationS: 2580, genres: ['Rock'] }), '1973 · 10 tracks · 43:00 · Rock');
    assert.equal(albumLine({ tracks: 1, durationS: 60, genres: [] }), '1 track · 1:00');
    const now = 1_800_000_000;
    assert.equal(addedText(now - 100, now), 'today');
    assert.equal(addedText(now - 86400 * 1.5, now), 'yesterday');
    assert.equal(addedText(now - 86400 * 5, now), '5 days ago');
    assert.match(addedText(now - 86400 * 90, now), /^[A-Z][a-z]{2} \d{4}$/);
    assert.equal(addedText(0, now), '');
});

test('the queue: in the album\'s order, or shuffled with every track once', () => {
    const a = { list: [{ path: 'x/1.flac' }, { path: 'x/2.flac' }, { path: 'x/3.flac' }, { path: null }] };
    const p = albumPaths(a);
    assert.deepEqual(p, ['x/1.flac', 'x/2.flac', 'x/3.flac']);
    assert.deepEqual(queueOrder(p), p);
    assert.notEqual(queueOrder(p), p, 'a copy');
    let n = 0;
    const seq = [0.9, 0.1, 0.5];
    const s = queueOrder(p, { shuffle: true, rng: () => seq[n++ % seq.length] });
    assert.deepEqual([...s].sort(), [...p].sort());
    assert.deepEqual(p, ['x/1.flac', 'x/2.flac', 'x/3.flac'], 'the album is not changed');
    const big = Array.from({ length: 40 }, (_, i) => String(i));
    assert.notDeepEqual(queueOrder(big, { shuffle: true }), big, 'forty tracks are not left in order');
    assert.deepEqual(missingPaths(['a', 'b', 'a', 'c'], ['b']), ['a', 'c']);
});

test('what plays from an album: its folder or a CD under it', () => {
    assert.ok(playsFrom('E:\\MUSIC\\A', 'E:\\MUSIC\\A\\01.flac'));
    assert.ok(playsFrom('E:\\MUSIC\\A', 'E:\\MUSIC\\A\\CD2\\01.flac'));
    assert.ok(playsFrom('E:\\MUSIC\\A', 'E:\\MUSIC\\A\\Disc 1\\01.flac'));
    assert.ok(!playsFrom('E:\\MUSIC\\A', 'E:\\MUSIC\\AB\\01.flac'));
    assert.ok(!playsFrom('E:\\MUSIC\\A', 'E:\\MUSIC\\A\\Bonus\\01.flac'));
    assert.ok(!playsFrom('E:\\MUSIC\\A', null));
});

test('the scan line', () => {
    assert.equal(scanLine(null), 'Reading the library…');
    assert.equal(scanLine({ roots: [] }), 'No music folder yet');
    assert.equal(scanLine({ roots: ['E:\\MUSIC'], scanning: true, total: 0 }), 'E:\\MUSIC · looking for music');
    assert.equal(scanLine({ roots: ['E:\\MUSIC'], scanning: true, done: 5, total: 90 }), 'E:\\MUSIC · reading 5 of 90');
    assert.equal(scanLine({ roots: ['E:\\MUSIC', 'D:\\Hi'], albums: 254, tracks: 3001 }), '2 folders · 254 albums · 3001 tracks');
});
