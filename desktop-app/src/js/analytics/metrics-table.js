/**
 * metrics-table.js — S|B|O metrics table for the analyzer window.
 *
 * Sections that never mix (ANALYZER-2.md §1):
 *  - SELECTION (while a stretch is selected on a graph): the whole-track metrics
 *    measured over that stretch of S, B and O (selection.js, `an:selstats`).
 *  - LIVE: the sound at the playhead now — S and B looked up in the whole-track
 *    passes' 100 ms series at the player's position, O measured at the output
 *    (AAN1). Updated with every live frame. LIVE window only.
 *  - WHOLE TRACK: background passes over the whole file (S, B) and over the
 *    whole track rendered through the chain heard (O, opass.rs). A value
 *    appears only when it is final (··· until then); the section's line
 *    shows the progress.
 * Provenance badges, B-column chevron with glow, short-track LRA [!] badge,
 * click-to-copy values and tooltip data-tip attributes as before.
 *
 * Public API:
 *   createMetricsTable(container, ctx)
 *     container  HTMLElement
 *     ctx        { store, bus }
 *   Returns:  { setData(frame), destroy() }
 *
 * Listens on bus for:
 *   an:live     — the LIVE section (O from the frame, S/B from the series)
 *   an:track    — WHOLE TRACK S, B, O (+ the series for LIVE) from AAN2
 *   an:selstats — the SELECTION section
 */

import { METRIC_ID, PROV, N_METRICS, formatMetric, STREAM_NOTE, STREAM_TEXTS } from './protocol.js';
import {
  METRIC_LABEL, METRIC_TOOLTIP, METRIC_TAPS,
  formatTime, formatTimeInt,
} from './format.js';
import {
  provBadgeHTML, applyHpRule, isShortTrackLRA,
} from './provenance.js';

// ── Constants ──────────────────────────────────────────────────────────────────

// Rows to show by default (always visible)
const DEFAULT_VISIBLE = new Set([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 14, 15, 16, 17, 20]);

// Metric display groups: [label, [metric IDs in display order]]
const METRIC_GROUPS = [
  ['Loudness',  [0, 1, 2, 3]],
  ['Peaks',     [4, 5, 6, 7]],
  ['Dynamics',  [8, 9, 10, 11, 22]],
  ['Clipping',  [12, 13, 14, 15]],
  ['Spectrum',  [16, 17, 18, 19, 20, 21, 23, 24]],
];

// In the whole-track section ids 1 and 2 are the track's maxima.
const WHOLE_LABEL = { 1: 'LUFS-S max', 2: 'LUFS-M max' };

// The LIVE section: [key, label, the metric id it formats like, tooltip].
const LIVE_ROWS = [
  ['m',   'LUFS-M',    2,  'Momentary loudness (400 ms) at the playhead. S: the file, B: the variant heard, O: the output.'],
  ['s',   'LUFS-S',    1,  'Short-term loudness (3 s) at the playhead.'],
  ['tp',  'TP (3 s)',  4,  'True peak (BS.1770) of the last 3 s.'],
  ['rms', STREAM_TEXTS.rms, 9, STREAM_TEXTS.rmsTip],
  ['plr', 'PLR (3 s)', 22, 'Peak to loudness now: TP (3 s) minus LUFS-S.'],
];
// The live rows a stream has only (S and O measured live there).
const STREAM_LIVE_ONLY = new Set(['rms']);

// A stream's song and session: [key, label, the metric id it formats like].
const TOTAL_ROWS = [
  ['lufsI', METRIC_LABEL[0], 0],
  ['lra',   METRIC_LABEL[3], 3],
  ['tp',    STREAM_TEXTS.tpMax, 4],
  ['dr',    METRIC_LABEL[8], 8],
];

// The SELECTION section: the metrics a stretch has (the O pass's meters over it),
// in display order; the rest under "Show all".
const SEL_GROUPS = [
  ['Loudness', [0, 1, 2, 3]],
  ['Peaks',    [4, 5, 6, 7]],
  ['Dynamics', [8, 9, 22]],
  ['Clipping', [14, 15]],
  ['Spectrum', [16, 17, 18, 20, 23, 24]],
];
const SEL_VISIBLE = new Set([0, 1, 2, 3, 4, 6, 7, 8, 9, 22, 14, 15, 16, 20]);
// Band levels a signal kept in f32 cannot show below its rounding (dBFS).
const F32_FLOOR_IDS = new Set([17, 18, 20]);
const F32_FLOOR_DB = -180;
const SEL_LABEL = { 1: 'LUFS-S max', 2: 'LUFS-M max', 14: 'Samples |x|>1', 15: 'TP>0dBTP events' };
const SEL_TIP = {
  14: 'Samples above full scale in the stretch.',
  15: 'Moments the true peak went above 0 dBTP in the stretch.',
};

const IS_LIVE_WINDOW = new URLSearchParams(location.search).get('mode') !== 'file';

/** The value of `series` (100 ms hop) at index i, NaN outside. */
function _at(series, i) {
  return series && i >= 0 && i < series.length ? series[i] : NaN;
}

/** S or B live values at `posS` from a whole-track pass's 100 ms series. */
function _liveFromSeries(mSer, sSer, tpSer, posS) {
  const i = Math.floor(posS * 10);
  // The loudness series' index k is the window ending at (k + 4) × 100 ms.
  const m = _at(mSer, i - 4);
  const s = _at(sSer, i - 4);
  let tp = NaN;
  if (tpSer) {
    for (let k = Math.max(0, i - 29); k <= Math.min(tpSer.length - 1, i); k++) {
      if (!(tp >= tpSer[k])) tp = tpSer[k];
    }
  }
  return { m, s, tp };
}

// All metric IDs in display order (flattened from METRIC_GROUPS)
const ALL_METRIC_IDS = METRIC_GROUPS.flatMap(([, ids]) => ids);

// ── HTML template ─────────────────────────────────────────────────────────────

// What the three columns are (their headers' tooltips). "O" alone read as
// nothing much, and an O column waiting for its pass looked simply empty.
const TH_TIP = {
  s: 'S, the source: the file as it is.',
  b: 'B, the source after its stages (DC, ISP, SUB, AHR), before the filter. The arrow shows or hides this column.',
  o: 'O, the output: the whole track through the chain you hear, rendered and measured in the background while this window is open (its progress is on the Whole track line; ··· until then). BIT-PERFECT: none, the file itself plays. A converted file: the file itself.',
};

function _buildTable(container) {
  container.innerHTML = `
    <div class="metrics-wrap">
      <table class="metrics-tbl" role="grid">
        <thead>
          <tr class="metrics-head-row">
            <th class="metrics-th metrics-th--label">Metric</th>
            <th class="metrics-th metrics-th--s" data-tip="${TH_TIP.s}">S <span class="metrics-th-name">source</span></th>
            <th class="metrics-th metrics-th--b metrics-b-col" aria-label="B column (expand)" data-tip="${TH_TIP.b}">
              <span class="metrics-b-label">B <span class="metrics-th-name">stages</span></span>
              <button class="metrics-b-toggle" aria-label="Toggle B column" aria-expanded="false">
                <svg width="10" height="8" viewBox="0 0 10 8" fill="none"
                  stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"
                  aria-hidden="true" class="metrics-b-chevron">
                  <polyline points="1,2 5,6 9,2"/>
                </svg>
              </button>
            </th>
            <th class="metrics-th metrics-th--o" data-tip="${TH_TIP.o}">O <span class="metrics-th-name">output</span></th>
          </tr>
        </thead>
        <tbody class="metrics-tbody">
        </tbody>
      </table>
      <div class="metrics-show-all">
        <button class="metrics-show-btn" aria-expanded="false">
          Show all <svg width="8" height="6" viewBox="0 0 8 6" fill="none"
            stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"
            aria-hidden="true" class="metrics-show-chevron"><polyline points="1,1 4,5 7,1"/></svg>
        </button>
      </div>
    </div>
  `;
}

// ── Factory ───────────────────────────────────────────────────────────────────

/**
 * @param {HTMLElement} container
 * @param {{ store: object, bus: EventTarget }} ctx
 */
export function createMetricsTable(container, ctx) {
  const { store, bus } = ctx;
  _buildTable(container);

  const tbody      = container.querySelector('.metrics-tbody');
  const bTh        = container.querySelector('.metrics-th--b');
  const bToggle    = container.querySelector('.metrics-b-toggle');
  const bChevron   = container.querySelector('.metrics-b-chevron');
  const showAllBtn  = container.querySelector('.metrics-show-btn');
  const showChevron = container.querySelector('.metrics-show-chevron');

  let bVisible   = false;
  let showAll    = false;

  // State snapshots used for rendering
  let _sMetrics  = new Float64Array(32).fill(NaN);
  let _bMetrics  = new Float64Array(32).fill(NaN);
  let _oMetrics  = new Float64Array(32).fill(NaN);
  let _sProv     = new Uint8Array(26);
  let _bProv     = new Uint8Array(26);
  let _oProv     = new Uint8Array(26);
  let _hpActive  = false;
  let _bReady    = false;
  let _bGlowed   = false;   // the B chevron glowed for this track's B already
  let _durationS = 0;
  // LIVE: the series (from AAN2) and the O values (from AAN1).
  let _series    = {};
  let _liveO     = { m: NaN, s: NaN, tp: NaN, rms: NaN };
  // A stream: S live from the frame's STRM tail; BIT-PERFECT (O is S).
  let _liveS     = { m: NaN, s: NaN, tp: NaN, rms: NaN };
  let _bp        = false;
  // WHOLE TRACK O progress (COVERAGE_O while COMPUTING) and its ETA base.
  let _oEta      = null;

  // ── LIVE section rows ─────────────────────────────────────────────────────
  const liveRows = new Map(); // key → { tdS, tdB, tdO }
  const bCells = [];          // every B cell, for the B toggle
  const _sectionRow = (label, first) => {
    const tr = document.createElement('tr');
    tr.className = 'metrics-section-row' + (first ? ' metrics-section-row--first' : '');
    const td = document.createElement('td');
    td.className = 'metrics-td metrics-td--label';
    td.textContent = label;
    const tdS = _makeValueCell('s', -1), tdB = _makeValueCell('b', -1), tdO = _makeValueCell('o', -1);
    tdB.classList.add('metrics-b-col');
    bCells.push(tdB);
    tr.append(td, tdS, tdB, tdO);
    tbody.appendChild(tr);
    return { tr, td, tdS, tdB, tdO };
  };

  // ── SELECTION section rows (hidden while nothing is selected) ─────────────
  const selSec = _sectionRow('Selection', true);
  selSec.tr.classList.add('metrics-sel');
  selSec.tr.setAttribute('data-tip', 'The stretch selected on a graph (Shift+drag): the same metrics, measured over it only.');
  const selRangeTr = document.createElement('tr');
  selRangeTr.className = 'metrics-sel metrics-sel-range';
  const selRangeTd = document.createElement('td');
  selRangeTd.colSpan = 4;
  selRangeTd.className = 'metrics-td';
  const selRangeText = document.createElement('span');
  const selClear = document.createElement('button');
  selClear.type = 'button';
  selClear.className = 'metrics-sel-clear';
  selClear.textContent = '✕';
  selClear.setAttribute('data-tip', 'Clear the selection (Esc)');
  selClear.addEventListener('click', () => bus.dispatchEvent(new CustomEvent('an:selection', { detail: null })));
  selRangeTd.append(selRangeText, selClear);
  selRangeTr.appendChild(selRangeTd);
  tbody.appendChild(selRangeTr);
  const selRows = new Map(); // id → { tr, tdS, tdB, tdO }
  for (const [groupLabel, ids] of SEL_GROUPS) {
    const g = document.createElement('tr');
    g.className = 'metrics-group-row metrics-sel';
    const gtd = document.createElement('td');
    gtd.colSpan = 4;
    gtd.textContent = groupLabel;
    g.appendChild(gtd);
    tbody.appendChild(g);
    for (const id of ids) {
      const tr = document.createElement('tr');
      tr.className = 'metrics-row metrics-sel';
      if (!SEL_VISIBLE.has(id)) tr.classList.add('metrics-sel--extra', 'metrics-row--hidden');
      const tdLabel = document.createElement('td');
      tdLabel.className = 'metrics-td metrics-td--label';
      tdLabel.textContent = SEL_LABEL[id] || METRIC_LABEL[id] || `Metric ${id}`;
      const tip = SEL_TIP[id] || METRIC_TOOLTIP[id];
      if (tip) { tdLabel.setAttribute('data-tip', tip); tdLabel.classList.add('metrics-td--has-tip'); }
      const tdS = _makeValueCell('s', id), tdB = _makeValueCell('b', id), tdO = _makeValueCell('o', id);
      tdB.classList.add('metrics-b-col');
      bCells.push(tdB);
      tr.append(tdLabel, tdS, tdB, tdO);
      tbody.appendChild(tr);
      selRows.set(id, { tr, tdS, tdB, tdO });
    }
  }
  let _sel = null;                 // the stretch shown
  const _selStats = {};            // src → AASL frame
  let _nextSecTr = null;            // the section under it (flush to the header while it is hidden)
  const _showSel = (on) => {
    for (const el of tbody.querySelectorAll('.metrics-sel')) {
      el.classList.toggle('metrics-sel--off', !on);
    }
    _nextSecTr?.classList.toggle('metrics-section-row--first', !on);
  };
  if (IS_LIVE_WINDOW) {
    const sec = _sectionRow('Live', false);
    _nextSecTr = sec.tr;
    sec.tr.setAttribute('data-tip', 'The sound at the playhead now: what the features do to it at this moment.');
    for (const [key, label, , tip] of LIVE_ROWS) {
      const tr = document.createElement('tr');
      tr.className = 'metrics-row metrics-row--live';
      if (STREAM_LIVE_ONLY.has(key)) tr.classList.add('metrics-stream-only');
      const tdLabel = document.createElement('td');
      tdLabel.className = 'metrics-td metrics-td--label metrics-td--has-tip';
      tdLabel.textContent = label;
      tdLabel.setAttribute('data-tip', tip);
      const tdS = _makeValueCell('s', -1), tdB = _makeValueCell('b', -1), tdO = _makeValueCell('o', -1);
      tdB.classList.add('metrics-b-col');
      bCells.push(tdB);
      tr.append(tdLabel, tdS, tdB, tdO);
      tbody.appendChild(tr);
      liveRows.set(key, { tdS, tdB, tdO, tdLabel, tip });
    }
  }
  // A stream: the song playing and the session (stream_totals.rs), S and O.
  const totals = {};
  if (IS_LIVE_WINDOW) {
    for (const [which, label, tip] of [['song', STREAM_TEXTS.song, STREAM_TEXTS.songTip],
                                       ['session', STREAM_TEXTS.session, STREAM_TEXTS.sessionTip]]) {
      const sec = _sectionRow(label, false);
      sec.tr.classList.add('metrics-stream-only');
      sec.tr.setAttribute('data-tip', tip);
      if (which === 'session') {
        const reset = document.createElement('button');
        reset.type = 'button';
        reset.className = 'metrics-sel-clear metrics-stream-reset';
        reset.textContent = STREAM_TEXTS.reset;
        reset.setAttribute('data-tip', STREAM_TEXTS.resetTip);
        reset.addEventListener('click', () => bus.dispatchEvent(new CustomEvent('an:stream:reset')));
        sec.td.appendChild(reset);
      }
      const rowsOf = new Map();
      for (const [key, rowLabel] of TOTAL_ROWS) {
        if (which === 'session' && key === 'dr') continue;   // DR is a song's, not a session's
        const tr = document.createElement('tr');
        tr.className = 'metrics-row metrics-row--stream metrics-stream-only';
        const tdLabel = document.createElement('td');
        tdLabel.className = 'metrics-td metrics-td--label';
        tdLabel.textContent = rowLabel;
        const tdS = _makeValueCell('s', -1), tdB = _makeValueCell('b', -1), tdO = _makeValueCell('o', -1);
        tdB.classList.add('metrics-b-col');
        bCells.push(tdB);
        tr.append(tdLabel, tdS, tdB, tdO);
        tbody.appendChild(tr);
        rowsOf.set(key, { tdS, tdO });
      }
      totals[which] = { sec, rows: rowsOf, tip };
    }
  }
  const wholeSec = _sectionRow('Whole track', false);
  if (!_nextSecTr) _nextSecTr = wholeSec.tr;
  _showSel(false);
  wholeSec.tr.setAttribute('data-tip',
    'The whole file (S, B) and the whole track through the chain heard (O), analysed in the background. A value appears when it is final.');
  // A live stream: the section's dashes say why.
  const wholeTip = wholeSec.tr.getAttribute('data-tip');
  // A live stream: S and O live, the song and the session — no whole track
  // (one line says why), no B.
  const wrapEl = container.querySelector('.metrics-wrap');
  const thS = container.querySelector('.metrics-th--s');
  const thO = container.querySelector('.metrics-th--o');
  ctx?.store?.on?.('_stream', on => {
    wholeSec.tr.setAttribute('data-tip', on ? STREAM_NOTE : wholeTip);
    wrapEl?.classList.toggle('metrics--stream', !!on);
    thS?.setAttribute('data-tip', on ? STREAM_TEXTS.thS : TH_TIP.s);
    thO?.setAttribute('data-tip', on ? STREAM_TEXTS.liveTail : TH_TIP.o);
    const m = liveRows.get('m');
    m?.tdLabel.setAttribute('data-tip',
      on ? m.tip.replace('S: the file, B: the variant heard, O: the output.', STREAM_TEXTS.liveTail) : m.tip);
    _renderLive();
  });
  ctx?.store?.on?.('_streamTotals', v => _renderTotals(v));

  // Build rows grouped by METRIC_GROUPS
  const rows = new Map(); // metricId → { tr, tdLabel, tdS, tdB, tdO, coverageBar }

  for (let gi = 0; gi < METRIC_GROUPS.length; gi++) {
    const [groupLabel, groupIds] = METRIC_GROUPS[gi];

    // Group header row
    const groupTr = document.createElement('tr');
    groupTr.className = 'metrics-group-row';
    const groupTd = document.createElement('td');
    groupTd.colSpan = 4;
    groupTd.textContent = groupLabel;
    groupTr.appendChild(groupTd);
    tbody.appendChild(groupTr);

    for (const id of groupIds) {
      const tr = document.createElement('tr');
      tr.className = 'metrics-row';
      tr.dataset.metricId = id;

      const visible = DEFAULT_VISIBLE.has(id);
      if (!visible) tr.classList.add('metrics-row--hidden');

      // Label cell
      const tdLabel = document.createElement('td');
      tdLabel.className = 'metrics-td metrics-td--label';
      tdLabel.textContent = WHOLE_LABEL[id] || METRIC_LABEL[id] || `Metric ${id}`;
      if (METRIC_TOOLTIP[id]) {
        tdLabel.setAttribute('data-tip', METRIC_TOOLTIP[id]);
        tdLabel.classList.add('metrics-td--has-tip');
      }
      // Short-track LRA [!] placeholder (shown/hidden dynamically)
      if (id === METRIC_ID.LRA) {
        const warn = document.createElement('span');
        warn.className = 'metrics-lra-warn metrics-hidden';
        warn.setAttribute('data-tip',
          'Fewer than 60 s of gated content — LRA may be unreliable for short or very quiet tracks.');
        warn.textContent = '[!]';
        tdLabel.appendChild(warn);
      }

      // Value cells
      const tdS = _makeValueCell('s', id);
      const tdB = _makeValueCell('b', id);
      const tdO = _makeValueCell('o', id);

      // B col: hidden by default
      tdB.classList.add('metrics-b-col');
      bCells.push(tdB);

      tr.append(tdLabel, tdS, tdB, tdO);
      tbody.appendChild(tr);
      rows.set(id, { tr, tdLabel, tdS, tdB, tdO });
    }
  }
  // B hidden: its cells stay in their rows, empty and narrow under the
  // header's arrow (analytics.css), so S and O stay under their own headers.
  if (!bVisible) for (const td of bCells) td.classList.add('metrics-b-off');

  // ── B toggle ─────────────────────────────────────────────────────────────

  bToggle.addEventListener('click', () => {
    bVisible = !bVisible;
    bToggle.setAttribute('aria-expanded', bVisible ? 'true' : 'false');
    bChevron.classList.toggle('metrics-b-chevron--open', bVisible);
    // Hide/show B value cells
    for (const tdB of bCells) {
      if (bVisible) tdB.classList.remove('metrics-b-off');
      else          tdB.classList.add('metrics-b-off');
    }
    // Collapse B column header label when B is hidden (only chevron remains)
    if (bTh) {
      if (bVisible) bTh.classList.remove('b-collapsed');
      else          bTh.classList.add('b-collapsed');
    }
  });

  // Apply initial collapsed state to B header
  if (bTh && !bVisible) bTh.classList.add('b-collapsed');

  // ── Show-all toggle ───────────────────────────────────────────────────────

  showAllBtn.addEventListener('click', () => {
    showAll = !showAll;
    showAllBtn.setAttribute('aria-expanded', showAll ? 'true' : 'false');
    showChevron.classList.toggle('metrics-show-chevron--open', showAll);
    for (const [id, { tr }] of rows) {
      if (!DEFAULT_VISIBLE.has(id)) {
        if (showAll) tr.classList.remove('metrics-row--hidden');
        else         tr.classList.add('metrics-row--hidden');
      }
    }
    for (const tr of tbody.querySelectorAll('.metrics-sel--extra')) tr.classList.toggle('metrics-row--hidden', !showAll);
  });

  // ── Rendering ─────────────────────────────────────────────────────────────

  /** A whole-track cell: the value only when final (··· while computing:
   *  a blank cell read as "O is never filled"). */
  function _wholeStr(id, v, p) {
    if (p === PROV.COMPUTING) return '···';
    if (id === METRIC_ID.PEAK_AT) return isNaN(v) ? '—' : formatTime(v);
    return formatMetric(id, v, p);
  }

  function _renderLive() {
    if (!IS_LIVE_WINDOW) return;
    // A stream's S is measured live (its STRM tail); a file's is looked up
    // in the whole-track series at the playhead.
    const stream = !!store.get('_stream');
    const posS = store.get('_posS') ?? 0;
    const sv = stream ? _liveS : _liveFromSeries(_series.mS, _series.sS, _series.tpS, posS);
    const bv = stream ? {} : _liveFromSeries(_series.mB, _series.sB, _series.tpB, posS);
    const bp = stream && _bp;
    for (const [key, , fmtId] of LIVE_ROWS) {
      const r = liveRows.get(key);
      const val = (x) => key === 'plr' ? x.tp - x.s : x[key];
      const str = (v) => Number.isFinite(v) ? formatMetric(fmtId, v, PROV.MEASURED) : '—';
      _setCell(r.tdS, str(val(sv)), '');
      _setCell(r.tdB, str(val(bv)), '');
      // BIT-PERFECT: the output is the source — one set of numbers.
      _setCell(r.tdO, bp ? STREAM_TEXTS.bp : str(val(_liveO)), '');
      if (bp) r.tdO.setAttribute('data-tip', STREAM_TEXTS.bpTip);
      else r.tdO.removeAttribute('data-tip');
    }
  }

  /** A stream's song and session (the /stream JSON, stream_totals.rs). */
  function _renderTotals(v) {
    for (const which of ['song', 'session']) {
      const t = totals[which];
      if (!t) continue;
      const d = v?.[which] || null;
      const untitled = which === 'song' && v?.titles === false;
      t.sec.tr.setAttribute('data-tip', untitled ? STREAM_TEXTS.noTitles : t.tip);
      _setCell(t.sec.tdS, Number.isFinite(d?.playedS) ? formatTimeInt(d.playedS) : '', '');
      const str = (fmtId, x) => (x != null && Number.isFinite(x)) ? formatMetric(fmtId, x, PROV.MEASURED) : '—';
      for (const [key, , fmtId] of TOTAL_ROWS) {
        const r = t.rows.get(key);
        if (!r) continue;
        _setCell(r.tdS, str(fmtId, d?.s?.[key]), '');
        _setCell(r.tdO, str(fmtId, d?.o?.[key]), '');
      }
    }
  }

  /** The section header's per-column status: S/B analysing, O progress + ETA. */
  function _renderWholeStatus() {
    const busy = (prov) => Array.from(prov || []).some(p => p === PROV.COMPUTING);
    _setCell(wholeSec.tdS, busy(_sProv) ? '···' : '', '');
    _setCell(wholeSec.tdB, busy(_bProv) ? '···' : '', '');
    const oP = _oProv[METRIC_ID.COVERAGE_O];
    const pct = _oMetrics[METRIC_ID.COVERAGE_O];
    let oStr = '';
    if (oP === PROV.COMPUTING && Number.isFinite(pct)) {
      const now = performance.now();
      if (!_oEta || pct < _oEta.p) _oEta = { t: now, p: pct };
      let eta = '';
      if (pct - _oEta.p >= 2) {
        const s = (100 - pct) * (now - _oEta.t) / 1000 / (pct - _oEta.p);
        eta = s >= 60 ? ` · ~${Math.round(s / 60)} min` : ` · ~${Math.max(1, Math.round(s))} s`;
      }
      oStr = `${Math.floor(pct)}%${eta}`;
      wholeSec.tdO.setAttribute('data-tip', 'O: the whole track rendered through the chain heard, in the background (paused while playback needs the CPU).');
    } else {
      _oEta = null;
    }
    _setCell(wholeSec.tdO, oStr, '');
  }

  function _render() {
    const shortTrack = isShortTrackLRA(_durationS);
    _renderLive();
    _renderWholeStatus();

    for (const id of ALL_METRIC_IDS) {
      const r = rows.get(id);
      if (!r) continue;

      const tapS = !!(METRIC_TAPS[id] & 0b001);
      const tapB = !!(METRIC_TAPS[id] & 0b010);
      const tapO = !!(METRIC_TAPS[id] & 0b100);

      const sVal = tapS ? _sMetrics[id] : NaN;
      const bVal = tapB ? _bMetrics[id] : NaN;
      const oVal = tapO ? _oMetrics[id] : NaN;
      const sP   = tapS ? _sProv[id] : PROV.UNAVAIL;
      const bP   = tapB ? _bProv[id] : PROV.UNAVAIL;
      const oP   = tapO ? _oProv[id] : PROV.UNAVAIL;

      const badge = (p, tap) => p === PROV.COMPUTING ? '' : provBadgeHTML(p, 100, _hpActive, tap);
      _setCell(r.tdS, _wholeStr(id, sVal, sP), badge(sP, 's'));
      _setCell(r.tdB, _wholeStr(id, bVal, bP), badge(bP, 'b'));
      _setCell(r.tdO, _wholeStr(id, oVal, oP), badge(oP, 'o'));

      // Short-track LRA [!]
      if (id === METRIC_ID.LRA) {
        const warn = r.tdLabel.querySelector('.metrics-lra-warn');
        if (warn) {
          if (shortTrack) warn.classList.remove('metrics-hidden');
          else            warn.classList.add('metrics-hidden');
        }
      }

      // B column: show spinner when B not ready
      if (!_bReady && !isNaN(bVal)) {
        // bVal already NaN while computing — handled by formatMetric
      }
    }

    // B chevron glow when B just became ready
    if (_bReady) {
      bToggle.classList.add('metrics-b-ready');
    } else {
      bToggle.classList.remove('metrics-b-ready');
    }
  }

  function _setCell(td, valueStr, badgeHTML) {
    // Find or create the value span and badge span
    let vs = td.querySelector('.metrics-val');
    let bs = td.querySelector('.metrics-prov');
    if (!vs) {
      vs = document.createElement('span');
      vs.className = 'metrics-val';
      td.prepend(vs);
    }
    if (!bs) {
      bs = document.createElement('span');
      bs.className = 'metrics-prov';
      td.appendChild(bs);
    }
    vs.textContent = valueStr;
    bs.innerHTML = badgeHTML;
  }

  /** The SELECTION section: its range and each signal's values (or its status). */
  function _renderSel() {
    if (!_sel) { _showSel(false); return; }
    _showSel(true);
    const len = _sel.t1 - _sel.t0;
    selRangeText.textContent = `${formatTime(_sel.t0, 2)} – ${formatTime(_sel.t1, 2)} · ${len.toFixed(len < 10 ? 2 : 1)} s`;
    for (const [src, td] of [['S', selSec.tdS], ['B', selSec.tdB], ['O', selSec.tdO]]) {
      const f = _selStats[src];
      _setCell(td, !f ? '···' : f.status === 2 ? '—' : '', '');
      td.setAttribute('data-tip', !f ? 'Measuring the stretch…'
        : f.status === 2 ? (src === 'S' ? 'The file could not be read again.' : `${src} is not kept for this track: it comes back with its next pass (a rack change, or playing the track again with the window open).`)
        : '');
    }
    for (const [id, r] of selRows) {
      for (const [src, td] of [['S', r.tdS], ['B', r.tdB], ['O', r.tdO]]) {
        const f = _selStats[src];
        if (!f || f.status !== 1) { _setCell(td, '', ''); continue; }
        const v = f.metrics[id], p = f.prov[id];
        let str = id === METRIC_ID.PEAK_AT ? (Number.isFinite(v) ? formatTime(v) : '—') : formatMetric(id, v, p);
        const rounded = f.f32_kept && F32_FLOOR_IDS.has(id) && Number.isFinite(v) && v < F32_FLOOR_DB;
        if (rounded) str = `< ${F32_FLOOR_DB} dBFS`;
        td.setAttribute('data-tip', rounded ? 'Below the precision O is kept at for a selection (32-bit float); the whole-track value is exact.' : '');
        _setCell(td, str, provBadgeHTML(p, 100, false, src.toLowerCase()));
      }
    }
  }

  // ── Click-to-copy ─────────────────────────────────────────────────────────

  tbody.addEventListener('click', (ev) => {
    const td = ev.target.closest('td.metrics-td--val');
    if (!td) return;
    const vs = td.querySelector('.metrics-val');
    if (!vs || !vs.textContent || vs.textContent === '—' || vs.textContent === '···') return;
    try {
      navigator.clipboard.writeText(vs.textContent.trim());
    } catch (_) { /* clipboard unavailable */ }
    td.classList.add('metrics-td--copied');
    setTimeout(() => td.classList.remove('metrics-td--copied'), 600);
  });

  // ── Bus listeners ─────────────────────────────────────────────────────────

  function _onLive(ev) {
    const { frame } = ev.detail;
    // The live frame: O at the output now (momentary, short-term, TP 3 s).
    const t = frame.strm;
    _liveO = {
      m: frame.live_metrics[METRIC_ID.LUFS_M_LIVE],
      s: frame.live_metrics[METRIC_ID.LUFS_S_LIVE],
      tp: frame.live_metrics[METRIC_ID.TP_EBUR],
      rms: t ? t.oRms : NaN,
    };
    // A stream: S measured live beside O (the frame's STRM tail).
    _liveS = t ? { m: t.sLufsM, s: t.sLufsS, tp: t.sTp, rms: t.sRms } : { m: NaN, s: NaN, tp: NaN, rms: NaN };
    _bp = !!t?.bp;
    _hpActive = frame.hp_active;
    _renderLive();
  }

  function _onTrack(ev) {
    const { frame } = ev.detail;
    if (frame.stub) return;
    _sMetrics  = frame.s_metrics;
    _bMetrics  = frame.b_metrics;
    _oMetrics  = new Float64Array(32);
    for (let i = 0; i < 32; i++) _oMetrics[i] = frame.o_metrics[i];
    _sProv     = frame.s_prov;
    _bProv     = frame.b_prov;
    _oProv     = frame.o_prov;
    _durationS = frame.duration_s || _durationS;
    _series = {
      mS: frame.lufs_m_series_s, sS: frame.lufs_s_series_s, tpS: frame.tp100_s,
      mB: frame.lufs_m_series_b, sB: frame.lufs_s_series_b, tpB: frame.tp100_b,
    };
    _bReady    = true;
    // Glow the B chevron briefly, once, when B has its first numbers. Every
    // track frame used to glow it again (they come several a second while
    // the O pass runs), and the overlapping 2 s timers made it blink.
    const bHas = Array.from(frame.b_prov || []).some(p => p !== PROV.COMPUTING && p !== PROV.UNAVAIL);
    if (bHas && !_bGlowed) {
      _bGlowed = true;
      bToggle.classList.add('metrics-b-glow');
      setTimeout(() => bToggle.classList.remove('metrics-b-glow'), 2000);
    }
    _render();
  }

  function _onChainChange(ev) {
    // The O pass restarts for the new chain; its track frame blanks O.
    _render();
  }

  function _onTrackChange() {
    // Clear everything, reset to computing state
    _sMetrics = new Float64Array(32).fill(NaN);
    _bMetrics = new Float64Array(32).fill(NaN);
    _oMetrics = new Float64Array(32).fill(NaN);
    _sProv    = new Uint8Array(26).fill(PROV.COMPUTING);
    _bProv    = new Uint8Array(26).fill(PROV.COMPUTING);
    _oProv    = new Uint8Array(26).fill(PROV.COMPUTING);
    _series   = {};
    _liveO    = { m: NaN, s: NaN, tp: NaN };
    _oEta     = null;
    _bReady   = false;
    _bGlowed  = false;
    _render();
  }

  function _onSelStats(ev) {
    const { sel, src, stats } = ev.detail || {};
    if (!sel) { _sel = null; for (const k of Object.keys(_selStats)) delete _selStats[k]; _renderSel(); return; }
    if (!_sel || sel.id !== _sel.id) { _sel = sel; for (const k of Object.keys(_selStats)) delete _selStats[k]; }
    if (src) _selStats[src] = stats;
    _renderSel();
  }

  bus.addEventListener('an:selstats',     _onSelStats);
  bus.addEventListener('an:live',         _onLive);
  bus.addEventListener('an:track',        _onTrack);
  bus.addEventListener('an:chain:change', _onChainChange);
  bus.addEventListener('an:track:change', _onTrackChange);

  // Initial render
  _render();

  return {
    setData(payload) {
      // Can be called directly by window.js as well
    },
    destroy() {
      bus.removeEventListener('an:selstats',     _onSelStats);
      bus.removeEventListener('an:live',         _onLive);
      bus.removeEventListener('an:track',        _onTrack);
      bus.removeEventListener('an:chain:change', _onChainChange);
      bus.removeEventListener('an:track:change', _onTrackChange);
    },
  };
}

// ── Internal helpers ──────────────────────────────────────────────────────────

function _makeValueCell(tap, metricId) {
  const td = document.createElement('td');
  td.className = `metrics-td metrics-td--val metrics-td--${tap}`;
  td.dataset.tap = tap;
  td.dataset.metricId = metricId;
  // Will be populated by _setCell
  return td;
}
