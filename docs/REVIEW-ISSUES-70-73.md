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

## 2026-10-12 产品复审修复

复审来源：[PR #74 评论](https://github.com/zhangyang-crazy-one/clash-verge-cli/pull/74#issuecomment-6116357639)，核对基线 `a184adf59c1c2fd3a7bc70ae04af260541764a80`。该基线 CI 已通过，但以下四项产品路径仍存在缺陷，本轮分别修复。

| 意见 | 修复契约 | 回归依据 |
|---|---|---|
| P1 overlay 丢失 no-resolve | 编辑器规范化身份与实际输出分离；按规范化字符串和出现次序匹配，保留引用及新增上游规则原文，不从 sing-box 简化模型生成 Mihomo 源规则 | 文件级重放覆盖已有/刷新新增 IPv4、IPv6、RULE-SET 修饰符；重开、重复保存后原文保持，既有规则顺序回归保留 |
| P1 capability 子进程 exe 不可读 | 实际直接 spawn 的 `Child` 用 ancestry、pidfd 和 starttime 建立归属；per-launch 通过内核凭证传递 pidfd，匹配已绑定 supervisor 代次、launch/kind/config、FD 的 PID 和已知启动入口 | uid=1000 的真实 `PR_SET_DUMPABLE=0` 子进程触发 exe 的 EACCES，仍可绑定并仅清理本次 child；同 exe 的其他进程、伪造文件/发送者/代次/FD 均拒绝；回执错误清理/reap、reparent 后清理覆盖 |
| P2 旧备份恢复留下 overlay | 归档内 overlay 的缺席也是恢复状态：v1（含早期 v1）及无 manifest 的 GUI/legacy 备份重置缺席的标准 CLI overlay；旧顶层 sidecar 缺席行为保留 | 同目录“无 overlay 备份→新增 overlay→恢复”后有效规则恢复；有 overlay 的备份恢复；删除与多文件写入中途失败回滚，暂存/新文件清理 |
| P2 DNS fragment 悬空 detour | 已声明且实际可生成的节点/组优先使用 detour；合法接口使用 bind_interface；DIRECT 直接拨号；RULES 等无法表达的策略逐条明确报告，不伪造出站 | eth0、RULES、DIRECT、已定义/未转换代理、转义组名、参数及同名接口/代理；内部 direct/block 标签不当作声明代理；完整配置校验与实际 lo DNS 查询 |

进程的外部/adopted 身份校验不放宽。readiness 对可读 exe/argv 仍要求精确一致，只有本次通过内核凭证认证的 child 才可容忍 exe 的 EACCES，且 live argv 仍须匹配。`WNOWAIT` 在接收回执前保留 supervisor 代次；接收方核对实际 sender starttime，不能只信 JSON。已消费的畸形消息继续有界读取，拒绝消息的 FD 自动关闭。独立只读复核确认这些归属约束与 controller/config 校验保持。

备份仅清除标准 SHA256 文件名、已知结构的普通 overlay；不相关文件保留，未知结构及不安全 symlink 在恢复写入前拒绝。删除与写入在同一恢复事务中暂存原件并回滚。无 manifest 的 GUI/legacy 备份会清除后来产生的 CLI overlay，使规则回到所恢复的源配置；该策略不会把未知文档静默删除。

DNS `#RULES` 当前明确不支持，不能等价于指定代理；对应条目报告并丢弃，全部上游均不支持时明确提示该来源 DNS 未应用、沿用存储/默认解析器。接口名只验证语法，可用性与权限由运行主机上的核心判断。依据：[Mihomo DNS](https://wiki.metacubex.one/config/dns/#dns-指定-代理接口-进行连接)、[Mihomo DNS selector 实现](https://raw.githubusercontent.com/MetaCubeX/mihomo/Meta/tunnel/dns_dialer.go)、[sing-box dial fields](https://sing-box.sagernet.org/configuration/shared/dial/)。

最终默认并行套件 **877 passed，0 failed，6 ignored**（859 CLI +4 integration +14 core）；其中三个 pinned-core 测试另行执行，**3 passed**，剩余三项 ignored 未运行。新版实核夹具包含 sing-box 1.14.2 的 DNS fragment 配置校验及真实 loopback `#lo` 查询，并重新执行已有 34 次 DNS/SRS 冷热缓存和双核 CORS 验证；公网 DoH/eth0 仅 check，没有实际连接。fmt、diff 和 workspace Clippy 通过。

本轮日志位于 `/tmp/clash-pr74-products/logs/`，Rust 来源清单 `/tmp/clash-pr74-products/source.sha256` 覆盖九个修改文件。永久夹具保留在源码中；旧产物和旧日志不作为新版代码的构建证明。

| 证据 | SHA-256 |
|---|---|
| source.sha256 | `49f865774828613e37211564192764721cab5dba10d4bf46f877d3fd69358245` |
| suite-parallel.log | `497f79230da316180966ec86f97eef7b67941a7b970301caa374c9d4ff50be84` |
| pinned-cores.log | `0c881c93b7af48102f8318bd9b459b53e1b1658855e26eccf28b43493d33f26b` |
| clippy.log | `0248de18639da1ce453258ee46167fb134c98b432c3409434c77439273cec2e3` |
| build-release.log | `a1e9332bb4afb584ab9e59eb83e834432f9abc230f82a65a4ec8ab1d1e2b9f12` |

locked release 构建及两份 OpenSpec strict 校验通过。新版 `target/release/clash-verge-cli` 的 SHA-256 为 `2fbd2b10c6f26980f072461d51957eaf56862634d4dcfdbfc6bbf9111b33fb0f`（版本仍 0.2.7）；构建源码是 `a184adf5` 加本轮九个 Rust 文件修改，清单逐项重新核对一致。没有覆盖系统入口。合并前要求新 head 的 hosted CI 通过，以 PR Checks 和对应运行链接为准。

实际 setcap/TUN 核心、systemd unit、完整切核/GUI 与非 loopback 网络验收仍保持未完成，不用不可 dump 夹具替代用户环境的授权验收。

### musl ABI 构建补充

首次产品修复 head `abe7bfdd` 的 [CI38195570594](https://github.com/zhangyang-crazy-one/clash-verge-cli/actions/runs/38195570594) 已通过 hosted 格式/Clippy/测试/release 和依赖审计，但 aarch64-musl 编译失败：`msghdr.msg_controllen` 与 `cmsghdr.cmsg_len` 在 GNU/musl 下的整数类型不同。Unix 凭证接收函数现在对有界缓冲区长度与解析索引做显式 ABI 转换，仅在该函数注明必要的同类型 cast lint 例外。

修正后再次运行完整并行套件 **877 passed/0 failed/6 ignored**、三个 pinned-core 测试 **3 passed**、进程专项 **11 passed**；fmt/diff/clippy 及 locked release 构建通过。最新九文件来源清单为 `/tmp/clash-pr74-products/source-after-musl.sha256`，新产物 `target/release/clash-verge-cli` SHA-256 `73797b0cb3e1393eff35c4b4b7ff4d590f6f7e229bdd7808811f4184c43f0a59`。前表的来源/日志/产物对应首次产品修复快照，不能混用。

| 最新证据（`/tmp/clash-pr74-products/`） | SHA-256 |
|---|---|
| source-after-musl.sha256 | `3d2707683fb13c2719377235ed3ab227742998d25cbe6f750b0ad1dc23e1b417` |
| logs/suite-after-musl.log | `920d71f2efb5890437056d7c438cec5698e5f0e104f9befedce913d80aa2917c` |
| logs/pinned-after-musl.log | `f5e5b12374e702c82d536c764169266c5710fae7d59a62312446d8f108dff70d` |
| logs/clippy-after-musl.log | `a4be99b273fbc0c276b7a7d729f1ef57d169eb6649beeea62512e97a03d9bfd1` |
| logs/build-after-musl.log | `e8066296ec28b26d10f59a097f58d0759fa9254a0a1d533f83887f9dc74c1b6c` |

本地没有安装 aarch64-musl/zig 工具链，完整交叉构建由新 head 的 hosted CI 复验，以最终 Checks 结果为准。
