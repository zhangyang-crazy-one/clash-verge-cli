# Project State

## Current work

- 本次任务：debug `stale-controller-socket` 完成，两核心不再把遗留 Unix socket 当作外部活跃端点；源码提交 `da535daa`，新增 OpenSpec 7.1–7.3 完成，累计 30/30。
- 最终隔离 suite：614 CLI +4 integration +13 core =631 passed，0 failed，1 real-core E2E filtered；locked debug/release build 通过，未执行产物。
- 只读 runtime 证据：CLI 私有 Unix socket 文件存在，无 `mihomo.pid`、无 `/proc/net/unix` 精确路径或同 dev/inode 的别名绑定；实际遗留文件及用户实例保持不变。
- 前轮：quick 261009-eth 引导核心更新与可回滚实际选型切换。
- 3/3 quick tasks 与新增 OpenSpec 6.1–6.5 完成；39 项 guided 回归，最终 622 项隔离测试通过、1 项真实核心 E2E 显式 filtered；locked build、fmt/diff/OpenSpec strict 通过。代码提交 `7b9a5136`、`bdf06fe6`、`dcf03232`、`71ba7ccb`。
- 历史对齐实施 `6aff4da7` 的 22/22 tasks 与 588 项测试仅为历史基线；本轮不以旧日志证明 editor/core-check 分支从未执行。
- 现有 ROADMAP 和 add-singbox-dual-core 的待验收项未改动。
- GUI 运行时允许候选检查/确认下载，实际应用仍检查所有权并可见拒绝；用户本轮从 `src-tui` 运行 `cargo run --release`。开发验证未操作运行实例、真实配置/核心/控制端点，未安装到 `/usr/local/bin`；debug/release 产物已构建，加载新代码须用户自行重启。

Last activity: 2026-10-09 - Resolved stale controller socket regression; isolated suite and locked dev/release builds

### Blockers/Concerns

- 真实核心启动/切换、联网、TLS/协议互通、TUN/权限与性能指标仍未验证；离线测试不等同于实际核心兼容认证。主规格同步和归档未执行。

### Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 261007-red | 双核心及客户端对齐 OpenSpec；保留运行实例；文档校验和隔离 test/build | 2026-10-07 | 9482663e | [261007-red-create-openspec-for-dual-core-and-client](./quick/261007-red-create-openspec-for-dual-core-and-client/) |
| 261007-ry8 | 双核心及客户端对齐实施；22/22 tasks；588 项隔离测试与 locked build | 2026-10-07 | 6aff4da7 | [261007-ry8-apply-dual-core-and-client-alignment-ope](./quick/261007-ry8-apply-dual-core-and-client-alignment-ope/) |
| 261009-eth | 引导可信更新/实际核心切换与回滚；3/3 tasks；622 项隔离测试与 locked build | 2026-10-09 | 71ba7ccb | [261009-eth-tui-test-build-gui](./quick/261009-eth-tui-test-build-gui/) |
