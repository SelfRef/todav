# Todav — Implementation Plan

Oct 8, 2026 · @Kris

## Overview

Todav is a personal, local-first task list synced through Nextcloud CalDAV (VTODO) with near-instant push via WebDAV-Push (`dav_push`) and a self-hosted ntfy server. One Rust core drives four front-ends: a GTK4/libadwaita app on Linux, a GNOME Shell extension for quick preview, an Android app, and a Pebble watchapp fed by the Android app.

**Goals**

- Same lists, same behaviour, same categories on every device; changes visible on other devices within seconds.
- Local-first: every UI reads and writes SQLite; the network is never on the UI path.
- Nextcloud Tasks web UI and DAVx5/Tasks.org keep working as fallbacks (no proprietary storage).
- Minimal dependencies: standard library and platform SDKs first; a third-party crate or library only where it replaces real work (HTTP/TLS, SQLite, XML, UniFFI).

**Non-goals (v1)**

- Multi-user sharing, assignees, comments.
- Recurring tasks, reminders/alarms, attachments.
- iOS, web front-end, non-Nextcloud servers (the core stays server-agnostic where cheap, but only Nextcloud is tested).

**Principles for the agent**

- The Rust core is the only place with sync, push, parsing and conflict logic. Front-ends are thin: they render state and call commands.
- Every feature lands in the core with tests against a real Nextcloud (docker compose in `infra/`) before any UI uses it.
- Push is best-effort by spec; polling with `sync-token` stays as the safety net everywhere.
- When in doubt, do the simpler thing and leave a `TODO(v2)` comment rather than a configuration option.

## Architecture

&#91;embedded content: nomorepo architecture: server, push transport, two apps, two satellites\]

Nextcloud is the only source of truth and both apps talk to it over plain CalDAV; ntfy only carries "something changed" signals, after which the app runs a sync-token pull. The GNOME extension and the Pebble watch hold no data of their own and go through their host app.

**Sync + push cycle (same on both apps)**

1. User action → core writes SQLite, marks the task dirty, emits `Changed` → UI updates at once.
2. Core pushes dirty tasks (`PUT` with `If-Match`), then pulls each list with `REPORT sync-collection`.
3. Nextcloud's `dav_push` sends a Web Push to the registered ntfy topic; ntfy fans it out: WebSocket to the Linux app, UnifiedPush to the Android app.
4. The receiving app maps the push topic to a calendar and runs step 2 for that calendar only.
5. A periodic sync (every 30 min, or 60 s while a window is focused) covers lost pushes.

## Data model

Storage is plain CalDAV: one calendar per list, one VTODO per task. Categories are `CATEGORIES` tags, not parent tasks. Subtasks use `RELATED-TO`.

| Concept | Where it lives | Notes |
| --- | --- | --- |
| List (Shopping, Personal, Work) | CalDAV calendar collection | Only calendars that advertise `VTODO` in supported-component-set are shown |
| Task | `VTODO` resource (`<uid>.ics`) | `UID`, `SUMMARY`, `STATUS`, `COMPLETED`, `PRIORITY`, `DUE`, `DESCRIPTION` |
| Category (Groceries, Home, Electronics, Mountains, Car) | `CATEGORIES` on the VTODO | Exactly one category per task in v1; multiple are read but the first is used for grouping |
| Subtask | `RELATED-TO;RELTYPE=PARENT:<parent uid>` | Depth 1 only in UI; deeper nesting is read and preserved but flattened |
| Manual order | `X-TODAV-ORDER` (integer) | Fallback: `CREATED` timestamp |
| Category metadata (order, icon, colour) | `todav/config.json` in Nextcloud Files via WebDAV | One file, fetched with `If-None-Match`; also pushed by notify\_push-free polling once per app start |
| Server change cursor | `sync-token` per calendar | Stored locally; `sync-collection` REPORT (RFC 6578) on every sync |
| Push identity | `DAV:Push` `topic` per calendar | Maps incoming push to a calendar |

**Local schema (SQLite, in the core)**

```sql
CREATE TABLE lists (
  href TEXT PRIMARY KEY, display_name TEXT, color TEXT,
  ctag TEXT, sync_token TEXT, push_topic TEXT,
  push_registration_href TEXT, push_expires INTEGER
);
CREATE TABLE tasks (
  uid TEXT PRIMARY KEY, list_href TEXT NOT NULL REFERENCES lists(href),
  etag TEXT, href TEXT, raw_ics TEXT NOT NULL,
  summary TEXT NOT NULL, status TEXT NOT NULL, completed_at INTEGER,
  category TEXT, parent_uid TEXT, priority INTEGER, due INTEGER,
  sort_order INTEGER, last_modified INTEGER,
  dirty INTEGER NOT NULL DEFAULT 0,     -- 0 clean, 1 modified, 2 deleted locally
  deleted_remote INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX tasks_list_cat ON tasks(list_href, category, status);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);  -- config.json cache, device id, etc.
```

`raw_ics` keeps the full original VTODO so properties the app does not understand survive a round trip. On write, the core patches the known properties in `raw_ics` and bumps `DTSTAMP`/`LAST-MODIFIED`/`SEQUENCE`.

**config.json** (category metadata only; everything else lives in VTODO)

```json
{
  "version": 1,
  "lists": {
    "shopping": { "categories": [
      { "name": "Groceries", "icon": "basket", "order": 0 },
      { "name": "Home", "icon": "house", "order": 1 }
    ] }
  }
}
```

Tasks whose category is not in config.json are grouped under "Other" at the end, so a missing or stale config never hides data.

## Repository layout

One monorepo, one Cargo workspace at the root, platform projects beside it.

```text
todav/
  Cargo.toml                 # workspace: core, cli, linux
  core/                      # todav-core (library)
    src/
      lib.rs                 # public API + UniFFI exports
      caldav/                # PROPFIND/REPORT/PUT/DELETE, XML builders + parsers
      ical/                  # minimal VTODO reader/writer
      store/                 # SQLite schema, migrations, queries
      sync/                  # engine: pull (sync-token), push (dirty queue), conflicts
      push/                  # WebDAV-Push registration, ntfy transport (Linux), message decode
      config/                # todav/config.json fetch/parse
      model.rs               # List, Task, Category, Change events
    tests/                   # integration tests against docker Nextcloud
    todav.udl                # UniFFI interface (or proc-macro exports)
  cli/                       # todav-cli: thin debugging/scripting front-end
  linux/
    app/                     # GTK4 + libadwaita app (Rust, gtk4-rs)
    data/                    # .desktop, metainfo, icons, gschema
    flatpak/                 # io.github.<you>.Todav.json
  gnome-extension/           # GNOME Shell extension (GJS), talks D-Bus to the Linux app
  android/
    app/                     # Kotlin + Jetpack Compose
    core/                    # Gradle module: UniFFI-generated Kotlin + libtodav_core.so
    pebble/                  # PebbleKit companion code (part of app module, separate package)
  pebble/                    # Pebble watchapp (C, Pebble SDK)
  infra/
    docker-compose.yml       # Nextcloud + Redis + ntfy + dav_push for tests
    nextcloud-setup.sh       # occ: enable tasks, dav_push; create test user + calendars
  scripts/
    migrate-categories.py    # one-off migration (see Migration)
  docs/
    ARCHITECTURE.md, SYNC.md, PUSH.md, PEBBLE_PROTOCOL.md
```

**Tooling**

- Rust stable; `cargo clippy -D warnings`, `cargo fmt`, `cargo test`.
- Android: Gradle, Kotlin, `cargo-ndk` to build `.so` for arm64-v8a (and x86\_64 for the emulator); UniFFI bindgen in a Gradle task.
- Linux: `cargo build -p todav-linux`; Flatpak manifest builds from the workspace with `cargo --offline` and a vendored sources JSON.
- Pebble: the open-source Pebble SDK toolchain (`pebble build`), pinned version in `pebble/README.md`.
- CI (GitHub Actions): core tests with the docker stack, clippy, Android debug build, Pebble build. No signing in CI.

**Dependency policy** (keep it short)

| Need | Crate / library | Why not std |
| --- | --- | --- |
| HTTP + TLS | `ureq` (sync, rustls) | No async runtime needed; keeps the core small |
| SQLite | `rusqlite` (bundled) | Standard choice, single file |
| XML | `quick-xml` | PROPFIND/REPORT bodies and multistatus responses |
| FFI | `uniffi` | Kotlin bindings generated from the Rust API |
| ntfy stream (Linux push) | — (`ureq`) | ntfy's `/<topic>/json` NDJSON stream over plain HTTP; no WebSocket crate needed |
| Web Push decryption | `ring` (HKDF, AES-128-GCM, RNG; already pulled in by rustls) + `p256` (static-key ECDH) | `dav_push` always encrypts (RFC 8291); `ece` drags in OpenSSL, ring can't load a stored ECDH key |
| iCalendar | hand-written in `core/ical` | Full iCal crates are large; VTODO needs \~15 properties, line folding and escaping |

Anything beyond this table needs a one-line justification in the PR.

## Rust core (`todav-core`)

The core owns all state and all network I/O. Front-ends get a `Client` handle, call commands, and subscribe to change events. The API is synchronous and blocking; callers run it on a background thread (GTK: `gio::spawn_blocking`, Android: coroutine on `Dispatchers.IO`).

**Public API (UniFFI surface)**

```rust
pub struct Client { /* SQLite conn, http agent, account */ }

impl Client {
    pub fn open(data_dir: String) -> Result<Client>;
    pub fn set_account(&self, url: String, user: String, app_password: String) -> Result<()>;
    pub fn account(&self) -> Option<Account>;

    // Read (never touches the network)
    pub fn lists(&self) -> Vec<List>;
    pub fn tasks(&self, list_href: String, include_done: bool) -> Vec<Task>;
    pub fn grouped(&self, list_href: String) -> Vec<CategoryGroup>;  // ordered by config.json

    // Write (local, marks dirty, emits Changed; sync pushes later)
    pub fn add_task(&self, list_href: String, summary: String, category: Option<String>, parent_uid: Option<String>) -> Result<Task>;
    pub fn set_done(&self, uid: String, done: bool) -> Result<()>;
    pub fn update_task(&self, uid: String, patch: TaskPatch) -> Result<()>;
    pub fn delete_task(&self, uid: String) -> Result<()>;
    pub fn reorder(&self, uid: String, before_uid: Option<String>) -> Result<()>;

    // Sync
    pub fn sync(&self) -> Result<SyncReport>;            // push dirty, then pull every list
    pub fn sync_list(&self, list_href: String) -> Result<SyncReport>;
    pub fn sync_topic(&self, topic: String) -> Result<SyncReport>;  // from a push message

    // Push
    pub fn push_register(&self, push_resource: String, expires_in_secs: u64) -> Result<Vec<PushRegistration>>;
    pub fn push_unregister(&self) -> Result<()>;
    pub fn push_decode(&self, body: Vec<u8>) -> Result<Vec<String>>;  // returns topics

    pub fn subscribe(&self, cb: Box<dyn ChangeListener>);  // Changed { list_href, uids }
}
```

**Modules**

1. `caldav` — discovery (`current-user-principal` → `calendar-home-set` → calendars with `VTODO`), `PROPFIND` for `getctag`, `sync-token`, `DAV:Push` `push-transports` and `topic`; `REPORT sync-collection`; `calendar-multiget` for changed hrefs; `PUT` with `If-Match`/`If-None-Match: *`; `DELETE` with `If-Match`.
2. `ical` — parse/serialize VCALENDAR with one VTODO. Handles line folding (75 octets), escaping (`\,` `\;` `\n`), `TZID` and `VALUE=DATE` on `DUE`, `RELATED-TO` with `RELTYPE`, multi-value `CATEGORIES`, unknown properties preserved verbatim. Property-tested against ics files exported from Nextcloud Tasks, Tasks.org and Errands.
3. `store` — rusqlite, migrations by `PRAGMA user_version`, one write transaction per command.
4. `sync` — the engine below.
5. `push` — WebDAV-Push registration per calendar; on Linux also the ntfy WebSocket listener (`push::ntfy::listen(url, token, on_message)`); on Android the app receives the message via UnifiedPush and hands bytes to `push_decode`.
6. `config` — fetch `todav/config.json` with `If-None-Match`, cache in `kv`, create it with defaults if 404.

**Sync engine**

1. Push phase: for each `dirty=1` task → `PUT` with `If-Match: <etag>` (or `If-None-Match: *` for new). `412` → fetch the server copy, run conflict rule, retry once. For `dirty=2` → `DELETE`; `404` counts as success.
2. Pull phase per list: `REPORT sync-collection` with the stored `sync-token`. Empty/invalid token (`403 valid-sync-token`) → full `calendar-query` for VTODO and rebuild. For each changed href → `calendar-multiget`, parse, upsert by UID; `404` entries → delete locally (unless `dirty=1`, then keep and re-push as new).
3. Conflict rule: compare `LAST-MODIFIED`; newer wins for the whole VTODO, except `STATUS=COMPLETED` always wins over an older `NEEDS-ACTION` (a tick on any device sticks). Log every conflict to `kv:conflict_log`.
4. Emit one `Changed` event per list with the affected UIDs.
5. `sync_topic(topic)` looks up the list by `push_topic` and runs only step 2 for it; unknown topic → full `sync()`.

**Push registration**

- After discovery, `PROPFIND` each calendar for `DAV:Push` `push-transports` and `topic`. If `web-push` is listed, `POST <calendar href>` with:

```xml
<?xml version="1.0" encoding="utf-8"?>
<push-register xmlns="DAV:Push">
  <expires>Thu, 09 Oct 2026 10:00:00 GMT</expires>
  <subscription>
    <web-push-subscription>
      <push-resource>https://ntfy.example.org/nmr-8f3a...?up=1</push-resource>
      <!-- add client-public-key / auth-secret here if dav_push requires encryption -->
    </web-push-subscription>
  </subscription>
</push-register>
```

- Store the `Location` header as `push_registration_href` and `push_expires`. Re-register when less than 25% of the lifetime is left or when the push resource changes (new UnifiedPush endpoint, new ntfy topic). `DELETE` old registrations.
- Polling fallback stays on: every 30 min in the background, every 60 s while a window is focused; a successful push resets the timer.
- `push_decode` accepts raw body bytes; if the first byte is not `{`/`<` and an `aes128gcm` content-encoding was indicated, decrypt with the stored keys (`ece`). Output is the list of topics (one in practice).

**Testing**

- Unit: `ical` round-trips, sync-engine state machine with a fake server trait.
- Integration: `infra/docker-compose.yml` brings up Nextcloud + Tasks + dav\_push + ntfy; tests create a calendar, write via Nextcloud's own API, assert the core sees it after `sync()`, then after a push message. Run with `cargo test --features integration`.

**CLI** (`todav-cli`): `login`, `ls`, `add`, `done`, `sync`, `push-register`, `listen` (ntfy loop → sync). Used to validate the core before any GUI exists and as a debugging aid later.

## Linux app (GTK4 + libadwaita)

A single Rust binary (`todav-linux`) that is both the GUI and a background service: it keeps the ntfy connection open, runs sync, and exposes a small D-Bus interface the GNOME extension uses. There is no separate daemon.

**Stack**: `gtk4-rs`, `libadwaita-rs`, `zbus` for D-Bus (the only extra dependency; it is the standard Rust D-Bus crate and also used by many GNOME apps). Settings in `GSettings` (account URL, user, ntfy topic); the app password in the Secret Service via `libsecret` (through `gio`/`secret` bindings, or `oo7` if lighter). State in `$XDG_DATA_HOME/todav/core.sqlite` via the core.

**UI**

- `AdwNavigationSplitView`: lists on the left (name, open count), tasks on the right.
- Task page: an `AdwPreferencesGroup`-style group per category in config order; rows are `AdwActionRow` with a `GtkCheckButton` prefix; subtasks indented under their parent row; done tasks collapsed in a "Done (n)" group at the bottom.
- Entry at the top of the list: type, Enter → `add_task` with the category of the last-added or currently expanded group; `Ctrl+K` category picker.
- Drag-and-drop reorders within a category → `reorder`.
- Toast for sync errors; a subtle "syncing" spinner in the header; manual refresh `F5`.

**Process model**

- `GtkApplication` with `HOLD` flag so the process stays alive when the window closes (`--gapplication-service` on login via autostart `.desktop` with `X-GNOME-Autostart`). Closing the window hides it; quitting is explicit (menu or `Ctrl+Q`).
- Background thread: `core::push::ntfy::listen` loop → on message `sync_topic` → emit `Changed` → UI refreshes via `glib::MainContext::channel`.
- Fallback timer: `glib::timeout_add_seconds` 60 s focused / 1800 s hidden → `sync()`.

**D-Bus interface** (session bus, name `io.github.<you>.Todav`, path `/io/github/<you>/Todav`)

```xml
<interface name="io.github.<you>.Todav1">
  <method name="GetLists">   <arg direction="out" type="a(ssu)"/> </method>            <!-- href, name, open_count -->
  <method name="GetTasks">   <arg direction="in" name="list" type="s"/>
                             <arg direction="out" type="a(sssb)"/> </method>           <!-- uid, summary, category, done -->
  <method name="SetDone">    <arg direction="in" type="s"/> <arg direction="in" type="b"/> </method>
  <method name="AddTask">    <arg direction="in" type="s"/> <arg direction="in" type="s"/> <arg direction="out" type="s"/> </method>
  <method name="ShowList">   <arg direction="in" type="s"/> </method>                    <!-- raise the window on that list -->
  <method name="Sync"/>
  <signal name="Changed">    <arg name="list" type="s"/> </signal>
  <property name="SyncState" type="s" access="read"/>                                 <!-- idle | syncing | error:<msg> -->
</interface>
```

The interface returns small tuples, not JSON, so the extension needs no parsing library. D-Bus activation (`DBusActivatable=true` + a service file) lets the extension start the app on demand.

**Packaging**: Flatpak on `org.gnome.Platform` 48 (or the current stable). Permissions: `--share=network`, `--talk-name=org.freedesktop.secrets`, `--own-name=io.github.<you>.Todav`. The ntfy WebSocket needs nothing extra.

## GNOME Shell extension

A panel indicator that shows the current list in a popup menu, lets you tick items and add one, and opens the app for anything more. It holds no state and does no networking: everything goes over D-Bus to the Linux app, which means the extension is \~300 lines of GJS with zero dependencies.

**Behaviour**

- Panel icon (symbolic checklist) with the open-task count of the pinned list as a small label; the pinned list is a GSettings key shared with the app (`io.github.<you>.Todav pinned-list`).
- Click → `PopupMenu`: list switcher at the top (`PopupSubMenuMenuItem`), then one section per category (`PopupMenuSection` with a `PopupSeparatorMenuItem` header), tasks as `PopupSwitchMenuItem`-style rows with a check ornament; toggling calls `SetDone` and keeps the menu open.
- Text entry at the bottom (`St.Entry`) → Enter calls `AddTask(list, text)` with no category (lands in "Other"; the app is where you categorise).
- "Open Todav" item → `ShowList(list)`.
- Subscribes to the `Changed` signal and `SyncState` property; rebuilds the menu on change; shows a small warning glyph when `SyncState` starts with `error:`.
- If the app is not running, the first D-Bus call activates it (D-Bus activation); the menu shows "Starting…" for that second.

**Implementation**

- `metadata.json`: `shell-version` 48 and 49; `settings-schema` reusing the app's compiled schema is not possible across Flatpak, so the extension ships its own schema with the single `pinned-list` key and the app reads it via `gsettings` host lookup; fallback is the first list.
- `Gio.DBusProxy.makeProxyWrapper(interfaceXml)` from the XML in the Linux section; copy the XML verbatim into `dbus.js`.
- Cap the popup at 25 tasks and show "+ n more" to keep the menu usable on small screens.
- Install: `make install` copies to `~/.local/share/gnome-shell/extensions/todav@<you>`; no extensions.gnome.org submission in v1.

**Non-goals**: editing text, reordering, categories, subtask display (subtasks appear flattened with a `↳` prefix).

## Android app

Kotlin + Jetpack Compose on top of the same core via UniFFI. Push arrives through UnifiedPush with the ntfy app as distributor; no Google services, no own foreground service.

**Dependencies** (AndroidX + Compose + these three): `net.java.dev.jna:jna` (required by UniFFI), `org.unifiedpush.android:connector`, and PebbleKit (vendored AAR, see Pebble section). No Room, Retrofit, OkHttp, Hilt or WorkManager-dependent libraries; the core already has storage and HTTP.

**Structure**

- `android/core`: Gradle library module; a `cargo-ndk` task builds `libtodav_core.so` for `arm64-v8a` and `x86_64`, then runs `uniffi-bindgen` to generate `uniffi/todav/todav.kt`.
- `android/app`: `TodavApp` (holds one `Client`), `MainActivity` (Compose), `SyncScheduler`, `PushReceiver`, `pebble/` package.

**UI (Compose, Material 3)**

- Bottom navigation or a list drawer mirroring the Linux layout; task screen is a `LazyColumn` with sticky category headers; checkbox rows; swipe-to-delete with undo snackbar.
- Quick-add bar at the bottom with a category chip row (from config.json) above the keyboard.
- Home-screen widget (`AppWidgetProvider`, `RemoteViews` list) for the pinned list: ticking an item sends a broadcast → `set_done` → `sync()`. This is the Android counterpart of the GNOME extension.
- State: a `ViewModel` per screen reads the core on `Dispatchers.IO` and re-reads on `ChangeListener` callbacks; no in-memory duplicate of the data.

**Push and sync**

1. On first launch, `UnifiedPush.registerApp(context)`; when `onNewEndpoint(endpoint)` arrives, call `client.push_register(endpoint, 86400)` on IO. On `onUnregistered`, `push_unregister()`.
2. `onMessage(bytes)` → `client.push_decode(bytes)` → for each topic `client.sync_topic(topic)`. UnifiedPush delivers in the background, so this runs in a broadcast receiver with `goAsync()` and a short coroutine; keep it under 10 s.
3. If the connector reports an encrypted Web Push message (UnifiedPush 3 connectors decrypt it before `onMessage`), the bytes are already plaintext; `push_decode` handles both.
4. Safety net: `WorkManager` (part of AndroidX, acceptable) periodic `SyncWorker` every 30 min with network constraint; plus `sync()` on app foreground.
5. Registration refresh: a `SyncWorker` run also re-registers push when `push_expires` is within 6 h.

**Account setup**: Nextcloud Login Flow v2 (`/index.php/login/v2`) in a Custom Tab yields an app password; store it in `EncryptedSharedPreferences` is deprecated, so use the Android Keystore to wrap it and keep the blob in `SharedPreferences`.

**Battery**: the ntfy app keeps the only persistent connection; this app wakes only on push or on the 30-min worker. Ask the user to exempt ntfy (not this app) from battery optimisation.

## Pebble watchapp + companion

The watch is a remote control for the phone's local database: it never talks to the network and never holds the source of truth. The Android app is the companion and pushes list snapshots over PebbleKit `AppMessage`.

**Watchapp (C, Pebble SDK)**

- Screens: list picker (`MenuLayer`) → task list grouped by category (section headers = categories) → select toggles done. Up/down long-press jumps between sections.
- Persists the last snapshot in watch storage (`persist_write_data`) so the list opens instantly and works when the phone is out of range; changes made offline are queued and sent when connected.
- Supports Aplite/Basalt/Chalk/Diorite/Emery and the Core Devices models; monochrome-safe design (no colour as the only signal).

**AppMessage protocol** (keys in `appinfo.json` / `package.json`; all strings UTF-8, ≤ 60 bytes, longer summaries truncated with `…` on the phone)

| Key | Direction | Payload |
| --- | --- | --- |
| `LISTS_BEGIN` | phone → watch | count |
| `LIST_ITEM` | phone → watch | index, short id (uint16), name, open count |
| `TASKS_BEGIN` | phone → watch | list short id, count, snapshot version |
| `TASK_ITEM` | phone → watch | index, short id (uint16), category index, done (0/1), summary |
| `CATEGORY_ITEM` | phone → watch | index, name |
| `END` | phone → watch | snapshot version |
| `REQUEST_LISTS` | watch → phone | — |
| `REQUEST_TASKS` | watch → phone | list short id |
| `SET_DONE` | watch → phone | task short id, done |
| `ACK_DONE` | phone → watch | task short id, snapshot version |

- The phone keeps a `short id ↔ UID` map per snapshot; short ids avoid sending 36-byte UIDs. A snapshot is at most 60 tasks (watch memory); more → the phone sends the first 60 open tasks and a trailing "+n more" item.
- Items are sent one `AppMessage` at a time with ack-driven pacing (`app_message_outbox_sent` callback); the companion resends on NACK up to 3 times.
- On `SET_DONE` the phone calls `set_done`, replies `ACK_DONE`, and triggers `sync()`.
- On any `Changed` event for the currently shown list, the phone pushes a fresh snapshot if the watch is connected.

**Companion (Android, `pebble/` package)**: `PebbleKit.registerReceivedDataHandler` for the UUID; a `PebbleBridge` singleton that serialises snapshots from the core; started lazily when the watchapp sends `REQUEST_LISTS`. Works with the Pebble app from Core Devices (PebbleKit intents are unchanged).

**Timeline/push**: no watch-side push; the phone's push → sync → `Changed` → snapshot path gives the watch updates within a few seconds while the app is in the foreground, and on next watch request otherwise.

## Migration of existing tasks

Today's lists use top-level tasks as category headers (e.g. "Spożywcze/gospodarcze", "Do mieszkania") with the real items as subtasks, and some headers are duplicated. A one-off script turns headers into `CATEGORIES` and removes them.

`scripts/migrate-categories.py` (stdlib only: `urllib`, `xml.etree`, `uuid`):

1. Discover calendars; for each, `calendar-query` all VTODO, parse `UID`, `SUMMARY`, `RELATED-TO`, `STATUS`.
2. A task is a header if it has at least one child and is itself not a child. Normalise its summary (trim, case-fold, collapse `/` and whitespace) → category name; merge duplicates ("Spożywcze/gospodarcze" and "Spożywcze/Gospodarcze" → one).
3. For each child: set `CATEGORIES:<name>`, drop its `RELATED-TO` to the header, `PUT` with `If-Match`.
4. `DELETE` the header. Headers with no children are kept as ordinary tasks (they may be real to-dos) unless listed in `--drop-empty`.
5. Write the discovered categories into `todav/config.json` in the order they appeared.
6. `--dry-run` prints the plan; the real run writes a backup of every touched `.ics` to `./backup/<calendar>/<uid>.ics` first.

Run it once after the core's `ical` module is proven on the same data (the script reuses nothing from the core on purpose, so it can run before any Rust exists).

## Progress

Deviation: no docker stack (`infra/`) for now — integration runs against the real instance (`cloud.aperte.dev`, user `Test`; ntfy at `ntfy.aperte.dev`, anonymous topics). Credentials live in a gitignored `.env`.

- [x] Cargo workspace (`core`, `cli`)
- [x] `core/src/ical.rs` — VTODO reader/writer, folding, escaping, unknown props + VALARM preserved (unit tests)
- [x] `core/src/caldav.rs` — XML tree, multistatus, discovery, `sync-collection` (incl. empty-token full sync), PUT/DELETE/GET with preconditions
- [x] `core/src/store.rs` — schema + migrations by `user_version`
- [x] `core/src/sync.rs` — push dirty queue, pull, conflict rule (unit test), `sync_topic`
- [x] `core/src/push.rs` — p256dh keys in `kv`, registration/refresh/unregister, aes128gcm decryption, ntfy JSON-stream listener
- [x] `todav` CLI: `lists`, `ls`, `add`, `done`, `undo`, `rm`, `sync`, `push-register`, `listen`
- [x] Auth: server is behind Authelia, so DAV needs an app password (in `.env`). Test user has task lists `Shopping` and `Work` (created via MKCALENDAR)
- [x] CLI round-trip against the server (add, done, edit on server → pull)
- [x] Integration tests (`cargo test -p todav-core --features integration`): two-device round trip, foreign props preserved, offline tick-vs-edit conflict → done, remote delete. Each test makes and deletes its own calendar
- [x] Push verified manually: server-side edit → dav_push → ntfy → decrypt → `sync_topic` in < 6 s
- [x] ical round-trip on a real Nextcloud export (318 VTODOs): byte-stable, patching SUMMARY touches only SUMMARY. Exports go in gitignored `tmp/` (test skips when empty). Fixed: empty `DESCRIPTION:`/`RELATED-TO:` read as absent, `PRIORITY:0` = none
- [ ] Automated push test + re-registration across expiry; invalid sync-token path test
- [x] `config` module: `todav/config.json` (ETag, created from categories in use if missing), `categories()`, `grouped()` (unknown categories get their own group after configured ones; uncategorised last as "Other")
- [x] Core: Login Flow v2 (`login_flow_start/poll`), `logout`, `setting/set_setting`; hand-written `json.rs` (no serde)
- [x] Integration fixture reuses `it-<name>` calendars (Nextcloud rate-limits MKCALENDAR: 10/h per user) — **unverified after this change; rerun once the limit allows**
- [ ] M3 migration script
- [x] M4 Linux app (`linux/app`): login via browser (Login Flow v2), app password in Secret Service (libsecret), lists sidebar, grouped tasks, add with category (Ctrl+K), tick, delete, edit dialog, Done expander, F5/Ctrl+Q, hide on close, `--gapplication-service`, sync worker + ntfy listener + 60 s/30 min fallback timer, D-Bus API. `make -C linux install` (desktop file, D-Bus activation, autostart). No GSettings: settings live in the core's kv. Verified via Broadway + D-Bus
- [ ] M4 leftovers: drag-and-drop reorder, undo for delete, app icon, Flatpak
- [x] M5 GNOME extension (`gnome-extension/`, ESM, Shell 48–51): pinned list, category sections, tick keeps menu open, add entry, list switcher, open app, warning on sync error. `make -C gnome-extension install`. Verified in a nested headless Shell
- [ ] M6 Android (+ UniFFI exports) → M7 Pebble
- [ ] Non-functional (last): README, `docs/`, CI, Flatpak

## Milestones

Each milestone ends with something you can use daily; nothing is started before the previous one is green in CI.

| # | Milestone | Done when |
| --- | --- | --- |
| 0 | Infra | `docker compose up` in `infra/` gives Nextcloud + Tasks + dav\_push + ntfy; `nextcloud-setup.sh` seeds a user and three lists; a push from a web edit reaches `test_client`-style listener |
| 1 | Core read/write | `todav-cli ls/add/done/sync` round-trips against the docker stack; `ical` round-trip tests pass on exports from Nextcloud Tasks, Tasks.org, Errands |
| 2 | Core push | `todav-cli listen` registers WebDAV-Push with an ntfy topic and syncs within 5 s of a web edit; registration refresh works across expiry |
| 3 | Migration | Script run in dry-run on the real account shows the expected plan; real run done; Nextcloud Tasks web shows categories as tags |
| 4 | Linux app | GTK app replaces Errands for daily use; survives window close; D-Bus interface answers `GetTasks`; Flatpak builds |
| 5 | GNOME extension | Popup shows the pinned list, ticks and adds; count label updates on `Changed` |
| 6 | Android app | Replaces Tasks.org for daily use; UnifiedPush via ntfy delivers in background; widget works |
| 7 | Pebble | Watch lists, ticks, works offline; snapshot refresh after a push on the phone |
| 8 | Polish | Conflict log visible in app settings; crash-free for 2 weeks; `docs/` complete |

## Open questions and risks

- [x] **dav\_push encryption** — resolved: `dav_push` (v1.0.3) *requires* `subscription-public-key type="p256dh"` + `auth-secret` and always sends aes128gcm. Namespace is `https://bitfire.at/webdav-push` (props `transports`, `topic`), registration is `POST <calendar>` with `push-register`, `201` + `Location`, max expiry 1 week, unregister = `DELETE <Location>`, echo suppression via `Push-Dont-Notify: "<Location>"`. Implemented in `core/src/push.rs`, verified against the RFC 8291 test vector. Original note: the WebDAV-Push draft leaves RFC 8291 message encryption as TODO, while DAVx5 states its pushes are end-to-end encrypted. Before writing `core/push`, read `nc_ext_dav_push`'s registration handler and capture one real DAVx5 ↔ Nextcloud exchange (dav\_push debug log). If keys are required: add `client-public-key`/`auth-secret` to the registration XML, generate P-256 keys in the core, store them in `kv`, and pull in `ece`. If not: skip encryption on this private server and note it in `docs/PUSH.md`.
- [ ] **dav\_push stability**: app-store build may lag GitHub; pin a git commit in `infra/` and in the server. Fallback if it misbehaves: a 50-line Nextcloud app that listens to `CalendarObject{Created,Updated,Deleted}Event` and `POST`s the topic to ntfy directly (the core's listener code stays identical).
- [ ] **ntfy topic security**: topics must be random (22+ chars) and the ntfy server should have ACLs so only the owner can subscribe; the Linux app authenticates with an ntfy token stored in the Secret Service.
- [ ] **Pebble SDK toolchain**: confirm the current open-source SDK builds for the user's watch model and that the Core Devices Pebble app forwards PebbleKit intents; otherwise fall back to the legacy Pebble app for the companion test.
- [ ] **GNOME Shell version**: target the Shell version installed on the user's machine; GJS APIs for popup menus moved between 45 and 48.
- [ ] **Nextcloud Tasks compatibility**: verify the web UI shows `X-TODAV-ORDER` tasks in a sane order (it uses its own `X-APPLE-SORT-ORDER`); consider writing both.

**Verification checklist for the agent** (run before marking any milestone done)

- [ ] `cargo test` incl. integration against the docker stack is green.
- [ ] A task created in Nextcloud Tasks web appears in the client under test within 5 s with the app in the background (Linux hidden window / Android not running).
- [ ] A task ticked on device A shows ticked on device B within 5 s; ticking the same task on both while offline resolves to "done" after reconnect.
- [ ] Killing the network for 10 min and restoring it produces no duplicates and no lost edits.
- [ ] Round-tripping a VTODO with `VALARM`, `DESCRIPTION` with newlines, and non-ASCII `SUMMARY` through the core changes only the intended properties (diff the `.ics`).
- [ ] No new third-party dependency without a line in the Dependency policy table.
