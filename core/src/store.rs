//! SQLite schema and row mapping. Every derived column is recomputed from `raw_ics`.

use crate::ical::Calendar;
use crate::{List, Result, Task};
use rusqlite::{Connection, OptionalExtension, params};

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE lists (
  href TEXT PRIMARY KEY, display_name TEXT NOT NULL, color TEXT,
  ctag TEXT, sync_token TEXT, push_topic TEXT,
  push_registration_href TEXT, push_expires INTEGER
);
CREATE TABLE tasks (
  uid TEXT PRIMARY KEY, list_href TEXT NOT NULL REFERENCES lists(href) ON DELETE CASCADE,
  etag TEXT, href TEXT, raw_ics TEXT NOT NULL,
  summary TEXT NOT NULL, description TEXT, status TEXT NOT NULL, completed_at INTEGER,
  category TEXT, parent_uid TEXT, priority INTEGER, due INTEGER,
  sort_order INTEGER, created INTEGER, last_modified INTEGER,
  dirty INTEGER NOT NULL DEFAULT 0,     -- 0 clean, 1 modified, 2 deleted locally
  deleted_remote INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX tasks_list_cat ON tasks(list_href, category, status);
CREATE INDEX tasks_href ON tasks(href);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
"#];

pub const CLEAN: i64 = 0;
pub const MODIFIED: i64 = 1;
pub const DELETED: i64 = 2;

pub fn open(path: &str) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = db.unchecked_transaction()?;
        tx.execute_batch(m)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(db)
}

pub fn kv_get(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0))
        .optional()?)
}

pub fn kv_set(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
        [key, value],
    )?;
    Ok(())
}

pub fn lists(db: &Connection) -> Result<Vec<List>> {
    let mut st = db.prepare(
        "SELECT l.href, l.display_name, l.color,
           (SELECT COUNT(*) FROM tasks t WHERE t.list_href = l.href AND t.dirty != 2
              AND t.status NOT IN ('COMPLETED', 'CANCELLED'))
         FROM lists l ORDER BY l.display_name COLLATE NOCASE",
    )?;
    let rows = st.query_map([], |r| {
        Ok(List {
            href: r.get(0)?,
            display_name: r.get(1)?,
            color: r.get(2)?,
            open_count: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

const TASK_COLS: &str = "uid, list_href, summary, description, status, completed_at, category, parent_uid, priority, due, sort_order, created, last_modified";

fn task_row(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    let status: String = r.get(4)?;
    Ok(Task {
        uid: r.get(0)?,
        list_href: r.get(1)?,
        summary: r.get(2)?,
        description: r.get(3)?,
        done: status == "COMPLETED",
        status,
        completed_at: r.get(5)?,
        category: r.get(6)?,
        parent_uid: r.get(7)?,
        priority: r.get(8)?,
        due: r.get(9)?,
        sort_order: r.get(10)?,
        created: r.get(11)?,
        last_modified: r.get(12)?,
    })
}

pub fn tasks(db: &Connection, list_href: &str, include_done: bool) -> Result<Vec<Task>> {
    let mut st = db.prepare(&format!(
        "SELECT {TASK_COLS} FROM tasks WHERE list_href = ?1 AND dirty != 2
           AND (?2 OR status NOT IN ('COMPLETED', 'CANCELLED'))
         ORDER BY COALESCE(sort_order, created, 0), created, uid"
    ))?;
    let rows = st.query_map(params![list_href, include_done], task_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn task(db: &Connection, uid: &str) -> Result<Option<Task>> {
    Ok(db
        .query_row(
            &format!("SELECT {TASK_COLS} FROM tasks WHERE uid = ?"),
            [uid],
            task_row,
        )
        .optional()?)
}

/// Raw row state needed by sync: (list_href, href, etag, raw_ics, dirty).
pub struct RawTask {
    pub uid: String,
    pub list_href: String,
    pub href: Option<String>,
    pub etag: Option<String>,
    pub raw_ics: String,
    pub dirty: i64,
}

fn raw_row(r: &rusqlite::Row) -> rusqlite::Result<RawTask> {
    Ok(RawTask {
        uid: r.get(0)?,
        list_href: r.get(1)?,
        href: r.get(2)?,
        etag: r.get(3)?,
        raw_ics: r.get(4)?,
        dirty: r.get(5)?,
    })
}

const RAW_COLS: &str = "uid, list_href, href, etag, raw_ics, dirty";

pub fn raw(db: &Connection, uid: &str) -> Result<Option<RawTask>> {
    Ok(db
        .query_row(
            &format!("SELECT {RAW_COLS} FROM tasks WHERE uid = ?"),
            [uid],
            raw_row,
        )
        .optional()?)
}

pub fn raw_by_href(db: &Connection, href: &str) -> Result<Option<RawTask>> {
    Ok(db
        .query_row(
            &format!("SELECT {RAW_COLS} FROM tasks WHERE href = ?"),
            [href],
            raw_row,
        )
        .optional()?)
}

pub fn dirty(db: &Connection) -> Result<Vec<RawTask>> {
    let mut st = db.prepare(&format!("SELECT {RAW_COLS} FROM tasks WHERE dirty != 0"))?;
    let rows = st.query_map([], raw_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Insert or replace a task from its calendar data, recomputing every derived column.
pub fn save(
    db: &Connection,
    list_href: &str,
    href: Option<&str>,
    etag: Option<&str>,
    cal: &Calendar,
    dirty: i64,
) -> Result<String> {
    let uid = cal.text("UID").unwrap_or_default();
    let status = cal.text("STATUS").unwrap_or_else(|| "NEEDS-ACTION".into());
    db.execute(
        "INSERT OR REPLACE INTO tasks (uid, list_href, etag, href, raw_ics, summary, description, status,
           completed_at, category, parent_uid, priority, due, sort_order, created, last_modified, dirty)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            uid,
            list_href,
            etag,
            href,
            cal.to_ics(),
            cal.text("SUMMARY").unwrap_or_default(),
            cal.text("DESCRIPTION"),
            status,
            cal.time("COMPLETED"),
            cal.categories().into_iter().next(),
            cal.parent_uid(),
            cal.text("PRIORITY").and_then(|p| p.parse::<i64>().ok()).filter(|&p| p != 0), // 0 = undefined
            cal.time("DUE"),
            cal.text("X-TODAV-ORDER").and_then(|p| p.parse::<i64>().ok()),
            cal.time("CREATED"),
            cal.time("LAST-MODIFIED"),
            dirty,
        ],
    )?;
    Ok(uid)
}

pub fn set_sync_state(
    db: &Connection,
    uid: &str,
    href: Option<&str>,
    etag: Option<&str>,
    dirty: i64,
) -> Result<()> {
    db.execute(
        "UPDATE tasks SET href = ?2, etag = ?3, dirty = ?4 WHERE uid = ?1",
        params![uid, href, etag, dirty],
    )?;
    Ok(())
}

pub fn delete(db: &Connection, uid: &str) -> Result<()> {
    db.execute("DELETE FROM tasks WHERE uid = ?", [uid])?;
    Ok(())
}
