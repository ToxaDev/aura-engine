/**
 * provenance.js — Provenance badge rendering helpers.
 *
 * Renders [A]/[F]/[M]/[=]/[?]/··· badges on metric cells, enforces the
 * HP/αHP rule (SPEC §5.3: never [A] or [F] for O when HP active),
 * and wires tooltip data-tip attributes.
 *
 * Public API:
 *   provBadgeHTML(prov, coveragePct, hpActive, tap)  → HTML string for a badge span
 *   applyProvToCells(cells, prov, coveragePct, hpActive, tap)
 *   provTooltipText(prov, coveragePct, tap)  → string for data-tip
 */

import { PROV } from './protocol.js';

// ── Badge text/color per provenance code ──────────────────────────────────────

const _BADGE_LABEL = {
  [PROV.ANALYTIC]:   '[A]',
  [PROV.FORECAST]:   '[F]',
  [PROV.MEASURED]:   '[M]',
  [PROV.UNCHANGED]:  '[=]',
  [PROV.HP_PENDING]: '[?]',
  [PROV.STALE]:      '[stale]',
  [PROV.COMPUTING]:  '···',
  [PROV.UNAVAIL]:    '',
};

const _BADGE_CLASS = {
  [PROV.ANALYTIC]:   'prov-a',
  [PROV.FORECAST]:   'prov-f',
  [PROV.MEASURED]:   'prov-m',
  [PROV.UNCHANGED]:  'prov-eq',
  [PROV.HP_PENDING]: 'prov-hp',
  [PROV.STALE]:      'prov-stale',
  [PROV.COMPUTING]:  'prov-computing',
  [PROV.UNAVAIL]:    'prov-unavail',
};

// Tooltip text per provenance (used as data-tip on the badge element)
const _BADGE_TIP = {
  [PROV.ANALYTIC]:   'Analytic [A] — exact model result, no render needed',
  [PROV.FORECAST]:   'Forecast [F] — model-based estimate; will be replaced by measured value',
  [PROV.MEASURED]:   'Measured [M] — computed on real signal',
  [PROV.UNCHANGED]:  'Unchanged [=] — provably invariant under this chain',
  [PROV.HP_PENDING]: 'Hybrid-Phase is time-varying — only measured values are valid here',
  [PROV.STALE]:      'Measured before the rack changed; measuring again',
  [PROV.COMPUTING]:  'Computing…',
  [PROV.UNAVAIL]:    '',
};

/**
 * Enforce the HP/αHP rule from SPEC §5.3:
 * If hp_active is true and tap is 'o', [A] and [F] become HP_PENDING.
 * @param {number} prov
 * @param {boolean} hpActive
 * @param {string} tap  's' | 'b' | 'o'
 * @returns {number}
 */
export function applyHpRule(prov, hpActive, tap) {
  if (!hpActive || tap !== 'o') return prov;
  // SPEC §5.2: [?] replaces cells that would normally show [A] or [F].
  // LUFS-S/M live O is [M] (live-accumulated), so MEASURED must NOT become [?].
  if (prov === PROV.ANALYTIC || prov === PROV.FORECAST) return PROV.HP_PENDING;
  return prov;
}

/**
 * Return the HTML string for a provenance badge.
 * Includes data-tip and class; no outer element.
 *
 * @param {number}  prov          PROV constant
 * @param {number}  coveragePct   0-100 (used only when prov===MEASURED)
 * @param {boolean} hpActive      true when HP/αHP in chain
 * @param {string}  tap           's' | 'b' | 'o'
 * @returns {string}
 */
export function provBadgeHTML(prov, coveragePct, hpActive, tap) {
  const p = applyHpRule(prov, hpActive, tap);
  if (p === PROV.UNAVAIL) return '';

  const label   = _BADGE_LABEL[p] ?? '';
  const cls     = _BADGE_CLASS[p] ?? 'prov-unavail';
  const rawTip  = _BADGE_TIP[p] ?? '';
  let   tip     = rawTip;

  // O is measured by rendering the whole track through the chain heard.
  if (p === PROV.MEASURED && tap === 'o') {
    tip = 'Measured [M] — the whole track rendered through the chain heard, in the background';
  }

  const tipAttr = tip ? ` data-tip="${_escAttr(tip)}"` : '';
  return `<span class="prov-badge ${cls}"${tipAttr}>${label}</span>`;
}

/**
 * Return tooltip text for a provenance value (for aria / title).
 * @param {number}  prov
 * @param {number}  coveragePct
 * @param {string}  tap
 * @returns {string}
 */
export function provTooltipText(prov, coveragePct, tap) {
  const p = prov;
  if (p === PROV.MEASURED && tap === 'o' && typeof coveragePct === 'number') {
    return `Measured [M] — ${coveragePct.toFixed(0)}% of track played`;
  }
  return _BADGE_TIP[p] ?? '';
}

/**
 * Check if the short-track LRA warning should be shown.
 * EBU 3342 recommends at least 60 s of gated content for reliable LRA.
 * We approximate: if durationS < 60 and LRA metric is defined, show [!].
 * @param {number} durationS
 * @returns {boolean}
 */
export function isShortTrackLRA(durationS) {
  return typeof durationS === 'number' && durationS < 60;
}

/**
 * CSS class list for a metric cell based on its state.
 * Returns an array of CSS class strings.
 * @param {number}  prov
 * @param {boolean} hpActive
 * @param {string}  tap
 * @returns {string[]}
 */
export function metricCellClasses(prov, hpActive, tap) {
  const p   = applyHpRule(prov, hpActive, tap);
  const cls = ['metric-cell'];
  if (p === PROV.STALE)      cls.push('metric-stale');
  if (p === PROV.HP_PENDING) cls.push('metric-hp-pending');
  if (p === PROV.COMPUTING)  cls.push('metric-computing');
  if (p === PROV.UNAVAIL)    cls.push('metric-unavail');
  return cls;
}

// ── Internal ─────────────────────────────────────────────────────────────────

function _escAttr(s) {
  return s
    .replace(/&/g, '&amp;')
    .replace(/"/g, '&quot;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}
