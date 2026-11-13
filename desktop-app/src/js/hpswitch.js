// Hybrid-Phase's unit at a track switch: its background (hpview.js) goes out
// quickly when the old sound stops and comes back with the new track's
// picture once that track is heard. With the next track's pair ready nothing
// else showed the switch: the old curve stood frozen, the clock held over the
// jump, till the new track's first answer (Anton 2.11: "it looks hung").
//
// The model is pure (status and time in, a phase out) so the tests drive it
// without a page; the class on the unit (dsprack.css `.hpu.swout`, `.swin`)
// does the fading. A Hybrid-Phase fill (hpprep.js, `.prep`) hides the view
// on its own meanwhile: out → fill → in.

/// The fades, ms (dsprack.css's transitions).
export const OUT_MS = 200, IN_MS = 250;
/// The old track this close to its end, s: a new track playing at once is
/// the gapless hand-over, not a press.
export const END_S = 2;
/// A gapless hand-over waits this long for the new track's picture, ms; then
/// the view goes out rather than stand frozen.
export const GAP_WAIT_MS = 400;

/// Phases: 'on' (shown) → 'out' (gone till the new track is heard and its
/// picture comes) → 'in' (fading back, IN_MS) → 'on'; a gapless hand-over
/// 'waits' GAP_WAIT_MS for its picture, shown, and goes 'out' without it.
export function createSwitchFade() {
    let phase = 'on', at = 0, id = null, nearEnd = false, heardAt = null;
    const go = (p, t) => { phase = p; at = t; };
    return {
        get phase() { return phase; },
        /// The view hidden now?
        get out() { return phase === 'out'; },
        /// Every player status at page time `t`.
        status(s, t) {
            const nid = s?.trackId ?? null;
            const heard = s?.state === 'playing' && !s?.jump?.pending;
            if (nid !== id && nid != null) {
                // Gapless: the track before ended into this one, no jump.
                if (id != null && heard && nearEnd) { if (phase === 'on') go('wait', t); }
                else go('out', t);
                heardAt = null;
            }
            id = nid;
            const pos = s?.positionS, dur = s?.durationS;
            nearEnd = Number.isFinite(pos) && Number.isFinite(dur) && dur > 0 && dur - pos <= END_S;
            if (phase === 'out') heardAt = heard ? (heardAt ?? t) : null;
            else if (phase === 'wait') heardAt ??= at;
            this.tick(t);
        },
        /// An answer with a picture, its request sent at `sent`: one asked
        /// for once the new track is heard brings the view back.
        fresh(sent, t) {
            if ((phase === 'out' || phase === 'wait') && heardAt != null && sent >= heardAt) go(phase === 'out' ? 'in' : 'on', t);
            this.tick(t);
        },
        tick(t) {
            if (phase === 'wait' && t - at >= GAP_WAIT_MS) { go('out', t); heardAt = t; }
            else if (phase === 'in' && t - at >= IN_MS) go('on', t);
        },
    };
}

let model = null;

const show = () => {
    const u = document.querySelector('.hpu');
    if (!u) return;
    u.classList.toggle('swout', model.out);
    u.classList.toggle('swin', model.phase === 'in');
};

/// Every player status (player.js takeStatus).
export function hpSwitchStatus(s) {
    model ??= createSwitchFade();
    model.status(s, performance.now());
    show();
}

/// hpview.js's answer with a picture, its request sent at page time `sent`.
export function hpSwitchFresh(sent) {
    if (!model) return;
    model.fresh(sent, performance.now());
    show();
}
