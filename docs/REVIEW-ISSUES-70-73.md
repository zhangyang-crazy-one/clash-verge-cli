# Issues #70–#73 本地接续修复核查

核查日期：2026-10-11。仓库：`zhangyang-crazy-one/clash-verge-cli`；本地分支：`fix/review-round5`；接续基线：`df0b7a73`。

## 接续前的实际状态

本地已有 `7a57bf6d`（C7：按实际合成顺序计算规则指纹）和 `df0b7a73`（A5：启动已验证候选、显式启动模式及恢复），领先 `main` 的 `f0ecde3c`。远端 issue 仍标记“修复中”，而 `.planning/STATE.md` 还停在更早的分支记录。此次以实际代码和重新执行的检查确认状态。

系统入口为 `/usr/local/bin/clash-verge-cli`，版本显示 `0.2.7`，安装文件时间为 2026-10-10 13:06:32 +0800。本轮不会通过该文件的版本号推断其包含新修复；新产物独立构建于仓库 `target/`。

## 逐项结果

| 意见 | 本地处理 | 验证范围与保留边界 |
|---|---|---|
| #70 A1 | 校验 native `dns.final`、resolver 对象/字符串、嵌套 DNS 规则和 HTTP-client 引用；拒绝非空 `services` | 无效候选在正式配置及生命周期变更前失败；引用/服务夹具覆盖 |
| #70 A2 | 外部替换先检查 system/user systemd cgroup，拒绝游离接管并提示对应命令；owned daemon 内部刷新允许 | 注入 system/user × external/owned 矩阵；真实 unit/MainPID/cgroup 验收保留 |
| #70 A3 | 每次启动独立 UUID 回执，绑定 PID/starttime、kind、exe、cmdline/config；pidfd 身份复核和冻结屏障 | 测试子进程实际 supervisor 退出/reparent、身份篡改、其他实例保护；旧内核不支持 pidfd 时拒绝发送信号，旧内核行为未验收 |
| #70 A4 / #72 C4 | 按 profile 保存规则覆盖，完整 Rules/Merge/Script 合成后重放；支持非连续链贡献 | 刷新、重排/删除/重复规则、重复保存、失败回滚、损坏文件、备份恢复和删除清理覆盖 |
| #70 A5 | 保留已有已验证候选启动、owned/adopted 恢复模式传递和事务授权 | 重新核验候选绑定及恢复回归；全部真实核心恢复/切换矩阵仍属 #73 |
| #71 B1 | 生成显式 `http_client: {engine: go}`，移除 `download_detour` 和隐式默认客户端依赖 | 1.14.2 实际冷缓存 SRS GET、关闭上游后热缓存离线启动；未宣称 1.16 实测 |
| #71 B2 | 保留 mieru 不支持的报告 | 未伪造 sing-box 的协议能力 |
| #71 B3 | 节点域名按 policy 使用真实 resolver；未命中使用业务 resolver；bootstrap 独立；循环依赖拒绝 | 节点实际 payload 与上游计数、策略命中/未命中、IPv4/IPv6 literal；DoH/DoT bootstrap 仅 check，实际加密握手未测 |
| #71 B4 | 实现 blacklist/whitelist/rule fake-IP 决策与优先级，保留 real-IP 查询的 nameserver-policy | 实际 34 次 A/AAAA 验证；不支持 geosite/rule-set 等逐条报告；普通 filter 的 `keyword:` 不擅自解释为子串 |
| #71 B5 | 保留 naive/cronet/UDP 的保守能力边界 | 未把未验证能力改成支持 |
| #72 C1 | 保留现有候选绑定授权链实现 | 真实 TUN 权限、start/restart/switch 完整实机授权链仍待验收 |
| #72 C2 | 删除未实现的 `GITHUB_TOKEN` 建议 | 仍采用原有认证状态/匿名元数据行为，没有新增 token 读取 |
| #72 C3 | 用户选择：可配置外部来源白名单，默认仍仅本地；两核共享设置及配置 get/set 入口 | 原始 YAML 未知字段保留、私有备份、重读有效值；显式额外来源同步允许该白名单的 PNA 预检 |
| #72 C5 | `--start-use-existing-config` 显式传递恢复模式，移除环境变量的隐式控制 | CLI 解析、独立模式与普通缺文件/恢复缺文件回归 |
| #72 C6 | help/start/status/stop 等改为实际核心或中性名称；JSON 增加 core/controller 并保留旧字段 | 两核心停止状态、CLI help 和失败指导的独立进程验证 |
| #72 C7 | 原 append 指纹修复继续保留；remote 编辑现由 profile 覆盖持久化 | save→load→compose→drift、重复保存、prepend/无片段及真实同长度来源变化回归 |
| #73 9.2–9.6 | 隔离回归和实核 DNS/下载/CORS 证据分项保存 | 不等同于完整切核、路由命中、GUI 互斥、真实 TUN/各栈的验收签字；不关闭未验收任务，不归档未完成规格 |

## 规则覆盖契约

`profiles/cli-rule-overlay-<profile-UID-SHA256>.yaml` 保存版本、原始合成规则和逐条来源引用/新增规则。原订阅 YAML 与现有链文件不改写。刷新时保留用户增加的规则、删除与排序；消失的原始规则不复活；新增订阅规则按来源顺序并入、避免落在 catch-all 后。规则身份为规范化字符串及其重复出现次序；来源改写同一条规则时，新的字符串按新来源规则处理，不猜测语义身份。

覆盖文件属于 CLI 后合成规则层；GUI 不自动消费该层。GUI 格式、订阅和原链文件保持兼容。原生 sing-box JSON 不使用 Clash 规则覆盖；已有覆盖遇到原生 JSON 来源时显式拒绝。逻辑规则顺序仍使用 sing-box sidecar，来源真正变化时继续产生 drift 提示。

## CORS 配置

```bash
./target/release/clash-verge-cli config set controller_allowed_origins '["https://dashboard.example:8443"]'
./target/release/clash-verge-cli config get controller_allowed_origins --json
# 恢复默认仅本地来源
./target/release/clash-verge-cli config set controller_allowed_origins '[]'
```

配置保存在 `verge.yaml` 的 `controller_allowed_origins`，现有文件先备份为 `verge.yaml.bak`；设置后重启核心生效。只接受规范化 http(s) origin，拒绝通配、凭据、路径、query/fragment。订阅或脚本内的 CORS 字段不能增加授权来源。控制器仍有 secret 且只监听原本的本地端点。外部来源访问私网控制器需要 PNA；该设置遵循 [sing-box Clash API 文档](https://sing-box.sagernet.org/configuration/experimental/clash-api/) 的要求。

## 验证证据

首次冻结源码（PR 提交 `73170f8d`）的串行套件：840 CLI + 4 integration + 14 core = **858 passed，0 failed，5 ignored**。其中两项 opt-in 实核测试单独执行并通过；剩余三项 ignored 测试未运行。`cargo fmt --all -- --check`、`git diff --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、两份 OpenSpec strict 校验及 locked release 构建通过。本机未安装 `cargo-deny`，其 CI 检查未在本地执行。

新产物为 `target/release/clash-verge-cli`，SHA-256 `82717d6ce46de059b439ce7ce038d5d5a1df82007422d84b7369a6e6882b7657`，版本仍为 `0.2.7`；这是基于 `df0b7a73` 加本轮修复构建的产物（构建时修改尚未提交）。已用该产物在 `/tmp/clash-review-20261011/release-smoke` 实际执行白名单 set/get 及恢复空列表，确认有效来源读回正确。没有安装覆盖系统入口。

冻结源码清单 `/tmp/clash-review-20261011/source.sha256` 覆盖 27 个本轮 Rust/Cargo 文件，全部重新核对一致；清单 SHA-256 `ff483bd3f14ea899a9f684d3d0c7f4225b53caf6753e3ce7531f793d77c377ac`。本地证据如下；`/tmp` 文件可能被系统清理，永久可复现的夹具保存在源码中。

| 证据文件（`/tmp/clash-review-20261011/logs/`） | SHA-256 |
|---|---|
| `suite.log` | `8666cc29e740b4e68d9f1553288a792dcdeef749a27878fddcfdeeaad651d16e` |
| `pinned-cores.log` | `c62127b412defb1abd538849dc486182e56aceab1b7294d88e7170a322e73a7c` |
| `clippy.log` | `ab6db7ddc130c81d633ec98ac5daa81fe326e38fb687ea61d4dbd68839c2b3d3` |
| `build-release.log` | `6946179c9b396355d66039b07bd672654c6b5885520c38abae066ee2a800413f` |

复验时应先准备隔离 XDG 目录；测试绑定 loopback/Unix socket，在当前环境需要网络沙箱审批。实核夹具仅复制并运行测试核心，不启用 TUN 或系统代理。核心路径可以替换为同版本、已校验的本地文件。

```bash
env CARGO_BUILD_JOBS=2 \
  XDG_CONFIG_HOME=/tmp/clash-review-20261011/config \
  XDG_DATA_HOME=/tmp/clash-review-20261011/data \
  XDG_RUNTIME_DIR=/tmp/clash-review-20261011/runtime \
  XDG_BIN_HOME=/tmp/clash-review-20261011/bin \
  cargo test --workspace --all-targets --locked -- --test-threads=1

env CLASH_TEST_SINGBOX_BIN=/tmp/clash-dns-review/sing-box \
  CLASH_TEST_MIHOMO_BIN=/home/zhangyangrui/.local/share/clash-verge-cli/mihomo-v1.19.32 \
  CARGO_BUILD_JOBS=2 \
  XDG_CONFIG_HOME=/tmp/clash-review-20261011/config \
  XDG_DATA_HOME=/tmp/clash-review-20261011/data \
  XDG_RUNTIME_DIR=/tmp/clash-review-20261011/runtime \
  XDG_BIN_HOME=/tmp/clash-review-20261011/bin \
  cargo test -p clash-verge-cli pinned_ -- --ignored --test-threads=1 --nocapture
```

Rust 单元测试及 opt-in 实核夹具随源码保留：

- `src-tui/src/mihomo_manager/process.rs` 与 manager/runtime/start 的生命周期回归。
- `src-tui/src/rule_overlay.rs`、Rules handler 与 backup 的覆盖/恢复回归。
- `src-tui/src/singbox/dns_runtime_tests.rs`：`pinned_core_dns_semantics_loopback`，要求显式提供 `CLASH_TEST_SINGBOX_BIN`，仅使用临时目录和 loopback，无 TUN/系统代理。
- `src-tui/src/commands/config.rs`：`pinned_cores_controller_origins_return_only_authorized_cors_headers`，两核实际验证默认/白名单、GET/OPTIONS/PNA、未授权来源及无 Bearer 的 401。此项是本地 HTTP 预检验证，未替代第三方浏览器端到端验收。
- DNS 实测使用 sing-box 1.14.2，revision `af6e64c3b69e6132ebaee0e1a3d24e93903f6709`，二进制 SHA-256 `fc9c6e6ab345f045b16a0ed10d1ff28d68e8e56e7749fca30738d1406e98d7b8`。源码规则集语义依据：[HTTP client](https://sing-box.sagernet.org/configuration/shared/http-client/)、[rule-set](https://sing-box.sagernet.org/configuration/rule-set/)、[1.14.2 dialer](https://github.com/SagerNet/sing-box/blob/v1.14.2/common/dialer/dialer.go)、[Mihomo DNS](https://wiki.metacubex.one/config/dns/)。

核查与测试阶段未修改用户真实配置、系统授权、已安装程序或远端 issue/PR。核查完成后，用户授权将这些修复提交并创建面向 `main` 的 PR；issue 的未验收门禁保持开放。工作区已有 `.agents/`、`.omo/` 和 ZIP 保留，不纳入 PR。

## PR #74 CI 夹具修复

[首次 Actions 运行](https://github.com/zhangyang-crazy-one/clash-verge-cli/actions/runs/38137067934) 的格式、Clippy、依赖审计和 aarch64 构建通过；workspace 测试为 838 passed、2 failed、5 ignored，因此该 job 的 release 构建被跳过。这两项 adopted-core 测试的 `sleep` 子进程继承 hosted runner 的 `hosted-compute-agent.service` cgroup，触发实际 systemd 接管保护，尚未进入待测启动/回滚路径。

修复仅增加 `#[cfg(test)]` 的每实例 cgroup setter，并在两项夹具中明确提供非服务 `terminal.scope`；默认值仍为 `None`，生产 `/proc` 读取和拒绝逻辑不变。既有 system/user service 拒绝测试复用同一入口，没有跳过测试或禁用保护。

将 runner service cgroup 临时注入两项测试后，均以退出码 101 复现原错误；修复后两项启动/回滚测试、system/user service 拒绝测试及纯策略矩阵单独通过。随后在隔离 XDG 目录运行与 CI 一致的默认并行 `cargo test --workspace --all-targets --locked`：**858 passed、0 failed、5 ignored**；fmt、diff 和 workspace Clippy 通过。日志保存在 `/tmp/clash-pr74-ci-fix/logs/`。这次仅改变测试代码，前述 release 和实核验证属于首次修复产物的历史证据。
