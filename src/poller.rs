//! Periodically re-runs every active search and notifies about new listings.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::airbnb::{self, Fetcher, Listing};
use crate::bot::Api;
use crate::config::Config;
use crate::store::Store;
use crate::util::{esc, now_ts, truncate};

/// Per-check cap on individual listing messages; the rest are summarized.
const MAX_MESSAGES_PER_CHECK: usize = 10;
/// Consecutive failures before the owner is told something is wrong.
const FAILURES_BEFORE_ALERT: u32 = 3;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Search was deleted or paused while it was being fetched.
    Gone,
    Baseline {
        chat_id: i64,
        id: u32,
        name: String,
        count: usize,
    },
    New {
        chat_id: i64,
        name: String,
        url: String,
        listings: Vec<Listing>,
    },
    Failed {
        chat_id: i64,
        id: u32,
        name: String,
        error: String,
        alert: bool,
    },
    Recovered {
        chat_id: i64,
        id: u32,
        name: String,
        new: Vec<Listing>,
        url: String,
    },
}

/// Records a fetch result in the store and decides what to tell the user.
pub fn apply(store: &mut Store, id: u32, result: Result<Vec<Listing>>) -> Outcome {
    let now = now_ts();
    let Some(s) = store.get_mut(id) else {
        return Outcome::Gone;
    };
    if s.paused {
        return Outcome::Gone;
    }
    match result {
        Err(e) => {
            s.failures += 1;
            s.last_error = Some(format!("{e:#}"));
            // Retry on the normal schedule rather than hammering Airbnb.
            s.last_check = now;
            Outcome::Failed {
                chat_id: s.chat_id,
                id,
                name: s.name.clone(),
                error: format!("{e:#}"),
                alert: s.failures == FAILURES_BEFORE_ALERT,
            }
        }
        Ok(listings) => {
            let was_alerting = s.failures >= FAILURES_BEFORE_ALERT;
            s.failures = 0;
            s.last_error = None;
            s.last_check = now;
            if !s.initialized {
                s.initialized = true;
                s.seen.extend(listings.iter().map(|l| l.id));
                return Outcome::Baseline {
                    chat_id: s.chat_id,
                    id,
                    name: s.name.clone(),
                    count: listings.len(),
                };
            }
            let new: Vec<Listing> = listings
                .into_iter()
                .filter(|l| s.seen.insert(l.id))
                .collect();
            if was_alerting {
                return Outcome::Recovered {
                    chat_id: s.chat_id,
                    id,
                    name: s.name.clone(),
                    new,
                    url: s.url.clone(),
                };
            }
            Outcome::New {
                chat_id: s.chat_id,
                name: s.name.clone(),
                url: s.url.clone(),
                listings: new,
            }
        }
    }
}

pub fn listing_caption(search_name: &str, search_url: &str, l: &Listing) -> String {
    let mut lines = vec![format!("🏠 <b>New in “{}”</b>", esc(search_name))];
    let headline = if l.name.is_empty() { &l.title } else { &l.name };
    if !headline.is_empty() {
        lines.push(format!("<b>{}</b>", esc(&truncate(headline, 150))));
    }
    if !l.name.is_empty() && !l.title.is_empty() && l.title != l.name {
        lines.push(esc(&truncate(&l.title, 150)));
    }
    if !l.details.is_empty() {
        lines.push(esc(&truncate(&l.details, 200)));
    }
    if !l.price.is_empty() {
        lines.push(format!("💰 {}", esc(&truncate(&l.price, 150))));
    }
    if !l.rating.is_empty() {
        lines.push(format!("⭐ {}", esc(&l.rating)));
    }
    lines.push(format!(
        "<a href=\"{}\">Open on Airbnb</a>",
        esc(&airbnb::listing_url(search_url, l.id))
    ));
    lines.join("\n")
}

fn notify_listings(api: &dyn Api, chat_id: i64, name: &str, url: &str, listings: &[Listing]) {
    for l in listings.iter().take(MAX_MESSAGES_PER_CHECK) {
        let caption = listing_caption(name, url, l);
        let sent_photo = match &l.picture {
            Some(p) => api.send_photo(chat_id, p, &caption).is_ok(),
            None => false,
        };
        if !sent_photo && let Err(e) = api.send(chat_id, &caption, None) {
            log::error!("notify {chat_id} failed: {e:#}");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if listings.len() > MAX_MESSAGES_PER_CHECK {
        let more = listings.len() - MAX_MESSAGES_PER_CHECK;
        let text = format!(
            "…and {more} more new listing(s) in “{}”. <a href=\"{}\">Open the search</a>",
            esc(name),
            esc(url)
        );
        if let Err(e) = api.send(chat_id, &text, None) {
            log::error!("notify {chat_id} failed: {e:#}");
        }
    }
}

pub fn report(api: &dyn Api, outcome: &Outcome) {
    let say = |chat_id: i64, text: &str| {
        if let Err(e) = api.send(chat_id, text, None) {
            log::error!("notify {chat_id} failed: {e:#}");
        }
    };
    match outcome {
        Outcome::Gone => {}
        Outcome::Baseline {
            chat_id,
            id,
            name,
            count,
        } => say(
            *chat_id,
            &format!(
                "✅ <b>#{id} {}</b> is live: {count} current listings saved. \
                 I'll message you when new ones appear.",
                esc(name)
            ),
        ),
        Outcome::New {
            chat_id,
            name,
            url,
            listings,
        } => notify_listings(api, *chat_id, name, url, listings),
        Outcome::Failed {
            chat_id,
            id,
            name,
            error,
            alert,
        } => {
            if *alert {
                say(
                    *chat_id,
                    &format!(
                        "⚠️ Search <b>#{id} {}</b> keeps failing: {}\n\
                         I'll keep retrying. If Airbnb is blocking requests, try configuring a proxy.",
                        esc(name),
                        esc(error)
                    ),
                )
            }
        }
        Outcome::Recovered {
            chat_id,
            id,
            name,
            new,
            url,
        } => {
            say(
                *chat_id,
                &format!("✅ Search <b>#{id} {}</b> works again.", esc(name)),
            );
            notify_listings(api, *chat_id, name, url, new);
        }
    }
}

pub fn run(
    api: Arc<dyn Api>,
    store: Arc<Mutex<Store>>,
    fetcher: Arc<Fetcher>,
    wake: Receiver<()>,
    stop: Arc<AtomicBool>,
    config_path: &Path,
) {
    let mut cfg = Config::default();
    while !stop.load(Ordering::Relaxed) {
        // A config broken by a hand edit must not stop the checks: keep the last good one.
        match Config::load(config_path) {
            Ok(c) => cfg = c,
            Err(e) => log::error!("config reload failed, keeping the previous one: {e:#}"),
        }
        let interval = (cfg.check_interval_minutes.max(1) * 60) as i64;
        // Jitter so requests don't happen at exactly the same times.
        let interval = interval + crate::util::jitter(60) as i64;

        let mut due = store.lock().unwrap().due(now_ts(), interval);
        // In private chats the chat id is the user id: skip users whose access was
        // revoked. Group chats (negative ids) were created by an allowed member.
        due.retain(|s| s.chat_id < 0 || cfg.is_allowed(s.chat_id));
        for (i, s) in due.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if i > 0 {
                fetcher.pause();
            }
            log::info!("checking #{} '{}'", s.id, s.name);
            let result = fetcher.fetch_all(&s.url, cfg.max_pages);
            match &result {
                Ok(l) => log::info!("#{}: {} listings on the pages scanned", s.id, l.len()),
                Err(e) => log::warn!("#{} failed: {e:#}", s.id),
            }
            let outcome = {
                let mut st = store.lock().unwrap();
                let o = apply(&mut st, s.id, result);
                if let Err(e) = st.save() {
                    log::error!("saving searches failed: {e:#}");
                }
                o
            };
            if let Outcome::New { listings, .. } = &outcome
                && !listings.is_empty()
            {
                log::info!("#{}: {} new listing(s)", s.id, listings.len());
            }
            report(api.as_ref(), &outcome);
        }

        // Sleep ~30s, waking early on request (new search, /check) or shutdown.
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match wake.recv_timeout(Duration::from_secs(1)) {
                Ok(()) => {
                    while wake.try_recv().is_ok() {}
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::tests::MockApi;

    fn listing(id: u64) -> Listing {
        Listing {
            id,
            name: format!("L{id}"),
            picture: Some("https://p/x.jpg".into()),
            ..Default::default()
        }
    }

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("abn-poll-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = Store::load(&dir.join("s.json")).unwrap();
        st.add(
            5,
            "Lisbon".into(),
            "https://www.airbnb.com/s/Lisbon/homes?adults=2".into(),
        );
        st
    }

    #[test]
    fn baseline_then_only_new() {
        let mut st = store("new");
        let o = apply(&mut st, 1, Ok(vec![listing(1), listing(2)]));
        assert!(matches!(o, Outcome::Baseline { count: 2, .. }));
        let o = apply(&mut st, 1, Ok(vec![listing(2), listing(3), listing(1)]));
        let Outcome::New {
            listings, chat_id, ..
        } = o
        else {
            unreachable!("expected New, got {o:?}")
        };
        assert_eq!(chat_id, 5);
        assert_eq!(listings.iter().map(|l| l.id).collect::<Vec<_>>(), vec![3]);
        // Listings that dropped out and came back are not "new".
        let o = apply(&mut st, 1, Ok(vec![listing(1)]));
        assert!(matches!(o, Outcome::New { ref listings, .. } if listings.is_empty()));
    }

    #[test]
    fn failed_baseline_stays_uninitialized() {
        let mut st = store("failbase");
        let o = apply(&mut st, 1, Err(anyhow::anyhow!("HTTP 403")));
        assert!(matches!(o, Outcome::Failed { alert: false, .. }));
        assert!(!st.all()[0].initialized);
        // First success is still a silent baseline.
        assert!(matches!(
            apply(&mut st, 1, Ok(vec![listing(1)])),
            Outcome::Baseline { .. }
        ));
    }

    #[test]
    fn alerts_once_then_recovers() {
        let mut st = store("alert");
        apply(&mut st, 1, Ok(vec![listing(1)]));
        let alerts: Vec<bool> = (0..5)
            .map(|_| {
                let o = apply(&mut st, 1, Err(anyhow::anyhow!("boom")));
                matches!(o, Outcome::Failed { alert: true, .. })
            })
            .collect();
        assert_eq!(alerts, vec![false, false, true, false, false]);
        let o = apply(&mut st, 1, Ok(vec![listing(1), listing(9)]));
        assert!(matches!(o, Outcome::Recovered { ref new, .. } if new.len() == 1));
        assert_eq!(st.all()[0].failures, 0);
    }

    #[test]
    fn deleted_or_paused_is_gone() {
        let mut st = store("gone");
        st.get_mut(1).unwrap().paused = true;
        assert_eq!(apply(&mut st, 1, Ok(vec![])), Outcome::Gone);
        assert_eq!(apply(&mut st, 99, Ok(vec![])), Outcome::Gone);
    }

    #[test]
    fn report_caps_messages_and_falls_back_to_text() {
        let api = MockApi {
            fail_photos: true,
            ..Default::default()
        };
        let listings: Vec<Listing> = (1..=12).map(listing).collect();
        report(
            &api,
            &Outcome::New {
                chat_id: 5,
                name: "L".into(),
                url: "https://www.airbnb.com/s/L/homes".into(),
                listings,
            },
        );
        let sent = api.sent.lock().unwrap();
        assert_eq!(sent.len(), 11); // 10 listings (text fallback) + summary
        assert!(sent[10].1.contains("2 more"));
    }

    #[test]
    fn caption_contents() {
        let l = Listing {
            id: 42,
            title: "Apartment in Lisbon".into(),
            name: "Sunny <loft>".into(),
            details: "2 beds".into(),
            price: "€80 night".into(),
            rating: "4.9 (10)".into(),
            picture: None,
        };
        let c = listing_caption(
            "Lisbon",
            "https://www.airbnb.com/s/L/homes?checkin=2026-11-01",
            &l,
        );
        assert!(c.contains("Sunny &lt;loft&gt;"));
        assert!(c.contains("Apartment in Lisbon"));
        assert!(c.contains("💰 €80 night"));
        assert!(c.contains("https://www.airbnb.com/rooms/42?check_in=2026-11-01"));
    }

    #[test]
    fn report_sends_photos_and_handles_failures() {
        let api = MockApi::default();
        let mut with_photo = listing(1);
        let mut without_photo = listing(2);
        without_photo.picture = None;
        with_photo.name = "Nice".into();
        let new = Outcome::New {
            chat_id: 5,
            name: "L".into(),
            url: "https://www.airbnb.com/s/L/homes".into(),
            listings: vec![with_photo, without_photo],
        };
        report(&api, &new);
        assert_eq!(api.photos.lock().unwrap().len(), 1);
        assert_eq!(api.sent.lock().unwrap().len(), 1);

        // Nothing to say for Gone, or for a failure below the alert threshold.
        report(&api, &Outcome::Gone);
        report(
            &api,
            &Outcome::Failed {
                chat_id: 5,
                id: 1,
                name: "L".into(),
                error: "x".into(),
                alert: false,
            },
        );
        assert_eq!(api.sent.lock().unwrap().len(), 1);

        // Telegram being down is logged, never a panic.
        let down = MockApi {
            fail_all: true,
            ..Default::default()
        };
        report(&down, &new);
        let many = Outcome::New {
            chat_id: 5,
            name: "L".into(),
            url: "u".into(),
            listings: (1..=11).map(listing).collect(),
        };
        report(&down, &many);
        report(
            &down,
            &Outcome::Baseline {
                chat_id: 5,
                id: 1,
                name: "L".into(),
                count: 0,
            },
        );
    }

    #[test]
    fn caption_uses_title_when_name_is_missing() {
        let l = Listing {
            id: 1,
            title: "Home in Porto".into(),
            ..Default::default()
        };
        let c = listing_caption("P", "https://www.airbnb.com/s/P/homes", &l);
        assert!(c.contains("<b>Home in Porto</b>"));
        assert_eq!(c.matches("Home in Porto").count(), 1);
        assert!(!c.contains("💰"));
        assert!(!c.contains("⭐"));
    }

    /// Runs the poller loop in-process against a fake Airbnb page server.
    #[test]
    fn run_loop_checks_due_searches_and_stops() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        // Minimal page server: every request gets a page with listings 1 and 2.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = format!(
                    "<script type=\"application/json\">{}</script>",
                    serde_json::json!({"staysSearch": {"searchResults": [
                        {"__typename": "StaySearchResult", "listingId": 1},
                        {"__typename": "StaySearchResult", "listingId": 2}
                    ]}})
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });

        let dir = std::env::temp_dir().join(format!("abn-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let cfg_path = cfg.clone();
        let good = Config {
            allowed_users: vec![5],
            ..Config::default()
        };
        good.save(&cfg).unwrap();

        let mut st = Store::load(&dir.join("s.json")).unwrap();
        st.add(5, "A".into(), "https://www.airbnb.com/s/A/homes".into());
        st.add(5, "B".into(), "https://www.airbnb.com/s/B/homes".into());
        // A file where the data directory should be, so saving fails (logged).
        let st = {
            let mut broken = Store::load(&dir.join("blocked/s.json")).unwrap();
            std::fs::write(dir.join("blocked"), "file").unwrap();
            for s in st.all() {
                broken.add(s.chat_id, s.name.clone(), s.url.clone());
            }
            broken
        };
        let store = Arc::new(Mutex::new(st));
        let fetcher = Arc::new(
            Fetcher::new(&[])
                .unwrap()
                .with_page_delay(Duration::ZERO)
                .with_origin(&origin)
                .unwrap(),
        );
        let api = Arc::new(MockApi::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let handle = {
            let (api, store, fetcher, stop) =
                (api.clone(), store.clone(), fetcher.clone(), stop.clone());
            let cfg = cfg.clone();
            std::thread::spawn(move || run(api, store, fetcher, rx, stop, &cfg))
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while api.sent.lock().unwrap().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "no baselines");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(store.lock().unwrap().all().iter().all(|s| s.initialized));

        // Let the wait loop time out at least once.
        std::thread::sleep(Duration::from_millis(1200));
        // Break the config by hand, force a re-check: the last good config is
        // kept, so the search is still checked.
        std::fs::write(&cfg_path, "max_pages = \"many\"").unwrap();
        store.lock().unwrap().get_mut(1).unwrap().last_check = 0;
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.lock().unwrap().all()[0].last_check == 0 {
            assert!(std::time::Instant::now() < deadline, "not re-checked");
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        // Stops promptly while waiting; exits when the channel closes too.
        let (tx2, rx2) = mpsc::channel::<()>();
        drop(tx2);
        let empty = Arc::new(Mutex::new(Store::load(&dir.join("none.json")).unwrap()));
        run(
            api.clone(),
            empty.clone(),
            fetcher.clone(),
            rx2,
            Arc::new(AtomicBool::new(false)),
            &dir.join("config.toml"),
        );
        // Already stopped: returns without checking anything.
        let (_tx3, rx3) = mpsc::channel::<()>();
        let two = Arc::new(Mutex::new(Store::load(&dir.join("two.json")).unwrap()));
        two.lock()
            .unwrap()
            .add(5, "x".into(), "https://www.airbnb.com/s/x/homes".into());
        two.lock()
            .unwrap()
            .add(5, "y".into(), "https://www.airbnb.com/s/y/homes".into());
        let stopped = Arc::new(AtomicBool::new(true));
        run(
            api.clone(),
            two.clone(),
            fetcher.clone(),
            rx3,
            stopped,
            &cfg_path,
        );
        assert!(two.lock().unwrap().all().iter().all(|s| !s.initialized));

        // Stop requested while the first search is being checked: the second
        // one is left for next time and the loop exits.
        std::fs::write(&cfg_path, "allowed_users = [5]").unwrap();
        let stop_mid = Arc::new(AtomicBool::new(false));
        let slow = TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_origin = format!("http://{}", slow.local_addr().unwrap());
        {
            let stop_mid = stop_mid.clone();
            std::thread::spawn(move || {
                for stream in slow.incoming() {
                    let mut stream = stream.unwrap();
                    let _ = stream.read(&mut [0u8; 4096]);
                    stop_mid.store(true, Ordering::Relaxed);
                    let body = "<script type=\"application/json\">{\"staysSearch\":{}}</script>";
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
        }
        let slow_fetcher = Arc::new(
            Fetcher::new(&[])
                .unwrap()
                .with_origin(&slow_origin)
                .unwrap(),
        );
        let (_tx4, rx4) = mpsc::channel::<()>();
        run(api, two.clone(), slow_fetcher, rx4, stop_mid, &cfg_path);
        let inits: Vec<bool> = two
            .lock()
            .unwrap()
            .all()
            .iter()
            .map(|s| s.initialized)
            .collect();
        assert_eq!(inits, [true, false]);
    }
}
