//! Fetcher tests against a local fake Airbnb and fake proxies.

mod common;

use std::time::Duration;

use airbnb_notifier::airbnb::Fetcher;
use common::{FakeAirbnb, MockServer, Response, cursors, search_page};

const SEARCH: &str = "https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2&checkin=2026-11-10";

fn fetcher(origin: &str) -> Fetcher {
    Fetcher::new(&[])
        .unwrap()
        .with_page_delay(Duration::ZERO)
        .with_origin(origin)
        .unwrap()
}

#[test]
fn fetches_all_pages_via_page_cursors() {
    let fake = FakeAirbnb::start(&(1..=7).collect::<Vec<_>>(), 3);
    let listings = fetcher(&fake.server.url()).fetch_all(SEARCH, 10).unwrap();
    assert_eq!(
        listings.iter().map(|l| l.id).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );

    let reqs = fake.server.requests();
    assert_eq!(reqs.len(), 3, "3 pages of 3");
    // The search path and filters are kept; later pages add the cursor.
    assert!(
        reqs[0]
            .target
            .starts_with("/s/Lisbon--Portugal/homes?adults=2&checkin=2026-11-10")
    );
    assert_eq!(reqs[0].query("cursor"), None);
    assert_eq!(reqs[1].query("cursor").as_deref(), Some("CURSOR1"));
    assert_eq!(reqs[2].query("cursor").as_deref(), Some("CURSOR2"));
    // Looks like a browser.
    assert!(
        reqs[0]
            .header("User-Agent")
            .unwrap()
            .contains("Mozilla/5.0")
    );
    assert!(reqs[0].header("Accept-Language").is_some());
}

#[test]
fn respects_max_pages() {
    let fake = FakeAirbnb::start(&(1..=9).collect::<Vec<_>>(), 3);
    let listings = fetcher(&fake.server.url()).fetch_all(SEARCH, 2).unwrap();
    assert_eq!(listings.len(), 6);
    assert_eq!(fake.server.requests().len(), 2);
}

#[test]
fn stops_when_a_page_brings_nothing_new() {
    // A broken server that ignores the cursor and keeps returning page 1.
    let server = MockServer::start(|_| Response::html(search_page(&[1, 2], &cursors(5))));
    let listings = fetcher(&server.url()).fetch_all(SEARCH, 10).unwrap();
    assert_eq!(listings.len(), 2);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn uses_next_page_cursor_when_present() {
    let server = MockServer::start(|req| {
        let (ids, next) = match req.query("cursor").as_deref() {
            None => (vec![1], Some("N2")),
            Some("N2") => (vec![2], None),
            other => panic!("unexpected cursor {other:?}"),
        };
        let state = serde_json::json!({"niobeClientData": [[null, {"data": {"presentation": {"staysSearch": {"results": {
            "searchResults": ids.iter().map(|&i| common::stay(i)).collect::<Vec<_>>(),
            "paginationInfo": {"nextPageCursor": next}
        }}}}}]]});
        Response::html(common::wrap_state(&state))
    });
    let listings = fetcher(&server.url()).fetch_all(SEARCH, 10).unwrap();
    assert_eq!(
        listings.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn empty_search_is_ok() {
    let fake = FakeAirbnb::start(&[], 3);
    assert!(
        fetcher(&fake.server.url())
            .fetch_all(SEARCH, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn http_errors_are_reported() {
    let fake = FakeAirbnb::start(&[1], 3);
    *fake.fail_with.lock().unwrap() = Some(429);
    let err = fetcher(&fake.server.url())
        .fetch_all(SEARCH, 10)
        .unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 429"), "{err:#}");
}

#[test]
fn captcha_page_is_an_error_not_an_empty_result() {
    let server = MockServer::start(|_| {
        Response::html("<html><body>Please verify you are a human</body></html>")
    });
    let err = fetcher(&server.url()).fetch_all(SEARCH, 10).unwrap_err();
    assert!(format!("{err:#}").contains("no search data"), "{err:#}");
}

#[test]
fn network_errors_are_reported() {
    // Nothing listens on port 1.
    let err = fetcher("http://127.0.0.1:1")
        .fetch_all(SEARCH, 1)
        .unwrap_err();
    assert!(format!("{err:#}").contains("network error"), "{err:#}");
}

/// A fake forward proxy. HTTP proxies receive the absolute URL as the request
/// target; this one serves Airbnb pages itself, or refuses with `status`.
fn proxy(status: Option<u16>, ids: &'static [u64]) -> MockServer {
    MockServer::start(move |req| {
        assert!(
            req.target.starts_with("http://"),
            "proxy got {}",
            req.target
        );
        match status {
            Some(code) => Response::status(code, "denied"),
            None => Response::html(search_page(ids, &cursors(1))),
        }
    })
}

#[test]
fn rotates_to_the_next_proxy_on_failure_and_sticks_with_it() {
    let bad = proxy(Some(403), &[]);
    let good = proxy(None, &[5, 6]);
    let f = Fetcher::new(&[bad.url(), good.url()])
        .unwrap()
        .with_page_delay(Duration::ZERO)
        .with_origin("http://airbnb.test")
        .unwrap();

    let ids: Vec<u64> = f
        .fetch_all(SEARCH, 1)
        .unwrap()
        .iter()
        .map(|l| l.id)
        .collect();
    assert_eq!(ids, vec![5, 6]);
    assert_eq!(bad.requests().len(), 1);
    assert_eq!(good.requests().len(), 1);
    assert!(
        good.requests()[0]
            .target
            .starts_with("http://airbnb.test/s/Lisbon--Portugal/homes")
    );

    // The next fetch goes straight to the working proxy.
    f.fetch_all(SEARCH, 1).unwrap();
    assert_eq!(bad.requests().len(), 1);
    assert_eq!(good.requests().len(), 2);
}

#[test]
fn captcha_via_one_proxy_also_rotates() {
    let captcha = MockServer::start(|_| Response::html("<html>captcha</html>"));
    let good = proxy(None, &[9]);
    let f = Fetcher::new(&[captcha.url(), good.url()])
        .unwrap()
        .with_origin("http://airbnb.test")
        .unwrap();
    assert_eq!(f.fetch_all(SEARCH, 1).unwrap()[0].id, 9);
}

#[test]
fn all_proxies_failing_is_an_error() {
    let a = proxy(Some(403), &[]);
    let b = proxy(Some(503), &[]);
    let f = Fetcher::new(&[a.url(), b.url()])
        .unwrap()
        .with_origin("http://airbnb.test")
        .unwrap();
    let err = f.fetch_all(SEARCH, 1).unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 503"), "{err:#}");
    assert_eq!(
        a.requests().len() + b.requests().len(),
        2,
        "each tried once"
    );
}

#[test]
fn proxy_credentials_are_sent_when_tunnelling_https() {
    // Real Airbnb traffic is HTTPS, which goes through a CONNECT tunnel.
    let p = MockServer::start(|_| Response::status(407, "auth required"));
    let url = format!("http://alice:s3cret@127.0.0.1:{}", p.port);
    let f = Fetcher::new(&[url]).unwrap();
    let err = f.fetch_all(SEARCH, 1).unwrap_err();
    let reqs = p.requests();
    assert_eq!(reqs[0].method, "CONNECT");
    assert_eq!(reqs[0].target, "www.airbnb.com:443");
    // "alice:s3cret" in base64; the scheme name is case-insensitive (RFC 7235).
    let auth = reqs[0].header("Proxy-Authorization").unwrap();
    let (scheme, creds) = auth.split_once(' ').unwrap();
    assert!(scheme.eq_ignore_ascii_case("basic"));
    assert_eq!(creds, "YWxpY2U6czNjcmV0");
    // The password never shows up in errors (they end up in logs and chats).
    assert!(!format!("{err:#}").contains("s3cret"), "{err:#}");
}

#[test]
fn invalid_proxy_and_origin_are_rejected() {
    assert!(Fetcher::new(&["not a proxy url at all".into()]).is_err());
    assert!(Fetcher::new(&[]).unwrap().with_origin("nope").is_err());
}

#[test]
fn resolve_accepts_search_urls_without_network() {
    let f = Fetcher::new(&[]).unwrap();
    assert_eq!(
        f.resolve("  https://www.airbnb.co.uk/s/London/homes?adults=1&cursor=abc  ")
            .unwrap(),
        "https://www.airbnb.co.uk/s/London/homes?adults=1"
    );
}

#[test]
fn resolve_follows_redirects_of_share_links() {
    // Through a proxy the fake can impersonate abnb.me and airbnb.com.
    let p = MockServer::start(|req| {
        if req.target.starts_with("http://abnb.me/") {
            Response::redirect("http://www.airbnb.com/s/Rome/homes?adults=2&cursor=zzz")
        } else {
            Response::html(search_page(&[1], &cursors(1)))
        }
    });
    let f = Fetcher::new(&[p.url()]).unwrap();
    assert_eq!(
        f.resolve("http://abnb.me/AbCdEf").unwrap(),
        "http://www.airbnb.com/s/Rome/homes?adults=2"
    );
}

#[test]
fn resolve_finds_the_search_url_inside_a_landing_page() {
    let server = MockServer::start(|_| {
        Response::html(
            r#"<html><head><meta property="og:url" content="https://www.airbnb.com/s/Porto/homes?adults=3&amp;price_max=120"></head></html>"#,
        )
    });
    let f = Fetcher::new(&[]).unwrap();
    assert_eq!(
        f.resolve(&format!("{}/share", server.url())).unwrap(),
        "https://www.airbnb.com/s/Porto/homes?adults=3&price_max=120"
    );
}

#[test]
fn resolve_rejects_other_links() {
    let server = MockServer::start(|_| Response::html("<html>a room page, not a search</html>"));
    let f = Fetcher::new(&[]).unwrap();
    let err = f.resolve(&format!("{}/rooms/1", server.url())).unwrap_err();
    assert!(
        err.to_string().contains("isn't an Airbnb search link"),
        "{err}"
    );
    let err = f.resolve("hello world").unwrap_err();
    assert!(
        err.to_string().contains("doesn't look like a link"),
        "{err}"
    );
}
