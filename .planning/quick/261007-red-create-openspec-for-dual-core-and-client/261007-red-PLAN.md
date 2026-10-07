# Quick Task 261007-red: Create OpenSpec for dual-core and client behavior - Plan

**Date:** 2026-10-07
**Status:** Completed (documents and baseline verification; alignment implementation pending)
**Change:** `align-dual-core-and-client-behavior`
**Scope:** OpenSpec proposal/specification/task documents only; no Rust implementation in this quick task.

## Objective

Create a complete, implementation-ready OpenSpec change for aligning mihomo and
sing-box behavior and closing the corresponding CLI/TUI gaps against reviewed
upstream fixes. The change must be independently reviewable and must not alter
the existing `add-singbox-dual-core` change or any main capability spec.

## Non-negotiable boundaries

- Keep the currently running `clash-tui` process untouched: no stop, restart,
  signal, kill, service command, core switch, or live-core probing.
- Do not write real configs, sockets, pidfiles, managed binaries, or perform
  ignored E2E/real-core tests. Use temporary XDG directories only where a
  document-validation fixture needs them.
- Do not change the current `main` branch/worktree or touch untracked
  `.agents/`, `.omo/`, or zip files.
- Verification is limited to OpenSpec validation and the documented safe
  commands: serialized Rust tests with the real sing-box spawn test explicitly
  skipped, plus `cargo build --workspace --locked`; this planning task itself
  must not run tests or builds and must not commit.
- Preserve already aligned SIGTERM cleanup, auth gzip handling, and existing
  lifecycle behavior as requirements; do not describe them as a rewrite.

## Tasks

### Task 1: Create the proposal and architecture design

- **files:** `openspec/changes/align-dual-core-and-client-behavior/.openspec.yaml`,
  `openspec/changes/align-dual-core-and-client-behavior/proposal.md`,
  `openspec/changes/align-dual-core-and-client-behavior/design.md`
- **action:** Run the OpenSpec scaffold/status workflow for the exact kebab-case
  change name, then write the proposal and design from the current repository
  context. Pin official upstream references and state their roles: mihomo
  `1.19.32`, sing-box `1.14.2`, and Clash Verge Rev stable `2.5.7`; mention
  CVR dev `2.5.8` only as advisory. Link to the corresponding official release
  pages/changelogs/docs, record the review date, and distinguish verified
  behavior from assumptions. Define a single dual-core policy with offline
  version comparison and non-downgrade enforcement, an actual sing-box output
  parser (including malformed/partial output handling), atomic download,
  temporary-file cleanup, executable validation, and integrity verification.
  Cover the client/TUI contracts for nested `route.rule_set`, conversion
  references and critical fields, persistent `profile_dns_settings` with
  unknown YAML field preservation, backup inclusion of sing-box sidecars,
  core-specific daemon refresh, capability-gated long-operation timeouts,
  provider-scope duplicate handling, and bounded/coalesced traffic/log events
  while preserving lifecycle events. Explicitly mark existing SIGTERM/auth
  gzip alignment as preservation constraints and record that
  `add-singbox-dual-core` remains pending and untouched.
- **verify:** `test -f` all three artifacts and inspect the rendered Markdown
  for the pinned versions, official URLs, safety boundaries, every listed
  behavior, and a clear distinction between stable `2.5.7` and advisory
  `2.5.8`; no source or existing-change file appears in the diff.
- **done:** Proposal states the user value and scope, design contains contracts,
  invariants, failure/rollback behavior, compatibility policy, and explicit
  non-goals; all requested dual-core and client concerns are traceable to a
  design decision.

### Task 2: Add capability specs for the aligned behavior

- **files:**
  `openspec/changes/align-dual-core-and-client-behavior/specs/dual-core-compatibility-policy/spec.md`,
  `openspec/changes/align-dual-core-and-client-behavior/specs/dual-core-client-alignment/spec.md`,
  `openspec/changes/align-dual-core-and-client-behavior/specs/tui-event-and-operation-safety/spec.md`,
  `openspec/changes/align-dual-core-and-client-behavior/specs/subscription-auto-update/spec.md`,
  `openspec/changes/align-dual-core-and-client-behavior/specs/subscription-probe-recovery/spec.md`,
  `openspec/changes/align-dual-core-and-client-behavior/specs/proxy-batch-delay-test/spec.md`
- **action:** Write Given/When/Then normative scenarios, keeping the specs
  narrowly separated by concern. New capability specs use `## ADDED Requirements`;
  existing capabilities use `## MODIFIED Requirements` with complete requirement
  bodies copied from the main spec and revised scenarios. Existing main specs are
  read-only; changes belong solely in the new change's delta specs. The core-policy spec must define pinned
  versions, offline non-downgrade comparison, parsing the actual
  `sing-box version` output, atomic download/integrity/executable checks, and
  no live-core mutation. The client-alignment spec must define nested route
  `rule_set`, conversion references and critical fields, persistent DNS
  settings with unknown-field round-trip preservation, sidecar backup,
  core-specific refresh, and duplicate handling scoped to providers. The
  event/operation spec must define capability-gated long-operation timeouts,
  bounded/coalesced traffic and log events, lifecycle-event delivery, and
  preservation of SIGTERM cleanup/auth gzip behavior. Include compatibility
  scenarios for mihomo and sing-box and explicit rejection/error outcomes;
  do not copy or mark implemented any pending requirement from
  `openspec/changes/add-singbox-dual-core`.
- **verify:** The new change contains exactly six capability delta files; each contains an
  appropriate `ADDED` or `MODIFIED Requirements` section with at least one `### Requirement:` and a
  `#### Scenario:`; `rg` confirms all requested terms and confirms no new spec
  references a live core, real socket, or ignored E2E as a required test.
- **done:** Every behavior in the proposal/design has a normative scenario,
  including failure paths and preservation requirements; specs are additive
  and do not edit `openspec/specs/subscription-auto-update/spec.md`,
  `openspec/specs/subscription-probe-recovery/spec.md`,
  `openspec/specs/proxy-batch-delay-test/spec.md`, or the pending dual-core
  change.

### Task 3: Build the implementation task list and validate the change

- **files:**
  `openspec/changes/align-dual-core-and-client-behavior/tasks.md`
- **action:** Create an ordered, dependency-aware checklist that maps every
  spec requirement to implementation, unit/property tests, safe fixture tests,
  and review gates. Tasks must cover offline fixtures for the real sing-box
  version-output parser, version-policy edge cases, atomic download/integrity
  failure cleanup, nested rule-set/conversion preservation, DNS unknown-field
  round trips, backup sidecars, daemon refresh, timeout capability gating,
  provider-scope duplicate detection, and event coalescing/lifecycle ordering.
  Include explicit audit tasks for tests that could spawn real services and
  require them to be skipped by name, and document the exact safe verification
  commands: `cargo test --workspace --all-targets --locked --
  --test-threads=1 --skip real_sing_box_spawns_and_answers_controller` and
  `cargo build --workspace --locked`. Keep all task checkboxes unchecked and
  state that actual implementation happens only in a later apply workflow.
- **verify:** Run only `openspec validate --change
  align-dual-core-and-client-behavior` (or the repository's equivalent
  schema-validation command) and inspect its result; confirm the change status
  reports proposal, design, specs, and tasks complete/ready. Do not run Cargo,
  do not start or inspect a core, and do not commit.
- **done:** OpenSpec validation passes; task ordering and traceability cover
  all new specs, safety restrictions are executable, implementation remains
  wholly future work, and the working tree contains only the new OpenSpec
  change artifacts plus the requested plan-owned file.

## Must-Haves

- **truths:**
  - A reviewer can understand the dual-core policy, client/TUI alignment, and
    exact safety boundaries without reading abandoned prior changes.
  - Every requested behavior has a normative spec scenario and an unchecked
    implementation task with safe verification guidance.
  - Upstream compatibility is pinned to mihomo `1.19.32`, sing-box `1.14.2`,
    and CVR stable `2.5.7`, with dev `2.5.8` advisory only.
  - The plan protects the running `clash-tui` and leaves existing specs,
    pending `add-singbox-dual-core`, and unrelated untracked files untouched.
- **artifacts:**
  - `openspec/changes/align-dual-core-and-client-behavior/proposal.md`
  - `openspec/changes/align-dual-core-and-client-behavior/design.md`
  - `openspec/changes/align-dual-core-and-client-behavior/specs/*/spec.md`
  - `openspec/changes/align-dual-core-and-client-behavior/tasks.md`
- **key_links:**
  - `proposal.md` → `design.md` → six capability delta specs → `tasks.md`
  - upstream pinned references → version policy/parser/download requirements
  - client alignment spec → safe offline fixture and serialized test tasks
  - event safety spec → bounded/coalesced data events while retaining lifecycle
    event delivery

## Verification boundary

The document executor validates Markdown structure and OpenSpec schema without
running Cargo or committing. After document review, the parent runs the user's
requested isolated, serialized Rust tests and workspace build, records their
actual results in the quick summary, and commits only the new OpenSpec/GSD
documents. No service commands, core switches, signals, live sockets, real
configs, managed binaries, ignored E2E, or application runs are permitted.
