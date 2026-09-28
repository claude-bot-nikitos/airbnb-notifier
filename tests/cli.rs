//! CLI commands, run as the real binary.

mod common;

use std::path::Path;
use std::process::Output;

use airbnb_notifier::config::Config;
use common::{FakeAirbnb, temp_dir};

fn run(config: &Path, args: &[&str]) -> Output {
    common::bin_command()
        .args(["--config", config.to_str().unwrap()])
        .args(args)
        .env_remove("TELEGRAM_BOT_TOKEN")
        .output()
        .unwrap()
}

fn ok(config: &Path, args: &[&str]) -> String {
    let out = run(config, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn fails(config: &Path, args: &[&str]) -> String {
    let out = run(config, args);
    assert!(!out.status.success(), "{args:?} should fail: {out:?}");
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn init_creates_a_private_commented_config_and_keeps_it() {
    let dir = temp_dir("cli-init");
    let cfg = dir.join("sub/config.toml");
    let out = ok(&cfg, &["init"]);
    assert!(out.contains("Created"));
    assert!(out.contains("init --token"));

    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("# Telegram bot token"));
    assert!(text.contains("check_interval_minutes = 15"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    ok(&cfg, &["user", "add", "5"]);
    let out = ok(&cfg, &["init", "--token", " 12:abc "]);
    assert!(out.contains("Updated"));
    assert!(!out.contains("init --token"));
    let c = Config::load_raw(&cfg).unwrap();
    assert_eq!(c.bot_token, "12:abc");
    assert_eq!(c.allowed_users, vec![5], "init keeps existing settings");
}

#[test]
fn manages_users() {
    let dir = temp_dir("cli-users");
    let cfg = dir.join("config.toml");
    assert!(ok(&cfg, &["user", "list"]).contains("No users allowed yet"));
    assert!(ok(&cfg, &["user", "add", "111"]).contains("Allowed 111"));
    assert!(ok(&cfg, &["user", "add", "222"]).contains("Allowed 222"));
    assert!(ok(&cfg, &["user", "add", "111"]).contains("already allowed"));
    assert_eq!(ok(&cfg, &["user", "list"]), "111\n222\n");
    assert!(ok(&cfg, &["user", "remove", "111"]).contains("Removed 111"));
    assert!(fails(&cfg, &["user", "remove", "111"]).contains("not in the list"));
    assert_eq!(ok(&cfg, &["user", "list"]), "222\n");
    // Negative ids (group chats) are accepted.
    ok(&cfg, &["user", "add", "--", "-100123"]);
    assert_eq!(
        Config::load_raw(&cfg).unwrap().allowed_users,
        vec![222, -100123]
    );
}

#[test]
fn config_path_from_env() {
    let dir = temp_dir("cli-env");
    let cfg = dir.join("from-env.toml");
    let out = common::bin_command()
        .args(["user", "add", "7"])
        .env("AIRBNB_NOTIFIER_CONFIG", &cfg)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(Config::load_raw(&cfg).unwrap().allowed_users, vec![7]);
}

#[test]
fn test_command_prints_listings() {
    let dir = temp_dir("cli-test");
    let cfg = dir.join("config.toml");
    let airbnb = FakeAirbnb::start(&[11, 12, 13], 2);
    Config {
        page_delay_ms: 0,
        airbnb_origin: Some(airbnb.server.url()),
        ..Config::default()
    }
    .save(&cfg)
    .unwrap();

    let out = ok(
        &cfg,
        &[
            "test",
            "https://www.airbnb.com/s/Porto--Portugal/homes?adults=2",
            "--pages",
            "5",
        ],
    );
    assert!(out.contains("Search URL: https://www.airbnb.com/s/Porto--Portugal/homes?adults=2"));
    assert!(out.contains("Default name: Porto, Portugal"));
    assert!(out.contains("Found 3 listings"), "{out}");
    assert!(
        out.contains("Flat 13  |  €113 for 5 nights  |  4.9"),
        "{out}"
    );

    // Default is a single page.
    assert!(ok(&cfg, &["test", "https://www.airbnb.com/s/x/homes"]).contains("Found 2 listings"));

    // With dates: other-date padding is labelled, --verify checks calendars.
    airbnb.other_dates.lock().unwrap().push(99);
    airbnb.booked.lock().unwrap().insert(12);
    let dated = "https://www.airbnb.com/s/x/homes?checkin=2026-11-10&checkout=2026-11-15";
    let out = ok(&cfg, &["test", dated, "--pages", "5", "--verify"]);
    assert!(out.contains("Dates: 10–15 Nov 2026 · 5 nights"), "{out}");
    assert!(out.contains("Found 4 listings, 3 for your dates:"), "{out}");
    assert!(out.contains("11  [FREE]"), "{out}");
    assert!(
        out.contains("12  [BOOKED (no check-in on 2026-11-10)]"),
        "{out}"
    );
    assert!(
        out.contains("99  [OTHER DATES 2026-12-01..2026-12-06]"),
        "{out}"
    );
    *airbnb.calendar_down.lock().unwrap() = true;
    let out = ok(&cfg, &["test", dated, "--verify"]);
    assert!(out.contains("11  [UNKNOWN (calendar: HTTP 500)]"), "{out}");
    *airbnb.calendar_down.lock().unwrap() = false;
    let out = ok(&cfg, &["test", dated]);
    assert!(out.contains("(more pages not read)"), "{out}");
    assert!(out.contains("11  [your dates]"), "{out}");
    let out = ok(
        &cfg,
        &["test", "https://www.airbnb.com/s/x/homes", "--pages", "5"],
    );
    assert!(out.contains("Dates: none"), "{out}");

    *airbnb.fail_with.lock().unwrap() = Some(403);
    assert!(fails(&cfg, &["test", "https://www.airbnb.com/s/x/homes"]).contains("HTTP 403"));
}

#[test]
fn broken_config_is_reported() {
    let dir = temp_dir("cli-broken");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "allowed_users = \"oops\"").unwrap();
    assert!(fails(&cfg, &["user", "list"]).contains("parsing"));
    std::fs::write(&cfg, "bot_token = \"x\"\nproxies = [\"nonsense\"]").unwrap();
    assert!(fails(&cfg, &["run"]).contains("bad proxy"));
}

fn selftest_setup(name: &str) -> (std::path::PathBuf, FakeAirbnb, common::FakeTelegram) {
    let dir = temp_dir(name);
    let cfg = dir.join("config.toml");
    let airbnb = FakeAirbnb::start(&[11, 12], 10);
    let tg = common::FakeTelegram::start();
    Config {
        bot_token: common::TOKEN.into(),
        allowed_users: vec![7],
        page_delay_ms: 0,
        telegram_api_url: tg.api_url(),
        airbnb_origin: Some(airbnb.server.url()),
        ..Config::default()
    }
    .save(&cfg)
    .unwrap();
    (cfg, airbnb, tg)
}

#[test]
fn selftest_passes_on_a_working_setup() {
    let (cfg, airbnb, _tg) = selftest_setup("selftest-ok");
    let out = ok(&cfg, &["selftest"]);
    assert!(out.contains("✅ Config"), "{out}");
    assert!(out.contains("✅ Users: 1 allowed"), "{out}");
    assert!(
        out.contains("✅ Telegram: token works, bot is @fake_bot"),
        "{out}"
    );
    assert!(out.contains("✅ Data file"), "{out}");
    // No saved searches yet: a dated sample search is used.
    assert!(
        out.contains("✅ Airbnb search (sample: Lisbon): 2 listings on page 1, 2 for your dates"),
        "{out}"
    );
    assert!(
        out.contains("✅ Calendar check (sample: Lisbon): works (listing 11 is free)"),
        "{out}"
    );
    assert!(out.contains("All good."), "{out}");
    let req = &airbnb.search_requests()[0];
    assert!(
        req.target
            .starts_with("/s/Lisbon--Portugal/homes?adults=2&checkin=20")
    );
}

#[test]
fn selftest_checks_saved_searches_and_fails_loudly() {
    let (cfg, airbnb, _tg) = selftest_setup("selftest-fail");
    std::fs::write(
        cfg.with_file_name("searches.json"),
        r#"{"next_id":3,"searches":[
            {"id":1,"chat_id":7,"name":"Imperia","url":"https://www.airbnb.com/s/Imperia/homes?checkin=2026-11-10&checkout=2026-11-15"},
            {"id":2,"chat_id":7,"name":"Flexible","url":"https://www.airbnb.com/s/Rome/homes"}]}"#,
    )
    .unwrap();
    let out = ok(&cfg, &["selftest"]);
    assert!(out.contains("✅ Calendar check #1 Imperia: works"), "{out}");
    // A booked listing still proves the calendar check works.
    airbnb.booked.lock().unwrap().insert(11);
    let out = ok(&cfg, &["selftest"]);
    assert!(
        out.contains("✅ Calendar check #1 Imperia: works (listing 11: no check-in on 2026-11-10)"),
        "{out}"
    );
    assert!(
        out.contains("⚠️ Calendar check #2 Flexible: skipped: the search has no dates"),
        "{out}"
    );

    // Calendar API broken → failure and a non-zero exit code.
    *airbnb.calendar_down.lock().unwrap() = true;
    let out = run(&cfg, &["selftest"]);
    assert!(!out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("❌ Calendar check #1 Imperia: calendar: HTTP 500"),
        "{text}"
    );
    assert!(text.contains("Some checks FAILED."), "{text}");

    // Blocked by Airbnb.
    *airbnb.fail_with.lock().unwrap() = Some(403);
    let out = String::from_utf8(run(&cfg, &["selftest"]).stdout).unwrap();
    assert!(
        out.contains("❌ Airbnb search #1 Imperia: HTTP 403"),
        "{out}"
    );

    // Nothing for the searched dates / nothing at all.
    *airbnb.fail_with.lock().unwrap() = None;
    airbnb.listings.lock().unwrap().clear();
    let out = String::from_utf8(run(&cfg, &["selftest"]).stdout).unwrap();
    assert!(
        out.contains("⚠️ Airbnb search #1 Imperia: 0 listings"),
        "{out}"
    );
    assert!(out.contains("skipped: no listing for your dates"), "{out}");
}

#[test]
fn selftest_reports_config_problems() {
    let dir = temp_dir("selftest-config");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "allowed_users = \"oops\"").unwrap();
    let out = run(&cfg, &["selftest"]);
    assert!(!out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains("❌ Config"));

    // Empty config: no users, no token.
    std::fs::write(&cfg, "").unwrap();
    let out = String::from_utf8(run(&cfg, &["selftest"]).stdout).unwrap();
    assert!(out.contains("⚠️ Users: nobody is allowed yet"), "{out}");
    assert!(out.contains("❌ Telegram: bot_token is empty"), "{out}");

    // Wrong token, unwritable data dir, bad proxy.
    std::fs::create_dir_all(dir.join("data-is-a-file")).unwrap();
    std::fs::write(dir.join("blocker"), "x").unwrap();
    std::fs::write(
        &cfg,
        "bot_token = \"wrong\"\ntelegram_api_url = \"http://127.0.0.1:1\"\n\
         data_file = \"blocker/searches.json\"\nproxies = [\"nonsense\"]\n",
    )
    .unwrap();
    let out = String::from_utf8(run(&cfg, &["selftest"]).stdout).unwrap();
    assert!(out.contains("❌ Telegram: telegram getMe"), "{out}");
    assert!(out.contains("❌ Data file: can't write"), "{out}");
    assert!(out.contains("❌ Proxies: bad proxy"), "{out}");
}

#[test]
fn selftest_reports_an_unreadable_searches_file() {
    let (cfg, _airbnb, _tg) = selftest_setup("selftest-badstore");
    std::fs::write(cfg.with_file_name("searches.json"), "{broken").unwrap();
    let out = String::from_utf8(run(&cfg, &["selftest"]).stdout).unwrap();
    assert!(out.contains("❌ Searches"), "{out}");
    // Falls back to the sample search so Airbnb access is still tested.
    assert!(out.contains("(sample: Lisbon)"), "{out}");
}
