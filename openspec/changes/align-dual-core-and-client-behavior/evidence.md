# 上游依据与本地对齐范围

审查日期：2026-10-07。本文件记录提案依据；代码尚未按本变更实现。

| 对齐项 | 上游依据 | 本地定位 | 结论与证据类型 |
|---|---|---|---|
| mihomo 配置能力 | [1.19.31](https://github.com/MetaCubeX/mihomo/releases/tag/v1.19.31)、[1.19.32](https://github.com/MetaCubeX/mihomo/releases/tag/v1.19.32) | `mihomo_manager/binary.rs`、`crates/clash-verge-core/src/config/clash.rs` | 官方版本及 mips 能力已核对；新目标真实兼容待验证，保留现有 gvisor 配置 |
| sing-box 版本输出 | [1.14.2 cmd_version.go](https://github.com/SagerNet/sing-box/blob/v1.14.2/cmd/sing-box/cmd_version.go) | `mihomo_manager/singbox_binary.rs:199` | 解析器只认 Version:；前轮本机输出 sing-box version 1.13.12，缓存判断不匹配 |
| 规则集层级 | [1.14.2 route.go](https://github.com/SagerNet/sing-box/blob/v1.14.2/option/route.go) | `singbox/config_gen.rs:157` | 静态代码错误；前轮合成配置校验显示顶层 rule_set 被 1.13.12 拒绝，嵌套版本通过 |
| DNS 迁移 | [1.14.2 dns.go](https://github.com/SagerNet/sing-box/blob/v1.14.2/option/dns.go)、[Migration](https://sing-box.sagernet.org/migration/) | `singbox/dns.rs`、`singbox/config_gen.rs` | 自建 typed DNS 已采用；原生订阅 legacy server/fakeip 需版本校验，不能一概判为不兼容 |
| 按订阅 DNS 确认 | [CVR 2.5.5](https://github.com/clash-verge-rev/clash-verge-rev/releases/tag/v2.5.5)、[2.5.7 修复 commit](https://github.com/clash-verge-rev/clash-verge-rev/commit/0c7e0c853545a7b7cb8664e75f017a68f6d46396) | `crates/clash-verge-core/src/config/verge.rs:103`、`:383` | 缺 profile_dns_settings；整体序列化不保留未知字段，新版配置读写有字段丢失风险 |
| 设置备份 | 项目自身双核心持久化模型 | `services/backup.rs:22`、`singbox/mod.rs:22` | 白名单缺三份持久化 JSON，静态确认；原生 profiles/*.json 已包含，不重复增加功能 |
| 后台更新应用 | [官方客户端通用行为](https://sing-box.sagernet.org/clients/general/) | `commands/daemon.rs:83`、`subscribe/scheduler.rs:385` | daemon/旧 probe 路径仍走 mihomo 入口；CLI 手动 reload 和 TUI 已有核心分流 |
| 慢速集合更新 | [CVR 2.5.6](https://github.com/clash-verge-rev/clash-verge-rev/releases/tag/v2.5.6) | `mihomo_api/client.rs:91`、`:407` | 固定 5 秒短客户端用于长操作，存在同类风险；本轮未实测慢返回 |
| 同名 provider 节点 | [固定 dev commit](https://github.com/clash-verge-rev/clash-verge-rev/commit/a9a3cfa3a7b8ea321e538565e5fc6ab597af480a) | `services/proxy.rs`、`mihomo_api/types.rs` | dev/2.5.8 参考，非稳定发布；本地按名称去重，需要 API 表达能力调查和 fixture 回归 |
| TUI 队列和重绘 | 项目自身 Rust TUI 结构；[CVR dev changelog](https://github.com/clash-verge-rev/clash-verge-rev/blob/dev/Changelog.md) 为性能参考 | `tui/event_loop.rs:21`、`:63` | 无界队列和固定重绘已定位；没有 CPU/内存 benchmark，不声称已发生泄漏 |
| 保留优雅退出 | [CVR 2.5.7](https://github.com/clash-verge-rev/clash-verge-rev/releases/tag/v2.5.7) | `mihomo_manager/signal.rs`、`watcher.rs` | 已有 SIGTERM 等待、fallback、代际及日志，不作为重新移植项目 |

本地路径除显式 `crates/` 前缀外均相对 `src-tui/src/`。行号定位审查时文件，不替代实现前重新读取。

## 规格到任务的映射

| 能力 | 实现任务 | 最小验证 |
|---|---|---|
| dual-core-compatibility-policy | 1.1–1.4 | 版本/缓存 fixture、安装锁和失败清理模拟 |
| dual-core-client-alignment | 2.1–2.5、3.1–3.3 | JSON 结构/引用、原始 YAML 字段保留、临时归档往返、应用失败回滚 |
| tui-event-and-operation-safety | 3.2、3.4、4.1–4.4 | 模拟慢请求、能力拒绝、有界队列过载、取消与控制事件交付 |
| subscription-auto-update | 3.1、3.5 | 双核心调度、停止态不隐式启动、取消/cooldown |
| subscription-probe-recovery | 3.1、3.5 | 三次失败/五分钟去抖、API 错误分类、固定出口回滚 |
| proxy-batch-delay-test | 3.3、3.4、4.3 | 来源 fixture、四并发、歧义目标隔离与进度 |

## 验证真实性

- OpenSpec validate 只证明文档结构可解析。
- 本轮 test/build 是未修改 Rust 实现的基线检查，不证明清单中的问题已经修复。
- 真实联网、TLS/协议互通、TUN 和跨核心切换均未按本变更验证；不得据此发布完整支持声明。
- 用户运行实例受保护；真实核心验证须另行授权且使用隔离资源。
