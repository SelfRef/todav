pub mod caldav;
mod config;
pub mod ical;
pub mod json;
mod login;
pub mod push;
pub mod store;
mod sync;

use caldav::Dav;
use ical::Calendar;
use rusqlite::Connection;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

pub use config::{CategoryGroup, CategoryMeta};
pub use login::{Credentials, LoginFlow, login_flow_poll, login_flow_start};
pub use push::{PushRegistration, ntfy_check};
pub use sync::SyncReport;

#[derive(Debug, Clone, PartialEq)]
pub struct List {
    pub href: String,
    pub display_name: String,
    pub color: Option<String>,
    pub open_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub uid: String,
    pub list_href: String,
    pub summary: String,
    pub description: Option<String>,
    pub status: String,
    pub done: bool,
    pub completed_at: Option<i64>,
    pub category: Option<String>,
    pub parent_uid: Option<String>,
    pub priority: Option<i64>,
    pub due: Option<i64>,
    pub sort_order: Option<i64>,
    pub created: Option<i64>,
    pub last_modified: Option<i64>,
}

/// Fields left `None` are untouched; `Some("")` clears description/category.
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub summary: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub url: String,
    pub user: String,
}

pub trait ChangeListener: Send + Sync {
    fn changed(&self, list_href: String, uids: Vec<String>);
}

pub struct Client {
    db: Mutex<Connection>,
    dav: Mutex<Option<Arc<Dav>>>,
    listeners: Mutex<Vec<Box<dyn ChangeListener>>>,
    sync_lock: Mutex<()>,
    /// Bumped by `stop_listening`; a `listen` call returns once it differs from the value it started with.
    listen_gen: AtomicU64,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut b = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("system rng");
    b
}

fn new_uid() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

impl Client {
    pub fn open(data_dir: String) -> Result<Client> {
        std::fs::create_dir_all(&data_dir).map_err(|e| Error::Db(e.to_string()))?;
        let db = store::open(&format!("{data_dir}/core.sqlite"))?;
        Ok(Client {
            db: Mutex::new(db),
            dav: Mutex::new(None),
            listeners: Mutex::new(Vec::new()),
            sync_lock: Mutex::new(()),
            listen_gen: AtomicU64::new(0),
        })
    }

    /// The password is kept in memory only; front-ends keep it in the platform keyring.
    pub fn set_account(&self, url: String, user: String, app_password: String) -> Result<()> {
        let db = self.db.lock().unwrap();
        if store::kv_get(&db, "account_url")?.as_deref() != Some(&url)
            || store::kv_get(&db, "account_user")?.as_deref() != Some(&user)
        {
            // Different account: local data belongs to the old one.
            db.execute_batch(
                "DELETE FROM tasks; DELETE FROM lists;
                 DELETE FROM kv WHERE key LIKE 'config_%' OR key IN ('push_resource', 'conflict_log');",
            )?;
        }
        store::kv_set(&db, "account_url", &url)?;
        store::kv_set(&db, "account_user", &user)?;
        *self.dav.lock().unwrap() = Some(Arc::new(Dav::new(&url, &user, &app_password)));
        Ok(())
    }

    pub fn account(&self) -> Option<Account> {
        let db = self.db.lock().unwrap();
        Some(Account {
            url: store::kv_get(&db, "account_url").ok()??,
            user: store::kv_get(&db, "account_user").ok()??,
        })
    }

    /// Forget the account and all local data (keeps device keys).
    pub fn logout(&self) -> Result<()> {
        let _ = self.push_unregister();
        let db = self.db.lock().unwrap();
        db.execute_batch(
            "DELETE FROM tasks; DELETE FROM lists;
             DELETE FROM kv WHERE key LIKE 'config_%' OR key LIKE 'account_%' OR key IN ('push_resource', 'conflict_log');",
        )?;
        *self.dav.lock().unwrap() = None;
        Ok(())
    }

    /// Free-form front-end settings (ntfy URL, pinned list, ...), stored next to the data.
    pub fn setting(&self, key: String) -> Option<String> {
        store::kv_get(&self.db.lock().unwrap(), &format!("setting_{key}")).ok()?
    }

    pub fn set_setting(&self, key: String, value: String) -> Result<()> {
        store::kv_set(&self.db.lock().unwrap(), &format!("setting_{key}"), &value)
    }

    fn dav(&self) -> Result<Arc<Dav>> {
        self.dav.lock().unwrap().clone().ok_or(Error::NoAccount)
    }

    pub fn lists(&self) -> Vec<List> {
        store::lists(&self.db.lock().unwrap()).unwrap_or_default()
    }

    pub fn tasks(&self, list_href: String, include_done: bool) -> Vec<Task> {
        store::tasks(&self.db.lock().unwrap(), &list_href, include_done).unwrap_or_default()
    }

    pub fn task(&self, uid: String) -> Option<Task> {
        store::task(&self.db.lock().unwrap(), &uid).ok()?
    }

    pub fn subscribe(&self, listener: Box<dyn ChangeListener>) {
        self.listeners.lock().unwrap().push(listener);
    }

    fn emit(&self, list_href: &str, uids: Vec<String>) {
        for l in self.listeners.lock().unwrap().iter() {
            l.changed(list_href.to_string(), uids.clone());
        }
    }

    pub fn add_task(
        &self,
        list_href: String,
        summary: String,
        category: Option<String>,
        parent_uid: Option<String>,
    ) -> Result<Task> {
        let uid = new_uid();
        let t = now();
        let mut cal = Calendar::new_todo(&uid, t);
        cal.set_text("SUMMARY", Some(&summary));
        cal.set("STATUS", "", Some("NEEDS-ACTION"));
        cal.set_text("CATEGORIES", category.as_deref().filter(|c| !c.is_empty()));
        if let Some(p) = &parent_uid {
            cal.set("RELATED-TO", ";RELTYPE=PARENT", Some(&ical::escape(p)));
        }
        cal.set_time("LAST-MODIFIED", Some(t));
        {
            let db = self.db.lock().unwrap();
            let next: i64 = db.query_row(
                "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM tasks WHERE list_href = ?",
                [&list_href],
                |r| r.get(0),
            )?;
            cal.set("X-TODAV-ORDER", "", Some(&next.to_string()));
            store::save(&db, &list_href, None, None, &cal, store::MODIFIED)?;
        }
        self.emit(&list_href, vec![uid.clone()]);
        self.task(uid.clone()).ok_or(Error::NotFound(uid))
    }

    /// Load, patch and save a task as locally modified.
    fn modify(&self, uid: &str, f: impl FnOnce(&mut Calendar)) -> Result<()> {
        let list = {
            let db = self.db.lock().unwrap();
            let raw = store::raw(&db, uid)?.ok_or_else(|| Error::NotFound(uid.into()))?;
            let mut cal = Calendar::parse(&raw.raw_ics).ok_or_else(|| Error::Parse(uid.into()))?;
            f(&mut cal);
            touch(&mut cal);
            store::save(
                &db,
                &raw.list_href,
                raw.href.as_deref(),
                raw.etag.as_deref(),
                &cal,
                store::MODIFIED,
            )?;
            raw.list_href
        };
        self.emit(&list, vec![uid.to_string()]);
        Ok(())
    }

    pub fn set_done(&self, uid: String, done: bool) -> Result<()> {
        self.modify(&uid, |cal| set_done(cal, done, now()))
    }

    pub fn update_task(&self, uid: String, patch: TaskPatch) -> Result<()> {
        self.modify(&uid, |cal| {
            if let Some(s) = &patch.summary {
                cal.set_text("SUMMARY", Some(s));
            }
            if let Some(d) = &patch.description {
                cal.set_text("DESCRIPTION", Some(d.as_str()).filter(|d| !d.is_empty()));
            }
            if let Some(c) = &patch.category {
                // Keep secondary categories; replace the first one.
                let mut cats = cal.categories();
                if cats.is_empty() {
                    cats.push(String::new());
                }
                cats[0] = c.clone();
                let v: Vec<String> = cats
                    .iter()
                    .filter(|c| !c.is_empty())
                    .map(|c| ical::escape(c))
                    .collect();
                cal.set(
                    "CATEGORIES",
                    "",
                    Some(v.join(",").as_str()).filter(|v| !v.is_empty()),
                );
            }
        })
    }

    pub fn delete_task(&self, uid: String) -> Result<()> {
        let list = {
            let db = self.db.lock().unwrap();
            let raw = store::raw(&db, &uid)?.ok_or_else(|| Error::NotFound(uid.clone()))?;
            if raw.href.is_none() {
                store::delete(&db, &uid)?; // never reached the server
            } else {
                db.execute("UPDATE tasks SET dirty = 2 WHERE uid = ?", [&uid])?;
            }
            raw.list_href
        };
        self.emit(&list, vec![uid]);
        Ok(())
    }

    /// Move `uid` right before `before_uid` (or to the end) among its siblings in the list.
    pub fn reorder(&self, uid: String, before_uid: Option<String>) -> Result<()> {
        let task = self
            .task(uid.clone())
            .ok_or_else(|| Error::NotFound(uid.clone()))?;
        let mut order: Vec<Task> = self.tasks(task.list_href.clone(), true);
        order.retain(|t| t.uid != uid);
        let pos = before_uid
            .and_then(|b| order.iter().position(|t| t.uid == b))
            .unwrap_or(order.len());
        order.insert(pos, task);
        // ponytail: renumbers the whole list (one PUT per moved task); use gaps if lists get long.
        for (i, t) in order.iter().enumerate() {
            let i = i as i64 + 1;
            if t.sort_order != Some(i) {
                self.modify(&t.uid, |cal| {
                    cal.set("X-TODAV-ORDER", "", Some(&i.to_string()))
                })?;
            }
        }
        Ok(())
    }
}

/// Bump LAST-MODIFIED, DTSTAMP and SEQUENCE after a local edit.
fn touch(cal: &mut Calendar) {
    let t = now();
    cal.set_time("LAST-MODIFIED", Some(t));
    cal.set_time("DTSTAMP", Some(t));
    let seq = cal
        .text("SEQUENCE")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    cal.set("SEQUENCE", "", Some(&(seq + 1).to_string()));
}

fn set_done(cal: &mut Calendar, done: bool, t: i64) {
    if done {
        cal.set("STATUS", "", Some("COMPLETED"));
        cal.set_time("COMPLETED", Some(t));
        cal.set("PERCENT-COMPLETE", "", Some("100"));
    } else {
        cal.set("STATUS", "", Some("NEEDS-ACTION"));
        cal.set_time("COMPLETED", None);
        cal.set("PERCENT-COMPLETE", "", None);
    }
}

#[derive(Debug)]
pub enum Error {
    Auth,
    Conflict,
    NoAccount,
    NotFound(String),
    Http(u16, String),
    Net(String),
    Parse(String),
    Db(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Auth => write!(f, "authentication failed"),
            Error::Conflict => write!(f, "precondition failed (changed on server)"),
            Error::NoAccount => write!(f, "no account configured"),
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Http(code, body) => write!(
                f,
                "HTTP {code}: {}",
                body.chars().take(300).collect::<String>()
            ),
            Error::Net(s) => write!(f, "network: {s}"),
            Error::Parse(s) => write!(f, "parse: {s}"),
            Error::Db(s) => write!(f, "database: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64; `url` = URL-safe alphabet without padding (as Web Push wants).
pub fn b64encode(data: &[u8], url: bool) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..=c.len() {
            let ch = B64[(n >> (18 - 6 * i) & 63) as usize] as char;
            out.push(match (url, ch) {
                (true, '+') => '-',
                (true, '/') => '_',
                _ => ch,
            });
        }
        if !url {
            out.extend(std::iter::repeat_n('=', 3 - c.len()));
        }
    }
    out
}

/// Decodes both standard and URL-safe base64, padded or not.
pub fn b64decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=' && !c.is_ascii_whitespace()) {
        let v = match c {
            b'-' => 62,
            b'_' => 63,
            c => B64
                .iter()
                .position(|&x| x == c)
                .ok_or_else(|| Error::Parse("bad base64".into()))? as u32,
        };
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
