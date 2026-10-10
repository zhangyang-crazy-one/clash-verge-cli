# HANDOFF — add-singbox-dual-core 实施状态（写于组3进行中）

## 已完成（11/30，全部在分支 feat/singbox-dual-core）
- 组1 全部、组2 全部、组3 的 3.1/3.2/3.3/3.4(构建块)
- 最新提交：7ef4d5ac（ReloadStrategy + prevalidate）

## 下一步立即要做：3.4 收尾——事件循环接线

**目标**：event_loop.rs 中 3 处 `commit_runtime_config` 调用点（约 L874 模式切换、L1413 TUN/profile 提交、daemon 路径）按核心分叉：
- mihomo：现有热重载路径一字不改
- singbox：新函数 `commit_singbox_config` —— 流程：
  1. `RUNTIME_CONFIG_IO.lock()`
  2. 备份 `dirs::singbox_config_path()`（存在则读 bytes）
  3. 重新生成配置（调 `ManagerInner::write_singbox_runtime_config` 或抽出的公共生成器）
  4. `prevalidate_singbox_config(binary, path)`（binary 来自 `singbox_binary::candidate_without_install()`）
  5. 预校验失败 → 直接报错返回（旧配置还在跑）
  6. `manager.restart()`（内部走 spawn_core = barrier + probe）
  7. 重启后探测失败 → 恢复备份文件 → 再 restart 兜底 → 报错

**注意**：
- `write_singbox_runtime_config` 是 ManagerInner 私有 async fn（manager.rs ~L317），需提为 pub(crate) 或抽到 runtime_config.rs
- event_loop 持有 `manager` 与 `api` 两个句柄；api() 已按 CoreKind 返回正确传输
- 判断核心类型用 `manager.core_kind()`，策略映射已有 `ReloadStrategy::for_core`

## 之后队列
- 3.5 GUI 互斥：切换前进程扫描（clash-verge 进程名 + 非 TUI 子进程 verge-mihomo）+ 数据目录所有权标记文件（owner/core/pid）；挂点在 settings arm 7（event_loop）的 toggle 分支里
- 组4 SingboxApi：trait/传输已就绪，实体化即可
- 组5 订阅转换（全协议白名单见 design D3）
- 组6 IRouteRule（SUB-RULE 走原始片段透传）、组7 规则UI、组8 高级配置表单
- 组9 真机端到端验证 ×3

## 环境坑（必读）
1. bash stdout 被 orca hook 污染：cargo 结果一律 `> /tmp/x.log 2>&1` 后用 read 工具读
2. lens 的"测试失败"多为幻影（stdout 污染），以落盘日志为准；真失败会出现在日志文件的 `test result: FAILED` 行
3. 编辑前必须重读文件（自动格式化会改内容）；python 盲补丁已两次翻车（残留行/重复定义），优先 read+edit 锚点编辑
4. git add 务必指定文件路径——仓库根有 202MB MindTheGapps zip 和 .omo/ 垃圾
5. proxy_core.rs 曾被钩子清空过一次，靠 HEAD 恢复；提交前 `git status --short` 核对 diff 合理

## 更新（3.4 接线进行到一半）
已完成：
- manager.rs restart() 现按 CoreKind 分叉：SingBox 走 restart_singbox()（resolve→preflight→stop→重新生成配置→spawn_core 带探测），修复了原先 sing-box 模式下 restart 会错拉 mihomo 的 bug
- write_singbox_runtime_config 已 pub(crate)
- runtime_config.rs 新增 apply_singbox_restart(manager)：备份→重生成→prevalidate→restart→失败回滚旧文件+兜底重启

- ✅ 已完成（本轮）
- event_loop.rs 两个调用点按核分叉：
  1. `apply_chain_config`（~L866，fn 签名只有 api/chain_nodes/enable_tun，无 manager）——需加 manager 参数并找其调用点传入
  2. profile switch 处（~L1413，作用域内有 manager，`let api = manager.api()` 在附近）——包一层 `if manager.core_kind() == CoreKind::SingBox { apply_singbox_restart(&manager) } else { commit_runtime_config(...) }`
- 注意：sing-box 下 PATCH /configs 的 mode 切换可用，无需动 apply_clash_mode

## 更新（组5进行中）
- 5.1 ✅ convert_node：11协议字段映射表 + TLS/transport 通用处理 + 字段级降级报告
- 5.2 🔶 convert_profile() ✅（节点+组+跳过报告）；剩余：
  1. apply_singbox_restart 接入 convert_profile——需要 profile YAML 内容：event_loop 调用点处有 item（PrfItem），从 app_profiles_dir/item.file 读文件传入；签名改为 apply_singbox_restart(manager, config_yaml: &str)
  2. write_singbox_runtime_config 改为接受 outbounds/groups 参数（当前写死空骨架）
  3. UI 报告：切换成功后 status_msg 显示 "N nodes, M skipped, K fields degraded"
- 组6 IRouteRule：注意 SUB-RULE 无等价物走原始片段；往返测试用 property 风格
- 测试环境：cargo 输出必须重定向文件后 read 读取（hook 污染回显）

## 更新（组6完成）
- 组6 ✅ 全部：src-tui/src/routing.rs（IRouteRule 模型 + 双核序列化 + Raw 透传）
- 下一步组7 路由管理 UI 的实施要点：
  1. App state 需新增编辑缓冲（pending_rules: Vec<IRouteRule>），Rules 视图从只读切到编辑态
  2. CRUD 动作走 Action enum 新变体（RuleAdd/RuleDelete/RuleMove/RuleEdit），event_loop 处理后写 profile
  3. 保存路径：IRouteRule → to_clash_rule_str 列表写回 profile YAML（mihomo）/ route rules JSON（singbox）；Raw 片段原样保留
  4. 保存生效：复用 3.4 的 ReloadStrategy 分叉（已就绪）
  5. 注意 event_loop 是巨型 select 循环——编辑前必须重读锚点区，小步提交

## 更新（代码任务全部完成，2026-08 会话）

7.2 / 7.5 / 8.1 已实现，组1-8 全部勾选；**剩余仅组9 的 9.2-9.6 真机验证（需用户真实终端配合）**。

本轮改动要点：
- **G1 修复（重启覆盖）**：`restart_singbox()` 与自动重启原先用空骨架覆盖 singbox.json，会冲掉 `apply_singbox_restart` 刚写入的转换结果。现统一走 `ManagerInner::write_singbox_full()` → `SingboxParts::assemble()`：活跃 profile YAML → convert_profile + load_profile_rules → to_singbox_json + sidecar 逻辑规则 + rule_sets + DNS spec → 单次装配落盘。节点/规则/规则集在崩溃自愈后也不再丢失。
- **G2 修复（route.rules 注入）**：`ConfigInput` 新增 `route_rules`/`dns` 字段；`generate_config` 注入 `route.rules` 数组与 `dns` 段；`route.default_domain_resolver` 由 manager 装配层写入。routing.rs 的 dead_code 标记已随启用移除。
- **G3 修复（逻辑规则无存储）**：新增 `singbox-rules.json` sidecar（LOGICAL_RULES_FILE）。保存时按核分叉：mihomo 下含逻辑规则直接报错；sing-box 下非逻辑规则写 profile YAML、逻辑规则写 sidecar。生成时追加在 profile 规则之后。
- **7.2 表单**：Rules 视图按 `f` 打开 `kind=value>target` 结构化输入（domain/suffix/keyword/ip/port/process/set × DIRECT/REJECT/出站名），`routing::build_simple_rule` 解析+校验。
- **7.5 确认弹窗**：`Overlay::RulesRestartConfirmation`——sing-box 下 W 保存先弹 y/N（n/Esc 取消保留编辑缓冲），确认后 `spawn_rules_save` 批量应用。
- **8.1 DNS 表单**：Settings 按 `d` 进入编辑器（servers/rules 双列表 Tab 切换、j/k 移动、x 删除、a/r/R 输入、w 应用重启、d 退出），持久化 `singbox-dns.json`；`dns.rs` 新格式模型（udp/tls/https/quic/local/fakeip server、detour、suffix/keyword/cidr 分流规则、fakeip 默认网段、domain_resolver 引导），绝不生成 legacy address 语法；空 spec 不产出 dns 段。
- 验证：cargo test 325 通过 / 0 失败（e2e 仍 #[ignore] 待真机）；clippy 无新增警告。

下一步：用户真机执行 9.2-9.6（切换回滚、双核 e2e、路由/DNS/TUN/GUI 互斥端到端）后归档本 change。
