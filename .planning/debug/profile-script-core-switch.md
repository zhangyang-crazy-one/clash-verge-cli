---
status: resolved
trigger: "singbox profile script execution is unsupported; remove the script override or use a preprocessed profile；singbox也不能在设置页面切换为mihomo，都是启动核心失败"
created: 2026-10-09
updated: 2026-10-09
---

# Profile scripts block both cores and stopped core selection

## Symptoms

- Expected: shared GUI-compatible Clash profile scripts enhance the profile before either mihomo output or sing-box conversion; a stopped Settings selection can choose mihomo without attempting startup or validating an unrelated sing-box profile.
- Actual: configured option.script unconditionally rejects preparation; both core targets share that path. Settings switching while stopped still prepares/validates the active subscription and reports a lifecycle failure, trapping selection.
- User screenshot: TUN off, permission present, Proxy Core singbox. Cached sing-box1.14.2 is verified, then script unsupported error.
- Runtime boundary: implementation debugger uses fixtures/test/build/passive metadata only; it never starts apps/real cores, contacts actual controllers, executes user scripts, invokes sudo/setcap/pkcheck, or modifies user configuration. Parent has explicit later authorization to operate a separate private TUI/core fixture and copied profile, while preserving GUI PID/start time and original configuration without privilege changes.

## Current Focus

- hypothesis: script support was explicitly absent in standalone composition, and stopped selection incorrectly depends on runtime configuration generation.
- test: fixture main(config, profileName) transformations/default no-op and failures across both generated core configs; stopped switch fixture with rejected script/native JSON must select without script execution, core startup, permission or runtime file writes.
- expecting: RED reproduces both independent defects; real bounded embedded JS and separate stopped-selection commit repair them without dropping configured scripts or relaxing conversion policy.
- next_action: all gates and parent private bidirectional live verification completed (see final Evidence entries); ready for commit after documentation sync.

reasoning_checkpoint:
  hypothesis: explicit script refusal causes the default GUI hook to reject both cores; preparation before the launch decision makes stopped selection dependent on profiles and TUN.
  confirming_evidence:
    - RED profile_script_default_and_real_transform fails with the exact unsupported-script error before even evaluating the default template.
    - RED stopped_selection_ignores fails at GUI ownership preflight despite launch=false and an intentionally nonexistent executable; initial code also unconditionally reads active_profile_yaml afterwards.
  falsification_test: if unchanged default script succeeds or launch=false returns without reading absent.yaml on current code, the hypotheses are wrong.
  fix_rationale: execute synchronous JavaScript within a bounded Rust engine before target conversion, and commit only CLI core selection/marker when manager is stopped.
  blind_spots: real core startup and copied user-profile execution are excluded for implementation debugger; parent owns those checks on explicitly private resources. Native JavaScript builtins have no OS/network hosts, with finite VM instruction/loop/recursion limits and input/output structure/size checks.

live_switch_reasoning_checkpoint:
  hypothesis: sharing the configured controller port makes target SingBox bind fail while old owned Mihomo holds it; guided_record_check only exempts an already-running SingBox child and therefore misclassifies the owned predecessor as foreign.
  confirming_evidence:
    - Parent private TUI successfully started Mihomo v1.19.32 with script changes applied; running switch to SingBox failed with address in use on the same configured controller, and Mihomo remained running.
    - guided_record_check target TCP guard skips its bind only when current_kind is SingBox, despite a tracked owned Mihomo PID and valid record.
  falsification_test: a mock owned Mihomo process holding the configured TCP port would already pass this guard, or actual socket ownership would belong to another PID.
  fix_rationale: prove ownership using tracked PID socket descriptor inodes and kernel TCP listener metadata instead of inferring ownership from current core kind.
  blind_spots: parent retains ownership of real process testing; debugger reproduces with local mock listeners only, including no-owner rejection and unknown ownership refusal.

reverse_selection_reasoning_checkpoint:
  hypothesis: the Mihomo target shortcut forwards sing-box direct/block tags unchanged, so target selector updates use nonexistent nodes instead of DIRECT/REJECT; neither target member nor leaf existence is validated before stopping the predecessor.
  confirming_evidence:
    - Parent private running SingBox to Mihomo switch reached selector restore, received HTTP 400 proxy-not-exist for direct versus DIRECT, and safely rolled back SingBox.
    - RED round-trip, exact-name precedence, and missing-target selector/member/leaf fixtures fail on the current guided_target_selections behavior.
  falsification_test: reverse direct/block selects the actual declared DIRECT/REJECT target members successfully before any mapping change, or the candidate already declares legal exact lowercase names.
  fix_rationale: prefer legal exact declared names, then only known reserved aliases in the target vocabulary, with target selector/membership/leaf validation before the stop boundary.
  blind_spots: custom names remain case-sensitive and native sing-box JSON remains non-convertible to Clash; parent owns actual process/HTTP verification.

## Evidence

- timestamp: 2026-10-09; runtime_config::compose_remote_profile rejects any configured option.script before reading its referenced file. chain::ChainType::Script also refuses all scripts.
- timestamp: 2026-10-09; manager.apply_prepared_core prepares active profile regardless of launch=false, so the same profile error blocks stopped selection of either core.
- timestamp: 2026-10-09; services/profile.rs apply_profile has a third blanket script rejection; sing-box profile switching reads raw YAML without enhancement, and active_profile_yaml treats a local base profile as an enhancement chain. These paths must share composition.
- timestamp: 2026-10-09; Boa 0.21.1 crate archives exist in Cargo registry cache; GUI-compatible default template is main(config, profileName) returning config. No knowledge-base file exists.
- timestamp: 2026-10-09; both new RED tests fail deterministically under /tmp/clash-profile-script-validation XDG roots. red-script.log reports blanket unsupported error; red-stopped.log reports GUI ownership rejection. No real process or configuration modified.
- timestamp: 2026-10-09; first GREEN script tests pass 3/3, and stopped selection passes with absent source profile, nonexistent executable and TUN enabled. Logs green-script.log and green-stopped.log under /tmp/clash-profile-script-validation.
- timestamp: 2026-10-09; parent pinned GUI v2.5.7 primary sources establish sequence rules/proxies/groups before app controls, then global Merge/Script and profile Merge/Script; default literal Script executes at both hook positions. Profile name is metadata name or empty. Merge top-level keys are ASCII lowercase, DNS children shallow-overlay, hosts replace, other maps deep-merge.
- timestamp: 2026-10-09; runtime focused tests pass 19/19 including default/global/local/remote hooks, import-created native JSON no-op fragments, correct UID/type/file resolution, preserved unknown fields and authoritative controls. Log green-runtime.log.
- timestamp: 2026-10-09; parent real old artifact baseline copied-user profile use reproduces third blanket refusal in services/profile.rs; old private TUI stopped selection reproduces GUI guard failure. Parent logs /tmp/clash-script-live-n4g0e9jx/logs/baseline-user-copy-profile-use.txt and baseline-stopped-failure.txt; GUI remained unchanged.
- timestamp: 2026-10-09; final focused DNS source-confirmation production regression passes after removing premature DNS overlay in the composer. DNS confirmation/global override remain after hooks at the existing single runtime boundary. Log green-dns-confirmation-final.log.
- timestamp: 2026-10-09; manager full safe suite reports 646 passed, 1 stale assertion failed, one exact real-sing-box test skipped. The failing existing command dispatch test expects the old remote-only missing-file diagnostic; actual shared composer reports the same missing fixture filename with failed-to-read context. No runtime behavior regression was observed.
- timestamp: 2026-10-09; stale command diagnostic assertion corrected to the concrete missing fixture filename plus exact unchanged metadata and unchanged manager kind/PID/executable. Targeted test passes, source re-frozen and all 17 checksums match source-freeze.sha256.
- timestamp: 2026-10-09; manager full safe final suite passes 664 tests (647 CLI unit, 4 integration, 13 core), zero failures, with only mihomo_manager::singbox_e2e::real_sing_box_spawns_and_answers_controller excluded. Locked debug build passes; artifact SHA256 0c316e8be28380ef38faebfe46c3174c40161aa7b8a7b4e5d28404092d67b5d1 delivered to parent for private live verification. Release build/fmt pending.
- timestamp: 2026-10-09; parent actual private TUI Mihomo startup succeeded and fixture script marker/rule applied. Subsequent running switch to SingBox failed safely at TCP ownership guard because old owned Mihomo holds configured port 49715. Evidence /tmp/clash-script-live-n4g0e9jx/logs/08-running-switch-singbox-failure.txt. Old core continued serving and GUI remained untouched. Source freeze reopened only for this concrete guard bug.
- timestamp: 2026-10-09; owned Mihomo controller RED reproduced actual address-in-use refusal; inode-based ownership fix passes two fixtures including preservation of the existing owned SingBox restart exemption. Parent next full safe suite passes 666 with exact real-core exclusion; this is intermediate pending later capability and reverse mapping repairs.
- timestamp: 2026-10-09; no-GUI tracked owned Mihomo FD PermissionDenied deferral is limited to address-in-use and guarded by mandatory target TCP availability after stopping the owned predecessor. Four mock fixtures pass for owned/foreign/unknown/GUI cases and rollback before spawn. Log green-controller-capability-fallback.log; no capabilities were altered.
- timestamp: 2026-10-09; parent actual Mihomo to SingBox switch succeeds, SingBox 1.14.2 controller/rules/local HTTP proxy pass. Reverse switch reveals unmapped direct tag, fails selector restore and rolls back the old SingBox service; evidence 21-ownfix-miho-back-running.txt. Reverse mapping RED fixture log red-reserved-selection-roundtrip.log records three failing tests.
- timestamp: 2026-10-09; reverse mapping now validates candidate selector/group membership and declared leaves before stop, preferring legal exact case-sensitive names over DIRECT/direct and REJECT/block aliases. Three focused fixtures pass in green-reserved-selection-roundtrip-final.log; cargo fmt succeeds. Capability-controller fixtures remain four passing in green-controller-capability-fallback.log. Source freezes for new complete gates; prior 664/666 suites and artifacts are intermediate evidence only.
- timestamp: 2026-10-09; frozen manifest SHA256 4a3d4d4af811d0ea25ce7f91959ced0903ad64afb7fb687c90db730542a69009 passes all 17 source checksums. Fresh safe-capability-final.log passes 671 tests (654 CLI unit, 4 integration, 13 core), zero failures, excluding only the exact real-sing-box test. Locked debug build passes with SHA256 31976f96746143158356e968ec28a4017331e83998e786bf959b58fb586a1775; parent received it for private bidirectional live verification. Release/fmt and parent verification remain pending.
- timestamp: 2026-10-09; parent (Pi takeover) found owns_listener raced under full-suite parallel load: /proc/self/fd entries can vanish between read_dir and read_link, failing the probe with ENOENT (2 failures in 655-test bin suite, 1-in-3 runs). Fix skips NotFound descriptors (closed fds cannot hold listeners; match still fails closed on inode mismatch) and adds churn regression owns_listener_tolerates_concurrent_fd_churn. 5 consecutive full suites pass 655 bin tests (672 across targets), zero failures, same real-sing-box exclusion.
- timestamp: 2026-10-09; parent private live round with final binary validated-debug-cli-pifix (SHA256 af80820fcc52d8f5c61014a95e99c2e3dc7087c7062a8e7647f66dcf8f52522c, includes fd-churn fix) passes bidirectional verification: sing-box 1.14.2 starts with script profile (original bug path), script rule converted into singbox.json and config.yaml marker/first-rule applied; running switches singbox→mihomo and mihomo→sing-box both succeed; HTTP proxy forwarding through mixed 35123 passes on both cores (logs proxy-pifix-box/miho/box-final.json, screens pifix-01..09). Re-frozen manifest SHA256 4c05711cc96d57d2e89db898a4eeb98d772dd71bd81109deb7714f9eccbc6811 matches all 17 sources; locked release/debug builds and cargo fmt --check pass. GUI/service/user PIDs+starttimes unchanged and all 46 user config file hashes identical after the round; private ports released and tmux fixture torn down.

## Resolution

- root_cause: standalone profile paths explicitly reject scripts or bypass enhancements; stopped switch unconditionally prepares selected profile and checks TUN before deciding whether to launch.
- fix: bounded Boa 0.21.1 synchronous hooks with disabled module loading and no external hosts; shared Clash local/remote composition before either core, native default no-op fragment compatibility; stopped selection commits only raw verge.yaml proxy_core/marker, retains runtime/executable cache and bumps shared confirmation epoch only on success. Narrow GUI coexistence requires distinct private non-TUN resources, loopback available TCP/UDP listeners, and actual owned-PID socket inode proof for live predecessor exemptions.
- verification: original script and stopped-selection RED/GREEN, three reverse selector fixtures and four capability-controller fixtures pass. Final frozen suite passes 672 tests across targets (655 CLI unit + 4 integration + 13 core) with zero failures in 5 consecutive runs, exact real-sing-box exclusion; fd-churn regression added. Locked debug/release builds and fmt pass; manifest SHA256 4c05711cc96d57d2e89db898a4eeb98d772dd71bd81109deb7714f9eccbc6811. Parent private live verification passed with final artifact af80820fcc52d8f5c61014a95e99c2e3dc7087c7062a8e7647f66dcf8f52522c covering sing-box script-profile startup, bidirectional running switches, script transform persistence and proxy forwarding, with GUI and user configuration verified untouched. User-network behavior, real TUN authorization and performance remain outside this isolated verification scope.
- files_changed: [Cargo.lock, src-tui/Cargo.toml, src-tui/src/chain.rs, src-tui/src/commands/mod.rs, src-tui/src/commands/profile.rs, src-tui/src/enhance/mod.rs, src-tui/src/main.rs, src-tui/src/mihomo_manager/manager.rs, src-tui/src/mihomo_manager/mod.rs, src-tui/src/mihomo_manager/gui_isolation.rs, src-tui/src/profile_store/store.rs, src-tui/src/profile_script.rs, src-tui/src/runtime_config.rs, src-tui/src/services/profile.rs, src-tui/src/subscribe/scheduler.rs, src-tui/src/tui/handlers/core_update.rs, src-tui/src/tui/handlers/profile.rs]
