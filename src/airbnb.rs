//! Fetches Airbnb search result pages and extracts listings.
//!
//! Airbnb server-renders search pages with the StaysSearch GraphQL response
//! embedded in `<script type="application/json">` blocks (e.g.
//! `data-deferred-state-0`). Rather than depending on the exact nesting, which
//! changes often, we walk every JSON block and pick up any object that looks
//! like a stay search result.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine;
use serde_json::Value;
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

/// Query params that describe the page position rather than the search itself.
const PAGING_PARAMS: &[&str] = &[
    "cursor",
    "items_offset",
    "section_offset",
    "pagination_search",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    pub id: u64,
    pub title: String,
    pub name: String,
    pub details: String,
    pub price: String,
    pub rating: String,
    pub picture: Option<String>,
}

#[derive(Debug, Default)]
pub struct Page {
    pub listings: Vec<Listing>,
    /// `paginationInfo.nextPageCursor`, when Airbnb provides it.
    pub next_cursor: Option<String>,
    /// `paginationInfo.pageCursors`: one cursor per result page, first page first.
    pub page_cursors: Vec<String>,
}

impl Page {
    /// The cursor of the page after the one fetched with `current`.
    pub fn cursor_after(&self, current: Option<&str>) -> Option<String> {
        if let Some(c) = &self.next_cursor {
            return Some(c.clone());
        }
        let next = match current {
            None => 1,
            Some(cur) => self.page_cursors.iter().position(|c| c == cur)? + 1,
        };
        self.page_cursors.get(next).cloned()
    }
}

pub struct Fetcher {
    /// (label for logs, agent). One per proxy, or a single direct agent.
    agents: Vec<(String, ureq::Agent)>,
    current: AtomicUsize,
    /// Replaces scheme/host/port of search URLs when fetching (for tests).
    origin: Option<Url>,
    /// Base pause between result pages; a random 0–2s is added.
    page_delay: Duration,
}

impl Fetcher {
    pub fn new(proxies: &[String]) -> Result<Fetcher> {
        let builder = || {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(40))
                .redirects(8)
                .user_agent(USER_AGENT)
        };
        let mut agents = Vec::new();
        for p in proxies {
            validate_proxy(p)?;
            let proxy =
                ureq::Proxy::new(p).map_err(|e| anyhow!("bad proxy '{}': {e}", redact(p)))?;
            agents.push((redact(p), builder().proxy(proxy).build()));
        }
        if agents.is_empty() {
            agents.push(("direct".to_string(), builder().build()));
        }
        Ok(Fetcher {
            agents,
            current: AtomicUsize::new(0),
            origin: None,
            page_delay: Duration::from_millis(1500),
        })
    }

    /// Sends search requests to `origin` (e.g. `http://127.0.0.1:8080`) instead
    /// of Airbnb. Used by tests.
    pub fn with_origin(mut self, origin: &str) -> Result<Fetcher> {
        self.origin = Some(Url::parse(origin).map_err(|e| anyhow!("bad origin '{origin}': {e}"))?);
        Ok(self)
    }

    pub fn with_page_delay(mut self, delay: Duration) -> Fetcher {
        self.page_delay = delay;
        self
    }

    /// Politeness pause between requests (page delay plus up to 2s of jitter).
    pub fn pause(&self) {
        if !self.page_delay.is_zero() {
            std::thread::sleep(self.page_delay + Duration::from_millis(crate::util::jitter(2000)));
        }
    }

    /// Runs `f` with the current agent; on failure rotates to the next proxy
    /// and retries, trying each agent at most once.
    fn with_rotation<T>(&self, mut f: impl FnMut(&ureq::Agent) -> Result<T>) -> Result<T> {
        let n = self.agents.len();
        let mut last_err = None;
        for _ in 0..n {
            let idx = self.current.load(Ordering::Relaxed) % n;
            let (label, agent) = &self.agents[idx];
            match f(agent) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if n > 1 {
                        log::warn!("request via {label} failed: {e:#}; rotating proxy");
                    }
                    self.current.store((idx + 1) % n, Ordering::Relaxed);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no agents")))
    }

    fn get_html(agent: &ureq::Agent, url: &str) -> Result<(String, String)> {
        let resp = agent
            .get(url)
            .set(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .set("Accept-Language", "en-US,en;q=0.9")
            .set("Cache-Control", "no-cache")
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => anyhow!("HTTP {code} from Airbnb"),
                ureq::Error::Transport(t) => anyhow!("network error: {t}"),
            })?;
        let final_url = resp.get_url().to_string();
        let body = resp.into_string()?;
        Ok((final_url, body))
    }

    /// Turns whatever the user pasted (a search URL, an app share link, a
    /// short link) into a canonical Airbnb search URL.
    pub fn resolve(&self, raw: &str) -> Result<String> {
        let url = Url::parse(raw.trim()).map_err(|_| anyhow!("that doesn't look like a link"))?;
        if is_search_url(&url) {
            return Ok(normalize(&url));
        }
        let (final_url, body) = self.with_rotation(|a| Self::get_html(a, url.as_str()))?;
        if let Ok(u) = Url::parse(&final_url)
            && is_search_url(&u)
        {
            return Ok(normalize(&u));
        }
        if let Some(u) = find_search_url_in_text(&body) {
            return Ok(normalize(&u));
        }
        Err(anyhow!(
            "this isn't an Airbnb search link. Open Airbnb, set your filters, then share/copy the \
             link of the results page (it contains /s/…/homes)"
        ))
    }

    pub fn fetch_page(&self, search_url: &str, cursor: Option<&str>) -> Result<Page> {
        let url = page_url(search_url, cursor, self.origin.as_ref())?;
        let (_, html) = self.with_rotation(|a| {
            let (u, html) = Self::get_html(a, &url)?;
            // A page without search data usually means a captcha/block page,
            // so it is worth retrying via another proxy.
            if !html.contains("StaySearchResult") && !html.contains("staysSearch") {
                return Err(anyhow!(
                    "no search data in the page (blocked, captcha, or Airbnb changed its format)"
                ));
            }
            Ok((u, html))
        })?;
        parse_page(&html)
    }

    /// Fetches up to `max_pages` pages and returns all unique listings.
    pub fn fetch_all(&self, search_url: &str, max_pages: u32) -> Result<Vec<Listing>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for page_no in 0..max_pages {
            if page_no > 0 {
                self.pause();
            }
            let mut page = self.fetch_page(search_url, cursor.as_deref())?;
            let before = out.len();
            for l in std::mem::take(&mut page.listings) {
                if seen.insert(l.id) {
                    out.push(l);
                }
            }
            log::debug!("page {}: {} new listings", page_no + 1, out.len() - before);
            // Stop when there is no next page, or when a page brought nothing new
            // (protects against cursors that loop).
            match page.cursor_after(cursor.as_deref()) {
                Some(c) if out.len() > before && Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// ureq accepts almost any string as a proxy, so check it properly: a typo
/// should fail at startup, not as mysterious request errors later.
fn validate_proxy(p: &str) -> Result<()> {
    let bad = |why: &str| anyhow!("bad proxy '{}': {why}", redact(p));
    let u = Url::parse(p).map_err(|_| bad("expected e.g. http://user:pass@host:port"))?;
    if !["http", "socks4", "socks4a", "socks5"].contains(&u.scheme()) {
        return Err(bad("scheme must be http, socks4, socks4a or socks5"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(bad("missing host"));
    }
    Ok(())
}

/// Hides credentials in a proxy URL for logging.
fn redact(proxy: &str) -> String {
    match Url::parse(proxy) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("***");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => proxy.to_string(),
    }
}

/// airbnb.com, www.airbnb.co.uk, fr.airbnb.ca, airbnb.com.au, …
pub fn is_airbnb_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let labels: Vec<&str> = host.split('.').collect();
    let Some(pos) = labels.iter().rposition(|l| *l == "airbnb") else {
        return false;
    };
    let suffix = &labels[pos + 1..];
    (1..=2).contains(&suffix.len()) && suffix.iter().all(|l| (2..=3).contains(&l.len()))
}

pub fn is_search_url(url: &Url) -> bool {
    is_airbnb_host(url) && url.path().starts_with("/s/")
}

/// Removes paging params so the stored URL always points to the first page.
pub fn normalize(url: &Url) -> String {
    let mut u = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !PAGING_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_fragment(None);
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.to_string()
}

fn page_url(search_url: &str, cursor: Option<&str>, origin: Option<&Url>) -> Result<String> {
    let mut u = Url::parse(search_url)?;
    if let Some(o) = origin {
        let mut replaced = o.clone();
        replaced.set_path(u.path());
        replaced.set_query(u.query());
        u = replaced;
    }
    if let Some(c) = cursor {
        u.query_pairs_mut()
            .append_pair("cursor", c)
            .append_pair("pagination_search", "true");
    }
    Ok(u.to_string())
}

/// Finds the first URL in free text (e.g. "Check this out https://…").
pub fn extract_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| w.starts_with("http://") || w.starts_with("https://"))
        .map(|w| {
            w.trim_end_matches(['.', ',', ')', '>', '"', '\''])
                .to_string()
        })
}

fn find_search_url_in_text(body: &str) -> Option<Url> {
    let mut rest = body;
    while let Some(pos) = rest.find("https://") {
        rest = &rest[pos..];
        let end = rest
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = rest[..end].replace("&amp;", "&");
        if let Ok(u) = Url::parse(&candidate)
            && is_search_url(&u)
        {
            return Some(u);
        }
        rest = &rest[end.max(1)..];
    }
    None
}

/// A human-friendly default name, e.g. "Lisbon, Portugal".
pub fn default_name(search_url: &str) -> String {
    let Ok(u) = Url::parse(search_url) else {
        return "Airbnb search".into();
    };
    if let Some((_, q)) = u
        .query_pairs()
        .find(|(k, v)| k == "query" && !v.trim().is_empty())
    {
        return q.trim().to_string();
    }
    let seg = u.path_segments().and_then(|mut s| {
        s.next(); // "s"
        s.next()
    });
    match seg {
        Some(seg) if !seg.is_empty() && seg != "homes" => {
            let decoded = percent_encoding::percent_decode_str(seg).decode_utf8_lossy();
            decoded.replace("--", ", ").replace('-', " ")
        }
        _ => "Airbnb search".into(),
    }
}

/// Link to a listing, keeping the dates and guests from the search.
pub fn listing_url(search_url: &str, id: u64) -> String {
    let base = Url::parse(search_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("https://{h}")))
        .unwrap_or_else(|| "https://www.airbnb.com".into());
    let mut u = Url::parse(&format!("{base}/rooms/{id}")).expect("valid url");
    if let Ok(s) = Url::parse(search_url) {
        let keep = [
            "checkin", "checkout", "adults", "children", "infants", "pets",
        ];
        let pairs: Vec<(String, String)> = s
            .query_pairs()
            .filter(|(k, _)| keep.contains(&k.as_ref()))
            .map(|(k, v)| {
                (
                    k.replace("checkin", "check_in")
                        .replace("checkout", "check_out"),
                    v.into_owned(),
                )
            })
            .collect();
        if !pairs.is_empty() {
            u.query_pairs_mut().extend_pairs(pairs);
        }
    }
    u.to_string()
}

/// Returns the contents of every `<script type="application/json">` block.
fn json_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else { break };
        let tag = &rest[..tag_end];
        let body_start = tag_end + 1;
        let Some(close) = rest[body_start..].find("</script>") else {
            break;
        };
        if tag.contains("application/json") {
            out.push(&rest[body_start..body_start + close]);
        }
        rest = &rest[body_start + close..];
    }
    out
}

pub fn parse_page(html: &str) -> Result<Page> {
    let mut page = Page::default();
    let mut index: HashMap<u64, usize> = HashMap::new();
    let mut saw_search = false;
    for script in json_scripts(html) {
        if !script.contains("StaySearchResult") && !script.contains("staysSearch") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(script) else {
            continue;
        };
        let mut raw = Vec::new();
        walk(&v, &mut raw, &mut page, &mut saw_search);
        for l in raw {
            match index.get(&l.id) {
                // The same listing appears in the list and on the map; merge.
                Some(&i) => merge(&mut page.listings[i], l),
                None => {
                    index.insert(l.id, page.listings.len());
                    page.listings.push(l);
                }
            }
        }
    }
    if !saw_search && page.listings.is_empty() {
        return Err(anyhow!(
            "no search data in the page (blocked, captcha, or Airbnb changed its format)"
        ));
    }
    Ok(page)
}

fn merge(into: &mut Listing, other: Listing) {
    let fill = |a: &mut String, b: String| {
        if a.is_empty() {
            *a = b;
        }
    };
    fill(&mut into.title, other.title);
    fill(&mut into.name, other.name);
    fill(&mut into.details, other.details);
    fill(&mut into.price, other.price);
    fill(&mut into.rating, other.rating);
    if into.picture.is_none() {
        into.picture = other.picture;
    }
}

fn walk(v: &Value, out: &mut Vec<Listing>, page: &mut Page, saw_search: &mut bool) {
    match v {
        Value::Object(map) => {
            let is_result = map.get("__typename").and_then(Value::as_str)
                == Some("StaySearchResult")
                || map.contains_key("demandStayListing");
            if is_result && let Some(l) = parse_listing(v) {
                out.push(l);
                return;
            }
            for (k, child) in map {
                match k.as_str() {
                    "staysSearch" => *saw_search = true,
                    "paginationInfo" => read_pagination(child, page),
                    _ => {}
                }
                walk(child, out, page, saw_search);
            }
        }
        Value::Array(items) => {
            for child in items {
                walk(child, out, page, saw_search);
            }
        }
        _ => {}
    }
}

fn read_pagination(info: &Value, page: &mut Page) {
    if let Some(c) = text(info, &["nextPageCursor"]) {
        page.next_cursor = Some(c);
    }
    if let Some(list) = info.get("pageCursors").and_then(Value::as_array) {
        let cursors: Vec<String> = list
            .iter()
            .filter_map(|c| c.as_str().filter(|c| !c.is_empty()).map(str::to_string))
            .collect();
        if !cursors.is_empty() {
            page.page_cursors = cursors;
        }
    }
}

/// The string at `path`, with runs of whitespace (including newlines and
/// non-breaking spaces) collapsed to single spaces. `None` if missing or blank.
fn text(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    let s = cur
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!s.is_empty()).then_some(s)
}

fn id_from(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok().or_else(|| decode_global_id(s)),
        _ => None,
    }
}

/// Decodes a GraphQL global id like base64("DemandStayListing:12345").
fn decode_global_id(s: &str) -> Option<u64> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (_, digits) = text.rsplit_once(':')?;
    digits.parse().ok()
}

fn parse_listing(v: &Value) -> Option<Listing> {
    let id = v
        .pointer("/demandStayListing/id")
        .and_then(id_from)
        .or_else(|| v.pointer("/listing/id").and_then(id_from))
        .or_else(|| v.get("listingId").and_then(id_from))
        .or_else(|| v.get("propertyId").and_then(id_from))?;

    let name = text(
        v,
        &["nameLocalized", "localizedStringWithTranslationPreference"],
    )
    .or_else(|| {
        text(
            v,
            &[
                "demandStayListing",
                "description",
                "name",
                "localizedStringWithTranslationPreference",
            ],
        )
    })
    .or_else(|| text(v, &["listing", "name"]))
    .unwrap_or_default();
    let title = text(v, &["title"])
        .or_else(|| text(v, &["listing", "title"]))
        .or_else(|| text(v, &["subtitle"]))
        .unwrap_or_default();

    // e.g. "2 bedrooms", "20–25 Sept". Distances are relative to wherever the
    // request came from, so they are noise here.
    let mut details: Vec<String> = Vec::new();
    for line in ["primaryLine", "secondaryLine"] {
        let items = v
            .pointer(&format!("/structuredContent/{line}"))
            .and_then(Value::as_array);
        for item in items.into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("DISTANCE") {
                continue;
            }
            if let Some(body) = text(item, &["body"])
                && !details.contains(&body)
            {
                details.push(body);
            }
        }
    }

    let sdp = v.get("structuredDisplayPrice").unwrap_or(&Value::Null);
    let mut price_parts: Vec<String> = Vec::new();
    let current = text(sdp, &["primaryLine", "discountedPrice"])
        .or_else(|| text(sdp, &["primaryLine", "price"]));
    if let Some(p) = current {
        let mut s = p.clone();
        if let Some(orig) = text(sdp, &["primaryLine", "originalPrice"])
            && orig != p
        {
            s = format!("{p} (was {orig})");
        }
        if let Some(q) = text(sdp, &["primaryLine", "qualifier"]) {
            s = format!("{s} {q}");
        }
        price_parts.push(s);
    } else if let Some(a) = text(sdp, &["primaryLine", "accessibilityLabel"]) {
        price_parts.push(a);
    }
    if let Some(total) = text(sdp, &["secondaryLine", "price"]) {
        price_parts.push(total);
    }

    let rating = text(v, &["avgRatingLocalized"])
        .or_else(|| text(v, &["avgRatingA11yLabel"]))
        .unwrap_or_default();
    let picture = v
        .get("contextualPictures")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find_map(|p| text(p, &["picture"])));

    Some(Listing {
        id,
        title,
        name,
        details: details.join(" · "),
        price: price_parts.join(" · "),
        rating,
        picture,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gid(id: u64) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("DemandStayListing:{id}"))
    }

    fn sample_html() -> String {
        let state = serde_json::json!({
            "niobeClientData": [["StaysSearch:{}", {"data": {"presentation": {"staysSearch": {
                "results": {
                    "searchResults": [
                        {
                            "__typename": "StaySearchResult",
                            "demandStayListing": {
                                "id": gid(111),
                                "description": {"name": {"localizedStringWithTranslationPreference": "Sunny loft"}}
                            },
                            "title": "Apartment in Lisbon",
                            "structuredContent": {"primaryLine": [{"body": "2 bedrooms"}, {"body": "3 beds"}]},
                            "structuredDisplayPrice": {
                                "primaryLine": {"discountedPrice": "€80", "originalPrice": "€95", "qualifier": "night"},
                                "secondaryLine": {"price": "€560 total"}
                            },
                            "avgRatingLocalized": "4.91 (57)",
                            "contextualPictures": [{"picture": "https://a0.muscache.com/p.jpg"}]
                        },
                        {"__typename": "StaySearchResult", "demandStayListing": {"id": gid(222)}, "title": "Home in Lisbon"},
                        {"__typename": "SomethingElse", "title": "ad"}
                    ],
                    "paginationInfo": {"nextPageCursor": "CURSOR2", "pageCursors": ["a", "b"]}
                },
                "mapResults": {"mapSearchResults": [
                    {"__typename": "StaySearchResult", "demandStayListing": {"id": gid(222)},
                     "contextualPictures": [{"picture": "https://a0.muscache.com/q.jpg"}]},
                    {"__typename": "StaySearchResult", "listing": {"id": "333", "name": "Legacy shape"}}
                ]}
            }}}}]]
        });
        format!(
            "<html><head><script>var x = 1;</script>\
             <script id=\"data-deferred-state-0\" data-deferred-state-0=\"true\" type=\"application/json\">{state}</script>\
             </head><body></body></html>"
        )
    }

    #[test]
    fn parses_listings_and_cursor() {
        let page = parse_page(&sample_html()).unwrap();
        let ids: Vec<u64> = page.listings.iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![111, 222, 333]);
        assert_eq!(page.next_cursor.as_deref(), Some("CURSOR2"));
        let l = &page.listings[0];
        assert_eq!(l.name, "Sunny loft");
        assert_eq!(l.title, "Apartment in Lisbon");
        assert_eq!(l.details, "2 bedrooms · 3 beds");
        assert_eq!(l.price, "€80 (was €95) night · €560 total");
        assert_eq!(l.rating, "4.91 (57)");
        assert_eq!(l.picture.as_deref(), Some("https://a0.muscache.com/p.jpg"));
        // map result merged its picture into the list result
        assert_eq!(
            page.listings[1].picture.as_deref(),
            Some("https://a0.muscache.com/q.jpg")
        );
        assert_eq!(page.listings[2].name, "Legacy shape");
    }

    #[test]
    fn page_without_data_is_error() {
        assert!(parse_page("<html><body>Access denied</body></html>").is_err());
    }

    #[test]
    fn search_url_detection() {
        let ok = Url::parse("https://www.airbnb.co.uk/s/London/homes?adults=2").unwrap();
        assert!(is_search_url(&ok));
        let room = Url::parse("https://www.airbnb.com/rooms/123").unwrap();
        assert!(!is_search_url(&room));
        let evil = Url::parse("https://airbnb.evil.com/s/x/homes").unwrap();
        assert!(!is_search_url(&evil));
        let short = Url::parse("https://abnb.me/abc").unwrap();
        assert!(!is_search_url(&short));
    }

    #[test]
    fn normalize_strips_paging() {
        let u = Url::parse(
            "https://www.airbnb.com/s/Lisbon/homes?adults=2&cursor=abc&items_offset=18#x",
        )
        .unwrap();
        assert_eq!(
            normalize(&u),
            "https://www.airbnb.com/s/Lisbon/homes?adults=2"
        );
    }

    #[test]
    fn page_url_adds_cursor() {
        let u = page_url(
            "https://www.airbnb.com/s/L/homes?adults=2",
            Some("c=1"),
            None,
        )
        .unwrap();
        assert_eq!(
            u,
            "https://www.airbnb.com/s/L/homes?adults=2&cursor=c%3D1&pagination_search=true"
        );
    }

    #[test]
    fn names() {
        assert_eq!(
            default_name("https://www.airbnb.com/s/Lisbon--Portugal/homes?x=1"),
            "Lisbon, Portugal"
        );
        assert_eq!(
            default_name("https://www.airbnb.com/s/homes?query=Porto%2C%20Portugal"),
            "Porto, Portugal"
        );
        assert_eq!(
            default_name("https://www.airbnb.com/s/S%C3%A3o-Paulo/homes"),
            "São Paulo"
        );
        assert_eq!(
            default_name("https://www.airbnb.com/s/homes"),
            "Airbnb search"
        );
    }

    #[test]
    fn listing_link_keeps_dates() {
        let s = "https://www.airbnb.de/s/Berlin/homes?checkin=2026-11-01&checkout=2026-11-05&adults=2&price_max=100";
        assert_eq!(
            listing_url(s, 42),
            "https://www.airbnb.de/rooms/42?check_in=2026-11-01&check_out=2026-11-05&adults=2"
        );
    }

    #[test]
    fn extracts_url_from_text() {
        assert_eq!(
            extract_url("look: https://www.airbnb.com/s/x/homes?a=1."),
            Some("https://www.airbnb.com/s/x/homes?a=1".into())
        );
        assert_eq!(extract_url("no link"), None);
    }

    #[test]
    fn finds_search_url_in_redirect_page() {
        let body = r#"<meta http-equiv="refresh" content="0;url=x"><a href="https://www.airbnb.com/s/Rome/homes?adults=1&amp;price_max=90">go</a>"#;
        let u = find_search_url_in_text(body).unwrap();
        assert_eq!(
            u.as_str(),
            "https://www.airbnb.com/s/Rome/homes?adults=1&price_max=90"
        );
    }

    #[test]
    fn validates_proxies() {
        for ok in [
            "http://h:8080",
            "http://u:p@h:1",
            "socks5://10.0.0.2:1080",
            "socks4://h:1",
        ] {
            assert!(validate_proxy(ok).is_ok(), "{ok}");
        }
        for bad in [
            "h:8080",
            "not a proxy",
            "ftp://h:1",
            "https://h:1",
            "http://:1",
            "socks5:/x",
        ] {
            assert!(validate_proxy(bad).is_err(), "{bad}");
        }
        let err = validate_proxy("ftp://bob:hunter2@h:1")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(redact("http://user:pw@h:8080"), "http://***@h:8080/");
        assert_eq!(redact("socks5://h:1080"), "socks5://h:1080");
    }

    #[test]
    fn tolerates_malformed_html() {
        // Unclosed tag, unclosed script, and invalid JSON that mentions the markers.
        assert!(parse_page("<script type=\"application/json\"").is_err());
        assert!(parse_page("<script type=\"application/json\">{\"staysSearch\":").is_err());
        let bad_json = "<script type=\"application/json\">{staysSearch: nope}</script>";
        assert!(parse_page(bad_json).is_err());
        // A later valid block still counts.
        let ok = format!("{bad_json}{}", sample_html());
        assert_eq!(parse_page(&ok).unwrap().listings.len(), 3);
    }

    #[test]
    fn id_shapes() {
        let v = serde_json::json!({"__typename": "StaySearchResult", "listingId": 77});
        assert_eq!(parse_listing(&v).unwrap().id, 77);
        let v = serde_json::json!({"__typename": "StaySearchResult", "propertyId": "88"});
        assert_eq!(parse_listing(&v).unwrap().id, 88);
        let v = serde_json::json!({"__typename": "StaySearchResult", "listingId": true});
        assert!(parse_listing(&v).is_none());
        let v = serde_json::json!({"__typename": "StaySearchResult", "listingId": "!!notbase64"});
        assert!(parse_listing(&v).is_none());
        // Results without any id are skipped, but still walked into.
        let html = format!(
            "<script type=\"application/json\">{}</script>",
            serde_json::json!({"staysSearch": {"x": {"__typename": "StaySearchResult"}}})
        );
        assert!(parse_page(&html).unwrap().listings.is_empty());
    }

    #[test]
    fn price_falls_back_to_accessibility_label() {
        let v = serde_json::json!({
            "listingId": 1,
            "__typename": "StaySearchResult",
            "structuredDisplayPrice": {"primaryLine": {"accessibilityLabel": "$90\u{00a0}per night"}}
        });
        assert_eq!(parse_listing(&v).unwrap().price, "$90 per night");
        let v = serde_json::json!({
            "listingId": 1,
            "__typename": "StaySearchResult",
            "structuredDisplayPrice": {"primaryLine": {"price": "€5", "originalPrice": "€5"}}
        });
        assert_eq!(
            parse_listing(&v).unwrap().price,
            "€5",
            "same original price is not shown"
        );
    }

    #[test]
    fn odd_urls() {
        assert_eq!(default_name("not a url"), "Airbnb search");
        assert!(!is_airbnb_host(&Url::parse("mailto:a@airbnb.com").unwrap()));
        assert!(is_airbnb_host(
            &Url::parse("https://fr.airbnb.ca/s/x").unwrap()
        ));
        assert!(is_airbnb_host(
            &Url::parse("https://www.airbnb.com.au/s/x").unwrap()
        ));
        assert!(!is_airbnb_host(
            &Url::parse("https://airbnb.example.co.uk/s/x").unwrap()
        ));
        // Only paging params are dropped; with none left there is no "?".
        let u = Url::parse("https://www.airbnb.com/s/x/homes?cursor=1").unwrap();
        assert_eq!(normalize(&u), "https://www.airbnb.com/s/x/homes");
        // Room links without search params, and a bad search url.
        assert_eq!(
            listing_url("https://www.airbnb.com/s/x/homes", 5),
            "https://www.airbnb.com/rooms/5"
        );
        assert_eq!(listing_url("garbage", 5), "https://www.airbnb.com/rooms/5");
    }

    #[test]
    fn pause_waits_for_the_configured_delay() {
        let f = Fetcher::new(&[])
            .unwrap()
            .with_page_delay(Duration::from_millis(30));
        let t = std::time::Instant::now();
        f.pause();
        assert!(t.elapsed() >= Duration::from_millis(30));
        let f = f.with_page_delay(Duration::ZERO);
        let t = std::time::Instant::now();
        f.pause();
        assert!(t.elapsed() < Duration::from_millis(30));
    }
}
