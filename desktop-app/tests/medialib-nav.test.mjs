// The library's Back / Forward and the head's path (js/medialib/nav.js), and
// where its menu goes (js/medialib/menu.js menuPlace).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHistory, crumbsOf, sectionMemory, sectionTarget } from '../src/js/medialib/nav.js';

const at = (o = {}) => ({ tab: 'albums', artist: null, genre: null, flt: null, album: null, folder: null, playlist: null, q: '', ...o });

test('back returns to the place left, forward to the one come back from', () => {
    const h = createHistory();
    const list = at(), album = at({ album: 'a1' });
    assert.equal(h.canBack(list), false);
    h.mark(list, 340);
    assert.equal(h.canBack(album), true);
    const b = h.back(album, 12);
    assert.deepEqual(b, { at: list, top: 340 });
    assert.equal(h.canForward(list), true);
    assert.deepEqual(h.forward(list, 340), { at: album, top: 12 });
    assert.equal(h.forward(album), null);
});

test('a section pressed where it already is leaves no step that goes nowhere', () => {
    const h = createHistory();
    const pls = at({ tab: 'playlists' });
    h.mark(at(), 0);
    h.mark(pls, 0);          // Playlists pressed on the playlists' own page
    h.mark(pls, 5);
    assert.deepEqual(h.back(pls).at, at());
    assert.equal(h.back(at()), null);
});

test('a new move forgets the way forward', () => {
    const h = createHistory();
    h.mark(at(), 0);
    h.back(at({ album: 'x' }));
    h.mark(at(), 0);
    assert.equal(h.canForward(at({ tab: 'genres' })), false);
});

test('the history keeps no more than its limit', () => {
    const h = createHistory(3);
    for (let i = 0; i < 10; i++) h.mark(at({ album: 'a' + i }), 0);
    let n = 0;
    let cur = at({ album: 'end' });
    for (let s; (s = h.back(cur)); cur = s.at) n++;
    assert.equal(n, 3);
});

test('the path: the section, then what is open in it; the last step leads nowhere', () => {
    const names = { tabs: { albums: 'Albums', playlists: 'Playlists', folders: 'Folders' }, album: 'Dangerous', playlist: 'Evening' };
    assert.deepEqual(crumbsOf(at(), names), [{ text: 'Albums', to: null }]);

    const a = crumbsOf(at({ artist: 'Michael Jackson', album: 'id1' }), names);
    assert.deepEqual(a.map(s => s.text), ['Albums', 'Michael Jackson', 'Dangerous']);
    assert.equal(a[0].to.artist, null);
    assert.equal(a[1].to.artist, 'Michael Jackson');
    assert.equal(a[1].to.album, null);
    assert.equal(a[2].to, null);

    // An open playlist: "Playlists" leads to the list of them all.
    const p = crumbsOf(at({ tab: 'playlists', playlist: 4 }), names);
    assert.deepEqual(p.map(s => s.text), ['Playlists', 'Evening']);
    assert.equal(p[0].to.playlist, null);
    assert.equal(p[0].to.tab, 'playlists');

    const f = crumbsOf(at({ tab: 'folders', folder: 'E:\\MUSIC\\Jazz\\' }), names);
    assert.deepEqual(f.map(s => s.text), ['Folders', 'Jazz']);

    const q = crumbsOf(at({ q: 'moon', album: 'id2' }), names);
    assert.deepEqual(q.map(s => s.text), ['Albums', 'Search: moon', 'Dangerous']);
});

test('the menu opens down and right of the point, and turns back at the window\'s edges', async () => {
    const { menuPlace } = await import('../src/js/medialib/menu.js');
    assert.deepEqual(menuPlace(100, 100, 200, 300, 1200, 800), { left: 100, top: 100 });
    assert.deepEqual(menuPlace(1100, 100, 200, 300, 1200, 800), { left: 900, top: 100 });
    assert.deepEqual(menuPlace(100, 700, 200, 300, 1200, 800), { left: 100, top: 400 });
    assert.equal(menuPlace(100, 200, 200, 790, 1200, 800).top, 6);
});

test('a section pressed again where it is shows it as it is: nothing is reset', () => {
    assert.equal(sectionTarget('albums', 'albums', null), null);
    assert.equal(sectionTarget('artists', 'artists', { at: at({ tab: 'artists', q: 'x' }), top: 5 }), null);
});

test('a section come back to is shown as it was left: search, chips, album, scroll', () => {
    const mem = sectionMemory('test-a');
    mem.forget();
    const left = { at: at({ q: 'modern', genre: 'Pop', flt: 'hi', album: 'a7' }), top: 880, list: 1200, title: 'Album', up: null };
    mem.keep('albums', left);
    left.at.q = 'changed later';                                  // kept as a copy
    const t = sectionTarget('artists', 'albums', mem.recall('albums'));
    assert.deepEqual(t.at, at({ q: 'modern', genre: 'Pop', flt: 'hi', album: 'a7' }));
    assert.equal(t.top, 880);
    assert.equal(t.kept.list, 1200);
    assert.equal(t.kept.title, 'Album');
});

test('a section never visited opens at its own page; a folder asked for, or root, open those', () => {
    const mem = sectionMemory('test-b');
    mem.forget();
    assert.deepEqual(sectionTarget('albums', 'genres', mem.recall('genres')), { at: { tab: 'genres' }, top: 0, kept: null });
    mem.keep('folders', { at: at({ tab: 'folders', folder: 'D:/M/A' }), top: 300 });
    assert.deepEqual(sectionTarget('albums', 'folders', mem.recall('folders'), { folder: 'E:/Music' }).at, { tab: 'folders', folder: 'E:/Music' });
    assert.deepEqual(sectionTarget('folders', 'folders', mem.recall('folders'), { folder: 'E:/Music' }).at, { tab: 'folders', folder: 'E:/Music' });
    assert.deepEqual(sectionTarget('folders', 'folders', mem.recall('folders'), { root: true }), { at: { tab: 'folders' }, top: 0, kept: null });
    assert.equal(sectionTarget('albums', 'folders', mem.recall('folders')).at.folder, 'D:/M/A');
});

test('the memory lasts across a browser mounted again, apart per layout', () => {
    sectionMemory('test-c').forget();
    sectionMemory('test-d').forget();
    sectionMemory('test-c').keep('playlists', { at: at({ tab: 'playlists', playlist: 'p1' }), top: 10 });
    assert.equal(sectionMemory('test-c').recall('playlists').at.playlist, 'p1');
    assert.equal(sectionMemory('test-d').recall('playlists'), null);
});
