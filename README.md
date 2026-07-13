# clash-verge-cli

A Linux-first terminal client for [mihomo](https://github.com/MetaCubeX/mihomo). It provides an interactive Ratatui interface and small non-interactive commands while remaining compatible with an existing Clash Verge Rev configuration directory.

## Features

- Eight terminal views for status, proxies, profiles, connections, rules, logs, unlock status, and settings
- Proxy selection, delay tests, and ordered proxy-chain editing
- Subscription import, update, and active-profile switching
- Live traffic, connection, and log streams over the mihomo controller socket
- English and Simplified Chinese interfaces
- `start`, `stop`, `restart`, and machine-readable `status` commands

The current release target is Linux. System proxy integration uses GNOME/KDE tools, service management uses systemd, and process control uses Unix signals.

## Build

Rust 1.95 or newer is required.

```bash
cargo build --release -p clash-verge-cli
```

The binary is written to `target/release/clash-verge-cli`.

## Usage

Open the TUI using the default Clash Verge Rev data directory:

```bash
clash-verge-cli
```

Use another configuration directory or invoke a non-interactive command:

```bash
clash-verge-cli --config-dir ~/.local/share/io.github.clash-verge-rev.clash-verge-rev
clash-verge-cli status --json
clash-verge-cli start
clash-verge-cli stop
clash-verge-cli restart
```

The TUI looks for an already running Clash Verge Rev/mihomo controller before starting its own process. It prefers `clash-verge.yaml` and retains `config.yaml` as a compatibility fallback.

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
