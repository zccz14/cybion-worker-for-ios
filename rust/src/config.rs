use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub controller_url: String,
    pub user_id: String,
    pub machine_id: String,
    pub access_token: String,
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("worker.toml")
}

pub fn pairing_path(dir: &Path) -> PathBuf {
    dir.join("worker.pairing.json")
}

pub fn load(path: &Path) -> Result<WorkerConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let config: WorkerConfig = toml::from_str(&content)
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    let controller_url = config.controller_url.trim_end_matches('/').to_owned();
    let parsed = url::Url::parse(&controller_url).context("controller_url must be an HTTPS URL")?;
    ensure!(
        parsed.scheme() == "https" || parsed.host_str() == Some("localhost"),
        "controller_url must use HTTPS"
    );
    ensure!(!config.user_id.trim().is_empty(), "user_id is required");
    ensure!(
        config.user_id.len() <= 128
            && config
                .user_id
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.')),
        "user_id contains unsupported characters"
    );
    ensure!(
        !config.machine_id.trim().is_empty(),
        "machine_id is required"
    );
    ensure!(
        !config.access_token.trim().is_empty(),
        "access_token is required"
    );
    Ok(WorkerConfig {
        controller_url,
        ..config
    })
}

pub async fn load_or_pair(
    dir: &Path,
    device: &crate::engine::DeviceInfo,
    state: &crate::state::SharedState,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<WorkerConfig> {
    let path = config_path(dir);
    if path.try_exists()? {
        return load(&path);
    }
    crate::pairing::pair(dir, device, state, shutdown).await
}

pub fn save_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("config path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .with_context(|| format!("refusing to overwrite {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_no_clobber() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.toml");
        save_new(&path, b"original").unwrap();
        assert!(save_new(&path, b"replacement").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn rejects_insecure_controller_urls() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.toml");
        fs::write(
            &path,
            "controller_url = \"http://example.com\"\nuser_id = \"u\"\nmachine_id = \"m\"\naccess_token = \"t\"\n",
        )
        .unwrap();
        assert!(load(&path).is_err());
    }
}
