# clash-verge-cli

A Linux-first terminal client for [mihomo](https://github.com/MetaCubeX/mihomo). It provides an interactive Ratatui interface and small non-interactive commands, fully standalone: it owns its data directory, its mihomo core, and its controller socket, with no runtime dependency on the Clash Verge Rev GUI.

## Features

- Eight terminal views for status, proxies, profiles, connections, rules, logs, unlock status, and settings
- Proxy selection, delay tests, and ordered proxy-chain editing
- Subscription import, update, and active-profile switching
- Live traffic, connection, and log streams over the mihomo controller socket
- English and Simplified Chinese interfaces
- `start`, `stop`, `restart`, and machine-readable `status` commands
- TUN mode with one-time capability setup (bordered askpass popup, works
  over SSH on headless servers; no root service or daemon)

## Mihomo core

On `start` (TUI `s` or `clash-verge-cli start`), the CLI resolves mihomo as follows:

1. Use a system `verge-mihomo` if present
2. Otherwise auto-download the latest release (fallback **v1.19.29**) into
   `$XDG_DATA_HOME/clash-verge-cli/mihomo` (or `~/.local/share/clash-verge-cli/mihomo`)
   and keep that managed binary in sync. Downloads are checked against the
   sha256 digest GitHub publishes for the asset, must be an ELF binary that
   reports the expected version, and are installed atomically under a lock
   shared by all running instances.

No separate install step is required for normal use.

## Build

Rust 1.95 or newer is required.

```bash
cargo build --release -p clash-verge-cli
```

The binary is written to `target/release/clash-verge-cli`.

## Usage

Open the TUI (standalone data directory, created on first run):

```bash
clash-verge-cli
```

Use another configuration directory or invoke a non-interactive command:

```bash
clash-verge-cli status --json
clash-verge-cli start
clash-verge-cli stop
clash-verge-cli restart
clash-verge-cli profile list
clash-verge-cli profile import 'https://example.com/sub.yaml' --name my-sub
clash-verge-cli profile update --all --reload
clash-verge-cli profile use my-sub           # uid or name
eval "$(clash-verge-cli sysproxy env)"   # proxy the current shell
```

Commands that talk to the running core (they fail with a hint when it is
not running):

```bash
clash-verge-cli proxy list                   # groups; `proxy list Proxy` for members
clash-verge-cli proxy select Proxy Tokyo
clash-verge-cli proxy delay Proxy            # every real node in the group
clash-verge-cli connections                  # `--json`, `close <ID>`, `close-all`
clash-verge-cli provider update --all
```

Settings that apply to the running core, or are saved for the next start:

```bash
clash-verge-cli mode global                  # `mode` alone prints it
clash-verge-cli tun on                       # after a one-time `tun setup`
clash-verge-cli sysproxy on                  # `off`, `status`
```

List commands take `--json` for scripting.

## System proxy

With *System proxy* enabled in Settings, the desktop proxy (GNOME, or KDE
Plasma 5/6) follows the core: it is applied when mihomo starts, with the
default LAN/localhost bypass list plus `system_proxy_bypass`, and released
when mihomo stops or exits, restoring the previous desktop settings. On
headless machines use `clash-verge-cli sysproxy env` (or `--unset`) instead.

## Logs

The core keeps running after `start` or the TUI exits; `stop`, `restart`,
and `status` find it from any later invocation. Its own output goes to
`<config-dir>/logs/mihomo.log` (the previous run is kept as
`mihomo.log.old`), and `start` reports the last lines of it if the core
fails to come up.

The TUI writes its own diagnostics to `<config-dir>/logs/clash-verge-cli-<date>.log`;
`start --foreground` and one-shot commands log to stderr (journald under
systemd). Use `-v`/`-vv`/`-vvv` or `RUST_LOG` (e.g. `RUST_LOG=info,mihomo=debug`)
for more detail.

## Migrating from Clash Verge Rev GUI

To import an existing GUI profile set (subscriptions, chain fragments,
settings) into the standalone directory:

```bash
clash-verge-cli profile migrate --from ~/.local/share/io.github.clash-verge-rev.clash-verge-rev
```

The CLI never reads the GUI directory at runtime; migration is one-shot.

## Keyboard shortcuts

| Keys | Action |
| --- | --- |
| `1`–`8` | Switch views |
| `Tab`, `h`, `l` | Move focus |
| `j`, `k`, arrows | Move selection |
| `?` | Toggle help |
| `/` | Filter connections or logs |
| `q` | Quit or dismiss the current overlay |
| `s`, `r`, `Shift+s` | Start, restart, or stop the core from Home |
| `i`, `u`, `Enter` | Import, update, or switch profiles |
| `t`, `Shift+t` | Test one or all proxy delays |
| `c`, `a`, `x` | Toggle, apply, or clear proxy-chain editing |

## Development

The commands used by CI are available as Cargo aliases:

```bash
cargo ci-fmt
cargo ci-clippy
cargo ci-test
```

## License and attribution

Licensed under GPL-3.0-only. The configuration compatibility layer is derived from [Clash Verge Rev](https://github.com/clash-verge-rev/clash-verge-rev); see [NOTICE](NOTICE).
