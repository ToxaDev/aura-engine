// The input's view (input.js): one card from the status — the driver not
// installed, no player yet, a player connected, playing — and the words the
// strip and the big player say while it plays.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { readFileSync } from 'node:fs';

import { INPUT_TEXTS, OFFER_KEY, OFFER_MISSING_KEY, MISSING_OFFER, offerKind, keptOffers, markOffered, offerView, inputCard, inputLines, inputStateText, inputStatus, inputTech, formatLine, sampleTypeText } from '../src/js/input.js';

const foobar = { name: 'foobar2000', running: true };
const hiRes = { rate: 96_000, channels: 2, sampleType: 18 };

test('no driver: say so, and offer to install it', () => {
    for (const driver of ['missing', undefined]) {
        const c = inputCard({ driver });
        assert.equal(c.kind, 'off');
        assert.equal(c.title, INPUT_TEXTS.missing.title);
        assert.equal(c.act, 'install');
        assert.equal(c.button, 'Install driver');
    }
    assert.equal(inputCard(null).act, 'install', 'no status yet reads as no driver');
});

test('a driver older than the app: update it — unless a player is on it now', () => {
    assert.equal(inputCard({ driver: 'stale' }).act, 'install');
    assert.equal(inputCard({ driver: 'stale' }).button, 'Update driver');
    assert.equal(inputCard({ driver: 'stale', client: foobar }).act, 'listen');
});

test('a player on the input is said first, whatever the registry says (16.10 window: feed, driver unregistered)', () => {
    const playing = inputCard({ driver: 'missing', client: foobar, format: hiRes, listening: true });
    assert.equal(playing.title, 'foobar2000', 'not "isn\'t installed" while it plays');
    assert.equal(playing.kind, 'on');
    assert.equal(inputCard({ driver: 'missing', client: foobar }).act, 'listen');
    assert.equal(inputCard({ driver: 'missing', client: { name: 'Roon', running: false } }).title, 'Roon');
    assert.equal(inputCard({ driver: 'missing', client: null }).act, 'install', 'no player: the driver is what is missing');
});

test('the driver there and no player: how to choose the device in each player', () => {
    const c = inputCard({ driver: 'installed', client: null });
    assert.equal(c.kind, 'wait');
    assert.equal(c.title, 'Waiting for a player');
    assert.equal(c.hints, true);
    assert.equal(c.act, null);
    assert.deepEqual(INPUT_TEXTS.hints.map(h => h[0]), ['foobar2000', 'JRiver', 'HQPlayer', 'Roon']);
    for (const [, how] of INPUT_TEXTS.hints) assert.match(how, /Aura Player ASIO/);
});

test('a player connected, stopped: press play there', () => {
    const c = inputCard({ driver: 'installed', client: { name: 'JRiver', running: false } });
    assert.equal(c.title, 'JRiver');
    assert.match(c.text, /Press play in JRiver/);
    assert.equal(c.hints, false);
});

test('a player playing that Aura Player does not listen to: Listen', () => {
    const c = inputCard({ driver: 'installed', client: foobar, listening: false });
    assert.equal(c.title, 'foobar2000 is playing');
    assert.equal(c.act, 'listen');
    assert.equal(c.button, 'Listen');
});

test('listening: the player\'s name, what comes in, playing or paused', () => {
    const on = inputCard({ driver: 'installed', client: foobar, format: hiRes, listening: true });
    assert.equal(on.kind, 'on');
    assert.equal(on.title, 'foobar2000');
    assert.equal(on.text, '96 kHz · 32-bit · 2 ch · Playing');
    const held = inputCard({ driver: 'installed', client: foobar, format: hiRes, listening: true, held: true });
    assert.equal(held.kind, 'wait');
    assert.match(held.text, /Paused$/);
});

test('installing: the card waits for Windows', () => {
    const c = inputCard({ driver: 'missing' }, 'install');
    assert.equal(c.kind, 'busy');
    assert.equal(c.title, 'Installing…');
    assert.equal(c.button, null);
});

test('installing without the driver at hand: the card says it is downloading it first', () => {
    const c = inputCard({ driver: 'missing', installing: 'download' }, 'install');
    assert.equal(c.kind, 'busy');
    assert.equal(c.title, 'Downloading the driver…');
    assert.equal(inputCard({ driver: 'stale', installing: 'install' }, 'install').title, 'Installing…');
    assert.equal(inputCard({ driver: 'missing', installing: 'download' }).act, 'install', 'not busy here: the card offers the install');
    for (const w of ['cancelled', 'failed', 'network', 'mismatch']) assert.ok(INPUT_TEXTS.err[w], w);
});

test('no Windows: the input is not there', () => {
    const c = inputCard({ driver: 'unsupported' });
    assert.equal(c.act, null);
    assert.equal(c.button, null);
});

test('the format in words', () => {
    assert.equal(formatLine({ rate: 44_100, channels: 2, sampleType: 18 }), '44.1 kHz · 32-bit · 2 ch');
    assert.equal(formatLine({ rate: 352_800, channels: 2, sampleType: 19 }), '352.8 kHz · 32-bit float · 2 ch');
    assert.equal(formatLine({ rate: 768_000, channels: 2, sampleType: 99 }), '768 kHz · 2 ch', 'an unknown type is left out');
    assert.equal(formatLine(null), '');
    assert.equal(formatLine({ rate: 0 }), '');
    assert.deepEqual([16, 17, 18, 19, 20].map(sampleTypeText), ['16-bit', '24-bit', '32-bit', '32-bit float', '64-bit float']);
});

test('the strip and the big player: the player\'s name, the format, INPUT — only while listening', () => {
    assert.deepEqual(inputLines({ input: { listening: true, client: foobar, format: hiRes } }),
        { title: 'foobar2000', sub: '96 kHz · 32-bit · 2 ch', badge: 'INPUT' });
    assert.equal(inputLines({ input: { listening: false, client: foobar } }), null);
    assert.equal(inputLines({}), null);
    assert.equal(inputLines({ input: { listening: true } }).title, 'Aura Player ASIO');
    assert.equal(inputStateText({ input: { listening: true, held: true, client: foobar } }), 'Paused');
    assert.match(inputStateText({ input: { listening: true, client: { name: 'Roon', running: false } } }), /Press play in Roon/);
    assert.equal(inputStateText({ input: { listening: true, client: foobar } }), null);
});

test('Spotify and other Windows apps: the way through Hi-Fi Cable and ASIO Bridge, and where to get it (Anton 17.10)', () => {
    const B = INPUT_TEXTS.bridge;
    assert.equal(B.steps.length, 3);
    assert.match(B.steps[1], /Hi-Fi Cable Input/);
    assert.match(B.steps[2], /Aura Player ASIO/);
    assert.equal(B.url, 'https://vb-audio.com/Cable/index.htm#DownloadASIOBridge');
    // Under the players' hints: shown while waiting for a player only.
    assert.equal(inputCard({ driver: 'installed' }).hints, true);
    assert.equal(inputCard({ driver: 'missing' }).hints, false);
    const src = readFileSync(new URL('../src/js/input.js', import.meta.url), 'utf8');
    assert.match(src, /hints\.appendChild\(bridge\)/);
});

test('the words: English', () => {
    const all = JSON.stringify(INPUT_TEXTS) + INPUT_TEXTS.connected('x').text + INPUT_TEXTS.notListening('x').text;
    assert.ok(!/[Ѐ-ӿ]/.test(all));
});

test("the status line: the input's failure in the backend's words, else its state, as a note", () => {
    const listening = { listening: true, client: foobar, format: hiRes };
    assert.equal(inputStatus({ input: { ...listening, listening: false } }), null, "not listened to: the radio's line");
    assert.equal(inputStatus({ input: listening, state: 'playing', radio: { phase: 'playing' } }), null, 'plays: nothing to say');
    assert.deepEqual(inputStatus({ input: { ...listening, held: true } }), { text: 'Paused', kind: 'note' });
    assert.deepEqual(inputStatus({ input: { ...listening, client: { name: 'JRiver', running: false } } }),
        { text: 'Connected. Press play in JRiver.', kind: 'note' });
    const failed = { input: listening, radioError: 'rate', error: 'Input: 705600 Hz plays BIT-PERFECT only' };
    assert.deepEqual(inputStatus(failed), { text: failed.error, kind: 'error' });
    assert.deepEqual(inputStatus({ input: listening, radio: { phase: 'failed', info: { error: 'x' } } }), { text: 'Input: x', kind: 'error' });
});

test('what comes in and what the rack plays it at', () => {
    const s = { input: { listening: true, client: foobar, format: hiRes }, state: 'playing', outRate: 768_000, radio: { src: 'input' } };
    assert.equal(inputTech(s), '96 kHz · 32-bit → 768 kHz');
    assert.equal(inputTech({ ...s, streamDirect: true, outRate: 96_000 }), '96 kHz · 32-bit · bit-perfect');
    assert.equal(inputTech({ ...s, state: 'preparing', radio: null }), '96 kHz · 32-bit', 'before its chain: what comes in');
    assert.equal(inputTech({ input: { listening: true } }), '');
});

test("player.js: the input's view, its status, ▶ after its ■, and the big player wired in", () => {
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(src, /import \{[^}]*initInput[^}]*inputRender[^}]*\} from '\.\/input\.js'/);
    assert.match(src, /initInput\(\{ call: invoke \}\)/, 'its commands go to the backend');
    assert.match(src, /function render\(\) \{[\s\S]{0,200}inputRender\(s\)/, 'each status reaches its view');
    assert.match(src, /lastSource === 'input'\) \{ invoke\('player_input_listen'\)/, '▶ after its ■ listens again');
    assert.match(src, /status\.input\?\.listening\) lastSource = 'input'/);
    assert.match(src, /s\?\.input\?\.listening \? inputStatus\(s\) : radioStateText\(s\)/, "the station's words are not the input's");
    const big = readFileSync(new URL('../src/js/listening.js', import.meta.url), 'utf8');
    assert.match(big, /const il = inputLines\(s\);\s*if \(il \|\| s\?\.radio\)/);
});

const memory = () => {
    const kept = new Map();
    return { kept, getItem: k => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, String(v)) };
};

test('after an update: a driver older than this version is offered an update, once per version', () => {
    assert.equal(offerKind({ driver: 'stale' }, '1.7.0', { stale: '1.6.0' }), 'update');
    assert.equal(offerKind({ driver: 'stale' }, '1.7.0', {}), 'update', 'never offered before');
    assert.equal(offerKind({ driver: 'stale' }, '1.7.0', { stale: '1.7.0' }), null, 'asked in this version');
    assert.equal(offerKind({ driver: 'stale', client: foobar }, '1.7.0', {}), 'update', 'a connected player keeps the old one: still worth it');
    const s = memory();
    assert.equal(offerKind({ driver: 'stale' }, '1.7.0', keptOffers(s)), 'update');
    markOffered(s, 'update', '1.7.0');
    assert.equal(s.kept.get(OFFER_KEY), '1.7.0');
    assert.equal(offerKind({ driver: 'stale' }, '1.7.0', keptOffers(s)), null, 'Later: not again in this version');
    assert.equal(offerKind({ driver: 'stale' }, '1.8.0', keptOffers(s)), 'update', 'the next update asks again');
});

test('after an update: no driver at all — offered to install it, once ever (Anton 18.10)', () => {
    assert.equal(MISSING_OFFER, 'once');
    const s = memory();
    assert.equal(offerKind({ driver: 'missing' }, '1.6.1', keptOffers(s)), 'install');
    markOffered(s, 'install', '1.6.1');
    assert.equal(s.kept.get(OFFER_MISSING_KEY), '1.6.1');
    assert.equal(s.kept.get(OFFER_KEY), undefined, "the update's own mark untouched");
    assert.equal(offerKind({ driver: 'missing' }, '1.6.1', keptOffers(s)), null, 'the next launch: not again');
    assert.equal(offerKind({ driver: 'missing' }, '1.7.0', keptOffers(s)), null, 'once ever: not after the next update either');
    assert.equal(offerKind({ driver: 'missing' }, '1.7.0', keptOffers(s), null, 'perVersion'), 'install', 'per version: asked again after an update');
    assert.equal(offerKind({ driver: 'missing' }, '1.6.1', keptOffers(s), null, 'perVersion'), null, 'per version: once in it');
    assert.equal(offerKind({ driver: 'missing', client: foobar }, '1.6.1', {}), null, 'a player on the input: the driver is there');
    assert.equal(offerKind({ driver: 'stale' }, '1.6.1', keptOffers(s)), 'update', 'installed later and then stale: its own rule');
});

test("the offer: never for a driver that is this version's or not for this system, nor while busy", () => {
    for (const driver of ['installed', 'unsupported', undefined]) {
        assert.equal(offerKind({ driver }, '1.7.0', {}), null, `driver ${driver}`);
        assert.equal(offerKind({ driver }, '1.7.0', {}, null, 'perVersion'), null, `driver ${driver}, per version`);
    }
    assert.equal(offerKind(null, '1.7.0', {}), null, 'no status yet');
    for (const driver of ['stale', 'missing']) {
        assert.equal(offerKind({ driver }, null, {}), null, 'the version not known yet');
        assert.equal(offerKind({ driver }, '', {}), null);
        assert.equal(offerKind({ driver, listening: true, client: foobar }, '1.7.0', {}), null, 'not while a player plays into it');
        assert.equal(offerKind({ driver, installing: 'download' }, '1.7.0', {}), null, 'not while it downloads');
        assert.equal(offerKind({ driver, installing: 'install' }, '1.7.0', {}), null, 'not while it installs');
        assert.equal(offerKind({ driver }, '1.7.0', {}, 'install'), null, 'not while the I view installs it');
    }
    const broken = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
    assert.deepEqual(keptOffers(broken), { stale: null, missing: null });
    assert.doesNotThrow(() => markOffered(broken, 'install', '1.7.0'));
    assert.deepEqual(keptOffers(null), { stale: null, missing: null });
});

test("the offers' words: ask, downloading / installing, done, the install's errors", () => {
    const up = offerView('update', 'ask');
    assert.equal(up.title, 'Update Aura Player ASIO?');
    assert.match(up.text, /^This Aura Player brings a newer ASIO driver\..*Windows will ask for permission\.$/);
    assert.deepEqual(up.buttons, [['later', 'Later'], ['install', 'Update driver']]);
    assert.equal(offerView('update', 'ok').text, 'Updated. Restart the player that uses Aura Player ASIO.');
    const inst = offerView('install', 'ask');
    assert.equal(inst.title, 'Install Aura Player ASIO?');
    assert.match(inst.text, /^With this driver, foobar2000, JRiver, HQPlayer, Roon and other ASIO players — and Spotify through ASIO Bridge — play through Aura Player\. Windows will ask for permission\.$/);
    assert.deepEqual(inst.buttons, [['later', 'Not now'], ['install', 'Install driver']]);
    assert.equal(inst.buttons[1][1], INPUT_TEXTS.missing.button, "the I view's own button");
    assert.equal(offerView('install', 'ok').text, 'Installed. In your player, choose Aura Player ASIO as the output.');
    for (const kind of ['update', 'install']) {
        assert.equal(offerView(kind, 'busy', 'download').text, 'Downloading the driver…');
        assert.equal(offerView(kind, 'busy', 'install').text, INPUT_TEXTS.installing);
        assert.deepEqual(offerView(kind, 'busy').buttons, [], 'no way out while Windows asks');
        assert.deepEqual(offerView(kind, 'ok').buttons, [['close', 'Close']]);
        for (const w of ['cancelled', 'network', 'mismatch', 'failed']) {
            assert.equal(offerView(kind, w).text, INPUT_TEXTS.err[w]);
            assert.deepEqual(offerView(kind, w).buttons, [['close', 'Close']]);
        }
        assert.equal(offerView(kind, 'weird').text, INPUT_TEXTS.err.failed);
    }
    assert.equal(INPUT_TEXTS.err.network, "Couldn't download the driver. Check the connection and try again.");
    assert.equal(INPUT_TEXTS.err.mismatch, "The driver didn't match this version — nothing was installed.");
});

test("the I view while the driver is fetched: Downloading the driver…, then the error's words", () => {
    assert.equal(inputCard({ driver: 'missing', installing: 'download' }, 'install').title, 'Downloading the driver…');
    assert.equal(inputCard({ driver: 'missing', installing: 'install' }, 'install').title, 'Installing…');
    assert.equal(inputCard({ driver: 'missing', installing: 'download' }, 'install').button, null);
    const src = readFileSync(new URL('../src/js/input.js', import.meta.url), 'utf8');
    assert.match(src, /note = r === 'ok' \? null : INPUT_TEXTS\.err\[r\] \|\| INPUT_TEXTS\.err\.failed;/);
});

test('input.js: the offer opens from the status, after the update dialog, marked as it opens', () => {
    const src = readFileSync(new URL('../src/js/input.js', import.meta.url), 'utf8');
    assert.match(src, /if \(up && !up\.classList\.contains\('up-hidden'\)\) return;/);
    assert.match(src, /offered = true;\s*markOffered\(storage\(\), kind, version\);\s*offerOf = kind;\s*offer = 'ask';/);
    assert.match(src, /offer = await install\(\);/, "the I view's install path");
    assert.match(src, /if \(offer === 'busy'\) renderOffer\(\);/, 'Downloading… follows the status');
    assert.match(readFileSync(new URL('../src/css/input.css', import.meta.url), 'utf8'), /#inOffer \{/);
});
