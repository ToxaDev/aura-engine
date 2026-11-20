// ══════════════════════════════════════════════════════════════════════
// tip-timing.js — when a tooltip shows. Shared by the main window
// (tooltip.js) and the analyzer window (analytics/window.js), so both
// behave the same.
//
// Cold: the pointer rests on something with a tooltip for COLD_DELAY_MS
// before it shows. Warm: while a tooltip is up, and for WARM_MS after it
// went away, the next one shows at once — moving along a row of buttons
// reads each without waiting again. Pointer away longer than that: cold.
// A click or a key is an action, not a look around: it hides the tooltip
// cold.
// ══════════════════════════════════════════════════════════════════════

export const COLD_DELAY_MS = 1500;
export const WARM_MS = 1000;

export function createTipTiming() {
    let up = false;     // a tooltip is showing
    let goneAt = 0;     // when the last one that was showing went away

    return {
        /** How long the next tooltip waits. */
        delay() {
            return up || Date.now() - goneAt < WARM_MS ? 0 : COLD_DELAY_MS;
        },
        shown() { up = true; },
        /** The tooltip went away; `cold` after a click or a key. */
        gone(cold = false) {
            if (up && !cold) goneAt = Date.now();
            if (cold) goneAt = 0;
            up = false;
        },
    };
}
