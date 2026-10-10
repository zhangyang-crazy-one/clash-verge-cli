## ADDED Requirements

### Requirement: Core selection
The TUI SHALL allow the user to select the proxy core (`mihomo` or `singbox`) via a settings entry, persisted in the verge config as `proxy_core` with default `mihomo`.

#### Scenario: Switch core
- **WHEN** the user selects a different core in settings and confirms
- **THEN** the TUI SHALL stop the running core process, generate the target core's configuration, and start the target core
- **AND** the status bar SHALL display the active core name

#### Scenario: sing-box binary missing
- **WHEN** the user selects `singbox` but no sing-box binary is found in the sidecar directory or PATH
- **THEN** the TUI SHALL keep the current core, show an informative error, and point to the expected binary location

### Requirement: Dual-transport API client
The API client layer SHALL support both unix socket transport (mihomo external-controller) and TCP transport (sing-box clash_api) behind a common interface.

#### Scenario: TCP connection to sing-box clash_api
- **WHEN** the active core is sing-box
- **THEN** all monitoring and control calls SHALL be sent over TCP to the configured `external_controller` address with Bearer secret auth

### Requirement: Core lifecycle management
The manager SHALL discover, spawn, stop, and watch the sing-box binary using the same lifecycle patterns as mihomo (sidecar directory first, PATH fallback).

#### Scenario: Version probe without starting
- **WHEN** the TUI probes for sing-box availability
- **THEN** it SHALL use the `sing-box version` subcommand so probing works while the core is not running

### Requirement: Core switch safety
Core switching SHALL be free of lifecycle conflicts between the outgoing and incoming core processes.

#### Scenario: Stale watcher cannot resurrect the old core
- **WHEN** the old core's exit event is processed by its watcher after the new core has started
- **THEN** the watcher SHALL NOT auto-restart the old core, because each watcher is bound to the generation it was spawned in

#### Scenario: Resources released before spawn
- **WHEN** the switch sequence spawns the new core
- **THEN** the controller port/socket SHALL have been verified released and, when TUN was enabled, the TUN device SHALL have been verified removed before spawn proceeds

#### Scenario: New core fails to start
- **WHEN** the new core fails readiness probing (timeout or immediate exit)
- **THEN** the TUI SHALL restore the previous configuration, restart the previous core as a fallback, and report the failure reason in the UI

### Requirement: Core-specific config apply strategy
Configuration changes SHALL be applied through a per-core strategy: mihomo via `PUT /configs` hot reload with file rollback on rejection; sing-box via process restart with readiness probing and rollback on failed start.

#### Scenario: Config change under mihomo
- **WHEN** the user changes configuration while mihomo is active
- **THEN** the change SHALL apply via hot reload without a core restart

#### Scenario: Config change under sing-box
- **WHEN** the user changes configuration while sing-box is active
- **THEN** the TUI SHALL write the config, restart the core, and verify readiness before reporting success

### Requirement: Single-manager ownership
The TUI SHALL prevent GUI takeover when running the sing-box core: switching to singbox SHALL be blocked while a GUI instance or GUI-managed core process is detected, and an ownership marker file SHALL be written in the data directory while sing-box mode is active.

#### Scenario: GUI detected during switch
- **WHEN** the user attempts to switch to singbox while a GUI instance is running
- **THEN** the switch SHALL be blocked with a message asking the user to exit the GUI first

#### Scenario: Ownership marker lifecycle
- **WHEN** sing-box mode becomes active
- **THEN** an ownership marker recording owner, core type, and pid SHALL exist in the data directory and SHALL be removed when the TUI exits sing-box mode

### Requirement: sing-box configuration skeleton generation
The TUI SHALL generate a sing-box JSON configuration containing inbounds (mixed, optional tun), `experimental.clash_api` bound to a local TCP address, log settings, and selector/urltest outbound groups derived from the profile's node list.

#### Scenario: Generate from profile nodes
- **WHEN** a sing-box profile is activated
- **THEN** the generated config SHALL contain one outbound per node plus a selector group matching the profile's group structure

### Requirement: Subscription conversion with full protocol coverage
The TUI SHALL convert clash YAML subscriptions into sing-box JSON profiles covering all subscription-relevant sing-box outbound protocols (ss, vmess, vless, trojan, hysteria, hysteria2, tuic, naive, shadowtls, anytls, snell, http, socks, wireguard), degrading at field level with a visible report.

#### Scenario: Unsupported field encountered
- **WHEN** conversion encounters a node field with no sing-box equivalent
- **THEN** the converter SHALL drop that field, continue converting the node, and include the drop in the degradation report

#### Scenario: Unconvertible node
- **WHEN** conversion encounters a node whose protocol cannot be represented in sing-box
- **THEN** the converter SHALL skip that node, continue with remaining nodes, and report the skip in the UI

### Requirement: Graceful degradation under sing-box
The TUI SHALL hide or disable features whose backend endpoints are non-functional under sing-box (rule providers panel, provider update action) instead of showing empty or failing views.

#### Scenario: Rules view under sing-box
- **WHEN** the active core is sing-box and the user opens the Rules view
- **THEN** the rules list SHALL render from `/rules`
- **AND** the rule providers panel SHALL be replaced with an explanatory notice rather than an empty list
