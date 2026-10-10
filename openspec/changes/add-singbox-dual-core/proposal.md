## Why

TUI 目前只支持 mihomo 单核心。sing-box 在 TUN 质量（gVisor 栈）、DNS 分流架构、路由规则表达力（逻辑组合规则、二进制 rule-set）、以及 ssh/tailscale/openvpn 等 VPN 型出口上是 mihomo 没有的增量，且其自带 `clash_api` 兼容层，现有 TUI 的 REST API 层大部分可直接复用。同时 TUI 的路由规则目前只能看不能管，借双核改造一并补齐路由管理能力。

## What Changes

**核心集成**
- 新增 sing-box 作为可选代理核心（外部二进制 spawn，与 mihomo 并存，用户可切换）
- `MihomoApi` 传输层抽象：支持 unix socket（mihomo）与 TCP（sing-box `clash_api` 仅支持 TCP 监听）双传输
- 核心生命周期管理扩展：`mihomo_manager` 增加 sing-box 二进制发现、启动、停止、watcher
- **核心切换生命周期协议**：代际计数器消灭旧核心复活竞态、资源释放屏障（端口/socket/TUN）、就绪探测 + 失败回滚
- **配置生效策略按核心区分**：mihomo 保持 `PUT /configs` 热重载；sing-box 的 `PUT /configs` 为空操作，走"写文件 → 重启 → 探测"路径
- Profile 新增 sing-box 原生 JSON 类型；clash YAML 订阅到 sing-box JSON 的转换器（覆盖 sing-box 全部订阅相关出站协议）
- 功能降级：sing-box 后端下 provider 端点为空实现，相关 UI 隐藏或置灰
- **GUI 互斥**：sing-box 模式下禁止 GUI 接管管理（见 Scope Notes）

**路由规则管理（两种核心通用）**
- 统一内部路由规则模型，按核心序列化为 clash YAML rules 或 sing-box route rules
- Rules 视图从只读升级为管理视图：增删改、排序、逻辑 AND/OR 规则、按目标动作分组浏览
- sing-box rule-set 管理（本地 .srs 与远程 rule-set 引用）

**sing-box 高级配置**
- DNS 结构化配置：多 server、按域名/IP 分流的 DNS 规则、fakeip
- TUN 配置项：栈选择（gVisor/system/mixed）、auto-route、strict-route 等
- 入站配置：mixed/tun 监听端口与开关
- 结构化表单 + 原始 JSON 编辑器双模式（高级用户兜底）

## Capabilities

### New Capabilities
- `singbox-dual-core`: 核心选择与切换、双传输 API 客户端、sing-box 进程生命周期、切换安全协议、配置生效策略、配置生成骨架、订阅转换、功能降级、GUI 互斥
- `routing-rule-management`: 统一路由规则模型、双核格式序列化、Rules 视图 CRUD/排序/逻辑规则、rule-set 管理
- `singbox-advanced-config`: DNS/TUN/入站的结构化配置与原始 JSON 编辑双模式

### Modified Capabilities

（无。现有主 specs 的需求行为不变。）

## Scope Notes

- **GUI 所有权（已决策）**：TUI 切换到 sing-box 核心时，**必须禁止 GUI 接管**。TUI 在 sing-box 模式下写入所有权标记文件；切换前检测运行中的 GUI 实例 / GUI 管理的核心进程，检测到即阻止切换并提示先退出 GUI。GUI 侧的标记检查由本仓库后续版本跟进。
- 路由规则管理覆盖 clash YAML 与 sing-box JSON 双格式的读写；GUI 版共享的 profiles.yaml 格式约束不变。
- sing-box 的 tor/cloudflared 出站、openvpn/tailscale/openconnect endpoint 不做结构化表单，仅保证可通过原始 JSON 编辑器配置并透传给核心生成。
