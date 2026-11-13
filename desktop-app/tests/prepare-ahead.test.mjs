// "Prepare next track early": off on a new install, kept, and sent to the
// player with the rack (the backend's `prepare_ahead`).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// Work ahead took up to half the CPU while tracks were skipped through, and
// the track on the air paid for it: nothing is made ahead unless asked for.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const JS = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'src', 'js');
const player = fs.readFileSync(path.join(JS, 'player.js'), 'utf8');

test('a new install does not prepare ahead: only a stored "1" turns it on', () => {
    const m = player.match(/let prepareAhead = load\(KEY_PREPARE_AHEAD, '([^']*)'\) === '1';/);
    assert.ok(m, 'prepareAhead is read from its key, on only for "1"');
    assert.equal(m[1], '0', 'the default is off');
    assert.match(player, /const KEY_PREPARE_AHEAD = 'auraPlayerPrepareAhead';/);
});

test('the tick is sent with the rack, and a change of it alone is sent too', () => {
    assert.match(player, /invoke\('player_set_settings', \{[^}]*\bprepareAhead\b[^}]*\}\)/);
    assert.match(player, /prepareAhead === lastAhead\) return Promise\.resolve\(\);/);
});

test('the menu row flips it, stores it and sends it at once', () => {
    assert.match(player, /id: 'plMenuAhead',\s*act: \(\) => setPrepareAhead\(!prepareAhead\)/);
    const fn = player.match(/function setPrepareAhead\(on\) \{([^}]*)\}/);
    assert.ok(fn, 'setPrepareAhead');
    assert.match(fn[1], /store\(KEY_PREPARE_AHEAD, on \? '1' : '0'\)/);
    assert.match(fn[1], /sendNow\(\)/);
});
