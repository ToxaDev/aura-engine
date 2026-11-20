// The player's output format: PCM, or the chain's output as one-bit DSD sent
// over DoP (DSD64 / DSD128 / DSD256) by one of the catalog's modulators. What
// the selector after the device button lists (built from what this PC can
// run, `player_dsd_capabilities`), what is stored, what the backend's
// `PlayerSettings.output` takes, and the selector's tip.

/// The DoP rates, in list order (the highest first).
export const OUTFMT_MULTIPLES = [256, 128, 64];

/// The modulator every DoP rate always has: the lightest, and what a stored
/// value from before the catalog ("256") means.
export const BASE_MODULATOR = 'sdm5';

/// The re-measure item's value (never stored).
export const OUTFMT_RECHECK = 'recheck';

export const OUTFMT_TIP = 'Output format. DSD turns the chain\'s output into one-bit DSD and sends it as DoP — '
    + 'DSD64/128/256 run the chain at ×4/×8/×16, so the FS setting is set aside. '
    + 'Volume still works. Needs a DAC that accepts DoP. Bit-perfect plays every file as it is: '
    + 'PCM as PCM, a DSD file as DoP at its own rate. '
    + 'SDM5, SDM7 and SDM9 are AuraEngine\'s one-bit modulators of the 5th, 7th and 9th order, best first: '
    + 'LA looks ahead (heavier on the CPU), Wide keeps the ultrasonic noise lower.';

/// The list's marks, said once at the end of the tip.
export const OUTFMT_LEGEND = 'High CPU: plays, but takes much of one core per channel on this PC. '
    + 'Grey: too heavy for this PC.';

/// The marks after a modulator's name in the open list.
export const OUTFMT_MARK = { heavy: ' · high CPU', tooHeavy: ' · too heavy for this PC' };

// The catalog's names by kind, as the last capabilities said them.
const names = new Map();

/// Remembers the catalog's names from a capabilities payload, so the strip
/// and the notes name a modulator as the list does.
export function rememberKindNames(caps) {
    for (const k of caps?.kinds || []) if (k?.kind && k.label) names.set(k.kind, String(k.label));
}

/// The capabilities as kept between starts: with the app's version, so an
/// update never shows the list of the build before it.
export function capsCacheEntry(caps, version) {
    return JSON.stringify({ version: String(version ?? ''), caps });
}

/// The kept capabilities, if kept by this very version; null otherwise.
export function capsFromCache(raw, version) {
    try {
        const e = JSON.parse(raw);
        return version && e?.version === String(version) && Array.isArray(e.caps?.kinds) ? e.caps : null;
    } catch (_) {
        return null;
    }
}

/// A modulator's name: the catalog's, or read from its kind before the
/// catalog has been heard ("sdm7WideLa" → "SDM7 Wide LA", "sdm7LaPlus" →
/// "SDM7 LA+").
export function kindLabel(kind) {
    const k = String(kind ?? '');
    if (names.has(k)) return names.get(k);
    const m = k.match(/^sdm(\d+)(Wide)?(La)?(Plus)?(Trellis)?$/);
    if (!m) return k;
    return [`SDM${m[1]}`, m[2] && 'Wide', m[3] && (m[4] ? 'LA+' : 'LA'), m[5] && 'Trellis'].filter(Boolean).join(' ');
}

/// A stored value as the selector takes it: 'pcm', or "<multiple>:<kind>".
/// A value from before the catalog ("256") is that rate with SDM5; anything
/// unknown is PCM.
export function outFmtValue(v) {
    const s = String(v ?? '');
    const m = s.match(/^(64|128|256)(?::([a-z][A-Za-z0-9]*))?$/);
    return m ? `${m[1]}:${m[2] || BASE_MODULATOR}` : 'pcm';
}

/// A value's DoP rate and modulator; null for PCM.
export function parseOutFmt(v) {
    const m = outFmtValue(v).match(/^(\d+):(.+)$/);
    return m ? { multiple: Number(m[1]), kind: m[2] } : null;
}

/// The closed selector's text: the rate alone ("PCM", "DSD64", "DSD128",
/// "DSD256"); the modulator is named in the open list and the tip, so the
/// button stays as narrow as "DSD256".
export function outFmtClosedLabel(v) {
    const p = parseOutFmt(v);
    return p ? `DSD${p.multiple}` : 'PCM';
}

/// The output format as `PlayerSettings.output` takes it.
export function outputFormatPayload(fmt) {
    const p = parseOutFmt(fmt);
    return p ? { kind: 'dsd', multiple: p.multiple, modulator: p.kind } : { kind: 'pcm' };
}

/// The source's rate family for the list: 48 kHz for its multiples, 44.1 kHz
/// otherwise — and before a track is loaded.
export function rateBase(srcRate) {
    return srcRate > 0 && srcRate % 48000 === 0 ? 48000 : 44100;
}

// How a modulator fares at one DoP rate of the family: its row, or null.
function rowOf(k, multiple, base) {
    return (k?.rates || []).find((r) => r.multiple === multiple && r.base === base) || null;
}

const byRank = (caps) => [...(caps?.kinds || [])].sort((a, b) => (a.rank ?? 1e9) - (b.rank ?? 1e9));

/// The selector's list: PCM, then DSD256, DSD128 and DSD64, each the best
/// modulator first, a separator between the rates. From the capabilities
/// (`caps`) of the family `base`: what is not built or not for that rate is
/// left out, what is too heavy for this PC listed but disabled, what is heavy
/// listed and marked; the reasons go to the tip. Each item: { value, short
/// (the closed selector), long (the open list), state, reason, disabled }, or
/// { sep: true }. Before the capabilities (`caps` null): SDM5 at every rate,
/// and the stored choice `saved` at its own. `recheck` adds the re-measure
/// item.
export function buildOutFmtList(caps, base, saved, recheck = false) {
    const items = [{ value: 'pcm', short: 'PCM', long: 'PCM', state: 'ok', reason: '', disabled: false }];
    const want = parseOutFmt(saved);
    for (const multiple of OUTFMT_MULTIPLES) {
        items.push({ sep: true });
        const item = (kind, label, state, reason, load = null) => {
            const short = `DSD${multiple} (${label})`;
            const mark = OUTFMT_MARK[state] || '';
            return { value: `${multiple}:${kind}`, short, long: short + mark, label, multiple, state, reason, load, disabled: state === 'tooHeavy' };
        };
        if (!caps) {
            if (want && want.multiple === multiple && want.kind !== BASE_MODULATOR) items.push(item(want.kind, kindLabel(want.kind), 'ok', ''));
            items.push(item(BASE_MODULATOR, kindLabel(BASE_MODULATOR), 'ok', ''));
            continue;
        }
        for (const k of byRank(caps)) {
            const r = rowOf(k, multiple, base);
            // The same modulator as a plainer kind here (SDM7 LA+ at DSD256 is
            // SDM7 LA): one row, that one's.
            if (!r || r.sameAs || !['ok', 'heavy', 'tooHeavy'].includes(r.state)) continue;
            items.push(item(k.kind, k.label || kindLabel(k.kind), r.state, String(r.reason || ''), Number.isFinite(r.load) ? r.load : null));
        }
    }
    if (caps && recheck) {
        const text = recheck === 'checking' ? 'Checking this PC…' : 'Re-check this PC…';
        items.push({ sep: true });
        items.push({ value: OUTFMT_RECHECK, short: text, long: text, state: 'ok', reason: '', disabled: false });
    }
    return items;
}

/// What plays for the stored choice `v` on this PC (`caps`, family `base`):
/// the choice itself while this PC can run it; else the best modulator it
/// can run at that rate (SDM5 at the least), with a note saying why. The
/// stored choice stays as it is — a re-check or a new version may give it
/// back. Before the capabilities: the choice. { value, note }.
export function resolveOutFmt(v, caps, base) {
    const value = outFmtValue(v);
    const p = parseOutFmt(value);
    if (!p || !caps) return { value, note: '' };
    const kinds = byRank(caps);
    const own = kinds.find((k) => k.kind === p.kind);
    const row = own && rowOf(own, p.multiple, base);
    // A choice that is another kind's very modulator at this rate plays as
    // that kind, without a note: nothing is lost.
    if (row?.sameAs && row.sameAs !== p.kind) return resolveOutFmt(`${p.multiple}:${row.sameAs}`, caps, base);
    if (row && (row.state === 'ok' || row.state === 'heavy')) return { value, note: '' };
    const runs = (state) => kinds.find((k) => {
        const r = rowOf(k, p.multiple, base);
        return r?.state === state && !r.sameAs;
    });
    const fb = runs('ok') || runs('heavy');
    const kind = fb ? fb.kind : BASE_MODULATOR;
    if (kind === p.kind) return { value, note: '' };
    const label = own?.label || kindLabel(p.kind);
    const why = !row || row.state === 'notAvailable' ? `${label} is not in this version`
        : row.state === 'tooHeavy' ? `${label} at DSD${p.multiple} is too heavy for this PC`
        : `${label} is ${row.reason || `not for DSD${p.multiple}`}`;
    const fbLabel = fb?.label || kindLabel(kind);
    return { value: `${p.multiple}:${kind}`, note: `${why} — ${fbLabel} plays instead.` };
}

/// The list's own lines for the tip, for the item `value` chosen: its load
/// when it is heavy, and which modulators are too heavy for this PC.
export function outFmtListLines(items, value) {
    const lines = [];
    const chosen = items.find((i) => i.value === value);
    if (chosen?.state === 'heavy') {
        const load = chosen.load !== null ? ` takes about ${Math.round(chosen.load * 100)} % of a core per channel —` : ' is heavy for this PC:';
        lines.push(`${chosen.label} at DSD${chosen.multiple}${load} a busy PC may make it step down to a lighter modulator.`);
    }
    const grey = items.filter((i) => i.disabled);
    if (grey.length) lines.push(`Too heavy for this PC: ${grey.map((i) => i.short).join(', ')}.`);
    return lines;
}

/// An item's own tip in the open list: how its modulator fares at its rate
/// on this PC ("SDM7 LA at DSD256: about 72 % of a core per channel: a busy
/// PC may make it step down"); the re-check item says what it does. PCM, and
/// the list from before the capabilities, have none.
export function outFmtItemTip(item) {
    if (!item || item.sep) return '';
    if (item.value === OUTFMT_RECHECK) return 'Measures again which modulators this PC can run.';
    return item.multiple && item.reason ? `${item.label} at DSD${item.multiple}: ${item.reason}` : '';
}

/// The strip's name of the playing DSD output (status `dsd`): the rate's
/// label, then the modulator that plays — after a step down the lighter one:
/// "DSD256 · SDM7 LA", "DSD256 (48k) · SDM5".
export function dsdStripLabel(dsd) {
    const m = dsd?.running || dsd?.modulator;
    return m ? `${dsd.label} · ${kindLabel(m)}` : String(dsd?.label ?? '');
}

/// The selector's tip and its note for the status `s`: DSD asked for — or a
/// DSD file under BIT-PERFECT — but PCM played, or a lighter modulator plays,
/// why (`dsdNote`, the selector marked); the stored choice this PC cannot
/// run (`list.note`, marked too); DSD output playing — the modulator's
/// overloads, when there were any; then the list's lines (`list.lines`).
export function outFmtTip(s, bitPerfect, list = {}) {
    const notes = [s?.dsdNote, list.note].filter(Boolean).map(String);
    const lines = [OUTFMT_TIP, ...notes];
    if (!notes.length && !bitPerfect && s?.dsd && s.dsd.overloads > 0) lines.push(`Modulator overloads: ${s.dsd.overloads}`);
    lines.push(...(list.lines || []), OUTFMT_LEGEND);
    return { tip: lines.join('\n'), note: notes.length > 0 };
}

/// The technical line for a DSD file played as it is (BIT-PERFECT, status
/// `dsd.asIs`): "DSD64 · DoP · bit-perfect · exclusive"; '' for any other
/// stream.
export function dsdAsIsLine(s) {
    return s?.dsd?.asIs ? `${s.dsd.label} · DoP · bit-perfect · exclusive` : '';
}
