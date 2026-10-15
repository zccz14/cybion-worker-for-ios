use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::platform;
use crate::tools::{required_string, valid_url};

/// The iOS Worker offers only the actions a sandboxed app can actually
/// perform. Everything else fails with an explicit error instead of a
/// silent no-op, matching how the other Workers report unsupported tools.
pub async fn run(arguments: &Value) -> Result<Value> {
    let action = required_string(arguments, "action")?;
    match action {
        "open" => {
            let url = valid_url(required_string(arguments, "text")?)?;
            let ok = bridge(move || platform::open_url(&url)).await?;
            ensure!(ok, "could not open the requested URL");
            Ok(json!({"ok": true}))
        }
        "click" | "long_press" | "swipe" | "type" | "key" | "ui_tree" | "launch" => {
            bail!(
                "{action} is not supported on iOS Workers; iOS exposes no way for an app to drive other apps"
            )
        }
        "screenshot" => {
            bail!(
                "screenshot is not supported on iOS Workers; a sandboxed app cannot capture the screen"
            )
        }
        "move" => bail!("move is not supported on iOS Workers"),
        unknown => bail!("unsupported computer action: {unknown}"),
    }
}

async fn bridge<T: Send + 'static>(task: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(task)
        .await
        .context("platform bridge task panicked")?
}
