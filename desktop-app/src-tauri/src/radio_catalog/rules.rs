//! What the catalog lists, in one place: the services whose terms forbid a
//! client like this one, and the formats the player plays.

/// A service whose terms forbid third-party clients outright: its stations
/// are not listed (an address the listener pastes still plays — the
/// service's own links for personal listening are just that).
pub struct Blocked {
    /// The service's domain: a station whose address, home page or icon is
    /// on it (or under it) is the service's.
    pub domain: &'static str,
    /// Lower-case names a station of the service is listed under.
    pub names: &'static [&'static str],
}

/// The services not listed (Anton 2.10: where a third-party client is
/// explicitly forbidden). SomaFM, terms of 04.09.2026: "We can't grant
/// permission for third-party SomaFM clients or applications, even
/// noncommercial ones."
pub const BLOCKED: &[Blocked] = &[Blocked { domain: "somafm.com", names: &["somafm", "soma fm", "soma.fm"] }];

/// Whether the player plays a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Plays,
    /// Listed, marked "not supported yet".
    NotYet,
    /// A TV channel's stream: not listed.
    Video,
}

/// What the player decodes, by Radio Browser's codec name (upper case). A
/// format the player learns is one row here; bump `FORMATS_REV` with it, so
/// a station remembered as failing on its format is tried again.
///
/// OGG plays as Vorbis or Opus (Radio Browser files Opus under OGG too);
/// AAC+ (HE-AAC) through the FDK AAC decoder, Opus through libopus.
/// UNKNOWN (and no name at all) is tried: most are MP3.
pub const FORMATS: &[(&str, Support)] = &[
    ("MP3", Support::Plays),
    ("AAC", Support::Plays),
    ("FLAC", Support::Plays),
    ("OGG", Support::Plays),
    ("UNKNOWN", Support::Plays),
    ("AAC+", Support::Plays),
    ("OPUS", Support::Plays),
];

/// HLS (`hls` = 1 in Radio Browser): a playlist of segments, not a stream.
/// It plays as far as the codec inside it does.
pub const HLS: Support = Support::Plays;

/// The revision of `FORMATS` and `HLS`.
pub const FORMATS_REV: u32 = 3;

/// Codec names that mean video.
const VIDEO: &[&str] = &["H.264", "H.265", "HEVC", "FLV", "VP8", "VP9", "THEORA", "MPEG-TS"];

/// Whether a station Radio Browser lists with `codec` (its names, comma
/// separated) and `hls` plays here. A codec not in the table is "not yet".
pub fn support(codec: &str, hls: bool) -> Support {
    let parts: Vec<String> = codec.split(',').map(|c| c.trim().to_ascii_uppercase()).filter(|c| !c.is_empty()).collect();
    if parts.iter().any(|c| VIDEO.contains(&c.as_str())) {
        return Support::Video;
    }
    if hls && HLS != Support::Plays {
        return HLS;
    }
    let name = parts.first().map(String::as_str).unwrap_or("UNKNOWN");
    FORMATS.iter().find(|(n, _)| *n == name).map_or(Support::NotYet, |(_, s)| *s)
}

/// The host of an http(s) address, lower case.
fn host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let host = auth.rsplit('@').next()?;
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether a station is one of a service that forbids third-party clients:
/// by any of its addresses (stream, resolved stream, home page, icon) or
/// its name.
pub fn blocked(name: &str, addresses: &[&str]) -> bool {
    let name = name.to_lowercase();
    BLOCKED.iter().any(|b| {
        b.names.iter().any(|n| name.contains(n))
            || addresses.iter().filter_map(|a| host(a)).any(|h| h == b.domain || h.ends_with(&format!(".{}", b.domain)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formats_the_player_plays() {
        for c in ["MP3", "mp3", "AAC", "FLAC", "OGG", "UNKNOWN", "", " MP3 "] {
            assert_eq!(support(c, false), Support::Plays, "{c:?}");
        }
        // HE-AAC and Opus play; a name nobody listed is not assumed.
        assert_eq!(support("AAC+", false), Support::Plays);
        assert_eq!(support("OPUS", false), Support::Plays);
        assert_eq!(support("MP4", false), Support::NotYet);
        // HLS plays as far as its codec does.
        assert_eq!(support("AAC", true), Support::Plays);
        assert_eq!(support("MP3", true), Support::Plays);
        assert_eq!(support("", true), Support::Plays);
        assert_eq!(support("AAC+", true), Support::Plays);
        // TV channels are not listed.
        assert_eq!(support("AAC,H.264", false), Support::Video);
        assert_eq!(support("UNKNOWN,H.264", true), Support::Video);
        assert_eq!(support("FLV", false), Support::Video);
    }

    #[test]
    fn a_service_that_forbids_clients_is_not_listed() {
        // By its addresses, under its domain at any depth.
        assert!(blocked("Groove Salad", &["https://ice1.somafm.com/groovesalad-256-mp3"]));
        assert!(blocked("Groove Salad", &["http://example.org/x", "https://somafm.com/"]));
        assert!(blocked("Drone Zone", &["http://user@SomaFM.com:8000/drone"]));
        // By its name.
        assert!(blocked("SomaFM: Groove Salad", &["http://example.org/x"]));
        assert!(blocked("Soma FM Drone Zone", &[]));
        // Not a look-alike domain, not a name that merely contains "soma".
        assert!(!blocked("Groove", &["https://notsomafm.com/x", "https://somafm.com.example.org/"]));
        assert!(!blocked("Radio Somalia", &["http://example.org/x"]));
        assert!(!blocked("Radio Paradise", &["https://stream.radioparadise.com/flac", "", "not an address"]));
    }

    #[test]
    fn hosts_are_read_from_any_address_form() {
        assert_eq!(host("https://Stream.Example.org:8443/a?b#c").as_deref(), Some("stream.example.org"));
        assert_eq!(host("http://[2a01:4f8::1]:80/x").as_deref(), Some("2a01:4f8::1"));
        assert_eq!(host("http://example.org.").as_deref(), Some("example.org"));
        assert_eq!(host("example.org/x"), None);
        assert_eq!(host("http:///x"), None);
    }
}
