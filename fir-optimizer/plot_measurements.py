"""Generate the measurement plots shown in docs/15-measurements.md.

Every curve in m1–m3 is computed from the actual production filter blobs
(fir-optimizer/output/*.npy) — the same files the converter loads at runtime.
Nothing is idealised: if the filters were bad, these plots would show it.
m4 is a synthetic illustration of the Hybrid-Phase switching logic (marked
as such on the plot).

Usage:
    python plot_measurements.py [--filter-dir DIR] [--out-dir DIR]

Defaults: --filter-dir output/ (or $AURA_FILTER_DIR), --out-dir ../docs/media/.
Requires the 352.8 kHz (FS8, 44.1 kHz family) blobs:
    fir_{1M,5M,10M,30M}_352800_linear_phase.npy
    fir_1M_352800_minimum_phase.npy

Measured numbers (passband ripple, stopband depth, DC-gain error) are
printed to stdout and embedded into the plots.
"""

import argparse
import os
import sys

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

# ── Dark theme (matches docs/index.html palette) ─────────────────────────
BG      = "#070b14"
PANEL   = "#0d1322"
GRID    = "#1e2a44"
TEXT    = "#d7e1f2"
DIM     = "#8a99b8"
TEAL    = "#2dd4bf"
PURPLE  = "#a78bfa"
AMBER   = "#fbbf24"
SKY     = "#38bdf8"
RED     = "#f87171"
GREEN   = "#4ade80"

plt.rcParams.update({
    "figure.facecolor": BG,
    "axes.facecolor": PANEL,
    "savefig.facecolor": BG,
    "axes.edgecolor": GRID,
    "axes.labelcolor": TEXT,
    "axes.titlecolor": TEXT,
    "xtick.color": DIM,
    "ytick.color": DIM,
    "text.color": TEXT,
    "grid.color": GRID,
    "grid.alpha": 0.5,
    "axes.grid": True,
    "font.family": "DejaVu Sans",
    "legend.facecolor": PANEL,
    "legend.edgecolor": GRID,
})

RATE = 352_800          # FS8 target of the 44.1 kHz family
F_PASS = 20_050.0       # design passband edge (44.1 family)
F_STOP = 22_050.0       # design stopband start = source Nyquist


def db(x, floor=-400.0):
    return np.maximum(20.0 * np.log10(np.maximum(np.abs(x), 1e-300)), floor)


def load(filter_dir, name):
    path = os.path.join(filter_dir, name)
    if not os.path.exists(path):
        sys.exit(f"[!] missing blob: {path}\n    generate with: python optimize.py --all-ratios")
    h = np.load(path, mmap_mode="r")
    print(f"[*] {name}: {len(h):,} taps")
    return h


def spectrum(h, pad_pow2_extra=1):
    """rfft magnitude of h zero-padded to the next power of two × 2."""
    n_fft = 1 << (int(np.ceil(np.log2(len(h)))) + pad_pow2_extra)
    H = np.fft.rfft(np.asarray(h, dtype=np.float64), n_fft)
    f = np.fft.rfftfreq(n_fft, d=1.0 / RATE)
    return f, np.abs(H)


def fig_new(h_ratio=1.0):
    fig = plt.figure(figsize=(10.8, 10.8 * h_ratio), dpi=150)
    return fig


def style_axes(ax):
    for s in ax.spines.values():
        s.set_color(GRID)


# ═════════════════════════════ m1: frequency response ════════════════════
def plot_m1(filter_dir, out_dir):
    h = load(filter_dir, "fir_30M_352800_linear_phase.npy")
    dc_err = abs(float(np.sum(np.asarray(h, dtype=np.float64))) - 1.0)
    f, mag = spectrum(h)
    mag_db = db(mag)

    pb = f <= F_PASS
    sb = f >= F_STOP
    ripple = float(np.max(np.abs(mag_db[pb])))
    stop = float(np.max(mag_db[sb]))
    print(f"    passband ripple : ±{ripple:.2e} dB (0–{F_PASS/1e3:.2f} kHz)")
    print(f"    stopband max    : {stop:.1f} dB (≥{F_STOP/1e3:.2f} kHz)")
    print(f"    |sum(h) − 1|    : {dc_err:.2e}")

    fig = fig_new(1.30)
    gs = fig.add_gridspec(3, 1, height_ratios=[2.2, 1, 1], hspace=0.42,
                          left=0.10, right=0.96, top=0.90, bottom=0.06)

    fig.suptitle("AuraEngine — measured filter response\n"
                 "fir_30M_352800_linear_phase.npy · the actual production blob "
                 "(44.1 kHz → 352.8 kHz, Kaiser β=14)",
                 fontsize=13, fontweight="bold", y=0.975)

    # full view
    ax = fig.add_subplot(gs[0]); style_axes(ax)
    sl = f <= 50_000
    ax.plot(f[sl] / 1e3, mag_db[sl], color=SKY, lw=1.0)
    ax.axvline(F_PASS / 1e3, color=GREEN, ls=":", lw=1, alpha=0.8)
    ax.axvline(F_STOP / 1e3, color=RED, ls=":", lw=1, alpha=0.8)
    ax.set_title("Full view — passband → stopband", color=PURPLE, fontsize=11)
    ax.set_xlabel("Frequency (kHz)"); ax.set_ylabel("Magnitude (dB)")
    ax.set_ylim(-340, 12)
    ax.annotate(f"measured stopband ≤ {stop:.0f} dB\n(design law: −140 dB, no ripple)",
                xy=(30, stop), xytext=(32, -80),
                arrowprops=dict(arrowstyle="->", color=DIM), fontsize=9, color=TEXT)
    ax.text(F_PASS / 1e3 - 0.6, -60, "20.05 kHz\npassband edge", color=GREEN,
            fontsize=8, ha="right")
    ax.text(2.0, -130, "passband: bit-flat\n(see bottom panel)", color=SKY, fontsize=8)

    # transition zoom
    ax = fig.add_subplot(gs[1]); style_axes(ax)
    sl = (f >= 19_000) & (f <= 25_000)
    ax.plot(f[sl] / 1e3, mag_db[sl], color=TEAL, lw=1.2)
    ax.axvline(F_PASS / 1e3, color=GREEN, ls=":", lw=1)
    ax.axvline(F_STOP / 1e3, color=RED, ls=":", lw=1)
    ax.set_title("Transition band zoom (19–25 kHz)", color=PURPLE, fontsize=11)
    ax.set_xlabel("Frequency (kHz)"); ax.set_ylabel("dB")
    ax.set_ylim(-340, 12)

    # passband ripple
    ax = fig.add_subplot(gs[2]); style_axes(ax)
    sl = f <= F_PASS
    ax.plot(f[sl] / 1e3, mag_db[sl] * 1e9, color=AMBER, lw=0.9)
    ax.set_title(f"Passband ripple — measured ±{ripple:.1e} dB "
                 f"(scale: nano-dB) · |sum(h)−1| = {dc_err:.1e}",
                 color=PURPLE, fontsize=11)
    ax.set_xlabel("Frequency (kHz)"); ax.set_ylabel("Magnitude (ndB)")

    out = os.path.join(out_dir, "m1-frequency-response.png")
    fig.savefig(out); plt.close(fig)
    print(f"    -> {out}")
    return ripple, stop, dc_err


# ═════════════════════════════ m2: impulse response ══════════════════════
def plot_m2(filter_dir, out_dir):
    h_lin = np.asarray(load(filter_dir, "fir_1M_352800_linear_phase.npy"), dtype=np.float64)
    h_min = np.asarray(load(filter_dir, "fir_1M_352800_minimum_phase.npy"), dtype=np.float64)

    n = len(h_lin)
    pk_lin = int(np.argmax(np.abs(h_lin)))
    pk_min = int(np.argmax(np.abs(h_min)))
    sym_err = float(np.max(np.abs(h_lin - h_lin[::-1])))
    pre_energy = float(np.sqrt(np.sum(h_min[:max(pk_min, 0)] ** 2)))
    print(f"    linear peak @ {pk_lin:,} (N/2 = {n // 2:,}) · symmetry err {sym_err:.1e}")
    print(f"    min-phase peak @ {pk_min:,} · pre-peak RMS energy {pre_energy:.2e}")

    fig = fig_new(1.15)
    gs = fig.add_gridspec(3, 1, height_ratios=[1, 1, 1], hspace=0.45,
                          left=0.10, right=0.96, top=0.88, bottom=0.06)
    fig.suptitle("AuraEngine — impulse response, measured from the production blobs\n"
                 "fir_1M_352800 pair · linear phase vs minimum phase",
                 fontsize=13, fontweight="bold", y=0.97)

    w = 1200
    ms = 1e3 / RATE

    ax = fig.add_subplot(gs[0]); style_axes(ax)
    x = np.arange(-w, w)
    ax.plot(x * ms, h_lin[pk_lin - w: pk_lin + w], color=SKY, lw=0.9)
    ax.set_title(f"Linear phase — symmetric, peak exactly at sample N/2 "
                 f"(max asymmetry {sym_err:.0e})", color=PURPLE, fontsize=10.5)
    ax.set_xlabel("Time from peak (ms)"); ax.set_ylabel("Amplitude")
    ax.annotate("pre-ringing\n(before the attack)", xy=(-w * ms * 0.55, 0.0006),
                xytext=(-w * ms * 0.9, 0.004),
                arrowprops=dict(arrowstyle="->", color=RED), color=RED, fontsize=9)

    ax = fig.add_subplot(gs[1]); style_axes(ax)
    # The blob's first sample IS the filter onset: pad explicit zeros to the
    # left so the plot shows what arrives before it — nothing.
    pad = 600
    seg = np.concatenate([np.zeros(pad), h_min[: pk_min + 2 * w]])
    x = np.arange(-pad - pk_min, 2 * w)
    ax.plot(x * ms, seg, color=AMBER, lw=0.9)
    ax.set_title("Minimum phase — causal: mathematically zero energy before the onset",
                 color=PURPLE, fontsize=10.5)
    ax.set_xlabel("Time from peak (ms)"); ax.set_ylabel("Amplitude")
    ax.annotate("nothing here — ever", xy=(-(pad * 0.55 + pk_min) * ms, 0.0),
                xytext=(-(pad + pk_min) * ms * 0.92, 0.03),
                arrowprops=dict(arrowstyle="->", color=GREEN), color=GREEN, fontsize=9)

    # log envelope comparison
    ax = fig.add_subplot(gs[2]); style_axes(ax)
    x = np.arange(-3000, 3000)
    ax.plot(x * ms, db(h_lin[pk_lin - 3000: pk_lin + 3000], -400), color=SKY,
            lw=0.7, label="linear phase")
    seg = np.zeros(6000)
    seg[3000 - pk_min:] = h_min[: 3000 + pk_min]
    ax.plot(x * ms, db(seg, -400), color=AMBER, lw=0.7, label="minimum phase")
    ax.set_ylim(-400, 20)
    ax.set_title("Envelope, log scale — min-phase left flank drops to the numerical floor",
                 color=PURPLE, fontsize=10.5)
    ax.set_xlabel("Time from peak (ms)"); ax.set_ylabel("dB")
    ax.legend(loc="upper right", fontsize=9)

    out = os.path.join(out_dir, "m2-impulse-response.png")
    fig.savefig(out); plt.close(fig)
    print(f"    -> {out}")


# ═════════════════════════════ m3: resolution ladder ═════════════════════
def plot_m3(filter_dir, out_dir):
    fig = fig_new(0.95)
    gs = fig.add_gridspec(2, 1, height_ratios=[1, 1], hspace=0.38,
                          left=0.10, right=0.97, top=0.84, bottom=0.08)
    fig.suptitle("Why million-tap filters — the same design measured at four sizes\n"
                 "fir_*_352800_linear_phase.npy · Kaiser β=14 · 44.1 kHz → 352.8 kHz",
                 fontsize=12.5, fontweight="bold", y=0.965)

    ax_tr = fig.add_subplot(gs[0]); style_axes(ax_tr)
    ax_sb = fig.add_subplot(gs[1]); style_axes(ax_sb)

    fc = 21_050.0  # transition centre = (20 050 + 22 050) / 2
    widths = {}
    for name, taps, color in [("1M", 1e6, DIM), ("5M", 5e6, SKY),
                              ("10M", 10e6, PURPLE), ("30M", 30e6, TEAL)]:
        h = load(filter_dir, f"fir_{name}_352800_linear_phase.npy")
        f, mag = spectrum(h, pad_pow2_extra=1)
        mag_db = db(mag)

        sl = (f >= fc - 30) & (f <= fc + 30)
        ax_tr.plot(f[sl] - fc, mag_db[sl], color=color, lw=1.4, label=f"{name} taps")
        # measured -6 dB..-120 dB transition width
        tr = (f >= fc - 200) & (f <= fc + 200)
        ft, mt = f[tr], mag_db[tr]
        try:
            w6 = ft[mt <= -6][0]; w120 = ft[mt <= -120][0]
            widths[name] = w120 - w6
        except IndexError:
            pass

        sl = (f >= 21_000) & (f <= 23_500)
        ax_sb.plot(f[sl] / 1e3, mag_db[sl], color=color, lw=1.0, label=f"{name} taps")

    wtxt = " · ".join(f"{k}: {v:.2f} Hz" for k, v in widths.items())
    print(f"    −6→−120 dB transition width: {wtxt}")
    ax_tr.set_title(f"Transition slope, ±30 Hz around 21.05 kHz — measured −6→−120 dB width:  {wtxt}",
                    color=PURPLE, fontsize=9.5)
    ax_tr.set_xlabel("Frequency offset from 21.05 kHz (Hz)")
    ax_tr.set_ylabel("Magnitude (dB)")
    ax_tr.set_ylim(-300, 15)
    ax_tr.legend(loc="upper right", fontsize=9)

    ax_sb.axvline(F_STOP / 1e3, color=RED, ls=":", lw=1)
    ax_sb.text(F_STOP / 1e3, 6, " 22.05 kHz — source Nyquist", color=RED, fontsize=8.5,
               transform=ax_sb.get_xaxis_transform())
    ax_sb.set_title("Stopband floor — more taps push the aliasing residue deeper",
                    color=PURPLE, fontsize=10.5)
    ax_sb.set_xlabel("Frequency (kHz)"); ax_sb.set_ylabel("Magnitude (dB)")
    ax_sb.set_ylim(-340, 12)
    ax_sb.legend(loc="upper right", fontsize=9)

    out = os.path.join(out_dir, "m3-resolution-ladder.png")
    fig.savefig(out); plt.close(fig)
    print(f"    -> {out}")


# ═════════════════════════════ m4: hybrid-phase stitch ═══════════════════
def plot_m4(out_dir):
    """Synthetic illustration of the stereo-linked zero-crossing switch."""
    sr = RATE
    t = np.arange(int(0.050 * sr)) / sr
    rng = np.random.default_rng(49582)

    sustain = 0.28 * np.sin(2 * np.pi * 320 * t) + 0.06 * np.sin(2 * np.pi * 810 * t + 0.7)
    onset = 0.030
    attack = np.zeros_like(t)
    m = t >= onset
    attack[m] = 0.9 * np.exp(-(t[m] - onset) * 260) * np.sin(2 * np.pi * 1400 * (t[m] - onset))
    pre_ring = np.zeros_like(t)
    m2 = (t > onset - 0.008) & (t < onset)
    pre_ring[m2] = 0.13 * np.sin(2 * np.pi * 17000 * (t[m2] - onset)) * \
        np.hanning(m2.sum()) if m2.sum() else 0.0

    y_lin = sustain + attack + pre_ring
    y_min = sustain + attack

    # envelope rises shortly before the onset (HPSS lookahead), switch snaps
    # to the nearest zero crossing of the mid signal
    detect = onset - 0.006
    zc_region = np.where((t > detect) & (np.abs(y_min) < 0.004))[0]
    stitch = zc_region[0] if len(zc_region) else int(detect * sr)
    fade = 32  # samples — the raised-cosine micro-fade (~0.09 ms at 352.8 kHz)

    y_hyb = y_lin.copy()
    y_hyb[stitch + fade:] = y_min[stitch + fade:]
    w = 0.5 - 0.5 * np.cos(np.linspace(0, np.pi, fade))
    y_hyb[stitch:stitch + fade] = (1 - w) * y_lin[stitch:stitch + fade] + \
        w * y_min[stitch:stitch + fade]

    fig = fig_new(0.85)
    gs = fig.add_gridspec(2, 1, height_ratios=[1, 1], hspace=0.35,
                          left=0.09, right=0.97, top=0.86, bottom=0.08)
    fig.suptitle("Hybrid-Phase — stereo-linked zero-crossing switch (illustration)\n"
                 "one switch plan from the mid signal · both channels switch at the same sample",
                 fontsize=12.5, fontweight="bold", y=0.97)

    ms = t * 1e3
    ax = fig.add_subplot(gs[0]); style_axes(ax)
    ax.plot(ms, y_lin, color=SKY, lw=0.9, label="linear-phase branch")
    ax.plot(ms, y_min, color=AMBER, lw=0.9, ls="--", alpha=0.85, label="minimum-phase branch")
    ax.axvspan((onset - 0.008) * 1e3, onset * 1e3, color=RED, alpha=0.10)
    ax.axvline(onset * 1e3, color=RED, ls=":", lw=1)
    ax.text(onset * 1e3 + 0.4, 0.90, "transient\nonset", color=RED, fontsize=8.5,
            transform=ax.get_xaxis_transform(), va="top")
    ax.text((onset - 0.0075) * 1e3, 0.90, "pre-ringing\n(linear only)", color=RED,
            fontsize=8, transform=ax.get_xaxis_transform(), va="top")
    ax.set_ylabel("Amplitude"); ax.legend(loc="upper left", fontsize=9)
    ax.set_title("The two full convolution passes the engine computes", color=PURPLE, fontsize=10.5)

    ax = fig.add_subplot(gs[1]); style_axes(ax)
    ax.plot(ms[:stitch + 1], y_hyb[:stitch + 1], color=SKY, lw=1.1, label="linear-phase segment")
    ax.plot(ms[stitch:], y_hyb[stitch:], color=AMBER, lw=1.1, label="minimum-phase segment")
    ax.axvline(t[stitch] * 1e3, color=GREEN, ls=":", lw=1.2)
    ax.text(t[stitch] * 1e3 + 0.4, 0.97, "switch @\nzero crossing", color=GREEN, fontsize=8.5,
            transform=ax.get_xaxis_transform(), va="top")
    ax.axvline(detect * 1e3, color=PURPLE, ls=":", lw=1)
    ax.text(detect * 1e3 - 0.4, 0.97, "HPSS\ndetect", color=PURPLE, fontsize=8.5,
            ha="right", transform=ax.get_xaxis_transform(), va="top")
    ax.annotate(f"raised-cosine micro-fade · {fade} samples (~{fade / sr * 1e3:.2f} ms)\n"
                "+ 20 ms anti-chatter hold",
                xy=(t[stitch] * 1e3, 0.1), xytext=(3.0, -0.95),
                arrowprops=dict(arrowstyle="->", color=DIM), fontsize=8.5)
    ax.set_ylim(-1.35, 1.0)
    ax.set_xlabel("Time (ms)"); ax.set_ylabel("Amplitude")
    ax.legend(loc="upper left", fontsize=9)
    ax.set_title("The rendered hybrid output — pre-ringing gone, sustain stays linear-phase",
                 color=PURPLE, fontsize=10.5)

    out = os.path.join(out_dir, "m4-hybrid-stitch.png")
    fig.savefig(out); plt.close(fig)
    print(f"    -> {out}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--filter-dir",
                    default=os.environ.get("AURA_FILTER_DIR",
                                           os.path.join(os.path.dirname(__file__), "output")))
    ap.add_argument("--out-dir",
                    default=os.path.join(os.path.dirname(__file__), "..", "docs", "media"))
    args = ap.parse_args()
    os.makedirs(args.out_dir, exist_ok=True)
    print(f"[*] filter dir: {args.filter_dir}")
    print(f"[*] out dir   : {args.out_dir}")

    plot_m1(args.filter_dir, args.out_dir)
    plot_m2(args.filter_dir, args.out_dir)
    plot_m3(args.filter_dir, args.out_dir)
    plot_m4(args.out_dir)
    print("[=] all measurement plots generated")


if __name__ == "__main__":
    main()
