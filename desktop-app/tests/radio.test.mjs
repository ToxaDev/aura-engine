// The radio's words and lines (radio.js): what the strip, the big player
// and the panel say of a stream from the player's status.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    STATIONS, RADIO_TEXTS, stationOf, isStreamAddress, stationName, radioTech, radioLines, radioStateText, stepStation,
    normStore, stationKey, isFavorite, toggleFavorite, addRecent, noteHeard, markFailed, playsHere, RECENT_KEEP,
    techLine, rowSub, initials, tileHue, searchKey, codecName, warmKeyOf, warmRateOf,
    nameRank, searchOrder, alphabetical, pickerItems, QUALITIES, qualityText, isLossless, SORTS, sortFromSaved,
} from '../src/js/radio.js';

const rp = 'https://stream.radioparadise.com/flac';

test('the stations are the ones agreed, the lossless ones first, and no SomaFM', () => {
    assert.equal(STATIONS.length, 10);
    assert.deepEqual(STATIONS.map(s => s.tech.split(' ')[0]), ['FLAC', 'FLAC', 'FLAC', 'FLAC', 'FLAC', 'FLAC', 'AAC', 'AAC', 'AAC', 'AAC']);
    assert.ok(STATIONS.every(s => s.name && /^https:\/\//.test(s.url) && s.tech));
    assert.ok(!STATIONS.some(s => /soma/i.test(s.url + s.name)));
    assert.equal(stationOf(rp + '/').name, 'Radio Paradise — Main Mix');
    assert.equal(stationOf('HTTPS://ICECAST.RADIOFRANCE.FR/fipjazz-hifi.aac').name, 'FIP Jazz');
    assert.equal(stationOf('https://example.org/live'), null);
});

test('a stream address starts with http:// or https://', () => {
    assert.ok(isStreamAddress(' https://a.example/x.pls '));
    assert.ok(isStreamAddress('http://a.example:8000/live'));
    assert.ok(!isStreamAddress('a.example/live'));
    assert.ok(!isStreamAddress('ftp://a.example/x'));
    assert.ok(!isStreamAddress(''));
});

test('a stream is named by the list, else by its server, else by its host', () => {
    assert.equal(stationName({ info: { url: rp, icyName: 'Radio Paradise (flac)' } }), 'Radio Paradise — Main Mix');
    assert.equal(stationName({ info: { url: 'http://x.example:8000/a', icyName: ' Jazz 24 ' } }), 'Jazz 24');
    assert.equal(stationName({ info: { url: 'http://x.example:8000/a', icyName: '' } }), 'x.example:8000');
});

test('the lines: what is heard, else the station; the codec and the rates', () => {
    const s = {
        state: 'playing', outRate: 88200,
        radio: { phase: 'playing', now: { text: 'Agnes Obel - The Curse', album: 'Aventine · 2013' },
            info: { url: rp, codec: 'flac', rate: 44100, bits: 16 } },
    };
    assert.deepEqual(radioLines(s), {
        station: 'Radio Paradise — Main Mix', title: 'Agnes Obel - The Curse',
        album: 'Aventine · 2013', tech: 'FLAC 44.1 kHz/16 → 88.2 kHz',
    });
    const fip = { state: 'preparing', outRate: 0,
        radio: { phase: 'connecting', now: null, info: { url: STATIONS[6].url, codec: 'aac', rate: 48000 } } };
    assert.deepEqual(radioLines(fip), { station: 'FIP', title: 'FIP', album: '', tech: 'AAC 48 kHz' });
    assert.equal(radioTech({ radio: { info: {} } }), '', 'no format before the first audio');
    // BIT-PERFECT: as a file's line says it, the codec first.
    const bp = (info, format) => radioTech({ state: 'playing', outRate: info.rate, streamDirect: true, exclusive: true, format,
        radio: { info } });
    assert.equal(bp({ codec: 'flac', rate: 44100, bits: 16 }, 'int16'), 'FLAC · bit-perfect · 44.1 kHz/16 · exclusive int16');
    assert.equal(bp({ codec: 'aac', rate: 48000 }, 'int32'), 'AAC · bit-perfect · 48 kHz · exclusive int32');
    assert.equal(radioTech({ state: 'paused', outRate: 88200, streamDirect: false, radio: { info: { codec: 'flac', rate: 44100, bits: 16 } } }),
        'FLAC 44.1 kHz/16 → 88.2 kHz', 'through the rack as before');
    assert.equal('bitperfect' in RADIO_TEXTS.err, false, 'the radio plays in BIT-PERFECT: no words for refusing it');
    // HE-AAC by its own name (the codecs' words, the HE-AAC decoder's).
    const he = c => radioTech({ state: 'stopped', radio: { info: { codec: c, rate: 48000 } } });
    assert.equal(he('he-aac'), 'HE-AAC 48 kHz');
    assert.equal(he('he-aac v2'), 'HE-AAC v2 48 kHz');
    assert.equal(he('HE-AAC v2'), 'HE-AAC v2 48 kHz');
    assert.equal(he('opus'), 'Opus 48 kHz');
    // A pasted stream's record keeps the same name, not capitals.
    const kept = noteHeard(normStore({ recent: [{ name: 'x.example', url: 'http://x.example/he' }] }), 'http://x.example/he',
        { rate: 48000, codec: 'HE-AAC v2' });
    assert.equal(kept.recent[0].codec, 'HE-AAC v2');
    assert.equal(codecName('mp3'), 'MP3');
    assert.equal(codecName('wma'), 'WMA', 'a codec without a name of its own: capitals');
});

test('where the stream is, in the agreed words', () => {
    const at = (phase, extra = {}) => radioStateText({ radio: { phase, info: { url: rp }, ...extra } });
    assert.deepEqual(at('connecting'), { text: 'Tuning in…', kind: 'busy' });
    assert.deepEqual(at('waiting', { waitS: 11.2 }), { text: 'Waiting for the filter: 12 s', kind: 'busy' });
    assert.deepEqual(at('waiting', { waitS: 0.3 }), { text: 'Waiting for the filter: 1 s', kind: 'busy' });
    assert.deepEqual(at('reconnecting'), { text: 'Reconnecting…', kind: 'busy' });
    assert.equal(at('playing'), null);
    assert.equal(radioStateText({ radio: null }), null);
    assert.deepEqual(radioStateText({ radioError: 'unreachable', error: 'Radio: HTTP 404', radio: null }),
        { text: "Couldn't reach the station", kind: 'error' });
    assert.equal(radioStateText({ radioError: 'format' }).text, "This stream's format isn't supported yet");
    assert.equal(radioStateText({ radioError: 'encrypted' }).text, "This stream is encrypted: it can't be played.");
    assert.equal(radioStateText({ radioError: 'format', error: 'Radio: unsupported codec (ac-3): x' }).text,
        "This stream's format isn't supported yet", 'the words, not the backend\'s text');
    assert.deepEqual(at('failed', { info: { errorKind: 'format', error: 'unsupported codec (ac-3): x' } }),
        { text: RADIO_TEXTS.err.format, kind: 'error' });
    assert.equal(radioStateText({ radioError: 'rate' }).text, "This stream's sample rate can't be upsampled");
    assert.deepEqual(at('failed', { info: { errorKind: 'encrypted', error: 'the HLS stream is encrypted (AES-128)' } }),
        { text: RADIO_TEXTS.err.encrypted, kind: 'error' });
    assert.deepEqual(radioStateText({ radioError: 'other', error: 'Radio: the filter is missing' }),
        { text: 'Radio: the filter is missing', kind: 'error' });
    assert.deepEqual(at('failed', { info: { errorKind: 'format', error: 'unsupported codec' } }),
        { text: RADIO_TEXTS.err.format, kind: 'error' });
});

test("while the stream's own filter is being made the status says so, a failure over it", () => {
    const at = (phase, extra = {}) => radioStateText({ radio: { phase, preparing: true, info: { url: rp }, ...extra } });
    const prep = { text: 'Preparing this filter for streams (once)…', kind: 'busy' };
    assert.deepEqual(at('connecting'), prep, 'over Tuning in…');
    assert.deepEqual(at('waiting', { waitS: 40 }), prep, 'over the wait');
    assert.deepEqual(at('playing'), prep, 'a new rack on a stream that plays');
    assert.deepEqual(at('reconnecting'), prep);
    assert.deepEqual(at('failed', { info: { errorKind: 'unreachable', error: 'HTTP 404' } }),
        { text: "Couldn't reach the station", kind: 'error' }, 'a failure is said over it');
    assert.deepEqual(radioStateText({ radioError: 'unreachable', radio: { phase: 'connecting', preparing: true } }),
        { text: RADIO_TEXTS.err.unreachable, kind: 'error' }, "the player's failure first");
    assert.deepEqual(at('connecting', { preparing: false }), { text: 'Tuning in…', kind: 'busy' }, 'made: the phase again');
    assert.equal(radioStateText({ radio: null }), null, 'no stream: a filter made ahead for the view is not said');
});

test('⏮ and ⏭ step through the list, round its ends', () => {
    assert.equal(stepStation(rp, 1).name, 'Radio Paradise — Mellow Mix');
    assert.equal(stepStation(rp, -1).name, 'FIP Groove');
    assert.equal(stepStation(STATIONS[9].url, 1).name, 'Radio Paradise — Main Mix');
    assert.equal(stepStation('https://pasted.example/live', 1), STATIONS[0]);
    assert.equal(stepStation('https://pasted.example/live', -1), STATIONS[9]);
});

test('⏮ and ⏭ step through the list a station was started from, past the ones that do not play', () => {
    const list = [{ name: 'A', url: 'http://a.example/s' }, { name: 'B', url: 'http://b.example/s', plays: false },
        { name: 'C', url: 'http://c.example/s' }];
    const skip = st => st.plays === false;
    assert.equal(stepStation('http://a.example/s', 1, list, skip).name, 'C');
    assert.equal(stepStation('http://c.example/s', 1, list, skip).name, 'A');
    assert.equal(stepStation('http://c.example/s', -1, list, skip).name, 'A');
    assert.equal(stepStation('http://pasted.example/', -1, list, skip).name, 'C');
    assert.equal(stepStation('http://a.example/s', 1, [list[1]], skip), null, 'nothing that plays');
    assert.equal(stepStation('http://a.example/s', 1, []), null);
});

const cat = (name, extra = {}) => ({ name, url: `http://${name.toLowerCase()}.example/live`, uuid: `00000000-0000-0000-0000-00000000000${name.length}`, ...extra });

test('the records: favorites starred and unstarred, recent first and once, a damaged record made whole', () => {
    let rec = normStore(null);
    assert.deepEqual(rec, { v: 1, favorites: [], recent: [], failed: {} });
    const a = cat('Alpha', { codec: 'MP3', bitrate: 128, plays: true, junk: 1 });
    rec = toggleFavorite(rec, a);
    assert.ok(isFavorite(rec, a));
    assert.equal(rec.favorites[0].junk, undefined, 'only what a row shows is kept');
    assert.ok(isFavorite(rec, { ...a, url: 'http://moved.example/' }), 'a catalog station is its uuid');
    rec = toggleFavorite(rec, a);
    assert.equal(rec.favorites.length, 0);
    // A pasted stream is its address.
    assert.equal(stationKey({ url: 'HTTP://X.example/live/' }), stationKey({ url: 'http://x.example/live' }));
    for (let i = 0; i < RECENT_KEEP + 5; i++) rec = addRecent(rec, cat('S' + i, { uuid: undefined }));
    assert.equal(rec.recent.length, RECENT_KEEP);
    assert.equal(rec.recent[0].name, 'S' + (RECENT_KEEP + 4));
    rec = addRecent(rec, cat('S10', { uuid: undefined }));
    assert.equal(rec.recent[0].name, 'S10');
    assert.equal(rec.recent.filter(r => r.name === 'S10').length, 1);
    const kept = normStore({ favorites: [a, { name: 'no address' }, 'x'], recent: 'nope', failed: { u: 2, v: 'x' } });
    assert.equal(kept.favorites.length, 1);
    assert.deepEqual([kept.recent, kept.failed], [[], { u: 2 }]);
});

test('what was heard is kept: the rate, and the name a pasted stream gives itself', () => {
    const pasted = { name: 'x.example', url: 'http://x.example/live' };
    let rec = addRecent(normStore(null), pasted);
    const same = noteHeard(rec, 'http://x.example/live', { rate: 0 });
    assert.equal(same, rec, 'nothing heard yet');
    rec = noteHeard(rec, 'http://x.example/live/', { rate: 44100, bits: 16, codec: 'flac', icyName: 'Station X' });
    assert.deepEqual(rec.recent[0], { name: 'Station X', url: 'http://x.example/live', rate: 44100, bits: 16, codec: 'FLAC' });
    assert.equal(noteHeard(rec, 'http://x.example/live', { rate: 44100, bits: 16, codec: 'flac', icyName: 'Other' }), rec, 'heard before');
    // A catalog station plays at the address the catalog gave at its start.
    const c = cat('Gamma', { codec: 'AAC', bitrate: 320 });
    rec = addRecent(rec, c);
    rec = noteHeard(rec, 'http://cdn.example/aac-320', { rate: 48000, codec: 'aac', icyName: 'Not its name' }, stationKey(c));
    assert.deepEqual([rec.recent[0].name, rec.recent[0].rate, rec.recent[0].codec], ['Gamma', 48000, 'AAC']);
});

test('a station that turned out not to play is marked until the formats change', () => {
    const ogg = cat('Opus', { codec: 'OGG', plays: true });
    let rec = normStore(null);
    assert.ok(playsHere(rec, ogg, 1));
    rec = markFailed(rec, ogg.uuid, 1);
    assert.ok(!playsHere(rec, ogg, 1));
    assert.ok(playsHere(rec, ogg, 2), 'a newer formats table tries it again');
    assert.ok(!playsHere(rec, cat('HE', { codec: 'AAC+', plays: false }), 1));
    assert.equal(markFailed(rec, undefined, 1), rec, 'a pasted stream has no uuid to remember');
});

test('a row says the genres, the country and the format, and that it does not play yet', () => {
    const st = { name: 'R', url: 'http://r.example/', tags: ['jazz', 'smooth jazz', 'lounge'], country: 'Switzerland', codec: 'MP3', bitrate: 128 };
    assert.equal(techLine(st), 'MP3 128k');
    assert.equal(rowSub(st), 'jazz, smooth jazz · Switzerland · MP3 128k');
    assert.equal(rowSub({ ...st, rate: 44100 }, false), 'jazz, smooth jazz · Switzerland · MP3 128k 44.1 kHz · not supported yet');
    assert.equal(techLine({ codec: '', bitrate: 64 }), '64k');
    assert.equal(techLine({ codec: 'FLAC', bitrate: 0, rate: 96000 }), 'FLAC 96 kHz');
    assert.equal(rowSub(STATIONS[0]), 'FLAC 44.1 kHz');
    assert.equal(initials('Radio Paradise — Main Mix'), 'RP');
    assert.equal(initials('1.FM - Jazz'), '1F');
    assert.equal(initials('  '), '?');
    assert.equal(tileHue('FIP'), tileHue('FIP'));
});

test("the stream's filter is made ahead again only for another rack or rate", () => {
    const rack = { mode: 'aura', fsMultiplier: 8, taps: 30000000, phase: 'linear', apodizer: 'off' };
    const k = warmKeyOf(rack, 0);
    assert.equal(warmKeyOf({ ...rack, apodizer: 'gentle' }, 0), k, 'a stage after the filter: the same filter');
    assert.equal(warmKeyOf(rack, null), k, 'no stream yet: the 44.1 kHz family');
    for (const other of [{ taps: 1000000 }, { fsMultiplier: 16 }, { phase: 'hybrid' }, { mode: 'direct' }]) {
        assert.notEqual(warmKeyOf({ ...rack, ...other }, 0), k, JSON.stringify(other));
    }
    assert.notEqual(warmKeyOf(rack, 48000), k, "the stream's rate");
    assert.equal(warmKeyOf(null, 0), warmKeyOf(undefined, 0), 'no rack yet: no throw');
});

test("the stream's filter is made ahead for its own rate, and not while it tunes in", () => {
    assert.equal(warmRateOf(null), 0, 'no stream: the 44.1 kHz family');
    assert.equal(warmRateOf({ phase: 'playing', info: { url: rp, rate: 48000 } }), 48000);
    assert.equal(warmRateOf({ phase: 'reconnecting', info: { url: rp, rate: 44100 } }), 44100);
    assert.equal(warmRateOf({ phase: 'connecting', info: { url: rp } }), null, 'its rate not said yet: left as it is');
    assert.equal(warmRateOf({ phase: 'waiting', info: { url: rp, rate: 0 } }), null);
    assert.equal(warmRateOf({ phase: 'failed', info: { url: rp, errorKind: 'unreachable' } }), 0, 'failed before its rate');
    assert.equal(warmRateOf({ phase: 'connecting', stopped: true, info: {} }), 0, 'stopped');
});

test('how close a name is to what was searched', () => {
    assert.equal(nameRank('1.FM', ' 1.fm '), 0, 'the name is it');
    assert.equal(nameRank('1.FM - Absolute 90s', '1.fm'), 1, 'the name starts with it');
    assert.equal(nameRank('Radio 1.FM Lounge', '1.fm'), 2, 'a word starts with it');
    assert.equal(nameRank('Lounge (1.FM)', '1.fm'), 2, 'after a bracket too');
    assert.equal(nameRank('181.fm - The Breeze', '1.fm'), 3, 'inside a word');
    assert.equal(nameRank('x1.fm 1.fmz', '1.fm'), 2, 'a later word start counts');
    assert.equal(nameRank('Cafe Jazz', 'café'), 4, 'the catalog matched it otherwise');
});

test('a search by name lists the closest names first, A to Z within each', () => {
    const names = ['181.FM - The Breeze', 'Radio 1.FM Lounge', '1.FM - Adore Jazz', 'x1.fmy', '1.fm', '181.fm - 80s',
        '1.FM - Absolute 90s', 'Café 1.fm'];
    assert.deepEqual(searchOrder(names.map(name => ({ name })), ' 1.FM ').map(s => s.name), [
        '1.fm',
        '1.FM - Absolute 90s', '1.FM - Adore Jazz',
        'Café 1.fm', 'Radio 1.FM Lounge',
        '181.fm - 80s', '181.FM - The Breeze', 'x1.fmy',
    ]);
    // The catalog's page is left as it was.
    const page = [{ name: 'b' }, { name: 'a' }];
    searchOrder(page, 'a');
    assert.deepEqual(page.map(s => s.name), ['b', 'a']);
});

test('A to Z: case not looked at, numbers by their value, equal names in the order given', () => {
    assert.deepEqual(alphabetical([{ name: 'Radio 10' }, { name: 'radio 2' }, { name: 'Radio 1' }, { name: 'ABD' }, { name: 'abc' }])
        .map(s => s.name), ['abc', 'ABD', 'Radio 1', 'radio 2', 'Radio 10']);
    assert.deepEqual(searchOrder([{ name: 'b' }, { name: 'A' }, { name: 'c 10' }, { name: 'c 9' }], '  ').map(s => s.name),
        ['A', 'b', 'c 9', 'c 10'], 'browsing without a name: A to Z');
    const a = { name: 'Jazz', url: 'a' }, b = { name: 'jazz', url: 'b' };
    assert.deepEqual(searchOrder([a, b], ''), [a, b]);
    assert.deepEqual(searchOrder([b, a], ''), [b, a], 'the most voted of equal names stays first');
    const favs = [{ name: 'Zeta' }, { name: 'alpha' }];
    assert.deepEqual(alphabetical(favs).map(s => s.name), ['alpha', 'Zeta']);
    assert.equal(favs[0].name, 'Zeta', 'the records keep their own order');
    assert.deepEqual(alphabetical(null), []);
});

test('the order picked is the order listed: name, the most voted or played, the best quality', () => {
    const st = (name, codec, bitrate, votes, clickcount) => ({ name, codec, bitrate, votes, clickcount });
    const found = [
        st('b', 'MP3', 128, 10, 900), st('a', 'AAC', 320, 50, 5), st('RP', 'OGG', 1441, 50, 70), st('K', 'FLAC', 0, 1, 1),
        st('v', 'OGG', 500, 7, 7), st('c', 'MP3', 320, 900, 0), st('z', '', 0, 0, 0),
    ];
    const names = (sort, q = '') => searchOrder(found, q, sort).map(s => s.name);
    assert.deepEqual(names('name'), ['a', 'b', 'c', 'K', 'RP', 'v', 'z']);
    assert.deepEqual(names(undefined), names('name'), 'by name unless told');
    assert.deepEqual(names('votes'), ['c', 'a', 'RP', 'b', 'v', 'K', 'z'], 'equal votes: A to Z');
    assert.deepEqual(names('clickcount'), ['b', 'RP', 'v', 'a', 'K', 'c', 'z']);
    assert.deepEqual(names('quality'), ['RP', 'K', 'v', 'a', 'c', 'b', 'z'], 'lossless first, then the highest bitrate');
    assert.deepEqual(names('votes', 'r'), names('votes'), 'a name asked does not reorder the most voted');
    assert.deepEqual(names('quality', 'RP').slice(0, 2), ['RP', 'K']);
});

test("lossless is FLAC, or Ogg past 600 kbps; a saved order that is not one is by name", () => {
    assert.equal(isLossless({ codec: 'FLAC', bitrate: 0 }), true);
    assert.equal(isLossless({ codec: 'ogg', bitrate: 1441 }), true);
    assert.equal(isLossless({ codec: 'OGG', bitrate: 500 }), false, 'Vorbis at its best');
    assert.equal(isLossless({ codec: 'MP3', bitrate: 1000 }), false);
    assert.equal(isLossless(null), false);
    assert.deepEqual(SORTS, ['name', 'votes', 'clickcount', 'quality']);
    assert.equal(sortFromSaved('quality'), 'quality');
    assert.equal(sortFromSaved(''), 'name', 'nothing saved: by name');
    assert.equal(sortFromSaved('random'), 'name');
    assert.equal(RADIO_TEXTS.orderName, 'Name');
    assert.equal(RADIO_TEXTS.orderQuality, 'Best quality');
});

test('the genres and countries to choose from A to Z, the languages as the catalog gives them', () => {
    const items = pickerItems({
        tags: [{ name: 'pop', count: 5000 }, { name: 'Jazz', count: 900 }, { name: '80s', count: 800 }, { name: 'ambient', count: 10 }, { name: '' }],
        countries: [{ code: 'CH', name: 'Switzerland' }, { code: 'AX', name: 'Åland Islands' }, { code: 'AT', name: 'austria' },
            { code: 'DE', name: 'Germany' }, { code: '', name: 'Nowhere' }],
        languages: [{ name: 'english' }, { name: 'german' }, { name: 'dutch' }],
    });
    assert.deepEqual(items.tags, [['80s', '80s'], ['ambient', 'ambient'], ['Jazz', 'Jazz'], ['pop', 'pop']]);
    assert.deepEqual(items.countries, [['AX', 'Åland Islands'], ['AT', 'austria'], ['DE', 'Germany'], ['CH', 'Switzerland']]);
    assert.deepEqual(items.languages, [['english', 'English'], ['german', 'German'], ['dutch', 'Dutch']], 'the most spoken first');
    assert.deepEqual(pickerItems(null), { tags: [], countries: [], languages: [] });
});

test('the same search is the same key', () => {
    const q = { name: ' Jazz ', tag: 'jazz', country: 'CH', language: '', codec: '', order: 'votes' };
    assert.equal(searchKey(q), searchKey({ ...q, name: 'jazz', offset: 50 }));
    assert.notEqual(searchKey(q), searchKey({ ...q, order: 'clickcount' }));
    assert.notEqual(searchKey(q), searchKey({ ...q, quality: 'lossless' }), 'the quality asks another search');
    assert.equal(searchKey(q), searchKey({ ...q, quality: '' }), 'a search kept before the quality: any');
});

test('the qualities a search can be narrowed to, in the agreed words', () => {
    assert.deepEqual(QUALITIES.map(qualityText),
        ['Lossless', '320 kbps and up', '256 kbps and up', '192 kbps and up', '128 kbps and up']);
    assert.equal(RADIO_TEXTS.anyQuality, 'Any quality');
});
