//! Live self-tests against the real Airbnb and Telegram, for the machine the
//! bot runs on. Used by `airbnb-notifier selftest`, and `/selftest` and
//! `/test` in the chat.

use anyhow::Result;

use crate::airbnb::{self, Fetcher, Verdict};
use crate::util::{esc, now_ts};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn new(status: Status, name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
        }
    }

    fn icon(&self) -> &'static str {
        match self.status {
            Status::Ok => "✅",
            Status::Warn => "⚠️",
            Status::Fail => "❌",
        }
    }

    pub fn text(&self) -> String {
        format!("{} {}: {}", self.icon(), self.name, self.detail)
    }

    pub fn html(&self) -> String {
        format!(
            "{} <b>{}</b>: {}",
            self.icon(),
            esc(&self.name),
            esc(&self.detail)
        )
    }
}

pub fn passed(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.status != Status::Fail)
}

/// A dated sample search (Lisbon, 3 nights, a month from now) for machines
/// without saved searches yet.
pub fn sample_search_url() -> String {
    let today = now_ts().div_euclid(86_400);
    format!(
        "https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2&checkin={}&checkout={}",
        airbnb::date_string(today + 30),
        airbnb::date_string(today + 33)
    )
}

/// Fetches the first result page of `url` and runs one calendar check.
pub fn check_search(fetcher: &Fetcher, label: &str, url: &str) -> Vec<Check> {
    let search = format!("Airbnb search {label}");
    let scan = match fetcher.fetch_all(url, 1) {
        Ok(scan) => scan,
        Err(e) => return vec![Check::new(Status::Fail, &search, format!("{e:#}"))],
    };
    let wanted = airbnb::search_dates(url);
    let exact: Vec<_> = scan
        .listings
        .iter()
        .filter(|l| l.matches_dates(wanted.as_ref()))
        .collect();
    let mut checks = vec![if scan.listings.is_empty() {
        Check::new(
            Status::Warn,
            &search,
            "0 listings on page 1. If Airbnb shows results for this link in a browser, \
             its page format changed",
        )
    } else {
        Check::new(
            Status::Ok,
            &search,
            format!(
                "{} listings on page 1, {} for your dates",
                scan.listings.len(),
                exact.len()
            ),
        )
    }];
    let calendar = format!("Calendar check {label}");
    let Some(stay) = wanted else {
        checks.push(Check::new(
            Status::Warn,
            &calendar,
            "skipped: the search has no dates",
        ));
        return checks;
    };
    let Some(first) = exact.first() else {
        checks.push(Check::new(
            Status::Warn,
            &calendar,
            "skipped: no listing for your dates on page 1",
        ));
        return checks;
    };
    checks.push(match fetcher.verify(url, first.id, &stay) {
        Verdict::Available => Check::new(
            Status::Ok,
            &calendar,
            format!("works (listing {} is free)", first.id),
        ),
        Verdict::Unavailable(why) => Check::new(
            Status::Ok,
            &calendar,
            format!("works (listing {}: {why})", first.id),
        ),
        Verdict::Unknown(why) => Check::new(
            Status::Fail,
            &calendar,
            format!("{why}. Alerts still arrive but say \"couldn't confirm\""),
        ),
    });
    checks
}

/// One result of a search, as `test` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    pub name: String,
    pub price: String,
    pub rating: String,
    /// "your dates", "FREE", "BOOKED (…)", "UNKNOWN (…)" or "OTHER DATES a..b".
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub url: String,
    pub stay: Option<(String, String)>,
    pub total: usize,
    pub exact: usize,
    pub complete: bool,
    pub rows: Vec<Row>,
}

/// Every result of a search with its status; up to `max_verify` listings for
/// the searched dates get a live calendar check.
pub fn breakdown(fetcher: &Fetcher, url: &str, pages: u32, max_verify: usize) -> Result<Breakdown> {
    let stay = airbnb::search_dates(url);
    let scan = fetcher.fetch_all(url, pages.max(1))?;
    let mut verified = 0;
    let mut rows = Vec::new();
    let mut exact = 0;
    for l in &scan.listings {
        let status = if !l.matches_dates(stay.as_ref()) {
            let (a, b) = l.dates.clone().unwrap_or_default();
            format!("OTHER DATES {a}..{b}")
        } else {
            exact += 1;
            match &stay {
                Some(s) if verified < max_verify => {
                    verified += 1;
                    match fetcher.verify(url, l.id, s) {
                        Verdict::Available => "FREE".to_string(),
                        Verdict::Unavailable(why) => format!("BOOKED ({why})"),
                        Verdict::Unknown(why) => format!("UNKNOWN ({why})"),
                    }
                }
                _ => "your dates".to_string(),
            }
        };
        rows.push(Row {
            id: l.id,
            name: if l.name.is_empty() {
                l.title.clone()
            } else {
                l.name.clone()
            },
            price: l.price.clone(),
            rating: l.rating.clone(),
            status,
        });
    }
    Ok(Breakdown {
        url: url.to_string(),
        stay,
        total: scan.listings.len(),
        exact,
        complete: scan.complete,
        rows,
    })
}

impl Breakdown {
    pub fn summary(&self) -> String {
        format!(
            "{} listings, {} for your dates{}",
            self.total,
            self.exact,
            if self.complete {
                ""
            } else {
                " (more pages not read)"
            }
        )
    }

    /// Chat version: at most `limit` rows, each linking to the listing.
    pub fn html(&self, title: &str, limit: usize) -> String {
        let mut out = format!("🔎 <b>{}</b>\n", esc(title));
        if let Some(stay) = &self.stay {
            out.push_str(&format!("📅 {}\n", esc(&airbnb::stay_label(stay))));
        }
        out.push_str(&format!("{}\n", esc(&self.summary())));
        for r in self.rows.iter().take(limit) {
            let icon = match r.status.split_whitespace().next().unwrap_or("") {
                "FREE" => "🟢",
                "BOOKED" => "🔴",
                "OTHER" => "⚪",
                "UNKNOWN" => "🟡",
                _ => "•",
            };
            out.push_str(&format!(
                "\n{icon} <a href=\"{}\">{}</a> — {}",
                esc(&airbnb::listing_url(&self.url, r.id)),
                esc(&crate::util::truncate(&r.name, 60)),
                esc(&r.status)
            ));
        }
        if self.rows.len() > limit {
            out.push_str(&format!("\n…and {} more", self.rows.len() - limit));
        }
        out.push_str(
            "\n\n🟢 free for your dates · 🔴 listed but booked · ⚪ only other dates · \
             🟡 couldn't check",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, status: &str) -> Row {
        Row {
            id,
            name: format!("Flat <{id}>"),
            price: String::new(),
            rating: String::new(),
            status: status.into(),
        }
    }

    #[test]
    fn breakdown_html() {
        let b = Breakdown {
            url: "https://www.airbnb.com/s/x/homes?checkin=2026-11-10&checkout=2026-11-15".into(),
            stay: Some(("2026-11-10".into(), "2026-11-15".into())),
            total: 5,
            exact: 4,
            complete: false,
            rows: vec![
                row(1, "FREE"),
                row(2, "BOOKED (x)"),
                row(3, "UNKNOWN (y)"),
                row(4, "your dates"),
                row(5, "OTHER DATES a..b"),
            ],
        };
        let h = b.html("#1 A&B", 4);
        assert!(h.starts_with("🔎 <b>#1 A&amp;B</b>\n📅 10–15 Nov 2026 · 5 nights\n"));
        assert!(h.contains("5 listings, 4 for your dates (more pages not read)"));
        assert!(h.contains("🟢 <a href=\"https://www.airbnb.com/rooms/1?check_in=2026-11-10&amp;check_out=2026-11-15\">Flat &lt;1&gt;</a> — FREE"));
        assert!(h.contains("🔴 "), "{h}");
        assert!(h.contains("🟡 "), "{h}");
        assert!(h.contains("• "), "{h}");
        assert!(!h.contains("Flat &lt;5&gt;"), "limited to 4 rows");
        assert!(h.contains("…and 1 more"));
        let flexible = Breakdown {
            stay: None,
            complete: true,
            rows: vec![row(5, "OTHER DATES a..b")],
            ..b
        };
        let h = flexible.html("x", 10);
        assert!(!h.contains("📅") && !h.contains("more pages") && !h.contains("…and"));
        assert!(h.contains("⚪ "));
    }

    #[test]
    fn checks() {
        let ok = Check::new(Status::Ok, "A<b>", "fine");
        let warn = Check::new(Status::Warn, "W", "hmm");
        let fail = Check::new(Status::Fail, "F", "x & y");
        assert_eq!(ok.text(), "✅ A<b>: fine");
        assert_eq!(ok.html(), "✅ <b>A&lt;b&gt;</b>: fine");
        assert_eq!(fail.html(), "❌ <b>F</b>: x &amp; y");
        assert!(passed(&[ok.clone(), warn.clone()]));
        assert!(!passed(&[ok, warn, fail]));
        let url = sample_search_url();
        let stay = airbnb::search_dates(&url).expect("sample search has dates");
        assert!(airbnb::stay_label(&stay).ends_with("3 nights"));
    }
}
