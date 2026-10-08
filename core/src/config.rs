//! `todav/config.json` in Nextcloud Files: category order and icons per list, plus grouping.

use crate::json::Json;
use crate::{Client, Error, Result, Task, store};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct CategoryMeta {
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoryGroup {
    /// None = tasks without a category ("Other").
    pub name: Option<String>,
    pub icon: Option<String>,
    /// Top-level tasks, each followed by its (flattened) subtasks.
    pub tasks: Vec<Task>,
}

/// Config key of a list: last path segment of its href (`.../calendars/user/shopping/` → `shopping`).
fn list_key(list_href: &str) -> &str {
    list_href
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

impl Client {
    /// Fetch config.json if it changed (ETag), or create it from the categories in use when missing.
    pub(crate) fn sync_config(&self) -> Result<()> {
        let dav = self.dav()?;
        let (href, etag) = {
            let db = self.db.lock().unwrap();
            (
                store::kv_get(&db, "config_href")?,
                store::kv_get(&db, "config_etag")?,
            )
        };
        let href = match href {
            Some(h) => h,
            None => {
                let principal = dav.principal()?;
                let h = format!(
                    "/remote.php/dav/files/{}/todav/config.json",
                    list_key(&principal)
                );
                store::kv_set(&self.db.lock().unwrap(), "config_href", &h)?;
                h
            }
        };
        let headers: Vec<(&str, &str)> = etag
            .as_deref()
            .map(|e| ("If-None-Match", e))
            .into_iter()
            .collect();
        let r = dav.request("GET", &href, &headers, "")?;
        let (body, etag) = match r.status {
            304 => return Ok(()),
            200 => (r.body, r.etag),
            404 => {
                let body = self.default_config().to_string();
                let dir = &href[..href.rfind('/').unwrap() + 1];
                dav.request("MKCOL", dir, &[], "")?; // 405 if it exists: fine
                let r = dav.request(
                    "PUT",
                    &href,
                    &[("Content-Type", "application/json"), ("If-None-Match", "*")],
                    &body,
                )?;
                match r.status {
                    200..=299 => {}
                    // Another device is creating it right now; fetched on the next sync.
                    404 | 412 => return Ok(()),
                    s => return Err(Error::Http(s, r.body)),
                }
                (body, r.etag)
            }
            s => return Err(Error::Http(s, r.body)),
        };
        {
            let db = self.db.lock().unwrap();
            store::kv_set(&db, "config_json", &body)?;
            store::kv_set(&db, "config_etag", etag.as_deref().unwrap_or_default())?;
        }
        for l in self.lists() {
            self.emit(&l.href, Vec::new());
        }
        Ok(())
    }

    /// Categories in use per list, in order of first appearance.
    fn default_config(&self) -> Json {
        let mut lists = BTreeMap::new();
        for l in self.lists() {
            let mut seen = HashSet::new();
            let cats: Vec<Json> = self
                .tasks(l.href.clone(), true)
                .into_iter()
                .filter_map(|t| t.category)
                .filter(|c| seen.insert(c.clone()))
                .enumerate()
                .map(|(i, c)| {
                    Json::Obj(BTreeMap::from([
                        ("name".into(), Json::Str(c)),
                        ("order".into(), Json::Num(i as f64)),
                    ]))
                })
                .collect();
            if cats.is_empty() {
                continue;
            }
            let entry = BTreeMap::from([("categories".to_string(), Json::Arr(cats))]);
            lists.insert(list_key(&l.href).to_string(), Json::Obj(entry));
        }
        Json::Obj(BTreeMap::from([
            ("version".into(), Json::Num(1.0)),
            ("lists".into(), Json::Obj(lists)),
        ]))
    }

    /// Configured categories of a list, in configured order.
    pub fn categories(&self, list_href: String) -> Vec<CategoryMeta> {
        let raw = store::kv_get(&self.db.lock().unwrap(), "config_json")
            .ok()
            .flatten();
        let Some(cfg) = raw.as_deref().and_then(Json::parse) else {
            return Vec::new();
        };
        let Some(list) = cfg.get("lists").and_then(|l| l.get(list_key(&list_href))) else {
            return Vec::new();
        };
        let mut cats: Vec<(f64, CategoryMeta)> = list
            .get("categories")
            .map(Json::arr)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| {
                let meta = CategoryMeta {
                    name: c.get("name")?.str()?.to_string(),
                    icon: c.get("icon").and_then(Json::str).map(str::to_string),
                };
                Some((c.get("order").and_then(Json::num).unwrap_or(f64::MAX), meta))
            })
            .collect();
        cats.sort_by(|a, b| a.0.total_cmp(&b.0));
        cats.into_iter().map(|(_, m)| m).collect()
    }

    /// Open tasks grouped by category: configured categories first (in order), then unknown
    /// categories alphabetically, then uncategorised. Subtasks follow their top-level ancestor.
    pub fn grouped(&self, list_href: String) -> Vec<CategoryGroup> {
        let meta = self.categories(list_href.clone());
        group(self.tasks(list_href, false), &meta)
    }
}

fn group(tasks: Vec<Task>, meta: &[CategoryMeta]) -> Vec<CategoryGroup> {
    let uids: HashSet<String> = tasks.iter().map(|t| t.uid.clone()).collect();
    let mut children: HashMap<String, Vec<Task>> = HashMap::new();
    let mut top = Vec::new();
    for t in tasks {
        match t.parent_uid.clone().filter(|p| uids.contains(p)) {
            Some(p) => children.entry(p).or_default().push(t),
            None => top.push(t),
        }
    }
    let mut groups: Vec<CategoryGroup> = meta
        .iter()
        .map(|m| CategoryGroup {
            name: Some(m.name.clone()),
            icon: m.icon.clone(),
            tasks: Vec::new(),
        })
        .collect();
    let mut unknown: BTreeMap<Option<String>, Vec<Task>> = BTreeMap::new();
    for t in top {
        let mut flat = Vec::new();
        let mut stack = vec![t];
        while let Some(t) = stack.pop() {
            if let Some(mut kids) = children.remove(&t.uid) {
                kids.reverse();
                stack.extend(kids);
            }
            flat.push(t);
        }
        let cat = flat[0].category.clone();
        match groups.iter_mut().find(|g| g.name == cat && cat.is_some()) {
            Some(g) => g.tasks.extend(flat),
            None => unknown.entry(cat).or_default().extend(flat),
        }
    }
    // BTreeMap orders None first; uncategorised goes last.
    let other = unknown.remove(&None);
    groups.extend(unknown.into_iter().map(|(name, tasks)| CategoryGroup {
        name,
        icon: None,
        tasks,
    }));
    groups.extend(other.map(|tasks| CategoryGroup {
        name: None,
        icon: None,
        tasks,
    }));
    groups.retain(|g| !g.tasks.is_empty());
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(uid: &str, cat: Option<&str>, parent: Option<&str>) -> Task {
        Task {
            uid: uid.into(),
            list_href: String::new(),
            summary: uid.into(),
            description: None,
            status: "NEEDS-ACTION".into(),
            done: false,
            completed_at: None,
            category: cat.map(Into::into),
            parent_uid: parent.map(Into::into),
            priority: None,
            due: None,
            sort_order: None,
            created: None,
            last_modified: None,
        }
    }

    #[test]
    fn groups_in_config_order_with_subtasks_and_leftovers() {
        let meta = [
            CategoryMeta {
                name: "Home".into(),
                icon: Some("house".into()),
            },
            CategoryMeta {
                name: "Groceries".into(),
                icon: None,
            },
            CategoryMeta {
                name: "Empty".into(),
                icon: None,
            },
        ];
        let tasks = vec![
            task("milk", Some("Groceries"), None),
            task("lamp", Some("Home"), None),
            task("bulb", None, Some("lamp")),
            task("socket", Some("Car"), Some("bulb")), // deeper nesting, own category ignored
            task("loose", None, None),
            task("orphan", Some("Car"), Some("gone")), // parent not open → top level
            task("tyres", Some("Car"), None),
        ];
        let g = group(tasks, &meta);
        let shape: Vec<(Option<&str>, Vec<&str>)> = g
            .iter()
            .map(|g| {
                (
                    g.name.as_deref(),
                    g.tasks.iter().map(|t| t.uid.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (Some("Home"), vec!["lamp", "bulb", "socket"]),
                (Some("Groceries"), vec!["milk"]),
                (Some("Car"), vec!["orphan", "tyres"]),
                (None, vec!["loose"]),
            ]
        );
        assert_eq!(g[0].icon.as_deref(), Some("house"));
    }
}
