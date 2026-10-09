//! Main window: login page, list sidebar, grouped task page.

use crate::{Event, core, request_sync};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use todav_core::{Task, TaskPatch};

/// Libadwaita only styles plain buttons as pills; round the ends of the sign-in split button to match.
/// The category section header turns its arrow like an expander row, without a "pressed" look.
/// Done subtasks are struck through.
const CSS: &str = "
splitbutton.pill { border-radius: 9999px; }
splitbutton.pill > button { padding: 10px 20px 10px 32px; border-radius: 9999px 0 0 9999px; }
splitbutton.pill > menubutton > button { padding: 10px 16px 10px 12px; border-radius: 0 9999px 9999px 0; }
.cat-toggle image { transition: -gtk-icon-transform 200ms ease; }
.cat-toggle:not(:checked) image { -gtk-icon-transform: rotate(-90deg); }
.cat-toggle:checked:not(:hover) { background: none; }
row.subtask-done label.title { text-decoration-line: line-through; opacity: 0.55; }
";

/// Left margin per subtask level, in pixels.
const INDENT: i32 = 24;

/// Completed tasks shown in the "Done" expander, newest first.
const DONE_SHOWN: usize = 50;

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    split: adw::NavigationSplitView,
    sidebar: gtk::ListBox,
    cat_section: gtk::Box,
    cat_list: gtk::ListBox,
    /// Category the task page is narrowed to, with the list it belongs to.
    filter: RefCell<Option<(String, String)>>,
    hrefs: RefCell<Vec<String>>,
    current: RefCell<Option<String>>,
    title: adw::WindowTitle,
    spinner: adw::Spinner,
    entry: gtk::Entry,
    cats: gtk::StringList,
    cat: gtk::DropDown,
    groups: gtk::Box,
    /// Root tasks whose subtasks are shown; kept across rebuilds of the task page.
    expanded: RefCell<HashSet<String>>,
    /// Root task shown as an expander for its first subtask, before it has any.
    adding: RefCell<Option<String>>,
    /// Root task whose "Add subtask" entry takes focus after the next rebuild.
    focus_sub: RefCell<Option<String>>,
    server: (gtk::DropDown, adw::EntryRow),
    ntfy: (gtk::DropDown, adw::EntryRow),
    login: adw::SplitButton,
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
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
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

pub fn toast(msg: &str) {
    if let Some(ui) = ui() {
        ui.toasts.add_toast(adw::Toast::new(msg));
    }
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

/// Loads a view compiled from `ui/<name>.blp` by build.rs.
macro_rules! view {
    ($name:literal) => {
        gtk::Builder::from_string(include_str!(concat!(env!("OUT_DIR"), "/", $name, ".ui")))
    };
}
pub(crate) use view;

pub fn get<T: IsA<glib::Object>>(b: &gtk::Builder, id: &str) -> T {
    b.object(id).unwrap_or_else(|| panic!("no `{id}` in view"))
}

fn build(app: &adw::Application) -> Rc<Ui> {
    let b = view!("window");
    let window: adw::ApplicationWindow = get(&b, "window");
    window.set_application(Some(app));

    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &WidgetExt::display(&window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let ui = Rc::new(Ui {
        window,
        toasts: get(&b, "toasts"),
        stack: get(&b, "stack"),
        split: get(&b, "split"),
        sidebar: get(&b, "sidebar"),
        cat_section: get(&b, "cat_section"),
        cat_list: get(&b, "cat_list"),
        filter: RefCell::new(None),
        hrefs: RefCell::new(Vec::new()),
        current: RefCell::new(None),
        title: get(&b, "title"),
        spinner: get(&b, "spinner"),
        entry: get(&b, "entry"),
        cats: get(&b, "cats"),
        cat: get(&b, "cat"),
        groups: get(&b, "groups"),
        expanded: RefCell::new(HashSet::new()),
        adding: RefCell::new(None),
        focus_sub: RefCell::new(None),
        server: (get(&b, "server_scheme"), get(&b, "server")),
        ntfy: (get(&b, "ntfy_scheme"), get(&b, "ntfy")),
        login: get(&b, "login"),
        cancel_login: get(&b, "cancel_login"),
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

    // Clicking the selected category clears the filter.
    let weak = Rc::downgrade(ui);
    ui.cat_list.connect_row_activated(move |list, row| {
        let Some(ui) = weak.upgrade() else { return };
        let (Some(href), Some(name)) = (
            ui.current.borrow().clone(),
            row.child().and_downcast::<gtk::Label>().map(|l| l.label()),
        ) else {
            return;
        };
        let pick = (href, name.to_string());
        let again = ui.filter.borrow().as_ref() == Some(&pick);
        if again {
            list.unselect_all();
        }
        *ui.filter.borrow_mut() = (!again).then_some(pick);
        ui.split.set_show_content(true);
        // Rebuild outside the handler: refresh_tasks replaces this row.
        glib::idle_add_local_once(move || refresh_tasks(&ui));
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
        // With a category filter the picker is hidden and the filter decides.
        let cat = match ui.filter.borrow().clone() {
            Some((_, c)) => Some(c),
            None => (ui.cat.selected() > 0)
                .then(|| ui.cats.string(ui.cat.selected()).map(|s| s.to_string()))
                .flatten(),
        };
        match core().add_task(list, text, cat, None) {
            Ok(_) => {
                entry.set_text("");
                request_sync();
            }
            Err(e) => ui.toasts.add_toast(adw::Toast::new(&e.to_string())),
        }
    });

    let weak = Rc::downgrade(ui);
    ui.login.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            start_login(&ui, false);
        }
    });
    let weak = Rc::downgrade(ui);
    let copy = gio::SimpleAction::new("copy-login", None);
    copy.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            start_login(&ui, true);
        }
    });
    ui.window.add_action(&copy);

    let weak = Rc::downgrade(ui);
    let prefs = gio::SimpleAction::new("preferences", None);
    prefs.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade().filter(|_| crate::logged_in()) {
            crate::settings::present(&ui.window);
        }
    });
    ui.window.add_action(&prefs);

    let weak = Rc::downgrade(ui);
    ui.cancel_login.connect_clicked(move |_| {
        LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst); // the running attempt sees it is stale and stops
        if let Some(ui) = weak.upgrade() {
            reset_login(&ui);
        }
    });
}

/// Full URL from a scheme picker and host entry; empty stays empty, a typed scheme wins over the picker.
pub fn url_text((scheme, row): &(gtk::DropDown, adw::EntryRow)) -> String {
    let host = row.text().trim().to_string();
    if host.is_empty() || host.contains("://") {
        return host;
    }
    let scheme = if scheme.selected() == 0 {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

/// Start Login Flow v2; `copy` puts the login link on the clipboard instead of opening the browser.
fn start_login(ui: &Ui, copy: bool) {
    let server = url_text(&ui.server);
    let ntfy = url_text(&ui.ntfy);
    ui.login.set_sensitive(false);
    ui.login.set_label(if copy {
        "Waiting for Sign-In…"
    } else {
        "Waiting for Browser…"
    });
    ui.cancel_login.set_visible(true);
    let attempt = LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || login_thread(attempt, server, ntfy, copy));
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

/// Login Flow v2: open the browser (or copy the link), poll until the user approves, then store the app password.
fn login_thread(attempt: u64, server: String, ntfy: String, copy: bool) {
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
        let Some(ui) = ui().filter(|_| current(attempt)) else {
            return;
        };
        if copy {
            ui.window.clipboard().set_text(&url);
            ui.toasts
                .add_toast(adw::Toast::new("Login link copied to clipboard"));
        } else {
            gtk::UriLauncher::new(&url).launch(Some(&ui.window), gio::Cancellable::NONE, |_| {});
        }
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

    // Sidebar categories; the filter only survives while its list is shown and the category exists.
    let filter = ui
        .filter
        .borrow()
        .clone()
        .filter(|(h, c)| *h == href && names.contains(c))
        .map(|(_, c)| c);
    *ui.filter.borrow_mut() = filter.clone().map(|c| (href.clone(), c));
    ui.cat.set_visible(filter.is_none());
    ui.cat_section.set_visible(!names.is_empty());
    ui.cat_list.remove_all();
    for (i, n) in names.iter().enumerate() {
        ui.cat_list
            .append(&gtk::Label::builder().label(n).xalign(0.0).build());
        if filter.as_ref() == Some(n) {
            ui.cat_list
                .select_row(ui.cat_list.row_at_index(i as i32).as_ref());
        }
    }
    let shown = |c: Option<&String>| filter.is_none() || c == filter.as_ref();
    let groups: Vec<_> = groups
        .into_iter()
        .filter(|g| shown(g.name.as_ref()))
        .collect();

    for g in &groups {
        let group = adw::PreferencesGroup::builder()
            .title(glib::markup_escape_text(
                g.name.as_deref().unwrap_or("Other"),
            ))
            .build();
        for (root, subs) in trees(&g.tasks) {
            let adding = ui.adding.borrow().as_ref() == Some(&root.uid);
            if subs.is_empty() && !adding {
                group.add(&leaf_row(ui, root));
            } else {
                if adding && !subs.is_empty() {
                    ui.adding.take(); // the first subtask exists; from now on subtasks decide
                }
                group.add(&tree_row(ui, root, &subs));
            }
        }
        ui.groups.append(&group);
    }

    let mut done: Vec<Task> = core()
        .finished(href)
        .into_iter()
        .filter(|t| shown(t.category.as_ref()))
        .collect();
    if !done.is_empty() {
        done.sort_by_key(|t| std::cmp::Reverse(t.completed_at));
        let exp = adw::ExpanderRow::builder()
            .title(format!("Done ({})", done.len()))
            .build();
        for t in done.iter().take(DONE_SHOWN) {
            exp.add_row(&task_row(ui, t, 0));
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

/// Split a group's depth-first task list into root tasks, each with its (depth, subtask) list.
fn trees(tasks: &[Task]) -> Vec<(&Task, Vec<(usize, &Task)>)> {
    let mut depth = HashMap::new();
    let mut out: Vec<(&Task, Vec<(usize, &Task)>)> = Vec::new();
    for t in tasks {
        match t.parent_uid.as_ref().and_then(|p| depth.get(p).copied()) {
            Some(d) => {
                depth.insert(&t.uid, d + 1);
                out.last_mut().unwrap().1.push((d + 1, t)); // a parent always precedes its subtasks
            }
            None => {
                depth.insert(&t.uid, 0usize);
                out.push((t, Vec::new()));
            }
        }
    }
    out
}

/// `indent` levels of left margin on the whole row, for subtasks.
fn task_row(ui: &Rc<Ui>, t: &Task, indent: usize) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&t.summary)
        .use_markup(false)
        .activatable(true)
        .build();
    row.set_margin_start(INDENT * indent as i32);
    if t.done && indent > 0 {
        row.add_css_class("subtask-done"); // stays under its parent instead of moving to "Done"
    }
    row.add_prefix(&done_check(t));
    if let Some(d) = subtitle(t) {
        row.set_subtitle(d);
    }
    row.add_suffix(&delete_button(t));
    let (weak, task) = (Rc::downgrade(ui), t.clone());
    row.connect_activated(move |_| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, &task);
        }
    });
    row
}

/// A root task without subtasks: its "+" turns it into a `tree_row` ready for the first one.
fn leaf_row(ui: &Rc<Ui>, t: &Task) -> adw::ActionRow {
    let row = task_row(ui, t, 0);
    let add = icon_button("list-add-symbolic", "Add Subtask");
    let (weak, uid) = (Rc::downgrade(ui), t.uid.clone());
    add.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        ui.expanded.borrow_mut().insert(uid.clone());
        *ui.adding.borrow_mut() = Some(uid.clone());
        *ui.focus_sub.borrow_mut() = Some(uid.clone());
        // Rebuild outside the handler: refresh_tasks replaces this row.
        glib::idle_add_local_once(move || refresh_tasks(&ui));
    });
    row.add_suffix(&add);
    row
}

/// A root task with its subtasks and an entry for adding more.
fn tree_row(ui: &Rc<Ui>, root: &Task, subs: &[(usize, &Task)]) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(&root.summary)
        .use_markup(false)
        .expanded(ui.expanded.borrow().contains(&root.uid))
        .build();
    row.add_prefix(&done_check(root));
    if let Some(d) = subtitle(root) {
        row.set_subtitle(d);
    }
    // The header toggles the subtasks, so editing gets its own button.
    let edit = icon_button("document-edit-symbolic", "Edit");
    let (weak, task) = (Rc::downgrade(ui), root.clone());
    edit.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, &task);
        }
    });
    row.add_suffix(&edit);
    row.add_suffix(&delete_button(root));
    let (weak, uid) = (Rc::downgrade(ui), root.uid.clone());
    let empty = subs.is_empty();
    row.connect_expanded_notify(move |r| {
        let Some(ui) = weak.upgrade() else { return };
        if r.is_expanded() {
            ui.expanded.borrow_mut().insert(uid.clone());
            return;
        }
        ui.expanded.borrow_mut().remove(&uid);
        // Closed before adding a first subtask: back to a plain row with "+".
        if empty && ui.adding.borrow().as_ref() == Some(&uid) {
            ui.adding.take();
            glib::idle_add_local_once(move || refresh_tasks(&ui));
        }
    });

    for (depth, t) in subs {
        row.add_row(&task_row(ui, t, *depth));
    }
    let entry = adw::EntryRow::builder().title("Add subtask").build();
    entry.set_margin_start(INDENT);
    entry.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
    if ui.focus_sub.borrow().as_ref() == Some(&root.uid) {
        ui.focus_sub.take();
        let e = entry.clone();
        glib::idle_add_local_once(move || {
            e.grab_focus();
        });
    }
    let (weak, root) = (Rc::downgrade(ui), root.clone());
    entry.connect_entry_activated(move |e| {
        let Some(ui) = weak.upgrade() else { return };
        let text = e.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let parent = Some(root.uid.clone());
        match core().add_task(root.list_href.clone(), text, root.category.clone(), parent) {
            Ok(_) => {
                e.set_text("");
                *ui.focus_sub.borrow_mut() = Some(root.uid.clone()); // keep typing after the rebuild
                request_sync();
            }
            Err(err) => ui.toasts.add_toast(adw::Toast::new(&err.to_string())),
        }
    });
    row.add_row(&entry);
    row
}

/// First line of the note; rows use plain text (`use_markup(false)`), so no escaping.
fn subtitle(t: &Task) -> Option<&str> {
    Some(t.description.as_deref()?.lines().next().unwrap_or_default())
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .valign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .build();
    b.add_css_class("flat");
    b
}

fn done_check(t: &Task) -> gtk::CheckButton {
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
    check
}

fn delete_button(t: &Task) -> gtk::Button {
    let del = icon_button("user-trash-symbolic", "Delete");
    let uid = t.uid.clone();
    del.connect_clicked(move |_| {
        if core().delete_task(uid.clone()).is_ok() {
            request_sync();
        }
    });
    del
}

fn edit_dialog(ui: &Rc<Ui>, t: &Task) {
    let b = view!("edit-dialog");
    let dialog: adw::AlertDialog = get(&b, "dialog");
    let summary: adw::EntryRow = get(&b, "summary");
    let category: adw::EntryRow = get(&b, "category");
    let description: adw::EntryRow = get(&b, "description");
    summary.set_text(&t.summary);
    category.set_text(t.category.as_deref().unwrap_or_default());
    description.set_text(t.description.as_deref().unwrap_or_default());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(uid: &str, parent: Option<&str>) -> Task {
        Task {
            uid: uid.into(),
            list_href: String::new(),
            summary: uid.into(),
            description: None,
            status: String::new(),
            done: false,
            completed_at: None,
            category: None,
            parent_uid: parent.map(Into::into),
            priority: None,
            due: None,
            sort_order: None,
            created: None,
            last_modified: None,
        }
    }

    #[test]
    fn trees_nest_depth_first_lists() {
        let tasks = [
            task("a", None),
            task("a1", Some("a")),
            task("a1x", Some("a1")),
            task("a2", Some("a")),
            task("b", None),
            task("c", Some("gone")), // parent closed or deleted: shown as a root
        ];
        let got: Vec<(&str, Vec<(usize, &str)>)> = trees(&tasks)
            .into_iter()
            .map(|(r, s)| {
                (
                    r.uid.as_str(),
                    s.iter().map(|(d, t)| (*d, t.uid.as_str())).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", vec![(1, "a1"), (2, "a1x"), (1, "a2")]),
                ("b", vec![]),
                ("c", vec![]),
            ]
        );
    }
}
