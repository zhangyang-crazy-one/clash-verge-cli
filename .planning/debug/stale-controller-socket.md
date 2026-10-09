---
status: resolved
live_validation: not_run
trigger: "两个核心都失败了；已验证 sing-box 1.14.2 和 mihomo v1.19.32 cached，均提示 An external controller socket has no CLI ownership record; manage it through its owner. The socket was left untouched."
created: 2026-10-09
updated: 2026-10-09
---

# Dual-core guided operation rejects controller socket

## Symptoms

- Expected: verified cached candidates can start/switch through the guided flow when no foreign core occupies the CLI endpoint.
- Actual: both core operations fail at the same no-ownership socket guard after version verification.
- Reproduction: user runs `cargo run --release`, verifies either cached candidate, then confirms apply in the TUI.
- Timeline: reported after guided-update changes at `514cf3a2`; the program remains running.
- Runtime boundary: passive metadata only; no real socket/controller connection, deletion, signal, core/app execution, installed-binary modification or real configuration writes. Only isolated fixtures/test/build.

## Current Focus

- hypothesis: `check_guided_record` equates a filesystem Unix socket path with a live externally owned controller, rejecting stale paths after graceful exit removes the PID record.
- test: inspect lifecycle cleanup and reproduce with a test-owned Unix listener dropped while its socket path remains; also preserve rejection of active external endpoints.
- expecting: stale fixture reproduces the user error; passive endpoint classification permits stale paths without weakening live/unknown ownership checks.
- next_action: none within the authorized implementation/test/build scope; parent independently reviewed source and final evidence. Live user validation requires manually loading the new binary; no developer runtime execution or mutation.

reasoning_checkpoint:
  hypothesis: A leftover Unix socket pathname is rejected as live because guided_record_check uses exists(), before the spawn barrier can clean it.
  confirming_evidence:
    - Dropped-listener actual wrapper fixture fails with the same no ownership record error (RED exit101).
    - Lifecycle removes the PID record on exit but preserves the socket pathname; preflight precedes resource_barrier.
  falsification_test: If dropped listener pathname were present in /proc/net/unix as a bound endpoint, the proposed stale classification would be invalid.
  fix_rationale: Use passive kernel presence plus owned socket/private parent metadata to distinguish stale from bound endpoints, retaining fail-closed policy and rechecking before unlink.
  blind_spots: Real user workflow cannot be executed under the explicit passive-only runtime constraint; separate syscalls cannot exclude a malicious same-uid replacement in the final unlink interval.

## Evidence

- timestamp: 2026-10-09; source `manager.rs` checks `socket_path.exists()` and rejects it when no PID record exists, without checking kernel endpoint state.
- timestamp: 2026-10-09; parent passive inspection found the actual pathname is a non-symlink AF_UNIX socket owned by uid1000, inode269, with no adjacent mihomo.pid and no exact /proc/net/unix path entry. No endpoint connection or removal was performed.
- timestamp: 2026-10-09; watcher.rs removes matching PID records after child exit, while stop() removes matching records without unlinking the socket. spawn_core_as calls resource_barrier only after guided_record_check, so the stale cleanup cannot be reached from the cold guided flow.
- timestamp: 2026-10-09; check_guided_record's socket refusal additionally requires record_pid.is_none(), allowing a live external endpoint to bypass the guard when an unrelated dead PID record exists. This adjacent guard defect must be covered without adopting or stopping an external core.
- timestamp: 2026-10-09; focused actual wrapper RED completed: 0 passed, 2 failed, exit101; logs /tmp/clash-stale-socket-validation/logs/red.log. Tests prove stale/no-record rejection and live/dead-record acceptance before implementation.
- timestamp: 2026-10-09; initial GREEN correctly refused fixture directories created with default umask permissions. Local tempfile source states directories use default permissions; fixture parents now explicitly mode0700. Product policy was not relaxed. Initial failed log preserved as green-initial-fixture-permissions.log.
- timestamp: 2026-10-09; final focused GREEN: 12 passed, 0 failed, exit0 (/tmp/clash-stale-socket-validation/logs/green.log). Both kinds accept stale/no-record without unlink during preflight, refuse bound/dead-record, and fail closed on unreadable/malformed kernel status; inode-preserving boundary recheck, alias-bound endpoint, private-parent-only cleanup, regular-file and symlink checks pass.
- timestamp: 2026-10-09; parent extended passive host inspection found no exact path and zero dev/inode-equivalent aliases among kernel table absolute paths for the actual user socket. This confirms the real case is unbound under the inspected namespace; no runtime mutation was performed.
- timestamp: 2026-10-09; first complete safe suite passed 613 CLI + 4 integration + 13 core tests (630 total), exit0, one real-core test filtered. Review identified the Missing early return can overlook a bound endpoint whose own pathname was unlinked; add a fixture and revalidate after narrow correction.
- timestamp: 2026-10-09; unlinked-live endpoint wrapper RED failed as predicted (exit101, unlinked-red.log). Missing paths now read kernel status too, refuse exact still-bound endpoints, and reject unreadable status. Final focused GREEN passed13/failed0 exit0; final source frozen for complete gates. Intermediate full suite preserved as full-suite-before-unlinked.log.
- timestamp: 2026-10-09; final exact safe suite passed 614 CLI +4 integration +13 core =631 total, 0 failed, 1 real-core test filtered, exit0 (/tmp/clash-stale-socket-validation/logs/full-suite.log). Locked debug build exit0; cargo fmt --all --check and git diff --check passed. Locked release build is pending; no built binary has been executed.
- timestamp: 2026-10-09; locked release build passed exit0, optimized compilation1m05s (build-release.log/build-release.exitcode). Source-only commit da535daa contains exactly controller_socket.rs, manager.rs and mod.rs. Debug/planning documents remain uncommitted for parent; no push performed.
- timestamp: 2026-10-09; artifact SHA256: debug CLI07ff3f8ffe67eb6c4eb316fa2e3d7b25ed64a8d7861a9b5a753a009c8841dbc5; release CLIb40f38672c6d875bbeb1d25367d88ab03004151b42d2b4a85d07f118bdfc15cd. Four artifact hashes recorded in /tmp/clash-stale-socket-validation/logs/artifacts.sha256; source hashes in source.sha256.

## Eliminated

## Resolution

- root_cause: Filesystem pathname existence is conflated with a bound endpoint before cold guided startup can reach stale cleanup; lifecycle removes the record but leaves the Unix pathname.
- fix: Passive Unix socket classification checks owned socket and private real parent, then readable parsed kernel state including filesystem aliases. Guided checks refuse live endpoints regardless of dead records; spawn barrier rechecks status/inode immediately before stale unlink and propagates failures. Diagnostics retain actual socket paths.
- verification: focused RED2 failures before fix; unlinked endpoint boundary RED1; final focused GREEN13; final exact safe full suite631 passed; locked workspace debug and release builds plus fmt check passed, all final gate exitcodes0. Source-only commit da535daa. Actual user workflow deliberately not executed, original socket untouched.
- files_changed: [src-tui/src/mihomo_manager/controller_socket.rs, src-tui/src/mihomo_manager/manager.rs, src-tui/src/mihomo_manager/mod.rs]
