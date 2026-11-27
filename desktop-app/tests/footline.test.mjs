import { test } from 'node:test';
import assert from 'node:assert/strict';
import { footConvView, FOOT_TEXTS } from '../src/js/footline-core.js';

const row = o => ({ cls: '', readable: true, convertable: true, isDisk: false, fromDisk: false, pct: 0, title: '', ...o });

test('nothing in the player: no chip; a running batch still shows', () => {
    assert.equal(footConvView(null).kind, 'none');
    assert.equal(footConvView(null).batch, null);
    assert.equal(footConvView(null, { converting: true, pct: 41.6 }).batch, 'Batch 42%');
});

test('a converted track: the chip, and where it plays from in the tip', () => {
    const v = footConvView(row({ isDisk: true, fromDisk: true, convertable: false, cls: 'done' }));
    assert.equal(v.kind, 'done');
    assert.equal(v.text, FOOT_TEXTS.done);
    assert.equal(v.tip, FOOT_TEXTS.doneTip);
    const live = footConvView(row({ isDisk: true, fromDisk: false, convertable: false }));
    assert.deepEqual([live.kind, live.text, live.tip], ['live', FOOT_TEXTS.live, FOOT_TEXTS.doneLiveTip]);
    const quiet = footConvView(row({ isDisk: true, fromDisk: false, liveWhy: 'volume', convertable: false }));
    assert.deepEqual([quiet.kind, quiet.text, quiet.tip], ['live', FOOT_TEXTS.liveVolume, FOOT_TEXTS.doneVolumeTip]);
});

test('not converted: the button; after a failure it says again and why', () => {
    const v = footConvView(row({}));
    assert.equal(v.kind, 'convert');
    assert.equal(v.text, 'Convert');
    const f = footConvView(row({ cls: 'error', title: 'Playing · Disk full' }));
    assert.equal(f.kind, 'convert');
    assert.equal(f.text, 'Convert again');
    assert.match(f.tip, /Disk full/);
});

test('its conversion runs: the percent, clamped, and no batch text beside it', () => {
    const v = footConvView(row({ cls: 'active pl-prep', convertable: false, pct: 41.6 }), { converting: true, pct: 10 });
    assert.equal(v.kind, 'active');
    assert.equal(v.text, 'CONVERTING 42%');
    assert.equal(v.pct, 41.6);
    assert.equal(v.batch, null);
    assert.equal(footConvView(row({ cls: 'active', convertable: false, pct: 140 })).pct, 100);
});

test('waiting its turn: queued, with the batch beside it', () => {
    const v = footConvView(row({ cls: 'queued', convertable: false }), { converting: true, pct: 7 });
    assert.equal(v.kind, 'queued');
    assert.equal(v.batch, 'Batch 7%');
});

test('an unreadable file, or one that cannot be converted now: no chip', () => {
    assert.equal(footConvView(row({ readable: false })).kind, 'none');
    assert.equal(footConvView(row({ convertable: false })).kind, 'none');
});

test('the class is matched as a word', () => {
    assert.equal(footConvView(row({ cls: 'inactive' })).kind, 'convert');
});
