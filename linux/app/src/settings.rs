//! Preferences dialog: server addresses with connection tests, applied on close.

use crate::window::{get, toast, url_text, view};
use crate::{core, request_sync};
use adw::prelude::*;
use gtk::{gio, glib};

type UrlRow = (gtk::DropDown, adw::EntryRow);

pub fn present(parent: &impl IsA<gtk::Widget>) {
    let b = view!("settings");
    let dialog: adw::PreferencesDialog = get(&b, "dialog");
    let server: UrlRow = (get(&b, "server_scheme"), get(&b, "server"));
    let ntfy: UrlRow = (get(&b, "ntfy_scheme"), get(&b, "ntfy"));

    let acc = core().account();
    let old_server = acc.as_ref().map(|a| a.url.clone()).unwrap_or_default();
    let user = acc.map(|a| a.user).unwrap_or_default();
    let password = crate::password_load(&old_server, &user).unwrap_or_default();
    let old_ntfy = core().setting("ntfy_url".into()).unwrap_or_default();
    set_url(&server, &old_server);
    set_url(&ntfy, &old_ntfy);

    let (u, p) = (user.clone(), password.clone());
    connect_test(&dialog, &get(&b, "server_test"), &server, move |url| {
        todav_core::caldav::Dav::new(&url, &u, &p)
            .principal()
            .map(|_| ())
    });
    connect_test(
        &dialog,
        &get(&b, "ntfy_test"),
        &ntfy,
        todav_core::ntfy_check,
    );

    dialog.connect_closed(move |_| {
        let new_ntfy = url_text(&ntfy);
        if new_ntfy != old_ntfy {
            let _ = core().set_setting("ntfy_url".into(), new_ntfy.clone());
            if new_ntfy.is_empty() {
                // No listener anymore: stop the server pushing to the old ntfy topic.
                std::thread::spawn(|| core().push_unregister());
            }
            crate::restart_push();
        }
        let new_server = url_text(&server);
        if !new_server.is_empty() && new_server != old_server {
            move_account(
                old_server.clone(),
                user.clone(),
                password.clone(),
                new_server,
            );
        }
    });
    dialog.present(Some(parent));
}

/// Fill a scheme picker and host entry from a full URL.
fn set_url((scheme, row): &UrlRow, url: &str) {
    match url.strip_prefix("http://") {
        Some(host) => {
            scheme.set_selected(1);
            row.set_text(host);
        }
        None => row.set_text(url.strip_prefix("https://").unwrap_or(url)),
    }
}

/// Run `check` on the entered URL off the main thread and report the result as a toast.
fn connect_test(
    dialog: &adw::PreferencesDialog,
    button: &gtk::Button,
    row: &UrlRow,
    check: impl Fn(String) -> Result<(), todav_core::Error> + Clone + Send + 'static,
) {
    let (dialog, row) = (dialog.clone(), row.clone());
    button.connect_clicked(move |button| {
        let url = url_text(&row);
        if url.is_empty() {
            return;
        }
        button.set_sensitive(false);
        let (dialog, button, check) = (dialog.clone(), button.clone(), check.clone());
        glib::spawn_future_local(async move {
            let msg = match gio::spawn_blocking(move || check(url)).await {
                Ok(Ok(())) => "Connection works".to_string(),
                Ok(Err(e)) => format!("Test failed: {e}"),
                Err(_) => "Test failed".to_string(),
            };
            button.set_sensitive(true);
            dialog.add_toast(adw::Toast::new(&msg));
        });
    });
}

/// Point the account at a new server address, keeping the user and app password.
fn move_account(old: String, user: String, password: String, new: String) {
    glib::spawn_future_local(async move {
        let r = gio::spawn_blocking(move || -> Result<(), String> {
            // Switching servers drops the local copy, so pending edits must reach the old one first.
            core()
                .sync()
                .map_err(|e| format!("Server not changed, cannot upload pending changes: {e}"))?;
            let _ = core().push_unregister();
            core()
                .set_account(new.clone(), user.clone(), password.clone())
                .map_err(|e| e.to_string())?;
            crate::password_store(&new, &user, &password)
                .map_err(|e| format!("Cannot save password: {e}"))?;
            crate::password_clear(&old, &user);
            Ok(())
        })
        .await;
        match r {
            Ok(Ok(())) => {
                toast("Nextcloud server changed");
                request_sync();
                crate::restart_push(); // register push on the new server now, not within the hour
            }
            Ok(Err(e)) => toast(&e),
            Err(_) => toast("Changing the server failed"),
        }
    });
}
