//! Queue execution and scheduling.
//!
//! IDM exposes a queue width plus a scheduler that can hold a job until a start time and
//! shut the machine down when the queue drains. Riptide implements the queue width, a
//! scheduled start time per job, and the same "hold the job in the queue and retry" idea
//! that IDM's Tips file describes ("leave this file alone in the queue of IDM scheduler
//! and start the queue. IDM scheduler will retry downloading the file every 30 seconds").

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, Semaphore};

use crate::config::Config;
use crate::engine::{Engine, ProgressFn, SharedLimits};
use crate::error::Result;
use crate::job::{Job, JobState};
use crate::net::ServerLimits;
use crate::project::ProjectStore;
use crate::quota::Quota;
use crate::state::JobStore;
use crate::throttle::RateLimiter;

pub struct Scheduler {
    pub cfg: Config,
    pub store: JobStore,
    /// Recurring definitions. Kept here so the CLI, the daemon and its ticker all reach
    /// the same directory without each re-deriving it from the config.
    pub projects: ProjectStore,
    /// Shared across every job, so "Maximum download speed" really is a global ceiling.
    pub global: Arc<RateLimiter>,
    /// Shared across every job, so "Max. connections number for a server" is enforced
    /// process-wide rather than per job.
    pub servers: Arc<ServerLimits>,
    /// Shared across every job, so "Download limits" is a budget for the *machine*, not
    /// for one download. A fresh `Scheduler` starts the window over.
    pub quota: Arc<Quota>,
    slots: Arc<Semaphore>,
    /// Raised on Ctrl-C or SIGTERM. Every in-flight engine watches this and stops cleanly,
    /// leaving a resumable partial file rather than a truncated one.
    shutdown: Option<Arc<AtomicBool>>,
    /// Units of work currently inside [`Scheduler::execute_with`] or a
    /// [`Scheduler::run_due_projects`] sweep, plus a `Notify` raised whenever one finishes.
    ///
    /// Counted here rather than left to the caller because a shutdown has to *wait* for
    /// these to finish saving — and the daemon, which replies to the client immediately,
    /// does not hold their handles.
    running: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

/// Counts one unit of work for as long as it lives, however it ends.
struct Running<'a>(&'a Scheduler);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if self.0.running.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Last one out: wake every waiter, which then sees zero.
            self.0.idle.notify_waiters();
        }
    }
}

impl Scheduler {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = JobStore::open(cfg.state_dir.clone())?;
        let projects = ProjectStore::open_in_state_dir(&cfg.state_dir)?;
        let global = Arc::new(RateLimiter::new(cfg.speed_limit));
        let servers = Arc::new(ServerLimits::new(cfg.per_server_connections));
        let quota = Arc::new(cfg.persistent_quota());
        let slots = Arc::new(Semaphore::new(cfg.max_concurrent_jobs.max(1)));
        Ok(Scheduler {
            cfg,
            store,
            projects,
            global,
            servers,
            quota,
            slots,
            shutdown: None,
            running: Arc::new(AtomicUsize::new(0)),
            idle: Arc::new(Notify::new()),
        })
    }

    /// Install a flag that, when raised, cancels every running job.
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = Some(flag);
    }

    /// Count one unit of work for as long as it lives.
    ///
    /// A guard rather than a pair of calls at the ends: every path out of the work —
    /// including the `?` on a failed save — has to decrement, or a shutdown would wait for
    /// a job that already ended.
    fn enter(&self) -> Running<'_> {
        self.running.fetch_add(1, Ordering::SeqCst);
        Running(self)
    }

    /// Wait until nothing is running, or `grace` elapses. Returns what is still running.
    ///
    /// This is what makes a signal-triggered stop *clean* rather than merely fast: a
    /// cancelled job stops at a chunk boundary, records `Paused` and flushes the quota
    /// before it returns, so a process that waits for this can then exit 0 instead of
    /// looking like a crash. The deadline is the backstop — a transfer wedged mid-chunk
    /// must not hold `docker stop` open until it is killed — and the count returned is what
    /// the caller reports when the deadline wins.
    pub async fn wait_for_idle(&self, grace: std::time::Duration) -> usize {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            // Registered *before* the count is read, so a finish that happens from here on
            // is never missed (`Notify` only signals waiters that already exist). The loop
            // re-reads the count after waking, so a notification raised in between is not
            // waited for twice.
            let waiting = self.idle.notified();
            tokio::pin!(waiting);
            if self.running.load(Ordering::SeqCst) == 0 {
                return 0;
            }
            match tokio::time::timeout_at(deadline, waiting).await {
                Ok(()) => continue,
                Err(_) => return self.running.load(Ordering::SeqCst),
            }
        }
    }

    /// Run one job to completion (or failure) and persist the outcome.
    pub async fn execute(&self, job: Job) -> Result<Job> {
        self.execute_with(job, None).await
    }

    pub async fn execute_with(&self, mut job: Job, progress: Option<ProgressFn>) -> Result<Job> {
        let _running = self.enter();
        job.state = JobState::Active;
        job.touch();
        self.store.save(&job)?;

        let mut engine = Engine::with_shared(
            self.cfg.clone(),
            job,
            SharedLimits {
                global: Some(self.global.clone()),
                servers: Some(self.servers.clone()),
                quota: Some(self.quota.clone()),
            },
        )?;
        if let Some(p) = progress {
            engine.on_progress(p);
        }

        // Watch for a shutdown request (Ctrl-C) and propagate it into this engine so the
        // partial file stays consistent and the job is recorded as Paused, i.e. resumable.
        let watcher = self.shutdown.as_ref().map(|flag| {
            let flag = flag.clone();
            let cancel = engine.cancel_handle();
            tokio::spawn(async move {
                while !flag.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                cancel.store(true, Ordering::SeqCst);
            })
        });

        // FTP has no HEAD probe; its own client discovers the size.
        let outcome = async {
            if engine.job().kind != crate::job::JobKind::Ftp {
                engine.probe().await?;
            }
            engine.run().await
        }
        .await;

        if let Some(w) = watcher {
            w.abort();
        }

        let mut finished = engine.job().clone();
        if outcome.is_err() {
            // `Engine::run` already recorded the failure state and message.
            if finished.error.is_none() {
                finished.error = outcome.as_ref().err().map(|e| e.to_string());
            }
        }
        finished.touch();
        self.store.save(&finished)?;
        // A job boundary is a natural checkpoint: persist the budget now so a process
        // killed straight afterwards cannot lose the bytes it just charged for.
        self.quota.flush();
        outcome.map(|_| finished)
    }

    /// Queue a job and run it in the background, respecting the queue width.
    pub fn spawn(self: &Arc<Self>, job: Job) -> tokio::task::JoinHandle<Result<Job>> {
        self.spawn_with(job, None)
    }

    /// Queue a job with a progress callback attached.
    pub fn spawn_with(
        self: &Arc<Self>,
        job: Job,
        progress: Option<ProgressFn>,
    ) -> tokio::task::JoinHandle<Result<Job>> {
        let this = self.clone();
        tokio::spawn(async move {
            let _permit = this
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| crate::error::RiptideError::other(e.to_string()))?;
            this.execute_with(job, progress).await
        })
    }

    /// Process every pending job, oldest first.
    pub async fn run_queue(&self) -> Result<Vec<Result<Job>>> {
        let pending = self.store.pending()?;
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let mut handles = Vec::new();
        for job in pending {
            if let Some(at) = job.scheduled_at {
                let now = crate::util::now_ms();
                if at > now {
                    let wait = std::time::Duration::from_millis(at - now);
                    tracing::info!(
                        id = %job.id,
                        seconds = wait.as_secs(),
                        "job is scheduled for later; waiting"
                    );
                    tokio::time::sleep(wait).await;
                }
            }
            let this = Arc::new(self.clone_shallow());
            handles.push((job.id.clone(), this.clone().spawn(job)));
        }

        let mut out = Vec::with_capacity(handles.len());
        for (id, h) in handles {
            match h.await {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::error!(job = %id, error = %e, "worker task failed");
                    out.push(Err(crate::error::RiptideError::other(e.to_string())));
                }
            }
        }

        // A job stopped by the download quota is Paused, not Failed, and becomes runnable
        // again by itself. Say so plainly, because a queue run that returns with work
        // still outstanding otherwise looks like a bug.
        for job in out.iter().filter_map(|r| r.as_ref().ok()) {
            if job.quota_paused {
                tracing::info!(
                    id = %job.id,
                    reason = job.error.as_deref().unwrap_or("download quota exhausted"),
                    "paused on the download quota; run the queue again after the window refills"
                );
            }
        }
        Ok(out)
    }

    /// A shallow copy sharing the same limiter and slot pool. Used for spawning.
    fn clone_shallow(&self) -> Scheduler {
        Scheduler {
            cfg: self.cfg.clone(),
            store: self.store.clone(),
            projects: self.projects.clone(),
            global: self.global.clone(),
            servers: self.servers.clone(),
            quota: self.quota.clone(),
            slots: self.slots.clone(),
            shutdown: self.shutdown.clone(),
            running: self.running.clone(),
            idle: self.idle.clone(),
        }
    }

    pub fn available_slots(&self) -> usize {
        self.slots.available_permits()
    }

    /// Run every project that is due, oldest first, and return what happened.
    ///
    /// Sequentially, on purpose: a scheduled sweep should not compete with itself, and a
    /// project that is slow is better than a machine running twenty crawls at once.
    pub async fn run_due_projects(
        &self,
        runner: &crate::project::ProjectRunner,
    ) -> Vec<(String, crate::project::RunOutcome)> {
        // A sweep is a project run's only handle too: counting it here is what lets a
        // shutdown wait for a crawl to finish saving rather than cutting it off.
        let _running = self.enter();
        let due = match self.projects.due(crate::util::now_ms()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list projects");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(due.len());
        for mut project in due {
            tracing::info!(project = %project.name, kind = project.kind.as_str(), "running");
            let outcome = runner.run(&mut project).await;
            if let Err(e) = self.projects.save(&project) {
                tracing::warn!(project = %project.name, error = %e, "cannot save project");
            }
            out.push((project.name.clone(), outcome));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::job::JobKind;
    use std::time::Duration;

    fn test_cfg(dir: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        cfg.state_dir = dir.join("state");
        cfg.download_dir = dir.join("downloads");
        cfg.connections = 2;
        cfg.min_chunk = 64;
        cfg.max_concurrent_jobs = 2;
        cfg
    }

    #[test]
    fn store_is_created_and_jobs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let sched = Scheduler::new(cfg).unwrap();
        assert!(dir.path().join("state").exists());

        let job = Job::new(
            "job1".into(),
            "http://example.test/a".into(),
            dir.path().join("downloads/a"),
            JobKind::Http,
        );
        sched.store.save(&job).unwrap();
        assert_eq!(sched.store.list().unwrap().len(), 1);
        assert_eq!(sched.store.pending().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn queue_width_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(dir.path());
        cfg.max_concurrent_jobs = 1;
        let sched = Scheduler::new(cfg).unwrap();
        assert_eq!(sched.available_slots(), 1);

        let permit = sched.slots.clone().acquire_owned().await.unwrap();
        assert_eq!(sched.available_slots(), 0);
        drop(permit);
        assert_eq!(sched.available_slots(), 1);
    }

    #[tokio::test]
    async fn an_empty_queue_finishes_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let sched = Scheduler::new(test_cfg(dir.path())).unwrap();
        let out = sched.run_queue().await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn an_idle_scheduler_is_idle_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let sched = Scheduler::new(test_cfg(dir.path())).unwrap();
        // Not "within the grace period": immediately. A stop that waited five seconds for
        // nothing is the failure this is here to catch.
        assert_eq!(sched.wait_for_idle(Duration::from_millis(10)).await, 0);
    }

    #[tokio::test]
    async fn waiting_for_idle_wakes_when_the_work_ends_and_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let sched = Arc::new(Scheduler::new(test_cfg(dir.path())).unwrap());

        // The deadline is the backstop: work that never ends must not hold the stop open.
        let stuck = sched.enter();
        assert_eq!(
            sched.wait_for_idle(Duration::from_millis(50)).await,
            1,
            "a job that outlasts the grace period should be reported, not waited for"
        );

        // And once it does end, the waiter is woken by it rather than by the deadline.
        let waiter = {
            let sched = sched.clone();
            tokio::spawn(async move { sched.wait_for_idle(Duration::from_secs(60)).await })
        };
        // Let the waiter register before the guard is dropped, so a missed notification
        // would show up as a timeout instead of a pass.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        drop(stuck);
        assert_eq!(waiter.await.unwrap(), 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "woke on the drop, not on the deadline: {:?}",
            started.elapsed()
        );
    }
}
