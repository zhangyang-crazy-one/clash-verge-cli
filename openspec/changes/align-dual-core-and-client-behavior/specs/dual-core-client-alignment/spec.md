## ADDED Requirements

### Requirement: Core-aware configuration conversion
The client MUST generate sing-box `rule_set` under `route`, preserve conversion references and reserved client-fingerprint data, and bind transport, security, DNS, and TUN fields to an explicit per-version capability matrix.

#### Scenario: supported fields preserved
- **WHEN** a profile contains supported route, rule-set, conversion, transport, security, or DNS fields
- **THEN** the selected core receives them at its supported schema location

#### Scenario: critical field unavailable
- **WHEN** a critical field cannot be represented by the selected core version
- **THEN** conversion rejects the affected node/config with the field and reason unless the user explicitly accepts a documented semantic degradation; it MUST NOT silently alter security or transport behavior

#### Scenario: skipped outbound leaves a group reference
- **WHEN** conversion skips an unsupported outbound that a group references
- **THEN** group references are rebuilt or the group is rejected before apply; dangling references and conflicts with reserved built-in tags MUST NOT be emitted

#### Scenario: legacy native DNS under sing-box 1.14
- **WHEN** a native subscription contains legacy DNS server address syntax or legacy top-level dns.fakeip
- **THEN** the client migrates it without semantic loss or rejects apply with a migration diagnostic while preserving the original profile and running configuration

#### Scenario: core-specific TUN stack preserved
- **WHEN** a profile retains an explicit supported gvisor stack or exposes mihomo mips configuration
- **THEN** the explicit setting is preserved and mihomo-only choices are not offered to sing-box

### Requirement: Lossless profile DNS persistence
The client MUST persist `profile_dns_settings` by profile UID with confirmation bound to source identity, preserve unknown valid root and nested YAML fields through read-edit-write, let typed edits win for owned fields, and distinguish unset inheritance from an explicit empty list.

#### Scenario: DNS confirmation survives restart
- **WHEN** a profile has a confirmed DNS override and the client restarts with the same subscription source
- **THEN** that profile retains its enabled/confirmed state without changing other profiles

#### Scenario: DNS source changes
- **WHEN** a confirmed profile's source changes
- **THEN** the source-bound confirmation is invalidated without logging credentials or disabling another profile's override

#### Scenario: unknown fields round-trip
- **WHEN** a known DNS field is edited in a YAML document containing unknown fields
- **THEN** the edit is persisted and all unknown valid fields remain unchanged

#### Scenario: DNS inheritance and clear
- **WHEN** a profile DNS field is unset or explicitly an empty list
- **THEN** unset inherits from profile/merge precedence while empty list clears the inherited value

#### Scenario: reload failure preserves valid state
- **WHEN** a new profile/config reload fails validation
- **THEN** the last valid configuration and persisted settings remain available and no secret URL is logged

### Requirement: Versioned sing-box sidecar backup
Backup and restore MUST include `singbox-dns.json`, `singbox-rules.json`, and `singbox-rule-sets.json` with staged all-or-nothing validation while excluding runtime JSON, sockets, pidfiles, downloads, and binaries.

#### Scenario: current archive includes sidecars
- **WHEN** a backup is created
- **THEN** the three versioned sidecars are included with existing permission and secret protections

#### Scenario: legacy archive restored
- **WHEN** an archive lacks one or more new sidecars
- **THEN** restore leaves existing values intact rather than resetting them

#### Scenario: invalid JSON sidecar aborts restore
- **WHEN** an archive contains a malformed or invalid persistent sing-box JSON file
- **THEN** all restore targets remain at their prior values and the failing file is identified

### Requirement: Atomic durable JSON settings
The client MUST validate and atomically replace durable sing-box DNS/rule/rule-set JSON files. A read or parse failure MUST be reported and MUST NOT be treated as an empty successful configuration.

#### Scenario: malformed settings file loaded
- **WHEN** loading a persistent JSON file fails
- **THEN** the client reports its path and reason and MUST NOT overwrite it with defaults

#### Scenario: interrupted settings save
- **WHEN** a settings save fails before atomic replacement
- **THEN** the previous complete file remains readable and the temporary file is cleaned up

### Requirement: Core-specific refresh and provider identity
Daemon refresh, forced probe recovery, rollback, and provider operations MUST dispatch through the selected core's capability interface, wait for readiness after reload, never start a stopped core implicitly, encode URL path segments, and deduplicate by provider provenance rather than display name.

#### Scenario: refresh targets selected core
- **WHEN** a due or forced refresh completes for a profile attached to sing-box
- **THEN** only the sing-box lifecycle is refreshed and success is reported after readiness

#### Scenario: ambiguous duplicate rejected
- **WHEN** two providers expose the same display name but provenance cannot be represented by the API
- **THEN** the operation rejects that target with an informative error and leaves uniquely identified targets eligible

#### Scenario: stopped core is not started
- **WHEN** a scheduler tick or probe sees a stopped core
- **THEN** it records a core-specific unavailable result without implicit startup

#### Scenario: staged configuration is rejected
- **WHEN** prevalidation rejects a staged candidate during an update
- **THEN** no live configuration replacement or restart occurs and previously valid runtime/profile state remains intact

#### Scenario: external sing-box attachment
- **WHEN** applying configuration would require restarting a sing-box core not owned by the manager
- **THEN** the client rejects that lifecycle action with an ownership explanation rather than stopping the attached core

### Requirement: Shared bounded profile script enhancement
The client MUST execute configured synchronous GUI-compatible `main(config, profileName)` hooks before mihomo generation or sing-box conversion, with the profile display name, bounded execution and no host filesystem/network APIs. Local and remote profiles MUST share enhancement ordering and preserve unknown supported fields and authoritative application controls.

#### Scenario: default or transforming script
- **WHEN** a Clash profile contains a no-op script or a valid script modifying configuration
- **THEN** both target cores receive the enhanced configuration through the same pipeline rather than rejecting the presence of a script

#### Scenario: invalid or unbounded script
- **WHEN** a script throws, returns an invalid configuration, or exceeds its execution limits
- **THEN** preparation reports the failing hook and retains the previous runtime and selection without silently omitting the script

#### Scenario: imported native JSON carries default enhancement references
- **WHEN** a native sing-box JSON import carries the generated empty fragments and canonical no-op script
- **THEN** loading preserves native JSON without adding Clash fields; genuinely nonempty Clash enhancements produce an explicit diagnostic

### Requirement: Stopped core selection independent of profile validity
Selecting a verified core while stopped MUST atomically update only its selection and marker, without profile execution, runtime generation, TUN authorization or implicit startup. The client MUST retain passive foreign-resource protections and full preparation/rollback for running switches.

#### Scenario: stopped selection with an unlaunchable profile
- **WHEN** the manager is stopped and the user selects another core while the active profile cannot be prepared for that core
- **THEN** selection succeeds without attempting preparation, and a later explicit start reports any remaining profile problem

### Requirement: Isolated non-TUN CLI instance while GUI is running
The client MAY operate its own core while the GUI is running only when it verifies separate configuration, private owned controller resources, loopback listeners, and TUN/system proxy disabled. It MUST reject shared or foreign resources and retain owned-process-only lifecycle control. Sing-box controller generation, API selection and readiness MUST use the same configured loopback endpoint.

#### Scenario: verified private instance
- **WHEN** an explicitly configured standalone instance has private independent resources and nonconflicting listeners with TUN/system proxy disabled
- **THEN** its owned lifecycle can proceed without adopting, contacting, signalling or reconfiguring the GUI

#### Scenario: isolation cannot be established
- **WHEN** the configuration/controller resources overlap, a listener is foreign, TUN/system proxy is enabled, or inspection cannot establish isolation
- **THEN** lifecycle apply is refused and the GUI remains untouched
