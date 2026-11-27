// The output format's panel: what drops down from the format button after
// the device button. Two tabs, PCM and DSD. On DSD: three ready choices
// (Auto, Max, Light), a slider for the rate (DSD64 … DSD1024; the rates this
// device cannot take are struck out), a slider for the modulator from the
// lightest to the best, coloured by how each fares on this PC (green runs
// freely, amber heavy, grey too heavy), Wide as its own switch, the CPU load
// and the chain's rate, and under "All modes" the map of every modulator at
// every rate. The model is outfmt.js's (`outFmtPanelModel`). Under the rate
// slider, "Your DAC": Auto (the DAC known by name, dac-db.js) or the
// listener's top rate for this device; the rates above it are struck out,
// and clicking one allows it.
//
// A slider previews while it is dragged and picks when it is let go, so one
// drag restarts the stream once. The panel stays open after a pick; only a
// press outside (which presses nothing else) or Escape closes it.

import { addPopup } from './popups.js';
import { searchDacs, KNOWN_DACS } from './dac-db.js';
import { outFmtPanelModel, shownPreset, panelValueAtRate, chainLine, loadLine, panelHint, OUTFMT_RECHECK,
    OUTFMT_DSD48K, OUTFMT_DSD48K_LABEL, OUTFMT_DSD48K_TIP } from './outfmt.js';

const STATE_CLS = { ok: 'ok', heavy: 'heavy', tooHeavy: 'too' };

const HTML = `
<div class="ofp-tabs" role="tablist">
  <button type="button" role="tab" data-tab="pcm">PCM</button>
  <button type="button" role="tab" data-tab="dsd">DSD<span class="ofp-via" data-el="via"></span></button>
</div>
<div class="ofp-scroll">
<div class="ofp-body" data-el="dsd">
  <div class="ofp-presets" data-el="presets">
    <button type="button" data-preset="auto" data-tip="The best modulator that runs freely on this PC at this rate.">Auto</button>
    <button type="button" data-preset="max" data-tip="The best modulator this PC can run at this rate, even if it is heavy.">Max</button>
    <button type="button" data-preset="light" data-tip="The lightest modulator at this rate.">Light</button>
    <span class="ofp-manual" data-el="manual">Manual</span>
  </div>
  <div class="ofp-ctl">
    <div class="ofp-head"><span class="ofp-name">DSD rate</span><span class="ofp-val" data-el="rateVal"></span></div>
    <div class="ofp-slider"><div class="ofp-track" data-el="rateTrack"></div><input type="range" min="0" max="4" step="1" data-el="rate" aria-label="DSD rate"></div>
    <div class="ofp-stops" data-el="rateStops"></div>
    <label class="ofp-dac" data-el="dacRow" data-tip="Your DAC, for this device: its top DSD rate comes from the list of known DACs. Auto finds it by the names Windows gives; when it cannot (Chord's driver serves every Chord), choose it here, or only the top rate. A driver may accept a rate its DAC stays silent at, so the driver is not asked."><span>Your DAC</span><input type="text" data-el="dacIn" role="combobox" aria-label="Your DAC" aria-autocomplete="list" aria-expanded="false" autocomplete="off" spellcheck="false"></label>
    <div class="ofp-dacres" data-el="dacRes" role="listbox" aria-label="DACs" hidden></div>
  </div>
  <div class="ofp-ctl">
    <div class="ofp-head"><span class="ofp-name">Modulator</span><span class="ofp-val" data-el="modVal"></span></div>
    <div class="ofp-slider"><div class="ofp-track" data-el="modTrack"></div><input type="range" min="0" max="4" step="1" data-el="mod" aria-label="Modulator"></div>
    <div class="ofp-stops" data-el="modStops"></div>
    <div class="ofp-ends"><span>lighter on the CPU</span><span>cleaner</span></div>
  </div>
  <label class="ofp-wide" data-el="wideRow"><input type="checkbox" data-el="wide"><span>Wide</span><small data-el="wideNote"></small></label>
  <label class="ofp-wide ofp-d48" data-el="d48Row" data-tip="${OUTFMT_DSD48K_TIP}"><input type="checkbox" data-el="d48"><span>${OUTFMT_DSD48K_LABEL}</span></label>
  <div class="ofp-meters">
    <div class="ofp-meter"><span class="ofp-cap">CPU</span><span class="ofp-num" data-el="load"></span><span class="ofp-sub" data-el="loadSub"></span><div class="ofp-bar"><b data-el="loadBar"></b></div></div>
    <div class="ofp-meter"><span class="ofp-cap">Chain</span><span class="ofp-num" data-el="chain"></span><span class="ofp-sub">the FS setting is set aside</span></div>
  </div>
  <p class="ofp-hint" data-el="hint"></p>
  <button type="button" class="ofp-all" data-el="all" aria-expanded="false">All modes</button>
  <div class="ofp-map" data-el="mapWrap" hidden><table data-el="map"></table>
    <p class="ofp-legend"><i class="ok"></i>runs freely<i class="heavy"></i>heavy<i class="too"></i>too heavy<span>% of a core per channel</span></p></div>
</div>
<div class="ofp-body" data-el="pcm">
  <p class="ofp-pcm">PCM plays the chain's output as it is, at the rate the FS setting gives. There is nothing to set here.</p>
</div>
</div>
<div class="ofp-foot"><span data-el="foot"></span><button type="button" class="ofp-recheck" data-el="recheck"></button></div>`;

/**
 * Turns `button` into the opener of the panel. `onPick(value)` gets the
 * chosen format ('pcm', "256:sdm7La") or the re-check item's value;
 * `canOpen()` false keeps it shut; `onToggle(open)` hears it open and close;
 * `tipOf(item)` gives a modulator's own tip; `onDacMax(multiple)` hears the
 * highest rate the listener says this DAC plays (0: the word taken back, Auto). Returns { setItems(items, value,
 * ctx), open(), close(), isOpen(), el }. `ctx`: { asio, base, lastDsd, dacMax (the limit in force),
 * dacName, dacSaid (the listener's word, 0: none), dacKnown ({ name, max } of the DAC in force: chosen or found, or null),
 * dacAuto (the DAC found by its name, or null), dacPicked (the DAC chosen from the list, or null), dacMaker (the
 * driver's maker when the list knows it), dacChoices ([{ maker, models: [{ name, max }] }]: the list to choose from) }.
 * `onDacModel(name)` hears the DAC chosen from the list ('': Auto).
 */
export function createOutFmtPanel(button, { onPick, onDacMax = () => {}, onDacModel = () => {}, canOpen = () => true, onToggle = () => {}, tipOf = () => '' } = {}) {
    const doc = button.ownerDocument;
    const panel = doc.createElement('div');
    panel.className = 'pick-list ofp';
    panel.id = `${button.id || 'outfmt'}Panel`;
    panel.setAttribute('role', 'dialog');
    panel.setAttribute('aria-label', 'Output format');
    const pop = typeof panel.showPopover === 'function';
    if (pop) panel.setAttribute('popover', 'manual');
    panel.innerHTML = HTML;
    doc.body.appendChild(panel);
    button.setAttribute('aria-haspopup', 'dialog');
    button.setAttribute('aria-expanded', 'false');
    button.setAttribute('aria-controls', panel.id);
    const el = {};
    for (const n of panel.querySelectorAll('[data-el]')) el[n.dataset.el] = n;

    let items = [];
    let value = 'pcm';
    let ctx = { asio: false, base: 44100, lastDsd: '', dacMax: 0, dacName: '', dacSaid: 0, dacKnown: null,
        dacAuto: null, dacPicked: null, dacMaker: '', dacChoices: [] };
    let pending = null;      // a slider's preview, not picked yet
    let shown = false;
    let lockNote = '';
    let mapOpen = false;
    let pressedPreset = '';  // the set last pressed (Auto, Max, Light): it stays lit while it gives what is chosen

    const model = () => outFmtPanelModel(items, pending ?? value, pending ? '' : ctx.lastDsd, ctx.dacMax);

    function stops(host, list) {
        host.replaceChildren(...list.map((s, i) => {
            const b = doc.createElement('button');
            b.type = 'button';
            b.tabIndex = -1;
            b.textContent = s.text;
            b.className = s.cls;
            if (s.tip) b.dataset.tip = s.tip;
            if (s.pick !== undefined) b.dataset.pick = s.pick;
            const at = list.length > 1 ? i / (list.length - 1) : 0;
            b.style.left = `${at * 100}%`;
            b.style.transform = `translateX(${i === 0 ? '0' : i === list.length - 1 ? '-100%' : '-50%'})`;
            return b;
        }));
    }

    function track(host, classes) {
        host.replaceChildren(...classes.map((c) => {
            const i = doc.createElement('i');
            i.className = c;
            return i;
        }));
    }

    function render() {
        const m = model();
        for (const t of panel.querySelectorAll('[data-tab]')) t.setAttribute('aria-selected', String((t.dataset.tab === 'pcm') === m.pcm));
        el.via.textContent = ctx.way || (ctx.asio ? 'ASIO' : 'DoP');
        el.d48.checked = !!ctx.dsd48k;
        el.pcm.hidden = !m.pcm;
        el.dsd.hidden = m.pcm;
        const c = m.chosen;

        const lit = shownPreset(m, pressedPreset);
        for (const b of el.presets.querySelectorAll('[data-preset]')) b.setAttribute('aria-pressed', String(b.dataset.preset === lit));
        el.manual.classList.toggle('on', lit === 'manual');

        // The rate.
        const ri = m.rates.findIndex((r) => r.multiple === m.multiple);
        if (el.rate.value !== String(ri)) el.rate.value = String(ri);
        el.rateVal.innerHTML = `DSD${m.multiple}<small>${(m.multiple * ctx.base / 1e6).toFixed(1)} MHz</small>`;
        track(el.rateTrack, m.rates.slice(1).map((r) => (r.locked ? 'lock' : 'plain')));
        stops(el.rateStops, m.rates.map((r) => ({
            text: String(r.multiple),
            cls: r.locked ? 'lock' : r.multiple === m.multiple ? 'on' : '',
            tip: r.why === 'dac' ? `${ctx.dacName ? `${ctx.dacName} does` : 'Marked: this DAC does'} not play DSD${r.multiple}. Click to allow it anyway.`
                : r.locked ? `DSD${r.multiple} goes only natively, through the DAC's ASIO driver (the "ASIO: …" devices).` : '',
            pick: `rate:${r.multiple}`,
        })));

        // A driver may take a rate its DAC stays silent at, and cannot be
        // asked: the top rate is the known DAC's (Auto), or the listener's.
        dacSelect(m);

        // The modulator.
        el.mod.max = String(Math.max(0, m.mods.length - 1));
        el.mod.disabled = m.mods.length < 2;
        if (el.mod.value !== String(m.index)) el.mod.value = String(m.index);
        el.modVal.textContent = c ? c.label : '';
        track(el.modTrack, m.mods.slice(1).map((x) => STATE_CLS[x.state] || 'ok'));
        stops(el.modStops, m.mods.map((x, i) => ({
            text: x.label.replace(/^SDM/, ''),
            cls: `${i === m.index ? 'on ' : ''}${x.state === 'ok' ? '' : STATE_CLS[x.state] || ''}`.trim(),
            tip: tipOf(x),
            pick: x.value,
        })));

        // Wide.
        el.wide.checked = m.wide.on;
        el.wide.disabled = !m.wide.can;
        el.wideRow.classList.toggle('off', !m.wide.can);
        el.wideNote.textContent = m.wide.can ? 'less ultrasonic noise, a little more noise in the audio band'
            : m.multiple < 128 ? 'from DSD128 up' : c ? `${c.label} has no Wide form` : '';

        // The meters and the hint.
        const measured = !!c && Number.isFinite(c.load) && c.load !== null;
        el.load.textContent = measured ? `${Math.round(c.load * 100)} %` : loadLine(c);
        el.loadSub.textContent = measured ? 'of a core per channel' : '';
        const load = measured ? Math.min(1, c.load) : 0;
        el.loadBar.style.width = `${Math.round(load * 100)}%`;
        el.loadBar.className = c ? STATE_CLS[c.state] || '' : '';
        el.chain.textContent = chainLine(m.multiple, ctx.base);
        el.hint.textContent = lockNote || panelHint(c);
        el.hint.classList.toggle('warn', !!lockNote || (c && c.state !== 'ok'));

        // The map.
        el.all.setAttribute('aria-expanded', String(mapOpen));
        el.mapWrap.hidden = !mapOpen;
        if (mapOpen) {
            const head = `<tr><th></th>${m.rates.map((r) => `<th class="${r.locked ? 'lock' : ''}">${r.multiple}</th>`).join('')}</tr>`;
            el.map.innerHTML = head + m.map.map((row) => `<tr><th class="row">${row.label}</th>${row.cells.map((cell, i) => {
                if (m.rates[i].locked) return '<td class="lock"></td>';
                if (!cell) return '<td class="na">·</td>';
                const on = c && cell.value === c.value ? ' on' : '';
                const pct = Number.isFinite(cell.load) && cell.load !== null ? Math.round(cell.load * 100) : '';
                return `<td><button type="button" tabindex="-1" class="${STATE_CLS[cell.state] || 'ok'}${on}" data-pick="${cell.value}" aria-label="${cell.short}">${pct}</button></td>`;
            }).join('')}</tr>`).join('');
            for (const b of el.map.querySelectorAll('[data-pick]')) {
                const cell = m.map.flatMap((r) => r.cells).find((x) => x && x.value === b.dataset.pick);
                const tip = tipOf(cell);
                if (tip) b.dataset.tip = tip;
            }
        }

        // The foot.
        el.foot.textContent = m.pcm ? 'PCM' : c ? `DSD${m.multiple} · ${c.label} · ${ctx.way === 'native' ? 'ALSA native' : ctx.asio ? 'ASIO native' : 'DoP'}` : '';
        el.recheck.hidden = !m.recheck;
        if (m.recheck) {
            el.recheck.textContent = m.recheck.short;
            el.recheck.dataset.tip = 'Measures again which modulators this PC can run.';
        }
    }

    function place() {
        const r = button.getBoundingClientRect();
        panel.style.left = '0px';
        panel.style.top = '0px';
        const box = panel.getBoundingClientRect();
        const vw = doc.defaultView.innerWidth, vh = doc.defaultView.innerHeight;
        // Never past the window's edge: under the button when it fits there,
        // else from as high as it needs (over the button too), and no taller
        // than the window — the middle scrolls, the tabs and the foot stay.
        panel.style.maxHeight = `${vh - 12}px`;
        const h = Math.min(panel.getBoundingClientRect().height, vh - 12);
        const under = r.bottom + 3;
        const top = under + h <= vh - 6 ? under : Math.max(6, vh - 6 - h);
        panel.style.left = `${Math.round(Math.max(6, Math.min(r.left, vw - box.width - 6)))}px`;
        panel.style.top = `${Math.round(top)}px`;
    }

    function open() {
        if (shown || !canOpen()) return;
        shown = true;
        pending = null;
        lockNote = '';
        render();
        if (pop) panel.showPopover();
        panel.classList.add('open');
        button.setAttribute('aria-expanded', 'true');
        button.classList.add('open');
        place();
        onToggle(true);
    }

    function close() {
        if (!shown) return;
        shown = false;
        pending = null;
        panel.classList.remove('open');
        if (pop) try { panel.hidePopover(); } catch (_) { /* gone already */ }
        button.setAttribute('aria-expanded', 'false');
        button.classList.remove('open');
        onToggle(false);
    }

    // Picks `v` now: the panel shows it at once, the caller stores and sends
    // it. What plays already is not sent again (a locked rate snaps back to
    // it); `keepNote` leaves the note about a locked rate up.
    function pick(v, keepNote = false) {
        if (!v) return;
        pending = null;
        if (!keepNote) lockNote = '';
        const same = v === value;
        if (v !== OUTFMT_RECHECK && v !== OUTFMT_DSD48K) value = v;
        render();
        if (!same) onPick?.(v);
        if (shown) place();
    }

    // The value for the rate slider's stop `i`; a locked stop snaps back to
    // the highest open rate and says why.
    function rateValue(i) {
        const m = model();
        const r = m.rates[i];
        if (!r) return '';
        if (r.why === 'dac') {
            // Picking a struck-out rate allows it, and those under it: the
            // listener's word, over what is known of the DAC.
            ctx = { ...ctx, dacMax: r.multiple, dacName: '', dacSaid: r.multiple };
            onDacMax(r.multiple);
            lockNote = '';
            return panelValueAtRate(items, pending ?? dsdNow(m), r.multiple);
        }
        if (r.locked) {
            lockNote = `DSD${r.multiple} goes only natively, through the DAC's ASIO driver.`;
            const top = [...m.rates].reverse().find((x) => !x.locked);
            return top ? panelValueAtRate(items, pending ?? dsdNow(m), top.multiple) : '';
        }
        lockNote = '';
        return panelValueAtRate(items, pending ?? dsdNow(m), r.multiple);
    }
    // The DSD choice the panel shows (on PCM: the one it would pick).
    const dsdNow = (m) => m.chosen?.value || value;

    // "Your DAC": a field that searches the list of known DACs as it is
    // typed in (as the EQ window finds headphones). Shut, it says what is in
    // force: Auto and what it found (or what is missing), the DAC chosen, or
    // only a top rate. Open, under it: Auto first, the DACs found (the
    // driver's maker first when nothing is typed), then the top rates alone.
    // Not redrawn while in use: the panel redraws on every status poll.
    let dacRates = [];
    let dacActive = 0;
    function dacAutoText() {
        const a = ctx.dacAuto;
        return a ? `Auto · ${a.name}${a.by === 'usb' ? ' (USB)' : ''}: DSD${a.max}`
            : ctx.dacMaker ? `Auto · ${ctx.dacMaker}, model not named: choose it`
            : 'Auto · not in the list: every rate';
    }
    function dacText() {
        if (ctx.dacSaid) return `Only the top rate: DSD${ctx.dacSaid}`;
        if (ctx.dacPicked) return `${ctx.dacPicked.name} · DSD${ctx.dacPicked.max}`;
        return dacAutoText();
    }
    function dacSelect(m) {
        dacRates = m.rates.filter((r) => r.why !== 'asio' || r.multiple === ctx.dacSaid).map((r) => r.multiple);
        const inp = el.dacIn;
        if (doc.activeElement === inp) return;
        inp.value = dacText();
        inp.title = inp.value;
    }
    function dacRows() {
        const q = el.dacIn.value;
        const found = searchDacs(q, { maker: ctx.dacMaker });
        const cur = ctx.dacSaid ? `rate:${ctx.dacSaid}` : ctx.dacPicked ? `dac:${ctx.dacPicked.name}` : '0';
        const rows = [{ v: '0', text: dacAutoText(), cls: 'auto' }];
        let maker = null;
        for (const d of found) {
            if (!q.trim() && d.maker !== maker) { maker = d.maker; rows.push({ head: d.maker === ctx.dacMaker ? `${d.maker}: your driver` : d.maker }); }
            rows.push({ v: `dac:${d.name}`, text: d.name, rate: `DSD${d.max}` });
        }
        if (q.trim() && !found.length) rows.push({ head: 'No DAC of the list has this name' });
        rows.push({ head: 'Or only the top rate' });
        rows.push({ rates: dacRates.map((x) => ({ v: `rate:${x}`, text: `DSD${x}` })) });
        return { rows, cur };
    }
    function drawDacRes() {
        const { rows, cur } = dacRows();
        const picks = [];
        const row = (o, cls) => {
            const b = doc.createElement('div');
            b.className = `ofp-dacrow ${cls || ''}${o.v === cur ? ' on' : ''}`.trim();
            b.setAttribute('role', 'option');
            b.setAttribute('aria-selected', String(o.v === cur));
            b.dataset.v = o.v;
            b.dataset.i = String(picks.length);
            picks.push(b);
            b.textContent = o.text;
            if (o.rate) {
                const s = doc.createElement('span');
                s.className = 'ofp-dacrate';
                s.textContent = o.rate;
                b.append(s);
            }
            return b;
        };
        el.dacRes.replaceChildren(...rows.map((r) => {
            if (r.head) {
                const n = doc.createElement('div');
                n.className = 'ofp-dachead';
                n.textContent = r.head;
                return n;
            }
            if (r.rates) {
                const n = doc.createElement('div');
                n.className = 'ofp-dacrates';
                n.append(...r.rates.map((o) => row(o, 'chip')));
                return n;
            }
            return row(r, r.cls);
        }));
        dacActive = Math.max(0, Math.min(dacActive, picks.length - 1));
        paintDacActive();
    }
    function paintDacActive() {
        for (const b of el.dacRes.querySelectorAll('[data-i]')) b.classList.toggle('active', Number(b.dataset.i) === dacActive);
        el.dacRes.querySelector(`[data-i="${dacActive}"]`)?.scrollIntoView?.({ block: 'nearest' });
    }
    function openDacRes() {
        el.dacIn.placeholder = `Search ${KNOWN_DACS.length} DACs…`;
        el.dacRes.hidden = false;
        el.dacIn.setAttribute('aria-expanded', 'true');
        dacActive = 0;
        drawDacRes();
        place();
    }
    function shutDacRes() {
        if (el.dacRes.hidden) return;
        el.dacRes.hidden = true;
        el.dacIn.setAttribute('aria-expanded', 'false');
        place();
    }
    function chooseDac(v) {
        if (v.startsWith('dac:')) setDacModel(v.slice(4));
        else if (v.startsWith('rate:')) setDacMax(Number(v.slice(5)) || 0);
        else setDacModel('');
        el.dacIn.blur();
    }

    // A DAC chosen from the list ('': Auto): its top rate is the limit now,
    // over what was found by name; a top rate said before is taken back.
    function setDacModel(name) {
        const d = (ctx.dacChoices || []).flatMap((g) => g.models).find((x) => x.name === name) || null;
        ctx = { ...ctx, dacPicked: d, dacKnown: d || ctx.dacAuto };
        onDacModel(d ? d.name : '');
        setDacMax(0);
    }

    // A top rate chosen in "Your DAC" (0: Auto). When it is under the
    // rate that plays, the panel steps down to the highest rate allowed.
    function setDacMax(said) {
        const max = said || ctx.dacKnown?.max || 0;
        ctx = { ...ctx, dacSaid: said, dacMax: max, dacName: said ? '' : ctx.dacKnown?.name || '' };
        onDacMax(said);
        const m = model();
        if (!m.pcm && max > 0 && m.multiple > max) {
            const top = [...m.rates].reverse().find((r) => !r.locked);
            if (top) {
                pick(panelValueAtRate(items, dsdNow(m), top.multiple));
                lockNote = `DSD${m.multiple} is above this DAC's top rate: DSD${top.multiple} plays.`;
            }
        } else {
            lockNote = '';
        }
        render();
    }

    panel.addEventListener('click', (e) => {
        const t = e.target.closest?.('button');
        if (!t || !panel.contains(t)) return;
        if (t.dataset.tab) {
            const m = model();
            if (t.dataset.tab === 'pcm') pick('pcm');
            else if (m.pcm) pick(m.chosen?.value || '');
        } else if (t.dataset.preset) {
            pressedPreset = t.dataset.preset;
            pick(model().presets[t.dataset.preset]);
        } else if (t.dataset.pick?.startsWith('rate:')) {
            pick(rateValue(rateIndex(model(), Number(t.dataset.pick.slice(5)))), true);
        } else if (t.dataset.pick) {
            pick(t.dataset.pick);
        } else if (t === el.all) {
            mapOpen = !mapOpen;
            render();
            place();
            if (mapOpen) el.mapWrap.scrollIntoView?.({ block: 'nearest' });
        } else if (t === el.recheck) {
            pick(OUTFMT_RECHECK);
        }
    });
    el.rate.addEventListener('input', () => { pending = rateValue(Number(el.rate.value)) || pending; render(); });
    el.rate.addEventListener('change', () => pick(pending ?? rateValue(Number(el.rate.value)), true));
    el.mod.addEventListener('input', () => {
        const m = model();
        const x = m.mods[Number(el.mod.value)];
        if (x) pending = m.wide.on && x.wide ? x.wide.value : x.value;
        lockNote = '';
        render();
    });
    el.mod.addEventListener('change', () => pick(pending ?? ''));
    el.dacIn.addEventListener('focus', () => { el.dacIn.value = ''; openDacRes(); });
    el.dacIn.addEventListener('input', () => { dacActive = 0; drawDacRes(); place(); });
    el.dacIn.addEventListener('keydown', (e) => {
        const n = el.dacRes.querySelectorAll('[data-i]').length;
        if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            dacActive = Math.max(0, Math.min(n - 1, dacActive + (e.key === 'ArrowDown' ? 1 : -1)));
            paintDacActive();
        } else if (e.key === 'Enter') {
            e.preventDefault();
            const b = el.dacRes.querySelector(`[data-i="${dacActive}"]`);
            if (b) chooseDac(b.dataset.v);
        }
    });
    // A press on the list keeps the field's focus, so it is not shut before
    // the click picks.
    el.dacRes.addEventListener('mousedown', (e) => e.preventDefault());
    el.dacRes.addEventListener('click', (e) => {
        const b = e.target.closest?.('[data-v]');
        if (b) chooseDac(b.dataset.v);
    });
    el.dacRes.addEventListener('mousemove', (e) => {
        const b = e.target.closest?.('[data-i]');
        if (b && Number(b.dataset.i) !== dacActive) { dacActive = Number(b.dataset.i); paintDacActive(); }
    });
    // Left: catch up with what came meanwhile (the select alone: a full
    // redraw now would replace the stop being pressed).
    el.dacIn.addEventListener('blur', () => { shutDacRes(); dacSelect(model()); });
    // The 48 kHz DSD switch: ticked or not, what is chosen stays.
    el.d48.addEventListener('change', () => onPick?.(OUTFMT_DSD48K));
    el.wide.addEventListener('change', () => {
        const m = model();
        const x = m.mods[m.index];
        if (x) pick(el.wide.checked && x.wide ? x.wide.value : x.value);
    });

    button.addEventListener('click', () => (shown ? close() : open()));
    button.addEventListener('keydown', (e) => {
        if (!shown && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
            e.preventDefault();
            open();
        }
    });
    // The app's popup rule (popups.js): a press beside the open panel only
    // closes it, nothing under the pointer is pressed; Esc closes it and
    // gives the focus back to the button. The window losing the focus
    // leaves it open; a new window size places it again.
    addPopup({
        isOpen: () => shown,
        inside: (t) => panel.contains(t) || button.contains(t),
        close,
        // Esc in the DAC search shuts the search alone.
        escape: () => { if (!el.dacRes.hidden) { el.dacIn.blur(); return; } close(); button.focus?.(); },
    });
    doc.defaultView.addEventListener('resize', () => { if (shown) place(); });

    return {
        el: panel,
        setItems(next, v, c = {}) {
            items = next;
            value = v;
            ctx = { asio: !!c.asio, base: c.base || 44100, lastDsd: c.lastDsd || '', dacMax: c.dacMax || 0, dacName: c.dacName || '',
                dacSaid: c.dacSaid || 0, dacKnown: c.dacKnown || null, dacAuto: c.dacAuto || null, dacPicked: c.dacPicked || null,
                dacMaker: c.dacMaker || '', dacChoices: c.dacChoices || [], way: c.way || '', dsd48k: !!c.dsd48k };
            if (shown) render();
        },
        open,
        close,
        isOpen: () => shown,
    };
}

// The index of the rate `multiple` on the rate slider.
function rateIndex(m, multiple) {
    return m.rates.findIndex((r) => r.multiple === multiple);
}
