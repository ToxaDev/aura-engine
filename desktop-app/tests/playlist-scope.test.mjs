// What plays stays in its playlist (Anton 16.10): "the playlist ends and
// sometimes jumps to another playlist". The player's queue holds the list
// shown; switching the list shown while a track played kept that track and
// queued the other playlist's tracks after it, so at its end the player went
// on into them.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// The player here is a model of its queue (controller.rs): player_add
// appends the tracks under the playlist named, player_remove takes one out,
// and what plays at a track's end is the next of its own playlist's tracks
// (`next_in_list` / `follows`; a cargo test pins that rule).

import test from 'node:test';
import assert from 'node:assert/strict';

import { state } from '../src/js/state.js';

/// The player's queue and what follows each track in it.
function player() {
    const queue = [];   // { id, path, list }
    let seq = 1;
    async function invoke(cmd, a = {}) {
        if (cmd === 'player_add') {
            const tracks = a.paths.map(path => ({ id: seq++, path, list: a.list ?? null }));
            queue.push(...tracks);
            return { tracks: tracks.map(t => ({ ...t })), errors: [] };
        }
        if (cmd === 'player_remove') {
            const i = queue.findIndex(t => t.id === a.id);
            if (i >= 0) queue.splice(i, 1);
            return null;
        }
        if (cmd === 'player_queue') return queue.map(t => ({ ...t }));
        if (cmd === 'player_paths_exist') return a.paths.map(() => true);
        return null;
    }
    /// What plays at the end of track `id` with this repeat mode.
    function follows(id, repeat = 'off') {
        if (repeat === 'one') return id;
        const t = queue.find(x => x.id === id);
        if (!t) return null;   // the track left the list: the player stops
        const mates = queue.filter(x => x.list === t.list);
        const i = mates.indexOf(t);
        return mates[i + 1]?.id ?? (repeat === 'all' ? mates[0].id : null);
    }
    return { queue, invoke, follows };
}

/// A page with these playlists saved (the first one shown), its library
/// started, and the player's ids on its rows. `lib` is a fresh copy of
/// library.js for each page.
async function page(playlists, { backend = player(), active = playlists[0].id } = {}) {
    const kept = new Map([['auraPlaylists', JSON.stringify({
        version: 1, active, plSeq: playlists.length + 1,
        playlists: playlists.map(p => ({ id: p.id, name: p.name, entries: p.paths.map(path => ({ path, name: path, convs: [] })) })),
    })]]);
    globalThis.localStorage = { getItem: k => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, String(v)) };
    globalThis.document = { getElementById: () => null, querySelector: () => null };
    globalThis.requestAnimationFrame = () => 0;
    globalThis.window = { __TAURI__: { core: { invoke: backend.invoke } } };
    const lib = await import(`../src/js/library.js?page=${Math.random()}`);
    const now = { id: null, state: 'stopped' };
    const adding = [];
    // What player.js's onAdded does with the rows the list adds: the
    // player's ids, under the playlist shown; rows let go meanwhile take
    // their tracks out again.
    async function onAdded(entries) {
        const r = await backend.invoke('player_add', { paths: entries.map(e => e.path), list: lib.getActiveId() });
        for (const t of r.tracks) {
            const e = entries.find(x => x.path === t.path && x.trackId == null);
            if (e) { e.trackId = t.id; e.info = t; }
        }
        for (const e of entries) {
            if (e.trackId != null && lib.inPlaylists && !lib.inPlaylists(e.trackId)) {
                backend.invoke('player_remove', { id: e.trackId });
            }
        }
    }
    await lib.initLibrary({
        listHooks: { nowPlaying: () => ({ ...now }) },
        renderFileQueue: () => {},
        convLive: () => false,
        addFiles: (paths, entries) => { adding.push(onAdded(entries)); },
    });
    const settle = () => Promise.all(adding.splice(0));
    await settle();
    /// The track of the row with this path in the list shown.
    const id = path => state.list.find(e => e.path === path)?.trackId;
    /// The player turns to a track (its status, seen by player.js).
    const plays = (trackId, st = 'playing') => {
        now.id = trackId; now.state = st;
        lib.playerMoved?.({ id: trackId, state: st });
    };
    /// Show another playlist, and let the player read its files.
    const show = async plId => { lib.switchPlaylist(plId); await settle(); };
    return { lib, backend, id, plays, show, settle };
}

const A = { id: 1, name: 'A', paths: ['a1.flac', 'a2.flac', 'a3.flac'] };
const B = { id: 2, name: 'B', paths: ['b1.flac', 'b2.flac'] };

test('a playlist that plays goes on in itself while another one is shown', async () => {
    const { backend, id, plays, show } = await page([A, B]);
    const [a1, a2, a3] = A.paths.map(id);
    plays(a2);

    await show(B.id);
    const [b1, b2] = B.paths.map(id);
    assert.ok(b1 != null && b2 != null, 'the playlist shown can be played');
    // Before 16.10: a2 → b1.
    assert.equal(backend.follows(a2), a3, 'the next track of A');
    assert.equal(backend.follows(a3), null, 'the end of A: stop');
    assert.equal(backend.follows(a3, 'all'), a1, 'repeat all: A again');
    assert.equal(backend.follows(a2, 'one'), a2, 'repeat one: the same track');
    // However long it goes round, A never runs into B, nor B into A.
    for (const repeat of ['off', 'all']) {
        let t = a1;
        for (let i = 0; i < 10 && t != null; i++) {
            assert.ok([a1, a2, a3].includes(t), `A, repeat ${repeat}`);
            t = backend.follows(t, repeat);
        }
    }
    assert.equal(backend.follows(b2), null);
    assert.equal(backend.follows(b2, 'all'), b1);

    // A shown again: its own rows and tracks, nothing read twice; B, which
    // does not play, leaves the player.
    await show(A.id);
    assert.deepEqual(A.paths.map(id), [a1, a2, a3]);
    assert.deepEqual(backend.queue.map(t => t.id), [a1, a2, a3]);
    assert.equal(backend.follows(a3), null);
});

test('a playlist that played is let go once a file of another one plays', async () => {
    const { backend, id, plays, show } = await page([A, B]);
    const [, a2] = A.paths.map(id);
    plays(a2);
    await show(B.id);
    const [b1, b2] = B.paths.map(id);

    // Paused, or still on A while b1 gets ready: A stays.
    plays(a2, 'paused');
    assert.equal(backend.queue.length, 5);
    // b1 played from the list shown: A's tracks leave the player.
    plays(b1);
    assert.deepEqual(backend.queue.map(t => t.id), [b1, b2]);
    assert.equal(backend.follows(b1), b2);
    assert.equal(backend.follows(b2), null, 'the end of B: stop');
});

test('a new playlist made while another one plays: that one goes on', async () => {
    const { lib, backend, id, plays, show } = await page([A, B]);
    const [a1, a2, a3] = A.paths.map(id);
    plays(a1);
    // "+ New playlist": a new empty one, shown.
    await show(lib.createPlaylist('C'));
    assert.equal(state.list.length, 0);
    assert.equal(backend.follows(a1), a2);
    assert.equal(backend.follows(a3), null);
    assert.equal(backend.follows(a3, 'all'), a1);
});

test('a playlist deleted while it plays: its track plays to its end, nothing follows', async () => {
    const { lib, backend, id, plays, show } = await page([A, B]);
    const [, a2] = A.paths.map(id);
    plays(a2);
    await show(B.id);
    lib.deletePlaylist(A.id);
    assert.equal(backend.follows(a2), null);
    assert.ok(backend.queue.every(t => t.list === B.id));
});

test('a playlist not playing leaves the player when another one is shown', async () => {
    const { backend, show } = await page([A, B]);
    await show(B.id);
    assert.deepEqual(backend.queue.map(t => t.path), B.paths);
});

test('a page reloaded takes back only its own playlist\'s tracks', async () => {
    // The player still holds A, which played while B was shown, and B.
    const backend = player();
    await backend.invoke('player_add', { paths: ['x.flac', 'b1.flac'], list: A.id });
    const [xa, ba] = backend.queue.map(t => t.id);
    await backend.invoke('player_add', { paths: ['b1.flac', 'b2.flac'], list: B.id });
    const [b1, b2] = backend.queue.slice(2).map(t => t.id);
    const { id } = await page([A, B], { backend, active: B.id });
    assert.equal(id('b1.flac'), b1, 'B\'s own b1, not A\'s');
    assert.equal(id('b2.flac'), b2);
    assert.ok(!backend.queue.some(t => t.id === xa || t.id === ba), 'A\'s tracks leave the player');
});
