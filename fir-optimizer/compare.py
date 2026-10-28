#!/usr/bin/env python3
"""
═══════════════════════════════════════════════════════════════
AuraEngine — Hybrid-Phase Blending Comparison Tool
═══════════════════════════════════════════════════════════════

Interactive GUI tool for comparing original audio with
Hybrid-Phase processed versions. Shows real waveforms at
transient locations with phase blend envelopes.

Usage:
    python compare.py
    (then use the GUI to load files)
"""

import os
import sys
import json
import tkinter as tk
from tkinter import filedialog, ttk
import numpy as np
import soundfile as sf
from pathlib import Path
from math import gcd

import matplotlib
matplotlib.use('TkAgg')
import matplotlib.pyplot as plt
from matplotlib.backends.backend_tkagg import FigureCanvasTkAgg, NavigationToolbar2Tk
from matplotlib.figure import Figure
from scipy.signal import resample_poly


# ═══════════════════════════════════════════════════════════
# Audio helpers
# ═══════════════════════════════════════════════════════════

def load_audio(path):
    """Load audio file → (mono_samples, sample_rate)"""
    data, sr = sf.read(path, dtype='float64')
    if data.ndim == 2:
        mono = (data[:, 0] + data[:, 1]) * 0.5
    else:
        mono = data
    return mono, sr


def resample_match(samples, from_sr, to_sr):
    """Resample to match target rate."""
    if from_sr == to_sr:
        return samples
    g = gcd(int(from_sr), int(to_sr))
    up = int(to_sr) // g
    down = int(from_sr) // g
    if up > 100 or down > 100:
        target_len = int(len(samples) * to_sr / from_sr)
        x_old = np.linspace(0, 1, len(samples))
        x_new = np.linspace(0, 1, target_len)
        return np.interp(x_new, x_old, samples)
    return resample_poly(samples, up, down)


def detect_transients(samples, sr, num_points=8):
    """Energy-based onset detection."""
    hop = max(64, sr // 500)
    win = hop * 2
    n = len(samples)
    num_frames = max(0, (n - win) // hop)
    if num_frames < 3:
        return []

    energy = np.zeros(num_frames)
    for f in range(num_frames):
        s = f * hop
        e = min(s + win, n)
        energy[f] = np.mean(samples[s:e] ** 2)

    onset = np.zeros(num_frames)
    onset[1:] = np.maximum(0, np.diff(energy))

    median = np.median(onset)
    mad = np.median(np.abs(onset - median))
    threshold = median + 3.0 * max(mad, 1e-10)

    min_dist = max(1, sr // (20 * hop))
    transients = []
    last = -min_dist

    for i in range(1, num_frames - 1):
        if (onset[i] > threshold and
            onset[i] >= onset[i-1] and
            onset[i] >= onset[i+1] and
            i - last >= min_dist):
            max_o = max(onset.max(), 1e-10)
            strength = min(1.0, onset[i] / max_o)
            if strength > 0.1:
                transients.append({
                    'sample': i * hop,
                    'time_s': (i * hop) / sr,
                    'strength': strength
                })
                last = i

    transients.sort(key=lambda t: -t['strength'])
    return transients[:num_points]


def build_envelope(transients, total_samples, sr, hold_ms=15.0, release_ms=30.0, pre_onset_ms=5.0, **kwargs):
    """Recreate blending envelope from transient positions (v2 continuous follower)."""
    # Handle legacy sidecar format
    if 'attack_ms' in kwargs:
        pre_onset_ms = kwargs['attack_ms']
    
    hold = int(hold_ms * sr / 1000.0)
    release = int(release_ms * sr / 1000.0)
    pre_onset = int(pre_onset_ms * sr / 1000.0)
    crossfade = int(2.0 * sr / 1000.0)  # 2ms crossfade
    envelope = np.zeros(total_samples)

    for t in transients:
        onset = int(t.get('sample', t.get('time_s', 0) * sr))
        strength = t.get('strength', 1.0)

        # Full min-phase zone: onset to onset + hold
        min_start = onset
        min_end = min(total_samples, onset + hold)
        for idx in range(min_start, min_end):
            envelope[idx] = max(envelope[idx], strength)

        # Pre-onset fade-in (cos^2): extends BEFORE onset for pre-ring protection
        pre_start = max(0, onset - pre_onset)
        if pre_start < onset:
            fl = onset - pre_start
            for i in range(fl):
                idx = pre_start + i
                if idx < total_samples:
                    t_norm = i / fl
                    val = 0.5 * (1.0 - np.cos(np.pi * t_norm))
                    envelope[idx] = max(envelope[idx], val * strength)

        # Post-hold release (cos^2 fade-out)
        fade_start = min_end
        fade_end = min(total_samples, fade_start + release)
        if fade_end > fade_start:
            fl = fade_end - fade_start
            for i in range(fl):
                idx = fade_start + i
                if idx < total_samples:
                    t_norm = i / fl
                    val = 0.5 * (1.0 + np.cos(np.pi * t_norm))
                    envelope[idx] = max(envelope[idx], val * strength)

    return np.clip(envelope, 0, 1)


def find_sidecar(flac_path):
    """Look for .hybrid_phase.json sidecar near the file."""
    parent = Path(flac_path).parent
    for f in parent.glob("*.hybrid_phase.json"):
        try:
            return json.loads(f.read_text(encoding='utf-8'))
        except:
            pass
    return None


# ═══════════════════════════════════════════════════════════
# GUI Application
# ═══════════════════════════════════════════════════════════

COLORS = {
    'bg':        '#0d0d15',
    'panel':     '#13131f',
    'accent':    '#1a1a2e',
    'border':    '#2a2a3e',
    'text':      '#e0e0e0',
    'dim':       '#666680',
    'orig':      '#3cff6e',
    'proc':      '#38c9ff',
    'envelope':  '#ff6b9d',
    'min_mark':  '#ff9500',
    'lin_mark':  '#9d4edd',
    'btn_bg':    '#1e1e32',
    'btn_hover': '#2e2e4a',
    'green':     '#00cc55',
    'blue':      '#3388ff',
}


class CompareApp:
    def __init__(self, root):
        self.root = root
        self.root.title("AuraEngine — Hybrid-Phase Comparison")
        self.root.geometry("1600x900")
        self.root.configure(bg=COLORS['bg'])
        self.root.minsize(1000, 600)

        # State
        self.original = None       # {'path', 'samples', 'sr', 'name'}
        self.comparisons = []      # list of {'path', 'samples', 'sr', 'name', 'sidecar'}
        self.transient_windows = []
        self.window_ms = 40        # default window width in ms

        self._build_ui()

    def _build_ui(self):
        # ── Top bar ──
        top = tk.Frame(self.root, bg=COLORS['panel'], height=60)
        top.pack(fill='x', padx=0, pady=0)
        top.pack_propagate(False)

        tk.Label(top, text="AuraEngine - Hybrid-Phase Blending Comparison",
                font=('Segoe UI', 14, 'bold'), fg='#ffffff', bg=COLORS['panel']
                ).pack(side='left', padx=16, pady=10)

        # Buttons
        btn_frame = tk.Frame(top, bg=COLORS['panel'])
        btn_frame.pack(side='right', padx=16)

        self.btn_orig = self._make_button(btn_frame, "[+] Load Original", self._load_original, COLORS['green'])
        self.btn_orig.pack(side='left', padx=4)

        self.btn_comp = self._make_button(btn_frame, "[+] Add Comparison", self._load_comparison, COLORS['blue'])
        self.btn_comp.pack(side='left', padx=4)

        self.btn_clear = self._make_button(btn_frame, "[x] Clear All", self._clear_all, '#aa3333')
        self.btn_clear.pack(side='left', padx=4)

        # Window size
        tk.Label(btn_frame, text="Window:", fg=COLORS['dim'], bg=COLORS['panel'],
                font=('Segoe UI', 9)).pack(side='left', padx=(16,4))
        
        self.win_var = tk.StringVar(value="40")
        win_combo = ttk.Combobox(btn_frame, textvariable=self.win_var,
                                values=["10", "20", "40", "80", "150", "300"],
                                width=5, state='readonly')
        win_combo.pack(side='left', padx=2)
        win_combo.bind('<<ComboboxSelected>>', lambda e: self._on_window_change())

        tk.Label(btn_frame, text="ms", fg=COLORS['dim'], bg=COLORS['panel'],
                font=('Segoe UI', 9)).pack(side='left', padx=2)

        # ── Status bar ──
        self.status_frame = tk.Frame(self.root, bg=COLORS['accent'], height=28)
        self.status_frame.pack(fill='x', side='bottom')
        self.status_frame.pack_propagate(False)

        self.status_label = tk.Label(self.status_frame, text="Load an original file to begin.",
                                    fg=COLORS['dim'], bg=COLORS['accent'],
                                    font=('Segoe UI', 9), anchor='w')
        self.status_label.pack(side='left', padx=12, fill='x')

        # ── File list (left sidebar) ──
        self.sidebar = tk.Frame(self.root, bg=COLORS['panel'], width=280)
        self.sidebar.pack(fill='y', side='left', padx=0, pady=0)
        self.sidebar.pack_propagate(False)

        tk.Label(self.sidebar, text="LOADED FILES", fg=COLORS['dim'], bg=COLORS['panel'],
                font=('Segoe UI', 9, 'bold')).pack(anchor='w', padx=12, pady=(10,4))

        self.file_list = tk.Frame(self.sidebar, bg=COLORS['panel'])
        self.file_list.pack(fill='both', expand=True, padx=8, pady=4)

        # Transient list
        tk.Label(self.sidebar, text="TRANSIENT WINDOWS", fg=COLORS['dim'], bg=COLORS['panel'],
                font=('Segoe UI', 9, 'bold')).pack(anchor='w', padx=12, pady=(10,4))

        self.transient_list = tk.Frame(self.sidebar, bg=COLORS['panel'])
        self.transient_list.pack(fill='x', padx=8, pady=(0,8))

        # ── Main plot area ──
        self.plot_frame = tk.Frame(self.root, bg=COLORS['bg'])
        self.plot_frame.pack(fill='both', expand=True)

        self.fig = Figure(facecolor=COLORS['bg'], dpi=100)
        self.canvas = FigureCanvasTkAgg(self.fig, master=self.plot_frame)
        self.canvas.get_tk_widget().pack(fill='both', expand=True)

        # Toolbar
        toolbar_frame = tk.Frame(self.plot_frame, bg=COLORS['bg'])
        toolbar_frame.pack(fill='x')
        self.toolbar = NavigationToolbar2Tk(self.canvas, toolbar_frame)
        self.toolbar.update()

        self._draw_empty()

    def _make_button(self, parent, text, command, color='#3388ff'):
        btn = tk.Button(parent, text=text, command=command,
                       font=('Segoe UI', 9, 'bold'),
                       fg='white', bg=COLORS['btn_bg'],
                       activebackground=COLORS['btn_hover'],
                       activeforeground='white',
                       bd=0, padx=12, pady=5, cursor='hand2',
                       relief='flat', highlightthickness=1,
                       highlightbackground=color, highlightcolor=color)
        btn.bind('<Enter>', lambda e: btn.config(bg=COLORS['btn_hover']))
        btn.bind('<Leave>', lambda e: btn.config(bg=COLORS['btn_bg']))
        return btn

    def _set_status(self, text):
        self.status_label.config(text=text)
        self.root.update_idletasks()

    def _load_original(self):
        path = filedialog.askopenfilename(
            title="Select Original Audio File",
            filetypes=[
                ("Audio files", "*.mp3 *.flac *.wav *.ogg *.m4a *.aac *.wma"),
                ("All files", "*.*")
            ]
        )
        if not path:
            return

        self._set_status(f"Loading original: {Path(path).name}...")
        try:
            samples, sr = load_audio(path)
            self.original = {
                'path': path,
                'samples': samples,
                'sr': sr,
                'name': Path(path).name,
            }
            self._set_status(f"Original loaded: {Path(path).name} ({len(samples)/sr:.1f}s @ {sr}Hz)")
            self._detect_windows()
            self._refresh_file_list()
            self._refresh_plot()
        except Exception as ex:
            self._set_status(f"Error loading: {ex}")

    def _load_comparison(self):
        if not self.original:
            self._set_status("Load the original file first!")
            return

        paths = filedialog.askopenfilenames(
            title="Select Comparison File(s)",
            filetypes=[
                ("Audio files", "*.mp3 *.flac *.wav *.ogg *.m4a"),
                ("All files", "*.*")
            ]
        )
        if not paths:
            return

        for path in paths:
            self._set_status(f"Loading: {Path(path).name}...")
            try:
                samples, sr = load_audio(path)
                sidecar = find_sidecar(path)
                self.comparisons.append({
                    'path': path,
                    'samples': samples,
                    'sr': sr,
                    'name': Path(path).name,
                    'sidecar': sidecar,
                })
                tag = " [+sidecar]" if sidecar else ""
                self._set_status(f"Loaded: {Path(path).name} ({len(samples)/sr:.1f}s @ {sr}Hz){tag}")
            except Exception as ex:
                self._set_status(f"Error loading {Path(path).name}: {ex}")

        self._refresh_file_list()
        self._refresh_plot()

    def _clear_all(self):
        self.original = None
        self.comparisons = []
        self.transient_windows = []
        self._refresh_file_list()
        self._draw_empty()
        self._set_status("Cleared. Load an original file to begin.")

    def _detect_windows(self):
        """Auto-detect 4 best transient locations from the original."""
        if not self.original:
            return
        samps = self.original['samples']
        sr = self.original['sr']
        transients = detect_transients(samps, sr, num_points=12)

        # Pick 4 spread-out transients
        transients.sort(key=lambda t: t['time_s'])
        track_len_s = len(samps) / sr
        min_gap = track_len_s / 6

        selected = []
        for t in transients:
            if not selected or (t['time_s'] - selected[-1]['time_s']) > min_gap:
                selected.append(t)
            if len(selected) >= 4:
                break

        # Fill remaining with strongest unused
        if len(selected) < 4:
            for t in sorted(transients, key=lambda t: -t['strength']):
                if t not in selected:
                    selected.append(t)
                if len(selected) >= 4:
                    break

        selected.sort(key=lambda t: t['time_s'])
        self.transient_windows = selected[:4]
        self._refresh_transient_list()

    def _refresh_file_list(self):
        for w in self.file_list.winfo_children():
            w.destroy()

        if self.original:
            f = tk.Frame(self.file_list, bg='#0a2010', highlightbackground=COLORS['green'],
                        highlightthickness=1)
            f.pack(fill='x', pady=2)
            tk.Label(f, text="● ORIGINAL", fg=COLORS['green'], bg='#0a2010',
                    font=('Segoe UI', 8, 'bold')).pack(anchor='w', padx=8, pady=(4,0))
            tk.Label(f, text=self.original['name'], fg=COLORS['text'], bg='#0a2010',
                    font=('Segoe UI', 8), wraplength=250).pack(anchor='w', padx=8, pady=(0,4))

        for i, comp in enumerate(self.comparisons):
            tag = " [sidecar]" if comp.get('sidecar') else ""
            f = tk.Frame(self.file_list, bg='#0a1020', highlightbackground=COLORS['proc'],
                        highlightthickness=1)
            f.pack(fill='x', pady=2)
            tk.Label(f, text=f"● VERSION {i+1}{tag}", fg=COLORS['proc'], bg='#0a1020',
                    font=('Segoe UI', 8, 'bold')).pack(anchor='w', padx=8, pady=(4,0))
            tk.Label(f, text=comp['name'], fg=COLORS['text'], bg='#0a1020',
                    font=('Segoe UI', 8), wraplength=250).pack(anchor='w', padx=8, pady=(0,4))

    def _refresh_transient_list(self):
        for w in self.transient_list.winfo_children():
            w.destroy()
        for i, t in enumerate(self.transient_windows):
            txt = f"#{i+1}  {t['time_s']:.2f}s  (str: {t['strength']:.2f})"
            tk.Label(self.transient_list, text=txt, fg=COLORS['dim'], bg=COLORS['panel'],
                    font=('Consolas', 8), anchor='w').pack(anchor='w', padx=4)

    def _on_window_change(self):
        try:
            self.window_ms = int(self.win_var.get())
        except:
            self.window_ms = 40
        self._refresh_plot()

    def _draw_empty(self):
        self.fig.clear()
        ax = self.fig.add_subplot(111)
        ax.set_facecolor(COLORS['bg'])
        ax.text(0.5, 0.5,
               "Load Original -> then Add Comparisons\n\n"
               "[+]  Original = reference track (green waveform)\n"
               "[+]  Comparisons = processed versions (blue waveform)\n\n"
               "The tool auto-detects 4 transient locations and shows\n"
               "how Hybrid-Phase Blending switches between\n"
               "Linear Phase (wide stage) / Minimum Phase (no pre-ring)",
               transform=ax.transAxes, ha='center', va='center',
               fontsize=12, color=COLORS['dim'], family='Segoe UI',
               linespacing=1.8)
        ax.set_xticks([])
        ax.set_yticks([])
        for spine in ax.spines.values():
            spine.set_visible(False)
        self.canvas.draw()

    def _refresh_plot(self):
        if not self.original or not self.comparisons:
            self._draw_empty()
            return

        if not self.transient_windows:
            self._detect_windows()

        self._set_status("Rendering comparison plots...")

        self.fig.clear()
        num_comp = len(self.comparisons)
        num_win = len(self.transient_windows)
        if num_win == 0:
            self._draw_empty()
            return

        axes = self.fig.subplots(num_comp, num_win, squeeze=False)
        self.fig.subplots_adjust(left=0.04, right=0.98, top=0.92, bottom=0.06,
                                 hspace=0.35, wspace=0.15)

        orig_samples = self.original['samples']
        orig_sr = self.original['sr']

        for ci, comp in enumerate(self.comparisons):
            comp_samples = comp['samples']
            comp_sr = comp['sr']
            sidecar = comp.get('sidecar')

            # Resample original to comparison rate
            if orig_sr != comp_sr:
                orig_at_comp_sr = resample_match(orig_samples, orig_sr, comp_sr)
            else:
                orig_at_comp_sr = orig_samples

            # Build envelope from sidecar
            envelope = None
            if sidecar:
                if 'envelope' in sidecar and 'envelope_sr' in sidecar:
                    # v2: direct envelope from Rust analysis (100Hz)
                    env_data = np.array(sidecar['envelope'], dtype=np.float64)
                    env_sr = float(sidecar['envelope_sr'])
                    # Upsample to comparison sample rate via linear interpolation
                    target_len = len(comp_samples)
                    env_time = np.arange(len(env_data)) / env_sr
                    target_time = np.arange(target_len) / comp_sr
                    envelope = np.interp(target_time, env_time, env_data)
                    envelope = np.clip(envelope, 0, 1)
                elif 'transients' in sidecar:
                    # Legacy v1: build from discrete transients
                    t_data = sidecar.get('transients', [])
                    uf = sidecar.get('upsample_factor', max(1, comp_sr // orig_sr))
                    t_remapped = []
                    for t in t_data:
                        tr = dict(t)
                        tr['sample'] = int(t['sample'] * uf)
                        t_remapped.append(tr)
                    envelope = build_envelope(t_remapped, len(comp_samples), comp_sr,
                                             hold_ms=sidecar.get('hold_ms', 15.0),
                                             release_ms=sidecar.get('release_ms', 30.0),
                                             pre_onset_ms=sidecar.get('pre_onset_ms', 5.0),
                                             attack_ms=sidecar.get('attack_ms', 0))

            for wi, tw in enumerate(self.transient_windows):
                ax = axes[ci][wi]
                ax.set_facecolor(COLORS['bg'])

                # Window bounds in comparison sample space
                center = int(tw['sample'] * comp_sr / orig_sr)
                half = int(self.window_ms / 1000.0 * comp_sr / 2)
                start = max(0, center - half)
                end = min(len(comp_samples), center + half)

                if start >= end:
                    ax.text(0.5, 0.5, 'Out of range', transform=ax.transAxes,
                           ha='center', va='center', color=COLORS['dim'], fontsize=9)
                    continue

                t_axis = np.arange(start, end) / comp_sr * 1000.0  # ms

                # Original waveform (green)
                o_start = min(start, len(orig_at_comp_sr))
                o_end = min(end, len(orig_at_comp_sr))
                if o_start < o_end:
                    o_slice = orig_at_comp_sr[o_start:o_end]
                    t_orig = np.arange(o_start, o_end) / comp_sr * 1000.0
                    ax.plot(t_orig, o_slice, color=COLORS['orig'],
                           linewidth=0.5, alpha=0.6, zorder=2)

                # Processed waveform (blue)
                p_slice = comp_samples[start:end]
                t_proc = t_axis[:len(p_slice)]
                ax.plot(t_proc, p_slice, color=COLORS['proc'],
                       linewidth=0.7, alpha=0.9, zorder=3)

                # Envelope + phase markers (binary hard switch visualization)
                if envelope is not None and end <= len(envelope):
                    env_s = envelope[start:end]
                    t_e = t_axis[:len(env_s)]

                    # Binary threshold matching Rust hard switch
                    switch_threshold = 0.3
                    binary_env = (env_s >= switch_threshold).astype(np.float64)

                    y_max = max(np.abs(p_slice).max(), 0.001)
                    env_vis = binary_env * y_max * 0.92

                    # Fill: solid pink for min-phase zones (binary, no gradient)
                    ax.fill_between(t_e, -env_vis, env_vis,
                                   alpha=0.12, color=COLORS['envelope'], zorder=1)

                    # Phase switch markers (vertical lines at transitions)
                    for k in range(1, len(binary_env)):
                        if binary_env[k-1] < 0.5 and binary_env[k] >= 0.5:
                            ax.axvline(t_e[k], color=COLORS['min_mark'],
                                      lw=1.2, alpha=0.9, ls='-', zorder=4)
                        if binary_env[k-1] >= 0.5 and binary_env[k] < 0.5:
                            ax.axvline(t_e[k], color=COLORS['lin_mark'],
                                      lw=1.0, alpha=0.8, ls='-', zorder=4)

                    # Stats label
                    min_pct = np.mean(binary_env) * 100
                    num_switches = np.sum(np.abs(np.diff(binary_env)) > 0.5)
                    if min_pct > 0.5:
                        label = f"Min:{min_pct:.0f}% ({int(num_switches)} sw)"
                        ax.text(0.5, 0.02, label, transform=ax.transAxes,
                               ha='center', fontsize=7, color=COLORS['envelope'],
                               fontweight='bold',
                               bbox=dict(boxstyle='round,pad=0.2',
                                        fc='black', alpha=0.7, ec=COLORS['envelope'],
                                        lw=0.5))
                    else:
                        ax.text(0.5, 0.02, "100% Linear Phase", transform=ax.transAxes,
                               ha='center', fontsize=7, color=COLORS['lin_mark'],
                               alpha=0.7)
                elif envelope is None:
                    ax.text(0.5, 0.02, "No sidecar data", transform=ax.transAxes,
                           ha='center', fontsize=7, color=COLORS['dim'], alpha=0.6)

                # Axes styling
                ax.set_xlim(t_proc[0], t_proc[-1])
                ax.grid(True, alpha=0.1, color=COLORS['border'])
                ax.tick_params(colors=COLORS['dim'], labelsize=6)
                for sp in ax.spines.values():
                    sp.set_color(COLORS['border'])
                    sp.set_linewidth(0.5)

                if wi == 0:
                    short = comp['name']
                    if len(short) > 35:
                        short = short[:32] + '…'
                    ax.set_ylabel(short, color=COLORS['text'],
                                fontsize=7, fontweight='bold')

                if ci == 0:
                    ax.set_title(f"Transient @ {tw['time_s']:.2f}s",
                               color=COLORS['text'], fontsize=9,
                               fontweight='bold', pad=6)

                if ci == num_comp - 1:
                    ax.set_xlabel('ms', color=COLORS['dim'], fontsize=7)

        # Legend on first axes
        if num_comp > 0 and num_win > 0:
            ax0 = axes[0][num_win - 1]
            from matplotlib.lines import Line2D
            legend_elements = [
                Line2D([0], [0], color=COLORS['orig'], lw=1.5, label='Original'),
                Line2D([0], [0], color=COLORS['proc'], lw=1.5, label='Processed'),
                Line2D([0], [0], color=COLORS['min_mark'], lw=1, ls='--', label='> Min Phase'),
                Line2D([0], [0], color=COLORS['lin_mark'], lw=1, ls=':', label='> Lin Phase'),
            ]
            ax0.legend(handles=legend_elements, loc='upper right', fontsize=6,
                      framealpha=0.6, facecolor='black', edgecolor=COLORS['border'],
                      labelcolor=COLORS['text'])

        self.canvas.draw()
        self._set_status(f"Showing {num_comp} comparison(s) × {num_win} transient windows  |  "
                        f"Window: {self.window_ms}ms  |  Use toolbar to zoom/pan")


# ═══════════════════════════════════════════════════════════
# Entry point
# ═══════════════════════════════════════════════════════════

def main():
    root = tk.Tk()

    # Dark window style
    style = ttk.Style()
    style.theme_use('clam')
    style.configure('TCombobox', fieldbackground=COLORS['btn_bg'],
                   background=COLORS['btn_bg'], foreground='white',
                   arrowcolor='white')

    try:
        root.iconbitmap(default='')
    except:
        pass

    app = CompareApp(root)
    root.mainloop()


if __name__ == '__main__':
    main()
