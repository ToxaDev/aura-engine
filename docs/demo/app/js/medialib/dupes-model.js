// ══════════════════════════════════════════════════════════════════════
// The Duplicates page's plain logic (dupes.js), with no page and no
// backend: the rows the page shows, which copy of each stays (the Keep
// rule), what is ticked to go, the totals, the part of a path that differs.
//
// The backend (lib_dupes) gives three lists:
//   groups   — the copies of one track: "exact" (same file) or "audio"
//              (same FLAC audio, other tags); `twin` names the twin row a
//              group folds into;
//   twins    — whole album folders that are copies of another one;
//   versions — the same song in another format or rate: never a copy,
//              shown for the eyes only.
// One copy of every track always stays: the keep is never ticked, and the
// backend refuses the last copy anyway.
// ══════════════════════════════════════════════════════════════════════

const low = s => String(s ?? '').toLowerCase();
const trimSep = s => String(s ?? '').replace(/[\\/]+$/, '');

/// The folder a file is in.
export const dirOf = p => String(p ?? '').replace(/[\\/][^\\/]*$/, '');

/// Whether `path` is `dir` or inside it (no case, either slash).
export function under(path, dir) {
    const a = low(path).replace(/\//g, '\\');
    const d = low(trimSep(dir)).replace(/\//g, '\\');
    return !!d && (a === d || a.startsWith(d + '\\'));
}

/// The Keep rules, with their words (the roots come as "folder:<root>").
export const RULES = { quality: 'Best quality', oldest: 'Added first', shortest: 'Shortest path' };
export function ruleText(rule) {
    if (String(rule).startsWith('folder:')) return `Copy in ${rule.slice(7)}`;
    return RULES[rule] || RULES.quality;
}

/// How high a file stands: DSD over PCM, then the rate, the bits, the size.
export function qualityOf(m) {
    const dsd = Number(m.dsd) || 0;
    return [dsd ? 1 : 0, dsd || Number(m.rate) || 0, Number(m.bits) || 0, Number(m.size) || 0];
}

const cmp = (a, b) => {
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
        const d = (a[i] ?? 0) - (b[i] ?? 0);
        if (d) return d;
    }
    return 0;
};

const pathOf = it => it.path ?? it.folder ?? '';
const pathsOf = it => it.paths ?? (it.path ? [it.path] : []);

/// Which of `items` stays (its index). Those with our conversions beside
/// them first (deleting the original takes them away), then one in a
/// playlist, then the rule, then the backend's order (its main is first).
export function chooseKeep(items, rule, { inPlaylist = new Set() } = {}) {
    const score = it => {
        const conv = (Number(it.conversions) || 0) > 0 ? 1 : 0;
        const pl = pathsOf(it).some(p => inPlaylist.has(p)) ? 1 : 0;
        let r = [];
        if (String(rule).startsWith('folder:')) r = [under(pathOf(it), rule.slice(7)) ? 1 : 0];
        else if (rule === 'quality') r = qualityOf(it);
        else if (rule === 'oldest') r = [-(Number(it.added) || 0)];
        else if (rule === 'shortest') r = [-pathOf(it).length];
        return [conv, pl, ...r];
    };
    let best = 0;
    for (let i = 1; i < items.length; i++) if (cmp(score(items[i]), score(items[best])) > 0) best = i;
    return best;
}

/// The page's rows: twin folders first, then the groups that did not fold
/// into one, then the other versions. Each row: { id, kind ('twin', 'exact',
/// 'audio', 'versions'), level ('exact' or 'audio' for a twin), title,
/// artist, items, groups (a twin's folded groups) }.
export function rowsOf(data) {
    const rows = [];
    const groups = data?.groups || [];
    (data?.twins || []).forEach((t, k) => {
        rows.push({
            id: 't' + k, kind: 'twin', level: t.kind, title: t.title, artist: t.artist, tracks: t.tracks,
            items: t.folders || [], groups: groups.filter(g => g.twin === k),
        });
    });
    groups.forEach((g, i) => {
        if (g.twin != null) return;
        rows.push({ id: 'g' + i, kind: g.kind, level: g.kind, title: g.title, artist: g.artist, album: g.album, durationS: g.durationS, items: g.members || [] });
    });
    (data?.versions || []).forEach((v, i) => {
        rows.push({ id: 'v' + i, kind: 'versions', level: 'versions', title: v.title, artist: v.artist, album: v.album, durationS: v.durationS, items: v.members || [] });
    });
    return rows;
}

/// The chips' counts: all copies rows, then each kind.
export function kindCounts(rows) {
    const c = { all: 0, twin: 0, exact: 0, audio: 0, versions: 0 };
    for (const r of rows) {
        c[r.kind]++;
        if (r.kind !== 'versions') c.all++;
    }
    c.all += c.versions;
    return c;
}

/// What is ticked to go when the page opens or the rule changes: every copy
/// but the keep of an exact copy (or an exact twin); "same audio" waits for
/// a click; other versions never.
export function ticksFor(row, keep) {
    const out = new Set();
    if (row.kind === 'versions' || row.level !== 'exact') return out;
    row.items.forEach((_, i) => { if (i !== keep) out.add(i); });
    return out;
}

/// The files a row's ticks send to the Recycle Bin. A twin folder sends its
/// tracks that are copies (a track only it has never goes).
export function pathsFor(row, ticked) {
    if (row.kind === 'versions') return [];
    if (row.kind !== 'twin') return [...ticked].map(i => row.items[i]?.path).filter(Boolean);
    const copies = new Set(row.groups.flatMap(g => (g.members || []).map(m => m.path)));
    return [...ticked].flatMap(i => (row.items[i]?.paths || []).filter(p => copies.has(p)));
}

/// Each file's size, from every group.
export function sizesOf(data) {
    const m = new Map();
    for (const g of [...(data?.groups || []), ...(data?.versions || [])]) for (const x of g.members || []) m.set(x.path, Number(x.size) || 0);
    return m;
}

/// The totals on top: the copy groups (folder twins count once), the files
/// beyond one of each track, and the bytes they take. `keepOf(row)` gives
/// the keep of a row (the rule or a click).
export function totals(rows, keepOf) {
    let groups = 0, files = 0, bytes = 0;
    for (const r of rows) {
        if (r.kind === 'versions') continue;
        groups++;
        const keep = keepOf(r);
        if (r.kind === 'twin') {
            const kept = r.items[keep]?.folder;
            for (const g of r.groups) {
                for (const m of g.members || []) {
                    if (kept != null && dirOf(m.path) !== kept && !under(m.path, kept)) { files++; bytes += Number(m.size) || 0; }
                }
            }
        } else {
            r.items.forEach((m, i) => { if (i !== keep) { files++; bytes += Number(m.size) || 0; } });
        }
    }
    return { groups, files, bytes };
}

/// The paths chosen, each once, and their bytes.
export function selection(paths, sizes) {
    const uniq = [...new Set(paths)];
    return { files: uniq.length, bytes: uniq.reduce((s, p) => s + (sizes.get(p) || 0), 0), paths: uniq };
}

/// The part of each path that differs from the others: [before, differs,
/// after] by folder steps (the file's name stays in `after` when it is the
/// same). One path alone: all in `before`.
export function differPart(paths) {
    const split = p => String(p ?? '').split(/(?<=[\\/])/);
    const parts = paths.map(split);
    if (parts.length < 2) return paths.map(p => [String(p ?? ''), '', '']);
    const same = (k, from) => parts.every(x => x.length > k && low(from(x, k)) === low(from(parts[0], k)));
    let a = 0;
    while (same(a, (x, k) => x[k])) a++;
    let b = 0;
    while (same(b, (x, k) => x[x.length - 1 - k]) && parts.every(x => x.length - b - 1 > a)) b++;
    return parts.map(x => [x.slice(0, a).join(''), x.slice(a, x.length - b).join(''), x.slice(x.length - b).join('')]);
}

/// The confirmation's lines: whole folders, the other files and the
/// folders they are in.
export function confirmLines(rows, ticks) {
    const whole = [];
    let files = 0;
    const dirs = new Set();
    for (const r of rows) {
        const t = ticks.get(r.id);
        if (!t || !t.size) continue;
        if (r.kind === 'twin') {
            for (const i of t) {
                const f = r.items[i];
                if (f) whole.push({ folder: f.folder, files: pathsFor(r, new Set([i])).length });
            }
        } else {
            for (const p of pathsFor(r, t)) { files++; dirs.add(low(dirOf(p))); }
        }
    }
    return { whole, files, dirs: dirs.size };
}

/// "4 Mar 2025" for a time in seconds; '' without one.
export function dateText(secs) {
    if (!secs) return '';
    const d = new Date(secs * 1000);
    return `${d.getDate()} ${d.toLocaleString('en', { month: 'short' })} ${d.getFullYear()}`;
}

/// A file's format words: "FLAC", "16/44.1".
export function fmtWords(m) {
    const ext = String(m.ext || (pathOf(m).match(/\.([^.\\/]+)$/) || [])[1] || '').toUpperCase();
    const tag = m.tag || (m.rate ? `${m.bits || ''}/${(m.rate / 1000).toString()}` : '');
    return [ext, tag && tag.toUpperCase() !== ext ? tag : ''].filter(Boolean);
}
