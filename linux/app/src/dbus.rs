//! Session-bus API for the GNOME Shell extension, exported on the GApplication's own connection.

use crate::{Event, core, request_sync, sync_state, window};
use glib::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;

pub const IFACE: &str = "io.github.selfref.Todav1";

const XML: &str = r#"<node>
<interface name="io.github.selfref.Todav1">
  <method name="GetLists"><arg direction="out" type="a(ssu)"/></method>
  <method name="GetTasks"><arg direction="in" name="list" type="s"/><arg direction="out" type="a(sssb)"/></method>
  <method name="SetDone"><arg direction="in" name="uid" type="s"/><arg direction="in" name="done" type="b"/></method>
  <method name="AddTask"><arg direction="in" name="list" type="s"/><arg direction="in" name="summary" type="s"/><arg direction="out" type="s"/></method>
  <method name="ShowList"><arg direction="in" name="list" type="s"/></method>
  <method name="Sync"/>
  <signal name="Changed"><arg name="list" type="s"/></signal>
  <property name="SyncState" type="s" access="read"/>
</interface>
</node>"#;

thread_local! {
    static BUS: RefCell<Option<(gio::DBusConnection, String)>> = const { RefCell::new(None) };
}

pub fn register(conn: &gio::DBusConnection, path: &str) {
    let info = gio::DBusNodeInfo::for_xml(XML).expect("valid introspection xml");
    let iface = info.lookup_interface(IFACE).expect("interface in xml");
    let res = conn
        .register_object(path, &iface)
        .method_call(
            |_, _, _, _, method, params, inv| match call(method, &params) {
                Ok(v) => inv.return_value(v.as_ref()),
                Err(e) => inv.return_dbus_error(&format!("{IFACE}.Error"), &e),
            },
        )
        .property(|_, _, _, _, _| sync_state().to_variant())
        .build();
    match res {
        Ok(_) => BUS.with(|b| *b.borrow_mut() = Some((conn.clone(), path.to_string()))),
        Err(e) => eprintln!("D-Bus registration failed: {e}"),
    }
}

fn call(method: &str, params: &glib::Variant) -> Result<Option<glib::Variant>, String> {
    let c = core();
    let bad = || format!("bad arguments for {method}");
    Ok(match method {
        "GetLists" => {
            let lists: Vec<(String, String, u32)> = c
                .lists()
                .into_iter()
                .map(|l| (l.href, l.display_name, l.open_count))
                .collect();
            Some((lists,).to_variant())
        }
        "GetTasks" => {
            let (list,) = params.get::<(String,)>().ok_or_else(bad)?;
            let mut out: Vec<(String, String, String, bool)> = Vec::new();
            for g in c.grouped(list) {
                let cat = g.name.unwrap_or_default();
                for t in g.tasks {
                    // The extension cannot see hierarchy; mark subtasks inline.
                    let summary = if t.parent_uid.is_some() {
                        format!("↳ {}", t.summary)
                    } else {
                        t.summary
                    };
                    out.push((t.uid, summary, cat.clone(), t.done));
                }
            }
            Some((out,).to_variant())
        }
        "SetDone" => {
            let (uid, done) = params.get::<(String, bool)>().ok_or_else(bad)?;
            c.set_done(uid, done).map_err(|e| e.to_string())?;
            request_sync();
            None
        }
        "AddTask" => {
            let (list, summary) = params.get::<(String, String)>().ok_or_else(bad)?;
            let t = c
                .add_task(list, summary, None, None)
                .map_err(|e| e.to_string())?;
            request_sync();
            Some((t.uid,).to_variant())
        }
        "ShowList" => {
            let (list,) = params.get::<(String,)>().ok_or_else(bad)?;
            window::show_list(&list);
            None
        }
        "Sync" => {
            request_sync();
            None
        }
        m => return Err(format!("unknown method {m}")),
    })
}

pub fn on_event(ev: &Event) {
    BUS.with(|b| {
        let Some((conn, path)) = &*b.borrow() else {
            return;
        };
        let _ = match ev {
            Event::Changed(list) => {
                conn.emit_signal(None, path, IFACE, "Changed", Some(&(list,).to_variant()))
            }
            Event::SyncState(s) => {
                let changed = HashMap::from([("SyncState".to_string(), s.to_variant())]);
                let args = (IFACE, changed, Vec::<String>::new()).to_variant();
                conn.emit_signal(
                    None,
                    path,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    Some(&args),
                )
            }
        };
    });
}
