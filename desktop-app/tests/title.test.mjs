// Whose analyzer window it is, by its title bar: a file window its file, the
// live window what plays — the file's name, or a stream's station and song —
// changing with it.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { windowTitle, titleParts, titleMarkup, fileName } from '../src/js/analytics/title.js';

test('a file window says its file (or its converted copy)', () => {
  assert.equal(windowTitle({ mode: 'file', path: 'D:\\Music\\Fetty Wap - RGF Island.mp3' }),
    'AuraEngine Analyzer · Fetty Wap - RGF Island.mp3');
  assert.equal(windowTitle({ mode: 'file', path: 'D:\\Music\\a.flac', conv: 'D:\\Out\\a [AE 1M].flac' }),
    'AuraEngine Analyzer · a [AE 1M].flac (converted)');
  assert.equal(windowTitle({ mode: 'file', entryId: '12' }), 'AuraEngine Analyzer · file 12');
  assert.equal(fileName('/home/x/Music/b.wav'), 'b.wav');
});

test('the live window says what plays: a file by its name, a stream by station and song', () => {
  assert.equal(windowTitle({ mode: 'live', file: 'WARGASM (UK) - Do It So Good.mp3' }),
    'AuraEngine Analyzer · LIVE · WARGASM (UK) - Do It So Good.mp3');
  assert.equal(windowTitle({ mode: 'live', stream: { station: '1.FM Top Rap', song: 'Ken Carson - Jennifer`s Body' } }),
    'AuraEngine Analyzer · 1.FM Top Rap · Ken Carson - Jennifer`s Body');
  assert.equal(windowTitle({ mode: 'live', stream: { station: 'Radio Paradise — Main Mix', song: '' } }),
    'AuraEngine Analyzer · Radio Paradise — Main Mix', 'no song title: the station');
  assert.equal(windowTitle({ mode: 'live' }), 'AuraEngine Analyzer · LIVE', 'nothing plays');
});

test('the live window follows the player: file, stream, the next song, a file again', () => {
  const seq = [
    { file: 'a.flac' },
    { stream: { station: 'FIP', song: 'Song one' } },
    { stream: { station: 'FIP', song: 'Song two' } },
    { file: 'b.mp3' },
  ].map(w => windowTitle({ mode: 'live', ...w }));
  assert.deepEqual(seq, [
    'AuraEngine Analyzer · LIVE · a.flac',
    'AuraEngine Analyzer · FIP · Song one',
    'AuraEngine Analyzer · FIP · Song two',
    'AuraEngine Analyzer · LIVE · b.mp3',
  ]);
  // A stream outranks a file name the status may still give in passing.
  assert.equal(windowTitle({ mode: 'live', file: 'a.flac', stream: { station: 'FIP', song: 'x' } }),
    'AuraEngine Analyzer · FIP · x');
});

// The bar's title in colours (Anton 4.10): each part in its own span.
const kinds = w => titleParts(w).map(p => `${p.kind}:${p.text}`);

test('the bar tells the parts apart: the name, the station, the song, the dots', () => {
  const radio = { mode: 'live', stream: { station: 'Radio Paradise — Main Mix', song: 'Slowdive — alife' } };
  assert.deepEqual(kinds(radio), [
    'app:AuraEngine Analyzer', 'sep: · ', 'station:Radio Paradise — Main Mix', 'sep: · ', 'song:Slowdive — alife',
  ]);
  assert.equal(titleMarkup(titleParts(radio)),
    '<span class="an-t-app">AuraEngine Analyzer</span><span class="an-t-sep"> · </span>'
    + '<span class="an-t-station">Radio Paradise — Main Mix</span><span class="an-t-sep"> · </span>'
    + '<span class="an-t-song">Slowdive — alife</span>');
  // No song title: the station alone; nothing plays: LIVE.
  assert.deepEqual(kinds({ mode: 'live', stream: { station: 'FIP', song: '' } }), ['app:AuraEngine Analyzer', 'sep: · ', 'station:FIP']);
  assert.deepEqual(kinds({ mode: 'live' }), ['app:AuraEngine Analyzer', 'sep: · ', 'mode:LIVE']);
});

test('a file is in white: the live window\'s and a file window\'s, its "(converted)" apart', () => {
  assert.deepEqual(kinds({ mode: 'live', file: 'a.flac' }),
    ['app:AuraEngine Analyzer', 'sep: · ', 'mode:LIVE', 'sep: · ', 'file:a.flac']);
  assert.deepEqual(kinds({ mode: 'file', path: 'D:\\Music\\b.wav' }), ['app:AuraEngine Analyzer', 'sep: · ', 'file:b.wav']);
  assert.deepEqual(kinds({ mode: 'file', path: 'D:\\Music\\a.flac', conv: 'D:\\Out\\a [AE 1M].flac' }),
    ['app:AuraEngine Analyzer', 'sep: · ', 'file:a [AE 1M].flac', 'note: (converted)']);
});

test('the parts joined are the system\'s title, the same text as before', () => {
  const all = [
    { mode: 'file', path: 'D:\\Music\\Fetty Wap - RGF Island.mp3' },
    { mode: 'file', path: 'D:\\Music\\a.flac', conv: 'D:\\Out\\a [AE 1M].flac' },
    { mode: 'file', entryId: '12' },
    { mode: 'live', file: 'x.mp3' },
    { mode: 'live', stream: { station: '1.FM Top Rap', song: 'Ken Carson - Jennifer`s Body' } },
    { mode: 'live', stream: { station: '', song: 'Only a song' } },
    { mode: 'live' },
  ];
  for (const w of all) {
    const parts = titleParts(w);
    assert.equal(parts.map(p => p.text).join(''), windowTitle(w));
    // The markup, its tags taken off, says the same.
    assert.equal(titleMarkup(parts).replace(/<[^>]+>/g, ''), windowTitle(w));
  }
});

test('a station\'s or a song\'s text from the stream is text, never markup', () => {
  const m = titleMarkup(titleParts({ mode: 'live', stream: { station: 'Rock & <b>Roll</b>', song: '"x" <img src=y onerror=1>' } }));
  assert.ok(!/<b>|<img/.test(m), m);
  assert.match(m, /<span class="an-t-station">Rock &amp; &lt;b&gt;Roll&lt;\/b&gt;<\/span>/);
  assert.match(m, /<span class="an-t-song">&quot;x&quot; &lt;img src=y onerror=1&gt;<\/span>/);
});

test('every part has its colour in the analyzer\'s sheet', () => {
  const css = readFileSync(new URL('../src/css/analytics.css', import.meta.url), 'utf8');
  for (const k of ['app', 'mode', 'station', 'song', 'file', 'note', 'sep']) {
    assert.match(css, new RegExp(`\\.an-title-text \\.an-t-${k}\\b`), k);
  }
});
