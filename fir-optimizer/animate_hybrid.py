"""Render the Hybrid-Phase explainer animation (docs/media/hybrid-phase.gif/.mp4).

A slow-motion (×20) side-by-side of the same musical moment rendered two ways:

  lane 1 — a conventional linear-phase filter: pre-ringing crawls out of
           silence BEFORE every attack;
  lane 2 — AuraEngine Hybrid-Phase: the HPSS detector sees the attack coming,
           the engine switches to the minimum-phase branch at a zero crossing
           of the mid signal, the attack lands clean, and after a 20 ms hold
           it switches back;
  lane 3 — the HPSS transient envelope and the active-branch strip.

This is an ILLUSTRATION of the switching logic (labelled as such); the real
engine lives in hpss_native.rs / hybrid_phase.rs and its verification is in
docs/06-hybrid-phase-proof.md.

Usage:
    python animate_hybrid.py [--out-dir DIR] [--frames-dir DIR]

Then assemble (ffmpeg required):
    ffmpeg -framerate 20 -i frames/f%04d.png -vf "palettegen=stats_mode=diff" pal.png
    ffmpeg -framerate 20 -i frames/f%04d.png -i pal.png \
        -lavfi "paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle" hybrid-phase.gif
    ffmpeg -framerate 20 -i frames/f%04d.png -c:v libx264 -pix_fmt yuv420p -crf 23 hybrid-phase.mp4
"""

import argparse
import os

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

BG      = "#070b14"
PANEL   = "#0d1322"
GRID    = "#1e2a44"
TEXT    = "#d7e1f2"
DIM     = "#8a99b8"
FAINT   = "#5d6b8a"
TEAL    = "#2dd4bf"
PURPLE  = "#a78bfa"
AMBER   = "#fbbf24"
SKY     = "#38bdf8"
RED     = "#f87171"
GREEN   = "#4ade80"

plt.rcParams.update({
    "figure.facecolor": BG, "axes.facecolor": PANEL, "savefig.facecolor": BG,
    "axes.edgecolor": GRID, "axes.labelcolor": TEXT, "axes.titlecolor": TEXT,
    "xtick.color": DIM, "ytick.color": DIM, "text.color": TEXT,
    "grid.color": GRID, "grid.alpha": 0.4, "axes.grid": True,
    "font.family": "DejaVu Sans",
})

# ── The scene: 160 ms of "music" with two drum hits ──────────────────────
SR = 12_000                     # display sample rate (visual smoothness only)
DUR = 0.160
T = np.arange(int(DUR * SR)) / SR
ATTACKS = [0.055, 0.115]        # transient onsets (s)
PRE_MS = 0.009                  # visible pre-ring length of the "bad" filter
DETECT_LEAD = 0.007             # HPSS lookahead before the onset
HOLD = 0.020                    # anti-chatter hold after the attack

rng = np.random.default_rng(49582)


def build_scene():
    sustain = 0.30 * np.sin(2 * np.pi * 55 * T) * 0 \
        + 0.26 * np.sin(2 * np.pi * 210 * T) \
        + 0.09 * np.sin(2 * np.pi * 545 * T + 0.8)

    attack = np.zeros_like(T)
    pre_ring = np.zeros_like(T)
    env = np.zeros_like(T)
    for t0 in ATTACKS:
        m = T >= t0
        attack[m] += 0.95 * np.exp(-(T[m] - t0) * 200) * \
            np.sin(2 * np.pi * 900 * (T[m] - t0))
        p = (T > t0 - PRE_MS) & (T < t0)
        if p.sum():
            pre_ring[p] += 0.34 * np.sin(2 * np.pi * 1350 * (T[p] - t0)) * \
                np.hanning(p.sum())
        e = T >= t0 - DETECT_LEAD
        env[e] = np.maximum(env[e], np.exp(-np.maximum(T[e] - t0, 0) * 55) *
                            np.minimum((T[e] - (t0 - DETECT_LEAD)) / DETECT_LEAD * 3, 1.0))

    y_lin = sustain + attack + pre_ring
    y_min = sustain + attack

    # switch plan: detect -> first zero crossing of the (mid) signal
    switches = []           # (on_idx, off_idx)
    for t0 in ATTACKS:
        det = t0 - DETECT_LEAD
        cand = np.where((T > det) & (np.abs(y_min) < 0.01))[0]
        on = cand[0] if len(cand) else int(det * SR)
        t_off_min = t0 + HOLD
        cand = np.where((T > t_off_min) & (np.abs(y_min) < 0.01))[0]
        off = cand[0] if len(cand) else int(t_off_min * SR)
        switches.append((on, off))

    y_hyb = y_lin.copy()
    for on, off in switches:
        y_hyb[on:off] = y_min[on:off]
    return y_lin, y_min, y_hyb, env, switches


def styled(ax):
    for s in ax.spines.values():
        s.set_color(GRID)
    ax.set_xlim(0, DUR * 1e3)
    ax.set_xticklabels([])
    ax.tick_params(length=0)


def branch_at(i, switches):
    for on, off in switches:
        if on <= i < off:
            return "min"
    return "lin"


def render(frames_dir, n_frames=190, hold_frames=30):
    y_lin, y_min, y_hyb, env, switches = build_scene()
    ms = T * 1e3
    n = len(T)

    total = n_frames + hold_frames
    for fi in range(total):
        head = min(int((fi + 1) / n_frames * n), n)

        fig = plt.figure(figsize=(8.6, 5.6), dpi=100)
        gs = fig.add_gridspec(3, 1, height_ratios=[1, 1, 0.42], hspace=0.30,
                              left=0.075, right=0.975, top=0.865, bottom=0.055)
        fig.suptitle("Hybrid-Phase — the AuraEngine signature engine   ·   slow motion ×20",
                     fontsize=12.5, fontweight="bold", y=0.975)
        fig.text(0.5, 0.912,
                 "both branches are rendered in full — the engine chooses per transient, "
                 "stereo-linked, at zero crossings   (illustration)",
                 ha="center", fontsize=8.6, color=DIM)

        # ── lane 1: conventional linear phase ──
        ax = fig.add_subplot(gs[0]); styled(ax)
        ax.set_ylim(-1.25, 1.25); ax.set_ylabel("amplitude", fontsize=8)
        ax.set_title("conventional upsampler — linear phase only", loc="left",
                     fontsize=9.5, color=SKY)
        ax.plot(ms[:head], y_lin[:head], color=SKY, lw=1.1)
        for t0 in ATTACKS:
            a, b = int((t0 - PRE_MS) * SR), int(t0 * SR)
            if head > a:
                ax.plot(ms[a:min(head, b)], y_lin[a:min(head, b)], color=RED, lw=1.6)
            if head >= b:
                ax.annotate("pre-ringing — audible smear\nBEFORE the hit",
                            xy=(ms[(a + b) // 2], 0.55), xytext=(ms[b] - 34, 0.86),
                            color=RED, fontsize=8,
                            arrowprops=dict(arrowstyle="->", color=RED, lw=0.8))

        # ── lane 2: hybrid output ──
        ax = fig.add_subplot(gs[1]); styled(ax)
        ax.set_ylim(-1.25, 1.25); ax.set_ylabel("amplitude", fontsize=8)
        ax.set_title("AuraEngine Hybrid-Phase — same moment, rendered clean", loc="left",
                     fontsize=9.5, color=TEAL)
        # draw segments colored by active branch
        start = 0
        segs = []
        for on, off in switches:
            segs += [(start, min(on, head), SKY), (on, min(off, head), AMBER)]
            start = off
        segs.append((start, head, SKY))
        for a, b, c in segs:
            if b > a:
                ax.plot(ms[a:b], y_hyb[a:b], color=c, lw=1.2)
        for on, off in switches:
            if head >= on:
                ax.axvline(ms[on], color=GREEN, ls=":", lw=1)
                ax.text(ms[on] - 1.2, 0.045, "switch @\nzero-cross", color=GREEN,
                        fontsize=6.8, ha="right", va="bottom",
                        transform=ax.get_xaxis_transform())
            if head >= off:
                ax.axvline(ms[off], color=GREEN, ls=":", lw=0.8, alpha=0.6)
        for t0 in ATTACKS:
            b = int(t0 * SR)
            if head >= b + int(0.004 * SR):
                ax.annotate("clean attack ✓", xy=(ms[b] + 1.5, 0.9), color=GREEN, fontsize=8.5)

        # ── lane 3: HPSS envelope + branch strip ──
        ax = fig.add_subplot(gs[2]); styled(ax)
        ax.set_ylim(0, 1.35); ax.set_yticks([])
        ax.set_title("HPSS transient envelope · active branch", loc="left",
                     fontsize=8.5, color=PURPLE)
        ax.fill_between(ms[:head], 0, env[:head] * 0.92, color=PURPLE, alpha=0.35, lw=0)
        ax.plot(ms[:head], env[:head] * 0.92, color=PURPLE, lw=1.0)
        for t0 in ATTACKS:
            d = int((t0 - DETECT_LEAD) * SR)
            if head >= d:
                ax.axvline(ms[d], color=PURPLE, ls=":", lw=1)
                ax.text(ms[d] - 1.2, 0.52, "detect", color=PURPLE, fontsize=6.8,
                        ha="right", transform=ax.get_xaxis_transform())
        # branch strip along the top of the lane
        start = 0
        for on, off in switches:
            if head > start:
                ax.axhspan(1.13, 1.30, xmin=ms[start] / (DUR * 1e3),
                           xmax=ms[min(on, head) - 1] / (DUR * 1e3), color=SKY, alpha=0.75, lw=0)
            if head > on:
                ax.axhspan(1.13, 1.30, xmin=ms[on] / (DUR * 1e3),
                           xmax=ms[min(off, head) - 1] / (DUR * 1e3), color=AMBER, alpha=0.85, lw=0)
            start = off
        if head > start:
            ax.axhspan(1.13, 1.30, xmin=ms[start] / (DUR * 1e3),
                       xmax=ms[head - 1] / (DUR * 1e3), color=SKY, alpha=0.75, lw=0)
        ax.text(0.4, 1.215, "LINEAR", fontsize=6.5, color=BG, fontweight="bold", va="center")

        # playhead across all lanes
        for a in fig.axes:
            a.axvline(ms[head - 1], color=TEXT, lw=0.8, alpha=0.55)

        if fi >= n_frames:  # hold: summary badges
            fig.text(0.735, 0.575, "pre-ringing: PRESENT", color=RED, fontsize=9,
                     fontweight="bold",
                     bbox=dict(boxstyle="round,pad=0.35", fc=PANEL, ec=RED, lw=1))
            fig.text(0.745, 0.315, "pre-ringing: NONE", color=GREEN, fontsize=9,
                     fontweight="bold",
                     bbox=dict(boxstyle="round,pad=0.35", fc=PANEL, ec=GREEN, lw=1))

        fig.savefig(os.path.join(frames_dir, f"f{fi:04d}.png"))
        plt.close(fig)
        if fi % 40 == 0:
            print(f"  frame {fi}/{total}")
    print(f"[=] {total} frames -> {frames_dir}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--frames-dir", default="hybrid_frames")
    args = ap.parse_args()
    os.makedirs(args.frames_dir, exist_ok=True)
    render(args.frames_dir)
