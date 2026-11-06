//! The bytes between the render loop and the IOProc: one writer (the audio
//! thread, period by period), one reader (CoreAudio's IO thread, which asks
//! for its buffer when it wants it). No lock and no allocation on either
//! side; the reader wakes the writer each time it takes a buffer.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use super::plan::spread;

pub struct Ring {
    buf: Box<[UnsafeCell<u8>]>,
    /// Bytes of one stereo frame as the packer writes it.
    frame: usize,
    /// Bytes written and read since the start; the difference is the fill.
    written: AtomicUsize,
    read: AtomicUsize,
    /// IO buffers the reader found short (the rest went out silent).
    pub underruns: AtomicU64,
    writer: OnceLock<std::thread::Thread>,
}

// SAFETY: one thread writes and one reads; each byte is written before
// `written` is published (Release) and read only after it is seen (Acquire),
// and the writer only writes bytes the reader has given back through `read`
// the same way — no byte is touched by both at once.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(frames: usize, frame_bytes: usize) -> Ring {
        Ring {
            buf: (0..frames * frame_bytes).map(|_| UnsafeCell::new(0)).collect(),
            frame: frame_bytes,
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            underruns: AtomicU64::new(0),
            writer: OnceLock::new(),
        }
    }

    fn cap(&self) -> usize {
        self.buf.len()
    }

    fn ptr(&self) -> *mut u8 {
        // UnsafeCell<u8> has u8's layout.
        self.buf.as_ptr() as *mut u8
    }

    /// The writer's thread, for the reader to wake.
    pub fn set_writer(&self) {
        self.writer.set(std::thread::current()).ok();
    }

    pub fn filled_frames(&self) -> usize {
        (self.written.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)) / self.frame
    }

    pub fn free_frames(&self) -> usize {
        self.cap() / self.frame - self.filled_frames()
    }

    /// Writer: append whole frames from `bytes`, as many as fit; the frames
    /// taken.
    pub fn push(&self, bytes: &[u8]) -> usize {
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let room = self.cap() - (w - r);
        let n = (bytes.len().min(room) / self.frame) * self.frame;
        let at = w % self.cap();
        let first = n.min(self.cap() - at);
        // SAFETY: [at, at+first) and [0, n-first) lie in the free part
        // (`room`), which the reader does not touch until `written` moves.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr().add(at), first);
            std::ptr::copy_nonoverlapping(bytes.as_ptr().add(first), self.ptr(), n - first);
        }
        self.written.store(w + n, Ordering::Release);
        n / self.frame
    }

    /// Reader: fill `frames` device frames of `dst_frame` bytes in `dst`
    /// (two channels from the ring, the rest silent); what the ring lacks
    /// goes out as silence and counts as an underrun. Wakes the writer.
    pub fn pop_into(&self, dst: &mut [u8], dst_frame: usize, frames: usize) -> usize {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.written.load(Ordering::Acquire);
        let have = ((w - r) / self.frame).min(frames);
        let n = have * self.frame;
        let at = r % self.cap();
        let first = n.min(self.cap() - at);
        // SAFETY: [r, r+n) is filled and published; the writer does not
        // write there until `read` moves past it.
        let (a, b) = unsafe {
            (
                std::slice::from_raw_parts(self.ptr().add(at), first),
                std::slice::from_raw_parts(self.ptr(), n - first),
            )
        };
        let fa = first / self.frame;
        spread(a, self.frame, dst, dst_frame, fa);
        spread(b, self.frame, &mut dst[fa * dst_frame..], dst_frame, have - fa);
        dst[have * dst_frame..frames * dst_frame].fill(0);
        self.read.store(r + n, Ordering::Release);
        if have < frames {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
        have
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn frames_go_round_in_order_across_the_wrap() {
        let ring = Ring::new(4, 2);
        assert_eq!(ring.free_frames(), 4);
        assert_eq!(ring.push(&[1, 1, 2, 2, 3, 3]), 3);
        let mut out = [9u8; 4];
        assert_eq!(ring.pop_into(&mut out, 2, 2), 2);
        assert_eq!(out, [1, 1, 2, 2]);
        // 3 left, room for 3: the write wraps.
        assert_eq!(ring.push(&[4, 4, 5, 5, 6, 6, 7, 7]), 3);
        assert_eq!(ring.free_frames(), 0);
        let mut out = [9u8; 8];
        assert_eq!(ring.pop_into(&mut out, 2, 4), 4);
        assert_eq!(out, [3, 3, 4, 4, 5, 5, 6, 6]);
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_short_ring_goes_out_silent_and_counts() {
        let ring = Ring::new(4, 2);
        ring.push(&[1, 1]);
        let mut out = [9u8; 6];
        assert_eq!(ring.pop_into(&mut out, 2, 3), 1);
        assert_eq!(out, [1, 1, 0, 0, 0, 0]);
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stereo_goes_into_a_wider_device_frame() {
        let ring = Ring::new(4, 2);
        ring.push(&[1, 2, 3, 4]);
        let mut out = [9u8; 8];
        ring.pop_into(&mut out, 4, 2);
        assert_eq!(out, [1, 2, 0, 0, 3, 4, 0, 0]);
    }

    #[test]
    fn a_writer_and_a_reader_on_two_threads_lose_nothing() {
        let ring = Arc::new(Ring::new(64, 4));
        let total = 20_000u32;
        let w = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                ring.set_writer();
                let mut next = 0u32;
                while next < total {
                    let free = ring.free_frames().min((total - next) as usize);
                    if free == 0 {
                        std::thread::park_timeout(std::time::Duration::from_millis(1));
                        continue;
                    }
                    let bytes: Vec<u8> = (next..next + free as u32).flat_map(|i| i.to_le_bytes()).collect();
                    next += ring.push(&bytes) as u32;
                }
            })
        };
        let mut seen = 0u32;
        let mut out = [0u8; 4 * 16];
        while seen < total {
            let got = ring.pop_into(&mut out, 4, 16);
            for f in out[..got * 4].chunks(4) {
                assert_eq!(u32::from_le_bytes(f.try_into().unwrap()), seen);
                seen += 1;
            }
            if got == 0 {
                std::thread::yield_now();
            }
        }
        w.join().unwrap();
    }
}
