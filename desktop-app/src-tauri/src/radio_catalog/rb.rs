//! Radio Browser (api.radio-browser.info): an open catalog of stations,
//! "completely free and open source. You may use it in free and non free
//! software". Asked as an ordinary client asks it: by name (User-Agent), one
//! mirror at a time — the next when one fails — and nothing asked again that
//! was asked lately.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use super::rules::{self, Support};

pub(super) const USER_AGENT: &str = concat!("AuraEngine/", env!("CARGO_PKG_VERSION"));
/// Any mirror answers with the list of them all.
const DISCOVERY: &str = "https://all.api.radio-browser.info/json/servers";
/// When that does not answer and no list was kept.
const FALLBACK: &[&str] = &["de1.api.radio-browser.info", "de2.api.radio-browser.info", "fi1.api.radio-browser.info"];
const TIMEOUT: Duration = Duration::from_secs(10);
const DAY_S: u64 = 24 * 3600;
/// A search asked again within this long is answered from memory.
const SEARCH_TTL: Duration = Duration::from_secs(600);
const SEARCH_KEEP: usize = 64;
/// Stations per page of a search.
pub const PAGE: u32 = 50;
/// Stations per page of a search by name: every match at once, as a rule, so
/// that the page can put the closest names first (a page of the most voted
/// would hold only some of them).
pub const NAME_PAGE: u32 = 500;
/// Genres kept in the list (the most stations first).
const TAGS_KEEP: usize = 200;

pub(super) fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub(super) fn dir() -> Option<PathBuf> {
    crate::app_dir::root().map(|r| r.join("radio"))
}

/// Why a mirror gave no answer: try the next one, or not (the request
/// itself was refused).
#[derive(Debug)]
pub(super) enum Fail {
    Next(String),
    Stop(String),
}

/// Ask the mirrors `names` in turn from `start`: the first answer, with the
/// index of the mirror that gave it. One that fails gives way to the next;
/// a request refused as such is not asked of the others.
pub(super) async fn first_answer<T, F, Fut>(names: &[String], start: usize, mut ask: F) -> Result<(usize, T), String>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    let mut last = "no catalog server is known".to_string();
    for k in 0..names.len() {
        let i = (start + k) % names.len();
        match ask(names[i].clone()).await {
            Ok(v) => return Ok((i, v)),
            Err(Fail::Next(e)) => {
                crate::aelog!("[CATALOG] {} did not answer: {}", names[i], e);
                last = e;
            }
            Err(Fail::Stop(e)) => return Err(e),
        }
    }
    Err(last)
}

/// The mirrors' names from the list a mirror gives (`/json/servers`), each
/// once, in the list's order.
pub(super) fn parse_servers(body: &str) -> Vec<String> {
    let list: Vec<Value> = serde_json::from_str(body).unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for s in list {
        let n = s.get("name").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        let ok = !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if ok && !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// A number Radio Browser may send as a number or as text.
fn num(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as u8 as f64,
        _ => 0.0,
    }
}

fn http(u: String) -> String {
    let l = u.to_ascii_lowercase();
    if (l.starts_with("http://") || l.starts_with("https://")) && !u.chars().any(char::is_whitespace) {
        u
    } else {
        String::new()
    }
}

/// A station as the panel shows it, from Radio Browser's record; None for
/// one not listed here (no name or address, a forbidden service's, video).
pub(super) fn station(v: &Value) -> Option<Value> {
    let name = text(v, "name");
    let url = [text(v, "url_resolved"), text(v, "url")].into_iter().map(http).find(|u| !u.is_empty())?;
    if name.is_empty() {
        return None;
    }
    let homepage = http(text(v, "homepage"));
    let favicon = http(text(v, "favicon"));
    if rules::blocked(&name, &[&url, &text(v, "url"), &homepage, &favicon]) {
        return None;
    }
    let codec = text(v, "codec").to_ascii_uppercase();
    let hls = num(v, "hls") > 0.0;
    let plays = match rules::support(&codec, hls) {
        Support::Video => return None,
        Support::Plays => true,
        Support::NotYet => false,
    };
    let mut tags: Vec<String> = Vec::new();
    for t in text(v, "tags").split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
        if tags.len() < 5 && !tags.contains(&t) {
            tags.push(t);
        }
    }
    Some(json!({
        "uuid": text(v, "stationuuid"),
        "name": name,
        "url": url,
        "homepage": homepage,
        "favicon": favicon,
        "tags": tags,
        "country": text(v, "country"),
        "countrycode": text(v, "countrycode").to_ascii_uppercase(),
        "language": text(v, "language"),
        "codec": if codec == "UNKNOWN" { String::new() } else { codec },
        "bitrate": num(v, "bitrate").max(0.0) as u32,
        "hls": hls,
        "votes": num(v, "votes").max(0.0) as u64,
        "clickcount": num(v, "clickcount").max(0.0) as u64,
        "plays": plays,
    }))
}

/// A search as the panel asks it. Empty fields do not narrow it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Query {
    pub name: String,
    /// A genre from the list (exact).
    pub tag: String,
    /// ISO 3166-1 code.
    pub country: String,
    /// A language from the list (exact).
    pub language: String,
    /// Radio Browser's codec name.
    pub codec: String,
    /// How the page lists what is found: "name" (the default), "votes",
    /// "clickcount" or "quality" — the catalog is asked in the same order,
    /// so that a next page goes on from where the last one ended.
    pub order: String,
    /// "lossless", or the kbps the stations reach at least ("320", "256",
    /// ...): a lossless station (`LOSSLESS`) passes any, one of unknown
    /// bitrate none. Anything else: any quality.
    pub quality: String,
    pub offset: u32,
}

/// The codec a lossless station is listed with — only a handful are: a
/// lossless stream in an Ogg container (FLAC in Ogg: Radio Paradise's FLAC
/// mixes, at 1441 kbps) is listed as Ogg, told from a lossy one by its
/// bitrate (Vorbis and Opus stay under 510 kbps).
const LOSSLESS: &str = "FLAC";
const OGG: &str = "OGG";
const OGG_LOSSLESS_KBPS: u32 = 600;

/// What a search's quality asks for (`Query::quality`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Quality {
    Any,
    Lossless,
    /// At least this many kbps — or lossless.
    AtLeast(u32),
}

impl Query {
    fn quality(&self) -> Quality {
        let q = self.quality.trim();
        if q.eq_ignore_ascii_case("lossless") {
            return Quality::Lossless;
        }
        match q.parse::<u32>() {
            Ok(k) if k > 0 => Quality::AtLeast(k),
            _ => Quality::Any,
        }
    }

    /// The catalog's asks for this search: one, or a few that make one page
    /// (the catalog cannot ask "this bitrate or FLAC" at once): lossless is
    /// FLAC, and Ogg past `OGG_LOSSLESS_KBPS`; a bitrate asks the lossless
    /// stations beside it. None when the format chosen cannot be lossless.
    /// Each filters on the catalog's side, so nothing past a page is lost.
    pub(super) fn asks(&self) -> Vec<Vec<(&'static str, String)>> {
        let codec = self.codec.trim();
        let is = |c: &str| codec.eq_ignore_ascii_case(c);
        let ask = |c: Option<&str>, min: Option<u32>| {
            let mut p = match c {
                Some(c) => Query { codec: c.into(), ..self.clone() }.params(),
                None => self.params(),
            };
            if let Some(k) = min {
                p.push(("bitrateMin", k.to_string()));
            }
            p
        };
        let mut asks = Vec::new();
        match self.quality() {
            // Best quality first: the FLAC stations asked beside, as their
            // bitrate is often not known (0) and would come last.
            Quality::Any if self.order == "quality" && codec.is_empty() => {
                asks.push(ask(None, None));
                asks.push(ask(Some(LOSSLESS), None));
            }
            Quality::Any => asks.push(ask(None, None)),
            Quality::Lossless => {
                if codec.is_empty() || is(LOSSLESS) {
                    asks.push(ask(Some(LOSSLESS), None));
                }
                if codec.is_empty() || is(OGG) {
                    asks.push(ask(Some(OGG), Some(OGG_LOSSLESS_KBPS)));
                }
            }
            Quality::AtLeast(_) if is(LOSSLESS) => asks.push(ask(Some(LOSSLESS), None)),
            Quality::AtLeast(k) => {
                asks.push(ask(None, Some(k)));
                if codec.is_empty() {
                    asks.push(ask(Some(LOSSLESS), None));
                }
                // A lossless Ogg stream is past any bitrate offered; past a
                // higher one asked, it is asked on its own.
                if (codec.is_empty() || is(OGG)) && k > OGG_LOSSLESS_KBPS {
                    asks.push(ask(Some(OGG), Some(OGG_LOSSLESS_KBPS)));
                }
            }
        }
        asks
    }
    /// How many stations a page of this search holds: a search by name
    /// lists them all at once (`NAME_PAGE`).
    pub(super) fn page(&self) -> u32 {
        if self.name.trim().is_empty() {
            PAGE
        } else {
            NAME_PAGE
        }
    }

    /// The search's parameters, broken stations left out, in the order the
    /// page lists them: by name, the most voted or played first, the highest
    /// bitrate first.
    pub(super) fn params(&self) -> Vec<(&'static str, String)> {
        let (order, reverse) = match self.order.as_str() {
            "votes" => ("votes", true),
            "clickcount" => ("clickcount", true),
            "quality" => ("bitrate", true),
            _ => ("name", false),
        };
        let mut p: Vec<(&'static str, String)> = vec![
            ("hidebroken", "true".into()),
            ("order", order.into()),
            ("reverse", reverse.to_string()),
            ("limit", self.page().to_string()),
            ("offset", self.offset.to_string()),
        ];
        let mut put = |k: &'static str, v: &str| {
            let v = v.trim();
            if !v.is_empty() {
                p.push((k, v.chars().take(100).collect()));
            }
        };
        put("name", &self.name);
        put("tag", &self.tag);
        put("countrycode", &self.country.to_ascii_uppercase());
        put("language", &self.language);
        put("codec", &self.codec);
        if !self.tag.trim().is_empty() {
            p.push(("tagExact", "true".into()));
        }
        if !self.language.trim().is_empty() {
            p.push(("languageExact", "true".into()));
        }
        p
    }
}

/// The stations of one page of a search (Radio Browser's answer `body`) as
/// the panel lists them; how many the catalog gave (the next page's offset
/// counts the ones not listed here too), and whether a next page may have
/// more: a full page of `page`.
pub(super) fn parse_search(body: &str, page: u32) -> Result<Value, String> {
    let list: Vec<Value> = serde_json::from_str(body).map_err(|e| format!("the catalog's answer: {}", e))?;
    let more = list.len() as u32 >= page;
    let stations: Vec<Value> = list.iter().filter_map(station).collect();
    Ok(json!({ "stations": stations, "count": list.len(), "more": more, "formatsRev": rules::FORMATS_REV }))
}

/// The genres, countries and languages to choose from, from Radio Browser's
/// three lists: genres and languages by how many stations they have,
/// countries by name.
pub(super) fn parse_lists(tags: &str, countries: &str, languages: &str) -> Result<Value, String> {
    let read = |body: &str| -> Result<Vec<Value>, String> {
        serde_json::from_str(body).map_err(|e| format!("the catalog's list: {}", e))
    };
    let counted = |list: Vec<Value>, keep: usize| -> Vec<Value> {
        let mut v: Vec<(String, u64)> = list
            .iter()
            .map(|x| (text(x, "name").to_lowercase(), num(x, "stationcount") as u64))
            .filter(|(n, c)| !n.is_empty() && *c > 0)
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.dedup_by(|a, b| a.0 == b.0);
        v.into_iter().take(keep).map(|(n, c)| json!({ "name": n, "count": c })).collect()
    };
    let mut cs: Vec<(String, String, u64)> = read(countries)?
        .iter()
        .map(|x| (text(x, "iso_3166_1").to_ascii_uppercase(), text(x, "name"), num(x, "stationcount") as u64))
        .filter(|(code, name, c)| code.len() == 2 && !name.is_empty() && *c > 0)
        .collect();
    cs.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    cs.dedup_by(|a, b| a.0 == b.0);
    Ok(json!({
        "tags": counted(read(tags)?, TAGS_KEEP),
        "countries": cs.into_iter().map(|(code, name, c)| json!({ "code": code, "name": name, "count": c })).collect::<Vec<_>>(),
        "languages": counted(read(languages)?, usize::MAX),
    }))
}

/// The address `/json/url/{uuid}` gives for a station (the call that counts
/// a listener's start, as Radio Browser asks of its clients).
pub(super) fn parse_station_url(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let u = http(text(&v, "url"));
    (!u.is_empty()).then_some(u)
}

/// A station's uuid as Radio Browser writes it.
pub(super) fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

struct Mirrors {
    names: Vec<String>,
    /// The one asked first: the last that answered.
    at: usize,
    /// Unix seconds the list is good until.
    until: u64,
}

struct Rb {
    client: reqwest::Client,
    mirrors: Mutex<Option<Mirrors>>,
    searches: Mutex<HashMap<String, (Instant, Value)>>,
}

fn rb() -> &'static Rb {
    static RB: OnceLock<Rb> = OnceLock::new();
    RB.get_or_init(|| Rb {
        client: reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default(),
        mirrors: Mutex::new(None),
        searches: Mutex::new(HashMap::new()),
    })
}

/// The kept list of mirrors: `{ "until": unix s, "names": [...] }`.
fn servers_file() -> Option<PathBuf> {
    dir().map(|d| d.join("servers.json"))
}

fn read_json(path: Option<PathBuf>) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path?).ok()?).ok()
}

async fn get(client: &reqwest::Client, url: &str, query: &[(&str, String)]) -> Result<String, Fail> {
    let resp = client.get(url).query(query).send().await.map_err(|e| Fail::Next(e.to_string()))?;
    let st = resp.status();
    if st.is_server_error() || st.as_u16() == 429 {
        return Err(Fail::Next(format!("HTTP {}", st.as_u16())));
    }
    if !st.is_success() {
        return Err(Fail::Stop(format!("HTTP {}", st.as_u16())));
    }
    resp.text().await.map_err(|e| Fail::Next(e.to_string()))
}

/// The mirrors as known now, while the list is good.
fn mirrors_known() -> Option<(Vec<String>, usize)> {
    let m = rb().mirrors.lock().unwrap();
    m.as_ref().filter(|m| m.until > now_s()).map(|m| (m.names.clone(), m.at))
}

/// The mirrors to ask, the one that answered last first. The list is asked
/// for once a day; without an answer, the kept one serves (however old),
/// else the names known here.
async fn mirrors() -> (Vec<String>, usize) {
    if let Some(known) = mirrors_known() {
        return known;
    }
    // One asks for the list; a search and the lists asked at once wait for
    // its answer rather than ask again.
    static ASKING: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _one = ASKING.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    if let Some(known) = mirrors_known() {
        return known;
    }
    let kept = read_json(servers_file());
    let kept_names: Vec<String> = kept
        .as_ref()
        .and_then(|v| v.get("names"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    let kept_until = kept.as_ref().map_or(0, |v| num(v, "until") as u64);
    let (names, until) = if !kept_names.is_empty() && kept_until > now_s() {
        (kept_names, kept_until)
    } else {
        match get(&rb().client, DISCOVERY, &[]).await.map(|b| parse_servers(&b)) {
            Ok(names) if !names.is_empty() => {
                let until = now_s() + DAY_S;
                if let Some(path) = servers_file() {
                    let body = json!({ "until": until, "names": names }).to_string();
                    let _ = super::store::write_atomic(&path, body.as_bytes());
                }
                crate::aelog!("[CATALOG] mirrors: {}", names.join(", "));
                (names, until)
            }
            other => {
                let why = match other {
                    Ok(_) => "an empty list".to_string(),
                    Err(Fail::Next(e)) | Err(Fail::Stop(e)) => e,
                };
                crate::aelog!("[CATALOG] the list of mirrors did not come ({}): the kept one serves", why);
                let names = if kept_names.is_empty() { FALLBACK.iter().map(|s| s.to_string()).collect() } else { kept_names };
                // Asked again in an hour, not at every search.
                (names, now_s() + 3600)
            }
        }
    };
    // Spread over the mirrors: each listener starts at one of them.
    let at = if names.len() > 1 { (rand::random::<u32>() as usize) % names.len() } else { 0 };
    *rb().mirrors.lock().unwrap() = Some(Mirrors { names: names.clone(), at, until });
    (names, at)
}

/// GET `path` with `query` from the first mirror that answers.
async fn ask(path: &str, query: Vec<(&'static str, String)>) -> Result<String, String> {
    let (names, start) = mirrors().await;
    let client = rb().client.clone();
    let (at, body) = first_answer(&names, start, |server| {
        let client = client.clone();
        let url = format!("https://{}{}", server, path);
        let query = query.clone();
        async move { get(&client, &url, &query).await }
    })
    .await?;
    if let Some(m) = rb().mirrors.lock().unwrap().as_mut() {
        m.at = at;
    }
    Ok(body)
}

/// One page of a search out of the pages of its asks (`Query::asks`, all at
/// one offset): their stations once each, in turn; the catalog's count the
/// largest of theirs (they step on together), more while any may have more.
/// No asks: an empty page.
pub(super) fn merge_pages(pages: Vec<Value>) -> Value {
    let mut seen = std::collections::HashSet::new();
    let mut stations = Vec::new();
    let (mut count, mut more) = (0u64, false);
    for p in pages {
        count = count.max(p["count"].as_u64().unwrap_or(0));
        more |= p["more"].as_bool().unwrap_or(false);
        for st in p["stations"].as_array().cloned().unwrap_or_default() {
            let key = match st["uuid"].as_str() {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => format!("url:{}", st["url"].as_str().unwrap_or("")),
            };
            if seen.insert(key) {
                stations.push(st);
            }
        }
    }
    json!({ "stations": stations, "count": count, "more": more, "formatsRev": rules::FORMATS_REV })
}

/// One page of a search.
pub async fn search(q: Query) -> Result<Value, String> {
    let asks = q.asks();
    let key = format!("{:?}", asks);
    {
        let cache = rb().searches.lock().unwrap();
        if let Some((t, v)) = cache.get(&key) {
            if t.elapsed() < SEARCH_TTL {
                return Ok(v.clone());
            }
        }
    }
    let mut pages = Vec::new();
    for params in asks {
        let body = ask("/json/stations/search", params).await?;
        pages.push(parse_search(&body, q.page())?);
    }
    let v = merge_pages(pages);
    let mut cache = rb().searches.lock().unwrap();
    cache.retain(|_, (t, _)| t.elapsed() < SEARCH_TTL);
    if cache.len() >= SEARCH_KEEP {
        if let Some(oldest) = cache.iter().min_by_key(|(_, (t, _))| *t).map(|(k, _)| k.clone()) {
            cache.remove(&oldest);
        }
    }
    cache.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

/// Genres, countries and languages: asked once a day, kept on disk; an old
/// copy serves when the catalog does not answer.
pub async fn lists() -> Result<Value, String> {
    let path = dir().map(|d| d.join("lists.json"));
    let kept = read_json(path.clone());
    if let Some(k) = kept.as_ref().filter(|k| num(k, "until") as u64 > now_s()) {
        return Ok(k.clone());
    }
    let fresh = async {
        let common = || vec![("hidebroken", "true".to_string()), ("order", "stationcount".to_string()), ("reverse", "true".to_string())];
        let tags = ask("/json/tags", [common(), vec![("limit", "400".to_string())]].concat()).await?;
        let countries = ask("/json/countries", common()).await?;
        let languages = ask("/json/languages", common()).await?;
        parse_lists(&tags, &countries, &languages)
    }
    .await;
    match fresh {
        Ok(mut v) => {
            v["until"] = json!(now_s() + DAY_S);
            if let Some(p) = path {
                let _ = super::store::write_atomic(&p, v.to_string().as_bytes());
            }
            Ok(v)
        }
        Err(e) => kept.ok_or(e),
    }
}

/// The address to play a catalog station at, asked of the catalog as each
/// start of it (it counts the listener).
pub async fn station_url(uuid: String) -> Result<String, String> {
    if !is_uuid(&uuid) {
        return Err(format!("not a station: {}", uuid));
    }
    let body = ask(&format!("/json/url/{}", uuid), vec![]).await?;
    parse_station_url(&body).ok_or_else(|| "the catalog gave no address for the station".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as Radio Browser sends it (trimmed of what is not read).
    fn record(name: &str, url: &str, codec: &str, hls: u8) -> Value {
        json!({
            "changeuuid": "d79de2ce-d834-4fd4-9e5f-1bb76d717014",
            "stationuuid": "9617a958-0601-11e8-ae97-52543be04c81",
            "name": name,
            "url": url,
            "url_resolved": url,
            "homepage": "https://radioparadise.com/",
            "favicon": "https://radioparadise.com/apple-touch-icon.png",
            "tags": "california,eclectic,free,internet,non-commercial,paradise,radio",
            "country": "The United States Of America",
            "countrycode": "US",
            "language": "english",
            "votes": 318132,
            "codec": codec,
            "bitrate": 320,
            "hls": hls,
            "lastcheckok": 1,
            "clickcount": 461,
        })
    }

    #[test]
    fn a_record_becomes_a_row() {
        let s = station(&record(" Radio Paradise Main Mix (EU) 320k AAC ", "http://stream-uk1.radioparadise.com/aac-320", "AAC", 0)).unwrap();
        assert_eq!(s["name"], "Radio Paradise Main Mix (EU) 320k AAC");
        assert_eq!(s["uuid"], "9617a958-0601-11e8-ae97-52543be04c81");
        assert_eq!(s["url"], "http://stream-uk1.radioparadise.com/aac-320");
        assert_eq!(s["codec"], "AAC");
        assert_eq!(s["bitrate"], 320);
        assert_eq!((s["votes"].clone(), s["clickcount"].clone()), (json!(318132), json!(461)), "what the lists sort by");
        assert_eq!(s["plays"], true);
        assert_eq!(s["countrycode"], "US");
        assert_eq!(s["tags"], json!(["california", "eclectic", "free", "internet", "non-commercial"]));
        // HE-AAC plays, in HLS too; HLS plays as its codec does; an UNKNOWN codec is tried and shows no name.
        assert_eq!(station(&record("x", "http://a.example/s", "AAC+", 0)).unwrap()["plays"], true);
        assert_eq!(station(&record("x", "http://a.example/s", "AAC+", 1)).unwrap()["plays"], true);
        assert_eq!(station(&record("x", "http://a.example/s", "MP3", 1)).unwrap()["plays"], true);
        let u = station(&record("x", "http://a.example/s", "UNKNOWN", 0)).unwrap();
        assert_eq!((u["plays"].clone(), u["codec"].clone()), (json!(true), json!("")));
        // Not listed: video, a forbidden service, no address, no name.
        assert!(station(&record("x", "http://a.example/s", "AAC,H.264", 0)).is_none());
        assert!(station(&record("Groove Salad", "https://ice2.somafm.com/groovesalad-128-mp3", "MP3", 0)).is_none());
        assert!(station(&record("x", "ftp://a.example/s", "MP3", 0)).is_none());
        assert!(station(&record(" ", "http://a.example/s", "MP3", 0)).is_none());
    }

    #[test]
    fn a_record_with_odd_fields_still_reads() {
        // The resolved address preferred, the given one when it is empty;
        // numbers as text; fields missing.
        let v = json!({ "name": "N", "url": "http://given.example/s", "url_resolved": "", "bitrate": "128", "hls": "0", "votes": null });
        let s = station(&v).unwrap();
        assert_eq!((s["url"].clone(), s["bitrate"].clone(), s["votes"].clone()), (json!("http://given.example/s"), json!(128), json!(0)));
        assert_eq!(s["favicon"], "");
        // A non-http icon is no icon.
        let v = json!({ "name": "N", "url": "http://a.example/s", "favicon": "data:image/png;base64,AAAA" });
        assert_eq!(station(&v).unwrap()["favicon"], "");
    }

    #[test]
    fn a_page_says_whether_more_may_follow() {
        let page: Vec<Value> = (0..PAGE).map(|i| record(&format!("S{i}"), "http://a.example/s", "MP3", 0)).collect();
        let body = serde_json::to_string(&page).unwrap();
        let v = parse_search(&body, PAGE).unwrap();
        assert_eq!((v["stations"].as_array().unwrap().len(), v["more"].clone()), (PAGE as usize, json!(true)));
        // The same 50 on a page of a search by name: all there is.
        assert_eq!(parse_search(&body, NAME_PAGE).unwrap()["more"], json!(false));
        // A short page is the last; what is not listed here still counted
        // toward the page.
        let short = json!([record("A", "http://a.example/s", "MP3", 0), record("B", "http://somafm.com/x", "MP3", 0)]);
        let v = parse_search(&short.to_string(), PAGE).unwrap();
        assert_eq!((v["stations"].as_array().unwrap().len(), v["more"].clone()), (1, json!(false)));
        assert_eq!(v["count"], 2, "the next page starts after both");
        assert_eq!(v["formatsRev"], rules::FORMATS_REV);
        assert!(parse_search("<html>busy</html>", PAGE).is_err());
    }

    #[test]
    fn the_search_asks_what_was_chosen() {
        let q = Query { name: " jazz ".into(), tag: "smooth jazz".into(), country: "ch".into(), order: "clickcount".into(), offset: 50, ..Query::default() };
        let p = q.params();
        let has = |k: &str, v: &str| p.iter().any(|(pk, pv)| *pk == k && pv == v);
        assert!(has("name", "jazz") && has("tag", "smooth jazz") && has("tagExact", "true") && has("countrycode", "CH"));
        assert!(has("order", "clickcount") && has("reverse", "true") && has("hidebroken", "true"));
        assert!(has("limit", &NAME_PAGE.to_string()) && has("offset", "50"), "by name: every match at once");
        assert!(!p.iter().any(|(k, _)| *k == "language" || *k == "codec" || *k == "languageExact"));
        // The order the page lists in, asked of the catalog; anything else by
        // name (A to Z); without a name, a page of 50.
        let ordered = |order: &str| {
            let p = Query { order: order.into(), name: "  ".into(), ..Query::default() }.params();
            let get = |k: &str| p.iter().find(|(pk, _)| *pk == k).map(|(_, v)| v.clone()).unwrap_or_default();
            (get("order"), get("reverse"), get("limit"))
        };
        assert_eq!(ordered("votes"), ("votes".into(), "true".into(), PAGE.to_string()));
        assert_eq!(ordered("quality"), ("bitrate".into(), "true".into(), PAGE.to_string()));
        assert_eq!(ordered("name"), ("name".into(), "false".into(), PAGE.to_string()));
        assert_eq!(ordered("random"), ("name".into(), "false".into(), PAGE.to_string()));
    }

    /// The quality is asked of the catalog: a bitrate at least, a lossless
    /// station whatever its bitrate, one of unknown bitrate (0) not.
    #[test]
    fn the_quality_is_asked_of_the_catalog() {
        type Ask = Vec<(&'static str, String)>;
        let has = |p: &Ask, k: &str, v: &str| p.iter().any(|(pk, pv)| *pk == k && pv == v);
        let any = |p: &Ask, k: &str| p.iter().any(|(pk, _)| *pk == k);
        let asks = |quality: &str, codec: &str| Query { quality: quality.into(), codec: codec.into(), ..Query::default() }.asks();
        let a = asks("", "");
        assert!(a.len() == 1 && !any(&a[0], "bitrateMin") && !any(&a[0], "codec"), "any quality: as it was");
        assert_eq!(asks("best", "").len(), 1, "an unknown quality is any");
        // Lossless: FLAC, and FLAC in Ogg (listed as Ogg, past 600 kbps).
        let a = asks("lossless", "");
        assert_eq!(a.len(), 2);
        assert!(has(&a[0], "codec", "FLAC") && !any(&a[0], "bitrateMin"));
        assert!(has(&a[1], "codec", "OGG") && has(&a[1], "bitrateMin", "600"));
        let a = asks("Lossless", "flac");
        assert!(a.len() == 1 && has(&a[0], "codec", "FLAC"));
        let a = asks("lossless", "OGG");
        assert!(a.len() == 1 && has(&a[0], "codec", "OGG") && has(&a[0], "bitrateMin", "600"));
        assert!(asks("lossless", "MP3").is_empty(), "MP3 is never lossless: nothing to ask");
        // A bitrate at least, the FLAC stations beside it (a lossless Ogg one
        // is past it already).
        let a = asks("320", "");
        assert_eq!(a.len(), 2);
        assert!(has(&a[0], "bitrateMin", "320") && !any(&a[0], "codec"));
        assert!(has(&a[1], "codec", "FLAC") && !any(&a[1], "bitrateMin"));
        let a = asks("192", "MP3");
        assert!(a.len() == 1 && has(&a[0], "bitrateMin", "192") && has(&a[0], "codec", "MP3"));
        let a = asks("128", "FLAC");
        assert!(a.len() == 1 && has(&a[0], "codec", "FLAC") && !any(&a[0], "bitrateMin"), "FLAC passes any bitrate");
        let a = asks("256", "OGG");
        assert!(a.len() == 1 && has(&a[0], "codec", "OGG") && has(&a[0], "bitrateMin", "256"));
        let a = asks("1000", "");
        assert_eq!(a.len(), 3, "past 600 kbps the lossless Ogg stations are asked on their own");
        assert!(has(&a[2], "codec", "OGG") && has(&a[2], "bitrateMin", "600"));
        // Best quality first: the FLAC stations beside (their bitrate is
        // often 0, the last of a catalog's page by bitrate).
        let best = |codec: &str| Query { order: "quality".into(), codec: codec.into(), ..Query::default() }.asks();
        let a = best("");
        assert_eq!(a.len(), 2);
        assert!(has(&a[0], "order", "bitrate") && !any(&a[0], "codec"));
        assert!(has(&a[1], "order", "bitrate") && has(&a[1], "codec", "FLAC"));
        assert_eq!(best("MP3").len(), 1);
        // The other choices go along with each ask.
        let a = Query { quality: "256".into(), tag: "jazz".into(), name: "fm".into(), ..Query::default() }.asks();
        assert!(a.iter().all(|p| has(p, "tag", "jazz") && has(p, "name", "fm") && has(p, "limit", &NAME_PAGE.to_string())));
    }

    #[test]
    fn a_page_of_two_asks_lists_each_station_once() {
        let rec = |name: &str, uuid: &str, codec: &str| {
            let mut r = record(name, &format!("http://a.example/{uuid}"), codec, 0);
            r["stationuuid"] = json!(uuid);
            r
        };
        let by_rate = parse_search(&json!([rec("A", "u-a", "MP3"), rec("B", "u-b", "FLAC")]).to_string(), 2).unwrap();
        let flac = parse_search(&json!([rec("B", "u-b", "FLAC"), rec("C", "u-c", "FLAC"), rec("C2", "", "FLAC")]).to_string(), 3).unwrap();
        let v = merge_pages(vec![by_rate, flac]);
        let names: Vec<&str> = v["stations"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["A", "B", "C", "C2"]);
        assert_eq!((v["count"].clone(), v["more"].clone()), (json!(3), json!(true)), "the next page: after the longest");
        let none = merge_pages(Vec::new());
        assert_eq!((none["stations"].as_array().unwrap().len(), none["more"].clone()), (0, json!(false)));
        assert_eq!(none["formatsRev"], rules::FORMATS_REV);
    }

    #[test]
    fn the_lists_to_choose_from() {
        let tags = r#"[{"name":"pop","stationcount":5000},{"name":"Jazz","stationcount":900},{"name":"","stationcount":3},{"name":"empty","stationcount":0}]"#;
        let countries = r#"[{"name":"Switzerland","iso_3166_1":"CH","stationcount":400},{"name":"Austria","iso_3166_1":"at","stationcount":300},{"name":"Nowhere","iso_3166_1":"","stationcount":1}]"#;
        let languages = r#"[{"name":"english","stationcount":9000},{"name":"german","stationcount":3000}]"#;
        let v = parse_lists(tags, countries, languages).unwrap();
        assert_eq!(v["tags"], json!([{ "name": "pop", "count": 5000 }, { "name": "jazz", "count": 900 }]));
        assert_eq!(v["countries"], json!([{ "code": "AT", "name": "Austria", "count": 300 }, { "code": "CH", "name": "Switzerland", "count": 400 }]));
        assert_eq!(v["languages"][0]["name"], "english");
        assert!(parse_lists("[]", "nope", "[]").is_err());
    }

    #[test]
    fn the_mirrors_and_a_stations_address() {
        let body = r#"[{"ip":"91.98.4.78","name":"de1.api.radio-browser.info"},{"ip":"2a01:4f8::1","name":"DE1.api.radio-browser.info"},{"ip":"1.2.3.4","name":"fi1.api.radio-browser.info"},{"name":"bad host/x"}]"#;
        assert_eq!(parse_servers(body), vec!["de1.api.radio-browser.info", "fi1.api.radio-browser.info"]);
        assert!(parse_servers("garbage").is_empty());
        let ok = r#"{"ok":true,"message":"retrieved station url","stationuuid":"9617a958-0601-11e8-ae97-52543be04c81","name":"RP","url":"http://stream-uk1.radioparadise.com/aac-320"}"#;
        assert_eq!(parse_station_url(ok).as_deref(), Some("http://stream-uk1.radioparadise.com/aac-320"));
        assert_eq!(parse_station_url(r#"{"ok":false,"url":""}"#), None);
        assert!(is_uuid("9617a958-0601-11e8-ae97-52543be04c81"));
        assert!(!is_uuid("../../etc/passwd") && !is_uuid(""));
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn a_mirror_that_fails_gives_way_to_the_next() {
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // From the second: it fails, the third answers.
        let asked = std::sync::Mutex::new(Vec::new());
        let r = block_on(first_answer(&names, 1, |n| {
            asked.lock().unwrap().push(n.clone());
            async move { if n == "b" { Err(Fail::Next("timed out".into())) } else { Ok(format!("from {n}")) } }
        }));
        assert_eq!(r, Ok((2, "from c".to_string())));
        assert_eq!(*asked.lock().unwrap(), ["b", "c"]);
        // Round the list: all fail — the last failure is the answer.
        let r: Result<(usize, String), String> =
            block_on(first_answer(&names, 2, |n| async move { Err(Fail::Next(format!("{n} down"))) }));
        assert_eq!(r, Err("b down".to_string()));
        // A request refused as such is not asked of the others.
        let asked = std::sync::Mutex::new(0);
        let r: Result<(usize, String), String> = block_on(first_answer(&names, 0, |_| {
            *asked.lock().unwrap() += 1;
            async { Err(Fail::Stop("HTTP 400".into())) }
        }));
        assert_eq!((r, *asked.lock().unwrap()), (Err("HTTP 400".to_string()), 1));
        // No mirror known.
        let r: Result<(usize, String), String> = block_on(first_answer(&[], 0, |_| async { Ok(String::new()) }));
        assert!(r.is_err());
    }
}
