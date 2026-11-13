/**
 * progress.js — The whole-track analysis's progress as the gold line under
 * the chain shows it: the stages still running, how much of the whole is
 * done, and where its percentage rides on the line. Pure (no DOM): window.js
 * draws it.
 */

/** What each stage weighs in the whole: the source and the source stages
 *  are quick passes, the output's (the whole track through the chain) takes
 *  the time — so the percentage runs about as the time does. */
export const STAGE_WEIGHT = { s: 1, b: 1, o: 8 };

const SOURCE = 'Source, the file as it is';
const STAGES = 'Stages, the file through the source stages';
const OUTPUT = 'Output, the whole track through the chain heard';

/**
 * The stages of the analysis now, from the track frame.
 * @param {Array|null} info  the frame's spec_info: S's, B's and O's spectrogram tiles, or null
 * @param {number|null} oPct  the output pass's progress 0..100; null when there is none
 *   (BIT-PERFECT plays the file itself, a stream has no whole track)
 * @param {{ decoding?: boolean, eta?: string }} [opts]
 *   decoding: a file whose S has not started yet waits for it (a stream has none);
 *   eta: what the output pass has left, said after its percentage (", about 12 s left")
 * @returns {{ key: 's'|'b'|'o', f: number, text: string }[]}
 */
export function analysisStages(info, oPct, { decoding = false, eta = '' } = {}) {
  const out = [];
  const tiles = (i, key, name) => {
    const t = info && info[i];
    if (!t || !t.total) return false;
    const f = Math.min(1, t.ready / t.total);
    out.push({ key, f, text: f >= 1 ? `${name}: done` : `${name}: spectrogram ${t.ready} of ${t.total}` });
    return true;
  };
  if (!tiles(0, 's', SOURCE) && decoding) out.push({ key: 's', f: 0, text: `${SOURCE}: decoding` });
  tiles(1, 'b', STAGES);
  if (oPct != null) {
    const f = Math.max(0, Math.min(1, oPct / 100));
    out.push({ key: 'o', f, text: f >= 1 ? `${OUTPUT}: done` : `${OUTPUT}: ${oPct}%${eta}` });
  }
  return out;
}

/** The whole: the stages' done shares, weighed (0..1), and how many still run. */
export function overallProgress(stages) {
  let w = 0, done = 0, open = 0;
  for (const st of stages) {
    const k = STAGE_WEIGHT[st.key] ?? 1;
    w += k;
    done += k * st.f;
    if (st.f < 1) open++;
  }
  return { frac: w > 0 ? done / w : 0, open };
}

/** The badge's words: the whole in whole percent, 100 only when all is done. */
export function percentText(frac) {
  const f = Math.max(0, Math.min(1, frac));
  return `${Math.floor(f * 100 + 1e-9)}%`;
}

/** A button counts as under the tag this near beside it (px). */
export const TAG_GAP = 6;
/** How far the tag keeps above a button under it (px). */
export const TAG_GAP_Y = 2;

/**
 * Where the badge goes on a line `lineW` px wide: centred on the end of the
 * fill, kept whole inside the line — at its ends with its edge on the end
 * of the fill, so at 100 % its right edge is the line's end.
 */
export function badgeLeft(frac, lineW, badgeW) {
  const x = Math.max(0, Math.min(1, frac)) * lineW - badgeW / 2;
  return Math.max(0, Math.min(Math.max(0, lineW - badgeW), x));
}

/**
 * The badge's place: on the end of the fill (badgeLeft) whatever lies under
 * it — it used to wait short of the line's end beside the button there, and
 * 100 % ended ten pixels before the line did (Anton 4.10). The toolbar's
 * buttons stand under the line's two ends, and the tag is taller than the
 * gap between the line and them (it touched B at 93 %): where a button is
 * under it, or within TAG_GAP beside it, the tag rises until it keeps
 * TAG_GAP_Y px above that button's top.
 * @param {number} frac  the whole's share, 0…1
 * @param {number} lineLeft  the line's left edge (page x)
 * @param {number} lineW  its width
 * @param {number} badgeW  the tag's width
 * @param {number} badgeBottom  the tag's bottom where it sits on the line, not risen (page y)
 * @param {{ left: number, right: number, top: number }[]} controls  the toolbar's controls (page)
 * @returns {{ left: number, lift: number }}  left in the line's own x; lift: px up
 */
export function placeBadge(frac, lineLeft, lineW, badgeW, badgeBottom, controls) {
  const left = badgeLeft(frac, lineW, badgeW);
  const x0 = lineLeft + left - TAG_GAP, x1 = lineLeft + left + badgeW + TAG_GAP;
  let lift = 0;
  for (const c of controls) {
    if (c.right <= x0 || c.left >= x1) continue;
    lift = Math.max(lift, badgeBottom + TAG_GAP_Y - c.top);
  }
  return { left, lift: Math.max(0, Math.ceil(lift)) };
}

/** The line's tooltip: whether it is all done, then a line per stage. */
export function progressTip(stages, open) {
  const head = open === 0 ? 'The whole-track analysis is complete'
                          : 'Still analysing — some views and numbers are not final yet';
  return stages.length ? `${head}\n${stages.map(s => s.text).join('\n')}` : head;
}

/** The status bar's word on it: said once it is all done (meanwhile the
 *  badge on the line says how far it is; a stream has nothing to say). */
export function statusText(stages, open) {
  return stages.length && open === 0 ? 'Whole track analysed' : '';
}
