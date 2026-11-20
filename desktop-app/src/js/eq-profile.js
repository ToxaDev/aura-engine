// Aura EQ profiles: reading the text other equalizers write and writing ours
// back in the same form. Equalizer APO / Peace config.txt, AutoEq
// "ParametricEQ.txt", REW "Export filter settings as text", squig.link exports
// and GraphicEQ lines (AutoEq, Wavelet) all share one line format:
//   Preamp: -6.0 dB
//   Filter 1: ON PK Fc 105 Hz Gain -3.4 dB Q 0.70
//   GraphicEQ: 20 -1.2; 21 -1.0; ...
// No DOM and no Tauri here, so node can test it.

export const MAX_BANDS = 24;
export const CUT_SLOPES = [6, 12, 18, 24, 36, 48, 72, 96];

const F_MIN = 10, F_MAX = 24000, G_LIM = 30, Q_MIN = 0.1, Q_MAX = 24;
const BW_Q = Math.SQRT1_2;

const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const r2 = v => Math.round(v * 100) / 100;
const r4 = v => Math.round(v * 10000) / 10000;

// APO type → our band type. Cuts read as one 12 dB/oct stage here and are
// merged into steeper cuts afterwards when the stages are a Butterworth set.
const TYPES = {
    PK: 'bell', PEQ: 'bell', MODAL: 'bell',
    LS: 'lowShelf', LSC: 'lowShelf', LSQ: 'lowShelf',
    HS: 'highShelf', HSC: 'highShelf', HSQ: 'highShelf',
    LP: 'highCut', LPQ: 'highCut',
    HP: 'lowCut', HPQ: 'lowCut',
    NO: 'notch',
    BP: 'bandPass',
};

const DEFAULT_Q = { bell: 1, lowShelf: BW_Q, highShelf: BW_Q, highCut: BW_Q, lowCut: BW_Q, notch: 10, bandPass: BW_Q };

// Lines APO understands that have no meaning for a stereo-linked parametric EQ.
const DIRECTIVES = /^(Device|Channel|Include|Copy|Stage|Delay|Convolution|If|ElseIf|Else|EndIf|Eval|LoudnessCorrection|GraphicEQ)\b/i;

// REW writes a header before the filters; it is neither a filter nor an error.
const REW_HEADER = /^(Filter Settings file|Room EQ V|Dated:|Notes:|Equaliser:|Equalizer:|Average\b|Measurement:|Generic\b)/i;

/// "1,250" → "1250" (thousands), "5,0" → "5.0" (decimal comma).
function normNumbers(s) {
    return s.replace(/(\d),(\d{3})(?!\d)/g, '$1$2').replace(/(\d),(\d)/g, '$1.$2');
}

/// Shelf slope in dB per octave (APO "LSC 10.8 dB") → RBJ Q for that gain.
function shelfSlopeToQ(slopeDb, gain) {
    const S = clamp(slopeDb / 12, 0.05, 1);
    const A = Math.pow(10, gain / 40);
    const inv = (A + 1 / A) * (1 / S - 1) + 2;
    return inv > 0 ? 1 / Math.sqrt(inv) : BW_Q;
}

/// Bandwidth in octaves → Q.
function bwToQ(n) {
    const p = Math.pow(2, n);
    return Math.sqrt(p) / (p - 1);
}

/// Butterworth pair Qs of an even-order cut, in the order exportApo writes them.
function butterworthQs(order) {
    const out = [];
    for (let k = 0; k < order / 2; k++) out.push(1 / (2 * Math.cos(Math.PI * (2 * k + 1) / (2 * order))));
    return out;
}

function parseFilter(line, onOff, code, rest) {
    const up = code.toUpperCase();
    rest = normNumbers(rest);
    let type = TYPES[up];
    // "LS 6dB" / "HS 12dB": shelves with a fixed slope given at the corner.
    const fixed = /^\s*(6|12)\s*dB\b/i.exec(rest);
    if ((up === 'LS' || up === 'HS') && fixed) rest = rest.slice(fixed[0].length);
    // "LSC 10.8 dB Fc …": slope in dB per octave before Fc.
    const slopeM = /^\s*([\d.]+)\s*dB\b/i.exec(rest);
    if ((up === 'LSC' || up === 'HSC') && slopeM) rest = rest.slice(slopeM[0].length);
    if (!type) return null;
    const fm = /\bFc\s*([\d.]+)\s*(k?)Hz/i.exec(rest);
    if (!fm) return null;
    const f = Number(fm[1]) * (fm[2] ? 1000 : 1);
    if (!isFinite(f) || f <= 0) return null;
    const gm = /\bGain\s*([-+]?[\d.]+)\s*dB/i.exec(rest);
    const qm = /\bQ\s*([\d.]+)/i.exec(rest);
    const bw = /\bBW\s*Oct\s*([\d.]+)/i.exec(rest);
    const gain = gm ? Number(gm[1]) : 0;
    let q = DEFAULT_Q[type];
    if (qm && Number(qm[1]) > 0) q = Number(qm[1]);
    else if (bw && Number(bw[1]) > 0) q = bwToQ(Number(bw[1]));
    else if (fixed) q = fixed[1] === '6' ? 0.5 : BW_Q;
    else if (slopeM) q = shelfSlopeToQ(Number(slopeM[1]), gain);
    const cut = type === 'lowCut' || type === 'highCut';
    return {
        type,
        f: r2(clamp(f, F_MIN, F_MAX)),
        gain: cut || type === 'notch' || type === 'bandPass' ? 0 : r2(clamp(gain, -G_LIM, G_LIM)),
        q: r4(clamp(q, Q_MIN, Q_MAX)),
        slope: 12,
        on: onOff.toUpperCase() === 'ON',
    };
}

/// Runs of cut stages at one frequency whose Qs are a Butterworth set (the
/// last one may be scaled, as our cuts allow) become one steeper cut. Other
/// stacks — two LR4 halves at Q 0.707, say — stay separate bands.
function mergeCuts(bands) {
    const out = [];
    for (let i = 0; i < bands.length; i++) {
        const b = bands[i];
        if (b.type !== 'lowCut' && b.type !== 'highCut') { out.push(b); continue; }
        let j = i + 1;
        while (j < bands.length && bands[j].type === b.type && bands[j].f === b.f && bands[j].on === b.on) j++;
        let n = j - i, merged = false;
        for (; n > 1 && !merged; n--) {
            if (!CUT_SLOPES.includes(n * 12)) continue;
            const want = butterworthQs(2 * n);
            const run = bands.slice(i, i + n);
            const ok = run.slice(0, -1).every((s, k) => Math.abs(s.q - want[k]) <= want[k] * 0.01 + 0.002);
            if (!ok) continue;
            const q = r4(clamp(run[n - 1].q / want[n - 1] * BW_Q, Q_MIN, Q_MAX));
            out.push({ ...b, slope: n * 12, q });
            i += n - 1;
            merged = true;
        }
        if (!merged) out.push(b);
    }
    return out;
}

/// Reads an equalizer text. Returns
/// `{kind:'apo'|'rew'|'graphic', bands, preamp, graphic, skipped, truncated}`:
/// bands are EqBands (at most 24, `truncated` says when more were there),
/// preamp is the summed Preamp lines in dB or null, graphic is the GraphicEQ
/// points [[Hz, dB], …] sorted by frequency or null, skipped the lines that
/// were understood as something we do not apply.
export function parseProfile(text) {
    const out = { kind: 'apo', bands: [], preamp: null, graphic: null, skipped: [], truncated: false };
    if (typeof text !== 'string') return out;
    let rew = false, channel = null, channelSkip = false;
    const raw = [];
    for (const line0 of text.split(/\r?\n/)) {
        const line = line0.replace(/#.*$/, '').trim();
        if (!line) continue;
        let m;
        if (REW_HEADER.test(line)) { rew = true; continue; }
        if ((m = /^Preamp\s*:\s*([-+]?[\d.,]+)\s*dB/i.exec(line))) {
            const v = Number(normNumbers(m[1]));
            if (isFinite(v) && !channelSkip) out.preamp = r2((out.preamp ?? 0) + v);
            continue;
        }
        if ((m = /^GraphicEQ\s*:\s*(.+)$/i.exec(line))) {
            if (channelSkip) { out.skipped.push(line0.trim()); continue; }
            const pts = m[1].split(';')
                .map(s => s.trim().split(/\s+/).map(v => Number(normNumbers(v))))
                .filter(p => p.length >= 2 && p[0] > 0 && isFinite(p[0]) && isFinite(p[1]))
                .map(p => [p[0], p[1]])
                .sort((a, b) => a[0] - b[0])
                .filter((p, i, a) => i === 0 || p[0] !== a[i - 1][0]);
            if (pts.length > 1) out.graphic = pts;
            continue;
        }
        if ((m = /^Channel\s*:\s*(.*)$/i.exec(line))) {
            const ch = m[1].trim().toUpperCase();
            const all = ch === 'ALL' || ch === '';
            // The first channel set is the one we apply; a later, different
            // set is another channel's correction, which a stereo-linked EQ
            // cannot follow.
            if (all) channelSkip = false;
            else if (channel === null) { channel = ch; channelSkip = false; }
            else channelSkip = ch !== channel;
            out.skipped.push(line0.trim());
            continue;
        }
        if ((m = /^Filter\s*\d*\s*:\s*(ON|OFF)\s+([A-Za-z]+)\b(.*)$/i.exec(line))) {
            if (/^none$/i.test(m[2])) continue;
            if (channelSkip) { out.skipped.push(line0.trim()); continue; }
            const b = parseFilter(line, m[1], m[2], m[3]);
            if (b) raw.push(b); else out.skipped.push(line0.trim());
            continue;
        }
        if (DIRECTIVES.test(line)) { out.skipped.push(line0.trim()); continue; }
        // Anything else is not ours to judge: free text, a REW notes line.
    }
    let bands = mergeCuts(raw);
    if (bands.length > MAX_BANDS) { bands = bands.slice(0, MAX_BANDS); out.truncated = true; }
    out.bands = bands;
    out.kind = rew ? 'rew' : !bands.length && out.graphic ? 'graphic' : 'apo';
    return out;
}

const fmt = (v, d) => String(Number(v.toFixed(d)));

/// Writes an EqState as APO text (also what AutoEq, Peace, Wavelet and REW
/// read). The preamp line is the state's own preamp when set, else
/// `opts.autoPreamp` (the window computes it from the curve), plus the
/// output gain — APO has one gain, and the sum sounds the same.
/// Tilt becomes its two shelves; cuts become Butterworth LPQ/HPQ stages. APO
/// has no first-order low/high-pass, so odd slopes (6, 18 dB/oct) are written
/// one order steeper, with a comment saying so.
export function exportApo(state, opts = {}) {
    const st = state || {};
    const L = [`# Aura EQ${st.name ? ' · ' + String(st.name).replace(/[\r\n]+/g, ' ') : ''}`];
    const pre = typeof st.preamp === 'number' ? st.preamp : typeof opts.autoPreamp === 'number' ? opts.autoPreamp : 0;
    const total = pre + (typeof st.output === 'number' ? st.output : 0);
    L.push(`Preamp: ${total.toFixed(1)} dB`);
    let n = 1;
    const line = (on, code, f, gain, q) => L.push(`Filter ${n++}: ${on ? 'ON' : 'OFF'} ${code} Fc ${fmt(f, 2)} Hz`
        + (gain != null ? ` Gain ${fmt(gain, 2)} dB` : '') + (q != null ? ` Q ${q.toFixed(4)}` : ''));
    for (const b of st.bands || []) {
        const on = b.on !== false;
        const g = Number(b.gain) || 0, q = Number(b.q) || BW_Q;
        switch (b.type) {
            case 'bell': line(on, 'PK', b.f, g, q); break;
            case 'lowShelf': line(on, 'LSC', b.f, g, q); break;
            case 'highShelf': line(on, 'HSC', b.f, g, q); break;
            case 'notch': line(on, 'NO', b.f, null, q); break;
            case 'bandPass': line(on, 'BP', b.f, null, q); break;
            case 'tilt': line(on, 'HSC', b.f, g / 2, 0.6); line(on, 'LSC', b.f, -g / 2, 0.6); break;
            case 'lowCut': case 'highCut': {
                let order = Math.round((Number(b.slope) || 12) / 6);
                if (order % 2) {
                    L.push(`# ${b.slope} dB/oct ${b.type === 'lowCut' ? 'low' : 'high'} cut written as ${(order + 1) * 6} dB/oct (APO has no first-order filter)`);
                    order++;
                }
                const qs = butterworthQs(order);
                qs[qs.length - 1] *= q / BW_Q;
                for (const sq of qs) line(on, b.type === 'lowCut' ? 'HPQ' : 'LPQ', b.f, null, sq);
                break;
            }
        }
    }
    return L.join('\n') + '\n';
}

/// One line for the import preview: "10 filters · preamp -6.0 dB · 2 lines skipped".
export function describe(r) {
    if (!r || (!r.bands?.length && !r.graphic)) return 'Nothing to import: no filters found';
    const parts = [];
    if (r.bands.length) {
        const off = r.bands.filter(b => b.on === false).length;
        parts.push(`${r.bands.length} filter${r.bands.length === 1 ? '' : 's'}${off ? ` (${off} off)` : ''}`);
    }
    if (r.graphic) parts.push(`graphic EQ, ${r.graphic.length} points`);
    if (r.preamp != null) parts.push(`preamp ${r.preamp.toFixed(1)} dB`);
    if (r.truncated) parts.push(`only the first ${MAX_BANDS} kept`);
    if (r.skipped?.length) parts.push(`${r.skipped.length} line${r.skipped.length === 1 ? '' : 's'} skipped`);
    const from = r.kind === 'rew' ? 'REW: ' : '';
    return from + parts.join(' · ');
}
