//! The report's JSON. Built by whitelist: every field is named here, so what
//! is not named here does not go, whatever the program holds at the time.

use super::known::Kind;
use super::pending::Pending;
use super::redact::Redactor;
use crate::player::output::DeviceInfo;
use crate::player::probe::TrackInfo;
use crate::player::settings::PlayerSettings;
use serde_json::{json, Map, Value};
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
/// How much of the log's end goes.
pub const LOG_LINES: usize = 2000;
/// Read no further back than this for those lines.
const LOG_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// What the running program can say, each part may be missing: a program
/// that hangs cannot answer everything.
#[derive(Default)]
pub struct Parts {
    pub status: Option<Value>,
    pub queue: Vec<TrackInfo>,
    pub settings: Option<PlayerSettings>,
    pub hp_prepare: Option<Value>,
    pub devices: Vec<DeviceInfo>,
    pub gpu: Option<Value>,
}

/// What the whole process is at when a report is built: the heavy threads'
/// markers and the variables set. Read once, so one report tells of one moment.
pub struct Moment {
    pub stage: String,
    pub progress: u64,
    pub markers: String,
    pub env: Vec<String>,
}

impl Moment {
    pub fn now() -> Self {
        let (stage, progress) = crate::watchdog::busiest().map_or((String::new(), 0), |(m, s, p)| (format!("{m}: {s}"), p));
        Moment { stage, progress, markers: crate::watchdog::markers_line(), env: env_names() }
    }
}

// ── the sections ─────────────────────────────────────────────────────────

/// The names of the `AURA_*` variables set, never their values.
pub fn env_names() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("AURA_") && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .collect();
    names.sort();
    names
}

fn app_section(env: &[String]) -> Value {
    json!({
        "version": crate::ui_pack::version_label(),
        "commit": option_env!("AURA_BUILD_COMMIT").unwrap_or("?"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "official": option_env!("AURA_RELEASE_BUILD").is_some(),
        "uptime_s": crate::watchdog::uptime_s(),
        "env": env,
    })
}

fn cpu_features() -> Value {
    #[cfg(target_arch = "x86_64")]
    {
        json!({
            "avx2": is_x86_feature_detected!("avx2"),
            "fma": is_x86_feature_detected!("fma"),
            "avx512f": is_x86_feature_detected!("avx512f"),
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        json!({})
    }
}

fn machine_section() -> Value {
    use sysinfo::{CpuExt, System, SystemExt};
    let mut sys = System::new();
    sys.refresh_cpu();
    let (total_mb, free_mb) = crate::audio::memory::ram_now_mb();
    let disk = crate::app_dir::root().and_then(|d| crate::audio::disk_space::free_bytes(&d));
    json!({
        "os": sys.long_os_version(),
        "kernel": sys.kernel_version(),
        "arch": std::env::consts::ARCH,
        "cpu": sys.cpus().first().map(|c| c.brand().trim().to_string()),
        "cores": sys.physical_core_count(),
        "threads": sys.cpus().len(),
        "features": cpu_features(),
        "ram_total_mb": total_mb,
        "ram_free_mb": free_mb,
        "data_disk_free_gb": disk.map(|b| b >> 30),
    })
}

/// The named keys of an object that has them.
fn pick(from: Option<&Value>, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Some(v) = from {
        for k in keys {
            if let Some(x) = v.get(*k) {
                out.insert((*k).to_string(), x.clone());
            }
        }
    }
    Value::Object(out)
}

fn gpu_section(gpu: Option<&Value>) -> Value {
    pick(gpu, &["found", "name", "backend", "discrete", "vramMb", "freeMb", "supported", "player", "forcedOff", "reason", "driver"])
}

/// Numbers and modes of the output. Not `device`, not `trackFile`.
fn audio_section(status: Option<&Value>) -> Value {
    let mut out = pick(
        status,
        &[
            "state", "srcRate", "bits", "srcFormat", "outRate", "chainRate", "exclusive", "format", "devFormat",
            "bufferS", "bufferTargetS", "bufferMode", "underrunFrames", "renderRtf", "latencyMs", "rebuilding",
            "streamDirect", "chain",
        ],
    );
    if let Some(dsd) = status.and_then(|s| s.get("dsd")).filter(|d| d.is_object()) {
        out["dsd"] = pick(
            Some(dsd),
            &["label", "rateHz", "modulator", "running", "overloads", "stepDowns", "gaps", "lateFrames", "delayMs", "asIs"],
        );
    }
    out
}

/// The rack's numbers and modes. The EQ only as on/off and how many bands: its
/// name may be a person's headphones.
fn settings_section(s: Option<&PlayerSettings>) -> Value {
    let Some(s) = s else { return json!({}) };
    let v = serde_json::to_value(s).unwrap_or(Value::Null);
    let mut out = pick(
        Some(&v),
        &[
            "mode", "fsMultiplier", "taps", "phase", "hpSens", "hpNet", "apodizing", "adaptiveApodizer", "headroomDb",
            "iirDcBlocking", "subsonicHz", "declip", "isp", "adaptiveHeadroom", "xtc", "useGpu", "upsampler",
        ],
    );
    out["output"] = pick(v.get("output"), &["kind", "multiple", "modulator"]);
    out["eq"] = json!({
        "on": s.eq.as_ref().is_some_and(|e| e.on),
        "bands": s.eq.as_ref().map_or(0, |e| e.bands.len()),
    });
    out
}

/// A codec named only if it is one we know: an extension can be anything.
fn codec_of(path: &str) -> &'static str {
    const KNOWN: &[&str] = &["flac", "wav", "aif", "aiff", "mp3", "m4a", "aac", "ogg", "opus", "wma", "ape", "wv", "dsf", "dff", "mka", "mp4"];
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    KNOWN.iter().find(|k| **k == ext).copied().unwrap_or("other")
}

fn track_entry(i: usize, t: &TrackInfo, size_bytes: Option<u64>, state: &str) -> Value {
    json!({
        "id": format!("track#{}", i + 1),
        "codec": codec_of(&t.path),
        "rate": t.sample_rate,
        "bits": t.bits,
        "ch": t.channels,
        "duration_s": t.duration_s.round() as u64,
        "size_mb": size_bytes.map(|b| (b + (1 << 19)) >> 20),
        "kind": if t.format.is_empty() { "PCM".to_string() } else { t.format.clone() },
        "state": state,
    })
}

fn queue_section(queue: &[TrackInfo], status: Option<&Value>, hp: Option<&Value>) -> Value {
    let on_air = status.and_then(|s| s.get("trackId")).and_then(Value::as_u64);
    let hp_track = hp.and_then(|h| h.get("track")).and_then(Value::as_u64);
    let hp_state = hp.and_then(|h| h.get("state")).and_then(Value::as_str).unwrap_or("?");
    Value::Array(
        queue
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let state = if on_air == Some(t.id) { "on air" } else { "waiting" };
                let mut e = track_entry(i, t, std::fs::metadata(&t.path).ok().map(|m| m.len()), state);
                if hp_track == Some(t.id) {
                    e["hp_prep"] = json!(hp_state);
                }
                e
            })
            .collect(),
    )
}

fn last_stall(log: &[String]) -> Option<&String> {
    log.iter().rev().find(|l| l.contains("[WATCHDOG] stall"))
}

/// How a session that left no fault of its own behind ended, by its log.
pub fn ended_as(log: &[String]) -> &'static str {
    if last_stall(log).is_some() { "stall" } else { "unclean_exit" }
}

/// What the report is about.
pub fn event_section(about_last: bool, pending: Option<&Pending>, log: &[String], moment: &Moment) -> Value {
    let stall = last_stall(log);
    if let Some(p) = pending.filter(|_| about_last) {
        return json!({
            "kind": p.kind, "stage": p.stage, "progress": p.progress, "thread": p.thread,
            "message": p.message, "backtrace": p.backtrace, "markers": p.markers, "last_stall": stall,
        });
    }
    if about_last {
        return json!({ "kind": ended_as(log), "last_stall": stall });
    }
    json!({ "kind": "manual", "stage": moment.stage, "progress": moment.progress, "markers": moment.markers, "last_stall": stall })
}

// ── what the redactor knows ──────────────────────────────────────────────

/// This machine's own names.
fn add_machine_names(r: &mut Redactor) {
    use sysinfo::{System, SystemExt};
    for k in ["USERPROFILE", "HOME"] {
        if let Some(v) = std::env::var_os(k) {
            r.add(&v.to_string_lossy(), "<home>");
        }
    }
    for k in ["USERNAME", "USER", "LOGNAME"] {
        if let Some(v) = std::env::var_os(k) {
            r.add(&v.to_string_lossy(), "<user>");
        }
    }
    for k in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(v) = std::env::var_os(k) {
            r.add(&v.to_string_lossy(), "<host>");
        }
    }
    if let Some(h) = System::new().host_name() {
        r.add(&h, "<host>");
    }
}

/// Numbers in the tokens stay put through the report: `track#3` in the log is
/// the third entry of `queue`.
pub fn build_redactor(queue: &[TrackInfo], devices: &[DeviceInfo], met: &[(Kind, String)]) -> Redactor {
    let mut r = Redactor::new();
    add_machine_names(&mut r);
    for (i, t) in queue.iter().enumerate() {
        let n = i + 1;
        r.add_path(&t.path, &format!("file#{n}"));
        r.add(&t.title, &format!("track#{n}"));
    }
    for (i, d) in devices.iter().enumerate() {
        r.add(&d.name, &format!("<device#{}>", i + 1));
        r.add(&d.id, &format!("<device#{}>", i + 1));
    }
    let (mut files, mut titles, mut tags, mut urls, mut device_count) = (queue.len(), queue.len(), 0, 0, devices.len());
    for t in queue {
        for tag in [&t.artist, &t.album] {
            tags += 1;
            r.add(tag, &format!("tag#{tags}"));
        }
    }
    for (kind, value) in met {
        match kind {
            Kind::Path => {
                files += 1;
                r.add_path(value, &format!("file#{files}"));
            }
            Kind::Title => {
                titles += 1;
                r.add(value, &format!("track#{titles}"));
            }
            Kind::Tag => {
                tags += 1;
                r.add(value, &format!("tag#{tags}"));
            }
            Kind::Url => {
                urls += 1;
                r.add(value, &format!("<url#{urls}>"));
            }
            Kind::Device => {
                device_count += 1;
                r.add(value, &format!("<device#{device_count}>"));
            }
        }
    }
    r.finish();
    r
}

// ── the log ──────────────────────────────────────────────────────────────

/// The last lines of a session log, as they are on the disk.
pub fn read_log_tail(path: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = f.take(LOG_TAIL_BYTES).read_to_end(&mut bytes);
    // A power cut can leave a file's end as zeros.
    tail_lines(String::from_utf8_lossy(&bytes).trim_end_matches('\0'), LOG_LINES)
}

fn tail_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}

/// Every string in a value, through the redactor.
fn redact_value(v: &mut Value, r: &Redactor) {
    match v {
        Value::String(s) => *s = r.text(s),
        Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, r)),
        Value::Object(o) => o.values_mut().for_each(|x| redact_value(x, r)),
        _ => {}
    }
}

// ── the whole ────────────────────────────────────────────────────────────

pub struct Input<'a> {
    pub parts: &'a Parts,
    pub about_last: bool,
    pub pending: Option<&'a Pending>,
    pub log: &'a [String],
    pub note: &'a str,
    pub redactor: &'a Redactor,
    pub moment: &'a Moment,
}

/// The sections that do not depend on this machine's hardware calls, kept
/// apart so a test can look through all of it.
fn assemble(app: Value, machine: Value, i: &Input) -> Value {
    let status = i.parts.status.as_ref();
    let mut sections = json!({
        "schema_version": SCHEMA_VERSION,
        "app": app,
        "machine": machine,
        "gpu": gpu_section(i.parts.gpu.as_ref()),
        "audio": audio_section(status),
        "settings": settings_section(i.parts.settings.as_ref()),
        "queue": queue_section(&i.parts.queue, status, i.parts.hp_prepare.as_ref()),
        "event": event_section(i.about_last, i.pending, i.log, i.moment),
    });
    redact_value(&mut sections, i.redactor);
    sections["log"] = Value::Array(i.log.iter().map(|l| json!(i.redactor.line(l))).collect());
    sections["note"] = json!(i.redactor.note(i.note));
    sections
}

pub fn build(i: &Input) -> Value {
    assemble(app_section(&i.moment.env), machine_section(), i)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Private things planted everywhere a real session has them.
    fn planted() -> (Parts, Vec<String>, Vec<&'static str>) {
        let queue = vec![
            track(5, r"D:\Музыка\Eagles\Hell Freezes Over\06 - Hotel California.flac", "Hotel California (Live)", "Eagles", "Hell Freezes Over"),
            track(6, r"\\nas\Share\夜に駆ける.dsf", "夜に駆ける", "YOASOBI", "THE BOOK"),
        ];
        let status = json!({
            "state": "playing", "trackId": 5, "outRate": 352800, "exclusive": true, "bufferS": 1.5,
            "device": "Chord Mojo 2", "trackFile": "06 - Hotel California.flac",
            "pending": {"what": "Preparing Hotel California (Live)"}, "error": "Cannot open D:\\Музыка\\x.flac",
            "radio": {"url": "http://ice1.somafm.com/groovesalad"}, "notes": ["Hotel California (Live)"],
        });
        let mut settings = PlayerSettings::default();
        settings.eq = Some(serde_json::from_value(json!({"v": 2, "on": true, "name": "Sennheiser HD 800 of Ivan", "bands": []})).unwrap());
        let parts = Parts {
            status: Some(status),
            queue,
            settings: Some(settings),
            hp_prepare: Some(json!({"track": 5, "state": "parts", "path": "D:\\Музыка\\x.flac"})),
            devices: vec![DeviceInfo { id: "{0.0.0}".into(), name: "Chord Mojo 2".into(), is_default: true }],
            gpu: Some(json!({"found": true, "name": "NVIDIA GeForce RTX 4090", "backend": "Vulkan", "secretPath": "C:\\x"})),
        };
        let log = vec![
            r"[10:00:00.000] [CONV] Decoding: 06 - Hotel California.flac".to_string(),
            r"[10:00:00.100] [PLAYER] lamp of Hotel California (Live): 5 MB free past the margin, 9 MB needed".to_string(),
            r"[10:00:00.200] [PLAYER] Opening Chord Mojo 2 exclusive 352800 Hz".to_string(),
            r"[10:00:00.300] [PLAYER] 夜に駆ける: its start gives way".to_string(),
            r"[10:00:00.400] [UI] loaded D:\Музыка\Eagles\Hell Freezes Over\06 - Hotel California.flac".to_string(),
            r"[10:00:00.500] [RADIO] tune http://ice1.somafm.com/groovesalad at Eagles".to_string(),
            r"[10:00:00.600] [HOME] C:\Users\ivan\AppData\Local\AuraEngine\logs written".to_string(),
        ];
        let secrets = vec![
            "Музыка", "Hotel California", "Hell Freezes", "Eagles", "夜に駆ける", "YOASOBI", "THE BOOK", "nas\\", "Share",
            "Chord", "Mojo", "somafm", "groovesalad", "Sennheiser", "Ivan", "ivan", "secretPath",
        ];
        (parts, log, secrets)
    }

    fn quiet() -> Moment {
        Moment { stage: String::new(), progress: 0, markers: String::new(), env: Vec::new() }
    }

    fn built(parts: &Parts, log: &[String], note: &str) -> String {
        let met = vec![(Kind::Url, "http://ice1.somafm.com/groovesalad".to_string())];
        let r = build_redactor(&parts.queue, &parts.devices, &met);
        let i = Input { parts, about_last: false, pending: None, log, note, redactor: &r, moment: &quiet() };
        serde_json::to_string_pretty(&assemble(json!({}), json!({}), &i)).unwrap()
    }

    #[test]
    fn nothing_private_is_anywhere_in_the_report() {
        let (parts, log, secrets) = planted();
        let text = built(&parts, &log, "It stops at Hotel California (Live), see D:\\Музыка\\x.flac");
        for s in secrets {
            assert!(!text.contains(s), "{s} leaked:\n{text}");
        }
    }

    #[test]
    fn the_report_still_says_what_a_developer_needs() {
        let (parts, log, _) = planted();
        let v: Value = serde_json::from_str(&built(&parts, &log, "note")).unwrap();
        assert_eq!(v["schema_version"], json!(SCHEMA_VERSION));
        assert_eq!(v["audio"]["outRate"], json!(352800));
        assert_eq!(v["gpu"]["name"], json!("NVIDIA GeForce RTX 4090"));
        assert_eq!(v["settings"]["eq"], json!({"on": true, "bands": 0}));
        assert_eq!(v["queue"][0], json!({
            "id": "track#1", "codec": "flac", "rate": 44100, "bits": 16, "ch": 2, "duration_s": 391,
            "size_mb": null, "kind": "PCM", "state": "on air", "hp_prep": "parts",
        }));
        assert_eq!(v["queue"][1]["id"], json!("track#2"));
        assert_eq!(v["queue"][1]["codec"], json!("dsf"));
        assert!(v["log"][1].as_str().unwrap().contains("lamp of track#1"), "the log's numbers are the queue's: {}", v["log"][1]);
        assert_eq!(v["note"], json!("note"));
    }

    #[test]
    fn a_codec_is_named_only_from_the_list() {
        assert_eq!(codec_of("a.FLAC"), "flac");
        assert_eq!(codec_of("a.my private note"), "other");
        assert_eq!(codec_of("noext"), "other");
    }

    #[test]
    fn the_log_tail_is_its_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), vec!["b".to_string(), "c".to_string()]);
        assert_eq!(tail_lines("a\n", 5), vec!["a".to_string()]);
    }

    #[test]
    fn an_unclean_end_with_a_stall_in_its_log_is_a_stall() {
        let log = vec!["[10:00:00.000] [WATCHDOG] stall control: no change for 31 s".to_string()];
        assert_eq!(event_section(true, None, &log, &quiet())["kind"], json!("stall"));
        assert_eq!(event_section(true, None, &[], &quiet())["kind"], json!("unclean_exit"));
        assert_eq!(event_section(false, None, &log, &quiet())["kind"], json!("manual"));
    }

    #[test]
    fn a_manual_report_tells_of_the_moment_it_was_given() {
        let moment = Moment { stage: "prep: parts".into(), progress: 7, markers: "prep=parts".into(), env: Vec::new() };
        let e = event_section(false, None, &[], &moment);
        assert_eq!((&e["stage"], &e["progress"], &e["markers"]), (&json!("prep: parts"), &json!(7), &json!("prep=parts")));
    }

    #[test]
    fn the_moment_now_reads_the_markers() {
        // Each marker is in the line even when idle; which stage, a test running alongside decides.
        assert!(!Moment::now().markers.is_empty());
    }

    #[test]
    fn a_fault_written_down_gives_the_event_its_detail() {
        let p = Pending { kind: "panic".into(), thread: "aura-control".into(), message: "boom".into(), ..Default::default() };
        let e = event_section(true, Some(&p), &[], &quiet());
        assert_eq!(e["kind"], json!("panic"));
        assert_eq!(e["thread"], json!("aura-control"));
    }

    #[test]
    fn only_names_of_variables_go() {
        std::env::set_var("AURA_TEST_REPORT_NAME", "C:\\private");
        let names = env_names();
        assert!(names.iter().any(|n| n == "AURA_TEST_REPORT_NAME"));
        assert!(!names.iter().any(|n| n.contains("private")));
        std::env::remove_var("AURA_TEST_REPORT_NAME");
    }
}
