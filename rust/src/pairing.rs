use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::config::{self, WorkerConfig};
use crate::engine::{DeviceInfo, shutdown_requested};
use crate::state::{self, Phase, SharedState, now_secs};

pub const CONTROLLER: &str = "https://cybion.ntnl.io";

#[derive(Serialize, Deserialize)]
struct Pending {
    id: String,
    user_code: String,
    expires_at: u64,
    device_secret: String,
    access_token: String,
}

#[derive(Deserialize)]
struct StartReply {
    id: String,
    user_code: String,
    expires_at: u64,
}

#[derive(Deserialize)]
struct PollReply {
    status: String,
    user_id: Option<String>,
    machine_id: String,
}

fn secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub async fn pair(
    dir: &Path,
    device: &DeviceInfo,
    state: &SharedState,
    shutdown: watch::Receiver<bool>,
) -> Result<WorkerConfig> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()?;
    let pending_path = config::pairing_path(dir);
    let pending = match load_pending(&client, &pending_path).await? {
        Some(pending) => pending,
        None => start_pairing(&client, &pending_path, device).await?,
    };
    state::set_phase(
        state,
        Phase::Pairing {
            user_code: pending.user_code.clone(),
            url: format!("{CONTROLLER}/#/workers?code={}", pending.user_code),
            expires_at: pending.expires_at,
        },
    );
    loop {
        if now_secs() >= pending.expires_at + 630 {
            bail!("Could not finish pairing within the recovery window. 配对未完成，请重试。");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            _ = shutdown_requested(shutdown.clone()) => bail!("stopped while pairing"),
        }
        let response = client
            .get(format!("{CONTROLLER}/worker/v1/pairings/{}", pending.id))
            .bearer_auth(&pending.device_secret)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_connect() || error.is_timeout() => continue,
            Err(error) => return Err(error.into()),
        };
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || response.status().is_server_error()
        {
            continue;
        }
        if response.status() == reqwest::StatusCode::GONE {
            let _ = fs::remove_file(&pending_path);
            bail!("Pairing expired. 配对已过期，请重新开启。");
        }
        let reply: PollReply = response.error_for_status()?.json().await?;
        match reply.status.as_str() {
            "pending" | "approving" => continue,
            "cancelled" => {
                let _ = fs::remove_file(&pending_path);
                bail!("Pairing cancelled. 配对已取消，请重新开启。");
            }
            "approved" => {
                let config = WorkerConfig {
                    controller_url: CONTROLLER.to_owned(),
                    user_id: reply.user_id.context("approval has no owner")?,
                    machine_id: reply.machine_id,
                    access_token: pending.access_token.clone(),
                };
                config::save_new(
                    &config::config_path(dir),
                    toml::to_string(&config)?.as_bytes(),
                )?;
                let _ = fs::remove_file(&pending_path);
                return Ok(config);
            }
            other => bail!("Unexpected pairing status: {other}"),
        }
    }
}

async fn load_pending(client: &Client, path: &Path) -> Result<Option<Pending>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let pending: Pending =
        serde_json::from_slice(&fs::read(path)?).context("Invalid pending pairing file")?;
    let response = client
        .get(format!("{CONTROLLER}/worker/v1/pairings/{}", pending.id))
        .bearer_auth(&pending.device_secret)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::GONE {
        fs::remove_file(path)?;
        return Ok(None);
    }
    response.error_for_status()?;
    Ok(Some(pending))
}

async fn start_pairing(client: &Client, path: &Path, device: &DeviceInfo) -> Result<Pending> {
    let device_secret = secret();
    let access_token = secret();
    let reply: StartReply = client
        .post(format!("{CONTROLLER}/worker/v1/pairings"))
        .json(&json!({
            "device_secret": device_secret,
            "token_hash": hex::encode(Sha256::digest(access_token.as_bytes())),
            "hostname": device.hostname,
            "platform": device.platform,
            "version": device.version,
        }))
        .send()
        .await
        .context("Could not contact Cybion. 无法连接 Cybion，请检查网络后重试。")?
        .error_for_status()?
        .json()
        .await?;
    let pending = Pending {
        id: reply.id,
        user_code: reply.user_code,
        expires_at: reply.expires_at,
        device_secret,
        access_token,
    };
    config::save_new(path, &serde_json::to_vec(&pending)?)?;
    Ok(pending)
}
