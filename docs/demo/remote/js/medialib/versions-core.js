// ══════════════════════════════════════════════════════════════════════
// An original and its versions, no DOM: the lists hold originals only, a
// converted file ("<stem> [AE · …].flac|dsf") is a version of its original
// and plays under the original's row. Here: telling a version by its name,
// where its original would be, a version still being written, why a version
// cannot be played now, the version a row is told to play (its pin), and
// which versions of originals that are gone may be deleted.
// ══════════════════════════════════════════════════════════════════════

const HEAD = '[AE · ';

/// The extensions an original may have, the preferred first (the backend's
/// SOURCE_EXT, conversions_commands.rs).
export const SOURCE_EXT = ['flac', 'wav', 'wave', 'aif', 'aiff', 'dsf', 'dff', 'wv', 'caf', 'm4a', 'aac', 'mp3', 'ogg', 'oga', 'opus'];

const baseName = p => String(p || '').split(/[\\/]/).pop();
const dirOf = p => {
    const s = String(p || '');
    const i = Math.max(s.lastIndexOf('\\'), s.lastIndexOf('/'));
    return i < 0 ? '' : s.slice(0, i);
};
const sepOf = p => (String(p || '').includes('\\') || !String(p || '').includes('/') ? '\\' : '/');
const low = p => String(p || '').toLowerCase();

/// The stem of a converted file's name (what stands before its final
/// `[AE · …]` block), or null when the name is not the converter's: the
/// block must close the name before `.flac` / `.dsf` and hold something
/// (the backend's split_name).
export function renderStem(name) {
    const n = baseName(name);
    const dot = n.lastIndexOf('.');
    if (dot < 0) return null;
    const ext = n.slice(dot + 1).toLowerCase();
    if (ext !== 'flac' && ext !== 'dsf') return null;
    const base = n.slice(0, dot);
    if (!base.endsWith(']')) return null;
    const inner0 = base.slice(0, -1);
    const a = inner0.lastIndexOf(HEAD);
    if (a < 0) return null;
    const inner = inner0.slice(a + HEAD.length);
    if (!inner || inner.includes(']')) return null;
    return base.slice(0, a).replace(/\s+$/, '');
}

/// Whether `path` is a converted file by its name.
export const isRenderName = path => renderStem(path) != null;

/// Where the original of the converted file `path` would be: its folder,
/// its stem, each source extension, the preferred first. Empty for a file
/// that is no version, or a version with no stem.
export function sourceCandidates(path) {
    const stem = renderStem(path);
    if (!stem) return [];
    const dir = dirOf(path);
    const sep = sepOf(path);
    return SOURCE_EXT.map(ext => `${dir}${sep}${stem}.${ext}`);
}

/// Paths about to be added to a list, apart: `plain` (originals, in order)
/// and `renders` (converted files, which never get a row of their own while
/// their original is there). A version whose original is among the paths is
/// left out at once.
export function splitAdds(paths) {
    const plain = [];
    const renders = [];
    for (const p of paths || []) (isRenderName(p) ? renders : plain).push(p);
    const have = new Set(plain.map(low));
    return { plain, renders: renders.filter(r => !sourceCandidates(r).some(c => have.has(low(c)))) };
}

/// What takes the place of each converted file among paths to add: its
/// original where one is on disk (`exists(path)`), the file itself where
/// none is — a version made elsewhere, with nothing else to stand for it.
/// Returns { add: paths in order, each once; swapped: how many versions gave
/// way to their originals }.
export function resolveAdds(renders, exists) {
    const add = [];
    const seen = new Set();
    let swapped = 0;
    for (const r of renders || []) {
        const src = sourceCandidates(r).find(c => exists(c));
        const p = src || r;
        if (src) swapped++;
        if (seen.has(low(p))) continue;
        seen.add(low(p));
        add.push(p);
    }
    return { add, swapped };
}

/// Whether the scanned version `v` ({ path, source }) is a file a conversion
/// writes now or is about to write: its original converts (`live`: the
/// conversions that run or wait, each { path, sig }), and the version is
/// either unknown to the records (`sigOf(path)` → the signature it was
/// recorded with, or null) or the very one being made again.
export function writingNow(v, live, sigOf) {
    if (!v?.source) return false;
    const src = low(v.source);
    for (const c of live || []) {
        if (low(c.path) !== src) continue;
        const known = sigOf ? sigOf(v.path) : null;
        if (known == null || c.sig == null || known === c.sig) return true;
    }
    return false;
}

/// The highest DSD rate a file goes out at as it is (DoP's ceiling here).
export const AS_IS_MAX_DSD = 256;

/// Why the version `v` cannot be played now, or null when it can:
/// 'writing' (not finished), 'gone' (no original), 'outdated' (made by an
/// older generation of the sound: the player would not take it), 'cue' (Cue
/// Consistency is on: every track plays live), 'rate' (a DSD rate no output
/// carries as it is), 'volume' (a DSD file takes no volume: only at the top).
export function playBlock(v, { writing = false, outdated = false, cue = false, volumeFull = true } = {}) {
    if (writing) return 'writing';
    if (!v?.source) return 'gone';
    if (outdated) return 'outdated';
    if (cue) return 'cue';
    if (v.kind === 'dsd') {
        if (!(Number(v.dsd) > 0) || Number(v.dsd) > AS_IS_MAX_DSD) return 'rate';
        if (!volumeFull) return 'volume';
    }
    return null;
}

/// A row's pin: the version it was told to play ({ out, dsd: the DSD
/// multiple, 0 for PCM }).
export const pinOf = v => ({
    out: v.path,
    dsd: v.kind === 'dsd' ? Number(v.dsd) || 0 : 0,
    // What the version's name says it was made with: Sound shows it while
    // it plays (version-view.js).
    v: {
        path: v.path, kind: v.kind, rate: v.rate ?? null, dsd: v.dsd ?? null, srcRate: v.srcRate ?? null,
        modulator: v.modulator ?? null, filter: v.filter ?? null, taps: v.taps ?? null,
        stages: Array.isArray(v.stages) ? [...v.stages] : [],
    },
});

/// Whether a pin still holds: the player's output is the version's own kind
/// — PCM for a PCM file, the same DSD rate for a DSF. `outFmt` is what the
/// player publishes: 'flac', or "<multiple>:<modulator>".
export function pinHolds(pin, outFmt) {
    if (!pin?.out) return false;
    const m = String(outFmt || '').match(/^(\d+):/);
    const dsd = m ? Number(m[1]) : 0;
    if (!m && outFmt !== 'flac') return false;
    return dsd === (Number(pin.dsd) || 0);
}

// ── versions of originals that are gone ───────────────────────────────

/// How many versions are deleted by themselves at once; more than
/// that waits for a yes (a mistake here would be a large one).
export const SWEEP_LIMIT = 25;

const isUnc = p => /^[\\/]{2}/.test(String(p || ''));

/// The versions this app is known to have made, as pairs { out, source },
/// from the rows' records (`entries`: each { path, convs, stale }) and the
/// kept list of what was made (`made`: { [out lower]: source }). A row that
/// is itself a converted file gives none. Each `out` once.
export function madePairs(entries, made) {
    const by = new Map();
    const put = (out, source) => {
        if (!out || !source || !isRenderName(out) || isRenderName(source)) return;
        if (!by.has(low(out))) by.set(low(out), { out, source });
    };
    for (const e of entries || []) {
        for (const c of [...(e.convs || []), ...(e.stale || [])]) put(c.out, e.path);
    }
    for (const [, v] of Object.entries(made || {})) put(v?.out, v?.source);
    return [...by.values()];
}

/// Which of the pairs are versions of an original that is gone, to be
/// deleted: the version is on disk, its original is not, and the
/// original's folder is — a drive that is unplugged or a folder that moved
/// away takes its versions' fate out of our hands. Never the file that plays
/// (`playing`), one being written (`writing(out)`), or one on a network
/// share (kept as before: a share's drive comes and goes). `exists(path)` answers for files and
/// folders alike.
export function orphansOf(pairs, exists, { playing = null, writing = null } = {}) {
    const out = [];
    for (const p of pairs || []) {
        if (isUnc(p.out) || isUnc(p.source)) continue;
        if (low(p.out) === low(playing)) continue;
        if (writing && writing(p.out)) continue;
        const dir = dirOf(p.source);
        if (!dir || !exists(dir)) continue;
        if (exists(p.source) || !exists(p.out)) continue;
        // A new original under another extension took the old one's place.
        if (sourceCandidates(p.out).some(c => exists(c))) continue;
        out.push(p);
    }
    return out;
}

/// What a sweep does with the orphans seen twice (`first` and `second`: two
/// looks some seconds apart, a file being replaced is gone for a moment):
/// { trash: those in both, or [] past the limit; ask: how many wait for a
/// yes instead }.
export function sweepPlan(first, second, limit = SWEEP_LIMIT) {
    const again = new Set((second || []).map(p => low(p.out)));
    const both = (first || []).filter(p => again.has(low(p.out)));
    return both.length > limit ? { trash: [], ask: both.length } : { trash: both, ask: 0 };
}

// ── the A/B of two versions (Conversions page) ─────────────────────

/// What the A/B goes on from: the original `source` on the air (playing,
/// paused or getting ready) → { trackId, at (seconds), paused }; else null.
export function abStart(s, playing, source) {
    if (!s || s.trackId == null || !s.state || s.state === 'stopped') return null;
    if (!playing || low(playing) !== low(source)) return null;
    return { trackId: s.trackId, at: Math.max(0, Number(s.positionS) || 0), paused: s.state === 'paused' };
}

/// A jump still owed once the other file is on the air: the second to go
/// to when it plays 3 s or more before where the A/B left (it started
/// over), else null. Ahead is time gone by since the jump.
export function abFix(at, s) {
    if (!(at > 3)) return null;
    const p = Number(s?.positionS);
    if (!Number.isFinite(p)) return null;
    return p < at - 3 ? at : null;
}
