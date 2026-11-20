// A small stand-in for the page an analyzer panel lives in (document, canvases,
// OffscreenCanvas, the colour worker, animation frames, storage), enough to
// create a view in node and drive it by its events. Not a test file itself.

/** A 2D context that keeps what was written and put where. */
export function fakeCtx() {
  const c = {
    texts: [], puts: [], rects: [],
    fillStyle: '', strokeStyle: '', font: '', lineWidth: 1, globalAlpha: 1, textAlign: 'start',
    textBaseline: 'alphabetic', globalCompositeOperation: 'source-over',
    fillText(t, x, y) { c.texts.push({ t: String(t), x, y }); },
    measureText: (t) => ({ width: String(t).length * 6 }),
    putImageData(img, x, y) { c.puts.push({ img, x, y }); },
    fillRect(x, y, w, h) { c.rects.push({ x, y, w, h, style: c.fillStyle }); },
    createLinearGradient: () => ({ addColorStop() {} }),
  };
  for (const m of ['clearRect', 'strokeRect', 'save', 'restore', 'translate', 'rotate', 'scale', 'setTransform',
    'beginPath', 'closePath', 'rect', 'roundRect', 'clip', 'moveTo', 'lineTo', 'arc', 'stroke', 'fill',
    'setLineDash', 'drawImage']) c[m] = () => {};
  return c;
}

class FakeElement {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.style = {};
    this.dataset = {};
    this.attrs = {};
    this.handlers = {};
    this.textContent = '';
    this.value = '';
    this.hidden = false;
    this.className = '';
    this.clientWidth = 0;
    this.clientHeight = 0;
    this.offsetHeight = 0;
    this.offsetWidth = 0;
    this.offsetLeft = 0;
    this.offsetTop = 0;
    this.offsetParent = {};
    this.width = 0;
    this.height = 0;
    const cls = new Set();
    this.classList = {
      add: (...a) => a.forEach(x => cls.add(x)), remove: (...a) => a.forEach(x => cls.delete(x)),
      toggle: (x, on) => { const v = on === undefined ? !cls.has(x) : !!on; if (v) cls.add(x); else cls.delete(x); return v; },
      contains: (x) => cls.has(x),
    };
    this._ctx = null;
  }
  // Each tag in it becomes a child (flat, with its class): enough to find a span.
  set innerHTML(v) {
    this.children = [];
    this._html = v;
    for (const m of String(v).matchAll(/<([a-zA-Z][\w-]*)([^>]*)>/g)) {
      const e = this.appendChild(new FakeElement(m[1]));
      const cls = /class="([^"]*)"/.exec(m[2]);
      if (cls) e.className = cls[1];
    }
  }
  get innerHTML() { return this._html || ''; }
  appendChild(e) { this.children.push(e); e.parentNode = this; return e; }
  append(...es) { for (const e of es) this.appendChild(e); }
  insertBefore(e) { return this.appendChild(e); }
  setAttribute(k, v) { this.attrs[k] = String(v); if (k.startsWith('data-')) this.dataset[k.slice(5)] = String(v); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; if (k.startsWith('data-')) delete this.dataset[k.slice(5)]; }
  addEventListener(type, fn) { (this.handlers[type] ||= []).push(fn); }
  removeEventListener(type, fn) { this.handlers[type] = (this.handlers[type] || []).filter(f => f !== fn); }
  /** Calls the element's handlers of `type` with `e` (preventDefault and the rest given). */
  fire(type, e = {}) {
    const ev = { preventDefault() {}, stopPropagation() {}, button: 0, clientX: 0, clientY: 0, deltaY: 0, deltaX: 0,
      shiftKey: false, altKey: false, ctrlKey: false, metaKey: false, pointerId: 1, target: this, ...e };
    for (const fn of this.handlers[type] || []) fn(ev);
  }
  querySelectorAll(sel) {
    const tag = String(sel).toUpperCase();
    const out = [];
    const walk = (e) => { for (const c of e.children) { if (c.tagName === tag) out.push(c); walk(c); } };
    walk(this);
    return out;
  }
  querySelector(sel) {
    if (String(sel).startsWith('.')) {
      const name = sel.slice(1);
      let hit = null;
      const walk = (e) => { for (const c of e.children) { if (!hit && String(c.className).split(' ').includes(name)) hit = c; walk(c); } };
      walk(this);
      return hit;
    }
    return this.querySelectorAll(sel)[0] || null;
  }
  getBoundingClientRect() { return { left: 0, top: 0, right: this.clientWidth, bottom: this.clientHeight, width: this.clientWidth, height: this.clientHeight }; }
  setPointerCapture() {}
  releasePointerCapture() {}
  getContext() { return (this._ctx ||= fakeCtx()); }
}

/**
 * Puts the stand-ins on globalThis. `size(el)` gives an element its client
 * size when asked for one (all of them `w`×`h` by default). Returns the
 * means to drive it: the animation frames (run by hand), the worker's
 * messages, and `restore()`.
 */
export function installFakeDom({ w = 800, h = 300 } = {}) {
  const saved = {};
  for (const k of ['document', 'window', 'localStorage', 'OffscreenCanvas', 'Worker', 'requestAnimationFrame', 'cancelAnimationFrame']) saved[k] = globalThis[k];
  const frames = new Map();
  let nextFrame = 1;
  const workers = [];
  globalThis.document = {
    createElement: (tag) => { const e = new FakeElement(tag); e.clientWidth = w; e.clientHeight = h; return e; },
    addEventListener() {}, removeEventListener() {},
    getElementById: () => null,
    visibilityState: 'visible',
  };
  globalThis.window = { devicePixelRatio: 1 };
  const mem = new Map();
  globalThis.localStorage = { getItem: (k) => (mem.has(k) ? mem.get(k) : null), setItem: (k, v) => mem.set(k, String(v)) };
  const offscreen = [];
  globalThis.OffscreenCanvas = class { constructor(cw, ch) { this.width = cw; this.height = ch; this._ctx = fakeCtx(); offscreen.push(this); } getContext() { return this._ctx; } };
  globalThis.Worker = class {
    constructor() { this.sent = []; this.onmessage = null; workers.push(this); }
    postMessage(m) { this.sent.push(m); }
    terminate() {}
  };
  globalThis.requestAnimationFrame = (fn) => { const id = nextFrame++; frames.set(id, fn); return id; };
  globalThis.cancelAnimationFrame = (id) => frames.delete(id);
  return {
    workers,
    /** Every OffscreenCanvas made, in order. */
    offscreen,
    /** Runs the animation frames asked for so far (not the ones they ask for). */
    flushFrames() { const due = [...frames.values()]; frames.clear(); for (const fn of due) fn(performance.now()); },
    /** The worker answers each colour message it holds, in order. */
    answerWorker() {
      for (const wk of workers) {
        const due = wk.sent.splice(0);
        for (const m of due) if (m.type === 'colorize') wk.onmessage?.({ data: { type: 'colorized', id: m.id, imageData: { id: m.id, nCols: m.nCols } } });
      }
    },
    restore() { for (const [k, v] of Object.entries(saved)) globalThis[k] = v; },
  };
}

/** The window's store, as window.js has it (get, set, setDeep, on; store.key reads). */
export function fakeStore(initial = {}) {
  const state = { ...initial };
  const listeners = new Map();
  const api = {
    get: (k) => state[k],
    set(k, v) { const old = state[k]; state[k] = v; for (const fn of listeners.get(k) || []) fn(v, old); },
    setDeep(path, v) {
      const parts = path.split('.');
      let o = state;
      for (const p of parts.slice(0, -1)) o = (o[p] ??= {});
      o[parts.at(-1)] = v;
    },
    on(k, fn) { if (!listeners.has(k)) listeners.set(k, new Set()); listeners.get(k).add(fn); return () => listeners.get(k).delete(fn); },
  };
  return new Proxy(api, { get: (t, p) => (p in t ? t[p] : state[p]) });
}

/** A tile cache that holds nothing. */
export const emptyTiles = { get: () => null, request() {}, drop() {}, dropPrefix() {} };
