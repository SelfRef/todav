//! Sync engine: push the dirty queue, then pull each list with sync-collection.

use crate::caldav::{RemoteList, SyncResult};
use crate::ical::Calendar;
use crate::{Client, Error, Result, store};
use rusqlite::params;
use std::collections::HashSet;

#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    pub pushed: u32,
    pub pulled: u32,
    pub deleted: u32,
    pub conflicts: u32,
}

impl Client {
    /// Push dirty tasks, refresh the list of calendars, then pull every list.
    pub fn sync(&self) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().unwrap();
        let mut report = self.push_dirty()?;
        let remote = self.dav()?.discover()?;
        self.save_lists(&remote)?;
        for l in &remote {
            self.pull(&l.href, &mut report)?;
        }
        Ok(report)
    }

    pub fn sync_list(&self, list_href: String) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().unwrap();
        let mut report = self.push_dirty()?;
        self.pull(&list_href, &mut report)?;
        Ok(report)
    }

    /// Pull only the list a push message was about; unknown topic → full sync.
    pub fn sync_topic(&self, topic: String) -> Result<SyncReport> {
        let href: Option<String> = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT href FROM lists WHERE push_topic = ?",
                [&topic],
                |r| r.get(0),
            )
            .ok()
        };
        match href {
            Some(h) => self.sync_list(h),
            None => self.sync(),
        }
    }

    fn save_lists(&self, remote: &[RemoteList]) -> Result<()> {
        let db = self.db.lock().unwrap();
        let tx = db.unchecked_transaction()?;
        for l in remote {
            tx.execute(
                "INSERT INTO lists (href, display_name, color, ctag, push_topic) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(href) DO UPDATE SET display_name = ?2, color = ?3, ctag = ?4, push_topic = ?5",
                params![l.href, l.display_name, l.color, l.ctag, l.push_topic],
            )?;
        }
        let keep: HashSet<&str> = remote.iter().map(|l| l.href.as_str()).collect();
        let local: Vec<String> = tx
            .prepare("SELECT href FROM lists")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for h in local.iter().filter(|h| !keep.contains(h.as_str())) {
            tx.execute("DELETE FROM lists WHERE href = ?", [h])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn push_dirty(&self) -> Result<SyncReport> {
        let dav = self.dav()?;
        let mut report = SyncReport::default();
        let dirty = store::dirty(&self.db.lock().unwrap())?;
        for t in dirty {
            let reg = self.registration(&t.list_href);
            if t.dirty == store::DELETED {
                let href = t.href.as_deref().unwrap_or_default();
                match dav.delete(href, t.etag.as_deref(), reg.as_deref()) {
                    // 412: changed on the server since we saw it; the server copy wins and the next pull restores it.
                    Ok(()) | Err(Error::Conflict) => {
                        store::delete(&self.db.lock().unwrap(), &t.uid)?
                    }
                    Err(e) => return Err(e),
                }
                report.pushed += 1;
                continue;
            }
            let href = t
                .href
                .clone()
                .unwrap_or_else(|| format!("{}{}.ics", t.list_href, t.uid));
            let etag = match dav.put(&href, &t.raw_ics, t.etag.as_deref(), reg.as_deref()) {
                Ok(etag) => etag,
                Err(Error::Conflict) => {
                    report.conflicts += 1;
                    self.resolve_conflict(&t, &href, reg.as_deref())?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let db = self.db.lock().unwrap();
            // Only clear the flag if nobody edited the row while we were uploading.
            let still_same = store::raw(&db, &t.uid)?.is_some_and(|r| r.raw_ics == t.raw_ics);
            let dirty = if still_same {
                store::CLEAN
            } else {
                store::MODIFIED
            };
            store::set_sync_state(&db, &t.uid, Some(&href), etag.as_deref(), dirty)?;
            report.pushed += 1;
        }
        Ok(report)
    }

    fn registration(&self, list_href: &str) -> Option<String> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT push_registration_href FROM lists WHERE href = ?",
            [list_href],
            |r| r.get(0),
        )
        .ok()?
    }

    /// 412 on PUT: fetch the server copy, merge by the conflict rule, upload the result once.
    fn resolve_conflict(
        &self,
        local: &store::RawTask,
        href: &str,
        reg: Option<&str>,
    ) -> Result<()> {
        let dav = self.dav()?;
        let l = Calendar::parse(&local.raw_ics).ok_or_else(|| Error::Parse(href.into()))?;
        let (merged, etag) = match dav.get(href)? {
            None => (l.clone(), dav.put(href, &local.raw_ics, None, reg)?), // gone on the server: re-create
            Some((server_etag, server_ics)) => {
                let s = Calendar::parse(&server_ics).ok_or_else(|| Error::Parse(href.into()))?;
                let merged = resolve(&l, &s);
                self.log_conflict(&local.uid, &l, &s)?;
                if merged.to_ics() == s.to_ics() {
                    (merged, server_etag)
                } else {
                    let etag = dav.put(href, &merged.to_ics(), server_etag.as_deref(), reg)?;
                    (merged, etag)
                }
            }
        };
        let db = self.db.lock().unwrap();
        store::save(
            &db,
            &local.list_href,
            Some(href),
            etag.as_deref(),
            &merged,
            store::CLEAN,
        )?;
        Ok(())
    }

    fn log_conflict(&self, uid: &str, local: &Calendar, server: &Calendar) -> Result<()> {
        let db = self.db.lock().unwrap();
        let mut log = store::kv_get(&db, "conflict_log")?.unwrap_or_default();
        log += &format!(
            "{} {uid} local={:?}/{:?} server={:?}/{:?}\n",
            crate::ical::fmt_utc(crate::now()),
            local.text("LAST-MODIFIED"),
            local.text("STATUS"),
            server.text("LAST-MODIFIED"),
            server.text("STATUS"),
        );
        // ponytail: unbounded log in kv; trim when it gets big enough to matter.
        store::kv_set(&db, "conflict_log", &log)
    }

    fn pull(&self, list_href: &str, report: &mut SyncReport) -> Result<()> {
        let dav = self.dav()?;
        let token: String = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT COALESCE(sync_token, '') FROM lists WHERE href = ?",
                [list_href],
                |r| r.get(0),
            )
            .map_err(|_| Error::NotFound(list_href.into()))?
        };
        let (full, res) = match dav.sync_collection(list_href, &token)? {
            SyncResult::InvalidToken => (true, dav.sync_collection(list_href, "")?),
            r => (token.is_empty(), r),
        };
        let SyncResult::Changes {
            token: new_token,
            changed,
            removed,
        } = res
        else {
            return Err(Error::Parse("sync-token rejected twice".into()));
        };

        let mut uids = Vec::new();
        let db = self.db.lock().unwrap();
        let tx = db.unchecked_transaction()?;
        let mut seen = HashSet::new();
        for (href, etag, data) in changed {
            let Some(server) = Calendar::parse(&data) else {
                continue;
            };
            let uid = server.text("UID").unwrap_or_default();
            seen.insert(uid.clone());
            let local = store::raw(&tx, &uid)?;
            if local
                .as_ref()
                .is_some_and(|l| l.etag.as_deref() == Some(&etag) && l.dirty == store::CLEAN)
            {
                continue;
            }
            match local {
                Some(l) if l.dirty == store::MODIFIED => {
                    // Edited locally during this sync: merge, keep dirty so the next push uploads it.
                    let local_cal =
                        Calendar::parse(&l.raw_ics).ok_or_else(|| Error::Parse(uid.clone()))?;
                    let merged = resolve(&local_cal, &server);
                    store::save(
                        &tx,
                        list_href,
                        Some(&href),
                        Some(&etag),
                        &merged,
                        store::MODIFIED,
                    )?;
                }
                Some(l) if l.dirty == store::DELETED => {
                    tx.execute(
                        "UPDATE tasks SET etag = ?2, href = ?3 WHERE uid = ?1",
                        params![uid, etag, href],
                    )?;
                }
                _ => {
                    store::save(
                        &tx,
                        list_href,
                        Some(&href),
                        Some(&etag),
                        &server,
                        store::CLEAN,
                    )?;
                }
            }
            report.pulled += 1;
            uids.push(uid);
        }
        let mut gone: Vec<store::RawTask> = Vec::new();
        for href in removed {
            gone.extend(store::raw_by_href(&tx, &href)?);
        }
        if full {
            // Full resync: anything clean we did not see no longer exists.
            let mut st =
                tx.prepare("SELECT uid FROM tasks WHERE list_href = ? AND href IS NOT NULL")?;
            let all: Vec<String> = st
                .query_map([list_href], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(st);
            for uid in all.into_iter().filter(|u| !seen.contains(u)) {
                gone.extend(store::raw(&tx, &uid)?);
            }
        }
        for t in gone {
            if t.dirty == store::MODIFIED {
                // Deleted remotely but edited here: re-push as a new resource.
                store::set_sync_state(&tx, &t.uid, None, None, store::MODIFIED)?;
            } else {
                store::delete(&tx, &t.uid)?;
                report.deleted += 1;
            }
            uids.push(t.uid);
        }
        tx.execute(
            "UPDATE lists SET sync_token = ? WHERE href = ?",
            params![new_token, list_href],
        )?;
        tx.commit()?;
        drop(db);
        if !uids.is_empty() {
            self.emit(list_href, uids);
        }
        Ok(())
    }
}

/// Conflict rule: newer LAST-MODIFIED wins the whole VTODO; a completed status always sticks.
pub fn resolve(local: &Calendar, server: &Calendar) -> Calendar {
    let lm = |c: &Calendar| c.time("LAST-MODIFIED").unwrap_or(0);
    let (winner, loser) = if lm(local) > lm(server) {
        (local, server)
    } else {
        (server, local)
    };
    let mut out = winner.clone();
    let done = |c: &Calendar| c.text("STATUS").as_deref() == Some("COMPLETED");
    if done(loser) && !done(winner) {
        for p in ["STATUS", "COMPLETED", "PERCENT-COMPLETE"] {
            let line = loser.prop(p).cloned();
            out.set(
                p,
                line.as_ref().map_or("", |l| &l.params),
                line.as_ref().map(|l| l.value.as_str()),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(lm: &str, status: &str, summary: &str) -> Calendar {
        Calendar::parse(&format!(
            "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:u\nSUMMARY:{summary}\nSTATUS:{status}\nLAST-MODIFIED:{lm}\nEND:VTODO\nEND:VCALENDAR\n"
        ))
        .unwrap()
    }

    #[test]
    fn newer_wins_but_completed_sticks() {
        let old_done = cal("20261001T000000Z", "COMPLETED", "old");
        let new_open = cal("20261002T000000Z", "NEEDS-ACTION", "new");
        let r = resolve(&old_done, &new_open);
        assert_eq!(r.text("SUMMARY").unwrap(), "new");
        assert_eq!(r.text("STATUS").unwrap(), "COMPLETED");

        let r = resolve(&new_open, &cal("20261001T000000Z", "NEEDS-ACTION", "old"));
        assert_eq!(r.text("SUMMARY").unwrap(), "new");
        assert_eq!(r.text("STATUS").unwrap(), "NEEDS-ACTION");
    }
}
