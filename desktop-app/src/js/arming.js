// What each stage of the rack is doing on the air: heard, switching in or
// switching out. One answer for the three places that show it — the badges
// in the player's status line, the breathing of the rack's own badges, and
// the thin bar under the status line — so they never disagree.
//
// No DOM here: the page hands in what it knows (the audible chain from the
// player's status, the chain's stages, which of them are on in the rack).

/// Token as the player's chain reports it, matched to the rack's: α → a, and
/// the subsonic corners (SUB10, SUB15, SUB20) to the rack's one "SUB".
export const normTok = t => String(t).replace('α', 'a').replace(/^SUB\d+$/, 'SUB');

/// Whether the chain on the air (the status's `audible`) follows the rack. A
/// converted file ('file') and BIT-PERFECT ('direct') do not: what plays is
/// the file as it is, so no stage switches in or out there — and none while
/// nothing plays.
export const switchesLive = audible => !!audible && audible.source !== 'file' && audible.source !== 'direct';

/// The stages of `stages` ([{ id, tok }], in chain order) that are on in the
/// rack (`isOn(id)`) or heard in `audible`, each as `{ id, tok, state, st, why }`:
/// - 'on': heard and on in the rack (`st`, `why` as the chain reports them);
/// - 'pending': on in the rack, not heard yet — switching in;
/// - 'leaving': still heard, off in the rack — switching out.
/// Nothing when the air does not follow the rack (switchesLive).
export function stageStates(audible, stages, isOn) {
    if (!switchesLive(audible)) return [];
    const heard = new Map((audible.stages || []).map(([tok, st, why]) => [normTok(tok), [st, why]]));
    const out = [];
    for (const { id, tok } of stages) {
        const h = heard.get(normTok(tok));
        const on = !!isOn(id);
        if (!h && !on) continue;
        const state = h && on ? 'on' : on ? 'pending' : 'leaving';
        out.push({ id, tok, state, st: h ? h[0] : null, why: h ? h[1] : '' });
    }
    return out;
}

/// Some stage is switching in or out.
export const anyWaiting = states => states.some(s => s.state === 'pending' || s.state === 'leaving');

/// The track is being made ready before its first sound (the player's
/// status): nothing of it is heard yet.
export const preparingBeforeSound = s =>
    !!s && (s.state === 'preparing' || (!!s.jump?.pending && s.pending != null));

/// Nothing heard yet, the rack's chain on its way: every stage the rack has
/// on is switching in, and so are its taps.
export const NOTHING_HEARD = Object.freeze({ source: 'live', stages: Object.freeze([]), quick: false, taps: null, downgrade: null });

/// The chain the badges, the rack's own badges and the bar count with: the
/// one heard (the status's `audible`) — or NOTHING_HEARD while the track is
/// being made ready before its first sound and will play through the rack
/// (`throughRack`: not BIT-PERFECT, not a converted file). Its badges used
/// to be missing until the sound, then came all at once, and the bar that
/// had run under the whole line ended again under them (Anton 1.10).
export function airOf(s, throughRack) {
    if (s?.audible) return s.audible;
    return throughRack && preparingBeforeSound(s) ? NOTHING_HEARD : null;
}

/// The bar under a stream's status line (the status's `radio`): while it
/// tunes in, the stream its chain waits for, filled as it comes (inS of
/// needS); while the stream's own filter (`preparing`) or a new chain for a
/// rack change on the air (`switching`) is being made, no share is known —
/// the same bar with a fill that runs. Null when nothing is on its way.
export function radioBar(r) {
    if (!r) return null;
    const tuning = r.phase === 'connecting' || r.phase === 'waiting';
    const making = !!r.preparing || r.switching != null;
    if (!tuning && !making) return null;
    const need = r.info?.needS || 0;
    const known = tuning && need > 0 && !r.preparing;
    return {
        seq: `radio@${r.info?.url ?? ''}@${r.info?.tPlannedMs ?? ''}@${r.switching ?? ''}`,
        frac: known ? Math.min(1, (r.inS || 0) / need) : 0,
        busy: true,
        run: !known,
    };
}

/// What the status line says in the badges' place while BIT-PERFECT is on
/// the air (`aud`, airOf): a quiet line while it plays — nothing while it
/// does not, nor while a warning (`warned`) stands: it says more.
export const BIT_PERFECT_LINE = 'Bit-perfect: samples reach the DAC unaltered';
export const bitPerfectLine = (s, aud, warned) =>
    (s?.state === 'playing' && aud?.source === 'direct' && !warned ? BIT_PERFECT_LINE : '');

// ── The taps chip ───────────────────────────────────────────────────────

/// A filter size as the player labels it (filter.rs taps_label): 5k, 1M,
/// 5M, 10M, 30M; null below the smallest.
export function tapsLabel(n) {
    if (!(n >= 2500)) return null;
    return n >= 25e6 ? '30M' : n >= 7.5e6 ? '10M' : n >= 2.5e6 ? '5M' : n >= 5e5 ? '1M' : '5k';
}

/// The taps chip for the chain `air` (airOf) and the rack's taps:
/// { label, switching }, null when none shows (nothing that follows the
/// rack, or a chain of no known size). Switching while the chain heard is
/// not built for the rack's taps: the new filter's swap not heard yet, or
/// nothing heard yet — its label is then the rack's, the size on its way.
/// A step down the player made (power, GPU: `downgrade`) is the rack's
/// chain at fewer taps: it counts as the size the rack asked for (`asked`:
/// the first step's `from`, a second step's being the size heard), and
/// switches only when the rack asks for another. `onItsWay`: something of
/// the rack is still on its way (a change to send, a rebuild, the player's
/// arming at work); with nothing coming (the new chain's build failed) the
/// chip is the plain one of the size heard, not a switch that never ends.
export function tapsChip(air, rackTaps, onItsWay = true) {
    if (!switchesLive(air)) return null;
    const want = tapsLabel(rackTaps);
    if (air.taps == null) return air === NOTHING_HEARD && want ? { label: want, switching: onItsWay } : null;
    const builtFor = air.downgrade ? (air.downgrade.asked ?? air.downgrade.from) : air.taps;
    const switching = want != null && builtFor !== want && onItsWay;
    return { label: switching ? want : air.taps, switching };
}

/// A stage or the taps are switching in or out: the bar has something to
/// show.
export const waitsOnAir = (states, chip) => anyWaiting(states) || !!chip?.switching;

// ── The bar under the badges ────────────────────────────────────────────
// Shown while a stage or the taps are switching in or out (all of them
// while the track is made ready before its first sound: NOTHING_HEARD, the
// badges already under the bar) and the player still has work on its way
// (the status's `arming`, arming.rs). It fills with that work; when no stage
// waits any more it runs to full and goes. When the player has nothing more
// on its way but a stage still waits (Hybrid-Phase held for the next track,
// a stage the player cannot run), it goes as it stands: nothing is coming.

/// The last stretch to full once the last stage is on (ms).
export const BAR_FILL_MS = 350;
/// The fade out (ms; player.css's transition on .pl-arm).
export const BAR_FADE_MS = 500;
/// A stage waits with nothing on its way this long before the bar goes.
export const BAR_IDLE_MS = 1500;

const HIDDEN = Object.freeze({ phase: 'hidden', seq: null, frac: 0, at: 0, idle: null, reset: false, q: false });

/// The bar's next state, one poll on from `prev` (null at first): `waiting`,
/// a stage or the taps switch in or out (waitsOnAir), or a track is being
/// made ready before its first sound; `arming`, the
/// status's arming ({ seq, frac, busy }) or null — or, while a rack change
/// has not gone to the player yet, one the page makes up ({ queued: true,
/// frac 0 }); `now` in milliseconds.
/// State: { phase: 'hidden' | 'shown' | 'finishing' | 'fading', seq, frac,
/// at (when the phase began), idle (since when nothing is on its way while a
/// stage waits), reset (the fill jumps rather than glides: a new arming),
/// q (a queued change: nothing done yet) }.
export function barStep(prev, { waiting, arming, now }) {
    const s = prev || HIDDEN;
    const busy = !!arming && !!arming.busy && waiting;
    const fresh = () => ({ phase: 'shown', seq: arming.seq, frac: arming.frac, at: now, idle: null, reset: true, q: !!arming.queued });
    switch (s.phase) {
    case 'shown':
        if (!arming) return { ...s, phase: 'fading', at: now, idle: null, reset: false };
        // No stage waits: done — or, for a change that never went out (a
        // stage clicked back), nothing to fill.
        if (!waiting) return s.q
            ? { ...s, phase: 'fading', at: now, idle: null, reset: false }
            : { ...s, phase: 'finishing', frac: 1, at: now, idle: null, reset: false };
        if (arming.seq !== s.seq && arming.busy) return fresh();
        if (!arming.busy) {
            const idle = s.idle ?? now;
            return now - idle >= BAR_IDLE_MS
                ? { ...s, phase: 'fading', at: now, idle: null, reset: false }
                : { ...s, idle, reset: false };
        }
        return { ...s, frac: Math.max(s.frac, arming.frac), idle: null, reset: false };
    case 'finishing':
        if (busy && arming.seq !== s.seq) return fresh();
        return now - s.at >= BAR_FILL_MS ? { ...s, phase: 'fading', at: now, reset: false } : { ...s, reset: false };
    case 'fading':
        if (busy) return arming.seq === s.seq && s.frac < 1
            ? { ...s, phase: 'shown', frac: Math.max(s.frac, arming.frac), at: now, idle: null, reset: false }
            : fresh();
        return now - s.at >= BAR_FADE_MS ? HIDDEN : { ...s, reset: false };
    default:
        return busy ? fresh() : HIDDEN;
    }
}

/// What the page draws for a bar state: seen or fading out, and the fill.
export const barView = s => ({
    on: s.phase === 'shown' || s.phase === 'finishing',
    present: s.phase !== 'hidden',
    frac: s.phase === 'hidden' ? 0 : s.frac,
    reset: !!s.reset,
});
