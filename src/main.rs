use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use clap::{Parser, Subcommand};

use airbnb_notifier::app::{self, UserAction as Action};

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
    let out = &mut std::io::stdout();
    match cli.command {
        Command::Init { token } => app::init(&cli.config, token, out),
        Command::Run => {
            let stop = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
            app::run(&cli.config, stop)
        }
        Command::User { action } => {
            let action = match action {
                UserAction::Add { id } => Action::Add(id),
                UserAction::Remove { id } => Action::Remove(id),
                UserAction::List => Action::List,
            };
            app::user(&cli.config, action, out)
        }
        Command::Test { url, pages } => app::test(&cli.config, &url, pages, out),
    }
}
