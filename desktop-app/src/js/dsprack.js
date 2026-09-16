// ══════════════════════════════════════════════════════════════════════
// Advanced DSP as a rack: one lit row per stage, top to bottom in the order
// the pipeline runs them — the rows the lab build uses, for the stages the
// release has.
//
//   ▌SUB  Subsonic Filter             ON  [20 Hz]
//   ▌AA   Adaptive Apodizer           auto cutoff
//   ▌PFR  Polyphase FIR Resampling    ON
//   ▌HP   Hybrid-Phase Blending       2× time
//
// The rows are the interface; the checkboxes behind them are still in the
// DOM, hidden, keeping their ids — so settings.js saves and restores them,
// converter.js reads them when a run starts, and every existing `change`
// listener (locking the apodizing select, the window select, the custom
// filter button, saving the settings) fires untouched. A row sets `.checked`
// and dispatches `change`; nothing downstream can tell the difference.
//
// The subsonic corner is a chip on its row, like the de-limiter's depth in
// the lab: one number with three useful values, cycled by clicking it. It
// lives in a hidden input for the same reason the checkboxes do.
// ══════════════════════════════════════════════════════════════════════

/// Corners the backend accepts (apodize.rs SUBSONIC_CORNERS_HZ), in the order
/// a click walks them: from the default down, then round again.
export const SUBSONIC_STEPS = [20, 15, 10];

const RACK = [
    { id: 'convSubsonic', tok: 'SUB', hue: '#60a5fa', name: 'Subsonic Filter',
      corner: 'convSubsonicHz',
      tip: 'Linear-phase high-pass below the corner on the chip — click the chip for 20, 15 or 10 Hz. '
         + 'Flat from the corner up, so nothing above it changes in level or in timing; at least 100 dB down '
         + 'from half the corner to DC. For the infrasonic content and slow drift a disc can carry, which removing '
         + 'the DC offset does not touch. Protection for the speakers, not a sound improvement.' },
    { id: 'convAdaptiveApodizer', tok: 'AA', hue: '#34d399', name: 'Adaptive Apodizer', note: 'auto cutoff',
      tip: 'Per-file source forensics: detects ADC/SRC pre-ringing and measures its exact frequency, unmasks fake '
         + 'hi-res (upsampled masters) in any container rate — including mirror-image aliasing from bad resamplers — '
         + 'and auto-sets the optimal cutoff. Leaves clean and minimum-phase sources untouched; falls back to the '
         + 'static preset when nothing is detected.' },
    { id: 'convFirResampling', tok: 'PFR', hue: '#facc15', name: 'Polyphase FIR Resampling', note: 'direct path',
      tip: 'Integrated polyphase resampling: your FIR filter IS the resampler — no intermediate library resampler '
         + 'in the chain. The whole conversion is one exact convolution of the original samples with the '
         + '128-bit-designed filter. Integer ratios only (FS2/4/8/16 within the same rate family).' },
    { id: 'convHybridPhase', tok: 'HP', hue: '#a78bfa', name: 'Hybrid-Phase Blending', note: '2× time',
      tip: 'Linear phase for sustained sections, minimum phase across detected attacks, switched at a zero crossing '
         + 'with a ~32-sample raised-cosine micro-fade. The trigger is an onset detector, not a measurement of filter '
         + 'ringing. Requires 2× processing time.' },
];
const BY_ID = Object.fromEntries(RACK.map(f => [f.id, f]));

const $ = id => document.getElementById(id);
const isOn = id => !!$(id)?.checked;

/// The corner the chip shows. A value that is not one of the steps — an old
/// or hand-edited settings blob — reads as the default rather than reaching
/// the backend, which would refuse it.
function cornerOf(f) {
    const v = parseInt($(f.corner)?.value, 10);
    return SUBSONIC_STEPS.includes(v) ? v : SUBSONIC_STEPS[0];
}

/// What converter.js sends as `subsonicHz`: 0 when the filter is off.
export function subsonicCornerHz() {
    const f = BY_ID.convSubsonic;
    return isOn(f.id) ? cornerOf(f) : 0;
}

function cycleCorner(f, dir = 1) {
    const el = $(f.corner);
    if (!el || el.disabled) return;
    const n = SUBSONIC_STEPS.length;
    const i = SUBSONIC_STEPS.indexOf(cornerOf(f));
    el.value = String(SUBSONIC_STEPS[(i + dir + n) % n]);
    el.dispatchEvent(new Event('change', { bubbles: true }));
    paint();
}

function toggle(id) {
    const el = $(id);
    if (!el || el.disabled) return;
    el.checked = !el.checked;
    el.dispatchEvent(new Event('change', { bubbles: true }));
    paint();
}

function paint() {
    document.querySelectorAll('.rack-badge').forEach(b => {
        const f = BY_ID[b.dataset.id], on = isOn(f.id);
        b.classList.toggle('on', on);
        b.setAttribute('aria-pressed', String(on));
        const st = b.querySelector('.st');
        if (st) {
            st.textContent = on ? 'ON' : (f.note || '');
            st.classList.toggle('dim', !on);
        }
        const amt = b.querySelector('.amt');
        if (amt) {
            amt.textContent = cornerOf(f) + ' Hz';
            b.setAttribute('aria-label', `${f.name}, corner ${cornerOf(f)} Hz — arrow keys change the corner`);
        }
    });
}

// ── tooltip ───────────────────────────────────────────────────────────
let tip = null;
function showTip(f, x, y) {
    if (!tip) return;
    tip.innerHTML = '';
    const head = document.createElement('b');
    head.style.color = f.hue;
    head.textContent = `${f.tok} · ${f.name}`;
    tip.append(head, document.createTextNode(f.tip));
    tip.classList.add('show');
    const r = tip.getBoundingClientRect();
    tip.style.left = Math.max(6, Math.min(x + 14, innerWidth - r.width - 8)) + 'px';
    tip.style.top = Math.max(6, Math.min(y + 16, innerHeight - r.height - 8)) + 'px';
}
const hideTip = () => tip && tip.classList.remove('show');

function build(rack) {
    rack.textContent = '';
    for (const f of RACK) {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = 'rack-badge';
        b.dataset.id = f.id;
        b.style.setProperty('--h', f.hue);
        b.setAttribute('aria-pressed', 'false');
        b.setAttribute('aria-label', f.name);
        b.innerHTML = '<i class="bar"></i>'
            + `<span class="tok">${f.tok}</span>`
            + `<span class="nm">${f.name}</span>`
            + '<span class="st"></span>'
            + (f.corner ? '<span class="amt" data-amt="1" title="Corner frequency — click to change"></span>' : '');
        rack.appendChild(b);
    }
}

/// Build the rack and wire it. Does nothing where there is no rack element,
/// so a page that brings its own panel (the lab build) is left alone.
export function initDspRack() {
    const rack = $('dspRack');
    if (!rack) return;
    build(rack);

    if (!$('rackTip')) {
        tip = document.createElement('div');
        tip.id = 'rackTip';
        tip.setAttribute('role', 'tooltip');
        document.body.appendChild(tip);
    } else {
        tip = $('rackTip');
    }

    rack.addEventListener('click', (e) => {
        const b = e.target.closest('.rack-badge');
        if (!b) return;
        hideTip();
        const f = BY_ID[b.dataset.id];
        // The chip is inside the button, so it has to be read before the
        // click is taken as a toggle: changing the corner must not also
        // switch the filter.
        if (f.corner && e.target.closest('[data-amt]')) { cycleCorner(f); return; }
        toggle(f.id);
    });
    rack.addEventListener('mouseover', (e) => {
        const b = e.target.closest('.rack-badge');
        if (b) showTip(BY_ID[b.dataset.id], e.clientX, e.clientY);
        else hideTip();   // the plate's padding and the gaps between rows
    });
    // The corner chip is a span inside the button, so it cannot take focus
    // of its own; from the keyboard the arrows on the focused SUB row turn it.
    rack.addEventListener('keydown', (e) => {
        if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
        const b = e.target.closest('.rack-badge');
        const f = b && BY_ID[b.dataset.id];
        if (!f || !f.corner) return;
        e.preventDefault();
        cycleCorner(f, e.key === 'ArrowLeft' ? -1 : 1);
    });
    rack.addEventListener('mouseleave', hideTip);
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hideTip(); });

    paint();
}

/// Repaint from the checkboxes — after settings are restored, or anything
/// else that writes them behind the rack's back.
export function syncDspRack() { paint(); }

/// Grey the whole rack out while a batch runs.
export function setDspRackEnabled(on) {
    $('dspRack')?.classList.toggle('locked', !on);
    if (!on) hideTip();
}
