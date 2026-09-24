use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

fn default_interval() -> u64 {
    15
}
fn default_max_pages() -> u32 {
    10
}
fn default_data_file() -> PathBuf {
    "searches.json".into()
}
fn default_page_delay_ms() -> u64 {
    1500
}
fn default_telegram_api_url() -> String {
    "https://api.telegram.org".into()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub allowed_users: Vec<i64>,
    #[serde(default = "default_interval")]
    pub check_interval_minutes: u64,
    #[serde(default = "default_max_pages")]
    pub max_pages: u32,
    #[serde(default = "default_data_file")]
    pub data_file: PathBuf,
    #[serde(default)]
    pub proxies: Vec<String>,

    // Advanced settings, only written to the file when changed.
    /// Pause between Airbnb page requests (a random 0–2s is added).
    #[serde(default = "default_page_delay_ms")]
    pub page_delay_ms: u64,
    /// Bot API server, e.g. a self-hosted one.
    #[serde(default = "default_telegram_api_url")]
    pub telegram_api_url: String,
    /// Fetch searches from this origin instead of Airbnb (for testing).
    #[serde(default)]
    pub airbnb_origin: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bot_token: String::new(),
            allowed_users: vec![],
            check_interval_minutes: default_interval(),
            max_pages: default_max_pages(),
            data_file: default_data_file(),
            proxies: vec![],
            page_delay_ms: default_page_delay_ms(),
            telegram_api_url: default_telegram_api_url(),
            airbnb_origin: None,
        }
    }
}

impl Config {
    /// Loads the config. A missing file yields defaults. `TELEGRAM_BOT_TOKEN`
    /// overrides `bot_token`.
    pub fn load(path: &Path) -> Result<Config> {
        let mut cfg = Config::load_raw(path)?;
        if let Ok(t) = std::env::var("TELEGRAM_BOT_TOKEN")
            && !t.trim().is_empty()
        {
            cfg.bot_token = t.trim().to_string();
        }
        // A relative data file is resolved next to the config file.
        if cfg.data_file.is_relative()
            && let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty())
        {
            cfg.data_file = dir.join(&cfg.data_file);
        }
        Ok(cfg)
    }

    /// Loads the file as written, without env overrides or path resolution,
    /// so that saving it back does not leak the env token into the file.
    pub fn load_raw(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate_for_run(&self) -> Result<()> {
        if self.bot_token.is_empty() {
            return Err(anyhow!(
                "bot_token is empty: set it in the config file or TELEGRAM_BOT_TOKEN"
            ));
        }
        if self.check_interval_minutes == 0 {
            return Err(anyhow!("check_interval_minutes must be > 0"));
        }
        if self.max_pages == 0 {
            return Err(anyhow!("max_pages must be > 0"));
        }
        Ok(())
    }

    pub fn is_allowed(&self, user_id: i64) -> bool {
        self.allowed_users.contains(&user_id)
    }

    /// Writes a commented TOML file (atomically).
    pub fn save(&self, path: &Path) -> Result<()> {
        let s = |v: &str| toml::Value::String(v.to_string()).to_string();
        let list = |items: Vec<String>| items.join(", ");
        let body = format!(
            "# airbnb-notifier config\n\
             \n\
             # Telegram bot token from @BotFather (env TELEGRAM_BOT_TOKEN overrides it)\n\
             bot_token = {}\n\
             \n\
             # Telegram user IDs allowed to use the bot.\n\
             # Manage with: airbnb-notifier user add|remove|list\n\
             allowed_users = [{}]\n\
             \n\
             # How often each search is re-checked, in minutes\n\
             check_interval_minutes = {}\n\
             \n\
             # Max result pages scanned per search (Airbnb shows ~18 listings per page, max 15 pages)\n\
             max_pages = {}\n\
             \n\
             # Where searches and seen listings are stored (relative to this file)\n\
             data_file = {}\n\
             \n\
             # Optional proxies for Airbnb requests, rotated on failure.\n\
             # e.g. \"http://user:pass@host:port\", \"socks5://host:1080\". Empty = direct.\n\
             proxies = [{}]\n",
            s(&self.bot_token),
            list(self.allowed_users.iter().map(|i| i.to_string()).collect()),
            self.check_interval_minutes,
            self.max_pages,
            s(&self.data_file.to_string_lossy()),
            list(self.proxies.iter().map(|p| s(p)).collect()),
        );
        let defaults = Config::default();
        let mut advanced = Vec::new();
        if self.page_delay_ms != defaults.page_delay_ms {
            advanced.push(format!("page_delay_ms = {}", self.page_delay_ms));
        }
        if self.telegram_api_url != defaults.telegram_api_url {
            advanced.push(format!("telegram_api_url = {}", s(&self.telegram_api_url)));
        }
        if let Some(o) = &self.airbnb_origin {
            advanced.push(format!("airbnb_origin = {}", s(o)));
        }
        let body = if advanced.is_empty() {
            body
        } else {
            format!("{body}\n# Advanced\n{}\n", advanced.join("\n"))
        };
        crate::util::write_atomic(path, body.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_for_missing_fields() {
        let c: Config = toml::from_str("bot_token = \"x\"").unwrap();
        assert_eq!(c.check_interval_minutes, 15);
        assert_eq!(c.max_pages, 10);
        assert!(c.allowed_users.is_empty());
        assert!(c.proxies.is_empty());
    }

    #[test]
    fn save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("abn-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        let c = Config {
            bot_token: "12:ab\"c".into(),
            allowed_users: vec![1, 22],
            proxies: vec!["socks5://h:1".into()],
            ..Config::default()
        };
        c.save(&p).unwrap();
        assert_eq!(Config::load_raw(&p).unwrap(), c);
        assert!(!std::fs::read_to_string(&p).unwrap().contains("Advanced"));

        let adv = Config {
            page_delay_ms: 0,
            telegram_api_url: "http://127.0.0.1:1".into(),
            airbnb_origin: Some("http://127.0.0.1:2".into()),
            ..c
        };
        adv.save(&p).unwrap();
        assert_eq!(Config::load_raw(&p).unwrap(), adv);
    }

    #[test]
    fn validation() {
        let ok = Config {
            bot_token: "t".into(),
            ..Config::default()
        };
        assert!(ok.validate_for_run().is_ok());
        assert!(Config::default().validate_for_run().is_err());
        let zero_interval = Config {
            check_interval_minutes: 0,
            ..ok.clone()
        };
        assert!(
            zero_interval
                .validate_for_run()
                .unwrap_err()
                .to_string()
                .contains("interval")
        );
        let zero_pages = Config { max_pages: 0, ..ok };
        assert!(
            zero_pages
                .validate_for_run()
                .unwrap_err()
                .to_string()
                .contains("max_pages")
        );
    }

    #[test]
    fn unreadable_file_is_an_error_and_data_file_is_relative_to_config() {
        let dir = std::env::temp_dir().join(format!("abn-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a-directory")).unwrap();
        assert!(Config::load(&dir.join("a-directory")).is_err());
        let c = Config::load(&dir.join("missing.toml")).unwrap();
        assert_eq!(c.data_file, dir.join("searches.json"));
        let c = Config::load(std::path::Path::new("missing-here.toml")).unwrap();
        assert_eq!(c.data_file, PathBuf::from("searches.json"));
    }
}
