use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::platform;
use crate::tools::{command_output, required_string};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn run(arguments: &Value, cancel: watch::Receiver<bool>) -> Result<Value> {
    let command = required_string(arguments, "command")?;
    let timeout = timeout_of(arguments)?;
    let environment = platform::shell_environment()
        .context("the bundled shell is not available; reinstall the app")?;
    let mut process = Command::new(&environment.shell);
    process
        .arg("-c")
        .arg(command)
        .current_dir(&environment.working_dir)
        .env("PATH", &environment.path)
        .env("HOME", &environment.working_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = process.spawn().context("could not start the shell")?;
    let pid = child.id().context("the shell has no process id")? as i32;
    let mut stdout_pipe = child.stdout.take().context("command stdout is not piped")?;
    let mut stderr_pipe = child.stderr.take().context("command stderr is not piped")?;
    let stdout = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buffer).await;
        buffer
    });
    let stderr = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer).await;
        buffer
    });
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = tokio::time::sleep(timeout) => {
            kill_group(pid);
            let _ = child.wait().await;
            bail!("Bash command timed out");
        }
        _ = cancelled(cancel) => {
            kill_group(pid);
            let _ = child.wait().await;
            bail!("Bash command cancelled by the Controller");
        }
    };
    let stdout = stdout.await.unwrap_or_default();
    let stderr = stderr.await.unwrap_or_default();
    Ok(command_output(status.code(), &stdout, &stderr))
}

fn kill_group(pid: i32) {
    // SAFETY: `kill` with a negative pid signals the process group; the group
    // was created for the child above, so this only affects that tree.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
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
