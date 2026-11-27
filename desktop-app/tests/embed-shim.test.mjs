// The Library window's centre pages (centre-page.js) are frames of the main
// window: their Close/Save must give the centre back, never close the app,
// also where the frame has a bridge of its own (WebView2 puts one in every
// frame; XTC's Save closed the app, 22.11).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const SRC = readFileSync(new URL('../src/js/vis/embed-shim.js', import.meta.url), 'utf8');

function frame({ own, parentBridge }) {
    const posted = [];
    const real = { closed: 0, sized: 0, full: null };
    const bridge = () => ({
        event: { listen: () => Promise.resolve(() => {}) },
        core: { invoke: () => Promise.resolve('ok') },
        window: {
            getCurrentWindow: () => ({
                close: () => { real.closed++; return Promise.resolve(); },
                setSize: () => { real.sized++; return Promise.resolve(); },
                setFullscreen: (on) => { real.full = on; return Promise.resolve(); },
            }),
        },
    });
    const parent = { postMessage: (m) => posted.push(m), __TAURI__: parentBridge ? bridge() : undefined };
    const window = { parent, location: { search: '?embedded' }, __TAURI__: own ? bridge() : undefined };
    const document = { documentElement: { classList: { add() {} } } };
    vm.runInNewContext(SRC, { window, document, location: window.location, Proxy, Object, Promise });
    return { window, posted, real };
}

for (const own of [true, false]) {
    test(`a centre page's Close gives the centre back, not the app (${own ? 'own' : 'borrowed'} bridge)`, async () => {
        const f = frame({ own, parentBridge: true });
        const w = f.window.__TAURI__.window.getCurrentWindow();
        await w.close();
        await w.setSize({});
        await w.minimize();
        assert.equal(f.real.closed, 0);
        assert.equal(f.real.sized, 0);
        assert.equal(JSON.stringify(f.posted), JSON.stringify([{ auraClosePage: true }]));
        await w.setFullscreen(true);
        assert.equal(f.real.full, true, 'full screen goes through');
        assert.equal(await f.window.__TAURI__.core.invoke('x'), 'ok', 'commands still work');
    });
}
