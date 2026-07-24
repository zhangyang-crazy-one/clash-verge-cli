use crate::mihomo_manager::binary::MihomoBinarySource;
use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    let resolved = manager.start().await?;
    let pid = manager.pid().unwrap_or(0);
    let source = match resolved.source {
        MihomoBinarySource::System => "system verge-mihomo",
        MihomoBinarySource::ManagedCached => "managed (already installed)",
        MihomoBinarySource::Downloaded => "managed (downloaded just now)",
    };
    println!("mihomo started");
    println!("  version: {}", resolved.version);
    println!("  source:  {source}");
    println!("  path:    {}", resolved.path.display());
    println!("  pid:     {pid}");
    Ok(())
}
