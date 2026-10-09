//! Todav for Linux: GTK UI + background service (sync worker, ntfy push listener, D-Bus API).

mod dbus;
mod settings;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};
use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use todav_core::{ChangeListener, Client};

pub const APP_ID: &str = "io.github.selfref.Todav";

static CORE: OnceLock<Arc<Client>> = OnceLock::new();
static SYNC: OnceLock<Sender<()>> = OnceLock::new();
static SYNC_STATE: Mutex<String> = Mutex::new(String::new());

pub fn core() -> &'static Arc<Client> {
    CORE.get().expect("core opened at startup")
}

/// Ask the worker for a full sync; requests arriving while one runs are coalesced.
pub fn request_sync() {
    if let Some(tx) = SYNC.get() {
        let _ = tx.send(());
    }
}

/// True once an account is active and the background service runs.
pub fn logged_in() -> bool {
    SYNC.get().is_some()
}

pub fn sync_state() -> String {
    SYNC_STATE.lock().unwrap().clone()
}

/// Events from background threads, handled on the main thread.
#[derive(Debug, Clone)]
pub enum Event {
    Changed(String),
    SyncState(String),
}

fn post(ev: Event) {
    glib::MainContext::default().invoke(move || {
        if let Event::SyncState(s) = &ev {
            *SYNC_STATE.lock().unwrap() = s.clone();
        }
        window::on_event(&ev);
        dbus::on_event(&ev);
    });
}

fn set_state(r: &Result<impl std::fmt::Debug, todav_core::Error>) {
    post(Event::SyncState(match r {
        Ok(_) => "idle".into(),
        Err(e) => format!("error:{e}"),
    }));
}

struct Listener;
impl ChangeListener for Listener {
    fn changed(&self, list_href: String, _uids: Vec<String>) {
        post(Event::Changed(list_href));
    }

    /// Synced settings (e.g. task order) affect every list's view.
    fn settings_changed(&self) {
        for l in core().lists() {
            post(Event::Changed(l.href));
        }
    }
}

// --- keyring -----------------------------------------------------------------

fn schema() -> libsecret::Schema {
    let attrs = HashMap::from([
        ("url", libsecret::SchemaAttributeType::String),
        ("user", libsecret::SchemaAttributeType::String),
    ]);
    libsecret::Schema::new(APP_ID, libsecret::SchemaFlags::NONE, attrs)
}

pub fn password_load(url: &str, user: &str) -> Option<String> {
    let attrs = HashMap::from([("url", url), ("user", user)]);
    libsecret::password_lookup_sync(Some(&schema()), attrs, gio::Cancellable::NONE)
        .ok()?
        .map(|s| s.to_string())
}

pub fn password_store(url: &str, user: &str, password: &str) -> Result<(), glib::Error> {
    let attrs = HashMap::from([("url", url), ("user", user)]);
    let label = format!("Todav ({user} @ {url})");
    libsecret::password_store_sync(
        Some(&schema()),
        attrs,
        Some(libsecret::COLLECTION_DEFAULT),
        &label,
        password,
        gio::Cancellable::NONE,
    )
}

pub fn password_clear(url: &str, user: &str) {
    let attrs = HashMap::from([("url", url), ("user", user)]);
    let _ = libsecret::password_clear_sync(Some(&schema()), attrs, gio::Cancellable::NONE);
}

/// Restore the account from the keyring; false → the UI shows the login page.
pub fn restore_account() -> bool {
    let Some(acc) = core().account() else {
        return false;
    };
    let Some(pw) = password_load(&acc.url, &acc.user) else {
        return false;
    };
    core().set_account(acc.url, acc.user, pw).is_ok()
}

// --- background --------------------------------------------------------------

/// Stop the push listener and start a new one if an ntfy server is configured.
pub fn restart_push() {
    core().stop_listening();
    if let Some(ntfy) = core().setting("ntfy_url".into()).filter(|u| !u.is_empty()) {
        std::thread::spawn(move || {
            // Blocks until the next restart_push; reconnects internally.
            let _ = core().listen(ntfy, &|r| set_state(&r));
        });
    }
}

/// Start the sync worker and, if an ntfy server is configured, the push listener. Idempotent.
pub fn start_background() {
    if SYNC.get().is_some() {
        request_sync();
        return;
    }
    let (tx, rx) = channel::<()>();
    let _ = SYNC.set(tx);
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            std::thread::sleep(Duration::from_millis(300)); // let a burst of edits settle
            while rx.try_recv().is_ok() {}
            post(Event::SyncState("syncing".into()));
            set_state(&core().sync());
        }
    });
    request_sync();

    restart_push();

    // Safety net for lost pushes: every minute while the window is focused, every 30 min otherwise.
    let mut ticks = 0u32;
    glib::timeout_add_seconds_local(60, move || {
        ticks += 1;
        if window::is_active() || ticks.is_multiple_of(30) {
            request_sync();
        }
        glib::ControlFlow::Continue
    });
}

fn main() -> glib::ExitCode {
    let data = glib::user_data_dir().join("todav");
    let client = Client::open(data.to_string_lossy().into()).expect("open local database");
    client.subscribe(Box::new(Listener));
    let _ = CORE.set(Arc::new(client));

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        // Keep running as a background service after the window closes; quit is explicit.
        std::mem::forget(app.hold());
        // First: the bus name is already ours, so calls arriving before this would fail.
        if let (Some(conn), Some(path)) = (app.dbus_connection(), app.dbus_object_path()) {
            dbus::register(&conn, &path);
        }
        window::setup_actions(app);
        if restore_account() {
            start_background();
        }
    });
    app.connect_activate(window::present);
    app.run()
}
