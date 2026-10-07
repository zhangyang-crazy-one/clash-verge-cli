## MODIFIED Requirements

### Requirement: Headless auto-refresh
The subscription auto-update scheduler MUST run in headless daemon mode (`start --foreground` / systemd) and interactive TUI mode, and MUST dispatch reload through the selected core's lifecycle capability.

#### Scenario: due subscription refreshed in daemon mode
- **WHEN** `clash-verge-cli start --foreground` is running, a remote profile has `allow_auto_update` enabled, and its `update_interval` elapsed
- **THEN** the profile is refreshed from its URL
- **AND** if current, the selected mihomo or sing-box core is reloaded and readiness is confirmed

#### Scenario: daemon stops cleanly during a refresh
- **WHEN** SIGTERM or SIGINT arrives while a refresh is in flight
- **THEN** the refresh is cancelled and the selected core stops cleanly using existing lifecycle behavior

### Requirement: CLI update interval configuration
The CLI MUST set a profile update interval and auto-update flag at import time.

#### Scenario: import with an explicit interval
- **WHEN** the user runs `clash-verge-cli profile import <url> --update-interval 15`
- **THEN** persisted profile data has `option.update_interval: 15` and scheduling uses it without manual edits

#### Scenario: import with auto-update disabled
- **WHEN** the user runs `clash-verge-cli profile import <url> --no-auto-update`
- **THEN** persisted data has `allow_auto_update: false` and the profile is never auto-refreshed

### Requirement: Failure cooldown
A failed refresh MUST NOT retry every scheduler tick, and cooldown state MUST be shared by the selected core operation.

#### Scenario: failed refresh cools down
- **WHEN** a remote refresh fails
- **THEN** the scheduler waits at least the profile interval and 30 minutes minimum before retrying, and clears cooldown after success

### Requirement: Fresh profile state in TUI
The TUI MUST observe external `profiles.yaml` changes without restart.

#### Scenario: external interval edit picked up
- **WHEN** `profiles.yaml` is edited externally while TUI runs
- **THEN** it rereads at least every five minutes and subsequent scheduling uses the new interval
