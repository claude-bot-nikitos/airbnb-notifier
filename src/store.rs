use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Search {
    pub id: u32,
    /// Chat that owns the search and receives its notifications.
    pub chat_id: i64,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub paused: bool,
    /// False until the first successful fetch has recorded the current listings.
    #[serde(default)]
    pub initialized: bool,
    /// Listings already alerted, or bookable for the searched dates when the
    /// search started. Places offered only for other dates are not in here.
    #[serde(default)]
    pub seen: BTreeSet<u64>,
    /// Seen listings missing from the latest complete scan (booked, hidden),
    /// with the time they went missing. They are alerted again on return.
    #[serde(default)]
    pub gone: BTreeMap<u64, i64>,
    /// Per-search check interval; `None` uses the config default.
    #[serde(default)]
    pub interval_minutes: Option<u64>,
    /// Version of the `seen` semantics; older searches are re-baselined silently.
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_check: i64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failures: u32,
}

/// Current meaning of `Search::seen` (2: exact-date matches only).
pub const SCHEMA: u32 = 2;

#[derive(Serialize, Deserialize, Debug, Default)]
struct Data {
    #[serde(default)]
    next_id: u32,
    #[serde(default)]
    searches: Vec<Search>,
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    data: Data,
}

impl Store {
    pub fn load(path: &Path) -> Result<Store> {
        let data = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Store {
            path: path.to_path_buf(),
            data,
        })
    }

    pub fn save(&self) -> Result<()> {
        let body = serde_json::to_vec_pretty(&self.data)?;
        crate::util::write_atomic(&self.path, &body)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn add(&mut self, chat_id: i64, name: String, url: String) -> Search {
        self.data.next_id = self.data.next_id.max(1);
        let id = self.data.next_id;
        self.data.next_id += 1;
        let s = Search {
            id,
            chat_id,
            name,
            url,
            paused: false,
            initialized: false,
            seen: BTreeSet::new(),
            gone: BTreeMap::new(),
            interval_minutes: None,
            schema: SCHEMA,
            created_at: crate::util::now_ts(),
            last_check: 0,
            last_error: None,
            failures: 0,
        };
        self.data.searches.push(s.clone());
        s
    }

    pub fn all(&self) -> &[Search] {
        &self.data.searches
    }

    pub fn for_chat(&self, chat_id: i64) -> Vec<&Search> {
        self.data
            .searches
            .iter()
            .filter(|s| s.chat_id == chat_id)
            .collect()
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Search> {
        self.data.searches.iter_mut().find(|s| s.id == id)
    }

    /// Like `get_mut`, but only if the search belongs to `chat_id`.
    pub fn owned_mut(&mut self, chat_id: i64, id: u32) -> Option<&mut Search> {
        self.get_mut(id).filter(|s| s.chat_id == chat_id)
    }

    pub fn remove(&mut self, chat_id: i64, id: u32) -> Option<Search> {
        let pos = self
            .data
            .searches
            .iter()
            .position(|s| s.id == id && s.chat_id == chat_id)?;
        Some(self.data.searches.remove(pos))
    }

    /// Active searches whose last check (successful or not) is older than
    /// their interval (`default_secs` unless set per search). New searches have
    /// `last_check == 0`, so they are due at once.
    pub fn due(&self, now: i64, default_secs: i64) -> Vec<Search> {
        self.data
            .searches
            .iter()
            .filter(|s| {
                let interval = s.interval_minutes.map_or(default_secs, |m| m as i64 * 60);
                !s.paused && now - s.last_check >= interval
            })
            .cloned()
            .collect()
    }

    pub fn get(&self, id: u32) -> Option<&Search> {
        self.data.searches.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abn-store-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("searches.json")
    }

    #[test]
    fn add_save_load() {
        let p = tmp("roundtrip");
        let mut st = Store::load(&p).unwrap();
        let a = st.add(1, "a".into(), "u".into());
        let b = st.add(1, "b".into(), "u".into());
        assert_eq!((a.id, b.id), (1, 2));
        st.get_mut(1).unwrap().seen.insert(42);
        st.save().unwrap();
        let st = Store::load(&p).unwrap();
        assert_eq!(st.all().len(), 2);
        assert!(st.all()[0].seen.contains(&42));
    }

    #[test]
    fn ids_not_reused_after_delete() {
        let mut st = Store::load(&tmp("ids")).unwrap();
        st.add(1, "a".into(), "u".into());
        assert!(st.remove(1, 1).is_some());
        assert_eq!(st.add(1, "b".into(), "u".into()).id, 2);
    }

    #[test]
    fn ownership_enforced() {
        let mut st = Store::load(&tmp("owner")).unwrap();
        st.add(1, "a".into(), "u".into());
        assert!(st.owned_mut(2, 1).is_none());
        assert!(st.remove(2, 1).is_none());
        assert!(st.owned_mut(1, 1).is_some());
    }

    #[test]
    fn due_logic() {
        let mut st = Store::load(&tmp("due")).unwrap();
        st.add(1, "new".into(), "u".into());
        st.add(1, "fresh".into(), "u".into());
        st.add(1, "stale".into(), "u".into());
        st.add(1, "paused".into(), "u".into());
        st.add(1, "failed-baseline".into(), "u".into());
        st.get_mut(5).unwrap().last_check = 1000;
        for (id, last) in [(2, 1000), (3, 100)] {
            let s = st.get_mut(id).unwrap();
            s.initialized = true;
            s.last_check = last;
        }
        st.get_mut(4).unwrap().paused = true;
        let due: Vec<u32> = st.due(1100, 600).iter().map(|s| s.id).collect();
        assert_eq!(due, vec![1, 3]);
    }

    #[test]
    fn unreadable_or_corrupt_files_are_errors() {
        let p = tmp("bad");
        std::fs::create_dir_all(&p).unwrap(); // a directory, not a file
        assert!(Store::load(&p).is_err());
        let p = tmp("corrupt");
        std::fs::write(&p, "{not json").unwrap();
        assert!(Store::load(&p).unwrap_err().to_string().contains("parsing"));
    }

    #[test]
    fn for_chat_filters() {
        let mut st = Store::load(&tmp("forchat")).unwrap();
        st.add(1, "a".into(), "u".into());
        st.add(2, "b".into(), "u".into());
        st.add(1, "c".into(), "u".into());
        let names: Vec<&str> = st.for_chat(1).iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn per_search_interval() {
        let mut st = Store::load(&tmp("interval")).unwrap();
        st.add(1, "fast".into(), "u".into());
        st.add(1, "slow".into(), "u".into());
        for id in [1, 2] {
            st.get_mut(id).unwrap().last_check = 1000;
        }
        st.get_mut(1).unwrap().interval_minutes = Some(2);
        // 3 minutes later, only the 2-minute search is due (default is 15).
        let due: Vec<u32> = st.due(1180, 900).iter().map(|s| s.id).collect();
        assert_eq!(due, vec![1]);
        assert_eq!(st.get(2).unwrap().name, "slow");
        assert!(st.get(9).is_none());
    }

    #[test]
    fn files_from_older_versions_load_with_defaults() {
        let p = tmp("old");
        std::fs::write(
            &p,
            r#"{"next_id":2,"searches":[{"id":1,"chat_id":5,"name":"n","url":"u","initialized":true,"seen":[1,2]}]}"#,
        )
        .unwrap();
        let st = Store::load(&p).unwrap();
        let s = st.get(1).unwrap();
        assert_eq!(s.schema, 0, "will be re-baselined");
        assert!(s.gone.is_empty());
        assert_eq!(s.interval_minutes, None);
        // New searches start on the current schema; gone survives a save.
        let mut st = st;
        let id = st.add(5, "new".into(), "u".into()).id;
        assert_eq!(st.get(id).unwrap().schema, SCHEMA);
        st.get_mut(1).unwrap().gone.insert(7, 123);
        st.save().unwrap();
        assert_eq!(
            Store::load(&p).unwrap().get(1).unwrap().gone.get(&7),
            Some(&123)
        );
    }
}
