## Context

The project is a Linux Rust CLI/TUI sharing clash-verge-rev YAML configuration while speaking to either mihomo or sing-box. Review on 2026-10-07 verified that the managed-binary fallbacks currently query latest versions unconditionally, while actual sing-box output is `sing-box version 1.13.12`; the compatibility baseline is mihomo 1.19.32 and sing-box 1.14.2. A synthetic check against sing-box 1.13.12 showed `rule_set` is accepted under `route` and rejected at the root. Existing daemon refresh, provider probing, delay timeout, DNS persistence, backup allowlists, duplicate identity, and TUI event flow have corresponding gaps. The pending `openspec/changes/add-singbox-dual-core` change remains separate and untouched.

The design must preserve existing graceful SIGTERM cleanup (5-second termination before kill), watcher generation/cancellation, logging/journald behavior, cleanup after render errors, gzip/basic empty-password TLS 1.2+ authentication, and fake-IP IPv6 handling. Verification uses fixtures and mocks; existing production controllers, configs, pidfiles and managed binaries must remain untouched. Disposable test-owned files, ephemeral mock listeners and existing tests that signal only their own non-core child processes are allowed. No application run, service management, real-core spawn or ignored E2E is permitted while the user's TUI is running. See [evidence.md](evidence.md) for findings and traceability.

## Goals / Non-Goals

**Goals:**

- Give both cores one explicit, pinnable compatibility policy with offline comparison and no silent downgrade.
- Parse real sing-box version output safely, validate staged executables and integrity, and make failed downloads recoverable.
- Preserve supported native config fields and unknown YAML fields, diagnose critical loss, and keep backup/refresh/probe semantics core-aware.
- Make long operations capability-gated and make high-volume TUI events bounded while lifecycle events remain reliable.

**Non-Goals:**

- Implementing the pending `add-singbox-dual-core` work or claiming live-core interoperability.
- Supporting every future sing-box protocol, guessed TUN field, legacy DNS provider shape, or full JavaScript hook runtime; unsupported critical fields must be rejected or explained.
- Changing GUI, WebView, Windows, or macOS behavior, or starting/stopping the user's running `clash-tui`.

## Decisions

1. **A single core policy with explicit capability matrices.** Store stable pinned candidates (mihomo 1.19.32, sing-box 1.14.2), compare versions offline, prefer compatible system binaries without mutating them, reuse a validated cache offline, and never silently downgrade. GitHub latest discovers updates but does not select a launch version. A newer binary is not compatible merely because its version is higher. These are candidate targets until implementation verification; they are not already-certified live-core support. Bind generated fields to a per-version supported matrix; preserve native JSON where supported and reject critical transport/security loss unless the user explicitly accepts the reported degradation.

2. **Parse executable output, not filenames.** Accept the official `sing-box version` form (including `--name` output where used), require an unambiguous semantic version, and reject malformed, partial, or conflicting output. Downloads use a lock, temporary file in the target directory, streamed integrity verification, executable-bit validation, atomic rename, and cleanup on every failure. A stale or invalid cache is never selected as a fallback.

3. **Use lossless config boundaries.** Read and write YAML through a representation that retains unknown root and nested fields; typed edits win only for fields they own. Compare preserved values with the original fixture, rather than only proving second serialization stability. Treat DNS unset as inheritance and an explicit empty list as clear. Persist `profile_dns_settings` by profile UID with confirmation scoped to the subscription source; retain confirmation across restart but invalidate it after source changes. Never log secret URLs. Generate `rule_set` beneath `route`, reserve built-in tags, normalize policy nodes and reject dangling references after unsupported nodes/groups are skipped. Preserve supported client-fingerprint fields, and reject critical fields that cannot be represented without explicit user acceptance. Self-generated DNS already uses typed servers; migrate or diagnose legacy native subscription DNS for 1.14. Keep mihomo-only `mips` out of sing-box choices and preserve the user's existing `gvisor` setting. Write durable sing-box JSON atomically and report malformed storage rather than treating it as empty.

4. **Make lifecycle orchestration core-aware.** Reuse existing core-aware apply helpers, with injectable operation seams for tests, rather than introducing a second supervisor. Dispatch scheduler/daemon/probe/rollback through the selected core, with readiness confirmation, cancellation/cooldown and selected-node rollback. Validate staged config before replacing the live file. Failed prevalidation must restore/retain both runtime and persisted profile state. A stopped core is never started implicitly; an externally attached core is not restarted without ownership. Product reload remains available when the user explicitly applies settings; the verification restriction concerns developer operations on the currently running instance. Encode URL path segments and resolve provider members within group `use`/`include-all-providers` scope. Do not invent unsupported API endpoints/parameters; reject genuinely unrepresentable ambiguity while retaining healthy unique targets.

5. **Separate deadlines from health.** Keep a 5-second health probe, but give delay requests a bounded deadline derived from `timeout_ms` plus response margin and give provider refresh a configurable/default 30-second request deadline. A timeout is not automatically proof that a core is dead; errors identify the selected core and operation.

6. **Bound data events, guarantee control events.** Coalesce traffic updates into a latest-value slot/watch and use a bounded log queue with dropped-count reporting. Use a separate reliable control/lifecycle channel with backpressure/reservation so start/ready/reload/stop/cancel events are never dropped. Producers must release config/manager locks before waiting on channel capacity. Render only when dirty within the rate budget, retain input responsiveness, and reject stale generations after cancellation. Performance gains require measurement rather than assumed CPU or memory claims.

7. **Version backups and verify atomically.** Add `singbox-dns.json`, `singbox-rules.json`, and `singbox-rule-sets.json` to the versioned allowlist and restore validation. Legacy archives leave absent fields untouched; staged restore commits all files together and retains existing permission and secret protections. Runtime JSON, sockets, pidfiles, downloads, and binaries remain excluded.

Alternatives considered: a latest-version-only policy was rejected because it causes unreviewed schema drift; a lossy typed rewrite was rejected because it discards GUI/new fields; an unbounded event channel was rejected because traffic bursts can starve control events; global provider-name deduplication was rejected because names lack provenance.

## Risks / Trade-offs

- **[Risk]** Version-specific schema matrices require maintenance as cores evolve → pin fixtures and require an explicit policy update for a new version.
- **[Risk]** Lossless YAML representation is more complex than typed serialization → constrain merge precedence and add round-trip fixtures before rollout.
- **[Risk]** Backpressure can delay lifecycle producers → reserve control capacity and test cancellation/overload with deterministic mocks.
- **[Risk]** System binaries may be incompatible despite a plausible version → validate output, executable behavior through offline fixtures, and integrity metadata without mutating the binary.
- **[Risk]** Real-core/TUN/network behavior remains unproven by unit tests → record that limitation and defer separately authorized isolated validation.

## Migration Plan

1. Add offline fixtures, policy/parser modules, capability matrices, and deterministic unit/property tests.
2. Add lossless persistence, conversion diagnostics, sidecar backup validation, and core-aware injected lifecycle paths.
3. Add scheduler/probe/provider/timeout changes, then bounded event channels and overload tests.
4. Run OpenSpec validation and, in the current workspace, run serialized tests with temporary XDG directories and `real_sing_box_spawns_and_answers_controller` skipped, followed by a locked workspace build. Do not use `--ignored` or `--include-ignored`. Live/network/TUN verification requires separate authorization and isolated resources.
5. Roll back by selecting the prior validated cache/config snapshot; failed staged downloads and restores leave the previous state intact.

## Open Questions

- Which upstream API endpoint and parameter represent provider provenance for each core? The implementation must use only an endpoint confirmed by fixtures and reject ambiguous cases.
- Which transitional sing-box 1.13.21 test profile, if any, is retained alongside the 1.14.2 candidate? It must be explicitly selected and never become an implicit downgrade.
