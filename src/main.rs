mod airbnb;
mod bot;
mod config;
mod poller;
mod store;
mod telegram;
mod util;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

use crate::airbnb::Fetcher;
use crate::config::Config;
use crate::store::Store;
use crate::telegram::Telegram;

#[derive(Parser)]
#[command(
    name = "airbnb-notifier",
    version,
    about = "Telegram bot that notifies about new Airbnb listings"
)]
struct Cli {
    /// Path to the config file
    #[arg(
        short,
        long,
        env = "AIRBNB_NOTIFIER_CONFIG",
        default_value = "config.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a config file (keeps an existing one)
    Init {
        /// Telegram bot token from @BotFather
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the bot
    Run,
    /// Manage who may use the bot
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Fetch a search URL once and print what was found (for troubleshooting)
    Test {
        url: String,
        /// Pages to fetch
        #[arg(long, default_value_t = 1)]
        pages: u32,
    },
}

#[derive(Subcommand)]
enum UserAction {
    /// Allow a Telegram user ID (the bot tells unknown users their ID)
    Add { id: i64 },
    /// Revoke access
    Remove { id: i64 },
    /// Show allowed users
    List,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { token } => init(&cli.config, token),
        Command::Run => run(&cli.config),
        Command::User { action } => user(&cli.config, action),
        Command::Test { url, pages } => test(&cli.config, &url, pages),
    }
}

fn init(path: &Path, token: Option<String>) -> Result<()> {
    let mut cfg = Config::load_raw(path)?;
    let existed = path.exists();
    if let Some(t) = token {
        cfg.bot_token = t.trim().to_string();
    }
    cfg.save(path)?;
    restrict_permissions(path);
    println!(
        "{} {}",
        if existed { "Updated" } else { "Created" },
        path.display()
    );
    if cfg.bot_token.is_empty() {
        println!(
            "Next: set bot_token (airbnb-notifier init --token <TOKEN>), then `user add <your id>`."
        );
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

fn user(path: &Path, action: UserAction) -> Result<()> {
    let mut cfg = Config::load_raw(path)?;
    match action {
        UserAction::Add { id } => {
            if cfg.allowed_users.contains(&id) {
                println!("{id} is already allowed.");
                return Ok(());
            }
            cfg.allowed_users.push(id);
            cfg.save(path)?;
            restrict_permissions(path);
            println!("Allowed {id}. No restart needed.");
        }
        UserAction::Remove { id } => {
            let before = cfg.allowed_users.len();
            cfg.allowed_users.retain(|u| *u != id);
            if cfg.allowed_users.len() == before {
                return Err(anyhow!("{id} is not in the list"));
            }
            cfg.save(path)?;
            println!("Removed {id}. Their searches are kept but paused until they are added back.");
        }
        UserAction::List => {
            if cfg.allowed_users.is_empty() {
                println!("No users allowed yet. Add one: airbnb-notifier user add <telegram id>");
            }
            for id in &cfg.allowed_users {
                println!("{id}");
            }
        }
    }
    Ok(())
}

fn test(path: &Path, url: &str, pages: u32) -> Result<()> {
    let cfg = Config::load(path)?;
    let fetcher = Fetcher::new(&cfg.proxies)?;
    let url = fetcher.resolve(url)?;
    println!("Search URL: {url}");
    println!("Default name: {}", airbnb::default_name(&url));
    let listings = fetcher.fetch_all(&url, pages.max(1))?;
    println!("Found {} listings:", listings.len());
    for l in &listings {
        let name = if l.name.is_empty() { &l.title } else { &l.name };
        println!("  {:>20}  {}  |  {}  |  {}", l.id, name, l.price, l.rating);
    }
    Ok(())
}

fn run(config_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path)?;
    cfg.validate_for_run()?;
    if cfg.allowed_users.is_empty() {
        log::warn!(
            "no allowed users yet: send /start to the bot to get your ID, then `airbnb-notifier user add <id>`"
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;

    let tg = Arc::new(Telegram::new(&cfg.bot_token));
    let store = Arc::new(Mutex::new(Store::load(&cfg.data_file)?));
    let fetcher = Arc::new(Fetcher::new(&cfg.proxies)?);
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
                std::thread::sleep(Duration::from_secs(5));
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
