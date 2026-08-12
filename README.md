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
2. Otherwise auto-download **v1.19.29** into `$XDG_DATA_HOME/clash-verge-cli/mihomo`
   (or `~/.local/share/clash-verge-cli/mihomo`) and keep that managed binary in sync

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
clash-verge-cli profile update --all
```

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
