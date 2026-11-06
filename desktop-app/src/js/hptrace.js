// HP unit background trace, off unless the app was started with
// AURA_HP_TRACE=1 (the status's hpTrace): what could make the picture jerk,
// one line in the session's log per event and a summary every
// TRACE_SUMMARY_MS. Off, nothing is made and hpview.js only tests for null.

/// A frame's move off the even motion that counts as a jerk, screen-free
/// px (one bin = one CSS px); a page frame this long is slow (a few vsyncs
/// missed), this long a gap; an answer this long on its way is late; the
/// summary's period.
export const JERK_PX = 1.5;
export const SLOW_MS = 20;
export const GAP_MS = 40;
export const LATE_MS = 150;
export const TRACE_SUMMARY_MS = 10_000;
/// This many jerks in a row whose speeds differ by less than this (px/ms)
/// are the picture's new speed, not jerks.
export const RELEARN_FRAMES = 5;
export const RELEARN_SPREAD = 0.1;

const r1 = (x) => Math.round(x * 10) / 10;

/// A tracer writing through `log(line)`. `frame(ts, pos, x)`: a frame drawn
/// at page time `ts` with the picture at `pos` bins, drawn at `x` screen px
/// (the grid's bin 0 as drawStrip put it; the summary counts its steps:
/// 0, 1, 2, 3+ px); `clock(kind, info)`:
/// viewClock's snap/hold/on again; `answer(ms, kind)`: an answer `ms` on
/// its way, kind 'ok' | 'empty' | 'kept' | 'failed'; `note(text)`: the
/// player's own word (status not playing, protocol pausing).
export function makeTrace(log) {
    let lastTs = null, lastPos = null, lastX = null, vel = null, quietTill = 0, lastAns = null, run = [];
    let s = null;
    const reset = (ts) => { s = { t0: ts, frames: 0, gapMax: 0, slow: 0, gaps: 0, jerks: 0, jerkMax: 0, latMax: 0, late: 0, empty: 0, failed: 0, snaps: 0, holds: 0, px: [0, 0, 0, 0], pxSum: 0, pxN: 0, pxTurns: 0, pxLast: null }; };
    const since = (ts) => (lastAns == null ? '-' : `${Math.round(ts - lastAns)}ms`);
    return {
        frame(ts, pos, x) {
            if (!s) reset(ts);
            s.frames++;
            if (x != null && lastX != null) {
                const d = Math.round(Math.abs(lastX - x) * 1000) / 1000;
                s.px[Math.min(3, Math.round(d))]++; s.pxSum += d; s.pxN++;
                if (s.pxLast != null && d !== s.pxLast) s.pxTurns++;
                s.pxLast = d;
            }
            lastX = x ?? null;
            if (lastTs != null) {
                const dt = ts - lastTs, dp = pos - lastPos;
                if (dt >= GAP_MS) { s.gaps++; log(`gap ${r1(dt)}ms step ${r1(dp)}px ans ${since(ts)} ago`); }
                else if (dt >= SLOW_MS) { s.slow++; log(`slow ${r1(dt)}ms`); }
                s.gapMax = Math.max(s.gapMax, dt);
                let jerk = false;
                if (dt > 0 && vel != null && ts >= quietTill) {
                    const off = dp - vel * dt;
                    if ((jerk = Math.abs(off) > JERK_PX)) {
                        s.jerks++; s.jerkMax = Math.max(s.jerkMax, Math.abs(off));
                        log(`jerk ${r1(off)}px frame ${r1(dt)}ms ans ${since(ts)} ago`);
                        run.push(dp / dt);
                        // Jerks in a row at one speed are a new speed (αHP's view moves at its own): learn it.
                        if (run.length >= RELEARN_FRAMES && Math.max(...run) - Math.min(...run) < RELEARN_SPREAD) {
                            const v = run.reduce((a, b) => a + b, 0) / run.length;
                            log(`speed ${r1(vel * 1000)} -> ${r1(v * 1000)}px/s`);
                            vel = v; run = [];
                        } else if (run.length >= RELEARN_FRAMES) run.shift();
                    } else run = [];
                }
                // The speed from even frames only: a jerk's step would make the next look off too.
                if (!jerk && dt > 0 && dt < 100) vel = vel == null ? dp / dt : vel * 0.9 + (dp / dt) * 0.1;
            }
            lastTs = ts; lastPos = pos;
            if (ts - s.t0 >= TRACE_SUMMARY_MS) {
                log(`10s: frames ${s.frames} slow ${s.slow} gaps ${s.gaps} max ${r1(s.gapMax)}ms jerks ${s.jerks} max ${r1(s.jerkMax)}px`
                    + ` ans late ${s.late} max ${r1(s.latMax)}ms empty ${s.empty} failed ${s.failed} snaps ${s.snaps} holds ${s.holds}`
                    + (s.pxN ? ` steps 0:${s.px[0]} 1:${s.px[1]} 2:${s.px[2]} 3+:${s.px[3]} mean ${Math.round(s.pxSum / s.pxN * 1000) / 1000}px changes ${s.pxTurns}` : ''));
                reset(ts);
            }
        },
        clock(kind, info = '') {
            if (s) { if (kind === 'snap') s.snaps++; else if (kind === 'hold') s.holds++; }
            // A snap or a hold moves the picture on purpose: not a jerk.
            quietTill = (lastTs ?? 0) + 50; vel = kind === 'hold' ? 0 : vel;
            log(`clock ${kind}${info ? ' ' + info : ''}`);
        },
        answer(ms, kind, ts) {
            if (!s) return;
            s.latMax = Math.max(s.latMax, ms);
            if (kind === 'ok' || kind === 'kept') lastAns = ts;
            if (kind === 'empty' || kind === 'kept') s.empty++;
            if (kind === 'failed') s.failed++;
            if (ms >= LATE_MS) { s.late++; log(`answer late ${r1(ms)}ms ${kind}`); }
            else if (kind !== 'ok') log(`answer ${kind} ${r1(ms)}ms`);
        },
        note(text) { log(text); },
    };
}

/// A long animation frame this long goes to the log; a script inside it
/// this long is named.
export const LOAF_MS = 20;
export const LOAF_SCRIPT_MS = 3;

const ms0 = (x) => Math.round(x || 0);

/// One 'long-animation-frame' entry as a log line: page time it began,
/// length, the part blocked by script, the part spent on style/layout/paint
/// (from renderStart to the end), and its scripts of LOAF_SCRIPT_MS and
/// more (invoker, file:function); `now` = page time it reached us.
export function loafLine(e, now) {
    const render = e.renderStart ? e.startTime + e.duration - e.renderStart : 0;
    const scripts = (e.scripts || []).filter((x) => x.duration >= LOAF_SCRIPT_MS)
        .map((x) => `${ms0(x.duration)}ms ${x.invoker || '?'} ${(x.sourceURL || '').split('/').pop()}:${x.sourceFunctionName || ''}`);
    return `loaf ${ms0(e.duration)}ms at ${ms0(e.startTime)} block ${ms0(e.blockingDuration)}ms render ${ms0(render)}ms`
        + ` seen ${ms0(now - e.startTime - e.duration)}ms after${scripts.length ? ' scripts ' + scripts.join('; ') : ''}`;
}

/// One 'longtask' entry (the main thread busy 50 ms and more).
export function taskLine(e, now) {
    return `longtask ${ms0(e.duration)}ms at ${ms0(e.startTime)} seen ${ms0(now - e.startTime - e.duration)}ms after`;
}

/// Long frames and long tasks of the page into `log` while the trace is on;
/// returns the call that stops them. Where the page has no such observer the
/// log says so once.
export function watchFrames(log, PO = globalThis.PerformanceObserver, now = () => performance.now()) {
    const obs = [];
    const watch = (type, line, keep) => {
        // An unknown type is only a console warning in observe(): ask first.
        if (PO.supportedEntryTypes && !PO.supportedEntryTypes.includes(type)) { log(`no ${type}`); return; }
        try {
            const o = new PO((list) => { for (const e of list.getEntries()) if (keep(e)) log(line(e, now())); });
            o.observe({ type, buffered: false });
            obs.push(o);
        } catch (err) { log(`no ${type}: ${err?.message || err}`); }
    };
    if (!PO) { log('no PerformanceObserver'); return () => {}; }
    watch('long-animation-frame', loafLine, (e) => e.duration >= LOAF_MS);
    watch('longtask', taskLine, () => true);
    return () => { for (const o of obs) o.disconnect(); };
}
