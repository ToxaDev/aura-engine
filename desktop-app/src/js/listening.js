
// ══════════════════════════════════════════════════════════════════════
// Listening mode: the same window with the rack folded away.
//
// One DOM, two layouts. body.mode-listening hides the rack — hidden, not
// switched off: every control, every listener and the player's chain stay
// as they are, so the sound does not change — lifts the player under the
// heading and grows it (cover, title, tags, the stages it plays, the
// spectrum), and gives the list the rest of the window in whole rows. The
// window keeps its height: main.js fitWindowToContent always measures the
// studio.
//
// This file owns the strip that switches the mode, the L key, the
// remembered mode, the layout numbers, the big player's text lines and the
// animation between the two layouts. The player's data comes from its own
// render pass (playerViewHooks.afterSub); nothing is polled twice.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';
import { radioLines } from './radio.js';
import { playerViewHooks, playerState, playerStatus, spectrumDelay } from './player.js';
import { initWaves } from './waves.js';
import { initFullView, SVG_FULL } from './fullview.js';
import { revolverResync } from './dropzone.js';
import { hide as hideTip } from './tooltip.js';

const CLS = 'mode-listening';
const KEY_MODE = 'auraListeningMode';
const $ = id => document.getElementById(id);

const load = () => { try { return localStorage.getItem(KEY_MODE) === '1'; } catch (_) { return false; } };
const store = on => { try { localStorage.setItem(KEY_MODE, on ? '1' : '0'); } catch (_) {} };

export const isListening = () => document.body.classList.contains(CLS);

// UI texts, all in one place.
const TEXTS = {
    studioTok: 'L', studioName: 'Listening mode',
    studioWhy: 'The settings fold away; the player and the list take the window. What you hear does not change.',
    listenTok: 'L', listenName: 'Studio',
    listenWhy: 'The settings come back. What you hear does not change.',
    hint: 'Click or press L.',
    aria: 'Listening mode',
    nothing: '▶ on a row plays it',
    barsAria: 'Spectrum bars',
    barsOn: 'Spectrum bars: shown. Click to hide them — the picture then fills the whole player.',
    barsOff: 'Spectrum bars: hidden, the picture fills the whole player. Click to bring the bars back.',
};

// One thin chevron, pointing up; the listening layout turns it down.
const SVG_CHEVRONS = '<svg viewBox="0 0 12 7" fill="none" stroke="currentColor" stroke-width="1.5" '
    + 'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M2 5.5 6 1.5 10 5.5"/></svg>';

// The waves' look switch: two waves, one behind the other.
const SVG_WAVES = '<svg viewBox="0 0 14 10" fill="none" stroke="currentColor" stroke-width="1.2" '
    + 'stroke-linecap="round" aria-hidden="true">'
    + '<path d="M1 4c1.5-2.4 3-2.4 4.5 0s3 2.4 4.5 0 2-2 3-1.2"/>'
    + '<path d="M1 7.5c1.5-2.4 3-2.4 4.5 0s3 2.4 4.5 0 2-2 3-1.2" opacity=".5"/></svg>';

// The bars' switch: four little bars of a spectrum; with the bars hidden a
// stroke crosses them out (listening.css).
const SVG_BARS = '<svg viewBox="0 0 12 10" aria-hidden="true">'
    + '<g fill="currentColor"><rect x="0.5" y="5" width="2" height="5" rx="0.5"/>'
    + '<rect x="3.5" y="1" width="2" height="9" rx="0.5"/><rect x="6.5" y="3" width="2" height="7" rx="0.5"/>'
    + '<rect x="9.5" y="6.5" width="2" height="3.5" rx="0.5"/></g>'
    + '<path class="pl-bars-cross" d="M1 9.5 11 0.5" fill="none" stroke="currentColor" stroke-width="1.3" '
    + 'stroke-linecap="round"/></svg>';

// The spectrum bars under the big player's picture (Anton 29.09): shown, or
// hidden with the picture — the scene or the cover — filling the whole
// player. Kept across starts.
const KEY_BARS = 'auraListeningBars';
const loadBars = () => { try { return localStorage.getItem(KEY_BARS) !== '0'; } catch (_) { return true; } };

// How close to the player's top edge the pointer shows the flap (its 12 px
// hit zone and a few pixels more).
const NEAR_TOP_PX = 18;

// ── layout numbers ────────────────────────────────────────────────────
//
// The player takes ~26 % of the window (270–288 px), the list the rest in
// whole rows (at least 4); what is left over from rounding to whole rows
// goes to the player, so no empty band sits under the last row.

const px = v => parseFloat(v) || 0;

function computeLayout() {
    const panel = $('converterPanel');
    if (!panel || !isListening()) return;
    const specs = $('convSpecsLine');
    const head = $('plListHead');
    const zone = $('dropZone');
    const list = $('fileQueue');
    if (!specs || !head || !zone || !list) return;

    const ps = getComputedStyle(panel);
    const pr = panel.getBoundingClientRect();
    const bottom = pr.bottom - px(ps.borderBottomWidth) - px(ps.paddingBottom);
    const ss = getComputedStyle(specs);
    const top = specs.getBoundingClientRect().bottom + px(ss.marginBottom);
    const avail = bottom - top;

    const hs = getComputedStyle(head), zs = getComputedStyle(zone);
    const root = getComputedStyle(document.documentElement);
    const rowH = px(root.getPropertyValue('--pl-row-h')) || 43;
    const gap = px(root.getPropertyValue('--pl-row-gap'));
    const pad = px(root.getPropertyValue('--pl-list-pad'));
    const fixed = px(hs.marginTop) + head.getBoundingClientRect().height
        + px(zs.marginTop) + px(zs.borderTopWidth) + px(zs.borderBottomWidth);
    const listH = n => n * rowH + (n - 1) * gap + 2 * pad;

    // 270 px is the lowest the grid holds (174 px of rows + the 96 px cover).
    let player = Math.min(288, Math.max(270, Math.round(0.26 * window.innerHeight)));
    let rows = Math.floor((avail - player - fixed - 2 * pad + gap) / (rowH + gap));
    if (rows < 4) rows = 4;
    player = Math.floor(avail - fixed - listH(rows));   // the rounding slack goes to the player
    // The device row is one line (22 px) unless the analyser's readout wrapped
    // it to a second one; the cover gives that height back.
    const dev = document.querySelector('#plBar .pl-dev');
    const devExtra = dev ? Math.max(0, Math.round(dev.getBoundingClientRect().height) - 22) : 0;
    const cover = Math.max(96, Math.min(216, player - 174 - devExtra));

    panel.style.setProperty('--lm-player-h', player + 'px');
    panel.style.setProperty('--lm-cover', cover + 'px');
    panel.style.setProperty('--lm-rows', String(rows));
}

// ── the big player's lines (from the player's own render pass) ────────

const fmtRate = hz => { const k = hz / 1000; return (Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1)) + ' kHz'; };
const setText = (el, t) => { if (el && el.textContent !== t) el.textContent = t; };

function afterSub(s) {
    // A stream: the station on the tags' first line, the album line its own
    // list gives on the second, its technical line under the title.
    if (s?.radio) {
        const L = radioLines(s);
        setText($('plTagsArtist'), L.station);
        setText($('plTagsAlbum'), L.album);
        $('plBar')?.classList.toggle('lm-no-track', false);
        if (!isListening()) return;
        const on = (s.state || 'stopped') !== 'stopped';
        setSub($('plSub'), on ? L.tech : '', on ? (s.device || '') : '');
        return;
    }
    const e = s?.trackId != null ? state.list.find(x => x.trackId === s.trackId) : null;
    const info = e?.info;
    // Tags: always written (hidden in the studio), so the big player has
    // them the moment the mode switches.
    setText($('plTagsArtist'), info?.artist || '');
    setText($('plTagsAlbum'), [info?.album, info?.year].filter(Boolean).join(' · '));
    // Nothing in the player (the list was cleared, or nothing was ever
    // played): the big player shows no cover, not the last one it had.
    $('plBar')?.classList.toggle('lm-no-track', !e);
    if (!isListening()) return;
    // The sub line of the big player is the technical line, always — the
    // tags have their own line here (the studio's showcase swaps them in).
    const st = s?.state || 'stopped';
    const bp = !!$('plBitPerfect')?.classList.contains('on');
    let sub, dev = '';
    if (e && s.outRate > 0 && st !== 'stopped') {
        const src = `${fmtRate(s.srcRate)}${s.bits ? '/' + s.bits : ''}`;
        sub = bp
            ? `bit-perfect · ${src} · ${s.exclusive ? 'exclusive' : 'shared'} ${s.format || ''}`
            : `${src} → ${fmtRate(s.outRate)} · ${s.exclusive ? 'exclusive' : 'shared'} ${s.format || ''}`;
        dev = s.device || '';
    } else if (e) {
        sub = info?.sampleRate ? fmtRate(info.sampleRate) + (info.bits ? '/' + info.bits : '') : '';
    } else {
        sub = TEXTS.nothing;
    }
    setSub($('plSub'), sub.trim(), dev);
}

/// The text over the picture keeps the height the cover square had; on a
/// low screen a long title, the tags and a long device name are taller than
/// that, and the cell cut its last line through the middle. Whole lines give
/// way instead, one at a time, each ending in "…": the device's second line,
/// the title's second, the device, then the album line (listening.css).
/// Measured only when the text or the cell's height changes.
const FIT_STEPS = ['sub2', 'title1', 'sub1', 'noalbum'];
let fitKey = '';
function fitText() {
    const now = document.querySelector('#plBar .pl-now');
    if (!now) return;
    const cover = $('converterPanel')?.style.getPropertyValue('--lm-cover') || '';
    const key = `${isListening()}\u0000${cover}\u0000${now.textContent}`;
    if (key === fitKey) return;
    fitKey = key;
    delete now.dataset.fit;
    if (!isListening()) return;
    const fit = [];
    for (const step of FIT_STEPS) {
        if (now.scrollHeight <= now.clientHeight) break;
        fit.push(step);
        now.dataset.fit = fit.join(' ');
    }
}

/// The technical line, and under it the device it plays on by its whole
/// name in a span of its own (listening.css colours it); written only when
/// it changes (the studio's render writes the line four times a second).
function setSub(el, text, dev) {
    if (!el) return;
    const key = text + '\u0000' + dev;
    if (el.dataset.lmSub === key && el.textContent === text + dev) return;
    el.dataset.lmSub = key;
    el.textContent = text;
    if (dev) {
        const d = document.createElement('span');
        d.className = 'pl-sub-dev';
        d.textContent = dev;
        el.appendChild(d);
    }
}

// ── the switch ────────────────────────────────────────────────────────

let strip = null;
let target = false;     // the mode the last switch is going to (the class may lag during a fade)
let gen = 0;            // generation of the running switch
let running = [];       // its Animation objects
let cleanup = null;     // its cleanup, run exactly once

function syncStrip() {
    if (!strip) return;
    const on = isListening();
    strip.setAttribute('aria-pressed', on ? 'true' : 'false');
    strip.dataset.labTok = on ? TEXTS.listenTok : TEXTS.studioTok;
    strip.dataset.labHue = '#7dd3fc';
    strip.dataset.labName = on ? TEXTS.listenName : TEXTS.studioName;
    strip.dataset.labWhy = `${on ? TEXTS.listenWhy : TEXTS.studioWhy}<span class="hint">${TEXTS.hint}</span>`;
}

/// The rack goes away under a pointer or a focus that player.js tracks for
/// its showcase (scRackActive). A hidden element gets no mouseleave, so
/// the flag would stay set and the showcase would never come; the rack's
/// own listeners are told instead.
function releaseRack() {
    const lf = $('labField');
    if (!lf) return;
    if (lf.contains(document.activeElement)) document.activeElement.blur();
    lf.dispatchEvent(new MouseEvent('mouseleave'));
}

/// In the big player the spectrum band is taller and wider than the
/// studio's strip; its canvas is sized to the band, in device pixels.
function sizeSpectrum() {
    if (!isListening()) return;
    const bg = document.querySelector('#plBar .pl-spec-bg');
    const canvas = $('plSpectrum');
    if (!bg || !canvas) return;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.round(bg.clientWidth * dpr), h = Math.round(bg.clientHeight * dpr);
    if (w > 0 && h > 0 && (canvas.width !== w || canvas.height !== h)) { canvas.width = w; canvas.height = h; }
}

/// In the big player the transport is its own grid cell; put it on whole
/// device pixels the way player.js does for the studio's slot.
function snapBigTransport() {
    const tr = document.querySelector('#plBar .pl-transport');
    if (!tr) return;
    tr.style.translate = '';
    if (!isListening()) return;
    const dpr = window.devicePixelRatio || 1;
    const r = tr.getBoundingClientRect();
    const fx = r.left * dpr - Math.floor(r.left * dpr), fy = r.top * dpr - Math.floor(r.top * dpr);
    const dx = fx > 0.001 && fx < 0.999 ? (1 - fx) / dpr : 0;
    const dy = fy > 0.001 && fy < 0.999 ? (1 - fy) / dpr : 0;
    if (dx || dy) tr.style.translate = `${dx}px ${dy}px`;
}

/// Put the class on or off, and everything that follows from it, with no
/// animation.
function apply(on) {
    document.body.classList.toggle(CLS, on);
    if (on) releaseRack();
    computeLayout();
    syncStrip();
    sizeSpectrum();
    snapBigTransport();
    revolverResync();
    // Back in the studio: player.js re-snaps its transport and the
    // showcase slide on resize.
    if (!on) window.dispatchEvent(new Event('resize'));
    afterSub(lastStatus);
}

let lastStatus = null;

function finishRunning() {
    const c = cleanup;
    cleanup = null;
    for (const a of running) { try { a.cancel(); } catch (_) {} }
    running = [];
    c?.();
}

/// `remember` false: a switch that is not the listener's (the tour shows
/// the other layout and puts the listener's back) is not kept for the next
/// start.
export function setListening(on, animate = true, remember = true) {
    // Already going there: a second press only hurries the running switch
    // to its end, it does not start the same switch over again.
    if (on === target) { finishRunning(); if (on === isListening()) return; }
    target = on;
    finishRunning();
    hideTip();
    if (remember) store(on);
    if (!animate) { apply(on); return; }
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) fade(on);
    else flip(on);
}

export const toggleListening = () => setListening(!target);

// ── the animation ─────────────────────────────────────────────────────
//
// FLIP: measure (First), switch the class, measure again (Last), start
// every element from where it was and let it run to where it is. Both
// measurements are taken in one task, so nothing is painted in between.
// Only transform, opacity and clip-path run per frame, plus the top and
// height of one childless pseudo-element (the player's shell).

const EASE = 'cubic-bezier(.2,.7,.2,1)';

/// The rack blocks that are on screen right now: the panel's children that
/// the listening layout hides.
function rackBlocks() {
    const keep = el => el.matches('.app-head-bar, #appVersion, h2, .specs, #plBar, #plListHead, #dropZone');
    return [...$('converterPanel').children].filter(el => !keep(el) && el.getBoundingClientRect().height > 0);
}

/// The player's cells that exist in both layouts and move between them.
/// The cover is not one of them: the studio's 34 px square and the big
/// player's background have nothing to move between; it fades in.
function barCells() {
    const bar = $('plBar');
    return ['.pl-transport', '.pl-now', '.pl-time-col', '.pl-seek', '.pl-dev', '.pl-status', '.pl-spec-bg']
        .map(sel => bar.querySelector(sel)).filter(Boolean);
}

const rectOf = el => el.getBoundingClientRect();

/// Where a cell is and how visible.
function cellState(el) {
    return { r: rectOf(el), o: parseFloat(getComputedStyle(el).opacity) };
}
/// The studio's rows: the stylesheet's ten, or fewer on a low screen (main.js).
const studioRows = () => parseInt(getComputedStyle(document.documentElement).getPropertyValue('--pl-visible-rows')) || 10;

const zoneHeightFor = rows => {
    const root = getComputedStyle(document.documentElement);
    const rowH = px(root.getPropertyValue('--pl-row-h')) || 43;
    const gap = px(root.getPropertyValue('--pl-row-gap'));
    const pad = px(root.getPropertyValue('--pl-list-pad'));
    const zs = getComputedStyle($('dropZone'));
    return rows * rowH + (rows - 1) * gap + 2 * pad + px(zs.borderTopWidth) + px(zs.borderBottomWidth);
};

function flip(on) {
    const my = ++gen;
    const body = document.body;
    const panel = $('converterPanel'), bar = $('plBar'), head = $('plListHead'), zone = $('dropZone');
    const title = $('plTitle');

    // ── First ─────────────────────────────────────────────────────────
    const pr = rectOf(panel);
    const ps = getComputedStyle(panel);
    const rack = on ? rackBlocks() : [];
    const rackFirst = rack.map(rectOf);
    const cells = barCells();
    const cellFirst = cells.map(cellState);
    const F = { bar: rectOf(bar), head: rectOf(head), zone: rectOf(zone), titleFs: px(getComputedStyle(title).fontSize) };

    // ── switch ────────────────────────────────────────────────────────
    body.classList.add('lm-anim');
    if (on) {
        // The rack blocks stay where they were, out of the flow, while they
        // fade: their offsets are from the panel's padding box.
        rack.forEach((el, i) => {
            const r = rackFirst[i];
            el.classList.add('lm-ghost');
            el.style.top = (r.top - pr.top - px(ps.borderTopWidth) + panel.scrollTop) + 'px';
            el.style.left = (r.left - pr.left - px(ps.borderLeftWidth)) + 'px';
            el.style.width = r.width + 'px';
        });
    } else {
        body.classList.add('lm-keeprows');   // the list keeps its rows until the end
    }
    body.classList.toggle(CLS, on);
    if (on) releaseRack();
    computeLayout();
    syncStrip();
    sizeSpectrum();
    afterSub(lastStatus);   // the big player's lines before they are measured
    bar.classList.add('lm-shell');

    // ── Last ──────────────────────────────────────────────────────────
    const L = { bar: rectOf(bar), head: rectOf(head), zone: rectOf(zone), titleFs: px(getComputedStyle(title).fontSize) };
    const cellLast = cells.map(cellState);

    const anims = [];
    const T = on
        ? { bar: [40, 260], head: [80, 220], zone: [80, 220], rack: 0 }
        : { bar: [0, 260], head: [0, 220], zone: [0, 220], rack: 60 };

    // The player's shell: from the old box to the new one.
    anims.push(bar.animate([
        { top: (F.bar.top - L.bar.top - 1) + 'px', height: (F.bar.height + 2) + 'px' },
        { top: '-1px', height: (L.bar.height + 2) + 'px' },
    ], { duration: T.bar[1], delay: T.bar[0], easing: EASE, fill: 'backwards', pseudoElement: '::before' }));

    // The cells: each from its old place (and size, where it scales) to the
    // new one. The bar itself does not move, so absolute deltas are right.
    cells.forEach((el, i) => {
        const f = cellFirst[i], l = cellLast[i];
        if (!l.r.width || !l.r.height) return;
        const kf0 = {}, kf1 = {};
        if (f.r.width && f.r.height) {
            let sx = 1, sy = 1;
            if (el.matches('.pl-transport')) sx = sy = f.r.width / l.r.width;
            else if (el.matches('.pl-now')) sx = sy = F.titleFs / L.titleFs;
            else if (el.matches('.pl-spec-bg')) { sx = f.r.width / l.r.width; sy = f.r.height / l.r.height; }
            kf0.transform = `translate(${f.r.left - l.r.left}px, ${f.r.top - l.r.top}px) scale(${sx}, ${sy})`;
            kf1.transform = 'translate(0px, 0px) scale(1, 1)';
            el.style.transformOrigin = '0 0';
        }
        if (Math.abs(f.o - l.o) > 0.01 || !f.r.width) { kf0.opacity = f.r.width ? f.o : 0; kf1.opacity = l.o; }
        if (!Object.keys(kf0).length) return;
        anims.push(el.animate([kf0, kf1], { duration: T.bar[1], delay: T.bar[0], easing: EASE, fill: 'backwards' }));
    });
    // The tags line appears with the big player (it has no studio place),
    // and so does the cover under the glass.
    const tags = $('plTags');
    if (on && tags) anims.push(tags.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 150, delay: 150, fill: 'backwards' }));
    for (const el of [bar.querySelector('.pl-art-wrap'), $('plWavesWrap')]) {
        if (on && el) anims.push(el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 220, delay: 120, fill: 'backwards' }));
    }

    // The list's head follows.
    anims.push(head.animate([
        { transform: `translateY(${F.head.top - L.head.top}px)` }, { transform: 'translateY(0px)' },
    ], { duration: T.head[1], delay: T.head[0], easing: EASE, fill: 'backwards' }));

    // The list: moves, and opens (or closes) at the bottom. Entering, it has
    // its new rows already and the clip opens them; leaving, it keeps the
    // rows (lm-keeprows) and the clip closes down to the studio's rows.
    const zoneR = 'round 16px';
    if (on) {
        const cut = Math.max(0, L.zone.height - F.zone.height);
        anims.push(zone.animate([
            { transform: `translateY(${F.zone.top - L.zone.top}px)`, clipPath: `inset(0px 0px ${cut}px 0px ${zoneR})` },
            { transform: 'translateY(0px)', clipPath: `inset(0px 0px 0px 0px ${zoneR})` },
        ], { duration: T.zone[1], delay: T.zone[0], easing: EASE, fill: 'backwards' }));
    } else {
        const cut = Math.max(0, L.zone.height - zoneHeightFor(studioRows()));
        anims.push(zone.animate([
            { transform: `translateY(${F.zone.top - L.zone.top}px)`, clipPath: `inset(0px 0px 0px 0px ${zoneR})` },
            { transform: 'translateY(0px)', clipPath: `inset(0px 0px ${cut}px 0px ${zoneR})` },
        ], { duration: T.zone[1], delay: T.zone[0], easing: EASE, fill: 'both' }));
    }

    // The rack: going, top first; coming back, bottom first.
    const blocks = on ? rack : rackBlocks();
    blocks.forEach((el, i) => {
        const k = on ? i : blocks.length - 1 - i;
        anims.push(on
            ? el.animate([{ opacity: 1, transform: 'translateY(0px)' }, { opacity: 0, transform: 'translateY(10px)' }],
                { duration: 100, delay: k * 30, easing: 'ease-in', fill: 'forwards' })
            : el.animate([{ opacity: 0, transform: 'translateY(-8px)' }, { opacity: 1, transform: 'translateY(0px)' }],
                { duration: 100, delay: T.rack + k * 30, easing: 'ease-out', fill: 'backwards' }));
    });

    running = anims;
    cleanup = () => {
        rack.forEach(el => {
            el.classList.remove('lm-ghost');
            el.style.top = el.style.left = el.style.width = '';
        });
        cells.forEach(el => { el.style.transformOrigin = ''; });
        bar.classList.remove('lm-shell');
        body.classList.remove('lm-keeprows', 'lm-anim');
        computeLayout();
        revolverResync();
        sizeSpectrum();
        snapBigTransport();
        if (!on) window.dispatchEvent(new Event('resize'));
    };
    Promise.all(anims.map(a => a.finished.catch(() => {}))).then(() => {
        if (my !== gen || !cleanup) return;
        const c = cleanup;
        cleanup = null;
        running.forEach(a => { try { a.cancel(); } catch (_) {} });   // drop the held end frames
        running = [];
        c();
    });
}

/// "Reduce motion": no movement at all — what changes fades out, the
/// layout switches, and it fades back in.
function fade(on) {
    const my = ++gen;
    const body = document.body;
    const parts = () => [$('plBar'), $('plListHead'), $('dropZone')].concat(on ? [] : rackBlocks());
    body.classList.add('lm-anim');   // no CSS transition may start under the fade
    const out = parts().concat(on ? rackBlocks() : []).map(el =>
        el.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 100, fill: 'forwards' }));
    running = out;
    cleanup = () => { apply(on); body.classList.remove('lm-anim'); };
    Promise.all(out.map(a => a.finished.catch(() => {}))).then(() => {
        if (my !== gen || !cleanup) return;
        cleanup = null;
        // The switch happens while everything is at zero: the faded-out
        // frames are dropped in the same task, so nothing shows in between.
        out.forEach(a => { try { a.cancel(); } catch (_) {} });
        apply(on);
        const ins = parts().map(el => el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 150, fill: 'backwards' }));
        running = ins;
        cleanup = () => body.classList.remove('lm-anim');
        Promise.all(ins.map(a => a.finished.catch(() => {}))).then(() => {
            if (my !== gen || !cleanup) return;
            const c = cleanup; cleanup = null; running = []; c();
        });
    });
}

// ── the bars ──────────────────────────────────────────────────────────

let barsBtn = null;

const barsShown = () => !$('plBar')?.classList.contains('lm-bars-off');

/// Bars on or off (the switch in the text's corner, the right-click menu):
/// off, player.js stops asking for the spectrum and the picture's box grows
/// to the whole player (listening.css, waves.js).
function setBars(on) {
    $('plBar')?.classList.toggle('lm-bars-off', !on);
    try { localStorage.setItem(KEY_BARS, on ? '1' : '0'); } catch (_) {}
    syncBarsBtn();
    sizeSpectrum();
}

function syncBarsBtn() {
    if (!barsBtn) return;
    const on = barsShown();
    barsBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
    barsBtn.classList.toggle('off', !on);
    barsBtn.dataset.tip = on ? TEXTS.barsOn : TEXTS.barsOff;
}

// ── the L key ─────────────────────────────────────────────────────────

function onKey(e) {
    // The physical key, so the Russian layout (where it types Д) works too.
    if (e.code !== 'KeyL' || e.repeat || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return;
    const t = e.target;
    if (t && t.nodeType === 1) {
        const tag = t.tagName;
        // OPTION: a row of an open list (select.css), typed to as to its select.
        if (t.isContentEditable || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'OPTION') return;
        if (tag === 'INPUT' && !/^(range|checkbox|radio|button|submit|reset|color)$/i.test(t.type || 'text')) return;
    }
    const up = $('upModal');
    if (up && !up.classList.contains('up-hidden')) return;
    const menu = $('labMenu');
    if (menu && menu.style.display === 'block') return;
    e.preventDefault();
    toggleListening();
}

// ── init ──────────────────────────────────────────────────────────────

/// Called by main.js right after initPlayer(), before its next await, so a
/// remembered listening mode is on before the first paint.
export function initListening() {
    const bar = $('plBar');
    if (!bar || $('plModeStrip')) return;

    strip = document.createElement('button');
    strip.type = 'button';
    strip.id = 'plModeStrip';
    strip.className = 'pl-mode-strip';
    strip.setAttribute('aria-label', TEXTS.aria);
    strip.innerHTML = SVG_CHEVRONS;
    bar.insertBefore(strip, bar.firstChild);
    strip.addEventListener('click', e => {
        // After a mouse click the strip gives the focus back, so Space goes
        // on meaning play/pause instead of pressing the strip again.
        if (e.detail > 0) strip.blur();
        toggleListening();
    });
    // The key comes out only near the top edge, not all over the player.
    bar.addEventListener('mousemove', e => {
        const near = e.clientY - bar.getBoundingClientRect().top < NEAR_TOP_PX;
        if (near !== bar.classList.contains('lm-near-top')) bar.classList.toggle('lm-near-top', near);
    });
    bar.addEventListener('mouseleave', () => bar.classList.remove('lm-near-top'));

    // The big player's tag lines, under the title.
    const title = $('plTitle');
    if (title && !$('plTags')) {
        const tags = document.createElement('div');
        tags.className = 'pl-tags';
        tags.id = 'plTags';
        tags.innerHTML = '<span class="pl-tags-artist" id="plTagsArtist"></span><span class="pl-tags-album" id="plTagsAlbum"></span>';
        title.after(tags);
    }

    // The waves where a cover would lie (waves.js), and their look switch.
    if (!$('plWaves')) {
        const wrap = document.createElement('div');
        wrap.className = 'pl-waves-wrap';
        wrap.id = 'plWavesWrap';
        wrap.setAttribute('aria-hidden', 'true');
        wrap.innerHTML = '<canvas class="pl-waves" id="plWaves"></canvas>';
        bar.insertBefore(wrap, bar.firstChild);
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.id = 'plWavesBtn';
        btn.className = 'pl-waves-btn';
        btn.innerHTML = SVG_WAVES + '<span class="pl-waves-name" id="plWavesName"></span>';
        bar.appendChild(btn);
        initWaves({
            wrap, canvas: $('plWaves'), button: btn, label: $('plWavesName'),
            state: playerState, status: playerStatus, delayMs: spectrumDelay, on: isListening,
        });
        // The picture on the whole screen: the left end of the same row.
        const full = document.createElement('button');
        full.type = 'button';
        full.id = 'plFullBtn';
        full.className = 'pl-waves-btn pl-full-btn';
        full.setAttribute('aria-label', 'Full screen');
        full.dataset.tip = 'Full screen — the picture on the whole screen, the title and the time in its corners, the transport under the pointer. Esc or this button: back.';
        full.innerHTML = SVG_FULL;
        bar.appendChild(full);
        initFullView({ bar, wrap, button: full, state: playerState, isListening });
        // The bars' switch: the player's right corner, the look switch beside
        // it on its left (listening.css).
        barsBtn = document.createElement('button');
        barsBtn.type = 'button';
        barsBtn.id = 'plBarsBtn';
        barsBtn.className = 'pl-waves-btn pl-bars-btn';
        barsBtn.setAttribute('aria-label', TEXTS.barsAria);
        barsBtn.innerHTML = SVG_BARS;
        bar.appendChild(barsBtn);
        barsBtn.addEventListener('click', e => {
            if (e.detail > 0) barsBtn.blur();   // Space stays play/pause
            setBars(!barsShown());
        });
        bar.classList.toggle('lm-bars-off', !loadBars());
        syncBarsBtn();
        playerViewHooks.bars = { shown: barsShown, set: setBars };
    }

    // The list's empty hint: its "C to convert" tail is the studio's.
    const hint = document.querySelector('#dzEmpty .dz-empty-hint');
    if (hint && /, C to convert$/.test(hint.textContent)) {
        hint.textContent = hint.textContent.replace(/, C to convert$/, '');
        const tail = document.createElement('span');
        tail.className = 'lm-studio-only';
        tail.textContent = ', C to convert';
        hint.appendChild(tail);
    }

    playerViewHooks.afterSub = s => { lastStatus = s; afterSub(s); fitText(); };
    document.addEventListener('keydown', onKey);

    // Whole rows follow the window (the window follows the studio's height,
    // and a move to another display changes the scaling).
    try {
        // Not while switching: the running animation was measured against
        // the numbers it started with; its cleanup recomputes them.
        new ResizeObserver(() => {
            if (document.body.classList.contains('lm-anim')) return;
            computeLayout(); revolverResync();
        }).observe($('converterPanel'));
        const bg = bar.querySelector('.pl-spec-bg');
        if (bg) new ResizeObserver(sizeSpectrum).observe(bg);
    } catch (_) { /* older engine: numbers are set on every switch */ }
    window.addEventListener('resize', () => { if (isListening()) snapBigTransport(); });

    target = load();
    apply(target);
}
