# 组 9 真机验证清单（需要真实终端 + 用户配合）

前提：`cargo build --release -p clash-verge-cli` 完成后，用 release 二进制在**测试数据目录**验证（避免影响正在运行的 TUI）：
```bash
XDG_DATA_HOME=/tmp/sb-verify XDG_RUNTIME_DIR=/tmp/sb-verify-runtime ./target/release/clash-verge-cli
```

## 9.2 切换安全专项
- [ ] 快速反复切换 mihomo↔singbox ≥5 次：无端口冲突报错、无旧核心复活（ps 确认单核心进程）
- [ ] 故意写坏 singbox.json（如截断 JSON）→ 切核/重启 → 状态栏报错且自动恢复旧配置

## 9.3 sing-box 端到端
- [ ] sing-box 二进制放入 `$XDG_DATA_HOME/clash-verge-cli/`（或 PATH）
- [ ] Settings 切到 singbox → 核心启动，状态栏显示 sing-box 版本
- [ ] 节点列表 / 选择节点 / 延迟测试 / 流量图 / 连接页 / 日志页 全部有数据
- [ ] 模式切换（rule/global）生效

## 9.4 路由管理双核往返
- [ ] 编辑态 E → D 删除一条 → W 保存 → `/rules` 运行时确认已消失
- [ ] 同一规则集在两种核心下编辑后运行时一致

## 9.5 高级配置端到端
- [ ] O 打开原始 JSON → 加 DNS 段或改 tun stack → 保存校验通过且生效
- [ ] TUN 开关在 sing-box 下实际创建 sb-tun0 接口并可关闭

## 9.6 GUI 互斥
- [ ] 启动 GUI 后切 singbox → 被阻止并提示
- [ ] singbox 模式下数据目录存在 core-owner.json；切回后消失

## 已知边界（非缺陷）
- Logical 规则仅 sing-box 模式可保存（clash 格式无此语法）
- rule providers 面板在 sing-box 下显示说明文案（上游空实现）
- 结构化 DNS/TUN 表单未做——O 原始 JSON 编辑覆盖全部字段
