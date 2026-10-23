use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::candidates;
use crate::config;
use crate::delivery::{self, DeliveryState};
use crate::protocol;
use crate::resources::ResourceSampler;
use crate::state::{self, Phase, SharedState, Snapshot, now_secs};
use crate::tools;

// Switch entries after this many consecutive failed reconnects, and re-check
// which entry is best this often while the task channel is healthy.
const FAILURES_BEFORE_SWITCH: u32 = 3;
const REPROBE_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Deserialize)]
pub struct DeviceInfo {
    pub hostname: String,
    pub platform: String,
    pub version: String,
}

static DEVICE: OnceLock<DeviceInfo> = OnceLock::new();
/// Working directory and `HOME` for `bash` calls: the app container folder
/// that also holds `worker.toml` and `worker.log`.
static SHELL_HOME: OnceLock<PathBuf> = OnceLock::new();
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

struct Engine {
    shutdown: watch::Sender<bool>,
    handle: Option<thread::JoinHandle<()>>,
    state: SharedState,
}

pub fn version() -> String {
    DEVICE
        .get()
        .map(|device| device.version.clone())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

pub(crate) fn hostname() -> String {
    DEVICE
        .get()
        .map(|device| device.hostname.clone())
        .unwrap_or_default()
}

pub(crate) fn shell_home() -> Option<PathBuf> {
    SHELL_HOME.get().cloned()
}

pub fn snapshot() -> Snapshot {
    match ENGINE.lock().expect("engine lock poisoned").as_ref() {
        Some(engine) => engine
            .state
            .lock()
            .expect("worker state lock poisoned")
            .clone(),
        None => Snapshot::stopped(version(), hostname()),
    }
}

pub fn start(config_dir: PathBuf, device: DeviceInfo) -> Result<()> {
    let mut engine = ENGINE.lock().expect("engine lock poisoned");
    if let Some(existing) = engine.as_ref()
        && existing
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    {
        return Ok(());
    }
    let _ = DEVICE.set(device.clone());
    let _ = SHELL_HOME.set(config_dir.clone());
    let state: SharedState = Arc::new(Mutex::new(Snapshot::starting(
        device.version.clone(),
        device.hostname.clone(),
        candidates::default_candidates()
            .ok()
            .and_then(|entries| entries.first().map(|entry| entry.base().to_owned()))
            .unwrap_or_default(),
    )));
    let (shutdown, receiver) = watch::channel(false);
    let thread_state = state.clone();
    let handle = thread::Builder::new()
        .name("cybion-worker".to_owned())
        .spawn(move || {
            let shutdown_probe = receiver.clone();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    if let Err(error) = runtime.block_on(run_worker(
                        config_dir,
                        device,
                        thread_state.clone(),
                        receiver,
                    )) && !*shutdown_probe.borrow()
                    {
                        state::fail(&thread_state, error.to_string());
                    }
                }
                Err(error) => state::fail(
                    &thread_state,
                    format!("could not start the async runtime: {error}"),
                ),
            }
            state::update(&thread_state, |snapshot| {
                snapshot.running = false;
                if !matches!(snapshot.phase, Phase::Failed { .. }) {
                    snapshot.phase = Phase::Stopped;
                }
            });
        })?;
    *engine = Some(Engine {
        shutdown,
        handle: Some(handle),
        state,
    });
    Ok(())
}

pub fn stop() {
    let Some(mut engine) = ENGINE.lock().expect("engine lock poisoned").take() else {
        return;
    };
    let _ = engine.shutdown.send(true);
    crate::upgrade::reset();
    if let Some(handle) = engine.handle.take() {
        let (done_sender, done_receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = handle.join();
            let _ = done_sender.send(());
        });
        let _ = done_receiver.recv_timeout(Duration::from_secs(5));
    }
    state::update(&engine.state, |snapshot| {
        snapshot.running = false;
        if !matches!(snapshot.phase, Phase::Failed { .. }) {
            snapshot.phase = Phase::Stopped;
        }
    });
}

pub fn reset(config_dir: &Path) -> Result<()> {
    {
        let engine = ENGINE.lock().expect("engine lock poisoned");
        if let Some(existing) = engine.as_ref()
            && existing
                .handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        {
            bail!("Worker is still running; stop it first");
        }
    }
    let _ = std::fs::remove_file(config::config_path(config_dir));
    let _ = std::fs::remove_file(config::pairing_path(config_dir));
    Ok(())
}

pub(crate) async fn shutdown_requested(mut receiver: watch::Receiver<bool>) {
    if *receiver.borrow_and_update() {
        return;
    }
    loop {
        match receiver.changed().await {
            Ok(()) => {
                if *receiver.borrow_and_update() {
                    return;
                }
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

enum Outcome {
    Shutdown,
    Session(Result<()>),
}

async fn run_worker(
    config_dir: PathBuf,
    device: DeviceInfo,
    state: SharedState,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(format!("cybion-worker-ios/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    state::set_phase(&state, Phase::Starting);
    self_check().await;
    let config = match config::load_or_pair(&config_dir, &device, &state, shutdown.clone()).await {
        Ok(config) => config,
        Err(error) => {
            fail_or_stop(&state, &shutdown, error);
            return Ok(());
        }
    };
    let selector =
        match candidates::resolve_candidates(&config.controller_url, &config.controller_urls)
            .and_then(candidates::Selector::new)
        {
            Ok(selector) => selector,
            Err(error) => {
                fail_or_stop(&state, &shutdown, error);
                return Ok(());
            }
        };
    // Connect through whichever entry actually answers on this phone's network.
    candidates::choose(&client, &selector).await;
    state::update(&state, |snapshot| {
        snapshot.controller_url = selector.current().base().to_owned();
        snapshot.machine_id = Some(config.machine_id.clone());
        snapshot.user_id = Some(config.user_id.clone());
    });
    let delivery = Arc::new(DeliveryState::new());
    let heartbeat = tokio::spawn(heartbeat_loop(
        client.clone(),
        config.clone(),
        selector.clone(),
        delivery.boot_id.clone(),
        device.clone(),
        shutdown.clone(),
    ));
    // Phone networks change under a long-lived process; re-measure periodically.
    let rebalance = tokio::spawn({
        let client = client.clone();
        let selector = selector.clone();
        async move {
            loop {
                tokio::time::sleep(REPROBE_INTERVAL).await;
                candidates::rebalance(&client, &selector).await;
            }
        }
    });
    let mut attempt: u32 = 0;
    loop {
        if attempt == 0 {
            state::set_phase(&state, Phase::Connecting);
        }
        let outcome = tokio::select! {
            result = event_session(
                client.clone(),
                config.clone(),
                selector.clone(),
                delivery.clone(),
                state.clone(),
                shutdown.clone(),
            ) => Outcome::Session(result),
            _ = shutdown_requested(shutdown.clone()) => Outcome::Shutdown,
        };
        match outcome {
            Outcome::Shutdown => break,
            Outcome::Session(Ok(())) => attempt = 0,
            Outcome::Session(Err(error)) => {
                if !delivery::retryable(&error) {
                    fail_or_stop(
                        &state,
                        &shutdown,
                        error.context("Worker connection rejected"),
                    );
                    break;
                }
                attempt = attempt.saturating_add(1);
                log::warn!(target: "cybion_worker", "event stream ended: {error}; reconnecting");
                if attempt.is_multiple_of(FAILURES_BEFORE_SWITCH) && selector.len() > 1 {
                    let next = selector.advance();
                    log::warn!(
                        target: "cybion_worker",
                        "could not reconnect; failing over to controller entry {}",
                        next.base()
                    );
                }
                state::set_phase(
                    &state,
                    Phase::Reconnecting {
                        attempt,
                        last_error: error.to_string(),
                    },
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delivery::retry_delay(attempt)) => {}
            _ = shutdown_requested(shutdown.clone()) => break,
        }
    }
    heartbeat.abort();
    rebalance.abort();
    Ok(())
}

/// The in-process engine is what makes `bash` possible; validate it once per
/// engine start so worker.log explains a broken build immediately.
async fn self_check() {
    let (_cancel_sender, cancel) = watch::channel(false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        crate::bash::run(&json!({"command": "echo cybion-worker-check"}), cancel),
    )
    .await;
    let detail = match outcome {
        Ok(Ok(value))
            if value["exit_code"] == 0
                && value["stdout"]
                    .as_str()
                    .is_some_and(|stdout| stdout.trim() == "cybion-worker-check") =>
        {
            "passed"
        }
        Ok(Ok(value)) => {
            log::warn!(target: "cybion_worker", "shell self-check failed: {value}");
            return;
        }
        Ok(Err(error)) => {
            log::warn!(target: "cybion_worker", "shell self-check failed: {error:#}");
            return;
        }
        Err(_) => {
            log::warn!(target: "cybion_worker", "shell self-check timed out");
            return;
        }
    };
    log::info!(target: "cybion_worker", "shell self-check {detail}");
}

fn fail_or_stop(state: &SharedState, shutdown: &watch::Receiver<bool>, error: anyhow::Error) {
    if *shutdown.borrow() {
        state::set_phase(state, Phase::Stopped);
    } else {
        state::fail(state, error.to_string());
    }
}

async fn event_session(
    client: Client,
    config: config::WorkerConfig,
    selector: Arc<candidates::Selector>,
    delivery: Arc<DeliveryState>,
    state: SharedState,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let device = DEVICE.get().context("worker device info is missing")?;
    // A session keeps the entry it was opened through; a switch closes it and
    // the caller reconnects through the new entry.
    let base = protocol::worker_url(selector.current().base(), &config);
    let mut switch = selector.subscribe();
    let response = client
        .get(format!("{base}/events"))
        .header(delivery::BOOT_HEADER, &delivery.boot_id)
        .header("x-cybion-worker-version", device.version.as_str())
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", config.access_token),
        )
        .send()
        .await?
        .error_for_status()?;
    state::set_phase(
        &state,
        Phase::Online {
            boot_id: delivery.boot_id.clone(),
            since: now_secs(),
        },
    );
    log::info!(target: "cybion_worker", "event stream connected");
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            // A switch must take effect without waiting for the next chunk.
            switched = switch.changed() => {
                if switched.is_err() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                log::info!(
                    target: "cybion_worker",
                    "controller entry switched; reopening the task channel via {}",
                    selector.current().base()
                );
                return Ok(());
            }
            chunk = tokio::time::timeout(Duration::from_secs(45), stream.next()) => {
                match chunk {
                    Err(_) => bail!("Worker event stream idle timeout"),
                    Ok(None) => return Ok(()),
                    Ok(Some(chunk)) => chunk?,
                }
            }
        };
        buffer.extend_from_slice(&chunk);
        while let Some((separator, width)) = delivery::frame_end(&buffer) {
            let event =
                String::from_utf8(buffer[..separator].to_vec()).context("invalid event UTF-8")?;
            buffer.drain(..separator + width);
            if let Some(call) = protocol::parse_sse_call(&event)? {
                spawn_call(
                    client.clone(),
                    config.clone(),
                    selector.clone(),
                    delivery.clone(),
                    call,
                );
            } else if let Some(cancelled) = protocol::parse_sse_cancel(&event)? {
                delivery.cancel(&cancelled);
            } else if let Some(upgrade) = protocol::parse_sse_upgrade(&event)? {
                if upgrade.boot_id != delivery.boot_id {
                    log::warn!(
                        target: "cybion_worker",
                        "ignoring an upgrade addressed to another Worker process"
                    );
                } else {
                    log::info!(
                        target: "cybion_worker",
                        "upgrade to {} requested",
                        upgrade.version
                    );
                    crate::upgrade::spawn(
                        client.clone(),
                        config.clone(),
                        selector.clone(),
                        delivery.boot_id.clone(),
                        upgrade,
                        shutdown.clone(),
                    );
                }
            }
        }
    }
}

fn spawn_call(
    client: Client,
    config: config::WorkerConfig,
    selector: Arc<candidates::Selector>,
    delivery: Arc<DeliveryState>,
    call: protocol::ToolCall,
) {
    let receipt_path = format!(
        "/{}/{}/received",
        protocol::call_category(&call.name),
        call.id
    );
    {
        let (client, config, selector, boot) = (
            client.clone(),
            config.clone(),
            selector.clone(),
            delivery.boot_id.clone(),
        );
        let call_id = call.id.clone();
        tokio::spawn(async move {
            match delivery::post_until_confirmed(
                &client,
                &config,
                &selector,
                &receipt_path,
                &boot,
                &json!({}),
            )
            .await
            {
                Ok(()) => {
                    log::info!(target: "cybion_worker", "receipt submitted call_id={call_id}")
                }
                Err(error) => {
                    log::warn!(target: "cybion_worker", "receipt rejected call_id={call_id}: {error}")
                }
            }
        });
    }
    match delivery.admit(&call) {
        Ok(true) => {}
        Ok(false) => {
            log::info!(target: "cybion_worker", "duplicate call skipped call_id={}", call.id);
            return;
        }
        Err(error) => {
            log::warn!(target: "cybion_worker", "dropping call {}: {error}", call.id);
            return;
        }
    }
    let cancel = delivery.watch_cancel(&call.id);
    tokio::spawn(async move {
        let call_id = call.id.clone();
        log::info!(
            target: "cybion_worker",
            "executing tool call call_id={call_id} tool={}",
            call.name
        );
        let result = tools::execute_call(&call, cancel).await;
        let (failed, payload) = match result {
            Ok(value) => (false, value),
            Err(error) => (true, json!({"error": error.to_string()})),
        };
        let path = format!(
            "/{}/{}/result",
            protocol::call_category(&call.name),
            call.id
        );
        if let Err(error) = delivery::post_until_confirmed(
            &client,
            &config,
            &selector,
            &path,
            &delivery.boot_id,
            &json!({"result": payload, "failed": failed}),
        )
        .await
        {
            log::warn!(target: "cybion_worker", "result submission rejected call_id={call_id}: {error}");
        }
        delivery.finish_cancel(&call_id);
    });
}

async fn heartbeat_loop(
    client: Client,
    config: config::WorkerConfig,
    selector: Arc<candidates::Selector>,
    boot_id: String,
    device: DeviceInfo,
    shutdown: watch::Receiver<bool>,
) {
    let mut sampler = ResourceSampler::default();
    loop {
        if *shutdown.borrow() {
            return;
        }
        // Resolved every round, so liveness follows a switch within 10 seconds.
        let base = protocol::worker_url(selector.current().base(), &config);
        let _ = post_json(
            &client,
            &config.access_token,
            &boot_id,
            &format!("{base}/heartbeat"),
            &json!({"hostname": device.hostname, "version": device.version}),
        )
        .await
        .map_err(|error| log::warn!(target: "cybion_worker", "heartbeat failed: {error}"));
        let resources = sampler.sample().await;
        let _ = post_json(
            &client,
            &config.access_token,
            &boot_id,
            &format!("{base}/resources"),
            &resources,
        )
        .await
        .map_err(|error| log::warn!(target: "cybion_worker", "resource report failed: {error}"));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = shutdown_requested(shutdown.clone()) => return,
        }
    }
}

async fn post_json(
    client: &Client,
    access_token: &str,
    boot_id: &str,
    url: &str,
    payload: &Value,
) -> Result<()> {
    client
        .post(url)
        .bearer_auth(access_token)
        .header(delivery::BOOT_HEADER, boot_id)
        .timeout(Duration::from_secs(15))
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
