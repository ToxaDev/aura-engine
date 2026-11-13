//! The UPnP AV face: AVTransport, RenderingControl and ConnectionManager.

use super::desc;
use super::renderer::{format_time, parse_time, Renderer, Repeat, Transport};
use super::xml::{esc, Call};

/// A SOAP fault: the UPnP error code and its words.
pub type Fault = (u32, &'static str);
pub type Reply = Result<Vec<(&'static str, String)>, Fault>;

pub const INVALID_ACTION: Fault = (401, "Invalid Action");
pub const INVALID_ARGS: Fault = (402, "Invalid Args");
const NOT_IMPLEMENTED: &str = "NOT_IMPLEMENTED";

fn instance(c: &Call) -> Result<(), Fault> {
    match c.arg("InstanceID").trim() {
        "0" | "" => Ok(()),
        _ => Err((718, "Invalid InstanceID")),
    }
}

fn actions_now(t: Transport, has_next: bool) -> String {
    let mut a = match t {
        Transport::NoMedia => vec![],
        Transport::Stopped => vec!["Play"],
        Transport::Playing => vec!["Pause", "Stop", "Seek"],
        Transport::Paused => vec!["Play", "Stop", "Seek"],
        Transport::Transitioning => vec!["Stop"],
    };
    if t != Transport::NoMedia {
        a.push("Previous");
        if has_next {
            a.push("Next");
        }
    }
    a.join(",")
}

fn play_mode(r: &Renderer) -> &'static str {
    match r.st.lock().unwrap().repeat {
        Repeat::Off => "NORMAL",
        Repeat::One => "REPEAT_ONE",
        Repeat::All => "REPEAT_ALL",
    }
}

pub fn av_transport(r: &Renderer, c: &Call) -> Reply {
    instance(c)?;
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    match c.action.as_str() {
        "SetAVTransportURI" => {
            let uri = c.arg("CurrentURI").trim().to_string();
            if uri.is_empty() {
                r.delete_all();
                return ok(vec![]);
            }
            if !super::media::fetchable(&uri) {
                return Err((714, "Illegal MIME-type"));
            }
            aura_core::aelog!("[UPNP] SetAVTransportURI");
            r.set_uri(uri, c.arg("CurrentURIMetaData").to_string());
            r.tick();
            ok(vec![])
        }
        "SetNextAVTransportURI" => {
            let uri = c.arg("NextURI").trim().to_string();
            if !uri.is_empty() && !super::media::fetchable(&uri) {
                return Err((714, "Illegal MIME-type"));
            }
            r.set_next_uri(uri, c.arg("NextURIMetaData").to_string());
            r.tick();
            ok(vec![])
        }
        "GetMediaInfo" => {
            let now = r.now();
            let next = r.next_entry();
            ok(vec![
                ("NrTracks", if now.is_some() { "1" } else { "0" }.into()),
                ("MediaDuration", now.as_ref().map_or(String::new(), |n| format_time(n.2))),
                ("CurrentURI", now.as_ref().map_or(String::new(), |n| n.0.uri.clone())),
                ("CurrentURIMetaData", now.as_ref().map_or(String::new(), |n| n.0.didl.clone())),
                ("NextURI", next.as_ref().map_or(String::new(), |n| n.uri.clone())),
                ("NextURIMetaData", next.as_ref().map_or(String::new(), |n| n.didl.clone())),
                ("PlayMedium", if now.is_some() { "NETWORK" } else { "NONE" }.into()),
                ("RecordMedium", NOT_IMPLEMENTED.into()),
                ("WriteStatus", NOT_IMPLEMENTED.into()),
            ])
        }
        "GetTransportInfo" => {
            let failed = r.st.lock().unwrap().error.is_some();
            ok(vec![
                ("CurrentTransportState", r.transport().av().into()),
                ("CurrentTransportStatus", if failed { "ERROR_OCCURRED" } else { "OK" }.into()),
                ("CurrentSpeed", "1".into()),
            ])
        }
        "GetPositionInfo" => {
            let now = r.now();
            let (track, dur, meta, uri, pos) = match &now {
                Some((e, pos, dur)) => ("1", format_time(*dur), e.didl.clone(), e.uri.clone(), format_time(*pos)),
                None => ("0", "0:00:00".into(), String::new(), String::new(), "0:00:00".into()),
            };
            ok(vec![
                ("Track", track.into()),
                ("TrackDuration", dur),
                ("TrackMetaData", meta),
                ("TrackURI", uri),
                ("RelTime", pos.clone()),
                ("AbsTime", pos),
                ("RelCount", "2147483647".into()),
                ("AbsCount", "2147483647".into()),
            ])
        }
        "GetDeviceCapabilities" => ok(vec![
            ("PlayMedia", "NETWORK".into()),
            ("RecMedia", NOT_IMPLEMENTED.into()),
            ("RecQualityModes", NOT_IMPLEMENTED.into()),
        ]),
        "GetTransportSettings" => ok(vec![("PlayMode", play_mode(r).into()), ("RecQualityMode", NOT_IMPLEMENTED.into())]),
        "GetCurrentTransportActions" => ok(vec![("Actions", actions_now(r.transport(), r.next_entry().is_some()))]),
        "Play" => {
            if r.now().is_none() {
                return Err((701, "Transition not available"));
            }
            r.play();
            ok(vec![])
        }
        "Pause" => {
            r.pause();
            ok(vec![])
        }
        "Stop" => {
            r.stop();
            ok(vec![])
        }
        "Seek" => {
            let unit = c.arg("Unit");
            match unit {
                "REL_TIME" | "ABS_TIME" => {
                    let at = parse_time(c.arg("Target")).ok_or((711, "Illegal seek target"))?;
                    r.seek(at);
                    ok(vec![])
                }
                "TRACK_NR" if c.arg("Target").trim() == "1" => {
                    r.seek(0.0);
                    ok(vec![])
                }
                _ => Err((710, "Seek mode not supported")),
            }
        }
        "Next" => match r.neighbour(1) {
            Some(id) => {
                r.jump(id);
                ok(vec![])
            }
            None => Err((711, "Illegal seek target")),
        },
        "Previous" => {
            r.seek(0.0);
            ok(vec![])
        }
        "SetPlayMode" => {
            let m = match c.arg("NewPlayMode") {
                "NORMAL" => Repeat::Off,
                "REPEAT_ONE" => Repeat::One,
                "REPEAT_ALL" => Repeat::All,
                _ => return Err((712, "Play mode not supported")),
            };
            r.st.lock().unwrap().repeat = m;
            r.tick();
            ok(vec![])
        }
        _ => Err(INVALID_ACTION),
    }
}

pub fn rendering_control(r: &Renderer, c: &Call) -> Reply {
    instance(c)?;
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    let channel = || match c.arg("Channel") {
        "Master" | "" => Ok(()),
        _ => Err(INVALID_ARGS),
    };
    match c.action.as_str() {
        "ListPresets" => ok(vec![("CurrentPresetNameList", "FactoryDefaults".into())]),
        "SelectPreset" => ok(vec![]),
        "GetMute" => {
            channel()?;
            ok(vec![("CurrentMute", bool_word(r.muted()).into())])
        }
        "SetMute" => {
            channel()?;
            r.set_mute(parse_bool(c.arg("DesiredMute")).ok_or(INVALID_ARGS)?);
            ok(vec![])
        }
        "GetVolume" => {
            channel()?;
            ok(vec![("CurrentVolume", r.volume().to_string())])
        }
        "SetVolume" => {
            channel()?;
            let v: u32 = c.arg("DesiredVolume").trim().parse().map_err(|_| INVALID_ARGS)?;
            r.set_volume(v.min(100));
            ok(vec![])
        }
        "GetVolumeDB" => {
            channel()?;
            ok(vec![("CurrentVolume", db_256(r.volume_db()).to_string())])
        }
        "SetVolumeDB" => {
            channel()?;
            let v: i32 = c.arg("DesiredVolume").trim().parse().map_err(|_| INVALID_ARGS)?;
            r.set_volume_db(v as f64 / 256.0);
            ok(vec![])
        }
        "GetVolumeDBRange" => {
            channel()?;
            ok(vec![("MinValue", "-30720".into()), ("MaxValue", "0".into())])
        }
        _ => Err(INVALID_ACTION),
    }
}

pub fn connection_manager(_r: &Renderer, c: &Call) -> Reply {
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    match c.action.as_str() {
        "GetProtocolInfo" => ok(vec![("Source", String::new()), ("Sink", desc::sink_protocol_info())]),
        "GetCurrentConnectionIDs" => ok(vec![("ConnectionIDs", "0".into())]),
        "GetCurrentConnectionInfo" => {
            if c.arg("ConnectionID").trim() != "0" {
                return Err((706, "Invalid connection reference"));
            }
            ok(vec![
                ("RcsID", "0".into()),
                ("AVTransportID", "0".into()),
                ("ProtocolInfo", String::new()),
                ("PeerConnectionManager", String::new()),
                ("PeerConnectionID", "-1".into()),
                ("Direction", "Input".into()),
                ("Status", "OK".into()),
            ])
        }
        _ => Err(INVALID_ACTION),
    }
}

/// The volume in 1/256 dB, as VolumeDB counts it.
fn db_256(db: f64) -> i32 {
    (db.max(-120.0) * 256.0).round().max(-30720.0) as i32
}

pub fn bool_word(b: bool) -> &'static str {
    if b {
        "1"
    } else {
        "0"
    }
}

pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

// ── events ─────────────────────────────────────────────────────────────────

/// AVTransport's evented variables, as its LastChange carries them.
pub fn av_transport_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    let t = r.transport();
    let now = r.now();
    let next = r.next_entry();
    let failed = r.st.lock().unwrap().error.is_some();
    vec![
        ("TransportState", t.av().into()),
        ("TransportStatus", if failed { "ERROR_OCCURRED" } else { "OK" }.into()),
        ("TransportPlaySpeed", "1".into()),
        ("CurrentPlayMode", play_mode(r).into()),
        ("NumberOfTracks", if now.is_some() { "1" } else { "0" }.into()),
        ("CurrentTrack", if now.is_some() { "1" } else { "0" }.into()),
        ("CurrentTrackDuration", now.as_ref().map_or("0:00:00".into(), |n| format_time(n.2))),
        ("CurrentMediaDuration", now.as_ref().map_or("0:00:00".into(), |n| format_time(n.2))),
        ("CurrentTrackURI", now.as_ref().map_or(String::new(), |n| n.0.uri.clone())),
        ("AVTransportURI", now.as_ref().map_or(String::new(), |n| n.0.uri.clone())),
        ("CurrentTrackMetaData", now.as_ref().map_or(String::new(), |n| n.0.didl.clone())),
        ("AVTransportURIMetaData", now.as_ref().map_or(String::new(), |n| n.0.didl.clone())),
        ("NextAVTransportURI", next.as_ref().map_or(String::new(), |n| n.uri.clone())),
        ("NextAVTransportURIMetaData", next.as_ref().map_or(String::new(), |n| n.didl.clone())),
        ("PlaybackStorageMedium", if now.is_some() { "NETWORK" } else { "NONE" }.into()),
        ("CurrentTransportActions", actions_now(t, next.is_some())),
    ]
}

pub fn rendering_control_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    vec![
        ("Volume", r.volume().to_string()),
        ("VolumeDB", db_256(r.volume_db()).to_string()),
        ("Mute", bool_word(r.muted()).into()),
        ("PresetNameList", "FactoryDefaults".into()),
    ]
}

pub fn connection_manager_vars() -> Vec<(&'static str, String)> {
    vec![
        ("SourceProtocolInfo", String::new()),
        ("SinkProtocolInfo", desc::sink_protocol_info()),
        ("CurrentConnectionIDs", "0".into()),
    ]
}

/// The LastChange document: the AV services' way of sending variables.
/// `ns`: the service's event namespace (AVT or RCS).
pub fn last_change(ns: &str, vars: &[(&'static str, String)], with_channel: bool) -> String {
    let mut x = format!("<Event xmlns=\"{ns}\"><InstanceID val=\"0\">");
    for (name, value) in vars {
        if with_channel && matches!(*name, "Volume" | "VolumeDB" | "Mute") {
            x.push_str(&format!("<{name} channel=\"Master\" val=\"{}\"/>", esc(value)));
        } else {
            x.push_str(&format!("<{name} val=\"{}\"/>", esc(value)));
        }
    }
    x.push_str("</InstanceID></Event>");
    x
}

pub const AVT_EVENT_NS: &str = "urn:schemas-upnp-org:metadata-1-0/AVT/";
pub const RCS_EVENT_NS: &str = "urn:schemas-upnp-org:metadata-1-0/RCS/";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upnp::xml;

    #[test]
    fn last_change_carries_its_variables_escaped() {
        let x = last_change(AVT_EVENT_NS, &[("TransportState", "PLAYING".into()), ("CurrentTrackMetaData", "<DIDL-Lite/>".into())], false);
        assert!(x.contains("<TransportState val=\"PLAYING\"/>"));
        assert!(x.contains("val=\"&lt;DIDL-Lite/&gt;\""));
        let ev = xml::find(&x, "CurrentTrackMetaData").unwrap();
        assert_eq!(ev.attr("val").as_deref(), Some("<DIDL-Lite/>"));
        let y = last_change(RCS_EVENT_NS, &[("Volume", "40".into())], true);
        assert!(y.contains("<Volume channel=\"Master\" val=\"40\"/>"));
    }

    #[test]
    fn booleans_are_read_in_every_spelling() {
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("False"), Some(false));
        assert_eq!(parse_bool("x"), None);
    }
}
