---
status: complete
phase: quick-261009-eth
plan: "01"
subsystem: guided-dual-core-lifecycle
tags: [rust, mihomo, sing-box, tui, integrity, rollback]
dependency-graph:
  requires: ["6aff4da7 dual-core/client baseline"]
  provides: [offline candidate inspection, confirmed trusted acquisition, shared committed core selection, owned switch rollback, responsive guided TUI]
  affects: [core lifecycle, settings core selection, subscription conversion, TUI background generations]
tech-stack:
  added: []
  patterns: [typed inspection, operation-bound consent, immutable managed candidates, cooperative cancellation, injected transaction hooks]
key-files:
  created: [src-tui/src/tui/handlers/core_update.rs]
  modified: [src-tui/src/mihomo_manager/binary.rs, src-tui/src/mihomo_manager/singbox_binary.rs, src-tui/src/subscribe/client_meta.rs, src-tui/src/mihomo_manager/manager.rs, src-tui/src/mihomo_manager/watcher.rs, src-tui/src/singbox/convert.rs, src-tui/src/app/mod.rs, src-tui/src/app/action.rs, src-tui/src/tui/background.rs, src-tui/src/tui/handlers/settings.rs, src-tui/src/tui/handlers/lifecycle.rs, src-tui/src/ui/mod.rs]
key-decisions:
  - "Keep exact mihomo 1.19.32 and sing-box 1.14.2 reviewed targets; newer unknown versions require review."
  - "Verified Ready requires explicit apply confirmation after download consent; retain old service through preparation."
  - "Reuse existing Cargo cache with jobs=2 and four temporary XDG roots; never install or execute the built app/core."
requirements-completed: [QUICK-CU-01, QUICK-CU-02, QUICK-CU-03, QUICK-CU-04]
metrics:
  duration: "approximately 28 minutes from first recorded RED commit"
  completed: 2026-10-09
  tasks: 3
  code-files: 20
implementation-commits: [7b9a5136, bdf06fe6, dcf03232, 71ba7ccb]
---

# Phase quick-261009-eth Plan 01: 引导可信核心更新与可回滚切换 Summary

旧系统候选不再挡住离线兼容缓存；TUI 经下载授权、实际版本校验及 Ready 确认，准备当前订阅后切换自有核心，成功提交共享 kind/API 和 YAML 选型，失败恢复旧状态。

## 完成结果

- Quick 3/3 tasks 完成；原 OpenSpec 22 项完成记录保留，新增 6.1–6.5 五项 follow-up 完成，总计 27/27。没有同步/归档主规格，没有修改 ROADMAP。
- mihomo `1.19.32` / sing-box `1.14.2` pin 不变。所有系统候选只读，旧版本跳过；可信 managed cache 在联网之前检查，未知高版本要求审查。新下载使用独立候选路径，保留旧可执行文件；SHA-256、官方 repo/tag/asset 绑定、manifest 精确文件名、锁、ELF/真实版本校验和暂存清理均保留。
- 共享 manager kind 为唯一已提交选型。显式目标 kind 驱动 spawn 参数、pid record、readiness transport/secret，目标 API 恢复选择器；共享 kind 在配置/marker 成功后才发布。停止态变更不自动启动。GUI、foreign supervisor、变化的 live record 或无所有权的外部 socket 拒绝操作并显示 overlay。
- 当前支持的 Clash 订阅经既有 nodes/groups/MATCH/DNS 转换准备，不要求手写 JSON。关键不支持字段、skipped/degraded 内容产生具体错误；原生 sing-box JSON 不作臆造的逆向 Clash 转换。
- TUI 检查/下载/校验在独立后台 operation 中运行。Ready 显示实际 version/source/path 后再次确认；准备阶段旧核心继续运行。取消、退出和停止等待 owned rollback；陈旧操作结果不能应用。准备失败保留旧 pid/version/data；回滚重启后重建代际相关 streams。

## 用户操作与交付边界

Home `s` 启动、`r` 重启，或 Settings 的 Proxy Core 行触发检查。`y`/Enter 授权下载；验证后再次 `y`/Enter 准备配置并应用。`n`/Esc 取消，结果用 Enter/Esc 关闭。GUI 运行时允许检查和经授权下载、验证 CLI 自有候选；实际启动/切换时显示持久可关闭的所有权说明并拒绝应用，不通过停止 GUI 实现切换。

仅构建 [target/debug/clash-verge-cli](../../../target/debug/clash-verge-cli)；没有安装到 `/usr/local/bin`、覆盖已安装 CLI 或 GUI 核心，没有运行应用。用户实际运行入口尚未确认。真实网络下载、核心启动/切换、TLS/协议互通、TUN/权限和性能未验证；本轮元数据、版本和 readiness 证据来自 fixture/mock。

## 提交

| Task | 内容 | Commit |
|---|---|---|
| 1 RED | typed inspection/授权 fixture 契约 | `06fb017b` |
| 1 GREEN | 离线检查与可信候选准备 | `7b9a5136` |
| 2 RED | clone/API 仍读取旧 copied kind 的失败回归 | `e8f21966` |
| 2 GREEN | 共享选型、配置准备与 owned commit/rollback | `bdf06fe6` |
| 3 | 响应式 TUI 确认、可见阶段/结果与取消 | `dcf03232` |
| 最终修正 | GUI 提供网络时仍允许候选准备，应用时检查所有权 | `71ba7ccb` |

Parent 已对最终修正独立验收；文档提交单独记录规划和验证证据。

## 验证

最终源码内容对应 `71ba7ccb`，safe suite 为 **622 passed / 0 failed / 1 filtered**：CLI 605、integration 4、core 13；其中新增 `guided_core` 39 项。显式 skip `real_sing_box_spawns_and_answers_controller`；无 warnings。

| 检查 | 结果 | 日志 |
|---|---|---|
| `cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller` | exit 0；622 passed | `/tmp/clash-guided-core-validation.ETH/suite-parent-final.log` |
| `cargo build --workspace --locked` | exit 0 | `/tmp/clash-guided-core-validation.ETH/build-parent-final.log` |
| `cargo fmt --all --check` | exit 0 | `/tmp/clash-guided-core-validation.ETH/fmt-parent-final.log` |
| `git diff --check` | exit 0 | `/tmp/clash-guided-core-validation.ETH/diff-parent-final.log` |
| `openspec validate align-dual-core-and-client-behavior --strict` | exit 0；valid | `/tmp/clash-guided-core-validation.ETH/openspec-parent-final.log` |

四个 XDG roots 位于 `/tmp/clash-guided-core-validation.ETH/{data,runtime,config,bin}`，`CARGO_BUILD_JOBS=2`，复用现有 Cargo 缓存。日志 SHA-256、完整环境/命令、覆盖和真实运行边界见 [evidence.md](../../../openspec/changes/align-dual-core-and-client-behavior/evidence.md)。构建产物 SHA-256：`1f39a9db1e612fd92bdebbc10b4e3e98ee3299408019de938905d5c0da61fb62`。

## Deviations from Plan

1. **[Rule 1 - Bug] 已映射协议字段被错误报告为 degraded。** 新 guided lossless gate 暴露 `cipher/password/uuid` 等已映射字段也进入 degraded 列表，会拒绝常见订阅；按当前 protocol field map 排除这些字段。支持的 ss 节点、selector、MATCH、DNS fixture 经过实际 guided conversion gate。文件：`src-tui/src/singbox/convert.rs`；提交 `bdf06fe6`。
2. **[Rule 3 - Blocking] 旧测试可能执行真实候选/check。** editor 测试改为静态 JSON；service 候选测试注入 None；旧 settings 环境依赖改为异步意图/纯 injected 回归，binary/UA/extraction 测试也改为 fixture。无法从旧 588 项日志核实当时是否进入真实 check 分支，旧证据仅标记为历史。提交 `7b9a5136`、`dcf03232`。
3. **验证缓存与元数据约定。** 按 parent 指令不创建新的 CARGO_TARGET_DIR，计划验证段改为 jobs=2；专用临时 XDG 目录共享给该 quick 的 worker。STATE 使用项目自定义 quick 表格，SDK `state.record-session` 返回 `No session fields found`，因此保留原结构并直接更新本次状态；没有推进常规 phase/ROADMAP。

## TDD Gate Compliance

Task 1 有 RED 契约编译失败与 GREEN 提交；Task 2 有实测 copied-kind 行为失败与 GREEN 提交。Task 3 在 typed 接口整合后补充 reducer/input/render 回归并修正 fixture，未单独产生先于实现的 Task 3 RED commit；这是本轮 TDD 提交顺序偏差，不能宣称所有三个任务严格执行了独立 RED/GREEN。最终所有要求的行为测试及工程检查已通过。

## Deferred Issues

真实 GUI/TUI 入口、下载、核心/控制端与 TUN 验收未执行；用户运行实例、配置和系统 binary 均未修改。`.agents/`、`.omo/` 与原 ZIP 保留。没有依赖安装、authentication gate、新未接线 stub 或意外 tracked 文件删除。

## Self-Check: PASSED

已核实新增 `core_update.rs`、本 SUMMARY 和构建产物存在；五个任务提交和最终修正提交均存在，未删除 tracked 文件。最终测试、build、fmt、diff 和 strict OpenSpec 均返回 0。STATE 和 follow-up 文档已更新；最终修正经 parent 隔离 suite/build 验收。
