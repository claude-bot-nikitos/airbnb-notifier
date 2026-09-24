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
    assert!(out.contains("Found 3 listings"));
    assert!(
        out.contains("Flat 13  |  €113 for 5 nights  |  4.9"),
        "{out}"
    );

    // Default is a single page.
    assert!(ok(&cfg, &["test", "https://www.airbnb.com/s/x/homes"]).contains("Found 2 listings"));

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
