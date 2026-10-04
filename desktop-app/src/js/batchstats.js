// ══════════════════════════════════════════════════════════════════════
// How fast the last batch went.
//
// When a batch ends, the status line under the list says how much audio it
// converted, in how long, and what that is as a multiple of real time: the
// sum of the durations of every file that finished, over the wall-clock time
// of the whole batch, from the click to the last file written. Preparation,
// filter loading, encoding and verification are all inside that time — it is
// the figure someone waiting for their library actually lives through, not a
// best case for the convolution alone.
//
// Per-file figures go in the line's tooltip. Several files are converted at
// once, so each file's own time overlaps the others', and it is read off the
// 200 ms progress poll: good to a fraction of a second, not better. The same
// figure stands beside a finished file's ✓ in its row (`speedX` on its
// record, kept with the converted file's record on disk).
//
// Durations come from the list (what the player read from each header when
// the file was added), looked up when the batch starts — the rows may have
// moved to another playlist by the time it ends.
//
// Multi-group batches (Convert all with M tracks having different settings)
// run sequentially. `accumulated` spans all groups of a single user-initiated
// run so the result shown in the Convert all button covers every file, not
// just the last group. `batchReset` clears it when a new user action begins.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';

let batch = null;
// Cross-group accumulator for multi-group runs.
let accumulated = null;
// Files named in the result's tip, one line each (the session's log has all).
const PER_FILE_TIP = 12;

function durationOf(path) {
    const e = state.list.find(x => x.path === path && x.info && x.info.durationS > 0);
    return e ? e.info.durationS : null;
}

/** Clear the cross-group accumulator at the start of a new user-initiated run. */
export function batchReset() {
    accumulated = null;
}

/** A batch was handed to the backend with these records. */
export function batchStarted(records) {
    batch = { t0: performance.now(), records };
    for (const f of records) f.durS = durationOf(f.path);
    // Initialize the accumulator on the first group of a run; subsequent groups
    // find it already set and just append to it.
    if (!accumulated) accumulated = { t0: batch.t0, audioS: 0, doneCount: 0, totalCount: 0, allDone: [] };
}

/** Called on every progress poll, after the records were updated. */
export function batchPolled() {
    if (!batch) return;
    const now = performance.now();
    for (const f of batch.records) {
        if (f.status === 'active' && f.tActive == null) f.tActive = now;
        if (f.status === 'done' && f.tDone == null) {
            f.tDone = now;
            // Started and finished between two polls.
            if (f.tActive == null) f.tActive = f.tPrevPoll ?? batch.t0;
            // Its own speed, for its row: known as soon as it is done.
            f.speedX = f.durS > 0 ? f.durS / fileSeconds(f, batch.t0) : null;
        }
        f.tPrevPoll = now;
    }
}

/// `m:ss`, `h:mm:ss` from an hour, and tenths below ten seconds, where a
/// whole second is most of the figure.
function fmtDur(s) {
    if (s < 10) return s.toFixed(1) + ' s';
    const t = Math.round(s);
    const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), sec = t % 60;
    const ss = String(sec).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

const fmtSpeed = x => (x >= 100 ? Math.round(x).toString() : x.toFixed(1));

/// A file's own time in seconds, read off the poll: from when it was first
/// seen converting to when it was first seen done.
function fileSeconds(f, t0) {
    return Math.max(0.001, (f.tDone - (f.tActive ?? t0)) / 1000);
}

/// A finished file's speed for its row, beside its ✓: `{ text: '×25.3',
/// title: '25.3× faster than real time' }`, or null when it is not known.
export function speedTag(x) {
    if (!(x > 0) || !Number.isFinite(x)) return null;
    const n = fmtSpeed(x);
    return { text: `×${n}`, title: `${n}× faster than real time` };
}

/// The finished batch as `{ text, title, short }` for the status line and the
/// Convert all button, or null when nothing finished. Forgets the current
/// group and folds it into the cross-group accumulator.
export function batchSummary() {
    if (!batch) return null;
    const b = batch;
    batch = null;
    const wall = (performance.now() - b.t0) / 1000;
    const done = b.records.filter(f => f.status === 'done' && !f.dismissed);
    if (done.length === 0 || wall <= 0) return null;

    // Fold this group into the cross-group accumulator.
    const acc = accumulated;
    if (acc) {
        const known = done.filter(f => f.durS > 0);
        acc.audioS     += known.reduce((sum, f) => sum + f.durS, 0);
        acc.doneCount  += done.length;
        acc.totalCount += b.records.length;
        // Attach this group's t0 as a fallback for the per-file timing in case
        // tActive was never set (should not happen, but defence-in-depth).
        acc.allDone.push(...done.filter(f => f.tDone != null).map(f => {
            if (f._batchT0 == null) f._batchT0 = b.t0;
            return f;
        }));
    }

    // Compute totals from the accumulator (all groups seen so far).
    const accWall  = acc ? (performance.now() - acc.t0) / 1000 : wall;
    const accAudio = acc ? acc.audioS   : done.filter(f => f.durS > 0).reduce((s, f) => s + f.durS, 0);
    const accDone  = acc ? acc.doneCount  : done.length;
    const accTotal = acc ? acc.totalCount : b.records.length;

    const files = accDone === accTotal
        ? `${accTotal} file${accTotal === 1 ? '' : 's'}`
        : `${accDone} of ${accTotal} files`;

    let text, title, short;
    if (accAudio > 0) {
        const x = accAudio / accWall;
        text  = `✓ ${files} · ${fmtDur(accAudio)} of audio in ${fmtDur(accWall)} · ×${fmtSpeed(x)} real time`;
        short = `✓ ${fmtDur(accAudio)} · ×${fmtSpeed(x)}`;
        title = `${fmtDur(accAudio)} of audio converted in ${fmtDur(accWall)}: ${fmtSpeed(x)}× faster than real time.\n`
            + 'The sum of the durations of the files that finished, over the time of the whole batch — '
            + 'from the click to the last file written, preparation, filter loading, encoding and the '
            + 'bit-perfect check included.';
        const knownCount = acc
            ? acc.allDone.filter(f => f.durS > 0).length
            : done.filter(f => f.durS > 0).length;
        if (knownCount < accDone) {
            title += `\n${accDone - knownCount} file(s) of unknown duration are not counted in the audio time.`;
        }
    } else {
        text  = `✓ ${files} · converted in ${fmtDur(accWall)}`;
        short = `✓ ${fmtDur(accWall)}`;
        title = `Converted in ${fmtDur(accWall)}. The durations of these files are not known, so there is no real-time figure.`;
    }

    // Per file across all groups, in the order they finished.
    const allDone = acc ? [...acc.allDone] : done.filter(f => f.tDone != null);
    const per = allDone
        .sort((a, c) => a.tDone - c.tDone)
        .map(f => {
            const t = fileSeconds(f, f._batchT0 ?? b.t0);
            const d = f.durS > 0
                ? `${fmtDur(f.durS)} in ${fmtDur(t)} · ×${fmtSpeed(f.durS / t)}`
                : `${fmtDur(t)}`;
            return `${f.name} — ${d}`;
        });
    if (per.length) {
        // The first files in the tip, all of them in the log.
        const shown = per.slice(0, PER_FILE_TIP);
        if (per.length > PER_FILE_TIP) shown.push(`…and ${per.length - PER_FILE_TIP} more`);
        title += '\n\nPer file (several run at once, so these overlap; timed from the 200 ms poll):\n' + shown.join('\n');
    }
    // The same for the session's log (player_ui_log): the batch's line, then
    // one per file with its own time — the status line under the list is not
    // in view any more (Anton 3.10).
    const log = [`Conversion done: ${text.replace(/^✓ /, '')}`, ...per.map(p => `  ${p}`)];
    return { text, title, short, log };
}
