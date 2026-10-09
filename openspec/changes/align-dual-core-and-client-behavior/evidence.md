# 上游依据与本地对齐范围

审查日期：2026-10-07。本文件保留提案时的上游依据，并记录 apply 实现与离线验证。前面的“本地定位”表描述审查时缺口，修复状态见下方实施映射。

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
- 文档阶段的 500 项基线测试不证明修复完成；apply 阶段的最终测试结果记录在下方，二者不得混用。
- 真实联网、TLS/协议互通、TUN 和跨核心切换均未按本变更验证；不得据此发布完整支持声明。
- 用户运行实例受保护；真实核心验证须另行授权且使用隔离资源。

## Apply 实施与回归映射

所有路径均以仓库根为基准。下表测试通过情况以最终隔离 suite 为准；schema fixtures 属于本地结构检查，不是完整 sing-box schema 或协议互通认证。

| 能力 / 任务 | 实现位置 | 主要验证依据 |
|---|---|---|
| compatibility-policy；1.1 | `src-tui/src/mihomo_manager/core_policy.rs`、`binary.rs`、`singbox_binary.rs` | `pinned_policy_accepts_only_explicitly_reviewed_versions`、`semantic_versions_order_prereleases_and_reject_ambiguity`；固定版本不调用 latest 选版 |
| version parsing；1.2 | `singbox_binary.rs` | `parses_official_and_installed_singbox_output_shapes`；official/full/raw/冲突/non-zero 输出 fixture |
| atomic acquisition；1.3 | `binary.rs`、`singbox_binary.rs` | `install_lock_is_exclusive_across_handles`、`verify_sha256_accepts_matching_digest_and_rejects_others`、`failed_or_cancelled_binary_replace_keeps_old_digest_receipt_valid`、归档提取 fixture；没有执行真实核心或下载发布资产 |
| compatibility test safety；1.4 / 5.1 | `mihomo_manager/mod.rs`、隔离路径 fixture、验证命令 | 路径测试使用纯 resolver；缺失控制端点使用测试自己的 tempdir；真实核心 E2E 显式跳过 |
| conversion/matrix；2.1 | `singbox/capabilities.rs`、`convert.rs`、`config_gen.rs`、`mihomo_manager/manager.rs` | client fingerprint/Reality/transport/UDP fixture；`vless_plaintext_is_preserved_and_malformed_tls_identity_is_rejected`；legacy DNS / removed outbound / TUN 栈诊断；未知 native 字段保留 |
| nested route/reference；2.2 | `singbox/config_gen.rs`、`mihomo_manager/manager.rs` | `rule_sets_are_nested_under_route_when_present`、`dangling_group_and_rule_set_references_are_rejected`、`nested_logical_rules_and_selector_defaults_cannot_reference_missing_targets`；不支持的 Clash route syntax 显式拒绝 |
| DNS/unknown YAML persistence；2.3 | `crates/clash-verge-core/src/config/{verge,profiles,prfitem}.rs`、`src-tui/src/{chain,runtime_config}.rs`、`services/profile.rs` | UID/source confirmation、root/nested round-trip、typed precedence、unset/empty、成功后确认持久化；`cancelled_apply_restores_previous_runtime_and_removes_staged_candidate`；`unsupported_dns_endpoint_diagnostics_do_not_expose_credentials_or_tokens` |
| versioned backup；2.4 | `services/backup.rs` | sidecar manifest/schema；`legacy_archives_leave_new_sidecars_untouched`、`failed_mid_commit_rolls_back_every_file_and_keeps_no_staging_files`；私有权限/secret/runtime 排除 fixture |
| atomic durable JSON；2.5 | `singbox/mod.rs`、`dns.rs` | `malformed_storage_is_reported_and_atomic_validation_preserves_previous_file`、`atomic_rename_failure_keeps_previous_json_and_cleans_temporary_file`、`stored_logical_rule_rejects_unmodeled_fields_instead_of_truncating` |
| injected lifecycle；3.1 | `subscribe/lifecycle.rs`、`scheduler.rs`、`runtime_config.rs`、`commands/daemon.rs` | fake 双核心 reload/stopped/external；`reload_selected_runs_readiness_before_selection_and_surfaces_failures`；取消 guard、watcher 代际、失败恢复 fixture |
| provider capability/readiness；3.2 | `mihomo_api/client.rs`、`core_api/proxy_core.rs`、`commands/provider.rs` | `singbox_provider_operations_are_rejected_before_connecting`；编码 scoped healthcheck / provider refresh 请求；`accepted_hot_reload_requires_controller_readiness_before_success` |
| provider provenance；3.3 | `services/proxy.rs`、`subscribe/lifecycle.rs`、`commands/proxy.rs`、`ui/proxy_list.rs` | provider scope / duplicate / include-all fixture；`saved_provider_identity_must_survive_refresh`；`one_shared_name_row_keeps_both_provider_outcomes`；CLI/TUI 歧义选择拒绝 |
| deadlines；3.4 | `mihomo_api/client.rs`、`error.rs` | delay 1..32767ms + 5s margin；health 5s；provider default 30s / 可配置上限 120s；慢响应和 timeout mock；长请求复用连接池 |
| scheduler/probe；3.5 | `subscribe/scheduler.rs`、`lifecycle.rs`、`commands/daemon.rs` | interval/disable/cooldown/external-edit、3 次失败/5 分钟去抖、controller error 分类、fixed-exit/provider identity、stopped unavailable fixture；已移除未使用的 mihomo-only probe/recovery 路径 |
| bounded traffic/log；4.1 | `tui/event_loop.rs`、`handlers/connections.rs` | latest-value watch、bounded log queue、dropped counter、NDJSON buffer cap；确定性 overload fixture |
| lifecycle/render/cancel；4.2 | `tui/background.rs`、`event_loop.rs`、`handlers/mod.rs`、`mihomo_manager/{manager,watcher}.rs` | reservation ordering、local coalescing/visible overflow、stale work/core generations；`intentional_restart_suppresses_predecessor_exit_but_keeps_ready_ordered`、`final_cancellation_waits_for_transaction_cleanup_and_filters_old_core_snapshots`；100ms dirty render budget |
| batch safety；4.3 | `services/proxy.rs`、`tui/handlers/proxy.rs`、`app/mod.rs` | `delay_request_scheduler_caps_concurrency_and_preserves_input_order`：9 个门控 mock，最多 4 并发，反序完成 progress，结果输入顺序；`dropping_scheduler_aborts_owned_requests`；重复批次 guard |
| retained regression；4.4 | `mihomo_manager/{signal,watcher}.rs`、`tui/event_loop.rs`、`subscribe/fetch.rs`、`enhance/mod.rs` | 5 秒 SIGTERM fallback 只作用于自建非核心 child；watcher cancellation；draw-failure cleanup seam；gzip mock / Basic 空密码；TLS1.2 最低版本 builder policy；fake-IP IPv6 fixture |
| verification；5.2–5.4 | 此文档、`tasks.md`、GSD quick summary | 最终 serialized suite / locked build / strict OpenSpec 校验；六份 delta specs 的 requirement/scenario 家族通过以上实现与 fixture 关联 |

补充官方依据：VLESS TLS 是可选项，已按 [1.14.2 VLESS outbound](https://github.com/SagerNet/sing-box/blob/v1.14.2/protocol/vless/outbound.go) 的分支行为保留 `tls: false`；该核对属于源码证据，不是握手测试。

### 最终验证与边界

实施提交：`6aff4da7`。最终验证使用同一代码状态：

- `cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller`：退出码 0，571 个 CLI/TUI 单测、4 个配置集成测试、13 个共享核心单测，共 588 passed / 0 failed / 1 filtered。
- `cargo build --workspace --locked`：退出码 0，dev workspace build 完成；没有运行、安装或部署构建产物。
- `cargo fmt --all -- --check`、`git diff --check`：退出码 0。
- `openspec validate align-dual-core-and-client-behavior --strict`：valid。
- 四项 XDG 环境变量指向 `/tmp/clash-alignment-validation.b0a49x` 下的 `data`、`runtime`、`config`、`bin`；`CARGO_BUILD_JOBS=2`。没有修改 HOME/CARGO_HOME。磁盘配置测试的 app home、缺失 socket 和 mock listener 使用各自临时资源。
- 日志：`/tmp/clash-alignment-validation.b0a49x/test.log`，SHA-256 `9c8eee0fe2cc3bc80aa25a5042503b79593939f0bcc46b9514f2ad9988cedc53`；`/tmp/clash-alignment-validation.b0a49x/build.log`，SHA-256 `fecede7c2157efe2f16c8c8b9dce94b0fb4dd384dca8207046c8bf7636eb0ffe`。这些路径是本地验证记录，不是发布包。

命令明确跳过 `real_sing_box_spawns_and_answers_controller`，不使用 `--ignored`、`--include-ignored`、`cargo run` 或应用生命周期/服务命令。测试自己的 mock listeners、tempfiles 和非核心 child 不与现有实例共用资源。

未验证：真实核心启动/切换/重启、网络资产下载、TLS 握手与协议互通、TUN/权限、联网订阅、真实性能指标。没有替换已安装二进制或部署构建产物，也没有停止用户当前运行的 clash-tui。对于没有经过映射审查的 DNS/transport/routing 字段，当前行为是保留原文件并报告拒绝；不声明完整 Clash→sing-box 无损转换。

## Quick 261009-eth：引导更新与实际选型切换（2026-10-09）

用户本次报告为切换 sing-box 没有任何显示、设置切换没有效果，并确认当前运行的是 GUI。上面的 `6aff4da7`、22 项任务和 588 项测试均为历史记录。本轮审计发现历史 editor 测试可能进入生产 `sing-box check`，service 测试可能枚举系统候选；本轮分别改为静态 JSON fixture 与显式候选注入，并删除旧 settings 的环境依赖测试。历史日志未记录这些分支是否命中，因此不能以历史 588 次通过证明它们绝未执行。

代码提交：`7b9a5136`（检查/可信准备）、`bdf06fe6`（共享类型/事务）、`dcf03232`（TUI 消费）、`71ba7ccb`（GUI 运行时允许候选准备、实际应用保留所有权门禁）。最终测试对应 `71ba7ccb` 的源码内容。保留历史 22 项完成记录，并新增完成 6.1–6.5 五项 follow-up（共 27 项），没有同步或归档主规格，没有修改 ROADMAP。

新增 `guided_core` 回归共 **39 项**，覆盖两个核心的旧系统+离线兼容缓存、cache-before-network、未知新版拒绝降级、同版本独立候选、授权/manifest/错误 digest/ELF/版本/取消清理；事务顺序、各晚期失败、文件/kind/API/选择器恢复、GUI 运行时允许候选准备、GUI/foreign/live-record 拒绝生命周期应用；常见 Clash nodes+selector+MATCH+DNS 准备、关键 DNS/TLS 拒绝；TUI 取消、重复/陈旧 Ready、stream cancellation 独立交付、代际变化、持久诊断及中英 TestBackend 渲染。网络元数据和版本来自夹具/注入，未发生真实核心或真实 GitHub 资产下载。

最终验证环境使用 `/tmp/clash-guided-core-validation.ETH/{data,runtime,config,bin}` 分别提供 `XDG_DATA_HOME`、`XDG_RUNTIME_DIR`、`XDG_CONFIG_HOME`、`XDG_BIN_HOME`，`CARGO_BUILD_JOBS=2`。不更改 HOME/CARGO_HOME，也不另设 CARGO_TARGET_DIR，沿用项目编译缓存。测试中的磁盘配置与控制器使用自己的临时路径和 mock listener。精确命令：

```bash
XDG_DATA_HOME=/tmp/clash-guided-core-validation.ETH/data \
XDG_RUNTIME_DIR=/tmp/clash-guided-core-validation.ETH/runtime \
XDG_CONFIG_HOME=/tmp/clash-guided-core-validation.ETH/config \
XDG_BIN_HOME=/tmp/clash-guided-core-validation.ETH/bin \
CARGO_BUILD_JOBS=2 \
cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller

# With the same four temporary XDG values and CARGO_BUILD_JOBS=2:
cargo build --workspace --locked
cargo fmt --all --check
git diff --check
openspec validate align-dual-core-and-client-behavior --strict
```

| 检查 | 实际结果 | 日志（根目录 `/tmp/clash-guided-core-validation.ETH/`） | SHA-256 |
|---|---|---|---|
| 最终 safe suite | exit 0；605 CLI +4 integration +13 core = **622 passed**；0 failed；1 filtered；无 warnings | `suite-parent-final.log` | `f76b8402b1d52b70dc6924f550c6861a0397f1601df6b3315c87f56b7aab3ae4` |
| locked workspace build | exit 0；复用测试阶段编译后的最终源码产物 | `build-parent-final.log` | `22e0df986057b9ba5be07526ac5fb348862a3dedef42108383cc3e088acbb889` |
| fmt | exit 0 | `fmt-parent-final.log` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| diff | exit 0 | `diff-parent-final.log` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| OpenSpec strict | exit 0；valid | `openspec-parent-final.log` | `e34801d7350adbe5d185a626ad0d6452e3b914065571d94bffe1cbd63e24a253` |

构建产物仅为 `target/debug/clash-verge-cli`，SHA-256 `1f39a9db1e612fd92bdebbc10b4e3e98ee3299408019de938905d5c0da61fb62`。没有安装到 `/usr/local/bin`，没有覆盖 GUI 实际核心/已安装 CLI；用户实际运行入口仍未确认。没有运行 app、`cargo run`、真实核心 version/check、服务/提权安装、真实控制端请求或 process signal。

TUI 使用启动 `s`、重启 `r` 或 Settings 的 Proxy Core 行启动检查；`y`/Enter 授权下载，验证后显示实际版本/来源/路径，再以 `y`/Enter 确认配置准备和应用；`n`/Esc 取消，结果以 Enter/Esc 关闭。GUI 运行时允许检查与经授权下载、验证 CLI 自有候选；实际应用时会显示持久可关闭的所有权解释并拒绝启动/切换，不通过停止 GUI 实现切换。准备失败保留旧核心；commit/boot 失败执行自有回滚。联网下载、真实核心启动/切换、协议/TLS/TUN与权限仍未验证，mock 通过不能替代真实认证。

## Stale controller socket 修复（2026-10-09）

用户确认两个已验证缓存核心均因无 PID 记录的 controller socket 被拒绝。只读现场检查发现 `/run/user/1000/clash-verge-cli/external-controller.sock` 是 uid1000、inode269 的非符号链接 Unix socket；相邻 `mihomo.pid` 不存在，父目录和 runtime 目录均为 uid1000/0700。内核表中无精确路径，也无绝对路径对应同 dev/inode 的别名绑定。检查只读取元数据与 `/proc/net/unix`，没有连接、删除 socket 或操作进程。

根因：旧 `guided_record_check` 用 `exists()` 判断外部控制端，在 spawn 的遗留文件清理之前就拒绝两个目标核心；正常退出清理 PID 记录后仍可能留下 socket 文件。源码 `da535daa` 增加 `mihomo_manager/controller_socket.rs`，区分 Missing/Bound/Stale；检查 socket 类型、私有真实父目录、UID、可读取且可解析的内核状态，以及路径/文件身份别名。缺失路径仍检查残留绑定；旧 dead PID 记录不能豁免活跃端点。spawn barrier 在删除前复查内核状态与 dev/inode，拒绝错误向上传播并带实际路径。仅清理程序确认的私有遗留文件；开发期间未清理用户文件。

新增9项单测，最终13项 socket 聚焦测试包含既有回归。原始两项 fixture RED：exit101；缺失但仍绑定端点另有 RED：exit101。最终 GREEN：13 passed。目录权限修正及追加边界前的630项 suite 为中间结果，不作为最终证据。两个目标 kind 的实际 wrapper 均覆盖遗留放行、live/dead-record 拒绝、状态读取/解析失败、已 unlink 但仍绑定端点及应用边界重查；分类/清理 fixture 覆盖别名、含空格路径、非 socket、symlink、非私有目录和活跃文件保留。

最终验证使用 `/tmp/clash-stale-socket-validation/{data,runtime,config,bin}` 作为四个 XDG roots，`CARGO_BUILD_JOBS=2`，沿用项目 Cargo 缓存。未重设 HOME/CARGO_HOME。精确命令：

```bash
XDG_DATA_HOME=/tmp/clash-stale-socket-validation/data \
XDG_RUNTIME_DIR=/tmp/clash-stale-socket-validation/runtime \
XDG_CONFIG_HOME=/tmp/clash-stale-socket-validation/config \
XDG_BIN_HOME=/tmp/clash-stale-socket-validation/bin \
CARGO_BUILD_JOBS=2 \
cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller

# With the same four temporary XDG values and CARGO_BUILD_JOBS=2:
cargo build --workspace --locked
cargo build --workspace --release --locked
cargo fmt --all --check
git diff --check
openspec validate align-dual-core-and-client-behavior --strict
```

| 日志（`/tmp/clash-stale-socket-validation/logs/`） | 结果 | SHA-256 |
|---|---|---|
| `red.log` | exit101；2 failed | `4f8edb3f6d8634923150d7cfe6ef86153929338277b1d45fd074a2f840020f51` |
| `unlinked-red.log` | exit101；1 failed | `abfd642f693daa609ff65bbfe94d2f463c5fd731661064575c651ad691246a6a` |
| `green.log` | exit0；13 passed | `f9ffdb7a48db87b18174753de157f52f6d9cd63a5fc3e42e2a13f24d318e0b48` |
| `full-suite.log` | exit0；614 CLI +4 integration +13 core = **631 passed**，0 failed，1 filtered；无 warnings | `0ddebd1441bf829d89a48bf9e48c348758ed868e48f4d838a4e9116707a62b4d` |
| `build-debug.log` | exit0；复用最终源码编译缓存 | `22e0df986057b9ba5be07526ac5fb348862a3dedef42108383cc3e088acbb889` |
| `build-release.log` | exit0；optimized build，1m05s | `b6e759df364741ce761092ebba2168301c31205e2ad21b02d9fe2655ff286f7f` |
| `parent-fmt.log` | exit0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `parent-diff.log` | exit0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `parent-openspec.log` | exit0；strict valid | `e34801d7350adbe5d185a626ad0d6452e3b914065571d94bffe1cbd63e24a253` |

产物：`target/debug/clash-verge-cli` SHA-256 `07ff3f8ffe67eb6c4eb316fa2e3d7b25ed64a8d7861a9b5a753a009c8841dbc5`；`target/release/clash-verge-cli` SHA-256 `b40f38672c6d875bbeb1d25367d88ab03004151b42d2b4a85d07f118bdfc15cd`。仅构建，未执行或安装产物，未触碰用户运行的 TUI/GUI、核心、配置、现有端点和系统 binary；真实核心启动/切换、下载、TUN/权限及协议互通仍未验证。源码由 parent 独立审查；开发实现与隔离验证完成，真实使用结果须区分于这些 fixture 证据。

## Guided TUN 授权与继续流程（2026-10-09）

用户再次报告两核心启动失败，错误已转为缺少 TUN capability。被动 `getcap` 确认旧 `/usr/bin/verge-mihomo` 具有 `cap_net_admin,cap_net_raw=eip`，两个新缓存文件没有；`findmnt` 显示所在 `/home` btrfs 未列出 nosuid/noexec。未执行权限授予。根因是 guided Ready 直接进入 manager 只读硬门禁，绕过原 TUN 授权流程；Settings 授权始终解析 mihomo，旧 boolean resume 也不能保留切换目标。

源码 `19902dd1` 引入精确 `GuidedTunContext`，保留 id/generation/完整 PreparedCore/kind/path/version/intent/TUN 快照。所有权与代际在检查、授权前后及 apply 边界复核；权限结果走独立 operation channel，普通流取消不吞结果。授权前复核 managed 文件 receipt；显式确认和密码提交后才调用已有权限事务，成功后对同一路径再查 capability 并继续原启动/重启/切换。拒绝、失败、过期、重复结果和阻塞 worker 异常均不启动错误目标，保留旧运行信息并显示持久结果。

Settings 使用共享 kind 与确切已选文件的离线检查；没有已选文件时经既有下载确认流程，成功只报告有效 TUN 权限，不隐式启动/切换、修改选型或伪造运行版本。既有较旧选中文件可做权限设置，但非 reviewed 版本不能通过该入口启动。Root/capable/TUN-off 路径不触发不必要提权；文案区分有效 root 权限和文件 capability，不宣称二者等同。中英确认与密码窗显示核心/版本/长路径，setup-only 窗口不再显示“选型已提交”。

RED 回归实测取消将旧 Running 错改为 Stopped：exit101。最终新增13项 guided_tun 回归覆盖两核心与三种生命周期意图、精确文件重查、取消/七类过期 context、失败/worker panic、setup-only 状态、root/capable/TUN-off、receipt 和中英渲染。历史 legacy DNS skip 测试可能调用实际 pkcheck；本轮在运行测试前改为注入 rule-needed seam。旧日志不能证明此前该分支从未执行。本轮 privilege/core 结果全部来自注入，不运行 sudo/setcap/pkcheck、真实核心或应用。中间 suite 的文案失败与修改前通过结果均保留为历史，不替代最终日志。

最终四个 XDG roots：`/tmp/clash-guided-tun-validation/{data,runtime,config,bin}`，`CARGO_BUILD_JOBS=2`，沿用项目 Cargo 缓存，不重设 HOME/CARGO_HOME。命令：

```bash
XDG_DATA_HOME=/tmp/clash-guided-tun-validation/data \
XDG_RUNTIME_DIR=/tmp/clash-guided-tun-validation/runtime \
XDG_CONFIG_HOME=/tmp/clash-guided-tun-validation/config \
XDG_BIN_HOME=/tmp/clash-guided-tun-validation/bin \
CARGO_BUILD_JOBS=2 \
cargo test --workspace --all-targets --locked -- --test-threads=1 --skip real_sing_box_spawns_and_answers_controller

# With the same four temporary XDG values and CARGO_BUILD_JOBS=2:
cargo build --workspace --locked
cargo build --workspace --release --locked
cargo fmt --all --check
git diff --check
openspec validate align-dual-core-and-client-behavior --strict
```

| 日志（`/tmp/clash-guided-tun-validation/logs/`） | 结果 | SHA-256 |
|---|---|---|
| `red.log` | exit101；1 failed | `8a2709da5896fe205991a467ecc16f6a7b8aede854509543f924abd1eef790ac` |
| `actual-final-workspace-test.log` | exit0；627 CLI +4 integration +13 core = **644 passed**，0 failed，1 filtered；无 warnings | `109b61bfd83a27627a808cd92dbc1dca05842725cfc509fb0f87b6b2f2914829` |
| `actual-final-build.log` | exit0；复用最终源码编译缓存 | `22e0df986057b9ba5be07526ac5fb348862a3dedef42108383cc3e088acbb889` |
| `actual-final-release.log` | exit0；optimized build，1m04s | `134b4bfae09000f3ce4c5143a12334b538dd29f65150580d45182f2c1e7baec5` |
| `parent-fmt.log` | exit0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `parent-diff.log` | exit0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `parent-openspec.log` | exit0；strict valid | `e34801d7350adbe5d185a626ad0d6452e3b914065571d94bffe1cbd63e24a253` |

最终11个源码文件均与 `actual-final-source.sha256` 一致，清单 SHA-256 `54432eb0ac61387d968f06c502d122bbf92697f78719a3d3a91f11047c1b2ca2`。产物：debug CLI SHA-256 `c5687a84f5dc444038793645fe4f47e6039389898375bdbaed14ce321d45091d`；release CLI SHA-256 `3fa59eaf01dd792bc999196af5e266bd8a4e49ad2841d49043b7542c464e4238`。没有执行/安装产物或代用户授予权限，真实授权、核心启动/切换、网络/TUN/协议互通仍未验收；用户需自行重启新版并完成明确的 TUI 授权。本轮实现与隔离验证完成，debug session 保留 `awaiting_human_verify`。
