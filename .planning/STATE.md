# Project State

## Current work

- debug `profile-script-core-switch` 已 resolved：GUI 兼容脚本引擎（有界 Boa）、双核心共享 composition、停止态选型仅提交 marker、owned-PID socket inode 端口归属证明、反向 selector 映射、FD 权限回退均已落地；OpenSpec 9.1–9.3 完成，累计 36/36。
- 接管收尾（Pi 父代理）：发现并修复 `owns_listener` 的 /proc fd ENOENT 竞态（全套件并发下 1/3 概率失败），新增 churn 回归测试；连续 5 轮全套件 672 项测试 0 失败（仅 real-sing-box E2E 排除）；locked debug/release build、fmt、OpenSpec strict 通过。
- 最终私有实际复验（产物 SHA256 `af80820f…f52522c`，含竞态修复）：sing-box 1.14.2 带脚本订阅启动成功（原报错路径），脚本改写进入 singbox.json 与 config.yaml；运行中双向切换 singbox↔mihomo 均成功；两核心 mixed 35123 HTTP 代理转发通过；GUI/服务/用户核心 4 个 PID+starttime 不变，46 个用户配置文件 hash 全同；私有端口已释放、tmux 夹具已拆除。证据 `/tmp/clash-script-live-n4g0e9jx/logs/pifix-*`。
- 用户最新授权可以操作 TUI，因为 GUI 正在运行；实际验证仅操作独立临时配置、私有控制端点/端口、关闭 TUN/系统代理的测试实例，必须保留 GUI 与用户配置。真实提权仍不执行。
- 当前分支 `fix/profile-script-core-selection`，基线 `2e6f696c`；source freeze manifest `/tmp/clash-profile-script-validation/source-freeze.sha256`（SHA256 `4c05711c…c6811`，17 文件全匹配）。父实际验证目录 `/tmp/clash-script-live-n4g0e9jx`：`config` 为改写规则脚本的合成订阅，`copied-user-config` 为关闭自动更新的用户配置副本，`data/clash-verge-cli` 为只复制字节的缓存候选，`drive_tui.py` 控制私有 tmux server；controller49715、mixed35123、私有 runtime/controller.sock，TUN/sysproxy关闭。
- 父实际旧产物 RED：`baseline-cli` SHA256 `3fa59eaf01dd792bc999196af5e266bd8a4e49ad2841d49043b7542c464e4238`；复制用户当前订阅 `profile use` 拒绝默认 no-op script；私有旧 TUI 停止态切 singbox Ready→apply 被 GUI presence 拒绝，测试 TUI 已 q 退出，无核心启动。证据在上述 live 目录 `logs/baseline-*`；GUI/service/core PID+starttime基线与用户配置文件hash已保存。
- 被动核对真实 CLI：TUN=false、system_proxy=true、mixed7897；GUI/服务监听7897和7890。保留原设置，实际启动只用独立端口。修复保留 CLI 单次 DNS source capture→overlay→commit/persist 契约，脚本处理不提前重复 DNS overlay；sing-box controller 固定127.0.0.1、读取配置端口，与manager API/readiness/generation一致。

- 当前 follow-up：debug `guided-core-tun-setup` 实现与隔离验证完成；源码 `19902dd1`，OpenSpec 8.1–8.3 完成，累计33/33。TUN 确认绑定确切候选，权限复核后继续原操作；Settings 权限设置按选中核心且不启动/切换。
- 最终隔离 suite：627 CLI +4 integration +13 core =644 passed，0 failed，1 real-core E2E filtered；locked debug/release build、fmt/diff/OpenSpec strict 通过，未执行产物。
- 被动 getcap：旧 `/usr/bin/verge-mihomo` 有 `cap_net_admin,cap_net_raw=eip`，新 `mihomo-v1.19.32` / `sing-box-v1.14.2` 均无；本轮没有实际 sudo/setcap/pkcheck 或真实配置/实例操作，真实授权与启动待用户自行验证。
- 前轮：debug `stale-controller-socket` 完成，两核心不再把遗留 Unix socket 当作外部活跃端点；源码 `da535daa`，OpenSpec 7.1–7.3 完成，历史 suite631 passed。
- 只读 runtime 证据：CLI 私有 Unix socket 文件存在，无 `mihomo.pid`、无 `/proc/net/unix` 精确路径或同 dev/inode 的别名绑定；实际遗留文件及用户实例保持不变。
- 前轮：quick 261009-eth 引导核心更新与可回滚实际选型切换。
- 3/3 quick tasks 与新增 OpenSpec 6.1–6.5 完成；39 项 guided 回归，最终 622 项隔离测试通过、1 项真实核心 E2E 显式 filtered；locked build、fmt/diff/OpenSpec strict 通过。代码提交 `7b9a5136`、`bdf06fe6`、`dcf03232`、`71ba7ccb`。
- 历史对齐实施 `6aff4da7` 的 22/22 tasks 与 588 项测试仅为历史基线；本轮不以旧日志证明 editor/core-check 分支从未执行。
- 现有 ROADMAP 和 add-singbox-dual-core 的待验收项未改动。
- GUI 运行时允许候选检查/确认下载，实际应用仍检查所有权并可见拒绝；用户本轮从 `src-tui` 运行 `cargo run --release`。开发验证未操作运行实例、真实配置/核心/控制端点，未安装到 `/usr/local/bin`；debug/release 产物已构建，加载新代码须用户自行重启。

Last activity: 2026-10-09 - Profile script + stopped selection debug resolved; fd-churn race fixed; private bidirectional live verification passed with GUI preserved

### Blockers/Concerns

- 真实网络下的 TLS/协议互通、TUN 授权（debug `guided-core-tun-setup` 仍 awaiting_human_verify）与性能指标未验证；隔离实际验证不等同于用户网络兼容认证。主规格同步和归档未执行。分支推送状态待复核（fork 远端未见 `fix/profile-script-core-selection`）。

### Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 261007-red | 双核心及客户端对齐 OpenSpec；保留运行实例；文档校验和隔离 test/build | 2026-10-07 | 9482663e | [261007-red-create-openspec-for-dual-core-and-client](./quick/261007-red-create-openspec-for-dual-core-and-client/) |
| 261007-ry8 | 双核心及客户端对齐实施；22/22 tasks；588 项隔离测试与 locked build | 2026-10-07 | 6aff4da7 | [261007-ry8-apply-dual-core-and-client-alignment-ope](./quick/261007-ry8-apply-dual-core-and-client-alignment-ope/) |
| 261009-eth | 引导可信更新/实际核心切换与回滚；3/3 tasks；622 项隔离测试与 locked build | 2026-10-09 | 71ba7ccb | [261009-eth-tui-test-build-gui](./quick/261009-eth-tui-test-build-gui/) |
