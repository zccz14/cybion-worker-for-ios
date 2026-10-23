//! Controller entry candidates and the runtime choice between them.
//!
//! The Controller answers on more than one entry domain. `.io` is the globally
//! distributed edge; `.top` reaches the same Controller through the mainland
//! entry, which is much faster there but is not reachable from everywhere and
//! can be reset by the carrier network. Which entry is better therefore depends
//! on where the phone runs and can change while the app is up, so the Worker
//! measures both and connects to whichever currently answers best.
//!
//! Probing uses the public `/health` endpoint: no credentials and no Controller
//! change are needed, and a probe never touches Worker state on the server.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use reqwest::Client;
use serde::Serialize;

/// Built-in entries in preference order, used until the device has paired.
pub const DEFAULT_CONTROLLER_URLS: &[&str] = &["https://cybion.ntnl.io", "https://cybion.ntnl.top"];

const PROBE_PATH: &str = "/health";
const PROBE_ATTEMPTS: usize = 3;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CANDIDATES: usize = 8;
/// A challenger must beat the entry in use by this much before it is adopted.
const SWITCH_MARGIN_PERCENT: f64 = 20.0;
/// …and the entry in use must have served this long, so switching cannot flap.
const MIN_DWELL: Duration = Duration::from_secs(60);

/// One Controller entry: a base URL with no path, query or trailing slash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    base: String,
}

impl Candidate {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim().trim_end_matches('/');
        ensure!(!trimmed.is_empty(), "controller entry is empty");
        let parsed =
            url::Url::parse(trimmed).with_context(|| format!("invalid controller entry {raw}"))?;
        // Plain HTTP is only tolerated for loopback, which is what the tests
        // serve; a real entry always has to be TLS.
        ensure!(
            parsed.scheme() == "https" || (parsed.scheme() == "http" && is_loopback(&parsed)),
            "controller entry must use HTTPS"
        );
        ensure!(
            parsed.path().is_empty() || parsed.path() == "/",
            "controller entry must not carry a path"
        );
        ensure!(
            parsed.query().is_none() && parsed.fragment().is_none(),
            "controller entry must not carry a query or fragment"
        );
        Ok(Self {
            base: trimmed.to_owned(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

fn is_loopback(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
    )
}

/// Resolves the configured entries, normalizing, de-duplicating and capping them.
pub fn resolve_candidates(
    controller_url: &str,
    controller_urls: &[String],
) -> Result<Vec<Candidate>> {
    let raw: Vec<&str> = if controller_urls.is_empty() {
        vec![controller_url]
    } else {
        controller_urls.iter().map(String::as_str).collect()
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    for entry in raw {
        let candidate = Candidate::parse(entry)?;
        if candidates.contains(&candidate) {
            continue;
        }
        ensure!(
            candidates.len() < MAX_CANDIDATES,
            "too many controller entries (max {MAX_CANDIDATES})"
        );
        candidates.push(candidate);
    }
    ensure!(!candidates.is_empty(), "no controller entry configured");
    Ok(candidates)
}

/// The built-in entry list for a device that has not paired yet.
pub fn default_candidates() -> Result<Vec<Candidate>> {
    let raw: Vec<String> = DEFAULT_CONTROLLER_URLS
        .iter()
        .map(|entry| (*entry).to_owned())
        .collect();
    resolve_candidates(DEFAULT_CONTROLLER_URLS[0], &raw)
}

/// What one entry looked like while probing.
#[derive(Clone, Debug, Serialize)]
pub struct Probe {
    pub candidate: Candidate,
    pub attempts: u32,
    pub successes: u32,
    pub median_ms: Option<u128>,
}

impl Probe {
    pub fn healthy(&self) -> bool {
        self.successes > 0
    }

    /// Median round trip, scaled up for missed samples so a flaky entry loses
    /// to a steady one with a similar median. `None` means nothing answered.
    pub fn score_ms(&self) -> Option<f64> {
        let median = self.median_ms?;
        Some(median as f64 * f64::from(self.attempts.max(1)) / f64::from(self.successes.max(1)))
    }
}

async fn probe(client: &Client, candidate: &Candidate) -> Probe {
    let mut latencies = Vec::new();
    let mut attempts = 0;
    for _ in 0..PROBE_ATTEMPTS {
        attempts += 1;
        let started = Instant::now();
        match client
            .get(format!("{}{PROBE_PATH}", candidate.base()))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                latencies.push(started.elapsed().as_millis());
            }
            Ok(response) => log::debug!(
                target: "cybion_worker",
                "controller probe {} answered {}",
                candidate.base(),
                response.status()
            ),
            Err(error) => log::debug!(
                target: "cybion_worker",
                "controller probe {} failed: {error}",
                candidate.base()
            ),
        }
    }
    latencies.sort_unstable();
    Probe {
        candidate: candidate.clone(),
        attempts,
        successes: latencies.len() as u32,
        median_ms: latencies.get(latencies.len() / 2).copied(),
    }
}

/// Probes every entry concurrently and returns them best-first, dead last.
pub async fn probe_all(client: &Client, candidates: &[Candidate]) -> Vec<Probe> {
    let mut probes =
        futures_util::future::join_all(candidates.iter().map(|entry| probe(client, entry))).await;
    probes.sort_by(|a, b| match (a.score_ms(), b.score_ms()) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    probes
}

fn log_probes(probes: &[Probe], current: usize, candidates: &[Candidate]) {
    for (rank, probe) in probes.iter().enumerate() {
        let index = candidates
            .iter()
            .position(|entry| entry == &probe.candidate)
            .unwrap_or(usize::MAX);
        match probe.score_ms() {
            Some(score) => log::info!(
                target: "cybion_worker",
                "controller entry {} rank={} current={} ok={}/{} score_ms={}",
                probe.candidate.base(),
                rank + 1,
                index == current,
                probe.successes,
                probe.attempts,
                score.round() as u64
            ),
            None => log::warn!(
                target: "cybion_worker",
                "controller entry {} did not answer the probe current={}",
                probe.candidate.base(),
                index == current
            ),
        }
    }
}

/// Startup: probe everything, report the ranking, adopt the best reachable one.
pub async fn choose(client: &Client, selector: &Arc<Selector>) -> Vec<Probe> {
    let probes = probe_all(client, selector.candidates()).await;
    log_probes(&probes, selector.current_index(), selector.candidates());
    match probes.iter().find(|probe| probe.healthy()) {
        Some(best) => {
            if let Some(index) = selector.index_of(best.candidate.base()) {
                selector.select(index);
            }
        }
        None => log::warn!(
            target: "cybion_worker",
            "no controller entry answered the probe; keeping the configured entry"
        ),
    }
    probes
}

/// Periodic re-check: move only when the entry in use is down, or clearly slow.
pub async fn rebalance(client: &Client, selector: &Arc<Selector>) -> bool {
    if selector.len() < 2 {
        return false;
    }
    let probes = probe_all(client, selector.candidates()).await;
    log_probes(&probes, selector.current_index(), selector.candidates());
    let Some(best) = probes.iter().find(|probe| probe.healthy()) else {
        return false;
    };
    let Some(best_index) = selector.index_of(best.candidate.base()) else {
        return false;
    };
    if best_index == selector.current_index() {
        return false;
    }
    let incumbent = probes
        .iter()
        .find(|probe| selector.index_of(probe.candidate.base()) == Some(selector.current_index()))
        .and_then(Probe::score_ms);
    let (from, worth_it) = match (incumbent, best.score_ms()) {
        // The entry in use is unreachable: any working entry is an improvement.
        (None, _) => (selector.current().base().to_owned(), true),
        (Some(_), None) => return false,
        (Some(incumbent), Some(best_score)) => (
            selector.current().base().to_owned(),
            best_score * 100.0 < incumbent * (100.0 - SWITCH_MARGIN_PERCENT),
        ),
    };
    if !worth_it {
        return false;
    }
    if incumbent.is_some() && selector.dwell() < MIN_DWELL {
        log::debug!(
            target: "cybion_worker",
            "{} measured better, but {} is still inside its dwell window",
            best.candidate.base(),
            from
        );
        return false;
    }
    selector.select(best_index);
    log::warn!(
        target: "cybion_worker",
        "switching controller entry from {from} to {}",
        best.candidate.base()
    );
    true
}

/// The single in-process source of truth for "which entry are we talking to".
#[derive(Debug)]
pub struct Selector {
    candidates: Vec<Candidate>,
    current: RwLock<usize>,
    switched_at: RwLock<Instant>,
    switch: tokio::sync::watch::Sender<u64>,
}

impl Selector {
    pub fn new(candidates: Vec<Candidate>) -> Result<Arc<Self>> {
        ensure!(
            !candidates.is_empty(),
            "at least one controller entry is required"
        );
        let (switch, _) = tokio::sync::watch::channel(0_u64);
        Ok(Arc::new(Self {
            candidates,
            current: RwLock::new(0),
            switched_at: RwLock::new(Instant::now()),
            switch,
        }))
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn current_index(&self) -> usize {
        *self.current.read().expect("selector index lock poisoned")
    }

    pub fn current(&self) -> Candidate {
        self.candidates[self.current_index()].clone()
    }

    pub fn index_of(&self, base: &str) -> Option<usize> {
        self.candidates
            .iter()
            .position(|entry| entry.base() == base)
    }

    /// Wakes the open event stream so it reconnects through the new entry.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.switch.subscribe()
    }

    fn announce(&self) {
        let next = *self.switch.borrow() + 1;
        let _ = self.switch.send(next);
    }

    fn mark_switched(&self) {
        *self
            .switched_at
            .write()
            .expect("selector switch lock poisoned") = Instant::now();
    }

    /// How long the entry in use has been serving.
    pub fn dwell(&self) -> Duration {
        self.switched_at
            .read()
            .expect("selector switch lock poisoned")
            .elapsed()
    }

    /// Adopts `index`; announces only when the entry actually changes.
    pub fn select(&self, index: usize) -> Candidate {
        if self.len() > 1 && index != self.current_index() {
            {
                let mut current = self.current.write().expect("selector index lock poisoned");
                *current = index % self.candidates.len();
            }
            self.mark_switched();
            self.announce();
        }
        self.current()
    }

    /// Moves to the next entry, wrapping around. A no-op with a single entry.
    pub fn advance(&self) -> Candidate {
        if self.len() > 1 {
            self.select((self.current_index() + 1) % self.candidates.len())
        } else {
            self.current()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal HTTP/1.1 responder, so the probe tests need no extra dependency.
    fn serve() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            }
        });
        format!("http://{address}")
    }

    fn entries(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|entry| (*entry).to_owned()).collect()
    }

    #[test]
    fn entries_are_normalized_and_validated() {
        assert_eq!(
            Candidate::parse("https://cybion.ntnl.top/").unwrap().base(),
            "https://cybion.ntnl.top"
        );
        assert!(Candidate::parse("http://cybion.ntnl.top").is_err());
        assert!(Candidate::parse("http://127.0.0.1:8080").is_ok());
        assert!(Candidate::parse("https://cybion.ntnl.io/base").is_err());
        assert!(Candidate::parse("").is_err());
    }

    #[test]
    fn resolve_keeps_order_deduplicates_and_falls_back() {
        let resolved = resolve_candidates(
            "https://cybion.ntnl.io",
            &entries(&[
                "https://cybion.ntnl.top/",
                "https://cybion.ntnl.io",
                "https://cybion.ntnl.top",
            ]),
        )
        .unwrap();
        assert_eq!(
            resolved.iter().map(Candidate::base).collect::<Vec<_>>(),
            vec!["https://cybion.ntnl.top", "https://cybion.ntnl.io"]
        );
        // A file written by an older app version has only the single field.
        let single = resolve_candidates("https://cybion.ntnl.io/", &[]).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].base(), "https://cybion.ntnl.io");
        assert_eq!(default_candidates().unwrap().len(), 2);
    }

    #[test]
    fn a_single_entry_never_announces_a_switch() {
        let selector = Selector::new(default_candidates().unwrap()[..1].to_vec()).unwrap();
        let switch = selector.subscribe();
        assert_eq!(selector.advance().base(), "https://cybion.ntnl.io");
        assert!(!switch.has_changed().unwrap());
    }

    #[test]
    fn advance_wraps_and_wakes_the_stream() {
        let selector = Selector::new(default_candidates().unwrap()).unwrap();
        let mut switch = selector.subscribe();
        assert_eq!(selector.advance().base(), "https://cybion.ntnl.top");
        assert!(switch.has_changed().unwrap());
        switch.borrow_and_update();
        assert_eq!(selector.advance().base(), "https://cybion.ntnl.io");
        assert!(switch.has_changed().unwrap());
    }

    #[tokio::test]
    async fn probing_skips_the_dead_entry_and_the_probe_is_believed() {
        let healthy_url = serve();
        // Port 1 is not served, so this entry fails to connect on every sample.
        let dead_url = "http://127.0.0.1:1".to_owned();
        let selector = Selector::new(
            resolve_candidates(
                &dead_url,
                &entries(&[dead_url.as_str(), healthy_url.as_str()]),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(selector.current_index(), 0);

        let probes = choose(&Client::new(), &selector).await;
        assert!(probes[0].healthy(), "the reachable entry must rank first");
        assert!(!probes[1].healthy());
        assert_eq!(selector.current().base(), healthy_url);
    }

    #[tokio::test]
    async fn rebalance_prefers_a_healthy_entry_over_a_dead_incumbent() {
        let healthy_url = serve();
        let dead_url = "http://127.0.0.1:1".to_owned();
        let selector = Selector::new(
            resolve_candidates(
                &dead_url,
                &entries(&[dead_url.as_str(), healthy_url.as_str()]),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(rebalance(&Client::new(), &selector).await);
        assert_eq!(selector.current().base(), healthy_url);
        // The incumbent is healthy now and the dwell window has not passed yet.
        assert!(!rebalance(&Client::new(), &selector).await);
    }
}
