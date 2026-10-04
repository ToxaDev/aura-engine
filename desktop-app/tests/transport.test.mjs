// The transport's buttons for what plays (transport.js).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import { fileTransport } from '../src/js/transport.js';

const file = { state: 'playing', trackId: 3, radio: null };
const stream = { state: 'playing', trackId: null, radio: { phase: 'playing', info: { url: 'https://icecast.radiofrance.fr/fip-hifi.aac' } } };
const stopped = { state: 'stopped', trackId: null, radio: null };

test('a file has the five buttons, a stream only play and stop', () => {
    assert.equal(fileTransport(file, { lastSource: 'list', stream: true, file: true }), true);
    assert.equal(fileTransport(stream, { lastSource: 'radio', stream: true, file: true }), false);
    // Tuning in, waiting, reconnecting: a stream all the same.
    assert.equal(fileTransport({ ...stream, state: 'preparing', radio: { phase: 'connecting' } }, { lastSource: 'radio', stream: true }), false);
    // A file paused or getting ready: five.
    assert.equal(fileTransport({ ...file, state: 'paused' }, { lastSource: 'list', stream: true, file: true }), true);
    assert.equal(fileTransport({ ...file, state: 'preparing' }, { lastSource: 'list', stream: true, file: true }), true);
});

test('stopped after the radio, the row stays two - play brings the radio back', () => {
    assert.equal(fileTransport(stopped, { lastSource: 'radio', stream: true, file: true }), false);
    // No file to play in the list: play brings the last stream back too.
    assert.equal(fileTransport(stopped, { lastSource: 'list', stream: true, file: false }), false);
});

test('a file chosen after the stop has five, and stopped after a file it stays five', () => {
    // A row's play sets the last source to the list, and the file plays.
    assert.equal(fileTransport(file, { lastSource: 'list', stream: true, file: true }), true);
    assert.equal(fileTransport(stopped, { lastSource: 'list', stream: true, file: true }), true);
    // Never a stream: the list's row.
    assert.equal(fileTransport(stopped, { lastSource: 'list', stream: false, file: false }), true);
    assert.equal(fileTransport(null), true, 'no status yet');
});

test('file, stream, stop, file: five, two, two, five', () => {
    const seq = [
        [file, { lastSource: 'list', stream: false, file: true }],
        [stream, { lastSource: 'radio', stream: true, file: true }],
        [stopped, { lastSource: 'radio', stream: true, file: true }],
        [file, { lastSource: 'list', stream: true, file: true }],
    ];
    assert.deepEqual(seq.map(([s, c]) => fileTransport(s, c)), [true, false, false, true]);
});
