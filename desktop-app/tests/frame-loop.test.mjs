// frame-loop.js: a loop runs every frame while it has something to draw,
// rests (a timer, no frames) once it has not, and wakes at once on wake().
import test from 'node:test';
import assert from 'node:assert/strict';

let frames = [];
globalThis.requestAnimationFrame = cb => { frames.push(cb); return frames.length; };
globalThis.document = { addEventListener() {}, removeEventListener() {} };
const { restingLoop, overlayLoop, markActive, markData } = await import('../src/js/frame-loop.js');

const flush = () => { const f = frames; frames = []; for (const cb of f) cb(performance.now()); };

test('a loop asks for frames while its step returns true and rests on false', () => {
    frames = [];
    let busy = true, calls = 0;
    const l = restingLoop(() => { calls++; return busy; }, 10_000);
    flush(); flush();
    assert.equal(calls, 2);
    assert.equal(frames.length, 1);
    busy = false;
    flush();
    assert.equal(frames.length, 0, 'resting: no frame asked');
    l.wake();
    assert.equal(frames.length, 1, 'wake asks for a frame at once');
    l.stop();
    flush();
    assert.equal(calls, 3, 'a stopped loop draws nothing');
});

test('an overlay draws while the window is active and rests after the grace time', () => {
    frames = [];
    let draws = 0;
    const el = { offsetParent: {} };
    const o = overlayLoop(() => draws++, el, 0);
    markActive();
    flush();
    assert.equal(draws, 1);
    assert.equal(frames.length, 0, 'grace 0: rests after one frame');
    el.offsetParent = null;
    markActive();
    flush();
    assert.equal(draws, 1, 'off screen: nothing drawn');
    o.stop();
});

test('an overlay off screen rests even while the window is active', () => {
    frames = [];
    let draws = 0;
    const el = { offsetParent: null };
    const o = overlayLoop(() => draws++, el, 60_000);
    markActive();
    flush();
    assert.equal(draws, 0);
    assert.equal(frames.length, 0, 'another tab: no frames asked');
    el.offsetParent = {};
    markActive();
    flush();
    assert.equal(draws, 1, 'shown again: drawn on the next wake');
    assert.equal(frames.length, 1, 'and every frame while active');
    o.stop();
});

test('new data does not wake an overlay resting off screen; the pointer does', () => {
    frames = [];
    let draws = 0;
    const el = { offsetParent: null };
    const o = overlayLoop(() => draws++, el, 60_000);
    flush();
    assert.equal(frames.length, 0, 'hidden: resting');
    markData();
    assert.equal(frames.length, 0, 'data every frame: no frame asked for a hidden tab');
    el.offsetParent = {};
    markActive();
    assert.equal(frames.length, 1, 'the click that shows the tab wakes it');
    flush();
    assert.equal(draws, 1);
    markData();
    flush();
    assert.equal(draws, 2, 'on screen: data keeps it drawing');
    o.stop();
});

test('an overlay is skipped only while it stays empty at the same size', async () => {
    const { blankGuard } = await import('../src/js/frame-loop.js');
    const g = blankGuard();
    assert.equal(g.keep(true, 100, 50), false, 'first empty frame clears');
    assert.equal(g.keep(true, 100, 50), true, 'already blank: left as it is');
    assert.equal(g.keep(false, 100, 50), false, 'something to show (hover, a cursor): drawn');
    assert.equal(g.keep(false, 100, 50), false, 'and drawn again every frame');
    assert.equal(g.keep(true, 100, 50), false, 'gone again: cleared once');
    assert.equal(g.keep(true, 100, 50), true);
    assert.equal(g.keep(true, 200, 50), false, 'resized (window, DPR): cleared');
    assert.equal(g.keep(true, 200, 80), false, 'height too');
});
