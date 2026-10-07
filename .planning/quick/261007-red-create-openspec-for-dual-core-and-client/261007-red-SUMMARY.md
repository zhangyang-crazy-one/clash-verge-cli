---
quick: true
status: complete
subsystem: planning
completed: 2026-10-07
requirements-completed: []
---

# Summary: 双核心与客户端对齐 OpenSpec

本次完成文档规划与现有代码基线检查，未实现对齐清单中的 Rust 功能。

## 交付

- 变更：`openspec/changes/align-dual-core-and-client-behavior/`。
- proposal、design、evidence、tasks、六份 delta specs 和 `.openspec.yaml`。
- 三项新增能力及三项主规格修改；22 项后续任务均为 `[ ]`。
- 目标为 mihomo 1.19.32 / sing-box 1.14.2 候选兼容及 CVR 2.5.7 稳定客户端行为；dev/2.5.8 仅参考。
- 主规格、已有待执行变更、Rust 源码及用户未提交文件保持原状。

## 验证

- `openspec validate align-dual-core-and-client-behavior --strict --no-interactive`：通过。
- `openspec status --change align-dual-core-and-client-behavior`：4/4 artifacts complete，文档可进入 apply。
- 相对链接、六份规格及 22 个未勾选任务检查通过。
- `cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller`：通过；CLI 单元 490、配置往返 3、core 单元 7，共 500 项，真实核心 E2E 1 项明确排除。
- 测试的 XDG_DATA_HOME / XDG_RUNTIME_DIR / XDG_CONFIG_HOME / XDG_BIN_HOME 均为 `/tmp/clash-openspec-validation.vKrXkM/` 下的临时目录；串行执行，CARGO_BUILD_JOBS=2。
- `cargo build --workspace --locked`：通过，dev profile。
- 日志：`/tmp/clash-openspec-validation.vKrXkM/test.log`、`build.log`。

## 运行保护与实际未完成项

未运行应用、服务管理、授权安装、真实核心 E2E，也未关闭、重启或切换用户正在运行的 clash-tui。测试只使用临时目录、模拟控制器及测试自身非核心子进程。

这些结果是未修改 Rust 实现的基线检查，不证明清单中的缺口已经修复。真实联网、协议互通、TUN 与跨核心切换仍未验证；实现任务尚未开始，已有 add-singbox-dual-core 的手动验收未改为完成。

## 本地提交

- OpenSpec 文档提交：`9482663e`。未推送远端。
- GSD 计划/摘要/STATE 另作本地记录提交；不包含源码或用户未提交文件。
