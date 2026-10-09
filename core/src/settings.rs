//! Front-end settings, kept locally and synced through `.config/todav/settings.json` in
//! Nextcloud Files: every key carries its last change time and the newer value wins.

use crate::json::Json;
use crate::{Client, Error, Result, store};
use std::collections::BTreeMap;

/// Settings that only make sense on one device and are never synced.
pub const DEVICE_ONLY: &[&str] = &["ntfy_url", "last_list", "sync_settings"];

/// key → (value, unix time of the last change)
type Map = BTreeMap<String, (String, i64)>;

impl Client {
    /// Free-form front-end settings (sort order, ntfy URL, ...), stored next to the data.
    pub fn setting(&self, key: String) -> Option<String> {
        store::kv_get(&self.db.lock().unwrap(), &format!("setting_{key}")).ok()?
    }

    /// Records the change time too, so the newest value wins when settings sync.
    pub fn set_setting(&self, key: String, value: String) -> Result<()> {
        let db = self.db.lock().unwrap();
        if store::kv_get(&db, &format!("setting_{key}"))?.as_deref() == Some(&value) {
            return Ok(()); // unchanged: keep the old time so it does not override other devices
        }
        store::kv_set(&db, &format!("setting_{key}"), &value)?;
        store::kv_set(&db, &format!("settime_{key}"), &crate::now().to_string())
    }

    /// Merge local and server settings (newer wins), uploading if this device has newer ones.
    /// Off when the `sync_settings` setting is "false".
    pub(crate) fn sync_settings(&self) -> Result<()> {
        if self.setting("sync_settings".into()).as_deref() == Some("false") {
            return Ok(());
        }
        let dav = self.dav()?;
        let cached = store::kv_get(&self.db.lock().unwrap(), "config_settings_href")?;
        let href = match cached {
            Some(h) => h,
            None => {
                let principal = dav.principal()?;
                let user = principal
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                let h = format!("/remote.php/dav/files/{user}/.config/todav/settings.json");
                store::kv_set(&self.db.lock().unwrap(), "config_settings_href", &h)?;
                h
            }
        };
        // A failed If-Match means another device wrote in between: fetch again and re-merge.
        for _ in 0..3 {
            let (etag, cached) = {
                let db = self.db.lock().unwrap();
                (
                    store::kv_get(&db, "config_settings_etag")?,
                    store::kv_get(&db, "config_settings_json")?,
                )
            };
            let headers: Vec<(&str, &str)> = etag
                .as_deref()
                .map(|e| ("If-None-Match", e))
                .into_iter()
                .collect();
            let r = dav.request("GET", &href, &headers, "")?;
            let (body, etag) = match r.status {
                304 => (cached.unwrap_or_default(), etag),
                200 => (r.body, r.etag),
                404 => (String::new(), None),
                s => return Err(Error::Http(s, r.body)),
            };
            let remote = parse(&body);
            let (apply, merged, upload) = merge(&self.synced()?, &remote);
            {
                let db = self.db.lock().unwrap();
                for (k, (v, t)) in &apply {
                    store::kv_set(&db, &format!("setting_{k}"), v)?;
                    store::kv_set(&db, &format!("settime_{k}"), &t.to_string())?;
                }
                store::kv_set(&db, "config_settings_json", &body)?;
                store::kv_set(
                    &db,
                    "config_settings_etag",
                    etag.as_deref().unwrap_or_default(),
                )?;
            }
            if !apply.is_empty() {
                for l in self.listeners.lock().unwrap().iter() {
                    l.settings_changed();
                }
            }
            if !upload {
                return Ok(());
            }
            let new_body = serialize(&merged);
            if r.status == 404 {
                let dir = &href[..href.rfind('/').unwrap()];
                dav.request("MKCOL", &dir[..dir.rfind('/').unwrap() + 1], &[], "")?; // .config/, 405 if it exists
                dav.request("MKCOL", &format!("{dir}/"), &[], "")?;
            }
            let cond = match etag.as_deref().filter(|e| !e.is_empty()) {
                Some(e) => ("If-Match", e.to_string()),
                None => ("If-None-Match", "*".to_string()),
            };
            let r = dav.request(
                "PUT",
                &href,
                &[("Content-Type", "application/json"), (cond.0, &cond.1)],
                &new_body,
            )?;
            match r.status {
                200..=299 => {
                    let db = self.db.lock().unwrap();
                    store::kv_set(&db, "config_settings_json", &new_body)?;
                    // Without an ETag the next GET simply returns the file again.
                    store::kv_set(
                        &db,
                        "config_settings_etag",
                        r.etag.as_deref().unwrap_or_default(),
                    )?;
                    return Ok(());
                }
                412 => continue,
                s => return Err(Error::Http(s, r.body)),
            }
        }
        Ok(()) // still racing: the next sync tries again
    }

    /// Local settings that sync, with change times (0 when set before times were recorded).
    fn synced(&self) -> Result<Map> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT substr(s.key, 9), s.value, t.value FROM kv s
             LEFT JOIN kv t ON t.key = 'settime_' || substr(s.key, 9)
             WHERE substr(s.key, 1, 8) = 'setting_'",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Map::new();
        for row in rows {
            let (k, v, t) = row?;
            if !DEVICE_ONLY.contains(&k.as_str()) {
                out.insert(k, (v, t.and_then(|t| t.parse().ok()).unwrap_or(0)));
            }
        }
        Ok(out)
    }
}

/// (remote entries to apply locally, merged map, whether the server lacks something newer).
/// Newer wins; on a tie the server wins, so devices converge.
fn merge(local: &Map, remote: &Map) -> (Map, Map, bool) {
    let mut apply = Map::new();
    let mut merged = remote.clone();
    let mut upload = false;
    for (k, (rv, rt)) in remote {
        if DEVICE_ONLY.contains(&k.as_str()) {
            merged.remove(k);
            continue;
        }
        match local.get(k) {
            Some((lv, lt)) if lt > rt => {
                merged.insert(k.clone(), (lv.clone(), *lt));
                upload = true;
            }
            Some((lv, _)) if lv == rv => {}
            _ => {
                apply.insert(k.clone(), (rv.clone(), *rt));
            }
        }
    }
    for (k, v) in local {
        if !remote.contains_key(k) {
            merged.insert(k.clone(), v.clone());
            upload = true;
        }
    }
    (apply, merged, upload)
}

/// `{"version": 1, "settings": {"key": {"value": "…", "modified": 1700000000}}}`
fn parse(body: &str) -> Map {
    let Some(json) = Json::parse(body) else {
        return Map::new();
    };
    let Some(Json::Obj(settings)) = json.get("settings") else {
        return Map::new();
    };
    settings
        .iter()
        .filter_map(|(k, e)| {
            let v = e.get("value")?.str()?.to_string();
            let t = e.get("modified").and_then(Json::num).unwrap_or(0.0) as i64;
            Some((k.clone(), (v, t)))
        })
        .collect()
}

fn serialize(map: &Map) -> String {
    let settings = map
        .iter()
        .map(|(k, (v, t))| {
            let entry = BTreeMap::from([
                ("value".to_string(), Json::Str(v.clone())),
                ("modified".to_string(), Json::Num(*t as f64)),
            ]);
            (k.clone(), Json::Obj(entry))
        })
        .collect();
    Json::Obj(BTreeMap::from([
        ("version".into(), Json::Num(1.0)),
        ("settings".into(), Json::Obj(settings)),
    ]))
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str, i64)]) -> Map {
        entries
            .iter()
            .map(|(k, v, t)| (k.to_string(), (v.to_string(), *t)))
            .collect()
    }

    #[test]
    fn newer_wins_and_ties_go_to_the_server() {
        let local = map(&[
            ("a", "local", 5),      // newer here → upload
            ("b", "local", 1),      // older here → apply remote
            ("c", "local", 3),      // tie, different → apply remote
            ("d", "same", 3),       // tie, same → nothing
            ("only_local", "x", 0), // missing remotely → upload
        ]);
        let remote = map(&[
            ("a", "remote", 4),
            ("b", "remote", 2),
            ("c", "remote", 3),
            ("d", "same", 3),
            ("only_remote", "y", 1),
            ("ntfy_url", "https://other", 9), // device-only: never applied or kept
        ]);
        let (apply, merged, upload) = merge(&local, &remote);
        assert_eq!(
            apply,
            map(&[
                ("b", "remote", 2),
                ("c", "remote", 3),
                ("only_remote", "y", 1)
            ])
        );
        assert_eq!(
            merged,
            map(&[
                ("a", "local", 5),
                ("b", "remote", 2),
                ("c", "remote", 3),
                ("d", "same", 3),
                ("only_local", "x", 0),
                ("only_remote", "y", 1),
            ])
        );
        assert!(upload);
        assert_eq!(parse(&serialize(&merged)), merged);
    }
}
