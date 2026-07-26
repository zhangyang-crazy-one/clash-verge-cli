use anyhow::{Context as _, Result, anyhow, bail};
use nanoid::nanoid;
use serde::{Serialize, de::DeserializeOwned};
use serde_yaml_ng::{Mapping, Value};
use std::{path::PathBuf, str::FromStr};

/// read data from yaml as struct T
pub async fn read_yaml<T: DeserializeOwned>(path: &PathBuf) -> Result<T> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        bail!("file not found \"{}\"", path.display());
    }

    let yaml_str = tokio::fs::read_to_string(path).await?;

    Ok(serde_yaml_ng::from_str::<T>(&yaml_str)?)
}

/// read mapping from yaml
pub async fn read_mapping(path: &PathBuf) -> Result<Mapping> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        bail!("file not found \"{}\"", path.display());
    }

    let yaml_str = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read the file \"{}\"", path.display()))?;

    // YAML syntax check
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&yaml_str) {
        Ok(mut val) => {
            val.apply_merge()
                .with_context(|| format!("failed to apply merge \"{}\"", path.display()))?;

            match val {
                Value::Mapping(map) => Ok(map),
                _ => Err(anyhow!("failed to transform to yaml mapping \"{}\"", path.display())),
            }
        }
        Err(err) => {
            bail!("YAML syntax error: {}", err)
        }
    }
}

/// save the data to the file
/// can set `prefix` string to add some comments
pub async fn save_yaml<T: Serialize + Sync>(path: &PathBuf, data: &T, prefix: Option<&str>) -> Result<()> {
    let data_str = serde_yaml_ng::to_string(data)?;

    let yaml_str = match prefix {
        Some(prefix) => format!("{prefix}\n\n{data_str}"),
        None => data_str,
    };

    // Atomic replace avoids torn reads when another task reads mid-write.
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("config.yaml");
    let temporary_path = path.with_file_name(format!("{file_name}.tmp"));

    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        if tokio::fs::try_exists(path).await.unwrap_or(false) {
            tokio::fs::metadata(path).await.ok().map(|meta| meta.permissions())
        } else {
            // Prefer a restrictive default for new config/profile files.
            Some(std::fs::Permissions::from_mode(0o600))
        }
    };

    tokio::fs::write(&temporary_path, yaml_str.as_bytes())
        .await
        .with_context(|| format!("failed to stage file \"{}\"", temporary_path.display()))?;

    #[cfg(unix)]
    if let Some(permissions) = permissions {
        tokio::fs::set_permissions(&temporary_path, permissions)
            .await
            .with_context(|| format!("failed to set permissions on \"{}\"", temporary_path.display()))?;
    }

    replace_file(&temporary_path, path).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok(())
}

/// Replace `to` with `from`. On Unix this is an atomic rename; on Windows the
/// destination is removed first because `rename` cannot overwrite.
async fn replace_file(from: &PathBuf, to: &PathBuf) -> Result<()> {
    #[cfg(windows)]
    if tokio::fs::try_exists(to).await.unwrap_or(false) {
        tokio::fs::remove_file(to)
            .await
            .with_context(|| format!("failed to remove existing file \"{}\"", to.display()))?;
    }
    tokio::fs::rename(from, to)
        .await
        .with_context(|| format!("failed to save file \"{}\"", to.display()))
}

const ALPHABET: [char; 62] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm',
    'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J',
    'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// generate the uid
pub fn get_uid(prefix: &str) -> String {
    let id = nanoid!(11, &ALPHABET);
    format!("{prefix}{id}")
}

/// parse the string
/// xxx=123123; => 123123
pub fn parse_str<T: FromStr>(target: &str, key: &str) -> Option<T> {
    target.split(';').map(str::trim).find_map(|s| {
        let mut parts = s.splitn(2, '=');
        match (parts.next(), parts.next()) {
            (Some(k), Some(v)) if k == key => v.parse::<T>().ok(),
            _ => None,
        }
    })
}
