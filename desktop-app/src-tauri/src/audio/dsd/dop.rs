//! DoP (DSD over PCM, v1.1): 16 one-bit samples per channel in the low 16
//! bits of a 24-bit PCM word, marker 0x05/0xFA in the top byte, alternating
//! frame to frame and the same for every channel of a frame. A DAC enters
//! DSD mode after 32 good markers in a row and leaves it at the first one
//! missing, so the markers never stop while a DoP stream is open: silence
//! goes out as `DSD_SILENCE` with markers, never as zero words.
//!
//! Inside the 24-bit word the earlier byte of the 16 bits is the more
//! significant one: word = [marker][first byte][second byte], each byte MSB
//! first (the engine's bit order, `super`).

use super::DSD_SILENCE;

/// The two DoP markers; a stream starts with the first.
pub const MARKERS: [u8; 2] = [0x05, 0xFA];

/// One-bit samples per channel in one DoP frame.
pub const BITS_PER_FRAME: usize = 16;

/// The PCM container the DoP word travels in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// 24-bit packed, little-endian: [second, first, marker].
    Int24,
    /// 32-bit little-endian with the DoP word in the top 24 bits and the low
    /// byte zero: [0, second, first, marker].
    Int32,
}

impl Container {
    /// Bytes of one stereo frame.
    pub fn frame_bytes(self) -> usize {
        match self {
            Container::Int24 => 6,
            Container::Int32 => 8,
        }
    }

    fn sample_bytes(self) -> usize {
        self.frame_bytes() / 2
    }
}

/// Packs DSD bytes into DoP frames, the marker phase carried from one call
/// to the next (across periods, pauses and holds of one stream).
#[derive(Clone, Debug, Default)]
pub struct DopPacker {
    /// Index into `MARKERS` of the next frame's marker.
    phase: usize,
}

impl DopPacker {
    pub fn new() -> DopPacker {
        DopPacker { phase: 0 }
    }

    /// The marker the next frame gets.
    #[cfg(test)]
    pub fn next_marker(&self) -> u8 {
        MARKERS[self.phase]
    }

    /// `frames` stereo DoP frames into `out` (exactly `frames ×
    /// frame_bytes`): `l` and `r` hold two DSD bytes per frame each, the
    /// earlier first.
    pub fn pack(&mut self, l: &[u8], r: &[u8], frames: usize, c: Container, out: &mut [u8]) {
        debug_assert!(l.len() >= 2 * frames && r.len() >= 2 * frames);
        debug_assert_eq!(out.len(), frames * c.frame_bytes());
        let sb = c.sample_bytes();
        for i in 0..frames {
            let m = MARKERS[self.phase];
            self.phase ^= 1;
            let o = i * 2 * sb;
            write_word(&mut out[o..o + sb], m, l[2 * i], l[2 * i + 1], c);
            write_word(&mut out[o + sb..o + 2 * sb], m, r[2 * i], r[2 * i + 1], c);
        }
    }

    /// `frames` stereo frames of DSD silence (`DSD_SILENCE` with markers).
    pub fn silence(&mut self, frames: usize, c: Container, out: &mut [u8]) {
        debug_assert_eq!(out.len(), frames * c.frame_bytes());
        let sb = c.sample_bytes();
        for i in 0..frames {
            let m = MARKERS[self.phase];
            self.phase ^= 1;
            let o = i * 2 * sb;
            write_word(&mut out[o..o + sb], m, DSD_SILENCE, DSD_SILENCE, c);
            write_word(&mut out[o + sb..o + 2 * sb], m, DSD_SILENCE, DSD_SILENCE, c);
        }
    }
}

#[inline]
fn write_word(out: &mut [u8], marker: u8, first: u8, second: u8, c: Container) {
    match c {
        Container::Int24 => {
            out[0] = second;
            out[1] = first;
            out[2] = marker;
        }
        Container::Int32 => {
            out[0] = 0;
            out[1] = second;
            out[2] = first;
            out[3] = marker;
        }
    }
}

/// What a DoP stream carries, frame by frame: the marker, and the two DSD
/// bytes of each channel — as a DAC reads it. None when the bytes are not
/// whole frames, the low byte of an Int32 word is not zero, or the two
/// channels of a frame carry different markers.
#[cfg(test)]
pub fn unpack(bytes: &[u8], c: Container) -> Option<Vec<(u8, [u8; 2], [u8; 2])>> {
    let fb = c.frame_bytes();
    if bytes.len() % fb != 0 {
        return None;
    }
    let sb = c.sample_bytes();
    let word = |w: &[u8]| -> Option<(u8, [u8; 2])> {
        match c {
            Container::Int24 => Some((w[2], [w[1], w[0]])),
            Container::Int32 => (w[0] == 0).then_some((w[3], [w[2], w[1]])),
        }
    };
    bytes
        .chunks(fb)
        .map(|f| {
            let (ml, l) = word(&f[..sb])?;
            let (mr, r) = word(&f[sb..])?;
            (ml == mr).then_some((ml, l, r))
        })
        .collect()
}

/// Whether `frames` (from `unpack`) carry valid markers, alternating from
/// frame to frame. `first`: the marker the first frame must carry, or any.
#[cfg(test)]
pub fn markers_alternate(frames: &[(u8, [u8; 2], [u8; 2])], first: Option<u8>) -> bool {
    let Some(&(m0, _, _)) = frames.first() else { return true };
    if !MARKERS.contains(&m0) || first.is_some_and(|f| f != m0) {
        return false;
    }
    frames.windows(2).all(|w| {
        let (a, b) = (w[0].0, w[1].0);
        MARKERS.contains(&b) && a != b
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-exact in both containers: marker on top, the earlier byte next,
    /// LE order; the Int32 low byte zero.
    #[test]
    fn pack_is_bit_exact_in_both_containers() {
        let l = [0xA1, 0xB2, 0xC3, 0xD4];
        let r = [0x11, 0x22, 0x33, 0x44];
        let mut p = DopPacker::new();
        let mut out = vec![0u8; 2 * 6];
        p.pack(&l, &r, 2, Container::Int24, &mut out);
        assert_eq!(out, [0xB2, 0xA1, 0x05, 0x22, 0x11, 0x05, 0xD4, 0xC3, 0xFA, 0x44, 0x33, 0xFA]);
        let mut p = DopPacker::new();
        let mut out = vec![0xEEu8; 2 * 8];
        p.pack(&l, &r, 2, Container::Int32, &mut out);
        assert_eq!(
            out,
            [0, 0xB2, 0xA1, 0x05, 0, 0x22, 0x11, 0x05, 0, 0xD4, 0xC3, 0xFA, 0, 0x44, 0x33, 0xFA]
        );
        // As a 32-bit sample: the 24-bit DoP word in the top 24 bits.
        let w = u32::from_le_bytes(out[..4].try_into().unwrap());
        assert_eq!(w, 0x05A1_B200);
    }

    /// The marker phase carries across calls and across silence: one stream
    /// of alternating markers however it is cut, and the payload round-trips.
    #[test]
    fn markers_alternate_across_calls_and_silence() {
        for c in [Container::Int24, Container::Int32] {
            let mut p = DopPacker::new();
            let mut all = Vec::new();
            let mut payload_l = Vec::new();
            for (k, frames) in [3usize, 1, 4, 7, 2].into_iter().enumerate() {
                let mut out = vec![0u8; frames * c.frame_bytes()];
                if k % 2 == 1 {
                    p.silence(frames, c, &mut out);
                    payload_l.extend(std::iter::repeat(DSD_SILENCE).take(2 * frames));
                } else {
                    let l: Vec<u8> = (0..2 * frames).map(|i| (i * 7 + k) as u8).collect();
                    let r: Vec<u8> = l.iter().map(|b| !b).collect();
                    p.pack(&l, &r, frames, c, &mut out);
                    payload_l.extend_from_slice(&l);
                }
                all.extend_from_slice(&out);
            }
            let frames = unpack(&all, c).expect("whole frames");
            assert_eq!(frames.len(), 17);
            assert!(markers_alternate(&frames, Some(0x05)), "{c:?}");
            let got: Vec<u8> = frames.iter().flat_map(|f| f.1).collect();
            assert_eq!(got, payload_l, "{c:?}");
        }
    }

    /// Silence is 0x69 in every payload byte, with markers — never a zero word.
    #[test]
    fn silence_frames_carry_0x69_and_markers() {
        for c in [Container::Int24, Container::Int32] {
            let mut p = DopPacker::new();
            let mut out = vec![0u8; 5 * c.frame_bytes()];
            p.silence(5, c, &mut out);
            let f = unpack(&out, c).unwrap();
            assert!(markers_alternate(&f, Some(0x05)));
            assert!(f.iter().all(|(_, l, r)| *l == [0x69; 2] && *r == [0x69; 2]));
            assert_eq!(p.next_marker(), 0xFA);
        }
    }

    #[test]
    fn unpack_refuses_what_is_not_dop() {
        // Zero words: marker 0, not DoP.
        let f = unpack(&[0u8; 12], Container::Int24).unwrap();
        assert!(!markers_alternate(&f, None));
        // Int32 with a non-zero low byte.
        assert!(unpack(&[1, 0, 0, 5, 0, 0, 0, 5], Container::Int32).is_none());
        // Channels of one frame with different markers.
        assert!(unpack(&[0, 0, 5, 0, 0, 0xFA], Container::Int24).is_none());
        // Two frames with the same marker.
        let f = unpack(&[0, 0, 5, 0, 0, 5, 0, 0, 5, 0, 0, 5], Container::Int24).unwrap();
        assert!(!markers_alternate(&f, None));
    }
}
