#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    SimplifiedChinese,
}

impl Language {
    pub fn from_config(value: Option<&str>) -> Self {
        if value.is_some_and(|language| language.to_ascii_lowercase().starts_with("zh")) {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    pub const fn config_code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::English => Self::SimplifiedChinese,
            Self::SimplifiedChinese => Self::English,
        }
    }
}

/// Translate a stable TUI key. English is the complete fallback locale.
pub fn tr(language: Language, key: &'static str) -> &'static str {
    if language == Language::SimplifiedChinese {
        chinese(key).unwrap_or_else(|| english(key))
    } else {
        english(key)
    }
}

fn english(key: &'static str) -> &'static str {
    match key {
        "core_update.title" => "Core check and update",
        "core_update.checking" => "Checking compatible local cores…",
        "core_update.consent" => "Compatible core update required",
        "core_update.downloading" => "Downloading pinned official archive…",
        "core_update.verifying" => "Verifying trusted SHA-256, executable and actual version…",
        "core_update.ready" => "Verified candidate ready; current core remains unchanged",
        "core_update.preparing" => "Preparing subscription configuration and TUN preflight…",
        "core_update.switching" => "Switching owned core; waiting for readiness…",
        "core_update.success" => "Core selection committed",
        "core_update.failed" => "Core operation failed",
        "core_update.cancelled" => "Core operation cancelled",
        "core_update.observed" => "Observed version/source",
        "core_update.target" => "Reviewed target",
        "core_update.verified" => "Verified version",
        "core_update.confirm_download" => {
            "y / Enter: authorize managed download and integrity validation · n / Esc: cancel"
        }
        "core_update.confirm_switch" => {
            "y / Enter: prepare config and apply this verified core · n / Esc: keep current core"
        }
        "core_update.cancel_hint" => "Esc / n: cancel this operation; current core stays running during preparation",
        "core_update.dismiss" => "Enter / Esc: close · retry from Start / Restart / Settings",
        "view.home" => "Home",
        "view.proxies" => "Proxies",
        "view.profiles" => "Profiles",
        "view.connections" => "Connections",
        "view.rules" => "Rules",
        "view.logs" => "Logs",
        "view.unlock" => "Unlock",
        "view.settings" => "Settings",
        "menu" => "Menu",
        "help" => "Help",
        "help.global" => "Global",
        "help.current_view" => "Current view",
        "help.close" => "Press ? or Esc to close",
        "help.global_commands" => "1-8 view | Tab focus | h/l focus | j/k move",
        "help.global_commands_more" => "/ filter | Esc close | q quit",
        "input.views_focus" => "1-8 views | Tab focus",
        "input.filter" => "FILTER",
        "input.close" => "CLOSE",
        "input.apply_cancel" => " | Enter apply | Esc cancel",
        "input.confirm_cancel" => " | Enter confirm | Esc cancel",
        "input.url" => "URL: ",
        "input.submit_cancel" => " Enter submit | Esc cancel",
        "input.help_quit" => " | ? help | q quit",
        "hint.menu" => "Enter content | j/k views",
        "hint.proxies" => "Enter select | t/T delay | o sort | H hide failed | / filter | c chain",
        "hint.profiles" => "i import | u update | Enter switch | / filter",
        "hint.connections" => "Enter/d close | / filter",
        "hint.rules" => "j/k browse | Tab panel | Enter update provider | r refresh | / filter",
        "hint.logs" => "/ filter | L level | j/k browse",
        "hint.unlock" => "r check all",
        "hint.settings" => "j/k select · Enter toggle",
        "hint.home" => "s start | r restart | m mode",
        "status.running" => "RUNNING",
        "status.starting" => "STARTING",
        "status.stopped" => "STOPPED",
        "status.error" => "ERROR",
        "status.profile" => "profile",
        "status.chain" => "chain",
        "status.on" => "on",
        "status.off" => "off",
        "common.unknown" => "unknown",
        "common.no_matches" => "no matches",
        "common.on" => "on",
        "common.off" => "off",
        "proxies.sort" => "Order",
        "proxies.sort_config" => "profile order",
        "proxies.sort_delay" => "by delay",
        "proxies.sort_name" => "by name",
        "proxies.hiding_failed" => "hiding failed",
        "proxies.showing_failed" => "showing failed nodes",
        "logs.level" => "level",
        "logs.level_failed" => "Could not change the log level",
        "logs.level_needs_core" => "Start the core to change its log level",
        "home.usage" => "Used",
        "home.updated" => "Updated",
        "home.expires" => "Expires",
        "home.expired" => "expired",
        "home.days_left" => "days left",
        "home.outbound" => "Exit",
        "home.outbound_unknown" => "loading proxies...",
        "home.system_proxy" => "System proxy",
        "home.session_total" => "Since start",
        "common.none" => "none",
        "common.yes" => "yes",
        "common.no" => "no",
        "common.failed" => "failed",
        "home.core" => "Core",
        "home.core_accepts" => "Core accepts API",
        "home.core_accepting" => "Core is accepting API requests",
        "home.core_waiting" => "Waiting for the core to become ready",
        "home.press_start" => "Press s to start",
        "home.press_start_core" => "Press s to start the core",
        "home.starting_core" => "Starting core (auto-installs mihomo if needed)...",
        "home.core_started" => "Core started",
        "home.core_attached" => "Attached to running core",
        "home.core_downloaded" => "downloaded mihomo",
        "home.core_cached" => "using installed mihomo",
        "home.core_system" => "system verge-mihomo",
        "home.active_profile" => "Active Profile",
        "home.no_active_profile" => "No active profile",
        "home.import_profile" => "Press 3, then i to import",
        "home.proxy_system" => "Proxy / System",
        // TUN state labels for the Home "Proxy / System" panel. Shown next
        // to the system proxy line so a misconfigured TUN (config on, binary
        // missing capabilities, core not running) is visibly distinct from
        // a healthy "on" — the previous code displayed only the config
        // intent and the user could not tell TUN was dead.
        "home.tun_on" => "on",
        "home.tun_off" => "off",
        "home.tun_needs_setup" => "on (needs setup)",
        "home.tun_pending" => "on (core starting)",
        "home.chain_enabled" => "Chain selection enabled",
        "home.direct_selection" => "Direct proxy selection",
        "home.menu_focus" => "Menu focus",
        "home.content_focus" => "Content focus",
        "home.traffic" => "Traffic",
        "home.no_traffic" => "No traffic sample",
        "home.press_traffic" => "Press s for traffic",
        "home.start_traffic" => "Start the core to collect traffic",
        "home.messages" => "Recent Messages",
        "home.no_messages" => "No recent messages",
        "proxies.title" => "Proxies",
        "proxies.groups" => "Groups",
        "proxies.proxy_groups" => "Proxy Groups",
        "proxies.nodes" => "Nodes",
        "proxies.detail" => "Proxy Detail",
        "proxies.type" => "Type",
        "proxies.selected" => "Selected",
        "proxies.group" => "Group",
        "proxies.delay" => "Delay",
        "proxies.current" => "Current",
        "proxies.not_tested" => "not tested",
        "proxies.no_selection" => "No proxy selection",
        "proxies.start_or_profile" => "Start the core or switch an active profile",
        "proxies.chain_off" => "Chain editing: off",
        "proxies.chain_route" => "Chain route: select entry, then exit",
        "proxies.chain_entry" => "Chain entry",
        "proxies.select_exit" => "select exit",
        "proxies.chain_nodes" => "nodes",
        "proxies.batch_delay" => "Batch delay test",
        "proxies.no_testable" => "No testable leaf proxy nodes",
        "proxies.hint_normal" => "Enter group/node | t | T | o | H | c",
        "proxies.hint_chain" => "Enter add | a apply | x clear | c",
        "profiles.title" => "Profiles",
        "profiles.detail" => "Profile Detail",
        "profiles.type" => "Type",
        "profiles.source" => "Source",
        "profiles.local" => "local profile",
        "profiles.no_selection" => "No profile selected",
        "profiles.import" => "Press i to import a subscription URL",
        "profiles.hint" => "Enter switch | u update | i add",
        "profiles.none" => "No profiles - press i to import a subscription",
        "connections.title" => "Connections",
        "connections.detail" => "Connection Detail",
        "connections.unknown_host" => "unknown host",
        "connections.network" => "Network",
        "connections.rule" => "Rule",
        "connections.transfer" => "Up: {up} B | Down: {down} B",
        "connections.close_hint" => "Enter or d: close selected connection",
        "connections.none" => "No connection selected",
        "connections.browse_filter" => "j/k browse | / filter",
        "connections.loading" => "loading connections...",
        "connections.empty" => "no active connections",
        "rules.title" => "Rules",
        "rules.limited" => "Limited provider support",
        "rules.inventory" => "Mihomo rule-provider inventory is not exposed by src-tui yet.",
        "rules.cached" => "Recent connection rules below are real cached data.",
        "rules.recent" => "Recent Matches",
        "rules.empty" => "No recent matched rules",
        "logs.title" => "Logs",
        "logs.loading" => "listening for logs...",
        "logs.empty" => "no logs received yet",
        "logs.filter" => "filter",
        "unlock.title" => "Unlock",
        "unlock.available" => "available",
        "unlock.originals_only" => "originals only",
        "unlock.unavailable" => "unavailable",
        "unlock.failed" => "failed",
        "unlock.checking" => "checking...",
        "unlock.press_r" => "Press r to check which services the current exit node reaches.",
        "unlock.via" => "Requests go through the core's proxy port, so results describe the exit node.",
        "unlock.needs_core" => "Start the core to run the checks",
        "unlock.exit" => "Exit",
        "unlock.checked_at" => "checked at",
        "unlock.service" => "Service",
        "unlock.status" => "Status",
        "unlock.region" => "Region",
        "unlock.detail" => "Detail",
        "settings.runtime" => "Runtime Settings",
        "settings.runtime_heading" => "Runtime",
        "settings.core" => "Core",
        "settings.profile_count" => "Profile count",
        "settings.mihomo_mode" => "Mihomo mode",
        "settings.ports" => "Ports",
        "settings.core_pid" => "Core PID",
        "settings.language" => "Language",
        "settings.change_language" => "Enter to switch language",
        "settings.system" => "System Proxy / TUN",
        "settings.gui_config" => "Writable settings",
        "settings.system_proxy" => "System proxy",
        "settings.dns_config" => "DNS config",
        "settings.proxy_host" => "Proxy host",
        "settings.not_configured" => "not configured",
        "settings.gui_managed" => "GUI-managed",
        "settings.not_running" => "not running",
        "settings.on" => "on",
        "settings.off" => "off",
        "settings.readonly_note" => {
            "System proxy, TUN, DNS, service controls, and autostart are all wired; toggles persist here."
        }
        "settings.writable_hint" => "Enter toggles the highlighted setting. TUN reloads or restarts the core.",
        "settings.language_saved" => "Language saved",
        "settings.language_save_failed" => "Could not save language",
        "settings.sysproxy_on" => "System proxy enabled",
        "settings.sysproxy_off" => "System proxy disabled",
        "settings.tun_on" => "TUN enabled (reloading core)",
        "settings.tun_off" => "TUN disabled (reloading core)",
        "settings.tun_saved_on" => "TUN enabled (saved; apply on next start)",
        "settings.tun_saved_off" => "TUN disabled (saved; apply on next start)",
        "settings.mode_set" => "Mode set",
        "settings.save_failed" => "Could not save settings",
        "settings.tun_setup" => "TUN setup",
        "settings.tun_capable" => "capabilities present",
        "settings.tun_missing" => "not configured — Enter grants once",
        "settings.tun_setup_prompt" => {
            "Enter password to install the TUN capability and the systemd-resolved DNS polkit rule for mihomo"
        }
        "settings.tun_setup_present" => "TUN setup already complete — capability and DNS rule present",
        "settings.tun_dns_rule_missing" => "TUN DNS polkit rule missing — run TUN setup once to avoid system dialogs",
        "home.mode" => "Mode",
        "dialog.confirm_close" => "Close active connection?",
        "dialog.target" => "Target",
        "dialog.password.title" => "Administrator privileges",
        "dialog.password.prompt" => "Password: ",
        "dialog.password.hint" => "Enter = confirm | Esc = cancel",
        "dialog.trust" => "Trust Host",
        "dialog.trust_title" => "Import blocked by SSRF safety check",
        "dialog.trust_warning" => {
            "This subscription resolves to a private or loopback address. Confirming trusts only this host for this profile, so it can be imported and updated later."
        }
        "dialog.trust_confirm" => "y = trust & import | n/Esc = cancel (no trust saved)",
        "dialog.trust_update" => "Trust & Update",
        "dialog.trust_update_title" => "Update blocked by SSRF safety check",
        "dialog.trust_update_warning" => {
            "This subscription resolves to a private or loopback address. Confirming trusts only this host for this profile and retries the update with it allow-listed."
        }
        "dialog.trust_update_confirm" => "y = trust & update | n/Esc = cancel (update stays failed)",
        "dialog.tun_setup" => "TUN Setup",
        "dialog.tun_setup_title" => "TUN needs one-time setup",
        "dialog.tun_setup_warning" => {
            "Starting with TUN enabled needs the mihomo file capability and the systemd-resolved DNS polkit rule. Installing them now means core start requires no system dialogs."
        }
        "dialog.tun_setup_confirm" => "y = setup now | n/Esc/q = start without setup",
        "dialog.tun_setup_confirm_hard" => "y = setup now | n/Esc/q = cancel start (TUN setup required)",
        "settings.tun_capability_missing" => "TUN capability missing",
        "settings.service" => "System service",
        "settings.service_status_running" => "installed · enabled · running",
        "settings.service_status_enabled_stopped" => "installed · enabled · stopped",
        "settings.service_status_running_disabled" => "installed · running · not enabled",
        "settings.service_status_installed_disabled" => "installed · not enabled · stopped",
        "settings.service_status_not_installed" => "not installed",
        "settings.service_install_prompt" => {
            "Enter password to install the clash-verge-cli system service (runs the core at boot)"
        }
        "settings.service_uninstall_prompt" => "Enter password to uninstall the clash-verge-cli system service",
        "settings.service_installed" => "Service installed and started",
        "settings.service_uninstalled" => "Service uninstalled",
        "settings.service_failed" => "Service action failed",
        "settings.service_cancelled" => "Service action cancelled",
        "settings.service_uninstall_cancelled" => "Service uninstall cancelled",
        "dialog.service_uninstall" => "Uninstall service",
        "dialog.service_uninstall_title" => "Remove the system service?",
        "dialog.service_uninstall_warning" => {
            "This removes the clash-verge-cli systemd service. The core will no longer start automatically at boot."
        }
        "dialog.service_uninstall_confirm" => "y = uninstall | n/Esc/q = cancel",
        "dialog.service_uninstall_hint" => "Uninstall the system service? y = uninstall | n/Esc/q = cancel",
        "settings.auto_launch" => "Launch at login",
        "settings.auto_launch_on_msg" => "Autostart enabled — core starts at login",
        "settings.auto_launch_off_msg" => "Autostart disabled",
        "settings.auto_launch_failed" => "Could not change autostart",
        "settings.autostart_conflicts_service" => {
            "Disable the system service first — 'Launch at login' and the system service are mutually exclusive"
        }
        "settings.service_conflicts_autostart" => {
            "Disable 'Launch at login' first — the system service and login autostart are mutually exclusive"
        }
        "settings.sudo_hint" => "TUN setup + service install/uninstall ask for sudo; start/toggle never prompt.",
        _ => key,
    }
}

fn chinese(key: &'static str) -> Option<&'static str> {
    Some(match key {
        "core_update.title" => "核心检查与更新",
        "core_update.checking" => "正在检查本地兼容核心…",
        "core_update.consent" => "需要更新到已审查的兼容核心",
        "core_update.downloading" => "正在下载固定版本的官方归档…",
        "core_update.verifying" => "正在验证可信 SHA-256、可执行格式和实际版本…",
        "core_update.ready" => "候选核心已验证就绪；当前核心保持不变",
        "core_update.preparing" => "正在转换当前订阅配置并检查 TUN 权限…",
        "core_update.switching" => "正在切换自有核心，等待控制端就绪…",
        "core_update.success" => "核心选型已提交",
        "core_update.failed" => "核心操作失败",
        "core_update.cancelled" => "核心操作已取消",
        "core_update.observed" => "检测版本与来源",
        "core_update.target" => "已审查目标版本",
        "core_update.verified" => "已验证版本",
        "core_update.confirm_download" => "y / Enter：授权下载并校验 · n / Esc：取消",
        "core_update.confirm_switch" => "y / Enter：准备配置并应用已验证核心 · n / Esc：保留当前核心",
        "core_update.cancel_hint" => "Esc / n：取消本次操作；准备阶段旧核心继续运行",
        "core_update.dismiss" => "Enter / Esc：关闭；可从启动、重启或设置重试",
        "view.home" => "首页",
        "view.proxies" => "代理",
        "view.profiles" => "订阅",
        "view.connections" => "连接",
        "view.rules" => "规则",
        "view.logs" => "日志",
        "view.unlock" => "解锁",
        "view.settings" => "设置",
        "menu" => "菜单",
        "help" => "帮助",
        "help.global" => "全局",
        "help.current_view" => "当前页面",
        "help.close" => "按 ? 或 Esc 关闭",
        "help.global_commands" => "1-8 切换页面 | Tab 切换焦点 | h/l 切换区域 | j/k 移动",
        "help.global_commands_more" => "/ 过滤 | Esc 关闭 | q 退出",
        "input.views_focus" => "1-8 页面 | Tab 焦点",
        "input.filter" => "过滤",
        "input.close" => "关闭",
        "input.apply_cancel" => " | Enter 应用 | Esc 取消",
        "input.confirm_cancel" => " | Enter 确认 | Esc 取消",
        "input.url" => "地址：",
        "input.submit_cancel" => " Enter 提交 | Esc 取消",
        "input.help_quit" => " | ? 帮助 | q 退出",
        "hint.menu" => "Enter 进入内容 | j/k 切换页面",
        "hint.home" => "s 启动 | r 重启 | m 模式",
        "hint.proxies" => "Enter 选择 | t/T 测速 | o 排序 | H 隐藏不可用 | / 过滤 | c 链式代理",
        "hint.profiles" => "i 导入 | u 更新 | Enter 切换 | / 过滤",
        "hint.connections" => "Enter/d 关闭 | / 过滤",
        "hint.rules" => "j/k 浏览 | Tab 切换面板 | Enter 更新提供商 | r 刷新 | / 过滤",
        "hint.logs" => "/ 过滤 | L 级别 | j/k 浏览",
        "hint.unlock" => "r 全部检测",
        "hint.settings" => "j/k 选择 · Enter 切换",
        "status.running" => "运行中",
        "status.starting" => "启动中",
        "status.stopped" => "已停止",
        "status.error" => "错误",
        "status.profile" => "订阅",
        "status.chain" => "链式代理",
        "status.on" => "开",
        "status.off" => "关",
        "common.unknown" => "未知",
        "common.no_matches" => "没有匹配项",
        "common.on" => "开",
        "common.off" => "关",
        "proxies.sort" => "排序",
        "proxies.sort_config" => "配置顺序",
        "proxies.sort_delay" => "按延迟",
        "proxies.sort_name" => "按名称",
        "proxies.hiding_failed" => "隐藏不可用",
        "proxies.showing_failed" => "显示不可用节点",
        "logs.level" => "级别",
        "logs.level_failed" => "无法修改日志级别",
        "logs.level_needs_core" => "请先启动内核再修改日志级别",
        "home.usage" => "已用",
        "home.updated" => "更新于",
        "home.expires" => "到期",
        "home.expired" => "已过期",
        "home.days_left" => "天后",
        "home.outbound" => "出口",
        "home.outbound_unknown" => "正在加载代理...",
        "home.system_proxy" => "系统代理",
        "home.session_total" => "本次启动以来",
        "common.none" => "无",
        "common.yes" => "是",
        "common.no" => "否",
        "common.failed" => "失败",
        "home.core" => "内核",
        "home.core_accepts" => "内核正在响应 API",
        "home.core_accepting" => "内核正在响应 API 请求",
        "home.core_waiting" => "正在等待内核就绪",
        "home.press_start" => "按 s 启动",
        "home.press_start_core" => "按 s 启动内核",
        "home.starting_core" => "正在启动内核（必要时自动下载 mihomo）...",
        "home.core_started" => "内核已启动",
        "home.core_attached" => "已连接到运行中的内核",
        "home.core_downloaded" => "已自动下载 mihomo",
        "home.core_cached" => "使用已安装的 mihomo",
        "home.core_system" => "系统 verge-mihomo",
        "home.active_profile" => "当前订阅",
        "home.no_active_profile" => "未选择订阅",
        "home.import_profile" => "按 3 后按 i 导入订阅",
        "home.proxy_system" => "代理 / 系统",
        // TUN 状态标签：在“代理 / 系统”面板中与系统代理同行显示，
        // 让「配置开启了 TUN 但权限缺失 / 内核未启动」与「正常运行」
        // 在 UI 上有明确区分（之前只显示配置意图，用户无法察觉 TUN 已死）。
        "home.tun_on" => "开",
        "home.tun_off" => "关",
        "home.tun_needs_setup" => "开（需设置权限）",
        "home.tun_pending" => "开（内核启动中）",
        "home.chain_enabled" => "已启用链式代理选择",
        "home.direct_selection" => "直接选择代理",
        "home.menu_focus" => "菜单焦点",
        "home.content_focus" => "内容焦点",
        "home.traffic" => "流量",
        "home.no_traffic" => "暂无流量数据",
        "home.press_traffic" => "按 s 获取流量",
        "home.start_traffic" => "启动内核以获取流量",
        "home.messages" => "最近消息",
        "home.no_messages" => "暂无最近消息",
        "proxies.title" => "代理",
        "proxies.groups" => "组",
        "proxies.proxy_groups" => "代理组",
        "proxies.nodes" => "节点",
        "proxies.detail" => "代理详情",
        "proxies.type" => "类型",
        "proxies.selected" => "当前选择",
        "proxies.group" => "组",
        "proxies.delay" => "延迟",
        "proxies.current" => "当前节点",
        "proxies.not_tested" => "未测速",
        "proxies.no_selection" => "未选择代理",
        "proxies.start_or_profile" => "请启动内核或切换活动订阅",
        "proxies.chain_off" => "链式编辑：关",
        "proxies.chain_route" => "链式路径：先选择入口，再选择出口",
        "proxies.chain_entry" => "链式入口",
        "proxies.select_exit" => "请选择出口",
        "proxies.chain_nodes" => "个节点",
        "proxies.batch_delay" => "批量测速",
        "proxies.no_testable" => "没有可测速的叶子节点",
        "proxies.hint_normal" => "Enter 组/节点 | t | T | o | H | c",
        "proxies.hint_chain" => "Enter 添加 | a 应用 | x 清空 | c",
        "profiles.title" => "订阅",
        "profiles.detail" => "订阅详情",
        "profiles.type" => "类型",
        "profiles.source" => "来源",
        "profiles.local" => "本地订阅",
        "profiles.no_selection" => "未选择订阅",
        "profiles.import" => "按 i 导入订阅地址",
        "profiles.hint" => "Enter 切换 | u 更新 | i 添加",
        "profiles.none" => "暂无订阅，按 i 导入订阅地址",
        "connections.title" => "连接",
        "connections.detail" => "连接详情",
        "connections.unknown_host" => "未知主机",
        "connections.network" => "网络",
        "connections.rule" => "规则",
        "connections.transfer" => "上传：{up} B | 下载：{down} B",
        "connections.close_hint" => "Enter 或 d：关闭选中连接",
        "connections.none" => "未选择连接",
        "connections.browse_filter" => "j/k 浏览 | / 过滤",
        "connections.loading" => "正在加载连接...",
        "connections.empty" => "暂无活动连接",
        "rules.title" => "规则",
        "rules.limited" => "提供商功能受限",
        "rules.inventory" => "src-tui 暂未开放 Mihomo 规则提供商清单。",
        "rules.cached" => "下方最近连接规则来自真实缓存数据。",
        "rules.recent" => "最近命中",
        "rules.empty" => "暂无最近命中规则",
        "logs.title" => "日志",
        "logs.loading" => "正在监听日志...",
        "logs.empty" => "暂无日志",
        "logs.filter" => "过滤",
        "unlock.title" => "解锁",
        "unlock.available" => "可用",
        "unlock.originals_only" => "仅自制剧",
        "unlock.unavailable" => "不可用",
        "unlock.failed" => "检测失败",
        "unlock.checking" => "检测中...",
        "unlock.press_r" => "按 r 检测当前出口节点可以访问哪些服务。",
        "unlock.via" => "请求经由内核代理端口发出，结果反映的是出口节点。",
        "unlock.needs_core" => "请先启动内核再进行检测",
        "unlock.exit" => "出口",
        "unlock.checked_at" => "检测于",
        "unlock.service" => "服务",
        "unlock.status" => "状态",
        "unlock.region" => "地区",
        "unlock.detail" => "说明",
        "settings.runtime" => "运行设置",
        "settings.runtime_heading" => "运行状态",
        "settings.core" => "内核",
        "settings.profile_count" => "订阅数量",
        "settings.mihomo_mode" => "Mihomo 模式",
        "settings.ports" => "端口",
        "settings.core_pid" => "内核 PID",
        "settings.language" => "语言",
        "settings.change_language" => "按 Enter 切换语言",
        "settings.system" => "系统代理 / TUN",
        "settings.gui_config" => "可写设置",
        "settings.system_proxy" => "系统代理",
        "settings.dns_config" => "DNS 配置",
        "settings.proxy_host" => "代理地址",
        "settings.not_configured" => "未配置",
        "settings.gui_managed" => "由 GUI 管理",
        "settings.not_running" => "未运行",
        "settings.on" => "开",
        "settings.off" => "关",
        "settings.readonly_note" => "系统代理、TUN、DNS、服务控制与开机自启均已接入，开关会持久化到本配置。",
        "settings.writable_hint" => "Enter 切换高亮项。开启/关闭 TUN 会重载或重启内核。",
        "settings.language_saved" => "语言已保存",
        "settings.language_save_failed" => "无法保存语言设置",
        "settings.sysproxy_on" => "已开启系统代理",
        "settings.sysproxy_off" => "已关闭系统代理",
        "settings.tun_on" => "已开启 TUN（正在重载内核）",
        "settings.tun_off" => "已关闭 TUN（正在重载内核）",
        "settings.tun_saved_on" => "已开启 TUN（已保存，下次启动生效）",
        "settings.tun_saved_off" => "已关闭 TUN（已保存，下次启动生效）",
        "settings.mode_set" => "模式已切换",
        "settings.save_failed" => "无法保存设置",
        "settings.tun_setup" => "TUN 权限设置",
        "settings.tun_capable" => "已具备权限",
        "settings.tun_missing" => "未配置 — Enter 一次性授权",
        "settings.tun_setup_prompt" => "输入密码为 mihomo 安装 TUN 权限与 systemd-resolved DNS polkit 规则",
        "settings.tun_setup_present" => "TUN 设置已完成 — 权限与 DNS 规则均已安装",
        "settings.tun_dns_rule_missing" => "TUN DNS polkit 规则缺失 — 请先运行一次 TUN 权限设置以避免系统弹窗",
        "home.mode" => "模式",
        "dialog.confirm_close" => "关闭活动连接？",
        "dialog.target" => "目标",
        "dialog.password.title" => "管理员权限",
        "dialog.password.prompt" => "密码: ",
        "dialog.password.hint" => "Enter = 确认 | Esc = 取消",
        "dialog.trust" => "信任主机",
        "dialog.trust_title" => "导入被 SSRF 安全检查拦截",
        "dialog.trust_warning" => "该订阅解析到私有或回环地址。确认后仅对本配置文件信任此主机，以便后续导入与更新。",
        "dialog.trust_confirm" => "y = 信任并导入 | n/Esc = 取消（不保存信任）",
        "dialog.trust_update" => "信任并更新",
        "dialog.trust_update_title" => "更新被 SSRF 安全检查拦截",
        "dialog.trust_update_warning" => {
            "该订阅解析到私有或回环地址。确认后仅对本配置文件信任此主机，并在更新时放行该主机。"
        }
        "dialog.trust_update_confirm" => "y = 信任并更新 | n/Esc = 取消（更新保持失败）",
        "dialog.tun_setup" => "TUN 权限设置",
        "dialog.tun_setup_title" => "启动 TUN 需要一次性权限设置",
        "dialog.tun_setup_warning" => {
            "启用 TUN 启动需要 mihomo 文件能力与 systemd-resolved DNS polkit 规则。现在安装它们可让核心启动不再弹出系统对话框。"
        }
        "dialog.tun_setup_confirm" => "y = 立即设置 | n/Esc/q = 不设置直接启动",
        "dialog.tun_setup_confirm_hard" => "y = 立即设置 | n/Esc/q = 取消启动（需先完成 TUN 设置）",
        "settings.tun_capability_missing" => "TUN 文件权限缺失",
        "settings.service" => "系统服务",
        "settings.service_status_running" => "已安装 · 已启用 · 运行中",
        "settings.service_status_enabled_stopped" => "已安装 · 已启用 · 已停止",
        "settings.service_status_running_disabled" => "已安装 · 运行中 · 未启用",
        "settings.service_status_installed_disabled" => "已安装 · 未启用 · 已停止",
        "settings.service_status_not_installed" => "未安装",
        "settings.service_install_prompt" => "输入密码以安装 clash-verge-cli 系统服务（开机时运行内核）",
        "settings.service_uninstall_prompt" => "输入密码以卸载 clash-verge-cli 系统服务",
        "settings.service_installed" => "服务已安装并启动",
        "settings.service_uninstalled" => "服务已卸载",
        "settings.service_failed" => "服务操作失败",
        "settings.service_cancelled" => "服务操作已取消",
        "settings.service_uninstall_cancelled" => "服务卸载已取消",
        "dialog.service_uninstall" => "卸载服务",
        "dialog.service_uninstall_title" => "移除系统服务？",
        "dialog.service_uninstall_warning" => "这将移除 clash-verge-cli systemd 服务。内核将不再于开机时自动启动。",
        "dialog.service_uninstall_confirm" => "y = 卸载 | n/Esc/q = 取消",
        "dialog.service_uninstall_hint" => "卸载系统服务？y = 卸载 | n/Esc/q = 取消",
        "settings.auto_launch" => "登录时启动",
        "settings.auto_launch_on_msg" => "已启用开机自启 — 登录时启动内核",
        "settings.auto_launch_off_msg" => "已禁用开机自启",
        "settings.auto_launch_failed" => "无法更改开机自启",
        "settings.autostart_conflicts_service" => "请先禁用系统服务 — 「登录时启动」与系统服务互斥",
        "settings.service_conflicts_autostart" => "请先关闭「登录时启动」— 系统服务与登录自启互斥",
        "settings.sudo_hint" => "TUN 权限设置与服务安装/卸载需要 sudo；启动/开关不会弹出密码。",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Language, tr};

    #[test]
    fn config_language_maps_to_the_available_tui_locale() {
        assert_eq!(Language::from_config(Some("zh-CN")), Language::SimplifiedChinese);
        assert_eq!(Language::from_config(Some("en")), Language::English);
        assert_eq!(Language::from_config(Some("jp")), Language::English);
    }

    #[test]
    fn translations_use_english_as_the_complete_fallback() {
        assert_eq!(tr(Language::SimplifiedChinese, "view.settings"), "设置");
        assert_eq!(tr(Language::English, "view.settings"), "Settings");
        assert_eq!(tr(Language::SimplifiedChinese, "missing.key"), "missing.key");
    }

    #[test]
    fn tun_status_keys_are_present_in_both_locales() {
        // The TUN status line on the Home panel must be readable in both
        // English and Simplified Chinese. The NeedsSetup variant is the
        // case where the GUI says TUN is on but the binary lacks the
        // capability — the silent-TUN-dead scenario must not regress to
        // the English fallback in the zh-CN build.
        for key in [
            "home.tun_on",
            "home.tun_off",
            "home.tun_needs_setup",
            "home.tun_pending",
        ] {
            let en = tr(Language::English, key);
            let zh = tr(Language::SimplifiedChinese, key);
            assert_ne!(en, key, "english `{key}` must be translated");
            assert_ne!(
                zh, en,
                "zh-CN `{key}` must differ from English (no Chinese translation)"
            );
            assert_ne!(zh, key, "zh-CN `{key}` must be translated");
        }
    }
}
