
// ══════════════════════════════════════════════════════════════════════
// The player: the converter's chain, live.
//
// There is no player "mode". The rack above is the one rack; the list below
// is the one list. A row's ▶ plays the file through exactly the chain the
// rack shows — the same engine stages, and a convolver that matches the
// converter's CPU path bit for bit — and every badge, slider and select
// reaches the sound within about half a second. A row's C writes it with
// that chain. What you hear is what you convert, with the exceptions named
// in the status line (a custom filter, Polyphase FIR off: the player always
// plays the filter matrix through the polyphase path).
//
// This file owns the transport strip over the list, the output device, the
// volume, BIT-PERFECT, and the bridge from the rack to the backend. The rows
// themselves belong to dropzone.js, which calls back through `listHooks`.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';
import { listHooks, refreshRows } from './dropzone.js';
import { TAP_PRESETS, FS_PRESETS } from './inventory.js';
import { subsonicCornerHz, FEAT_BY_ID, PLAYER_CHAIN, syncDspRack } from './dsprack.js';
import { xtcGeometryPayload } from './xtc.js';
import { wavesMenu } from './waves.js';
import { showNow } from './tooltip.js';
import {
    RADIO_TEXTS, initRadio, radioRender, radioLines, radioStateText,
    lastStream, rememberStream, isStreamAddress,
} from './radio.js';
import { fileTransport } from './transport.js';
import { addPopup } from './popups.js';
import {
    normTok, stageStates, switchesLive, preparingBeforeSound, airOf, tapsLabel, tapsChip, waitsOnAir, barStep, barView,
    bitPerfectLine, radioBar,
} from './arming.js';
import {
    mKey, isMActive, getMSettings, setM, getM, readMSnapshot, applyMSnapshot,
    toggleMEntry, loadMemory, loadBaseRack,
    saveBaseRackIfAbsent, restoreAndClearBaseRack, getBaseRack,
} from './memory.js';

const invoke = (cmd, args) => window.__TAURI__.tauri.invoke(cmd, args);
const $ = id => document.getElementById(id);
// Escape for both attribute values and text content.
const esc = s => String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');

const KEY_DEVICE    = 'auraPlayerDevice';
const KEY_VOLUME    = 'auraPlayerVolume';
const KEY_BITPERFECT = 'auraPlayerBitPerfect';
const KEY_REPEAT    = 'auraPlayerRepeat';
const KEY_INSTANT   = 'auraPlayerInstantStart';

// ── GPU chip texts (K6 — all in one place; {from}/{to} filled at render time) ──
// UI texts. Keep them here; do not scatter them.
const GPU_CHIP_TEXTS = {
    // GPU chip tooltip: active state.
    active:
        'Rendered on the graphics card in real time — the CPU alone has too little ' +
        'headroom for this filter. Same result as the CPU.',
    // GPU chip tooltip: failed state (chip stays, red style).
    failed:
        'The graphics card stopped responding during this track. ' +
        'Playback continued on the CPU.',
    // Taps chip tooltip while blinking: GPU failure caused the downgrade.
    tapGpuFailed:
        'Switched to {to} automatically: the graphics card failed and this CPU cannot ' +
        'run {from} in real time. Click to dismiss.',
    // Taps chip tooltip while blinking: CPU too slow with no usable GPU.
    tapPower:
        'Switched to {to} automatically: this computer cannot run {from} in real time. ' +
        'Click to dismiss.',
};

const load = (k, d) => { try { const v = localStorage.getItem(k); return v === null ? d : v; } catch (_) { return d; } };
const store = (k, v) => { try { localStorage.setItem(k, String(v)); } catch (_) {} };

// [listening mode] The one hook the big player needs (listening.js): called
// right after the title and sub lines are written — by render() and by the
// showcase swap — with the status they were written from, so the big player
// lays out the same data from the same pass.
export const playerViewHooks = { afterSub: null, bars: null };

let now = { id: null, state: 'stopped' };   // the player's track id and state
let status = null;                           // last status poll
let bitPerfect = load(KEY_BITPERFECT, '0') === '1';
let repeatMode = load(KEY_REPEAT, 'off');    // 'off' | 'all' | 'one'
// The menu's Instant start tick: on, the sound starts at once and the stages
// switch in as they are ready; off, the player prepares the whole chain first
// (the backend's `instant_start`). Sent with the rack.
let instantStart = load(KEY_INSTANT, '1') !== '0';
// What ▶ starts when nothing plays: the list, or the stream last played
// (after a stream's Stop, or with nothing in the list to play).
let lastSource = 'list';

// Acknowledge key for the blinking taps chip after a GPU→CPU downgrade.
// Format: `${trackId}:${gen}` where gen = aud.downgrade.gen (u32, never 0).
// Cleared on track change; set on user click.
let tapWarnAckKey = null;

// ── showcase mode state ───────────────────────────────────────────────
// Showcase: playing + pointer off #plBar + focus out for 1.2 s. Returns in
// ~0.2 s on pointer enter, focus, Space, pause/stop, seek, device list, tooltip.
let showcaseMode = false;
let showcaseTimer = null;
let scPointerOver = false;   // pointer currently over #plBar
let scFocusInside = false;   // keyboard focus currently inside #plBar
let scRackActive  = false;   // pointer over or focus inside the rack (#labField)
let scPressed = false;       // a mouse button held (a drag): the showcase waits for its end

// ── track memory live tracking ────────────────────────────────────────
// mLiveId: the track id whose M snapshot is currently active (the rack is
// showing that track's settings). Rack changes while this is set go into
// the track's memory rather than the global base.
let mLiveId = null;

/// Whether the currently playing track is an M track.
function isNowMTrack() {
    if (!status || !status.trackId) return false;
    const e = entryOfTrack(status.trackId);
    if (!e) return false;
    return isMActive(e);
}
let uiError = null, uiErrorTimer = null;

// Timers used to remove the one-shot failure-flash class from rack badges.
const failedFlashTimers = new Map();

// ── showcase helpers ──────────────────────────────────────────────────

// While the newcomer's tour (tour.js) points at the player, the showcase
// would fade the very controls it points at: the tour holds them up.
let scTourHold = false;

/// The tour's hold on the player's controls: on, the showcase leaves and
/// does not come; off, it goes on as the pointer and the focus say.
export function holdPlayerControls(on) {
    scTourHold = !!on;
    if (scTourHold) { leaveShowcase(); return; }
    refreshShowcaseInputs();
    maybeScheduleShowcase();
}

function isShowcaseEligible() {
    if (status?.state !== 'playing') return false;
    if (scTourHold) return false;
    if (scPointerOver) return false;
    if (scFocusInside) return false;
    if (scRackActive)  return false;
    if (seeking)       return false;
    // Device list open (select has focus).
    if (document.activeElement === $('plDevice')) return false;
    // Any tooltip currently visible.
    if (document.getElementById('labTip')?.classList.contains('show')) return false;
    return true;
}

/// The pointer and the focus as they are, not as the last events left them.
/// The countdown was only tried on an event (a track starting, the pointer
/// leaving): a tooltip up at that moment, or an "over" left from a pointer
/// that never moved out (the window or the layout changed under it), kept
/// the controls up until the pointer went over the player and away (Anton
/// 27.09). Focus holds the controls only when it came by keyboard: one a
/// click left behind does not (the showcase follows the pointer).
function refreshShowcaseInputs() {
    const bar = $('plBar');
    if (!bar || scPressed) return;
    scPointerOver = bar.matches(':hover');
    const ae = document.activeElement;
    scFocusInside = !!ae && ae !== document.body && bar.contains(ae) && ae.matches(':focus-visible');
}

/// Start the 1.2 s countdown to showcase mode. No-ops if already running or
/// already in showcase, or if not eligible right now.
function maybeScheduleShowcase() {
    if (showcaseMode || showcaseTimer) return;
    if (!isShowcaseEligible()) return;
    showcaseTimer = setTimeout(() => {
        showcaseTimer = null;
        if (isShowcaseEligible()) setShowcase(true);
    }, 1200);
}

/// Enter or leave showcase mode and apply the CSS class + text updates.
function setShowcase(on) {
    if (showcaseMode === on) return;
    showcaseMode = on;
    const top = $('plTop');
    if (top) top.classList.toggle('pl-showcase', on);
    // Immediately swap the sub-line so the text is ready before the
    // transition starts (the transition takes 0.5 s, render() has 0.25 s lag).
    if (on) {
        const s = status;
        const e = s?.trackId != null ? entryOfTrack(s.trackId) : null;
        const info = e?.info;
        if (e) {
            const parts = [];
            if (info?.artist) parts.push(info.artist);
            if (info?.album)  parts.push(info.album);
            if (info?.year)   parts.push(String(info.year));
            const subEl = $('plSub');
            if (subEl) subEl.textContent = parts.length
                ? parts.join(' · ')
                : e.name.replace(/\.[^.]+$/, '');
        }
    }
    // When turning off, render() will restore the technical sub on the next
    // poll (within 250 ms), which is before the 0.2 s return transition ends.
    playerViewHooks.afterSub?.(status);   // [listening mode]
}

/// Cancel any pending showcase timer and leave showcase mode.
function leaveShowcase() {
    if (showcaseTimer) { clearTimeout(showcaseTimer); showcaseTimer = null; }
    setShowcase(false);
}

// ── album art ──────────────────────────────────────────────────────────

/// Load the cover art for the given track id into the art element.
/// Called when the playing track changes. The image is fetched over
/// aura.localhost so it is always in cache by the time showcase activates.
function updateArt(trackId) {
    const wrap = $('plArtWrap');
    const img  = $('plArt');
    const init = $('plArtInit');
    if (!img || !wrap) return;

    if (trackId == null) {
        img.removeAttribute('src');
        img.style.display = 'none';
        if (init) { init.textContent = ''; init.style.display = 'none'; }
        wrap.classList.remove('pl-art-no-cover', 'pl-art-hidden');
        snapShowcaseSlide();
        return;
    }

    img.style.display = '';
    img.onload = () => {
        img.style.display = '';
        if (init) init.style.display = 'none';
        wrap.classList.remove('pl-art-no-cover', 'pl-art-hidden');
        snapShowcaseSlide();
    };
    img.onerror = () => {
        // No cover art: hide the art square entirely so the title can take
        // its place in showcase mode.
        img.style.display = 'none';
        if (init) { init.textContent = ''; init.style.display = 'none'; }
        wrap.classList.remove('pl-art-no-cover');
        wrap.classList.add('pl-art-hidden');
        snapShowcaseSlide();
    };
    img.src = `https://aura.localhost/player/cover?id=${trackId}`;
}

/// Compute the showcase slide offset (px) so the title appears right next to
/// the album art (or the left edge of the slot when there is no art).
/// Called once after layout, on resize, and when art presence changes.
function snapShowcaseSlide() {
    const slot = document.querySelector('.pl-left-slot');
    const top  = $('plTop');
    if (!slot || !top) return;
    const slotW = slot.getBoundingClientRect().width;
    // When art is present: title lands at (artW + gap) past the slot's left
    // edge. When absent (pl-art-hidden): slide the title to the slot start.
    const wrap = $('plArtWrap');
    const hasArt = wrap && !wrap.classList.contains('pl-art-hidden');
    const artW = hasArt ? 34 : 0;
    const gap  = hasArt ? 6  : 0;
    // Normal position of pl-now is (slotW + 8 px flex gap) from slot start.
    const slide = -Math.round(slotW - artW - gap);
    top.style.setProperty('--pl-showcase-slide', slide + 'px');
}

// The two frequent reads — the status four times a second, the spectrum up
// to sixty — go over the aura.localhost protocol. invoke is not for polling
// (a WebView leak once grew to 40 GB that way), so a failed fetch is not a
// switch for the rest of the session: the protocol is tried again after a
// growing pause (0.5 → 1 → 2 → 4 → 8 → 10 s) and the first success returns
// to it. Meanwhile the status comes through invoke at most once a second;
// the spectrum never does, it just waits the pause out.
const PROTO_PAUSE_MIN = 500, PROTO_PAUSE_MAX = 10000, INVOKE_STATUS_MS = 1000;
const proto = {
    status:   { down: false, pause: 0, retryAt: 0 },
    spectrum: { down: false, pause: 0, retryAt: 0 },
};
const protoDue = p => !p.down || performance.now() >= p.retryAt;
function protoFailed(p) {
    p.pause = p.pause ? Math.min(PROTO_PAUSE_MAX, p.pause * 2) : PROTO_PAUSE_MIN;
    p.retryAt = performance.now() + p.pause;
    p.down = true;
}
function protoOk(p) { p.down = false; p.pause = 0; p.retryAt = 0; }
let lastStatusInvoke = 0;
let radioArtGone = false;   // the cover was taken away for a stream

function showUiError(msg) {
    uiError = String(msg);
    console.warn('[player]', uiError);
    invoke('player_ui_log', { level: 'warn', message: uiError }).catch(() => {});
    if (uiErrorTimer) clearTimeout(uiErrorTimer);
    uiErrorTimer = setTimeout(() => { uiError = null; }, 15000);
}

// ── the rack, read the way the converter reads it ─────────────────────

/// What `player_set_settings` gets: the rack exactly as `startConversion`
/// reads it for `convert_files` (same controls, same parsing), in the
/// player's shape. The one field the rack does not have is `mode`.
export function collectPlayerSettings() {
    const pos = (id, n) => Math.min(parseInt($(id)?.value) || 0, n - 1);
    const on = id => !!$(id)?.checked;
    // αHP runs inside Hybrid-Phase; TFS takes the slot Hybrid-Phase would;
    // with none of the three the filter is the plain linear-phase one.
    const phase = on('convLabAlpha') ? 'alpha'
        : on('convHybridPhase') ? 'hybrid'
        : on('convLabTfs') ? 'tfs'
        : 'linear';
    return {
        mode: bitPerfect ? 'direct' : 'aura',
        fsMultiplier: FS_PRESETS[pos('convFsSlider', FS_PRESETS.length)],
        taps: TAP_PRESETS[pos('convTapSlider', TAP_PRESETS.length)],
        phase,
        apodizing: parseInt($('convApodizing')?.value || '0'),
        adaptiveApodizer: on('convAdaptiveApodizer'),
        headroomDb: parseFloat($('convHeadroom')?.value || '0'),
        iirDcBlocking: on('convIirDc'),
        subsonicHz: subsonicCornerHz(),
        declip: on('convLabDeclip'),
        isp: on('convLabIsp'),
        adaptiveHeadroom: on('convLabHeadroom'),
        xtc: on('convLabXtc'),
        xtcGeometry: xtcGeometryPayload(),
        // K7: the converter's GPU checkbox is the single GPU switch for the
        // whole app; the backend decides per-chain whether to use it.
        useGpu: on('convGpuCheck'),
    };
}

/// Build a PlayerSettings object from an M snapshot, using the current rack
/// for non-M fields (FS, taps, phase where not overridden). The result is
/// what player_set_track_settings expects.
function buildPlayerSettingsFromSnap(snap) {
    const pos = (id, n) => Math.min(parseInt($(id)?.value) || 0, n - 1);
    // Phase precedence: α > HP > TFS > linear, using M snapshot values.
    const isAlpha = snap.labFeatures?.continuousAlpha || false;
    const isHp    = snap.convHybridPhase || false;
    const isTfs   = !isHp && (snap.labFeatures?.tfsPhase || false);
    const phase   = isAlpha ? 'alpha' : isHp ? 'hybrid' : isTfs ? 'tfs' : 'linear';
    const sub = snap.convSubsonic ? (parseFloat(snap.convSubsonicHz) || 15) : 0;
    return {
        mode: 'aura',
        fsMultiplier: FS_PRESETS[pos('convFsSlider', FS_PRESETS.length)],
        taps:         TAP_PRESETS[pos('convTapSlider', TAP_PRESETS.length)],
        phase,
        apodizing:        parseInt(snap.convApodizing || '0'),
        adaptiveApodizer: snap.convAdaptiveApodizer,
        headroomDb:       parseFloat(snap.convHeadroom || '0'),
        iirDcBlocking:    !!$('convIirDc')?.checked,
        subsonicHz:       sub,
        declip:           snap.labFeatures?.declip           || false,
        isp:              snap.labFeatures?.isp              || false,
        adaptiveHeadroom: snap.labFeatures?.adaptiveHeadroom || false,
        xtc:              snap.labFeatures?.xtc              || false,
        xtcGeometry:      snap.xtcGeometry || xtcGeometryPayload(),
        // K7: read from the rack, never from the M snapshot (the converter's
        // GPU setting is not a per-track preference).
        useGpu:           !!$('convGpuCheck')?.checked,
    };
}

/// Where the player cannot sound like the converter would write, say so.
function approximations() {
    if (bitPerfect) return [];
    const out = [];
    if (state.convCustomFilterPath) out.push('custom filter → the player plays the matrix filter');
    if ($('convFirResampling') && !$('convFirResampling').checked) out.push('Polyphase FIR off → the player plays the polyphase path');
    return out;
}

// ── settings + rendered map ───────────────────────────────────────────

/// A rack change goes to the player once the rack has been still this long:
/// a burst of clicks becomes one change, and only what differs from what
/// plays is built. Clicking a stage on and off again sends nothing at all.
/// Selected stages show as pending on the player strip meanwhile.
const RACK_SETTLE_MS = 1500;

let sendTimer = null, lastSent = '', lastInstant = null;
let sendInFlight = null;   // the last send, until the player has taken it (flushRack)
function scheduleSend() {
    if (sendTimer) clearTimeout(sendTimer);
    sendTimer = setTimeout(sendNow, RACK_SETTLE_MS);
}
/// Returns a promise that settles once the backend has taken the settings
/// (at once when there was nothing new to send).
export function sendNow() {
    if (sendTimer) clearTimeout(sendTimer);
    sendTimer = null;
    const s = collectPlayerSettings();
    const r = listHooks.rendered?.() ?? null;
    // Include the rendered map in the dedup key: a conversion finishing
    // without any rack change must still reach the backend.
    const key = JSON.stringify([s, r]);
    if (key === lastSent && instantStart === lastInstant) return Promise.resolve();
    // The Instant start tick goes along with the rack; only a change of the
    // rack holds it while the sound is rebuilt.
    if (key !== lastSent) {
        rackSentAt = performance.now();
        applyRackLock();
    }
    lastSent = key;
    lastInstant = instantStart;
    const sent = invoke('player_set_settings', { settings: s, rendered: r, instantStart }).catch(e => {
        lastSent = '';
        lastInstant = null;
        showUiError('The player did not take the rack: ' + e);
    });
    sendInFlight = sent;
    sent.finally(() => { if (sendInFlight === sent) sendInFlight = null; });
    return sent;
}

/// The menu's Instant start tick: kept for the next session, and sent to the
/// player at once.
function setInstantStart(on) {
    instantStart = on;
    store(KEY_INSTANT, on ? '1' : '0');
    sendNow();
}

/// The rack holds while the player rebuilds the sound (Anton 26.09): from
/// the moment a change goes out until the status says the new sound is on
/// the air. Playing only; BIT-PERFECT and the transport never hold.
let rackSentAt = -1e9;
const RACK_SENT_GRACE_MS = 600;   // the status's first word after a send
function applyRackLock() {
    const s = status;
    const on = !!s && (s.state === 'playing' || s.state === 'paused' || s.state === 'preparing')
        && !bitPerfect
        && (performance.now() - rackSentAt < RACK_SENT_GRACE_MS || s.rebuilding === true);
    const field = $('labField');
    if (field && field.classList.contains('lab-locked') !== on) field.classList.toggle('lab-locked', on);
}

/// A rack change still settling goes out now: whatever starts playing
/// starts with the rack the page shows, not the one before the last click.
/// One already on its way is waited for too: a second action went past it.
function flushRack() {
    return sendTimer ? sendNow() : (sendInFlight ?? Promise.resolve());
}

/// The backend keeps its own copy of a remembered track's settings: it
/// starts the track (a direct play, the prefetch, the gapless hand-over)
/// from that copy, not from the rack. The copy is sent again whenever the
/// memory changes - a rack change while the track plays, the XTC triangle,
/// M on or off - so a replay starts with what the rack shows. Every row of
/// the same memory key gets it.
function pushTrackSettings(key) {
    const sends = [];
    for (const e of state.list) {
        if (e.trackId == null || mKey(e.name) !== key) continue;
        const snap = isMActive(e) ? getMSettings(e) : null;
        sends.push((snap
            ? invoke('player_set_track_settings', { id: e.trackId, settings: buildPlayerSettingsFromSnap(snap) })
            : invoke('player_clear_track_settings', { id: e.trackId })).catch(() => {}));
    }
    return Promise.all(sends);
}

// What the last press asked for. A press decides pause or play from the
// player's state; the status is polled four times a second, so a second
// press inside one poll read the same stale state and asked for the same
// thing twice. The intent of the last press stands in for the status until
// a status shows it (or a few seconds pass): two presses are pause, then play.
const INTENT_MS = 3000;
let intent = null;    // { state: 'playing' | 'paused', id, at }
// `from`: the state the player was in when the intent was made. A state that is
// neither that one, nor the intended one, nor on the way to it (preparing) means the
// player went elsewhere (stopped at the end of the list, or by another hand): the
// intent is dropped, or a Play right after would read it and pause instead.
function intend(state, id) {
    const before = shownNow().id;
    intent = { state, id: id ?? now.id, at: performance.now(), from: status?.state ?? null };
    // The list answers the press at once: the row pressed is lit (getting
    // ready), the one it leaves is not.
    refreshNowRows(before, intent.id);
}

// A press on another track's ▶: until the player plays that track, the list
// keeps the pressed row lit and every row's ▶ waits. The player reports the
// old track playing all the while the new one is prepared, and that report
// took the light back to the old row for a moment; presses meanwhile queued
// one switch behind another in the backend (Anton 27.09). Given up when the
// player turns elsewhere: Stop, Next/Prev, or another track playing with
// nothing being rebuilt for a second.
let switching = null;   // { id, at, away }
const SWITCH_MAX_MS = 30000;
const SWITCH_AWAY_MS = 1000;

function startSwitch(id) {
    switching = { id, at: performance.now(), away: null };
    refreshRows(new Set(state.list.map(e => e.id)));
}

function endSwitch() {
    if (!switching) return;
    switching = null;
    refreshRows(new Set(state.list.map(e => e.id)));
}

/// The status says where the switch stands: there (the track plays, or is
/// paused on), or turned elsewhere.
function checkSwitch(s) {
    if (!switching) return;
    const t = performance.now();
    if (s.trackId === switching.id && (s.state === 'playing' || s.state === 'paused')) return endSwitch();
    const away = !s.rebuilding && s.state !== 'preparing' && s.trackId !== switching.id;
    switching.away = away ? (switching.away ?? t) : null;
    if ((switching.away != null && t - switching.away >= SWITCH_AWAY_MS) || t - switching.at >= SWITCH_MAX_MS) endSwitch();
}

/// What the list shows as playing: the track a ▶ switched to until it plays;
/// the listener's last press for as long as it is fresh (a new track
/// "preparing" until the player reports it); else what the player reports.
/// `locked`: the rows' ▶ wait. Not with Instant start off: a track then waits
/// for its whole chain on a thread of its own, and another ▶ takes its place
/// at once rather than queue behind it.
function shownNow() {
    if (switching) return { id: switching.id, state: 'preparing', locked: instantStart };
    if (intent && intent.id != null && performance.now() - intent.at < INTENT_MS) {
        const other = intent.id !== now.id;
        return { id: intent.id, state: other && intent.state === 'playing' ? 'preparing' : intent.state };
    }
    return now;
}

/// Redraw the rows of these track ids.
function refreshNowRows(...trackIds) {
    const ids = new Set();
    for (const t of trackIds) {
        const e = t != null ? entryOfTrack(t) : null;
        if (e) ids.add(e.id);
    }
    if (ids.size) refreshRows(ids);
}
function current() {
    if (intent && performance.now() - intent.at < INTENT_MS) return { id: intent.id, state: intent.state };
    return { id: now.id, state: status?.state };
}

// ── the list ↔ the player's queue ─────────────────────────────────────

const entryOfTrack = id => state.list.find(e => e.trackId === id);

const samePath = (a, b) => !!a && !!b
    && a.replace(/\//g, '\\').toLowerCase() === b.replace(/\//g, '\\').toLowerCase();

/// The chain of a converted file played from disk, as [tok, state, why]:
/// the conversion's own record (this row's, else any row's — the file is one
/// thing on disk), else the converter's queue entry that wrote it (its chain
/// can reach the queue a poll after the record was taken), else the stages
/// the backend read off the file's name. A record wins only when it holds at
/// least every stage the name lists.
function diskChain(trackId, path, nameStages) {
    const fromName = Array.isArray(nameStages) ? nameStages : [];
    if (!path) return fromName;
    const entry = entryOfTrack(trackId);
    const recs = [...(entry?.convs || []), ...state.list.flatMap(e => e === entry ? [] : (e.convs || []))];
    const rec = recs.find(c => samePath(c.out, path));
    const q = [...(state.convFileQueue || [])].reverse().find(f => samePath(f.outPath, path));
    const fired = ch => (ch || []).filter(r => r[1] === 1).length;
    for (const ch of [rec?.labChain, q?.labChain]) {
        if (ch?.length && fired(ch) >= fromName.length) return ch;
    }
    return fromName;
}

async function onAdded(entries) {
    let r;
    try {
        r = await invoke('player_add', { paths: entries.map(e => e.path) });
    } catch (e) {
        showUiError('The player could not read the files: ' + e);
        return;
    }
    const ids = new Set();
    for (const t of r.tracks || []) {
        const e = entries.find(x => x.path === t.path && x.trackId == null);
        if (!e) continue;
        e.trackId = t.id;
        e.info = t;
        ids.add(e.id);
    }
    // A file the player could not open would fail the conversion the same
    // way: the row says so instead of offering buttons that cannot work.
    for (const e of entries) {
        if (e.trackId != null) continue;
        e.unreadable = (r.errors || []).find(m => m.includes(e.path)) || 'This file could not be read';
        ids.add(e.id);
    }
    refreshRows(ids);
    if (r.errors?.length) showUiError(r.errors.join('\n'));
    // Dropped into the middle of the list: the player's queue takes the
    // list's order (it appended them).
    const tail = state.list.slice(-entries.length);
    if (entries.some((e, i) => tail[i] !== e)) {
        invoke('player_reorder', { ids: state.list.map(e => e.trackId).filter(id => id != null) }).catch(showUiError);
    }

    // Register M settings for any M tracks we just added, so the prefetch
    // chain uses the right settings even if the global rack changes later.
    for (const e of entries) {
        if (e.trackId == null) continue;
        const snap = getMSettings(e);
        if (isMActive(e) && snap) {
            const s = buildPlayerSettingsFromSnap(snap);
            invoke('player_set_track_settings', { id: e.trackId, settings: s }).catch(() => {});
        }
    }
    // The rows have the player's ids now, so the map of which rows play a
    // converted file can name them. At start the list is restored before the
    // player answers: the map went out without these rows and nothing sent
    // it again — a converted row played live after a restart (Anton 28.09).
    sendNow();
}

function onRemoved(e) {
    if (e.trackId != null) {
        invoke('player_remove', { id: e.trackId }).catch(() => {});
        if (e.trackId === mLiveId) {
            mLiveId = null;
        }
    }
}

function onPlay(entry) {
    if (!entry) {
        // The transport's ▶/⏸ and the list's ▶ Play: pause, resume, or start
        // the list from the top.
        const cur = current();
        if (cur.state === 'playing') { intend('paused'); return invoke('player_pause').catch(showUiError); }
        if (cur.state === 'paused') { intend('playing'); return flushRack().then(() => invoke('player_play')).catch(showUiError); }
        // A track is getting ready (with Instant start off, its whole chain
        // first): ▶ waits for it rather than start the list from the top.
        if (cur.state === 'preparing') return;
        const first = state.list.find(e => e.trackId != null);
        const stream = lastStream();
        if (stream && (lastSource === 'radio' || !first)) { playRadio(stream); return; }
        if (first) {
            lastSource = 'list';
            setHeld(true);
            intend('playing', first.trackId);
            startSwitch(first.trackId);
            flushRack().then(() => invoke('player_play', { id: first.trackId }))
                .catch(e => { endSwitch(); showUiError(e); });
        } else if (state.list.length) {
            showUiError('Nothing in the list can be played — the files could not be read');
        }
        return;
    }
    if (entry.trackId == null) {
        showUiError(`${entry.name}: the player could not read this file`);
        return;
    }
    // Another track is getting ready: the rows' ▶ wait for it — with Instant
    // start off not (see shownNow).
    if (switching && instantStart) return;
    // ▶ on the row that is playing pauses it; on a paused one, resumes; on
    // the one still being prepared, waits for it rather than starting over.
    const cur = current();
    if (cur.id === entry.trackId && cur.state === 'preparing') return;
    if (cur.id === entry.trackId && cur.state === 'playing') { intend('paused', entry.trackId); return invoke('player_pause').catch(showUiError); }
    if (cur.id === entry.trackId && cur.state === 'paused') { intend('playing', entry.trackId); return flushRack().then(() => invoke('player_play')).catch(showUiError); }
    setHeld(true);

    // M track: apply its settings to the rack DOM before play so the
    // backend starts with the right chain from frame one. The play waits for
    // the backend to have taken them: sent in the same tick, the play ran
    // first, from the track's old copy, and the chain the memory asked for
    // only arrived with the re-render seconds later.
    let ready = null;
    if (!bitPerfect && isMActive(entry)) {
        const snap = getMSettings(entry);
        if (snap) {
            // Save the current rack as base before the first M track takes over.
            saveBaseRackIfAbsent();
            applyMSnapshot(snap);
            mLiveId = entry.trackId;
            ready = Promise.all([pushTrackSettings(mKey(entry.name)), sendNow()]);
        }
    } else {
        // Non-M track: restore the base rack if an M track was playing before.
        if (getBaseRack()) {
            restoreAndClearBaseRack();
            ready = sendNow();
        }
        mLiveId = null;
    }

    lastSource = 'list';
    intend('playing', entry.trackId);
    startSwitch(entry.trackId);
    Promise.all([ready, flushRack()])
        .then(() => invoke('player_play', { id: entry.trackId }))
        .catch(e => { endSwitch(); showUiError(e); });
}

/// What ▶ would start while nothing plays (onPlay), for the transport's row
/// (transport.js).
const transportContext = () => ({
    lastSource, stream: !!lastStream(), file: state.list.some(e => e.trackId != null),
});

/// Play a stream: the radio view's station or address, or ▶ after a
/// stream's Stop. Returns the words why it cannot, or null.
function playRadio(url) {
    const u = String(url || '').trim();
    if (!isStreamAddress(u)) return RADIO_TEXTS.address;
    lastSource = 'radio';
    rememberStream(u);
    intent = null;
    endSwitch();
    leaveShowcase();
    setHeld(true);
    flushRack().then(() => invoke('player_radio', { url: u })).catch(showUiError);
    return null;
}

/// Stop: the transport's ■.
function stopPlayer() {
    intent = null;
    endSwitch();
    leaveShowcase();
    invoke('player_stop').catch(showUiError);
    setHeld(false);
}

/// BIT-PERFECT turned off on a remembered track: the player plays its M
/// rack from now on (BIT-PERFECT is above M while it is on), so the rack
/// shows it too, as when that track starts. It went on showing the base
/// rack while the player played the remembered one.
function takeMemoryOfPlaying() {
    const cur = current();
    if (cur.id == null || !cur.state || cur.state === 'stopped') return;
    const entry = entryOfTrack(cur.id);
    const snap = entry && isMActive(entry) ? getMSettings(entry) : null;
    if (!snap) return;
    saveBaseRackIfAbsent();
    applyMSnapshot(snap);
    mLiveId = entry.trackId;
    pushTrackSettings(mKey(entry.name));
}

// ── what the player does not play: held while a track is on ────────────
// Three controls change the converted file and never the sound here: a
// custom .npy filter (the player plays the filter matrix), Polyphase FIR
// (the player always renders through it) and Album Level (one gain across
// a batch). While a track plays, is paused or is getting ready they hold —
// dimmed, with the not-allowed cursor, and a press on one says at once why
// — and Stop lets them go (Anton 1.10). PFR is switched on for the track,
// so the rack shows what plays, and back off on Stop if the listener had it
// off: the saved settings keep the listener's own (state.pfrBeforePlay).
// PFR is the one that plays, so it holds lit, not dimmed — dimmed it read
// as switched off (Anton 3.10). A
// filter loaded before ▶ stays loaded: the ≈ chip says the player plays the
// matrix instead.

// UI texts.
const HOLD_TEXTS = {
    npy: 'The player plays the built-in filter; a custom filter is for conversion. Stop to change it.',
    pfr: 'The player always renders through Polyphase FIR. Stop to change it.',
    alb: 'Album Level is a conversion setting — the player plays each track at its own level. Stop to change it.',
};
// The rack's badges are buttons with data-id; their checkboxes live apart,
// hidden (dsprack.js), and are what a held badge keeps as it was.
const HELD = [
    { sel: '#convLoadFilterBtn', tip: HOLD_TEXTS.npy },
    { sel: '#convClearFilterBtn', tip: HOLD_TEXTS.npy },
    { sel: '.lab-badge[data-id="convFirResampling"]', tip: HOLD_TEXTS.pfr, input: 'convFirResampling', lit: true },
    { sel: '.lab-badge[data-id="convAlbumLevel"]', tip: HOLD_TEXTS.alb, input: 'convAlbumLevel' },
];
let held = null;           // null, or { checkbox id → the value it holds }
let heldAt = 0;
// ▶, ⏮ and ⏭ hold at once, before the player says it plays; a press that
// starts nothing is let go when the player still says stopped after this.
const HOLD_GRACE_MS = 3000;

/// PFR's checkbox to `on`, as a click would leave it.
function setPfr(on) {
    const cb = $('convFirResampling');
    if (!cb || cb.checked === on) return;
    cb.checked = on;
    cb.dispatchEvent(new Event('change', { bubbles: true }));
    // Repaint the rack so the badge reflects the updated checkbox state.
    syncDspRack();
}

function setHeld(on) {
    if (on) heldAt = performance.now();
    if (!!held === on) return;
    if (on) {
        // Kept first: the change that switches PFR on saves the settings.
        state.pfrBeforePlay = !!$('convFirResampling')?.checked;
        setPfr(true);
        held = Object.fromEntries(HELD.filter(h => h.input).map(h => [h.input, !!$(h.input)?.checked]));
    } else {
        // Let go first: the guard would put PFR back on otherwise.
        held = null;
        const was = state.pfrBeforePlay;
        state.pfrBeforePlay = null;
        if (was === false) setPfr(false);
    }
    for (const h of HELD) {
        const el = document.querySelector(h.sel);
        if (!el) continue;
        el.classList.toggle('pl-held', on);
        el.classList.toggle('pl-held-lit', on && !!h.lit);
        // A badge's tooltip is built by dsprack.js, which adds this line;
        // a button's is its data-tip, put back as it was on Stop.
        if (el.classList.contains('lab-badge')) {
            if (on) el.dataset.holdTip = h.tip;
            else delete el.dataset.holdTip;
        } else if (on) {
            el.dataset.tipIdle = el.dataset.tip ?? '';
            el.dataset.tip = h.tip;
        } else {
            if (el.dataset.tipIdle) el.dataset.tip = el.dataset.tipIdle;
            else delete el.dataset.tip;
            delete el.dataset.tipIdle;
        }
    }
}

// ── transport SVG icons ───────────────────────────────────────────────
// Inline SVG instead of Unicode glyphs: these are geometrically centred in
// their viewbox and rendered consistently across every font renderer.
// The play triangle centroid is shifted ~1 px right (M6 vs M5) to compensate
// for the optical illusion that makes an equal-sided triangle look left-heavy.

const SVG_PREV = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><rect x="2" y="2.5" width="2.5" height="11" rx="1"/><path d="M14 3.5 L6 8 L14 12.5Z"/></svg>`;
// The triangle's box sits one unit right of centre (4.5–13.5 of 16): centred
// by its box it looks pushed left, centred by its centroid it looks pushed
// right; one unit is where it reads as centred in the circle.
const SVG_PLAY = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M4.5 2.5 L13.5 8 L4.5 13.5Z"/></svg>`;
// y=3 (integer) so the bars' top and bottom land on whole viewBox units;
// height 10 keeps the vertical centre at y=8. shape-rendering crispEdges
// ensures the rectangular bars render on whole device pixels.
const SVG_PAUSE = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true" shape-rendering="crispEdges"><rect x="3" y="3" width="3.5" height="10" rx="1.2"/><rect x="9.5" y="3" width="3.5" height="10" rx="1.2"/></svg>`;
const SVG_STOP = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><rect x="3" y="3" width="10" height="10" rx="1.5"/></svg>`;
// The output device: a loudspeaker cabinet in three-quarter view — its
// front with the woofer and the tweeter, its side receding to the right
// (Anton 27.09: a speaker you recognise at a glance).
const SVG_DEVICE = `<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">`
    + `<path d="M13 2.2 17.2 4.3V15.7L13 17.8" opacity=".7"/><rect x="3" y="2.2" width="10" height="15.6" rx="1.2"/>`
    + `<circle cx="8" cy="12.3" r="3.1"/><circle cx="8" cy="12.3" r="0.9" fill="currentColor" stroke="none"/><circle cx="8" cy="5.6" r="1.4"/></svg>`;
const SVG_NEXT = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M2 3.5 L10 8 L2 12.5Z"/><rect x="11.5" y="2.5" width="2.5" height="11" rx="1"/></svg>`;
// One icon per repeat mode, so the button tells which one is on by itself.
// Off: a plain down arrow — the list plays once, top to bottom, and stops.
// Shifted down 0.25 units so the arrow's geometric centre lands at y=8 (the
// viewBox centre) and the icon reads as optically centred in its circle.
const SVG_REPEAT_OFF = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.75V13.25M4.2 9.45 8 13.25l3.8-3.8"/></svg>`;
// Whole list: two circular arrows chasing each other. stroke-width matches the
// down arrow so every repeat icon has the same on-screen stroke thickness.
const SVG_REPEAT_ALL = `<svg viewBox="-0.5 -0.5 17 17" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.8 5A6 6 0 0 1 14 8M12.5 6.5 14 8l1.5-1.5M13.2 11A6 6 0 0 1 2 8M0.5 9.5 2 8l1.5 1.5"/></svg>`;
// One track: thin "1 with flag and underline". stroke-width matches the down
// arrow (1.4) so every repeat icon paints the same on-screen stroke thickness.
const SVG_REPEAT_ONE = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 5.5 L8.5 3 L8.5 13"/><path d="M5.5 13 L11.5 13"/></svg>`;

// ── repeat-button state ───────────────────────────────────────────────

// The rack's tooltip (dsprack.js reads data-lab-*) rather than a title: a
// native tooltip keeps showing the old mode after a click, this one is
// redrawn on the spot.
const REPEAT_MODES = {
    off: { svg: SVG_REPEAT_OFF, name: 'Off',
           why: 'The list plays once, from top to bottom, and stops.',
           next: 'Click: repeat the whole list.' },
    all: { svg: SVG_REPEAT_ALL, name: 'Whole list',
           why: 'After the last track the list starts again from the first.',
           next: 'Click: repeat one track.' },
    one: { svg: SVG_REPEAT_ONE, name: 'One track',
           why: 'The current track plays again and again.',
           next: 'Click: repeat off.' },
};

function updateRepeatBtn() {
    const btn = $('plRepeat');
    if (!btn) return;
    const m = REPEAT_MODES[repeatMode] || REPEAT_MODES.off;
    btn.innerHTML = m.svg;
    btn.dataset.labTok = 'REPEAT';
    btn.dataset.labName = m.name;
    btn.dataset.labWhy = `${m.why}<span class="hint">${m.next}</span>`;
    btn.setAttribute('aria-label', 'Repeat: ' + m.name);
    btn.classList.toggle('pl-repeat-on', repeatMode !== 'off');
    btn.classList.toggle('pl-repeat-one', repeatMode === 'one');
}

// ── the strip over the list ───────────────────────────────────────────

function buildBar() {
    const anchor = $('plListHead') || $('dropZone');
    if (!anchor || $('plBar')) return;
    const bar = document.createElement('div');
    bar.className = 'pl-bar';
    bar.id = 'plBar';
    bar.innerHTML = `
      <div class="pl-top" id="plTop">
        <div class="pl-spec-bg" aria-hidden="true">
          <canvas class="pl-spectrum" id="plSpectrum"></canvas>
        </div>
        <div class="pl-left-slot">
          <div class="pl-transport">
            <button type="button" id="plPrev" title="Previous">${SVG_PREV}</button>
            <button type="button" id="plPlay" class="pl-main" title="Play / Pause (Space)">${SVG_PLAY}</button>
            <button type="button" id="plStop" title="Stop">${SVG_STOP}</button>
            <button type="button" id="plNext" title="Next">${SVG_NEXT}</button>
            <button type="button" id="plRepeat" class="pl-repeat" aria-label="Repeat: Off">${SVG_REPEAT_OFF}</button>
          </div>
          <div class="pl-art-wrap" id="plArtWrap" aria-hidden="true">
            <img class="pl-art" id="plArt" alt="">
            <span class="pl-art-init" id="plArtInit"></span>
          </div>
        </div>
        <div class="pl-now">
          <div class="pl-title" id="plTitle">Nothing playing</div>
          <div class="pl-sub" id="plSub">&#9654; on a row plays it through the rack above</div>
        </div>
        <div class="pl-time-col">
          <span class="pl-time" id="plTime"></span>
          <span class="pl-buf" id="plBuf" style="display:none"></span>
        </div>
      </div>
      <div class="pl-seek" id="plSeek">
        <div class="pl-seek-fill" id="plSeekFill"></div>
        <div class="pl-seek-shimmer" id="plSeekShimmer"></div>
        <span class="pl-seek-thumb" id="plSeekThumb" style="display:none"></span>
        <span class="pl-seek-time-lbl" id="plSeekTimeLbl" style="display:none"></span>
        <span class="pl-seek-hover" id="plSeekHover" aria-hidden="true"><span class="pl-seek-hover-t" id="plSeekHoverT"></span></span>
      </div>
      <div class="pl-dev">
        <span class="pl-dev-pick" id="plDevPick">${SVG_DEVICE}<select id="plDevice" aria-label="Output device"></select></span>
        <button type="button" id="plBitPerfect" class="pl-bp"
          title="Bit-perfect: the file as decoded, at its own rate and depth, straight to the device — no rack, no volume. Switches while playing.">BIT-PERFECT</button>
        <span class="pl-vol-wrap"><input type="range" id="plVol" min="-60" max="0" step="0.1" value="0" aria-label="Volume"><span class="pl-vol-tip" id="plVolTip"></span></span>
        <span class="pl-vol-text" id="plVolText">0 dB</span>
      </div>
      <div class="pl-status" id="plStatus"><span id="plStatusBadges"></span><span id="plStatusMsg" class="pl-status-msg"></span><span class="pl-arm" id="plArm" aria-hidden="true"><span class="pl-arm-fill" id="plArmFill"></span></span></div>`;
    anchor.parentNode.insertBefore(bar, anchor);
    initSpectrum();
    requestAnimationFrame(() => { snapTransport(); snapShowcaseSlide(); });
    document.fonts?.ready.then(() => { snapTransport(); snapShowcaseSlide(); });
    window.addEventListener('resize', () => { snapTransport(); snapShowcaseSlide(); });
}

// The strip's height comes out of the text sizes above it, so its top
// usually falls between two device pixels, and the icons then land half a
// pixel apart: play/pause read higher than their neighbours. Shift the
// left slot (which contains the transport) by the fractional X and Y
// offsets so every button, and every icon in it, sits on whole device
// pixels. Targeting the slot rather than the transport preserves the CSS
// transform on the transport itself (used by the showcase scale). A resize
// also fires when the window moves to a display with another scaling.
function snapTransport() {
    const slot = document.querySelector('.pl-left-slot');
    if (!slot) return;
    slot.style.transform = '';
    const dpr = window.devicePixelRatio || 1;
    const rect = slot.getBoundingClientRect();
    const fy = rect.top  * dpr; const fracY = fy - Math.floor(fy);
    const fx = rect.left * dpr; const fracX = fx - Math.floor(fx);
    const dy = (fracY > 0.001 && fracY < 0.999) ? (1 - fracY) / dpr : 0;
    const dx = (fracX > 0.001 && fracX < 0.999) ? (1 - fracX) / dpr : 0;
    if (dy || dx) slot.style.transform = `translate(${dx}px,${dy}px)`;
}

function fmtTime(s) {
    if (!isFinite(s) || s < 0) return '0:00';
    return `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
}
const fmtRate = hz => { const k = hz / 1000; return (Number.isInteger(k) ? k.toFixed(0) : k.toFixed(1)) + ' kHz'; };
const fmtDb = db => (db === 0 ? '0 dB' : db.toFixed(1) + ' dB');

// ── badge rendering ───────────────────────────────────────────────────
// Same markup as dsprack.js cb() so the global tooltip handler in dsprack.js
// picks up these spans and shows their tooltip on hover.

/// The status line is rebuilt on every poll, which restarts any CSS animation
/// in it; a negative delay taken from the page clock keeps the phase running
/// across rebuilds (the pending pulse, the blinking taps chip).
const animPhase = periodMs => `animation-delay:-${(performance.now() % periodMs / 1000).toFixed(3)}s`;

function cbHtml(cls, tok, name, hue, why) {
    const pulse = cls.split(' ').includes('pending') ? `;${animPhase(1500)}` : '';
    return `<span class="cb ${cls}" style="--h:var(--lab-${hue})${pulse}" data-lab-why="${esc(why)}"` +
        ` data-lab-tok="${esc(tok)}" data-lab-name="${esc(name)}" data-lab-hue="var(--lab-${hue})">${esc(tok)}</span>`;
}

// Whether a DSP stage is currently on in the rack DOM.
const isRackOn = id => !!document.getElementById(id)?.checked;

/// The player's chain as arming.js reads it: each stage's rack id and token.
const chainStages = () => PLAYER_CHAIN.map(id => ({ id, tok: FEAT_BY_ID[id].tok }));

/// The rack's filter size (the converter's taps slider).
const rackTaps = () => TAP_PRESETS[Math.min(Math.max(0, parseInt($('convTapSlider')?.value || '0')), TAP_PRESETS.length - 1)];

/// Something of the rack is on its way to the air: a change not sent yet or
/// just sent, a rebuild, the player's arming at work (tapsChip's onItsWay).
const rackOnItsWay = s => sendTimer !== null || performance.now() - rackSentAt < RACK_SENT_GRACE_MS
    || !!s?.rebuilding || !!s?.arming?.busy;

/// The chain the status line, the rack's badges and the bar count with
/// (arming.js airOf): made ready before its first sound, a track plays
/// through the rack unless BIT-PERFECT or its converted file plays it.
function airNow(s) {
    const disk = !!s?.trackId && !!(listHooks.rendered?.() ?? []).find(r => r.id === s.trackId)?.path;
    return airOf(s, !bitPerfect && !disk);
}

/// Build the dimmed preview badges shown while the player is stopped.
/// Reflects what WILL play: every rack stage that is currently on in the
/// UI, plus the taps chip, all at opacity 0.4 with no pulse.
/// BIT-PERFECT mode shows nothing: no stage will play (its button says the mode).
function buildStoppedPreviewBadges() {
    const DIM = ' style="opacity:0.4"';
    if (bitPerfect) return '';
    const parts = [];
    for (const id of PLAYER_CHAIN) {
        if (!isRackOn(id)) continue;
        const feat = FEAT_BY_ID[id];
        parts.push(
            `<span class="cb" style="--h:var(--lab-${feat.hue});opacity:0.4"` +
            ` data-lab-tok="${esc(feat.tok)}" data-lab-name="${esc(feat.name)}"` +
            ` data-lab-hue="var(--lab-${feat.hue})">${esc(feat.tok)}</span>`
        );
    }
    // Taps chip — the slider's size, labelled as the playing chip is.
    const tapStr = tapsLabel(rackTaps());
    if (tapStr) {
        parts.push(`<span class="pl-tok pl-tap"${DIM}>${esc(tapStr)}</span>`);
    }
    return parts.join('');
}

/// Build the status-line badge HTML for a live source.
///
/// Three badge states, tracked against what the output thread is actually
/// hearing right now (audible.stages) vs. what the rack says:
/// - fired/skipped/failed: in audible.stages and in the rack (normal).
/// - pending: in the rack but not yet audible (dim + subtle pulse).
/// - leaving: audible but no longer in the rack (lit, fading out).
function buildLiveBadges(aud) {
    const parts = [];

    for (const { id, state: stageState, st, why } of stageStates(aud, chainStages(), isRackOn)) {
        const feat = FEAT_BY_ID[id];

        if (stageState === 'on') {
            const cls = st === 1 ? 'fired' : st === 3 ? 'failed' : 'skipped';
            const fallback = st === 1 ? 'Ran and changed the audio.'
                : st === 3 ? 'Ran and failed.'
                : 'Reached, and decided there was nothing to do.';
            parts.push(cbHtml(cls, feat.tok, feat.name, feat.hue, why || fallback));
        } else if (stageState === 'pending') {
            // On in the rack but not yet flowing through the output.
            const isHp = feat.tok === 'HP' || feat.tok === 'αHP';
            // Hybrid-Phase's own reason first: the first variant plays its
            // other stages already, and a pair on its way says why.
            const pendWhy = (aud.hpHeld && isHp)
                ? 'This track already stepped its taps down once so the CPU keeps up; Hybrid-Phase would need another step down, so it starts with the next track. Playing linear phase until then.'
                : (aud.hpDeferred && isHp)
                ? 'Hybrid-Phase envelope is being computed in the background — playing linear phase until ready.'
                : aud.quick
                ? 'The quick variant is playing (decode + DC only while the full render is prepared) — this stage switches in once the full render is ready.'
                : 'Switching in — not audible yet.';
            parts.push(cbHtml('pending', feat.tok, feat.name, feat.hue, pendWhy));
        } else {
            // Still audible from before the rack switch: leaving.
            // Keep the stage's own state class so a skipped stage stays grey
            // (never promoted to lit) and a fired stage stays lit.
            const ownCls = st === 1 ? 'fired' : st === 3 ? 'failed' : 'skipped';
            const fallback = st === 1 ? 'Ran and changed the audio.'
                : st === 3 ? 'Ran and failed.'
                : 'Nothing to do on this track.';
            const leavingWhy = (why || fallback) +
                ' · Switching out — will fade when the new render reaches the output.';
            parts.push(cbHtml(ownCls + ' leaving', feat.tok, feat.name, feat.hue, leavingWhy));
        }
    }

    // Taps chip (e.g. "30M"): neutral white-outline chip, not a DSP stage.
    // Switching (arming.js tapsChip: the rack's size not heard yet): the
    // outline flows, the digits breathe, the size is the one on its way.
    // Gains pl-tap-warn while a downgrade is unacknowledged; tooltip switches
    // to the rich format explaining why the player switched automatically.
    const chip = tapsChip(aud, rackTaps(), rackOnItsWay(status));
    if (chip?.switching) {
        parts.push(
            `<span class="pl-tok pl-tap pl-tap-switch" style="${animPhase(1500)}"` +
            ` title="${esc(`Switching to ${chip.label} taps — not audible yet.`)}">${esc(chip.label)}</span>`
        );
    } else if (chip) {
        const isWarn = aud.downgrade != null &&
            tapWarnAckKey !== `${status?.trackId}:${aud.downgrade.gen}`;
        if (isWarn) {
            const why = tapWarnText(aud.downgrade);
            parts.push(
                `<span class="pl-tok pl-tap pl-tap-warn" style="${animPhase(2400)}"` +
                ` data-lab-why="${esc(why)}"` +
                ` data-lab-tok="${esc(aud.taps)}"` +
                ` data-lab-name="Taps"` +
                ` data-lab-hue="rgba(255,255,255,0.82)">${esc(aud.taps)}</span>`
            );
        } else {
            parts.push(
                `<span class="pl-tok pl-tap" title="${esc(buildTapsTip(aud))}">${esc(aud.taps)}</span>`
            );
        }
    }

    return parts.join('');
}

/// The badges of a stream's chain: the stages it runs, as the chain says
/// (or as its tokens name them: the phase), and the stages of the rack it
/// cannot run, grey, with why. Nothing switches in or out on a stream: a
/// rack change tunes it in again. Before anything is heard, what will play.
function buildRadioBadges(aud, s) {
    if (!s?.audible) return buildStoppedPreviewBadges();
    const heard = new Map((aud.stages || []).map(([tok, st, why]) => [normTok(tok), [st, why]]));
    const tokens = new Set((s.chain || []).map(normTok));
    const parts = [];
    for (const { id, tok } of chainStages()) {
        const feat = FEAT_BY_ID[id];
        const h = heard.get(normTok(tok)) || (tokens.has(normTok(tok)) ? [1, ''] : null);
        if (h) {
            const [st, why] = h;
            const cls = st === 1 ? 'fired' : st === 3 ? 'failed' : 'skipped';
            parts.push(cbHtml(cls, feat.tok, feat.name, feat.hue, why || 'Ran and changed the audio.'));
        } else if (isRackOn(id)) {
            parts.push(cbHtml('skipped', feat.tok, feat.name, feat.hue, RADIO_TEXTS.notLive));
        }
    }
    if (aud.taps) parts.push(`<span class="pl-tok pl-tap" title="${esc(buildTapsTip(aud))}">${esc(aud.taps)}</span>`);
    return parts.join('');
}

/// Tooltip for the taps chip: filter size, ratio and phase kind.
function buildTapsTip(aud) {
    const parts = [];
    if (aud.taps) parts.push(`Filter size: ${aud.taps} taps.`);
    parts.push('Ratio: \xd78 (the player always renders through the polyphase path).');
    return parts.join(' ');
}

/// Expand the {from}/{to} placeholders in the taps-warn tooltip text.
function tapWarnText(downgrade) {
    if (!downgrade) return '';
    const tmpl = downgrade.reason === 'gpu-failed'
        ? GPU_CHIP_TEXTS.tapGpuFailed
        : GPU_CHIP_TEXTS.tapPower;
    return tmpl.replace('{from}', downgrade.from).replace('{to}', downgrade.to);
}

/// HTML for the GPU chip in the status line.
/// Present when aud.gpu is 'on' or 'failed'; absent when null.
/// Never removed mid-track after a failure — switches to pl-gpu-failed style.
function buildGpuChipHtml(aud) {
    const gpu = aud?.gpu;
    if (!gpu) return '';
    const failed = gpu === 'failed';
    const cls    = failed ? 'pl-tok pl-gpu pl-gpu-failed' : 'pl-tok pl-gpu';
    const tok    = 'GPU';
    const name   = failed ? 'GPU (failed)' : 'GPU renderer';
    const hue    = failed ? 'rgba(239,68,68,0.9)' : '#38bdf8';
    const why    = failed ? GPU_CHIP_TEXTS.failed : GPU_CHIP_TEXTS.active;
    return `<span class="${cls}" data-lab-why="${esc(why)}"` +
        ` data-lab-tok="${esc(tok)}" data-lab-name="${esc(name)}"` +
        ` data-lab-hue="${esc(hue)}">${esc(tok)}</span>`;
}

/// Compact metrics chips on the right side: level change and overs warning.
/// The buffer figure is shown separately in the time column (item H).
function buildMetrics(s) {
    if (!s) return '';
    const parts = [];
    const aud = s.audible;

    // Level change: the true-peak gain the chain applied (negative = quieter).
    if (aud && aud.source === 'live' && aud.gainDb < -0.05) {
        const tp = aud.tpDb != null ? aud.tpDb.toFixed(1) : null;
        const ceil = aud.ceilingDb != null ? aud.ceilingDb.toFixed(1) : null;
        let tip = '';
        if (tp != null && ceil != null) {
            tip = `Source true peak +${tp} dBTP brought to the ${ceil} dBTP ceiling, as the converter does.`;
        }
        parts.push(`<span class="pl-metric pl-metric-level" title="${esc(tip)}">level ${aud.gainDb.toFixed(1)} dB</span>`);
    }

    // Overs warning in BIT-PERFECT mode: the DAC path clips these, the rack
    // would have lowered the level to bring them in range instead.
    if (bitPerfect && s.srcOverPct > 0) {
        const peak = s.srcPeakDb != null ? '+' + s.srcPeakDb.toFixed(1) : '?';
        const tip = `The file has samples above full scale — BIT-PERFECT sends them unchanged, the DAC path clips them; the rack lowers the level instead.`;
        parts.push(`<span class="pl-metric pl-metric-warn" title="${esc(tip)}">⚠ overs ${peak} dBFS</span>`);
    }

    return parts.length ? `<span class="pl-metrics">${parts.join('')}</span>` : '';
}

/// Compact amber ≈ chip when the player cannot match the converter exactly.
/// One chip replaces the old full-sentence note so it never eats the status line.
function approxChipHtml() {
    const approx = approximations();
    if (!approx.length) return '';
    return `<span class="pl-approx-chip" title="${esc(approx.join('\n'))}">≈</span>`;
}

// ── rack arming / disarming (item C) ─────────────────────────────────
// When a DSP stage is switched on during playback but is not audible yet,
// the rack badge's coloured bar breathes (class pl-arming). When a stage is
// still audible after being switched off it dims (class pl-leaving). Both are
// removed once the stage transitions to its new steady state. The strip's
// pending badges pulse in the same rhythm via .cb.pending in player.css.
// A converted file or BIT-PERFECT switches nothing in (arming.js): the rack
// then rests, as when nothing plays. A disk file's name leaves out the
// stages its conversion declined, and their badges used to breathe for the
// whole track.

function updateRackArmingState(s) {
    const air = airNow(s);
    // A stream's chain does not follow the rack stage by stage: nothing breathes.
    const live = s?.state !== 'stopped' && !s?.radio && switchesLive(air);
    const byId = new Map(live ? stageStates(air, chainStages(), isRackOn).map(x => [x.id, x]) : []);

    for (const id of PLAYER_CHAIN) {
        const badge = document.getElementById(id)?.closest?.('.lab-badge');
        if (!badge) continue;

        if (!live) {
            badge.classList.remove('pl-arming', 'pl-leaving', 'pl-armed-fail');
            const t = failedFlashTimers.get(id);
            if (t) { clearTimeout(t); failedFlashTimers.delete(id); }
            continue;
        }

        const x = byId.get(id);
        badge.classList.toggle('pl-arming', x?.state === 'pending');
        badge.classList.toggle('pl-leaving', x?.state === 'leaving');

        // One-shot red flash when a stage just appeared as failed.
        if (x?.state === 'on' && x.st === 3
            && !badge.classList.contains('pl-armed-fail')
            && !failedFlashTimers.has(id)) {
            badge.classList.add('pl-armed-fail');
            failedFlashTimers.set(id, setTimeout(() => {
                badge.classList.remove('pl-armed-fail');
                failedFlashTimers.delete(id);
            }, 800));
        }
    }
}

// ── seeking state ─────────────────────────────────────────────────────

let seeking = false, seekPct = null;
let seekingPending = false;    // after release: waiting for the jump to resolve
let seekSending = null;        // { at, until }: a seek sent after a rack change, not yet the player's
let seekTargetS = null;        // seconds we sought to (for the "near enough" check)
let seekShimmerTimer = null;   // fallback clear in case the backend never confirms
let lastPosS = 0, lastDurS = 0;

function updateSeekDrag(pct) {
    const fill = $('plSeekFill');
    const thumb = $('plSeekThumb');
    const lbl = $('plSeekTimeLbl');
    if (fill) fill.style.width = pct * 100 + '%';
    if (thumb) { thumb.style.left = pct * 100 + '%'; thumb.style.display = ''; }
    if (lbl) {
        lbl.textContent = fmtTime(pct * lastDurS);
        // Pin to left edge a little so the label never overhangs the right.
        lbl.style.left = Math.max(0, Math.min(pct * 100, 88)) + '%';
        lbl.style.display = '';
    }
}

function hideSeekDragExtras() {
    const thumb = $('plSeekThumb'), lbl = $('plSeekTimeLbl');
    if (thumb) thumb.style.display = 'none';
    if (lbl) lbl.style.display = 'none';
}

// ── render ────────────────────────────────────────────────────────────

/// The status line is written on every poll; it is rebuilt only when what it
/// says changes. A rebuild under a pressed pointer loses the click (the taps
/// chip is clickable), and nodes that stay keep their animations running on
/// their own. The animation phases (animPhase) differ on every poll and are
/// left out of the comparison: they only matter when the nodes are new.
function setBadges(el, html) {
    const key = html.replace(/animation-delay:-[\d.]+s/g, '');
    if (el._badgesKey === key) return;
    el._badgesKey = key;
    el.innerHTML = html;
}

// ── the arming bar ────────────────────────────────────────────────────
// A 2 px line under the badges: how far the player is with bringing the
// rack onto the air (status.arming, arming.rs); when it shows and goes is
// arming.js's. From poll to poll only its style changes.

let armBar = null;

function renderArmBar(s) {
    const bar = $('plArm'), fill = $('plArmFill');
    if (!bar || !fill) return;
    const live = !!s && s.state !== 'stopped' && !bitPerfect;
    let waiting, arming;
    const r = live ? s.radio : null;
    if (r) {
        // A stream tuning in fills the bar with the stream its chain waits
        // for; its own filter or a rack change's new chain being made runs it
        // (no share known). It goes when the stream plays as asked (radioBar).
        arming = radioBar(r);
        waiting = !!arming;
    } else {
        // A stage or the taps on their way — with nothing heard yet, all of them
        // (airNow) — or the next track being made ready while the one before is
        // still heard.
        const air = live ? airNow(s) : null;
        waiting = live && (preparingBeforeSound(s)
            || waitsOnAir(stageStates(air, chainStages(), isRackOn), tapsChip(air, rackTaps(), rackOnItsWay(s))));
        arming = live ? s.arming : null;
        // A rack change waits RACK_SETTLE_MS before it goes out, and the status
        // takes a moment to show it: meanwhile the change is on its way, nothing
        // of it done yet.
        const queued = sendTimer !== null || performance.now() - rackSentAt < RACK_SENT_GRACE_MS;
        if (live && queued && !arming?.busy) {
            arming = { seq: `rack@${Math.round(rackSentAt)}`, frac: 0, busy: true, queued: true };
        }
    }
    armBar = barStep(armBar, { waiting, arming, now: performance.now() });
    const v = barView(armBar);
    if (!v.present) {
        if (bar.classList.contains('breathe')) {
            bar.classList.remove('on', 'breathe');
            fill.classList.add('jump');
            fill.style.transform = 'scaleX(0)';
            fill._f = '0.000';
        }
        return;
    }
    // Under the line's chips, the first to the last (the GPU chip left out:
    // it comes with the chain, and the bar's end must not move then); the
    // whole line while it has none.
    const chips = [...$('plStatusBadges').children].filter(e => e.matches('.cb, .pl-tok') && !e.matches('.pl-gpu'));
    const first = chips[0], last = chips[chips.length - 1];
    const left = first ? first.offsetLeft : 0;
    const width = first ? last.offsetLeft + last.offsetWidth - left : bar.parentElement.clientWidth;
    if (bar._geom !== `${left}:${width}`) {
        bar._geom = `${left}:${width}`;
        bar.style.left = left + 'px';
        bar.style.width = width + 'px';
    }
    if (!bar.classList.contains('breathe')) {
        // Breathing in step with the pending badges (animPhase).
        bar.style.setProperty('--arm-phase', `-${(performance.now() % 1500 / 1000).toFixed(3)}s`);
        bar.classList.add('breathe');
    }
    bar.classList.toggle('on', v.on);
    // No share known: the fill runs along the bar instead of growing.
    bar.classList.toggle('run', !!arming?.run && waiting);
    // A new arming starts where it is, not with a glide back.
    fill.classList.toggle('jump', v.reset);
    const f = v.frac.toFixed(3);
    if (fill._f !== f) {
        fill._f = f;
        fill.style.transform = `scaleX(${f})`;
    }
}


function render() {
    const s = status;
    radioRender(s);
    const st = s?.state || 'stopped';
    const e = s?.trackId != null ? entryOfTrack(s.trackId) : null;
    const info = e?.info;
    // A stream (radio.js): its lines instead of a track's.
    const rl = s?.radio ? radioLines(s) : null;

    // Play/pause icon — swap SVG content in place so focus and hover state
    // on the button itself are not disturbed.
    const plPlay = $('plPlay');
    if (plPlay) {
        const playing = st === 'playing';
        // Only when the state changes: rebuilding the icon on every poll put
        // a new <svg> under a pointer that was pressed on the old one, and
        // the click never came.
        if (plPlay.dataset.playing !== String(playing)) {
            plPlay.dataset.playing = String(playing);
            plPlay.innerHTML = playing ? SVG_PAUSE : SVG_PLAY;
            plPlay.title = playing ? 'Pause (Space)' : 'Play (Space)';
        }
    }

    $('plTitle').textContent = rl ? rl.title : e ? (info?.title || e.name) : 'Nothing playing';
    let sub = '';
    if (rl) {
        // A stream: the station and what it plays at; the showcase, the
        // station and the album line its own list gives.
        sub = showcaseMode && st !== 'stopped'
            ? [rl.station, rl.album].filter(Boolean).join(' · ')
            : [rl.station, rl.tech].filter(Boolean).join(' · ');
        if (!showcaseMode && s.device && st !== 'stopped') sub = `${sub} · ${deviceShort(s.device)}`;
    } else if (showcaseMode && e && st !== 'stopped') {
        // Showcase sub: "Artist · Album · Year" from tags, file name as fallback.
        const parts = [];
        if (info?.artist) parts.push(info.artist);
        if (info?.album)  parts.push(info.album);
        if (info?.year)   parts.push(String(info.year));
        sub = parts.length ? parts.join(' · ') : e.name.replace(/\.[^.]+$/, '');
    } else if (e && s.outRate > 0 && st !== 'stopped') {
        const src = `${fmtRate(s.srcRate)}${s.bits ? '/' + s.bits : ''}`;
        sub = bitPerfect
            ? `bit-perfect · ${src} · ${s.exclusive ? 'exclusive' : 'shared'} ${s.format || ''}`
            : `${src} → ${fmtRate(s.outRate)} · ${s.exclusive ? 'exclusive' : 'shared'} ${s.format || ''}`;
        // The device it plays on (Anton 26.09): the short name here, the
        // strip is narrow; the whole one in the tooltip and the big player.
        if (s.device) sub = `${sub.trim()} · ${deviceShort(s.device)}`;
    } else if (e) {
        sub = [info?.artist, info?.album].filter(Boolean).join(' · ');
    } else {
        sub = '► on a row plays it through the rack above';
    }
    const subEl = $('plSub');
    subEl.textContent = sub;
    const subTip = s?.device && (e || rl) && st !== 'stopped' && !showcaseMode ? `Playing on ${s.device}` : '';
    // (tooltip.js turns titles into data-tip; written there directly)
    if ((subEl.dataset.tip || '') !== subTip) {
        if (subTip) subEl.dataset.tip = subTip; else delete subEl.dataset.tip;
    }
    playerViewHooks.afterSub?.(s);   // [listening mode]

    lastPosS = s?.positionS || 0;
    lastDurS = s?.durationS || info?.durationS || 0;
    // A stream has no length: the time it has been heard.
    $('plTime').textContent = rl ? (st === 'playing' || st === 'paused' ? fmtTime(lastPosS) : '')
        : e ? `${fmtTime(lastPosS)} / ${fmtTime(lastDurS)}` : '';

    // Buffer figure: small dim readout under the time counter (item H).
    // Format: "buf 1.2s" — no space before 's', no '· auto'.
    const bufEl = $('plBuf');
    if (bufEl) {
        if (s?.bufferS != null && (st === 'playing' || st === 'paused')) {
            const underText = s.underrunFrames ? ` · ${s.underrunFrames} underrun${s.underrunFrames !== 1 ? 's' : ''}` : '';
            const targetText = s.bufferTargetS != null ? ` Adaptive target: ${s.bufferTargetS.toFixed(1)} s.` : '';
            bufEl.title = `Buffer.${targetText}${underText} Adapts automatically.`;
            bufEl.textContent = `buf ${s.bufferS.toFixed(1)}s`;
            bufEl.style.display = '';
        } else {
            bufEl.textContent = '';
            bufEl.style.display = 'none';
        }
    }

    if (!seeking) {
        const pct = lastDurS > 0 ? Math.min(1, lastPosS / lastDurS) : 0;
        $('plSeekFill').style.width = pct * 100 + '%';
    }
    // Nothing to seek in a stream: the bar rests.
    $('plSeek').classList.toggle('pl-seek-live', !!rl);

    // Jump target marker: while status.jump.pending the playhead (fill) shows
    // the audible position (positionS, still advancing) and the shimmer shows
    // the target being loaded. A newer click moves the target.
    // F4: skip shimmer update while paused — a paused seek is applied at once
    // so hold=true only lasts ~100 ms (chain build), during which targetS
    // would show the old position (not the click target), moving the shimmer
    // away from where the user clicked.
    // Only a seek the listener made has a target to mark: a track start or
    // switch is pending too (target = the current position), and its marker
    // used to stay lit at the start of the bar for the whole track.
    // A seek sent after a rack change (finishSeek) is the player's once the
    // status shows it — pending at its target, or the track there (a seek
    // while paused is made at once). Until then the rack change's own work
    // is what the status says: the band stays where the listener clicked,
    // neither gone nor on the playhead.
    if (seekSending && (performance.now() > seekSending.until
        || (s?.jump?.pending && Math.abs((s.jump.targetS ?? NaN) - seekSending.at) < 0.05)
        || Math.abs((s?.positionS ?? NaN) - seekSending.at) < 0.3)) {
        seekSending = null;
    }
    if (seekingPending && s?.jump?.pending && lastDurS > 0 && s?.state !== 'paused') {
        const tgt = seekSending ? seekSending.at : s.jump.targetS;
        if (tgt != null) placeSeekBand(lastPosS / lastDurS, tgt / lastDurS);
    } else if (!s?.jump?.pending && !seekSending) {
        // Jump resolved (or none) — clear the marker.
        if (seekingPending) {
            seekingPending = false;
            seekTargetS = null;
            if (seekShimmerTimer) { clearTimeout(seekShimmerTimer); seekShimmerTimer = null; }
        }
        $('plSeekShimmer')?.classList.remove('active');
    }

    $('plBitPerfect').classList.toggle('on', bitPerfect);
    $('plVol').disabled = bitPerfect;
    $('plVolText').textContent = bitPerfect ? '—' : fmtDb(volDb());

    // Update arming/leaving animations on the rack badges every poll.
    updateRackArmingState(s);
    applyRackLock();

    // ── status line ───────────────────────────────────────────────────
    // The line is a flex row with two spans: plStatusBadges (the DSP badges,
    // taps chip, metrics and approx chip) and plStatusMsg (error / busy text
    // to the right of the badges).  The badge span is never cleared while the
    // player is running — only BIT-PERFECT, with no stage, says a quiet line
    // in its place while it plays (bitPerfectLine; a warning says more).
    // Error and busy messages appear in the message span, not in place of badges.

    const line = $('plStatus');
    const badgesEl = $('plStatusBadges');
    const msgEl = $('plStatusMsg');
    line.className = 'pl-status';

    // Message span: error or busy text goes here, to the right of the badges.
    // A stream's state and failures, in the page's words (radio.js).
    const rs = radioStateText(s);
    const err = [rs?.kind === 'error' ? rs.text : s?.error, uiError].filter(Boolean).join(' · ');
    if (err) {
        if (msgEl) { msgEl.className = 'pl-status-msg is-error'; msgEl.textContent = '⚠ ' + err; msgEl.title = err; }
    } else if (rs?.kind === 'busy') {
        if (msgEl) { msgEl.className = 'pl-status-msg is-busy'; msgEl.textContent = '⟳ ' + rs.text; msgEl.title = rs.text; }
    } else if (s?.pending && st !== 'stopped') {
        if (msgEl) { msgEl.className = 'pl-status-msg is-busy'; msgEl.textContent = '⟳ ' + s.pending.what; msgEl.title = s.pending.what; }
    } else {
        if (msgEl) { msgEl.className = 'pl-status-msg'; msgEl.textContent = ''; msgEl.title = ''; }
    }

    if (st === 'stopped' || !s) {
        // Stopped: show a dimmed preview of what WILL play so the strip height
        // never changes, plus the approx chip when relevant.
        const preview = buildStoppedPreviewBadges();
        const chip = approxChipHtml();
        if (chip) line.classList.add('is-approx');
        if (badgesEl) setBadges(badgesEl, preview + chip);
        renderArmBar(s);
        return;
    }

    // Playing or paused — build badges + metrics. Made ready before its first
    // sound, the track shows the rack's badges switching in (airNow).
    let html = '';
    const aud = airNow(s);

    if (aud) {
        if (aud.source === 'direct') {
            // BIT-PERFECT: no stage plays — the quiet line below says so.
        } else if (aud.source === 'file') {
            // Playing a converted file from disk: aud.file is the file the
            // chain on the air plays (the page's rendered map moves with the
            // rack at once, the chain a moment later — the map is only the
            // fallback).
            const resolvedPath = aud.file
                ?? (listHooks.rendered?.() ?? []).find(r => r.id === s.trackId)?.path ?? null;
            for (const [tok, stBadge, why] of diskChain(s.trackId, resolvedPath, aud.stages)) {
                const feat = Object.values(FEAT_BY_ID).find(f => normTok(f.tok) === normTok(tok));
                if (feat) {
                    const cls = stBadge === 1 ? 'fired' : stBadge === 3 ? 'failed' : 'skipped';
                    html += cbHtml(cls, feat.tok, feat.name, feat.hue, why || '');
                }
            }
            // Neutral DISK chip: no DSP-stage hue, just a label.
            const fileName = resolvedPath ? resolvedPath.split(/[/\\]/).pop() : '';
            const diskTip = 'Playing the converted file from disk — no processing.'
                + (fileName ? ' File: ' + fileName : '');
            html += `<span class="pl-tok pl-disk" data-lab-why="${esc(diskTip)}"` +
                ` data-lab-tok="DISK" data-lab-name="Disk playback">DISK</span>`;
        } else if (rl) {
            // A stream's chain: what it runs, and what it cannot.
            html += buildRadioBadges(aud, s);
            html += buildGpuChipHtml(aud);
        } else {
            // Live: badges from audible.stages with pending/leaving transitions.
            html += buildLiveBadges(aud);
            // GPU chip: between taps and metrics (absent when aud.gpu is null).
            html += buildGpuChipHtml(aud);
        }
    }

    // M chip: shown when an M track is playing live (not BIT-PERFECT, not DISK).
    // Suppressed when source === 'direct' (handled by the BIT-PERFECT branch above).
    const mTrackNow = isNowMTrack();
    if (mTrackNow && aud && aud.source !== 'direct' && aud.source !== 'file') {
        html = `<span class="pl-tok pl-mem-chip" data-lab-why="Track memory is active — rack changes go into this track's memory" data-lab-tok="M" data-lab-name="Track memory">M</span>` + html;
    }

    const metrics = buildMetrics(s);
    const bpLine = bitPerfectLine(s, aud, !!err || metrics.includes('pl-metric-warn'));
    if (bpLine) html += `<span class="pl-bp-line" title="${esc(bpLine)}">${esc(bpLine)}</span>`;
    html += metrics;
    html += approxChipHtml();
    if (badgesEl) setBadges(badgesEl, html);
    else line.innerHTML = html;
    renderArmBar(s);

    // Rack tint: warm border on labField while an M track plays live.
    $('labField')?.classList.toggle('lab-m-live', mTrackNow && aud?.source !== 'direct');
}

// ── spectrum ──────────────────────────────────────────────────────────
// A canvas laid behind the .pl-top area (title, time, transport buttons)
// draws smooth spectrum bars in real time. The bars are deliberately low-
// contrast so they never compete with the text sitting above them.

const SPEC_BANDS = 48;
const SPEC_FRAME_MS = 1000 / 60;   // 60 Hz fetch cadence — halves gate jitter

let specBands = new Float32Array(SPEC_BANDS);
const SPEC_PEAK_DECAY = 0.955;   // half-life ≈ 15 frames = 0.5 s at 30 Hz
let specPeak = new Float32Array(SPEC_BANDS);
let specRunning = false;
let specFetchPending = false;
let specLastFrameMs = 0;
let oceanT0 = null;        // when the idle swell began (listening mode, nothing playing)

/// The bars' levels as drawn (0…1 per band; the idle sea while nothing
/// plays in listening mode): the big player's waves ride on them.
export const spectrumLevels = () => specBands;
export const playerPlaying = () => status?.state === 'playing';
export const playerState = () => status?.state ?? 'stopped';
export const playerStatus = () => status;
/// The listener's own delay for this device (the bars' delay setting), ms.
export const spectrumDelay = () => specDelayMs();

function initSpectrum() {
    const canvas = $('plSpectrum');
    const top = $('plTop');
    if (!canvas || !top) return;

    // Size the canvas once the layout has settled, then track resizes.
    const sizeCanvas = () => {
        canvas.width = top.clientWidth || 420;
        canvas.height = top.clientHeight || 40;
    };
    requestAnimationFrame(sizeCanvas);
    try { new ResizeObserver(sizeCanvas).observe(top); } catch (_) { /* older engine */ }

    // prefers-reduced-motion: leave the canvas blank, do not animate.
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;

    specRunning = true;
    requestAnimationFrame(specStep);
}

/// The big player's bars switched off (listening.js): nothing to ask for or draw.
const barsOff = () => document.body.classList.contains('mode-listening') && !!$('plBar')?.classList.contains('lm-bars-off');

function specStep(ts) {
    if (!specRunning) return;
    requestAnimationFrame(specStep);

    if (barsOff()) {
        setSea(false);
        oceanT0 = null;
        seaRise = 0;
        if (specBands.some(v => v > 0) || specPeak.some(v => v > 0)) { specBands.fill(0); specPeak.fill(0); clearSpectrum(); }
        return;
    }

    const playing = status?.state === 'playing';
    const sea = !playing && !document.hidden && document.body.classList.contains('mode-listening');
    setSea(sea);

    // Nothing plays in listening mode: the bars breathe like a sea (Anton
    // 26.09) — they settle first, then the swell rises slowly over ~4 s and
    // rolls on in three waves of their own length and speed, its height
    // itself drifting; crests rounded over the neighbouring bands. The peak
    // caps fall away and are not raised again. The studio's small player
    // does not breathe (Anton 26.09); a hidden window draws nothing.
    if (sea) {
        if (ts - specLastFrameMs < 33) return;   // 30 frames a second is plenty for a swell
        specLastFrameMs = ts;
        if (oceanT0 == null) oceanT0 = ts;
        const t = ts / 1000;
        const u = Math.max(0, Math.min(1, ((ts - oceanT0) / 1000 - 0.8) / 4));
        const rise = u * u * (3 - 2 * u);                       // smoothstep
        const drift = 0.82 + 0.18 * Math.sin(2 * Math.PI * t / 17);
        const n = specBands.length;
        const wave = new Float32Array(n);
        for (let i = 0; i < n; i++) {
            const x = i / Math.max(1, n - 1);
            const w1 = Math.sin(2 * Math.PI * (x * 0.9 - t / 9));      // the long swell
            const w2 = Math.sin(2 * Math.PI * (x * 1.7 + t / 13));     // a slower counter-swell
            const w3 = Math.sin(2 * Math.PI * (x * 3.3 - t / 5.5));    // a little chop
            wave[i] = 0.5 + 0.5 * (0.55 * w1 + 0.3 * w2 + 0.15 * w3);
        }
        for (let i = 0; i < n; i++) {
            const a = wave[Math.max(0, i - 1)], b = wave[i], c = wave[Math.min(n - 1, i + 1)];
            const h = (a + 2 * b + c) / 4;                          // rounded crests
            const target = 0.06 + rise * drift * (0.06 + 0.34 * h);
            specBands[i] = specBands[i] * 0.93 + target * 0.07;
            specPeak[i] *= SPEC_PEAK_DECAY;
        }
        seaRise = rise;
        drawSpectrum();
        return;
    }
    oceanT0 = null;
    seaRise = 0;
    // Hidden while not playing: decay toward silence and stop drawing once quiet.
    if (!playing || document.hidden) {
        let any = false;
        for (let i = 0; i < specBands.length; i++) {
            specBands[i] *= 0.87;
            specPeak[i] *= SPEC_PEAK_DECAY;
            if (specBands[i] > 0.003) any = true;
        }
        if (any) drawSpectrum();
        else clearSpectrum();
        return;
    }

    // The protocol is down and its pause is not over: the bars fall, and
    // nothing is asked until the pause ends (see proto above).
    if (!protoDue(proto.spectrum)) {
        for (let i = 0; i < specBands.length; i++) {
            specBands[i] *= 0.87;
            specPeak[i] *= SPEC_PEAK_DECAY;
        }
    }

    // Fetch new band data at ≤30 Hz without stacking up pending requests.
    if (ts - specLastFrameMs >= SPEC_FRAME_MS && !specFetchPending && protoDue(proto.spectrum)) {
        specLastFrameMs = ts;
        specFetchPending = true;
        // ahead_ms: JS pipeline lead (rAF + IPC round-trip, ~20 ms), less
        // the listener's delay for this device. The backend spectrum()
        // already subtracts the measured device latency from the read
        // position; adding latencyMs here would double-count it.
        const aheadMs = 20 - specDelayMs();
        fetch(`https://aura.localhost/player/spectrum?bands=${SPEC_BANDS}&ahead_ms=${aheadMs}`)
            .then(r => {
                if (!r.ok) throw new Error('HTTP ' + r.status);
                protoOk(proto.spectrum);
                return r.arrayBuffer();
            })
            .then(buf => {
                specFetchPending = false;
                const raw = buf && buf.byteLength > 0 ? new Uint8Array(buf) : null;
                if (!raw) {
                    // Empty body means nothing is sounding right now: decay.
                    for (let i = 0; i < specBands.length; i++) {
                        specBands[i] *= 0.87;
                        specPeak[i] *= SPEC_PEAK_DECAY;
                    }
                    return;
                }
                // Fast attack (~3 frames), faster release (~2 frames) so transients
                // read clearly without bars smearing across beats.
                for (let i = 0; i < Math.min(raw.length, specBands.length); i++) {
                    const v = raw[i] / 255;
                    // Instant attack (bars snap to peaks on the first frame after
                    // the transient arrives), slow release only.
                    specBands[i] = v > specBands[i]
                        ? v
                        : specBands[i] * 0.72 + v * 0.28;
                    if (specBands[i] > specPeak[i]) specPeak[i] = specBands[i];
                    else specPeak[i] *= SPEC_PEAK_DECAY;
                }
            })
            .catch(() => { specFetchPending = false; protoFailed(proto.spectrum); });
    }

    drawSpectrum();
}

/// The sea is drawn as faintly as the music's bars, but the band under it
/// is at full strength while it rolls (listening mode, nothing playing):
/// under the controls' 0.35 it was a few per cent of a colour on the dark
/// ground, and Anton saw no sea at all (26.09).
let seaOn = false;
let seaRise = 0;            // how far the swell has come up (0…1): its colour floor
function setSea(on) {
    if (on === seaOn) return;
    seaOn = on;
    $('plTop')?.classList.toggle('pl-sea', on);
}

function clearSpectrum() {
    const canvas = $('plSpectrum');
    if (!canvas) return;
    canvas.getContext('2d').clearRect(0, 0, canvas.width, canvas.height);
}

function drawSpectrum() {
    const canvas = $('plSpectrum');
    if (!canvas || !canvas.width || !canvas.height) return;
    const ctx = canvas.getContext('2d');
    const W = canvas.width, H = canvas.height;
    ctx.clearRect(0, 0, W, H);

    const n = specBands.length;
    const barW = W / n;

    for (let i = 0; i < n; i++) {
        const v = specBands[i];
        if (v < 0.003) continue;
        // Colour ramp: violet (left) → cyan (right).
        const t = i / (n - 1);
        const r = Math.round(124 + t * (56 - 124));
        const g = Math.round(58 + t * (189 - 58));
        const b = Math.round(237 + t * (248 - 237));
        // Vertical gradient: fade to 35% of base opacity at the top so the
        // title text above stays readable even on loud passages.
        // Base alpha is 0.85 so CSS wrapper opacity (0.35 control / 1.0 showcase)
        // yields effective alpha ≈ v*0.30 in control mode and v*0.85 in showcase.
        // The sea gains a floor of colour as it rises, so a low swell still
        // reads on the dark ground (the music's bars are not touched).
        const alpha = Math.min(0.8, v * 0.85 + seaRise * (0.22 + 0.1 * v));
        const barTop = H - v * H;
        const grad = ctx.createLinearGradient(0, barTop, 0, H);
        grad.addColorStop(0, `rgba(${r},${g},${b},${(alpha * 0.35).toFixed(3)})`);
        grad.addColorStop(1, `rgba(${r},${g},${b},${alpha.toFixed(3)})`);
        ctx.fillStyle = grad;
        ctx.fillRect(i * barW, barTop, Math.max(1, barW - 1), v * H);

        // Peak cap: 2 px bright line that holds for ~0.5 s then falls.
        if (specPeak[i] > 0.04) {
            ctx.fillStyle = `rgba(${r},${g},${b},0.88)`;
            const py = Math.round(H - specPeak[i] * H);
            ctx.fillRect(i * barW, py, Math.max(1, barW - 1), 2);
        }
    }
}

// ── polling ───────────────────────────────────────────────────────────

async function poll() {
    let next = null;
    if (protoDue(proto.status)) {
        try {
            const r = await fetch('https://aura.localhost/player/status');
            if (!r.ok) throw new Error('HTTP ' + r.status);
            next = await r.json();
            protoOk(proto.status);
        } catch (_) {
            protoFailed(proto.status);
        }
    }
    if (!next) {
        // The protocol is down: the status through invoke, once a second at
        // most, until the protocol answers again.
        const t = performance.now();
        if (t - lastStatusInvoke < INVOKE_STATUS_MS) return;
        lastStatusInvoke = t;
        try { next = await invoke('player_status'); } catch (_) { return; }
    }

    status = next;
    // A stream: ▶ and ■ only, here, in the big player and on the full
    // screen (player.css body.pl-stream); a file: all five; stopped, as
    // what ▶ would start.
    document.body.classList.toggle('pl-stream', !fileTransport(status, transportContext()));
    checkSwitch(status);
    // A stream has no cover (and no trackId): the track's before it goes.
    if (status.radio && !radioArtGone) updateArt(null);
    radioArtGone = !!status.radio;
    // Reset the intent once the backend confirms it, or after it expires.
    // If we intended to pause but the backend is still playing (it ignored the
    // pause because the track was preparing), resend while the intent is fresh.
    // Confirmed means the same track too: the old one plays on while the new
    // one is prepared.
    if (intent) {
        const elsewhere = status.state !== intent.from && status.state !== 'preparing' && status.state !== intent.state;
        const confirmed = status.state === intent.state && (intent.id == null || status.trackId === intent.id);
        if (confirmed || performance.now() - intent.at >= INTENT_MS || elsewhere) {
            const lapsed = intent;
            intent = null;
            refreshNowRows(lapsed.id, now.id, status.trackId ?? null);
        } else if (intent.state === 'paused' && status.state === 'playing') {
            invoke('player_pause').catch(showUiError);
        }
    }
    syncDeviceSelect(status);
    const cur = { id: status.trackId ?? null, state: status.state || 'stopped' };
    // A press that held the controls and started nothing lets them go.
    if (held && cur.state === 'stopped' && now.state === 'stopped'
        && performance.now() - heldAt > HOLD_GRACE_MS) setHeld(false);
    if (cur.id !== now.id || cur.state !== now.state) {
        const ids = new Set();
        const was = now.id != null ? entryOfTrack(now.id) : null;
        const is = cur.id != null ? entryOfTrack(cur.id) : null;
        // The held controls follow the state, including after a page reload
        // where the backend is already playing (nothing is held at first,
        // so the first poll that sees a track on must hold them).
        if (cur.state === 'stopped') {
            setHeld(false);
            // Track stopped: restore base rack if one is held.
            if (getBaseRack()) {
                restoreAndClearBaseRack();
                sendNow();
            }
            mLiveId = null;
            leaveShowcase();
        } else if (cur.state === 'paused') {
            setHeld(true);
            leaveShowcase();
        } else if (cur.state === 'playing' || cur.state === 'preparing') {
            setHeld(true);
            // A new 'playing' state or a new track: (re-)schedule showcase.
            if (now.state !== 'playing' || cur.id !== now.id) maybeScheduleShowcase();
        }
        // Track changed (gapless advance or manual switch to another track).
        if (cur.id !== now.id && cur.id != null) {
            // Clear the taps-warn acknowledge key so the next track starts fresh.
            tapWarnAckKey = null;
            updateArt(cur.id);
            const newEntry = entryOfTrack(cur.id);
            if (newEntry && !bitPerfect && isMActive(newEntry)) {
                // Gapless advance to an M track: apply its settings to DOM.
                const snap = getMSettings(newEntry);
                if (snap) {
                    saveBaseRackIfAbsent();
                    applyMSnapshot(snap);
                    // At the junction the player made its own copy of this
                    // memory its rack: the copy goes again, and the rack
                    // goes even when the page sent the same before — else an
                    // older copy could play on under the rack shown, its
                    // taps chip switching for good (review n6).
                    pushTrackSettings(mKey(newEntry.name));
                    lastSent = '';
                    sendNow();
                    mLiveId = cur.id;
                }
            } else if (cur.id !== mLiveId) {
                // Gapless advance to a non-M track: restore base rack.
                if (getBaseRack()) {
                    restoreAndClearBaseRack();
                    sendNow();
                }
                mLiveId = null;
            }
        }
        now = cur;
        if (was) ids.add(was.id);
        if (is) ids.add(is.id);
        refreshRows(ids);
    }
    // Playing and the controls up with no countdown: look again, every poll
    // — whatever held them (a tooltip, a stale "over") may be gone now.
    if (status.state === 'playing' && !showcaseMode && !showcaseTimer && !scPressed) {
        refreshShowcaseInputs();
        maybeScheduleShowcase();
    }
    render();
}

// ── device ────────────────────────────────────────────────────────────
// Chosen here and remembered by id. "System default" is an entry of its own
// that names the device it stands for right now: Windows moves the default on
// its own (a monitor waking up takes it over), and a list that only
// pre-selected the default showed one device while the player opened another.

let deviceWired = false;
// Rebuild the device option list.  On startup (initial=true) the stored
// preference is also applied to the backend; on focus re-entry it is not:
// the backend's device is already set (by the user's last explicit choice or
// by a prior startup), and re-sending the select value would overwrite a
// device the backend is using that the select has not yet reflected.
async function loadDevices(initial = false) {
    const sel = $('plDevice');
    let devs;
    try {
        devs = await invoke('player_devices');
    } catch (e) {
        showUiError('Output devices could not be listed: ' + e);
        return;
    }
    const want = sel.options.length ? sel.value : load(KEY_DEVICE, '');
    const def = devs.find(d => d.isDefault);
    sel.innerHTML = '';
    const sys = document.createElement('option');
    sys.value = '';
    sys.textContent = 'System default' + (def ? ` — ${def.name}` : '');
    sel.appendChild(sys);
    for (const d of devs) {
        const o = document.createElement('option');
        o.value = d.id;
        o.textContent = d.name;
        sel.appendChild(o);
    }
    // A remembered device that is not plugged in stays listed, marked, so the
    // choice is not silently swapped for another device.
    if (want && !devs.some(d => d.id === want)) {
        const o = document.createElement('option');
        o.value = want;
        o.textContent = 'Not connected — last chosen device';
        sel.appendChild(o);
    }
    sel.value = want;
    setDeviceTip(sel);
    // Apply the stored preference to the backend only on startup.  Focus
    // re-entries must not resend it: the select may lag behind the backend
    // (status polling catches up shortly), so resending would overwrite the
    // device the backend is actually using.
    if (initial) {
        await invoke('player_set_device', { id: want || null }).catch(e => showUiError('Output device: ' + e));
    }

    if (deviceWired) return;
    deviceWired = true;
    sel.addEventListener('change', () => {
        store(KEY_DEVICE, sel.value);
        setDeviceTip(sel);
        invoke('player_set_device', { id: sel.value || null }).catch(e => showUiError('Output device: ' + e));
    });
    // Devices come and go while the window is open; re-read on return.
    window.addEventListener('focus', () => { if (document.activeElement !== sel) loadDevices(false); });
}

/// The device button shows no text (Anton 26.09): what it plays on is in
/// its tooltip. (tooltip.js turns titles into data-tip; written directly.)
function setDeviceTip(sel) {
    const name = sel.selectedOptions[0]?.textContent || 'System default';
    sel.dataset.tip = `Output device: ${name}`;
}

/// A Windows endpoint is named "<endpoint> (<adapter>)" — "Наушники
/// (Realtek USB2.0 Audio)", "Динамики (Mojo 2)": the adapter tells the
/// devices apart, the endpoint word rarely does. Anything else as it is.
export function deviceShort(name) {
    const m = /^.*?\((.+)\)\s*$/.exec(name || '');
    return m ? m[1] : (name || '');
}

// Keep the device select in sync with the device the backend is actually
// using (from the status poll).  Only updates the DOM — never calls
// player_set_device.  Skipped while the user has the select open.
function syncDeviceSelect(st) {
    if (!st || !st.device) return;
    const sel = $('plDevice');
    if (!sel || !sel.options.length) return;
    if (document.activeElement === sel) return;
    // If the currently selected option already names this device, leave it.
    const cur = sel.selectedOptions[0];
    if (cur && cur.textContent.includes(st.device)) return;
    // Otherwise find the explicit option whose text matches the backend device.
    for (const opt of sel.options) {
        if (opt.value && opt.textContent === st.device) {
            sel.value = opt.value;
            setDeviceTip(sel);
            return;
        }
    }
}

// ── wiring ────────────────────────────────────────────────────────────

function wire() {
    // ── showcase: pointer + focus tracking on the whole strip ─────────
    const plBar = $('plBar');
    // The showcase only fades the controls; they keep taking the pointer. A
    // press anywhere on the player ends the showcase at once and goes through
    // to what it landed on. Coming back to the window with the pointer
    // already over the player sends no mouseenter until it moves: without
    // this the first press there (Play, after Alt+Tab) was lost.
    plBar.addEventListener('pointerdown', () => {
        scPointerOver = true;
        leaveShowcase();
    }, true);
    window.addEventListener('pointerdown', () => { scPressed = true; }, true);
    for (const t of ['pointerup', 'pointercancel', 'blur']) window.addEventListener(t, () => { scPressed = false; }, true);
    window.addEventListener('focus', () => {
        if (!plBar.matches(':hover')) return;
        scPointerOver = true;
        leaveShowcase();
    });
    plBar.addEventListener('mouseenter', () => {
        scPointerOver = true;
        leaveShowcase();
    });
    plBar.addEventListener('mouseleave', () => {
        scPointerOver = false;
        // Pointer leaving is sufficient to start the showcase countdown.
        // Resetting scFocusInside here means a button that was clicked (and
        // gained focus) no longer blocks the timer after the mouse has left —
        // the showcase trigger is pointer-based, not focus-based.
        scFocusInside = false;
        // Also blur the focused element to remove its visual focus indicator
        // and prevent it from keeping scFocusInside true on a future focusin.
        // Exemptions: the device select (may have an open dropdown) and an
        // active seek drag (pointer capture still in progress).
        const ae = document.activeElement;
        if (ae && plBar.contains(ae) && ae !== $('plDevice') && !seeking) {
            ae.blur();
        }
        maybeScheduleShowcase();
    });
    plBar.addEventListener('focusin', () => {
        scFocusInside = true;
        leaveShowcase();
    });
    plBar.addEventListener('focusout', ev => {
        if (!plBar.contains(ev.relatedTarget)) {
            scFocusInside = false;
            maybeScheduleShowcase();
        }
    });
    // Rack: pointer over or focus inside blocks showcase (user is mid-interaction).
    const labField = $('labField');
    if (labField) {
        labField.addEventListener('mouseenter', () => {
            scRackActive = true;
            leaveShowcase();
        });
        labField.addEventListener('mouseleave', () => {
            scRackActive = false;
            maybeScheduleShowcase();
        });
        labField.addEventListener('focusin', () => {
            scRackActive = true;
            leaveShowcase();
        });
        labField.addEventListener('focusout', ev => {
            if (!labField.contains(ev.relatedTarget)) {
                scRackActive = false;
                maybeScheduleShowcase();
            }
        });
    }
    // Device select: open = block showcase; close = re-check.
    $('plDevice').addEventListener('focus', () => leaveShowcase());
    $('plDevice').addEventListener('blur',  () => maybeScheduleShowcase());

    // Taps-warn click: acknowledge the blinking taps chip after a GPU→CPU downgrade.
    // The handler is on plStatusBadges (display:contents) — delegation reaches the chip.
    // stopPropagation is NOT used; the tooltip's global click→hide handler fires normally.
    // On pointerdown, not click: the chip sits in a line the poll may rebuild
    // between press and release.
    $('plStatusBadges').addEventListener('pointerdown', ev => {
        if (ev.button !== 0) return;
        const chip = ev.target.closest('.pl-tap.pl-tap-warn');
        if (!chip || !status?.trackId) return;
        const aud = status?.audible;
        if (aud?.downgrade?.gen != null) {
            tapWarnAckKey = `${status.trackId}:${aud.downgrade.gen}`;
            // Visual feedback before the next poll redraws the badge.
            chip.classList.remove('pl-tap-warn');
        }
    });

    $('plPlay').addEventListener('click', () => { leaveShowcase(); onPlay(null); });
    $('plStop').addEventListener('click', stopPlayer);
    $('plPrev').addEventListener('click', () => {
        // A stream has no list to step through (the button is not shown;
        // the full screen's ⏮ presses it all the same).
        if (!fileTransport(status, transportContext())) return;
        endSwitch();
        leaveShowcase();
        setHeld(true);
        flushRack().then(() => invoke('player_prev')).catch(showUiError);
    });
    $('plNext').addEventListener('click', () => {
        if (!fileTransport(status, transportContext())) return;
        endSwitch();
        leaveShowcase();
        setHeld(true);
        flushRack().then(() => invoke('player_next')).catch(showUiError);
    });

    // Repeat button: cycles off → all → one → off and persists the choice.
    $('plRepeat').addEventListener('click', (e) => {
        // The files' repeat: a stream leaves it as it was.
        if (!fileTransport(status, transportContext())) return;
        repeatMode = repeatMode === 'off' ? 'all' : repeatMode === 'all' ? 'one' : 'off';
        store(KEY_REPEAT, repeatMode);
        updateRepeatBtn();
        // The pointer is still on the button: redraw the tooltip for the new
        // mode through the same handler that drew it.
        if (e.currentTarget.matches(':hover')) {
            e.currentTarget.dispatchEvent(new MouseEvent('mouseover',
                { bubbles: true, clientX: e.clientX, clientY: e.clientY }));
        }
        invoke('player_set_repeat', { mode: repeatMode }).catch(e => showUiError('Repeat: ' + e));
    });

    // Seeking: pointer events with capture so the drag works even when the
    // pointer leaves the bar. While dragging the fill and thumb follow the
    // pointer; on release a shimmer at the target position stays until the
    // backend confirms the jump is audible (item 9 / J2).
    const bar = $('plSeek');
    const pctAt = ev => {
        const r = bar.getBoundingClientRect();
        return Math.max(0, Math.min(1, (ev.clientX - r.left) / r.width));
    };
    // Hover: the time a click would go to, over the pointer (the same pctAt
    // as the click). Only a transform moves, once a frame.
    const hover = $('plSeekHover'), hoverT = $('plSeekHoverT');
    // hoverPct: the pointer's share of the bar as pctAt measures it (on the
    // screen: listening mode scales the bar); the offsets below are in the
    // bar's own pixels.
    let hoverPct = null, hoverRaf = 0, hoverText = '', hoverHalf = 0;
    const drawHover = () => {
        hoverRaf = 0;
        if (hoverPct == null || seeking || lastDurS <= 0) { hover.style.display = 'none'; return; }
        const w = bar.clientWidth;
        const pct = hoverPct;
        const t = fmtTime(pct * lastDurS);
        hover.style.display = 'block';
        if (t !== hoverText) { hoverT.textContent = t; hoverText = t; hoverHalf = hoverT.offsetWidth / 2; }
        const x = pct * w;
        // The label stays over the bar at its ends; the hairline stays at the pointer.
        const dx = Math.max(hoverHalf, Math.min(w - hoverHalf, x)) - x;
        hover.style.transform = `translateX(${x}px)`;
        hoverT.style.transform = `translateX(calc(-50% + ${dx}px))`;
    };
    const moveHover = ev => {
        hoverPct = pctAt(ev);
        if (!hoverRaf) hoverRaf = requestAnimationFrame(drawHover);
    };
    bar.addEventListener('pointerenter', moveHover);
    bar.addEventListener('pointerleave', () => { hoverPct = null; if (!hoverRaf) hoverRaf = requestAnimationFrame(drawHover); });
    bar.addEventListener('pointerdown', ev => {
        if (lastDurS <= 0 || ev.button !== 0) return;
        leaveShowcase();
        bar.setPointerCapture(ev.pointerId);
        seeking = true;
        seekPct = pctAt(ev);
        updateSeekDrag(seekPct);
        hover.style.display = 'none';
    });
    bar.addEventListener('pointermove', ev => {
        if (!seeking) { moveHover(ev); return; }
        seekPct = pctAt(ev);
        updateSeekDrag(seekPct);
    });
    const finishSeek = () => {
        if (!seeking) return;
        seeking = false;
        hideSeekDragExtras();
        maybeScheduleShowcase();
        if (seekPct != null && lastDurS > 0) {
            seekTargetS = seekPct * lastDurS;
            seekingPending = true;
            // The fill stays at positionS (still audible from old place) — do not
            // snap it to the target. render() advances positionS each poll.
            // Show the target marker at the clicked position; render() will track
            // status.jump.targetS while pending (newer clicks move the marker).
            placeSeekBand(lastPosS / lastDurS, seekPct);
            // Safety timeout: clear the marker after 5 s if the backend never
            // confirms the jump.
            if (seekShimmerTimer) clearTimeout(seekShimmerTimer);
            seekShimmerTimer = setTimeout(() => {
                seekingPending = false;
                seekTargetS = null;
                seekShimmerTimer = null;
                $('plSeekShimmer')?.classList.remove('active');
            }, 5000);
            // A rack change still settling goes out first, as for Play and
            // ⏭: the seek is made with the rack the page shows, and the new
            // chain is heard from the seek's point. Sent after the seek, it
            // went on from where the track had been (Anton 1.10: new taps and
            // a quick seek back — the old taps played on).
            // Its band holds until the player shows the seek (render()).
            const at = seekTargetS;
            seekSending = { at, until: performance.now() + 5000 };
            flushRack()
                .then(() => invoke('player_seek', { seconds: at }))
                .catch(e => { seekSending = null; showUiError(e); });
        }
        seekPct = null;
    };
    bar.addEventListener('pointerup', finishSeek);
    bar.addEventListener('pointercancel', finishSeek);

    const vol = $('plVol');
    vol.value = String(volPos(parseFloat(load(KEY_VOLUME, '0')) || 0));
    vol.addEventListener('input', () => {
        const db = volDb();
        $('plVolText').textContent = fmtDb(db);
        store(KEY_VOLUME, db);
        showVolTip(true);
        invoke('player_set_volume', { db }).catch(() => {});
    });
    // The level shows over the thumb while it is held or moved (the big
    // player has no dB text beside the slider).
    vol.addEventListener('pointerdown', () => { volHeld = true; showVolTip(true); });
    const letGo = () => { volHeld = false; showVolTip(false); };
    vol.addEventListener('pointerup', letGo);
    vol.addEventListener('pointercancel', letGo);
    vol.addEventListener('blur', letGo);

    $('plBitPerfect').addEventListener('click', () => {
        bitPerfect = !bitPerfect;
        store(KEY_BITPERFECT, bitPerfect ? '1' : '0');
        if (!bitPerfect) takeMemoryOfPlaying();
        sendNow();
        render();
    });

    // Every control of the rack and the sliders above it reaches the player.
    const panel = $('converterPanel');
    const fromRack = ev => !ev.target.closest('#plBar, #plListHead, #fileQueue');
    panel.addEventListener('change', ev => {
        if (!fromRack(ev)) return;
        // While an M track plays, intercept rack changes: save the new state
        // into that track's M snapshot rather than the base rack.
        if (mLiveId != null) {
            const e = entryOfTrack(mLiveId);
            if (e && isMActive(e)) {
                const k = mKey(e.name);
                const rec = getM(k) || { on: true, settings: null };
                rec.settings = readMSnapshot();
                setM(k, rec);
                pushTrackSettings(k);
            }
        }
        scheduleSend();
    }, true);
    panel.addEventListener('input', ev => { if (fromRack(ev)) scheduleSend(); }, true);
    // The XTC triangle comes back from its own window as an event; xtc.js
    // stores it first (its listener was registered earlier), then this sends.
    window.__TAURI__.event?.listen('xtc:geometry', () => {
        if (mLiveId != null) {
            const e = entryOfTrack(mLiveId);
            if (e && isMActive(e)) {
                const k = mKey(e.name);
                const rec = getM(k) || { on: true, settings: null };
                rec.settings = readMSnapshot();
                setM(k, rec);
                pushTrackSettings(k);
            }
        }
        setTimeout(scheduleSend, 0);
    }).catch(() => {});

    // The held controls take no click and open no menu: caught on the way
    // down, before their own handlers. A key that presses a button arrives
    // as a click too. The press is answered at once with the tooltip that
    // says why. A held checkbox that changes another way — "Only this one"
    // on another stage of the rack — is put back.
    for (const type of ['click', 'dblclick', 'contextmenu']) {
        document.addEventListener(type, ev => {
            const el = held && ev.target.closest?.('.pl-held');
            if (!el) return;
            ev.preventDefault();
            ev.stopPropagation();
            // A key's click has no pointer: the tooltip goes under the control.
            if (ev.detail > 0 || type === 'contextmenu') showNow(el, ev.clientX, ev.clientY);
            else showNow(el);
        }, true);
    }
    document.addEventListener('change', ev => {
        if (!held || !Object.hasOwn(held, ev.target.id) || ev.target.checked === held[ev.target.id]) return;
        ev.target.checked = held[ev.target.id];
        ev.stopImmediatePropagation();
    }, true);

    document.addEventListener('keydown', ev => {
        // An open list's row is in its select (select.css: the list is the page's).
        if (ev.key !== ' ' || ev.target.closest('input, select, textarea, button')) return;
        ev.preventDefault();
        leaveShowcase();
        onPlay(null);
    });
    // The analyser's ▶/⏸ and its Space (analytics/window.js): the live
    // window's is this transport's; a track window's is that row's ▶.
    window.__TAURI__.event?.listen('player-transport', ev => {
        // The analyser's seek: a rack change still settling goes out first,
        // as for this window's own seek bar.
        if (ev.payload?.action === 'seek') {
            const at = Number(ev.payload.seconds);
            if (Number.isFinite(at)) flushRack().then(() => invoke('player_seek', { seconds: at })).catch(showUiError);
            return;
        }
        if (ev.payload?.action !== 'toggle') return;
        const id = ev.payload.trackId;
        const entry = id != null ? entryOfTrack(id) : null;
        if (id != null && !entry) return;   // the row is gone from the list
        onPlay(entry);
    }).catch(() => {});
}

/// The heading names the whole app now. Only its text changes: the markup
/// keeps `<h2>Aura Converter</h2>` (other builds patch against that string),
/// and anything else inside the heading is left where it is.
function renameHeading() {
    const h2 = document.querySelector('#converterPanel h2');
    const text = h2 && [...h2.childNodes].find(n => n.nodeType === Node.TEXT_NODE && n.textContent.trim());
    if (text) text.textContent = text.textContent.replace('Aura Converter', 'Aura Engine');
}

/// Handle the M button toggle on a list entry.
function onMemoryToggle(entry) {
    if (!entry) return;
    const wasOn = isMActive(entry);
    const k = mKey(entry.name);

    if (!wasOn) {
        // Turning on: capture the current rack as the initial M snapshot.
        const snap = readMSnapshot();
        setM(k, { on: true, settings: snap });
        // If this track is currently playing, switch to M mode live.
        if (now.id === entry.trackId && !bitPerfect) {
            saveBaseRackIfAbsent();
            mLiveId = entry.trackId;
        }
    } else {
        // Turning off.
        const rec = getM(k);
        if (rec) { rec.on = false; setM(k, rec); }
        // The rack shows this memory when the M-live track is one of its
        // rows - not "now.id is this row": now lags the poll, so during a
        // switch to this track (▶ already applied its memory, the status
        // still names the old track) the base was never restored, and the
        // poll, seeing mLiveId already on the track, skipped it too.
        const live = mLiveId != null ? entryOfTrack(mLiveId) : null;
        if (live && mKey(live.name) === k) {
            // The rack shows this memory — restore base rack immediately.
            mLiveId = null;
            if (getBaseRack()) {
                restoreAndClearBaseRack();
                sendNow();
            }
        }
    }
    // The backend's copy for every row of this memory: set when on, cleared
    // when off (the prefetch and the hand-over read it).
    pushTrackSettings(k);

    // Redraw the row to show the new M button state.
    const ids = new Set([entry.id]);
    // Also redraw any other rows sharing the same mKey (M1: name-only key).
    for (const e of state.list) {
        if (e !== entry && mKey(e.name) === k) ids.add(e.id);
    }
    refreshRows(ids);
    // Which converted file is this track's sound depends on M now (entrySig
    // in library.js): the player needs the new rendered map.
    sendNow();
}

/// The seek band: from where the sound is now (`from`) to the target
/// (`to`), both shares of the track, breathing until the jump is heard.
function placeSeekBand(from, to) {
    const band = $('plSeekShimmer');
    if (!band) return;
    const clamp = v => Math.max(0, Math.min(1, v || 0));
    const a = clamp(from), b = clamp(to);
    band.style.left = Math.min(a, b) * 100 + '%';
    band.style.width = Math.max(0.4, Math.abs(b - a) * 100) + '%';
    band.classList.toggle('pl-seek-shimmer--back', b < a);
    band.classList.add('active');
}

// ── the player's right-click menu ─────────────────────────────────────
// A menu, not a panel (Anton 29.09): a row for each thing. A row with more
// to it (›) opens its page in the same box — the window is narrow, so the
// page takes the menu's place instead of standing beside it — with a back
// row on top; Esc goes back a page, then closes. The big player: Visualization
// › (the scenes, Off, the studio), Spectrum bars (ticked while they show;
// off, the picture fills the whole player), Full screen; then Picture delay ›.
// The studio's small player: Listening mode, then Picture delay ›. Both end
// with Instant start (ticked: sound at once; not: the whole chain first).
//
// The picture delay is set per device. The backend measures the delay
// Windows knows of (up to the driver); a DAC's own filter or active
// speakers add time nobody reports, and the listener sets that part once
// per device. It holds the bars and the scenes back alike.

const SPEC_DELAY_KEY = 'auraSpecDelay:';
// UI texts.
const DELAY_TEXTS = {
    why: 'Holds the bars and the scenes back for this device: a DAC filter or active speakers add time Windows does not report.',
    measured: ms => `Windows reports ${ms} ms.`,
    reset: 'Reset',
};
const MENU_TEXTS = {
    vis: 'Visualization',
    bars: 'Spectrum bars',
    barsTip: 'The bars under the picture. Hidden, the picture fills the whole player.',
    full: 'Full screen',
    fullTip: 'The picture on the whole screen. Esc brings it back.',
    delay: 'Picture delay',
    delayTip: 'For this device: holds the bars and the scenes back until the sound reaches you.',
    listen: 'Listening mode',
    listenTip: 'The settings fold away; the player and the list take the window.',
    back: 'Back',
    off: 'Off',
    offTip: 'No visualization; covers are shown',
    studio: 'Studio…',
    studioTip: 'Write, tune and save visualizations (GLSL), or have an AI write them',
    instant: 'Instant start',
    instantTip: 'On: the sound starts at once, and the stages switch in as they get ready. Off: every stage is prepared first — the first sound you hear already has them all, after a seek, a new track or a rack change too.',
};

function specDelayMs() {
    try { return Number(localStorage.getItem(SPEC_DELAY_KEY + (status?.device || ''))) || 0; } catch (_) { return 0; }
}

function setSpecDelayMs(ms) {
    try { localStorage.setItem(SPEC_DELAY_KEY + (status?.device || ''), String(ms)); } catch (_) {}
}

const delayText = v => (v > 0 ? '+' : '') + v + ' ms';

let plMenu = null;
let menuAt = { x: 0, y: 0 };

function closePlayerMenu() {
    if (!plMenu) return;
    plMenu.remove();
    plMenu = null;
    window.removeEventListener('blur', closePlayerMenu);
}

/// Esc: a page goes back to the first one, the first one puts the menu away.
function menuEscape() {
    if (plMenu?.dataset.page !== 'main') showMenuPage('main');
    else closePlayerMenu();
}

/// A row: a tick column, the name, and at the right a key or a value and,
/// for a page, its ›.
function menuRow(parent, { text, tip, on, right, page, id, act }) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'pl-vis-item' + (on ? ' on' : '') + (page ? ' pl-menu-sub' : '');
    b.setAttribute('role', 'menuitem');
    if (id) b.id = id;
    b.innerHTML = '<span class="pl-vis-tick"></span><span class="pl-vis-nm"></span>';
    b.querySelector('.pl-vis-nm').textContent = text;
    if (right) {
        const r = document.createElement('span');
        r.className = 'pl-menu-right';
        r.textContent = right;
        b.appendChild(r);
    }
    if (page) b.insertAdjacentHTML('beforeend', '<span class="pl-vis-arrow">›</span>');
    if (tip) b.dataset.tip = tip;
    b.addEventListener('click', () => {
        if (page) { showMenuPage(page); return; }
        closePlayerMenu();
        act();
    });
    parent.appendChild(b);
    return b;
}

function menuSep(parent) {
    const d = document.createElement('div');
    d.className = 'pl-vis-sep';
    parent.appendChild(d);
}

/// The top of a page: back to the menu's first page.
function menuBack(parent, title) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'pl-vis-item pl-menu-back';
    b.setAttribute('aria-label', MENU_TEXTS.back);
    b.innerHTML = '<span class="pl-vis-tick">‹</span><span class="pl-vis-nm"></span>';
    b.querySelector('.pl-vis-nm').textContent = title;
    b.addEventListener('click', () => showMenuPage('main'));
    parent.appendChild(b);
    menuSep(parent);
}

/// The name of the look the big player shows, for the Visualization row.
function lookName(api) {
    const cur = api.current();
    if (cur === 'off') return MENU_TEXTS.off;
    return api.looks().find(l => l.id === cur)?.name || '';
}

const MENU_PAGES = {
    main(page) {
        if (document.body.classList.contains('mode-listening')) {
            const api = wavesMenu();
            if (api) menuRow(page, { text: MENU_TEXTS.vis, right: lookName(api), page: 'vis', id: 'plVisMenuBtn' });
            const bars = playerViewHooks.bars;
            if (bars) menuRow(page, { text: MENU_TEXTS.bars, tip: MENU_TEXTS.barsTip, on: bars.shown(), id: 'plMenuBars',
                act: () => bars.set(!bars.shown()) });
            const full = $('plFullBtn');
            if (full) menuRow(page, { text: MENU_TEXTS.full, tip: MENU_TEXTS.fullTip, id: 'plMenuFull', act: () => full.click() });
        } else {
            const strip = $('plModeStrip');
            if (strip) menuRow(page, { text: MENU_TEXTS.listen, tip: MENU_TEXTS.listenTip, right: 'L', id: 'plMenuListen',
                act: () => strip.click() });
        }
        if (page.firstChild) menuSep(page);
        menuRow(page, { text: MENU_TEXTS.delay, tip: MENU_TEXTS.delayTip, right: delayText(specDelayMs()), page: 'delay',
            id: 'plMenuDelay' });
        menuSep(page);
        menuRow(page, { text: MENU_TEXTS.instant, tip: MENU_TEXTS.instantTip, on: instantStart, id: 'plMenuInstant',
            act: () => setInstantStart(!instantStart) });
    },

    /// Visualization ›: the looks (the one chosen ticked), Off, the studio.
    vis(page) {
        const api = wavesMenu();
        if (!api) return;
        page.id = 'plVisMenu';
        menuBack(page, MENU_TEXTS.vis);
        const cur = api.current();
        const item = (text, tip, on, act, id) => menuRow(page, { text, tip, on, id, act });
        // A live stream's instruments are found on a graphics card: without
        // one (or its memory) the scenes that draw them show the mix, and say so.
        const stream = !!status?.radio && !status.radio.stopped;
        const off = stream ? RADIO_TEXTS.instrumentsOff[status.radio.instruments] : null;
        for (const l of api.looks()) {
            const b = item(l.name, l.spatial && off ? off : l.why, l.id === cur, () => api.set(l.id));
            b.dataset.look = l.id;
            if (api.failed(l.id)) b.classList.add('failed');
        }
        menuSep(page);
        item(MENU_TEXTS.off, MENU_TEXTS.offTip, cur === 'off', () => api.set('off')).dataset.look = 'off';
        menuSep(page);
        item(MENU_TEXTS.studio, MENU_TEXTS.studioTip, false, () => {
            const shown = api.current();
            try { if (/^vis:/.test(shown)) localStorage.setItem('auraVisStudioScene', shown.slice(4)); } catch (_) {}
            invoke('vis_studio_open').catch(showUiError);
            window.__TAURI__?.event.emit('vis:studio-select', { id: /^vis:/.test(shown) ? shown.slice(4) : null }).catch(() => {});
        }, 'plVisStudio');
    },

    /// Picture delay ›: the slider for this device, its reset, what it is for.
    delay(page) {
        page.id = 'plDelayMenu';
        menuBack(page, MENU_TEXTS.delay);
        const box = document.createElement('div');
        box.className = 'pl-delay-box';
        const line = document.createElement('div');
        line.className = 'pl-delay-row';
        const range = document.createElement('input');
        range.type = 'range'; range.min = '-100'; range.max = '400'; range.step = '10';
        range.value = String(specDelayMs());
        const val = document.createElement('span');
        val.className = 'pl-delay-val';
        const reset = document.createElement('button');
        reset.type = 'button';
        reset.className = 'pl-delay-reset';
        reset.textContent = DELAY_TEXTS.reset;
        line.append(range, val, reset);
        const why = document.createElement('div');
        why.className = 'pl-delay-why';
        why.textContent = DELAY_TEXTS.why;
        const meas = document.createElement('div');
        meas.className = 'pl-delay-meas';
        const lat = Math.round(status?.latencyMs || 0);
        meas.textContent = lat > 0 ? DELAY_TEXTS.measured(lat) : '';
        box.append(line, why, meas);
        page.appendChild(box);
        const show = () => { val.textContent = delayText(Number(range.value) || 0); };
        range.addEventListener('input', () => { setSpecDelayMs(Number(range.value) || 0); show(); });
        reset.addEventListener('click', () => { range.value = '0'; setSpecDelayMs(0); show(); });
        show();
    },
};

/// A page of the menu in its box, and the box kept inside the window from
/// where it was opened.
function showMenuPage(name) {
    if (!plMenu) return;
    const page = document.createElement('div');
    page.className = 'pl-menu-page';
    MENU_PAGES[name](page);
    plMenu.dataset.page = name;
    plMenu.replaceChildren(page);
    const r = plMenu.getBoundingClientRect();
    plMenu.style.left = Math.max(8, Math.min(menuAt.x, window.innerWidth - r.width - 8)) + 'px';
    plMenu.style.top = Math.max(8, Math.min(menuAt.y, window.innerHeight - r.height - 8)) + 'px';
}

function openPlayerMenu(x, y) {
    closePlayerMenu();
    const menu = document.createElement('div');
    menu.className = 'pl-menu';
    menu.id = 'plMenu';
    menu.setAttribute('role', 'menu');
    document.body.appendChild(menu);
    plMenu = menu;
    menuAt = { x, y };
    showMenuPage('main');
    window.addEventListener('blur', closePlayerMenu);
}

/// The menu's first page as a right-click would open it now, for the tour
/// to show in place: not opened, not wired to the pointer or the keys (the
/// tour holds them); the caller places it and takes it away.
export function playerMenuPreview() {
    const menu = document.createElement('div');
    menu.className = 'pl-menu';
    menu.dataset.page = 'main';
    const page = document.createElement('div');
    page.className = 'pl-menu-page';
    MENU_PAGES.main(page);
    menu.appendChild(page);
    return menu;
}

function wirePlayerMenu() {
    $('plBar')?.addEventListener('contextmenu', e => {
        if (e.target.closest('button, select, input, a')) return;
        e.preventDefault();
        openPlayerMenu(e.clientX, e.clientY);
    });
    // A press outside the menu — the right button's too — only puts it
    // away (popups.js).
    addPopup({ isOpen: () => !!plMenu, inside: t => !!plMenu?.contains(t), close: closePlayerMenu, escape: menuEscape });
}

export function initPlayer() {
    buildBar();
    renameHeading();
    listHooks.added = onAdded;
    listHooks.removed = onRemoved;
    // A row dragged: the player's queue takes the list's order (next, previous
    // and the gapless next follow it).
    listHooks.moved = (list) => {
        const ids = list.map(e => e.trackId).filter(id => id != null);
        invoke('player_reorder', { ids }).catch(showUiError);
    };
    listHooks.play = onPlay;
    listHooks.nowPlaying = () => shownNow();
    listHooks.memoryToggle = onMemoryToggle;
    // When a conversion finishes, a playlist switches, or rows are removed,
    // FE-Q calls this to re-send the rendered map immediately rather than
    // waiting for the next rack-change debounce.
    listHooks.renderedChanged = () => sendNow();

    // Load track memory from localStorage before any row renders.
    loadMemory();
    loadBaseRack();

    wire();
    wirePlayerMenu();
    initRadio({
        play: playRadio,
        call: invoke,
        // The rack as it would be sent now, for the stream's filter the view
        // has made ahead (radio_stream_wait).
        settings: collectPlayerSettings,
    });

    // Initialise the repeat button visual from the persisted state and inform
    // the backend of the restored mode.
    updateRepeatBtn();
    invoke('player_set_repeat', { mode: repeatMode }).catch(() => {});

    // What the strip shows is what the backend has, from the first moment:
    // device, volume and the chain are sent now, not on first touch.
    loadDevices(true);
    const db = volDb();
    $('plVolText').textContent = fmtDb(db);
    invoke('player_set_volume', { db }).catch(() => {});
    render();
    setInterval(poll, 250);
}

// ── volume ────────────────────────────────────────────────────────────
// The slider is not in dB (Anton 27.09): its value is a position from −60
// (the bottom) to 0 (the top), and the level falls with its square — the
// first stretch below the top takes off tenths of a dB, the middle −15,
// the bottom −60 — so a little quieter is easy to find. dB = −60·(1 − p)²,
// p = (value + 60) / 60. The ends are the same numbers in both scales, so
// value −60 still means −60 dB (our test tool sets it that way).
const VOL_FLOOR_DB = -60;
function volDb() {
    const v = parseFloat($('plVol')?.value);
    const p = Math.max(0, Math.min(1, ((Number.isFinite(v) ? v : 0) - VOL_FLOOR_DB) / -VOL_FLOOR_DB));
    return Math.round(VOL_FLOOR_DB * (1 - p) * (1 - p) * 10) / 10;
}
function volPos(db) {
    const q = Math.max(0, Math.min(1, db / VOL_FLOOR_DB));
    return Math.round((VOL_FLOOR_DB + -VOL_FLOOR_DB * (1 - Math.sqrt(q))) * 10) / 10;
}
let volHeld = false, volTipTimer = null;
function showVolTip(on) {
    const tip = $('plVolTip'), vol = $('plVol');
    if (!tip || !vol) return;
    clearTimeout(volTipTimer);
    if (on) {
        const p = (parseFloat(vol.value) - VOL_FLOOR_DB) / -VOL_FLOOR_DB;
        const thumb = 16;
        tip.textContent = fmtDb(volDb());
        tip.style.left = `${thumb / 2 + p * Math.max(0, vol.clientWidth - thumb)}px`;
        tip.classList.add('show');
    }
    if (!volHeld) volTipTimer = setTimeout(() => tip.classList.remove('show'), on ? 900 : 500);
}

/// Called once the rack has been restored from the saved settings.
export function playerSettingsReady() {
    sendNow();
}
