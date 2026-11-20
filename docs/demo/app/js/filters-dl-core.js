// The filter download's decisions, without the page: what the bar above the
// rack says, which files a ticked set of groups downloads, what a backend
// status means for the dialog, and what the storage view lists. The page side
// is filters-dl.js; this file stays free of the DOM and of Tauri so the tests
// can run it under node.

/// 1.0 GB, 760 MB, 8 MB — decimal units, the way the drive's free space reads.
export function fmtBytes(n) {
    const v = Math.max(0, Number(n) || 0);
    if (v >= 1e11) return Math.round(v / 1e9) + ' GB';
    if (v >= 1e9) return (v / 1e9).toFixed(1) + ' GB';
    if (v >= 1e6) return Math.round(v / 1e6) + ' MB';
    if (v >= 1e3) return Math.round(v / 1e3) + ' KB';
    return v + ' B';
}

/// The backend's margin (filters_dl::plan::space_needed): the bytes and 2 %
/// of them, at least 256 MB, so the disk is not filled to the brim.
export function spaceNeeded(bytes) {
    return bytes + Math.max(Math.floor(bytes / 50), 256 * 1024 * 1024);
}

/// "drive D:" for a Windows path, "disk" for any other.
export function driveOf(dest) {
    const m = /^([A-Za-z]):/.exec(dest || '');
    return m ? `drive ${m[1].toUpperCase()}:` : 'disk';
}

/// "FS16 (705.6k / 768k)".
export function fsText(fs) {
    const k = hz => { const v = hz / 1000; return (Number.isInteger(v) ? v.toFixed(0) : v.toFixed(1)) + 'k'; };
    return `FS${fs} (${k(44100 * fs)} / ${k(48000 * fs)})`;
}

/// Whether a setting needs the minimum-phase filter too: Hybrid-Phase and TFS
/// both render against the minimum-phase half of the pair. TFS ships on, so a
/// fresh install that fetched only the linear one could not play.
export function needsMinimum(hybridPhase, tfsPhase) {
    return !!hybridPhase || !!tfsPhase;
}

/// The phases a setting needs: the linear filter always, the minimum-phase one
/// with Hybrid-Phase or TFS — only that one when the linear is already in place.
export function phasesFor(needMin, onlyMinMissing) {
    if (onlyMinMissing) return ['minimum'];
    return needMin ? ['linear', 'minimum'] : ['linear'];
}

export function phasesText(phases) {
    return phases.length > 1 ? 'linear + minimum phase' : `${phases[0]} phase`;
}

/// Bytes still to come for one planned file (an interrupted part counts).
const left = f => Math.max(0, (f.size || 0) - (f.part || 0));

/// What each group of a plan (files tagged 44k / 48k / hires) still needs.
export function groupSizes(plan) {
    const out = { '44k': 0, '48k': 0, hires: 0 };
    for (const f of plan?.files || []) out[f.group] = (out[f.group] || 0) + left(f);
    return out;
}

/// The files the ticked groups download, their bytes, and whether `free`
/// holds them with the margin (`null` free: unknown, taken as enough).
export function selection(plan, picked, free) {
    const files = (plan?.files || []).filter(f => picked.has(f.group));
    const bytes = files.reduce((a, f) => a + left(f), 0);
    const need = spaceNeeded(bytes);
    return {
        names: files.map(f => f.name),
        files,
        bytes,
        need,
        enough: files.length === 0 || free == null || free >= need,
    };
}

/// The backend's error text, sorted by what the dialog does with it: a file
/// that failed its check was removed (`checksum`), the drive is too small
/// (`space`), the folder cannot be written (`folder`), anything else is the
/// connection — the parts are kept and Resume picks up where it stopped.
export function classifyError(text) {
    const t = String(text || '');
    if (/checksum|damaged|larger than the list/i.test(t)) return 'checksum';
    if (/not enough free space/i.test(t)) return 'space';
    if (/not writable|cannot write|permission|access is denied/i.test(t)) return 'folder';
    if (/not in the filter list|signature|update is being installed|already running/i.test(t)) return 'other';
    return 'network';
}

/// The dialog's view for a backend status (`/filters/status`).
export function viewOf(status) {
    switch (status?.phase) {
        case 'checking':
        case 'downloading': return 'dl';
        case 'done': return 'done';
        case 'cancelled': return 'cancelled';
        case 'error': {
            const k = classifyError(status.error);
            return k === 'network' ? 'paused' : k === 'space' ? 'nospace' : 'failed';
        }
        default: return 'idle';
    }
}

/// Download speed from the last few seconds of samples [{t ms, done bytes}].
export function rateOf(samples, now, windowMs = 5000) {
    const recent = samples.filter(s => now - s.t <= windowMs);
    if (recent.length < 2) return 0;
    const a = recent[0], b = recent[recent.length - 1];
    const dt = (b.t - a.t) / 1000;
    return dt > 0 ? Math.max(0, (b.done - a.done) / dt) : 0;
}

/// "30 s left", "4 min left", "1 h 5 min left"; '' while the speed is unknown.
export function etaText(remaining, rate) {
    if (!(rate > 0) || !(remaining > 0)) return '';
    const s = Math.ceil(remaining / rate);
    if (s < 60) return `${s} s left`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m} min left`;
    return `${Math.floor(m / 60)} h ${m % 60} min left`;
}

export function percent(done, total) {
    return total > 0 ? Math.min(100, Math.floor(done / total * 100)) : 0;
}

/// How long "filters ready" stays on the bar once the dialog is out of view.
export const READY_BAR_MS = 3500;

/// What the bar above the rack shows. `job` is a download under way or just
/// finished (its label, status and `key` = taps|fs), `missing` whether the chosen setting has
/// no filter; `size` the bytes the plan needs (undefined while unknown).
///
/// Returns null when there is nothing to say.
///
/// A finished download says so for READY_BAR_MS once nobody is looking at
/// the dialog: `job.readyUntil` is set then (filters-dl.js), and until it is
/// set the line stays — the user may still be reading the dialog.
export function barModel({ missing, empty, label, key, size, notOffered, job, hidden, now = Date.now() }) {
    if (job && job.view === 'dl' && hidden) {
        const pct = percent(job.done, job.total);
        return {
            kind: 'dl',
            text: `↓ ${job.label} — downloading ${pct}%`,
            sub: `· ${Math.round(job.done / 1e6)} / ${Math.round(job.total / 1e6)} MB${job.eta ? ' · ' + job.eta : ''}`,
            pct,
        };
    }
    if (job && job.view === 'done' && job.key === key && !(job.readyUntil != null && now >= job.readyUntil)) {
        return { kind: 'ok', text: `✓ ${job.label} — filters ready, the setting works now` };
    }
    if (!missing) return null;
    if (empty) {
        return { kind: 'missing', empty: true, text: '⚠ No filter files yet — the converter cannot run',
            sub: size ? `· ${fmtBytes(size)}` : '', download: !notOffered };
    }
    if (notOffered) {
        return { kind: 'missing', text: `⚠ ${label} — not in the filter release`, sub: '', download: false };
    }
    return { kind: 'missing', text: `⚠ ${label} — not downloaded`, sub: size ? `· ${fmtBytes(size)}` : '', download: true };
}

/// "1G · FS16", with "· 2 phases" / "· minimum phase" when Hybrid-Phase asks.
export function barLabel(tapsLabel, fs, phases) {
    const p = phases.length > 1 ? ' · 2 phases' : phases[0] === 'minimum' ? ' · minimum phase' : '';
    return `${tapsLabel} · FS${fs}${p}`;
}

const RUNGS = [
    { label: '1G', min: 1_000_000_000, color: '#a78bfa' },
    { label: '500M', min: 500_000_000, color: '#818cf8' },
    { label: '300M', min: 300_000_000, color: '#60a5fa' },
    { label: '100M', min: 100_000_000, color: '#38bdf8' },
    { label: '30M and less', min: 0, color: '#22d3ee' },
];

/// A file this version does not read: an unfinished download, a `fir_` name
/// nothing looks up, or a release name at a size the signed list does not have.
export function isUnused(item) {
    return item.kind !== 'filter' || item.verified === false;
}

/// The storage view from `filters_storage`: what the filters in use take by
/// rung (a self-made or legacy filter counts as in use) and the exact files
/// this version does not use — the only ones the button may move.
export function storageModel(items) {
    const rungs = RUNGS.map(r => ({ ...r, count: 0, bytes: 0 }));
    const unused = [];
    let used = 0, usedCount = 0;
    for (const it of items || []) {
        if (isUnused(it)) { unused.push(it); continue; }
        const r = rungs.find(r => (it.taps || 0) >= r.min) || rungs[rungs.length - 1];
        r.count += 1;
        r.bytes += it.size || 0;
        used += it.size || 0;
        usedCount += 1;
    }
    unused.sort((a, b) => a.name.localeCompare(b.name));
    return {
        rungs: rungs.filter(r => r.count > 0),
        used,
        usedCount,
        unused,
        unusedBytes: unused.reduce((a, it) => a + (it.size || 0), 0),
    };
}
