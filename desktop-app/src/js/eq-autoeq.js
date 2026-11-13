// AutoEq search for the EQ window: the list of results (backend
// eq_autoeq_index, kept a week on disk), a search over it that puts one
// recommended result per headphone first, and one result's ParametricEQ text
// (eq_autoeq_fetch), ready for parseProfile. No DOM; the Tauri call comes in
// as `invoke`, so node can test it.

export const AUTOEQ_SITE = 'https://autoeq.app';

/// Lower case, no accents, words split on anything but letters and digits:
/// "Sennheiser HD-600" → "sennheiser hd 600".
export function norm(s) {
    return String(s || '').normalize('NFKD').replace(/[̀-ͯ]/g, '').toLowerCase()
        .replace(/[^\p{L}\p{N}]+/gu, ' ').trim();
}

// How well a name matches the query words: lower is better, null = no match.
function score(name, words, whole) {
    if (!words.every(w => name.includes(w))) return null;
    if (name === whole) return 0;
    if (name.startsWith(whole)) return 1;
    const starts = words.every(w => name.startsWith(w) || name.includes(' ' + w));
    // A model number typed alone ("600") also matches a word that starts with it.
    return starts ? 2 : 3;
}

/// Results for `query` among `entries` ([{name, source, rig, path, rec}]).
/// Each headphone appears once in the first part, as its recommended result
/// (or the first one when AutoEq recommends none) — `best: true`, with
/// `others` = how many more measurements of it there are; the other
/// measurements follow, so a first-time user takes the top line and an
/// expert can still pick a measurer. At most `limit` items.
export function searchAutoEq(entries, query, { limit = 60 } = {}) {
    const whole = norm(query);
    if (!whole || !Array.isArray(entries)) return [];
    const words = whole.split(' ');
    const groups = new Map();
    for (const e of entries) {
        const n = norm(e.name);
        const s = score(n, words, whole);
        if (s === null) continue;
        let g = groups.get(n);
        if (!g) groups.set(n, g = { s, items: [] });
        g.items.push(e);
    }
    const ordered = [...groups.entries()].sort((a, b) => a[1].s - b[1].s
        || a[0].length - b[0].length || a[0].localeCompare(b[0]));
    const best = [], rest = [];
    for (const [, g] of ordered) {
        const pick = g.items.find(e => e.rec) || g.items[0];
        best.push({ ...pick, best: true, others: g.items.length - 1 });
        for (const e of g.items) if (e !== pick) rest.push({ ...e, best: false, others: 0 });
    }
    return best.concat(rest).slice(0, limit);
}

/// "measured by crinacle on 711" — shown with each result and after import.
export function autoEqCredit(e) {
    if (!e) return '';
    return `AutoEq · measured by ${e.source}${e.rig ? ' on ' + e.rig : ''}`;
}

/// The two backend calls, with the list asked once per window. `index()`
/// gives {entries, fetched, stale}: stale = the kept list, the server was
/// not reached. Both reject with a short English message for the user.
export function autoEqApi(invoke) {
    let pending = null;
    return {
        index() {
            if (!pending) pending = invoke('eq_autoeq_index').catch(e => { pending = null; throw errText(e); });
            return pending;
        },
        fetch(entry) {
            return invoke('eq_autoeq_fetch', { path: entry.path }).catch(e => { throw errText(e); });
        },
    };
}

function errText(e) {
    return new Error(typeof e === 'string' ? e : e?.message || 'AutoEq is not available right now');
}
