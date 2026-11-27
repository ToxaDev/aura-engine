// hpprep.js: the ▶ button's preparing fill — the state machine, the start
// watch and the poller, without a page.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createPrepModel, startWatch, createPoller, createRunner, paint, pctText, POLL_MS, ARM_MS, DISSOLVE_MS, FADE_MS, SHOW_MS } from '../src/js/hpprep.js';

const prep = (p, track = 1) => ({ state: 'preparing', progress: p, stage: 'stems', track, eta_s: 5 });
/// Frames every 16 ms from t to t + ms; the last frame's answer.
function run(m, t, ms) {
    let f = null;
    for (let x = 0; x <= ms; x += 16) f = m.frame(t + x);
    return f;
}

test('nothing is shown until the engine says preparing', () => {
    const m = createPrepModel();
    assert.equal(m.phase, 'off');
    m.arm(0);
    assert.equal(m.phase, 'armed');
    assert.equal(m.live(), false);
    assert.equal(m.frame(16).draw, false);
    m.answer(prep(0.1), 100);
    assert.equal(m.phase, 'filling');
    assert.equal(m.live(), true);
    assert.equal(m.frame(116).draw, true);
});

test('skipped and an instant ready of the track asked for show nothing and end the asking', () => {
    // Lists kept on disk (skipped) or made between two answers (ready): no
    // fill, but HP is lit until its sound (HL31); failed lights nothing.
    for (const [state, lit] of [['skipped', true], ['failed', false], ['ready', true]]) {
        const m = createPrepModel();
        m.arm(0, 7);
        m.answer({ state, progress: 0, track: 7 }, 50);
        assert.equal(m.phase, 'off', state);
        assert.equal(m.wantPoll(60), false, state);
        assert.equal(m.live(), false, state);
        assert.equal(m.litFor(7), lit, state);
        assert.equal(m.litFor(8), false, state);
    }
});

test("Anton's 1G αHP start with the lists on disk: HP lit from ▶ through the quick variant until αHP is heard (HL31)", () => {
    // Anton 31.10 04:06 (Latest 47b2b4f): hp_prepare skipped (lists from the
    // disk), sound at +1.4 s on the quick variant without αHP (its unit went
    // dark), the full chain with αHP heard only at +6.6 s.
    const m = createPrepModel(), watch = startWatch(m);
    const quick = { source: 'live', stages: [['ISP', 1, ''], ['XTC', 1, '']], quick: true, hpDeferred: true };
    const full = { source: 'live', stages: [['ISP', 1, ''], ['αHP', 1, ''], ['XTC', 1, '']], quick: false, hpDeferred: false };
    assert.equal(watch({ state: 'preparing', trackId: 19, audible: null }, 0), true);
    m.answer({ state: 'idle', track: null }, 40);
    assert.equal(m.litFor(19), false);
    m.answer({ state: 'skipped', track: 19 }, 140);
    for (let t = 200; t < 6600; t += 100) {
        const s = t < 1400 ? { state: 'preparing', trackId: 19, audible: null }
            : { state: 'playing', trackId: 19, audible: quick, jump: { pending: t < 1500 } };
        watch(s, t);
        assert.equal(m.litFor(19), true, `dark at ${t} ms`);
    }
    watch({ state: 'playing', trackId: 19, audible: full }, 6600);
    assert.equal(m.litFor(19), false);
});

test('the prepared lighting ends when the full chain plays without HP, not while the track before is still heard (HL31)', () => {
    const lit = () => { const m = createPrepModel(); m.arm(0, 2); m.answer({ state: 'skipped', track: 2 }, 50); return m; };
    // The full chain on the air without HP (and not deferred): it is not coming.
    let m = lit();
    startWatch(m)({ state: 'playing', trackId: 2, audible: { stages: [['ISP', 1, '']], quick: false, hpDeferred: false } }, 100);
    assert.equal(m.litFor(2), false);
    // Deferred (its envelope made in the background): still at work.
    m = lit();
    startWatch(m)({ state: 'playing', trackId: 2, audible: { stages: [], quick: false, hpDeferred: true } }, 100);
    assert.equal(m.litFor(2), true);
    // The jump to track 2 not on the air yet: the chain heard is track 1's.
    m = lit();
    startWatch(m)({ state: 'playing', trackId: 2, jump: { pending: true }, audible: { stages: [['HP', 1, '']], quick: false } }, 100);
    assert.equal(m.litFor(2), true);
});

test('while a start asks, idle and answers about another track change nothing (HL29)', () => {
    // Anton 31.10: the first answers after ▶ are idle (the engine has not
    // begun) or still about the track before; they used to end the asking,
    // and the preparing that came after was never seen — HP stayed dark.
    for (const a of [{ state: 'idle', track: null }, { state: 'ready', track: 6 }, { state: 'skipped', track: 6 }, { state: 'preparing', track: 6, progress: 0.4 }]) {
        const m = createPrepModel();
        m.arm(0, 7);
        m.answer({ progress: 1, ...a }, 50);
        assert.equal(m.phase, 'armed', a.state);
        m.answer({ state: 'preparing', progress: 0.1, track: 7 }, 150);
        assert.equal(m.phase, 'filling', a.state);
        assert.equal(m.litFor(7), true, a.state);
    }
    // ▶ before the track is known: no answer about a track ends it either.
    const m = createPrepModel();
    m.arm(0);
    m.answer({ state: 'ready', progress: 1, track: 6 }, 50);
    assert.equal(m.phase, 'armed');
});

test('the start watch keeps asking while the track is made ready, until the engine answers for it (HL29)', () => {
    const m = createPrepModel();
    const watch = startWatch(m);
    assert.equal(watch({ trackId: 7, state: 'preparing' }, 0), true);
    // The arming runs out (a slow start) — the next status asks again.
    assert.equal(m.wantPoll(ARM_MS + 10), false);
    assert.equal(watch({ trackId: 7, state: 'preparing' }, ARM_MS + 20), true);
    assert.equal(m.phase, 'armed');
    // Nothing to prepare for this track: it stops for good.
    m.answer({ state: 'skipped', progress: 0, track: 7 }, ARM_MS + 30);
    assert.equal(watch({ trackId: 7, state: 'preparing' }, ARM_MS + 300), false);
    assert.equal(m.wantPoll(ARM_MS + 310), false);
});

test('the live sequence of run A2: HP lit from its prepare until heard, through the quick variant (HL29)', () => {
    // hl29\runA2 (Anton's exe 670274b, Dinero): status and hp_prepare
    // answers as they came, ms from ▶. Before: HP's unit went dark at 17.53 s
    // (the strands gone, the quick variant without HP on the air) and lit
    // again at 18.03 s when HP was heard.
    const m = createPrepModel();
    const watch = startWatch(m);
    const quick = { source: 'live', stages: [['DC', 2], ['ISP', 1], ['SUB15', 1], ['AHR', 2], ['AA', 2], ['PFR', 1]], quick: true };
    const full = { source: 'live', stages: [...quick.stages, ['HP', 1]], quick: false };
    const ev = [
        [1520, 'arm'],
        [1690, 'hp', { state: 'idle', track: null }],
        [1690, 'st', { state: 'preparing', trackId: 1, audible: null }],
        [1790, 'hp', { state: 'preparing', track: 1, progress: 0 }],
        [9000, 'hp', { state: 'preparing', track: 1, progress: 0.5 }],
        [16110, 'st', { state: 'preparing', trackId: 1, audible: null }],
        [16130, 'hp', { state: 'ready', track: 1, progress: 1 }],
        [16280, 'st', { state: 'playing', trackId: 1, audible: quick }],
        [17460, 'st', { state: 'playing', trackId: 1, audible: quick }],
        [18030, 'st', { state: 'playing', trackId: 1, audible: full }],
    ];
    const lit = [];
    for (let t = 1500, i = 0; t <= 19000; t += 16) {
        while (i < ev.length && ev[i][0] <= t) {
            const [, k, x] = ev[i++];
            if (k === 'arm') m.arm(t);
            else if (k === 'hp') m.answer({ progress: 0, ...x }, t);
            else watch(x, t);
        }
        m.frame(t);
        lit.push([t, m.litFor(1)]);
    }
    const first = lit.find(([, on]) => on)[0];
    assert.ok(first < 1900, `lit at ${first}`);
    const dark = lit.filter(([t, on]) => t > first && t < 18030 && !on);
    assert.deepEqual(dark, [], 'never dark between its prepare and its sound');
    // Once heard, the prepare no longer lights it (a later switch-in breathes).
    watch({ state: 'playing', trackId: 1, audible: full }, 19100);
    assert.equal(m.litFor(1), false);
});

test('the prepared lighting ends with a stop, another track, or HP held off the track', () => {
    const ready = () => {
        const m = createPrepModel();
        m.arm(0, 1);
        m.answer({ state: 'preparing', track: 1, progress: 0.5 }, 10);
        m.answer({ state: 'ready', track: 1, progress: 1 }, 20);
        for (let t = 20; t < 3000; t += 16) m.frame(t);
        assert.equal(m.litFor(1), true);
        return m;
    };
    let m = ready(); startWatch(m)({ state: 'stopped', trackId: null }, 3000); assert.equal(m.litFor(1), false);
    m = ready(); startWatch(m)({ state: 'playing', trackId: 2, audible: null }, 3000); assert.equal(m.litFor(1), false);
    m = ready(); startWatch(m)({ state: 'playing', trackId: 1, audible: { stages: [], hpHeld: true } }, 3000); assert.equal(m.litFor(1), false);
});

test('armed gives up when no answer comes', () => {
    const m = createPrepModel();
    m.arm(0);
    assert.equal(m.wantPoll(ARM_MS - 1), true);
    assert.equal(m.wantPoll(ARM_MS + 1), false);
    assert.equal(m.phase, 'off');
});

test('the fill eases toward the progress and never goes back', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.5), 0);
    let last = 0;
    for (let t = 0; t < 3000; t += 16) {
        const f = m.frame(t);
        assert.ok(f.fill >= last, `fell back at ${t}`);
        last = f.fill;
    }
    assert.ok(last > 0.45 && last < 0.6, String(last));
    // A lower number (a jitter, a new stage counted from 0) changes nothing.
    m.answer(prep(0.2), 3000);
    const f = run(m, 3000, 1000);
    assert.ok(f.fill >= last);
});

test('a stall still creeps, but stays near the number and under the top', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.3), 0);
    const a = run(m, 0, 2000).fill;
    const b = run(m, 2016, 6000).fill;
    assert.ok(b > a, 'no motion while stalled');
    assert.ok(b <= 0.3 + 0.061, String(b));
    m.answer(prep(0.99), 9000);
    assert.ok(run(m, 9000, 20000).fill <= 0.99 + 1e-9);
});

test('ready fills to the top, then the backdrop dissolves and it ends', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.4), 0);
    run(m, 0, 500);
    m.answer({ state: 'ready', progress: 1, track: 1 }, 600);
    assert.equal(m.phase, 'finishing');
    let t = 600, f;
    while (m.phase === 'finishing' && t < 5000) f = m.frame(t += 16);
    assert.equal(m.phase, 'dissolving');
    assert.ok(t < 2500, 'finishing took too long');
    f = m.frame(t + DISSOLVE_MS / 2);
    assert.equal(f.draw, true);
    assert.equal(f.fill, 1);
    assert.ok(f.veil > 0 && f.veil < 1);
    assert.ok(f.rise > 0);
    assert.equal(m.frame(t + DISSOLVE_MS + 1).draw, false);
    assert.equal(m.phase, 'off');
});

test('the strands come in over SHOW_MS while the view and the controls are hidden', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.1), 100);
    const f0 = m.frame(100), fm = m.frame(100 + SHOW_MS / 2), f1 = m.frame(100 + SHOW_MS);
    assert.equal(f0.alpha, 0);
    assert.ok(fm.alpha > 0 && fm.alpha < 1 && fm.veil === fm.alpha);
    assert.equal(f1.alpha, 1);
    for (const f of [f0, fm, f1]) assert.equal(f.hide, true);
    // another track while it shows: the strands stay as they are
    m.answer(prep(0.05, 2), 600);
    assert.equal(m.frame(616).alpha, 1);
    // done: the view and the controls come back as the strands dissolve
    m.answer({ state: 'ready', progress: 1, track: 2 }, 700);
    assert.equal(m.frame(716).hide, true);
    let t = 716;
    while (m.phase === 'finishing' && t < 5000) m.frame(t += 16);
    assert.equal(m.phase, 'dissolving');
    const d = m.frame(t + 16);
    assert.equal(d.draw, true);
    assert.ok(!d.hide);
});

test('a fill that ends before it came in fades from where it was', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.1), 0);
    m.frame(0);
    m.answer({ state: 'failed', progress: 0.1 }, SHOW_MS / 4);
    const f = m.frame(SHOW_MS / 4 + 1);
    assert.ok(f.alpha < 0.2 && !f.hide, String(f.alpha));
});

test('failed (or skipped mid-way) fades out quietly, without finishing the fill', () => {
    for (const state of ['failed', 'skipped', 'idle']) {
        const m = createPrepModel();
        m.arm(0);
        m.answer(prep(0.4), 0);
        const before = run(m, 0, 1000).fill;
        m.answer({ state, progress: 0.4 }, 1100);
        assert.equal(m.phase, 'fading', state);
        const f = m.frame(1100 + FADE_MS / 2);
        assert.ok(f.fill <= before + 0.01, state);
        assert.ok(f.alpha < 1 && f.alpha > 0, state);
        assert.equal(m.frame(1100 + FADE_MS + 1).draw, false, state);
        assert.equal(m.phase, 'off', state);
    }
});

test('another track preparing starts the fill from the bottom', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.7, 1), 0);
    assert.ok(run(m, 0, 3000).fill > 0.5);
    m.answer(prep(0.05, 2), 3100);
    assert.equal(m.track, 2);
    assert.ok(m.frame(3116).fill < 0.1);
});

test('missing answers give up after a while, but one miss does not', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.3), 0);
    m.answer(null, 100);
    assert.equal(m.phase, 'filling');
    for (let i = 0; i < 40; i++) m.answer(null, 200 + i * 100);
    assert.equal(m.phase, 'fading');
});

test('polls only while armed or filling', () => {
    const m = createPrepModel();
    assert.equal(m.wantPoll(0), false);
    m.arm(0);
    assert.equal(m.wantPoll(10), true);
    m.answer(prep(0.1), 20);
    assert.equal(m.wantPoll(5000), true);
    m.answer({ state: 'ready', progress: 1, track: 1 }, 5000);
    assert.equal(m.wantPoll(5010), false);
});

test('the start watch arms on a new track and on a start, not on every status', () => {
    const m = createPrepModel();
    const w = startWatch(m);
    assert.equal(w({ state: 'stopped', trackId: null }, 0), false);
    assert.equal(w({ state: 'preparing', trackId: 3 }, 10), true);
    assert.equal(w({ state: 'preparing', trackId: 3 }, 20), false);
    assert.equal(w({ state: 'playing', trackId: 3 }, 30), false);
    assert.equal(w({ state: 'paused', trackId: 3 }, 40), false);
    assert.equal(w({ state: 'playing', trackId: 3 }, 50), true);
    assert.equal(w({ state: 'playing', trackId: 4 }, 60), true);
    assert.equal(w({ state: 'stopped', trackId: 4 }, 70), false);
});

test('the poller asks while wanted, one request at a time, and stops', async () => {
    const m = createPrepModel();
    let clock = 0, timers = [], asked = 0, inFlight = 0, maxInFlight = 0;
    const answers = [prep(0.2), prep(0.5), { state: 'ready', progress: 1, track: 1 }];
    const p = createPoller(m, {
        now: () => clock,
        setTimer: (fn, ms) => { timers.push({ fn, ms }); return timers.length; },
        clearTimer: () => {},
        fetchJson: async () => {
            inFlight++; maxInFlight = Math.max(maxInFlight, inFlight); asked++;
            await null;
            inFlight--;
            return answers[Math.min(asked - 1, answers.length - 1)];
        },
    });
    m.arm(0);
    p.kick();
    p.kick(); // a second start while asking: still one request
    await new Promise(r => setTimeout(r, 0));
    assert.equal(maxInFlight, 1);
    assert.equal(m.phase, 'filling');
    while (timers.length) {
        const { fn, ms } = timers.shift();
        assert.equal(ms, POLL_MS);
        clock += ms;
        await fn();
    }
    assert.equal(m.phase, 'finishing');
    assert.equal(asked, 3);
    assert.equal(p.pending, false);
});

test('paint clears when there is nothing, and masks the strands to the progress', () => {
    const calls = [], stops = [];
    const grad = { addColorStop: (o, c) => stops.push([o, c]) };
    const ctx = new Proxy({}, {
        get: (o, k) => (k in o ? o[k] : (...a) => { calls.push(k); return grad; }),
        set: (o, k, v) => { o[k] = v; return true; },
    });
    paint(ctx, 300, 40, { draw: false }, 0);
    assert.deepEqual(calls, ['clearRect']);
    calls.length = 0;
    paint(ctx, 300, 40, { draw: true, fill: 0.5, veil: 1, alpha: 1, rise: 0 }, 1.2);
    assert.ok(calls.filter(c => c === 'stroke').length >= 6);
    // The mask: opaque up to the front at half the width, faint after it.
    assert.ok(stops.some(([o, c]) => o === 0.5 && c === 'rgba(0,0,0,0.16)'));
    assert.ok(!calls.includes('arc'));
});

test('the percent readout is whole, 0..100', () => {
    assert.equal(pctText(0), '0');
    assert.equal(pctText(0.474), '47');
    assert.equal(pctText(1), '100');
    assert.equal(pctText(1.3), '100');
    assert.equal(pctText(NaN), '0');
});

test('working: from the first "preparing" until the strands are gone, not while only armed', () => {
    const m = createPrepModel();
    m.arm(0);
    assert.equal(m.working(), false, 'armed: nothing heard yet');
    m.answer(prep(0.2), 100);
    assert.equal(m.working(), true);
    m.answer({ state: 'ready' }, 200);
    assert.equal(m.working(), true, 'finishing');
    let t = 200;
    while (m.phase === 'finishing' && t < 10000) m.frame(t += 16);
    assert.equal(m.phase, 'dissolving');
    assert.equal(m.working(), true, 'its sound on the way');
    m.frame(t + DISSOLVE_MS + 1);
    assert.equal(m.phase, 'off');
    assert.equal(m.working(), false);
});

test('working ends when the preparation fails or is skipped', () => {
    const m = createPrepModel();
    m.arm(0);
    m.answer(prep(0.3), 100);
    m.answer({ state: 'failed' }, 200);
    assert.equal(m.phase, 'fading');
    assert.equal(m.working(), false);
});

// HL52: Hybrid-Phase's own work on the air (the status's arming, hpAhead).
const armg = (frac, step, busy = true, hpAhead = busy, leftS = 4) => ({ seq: 5, frac, step, busy, hpAhead, leftS });
const withHp = { source: 'live', stages: [['αHP', 1, '']], quick: false, hpDeferred: false };
const noHp = { source: 'live', stages: [['ISP', 1, '']], quick: false, hpDeferred: true };

test("Anton's rack change 30M → 1G while αHP plays: the fill shows the pair being made, never full before it is heard (HL52)", () => {
    const m = createPrepModel(), watch = startWatch(m);
    const at = (t, audible, arming) => watch({ state: 'playing', trackId: 3, audible, arming }, t);
    at(0, withHp, null);
    m.answer({ state: 'skipped', track: 3 }, 10);   // the start's asking, long over
    // The 1G chain without HP is built and lands; the old one with HP still heard: nothing yet.
    at(100, withHp, armg(0.1, 'build', true, true, 0.5));
    at(900, withHp, armg(0.3, 'land', true, true, 0.5));
    assert.equal(m.phase, 'off');
    // Heard without HP; the pair's envelope, build and landing.
    at(1300, noHp, armg(0.4, 'hp:env'));
    assert.equal(m.phase, 'filling');
    assert.equal(m.via, 'switch');
    assert.equal(m.litFor(3), true);
    assert.equal(m.wantPoll(1300), false, 'the lists are not asked about');
    let f = run(m, 1300, 200), last = f.fill;
    assert.ok(f.draw && f.hide && f.fill < 0.1, `from the bottom: ${f.fill}`);
    for (let t = 1500, k = 0.4; t < 7500; t += 100, k = Math.min(0.98, k + 0.01)) {
        at(t, noHp, armg(k, k < 0.6 ? 'hp:env' : k < 0.9 ? 'hp:build' : 'hp:land'));
        f = m.frame(t + 50);
        assert.ok(f.fill >= last, `never back at ${t}`);
        assert.ok(pctText(f.fill) !== '100', `not full before heard at ${t}`);
        last = f.fill;
    }
    assert.ok(last > 0.8, `well on: ${last}`);
    // The pair heard: fill to the top, dissolve, the picture back.
    at(7600, withHp, armg(1, 'done', false));
    assert.equal(m.phase, 'finishing');
    run(m, 7600, 2000);
    assert.equal(m.phase, 'off');
    assert.equal(m.via, 'lists');
    assert.equal(m.litFor(3), false);
});

test('a short rack change with no Hybrid-Phase work, or a rack without it, shows no fill (HL52)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'playing', trackId: 3, audible: withHp, arming: null }, 0);
    m.answer({ state: 'skipped', track: 3 }, 10);
    watch({ state: 'playing', trackId: 3, audible: withHp, arming: armg(0.5, 'land', true, false) }, 100);
    watch({ state: 'playing', trackId: 3, audible: withHp, arming: armg(1, 'done', false) }, 400);
    assert.equal(m.phase, 'off');
});

test('the pair held for the next track, a stop or a pause: the fill fades, nothing lit (HL52)', () => {
    for (const end of [{ state: 'playing', arming: armg(0.6, 'held', false) }, { state: 'stopped', arming: null }, { state: 'paused', arming: armg(0.6, 'hp:build') }]) {
        const m = createPrepModel(), watch = startWatch(m);
        watch({ state: 'playing', trackId: 3, audible: withHp, arming: null }, 0);
    m.answer({ state: 'skipped', track: 3 }, 10);
        watch({ state: 'playing', trackId: 3, audible: noHp, arming: armg(0.4, 'hp:env') }, 100);
        assert.equal(m.phase, 'filling');
        watch({ trackId: 3, audible: noHp, ...end }, 500);
        assert.equal(m.phase, 'fading', end.state);
        assert.equal(m.litFor(3), false, end.state);
        run(m, 500, FADE_MS + 50);
        assert.equal(m.phase, 'off');
    }
});

test('a 1G start that makes its lists: one fill from ▶ through the pair heard, no second blank (HL52)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    const prepS = (t, arming) => watch({ state: 'preparing', trackId: 9, audible: null, arming }, t);
    assert.equal(prepS(0, armg(0, 'decode')), true);
    m.answer({ state: 'preparing', progress: 0, track: 9, eta_s: 4 }, 50);
    prepS(100, armg(0.02, 'decode', true, true, 4));
    let last = 0;
    for (let t = 150, p = 0; t < 4000; t += 100, p = Math.min(1, p + 0.03)) {
        m.answer({ state: 'preparing', progress: p, track: 9, eta_s: 4 * (1 - p) }, t);
        const f = m.frame(t + 10);
        assert.ok(f.fill >= last); last = f.fill;
    }
    assert.ok(last < 0.6, `the lists take their share only: ${last}`);
    m.answer({ state: 'ready', progress: 1, track: 9 }, 4000);
    assert.equal(m.phase, 'filling', 'the lists done, the pair next: the same fill');
    assert.equal(m.via, 'switch');
    for (let t = 4100, k = 0.5; t < 9000; t += 100, k = Math.min(0.98, k + 0.01)) {
        watch({ state: 'playing', trackId: 9, audible: noHp, arming: armg(k, k < 0.7 ? 'hp:env' : 'hp:build') }, t);
        assert.ok(m.phase === 'filling', `still filling at ${t}`);
        const f = m.frame(t + 10);
        assert.ok(f.draw && f.fill >= last && pctText(f.fill) !== '100'); last = f.fill;
    }
    watch({ state: 'playing', trackId: 9, audible: withHp, arming: armg(1, 'done', false) }, 9100);
    assert.equal(m.phase, 'finishing');
});

test('a 1G start with its lists on disk: the pair being made shows from the first status after skipped (HL52)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'preparing', trackId: 19, audible: null, arming: armg(0.05, 'build') }, 0);
    m.answer({ state: 'skipped', track: 19 }, 50);
    assert.equal(m.phase, 'off');
    watch({ state: 'playing', trackId: 19, audible: noHp, arming: armg(0.2, 'land') }, 1400);
    assert.equal(m.phase, 'filling', 'no Hybrid-Phase heard on it yet: its work shows at once');
    assert.equal(m.via, 'switch');
});

// HL56: nothing stands in — the unit is full the moment the pair is heard.
const armh = (frac, step, hpHeard, busy = true, leftS = 4) => ({ seq: 7, frac, step, busy, hpAhead: busy && !hpHeard, hpHeard, leftS });

test("Anton's next track at 1G: the quick variant plays the pair — full and its curve at once, not 64 % for ~5 s (HL56)", () => {
    const m = createPrepModel(), watch = startWatch(m);
    const prepS = (t, arming) => watch({ state: 'preparing', trackId: 4, audible: null, arming }, t);
    assert.equal(prepS(0, armh(0, 'decode', false)), true);
    m.answer({ state: 'preparing', progress: 0.5, track: 4, eta_s: 8 }, 50);
    prepS(100, armh(0.02, 'decode', false));
    m.answer({ state: 'ready', progress: 1, track: 4 }, 200);
    assert.equal(m.via, 'switch', 'the lists done, the start chain next: the same fill');
    watch({ state: 'preparing', trackId: 4, audible: null, arming: armh(0.3, 'build', false) }, 300);
    assert.equal(m.phase, 'filling');
    // The quick variant's chain, the pair in it, heard; the full variant still coming.
    const quickHp = { source: 'live', stages: [['αHP', 1, '']], quick: true, hpDeferred: false };
    watch({ state: 'playing', trackId: 4, audible: quickHp, arming: armh(0.5, 'prep:isp', true) }, 1900);
    assert.equal(m.phase, 'finishing');
    run(m, 1900, 2000);
    assert.equal(m.phase, 'off');
});

test('a rack change 30M → 1G with Hybrid-Phase on: the fill shows the wait, full when the new pair is heard (HL56)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    const at = (t, arming) => watch({ state: 'playing', trackId: 3, audible: withHp, arming }, t);
    at(0, null);
    m.answer({ state: 'skipped', track: 3 }, 10);
    at(100, armh(0.1, 'build', false));
    assert.equal(m.phase, 'filling');
    assert.equal(m.via, 'switch');
    let last = 0;
    for (let t = 200, k = 0.1; t < 3000; t += 100, k += 0.02) {
        at(t, armh(k, 'land', false));
        const f = m.frame(t + 10);
        assert.ok(f.fill >= last && pctText(f.fill) !== '100'); last = f.fill;
    }
    at(3100, armh(1, 'done', true, false, 0));
    assert.equal(m.phase, 'finishing');
});

// HL61: ⏭ while a track plays — the pair is waited for at once, not after the lists are asked about.
test("Anton's ⏭ at 1G while a track plays: the unit fills from the first status of the next track (HL61)", () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'playing', trackId: 2, audible: withHp, arming: null }, 0);
    m.answer({ state: 'skipped', track: 2 }, 10);
    run(m, 10, 3000);
    // ⏭: the next track's chain and pair are built, the jump not heard yet.
    const next = (t, arming, audible = withHp) => watch({ state: 'playing', trackId: 3, audible, arming, jump: { pending: true } }, t);
    assert.equal(next(3000, armh(0.05, 'build', false, true, 1.1)), true, 'asks about the lists all the same');
    assert.equal(m.phase, 'filling', 'no answer about the lists waited for');
    assert.equal(m.via, 'switch');
    assert.equal(m.litFor(3), true);
    m.answer({ state: 'skipped', track: 3 }, 3050);
    assert.equal(m.phase, 'filling', 'the lists answer changes nothing');
    let last = 0;
    for (let t = 3100, k = 0.1; t < 4200; t += 100, k += 0.06) {
        next(t, armh(k, k < 0.6 ? 'build' : 'land', false, true, 1.1 - (t - 3000) / 1000));
        const f = m.frame(t + 10);
        assert.ok(f.draw && f.hide && f.fill >= last && pctText(f.fill) !== '100'); last = f.fill;
    }
    assert.ok(last > 0.2, `on its way: ${last}`);
    // The pair heard: full, dissolved, the picture back.
    watch({ state: 'playing', trackId: 3, audible: withHp, arming: armh(1, 'done', true, false, 0) }, 4300);
    assert.equal(m.phase, 'finishing');
    run(m, 4300, 2000);
    assert.equal(m.phase, 'off');
});

test('a start from stop still waits for the lists answer before a fill (HL61)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'preparing', trackId: 5, audible: null, arming: armh(0.02, 'decode', false) }, 0);
    assert.equal(m.phase, 'armed');
    m.answer({ state: 'preparing', progress: 0.1, track: 5, eta_s: 6 }, 50);
    assert.equal(m.via, 'lists');
});

// HL64: the statuses as the window showed them (rw run 4, 1G ×16 αHP): until the jump is heard the
// status names the OLD track, the arming (the next one's) hardly moves; then the new id, the quick
// variant with the pair heard, the rest of the arming not HP's.
const waiting = (frac) => ({ seq: 9, frac, step: 'prep:decode', busy: true, hpAhead: true, hpHeard: false, leftS: 7.5 });
const landed = (frac) => ({ seq: 9, frac, step: 'prep:aa', busy: true, hpAhead: false, hpHeard: true, leftS: 6.7 });
const quickHp64 = { source: 'live', stages: [['HP', 1, ''], ['αHP', 1, '']], quick: true, hpDeferred: false };

test("Anton's ⏭ at 1G as the window saw it: the fill shows the whole wait, full once the new track's pair is heard (HL64)", () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'playing', trackId: 1, audible: withHp, arming: null }, 0);
    m.answer({ state: 'skipped', track: 1 }, 10);
    run(m, 10, 3000);
    // ⏭: the press arms, the lists answer is about the next track (made ahead).
    m.arm(3000);
    m.answer({ state: 'skipped', track: 2 }, 3010);
    const old = t => watch({ state: 'playing', trackId: 1, audible: withHp, arming: waiting(t < 3500 ? 0 : 0.01), jump: { pending: true } }, t);
    old(3060);
    assert.equal(m.phase, 'filling');
    assert.equal(m.via, 'switch');
    let last = 0;
    for (let t = 3110; t < 4300; t += 50) {
        if (t % 250 < 50) old(t);
        const f = m.frame(t);
        assert.ok(f.draw && f.hide && f.fill >= last && pctText(f.fill) !== '100', `at ${t}: ${JSON.stringify(f)}`);
        last = f.fill;
    }
    assert.ok(last > 0.4, `the percent moves with the wait, not with the arming's 1 %: ${last}`);
    assert.ok(last <= 0.9, `never near the top before the pair is heard: ${last}`);
    // The jump heard: the new id, its pair on the air.
    assert.equal(watch({ state: 'playing', trackId: 2, audible: quickHp64, arming: landed(0.01) }, 4330), true);
    m.answer({ state: 'skipped', track: 2 }, 4340);
    assert.notEqual(m.phase, 'fading', 'the new id and its skipped lists do not end the wait unfinished');
    watch({ state: 'playing', trackId: 2, audible: quickHp64, arming: landed(0.11) }, 4580);
    assert.equal(m.phase, 'finishing');
    run(m, 4580, 2500);
    assert.equal(m.phase, 'off');
    // The next wait takes its share by the time this one took (~1.3 s).
    m.arm(9000);
    watch({ state: 'playing', trackId: 2, audible: quickHp64, arming: waiting(0), jump: { pending: true } }, 9050);
    assert.equal(m.via, 'switch');
    run(m, 9050, 700);
    assert.ok(m.shown > 0.25 && m.shown < 0.9, `${m.shown}`);
});

test('a rack change on the air keeps its fill by the arming alone, no time share (HL64)', () => {
    const m = createPrepModel(), watch = startWatch(m);
    watch({ state: 'playing', trackId: 4, audible: withHp, arming: null }, 0);
    m.answer({ state: 'skipped', track: 4 }, 10);
    run(m, 10, 3000);
    watch({ state: 'playing', trackId: 4, audible: noHp, arming: { ...waiting(0.02), leftS: 6 } }, 3000);
    assert.equal(m.via, 'switch');
    run(m, 3000, 3000);
    assert.ok(m.shown < 0.15, `${m.shown}`);
});

const settle = () => new Promise(r => setImmediate(r));
/// A page without a page: frames by hand (pump), timers kept, answers at once.
function fakePage(answer) {
    let T = 0, frames = [];
    const drawn = [];
    const env = {
        now: () => T, raf: cb => frames.push(cb), setTimer: () => 1, clearTimer: () => {},
        fetchJson: async () => answer(), draw: (f) => { drawn.push(f); return !!f.draw; },
    };
    return {
        env, drawn,
        at(t) { T = t; },
        /// Frames every 16 ms up to `t`.
        pump(t) { while (T < t && frames.length) { T += 16; const q = frames; frames = []; for (const cb of q) cb(T); } T = Math.max(T, t); },
        get waiting() { return frames.length; },
    };
}

test('a fill a status starts is drawn, ends, and the next one shows too — the model never sticks (HL64)', async () => {
    const m = createPrepModel();
    const page = fakePage(() => ({ state: 'skipped', track: 1 }));
    const r = createRunner(m, page.env);
    // ▶ from stop at 1G, lists on disk: the start waits in silence for the pair.
    page.at(0); r.arm();
    await settle();
    page.at(64); r.status({ state: 'preparing', trackId: 1, audible: null, arming: waiting(0) });
    await settle();
    page.at(186); r.status({ state: 'preparing', trackId: 1, audible: null, arming: waiting(0.01) });
    assert.equal(m.phase, 'filling');
    assert.ok(page.waiting > 0, 'the status itself asks for frames');
    page.pump(700);
    assert.ok(page.drawn.some(f => f.draw && f.hide), 'the fill is on screen');
    page.at(1574); r.status({ state: 'playing', trackId: 1, audible: quickHp64, arming: landed(0.01) });
    page.pump(5000);
    assert.equal(m.phase, 'off', 'finished and gone, not stuck in finishing');
    assert.equal(m.litFor(1), false);
    // ⏭ later: shown again.
    page.at(20000); r.arm();
    await settle();
    page.at(20060); r.status({ state: 'playing', trackId: 1, audible: withHp, arming: waiting(0), jump: { pending: true } });
    assert.equal(m.phase, 'filling');
    page.pump(20500);
    assert.ok(page.drawn.at(-1).hide);
});
