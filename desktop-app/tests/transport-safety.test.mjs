// Nothing starts sound but the listener's own press (Anton 31.10: paused,
// then a conversion finished, and the converted file started by itself).
// Every call of a player command that starts sound sits in a handler of a
// press; a new one — in a status poll, a conversion's end, a rack send —
// fails here until it is looked at and added. The player keeps a pause
// through everything else (controller.rs `aired`).
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const JS = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'src', 'js');
const STARTS = /['"](player_play|player_radio|player_input_listen|player_next|player_prev)['"]/;

/// file, command, the function it is called in — each a press.
const PRESSES = new Set([
    'player.js player_play onPlay',          // ▶ / a row's ▶ / Space / the analyser's ▶
    'player.js player_input_listen onPlay',  // ▶ after the input's ■
    'player.js player_radio playRadio',      // a station chosen, ▶ after a stream's ■
    'player.js player_prev wire',            // ⏮
    'player.js player_next wire',            // ⏭
    'blindtest.js player_play play',         // the blind test the listener started, muted till its level
    'input.js player_input_listen act',      // the input view's Listen
]);

function files(dir) {
    return fs.readdirSync(dir, { withFileTypes: true }).flatMap(e =>
        e.isDirectory() ? files(path.join(dir, e.name)) : e.name.endsWith('.js') ? [path.join(dir, e.name)] : []);
}

function calls() {
    const out = [];
    for (const f of files(JS)) {
        const lines = fs.readFileSync(f, 'utf8').split('\n');
        lines.forEach((l, i) => {
            const m = l.match(STARTS);
            if (!m || /^\s*\/\//.test(l)) return;
            let fn = '?';
            for (let j = i; j >= 0; j--) {
                const k = lines[j].match(/^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)/);
                if (k) { fn = k[1]; break; }
            }
            out.push({ key: `${path.relative(JS, f).split(path.sep).join('/')} ${m[1]} ${fn}`, line: i + 1 });
        });
    }
    return out;
}

test('every call that starts sound is in the handler of a press', () => {
    const found = calls();
    const stray = found.filter(c => !PRESSES.has(c.key)).map(c => `${c.key} (line ${c.line})`);
    assert.deepEqual(stray, [], 'a command that starts sound outside a press');
    // The list stays the real one: each press is still there.
    const seen = new Set(found.map(c => c.key));
    for (const p of PRESSES) assert.ok(seen.has(p), `gone: ${p}`);
});

test('the status poll and a conversion never start sound', () => {
    const player = fs.readFileSync(path.join(JS, 'player.js'), 'utf8');
    const converter = fs.readFileSync(path.join(JS, 'converter.js'), 'utf8');
    assert.doesNotMatch(converter, STARTS);
    // The rendered map goes to the player as settings only (sendNow).
    const send = player.slice(player.indexOf('export function sendNow'), player.indexOf('/// The menu\'s Instant start tick'));
    assert.ok(send.length > 0);
    assert.doesNotMatch(send, STARTS);
});
