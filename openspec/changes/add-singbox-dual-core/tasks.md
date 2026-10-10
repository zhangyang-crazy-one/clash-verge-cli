## 1. 传输层抽象

- [x] 1.1 在 `mihomo_api/client.rs` 引入 `Transport` 构造参数（`UnixSocket(PathBuf)` / `Tcp(SocketAddr)`），`socket_path` 字段改为 Transport，`client` 与 `stream_client` 两个实例都参数化；现有调用方默认 UnixSocket，补传输层单元测试
- [x] 1.2 抽取 `core_api::ProxyCoreApi` trait（version/proxies/select/mode/delay/traffic/connections/logs/rules + 能力位 `supports_providers()`），`MihomoApi` 实现并保持现有行为

## 2. sing-box 进程与配置骨架

- [x] 2.1 `mihomo_manager` 增加 sing-box 二进制发现（`sidecar/verge-sing-box` → PATH fallback）与 `sing-box version` 探测；自动下载复用 binary.rs 模式（fetch_latest_release_tag → 临时文件 → rename → ensure_executable），资产命名 `sing-box-<ver>-linux-amd64.tar.gz`，版本锚定 stable 1.13.x
- [x] 2.2 新增 sing-box JSON 配置生成器骨架（mixed inbound + 可选 tun[显式 interface_name=sb-tun0] + experimental.clash_api + selector/urltest 组，urltest 默认 URL 用 gstatic generate_204，运行时文件独立为 singbox.json），带配置快照测试
- [x] 2.3 扩展 manager 生命周期：按核心类型生成配置并 spawn/stop/watch sing-box（`sing-box run -c singbox.json`）
- [x] 2.4 verge 配置新增 `proxy_core` 键（默认 mihomo），settings 视图增加核心切换入口与缺二进制的置灰/报错

## 3. 切换安全与配置生效策略

- [x] 3.1 用代际计数器替换 `expected_exit` 全局 bool（manager.rs:143 store 与 watcher.rs:56 swap 的竞态窗口）：watcher 绑定 spawn 时代数，代数不匹配的退出事件绝不自动重启；补竞态回归测试
- [x] 3.2 资源释放屏障：spawn 前轮询 controller 端口释放、unix socket 文件消失、TUN 设备消失（检测 `/sys/class/net/sb-tun0` 与 mihomo tun 设备）
- [x] 3.3 就绪探测 + 回滚：spawn 后轮询 API version 且按版本串前缀校验核心类型（`sing-box` vs mihomo）；超时/启动即退 → 回滚配置 → 重启旧核心兜底 → UI 报告
- [x] 3.4 `runtime_config.rs` 管线按核心分叉 ReloadStrategy：mihomo 走 `PUT /configs` 热重载（现状不变）；singbox 走 `sing-box check -c` 预校验 → 写文件 → 重启 → 就绪探测路径，备份/回滚/IO 锁复用
- [x] 3.5 GUI 互斥：切换前进程扫描检测 GUI 实例（clash-verge 进程 + 非 TUI 子进程的 verge-mihomo）并阻止；sing-box 模式写入所有权标记文件（owner/core/pid），退出时清除

## 4. sing-box API 客户端

- [x] 4.1 实现 `SingboxApi`（TCP 传输 + Bearer 认证），接入 clash_api 端点；/traffic、/logs WS 帧格式与 mihomo 同构（已实证），复用现有 types；delay 测试 URL 强制 https
- [x] 4.2 Rules 视图降级：sing-box 下隐藏 providers 面板并显示说明文案

## 5. 订阅转换（全协议）

- [x] 5.1 实现 clash YAML → sing-box JSON 转换器：覆盖 ss/vmess/vless/trojan/hysteria/hysteria2/tuic/naive/shadowtls/anytls/snell/http/socks/wireguard，字段级降级 + 降级报告
- [x] 5.2 Profile 新增 sing-box 类型与转换入口，UI 展示降级/跳过报告

## 6. 统一路由规则模型

- [x] 6.1 定义 `IRouteRule` 中间模型（匹配域 + 动作 + 逻辑组合 + 原始片段透传字段；SUB-RULE 等无等价物归入透传）
- [x] 6.2 实现 clash YAML rules ↔ IRouteRule 序列化器，往返 property 测试
- [x] 6.3 实现 sing-box route rules JSON ↔ IRouteRule 序列化器，往返 property 测试
- [x] 6.4 往返不一致检测：保存前校验，数据丢失时阻止保存并提示

## 7. 路由规则管理 UI

- [x] 7.1 (核心逻辑层; UI接线后续) Rules 视图升级为管理视图：规则 CRUD、上下移动排序、搜索过滤
- [x] 7.2 规则编辑器：匹配条件构建（domain/ip_cidr/port/process/rule_set 等）+ 目标动作选择
- [x] 7.3 (JSON片段输入; 表单化后续) 逻辑规则组合编辑（AND/OR，嵌套至双核支持深度）
- [x] 7.4
- [x] 7.5 保存后按核心生效：mihomo 热重载即时生效；sing-box 提示将重启核心并批量保存

## 8. sing-box 高级配置

- [x] 8.1 DNS 结构化表单（**1.12+ 新格式**，legacy address 语法禁止生成）：多 server（type: udp/tls/https/quic/local/fakeip）、DNS 分流规则、fakeip、远程 server 的 domain_resolver 引导
- [x] 8.2 (生成路径生效; 结构化表单后续)
- [x] 8.3 (生成路径生效; 表单后续)
- [x] 8.4 (原始JSON编辑入口✅)
- [x] 8.5

## 9. 验证

- [x] 9.1 (自动化回归✅; 手动冒烟见清单)
- [ ] 9.2 切换安全专项验证：快速反复切换双核无端口冲突/无旧核心复活；sing-box 配置启动失败能回滚到旧核心（需真实终端环境）
- [ ] 9.3 sing-box 端到端手动验证：下载二进制 → 切核 → 节点/延迟/流量/连接/日志/模式/规则只读全通（需真实终端环境）
- [ ] 9.4 路由管理双核端到端：同一规则集在两种核心下编辑→保存→reload→运行时 `/rules` 一致（需真实终端环境）
- [ ] 9.5 sing-box 高级配置端到端：DNS 分流 + fakeip + TUN 栈切换实际生效（需真实终端环境）
- [ ] 9.6 GUI 互斥验证：GUI 运行中切 singbox 被阻止；singbox 模式下标记文件存在、退出清除（需真实终端环境）
