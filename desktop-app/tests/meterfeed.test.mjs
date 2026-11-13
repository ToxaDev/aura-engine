// The web remote's meter feed (web/remote/meterfeed.js): blocks of the
// bars and of HP's view, answered locally at the clock's frame.
import test from 'node:test';
import assert from 'node:assert/strict';
import { makeClock, clockAt, clockTake, bandsAt, synthView, synthLamp, parseViewBlock, createFeed, CLOCK_SNAP_MS, clockOffset, parseClockHeader } from '../../web/remote/meterfeed.js';
import { parseView } from '../src/js/hpview.js';

test('the clock follows answers a little at a time, and jumps on a seek', () => {
    const c = makeClock();
    assert.equal(clockTake(c, 1000, 0, 1, true, CLOCK_SNAP_MS), 'start');
    assert.equal(clockAt(c, 500), 1500);
    assert.equal(clockTake(c, 1540, 500, 1, true, CLOCK_SNAP_MS), 'follow');
    assert.ok(clockAt(c, 500) > 1500 && clockAt(c, 500) < 1540);
    assert.equal(clockTake(c, 9000, 600, 1, true, CLOCK_SNAP_MS), 'snap');
    assert.equal(clockAt(c, 600), 9000);
    assert.equal(clockTake(c, 9100, 700, 1, false, CLOCK_SNAP_MS), 'still');
    assert.equal(clockAt(c, 5000), 9100, 'paused: it stands');
});

test('the bars between two slices are drawn between them', () => {
    const s = new Map([[10, Uint8Array.from([0, 100])], [11, Uint8Array.from([100, 200])]]);
    assert.deepEqual([...bandsAt(s, 10.5, 2)], [50, 150]);
    assert.deepEqual([...bandsAt(s, 11.2, 2)], [100, 200], 'one side only: that slice');
    assert.equal(bandsAt(s, 20, 2), null);
});

function hpOf(bins, kind = 1) {
    return { kind, state: 0, hasEnv: 1, sens: 0, binMs: 5, bins: new Map(bins) };
}

test('a view made here reads as the engine\'s (hpview parseView), on the asked grid', () => {
    // A switch held over 100..150 ms of the grid's time.
    const bins = [];
    for (let i = 0; i < 200; i++) bins.push([i, [i >= 20 && i < 30 ? 255 : 0, i % 7]]);
    const hp = hpOf(bins);
    const u = synthView(hp, 400, -400, 400, 80);   // 10 ms bins, heard at 400 ms
    const v = parseView(u);
    assert.ok(v);
    assert.equal(v.n, 80);
    assert.equal(v.nowBin, 40);
    assert.equal(v.nowK, 40);
    assert.equal(v.frac, 0);
    // Bins 10..14 of the answer are 100..150 ms.
    assert.deepEqual([...v.w.slice(9, 16)], [0, 255, 255, 255, 255, 255, 0]);
    assert.equal(v.step, 0);
});

test('the lamp is the peak over the time asked for, on its kind\'s side', () => {
    const hp = hpOf([[20, [0, 0]], [21, [180, 0]], [22, [0, 0]]]);
    assert.deepEqual([...synthLamp(hp, 112, 10)], [180, 0, 0]);
    assert.deepEqual([...synthLamp(hp, 140, 10)], [0, 0, 0]);
    assert.deepEqual([...synthLamp(hpOf([[21, [90, 0]]], 2), 110, 8)], [0, 90, 0]);
});

test('the engine\'s view answer is read with its now bin', () => {
    const u = synthView(hpOf([[5, [10, 20]]]), 60, -60, 60, 12);
    const v = parseViewBlock(u.buffer);
    assert.equal(v.nowBin, 6);
    assert.equal(v.n, 12);
});

/// A fake engine: the heard frame runs at 48 kHz from page time 0; the ring
/// is written 3 s ahead; band 0 of each slice is its grid index & 255.
function engine(clock) {
    const rate = 48000, step = 960;
    const calls = [];
    const fetch = async (url) => {
        calls.push(url);
        const t = clock.t;
        const audible = Math.round(t / 1000 * rate);
        const q = new URLSearchParams(url.split('?')[1]);
        if (url.includes('spectrum_span')) {
            const bands = +q.get('bands');
            const lo = audible + +q.get('from_ms') / 1000 * rate, hi = audible + +q.get('to_ms') / 1000 * rate;
            const k0 = Math.ceil(Math.max(0, lo) / step), k1 = Math.floor(hi / step), n = k1 - k0 + 1;
            const buf = new ArrayBuffer(80 + n * (1 + bands));
            const dv = new DataView(buf);
            [audible, rate, step, k0, n, 1, 0, -1, audible + 3 * rate, 0].forEach((v, i) => dv.setFloat64(i * 8, v, true));
            for (let j = 0; j < n; j++) {
                const o = 80 + j * (1 + bands);
                const written = (k0 + j) * step <= audible + 3 * rate;
                dv.setUint8(o, written ? 1 : 0);
                dv.setUint8(o + 1, (k0 + j) & 255);
            }
            return new Response(buf);
        }
        return new Response(new ArrayBuffer(0));
    };
    return { fetch, calls };
}

test('the feed answers the page\'s frames from a block, and asks the network rarely', async () => {
    const clock = { t: 1000 };
    const e = engine(clock);
    const feed = createFeed({ fetch: e.fetch, now: () => clock.t });
    const ask = () => feed.handle('/aura/player/spectrum?bands=4&ahead_ms=0').then((r) => r.arrayBuffer()).then((b) => new Uint8Array(b));
    await ask();                                   // the first: nothing yet, a block goes
    await new Promise((r) => setTimeout(r, 5));
    let served = 0;
    for (let f = 0; f < 120; f++) {               // two seconds of frames
        clock.t += 1000 / 60;
        const b = await ask();
        await new Promise((r) => setImmediate(r));
        if (!b.length) continue;
        served++;
        // The slice of the heard frame: (t / 20 ms) & 255, within one slice.
        const want = Math.floor(clock.t / 20) & 255;
        assert.ok(Math.abs(b[0] - want) <= 1 || Math.abs(b[0] - want) >= 254, `${b[0]} vs ${want} at ${clock.t}`);
    }
    assert.ok(served > 110, `served ${served}`);
    assert.ok(e.calls.length <= 4, `${e.calls.length} requests in 2 s`);
});

test('a seek takes a block at once', async () => {
    const clock = { t: 1000 };
    const e = engine(clock);
    const feed = createFeed({ fetch: e.fetch, now: () => clock.t });
    await feed.handle('/aura/player/spectrum?bands=4&ahead_ms=0');
    await new Promise((r) => setTimeout(r, 5));
    const before = e.calls.length;
    clock.t += 100;
    feed.kick();
    await new Promise((r) => setTimeout(r, 5));
    assert.ok(e.calls.length > before);
});

test('the server\'s clock: bounded both ways from the quickest way up and the quickest way down', () => {
    // The server runs 5000 ms ahead of the page. One answer quick up and
    // slow down (a phone's Wi-Fi asleep), one slow up and quick down.
    const a = { sent: 0, read: 5002, send: 5002, got: 82 };
    const b = { sent: 100, read: 5140, send: 5140, got: 142 };
    assert.equal(clockOffset([a, b]), 5000);
    // Either alone: its round trip's middle, as before.
    assert.equal(clockOffset([a]), 5002 - 41);
    assert.equal(clockOffset([]), null);
    assert.deepEqual(parseClockHeader('12.5;13.25'), [12.5, 13.25]);
    assert.equal(parseClockHeader('x'), null);
    assert.equal(parseClockHeader(null), null);
});

test('a block slow on its way back is timed by the server\'s clock, not its round trip', async () => {
    const clock = { t: 1000 };
    const rate = 48000, step = 960, theta = 5000;
    let readAt = 0;
    const fetch = async (url) => {
        // Taken up at once; the answer 60 ms on its way back.
        readAt = clock.t;
        const audible = Math.round(readAt / 1000 * rate);
        const q = new URLSearchParams(url.split('?')[1]);
        const k0 = Math.ceil(Math.max(0, audible + +q.get('from_ms') / 1000 * rate) / step), n = 50;
        const buf = new ArrayBuffer(80 + n * 5);
        const dv = new DataView(buf);
        [audible, rate, step, k0, n, 1, 0, -1, audible + 3 * rate, 0].forEach((v, i) => dv.setFloat64(i * 8, v, true));
        const read = readAt + theta;
        clock.t += 60;
        return new Response(buf, { headers: { 'x-aura-clock': `${read};${read}` } });
    };
    const feed = createFeed({ fetch, now: () => clock.t });
    // The status poll, quick both ways.
    feed.noteClock(900, 902, `${900 + 1 + theta};${900 + 1 + theta}`);
    await feed.handle('/aura/player/spectrum?bands=4&ahead_ms=0');
    await new Promise((r) => setTimeout(r, 5));
    assert.ok(feed.spec.clock.synced);
    const err = clockAt(feed.spec.clock, readAt) - Math.round(readAt / 1000 * rate);
    // Within a ms (48 frames); the round trip's middle was 30 ms late.
    assert.ok(Math.abs(err) <= 48, `off by ${err} frames`);
});
