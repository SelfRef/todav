# Agent notes

## Settings

- Client settings are synced between devices by default. Any new setting syncs unless it only
  makes sense on one device (server URLs, UI state such as the last opened list); those go in
  `DEVICE_ONLY` in `core/src/settings.rs`.
- Store settings with `Client::set_setting` / `Client::setting`. `set_setting` records the change
  time; when syncing, the newer value wins implicitly per key (the server wins ties).
- Synced settings live in Nextcloud Files at `.config/todav/settings.json`, merged on every sync.
- The user can turn syncing off with "Sync settings with server" (Preferences → Connection),
  stored as the device-only `sync_settings` setting; on by default.

## Linux UI

- Static views are Blueprint files in `linux/app/ui/`, compiled by `build.rs`; dynamic UI
  (task rows, sidebar rows) stays in Rust code.
