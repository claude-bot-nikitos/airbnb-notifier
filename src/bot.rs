//! Handles Telegram messages and button presses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::airbnb::{self, Fetcher};
use crate::config::Config;
use crate::store::{Search, Store};
use crate::telegram::{Keyboard, Telegram, Update};
use crate::util::{esc, now_ts};

/// The subset of the Bot API we use, so handlers can be tested with a mock.
pub trait Api: Send + Sync {
    fn send(&self, chat_id: i64, html: &str, kb: Option<&Keyboard>) -> Result<()>;
    fn send_photo(&self, chat_id: i64, photo_url: &str, caption_html: &str) -> Result<()>;
    fn edit(&self, chat_id: i64, message_id: i64, html: &str, kb: Option<&Keyboard>) -> Result<()>;
    fn answer_callback(&self, id: &str, text: &str) -> Result<()>;
}

impl Api for Telegram {
    fn send(&self, chat_id: i64, html: &str, kb: Option<&Keyboard>) -> Result<()> {
        Telegram::send(self, chat_id, html, kb)
    }
    fn send_photo(&self, chat_id: i64, photo_url: &str, caption_html: &str) -> Result<()> {
        Telegram::send_photo(self, chat_id, photo_url, caption_html)
    }
    fn edit(&self, chat_id: i64, message_id: i64, html: &str, kb: Option<&Keyboard>) -> Result<()> {
        Telegram::edit(self, chat_id, message_id, html, kb)
    }
    fn answer_callback(&self, id: &str, text: &str) -> Result<()> {
        Telegram::answer_callback(self, id, text)
    }
}

pub const HELP: &str = "<b>Airbnb notifier</b>\n\n\
1. Open Airbnb, search and set all the filters you want (dates, guests, price, map area…).\n\
2. Copy/share the link of the results page and send it to me.\n\
3. I'll remember the current listings and message you whenever a <b>new</b> one shows up.\n\n\
/list — your searches with Pause / Rename / Delete buttons\n\
/check — check all your searches now\n\
/rename <i>id</i> <i>name</i>, /pause <i>id</i>, /resume <i>id</i>, /delete <i>id</i>\n\n\
You can have as many searches as you like.";

pub struct Bot<A: Api> {
    pub api: Arc<A>,
    pub store: Arc<Mutex<Store>>,
    pub fetcher: Arc<Fetcher>,
    pub wake: Sender<()>,
    pub config_path: PathBuf,
    /// chat_id → search id waiting for a new name.
    pending_rename: HashMap<i64, u32>,
    /// Last config that loaded; used if the file is broken by a hand edit.
    last_config: Option<Config>,
}

impl<A: Api> Bot<A> {
    pub fn new(
        api: Arc<A>,
        store: Arc<Mutex<Store>>,
        fetcher: Arc<Fetcher>,
        wake: Sender<()>,
        config_path: PathBuf,
    ) -> Self {
        Bot {
            api,
            store,
            fetcher,
            wake,
            config_path,
            pending_rename: HashMap::new(),
            last_config: None,
        }
    }

    fn say(&self, chat_id: i64, html: &str) {
        if let Err(e) = self.api.send(chat_id, html, None) {
            log::error!("send to {chat_id} failed: {e:#}");
        }
    }

    fn save(&self, store: &Store) {
        if let Err(e) = store.save() {
            log::error!("saving searches failed: {e:#}");
        }
    }

    /// Access is re-read from the config on every update, so `user add`
    /// works without a restart.
    fn allowed(&mut self, user_id: i64) -> bool {
        match Config::load(&self.config_path) {
            Ok(c) => self.last_config = Some(c),
            Err(e) => log::error!("config reload failed, keeping the previous one: {e:#}"),
        }
        // Never loaded successfully → nobody gets in.
        self.last_config
            .as_ref()
            .is_some_and(|c| c.is_allowed(user_id))
    }

    pub fn handle(&mut self, update: Update) {
        match update {
            Update::Message {
                chat_id,
                user_id,
                text,
            } => {
                if !self.allowed(user_id) {
                    log::info!("rejected message from unauthorized user {user_id}");
                    self.say(
                        chat_id,
                        &format!(
                            "⛔ You are not allowed to use this bot.\nYour Telegram ID: <code>{user_id}</code>\n\
                             Ask the admin to run:\n<code>airbnb-notifier user add {user_id}</code>"
                        ),
                    );
                    return;
                }
                self.on_text(chat_id, text.trim());
            }
            Update::Callback {
                id,
                chat_id,
                user_id,
                message_id,
                data,
            } => {
                if !self.allowed(user_id) {
                    let _ = self.api.answer_callback(&id, "Not allowed");
                    return;
                }
                let note = self.on_button(chat_id, message_id, &data);
                if let Err(e) = self.api.answer_callback(&id, &note) {
                    log::warn!("answerCallbackQuery failed: {e:#}");
                }
            }
        }
    }

    fn on_text(&mut self, chat_id: i64, text: &str) {
        if let Some(cmd) = text.strip_prefix('/') {
            self.pending_rename.remove(&chat_id);
            let (cmd, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
            let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
            let args = args.trim();
            let id_arg = || {
                args.split_whitespace()
                    .next()
                    .and_then(|s| s.trim_start_matches('#').parse::<u32>().ok())
            };
            match cmd.as_str() {
                "start" | "help" => self.say(chat_id, HELP),
                "list" => self.list(chat_id),
                "check" => self.check_now(chat_id),
                "pause" | "resume" => match id_arg() {
                    Some(id) => {
                        let msg = self.set_paused(chat_id, id, cmd == "pause");
                        self.say(chat_id, &msg);
                    }
                    None => self.say(chat_id, &format!("Usage: /{cmd} <i>id</i> (see /list)")),
                },
                "delete" | "remove" => match id_arg() {
                    Some(id) => {
                        let msg = self.delete(chat_id, id);
                        self.say(chat_id, &msg);
                    }
                    None => self.say(chat_id, "Usage: /delete <i>id</i> (see /list)"),
                },
                "rename" => {
                    let (id, name) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
                    match id.trim_start_matches('#').parse::<u32>() {
                        Ok(id) if !name.trim().is_empty() => {
                            let msg = self.rename(chat_id, id, name.trim());
                            self.say(chat_id, &msg);
                        }
                        Ok(id) => self.ask_rename(chat_id, id),
                        Err(_) => self.say(chat_id, "Usage: /rename <i>id</i> <i>new name</i>"),
                    }
                }
                _ => self.say(chat_id, HELP),
            }
            return;
        }

        if let Some(url) = airbnb::extract_url(text) {
            self.pending_rename.remove(&chat_id);
            self.create(chat_id, &url);
            return;
        }

        if let Some(id) = self.pending_rename.remove(&chat_id) {
            let msg = self.rename(chat_id, id, text);
            self.say(chat_id, &msg);
            return;
        }

        self.say(chat_id, "Send me an Airbnb search link, or /help.");
    }

    fn create(&mut self, chat_id: i64, raw_url: &str) {
        let url = match self.fetcher.resolve(raw_url) {
            Ok(u) => u,
            Err(e) => {
                self.say(chat_id, &format!("❌ {}", esc(&format!("{e:#}"))));
                return;
            }
        };
        let name = airbnb::default_name(&url);
        let search = {
            let mut store = self.store.lock().unwrap();
            let s = store.add(chat_id, name, url);
            self.save(&store);
            s
        };
        log::info!(
            "chat {chat_id} created search #{} '{}'",
            search.id,
            search.name
        );
        self.pending_rename.insert(chat_id, search.id);
        self.say(
            chat_id,
            &format!(
                "🆕 Search <b>#{}</b> created: <b>{}</b>\n\
                 I'm loading the current listings now — you'll only get notified about ones that appear later.\n\n\
                 ✏️ Send me a name for this search, or just ignore this to keep “{}”.",
                search.id,
                esc(&search.name),
                esc(&search.name)
            ),
        );
        let _ = self.wake.send(());
    }

    fn list(&self, chat_id: i64) {
        let searches: Vec<Search> = {
            let store = self.store.lock().unwrap();
            store.for_chat(chat_id).into_iter().cloned().collect()
        };
        if searches.is_empty() {
            self.say(
                chat_id,
                "You have no searches yet. Send me an Airbnb search link to create one.",
            );
            return;
        }
        for s in &searches {
            if let Err(e) = self.api.send(chat_id, &card(s), Some(&card_keyboard(s))) {
                log::error!("send to {chat_id} failed: {e:#}");
            }
        }
    }

    fn check_now(&self, chat_id: i64) {
        let n = {
            let mut store = self.store.lock().unwrap();
            let ids: Vec<u32> = store
                .for_chat(chat_id)
                .iter()
                .filter(|s| !s.paused)
                .map(|s| s.id)
                .collect();
            for id in &ids {
                if let Some(s) = store.get_mut(*id) {
                    s.last_check = 0;
                }
            }
            ids.len()
        };
        if n == 0 {
            self.say(chat_id, "No active searches to check.");
        } else {
            self.say(chat_id, &format!("🔄 Checking {n} search(es) now…"));
            let _ = self.wake.send(());
        }
    }

    fn set_paused(&self, chat_id: i64, id: u32, paused: bool) -> String {
        let mut store = self.store.lock().unwrap();
        let Some(s) = store.owned_mut(chat_id, id) else {
            return format!("No search #{id}. See /list");
        };
        s.paused = paused;
        let msg = if paused {
            format!("⏸ Paused <b>#{id} {}</b>", esc(&s.name))
        } else {
            // Re-check soon after resuming.
            s.last_check = 0;
            format!("▶️ Resumed <b>#{id} {}</b>", esc(&s.name))
        };
        self.save(&store);
        drop(store);
        if !paused {
            let _ = self.wake.send(());
        }
        msg
    }

    fn delete(&self, chat_id: i64, id: u32) -> String {
        let mut store = self.store.lock().unwrap();
        match store.remove(chat_id, id) {
            Some(s) => {
                self.save(&store);
                format!("🗑 Deleted <b>#{id} {}</b>", esc(&s.name))
            }
            None => format!("No search #{id}. See /list"),
        }
    }

    fn rename(&self, chat_id: i64, id: u32, name: &str) -> String {
        let name = crate::util::truncate(name.trim(), 100);
        let mut store = self.store.lock().unwrap();
        let Some(s) = store.owned_mut(chat_id, id) else {
            return format!("No search #{id}. See /list");
        };
        s.name = name.clone();
        self.save(&store);
        format!("✏️ Search <b>#{id}</b> is now called <b>{}</b>", esc(&name))
    }

    fn ask_rename(&mut self, chat_id: i64, id: u32) {
        let exists = self.store.lock().unwrap().owned_mut(chat_id, id).is_some();
        if exists {
            self.pending_rename.insert(chat_id, id);
            self.say(
                chat_id,
                &format!("✏️ Send me the new name for search <b>#{id}</b>"),
            );
        } else {
            self.say(chat_id, &format!("No search #{id}. See /list"));
        }
    }

    /// Handles an inline button; returns the short toast shown to the user.
    fn on_button(&mut self, chat_id: i64, message_id: i64, data: &str) -> String {
        let Some((action, id)) = data.split_once(':') else {
            return String::new();
        };
        let Ok(id) = id.parse::<u32>() else {
            return String::new();
        };
        match action {
            "p" | "r" => {
                self.set_paused(chat_id, id, action == "p");
                self.refresh_card(chat_id, message_id, id);
                if action == "p" { "Paused" } else { "Resumed" }.to_string()
            }
            "n" => {
                self.ask_rename(chat_id, id);
                String::new()
            }
            "d" => {
                let kb: Keyboard = vec![vec![
                    ("✅ Yes, delete".to_string(), format!("D:{id}")),
                    ("Cancel".to_string(), format!("c:{id}")),
                ]];
                let text = format!("Delete search <b>#{id}</b>?");
                if let Err(e) = self.api.edit(chat_id, message_id, &text, Some(&kb)) {
                    log::warn!("edit failed: {e:#}");
                }
                String::new()
            }
            "D" => {
                let msg = self.delete(chat_id, id);
                if let Err(e) = self.api.edit(chat_id, message_id, &msg, None) {
                    log::warn!("edit failed: {e:#}");
                }
                "Deleted".to_string()
            }
            "c" => {
                self.refresh_card(chat_id, message_id, id);
                String::new()
            }
            _ => String::new(),
        }
    }

    fn refresh_card(&self, chat_id: i64, message_id: i64, id: u32) {
        let s = self
            .store
            .lock()
            .unwrap()
            .owned_mut(chat_id, id)
            .map(|s| s.clone());
        let res = match s {
            Some(s) => self
                .api
                .edit(chat_id, message_id, &card(&s), Some(&card_keyboard(&s))),
            None => self.api.edit(
                chat_id,
                message_id,
                &format!("Search #{id} no longer exists."),
                None,
            ),
        };
        if let Err(e) = res {
            log::warn!("edit failed: {e:#}");
        }
    }
}

fn ago(ts: i64) -> String {
    if ts == 0 {
        return "never".into();
    }
    let d = (now_ts() - ts).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86400),
    }
}

pub fn card(s: &Search) -> String {
    let status = if s.paused {
        "⏸ paused"
    } else if !s.initialized {
        "⏳ loading"
    } else {
        "▶️ active"
    };
    let mut text = format!(
        "<b>#{} {}</b> — {status}\n<a href=\"{}\">Open search</a> · {} listings known · checked {}",
        s.id,
        esc(&s.name),
        esc(&s.url),
        s.seen.len(),
        ago(s.last_check)
    );
    if let Some(err) = &s.last_error {
        text.push_str(&format!("\n⚠️ Last error: {}", esc(err)));
    }
    text
}

pub fn card_keyboard(s: &Search) -> Keyboard {
    let toggle = if s.paused {
        ("▶️ Resume".to_string(), format!("r:{}", s.id))
    } else {
        ("⏸ Pause".to_string(), format!("p:{}", s.id))
    };
    vec![vec![
        toggle,
        ("✏️ Rename".to_string(), format!("n:{}", s.id)),
        ("🗑 Delete".to_string(), format!("d:{}", s.id)),
    ]]
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::mpsc;

    #[derive(Default)]
    pub struct MockApi {
        pub sent: Mutex<Vec<(i64, String)>>,
        pub photos: Mutex<Vec<(i64, String, String)>>,
        pub edits: Mutex<Vec<(i64, i64, String)>>,
        pub fail_photos: bool,
        /// Every call fails, like Telegram being down.
        pub fail_all: bool,
    }

    impl MockApi {
        fn check(&self) -> Result<()> {
            if self.fail_all {
                return Err(anyhow::anyhow!("telegram down"));
            }
            Ok(())
        }
    }

    impl Api for MockApi {
        fn send(&self, chat_id: i64, html: &str, _kb: Option<&Keyboard>) -> Result<()> {
            self.check()?;
            self.sent.lock().unwrap().push((chat_id, html.to_string()));
            Ok(())
        }
        fn send_photo(&self, chat_id: i64, photo: &str, caption: &str) -> Result<()> {
            if self.fail_photos || self.fail_all {
                return Err(anyhow::anyhow!("bad photo"));
            }
            self.photos
                .lock()
                .unwrap()
                .push((chat_id, photo.to_string(), caption.to_string()));
            Ok(())
        }
        fn edit(
            &self,
            chat_id: i64,
            message_id: i64,
            html: &str,
            _kb: Option<&Keyboard>,
        ) -> Result<()> {
            self.check()?;
            self.edits
                .lock()
                .unwrap()
                .push((chat_id, message_id, html.to_string()));
            Ok(())
        }
        fn answer_callback(&self, _id: &str, _text: &str) -> Result<()> {
            self.check()
        }
    }

    fn setup(name: &str) -> (Bot<MockApi>, mpsc::Receiver<()>) {
        setup_with(name, MockApi::default())
    }

    fn setup_with(name: &str, api: MockApi) -> (Bot<MockApi>, mpsc::Receiver<()>) {
        let dir = std::env::temp_dir().join(format!("abn-bot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        Config {
            allowed_users: vec![7],
            ..Config::default()
        }
        .save(&cfg_path)
        .unwrap();
        let store = Store::load(&dir.join("searches.json")).unwrap();
        let (tx, rx) = mpsc::channel();
        let bot = Bot::new(
            Arc::new(api),
            Arc::new(Mutex::new(store)),
            Arc::new(Fetcher::new(&[]).unwrap()),
            tx,
            cfg_path,
        );
        (bot, rx)
    }

    fn msg(text: &str, user: i64) -> Update {
        Update::Message {
            chat_id: user,
            user_id: user,
            text: text.into(),
        }
    }

    fn last_sent(bot: &Bot<MockApi>) -> String {
        bot.api.sent.lock().unwrap().last().unwrap().1.clone()
    }

    #[test]
    fn rejects_unknown_user_with_id() {
        let (mut bot, _rx) = setup("reject");
        bot.handle(msg("/start", 99));
        assert!(last_sent(&bot).contains("user add 99"));
    }

    #[test]
    fn create_then_name_then_manage() {
        let (mut bot, rx) = setup("flow");
        bot.handle(msg(
            "https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2&cursor=x",
            7,
        ));
        assert!(rx.try_recv().is_ok(), "poller should be woken");
        {
            let store = bot.store.lock().unwrap();
            let s = &store.all()[0];
            assert_eq!(s.name, "Lisbon, Portugal");
            assert_eq!(
                s.url,
                "https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2"
            );
            assert_eq!(s.chat_id, 7);
        }
        // Next plain text becomes the name.
        bot.handle(msg("Lisbon week", 7));
        assert_eq!(bot.store.lock().unwrap().all()[0].name, "Lisbon week");
        // A further plain text is not a rename.
        bot.handle(msg("hello", 7));
        assert_eq!(bot.store.lock().unwrap().all()[0].name, "Lisbon week");

        bot.handle(msg("/pause 1", 7));
        assert!(bot.store.lock().unwrap().all()[0].paused);
        bot.handle(msg("/resume #1", 7));
        assert!(!bot.store.lock().unwrap().all()[0].paused);
        bot.handle(msg("/rename 1 Porto too", 7));
        assert_eq!(bot.store.lock().unwrap().all()[0].name, "Porto too");

        bot.handle(msg("/list", 7));
        assert!(last_sent(&bot).contains("#1 Porto too"));

        bot.handle(msg("/delete 1", 7));
        assert!(bot.store.lock().unwrap().all().is_empty());
    }

    #[test]
    fn rejects_non_search_links() {
        let (mut bot, _rx) = setup("badlink");
        // Not an airbnb search and not resolvable offline → error, nothing stored.
        bot.handle(msg("http://127.0.0.1:1/nothing", 7));
        assert!(last_sent(&bot).starts_with("❌"));
        assert!(bot.store.lock().unwrap().all().is_empty());
    }

    #[test]
    fn buttons_pause_and_delete_with_confirm() {
        let (mut bot, _rx) = setup("buttons");
        bot.handle(msg("https://www.airbnb.com/s/Rome/homes", 7));
        let cb = |data: &str| Update::Callback {
            id: "q".into(),
            chat_id: 7,
            user_id: 7,
            message_id: 5,
            data: data.into(),
        };
        bot.handle(cb("p:1"));
        assert!(bot.store.lock().unwrap().all()[0].paused);
        bot.handle(cb("d:1"));
        assert_eq!(
            bot.store.lock().unwrap().all().len(),
            1,
            "needs confirmation"
        );
        bot.handle(cb("D:1"));
        assert!(bot.store.lock().unwrap().all().is_empty());
    }

    #[test]
    fn cannot_touch_other_users_searches() {
        let (mut bot, _rx) = setup("owner");
        bot.store
            .lock()
            .unwrap()
            .add(8, "theirs".into(), "u".into());
        bot.handle(msg("/delete 1", 7));
        assert_eq!(bot.store.lock().unwrap().all().len(), 1);
    }

    fn tap(data: &str, user: i64) -> Update {
        Update::Callback {
            id: "q".into(),
            chat_id: user,
            user_id: user,
            message_id: 5,
            data: data.into(),
        }
    }

    fn last_edit(bot: &Bot<MockApi>) -> String {
        bot.api.edits.lock().unwrap().last().unwrap().2.clone()
    }

    #[test]
    fn usage_messages_for_bad_arguments() {
        let (mut bot, _rx) = setup("usage");
        for (cmd, expect) in [
            ("/pause", "Usage: /pause"),
            ("/resume x", "Usage: /resume"),
            ("/delete", "Usage: /delete"),
            ("/rename", "Usage: /rename"),
            ("/rename abc name", "Usage: /rename"),
            ("/rename 9", "No search #9"),
            ("/pause 9", "No search #9"),
            ("/rename 9 new", "No search #9"),
            ("/check", "No active searches"),
            ("/list@my_bot", "no searches yet"),
        ] {
            bot.handle(msg(cmd, 7));
            assert!(
                last_sent(&bot).contains(expect),
                "{cmd} → {}",
                last_sent(&bot)
            );
        }
    }

    #[test]
    fn rename_command_without_name_asks_for_it() {
        let (mut bot, _rx) = setup("askname");
        bot.handle(msg("https://www.airbnb.com/s/Rome/homes", 7));
        bot.handle(msg("/list", 7)); // clears the pending "name it" prompt
        bot.handle(msg("/rename #1", 7));
        assert!(last_sent(&bot).contains("Send me the new name"));
        bot.handle(msg("  Roma  ", 7));
        assert_eq!(bot.store.lock().unwrap().all()[0].name, "Roma");
        // Names are capped.
        bot.handle(msg(&format!("/rename 1 {}", "x".repeat(300)), 7));
        assert_eq!(bot.store.lock().unwrap().all()[0].name.chars().count(), 100);
    }

    #[test]
    fn button_edge_cases() {
        let (mut bot, _rx) = setup("buttons-edge");
        bot.handle(msg("https://www.airbnb.com/s/Rome/homes", 7));
        // Malformed or unknown data is ignored.
        for data in ["garbage", "p:x", "zz:1"] {
            bot.handle(tap(data, 7));
        }
        assert!(bot.api.edits.lock().unwrap().is_empty());
        assert!(!bot.store.lock().unwrap().all()[0].paused);
        // Delete → Cancel restores the card.
        bot.handle(tap("d:1", 7));
        bot.handle(tap("c:1", 7));
        assert!(last_edit(&bot).contains("#1 Rome"));
        // Resume button.
        bot.handle(tap("p:1", 7));
        bot.handle(tap("r:1", 7));
        assert!(last_edit(&bot).contains("active") || last_edit(&bot).contains("loading"));
        // Unauthorized taps do nothing.
        bot.handle(tap("D:1", 99));
        assert_eq!(bot.store.lock().unwrap().all().len(), 1);
    }

    #[test]
    fn telegram_failures_do_not_break_handling() {
        let api = MockApi {
            fail_all: true,
            ..Default::default()
        };
        let (mut bot, rx) = setup_with("tg-down", api);
        bot.handle(msg("https://www.airbnb.com/s/Rome/homes", 7));
        assert!(rx.try_recv().is_ok());
        bot.handle(msg("/list", 7));
        bot.handle(tap("p:1", 7));
        bot.handle(tap("d:1", 7));
        bot.handle(tap("D:1", 7));
        assert!(bot.store.lock().unwrap().all().is_empty());
    }

    #[test]
    fn broken_config_keeps_last_good_access_list() {
        let (mut bot, _rx) = setup("badcfg");
        let good = std::fs::read_to_string(&bot.config_path).unwrap();
        std::fs::write(&bot.config_path, "allowed_users = 5").unwrap();
        // Never loaded → fail closed.
        bot.handle(msg("/start", 7));
        assert!(last_sent(&bot).contains("not allowed"));
        std::fs::write(&bot.config_path, good).unwrap();
        bot.handle(msg("/start", 7));
        assert!(last_sent(&bot).contains("Airbnb notifier"));
        // Broken later → previous list still applies.
        std::fs::write(&bot.config_path, "allowed_users = 5").unwrap();
        bot.handle(msg("/start", 7));
        assert!(last_sent(&bot).contains("Airbnb notifier"));
        bot.handle(msg("/start", 8));
        assert!(last_sent(&bot).contains("not allowed"));
    }

    #[test]
    fn unwritable_store_is_logged_not_fatal() {
        let (mut bot, _rx) = setup("unwritable");
        let dir = bot.config_path.parent().unwrap().to_path_buf();
        *bot.store.lock().unwrap() = Store::load(&dir.join("sub/searches.json")).unwrap();
        // A file where the data directory should be: every save fails.
        std::fs::write(dir.join("sub"), "not a dir").unwrap();
        bot.handle(msg("https://www.airbnb.com/s/Rome/homes", 7));
        bot.handle(msg("/pause 1", 7));
        assert!(bot.store.lock().unwrap().all()[0].paused, "kept in memory");
        assert!(last_sent(&bot).contains("Paused"));
    }

    #[test]
    fn card_shows_status_age_and_error() {
        let mut s = Search {
            id: 3,
            chat_id: 1,
            name: "A&B".into(),
            url: "https://www.airbnb.com/s/x/homes?a=1&b=2".into(),
            paused: false,
            initialized: false,
            seen: Default::default(),
            created_at: 0,
            last_check: 0,
            last_error: None,
            failures: 0,
        };
        let c = card(&s);
        assert!(c.contains("#3 A&amp;B"));
        assert!(c.contains("loading"));
        assert!(c.contains("checked never"));
        assert!(c.contains("a=1&amp;b=2"));
        s.initialized = true;
        s.last_error = Some("HTTP <403>".into());
        let now = now_ts();
        for (ago_secs, expect) in [
            (5, "just now"),
            (120, "2 min ago"),
            (7200, "2 h ago"),
            (3 * 86400, "3 d ago"),
        ] {
            s.last_check = now - ago_secs;
            let c = card(&s);
            assert!(c.contains(expect), "{c}");
            assert!(c.contains("active"));
            assert!(c.contains("Last error: HTTP &lt;403&gt;"));
        }
        s.paused = true;
        assert!(card(&s).contains("paused"));
        assert_eq!(card_keyboard(&s)[0][0].1, "r:3");
    }
}
