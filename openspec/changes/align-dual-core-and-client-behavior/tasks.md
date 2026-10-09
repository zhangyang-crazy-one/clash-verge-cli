## 1. Core policy and managed binaries

- [x] 1.1 Add pinned mihomo `1.19.32` and sing-box `1.14.2` policy data, offline semantic comparison, system-binary read-only preference, validated-cache reuse, and explicit no-downgrade errors; add edge-case unit/property fixtures.
- [x] 1.2 Implement the real `sing-box version` parser for complete official output and malformed, partial, conflicting, and non-zero-exit fixtures; include the installed-output shape without spawning a core.
- [x] 1.3 Implement cross-process locked staged downloads with streamed integrity verification, executable validation, atomic rename, temporary-file cleanup, and preservation of the prior validated binary on every failure.
- [x] 1.4 Add audit coverage proving compatibility tests cannot start, stop, signal, switch, or probe a live core, and document any real-core test name that must remain skipped.

## 2. Dual-core conversion and persistence

- [x] 2.1 Add versioned capability matrices for route/rule-set nesting, conversion references, client-fingerprint, transport/security, DNS, TUN, and native JSON; reject or explain critical unsupported fields and preserve supported unknown native fields.
- [x] 2.2 Fix sing-box output generation so `rule_set` is nested under `route`; add offline schema fixtures for accepted nested output and rejected root output, including skipped-node/group reference diagnostics.
- [x] 2.3 Persist `profile_dns_settings` by source identity with typed-field precedence, unset-inherits versus empty-clears semantics, and YAML root/nested unknown-field round-trip fixtures; preserve last valid data after reload failure and redact secret URLs.
- [x] 2.4 Add `singbox-dns.json`, `singbox-rules.json`, and `singbox-rule-sets.json` to versioned backup/restore validation; test staged all-or-nothing restore, legacy archive preservation, permissions/secrets, and exclusion of runtime files, sockets, pidfiles, downloads, and binaries.
- [x] 2.5 Validate and atomically save durable sing-box JSON settings, surface malformed reads instead of silently loading defaults, and test interruption/read-error paths without touching real files.

## 3. Core-aware client operations

- [x] 3.1 Introduce an injected core lifecycle interface for daemon refresh, scheduler ticks, forced probe refresh, readiness confirmation, rollback, and cancellation; test mihomo and sing-box dispatch without implicitly starting a stopped core.
- [x] 3.2 Add core-specific provider capability gates and diagnostics; percent-encode URL path segments and verify refresh success only after readiness.
- [x] 3.3 Resolve provider duplicate targets by provider/group/tag provenance; add fixtures for duplicate display names, representable identities, and informative rejection when the API cannot represent provenance.
- [x] 3.4 Separate 5-second health deadlines from delay deadlines derived from `timeout_ms` plus bounded response margin and configurable/default 30-second provider refresh deadlines; test that operation timeout alone does not mark a core dead.
- [x] 3.5 Preserve auto-update interval/disabled/cooldown behavior while making reload, forced refresh, rollback, and selected-node reapply core-aware; test controller errors are not node failures and stopped cores are not implicitly started.

## 4. TUI event and batch safety

- [x] 4.1 Replace unbounded traffic/log delivery with a latest-value traffic slot and bounded log queue with dropped-count reporting; add deterministic overload tests that do not assert wall-clock timing.
- [x] 4.2 Add bounded control/lifecycle delivery with reservation/backpressure, generation checks, cancellation, dirty-rate rendering, and ordered reload/ready/stop events; test stale events cannot resurrect a cancelled generation.
- [x] 4.3 Update batch delay target resolution to use scoped provider identity, retain stable unique leaves, reject ambiguous targets, enforce at most four concurrent requests, and prevent duplicate runs; add mock result/progress tests.
- [x] 4.4 Add regression tests for five-second SIGTERM cleanup, watcher cancellation, render-error cleanup, gzip/basic empty-password auth, TLS 1.2+, and fake-IP IPv6 preservation.

## 5. Verification and review gates

- [x] 5.1 Audit all tests and fixtures for access to existing processes, controllers, real configs, pidfiles, managed binaries, or ignored E2E; explicitly skip `real_sing_box_spawns_and_answers_controller`. Disposable mock endpoints and non-core test-owned child processes are permitted; never signal a production instance. Do not use `--ignored`, `--include-ignored`, `cargo run`, app lifecycle commands, service management or privileged installation during verification.
- [x] 5.2 Run the serialized safe test command in temporary `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, and `XDG_BIN_HOME` directories created under `mktemp` (do not repurpose `HOME` or `CARGO_HOME`): `cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller`.
- [x] 5.3 Run the locked workspace build: `cargo build --workspace --locked`.
- [x] 5.4 Review capability traceability from all six delta specs to implementation and unit/fixture tests, record mock/schema coverage versus unverified live/network/TUN behavior, and defer separately authorized real-core validation.

All 22 tasks are complete for the authorized offline implementation and verification scope. Real-core/network/TUN validation remains explicitly deferred; this change has not been archived or synced into the main specs. Implementation: `6aff4da7`. See [evidence.md](evidence.md).

## 6. Guided core update follow-up (quick 261009-eth)

- [x] 6.1 Inspect all read-only system candidates and integrity-verified managed caches before network access; recover from older unsupported versions with typed consent, while unknown newer versions require review.
- [x] 6.2 Require operation-bound download consent, official repo/tag/asset SHA-256 metadata or an exact official checksum manifest, bounded staging, actual version validation, and cancellation cleanup; retain prior executables at independent candidate paths.
- [x] 6.3 Make manager kind/API transport shared; prepare the current Clash subscription and nonprivileged preflight before stopping an owned core, commit after readiness, and await full config/selection/owner/kind/executable rollback on failure.
- [x] 6.4 Render check/consent/download/verify/verified-ready/prepare/switch/result in a responsive bilingual TUI; ready requires explicit apply confirmation, failures remain dismissible and retain a surviving old core, stale/cancelled operations cannot commit.
- [x] 6.5 Validate exclusively with isolated fixtures/mock endpoints and temporary XDG paths; explicitly skip the real-core E2E, run locked suite/build and format/diff/OpenSpec checks, and record fresh counts/log digests independently from historical 588-test evidence.

Follow-up: 5/5 complete under the isolated implementation scope; 27/27 total tracked implementation tasks. Code: `7b9a5136`, `bdf06fe6`, `dcf03232`, `71ba7ccb`. Fresh evidence follows the historical record in [evidence.md](evidence.md).

## 7. Stale controller socket regression follow-up

- [x] 7.1 Distinguish a stale CLI-private Unix socket file from a bound foreign endpoint using passive inspection; preserve live/unknown/unsafe endpoints and reject active sockets even with a dead PID record.
- [x] 7.2 Cover both guided target cores with test-owned stale and active Unix sockets, inspection failure and unsafe-path fixtures; capture a failing regression before the repair.
- [x] 7.3 Run the isolated serialized suite and locked dev/release builds without executing any real core/application or changing user runtime resources; record fresh evidence and artifact identities.

Socket regression: 3/3 complete; 30/30 tracked implementation tasks. Source: `da535daa`. Final isolated suite: 631 passed; locked dev/release builds passed. Live core startup remains unverified; see [evidence.md](evidence.md).
