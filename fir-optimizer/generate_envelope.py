#!/usr/bin/env python3
"""
===================================================================
  AuraEngine -- Hybrid-Phase Envelope Generator (librosa HPSS)
===================================================================

  Generates a high-quality onset envelope for the Hybrid-Phase engine
  using librosa's HPSS (Harmonic-Percussive Source Separation) +
  dual onset detection.

  Usage:
    python generate_envelope.py <source.flac>
    python generate_envelope.py <source.flac> --output <path.json>

  Output:
    <source_stem>.onset_envelope.json — compatible with the Rust converter.

  Algorithm:
    1. HPSS — separate percussive from harmonic content
    2. Dual onset detection:
       a) Percussive onset (drums, clicks, hi-hats)
       b) Full-signal onset (string plucks, piano, bass, vocals)
    3. Merge with percussive priority (×1.5 weight)
    4. Forward envelope follower (instant attack, 25ms hold, 8ms release)
    5. Backward lookahead (15ms pre-onset with cos² fade)
    6. Downsample to 100Hz → JSON sidecar

  Dependencies:
    pip install librosa numpy soundfile
"""

import sys
import json

# Force UTF-8 stdout on Windows (avoids cp1251 UnicodeEncodeError)
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
if hasattr(sys.stderr, "reconfigure"):
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
import argparse
import numpy as np
from pathlib import Path

try:
    import librosa
except ImportError:
    print("[ERROR] librosa not installed. Run: pip install librosa")
    sys.exit(1)

try:
    import soundfile as sf
except ImportError:
    print("[ERROR] soundfile not installed. Run: pip install soundfile")
    sys.exit(1)


# ═══════════════════════════════════════════════════════════════════
# Configuration — mirrors hybrid_phase.rs parameters
# ═══════════════════════════════════════════════════════════════════

ANALYSIS_SR = 1000.0       # Internal analysis rate (1 frame per ms)
OUTPUT_ENVELOPE_SR = 100.0 # Sidecar output rate (100 Hz)
HOLD_MS = 25.0             # Hold time after onset (covers transient body)
RELEASE_MS = 8.0           # Exponential release — fast to avoid comb filtering
PRE_ONSET_MS = 15.0        # Backward lookahead (pre-ring protection)
CROSSFADE_MS = 3.0         # cos² fade-in duration
NOISE_GATE_MIN = 0.01      # Zero out values below this
PERC_WEIGHT = 1.5          # Percussive onset priority multiplier


def load_audio_mono(path: str) -> tuple:
    """Load audio as mono float32, return (samples, sample_rate)."""
    y, sr = librosa.load(path, sr=None, mono=True)
    return y, sr


def compute_dual_onset_envelope(y: np.ndarray, sr: int) -> np.ndarray:
    """
    Dual onset detection: percussive (HPSS) + full-signal.

    Returns onset strength envelope at librosa's default hop rate.
    """
    print("  [1/5] HPSS: separating percussive component...")
    y_harm, y_perc = librosa.effects.hpss(y)

    perc_energy = np.sum(y_perc ** 2)
    harm_energy = np.sum(y_harm ** 2)
    print(f"        Percussive energy: {perc_energy:.2f}")
    print(f"        Harmonic energy:   {harm_energy:.2f}")
    print(f"        P/H ratio:         {perc_energy / max(harm_energy, 1e-10):.3f}")

    print("  [2/5] Dual onset detection...")

    # Onset on percussive component — catches drums, clicks
    hop_length = 512
    onset_perc = librosa.onset.onset_strength(
        y=y_perc, sr=sr, hop_length=hop_length,
    )

    # Onset on full signal — catches string plucks, piano, bass, vocal attacks
    onset_full = librosa.onset.onset_strength(
        y=y, sr=sr, hop_length=hop_length,
    )

    # Merge: percussive gets priority (×1.5), take max of both
    onset_combined = np.maximum(onset_perc * PERC_WEIGHT, onset_full)

    # Normalize to [0, 1]
    omax = onset_combined.max()
    if omax > 0:
        onset_combined /= omax

    # Stats
    n_frames = len(onset_combined)
    frame_rate = sr / hop_length
    active = np.sum(onset_combined > 0.1)
    print(f"        Frames: {n_frames} @ {frame_rate:.1f} Hz")
    print(f"        Active frames (>0.1): {active} ({100 * active / max(n_frames, 1):.1f}%)")

    return onset_combined, frame_rate


def build_envelope(onset_env: np.ndarray, frame_rate: float) -> np.ndarray:
    """
    Build forward/backward envelope from onset strength.
    Mirrors the logic in hybrid_phase.rs:
      - Instant attack
      - Configurable hold + exponential release
      - Backward lookahead with cos² fade
    """
    print("  [3/5] Building envelope (attack/hold/release)...")

    n = len(onset_env)

    # Adaptive noise floor: mean of positive values (same as Rust)
    positive = onset_env[onset_env > 0]
    if len(positive) > 0:
        noise_floor = np.mean(positive)
    else:
        noise_floor = 0.0

    omax = onset_env.max()
    print(f"        Noise floor: {noise_floor:.4f}, max: {omax:.4f}")

    # Apply noise gate + sqrt compression (same as Rust)
    onset_norm = np.zeros(n)
    for i in range(n):
        if onset_env[i] > noise_floor:
            onset_norm[i] = min(1.0, np.sqrt(
                (onset_env[i] - noise_floor) / max(omax - noise_floor, 1e-15)
            ))

    active_count = np.sum(onset_norm > 0)
    print(f"        After gate: {int(active_count)} active frames ({100 * active_count / max(n, 1):.1f}%)")

    # ── Forward pass: instant attack, hold, exponential release ──
    hold_frames = max(1, int(round(HOLD_MS * frame_rate / 1000.0)))
    release_coeff = np.exp(-1.0 / max(RELEASE_MS * frame_rate / 1000.0, 1.0))

    env_forward = np.zeros(n)
    hold_counter = 0

    for i in range(n):
        val = onset_norm[i]
        prev = env_forward[i - 1] if i > 0 else 0.0

        if val > prev:
            env_forward[i] = val
            hold_counter = hold_frames
        elif hold_counter > 0:
            env_forward[i] = prev
            hold_counter -= 1
        else:
            env_forward[i] = prev * release_coeff

    # ── Backward pass: pre-onset lookahead ──
    print("  [4/5] Backward lookahead (pre-onset protection)...")

    pre_frames = max(1, int(round(PRE_ONSET_MS * frame_rate / 1000.0)))
    fade_frames = max(1, int(round(CROSSFADE_MS * frame_rate / 1000.0)))

    env_final = env_forward.copy()

    for i in range(n - 1, -1, -1):
        for j in range(1, pre_frames + 1):
            future = i + j
            if future >= n:
                break

            future_val = env_forward[future]
            if future_val > env_final[i]:
                if j <= fade_frames:
                    fade = 0.5 * (1.0 + np.cos(np.pi * j / fade_frames))
                else:
                    decay_t = (j - fade_frames) / pre_frames
                    fade = max(0.0, 0.5 * (1.0 + np.cos(np.pi * decay_t)))
                env_final[i] = max(env_final[i], future_val * fade)

    # Zero out very small values
    env_final[env_final < NOISE_GATE_MIN] = 0.0

    # Smooth with short moving average (±3 frames)
    smooth_frames = 3
    env_smooth = np.zeros(n)
    for i in range(n):
        lo = max(0, i - smooth_frames)
        hi = min(n, i + smooth_frames + 1)
        env_smooth[i] = np.mean(env_final[lo:hi])

    return env_smooth


def downsample_to_100hz(envelope: np.ndarray, frame_rate: float) -> np.ndarray:
    """Downsample envelope from frame_rate to 100Hz."""
    target_sr = OUTPUT_ENVELOPE_SR
    ratio = frame_rate / target_sr
    n_out = int(len(envelope) / ratio)
    result = np.zeros(n_out)
    for i in range(n_out):
        src_pos = i * ratio
        idx = int(src_pos)
        frac = src_pos - idx
        v0 = envelope[idx] if idx < len(envelope) else 0.0
        v1 = envelope[idx + 1] if idx + 1 < len(envelope) else v0
        result[i] = v0 + frac * (v1 - v0)
    return np.clip(result, 0.0, 1.0)


def generate_envelope(audio_path: str, output_path: str = None):
    """Main entry point: generate onset envelope JSON for the Rust converter."""
    src = Path(audio_path)
    if not src.exists():
        print(f"[ERROR] File not found: {audio_path}")
        return False

    if output_path is None:
        output_path = str(src.parent / f"{src.stem}.onset_envelope.json")

    print()
    print("=" * 65)
    print("  AuraEngine -- Onset Envelope Generator (librosa HPSS)")
    print("=" * 65)
    print()
    print(f"  Source: {src.name}")
    print()

    # Load audio
    y, sr = load_audio_mono(str(src))
    duration = len(y) / sr
    print(f"  Loaded: {duration:.1f}s @ {sr}Hz ({len(y):,} samples)")
    print()

    # Dual onset detection
    onset_env, frame_rate = compute_dual_onset_envelope(y, sr)

    # Build envelope
    envelope = build_envelope(onset_env, frame_rate)

    # Downsample to 100Hz for compact sidecar
    print("  [5/5] Downsampling to 100Hz...")
    env_100hz = downsample_to_100hz(envelope, frame_rate)

    # Stats
    active_pct = 100.0 * np.mean(env_100hz > 0.01)
    minphase_pct = 100.0 * np.mean(env_100hz >= 0.3)

    # Count transient regions
    transient_count = 0
    was_active = False
    for v in env_100hz:
        if v > 0.1 and not was_active:
            transient_count += 1
        was_active = v > 0.1

    print(f"        Envelope: {len(env_100hz)} samples @ {OUTPUT_ENVELOPE_SR:.0f}Hz")
    print(f"        Active zones:     {active_pct:.1f}%")
    print(f"        Min-phase zones:  {minphase_pct:.1f}%")
    print(f"        Transient regions: {transient_count}")

    # Save JSON (compatible with Rust converter)
    sidecar = {
        "source_rate": int(sr),
        "algorithm": "librosa_hpss_dual_onset_v1",
        "envelope_sr": int(OUTPUT_ENVELOPE_SR),
        "envelope_length": len(env_100hz),
        "envelope": [round(float(v), 3) for v in env_100hz],
    }

    with open(output_path, 'w') as f:
        # Compact JSON: envelope array on one line
        f.write("{\n")
        f.write(f'  "source_rate": {sidecar["source_rate"]},\n')
        f.write(f'  "algorithm": "{sidecar["algorithm"]}",\n')
        f.write(f'  "envelope_sr": {sidecar["envelope_sr"]},\n')
        f.write(f'  "envelope_length": {sidecar["envelope_length"]},\n')
        f.write('  "envelope": [')
        f.write(','.join(f'{v:.3f}' for v in env_100hz))
        f.write(']\n}\n')

    size_kb = Path(output_path).stat().st_size / 1024
    print()
    print(f"  [OK] Saved: {Path(output_path).name} ({size_kb:.1f} KB)")
    print("=" * 65)
    print()

    return True


# ═══════════════════════════════════════════════════════════════════
# CLI
# ═══════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="AuraEngine — Generate onset envelope using librosa HPSS"
    )
    parser.add_argument("audio", help="Path to source audio file (FLAC/WAV)")
    parser.add_argument("--output", "-o", help="Output JSON path (default: <stem>.onset_envelope.json)")
    args = parser.parse_args()

    success = generate_envelope(args.audio, args.output)
    sys.exit(0 if success else 1)
