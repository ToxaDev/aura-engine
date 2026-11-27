// ══════════════════════════════════════════════════════════════════════
// The foot's third line (the landscape window's player): under the title,
// what is true of the track in the player —
//
//   its conversion   CONVERTED · a Convert button (the row's own C) ·
//                    QUEUED · CONVERTING 42% with its bar; and the batch's
//                    progress while other tracks are being converted
//   its playlist     the name of the playlist it plays from; a click shows
//                    that playlist in the queue, on the row that plays
//
// Drawn in the player's own render pass (playerViewHooks.foot): nothing is
// polled twice. The compact window does not show the line (player.css): the
// row is right under the player there.
// ══════════════════════════════════════════════════════════════════════

import { state } from './state.js';
import { listHooks, rowProps, convertEntry, showRow } from './dropzone.js';
import { heldEntryOf, playlistOfTrack, switchPlaylist } from './library.js';
import { playerViewHooks } from './player.js';
import { showQueueTab } from './landscape.js';
import { footConvView, FOOT_TEXTS } from './footline-core.js';

const $ = id => document.getElementById(id);

let trackId = null;     // the track the line is about
let last = '';          // what is drawn, as one string

const entryOf = id => (id == null ? null : state.list.find(e => e.trackId === id) || heldEntryOf(id));

function build() {
    const now = document.querySelector('#plBar .pl-now');
    if (!now || $('plFoot')) return !!now;
    const line = document.createElement('div');
    line.className = 'pl-foot';
    line.id = 'plFoot';
    line.innerHTML = `
      <button type="button" class="pl-foot-conv" id="plFootConv" hidden><span class="pl-foot-fill" id="plFootFill"></span><span class="pl-foot-t" id="plFootConvT"></span></button>
      <span class="pl-foot-batch" id="plFootBatch" hidden></span>
      <button type="button" class="pl-foot-from" id="plFootFrom" hidden><svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><line x1="4" y1="7" x2="20" y2="7"/><line x1="4" y1="12" x2="20" y2="12"/><line x1="4" y1="17" x2="13" y2="17"/></svg><span id="plFootFromT"></span></button>`;
    now.appendChild(line);
    $('plFootConv').addEventListener('click', e => {
        if (e.detail > 0) e.currentTarget.blur();   // Space stays play/pause
        if (e.currentTarget.dataset.kind !== 'convert') return;
        const entry = entryOf(trackId);
        if (!entry) return;
        // A row of the playlist that plays behind the one shown: its
        // conversion is tied to the rows shown, so that playlist comes up.
        const pl = playlistOfTrack(trackId);
        if (pl && !pl.shown) switchPlaylist(pl.id);
        convertEntry(entry);
    });
    $('plFootFrom').addEventListener('click', e => {
        if (e.detail > 0) e.currentTarget.blur();
        const pl = playlistOfTrack(trackId);
        if (!pl) return;
        if (!pl.shown) switchPlaylist(pl.id);
        showQueueTab();
        const entry = state.list.find(x => x.trackId === trackId);
        if (entry) requestAnimationFrame(() => showRow(entry.id));
    });
    return true;
}

function draw(s) {
    if (!build()) return;
    // A stream and the input have no row.
    trackId = s && !s.radio && !s.input?.listening ? s.trackId ?? null : null;
    const entry = entryOf(trackId);
    const v = footConvView(entry ? rowProps(entry, listHooks.nowPlaying()) : null,
        { converting: state.convIsConverting, pct: state.convOverallPct });
    const pl = entry ? playlistOfTrack(trackId) : null;
    const key = [v.kind, v.text, v.tip, v.pct.toFixed(1), v.batch, pl?.name].join('|');
    if (key === last) return;
    last = key;

    const btn = $('plFootConv');
    btn.hidden = v.kind === 'none';
    btn.dataset.kind = v.kind;
    btn.tabIndex = v.kind === 'convert' ? 0 : -1;
    $('plFootConvT').textContent = v.text;
    $('plFootFill').style.width = (v.kind === 'active' ? v.pct : 0) + '%';
    btn.title = v.tip;

    const batch = $('plFootBatch');
    batch.hidden = !v.batch;
    batch.textContent = v.batch || '';
    batch.title = v.batch ? FOOT_TEXTS.batchTip : '';

    const from = $('plFootFrom');
    from.hidden = !pl;
    $('plFootFromT').textContent = pl ? FOOT_TEXTS.from(pl.name) : '';
    from.title = pl ? FOOT_TEXTS.fromTip(pl.name) : '';
}

export function initFootLine() {
    playerViewHooks.foot = draw;
}
