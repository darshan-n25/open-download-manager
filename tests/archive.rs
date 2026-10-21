//! End-to-end tests for offline site archiving.
//!
//! These stand up a real HTTP server that serves a small site — a page, a stylesheet, a
//! script, two images, and a second page one hop away — and then assert the properties that
//! actually matter to somebody who wants to read the copy with the network off:
//!
//! * every reference the rewritten markup makes points at a file that exists;
//! * a reference the run could not hold points at the live site instead of at nothing;
//! * a run that changes nothing on disk says so, and moves no asset bodies;
//! * an asset that changes on its own is picked up even though the pages around it did not.

// Test fixtures are built field-by-field from `Default::default()`, which reads better than
// a long struct literal.
#![allow(clippy::field_reassign_with_default)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use odm::archive::{self, ArchiveConfig, ArchiveReport};
use odm::config::Config;
use odm::engine::Engine;
use odm::grabber::GrabberConfig;
use odm::job::{Job, JobKind};
use odm::net::ServerLimits;
use odm::project::{Project, ProjectKind, ProjectRunner, RunOutcome};
use odm::quota::Quota;
use odm::throttle::RateLimiter;

// ---------------------------------------------------------------------------
// A small server that really does conditional requests
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// Path → body, served with the content type implied by the extension.
    files: Vec<(String, Vec<u8>)>,
    /// Pages served as `text/html`, by path.
    pages: Vec<(String, String)>,
}

#[derive(Default)]
struct Log {
    /// Paths whose body was actually written to a socket this run.
    served: Vec<String>,
    /// Paths answered `304`, in order.
    unchanged: Vec<String>,
    /// Paths whose response carried `If-None-Match`.
    conditional: Vec<String>,
}

struct Site {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    log: Arc<Mutex<Log>>,
    requests: Arc<AtomicUsize>,
}

impl Site {
    async fn start() -> Arc<Site> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = Arc::new(Site {
            addr,
            state: Arc::new(Mutex::new(State::default())),
            log: Arc::new(Mutex::new(Log::default())),
            requests: Arc::new(AtomicUsize::new(0)),
        });

        let s = srv.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let s2 = s.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    serve(&mut sock, s2).await;
                });
            }
        });
        srv
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn page(&self, path: &str, html: &str) {
        let path = absolute(path);
        let mut st = self.state.lock().unwrap();
        st.pages.retain(|(p, _)| *p != path);
        st.pages.push((path, html.to_string()));
    }

    fn file(&self, path: &str, body: impl Into<Vec<u8>>) {
        let path = absolute(path);
        let mut st = self.state.lock().unwrap();
        let body = body.into();
        match st.files.iter_mut().find(|(p, _)| *p == path) {
            Some(slot) => slot.1 = body,
            None => st.files.push((path, body)),
        }
    }

    /// Forget what was served, so a second run can be measured on its own.
    fn reset_log(&self) {
        let mut log = self.log.lock().unwrap();
        log.served.clear();
        log.unchanged.clear();
        log.conditional.clear();
    }

    fn served(&self) -> Vec<String> {
        let mut v = self.log.lock().unwrap().served.clone();
        v.sort();
        v
    }

    fn unchanged(&self) -> Vec<String> {
        let mut v = self.log.lock().unwrap().unchanged.clone();
        v.sort();
        v
    }

    fn conditional(&self) -> Vec<String> {
        let mut v = self.log.lock().unwrap().conditional.clone();
        v.sort();
        v
    }

    /// The whole site, in one call, so each test only shows the part it varies.
    fn small_site(&self) {
        self.page(
            "/",
            r##"<!doctype html><html><head>
                 <link rel="stylesheet" href="style.css">
                 <script src="/js/app.js"></script>
               </head><body>
                 <img src="img/logo.png" alt="logo">
                 <a href="about/">about</a>
                 <a href="https://elsewhere.test/x">elsewhere</a>
                 <a href="mailto:nobody@example.test">mail</a>
                 <a href="#top">top</a>
               </body></html>"##,
        );
        self.page(
            "/about/",
            r##"<!doctype html><html><body>
                 <a href="/">home</a>
                 <img src="/img/logo.png">
               </body></html>"##,
        );
        self.file("style.css", b"body{background:url(img/bg.png)}".to_vec());
        self.file("/js/app.js", b"console.log('hi');".to_vec());
        self.file("/img/logo.png", b"\x89PNG\r\n\x1a\nlogo-bytes-1".to_vec());
        self.file("/img/bg.png", b"\x89PNG\r\n\x1a\nbg-bytes-1".to_vec());
    }
}

/// A request path always starts with `/`, so fixtures should too, whether or not the site's
/// own markup happens to write it that way.
fn absolute(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn etag_for(body: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("\"{h:016x}-{}\"", body.len())
}

fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "text/javascript"
    } else if path.ends_with(".png") {
        "image/png"
    } else {
        "application/octet-stream"
    }
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_value(head: &str, name: &str) -> Option<String> {
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

async fn serve(sock: &mut TcpStream, srv: Arc<Site>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Requests here are header-only, so the head is the whole request.
    let head_end = loop {
        if let Some(i) = header_end(&buf) {
            break i;
        }
        match sock.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    };
    srv.requests.fetch_add(1, Ordering::SeqCst);

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // The body and its content type, whichever collection it lives in. Scoped so the lock
    // is released before anything awaits: a guard held across an await makes the task
    // non-`Send`.
    let found = {
        let st = srv.state.lock().unwrap();
        if let Some((_, html)) = st.pages.iter().find(|(p, _)| *p == path) {
            Some((html.clone().into_bytes(), "text/html; charset=utf-8"))
        } else {
            st.files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, b)| (b.clone(), content_type_for(&path)))
        }
    };
    let Some((body, content_type)) = found else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };

    let etag = etag_for(&body);
    let last_modified = "Wed, 21 Oct 2015 07:28:00 GMT";

    if let Some(inm) = header_value(&head, "if-none-match") {
        srv.log.lock().unwrap().conditional.push(path.clone());
        let hit = inm.split(',').map(str::trim).any(|c| c == etag);
        if hit {
            srv.log.lock().unwrap().unchanged.push(path.clone());
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\n\
                         Last-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await;
            return;
        }
    }

    let head_txt = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         ETag: {etag}\r\nLast-Modified: {last_modified}\r\n\
         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head_txt.as_bytes()).await;
    if method != "HEAD" {
        srv.log.lock().unwrap().served.push(path.clone());
        let _ = sock.write_all(&body).await;
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn test_config(state_dir: &Path, download_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.state_dir = state_dir.to_path_buf();
    cfg.download_dir = download_dir.to_path_buf();
    cfg.connections = 4;
    cfg.connect_timeout_secs = 2;
    cfg.read_timeout_secs = 5;
    cfg.retry_backoff_ms = 1;
    cfg.max_retries = 1;
    cfg.auto_categorize = false;
    cfg
}

/// An engine, which is where the archiver's fetcher comes from.
fn engine_for(cfg: &Config, url: &str) -> Engine {
    let job = Job::new(
        "archive-test".into(),
        url.to_string(),
        cfg.download_dir.join(".probe"),
        JobKind::Grabber,
    );
    Engine::new(cfg.clone(), job).unwrap()
}

async fn run_archive(cfg: &Config, url: &str, root: &Path, acfg: ArchiveConfig) -> ArchiveReport {
    let engine = engine_for(cfg, url);
    let report = archive::archive(&engine.fetcher(), url, &acfg, root)
        .await
        .unwrap();
    engine.quota().flush();
    report
}

fn acfg(depth: usize) -> ArchiveConfig {
    ArchiveConfig {
        grab: GrabberConfig {
            depth,
            media_only: false,
            ..GrabberConfig::default()
        },
        ..ArchiveConfig::default()
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every local reference in a document, resolved against the document's own directory.
///
/// This is the assertion the feature lives or dies on: a rewrite that produces a path which
/// is not there shows up as a hole when the archive is read with the network off.
fn local_refs(document: &Path, text: &str) -> Vec<PathBuf> {
    let dir = document.parent().unwrap_or(Path::new(""));
    let mut out = Vec::new();
    for pattern in [
        r#"(?i)\b(?:href|src|poster)\s*=\s*"([^"]*)""#,
        r#"(?i)\b(?:href|src|poster)\s*=\s*'([^']*)'"#,
        r#"(?i)url\(\s*["']?([^'")]*)"#,
    ] {
        let re = regex::Regex::new(pattern).unwrap();
        for caps in re.captures_iter(text) {
            let raw = caps[1].trim();
            if raw.is_empty()
                || raw.starts_with('#')
                || raw.starts_with("http://")
                || raw.starts_with("https://")
                || raw.starts_with("mailto:")
                || raw.starts_with("data:")
                || raw.starts_with("//")
            {
                continue;
            }
            out.push(dir.join(raw));
        }
    }
    out
}

#[tokio::test]
async fn an_archive_holds_the_page_and_everything_it_needs_to_render() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    let report = run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.documents, 2, "the start page and the page it links");
    assert_eq!(report.stylesheets, 1);
    assert_eq!(report.assets, 3, "app.js, logo.png, bg.png");
    assert!(report.written > 0);

    for expected in [
        "index.html",
        "about/index.html",
        "style.css",
        "js/app.js",
        "img/logo.png",
        "img/bg.png",
    ] {
        assert!(root.join(expected).exists(), "{expected} is missing");
    }

    let home = read(&root.join("index.html"));
    assert!(home.contains(r#"href="style.css""#), "{home}");
    assert!(home.contains(r#"src="js/app.js""#), "{home}");
    assert!(home.contains(r#"src="img/logo.png""#), "{home}");
    assert!(home.contains(r#"href="about/index.html""#), "{home}");
    assert!(
        home.contains("https://elsewhere.test/x"),
        "another site's link is not ours to archive: {home}"
    );
    assert!(
        home.contains(r#"href="mailto:nobody@example.test""#),
        "{home}"
    );
    assert!(home.contains(r##"href="#top""##), "{home}");

    let about = read(&root.join("about/index.html"));
    assert!(about.contains(r#"src="../img/logo.png""#), "{about}");
    assert!(about.contains(r#"href="../index.html""#), "{about}");

    let css = read(&root.join("style.css"));
    assert!(css.contains("url(img/bg.png)"), "{css}");

    // And the property that matters: nothing the markup points at is missing.
    for doc in ["index.html", "about/index.html", "style.css"] {
        let path = root.join(doc);
        for target in local_refs(&path, &read(&path)) {
            assert!(
                target.exists(),
                "{doc} points at {}, which is not in the archive",
                target.display()
            );
        }
    }
}

#[tokio::test]
async fn a_second_run_moves_no_asset_bodies_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    let first = run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;
    assert!(first.written > 0);
    assert!(first.validators.len() >= 3, "{:?}", first.validators.keys());
    // The record of what the server said travels with the mirror, so a repeat run of a
    // one-shot `grab --archive` is incremental too, not just a scheduled one.
    assert!(
        archive::manifest_path(&root).exists(),
        "the archive should have written its manifest"
    );

    site.reset_log();
    let second = run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;

    assert!(second.failures.is_empty(), "{:?}", second.failures);
    assert!(second.is_unchanged(), "nothing changed on disk: {second:?}");
    assert_eq!(second.written, 0);

    // Pages and stylesheets are re-read by design, because their text is how the assets
    // were found in the first place.
    assert_eq!(
        site.served(),
        vec![
            "/".to_string(),
            "/about/".to_string(),
            "/style.css".to_string()
        ],
        "only the pages should have been transferred again"
    );
    assert_eq!(
        site.unchanged(),
        vec![
            "/img/bg.png".to_string(),
            "/img/logo.png".to_string(),
            "/js/app.js".to_string()
        ],
        "every asset should have been asked about conditionally"
    );
    assert_eq!(
        site.conditional(),
        vec![
            "/img/bg.png".to_string(),
            "/img/logo.png".to_string(),
            "/js/app.js".to_string()
        ],
        "only assets should carry a conditional header; pages are always re-read"
    );
    assert_eq!(
        second.unchanged, 6,
        "three assets the server confirmed, plus the three documents whose rewritten bytes \
         were already on disk"
    );
}

#[tokio::test]
async fn an_asset_is_picked_up_when_it_changes_on_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;
    assert_eq!(
        std::fs::read(root.join("img/logo.png")).unwrap(),
        b"\x89PNG\r\n\x1a\nlogo-bytes-1",
        "the first version landed"
    );

    // The pages are untouched; only the image moves. Asking before fetching is the only way
    // this can be noticed at all.
    site.file("/img/logo.png", b"\x89PNG\r\n\x1a\nlogo-bytes-2".to_vec());
    let second = run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;

    assert!(second.failures.is_empty(), "{:?}", second.failures);
    assert_eq!(
        std::fs::read(root.join("img/logo.png")).unwrap(),
        b"\x89PNG\r\n\x1a\nlogo-bytes-2",
        "the new image should have replaced the old one"
    );
    assert_eq!(second.written, 1, "only the image changed");
    assert!(!second.is_unchanged());
}

#[tokio::test]
async fn a_changed_stylesheet_is_reread_and_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;
    site.file("style.css", b"body{background:url(img/other.png)}".to_vec());
    site.file("/img/other.png", b"\x89PNG\r\n\x1a\nother".to_vec());

    let second = run_archive(&cfg, &site.url("/"), &root, acfg(1)).await;
    assert!(second.failures.is_empty(), "{:?}", second.failures);
    assert!(
        read(&root.join("style.css")).contains("url(img/other.png)"),
        "the stylesheet should have been re-read"
    );
    assert!(
        root.join("img/other.png").exists(),
        "a new reference in a stylesheet has to be followed"
    );
}

#[tokio::test]
async fn an_excluded_reference_is_left_pointing_at_the_live_site() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    let mut config = acfg(1);
    config.grab.exclude = vec!["*logo*".to_string()];
    let report = run_archive(&cfg, &site.url("/"), &root, config).await;

    assert!(
        !root.join("img/logo.png").exists(),
        "the exclusion should have been honoured"
    );
    let home = read(&root.join("index.html"));
    assert!(
        home.contains(&site.url("/img/logo.png")),
        "an unheld reference has to stay reachable: {home}"
    );
    assert!(
        report.remote_links >= 3,
        "two pages point at the image, plus the off-site link: {}",
        report.remote_links
    );
}

#[tokio::test]
async fn pages_past_the_depth_limit_are_not_followed_but_their_assets_are_not_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let site = Site::start().await;
    site.small_site();
    let root = tmp.path().join("mirror");

    let report = run_archive(&cfg, &site.url("/"), &root, acfg(0)).await;

    assert_eq!(report.documents, 1, "only the start page");
    assert!(!root.join("about/index.html").exists());
    // The start page still needs its own pictures, so depth only limits page-to-page hops.
    assert!(root.join("img/logo.png").exists());
    assert!(root.join("style.css").exists());
    let home = read(&root.join("index.html"));
    assert!(
        home.contains(&site.url("/about/")),
        "the page we did not follow stays reachable: {home}"
    );
}

#[tokio::test]
async fn an_unreachable_site_reports_the_failure_rather_than_leaving_a_partial_mirror() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    // A port nothing is listening on, obtained by binding and dropping: connecting is
    // refused at once rather than hanging until the connect timeout.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);
    let root = tmp.path().join("mirror");

    let url = format!("http://{dead}/index.html");
    let engine = engine_for(&cfg, &url);
    let report = archive::archive(&engine.fetcher(), &url, &acfg(1), &root)
        .await
        .unwrap();

    assert_eq!(report.written, 0);
    assert_eq!(report.failures.len(), 1);
    assert!(!report.is_unchanged(), "a failure is not 'unchanged'");
}

#[tokio::test]
async fn a_file_url_is_refused_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let root = tmp.path().join("mirror");
    let url = "http://example.test/movie.mp4";
    let engine = engine_for(&cfg, url);

    let err = archive::archive(&engine.fetcher(), url, &acfg(1), &root)
        .await
        .unwrap_err();
    // The message has to point at the command that does want a file.
    assert!(err.to_string().contains("odm-cli download"), "{err}");
    assert!(!root.exists(), "nothing should have been created");
}

// ---------------------------------------------------------------------------
// The same thing, on a schedule
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    cfg: Config,
    runner: ProjectRunner,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(&tmp.path().join("state"), &tmp.path().join("dl"));
    let runner = ProjectRunner::with_limits(
        cfg.clone(),
        Arc::new(RateLimiter::new(None)),
        Arc::new(ServerLimits::new(0)),
        Arc::new(Quota::new(None, std::time::Duration::from_secs(3600))),
    );
    Fixture {
        _tmp: tmp,
        cfg,
        runner,
    }
}

fn archive_project(name: &str, url: &str, output: &Path) -> Project {
    let mut p = Project::new(name, ProjectKind::Site, url, output.to_path_buf(), 3600);
    p.grab = GrabberConfig {
        depth: 1,
        max_pages: 50,
        same_host: true,
        include: Vec::new(),
        exclude: Vec::new(),
        media_only: false,
    };
    p.archive = true;
    p
}

#[tokio::test]
async fn a_scheduled_archive_reports_updated_then_unchanged() {
    let f = fixture();
    let site = Site::start().await;
    site.small_site();
    let root = f.cfg.download_dir.join("mirror");
    let mut p = archive_project("mirror", &site.url("/"), &root);

    let first = f.runner.run(&mut p).await;
    assert_eq!(first, RunOutcome::Updated, "{:?}", p.last_error);
    assert!(root.join("index.html").exists());
    assert!(p.last_found.unwrap() >= 6, "held: {:?}", p.last_found);
    assert!(
        p.last_fetched.unwrap() > 0,
        "the first run writes files: {:?}",
        p.last_fetched
    );
    assert!(
        archive::manifest_path(&root).exists(),
        "what the server said about each asset has to be kept for the next run"
    );
    assert!(
        p.validators.is_empty(),
        "an archive keeps its validators next to the mirror, not in the project document"
    );

    // Running it again must not touch the files: the value of a schedule is that the cheap
    // run is genuinely cheap.
    let before = std::fs::metadata(root.join("index.html"))
        .unwrap()
        .modified()
        .unwrap();
    let second = f.runner.run(&mut p).await;
    assert_eq!(second, RunOutcome::Unchanged, "{:?}", p.last_error);
    assert_eq!(p.last_fetched, Some(0));
    assert!(
        p.last_skipped.unwrap() >= 3,
        "assets the server confirmed: {:?}",
        p.last_skipped
    );
    let after = std::fs::metadata(root.join("index.html"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "an unchanged run must not rewrite files");
    assert!(!second.wrote_bytes());
}

#[tokio::test]
async fn a_scheduled_archive_that_cannot_reach_its_site_keeps_the_copy_it_has() {
    let f = fixture();
    let site = Site::start().await;
    site.small_site();
    let root = f.cfg.download_dir.join("mirror");
    let mut p = archive_project("mirror", &site.url("/"), &root);

    assert_eq!(f.runner.run(&mut p).await, RunOutcome::Updated);
    let good = std::fs::read(root.join("index.html")).unwrap();

    // Point the project at a site that is not there any more.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);
    p.url = format!("http://{dead}/");

    let outcome = f.runner.run(&mut p).await;
    assert_eq!(outcome, RunOutcome::Failed, "{:?}", p.last_error);
    assert!(outcome.is_failure());
    assert_eq!(
        std::fs::read(root.join("index.html")).unwrap(),
        good,
        "a failed run must leave the copy it could not replace alone"
    );
}
