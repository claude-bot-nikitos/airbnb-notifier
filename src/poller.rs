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
    while !stop.load(Ordering::Relaxed) {
        let cfg = match Config::load(config_path) {
            Ok(c) => c,
            Err(e) => {
                log::error!("config reload failed: {e:#}");
                Config::default()
            }
        };
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
                std::thread::sleep(Duration::from_millis(2000 + crate::util::jitter(3000)));
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
        match o {
            Outcome::New {
                listings, chat_id, ..
            } => {
                assert_eq!(chat_id, 5);
                assert_eq!(listings.iter().map(|l| l.id).collect::<Vec<_>>(), vec![3]);
            }
            other => panic!("unexpected {other:?}"),
        }
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
            .map(|_| match apply(&mut st, 1, Err(anyhow::anyhow!("boom"))) {
                Outcome::Failed { alert, .. } => alert,
                _ => panic!(),
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
}
