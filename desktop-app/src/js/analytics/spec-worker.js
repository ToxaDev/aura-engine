/**
 * spec-worker.js — colours spectrogram columns off the page's thread.
 *
 * Message:
 *   { type: 'colorize', id, data: Uint8Array, nCols, nBins,
 *     srcFloor, srcRange,   // the bytes' scale: v = 0 → srcFloor dB, 255 → srcFloor + srcRange
 *     lo, hi,               // the display range in dB: lo → bottom of the colour map, hi → top
 *     colormap }            // 'inferno' | 'magma' | 'viridis' | 'grayscale' | 'diverging' (Δ views)
 *   data layout: column-major, nCols columns × nBins bands (index = col*nBins + band)
 * Reply:
 *   { type: 'colorized', id, imageData }   // width nCols, height nBins; band 0 = bottom row
 */

'use strict';

// ── Colour maps ──────────────────────────────────────────────────────────────
// matplotlib's perceptually uniform maps from their published degree-6
// polynomial fits (error < 1 % of full scale).

const POLY = {
  inferno: [
    [0.0002189403691192265, 0.001651004631001012, -0.01948089843709184],
    [0.1065134194856116, 0.5639564367884091, 3.932712388889277],
    [11.60249308247187, -3.972853965665698, -15.9423941062914],
    [-41.70399613139459, 17.43639888205313, 44.35414519872813],
    [77.162935699427, -33.40235894210092, -81.80730925738993],
    [-71.31942824499214, 32.62606426397723, 73.20951985803202],
    [25.13112622477341, -12.24266895238567, -23.07032500287172],
  ],
  magma: [
    [-0.002136485053939582, -0.000749655052795221, -0.005386127855323933],
    [0.2516605407371642, 0.6775232436837668, 2.494026599312351],
    [8.353717279216625, -3.577719514958484, 0.3144679030132573],
    [-27.66873308576866, 14.26473078096533, -13.64921318813922],
    [52.17613981234068, -27.94360607168351, 12.94416944238394],
    [-50.76852536473588, 29.04658282127291, 4.23415299384598],
    [18.65570506591883, -11.48977351997711, -5.601961508734096],
  ],
  viridis: [
    [0.2777273272234177, 0.005407344544966578, 0.3340998053353061],
    [0.1050930431085774, 1.404613529898575, 1.384590162594685],
    [-0.3308618287255563, 0.214847559468213, 0.09509516302823659],
    [-4.634230498983486, -5.799100973351585, -19.33244095627987],
    [6.228269936347081, 14.17993336680509, 56.69055260068105],
    [4.776384997670288, -13.74514537774601, -65.35303263337234],
    [-5.435455855934631, 4.645852612178535, 26.3124352495832],
  ],
};

/** 256 RGBA entries packed as little-endian Uint32 (ImageData byte order). */
function buildLut(name) {
  const lut = new Uint32Array(256);
  const c = POLY[name];
  for (let i = 0; i < 256; i++) {
    const t = i / 255;
    const rgb = [i, i, i];
    if (c) {
      for (let ch = 0; ch < 3; ch++) {
        let v = 0;
        for (let k = c.length - 1; k >= 0; k--) v = v * t + c[k][ch];
        rgb[ch] = Math.round(Math.max(0, Math.min(1, v)) * 255);
      }
    }
    lut[i] = (255 << 24 | rgb[2] << 16 | rgb[1] << 8 | rgb[0]) >>> 0;
  }
  return lut;
}

/** A map through evenly spaced colour stops ('#rrggbb'), straight lines between. */
function stopsLut(stops) {
  const rgb = stops.map(s => [1, 3, 5].map(i => parseInt(s.slice(i, i + 2), 16)));
  const lut = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    const x = (i / 255) * (rgb.length - 1);
    const k = Math.min(rgb.length - 2, Math.floor(x));
    const t = x - k;
    const c = [0, 1, 2].map(ch => Math.round(rgb[k][ch] * (1 - t) + rgb[k + 1][ch] * t));
    lut[i] = (255 << 24 | c[2] << 16 | c[1] << 8 | c[0]) >>> 0;
  }
  return lut;
}

const LUTS = {
  inferno: buildLut('inferno'),
  magma: buildLut('magma'),
  viridis: buildLut('viridis'),
  grayscale: buildLut('grayscale'),
  // The Δ views: quieter blue, unchanged black, louder red (spectrogram-view.js DELTA_STOPS).
  diverging: stopsLut(['#9ec5ff', '#2f6fd0', '#0a0a0a', '#d0452f', '#ffb49e']),
};

/** The byte → colour table for one display range. */
function rangeLut(srcFloor, srcRange, lo, hi, colormap) {
  const base = LUTS[colormap] || LUTS.inferno;
  const out = new Uint32Array(256);
  const span = Math.max(1e-6, hi - lo);
  for (let v = 0; v < 256; v++) {
    const db = srcFloor + (v / 255) * srcRange;
    const t = Math.max(0, Math.min(1, (db - lo) / span));
    out[v] = base[Math.round(t * 255)];
  }
  return out;
}

function colorize(data, nCols, nBins, lut) {
  const rgba = new Uint8ClampedArray(nCols * nBins * 4);
  const px = new Uint32Array(rgba.buffer);
  for (let col = 0; col < nCols; col++) {
    const base = col * nBins;
    for (let bin = 0; bin < nBins; bin++) {
      px[(nBins - 1 - bin) * nCols + col] = lut[data[base + bin]];
    }
  }
  return new ImageData(rgba, nCols, nBins);
}

self.addEventListener('message', (evt) => {
  const m = evt.data;
  if (!m || m.type !== 'colorize') return;
  const lut = rangeLut(m.srcFloor, m.srcRange, m.lo, m.hi, m.colormap);
  const imageData = colorize(m.data, m.nCols, m.nBins, lut);
  self.postMessage({ type: 'colorized', id: m.id, imageData }, [imageData.data.buffer]);
});
