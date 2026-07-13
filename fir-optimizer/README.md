# AuraEngine FIR Filter Generator

Analytical generator for high-tap-count FIR coefficients used by the AuraEngine
offline upsampler.

## What it does

Instead of conventional windowing shortcuts, this tool generates filters through an
analytically exact pipeline:

1. **Linear-phase Kaiser** — sinc pulse windowed by Kaiser (beta 14), computed in
   vectorized float64 via scipy (default) or in quad-precision (128-bit) via mpmath
   when the legacy path is selected. Both paths produce bit-identical float64 `.npy`
   output (the scipy and mpmath results round to the same float64 values at save time).

2. **Minimum-phase conversion** — cepstral technique that scales the antisymmetric
   part of the complex cepstrum, mathematically guaranteeing exact magnitude
   preservation for any blend ratio.

3. **Optional hybrid output** (`--hybrid-blend`) — exports a matched linear-phase /
   minimum-phase pair. The Rust converter engine switches between them at
   zero-crossings using per-track transient detection (HPSS); no static alpha blend
   is written into the coefficients themselves.

Result: minimal pre-ringing in the audible band, stopband attenuation beyond
-150 dB (configurable), and a sharp brick-wall cut at the Nyquist of the source
sample rate.

## Requirements

- Python 3.10+
- NumPy, SciPy, soundfile, matplotlib, tqdm (see `requirements.txt`)
- mpmath — only needed for the legacy 128-bit path (`--legacy-mpmath`)
- Multi-core CPU (generation is parallelised with `multiprocessing`)
- No GPU or CUDA required

## Installation

```bash
cd /path/to/AuraEngine/fir-optimizer

# Create a virtual environment
python -m venv venv

# Activate (Windows)
venv\Scripts\activate
# Activate (Linux / macOS)
source venv/bin/activate

# Install dependencies
pip install -r requirements.txt
```

## Running

```bash
# Interactive menu (prompts for tap count and options)
python optimize.py

# Specify tap count directly
python optimize.py --taps 1M
python optimize.py --taps 5M
python optimize.py --taps 10M
python optimize.py --taps 30M

# Generate a matched linear/minimum-phase pair (Hybrid-Phase mode)
python optimize.py --taps 1M --hybrid-blend

# Batch-generate the full matrix for every conversion ratio the converter supports
# (4 sizes × 2 source rates × 4 multipliers × 2 phases = 64 .npy files)
python optimize.py --all-ratios

# Force the legacy mpmath 128-bit engine (bit-identical output, ~100-1000x slower)
python optimize.py --taps 1M --legacy-mpmath
```

## Output files

After generation, the `output/` directory contains:

| File | Description |
|------|-------------|
| `fir_1M_linear_phase.npy` | NumPy float64 binary — linear-phase coefficients |
| `fir_1M_minimum_phase.npy` | NumPy float64 binary — minimum-phase coefficients |
| `fir_1M_linear_phase.wav` | WAV float32 — for impulse response inspection |
| `fir_1M_minimum_phase.wav` | WAV float32 — for impulse response inspection |
| `fir_1M_linear_phase_meta.json` | Metadata: sample rates, passband/stopband Hz, analysis stats |
| `fir_1M_minimum_phase_meta.json` | Metadata for the minimum-phase variant |
| `fir_1M_comparison.png` | Frequency response and impulse response plots |

When `--all-ratios` is used the naming includes the target sample rate:
`fir_1M_88200_linear_phase.npy`, `fir_1M_352800_minimum_phase.npy`, etc.

## Configuration

Edit `config.json`:

```json
{
    "fs_source": 48000,
    "fs_target": 384000,
    "num_taps": 1000000,
    "freq_params": {
        "f_passband_hz": 20000,
        "f_stopband_hz": 24000,
        "target_stop_db": -150
    },
    "output_dir": "./output"
}
```

| Key | Description |
|-----|-------------|
| `fs_source` | Source sample rate in Hz |
| `fs_target` | Target (upsampled) sample rate in Hz |
| `num_taps` | Filter length — overridden by `--taps` on the command line |
| `freq_params.f_passband_hz` | Passband edge (Hz); typically 20 000 for full-range audio |
| `freq_params.f_stopband_hz` | Stopband edge (Hz); typically fs_source / 2 |
| `freq_params.target_stop_db` | Stopband attenuation target in dB (negative) |
| `output_dir` | Directory where `.npy`, `.wav`, `.json`, and `.png` files are written |

## Using custom filters in AuraEngine

1. Run the generator and place the `.npy` file(s) in a convenient directory.
2. In the AuraEngine converter UI, click the **Custom Filter (.npy)** button.
3. Select the `.npy` file. The tap count is read automatically from the file header.
4. For Hybrid-Phase mode, load the `_linear_phase.npy` file — the converter
   locates the matching `_minimum_phase.npy` in the same directory automatically.

> **Note:** the Custom Filter button is disabled while Hybrid-Phase mode is active
> in the converter, because the runtime manages the linear/minimum-phase pair
> internally.
