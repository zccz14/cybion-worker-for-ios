use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, ensure};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::protocol::ToolCall;

pub const BOOT_HEADER: &str = "x-cybion-worker-boot-id";

pub struct DeliveryState {
    pub boot_id: String,
    seen: Mutex<HashMap<String, String>>,
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl DeliveryState {
    pub fn new() -> Self {
        Self {
            boot_id: uuid::Uuid::new_v4().to_string(),
            seen: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
        }
    }

    /// Remembers a call ID so replayed deliveries are never executed twice.
    /// The fingerprint proves a repeated ID carries the same arguments.
    pub fn admit(&self, call: &ToolCall) -> Result<bool> {
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(
            &json!({"thread_id": call.thread_id, "name": call.name, "arguments": call.arguments}),
        )?));
        let mut seen = self.seen.lock().expect("delivery lock poisoned");
        if let Some(existing) = seen.get(&call.id) {
            ensure!(
                existing == &fingerprint,
                "repeated call ID has different arguments"
            );
            return Ok(false);
        }
        seen.insert(call.id.clone(), fingerprint);
        Ok(true)
    }

    pub fn watch_cancel(&self, call_id: &str) -> watch::Receiver<bool> {
        let (sender, receiver) = watch::channel(false);
        self.cancels
            .lock()
            .expect("delivery lock poisoned")
            .insert(call_id.to_owned(), sender);
        receiver
    }

    pub fn cancel(&self, call_id: &str) {
        if let Some(sender) = self
            .cancels
            .lock()
            .expect("delivery lock poisoned")
            .get(call_id)
        {
            let _ = sender.send(true);
        }
    }

    pub fn finish_cancel(&self, call_id: &str) {
        self.cancels
            .lock()
            .expect("delivery lock poisoned")
            .remove(call_id);
    }
}

pub fn frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    (0..bytes.len()).find_map(|index| {
        if bytes[index..].starts_with(b"\n\n") {
            Some((index, 2))
        } else if bytes[index..].starts_with(b"\r\n\r\n") {
            Some((index, 4))
        } else {
            None
        }
    })
}

pub fn retry_delay(attempt: u32) -> Duration {
    let base = (1_u64 << attempt.saturating_sub(1).min(5)).min(30) * 1000;
    let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) * base / 1024;
    Duration::from_millis((base + jitter).min(30_000))
}

pub fn retryable(error: &anyhow::Error) -> bool {
    match error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
    {
        Some(status) => {
            status.is_server_error() || status.as_u16() == 408 || status.as_u16() == 429
        }
        None => true,
    }
}

pub async fn post_until_confirmed(
    client: &Client,
    access_token: &str,
    boot_id: &str,
    url: &str,
    payload: &Value,
) -> Result<()> {
    let mut failures = 0;
    loop {
        let response = client
            .post(url)
            .bearer_auth(access_token)
            .header(BOOT_HEADER, boot_id)
            .timeout(Duration::from_secs(30))
            .json(payload)
            .send()
            .await;
        let retry_after = response
            .as_ref()
            .ok()
            .and_then(|response| response.headers().get("retry-after"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let result = response
            .and_then(reqwest::Response::error_for_status)
            .map(|_| ())
            .map_err(anyhow::Error::from);
        match result {
            Ok(()) => return Ok(()),
            Err(error) if retryable(&error) => {
                failures += 1;
                let delay = retry_after.unwrap_or_else(|| retry_delay(failures));
                log::warn!(
                    target: "cybion_worker",
                    "retrying post url={url} attempt={failures} delay_ms={}",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplication_survives_completion() {
        let state = DeliveryState::new();
        let call = ToolCall {
            id: "call".into(),
            thread_id: "thread".into(),
            name: "bash".into(),
            arguments: json!({"command": "echo hello"}),
        };
        assert!(state.admit(&call).unwrap());
        assert!(!state.admit(&call).unwrap());
        assert!(
            state
                .admit(&ToolCall {
                    arguments: json!({"command": "different"}),
                    ..call
                })
                .is_err()
        );
    }

    #[test]
    fn frame_boundaries_and_backoff_are_bounded() {
        assert_eq!(frame_end(b"data: a\r\n\r\nrest"), Some((7, 4)));
        assert_eq!(frame_end(b"data: a\n\nrest"), Some((7, 2)));
        assert_eq!(frame_end(b"partial"), None);
        for attempt in 0..100 {
            assert!(retry_delay(attempt) <= Duration::from_secs(30));
        }
    }

    #[tokio::test]
    async fn cancel_signal_reaches_the_receiver() {
        let state = DeliveryState::new();
        let mut receiver = state.watch_cancel("call");
        assert!(!*receiver.borrow());
        state.cancel("call");
        receiver.changed().await.unwrap();
        assert!(*receiver.borrow());
        state.finish_cancel("call");
    }
}
