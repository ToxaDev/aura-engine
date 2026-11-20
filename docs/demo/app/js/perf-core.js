// This app's load — the arithmetic the badge and the load window share, kept
// free of the page so node tests can hold it to account. A sample is one
// second from the backend (perf.rs): { t: ms since 1970 at its end, cpu, gpu
// (0–100), ram (working set, MB), play, conv }.

export const PERF_TIPS = {
    cpu: "This app's share of the whole processor (all cores), last second.",
    gpu: "This app's share of the video card's time, last second (0 when the card is not used).",
    ram: 'Memory this app uses now (working set).',
};

/** Memory for the badge and the readout: MB under 1 GB, GB to a tenth above; a dash before the first second. */
export function ramText(mb) {
    if (typeof mb !== 'number' || !Number.isFinite(mb) || mb <= 0) return '–';
    if (mb < 1000) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * The top of the memory graph's scale, in MB: a round number of GB at or
 * above the highest value (½ GB at the least), never above the machine's RAM
 * when it is known (`totalMb` > 0), so a full scale reads "all of it".
 */
export function ramScaleTop(maxMb, totalMb = 0) {
    const m = Number.isFinite(maxMb) ? Math.max(0, maxMb) : 0;
    let top = 512;
    while (top < m) top *= 2;
    return totalMb > 0 && top > totalMb ? Math.max(totalMb, m) : top;
}

/** An MB value as GB for the memory graph's scale: whole, or to a tenth / hundredth when small. */
export function gbAxisText(mb) {
    const g = (Number.isFinite(mb) ? mb : 0) / 1024;
    if (g === 0) return '0';
    return String(+g.toFixed(g >= 10 ? 0 : g >= 1 ? 1 : 2));
}

/** A load for the badge: a whole number, no sign; a dash before the first second. */
export function loadText(v) {
    if (typeof v !== 'number' || !Number.isFinite(v)) return '–';
    return String(Math.round(Math.min(100, Math.max(0, v))));
}

/** A load for the window's readout, to a tenth. */
export function loadText1(v) {
    if (typeof v !== 'number' || !Number.isFinite(v)) return '–';
    return Math.min(100, Math.max(0, v)).toFixed(1);
}

/** The index of the sample nearest to time `t` (samples sorted by t), or -1. */
export function nearestIndex(samples, t) {
    const n = samples.length;
    if (!n) return -1;
    let lo = 0, hi = n - 1;
    while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (samples[mid].t < t) lo = mid + 1; else hi = mid;
    }
    if (lo > 0 && Math.abs(samples[lo - 1].t - t) <= Math.abs(samples[lo].t - t)) return lo - 1;
    return lo;
}

/**
 * The stretches where `key` (play / conv) held, as [{ t0, t1 }]: a sample
 * covers the second before its time, so neighbours join; a gap of more than
 * `gapMs` (a sleeping machine) ends a stretch.
 */
export function stretches(samples, key, gapMs = 2500) {
    const out = [];
    let cur = null;
    let prevT = null;
    for (const s of samples) {
        const joined = cur && prevT !== null && s.t - prevT <= gapMs;
        if (s[key]) {
            if (joined) {
                cur.t1 = s.t;
            } else {
                if (cur) out.push(cur);
                cur = { t0: s.t - 1000, t1: s.t };
            }
        } else if (cur) {
            out.push(cur);
            cur = null;
        }
        prevT = s.t;
    }
    if (cur) out.push(cur);
    return out;
}

/** The top of a graph's scale: a round number at or above the highest value, 5 at the least. */
export function scaleTop(maxValue) {
    const m = Number.isFinite(maxValue) ? maxValue : 0;
    for (const top of [5, 10, 20, 25, 50, 100]) {
        if (m <= top) return top;
    }
    return 100;
}

/** HH:MM:SS of a time, in the local zone. */
export function clockText(ms) {
    const d = new Date(ms);
    const p = n => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** Keep at most `cap` samples, the newest; returns the same array. */
export function pushCapped(samples, s, cap = 3600) {
    samples.push(s);
    if (samples.length > cap) samples.splice(0, samples.length - cap);
    return samples;
}
