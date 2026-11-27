// ══════════════════════════════════════════════════════════════════════
// The foot's third line, the part with no page in it: what it says of the
// conversion of the track in the player, from that track's row (dropzone.js
// rowProps) and the batch that runs.
// ══════════════════════════════════════════════════════════════════════

export const FOOT_TEXTS = {
    done: 'CONVERTED',
    doneTip: 'This track is converted with the settings it plays with: its file plays from disk.',
    live: 'CONVERTED · LIVE WITH CUE',
    liveVolume: 'CONVERTED · LIVE BELOW FULL VOLUME',
    doneVolumeTip: 'This track is converted to DSD with these settings, but its file does not play now: a DSD file goes to the DAC as it is and takes no volume. Set the volume to its top to play the file from disk.',
    doneLiveTip: 'This track is converted with these settings, but its file does not play now: it plays live while Cue Consistency is on (the cue is not in the file). Turn CUE off to play the file from disk.',
    convert: 'Convert',
    convertTip: 'Convert this track with the rack as it is now',
    again: 'Convert again',
    queued: 'QUEUED',
    queuedTip: 'Waits its turn to be converted',
    active: pct => `CONVERTING ${Math.round(pct)}%`,
    activeTip: 'This track is being converted',
    batch: pct => `Batch ${Math.round(pct)}%`,
    batchTip: 'Other tracks are being converted: the whole batch so far',
    from: name => name,
    fromTip: name => `Plays from the playlist “${name}” — click to see it in the queue`,
};

const pctOf = v => Math.min(100, Math.max(0, Number(v) || 0));

/// What the line says. `row`: the track's rowProps, or null (nothing in the
/// player, a stream, the input). `batch`: { converting, pct }.
/// → { kind: none|done|live|active|queued|convert, text, tip, pct, batch }
///   batch: the batch's text when it runs on other tracks, else null.
export function footConvView(row, batch = {}) {
    const running = !!batch.converting;
    const others = running ? FOOT_TEXTS.batch(pctOf(batch.pct)) : null;
    if (!row || row.readable === false) return { kind: 'none', text: '', tip: '', pct: 0, batch: others };
    const cls = ` ${row.cls || ''} `;
    if (row.isDisk) {
        // The file is there, yet the track plays live (Cue Consistency):
        // said so, and not in the green of a file that plays.
        if (row.fromDisk === false) {
            const vol = row.liveWhy === 'volume';
            return { kind: 'live', text: vol ? FOOT_TEXTS.liveVolume : FOOT_TEXTS.live, tip: vol ? FOOT_TEXTS.doneVolumeTip : FOOT_TEXTS.doneLiveTip, pct: 100, batch: others };
        }
        return { kind: 'done', text: FOOT_TEXTS.done, tip: FOOT_TEXTS.doneTip, pct: 100, batch: others };
    }
    if (cls.includes(' active ')) {
        const pct = pctOf(row.pct);
        return { kind: 'active', text: FOOT_TEXTS.active(pct), tip: FOOT_TEXTS.activeTip, pct, batch: null };
    }
    if (cls.includes(' queued ')) return { kind: 'queued', text: FOOT_TEXTS.queued, tip: FOOT_TEXTS.queuedTip, pct: 0, batch: others };
    if (row.convertable) {
        const failed = cls.includes(' error ') || cls.includes(' cancelled ');
        return {
            kind: 'convert', text: failed ? FOOT_TEXTS.again : FOOT_TEXTS.convert,
            tip: failed && row.title ? `${row.title} — ${FOOT_TEXTS.convertTip.toLowerCase()}` : FOOT_TEXTS.convertTip,
            pct: 0, batch: others,
        };
    }
    return { kind: 'none', text: '', tip: '', pct: 0, batch: others };
}
