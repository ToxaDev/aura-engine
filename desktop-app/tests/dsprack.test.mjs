// The subsonic corner the UI sends, and the corners the backend accepts.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// Two lists name the corners: SUBSONIC_STEPS here and SUBSONIC_CORNERS_HZ in
// apodize.rs. The command refuses a corner that is not on its list, so a
// step added on one side only would be a batch that fails before its first
// file. The first case reads both and holds them together.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { SUBSONIC_STEPS, subsonicCornerHz } from '../src/js/dsprack.js';

test('the UI offers exactly the corners the backend accepts', () => {
    const rs = readFileSync(
        new URL('../src-tauri/src/audio/converter/apodize.rs', import.meta.url), 'utf8');
    const m = rs.match(/pub const SUBSONIC_CORNERS_HZ:\s*\[u32;\s*\d+\]\s*=\s*\[([^\]]*)\]/);
    assert.ok(m, 'SUBSONIC_CORNERS_HZ not found in apodize.rs');
    const backend = m[1].split(',').map(s => parseInt(s, 10)).filter(Number.isFinite);
    assert.deepEqual([...SUBSONIC_STEPS].sort((a, b) => a - b), backend.sort((a, b) => a - b));
    assert.equal(SUBSONIC_STEPS[0], 20, 'the chip opens on 20 Hz');
});

/// Just enough of a document for the two inputs the corner is read from.
function page({ on, hz }) {
    const els = {
        convSubsonic: { checked: on },
        convSubsonicHz: hz === undefined ? undefined : { value: hz },
    };
    globalThis.document = { getElementById: id => els[id] ?? null };
}

test('off sends 0 whatever the chip says', () => {
    page({ on: false, hz: '15' });
    assert.equal(subsonicCornerHz(), 0);
});

test('on sends the corner on the chip', () => {
    for (const hz of SUBSONIC_STEPS) {
        page({ on: true, hz: String(hz) });
        assert.equal(subsonicCornerHz(), hz);
    }
});

test('a corner the backend would refuse reads as the default', () => {
    for (const hz of ['12', '0', '', 'abc', '25']) {
        page({ on: true, hz });
        assert.equal(subsonicCornerHz(), 20, `value ${JSON.stringify(hz)}`);
    }
    page({ on: true, hz: undefined });
    assert.equal(subsonicCornerHz(), 20, 'missing input');
});
