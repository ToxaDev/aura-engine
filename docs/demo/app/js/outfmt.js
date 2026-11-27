// The player's output format: PCM, or the chain's output as one-bit DSD sent
// over DoP (DSD64 / DSD128 / DSD256) — or natively through a DAC's ASIO driver
// or (Linux) an ALSA device's DSD format, up to DSD1024 — by one of the
// catalog's modulators. What
// the selector after the device button lists (built from what this PC can
// run, `player_dsd_capabilities`), what is stored, what the backend's
// `PlayerSettings.output` takes, and the selector's tip.

/// The DoP rates, in list order (the highest first).
export const OUTFMT_MULTIPLES = [256, 128, 64];

/// The rates only native DSD carries (a DAC's ASIO driver, an ALSA DSD
/// format), listed above DoP's when the output device may take them.
export const OUTFMT_NATIVE_MULTIPLES = [1024, 512];

/// Whether a device id is a DAC's ASIO driver ("asio:<driver>").
export function isAsioDevice(id) {
    return /^asio:./.test(String(id ?? ''));
}

/// Whether a device id is an ALSA card's ("hw:CARD=<id>,DEV=<n>", Linux).
export function isAlsaDevice(id) {
    return /^hw:./.test(String(id ?? ''));
}

/// Whether DSD512 and DSD1024 are offered for the device `id` ('' is the
/// system default, `defaultId`): an ASIO driver or an ALSA card, which take
/// DSD natively where the driver (the kernel) has a DSD format — the stream
/// finds out, and says why it plays PCM when it has none.
export function takesNativeDsd(id, defaultId = '') {
    const d = String(id ?? '') || String(defaultId ?? '');
    return isAsioDevice(d) || isAlsaDevice(d);
}

/// The modulator every DoP rate always has: the lightest, and what a stored
/// value from before the catalog ("256") means.
export const BASE_MODULATOR = 'sdm5';

/// The lightest modulator at DSD512 and DSD1024 (SDM5 stops at DSD256).
export const NATIVE_BASE_MODULATOR = 'sdm7';

/// The modulator a rate always has: SDM5 to DSD256, SDM7 above.
export function baseModulator(multiple) {
    return OUTFMT_NATIVE_MULTIPLES.includes(Number(multiple)) ? NATIVE_BASE_MODULATOR : BASE_MODULATOR;
}

/// The re-measure item's value (never stored).
export const OUTFMT_RECHECK = 'recheck';

/// The 48 kHz family DSD switch's item value (a tick in the list, stored on
/// its own): off, the default, a 48 kHz family track goes out at the 44.1 kHz
/// family's DSD rate, which every DSD DAC takes.
export const OUTFMT_DSD48K = 'dsd48k';
export const OUTFMT_DSD48K_LABEL = '48 kHz DSD for 48 kHz tracks';
export const OUTFMT_DSD48K_TIP = 'Off (the default): a 48, 96 or 192 kHz track goes out as the 44.1 kHz family\'s DSD '
    + '(DSD128 at 5.6 MHz), which every DSD DAC takes; the filter still works at the track\'s own family, '
    + 'and only the last step before the modulator moves it to 44.1 kHz. '
    + 'On: the 48 kHz family\'s DSD (DSD128 at 6.1 MHz), for a DAC that takes it. '
    + 'A DAC that does not plays it as quiet noise.';

export const OUTFMT_TIP = 'Output format. DSD turns the chain\'s output into one-bit DSD and sends it as DoP — '
    + 'DSD64/128/256 run the chain at ×4/×8/×16, so the FS setting is set aside. '
    + 'Volume still works. Needs a DAC that accepts DoP. '
    + 'Through a DAC\'s ASIO driver (the "ASIO: …" devices), and on Linux where the system offers the DAC a native DSD format, DSD goes natively, and DSD512/1024 (the chain at ×16) is offered too. '
    + 'Bit-perfect plays every file as it is: '
    + 'PCM as PCM, a DSD file as DoP at its own rate. '
    + 'SDM5, SDM7 and SDM9 are AuraEngine\'s one-bit modulators of the 5th, 7th and 9th order, best first: '
    + 'LA looks ahead (heavier on the CPU), Wide keeps the ultrasonic noise lower.';

/// The list's marks, said once at the end of the tip.
export const OUTFMT_LEGEND = 'High CPU: plays, but takes much of one core per channel on this PC. '
    + 'Grey: too heavy for this PC; pick it to try anyway, a busy PC steps down to a lighter modulator by itself.';

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
    const m = s.match(/^(64|128|256|512|1024)(?::([a-z][A-Za-z0-9]*))?$/);
    return m ? `${m[1]}:${m[2] || baseModulator(m[1])}` : 'pcm';
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

/// The family of the DSD rate a track goes out at: its own with the 48 kHz
/// DSD switch on, else always 44.1 kHz (the player takes a 48 kHz family
/// chain down to it).
export function dsdBase(srcRate, keep48k) {
    return keep48k ? rateBase(srcRate) : 44100;
}

// How a modulator fares at one DoP rate of the family: its row, or null.
function rowOf(k, multiple, base) {
    return (k?.rates || []).find((r) => r.multiple === multiple && r.base === base) || null;
}

const byRank = (caps) => [...(caps?.kinds || [])].sort((a, b) => (a.rank ?? 1e9) - (b.rank ?? 1e9));

/// The selector's list: PCM, then DSD256, DSD128 and DSD64, each the best
/// modulator first, a separator between the rates. From the capabilities
/// (`caps`) of the family `base`: what is not built or not for that rate is
/// left out, what is too heavy for this PC listed, marked and pickable (to try
/// anyway: the player steps down by itself), what is heavy
/// listed and marked; the reasons go to the tip. Each item: { value, short
/// (the closed selector), long (the open list), state, reason, disabled }, or
/// { sep: true }. Before the capabilities (`caps` null): SDM5 at every rate,
/// and the stored choice `saved` at its own. `recheck` adds the re-measure
/// item. `nativeDev`: the device may take DSD natively (`takesNativeDsd`) —
/// DSD1024 and DSD512 first. `dsd48k` (true or false; left out, no item):
/// the 48 kHz DSD switch's item, ticked when on, before the re-check.
export function buildOutFmtList(caps, base, saved, recheck = false, nativeDev = false, dsd48k = null) {
    const items = [{ value: 'pcm', short: 'PCM', long: 'PCM', state: 'ok', reason: '', disabled: false }];
    const want = parseOutFmt(saved);
    // A native rate kept from another device stays listed (the status says
    // why it plays PCM here).
    const native = nativeDev || (want && OUTFMT_NATIVE_MULTIPLES.includes(want.multiple));
    for (const multiple of native ? [...OUTFMT_NATIVE_MULTIPLES, ...OUTFMT_MULTIPLES] : OUTFMT_MULTIPLES) {
        items.push({ sep: true });
        const item = (kind, label, state, reason, load = null, rank = null) => {
            const short = `DSD${multiple} (${label})`;
            const mark = OUTFMT_MARK[state] || '';
            return { value: `${multiple}:${kind}`, short, long: short + mark, label, multiple, state, reason, load, rank, disabled: false };
        };
        if (!caps) {
            const base0 = baseModulator(multiple);
            if (want && want.multiple === multiple && want.kind !== base0) items.push(item(want.kind, kindLabel(want.kind), 'ok', ''));
            items.push(item(base0, kindLabel(base0), 'ok', ''));
            continue;
        }
        for (const k of byRank(caps)) {
            const r = rowOf(k, multiple, base);
            // The same modulator as a plainer kind here (SDM7 LA+ at DSD256 is
            // SDM7 LA): one row, that one's.
            if (!r || r.sameAs || !['ok', 'heavy', 'tooHeavy'].includes(r.state)) continue;
            items.push(item(k.kind, k.label || kindLabel(k.kind), r.state, String(r.reason || ''), Number.isFinite(r.load) ? r.load : null, k.rank ?? null));
        }
    }
    if (dsd48k !== null) {
        const long = `${dsd48k ? '✓ ' : ''}${OUTFMT_DSD48K_LABEL}`;
        items.push({ sep: true });
        items.push({ value: OUTFMT_DSD48K, short: OUTFMT_DSD48K_LABEL, long, state: 'ok', reason: '', disabled: false, checked: !!dsd48k });
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
export function resolveOutFmt(v, caps, base, forced = '') {
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
    // A choice too heavy for this PC that the listener picked to try anyway
    // (`forced` is that very value): it plays as chosen, and the player steps
    // down by itself if this PC falls behind (the status's `dsdNote`).
    if (row && row.state === 'tooHeavy' && forced && forced === value) return { value, note: '' };
    const runs = (state) => kinds.find((k) => {
        const r = rowOf(k, p.multiple, base);
        return r?.state === state && !r.sameAs;
    });
    const fb = runs('ok') || runs('heavy');
    const kind = fb ? fb.kind : baseModulator(p.multiple);
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
    const grey = items.filter((i) => i.state === 'tooHeavy');
    if (grey.length) lines.push(`Too heavy for this PC: ${grey.map((i) => i.short).join(', ')} (pick one to try anyway).`);
    if (chosen?.state === 'tooHeavy') lines.push(`${chosen.label} at DSD${chosen.multiple} is tried anyway: if this PC falls behind, a lighter modulator plays instead.`);
    return lines;
}

/// An item's own tip in the open list: how its modulator fares at its rate
/// on this PC ("SDM7 LA at DSD256: about 72 % of a core per channel: a busy
/// PC may make it step down"); the re-check item says what it does. PCM, and
/// the list from before the capabilities, have none.
export function outFmtItemTip(item) {
    if (!item || item.sep) return '';
    if (item.value === OUTFMT_RECHECK) return 'Measures again which modulators this PC can run.';
    if (item.value === OUTFMT_DSD48K) return OUTFMT_DSD48K_TIP;
    if (!item.multiple || !item.reason) return '';
    const tip = `${item.label} at DSD${item.multiple}: ${item.reason}`;
    return item.state === 'tooHeavy' ? `${tip}. Pick it to try anyway: if this PC falls behind, a lighter modulator plays instead.` : tip;
}

/// The strip's name of the playing DSD output (status `dsd`): the rate's
/// label, then the modulator that plays — after a step down the lighter one:
/// "DSD256 · SDM7 LA", "DSD256 (48k) · SDM5".
export function dsdStripLabel(dsd) {
    const m = dsd?.running || dsd?.modulator;
    return m ? `${dsd.label} · ${kindLabel(m)}` : String(dsd?.label ?? '');
}

/// How DSD reaches the DAC on the output device `id`: natively through its
/// ASIO driver ("asio:<driver>") or, on Linux, an ALSA device's DSD format
/// (the stream's `format` says "DSD native …"), else as DoP.
export const dsdNative = (id, format = '') => isAsioDevice(id) || /^DSD native/.test(String(format ?? ''));

/// The output button's tag for the way DSD goes out: "ASIO" on a DAC's ASIO
/// driver, "native" where an ALSA device took it in its own DSD format
/// (the stream's `format`, Linux), "DoP" elsewhere.
export const dsdWayTag = (id, format = '') => (isAsioDevice(id) ? 'ASIO' : dsdNative(id, format) ? 'native' : 'DoP');

/// The technical line's words for the way DSD goes out: "ASIO native" on a
/// DAC's ASIO driver, "ALSA native" on a Linux device's DSD format (the
/// stream's `format`), "DoP · exclusive" elsewhere (the line said DoP on
/// ASIO too, Anton 20.11).
export const dsdWayLine = (id, format = '') =>
    (isAsioDevice(id) ? 'ASIO native' : dsdNative(id, format) ? 'ALSA native' : 'DoP · exclusive');

/// The DSD output a status `dsd` plays (not a file as it is), as the strip
/// and the heading say it: { label ("DSD256", "DSD256 (48k)"), multiple
/// (256), family (44100 | 48000) }; null when no DSD output plays.
export function dsdPlaying(dsd) {
    const hz = Number(dsd?.rateHz) || 0;
    if (!hz || dsd.asIs) return null;
    const family = hz % 44100 === 0 ? 44100 : 48000;
    return { label: String(dsd.label || `DSD${hz / family}`), multiple: Math.round(hz / family), family };
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

// ── The output format's panel (outfmt-panel.js): its model ────────────────

/// Every rate the panel's first slider shows, the lowest first.
export const PANEL_MULTIPLES = [64, 128, 256, 512, 1024];

/// A Wide kind's plain twin ("sdm7WideLa" → "sdm7La"); a plain kind itself.
export function plainKind(kind) {
    return String(kind ?? '').replace('Wide', '');
}

/// Whether a kind is a Wide one.
export function isWideKind(kind) {
    return String(kind ?? '').includes('Wide');
}

/// How many times the source rate the chain runs for a DSD rate: ×4 for
/// DSD64, ×8 for DSD128, ×16 from DSD256 up.
export function chainFs(multiple) {
    return multiple <= 64 ? 4 : multiple <= 128 ? 8 : 16;
}

/// The chain's rate for a DSD rate of the family `base`: "FS16 · 705.6 kHz".
export function chainLine(multiple, base) {
    const fs = chainFs(multiple);
    const khz = ((base || 44100) * fs / 1000).toFixed(1).replace(/\.0$/, '');
    return `FS${fs} · ${khz} kHz`;
}

// The best of `mods` (lightest first) whose state is one of `states`; the
// lightest when none is.
function bestOf(mods, states) {
    for (let i = mods.length - 1; i >= 0; i--) if (states.includes(mods[i].state)) return mods[i];
    return mods[0] || null;
}

/// The panel's model for the list `items` (`buildOutFmtList`) and the value
/// `value` that plays: the PCM / DSD tab, the rates (those not listed are
/// locked: DSD512 and DSD1024 away from an ASIO driver), the plain modulators
/// of the chosen rate from the lightest to the best — each with its Wide twin
/// when the rate has one — the chosen one, the three ready choices for that
/// rate (`auto`: the best that runs freely here; `max`: the best this PC can
/// run at all; `light`: the lightest) and which of them is on, the map of
/// every modulator at every rate, and the re-check item. On PCM the DSD side
/// shows `lastDsd` (the DSD choice before), or the best free one at DSD256.
/// `dacMax`: the highest rate the listener says this DAC plays (0: no word) —
/// the rates above it are locked too (`why: 'dac'`; an unlisted one `'asio'`).
export function outFmtPanelModel(items, value, lastDsd = '', dacMax = 0) {
    const dsd = (items || []).filter((i) => !i.sep && i.multiple);
    const at = (multiple) => dsd.filter((i) => i.multiple === multiple).map((i) => ({ ...i, kind: i.value.split(':')[1] }));
    const modsAt = (multiple) => {
        const all = at(multiple);
        return all.filter((i) => !isWideKind(i.kind)).reverse()
            .map((i) => ({ ...i, wide: all.find((w) => isWideKind(w.kind) && plainKind(w.kind) === i.kind) || null }));
    };
    const rates = PANEL_MULTIPLES.map((multiple) => {
        const why = !dsd.some((i) => i.multiple === multiple) ? 'asio' : dacMax > 0 && multiple > dacMax ? 'dac' : '';
        return { multiple, locked: !!why, why };
    });
    const open = rates.filter((r) => !r.locked).map((r) => r.multiple);
    const recheck = (items || []).find((i) => i.value === OUTFMT_RECHECK) || null;
    const playing = parseOutFmt(value);
    const pcm = !playing;
    let want = playing || parseOutFmt(lastDsd);
    if (want && !open.includes(want.multiple)) want = playing && rates.some((r) => r.multiple === want.multiple && r.why === 'dac') ? want : null;
    const multiple = want ? want.multiple : open.includes(256) ? 256 : open[open.length - 1] ?? 256;
    const mods = modsAt(multiple);
    const presets = {
        auto: bestOf(mods, ['ok'])?.value || '',
        max: bestOf(mods, ['ok', 'heavy'])?.value || '',
        light: mods[0]?.value || '',
    };
    let index = want ? mods.findIndex((m) => m.kind === plainKind(want.kind)) : -1;
    if (index < 0) index = Math.max(0, mods.findIndex((m) => m.value === presets.auto));
    const mod = mods[index] || null;
    const wideOn = !!(want && mod?.wide && isWideKind(want.kind));
    const chosen = mod ? (wideOn ? mod.wide : mod) : null;
    const preset = !chosen ? 'manual'
        : chosen.value === presets.auto ? 'auto'
        : chosen.value === presets.max ? 'max'
        : chosen.value === presets.light ? 'light' : 'manual';
    const kinds = [];
    for (const i of dsd) {
        const kind = i.value.split(':')[1];
        if (!kinds.some((k) => k.kind === kind)) kinds.push({ kind, label: i.label });
    }
    // Best first: by the catalog's rank; before the capabilities, as listed.
    const rankOf = (kind) => dsd.find((i) => i.value.split(':')[1] === kind)?.rank ?? 1e9;
    kinds.sort((a, b) => rankOf(a.kind) - rankOf(b.kind));
    const map = kinds.map((k) => ({
        kind: k.kind,
        label: k.label,
        cells: rates.map((r) => (r.locked ? null : at(r.multiple).find((i) => i.kind === k.kind) || null)),
    }));
    return { pcm, rates, multiple, mods, index, wide: { can: !!mod?.wide, on: wideOn }, chosen, presets, preset, map, recheck };
}

/// The set the panel lights: the one pressed (`pressed`: 'auto', 'max',
/// 'light' or '') while it still gives what is chosen, else the first that
/// does. Max and Auto often give the same modulator; a press on Max then
/// lights Max, not Auto (Anton 20.11).
export function shownPreset(m, pressed) {
    return pressed && m?.chosen && m.presets?.[pressed] === m.chosen.value ? pressed : m?.preset || 'manual';
}

/// The value the panel picks when the rate slider moves to `multiple`: the
/// same modulator there (Wide kept when that rate has it), else the best that
/// runs freely at that rate. '' for a locked rate.
export function panelValueAtRate(items, value, multiple) {
    const now = outFmtPanelModel(items, value);
    const next = outFmtPanelModel(items, `${multiple}:${baseModulator(multiple)}`);
    if (next.multiple !== multiple || !next.mods.length) return '';
    const kind = now.chosen ? plainKind(now.chosen.value.split(':')[1]) : '';
    const same = next.mods.find((m) => m.kind === kind);
    if (!same) return next.presets.auto;
    return now.wide.on && same.wide ? same.wide.value : same.value;
}

/// A modulator's load for the panel's meter: "35 % of a core per channel",
/// or its state in words before a measurement.
export function loadLine(item) {
    if (!item) return '';
    if (Number.isFinite(item.load) && item.load !== null) return `${Math.round(item.load * 100)} % of a core per channel`;
    return item.state === 'tooHeavy' ? 'too heavy for this PC' : item.state === 'heavy' ? 'high CPU' : 'not measured yet';
}

/// What the panel says under the meters about the chosen modulator.
export function panelHint(item) {
    if (!item) return '';
    if (item.state === 'tooHeavy') return 'Too heavy for this PC. It is tried anyway: if this PC falls behind, a lighter modulator plays instead.';
    if (item.state === 'heavy') return 'Heavy: it plays, but a busy PC may make it step down to a lighter modulator.';
    return 'Runs freely on this PC.';
}

/// The highest rate the listener says the DAC behind the device `id` plays
/// (0: no word), from the kept map `raw` (JSON: { "<device id>": 256 }).
export function dacMaxOf(raw, id) {
    try {
        const n = Number(JSON.parse(raw || '{}')?.[String(id ?? '')]);
        return PANEL_MULTIPLES.includes(n) ? n : 0;
    } catch (_) {
        return 0;
    }
}

/// The DAC model the listener chose from the list for the device `id`
/// (a name of dac-db.js), or ''.
export function dacModelOf(raw, id) {
    try {
        const v = JSON.parse(raw || '{}')?.[String(id ?? '')];
        return typeof v === 'string' ? v : '';
    } catch (_) {
        return '';
    }
}

/// The kept map with the device `id` set to the DAC model `name` the
/// listener chose from the list ('' : taken back, Auto).
export function dacModelSet(raw, id, name) {
    let map = {};
    try { map = JSON.parse(raw || '{}') || {}; } catch (_) { map = {}; }
    const key = String(id ?? '');
    if (name) map[key] = String(name);
    else delete map[key];
    return JSON.stringify(map);
}

/// The kept map with the device `id` set to `multiple` (the top rate: the
/// listener says it plays them all, whatever is known of the DAC; anything
/// that is no rate: the word taken back).
export function dacMaxSet(raw, id, multiple) {
    let map = {};
    try { map = JSON.parse(raw || '{}') || {}; } catch (_) { map = {}; }
    const key = String(id ?? '');
    if (PANEL_MULTIPLES.includes(multiple)) map[key] = multiple;
    else delete map[key];
    return JSON.stringify(map);
}
