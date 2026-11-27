import test from 'node:test';
import assert from 'node:assert/strict';
import { createReportFlow, startAction, AUTO_KEY } from '../src/js/crash-report-flow.js';

function rig({ state = {}, auto = false, sendFails = false, kept = 'K:\\kept.aerep' } = {}) {
    const calls = [];
    const seen = [];
    const store = new Map(auto ? [[AUTO_KEY, '1']] : []);
    const invoke = async (cmd, args) => {
        calls.push([cmd, args]);
        if (cmd === 'crash_report_state') return state;
        if (cmd === 'crash_report_build') return { json: '{"note":"x"}' };
        if (cmd === 'crash_report_send') {
            if (sendFails) throw { error: 'offline', kept };
            return { code: 'AE-7K3F-Q9' };
        }
        if (cmd === 'crash_report_save') return { code: 'AE-AAAA-22', path: 'D:\\x.aerep' };
        return null;
    };
    const view = new Proxy({}, {
        get: (_, name) => (name === 'note' ? () => 'my note' : (...a) => { seen.push([name, ...a]); }),
    });
    const storage = { getItem: k => store.get(k) ?? null, setItem: (k, v) => store.set(k, v) };
    const flow = createReportFlow({ invoke, view, storage });
    const called = cmd => calls.filter(c => c[0] === cmd);
    const did = name => seen.filter(s => s[0] === name);
    return { flow, calls, called, did, store };
}

test('a start offers according to how the last session ended', () => {
    assert.equal(startAction({ crashed: false, asked: false }, false), 'none');
    assert.equal(startAction({ crashed: false, asked: false }, true), 'none', 'a clean end sends nothing, box or no box');
    assert.equal(startAction({ crashed: true, asked: false }, false), 'window');
    assert.equal(startAction({ crashed: true, asked: true }, false), 'window');
    assert.equal(startAction({ crashed: false, asked: true }, false), 'window');
    assert.equal(startAction({ crashed: true, asked: false }, true), 'auto');
});

test('with the box off, nothing leaves after a bad end until Send is pressed', async () => {
    const r = rig({ state: { crashed: true, asked: false, kind: 'stall' } });
    assert.equal(await r.flow.start(), 'window');
    assert.deepEqual(r.did('showWindow')[0], ['showWindow', 'stall'], 'the window, saying how it ended');
    assert.equal(r.called('crash_report_build').length, 0);
    await r.flow.show();
    await r.flow.save();
    assert.equal(r.called('crash_report_send').length, 0, 'showing and saving do not send');
    await r.flow.send();
    assert.equal(r.called('crash_report_send').length, 1);
    assert.deepEqual(r.called('crash_report_send')[0][1], { auto: false });
});

test('a window opened by hand says nothing of a last session', async () => {
    const r = rig();
    r.flow.open('current');
    assert.deepEqual(r.did('showWindow')[0], ['showWindow', null]);
    await r.flow.dismiss();
    assert.equal(r.did('hideWindow').length, 1);
    assert.equal(r.called('crash_report_done').length, 0, 'closing it settles nothing');
});

test('Not now in the window settles the last session and sends nothing', async () => {
    const r = rig({ state: { crashed: true, kind: 'unclean_exit' } });
    await r.flow.start();
    await r.flow.dismiss();
    assert.equal(r.did('hideWindow').length, 1);
    assert.equal(r.called('crash_report_done').length, 1);
    assert.equal(r.called('crash_report_send').length, 0);
});

test('a state with no kind still opens the window', async () => {
    const r = rig({ state: { asked: true } });
    await r.flow.start();
    assert.deepEqual(r.did('showWindow')[0], ['showWindow', 'unclean_exit']);
});

test('a round of the outbox tells what the box says now, and swallows a failure', async () => {
    const seen = [];
    const store = new Map([[AUTO_KEY, '1']]);
    const flow = createReportFlow({
        invoke: async (c, a) => { seen.push([c, a]); throw new Error('no'); },
        view: {},
        storage: { getItem: k => store.get(k) ?? null, setItem: (k, v) => store.set(k, v) },
    });
    await flow.retryOutbox();
    assert.deepEqual(seen[0], ['crash_report_retry', { auto: true }]);
    store.set(AUTO_KEY, '0');
    await flow.retryOutbox();
    assert.deepEqual(seen[1], ['crash_report_retry', { auto: false }]);
});

test('with the box on, a bad end sends by itself and says the code', async () => {
    const r = rig({ state: { crashed: true, asked: false }, auto: true });
    assert.equal(await r.flow.start(), 'auto');
    assert.equal(r.called('crash_report_send').length, 1);
    assert.deepEqual(r.called('crash_report_build')[0][1], { about: 'last', note: 'my note' });
    assert.deepEqual(r.called('crash_report_send')[0][1], { auto: true });
    assert.equal(r.did('showWindow').length, 0);
    assert.deepEqual(r.did('autoSent')[0], ['autoSent', 'AE-7K3F-Q9']);
    assert.equal(r.called('crash_report_done').length, 1);
});

test('the box does not send a report the user opens by hand', async () => {
    const r = rig({ auto: true });
    assert.equal(await r.flow.start(), 'none');
    r.flow.open('current');
    assert.equal(r.called('crash_report_send').length, 0);
});

test('a report opened by hand is about this session and settles nothing', async () => {
    const r = rig();
    r.flow.open('current');
    await r.flow.send();
    assert.equal(r.called('crash_report_build')[0][1].about, 'current');
    assert.deepEqual(r.did('sent')[0], ['sent', 'AE-7K3F-Q9']);
    assert.equal(r.called('crash_report_done').length, 0);
});

test('Send builds again, so what was written last goes', async () => {
    const r = rig();
    r.flow.open('current');
    await r.flow.show();
    await r.flow.send();
    const order = r.calls.map(c => c[0]);
    assert.deepEqual(order, ['crash_report_build', 'crash_report_build', 'crash_report_send']);
});

test('a failed send says so, names the kept file and keeps the offer', async () => {
    const r = rig({ state: { crashed: true }, sendFails: true });
    await r.flow.start();
    r.flow.open('last');
    await r.flow.send();
    assert.deepEqual(r.did('failed')[0], ['failed', 'offline', 'K:\\kept.aerep']);
    assert.equal(r.called('crash_report_done').length, 0);
    assert.equal(r.did('sent').length, 0);
});

test('closing the window after a failed send settles the session: the file waits by itself', async () => {
    const r = rig({ state: { crashed: true }, sendFails: true });
    await r.flow.start();
    await r.flow.send();
    await r.flow.dismiss();
    assert.equal(r.called('crash_report_done').length, 1);
    assert.equal(r.called('crash_report_send').length, 1, 'closing sends nothing more');
});

test('declining settles the last session', async () => {
    const r = rig({ state: { crashed: true } });
    await r.flow.start();
    await r.flow.decline();
    assert.equal(r.called('crash_report_done').length, 1);
    assert.equal(r.called('crash_report_send').length, 0);
});

test('the box is remembered, off by default', async () => {
    const r = rig();
    r.flow.open('current');
    assert.deepEqual(r.did('setAuto')[0], ['setAuto', false]);
    r.flow.setAuto(true);
    assert.equal(r.store.get(AUTO_KEY), '1');
    r.flow.open('current');
    assert.deepEqual(r.did('setAuto')[1], ['setAuto', true]);
});

test('storage that throws reads as off', async () => {
    const view = new Proxy({}, { get: (_, n) => (n === 'note' ? () => '' : () => {}) });
    const calls = [];
    const flow = createReportFlow({
        invoke: async (c) => { calls.push(c); return c === 'crash_report_state' ? { crashed: true } : {}; },
        view,
        storage: { getItem() { throw new Error('no'); }, setItem() { throw new Error('no'); } },
    });
    assert.equal(await flow.start(), 'window');
    assert.ok(!calls.includes('crash_report_send'));
    flow.setAuto(true);
});

test('a report that could not go by itself says so in the bar: no window opens on a start', async () => {
    const r = rig({ state: { crashed: true, asked: false }, auto: true, sendFails: true });
    assert.equal(await r.flow.start(), 'auto');
    assert.deepEqual(r.did('autoFailed')[0], ['autoFailed', 'offline', 'K:\\kept.aerep']);
    assert.equal(r.did('failed').length, 0);
    assert.equal(r.did('showWindow').length, 0);
    assert.equal(r.called('crash_report_done').length, 1, 'it waits in the outbox: nothing is left to ask');
});

test('a report that could not go and could not wait keeps the offer', async () => {
    const r = rig({ state: { crashed: true }, auto: true, sendFails: true, kept: null });
    await r.flow.start();
    assert.deepEqual(r.did('autoFailed')[0], ['autoFailed', 'offline', null]);
    assert.equal(r.called('crash_report_done').length, 0);
});
