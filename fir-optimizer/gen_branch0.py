"""Branch-0 filter files for the multistage upsampler (format aura-branch0 v1).

A rung of 100M taps and more is stored for Multi only: instead of the whole
N-tap fir_<TAG>_<TARGET>_<phase>.npy, three files side by side

    <stem>.b0.npy       h[k*L], k = 0 .. ceil(N/L)-1   (branch 0 at the source rate)
    <stem>.b0.ends.npy  h[0 .. min(N,131072)] ++ h[N-min(N,64) .. N]
    <stem>.b0.json      N, L, rates, phase, sum_bits = the f64 bits of h[0]+h[1]+...+h[N-1]
                        added left to right (what the runtime sums over a whole file)

where h is exactly the filter optimize.generate() would write for that rung.
The runtime reads them in audio/dsp_core.rs (load_npy_f64_branch, Branch0Meta).

Linear phase never holds h: optimize._generate_linear_phase_kaiser_scipy_fast
is evaluated piece by piece with the same element-wise numpy/scipy calls, its
normalising sum is the same pairwise sum np.sum does over the whole array,
and branch 0, the ends and the left-to-right sum come from a second pass.
Minimum phase up to MIN_FULL_MAX_TAPS is the full-rate cepstrum of
optimize.generate_cepstral_phase (same operations, fewer arrays alive at once);
above it, branch 0 comes from minphase_branch0.py.

Usage:
    gen_branch0.py <out_dir> <mult,mult,...> <taps,taps,...> [linear|minimum|both] [44100,48000]
        taps as numbers or tags: 100M,180M,300M,500M,1G
    gen_branch0.py --from-full <fir_..._phase.npy> <L> [out_dir]
        the same three files from an existing whole blob
"""
import json
import math
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import numpy as np
import scipy.fft as sfft
from scipy import special

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import optimize  # noqa: E402

GENERATOR_VERSION = 1
FORMAT, FORMAT_VERSION = "aura-branch0", 1
# dsp_core::GROUP_DELAY_ANALYSIS_TAPS and BRANCH0_TAIL_TAPS.
HEAD_TAPS = 131072
TAIL_TAPS = 64
# The longest minimum-phase filter made from the full-rate cepstrum. Its peak is
# about five float64 arrays of n_fft (100M, n_fft 2^28: 10.7 GB measured), so
# 180M-268M (2^29) take ~22 GB and 300M (2^30) would take ~43 GB — too close to
# what this PC holds. Longer ones go through minphase_branch0.
MIN_FULL_MAX_TAPS = (1 << 29) // 2
# Elements per piece: 32 MB per float64 array.
PIECE = 1 << 22
WORKERS = 8
FAMILIES = (44100, 48000)
PHASES = {"linear": "linear_phase", "minimum": "minimum_phase"}


def log(msg):
    print(msg, flush=True)


# ---------------------------------------------------------------------------
# Linear phase, closed form
# ---------------------------------------------------------------------------

class LinearKaiser:
    """optimize._generate_linear_phase_kaiser_scipy_fast(N, ...) on any index range.

    raw(a, b) is that function's h before normalising, for n = a .. b-1, with
    the same calls in the same order (scipy.signal.windows.kaiser, then
    fc_norm * np.sinc(fc_norm * x), then their product), so every element is
    the same float64 as in the whole array.
    """

    def __init__(self, num_taps, f_pass, f_stop, fs, beta):
        self.N = int(num_taps)
        self.f_pass, self.f_stop, self.fs, self.beta = f_pass, f_stop, fs, beta
        f_cutoff = (f_pass + f_stop) / 2.0
        self.fc_norm = min(f_cutoff / (fs / 2), 0.99)
        self.alpha = (self.N - 1) / 2.0     # the window's centre (kaiser)
        self.center = (self.N - 1) / 2.0    # the sinc's centre
        self.i0_beta = special.i0(np.asarray(beta, dtype=np.float64))
        self.scale = None

    def raw(self, a, b):
        n = np.arange(a, b, dtype=np.float64)
        kaiser = special.i0(self.beta * np.sqrt(1 - ((n - self.alpha) / self.alpha) ** 2.0)) / self.i0_beta
        x = n - self.center
        sinc_part = self.fc_norm * np.sinc(self.fc_norm * x)
        return sinc_part * kaiser

    def normalise(self, pool, leaf=PIECE, order="pairwise"):
        """The divisor: float(np.sum(h)) over the whole raw h (longdouble is float64 here).

        order "pairwise": the sum numpy 2 makes (the 60M-200M blobs of 23.10).
        order "blocks8192": the sum the numpy that made the shipped 5k-30M blobs made —
        pairwise within blocks of 8192, the blocks added one after another.
        """
        if np.finfo(np.longdouble).bits != 64:
            raise RuntimeError("np.longdouble is not float64 on this platform: the pairwise sum below "
                               "reproduces the float64 np.sum only")
        if order == "pairwise":
            self.scale = float(pairwise_sum(self.raw, self.N, pool, leaf))
        elif order == "blocks8192":
            self.scale = blocks_sum(self.raw, self.N, pool)
        else:
            raise ValueError(order)
        self.order = order
        return self.scale

    def h(self, a, b):
        return self.raw(a, b) / self.scale


def _pairwise_leaves(a, n, leaf, out):
    """The ranges numpy's pairwise_sum splits [a, a+n) into, down to ≤ leaf elements."""
    if n <= leaf:
        out.append((a, a + n))
        return
    n2 = n // 2
    n2 -= n2 % 8
    _pairwise_leaves(a, n2, leaf, out)
    _pairwise_leaves(a + n2, n - n2, leaf, out)


def _pairwise_combine(a, n, leaf, sums):
    if n <= leaf:
        return sums[a]
    n2 = n // 2
    n2 -= n2 % 8
    return _pairwise_combine(a, n2, leaf, sums) + _pairwise_combine(a + n2, n - n2, leaf, sums)


def pairwise_sum(values, n, pool, leaf=PIECE):
    """np.sum over a contiguous float64 array of n elements that is never held whole.

    numpy adds a contiguous array by pairwise_sum: halves split at
    n2 = n/2 - (n/2) % 8 down to blocks of 128. The same split is done here
    down to `leaf` elements, and each leaf is summed by numpy itself
    (np.add.reduce with initial 0.0 is exactly pairwise_sum of the leaf).
    values(a, b) gives elements a .. b-1. A leaf can not be shorter than 128:
    numpy adds up to 128 elements in eight interleaved partial sums, not by halves.
    """
    if leaf < 128:
        raise ValueError("pairwise_sum: leaf must be at least 128")
    leaves = []
    _pairwise_leaves(0, n, leaf, leaves)
    part = lambda r: float(np.add.reduce(values(*r), initial=0.0))
    sums = dict(zip((r[0] for r in leaves), pool.map(part, leaves)))
    return _pairwise_combine(0, n, leaf, sums)


def blocks_sum(values, n, pool, block=8192, piece=PIECE):
    """Pairwise within blocks of `block`, the block sums added left to right from 0.0."""
    piece -= piece % block

    def part(r):
        x = values(*r)
        return [float(np.add.reduce(x[i:i + block], initial=0.0)) for i in range(0, len(x), block)]

    total = 0.0
    for sums in pool.map(part, [(a, min(a + piece, n)) for a in range(0, n, piece)]):
        for s in sums:
            total += s
    return total


# ---------------------------------------------------------------------------
# Branch 0, ends and the left-to-right sum from pieces of h
# ---------------------------------------------------------------------------

def branch0_from(get, N, L, pool=None, piece=PIECE):
    """h0 = h[::L], the ends, and Σh of the filter whose slices get(a, b) = h[a:b] gives.

    Σh is h[0] + h[1] + ... added left to right, as Rust's iter().sum() adds it
    (from -0.0): each piece carries the running total into its first element
    and np.cumsum adds the rest in order. Pieces start at multiples of L.
    """
    piece = max(L, piece - piece % L)
    ranges = [(a, min(a + piece, N)) for a in range(0, N, piece)]
    M = -(-N // L)
    h0 = np.empty(M, dtype=np.float64)
    head_n, tail_n = min(N, HEAD_TAPS), min(N, TAIL_TAPS)
    head = np.empty(head_n, dtype=np.float64)
    tail = np.empty(tail_n, dtype=np.float64)
    total = -0.0
    window = max(1, WORKERS) * 2 if pool is not None else 1
    pending = []
    it = iter(ranges)

    def submit():
        r = next(it, None)
        if r is None:
            return
        pending.append((r, pool.submit(get, *r) if pool is not None else None))

    for _ in range(window):
        submit()
    while pending:
        (a, b), fut = pending.pop(0)
        x = fut.result() if fut is not None else get(a, b)
        submit()
        x = np.ascontiguousarray(x, dtype=np.float64)
        assert len(x) == b - a
        h0[a // L:a // L + len(x[::L])] = x[::L]
        if a < head_n:
            head[a:min(b, head_n)] = x[:min(b, head_n) - a]
        if b > N - tail_n:
            lo = max(a, N - tail_n)
            tail[lo - (N - tail_n):b - (N - tail_n)] = x[lo - a:]
        c = x.copy()
        c[0] = total + c[0]
        total = float(np.cumsum(c)[-1])
    return h0, np.concatenate([head, tail]), total


# ---------------------------------------------------------------------------
# Minimum phase
# ---------------------------------------------------------------------------

def cepstral_minimum_phase(h_linear, order="pairwise"):
    """optimize.generate_cepstral_phase(h_linear, alpha=1.0), bit for bit, with fewer
    full-size arrays alive at once (each step is the same numpy/scipy operation on
    the same values; arrays are dropped as soon as the next one exists).
    order: the normalising sum, as in LinearKaiser.normalise.
    h_linear may be a one-element list: the filter is then taken out of it and
    dropped after the first transform."""
    alpha = 1.0
    if isinstance(h_linear, list):
        h_linear = h_linear.pop()
    N = len(h_linear)
    n_fft = optimize.get_n_fft(N)
    workers = os.cpu_count() or 1
    H = sfft.rfft(h_linear, n=n_fft, workers=workers)
    del h_linear
    log_mag = np.abs(H)
    del H
    log_mag += 1e-300
    np.log(log_mag, out=log_mag)
    cep = sfft.irfft(log_mag, n=n_fft, workers=workers)
    del log_mag
    n_half = n_fft // 2
    # cep_out[0] = c[0]; [1:n_half] = (1+alpha)*c; [n_half] = c[n_half]; [n_half+1:] = (1-alpha)*c
    # (np.longdouble is float64 here, checked by the caller).
    cep[1:n_half] *= (1.0 + alpha)
    if n_half + 1 < n_fft:
        cep[n_half + 1:] *= (1.0 - alpha)
    H_out = sfft.rfft(cep, n=n_fft, workers=workers)
    del cep
    np.exp(H_out, out=H_out)
    h_out = sfft.irfft(H_out, n=n_fft, workers=workers, overwrite_x=True)
    del H_out
    h_out = h_out[:N].copy()
    if order == "pairwise":
        total_out = float(np.sum(h_out))
    else:
        with ThreadPoolExecutor(WORKERS) as pool:
            total_out = blocks_sum(lambda a, b: h_out[a:b], N, pool)
    h_out /= total_out
    return h_out


def minimum_phase_branch0(lin, L, pool):
    """Branch 0 of the minimum-phase filter above MIN_FULL_MAX_TAPS (minphase_branch0.py).

    It must give branch 0, the whole filter's ends and its left-to-right Σh —
    the three things the file holds.
    """
    try:
        import minphase_branch0 as mb
    except ImportError as e:
        raise SystemExit(f"minimum phase over {MIN_FULL_MAX_TAPS:,} taps needs fir-optimizer/minphase_branch0.py "
                         f"(not here: {e})")
    h0, info = mb.minphase_branch0(lambda a, b: lin.h(a, b), L, n_taps=lin.N, workers=WORKERS, log=lambda *a: log(" ".join(map(str, a))))
    missing = [k for k in ("ends", "sum_seq") if k not in info]
    if missing:
        raise SystemExit(f"minphase_branch0 gives no {', '.join(missing)} of the whole filter: a branch-0 file "
                         f"needs its first {HEAD_TAPS} and last {TAIL_TAPS} taps and its left-to-right sum")
    return np.asarray(h0, dtype=np.float64), np.asarray(info["ends"], dtype=np.float64), float(info["sum_seq"]), info


# ---------------------------------------------------------------------------
# Files
# ---------------------------------------------------------------------------

def stem_for(out_dir, tag, target, phase):
    return Path(out_dir) / f"fir_{tag}_{target}_{phase}"


def write_branch0(stem, h0, ends, total, N, L, source_rate, phase, generator):
    head_n, tail_n = min(N, HEAD_TAPS), min(N, TAIL_TAPS)
    assert len(h0) == -(-N // L) and len(ends) == head_n + tail_n
    stem = str(stem)
    np.save(stem + ".b0.npy", np.ascontiguousarray(h0, dtype="<f8"))
    np.save(stem + ".b0.ends.npy", np.ascontiguousarray(ends, dtype="<f8"))
    meta = {
        "format": FORMAT,
        "version": FORMAT_VERSION,
        "taps": int(N),
        "l": int(L),
        "source_rate": int(source_rate),
        "target_rate": int(source_rate * L),
        "phase": phase,
        "sum_bits": f"{np.float64(total).view(np.uint64):016x}",
        "sum": float(total),
        "head_taps": head_n,
        "tail_taps": tail_n,
        "generator": generator,
    }
    Path(stem + ".b0.json").write_text(json.dumps(meta, indent=2))
    log(f"      [+] {Path(stem).name}.b0.* ({len(h0):,} taps of branch 0, sum {total!r})")


def design(N, source_rate, L):
    """What optimize.generate() designs for this rung: the band and the window."""
    f_pass, f_stop = optimize._band_for_source(source_rate)
    fs = source_rate * L
    beta = optimize.KAISER_BETA_LONG
    if N < optimize.SHORT_FILTER_MAX_TAPS:
        beta, _ = optimize.short_filter_beta(N, f_pass, f_stop, fs)
    return f_pass, f_stop, fs, beta


def generate(out_dir, N, source_rate, L, phases, pool):
    Path(out_dir).mkdir(parents=True, exist_ok=True)
    tag = optimize.fmt_taps(N)
    f_pass, f_stop, fs, beta = design(N, source_rate, L)
    base = {"script": "gen_branch0.py", "version": GENERATOR_VERSION, "beta": float(beta),
            "f_pass": f_pass, "f_stop": f_stop, "numpy": np.__version__}
    lin = LinearKaiser(N, f_pass, f_stop, fs, beta)
    t0 = time.time()
    lin.normalise(pool)
    log(f"  {tag} {fs} Hz (x{L}): linear sum {lin.scale!r} in {time.time() - t0:.0f}s")
    if "linear_phase" in phases:
        t0 = time.time()
        h0, ends, total = branch0_from(lin.h, N, L, pool)
        write_branch0(stem_for(out_dir, tag, fs, "linear_phase"), h0, ends, total, N, L, source_rate,
                      "linear_phase", dict(base, method="closed_form", fc_norm=lin.fc_norm))
        del h0
        log(f"      linear in {time.time() - t0:.0f}s")
    if "minimum_phase" in phases:
        t0 = time.time()
        if N <= MIN_FULL_MAX_TAPS:
            h = np.empty(N, dtype=np.float64)
            for a in range(0, N, PIECE):
                b = min(a + PIECE, N)
                h[a:b] = lin.h(a, b)
            h = cepstral_minimum_phase([h])
            h0, ends, total = branch0_from(lambda a, b: h[a:b], N, L)
            del h
            gen = dict(base, method="cepstrum_full", n_fft=optimize.get_n_fft(N))
        else:
            h0, ends, total, info = minimum_phase_branch0(lin, L, pool)
            gen = dict(base, method="minphase_branch0",
                       **{k: v for k, v in info.items() if k in ("n_fft", "method", "floor")})
        write_branch0(stem_for(out_dir, tag, fs, "minimum_phase"), h0, ends, total, N, L, source_rate,
                      "minimum_phase", gen)
        log(f"      minimum in {time.time() - t0:.0f}s")


def from_full(blob, L, out_dir=None):
    """The three files from a whole fir_<TAG>_<TARGET>_<phase>.npy designed for x L."""
    blob = Path(blob)
    name = blob.stem  # fir_<TAG>_<TARGET>_<phase>
    parts = name.split("_")
    target = int(parts[2])
    phase = "_".join(parts[3:])
    if target % L:
        raise SystemExit(f"{blob.name}: {target} Hz is not x{L} of a source rate")
    h = np.load(blob, mmap_mode="r")
    if h.dtype != np.float64 or h.ndim != 1:
        raise SystemExit(f"{blob.name}: {h.dtype} {h.shape}, expected 1-D float64")
    N = len(h)
    h0, ends, total = branch0_from(lambda a, b: np.array(h[a:b]), N, L)
    write_branch0(Path(out_dir or blob.parent) / name, h0, ends, total, N, L, target // L, phase,
                  {"script": "gen_branch0.py", "version": GENERATOR_VERSION, "method": "from_full",
                   "source": blob.name})


def main(argv):
    if len(argv) >= 3 and argv[0] == "--from-full":
        from_full(argv[1], int(argv[2]), argv[3] if len(argv) > 3 else None)
        return
    if len(argv) < 3:
        raise SystemExit(__doc__)
    out_dir = Path(argv[0])
    mults = [int(m) for m in argv[1].split(",")]
    sizes = [optimize.parse_taps(s) for s in argv[2].split(",")]
    which = argv[3] if len(argv) > 3 else "both"
    families = [int(f) for f in argv[4].split(",")] if len(argv) > 4 else FAMILIES
    phases = [PHASES[p] for p in PHASES] if which == "both" else [PHASES[which]]
    out_dir.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(WORKERS) as pool:
        for N in sizes:
            for L in mults:
                for src in families:
                    generate(out_dir, N, src, L, phases, pool)
    log("[all done]")


if __name__ == "__main__":
    main(sys.argv[1:])
