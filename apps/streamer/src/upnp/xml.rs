//! Just enough XML for UPnP: escaping what goes out, and reading the flat
//! elements a SOAP call or a DIDL-Lite item carries. Control points send
//! simple documents; a full parser would buy nothing here.

/// Text made safe inside an element or an attribute.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The text of escaped XML as it was: the five names and character numbers.
pub fn unesc(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let n = if let Some(h) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else {
                    name.strip_prefix('#').and_then(|d| d.parse::<u32>().ok())
                };
                n.and_then(char::from_u32)
            }
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An element's name without its namespace prefix.
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// One element found in a document: its local name, its attributes as
/// written, and its content (inner XML, not unescaped).
#[derive(Debug, Clone)]
pub struct Element<'a> {
    pub name: &'a str,
    pub attrs: &'a str,
    pub inner: &'a str,
}

impl<'a> Element<'a> {
    /// The text inside: CDATA as it is, the rest unescaped.
    pub fn text(&self) -> String {
        let t = self.inner.trim();
        if let Some(c) = t.strip_prefix("<![CDATA[").and_then(|c| c.strip_suffix("]]>")) {
            return c.to_string();
        }
        unesc(self.inner)
    }

    /// An attribute's value, unescaped (by its local name).
    pub fn attr(&self, name: &str) -> Option<String> {
        let mut rest = self.attrs;
        loop {
            let eq = rest.find('=')?;
            let key = local(rest[..eq].trim());
            let after = rest[eq + 1..].trim_start();
            let q = after.chars().next()?;
            if q != '"' && q != '\'' {
                return None;
            }
            let close = after[1..].find(q)? + 1;
            if key == name {
                return Some(unesc(&after[1..close]));
            }
            rest = &after[close + 1..];
        }
    }
}

/// The elements directly inside `doc` (the content of one element), in order.
pub fn children(doc: &str) -> Vec<Element<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(open) = doc[pos..].find('<').map(|i| i + pos) {
        let after = &doc[open + 1..];
        if after.starts_with('?') || after.starts_with('!') {
            // A declaration, a comment or a CDATA section between elements.
            let end = if after.starts_with("!--") {
                doc[open..].find("-->").map(|i| open + i + 3)
            } else if after.starts_with("![CDATA[") {
                doc[open..].find("]]>").map(|i| open + i + 3)
            } else {
                doc[open..].find('>').map(|i| open + i + 1)
            };
            let Some(end) = end else { break };
            pos = end;
            continue;
        }
        if after.starts_with('/') {
            // A closing tag of the parent: the content ends.
            break;
        }
        let Some(gt) = doc[open..].find('>').map(|i| open + i) else { break };
        let tag = &doc[open + 1..gt];
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (qname, attrs) = tag.split_once(|c: char| c.is_whitespace()).unwrap_or((tag, ""));
        if self_closing {
            out.push(Element { name: local(qname), attrs, inner: "" });
            pos = gt + 1;
            continue;
        }
        let Some(close) = matching_close(doc, gt + 1, qname) else { break };
        out.push(Element { name: local(qname), attrs, inner: &doc[gt + 1..close.0] });
        pos = close.1;
    }
    out
}

/// Where the element `qname` opened before `from` closes: (start of its
/// closing tag, the position after it). Nested elements of the same name
/// are counted.
fn matching_close(doc: &str, from: usize, qname: &str) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut pos = from;
    let open_pat = format!("<{qname}");
    let close_pat = format!("</{qname}");
    loop {
        let next_close = doc[pos..].find(&close_pat).map(|i| i + pos)?;
        // Opening tags of the same name before it, not counting CDATA.
        let mut scan = pos;
        while let Some(o) = doc[scan..next_close].find(&open_pat).map(|i| i + scan) {
            let c = doc[o + open_pat.len()..].chars().next();
            let gt = doc[o..].find('>').map(|i| o + i);
            if matches!(c, Some('>' | '/' | ' ' | '\t' | '\r' | '\n')) && gt.is_some_and(|g| !doc[..g].ends_with('/')) {
                depth += 1;
            }
            scan = o + open_pat.len();
        }
        let end = doc[next_close..].find('>').map(|i| next_close + i + 1)?;
        depth -= 1;
        if depth == 0 {
            return Some((next_close, end));
        }
        pos = end;
    }
}

/// The first element named `name` anywhere in `doc`, looking down through
/// the first elements of each level as well as their siblings.
pub fn find<'a>(doc: &'a str, name: &str) -> Option<Element<'a>> {
    for e in children(doc) {
        if e.name == name {
            return Some(e);
        }
        if let Some(inner) = find(e.inner, name) {
            return Some(inner);
        }
    }
    None
}

/// A SOAP call: the action's name and its arguments, by name.
pub struct Call {
    pub action: String,
    pub args: Vec<(String, String)>,
}

impl Call {
    pub fn arg(&self, name: &str) -> &str {
        self.args.iter().find(|(n, _)| n == name).map_or("", |(_, v)| v.as_str())
    }
}

/// The action inside a SOAP envelope.
pub fn soap_call(body: &str) -> Option<Call> {
    let b = find(body, "Body")?;
    let action = children(b.inner).into_iter().next()?;
    let args = children(action.inner).into_iter().map(|a| (a.name.to_string(), a.text())).collect();
    Some(Call { action: action.name.to_string(), args })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_goes_out_escaped_and_comes_back_as_it_was() {
        let s = "a < b & \"c\" > 'd' ü";
        assert_eq!(unesc(&esc(s)), s);
        assert_eq!(unesc("&#65;&#x42;&amp;amp;&bogus; & x"), "AB&amp;&bogus; & x");
    }

    #[test]
    fn a_soap_call_is_read_with_its_arguments() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <InstanceID>0</InstanceID>
  <CurrentURI>http://192.168.1.5:52100/Music/a.flac?x=1&amp;y=2</CurrentURI>
  <CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;A &amp;amp; B&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
  <Empty/>
 </u:SetAVTransportURI></s:Body></s:Envelope>"#;
        let c = soap_call(body).unwrap();
        assert_eq!(c.action, "SetAVTransportURI");
        assert_eq!(c.arg("InstanceID"), "0");
        assert_eq!(c.arg("CurrentURI"), "http://192.168.1.5:52100/Music/a.flac?x=1&y=2");
        let didl = c.arg("CurrentURIMetaData");
        assert_eq!(find(didl, "title").unwrap().text(), "A & B");
        assert_eq!(c.arg("Empty"), "");
        assert_eq!(c.arg("Missing"), "");
    }

    #[test]
    fn cdata_and_attributes_are_read() {
        let doc = r#"<DIDL-Lite xmlns:dc="x"><item id="1" parentID='0'><dc:title><![CDATA[Tom & Jerry]]></dc:title><res duration="0:03:10.000" protocolInfo="http-get:*:audio/flac:*">http://h/a.flac</res></item></DIDL-Lite>"#;
        let item = find(doc, "item").unwrap();
        assert_eq!(item.attr("id").as_deref(), Some("1"));
        assert_eq!(item.attr("parentID").as_deref(), Some("0"));
        assert_eq!(find(item.inner, "title").unwrap().text(), "Tom & Jerry");
        let res = find(item.inner, "res").unwrap();
        assert_eq!(res.attr("duration").as_deref(), Some("0:03:10.000"));
        assert_eq!(res.text(), "http://h/a.flac");
    }

    #[test]
    fn nested_elements_of_one_name_are_counted() {
        let doc = "<a><a>1</a><b/></a><c>2</c>";
        let top = children(doc);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].inner, "<a>1</a><b/>");
        assert_eq!(top[1].text(), "2");
    }
}
