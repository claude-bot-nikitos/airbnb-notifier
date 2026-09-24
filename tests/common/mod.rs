//! Test helpers: a tiny HTTP server used to fake Airbnb, proxies and the
//! Telegram Bot API.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The request target as sent: a path, or an absolute URL for proxy requests.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    /// Query parameter from the target.
    pub fn query(&self, key: &str) -> Option<String> {
        let full = if self.target.starts_with("http") {
            self.target.clone()
        } else {
            format!("http://x{}", self.target)
        };
        url::Url::parse(&full)
            .ok()?
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn html(body: impl Into<String>) -> Response {
        Response::with("text/html; charset=utf-8", 200, body.into().into_bytes())
    }
    pub fn json(v: Value) -> Response {
        Response::with("application/json", 200, v.to_string().into_bytes())
    }
    pub fn status(code: u16, body: &str) -> Response {
        Response::with("text/plain", code, body.as_bytes().to_vec())
    }
    pub fn redirect(location: &str) -> Response {
        let mut r = Response::status(302, "");
        r.headers.push(("Location".into(), location.into()));
        r
    }
    fn with(content_type: &str, status: u16, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body,
        }
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

pub struct MockServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (handler, requests) = (handler.clone(), requests.clone());
                            std::thread::spawn(move || serve(stream, &*handler, &requests));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
        }
        MockServer {
            port,
            requests,
            stop,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(stream: TcpStream, handler: &Handler, requests: &Mutex<Vec<Request>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    let _ = reader.read_exact(&mut body);
    let req = Request {
        method,
        target,
        headers,
        body,
    };
    requests.lock().unwrap().push(req.clone());
    let resp = handler(&req);
    let mut out = stream;
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.body.len()
    );
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(&resp.body);
    let _ = out.flush();
}

// ---------------------------------------------------------------- Airbnb

pub fn global_id(id: u64) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("DemandStayListing:{id}"))
}

/// One search result shaped like Airbnb's current StaySearchResult.
pub fn stay(id: u64) -> Value {
    json!({
        "__typename": "StaySearchResult",
        "avgRatingA11yLabel": "4.9 out of 5 average rating",
        "avgRatingLocalized": "4.9",
        "structuredDisplayPrice": {
            "primaryLine": {
                "__typename": "QualifiedDisplayPriceLine",
                "price": format!("€{}", 100 + id),
                "qualifier": "for 5 nights"
            },
            "secondaryLine": null
        },
        "title": null,
        "nameLocalized": {"localizedStringWithTranslationPreference": format!("Flat {id}")},
        "structuredContent": {
            "primaryLine": [{"body": "1,234 kilometres away", "type": "DISTANCE"}],
            "secondaryLine": [{"body": "10–15 Nov", "type": "DATE"}]
        },
        "contextualPictures": [{"picture": format!("https://a0.muscache.com/im/pictures/{id}.jpg")}],
        "demandStayListing": {"__typename": "DemandStayListing", "id": global_id(id)}
    })
}

/// Wraps a deferred-state JSON value in an HTML page like Airbnb serves.
pub fn wrap_state(state: &Value) -> String {
    format!(
        "<!doctype html><html><head><script>window.x = {{}};</script>\
         <script id=\"data-injector-instances\" type=\"application/json\">{{\"a\":1}}</script>\
         </head><body><div id=\"root\"></div>\
         <script id=\"data-deferred-state-0\" data-deferred-state-0=\"true\" type=\"application/json\">{state}</script>\
         </body></html>"
    )
}

/// A search results page with the given listing ids and pagination cursors.
pub fn search_page(ids: &[u64], page_cursors: &[String]) -> String {
    let results: Vec<Value> = ids.iter().map(|&id| stay(id)).collect();
    let state = json!({
        "niobeClientData": [[
            "StaysSearch:{}",
            {"data": {"presentation": {"staysSearch": {
                "results": {
                    "searchResults": results,
                    "paginationInfo": {"__typename": "StaysSearchPaginationInfo", "pageCursors": page_cursors}
                }
            }}}}
        ]]
    });
    wrap_state(&state)
}

pub fn cursors(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("CURSOR{i}")).collect()
}

/// Fake Airbnb: listings split into pages of `per_page`, addressed by `cursor`.
pub struct FakeAirbnb {
    pub server: MockServer,
    pub listings: Arc<Mutex<Vec<u64>>>,
    /// When set, every request gets this status code.
    pub fail_with: Arc<Mutex<Option<u16>>>,
}

impl FakeAirbnb {
    pub fn start(initial: &[u64], per_page: usize) -> FakeAirbnb {
        let listings = Arc::new(Mutex::new(initial.to_vec()));
        let fail_with = Arc::new(Mutex::new(None));
        let server = {
            let (listings, fail_with) = (listings.clone(), fail_with.clone());
            MockServer::start(move |req| {
                if let Some(code) = *fail_with.lock().unwrap() {
                    return Response::status(code, "blocked");
                }
                let all = listings.lock().unwrap().clone();
                let pages = all.len().div_ceil(per_page).max(1);
                let cursors = cursors(pages);
                let page = req
                    .query("cursor")
                    .and_then(|c| cursors.iter().position(|x| *x == c))
                    .unwrap_or(0);
                let slice: Vec<u64> = all
                    .iter()
                    .skip(page * per_page)
                    .take(per_page)
                    .copied()
                    .collect();
                Response::html(search_page(&slice, &cursors))
            })
        };
        FakeAirbnb {
            server,
            listings,
            fail_with,
        }
    }

    pub fn add(&self, id: u64) {
        self.listings.lock().unwrap().push(id);
    }
}

// ---------------------------------------------------------------- Telegram

/// Fake Bot API: queue updates for getUpdates, inspect what the bot sent.
pub struct FakeTelegram {
    pub server: MockServer,
    updates: Arc<Mutex<VecDeque<Value>>>,
    next_update_id: AtomicI64,
}

pub const TOKEN: &str = "123:TEST-TOKEN";

impl FakeTelegram {
    pub fn start() -> FakeTelegram {
        let updates: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
        let server = {
            let updates = updates.clone();
            let message_id = Arc::new(AtomicI64::new(1000));
            MockServer::start(move |req| {
                let Some(method) = req.target.strip_prefix(&format!("/bot{TOKEN}/")) else {
                    return Response::status(401, r#"{"ok":false,"description":"Unauthorized"}"#);
                };
                match method {
                    "getUpdates" => {
                        let offset = req.json()["offset"].as_i64().unwrap_or(0);
                        // Short long-poll so the bot loop doesn't spin.
                        let deadline = Instant::now() + Duration::from_millis(300);
                        loop {
                            let batch: Vec<Value> = {
                                let mut q = updates.lock().unwrap();
                                q.retain(|u| u["update_id"].as_i64().unwrap() >= offset);
                                q.iter().cloned().collect()
                            };
                            if !batch.is_empty() || Instant::now() > deadline {
                                return Response::json(json!({"ok": true, "result": batch}));
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    _ => {
                        let id = message_id.fetch_add(1, Ordering::Relaxed);
                        Response::json(json!({"ok": true, "result": {"message_id": id}}))
                    }
                }
            })
        };
        FakeTelegram {
            server,
            updates,
            next_update_id: AtomicI64::new(1),
        }
    }

    pub fn api_url(&self) -> String {
        self.server.url()
    }

    fn push(&self, mut update: Value) {
        let id = self.next_update_id.fetch_add(1, Ordering::Relaxed);
        update["update_id"] = json!(id);
        self.updates.lock().unwrap().push_back(update);
    }

    pub fn user_says(&self, user: i64, text: &str) {
        self.push(json!({"message": {
            "message_id": 1, "text": text,
            "chat": {"id": user, "type": "private"}, "from": {"id": user, "is_bot": false, "first_name": "U"}
        }}));
    }

    pub fn user_taps(&self, user: i64, data: &str) {
        self.push(json!({"callback_query": {
            "id": format!("cb{data}"), "data": data, "from": {"id": user, "is_bot": false, "first_name": "U"},
            "message": {"message_id": 77, "chat": {"id": user, "type": "private"}}
        }}));
    }

    /// Every Bot API call except getUpdates: (method, JSON body).
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.server
            .requests()
            .into_iter()
            .filter_map(|r| {
                let method = r.target.rsplit('/').next()?.to_string();
                (method != "getUpdates").then(|| (method, r.json()))
            })
            .collect()
    }

    /// Waits until a call matching `pred` shows up after the first `skip` calls.
    pub fn wait_for(
        &self,
        skip: usize,
        what: &str,
        pred: impl Fn(&str, &Value) -> bool,
    ) -> (String, Value) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(c) = self
                .calls()
                .into_iter()
                .skip(skip)
                .find(|(m, b)| pred(m, b))
            {
                return c;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; calls so far: {:#?}",
                self.calls()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for a sendMessage to `chat` whose text contains `needle`.
    pub fn wait_message(&self, skip: usize, chat: i64, needle: &str) -> Value {
        self.wait_for(
            skip,
            &format!("message to {chat} containing {needle:?}"),
            |m, b| {
                m == "sendMessage"
                    && b["chat_id"].as_i64() == Some(chat)
                    && b["text"].as_str().is_some_and(|t| t.contains(needle))
            },
        )
        .1
    }
}

pub fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("abn-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn temp_suffix() -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    N.fetch_add(1, Ordering::Relaxed).to_string()
}

/// Command for the built binary. `TEST_BIN_RUNNER` (e.g. `qemu-aarch64-static`)
/// runs it under an emulator when testing a cross-compiled build.
pub fn bin_command() -> std::process::Command {
    let exe = env!("CARGO_BIN_EXE_airbnb-notifier");
    match std::env::var("TEST_BIN_RUNNER") {
        Ok(runner) if !runner.is_empty() => {
            let mut c = std::process::Command::new(runner);
            c.arg(exe);
            c
        }
        _ => std::process::Command::new(exe),
    }
}
