// ══════════════════════════════════════════════════════════════════════
// Advanced DSP as a rack of stages, grouped into the three zones the
// pipeline runs them in, and inside each zone in the order it runs them —
// one row per stage, with its name and its state written out:
//
//   ┌ 1 SOURCE ─────────────┐ ┌ 2 CONVERSION ─────────┐
//   │ ▌DC   Declip       ON │ │ ▌AA   Apodizer     ON │
//   │ ▌ISP  Intersample     │ │ ▌PFR  Polyphase    ON │
//   │ ▌SUB  Subsonic  20 Hz │ │ ▌HP   Hybrid    ✕ TFS │
//   │ ▌AHR  Headroom ←Head. │ │ ▌αHP  Cont. Alpha ←HP │
//   └───────────────────────┘ │ ▌TFS  TFS Phase    ON │
//   ┌ 3 OUTPUT ─────────────┐ └───────────────────────┘
//   │ ▌XTC  Crosstalk  SET… │
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
// listener (locking the apodizing select, the window select, the custom
// filter button) fires untouched. A row sets `.checked` and dispatches
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

/// Strict pipeline order — prepare.rs, then process.rs. The zone layout and
/// the per-file chain in the queue both read from here.
export const DSP_CHAIN = [
    'convLabDeclip', 'convLabIsp', 'convSubsonic', 'convLabHeadroom',
    'convAdaptiveApodizer', 'convFirResampling', 'convHybridPhase', 'convLabAlpha', 'convLabTfs',
    'convLabXtc',
];

/// Where the chain is cut into zones. `n` is how many stages of DSP_CHAIN
/// belong to each, in order, so the three plates are exactly the chain.
export const DSP_ZONES = [
    { title: 'Source',     n: 4, note: 'runs on the source, before the filter sees it' },
    { title: 'Conversion', n: 5, note: 'the filter itself and the phase it is rendered in' },
    { title: 'Output',     n: 1, note: 'runs on the finished render' },
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
    { id: 'convLabAlpha', tok: 'αHP', hue: 'ahp', name: 'Continuous Alpha', needs: 'convHybridPhase',
      tip: 'Drops the hard switch in Hybrid-Phase for a per-sample crossfade driven by the HPSS envelope. Needs Hybrid-Phase.' },
    { id: 'convLabTfs', tok: 'TFS', hue: 'tfs', name: 'TFS Phase', conflicts: 'convHybridPhase',
      tip: 'Linear phase below 1.5 kHz, minimum phase above 4 kHz, blended between. Needs both filter blobs on disk. Replaces Hybrid-Phase for the file.' },

    { id: 'convLabXtc', tok: 'XTC', hue: 'xtc', name: 'Crosstalk Cancellation', setup: true,
      tip: 'Cancels the sound each speaker sends to the wrong ear, so the stereo image is no longer bounded by the speakers. Built from the triangle you measure — correct only for that seat, and only while your head stays in it. For speakers, never headphones.' },
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
};

export const FEAT_BY_ID = Object.fromEntries(FEAT.map(f => [f.id, f]));
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
function stateOf(id) {
    const f = FEAT_BY_ID[id];
    if (isOn(id)) return { text: 'ON', cls: '' };
    // Before its state, a stage waiting on configuration says what it wants.
    // Nothing else in the rack can be blocked by something outside the file.
    if (f.setup && !isXtcConfigured()) return { text: 'SET…', cls: 'unset' };
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
        // the dot marks a stage that is only up because something needed it
        b.classList.toggle('auto', !!(f.needs && on));
        const st = stateOf(id);
        b.classList.toggle('blocked', st.cls === 'blocked');
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
    });
    checkInvariants();
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

// ── tooltip ───────────────────────────────────────────────────────────
let tip = null;
function showTip(html, x, y) {
    if (!tip) return;
    tip.innerHTML = html;
    tip.classList.add('show');
    const r = tip.getBoundingClientRect();
    tip.style.left = Math.max(6, Math.min(x + 14, innerWidth - r.width - 8)) + 'px';
    tip.style.top = Math.max(6, Math.min(y + 16, innerHeight - r.height - 8)) + 'px';
}
const hideTip = () => tip && tip.classList.remove('show');

// ── menu ──────────────────────────────────────────────────────────────
let menuTarget = null;
function closeMenu() { const m = $('labMenu'); if (m) m.style.display = 'none'; }

/// One plate per zone, one row per pipeline stage.
///
/// Zones pair up two to a row and an odd one spans both columns, which for
/// the current 4/5/1 puts Source beside Conversion and Output underneath.
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
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'lab-badge';
            b.dataset.id = id;
            b.style.setProperty('--h', `var(--lab-${f.hue})`);
            b.innerHTML = '<i class="bar"></i>'
                + `<span class="tok">${f.tok}</span>`
                + `<span class="nm">${labelOf(f)}</span>`
                + '<span class="st"></span>'
                + (f.setup ? '<span class="gear" data-gear="1" title="Listening geometry…">⚙</span>' : '')
                + (f.amount ? `<span class="amt" data-amt="1" title="${f.amount.title}"></span>` : '')
                + '<span class="dep"></span>';
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
        tip = document.createElement('div');
        tip.id = 'labTip';
        tip.setAttribute('role', 'tooltip');
        document.body.appendChild(tip);
        const m = document.createElement('div');
        m.id = 'labMenu';
        m.innerHTML = '<button data-act="toggle">⏻ <span id="labMToggle">Enable</span></button>'
            + '<button data-act="solo">◎ Only this one</button>'
            + '<button data-act="setup" id="labMSetup">⚙ Geometry…</button>';
        document.body.appendChild(m);
        m.addEventListener('click', (e) => {
            const act = e.target.closest('button')?.dataset.act;
            if (!act || !menuTarget) return;
            closeMenu();
            if (act === 'toggle') toggle(menuTarget);
            if (act === 'solo') solo(menuTarget);
            if (act === 'setup') openXtcGeometry(paint);
        });
    } else {
        tip = $('labTip');
    }

    buildZones();
    // The geometry window hands its result back over the event bus; this is the
    // ear for it. Registered once, before any badge can ask for that window.
    initXtcBridge(paint);

    // Adaptive Headroom leans on this select rather than on a badge, so the
    // rack has to hear it move. settings.js also repaints after it restores,
    // which covers the other way the value can change.
    $('convHeadroom')?.addEventListener('change', selectChanged);

    const field = $('labField');
    field.addEventListener('click', (e) => {
        const b = e.target.closest('.lab-badge');
        if (!b) return;
        hideTip();
        const id = b.dataset.id, f = FEAT_BY_ID[id];
        // The gear is inside the button, so it has to be read before the
        // click is treated as a toggle.
        if (e.target.closest('[data-gear]')) { openXtcGeometry(paint); return; }
        // The number chip is inside the button too, and cycling it must not
        // also flip the stage.
        if (e.target.closest('[data-amt]')) { cycleAmount(id); return; }
        // Switching on a stage that has no geometry yet would build the
        // filter for an invented room. Ask first, then come up if it saved.
        if (f.setup && !isOn(id) && !isXtcConfigured()) {
            openXtcGeometry(() => toggle(id));
            return;
        }
        toggle(id);
    });
    field.addEventListener('contextmenu', (e) => {
        const b = e.target.closest('.lab-badge');
        if (!b) return;
        e.preventDefault();
        hideTip();
        menuTarget = b.dataset.id;
        const m = $('labMenu');
        $('labMToggle').textContent = isOn(menuTarget) ? 'Disable' : 'Enable';
        $('labMSetup').style.display = FEAT_BY_ID[menuTarget].setup ? 'block' : 'none';
        m.style.display = 'block';
        const r = m.getBoundingClientRect();
        m.style.left = Math.min(e.clientX, innerWidth - r.width - 8) + 'px';
        m.style.top = Math.min(e.clientY, innerHeight - r.height - 8) + 'px';
    });
    // The corner chip is a span inside the button, so it cannot take focus of
    // its own; from the keyboard the arrows on the focused SUB row turn it.
    field.addEventListener('keydown', (e) => {
        if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
        const b = e.target.closest('.lab-badge');
        const id = b && b.dataset.id;
        if (!id || !FEAT_BY_ID[id].amount) return;
        e.preventDefault();
        cycleAmount(id, e.key === 'ArrowLeft' ? -1 : 1);
    });
    document.addEventListener('click', (e) => { if (!e.target.closest('#labMenu')) closeMenu(); });
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape') { closeMenu(); hideTip(); } });

    document.addEventListener('mouseover', (e) => {
        const b = e.target.closest('.lab-badge');
        if (b) {
            const f = FEAT_BY_ID[b.dataset.id];
            let ex = '';
            if (f.needs) ex += `<span class="hint">Needs ${FEAT_BY_ID[f.needs].tok} — turning this on turns that on.</span>`;
            if (f.needsSelect) ex += `<span class="hint">Needs the ${f.needsSelect.label} control set to something other than Off.</span>`;
            if (f.conflicts) ex += `<span class="hint">Cannot run with ${FEAT_BY_ID[f.conflicts].tok}.</span>`;
            if (f.setup) ex += `<span class="hint">⚙ ${xtcSummary()}</span>`;
            showTip(`<b style="color:var(--lab-${f.hue})">${f.tok} · ${f.name}</b>${f.tip}${ex}`,
                    e.clientX, e.clientY);
            return;
        }
        const c = e.target.closest('[data-lab-why]');
        if (c) {
            showTip(`<b style="color:${c.dataset.labHue || '#7dd3fc'}">${c.dataset.labTok} · `
                    + `${c.dataset.labName}</b>${c.dataset.labWhy}`, e.clientX, e.clientY);
            return;
        }
        hideTip();
    });
    document.addEventListener('mouseout', (e) => {
        if (e.target.closest('.lab-badge,[data-lab-why]')) hideTip();
    });

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
    if (!on) hideTip();
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

function cb(cls, tok, name, hue, why) {
    return `<span class="cb ${cls}" style="--h:var(--lab-${hue})" data-lab-why="${esc(why)}"`
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
        const enabled = !!$(id)?.checked;
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
