use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::watch;

use crate::engine;
use crate::shell::{self, ShellState};
use crate::tools::{command_output, required_string};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs one `bash` tool call through the in-process command engine.
///
/// iOS cannot `exec` anything from a sandboxed app, so there is no child
/// process: the engine runs inside the app process. Cancellation and the
/// timeout work by dropping the whole engine future, so nothing keeps
/// running afterwards.
pub async fn run(arguments: &Value, cancel: watch::Receiver<bool>) -> Result<Value> {
    let command = required_string(arguments, "command")?;
    let timeout = timeout_of(arguments)?;
    let home = engine::shell_home().context("the Worker has not been started")?;
    let mut state = ShellState::new(home, engine::hostname());
    let result = tokio::select! {
        result = shell::run(command, &mut state) => result,
        _ = tokio::time::sleep(timeout) => bail!("Bash command timed out"),
        _ = cancelled(cancel) => bail!("Bash command cancelled by the Controller"),
    };
    Ok(command_output(
        Some(result.exit_code),
        result.stdout.as_bytes(),
        result.stderr.as_bytes(),
    ))
}

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow_and_update() {
        return;
    }
    loop {
        match cancel.changed().await {
            Ok(()) => {
                if *cancel.borrow_and_update() {
                    return;
                }
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

fn timeout_of(arguments: &Value) -> Result<Duration> {
    let Some(value) = arguments.get("timeout_seconds") else {
        return Ok(DEFAULT_TIMEOUT);
    };
    let seconds = value
        .as_u64()
        .filter(|seconds| *seconds >= 1)
        .context("timeout_seconds must be a positive number of seconds")?;
    Ok(Duration::from_secs(seconds))
}
