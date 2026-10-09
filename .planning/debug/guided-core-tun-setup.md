---
status: awaiting_human_verify
trigger: "两个还是存在问题启动失败；TUN is enabled but '/home/zhangyangrui/.local/share/clash-verge-cli/mihomo-v1.19.32' lacks cap_net_admin,cap_net_raw+eip."
created: 2026-10-09
updated: 2026-10-09
---

# Guided dual-core update loses TUN setup continuation

## Symptoms

- Expected: after verified candidate selection, a TUN-enabled operation offers explicit permission setup for that exact candidate, rechecks capability and resumes the same start/restart/switch while retaining the old service during setup.
- Actual: both target cores fail with capability guidance after cached version verification; no inline permission setup continuation is offered.
- Reproduction: `cargo run --release`, TUN enabled, confirm verified-ready target core.
- Runtime constraint: never stop/launch a user core or app, contact live controllers, grant capabilities, invoke sudo/setcap/pkcheck, modify real config or installed binaries. Passive getcap is allowed; only isolated fixtures, mock, test/build.

## Current Focus

- hypothesis: the new guided core apply bypasses the existing TUN setup confirmation flow; explicit Settings setup still resolves mihomo regardless of the selected core and may operate on a different executable.
- test: inject capability/setup outcomes and assert exact prepared path/kind/id/intent preservation, cancellation/stale completion refusal and selected-core Settings targeting.
- expecting: RED reproduces missing guided setup and wrong Settings target; no real privilege operation is required to validate the repair.
- next_action: manager records completed source delivery and offline verification; user live-workflow verification is outstanding because executing real app/core/privilege changes was forbidden. Do not archive as resolved without that confirmation.

reasoning_checkpoint:
  hypothesis: guided apply skips explicit setup, while legacy setup loses the prepared core identity and cancels a running old core in UI state.
  confirming_evidence:
    - Ready directly calls apply_prepared_core with missing-capability hard guard.
    - RED decline test observed Stopped instead of Running, exit 101.
  falsification_test: injected missing capability opens no exact candidate prompt, or stale completion starts/changes any target.
  fix_rationale: retain exact operation identity and candidate through consent/setup/recheck, reject stale generations and preserve old runtime until apply.
  blind_spots: real sudo, live controller, core launch and user environment are forbidden; verification uses injected seams and temporary fixtures.

## Evidence

- timestamp: 2026-10-09; guided apply calls manager TUN preflight directly; Settings begin_tun_setup uses mihomo-only candidate/resolve functions even when the selected manager is sing-box.
- timestamp: 2026-10-09; RED guided_tun_decline_preserves_old_runtime_and_exact_operation fails Stopped != Running; /tmp/clash-guided-tun-validation/logs/red.log, red.exit=101, red.sha256.
- timestamp: 2026-10-09; legacy DNS skip test used production pkcheck path; introduced injectable rule-needed closure before running current suite. Prior execution history was not observed and is not asserted safe retroactively.
- timestamp: 2026-10-09; focused 11 guided_tun tests passed, including both cores/all lifecycle intents, exact-path capability recheck, stream cancellation, stale contexts, setup-only runtime preservation and bilingual long-path rendering.
- timestamp: 2026-10-09; RED originally drove missing-capability prompt via legacy helper and observed Running loss. Final regression uses the new exact guided result seam; legacy prompt cannot silently convert into guided privilege consent.
- timestamp: 2026-10-09; first workspace run failed only the old render assertion for missing polkit wording (625 passed/1 failed); neutral bilingual warning was corrected. Logs workspace-test.log and exit101 preserve that result.
- timestamp: 2026-10-09; setup-only UI now says permission check/setup rather than core selection committed and root bypass reports effective permission. actual-final source manifest hash 54432eb0ac61387d968f06c502d122bbf92697f78719a3d3a91f11047c1b2ca2.
- timestamp: 2026-10-09; actual-final workspace tests passed 627 CLI unit +4 integration +13 core =644, 1 real sing-box fixture filtered; debug build and fmt check passed. Parent independent source review PASSED.
- timestamp: 2026-10-09; actual-final release build passed exit0 in 1m04s. All four required gate exit files contain 0; frozen manifest verifies all 11 source files before source-only commit 19902dd1131591958f1c064c58ce6b3fa76690c6. No docs commit, push, merge or deployment performed.
- timestamp: 2026-10-09; release artifact target/release/clash-verge-cli SHA256 3fa59eaf01dd792bc999196af5e266bd8a4e49ad2841d49043b7542c464e4238, 12025904 bytes; debug SHA256 c5687a84f5dc444038793645fe4f47e6039389898375bdbaed14ce321d45091d. artifacts.sha256 records both without executing them.

## Resolution

- root_cause: guided Ready apply skipped TUN permission consent/continuation and old boolean resume lost candidate identity; explicit Settings resolved mihomo regardless of selected manager core.
- fix: exact guided consent/setup/result context and expected-generation manager boundary; selected exact-core offline Settings setup-only flow; managed target receipt recheck before granting; preserve old runtime/config on decline/failure/stale completion.
- verification: RED exit101 (red.log/red.exit/red.sha256); focused GREEN 12 passed (focused-green-frozen.log); actual-final full workspace 644 passed/0 failed/1 real-core fixture filtered. Commands cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller, cargo build --workspace --locked, cargo build --workspace --release --locked, cargo fmt --all -- --check all exit0. Logs under /tmp/clash-guided-tun-validation/logs/actual-final-*.log with paired *.exit. actual-final-source.sha256 manifest SHA256 54432eb0ac61387d968f06c502d122bbf92697f78719a3d3a91f11047c1b2ca2. Parent source review PASSED. No live core, controller, sudo, setcap, pkcheck or real config mutation performed in this repair verification. Prior history was not fully observed. Live privilege/start/restart/switch remains unverified; session is not archived/resolved.
- source_commit: 19902dd1131591958f1c064c58ce6b3fa76690c6
- files_changed: [src-tui/src/app/action.rs, src-tui/src/app/mod.rs, src-tui/src/i18n.rs, src-tui/src/mihomo_manager/binary.rs, src-tui/src/mihomo_manager/manager.rs, src-tui/src/services/tun.rs, src-tui/src/tui/handlers/core_update.rs, src-tui/src/tui/handlers/mod.rs, src-tui/src/tui/handlers/settings.rs, src-tui/src/tui/handlers/tun.rs, src-tui/src/ui/mod.rs]
