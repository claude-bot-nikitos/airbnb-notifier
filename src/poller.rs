//! Periodically re-runs every active search and notifies about new listings.
//!
//! A listing is alerted when it shows up for the searched dates (Airbnb pads
//! thin results with places free only on other dates; those are ignored), its
//! live calendar confirms the stay can be booked, and it was not alerted
//! before - or it disappeared (booked) and came back (a cancellation).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::airbnb::{self, Fetcher, Listing, Scan, Verdict};
use crate::bot::Api;
use crate::config::Config;
use crate::store::{SCHEMA, Search, Store};
use crate::util::{esc, now_ts, truncate};

/// Per-check cap on individual listing messages; the rest are summarized.
const MAX_MESSAGES_PER_CHECK: usize = 10;
/// Consecutive failures before the owner is told something is wrong.
const FAILURES_BEFORE_ALERT: u32 = 3;
/// A seen listing must be missing at least this long before its return is
/// alerted, so that results reshuffling between two checks cause no noise.
pub const AGAIN_AFTER_SECS: i64 = 30 * 60;
/// Calendar checks per search and check; more candidates are alerted unchecked.
/// Verdicts are not cached: a place listed but booked is re-checked every
/// time, so a cancellation is caught on the very next check.
const MAX_VERIFY_PER_CHECK: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub listing: Listing,
    /// It was alerted before, went missing, and is back.
    pub again: bool,
    /// `Some(true)`: calendar confirms the stay; `Some(false)`: the check
    /// failed; `None`: the search has no fixed dates to check.
    pub verified: Option<bool>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Search was deleted or paused while it was being fetched.
    Gone,
    Baseline {
        chat_id: i64,
        id: u32,
        name: String,
        /// Listings for the searched dates.
        exact: usize,
        /// Listings Airbnb offers only for other dates.
        other_dates: usize,
    },
    New {
        chat_id: i64,
        name: String,
        url: String,
        alerts: Vec<Alert>,
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
        url: String,
        alerts: Vec<Alert>,
    },
}

/// Whether a listing present for the searched dates deserves an alert:
/// `Some(false)` first time, `Some(true)` back after being gone, `None` no.
fn alert_kind(s: &Search, id: u64, now: i64) -> Option<bool> {
    if !s.seen.contains(&id) {
        return Some(false);
    }
    match s.gone.get(&id) {
        Some(&since) if now - since >= AGAIN_AFTER_SECS => Some(true),
        _ => None,
    }
}

fn baselining(s: &Search) -> bool {
    !s.initialized || s.schema < SCHEMA
}

/// Listing ids that would be alerted if their calendar allows it.
pub fn candidates(s: &Search, scan: &Scan, now: i64) -> Vec<u64> {
    if baselining(s) {
        return vec![];
    }
    let wanted = airbnb::search_dates(&s.url);
    scan.listings
        .iter()
        .filter(|l| l.matches_dates(wanted.as_ref()) && alert_kind(s, l.id, now).is_some())
        .map(|l| l.id)
        .collect()
}

/// Records a fetch result (plus calendar verdicts for the candidates) in the
/// store and decides what to tell the user.
pub fn apply(
    store: &mut Store,
    id: u32,
    result: Result<Scan>,
    verdicts: &HashMap<u64, Verdict>,
) -> Outcome {
    let now = now_ts();
    let Some(s) = store.get_mut(id) else {
        return Outcome::Gone;
    };
    if s.paused {
        return Outcome::Gone;
    }
    let scan = match result {
        Err(e) => {
            s.failures += 1;
            s.last_error = Some(format!("{e:#}"));
            // Retry on the normal schedule rather than hammering Airbnb.
            s.last_check = now;
            return Outcome::Failed {
                chat_id: s.chat_id,
                id,
                name: s.name.clone(),
                error: format!("{e:#}"),
                alert: s.failures == FAILURES_BEFORE_ALERT,
            };
        }
        Ok(scan) => scan,
    };
    let was_alerting = s.failures >= FAILURES_BEFORE_ALERT;
    s.failures = 0;
    s.last_error = None;
    s.last_check = now;

    let wanted = airbnb::search_dates(&s.url);
    let (exact, other): (Vec<Listing>, Vec<Listing>) = scan
        .listings
        .into_iter()
        .partition(|l| l.matches_dates(wanted.as_ref()));

    if baselining(s) {
        let first = !s.initialized;
        s.initialized = true;
        s.schema = SCHEMA;
        s.seen = exact.iter().map(|l| l.id).collect();
        s.gone.clear();
        if first {
            return Outcome::Baseline {
                chat_id: s.chat_id,
                id,
                name: s.name.clone(),
                exact: exact.len(),
                other_dates: other.len(),
            };
        }
        // Upgraded from an older version: re-baseline without alerts.
        return Outcome::New {
            chat_id: s.chat_id,
            name: s.name.clone(),
            url: s.url.clone(),
            alerts: vec![],
        };
    }

    let present: HashSet<u64> = exact.iter().map(|l| l.id).collect();
    let mut alerts = Vec::new();
    for l in exact {
        let Some(again) = alert_kind(s, l.id, now) else {
            // Known, or back too soon to count as a new chance.
            s.gone.remove(&l.id);
            continue;
        };
        let verified = match verdicts.get(&l.id) {
            None => None,
            Some(Verdict::Available) => Some(true),
            Some(Verdict::Unknown(_)) => Some(false),
            // Listed but not bookable: stay quiet and look again next time.
            Some(Verdict::Unavailable(_)) => continue,
        };
        s.seen.insert(l.id);
        s.gone.remove(&l.id);
        alerts.push(Alert {
            listing: l,
            again,
            verified,
        });
    }
    // Only a scan that read every page proves a listing is gone.
    if scan.complete {
        for id in &s.seen {
            if !present.contains(id) {
                s.gone.entry(*id).or_insert(now);
            }
        }
    }

    if was_alerting {
        return Outcome::Recovered {
            chat_id: s.chat_id,
            id,
            name: s.name.clone(),
            url: s.url.clone(),
            alerts,
        };
    }
    Outcome::New {
        chat_id: s.chat_id,
        name: s.name.clone(),
        url: s.url.clone(),
        alerts,
    }
}

pub fn listing_caption(search_name: &str, search_url: &str, alert: &Alert) -> String {
    let l = &alert.listing;
    let header = if alert.again {
        format!("🔁 <b>Available again in “{}”</b>", esc(search_name))
    } else {
        format!("🏠 <b>New in “{}”</b>", esc(search_name))
    };
    let mut lines = vec![header];
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
    if let Some(stay) = airbnb::search_dates(search_url) {
        lines.push(format!("📅 {}", esc(&airbnb::stay_label(&stay))));
    }
    match alert.verified {
        Some(true) => lines.push("✅ Free for your dates — checked just now".into()),
        Some(false) => lines.push("⚠️ Couldn't confirm availability — open it quickly".into()),
        None => {}
    }
    lines.push(format!(
        "<a href=\"{}\">Open on Airbnb</a>",
        esc(&airbnb::listing_url(search_url, l.id))
    ));
    lines.join("\n")
}

fn notify(api: &dyn Api, chat_id: i64, name: &str, url: &str, alerts: &[Alert]) {
    for a in alerts.iter().take(MAX_MESSAGES_PER_CHECK) {
        let caption = listing_caption(name, url, a);
        // A big button: the fastest way to the booking page on a phone.
        let kb = vec![vec![(
            "🔗 Open in Airbnb".to_string(),
            airbnb::listing_url(url, a.listing.id),
        )]];
        let sent_photo = match &a.listing.picture {
            Some(p) => api.send_photo(chat_id, p, &caption, Some(&kb)).is_ok(),
            None => false,
        };
        if !sent_photo && let Err(e) = api.send(chat_id, &caption, Some(&kb)) {
            log::error!("notify {chat_id} failed: {e:#}");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if alerts.len() > MAX_MESSAGES_PER_CHECK {
        let more = alerts.len() - MAX_MESSAGES_PER_CHECK;
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
            exact,
            other_dates,
        } => {
            let mut text = format!(
                "✅ <b>#{id} {}</b> is live: {exact} listing(s) for your dates saved.",
                esc(name)
            );
            if *other_dates > 0 {
                text.push_str(&format!(
                    " {other_dates} more are only free on other dates — I ignore those \
                     unless they open up for yours."
                ));
            }
            text.push_str(" I'll message you when new ones appear.");
            say(*chat_id, &text)
        }
        Outcome::New {
            chat_id,
            name,
            url,
            alerts,
        } => notify(api, *chat_id, name, url, alerts),
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
            url,
            alerts,
        } => {
            say(
                *chat_id,
                &format!("✅ Search <b>#{id} {}</b> works again.", esc(name)),
            );
            notify(api, *chat_id, name, url, alerts);
        }
    }
}

/// Checks the live calendar of every alert candidate of a fresh scan.
fn verify_candidates(
    fetcher: &Fetcher,
    store: &Mutex<Store>,
    search_id: u32,
    scan: &Scan,
) -> HashMap<u64, Verdict> {
    let mut verdicts = HashMap::new();
    let Some(s) = store.lock().unwrap().get(search_id).cloned() else {
        return verdicts;
    };
    let Some(stay) = airbnb::search_dates(&s.url) else {
        return verdicts;
    };
    for (i, id) in candidates(&s, scan, now_ts()).into_iter().enumerate() {
        let verdict = if i >= MAX_VERIFY_PER_CHECK {
            Verdict::Unknown("too many new listings to check".into())
        } else {
            let v = fetcher.verify(&s.url, id, &stay);
            match &v {
                Verdict::Available => log::info!("#{search_id}: listing {id} is free"),
                Verdict::Unavailable(why) => {
                    log::info!("#{search_id}: listing {id} is listed but not bookable: {why}")
                }
                Verdict::Unknown(why) => {
                    log::warn!("#{search_id}: could not check listing {id}: {why}")
                }
            }
            v
        };
        verdicts.insert(id, verdict);
    }
    verdicts
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
        let now = now_ts() - crate::util::jitter(30) as i64;

        let mut due = store.lock().unwrap().due(now, interval);
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
            let verdicts = match &result {
                Ok(scan) => {
                    let wanted = airbnb::search_dates(&s.url);
                    let exact = scan
                        .listings
                        .iter()
                        .filter(|l| l.matches_dates(wanted.as_ref()))
                        .count();
                    log::info!(
                        "#{}: {} listings on the pages scanned, {exact} for the searched dates",
                        s.id,
                        scan.listings.len()
                    );
                    verify_candidates(&fetcher, &store, s.id, scan)
                }
                Err(e) => {
                    log::warn!("#{} failed: {e:#}", s.id);
                    HashMap::new()
                }
            };
            let outcome = {
                let mut st = store.lock().unwrap();
                let o = apply(&mut st, s.id, result, &verdicts);
                if let Err(e) = st.save() {
                    log::error!("saving searches failed: {e:#}");
                }
                o
            };
            if let Outcome::New { alerts, .. } = &outcome
                && !alerts.is_empty()
            {
                log::info!("#{}: {} alert(s)", s.id, alerts.len());
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

    const DATED: &str =
        "https://www.airbnb.com/s/Lisbon/homes?adults=2&checkin=2026-11-10&checkout=2026-11-15";

    fn stay() -> Option<(String, String)> {
        Some(("2026-11-10".into(), "2026-11-15".into()))
    }

    fn listing(id: u64) -> Listing {
        Listing {
            id,
            name: format!("L{id}"),
            picture: Some("https://p/x.jpg".into()),
            dates: stay(),
            ..Default::default()
        }
    }

    fn other_dates(id: u64) -> Listing {
        Listing {
            dates: Some(("2026-12-01".into(), "2026-12-06".into())),
            ..listing(id)
        }
    }

    fn scan(listings: Vec<Listing>) -> Result<Scan> {
        Ok(Scan {
            listings,
            complete: true,
        })
    }

    fn partial(listings: Vec<Listing>) -> Result<Scan> {
        Ok(Scan {
            listings,
            complete: false,
        })
    }

    fn none() -> HashMap<u64, Verdict> {
        HashMap::new()
    }

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("abn-poll-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = Store::load(&dir.join("s.json")).unwrap();
        st.add(5, "Lisbon".into(), DATED.into());
        st
    }

    fn alerts(o: Outcome) -> Vec<(u64, bool, Option<bool>)> {
        match o {
            Outcome::New { alerts, .. } | Outcome::Recovered { alerts, .. } => alerts
                .into_iter()
                .map(|a| (a.listing.id, a.again, a.verified))
                .collect(),
            other => unreachable!("expected alerts, got {other:?}"),
        }
    }

    #[test]
    fn baseline_counts_only_listings_for_the_searched_dates() {
        let mut st = store("baseline");
        let o = apply(
            &mut st,
            1,
            scan(vec![listing(1), listing(2), other_dates(3)]),
            &none(),
        );
        assert!(matches!(
            o,
            Outcome::Baseline {
                exact: 2,
                other_dates: 1,
                chat_id: 5,
                ..
            }
        ));
        let s = &st.all()[0];
        assert_eq!(s.seen.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(s.schema, SCHEMA);
        // A second check with nothing new says nothing.
        let o = apply(
            &mut st,
            1,
            scan(vec![listing(2), listing(1), other_dates(3)]),
            &none(),
        );
        assert!(alerts(o).is_empty());
    }

    #[test]
    fn other_dates_are_ignored_until_they_open_up_for_ours() {
        let mut st = store("otherdates");
        apply(&mut st, 1, scan(vec![listing(1)]), &none());
        let o = apply(&mut st, 1, scan(vec![listing(1), other_dates(9)]), &none());
        assert!(
            alerts(o).is_empty(),
            "padding for other dates is not an alert"
        );
        let mut now_free = other_dates(9);
        now_free.dates = stay();
        let o = apply(&mut st, 1, scan(vec![listing(1), now_free]), &none());
        assert_eq!(alerts(o), vec![(9, false, None)]);
    }

    #[test]
    fn results_without_dates_count_as_matching() {
        let mut st = store("nodates");
        apply(&mut st, 1, scan(vec![listing(1)]), &none());
        let undated = Listing {
            dates: None,
            ..listing(2)
        };
        assert_eq!(
            alerts(apply(&mut st, 1, scan(vec![undated]), &none())),
            vec![(2, false, None)]
        );
    }

    #[test]
    fn calendar_verdicts_decide() {
        let mut st = store("verdicts");
        apply(&mut st, 1, scan(vec![]), &none());
        let booked = HashMap::from([(4, Verdict::Unavailable("2026-11-11 is booked".into()))]);
        let o = apply(&mut st, 1, scan(vec![listing(4)]), &booked);
        assert!(alerts(o).is_empty(), "listed but booked: no alert");
        assert!(!st.all()[0].seen.contains(&4), "keeps watching it");

        let free = HashMap::from([(4, Verdict::Available)]);
        let o = apply(&mut st, 1, scan(vec![listing(4)]), &free);
        assert_eq!(alerts(o), vec![(4, false, Some(true))]);

        let unknown = HashMap::from([(5, Verdict::Unknown("HTTP 500".into()))]);
        let o = apply(&mut st, 1, scan(vec![listing(4), listing(5)]), &unknown);
        assert_eq!(alerts(o), vec![(5, false, Some(false))], "fail open");
    }

    #[test]
    fn booked_and_back_is_alerted_again() {
        let mut st = store("again");
        apply(&mut st, 1, scan(vec![listing(1), listing(2)]), &none());
        // 1 disappears from a complete scan → gone.
        apply(&mut st, 1, scan(vec![listing(2)]), &none());
        assert!(st.all()[0].gone.contains_key(&1));
        // Back right away (results reshuffled): no alert, no longer gone.
        let o = apply(&mut st, 1, scan(vec![listing(1), listing(2)]), &none());
        assert!(alerts(o).is_empty());
        assert!(st.all()[0].gone.is_empty());
        // Gone for longer than AGAIN_AFTER_SECS, then back: alert again.
        apply(&mut st, 1, scan(vec![listing(2)]), &none());
        *st.get_mut(1).unwrap().gone.get_mut(&1).unwrap() -= AGAIN_AFTER_SECS;
        assert_eq!(
            candidates(
                st.get(1).unwrap(),
                &scan(vec![listing(1)]).unwrap(),
                now_ts()
            ),
            vec![1]
        );
        let free = HashMap::from([(1, Verdict::Available)]);
        let o = apply(&mut st, 1, scan(vec![listing(1), listing(2)]), &free);
        assert_eq!(alerts(o), vec![(1, true, Some(true))]);
        assert!(st.all()[0].gone.is_empty());
    }

    #[test]
    fn incomplete_scans_do_not_mark_listings_gone() {
        let mut st = store("partial");
        apply(&mut st, 1, scan(vec![listing(1), listing(2)]), &none());
        apply(&mut st, 1, partial(vec![listing(2)]), &none());
        assert!(st.all()[0].gone.is_empty());
        // Going to other dates counts as gone in a complete scan.
        apply(&mut st, 1, scan(vec![other_dates(1), listing(2)]), &none());
        assert!(st.all()[0].gone.contains_key(&1));
    }

    #[test]
    fn candidates_are_new_or_returning_listings_for_our_dates() {
        let mut st = store("cands");
        let first = scan(vec![listing(1)]).unwrap();
        assert!(
            candidates(st.get(1).unwrap(), &first, 0).is_empty(),
            "baseline"
        );
        apply(&mut st, 1, Ok(first), &none());
        let later = scan(vec![listing(1), listing(2), other_dates(3)]).unwrap();
        assert_eq!(candidates(st.get(1).unwrap(), &later, now_ts()), vec![2]);
    }

    #[test]
    fn searches_from_older_versions_rebaseline_silently() {
        let mut st = store("migrate");
        {
            let s = st.get_mut(1).unwrap();
            s.initialized = true;
            s.schema = 0;
            s.seen = [1, 2, 3].into();
        }
        let o = apply(
            &mut st,
            1,
            scan(vec![listing(1), other_dates(3), listing(7)]),
            &none(),
        );
        assert!(alerts(o).is_empty(), "no burst of alerts after upgrading");
        let s = &st.all()[0];
        assert_eq!(s.seen.iter().copied().collect::<Vec<_>>(), vec![1, 7]);
        assert_eq!(s.schema, SCHEMA);
    }

    #[test]
    fn failed_baseline_stays_uninitialized() {
        let mut st = store("failbase");
        let o = apply(&mut st, 1, Err(anyhow::anyhow!("HTTP 403")), &none());
        assert!(matches!(o, Outcome::Failed { alert: false, .. }));
        assert!(!st.all()[0].initialized);
        assert!(matches!(
            apply(&mut st, 1, scan(vec![listing(1)]), &none()),
            Outcome::Baseline { .. }
        ));
    }

    #[test]
    fn alerts_once_then_recovers() {
        let mut st = store("alert");
        apply(&mut st, 1, scan(vec![listing(1)]), &none());
        let failures: Vec<bool> = (0..5)
            .map(|_| {
                let o = apply(&mut st, 1, Err(anyhow::anyhow!("boom")), &none());
                matches!(o, Outcome::Failed { alert: true, .. })
            })
            .collect();
        assert_eq!(failures, vec![false, false, true, false, false]);
        let o = apply(&mut st, 1, scan(vec![listing(1), listing(9)]), &none());
        assert!(matches!(o, Outcome::Recovered { .. }));
        assert_eq!(alerts(o), vec![(9, false, None)]);
        assert_eq!(st.all()[0].failures, 0);
    }

    #[test]
    fn deleted_or_paused_is_gone() {
        let mut st = store("gone");
        st.get_mut(1).unwrap().paused = true;
        assert_eq!(apply(&mut st, 1, scan(vec![]), &none()), Outcome::Gone);
        assert_eq!(apply(&mut st, 99, scan(vec![]), &none()), Outcome::Gone);
    }

    fn alert(l: Listing, again: bool, verified: Option<bool>) -> Alert {
        Alert {
            listing: l,
            again,
            verified,
        }
    }

    #[test]
    fn report_caps_messages_and_falls_back_to_text() {
        let api = MockApi {
            fail_photos: true,
            ..Default::default()
        };
        let alerts: Vec<Alert> = (1..=12).map(|i| alert(listing(i), false, None)).collect();
        report(
            &api,
            &Outcome::New {
                chat_id: 5,
                name: "L".into(),
                url: DATED.into(),
                alerts,
            },
        );
        let sent = api.sent.lock().unwrap();
        assert_eq!(sent.len(), 11); // 10 listings (text fallback) + summary
        assert!(sent[10].1.contains("2 more"));
        // Every listing message carries an "Open in Airbnb" link button.
        let kbs = api.keyboards.lock().unwrap();
        assert_eq!(kbs.len(), 10);
        assert_eq!(kbs[0][0][0].0, "🔗 Open in Airbnb");
        assert!(
            kbs[0][0][0]
                .1
                .starts_with("https://www.airbnb.com/rooms/1?")
        );
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
            dates: stay(),
        };
        let c = listing_caption("Lisbon", DATED, &alert(l.clone(), false, Some(true)));
        assert!(c.starts_with("🏠 <b>New in “Lisbon”</b>"));
        assert!(c.contains("Sunny &lt;loft&gt;"));
        assert!(c.contains("Apartment in Lisbon"));
        assert!(c.contains("💰 €80 night"));
        assert!(c.contains("📅 10–15 Nov 2026 · 5 nights"));
        assert!(c.contains("✅ Free for your dates"));
        assert!(c.contains("https://www.airbnb.com/rooms/42?adults=2&amp;check_in=2026-11-10"));

        let c = listing_caption("Lisbon", DATED, &alert(l.clone(), true, Some(false)));
        assert!(c.starts_with("🔁 <b>Available again in “Lisbon”</b>"));
        assert!(c.contains("⚠️ Couldn't confirm"));

        let undated = "https://www.airbnb.com/s/Lisbon/homes";
        let c = listing_caption("Lisbon", undated, &alert(l, false, None));
        assert!(!c.contains("📅") && !c.contains("✅") && !c.contains("⚠️"));
    }

    #[test]
    fn report_sends_photos_and_handles_failures() {
        let api = MockApi::default();
        let mut without_photo = listing(2);
        without_photo.picture = None;
        let new = Outcome::New {
            chat_id: 5,
            name: "L".into(),
            url: DATED.into(),
            alerts: vec![
                alert(listing(1), false, Some(true)),
                alert(without_photo, false, None),
            ],
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

        // Baseline text mentions ignored other-date listings only if any.
        for (other, expect) in [(0, false), (3, true)] {
            report(
                &api,
                &Outcome::Baseline {
                    chat_id: 5,
                    id: 1,
                    name: "L".into(),
                    exact: 2,
                    other_dates: other,
                },
            );
            let last = api.sent.lock().unwrap().last().unwrap().1.clone();
            assert!(last.contains("2 listing(s) for your dates"));
            assert_eq!(last.contains("3 more are only free on other dates"), expect);
        }

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
            alerts: (1..=11).map(|i| alert(listing(i), false, None)).collect(),
        };
        report(&down, &many);
    }

    #[test]
    fn caption_uses_title_when_name_is_missing() {
        let l = Listing {
            id: 1,
            title: "Home in Porto".into(),
            ..Default::default()
        };
        let c = listing_caption(
            "P",
            "https://www.airbnb.com/s/P/homes",
            &alert(l, false, None),
        );
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
