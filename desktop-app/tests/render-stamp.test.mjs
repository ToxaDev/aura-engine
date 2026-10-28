// Renders made by an older version do not count (Anton 19.10).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// Every render carries a stamp: the version that made it, the generation of
// the sound and the versions of the stages that ran (stamp.rs; cargo tests
// pin the reader and the rule). When the list opens, the backend reads the
// stamps of the files its records name: a record whose render is older, or
// has no stamp, is set aside. Its row reads as not converted and says why,
// it can be converted again, and the player plays the source.

import test from 'node:test';
import assert from 'node:assert/strict';

/// A page whose document has nothing in it, enough to load the list's modules.
function bare() {
    const kept = new Map();
    globalThis.localStorage = { getItem: k => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, String(v)), removeItem: k => kept.delete(k) };
    const el = () => ({
        style: {}, classList: { add() {}, remove() {}, toggle() {}, contains: () => false }, dataset: {},
        addEventListener() {}, appendChild() {}, querySelector: () => null, querySelectorAll: () => [],
        setAttribute() {}, getAttribute: () => null,
    });
    globalThis.document = {
        getElementById: () => null, querySelector: () => null, querySelectorAll: () => [],
        createElement: el, addEventListener() {}, body: el(), documentElement: el(),
    };
    globalThis.window = globalThis.window ?? { addEventListener() {} };
    globalThis.requestAnimationFrame = () => 0;
    return kept;
}

const OLD = 'Made by 1.6.2 — convert again';

/// The backend's answer for each path: on disk, and whether it counts.
function checks(map) {
    return async (cmd, a) => {
        if (cmd !== 'player_render_checks') throw new Error('no ' + cmd);
        return a.paths.map(p => map[p] ?? { exists: true, valid: true, tip: null });
    };
}

test('records are sorted by the stamps of their files', async () => {
    bare();
    const { checkConvRecords } = await import(`../src/js/library.js?c=${Math.random()}`);
    const e = {
        path: 'a.mp3',
        convs: [{ sig: 's', out: 'new.flac' }, { sig: 's', out: 'old.flac' }, { sig: 's', out: 'gone.flac' }],
    };
    await checkConvRecords([e], checks({
        'old.flac': { exists: true, valid: false, tip: OLD },
        'gone.flac': { exists: false, valid: false, tip: null },
    }));
    assert.deepEqual(e.convs.map(c => c.out), ['new.flac']);
    assert.deepEqual(e.stale, [{ sig: 's', out: 'old.flac', tip: OLD }]);

    // Opened again: still set aside; a file under that name that counts now
    // (converted again elsewhere) brings the record back.
    await checkConvRecords([e], checks({ 'old.flac': { exists: true, valid: false, tip: OLD } }));
    assert.equal(e.stale.length, 1);
    await checkConvRecords([e], checks({}));
    assert.deepEqual(e.convs.map(c => c.out), ['new.flac', 'old.flac']);
    assert.equal(e.stale, undefined);
    assert.equal(e.convs[1].tip, undefined);

    // An older backend without the check: existence alone, as before.
    const f = { convs: [{ sig: 's', out: 'x.flac' }, { sig: 's', out: 'y.flac' }] };
    await checkConvRecords([f], async (cmd, a) => {
        if (cmd === 'player_paths_exist') return a.paths.map(p => p === 'x.flac');
        throw new Error('no ' + cmd);
    });
    assert.deepEqual(f.convs.map(c => c.out), ['x.flac']);
});

test('the row of an older render offers convert, no VERIFIED, and the player takes the source', async () => {
    const kept = bare();
    const lib = await import(`../src/js/library.js?p=${Math.random()}`);
    const sig = lib.rackSig();
    kept.set('auraPlaylists', JSON.stringify({
        version: 1, active: 1, plSeq: 2,
        playlists: [{ id: 1, name: 'A', entries: [
            { path: 'old.mp3', name: 'old.mp3', convs: [{ sig, out: 'old [AE].flac', labChain: [], badge: 0, rack: [] }] },
            { path: 'new.mp3', name: 'new.mp3', convs: [{ sig, out: 'new [AE].flac', labChain: [], badge: 0, rack: [] }] },
        ] }],
    }));
    let id = 1;
    globalThis.window = { addEventListener() {}, __TAURI__: { core: { invoke: async (cmd, a) => {
        if (cmd === 'player_render_checks') {
            return a.paths.map(p => (p === 'old [AE].flac'
                ? { exists: true, valid: false, tip: 'Made before 1.7.0 — convert again' }
                : { exists: true, valid: true, tip: null }));
        }
        if (cmd === 'player_queue') return [];
        if (cmd === 'player_add') return { tracks: a.paths.map(path => ({ id: id++, path })), errors: [] };
        return null;
    } } } };
    const listHooks = { nowPlaying: () => ({ id: null, state: 'stopped' }) };
    const { state } = await import('../src/js/state.js');
    await lib.initLibrary({
        listHooks,
        renderFileQueue: () => {},
        convLive: () => false,
        addFiles: (paths, entries) => entries.forEach(e => { e.trackId = id++; }),
    });
    const [oldRow, newRow] = state.list;
    assert.equal(oldRow.path, 'old.mp3');
    assert.deepEqual(oldRow.convs, []);
    assert.equal(oldRow.stale[0].tip, 'Made before 1.7.0 — convert again');

    // The player gets the source for the old one, the render for the new one.
    const rendered = listHooks.rendered();
    assert.equal(rendered.find(r => r.id === oldRow.trackId).path, null);
    assert.equal(rendered.find(r => r.id === newRow.trackId).path, 'new [AE].flac');

    // The rows: the old one is not converted (C offered, no lit VERIFIED)
    // and says why; the new one plays from disk.
    const { rowProps } = await import('../src/js/dropzone.js');
    const now = { id: null, state: 'stopped' };
    const o = rowProps(oldRow, now);
    assert.equal(o.isDisk, false);
    assert.equal(o.convertable, true);
    assert.equal(o.icon, '○');
    assert.equal(o.title, 'Made before 1.7.0 — convert again');
    assert.doesNotMatch(o.badgeHtml, /VERIFIED/);
    const n = rowProps(newRow, now);
    assert.equal(n.isDisk, true);
    assert.equal(n.convertable, false);

    // Converted again: the new render replaces the record set aside.
    lib.pushConvRecord(oldRow, { outPath: 'old [AE].flac', sig });
    assert.deepEqual(oldRow.stale, []);
    assert.equal(oldRow.convs[0].out, 'old [AE].flac');
    assert.equal(listHooks.rendered().find(r => r.id === oldRow.trackId).path, 'old [AE].flac');
});
