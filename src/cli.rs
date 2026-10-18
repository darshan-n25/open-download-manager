//! Command line interface.
//!
//! The shape deliberately matches what IDM users expect: fire-and-forget downloads, a
//! persistent queue you can list and resume, a site grabber, and a daemon the browser
//! extension talks to.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::engine::{Engine, ProgressFn, ProgressSample};
use crate::error::{Result, RiptideError};
use crate::grabber::{self, GrabberConfig, TargetKind};
use crate::job::{Job, JobKind, JobState};
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

    /// Credentials: `user:pass` (Basic), `digest:user:pass`, `ntlm:user:pass:domain`,
    /// or `bearer:token`.
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

    /// Directory for `--download`.
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
        Command::StateDir => {
            println!("{}", Config::load().state_dir.display());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

/// Guess the transfer kind from the URL, before any network traffic.
pub fn guess_kind(url: &str) -> JobKind {
    let lower = url.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
        JobKind::Ftp
    } else if path.contains(".m3u8") {
        JobKind::Hls
    } else if path.contains(".mpd") {
        JobKind::Dash
    } else {
        JobKind::Http
    }
}

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
                "cannot parse --auth {a:?}; expected user:pass, digest:user:pass, \
                 ntlm:user:pass:domain, or bearer:token"
            ))
        })?);
    }
    for h in &net.headers {
        let (k, v) = h.split_once(':').ok_or_else(|| {
            RiptideError::other(format!("--header {h:?} must be 'Name: value'"))
        })?;
        ctx.headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(ctx)
}

/// Build a [`Job`] from a URL plus the command line.
fn build_job(cfg: &Config, url: &str, args: &DownloadArgs, explicit: Option<PathBuf>) -> Result<Job> {
    let kind = args.kind.map(JobKind::from).unwrap_or_else(|| guess_kind(url));

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
    if args.net.connections.is_some() {
        scratch.connections = args.net.connections.unwrap();
    }

    let engine = Engine::new(cfg.clone(), scratch)?;
    let fetcher = engine.fetcher();

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
        eprintln!("{} link(s) found; add --download to fetch them", targets.len());
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
            TargetKind::Manifest => guess_kind(&t.url),
            _ => JobKind::Http,
        };
        // With `--output-dir` every link is named after its own URL; otherwise the engine
        // derives the name from the response headers, which is more accurate.
        let explicit = args.output_dir.as_ref().and_then(|d| {
            util::filename_from_url(&t.url).map(|name| d.join(name))
        });
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
    let kind = guess_kind(&args.url);
    let job = Job::new(
        "probe".into(),
        args.url.clone(),
        cfg.download_dir.join(".probe"),
        if kind == JobKind::Ftp { JobKind::Ftp } else { JobKind::Http },
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
        "{:<10} {:<9} {:>10} {:>7}  {}",
        "ID", "STATE", "SIZE", "DONE", "NAME"
    );
    for j in &jobs {
        let pct = j.progress().map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_default();
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
    }

    let ctrl_c = spawn_ctrl_c(stop);
    let mut handles = Vec::new();
    for job in pending {
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
        eprintln!("{failed} job(s) failed or remain paused");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

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
            obj.insert(key.clone(), parsed);
            let updated: Config = serde_json::from_value(v)?;
            updated.save()?;
            println!("{key} set");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn kind_is_guessed_from_the_url() {
        assert_eq!(guess_kind("https://h/a/file.iso"), JobKind::Http);
        assert_eq!(guess_kind("https://h/live/master.m3u8?t=1"), JobKind::Hls);
        assert_eq!(guess_kind("https://h/dash/manifest.mpd"), JobKind::Dash);
        assert_eq!(guess_kind("ftp://h/pub/x.iso"), JobKind::Ftp);
        assert_eq!(guess_kind("http://h/video.M3U8"), JobKind::Hls);
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
