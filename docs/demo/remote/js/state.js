
export const state = {
    // Converter state
    convCustomFilterPath: null,
    convCustomFilterName: '',
    convCustomFilterTaps: 0,
    convIsConverting: false,
    convCancelRequested: false, // the batch was cancelled and is stopping
    convPollTimer: null,
    convFileQueue: [],        // current backend batch
    convNextQueue: [],        // files added while converting
    convMPendingGroups: [],   // M3: [{paths, mOverride}] waiting for the current batch
    convCurrentFilePct: 0,    // 0-100
    convLastDone: 0,
    convQueueRev: 0,          // last queue revision merged from get_queue_status
    convOverallPct: 0,        // overall batch progress 0–100 (for the Convert all button)
    convAllResult: null,      // { text, title } shown in the button after a batch ends
    dzClickSuppressed: false,
    // What the rack's strip says beside the controls (rackstrip.js): the
    // pre-flight found no usable GPU (its reason), and no filter is on disk
    // for the FS and length chosen (ui.js refreshFilterAvailability).
    gpuNone: null,
    filterMissing: false,
    // Polyphase FIR as the listener left it, while a track is on and the
    // player holds it switched on (player.js setHeld): saved in its place,
    // and put back on Stop. null while nothing plays.
    pfrBeforePlay: null,
    // The hidden upsampler switch ('single' | 'multi'), kept in the saved
    // settings and sent to the player and the converter; no control in the
    // interface. null = the engine's own default (multistage); 'single' is
    // the way back to the single-stage upsampler.
    upsampler: null,

    // The list under the rack: every file the listener added, in order.
    // One entry per row, whatever happens to it — played, converted, both.
    //   { id, path, name, trackId, info, conv }
    // trackId: the player's id for it (player_add); info: what the player
    // read from its header; conv: the conversion record it is linked to
    // (an object of convFileQueue / convNextQueue, kept after the batch ends
    // so the row still says how it went).
    list: [],
    listSeq: 1,
    // A row is being dragged to a new place (list-drag.js): the list is not
    // redrawn under it; a redraw asked for meanwhile runs when it is let go.
    listDragging: false,
    listRenderPending: false
};
