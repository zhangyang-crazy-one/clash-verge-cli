# Quick Task 261007-ry8: Apply dual-core and client alignment — Plan

**Status:** Planned; implementation has not started.
**Source of truth:** `openspec/changes/align-dual-core-and-client-behavior/`
**Scope:** Implement every unchecked item in `tasks.md` (22 total), retaining the source list's numbering and acceptance coverage.

## Objective

Bring mihomo/sing-box policy, config conversion and persistence, subscription operations, and TUI event handling into the contracts in the six delta specs. Preserve existing config compatibility and the running `clash-tui` instance throughout implementation and verification.

## Non-negotiable execution boundaries

- Never interact with the currently running `clash-tui` or any core: do not run the app, `cargo run`, lifecycle commands, service management, privileged installation, signals, stop/restart/switch operations, or live probing.
- Never access or mutate real user config/controller/socket/pidfile/managed-binary paths. Tests and builds must use disposable `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, and `XDG_BIN_HOME` directories created beneath `mktemp`; do not change `HOME` or `CARGO_HOME`.
- Never run `--ignored` or `--include-ignored`. The safe test invocation explicitly skips `real_sing_box_spawns_and_answers_controller`.
- Tests may use fixture data, temporary test-owned files, mock endpoints, and child processes created and owned by that test. Audit test setup and cleanup before running the suite.
- Keep `openspec/changes/add-singbox-dual-core` untouched. Do not modify upstream source/configs or the main specs as a shortcut around the six delta specs.
- Do not stage broadly (`git add -A` / `git add .`). Stage only reviewed files owned by this task. Preserve unrelated untracked `.agents/`, `.omo/`, and `MindTheGapps-13.0.0-arm64-20231025_200931.zip`.
- Existing OpenSpec text and pinned values are binding. Mihomo v1.19.32 supports provider-scoped delay at `GET /providers/proxies/{providerName}/{name}/healthcheck`; exact-name lookup is scoped inside that provider. Use it for provider provenance, URL-encode each path segment, and reject duplicates within one provider. Unique native sing-box tags use `/proxies/{tag}/delay`. Resolve group scope from the selected effective profile's `proxy-groups.use` / `include-all-providers`; do not invent API parameters. Keep mihomo `mips` unavailable to sing-box, preserve explicit supported `gvisor`, and do not silently downgrade.
- Parent/orchestrator owns integration and the verification gate. Workers may implement only their assigned lane. Before dispatch, map touched files and test files from the actual code; record exact ownership and resolve any overlap before two workers run concurrently. No worker may edit a file owned by another lane.

## Execution lanes and dependency graph

The source checklist is grouped into three disjoint ownership lanes. First, the root integrator performs 5.1 as a read-only safety audit and fixes any unsafe test boundary before implementation work. Lanes A and B can then proceed concurrently after the ownership map is recorded. Lane C starts after A/B contracts it consumes are available; it must not take ownership of their files. Root integration items 5.2–5.4 run after all three lanes have landed and been reviewed.

| Wave | Owner | Tasks | Ownership boundary |
|---|---|---|---|
| 0 | Root integrator | 5.1 | Read-only audit of test/process/config paths; coordinate test exclusion. No feature implementation. |
| 1 | Lane A — binary policy and durable stores | 1.1–1.4, 2.4–2.5 | `src-tui/src/mihomo_manager/{binary.rs,singbox_binary.rs,mod.rs}` plus policy/parser-owned modules and tests; `src-tui/src/services/backup.rs` and the dedicated sing-box durable-settings module/tests. Do not edit `src-tui/src/singbox/**` or shared config types. Claim exact paths before work. |
| 1 | Lane B — config conversion and profile identity | 2.1–2.3, 3.3, 4.3 | `src-tui/src/singbox/**`, shared profile/config persistence files in `crates/clash-verge-core/src/config/**`, and provider/batch target resolution files in `src-tui/src/{mihomo_api,services/proxy.rs,tui/handlers/proxy.rs}` plus their tests. Lane B alone owns `singbox/mod.rs` and sing-box config model files. Do not edit Lane A or C paths. |
| 2 | Lane C — core-aware orchestration and TUI event flow | 3.1–3.2, 3.4–3.5, 4.1–4.2, 4.4 | `src-tui/src/subscribe/**`, `src-tui/src/commands/{daemon,provider}.rs`, `src-tui/src/tui/{event_loop.rs,handlers/**,app/**}`, and targeted existing lifecycle/auth/network safety modules/tests. Do not edit A/B paths; request root integration for overlaps. |
| 3 | Root integrator | 5.2–5.4 | Run safe serialized tests/build in temp XDG only after 5.1 passes; close traceability and document limits. No production-path access. |

If code inspection shows an ownership path differs from this map, update the map before editing. A shared file must be assigned to exactly one lane; downstream changes use that lane's handoff. This plan has no approval checkpoint.

## Required implementation checklist

### Lane A — binary policy and durable stores

1. **1.1 Pinned core policy.** Add mihomo `1.19.32` and sing-box `1.14.2` policy data, offline semantic version comparison, read-only preference for compatible system binaries, validated-cache reuse, and explicit no-downgrade errors. Add boundary/property fixtures for equal/older/newer/prerelease/malformed versions and incompatibility despite a higher version. Update selection so “latest” may discover updates but never selects the launch version.
2. **1.2 Real sing-box parser.** Parse complete official `sing-box version` output, including the installed `--name` shape. Reject missing, malformed, partial, conflicting version output and non-zero exit status. Test from captured/inline outputs only; do not spawn sing-box in this task.
3. **1.3 Safe managed download.** Serialize cross-process acquisition with a lock; download to a same-target-directory staging file; stream and verify integrity; validate executable permissions; atomically rename only after all checks; remove staging files on all error/cancel paths; retain the previous validated binary on every failure.
4. **1.4 Compatibility test audit.** Add/adjust audit coverage and test naming/documentation proving compatibility checks cannot start, stop, signal, switch, or probe a live core. Keep `real_sing_box_spawns_and_answers_controller` explicitly skipped in the safe suite.
5. **2.4 Sidecar backup/restore.** Include `singbox-dns.json`, `singbox-rules.json`, and `singbox-rule-sets.json` in versioned backup validation. Verify staged all-or-nothing restore, absent fields in legacy archives remain unchanged, permissions/secrets remain protected, and runtime files/sockets/pidfiles/downloads/binaries remain excluded.
6. **2.5 Durable JSON settings.** Validate and atomically save the sing-box JSON settings; report malformed existing storage instead of substituting defaults. Cover interruption/read-error paths using temporary test-owned files only.

### Lane B — config conversion and profile identity

7. **2.1 Per-version capability matrix.** Define explicit per-core/version capabilities for route/rule-set nesting, conversion references, client-fingerprint, transport/security, DNS, TUN, and native JSON. Reject or explain unsupported critical fields; preserve supported unknown native fields. Keep mihomo-only `mips` out of sing-box options and preserve explicit supported `gvisor`.
8. **2.2 Correct generated route shape.** Emit `rule_set` beneath `route`; add offline fixtures accepting the nested form and rejecting the root form. Verify diagnostics/rebuilt references when unsupported nodes or groups are skipped, including reserved-tag conflicts and dangling references.
9. **2.3 Lossless DNS/profile persistence.** Persist `profile_dns_settings` by profile UID with typed-field precedence and source confirmation. Canonical source identity is sorted JSON SHA-256 over `profile_uid + NUL + proxy-server-nameserver + proxy-server-nameserver-policy + nameserver-policy`, ignoring empty values; confirmation invalidates when these fields or UID change, not merely when subscription URL changes. Unset means inherit; explicit empty means clear. Preserve unknown root/nested YAML values and compare them against originals. Keep last valid state on reload failure and redact secret URLs.
10. **3.3 Provider duplicate provenance.** Resolve targets by provider/group/tag provenance, with fixtures for duplicate display names, representable identities, and useful rejection when the API cannot represent provenance. For mihomo v1.19.32 use `GET /providers/proxies/{providerName}/{name}/healthcheck` (its exact-name lookup is scoped to that provider); for unique native sing-box tags use `/proxies/{tag}/delay`. Encode each path segment and limit group members to the selected effective profile's `use` / `include-all-providers` scope. Verify these contracts against pinned official source/fixtures; do not invent parameters.
11. **4.3 Batch delay safety.** Resolve batch targets by scoped provider identity; preserve stable unique leaves, reject ambiguous targets, cap concurrent delay requests at four, and prevent duplicate simultaneous runs. Add mock result/progress coverage and use only core-supported, representable delay semantics.

### Lane C — core-aware orchestration and TUI events

12. **3.1 Injected lifecycle seam.** Introduce an injectable core lifecycle interface for daemon refresh, scheduler ticks, forced probe refresh, readiness confirmation, rollback, and cancellation. Cover mihomo/sing-box dispatch without implicitly starting a stopped core. Reuse current core-aware apply helpers; do not add a second supervisor.
13. **3.2 Capability-gated providers.** Gate operations by selected-core capabilities, return actionable diagnostics, percent-encode URL path segments, and report refresh success only after readiness confirmation.
14. **3.4 Separate deadlines.** Preserve the five-second health deadline. Compute delay deadlines from `timeout_ms` plus bounded response margin, respecting each API's numeric range; give provider refresh a configurable/default 30-second deadline. Prove an operation timeout alone does not mark a core dead.
15. **3.5 Auto-update and recovery.** Preserve configured interval, disabled state, and cooldown. Make reload, forced refresh, rollback, and selected-node reapply core-aware. Test controller errors as controller/operation failures rather than node failures; do not implicitly start stopped cores. Failed prevalidation must leave/restore runtime and persisted profile state.
16. **4.1 Bounded data events.** Replace unbounded traffic/log delivery with a latest-value traffic slot and bounded log queue that reports dropped counts. Use deterministic saturation/overload tests without wall-clock assertions.
17. **4.2 Reliable lifecycle/control.** Add bounded control/lifecycle delivery with reservation/backpressure, generation checks, cancellation, dirty-rate rendering, and ordered reload/ready/stop behavior. Producers must release config/manager locks before waiting for capacity. Test stale events cannot resurrect cancelled generations.
18. **4.4 Preservation regressions.** Add focused regressions for five-second SIGTERM cleanup, watcher cancellation, render-error cleanup, gzip/basic auth with empty password, TLS 1.2+, and fake-IP IPv6 preservation. Tests must remain fixture/mock/test-owned and must not signal any core.

### Root integration and gates

19. **5.1 Safety audit (Wave 0, before implementation).** Inspect all tests and fixtures changed or exercised by the work for access to existing processes/controllers/real configs/pidfiles/managed binaries or ignored E2E. Ensure the safe suite excludes `real_sing_box_spawns_and_answers_controller`. Do not run ignored tests, app commands, services, privileged install, or real core.
20. **5.2 Serialized safe suite (after all lanes).** Create disposable XDG directories under `mktemp`; run exactly `cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller` with XDG variables pointed into that temporary root. Capture output and remove only the created temporary root after processes exit. If any test touches outside it or requires the live app/core, stop and report instead of weakening the boundary.
21. **5.3 Locked build (after 5.2 passes).** Run `cargo build --workspace --locked` with the same temporary XDG environment. Do not use `cargo run` or launch any built binary.
22. **5.4 Traceability/review.** Map all six delta specs' requirements/scenarios to implementation and unit/fixture tests. Record mock/schema coverage separately from unverified live/network/TUN behavior. Preserve separate authorization for any real-core validation.

## Goal-backward must-haves

- Users get pinned, offline-evaluable dual-core selection; parser/download failures cannot replace the last validated binary, and downgrade is explicit.
- Generated/persisted sing-box configuration preserves supported fields and references, reports unsupported critical semantics, and does not lose unrelated YAML/JSON data.
- Backup/restore and refresh/probe/recovery operations are core-aware, failure-safe, and never implicitly start a stopped core.
- Provider and batch-delay behavior handles provenance, ambiguity, deadlines, and concurrency within the selected API's actual capabilities.
- TUI data events remain bounded while lifecycle/control events remain ordered, cancellable, and reliable; documented lifecycle/auth/network behavior remains covered.
- All test/build evidence comes from the safe isolated commands; the live `clash-tui`, its core, and real user paths remain untouched.

## Source coverage audit

| Source | Item | Coverage |
|---|---|---|
| GOAL | Proposal goal: align mihomo/sing-box policy and client/TUI behavior | 1.1–5.4 |
| REQ | `dual-core-compatibility-policy` | 1.1–1.4, 5.1–5.4 |
| REQ | `dual-core-client-alignment` | 2.1–2.5, 3.1–3.5, 5.4 |
| REQ | `tui-event-and-operation-safety` | 3.2, 3.4, 4.1–4.2, 4.4, 5.1–5.4 |
| REQ | `subscription-auto-update` | 3.1–3.2, 3.5, 5.4 |
| REQ | `subscription-probe-recovery` | 3.1–3.5, 5.4 |
| REQ | `proxy-batch-delay-test` | 3.3–3.4, 4.3, 5.4 |
| RESEARCH | `proposal.md` / `design.md`: pinned versions, offline policy, lossless config, core-aware lifecycle, bounded events, versioned backups | 1.1–4.4 |
| CONTEXT | Design decisions 1–7 and explicit non-goals | 1.1–5.4; all seven decisions retain their specified behavior and preservation constraints |

Pinned API evidence to carry into implementation review: Mihomo `v1.19.32/hub/route/provider.go` defines the provider-scoped healthcheck route and provider-local lookup; sing-box `v1.14.2/experimental/clashapi/proxies.go` defines its proxy delay route. CVR `v2.5.7/src-tauri/src/config/dns.rs` defines `ProfileDnsSettings` and source confirmation semantics. The project evidence artifact remains the canonical traceability record.

## Verification policy

No test/build is run during planning. During execution the root integrator performs 5.1 first, then runs 5.2 and 5.3 only after implementation review. Use only temporary XDG roots created under `mktemp`; never redirect `HOME` or `CARGO_HOME`. Do not use `--ignored`, `--include-ignored`, `cargo run`, application lifecycle commands, service management, privileged installation, or real-core tests. A failed gate is reported with the actual output and the prohibited boundary remains in force.

## Completion output

The apply workflow records one summary with the 22 source IDs, exact safe verification results, spec-to-code/test traceability, any unverified live/network/TUN behavior, and the reviewed file list. Commit scope, if the parent authorizes/owns committing in its workflow, is limited to the implementation and necessary test/docs files for this change; never include unrelated untracked files.
