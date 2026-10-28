// A requestAnimationFrame loop that rests while it has nothing to draw.
//
// A loop that asks for every frame keeps the page and the GPU process
// producing frames at the display's rate (175 Hz on a gaming monitor) even
// when the picture does not move: the idle player cost ~22 % of a core in the
// GPU process for an empty canvas. `restingLoop(step)` calls `step(ts)` every
// frame while it returns true; once it returns false the loop looks again in
// `idleMs`, or at once on `wake()` or when the window is shown again.

const loops = new Set();
let lastActive = 0;

/// Something an overlay may show has changed (the pointer, a key, the
/// window's state, new data): overlay loops run again.
export function markActive() {
    lastActive = performance.now();
    for (const l of loops) l.wake();
}

/// New data came (the playhead clock sets it every frame while playing):
/// overlay loops on screen run again, but one resting because it is off
/// screen is left to its quarter-second look - waking it each frame only to
/// find it hidden kept a frame asked for every frame per hidden tab.
/// The pointer or a key (markActive) still wakes it at once.
export function markData() {
    lastActive = performance.now();
    for (const l of loops) if (!l.hidden) l.wake();
}

/// An overlay (playhead, hairline, selection) drawn every frame while the
/// window is active and for `graceMs` after; then once per rest (a quarter
/// second), so a change no one marked still shows within that time.
/// `el` given: while it is off screen (another tab) nothing is drawn and the
/// loop rests; the click that shows it wakes the loop again.
export function overlayLoop(draw, el = null, graceMs = 500) {
    const o = { hidden: false };
    const l = restingLoop(() => {
        o.hidden = !!el && el.offsetParent === null;
        if (o.hidden) return false;
        draw();
        return performance.now() - lastActive < graceMs;
    });
    o.wake = l.wake;
    o.stop = () => { loops.delete(o); l.stop(); };
    loops.add(o);
    return o;
}

/// An overlay with nothing to show that is already blank at this size is left
/// as it is: a clear every frame repainted the empty canvas at the display's
/// rate. Only the empty case is skipped - anything drawn is drawn every time,
/// so no input of a drawing can be missed. `keep(empty, w, h)` true = skip.
export function blankGuard() {
    let blank = false, bw = -1, bh = -1;
    return {
        keep(empty, w, h) {
            const keep = empty && blank && w === bw && h === bh;
            blank = empty; bw = w; bh = h;
            return keep;
        },
    };
}

export function restingLoop(step, idleMs = 250) {
    let timer = 0;
    let stopped = false;
    const run = ts => {
        if (stopped) return;
        if (step(ts) !== false) requestAnimationFrame(run);
        else timer = setTimeout(tick, idleMs);
    };
    const tick = () => { timer = 0; if (!stopped) requestAnimationFrame(run); };
    const wake = () => { if (timer) { clearTimeout(timer); tick(); } };
    document.addEventListener('visibilitychange', wake);
    requestAnimationFrame(run);
    return {
        wake,
        stop() { stopped = true; if (timer) clearTimeout(timer); timer = 0; document.removeEventListener('visibilitychange', wake); },
    };
}
