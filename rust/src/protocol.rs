use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::WorkerConfig;

#[derive(Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub thread_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Deserialize)]
pub struct Upgrade {
    pub id: String,
    pub version: String,
    pub boot_id: String,
}

pub fn worker_url(config: &WorkerConfig) -> String {
    format!(
        "{}/worker/v1/users/{}/workers/{}",
        config.controller_url, config.user_id, config.machine_id
    )
}

pub fn call_category(name: &str) -> &'static str {
    if name == "diagnostics" {
        "checks"
    } else {
        "calls"
    }
}

fn sse_data_of(event: &str, wanted: &str) -> Option<String> {
    let mut name = String::new();
    let mut data = String::new();
    for line in event.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.trim_start());
        }
    }
    (name == wanted).then_some(data)
}

pub fn parse_sse_call(event: &str) -> Result<Option<ToolCall>> {
    let Some(data) = sse_data_of(event, "tool_call") else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_str(&data).context("Worker received malformed tool_call")?,
    ))
}

pub fn parse_sse_cancel(event: &str) -> Result<Option<String>> {
    let Some(data) = sse_data_of(event, "cancel") else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&data).context("Worker received malformed cancel")?;
    Ok(Some(
        payload
            .get("id")
            .and_then(Value::as_str)
            .context("cancel event without a call id")?
            .to_owned(),
    ))
}

pub fn parse_sse_upgrade(event: &str) -> Result<Option<Upgrade>> {
    let Some(data) = sse_data_of(event, "upgrade") else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_str(&data).context("Worker received malformed upgrade")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_calls_and_ignores_other_events() {
        assert!(
            parse_sse_call("event: heartbeat\ndata: {}\n")
                .unwrap()
                .is_none()
        );
        let call = parse_sse_call(
            "event: tool_call\ndata: {\"id\":\"call\",\"thread_id\":\"thread\",\"name\":\"bash\",\"arguments\":{\"command\":\"pwd\"}}\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(call.name, "bash");
    }

    #[test]
    fn parses_cancel_events() {
        assert_eq!(
            parse_sse_cancel("event: cancel\ndata: {\"id\":\"call-1\"}\n")
                .unwrap()
                .unwrap(),
            "call-1"
        );
        assert!(parse_sse_cancel("event: cancel\ndata: {}\n").is_err());
    }

    #[test]
    fn builds_the_worker_protocol_path() {
        let config = WorkerConfig {
            controller_url: "https://cybion.ntnl.io".to_owned(),
            user_id: "auth-user".to_owned(),
            machine_id: "worker".to_owned(),
            access_token: "token".to_owned(),
        };
        assert_eq!(
            worker_url(&config),
            "https://cybion.ntnl.io/worker/v1/users/auth-user/workers/worker"
        );
    }
}
