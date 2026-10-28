// aura-url.js — the address of the app's own `aura` protocol (main.rs serves
// it: player status, spectra, covers, analyzer tiles, update progress).
//
// The webview names a custom protocol by the system: on Windows (and Android)
// it is https://aura.localhost/…, on macOS and Linux aura://localhost/…. The
// pages are served the same way (https://tauri.localhost or tauri://localhost),
// so the page's own address tells which. Outside the app (a page opened in a
// browser, the node tests) the Windows form, as before.

const AURA_BASE = (typeof location !== 'undefined' && location.protocol === 'tauri:')
    ? 'aura://localhost'
    : 'https://aura.localhost';

/** The full address of `path` ("/player/status") on the aura protocol. */
export function auraUrl(path) {
    return AURA_BASE + path;
}
