---
status: complete
phase: quick261007-ry8
plan: apply-dual-core-and-client-alignment-ope
subsystem: dual-core-client
tags: [mihomo, sing-box, tui, openspec]
provides: [pinned dual-core policy, lossless config persistence, core-aware refresh, bounded TUI events]
key-files:
  created: [src-tui/src/mihomo_manager/core_policy.rs, src-tui/src/singbox/capabilities.rs, src-tui/src/subscribe/lifecycle.rs, src-tui/src/tui/background.rs]
requirements-completed: [1.1, 1.2, 1.3, 1.4, 2.1, 2.2, 2.3, 2.4, 2.5, 3.1, 3.2, 3.3, 3.4, 3.5, 4.1, 4.2, 4.3, 4.4, 5.1, 5.2, 5.3, 5.4]
implementation-commit: 6aff4da7
completed: 2026-10-07
---

# Quick 261007-ry8: 双核心及客户端行为对齐

OpenSpec `align-dual-core-and-client-behavior` 的 22 项实施任务完成；代码提交 `6aff4da7`。本次仅实施及隔离 test/build，没有操作正在运行的 clash-tui，没有归档或同步主规格。

## 交付

- mihomo 1.19.32 / sing-box 1.14.2 固定策略、真实版本输出解析、安装锁、streamed digest、独立 digest receipt 与原子安装。
- sing-box capability matrix、route.rule_set、递归引用校验、关键 DNS/TLS/transport/routing 字段诊断；native JSON 未知字段保留，私有运行文件。
- UID/source-bound DNS 确认、YAML 未知字段保留、unset/empty 语义；持久化 JSON 验证与原子写入；三份 sing-box sidecar 的版本化备份及失败恢复。
- 双核心 injected lifecycle；停止态不启动、外部 sing-box 所有权拒绝、reload 后 readiness、失败/取消恢复；provider 来源与刷新后 fixed-exit 校验；分离 health/delay/provider deadlines。
- 有界 traffic/log/control delivery、local intent coalescing、代际过滤和退出取消清理、dirty render；provider-qualified batch 最多四并发、completion progress、输入顺序结果、取消 owned requests。

## 验证

- 串行 locked workspace all-target suite：588 passed / 0 failed / 1 filtered；显式 skip `real_sing_box_spawns_and_answers_controller`。
- locked workspace build、fmt check、diff check、OpenSpec strict validation 均通过。
- 临时 XDG roots：`/tmp/clash-alignment-validation.b0a49x`；完整命令和日志 SHA-256 见 [OpenSpec evidence](../../../openspec/changes/align-dual-core-and-client-behavior/evidence.md)。
- fixture/mock 只访问测试自己的临时文件、端点和非核心 child。兼容测试没有执行真实核心；SIGTERM fallback 测试只操作自己的 sleep/sh child。

## 验证边界与维护约定

真实核心启动/切换、联网下载及订阅、TLS 握手/协议互通、TUN/权限与性能指标未验证。新核心版本需更新策略、capability matrix 和 fixture；unsupported critical fields 显式拒绝，不能宣称完整格式无损转换。运行实例及真实配置、控制端点、已安装二进制未改动。现有 ROADMAP、`add-singbox-dual-core`、未跟踪的 `.agents/`、`.omo/` 和 ZIP 均保留。
