//! End-to-end tests for recurring projects.
//!
//! These stand up an HTTP server that really honours `If-None-Match`, really does byte
//! ranges, and can be told to fail or to change its body mid-test, because the interesting
//! properties of a synchronization project are all about what happens *between* runs:
//!
//! * an unchanged remote costs a conditional request and no bytes;
//! * a changed remote is fetched **in full**, never spliced onto the previous version;
//! * a failed run leaves the version on disk alone;
//! * a partial download from an older version is never mistaken for part of the new one.

// Test fixtures are built field-by-field from `Default::default()`, which reads better
// than a long struct literal.
#![allow(clippy::field_reassign_with_default)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use odm::config::Config;
use odm::grabber::GrabberConfig;
use odm::job::JobKind;
use odm::project::{parse_interval, Project, ProjectKind, ProjectRunner, ProjectStore, RunOutcome};

/// Deterministic pseudo-random bytes so a mismatch is obvious in a diff.
fn body_of(seed: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u64 = seed | 1;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn etag_for(body: &[u8]) -> String {
    // Any stable function of the bytes will do; it only has to change when they do.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("\"{h:016x}-{}\"", body.len())
}

#[derive(Default)]
struct State {
    /// Path → body.
    files: Vec<(String, Vec<u8>)>,
    /// Pages served as `text/html`, by path.
    pages: Vec<(String, String)>,
}

struct SyncServer {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    /// Times a conditional request arrived and matched.
    not_modified: Arc<AtomicUsize>,
    /// Times any request carried `If-None-Match`.
    conditionals: Arc<AtomicUsize>,
    /// Target body bytes actually written to a socket. Crawl pages are not counted: the
    /// question these tests ask is whether a *target* had to be transferred again.
    bytes_served: Arc<AtomicU64>,
    /// When set, `GET` on any path fails with 500 while `HEAD` keeps working — a server
    /// that can be probed but not downloaded from.
    fail_downloads: Arc<AtomicBool>,
    /// Paths whose body is served on request, for `Range` coverage.
    ranges: Arc<Mutex<Vec<(String, usize, usize)>>>,
}

impl SyncServer {
    async fn start(files: Vec<(&str, Vec<u8>)>) -> Arc<SyncServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = Arc::new(SyncServer {
            addr,
            state: Arc::new(Mutex::new(State {
                files: files.into_iter().map(|(p, b)| (p.to_string(), b)).collect(),
                pages: Vec::new(),
            })),
            not_modified: Arc::new(AtomicUsize::new(0)),
            conditionals: Arc::new(AtomicUsize::new(0)),
            bytes_served: Arc::new(AtomicU64::new(0)),
            fail_downloads: Arc::new(AtomicBool::new(false)),
            ranges: Arc::new(Mutex::new(Vec::new())),
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
                    drop(sock);
                });
            }
        });
        srv
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Replace (or add) a file body, which changes its ETag.
    fn set_body(&self, path: &str, body: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        match st.files.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = body,
            None => st.files.push((path.to_string(), body)),
        }
    }

    fn add_page(&self, path: &str, html: String) {
        self.state
            .lock()
            .unwrap()
            .pages
            .push((path.to_string(), html));
    }

    fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::SeqCst)
    }

    fn conditionals(&self) -> usize {
        self.conditionals.load(Ordering::SeqCst)
    }

    fn not_modified(&self) -> usize {
        self.not_modified.load(Ordering::SeqCst)
    }

    fn observed_ranges(&self) -> Vec<(String, usize, usize)> {
        self.ranges.lock().unwrap().clone()
    }

    fn fail_downloads(&self, on: bool) {
        self.fail_downloads.store(on, Ordering::SeqCst);
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

async fn serve(sock: &mut TcpStream, srv: Arc<SyncServer>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Requests here are header-only (GET/HEAD, no body), so the head is the whole request.
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
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    if let Some(inm) = header_value(&head, "if-none-match") {
        srv.conditionals.fetch_add(1, Ordering::SeqCst);
        // Only a real match produces 304; a stale ETag must fall through to a full response.
        let current = current_etag(&srv, &path);
        if let Some(cur) = current {
            // `If-None-Match` may be a list; a `*` never matches a file we are not sure about.
            let hit = inm
                .split(',')
                .map(|s| s.trim())
                .any(|candidate| candidate == cur);
            if hit {
                srv.not_modified.fetch_add(1, Ordering::SeqCst);
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 304 Not Modified\r\nETag: {cur}\r\n\
                             Accept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
                return;
            }
        }
    }

    if method == "GET" && srv.fail_downloads.load(Ordering::SeqCst) {
        let _ = sock
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }

    // Pages first: they are always served whole.
    let page = {
        let st = srv.state.lock().unwrap();
        st.pages
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, html)| html.clone())
    };
    if let Some(html) = page {
        send_full(
            sock,
            srv.clone(),
            html.as_bytes(),
            "text/html; charset=utf-8",
            method == "HEAD",
            // The page is what the crawl reads *to find* targets, so it is not counted
            // against "did any target body move?".
            false,
        )
        .await;
        return;
    }

    let body = {
        let st = srv.state.lock().unwrap();
        st.files
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, b)| b.clone())
    };
    let Some(body) = body else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };

    let range = header_value(&head, "range");
    match range.as_deref().and_then(|r| parse_range(r, body.len())) {
        Some((start, end)) if method != "HEAD" => {
            srv.ranges.lock().unwrap().push((path.clone(), start, end));
            let slice = &body[start..=end];
            let head_txt = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\n\
                 Content-Length: {}\r\nETag: {}\r\nAccept-Ranges: bytes\r\n\
                 Connection: close\r\n\r\n",
                body.len(),
                slice.len(),
                etag_for(&body),
            );
            let _ = sock.write_all(head_txt.as_bytes()).await;
            srv.bytes_served
                .fetch_add(slice.len() as u64, Ordering::SeqCst);
            let _ = sock.write_all(slice).await;
        }
        _ => {
            send_full(
                sock,
                srv.clone(),
                &body,
                "application/octet-stream",
                method == "HEAD",
                true,
            )
            .await
        }
    }
}

fn current_etag(srv: &SyncServer, path: &str) -> Option<String> {
    let st = srv.state.lock().unwrap();
    st.files
        .iter()
        .find(|(p, _)| p == path)
        .map(|(_, b)| etag_for(b))
}

async fn send_full(
    sock: &mut TcpStream,
    srv: Arc<SyncServer>,
    body: &[u8],
    content_type: &str,
    head_only: bool,
    count_bytes: bool,
) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         ETag: {}\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\
         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len(),
        etag_for(body),
    );
    let _ = sock.write_all(head.as_bytes()).await;
    if !head_only {
        if count_bytes {
            srv.bytes_served
                .fetch_add(body.len() as u64, Ordering::SeqCst);
        }
        let _ = sock.write_all(body).await;
    }
}

/// `bytes=start-end` → inclusive bounds.
fn parse_range(value: &str, total: usize) -> Option<(usize, usize)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (s, e) = first.split_once('-')?;
    let start: usize = s.trim().parse().ok()?;
    let end: usize = if e.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        e.trim().parse().ok()?
    };
    if start >= total || end < start {
        return None;
    }
    Some((start, end.min(total.saturating_sub(1))))
}

/// A config with tiny split threshold and fast retries, so the segmented path (and the
/// part-file handling that only it exercises) is used on small test bodies.
fn test_config(state_dir: &Path, download_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.state_dir = state_dir.to_path_buf();
    cfg.download_dir = download_dir.to_path_buf();
    cfg.connections = 4;
    cfg.min_chunk = 1024;
    cfg.split_threshold = 1024;
    cfg.retry_backoff_ms = 1;
    cfg.max_retries = 1;
    // Bounded so a fixture that accidentally points somewhere unreachable fails the test
    // instead of stalling it.
    cfg.connect_timeout_secs = 2;
    cfg.read_timeout_secs = 5;
    cfg.auto_categorize = false;
    cfg
}

/// A port nothing is listening on, obtained by binding and immediately dropping. Connecting
/// to it is refused at once, unlike a filtered port, which hangs until the connect timeout.
async fn a_closed_port() -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

fn file_project(name: &str, url: &str, output: &Path, interval_secs: u64) -> Project {
    Project::new(
        name,
        ProjectKind::File,
        url,
        output.to_path_buf(),
        interval_secs,
    )
}

// ---------------------------------------------------------------------------
// File projects: periodic synchronization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_file_project_fetches_once_then_transfers_nothing_while_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = body_of(1, 40_000);
    let srv = SyncServer::start(vec![("/feed.json", v1.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("feed.json");

    let mut p = file_project("feed", &srv.url("/feed.json"), &out, 3600);
    let runner = ProjectRunner::new(cfg);

    // First run: no validator yet, so a full fetch.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(
        std::fs::read(&out).unwrap(),
        v1,
        "v1 on disk, byte for byte"
    );
    assert!(
        p.validator.etag.is_some(),
        "the server's ETag must be remembered, or the next run cannot ask"
    );
    let first_bytes = srv.bytes_served();
    assert!(first_bytes >= v1.len() as u64);
    let first_requests = srv.conditionals();

    // Second run: the server is asked, says 304, and nothing is transferred.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Unchanged);
    assert_eq!(std::fs::read(&out).unwrap(), v1, "the file is untouched");
    assert_eq!(
        srv.not_modified(),
        1,
        "the server must have answered 304, not served the body again"
    );
    assert!(
        srv.conditionals() > first_requests,
        "the second run must actually send If-None-Match"
    );
    assert_eq!(
        srv.bytes_served(),
        first_bytes,
        "not one body byte should have moved on an unchanged sync"
    );

    // ...and a third run behaves the same way, so it is not a one-off.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Unchanged);
    assert_eq!(srv.bytes_served(), first_bytes);
    assert_eq!(p.last_status.as_deref(), Some("unchanged"));
    assert_eq!(p.runs, 3);
    assert_eq!(p.failures, 0, "an unchanged sync is a success");
}

#[tokio::test]
async fn a_changed_remote_replaces_the_file_with_the_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = body_of(1, 30_000);
    let v2 = body_of(2, 45_000); // deliberately a different length too
    let srv = SyncServer::start(vec![("/a.bin", v1.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("a.bin");

    let mut p = file_project("a", &srv.url("/a.bin"), &out, 3600);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    let old_etag = p.validator.etag.clone().unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), v1);

    srv.set_body("/a.bin", v2.clone());
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), v2, "the new version, in full");
    assert_ne!(
        p.validator.etag.clone().unwrap(),
        old_etag,
        "the stored validator must follow the bytes that are now on disk"
    );
    let bytes_after_change = srv.bytes_served();

    // And once more, to show the new validator is the one being used.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Unchanged);
    assert_eq!(srv.bytes_served(), bytes_after_change);
}

#[tokio::test]
async fn a_partial_download_from_an_older_version_is_never_spliced_in() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = body_of(1, 50_000);
    let v2 = body_of(2, 50_000); // same length: only a byte-level check catches a splice
    let srv = SyncServer::start(vec![("/x.bin", v1.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("x.bin");

    let mut p = file_project("x", &srv.url("/x.bin"), &out, 3600);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), v1);

    // Simulate a run that died half way through fetching the *next* version: a working
    // file full of bytes from a version the server has since replaced.
    let part = PathBuf::from(format!("{}.part", out.display()));
    std::fs::write(&part, body_of(99, v2.len())).unwrap();

    srv.set_body("/x.bin", v2.clone());
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(
        std::fs::read(&out).unwrap(),
        v2,
        "the new version exactly — not a mixture of the stale partial file and the new one"
    );
    assert!(
        !part.exists(),
        "the working file is renamed away on success"
    );
}

#[tokio::test]
async fn a_failed_run_leaves_the_previous_version_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = body_of(1, 20_000);
    let v2 = body_of(2, 20_000);
    let srv = SyncServer::start(vec![("/y.bin", v1.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("y.bin");

    let mut p = file_project("y", &srv.url("/y.bin"), &out, 3600);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), v1);

    // The remote changed, but the download cannot complete.
    srv.set_body("/y.bin", v2);
    srv.fail_downloads(true);
    let outcome = runner.run(&mut p).await;
    assert_eq!(outcome, RunOutcome::Failed);
    assert!(outcome.is_failure());
    assert!(
        p.last_error.is_some(),
        "the reason must be recorded on the project, not swallowed"
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        v1,
        "a failed fetch must never damage the copy that was already there"
    );
    assert_eq!(p.failures, 1, "a consecutive-failure count for the listing");

    // The server recovers and the next run completes normally.
    srv.fail_downloads(false);
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(p.failures, 0, "a success clears the failure count");
}

#[tokio::test]
async fn a_file_project_uses_ranges_like_any_other_download() {
    let dir = tempfile::tempdir().unwrap();
    // Comfortably more than `split_threshold`, so the segmented path is taken.
    let body = body_of(7, 200_000);
    let srv = SyncServer::start(vec![("/big.bin", body.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("big.bin");

    let mut p = file_project("big", &srv.url("/big.bin"), &out, 3600);
    p.connections = 4;
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), body);
    assert!(
        srv.observed_ranges().len() > 1,
        "a project's transfer goes through the same parallel engine as a download \
         (saw {:?})",
        srv.observed_ranges()
    );
}

#[tokio::test]
async fn a_server_that_offers_no_validator_is_re_downloaded_and_said_so() {
    // Nothing to assert about the wire here beyond "it still works"; the point is that a
    // project against such a server does not silently do nothing. The warning is emitted
    // through `tracing`, which the test does not capture, so the observable fact is that
    // the body keeps arriving.
    let dir = tempfile::tempdir().unwrap();
    let body = body_of(3, 5_000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body_clone = body.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let b = body_clone.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let head_only = head.starts_with("head");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    b.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                if !head_only {
                    let _ = sock.write_all(&b).await;
                }
                let _ = sock.shutdown().await;
            });
        }
    });

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("novalidator.bin");
    let mut p = file_project("nv", &format!("http://{addr}/nv.bin"), &out, 3600);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), body);
    assert!(
        p.validator.is_empty(),
        "the server gave us nothing to ask about, and that is recorded honestly"
    );
    // It still works on the next run, by fetching again.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(std::fs::read(&out).unwrap(), body);
}

// ---------------------------------------------------------------------------
// Site projects: scheduled grabber runs
// ---------------------------------------------------------------------------

fn site_project(name: &str, url: &str, dir: &Path, download: bool) -> Project {
    let mut p = Project::new(name, ProjectKind::Site, url, dir.to_path_buf(), 3600);
    p.grab = GrabberConfig {
        depth: 0,
        max_pages: 10,
        same_host: true,
        include: Vec::new(),
        exclude: Vec::new(),
        media_only: true,
    };
    p.download = download;
    p
}

fn page_with_links(paths: &[&str]) -> String {
    let mut html = String::from("<html><body><h1>Downloads</h1><ul>");
    for p in paths {
        html.push_str(&format!("<li><a href=\"{p}\">{p}</a></li>"));
    }
    html.push_str("</ul></body></html>");
    html
}

#[tokio::test]
async fn a_planning_only_site_project_records_what_it_found_without_fetching() {
    let dir = tempfile::tempdir().unwrap();
    let srv = SyncServer::start(vec![
        ("/a.zip", body_of(1, 1_000)),
        ("/b.pdf", body_of(2, 1_000)),
    ])
    .await;
    srv.add_page("/downloads", page_with_links(&["/a.zip", "/b.pdf"]));
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("site");

    let mut p = site_project("plan", &srv.url("/downloads"), &out, false);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Planned);
    assert_eq!(p.last_found, Some(2), "both links were found");
    assert_eq!(p.last_fetched, Some(0), "and none were fetched");
    assert_eq!(srv.bytes_served(), 0, "planning downloads nothing at all");
    assert!(!out.exists() || std::fs::read_dir(&out).unwrap().next().is_none());
}

#[tokio::test]
async fn a_site_project_fetches_what_it_finds_and_skips_it_next_time() {
    let dir = tempfile::tempdir().unwrap();
    let a = body_of(11, 12_000);
    let b = body_of(12, 9_000);
    let srv = SyncServer::start(vec![("/a.zip", a.clone()), ("/b.pdf", b.clone())]).await;
    srv.add_page("/downloads", page_with_links(&["/a.zip", "/b.pdf"]));
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("site");

    let mut p = site_project("get", &srv.url("/downloads"), &out, true);
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(p.last_found, Some(2));
    assert_eq!(p.last_fetched, Some(2));
    assert_eq!(std::fs::read(out.join("a.zip")).unwrap(), a);
    assert_eq!(std::fs::read(out.join("b.pdf")).unwrap(), b);
    assert_eq!(
        p.validators.len(),
        2,
        "each fetched target remembers its own validator"
    );
    let bytes_after_first = srv.bytes_served();

    // Second run: both targets are unchanged, so both are skipped after a conditional probe.
    assert_eq!(runner.run(&mut p).await, RunOutcome::Unchanged);
    assert_eq!(p.last_fetched, Some(0));
    assert_eq!(p.last_skipped, Some(2));
    assert_eq!(
        srv.bytes_served(),
        bytes_after_first,
        "an unchanged site must not re-fetch its targets"
    );
    assert_eq!(std::fs::read(out.join("a.zip")).unwrap(), a);

    // One target changes: only that one is re-fetched.
    let a2 = body_of(21, 15_000);
    srv.set_body("/a.zip", a2.clone());
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(p.last_fetched, Some(1), "only the changed target");
    assert_eq!(p.last_skipped, Some(1));
    assert_eq!(std::fs::read(out.join("a.zip")).unwrap(), a2);
    assert_eq!(std::fs::read(out.join("b.pdf")).unwrap(), b);
}

#[tokio::test]
async fn one_broken_target_does_not_lose_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let a = body_of(31, 4_000);
    let b = body_of(32, 4_000);
    let srv = SyncServer::start(vec![("/a.zip", a.clone()), ("/b.pdf", b.clone())]).await;
    srv.add_page(
        "/downloads",
        page_with_links(&["/a.zip", "/gone.zip", "/b.pdf"]),
    );
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("site");

    let mut p = site_project("mixed", &srv.url("/downloads"), &out, true);
    let runner = ProjectRunner::new(cfg);

    let outcome = runner.run(&mut p).await;
    assert_eq!(
        outcome,
        RunOutcome::Partial,
        "some targets succeeded and one did not"
    );
    assert!(outcome.is_failure(), "a partial run is worth reporting");
    assert_eq!(p.last_fetched, Some(2));
    assert_eq!(std::fs::read(out.join("a.zip")).unwrap(), a);
    assert_eq!(std::fs::read(out.join("b.pdf")).unwrap(), b);
    let err = p.last_error.clone().unwrap();
    assert!(
        err.contains("gone.zip"),
        "the failing target must be named in the error: {err}"
    );
    assert!(
        !err.contains("a.zip"),
        "a target that succeeded must not be blamed: {err}"
    );
}

#[tokio::test]
async fn a_site_run_respects_its_download_cap() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("/f{i}.zip"), body_of(i as u64 + 1, 2_000)))
        .collect();
    let srv = SyncServer::start(
        files
            .iter()
            .map(|(p, b)| (p.as_str(), b.clone()))
            .collect::<Vec<_>>(),
    )
    .await;
    let links: Vec<String> = (0..5).map(|i| format!("/f{i}.zip")).collect();
    srv.add_page(
        "/downloads",
        page_with_links(&links.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
    );
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("site");

    let mut p = site_project("capped", &srv.url("/downloads"), &out, true);
    p.max_downloads = 2;
    let runner = ProjectRunner::new(cfg);

    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);
    assert_eq!(p.last_found, Some(5));
    assert_eq!(p.last_fetched, Some(2), "the cap held");
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        2,
        "only two files were written"
    );
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn running_a_project_moves_its_next_run_to_a_full_interval_away() {
    let dir = tempfile::tempdir().unwrap();
    let body = body_of(5, 3_000);
    let srv = SyncServer::start(vec![("/s.bin", body)]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("s.bin");

    let mut p = file_project("s", &srv.url("/s.bin"), &out, 3600);
    // Pretend the schedule is long overdue — a laptop that was shut for a week.
    p.next_run_ms = Some(1_000);
    assert!(p.is_due(odm::util::now_ms()));

    let before = odm::util::now_ms();
    let runner = ProjectRunner::new(cfg);
    assert_eq!(runner.run(&mut p).await, RunOutcome::Updated);

    let next = p.next_run_ms.unwrap();
    assert!(
        next >= before + 3_600_000,
        "the next run is a whole interval after this one, not a catch-up of every missed \
         slot (next={next}, before={before})"
    );
    assert!(!p.is_due(odm::util::now_ms()));
}

#[tokio::test]
async fn the_store_reports_only_what_is_due_and_keeps_the_files_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let body = body_of(6, 3_000);
    let srv = SyncServer::start(vec![("/d.bin", body.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let store = ProjectStore::open_in_state_dir(&cfg.state_dir).unwrap();
    let out = dir.path().join("d.bin");

    let mut p = file_project("d", &srv.url("/d.bin"), &out, 60);
    let now = odm::util::now_ms();
    p.next_run_ms = Some(now + 600_000); // not due for ten minutes
    store.save(&p).unwrap();
    assert!(store.due(now).unwrap().is_empty());

    // A restart must find the same definition, validators and all.
    let mut reloaded = store.load("d").unwrap();
    assert_eq!(reloaded.url, p.url);
    let runner = ProjectRunner::new(cfg.clone());
    assert_eq!(runner.run(&mut reloaded).await, RunOutcome::Updated);
    store.save(&reloaded).unwrap();

    let back = store.load("d").unwrap();
    assert_eq!(back.runs, 1);
    assert!(
        back.validator.etag.is_some(),
        "validators survive a restart"
    );
    assert_eq!(std::fs::read(&out).unwrap(), body);
    assert!(!store
        .due(odm::util::now_ms())
        .unwrap()
        .iter()
        .any(|x| x.name == "d"));
}

#[tokio::test]
async fn a_disabled_project_is_not_due_and_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let store = ProjectStore::open_in_state_dir(&cfg.state_dir).unwrap();
    let mut p = file_project("off", "http://127.0.0.1:1/x", &dir.path().join("x"), 60);
    p.next_run_ms = Some(1); // overdue by any measure
    p.enabled = false;
    store.save(&p).unwrap();
    assert!(
        store.due(odm::util::now_ms()).unwrap().is_empty(),
        "a project the user switched off never runs by itself"
    );
}

/// The exact path the daemon's ticker takes: ask the scheduler for what is due, run it,
/// and persist the result — including the failure of one project not stopping the next.
#[tokio::test]
async fn the_schedulers_due_sweep_runs_and_saves_what_is_due() {
    let dir = tempfile::tempdir().unwrap();
    let body = body_of(8, 6_000);
    let srv = SyncServer::start(vec![("/t.bin", body.clone())]).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));

    let store = ProjectStore::open_in_state_dir(&cfg.state_dir).unwrap();
    let now = odm::util::now_ms();
    let out = dir.path().join("t.bin");
    let mut due = file_project("due", &srv.url("/t.bin"), &out, 60);
    due.next_run_ms = Some(now - 1);
    store.save(&due).unwrap();
    let mut not_yet = file_project("later", &srv.url("/t.bin"), &dir.path().join("l.bin"), 60);
    not_yet.next_run_ms = Some(now + 600_000);
    store.save(&not_yet).unwrap();
    // A project pointing at a dead port must not stop the sweep.
    let dead = a_closed_port().await;
    let mut broken = file_project(
        "broken",
        &format!("http://{dead}/x"),
        &dir.path().join("b.bin"),
        60,
    );
    broken.next_run_ms = Some(now - 1);
    broken.max_downloads = 1;
    store.save(&broken).unwrap();

    let sched = odm::scheduler::Scheduler::new(cfg).unwrap();
    let runner = ProjectRunner::with_limits(
        sched.cfg.clone(),
        sched.global.clone(),
        sched.servers.clone(),
        sched.quota.clone(),
    );
    let results = sched.run_due_projects(&runner).await;

    let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"due") && names.contains(&"broken"),
        "both due projects should have been attempted: {names:?}"
    );
    assert!(
        !names.contains(&"later"),
        "a project that is not due yet must be left alone: {names:?}"
    );

    // The results are persisted, not just returned.
    let after = store.load("due").unwrap();
    assert_eq!(after.runs, 1);
    assert_eq!(after.last_status.as_deref(), Some("updated"));
    assert!(after.next_run_ms.unwrap() > now, "and rescheduled forwards");
    assert_eq!(std::fs::read(&out).unwrap(), body);

    let broken_after = store.load("broken").unwrap();
    assert_eq!(broken_after.last_status.as_deref(), Some("failed"));
    assert_eq!(broken_after.runs, 1, "a failure is still a run");
    assert!(broken_after.last_error.is_some());

    // Nothing is due a moment later.
    assert!(sched
        .run_due_projects(&runner)
        .await
        .iter()
        .all(|(n, _)| n != "due"));
}

// ---------------------------------------------------------------------------
// Parsing and the name/filesystem boundary
// ---------------------------------------------------------------------------

#[test]
fn intervals_accept_the_units_a_person_types() {
    assert_eq!(parse_interval("30m").unwrap(), 1800);
    assert_eq!(parse_interval("1h").unwrap(), 3600);
    assert_eq!(parse_interval("1d").unwrap(), 86_400);
    assert_eq!(parse_interval("2h30m").unwrap(), 9000);
    assert_eq!(parse_interval("90").unwrap(), 90);
    assert!(parse_interval("0").is_err());
    assert!(parse_interval("soon").is_err());
    assert!(parse_interval("").is_err());
}

#[test]
fn a_project_name_can_never_escape_its_directory() {
    for bad in ["../x", "a/b", "", ".", "..", "a b"] {
        assert!(
            odm::project::validate_name(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
    odm::project::validate_name("nightly-feed.v2").unwrap();
}

#[test]
fn project_kinds_map_to_the_urls_the_engine_already_understands() {
    // A project pointed at a manifest has to end up in the media pipeline, which is what
    // `JobKind::guess` decides; nothing in the project module may re-derive it.
    assert_eq!(JobKind::guess("http://x/a.m3u8"), JobKind::Hls);
    assert_eq!(JobKind::guess("http://x/a.mpd"), JobKind::Dash);
    assert_eq!(JobKind::guess("http://x/a.zip"), JobKind::Http);
}
