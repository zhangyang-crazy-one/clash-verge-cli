# Project State

## Current work

- 本次任务：创建双核心与客户端行为对齐的 OpenSpec 文档。
- 文档已完成并通过校验；Rust 对齐实现未开始，22 项实施任务保持未勾选。
- 现有 ROADMAP 和 add-singbox-dual-core 的待验收项未改动。
- 用户的 clash-tui 正在运行；开发验证禁止操作该实例或其真实配置/控制端点。

Last activity: 2026-10-07 - Completed quick task 261007-red: dual-core/client alignment documentation and isolated baseline test/build

### Blockers/Concerns

- 新核心版本的真实联网、协议互通、TUN 和跨核心切换仍需另行授权的隔离验证；500 项基线测试通过不代表新功能已实现。

### Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 261007-red | 双核心及客户端对齐 OpenSpec；保留运行实例；文档校验和隔离 test/build | 2026-10-07 | 9482663e | [261007-red-create-openspec-for-dual-core-and-client](./quick/261007-red-create-openspec-for-dual-core-and-client/) |
