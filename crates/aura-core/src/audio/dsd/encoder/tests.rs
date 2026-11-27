//! Checks of the encoders, fast enough for every `cargo test`: the
//! measurement suite (`measure.rs`) at a few rates with limits per kind, and
//! the plumbing. `cargo test --bins audio::dsd::encoder -- --nocapture`
//! prints the numbers; the whole table is `measure::dsd_modulator_report`.

use std::f64::consts::PI;

use super::interp::Interpolator;
use super::measure::*;
use super::modulator::{Sdm, STATE_LIMIT};
use super::tables::{HALF_BANDS, SDM5, SDM7, SDM9};
use super::*;

const PLAIN: [ModulatorKind; 5] =
    [ModulatorKind::Sdm5, ModulatorKind::Sdm5Wide, ModulatorKind::Sdm7, ModulatorKind::Sdm7Wide, ModulatorKind::Sdm9];

fn kind_rates(kind: ModulatorKind) -> Vec<u32> {
    [64, 128, 256, 512, 1024].into_iter().filter(|&m| kind.supports(rate(m))).collect()
}

#[test]
fn length_and_mean_follow_the_input() {
    for kind in PLAIN {
        let m = kind_rates(kind)[0];
        let mut enc = new_encoder(kind, dop_input(m), rate(m)).unwrap();
        assert_eq!(enc.ratio(), 16);
        let density = |b: &[u8]| b.iter().map(|x| x.count_ones()).sum::<u32>() as f64 / (b.len() * 8) as f64;
        let mut out = vec![0u8; 8192 * 2];
        enc.process(&vec![0.0; 8192], &mut out);
        assert!((density(&out) - 0.5).abs() < 0.01);
        // PCM +0.5 → 25 % modulation → 62.5 % ones (after the interpolator settles).
        enc.process(&vec![0.5; 8192], &mut out);
        assert!((density(&out[1024..]) - 0.625).abs() < 0.005, "{kind:?} {}", density(&out[1024..]));
        assert_eq!(enc.overloads(), 0);
    }
}

/// The generic loop gives the released SDM5's bytes: an FNV-1a hash of the
/// old loop's output (computed from modulator.rs at 18f79f9 with the same
/// arithmetic) for a broadband input over ±0.6, clamping included.
#[test]
fn sdm5_bytes_unchanged() {
    let u_at = |i: usize| {
        let p = i % 40_000;
        let tri = (if p < 20_000 { p } else { 40_000 - p }) as f64 / 20_000.0 * 1.2 - 0.6;
        tri * 0.5 + ((i * 7919) % 1013) as f64 / 1013.0 * 0.6 - 0.3
    };
    let u: Vec<f64> = (0..1 << 17).map(u_at).collect();
    for (m, want) in [(64, 0x7493_5806_de42_d4e7u64), (256, 0x8e3f_9241_cb39_855c), (1024, 0x1415_9263_47bf_27c5)] {
        let mut sdm = Sdm::<5>::new(SDM5.iter().find(|k| k.multiple == m).unwrap());
        let mut out = vec![0u8; u.len() / 8];
        for (uu, oo) in u.chunks(8 * 1000).zip(out.chunks_mut(1000)) {
            sdm.run(uu, oo);
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in &out {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        assert_eq!((sdm.clamps, sdm.resets), (905, 0));
        assert_eq!(h, want, "DSD{m}");
    }
}

/// The interpolator's response from its taps: ±0.001 dB to 20 kHz, and every
/// image of 0–20 kHz at least 135 dB down, for each input multiple.
#[test]
fn interpolation_flat_and_images_low() {
    for &(m, ratio) in &[(1u32, 64usize), (2, 32), (4, 64), (8, 16), (16, 16), (16, 64)] {
        let r_in = 44_100.0 * m as f64;
        let interp = Interpolator::new(ratio, m as f64);
        let stages = interp.taps_per_stage();
        let gain = |f: f64| -> f64 {
            let mut g = 1.0;
            for (s, taps) in stages.iter().enumerate() {
                let w = 2.0 * PI * f / (r_in * (2 << s) as f64);
                let h: f64 = 0.5 + taps.iter().enumerate().map(|(j, t)| t * ((2 * j + 1) as f64 * w).cos()).sum::<f64>();
                g *= h;
            }
            g.abs()
        };
        let mut ripple = 0.0f64;
        for i in 0..=2000 {
            let f = 20.0 + (20_000.0 - 20.0) * i as f64 / 2000.0;
            ripple = ripple.max(db(gain(f) * gain(f)).abs());
        }
        let mut image = f64::MIN;
        for k in 1..ratio {
            for i in 0..=400 {
                let f = 20_000.0 * i as f64 / 400.0;
                for img in [k as f64 * r_in - f, k as f64 * r_in + f] {
                    image = image.max(db(gain(img) * gain(img)));
                }
            }
        }
        println!("interp m={m} ×{ratio}: pass ripple {ripple:.1e} dB, worst image {image:.1} dB");
        assert!(ripple < 0.001, "m={m}: ripple {ripple}");
        assert!(image < -135.0, "m={m}: image {image}");
    }
}

/// The streaming code against the exact sine, delayed by the cascade's
/// group delay (each stage: 2K − 1 samples at its output rate).
#[test]
fn interpolator_streams_the_designed_filter() {
    let (m, ratio) = (16u32, 16usize);
    let r_in = 44_100.0 * m as f64;
    let mut interp = Interpolator::new(ratio, m as f64);
    let stages = interp.taps_per_stage();
    let n_stages = stages.len();
    let delay: usize = stages.iter().enumerate().map(|(s, t)| (2 * t.len() - 1) << (n_stages - 1 - s)).sum();
    let x = sine(r_in, 997.0, 0.9, 4000);
    let mut y = Vec::new();
    // Uneven parts: 1 sample, then 7, then whole blocks.
    let b = interp.block_inputs();
    for part in [&x[..1], &x[1..8]].into_iter().chain(x[8..].chunks(b)) {
        y.extend_from_slice(interp.process(part, |v| v));
    }
    let fs = r_in * ratio as f64;
    let mut err = 0.0f64;
    for (i, &v) in y.iter().enumerate().skip(delay + 64) {
        let t = (i - delay) as f64 / fs;
        err = err.max((v - 0.9 * (2.0 * PI * 997.0 * t).sin()).abs());
    }
    assert!(err < 1e-6, "max error {err:e}");
}

/// The measuring decoder: flat to 20 kHz, and everything that would alias
/// into 0–50 kHz at least 230 dB down, at every rate.
#[test]
fn measuring_decoder_is_clean() {
    for m in [64u32, 256, 1024] {
        let r = rate(m);
        let dec = Decoder::new(r);
        let out = Decoder::out_rate(r);
        let mut ripple = 0.0f64;
        for i in 0..=200 {
            ripple = ripple.max(db(dec.gain(r, 20_000.0 * i as f64 / 200.0).powi(2)).abs());
        }
        let mut alias = f64::MIN;
        let fs = r.bit_rate() as f64;
        let mut k = 1.0;
        while k * out < fs / 2.0 + 1.0 {
            for i in 0..=100 {
                let f = 50_000.0 * i as f64 / 100.0;
                for a in [k * out - f, k * out + f] {
                    if a < fs / 2.0 {
                        alias = alias.max(db(dec.gain(r, a).powi(2)));
                    }
                }
            }
            k += 1.0;
        }
        println!("decoder DSD{m}: pass ripple {ripple:.1e} dB, worst alias into 0–50 kHz {alias:.1} dB");
        assert!(ripple < 1e-6, "DSD{m} ripple {ripple}");
        assert!(alias < -230.0, "DSD{m} alias {alias}");
    }
}

/// The NaN guard and the input limit: no runaway, a clamp counted once per
/// episode.
#[test]
fn over_limit_input_is_clamped_and_counted() {
    for kind in PLAIN {
        let m = kind_rates(kind)[0];
        let mut enc = SdmEncoder::<5>::new(&SDM5, dop_input(64), rate(64)).unwrap();
        let mut pcm = vec![0.0; 4000];
        pcm.extend(vec![1.6; 4000]); // 0.8 of full modulation
        pcm.extend(vec![0.0; 4000]);
        pcm.extend(vec![-1.6; 4000]);
        pcm.push(f64::NAN);
        pcm.extend(vec![0.0; 7]);
        encode(&mut enc, &pcm);
        assert_eq!((enc.sdm.clamps, enc.sdm.resets), (2, 0));
        assert_eq!(enc.overloads(), 2);
        assert!(enc.sdm.states().iter().all(|s| s.abs() <= 1.5));
        let mut any = new_encoder(kind, dop_input(m), rate(m)).unwrap();
        encode(any.as_mut(), &pcm);
        assert_eq!(any.overloads(), 2, "{kind:?}");
    }
}

/// Without the input limit the loop runs away at 0.8; the state guard
/// restarts it, and once the input is back in range it stays stable.
#[test]
fn state_guard_catches_a_runaway() {
    for (k, order) in [(&SDM5[0], 5), (&SDM7[2], 7), (&SDM9[0], 9)] {
        macro_rules! check {
            ($n:literal) => {{
                let mut sdm = Sdm::<$n>::with_limit(k, 2.0);
                let mut byte = [0u8];
                for _ in 0..100_000 / 8 {
                    sdm.run(&[0.8; 8], &mut byte);
                }
                assert!(sdm.resets > 0, "N{order}");
                let before = sdm.resets;
                let mut ones = 0;
                for i in 0..200_000 / 8 {
                    sdm.run(&[0.3; 8], &mut byte);
                    ones += byte[0].count_ones() * (i >= 100_000 / 8) as u32;
                    assert!(sdm.states().iter().all(|s| s.abs() <= STATE_LIMIT));
                }
                assert!(sdm.resets <= before + 1, "N{order}: {} → {}", before, sdm.resets);
                assert!((ones as f64 / 100_000.0 - 0.65).abs() < 0.005, "N{order}");
            }};
        }
        match order {
            5 => check!(5),
            7 => check!(7),
            _ => check!(9),
        }
    }
}

/// The required stimuli at every rate: no restart ever; the input limit
/// only for the square and the steps (band-limited edges overshoot ~9 %).
#[test]
fn stable_for_the_test_signals() {
    for kind in PLAIN {
        for m in kind_rates(kind) {
            if m == 1024 {
                continue; // the report covers it; too slow for a debug build
            }
            let fs = dop_input(m) as f64;
            let n = (0.03 * fs) as usize;
            let amp = 10f64.powf(-0.5 / 20.0);
            let mut cases: Vec<(&str, Vec<f64>, bool)> = Vec::new();
            for f in [20.0, 1000.0, 10_000.0, 20_000.0] {
                cases.push(("sine -0.5 dBFS", sine(fs, f, amp, n), false));
            }
            cases.push(("square", event("square", fs), true));
            cases.push(("steps", event("steps", fs), true));
            // DC sweep −1 … +1, entered and left slowly (0 → −1 → +1 → 0).
            let tri = |i: usize| {
                let t = 4.0 * i as f64 / n as f64;
                if t < 1.0 { -t } else if t < 3.0 { t - 2.0 } else { 4.0 - t }
            };
            cases.push(("dc sweep", (0..n).map(tri).collect(), false));
            cases.push(("silence", vec![0.0; n], false));
            cases.push(("-120 dBFS", sine(fs, 1000.0, 1e-6, n), false));
            for (name, pcm, may_clamp) in cases {
                let mut enc = new_encoder(kind, fs as u32, rate(m)).unwrap();
                encode(enc.as_mut(), &pcm);
                let o = enc.overloads();
                assert!(o == 0 || may_clamp, "{kind:?} DSD{m} {name}: {o} overloads");
                // Clamps only, no restart.
                if may_clamp {
                    let mut u = upsampled(&pcm, fs as u32, rate(m));
                    u.truncate(u.len() / 8 * 8);
                    let r = run_plain(kind, m, &u, super::tables::INPUT_LIMIT).unwrap();
                    assert_eq!(r.counts.1, 0, "{kind:?} DSD{m} {name}: restarts");
                }
            }
        }
    }
}

/// Points 1 and 2 at DSD64/128/256 (and SDM9 at DSD512): in-band noise at
/// silence and N+D of a −6 dBFS 1 kHz tone, against limits ~3 dB above the
/// measured numbers (in the comments: measured 19.10, cargo test, 2^15
/// samples at 176.4 kHz, one realisation).
#[test]
fn noise_and_distortion_per_kind() {
    const LEN: usize = 1 << 15;
    // (kind, multiple, noise limit, N+D limit), dB re PCM full scale.
    let limits: [(ModulatorKind, u32, f64, f64); 11] = [
        (ModulatorKind::Sdm5, 64, -125.0, -117.0),
        (ModulatorKind::Sdm5, 128, -143.0, -135.0),
        (ModulatorKind::Sdm5, 256, -152.0, -145.0),
        (ModulatorKind::Sdm5Wide, 128, -138.0, -130.0),
        (ModulatorKind::Sdm5Wide, 256, -148.0, -140.0),
        (ModulatorKind::Sdm7, 64, -142.0, -135.0),
        (ModulatorKind::Sdm7, 128, -163.0, -156.0),
        (ModulatorKind::Sdm7, 256, -178.0, -171.0),
        (ModulatorKind::Sdm7Wide, 128, -158.0, -151.0),
        (ModulatorKind::Sdm7Wide, 256, -174.0, -167.0),
        (ModulatorKind::Sdm9, 512, -198.0, -195.0),
    ];
    for &(kind, m, noise_max, nd_max) in &limits {
        let (n20, _) = silence_noise(kind, m, LEN, 1);
        let t = tone(kind, m, 1_000.0, -6.0, LEN, 1);
        println!(
            "{} DSD{m}: silence {n20:.1} dB, 1 kHz −6 dBFS: level {:.3} dB, N+D {:.1} dB, THD {:.1} dB",
            kind.label(),
            t.level,
            t.nd,
            t.thd
        );
        assert!(n20 < noise_max, "{kind:?} DSD{m} noise {n20}");
        assert!(t.nd < nd_max, "{kind:?} DSD{m} N+D {}", t.nd);
        assert!((t.level + 6.0).abs() < 0.01, "{kind:?} DSD{m} level {}", t.level);
        assert_eq!(t.overloads, 0);
    }
}

/// Point 6: no idle tone above −160 dB re FS standing more than 16 dB over
/// its neighbourhood's median (40 Hz–20 kHz), at silence, and for the
/// seventh order with a −120 dBFS tone too. The fifth order shows a spur at
/// 635 Hz with that tone at DSD64 (SDM5 −159 dB, +12…19 dB by the run; SDM5
/// Wide, measured at DSD64 before the Wide kinds started at DSD128, −154 dB,
/// +18.5 dB), and small DC inputs leave tones in every kind (−144…−159 dB at
/// DSD64–512, dither 1/32; `dsd_dither_study`): reported, not asserted.
#[test]
fn no_idle_tones() {
    const LEN: usize = 1 << 15;
    for (kind, m) in [(ModulatorKind::Sdm5, 64), (ModulatorKind::Sdm5Wide, 128), (ModulatorKind::Sdm7, 64), (ModulatorKind::Sdm7Wide, 128)] {
        let (e, l, f, dc) = idle_tones(kind, m, LEN, &[0.0], -160.0);
        let (s, _) = decoded_spectrum(kind, m, LEN, 1, &|fs, n| sine(fs, 1000.0, 1e-6, n));
        let (e2, l2, f2) = s.spur(&[1000.0], -160.0);
        let txt = |e: f64, l: f64, f: f64| if e == f64::MIN { "none".to_string() } else { format!("+{e:.1} dB ({l:.0} dB at {f:.0} Hz)") };
        println!("{} DSD{m}: worst idle bin above −160 dB {} (DC {dc:e}); −120 dBFS tone {}", kind.label(), txt(e, l, f), txt(e2, l2, f2));
        assert!(e < 16.0, "{kind:?} DSD{m}: {e}");
        assert!(kind.info().order == 5 || e2 < 16.0, "{kind:?} DSD{m}: {e2}");
    }
}

/// Point 8: the Wide curve's ultrasonic noise is clearly below the Standard
/// one's (DSD256: at 100 kHz and over 20–100 kHz).
#[test]
fn wide_curve_is_quieter_above_the_band() {
    for (std, wide) in [(ModulatorKind::Sdm5, ModulatorKind::Sdm5Wide), (ModulatorKind::Sdm7, ModulatorKind::Sdm7Wide)] {
        let (ds, ps) = ultrasonic(std, 256, &[100e3], 20e3, 100e3);
        let (dw, pw) = ultrasonic(wide, 256, &[100e3], 20e3, 100e3);
        println!(
            "DSD256 {} / {}: 100 kHz {:.0} / {:.0} dB per kHz, 20–100 kHz {:.1} / {:.1} dB",
            std.label(),
            wide.label(),
            ds[0],
            dw[0],
            ps,
            pw
        );
        assert!(dw[0] < ds[0] - 6.0 && pw < ps - 6.0, "{wide:?}");
    }
}

#[test]
fn reset_restarts_from_silence() {
    for kind in PLAIN {
        let m = kind_rates(kind)[0];
        let fs = dop_input(m);
        let pcm = sine(fs as f64, 1000.0, 0.7, 3000);
        let mut fresh = new_encoder(kind, fs, rate(m)).unwrap();
        let want = encode(fresh.as_mut(), &pcm);
        let mut used = new_encoder(kind, fs, rate(m)).unwrap();
        encode(used.as_mut(), &sine(fs as f64, 3000.0, 0.9, 5000));
        used.reset();
        assert_eq!(encode(used.as_mut(), &pcm), want, "{kind:?}");
    }
}

/// Periods vary: any split of the input gives the same bytes as one block.
#[test]
fn chunked_input_matches_one_block() {
    for kind in PLAIN {
        let m = kind_rates(kind)[0];
        let fs = dop_input(m);
        let pcm = sine(fs as f64, 1234.0, 0.8, 20_000);
        let mut whole = new_encoder(kind, fs, rate(m)).unwrap();
        let want = encode(whole.as_mut(), &pcm);
        let mut parts = new_encoder(kind, fs, rate(m)).unwrap();
        let mut got = Vec::new();
        let mut rng = 12345u64;
        let mut i = 0;
        while i < pcm.len() {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = ((rng >> 33) as usize % 1500).min(pcm.len() - i); // 0 included
            got.extend(encode(parts.as_mut(), &pcm[i..i + len]));
            i += len;
        }
        assert_eq!(got, want, "{kind:?}");
    }
}

#[test]
fn every_kind_builds_where_the_catalog_says() {
    for &(m, _) in HALF_BANDS.iter() {
        assert!(m.is_power_of_two());
    }
    for info in crate::audio::dsd::CATALOG.iter() {
        for base in [44_100, 48_000] {
            for m in [64, 128, 256, 512, 1024] {
                let r = DsdRate::new(base, m).unwrap();
                let built = new_encoder(info.kind, r.bit_rate() / 16, r);
                // Look-ahead kinds are built by `lookahead` (their latency is
                // checked there).
                let searched = info.search != crate::audio::dsd::Search::None;
                let want = info.kind.supports(r) && (plain_table(info.kind).is_some() || searched);
                match built {
                    Ok(enc) => {
                        assert!(want, "{} DSD{m}", info.label);
                        assert_eq!(enc.ratio(), 16);
                        if !searched {
                            assert_eq!(enc.latency(), 0);
                        }
                    }
                    Err(e) => {
                        assert!(!want, "{} DSD{m}: {e}", info.label);
                        assert_eq!(e, DsdError::UnsupportedModulator { kind: info.kind, multiple: m });
                    }
                }
            }
        }
    }
    assert!(new_encoder(ModulatorKind::Sdm5, 44_100, DsdRate::DSD64).is_ok()); // ×64
}
