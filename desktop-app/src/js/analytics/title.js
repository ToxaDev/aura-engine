/**
 * title.js — Whose analyzer window this is, in its title bar (the window has
 * no system frame: the bar is the page's own). Several can be open: a file
 * window says its file; the live window says what plays — the file's name,
 * or a stream's station and song — from the moment that changes.
 *
 * The bar tells the parts apart by colour (Anton 4.10: all in one colour they
 * ran together): the window's name in the app's blue, a stream's station in
 * the gold of the analysis line, the song or the file in white, the dots
 * between them dim (analytics.css .an-t-*). The system's title is the same
 * text, plain.
 */

const APP = 'AuraEngine Analyzer';
const SEP = ' · ';

/** A path's last part (either slash). */
export function fileName(p) {
  return String(p || '').split(/[\\/]/).pop();
}

/**
 * The window's title in its parts, in order: { kind, text } — kind 'app' (the
 * window's name), 'mode' (LIVE), 'station', 'song', 'file', 'note' (a file's
 * "(converted)"), 'sep' (the dots between). Joined, the texts are the title.
 * @param {{ mode: string, path?: string|null, conv?: string|null, entryId?: string|null,
 *           stream?: { station: string, song: string }|null, file?: string|null }} w
 *   mode: 'live' or 'file'; path, conv, entryId: a file window's own (its URL);
 *   stream: what the live window hears on a stream; file: the file it hears
 *   (the player's status names it)
 */
export function titleParts({ mode, path = null, conv = null, entryId = null, stream = null, file = null }) {
  const parts = [{ kind: 'app', text: APP }];
  const add = (kind, text) => { parts.push({ kind: 'sep', text: SEP }, { kind, text }); };
  if (mode !== 'live') {
    // A file window: the file's name (it read "file-12 · 3fa2b1c0" for a
    // file that was not converted: the list's id and a hash of its rack).
    if (conv) {
      add('file', fileName(conv));
      parts.push({ kind: 'note', text: ' (converted)' });
    } else {
      add('file', fileName(path) || (entryId ? `file ${entryId}` : 'file'));
    }
    return parts;
  }
  if (stream) {
    if (stream.station) add('station', stream.station);
    if (stream.song) add('song', stream.song);
    return parts;
  }
  add('mode', 'LIVE');
  if (file) add('file', file);
  return parts;
}

/** The window's title as one line (the system's title); see titleParts. */
export function windowTitle(w) {
  return titleParts(w).map(p => p.text).join('');
}

const escHtml = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** The title bar's markup: each part in its own span, class an-t-<kind>; a
 *  station's or a song's text comes from the stream, so it is escaped. */
export function titleMarkup(parts) {
  return parts.map(p => `<span class="an-t-${p.kind}">${escHtml(p.text)}</span>`).join('');
}
