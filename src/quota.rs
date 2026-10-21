//! Download quotas over a time window — IDM's "Download limits".
//!
//! [`crate::throttle::RateLimiter`] caps bytes *per second*. A quota caps bytes *per
//! window*: "no more than 500 MB in any hour". IDM reports `Download limits exceeded!`
//! and stops; Open Download Manager stops too, but leaves the job **resumable** and records how long
//! until the budget refills so the queue can pick it up again.
//!
//! The window is a fixed span measured from the first byte charged — not a sliding
//! window, and not a wall-clock hour. That matches the intuition of "I have 500 MB of
//! budget, starting now", and it is the only version whose behaviour can be pinned down
//! in a test without waiting for a real clock boundary.
//!
//! A quota built with [`Quota::persistent`] remembers the window on disk, so restarting
//! the process does not quietly hand back a full budget. Enforcement is only as good as
//! the accounting, and an in-memory-only budget can be reset by anyone who can restart the
//! daemon.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Returned when a charge would take the window past its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaExhausted {
    /// Room left in the current window at the moment of refusal.
    pub remaining: u64,
    /// How long until the budget refills.
    pub resets_in: Duration,
}

impl std::fmt::Display for QuotaExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "download quota exhausted ({} left in this window, refills in {}s)",
            self.remaining,
            self.resets_in.as_secs().max(1)
        )
    }
}

impl std::error::Error for QuotaExhausted {}

/// Write the window out at most this often…
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
/// …and at least every this many bytes, whichever comes first.
const FLUSH_BYTES: u64 = 4 << 20;

/// The on-disk form of a window. Wall-clock, so it survives a restart.
#[derive(serde::Serialize, serde::Deserialize)]
struct Persisted {
    /// When the budget refills, as Unix epoch milliseconds.
    expires_ms: u64,
    used: u64,
    /// The window length in force when this was written, so a changed setting starts a
    /// fresh window instead of being applied to half-spent bytes.
    window_secs: u64,
}

struct Window {
    /// When the budget refills. Monotonic, for the in-process countdown.
    expires_at: Instant,
    /// The same moment in wall-clock milliseconds, for persistence.
    expires_ms: u64,
    used: u64,
    /// `used` as of the last successful write, so an unchanged window is not rewritten.
    flushed: u64,
    last_flush: Instant,
}

/// A byte budget that refills every `window`.
pub struct Quota {
    limit: Option<u64>,
    window: Duration,
    state: Mutex<Window>,
    /// Where to remember the window, if anywhere.
    save_path: Option<PathBuf>,
}

impl Quota {
    /// An in-memory budget. `None` or `Some(0)` means unlimited.
    pub fn new(limit: Option<u64>, window: Duration) -> Self {
        Self::build(limit, window, None)
    }

    /// A budget that remembers the window at `path`, so a restart resumes it rather than
    /// refilling it. A window whose expiry has passed, or one written with a different
    /// window length, starts fresh.
    pub fn persistent(limit: Option<u64>, window: Duration, path: impl Into<PathBuf>) -> Self {
        Self::build(limit, window, Some(path.into()))
    }

    fn build(limit: Option<u64>, window: Duration, save_path: Option<PathBuf>) -> Self {
        // A zero window would refill on every check and make the quota a no-op, so fall
        // back to an hour rather than silently disabling it.
        let window = if window.is_zero() {
            Duration::from_secs(3600)
        } else {
            window
        };
        let limit = limit.filter(|b| *b > 0);

        let now = Instant::now();
        let now_ms = crate::util::now_ms();
        let state = match save_path.as_deref().and_then(|p| load_window(p, window)) {
            Some((expires_ms, used)) => Window {
                // Only ever *adds* a positive span to `Instant::now()`. Subtracting a
                // persisted timestamp from a monotonic clock would be wrong after a
                // reboot (the monotonic epoch restarts), and can panic.
                expires_at: now + Duration::from_millis(expires_ms.saturating_sub(now_ms)),
                expires_ms,
                used,
                flushed: used,
                last_flush: now,
            },
            None => Window {
                expires_at: now + window,
                expires_ms: now_ms + window.as_millis() as u64,
                used: 0,
                flushed: 0,
                last_flush: now,
            },
        };
        Quota {
            limit,
            window,
            state: Mutex::new(state),
            save_path,
        }
    }

    pub fn unlimited() -> Self {
        Quota::new(None, Duration::from_secs(3600))
    }

    pub fn is_unlimited(&self) -> bool {
        self.limit.is_none()
    }

    pub fn limit_bytes(&self) -> Option<u64> {
        self.limit
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Window> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start a fresh window if the current one has elapsed.
    fn roll_if_expired(&self, w: &mut Window) {
        if !w
            .expires_at
            .saturating_duration_since(Instant::now())
            .is_zero()
        {
            return;
        }
        let now = Instant::now();
        w.expires_at = now + self.window;
        w.expires_ms = crate::util::now_ms() + self.window.as_millis() as u64;
        w.used = 0;
        w.flushed = 0;
        w.last_flush = now;
    }

    /// Run `f` against the current window, rolling it first if it has elapsed.
    fn with_window<T>(&self, f: impl FnOnce(&mut Window, u64) -> T) -> Option<T> {
        let limit = self.limit?;
        let mut w = self.lock();
        self.roll_if_expired(&mut w);
        Some(f(&mut w, limit))
    }

    /// Charge `bytes` against the window.
    ///
    /// The first charge of any window always succeeds, even if it is larger than the
    /// whole budget. Without that escape hatch a budget smaller than one read chunk —
    /// say 64 KB against 1 MB chunks — could never make progress and the job would pause
    /// forever, refilling to nothing every window.
    pub fn charge(&self, bytes: u64) -> Result<(), QuotaExhausted> {
        if bytes == 0 {
            return Ok(());
        }
        let outcome = self.with_window(|w, limit| {
            if w.used == 0 {
                w.used = bytes;
                return Ok(());
            }
            if w.used.saturating_add(bytes) > limit {
                return Err(QuotaExhausted {
                    remaining: limit.saturating_sub(w.used),
                    resets_in: w.expires_at.saturating_duration_since(Instant::now()),
                });
            }
            w.used += bytes;
            Ok(())
        });
        let outcome = match outcome {
            Some(o) => o,
            None => return Ok(()), // unlimited
        };
        // A refusal is the moment a small budget becomes spent, and it may be the last
        // thing that happens before the process dies — so write it out now, bypassing the
        // throttle. `write_locked` is a no-op once the value is already on disk, so the
        // repeated refusals of a stalled transfer cost nothing.
        self.maybe_flush(outcome.is_err());
        outcome
    }

    pub fn used(&self) -> u64 {
        self.with_window(|w, _| w.used).unwrap_or(0)
    }

    pub fn remaining(&self) -> Option<u64> {
        self.with_window(|w, limit| limit.saturating_sub(w.used))
    }

    /// How long until the budget refills.
    ///
    /// `None` when unlimited, and [`Duration::ZERO`] when the window has *already*
    /// elapsed — the next charge starts a fresh one. Deliberately does not roll the
    /// window itself: a rolling getter here would report a whole fresh window the moment
    /// the old one expired, so [`Quota::wait_for_reset`] would sleep in circles forever.
    pub fn resets_in(&self) -> Option<Duration> {
        self.limit?;
        let w = self.lock();
        Some(w.expires_at.saturating_duration_since(Instant::now()))
    }

    /// Sleep until the current window expires, then return. A no-op when unlimited.
    pub async fn wait_for_reset(&self) {
        if let Some(d) = self.resets_in() {
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        // Rolls the expired window lazily, so a caller that checks `remaining()` right
        // after this call sees a refilled budget rather than the old one.
        let _ = self.used();
    }

    /// Write the window out now, whatever the throttle says. Worth calling when a job
    /// finishes, so a process that is then killed does not lose the accounting.
    pub fn flush(&self) {
        let Some(path) = self.save_path.clone() else {
            return;
        };
        let mut w = self.lock();
        self.write_locked(&mut w, &path);
    }

    /// Persist the window if the throttle allows, or unconditionally when `force` is set.
    fn maybe_flush(&self, force: bool) {
        let Some(path) = self.save_path.clone() else {
            return;
        };
        let mut w = self.lock();
        if w.used == w.flushed {
            return;
        }
        if !force
            && w.used.saturating_sub(w.flushed) < FLUSH_BYTES
            && w.last_flush.elapsed() < FLUSH_INTERVAL
        {
            return;
        }
        self.write_locked(&mut w, &path);
    }

    fn write_locked(&self, w: &mut Window, path: &Path) {
        if w.used == w.flushed {
            return;
        }
        let p = Persisted {
            expires_ms: w.expires_ms,
            used: w.used,
            window_secs: self.window.as_secs(),
        };
        let Ok(data) = serde_json::to_vec(&p) else {
            return;
        };
        if crate::util::atomic_write(path, &data).is_ok() {
            w.flushed = w.used;
            w.last_flush = Instant::now();
        }
    }

    /// A read-only view of the budget for status output and the daemon API.
    pub fn snapshot(&self) -> QuotaSnapshot {
        QuotaSnapshot {
            limit: self.limit,
            used: self.used(),
            remaining: self.remaining(),
            resets_in_secs: self.resets_in().map(|d| d.as_secs()),
        }
    }

    /// One line for a terminal, e.g. `12.00 MB of 500.00 MB used, window refills in 34m`.
    pub fn describe(&self) -> String {
        let Some(limit) = self.limit else {
            return "no download quota".to_string();
        };
        let mut s = format!(
            "{} of {} used",
            crate::util::human_bytes(self.used()),
            crate::util::human_bytes(limit)
        );
        if let Some(d) = self.resets_in() {
            s.push_str(&format!(", window refills in {}", human_duration(d)));
        }
        s
    }
}

/// Read a persisted window, unless it has expired or was written for a different window
/// length.
fn load_window(path: &Path, window: Duration) -> Option<(u64, u64)> {
    let data = std::fs::read(path).ok()?;
    let p: Persisted = serde_json::from_slice(&data).ok()?;
    if p.window_secs != window.as_secs() {
        return None;
    }
    if p.expires_ms <= crate::util::now_ms() {
        return None;
    }
    Some((p.expires_ms, p.used))
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// A snapshot of a [`Quota`], serialisable for the daemon's JSON API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct QuotaSnapshot {
    /// Total bytes per window; `None` when unlimited.
    pub limit: Option<u64>,
    pub used: u64,
    pub remaining: Option<u64>,
    /// Seconds until the budget refills; `None` when unlimited.
    pub resets_in_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_accepts_anything() {
        let q = Quota::unlimited();
        assert!(q.is_unlimited());
        for _ in 0..10 {
            q.charge(u64::MAX / 2).unwrap();
        }
        assert_eq!(q.remaining(), None);
        assert_eq!(q.resets_in(), None);
    }

    #[test]
    fn zero_limit_means_unlimited() {
        let q = Quota::new(Some(0), Duration::from_secs(60));
        assert!(q.is_unlimited());
    }

    #[test]
    fn charges_accumulate_and_then_refuse() {
        let q = Quota::new(Some(1000), Duration::from_secs(3600));
        q.charge(400).unwrap();
        q.charge(500).unwrap();
        assert_eq!(q.used(), 900);
        assert_eq!(q.remaining(), Some(100));

        let err = q.charge(200).unwrap_err();
        assert_eq!(err.remaining, 100);
        // A refusal must not consume anything.
        assert_eq!(q.used(), 900);
    }

    #[test]
    fn an_exactly_full_window_is_allowed() {
        let q = Quota::new(Some(1000), Duration::from_secs(3600));
        q.charge(1000).unwrap();
        assert_eq!(q.remaining(), Some(0));
        assert!(q.charge(1).is_err());
    }

    #[test]
    fn a_budget_smaller_than_one_chunk_still_makes_progress() {
        // 64 KB budget, 1 MB chunks: the first chunk of each window must go through or
        // the download would deadlock.
        let q = Quota::new(Some(64 * 1024), Duration::from_secs(3600));
        q.charge(1 << 20).unwrap();
        assert!(q.charge(1).is_err(), "the second chunk must be refused");
        assert_eq!(q.remaining(), Some(0));
    }

    #[test]
    fn the_window_rolls_and_the_budget_returns() {
        let q = Quota::new(Some(100), Duration::from_millis(60));
        q.charge(100).unwrap();
        assert!(q.charge(1).is_err());
        assert_eq!(q.remaining(), Some(0));

        std::thread::sleep(Duration::from_millis(90));

        // Rolling is lazy: any observation resets it.
        assert_eq!(q.used(), 0);
        assert_eq!(q.remaining(), Some(100));
        q.charge(100).unwrap();
        assert!(q.charge(1).is_err());
    }

    #[test]
    fn resets_in_counts_down() {
        let q = Quota::new(Some(50), Duration::from_secs(10));
        q.charge(50).unwrap();
        let left = q.resets_in().unwrap();
        assert!(left <= Duration::from_secs(10));
        assert!(left > Duration::from_secs(9));
    }

    #[tokio::test]
    async fn wait_for_reset_returns_after_the_window() {
        let q = Quota::new(Some(10), Duration::from_millis(80));
        q.charge(10).unwrap();
        let t = Instant::now();
        q.wait_for_reset().await;
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(70), "{waited:?}");
        // ...and the budget really is back, which is the whole point of the call.
        assert_eq!(q.remaining(), Some(10), "the window should have rolled");
        q.charge(10).unwrap();
        assert!(q.charge(1).is_err());
    }

    #[tokio::test]
    async fn wait_for_reset_terminates_when_the_window_has_already_expired() {
        // Regression: `resets_in` used to roll the window and then report a whole fresh
        // window, so `wait_for_reset` slept forever instead of returning.
        let q = Quota::new(Some(10), Duration::from_millis(30));
        q.charge(10).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(q.resets_in(), Some(Duration::ZERO));

        let t = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), q.wait_for_reset())
            .await
            .expect("wait_for_reset must return once the window has expired");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(q.remaining(), Some(10));
    }

    #[tokio::test]
    async fn wait_for_reset_is_a_no_op_when_unlimited() {
        let q = Quota::unlimited();
        let t = Instant::now();
        q.wait_for_reset().await;
        assert!(t.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn describe_reads_like_a_sentence() {
        assert_eq!(Quota::unlimited().describe(), "no download quota");

        let q = Quota::new(Some(500 << 20), Duration::from_secs(3600));
        q.charge(12 << 20).unwrap();
        let d = q.describe();
        assert!(d.contains("12.00 MB of 500.00 MB used"), "{d}");
        assert!(d.contains("window refills in 59m"), "{d}");

        let q = Quota::new(Some(1000), Duration::from_secs(45));
        q.charge(10).unwrap();
        assert!(
            q.describe().contains("window refills in 44s"),
            "{}",
            q.describe()
        );
    }

    #[test]
    fn snapshot_serialises_for_the_daemon_api() {
        let q = Quota::new(Some(1000), Duration::from_secs(600));
        q.charge(250).unwrap();
        let snap = q.snapshot();
        assert_eq!(snap.limit, Some(1000));
        assert_eq!(snap.used, 250);
        assert_eq!(snap.remaining, Some(750));
        assert_eq!(snap.resets_in_secs, Some(599));

        let json = serde_json::to_string(&snap).unwrap();
        for want in ["\"limit\":1000", "\"used\":250", "\"remaining\":750"] {
            assert!(json.contains(want), "{want} missing from {json}");
        }
    }

    // ---------------------------------------------------------------------
    // Persistence: a restart must not hand back a fresh budget
    // ---------------------------------------------------------------------

    #[test]
    fn a_restarted_quota_resumes_the_window_it_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");

        let q = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        q.charge(400).unwrap();
        q.flush();

        // "Restart": a brand new Quota reading the same file.
        let again = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        assert_eq!(
            again.used(),
            400,
            "the spent bytes must survive the restart"
        );
        assert_eq!(again.remaining(), Some(600));

        again.charge(600).unwrap();
        assert!(
            again.charge(1).is_err(),
            "the budget is spent for this window, not refilled"
        );
    }

    #[test]
    fn charges_are_remembered_without_an_explicit_flush() {
        // The throttle must still let a spent *small* budget reach the disk: a refusal
        // counts as a checkpoint.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");
        let q = Quota::persistent(Some(100), Duration::from_secs(3600), &path);
        q.charge(100).unwrap();
        assert!(q.charge(50).is_err());

        let again = Quota::persistent(Some(100), Duration::from_secs(3600), &path);
        assert_eq!(again.used(), 100);
    }

    #[test]
    fn an_expired_persisted_window_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");
        // A window that expired a minute ago.
        let stale = Persisted {
            expires_ms: crate::util::now_ms() - 60_000,
            used: 999,
            window_secs: 3600,
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

        let q = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        assert_eq!(q.used(), 0);
        assert_eq!(q.remaining(), Some(1000));
    }

    #[test]
    fn a_changed_window_length_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");
        let q = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        q.charge(700).unwrap();
        q.flush();

        // The user edited the window: half-spent bytes from the old window must not be
        // applied to the new one.
        let other = Quota::persistent(Some(1000), Duration::from_secs(60), &path);
        assert_eq!(other.used(), 0);
        assert_eq!(other.remaining(), Some(1000));
    }

    #[test]
    fn an_unreadable_or_corrupt_file_starts_fresh_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");
        std::fs::write(&path, b"{not json").unwrap();

        let q = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        assert_eq!(q.used(), 0);
        // ...and it recovers by writing a good file over the bad one.
        q.charge(100).unwrap();
        q.flush();
        let again = Quota::persistent(Some(1000), Duration::from_secs(3600), &path);
        assert_eq!(again.used(), 100);
    }

    #[test]
    fn an_unlimited_persistent_quota_never_touches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.json");
        let q = Quota::persistent(None, Duration::from_secs(3600), &path);
        q.charge(1 << 30).unwrap();
        q.flush();
        assert!(
            !path.exists(),
            "no budget means nothing to remember, so no file should appear"
        );
    }
}
