/**
 * layers.js — Layer strip component for canvas panels.
 *
 * Renders a 28 px vertical strip on the left edge of each canvas panel:
 * one row per layer with a circular color dot and eye-icon toggle.
 *
 * Keyboard shortcuts: pressing the layer's `shortcut` key (single char) toggles it.
 * Shift+click two dots selects the Δ pair for that canvas.
 *
 * Public API:
 *   createLayerStrip(container, registry, bus, viewId)
 *     container  HTMLElement — the panel container, strip is prepended
 *     registry   LAYER_REGISTRY array (see FRONTEND-CONTRACT.md §5)
 *     bus        EventTarget for dispatching an:layer:toggle events
 *     viewId     string identifying the parent view
 *
 *   Returns: { setOn(id, on), setState(id, state), destroy() }
 */

// LAYER_REGISTRY entry shape:
// { id, label, colorToken, defaultOn, shortcut, provenanceAware, axisRole }

/**
 * @param {HTMLElement}   container
 * @param {object[]}      registry
 * @param {EventTarget}   bus
 * @param {string}        viewId
 */
// What each layer toggle shows and says. The toggle carries the short name
// (a pill in the layer colour, dim when the layer is off); the tooltip says
// what the layer is.
const LAYER_INFO = {
  // spectrum
  s_psd:      ['S',     'S — the source file as it is: its whole-track spectrum.'],
  b_psd:      ['B',     'B — the source after its stages (DC, ISP, SUB, AHR), before the filter.'],
  o_psd:      ['O',     'O — the output, what goes to the device.'],
  h_mag:      ['|H|',   '|H| — the chain’s frequency response, in dB (right axis).'],
  delta_bs:   ['B−S',   'B − S: what the source stages changed.'],
  delta_ob:   ['O−B',   'O − B: what the filter changed.'],
  max_hold:   ['Max',   'The highest output level seen at each frequency.'],
  sel:        ['Sel',   'The selected stretch (Shift+drag on the spectrogram): its spectrum, dotted, for each signal shown.'],
  ultra:      ['>24k',  'Shades the band above 24 kHz.'],
  infra:      ['<20',   'Shades the band below 20 Hz.'],
  // loudness history
  lufs_s_o:   ['O 3s',  'Output, short-term loudness (3 s window).'],
  lufs_s_s:   ['S 3s',  'Source, short-term loudness (3 s window).'],
  lufs_m_o:   ['O .4s', 'Output, momentary loudness (400 ms window).'],
  lufs_m_s:   ['S .4s', 'Source, momentary loudness (400 ms window).'],
  lufs_i_line:['I',     'Integrated loudness of the whole track.'],
  lra_band_s: ['LRA S', 'Loudness range of the source: the band between its quiet and loud parts.'],
  lra_band_o: ['LRA O', 'Loudness range of the output.'],
  gate_ticks: ['Gate',  'Moments below the loudness gate (left out of LUFS-I).'],
  // spectrogram
  s_spec:     ['S',     'Spectrogram of the source.'],
  b_spec:     ['B',     'Spectrogram of B, the source after its stages.'],
  o_spec:     ['O',     'Spectrogram of the output.'],
  ultra_tint: ['>24k',  'Shades the band above 24 kHz.'],
  infra_tint: ['<20',   'Shades the band below 20 Hz.'],
  // waveform
  s_mip:      ['S',     'Waveform of the source.'],
  s_rms:      ['S rms', 'RMS envelope of the source.'],
  o_mip:      ['O',     'Waveform of the output.'],
  clips:      ['Clip',  'Clipped runs in the source.'],
  overs:      ['Over',  'Output peaks above 0 dBTP.'],
  dr_blocks:  ['DR',    'The loudest 20 % of 3-second blocks (what DR is computed from).'],
  gate_spans: ['Gate',  'Stretches below the loudness gate.'],
  // histogram
  loud_s:     ['S',     'How often each loudness occurs in the source.'],
  loud_b:     ['B',     'How often each loudness occurs in B.'],
  loud_o:     ['O',     'How often each loudness occurs in the output.'],
  amp_hist:   ['Amp',   'How often each sample level occurs.'],
  // stereo
  vectorscope:['Vec',   'Vectorscope of the output: mid up, side across, the last 50 ms.'],
  corr_meter: ['Corr',  'Stereo correlation: +1 the same in both channels, 0 unrelated, −1 opposite; its last minute and the whole track.'],
};

export function createLayerStrip(container, registry, bus, viewId) {
  const state = new Map(); // id → { on: boolean, row: HTMLElement, dot: HTMLElement, eye: HTMLElement }

  // Build the strip element
  const strip = document.createElement('div');
  strip.className = 'layer-strip';
  strip.setAttribute('role', 'group');
  strip.setAttribute('aria-label', 'Layer toggles');

  let shiftFirst = null; // first shift-clicked layer id

  for (const entry of registry) {
    const on = entry.defaultOn !== false;

    const row = document.createElement('div');
    row.className  = 'layer-row';
    row.dataset.id = entry.id;

    const dot = document.createElement('button');
    dot.className = 'layer-dot';
    dot.setAttribute('aria-pressed', on ? 'true' : 'false');
    const [short, what] = LAYER_INFO[entry.id] || [entry.label, entry.label];
    dot.setAttribute('aria-label', entry.label + ' layer');
    const key = entry.shortcut ? ` Key ${entry.shortcut.toUpperCase()}.` : '';
    dot.setAttribute('data-tip', `${what} Click to show or hide.${key}`);
    dot.style.setProperty('--layer-color', `var(${entry.colorToken})`);
    if (!on) dot.classList.add('layer-dot--off');

    dot.textContent = short;

    if (entry.provenanceAware) {
      dot.classList.add('layer-dot--prov-aware');
    }

    dot.addEventListener('click', (ev) => {
      if (ev.shiftKey) {
        // Δ pair selection
        if (shiftFirst === null) {
          shiftFirst = entry.id;
          dot.classList.add('layer-dot--shift-sel');
        } else if (shiftFirst !== entry.id) {
          bus.dispatchEvent(new CustomEvent('an:layer:toggle', {
            detail: { layerId: shiftFirst, pairedWith: entry.id, viewId, delta: true },
          }));
          // clear selection
          const prev = strip.querySelector('.layer-dot--shift-sel');
          if (prev) prev.classList.remove('layer-dot--shift-sel');
          shiftFirst = null;
        }
        return;
      }
      // Normal toggle
      shiftFirst = null;
      const prev = strip.querySelector('.layer-dot--shift-sel');
      if (prev) prev.classList.remove('layer-dot--shift-sel');

      const cur = state.get(entry.id);
      const newOn = !cur.on;
      _applyOn(entry.id, newOn);
      bus.dispatchEvent(new CustomEvent('an:layer:toggle', {
        detail: { layerId: entry.id, on: newOn, viewId },
      }));
    });

    row.appendChild(dot);
    strip.appendChild(row);
    state.set(entry.id, { on, row, dot, entry });
  }

  // Keyboard shortcuts — registered on the document while the strip is alive.
  // Only the strip of the graph shown answers (every graph has a strip, and
  // several use the same keys: 1 and 3, c, p...), and the key is matched by
  // its place, so a Russian layout or Caps Lock does not change it.
  // A digit is its key in the row or on the keypad.
  const codesOf = (k) => (/^[0-9]$/.test(k) ? [`Digit${k}`, `Numpad${k}`] : [`Key${k.toUpperCase()}`]);
  function _onKeyDown(ev) {
    if (ev.target.tagName === 'INPUT' || ev.target.tagName === 'TEXTAREA' || ev.target.tagName === 'SELECT') return;
    if (ev.ctrlKey || ev.altKey || ev.metaKey) return;
    if (strip.offsetParent === null) return;   // another graph's tab is up
    for (const entry of registry) {
      if (entry.shortcut && codesOf(entry.shortcut).includes(ev.code)) {
        ev.preventDefault();
        const cur = state.get(entry.id);
        const newOn = !cur.on;
        _applyOn(entry.id, newOn);
        bus.dispatchEvent(new CustomEvent('an:layer:toggle', {
          detail: { layerId: entry.id, on: newOn, viewId, keyboard: true },
        }));
        return;
      }
    }
  }
  document.addEventListener('keydown', _onKeyDown);

  function _applyOn(id, on) {
    const s = state.get(id);
    if (!s) return;
    s.on = on;
    s.dot.setAttribute('aria-pressed', on ? 'true' : 'false');
    // The pill stays; layer-dot--off dims it.
    if (on) {
      s.dot.classList.remove('layer-dot--off');
    } else {
      s.dot.classList.add('layer-dot--off');
    }
  }

  // Insert strip before the canvas content in container
  // (expects container to be the panel wrapper with position:relative)
  container.prepend(strip);

  return {
    /** Programmatically set a layer on/off (does not fire bus event). */
    setOn(id, on) {
      _applyOn(id, on);
    },
    /**
     * Set additional visual state (e.g. HP_PENDING amber border on a prov-aware dot).
     * @param {string} id
     * @param {'normal'|'hp-pending'} s
     */
    setState(id, s) {
      const e = state.get(id);
      if (!e) return;
      if (e.entry.provenanceAware) {
        if (s === 'hp-pending') {
          e.dot.classList.add('layer-dot--hp');
        } else {
          e.dot.classList.remove('layer-dot--hp');
        }
      }
    },
    destroy() {
      document.removeEventListener('keydown', _onKeyDown);
      strip.remove();
    },
  };
}
