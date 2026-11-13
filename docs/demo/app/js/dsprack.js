// ══════════════════════════════════════════════════════════════════════
// Advanced DSP as a rack of stages, grouped into the three zones the
// pipeline runs them in, and inside each zone in the order it runs them —
// one row per stage, with its name and its state written out:
//
//   ┌ 1 SOURCE ─────────────┐ ┌ 2 CONVERSION ─────────┐
//   │ ▌DC   Declip       ON │ │ ▌AA   Apodizer     ON │
//   │ ▌ISP  Intersample     │ │ ▌PFR  Polyphase    ON │
//   │ ▌SUB  Subsonic  20 Hz │ │ ▌TFS  TFS Phase  ✕ HP │
//   │ ▌AHR  Headroom ←Head. │ │ ▌HP   Hybrid-Phase  α │
//   └───────────────────────┘ │       (two-row unit)  │
//   ┌ 3 OUTPUT ─────────────┐ └───────────────────────┘
//   │ ▌XTC  Cross…  MEASURE │
//   └───────────────────────┘
//
// A rack rather than a column of checkboxes: it says what each stage is in
// words, and it reads top to bottom in the order the file is actually
// processed, which a checkbox list never did.
//
// The zones are not a second list. They are slices of DSP_CHAIN, which is
// also what the per-file rows in the queue render. Move a stage in the
// pipeline, change it there, and both follow.
//
// The rows are the interface; the checkboxes they replace are still in the
// DOM, hidden, keeping their ids — so settings.js saves and restores them,
// converter.js reads them when a run starts, and every existing `change`
// listener (locking the apodizing select, the custom filter button) fires
// untouched. A row sets `.checked` and dispatches
// `change`; nothing downstream can tell the difference.
//
// Dependencies and conflicts are the `← HP` and `✕ TFS` in the state column,
// and they propagate along the chain rather than one level deep: turning on
// αHP pulls HP up with it, and HP is the one that conflicts with TFS.
// Enabling recurses into dependencies, disabling recurses into dependents,
// and a conflict is read from both ends — TFS declares it, HP is bound by it
// too. The invariant check at the bottom is the standing proof; it is silent
// until something breaks it.
//
// One dependency does not point at another badge. Adaptive Headroom decides
// whether to APPLY the cut the Headroom control asks for — prepare.rs runs it
// only inside `if settings.headroom_db < 0.0` — so with Headroom on Off there
// is nothing for it to decide and it would be a lit badge doing nothing. It
// therefore depends on that select, not on a stage.
// ══════════════════════════════════════════════════════════════════════

import { isXtcConfigured, openXtcGeometry, xtcSummary, initXtcBridge } from './xtc.js';
import { initTooltip, hide } from './tooltip.js';
import { ICON_POWER, ICON_SOLO, ICON_GEAR } from './icons.js';
import { renderStrip } from './rackstrip.js';
import { addPopup } from './popups.js';
import { TAP_PRESETS } from './inventory.js';
import { getEqState, getEqPresets, setEqState, initEqBridge, noteEqSig } from './eq-bridge.js';
import { badgeLabel, allPresets as allEqPresets, speakersActive, speakerFields, balanceOf, delayOf } from './eq/state.js';
import { fmtBalance, fmtDelay } from './eq/controls.js';

/// Strict pipeline order — prepare.rs, then process.rs. The zone layout and
/// the per-file chain in the queue both read from here. TFS and HP never run
/// together (one phase path per file), so their order is the rack's choice:
/// TFS first, leaving HP's two-row unit at the bottom of its zone.
export const DSP_CHAIN = [
    'convLabDeclip', 'convLabIsp', 'convSubsonic', 'convLabHeadroom',
    'convAdaptiveApodizer', 'convFirResampling', 'convLabTfs', 'convHybridPhase', 'convLabAlpha',
    'convLabXtc', 'convEq', 'convAlbumLevel',
];

/// Where the chain is cut into zones. `n` is how many stages of DSP_CHAIN
/// belong to each, in order, so the three plates are exactly the chain.
export const DSP_ZONES = [
    { title: 'Source',     n: 4, note: 'runs on the source, before the filter sees it' },
    { title: 'Conversion', n: 5, note: 'the filter itself and the phase it is rendered in' },
    { title: 'Output',     n: 3, note: 'runs on the finished render' },
];

const FEAT = [
    { id: 'convLabDeclip', tok: 'DC', hue: 'dc', name: 'Declip',
      tip: 'Rebuilds peaks the master flattened at full scale. Constrained AR interpolation: the rail is a lower bound, not a value.' },
    { id: 'convLabIsp', tok: 'ISP', hue: 'isp', name: 'Intersample Peak Correction',
      tip: 'Finds true-peak overs above 0 dBTP with a 4x Lanczos scan and applies the smallest correction that clears them. Skips spans Declip rebuilt.' },
    // Runs here, after the repairs, rather than straight after DC blocking: a
    // high-pass in front of Declip or ISP would tilt the tops they read.
    { id: 'convSubsonic', tok: 'SUB', hue: 'sub', name: 'Subsonic Filter',
      amount: { input: 'convSubsonicHz', steps: [20, 15, 10], def: 15, strict: true, unit: 'Hz', digits: 0,
                title: 'Corner frequency — click to change' },
      tip: 'Linear-phase high-pass below the corner on the chip — 20, 15 or 10 Hz. Flat from the corner up, so nothing above it changes in level or in timing; at least 100 dB down from half the corner to DC. For the infrasonic content and slow drift a disc can carry, which removing the DC offset does not touch. Protection for the speakers, not a sound improvement. Runs again on the finished render, after the output limiter, whose gain dips would otherwise put infrasonic products back.' },
    { id: 'convLabHeadroom', tok: 'AHR', hue: 'ahr', name: 'Adaptive Headroom',
      needsSelect: { id: 'convHeadroom', off: '0', label: 'Headroom' },
      tip: 'Keeps the shipped ceiling instead of the one the Headroom control asks for, when lowering it would cost level for nothing: the source peak already sits below the target, or ENOB says this is a pristine high-bit-depth master. It decides whether to apply that cut, so it needs Headroom set to something other than Off.' },

    { id: 'convAdaptiveApodizer', tok: 'AA', hue: 'aa', name: 'Adaptive Apodizer',
      tip: 'Per-file forensics on the source: finds ADC or resampler pre-ringing, measures its frequency, and sets the cutoff from that. Leaves clean and minimum-phase sources alone.' },
    { id: 'convFirResampling', tok: 'PFR', hue: 'pfr', name: 'Polyphase FIR Resampling',
      tip: 'The FIR is the resampler — no second resampler in the chain. One exact convolution against the designed filter. Integer ratios only.' },
    { id: 'convHybridPhase', tok: 'HP', hue: 'hp', name: 'Hybrid-Phase Blending', cost: '2×',
      tip: 'Linear phase through sustained passages, minimum phase across attacks, switched at a zero crossing with a short fade. Doubles processing time.' },
    // No row of its own: the α switch on HP's unit (`unit`) turns it.
    { id: 'convLabAlpha', tok: 'αHP', hue: 'ahp', name: 'Continuous Alpha', needs: 'convHybridPhase', unit: 'convHybridPhase',
      tip: 'Drops the hard switch in Hybrid-Phase for a per-sample crossfade driven by the HPSS envelope. Needs Hybrid-Phase.' },
    { id: 'convLabTfs', tok: 'TFS', hue: 'tfs', name: 'TFS Phase', conflicts: 'convHybridPhase',
      tip: 'Linear phase below 1.5 kHz, minimum phase above 4 kHz, blended between. Needs both filter blobs on disk. Replaces Hybrid-Phase for the file.' },

    { id: 'convLabXtc', tok: 'XTC', hue: 'xtc', name: 'Crosstalk Cancellation', setup: true,
      tip: 'Cancels the sound each speaker sends to the wrong ear, so the stereo image is no longer bounded by the speakers. Built from the triangle you measure — correct only for that seat, and only while your head stays in it. For speakers, never headphones.' },
    { id: 'convEq', tok: 'EQ', hue: 'eq', name: 'Equalizer', setup: true,
      tip: 'Parametric EQ for headphone or room correction. Loads AutoEq, Equalizer APO and REW profiles. Off: the output is bit-identical.' },
    // A batch decision, not a stage of one file's sound: the player does not
    // run it (`batchOnly`), so its strip never shows it waiting.
    { id: 'convAlbumLevel', tok: 'ALB', hue: 'alb', name: 'Album Level', batchOnly: true,
      tip: 'Tracks of one album — same folder, same Album tag — get one gain: the deepest cut any of them needs, applied to all, so the album keeps the balance it was mastered with. Files without an Album tag, and an album\'s only track in the queue, keep their own.' },
];

/// The rack column is narrow, so a few stages carry a shorter label than
/// their full name. The full one is still what the tooltip says, and what
/// `aria-label` gives a screen reader.
const SHORT = {
    convAdaptiveApodizer: 'Adaptive Apodizer',
    convHybridPhase: 'Hybrid-Phase',
    convFirResampling: 'Polyphase FIR',
    convSubsonic: 'Subsonic Filter',
    convLabDeclip: 'Declip',
    convLabIsp: 'Intersample Peak',
    convLabTfs: 'TFS Phase',
    convLabAlpha: 'Continuous Alpha',
    convLabHeadroom: 'Adaptive Headroom',
    convLabXtc: 'Crosstalk Cancel',
    convEq: 'Equalizer',
    convAlbumLevel: 'Album Level',
};

export const FEAT_BY_ID = Object.fromEntries(FEAT.map(f => [f.id, f]));

/// The stages the player runs live: the chain less the batch decisions.
export const PLAYER_CHAIN = DSP_CHAIN.filter(id => !FEAT_BY_ID[id].batchOnly);
const labelOf = f => SHORT[f.id] || f.name;

/// The corners the backend accepts (apodize.rs SUBSONIC_CORNERS_HZ), in the
/// order a click walks them: from the default down, then round again.
///
/// Read off the stage's own chip rather than written twice, so the rack and
/// anything reading this cannot drift apart. The standing test in
/// tests/dsprack.test.mjs holds it against the Rust list, which is the pair
/// that can actually disagree.
export const SUBSONIC_STEPS = FEAT_BY_ID.convSubsonic.amount.steps;

/// Chain position -> zone index, derived once so the two can never disagree.
const ZONE_OF = (() => {
    const out = {};
    let i = 0;
    DSP_ZONES.forEach((z, zi) => {
        for (let k = 0; k < z.n; k++) out[DSP_CHAIN[i++]] = zi;
    });
    if (i !== DSP_CHAIN.length) {
        console.error('[dsp] zone sizes do not cover the chain', i, DSP_CHAIN.length);
    }
    return out;
})();
const zoneIds = zi => DSP_CHAIN.filter(id => ZONE_OF[id] === zi);

const $ = id => document.getElementById(id);
const isOn = id => !!$(id)?.checked;

// ── the one stage that carries a number ───────────────────────────────
/// SUB's corner lives in a hidden input so settings.js saves it with
/// everything else and converter.js reads it the same way it reads a
/// select. The rack only cycles it.
function amountOf(id) {
    const a = FEAT_BY_ID[id].amount;
    if (!a) return 0;
    const v = parseFloat($(a.input)?.value);
    // For a strict amount a value off the list (an old settings blob, a hand
    // edit) reads as the default: the backend refuses anything else.
    if (Number.isFinite(v) && (!a.strict || a.steps.includes(v))) return v;
    return a.def ?? a.steps[Math.floor(a.steps.length / 2)];
}
const amountText = (id) => {
    const a = FEAT_BY_ID[id].amount;
    return amountOf(id).toFixed(a.digits ?? 1) + ' ' + (a.unit || 'dB');
};

/// HP's lamp, in each of its modes: how much of the minimum-phase branch is
/// heard right now (the player asks /player/phase_lamp each screen frame).
const PHASE_LAMPS = {
    convHybridPhase: 'Lights while Hybrid-Phase plays the minimum-phase path across an attack, in step with the sound (to a screen frame, ~16 ms).',
    convLabAlpha: 'Glows with α, the share of the minimum-phase path heard right now: it jumps up on an attack and fades as α decays (~70 ms to a tenth).',
};

/// Why the lamps stay dark although HP is on: a converted file made without
/// HP, a converted file's lamp not worked out (yet), or BIT-PERFECT.
export const PHASE_LAMP_IDLE = {
    file: 'This track plays its converted file, made without HP or αHP: nothing blends, so the lamp stays dark.',
    pending: 'This track plays its converted file: the lamp is being worked out from the source in the background.',
    nosource: 'This track plays its converted file: its source is not there, so the lamp cannot be worked out.',
    lowmem: 'This track plays its converted file: not enough free memory to work the lamp out from the source.',
    converting: 'This track plays its converted file: a conversion is running, so the lamp is not worked out now.',
    failed: 'This track plays its converted file: its source could not be read, so the lamp stays dark.',
    direct: 'BIT-PERFECT: no stage plays, so the lamp stays dark.',
};

/// The lamp route's third byte (/player/phase_lamp): where the lamps come
/// from — 0 a live blend, 1 a converted file's timeline worked out from its
/// source, 2 still being worked out, 3.. why it cannot be. The PHASE_LAMP_IDLE
/// key it means, or null when the lamps may light. `file`: the track plays its
/// converted file (a 0 there: the file was made without HP).
export function lampIdleOf(state, file = false) {
    const why = [null, null, 'pending', 'nosource', 'lowmem', 'converting', 'failed'];
    if (state >= 2) return why[state] ?? 'failed';
    return file && !state ? 'file' : null;
}

/// Light HP's one lamp, 0..1: `hp` (the switch) while the blend switches,
/// `alpha` while α is on. The brightness is the value itself, frame by
/// frame — no easing of its own. `idle` (a PHASE_LAMP_IDLE key) says on the
/// lamp why it cannot light.
export function setPhaseLamps(hp, alpha, idle = null) {
    const el = document.querySelector('.lab-badge[data-id="convHybridPhase"] .lamp');
    if (!el) return;
    const id = isOn('convLabAlpha') ? 'convLabAlpha' : 'convHybridPhase';
    const s = lampLevel(id === 'convLabAlpha' ? alpha : hp);
    if (el.style.getPropertyValue('--lamp') !== s) {
        el.style.setProperty('--lamp', s);
        const lens = el.querySelector?.('.lens');
        if (lens) {
            const l = lampLens(+s);
            lens.style.setProperty('--lens-bg', l.bg);
            lens.style.setProperty('--lens-sh', l.shadow);
        }
    }
    const title = PHASE_LAMP_IDLE[idle] ?? PHASE_LAMPS[id];
    if (el.title !== title) el.title = title;
    el.classList.toggle('idle', !!PHASE_LAMP_IDLE[idle]);
}

// ── HP's unit: the α switch and the sensitivity slider ────────────────
/// The slider's steps, −3 (switches rarer) … +3 (more often); 0 is today's.
export const HP_SENS_MIN = -3, HP_SENS_MAX = 3;
/// The detector threshold each step means (HL6's engine side), for the
/// background's trigger zone: 0.3 / gain.
export const HP_SENS_THRESHOLDS = [0.45, 0.40, 0.35, 0.30, 0.26, 0.23, 0.20];

/// The step the hidden input holds; anything off the list reads as 0.
export function hpSensStep() {
    const n = Number($('convHpSens')?.value);
    return Number.isInteger(n) && n >= HP_SENS_MIN && n <= HP_SENS_MAX ? n : 0;
}
/// The value text: "−3" … "0" … "+3" (a true minus sign).
export const hpSensText = (n) => (n > 0 ? '+' + n : n < 0 ? '−' + -n : '0');
/// Whole-pixel places on a rail `w` px wide: step i of 7 sits at
/// round(i·w/6); a tick (2 px) starts 1 px before it, the thumb (10 px) 5 px
/// before it, so their centres coincide (Anton saw a ½-px shift once).
export function hpSensPlaces(w, step) {
    const x = i => Math.round(i * w / 6);
    return { ticks: [0, 1, 2, 3, 4, 5, 6].map(i => x(i) - 1), thumb: x(step - HP_SENS_MIN) - 5 };
}
/// The step under a pointer at `clientX` over a rail starting at `left`, `w` wide.
export function hpSensAt(clientX, left, w) {
    const f = w > 0 ? Math.min(1, Math.max(0, (clientX - left) / w)) : 0.5;
    return Math.round(f * 6) + HP_SENS_MIN;
}

/// α as the switch shows it: the checkbox while HP is on; while HP is off
/// (which takes αHP down with it) what it was, so it comes back with HP.
let alphaKept = false;
const alphaShown = () => (isOn('convHybridPhase') ? isOn('convLabAlpha') : alphaKept);

function setHpSens(n) {
    const el = $('convHpSens');
    n = Math.min(HP_SENS_MAX, Math.max(HP_SENS_MIN, n));
    if (!el || n === hpSensStep()) return;
    el.value = String(n);
    el.dispatchEvent(new Event('change', { bubbles: true }));
    paint();
    import('./settings.js').then(m => m.saveSettings());
}

function flipAlpha() {
    if (isOn('convHybridPhase')) { toggle('convLabAlpha'); return; }
    alphaKept = !alphaKept;
    paint();
}

function hpUnitHtml() {
    let ticks = '';
    for (let i = 0; i < 7; i++) ticks += `<i class="t1${i === 3 ? ' c' : ''}"></i>`;
    return '<canvas class="hpbg" aria-hidden="true"></canvas><canvas class="hpprep" aria-hidden="true"></canvas><i class="bar"></i>'
        + '<span class="pp" aria-hidden="true"></span>'
        + `<span class="r1"><span class="tok">HP</span><span class="nm">${SHORT.convHybridPhase}</span>`
        + `<span class="lamp" title="${PHASE_LAMPS.convHybridPhase}"><i class="lens"></i></span></span>`
        + '<span class="r2"><span class="ctl">'
        + '<span class="sw" data-alpha="1" role="switch" tabindex="0" aria-label="Continuous α"'
        + ' title="Continuous α: a smooth per-sample blend instead of the hard switch"><i></i></span>'
        + '<span class="al">α</span>'
        + `<span class="sk" data-sens="1" role="slider" tabindex="0" aria-valuemin="${HP_SENS_MIN}" aria-valuemax="${HP_SENS_MAX}"`
        + ' aria-label="Hybrid-Phase sensitivity" title="How often Hybrid-Phase switches: left rarer, right more often">'
        + `<span class="rail"><span class="ln"></span>${ticks}<span class="th"></span></span></span>`
        + '<span class="sv">0</span></span></span>';
}

/// Paint the unit's second row from the inputs.
function paintHpUnit(b) {
    if (isOn('convHybridPhase')) alphaKept = isOn('convLabAlpha');
    const a = alphaShown(), step = hpSensStep();
    const sw = b.querySelector('.sw');
    sw.classList.toggle('a', a);
    sw.setAttribute('aria-checked', String(a));
    b.classList.toggle('alpha', a);
    const sk = b.querySelector('.sk');
    sk.setAttribute('aria-valuenow', String(step));
    sk.setAttribute('aria-valuetext', hpSensText(step));
    b.querySelector('.sv').textContent = hpSensText(step);
    placeHpSens(b);
}

function placeHpSens(b) {
    const rail = b.querySelector('.rail');
    const w = rail?.offsetWidth;
    if (!w) return;
    const p = hpSensPlaces(w, hpSensStep());
    rail.querySelectorAll('.t1').forEach((e, i) => { e.style.left = p.ticks[i] + 'px'; });
    rail.querySelector('.th').style.left = p.thumb + 'px';
}

/// The slider's pointer and keys; clicks on it never reach the unit.
function wireHpUnit(b, locked) {
    const sk = b.querySelector('.sk'), rail = b.querySelector('.rail');
    const at = (e) => { const r = rail.getBoundingClientRect(); setHpSens(hpSensAt(e.clientX, r.left, r.width)); };
    sk.addEventListener('pointerdown', (e) => {
        e.stopPropagation();
        if (locked()) return;
        sk.setPointerCapture?.(e.pointerId);
        b.classList.add('drag');
        at(e);
    });
    sk.addEventListener('pointermove', (e) => { if (sk.hasPointerCapture?.(e.pointerId)) at(e); });
    // The controls stay bright while the thumb is held, off the unit too.
    for (const ev of ['pointerup', 'pointercancel', 'lostpointercapture']) sk.addEventListener(ev, () => b.classList.remove('drag'));
    sk.addEventListener('keydown', (e) => {
        const d = { ArrowRight: 1, ArrowUp: 1, ArrowLeft: -1, ArrowDown: -1 }[e.key];
        if (!d) return;
        e.preventDefault();
        e.stopPropagation();
        if (!locked()) setHpSens(hpSensStep() + d);
    });
    try { new ResizeObserver(() => placeHpSens(b)).observe(rail); } catch (_) { requestAnimationFrame(() => placeHpSens(b)); }
}

/// A lamp's brightness as the CSS reads it: 0..1, two decimals; 0 for nonsense.
export const lampLevel = (v) => (Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0).toFixed(2);

/// HP's lamp lens at brightness v (0..1): a matte convex square in its bezel
/// (Anton 29.10, variant C). Its colour runs dark violet → lilac by 0.6 →
/// near white; lit from above (hi), shaded below (lo); a lilac glow once it
/// is lit at all.
const LENS_OFF = [38, 32, 60], LENS_MID = [167, 139, 250], LENS_ON = [245, 243, 255];
const mix = (a, b, t) => a.map((x, i) => Math.round(x + (b[i] - x) * t));
const rgb = (c) => `rgb(${c.join(',')})`;
export function lampLens(v) {
    v = Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0;
    const c = v <= 0.6 ? mix(LENS_OFF, LENS_MID, v / 0.6) : mix(LENS_MID, LENS_ON, (v - 0.6) / 0.4);
    const hi = mix(c, [255, 255, 255], 0.35 + 0.25 * v), lo = mix(c, [0, 0, 0], 0.35);
    let shadow = `inset 0 1px 0 rgba(255,255,255,${(0.14 + 0.3 * v).toFixed(3)}), inset 0 -1px 0 rgba(0,0,0,.4)`;
    if (v > 0.03) {
        shadow += `, 0 0 ${(2 + 6 * v).toFixed(2)}px ${(1.2 * v).toFixed(2)}px `
            + `rgba(167,139,250,${Math.min(1, 1.2 * v).toFixed(3)})`;
    }
    return { c, bg: `radial-gradient(circle at 50% 35%, ${rgb(hi)} 0%, ${rgb(c)} 60%, ${rgb(lo)} 100%)`, shadow };
}

/// What converter.js sends as `subsonicHz`: 0 when the filter is off.
export function subsonicCornerHz() {
    return isOn('convSubsonic') ? amountOf('convSubsonic') : 0;
}

function cycleAmount(id, dir = 1) {
    const a = FEAT_BY_ID[id].amount;
    const el = a && $(a.input);
    if (!a || !el) return;
    const n = a.steps.length;
    const i = a.steps.indexOf(amountOf(id));
    el.value = String(a.steps[(i + dir + n) % n]);
    el.dispatchEvent(new Event('change', { bubbles: true }));
    paint();
    import('./settings.js').then(m => m.saveSettings());
}

// ── constraint propagation ────────────────────────────────────────────
const conflictPartners = (id) => {
    const out = new Set();
    if (FEAT_BY_ID[id].conflicts) out.add(FEAT_BY_ID[id].conflicts);
    FEAT.forEach(o => { if (o.conflicts === id) out.add(o.id); });
    return [...out];
};
const dependentsOf = id => FEAT.filter(o => o.needs === id).map(o => o.id);

/// Whether the control a stage leans on is set to something other than off.
/// Not a badge, so it is never switched from here — the rack only reads it.
function selectSatisfied(id) {
    const ns = FEAT_BY_ID[id].needsSelect;
    if (!ns) return true;
    const el = $(ns.id);
    return !!el && el.value !== ns.off;
}

function enable(id, ctx) {
    if (ctx.on.has(id)) return;
    // A stage whose select is on Off cannot come up, and nothing may pull it
    // up either — it would be lit and inert.
    if (!selectSatisfied(id)) return;
    ctx.on.add(id); ctx.off.delete(id);
    ctx.want.set(id, true);
    if (FEAT_BY_ID[id].needs) enable(FEAT_BY_ID[id].needs, ctx);
    conflictPartners(id).forEach(c => { if (ctx.want.get(c) ?? isOn(c)) disable(c, ctx); });
}
function disable(id, ctx) {
    if (ctx.on.has(id) || ctx.off.has(id)) return;   // never undo this op's own work
    ctx.off.add(id);
    ctx.want.set(id, false);
    dependentsOf(id).forEach(d => { if (ctx.want.get(d) ?? isOn(d)) disable(d, ctx); });
}

function apply(ctx) {
    // Write the checkboxes and let every existing listener run. Only the ones
    // that actually moved get an event, so nothing is re-run for nothing.
    let touched = false;
    for (const [id, want] of ctx.want) {
        const el = $(id);
        if (!el || el.checked === want) continue;
        el.checked = want;
        el.dispatchEvent(new Event('change', { bubbles: true }));
        touched = true;
    }
    paint();
    if (touched) import('./settings.js').then(m => m.saveSettings());
}

function toggle(id) {
    const ctx = { on: new Set(), off: new Set(), want: new Map() };
    isOn(id) ? disable(id, ctx) : enable(id, ctx);
    // HP comes back with the α its switch kept.
    if (id === 'convHybridPhase' && ctx.on.has(id) && alphaKept) enable('convLabAlpha', ctx);
    apply(ctx);
}
function solo(id) {
    const ctx = { on: new Set(), off: new Set(), want: new Map() };
    FEAT.forEach(f => ctx.want.set(f.id, false));
    enable(id, ctx);
    apply(ctx);
}

/// The Headroom select moved. A stage that leans on it has to follow, or it
/// would sit lit over a control that no longer gives it anything to do.
function selectChanged() {
    const ctx = { on: new Set(), off: new Set(), want: new Map() };
    FEAT.forEach(f => {
        if (f.needsSelect && isOn(f.id) && !selectSatisfied(f.id)) disable(f.id, ctx);
    });
    apply(ctx);
}

// ── painting ──────────────────────────────────────────────────────────
/// What the state column says about one stage, in priority order: that it is
/// on, that something is blocking it, what it depends on, or what it costs.
/// One short line each — the tooltip carries the sentences.
/// TFS this machine cannot derive at the rack's taps: the engine's reason
/// (`tfs_unavailable`) for the tap count it was last asked about. The unit
/// stays clickable — the player and the converter then run linear phase and
/// say why — but reads dim, its tip saying why first.
let tfsAsked = { taps: null, why: null };

function rackTapsNow() {
    const s = $('convTapSlider');
    return s ? TAP_PRESETS[Math.min(parseInt(s.value) || 0, TAP_PRESETS.length - 1)] : null;
}

/// The reason for the rack's taps now, asking the engine when they changed.
function tfsWhyNow() {
    const taps = rackTapsNow();
    if (taps !== tfsAsked.taps) {
        tfsAsked = { taps, why: null };
        window.__TAURI__?.core?.invoke('tfs_unavailable', { taps: taps > 30000000 ? 2000000000 : taps }).then((why) => {
            if (tfsAsked.taps === taps && (why || null) !== tfsAsked.why) {
                tfsAsked.why = why || null;
                paint();
            }
        }).catch(() => {});
    }
    return tfsAsked.why;
}

/// TFS's state cell when this machine cannot have it (`why`), else null:
/// armed it says ON, as it is; off it says why it would not run, in a word.
export function tfsLimitState(on, why) {
    if (!why) return null;
    return on ? { text: 'ON', cls: '', limited: true } : { text: 'RAM', cls: 'dim', limited: true };
}

function stateOf(id) {
    const f = FEAT_BY_ID[id];
    if (id === 'convLabTfs') {
        const lim = tfsLimitState(isOn(id), tfsWhyNow());
        if (lim) return lim;
    }
    // EQ badge shows the profile name (or FLAT) as its status text.
    if (id === 'convEq') return { text: badgeLabel(getEqState()), cls: '' };
    if (isOn(id)) return { text: 'ON', cls: '' };
    // Before its state, a stage waiting on configuration says what it wants.
    // Nothing else in the rack can be blocked by something outside the file.
    if (f.setup && !isXtcConfigured()) return { text: 'MEASURE', cls: 'unset' };
    const blocker = conflictPartners(id).find(isOn);
    if (blocker) return { text: '✕ ' + FEAT_BY_ID[blocker].tok, cls: 'blocked' };
    if (f.needs) return { text: '← ' + FEAT_BY_ID[f.needs].tok, cls: 'dim' };
    if (f.needsSelect && !selectSatisfied(id)) return { text: '← ' + f.needsSelect.label, cls: 'dim' };
    if (f.cost) return { text: f.cost, cls: 'dim' };
    return { text: '', cls: '' };
}

function paint() {
    document.querySelectorAll('.lab-badge').forEach(b => {
        const id = b.dataset.id, f = FEAT_BY_ID[id], on = isOn(id);
        b.classList.toggle('on', on);
        b.setAttribute('aria-pressed', String(on));
        const st = stateOf(id);
        b.classList.toggle('blocked', st.cls === 'blocked');
        b.classList.toggle('limited', !!st.limited);
        const amt = b.querySelector('.amt');
        if (amt) {
            amt.textContent = amountText(id);
            b.setAttribute('aria-label', `${f.name}, corner ${amountOf(id)} Hz — arrow keys change the corner`);
        }
        const cell = b.querySelector('.st');
        if (cell) {
            cell.textContent = st.text;
            cell.className = 'st' + (st.cls ? ' ' + st.cls : '');
        }
        if (b.classList.contains('hpu')) paintHpUnit(b);
        if (id === 'convEq') paintSpeakers(b);
    });
    checkInvariants();
    // The strip over the rack says Apodizing as AA while AA is on, and a
    // track's memory writes the rack behind it: it follows every repaint.
    renderStrip();
}

/// What the EQ's speakers play now, for the SPK mark and the tip:
/// "Balance L 1.5 dB · Delay L 2.04 ms", and "(EQ off)" when they play
/// alone; '' when they play nothing.
function speakersText() {
    const s = getEqState();
    if (!speakersActive(s)) return '';
    const { balance, balanceMode } = balanceOf(s), delay = delayOf(s);
    const parts = [];
    if (balance) parts.push('Balance ' + fmtBalance(balance, balanceMode));
    if (delay) parts.push('Delay ' + fmtDelay(delay));
    return 'Speakers: ' + parts.join(' · ') + (s.on ? '' : ' (EQ off)');
}

/// The SPK mark on the EQ's badge while a balance or a delay plays — with
/// the EQ off too, when the window keeps them on.
function paintSpeakers(b) {
    const text = speakersText();
    let el = b.querySelector('.spk');
    if (!text) { el?.remove(); return; }
    if (!el) {
        el = document.createElement('span');
        el.className = 'spk';
        el.textContent = 'SPK';
        const st = b.querySelector('.st');
        if (st) st.before(el); else b.append(el);
    }
    el.title = text;
    el.classList.toggle('alone', !getEqState().on);
}

/// A dozen lines that catch exactly the class of bug this rack already had
/// once: αHP switched on while TFS was on, αHP pulled HP up as its
/// dependency, and nobody re-checked HP against TFS. Silent when the rules
/// hold.
function checkInvariants() {
    const bad = [];
    FEAT.forEach(f => {
        if (f.needs && isOn(f.id) && !isOn(f.needs))
            bad.push(`${f.tok} is on without ${FEAT_BY_ID[f.needs].tok}`);
        if (f.conflicts && isOn(f.id) && isOn(f.conflicts))
            bad.push(`${f.tok} and ${FEAT_BY_ID[f.conflicts].tok} are both on`);
        if (f.needsSelect && isOn(f.id) && !selectSatisfied(f.id))
            bad.push(`${f.tok} is on with ${f.needsSelect.label} off`);
    });
    const el = $('labInv');
    if (!el) return;
    el.textContent = bad.length ? '✕ ' + bad.join('; ') : '';
    el.className = 'lab-inv' + (bad.length ? ' bad' : '');
}

// ── menu ──────────────────────────────────────────────────────────────
let menuTarget = null;
function closeMenu() { const m = $('labMenu'); if (m) m.style.display = 'none'; }

/// One plate per zone, one row per pipeline stage.
///
/// Zones pair up two to a row and an odd one spans both columns, which for
/// the current 4/5/2 puts Source beside Conversion and Output underneath.
/// Nothing here is written per zone: add a fourth and it pairs with Output
/// on its own.
function buildZones() {
    const field = $('labField');
    field.querySelectorAll('.lab-zone').forEach(e => e.remove());
    const odd = DSP_ZONES.length % 2 === 1;
    DSP_ZONES.forEach((z, zi) => {
        const wrap = document.createElement('div');
        wrap.className = 'lab-zone' + (odd && zi === DSP_ZONES.length - 1 ? ' span' : '');

        const head = document.createElement('div');
        head.className = 'lab-zone-head';
        head.title = z.note;
        const num = document.createElement('span');
        num.className = 'lab-zone-n';
        num.textContent = zi + 1;
        const cap = document.createElement('span');
        cap.className = 'lab-zone-t';
        cap.textContent = z.title;
        head.append(num, cap);

        const row = document.createElement('div');
        row.className = 'lab-row';
        zoneIds(zi).forEach(id => {
            const f = FEAT_BY_ID[id];
            if (f.unit) return;
            // HP is a unit two rows high with a switch and a slider inside,
            // which a <button> may not hold: a div that acts as one.
            if (id === 'convHybridPhase') {
                const u = document.createElement('div');
                u.className = 'lab-badge hpu';
                u.dataset.id = id;
                u.tabIndex = 0;
                u.setAttribute('role', 'button');
                u.style.setProperty('--h', `var(--lab-${f.hue})`);
                u.innerHTML = hpUnitHtml();
                u.setAttribute('aria-pressed', 'false');
                u.setAttribute('aria-label', f.name);
                wireHpUnit(u, () => field.classList.contains('lab-locked'));
                row.appendChild(u);
                return;
            }
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'lab-badge';
            b.dataset.id = id;
            b.style.setProperty('--h', `var(--lab-${f.hue})`);
            b.innerHTML = '<i class="bar"></i>'
                + `<span class="tok">${f.tok}</span>`
                + `<span class="nm">${labelOf(f)}</span>`
                + '<span class="st"></span>'
                + (f.setup ? '<span class="set" data-gear="1" tabindex="0" title="Settings…"><span>SET</span></span>' : '')
                + (f.amount ? `<span class="amt" data-amt="1" title="${f.amount.title}"></span>` : '');
            b.setAttribute('aria-pressed', 'false');
            b.setAttribute('aria-label', f.name);
            row.appendChild(b);
        });

        wrap.append(head, row);
        field.appendChild(wrap);
    });
}

/// Build the rack and wire it. Does nothing where there is no rack element,
/// so a page that brings its own panel is left alone.
export function initDspRack() {
    if (!$('labField')) return;

    if (!$('dspRackCss')) {
        const link = document.createElement('link');
        link.id = 'dspRackCss';
        link.rel = 'stylesheet';
        link.href = 'css/dsprack.css';
        document.head.appendChild(link);
    }
    if (!$('labTip')) {
        const labTipEl = document.createElement('div');
        labTipEl.id = 'labTip';
        labTipEl.setAttribute('role', 'tooltip');
        document.body.appendChild(labTipEl);
        const m = document.createElement('div');
        m.id = 'labMenu';
        m.innerHTML = `<button data-act="toggle">${ICON_POWER} <span id="labMToggle">Enable</span></button>`
            + `<button data-act="solo" id="labMSolo">${ICON_SOLO} Only this one</button>`
            + `<button data-act="setup" id="labMSetup">${ICON_GEAR} Geometry…</button>`
            + `<button data-act="eq-edit" id="labMEqEdit" style="display:none">${ICON_GEAR} Edit EQ…</button>`
            + `<div id="labMEqPresets" style="display:none"></div>`
            + `<button data-act="eq-import" id="labMEqImport" style="display:none">Load profile…</button>`;
        document.body.appendChild(m);
        m.addEventListener('click', (e) => {
            const btn = e.target.closest('button');
            const act = btn?.dataset.act;
            if (!act || !menuTarget) return;
            if ($('labField')?.classList.contains('lab-locked') && act !== 'setup' && act !== 'eq-edit' && act !== 'eq-import') { closeMenu(); return; }
            closeMenu();
            if (act === 'toggle') toggle(menuTarget);
            if (act === 'solo') solo(menuTarget);
            if (act === 'setup') openXtcGeometry(paint);
            if (act === 'eq-edit') window.__TAURI__?.core?.invoke('eq_window_open', { tab: null }).catch(() => {});
            if (act === 'eq-import') window.__TAURI__?.core?.invoke('eq_window_open', { tab: 'import' }).catch(() => {});
            if (act === 'eq-preset') {
                const name = btn?.dataset.presetName;
                if (!name) return;
                const allPresets = allEqPresets(getEqPresets());
                const found = allPresets.find(p => p.name === name);
                if (!found) return;
                // A preset brings a sound, not the room: the speakers stay.
                const newState = { ...found.state, ...speakerFields(getEqState()), on: isOn('convEq') };
                setEqState(newState);
                noteEqSig();
                import('./settings.js').then(m => m.saveSettings());
                window.__TAURI__?.core?.invoke('player_set_eq', { eq: newState }).catch(() => {});
                window.__TAURI__?.event?.emit('eq:set', { state: newState }).catch(() => {});
                paint();
            }
        });
    }

    buildZones();
    // The geometry window hands its result back over the event bus; this is the
    // ear for it. Registered once, before any badge can ask for that window.
    initXtcBridge(paint);
    // The EQ window feeds state back the same way.
    initEqBridge(paint);

    // Adaptive Headroom leans on this select rather than on a badge, so the
    // rack has to hear it move. settings.js also repaints after it restores,
    // which covers the other way the value can change.
    $('convHeadroom')?.addEventListener('change', selectChanged);

    const field = $('labField');
    // While the player rebuilds the sound the stages hold (player.js
    // setRackLocked): a click would start the rebuild over.
    const held = (e) => {
        if (!field.classList.contains('lab-locked') || !e.target.closest('.lab-badge')) return false;
        e.preventDefault();
        e.stopPropagation();
        return true;
    };
    field.addEventListener('click', (e) => {
        if (held(e)) return;
        const b = e.target.closest('.lab-badge');
        if (!b) return;
        hide();
        const id = b.dataset.id, f = FEAT_BY_ID[id];
        // The SET key is inside the button, so it has to be read before the
        // click is treated as a toggle.
        if (e.target.closest('[data-gear]')) {
            if (id === 'convEq') {
                window.__TAURI__?.core?.invoke('eq_window_open', { tab: null }).catch(() => {});
            } else {
                openXtcGeometry(paint);
            }
            return;
        }
        // The number chip is inside the button too, and cycling it must not
        // also flip the stage.
        if (e.target.closest('[data-amt]')) { cycleAmount(id); return; }
        // HP's α switch and slider are inside its unit: neither flips HP.
        if (e.target.closest('[data-alpha]')) { flipAlpha(); return; }
        if (e.target.closest('[data-sens]')) return;
        // Switching on XTC without a measured geometry would build the filter
        // for an invented room. Ask first, then come up if it saved.
        // EQ has no such gate — it can come up without configuration.
        if (id === 'convLabXtc' && f.setup && !isOn(id) && !isXtcConfigured()) {
            openXtcGeometry(() => toggle(id));
            return;
        }
        toggle(id);
    });
    field.addEventListener('contextmenu', (e) => {
        if (held(e)) return;
        const b = e.target.closest('.lab-badge');
        if (!b) return;
        e.preventDefault();
        hide();
        menuTarget = b.dataset.id;
        const m = $('labMenu');
        const isEq = menuTarget === 'convEq';
        $('labMToggle').textContent = isOn(menuTarget) ? 'Disable' : 'Enable';
        $('labMSolo').style.display = isEq ? 'none' : 'block';
        $('labMSetup').style.display = (!isEq && FEAT_BY_ID[menuTarget].setup) ? 'block' : 'none';
        $('labMEqEdit').style.display = isEq ? 'block' : 'none';
        $('labMEqImport').style.display = isEq ? 'block' : 'none';
        const eqPresetsEl = $('labMEqPresets');
        eqPresetsEl.style.display = isEq ? 'block' : 'none';
        if (isEq) {
            const allPresets = allEqPresets(getEqPresets());
            const curName = getEqState().name;
            eqPresetsEl.innerHTML = '';
            for (const p of allPresets) {
                const pbtn = document.createElement('button');
                pbtn.type = 'button';
                pbtn.dataset.act = 'eq-preset';
                pbtn.dataset.presetName = p.name;
                pbtn.textContent = (p.name === curName && isOn('convEq') ? '✓ ' : ' ') + p.name;
                eqPresetsEl.appendChild(pbtn);
            }
        }
        m.style.display = 'block';
        const r = m.getBoundingClientRect();
        m.style.left = Math.min(e.clientX, innerWidth - r.width - 8) + 'px';
        m.style.top = Math.min(e.clientY, innerHeight - r.height - 8) + 'px';
    });
    // The corner chip is a span inside the button, so it cannot take focus of
    // its own; from the keyboard the arrows on the focused SUB row turn it.
    field.addEventListener('keydown', (e) => {
        if ((e.key === 'Enter' || e.key === ' ' || e.key.startsWith('Arrow')) && held(e)) return;
        // HP's unit is a div, not a button: Enter and Space are said here,
        // to the unit itself or to its α switch.
        if ((e.key === 'Enter' || e.key === ' ') && e.target.closest('.hpu')) {
            if (e.target.matches('[data-alpha]')) { e.preventDefault(); flipAlpha(); }
            else if (e.target.matches('.hpu')) { e.preventDefault(); hide(); toggle('convHybridPhase'); }
            return;
        }
        // The SET key takes focus of its own but sits inside the button,
        // which does not hear Enter or Space meant for it: said here.
        if ((e.key === 'Enter' || e.key === ' ') && e.target.matches('[data-gear]')) {
            e.preventDefault();
            e.target.click();
            return;
        }
        if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
        const b = e.target.closest('.lab-badge');
        const id = b && b.dataset.id;
        if (!id || !FEAT_BY_ID[id].amount) return;
        e.preventDefault();
        cycleAmount(id, e.key === 'ArrowLeft' ? -1 : 1);
    });
    // The menu goes on a press outside it — the right button's too, on
    // another badge as well — and on Esc; that press goes nowhere else
    // (popups.js).
    addPopup({
        // Open is shown: a menu left 'block' under a hidden page is not
        // there to put away, and must not take the next press.
        isOpen: () => $('labMenu')?.style.display === 'block' && !!$('labMenu').getClientRects().length,
        inside: t => !!$('labMenu')?.contains(t),
        close: closeMenu,
    });
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hide(); });

    // Unified tooltip system — tooltip.js owns the event wiring.
    // Badge tip builder is injected here because FEAT_BY_ID and xtcSummary
    // are private to this module.
    initTooltip((b) => {
        const f = FEAT_BY_ID[b.dataset.id];
        if (!f) return null;
        let ex = '';
        if (f.needs) ex += `<span class="hint">Needs ${FEAT_BY_ID[f.needs].tok} — turning this on turns that on.</span>`;
        if (f.needsSelect) ex += `<span class="hint">Needs the ${f.needsSelect.label} control set to something other than Off.</span>`;
        if (f.conflicts) ex += `<span class="hint">Cannot run with ${FEAT_BY_ID[f.conflicts].tok}.</span>`;
        if (b.dataset.id === 'convLabTfs' && tfsAsked.why) ex = `<span class="hint">${tfsAsked.why}. Plays and converts in linear phase.</span>` + ex;
        if (f.setup && b.dataset.id !== 'convEq') ex += `<span class="hint">${xtcSummary()}</span>`;
        if (b.dataset.id === 'convEq' && speakersText()) ex += `<span class="hint">${speakersText()}</span>`;
        // Held by the player while a track is on (player.js setHeld).
        if (b.dataset.holdTip) ex += `<span class="hint">${b.dataset.holdTip}</span>`;
        return `<b style="color:var(--lab-${f.hue})">${f.tok} · ${f.name}</b>${f.tip}${ex}`;
    });

    // TFS reads dim at a tap count this machine cannot derive it for
    // (`tfsWhyNow`): the sliders that move the taps repaint it.
    for (const id of ['convTapSlider', 'convFsSlider']) $(id)?.addEventListener('input', () => paint());

    paint();
    // The rack is laid out by the grid, so there is no geometry to re-read on
    // resize. The one thing still worth a beat is the window: if the text
    // renders larger than main.js assumed at boot, the rack is taller than the
    // window it was sized for.
    setTimeout(() => window.fitWindowToContent?.(), 300);
}

/// Re-read the checkboxes — after settings are restored, or anything else
/// that writes them behind the rack's back.
///
/// It reconciles before it paints, because a restored blob is not necessarily
/// consistent: a saved state with Adaptive Headroom on and Headroom since put
/// back to Off would come up as a lit badge over a control that gives it
/// nothing to do. Reconciling is idempotent — with nothing to correct this is
/// a plain repaint.
export function syncDspRack() { selectChanged(); }

/// Grey the whole rack out while a batch runs.
export function setDspRackEnabled(on) {
    $('labField')?.classList.toggle('locked', !on);
    if (!on) hide();
}

// ══════════════════════════════════════════════════════════════════════
// The same chain, drawn under a file in the queue.
//
// Three states, and each of them has to be true rather than assumed: the
// stage ran, it was reached and decided against — the tooltip then says why
// — or it has not been reached yet. The backend fills `f.labChain` as it
// goes, so a stage with no entry is genuinely still ahead of the file, not a
// stage we forgot to ask about.
//
// VERIFIED sits last, behind a divider: it is a verdict on the output, not a
// stage of the chain.
// ══════════════════════════════════════════════════════════════════════

const norm = t => String(t).replace('α', 'a');
const esc = s => String(s).replace(/&/g, '&amp;').replace(/"/g, '&quot;')
    .replace(/</g, '&lt;').replace(/>/g, '&gt;');

/// A stage badge: the one markup every place draws — a row of the list, the
/// player's line, the big player — in the one look of dsprack.css `.cb`.
/// `why` is the tooltip's reason (left out when null); `style` is added to
/// the inline style (the player's pulse phase).
export function cb(cls, tok, name, hue, why, style = '') {
    const whyAttr = why == null ? '' : ` data-lab-why="${esc(why)}"`;
    return `<span class="cb${cls ? ' ' + cls : ''}" style="--h:var(--lab-${hue})${style ? ';' + style : ''}"${whyAttr}`
        + ` data-lab-tok="${esc(tok)}" data-lab-name="${esc(name)}"`
        + ` data-lab-hue="var(--lab-${hue})">${esc(tok)}</span>`;
}

export function chainBadgesHtml(f) {
    if (f.badge === 1) {
        return cb('failed', 'BAD', 'Unsupported sample rate', 'dc',
            'Non-standard sample rate — the file was skipped. Only the 44.1k and 48k families are supported.');
    }
    if (f.badge === 2) {
        return cb('skipped', 'SKIP', 'Nothing to upsample', 'ahr',
            f.badgeHint || 'Source sample rate is at or above the selected FS target.');
    }

    const report = new Map((f.labChain || []).map(r => [norm(r[0]), [r[1], r[2]]]));
    const out = [];
    for (const id of DSP_CHAIN) {
        const ft = FEAT_BY_ID[id];
        const rep = report.get(norm(ft.tok));
        // A record carries the rack its batch started with (the rack stays
        // live during a batch); only a record without one reads the rack now.
        const enabled = f.rack ? f.rack.includes(id) : !!$(id)?.checked;
        if (!rep && !enabled) continue;                 // not asked for at all
        if (!rep) {
            out.push(cb('', ft.tok, ft.name, ft.hue, 'Enabled — waiting its turn in the chain.'));
            continue;
        }
        const [st, why] = rep;
        const cls = st === 1 ? 'fired' : st === 3 ? 'failed' : 'skipped';
        const fallback = st === 1 ? 'Ran and changed the audio.'
            : st === 3 ? 'Ran and failed.'
            : 'Reached, and decided there was nothing to do.';
        out.push(cb(cls, ft.tok, ft.name, ft.hue, why || fallback));
    }

    const done = f.status === 'done' && f.batch !== 'next';
    let vcls = '', vtok = 'VERIFIED', vwhy = 'Runs after encoding, once there is an output to re-decode.';
    if (f.badge === 3) {
        vcls = 'failed'; vtok = '✗ VERIFIED';
        vwhy = 'The re-decoded output does not match the DSP result. The file may still be usable but cannot be confirmed lossless.';
    } else if (done) {
        vcls = 'fired';
        vwhy = 'Bit-perfect: the re-decoded output matches the DSP result sample for sample.';
    }
    out.push('<span class="cbsep"></span>');
    out.push(cb('term ' + vcls, vtok, 'Bit-perfect check', 'xtc', vwhy));
    return out.join('');
}

// Guarded so the module still imports outside a browser: the standing tests
// exercise the pure parts of this file under `node --test`, where there is no
// window to hang anything on.
if (typeof window !== 'undefined') window.__dspSyncRack = syncDspRack;
