// Blind test, for the developer only: AURA_BLIND_TEST=<trials> in the
// environment opens the app in it. One track two ways — its converted file
// from disk, and live through the rack from a copy of its source (the same
// audio under another name, so it has no converted file) — picked at random
// for each trial behind a curtain over the whole window. The listener says
// which one he heard; each guess with the truth, and the score, go to the
// session log ("[UI] blind: …") and show when the trials are done.
//
// Nothing but the sound may tell the two apart:
// - the curtain hides the list, the badges, the chain and the analyzer, and
//   takes every key (Space would pause the player under it);
// - each trial plays muted from the press until a fixed 5 s later, and on
//   until the live chain is its full variant (the quick one plays quieter,
//   then glides up); only then the level comes up, over 0.4 s. The disk
//   file's longer start (it is decoded whole) and the live build both fall
//   inside the muted part;
// - a random 0.2–1.2 s before each play spreads where in the music the sound
//   comes in, for both;
// - the live chain is built before the first trial (a muted warm-up).

import { state } from './state.js';
import { entrySig } from './library.js';

const invoke = (cmd, args) => window.__TAURI__.tauri.invoke(cmd, args);
const sleep = ms => new Promise(k => setTimeout(k, ms));
const MUTE_DB = -120;
const UNMUTE_AT_MS = 5000;
const RAMP_MS = 400;
const START_LIMIT_MS = 60000;

let trials = 0;
let pair = null;
let userDb = 0;
let seq = [];
let guesses = [];
let curtain = null;

function rnd() {
    const a = new Uint32Array(1);
    crypto.getRandomValues(a);
    return a[0] / 2 ** 32;
}

function log(message) {
    invoke('player_ui_log', { level: 'blind', message }).catch(() => {});
}

async function status() {
    try {
        const r = await fetch('https://aura.localhost/player/status');
        if (r.ok) return await r.json();
    } catch (_) { /* the protocol is down: ask through invoke */ }
    return invoke('player_status');
}

const hasFile = e => (e.convs || []).some(c => c.sig === entrySig(e));

/// A converted row and a row of a copy of its source: the copy's name starts
/// with the converted row's name without its extension ("X.mp3" and
/// "X — копия.mp3") and it has no converted file of its own.
function findPairs() {
    const out = [];
    for (const d of state.list) {
        if (d.trackId == null || !hasFile(d)) continue;
        const stem = d.name.replace(/\.[^.]+$/, '');
        const live = state.list.find(e => e !== d && e.trackId != null && !hasFile(e)
            && e.name !== d.name && e.name.startsWith(stem));
        if (live) out.push({ disk: d, live, title: stem });
    }
    return out;
}

const setVol = db => invoke('player_set_volume', { db }).catch(() => {});

async function rampUp() {
    const steps = 16;
    const top = 10 ** (userDb / 20);
    for (let i = 1; i <= steps; i++) {
        await setVol(Math.max(MUTE_DB, 20 * Math.log10(top * i / steps)));
        await sleep(RAMP_MS / steps);
    }
}

/// Play one of the pair from its start, muted until it is its full self and
/// 5 s have passed since the press; `listen` brings the level up then.
async function play(which, listen) {
    const e = which === 'disk' ? pair.disk : pair.live;
    const want = which === 'disk' ? 'file' : 'live';
    const t0 = performance.now();
    if (!curtain) return;
    await setVol(MUTE_DB);
    await invoke('player_stop').catch(() => {});
    await sleep(200 + rnd() * 1000);
    if (!curtain) return;
    await invoke('player_play', { id: e.trackId });
    let wrong = 0;   // polls in a row that hear this track from the wrong source
    for (;;) {
        await sleep(100);
        if (!curtain) return;
        const s = await status();
        if (s?.error) throw new Error(s.error);
        const a = s?.audible;
        const mine = s?.state === 'playing' && s.trackId === e.trackId && a && !s.jump?.pending;
        wrong = mine && a.source !== want ? wrong + 1 : 0;
        if (wrong >= 15) {
            throw new Error(a.source === 'direct' ? 'BIT-PERFECT is on: turn it off and start again.'
                : which === 'disk' ? 'The converted row plays live: its file was made with another rack.'
                : 'The copy plays from disk: it has a converted file of its own.');
        }
        const ready = mine && a.source === want && !a.quick && !s.quickVariant;
        if (ready && performance.now() - t0 >= UNMUTE_AT_MS) break;
        if (performance.now() - t0 > START_LIMIT_MS) throw new Error('The track did not start within a minute.');
    }
    if (listen && curtain) await rampUp();
}

// ── the curtain ───────────────────────────────────────────────────────

const CSS = `
#blindCurtain { position: fixed; inset: 0; z-index: 2147483000; background: #0b0f14; color: #e2e8f0;
  display: flex; align-items: center; justify-content: center; font: 15px/1.5 system-ui, sans-serif; }
#blindCurtain .bt-box { width: min(560px, 90vw); text-align: center; }
#blindCurtain h1 { font-size: 22px; font-weight: 600; margin: 0 0 6px; color: #fff; }
#blindCurtain .bt-sub { color: #94a3b8; margin: 0 0 22px; white-space: pre-line; }
#blindCurtain .bt-state { min-height: 24px; margin: 0 0 22px; color: #d4a94f; }
#blindCurtain .bt-row { display: flex; gap: 12px; justify-content: center; flex-wrap: wrap; margin: 0 0 14px; }
#blindCurtain button { font: inherit; padding: 12px 22px; border-radius: 8px; border: 1px solid #334155;
  background: #1e293b; color: #f1f5f9; cursor: pointer; min-width: 130px; }
#blindCurtain button.bt-big { font-size: 17px; padding: 16px 28px; min-width: 170px; }
#blindCurtain button:hover:not(:disabled) { background: #273449; }
#blindCurtain button:disabled { opacity: .4; cursor: default; }
#blindCurtain .bt-end { background: transparent; border-color: #1e293b; color: #64748b; min-width: 0; }
#blindCurtain table { margin: 0 auto 18px; border-collapse: collapse; }
#blindCurtain td, #blindCurtain th { padding: 3px 14px; border-bottom: 1px solid #1e293b; }
#blindCurtain .bt-ok { color: #4ade80; } #blindCurtain .bt-no { color: #f87171; }
`;

function blockKeys(ev) {
    if (!curtain) return;
    ev.stopPropagation();
    ev.preventDefault();
}

function show(html) {
    curtain.querySelector('.bt-box').innerHTML = html;
}

function el(sel) {
    return curtain.querySelector(sel);
}

function fail(e) {
    log('stopped: ' + (e?.message || e));
    if (!curtain) return;
    show(`<h1>The test stopped</h1><p class="bt-sub">${esc(e?.message || e)}</p>
      <div class="bt-row"><button class="bt-big" data-a="close">Close</button></div>`);
    setVol(MUTE_DB);
    invoke('player_stop').catch(() => {});
}

const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

async function close() {
    await invoke('player_stop').catch(() => {});
    await setVol(userDb);
    window.removeEventListener('keydown', blockKeys, true);
    window.removeEventListener('keyup', blockKeys, true);
    curtain?.remove();
    curtain = null;
    log('curtain closed');
}

function intro(pairs) {
    if (!pairs.length) {
        show(`<h1>Blind test</h1><p class="bt-sub">No pair in the list yet: a converted track (converted with the rack as it
is now) and a copy of its source whose name starts with the same name, e.g. "X.mp3" and "X - Copy.mp3".</p>
          <div class="bt-row"><button data-a="close">Close</button></div>`);
        return;
    }
    const buttons = pairs.map((p, i) => `<button class="bt-big" data-a="start" data-i="${i}">${esc(p.title)}</button>`).join('');
    show(`<h1>Blind test: disk or live</h1>
      <p class="bt-sub">${trials} trials. Each plays the track from its start, either its converted file from disk
or the copy live through the rack, at random. Say which one you hear.
Do not open the log window until the end.</p>
      <div class="bt-row">${buttons}</div>
      <div class="bt-row"><button class="bt-end" data-a="close">Close</button></div>`);
    curtain._pairs = pairs;
}

async function start(i) {
    pair = curtain._pairs[i];
    seq = Array.from({ length: trials }, () => (rnd() < 0.5 ? 'disk' : 'live'));
    guesses = [];
    log(`start: ${trials} trials, disk = "${pair.disk.name}", live = "${pair.live.name}"`);
    show(`<h1>Preparing…</h1><p class="bt-sub">The live chain is built once, muted, so that no trial starts with it.</p>`);
    try {
        await play('live', false);
        await play('disk', false);
        await invoke('player_stop');
    } catch (e) {
        return fail(e);
    }
    trial();
}

async function trial() {
    const n = guesses.length;
    show(`<h1>Trial ${n + 1} of ${trials}</h1>
      <p class="bt-state">Starting…</p>
      <div class="bt-row"><button class="bt-big" data-a="guess" data-g="disk" disabled>Disk</button>
        <button class="bt-big" data-a="guess" data-g="live" disabled>Live</button></div>
      <div class="bt-row"><button data-a="replay" disabled>Play again</button></div>
      <div class="bt-row"><button class="bt-end" data-a="close">End the test</button></div>`);
    try {
        await play(seq[n], true);
    } catch (e) {
        return fail(e);
    }
    if (!curtain || guesses.length !== n) return;
    el('.bt-state').textContent = 'Playing — which one is it?';
    curtain.querySelectorAll('button[disabled]').forEach(b => { b.disabled = false; });
}

async function replay() {
    const n = guesses.length;
    curtain.querySelectorAll('button[data-a="guess"], button[data-a="replay"]').forEach(b => { b.disabled = true; });
    el('.bt-state').textContent = 'Starting…';
    try {
        await play(seq[n], true);
    } catch (e) {
        return fail(e);
    }
    if (!curtain || guesses.length !== n) return;
    el('.bt-state').textContent = 'Playing — which one is it?';
    curtain.querySelectorAll('button[disabled]').forEach(b => { b.disabled = false; });
}

function guess(g) {
    const n = guesses.length;
    guesses.push(g);
    log(`trial ${n + 1}: guess ${g}, played ${seq[n]} ${g === seq[n] ? 'right' : 'wrong'}`);
    if (guesses.length < trials) return trial();
    done();
}

/// P(at least k right of n by guessing).
function pChance(k, n) {
    let c = 1, sum = 0;
    for (let j = 0; j <= n; j++) {
        if (j >= k) sum += c;
        c = c * (n - j) / (j + 1);
    }
    return sum / 2 ** n;
}

async function done() {
    await setVol(MUTE_DB);
    await invoke('player_stop').catch(() => {});
    const right = guesses.filter((g, i) => g === seq[i]).length;
    const p = pChance(right, trials);
    log(`score ${right}/${trials}, chance of that or better by guessing ${(p * 100).toFixed(1)} %`);
    const rows = seq.map((s, i) => `<tr><td>${i + 1}</td><td>${s}</td><td>${guesses[i]}</td>
      <td class="${guesses[i] === s ? 'bt-ok">✓' : 'bt-no">✗'}</td></tr>`).join('');
    show(`<h1>${right} of ${trials} right</h1>
      <p class="bt-sub">By guessing alone, ${right} or more right happens in ${(p * 100).toFixed(1)} % of tests.
${p <= 0.05 ? 'That is not luck: you hear a difference.' : 'That is within luck: no difference heard.'}</p>
      <table><tr><th>#</th><th>played</th><th>you said</th><th></th></tr>${rows}</table>
      <div class="bt-row"><button class="bt-big" data-a="close">Close</button></div>`);
}

async function onClick(ev) {
    const b = ev.target.closest('button');
    if (!b || b.disabled) return;
    switch (b.dataset.a) {
        case 'start': return start(Number(b.dataset.i));
        case 'guess': return guess(b.dataset.g);
        case 'replay': return replay();
        case 'close': return close();
    }
}

export async function initBlindTest(n) {
    trials = n;
    const s = await status().catch(() => null);
    userDb = Number.isFinite(s?.volumeDb) ? s.volumeDb : 0;
    const style = document.createElement('style');
    style.textContent = CSS;
    document.head.appendChild(style);
    curtain = document.createElement('div');
    curtain.id = 'blindCurtain';
    curtain.innerHTML = '<div class="bt-box"><h1>Blind test</h1><p class="bt-sub">Waiting for the list…</p></div>';
    curtain.addEventListener('click', onClick);
    curtain.addEventListener('contextmenu', e => { e.preventDefault(); e.stopPropagation(); });
    document.body.appendChild(curtain);
    window.addEventListener('keydown', blockKeys, true);
    window.addEventListener('keyup', blockKeys, true);
    log(`curtain up (${n} trials), the listener's level ${userDb} dB`);
    // The list is restored and given the player's ids a moment after start.
    const t0 = performance.now();
    let pairs = findPairs();
    while (!pairs.length && performance.now() - t0 < 15000) {
        await sleep(500);
        pairs = findPairs();
    }
    if (curtain) intro(pairs);
}
