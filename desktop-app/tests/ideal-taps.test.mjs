// The ideal position's size for the playing track (ideal-taps.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
    IDEAL_SLOT, idealTapsFor, idealShort, setIdealTrack, idealSlotLabel, shownTapsLabel, tapsReadout, idealTip,
    onIdealChange, isOwnSize, isTrackSize, autoBuilt, chainFs, engineTaps, markShiftHtml, SHIFT_MARK,
} from '../src/js/ideal-taps.js';

test('the ideal filter has 2 x length x rate taps at 16 x the family base', () => {
    assert.deepEqual(idealTapsFor(300, 44100), { taps: 423_360_001, rate: 705600, durationS: 300, shifted: false });
    assert.equal(idealTapsFor(300, 88200).rate, 705600);
    assert.equal(idealTapsFor(60, 48000).taps, 92_160_001);
    assert.equal(idealTapsFor(60, 96000).taps, 92_160_001);
    // The rack's FS sets the rate; Auto Shift is the even length, one tap fewer.
    assert.equal(idealTapsFor(300, 44100, 8).taps, 211_680_001);
    assert.equal(idealTapsFor(300, 44100, 16, true).taps, 423_360_000);
    assert.equal(idealTapsFor(0, 44100), null);
    assert.equal(idealTapsFor(300, 0), null);
});

test('a long track gets all the taps it needs; a chain of its own size is what Auto asks for', () => {
    assert.equal(idealTapsFor(4800, 44100).taps, 6_773_760_001);
    assert.equal(isOwnSize('423M'), true);
    assert.equal(isOwnSize('1G'), false);
    assert.equal(isOwnSize(null), false);
    assert.equal(autoBuilt('423M', true), true);
    assert.equal(autoBuilt('Auto', true), true);
    assert.equal(autoBuilt('30M', true), false, "a track's quick start on a stored rung is not its own filter yet");
    assert.equal(autoBuilt('30M', false), true, 'a stream or a live input: the stored rung is what Auto plays');
});

test('the engine gets one tap count for Auto and the grid beside it', () => {
    assert.deepEqual(engineTaps(IDEAL_SLOT), { taps: IDEAL_SLOT, autoShift: false });
    assert.deepEqual(engineTaps(IDEAL_SLOT + 1), { taps: IDEAL_SLOT, autoShift: true });
    assert.deepEqual(engineTaps(30_000_000), { taps: 30_000_000, autoShift: true });
});

test('sizes are written short', () => {
    assert.equal(idealShort(423_360_000), '423M');
    assert.equal(idealShort(1_270_080_000), '1.27G');
    assert.equal(idealShort(999_600_000), '1.00G');
    assert.equal(idealShort(600_000), '600k');
});

test('the slot shows the playing track, its name with nothing playing; other rungs are left alone', () => {
    setIdealTrack(0, 0);
    assert.equal(idealSlotLabel(), 'Auto');
    assert.equal(tapsReadout(IDEAL_SLOT), 'Auto');
    assert.equal(tapsReadout(IDEAL_SLOT + 1), 'Auto S');
    assert.equal(tapsReadout(30_000_000), '30.0M Taps');
    assert.equal(idealTip(), null);
    const seen = [];
    const off = onIdealChange((t) => seen.push(t?.taps ?? null));
    setIdealTrack(300, 44100);
    setIdealTrack(300, 44100);
    assert.equal(shownTapsLabel('Auto'), '423M');
    assert.equal(shownTapsLabel('1G'), '1G');
    assert.equal(tapsReadout(IDEAL_SLOT), '423M Taps · Auto');
    assert.equal(tapsReadout(IDEAL_SLOT + 1), '423M Taps · Auto S');
    assert.match(idealTip(), /423,360,001 taps = 2 × 5:00 × 705\.6 kHz/);
    setIdealTrack(0, 0);
    off();
    assert.deepEqual(seen, [423_360_001, null]);
});

test('on the shifted grid the size carries a small superscript s, plain Auto the size alone', () => {
    setIdealTrack(300, 44100, 16, true);
    assert.equal(idealSlotLabel(IDEAL_SLOT + 1), '423Mˢ');
    assert.equal(tapsReadout(IDEAL_SLOT + 1), '423Mˢ Taps · Auto S');
    assert.equal(shownTapsLabel('Auto'), '423Mˢ');
    setIdealTrack(300, 44100, 16, false);
    assert.equal(idealSlotLabel(IDEAL_SLOT), '423M');
    assert.equal(tapsReadout(IDEAL_SLOT), '423M Taps · Auto');
    setIdealTrack(0, 0);
});

test('the Auto S mark is drawn as a real superscript s; other text is left alone', () => {
    assert.equal(markShiftHtml('423M' + SHIFT_MARK + ' Taps'), '423M<sup class="shift-s">s</sup> Taps');
    assert.equal(markShiftHtml('423M Taps · Auto'), '423M Taps · Auto');
    assert.equal(markShiftHtml('30M'), '30M');
});

test('the player chip: the own size of the playing track gets the mark on Auto S only', () => {
    setIdealTrack(300, 44100, 16, true);
    assert.equal(shownTapsLabel('423M'), '423M' + SHIFT_MARK);
    assert.equal(shownTapsLabel('30M'), '30M');
    setIdealTrack(300, 44100, 16, false);
    assert.equal(shownTapsLabel('423M'), '423M');
    setIdealTrack(0, 0);
});

test('a DSD output runs the chain at its DoP carrier: the track is sized there, not at the rack FS (20.11)', () => {
    assert.equal(chainFs(16), 16);
    assert.equal(chainFs(16, 128), 8);
    assert.equal(chainFs(16, 256), 16);
    assert.equal(chainFs(16, 64), 4);
    assert.equal(chainFs(8, 512), 16);
    assert.equal(chainFs(16, 1024), 16);
    // Joyride 96 kHz, 23 224 320 frames, DSD128: the engine's 185 794 560 taps (log 20.11 00:27).
    assert.equal(idealShort(idealTapsFor(23224320 / 96000, 96000, chainFs(16, 128), true).taps), '186M');
});

test("a track whose own size reads as a rung's label is its own filter, with the shift mark (20.11)", () => {
    // Barbie World, 48 kHz mp3, 6 236 840 frames at x8: the engine's 99 794 944 taps, "100M".
    setIdealTrack(6236840 / 48000, 48000, 8, true);
    try {
        assert.equal(isOwnSize('100M'), false);
        assert.equal(isTrackSize('100M'), true);
        assert.equal(autoBuilt('100M'), true, 'not the 100M rung: no switch that never ends');
        assert.equal(shownTapsLabel('100M'), '100M' + SHIFT_MARK);
        assert.equal(shownTapsLabel('Auto'), '100M' + SHIFT_MARK, 'the size on its way is the one the engine makes');
        assert.equal(autoBuilt('30M'), false, 'the quick start is still not it');
    } finally { setIdealTrack(0, 0); }
    assert.equal(isTrackSize('100M'), false);
});
