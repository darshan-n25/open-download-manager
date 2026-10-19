//! Command line interface.
//!
//! The shape deliberately matches what IDM users expect: fire-and-forget downloads, a
//! persistent queue you can list and resume, a site grabber that can also keep an offline
//! mirror, and a daemon the browser extension talks to.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::archive::{self, ArchiveConfig};
use crate::config::Config;
use crate::engine::{Engine, ProgressFn, ProgressSample};
use crate::error::{Result, RiptideError};
use crate::grabber::{self, GrabberConfig, TargetKind};
use crate::job::{Job, JobKind, JobState};
use crate::project::{self, Project, ProjectKind, ProjectRunner, ProjectStore, RunOutcome};
use crate::scheduler::Scheduler;
use crate::state::JobStore;
use crate::util;

#[derive(Parser, Debug)]
#[command(
    name = "riptide",
    version,
    about = "Riptide — a fast, cross-platform download manager",
    long_about = "Riptide downloads files over HTTP/HTTPS/FTP with parallel ranged connections, \
                  resumes safely after a crash, captures HLS and DASH video, and exposes a \
                  localhost daemon for browser integration.\n\n\
                  Modeled behaviourally on Internet Download Manager."
)]
pub struct Cli {
    /// Alternate configuration file.
    #[arg(short = 'c', long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Increase log verbosity (-v for info, -vv for debug, -vvv for trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all logging.
    #[arg(long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Download one or more URLs.
    Download(DownloadArgs),
    /// Crawl a web page and find downloadable links.
    Grab(GrabArgs),
    /// Run the localhost daemon that browser extensions connect to.
    Daemon(DaemonArgs),
    /// Inspect a URL without downloading it.
    Probe(ProbeArgs),
    /// List jobs and their progress.
    List(ListArgs),
    /// Show one job in full.
    Show { id: String },
    /// Delete a job's record (the downloaded file is kept).
    Remove { id: String },
    /// Restart every unfinished job in the queue.
    Queue(QueueArgs),
    /// Mark a job as paused. It stays resumable.
    Pause { id: String },
    /// Inspect or edit the configuration.
    Config(ConfigArgs),
    /// Manage recurring projects: keep a file in sync, or re-crawl a site on a schedule.
    Project(ProjectArgs),
    /// Print the directory holding job state.
    StateDir,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Http,
    Hls,
    Dash,
    Ftp,
}

impl From<KindArg> for JobKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Http => JobKind::Http,
            KindArg::Hls => JobKind::Hls,
            KindArg::Dash => JobKind::Dash,
            KindArg::Ftp => JobKind::Ftp,
        }
    }
}

/// Request-shaping options, shared by every command that talks to a server.
#[derive(Args, Debug, Clone, Default)]
pub struct NetOpts {
    /// Proxy URL, or IDM-style `mode:value;bypass` (e.g. `proxy:host:8080;<local>`).
    #[arg(long)]
    pub proxy: Option<String>,

    /// `User-Agent` header. Defaults to a browser-like string.
    #[arg(short = 'A', long)]
    pub user_agent: Option<String>,

    /// `Referer` header. Many CDNs reject requests without one.
    #[arg(long)]
    pub referer: Option<String>,

    /// `Cookie` header, sent verbatim.
    #[arg(long)]
    pub cookie: Option<String>,

    /// Credentials: `user:pass` (Basic), `digest:user:pass`, `bearer:token`,
    /// `ntlm:DOMAIN\user:pass` (bare NTLM), or `negotiate:DOMAIN\user:pass`
    /// (SPNEGO, completed with NTLM).
    #[arg(long)]
    pub auth: Option<String>,

    /// Extra request header, `Name: value`. Repeatable.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Accept invalid TLS certificates.
    #[arg(long)]
    pub insecure: bool,

    /// Parallel connections for this job.
    #[arg(short = 'n', long)]
    pub connections: Option<usize>,

    /// Per-file speed ceiling, e.g. `2M` or `500k`.
    #[arg(long)]
    pub limit: Option<String>,

    /// Retry count for this job.
    #[arg(long)]
    pub retries: Option<u32>,
}

#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// URLs to download.
    pub urls: Vec<String>,

    #[command(flatten)]
    pub net: NetOpts,

    /// Output path. With several URLs this must be a directory.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Read additional URLs from a file, one per line.
    #[arg(long, value_name = "PATH")]
    pub input_file: Option<PathBuf>,

    /// Preferred quality for HLS/DASH: `best`, `worst`, or a height such as `720`.
    #[arg(short = 'q', long)]
    pub quality: Option<String>,

    /// Force the transfer kind instead of guessing from the URL.
    #[arg(long, value_enum)]
    pub kind: Option<KindArg>,

    /// Delay the start: a relative span (`+30m`) or an RFC 3339 timestamp.
    #[arg(long, value_name = "WHEN")]
    pub scheduled: Option<String>,

    /// How many of these jobs may run at once.
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Download sequentially rather than against a queue.
    #[arg(long)]
    pub no_queue: bool,
}

#[derive(Args, Debug)]
pub struct GrabArgs {
    /// Page to start crawling from.
    pub url: String,

    #[command(flatten)]
    pub net: NetOpts,

    /// Link hops to follow from the start page.
    #[arg(short = 'd', long, default_value_t = 1)]
    pub depth: usize,

    /// Cap on pages fetched while crawling.
    #[arg(long)]
    pub max_pages: Option<usize>,

    /// Keep only URLs matching this wildcard. Repeatable.
    #[arg(long)]
    pub include: Vec<String>,

    /// Drop URLs matching this wildcard. Repeatable.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Report HTML pages too, not just media and archives.
    #[arg(long)]
    pub pages: bool,

    /// Follow links to other hosts as well.
    #[arg(long)]
    pub any_host: bool,

    /// Actually download everything found instead of only listing it.
    #[arg(long)]
    pub download: bool,

    /// Mirror the page and what it needs to render: keep the stylesheets, scripts, images
    /// and fonts, and rewrite the markup to point at the local copies.
    ///
    /// Fetches by itself, so `--download` is not needed and is ignored.
    #[arg(long)]
    pub archive: bool,

    /// Cap on how many assets one `--archive` run will hold.
    #[arg(long, value_name = "N", default_value_t = archive::DEFAULT_MAX_ASSETS)]
    pub max_assets: usize,

    /// Directory for `--download`, or the archive root for `--archive`.
    #[arg(short = 'o', long)]
    pub output_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Address to listen on.
    #[arg(long, value_name = "HOST:PORT")]
    pub addr: Option<String>,

    /// Report the extension endpoint and exit without listening.
    #[arg(long)]
    pub print_endpoint: bool,
}

#[derive(Args, Debug)]
pub struct ProbeArgs {
    pub url: String,

    #[command(flatten)]
    pub net: NetOpts,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Only show jobs in this state.
    #[arg(long, value_enum)]
    pub state: Option<StateArg>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum StateArg {
    Queued,
    Scheduled,
    Active,
    Paused,
    Done,
    Failed,
}

impl From<StateArg> for JobState {
    fn from(s: StateArg) -> Self {
        match s {
            StateArg::Queued => JobState::Queued,
            StateArg::Scheduled => JobState::Scheduled,
            StateArg::Active => JobState::Active,
            StateArg::Paused => JobState::Paused,
            StateArg::Done => JobState::Done,
            StateArg::Failed => JobState::Failed,
        }
    }
}

#[derive(Args, Debug)]
pub struct QueueArgs {
    /// Cap on simultaneous transfers while draining the queue.
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,
    /// Override the download quota for this run, e.g. `--quota 500M` ("Download limits").
    /// `none` or `0` disables it.
    #[arg(long)]
    pub quota: Option<String>,
    /// Length of the quota window for this run, e.g. `1h`, `30m`, or seconds.
    #[arg(long, value_name = "SPAN")]
    pub quota_window: Option<String>,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the effective configuration.
    Show,
    /// Print the configuration file path.
    Path,
    /// Write a default configuration file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Set one key, e.g. `riptide config set connections 16`.
    Set { key: String, value: String },
}

#[derive(Args, Debug)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub action: ProjectAction,
}

#[derive(Subcommand, Debug)]
// Clap's derive requires each variant to hold its own argument struct directly, so `Add`
// is genuinely much larger than the rest. This enum is built once at startup.
#[allow(clippy::large_enum_variant)]
pub enum ProjectAction {
    /// Create a project. Its first run is one interval away.
    Add(ProjectAddArgs),
    /// List projects, when each next runs, and how the last run went.
    List {
        /// Print the projects as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one project in full.
    Show { name: String },
    /// Run one project now, whatever its schedule says.
    Run { name: String },
    /// Run every project that is due. This is the command to put in `cron`.
    Tick {
        /// Run every project, due or not.
        #[arg(long)]
        force: bool,
    },
    /// Stop a project from running on its own. Its files are kept.
    Disable { name: String },
    /// Let a disabled project run again.
    Enable { name: String },
    /// Delete a project. Everything it downloaded is kept.
    Remove { name: String },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProjectKindArg {
    /// Keep one URL up to date at one local file (IDM's periodic synchronization).
    File,
    /// Re-crawl a page on a schedule (IDM's site grabber project).
    Site,
}

impl From<ProjectKindArg> for ProjectKind {
    fn from(k: ProjectKindArg) -> Self {
        match k {
            ProjectKindArg::File => ProjectKind::File,
            ProjectKindArg::Site => ProjectKind::Site,
        }
    }
}

#[derive(Args, Debug)]
pub struct ProjectAddArgs {
    /// Project name. Also the file name its state is stored under.
    pub name: String,

    #[command(flatten)]
    pub net: NetOpts,

    /// The URL to synchronize (`--kind file`) or to crawl (`--kind site`).
    #[arg(long)]
    pub url: String,

    /// `file` keeps one URL up to date; `site` re-crawls a page.
    #[arg(long, value_enum, default_value_t = ProjectKindArg::File)]
    pub kind: ProjectKindArg,

    /// How often to run, e.g. `30m`, `1h`, `1d`, or a bare number of seconds.
    #[arg(long, value_name = "INTERVAL")]
    pub every: String,

    /// Where to write: a file for `--kind file`, a directory for `--kind site`.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Run once straight away instead of waiting for the first interval.
    #[arg(long)]
    pub start_now: bool,

    /// Preferred rendition for HLS/DASH targets: `best`, `worst`, or a height such as `720`.
    #[arg(short = 'q', long, value_name = "QUALITY")]
    pub quality: Option<String>,

    // ---------------------------------------------------------------- site only
    /// Link hops to follow from the start page.
    #[arg(short = 'd', long, default_value_t = 1)]
    pub depth: usize,

    /// Cap on how many pages the crawl will visit.
    #[arg(long, value_name = "N")]
    pub max_pages: Option<usize>,

    /// Keep only URLs matching this wildcard. Repeatable.
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Drop URLs matching this wildcard. Repeatable.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Report HTML pages too instead of only media and archives.
    #[arg(long)]
    pub pages: bool,

    /// Follow links to other hosts as well.
    #[arg(long)]
    pub any_host: bool,

    /// Also fetch what the crawl finds, instead of only recording it.
    #[arg(long)]
    pub download: bool,

    /// Keep an offline mirror instead: the pages, plus the stylesheets, scripts, images and
    /// fonts they need to render, with the markup rewritten to point at the local copies.
    #[arg(long)]
    pub archive: bool,

    /// Cap on how many crawl results one run will fetch, or on the assets `--archive`
    /// holds.
    #[arg(long, value_name = "N", default_value_t = project::DEFAULT_MAX_DOWNLOADS)]
    pub max_downloads: usize,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(cli: Cli) -> Result<()> {
    // Must happen before anything calls `Config::load`.
    if let Some(p) = &cli.config {
        std::env::set_var("RIPTIDE_CONFIG", p);
    }
    let quiet = cli.quiet;

    match cli.command {
        Command::Download(args) => cmd_download(args, quiet).await,
        Command::Grab(args) => cmd_grab(args, quiet).await,
        Command::Daemon(args) => cmd_daemon(args).await,
        Command::Probe(args) => cmd_probe(args).await,
        Command::List(args) => cmd_list(args),
        Command::Show { id } => cmd_show(&id),
        Command::Remove { id } => cmd_remove(&id),
        Command::Queue(args) => cmd_queue(args, quiet).await,
        Command::Pause { id } => cmd_pause(&id),
        Command::Config(args) => cmd_config(args),
        Command::Project(args) => cmd_project(args, quiet).await,
        Command::StateDir => {
            println!("{}", Config::load().state_dir.display());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn build_ctx(net: &NetOpts, cfg: &Config) -> Result<crate::job::RequestContext> {
    let mut ctx = crate::job::RequestContext {
        user_agent: net.user_agent.clone(),
        referer: net.referer.clone(),
        cookie: net.cookie.clone(),
        proxy: net.proxy.clone(),
        insecure: net.insecure || cfg.insecure,
        ..Default::default()
    };
    if let Some(a) = &net.auth {
        ctx.auth = Some(crate::auth::parse_auth_spec(a).ok_or_else(|| {
            RiptideError::other(format!(
                "cannot parse --auth {a:?}; expected `user:pass`, `basic:user:pass`, \
                 `digest:user:pass`, `bearer:token`, `ntlm:DOMAIN\\user:pass`, or \
                 `negotiate:DOMAIN\\user:pass`"
            ))
        })?);
    }
    for h in &net.headers {
        let (k, v) = h
            .split_once(':')
            .ok_or_else(|| RiptideError::other(format!("--header {h:?} must be 'Name: value'")))?;
        ctx.headers
            .push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(ctx)
}

/// Build a [`Job`] from a URL plus the command line.
fn build_job(
    cfg: &Config,
    url: &str,
    args: &DownloadArgs,
    explicit: Option<PathBuf>,
) -> Result<Job> {
    let kind = args
        .kind
        .map(JobKind::from)
        .unwrap_or_else(|| JobKind::guess(url));

    // For media jobs the output must be settled now: a `.m3u8` extension would be wrong
    // once the segments are muxed into one file.
    let explicit = explicit.or_else(|| {
        if matches!(kind, JobKind::Hls | JobKind::Dash) {
            let stem = util::filename_from_url(url)
                .map(|n| {
                    PathBuf::from(n)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("video")
                        .to_string()
                })
                .unwrap_or_else(|| "video".into());
            Some(cfg.resolve_output(None, &format!("{stem}.mp4"), Some("mp4")))
        } else {
            None
        }
    });

    let id = util::gen_id();
    let provisional = explicit.is_none();
    let output = explicit.unwrap_or_else(|| cfg.download_dir.join(format!(".{id}.pending")));

    let mut job = Job::new(id, url.to_string(), output, kind);
    job.output_provisional = provisional;
    job.connections = args.net.connections.unwrap_or(cfg.connections);
    job.max_retries = args.net.retries.unwrap_or(cfg.max_retries);
    job.speed_limit = args.net.limit.as_deref().and_then(util::parse_size);
    job.quality = args.quality.clone();
    // The configured segment floor and split threshold are what make "smallest useful
    // segment" a real setting rather than a description of `Job::new`'s defaults.
    job.min_chunk = cfg.min_chunk;
    if let Some(when) = &args.scheduled {
        job.scheduled_at = Some(parse_when(when)?);
        job.state = JobState::Scheduled;
    }
    job.ctx = build_ctx(&args.net, cfg)?;
    Ok(job)
}

/// `+30m` (relative) or an RFC 3339 timestamp.
pub fn parse_when(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        let d = humantime::parse_duration(rest)
            .map_err(|e| RiptideError::other(format!("bad duration {rest:?}: {e}")))?;
        return Ok(util::now_ms() + d.as_millis() as u64);
    }
    if let Ok(t) = humantime::parse_rfc3339(s) {
        let ms = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        return Ok(ms);
    }
    Err(RiptideError::other(format!(
        "cannot read {s:?} as a time; use a relative span like +30m or an RFC 3339 timestamp"
    )))
}

fn output_for(index: usize, total: usize, url: &str, args: &DownloadArgs) -> Option<PathBuf> {
    let base = args.output.as_ref()?;
    if total == 1 && !base.is_dir() {
        return Some(base.clone());
    }
    let name = util::filename_from_url(url).unwrap_or_else(|| format!("download-{}", index + 1));
    Some(base.join(name))
}

async fn cmd_download(args: DownloadArgs, quiet: bool) -> Result<()> {
    let mut cfg = Config::load();

    let mut urls = args.urls.clone();
    if let Some(f) = &args.input_file {
        let text = std::fs::read_to_string(f)
            .map_err(|e| RiptideError::other(format!("cannot read {}: {e}", f.display())))?;
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                urls.push(line.to_string());
            }
        }
    }
    urls.retain(|u| !u.trim().is_empty());
    if urls.is_empty() {
        return Err(RiptideError::other(
            "nothing to download: pass a URL or --input-file",
        ));
    }

    if let Some(j) = args.jobs {
        cfg.max_concurrent_jobs = j.max(1);
    }

    std::fs::create_dir_all(&cfg.download_dir).ok();
    for (i, url) in urls.iter().enumerate() {
        if let Some(p) = output_for(i, urls.len(), url, &args) {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
        }
    }

    let mut sched = Scheduler::new(cfg.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    sched.set_shutdown(stop.clone());
    let sched = Arc::new(sched);

    let mut jobs = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let explicit = output_for(i, urls.len(), url, &args);
        jobs.push(build_job(&cfg, url, &args, explicit)?);
    }

    // Persist every job before starting, so an interrupted run is still resumable.
    for job in &jobs {
        sched.store.save(job)?;
    }

    let ctrl_c = spawn_ctrl_c(stop.clone());

    let mut handles = Vec::new();
    for job in jobs {
        let label = job.display_name();
        let reporter = reporter(label.clone(), quiet);
        handles.push((label, sched.spawn_with(job, reporter)));
    }

    let mut failed = 0usize;
    for (label, h) in handles {
        match h.await {
            Ok(Ok(job)) => {
                if quiet {
                    println!("{}", job.output.display());
                } else {
                    println!("{}: done → {}", label, job.output.display());
                }
            }
            Ok(Err(e)) => {
                failed += 1;
                eprintln!("{}: failed: {e}", label);
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: task failed: {e}", label);
            }
        }
    }
    ctrl_c.abort();

    if failed > 0 {
        return Err(RiptideError::other(format!(
            "{failed} of the requested downloads did not complete"
        )));
    }
    Ok(())
}

fn spawn_ctrl_c(stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ninterrupted — stopping transfers; resume with `riptide queue`");
            stop.store(true, Ordering::SeqCst);
        }
    })
}

// ---------------------------------------------------------------------------
// Progress reporting
// ---------------------------------------------------------------------------

fn render_bar(fraction: f64, width: usize) -> String {
    let f = fraction.clamp(0.0, 1.0);
    let filled = (f * width as f64).round() as usize;
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        s.push(if i < filled { '#' } else { '.' });
    }
    s.push(']');
    s
}

fn reporter(label: String, quiet: bool) -> Option<ProgressFn> {
    if quiet {
        return None;
    }
    let tty = std::io::stderr().is_terminal();
    let last = Arc::new(AtomicU64::new(0));
    Some(Arc::new(move |s: ProgressSample| {
        let now = util::now_ms();
        let prev = last.load(Ordering::Relaxed);
        if now.saturating_sub(prev) < 200 {
            return;
        }
        last.store(now, Ordering::Relaxed);

        let speed = if s.speed_bps > 0.0 {
            format!("{}/s", util::human_bytes(s.speed_bps as u64))
        } else {
            "—".to_string()
        };
        let line = match s.total {
            Some(t) if t > 0 => {
                let f = s.downloaded as f64 / t as f64;
                format!(
                    "{label}  {} {:5.1}%  {} / {}  {speed}",
                    render_bar(f, 24),
                    f * 100.0,
                    util::human_bytes(s.downloaded),
                    util::human_bytes(t),
                )
            }
            _ => format!("{label}  {}  {speed}", util::human_bytes(s.downloaded)),
        };
        if tty {
            eprint!("\r\x1b[K{line}");
        } else {
            eprintln!("{line}");
        }
    }))
}

// ---------------------------------------------------------------------------
// Grabber
// ---------------------------------------------------------------------------

/// The directory a one-shot archive lands in when `-o` is not given.
///
/// Named after the host so that archiving two sites cannot mix them, and sanitized because a
/// host with a port contains a colon.
fn archive_root_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .map(|h| util::sanitize_filename(&h))
        .unwrap_or_else(|| "archive".to_string())
}

async fn cmd_grab(args: GrabArgs, quiet: bool) -> Result<()> {
    let mut cfg = Config::load();

    let gcfg = GrabberConfig {
        depth: args.depth,
        max_pages: args.max_pages.unwrap_or(grabber::DEFAULT_MAX_PAGES),
        same_host: !args.any_host,
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        media_only: !args.pages,
    };

    // A throwaway engine supplies a configured HTTP client (proxy, UA, TLS policy).
    let scratch = Job::new(
        "grab".into(),
        args.url.clone(),
        cfg.download_dir.join(".grab-probe"),
        JobKind::Grabber,
    );
    let mut ctx = build_ctx(&args.net, &cfg)?;
    ctx.insecure = args.net.insecure || cfg.insecure;
    let mut scratch = scratch;
    scratch.ctx = ctx;
    if let Some(c) = args.net.connections {
        scratch.connections = c;
    }

    let engine = Engine::new(cfg.clone(), scratch)?;
    let fetcher = engine.fetcher();

    if args.archive {
        let acfg = ArchiveConfig {
            grab: GrabberConfig {
                depth: args.depth,
                max_pages: args.max_pages.unwrap_or(grabber::DEFAULT_MAX_PAGES),
                same_host: !args.any_host,
                include: args.include.clone(),
                exclude: args.exclude.clone(),
                media_only: false,
            },
            max_assets: args.max_assets.max(1),
            ..ArchiveConfig::default()
        };
        let root = match &args.output_dir {
            Some(d) => d.clone(),
            None => cfg.download_dir.join(archive_root_name(&args.url)),
        };
        std::fs::create_dir_all(&root)?;
        if !quiet {
            eprintln!("archiving {} into {}", args.url, root.display());
        }
        let report = archive::archive(&fetcher, &args.url, &acfg, &root).await?;
        // The archive fetched through the engine's own budget without a job record, so the
        // window is checkpointed here rather than at a job boundary.
        engine.quota().flush();
        if !quiet {
            eprintln!(
                "{} document(s), {} stylesheet(s), {} asset(s): {} written, {} unchanged, {} bytes",
                report.documents,
                report.stylesheets,
                report.assets,
                report.written,
                report.unchanged,
                report.bytes
            );
            if report.remote_links > 0 {
                eprintln!(
                    "{} reference(s) left pointing at the live site (excluded by a filter, \
                     past the depth limit, or not retrievable)",
                    report.remote_links
                );
            }
        }
        if report.quota_paused {
            return Err(RiptideError::other(format!(
                "the download quota ran out part-way through the archive; {} file(s) were \
                 written into {} and will be re-checked on the next run",
                report.written,
                root.display()
            )));
        }
        if !report.failures.is_empty() {
            for f in &report.failures {
                eprintln!("failed: {f}");
            }
            return Err(RiptideError::other(format!(
                "{} of {} file(s) could not be archived",
                report.failures.len(),
                report.held()
            )));
        }
        return Ok(());
    }

    if !quiet {
        eprintln!(
            "crawling {} (depth {}, {} page limit)",
            args.url, gcfg.depth, gcfg.max_pages
        );
    }
    let targets = grabber::crawl(&fetcher, &args.url, &gcfg).await?;

    if targets.is_empty() {
        eprintln!("no downloadable links found");
        return Ok(());
    }

    for t in &targets {
        println!("{}\t{}", t.kind.as_str(), t.url);
    }

    if !args.download {
        eprintln!(
            "{} link(s) found; add --download to fetch them",
            targets.len()
        );
        return Ok(());
    }

    if let Some(dir) = &args.output_dir {
        cfg.download_dir = dir.clone();
    }
    std::fs::create_dir_all(&cfg.download_dir).ok();

    let mut sched = Scheduler::new(cfg.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    sched.set_shutdown(stop.clone());
    let sched = Arc::new(sched);
    let ctrl_c = spawn_ctrl_c(stop);

    let mut handles = Vec::new();
    for t in &targets {
        let args_for_job = DownloadArgs {
            urls: vec![t.url.clone()],
            net: args.net.clone(),
            output: None,
            input_file: None,
            quality: None,
            kind: None,
            scheduled: None,
            jobs: None,
            no_queue: true,
        };
        let kind = match t.kind {
            TargetKind::Manifest => JobKind::guess(&t.url),
            _ => JobKind::Http,
        };
        // With `--output-dir` every link is named after its own URL; otherwise the engine
        // derives the name from the response headers, which is more accurate.
        let explicit = args
            .output_dir
            .as_ref()
            .and_then(|d| util::filename_from_url(&t.url).map(|name| d.join(name)));
        let mut job = build_job(&cfg, &t.url, &args_for_job, explicit)?;
        job.kind = kind;
        sched.store.save(&job)?;
        let label = job.display_name();
        handles.push((label.clone(), sched.spawn_with(job, reporter(label, quiet))));
    }

    let mut failed = 0usize;
    for (label, h) in handles {
        match h.await {
            Ok(Ok(job)) => println!("{} → {}", label, job.output.display()),
            Ok(Err(e)) => {
                failed += 1;
                eprintln!("{label}: failed: {e}");
            }
            Err(e) => {
                failed += 1;
                eprintln!("{label}: task failed: {e}");
            }
        }
    }
    ctrl_c.abort();
    if failed > 0 {
        return Err(RiptideError::other(format!("{failed} download(s) failed")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

async fn cmd_daemon(args: DaemonArgs) -> Result<()> {
    let mut cfg = Config::load();
    if let Some(a) = &args.addr {
        cfg.daemon_addr = a.clone();
    }
    if args.print_endpoint {
        println!("ws://{}/?cid=riptide", cfg.daemon_addr);
        println!("subprotocol: {}", crate::daemon::WS_PROTOCOL);
        println!("http api:    http://{}/api/jobs", cfg.daemon_addr);
        return Ok(());
    }
    crate::daemon::serve(cfg).await
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

async fn cmd_probe(args: ProbeArgs) -> Result<()> {
    let cfg = Config::load();
    let kind = JobKind::guess(&args.url);
    let job = Job::new(
        "probe".into(),
        args.url.clone(),
        cfg.download_dir.join(".probe"),
        if kind == JobKind::Ftp {
            JobKind::Ftp
        } else {
            JobKind::Http
        },
    );
    let mut job = job;
    job.ctx = build_ctx(&args.net, &cfg)?;

    if kind == JobKind::Ftp {
        // FTP has no probe path; report what the URL alone tells us.
        let parsed = crate::ftp::parse_ftp_url(&args.url)?;
        let info = serde_json::json!({
            "url": args.url,
            "kind": "ftp",
            "host": parsed.host,
            "port": parsed.port,
            "user": parsed.user,
            "path": parsed.path,
            "note": "run `riptide download` to learn the size; FTP has no metadata request",
        });
        if args.json {
            println!("{}", serde_json::to_string_pretty(&info)?);
        } else {
            println!("kind        ftp");
            println!("host        {}:{}", parsed.host, parsed.port);
            println!("user        {}", parsed.user);
            println!("path        {}", parsed.path);
        }
        return Ok(());
    }

    let mut engine = Engine::new(cfg, job)?;
    engine.probe().await?;
    let j = engine.job();

    if args.json {
        let info = serde_json::json!({
            "url": j.url,
            "resolved_url": j.resolved_url,
            "kind": j.kind.as_str(),
            "status": "ok",
            "content_type": j.content_type,
            "total": j.total,
            "resumable": j.resumable,
            "etag": j.etag,
            "last_modified": j.last_modified,
            "would_save_to": j.output,
        });
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("kind          {}", j.kind.as_str());
        println!(
            "size          {}",
            j.total
                .map(util::human_bytes)
                .unwrap_or_else(|| "unknown".into())
        );
        println!("resumable     {}", if j.resumable { "yes" } else { "no" });
        println!(
            "content-type  {}",
            j.content_type.as_deref().unwrap_or("unknown")
        );
        if let Some(e) = &j.etag {
            println!("etag          {e}");
        }
        if let Some(l) = &j.last_modified {
            println!("last-modified {l}");
        }
        if j.resolved_url.as_deref() != Some(j.url.as_str()) {
            println!(
                "final url     {}",
                j.resolved_url.as_deref().unwrap_or(&j.url)
            );
        }
        println!("saves to      {}", j.output.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Queue inspection
// ---------------------------------------------------------------------------

fn store() -> Result<JobStore> {
    JobStore::open(Config::load().state_dir)
}

fn cmd_list(args: ListArgs) -> Result<()> {
    let store = store()?;
    let mut jobs = store.list()?;
    jobs.sort_by_key(|j| j.created_ms);

    if let Some(s) = args.state {
        let want: JobState = s.into();
        jobs.retain(|j| j.state == want);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }
    if jobs.is_empty() {
        println!("no jobs");
        return Ok(());
    }

    println!(
        "{:<10} {:<9} {:>10} {:>7}  NAME",
        "ID", "STATE", "SIZE", "DONE"
    );
    for j in &jobs {
        // `Job::progress` is already a percentage in 0.0..=100.0.
        let pct = j.progress().map(|p| format!("{p:.0}%")).unwrap_or_default();
        println!(
            "{:<10} {:<9} {:>10} {:>7}  {}",
            j.id,
            format!("{:?}", j.state).to_lowercase(),
            j.total.map(util::human_bytes).unwrap_or_else(|| "—".into()),
            pct,
            j.display_name(),
        );
    }
    Ok(())
}

fn cmd_show(id: &str) -> Result<()> {
    let store = store()?;
    let job = store
        .load(id)
        .map_err(|_| RiptideError::other(format!("no job with id {id:?}")))?;
    println!("{}", serde_json::to_string_pretty(&job)?);
    Ok(())
}

fn cmd_remove(id: &str) -> Result<()> {
    let store = store()?;
    store.delete(id)?;
    println!("removed {id}");
    Ok(())
}

fn cmd_pause(id: &str) -> Result<()> {
    let store = store()?;
    let mut job = store
        .load(id)
        .map_err(|_| RiptideError::other(format!("no job with id {id:?}")))?;
    if job.state.is_terminal() {
        return Err(RiptideError::other(format!(
            "job {id} is already {:?}",
            job.state
        )));
    }
    job.state = JobState::Paused;
    job.touch();
    store.save(&job)?;
    println!("paused {id} (resume with `riptide queue`)");
    Ok(())
}

async fn cmd_queue(args: QueueArgs, quiet: bool) -> Result<()> {
    let mut cfg = Config::load();
    if let Some(j) = args.jobs {
        cfg.max_concurrent_jobs = j.max(1);
    }
    // Per-run overrides for "Download limits", so a one-off budget does not have to be
    // written into the config file.
    if let Some(spec) = &args.quota {
        let spec = spec.trim();
        cfg.quota_bytes = if spec.is_empty() || spec.eq_ignore_ascii_case("none") || spec == "0" {
            None
        } else {
            Some(util::parse_size(spec).ok_or_else(|| {
                RiptideError::other(format!("cannot read {spec:?} as a size; try 500M or 1G"))
            })?)
        };
    }
    if let Some(span) = &args.quota_window {
        let d = humantime::parse_duration(span.trim())
            .map_err(|e| RiptideError::other(format!("bad quota window {span:?}: {e}")))?;
        cfg.quota_window_secs = d.as_secs().max(1);
    }
    let mut sched = Scheduler::new(cfg)?;
    let stop = Arc::new(AtomicBool::new(false));
    sched.set_shutdown(stop.clone());
    let sched = Arc::new(sched);

    let pending = sched.store.pending()?;
    if pending.is_empty() {
        println!("queue is empty");
        return Ok(());
    }
    if !quiet {
        eprintln!("restarting {} job(s)", pending.len());
        if sched.quota.limit_bytes().is_some() {
            eprintln!("download quota: {}", sched.quota.describe());
        }
    }

    let ctrl_c = spawn_ctrl_c(stop);
    let mut handles = Vec::new();
    for job in pending {
        let label = job.display_name();
        let id = job.id.clone();
        handles.push((
            id,
            label.clone(),
            sched.spawn_with(job, reporter(label, quiet)),
        ));
    }

    let mut failed = 0usize;
    let mut quota_paused = 0usize;
    for (id, label, h) in handles {
        let outcome = h.await;
        // The engine persists the outcome before the handle resolves, so the stored job is
        // the authoritative record of *why* it stopped.
        let was_quota_paused = sched
            .store
            .load(&id)
            .map(|j| j.quota_paused)
            .unwrap_or(false);
        match outcome {
            Ok(Ok(job)) => println!("{} → {}", label, job.output.display()),
            Ok(Err(e)) => {
                if was_quota_paused {
                    // Paused, not failed: it resumes on its own in a later window.
                    quota_paused += 1;
                    eprintln!("{label}: paused — {e}");
                } else {
                    failed += 1;
                    eprintln!("{label}: failed: {e}");
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{label}: task failed: {e}");
            }
        }
    }
    ctrl_c.abort();

    if sched.quota.limit_bytes().is_some() {
        println!("download quota: {}", sched.quota.describe());
    }
    if quota_paused > 0 {
        println!(
            "{quota_paused} job(s) paused on the download quota; \
             run `riptide queue` again once the window refills"
        );
    }
    if failed > 0 {
        eprintln!("{failed} job(s) failed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Settings that hold a byte count, so `config set` accepts the same `500M` shorthand as
/// `--limit` and `--quota`, and can switch the limit back off with `none`.
const BYTE_SETTINGS: [&str; 2] = ["speed_limit", "quota_bytes"];

/// Coerce a `config set` value before it is written.
///
/// For a byte-count setting: a size (`500M`, `2048`) becomes a number and `none` / `off` /
/// the empty string becomes JSON `null`. `0` is left as `0`, which the engine already reads
/// as "unlimited". Everything else is passed through untouched, so naming the byte settings
/// explicitly is worth it — guessing from the *current* value's JSON type used to make the
/// limit un-switchable-off once it had been set to a number.
fn coerce_setting(key: &str, parsed: serde_json::Value) -> Result<serde_json::Value> {
    use serde_json::Value;
    if !BYTE_SETTINGS.contains(&key) {
        return Ok(parsed);
    }
    let Value::String(s) = parsed else {
        // A bare number, or something that already parsed as JSON: leave the type check to
        // serde when the value is read back into `Config`.
        return Ok(parsed);
    };
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        return Ok(Value::Null);
    }
    match crate::util::parse_size(s) {
        Some(n) => Ok(Value::Number(n.into())),
        None => Err(RiptideError::other(format!(
            "{key} wants a byte count or \"none\", got {s:?} (try 500M, 2048, or none)"
        ))),
    }
}

fn cmd_config(args: ConfigArgs) -> Result<()> {
    match args.action {
        ConfigAction::Show => {
            let cfg = Config::load();
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        ConfigAction::Path => {
            println!("{}", Config::path().display());
            Ok(())
        }
        ConfigAction::Init { force } => {
            let path = Config::path();
            if path.exists() && !force {
                return Err(RiptideError::other(format!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                )));
            }
            let cfg = Config::default();
            cfg.save()?;
            println!("wrote {}", path.display());
            Ok(())
        }
        ConfigAction::Set { key, value } => {
            let cfg = Config::load();
            let mut v = serde_json::to_value(&cfg)?;
            let obj = v
                .as_object_mut()
                .ok_or_else(|| RiptideError::other("configuration is not an object"))?;
            if !obj.contains_key(&key) {
                let mut known: Vec<&String> = obj.keys().collect();
                known.sort();
                return Err(RiptideError::other(format!(
                    "unknown setting {key:?}; known settings: {}",
                    known
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let parsed: serde_json::Value = serde_json::from_str(&value)
                .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
            let parsed = coerce_setting(&key, parsed)?;
            obj.insert(key.clone(), parsed);
            let updated: Config = serde_json::from_value(v)?;
            updated.save()?;
            println!("{key} set");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

async fn cmd_project(args: ProjectArgs, quiet: bool) -> Result<()> {
    let cfg = Config::load();
    let store = ProjectStore::open_in_state_dir(&cfg.state_dir)?;
    match args.action {
        ProjectAction::Add(a) => cmd_project_add(&cfg, &store, a, quiet).await,
        ProjectAction::List { json } => cmd_project_list(&store, json),
        ProjectAction::Show { name } => cmd_project_show(&store, &name),
        ProjectAction::Run { name } => cmd_project_run(&cfg, &store, &name).await,
        ProjectAction::Tick { force } => cmd_project_tick(&cfg, &store, force, quiet).await,
        ProjectAction::Enable { name } => cmd_project_toggle(&store, &name, true),
        ProjectAction::Disable { name } => cmd_project_toggle(&store, &name, false),
        ProjectAction::Remove { name } => cmd_project_remove(&store, &name),
    }
}

async fn cmd_project_add(
    cfg: &Config,
    store: &ProjectStore,
    a: ProjectAddArgs,
    quiet: bool,
) -> Result<()> {
    project::validate_name(&a.name)?;
    if store.exists(&a.name) {
        return Err(RiptideError::other(format!(
            "a project named {:?} already exists; remove it first, or choose another name",
            a.name
        )));
    }
    let interval_secs = project::parse_interval(&a.every)?;
    if interval_secs < project::BUSY_INTERVAL_SECS && !quiet {
        eprintln!(
            "warning: {:?} will run every {interval_secs}s, which is probably more often \
             than you meant",
            a.name
        );
    }

    let kind: ProjectKind = a.kind.into();
    let output = match &a.output {
        Some(o) => o.clone(),
        None => match kind {
            // A file project's default name comes from the URL, so it lands where a plain
            // `download` of the same URL would have put it.
            ProjectKind::File => cfg
                .download_dir
                .join(util::filename_from_url(&a.url).unwrap_or_else(|| a.name.clone())),
            ProjectKind::Site => cfg.download_dir.join(&a.name),
        },
    };

    let mut p = Project::new(a.name.clone(), kind, a.url.clone(), output, interval_secs);
    p.ctx = build_ctx(&a.net, cfg)?;
    p.connections = a.net.connections.unwrap_or(cfg.connections);
    p.quality = a.quality.clone();

    if kind == ProjectKind::Site {
        p.grab = GrabberConfig {
            depth: a.depth,
            max_pages: a.max_pages.unwrap_or(grabber::DEFAULT_MAX_PAGES),
            same_host: !a.any_host,
            include: a.include.clone(),
            exclude: a.exclude.clone(),
            media_only: !a.pages,
        };
        p.download = a.download;
        p.archive = a.archive;
        p.max_downloads = a.max_downloads.max(1);
        if a.download || a.archive {
            std::fs::create_dir_all(&p.output).ok();
        }
    } else if a.download || a.archive {
        return Err(RiptideError::other(
            "--download and --archive only apply to `--kind site`; a `file` project always \
             fetches its URL",
        ));
    }

    if a.start_now {
        p.next_run_ms = Some(util::now_ms());
    }

    let runner = ProjectRunner::new(cfg.clone());
    if a.start_now {
        // Save before running: a crash mid-download should leave the project recorded,
        // with the error on it, rather than losing the definition entirely.
        store.save(&p)?;
        let outcome = runner.run(&mut p).await;
        store.save(&p)?;
        report_run(&p.name, outcome, p.last_error.as_deref());
        if outcome.is_failure() {
            return Err(RiptideError::other(format!(
                "project {:?} was created, but its first run did not succeed",
                p.name
            )));
        }
    } else {
        store.save(&p)?;
    }

    if !quiet {
        println!(
            "{:?} ({}) every {}, next run {}",
            p.name,
            p.kind.as_str(),
            human_interval(p.interval_secs),
            p.schedule_summary(util::now_ms()),
        );
        println!("  url    {}", p.url);
        println!("  output {}", p.output.display());
        if p.next_run_ms.is_some() && !a.start_now {
            println!(
                "  run it now with `riptide project run {}`, or let the daemon run it \
                 when it is due",
                p.name
            );
        }
    }
    Ok(())
}

fn human_interval(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn report_run(name: &str, outcome: RunOutcome, error: Option<&str>) {
    println!("{name}: {}", outcome.as_str());
    if let Some(e) = error {
        eprintln!("  {e}");
    }
}

fn cmd_project_list(store: &ProjectStore, json: bool) -> Result<()> {
    let projects = store.list()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    }
    if projects.is_empty() {
        println!("no projects");
        return Ok(());
    }
    let now = util::now_ms();
    // Column widths follow the data, so a long name does not push the table apart.
    let name_w = projects
        .iter()
        .map(|p| p.name.len())
        .chain(std::iter::once("NAME".len()))
        .max()
        .unwrap_or(4);
    let kind_w = 4; // "kind"
    println!(
        "{:<name_w$}  {:<kind_w$}  {:<7}  {:<12}  LAST RUN",
        "NAME", "KIND", "EVERY", "NEXT",
    );
    for p in &projects {
        println!(
            "{:<name_w$}  {:<kind_w$}  {:<7}  {:<12}  {}",
            p.name,
            p.kind.as_str(),
            human_interval(p.interval_secs),
            p.schedule_summary(now),
            p.last_run_summary(now),
        );
    }
    Ok(())
}

fn cmd_project_show(store: &ProjectStore, name: &str) -> Result<()> {
    let p = store.load(name)?;
    let now = util::now_ms();
    println!("{} ({})", p.name, p.kind.as_str());
    println!("  url        {}", p.url);
    println!("  output     {}", p.output.display());
    println!("  every      {}", human_interval(p.interval_secs));
    println!(
        "  state      {}",
        if p.enabled { "enabled" } else { "disabled" }
    );
    println!("  next run   {}", p.schedule_summary(now));
    println!("  last run   {}", p.last_run_summary(now));
    println!("  runs       {}", p.runs);
    if p.failures > 0 {
        println!("  failing    {} consecutive run(s)", p.failures);
    }
    if let Some(e) = &p.last_error {
        println!("  error      {e}");
    }
    if let Some(n) = p.last_bytes {
        println!("  bytes      {}", util::human_bytes(n));
    }
    if p.kind == ProjectKind::Site {
        println!("  crawl      depth {}", p.grab.depth);
        println!("  same host  {}", p.grab.same_host);
        if !p.grab.include.is_empty() {
            println!("  include    {}", p.grab.include.join(", "));
        }
        if !p.grab.exclude.is_empty() {
            println!("  exclude    {}", p.grab.exclude.join(", "));
        }
        if p.archive {
            println!("  mode       archive (offline mirror, links rewritten)");
            if let (Some(f), Some(x), Some(s)) = (p.last_found, p.last_fetched, p.last_skipped) {
                println!("  last pass  {f} file(s): {x} written, {s} already current");
            }
        } else {
            println!(
                "  download   {}",
                if p.download {
                    "yes"
                } else {
                    "no (planning only)"
                }
            );
            if let (Some(f), Some(x), Some(s)) = (p.last_found, p.last_fetched, p.last_skipped) {
                println!("  last crawl {f} link(s): {x} fetched, {s} already current");
            }
        }
    } else {
        match (&p.validator.etag, &p.validator.last_modified) {
            (None, None) => println!("  validator  none yet (first run has not happened)"),
            (etag, lm) => {
                if let Some(e) = etag {
                    println!("  etag       {e}");
                }
                if let Some(l) = lm {
                    println!("  modified   {l}");
                }
            }
        }
    }
    Ok(())
}

async fn cmd_project_run(cfg: &Config, store: &ProjectStore, name: &str) -> Result<()> {
    let mut p = store.load(name)?;
    let runner = ProjectRunner::new(cfg.clone());
    let outcome = runner.run(&mut p).await;
    store.save(&p)?;
    report_run(&p.name, outcome, p.last_error.as_deref());
    if outcome.is_failure() {
        return Err(RiptideError::other(format!(
            "project {name:?} did not succeed"
        )));
    }
    Ok(())
}

async fn cmd_project_tick(
    cfg: &Config,
    store: &ProjectStore,
    force: bool,
    quiet: bool,
) -> Result<()> {
    let now = util::now_ms();
    // `--force` means "run it now even though it is not due"; it never means "run the
    // projects the user switched off".
    let mut projects = store.list()?;
    if !force {
        projects.retain(|p| p.is_due(now));
    }
    projects.retain(|p| p.enabled);
    if projects.is_empty() {
        if !quiet {
            println!("no projects due");
        }
        return Ok(());
    }

    let runner = ProjectRunner::new(cfg.clone());
    let mut failed = Vec::new();
    for mut p in projects {
        let outcome = runner.run(&mut p).await;
        store.save(&p)?;
        if !quiet {
            report_run(&p.name, outcome, p.last_error.as_deref());
        }
        if outcome.is_failure() {
            failed.push(p.name);
        }
    }
    if !failed.is_empty() {
        // A non-zero exit is what makes `cron` able to tell somebody.
        return Err(RiptideError::other(format!(
            "{} project(s) failed: {}",
            failed.len(),
            failed.join(", ")
        )));
    }
    Ok(())
}

fn cmd_project_toggle(store: &ProjectStore, name: &str, enabled: bool) -> Result<()> {
    let mut p = store.load(name)?;
    p.enabled = enabled;
    p.updated_ms = util::now_ms();
    store.save(&p)?;
    println!("{name}: {}", if enabled { "enabled" } else { "disabled" });
    Ok(())
}

fn cmd_project_remove(store: &ProjectStore, name: &str) -> Result<()> {
    // Deliberately does not touch what the project downloaded: deleting a schedule should
    // never be able to delete somebody's files.
    let p = store.load(name)?;
    store.delete(name)?;
    println!("{name}: removed");
    println!("  kept {}", p.output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn byte_settings_accept_sizes_and_can_be_switched_off() {
        use serde_json::json;
        let coerce = |key: &str, raw: &str| {
            let parsed = serde_json::from_str(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
            coerce_setting(key, parsed)
        };

        assert_eq!(
            coerce("quota_bytes", "500M").unwrap(),
            json!(524_288_000u64)
        );
        assert_eq!(coerce("quota_bytes", "2G").unwrap(), json!(2u64 << 30));
        assert_eq!(
            coerce("speed_limit", "4194304").unwrap(),
            json!(4_194_304u64)
        );
        // Switch-off words, and 0, meaning "unlimited".
        assert_eq!(
            coerce("quota_bytes", "none").unwrap(),
            serde_json::Value::Null
        );
        assert_eq!(
            coerce("quota_bytes", "off").unwrap(),
            serde_json::Value::Null
        );
        assert_eq!(coerce("quota_bytes", "0").unwrap(), json!(0));

        // A non-byte setting is untouched even when it looks like a size.
        assert_eq!(coerce("user_agent", "500M").unwrap(), json!("500M"));
        assert_eq!(coerce("connections", "16").unwrap(), json!(16));

        assert!(coerce("quota_bytes", "banana").is_err());
    }

    #[test]
    fn relative_and_absolute_schedules_parse() {
        let now = util::now_ms();
        let t = parse_when("+1m").unwrap();
        assert!(t > now && t <= now + 61_000, "got {t} vs now {now}");

        let t = parse_when("2026-01-02T03:04:05Z").unwrap();
        assert_eq!(t, 1_767_323_045_000);

        assert!(parse_when("tomorrow-ish").is_err());
    }

    #[test]
    fn net_options_build_a_request_context() {
        let cfg = Config::default();
        let net = NetOpts {
            referer: Some("https://h/page".into()),
            cookie: Some("a=b".into()),
            auth: Some("bob:hunter2".into()),
            headers: vec!["X-Test: 1".into(), "Accept-Language: en".into()],
            insecure: true,
            ..Default::default()
        };
        let ctx = build_ctx(&net, &cfg).unwrap();
        assert_eq!(ctx.referer.as_deref(), Some("https://h/page"));
        assert_eq!(ctx.cookie.as_deref(), Some("a=b"));
        assert!(ctx.insecure);
        assert_eq!(ctx.headers.len(), 2);
        assert!(ctx.auth.is_some());
    }

    #[test]
    fn a_malformed_header_is_rejected() {
        let cfg = Config::default();
        let net = NetOpts {
            headers: vec!["not-a-header".into()],
            ..Default::default()
        };
        assert!(build_ctx(&net, &cfg).is_err());
    }

    #[test]
    fn a_malformed_auth_spec_is_rejected() {
        let cfg = Config::default();
        let net = NetOpts {
            auth: Some("garbage".into()),
            ..Default::default()
        };
        assert!(build_ctx(&net, &cfg).is_err());
    }

    #[test]
    fn download_args_become_a_job() {
        let cfg = Config::default();
        let args = DownloadArgs {
            urls: vec![],
            net: NetOpts {
                connections: Some(16),
                retries: Some(9),
                limit: Some("2M".into()),
                ..Default::default()
            },
            output: None,
            input_file: None,
            quality: Some("720".into()),
            kind: None,
            scheduled: None,
            jobs: None,
            no_queue: true,
        };
        let job = build_job(&cfg, "https://h/a.bin", &args, None).unwrap();
        assert_eq!(job.connections, 16);
        assert_eq!(job.max_retries, 9);
        assert_eq!(job.speed_limit, Some(2 * 1024 * 1024));
        assert_eq!(job.quality.as_deref(), Some("720"));
        assert!(job.output_provisional, "the engine names the file");
        assert_eq!(job.kind, JobKind::Http);
    }

    #[test]
    fn media_jobs_get_an_mp4_output_immediately() {
        let mut cfg = Config::default();
        cfg.download_dir = PathBuf::from("/tmp/dl");
        let args = DownloadArgs {
            urls: vec![],
            net: NetOpts::default(),
            output: None,
            input_file: None,
            quality: None,
            kind: None,
            scheduled: None,
            jobs: None,
            no_queue: true,
        };
        let job = build_job(&cfg, "https://h/dir/movie.m3u8", &args, None).unwrap();
        assert_eq!(job.kind, JobKind::Hls);
        assert!(!job.output_provisional);
        assert_eq!(job.output, PathBuf::from("/tmp/dl/Video/movie.mp4"));
    }

    #[test]
    fn an_explicit_output_is_used_verbatim() {
        let cfg = Config::default();
        let args = DownloadArgs {
            urls: vec![],
            net: NetOpts::default(),
            output: None,
            input_file: None,
            quality: None,
            kind: None,
            scheduled: None,
            jobs: None,
            no_queue: true,
        };
        let job = build_job(
            &cfg,
            "https://h/a.iso",
            &args,
            Some(PathBuf::from("/tmp/here.iso")),
        )
        .unwrap();
        assert_eq!(job.output, PathBuf::from("/tmp/here.iso"));
        assert!(!job.output_provisional);
    }

    #[test]
    fn several_urls_write_into_the_output_directory() {
        let args = DownloadArgs {
            urls: vec![],
            net: NetOpts::default(),
            output: Some(PathBuf::from("/tmp/out")),
            input_file: None,
            quality: None,
            kind: None,
            scheduled: None,
            jobs: None,
            no_queue: true,
        };
        assert_eq!(
            output_for(0, 2, "https://h/a.one", &args),
            Some(PathBuf::from("/tmp/out/a.one"))
        );
        assert_eq!(
            output_for(0, 1, "https://h/solo.bin", &args),
            Some(PathBuf::from("/tmp/out"))
        );
    }

    #[test]
    fn the_progress_bar_renders_monotonically() {
        assert_eq!(render_bar(0.0, 4), "[....]");
        assert_eq!(render_bar(0.5, 4), "[##..]");
        assert_eq!(render_bar(1.0, 4), "[####]");
        assert_eq!(render_bar(2.0, 4), "[####]");
    }
}
