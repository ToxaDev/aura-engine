#!/usr/bin/env python3
"""
===================================================================
AuraEngine — Adaptive Source Analyzer
===================================================================

2-Pass offline analysis engine:

  Pass 1: ADC Ringing Detection (Adaptive Apodizer)
    - Finds strongest transients via energy-based onset detection
    - Analyzes pre-onset spectral content in 18-22 kHz zone
    - Determines optimal apodizing cutoff per-file

  Pass 2: Transient Map (Hybrid-Phase Blending)
    - Exports transient positions + strengths for Rust converter
    - Computes per-transient envelope for phase morphing

Output: JSON analysis report for optimize.py and Rust converter.

Dependencies: numpy, soundfile (via requirements.txt)
"""

import os
import sys
import json
import argparse
import numpy as np
from pathlib import Path

try:
    import soundfile as sf
    HAS_SOUNDFILE = True
except ImportError:
    HAS_SOUNDFILE = False


# ===================================================================
# Audio Loading
# ===================================================================

def load_audio_mono(path, target_sr=None):
    """Load audio file, convert to mono float64."""
    if not HAS_SOUNDFILE:
        raise RuntimeError("soundfile is required: pip install soundfile")
    
    data, sr = sf.read(path, dtype='float64', always_2d=True)
    # Mix to mono
    mono = np.mean(data, axis=1)
    return mono, sr


# ===================================================================
# Onset Detection (energy-based, no librosa dependency)
# ===================================================================

def compute_onset_envelope(y, sr, hop_size=512):
    """
    Energy-based onset strength envelope.
    
    Uses spectral flux (increase in spectral energy) which is more
    robust than simple energy for detecting percussive onsets.
    """
    n_fft = 2048
    # Compute STFT magnitudes
    n_frames = 1 + (len(y) - n_fft) // hop_size
    if n_frames <= 0:
        return np.array([]), np.array([])
    
    envelope = np.zeros(n_frames)
    prev_mag = None
    
    window = np.hanning(n_fft)
    
    for i in range(n_frames):
        start = i * hop_size
        frame = y[start:start + n_fft] * window
        mag = np.abs(np.fft.rfft(frame))
        
        if prev_mag is not None:
            # Spectral flux: sum of positive magnitude differences
            diff = mag - prev_mag
            envelope[i] = np.sum(np.maximum(diff, 0.0))
        
        prev_mag = mag
    
    # Normalize
    if envelope.max() > 0:
        envelope /= envelope.max()
    
    # Frame times in samples
    frame_samples = np.arange(n_frames) * hop_size + n_fft // 2
    
    return envelope, frame_samples


def find_top_onsets(envelope, frame_samples, top_n=10, min_distance_frames=10, threshold=0.3):
    """
    Find top N strongest onsets with minimum distance constraint.
    Returns onset sample positions sorted by strength (descending).
    """
    if len(envelope) == 0:
        return []
    
    # Find peaks above threshold
    candidates = []
    for i in range(1, len(envelope) - 1):
        if envelope[i] > threshold:
            if envelope[i] > envelope[i-1] and envelope[i] >= envelope[i+1]:
                candidates.append((envelope[i], frame_samples[i], i))
    
    # Sort by strength descending
    candidates.sort(key=lambda x: -x[0])
    
    # Apply minimum distance constraint (greedy)
    selected = []
    used_frames = set()
    
    for strength, sample_pos, frame_idx in candidates:
        if len(selected) >= top_n:
            break
        
        # Check distance to already selected
        too_close = False
        for _, _, existing_frame in selected:
            if abs(frame_idx - existing_frame) < min_distance_frames:
                too_close = True
                break
        
        if not too_close:
            selected.append((strength, int(sample_pos), frame_idx))
    
    return selected


# ===================================================================
# ADC Ringing Detection (Adaptive Apodizer — Pass 1)
# ===================================================================

def analyze_pre_ringing_spectrum(y, sr, onset_sample, pre_window_ms=2.0, n_fft_analysis=8192):
    """
    Analyze the spectral content STRICTLY BEFORE a transient onset.
    
    This pre-onset window contains ADC anti-aliasing filter ringing.
    We look for spectral peaks in the 18-22 kHz range to identify
    the ADC's characteristic ringing frequency.
    
    Returns: (peak_frequency_hz, peak_magnitude_db, confidence)
    """
    pre_samples = int(pre_window_ms * sr / 1000.0)
    
    # Extract pre-onset window
    start = max(0, onset_sample - pre_samples)
    end = onset_sample
    
    if end - start < 64:  # Too short
        return None, None, 0.0
    
    window_data = y[start:end]
    
    # Apply Hanning window
    win = np.hanning(len(window_data))
    windowed = window_data * win
    
    # High-resolution FFT
    spectrum = np.fft.rfft(windowed, n=n_fft_analysis)
    mag = np.abs(spectrum)
    freqs = np.fft.rfftfreq(n_fft_analysis, 1.0 / sr)
    
    # Focus on 18 kHz — Nyquist range (ADC ringing zone)
    nyquist = sr / 2.0
    lo_hz = 18000.0
    hi_hz = min(nyquist - 100, nyquist)  # Leave margin
    
    mask = (freqs >= lo_hz) & (freqs <= hi_hz)
    if not np.any(mask):
        return None, None, 0.0
    
    hf_freqs = freqs[mask]
    hf_mag = mag[mask]
    
    # Also get the overall background level (1-10 kHz) for normalization
    bg_mask = (freqs >= 1000) & (freqs <= 10000)
    bg_level = np.median(mag[bg_mask]) if np.any(bg_mask) else 1e-30
    
    # Peak in HF zone
    peak_idx = np.argmax(hf_mag)
    peak_freq = hf_freqs[peak_idx]
    peak_mag = hf_mag[peak_idx]
    
    # Confidence: how much the HF peak stands out above background
    ratio = peak_mag / (bg_level + 1e-30)
    peak_db = 20.0 * np.log10(max(ratio, 1e-30))
    
    # If HF energy is significantly above noise floor, it's likely ADC ringing
    confidence = min(max((peak_db + 20.0) / 40.0, 0.0), 1.0)  # Normalize to [0, 1]
    
    return peak_freq, peak_db, confidence


def adaptive_apodizer_analysis(y, sr, config=None):
    """
    Full Adaptive Apodizer analysis.
    
    1. Find top transients
    2. Analyze pre-ringing spectrum for each
    3. Determine optimal cutoff frequency
    
    Returns analysis dict for optimize.py
    """
    if config is None:
        config = {}
    
    pre_window_ms = config.get('pre_window_ms', 2.0)
    target_suppression_db = config.get('target_suppression_db', -120)
    min_cutoff_hz = config.get('min_cutoff_hz', 18000)
    max_cutoff_hz = config.get('max_cutoff_hz', 21500)
    top_n = config.get('top_n_transients', 10)
    
    print(f"\n  [Adaptive Apodizer] Analyzing source ({len(y)/sr:.1f}s @ {sr} Hz)...")
    
    # Step 1: Find transients
    hop = 512
    envelope, frame_samples = compute_onset_envelope(y, sr, hop)
    onsets = find_top_onsets(envelope, frame_samples, top_n=top_n)
    
    if not onsets:
        print(f"  [Adaptive Apodizer] No transients found. Using default cutoff.")
        return {
            'detected_ringing_hz': None,
            'optimal_cutoff_hz': 20000,
            'num_transients_analyzed': 0,
            'confidence': 0.0,
            'per_transient': [],
            'method': 'default (no transients found)',
        }
    
    print(f"  [Adaptive Apodizer] Found {len(onsets)} transients. Analyzing pre-ringing...")
    
    # Step 2: Analyze pre-ringing for each transient
    ringing_data = []
    for strength, onset_sample, _ in onsets:
        peak_freq, peak_db, confidence = analyze_pre_ringing_spectrum(
            y, sr, onset_sample, pre_window_ms
        )
        if peak_freq is not None and confidence > 0.3:
            ringing_data.append({
                'onset_sample': onset_sample,
                'onset_time_s': onset_sample / sr,
                'onset_strength': float(strength),
                'ringing_freq_hz': float(peak_freq),
                'ringing_db': float(peak_db),
                'confidence': float(confidence),
            })
            print(f"    Transient @ {onset_sample/sr:.3f}s: "
                  f"ringing at {peak_freq:.0f} Hz ({peak_db:+.1f} dB), "
                  f"confidence={confidence:.2f}")
    
    # Step 3: Determine optimal cutoff
    if ringing_data:
        # Weighted median of detected ringing frequencies (weight by confidence)
        freqs = np.array([d['ringing_freq_hz'] for d in ringing_data])
        weights = np.array([d['confidence'] for d in ringing_data])
        
        # Weighted median
        sorted_idx = np.argsort(freqs)
        cumw = np.cumsum(weights[sorted_idx])
        median_idx = np.searchsorted(cumw, cumw[-1] / 2.0)
        detected_ringing = freqs[sorted_idx[median_idx]]
        
        avg_confidence = float(np.mean(weights))
        
        # Calculate cutoff: want >= target_suppression_db at detected_ringing
        # For Kaiser β=14 with 10M taps, the transition width is very narrow.
        # We set cutoff sufficiently below the ringing frequency.
        # Rule of thumb: 800-1200 Hz margin depending on tap count
        margin_hz = 800  # Conservative margin
        optimal_cutoff = detected_ringing - margin_hz
        
        # Clamp to allowed range
        optimal_cutoff = max(min_cutoff_hz, min(optimal_cutoff, max_cutoff_hz))
        
        print(f"\n  [Adaptive Apodizer] =======================================")
        print(f"  [Adaptive Apodizer] Detected studio ADC ringing at {detected_ringing:.0f} Hz")
        print(f"  [Adaptive Apodizer] Generating custom filter with roll-off at {optimal_cutoff:.0f} Hz")
        print(f"  [Adaptive Apodizer] Confidence: {avg_confidence:.1%} ({len(ringing_data)}/{len(onsets)} transients)")
        print(f"  [Adaptive Apodizer] =======================================")
    else:
        detected_ringing = None
        optimal_cutoff = 20000
        avg_confidence = 0.0
        print(f"  [Adaptive Apodizer] No significant HF ringing detected. Using default 20 kHz cutoff.")
    
    return {
        'detected_ringing_hz': float(detected_ringing) if detected_ringing else None,
        'optimal_cutoff_hz': float(optimal_cutoff),
        'num_transients_analyzed': len(ringing_data),
        'num_transients_total': len(onsets),
        'confidence': float(avg_confidence),
        'per_transient': ringing_data,
        'method': 'adaptive' if ringing_data else 'default',
    }


# ===================================================================
# Transient Map (Hybrid-Phase Blending — Pass 2)
# ===================================================================

def compute_transient_map(y, sr, config=None):
    """
    Generate a transient map for Hybrid-Phase Blending.
    
    For each detected transient, record:
    - Position (in samples, at source sample rate)
    - Strength (0.0 - 1.0)
    - Recommended alpha (phase blend factor)
    
    The Rust converter will use this map to:
    1. Delay-compensate minimum phase output
    2. Generate cos² crossfade envelope
    3. Blend linear + minimum phase outputs
    
    Returns transient map dict for Rust converter.
    """
    if config is None:
        config = {}
    
    attack_ms = config.get('attack_ms', 3.0)
    release_ms = config.get('release_ms', 20.0)
    top_n = config.get('top_n_transients', 50)  # More transients for phase blending
    threshold = config.get('threshold', 0.15)  # Lower threshold to catch more
    
    print(f"\n  [Hybrid-Phase] Scanning transients for phase blending...")
    
    # Find transients
    hop = 256  # Higher resolution for phase blending
    envelope, frame_samples = compute_onset_envelope(y, sr, hop)
    onsets = find_top_onsets(
        envelope, frame_samples,
        top_n=top_n,
        min_distance_frames=8,
        threshold=threshold
    )
    
    if not onsets:
        print(f"  [Hybrid-Phase] No transients found.")
        return {
            'transients': [],
            'onset_density_per_sec': 0.0,
            'recommended_static_alpha': 1.0,
            'attack_ms': attack_ms,
            'release_ms': release_ms,
        }
    
    # Build transient list
    duration_s = len(y) / sr
    transients = []
    
    for strength, onset_sample, _ in onsets:
        transients.append({
            'sample': int(onset_sample),
            'time_s': float(onset_sample / sr),
            'strength': float(strength),
        })
    
    # Sort by time
    transients.sort(key=lambda t: t['sample'])
    
    onset_density = len(transients) / duration_s if duration_s > 0 else 0
    
    # Compute recommended static alpha (fallback if hybrid blending is disabled)
    # High density → full minimum phase; Low density → more linear
    alpha = min(0.7 + 0.3 * min(onset_density / 5.0, 1.0), 1.0)
    
    print(f"  [Hybrid-Phase] Found {len(transients)} transients "
          f"({onset_density:.1f}/s)")
    print(f"  [Hybrid-Phase] Recommended static alpha: {alpha:.2f}")
    print(f"  [Hybrid-Phase] Attack: {attack_ms}ms, Release: {release_ms}ms")
    
    return {
        'transients': transients,
        'onset_density_per_sec': float(onset_density),
        'recommended_static_alpha': float(round(alpha, 2)),
        'attack_ms': float(attack_ms),
        'release_ms': float(release_ms),
        'source_sample_rate': int(sr),
    }


# ===================================================================
# Full Analysis Pipeline
# ===================================================================

def analyze_file(audio_path, output_path=None, config=None):
    """
    Run full analysis pipeline on a source audio file.
    
    Produces a JSON report with:
    1. Adaptive Apodizer data (optimal cutoff)
    2. Transient Map (for Hybrid-Phase Blending)
    """
    if config is None:
        config = {}
    
    audio_path = Path(audio_path)
    if not audio_path.exists():
        print(f"  [ERROR] File not found: {audio_path}")
        sys.exit(1)
    
    print(f"\n{'=' * 64}")
    print(f"  AuraEngine — Adaptive Source Analyzer")
    print(f"{'=' * 64}")
    print(f"  Input: {audio_path.name}")
    
    # Load audio
    y, sr = load_audio_mono(str(audio_path))
    print(f"  Duration: {len(y)/sr:.1f}s, Sample Rate: {sr} Hz")
    print(f"  Samples: {len(y):,}")
    
    result = {
        'source_file': str(audio_path.resolve()),
        'source_sample_rate': int(sr),
        'source_duration_s': float(len(y) / sr),
        'source_samples': len(y),
    }
    
    # Pass 1: Adaptive Apodizer
    apodizer_config = config.get('adaptive_apodizer', {})
    if config.get('enable_adaptive_apodizer', True):
        result['adaptive_apodizer'] = adaptive_apodizer_analysis(y, sr, apodizer_config)
    
    # Pass 2: Transient Map
    hybrid_config = config.get('hybrid_phase', {})
    if config.get('enable_hybrid_phase', True):
        result['transient_map'] = compute_transient_map(y, sr, hybrid_config)
    
    # Write output
    if output_path is None:
        output_path = audio_path.parent / f"{audio_path.stem}_analysis.json"
    
    output_path = Path(output_path)
    with open(output_path, 'w') as f:
        json.dump(result, f, indent=2)
    
    print(f"\n  Output: {output_path}")
    print(f"{'=' * 64}\n")
    
    return result


# ===================================================================
# CLI
# ===================================================================

def main():
    parser = argparse.ArgumentParser(
        description='AuraEngine Adaptive Source Analyzer',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  python analyze_source.py track.flac
  python analyze_source.py track.wav --output analysis.json
  python analyze_source.py track.flac --no-apodizer
  python analyze_source.py track.flac --no-hybrid-phase
        """
    )
    parser.add_argument('input', help='Input audio file (FLAC, WAV, etc.)')
    parser.add_argument('--output', '-o', help='Output JSON path (default: <input>_analysis.json)')
    parser.add_argument('--no-apodizer', action='store_true', help='Skip Adaptive Apodizer analysis')
    parser.add_argument('--no-hybrid-phase', action='store_true', help='Skip Hybrid-Phase transient map')
    parser.add_argument('--pre-window-ms', type=float, default=2.0, help='Pre-ringing analysis window (ms)')
    parser.add_argument('--attack-ms', type=float, default=3.0, help='Phase blend attack time (ms)')
    parser.add_argument('--release-ms', type=float, default=20.0, help='Phase blend release time (ms)')
    parser.add_argument('--top-n', type=int, default=10, help='Max transients for ADC analysis')
    
    args = parser.parse_args()
    
    config = {
        'enable_adaptive_apodizer': not args.no_apodizer,
        'enable_hybrid_phase': not args.no_hybrid_phase,
        'adaptive_apodizer': {
            'pre_window_ms': args.pre_window_ms,
            'top_n_transients': args.top_n,
        },
        'hybrid_phase': {
            'attack_ms': args.attack_ms,
            'release_ms': args.release_ms,
        },
    }
    
    analyze_file(args.input, args.output, config)


if __name__ == '__main__':
    main()
