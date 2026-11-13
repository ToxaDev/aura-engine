//! What the renderer says about itself: the device description and each
//! service's SCPD, made from the tables below. Control points read them once,
//! then call the actions by name.

use super::xml::esc;

/// A state variable of a service.
pub struct Var {
    pub name: &'static str,
    pub ty: &'static str,
    /// Sent in events (as itself, or inside LastChange for the AV services).
    pub evented: bool,
    pub allowed: &'static [&'static str],
    pub range: Option<(i64, i64, i64)>,
}

/// An argument: its name, whether it goes out, the variable it is of.
pub type Arg = (&'static str, bool, &'static str);

pub struct Action {
    pub name: &'static str,
    pub args: &'static [Arg],
}

pub struct Service {
    /// Its part of the paths: `/upnp/<short>/…`.
    pub short: &'static str,
    pub urn: &'static str,
    pub id: &'static str,
    pub vars: &'static [Var],
    pub actions: &'static [Action],
}

const fn v(name: &'static str, ty: &'static str) -> Var {
    Var { name, ty, evented: false, allowed: &[], range: None }
}
const fn ev(name: &'static str, ty: &'static str) -> Var {
    Var { name, ty, evented: true, allowed: &[], range: None }
}
const fn one_of(name: &'static str, allowed: &'static [&'static str]) -> Var {
    Var { name, ty: "string", evented: false, allowed, range: None }
}
const fn range(name: &'static str, ty: &'static str, lo: i64, hi: i64, step: i64) -> Var {
    Var { name, ty, evented: false, allowed: &[], range: Some((lo, hi, step)) }
}

const I: Arg = ("InstanceID", false, "A_ARG_TYPE_InstanceID");

pub static AV_TRANSPORT: Service = Service {
    short: "avt",
    urn: "urn:schemas-upnp-org:service:AVTransport:1",
    id: "urn:upnp-org:serviceId:AVTransport",
    vars: &[
        one_of("TransportState", &["STOPPED", "PLAYING", "PAUSED_PLAYBACK", "TRANSITIONING", "NO_MEDIA_PRESENT"]),
        one_of("TransportStatus", &["OK", "ERROR_OCCURRED"]),
        one_of("PlaybackStorageMedium", &["NONE", "NETWORK"]),
        one_of("RecordStorageMedium", &["NOT_IMPLEMENTED"]),
        v("PossiblePlaybackStorageMedia", "string"),
        v("PossibleRecordStorageMedia", "string"),
        one_of("CurrentPlayMode", &["NORMAL", "REPEAT_ONE", "REPEAT_ALL"]),
        one_of("TransportPlaySpeed", &["1"]),
        one_of("RecordMediumWriteStatus", &["NOT_IMPLEMENTED"]),
        one_of("CurrentRecordQualityMode", &["NOT_IMPLEMENTED"]),
        v("PossibleRecordQualityModes", "string"),
        v("NumberOfTracks", "ui4"),
        v("CurrentTrack", "ui4"),
        v("CurrentTrackDuration", "string"),
        v("CurrentMediaDuration", "string"),
        v("CurrentTrackMetaData", "string"),
        v("CurrentTrackURI", "string"),
        v("AVTransportURI", "string"),
        v("AVTransportURIMetaData", "string"),
        v("NextAVTransportURI", "string"),
        v("NextAVTransportURIMetaData", "string"),
        v("RelativeTimePosition", "string"),
        v("AbsoluteTimePosition", "string"),
        v("RelativeCounterPosition", "i4"),
        v("AbsoluteCounterPosition", "i4"),
        v("CurrentTransportActions", "string"),
        ev("LastChange", "string"),
        one_of("A_ARG_TYPE_SeekMode", &["REL_TIME", "ABS_TIME", "TRACK_NR"]),
        v("A_ARG_TYPE_SeekTarget", "string"),
        v("A_ARG_TYPE_InstanceID", "ui4"),
    ],
    actions: &[
        Action { name: "SetAVTransportURI", args: &[I, ("CurrentURI", false, "AVTransportURI"), ("CurrentURIMetaData", false, "AVTransportURIMetaData")] },
        Action { name: "SetNextAVTransportURI", args: &[I, ("NextURI", false, "NextAVTransportURI"), ("NextURIMetaData", false, "NextAVTransportURIMetaData")] },
        Action { name: "GetMediaInfo", args: &[I,
            ("NrTracks", true, "NumberOfTracks"), ("MediaDuration", true, "CurrentMediaDuration"),
            ("CurrentURI", true, "AVTransportURI"), ("CurrentURIMetaData", true, "AVTransportURIMetaData"),
            ("NextURI", true, "NextAVTransportURI"), ("NextURIMetaData", true, "NextAVTransportURIMetaData"),
            ("PlayMedium", true, "PlaybackStorageMedium"), ("RecordMedium", true, "RecordStorageMedium"),
            ("WriteStatus", true, "RecordMediumWriteStatus")] },
        Action { name: "GetTransportInfo", args: &[I,
            ("CurrentTransportState", true, "TransportState"), ("CurrentTransportStatus", true, "TransportStatus"),
            ("CurrentSpeed", true, "TransportPlaySpeed")] },
        Action { name: "GetPositionInfo", args: &[I,
            ("Track", true, "CurrentTrack"), ("TrackDuration", true, "CurrentTrackDuration"),
            ("TrackMetaData", true, "CurrentTrackMetaData"), ("TrackURI", true, "CurrentTrackURI"),
            ("RelTime", true, "RelativeTimePosition"), ("AbsTime", true, "AbsoluteTimePosition"),
            ("RelCount", true, "RelativeCounterPosition"), ("AbsCount", true, "AbsoluteCounterPosition")] },
        Action { name: "GetDeviceCapabilities", args: &[I,
            ("PlayMedia", true, "PossiblePlaybackStorageMedia"), ("RecMedia", true, "PossibleRecordStorageMedia"),
            ("RecQualityModes", true, "PossibleRecordQualityModes")] },
        Action { name: "GetTransportSettings", args: &[I,
            ("PlayMode", true, "CurrentPlayMode"), ("RecQualityMode", true, "CurrentRecordQualityMode")] },
        Action { name: "Stop", args: &[I] },
        Action { name: "Play", args: &[I, ("Speed", false, "TransportPlaySpeed")] },
        Action { name: "Pause", args: &[I] },
        Action { name: "Seek", args: &[I, ("Unit", false, "A_ARG_TYPE_SeekMode"), ("Target", false, "A_ARG_TYPE_SeekTarget")] },
        Action { name: "Next", args: &[I] },
        Action { name: "Previous", args: &[I] },
        Action { name: "SetPlayMode", args: &[I, ("NewPlayMode", false, "CurrentPlayMode")] },
        Action { name: "GetCurrentTransportActions", args: &[I, ("Actions", true, "CurrentTransportActions")] },
    ],
};

const CH: Arg = ("Channel", false, "A_ARG_TYPE_Channel");

pub static RENDERING_CONTROL: Service = Service {
    short: "rc",
    urn: "urn:schemas-upnp-org:service:RenderingControl:1",
    id: "urn:upnp-org:serviceId:RenderingControl",
    vars: &[
        v("PresetNameList", "string"),
        ev("LastChange", "string"),
        v("Mute", "boolean"),
        range("Volume", "ui2", 0, 100, 1),
        range("VolumeDB", "i2", -30720, 0, 1),
        one_of("A_ARG_TYPE_Channel", &["Master"]),
        v("A_ARG_TYPE_InstanceID", "ui4"),
        one_of("A_ARG_TYPE_PresetName", &["FactoryDefaults"]),
    ],
    actions: &[
        Action { name: "ListPresets", args: &[I, ("CurrentPresetNameList", true, "PresetNameList")] },
        Action { name: "SelectPreset", args: &[I, ("PresetName", false, "A_ARG_TYPE_PresetName")] },
        Action { name: "GetMute", args: &[I, CH, ("CurrentMute", true, "Mute")] },
        Action { name: "SetMute", args: &[I, CH, ("DesiredMute", false, "Mute")] },
        Action { name: "GetVolume", args: &[I, CH, ("CurrentVolume", true, "Volume")] },
        Action { name: "SetVolume", args: &[I, CH, ("DesiredVolume", false, "Volume")] },
        Action { name: "GetVolumeDB", args: &[I, CH, ("CurrentVolume", true, "VolumeDB")] },
        Action { name: "SetVolumeDB", args: &[I, CH, ("DesiredVolume", false, "VolumeDB")] },
        Action { name: "GetVolumeDBRange", args: &[I, CH, ("MinValue", true, "VolumeDB"), ("MaxValue", true, "VolumeDB")] },
    ],
};

pub static CONNECTION_MANAGER: Service = Service {
    short: "cm",
    urn: "urn:schemas-upnp-org:service:ConnectionManager:1",
    id: "urn:upnp-org:serviceId:ConnectionManager",
    vars: &[
        ev("SourceProtocolInfo", "string"),
        ev("SinkProtocolInfo", "string"),
        ev("CurrentConnectionIDs", "string"),
        one_of("A_ARG_TYPE_ConnectionStatus", &["OK", "ContentFormatMismatch", "InsufficientBandwidth", "UnreliableChannel", "Unknown"]),
        v("A_ARG_TYPE_ConnectionManager", "string"),
        one_of("A_ARG_TYPE_Direction", &["Input", "Output"]),
        v("A_ARG_TYPE_ProtocolInfo", "string"),
        v("A_ARG_TYPE_ConnectionID", "i4"),
        v("A_ARG_TYPE_AVTransportID", "i4"),
        v("A_ARG_TYPE_RcsID", "i4"),
    ],
    actions: &[
        Action { name: "GetProtocolInfo", args: &[("Source", true, "SourceProtocolInfo"), ("Sink", true, "SinkProtocolInfo")] },
        Action { name: "GetCurrentConnectionIDs", args: &[("ConnectionIDs", true, "CurrentConnectionIDs")] },
        Action { name: "GetCurrentConnectionInfo", args: &[
            ("ConnectionID", false, "A_ARG_TYPE_ConnectionID"), ("RcsID", true, "A_ARG_TYPE_RcsID"),
            ("AVTransportID", true, "A_ARG_TYPE_AVTransportID"), ("ProtocolInfo", true, "A_ARG_TYPE_ProtocolInfo"),
            ("PeerConnectionManager", true, "A_ARG_TYPE_ConnectionManager"), ("PeerConnectionID", true, "A_ARG_TYPE_ConnectionID"),
            ("Direction", true, "A_ARG_TYPE_Direction"), ("Status", true, "A_ARG_TYPE_ConnectionStatus")] },
    ],
};

/// What the engine reads, as a sink: the containers the player opens.
pub const SINK_MIMES: &[&str] = &[
    "audio/flac", "audio/x-flac", "audio/wav", "audio/x-wav", "audio/wave", "audio/aiff", "audio/x-aiff",
    "audio/mpeg", "audio/mp3", "audio/mp4", "audio/x-m4a", "audio/m4a", "audio/aac", "audio/x-aac",
    "audio/ogg", "audio/x-ogg", "audio/dsf", "audio/x-dsf", "audio/dff", "audio/x-dff", "audio/dsd",
];

pub fn sink_protocol_info() -> String {
    SINK_MIMES.iter().map(|m| format!("http-get:*:{m}:*")).collect::<Vec<_>>().join(",")
}

/// A service's SCPD document.
pub fn scpd(s: &Service) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\n\
         <specVersion><major>1</major><minor>0</minor></specVersion>\n<actionList>\n",
    );
    for a in s.actions {
        x.push_str(&format!("<action><name>{}</name>", a.name));
        if !a.args.is_empty() {
            x.push_str("<argumentList>");
            for (name, out, rel) in a.args {
                x.push_str(&format!(
                    "<argument><name>{name}</name><direction>{}</direction><relatedStateVariable>{rel}</relatedStateVariable></argument>",
                    if *out { "out" } else { "in" }
                ));
            }
            x.push_str("</argumentList>");
        }
        x.push_str("</action>\n");
    }
    x.push_str("</actionList>\n<serviceStateTable>\n");
    for var in s.vars {
        x.push_str(&format!(
            "<stateVariable sendEvents=\"{}\"><name>{}</name><dataType>{}</dataType>",
            if var.evented { "yes" } else { "no" },
            var.name,
            var.ty
        ));
        if !var.allowed.is_empty() {
            x.push_str("<allowedValueList>");
            for a in var.allowed {
                x.push_str(&format!("<allowedValue>{a}</allowedValue>"));
            }
            x.push_str("</allowedValueList>");
        }
        if let Some((lo, hi, step)) = var.range {
            x.push_str(&format!(
                "<allowedValueRange><minimum>{lo}</minimum><maximum>{hi}</maximum><step>{step}</step></allowedValueRange>"
            ));
        }
        x.push_str("</stateVariable>\n");
    }
    x.push_str("</serviceStateTable>\n</scpd>\n");
    x
}

/// Who the renderer is.
pub struct Identity {
    pub uuid: String,
    pub friendly_name: String,
    pub version: &'static str,
    /// The page's address (presentationURL), when known.
    pub presentation: Option<String>,
}

pub const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// The device description, with every service this renderer has.
pub fn device(id: &Identity, services: &[&Service]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\n\
         <specVersion><major>1</major><minor>0</minor></specVersion>\n<device>\n",
    );
    x.push_str(&format!("<deviceType>{DEVICE_TYPE}</deviceType>\n"));
    x.push_str(&format!("<friendlyName>{}</friendlyName>\n", esc(&id.friendly_name)));
    x.push_str("<manufacturer>Aura Engine</manufacturer>\n<manufacturerURL>https://aura-engine.app</manufacturerURL>\n");
    x.push_str("<modelDescription>High-resolution FIR upsampling renderer</modelDescription>\n");
    x.push_str(&format!("<modelName>Aura Streamer</modelName>\n<modelNumber>{}</modelNumber>\n", esc(id.version)));
    x.push_str("<modelURL>https://aura-engine.app</modelURL>\n");
    x.push_str(&format!("<UDN>uuid:{}</UDN>\n", id.uuid));
    x.push_str("<dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\n");
    x.push_str("<iconList><icon><mimetype>image/png</mimetype><width>128</width><height>128</height><depth>24</depth>\
                <url>/upnp/icon.png</url></icon></iconList>\n");
    x.push_str("<serviceList>\n");
    for s in services {
        x.push_str(&format!(
            "<service><serviceType>{}</serviceType><serviceId>{}</serviceId>\
             <SCPDURL>/upnp/{short}/scpd.xml</SCPDURL><controlURL>/upnp/{short}/control</controlURL>\
             <eventSubURL>/upnp/{short}/event</eventSubURL></service>\n",
            s.urn,
            s.id,
            short = s.short
        ));
    }
    x.push_str("</serviceList>\n");
    if let Some(p) = &id.presentation {
        x.push_str(&format!("<presentationURL>{}</presentationURL>\n", esc(p)));
    }
    x.push_str("</device>\n</root>\n");
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every argument names a variable of its service: a control point that
    /// checks the SCPD refuses a service whose tables disagree.
    #[test]
    fn every_argument_is_of_a_variable_of_its_service() {
        for s in super::super::services() {
            for a in s.actions {
                for (name, _, rel) in a.args {
                    assert!(s.vars.iter().any(|v| v.name == *rel), "{} {}.{name}: no {rel}", s.short, a.name);
                }
            }
            let doc = scpd(s);
            assert!(super::super::xml::find(&doc, "actionList").is_some(), "{}", s.short);
        }
    }

    #[test]
    fn the_device_lists_its_services() {
        let id = Identity { uuid: "u".into(), friendly_name: "A & B".into(), version: "2.0.0", presentation: None };
        let d = device(&id, super::super::services());
        assert!(d.contains("<friendlyName>A &amp; B</friendlyName>"));
        assert!(d.contains("/upnp/avt/control"));
        assert!(d.contains("<UDN>uuid:u</UDN>"));
    }
}
