//! Recurring projects: IDM's **periodic synchronization** and **scheduled site
//! grabber projects**.
//!
//! IDM keeps both in files with the same name (`sprojects.dat`), and so does this: a
//! [`Project`] is a named, persisted definition that is meant to run again and again.
//! The `kind` says what "again" means.
//!
//! * [`ProjectKind::File`] — one URL kept up to date at one local path. Each run asks the
//!   server whether the copy on disk is still current, using `If-None-Match` /
//!   `If-Modified-Since`. A `304 Not Modified` means there is nothing to do and **no body
//!   is transferred**. Anything else means a new version, which is fetched in full and
//!   swapped into place, so a reader never sees a half-written file.
//! * [`ProjectKind::Site`] — a page re-crawled on a schedule, optionally fetching what the
//!   crawl finds. Each target gets the same conditional treatment as a `File` project, so
//!   an unchanged page costs one conditional request rather than a re-download. With
//!   `archive` set, the same run instead keeps an **offline mirror**: the pages, plus the
//!   stylesheets, scripts, images and fonts they need to render, with the markup rewritten
//!   to point at the local copies (see [`crate::archive`]).
//!
//! Three rules are deliberate and worth stating, because the alternatives are all tempting:
//!
//! 1. **A sync never resumes a partial download across runs.** Bytes left in the working
//!    file belong to a version the server has since replaced, and splicing old and new
//!    bytes together produces a file that is silently corrupt. So when the remote turns
//!    out to have changed, the working file is discarded before the fetch begins. With
//!    resume *within* a run, a dropped connection still costs only the bytes lost.
//! 2. **A failed run never damages the local copy.** The fetch lands in the engine's
//!    working file and is only renamed into place once it is complete and verified. If it
//!    fails, the file that was already there is untouched and the error is recorded on the
//!    project.
//! 3. **The next run time is measured from the end of the previous run.** A run slower
//!    than its own interval therefore cannot queue up a backlog of missed slots; a daemon
//!    that was down for a week runs each project once when it comes back, not 168 times.
//!
//! Nothing here invents its own transfer machinery: every byte moves through
//! [`crate::engine::Engine`], so a project is governed by the same global and per-file
//! rate ceilings, the same per-server connection ceiling, and the same download quota as
//! an interactive download. A project that runs out of quota is left `Paused` and simply
//! retried at its next interval.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::archive;
use crate::config::Config;
use crate::engine::{Engine, SharedLimits};
use crate::error::{Result, RiptideError};
use crate::grabber::{self, GrabberConfig};
use crate::job::{Job, JobKind};
use crate::net::ServerLimits;
use crate::quota::Quota;
use crate::throttle::RateLimiter;
use crate::util;

/// Shortest interval a project may be given.
///
/// Not enforced out of paternalism: below about a second a "schedule" stops being one and
/// becomes a loop against somebody else's server, which is indistinguishable from abuse.
pub const MIN_INTERVAL_SECS: u64 = 1;

/// Intervals under this are allowed but called out, because they are almost never meant.
pub const BUSY_INTERVAL_SECS: u64 = 60;

/// Default ceiling on how many crawl results one site run will fetch.
///
/// A page listing ten thousand links should not turn one scheduled run into ten thousand
/// downloads; the crawl is capped by `max_pages` but the *result* list is not.
pub const DEFAULT_MAX_DOWNLOADS: usize = 500;

/// What a project does when it runs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    /// Keep one URL up to date at one local path (IDM's "periodic synchronization").
    File,
    /// Re-crawl a page on a schedule (IDM's "site grabber project").
    Site,
}

impl ProjectKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectKind::File => "file",
            ProjectKind::Site => "site",
        }
    }
}

/// The validators a server gave us for a resource: what to send back to ask
/// "is this still current?".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Validator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl Validator {
    /// Conditional request headers for this validator.
    ///
    /// Both are sent when both are known. RFC 9110 says `If-None-Match` wins when a server
    /// understands both, and a server that only understands `Last-Modified` still gets a
    /// usable hint — so sending both is strictly more compatible than picking one.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(e) = &self.etag {
            out.push(("If-None-Match".to_string(), e.clone()));
        }
        if let Some(lm) = &self.last_modified {
            out.push(("If-Modified-Since".to_string(), lm.clone()));
        }
        out
    }

    /// Copy the validators a completed job recorded for the bytes it just wrote.
    pub fn from_job(job: &Job) -> Self {
        Validator {
            etag: job.etag.clone(),
            last_modified: job.last_modified.clone(),
        }
    }

    /// Copy the validators a bare fetch reported, for bytes that are not a job — a site
    /// archiver's assets.
    pub fn from_meta(meta: &crate::engine::FetchMeta) -> Self {
        Validator {
            etag: meta.etag.clone(),
            last_modified: meta.last_modified.clone(),
        }
    }

    /// True when the server gave us nothing to ask about, so every run must re-download.
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

/// A named, persisted, recurring definition.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub name: String,
    pub kind: ProjectKind,
    pub url: String,
    /// `File`: the file to keep up to date. `Site`: the directory to write into.
    pub output: PathBuf,
    /// Seconds between runs.
    pub interval_secs: u64,
    pub enabled: bool,

    /// Request shape: proxy, headers, credentials, TLS policy.
    #[serde(default)]
    pub ctx: crate::job::RequestContext,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,

    /// `Site`: how to crawl.
    #[serde(default)]
    pub grab: GrabberConfig,
    /// `Site`: fetch what the crawl finds, rather than only noting it.
    #[serde(default)]
    pub download: bool,
    /// `Site`: mirror the pages *and everything they need to render*, rewritten so the copy
    /// works from disk. Supersedes `download`, and uses `max_downloads` as its asset cap.
    #[serde(default)]
    pub archive: bool,
    /// `Site`: cap on downloads per run.
    #[serde(default = "default_max_downloads")]
    pub max_downloads: usize,

    /// `File`: the validator for the version on disk.
    #[serde(default, skip_serializing_if = "Validator::is_empty")]
    pub validator: Validator,
    /// `Site`: per-target validators, so an unchanged target is skipped.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub validators: BTreeMap<String, Validator>,

    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub last_run_ms: Option<u64>,
    /// When the project should next run. `None` means "not scheduled" — it will only run
    /// when asked for explicitly.
    #[serde(default)]
    pub next_run_ms: Option<u64>,
    #[serde(default)]
    pub runs: u64,
    /// Consecutive failed runs; reset by any success. Surfaced so that a project quietly
    /// failing every hour for a month is visible in `project list`.
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Bytes written by the last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_bytes: Option<u64>,
    /// `Site`: links accepted by the last crawl, and how many were fetched or skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_found: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fetched: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skipped: Option<usize>,
}

fn default_connections() -> usize {
    8
}

fn default_max_downloads() -> usize {
    DEFAULT_MAX_DOWNLOADS
}

impl Project {
    /// A project that will next run one interval from now.
    pub fn new(
        name: impl Into<String>,
        kind: ProjectKind,
        url: impl Into<String>,
        output: PathBuf,
        interval_secs: u64,
    ) -> Self {
        let now = util::now_ms();
        Project {
            name: name.into(),
            kind,
            url: url.into(),
            output,
            interval_secs,
            enabled: true,
            ctx: crate::job::RequestContext::default(),
            connections: default_connections(),
            quality: None,
            grab: GrabberConfig::default(),
            download: false,
            archive: false,
            max_downloads: DEFAULT_MAX_DOWNLOADS,
            validator: Validator::default(),
            validators: BTreeMap::new(),
            created_ms: now,
            updated_ms: now,
            last_run_ms: None,
            next_run_ms: Some(now.saturating_add(interval_secs.saturating_mul(1000))),
            runs: 0,
            failures: 0,
            last_status: None,
            last_error: None,
            last_bytes: None,
            last_found: None,
            last_fetched: None,
            last_skipped: None,
        }
    }

    /// Whether this project should run at `now_ms`.
    pub fn is_due(&self, now_ms: u64) -> bool {
        self.enabled && matches!(self.next_run_ms, Some(at) if at <= now_ms)
    }

    /// Schedule the next run relative to the *end* of the run that just finished.
    pub fn reschedule(&mut self, finished_ms: u64) {
        self.next_run_ms =
            Some(finished_ms.saturating_add(self.interval_secs.saturating_mul(1000)));
        self.updated_ms = finished_ms;
    }

    /// A one-line summary for `project list`.
    pub fn schedule_summary(&self, now_ms: u64) -> String {
        if !self.enabled {
            return "disabled".into();
        }
        match self.next_run_ms {
            None => "not scheduled".into(),
            Some(at) if at <= now_ms => "due".into(),
            Some(at) => format!("in {}", human_span(at - now_ms)),
        }
    }

    /// What the last run did and how long ago, for `project list`.
    pub fn last_run_summary(&self, now_ms: u64) -> String {
        let Some(at) = self.last_run_ms else {
            return "never".into();
        };
        let what = self
            .last_status
            .clone()
            .unwrap_or_else(|| "ran".to_string());
        // Repeating failures are the thing a listing exists to surface.
        let what = if self.failures > 1 {
            format!("{what} x{}", self.failures)
        } else {
            what
        };
        format!("{what}, {} ago", human_span(now_ms.saturating_sub(at)))
    }
}

/// A coarse duration, good enough for a "next run in ..." column.
fn human_span(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Project names become file names, so they are restricted to a single safe path
/// component. Rejecting the bad cases here means the store can never be talked into
/// writing outside its directory.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(RiptideError::other("project name must not be empty"));
    }
    if name.len() > 64 {
        return Err(RiptideError::other(
            "project name must be at most 64 characters",
        ));
    }
    if name == "." || name == ".." {
        return Err(RiptideError::other("project name must not be `.` or `..`"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(RiptideError::other(format!(
            "project name may only contain letters, digits, `-`, `_` and `.` (found {c:?}); \
             it is also used as the file name"
        )));
    }
    Ok(())
}

/// Parse an interval such as `30m`, `1h`, `1d`.
pub fn parse_interval(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(RiptideError::other("interval must not be empty"));
    }
    // A bare number means seconds, matching how `--limit 500` means 500 bytes; requiring a
    // unit for the one time interval that is naturally a plain count would be pedantry.
    let secs = if s.chars().all(|c| c.is_ascii_digit()) {
        s.parse::<u64>()
            .map_err(|e| RiptideError::other(format!("cannot parse interval {s:?}: {e}")))?
    } else {
        humantime::parse_duration(s)
            .map_err(|e| RiptideError::other(format!("cannot parse interval {s:?}: {e}")))?
            .as_secs()
    };
    if secs < MIN_INTERVAL_SECS {
        return Err(RiptideError::other(format!(
            "interval must be at least {MIN_INTERVAL_SECS}s (got {})",
            s
        )));
    }
    Ok(secs)
}

/// One JSON document per project, written atomically — the same crash-safety rule the
/// job store follows.
#[derive(Clone, Debug)]
pub struct ProjectStore {
    dir: PathBuf,
}

impl ProjectStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(ProjectStore { dir })
    }

    /// The default location, under the state directory.
    ///
    /// A subdirectory keeps projects out of the job store's `*.json` listing, which reads
    /// every file in its own directory as a job.
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self> {
        Self::open(state_dir.join("projects"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    pub fn save(&self, p: &Project) -> Result<()> {
        validate_name(&p.name)?;
        let data = serde_json::to_vec_pretty(p)?;
        util::atomic_write(&self.path_for(&p.name), &data)?;
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<Project> {
        validate_name(name)?;
        let path = self.path_for(name);
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RiptideError::NoSuchProject(name.to_string())
            } else {
                RiptideError::Io(e)
            }
        })?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn exists(&self, name: &str) -> bool {
        validate_name(name).is_ok() && self.path_for(name).exists()
    }

    /// Every project, by name. An unparseable file is skipped rather than breaking the
    /// listing.
    pub fn list(&self) -> Result<Vec<Project>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = std::fs::read(&p) {
                if let Ok(project) = serde_json::from_slice::<Project>(&data) {
                    out.push(project);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Projects that should run at `now_ms`, earliest first.
    pub fn due(&self, now_ms: u64) -> Result<Vec<Project>> {
        let mut due: Vec<Project> = self
            .list()?
            .into_iter()
            .filter(|p| p.is_due(now_ms))
            .collect();
        due.sort_by_key(|p| p.next_run_ms.unwrap_or(u64::MAX));
        Ok(due)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        match std::fs::remove_file(self.path_for(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// What one run of a project did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The local copy is current; nothing was transferred.
    Unchanged,
    /// New bytes were written.
    Updated,
    /// The crawl ran and recorded what it found, but this project is planning-only, so
    /// nothing was fetched.
    Planned,
    /// Some of the work succeeded and some failed. Only a site run can be partial.
    Partial,
    /// Nothing succeeded.
    Failed,
    /// Stopped because the download quota ran out. Not a failure: the run is retried at
    /// the next interval, and the local copy is untouched.
    QuotaPaused,
}

impl RunOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunOutcome::Unchanged => "unchanged",
            RunOutcome::Updated => "updated",
            RunOutcome::Planned => "planned",
            RunOutcome::Partial => "partial",
            RunOutcome::Failed => "failed",
            RunOutcome::QuotaPaused => "quota-paused",
        }
    }

    /// Whether a caller (cron, the daemon) should treat the run as a failure.
    ///
    /// Running out of quota explicitly is not one: it resolves itself, and reporting it as
    /// an error would make a correctly behaving installation send mail every hour.
    pub fn is_failure(&self) -> bool {
        matches!(self, RunOutcome::Failed | RunOutcome::Partial)
    }

    /// Whether anything was written to disk.
    pub fn wrote_bytes(&self) -> bool {
        matches!(self, RunOutcome::Updated)
    }
}

/// Runs projects. Holds the limiters so that project traffic is governed by the same
/// ceilings as everything else in the process.
pub struct ProjectRunner {
    cfg: Config,
    global: Arc<RateLimiter>,
    servers: Arc<ServerLimits>,
    quota: Arc<Quota>,
}

impl ProjectRunner {
    /// A runner with limiters of its own, for a one-shot command.
    pub fn new(cfg: Config) -> Self {
        let global = Arc::new(RateLimiter::new(cfg.speed_limit));
        let servers = Arc::new(ServerLimits::new(cfg.per_server_connections));
        let quota = Arc::new(cfg.persistent_quota());
        ProjectRunner::with_limits(cfg, global, servers, quota)
    }

    /// A runner sharing a longer-lived process's limiters, as the daemon does, so a
    /// project competes for bandwidth with the interactive queue rather than doubling it.
    pub fn with_limits(
        cfg: Config,
        global: Arc<RateLimiter>,
        servers: Arc<ServerLimits>,
        quota: Arc<Quota>,
    ) -> Self {
        ProjectRunner {
            cfg,
            global,
            servers,
            quota,
        }
    }

    fn shared(&self) -> SharedLimits {
        SharedLimits {
            global: Some(self.global.clone()),
            servers: Some(self.servers.clone()),
            quota: Some(self.quota.clone()),
        }
    }

    /// Run one project once, folding the result into the project's bookkeeping.
    ///
    /// Failures are recorded on the project rather than returned, because this is called
    /// from a scheduler whose remaining work must not be abandoned because one project's
    /// server was down.
    pub async fn run(&self, project: &mut Project) -> RunOutcome {
        let started = util::now_ms();
        project.last_run_ms = Some(started);
        project.updated_ms = started;
        project.runs += 1;
        project.last_error = None;

        let outcome = match project.kind {
            ProjectKind::File => self.run_file(project).await,
            // Only one of these is a mirror, and the difference matters: a plain site run
            // harvests links, an archive run also holds what the pages need to render.
            ProjectKind::Site if project.archive => self.run_archive(project).await,
            ProjectKind::Site => self.run_site(project).await,
        };

        project.last_status = Some(outcome.as_str().to_string());
        if outcome.is_failure() {
            project.failures = project.failures.saturating_add(1);
        } else {
            project.failures = 0;
        }
        project.reschedule(util::now_ms());
        // The budget is a process-wide resource; checkpoint it at the project boundary so
        // a kill straight afterwards cannot lose the bytes this run charged for.
        self.quota.flush();
        outcome
    }

    async fn run_file(&self, project: &mut Project) -> RunOutcome {
        if project.validator.is_empty() && project.last_run_ms.is_some() && project.runs > 1 {
            // Worth saying once per run: without a validator every sync re-downloads the
            // whole file, which is easy to mistake for the sync not working.
            tracing::warn!(
                project = %project.name,
                "the server supplied neither ETag nor Last-Modified, so every run re-downloads"
            );
        }

        let mut job = self.job_for(project, &project.url, project.output.clone());
        for (k, v) in project.validator.headers() {
            job.ctx.headers.push((k, v));
        }

        let mut engine = match Engine::with_shared(self.cfg.clone(), job, self.shared()) {
            Ok(e) => e,
            Err(e) => return self.fail(project, &e.to_string()),
        };

        if let Err(e) = engine.probe().await {
            return self.fail(project, &e.to_string());
        }
        if engine.job().not_modified {
            tracing::info!(project = %project.name, "unchanged (304)");
            project.last_bytes = Some(0);
            return RunOutcome::Unchanged;
        }

        // The remote is a different version, so whatever is in the working file belongs to
        // a version we can no longer identify against it. Discard it rather than splicing
        // two versions together; this is also why a sync never resumes across runs.
        let part = engine.job().part_path();
        if part.exists() {
            tracing::debug!(project = %project.name, "discarding a partial download from an older version");
            let _ = std::fs::remove_file(&part);
        }

        match engine.run().await {
            Ok(()) => {
                // Remember the validators for the version just written, so the next run can
                // ask instead of pulling the body again.
                project.validator = Validator::from_job(engine.job());
                project.last_bytes = Some(engine.job().downloaded);
                tracing::info!(
                    project = %project.name,
                    bytes = engine.job().downloaded,
                    "updated"
                );
                RunOutcome::Updated
            }
            Err(RiptideError::QuotaExhausted(msg)) => {
                tracing::info!(project = %project.name, reason = %msg, "paused on the download quota");
                project.last_error = Some(msg.to_string());
                RunOutcome::QuotaPaused
            }
            Err(e) => self.fail(project, &e.to_string()),
        }
    }

    async fn run_site(&self, project: &mut Project) -> RunOutcome {
        // A throwaway job supplies a configured HTTP client (proxy, UA, credentials, TLS
        // policy) without being persisted anywhere.
        let scratch = self.job_for(
            project,
            &project.url,
            self.cfg.download_dir.join(".grab-probe"),
        );
        let mut scratch = scratch;
        scratch.kind = JobKind::Grabber;

        let engine = match Engine::with_shared(self.cfg.clone(), scratch, self.shared()) {
            Ok(e) => e,
            Err(e) => return self.fail(project, &e.to_string()),
        };
        let fetcher = engine.fetcher();

        let targets = match grabber::crawl(&fetcher, &project.url, &project.grab).await {
            Ok(t) => t,
            Err(e) => return self.fail(project, &e.to_string()),
        };
        project.last_found = Some(targets.len());

        if !project.download {
            tracing::info!(
                project = %project.name,
                links = targets.len(),
                "crawl finished (run with --download to fetch what it finds)"
            );
            project.last_fetched = Some(0);
            project.last_skipped = Some(targets.len());
            return RunOutcome::Planned;
        }

        if targets.is_empty() {
            project.last_fetched = Some(0);
            project.last_skipped = Some(0);
            return RunOutcome::Unchanged;
        }

        std::fs::create_dir_all(&project.output).ok();

        let queued: Vec<_> = targets.iter().take(project.max_downloads).collect();
        if targets.len() > queued.len() {
            tracing::warn!(
                project = %project.name,
                found = targets.len(),
                fetching = queued.len(),
                cap = project.max_downloads,
                "crawl found more links than this run will fetch"
            );
        }

        let mut fetched = 0usize;
        let mut skipped = 0usize;
        let mut errors = Vec::new();
        let mut bytes = 0u64;

        // Sequential on purpose: a scheduled job re-running unattended should be gentle
        // with the origin. Per-server ceilings still apply, and the crawl itself already
        // used the connection pool.
        for target in &queued {
            let dest = target_path(&project.output, &target.url);
            let mut job = self.job_for(project, &target.url, dest);
            if let Some(v) = project.validators.get(&target.url) {
                for (k, val) in v.headers() {
                    job.ctx.headers.push((k, val));
                }
            }

            let mut engine = match Engine::with_shared(self.cfg.clone(), job, self.shared()) {
                Ok(e) => e,
                Err(e) => {
                    errors.push(format!("{}: {e}", target.url));
                    continue;
                }
            };
            match engine.probe().await {
                Ok(()) if engine.job().not_modified => {
                    skipped += 1;
                    continue;
                }
                Ok(()) => {}
                Err(e) => {
                    errors.push(format!("{}: {e}", target.url));
                    continue;
                }
            }

            let part = engine.job().part_path();
            if part.exists() {
                let _ = std::fs::remove_file(&part);
            }

            match engine.run().await {
                Ok(()) => {
                    // Record as we go, so a later quota stop does not lose the validators
                    // for the targets that did finish.
                    project
                        .validators
                        .insert(target.url.clone(), Validator::from_job(engine.job()));
                    bytes = bytes.saturating_add(engine.job().downloaded);
                    fetched += 1;
                }
                Err(RiptideError::QuotaExhausted(msg)) => {
                    project.last_fetched = Some(fetched);
                    project.last_skipped = Some(skipped);
                    project.last_bytes = Some(bytes);
                    project.last_error = Some(format!(
                        "{msg}; {} target(s) remain for the next run",
                        queued.len() - fetched - skipped
                    ));
                    return RunOutcome::QuotaPaused;
                }
                Err(e) => errors.push(format!("{}: {e}", target.url)),
            }
        }

        project.last_fetched = Some(fetched);
        project.last_skipped = Some(skipped);
        project.last_bytes = Some(bytes);

        if errors.is_empty() {
            tracing::info!(
                project = %project.name,
                found = targets.len(),
                fetched,
                skipped,
                "site run finished"
            );
            if fetched == 0 && skipped > 0 {
                RunOutcome::Unchanged
            } else {
                RunOutcome::Updated
            }
        } else {
            let summary = format!(
                "{} of {} target(s) failed: {}",
                errors.len(),
                queued.len(),
                errors.join("; ")
            );
            project.last_error = Some(summary.clone());
            tracing::warn!(project = %project.name, %summary, "site run finished with errors");
            if fetched > 0 || skipped > 0 {
                RunOutcome::Partial
            } else {
                RunOutcome::Failed
            }
        }
    }

    /// A scheduled offline mirror: re-read the pages, transfer only the assets that changed,
    /// and rewrite the markup so the copy works from disk.
    ///
    /// What "unchanged" means here is deliberately strict: the run compares the files it
    /// would write against the files on disk, so a project whose site has not moved reports
    /// `Unchanged` even though it did re-read the pages to find that out.
    ///
    /// The validators live next to the mirror rather than in the project document, so moving
    /// or copying the mirror keeps its record of what the server said. See
    /// [`archive::manifest_path`].
    async fn run_archive(&self, project: &mut Project) -> RunOutcome {
        let mut scratch = self.job_for(
            project,
            &project.url,
            self.cfg.download_dir.join(".grab-probe"),
        );
        scratch.kind = JobKind::Grabber;

        let engine = match Engine::with_shared(self.cfg.clone(), scratch, self.shared()) {
            Ok(e) => e,
            Err(e) => return self.fail(project, &e.to_string()),
        };
        let fetcher = engine.fetcher();

        let cfg = archive::ArchiveConfig {
            grab: project.grab.clone(),
            // Reused rather than given its own knob: from the outside, "how much does one
            // run fetch" is one question, whether the answers are downloads or assets.
            max_assets: project.max_downloads.max(1),
            ..archive::ArchiveConfig::default()
        };
        if let Err(e) = std::fs::create_dir_all(&project.output) {
            return self.fail(project, &e.to_string());
        }

        let report = match archive::archive(&fetcher, &project.url, &cfg, &project.output).await {
            Ok(r) => r,
            Err(e) => return self.fail(project, &e.to_string()),
        };

        project.last_found = Some(report.held());
        project.last_fetched = Some(report.written);
        project.last_skipped = Some(report.unchanged);
        project.last_bytes = Some(report.bytes);

        if report.quota_paused {
            let msg = report
                .failures
                .last()
                .cloned()
                .unwrap_or_else(|| "download quota exhausted".to_string());
            tracing::info!(project = %project.name, reason = %msg, "archive paused on the download quota");
            project.last_error = Some(msg);
            return RunOutcome::QuotaPaused;
        }

        tracing::info!(
            project = %project.name,
            documents = report.documents,
            stylesheets = report.stylesheets,
            assets = report.assets,
            written = report.written,
            unchanged = report.unchanged,
            remote = report.remote_links,
            "archive run finished"
        );

        if !report.failures.is_empty() {
            let summary = format!(
                "{} of {} file(s) failed: {}",
                report.failures.len(),
                report.held(),
                report.failures.join("; ")
            );
            project.last_error = Some(summary.clone());
            tracing::warn!(project = %project.name, %summary, "archive run finished with errors");
            return if report.written > 0 {
                RunOutcome::Partial
            } else {
                RunOutcome::Failed
            };
        }

        if report.is_unchanged() {
            RunOutcome::Unchanged
        } else {
            RunOutcome::Updated
        }
    }

    /// Build a job for this project: its request context, its connection count, and the
    /// kind implied by the URL.
    fn job_for(&self, project: &Project, url: &str, output: PathBuf) -> Job {
        let mut job = Job::new(util::gen_id(), url.to_string(), output, JobKind::guess(url));
        job.ctx = project.ctx.clone();
        // The same knobs a `download` of this URL would get, so a project is governed by
        // the configuration rather than by `Job::new`'s defaults.
        job.connections = project.connections;
        job.min_chunk = self.cfg.min_chunk;
        job.max_retries = self.cfg.max_retries;
        job.quality = project.quality.clone();
        job.output_provisional = false;
        job
    }

    fn fail(&self, project: &mut Project, message: &str) -> RunOutcome {
        project.last_error = Some(message.to_string());
        tracing::warn!(project = %project.name, error = %message, "project run failed");
        RunOutcome::Failed
    }
}

/// Where a crawl result is written inside a project's directory.
///
/// A URL with no file name of its own (`https://host/` or `.../dir/`) gets one derived
/// from its host and path, so two index pages on one site cannot land on the same path.
fn target_path(dir: &Path, url: &str) -> PathBuf {
    if let Some(name) = util::filename_from_url(url) {
        return dir.join(name);
    }
    let parsed = url::Url::parse(url).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("index");
    let mut stem: String = parsed
        .as_ref()
        .map(|u| u.path().trim_matches('/').replace('/', "_"))
        .unwrap_or_default();
    if stem.is_empty() {
        stem = "index".into();
    }
    dir.join(util::sanitize_filename(&format!("{host}_{stem}.html")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::job::JobKind;

    fn project(name: &str) -> Project {
        Project::new(
            name,
            ProjectKind::File,
            "http://example.test/f.bin",
            PathBuf::from("/tmp/f.bin"),
            3600,
        )
    }

    #[test]
    fn a_project_round_trips_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();

        let mut p = project("nightly");
        p.validator = Validator {
            etag: Some("\"abc\"".into()),
            last_modified: None,
        };
        p.download = true;
        p.grab.depth = 3;
        p.grab.include = vec!["*.zip".into()];
        p.validators.insert(
            "http://example.test/a.zip".into(),
            Validator {
                etag: None,
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            },
        );
        store.save(&p).unwrap();

        let back = store.load("nightly").unwrap();
        assert_eq!(back.kind, ProjectKind::File);
        assert_eq!(back.interval_secs, 3600);
        assert_eq!(back.validator.etag.as_deref(), Some("\"abc\""));
        assert!(back.validator.last_modified.is_none());
        assert!(back.download);
        assert_eq!(back.grab.depth, 3);
        assert_eq!(back.grab.include, vec!["*.zip".to_string()]);
        assert_eq!(back.validators.len(), 1);
        assert!(back.enabled);
    }

    #[test]
    fn a_missing_project_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();
        match store.load("nope") {
            Err(RiptideError::NoSuchProject(n)) => assert_eq!(n, "nope"),
            other => panic!("expected NoSuchProject, got {other:?}"),
        }
    }

    #[test]
    fn project_names_are_confined_to_one_path_component() {
        for bad in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "a b",
            "sneaky\u{0}null",
            &"x".repeat(65),
        ] {
            assert!(
                validate_name(bad).is_err(),
                "{bad:?} should be rejected as a project name"
            );
        }
        for good in ["nightly", "feed.1", "my_site", "A-B_c.d"] {
            validate_name(good).unwrap_or_else(|e| panic!("{good:?} should be allowed: {e}"));
        }
    }

    #[test]
    fn a_rejected_name_never_reaches_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();
        let mut p = project("ok");
        p.name = "../escape".into();
        assert!(store.save(&p).is_err());
        assert_eq!(store.list().unwrap().len(), 0);
        assert!(!dir.path().parent().unwrap().join("escape.json").exists());
    }

    #[test]
    fn intervals_parse_and_have_a_floor() {
        assert_eq!(parse_interval("30m").unwrap(), 1800);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("1d").unwrap(), 86_400);
        assert_eq!(parse_interval("2h 30m").unwrap(), 9000);
        assert_eq!(parse_interval("90").unwrap(), 90, "bare seconds");

        assert!(parse_interval("0s").is_err(), "a zero interval is a loop");
        assert!(parse_interval("").is_err());
        assert!(parse_interval("now").is_err());
    }

    #[test]
    fn only_enabled_and_scheduled_projects_are_due() {
        let now = 1_000_000u64;
        let mut p = project("p");
        p.next_run_ms = Some(now - 1);
        assert!(p.is_due(now));
        p.next_run_ms = Some(now);
        assert!(p.is_due(now), "exactly due counts as due");
        p.next_run_ms = Some(now + 1);
        assert!(!p.is_due(now));

        p.next_run_ms = Some(now - 1);
        p.enabled = false;
        assert!(!p.is_due(now), "a disabled project never runs by itself");

        p.enabled = true;
        p.next_run_ms = None;
        assert!(
            !p.is_due(now),
            "an unscheduled project waits to be asked, rather than running on every tick"
        );
    }

    #[test]
    fn the_next_run_is_measured_from_the_end_of_the_run() {
        let mut p = project("p");
        p.interval_secs = 60;
        p.reschedule(5_000);
        assert_eq!(p.next_run_ms, Some(65_000));
        assert_eq!(p.updated_ms, 5_000);
    }

    #[test]
    fn a_run_slower_than_its_interval_does_not_queue_a_backlog() {
        // Interval 10s, run took 25s: the next run is 35s, not "now" (which catch-up
        // scheduling would produce, and which would then fire again immediately).
        let mut p = project("p");
        p.interval_secs = 10;
        p.next_run_ms = Some(0);
        assert!(p.is_due(0), "a project whose time has come is due");
        p.reschedule(25_000);
        assert_eq!(p.next_run_ms, Some(35_000));
        assert!(!p.is_due(26_000));
        assert!(p.is_due(35_000));
    }

    #[test]
    fn due_returns_the_earliest_first_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();
        let now = 5_000_000u64;

        let mut late = project("late");
        late.next_run_ms = Some(now + 60_000);
        let mut soon = project("soon");
        soon.next_run_ms = Some(now - 10);
        let mut also = project("also");
        also.next_run_ms = Some(now - 100_000);
        let mut off = project("off");
        off.next_run_ms = Some(now - 1);
        off.enabled = false;
        for p in [&late, &soon, &also, &off] {
            store.save(p).unwrap();
        }

        let due = store.due(now).unwrap();
        let names: Vec<&str> = due.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["also", "soon"],
            "earliest first, disabled excluded"
        );

        let all = store.list().unwrap();
        assert_eq!(
            all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["also", "late", "off", "soon"],
            "listing is by name"
        );
    }

    #[test]
    fn validators_become_the_right_conditional_headers() {
        let v = Validator {
            etag: Some("\"v1\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };
        let h = v.headers();
        assert_eq!(h[0].0, "If-None-Match");
        assert_eq!(h[0].1, "\"v1\"");
        assert_eq!(h[1].0, "If-Modified-Since");
        assert_eq!(h[1].1, "Wed, 21 Oct 2015 07:28:00 GMT");

        assert!(Validator::default().is_empty());
        assert!(!v.is_empty());
        // Only what we have: a server that gave us a Last-Modified date and no ETag.
        let lm = Validator {
            etag: None,
            last_modified: Some("x".into()),
        };
        assert_eq!(lm.headers().len(), 1);
    }

    #[test]
    fn an_empty_validator_serializes_away_but_is_not_confused_with_none() {
        let p = project("p");
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("validator"),
            "an empty validator is not worth persisting"
        );
        // ...and a project file written before validators existed still loads.
        let back: Project = serde_json::from_str(
            r#"{"name":"p","kind":"file","url":"http://x/","output":"/tmp/x",
                "interval_secs":60,"enabled":true,"created_ms":1,"updated_ms":1}"#,
        )
        .unwrap();
        assert_eq!(back.kind, ProjectKind::File);
        assert!(back.validator.is_empty());
        assert_eq!(back.connections, 8, "defaulted");
        assert_eq!(back.max_downloads, DEFAULT_MAX_DOWNLOADS);
        assert!(back.next_run_ms.is_none(), "an old file has no schedule");
    }

    #[test]
    fn defaults_are_written_out_so_a_project_keeps_its_behaviour() {
        // The defaults are serialized rather than omitted: a project created today should
        // keep running the same way if a future version changes a default.
        let p = project("p");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"connections\":8"));
        assert!(json.contains("\"max_downloads\":500"));
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connections, 8);
        assert_eq!(back.max_downloads, DEFAULT_MAX_DOWNLOADS);
    }

    #[test]
    fn a_url_without_a_filename_gets_one_derived_from_its_path() {
        let dir = Path::new("/tmp/out");
        assert_eq!(
            target_path(dir, "http://x/a/b.zip"),
            PathBuf::from("/tmp/out/b.zip")
        );
        assert_eq!(
            target_path(dir, "http://x/"),
            PathBuf::from("/tmp/out/x_index.html")
        );
        assert_eq!(
            target_path(dir, "http://x/docs/manual/"),
            PathBuf::from("/tmp/out/x_docs_manual.html")
        );
        // Two index pages on one host must not collide.
        assert_ne!(
            target_path(dir, "http://x/a/"),
            target_path(dir, "http://x/b/")
        );
    }

    #[test]
    fn schedule_summaries_read_as_a_human_wrote_them() {
        let mut p = project("p");
        p.next_run_ms = Some(1_000_000 + 30_000);
        assert_eq!(p.schedule_summary(1_000_000), "in 30s");
        p.next_run_ms = Some(1_000_000 + 25 * 60_000);
        assert_eq!(p.schedule_summary(1_000_000), "in 25m");
        p.next_run_ms = Some(1_000_000 + 90 * 60_000);
        assert_eq!(p.schedule_summary(1_000_000), "in 1h30m");
        p.next_run_ms = Some(1_000_000 + 3 * 86_400_000);
        assert_eq!(p.schedule_summary(1_000_000), "in 3d0h");
        p.next_run_ms = Some(999_999);
        assert_eq!(p.schedule_summary(1_000_000), "due");
        p.next_run_ms = None;
        assert_eq!(p.schedule_summary(1_000_000), "not scheduled");
        p.enabled = false;
        assert_eq!(p.schedule_summary(1_000_000), "disabled");
    }

    #[test]
    fn last_run_summaries_surface_repeated_failures() {
        let mut p = project("p");
        assert_eq!(p.last_run_summary(1_000_000), "never");

        p.last_run_ms = Some(1_000_000 - 120_000);
        p.last_status = Some("updated".into());
        assert_eq!(p.last_run_summary(1_000_000), "updated, 2m ago");

        // One failure is just a bad day; several in a row is a broken project.
        p.last_status = Some("failed".into());
        p.failures = 1;
        assert_eq!(p.last_run_summary(1_000_000), "failed, 2m ago");
        p.failures = 7;
        assert_eq!(p.last_run_summary(1_000_000), "failed x7, 2m ago");
    }

    #[test]
    fn outcomes_classify_quota_pauses_as_expected_work() {
        assert!(!RunOutcome::QuotaPaused.is_failure());
        assert!(!RunOutcome::Unchanged.is_failure());
        assert!(!RunOutcome::Updated.is_failure());
        assert!(
            !RunOutcome::Planned.is_failure(),
            "a planning run is not an error"
        );
        assert!(RunOutcome::Partial.is_failure());
        assert!(RunOutcome::Failed.is_failure());
        assert_eq!(RunOutcome::QuotaPaused.as_str(), "quota-paused");
        assert_eq!(RunOutcome::Planned.as_str(), "planned");
        // Only one outcome means the local copy was replaced.
        assert!(RunOutcome::Updated.wrote_bytes());
        assert!(!RunOutcome::Planned.wrote_bytes());
        assert!(!RunOutcome::Unchanged.wrote_bytes());
    }

    #[test]
    fn a_site_project_defaults_to_planning_rather_than_fetching() {
        let p = Project::new(
            "s",
            ProjectKind::Site,
            "http://x/",
            PathBuf::from("/tmp/site"),
            86_400,
        );
        assert!(!p.download, "a site project must be told to fetch");
        assert_eq!(p.grab.depth, 1);
        assert!(p.grab.same_host, "staying on the host is the safer default");
        assert!(p.grab.media_only);
    }

    #[test]
    fn an_unparseable_project_file_does_not_break_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();
        store.save(&project("good")).unwrap();
        std::fs::write(dir.path().join("broken.json"), b"{ not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hi").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn delete_is_idempotent_and_confined() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path()).unwrap();
        store.save(&project("gone")).unwrap();
        store.delete("gone").unwrap();
        store.delete("gone").unwrap();
        assert!(!store.exists("gone"));
        assert!(
            store.delete("../x").is_err(),
            "a traversal attempt is refused, not attempted"
        );
    }

    #[test]
    fn job_kind_is_inferred_the_same_way_for_projects_as_for_downloads() {
        // A project pointed at a manifest must go through the media pipeline, which is
        // `JobKind::guess`'s job; nothing project-specific may re-implement it.
        assert_eq!(JobKind::guess("http://x/a.m3u8"), JobKind::Hls);
        assert_eq!(JobKind::guess("http://x/a.mpd"), JobKind::Dash);
        assert_eq!(JobKind::guess("ftp://x/a.bin"), JobKind::Ftp);
        assert_eq!(JobKind::guess("http://x/a.bin"), JobKind::Http);
    }
}
