
export const state = {
    // Converter state
    convCustomFilterPath: null,
    convCustomFilterName: '',
    convCustomFilterTaps: 0,
    convIsConverting: false,
    convPollTimer: null,
    convFileQueue: [],        // current backend batch
    convNextQueue: [],        // files added while converting
    convCurrentFilePct: 0,    // 0-100
    convLastDone: 0,
    convQueueRev: 0,          // last queue revision merged from get_queue_status
    dzClickSuppressed: false
};
