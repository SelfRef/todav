//! Runs against the real server from the workspace `.env`: `cargo test -p todav-core --features integration`.
//! Each test creates its own calendar and deletes it afterwards.
#![cfg(feature = "integration")]

use todav_core::caldav::Dav;
use todav_core::{Client, now};

fn env() -> (String, String, String) {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"))
        .unwrap_or_default();
    let get = |k: &str| {
        std::env::var(k).ok().or_else(|| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string))
        })
    };
    (
        get("TODAV_URL").expect("TODAV_URL"),
        get("TODAV_USER").expect("TODAV_USER"),
        get("TODAV_PASS").expect("TODAV_PASS"),
    )
}

/// A throwaway task calendar plus two independent "devices".
struct Fixture {
    dav: Dav,
    href: String,
    a: Client,
    b: Client,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let (url, user, pass) = env();
        let dav = Dav::new(&url, &user, &pass);
        let href = format!("/remote.php/dav/calendars/{user}/it-{name}-{}/", now());
        let body = format!(
            r#"<?xml version="1.0"?><c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop><d:displayname>it-{name}</d:displayname><c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set></d:prop></d:set></c:mkcalendar>"#
        );
        assert_eq!(
            dav.request("MKCALENDAR", &href, &[], &body).unwrap().status,
            201
        );
        let client = |dev: &str| {
            let dir = std::env::temp_dir().join(format!("todav-it-{name}-{dev}-{}", now()));
            let c = Client::open(dir.to_string_lossy().into()).unwrap();
            c.set_account(url.clone(), user.clone(), pass.clone())
                .unwrap();
            c.sync().unwrap();
            c
        };
        let (a, b) = (client("a"), client("b"));
        Fixture { dav, href, a, b }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.dav.request("DELETE", &self.href, &[], "");
    }
}

#[test]
fn add_on_a_shows_on_b_and_external_edits_arrive() {
    let f = Fixture::new("roundtrip");
    let t =
        f.a.add_task(
            f.href.clone(),
            "Żółty ser, mleko".into(),
            Some("Groceries".into()),
            None,
        )
        .unwrap();
    f.a.sync().unwrap();
    f.b.sync_list(f.href.clone()).unwrap();
    let on_b = f.b.task(t.uid.clone()).expect("task synced to b");
    assert_eq!(
        (on_b.summary.as_str(), on_b.category.as_deref()),
        ("Żółty ser, mleko", Some("Groceries"))
    );

    // Edit from "another client" (raw PUT), keeping unknown properties.
    let href = format!("{}{}.ics", f.href, t.uid);
    let (etag, ics) = f.dav.get(&href).unwrap().unwrap();
    let ics = ics.replace("SUMMARY:", "X-OTHER:keep\r\nSUMMARY:Edited ");
    f.dav.put(&href, &ics, etag.as_deref(), None).unwrap();
    f.a.sync_list(f.href.clone()).unwrap();
    assert!(
        f.a.task(t.uid.clone())
            .unwrap()
            .summary
            .starts_with("Edited")
    );

    // Tick on a: the foreign property survives the round trip.
    f.a.set_done(t.uid.clone(), true).unwrap();
    f.a.sync().unwrap();
    let (_, ics) = f.dav.get(&href).unwrap().unwrap();
    assert!(
        ics.contains("X-OTHER:keep") && ics.contains("STATUS:COMPLETED"),
        "{ics}"
    );

    // Delete on b reaches a.
    f.b.sync().unwrap();
    f.b.delete_task(t.uid.clone()).unwrap();
    f.b.sync().unwrap();
    f.a.sync().unwrap();
    assert!(f.a.task(t.uid).is_none());
}

#[test]
fn offline_tick_vs_edit_resolves_to_done() {
    let f = Fixture::new("conflict");
    let t =
        f.a.add_task(f.href.clone(), "Buy bread".into(), None, None)
            .unwrap();
    f.a.sync().unwrap();
    f.b.sync().unwrap();

    // a ticks, b renames later (newer LAST-MODIFIED) — both offline, then both sync.
    f.a.set_done(t.uid.clone(), true).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
    f.b.update_task(
        t.uid.clone(),
        todav_core::TaskPatch {
            summary: Some("Buy rye bread".into()),
            ..Default::default()
        },
    )
    .unwrap();
    f.a.sync().unwrap();
    let r = f.b.sync().unwrap();
    assert_eq!(r.conflicts, 1);
    f.a.sync().unwrap();
    for c in [&f.a, &f.b] {
        let t = c.task(t.uid.clone()).unwrap();
        assert_eq!((t.summary.as_str(), t.done), ("Buy rye bread", true));
    }
}

#[test]
fn removed_remotely_disappears() {
    let f = Fixture::new("removed");
    let t =
        f.a.add_task(f.href.clone(), "Gone soon".into(), None, None)
            .unwrap();
    f.a.sync().unwrap();
    f.dav
        .delete(&format!("{}{}.ics", f.href, t.uid), None, None)
        .unwrap();
    f.a.sync().unwrap();
    assert!(f.a.task(t.uid).is_none());
}
