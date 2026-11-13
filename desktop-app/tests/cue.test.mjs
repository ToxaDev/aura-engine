// Whom Cue Consistency plays for, as the rack row and the player say it.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { cueMode, initCueBridge, initCueLine } from '../src/js/cue.js';
import { cb } from '../src/js/dsprack.js';

const src = path => readFileSync(new URL(`../src/${path}`, import.meta.url), 'utf8');

test('XTC on is Speakers, XTC off is Headphones, and the tip says why', () => {
    assert.equal(cueMode(true).label, 'Speakers');
    assert.equal(cueMode(false).label, 'Headphones');
    assert.match(cueMode(true).tip, /XTC delivers/);
    assert.match(cueMode(false).tip, /switch XTC on/);
});

test('the rack row and the player read the mode from XTC\'s badge', () => {
    assert.match(src('js/dsprack.js'), /id === 'convLabCue' && isOn\(id\)[^\n]*cueMode\(isOn\('convLabXtc'\)\)/);
    assert.match(src('js/player.js'), /cueMode\(isRackOn\('convLabXtc'\)\)/);
});

test('the letter is on a playing or coming CUE and while stopped, not on one that does not run', () => {
    const player = src('js/player.js');
    assert.match(player, /tok === 'CUE' && \(cls === 'fired' \|\| cls === 'pending'\)/);
    assert.match(player, /id === 'convLabCue' \? cueMode\(isRackOn\('convLabXtc'\)\) : null/);
    assert.equal('mark' in cueMode(true), false);
});

test('a badge is its token alone, no letter in the corner', () => {
    assert.match(cb('fired', 'CUE', 'Cue Consistency', 'cue', 'why'), />CUE<\/span>$/);
    assert.match(cb('fired', 'XTC', 'Crosstalk', 'xtc', 'why'), />XTC<\/span>$/);
});

function fakeTauri() {
    const on = new Map();
    const sent = [];
    const event = {
        listen: (name, fn) => { on.set(name, fn); return Promise.resolve(); },
        emit: (name, payload) => { sent.push([name, payload]); on.get(name)?.({ payload }); return Promise.resolve(); },
    };
    return { event, core: { invoke: () => Promise.resolve() }, sent };
}

function fakeButton() {
    const handlers = {};
    return {
        textContent: '', cls: new Set(), attrs: {},
        classList: { toggle(c, v) { v ? this.owner.cls.add(c) : this.owner.cls.delete(c); } },
        setAttribute(k, v) { this.attrs[k] = v; },
        addEventListener(k, fn) { handlers[k] = fn; },
        click() { handlers.click?.(); },
    };
}

test('the line in XTC\'s window asks, shows and switches the rack\'s row', async () => {
    const t = fakeTauri();
    globalThis.window = { __TAURI__: t };
    globalThis.localStorage = { getItem: () => null, setItem() {} };
    let row = true;
    let held = false;
    initCueBridge(() => {}, () => row, (want) => { if (!held) row = want; });
    const onBtn = fakeButton(); onBtn.classList.owner = onBtn;
    initCueLine(onBtn, fakeButton());
    await Promise.resolve(); await Promise.resolve();
    assert.equal(onBtn.textContent, 'On', 'it read the row when it opened');

    onBtn.click();
    assert.equal(row, false);
    assert.equal(onBtn.textContent, 'Off');

    held = true;
    onBtn.click();
    assert.equal(row, false, 'a held rack does not switch');
    assert.equal(onBtn.textContent, 'Off', 'and the line says so');
    delete globalThis.window;
    delete globalThis.localStorage;
});

test('every stage has a colour of its own', () => {
    const css = src('css/dsprack.css');
    const colours = [...css.matchAll(/--lab-(\w+):\s*(#[0-9a-fA-F]{3,8});/g)].map(m => [m[1], m[2].toLowerCase()]);
    assert.ok(colours.length >= 10);
    const seen = new Map();
    for (const [name, hex] of colours) {
        assert.equal(seen.get(hex), undefined, `${name} repeats ${seen.get(hex)}'s colour ${hex}`);
        seen.set(hex, name);
    }
});

import {
    depthOf, imageOf, speakerDeg, degText, followSources, playedDeg, itdDeg, originalDeg,
    newWave, mergeWave, waveAt, steppedBack, chasePlayhead, followRef, centroidOf, colourAt, widthOf,
} from '../src/js/cue-scene.js';

test('a quiet column beside a loud one is thin but not a hairline, a medium one clearly thicker', () => {
    const ref = 1;
    assert.equal(widthOf(1, ref), 1);
    assert.equal(widthOf(-2, ref), -1);
    assert.equal(widthOf(0, ref), 0);
    const quiet = widthOf(Math.pow(10, -30 / 20), ref);
    const medium = widthOf(Math.pow(10, -18 / 20), ref);
    const loud = widthOf(Math.pow(10, -6 / 20), ref);
    // Quiet still shows; each step up is clearly thicker.
    assert.ok(quiet >= .15 && quiet < .3, `quiet ${quiet}`);
    assert.ok(medium > quiet + .2, `medium ${medium}`);
    assert.ok(loud > medium + .2 && loud < .9, `loud ${loud}`);
    // Far under the programme it fades out: silence stays silence.
    assert.ok(widthOf(Math.pow(10, -50 / 20), ref) < .1);
    assert.equal(widthOf(Math.pow(10, -60 / 20), ref), 0);
    // Below, the sign is kept.
    assert.ok(Math.abs(widthOf(-.1, ref) + widthOf(.1, ref)) < 1e-12);
});

test('the stage marks 0° in the middle, no head drawn over it', () => {
    const s = src('js/cue-scene.js');
    assert.ok(!/function head\(/.test(s));
    assert.ok(s.includes("'0°'"));
});

test('a dry place stands near, a reverberant one far', () => {
    assert.equal(depthOf(1), 0);
    assert.equal(depthOf(0.2), 1);
    assert.ok(depthOf(0.9) < depthOf(0.7));
});

test('an image reads against the loudest band, inside its range', () => {
    const v = imageOf([-10, -10, -10, -100], -10);
    assert.equal(v[0], 1);
    assert.equal(v.length, 4);
    assert.ok(v[3] < v[2] && v[3] >= 0);
});

test('the speakers stand where the geometry puts them', () => {
    assert.equal(speakerDeg(null), 30);
    assert.ok(Math.abs(speakerDeg({ span: 2000, distL: 2000, distR: 2000 }) - 30) < 0.01);
});

test('left is positive, as the stage reads it', () => {
    assert.equal(degText(22.4), 'L 22°');
    assert.equal(degText(-37), 'R 37°');
    assert.equal(degText(0.2), '0°');
});

test('a source is followed by its number, a lost one fades, a new one comes in', () => {
    const make = d => ({ deg: d.deg, w: 1, z: 0, sp: [0], direct: 1 });
    const shown = followSources([], [{ id: 7, deg: 10 }], make);
    assert.equal(shown.length, 1);
    assert.equal(shown[0].a, 0);
    assert.equal(shown[0].wave.last, -1);
    // Another source near the first one's place is still another source.
    followSources(shown, [{ id: 7, deg: 30 }, { id: 8, deg: 11 }], make);
    assert.equal(shown.length, 2);
    assert.equal(shown[0].to.deg, 30);
    assert.equal(shown[1].id, 8);
    followSources(shown, [{ id: 8, deg: 11 }], make);
    assert.equal(shown[0].goal, 0);
    assert.equal(shown[1].goal, 1);
    // Found again before it faded: the same column comes back.
    followSources(shown, [{ id: 7, deg: 30 }, { id: 8, deg: 11 }], make);
    assert.equal(shown.length, 2);
    assert.equal(shown[0].goal, 1);
});

test('overlapping waves join by hop number, only newer hops taken', () => {
    const h = newWave();
    mergeWave(h, [-.1, .1, -.2, .2, -.3, .3], 12, 100);
    assert.equal(h.last, 12);
    assert.deepEqual(waveAt(h, 10), [-.1, .1]);
    // The next view repeats hops 11 and 12 and brings 13 and 14.
    mergeWave(h, [-.9, .9, -.9, .9, -.4, .4, -.5, .5], 14, 100);
    assert.equal(h.lo.length, 5);
    assert.deepEqual(waveAt(h, 12), [-.3, .3]);
    assert.deepEqual(waveAt(h, 14), [-.5, .5]);
    // Nothing new: nothing changes.
    mergeWave(h, [-.9, .9], 14, 100);
    assert.equal(h.lo.length, 5);
    // A hop never read is silence; beyond what is kept too.
    mergeWave(h, [-.6, .6], 17, 100);
    assert.deepEqual(waveAt(h, 15), [0, 0]);
    assert.deepEqual(waveAt(h, 17), [-.6, .6]);
    assert.deepEqual(waveAt(h, 99), [0, 0]);
    // Only the last `keep` hops stay.
    mergeWave(h, [-.7, .7, -.8, .8], 19, 4);
    assert.equal(h.lo.length, 4);
    assert.deepEqual(waveAt(h, 16), [0, 0]);
    assert.deepEqual(waveAt(h, 19), [-.8, .8]);
    // A jump further than is kept starts over.
    mergeWave(h, [-.2, .2], 500, 4);
    assert.equal(h.lo.length, 1);
});

test('a view stepping back is another chain, the clock\'s jitter is not', () => {
    assert.equal(steppedBack(null, 0), false);
    assert.equal(steppedBack(100, 101), false);
    assert.equal(steppedBack(100, 99), false);
    assert.equal(steppedBack(100, 40), true);
    assert.equal(steppedBack(100, 0), true);
});

test('the playhead runs at the hops\' rate, chases where it should be, holds at the newest', () => {
    const rate = 86;
    // On time: one frame moves it a frame's worth of hops.
    assert.ok(Math.abs(chasePlayhead(100, 100, 120, rate, .033) - (100 + rate * .033)) < 1e-9);
    // Behind: faster; ahead: slower, never backwards.
    assert.ok(chasePlayhead(100, 110, 120, rate, .033) > 100 + rate * .033);
    const slow = chasePlayhead(100, 95, 120, rate, .033);
    assert.ok(slow < 100 + rate * .033 && slow >= 100);
    // A pause: the newest hop holds it.
    assert.equal(chasePlayhead(119.9, 125, 120, rate, .033), 120);
    // Far off, or nothing shown yet: it jumps.
    assert.equal(chasePlayhead(100, 400, 410, rate, .033), 400);
    assert.equal(chasePlayhead(null, 50, 60, rate, .033), 50);
});

test('the columns\' scale climbs slowly, falls slowly, and starts at once from silence', () => {
    // From silence: the first wave sets it.
    assert.equal(followRef(1e-3, .05, .3), .3);
    // A kick 10 dB over the rest moves it under a dB in a 0.15 s hit.
    const hit = followRef(.1, .15, .316);
    assert.ok(hit > .1 && hit < .1 * Math.pow(10, 1 / 20));
    // Quiet: it falls 1.5 dB a second.
    const p = followRef(.5, 1, 0);
    assert.ok(p < .5 && p > .4);
    assert.ok(followRef(1e-6, 10, 0) > 0);
    // A beat of kicks every half second: the scale settles, it does not pump.
    let r = .1, lo = 1, hi = 0;
    for (let i = 0; i < 400; i++) {
        const t = (i * .01) % .5;
        r = followRef(r, .01, t < .15 ? .316 : .1);
        if (i >= 300) { lo = Math.min(lo, r); hi = Math.max(hi, r); }
    }
    assert.ok(20 * Math.log10(hi / lo) < 1.5, `ripple ${20 * Math.log10(hi / lo)} dB`);
});

test('a source\'s colour is where its sound lies: warm low, blue high', () => {
    assert.equal(centroidOf([1, 0, 0, 0, 0]), 0);
    assert.equal(centroidOf([0, 0, 0, 0, 1]), 1);
    assert.equal(centroidOf([1, 1, 1]), .5);
    assert.equal(centroidOf([0, 0, 0]), .5);
    const low = colourAt(centroidOf([1, .5, 0, 0])), high = colourAt(centroidOf([0, 0, .5, 1]));
    assert.ok(low[0] > low[2], 'low reads warm');
    assert.ok(high[2] > high[0], 'high reads blue');
    assert.deepEqual(colourAt(-1), colourAt(0));
    assert.deepEqual(colourAt(2), colourAt(1));
});

test('Stage width narrows the angle a place is played at, as the stage does', () => {
    assert.equal(playedDeg(60, 90), 60);
    assert.equal(playedDeg(-90, 30), -30);
    assert.equal(playedDeg(45, 60), 30);
});

test('ITD scale reads as the angle whose time difference it gives', () => {
    assert.ok(Math.abs(itdDeg(30, 100) - 30) < 0.01);
    assert.ok(itdDeg(30, 150) > 40 && itdDeg(30, 150) < 50);
    assert.ok(itdDeg(-30, 50) < -10 && itdDeg(-30, 50) > -20);
    assert.equal(itdDeg(80, 150), 90);
    assert.equal(itdDeg(0, 150), 0);
});

test('before the stage a place stands between the speakers, a hard pan at one', () => {
    assert.equal(originalDeg(90, 30), 30);
    assert.equal(originalDeg(-45, 30), -15);
    assert.equal(playedDeg(90, 30), originalDeg(90, 30));
});
