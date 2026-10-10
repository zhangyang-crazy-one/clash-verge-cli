## Context

TUI（`src-tui/`）当前与 mihomo 强耦合于两处：

1. **API 层** `src-tui/src/mihomo_api/client.rs`：`reqwest::Client::unix_socket()` 直连 mihomo external-controller unix socket，secret 走 `Authorization: Bearer`。方法面：version / proxies / select / mode / delay / traffic(WS) / connections / logs(WS) / rules / rule-providers。注意 `MihomoApi` 同时持有 `socket_path: PathBuf` 字段与两个 client（短超时 client + 无超时 stream_client），传输重构需同时覆盖。
2. **进程层** `src-tui/src/mihomo_manager/`（binary.rs / manager.rs / signal.rs / watcher.rs）：sidecar 启动、信号、文件 watch。

路由规则现状：Rules 视图（`src-tui/src/ui/views/rules.rs`）只读展示运行时规则与 rule providers；规则来源是 profile YAML，TUI 内无任何编辑能力。

sing-box（GPL-3.0，外部二进制方式使用无许可问题）自带实验性 `clash_api` 兼容层：`/version`、`/proxies`(GET/PUT)、`/proxies/{name}/delay`、`/configs`(PATCH 仅 mode)、`/traffic`(WS)、`/connections`、`/logs`(WS)、`/rules` 可用；`PUT /configs` 为空操作（返回 204 不做任何事）；`/providers/rules`、`/providers/proxies` 为空实现。**`external_controller` 仅支持 TCP 监听，不支持 unix socket**。

现有生命周期机制（单核时代设计）：`graceful_stop`（SIGTERM→5s→SIGKILL）、`expected_exit` 全局 bool（主动停止时 watcher 跳过自动重启）、D-09 先预检后停止的 restart 序列、3-in-60s 自动重启熔断、spawn 前 TUN capability 预检。**竞态已实锤**：`manager.rs:143` 在 spawn 后 `store(false)` 清标志，而旧核心 watcher 在 `watcher.rs:56` 才 `swap(false)`——切换场景下旧 watcher 可能在新核心 start 清标志之后才处理退出事件，导致旧核心被误判"意外退出"而复活。

GUI 共享：本项目同时包含 GUI（src-tauri）与 TUI，共享 app 数据目录（profiles.yaml、config.yaml）。GUI 只认 mihomo + config.yaml。本仓库 GUI 侧无可复用的单例/实例检测机制，TUI 侧检测需用进程扫描方案。

## Verified Facts（2026-08 调研实证）

以下事实已经源码/官方文档核实，实施时可直接依赖：

| # | 事实 | 影响 |
|---|---|---|
| F1 | sing-box `/version` 返回 `{"version":"sing-box 1.13.x","meta":true}`，版本串以 `sing-box` 开头；mihomo 返回 `v1.19.x` | 就绪探测可用版本串前缀区分核心类型 |
| F2 | `/traffic` WS 帧为 `{"up":n,"down":n}`（字节/秒，~1s 一帧）；`/logs` WS 帧为 `{"type":"<level>","payload":"..."}`——均与 mihomo 同构 | 现有 `types.rs` 的 TrafficData/LogEntry 直接复用 |
| F3 | delay 端点 url 参数必须 https，http 会被静默丢弃并用默认值 | TUI 统一 https 测试 URL |
| F4 | DNS 配置在 1.12 改为新格式（`dns.servers[].type`: udp/tls/https/quic/local/fakeip 等），legacy `address` 语法 1.12 弃用、**1.14 移除** | 任务 8.1 必须按新格式生成；目标版本锚定 stable 1.13.x |
| F5 | 最小客户端配置模式已验证：mixed inbound + selector/urltest 出站（urltest 默认 URL `https://www.gstatic.com/generate_204`）+ `experimental.clash_api`（127.0.0.1:9090 + secret） | 任务 2.2 骨架照此生成 |
| F6 | GitHub release 资产命名 `sing-box-<ver>-linux-amd64.tar.gz`（另有 amd64v3 变体）；`sing-box run -c` / `sing-box check -c` / `sing-box version` 子命令齐备 | binary.rs 自动安装模式可整体复用；`check` 子命令支持重启前预校验 |
| F7 | sing-box tun inbound 未指定 `interface_name` 时自动命名 | 生成配置显式写死 interface_name（如 `sb-tun0`），使资源屏障的设备检测确定化 |

## Goals / Non-Goals

**Goals:**
- 用户可在 mihomo 与 sing-box 之间切换核心（全局设置），主干功能全可用
- 核心切换过程无生命周期冲突：旧核心不复活、资源释放后再启动、新核心起不来能回滚
- 路由规则在 TUI 内可增删改排序，双核格式无损往返
- sing-box 的 DNS/TUN/rule-set/入站配置有结构化编辑入口，高级字段可用原始 JSON 编辑
- clash YAML 订阅转换覆盖 sing-box 全部订阅相关出站协议
- 现有 mihomo 路径行为零回归

**Non-Goals:**
- FFI 内嵌 sing-box 库（始终外部二进制 spawn）
- Windows/macOS 支持（v1 保持 Linux only）
- tor/cloudflared 出站与 openvpn/tailscale/openconnect endpoint 的结构化表单（原始 JSON 透传即可）
- 规则集在线市场/订阅源管理 UI（rule-set 引用手工添加）
- GUI 侧适配 sing-box（v1 只做 TUI 侧互斥检测与阻止）

## Decisions

### D1: 双核定位为"可选核心"，API trait 带能力位
新增 `src-tui/src/core_api/` 模块承载传输无关的 API trait（`ProxyCoreApi`），`MihomoApi` 与 `SingboxApi` 实现之。sing-box 缺失的 provider 方法以"不支持"能力位声明，而非空实现伪装成功。
*理由：两端点集差异是真实的产品差异，隐藏它会产生静默失败。*

### D2: 传输层参数化而非复制 client.rs
`client.rs` 构造函数改为接受传输枚举 `Transport::UnixSocket(PathBuf) | Tcp(SocketAddr)`；reqwest 原生支持两种。现有调用方默认 UnixSocket，零改动。`socket_path` 字段同步改为 `Transport` 字段；`client` 与 `stream_client` 两个实例都要参数化。
*理由：监控/控制类端点两端协议一致（F2 已实证），只有传输不同。配置生效类端点不一致（见 D9）。*

### D3: 订阅转换为全协议覆盖 + 字段级降级
转换器覆盖 sing-box 全部订阅相关出站协议（ss/ssr/vmess/vless/trojan/hysteria/hysteria2/tuic/naive/shadowtls/anytls/snell/http/socks/wireguard）。节点级字段映射不到目标格式时丢弃该字段并继续，节点整体不可表达时跳过该节点；所有降级汇总到 UI 报告。
*理由：机场订阅字段长尾很大，字段级降级 + 报告比白名单阻断体验好。*

### D4: 核心选择状态存独立设置键
verge 配置新增 `proxy_core: "mihomo" | "singbox"`（默认 mihomo），模式参照既有 `clash_core` / `VALID_CLASH_CORES` 先例。

### D5: sing-box 二进制发现沿用 sidecar 目录约定 + 复用自动安装模式
`sidecar/verge-sing-box`（PATH 查找 fallback），版本探测用 `sing-box version` 子命令。自动下载复用 binary.rs 现成模式（fetch_latest_release_tag → 临时文件下载 → rename → ensure_executable），资产命名见 F6。目标版本锚定 **stable 1.13.x**（F4）。

### D6: 统一路由规则模型（IRouteRule），双核序列化
定义核心无关的规则中间模型：匹配域（domain/domain_suffix/domain_keyword/ip_cidr/port/network/process/protocol/rule_set/逻辑组合）+ 动作（outbound/direct/block/dns 动作）。提供两个序列化器：clash YAML rules 语法 ↔ IRouteRule，sing-box route rules JSON ↔ IRouteRule。profile 存储仍按各自原生格式落盘，编辑时经 IRouteRule 往返。
*理由：避免在 TUI 里维护两套平行编辑器；往返测试保证无损。无法双向表达的极端字段通过"原始片段透传"保留（见 D7）。已知风险点：clash SUB-RULE 在 sing-box 无直接等价物，归入原始片段透传。*

### D7: 结构化表单 + 原始 JSON 双模式
DNS/TUN/入站/rule-set 提供结构化表单；每个配置域同时保留"原始 JSON"编辑入口（复用现有 `editor.rs`，`EditorTarget` 已预留 `Dns` 变体），表单未覆盖的字段原样保留。IRouteRule 同样支持"原始片段"附加字段承载不可表达项。
*理由：结构化表单永远追不上核心配置面，双模式是唯一不丢功能的方案。*

### D8: DNS 配置范围锁定为客户端常用面（新格式）
结构化表单覆盖：多 DNS server（type: udp/tls/https/quic/local/fakeip，F4 新格式）、server 级 detour/tags、按域名/IP 的 DNS 分流规则、fakeip 开关与网段、远程 server 域名的 domain_resolver 引导配置。legacy `address` 语法一律不生成（1.14 移除）。冷门字段走原始 JSON。

### D9: 配置生效策略按核心区分（ReloadStrategy）
mihomo 保持现有管线：写 config.yaml → `PUT /configs?force=true` → 失败回滚文件再 reload。sing-box 因 `PUT /configs` 为空操作，走新路径：**`sing-box check -c` 预校验**（F6）→ 写 singbox.json → 重启核心 → 就绪探测（API version 应答且版本串前缀匹配预期核心类型，F1）→ 探测失败则回滚配置文件并重启。两条路径在 `runtime_config.rs` 的 `commit_runtime_config` 骨架内分叉，备份/回滚/IO 锁复用。mihomo 侧亦可引入 `-t` 预校验作为增强（可选）。
*理由：预校验把大部分坏配置拦截在重启之前，回滚只剩兜底职责；SIGHUP 是关闭重建且丢运行态状态，语义不清。*

### D10: 核心切换生命周期协议（代际计数器 + 资源屏障 + 就绪回滚）
现有 `expected_exit` 全局 bool 在切换场景有竞态（见 Context，manager.rs:143 vs watcher.rs:56）。修复与扩展：
1. **代际计数器**替代全局 bool——每次 spawn generation+1，watcher 记录自己 spawn 时的代；退出处理时代数不匹配当前值即无视，绝不自动重启。
2. **资源释放屏障**——graceful_stop 返回后、spawn 新核心前轮询等待：controller 端口 connect 被拒、unix socket 文件消失、TUN 设备消失（生成的 sing-box 配置显式固定 `interface_name`，F7，使检测确定化）。
3. **就绪探测 + 回滚**——spawn 后轮询新核心 API 至 version 应答且前缀匹配预期核心类型（F1）；超时或启动即退出（watcher 已监测）→ 回滚配置文件 → 重启旧核心兜底 → UI 报告失败原因。
*理由：TUN 双核心抢默认路由最坏可致断网，切换必须原子且有兜底。*

### D11: GUI 互斥——sing-box 模式下禁止 GUI 接管（已决策）
TUI 切换到 sing-box 前检测运行中的 GUI 实例及其管理的核心进程（本仓库 GUI 无单例端口可查，采用进程扫描：clash-verge GUI 进程名 + 非 TUI 子进程的 verge-mihomo），检测到即阻止切换并提示先退出 GUI。sing-box 模式运行期间在数据目录写入所有权标记文件（记录 owner=TUI/singbox/pid）。GUI 侧的标记检查由本仓库后续版本跟进；v1 若 GUI 无视标记强行启动 mihomo，TUI watcher 会观察到自身核心异常，UI 提示存在外部接管冲突。
*理由：GUI 只认 mihomo + config.yaml，让它管理 sing-box 状态必然打架；互斥是唯一安全策略。*

## Risks / Trade-offs

- **clash_api 是实验性接口**：sing-box 升级可能破坏兼容。缓解：兼容性断言集中在 `core_api/` 一处；版本锚定 1.13.x 并在升级时跑兼容性测试集。
- **provider 功能降级**：sing-box 下 Rules providers 面板隐藏，面板内给明确提示文案。
- **双二进制体积**：同时分发两个 Go 核心 ~+25MB。缓解：sing-box 二进制可选下载，未安装时选项置灰。
- **IRouteRule 往返损耗**：SUB-RULE 等无等价物的结构走原始片段透传。缓解：往返 property 测试 + 不一致时禁止保存并提示。
- **sing-box 下每次配置变更需重启核心**：秒级中断，体验劣于 mihomo 热重载。缓解：UI 明示差异；批量保存减少重启次数；`check` 预校验避免无效重启。
- **GUI 强行接管的窗口期**：v1 GUI 不认识所有权标记，用户强行开 GUI 仍会拉起 mihomo。缓解：TUI 检测到异常即提示；GUI 侧检查在后续版本补齐。
- **范围大**：三个 capability 一个 change，任务分组严格按依赖排序，组间可独立交付验证。
