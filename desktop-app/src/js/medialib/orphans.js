// ══════════════════════════════════════════════════════════════════════
// Versions of originals that are gone: a converted file lives as long as
// its original does. When the original is no longer on disk (deleted, moved
// away, renamed), the versions this app made of it are deleted by
// themselves — only files the app's own records tie to that original, only
// where the original's folder is still there, seen missing twice some
// seconds apart, never the file that plays or one being written, and no more
// than a few at once (versions-core.js; more wait for a yes on the
// Conversions page).
// ══════════════════════════════════════════════════════════════════════

import { state } from '../state.js';
import { convLive } from '../dropzone.js';
import { madeVersions, forgetVersions, recordedSig } from '../library.js';
import { orphansOf, sweepPlan, sourceCandidates, writingNow } from './versions-core.js';

const NOTE_KEY = 'aura.convSweepNote';
const low = p => String(p || '').toLowerCase();
const dirOf = p => {
    const s = String(p || '');
    const i = Math.max(s.lastIndexOf('\\'), s.lastIndexOf('/'));
    return i < 0 ? '' : s.slice(0, i);
};

/// The conversions that run or wait, each { path, sig }.
export function liveConversions() {
    return [...(state.convFileQueue || []), ...(state.convNextQueue || [])]
        .filter(convLive)
        .map(c => ({ path: c.path, sig: c.sig ?? c.mSig ?? null }));
}

/// Whether the scanned version `v` ({ path, source }) is being written.
export const isWriting = v => writingNow(v, liveConversions(), recordedSig);

async function existing(invoke, paths) {
    const uniq = [...new Set(paths)];
    const ex = await invoke('player_paths_exist', { paths: uniq });
    if (!Array.isArray(ex) || ex.length !== uniq.length) throw new Error('no answer');
    return new Map(uniq.map((p, i) => [low(p), ex[i] === true]));
}

/// The versions of originals that are gone, as the disk says now:
/// [{ out, source }]. Throws when the disk could not be asked (nothing is
/// then taken for gone).
export async function orphansNow(invoke, playing = null) {
    const pairs = madeVersions();
    if (!pairs.length) return [];
    const seen = await existing(invoke, pairs.flatMap(p => [p.out, p.source, dirOf(p.source)]));
    const opts = { playing, writing: out => isWriting({ path: out, source: pairs.find(p => p.out === out)?.source }) };
    const first = orphansOf(pairs, p => seen.get(low(p)) === true, opts);
    if (!first.length) return [];
    // An original under another extension stands for the old one.
    const more = await existing(invoke, first.flatMap(p => sourceCandidates(p.out)));
    for (const [k, v] of more) seen.set(k, v);
    return orphansOf(first, p => seen.get(low(p)) === true, opts);
}

const text = n => `${n} version${n === 1 ? '' : 's'} of ${n === 1 ? 'a file that is' : 'files that are'} no longer there deleted`;
const askText = n => `${n} versions of files that are no longer there: see Conversions to delete them`;

let busy = false;

/// One sweep: look, wait, look again, and what was gone both times is deleted (past the limit nothing does: the page asks). Returns
/// { trashed, ask, note } — `note` is what to tell the listener, '' for
/// nothing; a note nobody could be told is kept for the Conversions page
/// (takeNote).
export async function sweepOrphans({ invoke, playing = () => null, wait = 5000, tell = null } = {}) {
    if (busy) return { trashed: 0, ask: 0, note: '' };
    busy = true;
    try {
        const first = await orphansNow(invoke, playing());
        if (!first.length) return { trashed: 0, ask: 0, note: '' };
        await new Promise(r => setTimeout(r, wait));
        const second = await orphansNow(invoke, playing());
        const plan = sweepPlan(first, second);
        let note = '';
        let trashed = 0;
        if (plan.ask) note = askText(plan.ask);
        else if (plan.trash.length) {
            const res = await invoke('conv_trash', { paths: plan.trash.map(p => p.out) });
            const gone = new Set((res || []).filter(r => r.ok).map(r => low(r.path)));
            trashed = gone.size;
            if (trashed) { forgetVersions(gone); note = text(trashed); }
        }
        if (note) {
            if (tell) tell(note);
            else try { localStorage.setItem(NOTE_KEY, note); } catch (_) {}
        }
        return { trashed, ask: plan.ask, note };
    } catch (_) {
        return { trashed: 0, ask: 0, note: '' };
    } finally {
        busy = false;
    }
}

/// The note of a sweep nobody was told about yet ('' when none), taken once.
export function takeNote() {
    try {
        const n = localStorage.getItem(NOTE_KEY) || '';
        if (n) localStorage.removeItem(NOTE_KEY);
        return n;
    } catch (_) { return ''; }
}
