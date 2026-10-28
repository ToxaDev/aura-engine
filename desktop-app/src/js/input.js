// The input in the player: what another player — foobar2000, JRiver,
// HQPlayer, Roon… — plays into Aura Player ASIO, played through the rack as
// a stream is. Its view stands in the list's place (I in F / R / I,
// listview.js): one card that says where the input is — the driver not
// installed, no player yet, a player connected, playing — and what to do
// next, with the way to choose the device in each player while none is
// there.
//
// The backend's status says it in `input` — driver ('missing',
// 'installed', 'stale', 'unsupported'), client ({ name, running } or null),
// format ({ rate, channels, sampleType } or null), listening, held, auto,
// error — and the words are this file's. player.js hands in each status and
// the hooks to the backend (`call`).

import { mountListView, listView, onListView, dropView } from './listview.js';

const $ = id => document.getElementById(id);

// UI texts.
export const INPUT_TEXTS = {
    title: 'Input',
    device: 'Aura Player ASIO',
    badge: 'INPUT',
    missing: {
        title: "Aura Player ASIO isn't installed",
        text: 'Install it once, and foobar2000, JRiver, HQPlayer, Roon and other ASIO players can play through Aura Player.',
        button: 'Install driver',
    },
    stale: {
        title: 'Aura Player ASIO needs an update',
        text: 'This Aura Player brings a newer driver.',
        button: 'Update driver',
    },
    unsupported: {
        title: 'The input needs Windows',
        text: 'ASIO players play into Aura Player on Windows.',
    },
    waiting: {
        title: 'Waiting for a player',
        text: 'In your player, choose the output Aura Player ASIO, then press play.',
    },
    connected: name => ({ title: name, text: `Connected. Press play in ${name}.` }),
    notListening: name => ({ title: `${name} is playing`, text: "Aura Player isn't listening to it.", button: 'Listen' }),
    playing: 'Playing',
    paused: 'Paused',
    installing: 'Installing…',
    downloading: 'Downloading the driver…',
    remove: 'Remove driver',
    auto: 'Switch to the input when a player starts playing',
    howTo: 'Choose the device in your player',
    hints: [
        ['foobar2000', 'Preferences → Playback → Output → Device: ASIO: Aura Player ASIO (with the ASIO component)'],
        ['JRiver', 'Options → Audio → Audio Device: Aura Player ASIO [ASIO]'],
        ['HQPlayer', 'Settings → Backend: ASIO → Device: Aura Player ASIO'],
        ['Roon', 'Settings → Audio → This PC: Aura Player ASIO → Enable'],
    ],
    // What plays only to Windows' own devices — Spotify, a browser — comes
    // in through a virtual cable and its ASIO bridge (Anton 17.10).
    bridge: {
        title: 'Spotify and other Windows apps',
        text: 'They play to Windows sound devices, not to ASIO. VB-Audio Hi-Fi Cable & ASIO Bridge (donationware) brings their sound here:',
        steps: [
            'Install it: run the setup as administrator, then restart Windows.',
            'In Windows sound settings or in the app, play to Hi-Fi Cable Input.',
            'In ASIO Bridge, choose Aura Player ASIO and press ASIO ON.',
        ],
        after: 'Aura Player picks the sound up here and plays it through the rack.',
        link: 'Download Hi-Fi Cable & ASIO Bridge for Windows (vb-audio.com)',
        url: 'https://vb-audio.com/Cable/index.htm#DownloadASIOBridge',
    },
    err: {
        cancelled: 'Windows asked for permission, and it was not given.',
        failed: "The driver couldn't be installed. Try again, or install Aura Player again.",
        network: "Couldn't download the driver. Check the connection and try again.",
        mismatch: "The driver didn't match this version — nothing was installed.",
        link: "Aura Player couldn't open its input. Restart Aura Player.",
    },
    // Asked after an update (Anton 18.10): to update a driver older than
    // this version's — ASIO players keep the old one until it is updated —
    // once per version; to install it where it never was, once (MISSING_OFFER).
    offer: {
        update: {
            title: 'Update Aura Player ASIO?',
            text: "This Aura Player brings a newer ASIO driver. Players using Aura Player ASIO keep the old one until it's updated. Windows will ask for permission.",
            go: 'Update driver',
            later: 'Later',
            done: 'Updated. Restart the player that uses Aura Player ASIO.',
        },
        install: {
            title: 'Install Aura Player ASIO?',
            text: 'With this driver, foobar2000, JRiver, HQPlayer, Roon and other ASIO players — and Spotify through ASIO Bridge — play through Aura Player. Windows will ask for permission.',
            go: 'Install driver',
            later: 'Not now',
            done: 'Installed. In your player, choose Aura Player ASIO as the output.',
        },
        close: 'Close',
    },
};

/// How often the driver is offered where it was never installed: 'once'
/// (ever), or 'perVersion' (after each update), as the stale one is.
export const MISSING_OFFER = 'once';

/// Where the versions the offers were last opened in are kept.
export const OFFER_KEY = 'auraAsioOffer';
export const OFFER_MISSING_KEY = 'auraAsioOfferMissing';

/// The offer to open now — 'update' (the registered driver is older than
/// this version's, not offered in this version yet), 'install' (none is
/// registered and no player is on the input, not offered yet as
/// `missingOffer` counts) — or null: the version not known yet, a player
/// playing into the input, an install under way, or the driver this
/// version's ('installed') or not for this system ('unsupported').
/// `kept` the versions they were opened in: { stale, missing }.
export function offerKind(input, version, kept = {}, busyNow = null, missingOffer = MISSING_OFFER) {
    const i = input || {};
    if (!version || busyNow || i.listening || i.installing) return null;
    if (i.driver === 'stale') return kept?.stale === version ? null : 'update';
    if (i.driver === 'missing' && !i.client) {
        const m = kept?.missing ?? null;
        const asked = missingOffer === 'perVersion' ? m === version : m !== null;
        return asked ? null : 'install';
    }
    return null;
}

/// The versions the offers were opened in ({ stale, missing }, null where
/// nothing is kept or the storage cannot be read), and marking one opened.
export function keptOffers(storage) {
    const get = k => { try { return storage?.getItem(k) ?? null; } catch (_) { return null; } };
    return { stale: get(OFFER_KEY), missing: get(OFFER_MISSING_KEY) };
}
export function markOffered(storage, kind, version) {
    try { storage?.setItem(kind === 'install' ? OFFER_MISSING_KEY : OFFER_KEY, version); } catch (_) {}
}

/// What offer `kind` says in each phase: 'ask', 'busy' (the install's
/// `installing` phase in hand), then the install's answer — 'ok' or an
/// error's word. Buttons as [act, label].
export function offerView(kind, phase, installing = null) {
    const O = INPUT_TEXTS.offer[kind] || INPUT_TEXTS.offer.update;
    if (phase === 'ask') return { title: O.title, text: O.text, buttons: [['later', O.later], ['install', O.go]] };
    if (phase === 'busy') {
        const text = installing === 'download' ? INPUT_TEXTS.downloading : INPUT_TEXTS.installing;
        return { title: INPUT_TEXTS.device, text, buttons: [] };
    }
    const text = phase === 'ok' ? O.done : INPUT_TEXTS.err[phase] || INPUT_TEXTS.err.failed;
    return { title: INPUT_TEXTS.device, text, buttons: [['close', INPUT_TEXTS.offer.close]] };
}

const fmtRate = hz => { const k = hz / 1000; return (Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1)) + ' kHz'; };

/// The player's sample format (an ASIO sample type) in words.
export function sampleTypeText(t) {
    switch (t) {
        case 16: return '16-bit';
        case 17: return '24-bit';
        case 18: return '32-bit';
        case 19: return '32-bit float';
        case 20: return '64-bit float';
        default: return '';
    }
}

/// "96 kHz · 32-bit · 2 ch": what comes in, or '' before it does.
export function formatLine(f) {
    if (!f || !(f.rate > 0)) return '';
    return [fmtRate(f.rate), sampleTypeText(f.sampleType), f.channels ? `${f.channels} ch` : ''].filter(Boolean).join(' · ');
}

/// The card the status asks for: its kind (the lamp's colour), title, text,
/// the button and what it does, and whether the players' hints show. A
/// player on the input is said first, whatever the registry says of the
/// driver: it is there, or the player could not be on it (a driver loaded
/// by its path, as the checking tool does, is not registered).
export function inputCard(input, busy = null) {
    const i = input || {};
    const name = i.client?.name || 'A player';
    if (busy === 'install') {
        const title = i.installing === 'download' ? INPUT_TEXTS.downloading : INPUT_TEXTS.installing;
        return { kind: 'busy', title, text: '', button: null, act: null, hints: false };
    }
    if (i.driver === 'unsupported') return { kind: 'off', ...INPUT_TEXTS.unsupported, button: null, act: null, hints: false };
    if (i.listening) {
        const state = i.held ? INPUT_TEXTS.paused : INPUT_TEXTS.playing;
        return { kind: i.held ? 'wait' : 'on', title: name, text: [formatLine(i.format), state].filter(Boolean).join(' · '), button: null, act: null, hints: false };
    }
    if (i.client?.running) return { kind: 'wait', ...INPUT_TEXTS.notListening(name), act: 'listen', hints: false };
    if (i.client) return { kind: 'wait', ...INPUT_TEXTS.connected(name), button: null, act: null, hints: false };
    if (i.driver === 'missing' || !i.driver) {
        return { kind: 'off', ...INPUT_TEXTS.missing, act: 'install', hints: false };
    }
    if (i.driver === 'stale') return { kind: 'off', ...INPUT_TEXTS.stale, act: 'install', hints: false };
    return { kind: 'wait', ...INPUT_TEXTS.waiting, button: null, act: null, hints: true };
}

/// What the strip and the big player say while the input plays: the
/// player's name, and what comes in.
export function inputLines(s) {
    const i = s?.input;
    if (!i?.listening) return null;
    return { title: i.client?.name || INPUT_TEXTS.device, sub: formatLine(i.format), badge: INPUT_TEXTS.badge };
}

/// A word on the input's state for the player's status line, or null.
export function inputStateText(s) {
    const i = s?.input;
    if (!i?.listening) return null;
    if (i.held) return INPUT_TEXTS.paused;
    if (!i.client?.running) return INPUT_TEXTS.connected(i.client?.name || INPUT_TEXTS.device).text;
    return null;
}

/// The player's status line while the input is listened to — { text,
/// kind: 'error' | 'note' } — or null: its failure (the backend's words,
/// "Input: …"), else the word on its state.
export function inputStatus(s) {
    if (!s?.input?.listening) return null;
    if (s.radioError || s.radio?.phase === 'failed') {
        const text = s.error || (s.radio?.info?.error ? `Input: ${s.radio.info.error}` : '');
        if (text) return { text, kind: 'error' };
    }
    const t = inputStateText(s);
    return t ? { text: t, kind: 'note' } : null;
}

/// What comes in and what the rack plays it at — "96 kHz · 32-bit → 768
/// kHz", or "96 kHz · 32-bit · bit-perfect" — while it plays; what comes in
/// alone before.
export function inputTech(s) {
    const f = s?.input?.format;
    if (!f || !(f.rate > 0)) return '';
    const src = [fmtRate(f.rate), sampleTypeText(f.sampleType)].filter(Boolean).join(' · ');
    if (!(s.outRate > 0 && s.state && s.state !== 'stopped' && s.radio)) return src;
    if (s.streamDirect === true) return `${src} · bit-perfect`;
    return `${src} → ${fmtRate(s.outRate)}`;
}

let hooks = null;   // { call(cmd, args) → Promise }
let view = null;    // #inView, once built
let last = null;    // the status last rendered
let busy = null;    // 'install' while Windows asks for permission
let note = null;    // the view's own word, for a while

const call = (cmd, args) => (hooks?.call ? hooks.call(cmd, args) : Promise.reject(new Error('no backend')));
const shown = () => !!view && listView() === 'input';

let version = null;  // this app's, once known
let offer = null;    // the driver's offer: its phase while open
let offerOf = null;  // and its kind (offerKind)
let offered = false; // it was opened in this launch

export function initInput(h) {
    hooks = h;
    mountListView();
    mountView();
    // Aura Player ASIO is a Windows driver: elsewhere the list has no Input.
    call('player_input_supported', {}).then(ok => { if (ok === false) dropView('input'); }).catch(() => {});
    try {
        window.__TAURI__?.app?.getVersion?.().then(v => { version = v; maybeOffer(); }).catch(() => {});
    } catch (_) {}
}

/// Installs (or updates) the driver; the backend's answer.
async function install() {
    busy = 'install';
    note = null;
    render();
    let r;
    try {
        r = await call('player_input_install', {});
        note = r === 'ok' ? null : INPUT_TEXTS.err[r] || INPUT_TEXTS.err.failed;
    } catch (_) {
        r = 'failed';
        note = INPUT_TEXTS.err.failed;
    }
    busy = null;
    render();
    return r;
}

async function act(what) {
    if (what === 'install') {
        await install();
    } else if (what === 'listen') {
        call('player_input_listen', {}).catch(() => {});
    } else if (what === 'remove') {
        try { await call('player_input_remove', {}); } catch (_) {}
    }
}

function mountView() {
    const zone = $('dropZone');
    if (!zone || $('inView')) return;
    const v = document.createElement('div');
    v.className = 'in-view';
    v.id = 'inView';
    v.innerHTML = `
      <div class="in-card" id="inCard">
        <div class="in-head"><span class="in-lamp" id="inLamp"></span><span class="in-title" id="inTitle"></span></div>
        <div class="in-text" id="inText"></div>
        <div class="in-note" id="inNote" hidden></div>
        <button type="button" class="rd-btn in-btn" id="inBtn" hidden></button>
      </div>
      <div class="in-hints" id="inHints" hidden><div class="in-hints-h"></div></div>
      <div class="in-foot" id="inFoot" hidden>
        <label class="in-auto"><input type="checkbox" id="inAuto"> <span></span></label>
        <button type="button" class="in-link" id="inRemove"></button>
      </div>`;
    v.querySelector('.in-hints-h').textContent = INPUT_TEXTS.howTo;
    const hints = v.querySelector('#inHints');
    for (const [who, how] of INPUT_TEXTS.hints) {
        const row = document.createElement('div');
        row.className = 'in-hint';
        row.innerHTML = '<b></b><span></span>';
        row.querySelector('b').textContent = who;
        row.querySelector('span').textContent = how;
        hints.appendChild(row);
    }
    const B = INPUT_TEXTS.bridge;
    const bridge = document.createElement('div');
    bridge.className = 'in-bridge';
    bridge.innerHTML = '<div class="in-hints-h"></div><div class="in-bridge-t"></div><ol></ol>'
        + '<div class="in-bridge-t"></div><button type="button" class="in-ext"></button>';
    bridge.querySelector('.in-hints-h').textContent = B.title;
    const [said, after] = bridge.querySelectorAll('.in-bridge-t');
    said.textContent = B.text;
    after.textContent = B.after;
    for (const s of B.steps) {
        const li = document.createElement('li');
        li.textContent = s;
        bridge.querySelector('ol').appendChild(li);
    }
    const get = bridge.querySelector('.in-ext');
    get.textContent = B.link;
    get.title = B.url;
    get.addEventListener('click', e => {
        if (e.detail > 0) e.currentTarget.blur();   // Space stays the player's
        try { window.__TAURI__.shell.open(B.url); } catch (_) {}
    });
    hints.appendChild(bridge);
    v.querySelector('.in-auto span').textContent = INPUT_TEXTS.auto;
    v.querySelector('#inRemove').textContent = INPUT_TEXTS.remove;
    v.querySelector('#inBtn').addEventListener('click', e => {
        if (e.detail > 0) e.currentTarget.blur();   // Space stays the player's
        act(e.currentTarget.dataset.act);
    });
    v.querySelector('#inRemove').addEventListener('click', () => act('remove'));
    v.querySelector('#inAuto').addEventListener('change', e => {
        call('player_input_auto', { on: !!e.target.checked }).catch(() => {});
    });
    zone.appendChild(v);
    view = v;
    onListView(on => { if (on === 'input') render(); });
    render();
}

function render() {
    if (!view) return;
    const i = last?.input || null;
    const c = inputCard(i, busy);
    view.querySelector('#inLamp').dataset.kind = c.kind;
    view.querySelector('#inTitle').textContent = c.title;
    view.querySelector('#inText').textContent = c.text;
    const n = view.querySelector('#inNote');
    n.hidden = !note;
    n.textContent = note || '';
    const b = view.querySelector('#inBtn');
    b.hidden = !c.button;
    b.textContent = c.button || '';
    b.dataset.act = c.act || '';
    view.querySelector('#inHints').hidden = !c.hints;
    const installed = i && (i.driver === 'installed' || i.driver === 'stale');
    view.querySelector('#inFoot').hidden = !installed;
    view.querySelector('#inAuto').checked = i?.auto !== false;
}

/// Each status (radioRender lights the switch's letters for all three).
export function inputRender(s) {
    last = s;
    if (shown()) render();
    if (offer === 'busy') renderOffer();
    else maybeOffer();
}

// ── the driver, offered after an update ───────────────────────────────────

function storage() {
    try { return window.localStorage; } catch (_) { return null; }
}

/// Opens the offer when the status asks for it — after the update's own
/// dialog, if that is open. Marked as offered as it opens: Later, Esc or a
/// closed app all leave it asked (offerKind says for how long).
function maybeOffer() {
    if (offered || offer) return;
    const kind = offerKind(last?.input, version, keptOffers(storage()), busy);
    if (!kind) return;
    const up = $('upModal');
    if (up && !up.classList.contains('up-hidden')) return;
    offered = true;
    markOffered(storage(), kind, version);
    offerOf = kind;
    offer = 'ask';
    renderOffer();
    $('inOffer')?.querySelector('[data-act="install"]')?.focus();
}

function closeOffer() {
    if (offer === 'busy') return;
    offer = null;
    $('inOffer')?.remove();
}

async function offerAct(what) {
    if (what === 'install') {
        offer = 'busy';
        renderOffer();
        offer = await install();
        renderOffer();
        $('inOffer')?.querySelector('[data-act="close"]')?.focus();
    } else {
        closeOffer();
    }
}

function renderOffer() {
    let o = $('inOffer');
    if (!offer) { o?.remove(); return; }
    if (!o) {
        o = document.createElement('div');
        o.id = 'inOffer';
        o.innerHTML = '<div class="in-offer" role="dialog" aria-modal="true" aria-labelledby="inOfferTitle">'
            + '<p class="in-offer-title" id="inOfferTitle"></p><p class="in-offer-text"></p>'
            + '<div class="in-offer-btns"></div></div>';
        o.addEventListener('keydown', e => {
            if (e.key === 'Escape') { e.stopPropagation(); closeOffer(); }
        });
        o.addEventListener('click', e => { if (e.target === o) closeOffer(); });
        document.body.appendChild(o);
    }
    const v = offerView(offerOf, offer, last?.input?.installing);
    o.querySelector('.in-offer-title').textContent = v.title;
    o.querySelector('.in-offer-text').textContent = v.text;
    const row = o.querySelector('.in-offer-btns');
    const now = v.buttons.map(b => b.join(':')).join('|');
    if (row.dataset.now === now) return;
    row.dataset.now = now;
    row.replaceChildren(...v.buttons.map(([what, label]) => {
        const b = document.createElement('button');
        b.type = 'button';
        b.dataset.act = what;
        b.textContent = label;
        b.addEventListener('click', e => {
            if (e.detail > 0) e.currentTarget.blur();   // Space stays the player's
            offerAct(what);
        });
        return b;
    }));
}
