use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Serialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Phase {
    Stopped,
    Starting,
    Pairing {
        user_code: String,
        url: String,
        expires_at: u64,
    },
    Connecting,
    Online {
        boot_id: String,
        since: u64,
    },
    Reconnecting {
        attempt: u32,
        last_error: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub running: bool,
    pub phase: Phase,
    pub version: String,
    pub hostname: String,
    pub controller_url: String,
    pub machine_id: Option<String>,
    pub user_id: Option<String>,
    pub connected_at: Option<u64>,
    pub boot_id: Option<String>,
    pub last_error: Option<String>,
}

pub type SharedState = Arc<Mutex<Snapshot>>;

impl Snapshot {
    pub fn stopped(version: String, hostname: String) -> Self {
        Self {
            running: false,
            phase: Phase::Stopped,
            version,
            hostname,
            controller_url: String::new(),
            machine_id: None,
            user_id: None,
            connected_at: None,
            boot_id: None,
            last_error: None,
        }
    }

    pub fn starting(version: String, hostname: String, controller_url: String) -> Self {
        Self {
            running: true,
            phase: Phase::Starting,
            controller_url,
            ..Self::stopped(version, hostname)
        }
    }
}

pub fn update(state: &SharedState, change: impl FnOnce(&mut Snapshot)) {
    change(&mut state.lock().expect("worker state lock poisoned"));
}

pub fn set_phase(state: &SharedState, phase: Phase) {
    update(state, |snapshot| {
        match &phase {
            Phase::Online { boot_id, since } => {
                snapshot.connected_at = Some(*since);
                snapshot.boot_id = Some(boot_id.clone());
            }
            Phase::Failed { message } => snapshot.last_error = Some(message.clone()),
            _ => {}
        }
        snapshot.phase = phase;
    });
}

pub fn fail(state: &SharedState, message: String) {
    update(state, |snapshot| {
        snapshot.last_error = Some(message.clone())
    });
    set_phase(state, Phase::Failed { message });
}
