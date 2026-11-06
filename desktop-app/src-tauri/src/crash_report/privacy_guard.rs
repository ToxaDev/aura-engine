//! What a report may hold, as tests that fail the build. The report is built
//! the real way from a session full of private things, then looked through:
//! every key must be on the list below, and no string may hold a planted value
//! or anything shaped like a path, an address, a name of a file or a lasting id.
//!
//! A new field in a report means a new line in `ALLOWED`, on purpose.

use super::known::Kind;
use super::pending::Pending;
use super::report::{build, build_redactor, Input, Moment, Parts};
use crate::player::output::DeviceInfo;
use crate::player::probe::TrackInfo;
use crate::player::settings::{OutputFormat, PlayerSettings};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Every key a report may have: `a.b` inside an object, `a[]` inside a list.
const ALLOWED: &[&str] = &[
    "schema_version",
    "note",
    "log",
    "log[]",
    "app",
    "app.version",
    "app.commit",
    "app.profile",
    "app.official",
    "app.uptime_s",
    "app.env",
    "app.env[]",
    "machine",
    "machine.os",
    "machine.kernel",
    "machine.arch",
    "machine.cpu",
    "machine.cores",
    "machine.threads",
    "machine.features",
    "machine.features.avx2",
    "machine.features.fma",
    "machine.features.avx512f",
    "machine.ram_total_mb",
    "machine.ram_free_mb",
    "machine.data_disk_free_gb",
    "gpu",
    "gpu.found",
    "gpu.name",
    "gpu.backend",
    "gpu.discrete",
    "gpu.vramMb",
    "gpu.freeMb",
    "gpu.supported",
    "gpu.player",
    "gpu.forcedOff",
    "gpu.reason",
    "gpu.driver",
    "audio",
    "audio.state",
    "audio.srcRate",
    "audio.bits",
    "audio.srcFormat",
    "audio.outRate",
    "audio.chainRate",
    "audio.exclusive",
    "audio.format",
    "audio.devFormat",
    "audio.bufferS",
    "audio.bufferTargetS",
    "audio.bufferMode",
    "audio.underrunFrames",
    "audio.renderRtf",
    "audio.latencyMs",
    "audio.rebuilding",
    "audio.streamDirect",
    "audio.chain",
    "audio.chain[]",
    "audio.dsd",
    "audio.dsd.label",
    "audio.dsd.rateHz",
    "audio.dsd.modulator",
    "audio.dsd.running",
    "audio.dsd.overloads",
    "audio.dsd.stepDowns",
    "audio.dsd.gaps",
    "audio.dsd.lateFrames",
    "audio.dsd.delayMs",
    "audio.dsd.asIs",
    "settings",
    "settings.mode",
    "settings.fsMultiplier",
    "settings.taps",
    "settings.phase",
    "settings.hpSens",
    "settings.hpNet",
    "settings.apodizing",
    "settings.adaptiveApodizer",
    "settings.headroomDb",
    "settings.iirDcBlocking",
    "settings.subsonicHz",
    "settings.declip",
    "settings.isp",
    "settings.adaptiveHeadroom",
    "settings.xtc",
    "settings.useGpu",
    "settings.upsampler",
    "settings.output",
    "settings.output.kind",
    "settings.output.multiple",
    "settings.output.modulator",
    "settings.eq",
    "settings.eq.on",
    "settings.eq.bands",
    "queue",
    "queue[]",
    "queue[].id",
    "queue[].codec",
    "queue[].rate",
    "queue[].bits",
    "queue[].ch",
    "queue[].duration_s",
    "queue[].size_mb",
    "queue[].kind",
    "queue[].state",
    "queue[].hp_prep",
    "event",
    "event.kind",
    "event.stage",
    "event.progress",
    "event.thread",
    "event.message",
    "event.backtrace",
    "event.markers",
    "event.last_stall",
];

/// Things a person would not want sent. Each is planted below somewhere a real
/// session holds it.
const PLANTED: &[&str] = &[
    "Музыка",
    "Пыльца",
    "Геометрия",
    "夜に駆ける",
    "YOASOBI",
    "ヨルシカ",
    "Hotel California",
    "Hell Freezes",
    "Eagles",
    "My Secret Mixtape",
    "dan.dzuban",
    "Dzuban",
    "Dan's AirPods",
    "AirPods",
    "Наушники",
    "Realtek",
    "Sennheiser",
    "somafm",
    "groovesalad",
    "radioparadise",
    "NAS-Living-Room",
    "PrivateShare",
    "Volumes",
    "14ad898c",
    "0a1b2c3d4e5f60718293a4b5",
    "aa:bb:cc:dd:ee:ff",
    "secretField",
    "someone@mail.example",
    "A Song On The Radio",
    "A Station Of Mine",
    "Gone Since",
    "Another Song",
    "Another Artist",
    "Living Room",
];

const GUID: &str = "{14ad898c-4e89-4348-897e-e4edca92000b}";

fn track(id: u64, path: &str, title: &str, artist: &str, album: &str) -> TrackInfo {
    TrackInfo {
        id,
        path: path.into(),
        title: title.into(),
        artist: artist.into(),
        album: album.into(),
        year: "1994".into(),
        duration_s: 391.4,
        sample_rate: 44100,
        bits: 16,
        channels: 2,
        format: String::new(),
    }
}

fn parts() -> Parts {
    let queue = vec![
        track(5, r"D:\Музыка\Eagles\Hell Freezes Over\06 - Hotel California.flac", "Hotel California (Live)", "Eagles", "Hell Freezes Over"),
        track(6, r"\\NAS-Living-Room\PrivateShare\夜に駆ける.dsf", "夜に駆ける", "YOASOBI", "THE BOOK"),
        track(7, r"E:\Пыльца - Геометрия.mp3", "Геометрия", "Пыльца", ""),
        track(8, "/home/dan.dzuban/Music/My Secret Mixtape.ogg", "My Secret Mixtape", "", ""),
        track(9, "/Volumes/Music/ヨルシカ/track 01.m4a", "", "ヨルシカ", ""),
    ];
    // Keys the report must leave behind sit among the ones it takes.
    let status = json!({
        "state": "playing", "trackId": 5, "outRate": 352800, "exclusive": true, "bufferS": 1.5, "chain": ["SUB15", "1M"],
        "device": "Dan's AirPods", "deviceId": GUID, "trackFile": "06 - Hotel California.flac",
        "pending": {"what": "Preparing Hotel California (Live)"}, "error": r"Cannot open D:\Музыка\x.flac",
        "radio": {"url": "http://ice1.somafm.com/groovesalad"}, "notes": ["Hotel California (Live)"],
        "secretField": "secret",
        "dsd": {
            "label": "DSD256", "rateHz": 11289600, "modulator": "sdm7", "running": "sdm7", "overloads": 0, "stepDowns": 0,
            "gaps": 0, "lateFrames": 0, "delayMs": 1.5, "asIs": false, "secretField": "secret",
        },
    });
    let mut settings = PlayerSettings::default();
    settings.output = OutputFormat::Dsd { multiple: 256, modulator: Default::default() };
    settings.eq = Some(serde_json::from_value(json!({"v": 2, "on": true, "name": "Sennheiser HD 800 of Dan", "bands": []})).unwrap());
    Parts {
        status: Some(status),
        queue,
        settings: Some(settings),
        hp_prepare: Some(json!({"track": 5, "state": "parts", "path": r"D:\Музыка\x.flac"})),
        devices: vec![
            DeviceInfo { id: format!("{{0.0.0.00000000}}.{GUID}"), name: "Dan's AirPods".into(), is_default: true },
            DeviceInfo { id: "hw:CARD=Realtek".into(), name: "Наушники (Realtek USB2.0 Audio)".into(), is_default: false },
        ],
        gpu: Some(json!({"found": true, "name": "NVIDIA GeForce RTX 4090", "backend": "Vulkan", "secretField": r"C:\x"})),
    }
}

/// What the rest of the library and the session has met, by kind.
fn met() -> Vec<(Kind, String)> {
    vec![
        (Kind::Url, "http://ice1.somafm.com/groovesalad".to_string()),
        (Kind::Url, "https://stream.radioparadise.com/flac?user=dan.dzuban".to_string()),
        (Kind::Path, r"D:\Музыка\Another Album\01 Another Song.wav".to_string()),
        (Kind::Title, "Another Song Of Mine".to_string()),
        (Kind::Tag, "Another Artist Of Mine".to_string()),
        (Kind::Device, "Living Room Speakers Of Dan".to_string()),
        (Kind::Title, "A Song On The Radio".to_string()),
        (Kind::Tag, "A Station Of Mine".to_string()),
    ]
}

/// Lines of the kind the program writes, each carrying something private.
fn log() -> Vec<String> {
    [
        r"[10:00:00.000] [CONV] Decoding: 06 - Hotel California.flac".to_string(),
        r"[10:00:00.100] [PLAYER] lamp of Hotel California (Live): 5 MB free past the margin, 9 MB needed".to_string(),
        r#"[10:00:00.200] [PLAYER] stream open: device="Dan's AirPods" rate=88200 ch=2"#.to_string(),
        r#"[10:00:00.201] [PLAYER] stream open: device="A Device Gone Since Then" rate=88200 ch=2"#.to_string(),
        format!("[10:00:00.210] [UI] device: {{0.0.0.00000000}}.{GUID}"),
        "[10:00:00.211] [UI] device: {0.0.0.00000000}.{aaaaaaaa-1111-2222-3333-444444444444}".to_string(),
        "[10:00:00.212] [PLAYER] an adapter aa:bb:cc:dd:ee:ff, serial 0a1b2c3d4e5f60718293a4b5".to_string(),
        r"[10:00:00.300] [PLAYER] 夜に駆ける: its start gives way".to_string(),
        r"[10:00:00.400] [UI] loaded D:\Музыка\Eagles\Hell Freezes Over\06 - Hotel California.flac".to_string(),
        r#"[10:00:00.401] [UI] loaded "D:\\Музыка\\Eagles\\Hell Freezes Over\\06 - Hotel California.flac""#.to_string(),
        r"[10:00:00.402] [UI] loaded D:/Музыка/Eagles/x y.flac".to_string(),
        r"[10:00:00.403] [UI] loaded \\?\D:\Музыка\Eagles\x y.flac".to_string(),
        r"[10:00:00.404] [UI] loaded \\NAS-Living-Room\PrivateShare\夜に駆ける.dsf".to_string(),
        "[10:00:00.405] [UI] loaded /home/dan.dzuban/Music/My Secret Mixtape.ogg".to_string(),
        "[10:00:00.406] [UI] loaded /Volumes/Music/ヨルシカ/track 01.m4a".to_string(),
        "[10:00:00.407] [UI] loaded file:///D:/Private%20Folder/Song.flac".to_string(),
        "[10:00:00.408] [CONV] An Unknown Song Name.FLAC done".to_string(),
        "[10:00:00.500] [RADIO] tune http://ice1.somafm.com/groovesalad at Eagles".to_string(),
        "[10:00:00.501] [RADIO] tune https://stream.radioparadise.com/flac?user=dan.dzuban".to_string(),
        "[10:00:00.502] [RADIO] redirected to stream.radioparadise.com/aac-320".to_string(),
        "[10:00:00.503] [RADIO] mms://radio.example/a and rtsp://10.0.0.5:554/b".to_string(),
        r"[10:00:00.600] [HOME] C:\Users\dan.dzuban\AppData\Local\AuraEngine\logs written".to_string(),
        "[10:00:00.601] [HOME] /Users/dan.dzuban/Library/Application Support/AuraEngine written".to_string(),
        "[10:00:00.700] [MAIL] from someone@mail.example".to_string(),
        "[10:00:00.800] [PLAYER] Another Song Of Mine by Another Artist Of Mine on Living Room Speakers Of Dan".to_string(),
        "[10:00:00.810] [RADIO] t=4.0s buffer=1.00s title=A Song On The Radio".to_string(),
        "[10:00:00.811] [RADIO] connected #1 in 5 ms: (audio/flac) A Station Of Mine".to_string(),
        "[10:00:00.900] [WATCHDOG] stall prep: no change for 31 s on Hotel California (Live)".to_string(),
    ]
    .into()
}

const NOTE: &str = r"It stops at Hotel California (Live), see D:\Музыка\x.flac, write to someone@mail.example";

fn report(about_last: bool, pending: Option<&Pending>) -> Value {
    let parts = parts();
    let log = log();
    let r = build_redactor(&parts.queue, &parts.devices, &met());
    build(&Input { parts: &parts, about_last, pending, log: &log, note: NOTE, redactor: &r, moment: &moment() })
}

/// The moment is planted too: the process's own is the whole test run's, moved
/// by any test running alongside.
fn moment() -> Moment {
    Moment {
        stage: "prep: Hotel California (Live)".into(),
        progress: 7,
        markers: "prep=parts of Hotel California (Live)".into(),
        env: vec!["AURA_FILTER_DIR".into()],
    }
}

fn fault() -> Pending {
    Pending {
        kind: "panic".into(),
        thread: "aura-control".into(),
        message: r"cannot open D:\Музыка\Eagles\06 - Hotel California.flac on Dan's AirPods".into(),
        backtrace: "at aura_engine::x\n  at C:\\Users\\dan.dzuban\\src\\x.rs:10\n  at /home/dan.dzuban/src/y.rs:20".into(),
        markers: "prep=parts of Hotel California (Live)".into(),
        stage: "prep: Hotel California (Live)".into(),
        progress: 7,
        asked: true,
    }
}

/// A manual report, one after an unclean end, and one after a fault.
fn every_kind() -> Vec<Value> {
    vec![report(false, None), report(true, None), report(true, Some(&fault()))]
}

fn keys_of(v: &Value, at: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                let path = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                keys_of(x, &path, out);
                out.insert(path);
            }
        }
        Value::Array(a) => {
            let path = format!("{at}[]");
            for x in a {
                keys_of(x, &path, out);
            }
            if !a.is_empty() {
                out.insert(path);
            }
        }
        _ => {}
    }
}

fn strings_of<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| strings_of(x, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| {
            out.push(k);
            strings_of(x, out);
        }),
        _ => {}
    }
}

#[test]
fn every_key_of_a_report_is_on_the_list() {
    let allowed: BTreeSet<&str> = ALLOWED.iter().copied().collect();
    for v in every_kind() {
        let mut keys = BTreeSet::new();
        keys_of(&v, "", &mut keys);
        let extra: Vec<&String> = keys.iter().filter(|k| !allowed.contains(k.as_str())).collect();
        assert!(extra.is_empty(), "keys not on the list: {extra:?}");
    }
}

#[test]
fn the_list_is_what_a_report_really_has() {
    // A section gone from the report would pass the walk above by being empty.
    let mut keys = BTreeSet::new();
    every_kind().iter().for_each(|v| keys_of(v, "", &mut keys));
    for k in ["app.version", "machine.cpu", "gpu.name", "audio.dsd.rateHz", "settings.output.multiple", "settings.eq.bands", "queue[].codec", "event.backtrace", "log[]", "note"] {
        assert!(keys.contains(k), "{k} is missing");
    }
}

#[test]
fn no_planted_value_is_anywhere_in_a_report() {
    for v in every_kind() {
        let mut strings = Vec::new();
        strings_of(&v, &mut strings);
        for s in strings {
            let low = s.to_lowercase();
            for p in PLANTED {
                assert!(!low.contains(&p.to_lowercase()), "{p} leaked in: {s}");
            }
        }
    }
}

#[test]
fn nothing_shaped_like_a_path_an_address_or_a_lasting_id_is_in_a_report() {
    let shapes = [
        ("a drive path", r"(?i)\b[a-z]:[\\/]"),
        ("a UNC path", r"\\\\[^\s\\]+\\"),
        ("a unix path", r#"(^|[\s"'=(\[,:])~?/[^/\s]+/"#),
        ("a URL", r"(?i)\b[a-z][a-z0-9+.\-]{1,15}://"),
        ("a host and path", r"(?i)\b[a-z0-9\-]+\.[a-z]{2,}(:\d+)?/"),
        ("an e-mail", r"[\w.+\-]+@[\w\-]+\.\w+"),
        ("an audio file", r"(?i)\.(flac|wav|aiff?|mp3|m4a|aac|ogg|opus|wma|ape|wv|dsf|dff|mka|mp4|m3u8?|pls|cue)\b"),
        ("a GUID", r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"),
        ("a long hex id", r"(?i)\b[0-9a-f]{16,}\b"),
        ("a MAC address", r"(?i)\b(?:[0-9a-f]{2}[:\-]){5}[0-9a-f]{2}\b"),
    ];
    let compiled: Vec<(&str, Regex)> = shapes.iter().map(|(n, r)| (*n, Regex::new(r).unwrap())).collect();
    for v in every_kind() {
        let mut strings = Vec::new();
        strings_of(&v, &mut strings);
        for s in strings {
            for (name, re) in &compiled {
                assert!(!re.is_match(s), "{name} in: {s}");
            }
        }
    }
}

#[test]
fn a_report_holds_no_id_of_the_person_or_the_machine() {
    // Two reports of one session differ only in what the moment changes: there
    // is nothing random or kept between them to tie them to one person.
    let same = |mut v: Value| {
        v["app"]["uptime_s"] = json!(0);
        v["machine"]["ram_free_mb"] = json!(0);
        v["machine"]["data_disk_free_gb"] = json!(0);
        v
    };
    assert_eq!(same(report(false, None)), same(report(false, None)));
    let v = report(false, None);
    let mut keys = BTreeSet::new();
    keys_of(&v, "", &mut keys);
    let id_like = Regex::new(r"(?i)uuid|guid|install|client|machine_?id|device_?id|user|host|serial|account|token").unwrap();
    assert!(!keys.iter().any(|k| id_like.is_match(k)), "{keys:?}");
    // The one `id` there is: the track's place in this report's queue.
    for (i, t) in v["queue"].as_array().unwrap().iter().enumerate() {
        assert_eq!(t["id"], json!(format!("track#{}", i + 1)));
    }
}
