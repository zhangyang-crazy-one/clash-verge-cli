use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    let Some(pid) = manager.pid() else {
        // Still release a desktop proxy left pointing at a dead core.
        manager.stop().await?;
        if super::core_running(&manager.api()).await {
            anyhow::bail!(
                "a {} core answers on {} but has no clash-verge-cli pid record; \
stop it where it was started",
                manager.core_kind().as_str(),
                controller_description(&manager),
            );
        }
        println!("{} is not running", manager.core_kind().as_str());
        return Ok(());
    };
    manager.stop().await?;
    println!("{} stopped (pid {pid})", manager.core_kind().as_str());
    Ok(())
}

/// Where the running core is reachable. sing-box's clash_api is TCP-only,
/// mihomo listens on the unix socket (#54): naming the right endpoint is
/// what tells the user which controller answered.
pub(super) fn controller_description(manager: &MihomoManager) -> String {
    match manager.core_kind() {
        crate::mihomo_manager::CoreKind::Mihomo => {
            format!("unix socket {}", manager.socket_path().display())
        }
        crate::mihomo_manager::CoreKind::SingBox => {
            format!("tcp {}", manager.singbox_controller_addr())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_description_uses_the_selected_core_transport() {
        let manager = MihomoManager::new("/tmp/core-description".into())
            .with_socket("/tmp/core-description/controller.sock".into())
            .with_singbox_controller("127.0.0.1:19999".parse().unwrap());
        assert_eq!(
            controller_description(&manager),
            "unix socket /tmp/core-description/controller.sock"
        );
        let manager = manager.with_core_kind(crate::mihomo_manager::CoreKind::SingBox);
        assert_eq!(controller_description(&manager), "tcp 127.0.0.1:19999");
    }
}
