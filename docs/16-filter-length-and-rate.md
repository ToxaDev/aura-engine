# Filter Length — why "30 million taps" is not a 10-minute window

> **Files**: `fir-optimizer/optimize.py`, `desktop-app/src-tauri/src/audio/converter/utils/filter.rs`
> **Audience**: anyone who divided 30 000 000 by 48 000 and got ten minutes
> **Status**: ✅ every number below is measured from the production blobs

---

## TL;DR

A tap count in AuraEngine is defined at the **output (oversampled) rate**,
never at the source rate. The 30M filter for 44.1 kHz × 8 is an **85-second**
kernel, not a ten-minute one — and **99.76 % of its energy sits within ±1 ms
of the centre**. Dividing by 44.1/48 kHz overstates the length by 7–8×.

---

## The question

> *"10 minute long FIR window at audio rate. How?"*

The arithmetic behind it is sound:

```
30 000 000 taps ÷ 48 000 Hz = 625 s ≈ 10.4 minutes
30 000 000 taps ÷ 44 100 Hz = 680 s ≈ 11.3 minutes
```

Number of taps ÷ sample rate = length of the impulse response. That is the
right formula. The only wrong ingredient is the rate: **the big FIR never runs
at 44.1 or 48 kHz.**

## Where the taps actually live

The filter is designed *for the output rate*. In `optimize.py` the designer
takes its sample rate from `fs_target`, and the cutoff is normalised against
the **output** Nyquist:

```python
num_taps = config['num_taps']
fs       = config['fs_target']          # e.g. 352800, not 44100

f_cutoff = (f_pass + f_stop) / 2.0
fc_norm  = min(f_cutoff / (fs / 2), 0.99)
```

The blob filenames say the same thing out loud — tap count *and* output rate:

```
fir_30M_352800_linear_phase.npy      # 30M taps @ 44.1 kHz × 8
fir_5M_768000_linear_phase.npy       #  5M taps @ 48   kHz × 16
```

`filter.rs` resolves exactly that name from `(taps, target_rate_hz, phase)`,
so a 30M blob only ever exists in the context of one specific output rate.

## Kernel span, all presets, all rates

Length in seconds = taps ÷ output rate:

| Output rate | 1M | 5M | 10M | 30M |
|---|---|---|---|---|
| 88.2 kHz (44.1 × FS2) | 11.34 s | 56.69 s | 1.89 min | **5.67 min** |
| 96 kHz (48 × FS2) | 10.42 s | 52.08 s | 1.74 min | 5.21 min |
| 176.4 kHz (44.1 × FS4) | 5.67 s | 28.34 s | 56.69 s | 2.83 min |
| 192 kHz (48 × FS4) | 5.21 s | 26.04 s | 52.08 s | 2.60 min |
| 352.8 kHz (44.1 × FS8) | 2.83 s | 14.17 s | 28.34 s | **1.42 min** |
| 384 kHz (48 × FS8) | 2.60 s | 13.02 s | 26.04 s | 1.30 min |
| 705.6 kHz (44.1 × FS16) | 1.42 s | 7.09 s | 14.17 s | 42.52 s |
| 768 kHz (48 × FS16) | 1.30 s | 6.51 s | 13.02 s | **39.06 s** |

So the honest range for the largest preset is **39 seconds to 5.7 minutes**,
depending on output rate. The lowest multiplier really does produce a
multi-minute kernel — the 10-minute estimate is not wild, just off by roughly
2× at the extreme and by 7–8× at the FS8 default.

## The polyphase view gives the same answer

When `out_rate % src_rate == 0`, `process.rs` takes the integrated path and
`polyphase_decompose(&full_coeffs, L)` splits the prototype into `L`
sub-filters of `N/L` taps, each fed at the **input** rate:

```
30M taps @ 352.8 kHz  ≡  8 phases × 3.75M taps @ 44.1 kHz
        85 s          ≡            85 s
```

The decomposition changes the arithmetic cost per output sample. It does not
change the support of the kernel — that is invariant. Either way you are
looking at 85 seconds, not 10 minutes.

## Group delay: 42.5 s, and that is fine

The linear-phase filters are symmetric, so group delay is `(N−1)/2` samples —
for 30M @ 352.8 kHz, **42.52 s** (measured `group_delay_mean = 14 999 999.5`
samples, σ = 6.7 × 10⁻¹² ms).

This is why AuraEngine is an **offline converter** and not a plugin. There is
no deadline to miss: the delay is trimmed analytically together with the
overlap-save convolver latency (2 blocks CPU / 1 block GPU) and covered by
unit tests rather than tuned by ear. A real-time path could not do this at
all; a file renderer simply pays wall-clock time.

## Why the kernel is this long: transition width

Length is the knob for frequency resolution — transition width scales as
`fs / N`. Measured on the production 30M @ 352.8 kHz blob:

| Quantity | Measured |
|---|---|
| Stopband attenuation (≥ 22.05 kHz) | **−220.93 dB** |
| Passband ripple (DC … 20.05 kHz) | **≤ 7.4 × 10⁻¹¹ dB** |
| Transition width (−6 → −120 dB) | **≈ 0.05 Hz** |
| DC gain | 1.000000000000 |

Design spec is `f_pass = 20 050 Hz`, `f_stop = 22 050 Hz` — a 2 kHz guard —
but 30M taps realise a transition far sharper than requested, so the −6 dB
point sits at `fc = 21 050 Hz` and the roll-off is ≈0.05 Hz wide. For scale,
the transition band of a typical DAC reconstruction filter is 2–4 kHz.

### Re-verified without an FFT

A −220 dB figure deserves suspicion: a 67M-point `f64` FFT has an arithmetic
noise floor of roughly that order, so an FFT-derived number could be measuring
the measurement. The response was therefore recomputed straight from the
symmetric kernel, with the phase reduced by exact integer arithmetic
(`(num·(2j+1)) mod den`, no rounding in the argument), which drops the
arithmetic floor to ≈ −280 dB:

| Frequency | Attenuation |
|---|---|
| 21 050 Hz (`fc`) | −6.02 dB |
| 21 051 Hz | −161.0 dB |
| 21 100 Hz | −199.4 dB |
| 21 500 Hz | −218.7 dB |
| **22 050 Hz** (stopband edge) | **−220.9 dB** |
| 25 000 Hz | −240.7 dB |
| 30 000 Hz | −254.3 dB |
| 88 200 Hz | −266.7 dB |

The independent method reproduces −220.9 dB at the stopband edge, so the figure
is a property of the filter, not of the FFT.

One caveat worth stating plainly: a Kaiser window's **peak sidelobe level is set
by β, not by tap count**, so within roughly 0.3 Hz above `fc` the sidelobes do
reach about −137 dB. That sliver sits inside the nominal transition band — 1 Hz
above the cutoff the response is already −161 dB. What the tap count buys is how
fast the sidelobe envelope collapses across the 1 kHz gap to `f_stop`: the same
design at 2M taps measures −197.6 dB at 22.05 kHz, at 30M taps −220.9 dB.

Whether any of this is *audibly* better than a 2 kHz DAC transition is a separate
argument this document does not make. These are measurements, not claims about
perception.

## 85 seconds of support is not 85 seconds of ringing

The obvious follow-up: does an 85-second kernel smear transients across 85
seconds? No — and this is measurable rather than arguable. From the actual
`fir_30M_352800_linear_phase.npy`:

| Energy within … of the centre | Share of total |
|---|---|
| ±1 ms | **99.7588 %** |
| ±10 ms | 99.9760 % |
| ±100 ms | 99.9976 % |
| ±1000 ms | 99.9998 % |

The outermost coefficients sit at **−250 dB** relative to the peak — some 110 dB
below the least-significant bit of a 24-bit sample. The 85-second support is a
mathematical property of the window; the energy that does any work is confined
to a couple of milliseconds. What the far tails buy is the 0.05 Hz transition,
nothing else.

Symmetric pre-ringing is inherent to linear phase, which is why every tap size
also ships a **minimum-phase** variant (`fir_30M_352800_minimum_phase.npy`,
derived by the cepstral method), and why the Hybrid-Phase engine exists —
see [06-hybrid-phase-proof.md](06-hybrid-phase-proof.md).

## How a 30M-tap convolution is actually computed

Direct time-domain convolution would need 3 × 10⁷ multiply-accumulates *per
output sample*. It is never done that way. Both paths use **uniform
partitioned FFT overlap-save**:

- **CPU** (`dsp_core.rs`): partitions of 32 768 taps, FFT size 65 536, each
  partition pre-transformed once. The frequency-domain accumulation loop is
  Kahan-compensated in `f64`, over a circular frequency-domain delay line.
- **GPU** (`gpu/wola.rs`): the same structure with block size
  `next_power_of_two(taps).clamp(262 144, 2 097 152)`, radix-2 Cooley-Tukey
  butterflies in GLSL compute shaders, double-single arithmetic (~48-bit
  mantissa) to stay within ~−260 dB of the CPU reference.

## Reproduce these numbers

```python
import numpy as np
h  = np.load('fir_30M_352800_linear_phase.npy')
fs = 352_800.0

print(len(h) / fs)                      # 85.03  seconds of span
print((len(h) - 1) / 2 / fs)            # 42.52  seconds of group delay

pk, c, tot = np.abs(h).max(), len(h)//2, h @ h
print(20*np.log10(abs(h[0])/pk))        # -250.0 dB  outermost coefficient
w = int(fs * 0.001)                     # +/-1 ms window
print((h[c-w:c+w+1] @ h[c-w:c+w+1]) / tot)   # 0.99759  of all energy
```

Filter packs are on the
[Releases page](https://github.com/ToxaDev/aura-engine/releases); the full
measurement gallery with methodology is in
[15-measurements.md](15-measurements.md).
