// The streamer's answer to the app's aura-url.js: the `aura` protocol's routes
// are on the same server as the page, under /aura.

/** The full address of `path` ("/player/status") on the aura protocol. */
export function auraUrl(path) {
    return '/aura' + path;
}
