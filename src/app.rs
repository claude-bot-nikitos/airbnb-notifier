//! The CLI commands.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::airbnb::{self, Fetcher};
use crate::config::Config;
use crate::diag::{self, Check, Status};
use crate::store::Store;
use crate::telegram::Telegram;
use crate::{bot, poller};

pub enum UserAction {
    Add(i64),
    Remove(i64),
    List,
}

pub fn fetcher(cfg: &Config) -> Result<Fetcher> {
    let f = Fetcher::new(&cfg.proxies)?.with_page_delay(Duration::from_millis(cfg.page_delay_ms));
    match &cfg.airbnb_origin {
        Some(origin) => f.with_origin(origin),
        None => Ok(f),
    }
}

pub fn init(path: &Path, token: Option<String>, out: &mut dyn Write) -> Result<()> {
    let mut cfg = Config::load_raw(path)?;
    let existed = path.exists();
    if let Some(t) = token {
        cfg.bot_token = t.trim().to_string();
    }
    cfg.save(path)?;
    restrict_permissions(path);
    let verb = if existed { "Updated" } else { "Created" };
    writeln!(out, "{verb} {}", path.display())?;
    if cfg.bot_token.is_empty() {
        writeln!(
            out,
            "Next: set bot_token (airbnb-notifier init --token <TOKEN>), then `user add <your id>`."
        )?;
    }
    Ok(())
}

/// The config holds the bot token, so keep it private.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

pub fn user(path: &Path, action: UserAction, out: &mut dyn Write) -> Result<()> {
    let mut cfg = Config::load_raw(path)?;
    match action {
        UserAction::Add(id) => {
            if cfg.allowed_users.contains(&id) {
                writeln!(out, "{id} is already allowed.")?;
                return Ok(());
            }
            cfg.allowed_users.push(id);
            cfg.save(path)?;
            restrict_permissions(path);
            writeln!(out, "Allowed {id}. No restart needed.")?;
        }
        UserAction::Remove(id) => {
            let before = cfg.allowed_users.len();
            cfg.allowed_users.retain(|u| *u != id);
            if cfg.allowed_users.len() == before {
                return Err(anyhow!("{id} is not in the list"));
            }
            cfg.save(path)?;
            writeln!(
                out,
                "Removed {id}. Their searches are kept but paused until they are added back."
            )?;
        }
        UserAction::List => {
            if cfg.allowed_users.is_empty() {
                writeln!(
                    out,
                    "No users allowed yet. Add one: airbnb-notifier user add <telegram id>"
                )?;
            }
            for id in &cfg.allowed_users {
                writeln!(out, "{id}")?;
            }
        }
    }
    Ok(())
}

/// `verify`: also check each listing's live calendar for the searched dates.
pub fn test(path: &Path, url: &str, pages: u32, verify: bool, out: &mut dyn Write) -> Result<()> {
    let cfg = Config::load(path)?;
    let fetcher = fetcher(&cfg)?;
    let url = fetcher.resolve(url)?;
    writeln!(out, "Search URL: {url}")?;
    writeln!(out, "Default name: {}", airbnb::default_name(&url))?;
    match airbnb::search_dates(&url) {
        Some(stay) => writeln!(out, "Dates: {}", airbnb::stay_label(&stay))?,
        None => writeln!(out, "Dates: none (flexible search, nothing to verify)")?,
    }
    let max_verify = if verify { usize::MAX } else { 0 };
    let b = diag::breakdown(&fetcher, &url, pages, max_verify)?;
    writeln!(out, "Found {}:", b.summary())?;
    for r in &b.rows {
        writeln!(
            out,
            "  {:>20}  [{}]  {}  |  {}  |  {}",
            r.id, r.status, r.name, r.price, r.rating
        )?;
    }
    Ok(())
}

/// Live smoke test of this machine's setup. Returns whether nothing failed.
pub fn selftest(path: &Path, out: &mut dyn Write) -> Result<bool> {
    let mut checks = Vec::new();
    let cfg = match Config::load(path) {
        Ok(c) => {
            checks.push(Check::new(Status::Ok, "Config", path.display().to_string()));
            c
        }
        Err(e) => {
            checks.push(Check::new(Status::Fail, "Config", format!("{e:#}")));
            return report(&checks, out);
        }
    };
    if cfg.allowed_users.is_empty() {
        checks.push(Check::new(
            Status::Warn,
            "Users",
            "nobody is allowed yet: `airbnb-notifier user add <id>`",
        ));
    } else {
        checks.push(Check::new(
            Status::Ok,
            "Users",
            format!("{} allowed", cfg.allowed_users.len()),
        ));
    }
    if cfg.bot_token.is_empty() {
        checks.push(Check::new(
            Status::Fail,
            "Telegram",
            "bot_token is empty: `airbnb-notifier init --token …`",
        ));
    } else {
        let tg = Telegram::new(&cfg.telegram_api_url, &cfg.bot_token);
        checks.push(match tg.get_me() {
            Ok(name) => Check::new(
                Status::Ok,
                "Telegram",
                format!("token works, bot is @{name}"),
            ),
            Err(e) => Check::new(Status::Fail, "Telegram", format!("{e:#}")),
        });
    }
    // The service writes searches next to the config.
    let probe = cfg.data_file.with_file_name(".selftest-write");
    checks.push(
        match crate::util::write_atomic(&probe, b"ok")
            .and_then(|_| Ok(std::fs::remove_file(&probe)?))
        {
            Ok(()) => Check::new(Status::Ok, "Data file", cfg.data_file.display().to_string()),
            Err(e) => Check::new(
                Status::Fail,
                "Data file",
                format!("can't write next to {}: {e:#}", cfg.data_file.display()),
            ),
        },
    );
    let fetcher = match fetcher(&cfg) {
        Ok(f) => f,
        Err(e) => {
            checks.push(Check::new(Status::Fail, "Proxies", format!("{e:#}")));
            return report(&checks, out);
        }
    };
    let searches: Vec<(String, String)> = match Store::load(&cfg.data_file) {
        Ok(st) => st
            .all()
            .iter()
            .filter(|s| !s.paused)
            .take(5)
            .map(|s| (format!("#{} {}", s.id, s.name), s.url.clone()))
            .collect(),
        Err(e) => {
            checks.push(Check::new(Status::Fail, "Searches", format!("{e:#}")));
            vec![]
        }
    };
    if searches.is_empty() {
        checks.extend(diag::check_search(
            &fetcher,
            "(sample: Lisbon)",
            &diag::sample_search_url(),
        ));
    }
    for (label, url) in &searches {
        checks.extend(diag::check_search(&fetcher, label, url));
    }
    report(&checks, out)
}

fn report(checks: &[Check], out: &mut dyn Write) -> Result<bool> {
    for c in checks {
        writeln!(out, "{}", c.text())?;
    }
    let ok = diag::passed(checks);
    writeln!(
        out,
        "{}",
        if ok {
            "\nAll good."
        } else {
            "\nSome checks FAILED."
        }
    )?;
    Ok(ok)
}

/// Runs the bot until `stop` is set.
pub fn run(config_path: &Path, stop: Arc<AtomicBool>) -> Result<()> {
    let cfg = Config::load(config_path)?;
    cfg.validate_for_run()?;
    if cfg.allowed_users.is_empty() {
        log::warn!(
            "no allowed users yet: send /start to the bot to get your ID, then `airbnb-notifier user add <id>`"
        );
    }

    let tg = Arc::new(Telegram::new(&cfg.telegram_api_url, &cfg.bot_token));
    let store = Arc::new(Mutex::new(Store::load(&cfg.data_file)?));
    let fetcher = Arc::new(fetcher(&cfg)?);
    let (wake_tx, wake_rx) = mpsc::channel();

    if let Err(e) = tg.set_commands() {
        log::warn!("setMyCommands failed: {e:#}");
    }
    log::info!(
        "started: {} search(es), checking every {} min, {} proxy(ies)",
        store.lock().unwrap().all().len(),
        cfg.check_interval_minutes,
        cfg.proxies.len()
    );

    let poller = {
        let (api, store, fetcher, stop) =
            (tg.clone(), store.clone(), fetcher.clone(), stop.clone());
        let config_path = config_path.to_path_buf();
        std::thread::spawn(move || poller::run(api, store, fetcher, wake_rx, stop, &config_path))
    };

    let mut bot = bot::Bot::new(
        tg.clone(),
        store,
        fetcher,
        wake_tx,
        config_path.to_path_buf(),
    );
    let mut offset = 0i64;
    while !stop.load(Ordering::Relaxed) {
        match tg.get_updates(offset, 10) {
            Ok((next, updates)) => {
                offset = next;
                for u in updates {
                    bot.handle(u);
                }
            }
            Err(e) => {
                log::error!("getUpdates failed: {e:#}");
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    log::info!("stopping");
    drop(bot); // closes the wake channel so the poller exits promptly
    if poller.join().is_err() {
        log::error!("poller thread panicked");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that fails once `ok` lines have been written, like a closed pipe.
    struct Broken {
        ok: usize,
    }
    impl Write for Broken {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.ok == 0 {
                return Err(std::io::Error::other("closed"));
            }
            self.ok -= buf.iter().filter(|b| **b == b'\n').count().min(self.ok);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn cfg_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("abn-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn output_errors_are_returned() {
        let broken = || Broken { ok: 0 };
        let p = cfg_path("broken-out");
        assert!(init(&p, None, &mut broken()).is_err());
        // The second line ("Next: …") fails.
        assert!(init(&p, None, &mut Broken { ok: 1 }).is_err());
        assert!(init(&p, Some("t".into()), &mut broken()).is_err());
        assert!(user(&p, UserAction::List, &mut broken()).is_err());
        assert!(user(&p, UserAction::Add(1), &mut broken()).is_err());
        assert!(user(&p, UserAction::Add(1), &mut broken()).is_err()); // already allowed
        assert!(user(&p, UserAction::List, &mut broken()).is_err());
        assert!(user(&p, UserAction::Remove(1), &mut broken()).is_err());
        let mut w = broken();
        assert!(w.flush().is_ok());
    }

    #[test]
    fn test_command_output_errors_are_returned() {
        use std::io::{Read, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let _ = stream.read(&mut [0u8; 4096]);
                let body = r#"<script type="application/json">{"staysSearch":{"r":[{"__typename":"StaySearchResult","listingId":1}]}}</script>"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let p = cfg_path("test-out");
        Config {
            airbnb_origin: Some(origin),
            page_delay_ms: 0,
            ..Config::default()
        }
        .save(&p)
        .unwrap();
        let url = "https://www.airbnb.com/s/x/homes";
        // Fail on each of the lines in turn: URL, name, count, listing.
        for ok in 0..4 {
            assert!(
                test(&p, url, 1, false, &mut Broken { ok }).is_err(),
                "ok={ok}"
            );
        }
        let mut out = Vec::new();
        test(&p, url, 1, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Found 1 listings"));
    }

    #[test]
    fn fetcher_from_config() {
        assert!(fetcher(&Config::default()).is_ok());
        let bad = Config {
            airbnb_origin: Some("::".into()),
            ..Config::default()
        };
        assert!(fetcher(&bad).is_err());
    }

    #[test]
    fn run_validates_before_starting() {
        let p = cfg_path("run-invalid");
        let stop = Arc::new(AtomicBool::new(true));
        assert!(run(&p, stop).is_err(), "no token");
    }
}
