## Why

CLI/TUI 已接入 mihomo 与 sing-box，但自动选版、配置生成、订阅更新、备份及能力提示尚未形成一致契约。上游客户端近期修复了 DNS 状态持久化、慢速集合更新和同名节点解析；本项目也存在可定位的外围缺口，需要同时对齐核心兼容和客户端行为。

审查日期：2026-10-07。拟验证目标为 [mihomo v1.19.32](https://github.com/MetaCubeX/mihomo/releases/tag/v1.19.32)、[sing-box v1.14.2](https://github.com/SagerNet/sing-box/releases/tag/v1.14.2)；客户端稳定版参考为 [Clash Verge Rev v2.5.7](https://github.com/clash-verge-rev/clash-verge-rev/releases/tag/v2.5.7)。这些是本次实现采用的固定策略目标；离线回归不证明真实联网/TUN 兼容。CVR `2.5.8` 的[开发改动](https://github.com/clash-verge-rev/clash-verge-rev/blob/dev/Changelog.md)仅作参考。具体依据见 [evidence.md](evidence.md)。

## What Changes

- 建立显式、可固定的核心版本策略，修复实际版本输出解析；离线复用已验证缓存，禁止隐式跨版本升级或降级，完善下载校验与原子安装。
- 修复 sing-box `route.rule_set` 层级、转换后的引用及关键字段损失；按核心和版本校验 DNS/TUN 配置。
- 保留新版 GUI 的 `profile_dns_settings` 与未知 YAML 字段，统一 DNS 覆写优先级；把三份 sing-box 持久化设置纳入备份和恢复。
- 统一 CLI、TUI、daemon 的订阅应用与回滚，补齐能力门控、长操作超时、路径编码和 provider 来源解析。
- 对流量/日志进行合并和限流，可靠交付控制事件，按数据变化重绘；保留已有退出、认证、IPv6 与所有权保护。
- 本轮按 apply 工作流实施上述修复；验证仅使用隔离 test/build，禁止操作用户正在运行的实例。

## Capabilities

### New Capabilities

- `dual-core-compatibility-policy`: Version policy, parser, download safety, integrity, executable validation, and no-downgrade behavior for mihomo and sing-box.
- `dual-core-client-alignment`: Core-aware configuration conversion, persistence, backups, refresh, timeout, and provider identity contracts.
- `tui-event-and-operation-safety`: Bounded event flow, lifecycle ordering, capability-gated operations, and preservation constraints.

### Modified Capabilities

- `subscription-auto-update`: Core-aware scheduling, refresh, rollback, timeout, and provider identity behavior.
- `subscription-probe-recovery`: Core-specific probing and recovery with capability-aware deadlines and diagnostics.
- `proxy-batch-delay-test`: Scoped batch identity and core-appropriate delay/test semantics.

## Impact

实现涉及 `mihomo_manager`、`singbox`、共享配置模型、备份、scheduler、CLI API 与 TUI 事件循环。保持现有 YAML 格式，不强制迁移用户文件；已有主规格和待完成的 `add-singbox-dual-core` 变更不在本轮修改范围。

验证采用临时目录、模拟控制器、串行 `cargo test` 和 `cargo build`。不运行应用、真实核心 E2E、服务管理或现有实例的停止/重启操作；单元测试通过不等同于真实核心兼容认证。

## 后续修复范围（2026-10-09）

用户实际运行反馈显示：GUI 风格订阅脚本被 CLI/TUI 直接拒绝，且停止态核心切换错误地依赖配置生成成功。本次补齐受限的 Rust 内嵌同步 `main(config, profileName)` 支持，统一本地/远程订阅增强路径，并将停止态核心选型与启动准备分离；不删除或静默忽略用户脚本。

用户随后明确授权操作 TUI，因为 GUI 正在运行。此前离线验证范围保留为历史记录；新增实际验证使用私有临时配置、控制端点与端口，关闭测试实例的 TUN/系统代理，只操作测试实例，保留 GUI 及用户配置。真实联网协议、TUN 和提权行为须按实际证据分别说明。
