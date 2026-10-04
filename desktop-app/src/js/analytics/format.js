/**
 * format.js — Display formatting helpers for the analyzer window.
 *
 * Provides time formatting, Hz formatting, and metric row label strings.
 * These are pure functions with no side effects.
 */

/**
 * Format a duration in seconds as m:ss.d (one decimal second).
 * e.g. 288.6 → "4:48.6"
 * @param {number} s  Seconds (may be fractional)
 * @returns {string}
 */
export function formatTime(s) {
  if (!isFinite(s) || s < 0) return '--:--';
  const m   = Math.floor(s / 60);
  const sec = s - m * 60;
  return `${m}:${sec < 10 ? '0' : ''}${sec.toFixed(1)}`;
}

/**
 * Format a duration in seconds as m:ss (integer seconds, no decimal).
 * @param {number} s
 * @returns {string}
 */
export function formatTimeInt(s) {
  if (!isFinite(s) || s < 0) return '--:--';
  const tot = Math.round(s);
  const m   = Math.floor(tot / 60);
  const sec = tot % 60;
  return `${m}:${sec < 10 ? '0' : ''}${sec}`;
}

/**
 * Format a frequency value for display.
 * < 1000 Hz → "123 Hz", >= 1000 Hz → "12.3 kHz"
 * @param {number} hz
 * @returns {string}
 */
export function formatHz(hz) {
  if (!isFinite(hz)) return '--';
  if (hz < 1000) return `${hz.toFixed(0)} Hz`;
  return `${(hz / 1000).toFixed(hz < 10000 ? 2 : 1)} kHz`;
}

/**
 * Display name for each metric (used in the row label column).
 * Indexed by METRIC_ID.
 * @type {string[]}
 */
export const METRIC_LABEL = [
  'LUFS-I',           // 0
  'LUFS-S (live)',    // 1
  'LUFS-M (live)',    // 2
  'LRA',              // 3
  'TP (BS.1770)',     // 4
  'TP (engine 4×)',   // 5
  'SP',               // 6
  'Peak@',            // 7
  'DR (crest)',       // 8
  'RMS (whole)',      // 9
  'RMS (top-20%)',    // 10
  'Gain (−18 LUFS)',  // 11
  'Clips src ≥2',     // 12
  '↳ Clips src ≥17',  // 13
  'Clips out |x|>1',  // 14
  '↳ TP>0dBTP events',// 15
  'DC offset',        // 16
  'Ultrasonic peak',  // 17
  'Ultrasonic RMS',   // 18
  '↳ HP spike count', // 19
  'Infrasonic RMS',   // 20
  '↳ SUB removed',    // 21
  'PLR',              // 22
  'Stereo corr.',     // 23
  'Eff. bandwidth',   // 24
  'Coverage',         // 25
];

/**
 * Tooltip texts for metric row labels (maps METRIC_ID to text).
 * User-visible texts.
 * @type {string[]}
 */
export const METRIC_TOOLTIP = [
  // 0 LUFS_I
  'Integrated loudness (BS.1770-4/-5). Absolute gate −70 LUFS, relative gate −10 LU. Window 400 ms, step 100 ms.',
  // 1 LUFS_S_LIVE
  'Short-term loudness (BS.1770). 3-second sliding window, updated every 100 ms.',
  // 2 LUFS_M_LIVE
  'Momentary loudness (BS.1770). 400 ms sliding window, updated every 100 ms.',
  // 3 LRA
  'Loudness Range (EBU 3342). Short-term windows 3 s, step 100 ms. Absolute gate −70, relative −20 LU. P95 − P10, nearest-rank percentile (reproduces published ASR measurements).',
  // 4 TP_EBUR
  'True Peak, BS.1770 / libebur128 algorithm. 4× oversampling below 96 kHz, 2× below 192 kHz, sample peak at or above 192 kHz. Matches foo_truepeak scanner output.',
  // 5 TP_ENGINE
  "True Peak, AuraEngine's own 4× Lanczos interpolator. May differ from BS.1770 value by ±0.02 dBTP.",
  // 6 SP
  'Sample peak (linear, not in dBFS). Value of 1.0 = 0 dBFS.',
  // 7 PEAK_AT
  'Position in track where the largest sample peak occurs.',
  // 8 DR
  'TT DR (foo_dr_meter compatible). Top-20% of 3-second RMS blocks (×√2), second-largest sample peak. Grows after AuraEngine conversion because the reconstructed output peak is higher than the source sample peak — dynamics (LRA) are not changed.',
  // 9 RMS
  'RMS level of the full track (dBFS).',
  // 10 RMS_TOP20
  'RMS of the loudest top-20% of 3-second blocks.',
  // 11 GAIN_18LUFS
  'ReplayGain Track Gain targeting −18 LUFS (EBU R128). Apply this gain to reach −18 LUFS integrated.',
  // 12 CLIPS_GE2
  'Runs of ≥2 flat samples (clipping runs). These are detected but not necessarily repaired.',
  // 13 CLIPS_GE17
  'Runs of ≥17 clipped samples — declip candidate threshold (MIN_RUN_GATE). These are the clips the DC stage attempts to repair.',
  // 14 CLIPS_OUT_X1
  'Output samples where |x| > 1.0 (true clip on the digital output rail).',
  // 15 TP_OVER0_EVENTS
  'True Peak events above 0 dBTP on the output. Each event is one waveform peak overshoot.',
  // 16 DC_OFFSET
  'Mean DC offset as a percentage of full scale.',
  // 17 ULTRA_PEAK
  'Peak energy above 24 kHz (dBFS). Indicates ultrasonic content or artefacts.',
  // 18 ULTRA_RMS
  'RMS energy above 24 kHz (dBFS).',
  // 19 ULTRA_EVENTS
  'Number of ultrasonic spike events in the output (Hybrid-Phase transients).',
  // 20 INFRA_RMS
  'RMS energy below 20 Hz (dBFS). Indicates infrasonic content.',
  // 21 SUB_REMOVED
  'Energy removed by the Subsonic filter (dBFS of the removed signal).',
  // 22 PLR
  'Peak-to-Loudness Ratio (PLR = TP − LUFS-I, dB). Measures the loudness–peak relationship.',
  // 23 STEREO_CORR
  'Stereo correlation coefficient. +1 = mono, 0 = uncorrelated, −1 = out of phase.',
  // 24 EFF_BW
  'Effective bandwidth: highest frequency bin with energy above −60 dBr of the passband peak.',
  // 25 COVERAGE_O
  'The share of the track heard so far (a part played twice counts twice). At 100%, the O numbers are fully measured.',
];

/**
 * Which taps are valid for each metric.
 * Each element is a bitmask: bit 0 = S, bit 1 = B, bit 2 = O.
 * @type {number[]}
 */
export const METRIC_TAPS = [
  0b111, // 0  LUFS_I         S B O
  0b111, // 1  LUFS_S_LIVE    S B O
  0b111, // 2  LUFS_M_LIVE    S B O
  0b111, // 3  LRA            S B O
  0b111, // 4  TP_EBUR        S B O
  0b111, // 5  TP_ENGINE      S B O
  0b111, // 6  SP             S B O
  0b111, // 7  PEAK_AT        S B O
  0b111, // 8  DR             S B O
  0b111, // 9  RMS            S B O
  0b111, // 10 RMS_TOP20      S B O
  0b111, // 11 GAIN_18LUFS    S B O
  0b011, // 12 CLIPS_GE2      S B (source clips; O has CLIPS_OUT_X1)
  0b011, // 13 CLIPS_GE17     S B
  0b100, // 14 CLIPS_OUT_X1   O
  0b100, // 15 TP_OVER0       O
  0b011, // 16 DC_OFFSET      S B
  0b111, // 17 ULTRA_PEAK     S B O
  0b111, // 18 ULTRA_RMS      S B O
  0b100, // 19 ULTRA_EVENTS   O
  0b111, // 20 INFRA_RMS      S B O
  0b010, // 21 SUB_REMOVED    B
  0b111, // 22 PLR            S B O
  0b111, // 23 STEREO_CORR    S B O
  0b111, // 24 EFF_BW         S B O
  0b100, // 25 COVERAGE_O     O: the O pass's progress (not a table row)
];

/**
 * Log-frequency axis helpers.
 */

/**
 * Map normalized x in [0,1] to Hz on a log scale between f0 and f1.
 * @param {number} x  Normalized position in [0,1]
 * @param {number} f0  Low bound Hz
 * @param {number} f1  High bound Hz
 * @returns {number}
 */
export function logFreqFromNorm(x, f0, f1) {
  return f0 * Math.pow(f1 / f0, x);
}

/**
 * Map Hz to normalized x in [0,1] on a log scale between f0 and f1.
 * @param {number} hz
 * @param {number} f0
 * @param {number} f1
 * @returns {number}
 */
export function normFromLogFreq(hz, f0, f1) {
  return Math.log(hz / f0) / Math.log(f1 / f0);
}

/**
 * Generate log-frequency axis tick marks for [f0, f1].
 * @param {number} f0
 * @param {number} f1
 * @param {number} [maxTicks=12]
 * @returns {{hz: number, label: string}[]}
 */
export function logFreqTicks(f0, f1, maxTicks = 12) {
  const decades = [10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000, 50000, 100000];
  const result = [];
  for (const hz of decades) {
    if (hz >= f0 && hz <= f1) {
      const label = hz < 1000 ? `${hz}` : `${hz / 1000}k`;
      result.push({ hz, label });
      if (result.length >= maxTicks) break;
    }
  }
  return result;
}

/**
 * Generate dB axis tick values for a linear dB axis.
 * @param {number} floor   dBFS floor (negative)
 * @param {number} range   Total range in dB (positive)
 * @param {number} [maxTicks=10]
 * @returns {number[]}
 */
export function dbAxisTicks(floor, range, maxTicks = 10) {
  const candidates = [1, 2, 3, 5, 6, 10, 12, 20, 24, 30, 40, 60, 100];
  let step = 10;
  for (const c of candidates) {
    if (range / c <= maxTicks) { step = c; break; }
  }
  const ticks = [];
  const top   = floor + range;
  let   v     = Math.ceil(floor / step) * step;
  while (v <= top) {
    ticks.push(v);
    v += step;
  }
  return ticks;
}

/**
 * Set up a canvas for DPR-correct rendering.
 * Call on every resize; then call ctx.scale(scale, scale) after.
 * @param {HTMLCanvasElement} canvas
 * @returns {{ scale: number }}
 */
export function setupCanvas(canvas) {
  const dpr = window.devicePixelRatio || 1;
  const w   = canvas.clientWidth;
  const h   = canvas.clientHeight;
  canvas.width  = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  canvas.style.width  = w + 'px';
  canvas.style.height = h + 'px';
  const ctx = canvas.getContext('2d');
  if (ctx) ctx.scale(dpr, dpr);
  return { scale: dpr };
}
