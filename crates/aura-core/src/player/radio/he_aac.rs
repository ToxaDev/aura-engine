//! HE-AAC (AAC with SBR, and PS), which symphonia does not decode, for the
//! radio (`decode`) and for files (the converter's decode): the FDK AAC
//! decoder (`fdk`), the same on every system.
//!
//! It takes a stream's ADTS frames one by one and gives f64 at the rate SBR
//! gives, its samples where Windows' own decoder (which played HE-AAC here
//! before) gave them, and where symphonia gives AAC-LC's. At the end of a
//! file `finish` hands on what the decoder still holds.

use super::adts::Header;
use super::fdk::FdkAac;

/// What a decoder gives out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u32,
}

/// An HE-AAC (and AAC-LC) decoder on ADTS frames, out at the rate SBR gives.
pub struct HeAac {
    dec: FdkAac,
    /// The stream's first header: its core, which names what is decoded.
    core: Header,
}

impl HeAac {
    /// A decoder for the stream `core` is the first header of.
    pub fn new(core: &Header) -> Result<HeAac, String> {
        Ok(HeAac { dec: FdkAac::new()?, core: *core })
    }

    /// What it gives out: known once it has given samples.
    pub fn format(&self) -> Format {
        self.dec.format()
    }

    /// The name of what it decodes, from what it gives out: SBR doubles the
    /// core's rate, PS makes a mono core stereo.
    pub fn codec(&self) -> &'static str {
        let f = self.format();
        let sbr = f.rate >= 2 * self.core.rate();
        let ps = self.core.channels == 1 && f.channels >= 2;
        match (sbr, ps) {
            (true, true) => "HE-AAC v2",
            (true, false) => "HE-AAC",
            _ => "AAC",
        }
    }

    /// One ADTS frame (header and all) in; what the decoder gives for it
    /// onto `out`, interleaved, in f64. An error says the frame did not
    /// decode; what the decoder gave in its place, if anything, is on `out`
    /// all the same, and the decoder is ready for the next frame.
    pub fn push(&mut self, frame: &[u8], out: &mut Vec<f64>) -> Result<(), String> {
        self.dec.push(frame, out)
    }

    /// The end of the stream (a file's): what the decoder still holds, onto
    /// `out`.
    pub fn finish(&mut self, out: &mut Vec<f64>) -> Result<(), String> {
        self.dec.finish(out)
    }

    /// The decoder was made afresh after a fault since this was last asked:
    /// what follows comes from a new decoder, which the stream meets as a gap.
    pub fn restarted(&mut self) -> bool {
        self.dec.restarted()
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::adts::{frame_id, Placed, Trail};
    use super::*;

    /// What the test streams are made as.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Profile {
        Lc,
        /// HE-AAC: SBR on a core at half the rate.
        He,
        /// HE-AAC v2: SBR and PS, a mono core made stereo.
        HeV2,
    }

    /// An ADTS stream made by the FDK AAC encoder (C, a dev-dependency:
    /// the same streams on every system) from `channels` of interleaved
    /// 16-bit `pcm` at `rate`, at `bitrate` b/s; the encoder's look-ahead
    /// pushed out with silence. And the AudioSpecificConfig it gives, as an
    /// MP4 file holds it.
    pub fn encode(profile: Profile, rate: u32, channels: u32, bitrate: u32, pcm: &[i16]) -> (Vec<u8>, Vec<u8>) {
        use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
        let enc = Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(bitrate),
            sample_rate: rate,
            transport: Transport::Adts,
            channels: if channels == 1 { ChannelMode::Mono } else { ChannelMode::Stereo },
            audio_object_type: match profile {
                Profile::Lc => AudioObjectType::Mpeg4LowComplexity,
                Profile::He => AudioObjectType::Mpeg4HeAac,
                Profile::HeV2 => AudioObjectType::Mpeg4HeAacV2,
            },
        })
        .expect("the FDK AAC encoder");
        let info = enc.info().expect("the encoder's settings");
        let asc = info.confBuf[..info.confSize as usize].to_vec();
        let (ch, frame) = (channels as usize, info.frameLength as usize);
        let mut input = pcm.to_vec();
        input.resize(pcm.len() + (info.nDelay as usize + 2 * frame) * ch, 0);
        let (mut stream, mut buf, mut at) = (vec![], vec![0u8; 8192], 0);
        while at < input.len() {
            let end = (at + frame * ch).min(input.len());
            let r = enc.encode(&input[at..end], &mut buf).expect("encodes");
            stream.extend_from_slice(&buf[..r.output_size]);
            if r.input_consumed == 0 && r.output_size == 0 {
                break;
            }
            at += r.input_consumed;
        }
        (stream, asc)
    }

    /// Interleaved 16-bit stereo, quiet: a sine on each side (441 Hz on the
    /// left, 1234.5 Hz on the right).
    pub fn tone(rate: u32, secs: f64) -> Vec<i16> {
        let n = (rate as f64 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            v.push(((2.0 * std::f64::consts::PI * 441.0 * t).sin() * 6000.0) as i16);
            v.push(((2.0 * std::f64::consts::PI * 1234.5 * t).sin() * 5000.0) as i16);
        }
        v
    }

    /// Interleaved 16-bit stereo with a top octave: a sine on each side and
    /// noise over the whole band (−26 dBFS), the same every time.
    pub fn bright(rate: u32, secs: f64) -> Vec<i16> {
        let mut x = 0x2545_f491u32;
        let n = (rate as f64 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            for (k, f0) in [440.0, 660.0].into_iter().enumerate() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let noise = (x as f64 / u32::MAX as f64 - 0.5) * 0.1;
                let s = (2.0 * std::f64::consts::PI * f0 * t).sin() * (0.2 - 0.1 * k as f64) + noise;
                v.push((s * 32767.0) as i16);
            }
        }
        v
    }

    /// An ADTS stream's whole frames.
    pub fn frames_of(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut f = super::super::adts::Framer::new();
        f.push(stream);
        let (mut all, mut fr) = (vec![], vec![]);
        while f.next(true, &mut fr).is_some() {
            all.push(fr.clone());
        }
        all
    }

    /// Frames through a decoder to the end of the stream, interleaved; the
    /// frames it would not decode.
    pub fn decode_all(d: &mut HeAac, frames: &[Vec<u8>]) -> (Vec<f64>, usize) {
        let mut out = vec![];
        let refused = frames.iter().filter(|fr| d.push(fr, &mut out).is_err()).count();
        d.finish(&mut out).expect("the decoder's last frames");
        (out, refused)
    }

    /// How far under `a` the difference `a - b` is, dB (over the shorter).
    pub fn under(a: &[f64], b: &[f64]) -> f64 {
        let (mut sig, mut dif) = (0f64, 0f64);
        for (x, y) in a.iter().zip(b) {
            sig += x * x;
            dif += (x - y) * (x - y);
        }
        10.0 * (sig / dif.max(1e-300)).log10()
    }

    /// The energy of one channel of interleaved `x` (frames from `from` on)
    /// in the band `lo..hi` Hz, dB against full scale.
    pub fn band_db(x: &[f64], ch: usize, c: usize, from: usize, rate: u32, lo: f64, hi: f64) -> f64 {
        use rustfft::num_complex::Complex;
        let n = 8192;
        let mut fft = rustfft::FftPlanner::<f64>::new();
        let plan = fft.plan_fft_forward(n);
        let (mut e, mut blocks) = (0f64, 0);
        let mut at = from;
        while (at + n) * ch <= x.len() {
            let mut buf: Vec<Complex<f64>> = (0..n)
                .map(|i| {
                    let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
                    Complex::new(x[(at + i) * ch + c] * w, 0.0)
                })
                .collect();
            plan.process(&mut buf);
            for (k, v) in buf.iter().enumerate().take(n / 2) {
                let f = k as f64 * rate as f64 / n as f64;
                if f >= lo && f < hi {
                    e += v.norm_sqr();
                }
            }
            blocks += 1;
            at += n;
        }
        10.0 * (e / blocks.max(1) as f64 / (n as f64 * n as f64 / 8.0)).max(1e-30).log10()
    }

    /// symphonia's decode of an ADTS stream (AAC-LC), interleaved.
    pub fn symphonia_pcm(stream: &[u8]) -> Vec<f32> {
        use symphonia::core::codecs::audio::AudioDecoderOptions;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(stream.to_vec())), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("aac");
        let mut format =
            symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
        let track = format.tracks()[0].clone();
        let params = track.codec_params.as_ref().unwrap().audio().unwrap().clone();
        let mut dec = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default()).unwrap();
        let mut out = vec![];
        let mut tmp: Vec<f32> = vec![];
        while let Ok(Some(p)) = format.next_packet() {
            if let Ok(d) = dec.decode(&p) {
                d.copy_to_vec_interleaved(&mut tmp);
                out.extend_from_slice(&tmp);
            }
        }
        out
    }

    /// An MP4 (.m4a) file of one AAC track: the ADTS frames' payloads, with
    /// `asc` in its esds; `rate` its time scale, each frame `per_frame` of
    /// it (as ffmpeg writes an HE-AAC track: the output's rate).
    pub fn mp4(asc: &[u8], rate: u32, channels: u16, frames: &[Vec<u8>], per_frame: u32) -> Vec<u8> {
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
        }
        fn full(kind: &[u8; 4], version_flags: u32, body: &[u8]) -> Vec<u8> {
            boxed(kind, &[&version_flags.to_be_bytes()[..], body].concat())
        }
        fn desc(tag: u8, body: &[u8]) -> Vec<u8> {
            [&[tag, body.len() as u8][..], body].concat()
        }
        let be = |v: u32| v.to_be_bytes();
        let payloads: Vec<&[u8]> = frames.iter().map(|f| &f[Header::LEN..]).collect();
        let n = payloads.len() as u32;
        let duration = n * per_frame;
        let matrix: Vec<u8> = [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000].iter().flat_map(|v| v.to_be_bytes()).collect();
        let mvhd = [&be(0)[..], &be(0), &be(rate), &be(duration), &be(0x0001_0000), &[1, 0], &[0; 10], &matrix, &[0; 24], &be(2)].concat();
        let tkhd = [&be(0)[..], &be(0), &be(1), &be(0), &be(duration), &[0; 8], &[0, 0, 0, 0, 1, 0, 0, 0], &matrix, &be(0), &be(0)].concat();
        let mdhd = [&be(0)[..], &be(0), &be(rate), &be(duration), &[0x55, 0xC4, 0, 0]].concat();
        let hdlr = [&be(0)[..], b"soun", &[0; 12], b"SoundHandler\0"].concat();
        let dcd = [&[0x40u8, 0x15, 0, 0, 0][..], &be(0), &be(0), &desc(5, asc)].concat();
        let es = [&[0u8, 1, 0][..], &desc(4, &dcd), &desc(6, &[2])].concat();
        let esds = full(b"esds", 0, &desc(3, &es));
        let mp4a = [&[0u8; 6][..], &[0, 1], &[0; 8], &channels.to_be_bytes(), &[0, 16], &[0; 4], &be(rate << 16), &esds].concat();
        let stsd = full(b"stsd", 0, &[&be(1)[..], &boxed(b"mp4a", &mp4a)].concat());
        let stts = full(b"stts", 0, &[be(1), be(n), be(per_frame)].concat());
        let stsc = full(b"stsc", 0, &[be(1), be(1), be(n), be(1)].concat());
        let sizes: Vec<u8> = payloads.iter().flat_map(|p| be(p.len() as u32)).collect();
        let stsz = full(b"stsz", 0, &[&be(0)[..], &be(n), &sizes].concat());
        let ftyp = boxed(b"ftyp", &[&b"M4A "[..], &be(0), b"M4A ", b"mp42", b"isom"].concat());
        let moov = |offset: u32| {
            let stco = full(b"stco", 0, &[be(1), be(offset)].concat());
            let stbl = boxed(b"stbl", &[&stsd[..], &stts, &stsc, &stsz, &stco].concat());
            let dinf = boxed(b"dinf", &full(b"dref", 0, &[&be(1)[..], &full(b"url ", 1, &[])].concat()));
            let minf = boxed(b"minf", &[&full(b"smhd", 0, &[0; 4])[..], &dinf, &stbl].concat());
            let mdia = boxed(b"mdia", &[&full(b"mdhd", 0, &mdhd)[..], &full(b"hdlr", 0, &hdlr), &minf].concat());
            let trak = boxed(b"trak", &[&full(b"tkhd", 7, &tkhd)[..], &mdia].concat());
            boxed(b"moov", &[&full(b"mvhd", 0, &mvhd)[..], &trak].concat())
        };
        let offset = (ftyp.len() + moov(0).len() + 8) as u32;
        [ftyp, moov(offset), boxed(b"mdat", &payloads.concat())].concat()
    }

    /// The FDK decoder against symphonia on AAC-LC: the same samples at the
    /// same places, to the rounding of their f32, and as many — so its
    /// start-up frame is dropped and its last frame comes out at the end.
    #[test]
    fn the_fdk_decoder_decodes_aac_lc_as_symphonia_does_place_for_place() {
        for (rate, ch) in [(44_100, 2), (48_000, 2), (44_100, 1)] {
            let pcm: Vec<i16> = if ch == 2 { tone(rate, 2.0) } else { tone(rate, 2.0).into_iter().step_by(2).collect() };
            let (stream, _) = encode(Profile::Lc, rate, ch, 64_000 * ch, &pcm);
            let frames = frames_of(&stream);
            let mut d = HeAac::new(&Header::parse(&frames[0]).unwrap()).unwrap();
            let (got, refused) = decode_all(&mut d, &frames);
            let want: Vec<f64> = symphonia_pcm(&stream).into_iter().map(f64::from).collect();
            let db = under(&want, &got);
            eprintln!("AAC-LC {} Hz {} ch: {} frames, {} / {} samples, {:.1} dB apart", rate, ch, frames.len(), got.len(), want.len(), db);
            assert_eq!((d.format(), d.codec(), refused), (Format { rate, channels: ch }, "AAC", 0));
            assert_eq!(got.len(), frames.len() * 1024 * ch as usize, "every frame");
            assert_eq!(got.len(), want.len());
            assert!(db > 120.0, "only {:.1} dB apart", db);
        }
    }

    /// HE-AAC: out at twice its core's rate, with the top octave SBR makes
    /// of the core's band — the band over the core's Nyquist as loud as the
    /// source's (within 6 dB), where the core alone has nothing.
    #[test]
    fn he_aac_comes_out_at_twice_its_core_rate_with_its_top_octave() {
        let rate = 44_100;
        let pcm = bright(rate, 4.0);
        let (stream, _) = encode(Profile::He, rate, 2, 48_000, &pcm);
        let frames = frames_of(&stream);
        let core = Header::parse(&frames[0]).unwrap();
        assert_eq!((core.object, core.rate(), core.channels), (2, 22_050, 2), "an HE-AAC stream's ADTS header: its core");
        let mut d = HeAac::new(&core).unwrap();
        let (got, refused) = decode_all(&mut d, &frames);
        assert_eq!((d.format(), d.codec(), refused), (Format { rate, channels: 2 }, "HE-AAC", 0));
        assert_eq!(got.len(), frames.len() * 2048 * 2, "every frame");
        let src: Vec<f64> = pcm.iter().map(|&v| f64::from(v) / 32768.0).collect();
        for c in 0..2 {
            let (top, top_src) = (band_db(&got, 2, c, rate as usize, rate, 12_000.0, 16_000.0), band_db(&src, 2, c, rate as usize, rate, 12_000.0, 16_000.0));
            eprintln!("HE-AAC, channel {}: 12-16 kHz at {:.1} dB, the source's {:.1} dB", c, top, top_src);
            assert!((top - top_src).abs() < 6.0, "the top octave: {:.1} dB against the source's {:.1} dB", top, top_src);
        }
    }

    /// HE-AAC v2: a mono core made stereo by PS — the left and right as the
    /// source had them (each side's sine louder on its own side).
    #[test]
    fn he_aac_v2_makes_its_mono_core_stereo() {
        let rate = 44_100;
        let (stream, _) = encode(Profile::HeV2, rate, 2, 32_000, &bright(rate, 4.0));
        let frames = frames_of(&stream);
        let core = Header::parse(&frames[0]).unwrap();
        assert_eq!((core.rate(), core.channels), (22_050, 1), "an HE-AAC v2 stream's ADTS header: its mono core");
        let mut d = HeAac::new(&core).unwrap();
        let (got, refused) = decode_all(&mut d, &frames);
        assert_eq!((d.format(), d.codec(), refused), (Format { rate, channels: 2 }, "HE-AAC v2", 0));
        assert_eq!(got.len(), frames.len() * 2048 * 2, "every frame");
        let at = |c: usize, f: f64| band_db(&got, 2, c, rate as usize, rate, f - 20.0, f + 20.0);
        let (l440, r440, l660, r660) = (at(0, 440.0), at(1, 440.0), at(0, 660.0), at(1, 660.0));
        eprintln!("HE-AAC v2: 440 Hz L {:.1} R {:.1} dB; 660 Hz L {:.1} R {:.1} dB", l440, r440, l660, r660);
        assert!(l440 > r440 + 6.0 && r660 > l660 + 6.0, "the sides as the source had them");
    }

    /// A break and a reconnect as the radio meets them: the first
    /// connection's frames up to `cut`, the next one's from `from` (before
    /// `cut`: the server's burst repeats them), placed among the frames had
    /// by their bytes and decoded on by the same decoder. The stream as
    /// heard, and how the new connection was placed.
    pub fn reconnect(mut d: HeAac, frames: &[Vec<u8>], cut: usize, from: usize) -> (Vec<f64>, Placed) {
        let mut trail = Trail::new(4096);
        let mut out = vec![];
        for fr in &frames[..cut] {
            trail.had(frame_id(fr));
            d.push(fr, &mut out).expect("decodes");
        }
        let (mut held, mut ids, mut placed) = (vec![], vec![], None);
        for fr in &frames[from..] {
            if placed.is_some() {
                trail.had(frame_id(fr));
                d.push(fr, &mut out).expect("decodes");
                continue;
            }
            ids.push(frame_id(fr));
            held.push(fr.clone());
            if let Some(p) = trail.place(&ids) {
                placed = Some(p);
                let skip = if let Placed::Repeats(n) = p { n } else { 0 };
                for h in held.drain(..).skip(skip) {
                    trail.had(frame_id(&h));
                    d.push(&h, &mut out).expect("decodes");
                }
            }
        }
        // Never placed: audio was lost between the two (the radio gives up
        // after a while and starts afresh).
        (out, placed.unwrap_or(Placed::New))
    }

    /// The decoder goes on across a reconnect from the first frame it has
    /// not had: the stream as if it had never broken, to the bit; a
    /// reconnect after lost audio is not placed (a gap).
    #[test]
    fn a_new_connection_goes_on_in_the_same_decoder_as_if_it_had_never_broken() {
        let rate = 44_100;
        let (stream, _) = encode(Profile::He, rate, 2, 48_000, &bright(rate, 6.0));
        let frames = frames_of(&stream);
        let first = Header::parse(&frames[0]).unwrap();
        let mut whole = vec![];
        let mut d = HeAac::new(&first).unwrap();
        for fr in &frames {
            d.push(fr, &mut whole).expect("decodes");
        }
        let (cut, from) = (frames.len() * 6 / 10, frames.len() * 4 / 10);
        let (heard, placed) = reconnect(HeAac::new(&first).unwrap(), &frames, cut, from);
        assert_eq!(placed, Placed::Repeats(cut - from));
        assert!(heard == whole, "the stream as if unbroken: {} vs {} samples", heard.len(), whole.len());
        let (_, placed) = reconnect(HeAac::new(&first).unwrap(), &frames, from, cut);
        assert_eq!(placed, Placed::New);
    }

    /// Damaged frames (bytes of the payload changed, half a frame zeroed):
    /// the FDK decoder says which did not decode, does not panic, and keeps
    /// the stream's time with what it gives in their place.
    #[test]
    fn damaged_frames_keep_the_stream_s_time() {
        let rate = 44_100;
        let (stream, _) = encode(Profile::HeV2, rate, 2, 32_000, &bright(rate, 6.0));
        let mut frames = frames_of(&stream);
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for (k, f) in frames.iter_mut().enumerate() {
            if k % 23 == 5 && f.len() > 16 {
                for _ in 0..8 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let p = Header::LEN + (x as usize) % (f.len() - Header::LEN);
                    f[p] ^= (x >> 32) as u8 | 1;
                }
            }
            if k % 61 == 30 {
                let n = f.len();
                f[n / 2..].iter_mut().for_each(|b| *b = 0);
            }
        }
        let mut d = HeAac::new(&Header::parse(&frames[0]).unwrap()).unwrap();
        let (got, refused) = decode_all(&mut d, &frames);
        eprintln!("{} damaged-stream frames: {} did not decode, {} samples", frames.len(), refused, got.len());
        assert!(refused > 0, "some frames did not decode");
        assert!(!d.restarted(), "no panic");
        assert_eq!(got.len(), frames.len() * 2048 * 2, "the stream's time kept");
        assert!(got.iter().all(|v| v.is_finite()));
    }

    /// A panic inside the FDK decoder is caught: the frame is lost, the
    /// decoder is made afresh (`restarted`, once), and the stream goes on
    /// from the new one — its start-up frame dropped as any new decoder's.
    #[test]
    fn a_panic_in_the_fdk_decoder_makes_a_new_one_that_goes_on() {
        let rate = 44_100;
        let (stream, _) = encode(Profile::He, rate, 2, 48_000, &bright(rate, 3.0));
        let frames = frames_of(&stream);
        let mut d = HeAac::new(&Header::parse(&frames[0]).unwrap()).unwrap();
        let mut out = vec![];
        let k = frames.len() / 2;
        for (i, fr) in frames.iter().enumerate() {
            if i == k {
                super::super::fdk::PANIC_NEXT.with(|p| p.set(true));
                assert!(d.push(fr, &mut out).is_err(), "the frame it panicked on");
                assert!(d.restarted() && !d.restarted(), "made afresh, said once");
                continue;
            }
            d.push(fr, &mut out).expect("decodes");
        }
        d.finish(&mut out).expect("the last frame");
        assert_eq!(out.len(), (frames.len() - 2) * 2048 * 2, "one frame lost, one the new decoder's start-up");
        assert_eq!(d.format(), Format { rate, channels: 2 });
    }

    /// Dumps of live stations, kept outside the repository:
    /// AURA_TEST_ADTS="a.aac;b.aac" (HE-AAC v1 and v2 among them). Each
    /// decodes without a refused frame, and a reconnect goes on as if
    /// unbroken.
    #[test]
    #[ignore]
    fn live_dumps_decode_and_go_on_after_a_reconnect_as_if_unbroken() {
        use super::super::adts::{sniff, Sniffed};
        let Ok(list) = std::env::var("AURA_TEST_ADTS") else {
            eprintln!("SKIPPED: AURA_TEST_ADTS is not set");
            return;
        };
        for path in list.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let bytes = std::fs::read(path).expect("a dump");
            let at = match sniff(&bytes, true) {
                Sniffed::Adts(_, at) => at,
                s => panic!("{}: {:?}", path, s),
            };
            let frames = frames_of(&bytes[at..]);
            let first = Header::parse(&frames[0]).unwrap();
            let mut d = HeAac::new(&first).unwrap();
            let mut whole = vec![];
            let refused = frames.iter().filter(|fr| d.push(fr, &mut whole).is_err()).count();
            let (cut, from) = (frames.len() * 6 / 10, frames.len() * 4 / 10);
            let (heard, placed) = reconnect(HeAac::new(&first).unwrap(), &frames, cut, from);
            eprintln!("{}: {:?} {}, {} of {} frames refused; reconnect {:?}, as if unbroken: {}", path, d.format(), d.codec(), refused, frames.len(), placed, heard == whole);
            assert_eq!(refused, 0, "{}", path);
            assert_eq!(placed, Placed::Repeats(cut - from), "{}", path);
            assert!(heard == whole, "{}", path);
        }
    }
}
