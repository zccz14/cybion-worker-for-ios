//! Remote upgrades are not possible on iOS: a sandboxed app cannot install a
//! new build of itself, and no such mechanism exists outside MDM. The Worker
//! answers every queued upgrade with a single `failed` report that explains
//! the limitation, so the console records the outcome instead of waiting.
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::watch;

use crate::config::WorkerConfig;
use crate::delivery;
use crate::protocol;

pub const REASON: &str = "iOS Workers cannot upgrade themselves; build and install the new version (Xcode or a sideload tool), then remove the queued upgrade in the console";

/// The Controller repeats the `upgrade` event until the status changes.
/// Report each upgrade id at most once per worker process.
static REPORTED: Mutex<Option<String>> = Mutex::new(None);

/// Forgets the last reported upgrade when the engine stops, so a fresh session
/// can answer a still-queued upgrade again.
pub fn reset() {
    *REPORTED.lock().expect("upgrade lock poisoned") = None;
}

pub fn spawn(
    client: reqwest::Client,
    config: WorkerConfig,
    selector: Arc<crate::candidates::Selector>,
    boot_id: String,
    upgrade: protocol::Upgrade,
    _shutdown: watch::Receiver<bool>,
) {
    if upgrade.boot_id != boot_id {
        log::warn!(
            target: "cybion_worker",
            "ignoring an upgrade addressed to another Worker process"
        );
        return;
    }
    {
        let mut reported = REPORTED.lock().expect("upgrade lock poisoned");
        if reported.as_deref() == Some(upgrade.id.as_str()) {
            return;
        }
        *reported = Some(upgrade.id.clone());
    }
    log::info!(
        target: "cybion_worker",
        "upgrade to {} requested; reporting it as unsupported",
        upgrade.version
    );
    tokio::spawn(async move {
        let payload = json!({"id": upgrade.id, "status": "failed", "error": REASON});
        if let Err(error) = delivery::post_until_confirmed(
            &client, &config, &selector, "/upgrade", &boot_id, &payload,
        )
        .await
        {
            log::warn!(
                target: "cybion_worker",
                "upgrade failure report rejected for {}: {error:#}",
                upgrade.id
            );
            // Let a repeated event retry the report instead of staying silent.
            let mut reported = REPORTED.lock().expect("upgrade lock poisoned");
            if reported.as_deref() == Some(upgrade.id.as_str()) {
                *reported = None;
            }
        }
    });
}
