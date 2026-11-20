//! Events (GENA): a control point subscribes to a service and is sent its
//! variables as they change, so it need not ask every second.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::renderer::Renderer;
use super::xml::esc;

/// What a subscription may ask for, at most; renewed before it lapses.
const MAX_TIMEOUT_S: u64 = 1800;
/// Events at most this often for one subscription.
const MODERATION: Duration = Duration::from_millis(200);
/// Failed sends in a row after which a subscriber is taken as gone.
const MAX_FAILURES: u32 = 3;

struct Sub {
    sid: String,
    service: &'static str,
    callbacks: Vec<String>,
    expires: Instant,
    seq: u32,
    /// What it was last sent (empty: the first event is still to go).
    last: Vec<(&'static str, String)>,
    failures: u32,
}

#[derive(Default)]
pub struct Subs {
    list: Mutex<Vec<Sub>>,
}

/// The callback addresses of a CALLBACK header (`<http://…><http://…>`):
/// only plain http on the home network.
pub fn callbacks(header: &str) -> Vec<String> {
    header
        .split('<')
        .filter_map(|p| p.split('>').next())
        .map(str::trim)
        .filter(|u| {
            let Some(rest) = u.strip_prefix("http://") else { return false };
            let host = rest.split('/').next().unwrap_or("");
            let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
            host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(super::private_ip)
        })
        .map(str::to_string)
        .collect()
}

/// `Second-N` or `infinite`, as asked, within what is given.
pub fn timeout_of(header: Option<&str>) -> u64 {
    header
        .and_then(|h| h.trim().strip_prefix("Second-").or_else(|| h.trim().strip_prefix("second-")))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(MAX_TIMEOUT_S)
        .clamp(60, MAX_TIMEOUT_S)
}

impl Subs {
    pub fn subscribe(&self, service: &'static str, callbacks: Vec<String>, timeout_s: u64) -> String {
        let sid = format!("uuid:{}", super::new_uuid());
        self.list.lock().unwrap().push(Sub {
            sid: sid.clone(),
            service,
            callbacks,
            expires: Instant::now() + Duration::from_secs(timeout_s),
            seq: 0,
            last: Vec::new(),
            failures: 0,
        });
        sid
    }

    pub fn renew(&self, sid: &str, timeout_s: u64) -> bool {
        let mut l = self.list.lock().unwrap();
        match l.iter_mut().find(|s| s.sid == sid) {
            Some(s) => {
                s.expires = Instant::now() + Duration::from_secs(timeout_s);
                true
            }
            None => false,
        }
    }

    pub fn unsubscribe(&self, sid: &str) -> bool {
        let mut l = self.list.lock().unwrap();
        let before = l.len();
        l.retain(|s| s.sid != sid);
        l.len() != before
    }
}

/// The body of one event: each variable a property, or the AV services'
/// one LastChange.
fn propertyset(props: &[(&str, String)]) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">");
    for (name, value) in props {
        x.push_str(&format!("<e:property><{name}>{}</{name}></e:property>", esc(value)));
    }
    x.push_str("</e:propertyset>");
    x
}

/// Send events as things change, to every subscriber, for as long as the
/// program runs.
pub async fn run(subs: Arc<Subs>, r: Arc<Renderer>) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap_or_default();
    let notify = reqwest::Method::from_bytes(b"NOTIFY").expect("a method name");
    loop {
        // A change, or a second at most (OpenHome's Time counts seconds).
        let _ = tokio::time::timeout(Duration::from_secs(1), r.changed.notified()).await;
        tokio::time::sleep(MODERATION).await;
        // What each subscription is to be sent now.
        let mut sends = Vec::new();
        {
            let services: Vec<&'static str> = {
                let now = Instant::now();
                let mut l = subs.list.lock().unwrap();
                l.retain(|s| s.expires > now && s.failures < MAX_FAILURES);
                l.iter().map(|s| s.service).collect()
            };
            let mut current = Vec::new();
            for s in services {
                if !current.iter().any(|(n, _): &(&str, _)| *n == s) {
                    let r2 = r.clone();
                    let vars = tokio::task::spawn_blocking(move || super::vars_of(&r2, s)).await.unwrap_or_default();
                    current.push((s, vars));
                }
            }
            let mut l = subs.list.lock().unwrap();
            for sub in l.iter_mut() {
                let Some((_, vars)) = current.iter().find(|(n, _)| *n == sub.service) else { continue };
                let changed: Vec<(&'static str, String)> = if sub.last.is_empty() {
                    vars.clone()
                } else {
                    vars.iter().filter(|(n, v)| sub.last.iter().find(|(m, _)| m == n).is_none_or(|(_, w)| w != v)).cloned().collect()
                };
                if changed.is_empty() {
                    continue;
                }
                sub.last = vars.clone();
                let body = propertyset(&super::event_props(sub.service, &changed));
                sends.push((sub.sid.clone(), sub.callbacks.clone(), sub.seq, body));
                sub.seq = sub.seq.checked_add(1).unwrap_or(1);
            }
        }
        for (sid, callbacks, seq, body) in sends {
            let mut sent = false;
            for url in &callbacks {
                let req = client
                    .request(notify.clone(), url)
                    .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
                    .header("NT", "upnp:event")
                    .header("NTS", "upnp:propchange")
                    .header("SID", &sid)
                    .header("SEQ", seq.to_string())
                    .body(body.clone());
                if req.send().await.is_ok_and(|a| a.status().is_success()) {
                    sent = true;
                    break;
                }
            }
            let mut l = subs.list.lock().unwrap();
            if let Some(s) = l.iter_mut().find(|s| s.sid == sid) {
                s.failures = if sent { 0 } else { s.failures + 1 };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_callbacks_on_the_home_network_are_kept() {
        let c = callbacks("<http://192.168.1.20:49152/evt/1><http://8.8.8.8/x><https://192.168.1.2/><http://name.example/>");
        assert_eq!(c, ["http://192.168.1.20:49152/evt/1"]);
        assert!(callbacks("").is_empty());
    }

    #[test]
    fn the_timeout_is_kept_within_bounds() {
        assert_eq!(timeout_of(Some("Second-300")), 300);
        assert_eq!(timeout_of(Some("Second-5")), 60);
        assert_eq!(timeout_of(Some("infinite")), MAX_TIMEOUT_S);
        assert_eq!(timeout_of(None), MAX_TIMEOUT_S);
    }

    #[test]
    fn a_subscription_is_renewed_and_ended_by_its_sid() {
        let s = Subs::default();
        let sid = s.subscribe("avt", vec!["http://192.168.1.2/".into()], 300);
        assert!(sid.starts_with("uuid:"));
        assert!(s.renew(&sid, 600));
        assert!(s.unsubscribe(&sid));
        assert!(!s.renew(&sid, 600));
    }

    #[test]
    fn a_propertyset_escapes_what_it_carries() {
        let x = propertyset(&[("LastChange", "<Event/>".into())]);
        assert!(x.contains("<e:property><LastChange>&lt;Event/&gt;</LastChange></e:property>"));
    }
}
