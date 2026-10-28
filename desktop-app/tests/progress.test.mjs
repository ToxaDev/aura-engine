// The gold line under the analyzer's chain: the stages of the whole-track
// analysis, the whole weighed, its percentage on a tag riding at the fill's
// end, and what a stream (nothing to analyse) and a file make of it.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  STAGE_WEIGHT, TAG_GAP, TAG_GAP_Y, analysisStages, overallProgress, percentText, badgeLeft, placeBadge, progressTip, statusText,
} from '../src/js/analytics/progress.js';

const tiles = (ready, total, gen = 1) => ({ gen, ready, total });
const state = (info, oPct, opts) => {
  const stages = analysisStages(info, oPct, opts);
  return { stages, ...overallProgress(stages) };
};

test('a file just heard: S is decoding, the line shows at 0 %', () => {
  const s = state([tiles(0, 0), tiles(0, 0), tiles(0, 0)], null, { decoding: true });
  assert.equal(s.open, 1);
  assert.equal(s.frac, 0);
  assert.equal(percentText(s.frac), '0%');
  assert.match(progressTip(s.stages, s.open), /^Still analysing.*\nSource, the file as it is: decoding$/);
  assert.equal(statusText(s.stages, s.open), '');
});

test('the output pass weighs eight times a quick pass: the percentage runs as the time does', () => {
  assert.deepEqual(STAGE_WEIGHT, { s: 1, b: 1, o: 8 });
  // S and B done, O half way: (1 + 1 + 8 × 0.5) / 10.
  const half = state([tiles(256, 256), tiles(256, 256), tiles(0, 0)], 50);
  assert.equal(percentText(half.frac), '60%');
  assert.equal(half.open, 1);
  // S half way, B with no tiles yet (not counted), O queued at 0: 0.5 / 9.
  const early = state([tiles(128, 256), tiles(0, 0), tiles(0, 0)], 0);
  assert.equal(percentText(early.frac), '5%');
  assert.match(progressTip(early.stages, early.open),
    /Source, the file as it is: spectrogram 128 of 256\nOutput, the whole track through the chain heard: 0%$/);
});

test('words, not lone letters: each stage named, the output pass with what it has left', () => {
  const s = state([tiles(256, 256), tiles(10, 256), tiles(0, 0)], 24, { eta: ', about 12 s left' });
  const tip = progressTip(s.stages, s.open);
  assert.ok(tip.includes('Source, the file as it is: done'));
  assert.ok(tip.includes('Stages, the file through the source stages: spectrogram 10 of 256'));
  assert.ok(tip.includes('Output, the whole track through the chain heard: 24%, about 12 s left'));
  assert.ok(!/(^|\n)[SBO][, ]/.test(tip), 'no stage line begins with a letter alone');
});

test('BIT-PERFECT has no output pass: S and B alone make the whole', () => {
  const s = state([tiles(256, 256), tiles(128, 256), tiles(0, 0)], null);
  assert.equal(s.frac, 0.75);
  assert.equal(percentText(s.frac), '75%');
});

test('all done: 100 %, the line goes, the status bar says so', () => {
  const s = state([tiles(256, 256), tiles(256, 256), tiles(256, 256)], 100);
  assert.equal(s.open, 0);
  assert.equal(percentText(s.frac), '100%');
  assert.equal(progressTip(s.stages, s.open).split('\n')[0], 'The whole-track analysis is complete');
  assert.equal(statusText(s.stages, s.open), 'Whole track analysed');
});

test('a stream to a file and back: nothing waits on the stream, the file starts at once', () => {
  // The stream: no whole track (its frame has no tiles, no O pass), nothing decoding.
  const stream = state([tiles(0, 0), tiles(0, 0), tiles(0, 0)], null, { decoding: false });
  assert.equal(stream.stages.length, 0);
  assert.equal(stream.open, 0, 'the line stays away');
  assert.equal(statusText(stream.stages, stream.open), '', 'nothing said');
  // The file played next: its S decoding, its O pass queued — the line is up.
  const file = state([tiles(0, 0), tiles(0, 0), tiles(0, 0)], 0, { decoding: true });
  assert.equal(file.open, 2);
  assert.equal(percentText(file.frac), '0%');
  // The stream again: away.
  const again = state([tiles(0, 0), tiles(0, 0), tiles(0, 0)], null, { decoding: false });
  assert.equal(again.open, 0);
});

test('the percentage never reads 100 before all is done', () => {
  assert.equal(percentText(0.999), '99%');
  assert.equal(percentText(0.29), '29%');
  assert.equal(percentText(1), '100%');
  assert.equal(percentText(-0.1), '0%');
  assert.equal(percentText(1.4), '100%');
});

test('the tag rides on the end of the fill and stays whole inside the line', () => {
  const W = 1000, bw = 30;
  assert.equal(badgeLeft(0.5, W, bw), 485, 'centred on the end');
  assert.equal(badgeLeft(0, W, bw), 0, 'at the start against the left end');
  assert.equal(badgeLeft(0.01, W, bw), 0);
  assert.equal(badgeLeft(1, W, bw), W - bw, 'at the end against the right one');
  assert.equal(badgeLeft(0.995, W, bw), W - bw);
  assert.equal(badgeLeft(0.5, 20, bw), 0, 'a line narrower than the tag');
  for (let f = 0; f <= 1; f += 0.05) {
    const x = badgeLeft(f, W, bw);
    assert.ok(x >= 0 && x + bw <= W, `inside at ${f}`);
  }
});

// The window of the 3.10 run (1180 px): the line from x 17, 1146 px wide;
// Snapshot 12–93 on the left, B 1053–1080 and Export 1086–1168 on the
// right, their tops at y 109; the tag, 29 px wide, sits on the line with its
// bottom at y 109 too (it touched B at 93 %).
const RUN = { lineLeft: 17, W: 1146, bw: 29, bottom: 109,
  controls: [[12, 93], [1053, 1080], [1086, 1168]].map(([left, right]) => ({ left, right, top: 109 })) };
const place = (f, g = RUN) => placeBadge(f, g.lineLeft, g.W, g.bw, g.bottom, g.controls);

test('at 100 % the tag ends where the line ends, the button under that end or not', () => {
  // It waited beside the button under the end: 116 px short here, ten on
  // Anton's screen (the line to x 1085, the tag to 1075).
  const p = place(1);
  assert.equal(p.left + RUN.bw, RUN.W, 'its right edge on the end of the fill');
  assert.ok(p.lift > 0, 'risen over Export');
  // Anton's window: the line ends at 1085, B and Export under its end.
  const anton = { lineLeft: 17, W: 1068, bw: 29, bottom: 109,
    controls: [{ left: 12, right: 93, top: 109 }, { left: 975, right: 1002, top: 109 }, { left: 1008, right: 1090, top: 109 }] };
  const a = place(1, anton);
  assert.equal(anton.lineLeft + a.left + anton.bw, 1085);
  // Nothing under the end: on the line as it is.
  assert.deepEqual(placeBadge(1, 17, 1146, 29, 109, []), { left: 1146 - 29, lift: 0 });
});

test('the tag rides on the end of the fill all the way; over a button it rises clear of it', () => {
  for (let i = 0; i <= 100; i++) {
    const f = i / 100;
    const { left, lift } = place(f);
    assert.equal(left, badgeLeft(f, RUN.W, RUN.bw), `${f}: on the end of the fill`);
    const x0 = RUN.lineLeft + left, x1 = x0 + RUN.bw;
    const near = RUN.controls.filter(c => c.right > x0 - TAG_GAP && c.left < x1 + TAG_GAP);
    for (const c of near) assert.ok(RUN.bottom - lift <= c.top - TAG_GAP_Y, `${f}: ${c.top - (RUN.bottom - lift)} px above the button`);
    if (!near.length) assert.equal(lift, 0, `${f}: on the line`);
  }
  // Snapshot under the start, B at 93 %, nothing in the middle.
  assert.ok(place(0).lift > 0 && place(0.03).lift > 0);
  assert.ok(place(0.93).lift > 0);
  assert.deepEqual(place(0.5), { left: 0.5 * RUN.W - RUN.bw / 2, lift: 0 });
});

test('a narrow window: buttons under the whole line, the tag still on the fill, always clear of them', () => {
  const g = { lineLeft: 8, W: 200, bw: 29, bottom: 109,
    controls: [{ left: 4, right: 100, top: 109 }, { left: 106, right: 215, top: 112 }] };
  for (const f of [0, 0.2, 0.5, 0.8, 1]) {
    const { left, lift } = place(f, g);
    assert.ok(left >= 0 && left + g.bw <= g.W, `${f}: inside the line`);
    assert.equal(left, badgeLeft(f, g.W, g.bw));
    const x0 = g.lineLeft + left, x1 = x0 + g.bw;
    for (const c of g.controls.filter(c => c.right > x0 - TAG_GAP && c.left < x1 + TAG_GAP)) {
      assert.ok(g.bottom - lift <= c.top - TAG_GAP_Y, `${f}: clear of the button`);
    }
  }
  // Over the left group it rises; the right one, 3 px lower, leaves room enough.
  assert.ok(place(0, g).lift >= TAG_GAP_Y);
  assert.equal(place(1, g).lift, 0);
});
