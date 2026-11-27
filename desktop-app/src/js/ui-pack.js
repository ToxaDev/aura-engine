// The interface pack: which interface this run serves, and what the update
// dialog says about a newer one.
//
// The exe serves a signed pack of the interface for its own version when one
// lies beside it (ui_pack.rs). The version shown stays the bare number
// (Anton 24.11: nothing after it, an interface update included); the pack's
// number is kept for the update check and the logs only.

/// The version as shown: the bare number, whichever interface is served.
export function versionLabel(version, _patch) {
    return String(version || '');
}

/// The interface update an update_check result offers, or null: none when
/// an app update is available (that one always wins) or when it is not
/// newer than the interface this run serves.
export function uiOffer(result) {
    if (!result || !result.enabled || result.available) return null;
    const u = result.ui_pack;
    const current = Number(result.ui_current) || 0;
    if (!u || !Number.isInteger(u.patch) || u.patch <= current) return null;
    return { patch: u.patch, notes: String(u.notes || ''), version: String(result.current || '') };
}

/// What "Skip this update" remembers: this version's interface update N.
export function uiSkipKey(offer) {
    return `ui:v${offer.version}-p${offer.patch}`;
}

/// The dialog's and the badge's words for an interface update.
export function uiOfferTexts(offer) {
    const what = `Interface update ${offer.patch}`;
    return {
        badge: `${what} for ${offer.version} is available — click to update`,
        title: 'Interface update available',
        line: `${what} for Aura Engine ${offer.version} is ready to install. Only the interface changes; the app restarts.`,
        skip: 'Skip this update',
    };
}

let _info = null;

/// { version, patch, sha256, commit, label } from the app (once), or null
/// in a page opened outside it.
export async function uiPackInfo() {
    if (_info) return _info;
    try {
        _info = await window.__TAURI__.core.invoke('ui_pack_info');
    } catch (_) {
        _info = null;
    }
    return _info;
}

/// The version as the app shows it: the bare number, also with an interface
/// update served.
export async function appVersionLabel() {
    const info = await uiPackInfo();
    if (info && info.version) return versionLabel(info.version, info.patch);
    try {
        return (await window.__TAURI__?.app?.getVersion?.()) || '';
    } catch (_) {
        return '';
    }
}
