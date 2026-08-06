# GUI Parity Roadmap

Phased plan to reach Clash Verge Rev GUI feature parity in `clash-verge-cli`.
Tray, global hotkeys, and lightweight window mode are explicitly out of scope.

## Done

- **Phase 1 — Subscriptions**: remote import/update (TUI `i`/`u` + `profile
  import|update|list`), Clash YAML validation, chain fragments, SSRF-safe
  fetch (gzip + Basic Auth included), mihomo **v1.19.29** auto-install on
  start.
- **Phase 2 — Profiles CRUD + enhance**: delete/rename/import, full enhance
  chain (merge/script/rules/proxies/groups), auto-update timer — now shared
  between the interactive TUI and the headless daemon
  (`subscribe::scheduler`), with CLI `profile import --update-interval
  <minutes>` / `--no-auto-update`, failure cooldown, and periodic
  `profiles.yaml` re-snapshot.
- **Phase 3 — System integration**: system proxy (GNOME/KDE), clash mode
  cycle, TUN via runtime config, `service install|uninstall|status` CLI.
- **Phase 4 — Proxies / Rules / observability**: proxies view, rules +
  rule-provider panels with provider update, close connection /
  close-all-connections, live traffic/connection/log streams. Log level is
  fixed at `info` (filtering is client-side).
- **Phase 5 — Settings / DNS / runtime**: settings view with `$EDITOR` for
  verge config / DNS / runtime YAML, port display.

## Remaining

- **Phase 6 — Unlock / media tests**: `unlock` view is a placeholder ("use
  the GUI workflow"); real media-unlock checks are not wired.
- **Phase 7 — Backup / WebDAV**: not started.
