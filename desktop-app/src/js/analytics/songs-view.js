/**
 * songs-view.js — a stream's songs (the radio): the one playing and the last
 * ones heard, each with its integrated loudness, LRA, true peak, DR and
 * bandwidth, S beside O (stream_totals.rs, through /player/an/stream, kept
 * by window.js in the store as `_streamTotals`). Songs are cut where the
 * stream's title changes, so their edges are approximate: the Song header's
 * tooltip says so. Titles come from the stream: they go in as text, never
 * as markup.
 *
 * createSongsView(container, store, stationOf)
 *   stationOf(entry) — the station's name for a song's {url, icy}.
 */

import { STREAM_TEXTS, bandwidthShort, formatMetric, PROV } from './protocol.js';
import { METRIC_LABEL, formatTimeInt } from './format.js';

// [key, the metric id it formats like, its header].
const COLS = [
  ['lufsI', 0, METRIC_LABEL[0]],
  ['lra',   3, METRIC_LABEL[3]],
  ['tp',    4, STREAM_TEXTS.tpMax],
  ['dr',    8, METRIC_LABEL[8]],
];

function el(tag, cls, text, tip) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  if (tip) e.setAttribute('data-tip', tip);
  return e;
}

const fmt = (id, v) => (v != null && Number.isFinite(v)) ? formatMetric(id, v, PROV.MEASURED) : '—';

/** One song's row: the title (and its marks, its station), the time heard,
 *  S and O of each measure, the bandwidth. */
function songRow(s, playing, stationOf) {
  const tr = el('tr', playing ? 'an-songs-now' : '');
  const name = el('td', 'an-songs-title');
  name.append(el('span', '', s.title || '—'));
  if (playing) name.append(el('span', 'an-songs-mark', STREAM_TEXTS.now));
  if (s.joined) name.append(el('span', 'an-songs-mark', STREAM_TEXTS.joined, STREAM_TEXTS.joinedTip));
  if (s.cut) name.append(el('span', 'an-songs-mark', STREAM_TEXTS.cut, STREAM_TEXTS.cutTip));
  const st = stationOf ? stationOf(s) : '';
  if (st) name.append(el('div', 'an-songs-station', st));
  tr.append(name, el('td', 'an-songs-num', formatTimeInt(s.playedS)));
  for (const [key, id] of COLS) {
    tr.append(el('td', 'an-songs-num an-songs-s', fmt(id, s.s?.[key])));
    tr.append(el('td', 'an-songs-num an-songs-o', fmt(id, s.o?.[key])));
  }
  tr.append(el('td', 'an-songs-num', bandwidthShort(s.bw), STREAM_TEXTS.bwTip));
  return tr;
}

export function createSongsView(container, store, stationOf) {
  const render = (v) => {
    container.textContent = '';
    if (!v) return;
    const done = v.songs || [];
    if (v.titles === false && !done.length) {
      container.append(el('div', 'an-songs-empty', STREAM_TEXTS.noTitles));
      return;
    }
    const table = el('table', 'an-songs-tbl');
    const h1 = el('tr'), h2 = el('tr');
    h1.append(el('th', 'an-songs-title', STREAM_TEXTS.colSong, STREAM_TEXTS.songTip), el('th', 'an-songs-num', STREAM_TEXTS.colPlayed));
    h2.append(el('th'), el('th'));
    for (const [, , label] of COLS) {
      const th = el('th', 'an-songs-group', label);
      th.colSpan = 2;
      h1.append(th);
      h2.append(el('th', 'an-songs-s', 'S'), el('th', 'an-songs-o', 'O'));
    }
    h1.append(el('th', 'an-songs-num', STREAM_TEXTS.colBandwidth, STREAM_TEXTS.bwTip));
    h2.append(el('th'));
    const head = el('thead');
    head.append(h1, h2);
    const body = el('tbody');
    if (v.song) body.append(songRow(v.song, true, stationOf));
    for (const s of done) body.append(songRow(s, false, stationOf));
    table.append(head, body);
    container.append(table);
    if (!done.length) container.append(el('div', 'an-songs-empty', STREAM_TEXTS.noneYet));
  };
  store.on('_streamTotals', render);
  render(store.get('_streamTotals'));
}
