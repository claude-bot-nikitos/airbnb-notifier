//! End-to-end: runs the real binary against a fake Telegram Bot API and a fake
//! Airbnb, and walks through what a user does in the chat.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use airbnb_notifier::config::Config;
use common::{FakeAirbnb, FakeTelegram, TOKEN, temp_dir};
use serde_json::Value;

const ME: i64 = 111;
const STRANGER: i64 = 999;
const LINK: &str = "https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2&checkin=2026-11-10&checkout=2026-11-15";

fn bin() -> Command {
    common::bin_command()
}

struct Running {
    child: Child,
    log: PathBuf,
}

impl Running {
    fn start(config: &Path) -> Running {
        Running::start_with_token(config, None)
    }

    fn start_with_token(config: &Path, env_token: Option<&str>) -> Running {
        let log = config.with_file_name(format!("bot-{}.log", crate::common::temp_suffix()));
        let mut cmd = bin();
        match env_token {
            Some(t) => cmd.env("TELEGRAM_BOT_TOKEN", t),
            None => cmd.env_remove("TELEGRAM_BOT_TOKEN"),
        };
        let child = cmd
            .args(["--config", config.to_str().unwrap(), "run"])
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Running { child, log }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGTERM and expect a clean, prompt exit.
    fn stop(mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "exit {status:?}\n{}", self.logs());
                assert!(self.logs().contains("stopping"));
                return;
            }
            assert!(
                Instant::now() < deadline,
                "bot did not stop\n{}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn write_config(dir: &Path, tg: &FakeTelegram, airbnb: &FakeAirbnb) -> PathBuf {
    let path = dir.join("config.toml");
    Config {
        bot_token: TOKEN.into(),
        allowed_users: vec![ME],
        page_delay_ms: 0,
        telegram_api_url: tg.api_url(),
        airbnb_origin: Some(airbnb.server.url()),
        ..Config::default()
    }
    .save(&path)
    .unwrap();
    path
}

fn text(v: &Value) -> &str {
    v["text"].as_str().or(v["caption"].as_str()).unwrap_or("")
}

fn photos_to(tg: &FakeTelegram, skip: usize, chat: i64) -> Vec<String> {
    tg.calls()
        .into_iter()
        .skip(skip)
        .filter(|(m, b)| m == "sendPhoto" && b["chat_id"] == chat)
        .map(|(_, b)| text(&b).to_string())
        .collect()
}

fn cli(config: &Path, args: &[&str]) -> String {
    let out = bin()
        .args(["--config", config.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn full_user_journey() {
    let dir = temp_dir("journey");
    let tg = FakeTelegram::start();
    let airbnb = FakeAirbnb::start(&[1, 2, 3, 4, 5], 2); // 3 pages
    let config = write_config(&dir, &tg, &airbnb);
    let bot = Running::start(&config);

    // Commands menu is registered at startup.
    tg.wait_for(0, "setMyCommands", |m, _| m == "setMyCommands");

    // A stranger is turned away and told their ID.
    let mark = tg.calls().len();
    tg.user_says(STRANGER, "/start");
    let reply = tg.wait_message(mark, STRANGER, "not allowed");
    assert!(text(&reply).contains("user add 999"));

    // The owner gets help.
    let mark = tg.calls().len();
    tg.user_says(ME, "/start");
    tg.wait_message(mark, ME, "Airbnb notifier");

    // Share a search link → search created, current listings become the baseline.
    let mark = tg.calls().len();
    tg.user_says(ME, &format!("Check this out {LINK}"));
    tg.wait_message(
        mark,
        ME,
        "Search <b>#1</b> created: <b>Lisbon, Portugal</b>",
    );
    tg.wait_message(mark, ME, "5 current listings saved");
    assert!(
        photos_to(&tg, mark, ME).is_empty(),
        "baseline must be silent"
    );
    // All 3 pages were requested, with the user's filters.
    let reqs = airbnb.server.requests();
    assert_eq!(reqs.len(), 3);
    assert!(reqs.iter().all(|r| r.target.contains("checkin=2026-11-10")));

    // Name it.
    let mark = tg.calls().len();
    tg.user_says(ME, "Lisbon trip");
    tg.wait_message(mark, ME, "now called <b>Lisbon trip</b>");

    // A new flat appears → one notification with photo, price and link.
    airbnb.add(42);
    let mark = tg.calls().len();
    tg.user_says(ME, "/check");
    tg.wait_message(mark, ME, "Checking 1 search");
    let (_, photo) = tg.wait_for(mark, "photo for 42", |m, b| {
        m == "sendPhoto" && text(b).contains("/rooms/42")
    });
    assert_eq!(photo["chat_id"], ME);
    assert_eq!(photo["photo"], "https://a0.muscache.com/im/pictures/42.jpg");
    let caption = text(&photo);
    assert!(caption.contains("New in “Lisbon trip”"), "{caption}");
    assert!(caption.contains("<b>Flat 42</b>"), "{caption}");
    assert!(caption.contains("€142 for 5 nights"), "{caption}");
    assert!(caption.contains("10–15 Nov"), "{caption}");
    assert!(!caption.contains("kilometres"), "{caption}");
    assert!(
        caption.contains("rooms/42?adults=2&amp;check_in=2026-11-10&amp;check_out=2026-11-15"),
        "{caption}"
    );
    assert_eq!(photos_to(&tg, mark, ME).len(), 1, "only the new listing");

    // /list shows the search with its buttons.
    let mark = tg.calls().len();
    tg.user_says(ME, "/list");
    let card = tg.wait_message(mark, ME, "#1 Lisbon trip");
    assert!(text(&card).contains("6 listings known"));
    let buttons: Vec<&str> = card["reply_markup"]["inline_keyboard"][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["callback_data"].as_str().unwrap())
        .collect();
    assert_eq!(buttons, ["p:1", "n:1", "d:1"]);

    // Pause with the button: the card updates, and checks are skipped.
    let mark = tg.calls().len();
    tg.user_taps(ME, "p:1");
    tg.wait_for(mark, "card edited to paused", |m, b| {
        m == "editMessageText" && text(b).contains("paused") && b["message_id"] == 77
    });
    tg.wait_for(mark, "toast", |m, b| {
        m == "answerCallbackQuery" && b["text"] == "Paused"
    });
    airbnb.add(43);
    let mark = tg.calls().len();
    tg.user_says(ME, "/check");
    tg.wait_message(mark, ME, "No active searches");

    // Resume → checked right away → the listing added while paused arrives.
    let mark = tg.calls().len();
    tg.user_taps(ME, "r:1");
    tg.wait_for(mark, "photo for 43", |m, b| {
        m == "sendPhoto" && text(b).contains("/rooms/43")
    });

    // Rename with the button.
    let mark = tg.calls().len();
    tg.user_taps(ME, "n:1");
    tg.wait_message(mark, ME, "Send me the new name");
    tg.user_says(ME, "Lisbon Nov");
    tg.wait_message(mark, ME, "now called <b>Lisbon Nov</b>");

    // Airbnb starts blocking: silent twice, then one warning.
    *airbnb.fail_with.lock().unwrap() = Some(403);
    let mark = tg.calls().len();
    for i in 1..=3 {
        tg.user_says(ME, "/check");
        wait_until(|| failures(&dir) == i);
    }
    let warning = tg.wait_message(mark, ME, "keeps failing");
    assert!(text(&warning).contains("HTTP 403"));
    let warnings = tg
        .calls()
        .into_iter()
        .skip(mark)
        .filter(|(_, b)| text(b).contains("keeps failing"))
        .count();
    assert_eq!(warnings, 1);

    // Recovery is announced, with anything new since.
    *airbnb.fail_with.lock().unwrap() = None;
    airbnb.add(44);
    let mark = tg.calls().len();
    tg.user_says(ME, "/check");
    tg.wait_message(mark, ME, "works again");
    tg.wait_for(mark, "photo for 44", |m, b| {
        m == "sendPhoto" && text(b).contains("/rooms/44")
    });

    // A second search, then delete it with confirmation.
    let mark = tg.calls().len();
    tg.user_says(ME, "https://www.airbnb.com/s/Porto/homes?adults=1");
    tg.wait_message(mark, ME, "Search <b>#2</b> created");
    tg.wait_message(mark, ME, "#2 Porto</b> is live");
    let mark = tg.calls().len();
    tg.user_taps(ME, "d:2");
    tg.wait_for(mark, "confirmation", |m, b| {
        m == "editMessageText" && text(b).contains("Delete search <b>#2</b>?")
    });
    tg.user_taps(ME, "D:2");
    tg.wait_for(mark, "deleted", |m, b| {
        m == "editMessageText" && text(b).contains("Deleted <b>#2 Porto</b>")
    });

    // Revoking access from the CLI works without a restart.
    assert!(cli(&config, &["user", "remove", "111"]).contains("Removed 111"));
    let mark = tg.calls().len();
    tg.user_says(ME, "/list");
    tg.wait_message(mark, ME, "not allowed");
    assert!(cli(&config, &["user", "add", "111"]).contains("Allowed 111"));

    bot.stop();

    // State survived on disk.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("searches.json")).unwrap()).unwrap();
    let searches = saved["searches"].as_array().unwrap();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0]["name"], "Lisbon Nov");
    assert_eq!(searches[0]["seen"].as_array().unwrap().len(), 8);

    // After a restart nothing is re-sent; only genuinely new listings are.
    let bot = Running::start(&config);
    airbnb.add(45);
    let mark = tg.calls().len();
    tg.user_says(ME, "/check");
    tg.wait_for(mark, "photo for 45", |m, b| {
        m == "sendPhoto" && text(b).contains("/rooms/45")
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(photos_to(&tg, mark, ME).len(), 1);
    bot.stop();
}

#[test]
fn many_new_listings_are_capped_and_summarized() {
    let dir = temp_dir("many");
    let tg = FakeTelegram::start();
    let airbnb = FakeAirbnb::start(&[1], 50);
    let config = write_config(&dir, &tg, &airbnb);
    let bot = Running::start(&config);

    let mark = tg.calls().len();
    tg.user_says(ME, LINK);
    tg.wait_message(mark, ME, "1 current listings saved");

    for id in 100..115 {
        airbnb.add(id);
    }
    let mark = tg.calls().len();
    tg.user_says(ME, "/check");
    tg.wait_message(mark, ME, "and 5 more new listing(s)");
    assert_eq!(photos_to(&tg, mark, ME).len(), 10);
    bot.stop();
}

#[test]
fn users_only_see_their_own_searches() {
    let dir = temp_dir("isolation");
    let tg = FakeTelegram::start();
    let airbnb = FakeAirbnb::start(&[1], 10);
    let config = write_config(&dir, &tg, &airbnb);
    cli(&config, &["user", "add", "222"]);
    let bot = Running::start(&config);

    let mark = tg.calls().len();
    tg.user_says(ME, LINK);
    tg.wait_message(mark, ME, "is live");

    let mark = tg.calls().len();
    tg.user_says(222, "/list");
    tg.wait_message(mark, 222, "no searches yet");
    tg.user_says(222, "/delete 1");
    tg.wait_message(mark, 222, "No search #1");
    tg.user_taps(222, "p:1");
    tg.wait_for(mark, "other user's tap handled", |m, b| {
        m == "editMessageText" && text(b).contains("no longer exists")
    });

    // Still intact and active for its owner.
    let mark = tg.calls().len();
    tg.user_says(ME, "/list");
    let card = tg.wait_message(mark, ME, "#1 Lisbon, Portugal");
    assert!(text(&card).contains("active"));
    bot.stop();
}

#[test]
fn bad_links_are_rejected_in_chat() {
    let dir = temp_dir("badlinks");
    let tg = FakeTelegram::start();
    let airbnb = FakeAirbnb::start(&[1], 10);
    let config = write_config(&dir, &tg, &airbnb);
    let bot = Running::start(&config);

    let mark = tg.calls().len();
    tg.user_says(ME, &format!("{}/rooms/123", airbnb.server.url()));
    tg.wait_message(mark, ME, "isn't an Airbnb search link");
    tg.user_says(ME, "just chatting");
    tg.wait_message(mark, ME, "Send me an Airbnb search link");
    tg.user_says(ME, "/nonsense");
    tg.wait_message(mark, ME, "/list — your searches");
    bot.stop();
    assert!(!dir.join("searches.json").exists());
}

#[test]
fn survives_telegram_outages() {
    let dir = temp_dir("outage");
    let airbnb = FakeAirbnb::start(&[1], 10);
    // Telegram unreachable: the bot must keep retrying, not exit. The token
    // comes from the environment instead of the file.
    let path = dir.join("config.toml");
    Config {
        allowed_users: vec![ME],
        page_delay_ms: 0,
        telegram_api_url: "http://127.0.0.1:1".into(),
        airbnb_origin: Some(airbnb.server.url()),
        ..Config::default()
    }
    .save(&path)
    .unwrap();
    let bot = Running::start_with_token(&path, Some(TOKEN));
    wait_until(|| bot.logs().matches("getUpdates failed").count() >= 2);
    assert!(!bot.logs().contains(TOKEN), "token leaked into logs");
    bot.stop();
}

#[test]
fn run_refuses_to_start_without_a_token() {
    let dir = temp_dir("notoken");
    let path = dir.join("config.toml");
    Config::default().save(&path).unwrap();
    let out = bin()
        .args(["--config", path.to_str().unwrap(), "run"])
        .env_remove("TELEGRAM_BOT_TOKEN")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bot_token is empty"));
}

/// Consecutive failures of search #1, as saved on disk.
fn failures(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join("searches.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["searches"][0]["failures"].as_u64())
        .unwrap_or(0)
}

fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}
