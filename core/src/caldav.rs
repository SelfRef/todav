//! CalDAV over blocking HTTP: discovery, sync-collection, PUT/DELETE, plus a tiny XML tree.

use crate::{Error, Result};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CS: &str = "http://calendarserver.org/ns/";
pub const APPLE: &str = "http://apple.com/ns/ical/";
pub const PUSH: &str = "https://bitfire.at/webdav-push";

#[derive(Debug, Default, Clone)]
pub struct Element {
    pub ns: String,
    pub name: String,
    pub text: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn parse(xml: &str) -> Result<Element> {
        let mut r = quick_xml::NsReader::from_str(xml);
        let mut stack = vec![Element::default()];
        loop {
            let (ns, ev) = r
                .read_resolved_event()
                .map_err(|e| Error::Parse(e.to_string()))?;
            let ns = match ns {
                ResolveResult::Bound(n) => n.as_ref().to_string(),
                _ => String::new(),
            };
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let attrs = e
                        .attributes()
                        .flatten()
                        .map(|a| {
                            let v = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .map(|v| v.into_owned())
                                .unwrap_or_default();
                            (a.key.local_name().as_ref().to_string(), v)
                        })
                        .collect();
                    let el = Element {
                        ns,
                        name: e.local_name().as_ref().to_string(),
                        attrs,
                        ..Default::default()
                    };
                    stack.push(el);
                    if matches!(ev, Event::Empty(_)) {
                        let el = stack.pop().unwrap();
                        stack.last_mut().unwrap().children.push(el);
                    }
                }
                Event::End(_) => {
                    let el = stack.pop().unwrap();
                    stack
                        .last_mut()
                        .ok_or_else(|| Error::Parse("unbalanced xml".into()))?
                        .children
                        .push(el);
                }
                Event::Text(t) => stack.last_mut().unwrap().text += &*t,
                Event::CData(t) => stack.last_mut().unwrap().text += &*t,
                Event::GeneralRef(g) => {
                    let ent = format!("&{};", &*g);
                    let s = quick_xml::escape::unescape(&ent)
                        .map_err(|e| Error::Parse(e.to_string()))?;
                    stack.last_mut().unwrap().text += &s;
                }
                Event::Eof => break,
                _ => {}
            }
        }
        stack
            .pop()
            .and_then(|mut root| root.children.pop())
            .ok_or_else(|| Error::Parse("empty xml".into()))
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.ns == ns && c.name == name)
    }

    pub fn children<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children
            .iter()
            .filter(move |c| c.ns == ns && c.name == name)
    }

    /// Depth-first search for the first descendant with this name.
    pub fn find(&self, ns: &str, name: &str) -> Option<&Element> {
        self.children.iter().find_map(|c| {
            if c.ns == ns && c.name == name {
                Some(c)
            } else {
                c.find(ns, name)
            }
        })
    }

    pub fn find_text(&self, ns: &str, name: &str) -> Option<String> {
        self.find(ns, name)
            .map(|e| e.text.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// One `<d:response>` of a multistatus: href, HTTP status, and the merged 200 `<d:prop>`.
#[derive(Debug)]
pub struct DavResponse {
    pub href: String,
    pub status: u16,
    pub props: Element,
}

pub fn multistatus(xml: &str) -> Result<(Vec<DavResponse>, Option<String>)> {
    let root = Element::parse(xml)?;
    let status_code = |e: &Element| {
        e.child(DAV, "status")
            .and_then(|s| s.text.split_whitespace().nth(1)?.parse().ok())
    };
    let mut out = Vec::new();
    for r in root.children(DAV, "response") {
        let href = r
            .child(DAV, "href")
            .map(|h| h.text.trim().to_string())
            .unwrap_or_default();
        let mut props = Element::default();
        for ps in r.children(DAV, "propstat") {
            if status_code(ps) == Some(200)
                && let Some(p) = ps.children.iter().find(|c| c.name == "prop")
            {
                props.children.extend(p.children.iter().cloned());
            }
        }
        let status = status_code(r).unwrap_or(200);
        out.push(DavResponse {
            href: percent_decode(&href),
            status,
            props,
        });
    }
    let token = root
        .child(DAV, "sync-token")
        .map(|t| t.text.trim().to_string());
    Ok((out, token))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_path(s: &str) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"/-._~@!$&'()*+,;=:".contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

pub struct HttpResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub location: Option<String>,
    pub body: String,
}

pub struct Dav {
    agent: ureq::Agent,
    origin: String,
    root: String,
    auth: String,
}

#[derive(Debug, Clone)]
pub struct RemoteList {
    pub href: String,
    pub display_name: String,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub push_topic: Option<String>,
    /// VAPID key advertised by the server; Some only when web-push is supported.
    pub push_vapid: Option<String>,
}

pub enum SyncResult {
    Changes {
        token: String,
        /// (href, etag, calendar data)
        changed: Vec<(String, String, String)>,
        removed: Vec<String>,
    },
    InvalidToken,
}

impl Dav {
    /// `url` is the Nextcloud base (https://cloud.example) or a full DAV root.
    pub fn new(url: &str, user: &str, password: &str) -> Dav {
        let url = url.trim_end_matches('/');
        let root = if url.contains("/remote.php/") {
            format!("{url}/")
        } else {
            format!("{url}/remote.php/dav/")
        };
        let origin = url.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        let agent = ureq::Agent::config_builder()
            .allow_non_standard_methods(true)
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();
        Dav {
            agent,
            origin,
            root,
            auth: format!(
                "Basic {}",
                crate::b64encode(format!("{user}:{password}").as_bytes(), false)
            ),
        }
    }

    pub fn url(&self, href: &str) -> String {
        if href.starts_with("http") {
            href.to_string()
        } else {
            format!("{}{}", self.origin, percent_encode_path(href))
        }
    }

    pub fn request(
        &self,
        method: &str,
        href: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse> {
        let mut b = ureq::http::Request::builder()
            .method(method)
            .uri(self.url(href))
            .header("Authorization", &self.auth);
        if !body.is_empty()
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            b = b.header("Content-Type", "application/xml; charset=utf-8");
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(body.to_string())
            .map_err(|e| Error::Net(e.to_string()))?;
        let mut resp = self.agent.run(req).map_err(|e| Error::Net(e.to_string()))?;
        let header = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (etag, location) = (header("etag"), header("location"));
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Net(e.to_string()))?;
        if status == 401 {
            return Err(Error::Auth);
        }
        Ok(HttpResponse {
            status,
            etag,
            location,
            body,
        })
    }

    fn propfind(&self, href: &str, depth: &str, props: &str) -> Result<Vec<DavResponse>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="{CALDAV}" xmlns:cs="{CS}" xmlns:a="{APPLE}"><d:prop>{props}</d:prop></d:propfind>"#
        );
        let r = self.request("PROPFIND", href, &[("Depth", depth)], &body)?;
        if r.status != 207 {
            return Err(Error::Http(r.status, r.body));
        }
        Ok(multistatus(&r.body)?.0)
    }

    pub fn discover(&self) -> Result<Vec<RemoteList>> {
        let root = self.root.clone();
        let p = self.propfind(&root, "0", "<d:current-user-principal/>")?;
        let principal = p
            .first()
            .and_then(|r| r.props.find_text(DAV, "href"))
            .ok_or_else(|| Error::Parse("no current-user-principal".into()))?;
        let p = self.propfind(&principal, "0", "<c:calendar-home-set/>")?;
        let home = p
            .first()
            .and_then(|r| r.props.find_text(DAV, "href"))
            .ok_or_else(|| Error::Parse("no calendar-home-set".into()))?;
        let props = format!(
            r#"<d:resourcetype/><d:displayname/><d:sync-token/><cs:getctag/><a:calendar-color/><c:supported-calendar-component-set/><p:transports xmlns:p="{PUSH}"/><p:topic xmlns:p="{PUSH}"/>"#
        );
        let mut lists = Vec::new();
        for r in self.propfind(&home, "1", &props)? {
            let p = &r.props;
            let is_cal = p
                .child(DAV, "resourcetype")
                .is_some_and(|t| t.child(CALDAV, "calendar").is_some());
            let todo = p
                .child(CALDAV, "supported-calendar-component-set")
                .is_some_and(|s| s.children.iter().any(|c| c.attr("name") == Some("VTODO")));
            if !is_cal || !todo {
                continue;
            }
            lists.push(RemoteList {
                display_name: p
                    .find_text(DAV, "displayname")
                    .unwrap_or_else(|| r.href.clone()),
                color: p.find_text(APPLE, "calendar-color"),
                ctag: p.find_text(CS, "getctag"),
                sync_token: p.find_text(DAV, "sync-token"),
                push_topic: p.find_text(PUSH, "topic"),
                push_vapid: p
                    .child(PUSH, "transports")
                    .and_then(|t| t.child(PUSH, "web-push"))
                    .map(|w| w.find_text(PUSH, "vapid-public-key").unwrap_or_default()),
                href: r.href,
            });
        }
        Ok(lists)
    }

    /// RFC 6578 sync-collection; an empty token returns every member.
    pub fn sync_collection(&self, href: &str, token: &str) -> Result<SyncResult> {
        let token = xml_escape(token);
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:sync-collection xmlns:d="DAV:" xmlns:c="{CALDAV}"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/><c:calendar-data/></d:prop></d:sync-collection>"#
        );
        // TODO(v2): servers that omit calendar-data here need a calendar-multiget pass.
        let r = self.request("REPORT", href, &[("Depth", "1")], &body)?;
        if matches!(r.status, 403 | 409) && r.body.contains("valid-sync-token") {
            return Ok(SyncResult::InvalidToken);
        }
        if r.status != 207 {
            return Err(Error::Http(r.status, r.body));
        }
        let (resps, token) = multistatus(&r.body)?;
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        for resp in resps {
            if resp.href.trim_end_matches('/') == href.trim_end_matches('/') {
                continue;
            }
            if resp.status == 404 {
                removed.push(resp.href);
            } else if let (Some(etag), Some(data)) = (
                resp.props.find_text(DAV, "getetag"),
                resp.props.find(CALDAV, "calendar-data"),
            ) {
                changed.push((resp.href, etag, data.text.clone()));
            }
        }
        Ok(SyncResult::Changes {
            token: token.unwrap_or_default(),
            changed,
            removed,
        })
    }

    /// PUT with `If-Match` (update) or `If-None-Match: *` (create). Returns the new ETag if the server sent one.
    /// `dont_notify`: our own push registration URL, so the server does not echo this change back to us.
    pub fn put(
        &self,
        href: &str,
        ics: &str,
        etag: Option<&str>,
        dont_notify: Option<&str>,
    ) -> Result<Option<String>> {
        let mut h = vec![("Content-Type", "text/calendar; charset=utf-8".to_string())];
        h.push(match etag {
            Some(e) => ("If-Match", e.to_string()),
            None => ("If-None-Match", "*".to_string()),
        });
        h.extend(dont_notify.map(|r| ("Push-Dont-Notify", format!("\"{r}\""))));
        let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = self.request("PUT", href, &h, ics)?;
        match r.status {
            200..=299 => Ok(r.etag),
            412 => Err(Error::Conflict),
            s => Err(Error::Http(s, r.body)),
        }
    }

    pub fn delete(&self, href: &str, etag: Option<&str>, dont_notify: Option<&str>) -> Result<()> {
        let mut h: Vec<(&str, String)> = etag
            .map(|e| ("If-Match", e.to_string()))
            .into_iter()
            .collect();
        h.extend(dont_notify.map(|r| ("Push-Dont-Notify", format!("\"{r}\""))));
        let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = self.request("DELETE", href, &h, "")?;
        match r.status {
            200..=299 | 404 => Ok(()),
            412 => Err(Error::Conflict),
            s => Err(Error::Http(s, r.body)),
        }
    }

    /// GET a resource: (etag, body), or None if gone.
    pub fn get(&self, href: &str) -> Result<Option<(Option<String>, String)>> {
        let r = self.request("GET", href, &[], "")?;
        match r.status {
            200 => Ok(Some((r.etag, r.body))),
            404 => Ok(None),
            s => Err(Error::Http(s, r.body)),
        }
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multistatus() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:x1="https://bitfire.at/webdav-push">
 <d:response><d:href>/remote.php/dav/calendars/Test/zakupy%20dom/</d:href>
  <d:propstat><d:prop><d:displayname>Zakupy &amp; dom</d:displayname>
   <cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set>
   <x1:transports><x1:web-push><x1:vapid-public-key>BA1H</x1:vapid-public-key></x1:web-push></x1:transports>
   <x1:topic>calendar-12</x1:topic></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getctag/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/x/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>
 <d:sync-token>http://sabre.io/ns/sync/42</d:sync-token>
</d:multistatus>"#;
        let (r, token) = multistatus(xml).unwrap();
        assert_eq!(token.as_deref(), Some("http://sabre.io/ns/sync/42"));
        assert_eq!(r[0].href, "/remote.php/dav/calendars/Test/zakupy dom/");
        assert_eq!(
            r[0].props.find_text(DAV, "displayname").unwrap(),
            "Zakupy & dom"
        );
        assert!(r[0].props.child(CS, "getctag").is_none());
        assert_eq!(
            r[0].props.find(CALDAV, "comp").unwrap().attr("name"),
            Some("VTODO")
        );
        assert_eq!(
            r[0].props.find_text(PUSH, "vapid-public-key").unwrap(),
            "BA1H"
        );
        assert_eq!((r[1].href.as_str(), r[1].status), ("/x/gone.ics", 404));
    }

    #[test]
    fn helpers() {
        assert_eq!(crate::b64encode(b"Test:6*U", false), "VGVzdDo2KlU=");
        assert_eq!(crate::b64encode(b"ab", false), "YWI=");
        assert_eq!(crate::b64decode("YWI=").unwrap(), b"ab");
        assert_eq!(crate::b64decode("_-8").unwrap(), [0xff, 0xef]);
        assert_eq!(percent_encode_path("/a b/ż.ics"), "/a%20b/%C5%BC.ics");
        assert_eq!(percent_decode("/a%20b/%C5%BC.ics"), "/a b/ż.ics");
    }
}
