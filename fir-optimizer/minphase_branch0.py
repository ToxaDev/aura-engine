"""Branch 0 of the minimum-phase filter without the full-rate filter in RAM.

What it computes is exactly `optimize.generate_cepstral_phase(h, alpha=1.0)[::L]`
(the same n_fft, the same log floor, the same fold, the same /sum
normalisation), but only the coefficients Multi uses: h_min[k*L], the
branch 0 at the source rate.

Why it fits where the old path does not: the old path holds several full
n_fft arrays at once (1000M taps -> n_fft 2^31, 32 GB per complex array).
Here every real FFT of size n_fft is a complex FFT of n_fft/2 (two real
samples packed into one complex), done by the four-step algorithm on one
complex array of n_fft/2 that is transformed in place block by block
(1000M: 16 GB, 500M: 8 GB). Between the transforms the spectrum stays in
the four-step's transposed order - log, exp and the fold do not care about
order. At the end the spectrum is folded (aliased) onto n_fft/L bins, and
one small inverse FFT of that gives h_min[k*L] exactly: decimating by L in
time is summing the L shifted copies of the spectrum. The sum of the whole
h_min[:N] (the normalisation) comes from the same pass with the closed-form
spectrum of the N-sample window, so the full h_min never exists.

Exact in exact arithmetic. In floating point it differs from the old path by
rounding only - but the rounding of log|H| in a -200 dB stopband is not
small (see minphase_branch0_check.py and the measurements in the task notes).

    h0, info = minphase_branch0(h_linear, L)

h_linear: the linear-phase prototype at the output rate - an ndarray or a
np.memmap (np.load(path, mmap_mode='r')), or a callable chunk(start, stop)
returning h[start:stop] as float64 (then pass n_taps).
"""
import math
import time

import numpy as np
import scipy.fft as sfft


def get_n_fft(N):
    """Same rule as optimize.get_n_fft (checked by minphase_branch0_check.py)."""
    if N < 1_000_000:
        mult = 16
    elif N < 5_000_000:
        mult = 8
    elif N < 20_000_000:
        mult = 4
    else:
        mult = 2
    return max(2 ** int(np.ceil(np.log2(mult * N))), 1 << 23)


_OCT_SWAP = np.array([0, 1, 1, 0, 0, 1, 1, 0], dtype=bool)
_OCT_SC = np.array([1, 1, -1, -1, -1, -1, 1, 1], dtype=np.float64)
_OCT_SS = np.array([1, 1, 1, 1, -1, -1, -1, -1], dtype=np.float64)


def _cis(m, n, sign):
    """exp(sign * 2*pi*i * m/n) for integer arrays m, to ~0.5 ulp.

    The angle is reduced to one octant in integers first, so the float angle
    never exceeds pi/4 - a plain cos(2*pi*m/n) loses ~10x that accuracy, and
    in a -200 dB stopband the min-phase result is that sensitive to it.
    """
    t = 8 * (np.asarray(m, dtype=np.int64) % n)
    o = t // n
    f = t - o * n
    odd = (o & 1).astype(bool)
    g = np.where(odd, n - f, f).astype(np.float64)
    x = g * (math.pi / (4 * n))
    cx, sx = np.cos(x), np.sin(x)
    swap = _OCT_SWAP[o]
    c = np.where(swap, sx, cx) * _OCT_SC[o]
    s = np.where(swap, cx, sx) * _OCT_SS[o]
    return c + (1j * sign) * s


class _Packed:
    """A real signal of length n = 2*Nc held as Nc complex samples in an
    (R, C) array, natural order z[c + C*r] = x[2m] + j x[2m+1] with m = c + C*r.
    After forward() the array holds X[k] (k < Nc) of the real FFT at
    [k2, k1] with k = k2 + R*k1 ("transposed" order) and X[Nc] in `nyq`."""

    def __init__(self, n, max_rows, workers, block_elems, log):
        assert n & (n - 1) == 0 and n >= 16
        self.n = n
        self.Nc = n // 2
        lg = self.Nc.bit_length() - 1
        R = 1 << (lg // 2)
        R = min(R, max_rows)  # rows must not exceed the fold length M
        self.R, self.C = R, self.Nc // R
        self.workers = workers
        self.block_elems = block_elems
        self.log = log or (lambda *_: None)
        t0 = time.time()
        self.a = np.empty((self.R, self.C), dtype=np.complex128)
        self.log(f"array {self.R}x{self.C} complex ({self.a.nbytes / 2**30:.1f} GB) in {time.time() - t0:.1f}s")
        self.nyq = 0.0

    # --- block helpers -------------------------------------------------
    def _row_blocks(self, start=0, stop=None):
        stop = self.R if stop is None else stop
        step = max(1, self.block_elems // self.C)
        for r0 in range(start, stop, step):
            yield r0, min(stop, r0 + step)

    def _col_blocks(self):
        step = max(1, self.block_elems // self.R)
        for c0 in range(0, self.C, step):
            yield c0, min(self.C, c0 + step)

    def _twiddle(self, r0, r1, sign):
        """exp(sign*2*pi*i*(k2*n1 mod Nc)/Nc) for rows k2 in [r0, r1), all columns n1."""
        k2 = np.arange(r0, r1, dtype=np.int64)[:, None]
        n1 = np.arange(self.C, dtype=np.int64)[None, :]
        return _cis(k2 * n1, self.Nc, sign)

    # --- the complex FFT of Nc, four-step ------------------------------
    def _fft_inplace(self):
        a = self.a
        for c0, c1 in self._col_blocks():
            a[:, c0:c1] = sfft.fft(a[:, c0:c1], axis=0, workers=self.workers, overwrite_x=True)
        for r0, r1 in self._row_blocks():
            blk = a[r0:r1] * self._twiddle(r0, r1, -1.0)
            a[r0:r1] = sfft.fft(blk, axis=1, workers=self.workers, overwrite_x=True)

    def _ifft_inplace(self):
        a = self.a
        for r0, r1 in self._row_blocks():
            blk = sfft.ifft(a[r0:r1], axis=1, workers=self.workers)
            blk *= self._twiddle(r0, r1, +1.0)
            a[r0:r1] = blk
        for c0, c1 in self._col_blocks():
            a[:, c0:c1] = sfft.ifft(a[:, c0:c1], axis=0, workers=self.workers, overwrite_x=True)

    # --- the k <-> Nc-k pairing in transposed order ----------------------
    def _k_of(self, r0, r1):
        """k = k2 + R*k1 for rows [r0, r1)."""
        return (np.arange(r0, r1, dtype=np.int64)[:, None]
                + self.R * np.arange(self.C, dtype=np.int64)[None, :])

    def _pairs(self):
        """Yield (rows, partner-rows) slabs: partner[i, j] holds Z[Nc - k] for
        the k at rows[i, j]; both are views/copies the caller writes back."""
        R = self.R
        half = R // 2
        # rows 1..half-1 pair with rows R-1..half+1 (columns reversed)
        for r0, r1 in self._row_blocks(1, half):
            yield r0, r1
        # rows 0 and half pair with themselves
        yield 0, 1
        if half > 0:
            yield half, half + 1

    def _partner(self, r0, r1):
        a, R = self.a, self.R
        if r0 == 0 and r1 == 1:
            return np.roll(a[0, ::-1], 1)[None, :]
        if r0 == R // 2 and r1 == r0 + 1:
            return a[r0, ::-1][None, :].copy()
        return a[R - r1 + 1:R - r0 + 1][::-1, ::-1].copy()

    def _store_partner(self, r0, r1, p):
        """Write back the partner rows of a pair block (not the self-paired rows)."""
        R = self.R
        self.a[R - r1 + 1:R - r0 + 1] = p[::-1, ::-1]

    def forward(self):
        """Natural-order packed real signal -> half spectrum (transposed)."""
        self._fft_inplace()
        n, R = self.n, self.R
        z00 = self.a[0, 0]
        for r0, r1 in self._pairs():
            z = self.a[r0:r1].copy()
            zp = self._partner(r0, r1)
            k = self._k_of(r0, r1)
            w = _cis(k, n, -1.0)
            e = 0.5 * (z + np.conj(zp))
            o = -0.5j * (z - np.conj(zp))
            self.a[r0:r1] = e + w * o
            if not (r0 == 0 or (r0 == R // 2 and r1 == r0 + 1)):
                # the partner rows: k' = Nc - k, their partner is z
                kp = self.Nc - k
                wp = _cis(kp, n, -1.0)
                ep = 0.5 * (zp + np.conj(z))
                op = -0.5j * (zp - np.conj(z))
                self._store_partner(r0, r1, ep + wp * op)
        self.nyq = float(z00.real - z00.imag)
        # X[0] is real for a real signal
        self.a[0, 0] = complex(z00.real + z00.imag, 0.0)

    def inverse(self):
        """Half spectrum (transposed, X[Nc] in nyq) -> natural-order packed real."""
        n, R, Nc = self.n, self.R, self.Nc
        for r0, r1 in self._pairs():
            x = self.a[r0:r1].copy()
            xp = self._partner(r0, r1)
            if r0 == 0:
                xp = xp.copy()
                xp[0, 0] = self.nyq  # partner of k = 0 is X[Nc]
            k = self._k_of(r0, r1)
            winv = _cis(k, n, 1.0)
            e = 0.5 * (x + np.conj(xp))
            o = 0.5 * (x - np.conj(xp)) * winv
            self.a[r0:r1] = e + 1j * o
            if not (r0 == 0 or (r0 == R // 2 and r1 == r0 + 1)):
                kp = Nc - k
                winvp = _cis(kp, n, 1.0)
                ep = 0.5 * (xp + np.conj(x))
                op = 0.5 * (xp - np.conj(x)) * winvp
                self._store_partner(r0, r1, ep + 1j * op)
        self._ifft_inplace()


def _log_mag(x, floor):
    if floor is None:
        return np.log(np.abs(x) + 1e-300)
    return np.log(np.maximum(np.abs(x), floor))


def _fill(p, h_linear, N):
    """Packed natural order: row r holds x[2*C*r : 2*C*(r+1)], zero past N."""
    C = p.C
    for r0, r1 in p._row_blocks():
        s, e = 2 * C * r0, 2 * C * r1
        x = np.zeros(e - s, dtype=np.float64)
        if s < N:
            stop = min(e, N)
            chunk = h_linear(s, stop) if callable(h_linear) else h_linear[s:stop]
            x[:stop - s] = chunk
        p.a[r0:r1] = (x[0::2] + 1j * x[1::2]).reshape(r1 - r0, C)


class MinPhase:
    """The whole h_min of optimize.generate_cepstral_phase(h, 1.0), unnormalised,
    held as one float64 view of the packed array (n_fft values, 8*n_fft bytes)."""

    def __init__(self, p, N, info):
        self._p = p
        self.N = N
        self.info = info
        self.h_out = p.a.reshape(-1).view(np.float64)[:N]  # natural order, zero-copy
        # the old path: total = np.sum(h_out[:N]) (pairwise), h = h_out / total
        self.total = float(np.sum(self.h_out))

    def h(self, start, stop, step=1):
        """The normalised filter h[start:stop:step] - the values the full blob holds."""
        return self.h_out[start:stop:step] / self.total

    def branch(self, L, r=0):
        """h[r::L] - branch r at the source rate (branch 0 is what Multi uses)."""
        return self.h(r, self.N, L)

    def seq_sum(self, chunk=1 << 24):
        """Sum of the normalised h, left to right in index order (Rust iter().sum())."""
        acc = 0.0
        for s in range(0, self.N, chunk):
            c = self.h(s, min(self.N, s + chunk))
            acc = float(np.cumsum(np.concatenate(([acc], c)))[-1])
        return acc

    def save_full(self, path, chunk=1 << 24):
        """Write the whole normalised filter as a plain .npy (what optimize would save)."""
        out = np.lib.format.open_memmap(path, mode='w+', dtype=np.float64, shape=(self.N,))
        for s in range(0, self.N, chunk):
            e = min(self.N, s + chunk)
            out[s:e] = self.h(s, e)
        out.flush()
        del out

    def close(self):
        self.h_out = None
        self._p = None


def minphase(h_linear, n_taps=None, n_fft=None, floor=None, workers=8,
             block_elems=1 << 22, log=None):
    """optimize.generate_cepstral_phase(h_linear, alpha=1.0) in one n_fft/2 complex array.

    Returns a MinPhase (call .close() or drop it to free the array).
    floor=None: log(|H| + 1e-300), the old path. floor=x: log(max(|H|, x)).
    (A floor was measured 23.10: it does not make the result FFT-independent
    to f64 and changes the filter a lot - the old path stays the default.)
    """
    N = len(h_linear) if n_taps is None else int(n_taps)
    n = get_n_fft(N) if n_fft is None else int(n_fft)
    assert n >= N
    log = log or (lambda *_: None)
    t = {}
    t0 = time.time()
    p = _Packed(n, max_rows=1 << 30, workers=workers, block_elems=block_elems, log=log)
    R = p.R
    _fill(p, h_linear, N)
    t['fill'] = time.time() - t0

    # 1. H = rfft(h, n); log_mag = log(|H| + 1e-300)
    t0 = time.time()
    p.forward()
    for r0, r1 in p._row_blocks():
        p.a[r0:r1] = _log_mag(p.a[r0:r1], floor)
    p.nyq = float(_log_mag(np.array([p.nyq]), floor)[0])
    t['rfft+log'] = time.time() - t0

    # 2. cepstrum = irfft(log_mag, n)
    t0 = time.time()
    p.inverse()
    t['irfft'] = time.time() - t0

    # 3. fold: c[0], 2c[1:Nc], c[Nc], 0...  (packed: rows < R/2 hold c[0:Nc])
    t0 = time.time()
    half = R // 2
    for r0, r1 in p._row_blocks(0, half):
        p.a[r0:r1] *= 2.0
    c0 = p.a[0, 0]
    p.a[0, 0] = complex(c0.real / 2.0, c0.imag)
    cn = p.a[half, 0].real
    p.a[half:] = 0.0
    p.a[half, 0] = complex(cn, 0.0)
    t['fold'] = time.time() - t0

    # 4. h_out = irfft(exp(rfft(c_out)), n) - in place, natural order
    t0 = time.time()
    p.forward()
    for r0, r1 in p._row_blocks():
        p.a[r0:r1] = np.exp(p.a[r0:r1])
    p.a[0, 0] = complex(p.a[0, 0].real, 0.0)  # irfft ignores the imaginary DC / Nyquist
    p.nyq = math.exp(p.nyq)
    t['rfft+exp'] = time.time() - t0
    t0 = time.time()
    p.inverse()
    t['irfft2'] = time.time() - t0
    return MinPhase(p, N, {'n_fft': n, 'N': N, 'times': t})


def minphase_branch0(h_linear, L, n_taps=None, n_fft=None, floor=None, workers=8,
                     block_elems=1 << 22, head=131072, tail=64, log=None):
    """Branch 0 of the min-phase filter and what the branch-0 file needs.

    Returns (h0, info): h0 = h_min[::L] (len ceil(N/L)); info: n_fft, N, L,
    total (what the old path divides by), sum_seq (left-to-right sum of the
    normalised h, for sum_bits), ends (h[:min(N, head)] ++ h[N - min(N, tail):]),
    times (s per stage).
    """
    mp = minphase(h_linear, n_taps=n_taps, n_fft=n_fft, floor=floor, workers=workers,
                  block_elems=block_elems, log=log)
    N = mp.N
    t0 = time.time()
    h0 = mp.branch(int(L))
    hh, tt = min(N, head), min(N, tail)
    ends = np.concatenate((mp.h(0, hh), mp.h(N - tt, N)))
    info = dict(mp.info, L=int(L), total=mp.total, sum_seq=mp.seq_sum(), ends=ends,
                head_taps=hh, tail_taps=tt)
    info['times']['branch0'] = time.time() - t0
    mp.close()
    return h0, info
