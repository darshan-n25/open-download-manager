//! The segmented HTTP/HTTPS download engine.
//!
//! Structure mirrors what the reverse engineering of IDM found:
//!
//! * `Range: bytes=<start>-<end>` per connection, `If-Range` revalidation on resume;
//! * a mutable segment table that idle workers carve up at runtime, standing in for
//!   IDM's `ChLst` + `Change chunk size to %ld`;
//! * per-connection timeout/retry/restart with backoff, standing in for IDM's
//!   `Connection restarted because of timeout (1)|(2)`;
//! * independent global and per-file bandwidth ceilings, plus a process-wide
//!   bytes-per-window budget;
//! * direct positional writes, so there is no end-of-download assembly pass.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    IF_RANGE, LAST_MODIFIED, WWW_AUTHENTICATE,
};
use reqwest::{Method, StatusCode};
use tokio::sync::{Notify, OwnedSemaphorePermit};

use crate::auth::{self, AuthKind, DigestSession};
use crate::config::Config;
use crate::error::{Result, RiptideError};
use crate::job::{Job, JobKind, JobState, Segment, SegmentState};
use crate::net::{NetClient, ServerLimits};
use crate::quota::Quota;
use crate::segment as segplan;
use crate::storage::Sink;
use crate::throttle::RateLimiter;

const ST_PENDING: u8 = 0;
const ST_RUNNING: u8 = 1;
const ST_DONE: u8 = 2;
const ST_FAILED: u8 = 3;

/// One sample handed to the progress callback.
#[derive(Clone, Copy, Debug)]
pub struct ProgressSample {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
}

pub type ProgressFn = Arc<dyn Fn(ProgressSample) + Send + Sync>;

/// A live segment. `end` is mutable so a worker can shorten it while another worker
/// takes the tail; `done` is the byte count already committed for this segment.
struct SegHandle {
    id: u64,
    start: u64,
    end: AtomicU64,
    done: AtomicU64,
    attempts: AtomicU32,
    state: AtomicU8,
}

impl SegHandle {
    fn new(id: u64, start: u64, end: u64) -> Arc<Self> {
        Arc::new(SegHandle {
            id,
            start,
            end: AtomicU64::new(end),
            done: AtomicU64::new(0),
            attempts: AtomicU32::new(0),
            state: AtomicU8::new(ST_PENDING),
        })
    }

    fn from_segment(s: &Segment) -> Arc<Self> {
        let h = SegHandle::new(s.id, s.start, s.end);
        h.done.store(s.done, Ordering::SeqCst);
        h.attempts.store(s.attempts, Ordering::SeqCst);
        h.state.store(
            match s.state {
                SegmentState::Done => ST_DONE,
                SegmentState::Failed => ST_FAILED,
                _ => ST_PENDING,
            },
            Ordering::SeqCst,
        );
        h
    }

    fn cursor(&self) -> u64 {
        self.start + self.done.load(Ordering::SeqCst)
    }

    fn remaining(&self) -> Option<u64> {
        let end = self.end.load(Ordering::SeqCst);
        if end == u64::MAX {
            return None;
        }
        let c = self.cursor();
        if c > end {
            Some(0)
        } else {
            Some(end - c + 1)
        }
    }

    fn to_segment(&self) -> Segment {
        Segment {
            id: self.id,
            start: self.start,
            end: self.end.load(Ordering::SeqCst),
            done: self.done.load(Ordering::SeqCst),
            state: match self.state.load(Ordering::SeqCst) {
                ST_DONE => SegmentState::Done,
                ST_FAILED => SegmentState::Failed,
                ST_RUNNING => SegmentState::Running,
                _ => SegmentState::Pending,
            },
            attempts: self.attempts.load(Ordering::SeqCst),
        }
    }
}

/// Shared, lock-light state for one download's workers.
struct RunState {
    handles: Mutex<BTreeMap<u64, Arc<SegHandle>>>,
    pending: Mutex<VecDeque<u64>>,
    next_id: AtomicU64,
    done_bytes: AtomicU64,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    /// Set when the run must stop for a reason that is *not* a segment failure. Today the
    /// only cause is a spent download quota. It is deliberately separate from `failed`:
    /// the segments keep their progress, because the job must stay resumable and will run
    /// again once the window refills.
    stop: Mutex<Option<crate::quota::QuotaExhausted>>,
    notify: Notify,
}

impl RunState {
    fn new() -> Arc<Self> {
        Arc::new(RunState {
            handles: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
            done_bytes: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            stop: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    /// Ask every worker to stand down without failing anything.
    fn stop(&self, why: crate::quota::QuotaExhausted) {
        let mut s = self.stop.lock().unwrap_or_else(|p| p.into_inner());
        if s.is_none() {
            *s = Some(why);
        }
        drop(s);
        self.notify.notify_waiters();
    }

    fn is_stopped(&self) -> bool {
        self.stop
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    fn stop_reason(&self) -> Option<crate::quota::QuotaExhausted> {
        *self.stop.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Arc<SegHandle>>> {
        self.handles.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bytes committed to disk across every live segment.
    ///
    /// Summing `done` (rather than recomputing from `cursor`) is what makes this correct
    /// under runtime re-splitting: a tail segment created by a split starts at `done == 0`,
    /// so bytes are never counted twice even while the original is being shortened.
    fn total_done(&self) -> u64 {
        self.handles()
            .values()
            .map(|h| h.done.load(Ordering::SeqCst))
            .sum()
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<u64>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add(&self, handles: Vec<Arc<SegHandle>>) {
        let mut map = self.handles();
        let mut queue = self.pending();
        for h in handles {
            let id = h.id;
            if h.state.load(Ordering::SeqCst) != ST_DONE {
                queue.push_back(id);
            }
            map.insert(id, h);
        }
        drop(queue);
        drop(map);
        self.notify.notify_waiters();
    }

    /// Take the next pending segment, marking it Running.
    fn claim(&self) -> Option<Arc<SegHandle>> {
        loop {
            let id = self.pending().pop_front()?;
            let h = self.handles().get(&id).cloned();
            let h = match h {
                Some(h) => h,
                None => continue,
            };
            if h.state.load(Ordering::SeqCst) == ST_DONE {
                continue;
            }
            h.state.store(ST_RUNNING, Ordering::SeqCst);
            return Some(h);
        }
    }

    /// Re-split the largest outstanding remainder. Called by a worker that ran out of
    /// work, which is exactly when extra parallelism is worth having.
    fn try_split(&self, split_threshold: u64, min_chunk: u64) -> bool {
        let victim = {
            let map = self.handles();
            map.values()
                .filter(|h| h.state.load(Ordering::SeqCst) != ST_DONE)
                .filter_map(|h| {
                    let rem = h.remaining()?;
                    if rem >= split_threshold && rem >= min_chunk.saturating_mul(2) {
                        Some((rem, h.clone()))
                    } else {
                        None
                    }
                })
                .max_by_key(|(rem, _)| *rem)
                .map(|(_, h)| h)
        };

        let Some(victim) = victim else { return false };
        let snap = victim.to_segment();
        let Some((mut tail, new_end)) = segplan::split_off(&snap, min_chunk) else {
            return false;
        };
        // Shorten the victim first. Any worker already streaming it re-reads `end` before
        // each write, so it will stop at the new boundary instead of racing the tail.
        victim.end.store(new_end, Ordering::SeqCst);
        tail.id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let tail_handle = SegHandle::new(tail.id, tail.start, tail.end);
        self.add(vec![tail_handle]);
        true
    }

    fn all_done(&self) -> bool {
        self.handles()
            .values()
            .all(|h| h.state.load(Ordering::SeqCst) == ST_DONE)
    }

    fn any_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    fn fail(&self, msg: impl Into<String>) {
        let mut e = self.error.lock().unwrap_or_else(|p| p.into_inner());
        if e.is_none() {
            *e = Some(msg.into());
        }
        self.failed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn error_message(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn snapshot(&self) -> Vec<Segment> {
        let mut v: Vec<Segment> = self.handles().values().map(|h| h.to_segment()).collect();
        v.sort_by_key(|s| s.start);
        v
    }
}

/// Everything a worker needs. Deliberately not the `Engine` itself, so workers can be
/// plain `tokio::spawn`ed futures.
struct Shared {
    client: Arc<NetClient>,
    url: String,
    validator: Option<String>,
    auth_override: Option<String>,
    read_timeout: Duration,
    retry_backoff: Duration,
    max_retries: u32,
    split_threshold: u64,
    min_chunk: u64,
    global: Arc<RateLimiter>,
    local: Arc<RateLimiter>,
    servers: Arc<ServerLimits>,
    quota: Arc<Quota>,
    cancel: Arc<AtomicBool>,
}

impl Shared {
    fn request(&self, method: Method) -> reqwest::RequestBuilder {
        let mut rb = self.client.raw().request(method, &self.url);
        if let Some(a) = &self.auth_override {
            rb = rb.header("Authorization", a);
        }
        rb
    }

    /// Account for `n` bytes before they are written.
    ///
    /// The quota is tested first because it is cheap, synchronous, and a spent budget
    /// should fail the transfer at once rather than after waiting for bandwidth the
    /// process is not allowed to use.
    async fn charge(&self, n: u64) -> Result<()> {
        self.quota.charge(n)?;
        self.global.acquire(n).await;
        self.local.acquire(n).await;
        Ok(())
    }
}

pub struct Engine {
    pub cfg: Config,
    pub job: Job,
    client: Arc<NetClient>,
    global: Arc<RateLimiter>,
    local: Arc<RateLimiter>,
    /// Simultaneous connections allowed to any one host, shared with every other job in the
    /// process ("Max. connections number for a server").
    servers: Arc<ServerLimits>,
    /// Bytes-per-window budget, shared with every other job in the process ("Download
    /// limits"). Never per job: the whole point is that the *machine* has a budget.
    quota: Arc<Quota>,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressFn>,
    /// Negotiated Digest state, reused across requests that share a nonce.
    digest: Option<DigestSession>,
    auth_override: Option<String>,
}

/// Process-wide ceilings shared by every job.
///
/// Built once by the [`Scheduler`](crate::scheduler::Scheduler) and handed to each
/// [`Engine`], so "Maximum download speed", "Max. connections number for a server" and
/// "Download limits" are properties of the process rather than of one download. Every
/// field is optional: an `Engine` built without them derives them from its own `Config`
/// (which is what `probe` and `grab` do), so even a one-off crawl is charged against the
/// same budget as the queue.
#[derive(Clone, Default)]
pub struct SharedLimits {
    pub global: Option<Arc<RateLimiter>>,
    pub servers: Option<Arc<ServerLimits>>,
    pub quota: Option<Arc<Quota>>,
}

impl Engine {
    pub fn new(cfg: Config, job: Job) -> Result<Self> {
        Self::with_shared(cfg, job, SharedLimits::default())
    }

    pub fn with_shared(cfg: Config, job: Job, limits: SharedLimits) -> Result<Self> {
        let client = Arc::new(NetClient::build(
            &job.ctx,
            &cfg.user_agent,
            Duration::from_secs(cfg.connect_timeout_secs),
        )?);
        let global = limits
            .global
            .unwrap_or_else(|| Arc::new(RateLimiter::new(cfg.speed_limit)));
        let local = Arc::new(RateLimiter::new(job.speed_limit));
        let servers = limits
            .servers
            .unwrap_or_else(|| Arc::new(ServerLimits::new(cfg.per_server_connections)));
        let quota = limits
            .quota
            .unwrap_or_else(|| Arc::new(cfg.persistent_quota()));
        Ok(Engine {
            cfg,
            job,
            client,
            global,
            local,
            servers,
            quota,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: None,
            digest: None,
            auth_override: None,
        })
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    pub fn job_mut(&mut self) -> &mut Job {
        &mut self.job
    }

    /// A cheap, cloneable handle for fetching auxiliary resources (manifest side files,
    /// media segments) — including from several tasks at once.
    pub fn fetcher(&self) -> Fetcher {
        Fetcher {
            client: self.client.clone(),
            auth: self.auth_override.clone(),
            read_timeout: Duration::from_secs(self.cfg.read_timeout_secs),
            global: self.global.clone(),
            local: self.local.clone(),
            servers: self.servers.clone(),
            quota: self.quota.clone(),
            cancel: self.cancel.clone(),
        }
    }

    pub fn client(&self) -> &Arc<NetClient> {
        &self.client
    }

    /// Handle used to ask the running download to stop.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Account for `n` bytes against every ceiling and the process budget.
    ///
    /// Exposed for the FTP path, whose transfer loop owns its own socket and so cannot
    /// use [`Fetcher`]. Returns the quota error when the window's budget is spent, which
    /// leaves the job resumable.
    pub async fn charge_bytes(&self, n: u64) -> Result<()> {
        self.quota.charge(n)?;
        self.global.acquire(n).await;
        self.local.acquire(n).await;
        Ok(())
    }

    pub fn on_progress(&mut self, f: ProgressFn) {
        self.progress = Some(f);
    }

    pub fn progress_cb(&self) -> Option<ProgressFn> {
        self.progress.clone()
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    fn request_uri(url: &str) -> String {
        url::Url::parse(url)
            .map(|u| match u.query() {
                Some(q) => format!("{}?{}", u.path(), q),
                None => u.path().to_string(),
            })
            .unwrap_or_else(|_| url.to_string())
    }

    /// Send a request, transparently completing a Basic/Bearer/Digest/NTLM challenge.
    async fn send_authed(
        &mut self,
        method: Method,
        url: &str,
        extra: &[(String, String)],
    ) -> Result<reqwest::Response> {
        for _ in 0..3 {
            let mut rb = self.client.raw().request(method.clone(), url);
            for (k, v) in extra {
                rb = rb.header(k, v);
            }
            if let Some(a) = &self.auth_override {
                rb = rb.header("Authorization", a);
            }
            let resp = rb.send().await?;

            if resp.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                return Err(RiptideError::AuthFailed(
                    "proxy authentication required (set credentials in the proxy URL)".into(),
                ));
            }
            if resp.status() != StatusCode::UNAUTHORIZED {
                return Ok(resp);
            }

            let Some(spec) = self.job.ctx.auth.clone() else {
                return Err(RiptideError::AuthRequired {
                    scheme: "unknown".into(),
                });
            };
            // A server commonly advertises several schemes at once — `Basic` and
            // `Negotiate`, or `Negotiate` and `NTLM` — so answer the one this job was
            // configured for rather than whichever happens to be listed first.
            let headers = www_authenticate_headers(&resp);
            let wanted = match spec.kind {
                AuthKind::Basic => "basic",
                AuthKind::Digest => "digest",
                AuthKind::Bearer => "bearer",
                AuthKind::Ntlm => "ntlm",
                AuthKind::Negotiate => "negotiate",
            };
            let challenge = headers
                .iter()
                .find(|h| auth::scheme_of(h).as_deref() == Some(wanted))
                .or_else(|| headers.first())
                .cloned()
                .unwrap_or_default();

            match spec.kind {
                AuthKind::Digest => {
                    let Some(chal) = auth::parse_digest_challenge(&challenge) else {
                        return Err(RiptideError::AuthRequired {
                            scheme: "digest".into(),
                        });
                    };
                    let session = self.digest.get_or_insert_with(DigestSession::new);
                    let value = auth::digest_authorization(
                        &chal,
                        spec.user(),
                        spec.pass(),
                        method.as_str(),
                        &Self::request_uri(url),
                        session,
                        b"",
                    );
                    self.auth_override = Some(value);
                }
                AuthKind::Ntlm => {
                    self.auth_override = Some(
                        ntlm_exchange(&self.client, url, &spec, extra, AuthExchange::Ntlm).await?,
                    );
                }
                AuthKind::Negotiate => {
                    self.auth_override = Some(
                        ntlm_exchange(&self.client, url, &spec, extra, AuthExchange::Spnego)
                            .await?,
                    );
                }
                AuthKind::Basic | AuthKind::Bearer => {
                    // Both are preemptive; a 401 means the credentials are simply wrong.
                    return Err(RiptideError::AuthFailed(format!(
                        "{:?} credentials rejected",
                        spec.kind
                    )));
                }
            }
        }
        Err(RiptideError::AuthFailed(
            "authentication did not converge after 3 attempts".into(),
        ))
    }

    // -----------------------------------------------------------------------
    // Probe
    // -----------------------------------------------------------------------

    /// Discover size, range support, validators and the real filename.
    pub async fn probe(&mut self) -> Result<()> {
        let url = self.job.url.clone();
        let identity = [("Accept-Encoding".to_string(), "identity".to_string())];

        // A probe is a real connection too; count it against the host's ceiling.
        let _slot = self.servers.acquire(&url).await;

        // A HEAD is cheaper, but plenty of CDNs reject it, so fall back to a 1-byte GET.
        let mut used_get = false;
        let mut resp = self.send_authed(Method::HEAD, &url, &identity).await?;
        if resp.status().is_client_error() || resp.status() == StatusCode::METHOD_NOT_ALLOWED {
            resp = self
                .send_authed(
                    Method::GET,
                    &url,
                    &[
                        ("Accept-Encoding".to_string(), "identity".to_string()),
                        ("Range".to_string(), "bytes=0-0".to_string()),
                    ],
                )
                .await?;
            used_get = true;
        }

        let status = resp.status();
        // A conditional request answered `304 Not Modified`: the copy on disk is current.
        // There is no body and no size to learn, so record that and stop — the caller
        // (a synchronization project) decides that there is nothing to do.
        if status == StatusCode::NOT_MODIFIED {
            self.job.not_modified = true;
            drop(resp);
            return Ok(());
        }
        if status == StatusCode::NOT_FOUND {
            return Err(RiptideError::other(format!("404 Not Found: {url}")));
        }
        if !status.is_success() {
            return Err(RiptideError::other(format!(
                "server returned HTTP {status} for {url}"
            )));
        }

        let final_url = resp.url().to_string();
        let headers = resp.headers().clone();

        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let etag = headers
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = headers
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let accept_ranges = headers
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

        let mut total: Option<u64> = None;
        if used_get {
            if status == StatusCode::PARTIAL_CONTENT {
                total = headers
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range_total);
            } else if status == StatusCode::OK {
                total = headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
            }
        } else {
            total = headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
        }

        let disposition_filename = headers
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(crate::util::filename_from_disposition);

        // Everything borrowed from `headers`/`resp` must be released before we mutate self.
        drop(resp);

        self.job.resolved_url = Some(final_url);
        self.job.total = total;
        self.job.etag = etag;
        self.job.last_modified = last_modified;
        self.job.content_type = content_type.clone();
        // A 206 to a 1-byte range probe is itself proof the server honours ranges, even
        // if it forgot to advertise Accept-Ranges.
        self.job.resumable =
            total.is_some() && (accept_ranges || used_get && status == StatusCode::PARTIAL_CONTENT);

        // Resolve a provisional output path now that the name is known.
        if self.job.output_provisional {
            let ext_from_url = crate::util::filename_from_url(&self.job.url).and_then(|n| {
                Path::new(&n)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(String::from)
            });
            let ext_from_mime = content_type
                .as_deref()
                .and_then(crate::util::extension_from_mime)
                .map(String::from);
            let ext = ext_from_url.or(ext_from_mime);
            let ext_for_name = ext.clone();
            let name = disposition_filename
                .clone()
                .or_else(|| crate::util::filename_from_url(&self.job.url))
                .map(|mut n| {
                    if Path::new(&n).extension().is_none() {
                        if let Some(e) = &ext_for_name {
                            n.push('.');
                            n.push_str(e);
                        }
                    }
                    n
                })
                .unwrap_or_else(|| {
                    format!("download.{}", ext.clone().unwrap_or_else(|| "bin".into()))
                });
            self.job.output = self.cfg.resolve_output(None, &name, ext.as_deref());
            self.job.output_provisional = false;
        }

        // Route manifest content to the media pipeline.
        if self.job.kind == JobKind::Http {
            let lower_url = self.job.url.to_ascii_lowercase();
            let mime = content_type
                .as_deref()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if mime.contains("mpegurl") || lower_url.contains(".m3u8") {
                self.job.kind = JobKind::Hls;
            } else if mime.contains("dash+xml") || lower_url.contains(".mpd") {
                self.job.kind = JobKind::Dash;
            }
        }
        self.job.touch();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Run
    // -----------------------------------------------------------------------

    pub async fn run(&mut self) -> Result<()> {
        self.job.state = JobState::Active;
        self.job.touch();

        // A conditional request that the server answered `304 Not Modified` already told us
        // the local copy is current; `probe` recorded it. There is nothing to transfer, and
        // reporting `Done` is the honest answer: the job's goal is met.
        if self.job.not_modified {
            self.job.state = JobState::Done;
            self.job.error = None;
            self.job.quota_paused = false;
            self.job.touch();
            return Ok(());
        }

        let result = match self.job.kind {
            JobKind::Hls | JobKind::Dash => crate::media::download(self).await,
            JobKind::Ftp => crate::ftp::download(self).await,
            _ => {
                if self.job.resumable && self.job.total.is_some() {
                    match self.run_segmented().await {
                        Err(RiptideError::NoRangeSupport) => {
                            tracing::info!(
                                "server ignored Range mid-flight; falling back to a single stream"
                            );
                            self.run_single().await
                        }
                        other => other,
                    }
                } else {
                    self.run_single().await
                }
            }
        };

        match result {
            Ok(()) => {
                self.job.state = JobState::Done;
                self.job.error = None;
                self.job.quota_paused = false;
                self.job.touch();
                Ok(())
            }
            Err(e) => {
                let quota = matches!(e, RiptideError::QuotaExhausted(_));
                self.job.state = if self.cancel.load(Ordering::SeqCst) || quota {
                    JobState::Paused
                } else {
                    JobState::Failed
                };
                self.job.quota_paused = quota;
                self.job.error = Some(e.to_string());
                self.job.touch();
                Err(e)
            }
        }
    }

    fn shared(&self) -> Shared {
        Shared {
            client: self.client.clone(),
            url: self
                .job
                .resolved_url
                .clone()
                .unwrap_or_else(|| self.job.url.clone()),
            validator: self
                .job
                .etag
                .clone()
                .or_else(|| self.job.last_modified.clone()),
            auth_override: self.auth_override.clone(),
            read_timeout: Duration::from_secs(self.cfg.read_timeout_secs),
            retry_backoff: Duration::from_millis(self.cfg.retry_backoff_ms),
            max_retries: self.job.max_retries,
            split_threshold: self.cfg.split_threshold,
            min_chunk: self.job.min_chunk,
            global: self.global.clone(),
            local: self.local.clone(),
            servers: self.servers.clone(),
            quota: self.quota.clone(),
            cancel: self.cancel.clone(),
        }
    }

    /// Seed the segment table: resume the stored plan, or plan a fresh split.
    fn build_state(&self, total: u64) -> Arc<RunState> {
        let st = RunState::new();
        let resume = !self.job.segments.is_empty()
            && self
                .job
                .segments
                .iter()
                .all(|s| s.end == u64::MAX || s.end < total)
            && self.job.segments.iter().any(|s| !s.is_complete());

        let handles: Vec<Arc<SegHandle>> = if resume {
            let mut v: Vec<_> = self
                .job
                .segments
                .iter()
                .map(SegHandle::from_segment)
                .collect();
            v.sort_by_key(|h| h.start);
            v
        } else {
            segplan::plan(total, self.job.connections, self.job.min_chunk)
                .iter()
                .map(|s| SegHandle::new(s.id, s.start, s.end))
                .collect()
        };

        let done: u64 = handles
            .iter()
            .filter(|h| h.state.load(Ordering::SeqCst) == ST_DONE)
            .map(|h| h.end.load(Ordering::SeqCst).saturating_sub(h.start) + 1)
            .sum();
        st.done_bytes.store(done, Ordering::SeqCst);
        st.next_id.store(handles.len() as u64, Ordering::SeqCst);
        st.add(handles);
        st
    }

    async fn run_segmented(&mut self) -> Result<()> {
        let total = self.job.total.ok_or(RiptideError::NoRangeSupport)?;
        let part = self.job.part_path();
        let sink = Arc::new(Sink::create(&part, Some(total))?);

        let st = self.build_state(total);
        let shared = Arc::new(self.shared());
        let workers = self.job.connections.clamp(1, 64);

        let speed = self.spawn_monitor(st.clone(), total);

        let mut tasks = Vec::with_capacity(workers);
        for _ in 0..workers {
            let st = st.clone();
            let shared = shared.clone();
            let sink = sink.clone();
            tasks.push(tokio::spawn(async move {
                worker_loop(st, shared, sink).await;
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
        speed.store(false, Ordering::SeqCst);
        st.notify.notify_waiters();

        self.job.segments = st.snapshot();
        // Live progress, not just what was already on disk: `total_done` reads the working
        // handles, so a failed or cancelled run records the bytes it really fetched.
        self.job.downloaded = st.total_done();
        st.done_bytes.store(self.job.downloaded, Ordering::SeqCst);

        // A spent budget outranks everything else here: the job is neither failed nor
        // finished, it is simply waiting for the window to refill.
        if let Some(q) = st.stop_reason() {
            self.emit_progress();
            return Err(RiptideError::QuotaExhausted(q));
        }
        if let Some(msg) = st.error_message() {
            self.emit_progress();
            return Err(RiptideError::other(msg));
        }
        if self.cancel.load(Ordering::SeqCst) {
            self.emit_progress();
            return Err(RiptideError::Cancelled);
        }
        if !st.all_done() {
            return Err(RiptideError::other(
                "download ended with unfinished segments",
            ));
        }

        self.job.downloaded = total;
        self.emit_progress();
        sink.sync().await?;
        sink.truncate_to(total)?;
        sink.finalize(&self.job.output)?;
        Ok(())
    }

    /// Single-connection mode, used when the server will not honour ranges.
    async fn run_single(&mut self) -> Result<()> {
        let part = self.job.part_path();
        // No range support means no safe resume: restart from zero.
        self.job.downloaded = 0;
        self.job.segments.clear();
        let sink = Sink::create(&part, None)?;
        sink.truncate_to(0)?;

        let shared = self.shared();
        let mut offset = 0u64;
        let mut attempts = 0u32;

        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(RiptideError::Cancelled);
            }
            let extra = vec![("Accept-Encoding".to_string(), "identity".to_string())];
            // Held for the whole response, including the body loop below.
            let _slot = self.servers.acquire(&self.job.url.clone()).await;
            let resp = match self
                .send_authed(Method::GET, &self.job.url.clone(), &extra)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    attempts += 1;
                    if attempts > self.job.max_retries {
                        return Err(e);
                    }
                    tokio::time::sleep(shared.retry_backoff * attempts).await;
                    continue;
                }
            };

            if resp.status() == StatusCode::PARTIAL_CONTENT {
                // We asked for no range and got one; treat as fatal confusion.
                return Err(RiptideError::other("unexpected 206 in single-stream mode"));
            }
            if !resp.status().is_success() {
                return Err(RiptideError::other(format!(
                    "HTTP {} in single-stream mode",
                    resp.status()
                )));
            }

            let total = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            self.job.total = total;

            let mut stream = resp.bytes_stream();
            let mut ok = false;
            loop {
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(RiptideError::Cancelled);
                }
                let next = tokio::time::timeout(shared.read_timeout, stream.next()).await;
                match next {
                    Err(_) => {
                        // Idle read timeout: IDM's "Connection restarted because of timeout".
                        tracing::warn!("read timeout after {} bytes, restarting", offset);
                        break;
                    }
                    Ok(None) => {
                        ok = true;
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(error = %e, "stream error, restarting");
                        break;
                    }
                    Ok(Some(Ok(chunk))) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        let n = chunk.len() as u64;
                        shared.charge(n).await?;
                        sink.write_at(chunk, offset).await?;
                        offset += n;
                        self.job.downloaded = offset;
                    }
                }
            }

            if ok {
                break;
            }
            attempts += 1;
            if attempts > self.job.max_retries {
                return Err(RiptideError::other(
                    "too many restarts in single-stream mode",
                ));
            }
            if self.job.total.is_some() {
                // The server told us the length, so we can resume with a Range header even
                // though it never advertised Accept-Ranges.
                tracing::info!("resuming single stream from byte {offset}");
                let resp = self
                    .send_authed(
                        Method::GET,
                        &self.job.url.clone(),
                        &[
                            ("Accept-Encoding".to_string(), "identity".to_string()),
                            ("Range".to_string(), format!("bytes={offset}-")),
                        ],
                    )
                    .await?;
                if resp.status() == StatusCode::PARTIAL_CONTENT {
                    continue;
                }
            }
            return Err(RiptideError::other("stream interrupted and cannot resume"));
        }

        if let Some(total) = self.job.total {
            if offset > total {
                sink.truncate_to(total)?;
            }
            self.job.downloaded = total;
        } else {
            self.job.total = Some(offset);
            self.job.downloaded = offset;
        }
        self.emit_progress();
        sink.sync().await?;
        sink.finalize(&self.job.output)?;
        Ok(())
    }

    fn spawn_monitor(&self, st: Arc<RunState>, total: u64) -> Arc<AtomicBool> {
        let running = Arc::new(AtomicBool::new(true));
        let Some(cb) = self.progress.clone() else {
            return running;
        };
        let r = running.clone();
        tokio::spawn(async move {
            let mut last = (std::time::Instant::now(), 0u64);
            while r.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let now = std::time::Instant::now();
                let got = st.total_done();
                let dt = now.duration_since(last.0).as_secs_f64();
                let speed = if dt > 0.0 {
                    (got.saturating_sub(last.1)) as f64 / dt
                } else {
                    0.0
                };
                last = (now, got);
                cb(ProgressSample {
                    downloaded: got,
                    total: Some(total),
                    speed_bps: speed,
                });
            }
        });
        running
    }

    fn emit_progress(&self) {
        if let Some(cb) = &self.progress {
            cb(ProgressSample {
                downloaded: self.job.downloaded,
                total: self.job.total,
                speed_bps: 0.0,
            });
        }
    }

    /// Fetch a whole small resource into memory (manifests, encryption keys).
    /// Refuses to buffer more than `max` bytes.
    pub async fn fetch_bytes(&mut self, url: &str, max: u64) -> Result<Vec<u8>> {
        self.fetcher().to_bytes(url, max).await
    }

    /// Stream a resource into a file (used for media segments). Returns bytes written.
    pub async fn fetch_to_file(&mut self, url: &str, dest: &Path) -> Result<u64> {
        self.fetcher().to_file(url, dest).await
    }
}

/// Cloneable fetch handle. This is what makes concurrent HLS/DASH segment download
/// possible without holding a mutable borrow of the [`Engine`].
#[derive(Clone)]
pub struct Fetcher {
    client: Arc<NetClient>,
    auth: Option<String>,
    read_timeout: Duration,
    global: Arc<RateLimiter>,
    local: Arc<RateLimiter>,
    servers: Arc<ServerLimits>,
    quota: Arc<Quota>,
    cancel: Arc<AtomicBool>,
}

impl Fetcher {
    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut rb = self.client.raw().get(url);
        if let Some(a) = &self.auth {
            rb = rb.header("Authorization", a);
        }
        rb
    }

    /// Reserve a connection slot for `url`'s host. The permit is held for the whole response,
    /// which is what makes "Max. connections number for a server" mean simultaneous sockets.
    async fn permit(&self, url: &str) -> Option<OwnedSemaphorePermit> {
        self.servers.acquire(url).await
    }

    /// Account for `n` bytes before they are written; see [`Shared::charge`].
    async fn charge(&self, n: u64) -> Result<()> {
        self.quota.charge(n)?;
        self.global.acquire(n).await;
        self.local.acquire(n).await;
        Ok(())
    }

    pub async fn to_bytes(&self, url: &str, max: u64) -> Result<Vec<u8>> {
        let _slot = self.permit(url).await;
        let resp = self.request(url).send().await?;
        if !resp.status().is_success() {
            return Err(RiptideError::other(format!(
                "HTTP {} fetching {url}",
                resp.status()
            )));
        }
        if let Some(len) = resp.content_length() {
            if len > max {
                return Err(RiptideError::other(format!(
                    "refusing to buffer {len} bytes from {url} (limit {max})"
                )));
            }
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() as u64 + chunk.len() as u64 > max {
                return Err(RiptideError::other(format!(
                    "resource {url} exceeds {max} bytes"
                )));
            }
            self.charge(chunk.len() as u64).await?;
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    pub async fn to_file(&self, url: &str, dest: &Path) -> Result<u64> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(RiptideError::Cancelled);
        }
        let _slot = self.permit(url).await;
        let resp = self.request(url).send().await?;
        if !resp.status().is_success() {
            return Err(RiptideError::other(format!(
                "HTTP {} fetching {url}",
                resp.status()
            )));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = dest.with_extension("tmpdl");
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written = 0u64;
        let mut stream = resp.bytes_stream();
        loop {
            match tokio::time::timeout(self.read_timeout, stream.next()).await {
                Err(_) => return Err(RiptideError::other(format!("read timeout on {url}"))),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(Some(Ok(chunk))) => {
                    use tokio::io::AsyncWriteExt;
                    let n = chunk.len() as u64;
                    self.charge(n).await?;
                    file.write_all(&chunk).await?;
                    written += n;
                }
            }
        }
        {
            use tokio::io::AsyncWriteExt;
            file.flush().await?;
        }
        drop(file);
        tokio::fs::rename(&tmp, dest).await?;
        Ok(written)
    }

    /// Fetch a byte range of a resource fully into memory. Used for HLS
    /// `#EXT-X-BYTERANGE` segments, where one file holds many segments.
    pub async fn to_bytes_range(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        max: u64,
    ) -> Result<Vec<u8>> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(RiptideError::Cancelled);
        }
        let mut rb = self.request(url);
        if let Some((start, end)) = range {
            rb = rb.header("Range", format!("bytes={start}-{end}"));
        }
        rb = rb.header("Accept-Encoding", "identity");
        let _slot = self.permit(url).await;
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            return Err(RiptideError::other(format!(
                "HTTP {} fetching {url}",
                resp.status()
            )));
        }
        let mut buf: Vec<u8> = Vec::with_capacity(
            range
                .map(|(a, b)| (b - a + 1) as usize)
                .unwrap_or(64 * 1024),
        );
        let mut stream = resp.bytes_stream();
        loop {
            match tokio::time::timeout(self.read_timeout, stream.next()).await {
                Err(_) => return Err(RiptideError::other(format!("read timeout on {url}"))),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(Some(Ok(chunk))) => {
                    if buf.len() as u64 + chunk.len() as u64 > max {
                        return Err(RiptideError::other(format!(
                            "segment {url} exceeds {max} bytes"
                        )));
                    }
                    buf.extend_from_slice(&chunk);
                }
            }
        }
        Ok(buf)
    }
}

/// Parse the total out of `Content-Range: bytes 0-1/12345`.
pub fn parse_content_range_total(v: &str) -> Option<u64> {
    let after = v.split('/').nth(1)?;
    let t = after.trim();
    if t == "*" {
        None
    } else {
        t.parse::<u64>().ok()
    }
}

/// Longest allowed idle gap in the middle of a segment before we call it closed.
async fn worker_loop(st: Arc<RunState>, shared: Arc<Shared>, sink: Arc<Sink>) {
    loop {
        if shared.cancel.load(Ordering::SeqCst) || st.any_failed() || st.is_stopped() {
            return;
        }
        if let Some(h) = st.claim() {
            transfer_segment(&st, &shared, &sink, h).await;
            continue;
        }
        // No queued work: try to manufacture some by halving the biggest remainder.
        if st.try_split(shared.split_threshold, shared.min_chunk) {
            continue;
        }
        if st.all_done() || st.any_failed() {
            return;
        }
        // Nothing to do yet; wait for someone to finish or for new work to appear.
        let _ = tokio::time::timeout(Duration::from_millis(200), st.notify.notified()).await;
    }
}

async fn transfer_segment(
    st: &Arc<RunState>,
    shared: &Arc<Shared>,
    sink: &Arc<Sink>,
    h: Arc<SegHandle>,
) {
    let mut attempt = 0u32;
    loop {
        if shared.cancel.load(Ordering::SeqCst) || st.any_failed() || st.is_stopped() {
            return;
        }
        let cursor = h.cursor();
        let end = h.end.load(Ordering::SeqCst);
        if cursor > end {
            h.state.store(ST_DONE, Ordering::SeqCst);
            st.notify.notify_waiters();
            return;
        }

        match stream_range(shared, sink, &h, cursor, end).await {
            Ok(StreamOutcome::Complete) => {
                h.state.store(ST_DONE, Ordering::SeqCst);
                st.notify.notify_waiters();
                return;
            }
            Ok(StreamOutcome::ShortRead) => {
                // Server closed early. Retry from the new cursor — this is resume within a
                // segment, and it is why `done` is advanced per write rather than per segment.
                attempt += 1;
            }
            Err(RiptideError::NoRangeSupport) => {
                st.fail("server stopped honouring byte ranges");
                return;
            }
            Err(RiptideError::QuotaExhausted(q)) => {
                // A spent budget is not a segment failure. Stop the whole run and leave
                // every segment's progress intact, so the job resumes where it left off
                // once the window refills.
                st.stop(q);
                return;
            }
            Err(e) => {
                attempt += 1;
                tracing::debug!(seg = h.id, attempt, error = %e, "segment attempt failed");
            }
        }

        h.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt > shared.max_retries {
            h.state.store(ST_FAILED, Ordering::SeqCst);
            st.fail(format!(
                "segment {} failed after {} attempts",
                h.id, attempt
            ));
            return;
        }
        tokio::time::sleep(shared.retry_backoff * attempt).await;
    }
}

enum StreamOutcome {
    Complete,
    ShortRead,
}

async fn stream_range(
    shared: &Arc<Shared>,
    sink: &Arc<Sink>,
    h: &Arc<SegHandle>,
    start: u64,
    end: u64,
) -> Result<StreamOutcome> {
    let mut rb = shared.request(Method::GET);
    rb = rb.header("Range", format!("bytes={start}-{end}"));
    rb = rb.header("Accept-Encoding", "identity");
    if let Some(v) = &shared.validator {
        rb = rb.header(IF_RANGE, v);
    }

    // Hold one of this host's connection slots for as long as the response is open, so
    // several jobs pointed at the same server cannot exceed "Max. connections for a server".
    let _slot = shared.servers.acquire(&shared.url).await;
    let resp = rb.send().await?;
    let status = resp.status();

    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // The range is beyond EOF: the file must have shrunk. Treat as complete.
        return Ok(StreamOutcome::Complete);
    }
    if status == StatusCode::OK {
        // Server ignored our Range and is sending the whole entity.
        return Err(RiptideError::NoRangeSupport);
    }
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(RiptideError::other(format!("unexpected HTTP {status}")));
    }

    // Trust Content-Range over our own arithmetic: some servers shift the start.
    let actual_start = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let first = v.trim_start_matches("bytes").trim().split('-').next()?;
            first.trim().parse::<u64>().ok()
        })
        .unwrap_or(start);
    if actual_start != start {
        tracing::warn!(
            expected = start,
            actual = actual_start,
            "server shifted the range start"
        );
        h.done
            .store(actual_start.saturating_sub(h.start), Ordering::SeqCst);
    }

    let mut pos = actual_start;
    let mut stream = resp.bytes_stream();

    loop {
        if shared.cancel.load(Ordering::SeqCst) {
            return Err(RiptideError::Cancelled);
        }
        // Re-read the boundary every chunk: a re-split may have shortened us.
        let live_end = h.end.load(Ordering::SeqCst);

        match tokio::time::timeout(shared.read_timeout, stream.next()).await {
            Err(_) => {
                h.done.store(pos.saturating_sub(h.start), Ordering::SeqCst);
                return Ok(StreamOutcome::ShortRead);
            }
            Ok(None) => {
                h.done.store(pos.saturating_sub(h.start), Ordering::SeqCst);
                return Ok(if pos > live_end {
                    StreamOutcome::Complete
                } else {
                    StreamOutcome::ShortRead
                });
            }
            Ok(Some(Err(e))) => {
                h.done.store(pos.saturating_sub(h.start), Ordering::SeqCst);
                return Err(e.into());
            }
            Ok(Some(Ok(chunk))) => {
                if chunk.is_empty() {
                    continue;
                }
                // Clamp to the live boundary so a concurrent re-split cannot be violated.
                let room = live_end.saturating_sub(pos) + 1;
                let take = (chunk.len() as u64).min(room) as usize;
                if take == 0 {
                    return Ok(StreamOutcome::Complete);
                }
                let piece = if take == chunk.len() {
                    chunk
                } else {
                    bytes::Bytes::copy_from_slice(&chunk[..take])
                };
                shared.charge(take as u64).await?;
                sink.write_at(piece, pos).await?;
                pos += take as u64;
                h.done.store(pos - h.start, Ordering::SeqCst);
                st_progress_hint(shared, h, pos, live_end);

                if pos > live_end {
                    return Ok(StreamOutcome::Complete);
                }
            }
        }
    }
}

/// Cheap hook so the monitor's byte counter tracks in-flight data even if the monitor
/// task is not scheduled; currently a no-op beyond keeping the signature honest.
fn st_progress_hint(_shared: &Arc<Shared>, _h: &Arc<SegHandle>, _pos: u64, _end: u64) {}

// ---------------------------------------------------------------------------
// NTLM and Negotiate
// ---------------------------------------------------------------------------

/// Every value of every `WWW-Authenticate` header, in order.
///
/// A server routinely advertises several schemes at once — `Negotiate` and `NTLM`,
/// or `Basic` and `Digest` — as repeated headers. Reading only the first one picks
/// whichever the server happened to list first, which is how a job configured for
/// Digest ends up being answered with a Basic challenge.
fn www_authenticate_headers(resp: &reqwest::Response) -> Vec<String> {
    resp.headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect()
}

/// How a challenge-response exchange puts its tokens on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthExchange {
    /// `Authorization: NTLM <base64>`, carrying bare NTLMSSP messages.
    Ntlm,
    /// `Authorization: Negotiate <base64>`, with the NTLMSSP messages inside SPNEGO.
    Spnego,
}

impl AuthExchange {
    fn first(&self, type1: &[u8]) -> String {
        match self {
            AuthExchange::Ntlm => format!("NTLM {}", auth::base64_encode(type1)),
            AuthExchange::Spnego => {
                format!("Negotiate {}", auth::spnego::initial_authorization(type1))
            }
        }
    }

    fn last(&self, type3: &[u8]) -> String {
        match self {
            AuthExchange::Ntlm => format!("NTLM {}", auth::base64_encode(type3)),
            AuthExchange::Spnego => {
                format!("Negotiate {}", auth::spnego::response_authorization(type3))
            }
        }
    }
}

/// Perform the 3-leg NTLM handshake and return the final `Authorization` value.
///
/// Both of Riptide's NTLM-shaped schemes run through here, because the messages are
/// identical and only the wrapping differs: `NTLM` sends them bare, and `Negotiate`
/// sends them inside SPNEGO (see [`auth::spnego`]).
async fn ntlm_exchange(
    client: &Arc<NetClient>,
    url: &str,
    spec: &auth::AuthSpec,
    extra: &[(String, String)],
    mode: AuthExchange,
) -> Result<String> {
    let ws = spec.workstation();
    let type1 = auth::ntlm::type1(spec.domain(), &ws);

    let mut rb = client.raw().request(Method::GET, url);
    for (k, v) in extra {
        rb = rb.header(k, v);
    }
    let resp = rb
        .header("Authorization", mode.first(&type1))
        .send()
        .await?;
    let headers = www_authenticate_headers(&resp);
    drop(resp);

    let t2 = match mode {
        AuthExchange::Ntlm => headers
            .iter()
            .find_map(|h| auth::ntlm::challenge_from_header(h))
            .ok_or_else(|| {
                RiptideError::AuthFailed(
                    "server did not offer an NTLM challenge, so there is nothing to answer".into(),
                )
            })?,
        AuthExchange::Spnego => {
            let challenge = headers
                .iter()
                .find_map(|h| auth::spnego::negotiate_payload(h))
                .and_then(|payload| auth::spnego::parse_server_token(&payload))
                .ok_or_else(|| {
                    RiptideError::AuthFailed(
                        "the server answered with an unreadable Negotiate token. A server \
                         configured for Kerberos only often replies with an empty or \
                         Kerberos-only challenge, which an NTLM client cannot answer; \
                         enable NTLM on it, or use `ntlm:`, `basic:`, or `digest:`"
                            .into(),
                    )
                })?;

            // Say this in the right words. Failing with "not an NTLM message" would
            // leave the user guessing at their server's Kerberos configuration.
            if challenge.mech() == Some(auth::spnego::Mech::Kerberos) {
                return Err(RiptideError::AuthFailed(
                    "the server answered Negotiate with a Kerberos token. Riptide \
                     implements SPNEGO with NTLM as the inner mechanism and has no \
                     Kerberos implementation, so a Kerberos-only server cannot be \
                     authenticated against; enable NTLM on the server, or use \
                     `ntlm:`, `basic:`, or `digest:`"
                        .into(),
                ));
            }
            if challenge.is_reject() {
                return Err(RiptideError::AuthFailed(
                    "the server rejected the offered mechanism, which for Riptide is \
                     NTLMSSP alone"
                        .into(),
                ));
            }

            let token = challenge.token.as_deref().ok_or_else(|| {
                RiptideError::AuthFailed(
                    "the Negotiate challenge carried no mechanism token".into(),
                )
            })?;
            auth::ntlm::parse_type2(token).ok_or_else(|| {
                RiptideError::AuthFailed(
                    "the Negotiate challenge did not contain an NTLM message".into(),
                )
            })?
        }
    };

    let type3 = auth::ntlm::type3(&t2, spec.user(), spec.domain(), spec.pass(), &ws);
    Ok(mode.last(&type3))
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    #[test]
    fn content_range_total_parsing() {
        assert_eq!(parse_content_range_total("bytes 0-1/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    #[test]
    fn run_state_claim_and_split() {
        let st = RunState::new();
        let segs = segplan::plan(8 << 20, 2, 1 << 10);
        st.add(
            segs.iter()
                .map(|s| SegHandle::new(s.id, s.start, s.end))
                .collect(),
        );
        st.next_id.store(2, Ordering::SeqCst);

        let a = st.claim().expect("one");
        let b = st.claim().expect("two");
        assert!(st.claim().is_none());
        assert_eq!(a.state.load(Ordering::SeqCst), ST_RUNNING);
        assert_eq!(b.state.load(Ordering::SeqCst), ST_RUNNING);

        // With both segments running, an idle worker can still split the largest.
        assert!(st.try_split(1 << 20, 1 << 10));
        assert!(st.claim().is_some(), "split produced new work");

        // Coverage is still complete after splitting.
        let snap = st.snapshot();
        let mut sorted = snap.clone();
        sorted.sort_by_key(|s| s.start);
        assert_eq!(sorted[0].start, 0);
        let end_max = sorted.iter().map(|s| s.end).max().unwrap();
        assert_eq!(end_max, (8 << 20) - 1);
    }

    #[test]
    fn done_segments_are_not_requeued_on_resume() {
        let mut s0 = Segment::new(0, 0, 99);
        s0.done = 100;
        s0.state = SegmentState::Done;
        let s1 = Segment::new(1, 100, 199);

        let st = RunState::new();
        st.add(vec![
            SegHandle::from_segment(&s0),
            SegHandle::from_segment(&s1),
        ]);
        let claimed = st.claim().expect("only the unfinished one is claimable");
        assert_eq!(claimed.id, 1);
        assert!(st.claim().is_none());
    }

    #[test]
    fn resume_seed_computes_downloaded_bytes() {
        let mut cfg = Config::default();
        cfg.connections = 2;
        let mut job = Job::new(
            "x".into(),
            "http://h/f".into(),
            std::path::PathBuf::from("/tmp/f"),
            JobKind::Http,
        );
        job.total = Some(200);
        let mut a = Segment::new(0, 0, 99);
        a.state = SegmentState::Done;
        a.done = 100;
        job.segments = vec![a, Segment::new(1, 100, 199)];

        let eng = Engine::new(cfg, job).unwrap();
        let st = eng.build_state(200);
        assert_eq!(st.done_bytes.load(Ordering::SeqCst), 100);
        assert_eq!(st.handles().len(), 2);
    }

    #[test]
    fn fail_records_only_the_first_error() {
        let st = RunState::new();
        st.fail("first");
        st.fail("second");
        assert_eq!(st.error_message().as_deref(), Some("first"));
        assert!(st.any_failed());
    }
}
