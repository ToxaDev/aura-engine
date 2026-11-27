// The DACs whose top DSD rate is known. A driver's own answer cannot be
// trusted: Chord's ASIO driver opens DSD512 and DSD1024 for a Mojo 2, which
// then stays silent. So the DAC is told by how it names itself (its USB
// product string, its driver's name, the names of its outputs), and the top
// rate it plays over USB comes from here (or from the listener, per device:
// the output format's panel).
//
// The list itself is dac-db-data.js (built by tools/dac-db/build.mjs from the
// collected sources, each rate with the page it was read on). Each entry here:
// `maker` matches the maker's name in a driver's name, `model` the model's
// name; `max` is the top DSD rate over USB (64 … 1024), `usb` its USB ids,
// `product` the product strings it reports. Where several models fit one
// name, the one that fits more of it counts ("Mojo 2" over "Mojo"); two that
// fit as much are none: unknown is better than wrong.

import { isAsioDevice } from './outfmt.js';
import { norm } from './eq-autoeq.js';
import { DAC_DATA, SHARED_USB_IDS } from './dac-db-data.js';

/// Model patterns written by hand, where the name alone does not tell
/// (a model a longer one contains, Roman numerals, a short form).
const MODEL_PATTERNS = {
    'Chord Mojo': /mojo(?!\s*2)/i,
    'Chord DAVE': /\bdave\b/i,
    'Topping D90 III Sabre': /d90\s*(iii|3)\s*sabre/i,
    'Topping E70 Velvet': /e70\s*velvet|\be70v\b/i,
    'Topping DX5 II': /dx5\s*(ii|2)\b/i,
    'SMSL DO300': /do300(?!\s*ex)/i,
    'iFi ZEN DAC 3': /zen\s*dac\s*(3|v3|iii)\b/i,
    'RME ADI-2 DAC FS': /adi-?2\s*dac/i,
    'Denafrips Ares 12th-1': /ares\s*12th/i,
    'Denafrips Pontus II 12th-1': /pontus\s*(ii|2)\s*12th/i,
    'Holo Audio May': /holo\s*(audio\s*)?may\b|\bmay\s*(dac|kte)\b/i,
    'Mytek Brooklyn DAC': /brooklyn\s*dac(?!\s*\+)/i,
    'Singxer SDA-6 Pro2': /sda-?6\s*pro\s*2/i,
};

/// Other names a maker's drivers go by.
const MAKER_ALSO = { iFi: ['amr'], SMSL: ['s.m.s.l'] };
/// Words that say nothing of the maker ("Holo Audio": "holo" is enough).
const MAKER_FILLER = new Set(['audio', 'hifi', 'labs', 'electronics', 'acoustics', 'design', 'technology']);

const words = (s) => String(s).toLowerCase().match(/[a-z]+|[0-9]+/g) || [];

/// A phrase as a pattern that bears other spacing ("Mojo 2" ~ "Mojo2",
/// "D90SE" ~ "D90 SE") and stands as whole words.
function phrasePattern(phrase) {
    const w = words(phrase);
    return w.length ? `(?<![a-z0-9])${w.join('[\\W_]*')}(?![a-z0-9])` : '';
}

function makerPattern(maker) {
    const own = words(maker).filter((w) => !MAKER_FILLER.has(w));
    const forms = [own.length ? own.join(' ') : maker, ...(MAKER_ALSO[maker] || [])];
    // A short maker written in capitals may come with dots ("S.M.S.L").
    if (/^[A-Z]{2,5}$/.test(maker)) forms.push(maker.split('').join('.'));
    return new RegExp(forms.map(phrasePattern).filter(Boolean).join('|'), 'i');
}

const makerRes = new Map();
function makerRe(maker) {
    if (!makerRes.has(maker)) makerRes.set(maker, makerPattern(maker));
    return makerRes.get(maker);
}

function modelPattern(d) {
    if (MODEL_PATTERNS[d.n]) return MODEL_PATTERNS[d.n];
    const model = d.n.slice(d.k.length).trim() || d.n;
    const parts = [model, ...(d.a || []), ...(d.p || [])].map(phrasePattern).filter(Boolean);
    return new RegExp([...new Set(parts)].join('|'), 'i');
}

/// Every known DAC with a top DSD rate: { maker, model, name, max, usb,
/// product, makerName }. A DAC that plays no DSD stays out: its 0 would read
/// as "every rate".
export const KNOWN_DACS = DAC_DATA.filter((d) => d.m > 0).map((d) => ({
    maker: makerRe(d.k), model: modelPattern(d), name: d.n, max: d.m,
    usb: d.u || [], product: d.p || [], makerName: d.k,
}));

const shared = new Set(SHARED_USB_IDS);
/// Words of a driver's name that name no model.
const GENERIC = new Set(['dac', 'usb', 'audio', 'usbaudio', 'usbdac', 'asio', 'hifi', 'amp', 'pro', 'digital', 'output', 'speakers']);
const squash = (s) => String(s ?? '').toLowerCase().replace(/[^a-z0-9]/g, '');

/// How much of `text` the entry's model fits (0: none).
function fit(d, text) {
    const m = d.model.exec(text);
    return m ? m[0].length : 0;
}

/// The entries that fit `text` best: the most of it; [] when none, two or
/// more when two fit as much.
function best(text, db, ok = () => true) {
    let top = 0;
    let found = [];
    for (const d of db) {
        if (!ok(d)) continue;
        const n = fit(d, text);
        if (n > top) { top = n; found = [d]; } else if (n && n === top) found.push(d);
    }
    return found;
}

/// The known DACs among the USB devices plugged in (`usb`: [{ usb:
/// "vvvv:pppp", product }] from the backend): by the product string it
/// reports, else by its USB id when only one model of the list has it.
export function usbDacs(usb = [], db = KNOWN_DACS) {
    const out = [];
    for (const u of usb || []) {
        const id = String(u?.usb || '').toLowerCase();
        const product = String(u?.product || '');
        let hit = null;
        const exact = db.filter((d) => (d.product || []).some((p) => squash(p) === squash(product)) && squash(product));
        if (exact.length === 1) hit = exact[0];
        if (!hit && product) {
            // The product string names the model ("FiiO K7"), with its maker
            // or under an id of that maker's models.
            const b = best(product, db, (d) => d.maker.test(product) || (d.usb || []).includes(id));
            if (b.length === 1) hit = b[0];
        }
        if (!hit && id && !shared.has(id)) {
            const byId = db.filter((d) => (d.usb || []).includes(id));
            if (byId.length === 1) hit = byId[0];
        }
        if (hit && !out.includes(hit)) out.push(hit);
    }
    return out;
}

/// The known DAC behind the ASIO device `id`: { name, max } (and by: 'usb'
/// when the DAC told its model on the USB bus), or null. First the USB devices
/// plugged in (`usb`): a known DAC of the
/// driver's maker (or the one known DAC there, under a driver that names no
/// maker the list knows). Then the names: the model in the driver's own name
/// (its maker's models first), or the maker in the driver's name and the
/// model in the name of another output Windows lists (`names`: a maker's one
/// driver serving every model, as "ASIO Chord 44.1Khz 768Khz" beside
/// "Digital output (Mojo2)"). Two different models: null. Any device that is
/// no ASIO driver: null.
export function knownDac(id, names = [], db = KNOWN_DACS, usb = []) {
    if (!isAsioDevice(id)) return null;
    const driver = String(id).replace(/^asio:/, '');
    const result = (d, by) => (by === 'usb' ? { name: d.name, max: d.max, by } : { name: d.name, max: d.max });
    const onUsb = usbDacs(usb, db);
    if (onUsb.length) {
        const own = onUsb.filter((d) => d.maker.test(driver));
        if (own.length === 1) return result(own[0], 'usb');
        const makerKnown = db.some((d) => d.maker.test(driver));
        if (!own.length && !makerKnown && onUsb.length === 1) return result(onUsb[0], 'usb');
    }
    const found = new Set();
    const byMaker = best(driver, db, (d) => d.maker.test(driver));
    // Without its maker, only a model's name that says something
    // ("ADI-2 DAC", not "DAC"): a driver's generic words name no model.
    const telling = (d) => {
        const m = d.model.exec(driver);
        return !!m && squash(m[0]).length >= 4 && !GENERIC.has(squash(m[0]));
    };
    const own = byMaker.length ? byMaker : best(driver, db, telling);
    if (own.length === 1) found.add(own[0]);
    else if (own.length > 1) return null;
    for (const n of names) {
        const text = String(n ?? '');
        if (/^asio\s*:/i.test(text)) continue;   // another driver, not an output of this one
        const hit = best(text, db, (d) => d.maker.test(driver));
        if (hit.length === 1) found.add(hit[0]);
        else if (hit.length > 1) return null;
    }
    if (found.size !== 1) return null;
    const [d] = found;
    return result(d, 'name');
}

/// The maker of an entry ("Chord", "Holo Audio", …).
const makerOf = (d) => d.makerName || d.name.split(' ')[0];

/// The maker behind the ASIO device `id` when its driver names one the list
/// knows ("ASIO Chord 44.1Khz 768Khz": 'Chord'), else ''. Chord's one driver
/// serves every model and Windows names the output "Chord Async USB
/// 44.1kHz-768kHz": the maker is known, the model is not (Anton 20.11).
export function dacMakerOf(id, db = KNOWN_DACS) {
    if (!isAsioDevice(id)) return '';
    const driver = String(id).replace(/^asio:/, '');
    const d = db.find((x) => x.maker.test(driver));
    return d ? makerOf(d) : '';
}

/// The list to choose a DAC from: { maker, models: [{ name, max }] } by
/// maker, the device's own maker (`first`) at the top, the others by name.
export function dacChoices(first = '', db = KNOWN_DACS) {
    const by = new Map();
    for (const d of db) {
        const m = makerOf(d);
        if (!by.has(m)) by.set(m, []);
        by.get(m).push({ name: d.name, max: d.max });
    }
    const makers = [...by.keys()].sort((a, b) => (a === first ? -1 : b === first ? 1 : a.localeCompare(b)));
    return makers.map((maker) => ({ maker, models: by.get(maker).sort((a, b) => a.name.localeCompare(b.name)) }));
}

/// A device's name as its row in the device list shows it: without the
/// "ASIO:" its section says, a rate range as "44.1–768 kHz", a driver that
/// starts with "ASIO" with the maker first ("ASIO Chord …" → "Chord ASIO …").
export function tidyDeviceName(name) {
    let s = String(name ?? '').replace(/^\s*asio\s*:\s*/i, '');
    s = s.replace(/(\d+(?:[.,]\d+)?)\s*k?hz\s*(?:-|–|to|~|\s)\s*(\d+(?:[.,]\d+)?)\s*khz/gi, '$1–$2 kHz');
    s = s.replace(/\b(\d+(?:[.,]\d+)?)\s*khz\b/gi, '$1 kHz');
    s = s.replace(/^asio\s+(?!asio\b)(\S+)/i, '$1 ASIO');
    return s.replace(/\s{2,}/g, ' ').trim();
}

/// The DAC behind a device of the list ({ name, max, by? }, or null): an
/// ASIO driver as `knownDac` tells it (with the other devices' `names`); any
/// other device by its own name, with the USB devices only when its name
/// says the maker (a Realtek endpoint is not the one USB DAC plugged in).
export function deviceDac(id, name, names = [], usb = [], db = KNOWN_DACS) {
    if (isAsioDevice(id)) return knownDac(id, names, db, usb);
    const as = 'asio:' + String(name ?? '');
    return knownDac(as, [], db, dacMakerOf(as, db) ? usb : []);
}

/// A DAC of the list by its name: { name, max }, or null.
export function dacByName(name, db = KNOWN_DACS) {
    const d = name ? db.find((x) => x.name === name) : null;
    return d ? { name: d.name, max: d.max } : null;
}

/// The DACs of the list for what is typed (`query`), as the EQ window finds
/// headphones: every word in the name, the name that starts with it first,
/// then the shorter one. Nothing typed: the device's own maker (`maker`)
/// first, then all by name. [{ name, max, maker }], at most `limit`.
export function searchDacs(query, { maker = '', limit = 1000, db = KNOWN_DACS } = {}) {
    const all = db.map((d) => ({ name: d.name, max: d.max, maker: makerOf(d) }));
    const whole = norm(query);
    if (!whole) {
        return all.sort((a, b) => (a.maker === maker) !== (b.maker === maker) ? (a.maker === maker ? -1 : 1)
            : a.name.localeCompare(b.name)).slice(0, limit);
    }
    const qw = whole.split(' ');
    const scored = [];
    for (const d of all) {
        const n = norm(d.name);
        const squashed = n.replace(/ /g, '');
        if (!qw.every((w) => n.includes(w) || squashed.includes(w))) continue;
        const s = n === whole ? 0 : n.startsWith(whole) ? 1
            : qw.every((w) => n.startsWith(w) || n.includes(' ' + w)) ? 2 : 3;
        scored.push({ d, s, n });
    }
    scored.sort((a, b) => a.s - b.s || (a.d.maker === maker ? 0 : 1) - (b.d.maker === maker ? 0 : 1)
        || a.n.length - b.n.length || a.n.localeCompare(b.n));
    return scored.slice(0, limit).map((x) => x.d);
}
