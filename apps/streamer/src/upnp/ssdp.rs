//! SSDP: how control points find the renderer. It answers their searches on
//! the multicast group and says it is there (and, on the way out, that it is
//! gone).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
const MAX_AGE_S: u64 = 1800;
/// How often it says it is there: well within its max-age.
const ALIVE_EVERY: Duration = Duration::from_secs(600);

pub struct Ssdp {
    pub uuid: String,
    pub http_port: u16,
    /// Every notification type: the root device, the device itself, its type,
    /// each service's type.
    pub types: Vec<String>,
}

impl Ssdp {
    fn usn(&self, nt: &str) -> String {
        if nt == format!("uuid:{}", self.uuid) {
            nt.to_string()
        } else {
            format!("uuid:{}::{nt}", self.uuid)
        }
    }

    /// The types that answer search target `st`.
    fn matches(&self, st: &str) -> Vec<String> {
        let st = st.trim();
        if st == "ssdp:all" {
            return self.types.clone();
        }
        if let Some(t) = self.types.iter().find(|t| t.as_str() == st) {
            return vec![t.clone()];
        }
        // A lower version of a type this one has (e.g. MediaRenderer:1 of :2).
        self.types
            .iter()
            .filter(|t| {
                let (a, va) = t.rsplit_once(':').unwrap_or((t, ""));
                let (b, vb) = st.rsplit_once(':').unwrap_or((st, ""));
                a == b && t.starts_with("urn:") && vb.parse::<u32>().ok().zip(va.parse::<u32>().ok()).is_some_and(|(x, y)| x <= y)
            })
            .map(|_| st.to_string())
            .collect()
    }

    fn location(&self, ip: IpAddr) -> String {
        format!("http://{ip}:{}/upnp/device.xml", self.http_port)
    }

    fn answer(&self, st: &str, ip: IpAddr) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE_S}\r\nDATE: {}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {}\r\nST: {st}\r\nUSN: {}\r\n\
             BOOTID.UPNP.ORG: 1\r\nCONFIGID.UPNP.ORG: 1\r\nContent-Length: 0\r\n\r\n",
            http_date(),
            self.location(ip),
            server_word(),
            self.usn(st),
        )
    }

    fn notify(&self, nt: &str, alive: bool, ip: IpAddr) -> String {
        if alive {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age={MAX_AGE_S}\r\nLOCATION: {}\r\nNT: {nt}\r\n\
                 NTS: ssdp:alive\r\nSERVER: {}\r\nUSN: {}\r\nBOOTID.UPNP.ORG: 1\r\nCONFIGID.UPNP.ORG: 1\r\nContent-Length: 0\r\n\r\n",
                self.location(ip),
                server_word(),
                self.usn(nt),
            )
        } else {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {}\r\n\
                 BOOTID.UPNP.ORG: 1\r\nCONFIGID.UPNP.ORG: 1\r\nContent-Length: 0\r\n\r\n",
                self.usn(nt),
            )
        }
    }

    /// Say to the group that it is there (`alive`) or gone.
    pub fn announce(&self, alive: bool) {
        let Some(ip) = super::lan_ipv4() else { return };
        let Ok(s) = UdpSocket::bind(SocketAddrV4::new(ip, 0)) else { return };
        let _ = s.set_multicast_ttl_v4(4);
        for nt in &self.types {
            let _ = s.send_to(self.notify(nt, alive, IpAddr::V4(ip)).as_bytes(), SocketAddrV4::new(GROUP, PORT));
        }
    }
}

fn server_word() -> String {
    format!("{}/{} UPnP/1.0 AuraStreamer/{}", std::env::consts::OS, "1.0", env!("CARGO_PKG_VERSION"))
}

/// The date as HTTP writes it (RFC 1123).
pub fn http_date() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(mo - 1) as usize];
    format!("{weekday}, {d:02} {month} {y} {h:02}:{m:02}:{s:02} GMT")
}

/// The search's target and how long it allows for an answer, when the
/// datagram is an M-SEARCH for devices.
fn search_of(msg: &str) -> Option<(String, u64)> {
    let mut lines = msg.split("\r\n");
    if !lines.next()?.to_ascii_uppercase().starts_with("M-SEARCH * HTTP/1.1") {
        return None;
    }
    let mut st = None;
    let mut mx = 1;
    let mut discover = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_uppercase().as_str() {
            "ST" => st = Some(v.trim().to_string()),
            "MX" => mx = v.trim().parse().unwrap_or(1),
            "MAN" => discover = v.trim().trim_matches('"') == "ssdp:discover",
            _ => {}
        }
    }
    discover.then_some(())?;
    Some((st?, mx))
}

/// The local address this machine reaches `peer` from.
fn local_toward(peer: SocketAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect(peer).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn group_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Other programs on this machine listen here too (the system's own
    // discovery, other players).
    s.set_reuse_address(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    let s: UdpSocket = s.into();
    let mut joined = s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED).is_ok();
    if let Some(ip) = super::lan_ipv4() {
        joined |= s.join_multicast_v4(&GROUP, &ip).is_ok();
    }
    if !joined {
        return Err(std::io::Error::other("cannot join the SSDP group"));
    }
    Ok(s)
}

/// Answer searches and keep saying it is there, on threads of its own.
pub fn start(ssdp: Arc<Ssdp>) {
    let socket = match group_socket() {
        Ok(s) => s,
        Err(e) => {
            aura_core::aelog!("[UPNP] discovery is off: {e}");
            return;
        }
    };
    {
        let ssdp = ssdp.clone();
        let _ = std::thread::Builder::new().name("upnp-alive".into()).spawn(move || loop {
            // Three times at the start: datagrams get lost.
            for _ in 0..3 {
                ssdp.announce(true);
                std::thread::sleep(Duration::from_millis(300));
            }
            std::thread::sleep(ALIVE_EVERY);
        });
    }
    let _ = std::thread::Builder::new().name("upnp-ssdp".into()).spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf) else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if !super::private_ip(peer.ip()) {
                continue;
            }
            let Some((st, mx)) = search_of(&String::from_utf8_lossy(&buf[..n])) else { continue };
            let types = ssdp.matches(&st);
            if types.is_empty() {
                continue;
            }
            let Some(ip) = local_toward(peer) else { continue };
            let ssdp = ssdp.clone();
            // Within the time the search allows, at random: many devices
            // answering at once lose datagrams.
            let wait = rand::thread_rng().gen_range(0..=(mx.clamp(1, 5) * 1000).min(1000));
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(wait));
                let Ok(out) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return };
                for t in types {
                    let _ = out.send_to(ssdp.answer(&t, ip).as_bytes(), peer);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssdp() -> Ssdp {
        Ssdp {
            uuid: "abc".into(),
            http_port: 49494,
            types: vec![
                "upnp:rootdevice".into(),
                "uuid:abc".into(),
                "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                "urn:schemas-upnp-org:service:AVTransport:1".into(),
            ],
        }
    }

    #[test]
    fn a_search_is_read_and_answered_for_what_it_asks() {
        let m = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        let (st, mx) = search_of(m).unwrap();
        assert_eq!((st.as_str(), mx), ("urn:schemas-upnp-org:device:MediaRenderer:1", 3));
        assert!(search_of("NOTIFY * HTTP/1.1\r\nNT: x\r\n\r\n").is_none());
        assert!(search_of("M-SEARCH * HTTP/1.1\r\nST: ssdp:all\r\n\r\n").is_none(), "no MAN");
        let s = ssdp();
        assert_eq!(s.matches("ssdp:all").len(), 4);
        assert_eq!(s.matches("uuid:abc"), ["uuid:abc"]);
        assert!(s.matches("urn:schemas-upnp-org:device:MediaServer:1").is_empty());
        let a = s.answer("upnp:rootdevice", "192.168.1.9".parse().unwrap());
        assert!(a.contains("LOCATION: http://192.168.1.9:49494/upnp/device.xml\r\n"));
        assert!(a.contains("USN: uuid:abc::upnp:rootdevice\r\n"));
        assert!(s.answer("uuid:abc", "192.168.1.9".parse().unwrap()).contains("USN: uuid:abc\r\n"));
    }

    #[test]
    fn the_date_is_written_as_http_does() {
        let d = http_date();
        assert!(d.ends_with(" GMT") && d.len() == 29, "{d}");
    }
}
