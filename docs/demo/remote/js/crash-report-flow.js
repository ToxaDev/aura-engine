// What the report window does, apart from how it looks (crash-report.js draws
// it): which offer a start makes, and when a report leaves. A report leaves
// only on a press of Send, or, after a session that ended badly, with the
// "always send" box the user ticked. Only those two call crash_report_send;
// crash_report_retry sends again only what they could not send.

export const AUTO_KEY = 'auraReportAuto';
export const RETRY_EVERY_MS = 3600 * 1000;

function readAuto(storage) {
    try { return storage?.getItem(AUTO_KEY) === '1'; } catch (_) { return false; }
}

function writeAuto(storage, on) {
    try { storage?.setItem(AUTO_KEY, on ? '1' : '0'); } catch (_) { /* a session's choice, then */ }
}

/// What a start does about `state` (`crash_report_state`): 'none', 'window'
/// (it asks), or 'auto' (the box is ticked: it goes by itself).
export function startAction(state, auto) {
    if (!state?.crashed && !state?.asked) return 'none';
    return auto ? 'auto' : 'window';
}

/// `invoke`: Tauri's. `view`: hideBar(), showWindow(kind: how the last session
/// ended, null for a report opened by hand), hideWindow(), showPreview(json),
/// busy(on), sent(code), saved(code, path), failed(error, kept), autoSent(code),
/// autoFailed(error, kept), setAuto(on), note(). `storage`: localStorage.
export function createReportFlow({ invoke, view, storage }) {
    let about = 'current';
    let lastKind = null;

    const build = () => invoke('crash_report_build', { about, note: view.note() });

    /// The session that ended badly has had its report: not offered again.
    const settled = () => (about === 'last' ? invoke('crash_report_done') : Promise.resolve());

    const flow = {
        async start() {
            const state = await invoke('crash_report_state');
            const act = startAction(state, readAuto(storage));
            if (act === 'none') return act;
            about = 'last';
            lastKind = state.kind || 'unclean_exit';
            if (act === 'window') {
                flow.open('last');
            } else {
                await flow.sendQuietly();
            }
            return act;
        },

        open(what = 'current') {
            about = what;
            view.hideBar();
            view.setAuto(readAuto(storage));
            view.showWindow(what === 'last' ? lastKind : null);
        },

        close() {
            view.hideWindow();
        },

        /// The bar's or the window's "not now".
        async decline() {
            view.hideBar();
            view.hideWindow();
            await settled();
        },

        /// The window's own button: "not now" about a session that ended badly,
        /// and only "close" about a report opened by hand.
        dismiss() {
            return about === 'last' ? flow.decline() : flow.close();
        },

        async show() {
            view.busy(true);
            try {
                view.showPreview((await build()).json);
            } catch (e) {
                view.failed(String(e), null);
            } finally {
                view.busy(false);
            }
        },

        /// The button: what the user wrote goes in, then it goes.
        async send() {
            view.busy(true);
            try {
                const built = await build();
                view.showPreview(built.json);
                const { code } = await invoke('crash_report_send', { auto: false });
                view.sent(code);
                await settled();
            } catch (e) {
                view.failed(e?.error || String(e), e?.kept || null);
            } finally {
                view.busy(false);
            }
        },

        async save() {
            view.busy(true);
            try {
                await build();
                const { code, path } = await invoke('crash_report_save');
                view.saved(code, path);
                await settled();
            } catch (e) {
                view.failed(String(e), null);
            } finally {
                view.busy(false);
            }
        },

        /// The ticked box, after a bad end: no window, one line with the code.
        async sendQuietly() {
            try {
                await build();
                const { code } = await invoke('crash_report_send', { auto: true });
                view.autoSent(code);
                await settled();
            } catch (e) {
                view.autoFailed(e?.error || String(e), e?.kept || null);
                // It waits in the outbox and goes by itself: nothing is left to ask.
                if (e?.kept) await settled();
            }
        },

        /// A quiet try at what is waiting in the outbox. Says nothing either way.
        async retryOutbox() {
            try {
                await invoke('crash_report_retry', { auto: readAuto(storage) });
            } catch (_) { /* the next round */ }
        },

        setAuto(on) {
            writeAuto(storage, on);
        },
    };
    return flow;
}
