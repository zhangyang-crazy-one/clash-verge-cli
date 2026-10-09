---
phase: quick-261009-eth
plan: "01"
type: execute
wave: 1
depends_on: []
files_modified:
  - src-tui/src/mihomo_manager/binary.rs
  - src-tui/src/mihomo_manager/singbox_binary.rs
  - src-tui/src/mihomo_manager/manager.rs
  - src-tui/src/subscribe/client_meta.rs
  - src-tui/src/app/action.rs
  - src-tui/src/app/mod.rs
  - src-tui/src/tui/handlers/mod.rs
  - src-tui/src/tui/handlers/lifecycle.rs
  - src-tui/src/tui/handlers/settings.rs
  - src-tui/src/tui/event_loop.rs
  - src-tui/src/tui/input.rs
  - src-tui/src/ui/mod.rs
  - src-tui/src/ui/views/settings.rs
  - src-tui/src/i18n.rs
  - openspec/changes/align-dual-core-and-client-behavior/design.md
  - openspec/changes/align-dual-core-and-client-behavior/tasks.md
  - openspec/changes/align-dual-core-and-client-behavior/specs/dual-core-compatibility-policy/spec.md
  - openspec/changes/align-dual-core-and-client-behavior/specs/tui-event-and-operation-safety/spec.md
  - openspec/changes/align-dual-core-and-client-behavior/evidence.md
autonomous: true
requirements: [QUICK-CU-01, QUICK-CU-02, QUICK-CU-03, QUICK-CU-04]
must_haves:
  truths:
    - "旧系统 mihomo 1.19.29 或 sing-box 1.13 不再截断客户端自有兼容缓存/更新路径；系统二进制保持只读。"
    - "TUI 能打开并显示核心检查与更新确认；用户同意后才下载，能看到 download/verify/ready 的真实状态和验证后版本。"
    - "准备失败或取消保持旧核心；目标就绪后由客户端完成有所有权约束的切换，成功后配置、manager kind、API transport 一致。"
    - "切换失败恢复旧配置、选择器、kind、运行文件与先前运行的自有核心；失败诊断不会把仍存活旧核心标记为停止。"
    - "离线兼容缓存无需 release API；未知更高版本不自动降级；不放宽 fixed targets 或静默丢弃 sing-box 关键配置字段。"
    - "交付证据来自临时目录、fixture、mock、隔离 test/build；当前 GUI、真实核心、控制端点、系统二进制不被开发验证操作。"
  artifacts:
    - path: src-tui/src/mihomo_manager/binary.rs
      provides: "mihomo 兼容候选检查与用户授权后的 staged acquisition"
    - path: src-tui/src/mihomo_manager/singbox_binary.rs
      provides: "sing-box 同等 guided acquisition 与版本诊断"
    - path: src-tui/src/mihomo_manager/manager.rs
      provides: "共享 kind 与注入式 prepare/commit/rollback 切换事务"
    - path: src-tui/src/tui/handlers/lifecycle.rs
      provides: "启动/重启接入更新确认与 prepared binary 消费"
    - path: src-tui/src/ui/mod.rs
      provides: "可取消更新确认、阶段进度、验证后版本与结果渲染"
  key_links:
    - from: src-tui/src/tui/handlers/settings.rs
      to: src-tui/src/mihomo_manager/manager.rs
      via: "目标准备成功后调用切换事务，不先 stop，不只保存 proxy_core"
    - from: src-tui/src/tui/handlers/lifecycle.rs
      to: src-tui/src/mihomo_manager/binary.rs
      via: "typed inspection -> consent -> verified prepared candidate -> launch"
    - from: src-tui/src/tui/handlers/lifecycle.rs
      to: src-tui/src/mihomo_manager/singbox_binary.rs
      via: "sing-box 使用同等检查/授权/验证流程"
    - from: src-tui/src/mihomo_manager/manager.rs
      to: src-tui/src/tui/background.rs
      via: "共享核心代际过滤旧快照，更新结果具有独立 operation identity"
---

<objective>
修复 6aff4da7 后旧系统核心导致启动被 strict policy 提前拒绝的问题，并交付用户确认后的可信更新、验证版本展示和可回滚双核心切换。

Purpose: 保留明确兼容策略，让用户在 TUI 内完成安全更新；避免当前 settings 先停止核心、仅保存 proxy_core 却继续使用旧 manager kind 的行为。
Output: Rust 实现、mock/fixture 回归、现有 OpenSpec 小范围 follow-up 与隔离验证证据。固定目标继续为 mihomo 1.19.32 / sing-box 1.14.2。
</objective>

<execution_context>
@/home/zhangyangrui/.codex/get-shit-done/workflows/execute-plan.md
@/home/zhangyangrui/.codex/get-shit-done/templates/summary.md
</execution_context>

<context>
@AGENTS.md
@.planning/STATE.md
@.planning/quick/261007-ry8-apply-dual-core-and-client-alignment-ope/261007-ry8-SUMMARY.md
@openspec/changes/align-dual-core-and-client-behavior/design.md
@openspec/changes/align-dual-core-and-client-behavior/specs/dual-core-compatibility-policy/spec.md
@openspec/changes/align-dual-core-and-client-behavior/specs/tui-event-and-operation-safety/spec.md
@src-tui/src/mihomo_manager/core_policy.rs
@src-tui/src/mihomo_manager/binary.rs
@src-tui/src/mihomo_manager/singbox_binary.rs
@src-tui/src/mihomo_manager/manager.rs
@src-tui/src/tui/background.rs
@src-tui/src/tui/handlers/lifecycle.rs
@src-tui/src/tui/handlers/settings.rs
@src-tui/src/commands/mod.rs

Locked instructions from this quick request (local trace IDs; no separate CONTEXT.md exists):
- D-01: 网络可用时提示需更新，用户确认后下载校验，展示验证后的版本和状态，再完成切换；TUI 不能被 resolver 错误挡在 shell 外。
- D-02: check/download/verify/ready 均可见且准备阶段可取消；原核心保持直到目标 binary/config/preflight 准备成功。
- D-03: 旧系统 binary 可跳过，使用 CLI 自有缓存/更新；不覆盖 GUI 系统 binary，不放开全部版本，不自动降级未知 newer。
- D-04: 保留 trusted SHA-256、安装锁、原子写入；上游 digest 缺失须可信官方校验元数据或明确诊断，不能降级为未经校验下载。
- D-05: 成功才提交选型；manager、inner、所有 clone、API transport、watcher 一致；失败恢复旧配置/选择器/kind/运行状态，保留 unknown fields。
- D-06: 保留所有权边界；运行 GUI/外部核心不能被自动停止、探测、采用或修改以完成 sing-box 切换。
- D-07: 开发仅 isolated test/build、tempfiles、captured version fixtures、mock endpoints；禁止运行应用、真实核心 version/check、真实控制端点、GUI probe/stop/restart/signal、系统 binary 与提权安装；显式跳过 real_sing_box_spawns_and_answers_controller。
- D-08: 用户准确症状是 settings 切换 sing-box 没有显示/没有效果，并期待直接转换当前订阅；支持的订阅自动转换，不要求手工准备 sing-box JSON。unsupported critical fields 保留明确可见 diagnostics，不 silent drop，也不臆造不存在的 sing-box 报错。

<interfaces>
Existing contracts and integration facts:
- binary::ResolvedMihomo { path: PathBuf, source: MihomoBinarySource, version: String }; singbox_binary::ResolvedSingBox has the corresponding fields.
- binary::resolve_or_install() and singbox_binary::resolve_or_install() currently bail on the first incompatible system candidate; managed candidates are never reached in that branch.
- core_policy::is_compatible(core, observed, required) accepts only reviewed exact stable targets; is_newer_than prevents implicit downgrade. Keep this behavior.
- commands::build_manager(config_dir) reads configuration and adopts only CLI pid records; it does not resolve/download a binary. event_loop::run builds the shell; lifecycle::start/restart perform acquisition later.
- MihomoManager::core_kind()/api() currently use the outer copied CoreKind, while ManagerInner stores a separate AtomicU8 kind. with_core_kind updates both only at construction. Make shared committed selection authoritative before adding runtime switching.
- ManagerInner::generation and EventSender::{for_current,accept,cancel_and_wait} already gate stale core snapshots and wait for owned-future cleanup. Preserve these mechanisms; a UI cancellation must not accidentally abort unrelated traffic before candidate preparation succeeds.
- lifecycle::note_error currently clears pid/runtime caches unconditionally. Update/switch failures need a separate result that retains old running status.
- Existing orchestrate_restart / orchestrate_start_singbox are injected lifecycle seams; extend their transaction pattern with verified-candidate input and rollback rather than building a new updater service.
</interfaces>
</context>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: 解开旧系统候选阻断并建立可信、可取消的核心准备契约</name>
  <files>src-tui/src/mihomo_manager/binary.rs, src-tui/src/mihomo_manager/singbox_binary.rs, src-tui/src/subscribe/client_meta.rs, openspec/changes/align-dual-core-and-client-behavior/design.md, openspec/changes/align-dual-core-and-client-behavior/tasks.md, openspec/changes/align-dual-core-and-client-behavior/specs/dual-core-compatibility-policy/spec.md, openspec/changes/align-dual-core-and-client-behavior/specs/tui-event-and-operation-safety/spec.md</files>
  <behavior>
    - captured mihomo 1.19.29 / sing-box 1.13 -> incompatible candidate recorded, compatible validated cache still selected offline.
    - no compatible cache -> typed NeedsUpdate with observed/source/required version and intended action; no binary download before confirmation.
    - unknown higher system/cache version -> explicit review diagnostic; no automatic installation/selection of a lower target.
    - compatible validated cache -> no release API call; corrupt receipt/cache, malformed/non-zero version output -> diagnostic or explicit acquisition, never eligible silently.
    - asset digest or trusted official SHA-256 manifest succeeds; missing/ambiguous/malformed metadata, wrong archive digest, ELF/version mismatch, cancellation -> prior candidate untouched and staged artifacts cleaned.
  </behavior>
  <action>Per D-01/D-03/D-04/D-07/D-08, add compact typed inspection/acquisition contracts inside the existing binary modules: ready verified candidate, update needed, unavailable/review-required diagnostic, and check/download/verify/ready progress carrying core, target, operation identity and actual validated version. Inject filesystem paths/candidate lists, captured version probe results and HTTP responses for tests. Inspect all read-only system candidates, then validated managed cache; an older unsupported system candidate is recorded/skipped rather than returned as a fatal error. Do not mutate system modes or files. Valid cache selection is fully offline, with no informational latest lookup ahead of it; fixed targets remain exact and latest remains informational. Separate inspection from confirmation-gated download for TUI while retaining existing noninteractive command semantics. Keep process/cross-process locks, streamed checksum, unique temporary staging, digest-qualified receipts, executable/ELF checks, runtime-only version probe and atomic publication. Distinguish release HTTP failure, missing asset, null digest, unsupported digest algorithm and invalid checksum text. Where official releases provide a checksums asset, accept only an HTTPS official repo/tag-bound manifest entry for the exact archive filename with an unambiguous valid 64-hex SHA-256; otherwise preserve prior binary and report an actionable failure. A computed local hash is never its own trusted expected digest. Product runtime may probe staged version; developer tests must inject fixture output and never execute a core. Update the existing two delta specs and design with these narrow scenarios, retain the existing 22 checked tasks, append follow-up tasks instead of rewriting completion evidence, and do not sync/archive main specs.</action>
  <verify><automated>In the temporary-XDG validation environment below: cargo test -p clash-verge-cli --bin clash-verge-cli --locked guided_core -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller; newly added tests in these files must include guided_core in their names. openspec validate align-dual-core-and-client-behavior --strict</automated></verify>
  <done>Both resolvers expose recoverable update preparation, old system cores cannot hide valid managed cache, downloads require the TUI authorization token, trusted integrity remains mandatory, and new fixture tests pass without core processes or real files.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 2: 实现 manager 真实变更和可回滚核心切换事务</name>
  <files>src-tui/src/mihomo_manager/manager.rs, src-tui/src/tui/handlers/settings.rs</files>
  <behavior>
    - preparation/config-generation/TUN-preflight failure or cancellation -> no stop, no persisted selection, original core remains running.
    - prepared running switch -> prepare before stop, target spawn/readiness before commit, only then publish success/config/marker; never two cores on the shared listener simultaneously.
    - switched shared manager -> all clones and inner report target kind, API transport and later auto-restart dispatch match target; subsequent starts consume the verified candidate rather than reselecting the rejected system binary.
    - stopped selection change -> prepared candidate and selection committed consistently without implicitly starting a core.
    - stop/spawn/readiness/config-save/marker failure -> rollback prior binary/config/kind/selector/marker and restore a previously running owned core; rollback failure reported separately with accurate status.
    - selected subscription -> existing sing-box conversion prepares supported runtime config automatically; critical unsupported fields -> visible preparation diagnostic with no stop or partial selection.
    - GUI/foreign owner or changed ownership/generation -> refuse the lifecycle mutation with a visible owned-core/GUI explanation; no external process signal or controller request.
  </behavior>
  <action>Per D-02/D-05/D-06/D-08, make the committed selection shared and authoritative for MihomoManager clones, api(), inner/core_kind(), auto-restart and watcher dispatch; avoid updating only inner AtomicU8 while outer copied kind remains stale. Extend the injected lifecycle orchestration with a verified prepared candidate and staged target runtime config. Define prepare/commit/rollback boundaries and test every boundary before production wiring. Snapshot old executable selection (retaining a usable old managed binary if acquisition replaces its path), runtime files, GUI config including unknown YAML fields, ownership marker, runtime kind, selected-node state and running/owned state. Resolve, digest/version validate, generate target config with existing critical-field diagnostics and perform nonprivileged TUN preflight before any stop or durable selection change. Recheck ownership/generation before commit. For an owned running core, stop only after preparation succeeds, start the prepared target with its correct API transport, await readiness, then save proxy_core/update marker and publish success; no second resolve call may undo the prepared selection. For stopped cores, commit selection without auto-start. Restore snapshots on any later failure; restarting the old core is permitted only when this transaction stopped a previously owned running core. Keep cancellation cleanup awaited so exit/stop cannot race rollback into resurrecting a core. Remove settings' stop-first/save-next-start flow and its binary-not-found hard block; make it request this preparation/switch flow. Retain the GUI/foreign-owner coexistence gate, returning an actionable message without stopping the GUI. Tests use injected hooks/mock endpoints; do not invoke production gui_process_running, system config, process or capability operations.</action>
  <verify><automated>In the temporary-XDG validation environment: cargo test -p clash-verge-cli --bin clash-verge-cli --locked guided_core -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller; switch tests must include guided_core in their names and assert operation order, all clones' kind/API transport, rollback contents and zero mutation hooks before ready.</automated></verify>
  <done>The transaction changes the actual manager type/transport, retains original service during preparation, commits durable selection only after readiness, restores prior state on failure, and never operates a GUI/foreign process.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 3: 接入 TUI 更新确认、阶段展示与隔离验收证据</name>
  <files>src-tui/src/app/action.rs, src-tui/src/app/mod.rs, src-tui/src/tui/handlers/mod.rs, src-tui/src/tui/handlers/lifecycle.rs, src-tui/src/tui/handlers/settings.rs, src-tui/src/tui/event_loop.rs, src-tui/src/tui/input.rs, src-tui/src/ui/mod.rs, src-tui/src/ui/views/settings.rs, src-tui/src/i18n.rs, openspec/changes/align-dual-core-and-client-behavior/tasks.md, openspec/changes/align-dual-core-and-client-behavior/evidence.md</files>
  <behavior>
    - initial shell/start/restart/settings switch -> check visible; missing/old core opens confirmation with observed version, target, managed destination and download/validation explanation.
    - y/Enter -> one authorized background operation; n/Esc -> decline/cancel, zero download before confirmation, no old-core stop during preparation.
    - checking/downloading/verifying/ready/switching/success/failure/cancelled states render actual phase; verified version appears only after validation, UI remains responsive under stalled mock download.
    - bounded progress and duplicate input/stale completion -> one operation, generation/identity reject late results; q/exit waits owned cleanup and does not mutate another core.
    - preparation/switch error with old core surviving -> old Running/pid/version remain visible alongside update diagnostic; shell survives offline/missing metadata and offers retry.
    - successful settings switch -> settings core row and verified version/status visibly change in this session; GUI/foreign-owner block or subscription conversion failure -> visible dismissible overlay, not an ephemeral status hidden by refresh.
  </behavior>
  <action>Per D-01/D-02/D-05/D-07/D-08, add narrowly scoped update state/actions/confirmation overlay and English/Chinese text using existing input/overlay/render patterns. Wire lifecycle start/restart and settings row 7 through Task 1 inspection and Task 2 transaction; inspection/updates happen in bounded background work after the shell exists, not setup-time resolver failure or blocking awaits inside the event loop. Record the intended operation (start/restart/switch and target kind) in the pending request so confirmation resumes exactly it. Show current observed version/source, pinned target, check/download/verify/ready phases, verified version/source/path, final Running or stopped Ready status and actionable diagnostics; network availability is learned through timed release/download requests rather than an unrelated connectivity probe. On success, update the settings core row, verified version and status immediately. GUI/foreign ownership rejection and subscription conversion failure must open a dismissible explanatory overlay so refresh cannot hide the result. Reuse existing selected-subscription conversion during preparation; supported input requires no hand-written sing-box JSON and unsupported critical fields remain explicit visible failures. Cancellation stops only this operation, preserves old core during prepare, and cannot apply a stale ready event or imply target readiness from its requested version alone. Preserve traffic/log channels and core-generation snapshot filtering; refresh caches/streams only after committed switch. Use separate update/switch failure handling rather than lifecycle::note_error clearing a still-running old core. Add inline guided_core tests for reducer/input, TestBackend rendering, mocked update/transaction integration and zero production hooks; amend old settings tests that expected delayed next-start behavior. Audit test paths before execution, then run the prescribed isolated suite/build/fmt/diff/OpenSpec checks. Keep prior 6aff4da7 evidence explicitly historical, append new exact commands/test counts and mock coverage; mark only newly implemented follow-up tasks done. Report real startup/network/TUN as unverified; the user reported no visible effect when switching, not a specific sing-box error.</action>
  <verify><automated>Run the temporary-XDG full validation command block below after auditing tests; test suite, locked build, fmt, diff and OpenSpec strict must pass. No app smoke launch is permitted.</automated></verify>
  <done>Users can confirm or cancel an update in TUI, observe actual validation/version/status and automatic owned-core switching, failure preserves accurate old-core status, and evidence documents only isolated test/build coverage.</done>
</task>

</tasks>

<threat_model>
## Trust boundaries and mitigations

| Threat | Boundary/component | Disposition | Concrete mitigation |
|---|---|---|---|
| T-CU-T | Release/download -> executable | mitigate | Exact official repo/tag/asset, trusted SHA-256, bounded requests, staged ELF/version validation, installation lock, atomic publication; no unchecked fallback. |
| T-CU-S | User confirmation -> lifecycle | mitigate | Explicit operation/target identity, reject duplicated/stale results, ownership/generation recheck before stop; no GUI/foreign adoption or signals. |
| T-CU-D | Background download -> TUI | mitigate | Async bounded delivery, operation cancellation, temporary cleanup, no long network awaits in input loop. |
| T-CU-I | Error/evidence output | mitigate | Existing secret URL redaction, no secret/controller auth values in diagnostics or evidence. |
| T-CU-T2 | Selection/config -> process/API | mitigate | Shared authoritative kind, staged runtime config, readiness then persistence, rollback snapshots retaining unknown fields and old executable. |
| T-CU-SC | Package installation | accept | This plan adds no dependency and uses locked existing Cargo dependencies; no package-manager install task. |
</threat_model>

<verification>
Level 0 discovery: reuse existing Tokio/reqwest/tempfile/sha2, binary receipts, injected lifecycle, TUI overlay/actions and generation delivery; no new dependencies or upstream compatibility assumptions. Authoritative network metadata behavior is verified with captured responses/mock servers, not asserted from memory.

Create test-specific roots once; do not change HOME/CARGO_HOME or discover real binary paths. Audit the suite for production candidate/version probes, GUI process probes, capability mutations, config/pidfile/controller access and ignored tests before running. Redirect those tests to injected mocks/temporary roots; never enable ignored tests.

```bash
validation_root=$(mktemp -d /tmp/clash-guided-core-validation.XXXXXX)
mkdir -p "$validation_root/data" "$validation_root/runtime" "$validation_root/config" "$validation_root/bin"
export XDG_DATA_HOME="$validation_root/data"
export XDG_RUNTIME_DIR="$validation_root/runtime"
export XDG_CONFIG_HOME="$validation_root/config"
export XDG_BIN_HOME="$validation_root/bin"
export CARGO_BUILD_JOBS=2
# Reuse the existing Cargo cache; test runtime paths remain isolated by XDG.
cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller
cargo build --workspace --locked
cargo fmt --all --check
git diff --check
openspec validate align-dual-core-and-client-behavior --strict
```

Capture logs/exit codes under this task's evidence paths and report counts actually observed. Commands may compile real core integrations but MUST NOT execute an installed/downloaded core (including --version/check), app binary, cargo run, real lifecycle/GUI/service commands, privileged installer, real controller or real configuration. No runtime fallback involving real system paths is permitted in tests. Long cargo commands may run asynchronously with progress updates; task-local quick checks can reuse this target directory after compilation.
</verification>

<source_audit>
| Source | ID | Scope | Task | Status |
|---|---|---|---|---|
| GOAL | quick request | 旧核心启动阻断修复、用户确认更新与切换 | 1–3 | COVERED |
| REQ | QUICK-CU-01 | 旧系统候选跳过、固定兼容策略、离线缓存、禁止降级 | 1 | COVERED |
| REQ | QUICK-CU-02 | 确认、可信下载、验证版本、阶段/取消 | 1, 3 | COVERED |
| REQ | QUICK-CU-03 | 真正切换、所有权、持久化与回滚 | 2, 3 | COVERED |
| REQ | QUICK-CU-04 | 隔离回归、OpenSpec follow-up、证据真实性 | 1–3 | COVERED |
| RESEARCH | absent | Quick 无独立 RESEARCH；源码已定位现有 seam，无新包/新架构选择 | — | NOT APPLICABLE |
| CONTEXT | D-01–D-08 | 上述用户约束全部映射到 action | 1–3 | COVERED |
| GOAL/REQ | ROADMAP | Quick 独立于 integration phase，无新增 roadmap requirement，禁止改 ROADMAP | — | EXCLUDED |
</source_audit>

<success_criteria>
- 3 个串行任务按 inspection/acquisition -> manager transaction -> TUI consumer 顺序完成；不另建 updater subsystem 或变更 roadmap。
- 固定 pin 不变、可信 hash 不变、未知高版本不自动降级；用户先授权才能 TUI 下载。
- 同一 TUI 会话中的真实 manager/API kind 随已提交选型改变；准备/失败不失去原服务，停止态不意外启动。
- 当前 GUI/真实配置/系统 binary 不受开发验证影响；isolated suite/build 和文档检查通过并记录实际证据。
- 返回代码差异、测试/build结果和真实网络/核心启动仍未验证的边界；planner 本次只写本文件，不执行这些任务。
</success_criteria>

<output>
Executor creates `.planning/quick/261009-eth-tui-test-build-gui/261009-eth-SUMMARY.md` and returns verification evidence to the parent. Do not update ROADMAP. Branch/commit policy is controlled by the parent; this planning handoff does not authorize a commit.
</output>
