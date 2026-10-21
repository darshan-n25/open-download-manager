//! Per-server connection ceiling.
//!
//! IDM exposes two independent concurrency knobs: `Max. connections number` (per file) and
//! `Max. connections number for a server` (per host, across every job). The second one is
//! what stops four parallel downloads from one CDN from opening thirty-two sockets to it and
//! getting the client throttled or banned.
//!
//! Open Download Manager enforces it at the point where a request is actually in flight: a caller holds a
//! permit for the duration of one response, then releases it. Permits are shared through an
//! `Arc`, so the ceiling is global across the whole process rather than per job.
//!
//! The limit is deliberately expressed as *simultaneous connections to a host*, not as a
//! counter of jobs, so a job with a single long transfer does not consume the whole budget
//! for that host while a second job can still make progress.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Extract the host (with port) used as the grouping key for the ceiling.
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_else(|| url.to_string())
}

#[derive(Clone, Debug)]
pub struct ServerLimits {
    semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    max: usize,
}

impl ServerLimits {
    /// `max == 0` means no limit.
    pub fn new(max: usize) -> Self {
        ServerLimits {
            semaphores: Arc::new(Mutex::new(HashMap::new())),
            max,
        }
    }

    pub fn unlimited() -> Self {
        ServerLimits::new(0)
    }

    pub fn max(&self) -> usize {
        self.max
    }

    pub fn is_unlimited(&self) -> bool {
        self.max == 0
    }

    /// Reserve one of this host's slots. The permit is held for as long as the caller wants
    /// the connection counted, and released when dropped.
    ///
    /// Returns `None` when the ceiling is disabled, so the common case costs nothing.
    pub async fn acquire(&self, url: &str) -> Option<OwnedSemaphorePermit> {
        if self.max == 0 {
            return None;
        }
        let host = host_of(url);
        let semaphore = {
            let mut map = self.semaphores.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(host)
                .or_insert_with(|| Arc::new(Semaphore::new(self.max)))
                .clone()
        };
        // The semaphore is never closed, so a failure here only happens if the runtime is
        // shutting down; treat that as "proceed without a permit" rather than failing the
        // transfer, since the request itself will report the real error.
        semaphore.acquire_owned().await.ok()
    }

    /// How many slots this host currently has free. Used by the daemon status reply.
    pub fn available(&self, url: &str) -> usize {
        if self.max == 0 {
            return usize::MAX;
        }
        let host = host_of(url);
        let map = self.semaphores.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&host) {
            Some(s) => s.available_permits(),
            None => self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_key_ignores_path_and_port_defaults() {
        assert_eq!(
            host_of("https://CDN.Example.com/a/b.bin?x=1"),
            "cdn.example.com"
        );
        assert_eq!(host_of("http://127.0.0.1:8080/x"), "127.0.0.1");
        // Unparseable input degrades to the whole string rather than panicking.
        assert_eq!(host_of("not a url"), "not a url");
    }

    #[tokio::test]
    async fn an_unlimited_ceiling_hands_out_no_permits() {
        let limits = ServerLimits::unlimited();
        assert!(limits.is_unlimited());
        assert!(limits.acquire("https://h/a").await.is_none());
        assert_eq!(limits.available("https://h/a"), usize::MAX);
    }

    #[tokio::test]
    async fn the_ceiling_is_enforced_per_host() {
        let limits = ServerLimits::new(2);
        let a = limits.acquire("https://one.test/x").await.unwrap();
        let b = limits.acquire("https://one.test/y").await.unwrap();
        assert_eq!(limits.available("https://one.test/z"), 0);

        // A different host has its own budget.
        let other = limits.acquire("https://two.test/z").await.unwrap();
        assert_eq!(limits.available("https://two.test/z"), 1);

        drop(a);
        assert_eq!(limits.available("https://one.test/z"), 1);
        drop(b);
        drop(other);
        assert_eq!(limits.available("https://one.test/z"), 2);
    }

    #[tokio::test]
    async fn a_third_caller_waits_rather_than_proceeding() {
        let limits = ServerLimits::new(1);
        let held = limits.acquire("https://h/x").await.unwrap();

        let waiting = tokio::spawn({
            let limits = limits.clone();
            async move {
                let _permit = limits.acquire("https://h/y").await;
                true
            }
        });

        // Give the waiter a chance to block.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "the waiter should be blocked");

        drop(held);
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("the waiter should be released")
            .unwrap();
        assert!(got);
    }

    #[tokio::test]
    async fn concurrent_tasks_never_exceed_the_ceiling() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limits = ServerLimits::new(3);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..24 {
            let limits = limits.clone();
            let in_flight = in_flight.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = limits.acquire("https://busy.test/x").await;
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "saw {} simultaneous connections, ceiling was 3",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(limits.available("https://busy.test/x"), 3);
    }
}
