//! The OpenHome face: Product, Playlist, Time, Volume and Info. The playlist
//! lives here, on the renderer: the control point fills it and may go away.

use super::av::{bool_word, parse_bool, Fault, Reply, INVALID_ACTION, INVALID_ARGS};
use super::desc::{Action, Arg, Service, Var};
use super::renderer::{Renderer, Repeat, Transport};
use super::xml::{esc, Call};

const TRACKS_MAX: usize = 1000;
const INVALID_ID: Fault = (800, "Invalid id");

const fn v(name: &'static str, ty: &'static str) -> Var {
    Var { name, ty, evented: false, allowed: &[], range: None }
}
const fn ev(name: &'static str, ty: &'static str) -> Var {
    Var { name, ty, evented: true, allowed: &[], range: None }
}

const fn out(name: &'static str, rel: &'static str) -> Arg {
    (name, true, rel)
}
const fn inn(name: &'static str, rel: &'static str) -> Arg {
    (name, false, rel)
}

pub static PRODUCT: Service = Service {
    short: "ohproduct",
    urn: "urn:av-openhome-org:service:Product:1",
    id: "urn:av-openhome-org:serviceId:Product",
    vars: &[
        ev("ManufacturerName", "string"), ev("ManufacturerInfo", "string"), ev("ManufacturerUrl", "string"),
        ev("ManufacturerImageUri", "string"), ev("ModelName", "string"), ev("ModelInfo", "string"),
        ev("ModelUrl", "string"), ev("ModelImageUri", "string"), ev("ProductRoom", "string"),
        ev("ProductName", "string"), ev("ProductInfo", "string"), ev("ProductUrl", "string"),
        ev("ProductImageUri", "string"), ev("Standby", "boolean"), ev("SourceIndex", "ui4"),
        ev("SourceCount", "ui4"), ev("SourceXml", "string"), ev("Attributes", "string"),
        v("SourceXmlChangeCount", "ui4"), v("SourceSystemName", "string"), v("SourceType", "string"),
        v("SourceName", "string"), v("SourceVisible", "boolean"),
    ],
    actions: &[
        Action { name: "Manufacturer", args: &[out("Name", "ManufacturerName"), out("Info", "ManufacturerInfo"), out("Url", "ManufacturerUrl"), out("ImageUri", "ManufacturerImageUri")] },
        Action { name: "Model", args: &[out("Name", "ModelName"), out("Info", "ModelInfo"), out("Url", "ModelUrl"), out("ImageUri", "ModelImageUri")] },
        Action { name: "Product", args: &[out("Room", "ProductRoom"), out("Name", "ProductName"), out("Info", "ProductInfo"), out("Url", "ProductUrl"), out("ImageUri", "ProductImageUri")] },
        Action { name: "Standby", args: &[out("Value", "Standby")] },
        Action { name: "SetStandby", args: &[inn("Value", "Standby")] },
        Action { name: "SourceCount", args: &[out("Value", "SourceCount")] },
        Action { name: "SourceXml", args: &[out("Value", "SourceXml")] },
        Action { name: "SourceIndex", args: &[out("Value", "SourceIndex")] },
        Action { name: "SetSourceIndex", args: &[inn("Value", "SourceIndex")] },
        Action { name: "SetSourceIndexByName", args: &[inn("Value", "SourceName")] },
        Action { name: "Source", args: &[inn("Index", "SourceIndex"), out("SystemName", "SourceSystemName"), out("Type", "SourceType"), out("Name", "SourceName"), out("Visible", "SourceVisible")] },
        Action { name: "Attributes", args: &[out("Value", "Attributes")] },
        Action { name: "SourceXmlChangeCount", args: &[out("Value", "SourceXmlChangeCount")] },
    ],
};

pub static PLAYLIST: Service = Service {
    short: "ohplaylist",
    urn: "urn:av-openhome-org:service:Playlist:1",
    id: "urn:av-openhome-org:serviceId:Playlist",
    vars: &[
        Var { name: "TransportState", ty: "string", evented: true, allowed: &["Playing", "Paused", "Stopped", "Buffering"], range: None },
        ev("Repeat", "boolean"), ev("Shuffle", "boolean"), ev("Id", "ui4"), ev("IdArray", "bin.base64"),
        ev("TracksMax", "ui4"), ev("ProtocolInfo", "string"),
        v("Index", "ui4"), v("Uri", "string"), v("Metadata", "string"), v("Token", "ui4"),
        v("IdList", "string"), v("TrackList", "string"), v("Changed", "boolean"),
        v("AbsoluteSeconds", "ui4"), v("RelativeSeconds", "i4"),
    ],
    actions: &[
        Action { name: "Play", args: &[] },
        Action { name: "Pause", args: &[] },
        Action { name: "Stop", args: &[] },
        Action { name: "Next", args: &[] },
        Action { name: "Previous", args: &[] },
        Action { name: "SetRepeat", args: &[inn("Value", "Repeat")] },
        Action { name: "Repeat", args: &[out("Value", "Repeat")] },
        Action { name: "SetShuffle", args: &[inn("Value", "Shuffle")] },
        Action { name: "Shuffle", args: &[out("Value", "Shuffle")] },
        Action { name: "SeekSecondAbsolute", args: &[inn("Value", "AbsoluteSeconds")] },
        Action { name: "SeekSecondRelative", args: &[inn("Value", "RelativeSeconds")] },
        Action { name: "SeekId", args: &[inn("Value", "Id")] },
        Action { name: "SeekIndex", args: &[inn("Value", "Index")] },
        Action { name: "TransportState", args: &[out("Value", "TransportState")] },
        Action { name: "Id", args: &[out("Value", "Id")] },
        Action { name: "Read", args: &[inn("Id", "Id"), out("Uri", "Uri"), out("Metadata", "Metadata")] },
        Action { name: "ReadList", args: &[inn("IdList", "IdList"), out("TrackList", "TrackList")] },
        Action { name: "Insert", args: &[inn("AfterId", "Id"), inn("Uri", "Uri"), inn("Metadata", "Metadata"), out("NewId", "Id")] },
        Action { name: "DeleteId", args: &[inn("Value", "Id")] },
        Action { name: "DeleteAll", args: &[] },
        Action { name: "TracksMax", args: &[out("Value", "TracksMax")] },
        Action { name: "IdArray", args: &[out("Token", "Token"), out("Array", "IdArray")] },
        Action { name: "IdArrayChanged", args: &[inn("Token", "Token"), out("Value", "Changed")] },
        Action { name: "ProtocolInfo", args: &[out("Value", "ProtocolInfo")] },
    ],
};

pub static TIME: Service = Service {
    short: "ohtime",
    urn: "urn:av-openhome-org:service:Time:1",
    id: "urn:av-openhome-org:serviceId:Time",
    vars: &[ev("TrackCount", "ui4"), ev("Duration", "ui4"), ev("Seconds", "ui4")],
    actions: &[Action { name: "Time", args: &[out("TrackCount", "TrackCount"), out("Duration", "Duration"), out("Seconds", "Seconds")] }],
};

pub static VOLUME: Service = Service {
    short: "ohvolume",
    urn: "urn:av-openhome-org:service:Volume:1",
    id: "urn:av-openhome-org:serviceId:Volume",
    vars: &[
        ev("Volume", "ui4"), ev("Mute", "boolean"), ev("Balance", "i4"), ev("Fade", "i4"),
        ev("VolumeLimit", "ui4"), ev("VolumeMax", "ui4"), ev("VolumeUnity", "ui4"), ev("VolumeSteps", "ui4"),
        ev("VolumeMilliDbPerStep", "ui4"), ev("BalanceMax", "ui4"), ev("FadeMax", "ui4"),
    ],
    actions: &[
        Action { name: "Characteristics", args: &[
            out("VolumeMax", "VolumeMax"), out("VolumeUnity", "VolumeUnity"), out("VolumeSteps", "VolumeSteps"),
            out("VolumeMilliDbPerStep", "VolumeMilliDbPerStep"), out("BalanceMax", "BalanceMax"), out("FadeMax", "FadeMax")] },
        Action { name: "SetVolume", args: &[inn("Value", "Volume")] },
        Action { name: "VolumeInc", args: &[] },
        Action { name: "VolumeDec", args: &[] },
        Action { name: "Volume", args: &[out("Value", "Volume")] },
        Action { name: "SetBalance", args: &[inn("Value", "Balance")] },
        Action { name: "BalanceInc", args: &[] },
        Action { name: "BalanceDec", args: &[] },
        Action { name: "Balance", args: &[out("Value", "Balance")] },
        Action { name: "SetFade", args: &[inn("Value", "Fade")] },
        Action { name: "FadeInc", args: &[] },
        Action { name: "FadeDec", args: &[] },
        Action { name: "Fade", args: &[out("Value", "Fade")] },
        Action { name: "SetMute", args: &[inn("Value", "Mute")] },
        Action { name: "Mute", args: &[out("Value", "Mute")] },
        Action { name: "VolumeLimit", args: &[out("Value", "VolumeLimit")] },
    ],
};

pub static INFO: Service = Service {
    short: "ohinfo",
    urn: "urn:av-openhome-org:service:Info:1",
    id: "urn:av-openhome-org:serviceId:Info",
    vars: &[
        ev("TrackCount", "ui4"), ev("DetailsCount", "ui4"), ev("MetatextCount", "ui4"),
        ev("Uri", "string"), ev("Metadata", "string"), ev("Duration", "ui4"), ev("BitRate", "ui4"),
        ev("BitDepth", "ui4"), ev("SampleRate", "ui4"), ev("Lossless", "boolean"),
        ev("CodecName", "string"), ev("Metatext", "string"),
    ],
    actions: &[
        Action { name: "Counters", args: &[out("TrackCount", "TrackCount"), out("DetailsCount", "DetailsCount"), out("MetatextCount", "MetatextCount")] },
        Action { name: "Track", args: &[out("Uri", "Uri"), out("Metadata", "Metadata")] },
        Action { name: "Details", args: &[
            out("Duration", "Duration"), out("BitRate", "BitRate"), out("BitDepth", "BitDepth"),
            out("SampleRate", "SampleRate"), out("Lossless", "Lossless"), out("CodecName", "CodecName")] },
        Action { name: "Metatext", args: &[out("Value", "Metatext")] },
    ],
};

// ── Product ────────────────────────────────────────────────────────────────

/// Who the product is: (room, name), from the renderer's identity.
pub struct Names {
    pub room: String,
    pub name: String,
    pub presentation: String,
}

fn source_xml() -> String {
    "<SourceList><Source><Name>Playlist</Name><Type>Playlist</Type><Visible>true</Visible></Source></SourceList>".into()
}

pub fn product(c: &Call, n: &Names) -> Reply {
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    match c.action.as_str() {
        "Manufacturer" => ok(vec![("Name", "Aura Engine".into()), ("Info", String::new()), ("Url", "https://aura-engine.app".into()), ("ImageUri", String::new())]),
        "Model" => ok(vec![("Name", "Aura Streamer".into()), ("Info", env!("CARGO_PKG_VERSION").into()), ("Url", "https://aura-engine.app".into()), ("ImageUri", String::new())]),
        "Product" => ok(vec![("Room", n.room.clone()), ("Name", n.name.clone()), ("Info", String::new()), ("Url", n.presentation.clone()), ("ImageUri", String::new())]),
        "Standby" => ok(vec![("Value", "0".into())]),
        "SetStandby" => ok(vec![]),
        "SourceCount" => ok(vec![("Value", "1".into())]),
        "SourceXml" => ok(vec![("Value", source_xml())]),
        "SourceIndex" => ok(vec![("Value", "0".into())]),
        "SetSourceIndex" => match c.arg("Value").trim() {
            "0" => ok(vec![]),
            _ => Err(INVALID_ARGS),
        },
        "SetSourceIndexByName" => match c.arg("Value").trim() {
            "Playlist" => ok(vec![]),
            _ => Err(INVALID_ARGS),
        },
        "Source" => match c.arg("Index").trim() {
            "0" => ok(vec![("SystemName", "Playlist".into()), ("Type", "Playlist".into()), ("Name", "Playlist".into()), ("Visible", "1".into())]),
            _ => Err(INVALID_ARGS),
        },
        "Attributes" => ok(vec![("Value", "Info Time Volume".into())]),
        "SourceXmlChangeCount" => ok(vec![("Value", "0".into())]),
        _ => Err(INVALID_ACTION),
    }
}

pub fn product_vars(n: &Names) -> Vec<(&'static str, String)> {
    vec![
        ("ManufacturerName", "Aura Engine".into()),
        ("ManufacturerInfo", String::new()),
        ("ManufacturerUrl", "https://aura-engine.app".into()),
        ("ManufacturerImageUri", String::new()),
        ("ModelName", "Aura Streamer".into()),
        ("ModelInfo", env!("CARGO_PKG_VERSION").into()),
        ("ModelUrl", "https://aura-engine.app".into()),
        ("ModelImageUri", String::new()),
        ("ProductRoom", n.room.clone()),
        ("ProductName", n.name.clone()),
        ("ProductInfo", String::new()),
        ("ProductUrl", n.presentation.clone()),
        ("ProductImageUri", String::new()),
        ("Standby", "0".into()),
        ("SourceIndex", "0".into()),
        ("SourceCount", "1".into()),
        ("SourceXml", source_xml()),
        ("Attributes", "Info Time Volume".into()),
    ]
}

// ── Playlist ───────────────────────────────────────────────────────────────

fn ids(r: &Renderer) -> Vec<u32> {
    r.st.lock().unwrap().entries.iter().map(|e| e.id).collect()
}

/// The ids, big-endian, in base64: OpenHome's IdArray.
pub fn id_array(ids: &[u32]) -> String {
    let bytes: Vec<u8> = ids.iter().flat_map(|i| i.to_be_bytes()).collect();
    base64(&bytes)
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn current_id(r: &Renderer) -> u32 {
    r.st.lock().unwrap().current.unwrap_or(0)
}

fn u32_arg(c: &Call, name: &str) -> Result<u32, Fault> {
    c.arg(name).trim().parse().map_err(|_| INVALID_ARGS)
}

pub fn playlist(r: &Renderer, c: &Call) -> Reply {
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    r.use_as_playlist();
    match c.action.as_str() {
        "Play" => {
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
        "Next" | "Previous" => {
            if let Some(id) = r.neighbour(if c.action == "Next" { 1 } else { -1 }) {
                r.jump(id);
            }
            ok(vec![])
        }
        "SetRepeat" => {
            let on = parse_bool(c.arg("Value")).ok_or(INVALID_ARGS)?;
            r.st.lock().unwrap().repeat = if on { Repeat::All } else { Repeat::Off };
            r.tick();
            ok(vec![])
        }
        "Repeat" => ok(vec![("Value", bool_word(r.st.lock().unwrap().repeat != Repeat::Off).into())]),
        "SetShuffle" => {
            // Kept and told; the list plays in its order.
            r.st.lock().unwrap().shuffle = parse_bool(c.arg("Value")).ok_or(INVALID_ARGS)?;
            ok(vec![])
        }
        "Shuffle" => ok(vec![("Value", bool_word(r.st.lock().unwrap().shuffle).into())]),
        "SeekSecondAbsolute" => {
            r.seek(f64::from(u32_arg(c, "Value")?));
            ok(vec![])
        }
        "SeekSecondRelative" => {
            let d: i64 = c.arg("Value").trim().parse().map_err(|_| INVALID_ARGS)?;
            let at = r.now().map_or(0.0, |n| n.1);
            r.seek((at + d as f64).max(0.0));
            ok(vec![])
        }
        "SeekId" => {
            if r.jump(u32_arg(c, "Value")?) {
                ok(vec![])
            } else {
                Err(INVALID_ID)
            }
        }
        "SeekIndex" => {
            let i = u32_arg(c, "Value")? as usize;
            match ids(r).get(i) {
                Some(&id) => {
                    r.jump(id);
                    ok(vec![])
                }
                None => Err((801, "Invalid index")),
            }
        }
        "TransportState" => ok(vec![("Value", r.transport().oh().into())]),
        "Id" => ok(vec![("Value", current_id(r).to_string())]),
        "Read" => {
            let id = u32_arg(c, "Id")?;
            let st = r.st.lock().unwrap();
            let e = st.entries.iter().find(|e| e.id == id).ok_or(INVALID_ID)?;
            ok(vec![("Uri", e.uri.clone()), ("Metadata", e.didl.clone())])
        }
        "ReadList" => {
            let st = r.st.lock().unwrap();
            let mut x = String::from("<TrackList>");
            for id in c.arg("IdList").split_whitespace().filter_map(|s| s.parse::<u32>().ok()) {
                if let Some(e) = st.entries.iter().find(|e| e.id == id) {
                    x.push_str(&format!("<Entry><Id>{}</Id><Uri>{}</Uri><Metadata>{}</Metadata></Entry>", e.id, esc(&e.uri), esc(&e.didl)));
                }
            }
            x.push_str("</TrackList>");
            ok(vec![("TrackList", x)])
        }
        "Insert" => {
            let after = u32_arg(c, "AfterId")?;
            let uri = c.arg("Uri").trim().to_string();
            if !super::media::fetchable(&uri) {
                return Err(INVALID_ARGS);
            }
            if r.st.lock().unwrap().entries.len() >= TRACKS_MAX {
                return Err((801, "Playlist full"));
            }
            let id = r.insert(after, uri, c.arg("Metadata").to_string()).map_err(|_| INVALID_ID)?;
            r.tick();
            ok(vec![("NewId", id.to_string())])
        }
        "DeleteId" => {
            r.delete(u32_arg(c, "Value")?);
            r.tick();
            ok(vec![])
        }
        "DeleteAll" => {
            r.delete_all();
            ok(vec![])
        }
        "TracksMax" => ok(vec![("Value", TRACKS_MAX.to_string())]),
        "IdArray" => {
            let rev = r.st.lock().unwrap().list_rev;
            ok(vec![("Token", rev.to_string()), ("Array", id_array(&ids(r)))])
        }
        "IdArrayChanged" => {
            let rev = r.st.lock().unwrap().list_rev;
            ok(vec![("Value", bool_word(u32_arg(c, "Token")? != rev).into())])
        }
        "ProtocolInfo" => ok(vec![("Value", super::desc::sink_protocol_info())]),
        _ => Err(INVALID_ACTION),
    }
}

pub fn playlist_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    let (repeat, shuffle) = {
        let st = r.st.lock().unwrap();
        (st.repeat != Repeat::Off, st.shuffle)
    };
    vec![
        ("TransportState", r.transport().oh().into()),
        ("Repeat", bool_word(repeat).into()),
        ("Shuffle", bool_word(shuffle).into()),
        ("Id", current_id(r).to_string()),
        ("IdArray", id_array(&ids(r))),
        ("TracksMax", TRACKS_MAX.to_string()),
        ("ProtocolInfo", super::desc::sink_protocol_info()),
    ]
}

// ── Time, Volume, Info ─────────────────────────────────────────────────────

/// The current track's number among those that played: a new one when the
/// current entry changes.
fn track_count(r: &Renderer) -> u32 {
    current_id(r)
}

pub fn time(r: &Renderer, c: &Call) -> Reply {
    match c.action.as_str() {
        "Time" => Ok(time_vars(r)),
        _ => Err(INVALID_ACTION),
    }
}

pub fn time_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    let (pos, dur) = r.now().map_or((0.0, 0.0), |n| (n.1, n.2));
    vec![
        ("TrackCount", track_count(r).to_string()),
        ("Duration", (dur.max(0.0) as u32).to_string()),
        ("Seconds", (pos.max(0.0) as u32).to_string()),
    ]
}

pub fn volume(r: &Renderer, c: &Call) -> Reply {
    let ok = |v: Vec<(&'static str, String)>| Ok(v);
    match c.action.as_str() {
        "Characteristics" => ok(vec![
            ("VolumeMax", "100".into()), ("VolumeUnity", "100".into()), ("VolumeSteps", "100".into()),
            ("VolumeMilliDbPerStep", "600".into()), ("BalanceMax", "0".into()), ("FadeMax", "0".into()),
        ]),
        "SetVolume" => {
            r.set_volume(u32_arg(c, "Value")?.min(100));
            ok(vec![])
        }
        "VolumeInc" => {
            r.set_volume((r.volume() + 1).min(100));
            ok(vec![])
        }
        "VolumeDec" => {
            r.set_volume(r.volume().saturating_sub(1));
            ok(vec![])
        }
        "Volume" => ok(vec![("Value", r.volume().to_string())]),
        "SetBalance" | "SetFade" => match c.arg("Value").trim() {
            "0" => ok(vec![]),
            _ => Err(INVALID_ARGS),
        },
        "BalanceInc" | "BalanceDec" | "FadeInc" | "FadeDec" => ok(vec![]),
        "Balance" | "Fade" => ok(vec![("Value", "0".into())]),
        "SetMute" => {
            r.set_mute(parse_bool(c.arg("Value")).ok_or(INVALID_ARGS)?);
            ok(vec![])
        }
        "Mute" => ok(vec![("Value", bool_word(r.muted()).into())]),
        "VolumeLimit" => ok(vec![("Value", "100".into())]),
        _ => Err(INVALID_ACTION),
    }
}

pub fn volume_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    vec![
        ("Volume", r.volume().to_string()),
        ("Mute", bool_word(r.muted()).into()),
        ("Balance", "0".into()),
        ("Fade", "0".into()),
        ("VolumeLimit", "100".into()),
        ("VolumeMax", "100".into()),
        ("VolumeUnity", "100".into()),
        ("VolumeSteps", "100".into()),
        ("VolumeMilliDbPerStep", "600".into()),
        ("BalanceMax", "0".into()),
        ("FadeMax", "0".into()),
    ]
}

pub fn info(r: &Renderer, c: &Call) -> Reply {
    let all = info_vars(r);
    let pick = |names: &[&'static str]| -> Vec<(&'static str, String)> {
        names.iter().filter_map(|n| all.iter().find(|(m, _)| m == n).cloned()).collect()
    };
    match c.action.as_str() {
        "Counters" => Ok(pick(&["TrackCount", "DetailsCount", "MetatextCount"])),
        "Track" => Ok(pick(&["Uri", "Metadata"])),
        "Details" => Ok(pick(&["Duration", "BitRate", "BitDepth", "SampleRate", "Lossless", "CodecName"])),
        "Metatext" => Ok(all.iter().find(|(n, _)| *n == "Metatext").map(|(_, v)| vec![("Value", v.clone())]).unwrap_or_default()),
        _ => Err(INVALID_ACTION),
    }
}

pub fn info_vars(r: &Renderer) -> Vec<(&'static str, String)> {
    let now = r.now();
    let seen = r.seen();
    let playing = matches!(r.transport(), Transport::Playing | Transport::Paused);
    let (rate, bits) = if playing { (seen.src_rate, seen.bits) } else { (0, 0) };
    let codec = match now.as_ref().map(|n| n.0.meta.mime.as_str()).unwrap_or("") {
        m if m.contains("flac") => "FLAC",
        m if m.contains("wav") => "WAV",
        m if m.contains("aiff") => "AIFF",
        m if m.contains("dsf") || m.contains("dff") || m.contains("dsd") => "DSD",
        m if m.contains("mpeg") || m.contains("mp3") => "MP3",
        m if m.contains("mp4") || m.contains("m4a") || m.contains("aac") => "AAC",
        _ => "",
    };
    let lossless = !matches!(codec, "MP3" | "AAC");
    let bitrate = u64::from(rate) * u64::from(bits) * 2;
    let n = track_count(r);
    vec![
        ("TrackCount", n.to_string()),
        ("DetailsCount", n.to_string()),
        ("MetatextCount", n.to_string()),
        ("Uri", now.as_ref().map_or(String::new(), |n| n.0.uri.clone())),
        ("Metadata", now.as_ref().map_or(String::new(), |n| n.0.didl.clone())),
        ("Duration", now.as_ref().map_or(0, |n| n.2.max(0.0) as u32).to_string()),
        ("BitRate", bitrate.min(u64::from(u32::MAX)).to_string()),
        ("BitDepth", bits.to_string()),
        ("SampleRate", rate.to_string()),
        ("Lossless", bool_word(lossless).into()),
        ("CodecName", codec.into()),
        ("Metatext", if seen.radio { seen.radio_title } else { String::new() }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_array_is_big_endian_in_base64() {
        assert_eq!(id_array(&[]), "");
        assert_eq!(id_array(&[1]), "AAAAAQ==");
        assert_eq!(id_array(&[1, 2]), "AAAAAQAAAAI=");
        assert_eq!(base64(b"Man"), "TWFu");
    }
}
