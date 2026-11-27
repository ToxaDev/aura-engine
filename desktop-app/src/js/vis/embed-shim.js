// A page shown inside the main window (an inner frame: the library's
// Visualizer page, vis-studio.html?embedded; a window of the app in the
// Library window's centre, centre-page.js): the page's root is marked, so
// the window's own head and the full screen are left out (vis-studio.css),
// and its bridge to the app (its own, else the main window's) has no window
// of its own: Close, Cancel and Save give the centre back. Its event listeners are counted, so the page can end them when it
// puts the frame away (`__auraOff`).
(function () {
    if (!/[?&]embedded\b/.test(location.search) || window.parent === window) return;
    document.documentElement.classList.add('embedded');
    window.__auraOff = function () {};
    try {
        // The frame's own bridge where the app gives it one (WebView2 puts it
        // in every frame), else the main window's. Either way its window is
        // the main window: left as it is, the XTC page's Save closed the app
        // (22.11) and the Cue page sized the main window to itself.
        var own = window.__TAURI__;
        var P = own || window.parent.__TAURI__, offs = [];
        if (!P) return;
        var event = Object.create(P.event);
        event.listen = function () { var p = P.event.listen.apply(P.event, arguments); offs.push(p); return p; };
        var T = Object.create(P);
        T.event = event;
        // This page is no window of its own: its Close, Cancel and Save
        // give the centre back (centre-page.js), the rest does nothing — the
        // main window's would minimize, move or close the app.
        var close = function () { try { window.parent.postMessage({ auraClosePage: true }, '*'); } catch (_) {} return Promise.resolve(); };
        // The main window's full screen (the Visualizer's preview) and
        // what it is asked about go through; sizing, moving, minimizing
        // and closing do not.
        var realOf = P.window && P.window.getCurrentWindow, real = null;
        var THROUGH = { setFullscreen: 1, isFullscreen: 1, isMaximized: 1, isMinimized: 1, isFocused: 1, isVisible: 1, scaleFactor: 1, innerSize: 1, outerSize: 1, innerPosition: 1, outerPosition: 1 };
        var me = new Proxy({ label: 'embedded' }, {
            get: function (t, k) {
                if (k in t) return t[k];
                if (k === 'then') return undefined;
                if (k === 'close' || k === 'destroy' || k === 'hide') return close;
                if (THROUGH[k] && realOf) {
                    return function () { try { real = real || realOf.call(P.window); return real[k].apply(real, arguments); } catch (e) { return Promise.reject(e); } };
                }
                if (typeof k === 'string' && (k.indexOf('on') === 0 || k === 'listen')) return function () { return Promise.resolve(function () {}); };
                return function () { return Promise.resolve(null); };
            },
        });
        T.window = Object.assign(Object.create(P.window || {}), { getCurrentWindow: function () { return me; } });
        if (P.webviewWindow) T.webviewWindow = Object.assign(Object.create(P.webviewWindow), { getCurrentWebviewWindow: function () { return me; } });
        try { window.__TAURI__ = T; } catch (_) {}
        if (window.__TAURI__ !== T) {
            // A bridge that can not be replaced: its window calls in place.
            try { Object.defineProperty(P.window, 'getCurrentWindow', { value: function () { return me; }, configurable: true }); } catch (_) {}
            try { if (P.webviewWindow) Object.defineProperty(P.webviewWindow, 'getCurrentWebviewWindow', { value: function () { return me; }, configurable: true }); } catch (_) {}
        }
        window.__auraOff = function () {
            offs.splice(0).forEach(function (p) { Promise.resolve(p).then(function (f) { try { f(); } catch (_) {} }); });
        };
    } catch (_) {}
})();
