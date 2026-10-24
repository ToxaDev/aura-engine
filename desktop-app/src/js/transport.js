// ══════════════════════════════════════════════════════════════════════
// The transport's buttons for what plays (player.js): a file has a list
// to step through and a repeat mode — ⏮ ▶ ■ ⏭ and repeat; a stream has
// neither — ▶ and ■ only. On the radio ⏮ and ⏭ stepped to another station
// and the stream started loading again (Anton 4.10). The repeat chosen for
// files stays as it was while a stream plays.
// ══════════════════════════════════════════════════════════════════════

/// True when the row is the files' five buttons. A stream playing has two
/// — the input too (another player's stream), listened to with or without
/// its player playing yet; a file playing, paused or getting ready, five.
/// Nothing playing: what ▶ would start now (player.js onPlay) — the input,
/// when it was the last thing played, or the last stream, when the radio
/// was or the list has no file to play, has two; the list, five. So Stop
/// and ▶ again do not change the row.
///   lastSource 'radio' | 'list' | 'input' — what played last; stream — a
///   last stream is there to come back to; file — the list has a file to play.
export function fileTransport(status, { lastSource = 'list', stream = false, file = false } = {}) {
    if (status?.radio || status?.input?.listening) return false;
    if ((status?.state || 'stopped') !== 'stopped') return true;
    if (lastSource === 'input') return false;
    return !(stream && (lastSource === 'radio' || !file));
}
