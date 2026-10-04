// analytics/export.js — Export popover: text blocks, CSV/TSV, PNG
// Reads the store; window.js gives it the Export button and the graph shown.
// The user-visible strings of the export blocks.
//
// Public API:
//   export function createExportPopover(container, ctx)
//   ctx: { store, getPanelCanvases, getMetricsEl, getWindowTitle }
//
// getPanelCanvases() -> Array<{id, label, canvas: HTMLCanvasElement|null}>
// getMetricsEl()    -> HTMLElement|null  (the metrics table DOM node)
// getWindowTitle()  -> string

import { METRIC_ID, PROV, N_METRICS, formatMetric } from './protocol.js';
import { addPopup } from '../popups.js';

// ── App version (injected at build time; fall back to unknown) ──────────────
const APP_VERSION = (() => {
  if (typeof window !== 'undefined' && window.__TAURI__) {
    // Lazy: resolve once on first export call (synchronous cache).
    let v = null;
    return () => {
      if (v) return v;
      // Fire-and-forget; synchronously return placeholder for now.
      window.__TAURI__.app?.getVersion?.().then(r => { v = r; });
      return '1.5.0';
    };
  }
  return () => '1.5.0';
})();

// ── Metric display names ───────────────────────────────────────────────────
const METRIC_NAMES = [
  'LUFS-I', 'LUFS-S (live)', 'LUFS-M (live)', 'LRA', 'TP (BS.1770)', 'TP (engine 4×)',
  'SP', 'Peak@', 'DR (crest)', 'RMS (whole)', 'RMS top-20%',
  'Gain (−18 LUFS)', 'Clips src ≥2', 'Clips src ≥17', 'Clips out ×1',
  'TP overs', 'DC offset', 'Ultrasonic peak', 'Ultrasonic RMS',
  'Ultrasonic events', 'Infrasonic RMS', 'Sub removed', 'PLR', 'Stereo corr',
  'Eff. BW', 'Coverage O',
];

const METRIC_UNITS = [
  'LUFS', 'LUFS', 'LUFS', 'LU', 'dBTP', 'dBTP', '', 's', '', 'dBFS', 'dBFS',
  'dB', '', '', '', '', '%', 'dBFS', 'dBFS', '', 'dBFS', 'dBFS', 'dB', '', 'Hz', '%',
];

const PROV_NAMES = ['', 'Analytic', 'Forecast', 'Measured', 'Unchanged',
                    'HP Pending', 'Stale', 'Computing'];

// ── Duration helpers ───────────────────────────────────────────────────────
function fmtDur(s) {
  if (!isFinite(s) || s < 0) return '?:??';
  const m = Math.floor(s / 60), sec = (s % 60).toFixed(0).padStart(2, '0');
  return `${m}:${sec}`;
}

function isoNow() {
  return new Date().toISOString().replace(/\.\d+Z$/, 'Z');
}

function fmtDateLocal() {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')} ` +
    `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}`;
}

// ── Metric value access ────────────────────────────────────────────────────
function getVal(store, tap, id) {
  const arr = tap === 's' ? store.metrics.s : tap === 'b' ? store.metrics.b : store.metrics.o;
  return arr ? arr[id] : NaN;
}

function getProv(store, tap, id) {
  const arr = tap === 's' ? store.metrics.sProv : tap === 'b' ? store.metrics.bProv : store.metrics.oProv;
  return arr ? arr[id] : PROV.UNAVAIL;
}

// Format a metric value with sign where appropriate.
function fmtNum(id, v, dp) {
  if (!isFinite(v)) return '—';
  if (dp === undefined) {
    // Use PROTOCOL display rounding
    const rounding = [2,2,2,2,2,2,4,3,0,2,2,2,0,0,0,0,3,1,1,0,1,1,2,3,0,1];
    dp = rounding[id] ?? 2;
  }
  return v.toFixed(dp);
}

// Format TP as "ratio (dBTP)" per foo_truepeak format
function fmtTP(linear, dbtp) {
  if (!isFinite(linear) || !isFinite(dbtp)) return '—';
  const sign = dbtp >= 0 ? '+' : '';
  return `${Math.abs(linear).toFixed(6)} (${sign}${dbtp.toFixed(2)})`;
}

// Format Peak@ as M:SS.mmm
function fmtPeakAt(s) {
  if (!isFinite(s) || s < 0) return '—';
  const m = Math.floor(s / 60);
  const sec = (s % 60).toFixed(3).padStart(6, '0');
  return `${m}:${sec}`;
}

// ── Block A: foo_truepeak ──────────────────────────────────────────────────
function buildFooTruepeak(store, taps = ['s', 'o']) {
  const ver = APP_VERSION();
  const ts = isoNow();
  const tn = store.trackName || 'Unknown Track';

  // Source info
  const srcRate  = store._srcRate || 44100;
  const outRate  = store._outRate || 352800;
  const bits     = store._bits || 24;
  const ch       = 'stereo';
  // Use full filename (with extension) per SPEC §5.5; trackName may be bare name or full path.
  const convPath = store._convPath || store._sourcePath || null;
  const fileBase = convPath
    ? convPath.split(/[/\\]/).pop()
    : tn;  // fall back to track name if no path known

  // Helper: get a scalar metric by ID and tap
  const v = (tap, id) => getVal(store, tap, id);

  // TP linear from dBTP: 10^(db/20)
  const dbToLinear = db => isFinite(db) ? Math.pow(10, db / 20) : NaN;

  const rows = taps.map(tap => {
    const label = tap === 's' ? 'Source' : tap === 'o' ? 'Output' : 'B-tap';
    const tpDb  = v(tap, METRIC_ID.TP_EBUR);
    const tpLin = dbToLinear(tpDb);
    const sp    = v(tap, METRIC_ID.SP);
    const gain  = v(tap, METRIC_ID.GAIN_18LUFS);
    const lra   = v(tap, METRIC_ID.LRA);
    const lufs  = v(tap, METRIC_ID.LUFS_I);
    const dr    = v(tap, METRIC_ID.DR);
    const rms   = v(tap, METRIC_ID.RMS);
    const peakAt = v(tap, METRIC_ID.PEAK_AT);
    const clips = tap === 's' ? v('s', METRIC_ID.CLIPS_GE2) : v('o', METRIC_ID.CLIPS_OUT_X1);

    return { label, tpDb, tpLin, sp, gain, lra, lufs, dr, rms, peakAt, clips };
  });

  // Column widths (fixed for alignment)
  const COL = { label: 8, tp: 22, sp: 8, gain: 7, lra: 5, lufs: 7, dr: 4, rms: 7, peakAt: 10, clips: 6 };

  const pad = (s, w) => String(s).padStart(w);
  const header = [
    pad('', COL.label), pad('TP', COL.tp), pad('SP', COL.sp), pad('Gain', COL.gain),
    pad('LRA', COL.lra), pad('LUFS-I', COL.lufs), pad('DR', COL.dr), pad('RMS', COL.rms),
    pad('Peak@', COL.peakAt), pad('Clips', COL.clips),
  ].join('  ');

  const lines = [
    `AuraEngine v${ver} / Loudness + True Peak / ${ts}`,
    `File: ${fileBase} — ${srcRate} Hz, ${ch}, ${bits}-bit`,
    '',
    header,
  ];

  for (const r of rows) {
    const row = [
      pad(r.label + ':', COL.label),
      pad(fmtTP(r.tpLin, r.tpDb), COL.tp),
      pad(fmtNum(METRIC_ID.SP, r.sp, 4), COL.sp),
      pad(fmtNum(METRIC_ID.GAIN_18LUFS, r.gain, 2), COL.gain),
      pad(fmtNum(METRIC_ID.LRA, r.lra, 2), COL.lra),
      pad(fmtNum(METRIC_ID.LUFS_I, r.lufs, 2), COL.lufs),
      pad(fmtNum(METRIC_ID.DR, r.dr, 0), COL.dr),
      pad(fmtNum(METRIC_ID.RMS, r.rms, 2), COL.rms),
      pad(fmtPeakAt(r.peakAt), COL.peakAt),
      pad(fmtNum(METRIC_ID.CLIPS_GE2, r.clips, 0), COL.clips),
    ].join('  ');
    lines.push(row);
  }

  lines.push('');
  const outRateKhz = (outRate / 1000).toFixed(1);
  lines.push(
    `  TP: BS.1770 (libebur128): 4× oversampling below 96 kHz, 2× below 192 kHz,`,
    `      sample peak at or above 192 kHz (output is ${outRateKhz} kHz).`,
    `  Gain: ReplayGain Track Gain targeting −18 LUFS (EBU R128).`,
    `  DR: crest factor metric (TT DR), not "dynamics". Grows after conversion because`,
    `      reconstruction raises the inter-sample peak, not because quiet passages get quieter.`,
    `  LRA: EBU 3342, nearest-rank percentiles (P95-P10).`,
  );

  return lines.join('\n');
}

// ── Block B: foo_dr_meter ──────────────────────────────────────────────────
function buildFooDr(store) {
  const ver  = APP_VERSION();
  const date = fmtDateLocal();
  const tn   = store.trackName || 'Unknown Track';
  const durS = store.durationS || 0;
  const dur  = fmtDur(durS);

  const srcRate = store._srcRate || 44100;
  const outRate = store._outRate || 352800;
  const bits    = store._bits || 24;
  // A stream: its own codec (the first word of its line, "MP3 · 128 kbps · …").
  const codec   = (store._radioNow?.tech || '').split(' · ')[0] || store._codec || 'FLAC';
  const chain   = store.chain?.tokens || '';

  const sDr   = getVal(store, 's', METRIC_ID.DR);
  const sTp   = getVal(store, 's', METRIC_ID.TP_EBUR);
  const sRms  = getVal(store, 's', METRIC_ID.RMS);

  const oDr   = getVal(store, 'o', METRIC_ID.DR);
  const oTp   = getVal(store, 'o', METRIC_ID.TP_EBUR);
  const oRms  = getVal(store, 'o', METRIC_ID.RMS);

  const fmtDR = v => isFinite(v) ? `DR ${fmtNum(METRIC_ID.DR, v, 0)}` : 'DR ?';
  const fmtDbSign = v => !isFinite(v) ? '—' : (v >= 0 ? '+' : '') + fmtNum(0, v, 2);

  const SEP = '-'.repeat(80);

  const srcLabel = `${tn} (source)`;
  const outLabel = `${tn} (output · ${chain || '?'})`;
  const maxLen = Math.max(srcLabel.length, outLabel.length);
  const pad = (s, w) => s.padEnd(w);

  const lines = [
    `AuraEngine v${ver} / DR Meter / Log date: ${date}`,
    '',
    `Analyzed: ${store._artist || 'Unknown Artist'} / ${store._album || 'Unknown Album'}`,
    '',
    '         DR      Peak         RMS       Duration   Track',
    SEP,
  ];

  if (isFinite(sDr)) {
    lines.push(
      `${fmtDR(sDr).padEnd(9)} ${fmtDbSign(sTp).padStart(7)}   ${fmtNum(METRIC_ID.RMS, sRms, 2).padStart(7)}       ${dur.padEnd(10)} ${srcLabel}`
    );
  }
  if (isFinite(oDr)) {
    lines.push(
      `${fmtDR(oDr).padEnd(9)} ${fmtDbSign(oTp).padStart(7)}   ${fmtNum(METRIC_ID.RMS, oRms, 2).padStart(7)}       ${dur.padEnd(10)} ${outLabel}`
    );
  }

  lines.push('');
  lines.push(`Official DR value:  ${fmtNum(METRIC_ID.DR, sDr, 0)} (source) · ${fmtNum(METRIC_ID.DR, oDr, 0)} (output)`);
  lines.push(`Samplerate:         ${srcRate} Hz (source) · ${outRate} Hz (output)`);
  lines.push('Channels:               2');
  lines.push(`Bits per sample:       ${bits}`);
  lines.push(`Codec:               ${codec}`);
  lines.push('');
  lines.push('Note: Peak = sample peak. RMS = top-20% loudest 3-second blocks, √2-normalised.');
  lines.push('      Partial last block: discarded (matches foo_dr_meter 1.0.x behaviour).');

  return lines.join('\n');
}

// ── Block C: LAB REPORT ───────────────────────────────────────────────────
// Two lines exactly matching ASR post #60 format.
function buildLabReport(store) {
  const tn    = store.trackName || 'Unknown Track';
  const chain = store.chain?.tokens || '?';
  const outRate = store._outRate || 352800;
  const outKhz  = (outRate / 1000).toFixed(1);

  const sLufs = getVal(store, 's', METRIC_ID.LUFS_I);
  const sLra  = getVal(store, 's', METRIC_ID.LRA);
  const sTp   = getVal(store, 's', METRIC_ID.TP_EBUR);

  const oLufs = getVal(store, 'o', METRIC_ID.LUFS_I);
  const oLra  = getVal(store, 'o', METRIC_ID.LRA);
  const oTp   = getVal(store, 'o', METRIC_ID.TP_EBUR);

  const fmtLufs = v => isFinite(v) ? fmtNum(0, v, 2) : '?';
  const fmtLra  = v => isFinite(v) ? fmtNum(0, v, 2) : '?';
  const fmtTp   = v => isFinite(v) ? (v >= 0 ? '+' : '') + fmtNum(0, v, 2) + ' dBTP' : '?';

  const line1 = `${tn}: I ${fmtLufs(sLufs)} / LRA ${fmtLra(sLra)} / TP ${fmtTp(sTp)}  [Source]`;
  const line2 = `${' '.repeat(tn.length + 2)}I ${fmtLufs(oLufs)} / LRA ${fmtLra(oLra)} / TP ${fmtTp(oTp)}  [Output · ${chain} · ${outKhz}kHz]`;

  return line1 + '\n' + line2;
}

// ── CSV export ─────────────────────────────────────────────────────────────
// Time-series: one row per 100 ms. SPEC §8.4
export function buildCSV(store) {
  if (store._stream) return buildStreamCSV(store);
  const sProv = (store.metrics.sProv || new Uint8Array(N_METRICS));
  const oProv = (store.metrics.oProv || new Uint8Array(N_METRICS));

  // Column provenance suffixes.
  // ERRATA E1: every number computed on the real S signal is [M], never [A].
  // S-tap columns always carry [M]; O-tap columns use the actual prov code.
  const oProvSuffix = (p) => {
    const n = ['', '[A]', '[F]', '[M]', '[=]', '[?]', '[M-stale]', '···'];
    return n[p] || '';
  };

  const oLufsProv = oProvSuffix(oProv[METRIC_ID.LUFS_S_LIVE]);

  const header = [
    'time_s',
    'lufs_s_S[M]',
    `lufs_s_O${oLufsProv}`,
    'lufs_m_S[M]',
    `lufs_m_O${oProvSuffix(oProv[METRIC_ID.LUFS_M_LIVE])}`,
    'tp_S[M]',
    `tp_O${oProvSuffix(oProv[METRIC_ID.TP_EBUR])}`,
  ].join(',');

  const rows = [header];
  const n = (store.series.lufs_s_s || store.series.lufs_s_o)?.length || 0;
  const lufs_s_s = store.series.lufs_s_s;
  const lufs_m_s = store.series.lufs_m_s;
  // O: the O pass's whole-track series (on S's grid); none while it runs.
  const lufs_s_o = store.series.lufs_s_o_whole || [];
  const lufs_m_o = store.series.lufs_m_o_whole || [];

  const count = Math.max(lufs_s_s?.length || 0, lufs_s_o.length);
  for (let i = 0; i < count; i++) {
    // Index i: the window ending at (i + 4) × 100 ms.
    const t = ((i + 4) * 0.1).toFixed(1);
    const ss = lufs_s_s ? (lufs_s_s[i] ?? '') : '';
    const ms = lufs_m_s ? (lufs_m_s[i] ?? '') : '';
    const so = Number.isFinite(lufs_s_o[i]) ? lufs_s_o[i].toFixed(2) : '';
    const mo = Number.isFinite(lufs_m_o[i]) ? lufs_m_o[i].toFixed(2) : '';
    // TP: scalar per track, not time-series; repeat track value in each row
    const tps = isFinite(getVal(store, 's', METRIC_ID.TP_EBUR)) ? getVal(store, 's', METRIC_ID.TP_EBUR).toFixed(2) : '';
    const tpo = isFinite(getVal(store, 'o', METRIC_ID.TP_EBUR)) ? getVal(store, 'o', METRIC_ID.TP_EBUR).toFixed(2) : '';
    // Number.isFinite: a missing value here is '' (and isFinite('') is true).
    rows.push([t, Number.isFinite(ss) ? ss.toFixed(2) : '', so, Number.isFinite(ms) ? ms.toFixed(2) : '', mo, tps, tpo].join(','));
  }

  return rows.join('\n');
}

/**
 * A stream's: its loudness as the page keeps it (the last 30 minutes, on the
 * stream's clock), a row per 100 ms where S or O has a value — both measured
 * live; TP is the song's at that time and its title closes the row (songs of
 * this station only: the list goes on across stations, each on its own clock).
 */
function buildStreamCSV(store) {
  const s = store.series || {};
  const rows = new Map();   // tenths of a second → [S LUFS-S, O LUFS-S, S LUFS-M, O LUFS-M]
  const put = (pts, j) => {
    for (const p of pts || []) {
      if (!Number.isFinite(p.value)) continue;
      const k = Math.round(p.time_s * 10);
      let r = rows.get(k);
      if (!r) rows.set(k, (r = [NaN, NaN, NaN, NaN]));
      r[j] = p.value;
    }
  };
  put(s.lufs_s_s_live, 0); put(s.lufs_s_o, 1); put(s.lufs_m_s_live, 2); put(s.lufs_m_o, 3);
  const tot = store._streamTotals;
  const url = tot?.song?.url;
  const songs = url
    ? [tot.song, ...(tot.songs || []).filter(x => x?.url === url)].filter(x => Number.isFinite(x?.startS)).sort((a, b) => a.startS - b.startS)
    : [];
  const songAt = (t) => { let hit = null; for (const x of songs) if (x.startS <= t + 1e-6) hit = x; return hit; };
  const num = (v) => (Number.isFinite(v) ? v.toFixed(2) : '');
  const text = (v) => (/[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v);
  const out = ['time_s,lufs_s_S[M],lufs_s_O[M],lufs_m_S[M],lufs_m_O[M],tp_S[M],tp_O[M],song'];
  for (const k of [...rows.keys()].sort((a, b) => a - b)) {
    const [ss, so, ms, mo] = rows.get(k), t = k / 10, song = songAt(t);
    out.push([t.toFixed(1), num(ss), num(so), num(ms), num(mo), num(song?.s?.tp), num(song?.o?.tp), text(song?.title || '')].join(','));
  }
  return out.join('\n');
}

// ── TSV export ─────────────────────────────────────────────────────────────
// Flat metrics table. SPEC §8.5
function buildTSV(store) {
  const header = ['Metric', 'Unit', 'S', 'B', 'O', 'Prov_O'].join('\t');
  const rows = [header];

  for (let id = 0; id < N_METRICS; id++) {
    const sV = getVal(store, 's', id);
    const bV = getVal(store, 'b', id);
    const oV = getVal(store, 'o', id);
    const oP = getProv(store, 'o', id);
    rows.push([
      METRIC_NAMES[id] || `metric_${id}`,
      METRIC_UNITS[id] || '',
      isFinite(sV) ? fmtNum(id, sV) : '—',
      isFinite(bV) ? fmtNum(id, bV) : '—',
      isFinite(oV) ? fmtNum(id, oV) : '—',
      PROV_NAMES[oP] || '',
    ].join('\t'));
  }

  return rows.join('\n');
}

// ── PNG export ─────────────────────────────────────────────────────────────
// A screenshot of the window as it looks (WebView2 draws the page: the
// numbers, the graph shown, everything), cropped to the graph when asked,
// with a line of what was measured under it. SPEC §8.2.
// (It used to glue the first canvas of every panel together, hidden tabs'
// too: not what the window showed.)
const FOOTER_H = 28;   // the footer line, CSS px

/** The page as the screen shows it: an ImageBitmap, or throws. */
async function capturePage() {
  const invoke = window.__TAURI__?.tauri?.invoke;
  if (!invoke) throw new Error('no app to take the picture');
  // Nothing floating over the page while it is taken.
  document.body.classList.add('an-capturing');
  try {
    await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
    const b64 = await invoke('an_window_png');
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return await createImageBitmap(new Blob([bytes], { type: 'image/png' }));
  } finally {
    document.body.classList.remove('an-capturing');
  }
}

/**
 * The PNG to save: the window, or the part of it `crop` covers (a DOMRect in
 * CSS px), and the footer line.
 */
async function buildPNG(store, crop) {
  const img = await capturePage();
  const k = img.width / Math.max(1, window.innerWidth);   // picture px per CSS px
  const sx = crop ? Math.max(0, Math.round(crop.left * k)) : 0;
  const sy = crop ? Math.max(0, Math.round(crop.top * k)) : 0;
  const sw = crop ? Math.min(img.width - sx, Math.round(crop.width * k)) : img.width;
  const sh = crop ? Math.min(img.height - sy, Math.round(crop.height * k)) : img.height;
  const fh = Math.round(FOOTER_H * k);
  const oc = new OffscreenCanvas(sw, sh + fh);
  const ctx = oc.getContext('2d');
  ctx.fillStyle = '#0b1928'; // the window's background (its rounded corners are see-through)
  ctx.fillRect(0, 0, sw, sh + fh);
  ctx.drawImage(img, sx, sy, sw, sh, 0, 0, sw, sh);
  img.close?.();

  const ver    = APP_VERSION();
  const ts     = new Date().toLocaleString('sv-SE').replace(',', '');
  const chain  = store.chain?.tokens || '';
  const format = `${store._srcRate || 44100} Hz, ${store._bits || 24}-bit`;
  const footer = `AuraEngine v${ver}  ·  ${store.trackName || '?'}  ·  ${format}  ·  Chain: ${chain || '—'}  ·  ${ts}`;
  ctx.fillStyle = '#7dd3fc'; // --an-axis-label
  ctx.font = `${Math.round(11 * k)}px "Cascadia Mono", "Consolas", monospace`;
  ctx.textBaseline = 'middle';
  ctx.fillText(footer, Math.round(8 * k), sh + fh / 2, sw - Math.round(16 * k));
  return oc.convertToBlob({ type: 'image/png' });
}

// ── File save ─────────────────────────────────────────────────────────────
// Returns the path saved to (null: the dialog was cancelled).

/** `fn` in the folder `dirFn` names (Pictures, Documents), or `fn` alone. */
async function defaultPath(fn, dirFn) {
  try {
    const p = window.__TAURI__?.path;
    if (p && p[dirFn]) return await p.join(await p[dirFn](), fn);
  } catch { /* the dialog's own folder */ }
  return fn;
}

async function saveText(filename, content, ext, mime) {
  const fn = `${filename}.${ext}`;
  if (window.__TAURI__?.dialog) {
    const saved = await window.__TAURI__.dialog.save({
      defaultPath: await defaultPath(fn, 'documentDir'),
      filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
    });
    if (!saved) return null;
    // writeTextFile takes the text itself. (writeFile with bytes took them
    // for its options and wrote an empty file.) The UTF-8 BOM first: Excel
    // in a Russian locale read the dashes, the minus and the ··· as ANSI.
    await window.__TAURI__.fs.writeTextFile(saved, '﻿' + content);
    return saved;
  }
  // Browser download fallback (stand preview)
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob(['﻿' + content], { type: mime }));
  a.download = fn;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  return fn;
}

async function savePNG(filename, blob) {
  const fn = `${filename}.png`;
  if (window.__TAURI__?.dialog) {
    const saved = await window.__TAURI__.dialog.save({
      defaultPath: await defaultPath(fn, 'pictureDir'),
      filters: [{ name: 'PNG', extensions: ['png'] }],
    });
    if (!saved) return null;
    const arr = await blob.arrayBuffer();
    await window.__TAURI__.fs.writeBinaryFile(saved, new Uint8Array(arr));
    return saved;
  }
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = fn;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  return fn;
}

// ── Clipboard ─────────────────────────────────────────────────────────────
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fallback: execCommand
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); return true; } catch { return false; }
    finally { ta.remove(); }
  }
}

// ── Filename helper ────────────────────────────────────────────────────────
function exportFilename(store) {
  const name = (store.trackName || 'track').replace(/[/\\:*?"<>|]/g, '_').slice(0, 40);
  const chain8 = store.chainHex || '00000000';
  const ts = new Date().toISOString().slice(0, 19).replace(/[T:]/g, '-');
  return `aura-${name}-${chain8}-${ts}`;
}

// ── Popover component ─────────────────────────────────────────────────────
// The Export ▾ menu under its button (`anchor`). ctx: { store, getGraphRect }
// — getGraphRect() is the graph shown (its DOMRect), for "the graph shown".
// It used to be appended into the toolbar with no styles at all and placed
// by a hidden button of its own: it opened out of sight, so Export seemed to
// do nothing.
export function createExportPopover(anchor, ctx) {
  const { store } = ctx;

  let popover = null;

  function buildPopoverHTML() {
    // SVGs inline per requirement (no emoji)
    const iconCopy = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><rect x="5" y="3" width="8" height="10" rx="1"/><path d="M3 2h7v1H4v9H3V2z"/></svg>`;
    const iconImg  = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><rect x="2" y="3" width="12" height="10" rx="1"/><circle cx="6" cy="7" r="1.5"/><path d="m2 10 3.5-3.5L9 10l2.5-2 2.5 2"/></svg>`;
    const iconCSV  = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M3 2h7l3 3v9H3V2z"/><path d="M10 2v3h3"/><path d="M5 9h6M5 12h4"/></svg>`;

    return `
<div class="an-export-pop" role="menu" aria-label="Export">
  <button class="an-export-item" role="menuitem" data-act="copy-forum" data-tip="The lab report line, the foo_truepeak and the foo_dr blocks, as text for a forum post">
    ${iconCopy}<span>Copy text (forum)</span>
  </button>
  <button class="an-export-item" role="menuitem" data-act="png-window" data-tip="A picture of this window as it looks now, with a line of what was measured (Ctrl+Shift+S)">
    ${iconImg}<span>Save PNG — the window</span>
  </button>
  <button class="an-export-item" role="menuitem" data-act="png-graph" data-tip="A picture of the graph shown, with a line of what was measured">
    ${iconImg}<span>Save PNG — the graph</span>
  </button>
  <div class="an-export-sep"></div>
  <button class="an-export-item" role="menuitem" data-act="csv" data-tip="Loudness over time, S and O, every 100 ms">
    ${iconCSV}<span>Save CSV — loudness over time</span>
  </button>
  <button class="an-export-item" role="menuitem" data-act="tsv" data-tip="The metrics table: S, B and O">
    ${iconCSV}<span>Save TSV — the metrics</span>
  </button>
</div>`;
  }

  /** Under the button, its right edge on the button's, whole inside the window. */
  function place() {
    const br = anchor.getBoundingClientRect();
    const pr = popover.getBoundingClientRect();
    let left = br.right - pr.width;
    let top = br.bottom + 4;
    if (top + pr.height > window.innerHeight - 6) top = br.top - 4 - pr.height;
    left = Math.max(6, Math.min(left, window.innerWidth - pr.width - 6));
    top = Math.max(6, Math.min(top, window.innerHeight - pr.height - 6));
    popover.style.left = `${Math.round(left)}px`;
    popover.style.top = `${Math.round(top)}px`;
  }

  function open() {
    if (popover) { close(); return; }
    const div = document.createElement('div');
    div.innerHTML = buildPopoverHTML();
    popover = div.firstElementChild;
    document.body.appendChild(popover);
    place();
    anchor.setAttribute('aria-expanded', 'true');
    popover.addEventListener('click', handleAction);
    window.addEventListener('resize', close);
  }

  function close() {
    if (!popover) return;
    popover.remove();
    popover = null;
    anchor.setAttribute('aria-expanded', 'false');
    window.removeEventListener('resize', close);
  }

  // A press outside the menu only puts it away, and Esc does (popups.js);
  // a press on the Export button itself is its click's to toggle.
  addPopup({ isOpen: () => !!popover, inside: t => !!popover?.contains(t) || anchor.contains(t), close });

  const copyAll = () => copyText([buildLabReport(store), buildFooTruepeak(store), buildFooDr(store)].join('\n\n'));

  /** Save a picture of the window, or of `crop` (a DOMRect) in it; the path
   *  saved, or null. One at a time: a second press while the first is taken
   *  or its dialog is up does nothing (it opened a second dialog). */
  let shooting = false;
  async function screenshot(crop) {
    if (shooting) return null;
    shooting = true;
    try {
      close();
      const blob = await buildPNG(store, crop || null);
      return await savePNG(exportFilename(store), blob);
    } finally {
      shooting = false;
    }
  }

  async function handleAction(e) {
    const btn = e.target.closest('[data-act]');
    if (!btn) return;
    const act = btn.dataset.act;
    const fn  = exportFilename(store);
    let saved = null;
    try {
      switch (act) {
        case 'copy-forum':
          await copyAll();
          flashBtn(btn, 'Copied');
          return;
        case 'png-window':
          saved = await screenshot(null);
          break;
        case 'png-graph':
          saved = await screenshot(ctx.getGraphRect?.() || null);
          break;
        case 'csv':
          close();
          saved = await saveText(fn, buildCSV(store), 'csv', 'text/csv');
          break;
        case 'tsv':
          close();
          saved = await saveText(fn, buildTSV(store), 'tsv', 'text/tab-separated-values');
          break;
      }
      if (saved) flashEl(anchor, 'Saved');
    } catch (err) {
      console.error('[export]', act, err);
      close();
      flashEl(anchor, 'Failed');
    }
  }

  function flashBtn(btn, msg) {
    const span = btn.querySelector('span') || btn;
    const orig = span.textContent;
    span.textContent = msg;
    setTimeout(() => { span.textContent = orig; }, 1200);
  }

  // ── Per-block copy (the card strip under the metrics) ──
  function copyBlock(block) {
    switch (block) {
      case 'truepeak': return copyText(buildFooTruepeak(store));
      case 'dr':       return copyText(buildFooDr(store));
      case 'lab':      return copyText(buildLabReport(store));
      case 'all':      return copyAll();
    }
    return Promise.resolve(false);
  }

  anchor.setAttribute('aria-haspopup', 'menu');
  anchor.setAttribute('aria-expanded', 'false');
  anchor.addEventListener('click', open);

  // ── Keyboard: Ctrl+Shift+S saves the window's picture; Ctrl+C with nothing
  //    selected copies the forum text. (The single keys P and C it had are
  //    the waveform's and the stereo graph's layer keys.)
  document.addEventListener('keydown', e => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey || e.repeat) return;
    if (e.shiftKey && e.code === 'KeyS') {
      e.preventDefault();
      screenshot(null)
        .then(p => { if (p) flashEl(anchor, 'Saved'); })
        .catch(err => { console.error('[export png]', err); flashEl(anchor, 'Failed'); });
    } else if (!e.shiftKey && e.code === 'KeyC') {
      const sel = window.getSelection && window.getSelection();
      if (sel && !sel.isCollapsed) return;   // the text selected is the copy
      e.preventDefault();
      copyAll();
    }
  });

  return {
    open,
    close,
    copyBlock,
    screenshot,
    buildFooTruepeak: (taps) => buildFooTruepeak(store, taps),
    buildFooDr:       () => buildFooDr(store),
    buildLabReport:   () => buildLabReport(store),
    buildCSV:         () => buildCSV(store),
    buildTSV:         () => buildTSV(store),
  };
}

/**
 * Show `msg` on a button for a moment: in its `.an-btn-label` when it has
 * one (its icon stays), else in place of its content.
 */
export function flashEl(btn, msg, ms = 1400) {
  if (!btn) return;
  const label = btn.querySelector('.an-btn-label');
  clearTimeout(btn._flashT);
  if (label) {
    if (btn._flashOrig == null) btn._flashOrig = label.textContent;
    label.textContent = msg;
    btn._flashT = setTimeout(() => { label.textContent = btn._flashOrig; btn._flashOrig = null; }, ms);
  } else {
    if (btn._flashOrig == null) btn._flashOrig = btn.innerHTML;
    btn.textContent = msg;
    btn._flashT = setTimeout(() => { btn.innerHTML = btn._flashOrig; btn._flashOrig = null; }, ms);
  }
}
