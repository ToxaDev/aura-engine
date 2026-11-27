// ══════════════════════════════════════════════════════════════════════
// The Conversions page's own arithmetic, no DOM: the converted files the
// backend found (conv_scan) put under their sources, each source's versions
// as a tree by kind, the kinds for the filter, and the space they take.
//
// A file of the scan: { path, source, sourceSize, size, mtimeMs, kind:
// 'pcm'|'dsd', rate, dsd, srcRate, modulator, filter, taps, precision,
// stages: [..], tags: [..] }.
// ══════════════════════════════════════════════════════════════════════

const baseName = p => String(p || '').split(/[\\/]/).pop();
const dirOf = p => String(p || '').replace(/[\\/][^\\/]*$/, '');

/// A rate as the app writes it: 352800 → "352.8k", 384000 → "384k".
export function rateText(hz) {
    const n = Number(hz) || 0;
    if (!n) return '';
    const k = n / 1000;
    return `${Number.isInteger(k) ? k : k.toFixed(1)}k`;
}

/// How many times the base rate (44.1k or 48k) a PCM rate is: 352800 → 8.
/// 0 for a rate of neither family.
export function fsOf(hz) {
    const n = Number(hz) || 0;
    for (const base of [44100, 48000]) {
        if (n >= base && n % base === 0) return n / base;
    }
    return 0;
}

/// The kind a version is filed under and filtered by: "DSD256", "FS8", or
/// the rate itself for a PCM rate of no family ("PCM 96k" is "FS2").
export function typeOf(v) {
    if (v.kind === 'dsd') return v.dsd ? `DSD${v.dsd}` : 'DSD';
    const fs = fsOf(v.rate);
    if (fs) return `FS${fs}`;
    return v.rate ? rateText(v.rate) : 'PCM';
}

/// Kinds in the order they are shown: PCM by rate, then DSD by rate.
function typeRank(key) {
    const d = key.match(/^DSD(\d*)$/);
    if (d) return 1e6 + (Number(d[1]) || 0);
    const f = key.match(/^FS(\d+)$/);
    return f ? Number(f[1]) : 0;
}

/// A version's own line: what tells it from its neighbours of the same
/// kind — the rate, the modulator, the filter and its taps, the precision.
export function versionLine(v) {
    const parts = [];
    if (v.kind === 'dsd') {
        if (v.modulator) parts.push(v.modulator);
    } else if (v.rate) parts.push(rateText(v.rate));
    if (v.filter) parts.push([v.filter, v.taps].filter(Boolean).join(' '));
    if (v.precision) parts.push(`f${String(v.precision).replace(/^f/, '')}`);
    return parts.join(' · ');
}

/// The stages a version was rendered through, as the file's name has them.
export const stagesLine = v => (v.stages || []).join(' · ');

/// The files under their sources. A file whose source is gone stands under
/// its own folder and the name before the tag. Each source: { key, source
/// (the path, or null when gone), name, dir, sourceSize, versions (the
/// newest first), size, last }.
export function groupFiles(files) {
    const by = new Map();
    for (const f of files || []) {
        const stem = baseName(f.path).replace(/ \[AE · [^\]]*\]\.[^.]+$/, '');
        const key = (f.source || `${dirOf(f.path)}\u0000${stem}`).toLowerCase();
        let g = by.get(key);
        if (!g) {
            g = {
                key, source: f.source || null, name: f.source ? baseName(f.source) : stem,
                dir: dirOf(f.source || f.path), sourceSize: f.sourceSize ?? null,
                versions: [], size: 0, last: 0,
            };
            by.set(key, g);
        }
        g.versions.push(f);
        g.size += Number(f.size) || 0;
        g.last = Math.max(g.last, Number(f.mtimeMs) || 0);
    }
    const out = [...by.values()];
    for (const g of out) g.versions.sort((a, b) => (b.mtimeMs || 0) - (a.mtimeMs || 0));
    return out;
}

/// A source's versions as a tree: one branch a kind (PCM first, by rate,
/// then DSD), its versions under it, the newest first.
export function versionTree(group) {
    const by = new Map();
    for (const v of group?.versions || []) {
        const key = typeOf(v);
        if (!by.has(key)) by.set(key, { type: key, kind: v.kind === 'dsd' ? 'dsd' : 'pcm', versions: [], size: 0 });
        const b = by.get(key);
        b.versions.push(v);
        b.size += Number(v.size) || 0;
    }
    return [...by.values()].sort((a, b) => typeRank(a.type) - typeRank(b.type));
}

/// The kinds among the files, for the filter and the space bar: { type,
/// kind, count, size }, in the order they are shown.
export function typesOf(files) {
    const by = new Map();
    for (const v of files || []) {
        const key = typeOf(v);
        if (!by.has(key)) by.set(key, { type: key, kind: v.kind === 'dsd' ? 'dsd' : 'pcm', count: 0, size: 0 });
        const t = by.get(key);
        t.count++;
        t.size += Number(v.size) || 0;
    }
    return [...by.values()].sort((a, b) => typeRank(a.type) - typeRank(b.type));
}

/// The space: the converted files' total, their sources' (each once), and
/// the kinds with their shares of the converted (0..1).
export function usageOf(files) {
    const types = typesOf(files);
    const total = types.reduce((s, t) => s + t.size, 0);
    const seen = new Set();
    let originals = 0;
    for (const f of files || []) {
        if (!f.source || seen.has(f.source.toLowerCase())) continue;
        seen.add(f.source.toLowerCase());
        originals += Number(f.sourceSize) || 0;
    }
    return {
        total, originals, count: (files || []).length, sources: seen.size,
        types: types.map(t => ({ ...t, share: total ? t.size / total : 0 })),
    };
}

/// The sources the search and the kind leave, with only the versions of
/// that kind, sorted: 'recent' (the last converted first), 'name', 'size'.
export function pick(groups, { q = '', type = null, sort = 'recent' } = {}) {
    const words = String(q).toLowerCase().split(/\s+/).filter(Boolean);
    const out = [];
    for (const g of groups || []) {
        const hay = `${g.name} ${g.dir}`.toLowerCase();
        if (!words.every(w => hay.includes(w) || g.versions.some(v => baseName(v.path).toLowerCase().includes(w)))) continue;
        const versions = type ? g.versions.filter(v => typeOf(v) === type) : g.versions;
        if (!versions.length) continue;
        out.push(type ? {
            ...g, versions,
            size: versions.reduce((s, v) => s + (Number(v.size) || 0), 0),
            last: Math.max(...versions.map(v => Number(v.mtimeMs) || 0)),
        } : g);
    }
    const name = g => g.name.toLowerCase();
    if (sort === 'name') out.sort((a, b) => name(a).localeCompare(name(b)));
    else if (sort === 'size') out.sort((a, b) => b.size - a.size || name(a).localeCompare(name(b)));
    else out.sort((a, b) => b.last - a.last || name(a).localeCompare(name(b)));
    return out;
}

/// A size for reading: "812 KB", "37.2 MB", "1.84 GB".
export function sizeText(bytes) {
    const n = Number(bytes) || 0;
    if (n < 1024) return `${n} B`;
    const units = ['KB', 'MB', 'GB', 'TB'];
    let v = n / 1024;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return `${v >= 100 ? Math.round(v) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${units[i]}`;
}

/// When a version was made, against `now`: "today", "yesterday", "5 days
/// ago", then the date.
export function whenText(ms, now = Date.now()) {
    if (!ms) return '';
    const day = 86400000;
    const d0 = new Date(now); d0.setHours(0, 0, 0, 0);
    const days = Math.ceil((d0.getTime() - ms) / day);
    if (days <= 0) return 'today';
    if (days === 1) return 'yesterday';
    if (days < 30) return `${days} days ago`;
    const d = new Date(ms);
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/// The folders the scan is asked to read besides the library's own: where
/// the playlists' files are, each once, a folder inside another left out.
export function foldersOf(paths) {
    const dirs = [...new Set((paths || []).map(dirOf).filter(Boolean))].sort((a, b) => a.length - b.length);
    const out = [];
    for (const d of dirs) {
        const low = d.toLowerCase();
        if (out.some(o => low === o.toLowerCase() || low.startsWith(o.toLowerCase() + '\\') || low.startsWith(o.toLowerCase() + '/'))) continue;
        out.push(d);
    }
    return out;
}
