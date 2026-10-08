//! Main window: login page, list sidebar, grouped task page.

use crate::{Event, core, request_sync};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use todav_core::{Task, TaskPatch};

/// Completed tasks shown in the "Done" expander, newest first.
const DONE_SHOWN: usize = 50;

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    split: adw::NavigationSplitView,
    sidebar: gtk::ListBox,
    hrefs: RefCell<Vec<String>>,
    current: RefCell<Option<String>>,
    title: adw::WindowTitle,
    spinner: adw::Spinner,
    entry: gtk::Entry,
    cats: gtk::StringList,
    cat: gtk::DropDown,
    groups: gtk::Box,
    server: adw::EntryRow,
    ntfy: adw::EntryRow,
    login: gtk::Button,
    cancel_login: gtk::Button,
    last_error: RefCell<String>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

fn ui() -> Option<Rc<Ui>> {
    UI.with(|u| u.borrow().clone())
}

pub fn is_active() -> bool {
    ui().is_some_and(|u| u.window.is_visible() && u.window.is_active())
}

pub fn setup_actions(app: &adw::Application) {
    let sync = gio::SimpleAction::new("sync", None);
    sync.connect_activate(|_, _| request_sync());
    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| app.quit()
    ));
    let logout = gio::SimpleAction::new("logout", None);
    logout.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            if let Some(acc) = core().account() {
                crate::password_clear(&acc.url, &acc.user);
            }
            let _ = core().logout();
            app.quit(); // background threads hold the old session; next start shows the login page
        }
    ));
    app.add_action(&sync);
    app.add_action(&quit);
    app.add_action(&logout);
    app.set_accels_for_action("app.sync", &["F5"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
}

pub fn present(app: &adw::Application) {
    let ui = ui().unwrap_or_else(|| {
        let ui = build(app);
        UI.with(|u| *u.borrow_mut() = Some(ui.clone()));
        ui
    });
    let logged_in = crate::logged_in();
    ui.stack
        .set_visible_child_name(if logged_in { "main" } else { "login" });
    if logged_in {
        refresh_lists(&ui);
    }
    ui.window.present();
}

pub fn show_list(href: &str) {
    if let Some(app) = gio::Application::default().and_downcast::<adw::Application>() {
        present(&app);
    }
    if let Some(ui) = ui() {
        *ui.current.borrow_mut() = Some(href.to_string());
        refresh_lists(&ui);
        ui.split.set_show_content(true);
    }
}

pub fn on_event(ev: &Event) {
    let Some(ui) = ui() else { return };
    match ev {
        Event::Changed(list) => {
            let list = list.clone();
            // Rebuild outside the signal handler that caused the change.
            glib::idle_add_local_once(move || {
                refresh_sidebar_counts(&ui);
                if ui.current.borrow().as_deref() == Some(&list) {
                    refresh_tasks(&ui);
                }
            });
        }
        Event::SyncState(s) => {
            ui.spinner.set_visible(s == "syncing");
            if let Some(err) = s.strip_prefix("error:") {
                if *ui.last_error.borrow() != err {
                    ui.toasts
                        .add_toast(adw::Toast::new(&format!("Sync failed: {err}")));
                    *ui.last_error.borrow_mut() = err.to_string();
                }
            } else if s == "idle" {
                ui.last_error.borrow_mut().clear();
                if ui.stack.visible_child_name().as_deref() == Some("main")
                    && ui.hrefs.borrow().is_empty()
                {
                    refresh_lists(&ui);
                }
            }
        }
    }
}

fn build(app: &adw::Application) -> Rc<Ui> {
    // Sidebar
    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("navigation-sidebar");
    let menu = gio::Menu::new();
    menu.append(Some("Sign Out"), Some("app.logout"));
    menu.append(Some("Quit"), Some("app.quit"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .build();
    let side_header = adw::HeaderBar::new();
    side_header.pack_end(&menu_btn);
    let side_view = adw::ToolbarView::new();
    side_view.add_top_bar(&side_header);
    side_view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .child(&sidebar)
            .vexpand(true)
            .build(),
    ));
    let side_page = adw::NavigationPage::new(&side_view, "Todav");

    // Task page
    let title = adw::WindowTitle::new("", "");
    let spinner = adw::Spinner::new();
    spinner.set_visible(false);
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Sync (F5)")
        .action_name("app.sync")
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_end(&refresh);
    header.pack_end(&spinner);

    let entry = gtk::Entry::builder()
        .placeholder_text("Add a task")
        .hexpand(true)
        .build();
    let cats = gtk::StringList::new(&["No category"]);
    let cat = gtk::DropDown::builder()
        .model(&cats)
        .tooltip_text("Category (Ctrl+K)")
        .build();
    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    add_row.append(&entry);
    add_row.append(&cat);
    let groups = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&add_row);
    body.append(&groups);
    let clamp = adw::Clamp::builder().maximum_size(720).child(&body).build();
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&header);
    content_view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .child(&clamp)
            .vexpand(true)
            .build(),
    ));
    let content_page = adw::NavigationPage::new(&content_view, "Tasks");

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&side_page));
    split.set_content(Some(&content_page));

    // Login page
    let server = adw::EntryRow::builder()
        .title("Nextcloud server")
        .text("https://")
        .build();
    let ntfy = adw::EntryRow::builder()
        .title("ntfy server for push (optional)")
        .build();
    let form = adw::PreferencesGroup::new();
    form.add(&server);
    form.add(&ntfy);
    let login = gtk::Button::builder()
        .label("Sign In with Browser")
        .halign(gtk::Align::Center)
        .build();
    login.add_css_class("suggested-action");
    login.add_css_class("pill");
    let cancel_login = gtk::Button::builder()
        .label("Cancel")
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    cancel_login.add_css_class("flat");
    let buttons = gtk::Box::new(gtk::Orientation::Vertical, 6);
    buttons.append(&login);
    buttons.append(&cancel_login);
    let login_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    login_box.append(&form);
    login_box.append(&buttons);
    let status = adw::StatusPage::builder()
        .icon_name("checkbox-checked-symbolic")
        .title("Todav")
        .description("Tasks synced with your Nextcloud")
        .child(
            &adw::Clamp::builder()
                .maximum_size(420)
                .child(&login_box)
                .build(),
        )
        .build();
    let login_view = adw::ToolbarView::new();
    login_view.add_top_bar(&adw::HeaderBar::new());
    login_view.set_content(Some(&status));

    let stack = gtk::Stack::new();
    stack.add_named(&login_view, Some("login"));
    stack.add_named(&split, Some("main"));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Todav")
        .default_width(900)
        .default_height(650)
        .content(&toasts)
        .hide_on_close(true)
        .build();
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        600.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(bp);

    let ui = Rc::new(Ui {
        window,
        toasts,
        stack,
        split,
        sidebar,
        hrefs: RefCell::new(Vec::new()),
        current: RefCell::new(None),
        title,
        spinner,
        entry,
        cats,
        cat,
        groups,
        server,
        ntfy,
        login,
        cancel_login,
        last_error: RefCell::new(String::new()),
    });
    connect(&ui);
    ui
}

fn connect(ui: &Rc<Ui>) {
    let weak = Rc::downgrade(ui);
    ui.sidebar.connect_row_selected(move |_, row| {
        let (Some(ui), Some(row)) = (weak.upgrade(), row) else {
            return;
        };
        let href = ui.hrefs.borrow().get(row.index() as usize).cloned();
        if href.is_some() && *ui.current.borrow() != href {
            *ui.current.borrow_mut() = href.clone();
            let _ = core().set_setting("last_list".into(), href.unwrap_or_default());
            refresh_tasks(&ui);
        }
        ui.split.set_show_content(true);
    });

    let weak = Rc::downgrade(ui);
    ui.entry.connect_activate(move |entry| {
        let Some(ui) = weak.upgrade() else { return };
        let text = entry.text().trim().to_string();
        let Some(list) = ui.current.borrow().clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let cat = (ui.cat.selected() > 0)
            .then(|| ui.cats.string(ui.cat.selected()).map(|s| s.to_string()))
            .flatten();
        match core().add_task(list, text, cat, None) {
            Ok(_) => {
                entry.set_text("");
                request_sync();
            }
            Err(e) => ui.toasts.add_toast(adw::Toast::new(&e.to_string())),
        }
    });

    // Ctrl+K opens the category picker.
    let keys = gtk::ShortcutController::new();
    let cat = ui.cat.clone();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>k"),
        Some(gtk::CallbackAction::new(move |_, _| {
            cat.activate();
            glib::Propagation::Stop
        })),
    ));
    ui.window.add_controller(keys);

    let weak = Rc::downgrade(ui);
    ui.login.connect_clicked(move |btn| {
        let Some(ui) = weak.upgrade() else { return };
        let server = ui.server.text().trim().to_string();
        let ntfy = ui.ntfy.text().trim().to_string();
        btn.set_sensitive(false);
        btn.set_label("Waiting for Browser…");
        ui.cancel_login.set_visible(true);
        let attempt = LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::spawn(move || login_thread(attempt, server, ntfy));
    });

    let weak = Rc::downgrade(ui);
    ui.cancel_login.connect_clicked(move |_| {
        LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst); // the running attempt sees it is stale and stops
        if let Some(ui) = weak.upgrade() {
            reset_login(&ui);
        }
    });
}

/// Bumped on every sign-in click and on cancel; a login thread only acts while its number is current.
static LOGIN_ATTEMPT: AtomicU64 = AtomicU64::new(0);

fn current(attempt: u64) -> bool {
    LOGIN_ATTEMPT.load(Ordering::SeqCst) == attempt
}

fn reset_login(ui: &Ui) {
    ui.login.set_sensitive(true);
    ui.login.set_label("Sign In with Browser");
    ui.cancel_login.set_visible(false);
}

/// Login Flow v2: open the browser, poll until the user approves, then store the app password.
fn login_thread(attempt: u64, server: String, ntfy: String) {
    let fail = |msg: String| {
        glib::MainContext::default().invoke(move || {
            if let Some(ui) = ui().filter(|_| current(attempt)) {
                reset_login(&ui);
                ui.toasts.add_toast(adw::Toast::new(&msg));
            }
        })
    };
    let flow = match todav_core::login_flow_start(server) {
        Ok(f) => f,
        Err(e) => return fail(format!("Cannot start login: {e}")),
    };
    let url = flow.login_url.clone();
    if !current(attempt) {
        return;
    }
    glib::MainContext::default().invoke(move || {
        let win = ui().map(|u| u.window.clone());
        gtk::UriLauncher::new(&url).launch(win.as_ref(), gio::Cancellable::NONE, |_| {});
    });
    for _ in 0..600 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if !current(attempt) {
            return;
        }
        match todav_core::login_flow_poll(&flow) {
            Ok(None) => continue,
            Ok(Some(c)) => {
                // Checked again on the main thread: cancel may land while this is queued.
                glib::MainContext::default().invoke(move || {
                    if current(attempt) {
                        finish_login(c, ntfy)
                    }
                });
                return;
            }
            Err(e) => return fail(format!("Login failed: {e}")),
        }
    }
    fail("Login timed out".into());
}

fn finish_login(c: todav_core::Credentials, ntfy: String) {
    let Some(ui) = ui() else { return };
    reset_login(&ui);
    if let Err(e) = crate::password_store(&c.server, &c.login_name, &c.app_password) {
        ui.toasts
            .add_toast(adw::Toast::new(&format!("Cannot save password: {e}")));
    }
    let _ = core().set_setting("ntfy_url".into(), ntfy);
    if let Err(e) = core().set_account(c.server, c.login_name, c.app_password) {
        ui.toasts.add_toast(adw::Toast::new(&e.to_string()));
        return;
    }
    crate::start_background();
    ui.stack.set_visible_child_name("main");
    refresh_lists(&ui);
}

fn refresh_lists(ui: &Rc<Ui>) {
    let lists = core().lists();
    if ui
        .current
        .borrow()
        .as_ref()
        .is_none_or(|c| !lists.iter().any(|l| &l.href == c))
    {
        let last = core().setting("last_list".into());
        let pick = lists
            .iter()
            .find(|l| Some(&l.href) == last.as_ref())
            .or(lists.first());
        *ui.current.borrow_mut() = pick.map(|l| l.href.clone());
    }
    ui.sidebar.remove_all();
    *ui.hrefs.borrow_mut() = lists.iter().map(|l| l.href.clone()).collect();
    for l in &lists {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let name = gtk::Label::builder()
            .label(&l.display_name)
            .xalign(0.0)
            .hexpand(true)
            .build();
        let count = gtk::Label::new(Some(&l.open_count.to_string()));
        count.add_css_class("dim-label");
        row.append(&name);
        row.append(&count);
        ui.sidebar.append(&row);
    }
    let current = ui.current.borrow().clone();
    if let Some(i) = lists.iter().position(|l| Some(&l.href) == current.as_ref()) {
        ui.sidebar
            .select_row(ui.sidebar.row_at_index(i as i32).as_ref());
    }
    refresh_tasks(ui);
}

fn refresh_sidebar_counts(ui: &Rc<Ui>) {
    let lists = core().lists();
    if lists.iter().map(|l| &l.href).ne(ui.hrefs.borrow().iter()) {
        return refresh_lists(ui);
    }
    for (i, l) in lists.iter().enumerate() {
        let count = ui
            .sidebar
            .row_at_index(i as i32)
            .and_then(|r| r.child())
            .and_then(|b| b.last_child())
            .and_downcast::<gtk::Label>();
        if let Some(c) = count {
            c.set_label(&l.open_count.to_string());
        }
    }
}

fn refresh_tasks(ui: &Rc<Ui>) {
    while let Some(c) = ui.groups.first_child() {
        ui.groups.remove(&c);
    }
    let Some(href) = ui.current.borrow().clone() else {
        ui.title.set_title("");
        return;
    };
    let list = core().lists().into_iter().find(|l| l.href == href);
    ui.title
        .set_title(list.as_ref().map_or("", |l| &l.display_name));
    ui.title.set_subtitle(
        &list
            .map(|l| format!("{} open", l.open_count))
            .unwrap_or_default(),
    );

    let groups = core().grouped(href.clone());

    // Category picker: configured categories, then any others in use; keep the selection.
    let selected = ui.cats.string(ui.cat.selected()).map(|s| s.to_string());
    let mut names: Vec<String> = core()
        .categories(href.clone())
        .into_iter()
        .map(|c| c.name)
        .collect();
    for g in &groups {
        if let Some(n) = &g.name
            && !names.contains(n)
        {
            names.push(n.clone());
        }
    }
    let old = ui.cats.n_items();
    let refs: Vec<&str> = std::iter::once("No category")
        .chain(names.iter().map(String::as_str))
        .collect();
    ui.cats.splice(0, old, &refs);
    let sel = selected
        .and_then(|s| refs.iter().position(|r| *r == s))
        .unwrap_or(0);
    ui.cat.set_selected(sel as u32);

    for g in &groups {
        let group = adw::PreferencesGroup::builder()
            .title(glib::markup_escape_text(
                g.name.as_deref().unwrap_or("Other"),
            ))
            .build();
        for t in &g.tasks {
            group.add(&task_row(ui, t));
        }
        ui.groups.append(&group);
    }

    let mut done: Vec<Task> = core()
        .tasks(href, true)
        .into_iter()
        .filter(|t| t.done)
        .collect();
    if !done.is_empty() {
        done.sort_by_key(|t| std::cmp::Reverse(t.completed_at));
        let exp = adw::ExpanderRow::builder()
            .title(format!("Done ({})", done.len()))
            .build();
        for t in done.iter().take(DONE_SHOWN) {
            exp.add_row(&task_row(ui, t));
        }
        let group = adw::PreferencesGroup::new();
        group.add(&exp);
        ui.groups.append(&group);
    }
    if groups.is_empty() {
        let empty = adw::StatusPage::builder()
            .icon_name("checkbox-checked-symbolic")
            .title("All Done")
            .build();
        empty.add_css_class("compact");
        ui.groups.prepend(&empty);
    }
}

fn task_row(ui: &Rc<Ui>, t: &Task) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&t.summary)
        .use_markup(false)
        .activatable(true)
        .build();
    if t.parent_uid.is_some() && !t.done {
        row.add_prefix(&gtk::Box::builder().width_request(18).build());
    }
    let check = gtk::CheckButton::builder()
        .active(t.done)
        .valign(gtk::Align::Center)
        .build();
    let uid = t.uid.clone();
    check.connect_toggled(move |c| {
        if core().set_done(uid.clone(), c.is_active()).is_ok() {
            request_sync();
        }
    });
    row.add_prefix(&check);
    if let Some(d) = &t.description {
        row.set_subtitle(&glib::markup_escape_text(
            d.lines().next().unwrap_or_default(),
        ));
    }
    let del = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Delete")
        .build();
    del.add_css_class("flat");
    let uid = t.uid.clone();
    del.connect_clicked(move |_| {
        if core().delete_task(uid.clone()).is_ok() {
            request_sync();
        }
    });
    row.add_suffix(&del);
    let (weak, task) = (Rc::downgrade(ui), t.clone());
    row.connect_activated(move |_| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, &task);
        }
    });
    row
}

fn edit_dialog(ui: &Rc<Ui>, t: &Task) {
    let summary = adw::EntryRow::builder()
        .title("Title")
        .text(&t.summary)
        .build();
    let category = adw::EntryRow::builder()
        .title("Category")
        .text(t.category.as_deref().unwrap_or_default())
        .build();
    let description = adw::EntryRow::builder()
        .title("Note")
        .text(t.description.as_deref().unwrap_or_default())
        .build();
    let form = adw::PreferencesGroup::new();
    form.add(&summary);
    form.add(&category);
    form.add(&description);
    let dialog = adw::AlertDialog::builder()
        .heading("Edit Task")
        .extra_child(&form)
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    let task = t.clone();
    dialog.connect_response(Some("save"), move |_, _| {
        // Only send what changed: a one-line entry would flatten a multi-line note.
        let changed =
            |new: String, old: Option<&str>| (new != old.unwrap_or_default()).then_some(new);
        let patch = TaskPatch {
            summary: changed(summary.text().trim().to_string(), Some(&task.summary))
                .filter(|s| !s.is_empty()),
            category: changed(category.text().trim().to_string(), task.category.as_deref()),
            description: changed(description.text().to_string(), task.description.as_deref()),
        };
        let uid = task.uid.clone();
        if core().update_task(uid.clone(), patch).is_ok() {
            request_sync();
        }
    });
    dialog.present(Some(&ui.window));
}
